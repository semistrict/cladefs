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
//! `<tenant>.<cell>.promise`, and its logs `<tenant>.<cell>.<number>.<epoch>.<log>`, keyed by the
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
    /// An append past the log's end: what is before it is missing.
    #[error("log {log} ends at {end}, appended at {offset}")]
    Gap { log: u64, end: u64, offset: u64 },
    /// The cell's logs would hold more than the member holds for a cell.
    #[error("the cell's logs would hold {would} bytes, past {most}")]
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
    /// The most bytes a cell's logs hold here, of every configuration: the writer's lag bound in
    /// bytes, which bounds what is not in the bucket yet (doc-21, Commit).
    most: u64,
    cells: Mutex<HashMap<CellId, Arc<Mutex<Held>>>>,
}

/// What the directory holds of a cell, under its lock: an append and a fence of it never at once,
/// so that a fence answered leaves no append of an older epoch in flight.
#[derive(Default)]
struct Held {
    /// Its promise, as on the disk.
    promise: u64,
    /// Its logs' ends, by configuration and log.
    ends: BTreeMap<(Key, u64), u64>,
}

impl Held {
    fn bytes(&self) -> u64 {
        self.ends.values().sum()
    }
}

impl<D: Dir> Replicas<D> {
    /// The replica directory `dir`: of the incarnation its file names, or a new one, made, if it
    /// holds nothing yet. A directory holding files and no incarnation is refused: what its logs
    /// were promised can't be known. Each cell's logs hold at most `most` bytes.
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
            if !names.is_empty() {
                return Err(anyhow!(
                    "the replica directory holds {} files and no {INCARNATION}: refused, its \
                     promises unknown",
                    names.len()
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
    /// synced. Bytes before the end again are written again, as a writer catching a member up
    /// sends what it may have had: the same, as a configuration's log is written once.
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
        let end = held.ends.get(&(key, log)).copied().unwrap_or(0);
        if offset > end {
            return Err(Refused::Gap { log, end, offset }.into());
        }
        let after = end.max(offset + bytes.len() as u64);
        let would = held.bytes() - end + after;
        if would > self.most {
            return Err(Refused::Full { would, most: self.most }.into());
        }
        let path = log_path(cell, key, log);
        let file = match end {
            0 if !held.ends.contains_key(&(key, log)) => {
                let file =
                    self.dir.create(&path, false).with_context(|| format!("making {path}"))?;
                self.dir.sync_dir("").context("syncing the replica directory")?;
                file
            }
            _ => self.dir.open(&path, false).with_context(|| format!("opening {path}"))?,
        };
        file.write_at(bytes, offset).with_context(|| format!("writing {path}"))?;
        file.sync_cached().with_context(|| format!("syncing {path}"))?;
        held.ends.insert((key, log), after);
        Ok(after)
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
            write_text(&self.dir, &promise_path(cell), &format!("{epoch}\n"))?;
            held.promise = epoch;
        }
        Ok(held
            .ends
            .range((key, 0)..=(key, u64::MAX))
            .map(|(&(_, log), &end)| (log, end))
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
        let end = held.ends.get(&(key, log)).copied().unwrap_or(0);
        let len = len.min(end.saturating_sub(offset));
        let mut bytes = vec![0; len as usize];
        if len > 0 {
            let path = log_path(cell, key, log);
            let file = self.dir.open(&path, false).with_context(|| format!("opening {path}"))?;
            file.read_at(&mut bytes, offset).with_context(|| format!("reading {path}"))?;
        }
        Ok(bytes)
    }

    /// Drops `cell`'s logs that no take will read: those of the configuration `key` to `through`,
    /// which the bucket holds, the head naming the segments they wrote; and every configuration's
    /// before `key`, which the head names no more.
    pub fn drop_logs(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        through: Option<u64>,
    ) -> Result<(), Error> {
        self.check(incarnation)?;
        let held = self.held(cell)?;
        let mut held = held.lock().unwrap();
        let gone: Vec<(Key, u64)> = (held.ends.keys())
            .filter(|&&(of, log)| {
                of < key || (of == key && through.is_some_and(|last| log <= last))
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

    /// Drops what the directory holds of `cell` that its head names no more: every configuration's
    /// logs before `active`, the one it names; or, of a cell deleted, gone, or no longer replicated
    /// (`active` none), its logs and its promise.
    pub fn sweep(&self, cell: CellId, active: Option<Key>) -> Result<(), Error> {
        let held = self.held(cell)?;
        let mut held = held.lock().unwrap();
        let gone: Vec<(Key, u64)> = (held.ends.keys())
            .filter(|&&(of, _)| active.is_none_or(|active| of < active))
            .copied()
            .collect();
        self.remove(cell, &mut held, &gone)?;
        if active.is_none() && held.promise > 0 {
            self.dir.remove(&promise_path(cell)).context("removing a promise")?;
            self.dir.sync_dir("").context("syncing the replica directory")?;
            held.promise = 0;
        }
        Ok(())
    }

    fn remove(&self, cell: CellId, held: &mut Held, gone: &[(Key, u64)]) -> Result<(), Error> {
        for &(key, log) in gone {
            let path = log_path(cell, key, log);
            self.dir.remove(&path).with_context(|| format!("removing {path}"))?;
            held.ends.remove(&(key, log));
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
                    let text = read_text(&self.dir, &name)?;
                    held.promise = (text.trim().parse())
                        .with_context(|| format!("{name}: bad promise {text:?}"))?;
                }
                Some((of, Some(key))) if of == cell => {
                    let log = name.rsplit('.').next().and_then(|log| log.parse().ok());
                    let log = log.ok_or_else(|| anyhow!("bad log name {name:?}"))?;
                    let file =
                        self.dir.open(&name, false).with_context(|| format!("opening {name}"))?;
                    held.ends
                        .insert((key, log), file.size().with_context(|| format!("sizing {name}"))?);
                }
                _ => {}
            }
        }
        let held = Arc::new(Mutex::new(held));
        cells.insert(cell, held.clone());
        Ok(held)
    }
}

fn promise_path(cell: CellId) -> String {
    format!("{}.{}.promise", cell.tenant, cell.id)
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
        3 if parts[2] == "promise" => Some((cell, None)),
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
