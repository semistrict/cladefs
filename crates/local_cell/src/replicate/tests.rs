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
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> Result<u64, LinkError> {
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
    }

    fn drop_through(&self, through: u64) -> Result<(), LinkError> {
        if let Some(error) = self.failing.lock().unwrap().clone() {
            return Err(error);
        }
        self.logs.lock().unwrap().retain(|&log, _| log > through);
        *self.dropped.lock().unwrap() = Some(through);
        Ok(())
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
        fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> Result<u64, LinkError> {
            self.0.lock().unwrap().push(log);
            Ok(offset + bytes.len() as u64)
        }
        fn drop_through(&self, _: u64) -> Result<(), LinkError> {
            Ok(())
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

/// TASK-537.3.4's review: a store's own log is recovered unless its members' go past it: a later
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
