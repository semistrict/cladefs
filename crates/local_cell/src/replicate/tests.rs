// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// A member in memory: its logs, and what it does when sent something.
#[derive(Default)]
struct Fake {
    logs: Mutex<BTreeMap<u64, Vec<u8>>>,
    dropped: Mutex<Option<u64>>,
    /// What it answers instead of taking an append, if anything.
    failing: Mutex<Option<LinkError>>,
    appends: AtomicUsize,
}

impl Fake {
    fn fail(&self, error: Option<LinkError>) {
        *self.failing.lock().unwrap() = error;
    }

    fn log(&self, log: u64) -> Vec<u8> {
        self.logs.lock().unwrap().get(&log).cloned().unwrap_or_default()
    }

    /// Loses what it held of `log`, as a member whose disk was replaced would.
    fn lose(&self, log: u64) {
        self.logs.lock().unwrap().remove(&log);
    }
}

impl MemberLink for Fake {
    fn append<'a>(
        &'a self,
        log: u64,
        offset: u64,
        bytes: Vec<u8>,
    ) -> futures::future::BoxFuture<'a, Result<u64, LinkError>> {
        Box::pin(async move {
            let bytes = &bytes[..];
            if let Some(error) = self.failing.lock().unwrap().clone() {
                return Err(error);
            }
            self.appends.fetch_add(1, Ordering::Relaxed);
            let mut logs = self.logs.lock().unwrap();
            let held = logs.entry(log).or_default();
            let end = held.len() as u64;
            if offset > end {
                return Ok(end);
            }
            held.truncate(offset as usize);
            held.extend_from_slice(bytes);
            Ok(held.len() as u64)
        })
    }

    fn drop_through<'a>(
        &'a self,
        through: u64,
    ) -> futures::future::BoxFuture<'a, Result<(), LinkError>> {
        Box::pin(async move {
            if let Some(error) = self.failing.lock().unwrap().clone() {
                return Err(error);
            }
            self.logs.lock().unwrap().retain(|&log, _| log > through);
            *self.dropped.lock().unwrap() = Some(through);
            Ok(())
        })
    }
}

fn members(count: usize) -> Vec<Arc<Fake>> {
    (0..count).map(|_| Arc::new(Fake::default())).collect()
}

fn start(f: usize, base: u64, members: &[Arc<Fake>]) -> (Arc<Replicator>, Arc<AtomicUsize>) {
    let notified = Arc::new(AtomicUsize::new(0));
    let counted = notified.clone();
    let links = members.iter().map(|member| member.clone() as Arc<dyn MemberLink>).collect();
    let spool = Arc::new(MemSpool::default());
    let replicator = Replicator::start(f, base, spool, links, move || {
        _ = counted.fetch_add(1, Ordering::Relaxed);
    });
    (replicator, notified)
}

/// Waits for `done` to hold, failing the test past 10 s.
fn until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(10), "waited for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn flushed(epoch: u64, appends: &[(u64, u64, &[u8])]) -> Flushed {
    Flushed {
        epoch,
        appends: appends
            .iter()
            .map(|&(log, offset, bytes)| (log, offset, bytes.to_vec()))
            .collect(),
    }
}

/// With f = 1, a commit is replicated once one member besides the writer holds it, not before;
/// with both down, never, its waiters left to the bucket (doc-21's disjunction).
#[test]
fn commits_are_replicated_on_f_members_besides_the_writer() {
    let members = members(2);
    members[1].fail(Some(LinkError::Down("out of reach".into())));
    let (replicator, notified) = start(1, 0, &members);
    replicator.flushed(flushed(3, &[(log_of(1), 0, b"header+record")])).unwrap();
    until("one member", || replicator.replicated() == 3);
    assert!(notified.load(Ordering::Relaxed) >= 1);
    assert_eq!(members[0].log(log_of(1)), b"header+record");
    until("the other member down", || replicator.down().len() == 1);

    members[0].fail(Some(LinkError::Down("out of reach too".into())));
    replicator.flushed(flushed(4, &[(log_of(1), 13, b"+next")])).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(replicator.replicated(), 3, "no member holds 4");
    members[1].fail(None);
    until("the other member, caught up from its start", || replicator.replicated() == 4);
    assert_eq!(members[1].log(log_of(1)), b"header+record+next");
}

/// With f = 2, two of four members must hold a commit.
#[test]
fn f_of_two_needs_two_members() {
    let members = members(4);
    for member in &members[1..] {
        member.fail(Some(LinkError::Down("down".into())));
    }
    let (replicator, _) = start(2, 0, &members);
    replicator.flushed(flushed(7, &[(log_of(2), 0, b"x")])).unwrap();
    until("the first member", || members[0].log(log_of(2)) == b"x");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(replicator.replicated(), 0);
    members[3].fail(None);
    until("two members", || replicator.replicated() == 7);
}

/// What a guest is told replicated counts the members only once the bucket holds the commits
/// before the replicator started, which no member holds.
#[test]
fn commits_before_the_start_are_told_replicated_once_uploaded() {
    let members = members(2);
    let (replicator, _) = start(1, 10, &members);
    replicator.flushed(flushed(12, &[(log_of(5), 0, b"x")])).unwrap();
    until("replicated", || replicator.replicated() == 12);
    assert_eq!(replicator.told(9), 9, "commit 10 is on no member, nor uploaded");
    assert_eq!(replicator.told(10), 12);
}

/// A member answering less than it was sent, having lost what it held, is sent its log again from
/// where it says it ends.
#[test]
fn members_that_lost_logs_are_sent_them_again() {
    let members = members(2);
    members[1].fail(Some(LinkError::Down("down".into())));
    let (replicator, _) = start(1, 0, &members);
    replicator.flushed(flushed(1, &[(log_of(1), 0, b"aaaa")])).unwrap();
    until("held", || replicator.replicated() == 1);
    members[0].lose(log_of(1));
    replicator.flushed(flushed(2, &[(log_of(1), 4, b"bbbb")])).unwrap();
    until("sent again from its start", || replicator.replicated() == 2);
    assert_eq!(members[0].log(log_of(1)), b"aaaabbbb");
}

/// A log longer than a piece goes in pieces.
#[test]
fn long_logs_go_in_pieces() {
    let members = members(2);
    let (replicator, _) = start(1, 0, &members);
    let long = vec![9; PIECE * 2 + 1];
    replicator.flushed(flushed(1, &[(log_of(1), 0, &long)])).unwrap();
    until("held", || replicator.replicated() == 1);
    assert_eq!(members[0].log(log_of(1)), long);
    assert!(members[0].appends.load(Ordering::Relaxed) >= 3);
}

/// A member saying a later epoch fenced the cell fences the writer: nothing more is replicated,
/// and it is told.
#[test]
fn a_member_fencing_fences_the_writer() {
    let members = members(2);
    for member in &members {
        member.fail(Some(LinkError::Fenced("fenced at epoch 9".into())));
    }
    let (replicator, notified) = start(1, 0, &members);
    replicator.flushed(flushed(1, &[(log_of(1), 0, b"x")])).unwrap();
    until("fenced", || replicator.fenced().is_some());
    assert!(notified.load(Ordering::Relaxed) >= 1);
    assert_eq!(replicator.replicated(), 0);
}

/// The head naming a segment drops its logs, here and on the members, and the marks of commits in
/// them count as held, in the bucket.
#[test]
fn acknowledged_segments_are_dropped() {
    let members = members(2);
    members[1].fail(Some(LinkError::Down("down".into())));
    let (replicator, _) = start(1, 0, &members);
    replicator.flushed(flushed(1, &[(log_of(1), 0, b"one"), (own_of(1), 0, b"own")])).unwrap();
    replicator.flushed(flushed(2, &[(log_of(2), 0, b"two")])).unwrap();
    until("held", || replicator.replicated() == 2);
    replicator.acknowledged(1);
    until("dropped", || *members[0].dropped.lock().unwrap() == Some(own_of(1)));
    assert_eq!(members[0].log(log_of(1)), b"");
    assert_eq!(members[0].log(log_of(2)), b"two");
    assert_eq!(replicator.spooled(), 3);
    // The member down, back, is sent the drop and segment 2 alone.
    members[1].fail(None);
    until("the other caught up", || members[1].log(log_of(2)) == b"two");
    assert_eq!(members[1].log(log_of(1)), b"");
}

/// Own extents' frames read back as framed, and a torn last frame not at all.
#[test]
fn own_frames_read_back() {
    let mut bytes = own_frame(4, b"four");
    bytes.extend(own_frame(9, b""));
    bytes.extend(own_frame(12, b"twelve"));
    assert_eq!(
        own_frames(&bytes),
        [(4, b"four".to_vec()), (9, Vec::new()), (12, b"twelve".to_vec())]
    );
    assert_eq!(own_frames(&bytes[..bytes.len() - 1]).len(), 2);
}

/// A spool that fails breaks the replicator: nothing more is sent, its commits wait for the bucket
/// (degraded), and the flush that found it fails.
#[test]
fn a_spool_failing_breaks_the_replicator() {
    struct Failing;
    impl Spool for Failing {
        fn append(&self, _: u64, _: u64, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("the disk is full"))
        }
        fn read(&self, _: u64, _: u64, _: u64) -> std::io::Result<Vec<u8>> {
            unreachable!()
        }
        fn remove(&self, _: u64) -> std::io::Result<()> {
            Ok(())
        }
    }
    let members = members(2);
    let links = members.iter().map(|member| member.clone() as Arc<dyn MemberLink>).collect();
    let replicator = Replicator::start(1, 0, Arc::new(Failing), links, || {});
    assert!(replicator.flushed(flushed(1, &[(log_of(1), 0, b"x")])).is_err());
    assert!(replicator.broken().is_some());
    assert!(replicator.degraded());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(members[0].log(log_of(1)), b"");
}

/// TASK-537.3.4's review: a member is sent each segment's own extents before its records, so that
/// one holding a record holds the files of the extents it stored on their own.
#[test]
fn own_extents_go_before_their_records() {
    #[derive(Default)]
    struct Ordered(Mutex<Vec<u64>>);
    impl MemberLink for Ordered {
        fn append<'a>(
            &'a self,
            log: u64,
            offset: u64,
            bytes: Vec<u8>,
        ) -> futures::future::BoxFuture<'a, Result<u64, LinkError>> {
            Box::pin(async move {
                let bytes = &bytes[..];
                self.0.lock().unwrap().push(log);
                Ok(offset + bytes.len() as u64)
            })
        }
        fn drop_through<'a>(
            &'a self,
            _: u64,
        ) -> futures::future::BoxFuture<'a, Result<(), LinkError>> {
            Box::pin(async move { Ok(()) })
        }
    }
    let ordered = Arc::new(Ordered::default());
    let links: Vec<Arc<dyn MemberLink>> = vec![ordered.clone(), Arc::new(Fake::default())];
    // Started with its sends held back by a flush of both at once.
    let replicator = Replicator::start(1, 0, Arc::new(MemSpool::default()), links, || {});
    replicator.flushed(flushed(1, &[(log_of(3), 0, b"records"), (own_of(3), 0, b"own")])).unwrap();
    until("both sent", || ordered.0.lock().unwrap().len() == 2);
    assert_eq!(*ordered.0.lock().unwrap(), [own_of(3), log_of(3)]);
}

/// TASK-537.3.4's review: a cell's own log is recovered unless its members' go past it: a later
/// writer's commits, or the same writer's beyond its own log (sent before its own sync).
#[test]
fn members_go_past_own_logs_by_writer_then_commit() {
    let tag = |writer: u64, commit: u64| (writer << 40) | commit;
    assert!(!members_ahead(Some(tag(3, 9)), None));
    assert!(members_ahead(None, Some(tag(3, 9))));
    assert!(members_ahead(Some(tag(3, 9)), Some(tag(4, 6))), "a later writer's");
    assert!(members_ahead(Some(tag(3, 9)), Some(tag(3, 10))), "beyond its own sync");
    assert!(!members_ahead(Some(tag(3, 9)), Some(tag(3, 9))), "the same");
    assert!(!members_ahead(Some(tag(3, 12)), Some(tag(3, 9))), "its own Local commits past them");
    assert!(!members_ahead(Some(tag(4, 2)), Some(tag(3, 9))), "an earlier writer's");
}

fn links(members: &[Arc<Fake>]) -> Vec<Arc<dyn MemberLink>> {
    members.iter().map(|member| member.clone() as Arc<dyn MemberLink>).collect()
}

/// TASK-537.3.5 (doc-21, Replacing a member): a replacement's members are sent every log spooled,
/// from its start; while its swap is in flight a commit is replicated only once quorums of both
/// configurations hold it (TLC's joint rule); once swapped, the replacement's alone counts.
#[test]
fn replacements_get_the_tail_and_count_jointly_until_swapped() {
    let old = members(2);
    let (replicator, _) = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one")])).unwrap();
    until("the first commit replicated", || replicator.replicated() == 1);
    old[1].fail(Some(LinkError::Down("lost".into())));

    let new = members(2);
    for member in &new {
        member.fail(Some(LinkError::Down("not yet".into())));
    }
    replicator.replacing(1, links(&new));
    assert!(!replicator.installed(0), "installed before a new member held the tail");
    replicator.flushed(flushed(2, &[(2, 3, b"two")])).unwrap();
    until("the old configuration holds it", || old[0].log(2) == b"onetwo");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(replicator.replicated(), 1, "told on the old configuration alone");

    new[0].fail(None);
    until("the new member holds the tail", || new[0].log(2) == b"onetwo");
    until("replicated on both", || replicator.replicated() == 2);
    assert!(replicator.installed(0));

    replicator.swapped();
    old[0].fail(Some(LinkError::Down("gone".into())));
    replicator.flushed(flushed(3, &[(2, 6, b"three")])).unwrap();
    until("replicated on the new one alone", || replicator.replicated() == 3);
    assert_eq!(old[0].log(2), b"onetwo", "a retired member is sent nothing more");
    assert_eq!(replicator.members_up(), 1);
}

/// TASK-537.3.5: a replacement given up, its swap never sent, counts for nothing after.
#[test]
fn abandoned_replacements_count_no_more() {
    let old = members(2);
    let (replicator, _) = start(1, 0, &old);
    let new = members(2);
    for member in &new {
        member.fail(Some(LinkError::Down("out of reach".into())));
    }
    replicator.replacing(1, links(&new));
    replicator.flushed(flushed(1, &[(2, 0, b"one")])).unwrap();
    until("the old configuration holds it", || old[0].log(2) == b"one");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(replicator.replicated(), 0);
    replicator.abandon();
    assert_eq!(replicator.replicated(), 1);
    assert!(!replicator.replacing_now());
}

/// TASK-537.3.5 (doc-21, Degraded): a drop to f = 0 holds no quorum, so nothing more is told
/// replicated from its start (TLC's zeroNoQuorum), and it may be swapped in only once the bucket
/// holds what was (zeroAfterUpload). Swapped, the replicator is degraded: its commits wait for the
/// bucket.
#[test]
fn drops_to_f_zero_wait_for_the_bucket_and_tell_nothing_replicated() {
    let old = members(2);
    let (replicator, _) = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one")])).unwrap();
    until("replicated", || replicator.replicated() == 1);
    replicator.replacing(0, Vec::new());
    replicator.flushed(flushed(2, &[(2, 3, b"two")])).unwrap();
    until("the old configuration holds it", || old[0].log(2) == b"onetwo");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(replicator.replicated(), 1, "told replicated while dropping to f = 0");
    assert!(!replicator.installed(0), "dropped before the bucket held what was told");
    assert!(replicator.installed(1));
    replicator.swapped();
    assert!(replicator.degraded());
    assert_eq!(replicator.f(), 0);
}

/// TASK-537.3.5: a replacement of a configuration whose spooled logs the bucket all holds is
/// installed at once, nothing to send its members.
#[test]
fn replacements_with_nothing_spooled_are_installed_at_once() {
    let old = members(2);
    let (replicator, _) = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one"), (3, 0, b"own")])).unwrap();
    until("replicated", || replicator.replicated() == 1);
    replicator.acknowledged(1);
    let new = members(2);
    replicator.replacing(1, links(&new));
    assert!(replicator.installed(0));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(new[0].appends.load(Ordering::Relaxed), 0, "sent what the bucket holds");
}

/// TASK-537.3.5: a member refusing the writer, or down long enough, is one to replace.
#[test]
fn members_refusing_or_down_long_are_lost() {
    let old = members(2);
    old[1].fail(Some(LinkError::Refused("full".into())));
    let (replicator, _) = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one")])).unwrap();
    until("refused", || replicator.lost(Duration::from_secs(3600)));
    assert_eq!(replicator.refusing(), vec![1]);
    assert!(!replicator.degraded(), "one of two up is a quorum of f = 1");
}

/// TASK-537.3.5's review: a replacement whose members refuse every append is no configuration
/// to swap in, though the bucket holds all that was told.
#[test]
fn replacements_whose_members_refuse_are_not_installed() {
    let old = members(2);
    let (replicator, _) = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one")])).unwrap();
    until("replicated", || replicator.replicated() == 1);
    let new = members(2);
    for member in &new {
        member.fail(Some(LinkError::Refused("full".into())));
    }
    replicator.replacing(1, links(&new));
    replicator.flushed(flushed(2, &[(2, 3, b"two")])).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!replicator.installed(2), "installed on the bucket alone");
    assert_eq!(replicator.abandon(), vec![0, 1]);
}

/// TASK-537.3.5's review: a member down, whose first answer back is a drop, nothing else left to
/// send it, is up again.
#[test]
fn members_back_with_a_drop_are_up() {
    let members = members(2);
    let (replicator, _) = start(1, 0, &members);
    replicator.flushed(flushed(1, &[(2, 0, b"one")])).unwrap();
    until("replicated", || replicator.replicated() == 1);
    members[1].fail(Some(LinkError::Down("out of reach".into())));
    replicator.flushed(flushed(2, &[(2, 3, b"two")])).unwrap();
    until("down", || replicator.members_up() == 1);
    replicator.acknowledged(1);
    members[1].fail(None);
    until("up again", || replicator.members_up() == 2);
}

/// A replica log of segment `number` of the cell `trimmed` reads, its records each naming the
/// commits of `commits`' entries, as a member holds it.
fn replica_log(number: u64, commits: &[&[u64]]) -> (u64, Vec<u8>) {
    replica_log_under(number, commits, [5; 32])
}

/// The cell `trimmed` reads' keys, its DEK the one [`replica_log`]'s are MACed under.
fn trimmed_keys() -> Arc<bucket_cell::keys::Ring> {
    let cell = bucket_cell::layout::CellId { tenant: TRIMMED.tenant, id: TRIMMED.id };
    bucket_cell::keys::Ring::writing_under(cell, extent_frame::Secret::from_bytes(&[5; 32]))
}

/// TASK-680's review: members' logs beginning past the head's last commit are their writer's
/// base, given up, only once the cell's keys find them authentic: a log MACed under another key is
/// refused, as a forged one past the head's last would otherwise make a take give up commits told
/// replicated.
#[test]
fn gaps_are_given_up_only_when_authentic() {
    let keys = trimmed_keys();
    let given = |logs: &[(u64, Vec<u8>)]| {
        trimmed(TRIMMED, logs, Some(2), Some(&keys)).map_err(|err| format!("{err:#}"))
    };
    let hole = [replica_log(2, &[&[4]])];
    assert_eq!(given(&hole), Ok(vec![]));
    let forged = [replica_log_under(2, &[&[4]], [6; 32])];
    assert!(given(&forged).unwrap_err().contains("not authentic"), "{:?}", given(&forged));
}

/// A replica log as [`replica_log`] makes it, MACed under a DEK of `secret`'s bytes.
fn replica_log_under(number: u64, commits: &[&[u64]], secret: [u8; 32]) -> (u64, Vec<u8>) {
    use crate::wal::{LogRecord, ReplicaLog, Runs};
    let segment = crate::layout::SegmentId { cell: TRIMMED.id, number };
    let key = extent_frame::KeyId { cell: TRIMMED.id, generation: 1 };
    let secret = extent_frame::Secret::from_bytes(&secret);
    let encryption = Arc::new(crate::raw::RawEncryption::new(&secret, key));
    let header = crate::wal::log_header(number + 1, segment, &encryption);
    let (mut bytes, mut log) = ReplicaLog::new(&header, segment, encryption).unwrap();
    let records: Vec<LogRecord> = (commits.iter())
        .map(|commits| LogRecord {
            contents: extent_frame::segment::Contents {
                commits: commits.to_vec(),
                ..Default::default()
            },
            runs: Runs::Held(Default::default()),
        })
        .collect();
    bytes.extend(log.append(&records));
    (log_of(number), bytes)
}

const TRIMMED: extent_frame::segment::CellId = bucket_cell::layout::test_cell(3);

/// TASK-537.3.5's and TASK-537.3.6's reviews: a take keeps what follows the last segment naming a
/// commit through the head's last, a member's copy of that segment cut short among them; and
/// refuses logs no writer makes, as a forged record naming an old commit, at the end of the last
/// log or in a log of its own after it, which would otherwise make the honest ones look named.
#[test]
fn trims_keep_what_follows_the_head_and_refuse_forged_logs() {
    let kept = |logs: &[(u64, Vec<u8>)], last: u64| -> Result<Vec<u64>, String> {
        let trimmed = trimmed(TRIMMED, logs, Some(last), None).map_err(|err| format!("{err:#}"))?;
        Ok(trimmed.into_iter().map(|(log, _)| log >> 1).collect())
    };
    let honest = [replica_log(1, &[&[1], &[2]]), replica_log(2, &[&[3], &[4]])];
    assert_eq!(kept(&honest, 2), Ok(vec![2]));
    assert_eq!(kept(&honest, 4), Ok(vec![]));
    assert_eq!(kept(&honest, 0), Ok(vec![1, 2]));
    // Segment 1 named whole by the head, a member's copy of it cut short.
    let cut = [replica_log(1, &[&[1]]), replica_log(2, &[&[3], &[4]])];
    assert_eq!(kept(&cut, 2), Ok(vec![2]));
    // A hole: commit 3 on no member, the base of the writer that sent the rest, which told none of
    // it replicated, the bucket holding not its base (TASK-680), given up only once the cell's
    // keys find the logs authentic: refused with none, as a forged record past the head's last
    // would otherwise make a take give up what was told (TASK-680's review).
    let hole = [replica_log(2, &[&[4]])];
    assert!(kept(&hole, 2).unwrap_err().contains("no key authenticates them"));
    // Forged: an old commit after the honest records, or a log of its own past them.
    let appended = [replica_log(1, &[&[1], &[2]]), replica_log(2, &[&[3], &[4], &[1]])];
    assert!(kept(&appended, 2).unwrap_err().contains("not one writer's"));
    let mut own = honest.to_vec();
    own.push(replica_log(3, &[&[2]]));
    assert!(kept(&own, 2).unwrap_err().contains("not one writer's"));
}

/// Board 5408: an own log's bytes, a member's, untrusted: frames read until one is cut short, or
/// says a length past what is left, the frames before it kept, whatever the length (u64::MAX
/// among them), which overflowed `16 + len` before its bounds check.
#[test]
fn own_frames_end_at_one_cut_short_or_past_the_end() {
    let frame = |sequence: u64, file: &[u8]| {
        let mut bytes = sequence.to_le_bytes().to_vec();
        bytes.extend_from_slice(&(file.len() as u64).to_le_bytes());
        bytes.extend_from_slice(file);
        bytes
    };
    let whole = [frame(1, b"one"), frame(2, b"")].concat();
    assert_eq!(own_frames(&whole), [(1, b"one".to_vec()), (2, Vec::new())]);
    for len in [4, u64::MAX, u64::MAX - 15, 1 << 63] {
        let mut bytes = whole.clone();
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(b"abc");
        assert_eq!(own_frames(&bytes).len(), 2, "a frame of {len} bytes");
    }
    // Cut short anywhere: the first frame whole from its 19th byte, the second at the 35th.
    for cut in 0..=whole.len() {
        let expected = match cut {
            0..19 => 0,
            19..35 => 1,
            _ => 2,
        };
        assert_eq!(own_frames(&whole[..cut]).len(), expected, "cut at {cut}");
    }
}
