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

/// A host's peers, as its daemon's members are (`local_cell::upload::Members`), reaching the
/// replica directories in memory directly.
struct Peers {
    me: String,
    all: Directories,
}

fn key(configuration: &bucket_cell::layout::Configuration) -> local_cell::replica::Key {
    local_cell::replica::Key { number: configuration.number, epoch: configuration.epoch }
}

/// A writer's link to a member in memory.
struct Link {
    replicas: Arc<local_cell::replica::Replicas<MemDir>>,
    of: (bucket_cell::layout::CellId, local_cell::replica::Key, u64),
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
        self.replicas.append(self.of, log, offset, bytes).map_err(link_error)
    }

    fn drop_through(&self, through: u64) -> Result<(), LinkError> {
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
    ) -> Result<Vec<bucket_cell::layout::Member>, String> {
        let others = self.all.iter().filter(|(address, _)| **address != self.me);
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
        Arc::new(Link { replicas, of: (cell, key(configuration), member.incarnation) })
    }

    fn fence(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
        epoch: u64,
    ) -> Result<Vec<(u64, u64)>, String> {
        let replicas = self.all.get(&member.address).ok_or("lost")?;
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
        let replicas = self.all.get(&member.address).ok_or("lost")?;
        let of = (cell, key(configuration), member.incarnation);
        replicas.read(of, log, 0, u64::MAX).map_err(|err| err.to_string())
    }

    fn drop_before(
        &self,
        cell: bucket_cell::layout::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Result<(), String> {
        let replicas = self.all.get(&member.address).ok_or("lost")?;
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
    let peers = |me: &str, all: &Directories| Peers { me: me.into(), all: all.clone() };

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
    let a = Peers { me: "a".into(), all: all.clone() };
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
    let d = Peers { me: "d".into(), all: left };
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
