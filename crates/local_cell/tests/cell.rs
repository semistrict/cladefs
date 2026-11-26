// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The cell's behaviour, in memory and in a directory of the host's.

mod common;

use base32_id::Padded;
use common::noise;
use extent_frame::segment::{Chain, Contents, Location, NewVersion};
use local_cell::{
    BucketData, Closed, Description, Dir, File as _, HostDir, LocalCell, Located, MemDir, MemFile,
    NoSpace, Opening, Options, Place, Relocating, Sparse, SpentExtent, TooLong, TooSmall,
    Unsectored,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use storage_device::{ExtentId, NoExtent, OtherCell, SealedExtent};

const BLOCK: usize = 4096;
const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);
/// The most bytes an extent holds: doc-4's `max_extent`, the cell's default.
const MAX_EXTENT: u64 = 64 << 20;

/// Small segments and large extents, so that tests make several of each.
const OPTIONS: Options = Options {
    max_extent: MAX_EXTENT,
    segment_size: 64 << 10,
    segment_age: std::time::Duration::from_secs(3600),
    large_extent: 32 << 10,
    open_files: 2,
    held_most: 256 << 10,
};

/// The chain of a writer of the cell in object storage, whose segments the cell closes: that of
/// a cell opened where the bucket holds nothing of its cell (`Opening::new`).
const WRITER: Chain = Chain::new(CELL, 0, 0, 0);

fn extent(sequence: u64) -> ExtentId {
    ExtentId::new(CELL.id, sequence)
}

/// The cell's directory of `kind` (`segments`, `extents`, `index`).
fn of(kind: &str, cell: u64) -> String {
    format!("{kind}/{}", Padded(cell))
}

/// Bytes each their offset modulo 251, from `offset`, plus `seed`.
fn pattern(seed: u8, offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len).map(|i| (i % 251) as u8 ^ seed).collect()
}

fn create<D: Dir>(dir: D, capacity: u64) -> LocalCell<D> {
    let description = Description { block_size: BLOCK as u32, capacity, cell: CELL };
    host_fs::may_block(|| LocalCell::create(dir, description, OPTIONS).unwrap())
}

/// cladefs: the cell in `dir`, opened where the bucket holds `state` of its cell, its writer's
/// chain `chain` (TASK-165.9), its head naming the segments before the chain's, of its epoch.
fn opened_on(dir: MemDir, state: bucket_cell::State, chain: Chain) -> LocalCell<MemDir> {
    let head = bucket_cell::layout::Head::default().holding(chain.epoch(), chain.next_segment());
    let taken = bucket_cell::TakenCell::of(head, &state, chain);
    let keys = bucket_cell::testing::ring([CELL]);
    let opening = Opening { state, layer: None, taken: Some(taken), cache: None, keys, held: None };
    LocalCell::recover_unmade(dir).unwrap().open(OPTIONS, opening).unwrap()
}

/// cladefs: a new cell in `dir`, of `capacity` bytes, made whole, opened as [`opened_on`] opens it.
fn created_on(
    dir: MemDir,
    capacity: u64,
    state: bucket_cell::State,
    chain: Chain,
) -> LocalCell<MemDir> {
    let description = Description { block_size: BLOCK as u32, capacity, cell: CELL };
    LocalCell::make(&dir, description, None, None).unwrap();
    let cell = opened_on(dir, state, chain);
    cell.made().unwrap();
    cell
}

fn read<D: Dir>(cell: &LocalCell<D>, extent: ExtentId, offset: u64, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    host_fs::may_block(|| cell.read(extent, offset, &mut data).unwrap());
    data
}

fn error_is<E: std::error::Error + PartialEq + Send + Sync + 'static>(
    result: Result<(), anyhow::Error>,
    expected: E,
) {
    let error = result.expect_err("succeeded");
    assert_eq!(error.downcast_ref::<E>(), Some(&expected), "{error:?}");
}

/// Writes, reads, commits and reopens a cell in `dir`, which `reopen` opens again.
async fn reads_what_was_written<D: Dir>(dir: D, reopen: impl Fn(&D) -> D) {
    let cell = create(dir, 64 << 20);
    // Small extents, in segments, one in pieces, and a large one, in a file of its own.
    cell.write(extent(1), 0, &pattern(1, 0, 3 * BLOCK)).await.unwrap();
    cell.write(extent(2), 2 * BLOCK as u64, &pattern(2, 0, BLOCK)).await.unwrap();
    cell.write(extent(2), 0, &pattern(3, 0, BLOCK)).await.unwrap();
    cell.write(extent(2), 2 * BLOCK as u64, &pattern(4, 0, BLOCK)).await.unwrap();
    cell.write(extent(3), 0, &pattern(5, 0, 40 << 10)).await.unwrap();

    let check = |cell: &LocalCell<D>| {
        assert_eq!(read(cell, extent(1), 0, 3 * BLOCK), pattern(1, 0, 3 * BLOCK));
        // Blocks never written read as zeros, within an extent and past its end; the later of
        // two writes of a block holds.
        assert_eq!(
            read(cell, extent(2), 0, 4 * BLOCK),
            [pattern(3, 0, BLOCK), vec![0; BLOCK], pattern(4, 0, BLOCK), vec![0; BLOCK]].concat()
        );
        assert_eq!(
            read(cell, extent(3), BLOCK as u64, 40 << 10),
            [pattern(5, BLOCK, 36 << 10), vec![0; BLOCK]].concat()
        );
        assert_eq!(
            cell.extents().unwrap(),
            vec![
                (extent(1), 3 * BLOCK as u64),
                (extent(2), 3 * BLOCK as u64),
                (extent(3), 40 << 10)
            ]
        );
        assert_eq!(cell.info().unwrap().used, 64 << 10);
        assert_eq!(cell.info().unwrap().next_sequence, 4);
        error_is(cell.read(extent(4), 0, &mut [0; BLOCK]), NoExtent(extent(4)));
    };
    check(&cell);
    assert_eq!(cell.usage().unwrap().own_bytes, 40 << 10);
    cell.commit(Some(b"root")).await.unwrap();
    check(&cell);

    // Uncommitted writes and frees are gone after reopening.
    cell.write(extent(9), 0, &pattern(9, 0, BLOCK)).await.unwrap();
    cell.free(extent(1)).unwrap();
    let dir = host_fs::may_block(|| reopen(cell.dir()));
    drop(cell);
    let cell = host_fs::may_block(|| LocalCell::open(dir, OPTIONS).unwrap());
    check(&cell);
    assert_eq!(cell.root().unwrap(), b"root");
    assert!(cell.last_commit().unwrap() >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_what_was_written_in_memory() {
    reads_what_was_written(MemDir::new(), MemDir::clone).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_what_was_written_on_the_host() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().to_owned();
    let dir = host_fs::may_block(|| HostDir::new(&path).unwrap());
    reads_what_was_written(dir, |dir| HostDir::new(dir.path()).unwrap()).await;
}

/// Extents are written once: commits seal them. A cell writes only its cell's extents.
#[tokio::test(flavor = "multi_thread")]
async fn writes_once() {
    let cell = create(MemDir::new(), 1 << 20);
    assert!(!cell.has_open().unwrap());
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.write(extent(1), BLOCK as u64, &[2; BLOCK]).await.unwrap();
    assert!(cell.has_open().unwrap());
    cell.commit(None).await.unwrap();
    assert!(!cell.has_open().unwrap());
    error_is(cell.write(extent(1), 2 * BLOCK as u64, &[3; BLOCK]).await, SealedExtent(extent(1)));
    error_is(
        cell.write(ExtentId::new(CELL.id + 1, 1), 0, &[3; BLOCK]).await,
        OtherCell(ExtentId::new(CELL.id + 1, 1)),
    );
    assert!(cell.write(extent(2), 1, &[3; BLOCK]).await.is_err(), "unaligned");
    assert_eq!(read(&cell, extent(1), 0, 2 * BLOCK), [[1; BLOCK], [2; BLOCK]].concat());
}

/// Board 2762: no cell writes an extent of the last sequence, past which no next sequence is: a
/// write of one is refused, its next sequence as it was, and the cell goes on. Written, it wrapped
/// the next sequence to 0 (release) or panicked under the state lock (checked builds).
#[tokio::test(flavor = "multi_thread")]
async fn extents_of_the_last_sequence_are_refused() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    let last = extent(u64::MAX);
    error_is(cell.write(last, 0, &[2; BLOCK]).await, local_cell::LastSequence(last));
    assert_eq!(cell.info().unwrap().next_sequence, 2);
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, extent(1), 0, BLOCK), [1; BLOCK]);
}

/// cladefs: an extent freed once sealed is written no more, nor opened, though the cell holds it
/// no longer, nor once it opens again, from its deltas or its checkpoint: forks and caches
/// that hold it by its ID would take another's data for it (TASK-79.6). One freed while open,
/// which nothing but its writer saw, may be written again; and one never written, though below
/// the cell's next sequence number, as Fxfs's journal writes the tail it minted ahead in an
/// earlier mount.
#[tokio::test(flavor = "multi_thread")]
async fn freed_extents_are_written_no_more() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.write(extent(2), 0, &[2; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.free(extent(1)).unwrap();
    error_is(cell.write(extent(1), 0, &[3; BLOCK]).await, SpentExtent(extent(1)));
    cell.commit(None).await.unwrap();
    error_is(cell.write(extent(1), 0, &[3; BLOCK]).await, SpentExtent(extent(1)));
    error_is(cell.write(extent(2), 0, &[3; BLOCK]).await, SealedExtent(extent(2)));

    cell.write(extent(4), 0, &[4; BLOCK]).await.unwrap();
    cell.free(extent(4)).unwrap();
    cell.write(extent(4), 0, &[5; BLOCK]).await.unwrap();
    cell.write(extent(5), 0, &[5; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();

    // From its deltas; then from the checkpoint that opening wrote.
    let cell = reopened(cell, &dir);
    error_is(cell.write(extent(1), 0, &[6; BLOCK]).await, SpentExtent(extent(1)));
    cell.free(extent(2)).unwrap();
    cell.commit(None).await.unwrap();
    let cell = reopened(cell, &dir);
    for sequence in [1, 2] {
        error_is(cell.write(extent(sequence), 0, &[6; BLOCK]).await, SpentExtent(extent(sequence)));
    }
    cell.write(extent(3), 0, &[7; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, extent(3), 0, BLOCK), [7; BLOCK]);
    assert_eq!(read(&cell, extent(4), 0, BLOCK), [5; BLOCK]);
}

/// The cell holds at most its capacity, and frees make room.
#[tokio::test(flavor = "multi_thread")]
async fn capacity() {
    let cell = create(MemDir::new(), 64 << 10);
    cell.write(extent(1), 0, &[1; 48 << 10]).await.unwrap();
    let full = cell.write(extent(2), 0, &[2; 32 << 10]).await;
    // Which bound refused it, and its numbers (TASK-79.105).
    assert_eq!(
        format!("{:#}", full.as_ref().unwrap_err()),
        "49152 bytes used, 0 reserved and 32768 written past the capacity's 65536: the extent \
         store is full"
    );
    error_is(full, NoSpace);
    assert_eq!(cell.info().unwrap().available, 16 << 10);
    cell.free(extent(1)).unwrap();
    cell.free(extent(1)).expect("freeing a freed extent failed");
    cell.write(extent(2), 0, &[2; 32 << 10]).await.unwrap();
    // Freeing doesn't make sequence numbers available again.
    assert_eq!(cell.info().unwrap().next_sequence, 3);
}

/// Freed extents' files, and segments whose extents are all freed, are removed once the frees are
/// committed, and cladefs: their segments acknowledged in the bucket (TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn frees_remove_files() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    // Fills the first segment, so that the commit starts another.
    for sequence in 0..16 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.write(extent(16), 0, &pattern(16, 0, 32 << 10)).await.unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    // The first segment, closed, and the next, open.
    // cladefs: segments' files, not the next segment's, placed ahead of it (TASK-384).
    let segments = || {
        let count = |kind| segment_files(&dir, kind).len();
        (count("segments"), count("open-segments"))
    };
    assert_eq!(segments(), (1, 1));
    assert_eq!(dir.list(&of("extents", CELL.id)).unwrap(), vec!["000000000000g"]);
    assert_eq!(dir.list(&of("open-extents", CELL.id)).unwrap(), Vec::<String>::new());

    for sequence in 0..=16 {
        cell.free(extent(sequence)).unwrap();
    }
    // cladefs: not until the frees are committed, and the segment is acknowledged in the bucket,
    // the write-ahead log keeping it until then (TASK-165.9).
    assert_eq!(segments(), (1, 1));
    assert_eq!(dir.list(&of("extents", CELL.id)).unwrap().len(), 1);
    cell.commit(None).await.unwrap();
    assert_eq!(segments(), (1, 1));
    assert_eq!(dir.list(&of("extents", CELL.id)).unwrap().len(), 1);
    error_is(cell.read(extent(16), 0, &mut [0; BLOCK]), NoExtent(extent(16)));
    let closed = closed_one(&cell).await;
    cell.acknowledged(closed.number).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(segments(), (0, 1));
    assert_eq!(dir.list(&of("extents", CELL.id)).unwrap(), Vec::<String>::new());
}

/// cladefs: doc-27 (board 6214): a segment the cell acknowledged in the bucket and then freed most
/// of is sparse, at its data's length as framed, with what the cell holds there live, unless a
/// kept state pins it. One freed to exactly half is not, nor is one freed whole, which the
/// collector deletes: nothing in it to relocate. Its live bytes counted as first asked, then kept
/// on as frees come after.
#[tokio::test(flavor = "multi_thread")]
async fn segments_freed_past_half_are_sparse() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    for sequence in 0..8 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let closed = closed_one(&cell).await;
    let key = bucket_cell::layout::segment(CELL, closed.footer.segment, closed.footer.epoch);
    cell.acknowledged(closed.number).await.unwrap();
    let sparse = |pinned: bool| cell.sparse(|at| pinned && at == key).unwrap();
    assert_eq!(sparse(false), []);

    for sequence in 0..4 {
        cell.free(extent(sequence)).unwrap();
    }
    cell.commit(None).await.unwrap();
    assert_eq!(sparse(false), [], "half live");
    cell.free(extent(4)).unwrap();
    cell.commit(None).await.unwrap();
    let len = 8 * BLOCK as u64;
    let live = 3 * BLOCK as u64;
    assert_eq!(sparse(false), [Sparse { key: key.clone(), live, len }]);
    assert_eq!(sparse(true), [], "pinned");
    for sequence in 5..8 {
        cell.free(extent(sequence)).unwrap();
    }
    cell.commit(None).await.unwrap();
    assert_eq!(sparse(false), [], "freed whole");
}

/// cladefs: doc-27 (board 6214; Codex's review of fa1912379): a relocation is admitted to its
/// cell's disk as a guest's write is: on a disk as full as its allowance lets it be, it is refused
/// for room, the cell unchanged, its extent still in the bucket.
#[tokio::test(flavor = "multi_thread")]
async fn relocations_are_admitted_to_the_disk_as_writes_are() {
    let dir = MemDir::with_capacity(64 << 20);
    let cell = create(dir.clone(), 1 << 20);
    for sequence in 0..4 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let first = closed_one(&cell).await;
    cell.acknowledged(first.number).await.unwrap();
    let Some(Place::Bucket(from)) = cell.place(extent(0)).unwrap() else { panic!("not uploaded") };
    let full = dir.footprint().bytes();
    dir.disk().disk().govern(full, 64 << 20, false);
    let relocating =
        Relocating { extent: extent(0), from: from.clone(), data: pattern(0, 0, BLOCK) };
    let refused = cell.relocate(vec![relocating]).await.unwrap_err();
    assert!(refused.downcast_ref::<local_cell::footprint::DiskFull>().is_some(), "{refused:#}");
    assert_eq!(cell.place(extent(0)).unwrap(), Some(Place::Bucket(from)));
}

/// cladefs: doc-27 (board 6214): a relocation copies the extents its writer read from the cell's
/// segments in the bucket into the open segment, in a commit of its own, which names them there.
/// The guest's extent open stays open, and takes its next write; its free not yet committed is
/// for its own next commit. Those freed since they were read, or read where the cell holds them
/// no more, are passed over; bytes of another length are refused, the cell unchanged. Once
/// acknowledged, the extent is in the bucket where the relocation put it, and the segment it
/// was read from holds less live.
#[tokio::test(flavor = "multi_thread")]
async fn relocations_commit_alone_and_leave_the_guests_extents_open() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    for sequence in 0..4 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let first = closed_one(&cell).await;
    let read_from = bucket_cell::layout::segment(CELL, first.footer.segment, first.footer.epoch);
    cell.acknowledged(first.number).await.unwrap();
    let located = |sequence| match cell.place(extent(sequence)).unwrap() {
        Some(Place::Bucket(at)) => at,
        place => panic!("{:?} at {place:?}", extent(sequence)),
    };
    let at: Vec<Location> = (0..4).map(located).collect();
    cell.free(extent(2)).unwrap();
    cell.commit(None).await.unwrap();
    // The guest's, since its last commit: an extent open, and a free. Its run ends before the
    // open segment's end, where an extent the guest freed open wrote: a relocation yields to a run
    // the guest's next write would go on with (relocations_leave_the_guests_writes_to_seal_in_place).
    cell.write(extent(10), 0, &pattern(10, 0, BLOCK)).await.unwrap();
    cell.write(extent(11), 0, &pattern(11, 0, BLOCK)).await.unwrap();
    cell.free(extent(11)).unwrap();
    cell.free(extent(3)).unwrap();

    let relocating = |sequence: u64, from: &Location, len: usize| Relocating {
        extent: extent(sequence),
        from: from.clone(),
        data: pattern(sequence as u8, 0, len),
    };
    let wrong = cell.relocate(vec![relocating(1, &at[1], 10)]).await;
    assert!(wrong.is_err(), "{wrong:?}");
    let relocated = cell.relocate(vec![
        relocating(0, &at[0], BLOCK),
        relocating(1, &at[0], BLOCK),
        relocating(2, &at[2], BLOCK),
        relocating(3, &at[3], BLOCK),
    ]);
    assert_eq!(relocated.await.unwrap().bytes, BLOCK as u64);
    assert_eq!(
        cell.place(extent(0)).unwrap(),
        Some(Place::Run { cell: CELL.id, number: 1, offset: 2 * BLOCK as u64 })
    );
    let mut read = vec![0; BLOCK];
    cell.read(extent(0), 0, &mut read).unwrap();
    assert_eq!(read, pattern(0, 0, BLOCK));
    // Closed between commits, its segment names the relocation's commit and those before: the
    // extent relocated, and the free committed before it, not the guest's since.
    assert!(cell.close_now().await.unwrap());
    let second = closed_one(&cell).await;
    let sealed = |closed: &Closed<MemFile>| -> BTreeSet<(ExtentId, u64)> {
        let extents = closed.footer.contents.extents.iter();
        extents.map(|&(extent, _, len)| (extent, len)).collect()
    };
    assert_eq!(sealed(&second), BTreeSet::from([(extent(0), BLOCK as u64)]));
    assert_eq!(second.footer.contents.frees, [extent(2)]);
    cell.write(extent(10), BLOCK as u64, &pattern(10, BLOCK, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let third = closed_one(&cell).await;
    assert_eq!(sealed(&third), BTreeSet::from([(extent(10), 2 * BLOCK as u64)]));
    assert_eq!(third.footer.contents.frees, [extent(3)]);

    for closed in [&second, &third] {
        cell.acknowledged(closed.number).await.unwrap();
    }
    let key = bucket_cell::layout::segment(CELL, second.footer.segment, second.footer.epoch);
    assert_eq!(located(0).key, key);
    assert_eq!(located(1), at[1]);
    let live = |at: &str| {
        let sparse = cell.sparse(|_| false).unwrap();
        sparse.into_iter().find(|sparse| sparse.key == at).map(|sparse| sparse.live)
    };
    assert_eq!(live(&read_from), Some(BLOCK as u64), "extent 1 alone held there");
}

/// cladefs: doc-27 (board 6214): an extent the cell holds as it opened from the bucket, in the
/// bucket's layer of its map, relocates as one it acknowledged does: its own layer holds it in the
/// open segment from then on. Placing it there took an entry of its own layer it hadn't, and
/// aborted the process (the verifying review of 6a7f60008).
#[tokio::test(flavor = "multi_thread")]
async fn extents_the_cell_opened_with_relocate() {
    let key = bucket_cell::layout::segment(CELL, 3, 1);
    let location = Location { key, offset: 0, len: BLOCK as u64 };
    let mut state = bucket_cell::State::new(CELL);
    state.extents.insert(extent(5), location.clone());
    state.next_sequence = 6;
    let cell = created_on(MemDir::new(), 1 << 20, state, WRITER);
    let relocating = Relocating { extent: extent(5), from: location, data: pattern(5, 0, BLOCK) };
    assert_eq!(cell.relocate(vec![relocating]).await.unwrap().bytes, BLOCK as u64);
    let place = cell.place(extent(5)).unwrap();
    assert_eq!(place, Some(Place::Run { cell: CELL.id, number: 0, offset: 0 }));
    let mut read = vec![0; BLOCK];
    cell.read(extent(5), 0, &mut read).unwrap();
    assert_eq!(read, pattern(5, 0, BLOCK));
}

/// cladefs: doc-27 (board 6214): a guest's free, not yet committed when its writer's relocation
/// commits, is freed by the guest's next commit, not the relocation's: kept for the uploader once
/// the relocation's commit is acknowledged, as the commit freeing it is not. A free was numbered as
/// the next commit's when made, which the relocation's took (the reviews of 6a7f60008; a resize's
/// did too).
#[tokio::test(flavor = "multi_thread")]
async fn frees_pending_across_a_relocation_are_the_next_commits() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    for sequence in 0..2 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let closed = closed_one(&cell).await;
    cell.acknowledged(closed.number).await.unwrap();
    let Some(Place::Bucket(from)) = cell.place(extent(1)).unwrap() else { panic!("in the bucket") };
    cell.free(extent(0)).unwrap();
    let relocating = Relocating { extent: extent(1), from, data: pattern(1, 0, BLOCK) };
    assert_eq!(cell.relocate(vec![relocating]).await.unwrap().bytes, BLOCK as u64);
    assert!(cell.close_now().await.unwrap());
    let relocated = closed_one(&cell).await;
    cell.acknowledged(relocated.number).await.unwrap();
    assert_eq!(
        cell.usage().unwrap().kept_bytes,
        BLOCK as u64,
        "freed after the relocation's commit"
    );
}

/// cladefs: doc-27 (board 6214): a relocation between the guest's commits leaves what the guest
/// wrote in the open segment to its next commit to seal in place, copying none of it; and while
/// the guest has filled a segment since its last commit, it relocates nothing, closing none of the
/// segments the guest writes in. A relocation's commit closed the segments filled since, and the
/// guest's next commit copied out what it wrote in them (the verifying review of 6a7f60008).
#[tokio::test(flavor = "multi_thread")]
async fn relocations_leave_the_guests_writes_to_seal_in_place() {
    let dir = MemDir::new();
    // Its segments filled in a few dozen writes.
    let options = Options { max_extent: 64 << 10, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    for sequence in 0..2 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let closed = closed_one(&cell).await;
    cell.acknowledged(closed.number).await.unwrap();
    let from = |sequence| match cell.place(extent(sequence)).unwrap() {
        Some(Place::Bucket(at)) => at,
        place => panic!("{place:?}"),
    };
    let relocating = |sequence: u64| Relocating {
        extent: extent(sequence),
        from: from(sequence),
        data: pattern(sequence as u8, 0, BLOCK),
    };
    // An extent the guest has open, its run ended, which an extent it freed open wrote after.
    cell.write(extent(10), 0, &pattern(10, 0, BLOCK)).await.unwrap();
    cell.write(extent(12), 0, &pattern(12, 0, BLOCK)).await.unwrap();
    cell.free(extent(12)).unwrap();
    let copied = local_cell::written().copied;
    assert_eq!(cell.relocate(vec![relocating(0)]).await.unwrap().bytes, BLOCK as u64);
    cell.commit(None).await.unwrap();
    assert_eq!(local_cell::written().copied, copied, "the guest's commit copied");
    assert_eq!(
        cell.place(extent(10)).unwrap(),
        Some(Place::Run { cell: CELL.id, number: 1, offset: 0 })
    );

    // A run the guest writes on, which a copy at its end would split: the relocation yields.
    cell.write(extent(11), 0, &pattern(11, 0, BLOCK)).await.unwrap();
    assert_eq!(cell.relocate(vec![relocating(1)]).await.unwrap().bytes, 0);
    cell.write(extent(11), BLOCK as u64, &pattern(11, BLOCK, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(local_cell::written().copied, copied, "the guest's commit copied");

    // The guest fills the open segment, and writes on in the next.
    let filled = options.segment_limit() / BLOCK as u64 + 1;
    for sequence in 20..20 + filled {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    assert_eq!(cell.relocate(vec![relocating(1)]).await.unwrap().bytes, 0);
    assert_eq!(cell.place(extent(1)).unwrap(), Some(Place::Bucket(from(1))));
    cell.commit(None).await.unwrap();
    assert_eq!(local_cell::written().copied, copied, "the guest's commit copied");
}

/// cladefs: doc-27 (board 6214): a relocation's copy that fails, as a disk's write would, fails the
/// relocation, not the cell: what it copied before is committed, the rest stays in the bucket, and
/// the guest goes on writing and committing (the verifying review of 6a7f60008).
#[tokio::test(flavor = "multi_thread")]
async fn relocations_failing_leave_the_cell_whole() {
    let scenario = fail::FailScenario::setup();
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    for sequence in 0..2 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let closed = closed_one(&cell).await;
    cell.acknowledged(closed.number).await.unwrap();
    let from: Vec<Location> = (0..2)
        .map(|sequence| match cell.place(extent(sequence)).unwrap() {
            Some(Place::Bucket(at)) => at,
            place => panic!("{place:?}"),
        })
        .collect();
    let relocating = (0..2)
        .map(|sequence| Relocating {
            extent: extent(sequence),
            from: from[sequence as usize].clone(),
            data: pattern(sequence as u8, 0, BLOCK),
        })
        .collect();
    fail::cfg("relocate::write", "1*off->return").unwrap();
    let failed = cell.relocate(relocating).await.unwrap_err();
    assert_eq!(
        format!("{failed:#}"),
        "relocating, 4096 bytes relocated: relocating ExtentId { cell: 7, sequence: 1 }: failpoint relocate::write"
    );
    fail::remove("relocate::write");
    assert!(matches!(cell.place(extent(0)).unwrap(), Some(Place::Run { .. })));
    assert_eq!(cell.place(extent(1)).unwrap(), Some(Place::Bucket(from[1].clone())));
    cell.write(extent(10), 0, &pattern(10, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    let mut read = vec![0; BLOCK];
    cell.read(extent(0), 0, &mut read).unwrap();
    assert_eq!(read, pattern(0, 0, BLOCK));
    scenario.teardown();
}

/// cladefs: a commit that fills the open segment closes it, and leaves its framing to a thread of
/// its own: its extents read from its raw file meanwhile, and the flush of the commit waits for
/// its framed file, which holds them from then on, the uploader takes, and its raw file goes for
/// (TASK-41).
#[tokio::test(flavor = "multi_thread")]
async fn segments_close_framed_by_the_commits_flush() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    for sequence in 0..17 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.commit_ordered(None).await.unwrap();
    let count = |kind| segment_files(&dir, kind).len();
    // Closed, and the next open: its raw file stays until the commit is durable.
    assert_eq!(count("open-segments"), 2);
    let reads = || (0..17).map(|sequence| read(&cell, extent(sequence), 0, BLOCK));
    let written: Vec<_> = (0..17).map(|sequence| pattern(sequence as u8, 0, BLOCK)).collect();
    assert_eq!(reads().collect::<Vec<_>>(), written);
    // Not the uploader's until its commit is durable (TASK-165.9).
    assert!(cell.closed(usize::MAX).await.unwrap().is_empty());

    cell.wait_framed().await.unwrap();
    cell.flush().await.unwrap();
    assert_eq!((count("segments"), count("open-segments")), (1, 1));
    assert_eq!(reads().collect::<Vec<_>>(), written);
    let closed = cell.closed(usize::MAX).await.unwrap();
    let [segment] = &closed[..] else { panic!("{} segments closed", closed.len()) };
    let data = extent_frame::data(
        &segment.bytes().unwrap(),
        &bucket_cell::testing::ring([CELL]).of(CELL.id),
    )
    .unwrap();
    assert_eq!(&data[..BLOCK], &pattern(0, 0, BLOCK)[..]);
}

/// The one segment `cell` closed whose commits are durable, which [`LocalCell::closed`] gives.
async fn closed_one(cell: &LocalCell<MemDir>) -> Closed<MemFile> {
    let closed = cell.closed(usize::MAX).await.unwrap();
    let count = closed.len();
    let [closed] = <[_; 1]>::try_from(closed).unwrap_or_else(|_| panic!("{count} segments closed"));
    closed
}

/// cladefs: the uploader takes the segments of the write-ahead log once their commits are durable,
/// whose footers say what the commits wrote, with its lengths, freed, and set as the root; the
/// data of extents freed since stays in the log's files, which the uploader reads, until the
/// segments are acknowledged (TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn the_write_ahead_log_keeps_freed_data_until_acknowledged() {
    use extent_frame::segment::{Location, extent_key};
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert!(cell.closed(usize::MAX).await.unwrap().is_empty());

    for sequence in 2..=16 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    cell.write(extent(17), 0, &pattern(17, 0, 32 << 10)).await.unwrap();
    // Filling its segment, which it closes; not taken until durable.
    cell.commit_ordered(Some(b"root")).await.unwrap();
    assert!(cell.closed(usize::MAX).await.unwrap().is_empty());
    cell.flush().await.unwrap();
    let writing = closed_one(&cell).await;
    let written: Vec<_> =
        writing.footer.contents.extents.iter().map(|&(extent, _, len)| (extent, len)).collect();
    let blocks: Vec<_> = (1..=16).map(|sequence| (extent(sequence), BLOCK as u64)).collect();
    assert_eq!(written, blocks);
    let own = Location {
        key: extent_key(CELL, WRITER.epoch(), writing.footer.segment, extent(17)),
        offset: 0,
        len: 32 << 10,
    };
    assert_eq!(writing.footer.own().collect::<Vec<_>>(), [(extent(17), own)]);
    let (frees, root) = (&writing.footer.contents.frees, &writing.footer.contents.root);
    assert_eq!((frees.clone(), root.clone()), (vec![], Some(b"root".to_vec())));
    assert_eq!(cell.wal_bytes().unwrap(), 16 * BLOCK as u64 + (32 << 10));

    for sequence in 1..=17 {
        cell.free(extent(sequence)).unwrap();
    }
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let freeing = closed_one(&cell).await;
    assert_eq!(freeing.footer.contents.frees, (1..=17).map(extent).collect::<Vec<_>>());
    // Freed: the cell's users read them not, the log holds them, which the uploader reads.
    error_is(cell.read(extent(17), 0, &mut [0; BLOCK]), NoExtent(extent(17)));
    let [(_, own_bytes)] = &writing.own_bytes().unwrap()[..] else { panic!("no extent's own") };
    assert_eq!(
        extent_frame::data(own_bytes, &bucket_cell::testing::ring([CELL]).of(CELL.id)).unwrap(),
        pattern(17, 0, 32 << 10)
    );
    let data = extent_frame::data(
        &writing.bytes().unwrap(),
        &bucket_cell::testing::ring([CELL]).of(CELL.id),
    )
    .unwrap();
    assert_eq!(&data[2 * BLOCK..3 * BLOCK], &pattern(3, 0, BLOCK)[..]);
    cell.commit(None).await.unwrap();
    assert_eq!(dir.list(&of("extents", CELL.id)).unwrap().len(), 1);
    let segments = dir.list(&of("segments", CELL.id)).unwrap().len();

    // Acknowledged, they go.
    for closed in [writing, freeing] {
        cell.acknowledged(closed.number).await.unwrap();
    }
    assert_eq!(cell.wal_bytes().unwrap(), 0);
    cell.commit(None).await.unwrap();
    assert_eq!(dir.list(&of("extents", CELL.id)).unwrap(), Vec::<String>::new());
    assert!(dir.list(&of("segments", CELL.id)).unwrap().len() < segments);
}

/// cladefs: where in the bucket an extent of `len` bytes is: at `offset` in the object at `key`.
fn bucket_location(key: &str, offset: u64, len: u64) -> Location {
    Location { key: key.to_owned(), offset, len }
}

/// cladefs: a cell holds the extents the bucket holds of its cell as of its opening: recovered
/// there, read there, at their locations there, which a read's error names, and freed like others
/// (TASK-165.8, TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn extents_the_bucket_holds_are_held_without_their_data() {
    let dir = MemDir::new();
    let location = bucket_location("segments/0000000000007/0000000000003", 8192, 3 * BLOCK as u64);
    let other = bucket_location("extents/0000000000008/1.2.9", 0, BLOCK as u64);
    let mut state = bucket_cell::State::new(CELL);
    state.extents.insert(extent(5), location.clone());
    state.extents.insert(ExtentId::new(CELL.id + 1, 9), other);
    state.next_sequence = 6;
    let check = async |cell: &LocalCell<MemDir>| {
        assert_eq!(cell.extent(extent(5)).unwrap(), Some((3 * BLOCK as u64, false)));
        let in_bucket = BucketData { extent: extent(5), location: location.clone() };
        error_is(cell.read(extent(5), 0, &mut [0; BLOCK]), in_bucket);
        assert_eq!(cell.place(extent(5)).unwrap(), Some(Place::Bucket(location.clone())));
        assert_eq!(cell.info().unwrap().used, 4 * BLOCK as u64);
        assert_eq!(cell.info().unwrap().next_sequence, 6);
        // Sealed.
        error_is(cell.write(extent(5), 0, &[0; BLOCK]).await, SealedExtent(extent(5)));
    };
    let cell = created_on(dir.clone(), 1 << 20, state.clone(), WRITER);
    check(&cell).await;
    drop(cell);
    let cell = opened_on(dir, state, WRITER);
    check(&cell).await;
    cell.free(extent(5)).unwrap();
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    assert_eq!(closed_one(&cell).await.footer.contents.frees, [extent(5)]);
    error_is(cell.read(extent(5), 0, &mut [0; BLOCK]), NoExtent(extent(5)));
}

/// cladefs: commits append their records to the open segment's log, its alone, and the cell opened
/// again holds them all (TASK-165.9). The logs of the segments the commits closed go once their
/// framed files are durable, after the commits return (framings run off them, TASK-165.9, and
/// make their framed files durable on a thread of the cell's, TASK-310): counted once the cell
/// settled. Here none is made durable until the commits are made, as on a busy disk; counted at
/// once, as the test did, the closed segments' logs were still there, (12, 0), and on a CI
/// runner two had gone together, two spares kept, (1, 2); so too before TASK-310 with framings
/// as slow (TASK-370).
#[tokio::test(flavor = "multi_thread")]
async fn commits_reopen_from_their_logs() {
    use std::sync::Mutex;
    use std::sync::mpsc::channel;
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    let (go, gone) = channel::<()>();
    let gone = Mutex::new(gone);
    let held = fail::FailGuard::with_callback("segments::making_durable", move || {
        let _ = gone.lock().unwrap().recv();
    })
    .unwrap();
    for sequence in 0..200 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        if sequence % 3 == 0 {
            cell.free(extent(sequence)).unwrap();
        }
        cell.commit(Some(format!("root {sequence}").as_bytes())).await.unwrap();
    }
    drop((go, held));
    settled(&cell).await;
    // The open segment's log, and under spares' names the logs of segments gone kept as spares,
    // two at most (TASK-301); logs are created lazily on first flush (board 11709).
    let logs = |cell: &LocalCell<MemDir>| {
        let _ = cell;
        let names = dir.list(&of("logs", CELL.id)).unwrap();
        let spares = names.iter().filter(|name| name.starts_with("spare-")).count();
        (segment_files(&dir, "logs").len(), spares)
    };
    assert_eq!(logs(&cell), (1, 2));
    drop(cell);
    let cell = LocalCell::open(dir.clone(), OPTIONS).unwrap();
    // Reopening creates no eager log for the new open segment until its first commit flushes; both
    // spares stay ready for reuse (board 11709).
    assert_eq!(logs(&cell), (0, 2));
    assert_eq!(cell.root().unwrap(), b"root 199");
    assert_eq!(cell.last_commit().unwrap(), 200);
    let held: Vec<u64> =
        cell.extents().unwrap().iter().map(|(extent, _)| extent.sequence).collect();
    assert_eq!(held, (0..200).filter(|sequence| sequence % 3 != 0).collect::<Vec<_>>());
    for sequence in held {
        assert_eq!(read(&cell, extent(sequence), 0, BLOCK), pattern(sequence as u8, 0, BLOCK));
    }
}

/// What a cell holds as of its last commit, as its users see it: its epoch, root and next
/// sequence number, its extents with their lengths and where they are, and what it spent.
type Held = (u64, Vec<u8>, u64, Vec<(ExtentId, u64, Place)>, extent_frame::spent::Spent);

/// What `cell` holds ([`Held`]), its open segment closed by a commit.
async fn closed_state<D: Dir>(cell: &LocalCell<D>) -> Held {
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    settled(cell).await;
    let next_sequence = cell.info().unwrap().next_sequence;
    let (epoch, root) = (cell.last_commit().unwrap(), cell.root().unwrap());
    (epoch, root, next_sequence, cell.places().unwrap(), cell.spent().unwrap())
}

/// cladefs: `cell` once the segments its commits closed are framed, and the raw files and logs of
/// those whose commits are durable are gone: as a flush after their framings leaves it, none of
/// which a flush waits for (TASK-165.9).
async fn settled<D: Dir>(cell: &LocalCell<D>) {
    cell.wait_framed().await.unwrap();
    cell.flush().await.unwrap();
}

/// What `cell`, of `dir`, holds once opened again, its write-ahead log's footers replayed
/// (TASK-165.9): what [`closed_state`] said.
fn reopened_state(cell: LocalCell<MemDir>, dir: &MemDir) -> (Held, LocalCell<MemDir>) {
    let cell = reopened(cell, dir);
    let next_sequence = cell.info().unwrap().next_sequence;
    let (epoch, root) = (cell.last_commit().unwrap(), cell.root().unwrap());
    let held = (epoch, root, next_sequence, cell.places().unwrap(), cell.spent().unwrap());
    (held, cell)
}

/// cladefs: an extent freed while open, which no commit held, is not spent: the cell writes it
/// again, and so does the cell opened again, its write-ahead log's footers spending those freed
/// once sealed alone, as the cell does (TASK-160.15, TASK-165.9). One sealed, and freed by a
/// later commit of the same segment, is spent, though no footer names it held.
#[tokio::test(flavor = "multi_thread")]
async fn reopening_spends_what_the_cell_spends() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    for (sequence, len) in [(1, BLOCK), (2, BLOCK), (3, 40 << 10)] {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, len)).await.unwrap();
    }
    // Freed open, in segments and in a raw file of its own.
    cell.free(extent(2)).unwrap();
    cell.free(extent(3)).unwrap();
    cell.commit(None).await.unwrap();
    // Sealed, then freed.
    cell.free(extent(1)).unwrap();
    let state = closed_state(&cell).await;
    assert_eq!(state.4.runs().collect::<Vec<_>>(), [(1, 2)]);
    let (reopened, cell) = reopened_state(cell, &dir);
    // Its next sequence past what it holds, none: those freed, its next writer's floor is past.
    let (epoch, root, _, places, spent) = state;
    assert_eq!(reopened, (epoch, root, 0, places, spent));
    for sequence in [2, 3] {
        cell.write(extent(sequence), 0, &pattern(9, 0, BLOCK)).await.unwrap();
    }
}

/// A commit seals each extent in one run of the open segment, however it was written, or, sealed
/// large, in a file of its own.
#[tokio::test(flavor = "multi_thread")]
async fn sealed_extents_are_one_run_each() {
    let open_cell = create(MemDir::new(), 1 << 20);
    // Interleaved, and rewritten, and with a hole.
    open_cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    open_cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
    open_cell.write(extent(1), BLOCK as u64, &pattern(1, BLOCK, BLOCK)).await.unwrap();
    open_cell.write(extent(1), 0, &pattern(3, 0, BLOCK)).await.unwrap();
    open_cell.write(extent(3), 2 * BLOCK as u64, &pattern(4, 0, BLOCK)).await.unwrap();
    // Small at first, large at its seal.
    open_cell.write(extent(4), 0, &pattern(5, 0, BLOCK)).await.unwrap();
    open_cell.write(extent(4), BLOCK as u64, &pattern(5, BLOCK, 32 << 10)).await.unwrap();
    assert_eq!(open_cell.places().unwrap()[0].2, Place::Pieces);
    open_cell.commit(None).await.unwrap();

    let places = open_cell.places().unwrap();
    for (extent, len, place) in &places[..3] {
        let Place::Run { cell, offset, .. } = place else { panic!("{extent:?} is {place:?}") };
        assert_eq!((*cell, offset % BLOCK as u64), (CELL.id, 0), "{extent:?} at {offset}");
        assert!(*len > 0);
    }
    assert_eq!(places[3], (extent(4), (32 << 10) + BLOCK as u64, Place::Own));
    assert_eq!(
        read(&open_cell, extent(1), 0, 2 * BLOCK),
        [pattern(3, 0, BLOCK), pattern(1, BLOCK, BLOCK)].concat()
    );
    assert_eq!(read(&open_cell, extent(2), 0, BLOCK), pattern(2, 0, BLOCK));
    assert_eq!(
        read(&open_cell, extent(3), 0, 3 * BLOCK),
        [vec![0; 2 * BLOCK], pattern(4, 0, BLOCK)].concat()
    );
    assert_eq!(read(&open_cell, extent(4), BLOCK as u64, 32 << 10), pattern(5, BLOCK, 32 << 10));
    let check = |open_cell: &LocalCell<MemDir>| assert_eq!(open_cell.places().unwrap(), places);
    check(&open_cell);
    let dir = open_cell.dir().clone();
    drop(open_cell);
    check(&LocalCell::open(dir, OPTIONS).unwrap());
}

/// Segments close when full, when old, when asked to, and at a version, their footers after their
/// data; and the cell's state is the replay of its write-ahead log's footers, which the cell
/// opened again holds (TASK-165.9), and which its audit checks at each closing.
#[tokio::test(flavor = "multi_thread")]
async fn closed_segments_hold_the_cells_history() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    let segments = || {
        dir.list(&of("segments", CELL.id)).unwrap().len()
            + segment_files(&dir, "open-segments").len()
    };
    cell.write(extent(0), 0, &pattern(0, 0, BLOCK)).await.unwrap();
    cell.commit(Some(b"one")).await.unwrap();
    assert_eq!(segments(), 1);
    // Asked to.
    let state = closed_state(&cell).await;
    assert_eq!(segments(), 2);
    let (reopened, cell) = reopened_state(cell, &dir);
    assert_eq!(reopened, state);

    // Full: 64 KiB of extents, freed and kept.
    for sequence in 1..=40 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        if sequence % 3 == 0 {
            cell.free(extent(sequence)).unwrap();
        }
        cell.commit(Some(format!("root {sequence}").as_bytes())).await.unwrap();
    }
    assert!(segments() > 2, "no segment filled");
    cell.free(extent(0)).unwrap();
    cell.commit(None).await.unwrap();
    let state = closed_state(&cell).await;
    let (reopened, cell) = reopened_state(cell, &dir);
    assert_eq!(reopened, state);
    // A run's data is in its segment, framed, where its footer says.
    let (_, _, place) = state.3.iter().find(|(id, ..)| *id == extent(40)).unwrap();
    let Place::Run { number, offset, .. } = *place else { panic!("{place:?}") };
    let file = dir.open(&format!("{}/{}", of("segments", CELL.id), Padded(number)), true).unwrap();
    let mut bytes = vec![0; local_cell::File::size(&file).unwrap() as usize];
    local_cell::File::read_at(&file, &mut bytes, 0).unwrap();
    let data = extent_frame::data(&bytes, &bucket_cell::testing::ring([CELL]).of(CELL.id)).unwrap();
    assert_eq!(data[offset as usize..][..BLOCK], pattern(40, 0, BLOCK));

    // Old.
    drop(cell);
    let options = Options { segment_age: std::time::Duration::ZERO, ..OPTIONS };
    let cell = LocalCell::open(dir.clone(), options).unwrap();
    let before = segments();
    cell.write(extent(50), 0, &pattern(50, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    assert_eq!(segments(), before + 1);

    // At a version.
    cell.write(extent(51), 0, &pattern(51, 0, BLOCK)).await.unwrap();
    cell.commit_version(NewVersion { id: 1, keep_last: None }, false).await.unwrap();
    let places = cell.places().unwrap();
    let (reopened, _) = reopened_state(cell, &dir);
    assert_eq!(reopened.3, places);
}

/// A cell with closed segments, and one not, reopens in a directory of the host's, whose
/// extents' files bypass its page cache where its file system allows: their trailers' and
/// footers' reads are aligned as such reads must be (`O_DIRECT` on Linux).
#[tokio::test(flavor = "multi_thread")]
async fn closed_segments_reopen_on_the_host() {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let open = || host_fs::may_block(|| HostDir::new(temp.path()).unwrap());
    let cell = create(open(), 64 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, 3 * BLOCK)).await.unwrap();
    let state = closed_state(&cell).await;
    cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
    cell.commit(Some(b"root")).await.unwrap();
    drop(cell);
    let cell = host_fs::may_block(|| LocalCell::open(open(), OPTIONS).unwrap());
    assert_eq!(read(&cell, extent(1), 0, 3 * BLOCK), pattern(1, 0, 3 * BLOCK));
    assert_eq!(read(&cell, extent(2), 0, BLOCK), pattern(2, 0, BLOCK));
    assert!(cell.last_commit().unwrap() > state.0);
    let state = closed_state(&cell).await;
    drop(cell);
    let cell = host_fs::may_block(|| LocalCell::open(open(), OPTIONS).unwrap());
    assert_eq!(closed_state(&cell).await.3, state.3);
}

/// Closed segments and sealed extents are stored compressed, but for parts that do not compress,
/// and read as written: while open, raw, once closed, framed, and once reopened.
#[tokio::test(flavor = "multi_thread")]
async fn closed_segments_and_sealed_extents_are_compressed() {
    let dir = MemDir::new();
    let options = Options { segment_size: 4 << 20, large_extent: 8 << 20, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    let mib = 1 << 20;
    // In segments, compressible and not, across parts; and on their own, compressible and not.
    let contents = [
        (extent(1), pattern(1, 0, 3 * mib / 2)),
        (extent(2), noise(2, mib + 8 * BLOCK)),
        (extent(3), pattern(3, 0, 12 * BLOCK)),
        (extent(4), pattern(4, 0, 9 * mib)),
        (extent(5), [noise(5, 8 * mib), pattern(5, 0, mib)].concat()),
    ];
    for (extent, data) in &contents {
        cell.write(*extent, 0, data).await.unwrap();
    }
    let reads = |cell: &LocalCell<MemDir>| {
        for (extent, data) in &contents {
            assert!(read(cell, *extent, 0, data.len()) == *data, "{extent:?}");
            // A range across parts.
            let (at, len) = (mib - 2 * BLOCK, 5 * BLOCK);
            if data.len() >= at + len {
                let range = read(cell, *extent, at as u64, len);
                assert!(range == data[at..at + len], "{extent:?} at {at}");
            }
        }
    };
    reads(&cell);
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    reads(&cell);

    // Framed on a thread of its own, made durable on another: its framed file's size counts once
    // its framing ends, which the commit waits not for (TASK-310).
    cell.wait_framed().await.unwrap();
    let usage = cell.usage().unwrap();
    let in_segments: u64 = contents[..3].iter().map(|(_, data)| data.len() as u64).sum();
    assert_eq!((usage.segment_bytes, usage.own_bytes), (in_segments, 18 * mib as u64));
    // The noise takes what it is, the patterns next to nothing.
    let noise_bytes = (mib + 8 * BLOCK + 8 * mib) as u64;
    assert!(usage.disk_bytes >= noise_bytes, "{usage:?}");
    assert!(usage.disk_bytes < noise_bytes + 3 * mib as u64 / 10, "{usage:?}");
    drop(cell);
    let cell = LocalCell::open(dir, options).unwrap();
    reads(&cell);
    assert_eq!(cell.usage().unwrap().disk_bytes, usage.disk_bytes);
}

/// A cell full of data that compresses takes a fraction of it on disk.
#[tokio::test(flavor = "multi_thread")]
async fn compressible_data_takes_less_disk() {
    let cell = create(MemDir::new(), 64 << 20);
    for sequence in 0..64 {
        let len = if sequence % 8 == 7 { 64 << 10 } else { 4 * BLOCK };
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, len)).await.unwrap();
        cell.commit(None).await.unwrap();
    }
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    let usage = cell.usage().unwrap();
    let held = usage.segment_bytes + usage.own_bytes;
    assert_eq!(held, 56 * 4 * BLOCK as u64 + 8 * (64 << 10));
    assert!(usage.disk_bytes * 4 < held, "{usage:?}");
}

/// cladefs: a segment a commit closes is that commit's record once framed, its framing having made
/// its framed file, which holds the commit's data, durable: a crash after leaves the cell as of
/// that commit, though no flush made it durable, the framed file read, its raw file gone once
/// opened (TASK-165.9). Before, a crash closes it again from its log (`wal`'s tests). The next
/// segment's raw file and log, made as it closed, are durable with the next flush, which syncs
/// their directories, not with the framing (TASK-310): a crash before may keep them or not, and
/// the opening opens the segment after the last it finds.
#[tokio::test(flavor = "multi_thread")]
async fn segments_closed_by_commits_are_their_records_once_framed() {
    for seed in 0..16 {
        let dir = MemDir::serial();
        let cell = create(dir.clone(), 64 << 20);
        cell.write(extent(1), 0, &pattern(1, 0, 3 * BLOCK)).await.unwrap();
        cell.commit(Some(b"one")).await.unwrap();
        cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
        cell.close_segment().unwrap();
        cell.commit_ordered(Some(b"two")).await.unwrap();
        let crashed = dir.crash(seed);
        drop(cell);
        // The opening's open segment is past every one the crash kept a file of: segment 1's,
        // opened, and cladefs: segment 2's, placed ahead of it (TASK-384).
        let kept = ["segments", "open-segments", "logs"].iter().flat_map(|kind| {
            let names = crashed.list(&of(kind, CELL.id)).unwrap();
            names.into_iter().filter_map(|name| base32_id::parse_padded(&name))
        });
        let open = Padded(kept.max().unwrap() + 1).to_string();
        let cell = LocalCell::open(crashed.clone(), OPTIONS).unwrap();
        assert_eq!(cell.root().unwrap(), b"two", "seed {seed}");
        assert_eq!(read(&cell, extent(1), 0, 3 * BLOCK), pattern(1, 0, 3 * BLOCK));
        assert_eq!(read(&cell, extent(2), 0, BLOCK), pattern(2, 0, BLOCK));
        assert_eq!(crashed.list(&of("open-segments", CELL.id)).unwrap(), [open], "seed {seed}");
    }
}

/// cladefs: a segment closed between commits, the last of whose commits is not durable, is closed
/// again with no footer by the cell's next opening: which, crashing as it does, leaves its data
/// whole, for the opening after (TASK-79.33).
#[tokio::test(flavor = "multi_thread")]
async fn segments_closed_between_commits_survive_crashes_closing_them_again() {
    for seed in 0..64 {
        let dir = MemDir::serial();
        let cell = create(dir.clone(), 1 << 20);
        cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
        cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        assert!(cell.close_now().await.unwrap());
        let crashed = dir.crash(seed);
        drop(cell);
        // An opening stopped after each of its first operations, as by a crash, then another.
        for operations in 0.. {
            let opening = crashed.crash(seed);
            opening.fail_after(Some(operations));
            let opened = LocalCell::open(opening.clone(), OPTIONS).map(drop);
            opening.fail_after(None);
            let reopened = LocalCell::open(opening.crash(seed + operations), OPTIONS);
            let reopened = reopened.unwrap_or_else(|error| {
                panic!("seed {seed}, stopped after {operations} operations: {error:#}")
            });
            assert_eq!(read(&reopened, extent(1), 0, BLOCK), pattern(1, 0, BLOCK));
            if opened.is_ok() {
                break;
            }
        }
    }
}

/// cladefs: a segment closed between commits, the commits its footer names durable, whose framing a
/// crash stopped before its framed file was synced, is framed again by the cell's next opening,
/// from its raw file: its framed file may be whole in its trailer and footer, and torn in its data
/// (TASK-79.33).
#[tokio::test(flavor = "multi_thread")]
async fn segments_closed_between_commits_survive_crashes_framing_them() {
    for seed in 0..64 {
        let dir = MemDir::serial();
        let cell = create(dir.clone(), 1 << 20);
        for sequence in 0..4 {
            cell.write(extent(sequence), 0, &noise(sequence, BLOCK)).await.unwrap();
        }
        cell.commit(None).await.unwrap();
        // The next segment's raw file made, the framed file written, then its sync fails.
        dir.fail_after(Some(3));
        assert!(cell.close_now().await.is_err(), "seed {seed}: the closing went on");
        dir.fail_after(None);
        let crashed = dir.crash(seed);
        drop(cell);
        let cell = LocalCell::open(crashed, OPTIONS).unwrap();
        for sequence in 0..4 {
            let data = read(&cell, extent(sequence), 0, BLOCK);
            assert!(data == noise(sequence, BLOCK), "seed {seed}: extent {sequence} differs");
        }
    }
}

/// cladefs: so too if the cell is opened again in the same process, its cell failed as the
/// framed file's sync did: the opening makes the framed file durable before its raw file goes,
/// whatever a crash after it then keeps (TASK-79.33).
#[tokio::test(flavor = "multi_thread")]
async fn segments_closed_between_commits_survive_failures_framing_them() {
    for seed in 0..64 {
        let dir = MemDir::serial();
        let cell = create(dir.clone(), 1 << 20);
        for sequence in 0..4 {
            cell.write(extent(sequence), 0, &noise(sequence, BLOCK)).await.unwrap();
        }
        cell.commit(None).await.unwrap();
        dir.fail_after(Some(3));
        assert!(cell.close_now().await.is_err(), "seed {seed}: the closing went on");
        dir.fail_after(None);
        drop(cell);
        drop(LocalCell::open(dir.clone(), OPTIONS).unwrap());
        let cell = LocalCell::open(dir.crash(seed), OPTIONS).unwrap();
        for sequence in 0..4 {
            let data = read(&cell, extent(sequence), 0, BLOCK);
            assert!(data == noise(sequence, BLOCK), "seed {seed}: extent {sequence} differs");
        }
    }
}

/// cladefs: a segment closed between commits keeps its raw file and log until a commit after it is
/// durable, with the commits its footer names, though its framed file, durable once framed, holds
/// them (TASK-92.16, TASK-165.9): a crash then leaves the cell as of its last commit, its
/// framed file read, whose footer names it.
#[tokio::test(flavor = "multi_thread")]
async fn segments_closed_between_commits_keep_their_raw_files_until_a_commit_after_is_durable() {
    use extent_frame::segment::{Footer, tag};
    let path = |kind| format!("{}/{}", of(kind, CELL.id), Padded(0));
    for seed in 0..16 {
        let dir = MemDir::serial();
        let cell = create(dir.clone(), 1 << 20);
        cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
        cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        assert!(cell.close_now().await.unwrap());
        let crashed = dir.crash(seed);
        assert!(dir.open(&path("open-segments"), true).is_ok(), "seed {seed}: the raw file went");
        assert!(dir.open(&path("logs"), false).is_ok(), "seed {seed}: the log went");
        cell.commit(None).await.unwrap();
        assert!(dir.open(&path("open-segments"), true).is_err(), "seed {seed}: the raw file stays");
        assert!(dir.open(&path("logs"), false).is_err(), "seed {seed}: the log stays");
        drop(cell);
        let cell = LocalCell::open(crashed.clone(), OPTIONS).unwrap();
        assert_eq!(cell.last_commit().unwrap(), 2, "seed {seed}");
        assert_eq!(read(&cell, extent(2), 0, BLOCK), pattern(2, 0, BLOCK));
        let framed = local_cell::read_whole(&crashed.open(&path("segments"), true).unwrap());
        let footer =
            extent_frame::footer(&framed.unwrap(), &extent_frame::NoKeys).unwrap().to_vec();
        let footer = Footer::decode(&footer, bucket_cell::layout::FORMAT).unwrap();
        assert_eq!(footer.contents.commits, [tag(0, 2)], "seed {seed}");
    }
}

/// Reads find a part stored compressed packed, to be unpacked from its stored bytes, then, once
/// read, unpacked; a part stored raw, in place.
#[tokio::test(flavor = "multi_thread")]
async fn locating_parts_stored_compressed() {
    let options = Options { segment_size: 4 << 20, large_extent: 8 << 20, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(MemDir::new(), description, options).unwrap();
    cell.write(extent(1), 0, &pattern(1, 0, 16 * BLOCK)).await.unwrap();
    cell.write(extent(2), 0, &noise(2, 16 * BLOCK)).await.unwrap();
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    let kinds = |extent| -> Vec<&'static str> {
        (cell.locate(extent, 4 * BLOCK as u64, 2 * BLOCK as u64).unwrap().iter())
            .map(|part| match part {
                Located::Data { .. } => "data",
                Located::Packed { .. } => "packed",
                Located::Unpacked { .. } => "unpacked",
                Located::Zeros(_) => "zeros",
            })
            .collect()
    };
    // Both in the segment's first part, which compresses.
    assert_eq!(kinds(extent(1)), ["packed"]);
    let [Located::Packed { part, skip, len }] =
        &cell.locate(extent(1), 0, BLOCK as u64).unwrap()[..]
    else {
        panic!("not packed");
    };
    assert_eq!((*skip, *len, part.read_range().start % BLOCK as u64), (0, BLOCK as u64, 0));
    // Its range read, as a reader bypassing the cell reads it, unpacks to its data.
    let range = part.read_range();
    let mut bytes = local_cell::Aligned::new((range.end - range.start) as usize);
    part.file.read_at(bytes.as_mut_slice(), range.start).unwrap();
    assert_eq!(cell.unpack(part, bytes).unwrap()[..16 * BLOCK], pattern(1, 0, 16 * BLOCK));
    let unpacked = cell.read_packed(part).unwrap();
    assert_eq!(unpacked[..16 * BLOCK], pattern(1, 0, 16 * BLOCK));
    assert_eq!(kinds(extent(1)), ["unpacked"]);
    assert_eq!(kinds(extent(2)), ["unpacked"]);
    assert_eq!(read(&cell, extent(2), 0, 16 * BLOCK), noise(2, 16 * BLOCK));
}

/// cladefs: a resize is a commit of its own, which seals nothing, a segment's footer saying it, as
/// object storage keeps the cell's capacity: the last resize of its commits', or none if none
/// resized it (TASK-165.3, TASK-165.9). The extents open stay open.
#[tokio::test(flavor = "multi_thread")]
async fn resizes_are_commits_of_their_own() {
    let cell = create(MemDir::new(), 1 << 20);
    let capacity = async |cell: &LocalCell<MemDir>| {
        cell.close_now().await.unwrap();
        cell.flush().await.unwrap();
        closed_one(cell).await.footer.contents.capacity
    };
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    let epoch = cell.last_commit().unwrap();
    cell.set_capacity(2 << 20).await.unwrap();
    cell.set_capacity(3 << 20).await.unwrap();
    assert_eq!(cell.last_commit().unwrap(), epoch + 2);
    // Open still: written on, sealed by a commit.
    assert_eq!(cell.extent(extent(1)).unwrap(), Some((BLOCK as u64, true)));
    cell.write(extent(1), BLOCK as u64, &pattern(1, BLOCK, BLOCK)).await.unwrap();
    assert_eq!(capacity(&cell).await, Some(3 << 20));
    cell.commit(None).await.unwrap();
    assert_eq!(capacity(&cell).await, None);
    cell.set_capacity(4 << 20).await.unwrap();
    assert_eq!(capacity(&cell).await, Some(4 << 20));
    assert_eq!(cell.info().unwrap().capacity, 4 << 20);
    assert_eq!(read(&cell, extent(1), 0, 2 * BLOCK), pattern(1, 0, 2 * BLOCK));
}

/// cladefs: opened by a writer, the cell closes its segments for the writer, as object storage
/// keeps them: numbered on from where the writer's chain is, each following the one before, its
/// commits tagged with the writer's epoch, its extents of their own at the writer's keys; and
/// keeps each, once its commits are durable, for the uploader, until it is uploaded. Opened as
/// another cell's, it refuses.
#[tokio::test(flavor = "multi_thread")]
async fn segments_close_for_the_writer_of_their_chain() {
    use extent_frame::segment::{Footer, Location, extent_key, tag};
    let dir = MemDir::new();
    let state = bucket_cell::State::new(CELL);
    let cell = created_on(dir.clone(), 1 << 20, state, Chain::new(CELL, 3, 5, 100));
    drop(cell);
    let other = bucket_cell::State::new(bucket_cell::layout::test_cell(CELL.id + 1));
    let opening = Opening::new(bucket_cell::layout::test_cell(CELL.id + 1));
    let refused =
        LocalCell::recover(dir.clone()).unwrap().open(OPTIONS, Opening { state: other, ..opening });
    assert_eq!(refused.err().unwrap().to_string(), "cell 1/7 opened as 1/8");
    let cell = opened_on(dir.clone(), bucket_cell::State::new(CELL), Chain::new(CELL, 3, 5, 100));
    // Nothing committed since the last closing, nothing to close.
    assert!(!cell.close_now().await.unwrap());

    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.write(extent(2), 0, &pattern(2, 0, 32 << 10)).await.unwrap();
    cell.commit_ordered(Some(b"one")).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    // Not until its commit is durable.
    assert!(cell.closed(usize::MAX).await.unwrap().is_empty());
    cell.flush().await.unwrap();
    let closed = cell.closed(usize::MAX).await.unwrap();
    let [first] = &closed[..] else { panic!("{} segments closed", closed.len()) };
    let own = Location { key: extent_key(CELL, 3, 5, extent(2)), offset: 0, len: 32 << 10 };
    assert_eq!(
        first.footer,
        Footer {
            cell: CELL,
            segment: 5,
            epoch: 3,
            contents: Contents {
                commits: vec![tag(3, 1)],
                extents: vec![(extent(1), 0, BLOCK as u64)],
                own: vec![(2, 32 << 10)],
                frees: vec![],
                root: Some(b"one".to_vec()),
                next_sequence: 100,
                // Its first commit sets its capacity, which the bucket said none of.
                capacity: Some(1 << 20),
                continued: None,
                versions: vec![],
            },
        }
    );
    assert_eq!(first.footer.own().collect::<Vec<_>>(), [(extent(2), own.clone())]);
    // Its file is the object, footer and all; its extent of its own's, the extent's object.
    let bytes = first.bytes().unwrap();
    let footer = Footer::decode(
        extent_frame::footer(&bytes, &extent_frame::NoKeys).unwrap(),
        bucket_cell::layout::FORMAT,
    )
    .unwrap();
    assert_eq!(footer, first.footer);
    assert_eq!(
        extent_frame::data(&bytes, &bucket_cell::testing::ring([CELL]).of(CELL.id)).unwrap()
            [..BLOCK],
        pattern(1, 0, BLOCK)
    );
    let [(key, own_bytes)] = &first.own_bytes().unwrap()[..] else { panic!("no extent's own") };
    assert_eq!(
        (
            key,
            extent_frame::data(own_bytes, &bucket_cell::testing::ring([CELL]).of(CELL.id)).unwrap()
        ),
        (&own.key, pattern(2, 0, 32 << 10))
    );

    // The next follows it; a version's names the version.
    cell.write(extent(3), 0, &pattern(3, 0, BLOCK)).await.unwrap();
    let version = NewVersion { id: 1, keep_last: None };
    cell.commit_version(version, false).await.unwrap();
    let closed = cell.closed(usize::MAX).await.unwrap();
    let [second] = &closed[..] else { panic!("{} segments closed", closed.len()) };
    assert_eq!((second.footer.segment, second.footer.epoch), (6, 3));
    assert_eq!(
        (second.footer.contents.commits.clone(), second.footer.contents.versions.clone()),
        (vec![tag(3, 2)], vec![version])
    );

    // Freed, its extents' data goes once released, but the segment stays until uploaded.
    cell.free(extent(1)).unwrap();
    cell.commit(None).await.unwrap();
    cell.commit(None).await.unwrap();
    let held = cell.usage().unwrap().segments;
    cell.acknowledged(first.number).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.usage().unwrap().segments, held - 1);
}

/// cladefs: a segment closed between commits holds what the commits made, as of the last: not
/// what was written or freed since, which the next segment holds.
#[tokio::test(flavor = "multi_thread")]
async fn segments_closed_between_commits_hold_the_last_commits_state() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    // Since: one freed, another written.
    cell.free(extent(1)).unwrap();
    cell.write(extent(3), 0, &pattern(3, 0, BLOCK)).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let closed = cell.closed(usize::MAX).await.unwrap();
    let [first] = &closed[..] else { panic!("{} segments closed", closed.len()) };
    let listed: Vec<_> = first.footer.contents.extents.iter().map(|(extent, ..)| *extent).collect();
    assert_eq!((listed, first.footer.contents.frees.clone()), (vec![extent(1), extent(2)], vec![]));
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let closed = cell.closed(usize::MAX).await.unwrap();
    let [second] = &closed[..] else { panic!("{} segments closed", closed.len()) };
    let listed: Vec<_> =
        second.footer.contents.extents.iter().map(|(extent, ..)| *extent).collect();
    assert_eq!((listed, second.footer.contents.frees.clone()), (vec![extent(3)], vec![extent(1)]));
    assert_eq!(read(&cell, extent(3), 0, BLOCK), pattern(3, 0, BLOCK));
}

/// cladefs (TASK-79.19): the uploader takes so many of the segments closed at once, oldest first;
/// the rest wait in the cell for its next take.
#[tokio::test(flavor = "multi_thread")]
async fn closed_segments_are_taken_so_many_at_once() {
    let cell = create(MemDir::new(), 1 << 20);
    for sequence in 1..=3 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.close_segment().unwrap();
        cell.commit(None).await.unwrap();
    }
    let taken = async |limit| -> Vec<u64> {
        cell.closed(limit).await.unwrap().iter().map(|closed| closed.last()).collect()
    };
    assert_eq!(taken(2).await, [1, 2]);
    assert_eq!(taken(2).await, [3]);
    assert_eq!(taken(2).await, Vec::<u64>::new());
}

/// A cell's capacity grows, and shrinks no lower than what it holds, durably: the cell reopened
/// has it, and a fork inherits it.
#[tokio::test(flavor = "multi_thread")]
async fn capacity_resizes() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 128 << 10);
    cell.write(extent(1), 0, &pattern(1, 0, 64 << 10)).await.unwrap();
    cell.commit(None).await.unwrap();
    error_is(cell.write(extent(2), 0, &pattern(2, 0, 96 << 10)).await, NoSpace);
    cell.set_capacity(256 << 10).await.unwrap();
    assert_eq!(
        (cell.info().unwrap().capacity, cell.info().unwrap().available),
        (256 << 10, 192 << 10)
    );
    cell.write(extent(2), 0, &pattern(2, 0, 96 << 10)).await.unwrap();
    cell.commit(None).await.unwrap();
    let held = (64 + 96) << 10;
    let refused = cell.set_capacity(held - BLOCK as u64).await.unwrap_err();
    assert_eq!(
        refused.downcast_ref::<TooSmall>(),
        Some(&TooSmall { capacity: held - BLOCK as u64, held })
    );
    assert!(cell.set_capacity(BLOCK as u64 - 1).await.is_err());
    cell.set_capacity(held).await.unwrap();
    assert_eq!(cell.info().unwrap().available, 0);
    // cladefs: durable, though no commit came since, the index holding it (TASK-165.7).
    drop(cell);
    let cell = LocalCell::open(dir.clone(), OPTIONS).unwrap();
    assert_eq!(cell.info().unwrap().capacity, held);
    cell.set_capacity(2 * held).await.unwrap();
    cell.commit(None).await.unwrap();
    drop(cell);
    let cell = LocalCell::open(dir, OPTIONS).unwrap();
    assert_eq!(cell.info().unwrap().capacity, 2 * held);
    assert_eq!(read(&cell, extent(2), 0, 96 << 10), pattern(2, 0, 96 << 10));
}

/// cladefs: a framed copy is written back a MiB at a time, each waited for, before it is synced,
/// so that its sync leaves little for the host's disk to write at once: a framed segment's whole
/// data, synced at once, filled the disk's queue, which the syncs of a daemon's turns waited
/// behind, up to a second under pgbench (GCE r728, TASK-165.9). Its framing writes it, the making
/// durable after writes it back (TASK-401): waiting for each MiB, a framing waited 1.0-2.7 s behind
/// a disk VMs share. No sync writes more at once than a commit's, of a quarter MiB of 16 KiB
/// extents in the open segment. Red, the 12 MiB copy synced at once, with the making durable
/// syncing it unwritten back.
#[tokio::test(flavor = "multi_thread")]
async fn framings_write_back_as_they_go() {
    let dir = MemDir::new();
    let options = Options { segment_size: 32 << 20, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    // Half noise, half zeros: a copy of about half the data.
    let len = 16 << 10;
    let data = |sequence| {
        let mut data = noise(sequence, len / 2);
        data.resize(len, 0);
        data
    };
    let in_place = local_cell::segments_framed_in_place();
    for sequence in 1..=1536 {
        cell.write(extent(sequence), 0, &data(sequence)).await.unwrap();
        if sequence % 16 == 0 {
            cell.commit(None).await.unwrap();
        }
    }
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    cell.wait_framed().await.unwrap();
    assert_eq!(local_cell::segments_framed_in_place(), in_place, "framed in a copy");
    let framed = dir.open(&format!("{}/{}", of("segments", CELL.id), Padded(0)), true).unwrap();
    let size = framed.size().unwrap();
    assert!((10 << 20..16 << 20).contains(&size), "a copy of 24 MiB, half zeros: {size} bytes");
    // A commit's sync: its 16 extents of 16 KiB, and its record, its version and MAC (TASK-411.5,
    // doc-28).
    assert_eq!(dir.io().most_synced_unwritten, (256 << 10) + 4 + 603 + 32);
    for sequence in 1..=1536 {
        assert_eq!(read(&cell, extent(sequence), 0, len), data(sequence));
    }
}

/// cladefs: what a guest writes to the open segment is written back as it writes it, its writing
/// back begun a MiB at a time and waited for 8 MiB behind, before any commit, so that the sync of
/// the flush after has little left for the host's disk to write at once: a guest's burst, up to
/// 300 MB a second under pgbench, waited in the host's page cache for the flush of cladefsd's next
/// turn, which synced 100 MB at once in 250-420 ms (GCE r954); its writing back begun but not
/// waited for filled the disk's queue as well, syncs waiting behind it up to 0.6 s (r956,
/// TASK-221). Red with nothing written back until the flush. cladefs: what the sync has left is
/// what the cell's page cache holds dirty (TASK-433), here one that would hold all 32 MiB.
#[tokio::test(flavor = "multi_thread")]
async fn writes_are_written_back_as_they_go() {
    let big = 64 << 20;
    let pages =
        local_cell::pages::Options { background: big, hard: big, most: big, ..Default::default() };
    let dir = MemDir::with_cache(pages);
    let options = Options { segment_size: 64 << 20, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    let len = 16 << 10;
    for sequence in 1..=2048 {
        cell.write(extent(sequence), 0, &noise(sequence, len)).await.unwrap();
    }
    assert_eq!(dir.cache().usage().peak_dirty, 9 << 20);
    cell.commit(None).await.unwrap();
    for sequence in 1..=2048 {
        assert_eq!(read(&cell, extent(sequence), 0, len), noise(sequence, len));
    }
}

/// cladefs: the framing of segment 0, which the commit after `cell`'s first closes, held where its
/// framed file is created, not yet written, until the returned sender says go or drops: its
/// framing begun once this returns.
async fn framing_held(cell: &LocalCell<MemDir>) -> (std::sync::mpsc::Sender<()>, fail::FailGuard) {
    use std::sync::Mutex;
    use std::sync::mpsc::channel;
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.flush().await.unwrap();
    let (waiting, waits) = channel();
    let (go, gone) = channel::<()>();
    let gone = Mutex::new(gone);
    let guard = fail::FailGuard::with_callback("segments::framing", move || {
        let _ = waiting.send(());
        let _ = gone.lock().unwrap().recv();
    })
    .unwrap();
    cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    waits.recv().unwrap();
    (go, guard)
}

/// cladefs: a commit made in a segment as it closes, its framing not yet written, is durable once a
/// flush has synced the segment's raw file and appended the commit's record to the segment's log,
/// which it syncs: no flush waits for a framing (TASK-221), and an opening after a crash closes the
/// segment again from its raw file and log (TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn commits_in_segments_being_framed_are_durable_in_their_raw_files_and_logs() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    let (go, guard) = framing_held(&cell).await;
    cell.flush().await.unwrap();
    for seed in 0..16 {
        let reopened = LocalCell::open(dir.crash(seed), OPTIONS).unwrap();
        assert_eq!(reopened.last_commit().unwrap(), 2, "seed {seed}");
        for sequence in 1..=2 {
            assert_eq!(
                read(&reopened, extent(sequence), 0, BLOCK),
                pattern(sequence as u8, 0, BLOCK),
                "seed {seed}, extent {sequence}"
            );
        }
    }
    go.send(()).unwrap();
    drop(guard);
}

/// cladefs: a segment's raw file and log stay until its framed file is synced, its commits durable:
/// an opening after a crash closes a segment whose framed file isn't whole again from them
/// (TASK-165.9); what goes once it is, the framed file holds, a crash after finding it whole.
#[tokio::test(flavor = "multi_thread")]
async fn raw_files_and_logs_stay_until_their_framed_files_are_synced() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    let (go, guard) = framing_held(&cell).await;
    cell.flush().await.unwrap();
    let name = Padded(0).to_string();
    let held = |kind: &str| dir.list(&of(kind, CELL.id)).unwrap().contains(&name);
    assert!(held("open-segments") && held("logs"), "segment 0's raw file or log went, unframed");
    go.send(()).unwrap();
    drop(guard);
    settled(&cell).await;
    assert!(!held("open-segments") && !held("logs"), "segment 0's raw file or log stayed, framed");
    for seed in 0..16 {
        let reopened = LocalCell::open(dir.crash(seed), OPTIONS).unwrap();
        assert_eq!(reopened.last_commit().unwrap(), 2, "seed {seed}");
        for sequence in 1..=2 {
            assert_eq!(
                read(&reopened, extent(sequence), 0, BLOCK),
                pattern(sequence as u8, 0, BLOCK),
                "seed {seed}, extent {sequence}"
            );
        }
    }
}

/// cladefs: the bytes the process's cells wrote of extents' own files, framed, since `before`
/// (`local_cell::written`): the tests of a process are its own (cargo nextest's).
fn own_written_since(before: local_cell::Written) -> usize {
    (local_cell::written().own - before.own) as usize
}

/// cladefs: the names in the cell's directory of `kind`.
fn names(dir: &MemDir, kind: &str) -> Vec<String> {
    dir.list(&of(kind, CELL.id)).unwrap()
}

/// cladefs: the names of the cell's files of `kind` in `dir`, excluding spare logs (TASK-301).
fn segment_files(dir: &MemDir, kind: &str) -> Vec<String> {
    names(dir, kind).into_iter().filter(|name| !name.starts_with("spare-")).collect()
}

/// cladefs: a large extent none of whose parts compresses is framed in place: its raw file is its
/// own file then, its tail appended, renamed, its data written once, as its guest wrote it, not a
/// second time framed in a file of its own, as a sandbox host's daemon wrote it, some 2.3 times
/// what its guests wrote, its framings waiting seconds behind (TASK-310). What it wrote of own
/// files, its tail alone; through a crash after, it reads as written. Red with every large
/// extent framed in a copy: its data written again.
#[tokio::test(flavor = "multi_thread")]
async fn incompressible_large_extents_are_framed_in_place() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 4 << 20);
    let data = noise(1, 3 * OPTIONS.large_extent as usize + BLOCK);
    cell.write(extent(1), 0, &data).await.unwrap();
    let before = local_cell::written();
    cell.commit(None).await.unwrap();
    // cladefs: its footer names it (TASK-411.3).
    let footer = bucket_cell::layout::own_extent_footer(extent(1));
    let framed = extent_frame::framed(&data, &footer, bucket_cell::testing::encrypting(CELL));
    assert_eq!(own_written_since(before), framed.len() - data.len());
    let name = Padded(1).to_string();
    assert_eq!(names(&dir, "open-extents"), Vec::<String>::new());
    assert_eq!(names(&dir, "extents"), vec![name]);
    assert!(read(&cell, extent(1), 0, data.len()) == data, "extent 1 reads otherwise");
    drop(cell);
    for seed in 0..16 {
        let reopened = LocalCell::open(dir.crash(seed), OPTIONS).unwrap();
        assert_eq!(reopened.last_commit().unwrap(), 1, "seed {seed}");
        assert!(read(&reopened, extent(1), 0, data.len()) == data, "seed {seed}");
    }
}

/// cladefs: a guest writing more than a segment of small extents between commits, as one writing a
/// large file before its fsync, after a commit: each segment it fills waits for the next commit,
/// which seals every extent where it was written, copying nothing and making no file of an
/// extent's own, and closes them, their footers naming their runs as the commit's, which the open
/// segment ends (TASK-92.41); none of their parts compressing, each is framed in place, its data
/// written once (TASK-310). Before, the commit copied out every extent of the segments set aside,
/// each into a file of its own past the open segment's room: cell-bench's burst, 640 own files,
/// each its own create, frame, sync and upload. Then a crash keeps them all. Red with the extents
/// in segments filled copied out: 516 KB copied; with segments framed in copies: none in place.
#[tokio::test(flavor = "multi_thread")]
async fn bursts_past_a_segment_between_commits_are_sealed_where_they_are() {
    use extent_frame::segment::tag;
    let dir = MemDir::new();
    // Segments full once they hold a segment's size and an extent's most, as small here.
    let options = Options { max_extent: OPTIONS.segment_size, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    cell.write(extent(1), 0, &noise(1, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    let (extents, len) = (8 * OPTIONS.segment_size as usize / (2 * BLOCK), 2 * BLOCK);
    for sequence in 2..=extents as u64 {
        cell.write(extent(sequence), 0, &noise(sequence, len)).await.unwrap();
    }
    let before = (local_cell::written(), local_cell::segments_framed_in_place());
    let (before, framed_before) = before;
    // The open segment closes with the commit too, which ends in it.
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    let after = local_cell::written();
    let copied = (after.own + after.copied) - (before.own + before.copied);
    assert_eq!(copied, 0, "copied");
    assert_eq!((names(&dir, "extents"), names(&dir, "open-extents")), (vec![], vec![]));
    let closed = cell.closed(usize::MAX).await.unwrap();
    let continued = |closed: &local_cell::Closed<MemFile>| {
        let contents = &closed.footer.contents;
        (contents.commits.clone(), contents.continued.as_ref().map(|c| c.commit))
    };
    let mut expected = vec![(vec![], Some(tag(0, 2))); closed.len()];
    expected[0].0 = vec![tag(0, 1)];
    expected[closed.len() - 1] = (vec![tag(0, 2)], None);
    assert_eq!(closed.iter().map(continued).collect::<Vec<_>>(), expected);
    assert_eq!(closed.len(), 4, "the three segments filled, and the open one");
    let in_place = local_cell::segments_framed_in_place() - framed_before;
    assert_eq!(in_place as usize, closed.len(), "framed in place");
    let framed = after.framed - before.framed;
    assert!(framed < (closed.len() * BLOCK) as u64, "framed {framed}");
    drop(cell);
    for seed in 0..4 {
        let reopened = LocalCell::open(dir.crash(seed), options).unwrap();
        assert_eq!(reopened.last_commit().unwrap(), 2, "seed {seed}");
        for sequence in 2..=extents as u64 {
            let read = read(&reopened, extent(sequence), 0, len);
            assert!(read == noise(sequence, len), "seed {seed}, {sequence}");
        }
    }
}

/// cladefs: segments compressing their parts would save less than an eighth of are framed in place,
/// their parts stored raw, as a guest's filesystem fills them, its metadata compressing in a few
/// parts of a large file's data; those it would save more of are framed in copies, compressed
/// (TASK-310). Red with segments framed in place only if no part compresses: none was.
#[tokio::test(flavor = "multi_thread")]
async fn segments_compressing_little_are_framed_in_place() {
    let options = Options { max_extent: OPTIONS.segment_size, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };
    for (every, in_place) in [(16, true), (1, false)] {
        let every = every as u64;
        let dir = MemDir::new();
        let cell = LocalCell::create(dir.clone(), description, options).unwrap();
        let (extents, len) = (4 * OPTIONS.segment_size as usize / (2 * BLOCK), 2 * BLOCK);
        // Every `every`th extent compresses, the first of each segment's.
        let data = |sequence: u64| match sequence % every == 1 % every {
            true => pattern(sequence as u8, 0, len),
            false => noise(sequence, len),
        };
        for sequence in 1..=extents as u64 {
            cell.write(extent(sequence), 0, &data(sequence)).await.unwrap();
        }
        let framed = local_cell::segments_framed_in_place();
        cell.close_segment().unwrap();
        cell.commit(None).await.unwrap();
        settled(&cell).await;
        let closed = cell.closed(usize::MAX).await.unwrap().len() as u64;
        let framed = local_cell::segments_framed_in_place() - framed;
        assert_eq!(framed, if in_place { closed } else { 0 }, "one in {every} compressing");
        assert!(closed >= 2, "{closed} segments closed");
        drop(cell);
        let reopened = LocalCell::open(dir.crash(0), options).unwrap();
        for sequence in 1..=extents as u64 {
            let read = read(&reopened, extent(sequence), 0, len);
            assert!(read == data(sequence), "one in {every} compressing, {sequence}");
        }
    }
}

/// cladefs TASK-343: a segment holding noise, then extents' holes, as Hunt's hostile VM process's
/// did (3 MiB of its fills' noise, then 2.6 MB of zeros its sparse extents' commit sealed in one
/// run each), is framed in a copy, compressed, its holes taking nothing; not in place, raw, which
/// kept the holes, bytes no write of the tenant's was admitted for, on its disk past its limit
/// (Hunt run 42949975683). Red with in-place framing decided by every eighth MiB part's
/// compression: the first part alone, of noise. Its extents read as written, before and after a
/// crash.
#[tokio::test(flavor = "multi_thread")]
async fn segments_of_noise_then_holes_are_framed_compressed() {
    const MIB: usize = 1 << 20;
    let options = Options {
        max_extent: 8 * MIB as u64,
        segment_size: 8 * MIB as u64,
        large_extent: 8 * MIB as u64,
        ..OPTIONS
    };
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let dir = MemDir::new();
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    for sequence in 1..=3 {
        cell.write(extent(sequence), 0, &noise(sequence, MIB)).await.unwrap();
    }
    // A block at the end of each of three extents of a MiB: a MiB's hole before each.
    for sequence in 4..=6 {
        cell.write(extent(sequence), (MIB - BLOCK) as u64, &noise(sequence, BLOCK)).await.unwrap();
    }
    let framed = local_cell::segments_framed_in_place();
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    assert!(!cell.closed(usize::MAX).await.unwrap().is_empty(), "no segment closed");
    assert_eq!(local_cell::segments_framed_in_place() - framed, 0, "framed in place");
    let expect = |sequence: u64| match sequence {
        1..=3 => noise(sequence, MIB),
        _ => {
            let mut held = vec![0; MIB - BLOCK];
            held.extend(noise(sequence, BLOCK));
            held
        }
    };
    for sequence in 1..=6 {
        assert!(read(&cell, extent(sequence), 0, MIB) == expect(sequence), "{sequence}");
    }
    drop(cell);
    let reopened = LocalCell::open(dir.crash(0), options).unwrap();
    for sequence in 1..=6 {
        assert!(read(&reopened, extent(sequence), 0, MIB) == expect(sequence), "{sequence}");
    }
}

/// cladefs TASK-400: a segment that compresses has its parts packed as they are written whole, held
/// in memory, and its framing in a copy reads none of them back, but its last part, never whole;
/// one whose parts were let go of to make room, as a daemon's governor lets them go, reads them
/// back and packs them as before: framed alike, byte for byte, either way. Held parts are derived:
/// a crash as a closing between commits frames it, before its framed file is durable, keeps the
/// commits in the raw file and log, as before, whatever was held. Before, a framing in a copy read
/// its whole raw file back as it closed, seconds after the guest wrote it, from a disk eight VMs
/// keep busy once the host had let its pages go.
#[tokio::test(flavor = "multi_thread")]
async fn segments_framed_in_copies_take_their_parts_held() {
    use local_cell::HeldParts;
    use std::sync::Mutex;
    const MIB: usize = 1 << 20;
    let options = Options {
        max_extent: 8 * MIB as u64,
        segment_size: 8 * MIB as u64,
        large_extent: 8 * MIB as u64,
        ..OPTIONS
    };
    // Letters at random, sixteen of them: compresses by about half, as a build's files do.
    let text = |seed: u64, len: usize| -> Vec<u8> {
        noise(seed, len).into_iter().map(|byte| b'a' + byte % 16).collect()
    };
    let data = |sequence: u64| text(sequence, if sequence <= 6 { MIB } else { BLOCK });
    let framed = async |evicted: bool, dir: MemDir| {
        let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
        LocalCell::make(&dir, description, None, None).unwrap();
        let held = HeldParts::new();
        let opening = Opening { held: Some(held.clone()), ..Opening::new(CELL) };
        let cell = LocalCell::recover_unmade(dir.clone()).unwrap().open(options, opening).unwrap();
        cell.made().unwrap();
        assert!(Arc::ptr_eq(cell.held_parts(), &held));
        for sequence in 1..=7 {
            cell.write(extent(sequence), 0, &data(sequence)).await.unwrap();
        }
        cell.commit(None).await.unwrap();
        let holding = held.counts();
        if evicted {
            assert_eq!(held.evict_to(0), holding.bytes);
        }
        // Crashes as the framed file is written, not yet durable.
        let crashes = Arc::new(Mutex::new(Vec::new()));
        let guard = {
            let (crashes, dir) = (crashes.clone(), dir.clone());
            fail::FailGuard::with_callback("segments::making_durable", move || {
                crashes.lock().unwrap().extend((0..4).map(|seed| dir.crash(seed)));
            })
            .unwrap()
        };
        let before = local_cell::framing_reads().0;
        assert!(cell.close_now().await.unwrap(), "closed between commits");
        settled(&cell).await;
        drop(guard);
        let read_back = local_cell::framing_reads().0 - before;
        for sequence in 1..=7 {
            assert!(read(&cell, extent(sequence), 0, data(sequence).len()) == data(sequence));
        }
        let segments = names(&dir, "segments");
        assert_eq!(segments.len(), 1, "{segments:?}");
        let path = format!("{}/{}", of("segments", CELL.id), segments[0]);
        let bytes = local_cell::read_whole(&dir.open(&path, true).unwrap()).unwrap();
        assert!(bytes.len() < 4 * MIB, "a copy, compressed: {} bytes", bytes.len());
        drop(cell);
        let crashes = std::mem::take(&mut *crashes.lock().unwrap());
        assert_eq!(crashes.len(), 4);
        for (seed, crashed) in crashes.into_iter().enumerate() {
            let reopened = LocalCell::open(crashed, options).unwrap();
            assert_eq!(reopened.last_commit().unwrap(), 1, "seed {seed}");
            for sequence in 1..=7 {
                let read = read(&reopened, extent(sequence), 0, data(sequence).len());
                assert!(read == data(sequence), "seed {seed}, evicted {evicted}, {sequence}");
            }
        }
        (bytes, read_back, holding, held.counts())
    };
    let (bytes, read_back, holding, after) = framed(false, MemDir::serial()).await;
    assert_eq!(read_back, BLOCK as u64, "the last part alone read");
    assert!(holding.bytes > 2 * MIB as u64 && holding.bytes < 4 * MIB as u64, "{holding:?}");
    assert_eq!((after.bytes, after.taken, after.evicted), (0, holding.held, 0));
    let (evicted_bytes, evicted_read_back, _, evicted_after) = framed(true, MemDir::serial()).await;
    assert_eq!(evicted_read_back, 6 * MIB as u64 + BLOCK as u64, "every part read back");
    assert_eq!((evicted_after.evicted_parts, evicted_after.taken), (6, 0));
    // Each framing encrypted under a salt of its own (TASK-411.28): alike once opened.
    let opened = |bytes: &[u8]| {
        let keys = bucket_cell::testing::ring([CELL]);
        let keys = keys.of(CELL.id);
        let parts = extent_frame::Parts::of(bytes, &keys).unwrap();
        let stored: Vec<_> = parts.stored.iter().map(|part| (part.encoding, part.len)).collect();
        (stored, extent_frame::data(bytes, &keys).unwrap())
    };
    assert!(opened(&bytes) == opened(&evicted_bytes), "framed otherwise, held or not");
    // A directory whose I/O runs on the cell's threads: its parts packed ahead on threads of
    // their own, at the host's lowest priority, held as they are packed, or read and packed by the
    // framing, if not yet: framed alike whichever.
    let (concurrent_bytes, _, _, concurrent_after) = framed(false, MemDir::new()).await;
    assert!(
        opened(&bytes) == opened(&concurrent_bytes),
        "framed otherwise, packed ahead on threads"
    );
    assert_eq!(concurrent_after.bytes, 0, "{concurrent_after:?}");
}

/// cladefs: extents not one run in a segment filled since the last commit, written in pieces
/// interleaved with others', or across two segments, are copied by the commit into the open
/// segment, and past its room into the next, it full in turn, which the commit goes on with: none
/// into a file of its own (TASK-92.41); and all read as written, before and after a crash.
#[tokio::test(flavor = "multi_thread")]
async fn copies_past_the_open_segments_room_go_on_in_the_next() {
    let dir = MemDir::new();
    let options = Options { max_extent: OPTIONS.segment_size, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    // Two extents at a time, a block of each in turn: each in pieces, other writes between.
    let (extents, blocks) = (8 * OPTIONS.segment_size as usize / (2 * BLOCK), 2);
    let data = |sequence: u64| noise(sequence, blocks * BLOCK);
    for pair in (1..=extents as u64).step_by(2) {
        for block in 0..blocks {
            for sequence in [pair, pair + 1] {
                let at = block * BLOCK;
                cell.write(extent(sequence), at as u64, &data(sequence)[at..at + BLOCK])
                    .await
                    .unwrap();
            }
        }
    }
    let before = local_cell::written();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    let after = local_cell::written();
    assert_eq!(after.own - before.own, 0, "own files written");
    assert_eq!(after.copied - before.copied, (extents * blocks * BLOCK) as u64, "copied");
    assert_eq!(names(&dir, "extents"), Vec::<String>::new());
    for sequence in 1..=extents as u64 {
        assert!(read(&cell, extent(sequence), 0, blocks * BLOCK) == data(sequence), "{sequence}");
    }
    drop(cell);
    let reopened = LocalCell::open(dir.crash(0), options).unwrap();
    for sequence in 1..=extents as u64 {
        let read = read(&reopened, extent(sequence), 0, blocks * BLOCK);
        assert!(read == data(sequence), "{sequence}");
    }
}

/// cladefs: a large extent that compresses is framed in a file of its own, as before, its raw file
/// gone once its commit is durable; and one whose parts compress past its first 16 MiB, which are
/// framed in place no more, is too, those copied there as they are ([`LocalCell::seal_own`],
/// TASK-310): what each wrote of own files, its framed file's bytes.
#[tokio::test(flavor = "multi_thread")]
async fn compressible_large_extents_are_framed_in_copies() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    let compressible = pattern(1, 0, 3 * OPTIONS.large_extent as usize + BLOCK);
    let mut mixed = noise(2, 16 << 20);
    mixed.extend(pattern(2, 0, 1 << 20));
    for (sequence, data) in [(1, &compressible), (2, &mixed)] {
        cell.write(extent(sequence), 0, data).await.unwrap();
        let before = local_cell::written();
        cell.commit(None).await.unwrap();
        let footer = bucket_cell::layout::own_extent_footer(extent(sequence));
        let framed = extent_frame::framed(data, &footer, bucket_cell::testing::encrypting(CELL));
        assert!(framed.len() < data.len() + framed.len() / 2, "extent {sequence} compresses");
        assert_eq!(own_written_since(before), framed.len(), "extent {sequence}");
    }
    assert_eq!(names(&dir, "open-extents"), Vec::<String>::new());
    assert_eq!(names(&dir, "extents"), vec![Padded(1).to_string(), Padded(2).to_string()]);
    drop(cell);
    let reopened = LocalCell::open(dir.crash(0), OPTIONS).unwrap();
    for (sequence, data) in [(1, &compressible), (2, &mixed)] {
        assert!(read(&reopened, extent(sequence), 0, data.len()) == *data, "extent {sequence}");
    }
}

/// cladefs: a commit framing a large extent in place, its raw file renamed from one directory to
/// another (TASK-310), stopped after each of its operations, as by a crash: the cell opens, the
/// extent a commit before framed in place reading as written, and the commit's extent reading as
/// written exactly if the commit is durable.
#[tokio::test(flavor = "multi_thread")]
async fn extents_framed_in_place_survive_crashes_framing_them() {
    let len = 3 * OPTIONS.large_extent as usize + BLOCK;
    let (first, second) = (noise(1, len), noise(2, len));
    for operations in 0.. {
        let dir = MemDir::serial();
        let cell = create(dir.clone(), 4 << 20);
        cell.write(extent(1), 0, &first).await.unwrap();
        cell.commit(None).await.unwrap();
        cell.write(extent(2), 0, &second).await.unwrap();
        dir.fail_after(Some(operations));
        let committed = cell.commit(None).await;
        dir.fail_after(None);
        for seed in 0..4 {
            let reopened = LocalCell::open(dir.crash(seed), OPTIONS);
            let reopened = reopened.unwrap_or_else(|error| {
                panic!("stopped after {operations} operations, seed {seed}: {error:#}")
            });
            let at = format!("stopped after {operations} operations, seed {seed}");
            assert!(read(&reopened, extent(1), 0, len) == first, "{at}: extent 1");
            let mut read = vec![0; len];
            let intact = reopened.read(extent(2), 0, &mut read).is_ok() && read == second;
            assert_eq!(
                reopened.last_commit().unwrap() == 2,
                intact,
                "{at}: extent 2, durable or not"
            );
        }
        drop(cell);
        if committed.is_ok() {
            break;
        }
    }
}

/// cladefs: a commit closing segments filled since the commit before, sealing in them what is one
/// run there, each framed in place, none of its parts compressing, its raw file linked as its
/// framed file (TASK-310, TASK-92.41), stopped after each of its operations, as by a crash: the
/// cell opens, what the commit before made reading as written, and what the commit made reading
/// as written exactly if the commit is durable.
#[tokio::test(flavor = "multi_thread")]
async fn segments_framed_in_place_survive_crashes_framing_them() {
    let options = Options { max_extent: OPTIONS.segment_size, ..OPTIONS };
    let (extents, len) = (3 * OPTIONS.segment_size as usize / (2 * BLOCK), 2 * BLOCK);
    let first = noise(1, BLOCK);
    for operations in 0.. {
        let dir = MemDir::serial();
        let description = Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };
        let cell = LocalCell::create(dir.clone(), description, options).unwrap();
        cell.write(extent(1), 0, &first).await.unwrap();
        cell.commit(None).await.unwrap();
        for sequence in 2..=extents as u64 {
            cell.write(extent(sequence), 0, &noise(sequence, len)).await.unwrap();
        }
        let framed = local_cell::segments_framed_in_place();
        dir.fail_after(Some(operations));
        cell.close_segment().unwrap();
        let committed = cell.commit(None).await;
        dir.fail_after(None);
        for seed in 0..4 {
            let reopened = LocalCell::open(dir.crash(seed), options);
            let reopened = reopened.unwrap_or_else(|error| {
                panic!("stopped after {operations} operations, seed {seed}: {error:#}")
            });
            let at = format!("stopped after {operations} operations, seed {seed}");
            assert!(read(&reopened, extent(1), 0, BLOCK) == first, "{at}: extent 1");
            let durable = reopened.last_commit().unwrap() == 2;
            for sequence in 2..=extents as u64 {
                let mut read = vec![0; len];
                let intact = reopened.read(extent(sequence), 0, &mut read).is_ok()
                    && read == noise(sequence, len);
                assert_eq!(durable, intact, "{at}: extent {sequence}, durable or not");
            }
        }
        drop(cell);
        if committed.is_ok() {
            assert!(local_cell::segments_framed_in_place() > framed, "none framed in place");
            break;
        }
    }
}

/// cladefs: segments one commit closes, framed one after another, then made durable together, their
/// framed files synced, then their directory, once
/// (`segments::make_durable`, TASK-310), stopped after each of the commit's operations, as by a
/// crash, which may keep any of the framed files whole and any torn, a later one's whole and an
/// earlier one's not: the cell opens, what the commit before made reading as written, and what
/// the commit made reading as written exactly if it is durable: an opening taking the last's
/// footer, whole, the commit's record, though a crash lost the first's, which it closed again
/// without the commit, whose record its log lacked, read the commit with its runs there lost; and
/// one taking a framed file whose raw file's entry was never durable, a crash keeping its entry,
/// as unchecked, read it torn (`wal::recover`, `Segments::unsynced_entries`). Whole, the
/// commit made its segments durable together, at once.
#[tokio::test(flavor = "multi_thread")]
async fn segments_made_durable_together_survive_crashes_making_them_durable() {
    let options = Options { max_extent: OPTIONS.segment_size, ..OPTIONS };
    let len = 2 * BLOCK;
    let per_segment = OPTIONS.segment_size as usize / len;
    // Six segments' worth, of bytes that compress: framed in copies of a few blocks each, which a
    // crash keeps whole, or not, as often (`MemDir::crash`), where a segment framed in place, a
    // segment's data unsynced, it never keeps whole (segments_framed_in_place_survive_...'s).
    let extents = 6 * per_segment as u64;
    let data = |sequence: u64| pattern(sequence as u8, 0, len);
    let first = noise(1, BLOCK);
    for operations in 0.. {
        let dir = MemDir::serial();
        let description = Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };
        let cell = LocalCell::create(dir.clone(), description, options).unwrap();
        cell.write(extent(1), 0, &first).await.unwrap();
        cell.commit(None).await.unwrap();
        for sequence in 2..2 + extents {
            cell.write(extent(sequence), 0, &data(sequence)).await.unwrap();
        }
        let before = local_cell::timings();
        dir.fail_after(Some(operations));
        cell.close_segment().unwrap();
        let committed = cell.commit(None).await;
        dir.fail_after(None);
        for seed in 0..16 {
            let reopened = LocalCell::open(dir.crash(seed), options);
            let reopened = reopened.unwrap_or_else(|error| {
                panic!("stopped after {operations} operations, seed {seed}: {error:#}")
            });
            let at = format!("stopped after {operations} operations, seed {seed}");
            assert!(read(&reopened, extent(1), 0, BLOCK) == first, "{at}: extent 1");
            let durable = reopened.last_commit().unwrap() == 2;
            for sequence in 2..2 + extents {
                let mut read = vec![0; len];
                let intact =
                    reopened.read(extent(sequence), 0, &mut read).is_ok() && read == data(sequence);
                assert_eq!(durable, intact, "{at}: extent {sequence}, durable or not");
            }
        }
        drop(cell);
        if committed.is_ok() {
            let after = local_cell::timings();
            let batches = after.durable.count - before.durable.count;
            let segments = after.durable_segments - before.durable_segments;
            assert_eq!(batches, 1, "the commit's segments made durable apart");
            // Three filled, a segment and an extent each, then the open one.
            assert_eq!(segments, 4, "segments made durable");
            break;
        }
    }
}

/// cladefs: a segment's framing waits for no other's syncs: while the segments framed before it
/// are being made durable, held there, it is framed, its framed file written, and it is made
/// durable after, with those framed meanwhile; neither is durable, nor its raw file and log gone,
/// before it is (TASK-310). Each sync of a framing waited for those before it, on a disk VMs
/// share seconds each.
#[tokio::test(flavor = "multi_thread")]
async fn framings_wait_for_no_other_framings_syncs() {
    use std::sync::Mutex;
    use std::sync::mpsc::channel;
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.flush().await.unwrap();
    let before = local_cell::timings();
    // Each framing, as it makes its framed file; each making durable, held until told to go.
    let (framing, framings) = channel();
    let framing = Mutex::new(framing);
    let _framing = fail::FailGuard::with_callback("segments::framing", move || {
        let _ = framing.lock().unwrap().send(());
    })
    .unwrap();
    let (holding, held) = channel();
    let (go, gone) = channel::<()>();
    let (holding, gone) = (Mutex::new(holding), Mutex::new(gone));
    let _durable = fail::FailGuard::with_callback("segments::making_durable", move || {
        let _ = holding.lock().unwrap().send(());
        let _ = gone.lock().unwrap().recv();
    })
    .unwrap();
    cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    framings.recv().unwrap();
    held.recv().unwrap();
    // Segment 0 being made durable, held: segment 1 closes, and is framed meanwhile.
    cell.write(extent(3), 0, &pattern(3, 0, BLOCK)).await.unwrap();
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    framings.recv().unwrap();
    assert_eq!(cell.framing(), [0, 1], "framed, neither durable yet");
    let raw = |number: u64| {
        let name = Padded(number).to_string();
        let held = |kind: &str| dir.list(&of(kind, CELL.id)).unwrap().contains(&name);
        held("open-segments") && held("logs")
    };
    assert!(raw(0) && raw(1), "a raw file or log went before its framed file was durable");
    drop(go);
    settled(&cell).await;
    assert!(cell.framing().is_empty(), "framings still running, settled");
    assert!(!raw(0) && !raw(1), "a raw file or log stayed, its framed file durable");
    let after = local_cell::timings();
    assert_eq!(after.durable.count - before.durable.count, 2, "segment 1 waited for segment 0");
    assert_eq!(after.durable_segments - before.durable_segments, 2);
    for seed in 0..16 {
        let reopened = LocalCell::open(dir.crash(seed), OPTIONS).unwrap();
        assert_eq!(reopened.last_commit().unwrap(), 3, "seed {seed}");
        for sequence in 1..=3 {
            assert_eq!(
                read(&reopened, extent(sequence), 0, BLOCK),
                pattern(sequence as u8, 0, BLOCK),
                "seed {seed}, extent {sequence}"
            );
        }
    }
}

/// cladefs: making a segment's framed file durable syncs the directories of raw files and logs only
/// for its own entries, and only if no flush synced them since it opened: the commit's flush after
/// a segment opens syncs them, and says so. So a closing costs its commit's flush those two, and
/// the making durable the directory of framed files alone, made durable here before that flush,
/// the next segment's entries unsynced then, which its own don't need. Synced again for every
/// segment whose entries no making had synced, the one just opened always among them, both cost a
/// journal commit more each closing, behind which pgbench-dax's flushes waited seconds on a disk
/// VMs share (TASK-369): 5 a closing, not 3; red so with the flush's marking left out, or every
/// segment's synced.
#[tokio::test(flavor = "multi_thread")]
async fn segments_made_durable_sync_no_entries_a_flush_synced() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    for sequence in 2..6 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
        settled(&cell).await;
        let before = dir.io().synced_dirs;
        cell.close_segment().unwrap();
        cell.commit_ordered(None).await.unwrap();
        // Framed and made durable (1 dir sync: SEGMENTS), then the commit's flush (1 dir sync:
        // OPEN_SEGMENTS for the newly opened segment; LOGS is not synced because the closed
        // segment already had its log created by the prior durable commit, board 11709).
        cell.wait_framed().await.unwrap();
        cell.flush().await.unwrap();
        let synced = dir.io().synced_dirs - before;
        assert_eq!(synced, 2, "closing after extent {sequence}: directories synced");
    }
}

/// cladefs: board 11709 / board 1770: subsequent flushes in the same open segment cost zero
/// directory syncs once its log exists, and opening a new segment on close costs zero file syncs
/// until a flush actually appends commits to it.
#[tokio::test(flavor = "multi_thread")]
async fn subsequent_flushes_in_an_open_segment_sync_no_directory() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    for sequence in 2..6 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        let before = dir.io().synced_dirs;
        cell.flush().await.unwrap();
        let synced = dir.io().synced_dirs - before;
        assert_eq!(synced, 0, "subsequent flush for extent {sequence}: directories synced");
    }
    for sequence in 1..6 {
        assert_eq!(read(&cell, extent(sequence), 0, BLOCK), pattern(sequence as u8, 0, BLOCK));
    }
}

/// cladefs: board 11709 (hotbed review 165): a spanning commit that closes multiple segments in a
/// single `make_durable` batch when NONE of those segments have durable log files yet recovers
/// cleanly across a crash after every filesystem operation inside that batch (`fail_after`),
/// verifying the intra-batch `SEGMENTS` directory sync + barrier between renames.
#[tokio::test(flavor = "multi_thread")]
async fn unlogged_multi_segment_batches_recover_across_crashes() {
    let options = Options { max_extent: OPTIONS.segment_size, ..OPTIONS };
    let len = 2 * BLOCK;
    let per_segment = OPTIONS.segment_size as usize / len;
    // Six segments' worth (`segment_limit() = 2 * segment_size`, so 3 filled + 1 open = 4
    // segments) of compressible extents written BEFORE any commit or flush, so zero segment log
    // files exist when the spanning commit closes and frames them in one batch.
    let extents = 6 * per_segment as u64;
    let data = |sequence: u64| pattern(sequence as u8, 0, len);
    for operations in 0.. {
        let dir = MemDir::serial();
        let description = Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };
        let cell = LocalCell::create(dir.clone(), description, options).unwrap();
        for sequence in 1..=extents {
            cell.write(extent(sequence), 0, &data(sequence)).await.unwrap();
        }
        assert!(
            dir.list(&of("logs", CELL.id)).unwrap().is_empty(),
            "all closed segments must be unlogged before the spanning commit"
        );
        let before = local_cell::timings();
        dir.fail_after(Some(operations));
        cell.close_segment().unwrap();
        let committed = cell.commit(None).await;
        dir.fail_after(None);
        for seed in 0..16 {
            let reopened = LocalCell::open(dir.crash(seed), options).unwrap_or_else(|error| {
                panic!("stopped after {operations} operations, seed {seed}: {error:#}")
            });
            let at = format!("stopped after {operations} operations, seed {seed}");
            let durable = reopened.last_commit().unwrap() == 1;
            assert!(
                reopened.last_commit().unwrap() <= 1,
                "{at}: unexpected commit {}",
                reopened.last_commit().unwrap()
            );
            for sequence in 1..=extents {
                let mut buf = vec![0; len];
                let intact =
                    reopened.read(extent(sequence), 0, &mut buf).is_ok() && buf == data(sequence);
                assert_eq!(durable, intact, "{at}: extent {sequence}, durable or not");
            }
        }
        drop(cell);
        if committed.is_ok() {
            let after = local_cell::timings();
            let batches = after.durable.count - before.durable.count;
            let segments = after.durable_segments - before.durable_segments;
            assert_eq!(batches, 1, "all unlogged segments must enter one make_durable batch");
            assert!(segments >= 3, "expected multi-segment unlogged batch, got {segments}");
            break;
        }
    }
}

/// cladefs: a cell creates no log until its first durable commit (board 11709): one that only
/// reads, as a fork a daemon serves, creates no log file and holds none open.
#[tokio::test(flavor = "multi_thread")]
async fn cells_that_only_read_prepare_no_log() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    let logs = || dir.list(&of("logs", CELL.id)).unwrap().len();
    cell.flush().await.unwrap();
    assert_eq!(logs(), 0, "a cell that only reads creates no log");
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(logs(), 1, "first durable commit creates the open segment's log");
}

/// cladefs: the uploader takes a segment once its framing wrote its framed file, not once it made
/// it durable, which its raw file and log stand for meanwhile, until the commits its footer names
/// are (TASK-384): a durable commit waited, one after another, for the framed file's sync and its
/// directory's, each a commit of the host's journal, before its upload began. A crash anywhere
/// before the framed file is durable keeps every commit: as the cell's write-ahead log holds them,
/// if the bucket has not acknowledged the segment, or as the bucket does, once it has, the cell not
/// told so yet. The cell hears of the acknowledgement once the framed file is durable, which it
/// waits for, and lets the segment's files go then, as before; and a crash after keeps them too.
#[tokio::test(flavor = "multi_thread")]
async fn segments_upload_before_their_framed_files_are_durable() {
    use std::sync::Mutex;
    use std::sync::mpsc::channel;
    let dir = MemDir::new();
    let cell = created_on(dir.clone(), 1 << 20, bucket_cell::State::new(CELL), WRITER);
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    // Its making durable held until told to go.
    let (holding, held) = channel();
    let (go, gone) = channel::<()>();
    let (holding, gone) = (Mutex::new(holding), Mutex::new(gone));
    let durable = fail::FailGuard::with_callback("segments::making_durable", move || {
        let _ = holding.lock().unwrap().send(());
        let _ = gone.lock().unwrap().recv();
    })
    .unwrap();
    assert!(cell.close_for_upload(false).await.unwrap());
    held.recv().unwrap();
    cell.flush().await.unwrap();
    let closed = cell.closed(usize::MAX).await.unwrap();
    let [segment] = &closed[..] else { panic!("{} segments closed", closed.len()) };
    assert_eq!(cell.framing(), [0], "taken for the upload, its framed file not durable");
    let framed = Padded(segment.number).to_string();
    assert!(!names(&dir, "segments").contains(&framed), "its framed file named its own");
    let mut landed = bucket_cell::State::new(CELL);
    landed.apply(&segment.footer, segment.data_len);
    let chain = WRITER.admit(&segment.footer).unwrap();
    // What a cell opened on what the bucket holds, crashed at `seed`, holds: both extents, here,
    // or in the bucket, once it landed there.
    let check = |seed: u64, bucket: &bucket_cell::State, chain: Chain| {
        let reopened = opened_on(dir.crash(seed), bucket.clone(), chain);
        for sequence in 1..=2 {
            match reopened.place(extent(sequence)).unwrap() {
                Some(Place::Bucket(_)) => assert!(bucket.last_commit.is_some(), "seed {seed}"),
                Some(_) => assert_eq!(
                    read(&reopened, extent(sequence), 0, BLOCK),
                    pattern(sequence as u8, 0, BLOCK),
                    "seed {seed}, extent {sequence}"
                ),
                None => panic!("seed {seed}: extent {sequence} lost"),
            }
        }
    };
    for seed in 0..16 {
        // Uploaded, not landed.
        check(seed, &bucket_cell::State::new(CELL), WRITER);
        // Landed, its acknowledgement not heard.
        check(seed, &landed, chain);
    }
    drop(go);
    drop(durable);
    cell.acknowledged(segment.number).await.unwrap();
    assert!(cell.framing().is_empty(), "its framing ended, durable, before it was heard");
    assert!(!names(&dir, "segments").contains(&framed), "acknowledged, it left");
    for seed in 0..16 {
        check(seed, &landed, chain);
    }
}

/// The files the process has open.
fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

/// A cell keeps no more segments' files open than its options say, however many segments its
/// commits close: closing one each, as a cell uploading to object storage does on every sync,
/// once ran a daemon out of file descriptors.
#[tokio::test(flavor = "multi_thread")]
async fn closed_segments_keep_no_more_files_open_than_allowed() {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let cell = create(host_fs::may_block(|| HostDir::new(temp.path()).unwrap()), 64 << 20);
    let mut open = Vec::new();
    for sequence in 1..=40 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.close_segment().unwrap();
        cell.commit(None).await.unwrap();
        // cladefs: framed, which no commit waits for: a segment being framed holds its raw file and
        // log open till then (TASK-165.9).
        settled(&cell).await;
        open.push(open_fds());
    }
    assert_eq!(open[39], open[9], "{open:?}");
    assert_eq!(read(&cell, extent(1), 0, BLOCK), pattern(1, 0, BLOCK));
    assert_eq!(read(&cell, extent(40), 0, BLOCK), pattern(40, 0, BLOCK));
}

/// What a cell's files did, by the directory each is in: the bytes read.
#[derive(Default)]
struct Counts {
    read: BTreeMap<String, u64>,
}

/// cladefs: a cell directory in memory whose files count what they do ([`Counts`]).
#[derive(Clone)]
struct Counting {
    dir: MemDir,
    counts: Arc<std::sync::Mutex<Counts>>,
}

struct CountedFile {
    file: MemFile,
    /// The directory it is in.
    kind: String,
    counts: Arc<std::sync::Mutex<Counts>>,
}

impl Counting {
    fn new() -> Self {
        Self { dir: MemDir::new(), counts: Default::default() }
    }

    fn file(&self, file: MemFile, path: &str) -> CountedFile {
        let kind = path.split('/').next().unwrap().to_owned();
        CountedFile { file, kind, counts: self.counts.clone() }
    }

    /// The bytes read so far.
    fn read(&self) -> BTreeMap<String, u64> {
        self.counts.lock().unwrap().read.clone()
    }
}

impl local_cell::File for CountedFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        *self.counts.lock().unwrap().read.entry(self.kind.clone()).or_default() += buf.len() as u64;
        self.file.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.file.write_at(buf, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        self.file.size()
    }

    fn sync(&self) -> std::io::Result<()> {
        self.file.sync()
    }
}

impl Dir for Counting {
    type File = CountedFile;

    fn create(&self, path: &str, data: bool) -> std::io::Result<CountedFile> {
        Ok(self.file(self.dir.create(path, data)?, path))
    }

    fn open(&self, path: &str, data: bool) -> std::io::Result<CountedFile> {
        Ok(self.file(self.dir.open(path, data)?, path))
    }

    fn remove(&self, path: &str) -> std::io::Result<()> {
        self.dir.remove(path)
    }

    fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
        self.dir.rename(from, to)
    }

    fn link(&self, from: &str, to: &str) -> std::io::Result<()> {
        self.dir.link(from, to)
    }

    fn replace(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.dir.replace(path, bytes)
    }

    fn create_dir(&self, path: &str) -> std::io::Result<()> {
        self.dir.create_dir(path)
    }

    fn list(&self, path: &str) -> std::io::Result<Vec<String>> {
        self.dir.list(path)
    }

    fn sync_dir(&self, path: &str) -> std::io::Result<()> {
        self.dir.sync_dir(path)
    }

    fn barrier(&self) -> std::io::Result<()> {
        self.dir.barrier()
    }
}

/// cladefs: a cell opening again reads of the segments of its write-ahead log their tails alone:
/// each's trailer and footer, which the replay of its footers needs, and its trailer and table,
/// which say how much data it holds, three blocks; none of their data, their raw files gone, their
/// commits durable (TASK-92.16, TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn cells_open_reading_the_tails_of_their_logs_segments() {
    let dir = Counting::new();
    let cell = create(dir.clone(), 1 << 20);
    for sequence in 0..16 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.close_segment().unwrap();
        cell.commit(None).await.unwrap();
    }
    drop(cell);
    // None yet: commits closed them, from their raw files.
    let segments_read = || dir.read().get("segments").copied().unwrap_or(0);
    assert_eq!(segments_read(), 0);
    let cell = LocalCell::open(dir.clone(), OPTIONS).unwrap();
    assert_eq!(segments_read(), 16 * 3 * BLOCK as u64);
    assert_eq!(cell.extents().unwrap().len(), 16);
}

/// The host's page size.
#[cfg(target_os = "linux")]
fn page_size() -> usize {
    // SAFETY: Reads a configuration value.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// The pages of the file at `path` that the host's page cache holds.
#[cfg(target_os = "linux")]
fn cached_pages(path: &std::path::Path) -> usize {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    assert_ne!(len, 0, "{} holds nothing", path.display());
    let (prot, flags) = (libc::PROT_READ, libc::MAP_SHARED);
    // SAFETY: Maps the file, unread, to ask the kernel which of its pages it caches, into a byte
    // for each, and unmaps it.
    unsafe {
        let addr = libc::mmap(std::ptr::null_mut(), len, prot, flags, file.as_raw_fd(), 0);
        assert_ne!(addr, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
        let mut cached = vec![0u8; len.div_ceil(page_size())];
        assert_eq!(libc::mincore(addr, len, cached.as_mut_ptr()), 0);
        libc::munmap(addr, len);
        cached.iter().filter(|page| **page & 1 == 1).count()
    }
}

/// cladefs: on Linux, no file of a cell's holds pages in the host's page cache, its writes and its
/// reads going around it (`O_DIRECT`), what is held held in the process's own page cache
/// (`local_cell::pages`, TASK-433): the open segment's raw file as durable commits write it, its
/// log, and the segment's framed file once framed. Until TASK-433 the raw file's pages stayed in
/// the host's page cache through durable commits (TASK-92.23), which this tested.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_files_hold_no_pages_in_the_hosts_page_cache() {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let cell = create(host_fs::may_block(|| HostDir::new(temp.path()).unwrap()), 64 << 20);
    for sequence in 1..=8 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
    }
    // Written back, so that the files hold what was written, written around the host's cache.
    host_fs::may_block(|| local_cell::pages::shared().write_back_all()).unwrap();
    let path = |kind| temp.path().join(of(kind, CELL.id)).join(Padded(0).to_string());
    assert_eq!(cached_pages(&path("open-segments")), 0, "the open segment's pages cached");
    assert_eq!(cached_pages(&path("logs")), 0, "the open segment's log's pages cached");
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    assert_eq!(cached_pages(&path("segments")), 0, "the framed segment's pages cached");
}

/// Segments of 64 KiB and extents of 64 KiB at most, so that a segment holds 128 KiB at most.
const SMALL_SEGMENTS: Options = Options { max_extent: 64 << 10, ..OPTIONS };

/// The most bytes of data a segment of [`SMALL_SEGMENTS`] holds: a segment's, and an extent's.
const SEGMENT_LIMIT: u64 = 128 << 10;

/// cladefs: the bytes of `cell`'s segments' data no extent is in, no write being in progress, and
/// of the frees it keeps for the bucket alone, until acknowledged there (TASK-165.9).
fn dead_bytes(cell: &LocalCell<MemDir>) -> u64 {
    let usage = cell.usage().unwrap();
    usage.segment_bytes - usage.live_bytes + usage.kept_bytes
}

/// The bytes of data of each of the cell's segments in `dir`: open ones' raw files,
/// closed ones' framed data. Not the framed file of a segment being framed behind what closed it,
/// which may not be written yet, and whose raw file holds its data until its framing ends
/// (TASK-189): named its own until it is durable (`framing-`, TASK-310).
fn segment_data(dir: &MemDir) -> Vec<u64> {
    let mut lens = Vec::new();
    for kind in ["open-segments", "segments"] {
        for name in dir.list(&of(kind, CELL.id)).unwrap() {
            if name.starts_with("framing-") {
                continue;
            }
            let file = dir.open(&format!("{}/{name}", of(kind, CELL.id)), true).unwrap();
            let mut bytes = vec![0; file.size().unwrap() as usize];
            file.read_at(&mut bytes, 0).unwrap();
            let data = if kind == "segments" {
                extent_frame::data(&bytes, &bucket_cell::testing::ring([CELL]).of(CELL.id)).unwrap()
            } else {
                bytes
            };
            lens.push(data.len() as u64);
        }
    }
    lens
}

/// TASK-189: reading the cell's segments beside the cell between writes, as [`segment_data`]
/// does, reads no framed file a framing has created and not yet written: a commit closed the
/// segments writes filled, and framed them behind, on a thread of its own (TASK-92.41).
/// Each framing waits at the failpoint between its file's creation and its write while the helper
/// reads: read so, unskipped, the file was empty, `Corrupt("an object's trailer")`, as in CI run
/// 42105764695. The failpoint is the process's, as nextest gives each test.
#[tokio::test(flavor = "multi_thread")]
async fn segments_read_beside_the_cell_skip_framed_files_being_written() {
    use std::sync::Mutex;
    use std::sync::mpsc::channel;
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, SMALL_SEGMENTS).unwrap();
    let block = |sequence: u64| pattern(sequence as u8, 0, BLOCK);
    cell.write(extent(96), 0, &block(96)).await.unwrap();
    cell.commit(None).await.unwrap();
    // Each framing says it waits, its file created, and waits until let go: or until the test
    // stops, failing, its channels dropped, so that the cell's drop, waiting for its framings,
    // ends.
    let (waiting, waits) = channel();
    let (go, gone) = channel::<()>();
    let gone = Mutex::new(gone);
    let guard = fail::FailGuard::with_callback("segments::framing", move || {
        let _ = waiting.send(());
        let _ = gone.lock().unwrap().recv();
    })
    .unwrap();
    let (mut seen, mut reads) = (BTreeSet::new(), 0);
    for sequence in 0..48 {
        cell.write(extent(sequence), 0, &block(sequence)).await.unwrap();
    }
    // cladefs: the segments filled, framed behind the commit closing them (TASK-92.41).
    cell.commit_ordered(None).await.unwrap();
    loop {
        let begun: Vec<u64> = (cell.framing().into_iter()).filter(|n| seen.insert(*n)).collect();
        if begun.is_empty() {
            break;
        }
        for _ in &begun {
            waits.recv().unwrap();
            let lens = segment_data(&dir);
            assert!(lens.iter().all(|len| *len <= SEGMENT_LIMIT), "segments of {lens:?}");
            reads += 1;
            go.send(()).unwrap();
        }
    }
    drop(guard);
    assert!(reads > 1, "no segments framed behind a commit");
    cell.commit(None).await.unwrap();
    for sequence in 0..48 {
        assert!(read(&cell, extent(sequence), 0, BLOCK) == block(sequence), "{sequence}");
    }
}

/// TASK-150.2: a segment writes filled, holding a commit's data, goes to the uploader with no
/// commit after it: the uploader's closing between commits closes it, and frames it, as nothing
/// else would until the guest next commits, which a sync of the cell, or a fork, waiting for that
/// commit's upload, may not live to see (TASK-92.41). It reads as before, and the next commit
/// copies out of it what was written since the commit before.
#[tokio::test(flavor = "multi_thread")]
async fn segments_filled_go_to_the_uploader_with_no_commit_after() {
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir, description, SMALL_SEGMENTS).unwrap();
    let block = |sequence: u64| pattern(sequence as u8, 0, BLOCK);
    cell.write(extent(0), 0, &block(0)).await.unwrap();
    cell.commit(None).await.unwrap();
    // Past what a segment holds, no commit with them.
    for sequence in 1..48 {
        cell.write(extent(sequence), 0, &block(sequence)).await.unwrap();
    }

    cell.flush().await.unwrap();
    assert_eq!(cell.closed(8).await.unwrap().len(), 0, "a segment closed before the uploader's");
    assert!(cell.close_now().await.unwrap(), "the segment filled, holding the commit, not closed");
    let lasts: Vec<u64> = cell.closed(8).await.unwrap().iter().map(Closed::last).collect();
    assert_eq!(lasts, [1]);
    for sequence in 0..48 {
        assert!(read(&cell, extent(sequence), 0, BLOCK) == block(sequence), "{sequence}");
    }
    cell.commit(None).await.unwrap();
    for sequence in 0..48 {
        assert!(read(&cell, extent(sequence), 0, BLOCK) == block(sequence), "{sequence}");
    }
}

/// cladefs: an uploader's interval leaves a segment the guest is filling for its next commit, which
/// seals in place what it wrote there (TASK-92.41): one filled since the last commit, or the open
/// one past 8 MiB written since; it closes one written little since, as a journal's commits leave
/// it, and a commit waiting for its upload closes it whatever it holds.
#[tokio::test(flavor = "multi_thread")]
async fn interval_closings_leave_segments_being_filled() {
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir, description, SMALL_SEGMENTS).unwrap();
    let block = |sequence: u64| pattern(sequence as u8, 0, BLOCK);
    cell.write(extent(0), 0, &block(0)).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.write(extent(1), 0, &block(1)).await.unwrap();
    assert!(cell.close_now_unless_filling().await.unwrap(), "a segment written little since, open");
    cell.write(extent(2), 0, &block(2)).await.unwrap();
    cell.commit(None).await.unwrap();
    // Past what a segment holds: it fills.
    for sequence in 3..48 {
        cell.write(extent(sequence), 0, &block(sequence)).await.unwrap();
    }
    assert!(!cell.close_now_unless_filling().await.unwrap(), "a segment filled, closed");
    assert!(cell.close_now().await.unwrap(), "a segment filled, holding a commit, left open");
    // The open one, past 8 MiB written since the last commit.
    let options = Options { max_extent: 16 << 20, large_extent: 16 << 20, ..OPTIONS };
    let cell = LocalCell::create(MemDir::new(), description, options).unwrap();
    cell.write(extent(0), 0, &block(0)).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.write(extent(1), 0, &vec![1; 9 << 20]).await.unwrap();
    assert!(!cell.close_now_unless_filling().await.unwrap(), "a segment being filled, closed");
    assert!(cell.close_now().await.unwrap(), "a segment being filled, holding a commit, left open");
    // Commit-heavy burst: past 1 MiB across frequent commits stays open against interval closes
    // (hotbed question 534), while urgent close_now still closes it.
    let burst_options = Options { segment_size: 32 << 20, ..options };
    let cell = LocalCell::create(MemDir::new(), description, burst_options).unwrap();
    for sequence in 0..4u64 {
        cell.write(extent(sequence), 0, &vec![sequence as u8; 512 << 10]).await.unwrap();
        cell.commit(None).await.unwrap();
    }
    assert!(
        !cell.close_now_unless_filling().await.unwrap(),
        "an actively filling segment across commits was closed by interval"
    );
    assert!(cell.close_now().await.unwrap(), "urgent close must still close an active burst");
}

/// The open segment takes writes until it holds a segment and an extent, `Options::segment_size`
/// and `Options::max_extent`, between commits: then the next goes to another, which no segment
/// grows past, however much is written between commits (TASK-79.2). Full, holding commits' data or
/// not, it waits for the next commit, which seals the extents in it where they are, none in a file
/// of its own, and closes it, the commit going on past it (TASK-92.41). Every extent reads as
/// written meanwhile, and once committed, and so after opening the cell again.
#[tokio::test(flavor = "multi_thread")]
async fn segments_hold_a_segment_and_an_extent_at_most() {
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, SMALL_SEGMENTS).unwrap();
    let block = |sequence: u64| pattern(sequence as u8, 0, BLOCK);
    let check = |cell: &LocalCell<MemDir>, sequences: std::ops::Range<u64>| {
        for sequence in sequences {
            assert!(read(cell, extent(sequence), 0, BLOCK) == block(sequence), "{sequence}");
        }
    };
    // 192 KiB of blocks, no commit's data with them.
    for sequence in 0..48 {
        cell.write(extent(sequence), 0, &block(sequence)).await.unwrap();
        let lens = segment_data(&dir);
        assert!(lens.iter().all(|len| *len <= SEGMENT_LIMIT), "segments of {lens:?}");
    }
    check(&cell, 0..48);
    cell.commit(None).await.unwrap();
    check(&cell, 0..48);
    let lens = segment_data(&dir);
    assert!(lens.iter().all(|len| *len <= SEGMENT_LIMIT), "segments of {lens:?}");
    let own = dir.list(&of("extents", CELL.id)).unwrap();
    assert_eq!(own, Vec::<String>::new(), "extents sealed in files of their own");

    // A commit's data in the open segment, then another 192 KiB of blocks: the segment holding it
    // fills as they come, and closes with the commit after.
    cell.write(extent(96), 0, &block(96)).await.unwrap();
    cell.commit(None).await.unwrap();
    for sequence in 48..96 {
        cell.write(extent(sequence), 0, &block(sequence)).await.unwrap();
        let lens = segment_data(&dir);
        assert!(lens.iter().all(|len| *len <= SEGMENT_LIMIT), "segments of {lens:?}");
    }
    cell.commit(None).await.unwrap();
    check(&cell, 0..97);
    let lasts: Vec<u64> = cell.closed(usize::MAX).await.unwrap().iter().map(Closed::last).collect();
    assert_eq!(lasts, [1, 1, 3, 3]);
    let lens = segment_data(&dir);
    assert!(lens.iter().all(|len| *len <= SEGMENT_LIMIT), "segments of {lens:?}");
    drop(cell);
    check(&LocalCell::open(dir.clone(), SMALL_SEGMENTS).unwrap(), 0..97);
}

/// The segment the newest checkpoint names, whose footer would follow it, set aside full with no
/// commit's data, and so no footer: the cell is still its newest checkpoint and the footers of
/// its closed segments after it (TASK-79.2).
#[tokio::test(flavor = "multi_thread")]
async fn segments_set_aside_leave_the_cell_its_footers() {
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, SMALL_SEGMENTS).unwrap();
    // 192 KiB of blocks after the checkpoint the cell opened with, which names segment 0.
    for sequence in 0..48 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    assert_eq!(dir.list(&of("segments", CELL.id)).unwrap(), Vec::<String>::new());
    let state = closed_state(&cell).await;
    assert_eq!(state.0, 1);
    assert_eq!(reopened_state(cell, &dir).0, state);
}

/// An extent written again and again between commits takes no segment past a segment and an
/// extent: its writes go to others (TASK-79.2). It reads as last written, and once committed,
/// where it was last written, in the open segment; the segments it filled, no extent's data in
/// them, closed with the commit, their footers alone (TASK-92.41).
#[tokio::test(flavor = "multi_thread")]
async fn rewrites_take_no_segment_past_a_segment_and_an_extent() {
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, SMALL_SEGMENTS).unwrap();
    for seed in 0..64 {
        cell.write(extent(1), 0, &pattern(seed, 0, 2 * BLOCK)).await.unwrap();
        let lens = segment_data(&dir);
        assert!(lens.iter().all(|len| *len <= SEGMENT_LIMIT), "segments of {lens:?}");
    }
    assert_eq!(read(&cell, extent(1), 0, 2 * BLOCK), pattern(63, 0, 2 * BLOCK));
    cell.commit(None).await.unwrap();
    settled(&cell).await;
    assert_eq!(read(&cell, extent(1), 0, 2 * BLOCK), pattern(63, 0, 2 * BLOCK));
    let place = Place::Run { cell: CELL.id, number: 4, offset: 0 };
    assert_eq!(cell.place(extent(1)).unwrap(), Some(place));
    let segments: Vec<String> = (0..4).map(|number| Padded(number).to_string()).collect();
    assert_eq!(segment_files(&dir, "open-segments"), ["0000000000004"]);
    assert_eq!(dir.list(&of("segments", CELL.id)).unwrap(), segments);
    for closed in cell.closed(usize::MAX).await.unwrap() {
        let data = extent_frame::data(
            &closed.bytes().unwrap(),
            &bucket_cell::testing::ring([CELL]).of(CELL.id),
        )
        .unwrap();
        assert_eq!(data.len(), 0, "segment {}, no extent's data in it, framed", closed.number);
    }
    let cell = reopened(cell, &dir);
    assert_eq!(read(&cell, extent(1), 0, 2 * BLOCK), pattern(63, 0, 2 * BLOCK));
}

/// Rewrites of an open extent take room in segments, which no extent holds once the extent is
/// sealed: past the capacity's worth, they count as used, until the commit sealing the extent
/// takes them back, which is the writer's to make (TASK-79.2); then they are dead space, which
/// takes no room, and which leaves the cell with its segment, once acknowledged in the bucket
/// (TASK-165.8). So a guest rewriting a block between commits takes twice its capacity of the
/// host's disk at most until it commits.
#[tokio::test(flavor = "multi_thread")]
async fn rewrites_past_the_capacity_count_as_used() {
    let capacity = 64 * BLOCK as u64;
    let cell = create(MemDir::new(), capacity);
    // The first write, and the capacity's worth of rewrites, less the block the extent holds.
    for seed in 0..128 {
        cell.write(extent(1), 0, &pattern(seed, 0, BLOCK)).await.unwrap();
    }
    assert_eq!(cell.info().unwrap().available, 0);
    error_is(cell.write(extent(1), 0, &pattern(128, 0, BLOCK)).await, NoSpace);
    error_is(cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await, NoSpace);
    assert_eq!(read(&cell, extent(1), 0, BLOCK), pattern(127, 0, BLOCK));
    // Sealed, the extent leaves them in a segment no extent holds but it, which takes none of the
    // room: it leaves the cell once acknowledged in the bucket (TASK-165.8).
    cell.commit(None).await.unwrap();
    assert_eq!(dead_bytes(&cell), 128 * BLOCK as u64);
    assert_eq!(cell.info().unwrap().available, capacity - BLOCK as u64);
    cell.write(extent(2), 0, &pattern(2, 0, BLOCK)).await.unwrap();
    assert_eq!(read(&cell, extent(1), 0, BLOCK), pattern(127, 0, BLOCK));
}

/// cladefs: what frees of extents open let go of, of what writes since the last commit wrote,
/// written over or not, in segments or in raw files of their own, counts as used past the
/// capacity's worth, as what writes wrote over does, until the commit takes it back (TASK-79.2):
/// so that a writer writing extents and freeing them, never committing, takes twice the capacity
/// of the host's disk at most, though its extents hold nothing.
#[tokio::test(flavor = "multi_thread")]
async fn frees_of_open_extents_past_the_capacity_count_as_used() {
    let capacity = 64 * BLOCK as u64;
    let cell = create(MemDir::new(), capacity);
    // Blocks written and freed; blocks written, written over and freed; and extents in raw files
    // of their own, of 8 blocks, written and freed: twice the capacity's worth, 512 KiB.
    for sequence in 0..32 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.free(extent(sequence)).unwrap();
    }
    for sequence in 32..48 {
        cell.write(extent(sequence), 0, &pattern(1, 0, BLOCK)).await.unwrap();
        cell.write(extent(sequence), 0, &pattern(2, 0, BLOCK)).await.unwrap();
        cell.free(extent(sequence)).unwrap();
    }
    for sequence in 48..56 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, 8 * BLOCK)).await.unwrap();
        cell.free(extent(sequence)).unwrap();
    }
    error_is(cell.write(extent(56), 0, &pattern(56, 0, BLOCK)).await, NoSpace);
    assert_eq!(cell.info().unwrap().available, 0);
    // The commit takes it back: nothing holds it, as no commit did.
    cell.commit(None).await.unwrap();
    assert_eq!(cell.info().unwrap().available, capacity);
    cell.write(extent(56), 0, &pattern(56, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, extent(56), 0, BLOCK), pattern(56, 0, BLOCK));
}

/// Writes, a commit, and frees of all but one of the extents written, and a commit: a segment
/// closed, which one extent pins. Of data that does not compress, so that what no extent is in
/// takes as much of the host's disk as it is.
async fn churn(cell: &LocalCell<MemDir>, first: u64) {
    for sequence in first..first + 16 {
        cell.write(extent(sequence), 0, &noise(sequence, BLOCK)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    for sequence in first + 1..first + 16 {
        cell.free(extent(sequence)).unwrap();
    }
    cell.commit(None).await.unwrap();
}

/// What the cell's segments hold that no extent is in, freed, is dead space until its segments
/// leave the cell, acknowledged in the bucket, which takes none of the cell's room, past the
/// capacity's worth too: the bucket's collector's to take back, which the cell's writer can't
/// make room for (TASK-79.2, TASK-165.8).
#[tokio::test(flavor = "multi_thread")]
async fn freed_space_is_dead_space_taking_no_room() {
    let capacity = 64 * BLOCK as u64;
    let cell = create(MemDir::new(), capacity);
    for cycle in 0..4 {
        churn(&cell, 16 * cycle).await;
    }
    // 240 KiB freed: within the capacity's worth.
    assert_eq!(dead_bytes(&cell), 60 * BLOCK as u64);
    assert_eq!(cell.info().unwrap().available, capacity - 4 * BLOCK as u64);
    churn(&cell, 64).await;
    // 300 KiB freed: past the capacity's worth.
    assert_eq!(dead_bytes(&cell), 75 * BLOCK as u64);
    assert_eq!(cell.info().unwrap().available, capacity - 5 * BLOCK as u64);
    error_is(cell.write(extent(100), 0, &vec![1; 60 * BLOCK]).await, NoSpace);
    cell.write(extent(100), 0, &vec![1; 59 * BLOCK]).await.unwrap();
    cell.free(extent(100)).unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.info().unwrap().available, capacity - 5 * BLOCK as u64);
}

/// cladefs: what the cell's segments hold that no extent is in, freed, takes none of the room the
/// cell's writes need, however much it is: it leaves the cell with its segments, acknowledged
/// in the bucket, whose collector takes it back (TASK-165.8). Here it gathers commit by commit,
/// past twice the capacity (TASK-79.2).
#[tokio::test(flavor = "multi_thread")]
async fn dead_space_takes_none_of_the_room_writes_need() {
    let capacity = 64 * BLOCK as u64;
    let cell = create(MemDir::new(), capacity);
    // Ten segments, each pinned by one extent: 600 KiB freed, more than twice the capacity.
    for cycle in 0..10 {
        churn(&cell, 16 * cycle).await;
    }
    assert_eq!(dead_bytes(&cell), 150 * BLOCK as u64);
    assert_eq!(cell.info().unwrap().available, capacity - 10 * BLOCK as u64);
    // All the room left.
    cell.write(extent(200), 0, &pattern(200, 0, 54 * BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(dead_bytes(&cell), 150 * BLOCK as u64);
    assert_eq!(cell.info().unwrap().available, 0);
    error_is(cell.write(extent(201), 0, &pattern(201, 0, BLOCK)).await, NoSpace);
    for sequence in (0..160).step_by(16) {
        assert!(read(&cell, extent(sequence), 0, BLOCK) == noise(sequence, BLOCK), "{sequence}");
    }
    assert_eq!(read(&cell, extent(200), 0, 54 * BLOCK), pattern(200, 0, 54 * BLOCK));
}

/// cladefs: an uploader keeps the frees of a guest writing and freeing files until it has uploaded
/// the segments holding them, and lets go of them at once, the cell's dead space past twice its
/// capacity of a sudden. Counted as used past the capacity's worth, the dead space found the
/// guest's next write, a journal write of Fxfs's, refused (`ENOSPC`), the cell a tenth used: the
/// guest's filesystem failed (TASK-79.2). The segments, acknowledged, leave the cell, what is
/// live of them read in the bucket, and the dead space with them (TASK-165.8): the write takes
/// the room the extents leave it.
#[tokio::test(flavor = "multi_thread")]
async fn dead_space_the_uploader_lets_go_takes_none_of_the_room() {
    let capacity = 64 * BLOCK as u64;
    let cell = create(MemDir::new(), capacity);
    // Ten segments, each pinned by one extent, what is freed in them kept for the bucket.
    for cycle in 0..10 {
        churn(&cell, 16 * cycle).await;
    }
    assert_eq!(dead_bytes(&cell), 150 * BLOCK as u64);
    // Acknowledged, and released: 600 KiB let go of at once, more than twice the capacity, with
    // the segments.
    for closed in cell.closed(usize::MAX).await.unwrap() {
        cell.acknowledged(closed.number).await.unwrap();
    }
    // The write the next commit comes with, of all the room the extents leave.
    cell.write(extent(200), 0, &pattern(200, 0, 54 * BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    // But the frees of the last churn's commit, its segment open still, kept until the bucket
    // acknowledges it, their data there.
    assert_eq!(dead_bytes(&cell), 15 * BLOCK as u64);
    assert_eq!(cell.usage().unwrap().segments, 1);
    assert_eq!(cell.info().unwrap().available, 0);
    for sequence in (0..160).step_by(16) {
        let place = cell.place(extent(sequence)).unwrap();
        assert!(matches!(place, Some(Place::Bucket(_))), "{sequence} at {place:?}");
    }
    assert_eq!(read(&cell, extent(200), 0, 54 * BLOCK), pattern(200, 0, 54 * BLOCK));
}

/// cladefs: the bytes the cell's files of extents' data take of the host's disk: its segments',
/// open and closed, and its extents' own files and raw files (TASK-79.89), in `dir`, `cell`'s.
/// Once the cell is idle (TASK-463): a framing made durable behind a commit renamed its segment's
/// framed file its own between this listing it and opening it, which failed `NotFound` (CI run
/// 43729753505).
async fn data_files(cell: &LocalCell<MemDir>, dir: &MemDir) -> u64 {
    cell.wait_idle().await.unwrap();
    let mut bytes = 0;
    for kind in ["segments", "open-segments", "extents", "open-extents"] {
        for name in dir.list(&of(kind, CELL.id)).unwrap() {
            let file = dir.open(&format!("{}/{name}", of(kind, CELL.id)), true).unwrap();
            bytes += file.size().unwrap();
        }
    }
    bytes
}

/// cladefs: a guest writing a segment of blocks of data that does not compress at each commit, and
/// freeing all but two of them at the next, which pin the segment, until it lets go of them too,
/// twenty-one commits later; its uploader acknowledging each segment the cell closes, and
/// releasing each commit, as it goes. Each segment acknowledged leaves the cell, its extents read
/// in the bucket from then on: what the pins hold, and what the frees left, is the bucket's, whose
/// collector takes it back (TASK-165.8). Once the uploader has caught up, the cell's files are
/// none but its open segment's, which the commit that closed the last left empty. Before, they
/// took what cleaning behind the commits had yet to take back, which writes were refused past
/// three times the capacity for (TASK-79.89).
#[tokio::test(flavor = "multi_thread")]
async fn acknowledged_segments_leave_a_churning_guests_files() {
    let capacity = 64 * BLOCK as u64;
    let dir = MemDir::new();
    let cell = create(dir.clone(), capacity);
    let mut live = BTreeSet::new();
    let free = |live: &mut BTreeSet<u64>, sequence: u64| {
        cell.free(extent(sequence)).unwrap();
        live.remove(&sequence);
    };
    for cycle in 0..100 {
        let first = 16 * cycle;
        if cycle > 0 {
            for sequence in first - 14..first {
                free(&mut live, sequence);
            }
        }
        if cycle > 20 {
            let pins = 16 * (cycle - 21);
            free(&mut live, pins);
            free(&mut live, pins + 1);
        }
        for sequence in first..first + 16 {
            cell.write(extent(sequence), 0, &noise(sequence, BLOCK)).await.unwrap();
            live.insert(sequence);
        }
        cell.commit(None).await.unwrap();
        cell.flush().await.unwrap();
        for closed in cell.closed(usize::MAX).await.unwrap() {
            cell.acknowledged(closed.number).await.unwrap();
        }
        settled(&cell).await;
        assert_eq!(data_files(&cell, &dir).await, 0, "the files after cycle {cycle}");
    }
    for sequence in live {
        let place = cell.place(extent(sequence)).unwrap();
        assert!(matches!(place, Some(Place::Bucket(_))), "{sequence} at {place:?}");
    }
}

/// cladefs: an honest guest whose uploader lags (TASK-79.89): forty commits, each of sixteen blocks
/// in segments and an extent of eight in a file of its own, of data that does not compress, all of
/// it freed by the next, its extents holding a third of its capacity at most. The uploader keeps
/// what the frees let go of until it releases their commits, and the segments until they are
/// acknowledged in the bucket, if they are as they close (`acknowledges`): returns what the
/// cell's files take then, which the upload lag bound bounds, making commits wait (cladefs's
/// `max_lag_bytes`). Once the uploader catches up, the next commit lets it all go, the bucket
/// holding what it kept (TASK-165.8).
async fn uploader_lagging(acknowledges: bool) -> u64 {
    let capacity = 64 * BLOCK as u64;
    let dir = MemDir::new();
    let cell = create(dir.clone(), capacity);
    for cycle in 0..40 {
        let first = 17 * cycle;
        for sequence in first..first + 16 {
            cell.write(extent(sequence), 0, &noise(sequence, BLOCK)).await.unwrap();
        }
        cell.write(extent(first + 16), 0, &noise(first + 16, 8 * BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
        for sequence in first..first + 17 {
            cell.free(extent(sequence)).unwrap();
        }
        cell.commit(None).await.unwrap();
        if acknowledges {
            for closed in cell.closed(usize::MAX).await.unwrap() {
                cell.acknowledged(closed.number).await.unwrap();
            }
        }
    }
    let files = data_files(&cell, &dir).await;
    for closed in cell.closed(usize::MAX).await.unwrap() {
        cell.acknowledged(closed.number).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    assert_eq!(data_files(&cell, &dir).await, 0);
    files
}

/// cladefs: `uploader_lagging`, its uploader acknowledging nothing, nor releasing anything: the
/// cell's files take ten times the capacity, which no bound of the cell's refuses writes for
/// (TASK-79.89).
#[tokio::test(flavor = "multi_thread")]
async fn what_an_uploader_keeps_stays_until_acknowledged() {
    let files = uploader_lagging(false).await;
    assert!(files > 10 * 64 * BLOCK as u64, "{files} bytes of files");
}

/// cladefs: `uploader_lagging`, its uploader acknowledging the segments as the cell closes them,
/// and releasing nothing: the frees it keeps, in the bucket, hold no file of the cell's
/// (TASK-165.8).
#[tokio::test(flavor = "multi_thread")]
async fn frees_kept_in_segments_acknowledged_hold_no_file() {
    assert_eq!(uploader_lagging(true).await, 0);
}

/// cladefs: a cell opened again in the same process lets go of the segments its last cell set
/// aside, full, or left open, holding only what writes after its last commit wrote: they go, as
/// opening the cell would remove them (TASK-79.2).
#[tokio::test(flavor = "multi_thread")]
async fn segments_no_cell_holds_go_as_cells_open() {
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, SMALL_SEGMENTS).unwrap();
    cell.write(extent(1), 0, &pattern(1, 0, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    // Two segments' worth after the commit, the first set aside.
    for sequence in 2..50 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
    }
    drop(cell);
    let cell = LocalCell::open(dir.clone(), SMALL_SEGMENTS).unwrap();
    assert_eq!(cell.extents().unwrap(), [(extent(1), BLOCK as u64)]);
    let segments = dir.list(&of("segments", CELL.id)).unwrap();
    assert_eq!(segments, ["0000000000000"]);
    assert_eq!(read(&cell, extent(1), 0, BLOCK), pattern(1, 0, BLOCK));
}

/// A write in progress reserves the room it may take, which counts against the capacity until it
/// is finished or abandoned; an abandoned write holds no commit back; and the capacity is room
/// enough to the byte (TASK-44.1).
#[tokio::test(flavor = "multi_thread")]
async fn writes_reserve_room_until_done() {
    let cell = create(MemDir::new(), 64 << 10);
    let first = cell.begin_write(extent(1), 0, 24 << 10).await.unwrap();
    error_is(cell.write(extent(2), 0, &[2; 44 << 10]).await, NoSpace);
    drop(first);
    cell.write(extent(2), 0, &[2; 44 << 10]).await.unwrap();
    cell.commit(None).await.unwrap();
    // To the byte: what is left, exactly.
    cell.write(extent(3), 0, &[3; 16 << 10]).await.unwrap();
    cell.write(extent(4), 0, &[4; 4 << 10]).await.unwrap();
    assert_eq!(cell.info().unwrap().available, 0);
    error_is(cell.write(extent(5), 0, &[5; BLOCK]).await, NoSpace);
}

/// Begins a write of `data` at `offset` in `extent`, and writes it to its file, unfinished: a
/// write in flight, which a free may race (TASK-79.1).
async fn in_flight<'a>(
    cell: &'a LocalCell<MemDir>,
    extent: ExtentId,
    offset: u64,
    data: &[u8],
) -> local_cell::Write<'a, MemDir> {
    let write = cell.begin_write(extent, offset, data.len() as u64).await.unwrap();
    write.write(data, 0).unwrap();
    write
}

/// The cell, reopened from `dir` once `cell` is gone: what it made durable.
fn reopened(cell: LocalCell<MemDir>, dir: &MemDir) -> LocalCell<MemDir> {
    drop(cell);
    LocalCell::open(dir.clone(), OPTIONS).unwrap()
}

/// A write in flight to an extent freed meanwhile, which the extent's next write placed in the
/// open segment rather than its file of its own, is dropped as it finishes: the extent holds what
/// was written after the free, and the cell goes on, commits and opens again (TASK-79.1).
#[tokio::test(flavor = "multi_thread")]
async fn writes_finishing_after_their_extents_free_and_rewrite_are_dropped() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    let freed = in_flight(&cell, extent(1), 0, &pattern(1, 0, 40 << 10)).await;
    cell.free(extent(1)).unwrap();
    cell.write(extent(1), BLOCK as u64, &pattern(2, 0, BLOCK)).await.unwrap();
    freed.finish();
    let written = [vec![0; BLOCK], pattern(2, 0, BLOCK)].concat();
    assert!(read(&cell, extent(1), 0, 2 * BLOCK) == written, "the extent reads other data");
    cell.commit(None).await.unwrap();
    let cell = reopened(cell, &dir);
    assert_eq!(cell.extents().unwrap(), [(extent(1), 2 * BLOCK as u64)]);
    assert!(read(&cell, extent(1), 0, 2 * BLOCK) == written, "the extent reads other data");
}

/// A write in flight to an extent freed meanwhile, and not written since, is dropped as it
/// finishes: the cell holds no such extent, commits, and opens again (TASK-79.1).
#[tokio::test(flavor = "multi_thread")]
async fn writes_finishing_after_their_extents_free_are_dropped() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    let freed = in_flight(&cell, extent(1), 0, &pattern(1, 0, 40 << 10)).await;
    cell.free(extent(1)).unwrap();
    freed.finish();
    assert_eq!(cell.extent(extent(1)).unwrap(), None);
    assert_eq!(cell.info().unwrap().used, 0);
    cell.commit(None).await.unwrap();
    let cell = reopened(cell, &dir);
    assert_eq!(cell.extents().unwrap(), []);
    assert_eq!(dir.list(&of("open-extents", CELL.id)).unwrap(), Vec::<String>::new());
    assert_eq!(dir.list(&of("extents", CELL.id)).unwrap(), Vec::<String>::new());
}

/// Extents freed while open, and written anew, are held by the commit that seals them: the footer
/// of its segment names them among the extents it seals, in the segment or in files of their own,
/// and not among those it frees, which readers of footers apply after those, the cell opened
/// again or object storage (TASK-79.1).
#[tokio::test(flavor = "multi_thread")]
async fn footers_hold_extents_freed_open_and_written_anew() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    let lens = [BLOCK, 40 << 10];
    for (sequence, len) in (1..).zip(lens) {
        cell.write(extent(sequence), 0, &pattern(1, 0, len)).await.unwrap();
        cell.free(extent(sequence)).unwrap();
        cell.write(extent(sequence), 0, &pattern(2, 0, len)).await.unwrap();
    }
    let state = closed_state(&cell).await;
    assert_eq!(state.3.len(), 2);
    assert_eq!(reopened_state(cell, &dir).0, state);
}

/// A write in flight to an extent freed meanwhile, written again in the open segment as it was
/// before, lands none of its bytes in the extent written again (TASK-79.1).
#[tokio::test(flavor = "multi_thread")]
async fn writes_finishing_after_their_extents_free_land_nothing_in_its_next_writes() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    let freed = in_flight(&cell, extent(1), 0, &pattern(1, 0, BLOCK)).await;
    cell.free(extent(1)).unwrap();
    cell.write(extent(1), BLOCK as u64, &pattern(2, 0, BLOCK)).await.unwrap();
    freed.finish();
    let written = [vec![0; BLOCK], pattern(2, 0, BLOCK)].concat();
    assert!(read(&cell, extent(1), 0, 2 * BLOCK) == written, "the extent reads other data");
    cell.commit(None).await.unwrap();
    assert_eq!(cell.usage().unwrap().live_bytes, 2 * BLOCK as u64);
    let cell = reopened(cell, &dir);
    assert!(read(&cell, extent(1), 0, 2 * BLOCK) == written, "the extent reads other data");
}

/// An abandoned write to a large extent, never written since, leaves its raw file to no one: the
/// commit removes it, once durable, and the extent may be written anew (TASK-79.5).
#[tokio::test(flavor = "multi_thread")]
async fn abandoned_writes_leave_no_raw_files() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    drop(cell.begin_write(extent(1), 0, 40 << 10).await.unwrap());
    cell.commit(None).await.unwrap();
    assert_eq!(dir.list(&of("open-extents", CELL.id)).unwrap(), Vec::<String>::new());
    cell.write(extent(1), 0, &pattern(1, 0, 40 << 10)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, extent(1), 0, 40 << 10), pattern(1, 0, 40 << 10));
}

/// An abandoned write to a large extent, never written since, then a commit only ordered: the
/// extent may be written anew before the commit's flush, as the guest's next commit is, its raw
/// file gone with the commit's seal, not its flush (TASK-79.5).
#[tokio::test(flavor = "multi_thread")]
async fn abandoned_writes_leave_no_raw_files_to_writes_before_the_flush() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    drop(cell.begin_write(extent(1), 0, 40 << 10).await.unwrap());
    cell.commit_ordered(None).await.unwrap();
    assert_eq!(dir.list(&of("open-extents", CELL.id)).unwrap(), Vec::<String>::new());
    cell.write(extent(1), 0, &pattern(1, 0, 40 << 10)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert!(read(&cell, extent(1), 0, 40 << 10) == pattern(1, 0, 40 << 10), "other data");
    let cell = reopened(cell, &dir);
    assert!(read(&cell, extent(1), 0, 40 << 10) == pattern(1, 0, 40 << 10), "other data");
}

/// A large extent freed while open is written anew, large again, before any commit: the raw file
/// it had, which the free left to the next commit's flush to remove, makes way for another
/// (TASK-79.5).
#[tokio::test(flavor = "multi_thread")]
async fn large_extents_freed_open_are_written_anew_at_once() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, 40 << 10)).await.unwrap();
    cell.free(extent(1)).unwrap();
    cell.write(extent(1), 0, &pattern(2, 0, 40 << 10)).await.unwrap();
    assert!(read(&cell, extent(1), 0, 40 << 10) == pattern(2, 0, 40 << 10), "other data");
    cell.commit(None).await.unwrap();
    assert_eq!(cell.place(extent(1)).unwrap(), Some(Place::Own));
    let cell = reopened(cell, &dir);
    assert!(read(&cell, extent(1), 0, 40 << 10) == pattern(2, 0, 40 << 10), "other data");
    assert_eq!(dir.list(&of("open-extents", CELL.id)).unwrap(), Vec::<String>::new());
}

/// A write in flight to a large extent freed meanwhile, which the extent's next write placed in a
/// file of its own again, is dropped as it finishes: the extent holds what was written after the
/// free, in a raw file of its own, not the one the write in flight wrote to (TASK-79.1,
/// TASK-79.5).
#[tokio::test(flavor = "multi_thread")]
async fn writes_finishing_after_their_large_extents_free_and_rewrite_are_dropped() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    let freed = in_flight(&cell, extent(1), 0, &pattern(1, 0, 40 << 10)).await;
    cell.free(extent(1)).unwrap();
    cell.write(extent(1), 0, &pattern(2, 0, 40 << 10)).await.unwrap();
    freed.finish();
    assert!(read(&cell, extent(1), 0, 40 << 10) == pattern(2, 0, 40 << 10), "other data");
    cell.commit(None).await.unwrap();
    assert_eq!(cell.place(extent(1)).unwrap(), Some(Place::Own));
    let cell = reopened(cell, &dir);
    assert!(read(&cell, extent(1), 0, 40 << 10) == pattern(2, 0, 40 << 10), "other data");
}

/// A cell whose cell failed, opened again in the same process, takes writes to the extents the
/// failed cell had open, whose raw files go as it opens (TASK-79.5).
#[tokio::test(flavor = "multi_thread")]
async fn cells_opened_again_after_a_failure_leave_no_raw_files() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, 40 << 10)).await.unwrap();
    dir.fail_after(Some(0));
    assert_eq!(cell.commit(None).await.unwrap_err().to_string(), "committing the extent store");
    dir.fail_after(None);
    drop(cell);
    let cell = LocalCell::open(dir.clone(), OPTIONS).unwrap();
    assert_eq!(dir.list(&of("open-extents", CELL.id)).unwrap(), Vec::<String>::new());
    cell.write(extent(1), 0, &pattern(2, 0, 40 << 10)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, extent(1), 0, 40 << 10), pattern(2, 0, 40 << 10));
}

/// A cell whose flush failed, opened again in the same process as of its last durable commit,
/// keeps the files that commit's state holds, which the commit not durable let go: they go not as
/// the cell commits again (TASK-79.5).
#[tokio::test(flavor = "multi_thread")]
async fn cells_opened_again_after_a_failed_flush_keep_what_they_hold() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.write(extent(1), 0, &pattern(1, 0, 40 << 10)).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.free(extent(1)).unwrap();
    cell.commit_ordered(None).await.unwrap();
    dir.fail_after(Some(0));
    assert_eq!(cell.flush().await.unwrap_err().to_string(), "committing the extent store");
    dir.fail_after(None);
    drop(cell);
    let cell = LocalCell::open(dir.clone(), OPTIONS).unwrap();
    assert_eq!(cell.last_commit().unwrap(), 1);
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, extent(1), 0, 40 << 10), pattern(1, 0, 40 << 10));
}

/// cladefs: a cell of eight closed segments, an extent in each, which keeps the files of
/// `open_files` of them open.
async fn eight_segments(open_files: usize) -> (MemDir, LocalCell<MemDir>) {
    let dir = MemDir::new();
    let options = Options { open_files, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    for sequence in 0..8 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
        cell.close_segment().unwrap();
        cell.commit(None).await.unwrap();
    }
    settled(&cell).await;
    (dir, cell)
}

/// The files opened, and the bytes read, reading, through `cell`, the extents of a cell
/// [`eight_segments`] made: three again and again, and each of the others once between.
fn opened_reading(dir: &MemDir, cell: &LocalCell<MemDir>) -> (u64, u64) {
    let before = dir.io();
    for other in 3..8 {
        for sequence in [0, 1, 2, other] {
            assert_eq!(read(cell, extent(sequence), 0, BLOCK), pattern(sequence as u8, 0, BLOCK));
        }
    }
    let after = dir.io();
    (after.opened - before.opened, after.read - before.read)
}

/// cladefs: a cell reading what it holds in more closed segments than it keeps the files of open
/// opens each of them once: as one more opens, the least lately read closes,
/// not all of them, which reads would open again; and opening one reads its table and trailer,
/// not its footer, a block, as each part's first read does (TASK-92.2).
#[tokio::test(flavor = "multi_thread")]
async fn closed_segments_files_close_least_lately_read_first() {
    let (dir, cell) = eight_segments(4).await;
    assert_eq!(opened_reading(&dir, &cell), (8, 16 * BLOCK as u64));
}

/// An extent written past its end grows by what the write adds, no more.
#[tokio::test(flavor = "multi_thread")]
async fn extending_an_extent_counts_its_growth() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), 0, &[1; 2 * BLOCK]).await.unwrap();
    cell.write(extent(1), BLOCK as u64, &[2; 2 * BLOCK]).await.unwrap();
    assert_eq!(cell.info().unwrap().used, 3 * BLOCK as u64);
}

/// A shrink counts the room writes in progress reserve, as held.
#[tokio::test(flavor = "multi_thread")]
async fn shrinks_count_writes_in_progress() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), 0, &[1; 4 * BLOCK]).await.unwrap();
    let write = cell.begin_write(extent(2), 0, 2 * BLOCK as u64).await.unwrap();
    let held = 6 * BLOCK as u64;
    let refused = cell.set_capacity(held - BLOCK as u64).await.unwrap_err();
    assert_eq!(
        refused.downcast_ref::<TooSmall>(),
        Some(&TooSmall { capacity: held - BLOCK as u64, held })
    );
    cell.set_capacity(held).await.unwrap();
    drop(write);
}

/// cladefs: ordered commits wait for a flush to make them durable, and say so.
#[tokio::test(flavor = "multi_thread")]
async fn ordered_commits_are_pending_until_flushed() {
    let cell = create(MemDir::new(), 1 << 20);
    assert!(!cell.flush_pending());
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.commit_ordered(None).await.unwrap();
    assert!(cell.flush_pending());
    cell.flush().await.unwrap();
    assert!(!cell.flush_pending());
}

/// Where an extent's data is: one run in a segment, once sealed in one write, cladefs: or in writes
/// one right after another (TASK-92.1); pieces while open or written in several otherwise; its own
/// file if large.
#[tokio::test(flavor = "multi_thread")]
async fn places_of_extents() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), 0, &[1; 2 * BLOCK]).await.unwrap();
    // Its second block first.
    cell.write(extent(2), BLOCK as u64, &[2; BLOCK]).await.unwrap();
    cell.write(extent(2), 0, &[2; BLOCK]).await.unwrap();
    cell.write(extent(3), BLOCK as u64, &[3; BLOCK]).await.unwrap();
    cell.write(extent(4), 0, &[4; 40 << 10]).await.unwrap();
    cell.write(extent(6), 0, &[6; BLOCK]).await.unwrap();
    cell.write(extent(6), BLOCK as u64, &[6; BLOCK]).await.unwrap();
    assert_eq!(
        cell.place(extent(1)).unwrap(),
        Some(Place::Run { cell: CELL.id, number: 0, offset: 0 })
    );
    assert_eq!(cell.place(extent(2)).unwrap(), Some(Place::Pieces));
    // Not from its start.
    assert_eq!(cell.place(extent(3)).unwrap(), Some(Place::Pieces));
    assert_eq!(cell.place(extent(4)).unwrap(), Some(Place::Own));
    assert_eq!(cell.place(extent(5)).unwrap(), None);
    assert_eq!(
        cell.place(extent(6)).unwrap(),
        Some(Place::Run { cell: CELL.id, number: 0, offset: 5 * BLOCK as u64 })
    );
}

/// cladefs: the uploader's lag counts the bytes of the write-ahead log's segments not yet
/// acknowledged, their data and their extents of their own, the open segment's included:
/// acknowledged, they count no more (TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn the_lag_counts_the_logs_bytes_not_acknowledged() {
    let cell = create(MemDir::new(), 1 << 20);
    for (sequence, len) in [(1, BLOCK), (2, 2 * BLOCK), (3, 40 << 10)] {
        cell.write(extent(sequence), 0, &vec![sequence as u8; len]).await.unwrap();
        cell.close_segment().unwrap();
        cell.commit(None).await.unwrap();
    }
    assert_eq!(cell.wal_bytes().unwrap(), 3 * BLOCK as u64 + (40 << 10));
    let closed = cell.closed(usize::MAX).await.unwrap();
    cell.acknowledged(closed[0].number).await.unwrap();
    assert_eq!(cell.wal_bytes().unwrap(), 2 * BLOCK as u64 + (40 << 10));
    // In order only.
    assert!(cell.acknowledged(closed[2].number).await.is_err());
    // The open segment's, committed in it, and on their own.
    cell.write(extent(4), 0, &[4; BLOCK]).await.unwrap();
    cell.write(extent(5), 0, &[5; 40 << 10]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.wal_bytes().unwrap(), 3 * BLOCK as u64 + (80 << 10));
}

/// cladefs: an uploader starting while commits wait to be made durable takes no segment of the
/// commits after it until they are: a crash may lose them here, which the bucket would hold
/// (TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn uploads_started_with_commits_pending_take_only_durable_ones() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.commit_ordered(None).await.unwrap();
    cell.commit_ordered(None).await.unwrap();
    cell.write(extent(2), 0, &[2; BLOCK]).await.unwrap();
    cell.commit_ordered(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    assert!(cell.closed(usize::MAX).await.unwrap().is_empty());
    cell.flush().await.unwrap();
    assert_eq!(closed_one(&cell).await.last(), 3);
}

/// A closed segment's last commit is the commit that closed it.
#[tokio::test(flavor = "multi_thread")]
async fn closed_segments_know_their_last_commit() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.write(extent(2), 0, &[2; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    let closed = cell.closed(usize::MAX).await.unwrap();
    let [closed] = &closed[..] else { panic!("{} segments closed", closed.len()) };
    assert_eq!(closed.last(), 2);
}

/// The raw file of an extent freed while open goes with the next durable commit, though the open
/// segment closed between.
#[tokio::test(flavor = "multi_thread")]
async fn files_freed_between_commits_go_across_closings() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.write(extent(2), 0, &[2; 40 << 10]).await.unwrap();
    cell.free(extent(2)).unwrap();
    assert!(cell.close_now().await.unwrap());
    // Nothing committed since: nothing to close.
    assert!(!cell.close_now().await.unwrap());
    cell.commit(None).await.unwrap();
    assert_eq!(dir.list(&of("open-extents", CELL.id)).unwrap(), Vec::<String>::new());
}

/// A commit lists an extent it wrote in two writes once, at its length: its segment's footer names
/// it so, and the bytes it wrote count it so.
#[tokio::test(flavor = "multi_thread")]
async fn commits_list_each_extent_written_once() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.write(extent(1), BLOCK as u64, &[1; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert!(cell.close_now().await.unwrap());
    assert_eq!(cell.wal_bytes().unwrap(), 2 * BLOCK as u64);
    let written = closed_one(&cell).await.footer.contents.extents;
    assert_eq!(
        written.iter().map(|&(extent, _, len)| (extent, len)).collect::<Vec<_>>(),
        [(extent(1), 2 * BLOCK as u64)]
    );
}

/// With segments closing as soon as they are old, an empty one closes not: a commit that wrote
/// nothing leaves no segment.
#[tokio::test(flavor = "multi_thread")]
async fn empty_segments_close_not() {
    let options = Options { segment_age: std::time::Duration::ZERO, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let cell = LocalCell::create(MemDir::new(), description, options).unwrap();
    cell.commit(None).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.closed(usize::MAX).await.unwrap().len(), 0);
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.closed(usize::MAX).await.unwrap().len(), 1);
}

/// Sealing an extent written in pieces moves its data to one run: what is live in the segments is
/// its length, once.
#[tokio::test(flavor = "multi_thread")]
async fn sealing_moves_pieces_live_once() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), BLOCK as u64, &[1; BLOCK]).await.unwrap();
    cell.write(extent(1), 0, &[2; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.usage().unwrap().live_bytes, 2 * BLOCK as u64);
    assert_eq!(read(&cell, extent(1), 0, 2 * BLOCK), [[2; BLOCK], [1; BLOCK]].concat());
}

/// cladefs: an extent written in runs one after another in the open segment, as the guest's device
/// splits a write larger than its most I/O (1008 KiB) in two, is one run, which its commit seals
/// where it is, reading and writing none of it again, whichever of its writes finished first; and
/// is so once opened again (TASK-92.1).
#[tokio::test(flavor = "multi_thread")]
async fn extents_written_in_runs_seal_where_they_are() {
    const MIB: usize = 1 << 20;
    const MAX_IO: usize = 1008 << 10;
    let options = Options { segment_size: 32 << 20, large_extent: 4 << 20, ..OPTIONS };
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, options).unwrap();
    let data = |sequence| pattern(sequence as u8, 0, MIB);
    cell.write(extent(1), 0, &data(1)[..MAX_IO]).await.unwrap();
    cell.write(extent(1), MAX_IO as u64, &data(1)[MAX_IO..]).await.unwrap();
    let first = in_flight(&cell, extent(2), 0, &data(2)[..MAX_IO]).await;
    let second = in_flight(&cell, extent(2), MAX_IO as u64, &data(2)[MAX_IO..]).await;
    second.finish();
    first.finish();
    // cladefs: the commit's I/O alone: not the framer's reads of the parts written whole, looking
    // at them ahead as the segment fills, which the writes queued, and which may land in the commit
    // otherwise (TASK-400, TASK-423).
    cell.wait_looked_ahead().await;
    let before = dir.io();
    cell.commit_ordered(None).await.unwrap();
    let sealed = dir.io();
    assert_eq!((sealed.read - before.read, sealed.written - before.written), (0, 0));
    let runs = [(1, 0), (2, MIB as u64)].map(|(sequence, offset)| {
        (extent(sequence), MIB as u64, Place::Run { cell: CELL.id, number: 0, offset })
    });
    assert_eq!(cell.places().unwrap(), runs);
    cell.flush().await.unwrap();
    drop(cell);
    let cell = LocalCell::open(dir, options).unwrap();
    assert_eq!(cell.places().unwrap(), runs);
    for sequence in 1..=2 {
        assert!(read(&cell, extent(sequence), 0, MIB) == data(sequence), "{sequence} differs");
    }
}

/// cladefs: once `LocalCell::close` returns, the cell reads its directory no more: a look ahead
/// reading a part written whole when the cell closed (on the framer's thread, holding no lock,
/// TASK-400) ends before close returns. It read on after, and a cell opened on the directory
/// meanwhile counted up to a part of the closed cell's reads as its opening's (TASK-426).
#[tokio::test(flavor = "multi_thread")]
async fn close_waits_for_a_look_ahead_reading() {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::sync::{Mutex, atomic::AtomicBool, atomic::Ordering};
    const MIB: usize = 1 << 20;
    let options = Options { segment_size: 32 << 20, large_extent: 4 << 20, ..OPTIONS };
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell = Arc::new(LocalCell::create(MemDir::new(), description, options).unwrap());
    let (reading, looking) = channel();
    let (go, gone) = channel::<()>();
    let (reading, gone) = (Mutex::new(reading), Mutex::new(gone));
    let released = Arc::new(AtomicBool::new(false));
    let held = fail::FailGuard::with_callback("cell::looking_ahead", move || {
        let _ = reading.lock().unwrap().send(());
        let _ = gone.lock().unwrap().recv();
    })
    .unwrap();
    // Its first parts, written whole: looks ahead at them queued, the first reading, held.
    cell.write(extent(1), 0, &pattern(1, 0, 2 * MIB)).await.unwrap();
    looking.recv().unwrap();
    let (closed, closing) = channel();
    let closer = tokio::spawn({
        let (cell, released) = (cell.clone(), released.clone());
        async move {
            cell.close().await.unwrap();
            closed.send(released.load(Ordering::SeqCst)).unwrap();
        }
    });
    let timeout = std::time::Duration::from_millis(500);
    assert_eq!(closing.recv_timeout(timeout), Err(RecvTimeoutError::Timeout), "closed, reading");
    released.store(true, Ordering::SeqCst);
    drop((go, held));
    assert_eq!(closing.recv(), Ok(true), "close returned once the look ahead read");
    closer.await.unwrap();
}

/// An extent larger than the part sealing frames at once (16 MiB) is framed whole.
#[tokio::test(flavor = "multi_thread")]
async fn extents_larger_than_a_framing_chunk_seal_whole() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 64 << 20);
    let data = pattern(9, 0, (17 << 20) + BLOCK);
    cell.write(extent(1), 0, &data).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.usage().unwrap().own_bytes, data.len() as u64);
    // Its file frames its data, no more.
    let file = dir.open(&format!("{}/{}", of("extents", CELL.id), Padded(1)), true).unwrap();
    let mut framed = vec![0; file.size().unwrap() as usize];
    file.read_at(&mut framed, 0).unwrap();
    assert_eq!(
        extent_frame::data(&framed, &bucket_cell::testing::ring([CELL]).of(CELL.id)).unwrap().len(),
        data.len()
    );
    drop(cell);
    let cell = LocalCell::open(dir, OPTIONS).unwrap();
    assert!(read(&cell, extent(1), 0, data.len()) == data, "the extent reads other data");
}

/// Zero bytes are no write.
#[tokio::test(flavor = "multi_thread")]
async fn zero_byte_writes_are_refused() {
    let cell = create(MemDir::new(), 1 << 20);
    assert!(cell.write(extent(1), 0, &[]).await.is_err());
}

/// A write, or a read, ends within the most bytes an extent holds, doc-4's `max_extent`: one past
/// it, or so far that its end wraps, fails, and makes no extent, which its seal and its uploader
/// would hold whole in memory, however long the guest made it (TASK-79.2).
#[tokio::test(flavor = "multi_thread")]
async fn io_ends_within_the_most_an_extent_holds() {
    let cell = create(MemDir::new(), 1 << 40);
    let refused = |result: Result<(), anyhow::Error>| result.expect_err("succeeded").to_string();
    let past = refused(cell.write(extent(1), MAX_EXTENT, &[1; BLOCK]).await);
    assert_eq!(past, format!("{BLOCK} bytes at {MAX_EXTENT}: past the most an extent holds"));
    let too_long = TooLong { offset: MAX_EXTENT, len: BLOCK as u64 };
    error_is(cell.write(extent(1), MAX_EXTENT, &[1; BLOCK]).await, too_long);
    // Its end past the largest offset.
    let wrapping = u64::MAX - (BLOCK as u64 - 1);
    let wrapped = refused(cell.write(extent(1), wrapping, &[1; 2 * BLOCK]).await);
    let expected = format!("{} bytes at {wrapping}: past the most an extent holds", 2 * BLOCK);
    assert_eq!(wrapped, expected);
    assert_eq!(refused(cell.read(extent(1), wrapping, &mut [0; 2 * BLOCK])), expected);
    assert_eq!(cell.extents().unwrap(), []);
    // To its end, the most.
    cell.write(extent(1), MAX_EXTENT - BLOCK as u64, &[1; BLOCK]).await.unwrap();
    assert_eq!(cell.extents().unwrap(), [(extent(1), MAX_EXTENT)]);
}

/// cladefs: what goes to the open segment is whole sectors of 4 KiB, which it is to be encrypted in
/// (doc-16, TASK-411.8): a write of an extent stored in segments at an offset, or of a length, that
/// is not, though whole blocks of a cell of smaller blocks, is refused, writing nothing; a large
/// extent, in a file of its own, takes it.
#[tokio::test(flavor = "multi_thread")]
async fn writes_to_segments_are_whole_sectors() {
    let description = Description { block_size: 512, capacity: 1 << 30, cell: CELL };
    let cell = LocalCell::create(MemDir::new(), description, OPTIONS).unwrap();
    let refused = async |offset: u64, len: usize| {
        let error = cell.write(extent(1), offset, &vec![1; len]).await.expect_err("succeeded");
        (error.downcast_ref::<Unsectored>().copied(), error.to_string())
    };
    let message = |offset, len| {
        format!(
            "{len} bytes at {offset}: not whole sectors of 4096 bytes, as an open segment takes"
        )
    };
    let at = Unsectored { offset: 512, len: BLOCK as u64 };
    assert_eq!(refused(512, BLOCK).await, (Some(at), message(512, BLOCK)));
    let of = Unsectored { offset: 0, len: BLOCK as u64 + 512 };
    assert_eq!(refused(0, BLOCK + 512).await, (Some(of), message(0, BLOCK + 512)));
    assert_eq!(cell.extents().unwrap(), []);
    // Whole sectors go where the first would have, at the open segment's start.
    cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(
        cell.place(extent(1)).unwrap(),
        Some(Place::Run { cell: CELL.id, number: 0, offset: 0 })
    );
    // Large, in a file of its own: not a segment's; whole sectors too, as its raw file, encrypted,
    // takes them alone (TASK-411.5).
    let large = OPTIONS.large_extent as usize + BLOCK;
    let unsectored = cell.write(extent(2), 0, &vec![2; large + 512]).await.expect_err("succeeded");
    let at = Unsectored { offset: 0, len: large as u64 + 512 };
    assert_eq!(unsectored.downcast_ref::<Unsectored>().copied(), Some(at));
    cell.write(extent(2), 0, &vec![2; large]).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(cell.place(extent(2)).unwrap(), Some(Place::Own));
    assert_eq!(read(&cell, extent(2), 0, large), vec![2; large]);
}

/// What is read past an extent's end, or where nothing was written, reads as zeros, whatever the
/// buffer held.
#[tokio::test(flavor = "multi_thread")]
async fn reads_past_an_extents_end_are_zeros() {
    let cell = create(MemDir::new(), 1 << 20);
    cell.write(extent(1), BLOCK as u64, &[1; BLOCK]).await.unwrap();
    cell.write(extent(2), 0, &[2; 40 << 10]).await.unwrap();
    for (extent, len) in [(extent(1), 2 * BLOCK), (extent(2), 40 << 10)] {
        let mut data = vec![0xff; len + 2 * BLOCK];
        cell.read(extent, 0, &mut data).unwrap();
        assert!(data[len..].iter().all(|byte| *byte == 0), "past {extent:?}'s end");
    }
    let mut data = vec![0xff; 3 * BLOCK];
    cell.read(extent(1), 0, &mut data).unwrap();
    assert_eq!(data, [vec![0; BLOCK], vec![1; BLOCK], vec![0; BLOCK]].concat());
}

/// A commit that fails part way, which leaves the cell's state ahead of its files, fails the cell:
/// it does nothing more, though its file system works again. Here the file system fails the
/// commit's first change to it: the creation of the next open segment's file as the commit closes
/// the last. cladefs: not that of the index file a checkpoint begins, which the commit's flush
/// makes (`checkpoints_failing_in_their_flushes_fail_the_cell`, TASK-92.56). Nor a seal's copy of
/// an extent written in two writes, its second block first, which are not one run: the copy lands
/// in the cell's page cache, the file system untouched, and the flush after fails (TASK-433).
#[tokio::test(flavor = "multi_thread")]
async fn commits_failing_part_way_fail_the_cell() {
    let created = || {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = LocalCell::create(dir.clone(), description, OPTIONS).unwrap();
        (dir, cell)
    };
    let sealing = async || {
        let (dir, cell) = created();
        cell.write(extent(1), BLOCK as u64, &[2; BLOCK]).await.unwrap();
        cell.write(extent(1), 0, &[1; BLOCK]).await.unwrap();
        (dir, cell)
    };
    let closing = || {
        let (dir, cell) = created();
        cell.close_segment().unwrap();
        (dir, cell)
    };
    let failure = |result: Result<(), anyhow::Error>| result.unwrap_err().to_string();
    let (dir, cell) = sealing().await;
    dir.fail_after(Some(0));
    cell.commit_ordered(None).await.unwrap();
    assert_eq!(failure(cell.flush().await), "committing the extent store");
    for budget in [Some(0), None] {
        dir.fail_after(budget);
        let failed = "the extent store failed: committing the extent store: appending commits' \
                      records: the file system crashed";
        assert_eq!(failure(cell.commit(None).await), failed);
        assert_eq!(failure(cell.read(extent(1), 0, &mut [0; BLOCK])), failed);
    }
    for (dir, cell) in [closing()] {
        dir.fail_after(Some(0));
        let failed = cell.commit_ordered(None).await.unwrap_err();
        assert_eq!(failed.to_string(), "committing the extent store");
        for budget in [Some(0), None] {
            dir.fail_after(budget);
            // cladefs: saying why (TASK-165.9).
            let failed = "the extent store failed: the file system crashed";
            assert_eq!(failure(cell.commit(None).await), failed);
            assert_eq!(failure(cell.read(extent(1), 0, &mut [0; BLOCK])), failed);
            assert_eq!(failure(cell.last_commit().map(drop)), failed);
        }
    }
}

/// cladefs: an ordered commit's record waits for its flush, which appends it to the open segment's
/// log: a crash before leaves the cell as of the commit before; its flush failing fails the
/// cell, as a flush failing does, its state ahead of its files (TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn logs_failing_in_their_flushes_fail_the_cell() {
    let dir = MemDir::new();
    let cell = create(dir.clone(), 1 << 20);
    cell.commit_ordered(Some(b"ordered")).await.unwrap();
    assert_eq!(LocalCell::open(dir.crash(0), OPTIONS).unwrap().last_commit().unwrap(), 0);
    dir.fail_after(Some(0));
    let failure = |result: Result<(), anyhow::Error>| result.unwrap_err().to_string();
    assert_eq!(failure(cell.flush().await), "committing the extent store");
    dir.fail_after(None);
    let failed = "the extent store failed: committing the extent store: appending commits' \
                  records: the file system crashed";
    assert_eq!(failure(cell.commit(None).await), failed);
    assert_eq!(failure(cell.flush().await), failed);
}

/// Parts stored compressed are unpacked once while they are kept unpacked, those read last.
/// cladefs: as many as the process keeps, 64 MiB of them, every cell's together, which it counts
/// (TASK-603).
#[tokio::test(flavor = "multi_thread")]
async fn parts_read_again_are_unpacked_once() {
    const PART: usize = 1 << 20;
    const PARTS: usize = 80;
    let description = Description { block_size: BLOCK as u32, capacity: 128 << 20, cell: CELL };
    let cell = LocalCell::create(MemDir::new(), description, OPTIONS).unwrap();
    // Two extents of half of them each, as an extent holds 64 MiB at most.
    const HALF: usize = PARTS / 2;
    cell.write(extent(1), 0, &pattern(1, 0, HALF * PART)).await.unwrap();
    cell.write(extent(2), 0, &pattern(1, HALF * PART, HALF * PART)).await.unwrap();
    cell.commit(None).await.unwrap();
    let read_part = |part: usize| {
        let (sequence, within) = (1 + (part / HALF) as u64, part % HALF);
        read(&cell, extent(sequence), (within * PART) as u64, BLOCK)
    };
    let unpacks = || local_cell::unpacks();
    let before = unpacks();
    for part in 0..PARTS {
        assert_eq!(read_part(part), pattern(1, part * PART, BLOCK));
    }
    assert_eq!(unpacks() - before, PARTS as u64);
    let kept = local_cell::unpacked().bytes();
    assert!((60 << 20..=64 << 20).contains(&kept), "{kept} bytes kept unpacked");
    // The 32 read last kept: none unpacked again.
    for part in PARTS - 32..PARTS {
        read_part(part);
    }
    assert_eq!(unpacks() - before, PARTS as u64);
    // The first, long gone, again.
    read_part(0);
    assert_eq!(unpacks() - before, PARTS as u64 + 1);
}

/// cladefs: a cell's bytes, and its extents, are its cell's own, of the extents it wrote, or
/// inherited from the cells it was forked from, which it holds in the bucket (TASK-165.7), as
/// they write, grow and free, and opened again (TASK-49); the extents counted as they come and go
/// (TASK-454).
#[tokio::test(flavor = "multi_thread")]
async fn cells_tell_their_own_bytes_from_those_inherited() {
    let parent = CELL.id + 1;
    let dir = MemDir::new();
    let mut state = bucket_cell::State::new(CELL);
    for (sequence, blocks) in [(1, 2), (2, 3)] {
        let key = "segments/0000000000008/0000000000001";
        let location = bucket_location(key, sequence * 8 * BLOCK as u64, blocks * BLOCK as u64);
        state.extents.insert(ExtentId::new(parent, sequence), location);
    }
    // Own bytes and inherited, own extents and inherited.
    let held = |cell: &LocalCell<MemDir>| {
        let info = cell.info().unwrap();
        (info.own, info.inherited, info.own_extents, info.inherited_extents)
    };
    let block = BLOCK as u64;
    let cell = created_on(dir.clone(), 1 << 20, state.clone(), WRITER);
    assert_eq!(held(&cell), (0, 5 * block, 0, 2));
    cell.write(extent(1), 0, &[3; BLOCK]).await.unwrap();
    cell.write(extent(2), 0, &[4; 40 << 10]).await.unwrap();
    // Written again, and grown: no more extents.
    cell.write(extent(1), 0, &[5; BLOCK]).await.unwrap();
    cell.write(extent(2), 40 << 10, &[6; BLOCK]).await.unwrap();
    assert_eq!(held(&cell), (block + (44 << 10), 5 * block, 2, 2));
    cell.free(ExtentId::new(parent, 1)).unwrap();
    cell.free(extent(2)).unwrap();
    // Freed again: held no more already.
    cell.free(extent(2)).unwrap();
    assert_eq!(held(&cell), (block, 3 * block, 1, 1));
    cell.commit(None).await.unwrap();
    assert_eq!(held(&cell), (block, 3 * block, 1, 1));
    drop(cell);
    assert_eq!(held(&opened_on(dir, state, WRITER)), (block, 3 * block, 1, 1));
}

/// TASK-295: a write-ahead log dropped, as `cladefsctl take` drops one another writer's commits
/// moved the cell on from, stays dropped whatever a crash after it keeps: the cell is
/// what it committed since, and none of the log's.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_log_stays_dropped_through_a_crash() {
    for seed in 0..64 {
        let dir = MemDir::new();
        let cell = create(dir.clone(), 64 << 20);
        for sequence in 1..=2 {
            cell.write(extent(sequence), 0, &pattern(sequence as u8, 0, BLOCK)).await.unwrap();
            cell.close_segment().unwrap();
            cell.commit(None).await.unwrap();
        }
        drop(cell);
        let mut recovering = LocalCell::recover(dir.clone()).unwrap();
        recovering.drop_wal().unwrap();
        // Its commit in its open segment: no segment framed since, whose directory's sync would
        // make the drop's removals durable too.
        let cell = recovering.open(OPTIONS, Opening::new(CELL)).unwrap();
        cell.write(extent(3), 0, &pattern(3, 0, BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
        let crashed = dir.crash(seed);
        drop(cell);
        let reopened = LocalCell::open(crashed, OPTIONS).unwrap();
        assert_eq!(reopened.extents().unwrap(), [(extent(3), BLOCK as u64)], "seed {seed}");
    }
}

/// TASK-359: a cell whose directory is on another filesystem than the host's cache is refused
/// as it opens: what the bucket acknowledged goes into the cache by renames, which can't cross
/// filesystems, and every read of it would go to the bucket. On Linux, /dev/shm (tmpfs) and the
/// target directory (on disk) are two.
#[cfg(target_os = "linux")]
#[test]
fn cells_elsewhere_than_the_hosts_cache_are_refused() {
    use std::os::unix::fs::MetadataExt as _;
    let cached = tempfile::tempdir_in("/dev/shm").unwrap();
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let device = |path: &std::path::Path| std::fs::metadata(path).unwrap().dev();
    assert_ne!(device(cached.path()), device(temp.path()), "two filesystems");
    let cache = Arc::new(bucket_cell::cache::Cache::open(cached.path(), 1 << 20).unwrap());
    let cache_dir = cache.dir().to_owned();
    let dir = HostDir::new(temp.path()).unwrap();
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    LocalCell::make(&dir, description, None, None).unwrap();
    let opening = Opening { cache: Some(cache), ..Opening::new(CELL) };
    let refused = LocalCell::recover_unmade(dir).unwrap().open(OPTIONS, opening).err().unwrap();
    assert_eq!(
        refused.to_string(),
        format!(
            "the cell is on another filesystem than the host's cache at {}: the cell's \
             segments go into the cache by renaming them",
            cache_dir.display()
        )
    );
}

/// board 11380: a flush sealing more than `CHECKED_MOST` (8 MiB) without own-extent files takes
/// the `Runs::Synced` path instead of `Runs::Checked`, avoiding multi-gigabyte `O_DIRECT`
/// read-backs during bulk commits while preserving crash recovery on reopen. Under the default
/// options, as its extents assume: `OPTIONS`' 32 KiB `large_extent` put every 1 MiB extent in a
/// file of its own, no run in any segment, and it passed checking none, until board 12555 sent
/// those past `open_files` to segments, which their seal copies out of.
#[tokio::test(flavor = "multi_thread")]
async fn large_flushes_use_runs_synced_without_reading_raw_segments_back() {
    let defaults = Options { max_extent: MAX_EXTENT, ..Options::default() };
    let dir = Counting::new();
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    let cell =
        host_fs::may_block(|| LocalCell::create(dir.clone(), description, defaults).unwrap());
    // Nine 1 MiB extents (> 8 MiB CHECKED_MOST, each < 4 MiB large_extent so none uses own files).
    let chunk = 1 << 20;
    for seq in 1..=9u64 {
        cell.write(extent(seq), 0, &pattern(seq as u8, 0, chunk)).await.unwrap();
    }
    // The commit's reads alone: not the framer's of the nine parts the writes filled, looking at
    // each ahead as the segment fills (TASK-400), which may land in the commit otherwise: one did,
    // a MiB, in most runs (board 13403).
    cell.wait_looked_ahead().await;
    let before = dir.read().get("open-segments").copied().unwrap_or(0);
    cell.commit(None).await.unwrap();
    let after = dir.read().get("open-segments").copied().unwrap_or(0);
    // Runs::Checked would read all nine back.
    assert_eq!(
        after - before,
        0,
        "large flush should use Runs::Synced with no raw-segment read-back, read {} bytes",
        after - before
    );
    drop(cell);
    let reopened = LocalCell::open(dir.dir, defaults).unwrap();
    for seq in 1..=9u64 {
        assert_eq!(read(&reopened, extent(seq), 0, BLOCK), pattern(seq as u8, 0, BLOCK));
    }
}
