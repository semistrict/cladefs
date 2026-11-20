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

/// A member keeps what writers append, synced, each log at its own end, and reads it back; reopened,
/// it is the same incarnation, holding the same.
#[test]
fn appends_are_read_back_and_kept_across_openings() {
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    assert_eq!(replicas.append(me, 7, 0, b"first ").unwrap(), 6);
    assert_eq!(replicas.append(me, 7, 6, b"second").unwrap(), 12);
    assert_eq!(replicas.append(me, 8, 0, b"other log").unwrap(), 9);
    assert_eq!(replicas.read(me, 7, 0, 100).unwrap(), b"first second");
    assert_eq!(replicas.read(me, 7, 6, 3).unwrap(), b"sec");
    assert_eq!(replicas.read(me, 9, 0, 100).unwrap(), b"", "a log it does not hold");

    let incarnation = replicas.incarnation();
    drop(replicas);
    let reopened = Replicas::open(dir.clone(), MOST).unwrap();
    assert_eq!(reopened.incarnation(), incarnation);
    assert_eq!(reopened.read(me, 7, 0, 100).unwrap(), b"first second");
    assert_eq!(reopened.append(me, 7, 12, b"!").unwrap(), 13);
    assert_eq!(reopened.cells().unwrap(), [(CELL, vec![key(1, 4)])]);
}

/// A take fences the cell: appends of the epochs before refused after, of its own and later taken;
/// a fence of an earlier epoch refused; and what the fence answers is the logs of the configuration
/// asked, as they end.
#[test]
fn fences_refuse_appends_of_earlier_epochs() {
    let replicas = Replicas::open(MemDir::new(), MOST).unwrap();
    let ours = replicas.incarnation();
    let old = (CELL, key(1, 4), ours);
    replicas.append(old, 1, 0, b"aaaa").unwrap();
    replicas.append(old, 2, 0, b"bb").unwrap();
    replicas.append((CELL, key(0, 3), ours), 1, 0, b"older").unwrap();

    assert_eq!(replicas.fence(old, 5).unwrap(), [(1, 4), (2, 2)]);
    assert_eq!(refused(replicas.append(old, 2, 2, b"late")), Refused::Fenced { promise: 5 });
    assert_eq!(replicas.read(old, 2, 0, 10).unwrap(), b"bb", "nothing of it written");
    assert_eq!(refused(replicas.fence(old, 4)), Refused::Fenced { promise: 5 });
    // Fenced again at the same epoch, as a take retrying does; and the taker's own configuration
    // appended to.
    assert_eq!(replicas.fence(old, 5).unwrap(), [(1, 4), (2, 2)]);
    let new = (CELL, key(2, 5), ours);
    assert_eq!(replicas.append(new, 1, 0, b"new").unwrap(), 3);
    // Another cell is not fenced.
    let other = (CellId { tenant: 3, id: 10 }, key(1, 4), ours);
    assert_eq!(replicas.append(other, 1, 0, b"x").unwrap(), 1);
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

/// A call naming another incarnation is refused, whatever it is: a directory made anew, its disk
/// lost, is another member, whose promises are not the old one's (doc-21, Incarnations).
#[test]
fn other_incarnations_are_refused() {
    let replicas = Replicas::open(MemDir::new(), MOST).unwrap();
    let lost = Replicas::open(MemDir::new(), MOST).unwrap();
    assert_ne!(replicas.incarnation(), lost.incarnation());
    let wrong = (CELL, key(1, 4), lost.incarnation());
    let expected = Refused::Incarnation { asked: lost.incarnation(), ours: replicas.incarnation() };
    assert_eq!(refused(replicas.append(wrong, 1, 0, b"x")), expected);
    assert_eq!(refused(replicas.fence(wrong, 5)), expected);
    assert_eq!(refused(replicas.read(wrong, 1, 0, 1)), expected);
    assert_eq!(refused(replicas.drop_logs(wrong, None)), expected);
}

/// A log is appended to at its end or before it: past it, nothing is written and the end is
/// answered, for the writer to send again from; and a configuration's logs hold at most what the
/// member holds for one.
#[test]
fn appends_past_the_end_or_the_bound_are_refused() {
    let replicas = Replicas::open(MemDir::new(), 10).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    assert_eq!(replicas.append(me, 1, 3, b"x").unwrap(), 0, "past the end: nothing written");
    assert_eq!(replicas.read(me, 1, 0, 10).unwrap(), b"");
    replicas.append(me, 1, 0, b"abcdef").unwrap();
    assert_eq!(replicas.append(me, 1, 2, b"cdefgh").unwrap(), 8, "sent again, and more");
    assert_eq!(replicas.append(me, 1, 0, b"abc").unwrap(), 8, "all of it held already");
    assert_eq!(replicas.read(me, 1, 0, 10).unwrap(), b"abcdefgh");
    assert_eq!(replicas.read(me, 1, 3, 4).unwrap(), b"defg", "across frames");
    assert_eq!(refused(replicas.append(me, 2, 0, b"xyz")), Refused::Full { would: 11, most: 10 });
    // Empty appends make nothing, however many logs they name (Codex's review).
    for log in 10..20 {
        assert_eq!(replicas.append(me, log, 0, b"").unwrap(), 0);
    }
    assert_eq!(replicas.fence(me, 4).unwrap(), [(1, 8)]);
    replicas.drop_logs(me, Some(1)).unwrap();
    assert_eq!(replicas.append(me, 2, 0, b"xyz").unwrap(), 3, "room once the bucket has log 1");
}

/// TASK-537.3.2's review: the bound is a configuration's, as a take installs its own on members
/// still holding the configuration before, the same commits, until it activates.
#[test]
fn takes_install_beside_the_configuration_before() {
    let replicas = Replicas::open(MemDir::new(), 10).unwrap();
    let ours = replicas.incarnation();
    let (old, new) = ((CELL, key(1, 4), ours), (CELL, key(2, 5), ours));
    replicas.append(old, 1, 0, b"sixsix").unwrap();
    replicas.fence(old, 5).unwrap();
    assert_eq!(replicas.append(new, 1, 0, b"sixsix").unwrap(), 6);
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

/// Logs the bucket holds, or of configurations the head names no more, are dropped; a sweep of a
/// cell gone drops its promise too; and a sweep of a replicated cell whose head names no
/// configuration yet drops nothing (TASK-537.3.2's review), as its first may be being installed.
#[test]
fn drops_and_sweeps_remove_what_no_take_reads() {
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let ours = replicas.incarnation();
    let (old, active, pending) = (key(1, 4), key(2, 5), key(3, 6));
    for of in [old, active, pending] {
        for log in 1..=3 {
            replicas.append((CELL, of, ours), log, 0, b"x").unwrap();
        }
    }
    replicas.drop_logs((CELL, active, ours), Some(2)).unwrap();
    // Only what the bucket holds of the configuration named: the one before kept, as a
    // replacement's members drop theirs while the head names the one before (TASK-537.3.5).
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![old, active, pending])]);
    replicas.drop_logs((CELL, active, ours), None).unwrap();
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![active, pending])]);
    assert_eq!(replicas.fence((CELL, active, ours), 6).unwrap(), [(3, 1)]);

    let first = CellId { tenant: 3, id: 12 };
    replicas.append((first, key(1, 2), ours), 1, 0, b"x").unwrap();
    replicas.sweep(first, Swept::Unconfigured).unwrap();
    assert_eq!(replicas.read((first, key(1, 2), ours), 1, 0, 1).unwrap(), b"x");

    let other = CellId { tenant: 3, id: 11 };
    replicas.append((other, key(4, 2), ours), 1, 0, b"x").unwrap();
    replicas.fence((other, key(4, 2), ours), 3).unwrap();
    replicas.sweep(CELL, Swept::Before(pending)).unwrap();
    replicas.sweep(other, Swept::Gone).unwrap();
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![pending]), (first, vec![key(1, 2)])]);
    let names: Vec<String> = dir.files().into_iter().map(|(name, _)| name).collect();
    assert!(!names.iter().any(|name| name.starts_with("3.11.")), "{names:?}");
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
