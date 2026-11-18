// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a store keeps in its directory. See the crate's docs.

use crate::dir::Dir;
use anyhow::{Context as _, Error, anyhow, ensure};
use base32_id::Padded;
use extent_bucket::cell::Kind;
use extent_frame::segment::Branch;
#[cfg(test)]
use extent_frame::segment::Contents;
use extent_frame::segment::Location;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use storage_device::ExtentId;

/// The first line of a store's description: its format. cladefs: 6 names the bucket a host's store
/// keeps its branches in (TASK-165.6); 7 places an extent whose data is in the bucket by its
/// location there (TASK-165.8), and holds one branch, no branch records (TASK-165.7); 8 has no
/// index: its branch is the bucket's, its write-ahead log and its segments' logs of their
/// commits, and its description names its capacity until a commit sets one, and its placement's
/// fencing token, if placed (TASK-165.9); 9's footers and its logs' records name the runs of a
/// commit spanning segments in each segment it spans (TASK-92.41); 10's logs' headers name their
/// segments, and every log has one (`wal::LOG_MAGIC`, TASK-341); 11's description names its branch
/// with its tenant's cell, `TENANT/CELL/ID` (TASK-364); 12's names the vdisk of its cell it is as
/// `vdisk NAME`, not `volume NAME` (TASK-482); 13's footers and its logs' records name the versions
/// cut at their ends, and its description may name a version of its branch it is a copy of
/// (TASK-476); 14 writes every ID in base 32, its description's and its files' names, which
/// were in hexadecimal (`base32_id`, TASK-504); 15's description names its cell's kind alone, as a
/// cell has one kind, of no vdisk's name (doc-20, TASK-508.1). A store of another format is
/// refused ([`Format`]): nothing reads, converts or upgrades it.
pub(crate) const FORMAT: &str = "cladefs extent store 15";

/// cladefs: the error of a store whose description is of another format than [`FORMAT`], an older
/// one included: refused, as nothing reads it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a store of format {found:?}, not {FORMAT:?}: refused")]
pub struct Format {
    /// The description's first line.
    pub found: String,
}

/// A store, as its user sees it: the size of the blocks extents are read and written in, the most
/// bytes its extents may hold, and its branch, whose extents its user mints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Description {
    pub block_size: u32,
    pub capacity: u64,
    pub branch: Branch,
}

/// The store's description, its `store` file: its format, block size, and branch; and cladefs: the
/// URL of the bucket its branch is kept in, which every store in a host's directory names
/// (`create_host`, TASK-165.6), and a store of memory's need not; the capacity its branch has
/// until a commit sets one, as a branch made for it, or taken into it, says none; and the fencing
/// token of its placement, if it was placed (TASK-39.7), which takes the branch's cell,
/// and so its head (TASK-165.9, TASK-364).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreFile {
    pub block_size: u32,
    pub branch: Branch,
    pub bucket: Option<String>,
    pub capacity: u64,
    pub token: Option<u64>,
    /// cladefs: its cell's kind, if it is kept in a bucket (TASK-364, doc-20).
    pub kind: Option<Kind>,
    /// cladefs: the version of its branch it is a copy of, if it is one: its branch's state at the
    /// version, which nothing writes, as a version's copy (doc-18, TASK-476).
    pub version: Option<u64>,
}

/// The fields of a text of lines of `key value`, after its first, which is `first`.
fn fields(text: &str, first: &str) -> Result<HashMap<String, String>, Error> {
    let mut lines = text.lines();
    ensure!(lines.next() == Some(first), "not {first:?}");
    let mut fields = HashMap::new();
    for line in lines {
        let (key, value) = line.split_once(' ').ok_or_else(|| anyhow!("bad line {line:?}"))?;
        fields.insert(key.to_owned(), value.to_owned());
    }
    Ok(fields)
}

fn number(fields: &HashMap<String, String>, key: &str) -> Result<u64, Error> {
    let value = fields.get(key).ok_or_else(|| anyhow!("no {key}"))?;
    value.parse().with_context(|| format!("bad {key}"))
}

impl StoreFile {
    pub(crate) fn parse(text: &str) -> Result<Self, Error> {
        let found = text.lines().next().unwrap_or_default();
        if found != FORMAT {
            return Err(Format { found: found.to_owned() }.into());
        }
        let fields = fields(text, FORMAT)?;
        let token = fields.contains_key("token").then(|| number(&fields, "token")).transpose()?;
        let version = (fields.get("version"))
            .map(|id| base32_id::parse_padded(id).ok_or_else(|| anyhow!("bad version {id:?}")))
            .transpose()?;
        let kind = (fields.get("kind"))
            .map(|kind| Kind::named(kind).ok_or_else(|| anyhow!("bad kind {kind:?}")))
            .transpose()?;
        Ok(Self {
            block_size: number(&fields, "block_size")?.try_into().context("bad block_size")?,
            branch: (fields.get("branch").ok_or_else(|| anyhow!("no branch"))?)
                .parse()
                .map_err(|err: String| anyhow!("bad branch: {err}"))?,
            bucket: fields.get("bucket").cloned(),
            capacity: number(&fields, "capacity")?,
            token,
            kind,
            version,
        })
    }

    pub(crate) fn text(&self) -> String {
        let Self { block_size, branch, bucket, capacity, token, kind, version } = self;
        let mut text = format!("{FORMAT}\nblock_size {block_size}\nbranch {branch}\n");
        text += &format!("capacity {capacity}\n");
        if let Some(url) = bucket {
            text += &format!("bucket {url}\n");
        }
        if let Some(token) = token {
            text += &format!("token {token}\n");
        }
        if let Some(kind) = kind {
            text += &format!("kind {}\n", kind.name());
        }
        if let Some(id) = version {
            text += &format!("version {}\n", Padded(*id));
        }
        text
    }
}

/// The description's file.
pub(crate) const DESCRIPTION: &str = "store";
/// The directory of segments, a directory for each branch's.
pub(crate) const SEGMENTS: &str = "segments";
/// The directory of extents stored on their own, a directory for each branch's.
pub(crate) const EXTENTS: &str = "extents";
/// cladefs: the directory of the open segments' logs, a directory for each branch's: the commits each
/// holds, appended and synced by the flushes that make them durable (TASK-165.9).
pub(crate) const LOGS: &str = "logs";
/// The directory of open segments' files, raw, a directory for each branch's.
pub(crate) const OPEN_SEGMENTS: &str = "open-segments";
/// The directory of the files of open extents stored on their own, raw, a directory for each
/// branch's.
pub(crate) const OPEN_EXTENTS: &str = "open-extents";

/// The directory of `branch`'s files of `kind` (`SEGMENTS`, `EXTENTS`, `LOGS`, ...).
pub(crate) fn branch_dir(kind: &'static str, branch: u64) -> String {
    format!("{kind}/{}", Padded(branch))
}

/// The path of file `number` of `branch`'s of `kind`: a segment's, a log's, an extent's.
pub(crate) fn numbered_path(kind: &'static str, branch: u64, number: u64) -> String {
    format!("{}/{}", branch_dir(kind, branch), Padded(number))
}

/// A segment: the branch that wrote it, and its number among the branch's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct SegmentId {
    pub branch: u64,
    pub number: u64,
}

/// The path of a segment.
pub(crate) fn segment_path(segment: SegmentId) -> String {
    numbered_path(SEGMENTS, segment.branch, segment.number)
}

/// The path of the raw file of a segment while it is open.
pub(crate) fn open_segment_path(segment: SegmentId) -> String {
    numbered_path(OPEN_SEGMENTS, segment.branch, segment.number)
}

/// The path of the raw file of `extent`, stored on its own, while it is open.
pub(crate) fn open_extent_path(extent: ExtentId) -> String {
    numbered_path(OPEN_EXTENTS, extent.branch, extent.sequence)
}

/// The path of the file of `extent`, stored on its own, in its branch's directory.
pub(crate) fn extent_path(extent: ExtentId) -> String {
    numbered_path(EXTENTS, extent.branch, extent.sequence)
}

/// cladefs: the path of the log of the commits of a segment, while it is open (TASK-165.9).
pub(crate) fn log_path(segment: SegmentId) -> String {
    numbered_path(LOGS, segment.branch, segment.number)
}

/// The number in a file's name: a segment's, a log's, an extent's sequence number, or a branch's
/// directory's.
pub(crate) fn parse_number(name: &str) -> Option<u64> {
    base32_id::parse_padded(name)
}

/// cladefs: the prefix of the names of spare logs, in a branch's directory of logs: logs of segments
/// gone, kept to be the logs of segments to come (`segments::Segments::recycle`, TASK-301).
pub(crate) const SPARE: &str = "spare-";

/// cladefs: the path of the spare log that was the log of `segment`.
pub(crate) fn spare_path(segment: SegmentId) -> String {
    format!("{}/{SPARE}{}", branch_dir(LOGS, segment.branch), Padded(segment.number))
}

/// cladefs: the prefix of the names of segments' framed files being made durable, in a branch's
/// directory of segments: renamed to theirs once their raw files' and logs' entries are durable
/// (`segments::make_durable`, TASK-310), so that no framed file's entry is durable before those;
/// an opening removes any left.
pub(crate) const FRAMING: &str = "framing-";

/// cladefs: the path of `segment`'s framed file being made durable ([`FRAMING`]).
pub(crate) fn framing_path(segment: SegmentId) -> String {
    format!("{}/{FRAMING}{}", branch_dir(SEGMENTS, segment.branch), Padded(segment.number))
}

/// The numbers of the files in `path`, in order; cladefs: but spare logs, which hold none, and
/// framed files being made durable ([`FRAMING`]).
pub(crate) fn list_numbers<D: Dir>(dir: &D, path: &str) -> Result<Vec<u64>, Error> {
    let mut numbers = Vec::new();
    let named = |name: &String| !name.starts_with(SPARE) && !name.starts_with(FRAMING);
    for name in dir.list(path)?.into_iter().filter(named) {
        numbers.push(parse_number(&name).ok_or_else(|| anyhow!("{path}/{name} is no file"))?);
    }
    numbers.sort_unstable();
    Ok(numbers)
}

/// The directories of a store's files, each a directory for each branch's.
pub(crate) const KINDS: [&str; 5] = [SEGMENTS, EXTENTS, LOGS, OPEN_SEGMENTS, OPEN_EXTENTS];

/// Creates `branch`'s directories of segments, extents, logs and open files, where it has none.
pub(crate) fn create_branch_dirs<D: Dir>(dir: &D, branch: u64) -> Result<(), Error> {
    for kind in KINDS {
        let path = branch_dir(kind, branch);
        match dir.create_dir(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            created => created.with_context(|| format!("creating {path}"))?,
        }
    }
    Ok(())
}

/// `len` bytes of an extent's data, from `extent_offset` in the extent, at `offset` in segment
/// `segment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Piece {
    pub segment: SegmentId,
    pub offset: u64,
    pub extent_offset: u64,
    pub len: u64,
}

impl Piece {
    /// cladefs: this piece and `other` as one, if they are one run: one right after the other in
    /// the extent, and so in the segment, whichever comes first.
    fn joined(&self, other: &Piece) -> Option<Piece> {
        let (first, second) =
            if self.extent_offset < other.extent_offset { (self, other) } else { (other, self) };
        let run = first.segment == second.segment
            && first.extent_offset + first.len == second.extent_offset
            && first.offset + first.len == second.offset;
        run.then(|| Piece { len: first.len + second.len, ..*first })
    }
}

/// Adds `piece`, written last, to `pieces`, an extent's, joined to the last while they are one
/// run: they hold none of each other's bytes, so that which is painted over which does not matter.
/// cladefs: an extent written in runs one after another in a segment, as the guest's device splits a
/// write larger than its most I/O, is so one piece, which its commit seals where it is, rather than
/// reading it back and writing it again (TASK-92.1).
pub(crate) fn add_piece(pieces: &mut Vec<Piece>, mut piece: Piece) {
    while let Some(joined) = pieces.last().and_then(|last| last.joined(&piece)) {
        pieces.pop();
        piece = joined;
    }
    pieces.push(piece);
}

/// Where an extent's data is: pieces in segments, the later of overlapping pieces holding the data
/// written last, or a file of its own; or, cladefs, in the bucket, at its location there, which the
/// store's user reads it from (TASK-165.8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Data {
    Pieces(Vec<Piece>),
    Own,
    Bucket(Location),
}

impl Data {
    /// cladefs: if in the bucket, named at `to`, where the collector moved it, if it is named on its
    /// way there (`extent_bucket::layout::further`, TASK-195).
    pub fn follow(&mut self, to: &Location) {
        if let Data::Bucket(at) = self
            && extent_bucket::layout::further(&at.key, &to.key)
        {
            *at = to.clone();
        }
    }
}

/// An extent: its length, the end of its furthest write, and where its data is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Extent {
    pub len: u64,
    pub data: Data,
}

/// A file of extents' data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum FileKey {
    Segment(SegmentId),
    Own(ExtentId),
}

impl FileKey {
    /// Its paths: framed, and raw, while it is open.
    pub(crate) fn paths(&self) -> [String; 2] {
        match self {
            Self::Segment(segment) => [segment_path(*segment), open_segment_path(*segment)],
            Self::Own(extent) => [extent_path(*extent), open_extent_path(*extent)],
        }
    }
}

/// The files `extents`' data is in: of a branch's, those of its own layer
/// ([`Extents::own`](crate::extents::Extents::own)), the bucket's in the bucket.
pub(crate) fn files_of<'a>(
    extents: impl IntoIterator<Item = (&'a ExtentId, &'a Extent)>,
) -> BTreeSet<FileKey> {
    let mut files = BTreeSet::new();
    for (extent, entry) in extents {
        match &entry.data {
            Data::Pieces(pieces) => {
                files.extend(pieces.iter().map(|piece| FileKey::Segment(piece.segment)));
            }
            Data::Own => {
                files.insert(FileKey::Own(*extent));
            }
            Data::Bucket(_) => {}
        }
    }
    files
}

/// A change a commit makes to the branch, which the next commit records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// A piece of the extent's data was written to a segment.
    Write { extent: ExtentId, piece: Piece },
    /// The extent's own file was written, which it holds `len` bytes of now.
    WriteOwn { extent: ExtentId, len: u64 },
    /// The extent was freed.
    Free(ExtentId),
    /// The extent's data was copied to the pieces, which replace its others: sealing it in one
    /// run.
    Move { extent: ExtentId, pieces: Vec<Piece> },
    /// The extent's data was copied to its own file, sealing it at its length.
    MoveOwn { extent: ExtentId },
}

/// cladefs: of `changes`, an epoch's, the extents it freed that a commit held: but those written in
/// it, open, which their frees let go of before any commit held them (TASK-79.1). Those of them
/// that are the branch's own it spends, as the store spends them as it frees them (TASK-79.6);
/// and a footer frees them alone, so that what its readers spend of it, all its frees, is that
/// too (TASK-160.15).
pub(crate) fn held_frees(changes: &[Change]) -> HashSet<ExtentId> {
    let (mut written, mut freed) = (HashSet::new(), HashSet::new());
    for change in changes {
        match change {
            Change::Write { extent, .. } | Change::WriteOwn { extent, .. } => {
                written.insert(*extent);
            }
            Change::Free(extent) if !written.contains(extent) => {
                freed.insert(*extent);
            }
            _ => {}
        }
    }
    freed
}

/// Where an extent's data is, as a store's user sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// One run in a segment of a branch's, at an offset: the extent is sealed.
    Run { branch: u64, number: u64, offset: u64 },
    /// Its own file.
    Own,
    /// cladefs: in the bucket, at its location there.
    Bucket(Location),
    /// Pieces, as it was written: the extent is open.
    Pieces,
}

/// A record's header: its payload's length, and its hash.
const HEADER: usize = 4 + 8;

/// The seed of records' hashes.
const SEED: u64 = 0x6678_6461_7832_0001;

fn hash(payload: &[u8]) -> u64 {
    rapidhash::rapidhash_seeded(payload, SEED ^ payload.len() as u64)
}

/// `record`, as a log holds it: its header, then its payload.
pub(crate) fn encode<R: Serialize>(record: &R) -> Vec<u8> {
    encode_salted(record, 0)
}

/// cladefs: `record`, encoded as [`encode`] does, its hash salted with `salt`: what reads it with
/// another salt finds torn, as a log reused finds what it held before (TASK-301).
pub(crate) fn encode_salted<R: Serialize>(record: &R, salt: u64) -> Vec<u8> {
    frame_salted(&bincode::serialize(record).expect("records serialize"), salt)
}

/// cladefs: `payload`, a record's, as a log holds it, its hash salted with `salt`: its header, then
/// it ([`encode_salted`]).
pub(crate) fn frame_salted(payload: &[u8], salt: u64) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("a record is under 4 GiB");
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&(hash(payload) ^ salt).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// The records at the start of `bytes` that are whole and intact, and how many bytes they take.
pub(crate) fn decode<R: serde::de::DeserializeOwned>(bytes: &[u8]) -> (Vec<R>, usize) {
    decode_with(bytes, 0, |_, payload| bincode::deserialize(payload).ok())
}

/// cladefs: the records at the start of `bytes` that are whole and intact, their hashes salted with
/// `salt` ([`encode_salted`]), each payload's as `parse` reads it, given where in `bytes` its
/// record starts, and how many bytes they take.
pub(crate) fn decode_with<R>(
    bytes: &[u8],
    salt: u64,
    parse: impl Fn(usize, &[u8]) -> Option<R>,
) -> (Vec<R>, usize) {
    let mut records = Vec::new();
    let mut at = 0;
    while let Some(header) = bytes.get(at..at + HEADER) {
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let expected = u64::from_le_bytes(header[4..].try_into().unwrap());
        let Some(payload) = bytes.get(at + HEADER..at + HEADER + len) else { break };
        if len == 0 || hash(payload) ^ salt != expected {
            break;
        }
        let Some(record) = parse(at, payload) else { break };
        records.push(record);
        at += HEADER + len;
    }
    (records, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record of a commit of `epoch`'s, as a segment's log holds it (TASK-165.9).
    fn commit(epoch: u64) -> Contents {
        Contents {
            commits: vec![extent_frame::segment::tag(3, epoch)],
            extents: vec![(ExtentId::new(1, 2), 4096, 8192)],
            frees: vec![ExtentId::new(1, 1)],
            root: Some(b"root".to_vec()),
            next_sequence: 9,
            capacity: Some(1 << 20),
            ..Contents::default()
        }
    }

    fn epochs(records: &[Contents]) -> Vec<u64> {
        let last = |record: &Contents| extent_frame::segment::epoch_of(record.commits[0]);
        records.iter().map(last).collect()
    }

    #[test]
    fn records_decode_until_one_is_torn() {
        let bytes = [encode(&commit(1)), encode(&commit(2)), encode(&commit(3))].concat();
        let (records, len) = decode::<Contents>(&bytes);
        assert_eq!(epochs(&records), vec![1, 2, 3]);
        assert_eq!(len, bytes.len());

        let second = encode(&commit(1)).len();
        // Torn in the third record, or a byte of it changed.
        let (records, len) = decode::<Contents>(&bytes[..bytes.len() - 1]);
        assert_eq!((epochs(&records), len), (vec![1, 2], 2 * second));
        let mut changed = bytes.clone();
        changed[2 * second + HEADER + 1] ^= 1;
        assert_eq!(epochs(&decode::<Contents>(&changed).0), vec![1, 2]);
        // Zeros past the records, as a file that grew holds.
        let zeros = [bytes.clone(), vec![0; 4096]].concat();
        assert_eq!(decode::<Contents>(&zeros), (decode(&bytes).0, bytes.len()));
    }

    /// cladefs: pieces one right after another, in the extent and so in the segment, are one,
    /// whichever was written first; those apart in either, holding each other's bytes, or in
    /// other segments, are not (TASK-92.1).
    #[test]
    fn pieces_of_one_run_are_one() {
        let piece = |number, offset, extent_offset, len| Piece {
            segment: SegmentId { branch: 1, number },
            offset,
            extent_offset,
            len,
        };
        let added = |written: &[Piece]| {
            let mut pieces = Vec::new();
            for &piece in written {
                add_piece(&mut pieces, piece);
            }
            pieces
        };
        let one = [piece(0, 0, 0, 12)];
        assert_eq!(added(&[piece(0, 0, 0, 8), piece(0, 8, 8, 4)]), one);
        assert_eq!(added(&[piece(0, 8, 8, 4), piece(0, 0, 0, 8)]), one);
        // The last two joined, then the first to them.
        let three = [piece(0, 8, 8, 4), piece(0, 0, 0, 4), piece(0, 4, 4, 4)];
        assert_eq!(added(&three), one);
        for apart in [
            [piece(0, 0, 0, 8), piece(0, 12, 8, 4)],
            [piece(0, 0, 0, 8), piece(0, 8, 12, 4)],
            [piece(0, 0, 0, 8), piece(0, 8, 4, 8)],
            [piece(0, 0, 0, 8), piece(1, 8, 8, 4)],
        ] {
            assert_eq!(added(&apart), apart);
        }
    }

    #[test]
    fn descriptions() {
        let file = StoreFile {
            block_size: 4096,
            branch: Branch {
                cell: extent_frame::segment::Cell { tenant: 0xa, id: u64::MAX },
                id: u64::MAX,
            },
            bucket: None,
            capacity: 1,
            token: None,
            kind: None,
            version: None,
        };
        assert_eq!(StoreFile::parse(&file.text()).unwrap(), file);
        // cladefs: a host's store names its bucket (TASK-165.6), and a placed one its placement's
        // token (TASK-165.9).
        let bucket = Some("file:///images/a/bucket".to_owned());
        let kept = StoreFile { bucket, token: Some(7), ..file.clone() };
        assert_eq!(StoreFile::parse(&kept.text()).unwrap(), kept);
        // cladefs: older formats are refused, by their format (TASK-165.6), the index's last
        // (TASK-165.9).
        for older in [
            "cladefs extent store 2",
            "cladefs extent store 7",
            "cladefs extent store 11",
            "cladefs extent store 12",
            "cladefs extent store 13",
            "cladefs extent store 14",
        ] {
            let refused = StoreFile::parse(&format!("{older}\nblock_size 4096\nbranch 1\n"));
            let found = refused.unwrap_err().downcast::<Format>().unwrap().found;
            assert_eq!(found, older);
        }
        let capacity = "capacity 4096\n";
        assert!(StoreFile::parse(&format!("{FORMAT}\nblock_size 4096\n{capacity}")).is_err());
        assert!(StoreFile::parse(&format!("{FORMAT}\nblock_size 4096\nbranch 1\n")).is_err());
        // cladefs: its branch is named with its cell, as the bucket names it (TASK-364).
        let text = file.text();
        assert!(text.contains("\nbranch a/fzzzzzzzzzzzz/fzzzzzzzzzzzz\n"), "{text}");
        let bare = format!("{FORMAT}\nblock_size 4096\nbranch 1\n{capacity}");
        let refused = StoreFile::parse(&bare).unwrap_err().to_string();
        assert_eq!(refused, "bad branch: no branch, TENANT/CELL/ID in base 32: 1");
        // cladefs: a store kept in a bucket names its cell's kind (doc-20).
        let named = StoreFile { kind: Some(Kind::Sqlite), ..kept.clone() };
        assert!(named.text().ends_with("\nkind sqlite\n"));
        assert_eq!(StoreFile::parse(&named.text()).unwrap(), named);
        // cladefs: a version's copy names the version (TASK-476).
        let copy = StoreFile { version: Some(190), ..kept.clone() };
        assert!(copy.text().ends_with("version 000000000005y\n"));
        assert_eq!(StoreFile::parse(&copy.text()).unwrap(), copy);
        let hex = format!("{}version 00000000000000be\n", kept.text());
        assert_eq!(
            StoreFile::parse(&hex).unwrap_err().to_string(),
            "bad version \"00000000000000be\""
        );
        let text = format!("{}kind ext4\n", kept.text());
        assert_eq!(StoreFile::parse(&text).unwrap_err().to_string(), "bad kind \"ext4\"");
    }

    #[test]
    fn names() {
        let extent = ExtentId::new(11, 90);
        assert_eq!(extent_path(extent), "extents/000000000000b/000000000002t");
        let segment = SegmentId { branch: 11, number: 77 };
        assert_eq!(segment_path(segment), "segments/000000000000b/000000000002d");
        assert_eq!(log_path(segment), "logs/000000000000b/000000000002d");
        assert_eq!(parse_number("000000000002d"), Some(77));
        assert_eq!(parse_number("2d"), None);
        assert_eq!(parse_number("000000000000004d"), None);
    }
}
