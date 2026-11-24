// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a cell keeps in its directory. See the crate's docs.

use crate::dir::Dir;
use anyhow::{Context as _, Error, anyhow, ensure};
use base32_id::Padded;
use bucket_cell::cell::Kind;
use extent_frame::segment::CellId;
#[cfg(test)]
use extent_frame::segment::Contents;
use extent_frame::segment::Location;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use storage_device::ExtentId;

/// The first line of a cell's description: its format. cladefs: 6 names the bucket a host's cell
/// keeps its cells in (TASK-165.6); 7 places an extent whose data is in the bucket by its
/// location there (TASK-165.8), and holds one cell, no cell records (TASK-165.7); 8 has no
/// index: its cell is the bucket's, its write-ahead log and its segments' logs of their
/// commits, and its description names its capacity until a commit sets one, and its placement's
/// fencing token, if placed (TASK-165.9); 9's footers and its logs' records name the runs of a
/// commit spanning segments in each segment it spans (TASK-92.41); 10's logs' headers name their
/// segments, and every log has one (`wal::LOG_MAGIC`, TASK-341); 11's description names its cell
/// with its tenant's cell, `TENANT/CELL` (TASK-364); 12's names the cell it is as
/// `cell NAME`, not `volume NAME` (TASK-482); 13's footers and its logs' records name the versions
/// cut at their ends, and its description may name a version of its cell it is a copy of
/// (TASK-476); 14 writes every ID in base 32, its description's and its files' names, which
/// were in hexadecimal (`base32_id`, TASK-504); 15's description names its cell's kind alone, as a
/// cell has one kind, of no cell's name (doc-20, TASK-508.1); 16's logs carry a keyless header
/// hash (`FXLOG\0\0\x04`, board 6902) and ephemeral cells store closed segments in `segments/`
/// with an authenticated `extent_frame::table` `local-checkpoint` (board 6905); 17's framed files
/// are extent frames of version 3, their version a number (board 10040). A cell of another
/// format is refused ([`Format`]): nothing reads, converts or upgrades it.
pub const FORMAT: &str = "cladefs extent store 17";

/// cladefs: the error of a cell whose description is of another format than [`FORMAT`], an older
/// one included: refused, as nothing reads it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a cell of format {found:?}, not {FORMAT:?}: refused")]
pub struct Format {
    /// The description's first line.
    pub found: String,
}

/// A cell, as its user sees it: the size of the blocks extents are read and written in, the most
/// bytes its extents may hold, and its cell, whose extents its user mints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Description {
    pub block_size: u32,
    pub capacity: u64,
    pub cell: CellId,
}

/// The cell's description, its `store` file: its format, block size, and cell; and cladefs: the
/// URL of the bucket its cell is kept in, which every cell in a host's directory names
/// (`create_host`, TASK-165.6), and a cell of memory's need not; the capacity its cell has
/// until a commit sets one, as a cell made for it, or taken into it, says none; and the fencing
/// token of its placement, if it was placed (TASK-39.7), which takes the cell,
/// and so its head (TASK-165.9, TASK-364); and whether that placement's takings are forced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CellFile {
    pub block_size: u32,
    pub cell: CellId,
    pub bucket: Option<String>,
    pub capacity: u64,
    pub token: Option<u64>,
    /// cladefs: the token of the placement whose takings of a replicated cell are forced, if one
    /// is (`PlaceCell`'s `force`, TASK-681): they go on with the members of the configuration its
    /// head names that answer, too few for a take, the commits only the others held lost. A later
    /// placement, of another token, is forced only if it is asked to be.
    pub forced: Option<u64>,
    /// cladefs: of a forced placement whose taking fenced fewer members than a take needs, the
    /// epoch of the last commit the bucket held then, past which commits told replicated may be
    /// lost (TASK-681): what the cell's status says while that placement holds it.
    pub forced_after: Option<u64>,
    /// cladefs: the epochs of the replicated cell's configurations it is its writer's copy of
    /// (TASK-683): of each, its write-ahead log holds what the writer synced and the bucket
    /// doesn't hold yet, all of it, or, empty, nothing the bucket doesn't. Its takes add theirs;
    /// a write-ahead log dropped clears them.
    pub writer_copies: Vec<u64>,
    /// cladefs: its cell's kind, if it is kept in a bucket (TASK-364, doc-20).
    pub kind: Option<Kind>,
    /// cladefs: the version of its cell it is at, if it is one: its cell's state at the
    /// version, which nothing writes, as a cell at a version (doc-18, TASK-476).
    pub version: Option<u64>,
    /// cladefs: whether its cell is ephemeral (doc-25, TASK-698.2): kept on this host's disk alone,
    /// never uploaded, its closed segments raw, recovered from its local checkpoint
    /// ([`crate::ephemeral`]).
    pub ephemeral: bool,
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

impl CellFile {
    pub(crate) fn parse(text: &str) -> Result<Self, Error> {
        let found = text.lines().next().unwrap_or_default();
        if found != FORMAT {
            return Err(Format { found: found.to_owned() }.into());
        }
        let fields = fields(text, FORMAT)?;
        let token = fields.contains_key("token").then(|| number(&fields, "token")).transpose()?;
        let forced =
            fields.contains_key("forced").then(|| number(&fields, "forced")).transpose()?;
        let forced_after = (fields.contains_key("forced_after"))
            .then(|| number(&fields, "forced_after"))
            .transpose()?;
        let writer_copies = match fields.get("writer_copies") {
            None => Vec::new(),
            Some(epochs) => (epochs.split(' '))
                .map(|epoch| epoch.parse().with_context(|| format!("bad writer_copies {epochs:?}")))
                .collect::<Result<_, Error>>()?,
        };
        let version = (fields.get("version"))
            .map(|id| base32_id::parse_padded(id).ok_or_else(|| anyhow!("bad version {id:?}")))
            .transpose()?;
        let kind = (fields.get("kind"))
            .map(|kind| Kind::named(kind).ok_or_else(|| anyhow!("bad kind {kind:?}")))
            .transpose()?;
        Ok(Self {
            block_size: number(&fields, "block_size")?.try_into().context("bad block_size")?,
            cell: (fields.get("cell").ok_or_else(|| anyhow!("no cell"))?)
                .parse()
                .map_err(|err: String| anyhow!("bad cell: {err}"))?,
            bucket: fields.get("bucket").cloned(),
            capacity: number(&fields, "capacity")?,
            token,
            forced,
            forced_after,
            writer_copies,
            kind,
            version,
            ephemeral: fields.contains_key("ephemeral"),
        })
    }

    pub(crate) fn text(&self) -> String {
        let Self {
            block_size,
            cell,
            bucket,
            capacity,
            token,
            forced,
            forced_after,
            writer_copies,
            kind,
            version,
            ephemeral,
        } = self;
        let mut text = format!("{FORMAT}\nblock_size {block_size}\ncell {cell}\n");
        text += &format!("capacity {capacity}\n");
        if let Some(url) = bucket {
            text += &format!("bucket {url}\n");
        }
        if let Some(token) = token {
            text += &format!("token {token}\n");
        }
        if let Some(forced) = forced {
            text += &format!("forced {forced}\n");
        }
        if let Some(after) = forced_after {
            text += &format!("forced_after {after}\n");
        }
        if !writer_copies.is_empty() {
            let epochs: Vec<_> = writer_copies.iter().map(u64::to_string).collect();
            text += &format!("writer_copies {}\n", epochs.join(" "));
        }
        if let Some(kind) = kind {
            text += &format!("kind {}\n", kind.name());
        }
        if let Some(id) = version {
            text += &format!("version {}\n", Padded(*id));
        }
        if *ephemeral {
            text += "ephemeral yes\n";
        }
        text
    }
}

/// The description's file.
pub const DESCRIPTION: &str = "store";
/// The directory of segments, a directory for each cell's.
pub(crate) const SEGMENTS: &str = "segments";
/// The directory of extents stored on their own, a directory for each cell's.
pub(crate) const EXTENTS: &str = "extents";
/// cladefs: the directory of the open segments' logs, a directory for each cell's: the commits each
/// holds, appended and synced by the flushes that make them durable (TASK-165.9).
pub(crate) const LOGS: &str = "logs";
/// cladefs: an ephemeral cell's local checkpoint: its state as of the last segment it covers
/// ([`crate::ephemeral::LocalCheckpoint`], TASK-698.2).
pub(crate) const LOCAL_CHECKPOINT: &str = "local-checkpoint";

/// The directory of open segments' files, raw, a directory for each cell's.
pub(crate) const OPEN_SEGMENTS: &str = "open-segments";
/// The directory of the files of open extents stored on their own, raw, a directory for each
/// cell's.
pub(crate) const OPEN_EXTENTS: &str = "open-extents";

/// The directory of `cell`'s files of `kind` (`SEGMENTS`, `EXTENTS`, `LOGS`, ...).
pub(crate) fn cell_dir(kind: &'static str, cell: u64) -> String {
    format!("{kind}/{}", Padded(cell))
}

/// The path of file `number` of `cell`'s of `kind`: a segment's, a log's, an extent's.
pub(crate) fn numbered_path(kind: &'static str, cell: u64, number: u64) -> String {
    format!("{}/{}", cell_dir(kind, cell), Padded(number))
}

/// A segment: the cell that wrote it, and its number among the cell's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct SegmentId {
    pub cell: u64,
    pub number: u64,
}

/// The path of a segment.
pub(crate) fn segment_path(segment: SegmentId) -> String {
    numbered_path(SEGMENTS, segment.cell, segment.number)
}

/// The path of the raw file of a segment while it is open.
pub(crate) fn open_segment_path(segment: SegmentId) -> String {
    numbered_path(OPEN_SEGMENTS, segment.cell, segment.number)
}

/// The path of the raw file of `extent`, stored on its own, while it is open.
pub(crate) fn open_extent_path(extent: ExtentId) -> String {
    numbered_path(OPEN_EXTENTS, extent.cell, extent.sequence)
}

/// The path of the file of `extent`, stored on its own, in its cell's directory.
pub(crate) fn extent_path(extent: ExtentId) -> String {
    numbered_path(EXTENTS, extent.cell, extent.sequence)
}

/// cladefs: the path of the log of the commits of a segment, while it is open (TASK-165.9).
pub(crate) fn log_path(segment: SegmentId) -> String {
    numbered_path(LOGS, segment.cell, segment.number)
}

/// The number in a file's name: a segment's, a log's, an extent's sequence number, or a cell's
/// directory's.
pub(crate) fn parse_number(name: &str) -> Option<u64> {
    base32_id::parse_padded(name)
}

/// cladefs: the prefix of the names of spare logs, in a cell's directory of logs: logs of segments
/// gone, kept to be the logs of segments to come (`segments::Segments::recycle`, TASK-301).
pub(crate) const SPARE: &str = "spare-";

/// cladefs: the path of the spare log that was the log of `segment`.
pub(crate) fn spare_path(segment: SegmentId) -> String {
    format!("{}/{SPARE}{}", cell_dir(LOGS, segment.cell), Padded(segment.number))
}

/// cladefs: the prefix of the names of segments' framed files being made durable, in a cell's
/// directory of segments: renamed to theirs once their raw files' and logs' entries are durable
/// (`segments::make_durable`, TASK-310), so that no framed file's entry is durable before those;
/// an opening removes any left.
pub(crate) const FRAMING: &str = "framing-";

/// cladefs: the path of `segment`'s framed file being made durable ([`FRAMING`]).
pub(crate) fn framing_path(segment: SegmentId) -> String {
    format!("{}/{FRAMING}{}", cell_dir(SEGMENTS, segment.cell), Padded(segment.number))
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

/// The directories of a cell's files, each a directory for each cell's.
pub(crate) const KINDS: [&str; 5] = [SEGMENTS, EXTENTS, LOGS, OPEN_SEGMENTS, OPEN_EXTENTS];

/// Creates the directories of segments, extents, logs and open files, and `cell`'s in each, where
/// there are none: durable at the next sync of anything on their disk ([`make_dir`]).
pub(crate) fn create_cell_dirs<D: Dir>(dir: &D, cell: u64) -> Result<(), Error> {
    for kind in KINDS {
        make_dir(dir, kind)?;
        make_dir(dir, &cell_dir(kind, cell))?;
    }
    Ok(())
}

/// cladefs: makes the directory at `path` in `dir`, unless it is there: durable at the next sync
/// of anything on its disk, which commits its file system's journal, and lost at a crash before,
/// with all under it (board 5748), as ext4 and XFS have it.
pub(crate) fn make_dir<D: Dir>(dir: &D, path: &str) -> Result<(), Error> {
    match dir.create_dir(path) {
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        made => made.with_context(|| format!("creating {path}")),
    }
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

/// Adds `piece`, written last, to `pieces`, an extent's, joined to the last while they are one run:
/// they hold none of each other's bytes, so that which is painted over which does not matter.
/// cladefs: an extent written in runs one after another in a segment, as the guest's device splits
/// a write larger than its most I/O, is so one piece, which its commit seals where it is, rather
/// than reading it back and writing it again (TASK-92.1).
pub(crate) fn add_piece(pieces: &mut Vec<Piece>, mut piece: Piece) {
    while let Some(joined) = pieces.last().and_then(|last| last.joined(&piece)) {
        pieces.pop();
        piece = joined;
    }
    pieces.push(piece);
}

/// Where an extent's data is: pieces in segments, the later of overlapping pieces holding the data
/// written last, or a file of its own; or, cladefs, in the bucket, at its location there, which the
/// cell's user reads it from (TASK-165.8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Data {
    Pieces(Vec<Piece>),
    Own,
    Bucket(Location),
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

/// The files `extents`' data is in: of a cell's, those of its own layer
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

/// A change a commit makes to the cell, which the next commit records.
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
/// that are the cell's own it spends, as the cell spends them as it frees them (TASK-79.6);
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

/// Where an extent's data is, as a cell's user sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// One run in a segment of a cell's, at an offset: the extent is sealed.
    Run { cell: u64, number: u64, offset: u64 },
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
    // Not whole: the records before it are, as a log a crash cut short holds.
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
            segment: SegmentId { cell: 1, number },
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
        let file = CellFile {
            block_size: 4096,
            cell: CellId { tenant: 0xa, id: u64::MAX },
            bucket: None,
            capacity: 1,
            token: None,
            forced: None,
            forced_after: None,
            writer_copies: Vec::new(),
            kind: None,
            version: None,
            ephemeral: false,
        };
        assert_eq!(CellFile::parse(&file.text()).unwrap(), file);
        // cladefs: a host's cell names its bucket (TASK-165.6), and a placed one its placement's
        // token (TASK-165.9).
        let bucket = Some("file:///images/a/bucket".to_owned());
        let kept = CellFile { bucket, token: Some(7), ..file.clone() };
        assert_eq!(CellFile::parse(&kept.text()).unwrap(), kept);
        // cladefs: and a forced placement its token again (TASK-681).
        let forced = CellFile { forced: Some(7), forced_after: Some(12), ..kept.clone() };
        assert_eq!(CellFile::parse(&forced.text()).unwrap(), forced);
        // cladefs: and the epochs of the configurations it is its writer's copy of (TASK-683).
        let written = CellFile { writer_copies: vec![9], ..kept.clone() };
        assert_eq!(CellFile::parse(&written.text()).unwrap(), written);
        let written = CellFile { writer_copies: vec![9, 12], ..kept.clone() };
        assert_eq!(CellFile::parse(&written.text()).unwrap(), written);
        // cladefs: older formats are refused, by their format (TASK-165.6), the index's last
        // (TASK-165.9).
        for older in [
            "cladefs extent store 2",
            "cladefs extent store 7",
            "cladefs extent store 11",
            "cladefs extent store 12",
            "cladefs extent store 13",
            "cladefs extent store 14",
            "cladefs extent store 15",
            "cladefs extent store 16",
        ] {
            let refused = CellFile::parse(&format!("{older}\nblock_size 4096\ncell 1\n"));
            let found = refused.unwrap_err().downcast::<Format>().unwrap().found;
            assert_eq!(found, older);
        }
        let capacity = "capacity 4096\n";
        assert!(CellFile::parse(&format!("{FORMAT}\nblock_size 4096\n{capacity}")).is_err());
        assert!(CellFile::parse(&format!("{FORMAT}\nblock_size 4096\ncell 1\n")).is_err());
        // cladefs: its cell is named with its cell, as the bucket names it (TASK-364).
        let text = file.text();
        assert!(text.contains("\ncell a/fzzzzzzzzzzzz\n"), "{text}");
        let bare = format!("{FORMAT}\nblock_size 4096\ncell 1\n{capacity}");
        let refused = CellFile::parse(&bare).unwrap_err().to_string();
        assert_eq!(refused, "bad cell: no cell, TENANT/CELL in base 32: 1");
        // cladefs: a cell kept in a bucket names its cell's kind (doc-20).
        let named = CellFile { kind: Some(Kind::Sqlite), ..kept.clone() };
        assert!(named.text().ends_with("\nkind sqlite\n"));
        assert_eq!(CellFile::parse(&named.text()).unwrap(), named);
        // cladefs: a cell at a version names the version (TASK-476).
        let copy = CellFile { version: Some(190), ..kept.clone() };
        assert!(copy.text().ends_with("version 000000000005y\n"));
        assert_eq!(CellFile::parse(&copy.text()).unwrap(), copy);
        // cladefs: an ephemeral cell says so (doc-25, TASK-698.2).
        let ephemeral = CellFile { ephemeral: true, ..named.clone() };
        assert!(ephemeral.text().ends_with("\nkind sqlite\nephemeral yes\n"));
        assert_eq!(CellFile::parse(&ephemeral.text()).unwrap(), ephemeral);
        let hex = format!("{}version 00000000000000be\n", kept.text());
        assert_eq!(
            CellFile::parse(&hex).unwrap_err().to_string(),
            "bad version \"00000000000000be\""
        );
        let text = format!("{}kind ext4\n", kept.text());
        assert_eq!(CellFile::parse(&text).unwrap_err().to_string(), "bad kind \"ext4\"");
    }
}
