// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An ephemeral cell (doc-25, TASK-698.2): writes, frees, checkpoints, givings back,
//! reopenings and crashes, over a parent's version whose extents are read in the bucket.

use super::*;
use crate::MemDir;
use extent_frame::segment::tag;
use storage_device::NoExtent;

const BLOCK: usize = 4096;
const PARENT: CellId = bucket_cell::layout::test_cell(1);
const CHILD: CellId = bucket_cell::layout::test_cell(2);

/// The parent's extent, read in the bucket.
fn inherited() -> ExtentId {
    ExtentId::new(PARENT.id, 1)
}

/// The child's own extent `sequence`.
fn own(sequence: u64) -> ExtentId {
    ExtentId::new(CHILD.id, sequence)
}

/// The parent's state at the version: one extent of its own, its root and capacity, its last
/// commit its writer's.
fn parent() -> Arc<bucket_cell::State> {
    let mut state = bucket_cell::State::new(PARENT);
    let location = Location { key: "parent/segment".to_owned(), offset: 0, len: BLOCK as u64 };
    state.extents.insert(inherited(), location);
    state.root = Some(b"parent's root".to_vec());
    state.capacity = Some(1 << 30);
    state.last_commit = Some(tag(3, 10));
    Arc::new(state)
}

/// Every commit with data closes its segment.
fn options() -> Options {
    Options { segment_size: BLOCK as u64, ..Options::default() }
}

fn keys() -> Arc<Ring> {
    Ring::writing_under(CHILD, extent_frame::Secret::from_bytes(&UNKEPT_DEK))
}

/// A new ephemeral cell in `dir`, of the child's, forked from the parent's version.
fn make(dir: &MemDir) {
    let checkpoint = LocalCheckpoint::fork(
        Parent { cell: PARENT, version: 7, at: 3 },
        &parent(),
        b"wrapped".to_vec(),
    );
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 30, cell: CHILD };
    LocalCell::make_ephemeral(dir, description, Some("memory:///p"), Some(Kind::Fxfs), &checkpoint)
        .unwrap();
}

/// The ephemeral cell in `dir`, opened.
fn open(dir: &MemDir) -> LocalCell<MemDir> {
    let recovering = LocalCell::recover_unmade(dir.clone()).unwrap();
    assert!(recovering.is_ephemeral());
    assert_eq!(recovering.local_checkpoint().unwrap().keys(), b"wrapped");
    let opening = EphemeralOpening { parent: parent(), keys: keys(), cache: None, held: None };
    let cell = recovering.open_ephemeral(options(), opening).unwrap();
    cell.made().unwrap();
    cell
}

/// Writes and commits the child's extent `sequence`, a block of `byte`s.
async fn commit(cell: &LocalCell<MemDir>, sequence: u64, byte: u8) {
    cell.write(own(sequence), 0, &[byte; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
}

/// What the child's extent `sequence` holds: a block of one byte, or none.
fn byte_of(cell: &LocalCell<MemDir>, sequence: u64) -> Option<u8> {
    let mut data = [0; BLOCK];
    match cell.read(own(sequence), 0, &mut data) {
        Ok(()) => {
            assert!(data.iter().all(|&byte| byte == data[0]), "extent {sequence} torn");
            Some(data[0])
        }
        Err(err) if err.is::<NoExtent>() => None,
        Err(err) => panic!("reading extent {sequence}: {err:#}"),
    }
}

/// The numbers of the files in `kind`'s directory of the child's.
fn numbers(dir: &MemDir, kind: &'static str) -> Vec<u64> {
    list_numbers(dir, &cell_dir(kind, CHILD.id)).unwrap()
}

/// The bytes the child's kept segment `number` takes of its disk.
fn allocated(dir: &MemDir, number: u64) -> u64 {
    let path = kept_path(SegmentId { cell: CHILD.id, number });
    dir.open(&path, true).unwrap().allocated().unwrap()
}

/// An ephemeral cell reads its parent's extents in the bucket, and its own here; its segments
/// close raw, never framed, and a checkpoint covers them once eight are closed, moving them to
/// `kept/`, its local checkpoint naming them; it reopens as it was.
#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_cell_keeps_its_segments_raw_and_checkpoints_them() {
    let dir = MemDir::new();
    make(&dir);
    let cell = open(&dir);
    assert!(cell.is_ephemeral());
    assert_eq!(cell.root().unwrap(), b"parent's root");
    let mut data = [0; BLOCK];
    let read = cell.read(inherited(), 0, &mut data).unwrap_err();
    let in_bucket = read.downcast::<BucketData>().unwrap();
    assert_eq!(in_bucket.location.key, "parent/segment");
    for sequence in 0..7 {
        commit(&cell, sequence, sequence as u8 + 1).await;
    }
    assert_eq!(numbers(&dir, SEGMENTS), Vec::<u64>::new(), "nothing framed");
    assert_eq!(numbers(&dir, KEPT), Vec::<u64>::new(), "seven closed: no checkpoint yet");
    // Those of the seven closed and the open one's; the next one's, if prepared ahead yet.
    let logs = numbers(&dir, LOGS);
    assert_eq!(logs[..8], (0..8).collect::<Vec<_>>(), "{logs:?}");
    commit(&cell, 7, 8).await;
    assert_eq!(numbers(&dir, KEPT), (0..8).collect::<Vec<_>>(), "eight closed: covered");
    for kind in [LOGS, OPEN_SEGMENTS] {
        let left = numbers(&dir, kind);
        assert_eq!(left[0], 8, "the open segment's, and the next's if prepared: {left:?}");
    }
    let checkpoint = LocalCheckpoint::read(&dir).unwrap();
    assert_eq!((checkpoint.through, checkpoint.own.len(), checkpoint.kept.len()), (Some(7), 8, 8));
    assert_eq!(checkpoint.last_commit, Some(tag(0, 18)), "the parent's commit 10, then eight");
    for sequence in 0..8 {
        assert_eq!(byte_of(&cell, sequence), Some(sequence as u8 + 1));
    }
    drop(cell);

    let cell = open(&dir);
    for sequence in 0..8 {
        assert_eq!(byte_of(&cell, sequence), Some(sequence as u8 + 1));
    }
    assert_eq!(cell.read(inherited(), 0, &mut data).unwrap_err().is::<BucketData>(), true);
    assert_eq!(cell.last_commit().unwrap(), 18);
}

/// What a checkpoint covers of frees gives back the runs of the extents freed, in their kept
/// segments, which then take less of the disk; a kept segment no extent is in any more goes; a
/// freed extent of the parent's is the bucket's no more.
#[tokio::test(flavor = "multi_thread")]
async fn checkpoints_give_back_what_was_freed() {
    let dir = MemDir::new();
    make(&dir);
    let cell = open(&dir);
    for sequence in 0..8 {
        commit(&cell, sequence, 1).await;
    }
    assert_eq!(numbers(&dir, KEPT), (0..8).collect::<Vec<_>>());
    let whole = allocated(&dir, 0);
    assert!(whole >= BLOCK as u64, "a kept segment holds its block: {whole}");
    cell.free(own(0)).unwrap();
    cell.free(inherited()).unwrap();
    cell.commit(None).await.unwrap();
    // Eight more closed, the first with the commit freeing, all covered by the next checkpoint.
    for sequence in 8..16 {
        commit(&cell, sequence, 2).await;
    }
    assert_eq!(byte_of(&cell, 0), None);
    assert!(!numbers(&dir, KEPT).contains(&0), "segment 0 held extent 0 alone: it goes");
    let checkpoint = LocalCheckpoint::read(&dir).unwrap();
    assert_eq!(checkpoint.freed, vec![inherited()]);
    assert!(checkpoint.own.iter().all(|(extent, _)| *extent != own(0)));
    drop(cell);

    let cell = open(&dir);
    let mut data = [0; BLOCK];
    let read = cell.read(inherited(), 0, &mut data).unwrap_err();
    assert!(read.is::<NoExtent>(), "freed, the parent's extent is held no more: {read:#}");
    for sequence in 1..16 {
        assert_eq!(byte_of(&cell, sequence), Some(if sequence < 8 { 1 } else { 2 }));
    }
}

/// A run freed in a kept segment that still holds others is given back alone: the segment takes
/// less of the disk, and its other extents read as they were.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_segment_gives_back_a_run_alone() {
    let dir = MemDir::new();
    make(&dir);
    let options = Options { segment_size: 4 * BLOCK as u64, ..Options::default() };
    let recovering = LocalCell::recover_unmade(dir.clone()).unwrap();
    let opening = EphemeralOpening { parent: parent(), keys: keys(), cache: None, held: None };
    let cell = recovering.open_ephemeral(options, opening).unwrap();
    cell.made().unwrap();
    // Four extents a segment, eight segments.
    for sequence in 0..32 {
        commit(&cell, sequence, sequence as u8).await;
    }
    assert!(numbers(&dir, KEPT).contains(&0));
    let before = allocated(&dir, 0);
    cell.free(own(1)).unwrap();
    cell.commit(None).await.unwrap();
    for sequence in 32..64 {
        commit(&cell, sequence, sequence as u8).await;
    }
    let after = allocated(&dir, 0);
    assert_eq!(after, before - BLOCK as u64, "extent 1's run given back");
    for sequence in [0, 2, 3] {
        assert_eq!(byte_of(&cell, sequence), Some(sequence as u8));
    }
    assert_eq!(byte_of(&cell, 1), None);
}

/// Closing it covers every closed segment, however few: its next opening replays the open
/// segment's log alone.
#[tokio::test(flavor = "multi_thread")]
async fn closing_covers_every_closed_segment() {
    let dir = MemDir::new();
    make(&dir);
    let cell = open(&dir);
    for sequence in 0..3 {
        commit(&cell, sequence, 9).await;
    }
    assert_eq!(numbers(&dir, KEPT), Vec::<u64>::new());
    drop(cell);
    assert_eq!(numbers(&dir, KEPT), vec![0, 1, 2]);
    let cell = open(&dir);
    for sequence in 0..3 {
        assert_eq!(byte_of(&cell, sequence), Some(9));
    }
}

/// A crash at any step of the commits, closings and checkpoints keeps every commit made local:
/// the disk fails after `n` operations, for every `n` through a run of checkpoints, the directory
/// is left as a crash would leave it, and the cell opened again holds each extent whose commit
/// returned, and none torn.
#[tokio::test(flavor = "multi_thread")]
async fn crashes_at_every_step_keep_what_was_committed() {
    for fail_after in 1..400 {
        let dir = MemDir::new();
        make(&dir);
        let cell = open(&dir);
        dir.fail_after(Some(fail_after));
        let mut committed = BTreeMap::new();
        for sequence in 0..20u64 {
            let written = cell.write(own(sequence), 0, &[sequence as u8 + 1; BLOCK]).await;
            let freed = match sequence {
                12 => cell.free(own(3)),
                _ => Ok(()),
            };
            // A commit that failed may be local or not: what it changed is either way.
            if sequence == 12 {
                committed.remove(&3);
            }
            let changed = match written.and(freed) {
                Ok(()) => cell.commit(None).await,
                failed => failed,
            };
            if changed.is_err() {
                break;
            }
            committed.insert(sequence, sequence as u8 + 1);
        }
        drop(cell);
        let crashed = dir.crash(fail_after);
        crashed.fail_after(None);
        let cell = open(&crashed);
        for (&sequence, &byte) in &committed {
            assert_eq!(byte_of(&cell, sequence), Some(byte), "after {fail_after}: {sequence}");
        }
        let mut data = [0; BLOCK];
        let parent = cell.read(inherited(), 0, &mut data).unwrap_err();
        assert!(parent.is::<BucketData>(), "after {fail_after}: {parent:#}");
    }
}
