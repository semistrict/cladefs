// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::MemDir;
use crate::replicate::{log_of, own_frame, own_frames, own_of};

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

/// Board 5420: a fence failing at any of its steps, made again, then the next, fence as they
/// would have: its promise kept once its file was made, its file made again if a step after failed,
/// and the promises before it removed by a later fence if their removal failed; reopened, the
/// promise is the last.
#[test]
fn fences_failing_partway_are_made_again() {
    let acked = own_frame(1, b"acked");
    let held = [(own_of(0), acked.len() as u64)];
    for failing in 1..=6 {
        let dir = MemDir::serial();
        let replicas = Replicas::open(dir.clone(), MOST).unwrap();
        let me = (CELL, key(1, 4), replicas.incarnation());
        replicas.append(me, own_of(0), 0, &acked).unwrap();
        replicas.fence(me, 4).unwrap();
        dir.fail_after(Some(failing));
        let failed = replicas.fence(me, 5);
        dir.fail_after(None);
        assert_eq!(replicas.fence(me, 5).unwrap(), held, "{failing}: {failed:?}");
        assert_eq!(replicas.fence(me, 6).unwrap(), held, "{failing}");
        let promises: Vec<String> = (dir.files().into_iter())
            .map(|(name, _)| name)
            .filter(|name| name.contains(".promise."))
            .collect();
        assert_eq!(promises, [promise_path(CELL, 6)], "{failing}");
        let reopened = Replicas::open(dir.clone(), MOST).unwrap();
        assert_eq!(refused(reopened.fence(me, 5)), Refused::Fenced { promise: 6 }, "{failing}");
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
    assert_eq!(refused(replicas.append(wrong, own_of(0), 0, b"x")), expected);
    assert_eq!(refused(replicas.fence(wrong, 5)), expected);
    assert_eq!(refused(replicas.read(wrong, own_of(0), 0, 1)), expected);
    assert_eq!(refused(replicas.drop_logs(wrong, None)), expected);
}

/// A log is appended to at its end or before it: past it, nothing is written and the end is
/// answered, for the writer to send again from; and a configuration's logs hold at most what the
/// member holds for one. Own extents' logs, held as sent, whatever their bytes.
#[test]
fn appends_past_the_end_or_the_bound_are_refused() {
    let replicas = Replicas::open(MemDir::new(), 10).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    let (first, second) = (own_of(0), own_of(1));
    assert_eq!(replicas.append(me, first, 3, b"x").unwrap(), 0, "past the end: nothing written");
    assert_eq!(replicas.read(me, first, 0, 10).unwrap(), b"");
    replicas.append(me, first, 0, b"abcdef").unwrap();
    assert_eq!(replicas.append(me, first, 2, b"cdefgh").unwrap(), 8, "sent again, and more");
    assert_eq!(replicas.append(me, first, 0, b"abc").unwrap(), 8, "all of it held already");
    assert_eq!(replicas.read(me, first, 0, 10).unwrap(), b"abcdefgh");
    assert_eq!(replicas.read(me, first, 3, 4).unwrap(), b"defg", "across frames");
    let full = Refused::Full { would: 11, most: 10 };
    assert_eq!(refused(replicas.append(me, second, 0, b"xyz")), full);
    assert_eq!(replicas.fence(me, 4).unwrap(), [(first, 8)]);
    replicas.drop_logs(me, Some(first)).unwrap();
    assert_eq!(replicas.append(me, second, 0, b"xyz").unwrap(), 3, "room once the bucket has it");
}

/// TASK-537.3.2's review: the bound is a configuration's, as a take installs its own on members
/// still holding the configuration before, the same commits, until it activates.
#[test]
fn takes_install_beside_the_configuration_before() {
    let replicas = Replicas::open(MemDir::new(), 10).unwrap();
    let ours = replicas.incarnation();
    let (old, new) = ((CELL, key(1, 4), ours), (CELL, key(2, 5), ours));
    replicas.append(old, own_of(0), 0, b"sixsix").unwrap();
    replicas.fence(old, 5).unwrap();
    assert_eq!(replicas.append(new, own_of(0), 0, b"sixsix").unwrap(), 6);
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
        for segment in 0..3 {
            replicas.append((CELL, of, ours), own_of(segment), 0, b"x").unwrap();
        }
    }
    replicas.drop_logs((CELL, active, ours), Some(own_of(1))).unwrap();
    // Only what the bucket holds of the configuration named: the one before kept, as a
    // replacement's members drop theirs while the head names the one before (TASK-537.3.5).
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![old, active, pending])]);
    replicas.drop_logs((CELL, active, ours), None).unwrap();
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![active, pending])]);
    assert_eq!(replicas.fence((CELL, active, ours), 6).unwrap(), [(own_of(2), 1)]);

    let first = CellId { tenant: 3, id: 12 };
    replicas.append((first, key(1, 2), ours), own_of(0), 0, b"x").unwrap();
    replicas.sweep(first, Swept::Unconfigured).unwrap();
    assert_eq!(replicas.read((first, key(1, 2), ours), own_of(0), 0, 1).unwrap(), b"x");

    let other = CellId { tenant: 3, id: 11 };
    replicas.append((other, key(4, 2), ours), own_of(0), 0, b"x").unwrap();
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
    dir.create(&promise_path(CELL, 5), false).unwrap();
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
    dir.create(&promise_path(CELL, 6), false).unwrap();
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
    // Headed as the logs of cell directories 16 and 17 were (doc-28).
    let mut foreign = header.clone();
    foreign[..8].copy_from_slice(b"FXLOG\0\0\x04");
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    let path = log_path(CELL, key(1, 4), log_of(1));
    let format = |at: &str, found: &str| Refused::Format { log: at.into(), found: found.into() };
    let at = |log: u64, offset: u64| format!("log {log} of {CELL} at {offset}");
    // Its first bytes another format's: refused, nothing written.
    let refusal = refused(replicas.append(me, log_of(1), 0, &foreign));
    assert_eq!(refusal, format(&at(2, 0), "FXLOG\\x00\\x00\\x04"));
    assert_eq!(dir.list("").unwrap(), [INCARNATION]);
    // Split: its first byte taken, the rest refused as it is sent.
    assert_eq!(replicas.append(me, log_of(1), 0, &foreign[..1]).unwrap(), 1);
    let refusal = refused(replicas.append(me, log_of(1), 1, &foreign[1..]));
    assert_eq!(refusal, format(&at(2, 1), "XLOG\\x00\\x00\\x04"));
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
    assert_eq!(refused(reopened.fence(me, 5)), format(&path, "FXLOG\\x00\\x00\\x04"));
    let mut torn = header.clone();
    torn[crate::wal::LOG_HASHED] ^= 1;
    file.write_at(&torn, 0).unwrap();
    file.sync().unwrap();
    let reopened = Replicas::open(dir.clone(), MOST).unwrap();
    assert_eq!(reopened.fence(me, 5).unwrap(), [(log_of(1), 0), (own_of(1), own.len() as u64)]);

    // A directory of a version this build doesn't read, older or newer, or saying more than an
    // incarnation file does: refused as it opens (doc-28).
    for (text, refusal) in [
        (
            "cladefs replicas 2\nincarnation 5\n",
            "\"cladefs replicas 2\": this build reads cladefs replica directory 4 to 4",
        ),
        (
            "cladefs replicas 3, logs FXLOG\\x00\\x00\\x04, own frames 0x6678646178320002\nincarnation 5\n",
            "\"cladefs replicas 3, logs FXLOG\\\\x00\\\\x00\\\\x04, own frames 0x6678646178320002\": this build reads cladefs replica directory 4 to 4",
        ),
        (
            "cladefs replica directory 5\nincarnation 5\n",
            "\"cladefs replica directory 5\": this build reads cladefs replica directory 4 to 4",
        ),
        (
            "cladefs replica directory 4\nincarnation 5\nspare\n",
            "a line past its incarnation: \"spare\"",
        ),
    ] {
        let older = MemDir::new();
        older.replace(INCARNATION, text.as_bytes()).unwrap();
        let opened = Replicas::open(older, MOST).err().expect("a directory not read opened");
        assert_eq!(opened.to_string(), format!("{INCARNATION}: {refusal}"), "{text:?}");
    }
    // A segment's log headed `FXLG` and a version this build doesn't read: refused as sent.
    let mut newer = header.clone();
    newer[4..8].copy_from_slice(&19u32.to_le_bytes());
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    let refusal = refused(replicas.append(me, log_of(1), 0, &newer));
    assert_eq!(refusal, format(&at(2, 0), "FXLG\\x13\\x00\\x00\\x00"));
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

/// Board 8445: a call is admitted only if the cell's head, as the bucket holds it, could have asked
/// it, the edges of configuration numbers, epochs and logs drawn (0, 1, the head's own and their
/// neighbours, `u64::MAX` and its): no fence past the head's epoch (`u64::MAX` among them), so
/// that its writer appends still; no drop of a log of a segment the head names not; no append to
/// a configuration its writer, or a take, makes not, nor a drop of one; nothing of a cell with no
/// live replicated head. Every call admitted but a drop of its writer's own configuration's logs
/// (which the head, naming the bucket's segments and not the writer's cell's, can't bound),
/// applied, leaves the writer appending, and every log it appended.
#[test]
fn calls_the_head_could_not_have_asked_are_refused() {
    const MAX: u64 = u64::MAX;
    // The head: its writer of epoch 5, configuration 3 of it active, segments 0 to 10 named.
    let head = Seen { epoch: 5, end: 10, active: Some(key(3, 5)) };
    let unasked = |result: Result<(), Refused>| match result {
        Err(Refused::Unasked { .. }) => {}
        other => panic!("not refused as unasked: {other:?}"),
    };
    let admitted = |asked| admit(CELL, Some(&head), asked);
    assert_eq!(admitted(Asked::Fence(key(3, 5), 5)), Ok(()));
    assert_eq!(admitted(Asked::Fence(key(3, 5), 0)), Ok(()));
    for at in [6, MAX / 2, MAX - 1, MAX] {
        unasked(admitted(Asked::Fence(key(3, 5), at)));
    }
    unasked(admitted(Asked::Fence(key(3, 4), 5)));
    unasked(admitted(Asked::Fence(key(4, 5), 5)));
    for through in [0, 19, 20, MAX] {
        assert_eq!(admitted(Asked::Drop(key(3, 5), Some(through))), Ok(()));
        assert_eq!(admitted(Asked::Drop(key(4, 5), Some(through))), Ok(()));
        for dropped in [key(3, 4), key(3, 6), key(2, 5), key(4, 6), key(MAX, MAX)] {
            unasked(admitted(Asked::Drop(dropped, Some(through))));
        }
    }
    assert_eq!(admitted(Asked::Drop(key(3, 5), None)), Ok(()));
    assert_eq!(admitted(Asked::Drop(key(2, MAX), None)), Ok(()));
    unasked(admitted(Asked::Drop(key(3, 6), None)));
    unasked(admitted(Asked::Drop(key(4, 0), None)));
    for appended in [key(3, 5), key(4, 5), key(MAX, 5)] {
        assert_eq!(admitted(Asked::Append(appended)), Ok(()), "{appended:?}");
    }
    for appended in [key(3, 4), key(3, 6), key(2, 5), key(4, 6), key(4, 4), key(MAX, MAX)] {
        unasked(admitted(Asked::Append(appended)));
    }
    // Noted before the bucket went out of reach: a writer taking the cell since, of a later
    // epoch, appends to its configuration after the active one, and drops its logs; no fence past
    // the noted epoch, nor of another configuration, nor drops before one past the active.
    let noted = |asked| admit_noted(CELL, &head, asked);
    for appended in [key(3, 5), key(4, 5), key(4, 6), key(5, 9), key(2, 6), key(2, MAX)] {
        assert_eq!(noted(Asked::Append(appended)), Ok(()), "{appended:?}");
        assert_eq!(noted(Asked::Drop(appended, Some(MAX))), Ok(()), "{appended:?}");
    }
    for appended in [key(2, 5), key(3, 4), key(0, 0), key(2, 4)] {
        unasked(noted(Asked::Append(appended)));
        unasked(noted(Asked::Drop(appended, Some(0))));
    }
    assert_eq!(noted(Asked::Fence(key(3, 5), 5)), Ok(()));
    for at in [6, MAX] {
        unasked(noted(Asked::Fence(key(3, 5), at)));
        unasked(noted(Asked::Fence(key(4, 6), at)));
    }
    unasked(noted(Asked::Drop(key(4, 6), None)));
    // None active yet: its writer installs its first, of its epoch; nothing fenced or dropped
    // before.
    let first = Seen { epoch: 5, end: 0, active: None };
    assert_eq!(admit(CELL, Some(&first), Asked::Append(key(1, 5))), Ok(()));
    unasked(admit(CELL, Some(&first), Asked::Append(key(1, 4))));
    unasked(admit(CELL, Some(&first), Asked::Fence(key(1, 5), 5)));
    unasked(admit(CELL, Some(&first), Asked::Drop(key(1, 5), None)));
    unasked(admit(CELL, Some(&first), Asked::Drop(key(1, 4), Some(0))));

    // Every edge drawn, what is admitted applied to a member holding the writer's logs of
    // segments 8 to 11, the bucket holding 8 and 9.
    let numbers = [0, 1, 2, 3, 4, MAX / 2, MAX - 1, MAX];
    let epochs = [0, 1, 4, 5, 6, MAX / 2, MAX - 1, MAX];
    let logs = [0, 1, 18, 19, 20, 21, MAX / 2, MAX - 1, MAX];
    let dir = MemDir::new();
    let replicas = Replicas::open(dir, MOST).unwrap();
    let writer = (CELL, key(3, 5), replicas.incarnation());
    let frame = own_frame(1, b"told");
    for segment in 8..12 {
        replicas.append(writer, own_of(segment), 0, &frame).unwrap();
    }
    let mut applied = 0;
    for number in numbers {
        for epoch in epochs {
            let of = (CELL, key(number, epoch), replicas.incarnation());
            assert_eq!(
                admit(CELL, None, Asked::Fence(key(number, epoch), epoch)),
                Err(Refused::Unheaded { cell: CELL })
            );
            for at in epochs {
                if admit(CELL, Some(&head), Asked::Fence(key(number, epoch), at)).is_ok() {
                    replicas.fence(of, at).unwrap();
                    applied += 1;
                }
            }
            // Its writer's own configuration's drops aside: the head can't bound them.
            if key(number, epoch) == key(3, 5) {
                continue;
            }
            for through in logs.map(Some).into_iter().chain([None]) {
                if admit(CELL, Some(&head), Asked::Drop(key(number, epoch), through)).is_ok() {
                    replicas.drop_logs(of, through).unwrap();
                    applied += 1;
                }
            }
        }
    }
    assert!(applied > 0);
    // Its writer appends still; what the bucket holds not is there still.
    let more = own_frame(2, b"more");
    let end = frame.len() as u64;
    assert_eq!(replicas.append(writer, own_of(11), end, &more).unwrap(), end + more.len() as u64);
    for segment in 8..12 {
        let read = replicas.read(writer, own_of(segment), 0, end).unwrap();
        assert_eq!(read, frame, "segment {segment}");
    }
}

/// Board 8451: calls of 100,000 cells holding nothing, a peer's (fences at the earliest epoch,
/// appends of nothing, reads, drops), leave nothing kept in memory, no file made, and nothing
/// for a sweep to read the heads of.
#[test]
fn calls_of_cells_holding_nothing_leave_nothing() {
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    for id in 0..100_000 {
        let of = (CellId { tenant: 3, id }, key(1, 0), replicas.incarnation());
        assert_eq!(replicas.fence(of, 0).unwrap(), []);
        assert_eq!(replicas.append(of, own_of(0), 0, b"").unwrap(), 0);
        assert_eq!(replicas.read(of, own_of(0), 0, 10).unwrap(), b"");
        replicas.drop_logs(of, None).unwrap();
        replicas.trim(of, None).unwrap();
    }
    assert_eq!(replicas.cells_kept(), 0);
    assert_eq!(replicas.cells().unwrap(), []);
    assert_eq!(dir.list("").unwrap(), [INCARNATION]);
    // A cell holding something is kept, until a sweep finds it gone.
    let held = (CELL, key(1, 4), replicas.incarnation());
    replicas.append(held, own_of(0), 0, &own_frame(1, b"x")).unwrap();
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![key(1, 4)])]);
    replicas.sweep(CELL, Swept::Gone).unwrap();
    assert_eq!(replicas.cells_kept(), 0);
}

/// Board 8451: the directory is listed once, as it opens: its cells' files read as each is first
/// asked of, the others' not, and the sweep's cells as that listing found them.
#[test]
fn directories_are_listed_once_as_they_open() {
    let dir = MemDir::new();
    let replicas = Replicas::open(dir.clone(), MOST).unwrap();
    let other = CellId { tenant: 3, id: 10 };
    let frame = own_frame(1, b"x");
    for (cell, number) in [(CELL, 1), (CELL, 2), (other, 1)] {
        replicas.append((cell, key(number, 4), replicas.incarnation()), 1, 0, &frame).unwrap();
    }
    replicas.fence((other, key(1, 4), replicas.incarnation()), 4).unwrap();
    let reopened = Replicas::open(dir, MOST).unwrap();
    assert_eq!(reopened.cells_kept(), 2);
    assert_eq!(
        reopened.cells().unwrap(),
        [(CELL, vec![key(1, 4), key(2, 4)]), (other, vec![key(1, 4)])]
    );
    let of = (CELL, key(2, 4), reopened.incarnation());
    assert_eq!(reopened.fence(of, 4).unwrap(), [(1, frame.len() as u64)]);
    assert_eq!(
        refused(reopened.append((other, key(1, 3), reopened.incarnation()), 1, 0, &frame)),
        Refused::Fenced { promise: 4 }
    );
}

/// Board 8451: an append the disk governor has no room for is refused, nothing of it written, and
/// taken once it has room.
#[test]
fn appends_past_the_disk_governor_are_refused() {
    const LEN: usize = 64 << 10;
    let dir = MemDir::new();
    let disk = Dir::disk(&dir).unwrap().clone();
    disk.govern(1 << 20, 0, false);
    let replicas = Replicas::open(dir, MOST).unwrap();
    let me = (CELL, key(1, 4), replicas.incarnation());
    let mut end = 0;
    let refusal = loop {
        let frame = own_frame(end, &[7; LEN]);
        match replicas.append(me, own_of(0), end, &frame) {
            Ok(now) => end = now,
            Err(err) => break err,
        }
        assert!(end < 1 << 20, "appended past the disk's allowance");
    };
    let frame = own_frame(end, &[7; LEN]);
    let Error::Refused(refusal) = refusal else { panic!("failed: {refusal:?}") };
    let full = "its disk's allowance".to_owned();
    assert_eq!(refusal, Refused::Disk { bytes: frame.len() as u64, full });
    assert_eq!(replicas.fence(me, 4).unwrap(), [(own_of(0), end)]);
    disk.govern(u64::MAX, 0, false);
    assert_eq!(replicas.append(me, own_of(0), end, &frame).unwrap(), end + frame.len() as u64);
}

/// Board 8451: a writer's stream of appends to a configuration trims those it left behind: its own
/// given up before it, and an earlier writer's not active; the active one, which a take reads, and
/// a racing taker's of a later epoch, kept. A cell holds logs of at most `CONFIGURATIONS_MOST`
/// configurations: a new one past them refused.
#[test]
fn streams_trim_the_configurations_they_leave_behind() {
    let replicas = Replicas::open(MemDir::new(), MOST).unwrap();
    let ours = replicas.incarnation();
    let frame = own_frame(1, b"x");
    for (number, epoch) in [(1, 4), (2, 5), (3, 5), (3, 6), (4, 5)] {
        replicas.append((CELL, key(number, epoch), ours), 1, 0, &frame).unwrap();
    }
    replicas.trim((CELL, key(4, 5), ours), Some(key(2, 5))).unwrap();
    assert_eq!(replicas.cells().unwrap(), [(CELL, vec![key(2, 5), key(3, 6), key(4, 5)])]);

    let replicas = Replicas::open(MemDir::new(), MOST).unwrap();
    let ours = replicas.incarnation();
    for number in 0..CONFIGURATIONS_MOST as u64 {
        replicas.append((CELL, key(number, 4), ours), 1, 0, &frame).unwrap();
    }
    let past = (CELL, key(CONFIGURATIONS_MOST as u64, 4), ours);
    assert_eq!(
        refused(replicas.append(past, 1, 0, &frame)),
        Refused::Configurations { cell: CELL, most: CONFIGURATIONS_MOST }
    );
    let held = (CELL, key(0, 4), ours);
    let end = frame.len() as u64;
    assert_eq!(replicas.append(held, 1, end, &frame).unwrap(), 2 * end);
}

/// board 5418: if `sync_dir("")` fails while creating a new log file on a replica member, a
/// retried append must still sync the directory before acknowledging, so a crash afterward keeps
/// the log file intact.
#[test]
fn retried_first_append_after_sync_dir_failure_keeps_log_across_crash() {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone)]
    struct FailSyncDir {
        inner: MemDir,
        fail_sync_dir: Arc<AtomicBool>,
    }

    impl crate::Dir for FailSyncDir {
        type File = crate::MemFile;
        fn create(&self, path: &str, data: bool) -> io::Result<Self::File> {
            self.inner.create(path, data)
        }
        fn open(&self, path: &str, data: bool) -> io::Result<Self::File> {
            self.inner.open(path, data)
        }
        fn create_dir(&self, path: &str) -> io::Result<()> {
            self.inner.create_dir(path)
        }
        fn remove(&self, path: &str) -> io::Result<()> {
            self.inner.remove(path)
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn link(&self, from: &str, to: &str) -> io::Result<()> {
            self.inner.link(from, to)
        }
        fn list(&self, path: &str) -> io::Result<Vec<String>> {
            self.inner.list(path)
        }
        fn replace(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
            self.inner.replace(path, bytes)
        }
        fn sync_dir(&self, path: &str) -> io::Result<()> {
            if self.fail_sync_dir.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("injected sync_dir failure"));
            }
            self.inner.sync_dir(path)
        }
        fn barrier(&self) -> io::Result<()> {
            self.inner.barrier()
        }
    }

    let inner = MemDir::new();
    let fail_sync_dir = Arc::new(AtomicBool::new(false));
    let dir = FailSyncDir { inner: inner.clone(), fail_sync_dir: fail_sync_dir.clone() };
    let replicas = Replicas::open(dir, MOST).unwrap();
    let ours = replicas.incarnation();
    let target = (CELL, key(1, 1), ours);
    let frame = own_frame(1, b"durable");

    // Fail `sync_dir("")` on the first append after `dir.create` created the file.
    fail_sync_dir.store(true, Ordering::SeqCst);
    assert!(replicas.append(target, own_of(0), 0, &frame).is_err());

    // Retry succeeds and MUST sync the directory before acknowledging.
    assert_eq!(replicas.append(target, own_of(0), 0, &frame).unwrap(), frame.len() as u64);

    // Crash drops any file whose directory entry was never synced; the log must survive.
    let crashed = inner.crash(0);
    let reopened = Replicas::open(crashed, MOST).unwrap();
    let again = (CELL, key(1, 1), reopened.incarnation());
    assert_eq!(reopened.fence(again, 1).unwrap(), vec![(own_of(0), frame.len() as u64)]);
    assert_eq!(reopened.read(again, own_of(0), 0, frame.len() as u64).unwrap(), frame);
}

#[test]
fn every_frozen_version_is_read() {
    formats::golden::check::<Files>(env!("CARGO_MANIFEST_DIR"));
}

/// The replica directory's own: its incarnation file, the names of its promises and logs, and an
/// own log's frames. A segment's log it holds is the cell directory's (`formats/cell-directory`),
/// an own frame's file an extent frame (`formats/extent-frame`).
struct Files;

impl formats::golden::Family for Files {
    const FORMAT: formats::Format = FORMAT;

    fn write() -> Vec<(&'static str, Vec<u8>)> {
        let cell = CellId { tenant: 0x2f, id: 0x1_0000_0007 };
        let key = Key { number: 3, epoch: 0x51 };
        let names = format!("{}\n{}\n", promise_path(cell, 0x51), log_path(cell, key, own_of(4)));
        let own = [own_frame(7, b"an extent frame"), own_frame(9, b"")].concat();
        vec![
            ("incarnation", incarnation_text(0xfedc_ba98_7654_3211).into_bytes()),
            ("names", names.into_bytes()),
            ("own-log", own),
        ]
    }

    fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
        // Version 4's readers are this build's.
        if version != 4 {
            return Err(format!("no reader of version {version}"));
        }
        Self::read_as(case, bytes)
    }

    fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
        Self::read_as(case, bytes).map(drop)
    }
}

impl Files {
    fn read_as(case: &str, bytes: &[u8]) -> Result<String, String> {
        let text = || std::str::from_utf8(bytes).map_err(|err| err.to_string());
        match case {
            "incarnation" => parse_incarnation(text()?)
                .map(|incarnation| format!("{incarnation:#x}"))
                .map_err(|err| format!("{err:#}")),
            "names" => {
                let names: Option<Vec<_>> = text()?.lines().map(parse_name).collect();
                names.map(|names| format!("{names:#?}")).ok_or_else(|| "a name not read".into())
            }
            "own-log" => {
                let frames = own_frames(bytes).map_err(|err| format!("{err:#}"))?;
                let said: Vec<_> = (frames.iter())
                    .map(|(sequence, file)| (sequence, file.escape_ascii().to_string()))
                    .collect();
                Ok(format!("{said:#?}"))
            }
            _ => Err(format!("no file {case}")),
        }
    }
}
