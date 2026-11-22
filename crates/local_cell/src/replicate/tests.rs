// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use std::time::Instant;

/// A member in memory: its logs, and what it does when sent something.
#[derive(Default)]
struct Fake {
    logs: Mutex<BTreeMap<u64, Vec<u8>>>,
    /// What it answers instead of taking an append, if anything.
    failing: Mutex<Option<LinkError>>,
}

impl Fake {
    fn fail(&self, error: Option<LinkError>) {
        *self.failing.lock().unwrap() = error;
    }

    fn log(&self, log: u64) -> Vec<u8> {
        self.logs.lock().unwrap().get(&log).cloned().unwrap_or_default()
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
            Ok(())
        })
    }
}

fn members(count: usize) -> Vec<Arc<Fake>> {
    (0..count).map(|_| Arc::new(Fake::default())).collect()
}

fn start(f: usize, base: u64, members: &[Arc<Fake>]) -> Arc<Replicator> {
    let links = members.iter().map(|member| member.clone() as Arc<dyn MemberLink>).collect();
    Replicator::start(f, base, None, links, || {})
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
            .map(|&(log, offset, bytes)| (log, offset, Arc::from(bytes)))
            .collect(),
    }
}

/// A backing source that fails when read on catch-up after hot eviction breaks the replicator:
/// nothing more is sent, its commits wait for the bucket (degraded).
#[test]
fn a_failing_source_breaks_the_replicator() {
    struct Failing;
    impl LogSource for Failing {
        fn read(&self, _: u64, _: u64, _: u64) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::other("the disk failed"))
        }
    }
    let members = members(2);
    members[0].fail(Some(LinkError::Down("lagging".into())));
    members[1].fail(Some(LinkError::Down("lagging".into())));
    let links = members.iter().map(|member| member.clone() as Arc<dyn MemberLink>).collect();
    let replicator = Replicator::start(1, 0, Some(Arc::new(Failing)), links, || {});
    // Exceed HOT_BYTES so the first chunk is evicted from the hot window once persisted.
    let big = vec![b'a'; (HOT_BYTES as usize / 2) + 4096];
    for i in 0..3u64 {
        let log = log_of(i + 1);
        replicator
            .flushed(Flushed { epoch: i + 1, appends: vec![(log, 0, Arc::from(big.as_slice()))] });
        replicator.persisted();
    }
    assert!(replicator.hot_bytes() <= HOT_BYTES);
    members[0].fail(None);
    until("broken by source read failure", || replicator.broken().is_some());
    assert!(replicator.degraded());
    assert_eq!(members[0].log(log_of(1)), b"");
}

/// Hot bytes evicted past HOT_BYTES are streamed from the backing LogSource when a lagging or
/// replacement member catches up (board 6902).
#[test]
fn evicted_hot_bytes_stream_from_log_source() {
    struct MemSource(Mutex<BTreeMap<u64, Vec<u8>>>);
    impl LogSource for MemSource {
        fn read(&self, log: u64, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
            let map = self.0.lock().unwrap();
            let Some(bytes) = map.get(&log) else { return Ok(Vec::new()) };
            let start = (offset as usize).min(bytes.len());
            let end = start.saturating_add(len as usize).min(bytes.len());
            Ok(bytes[start..end].to_vec())
        }
    }
    let backing = Arc::new(MemSource(Mutex::default()));
    let members = members(2);
    members[1].fail(Some(LinkError::Down("lagging".into())));
    let links = members.iter().map(|member| member.clone() as Arc<dyn MemberLink>).collect();
    let replicator = Replicator::start(1, 0, Some(backing.clone()), links, || {});
    let chunk_len = (HOT_BYTES as usize / 2) + 4096;
    for i in 0..3u64 {
        let log = log_of(i + 1);
        let data = vec![(i as u8) + 1; chunk_len];
        backing.0.lock().unwrap().insert(log, data.clone());
        replicator.flushed(Flushed { epoch: i + 1, appends: vec![(log, 0, Arc::from(data))] });
        replicator.persisted();
    }
    until("caught-up member replicated", || replicator.replicated() == 3);
    assert!(replicator.hot_bytes() <= HOT_BYTES);
    members[1].fail(None);
    until("lagging member caught up from LogSource", || {
        (0..3u64).all(|i| members[1].log(log_of(i + 1)) == vec![(i as u8) + 1; chunk_len])
    });
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
    let replicator = Replicator::start(1, 0, None, links, || {});
    replicator.flushed(flushed(1, &[(log_of(3), 0, b"records"), (own_of(3), 0, b"own")]));
    until("both sent", || ordered.0.lock().unwrap().len() == 2);
    assert_eq!(*ordered.0.lock().unwrap(), [own_of(3), log_of(3)]);
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
    let replicator = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one")]));
    until("the first commit replicated", || replicator.replicated() == 1);
    old[1].fail(Some(LinkError::Down("lost".into())));

    let new = members(2);
    for member in &new {
        member.fail(Some(LinkError::Down("not yet".into())));
    }
    replicator.replacing(1, links(&new));
    assert!(!replicator.installed(0), "installed before a new member held the tail");
    replicator.flushed(flushed(2, &[(2, 3, b"two")]));
    until("the old configuration holds it", || old[0].log(2) == b"onetwo");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(replicator.replicated(), 1, "told on the old configuration alone");

    new[0].fail(None);
    until("the new member holds the tail", || new[0].log(2) == b"onetwo");
    until("replicated on both", || replicator.replicated() == 2);
    assert!(replicator.installed(0));

    replicator.swapped();
    old[0].fail(Some(LinkError::Down("gone".into())));
    replicator.flushed(flushed(3, &[(2, 6, b"three")]));
    until("replicated on the new one alone", || replicator.replicated() == 3);
    assert_eq!(old[0].log(2), b"onetwo", "a retired member is sent nothing more");
    assert_eq!(replicator.members_up(), 1);
}

/// TASK-537.3.5 (doc-21, Degraded): a drop to f = 0 holds no quorum, so nothing more is told
/// replicated from its start (TLC's zeroNoQuorum), and it may be swapped in only once the bucket
/// holds what was (zeroAfterUpload). Swapped, the replicator is degraded: its commits wait for the
/// bucket.
#[test]
fn drops_to_f_zero_wait_for_the_bucket_and_tell_nothing_replicated() {
    let old = members(2);
    let replicator = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one")]));
    until("replicated", || replicator.replicated() == 1);
    replicator.replacing(0, Vec::new());
    replicator.flushed(flushed(2, &[(2, 3, b"two")]));
    until("the old configuration holds it", || old[0].log(2) == b"onetwo");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(replicator.replicated(), 1, "told replicated while dropping to f = 0");
    assert!(!replicator.installed(0), "dropped before the bucket held what was told");
    assert!(replicator.installed(1));
    replicator.swapped();
    assert!(replicator.degraded());
    assert_eq!(replicator.f(), 0);
}

/// TASK-537.3.5's review: a replacement whose members refuse every append is no configuration
/// to swap in, though the bucket holds all that was told.
#[test]
fn replacements_whose_members_refuse_are_not_installed() {
    let old = members(2);
    let replicator = start(1, 0, &old);
    replicator.flushed(flushed(1, &[(2, 0, b"one")]));
    until("replicated", || replicator.replicated() == 1);
    let new = members(2);
    for member in &new {
        member.fail(Some(LinkError::Refused("full".into())));
    }
    replicator.replacing(1, links(&new));
    replicator.flushed(flushed(2, &[(2, 3, b"two")]));
    std::thread::sleep(Duration::from_millis(100));
    assert!(!replicator.installed(2), "installed on the bucket alone");
    assert_eq!(replicator.abandon(), vec![0, 1]);
}

/// TASK-537.3.5's review: a member down, whose first answer back is a drop, nothing else left to
/// send it, is up again.
#[test]
fn members_back_with_a_drop_are_up() {
    let members = members(2);
    let replicator = start(1, 0, &members);
    replicator.flushed(flushed(1, &[(2, 0, b"one")]));
    until("replicated", || replicator.replicated() == 1);
    members[1].fail(Some(LinkError::Down("out of reach".into())));
    replicator.flushed(flushed(2, &[(2, 3, b"two")]));
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
    let whole = [own_frame(1, b"one"), own_frame(2, b"")].concat();
    assert_eq!(own_frames(&whole), [(1, b"one".to_vec()), (2, Vec::new())]);
    assert_eq!(own_frames_end(&whole), whole.len());
    for len in [4, u64::MAX, u64::MAX - 23, 1 << 63] {
        let mut bytes = whole.clone();
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(b"abc");
        assert_eq!(own_frames(&bytes).len(), 2, "a frame of {len} bytes");
        assert_eq!(own_frames_end(&bytes), whole.len());
    }
    // Corrupted payload byte stops at the frame before it.
    let mut bad = whole.clone();
    bad[25] ^= 1;
    assert_eq!(own_frames(&bad), []);
    assert_eq!(own_frames_end(&bad), 0);
    // Cut short anywhere: the first frame whole from its 27th byte, the second at the 51st.
    for cut in 0..=whole.len() {
        let expected = match cut {
            0..27 => 0,
            27..51 => 1,
            _ => 2,
        };
        assert_eq!(own_frames(&whole[..cut]).len(), expected, "cut at {cut}");
    }
}

/// Board 6902 (Codex review): replication hot buffers across multiple replicators (including
/// pinned take-time seeds larger than `HOT_BYTES`) are counted by `replicate::hot_bytes()`,
/// capped by `size_hot_to()`, and reclaimed by `evict_hot_to()` while lagging peers still catch
/// up cleanly from `LogSource`.
#[test]
fn hot_buffers_are_counted_and_evicted_by_global_budget() {
    struct MemSource(Mutex<BTreeMap<u64, Vec<u8>>>);
    impl LogSource for MemSource {
        fn read(&self, log: u64, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
            let logs = self.0.lock().unwrap();
            let Some(data) = logs.get(&log) else { return Ok(Vec::new()) };
            let start = (offset as usize).min(data.len());
            let end = (offset.saturating_add(len) as usize).min(data.len());
            Ok(data[start..end].to_vec())
        }
    }
    let _reset = {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                size_hot_to(u64::MAX);
            }
        }
        Reset
    };
    let baseline = hot_bytes();

    let mut replicators = Vec::new();
    let mut all_members = Vec::new();
    let chunk = vec![0x42u8; 4 << 20];
    for r in 0..2u64 {
        let src = Arc::new(MemSource(Mutex::new(BTreeMap::new())));
        let ms = members(2);
        for m in &ms {
            m.fail(Some(LinkError::Down("gated".into())));
        }
        let links = ms.iter().map(|m| m.clone() as Arc<dyn MemberLink>).collect();
        let rep = Replicator::start(1, 0, Some(src.clone()), links, || {});
        let log = log_of(r + 1);
        src.0.lock().unwrap().insert(log, chunk.clone());
        rep.flushed(Flushed { epoch: 1, appends: vec![(log, 0, Arc::from(chunk.as_slice()))] });
        rep.persisted();
        replicators.push((rep, log));
        all_members.push(ms);
    }

    // Include a take-time seed larger than HOT_BYTES so its bytes are counted in the global total.
    let seed_len = (HOT_BYTES as usize) + (1 << 20);
    let seed_ms = members(2);
    for m in &seed_ms {
        m.fail(Some(LinkError::Down("gated".into())));
    }
    let seed_links = seed_ms.iter().map(|m| m.clone() as Arc<dyn MemberLink>).collect();
    let seeded = Replicator::start(1, 0, None, seed_links, || {});
    seeded.seed(1, vec![(log_of(10), vec![0x77u8; seed_len])]);

    let expected_total = 2 * chunk.len() as u64 + seed_len as u64;
    assert!(
        hot_bytes() >= baseline + expected_total,
        "global hot_bytes {} missing replicators/seed ({expected_total})",
        hot_bytes()
    );

    // Governor shrinks budget to 0: all persisted non-pinned chunks across both replicators must be
    // evicted, leaving only the pinned seed counted.
    size_hot_to(0);
    let freed = evict_hot_to(0);
    assert!(freed >= 2 * chunk.len() as u64, "freed {freed}");
    for (rep, _) in &replicators {
        assert_eq!(rep.hot_bytes(), 0);
    }
    assert_eq!(seeded.hot_bytes(), seed_len as u64);

    // Re-enabling peers lets them catch up cleanly from MemSource.
    for (ms, (rep, log)) in all_members.iter().zip(&replicators) {
        for m in ms {
            m.fail(None);
        }
        until("caught up from source", || rep.replicated() == 1);
        assert_eq!(ms[0].log(*log).len(), chunk.len());
    }

    // Acknowledging the seed drops its pinned bytes from global accounting too.
    seeded.acknowledged(10);
    assert_eq!(seeded.hot_bytes(), 0);
}
