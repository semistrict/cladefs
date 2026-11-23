// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::MemDir;
use crate::replicate::{log_of, own_frame, own_of};

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

/// Builds a segment's replica log header and two distinct record appends for testing even logs.
fn replica_chunks(segment_num: u64) -> (Vec<u8>, Vec<u8>) {
    use crate::wal::{LogRecord, ReplicaLog, Runs};
    let segment = crate::layout::SegmentId { cell: CELL.id, number: segment_num };
    let key = extent_frame::KeyId { cell: CELL.id, generation: 1 };
    let secret = extent_frame::Secret::from_bytes(&[5; 32]);
    let encryption = Arc::new(crate::raw::RawEncryption::new(&secret, key));
    let header = crate::wal::log_header(segment_num + 1, segment, &encryption);
    let (mut first, mut log) = ReplicaLog::new(&header, segment, encryption).unwrap();
    let rec = |commit: u64, len: usize| LogRecord {
        contents: extent_frame::segment::Contents { commits: vec![commit], ..Default::default() },
        runs: Runs::Held(crate::wal::Bytes(vec![commit as u8; len])),
    };
    first.extend(log.append(&[rec(1, 512)]));
    let second = log.append(&[rec(2, 1500)]);
    (first, second)
}

/// A promise is on the disk before its fence answers: a crash right after keeps it.
#[test]
fn promises_survive_crashes() {
    let payload = own_frame(1, b"acked");
    let more = own_frame(2, b"x");
    for seed in 0..16 {
        let dir = MemDir::new();
        let replicas = Replicas::open(dir.clone(), MOST).unwrap();
        let old = (CELL, key(1, 4), replicas.incarnation());
        replicas.append(old, own_of(0), 0, &payload).unwrap();
        replicas.fence(old, 5).unwrap();
        let crashed = Replicas::open(dir.crash(seed), MOST).unwrap();
        assert_eq!(crashed.incarnation(), replicas.incarnation(), "seed {seed}");
        assert_eq!(
            refused(crashed.append(old, own_of(0), payload.len() as u64, &more)),
            Refused::Fenced { promise: 5 }
        );
        assert_eq!(
            crashed.read(old, own_of(0), 0, payload.len() as u64 + 10).unwrap(),
            payload,
            "seed {seed}"
        );
    }
}

/// TASK-537.3.2's review / board 6902: a crash may leave a log's file longer than what was written,
/// padded with zeros (a direct write's whole blocks, cut back after): its end, reopened, is what
/// its inner records/frames hold, never the file's size, so a take never takes zeros for commits.
#[test]
fn logs_padded_by_crashes_end_where_their_frames_do() {
    let (even_a, even_b) = replica_chunks(1);
    let odd_a = own_frame(1, &[7; 100]);
    let odd_b = own_frame(2, b"more");
    for (log, first, second) in [(log_of(1), even_a, even_b), (own_of(0), odd_a, odd_b)] {
        let dir = MemDir::new();
        let replicas = Replicas::open(dir.clone(), MOST).unwrap();
        let me = (CELL, key(1, 4), replicas.incarnation());
        let first_len = first.len() as u64;
        replicas.append(me, log, 0, &first).unwrap();
        let path = log_path(CELL, key(1, 4), log);
        let file = dir.open(&path, false).unwrap();
        let size = file.size().unwrap();
        let pad = (size.div_ceil(4096) * 4096 - size).max(4096) as usize;
        file.write_at(&vec![0; pad], size).unwrap();
        file.sync().unwrap();
        let reopened = Replicas::open(dir.clone(), MOST).unwrap();
        assert_eq!(reopened.fence(me, 4).unwrap(), [(log, first_len)], "log {log}");
        let total = first_len + second.len() as u64;
        assert_eq!(reopened.append(me, log, first_len, &second).unwrap(), total, "log {log}");
        let mut expected = first.clone();
        expected.extend_from_slice(&second);
        assert_eq!(reopened.read(me, log, 0, total + 100).unwrap(), expected, "log {log}");
    }
}

/// TASK-537.3.2's review: a log whose first append failed after its file was made is appended to
/// the next time, not refused for good as made already.
#[test]
fn logs_whose_first_append_failed_are_appended_to_after() {
    let first_bytes = own_frame(1, b"first");
    let total = first_bytes.len() as u64;
    for failing in 1..=4 {
        let dir = MemDir::serial();
        let replicas = Replicas::open(dir.clone(), MOST).unwrap();
        let me = (CELL, key(1, 4), replicas.incarnation());
        dir.fail_after(Some(failing));
        let first = replicas.append(me, own_of(0), 0, &first_bytes);
        dir.fail_after(None);
        let end = first.unwrap_or(0);
        assert_eq!(
            replicas.append(me, own_of(0), end, &first_bytes[end as usize..]).unwrap(),
            total,
            "{failing}"
        );
        assert_eq!(replicas.read(me, own_of(0), 0, total + 10).unwrap(), first_bytes, "{failing}");
    }
}

/// TASK-680 / board 6902: an append whose sync never came, its bytes written, then a crash that
/// corrupts what it kept of it, garbage or torn, leaves the log ending at the last intact inner
/// record or own-frame: what was answered synced is there, nothing of the rest, and the log is
/// appended to from its end.
#[test]
fn logs_end_at_their_last_whole_frame_after_corrupting_crashes() {
    let (even_a, even_b) = replica_chunks(2);
    let odd_a = own_frame(1, &[5; 600]);
    let odd_b = own_frame(2, &[6; 1500]);
    let next_odd = own_frame(3, b"next");
    for (log, a, b, next) in
        [(own_of(0), odd_a, odd_b, next_odd), (log_of(2), even_a, even_b.clone(), Vec::new())]
    {
        let a_len = a.len() as u64;
        let ab_len = a_len + b.len() as u64;
        for seed in 0..32 {
            for failing in 1..=3 {
                let dir = MemDir::serial();
                let replicas = Replicas::open(dir.clone(), MOST).unwrap();
                let me = (CELL, key(1, 4), replicas.incarnation());
                assert_eq!(replicas.append(me, log, 0, &a).unwrap(), a_len);
                dir.fail_after(Some(failing));
                let torn = replicas.append(me, log, a_len, &b);
                dir.fail_after(None);
                let crashed = Replicas::open(dir.crash_corrupting(seed), MOST).unwrap();
                let ends = crashed.fence(me, 4).unwrap();
                let end = ends[0].1;
                let at = format!("log {log}, seed {seed}, failing after {failing}: {torn:?}");
                assert!(end == a_len || end == ab_len, "{at}: ends {ends:?}");
                let mut expected = a.clone();
                if end == ab_len {
                    expected.extend_from_slice(&b);
                }
                assert_eq!(crashed.read(me, log, 0, ab_len + 1000).unwrap(), expected, "{at}");
                if !next.is_empty() {
                    let after = end + next.len() as u64;
                    assert_eq!(crashed.append(me, log, end, &next).unwrap(), after, "{at}");
                }
            }
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

/// Board 6902 (Codex review): open replica log file handles are bounded by `OPEN_FILES` even when
/// appending to hundreds of distinct small logs or reopening a cell with many logs.
#[test]
fn open_file_descriptors_stay_bounded_across_many_logs() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TrackedFile {
        inner: crate::MemFile,
        open: Arc<AtomicUsize>,
    }
    impl Drop for TrackedFile {
        fn drop(&mut self) {
            self.open.fetch_sub(1, Ordering::Relaxed);
        }
    }
    impl File for TrackedFile {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            self.inner.read_at(buf, offset)
        }
        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            self.inner.write_at(buf, offset)
        }
        fn size(&self) -> io::Result<u64> {
            self.inner.size()
        }
        fn sync(&self) -> io::Result<()> {
            self.inner.sync()
        }
        fn sync_cached(&self) -> io::Result<()> {
            self.inner.sync_cached()
        }
    }
    #[derive(Clone)]
    struct TrackedDir {
        inner: MemDir,
        open: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }
    impl TrackedDir {
        fn track(&self, file: crate::MemFile) -> TrackedFile {
            let now = self.open.fetch_add(1, Ordering::Relaxed) + 1;
            self.peak.fetch_max(now, Ordering::Relaxed);
            TrackedFile { inner: file, open: self.open.clone() }
        }
    }
    impl Dir for TrackedDir {
        type File = TrackedFile;
        fn create(&self, path: &str, data: bool) -> io::Result<Self::File> {
            Ok(self.track(self.inner.create(path, data)?))
        }
        fn open(&self, path: &str, data: bool) -> io::Result<Self::File> {
            Ok(self.track(self.inner.open(path, data)?))
        }
        fn remove(&self, path: &str) -> io::Result<()> {
            self.inner.remove(path)
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn create_dir(&self, path: &str) -> io::Result<()> {
            self.inner.create_dir(path)
        }
        fn list(&self, path: &str) -> io::Result<Vec<String>> {
            self.inner.list(path)
        }
        fn sync_dir(&self, path: &str) -> io::Result<()> {
            self.inner.sync_dir(path)
        }
        fn replace(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
            self.inner.replace(path, bytes)
        }
        fn take(&self) -> io::Result<()> {
            self.inner.take()
        }
        fn let_go(&self) {
            self.inner.let_go()
        }
        fn barrier(&self) -> io::Result<()> {
            self.inner.barrier()
        }
    }

    let open = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let dir = TrackedDir { inner: MemDir::new(), open: open.clone(), peak: peak.clone() };
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    let count = (OPEN_FILES * 3) as u64;
    for seg in 0..count {
        let frame = own_frame(seg, &seg.to_le_bytes());
        replicas.append(me, own_of(seg), 0, &frame).unwrap();
        assert!(replicas.open_files() <= OPEN_FILES);
    }
    assert!(
        open.load(Ordering::Relaxed) <= OPEN_FILES,
        "live handles {}",
        open.load(Ordering::Relaxed)
    );
    assert!(
        peak.load(Ordering::Relaxed) <= OPEN_FILES + 1,
        "peak handles {}",
        peak.load(Ordering::Relaxed)
    );
    drop(replicas);
    assert_eq!(open.load(Ordering::Relaxed), 0);

    // Reopening a directory with many logs does not retain all handles either.
    peak.store(0, Ordering::Relaxed);
    let reopened = Replicas::open(dir, MOST).unwrap();
    let ends = reopened.fence(me, 4).unwrap();
    assert_eq!(ends.len(), count as usize);
    assert_eq!(open.load(Ordering::Relaxed), 0, "cold reopen must not hold descriptors");
    for seg in 0..count {
        let expected = own_frame(seg, &seg.to_le_bytes());
        assert_eq!(reopened.read(me, own_of(seg), 0, 1024).unwrap(), expected);
    }
    assert!(open.load(Ordering::Relaxed) <= OPEN_FILES);
    assert!(peak.load(Ordering::Relaxed) <= OPEN_FILES + 1);
}

/// Board 8457: a segment's log of another format than this build's is refused, never taken for one
/// holding nothing. As its writer sends it, at its first bytes, however they are split, before any
/// ack (a writer of another build gathers no quorum on this member); as the directory holds it,
/// which a member of an older build left, when the member starts again (it acked what it holds);
/// and a directory naming other log formats is refused as it opens. A header torn by a crash, its
/// hash failing, still holds nothing, as nothing of it was acked; an own extents' log names no
/// format and is held as sent.
#[test]
fn logs_of_other_formats_are_refused_as_sent_and_as_held() {
    let (header, _) = replica_chunks(1);
    let mut foreign = header.clone();
    foreign[7] = 3;
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    let path = log_path(CELL, key(1, 4), log_of(1));
    let format = |at: &str, found: &str| Refused::Format { log: at.into(), found: found.into() };
    let at = |log: u64, offset: u64| format!("log {log} of {CELL} at {offset}");
    // Its first bytes another format's: refused, nothing written.
    let refusal = refused(replicas.append(me, log_of(1), 0, &foreign));
    assert_eq!(refusal, format(&at(2, 0), "FXLOG\\x00\\x00\\x03"));
    assert_eq!(dir.list("").unwrap(), [INCARNATION]);
    // Split: its first byte taken, the rest refused as it is sent.
    assert_eq!(replicas.append(me, log_of(1), 0, &foreign[..1]).unwrap(), 1);
    let refusal = refused(replicas.append(me, log_of(1), 1, &foreign[1..]));
    assert_eq!(refusal, format(&at(2, 1), "XLOG\\x00\\x00\\x03"));
    let refusal = refused(replicas.append(me, log_of(2), 0, b"no magic"));
    assert_eq!(refusal, format(&at(4, 0), "no magic"));
    assert_eq!(replicas.fence(me, 4).unwrap(), [(log_of(1), 1)]);
    let own = own_frame(1, b"any");
    assert_eq!(replicas.append(me, own_of(1), 0, &own).unwrap(), own.len() as u64);

    // Held: a log of another format, as a member of the build before left it, refused as the
    // member reads it back; a torn header holding nothing.
    let file = dir.open(&path, false).unwrap();
    file.write_at(&foreign, 0).unwrap();
    file.sync().unwrap();
    let reopened = Replicas::open(dir.clone(), MOST).unwrap();
    assert_eq!(refused(reopened.fence(me, 5)), format(&path, "FXLOG\\x00\\x00\\x03"));
    let mut torn = header.clone();
    torn[crate::wal::LOG_HASHED] ^= 1;
    file.write_at(&torn, 0).unwrap();
    file.sync().unwrap();
    let reopened = Replicas::open(dir.clone(), MOST).unwrap();
    assert_eq!(reopened.fence(me, 5).unwrap(), [(log_of(1), 0), (own_of(1), own.len() as u64)]);

    // A directory naming other log formats: refused as it opens.
    let older = MemDir::new();
    older.replace(INCARNATION, b"cladefs replicas 2\nincarnation 5\n").unwrap();
    let opened =
        Replicas::open(older, MOST).err().expect("a directory of other log formats opened");
    assert_eq!(opened.to_string(), format!("{INCARNATION}: not {:?}", magic()));
}

/// Board 8449: a cell gone for good loses every promise, of `u64::MAX` too, durably, and holds 0
/// only once they are gone: the edges of epochs, log and configuration numbers drawn, 0, 1 and
/// `u64::MAX` and their neighbours, as a hostile peer may send them.
#[test]
fn cells_swept_gone_lose_promises_of_every_epoch() {
    const EDGES: [u64; 6] = [0, 1, 2, u64::MAX / 2, u64::MAX - 1, u64::MAX];
    for epoch in EDGES {
        for number in EDGES {
            let dir = MemDir::new();
            let replicas = Replicas::open(dir.clone(), MOST).unwrap();
            let me = (CELL, key(number, epoch), replicas.incarnation());
            let at = format!("epoch {epoch}, configuration {number}");
            for log in [1, u64::MAX] {
                let frame = own_frame(log, b"x");
                assert_eq!(
                    replicas.append(me, log, 0, &frame).unwrap(),
                    frame.len() as u64,
                    "{at}"
                );
            }
            replicas.fence(me, epoch.saturating_sub(1)).unwrap();
            replicas.fence(me, epoch).unwrap();
            replicas.sweep(CELL, Swept::Gone).unwrap();
            assert_eq!(dir.list("").unwrap(), [INCARNATION], "{at}");
            // Nothing promised: an append of the earliest epoch is taken, here and once the
            // directory is opened again.
            let earliest = (CELL, key(0, 0), replicas.incarnation());
            let frame = own_frame(0, b"y");
            assert_eq!(
                replicas.append(earliest, 1, 0, &frame).unwrap(),
                frame.len() as u64,
                "{at}"
            );
            let reopened = Replicas::open(dir, MOST).unwrap();
            let end = reopened.append(earliest, 1, frame.len() as u64, &frame);
            assert_eq!(end.unwrap(), 2 * frame.len() as u64, "{at}");
        }
    }
}

/// Board 8453: a member holds at most so many logs of a configuration ([`LOGS_MOST`]), as a take
/// refuses a fence answer naming more: the next new log is refused, those it holds appended to
/// still, another configuration's its own.
#[test]
fn members_hold_at_most_so_many_logs_of_a_configuration() {
    const LOGS: usize = 5;
    let replicas = Replicas::open(MemDir::new(), MOST).unwrap().holding_at_most(LOGS);
    let me = (CELL, key(1, 4), replicas.incarnation());
    let frame = own_frame(0, b"x");
    for segment in 0..LOGS as u64 {
        replicas.append(me, own_of(segment), 0, &frame).unwrap();
    }
    let refusal = refused(replicas.append(me, own_of(LOGS as u64), 0, &frame));
    assert_eq!(refusal, Refused::Logs { most: LOGS });
    let end = replicas.append(me, own_of(0), frame.len() as u64, &frame).unwrap();
    assert_eq!(end, 2 * frame.len() as u64);
    let next = (CELL, key(2, 5), replicas.incarnation());
    assert_eq!(replicas.append(next, own_of(0), 0, &frame).unwrap(), frame.len() as u64);
}
