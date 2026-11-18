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

/// A log is appended to at its end or before it, never past it; and a cell's logs hold at most
/// what the member holds for a cell.
#[test]
fn appends_past_the_end_or_the_bound_are_refused() {
    let replicas = Replicas::open(MemDir::new(), 10).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    assert_eq!(
        refused(replicas.append(me, 1, 3, b"x")),
        Refused::Gap { log: 1, end: 0, offset: 3 }
    );
    replicas.append(me, 1, 0, b"abcdef").unwrap();
    assert_eq!(replicas.append(me, 1, 2, b"cdefgh").unwrap(), 8, "sent again, and more");
    assert_eq!(replicas.read(me, 1, 0, 10).unwrap(), b"abcdefgh");
    assert_eq!(refused(replicas.append(me, 2, 0, b"xyz")), Refused::Full { would: 11, most: 10 });
    replicas.drop_logs(me, Some(1)).unwrap();
    assert_eq!(replicas.append(me, 2, 0, b"xyz").unwrap(), 3, "room once the bucket has log 1");
}

/// Logs the bucket holds, or of configurations the head names no more, are dropped; a sweep of a
/// cell no longer replicated drops its promise too.
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
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![active, pending])]);
    assert_eq!(replicas.fence((CELL, active, ours), 6).unwrap(), [(3, 1)]);

    let other = CellId { tenant: 3, id: 11 };
    replicas.append((other, key(4, 2), ours), 1, 0, b"x").unwrap();
    replicas.fence((other, key(4, 2), ours), 3).unwrap();
    replicas.sweep(CELL, Some(pending)).unwrap();
    replicas.sweep(other, None).unwrap();
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![pending])]);
    let names: Vec<String> = dir.files().into_iter().map(|(name, _)| name).collect();
    assert!(!names.iter().any(|name| name.starts_with("3.11.")), "{names:?}");
}

/// A directory holding files and no incarnation is refused: what it promised can't be known.
#[test]
fn directories_without_incarnations_are_refused() {
    let dir = MemDir::new();
    dir.create("3.9.promise", false).unwrap();
    let opened = Replicas::open(dir, MOST);
    assert!(matches!(opened, Err(Error::Io(_))), "{:?}", opened.err());
}
