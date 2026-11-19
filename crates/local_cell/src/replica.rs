// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a host's replica directory (doc-21, TASK-537.3.2): the logs of other hosts' cells
//! whose configurations it is a member of, as their writers send them, until the bucket holds
//! them, and each such cell's promise.
//!
//! A member holds, for each configuration of a cell it is in, the writer's segment logs byte for
//! byte, every record held whole (`wal::Runs::Held`): ciphertext under the cell's DEK, and commit
//! metadata MACed, as the bucket's footers are. It opens nothing and checks nothing of them, as it
//! has no key; a taker checks their MACs as it recovers them.
//!
//! Its promise for a cell is the greatest epoch that fenced it: a take fences f+1 members before it
//! reads their logs, and a member refuses every append of an older epoch after, so that no writer
//! the take fenced gathers a quorum behind its read. A promise is on the member's disk, synced
//! before the fence answers; and a disk lost loses its promises with it, so the directory has an
//! incarnation, a random number made with it, which a configuration names its members by: a
//! directory made anew is another member, which appends to configurations naming the one before
//! are refused by (doc-21, Incarnations; the spec's `NoIncarnation`).
//!
//! The directory is flat, as a [`Dir`] removes no directories: `incarnation`, then for each cell
//! `<tenant>.<cell>.promise.<epoch>`, and its logs `<tenant>.<cell>.<number>.<epoch>.<log>`, keyed by the
//! configuration's number and the epoch of the writer that made it: two takers racing may both
//! make the configuration after the active one, each its own.

use crate::dir::{Dir, File};
use anyhow::{Context as _, anyhow};
use extent_frame::segment::CellId;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Arc, Mutex};

/// The directory's incarnation file.
const INCARNATION: &str = "incarnation";
/// Its first line.
const MAGIC: &str = "cladefs replicas 1";

/// A configuration of a cell, as a member keys its logs: its number, and the epoch of the writer
/// that made it, which alone appends to it. Ordered as configurations supersede each other: by
/// number, then epoch (two takers racing make the same number, the later's epoch greater).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub number: u64,
    pub epoch: u64,
}

/// What a member refuses.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refused {
    /// The call named another incarnation of the directory than this one: it is not that member.
    #[error("incarnation {asked} asked, this directory's is {ours}")]
    Incarnation { asked: u64, ours: u64 },
    /// A take of a later epoch fenced the cell: appends of this one, or fences of an earlier, are
    /// refused.
    #[error("fenced at epoch {promise}")]
    Fenced { promise: u64 },
    /// The configuration's logs would hold more than the member holds for a configuration.
    #[error("the configuration's logs would hold {would} bytes, past {most}")]
    Full { would: u64, most: u64 },
}

/// What a member did not do: refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Refused(#[from] Refused),
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err.into())
    }
}

/// A host's replica directory.
pub struct Replicas<D: Dir> {
    dir: D,
    incarnation: u64,
    /// The most bytes a configuration's logs hold here: the writer's lag bound in bytes, which
    /// bounds what is not in the bucket yet (doc-21, Commit). Per configuration, as a take installs
    /// its own while the one before, holding the same commits, is still here until it activates.
    most: u64,
    cells: Mutex<HashMap<CellId, Arc<Mutex<Held>>>>,
}

/// What the directory holds of a cell, under its lock: an append and a fence of it never at once,
/// so that a fence answered leaves no append of an older epoch in flight.
#[derive(Default)]
struct Held {
    /// Its promise, as on the disk.
    promise: u64,
    /// Its logs, by configuration and log.
    logs: BTreeMap<(Key, u64), Log>,
}

impl Held {
    /// The bytes the logs of configuration `key` hold.
    fn bytes(&self, key: Key) -> u64 {
        self.logs.range((key, 0)..=(key, u64::MAX)).map(|(_, log)| log.end).sum()
    }
}

/// A log a member holds: its frames, each of bytes the writer sent at an offset of its log, one
/// after another in the file and in the log (a frame's offset its frame before's end), each
/// written whole, with its CRC, so that a reopening reads the log's end from its frames, never from
/// the file's size, which a crash leaves past them, padded with zeros (a direct write's whole
/// blocks, `bucket_cell::direct::write_at`, cut back after; TASK-537.3.2's review).
#[derive(Default)]
struct Log {
    /// Its end: the last frame's offset and length.
    end: u64,
    /// Where its next frame goes in the file.
    at: u64,
    /// Each frame's offset in the log, and where its bytes are in the file.
    frames: Vec<(u64, u64)>,
}

/// A frame's header: its offset in the log (u64), its length (u32), and the CRC of the two and its
/// bytes (u32), little-endian.
const HEADER: u64 = 16;

fn frame(offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER as usize + bytes.len());
    frame.extend_from_slice(&offset.to_le_bytes());
    frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    let crc = frame_crc(&frame, bytes);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(bytes);
    frame
}

fn frame_crc(head: &[u8], bytes: &[u8]) -> u32 {
    let mut both = head[..12].to_vec();
    both.extend_from_slice(bytes);
    extent_frame::crc(&both)
}

impl Log {
    /// The log in `file`, as far as its frames go: each whole, its CRC right, at the end of the one
    /// before. A frame torn by a crash, zeros past the last, or anything after either, is no part
    /// of it: none of it was answered synced.
    fn read(file: &impl File) -> io::Result<Self> {
        let size = file.size()?;
        let mut log = Self::default();
        while log.at + HEADER <= size {
            let mut head = [0; HEADER as usize];
            file.read_at(&mut head, log.at)?;
            let offset = u64::from_le_bytes(head[..8].try_into().unwrap());
            let len = u64::from(u32::from_le_bytes(head[8..12].try_into().unwrap()));
            let crc = u32::from_le_bytes(head[12..].try_into().unwrap());
            if len == 0 || offset != log.end || log.at + HEADER + len > size {
                break;
            }
            let mut bytes = vec![0; len as usize];
            file.read_at(&mut bytes, log.at + HEADER)?;
            if frame_crc(&head, &bytes) != crc {
                break;
            }
            log.frames.push((offset, log.at + HEADER));
            log.end += len;
            log.at += HEADER + len;
        }
        Ok(log)
    }

    /// Where the log's bytes from `offset`, `len` of them at most, are in the file, frame by frame.
    fn pieces(&self, offset: u64, len: u64) -> Vec<(u64, u64)> {
        let last = offset.saturating_add(len).min(self.end);
        let mut pieces = Vec::new();
        for (i, &(start, at)) in self.frames.iter().enumerate() {
            let stop = self.frames.get(i + 1).map_or(self.end, |&(next, _)| next);
            let (from, to) = (start.max(offset), stop.min(last));
            if from < to {
                pieces.push((at + (from - start), to - from));
            }
        }
        pieces
    }
}

impl<D: Dir> Replicas<D> {
    /// The replica directory `dir`: of the incarnation its file names, or a new one, made, if it
    /// holds nothing of a member's yet (a file system's own `lost+found`, or an incarnation's file
    /// a crash left half made, aside). A directory holding a member's files and no incarnation is
    /// refused: what its logs were promised can't be known. Each configuration's logs hold at most
    /// `most` bytes.
    pub fn open(dir: D, most: u64) -> Result<Self, Error> {
        let names = dir.list("").context("listing the replica directory")?;
        let incarnation = if names.iter().any(|name| name == INCARNATION) {
            let text = read_text(&dir, INCARNATION)?;
            let mut lines = text.lines();
            if lines.next() != Some(MAGIC) {
                return Err(anyhow!("{INCARNATION}: not {MAGIC:?}").into());
            }
            let line = lines.next().unwrap_or_default();
            let number = line.strip_prefix("incarnation ").unwrap_or_default();
            number.parse().with_context(|| format!("{INCARNATION}: bad line {line:?}"))?
        } else {
            let held: Vec<&String> =
                names.iter().filter(|name| parse_name(name).is_some()).collect();
            if !held.is_empty() {
                return Err(anyhow!(
                    "the replica directory holds {} files of a member's ({}...) and no \
                     {INCARNATION}: refused, its promises unknown",
                    held.len(),
                    held[0]
                )
                .into());
            }
            let incarnation = loop {
                let number: u64 = rand::random();
                if number != 0 {
                    break number;
                }
            };
            write_text(&dir, INCARNATION, &format!("{MAGIC}\nincarnation {incarnation}\n"))?;
            incarnation
        };
        Ok(Self { dir, incarnation, most, cells: Mutex::default() })
    }

    /// The directory's incarnation: what a configuration naming this host as a member names.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// Appends `bytes` at `offset` of the log `log` of `cell`'s configuration `key`, whose writer's
    /// epoch is `key.epoch`, to this member named in `incarnation`, and syncs it: the log's end,
    /// synced. Bytes before the end again are not written again, as a writer catching a member up
    /// sends what it may have had: the same, as a configuration's log is written once. An append
    /// past the end writes nothing, and answers the end, from which the writer sends again.
    pub fn append(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        log: u64,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, Error> {
        self.check(incarnation)?;
        let held = self.held(cell)?;
        let mut held = held.lock().unwrap();
        if key.epoch < held.promise {
            return Err(Refused::Fenced { promise: held.promise }.into());
        }
        // Nothing to append makes nothing: no file a writer's empty appends would make by the
        // thousand, past the bound, which counts bytes (Codex's review of TASK-537.3.2).
        if bytes.is_empty() {
            return Ok(held.logs.get(&(key, log)).map_or(0, |state| state.end));
        }
        let path = log_path(cell, key, log);
        // What it would append, checked before any file is made: an append past the end, all of
        // it held already, or past the bound, leaves nothing behind.
        let end = held.logs.get(&(key, log)).map_or(0, |state| state.end);
        if offset > end || offset + bytes.len() as u64 <= end {
            return Ok(end);
        }
        let would = held.bytes(key) + (offset + bytes.len() as u64 - end);
        if would > self.most {
            return Err(Refused::Full { would, most: self.most }.into());
        }
        if !held.logs.contains_key(&(key, log)) {
            // Its file made, it is held at once, empty, however its first append ends: a failure
            // after leaves a file the next append opens, and drops and sweeps remove.
            match self.dir.create(&path, false) {
                Ok(_) => {
                    held.logs.insert((key, log), Log::default());
                    self.dir.sync_dir("").context("syncing the replica directory")?;
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    // Made by an append that failed before it was held: what it holds, if any of
                    // it was written, read as a reopening would.
                    let file =
                        self.dir.open(&path, false).with_context(|| format!("opening {path}"))?;
                    let found = Log::read(&file).with_context(|| format!("reading {path}"))?;
                    let found_end = found.end;
                    held.logs.insert((key, log), found);
                    if found_end != 0 {
                        drop(held);
                        return self.append((cell, key, incarnation), log, offset, bytes);
                    }
                }
                Err(err) => {
                    return Err(anyhow::Error::from(err).context(format!("making {path}")).into());
                }
            }
        }
        let state = held.logs.get_mut(&(key, log)).unwrap();
        let new = &bytes[(end - offset) as usize..];
        let file = self.dir.open(&path, false).with_context(|| format!("opening {path}"))?;
        file.write_at(&frame(end, new), state.at).with_context(|| format!("writing {path}"))?;
        file.sync_cached().with_context(|| format!("syncing {path}"))?;
        state.frames.push((end, state.at + HEADER));
        state.at += HEADER + new.len() as u64;
        state.end += new.len() as u64;
        Ok(state.end)
    }

    /// Fences `cell` at `epoch`, for this member named in `incarnation`: its promise raised to it,
    /// on the disk, before this returns; then the ends of the logs it holds of the configuration
    /// `key`. Refused if a later epoch fenced it already.
    pub fn fence(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        epoch: u64,
    ) -> Result<Vec<(u64, u64)>, Error> {
        self.check(incarnation)?;
        let held = self.held(cell)?;
        let mut held = held.lock().unwrap();
        if epoch < held.promise {
            return Err(Refused::Fenced { promise: held.promise }.into());
        }
        if epoch > held.promise {
            // A file of each promise, made and synced before the one before goes: a crash
            // between leaves both, the greatest read. Nothing renamed over another (TASK-537.3.4's
            // review: `Dir::rename` renames to no file there).
            let path = promise_path(cell, epoch);
            let file = self.dir.create(&path, false).with_context(|| format!("making {path}"))?;
            file.sync().with_context(|| format!("syncing {path}"))?;
            self.dir.sync_dir("").context("syncing the replica directory")?;
            if held.promise > 0 {
                let before = promise_path(cell, held.promise);
                self.dir.remove(&before).with_context(|| format!("removing {before}"))?;
                self.dir.sync_dir("").context("syncing the replica directory")?;
            }
            held.promise = epoch;
        }
        Ok((held.logs.range((key, 0)..=(key, u64::MAX)))
            .map(|(&(_, log), state)| (log, state.end))
            .collect())
    }

    /// `len` bytes at `offset` of the log `log` of `cell`'s configuration `key`, as far as it holds
    /// them: fewer past its end, none of a log it does not hold.
    pub fn read(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        log: u64,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, Error> {
        self.check(incarnation)?;
        let held = self.held(cell)?;
        let held = held.lock().unwrap();
        let Some(state) = held.logs.get(&(key, log)) else { return Ok(Vec::new()) };
        let pieces = state.pieces(offset, len);
        let mut bytes = Vec::new();
        if !pieces.is_empty() {
            let path = log_path(cell, key, log);
            let file = self.dir.open(&path, false).with_context(|| format!("opening {path}"))?;
            for (at, len) in pieces {
                let start = bytes.len();
                bytes.resize(start + len as usize, 0);
                file.read_at(&mut bytes[start..], at).with_context(|| format!("reading {path}"))?;
            }
        }
        Ok(bytes)
    }

    /// Drops `cell`'s logs that no take will read: with `through`, those of the configuration
    /// `key` to it, which the bucket holds, the head naming the segments they wrote, and nothing of
    /// any other configuration; without, every configuration's before `key`, which the head names
    /// no more. A writer replacing its configuration drops the new one's logs as the bucket takes
    /// them while the head still names the one before, whose logs a take would read (Codex's
    /// review of TASK-537.3.5): only a writer whose head names `key` drops those before.
    pub fn drop_logs(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        through: Option<u64>,
    ) -> Result<(), Error> {
        self.check(incarnation)?;
        let held = self.held(cell)?;
        let mut held = held.lock().unwrap();
        let gone: Vec<(Key, u64)> = (held.logs.keys())
            .filter(|&&(of, log)| match through {
                Some(last) => of == key && log <= last,
                None => of < key,
            })
            .copied()
            .collect();
        self.remove(cell, &mut held, &gone)
    }

    /// The cells the directory holds logs or a promise of, and the configurations it holds logs
    /// of: what [`Replicas::sweep`] looks at, against each cell's head.
    pub fn cells(&self) -> Result<Vec<(CellId, Vec<Key>)>, Error> {
        let mut cells: BTreeMap<(u64, u64), Vec<Key>> = BTreeMap::new();
        for name in self.dir.list("").context("listing the replica directory")? {
            if let Some((cell, key)) = parse_name(&name) {
                let keys = cells.entry((cell.tenant, cell.id)).or_default();
                keys.extend(key.filter(|key| !keys.contains(key)));
            }
        }
        Ok(cells.into_iter().map(|((tenant, id), keys)| (CellId { tenant, id }, keys)).collect())
    }

    /// Drops what the directory holds of `cell` that its head names no more ([`Swept`]).
    pub fn sweep(&self, cell: CellId, swept: Swept) -> Result<(), Error> {
        let held = self.held(cell)?;
        let mut held = held.lock().unwrap();
        let gone: Vec<(Key, u64)> = match swept {
            // A replicated cell whose head names no configuration yet: its first may be being
            // installed, or active since the head was read. Nothing goes (the spec sweeps only
            // what the head names a configuration later than; TASK-537.3.2's review).
            Swept::Unconfigured => return Ok(()),
            Swept::Before(active) => {
                held.logs.keys().filter(|&&(of, _)| of < active).copied().collect()
            }
            Swept::Gone => held.logs.keys().copied().collect(),
        };
        self.remove(cell, &mut held, &gone)?;
        if swept == Swept::Gone && held.promise > 0 {
            self.dir.remove(&promise_path(cell, held.promise)).context("removing a promise")?;
            self.dir.sync_dir("").context("syncing the replica directory")?;
            held.promise = 0;
        }
        Ok(())
    }

    fn remove(&self, cell: CellId, held: &mut Held, gone: &[(Key, u64)]) -> Result<(), Error> {
        for &(key, log) in gone {
            let path = log_path(cell, key, log);
            self.dir.remove(&path).with_context(|| format!("removing {path}"))?;
            held.logs.remove(&(key, log));
        }
        if !gone.is_empty() {
            self.dir.sync_dir("").context("syncing the replica directory")?;
        }
        Ok(())
    }

    fn check(&self, incarnation: u64) -> Result<(), Refused> {
        match incarnation == self.incarnation {
            true => Ok(()),
            false => Err(Refused::Incarnation { asked: incarnation, ours: self.incarnation }),
        }
    }

    /// What the directory holds of `cell`, read from the disk the first time it is asked.
    fn held(&self, cell: CellId) -> Result<Arc<Mutex<Held>>, Error> {
        let mut cells = self.cells.lock().unwrap();
        if let Some(held) = cells.get(&cell) {
            return Ok(held.clone());
        }
        let mut held = Held::default();
        for name in self.dir.list("").context("listing the replica directory")? {
            match parse_name(&name) {
                Some((of, None)) if of == cell => {
                    let epoch = name.rsplit('.').next().and_then(|epoch| epoch.parse().ok());
                    let epoch = epoch.ok_or_else(|| anyhow!("bad promise {name:?}"))?;
                    // A crash between a promise and the removal of the one before leaves both.
                    held.promise = held.promise.max(epoch);
                }
                Some((of, Some(key))) if of == cell => {
                    let log = name.rsplit('.').next().and_then(|log| log.parse().ok());
                    let log = log.ok_or_else(|| anyhow!("bad log name {name:?}"))?;
                    let file =
                        self.dir.open(&name, false).with_context(|| format!("opening {name}"))?;
                    held.logs.insert(
                        (key, log),
                        Log::read(&file).with_context(|| format!("reading {name}"))?,
                    );
                }
                _ => {}
            }
        }
        let held = Arc::new(Mutex::new(held));
        cells.insert(cell, held.clone());
        Ok(held)
    }
}

/// What a sweep found of a cell in its head ([`Replicas::sweep`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swept {
    /// Its head names this configuration: those before it go.
    Before(Key),
    /// Its head names no configuration: nothing goes, as one may be installed and active by now.
    Unconfigured,
    /// It is gone, or deleted, for good: all of it goes, its promise too.
    Gone,
}

fn promise_path(cell: CellId, epoch: u64) -> String {
    format!("{}.{}.promise.{epoch}", cell.tenant, cell.id)
}

fn log_path(cell: CellId, key: Key, log: u64) -> String {
    format!("{}.{}.{}.{}.{log}", cell.tenant, cell.id, key.number, key.epoch)
}

/// The cell a file of the directory is of, and the configuration, if it is a log's.
fn parse_name(name: &str) -> Option<(CellId, Option<Key>)> {
    let parts: Vec<&str> = name.split('.').collect();
    let number = |at: usize| parts.get(at).and_then(|part| part.parse::<u64>().ok());
    let cell = CellId { tenant: number(0)?, id: number(1)? };
    match parts.len() {
        4 if parts[2] == "promise" => {
            number(3)?;
            Some((cell, None))
        }
        5 => {
            number(4)?;
            Some((cell, Some(Key { number: number(2)?, epoch: number(3)? })))
        }
        _ => None,
    }
}

fn read_text<D: Dir>(dir: &D, path: &str) -> Result<String, Error> {
    let file = dir.open(path, false).with_context(|| format!("opening {path}"))?;
    let mut bytes = vec![0; file.size().with_context(|| format!("sizing {path}"))? as usize];
    file.read_at(&mut bytes, 0).with_context(|| format!("reading {path}"))?;
    Ok(String::from_utf8(bytes).with_context(|| format!("{path}: not text"))?)
}

/// Writes `text` as the file at `path`, at once: written beside it, synced, renamed over it, and
/// the directory synced, so that a crash leaves the old text or the new, whole.
fn write_text<D: Dir>(dir: &D, path: &str, text: &str) -> Result<(), Error> {
    let new = format!("{path}.new");
    match dir.remove(&new) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            return Err(anyhow::Error::from(err).context(format!("removing {new}")).into());
        }
        _ => {}
    }
    let file = dir.create(&new, false).with_context(|| format!("making {new}"))?;
    file.write_at(text.as_bytes(), 0).with_context(|| format!("writing {new}"))?;
    file.sync().with_context(|| format!("syncing {new}"))?;
    dir.rename(&new, path).with_context(|| format!("renaming {new} to {path}"))?;
    dir.sync_dir("").context("syncing the replica directory")?;
    Ok(())
}

#[cfg(test)]
mod tests;
