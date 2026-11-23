// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hosts sharing a bucket in this process, each reaching the others by a link that calls their
//! serving fetchers directly: what asking peers saves, and what it does when they fail. The
//! daemon's gRPC links are `cladefsd::peers`'.

use super::*;
use bucket_cell::bucket::{Bucket as _, PutMode};
use bucket_cell::cache::Cache;
use bucket_cell::fetch::{Budget, Fetcher, Limits, Memory, Priority};
use bucket_cell::layout::{self, Contents, Location, test_cell};
use bucket_cell::object_bucket::{ObjectBucket, Request, RoutedBucket};
use bucket_cell::peer::Peers as _;
use std::sync::atomic::AtomicBool;
use storage_device::ExtentId;

const PARTS: u64 = 8;
const PART: u64 = layout::PART;
const EXTENT: ExtentId = ExtentId::new(1, 1);

/// Bytes that don't compress, so that each part is stored as it is.
pub(crate) fn noise(len: u64) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// A bucket holding a segment of `data`, all of which is [`EXTENT`]: where it is.
pub(crate) fn holding(data: &[u8]) -> (Arc<RoutedBucket>, Location) {
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let key = layout::segment(test_cell(1), 1, 1);
    let contents =
        Contents { extents: vec![(EXTENT, 0, data.len() as u64)], ..Contents::default() };
    let footer = layout::Chain::new(test_cell(1), 1, 0, 0).close(contents);
    // cladefs: encrypted under the tests' DEK, which the hosts' fetchers hold (TASK-411.28).
    let object = layout::Segment { data: data.to_vec(), footer }
        .encode(bucket_cell::testing::encrypting(test_cell(1)));
    futures::executor::block_on(bucket.put(&key, object.into(), PutMode::Create)).unwrap();
    (bucket, Location { key, offset: 0, len: data.len() as u64 })
}

/// A host of this process: the fetcher its open cells read by, asking its peers, and the one it
/// serves them by, asking none, which share its memory, budget and cache, as a daemon's do.
struct Host {
    reads: Fetcher<RoutedBucket>,
    peers: Arc<Peers>,
    _cache: tempfile::TempDir,
}

/// A link to a host of this process, by its serving fetcher: failing every request, if told to;
/// or answering that it has no such part, as a host whose read of the object found none would.
struct InProcess {
    serving: Arc<Fetcher<RoutedBucket>>,
    calls: AtomicU64,
    failing: AtomicBool,
    missing: AtomicBool,
    /// cladefs: refusing as of another region (TASK-633.4).
    foreign: AtomicBool,
}

impl Link for InProcess {
    fn get(&self, asked: Asked) -> BoxFuture<'static, Result<Option<Vec<u8>>, Refusal>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.failing.load(Ordering::Relaxed) {
            return futures::future::ready(Err(Refusal::Failed("unreachable".to_owned()))).boxed();
        }
        if self.foreign.load(Ordering::Relaxed) {
            let refused = Refusal::Foreign("another region".to_owned());
            return futures::future::ready(Err(refused)).boxed();
        }
        if self.missing.load(Ordering::Relaxed) {
            return futures::future::ready(Ok(None)).boxed();
        }
        let serving = self.serving.clone();
        async move {
            let Asked { key, part } = asked;
            let served = serving.stored_part(&key, part).await;
            let served = served.map(|bytes| bytes.map(|bytes| bytes.to_vec()));
            served.map_err(|err| Refusal::Failed(err.to_string()))
        }
        .boxed()
    }
}

/// Hosts at `addresses`, sharing `bucket`, each with a cache of its own, each asking the others
/// if `peered`; and the links to each, by address.
fn hosts(
    bucket: &Arc<RoutedBucket>,
    addresses: &[&str],
    peered: bool,
) -> (Vec<Host>, HashMap<String, Arc<InProcess>>) {
    let made: Vec<_> = addresses
        .iter()
        .map(|_| {
            let cache = tempfile::tempdir().unwrap();
            let kept = Arc::new(Cache::open(cache.path(), u64::MAX).unwrap());
            let (budget, memory) = (Budget::new(Limits::UNBOUNDED), Memory::new());
            // cladefs: holding the tests' keys, which the objects are encrypted under, for good.
            std::mem::forget(memory.keys().hold(bucket_cell::testing::ring([test_cell(1)])));
            let fetcher =
                || Fetcher::new(bucket.clone(), budget.clone(), memory.clone(), Some(kept.clone()));
            (fetcher(), Arc::new(fetcher()), cache)
        })
        .collect();
    let links: HashMap<String, Arc<InProcess>> = (addresses.iter().zip(&made))
        .map(|(address, (_, serving, _))| {
            let link = InProcess {
                serving: serving.clone(),
                calls: AtomicU64::new(0),
                failing: AtomicBool::new(false),
                missing: AtomicBool::new(false),
                foreign: AtomicBool::new(false),
            };
            ((*address).to_owned(), Arc::new(link))
        })
        .collect();
    let hosts = (addresses.iter().zip(made))
        .map(|(address, (reads, _, cache))| {
            let links = links.clone();
            let peers = Peers::new(*address, move |to| links[to].clone() as Arc<dyn Link>);
            if peered {
                peers.set(addresses);
            }
            Host { reads: reads.asking(peers.clone()), peers, _cache: cache }
        })
        .collect();
    (hosts, links)
}

/// Reads all of [`EXTENT`], at `location`, with `host`'s fetcher, a part at a time.
fn read_all(host: &Host, location: &Location) -> Vec<u8> {
    let mut read = Vec::new();
    for part in 0..location.len.div_ceil(PART) {
        let reading = host.reads.read(EXTENT, location, part * PART, PART, Priority::Demand);
        read.extend(futures::executor::block_on(reading).unwrap());
    }
    read
}

/// The GETs the bucket was sent, whole and of ranges.
pub(crate) fn gets(bucket: &RoutedBucket) -> u64 {
    let stats = bucket.stats();
    stats.requests(Request::Get) + stats.requests(Request::GetRange)
}

/// How many of the parts of the object at `key` the host at `address` owns among `addresses`.
fn owned(addresses: &[&str], address: &str, key: &str) -> u64 {
    let owner = |part| owner_of(addresses.iter().copied(), key, part);
    (0..PARTS).filter(|part| owner(*part) == Some(address)).count() as u64
}

/// Three hosts reading one object fetch each of its parts from the bucket once, its owner's GET,
/// not once each: 16 GETs fewer for its 8 parts. Each host GETs the object's tail itself.
#[test]
fn hosts_asking_their_peers_fetch_each_part_once() {
    let data = noise(PARTS * PART);
    let addresses = ["10.0.0.1:7070", "10.0.0.2:7070", "10.0.0.3:7070"];
    let mut got = Vec::new();
    for peered in [false, true] {
        let (bucket, location) = holding(&data);
        let (hosts, _) = hosts(&bucket, &addresses, peered);
        for host in &hosts {
            assert!(read_all(host, &location) == data, "{} read other data", host.peers.me());
        }
        got.push(gets(&bucket));
        let given: u64 = hosts.iter().map(|host| host.peers.counts().given).sum();
        assert_eq!(given, if peered { 2 * PARTS } else { 0 });
    }
    assert_eq!(got[1], got[0] - 2 * PARTS, "GETs without peers, and with: {got:?}");
}

/// A peer that fails is asked nothing for a while: the parts it owns come from the bucket
/// meanwhile, read whole; once its while is over, it is asked again.
#[test]
fn a_peer_that_fails_is_asked_nothing_for_a_while() {
    let data = noise(PARTS * PART);
    let (bucket, location) = holding(&data);
    let addresses = ["10.0.0.1:7070", "10.0.0.2:7070"];
    let (hosts, links) = hosts(&bucket, &addresses, true);
    assert!(owned(&addresses, addresses[1], &location.key) > 1, "the test needs two parts owned");
    let other = &links[addresses[1]];
    other.failing.store(true, Ordering::Relaxed);
    assert!(read_all(&hosts[0], &location) == data);
    assert_eq!(other.calls.load(Ordering::Relaxed), 1, "asked again while down");
    let counts = hosts[0].peers.counts();
    assert_eq!(counts, Counts { asked: 1, given: 0, missing: 0, failed: 1 });
    assert_eq!(gets(&bucket), 1 + PARTS, "its tail, and each part, from the bucket");

    let member = hosts[0].peers.others.read().unwrap()[0].clone();
    assert!(!member.health.up_at(Instant::now()));
    assert!(member.health.up_at(Instant::now() + DOWN));
}

/// TASK-633.4: a peer that says it is of another region is never asked again, and owns no part:
/// every part comes from this region, the bucket here, once the one asked refused. Peers never
/// serve across regions.
#[test]
fn a_peer_of_another_region_is_never_asked_again() {
    let data = noise(PARTS * PART);
    let (bucket, location) = holding(&data);
    let addresses = ["10.0.0.1:7070", "10.0.0.2:7070"];
    let (hosts, links) = hosts(&bucket, &addresses, true);
    assert!(owned(&addresses, addresses[1], &location.key) > 1, "the test needs two parts owned");
    let other = &links[addresses[1]];
    other.foreign.store(true, Ordering::Relaxed);
    assert!(read_all(&hosts[0], &location) == data);
    assert!(read_all(&hosts[0], &location) == data);
    assert_eq!(other.calls.load(Ordering::Relaxed), 1, "asked again after it refused");
    let member = hosts[0].peers.others.read().unwrap()[0].clone();
    assert!(member.health.is_foreign());
    assert!(member.health.up_at(Instant::now() + DOWN), "foreign is not down for a while");
    for part in 0..PARTS as u64 {
        assert!(hosts[0].peers.owner(&location.key, part).is_none(), "part {part} owned abroad");
    }
}

/// TASK-633.4: hosts of one region serve each other; of another region, or another system of
/// buckets, never; with no region yet, by their bucket's URL alone.
#[test]
fn peers_serve_their_region_alone() {
    let (east, west) = (Region { home: 2, root: 7 }, Region { home: 3, root: 7 });
    let other_system = Region { home: 2, root: 8 };
    assert_eq!(admits((Some(east), "s3://e"), (Some(east), "s3://e")), Ok(()));
    assert!(admits((Some(east), "s3://e"), (Some(west), "s3://e")).is_err());
    assert!(admits((Some(east), "s3://e"), (Some(other_system), "s3://e")).is_err());
    assert_eq!(admits((None, "file:///b"), (None, "file:///b")), Ok(()));
    // Its bucket never said (board 8437).
    let refused = admits((None, "file:///b"), (None, "file:///c"));
    assert_eq!(refused, Err("asked of a bucket not this host's".to_owned()));
    assert_eq!(admits((Some(east), "s3://e"), (None, "s3://e")), Ok(()));
}

/// A peer without the part is no failure: the part comes from the bucket, and the peer is asked
/// for the next.
#[test]
fn a_peer_without_the_part_stays_up() {
    let data = noise(PARTS * PART);
    let (bucket, location) = holding(&data);
    let addresses = ["10.0.0.1:7070", "10.0.0.2:7070"];
    let (hosts, links) = hosts(&bucket, &addresses, true);
    let owned = owned(&addresses, addresses[1], &location.key);
    assert!(owned > 1, "the test needs two parts owned");
    let other = &links[addresses[1]];
    other.missing.store(true, Ordering::Relaxed);
    assert!(read_all(&hosts[0], &location) == data);
    assert_eq!(other.calls.load(Ordering::Relaxed), owned);
    let counts = hosts[0].peers.counts();
    assert_eq!(counts, Counts { asked: owned, given: 0, missing: owned, failed: 0 });
}

/// Every part has one owner among the hosts, whatever their order; a host added takes about its
/// share, from the others, and no part moves among those that stay.
#[test]
fn ownership_moves_only_with_the_hosts_added() {
    let key = layout::segment(test_cell(1), 1, 1);
    let three = ["10.0.0.1:7070", "10.0.0.2:7070", "10.0.0.3:7070"];
    let four = ["10.0.0.4:7070", "10.0.0.3:7070", "10.0.0.2:7070", "10.0.0.1:7070"];
    let mut moved = 0;
    for part in 0..10_000 {
        let before = owner_of(three, &key, part).unwrap();
        let reversed = owner_of(three.iter().rev().copied(), &key, part).unwrap();
        assert_eq!(before, reversed, "part {part}: two owners by order");
        let after = owner_of(four, &key, part).unwrap();
        if after != before {
            assert_eq!(after, four[0], "part {part} moved between hosts that stayed");
            moved += 1;
        }
    }
    assert!((2_000..3_000).contains(&moved), "the host added took {moved} of 10000");
}

/// A host's list kept, set again, keeps its peers as they were, down or not; one dropped and added
/// again is new.
#[test]
fn peers_set_again_stay_as_they_were() {
    let addresses = ["10.0.0.1:7070", "10.0.0.2:7070"];
    let link: Arc<dyn Link> = Arc::new(InProcess {
        serving: Arc::new(Fetcher::new(
            holding(&[]).0,
            Budget::new(Limits::UNBOUNDED),
            Memory::new(),
            None,
        )),
        calls: AtomicU64::new(0),
        failing: AtomicBool::new(false),
        missing: AtomicBool::new(false),
        foreign: AtomicBool::new(false),
    });
    let peers = Peers::new(addresses[0], move |_| link.clone());
    peers.set(addresses);
    let member = || peers.others.read().unwrap()[0].clone();
    member().health.down("a test");
    peers.set(addresses.iter().rev());
    assert!(!member().health.up_at(Instant::now()), "up again, set again");
    assert_eq!(peers.addresses(), addresses);
    peers.set([addresses[0]]);
    assert_eq!(peers.addresses(), [addresses[0]]);
    peers.set(addresses);
    assert!(member().health.up_at(Instant::now()), "down, added again");
}

/// Only parts of framed objects are asked of peers: never a cell's record, a head, or a cell's
/// object, which change in place. And a host asks no peer for what it owns.
#[test]
fn hosts_ask_peers_only_for_framed_objects_they_dont_own() {
    let me = "10.0.0.1:7070";
    let peers = Peers::new(me, |_: &str| -> Arc<dyn Link> { panic!("no link made") });
    let segment = layout::segment(test_cell(1), 1, 1);
    assert!(peers.owner(&segment, 0).is_none(), "asked with no peers");
    peers.set([me]);
    assert!(peers.owner(&segment, 0).is_none(), "asked itself");
    assert_eq!(peers.addresses(), [me]);
    assert!(!bucket_cell::peer::served(&layout::head(test_cell(1))), "a head served");
    assert!(bucket_cell::peer::served(&segment));
}
