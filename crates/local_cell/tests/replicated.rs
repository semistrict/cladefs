// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-537.3.3 (doc-21): what a member holds of a replicated cell's writer's logs recovers. A
//! store's commits, made after its replicator started, are sent to its members; its host lost,
//! what one member holds, written into a new store's directory as its write-ahead log
//! (`replicate::restore`), opens holding every one of them, beside the bucket's.

mod common;

use bucket_cell::fetch::{Budget, Limits};
use bucket_cell::layout::test_cell;
use bucket_cell::object_bucket::ObjectBucket;
use local_cell::replicate::{self, LinkError, MemberLink, Replicator};
use local_cell::upload::{self, Runtime, Uploader};
use local_cell::{Description, LocalCell, MemDir, Options, Recovering};
use object_store::memory::InMemory;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use storage_device::ExtentId;

const BLOCK: usize = 4096;
const URL: &str = "memory:///replicated";
const DESCRIPTION: Description =
    Description { block_size: BLOCK as u32, capacity: 16 << 20, cell: test_cell(3) };

/// Small segments, so that commits span them, and a small bound for extents stored on their own,
/// so that some are.
/// Flushes of either kind, holding their runs or checking them, as the crash tests' (TASK-301).
const OPTIONS: Options = Options {
    max_extent: 48 << 10,
    segment_size: 16 << 10,
    segment_age: Duration::from_secs(3600),
    large_extent: 24 << 10,
    open_files: 3,
    view_lag: None,
    view_kept_bytes: None,
    held_most: 8 << 10,
};

/// A member in memory.
#[derive(Default)]
struct Member {
    logs: Mutex<BTreeMap<u64, Vec<u8>>>,
}

impl MemberLink for Member {
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> Result<u64, LinkError> {
        let mut logs = self.logs.lock().unwrap();
        let held = logs.entry(log).or_default();
        if offset > held.len() as u64 {
            return Ok(held.len() as u64);
        }
        held.truncate(offset as usize);
        held.extend_from_slice(bytes);
        Ok(held.len() as u64)
    }

    fn drop_through(&self, through: u64) -> Result<(), LinkError> {
        self.logs.lock().unwrap().retain(|&log, _| log > through);
        Ok(())
    }
}

/// A member out of reach.
struct Down;

impl MemberLink for Down {
    fn append(&self, _: u64, _: u64, _: &[u8]) -> Result<u64, LinkError> {
        Err(LinkError::Down("out of reach".into()))
    }

    fn drop_through(&self, _: u64) -> Result<(), LinkError> {
        Err(LinkError::Down("out of reach".into()))
    }
}

struct Host {
    runtime: tokio::runtime::Runtime,
    bucket: Arc<ObjectBucket>,
}

impl Host {
    fn open(
        &self,
        recovering: Recovering<MemDir>,
        holder: u64,
    ) -> (Arc<LocalCell<MemDir>>, Uploader<MemDir>) {
        let runtime = Runtime {
            handle: self.runtime.handle().clone(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        // Nothing uploaded but what is asked for: the members hold the rest.
        let options =
            upload::Options { interval: Duration::from_secs(3600), ..upload::Options::default() };
        upload::open(recovering, OPTIONS, self.bucket.clone(), (holder, options), &runtime).unwrap()
    }
}

/// A runtime for a host the test loses.
fn runtime_of_its_own() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

/// Loses `host`, and what of it `leaked` is: its runtime stopped, nothing of its uploads going on,
/// and the rest left as a host's crash leaves it, never dropped (a drop would upload, or close).
fn lose(host: Host, leaked: impl Sized) {
    std::mem::forget(leaked);
    host.runtime.shutdown_background();
}

fn until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(20), "waited for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Host {
    /// Every extent `store` holds, read from it, or from the bucket where it says it is, and its
    /// root.
    fn contents(&self, store: &LocalCell<MemDir>) -> (BTreeMap<ExtentId, Vec<u8>>, Vec<u8>) {
        let mut extents = BTreeMap::new();
        for (extent, len) in store.extents().unwrap() {
            let mut data = vec![0; len as usize];
            if let Err(err) = store.read(extent, 0, &mut data) {
                let local_cell::BucketData { location, .. } = err
                    .downcast_ref::<local_cell::BucketData>()
                    .cloned()
                    .unwrap_or_else(|| panic!("{err:#}"));
                let read = bucket_cell::testing::read(&self.bucket, extent, &location);
                data = self.runtime.block_on(read).unwrap();
            }
            extents.insert(extent, data);
        }
        (extents, store.root().unwrap())
    }
}

#[test]
fn what_a_member_holds_recovers_on_another_host() {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
    let runtime = runtime.unwrap();
    let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
    let host = Host { runtime, bucket: bucket.clone() };
    let cell = DESCRIPTION.cell;
    host.runtime.block_on(bucket_cell::lifecycle::create(&*host.bucket, cell, 1)).unwrap();
    // The writer's host, on a runtime of its own, which goes with it: nothing of it uploads after.
    let lost = Host { runtime: runtime_of_its_own(), bucket };

    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let (store, uploader) = lost.open(LocalCell::recover_unmade(dir).unwrap(), 1);
    store.made().unwrap();
    // What it made opening, uploaded, its segment closed: what no member holds is in the bucket.
    let base = store.last_commit().unwrap();
    uploader.wait_uploaded(base).unwrap();
    store.close_segment().unwrap();
    let member = Arc::new(Member::default());
    let links: Vec<Arc<dyn MemberLink>> = vec![member.clone(), Arc::new(Down)];
    let replicator = Replicator::start(1, base, store.spool().unwrap(), links, uploader.waker());
    store.replicate_to(replicator.clone());

    // Commits spanning segments, and extents stored on their own.
    let mut sequence = store.info().unwrap().next_sequence;
    for round in 0..12u64 {
        let len = if round % 3 == 0 { 40 << 10 } else { BLOCK * (1 + round as usize % 4) };
        let extent = ExtentId { cell: cell.id, sequence };
        sequence += 1;
        store.write(extent, 0, &common::noise(round, len)).unwrap();
        store.commit_ordered(Some(format!("root {round}").as_bytes())).unwrap();
    }
    store.flush().unwrap();
    let last = store.last_commit().unwrap();
    until("the commits replicated", || replicator.told(uploader.last_uploaded()) >= last);
    uploader.wait_replicated(last).unwrap();
    let expected = lost.contents(&store);
    assert!(member.logs.lock().unwrap().keys().any(|log| log & 1 == 1), "own extents sent");
    // What it sends is spooled on the store's disk, not held in memory (TASK-537.3.3).
    assert!(replicator.spooled() > 0);

    // The host is lost: what the member holds, and the bucket, are all there is.
    lose(lost, (uploader, store, replicator));
    let bucket = host.runtime.block_on(bucket_cell::recover(&*host.bucket, cell, None)).unwrap();
    let uploaded = bucket.state.last_commit.map_or(0, extent_frame::segment::epoch_of);
    assert!(uploaded < last, "the bucket holds commit {uploaded}, of {last}: nothing to recover");
    let logs: Vec<(u64, Vec<u8>)> = member.logs.lock().unwrap().clone().into_iter().collect();
    // TASK-537.3.5's review: a member's partial copy of a segment the head names, its drop never
    // come, is the bucket's already, not a hole.
    let first = logs.iter().find(|(log, _)| log & 1 == 0).unwrap().0 >> 1;
    let whole: Vec<_> = logs.iter().filter(|(log, _)| log >> 1 == first).cloned().collect();
    let named = replicate::last_commit(cell, &whole);
    let mut partial = logs.clone();
    let cut = partial.iter_mut().find(|(log, _)| *log == replicate::log_of(first)).unwrap();
    cut.1.truncate(cut.1.len() * 2 / 3);
    let kept = replicate::trimmed(cell, &partial, named, None).unwrap();
    assert!(kept.iter().all(|(log, _)| log >> 1 > first) && !kept.is_empty());
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    // As a take reads it: the last commit the head names, which members' logs go on from, or
    // begin before, having dropped what it names later than it named it.
    let recovered = host.runtime.block_on(bucket_cell::recover(&*host.bucket, cell, None)).unwrap();
    replicate::restore(&elsewhere, cell, &logs, recovered.state.last_commit).unwrap();
    let (taken, uploader) = host.open(LocalCell::recover_unmade(elsewhere).unwrap(), 2);
    taken.made().unwrap();
    assert!(taken.last_commit().unwrap() >= last);
    assert_eq!(host.contents(&taken), expected);
    drop(uploader);
}

/// Hosts' replica directories in memory, by address: what each host's [`Peers`] reaches.
type Directories = BTreeMap<String, Arc<local_cell::replica::Replicas<MemDir>>>;

/// The hosts out of reach, by address.
type Lost = Arc<Mutex<std::collections::BTreeSet<String>>>;

/// A host's peers, as its daemon's members are (`local_cell::upload::Members`), reaching the
/// replica directories in memory directly, but those `lost`.
struct Peers {
    me: String,
    all: Directories,
    lost: Lost,
}

impl Peers {
    fn reach(&self, address: &str) -> Result<&Arc<local_cell::replica::Replicas<MemDir>>, String> {
        match self.lost.lock().unwrap().contains(address) {
            true => Err(format!("{address} lost")),
            false => self.all.get(address).ok_or_else(|| format!("{address} unknown")),
        }
    }
}

fn key(configuration: &bucket_cell::layout::Configuration) -> local_cell::replica::Key {
    local_cell::replica::Key { number: configuration.number, epoch: configuration.epoch }
}

/// A writer's link to a member in memory.
struct Link {
    replicas: Arc<local_cell::replica::Replicas<MemDir>>,
    of: (bucket_cell::layout::CellId, local_cell::replica::Key, u64),
    address: String,
    lost: Lost,
}

impl Link {
    fn reach(&self) -> Result<(), LinkError> {
        match self.lost.lock().unwrap().contains(&self.address) {
            true => Err(LinkError::Down(format!("{} lost", self.address))),
            false => Ok(()),
        }
    }
}

fn link_error(err: local_cell::replica::Error) -> LinkError {
    use local_cell::replica::{Error, Refused};
    match err {
        Error::Refused(Refused::Fenced { promise }) => LinkError::Fenced(format!("at {promise}")),
        Error::Refused(refused) => LinkError::Refused(refused.to_string()),
        Error::Io(err) => LinkError::Down(format!("{err:#}")),
    }
}

impl MemberLink for Link {
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> Result<u64, LinkError> {
        self.reach()?;
        self.replicas.append(self.of, log, offset, bytes).map_err(link_error)
    }

    fn drop_through(&self, through: u64) -> Result<(), LinkError> {
        self.reach()?;
        self.replicas.drop_logs(self.of, Some(through)).map_err(link_error)
    }
}

impl local_cell::upload::Members for Peers {
    fn me(&self) -> Result<bucket_cell::layout::Member, String> {
        Ok(bucket_cell::layout::Member {
            address: self.me.clone(),
            incarnation: self.all[&self.me].incarnation(),
        })
    }

    fn pick(
        &self,
        _: bucket_cell::layout::CellId,
        _: u64,
        f: u8,
        avoid: &[String],
    ) -> Result<Vec<bucket_cell::layout::Member>, String> {
        let lost = self.lost.lock().unwrap().clone();
        let others = self.all.iter().filter(|(address, _)| {
            **address != self.me && !lost.contains(*address) && !avoid.contains(address)
        });
        let picked: Vec<_> = others
            .take(2 * usize::from(f))
            .map(|(address, replicas)| bucket_cell::layout::Member {
                address: address.clone(),
                incarnation: replicas.incarnation(),
            })
            .collect();
        match picked.len() == 2 * usize::from(f) {
            true => Ok(picked),
            false => Err(format!("{} peers", picked.len())),
        }
    }

    fn link(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Arc<dyn MemberLink> {
        let replicas = self.all[&member.address].clone();
        let of = (cell, key(configuration), member.incarnation);
        Arc::new(Link { replicas, of, address: member.address.clone(), lost: self.lost.clone() })
    }

    fn fence(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
        epoch: u64,
    ) -> Result<Vec<(u64, u64)>, String> {
        let replicas = self.reach(&member.address)?;
        let of = (cell, key(configuration), member.incarnation);
        replicas.fence(of, epoch).map_err(|err| err.to_string())
    }

    fn read(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
        log: u64,
    ) -> Result<Vec<u8>, String> {
        let replicas = self.reach(&member.address)?;
        let of = (cell, key(configuration), member.incarnation);
        replicas.read(of, log, 0, u64::MAX).map_err(|err| err.to_string())
    }

    fn drop_before(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Result<(), String> {
        let replicas = self.reach(&member.address)?;
        let of = (cell, key(configuration), member.incarnation);
        replicas.drop_logs(of, None).map_err(|err| err.to_string())
    }
}

impl Host {
    /// Opens the store `recovering` recovered as `holder`, a replicated cell's, asking for
    /// `replication` if any, its members `peers`.
    fn open_replicated(
        &self,
        recovering: Recovering<MemDir>,
        holder: u64,
        replication: Option<bucket_cell::layout::Wish>,
        peers: Peers,
    ) -> Result<(Arc<LocalCell<MemDir>>, Uploader<MemDir>), upload::Failure> {
        let runtime = Runtime {
            handle: self.runtime.handle().clone(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let options = upload::Options {
            interval: Duration::from_secs(3600),
            replication,
            members: Some(upload::MembersOf(Arc::new(peers))),
            // Members replaced, and cells dropped to f = 0, within the tests' patience.
            replace_after: Duration::from_secs(1),
            ..upload::Options::default()
        };
        upload::open(recovering, OPTIONS, self.bucket.clone(), (holder, options), &runtime)
    }
}

/// TASK-537.3.4 (doc-21): a replicated cell's writer's host lost, its disk with it, another host
/// takes the cell from its members and the bucket alone: it fences them, reads the longest copy
/// of each log, recovers every commit its guest was told replicated, gives them to its own
/// configuration's members, and activates that, the head naming it.
#[test]
fn takes_recover_what_a_lost_writers_members_hold() {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
    let host = Host {
        runtime: runtime.unwrap(),
        bucket: Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "")),
    };
    let cell = DESCRIPTION.cell;
    host.runtime.block_on(bucket_cell::lifecycle::create(&*host.bucket, cell, 1)).unwrap();
    let directories = |names: &[&str]| -> Directories {
        let replicas =
            |_| Arc::new(local_cell::replica::Replicas::open(MemDir::new(), 1 << 30).unwrap());
        names.iter().map(|name| (name.to_string(), replicas(name))).collect()
    };
    let all = directories(&["a", "b", "c", "d"]);
    let peers = |me: &str, all: &Directories| Peers {
        me: me.into(),
        all: all.clone(),
        lost: Lost::default(),
    };

    // Host a takes the cell, replicated with f = 1: b and c its members.
    let lost = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let (store, uploader) = lost
        .open_replicated(LocalCell::recover_unmade(dir).unwrap(), 1, wish, peers("a", &all))
        .unwrap();
    let replicator = store.replicator().expect("replicating");
    // What it made opening, uploaded, its segment closed and uploaded too, the uploader left with
    // nothing to close: the commits after are on the members alone.
    store.made().unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    let mut sequence = store.info().unwrap().next_sequence;
    for round in 0..9u64 {
        let len = if round % 3 == 0 { 40 << 10 } else { BLOCK * (1 + round as usize % 4) };
        store.write(ExtentId { cell: cell.id, sequence }, 0, &common::noise(round, len)).unwrap();
        sequence += 1;
        store.commit_ordered(Some(format!("root {round}").as_bytes())).unwrap();
    }
    store.flush().unwrap();
    let last = store.last_commit().unwrap();
    uploader.wait_replicated(last).unwrap();
    let expected = lost.contents(&store);

    // Host a is lost, its disk and replica directory with it; d takes the cell from b and c.
    lose(lost, (uploader, store, replicator));
    // The bucket holds not all of them: the take must recover the rest from the members.
    let bucket = host.runtime.block_on(bucket_cell::recover(&*host.bucket, cell, None)).unwrap();
    let uploaded = bucket.state.last_commit.map_or(0, extent_frame::segment::epoch_of);
    assert!(uploaded < last, "the bucket holds commit {uploaded}, of {last}: nothing to recover");
    let mut left = all.clone();
    left.remove("a");
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let (taken, uploader) = host.open_replicated(recovering, 2, None, peers("d", &left)).unwrap();
    taken.made().unwrap();
    assert!(taken.last_commit().unwrap() >= last);
    assert_eq!(host.contents(&taken), expected);

    // The head names d's configuration, of b and c, which hold what a's held.
    let head = host.runtime.block_on(bucket_cell::head::get(&*host.bucket, cell)).unwrap().unwrap();
    let configuration = head.replication.unwrap().configuration.unwrap();
    let addresses: Vec<&str> = configuration.members.iter().map(|m| m.address.as_str()).collect();
    assert_eq!((configuration.number, addresses), (2, vec!["d", "b", "c"]));
    // d activated it only once b or c held the seed; b holds it still, or dropped it as the head
    // named what d uploaded of it since.
    let held =
        left["b"].fence((cell, key(&configuration), left["b"].incarnation()), head.epoch).unwrap();
    let now = host.runtime.block_on(bucket_cell::recover(&*host.bucket, cell, None)).unwrap();
    let uploaded = now.state.last_commit.map_or(0, extent_frame::segment::epoch_of);
    assert!(!held.is_empty() || uploaded >= last, "b holds the seed, or the bucket does");
    drop(uploader);
}

/// TASK-537.3.4: a take finding fewer than f+1 members of the configuration before, besides its
/// writer, to fence is refused: what only the others held may be commits told replicated.
#[test]
fn takes_without_enough_members_to_fence_are_refused() {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
    let host = Host {
        runtime: runtime.unwrap(),
        bucket: Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "")),
    };
    let cell = DESCRIPTION.cell;
    host.runtime.block_on(bucket_cell::lifecycle::create(&*host.bucket, cell, 1)).unwrap();
    let make = |_| Arc::new(local_cell::replica::Replicas::open(MemDir::new(), 1 << 30).unwrap());
    let all: Directories = ["a", "b", "c", "d"].iter().map(|n| (n.to_string(), make(n))).collect();
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let a = Peers { me: "a".into(), all: all.clone(), lost: Lost::default() };
    let (store, uploader) =
        host.open_replicated(LocalCell::recover_unmade(dir).unwrap(), 1, wish, a).unwrap();
    store.made().unwrap();
    drop((uploader, store));
    // a and c lost: b alone answers, of the two a take must fence.
    let mut left = all.clone();
    left.remove("a");
    left.remove("c");
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let d = Peers { me: "d".into(), all: left, lost: Lost::default() };
    let refused = host.open_replicated(recovering, 2, None, d).map(drop);
    let err = refused.expect_err("taken with one member fenced");
    assert!(err.to_string().contains("of the 2 members"), "{err}");
}

/// A member in memory whose appends wait at a gate until it opens.
#[derive(Default)]
struct Gated {
    member: Member,
    open: Mutex<bool>,
    opened: std::sync::Condvar,
}

impl Gated {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

impl MemberLink for Gated {
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> Result<u64, LinkError> {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
        drop(open);
        self.member.append(log, offset, bytes)
    }

    fn drop_through(&self, through: u64) -> Result<(), LinkError> {
        self.member.drop_through(through)
    }
}

/// TASK-537.3.3, Codex's review: a waiter for Replicated is woken by its members' answers, waiting
/// before they come, with nothing uploaded; and, with its members down, by the bucket, the cell
/// degraded asking for its upload.
#[test]
fn replicated_waiters_are_woken_by_members_or_the_bucket() {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
    let host = Host {
        runtime: runtime.unwrap(),
        bucket: Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "")),
    };
    let cell = DESCRIPTION.cell;
    host.runtime.block_on(bucket_cell::lifecycle::create(&*host.bucket, cell, 1)).unwrap();
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let (store, uploader) = host.open(LocalCell::recover_unmade(dir).unwrap(), 1);
    store.made().unwrap();
    store.close_segment().unwrap();
    let base = store.last_commit().unwrap();
    uploader.wait_uploaded(base).unwrap();
    let gated = Arc::new(Gated::default());
    let links: Vec<Arc<dyn MemberLink>> = vec![gated.clone(), Arc::new(Down)];
    let replicator = Replicator::start(1, base, store.spool().unwrap(), links, uploader.waker());
    store.replicate_to(replicator.clone());

    let extent = ExtentId { cell: cell.id, sequence: store.info().unwrap().next_sequence };
    store.write(extent, 0, &common::noise(1, BLOCK)).unwrap();
    store.commit_ordered(Some(b"one")).unwrap();
    store.flush().unwrap();
    let epoch = store.last_commit().unwrap();
    let uploader = Arc::new(uploader);
    let waiting = {
        let uploader = uploader.clone();
        std::thread::spawn(move || uploader.wait_replicated(epoch))
    };
    std::thread::sleep(Duration::from_millis(200));
    assert!(!waiting.is_finished(), "told replicated before a member held it");
    gated.open();
    waiting.join().unwrap().unwrap();
    assert!(uploader.last_uploaded() < epoch, "woken by its member, not the bucket");

    // Its members down: degraded, its waiters woken by the bucket.
    drop(replicator);
    let links: Vec<Arc<dyn MemberLink>> = vec![Arc::new(Down), Arc::new(Down)];
    let down = Replicator::start(1, epoch, store.spool().unwrap(), links, uploader.waker());
    store.replicate_to(down.clone());
    store.close_segment().unwrap();
    store
        .write(
            ExtentId { cell: cell.id, sequence: extent.sequence + 1 },
            0,
            &common::noise(2, BLOCK),
        )
        .unwrap();
    store.commit_ordered(Some(b"two")).unwrap();
    store.flush().unwrap();
    let epoch = store.last_commit().unwrap();
    until("degraded", || down.degraded());
    uploader.wait_replicated(epoch).unwrap();
    // The head named it as the waiter was woken: the replicator counts the logs it dropped as the
    // head named them, the uploader's progress said so a moment after.
    until("woken by the bucket", || uploader.last_uploaded() >= epoch);
}

/// A host on a runtime of its own, its bucket in memory, and a replicated cell made in it.
fn replicated_host() -> Host {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
    let host = Host {
        runtime: runtime.unwrap(),
        bucket: Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "")),
    };
    let cell = DESCRIPTION.cell;
    host.runtime.block_on(bucket_cell::lifecycle::create(&*host.bucket, cell, 1)).unwrap();
    host
}

/// Replica directories in memory, by the hosts' names.
fn directories(names: &[&str]) -> Directories {
    let replicas =
        || Arc::new(local_cell::replica::Replicas::open(MemDir::new(), 1 << 30).unwrap());
    names.iter().map(|name| (name.to_string(), replicas())).collect()
}

/// The configuration the cell's head names.
fn configuration_named(host: &Host) -> bucket_cell::layout::Configuration {
    let head = host.runtime.block_on(bucket_cell::head::get(&*host.bucket, DESCRIPTION.cell));
    head.unwrap().unwrap().replication.unwrap().configuration.unwrap()
}

fn addresses(configuration: &bucket_cell::layout::Configuration) -> Vec<&str> {
    configuration.members.iter().map(|member| member.address.as_str()).collect()
}

/// Commits `rounds` writes of `store`'s, from `round`, flushed: the last's epoch.
fn commit(store: &LocalCell<MemDir>, round: u64, rounds: u64) -> u64 {
    let mut sequence = store.info().unwrap().next_sequence;
    for round in round..round + rounds {
        let len = if round % 3 == 0 { 40 << 10 } else { BLOCK * (1 + round as usize % 4) };
        let extent = ExtentId { cell: DESCRIPTION.cell.id, sequence };
        store.write(extent, 0, &common::noise(round, len)).unwrap();
        sequence += 1;
        store.commit_ordered(Some(format!("root {round}").as_bytes())).unwrap();
    }
    store.flush().unwrap();
    store.last_commit().unwrap()
}

/// TASK-537.3.5 (doc-21, Replacing a member): a member lost, its writer replaces it, by a swap of
/// the head to a configuration of the survivor and a new member, given the tail; its commits stay
/// replicated, the bucket holding none of them; and the writer lost too, a take recovers them
/// from the new configuration.
#[test]
fn lost_members_are_replaced_and_commits_stay_replicated() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c", "d", "e"]);
    let lost = Lost::default();
    let peers = |me: &str| Peers { me: me.into(), all: all.clone(), lost: lost.clone() };
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers("a")).unwrap();
    store.made().unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    assert_eq!(addresses(&configuration_named(&host)), ["a", "b", "c"]);
    let last = commit(&store, 0, 4);
    uploader.wait_replicated(last).unwrap();

    // b lost: c alone holds what comes next, until the writer replaces b.
    lost.lock().unwrap().insert("b".into());
    let last = commit(&store, 4, 3);
    uploader.wait_replicated(last).unwrap();
    until("b replaced", || configuration_named(&host).number == 2);
    let configuration = configuration_named(&host);
    assert_eq!((configuration.f, addresses(&configuration)), (1, vec!["a", "c", "d"]));
    // The head swapped, the writer learns so a moment after.
    until("swapped", || {
        let status = uploader.status().unwrap().replication.unwrap();
        !status.degraded && !status.replacing && status.members_up == 2
    });

    // c lost too: d, given the tail, holds every commit.
    lost.lock().unwrap().insert("c".into());
    let last = commit(&store, 7, 3);
    uploader.wait_replicated(last).unwrap();
    assert!(uploader.last_uploaded() < last, "told replicated by the bucket, not d");
    let expected = writer.contents(&store);
    let replicator = store.replicator().unwrap();
    lose(writer, (uploader, store, replicator));

    // a lost, and c back: e takes the cell, fencing c and d, and recovers every commit.
    lost.lock().unwrap().extend(["a".to_string()]);
    lost.lock().unwrap().remove("c");
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let (taken, uploader) = host.open_replicated(recovering, 2, None, peers("e")).unwrap();
    taken.made().unwrap();
    assert!(taken.last_commit().unwrap() >= last);
    assert_eq!(host.contents(&taken), expected);
    drop(uploader);
}

/// TASK-537.3.5 (doc-21, Degraded): its members lost, and none to replace them, a replicated
/// cell is degraded, its commits told durable once the bucket holds them, uploaded at once; it
/// drops to f = 0 once the bucket holds all it told; its members back, it is replicated again.
#[test]
fn degraded_cells_drop_to_f_zero_and_come_back() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c"]);
    let lost = Lost::default();
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let a = Peers { me: "a".into(), all: all.clone(), lost: lost.clone() };
    let (store, uploader) = host.open_replicated(recovering, 1, wish, a).unwrap();
    store.made().unwrap();
    let last = commit(&store, 0, 3);
    uploader.wait_replicated(last).unwrap();

    lost.lock().unwrap().extend(["b".to_string(), "c".to_string()]);
    let last = commit(&store, 3, 3);
    // Woken by the bucket: the interval an hour, its upload asked for at once as it is degraded.
    uploader.wait_replicated(last).unwrap();
    // The head named it as the waiter was woken; the uploader's progress says so a moment after.
    until("uploaded", || uploader.last_uploaded() >= last);
    until("degraded", || uploader.status().unwrap().replication.unwrap().degraded);
    until("dropped to f = 0", || configuration_named(&host).f == 0);
    let configuration = configuration_named(&host);
    assert_eq!((configuration.number, addresses(&configuration)), (2, vec!["a"]));
    until("swapped", || {
        let status = uploader.status().unwrap().replication.unwrap();
        status.degraded && status.f == 0
    });
    // Its commits go on, each waiting for the bucket, uploaded at once.
    let last = commit(&store, 6, 2);
    uploader.wait_replicated(last).unwrap();

    lost.lock().unwrap().clear();
    until("replicated again", || configuration_named(&host).f == 1);
    let configuration = configuration_named(&host);
    assert_eq!((configuration.number, addresses(&configuration)), (3, vec!["a", "b", "c"]));
    until("degraded no more", || !uploader.status().unwrap().replication.unwrap().degraded);
    let last = commit(&store, 8, 2);
    uploader.wait_replicated(last).unwrap();
    assert!(uploader.last_uploaded() < last, "told replicated by the bucket, not its members");

    // Its members lost again, it drops to f = 0 again; its writer gone, a take needs no peers
    // (Codex's review): it serves degraded, its commits waiting for the bucket.
    lost.lock().unwrap().extend(["b".to_string(), "c".to_string()]);
    // A writer finds its members lost as it sends them what it commits.
    let last = commit(&store, 10, 1);
    uploader.wait_replicated(last).unwrap();
    until("dropped to f = 0 again", || configuration_named(&host).f == 0);
    let expected = host.contents(&store);
    drop((uploader, store));
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let a = Peers { me: "a".into(), all: all.clone(), lost: lost.clone() };
    let (taken, uploader) = host.open_replicated(recovering, 2, None, a).unwrap();
    taken.made().unwrap();
    assert_eq!(host.contents(&taken), expected);
    let status = uploader.status().unwrap().replication.unwrap();
    assert!(status.degraded && status.f == 0, "{status:?}");
    let last = commit(&taken, 11, 2);
    uploader.wait_replicated(last).unwrap();
    until("uploaded", || uploader.last_uploaded() >= last);
    drop(uploader);
}

/// TASK-537.3.5 (doc-21, Degraded; TLC's zeroAfterUpload): a take asking for f = 0 of a cell
/// replicated recovers from its members, as any take of it, then drops to f = 0 once the bucket
/// holds what its configuration held: the cell replicated no more.
#[test]
fn takes_asking_for_f_zero_drop_once_the_bucket_holds_what_was_told() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c", "d"]);
    let lost = Lost::default();
    let peers = |me: &str| Peers { me: me.into(), all: all.clone(), lost: lost.clone() };
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers("a")).unwrap();
    store.made().unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    let last = commit(&store, 0, 5);
    uploader.wait_replicated(last).unwrap();
    let expected = writer.contents(&store);
    let replicator = store.replicator().unwrap();
    lose(writer, (uploader, store, replicator));
    lost.lock().unwrap().insert("a".into());

    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let wish = bucket_cell::layout::Wish::new(0, bucket_cell::layout::Fsync::Local);
    let (taken, uploader) = host.open_replicated(recovering, 2, wish, peers("d")).unwrap();
    taken.made().unwrap();
    assert_eq!(host.contents(&taken), expected);
    // Its commits restamped at its epoch, the members' recovered among them.
    let recovered = taken.last_commit().unwrap();
    assert!(recovered >= last);
    until("dropped to f = 0", || configuration_named(&host).f == 0);
    until("replicated no more", || taken.replicator().is_none());
    assert!(uploader.last_uploaded() >= recovered, "dropped to f = 0 before the bucket held it");
    let last = commit(&taken, 5, 2);
    uploader.wait_replicated(last).unwrap();
    drop(uploader);
}

/// doc-21, Take, steps 5 and 6 (the spec's installOwn, its mutation InstallMembersOnly; TASK-678):
/// a writer whose daemon restarts takes its cell again on its own disk, its write-ahead log
/// holding commits told replicated that the one member it fences lacks (the member holding them
/// out of reach). The head names the configuration it makes, none of whose members holds them,
/// only once the bucket does, in the swap that acknowledges them: else its host lost too, a take
/// elsewhere reads that configuration, and the bucket, and loses them.
#[test]
fn retakes_on_the_writers_disk_install_what_its_own_log_holds() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c", "d", "e"]);
    let lost = Lost::default();
    let peers = |me: &str| Peers { me: me.into(), all: all.clone(), lost: lost.clone() };
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir.clone()).unwrap();
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers("a")).unwrap();
    store.made().unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    assert_eq!(addresses(&configuration_named(&host)), ["a", "b", "c"]);

    // c out of reach: b alone holds the commits, with a's own log.
    lost.lock().unwrap().insert("c".into());
    let last = commit(&store, 0, 5);
    uploader.wait_replicated(last).unwrap();
    assert!(uploader.last_uploaded() < last, "told replicated by b, not the bucket");
    let expected = writer.contents(&store);
    let replicator = store.replicator().unwrap();
    // a's daemon restarts before it replaces c, its disk kept.
    lose(writer, (uploader, store, replicator));
    assert_eq!(configuration_named(&host).number, 1, "c replaced before the restart");

    // b out of reach now, c back: a takes the cell again on its own disk, fencing c alone.
    lost.lock().unwrap().remove("c");
    lost.lock().unwrap().insert("b".into());
    let again = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let recovering = LocalCell::recover(dir.crash(1)).unwrap();
    let (retaken, uploader) = again.open_replicated(recovering, 2, None, peers("a")).unwrap();
    assert!(retaken.last_commit().unwrap() >= last);
    assert_eq!(again.contents(&retaken), expected);
    // Its configuration named once the bucket holds what its log held, uploaded at once.
    until("a's configuration named", || configuration_named(&host).number == 2);
    let configuration = configuration_named(&host);
    assert_eq!(addresses(&configuration), ["a", "c", "d"]);
    let bucket = host.runtime.block_on(bucket_cell::recover(&*host.bucket, cell(), None)).unwrap();
    let uploaded = bucket.state.last_commit.map_or(0, extent_frame::segment::epoch_of);
    assert!(uploaded >= last, "named, the bucket holding commit {uploaded}, of {last}");

    // a lost, its disk with it: e takes the cell from c, d and the bucket.
    let replicator = retaken.replicator().unwrap();
    lose(again, (uploader, retaken, replicator));
    lost.lock().unwrap().insert("a".into());
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let (taken, uploader) = host.open_replicated(recovering, 3, None, peers("e")).unwrap();
    taken.made().unwrap();
    assert!(taken.last_commit().unwrap() >= last);
    assert_eq!(host.contents(&taken), expected);
    drop(uploader);
}

/// The cell the tests' stores keep.
fn cell() -> bucket_cell::layout::CellId {
    DESCRIPTION.cell
}

/// TASK-681 (doc-21's row "Writer's host and a peer lost"): a replicated cell whose writer's host
/// and both its members' are lost for good can't be taken: fewer than f+1 members of its
/// configuration answer. A placement forced takes it from the bucket and what the members that
/// answer hold, none here, and its status says past which commit commits told replicated are lost.
#[test]
fn takes_of_cells_whose_members_are_lost_go_on_by_force() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c", "d", "e"]);
    let lost = Lost::default();
    let peers = |me: &str| Peers { me: me.into(), all: all.clone(), lost: lost.clone() };
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers("a")).unwrap();
    store.made().unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    assert_eq!(addresses(&configuration_named(&host)), ["a", "b", "c"]);
    let last = commit(&store, 0, 4);
    uploader.wait_replicated(last).unwrap();
    assert!(uploader.last_uploaded() < last, "told replicated by b and c, not the bucket");
    let replicator = store.replicator().unwrap();
    lose(writer, (uploader, store, replicator));
    let bucket = host.runtime.block_on(bucket_cell::recover(&*host.bucket, cell(), None)).unwrap();
    let uploaded = bucket.state.last_commit.map_or(0, extent_frame::segment::epoch_of);

    // a, b and c lost for good: d's take is refused, no member of a's configuration answering.
    lost.lock().unwrap().extend(["a", "b", "c"].map(String::from));
    let refused = MemDir::new();
    LocalCell::make(&refused, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(refused).unwrap();
    let failure = host.open_replicated(recovering, 2, None, peers("d")).err().unwrap();
    assert!(
        failure.to_string().contains("0 of the 2 members of configuration 1 a take must fence"),
        "{failure}"
    );

    // Placed forced, d takes it from the bucket.
    let forced = MemDir::new();
    LocalCell::make(&forced, DESCRIPTION, Some(URL), None).unwrap();
    local_cell::place(&forced, 7, true).unwrap();
    let recovering = LocalCell::recover_unmade(forced).unwrap();
    let (taken, uploader) = host.open_replicated(recovering, 3, None, peers("d")).unwrap();
    taken.made().unwrap();
    let taking = uploader.status().unwrap().taking.unwrap();
    assert_eq!(taking.replication.unwrap().forced_after, Some(uploaded));
    let next = commit(&taken, 4, 2);
    uploader.wait_replicated(next).unwrap();
    drop(uploader);
}

/// TASK-683 (the spec's Take, its `self`): a writer whose daemon restarts with every commit
/// uploaded, its write-ahead log empty, takes its cell again on its own disk, one member of its
/// configuration lost: its copy, empty, counts, as its store took the cell as the configuration's
/// writer, and one member more answering is enough for f = 1. It needed two before, the store's
/// log holding nothing to say it was the writer's. A store made anew on its host is not its copy:
/// refused.
#[test]
fn retakes_on_the_writers_disk_count_its_empty_log() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c", "d", "e"]);
    let lost = Lost::default();
    let peers = |me: &str| Peers { me: me.into(), all: all.clone(), lost: lost.clone() };
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir.clone()).unwrap();
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers("a")).unwrap();
    store.made().unwrap();
    let last = commit(&store, 0, 4);
    uploader.wait_replicated(last).unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    assert_eq!(addresses(&configuration_named(&host)), ["a", "b", "c"]);
    let expected = writer.contents(&store);
    // a's daemon restarts, every commit uploaded, its disk kept; b's disk lost meanwhile.
    let replicator = store.replicator().unwrap();
    lose(writer, (uploader, store, replicator));
    lost.lock().unwrap().insert("b".into());
    let recovering = LocalCell::recover(dir.crash(1)).unwrap();
    assert!(!recovering.has_wal(), "its log holds what the bucket does not");

    // A store made anew on a: not a's copy, and c alone answers.
    let anew = MemDir::new();
    LocalCell::make(&anew, DESCRIPTION, Some(URL), None).unwrap();
    let refused = LocalCell::recover_unmade(anew).unwrap();
    let failure = host.open_replicated(refused, 2, None, peers("a")).err().unwrap();
    assert!(
        failure.to_string().contains("1 of the 2 members of configuration 1 a take must fence"),
        "{failure}"
    );

    // a's own store: its copy, and c's, enough.
    let again = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let (retaken, uploader) = again.open_replicated(recovering, 3, None, peers("a")).unwrap();
    assert!(retaken.last_commit().unwrap() >= last);
    assert_eq!(again.contents(&retaken), expected);
    let next = commit(&retaken, 4, 2);
    uploader.wait_replicated(next).unwrap();
    drop(uploader);
}

/// TASK-683's review: a writer's store whose write-ahead log is dropped, as a take recovering from
/// members ahead of it drops it before restoring theirs, is its writer's copy no more, from before
/// the log goes, whatever a crash keeps of what follows: else a take after a crash, or a failure,
/// in the restore, counted its log, empty, as the copy that held commits told replicated.
#[test]
fn dropped_logs_are_no_writers_copy() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c"]);
    let peers = Peers { me: "a".into(), all, lost: Lost::default() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir.clone()).unwrap();
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers).unwrap();
    store.made().unwrap();
    let last = commit(&store, 0, 4);
    uploader.wait_replicated(last).unwrap();
    let configuration = configuration_named(&host);
    let replicator = store.replicator().unwrap();
    lose(writer, (uploader, store, replicator));
    let restarted = dir.crash(1);
    let mut recovering = LocalCell::recover(restarted.clone()).unwrap();
    assert!(recovering.has_wal(), "its log holds commits the bucket does not");
    assert!(recovering.holds_own_copy(configuration.epoch));
    recovering.drop_wal().unwrap();
    assert!(!recovering.holds_own_copy(configuration.epoch));
    drop(recovering);
    for seed in 0..8 {
        let recovering = LocalCell::recover(restarted.crash(seed)).unwrap();
        assert!(!recovering.holds_own_copy(configuration.epoch), "crash {seed}");
    }
}

/// A store in memory taking `cell` on `host` as a replicated cell's, its bucket's writes failing
/// as `faults` say.
fn faulty_host(faults: &Arc<bucket_cell::faults::Faults>) -> Host {
    let objects = Arc::new(InMemory::new());
    let faulty = bucket_cell::faults::Faulty::new(objects, faults.clone());
    let bucket = Arc::new(ObjectBucket::new(Arc::new(faulty), ""));
    let runtime = runtime_of_its_own();
    runtime.block_on(bucket_cell::lifecycle::create(&*bucket, cell(), 1)).unwrap();
    Host { runtime, bucket }
}

/// The ends of the logs `replicas` holds of `cell()`'s `configuration`, by reading its fences at
/// epoch 0, which no take fenced yet: no promise raised.
fn ends(
    replicas: &local_cell::replica::Replicas<MemDir>,
    configuration: &bucket_cell::layout::Configuration,
) -> u64 {
    let of = (cell(), key(configuration), replicas.incarnation());
    replicas.fence(of, 0).unwrap().iter().map(|&(_, end)| end).sum()
}

/// Whether the bucket's writes fail.
fn writes_fail(faults: &bucket_cell::faults::Faults, fail: bool) {
    use bucket_cell::faults::{Failing, Fault};
    let failing = if fail { Failing::NoSpace } else { Failing::Not };
    faults.set(Fault { latency: Duration::ZERO, failing, ..Fault::default() });
}

/// TASK-678's verifying review: the old writer, on a, sends its last two commits to its member b
/// and crashes before its own sync, its log ending before them. A retake on a, b out of reach,
/// defers its configuration's activation, commits one of its own at the place of the first, and
/// crashes with the bucket's writes failing. A retake on a again, c out of reach now, fences b
/// alone: b's logs are the old writer's, past what this store's own log went on from. Given to the
/// new configuration's members, a later take finds an old writer's segment naming a commit past
/// the head's last, and refuses the cell for good, having lost what was told replicated since;
/// given up, every commit told replicated is there after the next take, elsewhere.
#[test]
fn retakes_give_their_configurations_no_earlier_writers_logs() {
    // How many of the writer's file system's operations its flush makes before its log's sync:
    // a flush failing there has its records spooled, sent, and not synced.
    for budget in 0..16 {
        if earlier_writers_logs_given_up(budget) {
            return;
        }
    }
    panic!("no flush failed between sending its records and syncing them");
}

/// [`retakes_give_their_configurations_no_earlier_writers_logs`], its old writer's flush failing
/// after `budget` operations: whether that flush sent its records unsynced, which the rest needs.
fn earlier_writers_logs_given_up(budget: u64) -> bool {
    let faults = bucket_cell::faults::Faults::new(Duration::ZERO, None);
    let host = faulty_host(&faults);
    let all = directories(&["a", "b", "c", "d", "e"]);
    let lost = Lost::default();
    let peers = |me: &str| Peers { me: me.into(), all: all.clone(), lost: lost.clone() };
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir.clone()).unwrap();
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers("a")).unwrap();
    store.made().unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    let told = commit(&store, 0, 2);
    uploader.wait_replicated(told).unwrap();
    let first = configuration_named(&host);
    assert_eq!(addresses(&first), ["a", "b", "c"]);

    // c out of reach: commits on a's log and b's, past c's; then two flushed, sent to b, the log's
    // sync failing.
    lost.lock().unwrap().insert("c".into());
    let synced = commit(&store, 2, 2);
    uploader.wait_replicated(synced).unwrap();
    let before = ends(&all["b"], &first);
    for (sequence, round) in (store.info().unwrap().next_sequence..).zip(4..6) {
        let extent = ExtentId { cell: cell().id, sequence };
        store.write(extent, 0, &common::noise(round, BLOCK)).unwrap();
        store.commit_ordered(Some(format!("root {round}").as_bytes())).unwrap();
    }
    let unsynced = store.last_commit().unwrap();
    dir.fail_after(Some(budget));
    let flushed = store.flush();
    dir.fail_after(None);
    let started = Instant::now();
    while flushed.is_err() && ends(&all["b"], &first) == before {
        if started.elapsed() > Duration::from_secs(2) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let sent = flushed.is_err() && ends(&all["b"], &first) > before;
    let replicator = store.replicator().unwrap();
    lose(writer, (uploader, store, replicator));
    if !sent {
        return false;
    }
    // a's log as a crash leaves it: without them.
    let crashed = (0..64)
        .map(|seed| dir.crash(seed))
        .find(|crashed| {
            let held = LocalCell::recover(crashed.clone()).unwrap().last_commit_held();
            held.map_or(0, extent_frame::segment::epoch_of) < unsynced - 1
        })
        .expect("a crash that keeps neither of them");

    // A retake on a, b out of reach, c back: deferred, the head naming a's first configuration.
    lost.lock().unwrap().remove("c");
    lost.lock().unwrap().insert("b".into());
    let again = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let recovering = LocalCell::recover(crashed.clone()).unwrap();
    let (retaken, uploader) = again.open_replicated(recovering, 2, None, peers("a")).unwrap();
    writes_fail(&faults, true);
    assert_eq!(configuration_named(&host).number, first.number, "deferred");
    let own = commit(&retaken, 10, 1);
    assert_eq!(own, unsynced - 1, "its own commit at the place of the first of them");
    let replicator = retaken.replicator().unwrap();
    lose(again, (uploader, retaken, replicator));

    // A retake on a again, b back, c out of reach: it fences b alone.
    writes_fail(&faults, false);
    lost.lock().unwrap().remove("b");
    lost.lock().unwrap().insert("c".into());
    let third = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let recovering = LocalCell::recover(crashed.crash(1)).unwrap();
    let (taken, uploader) = third.open_replicated(recovering, 3, None, peers("a")).unwrap();
    let base = taken.last_commit().unwrap();
    assert_eq!(base, own, "its own log's, not b's");
    uploader.wait_uploaded(base).unwrap();
    until("its configuration named", || configuration_named(&host).epoch == 3);
    // A commit told replicated with the bucket's writes failing: on its configuration's members.
    writes_fail(&faults, true);
    let last = commit(&taken, 20, 1);
    uploader.wait_replicated(last).unwrap();
    assert!(uploader.last_uploaded() < last, "told replicated by its members, not the bucket");
    let expected = third.contents(&taken);
    let replicator = taken.replicator().unwrap();
    lose(third, (uploader, taken, replicator));

    // a lost: d takes the cell from that configuration's members and the bucket.
    writes_fail(&faults, false);
    lost.lock().unwrap().remove("c");
    lost.lock().unwrap().insert("a".into());
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let (taken, uploader) = host.open_replicated(recovering, 4, None, peers("d")).unwrap();
    taken.made().unwrap();
    assert!(taken.last_commit().unwrap() >= last);
    assert_eq!(host.contents(&taken), expected);
    drop(uploader);
    true
}

/// A host's peers as [`Peers`] reaches them, but `garbled`, whose answers say its logs of
/// segments' records are `PAD` bytes longer than they are, padded with what no record frames.
struct Garbling {
    peers: Peers,
    garbled: String,
}

/// What [`Garbling`] pads a member's logs with.
const PAD: usize = 64 << 10;

impl local_cell::upload::Members for Garbling {
    fn me(&self) -> Result<bucket_cell::layout::Member, String> {
        self.peers.me()
    }

    fn pick(
        &self,
        cell: bucket_cell::layout::CellId,
        number: u64,
        f: u8,
        avoid: &[String],
    ) -> Result<Vec<bucket_cell::layout::Member>, String> {
        self.peers.pick(cell, number, f, avoid)
    }

    fn link(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Arc<dyn MemberLink> {
        self.peers.link(cell, configuration, member)
    }

    fn fence(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
        epoch: u64,
    ) -> Result<Vec<(u64, u64)>, String> {
        let ends = self.peers.fence(cell, configuration, member, epoch)?;
        Ok(match member.address == self.garbled {
            true => ends
                .into_iter()
                .map(|(log, end)| (log, end + if log & 1 == 0 { PAD as u64 } else { 0 }))
                .collect(),
            false => ends,
        })
    }

    fn read(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
        log: u64,
    ) -> Result<Vec<u8>, String> {
        let mut bytes = self.peers.read(cell, configuration, member, log)?;
        if member.address == self.garbled && log & 1 == 0 {
            bytes.extend(std::iter::repeat_n(0xa5, PAD));
        }
        Ok(bytes)
    }

    fn drop_before(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Result<(), String> {
        self.peers.drop_before(cell, configuration, member)
    }
}

/// TASK-684: a take finding a member whose copy of a segment's log is the longest by its bytes,
/// garbled past a prefix shorter than another member's honest copy, takes the honest one: every
/// commit told replicated, held by that member alone, is there.
#[test]
fn takes_prefer_whole_authentic_copies_to_longer_garbled_ones() {
    let host = replicated_host();
    let all = directories(&["a", "b", "c", "d", "e"]);
    let lost = Lost::default();
    let peers = |me: &str| Peers { me: me.into(), all: all.clone(), lost: lost.clone() };
    let writer = Host { runtime: runtime_of_its_own(), bucket: host.bucket.clone() };
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let wish = bucket_cell::layout::Wish::new(1, bucket_cell::layout::Fsync::Replicated);
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (store, uploader) = writer.open_replicated(recovering, 1, wish, peers("a")).unwrap();
    store.made().unwrap();
    store.close_segment().unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    assert_eq!(addresses(&configuration_named(&host)), ["a", "b", "c"]);
    let both = commit(&store, 0, 2);
    uploader.wait_replicated(both).unwrap();
    // c out of reach: b alone holds what comes next.
    lost.lock().unwrap().insert("c".into());
    let last = commit(&store, 2, 3);
    uploader.wait_replicated(last).unwrap();
    assert!(uploader.last_uploaded() < last, "told replicated by b, not the bucket");
    let expected = writer.contents(&store);
    let replicator = store.replicator().unwrap();
    lose(writer, (uploader, store, replicator));

    // a lost, c back, answering its logs as longer than b's: d takes the cell.
    lost.lock().unwrap().remove("c");
    lost.lock().unwrap().insert("a".into());
    let garbling = Garbling { peers: peers("d"), garbled: "c".into() };
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let runtime = Runtime {
        handle: host.runtime.handle().clone(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let options = upload::Options {
        interval: Duration::from_secs(3600),
        members: Some(upload::MembersOf(Arc::new(garbling))),
        replace_after: Duration::from_secs(1),
        ..upload::Options::default()
    };
    let opened = upload::open(recovering, OPTIONS, host.bucket.clone(), (2, options), &runtime);
    let (taken, uploader) = opened.unwrap();
    taken.made().unwrap();
    assert!(taken.last_commit().unwrap() >= last);
    assert_eq!(host.contents(&taken), expected);
    drop(uploader);
}
