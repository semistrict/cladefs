// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's write-ahead log (doc-14, TASK-165.9): what the bucket doesn't
//! have yet of the cell, on local disk, and what recovering it from there takes.
//!
//! - **Its segments**, closed and not yet acknowledged in the bucket: each framed with its footer,
//!   `segments/<cell>/<n>`, the object it is to be in the bucket, and the extents its footer
//!   names stored on their own, `extents/<cell>/<sequence>`. They are framed in the order they
//!   closed, one after another, and made durable together, those framed meanwhile
//!   (`segments::make_durable`, TASK-310): a crash may leave any of those being made durable
//!   whole and any torn, a later one whole and an earlier one not; their raw files and logs stay
//!   until they are durable, and recovery checks each whose raw file or log is there (below).
//! - **Each segment's log** of the commits made in it, `logs/<cell>/<n>`, beside its raw data,
//!   `open-segments/<cell>/<n>`: a record of each commit's contents, as its footer will fold
//!   them (`extent_frame::segment::Contents`), each hashed, appended and synced by the flush that
//!   makes the commit durable, whether the segment is open, or closed since and being framed: a
//!   flush waits for no framing. cladefs: a flush that seals little puts the runs its commits
//!   sealed in their records, and syncs its logs alone, one sync, as a database's write-ahead log
//!   holds its pages; recovery writes them into the raw data again. Others put the CRC32C of each
//!   run in the records, and sync the data and append the records at once: recovery stops at the
//!   first record whose runs the raw data doesn't hold, as at a torn record, a commit whose record
//!   was made durable before its data was not durable ([`LogRecord`], TASK-301). A flush of
//!   commits that stored extents on their own, which no record checks, syncs their data first,
//!   as before. Its raw data and log go once it is
//!   framed and the commits it names are durable, its framed file their record then.
//! - **Its start**, `wal`: the cell's commit its first segment goes on from, which the cell
//!   writes, durably, before the segments it acknowledged leave it ([`StartFile`]): a segment a
//!   crash brings back, its removal not durable, whose last commit is the start's or before, is
//!   acknowledged, and goes again.
//!
//! Recovering it ([`recover`]) keeps each segment whose framed file is whole, and closes again
//! each whose framed file is not, left open, or its framing or its entry's sync stopped by a
//! crash, from its raw data and the records of its log that are whole, which are the commits made
//! durable, at least; cladefs: and, after the first segment not whole, each whose raw data and log
//! are still there, whole or not, as segments are made durable several at once (TASK-310), what
//! it keeps ending at a log refusing a record's runs, at a segment holding no durable commit, or at
//! a log not following the last commit kept (TASK-334); framed files left being made durable,
//! `framing-` ones, it removes: a footer
//! of their contents folded, as the segment's closing would have; one whose log holds none held no
//! durable commit, and goes. A log may lack its segment's last
//! commits, those not durable, as flushes append records in their commits' order: the next segment
//! holding commits then follows commits it lacks, which no later segment holds durable, and those
//! after it go, durably, before it is closed again. cladefs: closed again, it is framed once the
//! cell's keys are known, not by recovery, which holds none yet ([`Recovered::close`],
//! TASK-411.9). The cell's commits are numbered one after
//! another, and the segments a crash left torn are all of the last opening's, which numbered them
//! alike: one closed again is whole if the next segment holding commits holds the next commit first
//! (TASK-165.9's crash rounds, cutting openings, and its Shuttle tests, a crash bringing back a
//! segment's raw data without its log). The footers recovered are
//! stamped as they were, or provisionally for those closed again: opening the cell puts the log
//! after the cell's end by its commits (`bucket_cell::Wal::rebase`), and stamps them anew.

use crate::dir::{Dir, File, not_found, read_file};
use crate::framed::{read_frame, read_whole};
use crate::layout::{
    FORMAT, FRAMING, LOGS, OPEN_SEGMENTS, SEGMENTS, SegmentId, cell_dir, decode_with, extent_path,
    frame_salted, list_numbers, log_path, open_segment_path, read_versioned, segment_path,
    versioned,
};
use crate::raw::{RawEncryption, SECTOR};
use crate::segments::{fold, footer_last, segment_footer};
use crate::untrusted::Cursor;
use anyhow::{Context as _, Error, ensure};
use extent_frame::segment::{CellId, Chain, Contents, Footer, epoch_of, writer_of};
use extent_frame::{KeyId, Salt};
use extent_keys::{MAC_LEN, SALT_LEN};
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

/// The file of the WAL's start.
pub(crate) const START: &str = "wal";

/// The bytes of each of the start file's two slots: a record each, written whole or torn alone.
const SLOT: u64 = 4096;

/// A slot of the start file: the start, and the serial number of its writing, which the later
/// writing's exceeds.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
struct Slot {
    serial: u64,
    start: Option<u64>,
}

/// The whole slots of `bytes`, the start file's. A slot whole, of a version this build doesn't
/// read, is refused, never taken for one torn, which would leave the other's start (doc-28).
fn slots(bytes: &[u8]) -> Result<Vec<Slot>, Error> {
    let mut slots = Vec::new();
    for slot in bytes.chunks(SLOT as usize) {
        let (whole, _) = decode_with(slot, 0, |_, payload| Some(payload.to_vec()));
        if let Some(payload) = whole.first() {
            slots.push(read_versioned::<Slot>(payload)?);
        }
    }
    Ok(slots)
}

/// The WAL's start, kept durably in two slots of [`START`]: each writing writes the slot the latest
/// is not in, and syncs it, so that a writing a crash tears leaves the one before. Read, it is the
/// whole slot of the higher serial number; none, before the first writing. Opened for each
/// writing, which comes with an acknowledgement, rather than held open: a host serves hundreds
/// of cells, forks most of them, which acknowledge little.
pub(crate) struct StartFile {
    made: bool,
    serial: u64,
}

impl StartFile {
    /// The start file of the cell in `dir`, and the start it holds.
    pub fn read<D: Dir>(dir: &D) -> Result<(Self, Option<u64>), Error> {
        let file = match dir.open(START, false) {
            Ok(file) => file,
            Err(err) if not_found(&err) => return Ok((Self { made: false, serial: 0 }, None)),
            Err(err) => return Err(Error::new(err).context("opening the WAL's start")),
        };
        let bytes = read_file(&file, crate::MAX_START_BYTES).context("reading the WAL's start")?;
        let slots = slots(&bytes).context("reading the WAL's start")?;
        let latest = slots.into_iter().max_by_key(|slot| slot.serial);
        let (serial, start) = latest.map_or((0, None), |slot| (slot.serial, slot.start));
        Ok((Self { made: true, serial }, start))
    }

    /// Writes `start`, durably, in `dir`: the file made, and its entry made durable, first if it
    /// isn't there.
    pub fn write<D: Dir>(&mut self, dir: &D, start: Option<u64>) -> Result<(), Error> {
        let file = match self.made {
            true => dir.open(START, false).context("opening the WAL's start")?,
            false => {
                let file = dir.create(START, false).context("making the WAL's start")?;
                dir.sync_dir("")?;
                self.made = true;
                file
            }
        };
        // A test's: its disk full, as a write of the record fails then; or, `return(eio)`, failing
        // (board 5359).
        fail::fail_point!("wal::start", |arg: Option<String>| {
            let errno = if arg.as_deref() == Some("eio") { libc::EIO } else { libc::ENOSPC };
            Err(Error::new(std::io::Error::from_raw_os_error(errno))
                .context("writing the WAL's start"))
        });
        let serial = self.serial + 1;
        let record = frame_salted(&versioned(&Slot { serial, start }), 0);
        assert!(record.len() as u64 <= SLOT, "a start's record fits its slot");
        file.write_at(&record, (serial % 2) * SLOT).context("writing the WAL's start")?;
        crate::written::wrote(crate::written::Kind::Log, record.len());
        file.sync().context("syncing the WAL's start")?;
        self.serial = serial;
        Ok(())
    }
}

/// cladefs: the corpus's segment log (doc-28): segment 3's of cell `0x2f/7`, salted `0x5a1d`,
/// under the tests' DEK of the cell, a raw-file salt of `0x33`s, holding a record whose runs were
/// synced, then one holding them.
#[cfg(test)]
pub(crate) mod golden {
    use super::*;

    const SEGMENT: SegmentId = SegmentId { cell: 7, number: 3 };

    fn encryption() -> RawEncryption {
        let (dek, key) = bucket_cell::testing::encrypting(bucket_cell::layout::test_cell(7));
        RawEncryption::of(dek, key, Salt([0x33; SALT_LEN]))
    }

    pub(crate) fn log() -> Vec<u8> {
        let encryption = encryption();
        let mut log = log_header(0x5a1d, SEGMENT, &encryption);
        let contents = |epoch: u64, extent: u64| Contents {
            commits: vec![epoch << 20 | 2],
            extents: vec![(storage_device::ExtentId::new(7, extent), 0, 4096)],
            next_sequence: extent + 1,
            ..Contents::default()
        };
        let records = [
            LogRecord { contents: contents(1, 1), runs: Runs::Checked(vec![0xc0ffee]) },
            LogRecord { contents: contents(2, 2), runs: Runs::Held(Bytes(vec![0xab; 16])) },
        ];
        for record in &records {
            let end = log.len() as u64;
            log.extend(framed(record, end, 0x5a1d, &encryption));
        }
        log
    }

    /// `bytes`, the corpus's log, as this build reads it: its salt and records, as their `{:#?}`.
    pub(crate) fn log_said(bytes: &[u8]) -> Result<String, Error> {
        let salt = log_salt(bytes, SEGMENT).ok_or_else(|| anyhow::anyhow!("no header"))?;
        let records = LogRecord::decode(bytes, SEGMENT, Some(&encryption()))?;
        Ok(format!("salt {salt:#x}\n{records:#?}"))
    }

    /// The corpus's start file: its start written twice, 5, then 9.
    pub(crate) fn start() -> Vec<u8> {
        let dir = crate::MemDir::new();
        let (mut file, _) = StartFile::read(&dir).expect("no start yet");
        file.write(&dir, Some(5)).expect("writing a start");
        file.write(&dir, Some(9)).expect("writing a start");
        read_file(&dir.open(START, false).expect("the start file"), crate::MAX_START_BYTES)
            .expect("reading it")
    }

    /// `bytes`, a start file, as this build reads its slots, as their `{:#?}`.
    pub(crate) fn start_said(bytes: &[u8]) -> Result<String, Error> {
        slots(bytes).map(|slots| format!("{slots:#?}"))
    }

    /// The layout of a start file's slots' types and a log's records'.
    pub(crate) fn fingerprint() -> String {
        [<Slot as TypeFingerprint>::fingerprint(), <LogRecord as TypeFingerprint>::fingerprint()]
            .join("\n")
    }
}

/// cladefs: a commit's record in its segment's log: its contents, and what makes the runs its
/// extents were sealed in durable with it (TASK-301): the runs themselves, for a flush that seals
/// little, which then syncs its log alone, as a database's write-ahead log holds its pages; or the
/// CRC32C of each, which recovery checks against the segment's raw data, for one whose data it
/// syncs beside its records. cladefs: a record authentic and whole that doesn't decode as one is
/// refused, never read in another form (board 10062).
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub(crate) struct LogRecord {
    pub contents: Contents,
    pub runs: Runs,
}

/// What a [`LogRecord`] holds of the runs its commit sealed, in the order its contents name them
/// (`Contents::runs`: cladefs: those of the commit it goes on with too, TASK-92.41).
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub(crate) enum Runs {
    /// Nothing: its data was synced before it.
    Synced,
    /// The CRC32C of each.
    Checked(Vec<u32>),
    /// Their bytes, one after another.
    Held(Bytes),
}

/// cladefs: bytes, which bincode writes as a length and the bytes, not a sequence of each byte's
/// value, as it would a `Vec<u8>`: a record holding a commit's runs encodes as fast as it copies.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Bytes(pub Vec<u8>);

/// As bincode writes it: bytes, not a sequence.
impl TypeFingerprint for Bytes {
    fn fingerprint() -> String {
        "bytes".to_owned()
    }
}

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(bytes.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(bytes))
            }
        }
        deserializer.deserialize_byte_buf(Visitor)
    }
}

impl LogRecord {
    /// The record of `contents`, made in a segment whose raw data `raw` is: the runs it sealed
    /// read back, and held, if `hold`, or checksummed.
    pub fn of<F: File>(contents: Contents, raw: &F, hold: bool) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        let mut crcs = Vec::with_capacity(contents.extents.len());
        for &(extent, offset, len) in contents.runs() {
            let start = bytes.len();
            bytes.resize(start + len as usize, 0);
            raw.read_at(&mut bytes[start..], offset)
                .with_context(|| format!("reading {extent:?}'s run back"))?;
            if !hold {
                crcs.push(extent_frame::crc(&bytes[start..]));
                bytes.truncate(start);
            }
        }
        let runs = if hold { Runs::Held(Bytes(bytes)) } else { Runs::Checked(crcs) };
        Ok(Self { contents, runs })
    }

    /// The bytes of the runs `contents` names.
    pub fn run_bytes(contents: &Contents) -> u64 {
        contents.runs().fold(0u64, |acc, &(_, _, len)| acc.saturating_add(len))
    }

    /// Makes `raw`, its segment's raw data, hold the runs it names, as its commit wrote them:
    /// those it holds, written into it; whether it does, those it checks, or those synced.
    fn restore(&self, raw: &mut Vec<u8>) -> bool {
        let Some(runs) = self
            .contents
            .runs()
            .map(|&(_, offset, len)| {
                let end = offset.checked_add(len)?;
                Some(usize::try_from(offset).ok()?..usize::try_from(end).ok()?)
            })
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        match &self.runs {
            Runs::Synced => true,
            Runs::Checked(crcs) => {
                crcs.len() == runs.len()
                    && runs.into_iter().zip(crcs).all(|(run, &crc)| {
                        raw.get(run).is_some_and(|run| extent_frame::crc(run) == crc)
                    })
            }
            Runs::Held(Bytes(bytes)) => {
                let mut at = 0usize;
                for run in runs {
                    let Some(next_at) = at.checked_add(run.len()) else { return false };
                    let Some(held) = bytes.get(at..next_at) else { return false };
                    if raw.len() < run.end {
                        raw.resize(run.end, 0);
                    }
                    raw[run.clone()].copy_from_slice(held);
                    at = next_at;
                }
                at == bytes.len()
            }
        }
    }

    /// The records of `bytes`, `segment`'s log, that are whole: those of any kind, after its
    /// header, salted as it says ([`LOG_MAGIC`]), as a log reused does, whose records from before
    /// are torn then; none if its header isn't `segment`'s ([`log_salt`]). cladefs: each followed
    /// by its MAC, which is checked if `encryption`, its encryption, is given: one not authentic
    /// fails it, naming it; read holding no key, as recovery reads, it is not checked (TASK-411.5).
    pub(crate) fn decode(
        bytes: &[u8],
        segment: SegmentId,
        encryption: Option<&RawEncryption>,
    ) -> Result<Vec<Self>, Error> {
        Self::decode_to(bytes, segment, encryption).map(|(records, _)| records)
    }

    /// cladefs: the records of `bytes` as [`LogRecord::decode`] reads them, and where in it the
    /// last ends: what of it is whole (TASK-684).
    fn decode_to(
        bytes: &[u8],
        segment: SegmentId,
        encryption: Option<&RawEncryption>,
    ) -> Result<(Vec<Self>, usize), Error> {
        if let Some(found) = unsupported_log_magic(bytes) {
            anyhow::bail!(
                "{segment:?}'s log has unsupported format {found:?}, not {LOG_MAGIC:?}: refused"
            );
        }
        let Some(header) = LogHeader::of(bytes, segment) else { return Ok((Vec::new(), 0)) };
        let bytes = bytes.get(LOG_RECORDS as usize..).unwrap_or_default();
        let refused = std::cell::RefCell::new(None);
        let (records, end) = decode_with(bytes, header.salt, |at, payload| {
            let (payload, mac) = payload.split_at_checked(payload.len().checked_sub(MAC_LEN)?)?;
            let at = LOG_RECORDS + at as u64;
            let mac = mac.try_into().expect("a MAC's bytes");
            if encryption.is_some_and(|encryption| !encryption.verify(at, payload, mac)) {
                refused.replace(Some((at, "is not authentic".to_owned())));
                return None;
            }
            // Hashed whole, and authentic where it is checked: a record of another form or of a
            // version this build doesn't read, never read as one, nor taken for the log's torn end
            // (board 10062, doc-28).
            match read_versioned::<Self>(payload) {
                Ok(record) => Some(record),
                Err(err) => {
                    refused.replace(Some((at, format!("is not a record this build reads: {err}"))));
                    None
                }
            }
        });
        match refused.into_inner() {
            Some((at, why)) => Err(anyhow::anyhow!("{segment:?}'s log: its record at {at} {why}")),
            None => Ok((records, LOG_RECORDS as usize + end)),
        }
    }
}

/// cladefs: what a segment's log starts with: its header, a block, its salt after this, then its
/// records, each salted with it, so that a log reused, a spare, holding records of another
/// segment's after them, reads as holding its own alone (TASK-301): a log made ahead of its records
/// is overwritten as it is appended to, a sync then syncing the records alone, not the log's size
/// and blocks too, a third of the time on a cloud disk. The header is synced before any record is
/// appended: a log whose new header a crash lost would read its old one's records. cladefs: after
/// the salt, the segment the log is of, the cell's then the number: a spare is renamed a log, and
/// was renamed a spare, neither rename durable until a flush syncs the directory of logs, so a
/// crash may keep it under its last segment's name, holding the new one's header and records, which
/// an opening took for the last one's commits (TASK-341, found by TASK-310's crash rounds, round
/// 81); a log whose header names another segment holds none of its own. cladefs: after those
/// fields, the DEK its segment's raw file is encrypted under and its log authenticated with, its
/// cell's and its generation, and the salt of their subkeys (`raw::RawEncryption`), then the
/// header's MAC, then an 8-byte rapidhash of the header fields and MAC so a member holding no key
/// detects a torn or corrupted header on reopen (board 6902); each record of it is followed by its
/// MAC, of its offset in the log and its bytes (TASK-411.5). Its records hold of the runs of its
/// commits what the raw file stores, encrypted. Every cell's are so (TASK-411.28): a log headed in
/// the clear, `FXLOG\0\0\x02` as before, or `FXLOG\0\0\x03` without the header hash, is refused.
/// cladefs: `FXLG`, then the cell directory's version (doc-28, board 10044), where logs of 16 and
/// 17 said `FXLOG\0\0\x04`; and each record says its version too, as a log may outlive the build
/// that headed it ([`crate::layout::versioned`]).
pub(crate) const LOG_MAGIC: &[u8; 8] = &{
    let version = FORMAT.current.to_le_bytes();
    [b'F', b'X', b'L', b'G', version[0], version[1], version[2], version[3]]
};

/// What a log of 18 on begins with, its version after it.
const LOG_KIND: &[u8; 4] = b"FXLG";

/// cladefs: whether `log` begins as a log of a format this build doesn't read does: an older
/// `FXLOG\0\0*`, or `FXLG` and a version outside [`FORMAT`]'s; its first bytes, if so. Refused
/// rather than treated as empty and deleted by recovery (board 6902, doc-28).
pub(crate) fn unsupported_log_magic(log: &[u8]) -> Option<[u8; 8]> {
    let (&magic, _) = log.split_first_chunk::<8>()?;
    let (kind, version) = magic.split_at(4);
    let refused = match kind == LOG_KIND {
        true => FORMAT.check(u32::from_le_bytes(version.try_into().unwrap())).is_err(),
        false => magic.starts_with(b"FXLOG\0\0"),
    };
    refused.then_some(magic)
}

/// cladefs: where a segment's log's records start, after its header.
pub(crate) const LOG_RECORDS: u64 = 4096;

/// cladefs: the bytes of a log's header's first fields ([`LOG_MAGIC`]).
const LOG_FIELDS: usize = LOG_MAGIC.len() + 3 * 8;

/// cladefs: the bytes of the header's keyless integrity hash (board 6902).
const LOG_HASH: usize = 8;

/// cladefs: the bytes of a log's header that say anything ([`LOG_MAGIC`]): what a reader of its
/// header reads.
pub(crate) const LOG_HEADER: usize = LOG_FIELDS + 8 + 4 + SALT_LEN + MAC_LEN + LOG_HASH;

/// cladefs: the bytes of a log's header its MAC is of: all before the MAC.
const LOG_SIGNED: usize = LOG_FIELDS + 8 + 4 + SALT_LEN;

/// cladefs: the bytes of a log's header its keyless integrity hash covers: all before the hash.
pub(crate) const LOG_HASHED: usize = LOG_HEADER - LOG_HASH;

/// cladefs: the header of `segment`'s log, its records salted with `salt`, MACed by `encryption`,
/// which encrypts its raw file and log ([`LOG_MAGIC`]).
pub(crate) fn log_header(salt: u64, segment: SegmentId, encryption: &RawEncryption) -> Vec<u8> {
    let mut header = vec![0; LOG_RECORDS as usize];
    let fields = [salt, segment.cell, segment.number].map(u64::to_le_bytes);
    header[..LOG_MAGIC.len()].copy_from_slice(LOG_MAGIC);
    header[LOG_MAGIC.len()..LOG_FIELDS].copy_from_slice(fields.as_flattened());
    let mut at = LOG_FIELDS;
    let key = (encryption.key.cell.to_le_bytes(), encryption.key.generation.to_le_bytes());
    for field in [&key.0[..], &key.1[..], &encryption.salt.0[..]] {
        header[at..at + field.len()].copy_from_slice(field);
        at += field.len();
    }
    let mac = encryption.mac(0, &header[..at]);
    header[at..at + MAC_LEN].copy_from_slice(&mac);
    at += MAC_LEN;
    let hash = rapidhash::rapidhash(&header[..at]);
    header[at..at + LOG_HASH].copy_from_slice(&hash.to_le_bytes());
    header
}

/// cladefs: a segment's log's header, as its bytes say ([`LogHeader::of`]): the salt of its
/// records, and the DEK and salt of its encryption, with the bytes its MAC is of, and the MAC.
pub(crate) struct LogHeader {
    pub salt: u64,
    encrypted: (KeyId, Salt, [u8; MAC_LEN]),
    signed: Vec<u8>,
}

impl LogHeader {
    /// The header of `log`, `segment`'s log's bytes, if it has one, and it is `segment`'s.
    /// cladefs: the log's bytes, its host's disk's or a member's, untrusted: read through
    /// [`Cursor`], none of a log shorter than a header, whatever its first bytes (board 5436).
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn of(log: &[u8], segment: SegmentId) -> Option<Self> {
        let (header, _) = log.split_first_chunk::<LOG_HEADER>()?;
        let mut fields = Cursor::new(header);
        if fields.array()? != *LOG_KIND || FORMAT.check(fields.u32()?).is_err() {
            return None;
        }
        let (salt, cell, number) = (fields.u64()?, fields.u64()?, fields.u64()?);
        if (SegmentId { cell, number }) != segment {
            return None;
        }
        let key = KeyId { cell: fields.u64()?, generation: fields.u32()? };
        let key_salt = Salt(fields.array()?);
        let mac = fields.array()?;
        let hash = fields.u64()?;
        let (hashed, _) = header.split_first_chunk::<LOG_HASHED>()?;
        if rapidhash::rapidhash(hashed) != hash {
            return None;
        }
        let (signed, _) = header.split_first_chunk::<LOG_SIGNED>()?;
        Some(Self { salt, encrypted: (key, key_salt, mac), signed: signed.to_vec() })
    }

    /// The encryption of `segment`'s raw file and log, as this, its log's header, says, under the
    /// DEK of `keys`' it names, the header's MAC checked: refused if `keys` hold no such DEK, or
    /// its MAC is not the header's.
    pub fn encryption(
        &self,
        segment: SegmentId,
        keys: &bucket_cell::keys::Ring,
    ) -> Result<Arc<RawEncryption>, Error> {
        let (key, salt, mac) = &self.encrypted;
        let keys = keys.of(segment.cell);
        let dek = extent_frame::Keys::dek(&keys, *key).ok_or_else(|| {
            anyhow::anyhow!(
                "{segment:?}'s raw file is encrypted under {key:?}, which the cell holds not"
            )
        })?;
        let encryption = RawEncryption::of(dek, *key, *salt);
        ensure!(
            encryption.verify(0, &self.signed, mac),
            "{segment:?}'s log: its header is not authentic"
        );
        Ok(Arc::new(encryption))
    }
}

/// cladefs: the salt of the records of `log`, `segment`'s log's bytes, as its header says, if it
/// has one, and it is `segment`'s.
pub(crate) fn log_salt(log: &[u8], segment: SegmentId) -> Option<u64> {
    LogHeader::of(log, segment).map(|header| header.salt)
}

/// cladefs: a salt for the records of a log of `segment`'s made in `lifetime`, a value of the
/// opening of the cell that made it ([`crate::segments::Segments::open`]): none another log
/// had, as a cell whose segments all left numbers its next from 0 again, its epoch past theirs;
/// and none 0, the salt of logs before they had headers.
pub(crate) fn new_salt(lifetime: u64, segment: SegmentId) -> u64 {
    let mut bytes = [0; 24];
    bytes[..8].copy_from_slice(&lifetime.to_le_bytes());
    bytes[8..16].copy_from_slice(&segment.cell.to_le_bytes());
    bytes[16..].copy_from_slice(&segment.number.to_le_bytes());
    rapidhash::rapidhash(&bytes) | 1
}

/// The log of the commits made in a segment ([`log_path`]), open, or closed since: where its next
/// record goes, which the flushes append, one at a time, in their commits' order.
pub(crate) struct SegmentLog<F> {
    file: Arc<F>,
    end: u64,
    salt: u64,
    /// cladefs: what MACs its records (TASK-411.5).
    encryption: Arc<RawEncryption>,
}

impl<F: File> SegmentLog<F> {
    /// A segment's log, `file`, holding no records yet, after its header ([`log_header`]), made or
    /// reused with it, its records salted with `salt`; cladefs: and MACed by `encryption`.
    pub fn new(file: Arc<F>, salt: u64, encryption: Arc<RawEncryption>) -> Self {
        Self { file, end: LOG_RECORDS, salt, encryption }
    }

    /// Appends `records` after those before, encoded, salted ([`crate::layout::encode_salted`]),
    /// and syncs them. cladefs: keeping the log's pages in the host's page cache, as the open
    /// segment's raw file keeps its (TASK-301): a log is appended to and synced again and again,
    /// and its next record's append would read its last page back from the disk first. cladefs:
    /// each followed by its MAC ([`LOG_MAGIC`]).
    pub fn append<'a>(
        &mut self,
        records: impl IntoIterator<Item = &'a LogRecord>,
    ) -> Result<(), Error> {
        let start = self.end;
        for record in records {
            let record = &framed(record, self.end, self.salt, &self.encryption);
            self.file.write_at(record, self.end)?;
            crate::written::wrote(crate::written::Kind::Log, record.len());
            self.end += record.len() as u64;
        }
        if self.end > start {
            self.file.sync()?;
        }
        Ok(())
    }
}

/// `record` as a log holds it at `at`: encoded, MACed at `at` by `encryption` (TASK-411.5), and
/// framed, salted with `salt` ([`LOG_MAGIC`]).
pub(crate) fn framed(
    record: &LogRecord,
    at: u64,
    salt: u64,
    encryption: &RawEncryption,
) -> Vec<u8> {
    let mut payload = versioned(record);
    let mac = encryption.mac(at, &payload);
    payload.extend_from_slice(&mac);
    frame_salted(&payload, salt)
}

/// A cell's WAL as its directory holds it, recovered ([`recover`]): its start; its segments,
/// footers by their numbers, in order; and the number of the next segment, past every one the
/// directory held. cladefs: of those, the ones closed again, not yet framed, by their numbers, with
/// the records of their logs their footers fold, their raw files and logs still there: framed once
/// the cell's keys are known ([`Recovered::close`], TASK-411.9).
pub(crate) struct Recovered {
    pub start: Option<u64>,
    pub start_file: StartFile,
    pub segments: Vec<(Footer, u64)>,
    pub next: u64,
    pub closing: BTreeMap<u64, Vec<Contents>>,
}

impl Recovered {
    /// cladefs: frames the segments recovery closed again ([`Recovered::closing`]), in `dir`, of
    /// `cell`, holding `keys`, the cell's: encrypted under its cell's newest DEK, or as in-place
    /// `Encryption::Sectors` if `sectors` (for ephemeral cells, board 6905); each from its raw
    /// data and its log's records read again, which must still be those recovery kept, with the
    /// footer recovery folded them into, and synced; then the directory of framed files synced, and
    /// their raw files and logs removed, as the commits they hold are durable in the framed files
    /// then. Recovery frames nothing itself: it holds no key yet, and a sealed cell's segment
    /// framed in the clear and resealed later left its data on the host's disk in the clear
    /// meanwhile, and for good if the host died between (TASK-411.9). A crash before leaves the raw
    /// files and logs, which the next recovery closes again alike. cladefs: an encrypted cell's raw
    /// data is decrypted first, under the DEK and salt its log's header names, which the header's
    /// MAC and each record's must be authentic under, as recovery read them holding no key
    /// (TASK-411.5).
    pub fn close<D: Dir>(
        &mut self,
        dir: &D,
        cell: CellId,
        keys: &bucket_cell::keys::Ring,
        sectors: bool,
    ) -> Result<(), Error> {
        if self.closing.is_empty() {
            return Ok(());
        }
        let encrypting = keys.try_encrypting()?;
        for (footer, number) in &self.segments {
            let Some(kept) = self.closing.get(number) else { continue };
            let segment = SegmentId { cell: cell.id, number: *number };
            let Logged { records, mut data, encryption, .. } =
                log_records(dir, segment, Some(keys))?;
            ensure!(
                records.get(..kept.len()) == Some(&kept[..]),
                "{segment:?}'s log changed since its recovery"
            );
            // cladefs: to the end of its last run: past it, nothing the records name, as a framing
            // in place cut short leaves the raw file with its tail, torn (TASK-310).
            let end = footer.contents.runs().map(|&(_, offset, len)| offset + len).max();
            data.truncate((end.unwrap_or(0) as usize).min(data.len()));
            // cladefs: its runs are whole sectors (TASK-411.5, TASK-411.8).
            let encryption = encryption.with_context(|| format!("{segment:?} has no log"))?;
            ensure!(
                (data.len() as u64).is_multiple_of(SECTOR),
                "{segment:?}'s runs end at {}, inside a sector",
                data.len()
            );
            let path = segment_path(segment);
            remove(dir, &path)?;
            let file = dir.create(&path, true)?;
            let encoded_footer = bucket_cell::layout::encode_segment_footer(&footer);
            if sectors {
                let mut packer = extent_frame::Packer::in_place(encryption.sectors().clone());
                let raw_parts =
                    data.chunks(extent_frame::PART as usize).enumerate().map(|(index, chunk)| {
                        extent_frame::RawPart::of(index as u64, chunk, encryption.sectors())
                    });
                packer.push_raw_parts(raw_parts);
                let tail = packer.finish(&encoded_footer);
                if !data.is_empty() {
                    file.write_at(&data, 0)?;
                }
                file.write_at(&tail, data.len() as u64)?;
                crate::written::wrote(crate::written::Kind::Framed, data.len() + tail.len());
            } else {
                encryption.decrypt(0, &mut data);
                let framed =
                    extent_frame::framed(&data, &encoded_footer, (&encrypting.0, encrypting.1));
                file.write_at(&framed, 0)?;
                crate::written::wrote(crate::written::Kind::Framed, framed.len());
            }
            file.sync()?;
        }
        dir.sync_dir(&cell_dir(SEGMENTS, cell.id))?;
        dir.barrier()?;
        for number in std::mem::take(&mut self.closing).into_keys() {
            let segment = SegmentId { cell: cell.id, number };
            remove(dir, &open_segment_path(segment))?;
            remove(dir, &log_path(segment))?;
        }
        Ok(())
    }

    /// cladefs: drops what it holds of the segments closed again, not yet framed: their raw files
    /// and logs, durably, so that no recovery closes them again (`Recovering::drop_wal`).
    pub fn drop_closing<D: Dir>(&mut self, dir: &D, cell: CellId) -> Result<(), Error> {
        if self.closing.is_empty() {
            return Ok(());
        }
        for number in std::mem::take(&mut self.closing).into_keys() {
            let segment = SegmentId { cell: cell.id, number };
            remove(dir, &open_segment_path(segment))?;
            remove(dir, &log_path(segment))?;
        }
        for kind in [OPEN_SEGMENTS, LOGS] {
            dir.sync_dir(&cell_dir(kind, cell.id))?;
        }
        Ok(())
    }
}

/// Drops every local WAL file of `cell` in `dir` without parsing or syncing them and returns an
/// empty [`Recovered`] (doc-29, Hotbed review 118, board 12181): used only after verifying the
/// cold bucket head has an active Fast WAL so local disk sync/removal failures never prevent
/// reopening when both remote buckets are healthy.
pub(crate) fn drop_all<D: Dir>(dir: &D, cell: CellId) -> Result<Recovered, Error> {
    for kind in [SEGMENTS, OPEN_SEGMENTS, LOGS, crate::layout::EXTENTS] {
        let sub = cell_dir(kind, cell.id);
        let listed = match dir.list(&sub) {
            Ok(names) => names,
            Err(err) if not_found(&err) => continue,
            Err(err) => {
                log::warn!(
                    "cladefs: listing damaged cache dir {sub} during fast WAL reopen: {err}"
                );
                continue;
            }
        };
        for name in listed {
            let path = format!("{sub}/{name}");
            if let Err(err) = remove(dir, &path) {
                log::warn!(
                    "cladefs: removing damaged cache file {path} during fast WAL reopen: {err}"
                );
            }
        }
    }
    if let Err(err) = remove(dir, START) {
        log::warn!("cladefs: removing damaged WAL start file during fast WAL reopen: {err}");
    }
    Ok(Recovered {
        start: None,
        start_file: StartFile { made: false, serial: 0 },
        segments: Vec::new(),
        next: 0,
        closing: BTreeMap::new(),
    })
}

/// Recovers the WAL of `cell` in `dir` (see the module's docs): each segment whose framed file
/// is whole, in order, and each whose framed file is not that its log closes again, holding
/// commits, to the last durable commit; and nothing else of segments: raw data, logs and framed
/// files torn, closed again or holding nothing durable, go. For an ephemeral cell, `ephemeral`
/// holds `Some(through)`, the last segment covered by its local checkpoint: segments `<= through`
/// stay in `segments/` untouched by WAL recovery, and acknowledged segments are kept locally.
pub(crate) fn recover<D: Dir>(
    dir: &D,
    cell: CellId,
    ephemeral: Option<Option<u64>>,
) -> Result<Recovered, Error> {
    let (start_file, start) = StartFile::read(dir)?;
    let listed = |kind| -> Result<BTreeSet<u64>, Error> {
        Ok(list_numbers(dir, &cell_dir(kind, cell.id))?.into_iter().collect())
    };
    // cladefs: framed files a crash stopped being made durable, named their own: their segments are
    // closed again from their raw files and logs, which stay until they are (TASK-310).
    for name in dir.list(&cell_dir(SEGMENTS, cell.id))? {
        if name.starts_with(FRAMING) {
            remove(dir, &format!("{}/{name}", cell_dir(SEGMENTS, cell.id)))?;
        }
    }
    let (framed, raw, logs) = (listed(SEGMENTS)?, listed(OPEN_SEGMENTS)?, listed(LOGS)?);
    let covered = |number: u64| ephemeral.flatten().is_some_and(|through| number <= through);
    let numbers: BTreeSet<u64> =
        framed.iter().chain(&raw).chain(&logs).copied().filter(|&n| !covered(n)).collect();
    let left = |number| raw.contains(&number) || logs.contains(&number);
    // cladefs: the logs that are their segments' own, their headers naming them: one renamed a
    // spare and taken as another segment's log, which a crash brought back under its old name,
    // holds none of its segment's records, and says nothing of whether its segment was made
    // durable: a segment the start acknowledged, its files' removal undone, its log a spare taken
    // since, read as one past the start whose raw file and log are both there, broke the run of
    // segments framed whole, those after it closed again from their raw files and logs, which they
    // hold no more, their commits lost (TASK-369, the Shuttle rounds, with the next segment's log
    // prepared from a spare as soon as it opens).
    let mut own = BTreeSet::new();
    for &number in &logs {
        if own_log(dir, SegmentId { cell: cell.id, number })? {
            own.insert(number);
        }
    }
    // What a segment holds is in its framed file, durably, or nothing durable is: its raw data and
    // log go.
    let settled = |number| -> Result<(), Error> {
        if left(number) {
            let segment = SegmentId { cell: cell.id, number };
            dir.sync_dir(&cell_dir(SEGMENTS, cell.id))?;
            dir.barrier()?;
            remove(dir, &open_segment_path(segment))?;
            remove(dir, &log_path(segment))?;
        }
        Ok(())
    };
    // A log's commits the start acknowledged are the bucket's: its framed file left, its raw data
    // and log a crash brought back, it holds nothing of the log's. cladefs: a commit's record going
    // on with it past the segment, the commit's (TASK-92.41).
    let after_start = |records: &[Contents]| {
        let last = records.iter().rev().find_map(last_commit);
        last.is_some_and(|last| start.is_none_or(|start| epoch_of(last) > epoch_of(start)))
    };
    // A segment holding nothing goes, its framed file's removal durable before its raw data's and
    // log's, as its framing was before they went: a framed file a crash kept without them, which
    // the next recovery takes as framed whole, unchecked, is whole (TASK-318).
    let gone = |number| -> Result<(), Error> {
        let segment = SegmentId { cell: cell.id, number };
        remove(dir, &segment_path(segment))?;
        if framed.contains(&number) && left(number) {
            dir.sync_dir(&cell_dir(SEGMENTS, cell.id))?;
            dir.barrier()?;
        }
        remove(dir, &open_segment_path(segment))?;
        remove(dir, &log_path(segment))
    };
    // What each segment holds, in order. cladefs: its framed file, whole, only while every segment
    // before it is framed whole, or holds only commits the start acknowledged; after the first that
    // isn't, each whose raw file and log are both still there is closed again from them, as a
    // torn one is (TASK-310). Segments are made durable several at once, a later one's framed file
    // whole while an earlier one's is not, its commits not durable then, which its log lacks: the
    // later footer, its record of commits after those, or of the rest of a commit spanning both,
    // read whole, would hold the cell past what the earlier one lost (TASK-310's crash rounds).
    // Their logs hold every commit a flush made durable, which is all a crash must keep. One whose
    // log is gone, or its raw file, was durable, its commits too, as they go only then: its
    // making durable made their entries durable before its framed file's
    // (`Segments::unsynced_entries`).
    let mut found = Vec::new();
    let mut broken = false;
    // cladefs: the segments whose logs hold a record whose runs their raw data doesn't hold: its
    // commit's data not durable, the commit isn't, nor those after it (`log_records`); and those
    // which end what is durable so, holding nothing durable, their framed file not whole, nor any
    // record: one whose raw file or log is there, past the segments the start acknowledged, made
    // as the next commits' flushes would have synced its entries, which no durable commit's did
    // (TASK-334). A segment the start acknowledged, its files' removal undone, comes before those
    // past it.
    let mut refusing = BTreeSet::new();
    let mut ending = BTreeSet::new();
    let mut past_start = start.is_none();
    for number in numbers {
        let segment = SegmentId { cell: cell.id, number };
        let again = broken && raw.contains(&number) && own.contains(&number);
        let footer = match framed.contains(&number) && !again {
            true => whole_footer(dir, segment, left(number))?,
            false => None,
        };
        let records = match (&footer, raw.contains(&number)) {
            (None, true) => {
                let Logged { records, refused, .. } = log_records(dir, segment, None)?;
                if refused {
                    refusing.insert(number);
                }
                records
            }
            _ => Vec::new(),
        };
        // Not whole, and not one the start acknowledged, whose removal a crash undid: one whose
        // raw file or log is gone was durable, as they go only then.
        let lost = raw.contains(&number) && own.contains(&number);
        broken |= footer.is_none() && lost && (records.is_empty() || after_start(&records));
        let holds_nothing = past_start && records.is_empty();
        if footer.is_none() && left(number) && (refusing.contains(&number) || holds_nothing) {
            ending.insert(number);
        }
        past_start |= after_start(&records)
            || footer.as_ref().is_some_and(|footer| {
                start.is_none_or(|start| footer_last(footer) > epoch_of(start))
            });
        found.push(match footer {
            Some(footer) => (number, Found::Whole(footer)),
            None if after_start(&records) => (number, Found::Torn(records)),
            None => (number, Found::Nothing),
        });
    }
    // cladefs: whether the start acknowledged a segment's commits, its whole.
    let acknowledged =
        |footer: &Footer| start.is_some_and(|start| footer_last(footer) <= epoch_of(start));
    // cladefs: whether the commit the segment found at `at` goes on with past it, `commit`, ends:
    // the segments after it, one after another, each going on with it, to one ending it
    // (TASK-92.41). A flush appends a commit's records to its segments' logs in their order, the
    // one ending it last, but checks of their runs against the raw data may stop at any of them.
    let ended = |at: usize, commit: u64| -> bool {
        let mut number = found[at].0;
        for (next, found) in &found[at + 1..] {
            if *next != number + 1 {
                return false;
            }
            number = *next;
            match found.goes_on(commit) {
                Some(true) => return true,
                Some(false) => {}
                None => return false,
            }
        }
        false
    };
    // Each segment, in order: whole, framed; or, its framed file not whole, closed again from its
    // log, which holds the records of the commits made in it that a flush made durable, at least;
    // or holding no durable commit, its files going. One closed again whose log lacks its last
    // commits, the next segment holding commits not following its last, is the last: those after it
    // go, durably, first (see the module's docs). cladefs: and one going on with a commit that no
    // segment after it ends is the last, closed again without it, from its raw data and log, which
    // stay until the commit is durable: the commit is not.
    let mut segments: Vec<(Footer, u64)> = Vec::new();
    let mut closing = BTreeMap::new();
    for (at, (number, found_here)) in found.iter().enumerate() {
        let number = *number;
        let segment = SegmentId { cell: cell.id, number };
        let continues = |record: &Contents| record.continued.is_some();
        // cladefs: those from `from` on go, durably, before any is closed again: those after it
        // first, then it, whose files say why they go, a log refusing a record or a commit past
        // the last kept, which a crash between its files' removals and theirs would leave gone,
        // and theirs kept (TASK-334's opening rounds).
        let cut = |from: usize| -> Result<(), Error> {
            let synced = || -> Result<(), Error> {
                for kind in [SEGMENTS, OPEN_SEGMENTS, LOGS] {
                    dir.sync_dir(&cell_dir(kind, cell.id))?;
                }
                dir.barrier().map_err(Error::from)
            };
            for (number, _) in found[from + 1..].iter().rev() {
                gone(*number)?;
            }
            synced()?;
            gone(found[from].0)?;
            synced()
        };
        let (mut records, unended) = match found_here {
            // One the start acknowledged, whose files' removal a crash undid, goes below: its
            // commit is the bucket's, whole, whatever is left of the segments after it.
            Found::Whole(footer) => match &footer.contents.continued {
                Some(continued) if !acknowledged(footer) && !ended(at, continued.commit) => {
                    (log_records(dir, segment, None)?.records, true)
                }
                _ => {
                    settled(number)?;
                    segments.push((footer.clone(), number));
                    continue;
                }
            },
            // cladefs: one whose log refused a record, or holds none, its raw file and log there,
            // holds the first commit not durable, if any: it and those after go (TASK-334): a
            // commit spanning it and the next, its record in the next's log whole, its runs there
            // durable, its record here lost or refused, read whole, would be read without them.
            Found::Nothing if ending.contains(&number) => {
                cut(at)?;
                break;
            }
            Found::Nothing => {
                gone(number)?;
                continue;
            }
            Found::Torn(records) => {
                // cladefs: its log goes on from the last commit kept, the start's or a segment's,
                // or it and those after go, durably: a flush appends its commits' records to the
                // logs of their segments at once, and a crash may keep a later commit's record, in
                // this one's log, and lose an earlier one's, in the log of a segment before, which
                // went, holding nothing durable (TASK-334, found by TASK-310's Shuttle crash
                // rounds).
                let kept = segments.last().map(|(footer, _)| footer_last(footer));
                let kept = kept.max(start.map(epoch_of));
                if let (Some(first), Some(kept)) = (first_epoch(records), kept)
                    && first > kept + 1
                {
                    cut(at)?;
                    break;
                }
                let continued = records.last().and_then(|record| record.continued.as_ref());
                let unended = continued.is_some_and(|continued| !ended(at, continued.commit));
                (records.clone(), unended)
            }
        };
        let goes_on = !unended && records.last().is_some_and(continues);
        if unended && records.last().is_some_and(continues) {
            records.pop();
        }
        let last = records.iter().rev().find_map(|record| record.commits.last()).copied();
        let next = found[at + 1..].iter().find_map(|(_, found)| found.first_commit());
        let follows = next.is_some_and(|next| last.is_some_and(|last| next == epoch_of(last) + 1));
        // cladefs: and not if its log refused a record, its commit not durable (TASK-334).
        let whole = !refusing.contains(&number) && (goes_on || (!unended && follows));
        if !whole && at + 1 < found.len() {
            cut(at + 1)?;
        }
        if !after_start(&records) {
            gone(number)?;
            break;
        }
        if framed.contains(&number) {
            remove(dir, &segment_path(segment))?;
        }
        // cladefs: framed once the cell's keys are known, its raw file and log kept until then
        // (`Recovered::close`, TASK-411.9).
        let previous = segments.last().map(|(footer, _)| footer);
        segments.push((closed_again(cell, previous, &records)?, number));
        closing.insert(number, records);
        if !whole {
            break;
        }
    }
    // Those acknowledged by the start, whose files' removal a crash undid, are the bucket's: they
    // go again. For an ephemeral cell, segments live locally until all their extents are freed.
    let (gone, segments): (Vec<_>, Vec<_>) =
        segments.into_iter().partition(|(footer, _)| acknowledged(footer));
    if ephemeral.is_none() {
        for (footer, number) in gone {
            remove(dir, &segment_path(SegmentId { cell: cell.id, number }))?;
            for (extent, _) in footer.own() {
                remove(dir, &extent_path(extent))?;
            }
        }
    }
    let after_files = match framed.iter().chain(&raw).chain(&logs).max() {
        Some(&last) => last
            .checked_add(1)
            .with_context(|| format!("cell {cell} segment number {last} overflows u64"))?,
        None => 0,
    };
    let after_through = match ephemeral.flatten() {
        Some(through) => through.checked_add(1).with_context(|| {
            format!("ephemeral cell {cell} checkpoint through {through} overflows u64")
        })?,
        None => 0,
    };
    let next = after_files.max(after_through);
    Ok(Recovered { start, start_file, segments, next, closing })
}

/// The footer of `segment`'s framed file, if it is whole: checked whole, data and all, if `left`,
/// its raw data and log still there, as its framing may have stopped part way; else whole, its
/// framing having made it durable before they went, or not whole, a crash cutting short its
/// removal, as holding nothing durable, after its raw data and log went (TASK-165.9). Synced, if
/// checked.
fn whole_footer<D: Dir>(dir: &D, segment: SegmentId, left: bool) -> Result<Option<Footer>, Error> {
    let path = segment_path(segment);
    let file = dir.open(&path, true)?;
    let footer = match read_frame(&file, &extent_frame::NoKeys)? {
        // cladefs: a footer that passed its check but doesn't decode, another build's or a bug's,
        // fails the recovery: taken for none, its segment's commits would be taken for lost.
        Some((_, footer)) => segment_footer(&footer).with_context(|| path.clone())?,
        None => None,
    };
    if !left {
        return Ok(footer);
    }
    // cladefs: whole by the keyless check, its parts' stored bytes against their CRCs, encrypted or
    // not: what a recovery holding no key yet tells of an encrypted one, never taken for torn and
    // closed again from its raw file (TASK-411.3).
    let whole = footer.is_some() && extent_frame::check(&read_whole(&file)?).is_ok();
    if whole {
        file.sync()?;
    }
    Ok(footer.filter(|_| whole))
}

/// cladefs: whether `segment`'s log is its own: there, its header naming it (`LOG_MAGIC`).
fn own_log<D: Dir>(dir: &D, segment: SegmentId) -> Result<bool, Error> {
    let log = match dir.open(&log_path(segment), false) {
        Ok(log) => log,
        Err(err) if not_found(&err) => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    if log.size()? < LOG_HEADER as u64 {
        return Ok(false);
    }
    let mut header = [0; LOG_HEADER];
    log.read_at(&mut header, 0)?;
    if let Some(found) = unsupported_log_magic(&header) {
        anyhow::bail!(
            "{segment:?}'s log has unsupported format {found:?}, not {LOG_MAGIC:?}: refused"
        );
    }
    Ok(log_salt(&header, segment).is_some())
}

/// cladefs: the cell's epoch of the first commit `records`, a segment's log's, hold: the first a
/// record ends, or goes on with (TASK-92.41).
fn first_epoch(records: &[Contents]) -> Option<u64> {
    let first = records.first()?;
    let commit =
        first.commits.first().or(first.continued.as_ref().map(|continued| &continued.commit));
    commit.copied().map(epoch_of)
}

/// What recovery finds of a segment: its framed file whole; or not, and the records of its log,
/// holding commits after the WAL's start, to close it again from, with its raw data; or nothing
/// durable.
enum Found {
    Whole(Footer),
    Torn(Vec<Contents>),
    Nothing,
}

impl Found {
    /// cladefs: whether it ends `commit`, which the segment before it went on with, or goes on with
    /// it, ending none: `Some(true)` or `Some(false)`; none if neither (TASK-92.41). Its first
    /// record, of a log, ends it, or goes on with it.
    fn goes_on(&self, commit: u64) -> Option<bool> {
        let first = match self {
            Found::Whole(footer) => &footer.contents,
            Found::Torn(records) => records.first()?,
            Found::Nothing => return None,
        };
        match (first.ends(commit), first.continues(commit)) {
            (true, _) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        }
    }

    /// The cell's epoch of the first commit it holds, if any.
    fn first_commit(&self) -> Option<u64> {
        let first = match self {
            Found::Whole(footer) => footer.contents.commits.first(),
            Found::Torn(records) => records.iter().find_map(|record| record.commits.first()),
            Found::Nothing => None,
        };
        first.copied().map(epoch_of)
    }
}

/// What a segment's log and raw file hold ([`log_records`]).
struct Logged {
    /// The whole records of its log, the commits it holds, up to the first whose runs its raw data
    /// doesn't hold ([`LogRecord`]).
    records: Vec<Contents>,
    /// Its raw data, as stored, with the runs the records hold written into it.
    data: Vec<u8>,
    /// cladefs: whether there is such a record, whose commit is then not durable, nor those after
    /// it: a flush makes a commit durable only once its record and its runs' data both are
    /// (TASK-334).
    refused: bool,
    /// cladefs: what its raw data is encrypted under and its log authenticated with, read holding
    /// the cell's keys; none read holding no key (TASK-411.5).
    encryption: Option<Arc<RawEncryption>>,
}

/// What the log of `segment` and its raw file hold ([`Logged`]); none if it has no log. Read
/// holding no key, as recovery reads, its records' MACs unchecked if it is encrypted, its data as
/// stored; cladefs: or holding `keys`, the cell's, which give its encryption, and which its header
/// and records must be authentic under (TASK-411.5).
fn log_records<D: Dir>(
    dir: &D,
    segment: SegmentId,
    keys: Option<&bucket_cell::keys::Ring>,
) -> Result<Logged, Error> {
    let bytes = match dir.open(&log_path(segment), false) {
        Ok(log) => read_file(&log, crate::MAX_LOG_BYTES)?,
        Err(err) if not_found(&err) => {
            let (records, data) = (Vec::new(), Vec::new());
            return Ok(Logged { records, data, refused: false, encryption: None });
        }
        Err(err) => return Err(err.into()),
    };
    let header = LogHeader::of(&bytes, segment);
    let encryption = match (keys, &header) {
        (Some(keys), Some(header)) => Some(header.encryption(segment, keys)?),
        _ => None,
    };
    let records = LogRecord::decode(&bytes, segment, encryption.as_deref())?;
    let raw = dir.open(&open_segment_path(segment), true)?;
    let mut data = read_file(&raw, crate::MAX_RAW_SEGMENT_BYTES)?;
    let whole = records.iter().take_while(|record| record.restore(&mut data)).count();
    let refused = whole < records.len();
    let records = records.into_iter().take(whole).map(|record| record.contents).collect();
    Ok(Logged { records, data, refused, encryption })
}

/// The footer of a segment closed again, left open, or framed part way, from the records of its
/// log that are whole, which hold a commit at least: their contents folded, stamped provisionally
/// as `cell`'s, after `last`, the footer before it, if any. cladefs: framed with it once the cell's
/// keys are known ([`Recovered::close`], TASK-411.9).
fn closed_again(
    cell: CellId,
    last: Option<&Footer>,
    records: &[Contents],
) -> Result<Footer, Error> {
    let first = records.first().and_then(last_commit);
    let first = first.context("a log closed again holds a commit")?;
    let mut chain = match last {
        Some(last) => {
            let next_segment =
                last.segment.checked_add(1).context("recovered WAL segment number overflow")?;
            Chain::new(cell, last.epoch, next_segment, last.contents.next_sequence)
        }
        None => Chain::new(cell, writer_of(first), 0, 0),
    };
    Ok(chain.close(fold(cell.id, records)))
}

/// cladefs: the last commit `record`, a log's record, holds of: the one it goes on with past its
/// segment, if any, or its last (TASK-92.41).
fn last_commit(record: &Contents) -> Option<u64> {
    let continued = record.continued.as_ref().map(|continued| continued.commit);
    continued.or(record.commits.last().copied())
}

/// Removes the file at `path` in `dir`, unless it is gone.
fn remove<D: Dir>(dir: &D, path: &str) -> Result<(), Error> {
    match dir.remove(path) {
        Err(err) if !not_found(&err) => Err(Error::new(err).context(format!("removing {path}"))),
        _ => Ok(()),
    }
}

/// A segment of the WAL, closed, and not yet acknowledged: its number, its footer, stamped by its
/// writer's chain, and, if its writer's chain stamped it anew after a takeover, which its framed
/// file says not, so ([`Queue`]); the bytes of its data and of the extents stored on their own it
/// names; and whether the uploader took it.
pub(crate) struct Queued {
    pub number: u64,
    pub footer: Footer,
    pub restamped: bool,
    pub bytes: u64,
    taken: bool,
}

/// The WAL's segments not yet acknowledged, in order, which the uploader takes once their
/// commits are durable and they are framed, and acknowledges in order; and its start, the cell's
/// commit before the first, which acknowledging moves on.
pub(crate) struct Queue {
    segments: VecDeque<Queued>,
    start: Option<u64>,
    /// cladefs: those acknowledged that go on with a commit the next end, held until the one ending
    /// it is acknowledged too ([`Queue::acknowledge`], TASK-338).
    held: Vec<Queued>,
}

impl Queue {
    /// A queue of `segments`, after `start`.
    pub fn new(start: Option<u64>, segments: impl IntoIterator<Item = Queued>) -> Self {
        Self { segments: segments.into_iter().collect(), start, held: Vec::new() }
    }

    /// Segment `number`, closed, whose footer is `footer`, holding `bytes` of data with the
    /// extents stored on their own its footer names, as its writer's chain stamped it as it closed.
    pub fn push(&mut self, number: u64, footer: Footer, bytes: u64) {
        self.segments.push_back(Queued::new(number, footer, false, bytes));
    }

    /// The WAL's start: the cell's commit before its first segment.
    #[cfg(test)]
    pub fn start(&self) -> Option<u64> {
        self.start
    }

    /// The bytes of the segments not yet acknowledged: what the uploader's lag counts.
    pub fn bytes(&self) -> u64 {
        self.segments.iter().map(|queued| queued.bytes).sum()
    }

    /// Takes the segments not yet taken, in order, `limit` of them at most: up to the first of
    /// commits past `durable`, the last commit durable, or `framing` says is being framed, whose
    /// framed file is what is uploaded.
    pub fn take(
        &mut self,
        durable: u64,
        framing: impl Fn(u64) -> bool,
        limit: usize,
    ) -> Vec<&Queued> {
        let mut taken = Vec::new();
        for queued in self.segments.iter_mut().filter(|queued| !queued.taken) {
            let last = footer_last(&queued.footer);
            if taken.len() == limit || last > durable || framing(queued.number) {
                break;
            }
            queued.taken = true;
            taken.push(&*queued);
        }
        taken
    }

    /// Numbers of untaken durable segments (up to `limit`) whose framed files are still being
    /// written (`writing`), in order.
    pub fn pending_writing(
        &self,
        durable: u64,
        limit: usize,
        writing: impl Fn(u64) -> bool,
    ) -> Vec<u64> {
        let mut pending = Vec::new();
        let mut seen = 0;
        for queued in self.segments.iter().filter(|queued| !queued.taken) {
            if seen == limit || footer_last(&queued.footer) > durable {
                break;
            }
            seen += 1;
            if writing(queued.number) {
                pending.push(queued.number);
            }
        }
        pending
    }

    /// If the queue begins with `numbers` in exact order, returns the WAL start commit after
    /// acknowledging all of them (the last commit in `numbers`, falling back to `self.start`).
    pub fn start_after(&self, numbers: &[u64]) -> Option<Option<u64>> {
        if self.segments.len() < numbers.len() {
            return None;
        }
        let mut after = self.start;
        for (queued, &number) in self.segments.iter().zip(numbers) {
            if queued.number != number {
                return None;
            }
            after = queued.footer.contents.commits.last().copied().or(after);
        }
        Some(after)
    }

    /// cladefs: the segments, from the first, whose commits are `durable` or before, ending where
    /// no commit goes on past them, by their numbers, with their footers: what an ephemeral cell's
    /// checkpoint covers (doc-25, TASK-698.2).
    pub fn covering(&self, durable: u64) -> Vec<(u64, Footer)> {
        let durable =
            self.segments.iter().take_while(|queued| footer_last(&queued.footer) <= durable);
        let mut covering: Vec<_> =
            durable.map(|queued| (queued.number, queued.footer.clone())).collect();
        while covering.last().is_some_and(|(_, footer)| footer.contents.continued.is_some()) {
            covering.pop();
        }
        covering
    }

    /// Segment `number`, the first, is acknowledged: it leaves the queue, which goes on from its
    /// last commit. Returns the segments whose acknowledgements are whole now, which their cell
    /// may let go of: it, and those held before it; none if it goes on with a commit, which it is
    /// held for, until the segment ending that commit is acknowledged (TASK-338). Let go of before,
    /// its files leaving for the host's cache, the start not yet past the commit, a crash left the
    /// segments before and after it, the commit's runs in it missing, and an opening closed the
    /// commit's first segment again without it, losing it and those after, durable. None if it is
    /// not the first.
    pub fn acknowledge(&mut self, number: u64) -> Option<Vec<Queued>> {
        if self.segments.front().is_none_or(|queued| queued.number != number) {
            return None;
        }
        let queued = self.segments.pop_front().unwrap();
        self.start = queued.footer.contents.commits.last().copied().or(self.start);
        let continues = queued.footer.contents.continued.is_some();
        self.held.push(queued);
        Some(if continues { Vec::new() } else { std::mem::take(&mut self.held) })
    }
}

impl Queued {
    /// cladefs: the bytes of its data: what it holds but its footer's extents stored on their own
    /// (doc-27: what its density is measured against once acknowledged).
    pub fn data_len(&self) -> u64 {
        let own: u64 = self.footer.contents.own.iter().map(|(_, len)| len).sum();
        self.bytes - own
    }

    pub fn new(number: u64, footer: Footer, restamped: bool, bytes: u64) -> Self {
        Self { number, footer, restamped, bytes, taken: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemDir;
    use crate::layout::create_cell_dirs;
    use extent_frame::segment::{Continued, tag};
    use storage_device::ExtentId;

    const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);

    fn segment(number: u64) -> SegmentId {
        SegmentId { cell: CELL.id, number }
    }

    /// A commit of epoch `epoch`, sealing `extent` at `offset`, of a block.
    fn commit(epoch: u64, extent: u64, offset: u64) -> Contents {
        Contents {
            commits: vec![tag(2, epoch)],
            extents: vec![(ExtentId::new(CELL.id, extent), offset, 4096)],
            next_sequence: extent + 1,
            ..Contents::default()
        }
    }

    /// Board 5436: a log's header is read from untrusted bytes: of a log shorter than a header,
    /// whatever its first bytes (its magic among them), none, at every length; and of a whole one,
    /// the header it was written with. Of 8 to 31 bytes starting with the magic, the read panicked.
    #[test]
    fn headers_of_logs_cut_short_are_none() {
        let header = log_header(0x5a17, segment(3), &encryption());
        for len in 0..LOG_HEADER {
            assert!(LogHeader::of(&header[..len], segment(3)).is_none(), "{len} bytes");
        }
        let read = LogHeader::of(&header, segment(3)).expect("a whole header");
        assert_eq!(read.salt, 0x5a17);
        assert!(LogHeader::of(&header, segment(4)).is_none(), "another segment's");
    }

    /// cladefs: an encryption of a segment's raw file and log, under the tests' DEK, as the cell's
    /// of [`CELL`] encrypts them (`bucket_cell::testing::encrypting`, TASK-411.28).
    fn encryption() -> Arc<RawEncryption> {
        let (dek, key) = bucket_cell::testing::encrypting(CELL);
        Arc::new(RawEncryption::new(dek, key))
    }

    /// cladefs: the encryption segment `number`'s log's header names, read under the tests' keys.
    fn sealer_of(dir: &MemDir, number: u64) -> Arc<RawEncryption> {
        let log =
            read_file(&dir.open(&log_path(segment(number)), false).unwrap(), crate::MAX_LOG_BYTES)
                .unwrap();
        let header = LogHeader::of(&log, segment(number)).unwrap();
        header.encryption(segment(number), &bucket_cell::testing::ring([CELL])).unwrap()
    }

    /// Writes segment `number`'s raw data, `blocks` blocks, and its log, of `records`, as an open
    /// segment's flushes leave them. cladefs: encrypted (TASK-411.5); and durable, the raw data and
    /// the files' entries, as the cell makes them before its records' runs say so
    /// (`Segments::opened`, `Runs::Synced`): not, a crash tore the raw data inside a run, or lost a
    /// segment whose next kept its commits, which no cell's crash leaves (board 8264).
    fn left_open(dir: &MemDir, number: u64, blocks: u64, records: &[Contents]) {
        let encryption = encryption();
        let raw = dir.create(&open_segment_path(segment(number)), true).unwrap();
        let raw = crate::raw::RawFile::new(raw, encryption.clone());
        raw.write_at(&vec![number as u8; (blocks * 4096) as usize], 0).unwrap();
        raw.sync().unwrap();
        logged_under(dir, number, number, records, encryption);
        for kind in [OPEN_SEGMENTS, LOGS] {
            dir.sync_dir(&cell_dir(kind, CELL.id)).unwrap();
        }
    }

    /// Writes segment `number`'s log as segment `of`'s log, of `records`, its header and records
    /// `of`'s: a spare reused for `of` that a crash left under `number`'s name.
    fn logged(dir: &MemDir, number: u64, of: u64, records: &[Contents]) {
        logged_under(dir, number, of, records, encryption());
    }

    /// [`logged`], its header and records MACed by `encryption`.
    fn logged_under(
        dir: &MemDir,
        number: u64,
        of: u64,
        records: &[Contents],
        encryption: Arc<RawEncryption>,
    ) {
        let log = Arc::new(dir.create(&log_path(segment(number)), false).unwrap());
        let salt = new_salt(0, segment(of));
        log.write_at(&log_header(salt, segment(of), &encryption), 0).unwrap();
        let mut log = SegmentLog::new(log, salt, encryption);
        let records: Vec<LogRecord> = (records.iter())
            .map(|contents| LogRecord { contents: contents.clone(), runs: Runs::Synced })
            .collect();
        log.append(&records).unwrap();
    }

    /// cladefs: the WAL in `dir` recovered, and the segments it closed again framed, encrypted, as
    /// a cell opens it holding its keys (TASK-411.9).
    fn recover_closing(dir: &MemDir) -> Result<Recovered, Error> {
        let mut recovered = recover(dir, CELL, None)?;
        recovered.close(dir, CELL, &bucket_cell::testing::ring([CELL]), false)?;
        Ok(recovered)
    }

    fn files(dir: &MemDir, kind: &'static str) -> Vec<u64> {
        list_numbers(dir, &cell_dir(kind, CELL.id)).unwrap()
    }

    /// The start is the last written, whole: a writing torn leaves the one before; none before the
    /// first.
    #[test]
    fn starts_are_the_last_written_whole() {
        let dir = MemDir::new();
        let (mut file, start) = StartFile::read(&dir).unwrap();
        assert_eq!(start, None);
        for written in [Some(3), None, Some(9)] {
            file.write(&dir, written).unwrap();
            assert_eq!(StartFile::read(&dir).unwrap().1, written);
        }
        // The next writing, to the slot of `None`'s, torn.
        let raw = dir.open(START, false).unwrap();
        raw.write_at(&[0xff; 8], 0).unwrap();
        assert_eq!(StartFile::read(&dir).unwrap().1, Some(9));
    }

    /// Board 6908: a segment's framed file whose footer passes its check but doesn't decode, of
    /// another build's or a bug's, fails the recovery, naming the file: taken for none, the
    /// segment's commits, its raw data and log gone, would be taken for lost.
    #[test]
    fn footers_that_dont_decode_fail_recoveries() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        left_open(&dir, 0, 1, &[commit(1, 1, 0)]);
        recover_closing(&dir).unwrap();
        let path = segment_path(segment(0));
        let framed = read_whole(&dir.open(&path, true).unwrap()).unwrap();
        let keys = bucket_cell::testing::ring([CELL]);
        let bad_body =
            [&bucket_cell::layout::FORMAT.current.to_le_bytes()[..], b"no footer"].concat();
        let restamped = extent_frame::restamp(&framed, &bad_body, &keys.of(CELL.id)).unwrap();
        dir.remove(&path).unwrap();
        dir.create(&path, true).unwrap().write_at(&restamped, 0).unwrap();
        let failed = recover(&dir, CELL, None).map(drop).map_err(|err| format!("{err:#}"));
        let footer = "a segment's footer: a footer: io error: unexpected end of file";
        assert_eq!(failed, Err(format!("{path}: {footer}")));
        let newer = (bucket_cell::layout::FORMAT.current + 1).to_le_bytes();
        let restamped = extent_frame::restamp(&framed, &newer, &keys.of(CELL.id)).unwrap();
        dir.remove(&path).unwrap();
        dir.create(&path, true).unwrap().write_at(&restamped, 0).unwrap();
        let failed = recover(&dir, CELL, None).map(drop).map_err(|err| format!("{err:#}"));
        let refused = formats::Refused {
            format: bucket_cell::layout::FORMAT,
            found: bucket_cell::layout::FORMAT.current + 1,
        };
        assert_eq!(failed, Err(format!("{path}: a segment's footer: {refused}")));
    }

    /// TASK-411.9: a recovery frames nothing: a segment it closes again keeps its raw data and log,
    /// no framed file, until the cell's keys frame it, encrypted under its cell's DEK, never in
    /// the clear, its data as it was. TASK-411.3: an encrypted segment's framed file, whole, its
    /// raw data and log still there, is whole to a recovery holding no key, as one's in the clear
    /// is: checked by its CRCs, never closed again from its raw data.
    #[test]
    fn segments_closed_again_are_framed_under_the_keys_alone() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        left_open(&dir, 0, 1, &[commit(1, 1, 0)]);
        let mut recovered = recover(&dir, CELL, None).unwrap();
        let footer = recovered.segments[0].0.clone();
        assert_eq!(recovered.closing.keys().copied().collect::<Vec<_>>(), [0]);
        assert_eq!(files(&dir, SEGMENTS), Vec::<u64>::new());
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![0], vec![0]));
        // Recovered again before it is framed, it holds the same.
        assert_eq!(recover(&dir, CELL, None).unwrap().segments, recovered.segments);
        let keys = bucket_cell::testing::ring([CELL]);
        recovered.close(&dir, CELL, &keys, false).unwrap();
        assert!(recovered.closing.is_empty());
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
        let path = segment_path(segment(0));
        let read = || read_whole(&dir.open(&path, true).unwrap()).unwrap();
        let encrypted = read();
        let key = extent_frame::KeyId { cell: CELL.id, generation: 1 };
        assert!(matches!(
            extent_frame::Frame::of(&encrypted).unwrap().encryption,
            extent_frame::Encryption::Encrypted { key: under, .. } if under == key
        ));
        assert_eq!(extent_frame::data(&encrypted, &keys.of(CELL.id)).unwrap(), vec![0; 4096]);
        assert_eq!(
            extent_frame::data(&encrypted, &extent_frame::NoKeys),
            Err(extent_frame::Error::NoKey(key))
        );
        // Framed whole, encrypted, its raw data and log still there.
        left_open(&dir, 0, 1, &[commit(1, 1, 0)]);
        let recovered = recover(&dir, CELL, None).unwrap();
        assert_eq!(recovered.segments, [(footer, 0)]);
        assert!(recovered.closing.is_empty());
        assert_eq!(read(), encrypted);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// A segment the start acknowledged, its framed file gone, as acknowledging it removed it, but
    /// its raw data and log a crash brought back, holds nothing of the log's: the whole segment
    /// after it is kept, not taken to follow one closed again, and dropped with the durable
    /// commits it holds (found by the kept crash rounds, TASK-165.9).
    #[test]
    fn segments_acknowledged_brought_back_are_no_logs() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        left_open(&dir, 0, 1, &[commit(1, 1, 0)]);
        recover_closing(&dir).unwrap();
        left_open(&dir, 1, 1, &[commit(2, 2, 0)]);
        let framed = recover_closing(&dir).unwrap().segments;
        assert_eq!(framed.iter().map(|(_, number)| *number).collect::<Vec<_>>(), [0, 1]);
        // Segment 0 acknowledged: the start names its commit, and its framed file went; a crash
        // brought back its raw data and log, whose removals were not durable.
        StartFile::read(&dir).unwrap().0.write(&dir, Some(tag(2, 1))).unwrap();
        dir.remove(&segment_path(segment(0))).unwrap();
        left_open(&dir, 0, 1, &[commit(1, 1, 0)]);
        let recovered = recover_closing(&dir).unwrap();
        assert_eq!(recovered.segments, framed[1..]);
        assert_eq!(files(&dir, SEGMENTS), [1]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// A segment whose framed file a crash tore, its raw data and log there, the log holding no
    /// durable commit, goes, its framed file's removal durable before theirs: a crash keeping the
    /// framed file without them would have the next recovery take it as framed whole, unchecked,
    /// its parts failing their CRCs as they are read (TASK-318).
    #[test]
    fn segments_holding_nothing_go_framed_files_first() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        left_open(&dir, 0, 1, &[commit(1, 1, 0)]);
        left_open(&dir, 1, 1, &[commit(2, 2, 0)]);
        let framed = recover_closing(&dir).unwrap().segments;
        assert_eq!(framed.iter().map(|(_, number)| *number).collect::<Vec<_>>(), [0, 1]);
        // Segment 1's framed file torn, its trailer, table and footer whole but not its data; its
        // raw data and log back, the log holding no record durable.
        // Its first byte flipped, whatever it is: a write of a fixed byte left it whole when that
        // was the byte there, one segment in 256 (TASK-689).
        let torn = dir.open(&segment_path(segment(1)), true).unwrap();
        let mut first = [0];
        torn.read_at(&mut first, 0).unwrap();
        torn.write_at(&[!first[0]], 0).unwrap();
        torn.sync().unwrap();
        left_open(&dir, 1, 1, &[]);
        for kind in [SEGMENTS, OPEN_SEGMENTS, LOGS] {
            dir.sync_dir(&cell_dir(kind, CELL.id)).unwrap();
        }
        assert_eq!(recover_closing(&dir).unwrap().segments, framed[..1]);
        // The opening makes its next open segment, whose raw data and log it makes durable.
        for kind in [OPEN_SEGMENTS, LOGS] {
            dir.sync_dir(&cell_dir(kind, CELL.id)).unwrap();
        }
        for seed in 0..64 {
            let recovered = recover_closing(&dir.crash(seed)).unwrap();
            assert_eq!(recovered.segments, framed[..1], "seed {seed}");
        }
    }

    /// cladefs: the record of commit `epoch` going on past its segment, sealing `extent` there at
    /// `offset`, of a block: a commit spanning segments (TASK-92.41).
    fn continuing(epoch: u64, extent: u64, offset: u64) -> Contents {
        let extents = vec![(ExtentId::new(CELL.id, extent), offset, 4096)];
        let continued = Some(extent_frame::segment::Continued { commit: tag(2, epoch), extents });
        Contents { continued, ..Contents::default() }
    }

    /// cladefs: a commit spanning segments whose records, each segment's part of it, are whole, the
    /// last ending it, is kept, each segment closed again with its part, going on with it, but the
    /// last; one whose last segment's log lacks its record, ending it, is not: the first closes
    /// again with the commits before it alone, and those after go (TASK-92.41).
    #[test]
    fn commits_spanning_segments_are_kept_whole_or_not_at_all() {
        let ended = |dir: &MemDir| {
            create_cell_dirs(dir, CELL.id).unwrap();
            left_open(dir, 0, 2, &[commit(1, 1, 0), continuing(2, 2, 4096)]);
            left_open(dir, 1, 1, &[continuing(2, 3, 0)]);
        };
        let dir = MemDir::new();
        ended(&dir);
        left_open(&dir, 2, 1, &[commit(2, 4, 0), commit(3, 5, 0)]);
        let recovered = recover_closing(&dir).unwrap().segments;
        let place = |(footer, number): &(Footer, u64)| {
            let continued = footer.contents.continued.as_ref().map(|continued| continued.commit);
            (*number, footer.contents.commits.clone(), continued)
        };
        let places: Vec<_> = recovered.iter().map(place).collect();
        let expected = [
            (0, vec![tag(2, 1)], Some(tag(2, 2))),
            (1, vec![], Some(tag(2, 2))),
            (2, vec![tag(2, 3)], None),
        ];
        assert_eq!(places, expected);
        assert_eq!(recover_closing(&dir).unwrap().segments, recovered, "recovered again");

        let dir = MemDir::new();
        ended(&dir);
        left_open(&dir, 2, 1, &[]);
        let recovered = recover_closing(&dir).unwrap().segments;
        assert_eq!(recovered.iter().map(place).collect::<Vec<_>>(), [(0, vec![tag(2, 1)], None)]);
        assert_eq!(files(&dir, SEGMENTS), [0]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
        // Framed whole, going on with the commit, its raw data and log there: the same.
        let dir = MemDir::new();
        ended(&dir);
        left_open(&dir, 2, 1, &[commit(2, 4, 0)]);
        let framed = recover_closing(&dir).unwrap().segments;
        assert_eq!(framed.len(), 3);
        left_open(&dir, 0, 2, &[commit(1, 1, 0), continuing(2, 2, 4096)]);
        left_open(&dir, 1, 1, &[continuing(2, 3, 0)]);
        dir.remove(&segment_path(segment(2))).unwrap();
        left_open(&dir, 2, 1, &[]);
        let recovered = recover_closing(&dir).unwrap().segments;
        assert_eq!(recovered.iter().map(place).collect::<Vec<_>>(), [(0, vec![tag(2, 1)], None)]);
    }

    /// cladefs: a segment going on with a commit the start acknowledged, its raw data gone, its
    /// framed file's removal, as it left for the host's cache, and its log's undone by a crash, the
    /// segment ending the commit gone: it goes, its commit the bucket's, whole, not taken for one
    /// going on with a commit no segment ends, whose raw data it would close it again from (found
    /// by the cut openings of the kept crash rounds, TASK-92.41).
    #[test]
    fn segments_going_on_with_acknowledged_commits_go() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        left_open(&dir, 0, 2, &[commit(1, 1, 0), continuing(2, 2, 4096)]);
        left_open(&dir, 1, 1, &[commit(2, 3, 0)]);
        assert_eq!(recover_closing(&dir).unwrap().segments.len(), 2);
        StartFile::read(&dir).unwrap().0.write(&dir, Some(tag(2, 2))).unwrap();
        dir.remove(&segment_path(segment(1))).unwrap();
        // Its log's removal undone too, not its raw data's.
        left_open(&dir, 0, 2, &[commit(1, 1, 0), continuing(2, 2, 4096)]);
        dir.remove(&open_segment_path(segment(0))).unwrap();
        let recovered = recover_closing(&dir).unwrap();
        assert_eq!(recovered.segments, []);
        assert_eq!(files(&dir, SEGMENTS), Vec::<u64>::new());
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// Segments 0 to 2 framed whole, from logs of commits 1 and 2, 3, and 4: a flush appends its
    /// commits' records to the logs of the segments they were made in, being framed or open, so
    /// that each segment left torn closes again (TASK-165.9).
    fn framed_three(dir: &MemDir) -> Vec<(Footer, u64)> {
        create_cell_dirs(dir, CELL.id).unwrap();
        left_open(dir, 0, 1, &[commit(1, 1, 0), commit(2, 2, 0)]);
        left_open(dir, 1, 1, &[commit(3, 3, 0)]);
        left_open(dir, 2, 1, &[commit(4, 4, 0)]);
        let recovered = recover_closing(dir).unwrap().segments;
        let last: Vec<u64> = recovered.iter().map(|(footer, _)| footer_last(footer)).collect();
        assert_eq!(last, [2, 3, 4]);
        recovered
    }

    /// A segment whose framed file isn't whole, its log holding each of its commits, closes again,
    /// and the segments after it, holding the commits after its last, are kept: though a crash
    /// brought their raw data back without their logs, their removals not durable, which doesn't
    /// say whether their commits were durable (found by Shuttle, TASK-165.9).
    #[test]
    fn segments_after_one_closed_again_are_kept_if_they_follow_it() {
        let dir = MemDir::new();
        let framed = framed_three(&dir);
        dir.remove(&segment_path(segment(0))).unwrap();
        left_open(&dir, 0, 1, &[commit(1, 1, 0), commit(2, 2, 0)]);
        for number in [1, 2] {
            let raw = dir.create(&open_segment_path(segment(number)), true).unwrap();
            raw.write_at(&[number as u8; 4096], 0).unwrap();
        }
        let recovered = recover_closing(&dir).unwrap();
        let numbers: Vec<u64> = recovered.segments.iter().map(|(_, number)| *number).collect();
        assert_eq!(numbers, [0, 1, 2]);
        assert_eq!(recovered.segments[1..], framed[1..]);
        assert_eq!(footer_last(&recovered.segments[0].0), 2);
        assert_eq!(files(&dir, SEGMENTS), [0, 1, 2]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// cladefs: after a segment that holds nothing durable, its framed file lost, a segment framed
    /// whole whose raw data and log are still there is closed again from them, not read from its
    /// framed file: segments are made durable several at once, and a crash may keep a later one's
    /// framed file and lose an earlier one's, its commits not durable, its log lacking them, the
    /// later footer then a record of commits after those it lost (TASK-310's Shuttle crash
    /// rounds, round 137). Its log lacking its commits too, it goes.
    #[test]
    fn segments_framed_after_one_lost_close_again_from_their_logs() {
        let dir = MemDir::new();
        framed_three(&dir);
        dir.remove(&segment_path(segment(1))).unwrap();
        left_open(&dir, 1, 1, &[]);
        left_open(&dir, 2, 1, &[]);
        let recovered = recover_closing(&dir).unwrap();
        let numbers: Vec<u64> = recovered.segments.iter().map(|(_, number)| *number).collect();
        assert_eq!(numbers, [0]);
        assert_eq!(footer_last(&recovered.segments[0].0), 2);
        assert_eq!(files(&dir, SEGMENTS), [0]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// cladefs: a segment left open whose log's first commit isn't the next after the last kept
    /// goes, and those after it: a flush appends its commits' records to their segments' logs at
    /// once, and a crash may keep a later commit's record and lose an earlier one's in the log
    /// before, whose segment then holds nothing durable, and goes (TASK-310's Shuttle crash rounds,
    /// round 20).
    #[test]
    fn logs_following_a_commit_lost_go() {
        let dir = MemDir::new();
        framed_three(&dir);
        left_open(&dir, 3, 1, &[]);
        left_open(&dir, 4, 1, &[commit(6, 6, 0)]);
        let recovered = recover_closing(&dir).unwrap();
        let last: Vec<u64> =
            recovered.segments.iter().map(|(footer, _)| footer_last(footer)).collect();
        assert_eq!(last, [2, 3, 4]);
        assert_eq!(files(&dir, SEGMENTS), [0, 1, 2]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// cladefs: a commit spanning two segments left open, its record in the second's log whole and
    /// its runs there durable, but its record in the first's checking runs the first's raw data
    /// doesn't hold, isn't durable: both go, the commit with them, though the second's log alone
    /// reads it whole (TASK-334, found by TASK-310's bucket crash rounds: a flush syncs a commit's
    /// runs' data at once with its records, and a crash before both are durable kept the records
    /// and the second's data, not the first's).
    /// The segments [`framed_three`] leaves, then segment 3, filled since commit 4, going on with
    /// commit 5 in its run of extent 5, its record checking a run its raw data, a crash's, doesn't
    /// hold; and segment 4, the open one, ending it in its run of extent 6.
    fn refusing_two(dir: &MemDir) {
        framed_three(dir);
        left_open(dir, 3, 1, &[]);
        let continued = Contents {
            continued: Some(Continued {
                commit: tag(2, 5),
                extents: vec![(ExtentId::new(CELL.id, 5), 0, 4096)],
            }),
            ..Contents::default()
        };
        let crc = extent_frame::crc(&[0xee; 4096]);
        let record = LogRecord { contents: continued, runs: Runs::Checked(vec![crc]) };
        let log = Arc::new(dir.open(&log_path(segment(3)), false).unwrap());
        let salt = new_salt(0, segment(3));
        SegmentLog::new(log, salt, sealer_of(dir, 3)).append([&record]).unwrap();
        left_open(dir, 4, 1, &[commit(5, 6, 0)]);
    }

    /// cladefs: so too after a recovery stopped after each of its operations, as by a crash, which
    /// may keep any of what it removed: segment 4 goes, durably, before segment 3, whose log says
    /// why both go; removed first, a crash kept segment 3's raw file without its log, which then
    /// read as gone durable, and segment 4's commit was kept, past the lost one (TASK-334, found by
    /// TASK-310's opening rounds, round 28).
    #[test]
    fn commits_whose_runs_a_log_refuses_go_whatever_a_recovery_stopped_left() {
        for operations in 0.. {
            let dir = MemDir::serial();
            refusing_two(&dir);
            dir.fail_after(Some(operations));
            let stopped = recover_closing(&dir).is_err();
            dir.fail_after(None);
            for seed in 0..8 {
                let recovered = recover_closing(&dir.crash(seed)).unwrap();
                let last: Vec<u64> =
                    recovered.segments.iter().map(|(footer, _)| footer_last(footer)).collect();
                assert_eq!(last, [2, 3, 4], "stopped after {operations} operations, seed {seed}");
            }
            if !stopped {
                break;
            }
        }
    }

    /// cladefs: a segment the start acknowledged, its raw file brought back by a crash without its
    /// log or framed file, breaks no run of whole framed files: the segments after it are read from
    /// theirs, though their raw files and logs are still there, their commits durable (TASK-310).
    #[test]
    fn segments_acknowledged_brought_back_break_no_whole_run() {
        let dir = MemDir::new();
        let framed = framed_three(&dir);
        let (mut start, _) = StartFile::read(&dir).unwrap();
        start.write(&dir, Some(tag(2, 2))).unwrap();
        dir.remove(&segment_path(segment(0))).unwrap();
        dir.create(&open_segment_path(segment(0)), true).unwrap();
        // Segment 2's commit after its framing, its raw file and log not gone yet.
        left_open(&dir, 2, 1, &[]);
        let recovered = recover_closing(&dir).unwrap();
        assert_eq!(recovered.segments, framed[1..]);
    }

    /// cladefs: a log whose header is another segment's holds none of its segment's commits: a
    /// spare, renamed from segment 3's log, then renamed segment 6's, neither rename durable, its
    /// header and records then 6's, which a crash kept under 3's name. Read as 3's, its commits
    /// were kept with segment 3's raw data, which holds none of their runs (TASK-341, TASK-310's
    /// crash rounds, round 81).
    #[test]
    fn logs_of_other_segments_hold_none_of_their_names() {
        let dir = MemDir::new();
        framed_three(&dir);
        let raw = dir.create(&open_segment_path(segment(3)), true).unwrap();
        raw.write_at(&[3; 4096], 0).unwrap();
        logged(&dir, 3, 6, &[commit(5, 5, 0)]);
        let recovered = recover_closing(&dir).unwrap();
        let last: Vec<u64> =
            recovered.segments.iter().map(|(footer, _)| footer_last(footer)).collect();
        assert_eq!(last, [2, 3, 4]);
        assert_eq!(files(&dir, SEGMENTS), [0, 1, 2]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// cladefs: a segment the start acknowledged, its framed file gone, its raw file and log back,
    /// a crash undoing their removal, its log a spare taken since as another segment's, breaks no
    /// run of segments framed whole: those after it are read framed, though their raw files and
    /// logs are back too, their logs spares taken since. Its log taken for its own, holding no
    /// record, it broke the run, and the segments after it were closed again from logs holding none
    /// of their records, commits the uploader had lost (TASK-369, the Shuttle rounds, round
    /// writer_closer_and_uploader_randomly's: "uploaded commit 3 before durable").
    #[test]
    fn logs_taken_since_break_no_run_of_whole_segments() {
        let dir = MemDir::new();
        framed_three(&dir);
        let (mut start, _) = StartFile::read(&dir).unwrap();
        start.write(&dir, Some(tag(2, 2))).unwrap();
        dir.remove(&segment_path(segment(0))).unwrap();
        for (number, of) in [(0, 9), (1, 8)] {
            let raw = dir.create(&open_segment_path(segment(number)), true).unwrap();
            raw.write_at(&[number as u8; 4096], 0).unwrap();
            logged(&dir, number, of, &[]);
        }
        let recovered = recover_closing(&dir).unwrap();
        let last: Vec<u64> =
            recovered.segments.iter().map(|(footer, _)| footer_last(footer)).collect();
        assert_eq!(last, [3, 4]);
        assert_eq!(files(&dir, SEGMENTS), [1, 2]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// cladefs: a segment left open whose log goes on from the start's commit is kept, though the
    /// segments the start acknowledged that a crash brought back end before it: the segment of the
    /// start's commit left for the host's cache, those before it its files' removal undone
    /// (TASK-334).
    #[test]
    fn logs_following_the_start_are_kept() {
        let dir = MemDir::new();
        framed_three(&dir);
        let (mut start, _) = StartFile::read(&dir).unwrap();
        start.write(&dir, Some(tag(2, 4))).unwrap();
        dir.remove(&segment_path(segment(2))).unwrap();
        left_open(&dir, 3, 1, &[commit(5, 5, 0)]);
        let recovered = recover_closing(&dir).unwrap();
        let last: Vec<u64> =
            recovered.segments.iter().map(|(footer, _)| footer_last(footer)).collect();
        assert_eq!(last, [5]);
        assert_eq!(files(&dir, SEGMENTS), [3]);
    }

    /// A segment whose framed file isn't whole, its log lacking its last commit, not durable,
    /// closes again with the commits it holds, and the segments after it go, though whole: they
    /// follow the commit it lacks (TASK-165.9).
    #[test]
    fn segments_after_a_log_lacking_its_last_commits_go() {
        let dir = MemDir::new();
        framed_three(&dir);
        dir.remove(&segment_path(segment(0))).unwrap();
        left_open(&dir, 0, 1, &[commit(1, 1, 0)]);
        let recovered = recover_closing(&dir).unwrap();
        let [(footer, 0)] = &recovered.segments[..] else {
            panic!("recovered {:?}", recovered.segments)
        };
        assert_eq!(footer.contents.commits, [tag(2, 1)]);
        assert_eq!(files(&dir, SEGMENTS), [0]);
        assert_eq!((files(&dir, OPEN_SEGMENTS), files(&dir, LOGS)), (vec![], vec![]));
    }

    /// A segment whose framed file isn't whole, its raw data and log gone, as a crash part way
    /// through removing it leaves, it holding nothing durable, goes, rather than refusing the
    /// cell: its raw data and log go otherwise only once it is framed whole (found by the crash
    /// rounds cutting openings, TASK-165.9).
    #[test]
    fn segments_torn_without_raw_data_or_log_go() {
        let dir = MemDir::new();
        let framed = framed_three(&dir);
        let torn = dir.create(&segment_path(segment(3)), true).unwrap();
        torn.write_at(&[9; 100], 0).unwrap();
        let recovered = recover_closing(&dir).unwrap();
        assert_eq!(recovered.segments, framed);
        assert_eq!(files(&dir, SEGMENTS), [0, 1, 2]);
    }

    /// The queue gives the uploader the segments that are framed, whose commits are durable, in
    /// order, each once, and is acknowledged in order, its start moving on.
    #[test]
    fn queues_give_what_may_upload_in_order() {
        let footer = |segment, epoch| Footer {
            cell: CELL,
            segment,
            epoch: 2,
            contents: Contents { commits: vec![tag(2, epoch)], ..Contents::default() },
        };
        let mut queue = Queue::new(Some(tag(1, 4)), []);
        for (number, epoch) in [(1, 5), (2, 6), (3, 7)] {
            queue.push(number, footer(number, epoch), 100);
        }
        assert_eq!(queue.bytes(), 300);
        let numbers = |taken: Vec<&Queued>| taken.iter().map(|q| q.number).collect::<Vec<_>>();
        assert_eq!(numbers(queue.take(7, |_| false, 1)), [1]);
        assert_eq!(numbers(queue.take(7, |number| number == 2, 9)), Vec::<u64>::new());
        assert_eq!(numbers(queue.take(6, |_| false, 9)), [2]);
        assert_eq!(numbers(queue.take(7, |_| false, 9)), [3]);
        assert_eq!(numbers(queue.take(7, |_| false, 9)), Vec::<u64>::new());
        assert!(queue.acknowledge(2).is_none());
        let segments =
            |group: Vec<Queued>| group.iter().map(|q| q.footer.segment).collect::<Vec<_>>();
        assert_eq!(queue.acknowledge(1).map(segments), Some(vec![1]));
        assert_eq!((queue.start(), queue.bytes()), (Some(tag(2, 5)), 200));
    }

    /// cladefs: acknowledged, the segments going on with a commit are held until the one ending it
    /// is acknowledged too, then given together, the start past the commit: let go one at a time, a
    /// crash between left a hole in the commit's segments here, which recovery read as the commit
    /// unended (TASK-338, TASK-310's crash rounds, round 65).
    #[test]
    fn queues_hold_acknowledged_segments_going_on_with_a_commit() {
        let footer = |segment, commits: Vec<u64>, continued: Option<u64>| Footer {
            cell: CELL,
            segment,
            epoch: 2,
            contents: Contents {
                commits: commits.into_iter().map(|epoch| tag(2, epoch)).collect(),
                continued: continued
                    .map(|epoch| Continued { commit: tag(2, epoch), extents: Vec::new() }),
                ..Contents::default()
            },
        };
        let mut queue = Queue::new(Some(tag(1, 4)), []);
        // 1 ends commit 5; 2 ends 6 and goes on with 7, which 3 goes on with, and 4 ends; 5 ends 8.
        queue.push(1, footer(1, vec![5], None), 100);
        queue.push(2, footer(2, vec![6], Some(7)), 100);
        queue.push(3, footer(3, vec![], Some(7)), 100);
        queue.push(4, footer(4, vec![7], None), 100);
        queue.push(5, footer(5, vec![8], None), 100);
        let numbers = |group: Vec<Queued>| group.iter().map(|q| q.number).collect::<Vec<_>>();
        assert_eq!(queue.acknowledge(1).map(numbers), Some(vec![1]));
        assert_eq!(queue.acknowledge(2).map(numbers), Some(vec![]));
        assert_eq!(queue.start(), Some(tag(2, 6)));
        assert_eq!(queue.acknowledge(3).map(numbers), Some(vec![]));
        assert_eq!(queue.acknowledge(4).map(numbers), Some(vec![2, 3, 4]));
        assert_eq!(queue.start(), Some(tag(2, 7)));
    }

    /// Board 6902 (Codex review): a log from the previous WAL format (`FXLOG\0\0\x03`) must be
    /// refused with a format error, never treated as empty and deleted along with its open segment.
    #[test]
    fn previous_wal_format_is_refused_without_deleting_files() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        let seg = segment(0);
        let raw = dir.create(&open_segment_path(seg), true).unwrap();
        raw.write_at(&[0xab; 4096], 0).unwrap();
        raw.sync().unwrap();
        let enc = encryption();
        let mut header = log_header(7, seg, &enc);
        // Simulate a genuine v3 header: magic FXLOG\0\0\x03, MAC over signed fields, no trailing hash.
        header[..8].copy_from_slice(b"FXLOG\0\0\x03");
        let mac = enc.mac(0, &header[..LOG_SIGNED]);
        header[LOG_SIGNED..LOG_SIGNED + MAC_LEN].copy_from_slice(&mac);
        header[LOG_HASHED..LOG_HEADER].fill(0);
        let record = framed(
            &LogRecord { contents: commit(1, 1, 0), runs: Runs::Synced },
            LOG_RECORDS,
            7,
            &enc,
        );
        let log = dir.create(&log_path(seg), false).unwrap();
        log.write_at(&header, 0).unwrap();
        log.write_at(&record, LOG_RECORDS).unwrap();
        log.sync().unwrap();
        let before = dir.files();
        let err = recover(&dir, CELL, None).err().expect("v3 WAL must be refused");
        assert!(err.to_string().contains("unsupported format"), "{err:#}");
        assert_eq!(dir.files(), before, "refused recovery must preserve every file byte-for-byte");
    }

    /// Doc-28: a start file's slot, a log's header, or a log's record, whole, of a version this
    /// build doesn't read, a newer build's, is refused by name, never taken for one torn: the
    /// start's other slot read in its place, the log read as holding nothing, or its records
    /// dropped from there.
    #[test]
    fn files_of_versions_not_read_are_refused() {
        let future = FORMAT.current + 1;
        let refusal = format!(
            "cell directory version {future}: this build reads versions {} to {}",
            FORMAT.oldest, FORMAT.current
        );
        let newer = |mut payload: Vec<u8>| {
            payload[..4].copy_from_slice(&future.to_le_bytes());
            payload
        };

        let dir = MemDir::new();
        let (mut start, _) = StartFile::read(&dir).unwrap();
        start.write(&dir, Some(5)).unwrap();
        let slot = newer(versioned(&Slot { serial: 2, start: Some(9) }));
        let file = dir.open(START, false).unwrap();
        file.write_at(&frame_salted(&slot, 0), 0).unwrap();
        let err = StartFile::read(&dir).err().expect("a future slot is refused");
        assert_eq!(format!("{err:#}"), format!("reading the WAL's start: {refusal}"));

        let (dir, seg, enc) = (MemDir::new(), segment(0), encryption());
        create_cell_dirs(&dir, CELL.id).unwrap();
        let raw = dir.create(&open_segment_path(seg), true).unwrap();
        raw.write_at(&[0xab; 4096], 0).unwrap();
        raw.sync().unwrap();
        let record = LogRecord { contents: commit(1, 1, 0), runs: Runs::Synced };
        let mut payload = newer(versioned(&record));
        let mac = enc.mac(LOG_RECORDS, &payload);
        payload.extend_from_slice(&mac);
        let log = dir.create(&log_path(seg), false).unwrap();
        log.write_at(&log_header(7, seg, &enc), 0).unwrap();
        log.write_at(&frame_salted(&payload, 7), LOG_RECORDS).unwrap();
        log.sync().unwrap();
        let before = dir.files();
        let err = recover(&dir, CELL, None).err().expect("a future record is refused");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "{seg:?}'s log: its record at {LOG_RECORDS} is not a record this build reads: \
                 {refusal}"
            )
        );
        assert_eq!(dir.files(), before, "refused recovery keeps every file as it was");

        let mut header = log_header(7, seg, &enc);
        header[4..8].copy_from_slice(&future.to_le_bytes());
        log.write_at(&header, 0).unwrap();
        log.sync().unwrap();
        let before = dir.files();
        let err = recover(&dir, CELL, None).err().expect("a future log header is refused");
        assert!(format!("{err:#}").contains("'s log has unsupported format"), "{err:#}");
        assert_eq!(dir.files(), before, "refused recovery keeps every file as it was");
    }

    /// Board 10062: a record hashed whole and authentic that is not a `LogRecord`, such as the
    /// contents alone that logs held before runs were (TASK-301), is refused, never read as
    /// another form of record nor taken for the log's torn end, which would drop it and every
    /// record after it.
    #[test]
    fn records_of_another_form_are_refused() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        let seg = segment(0);
        let raw = dir.create(&open_segment_path(seg), true).unwrap();
        raw.write_at(&[0xab; 4096], 0).unwrap();
        raw.sync().unwrap();
        let enc = encryption();
        let mut payload = versioned(&commit(1, 1, 0));
        let mac = enc.mac(LOG_RECORDS, &payload);
        payload.extend_from_slice(&mac);
        let log = dir.create(&log_path(seg), false).unwrap();
        log.write_at(&log_header(7, seg, &enc), 0).unwrap();
        log.write_at(&crate::layout::frame_salted(&payload, 7), LOG_RECORDS).unwrap();
        log.sync().unwrap();
        let before = dir.files();
        let err = recover(&dir, CELL, None).err().expect("a contents-only record is refused");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "{seg:?}'s log: its record at {LOG_RECORDS} is not a record this build reads: \
                 io error: unexpected end of file"
            )
        );
        assert_eq!(dir.files(), before, "refused recovery keeps every file as it was");
    }

    #[test]
    fn adv_wal_recover_footer_segment_u64_max_must_not_panic() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        let keys = bucket_cell::testing::ring([CELL]);
        let footer = Footer { cell: CELL, segment: u64::MAX, epoch: 1, contents: commit(1, 1, 0) };
        let encoded_footer = bucket_cell::layout::encode_segment_footer(&footer);
        let encrypting = keys.try_encrypting().unwrap();
        let framed_bytes =
            extent_frame::framed(&[0u8; 4096], &encoded_footer, (&encrypting.0, encrypting.1));
        dir.create(&segment_path(segment(0)), true).unwrap().write_at(&framed_bytes, 0).unwrap();
        left_open(&dir, 1, 1, &[commit(1, 2, 0)]);
        let err = recover(&dir, CELL, None)
            .err()
            .expect("footer.segment == u64::MAX followed by torn segment must return Err");
        assert!(err.to_string().contains("overflow"), "unexpected error: {err:#}");
    }

    #[test]
    fn adv_wal_recover_run_offset_plus_len_overflow_must_not_panic() {
        let dir = MemDir::new();
        create_cell_dirs(&dir, CELL.id).unwrap();
        let seg = segment(0);
        let raw = dir.create(&open_segment_path(seg), true).unwrap();
        raw.write_at(&[0xab; 4096], 0).unwrap();
        raw.sync().unwrap();
        let enc = encryption();
        let mut contents = commit(1, 1, 0);
        contents.extents = vec![(storage_device::ExtentId::new(CELL.id, 1), u64::MAX, 4096)];
        let record = LogRecord { contents, runs: Runs::Checked(vec![0]) };
        let log = Arc::new(dir.create(&log_path(seg), false).unwrap());
        let salt = new_salt(0, seg);
        log.write_at(&log_header(salt, seg, &enc), 0).unwrap();
        let mut seg_log = SegmentLog::new(log, salt, enc);
        seg_log.append(&[record]).unwrap();
        let recovered =
            recover(&dir, CELL, None).expect("overflowing run must be treated as torn, not panic");
        assert!(recovered.segments.is_empty());
    }
}
