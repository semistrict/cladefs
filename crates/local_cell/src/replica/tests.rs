// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::MemDir;

const CELL: CellId = CellId { tenant: 3, id: 9 };
const MOST: u64 = 1 << 20;

fn key(number: u64, epoch: u64) -> Key {
    Key { number, epoch }
}

fn refused(result: Result<impl std::fmt::Debug, Error>) -> Refused {
    match result {
        Err(Error::Refused(refused)) => refused,
        other => panic!("not refused: {other:?}"),
    }
}

/// A promise is on the disk before its fence answers: a crash right after keeps it.
#[test]
fn promises_survive_crashes() {
    for seed in 0..16 {
        let dir = MemDir::new();
        let replicas = Replicas::open(dir.clone(), MOST).unwrap();
        let old = (CELL, key(1, 4), replicas.incarnation());
        replicas.append(old, 1, 0, b"acked").unwrap();
        replicas.fence(old, 5).unwrap();
        let crashed = Replicas::open(dir.crash(seed), MOST).unwrap();
        assert_eq!(crashed.incarnation(), replicas.incarnation(), "seed {seed}");
        assert_eq!(refused(crashed.append(old, 1, 5, b"x")), Refused::Fenced { promise: 5 });
        assert_eq!(crashed.read(old, 1, 0, 10).unwrap(), b"acked", "seed {seed}");
    }
}

/// TASK-537.3.2's review: a crash may leave a log's file longer than what was written, padded with
/// zeros (a direct write's whole blocks, cut back after): its end, reopened, is what its frames
/// hold, never the file's size, so a take never takes zeros for commits.
#[test]
fn logs_padded_by_crashes_end_where_their_frames_do() {
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    replicas.append(me, 1, 0, &[7; 100]).unwrap();
    let path = log_path(CELL, key(1, 4), 1);
    let file = dir.open(&path, false).unwrap();
    let size = file.size().unwrap();
    file.write_at(&vec![0; 4096 - size as usize], size).unwrap();
    file.sync().unwrap();
    // A frame torn past the zeros' start counts no more than they do.
    let reopened = Replicas::open(dir.clone(), MOST).unwrap();
    assert_eq!(reopened.fence(me, 4).unwrap(), [(1, 100)]);
    assert_eq!(reopened.append(me, 1, 100, b"more").unwrap(), 104);
    let mut expected = vec![7; 100];
    expected.extend_from_slice(b"more");
    assert_eq!(reopened.read(me, 1, 0, 200).unwrap(), expected);
}

/// TASK-537.3.2's review: a log whose first append failed after its file was made is appended to
/// the next time, not refused for good as made already.
#[test]
fn logs_whose_first_append_failed_are_appended_to_after() {
    for failing in 1..=4 {
        let dir = MemDir::serial();
        let replicas = Replicas::open(dir.clone(), MOST).unwrap();
        let me = (CELL, key(1, 4), replicas.incarnation());
        dir.fail_after(Some(failing));
        let first = replicas.append(me, 1, 0, b"first");
        dir.fail_after(None);
        let end = first.unwrap_or(0);
        assert_eq!(replicas.append(me, 1, end, &b"first"[end as usize..]).unwrap(), 5, "{failing}");
        assert_eq!(replicas.read(me, 1, 0, 10).unwrap(), b"first", "{failing}");
    }
}

/// TASK-680: an append whose sync never came, its frame written, then a crash that corrupts what
/// it kept of it, garbage or torn, leaves the log ending at the last frame its CRC holds: what was
/// answered synced is there, nothing of the rest, and the log is appended to from its end.
#[test]
fn logs_end_at_their_last_whole_frame_after_corrupting_crashes() {
    for seed in 0..32 {
        for failing in 1..=3 {
            let dir = MemDir::serial();
            let replicas = Replicas::open(dir.clone(), MOST).unwrap();
            let me = (CELL, key(1, 4), replicas.incarnation());
            assert_eq!(replicas.append(me, 1, 0, &[5; 600]).unwrap(), 600);
            dir.fail_after(Some(failing));
            let torn = replicas.append(me, 1, 600, &[6; 1500]);
            dir.fail_after(None);
            let crashed = Replicas::open(dir.crash_corrupting(seed), MOST).unwrap();
            let ends = crashed.fence(me, 4).unwrap();
            let end = ends[0].1;
            let at = format!("seed {seed}, failing after {failing}: {torn:?}");
            assert!(end == 600 || end == 2100, "{at}: ends {ends:?}");
            let mut expected = vec![5; 600];
            expected.resize(end as usize, 6);
            assert_eq!(crashed.read(me, 1, 0, 4000).unwrap(), expected, "{at}");
            assert_eq!(crashed.append(me, 1, end, b"next").unwrap(), end + 4, "{at}");
        }
    }
}

/// A directory holding a member's files and no incarnation is refused: what it promised can't be
/// known. One holding only what a file system or a crash on its first opening leaves is made a
/// member (TASK-537.3.2's review).
#[test]
fn directories_without_incarnations_are_refused() {
    let dir = MemDir::new();
    dir.create("3.9.promise.5", false).unwrap();
    let opened = Replicas::open(dir, MOST);
    assert!(matches!(opened, Err(Error::Io(_))), "{:?}", opened.err());

    let dir = MemDir::new();
    dir.create_dir("lost+found").unwrap();
    dir.create("incarnation.new", false).unwrap();
    let opened = Replicas::open(dir.clone(), MOST).unwrap();
    assert_eq!(Replicas::open(dir, MOST).unwrap().incarnation(), opened.incarnation());
}

/// TASK-537.3.4's review: a cell fenced again by a later take, its promise a file of each epoch,
/// none renamed over another; a crash leaving two reads the greater.
#[test]
fn cells_are_fenced_again_by_later_takes() {
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    replicas.fence(me, 5).unwrap();
    replicas.fence(me, 7).unwrap();
    assert_eq!(refused(replicas.append(me, 1, 0, b"x")), Refused::Fenced { promise: 7 });
    dir.create("3.9.promise.6", false).unwrap();
    let reopened = Replicas::open(dir, MOST).unwrap();
    assert_eq!(refused(reopened.fence(me, 6)), Refused::Fenced { promise: 7 });
}
