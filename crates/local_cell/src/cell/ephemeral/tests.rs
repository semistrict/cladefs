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
        Parent { cell: PARENT, version: 7, at: 3, epoch: 1 },
        &parent(),
        b"wrapped".to_vec(),
    );
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 30, cell: CHILD };
    let ring = keys();
    LocalCell::make_ephemeral(
        dir,
        description,
        Some("memory:///p"),
        Some(Kind::Fxfs),
        &checkpoint,
        ring.encrypting(),
        |_| Ok(()),
    )
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

/// The bytes the child's segment `number` takes of its disk.
fn allocated(dir: &MemDir, number: u64) -> u64 {
    let path = segment_path(SegmentId { cell: CHILD.id, number });
    dir.open(&path, true).unwrap().allocated().unwrap()
}

/// An ephemeral cell reads its parent's extents in the bucket, and its own here; its segments
/// frame in place as `Encryption::Sectors` in `segments/` (no `kept/`), and a checkpoint covers
/// them once eight are closed, its authenticated `extent_frame::table` local checkpoint naming
/// them; it reopens as it was and refuses tampered or legacy `cfsbase2` checkpoints.
#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_cell_frames_segments_in_place_and_checkpoints_them() {
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
    cell.wait_framed().await.unwrap();
    assert_eq!(numbers(&dir, SEGMENTS), (0..7).collect::<Vec<_>>(), "framed in place");
    assert!(dir.list("kept").is_err(), "no kept/ directory");
    assert_eq!(
        LocalCheckpoint::read(&dir).unwrap().through,
        None,
        "seven closed: no checkpoint yet"
    );
    commit(&cell, 7, 8).await;
    assert_eq!(numbers(&dir, SEGMENTS), (0..8).collect::<Vec<_>>(), "eight closed: covered");
    for kind in [LOGS, OPEN_SEGMENTS] {
        let left = numbers(&dir, kind);
        assert_eq!(left[0], 8, "the open segment's, and the next's if prepared: {left:?}");
    }
    let ring = keys();
    let checkpoint = LocalCheckpoint::read_under(&dir, &ring.of(CHILD.id)).unwrap();
    assert_eq!((checkpoint.through, checkpoint.own.len()), (Some(7), 8));
    assert_eq!(checkpoint.last_commit, Some(tag(0, 18)), "the parent's commit 10, then eight");
    // Every closed ephemeral segment is sector-encrypted.
    for number in 0..8 {
        let path = segment_path(SegmentId { cell: CHILD.id, number });
        let bytes = crate::framed::read_whole(&dir.open(&path, true).unwrap()).unwrap();
        assert!(matches!(
            extent_frame::Frame::of(&bytes).unwrap().encryption,
            extent_frame::Encryption::Sectors { .. }
        ));
    }
    for sequence in 0..8 {
        assert_eq!(byte_of(&cell, sequence), Some(sequence as u8 + 1));
    }
    drop(cell);

    // Tampered local checkpoint is refused when authenticated under keys, and legacy cfsbase2 is
    // refused outright.
    let wrong_ring = Ring::writing_under(CHILD, extent_frame::Secret::from_bytes(&[0x42; 32]));
    assert!(LocalCheckpoint::read_under(&dir, &wrong_ring.of(CHILD.id)).is_err());
    assert!(LocalCheckpoint::decode(b"cfsbase2\x00\x00\x00\x00", &extent_frame::NoKeys).is_err());

    let cell = open(&dir);
    for sequence in 0..8 {
        assert_eq!(byte_of(&cell, sequence), Some(sequence as u8 + 1));
    }
    assert_eq!(cell.read(inherited(), 0, &mut data).unwrap_err().is::<BucketData>(), true);
    assert_eq!(cell.last_commit().unwrap(), 18);
}

/// What a checkpoint covers of frees gives back the runs of the extents freed, in their segments,
/// which then take less of the disk; a segment no extent is in any more goes; a freed extent of
/// the parent's is the bucket's no more.
#[tokio::test(flavor = "multi_thread")]
async fn checkpoints_give_back_what_was_freed() {
    let dir = MemDir::new();
    make(&dir);
    let cell = open(&dir);
    for sequence in 0..8 {
        commit(&cell, sequence, 1).await;
    }
    assert_eq!(numbers(&dir, SEGMENTS), (0..8).collect::<Vec<_>>());
    let whole = allocated(&dir, 0);
    assert!(whole >= BLOCK as u64, "a segment holds its block: {whole}");
    cell.free(own(0)).unwrap();
    cell.free(inherited()).unwrap();
    cell.commit(None).await.unwrap();
    // Eight more closed, the first with the commit freeing, all covered by the next checkpoint.
    for sequence in 8..16 {
        commit(&cell, sequence, 2).await;
    }
    assert_eq!(byte_of(&cell, 0), None);
    assert!(!numbers(&dir, SEGMENTS).contains(&0), "segment 0 held extent 0 alone: it goes");
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

/// A run freed in a segment that still holds others is given back alone: the segment takes less of
/// the disk, and its other extents read as they were (even in the same 1 MiB part).
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
    assert!(numbers(&dir, SEGMENTS).contains(&0));
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
    assert_eq!(LocalCheckpoint::read(&dir).unwrap().through, None);
    drop(cell);
    assert_eq!(LocalCheckpoint::read(&dir).unwrap().through, Some(2));
    assert_eq!(numbers(&dir, SEGMENTS), vec![0, 1, 2]);
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
    stops_at_every_step(false).await;
}

/// Board 5744: killed at any step, its host up, its unsynced writes left in the host's caches,
/// then opened again, which reads them, then the host losing power, it keeps every commit made
/// local: a local checkpoint renamed into place, its directory not yet synced, among them.
#[tokio::test(flavor = "multi_thread")]
async fn kills_at_every_step_then_a_power_loss_keep_what_was_committed() {
    stops_at_every_step(true).await;
}

/// The disk failing after `n` operations, for every `n` through a run of checkpoints, the cell
/// stopping there: its host losing power, or, if `killed`, its process killed, opened again, then
/// its host losing power; the cell opened again holds each extent whose commit returned.
async fn stops_at_every_step(killed: bool) {
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
        let crashed = match killed {
            false => dir.crash(fail_after),
            true => {
                let killed = dir.killed();
                drop(open(&killed));
                killed.crash(fail_after)
            }
        };
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

/// Board 6905 (verifying review): closing an ephemeral segment whose live bytes dropped to 0
/// (e.g. an uncommitted write freed before commit) must not hard-link the longer raw file with
/// stale trailing ciphertext after the trailer, which lost durable commits on crash.
#[tokio::test(flavor = "multi_thread")]
async fn empty_live_segment_with_dead_raw_bytes_survives_crash() {
    let dir = MemDir::new();
    make(&dir);
    let cell = open(&dir);
    cell.write(own(0), 0, &[0x55; BLOCK]).await.unwrap();
    cell.free(own(0)).unwrap();
    cell.close_segment().unwrap();
    cell.commit(Some(b"after")).await.unwrap();
    cell.wait_framed().await.unwrap();
    cell.flush().await.unwrap();
    // Simulate a crash without running clean-close checkpointing.
    let crashed = dir.crash(0);
    std::mem::forget(cell);
    let reopened = open(&crashed);
    assert_eq!(reopened.root().unwrap(), b"after");
}

/// Board 6905 (verifying review): a second kill/crash after reopening an ephemeral cell whose WAL
/// freed the last extent of a checkpoint-covered segment must still reopen cleanly even though
/// the first reopen already unlinked that segment's file while `local-checkpoint` still named it.
#[tokio::test(flavor = "multi_thread")]
async fn second_kill_after_reaping_checkpointed_segment_reopens() {
    let dir = MemDir::new();
    make(&dir);
    let cell = open(&dir);
    for seq in 0..8u64 {
        cell.write(own(seq), 0, &[seq as u8 + 1; BLOCK]).await.unwrap();
        cell.commit(None).await.unwrap();
    }
    // Wait for the 8th segment's checkpoint (`through = Some(7)`).
    cell.wait_framed().await.unwrap();
    cell.flush().await.unwrap();
    // Free the sole extent in checkpoint-covered segment 0 (recorded in WAL segment 8).
    cell.free(own(0)).unwrap();
    cell.commit(Some(b"freed-0")).await.unwrap();
    cell.wait_framed().await.unwrap();
    cell.flush().await.unwrap();
    // Kill 1 without clean close (keeps `local-checkpoint` referencing segment 0).
    let killed1 = dir.killed();
    std::mem::forget(cell);
    // Reopen 1 unlinks segment 0, then kill 2 before any new checkpoint is written.
    let cell2 = open(&killed1);
    assert_eq!(byte_of(&cell2, 0), None);
    let killed2 = killed1.killed();
    std::mem::forget(cell2);
    // Reopen 2 must succeed without ENOENT on segment 0.
    let cell3 = open(&killed2);
    assert_eq!(cell3.root().unwrap(), b"freed-0");
    assert_eq!(byte_of(&cell3, 0), None);
    for seq in 1..8u64 {
        assert_eq!(byte_of(&cell3, seq), Some(seq as u8 + 1));
    }
}

/// board 5428: if an ephemeral reopen fails after local recovery (which removed an empty open
/// segment), retrying must still pick a next segment number past `checkpoint.through` and every
/// covered segment, so new commits are never overwritten or unlinked by `settle` on a later crash.
#[tokio::test(flavor = "multi_thread")]
async fn reopen_after_failed_open_keeps_next_segment_past_checkpoint_through() {
    let dir = MemDir::new();
    make(&dir);
    let cell = open(&dir);
    for seq in 0..8u64 {
        commit(&cell, seq, seq as u8 + 1).await;
    }
    // Clean close writes the local checkpoint covering segments 0..=7 (`through = Some(7)`).
    cell.close().await.unwrap();
    drop(cell);

    // First reopen runs `settle` + `wal::recover` (which removes any empty open segment) and then
    // fails before `open_ephemeral` completes (dropping `Recovering`).
    let failed = LocalCell::recover_unmade(dir.clone()).unwrap();
    drop(failed);

    // Second reopen must still open past `through` (segment >= 8), not segment 0.
    let cell2 = open(&dir);
    commit(&cell2, 8, 0x99).await;
    cell2.wait_framed().await.unwrap();
    cell2.flush().await.unwrap();

    // Simulate a crash (so `settle` runs on the next reopen before WAL replay).
    let crashed = dir.crash(0);
    std::mem::forget(cell2);
    let cell3 = open(&crashed);
    for seq in 0..8u64 {
        assert_eq!(byte_of(&cell3, seq), Some(seq as u8 + 1), "kept extent {seq}");
    }
    assert_eq!(byte_of(&cell3, 8), Some(0x99), "post-retry commit must survive settle on crash");
}

/// board 9316: `LocalCheckpoint::read` refuses a sparse oversized checkpoint file before
/// allocating memory for its declared size.
#[test]
fn oversized_local_checkpoint_is_refused_before_allocating() {
    struct SparseDir;
    struct SparseFile;
    impl crate::File for SparseFile {
        fn read_at(&self, _buf: &mut [u8], _offset: u64) -> io::Result<()> {
            panic!("read_at must not be called on an oversized file")
        }
        fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<()> {
            unreachable!()
        }
        fn sync(&self) -> io::Result<()> {
            Ok(())
        }
        fn size(&self) -> io::Result<u64> {
            Ok(1u64 << 40) // 1 TiB sparse
        }
    }
    impl crate::Dir for SparseDir {
        type File = SparseFile;
        fn create(&self, _path: &str, _data: bool) -> io::Result<Self::File> {
            unreachable!()
        }
        fn open(&self, _path: &str, _data: bool) -> io::Result<Self::File> {
            Ok(SparseFile)
        }
        fn create_dir(&self, _path: &str) -> io::Result<()> {
            unreachable!()
        }
        fn remove(&self, _path: &str) -> io::Result<()> {
            unreachable!()
        }
        fn rename(&self, _from: &str, _to: &str) -> io::Result<()> {
            unreachable!()
        }
        fn link(&self, _from: &str, _to: &str) -> io::Result<()> {
            unreachable!()
        }
        fn list(&self, _path: &str) -> io::Result<Vec<String>> {
            unreachable!()
        }
        fn replace(&self, _path: &str, _bytes: &[u8]) -> io::Result<()> {
            unreachable!()
        }
        fn sync_dir(&self, _path: &str) -> io::Result<()> {
            Ok(())
        }
        fn barrier(&self) -> io::Result<()> {
            Ok(())
        }
    }
    let err = LocalCheckpoint::read(&SparseDir).unwrap_err();
    assert!(err.to_string().contains("exceeds limit"), "expected size limit error, got: {err:#}");
}
