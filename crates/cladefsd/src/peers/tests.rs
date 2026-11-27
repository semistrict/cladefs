// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's peers over gRPC: parts served, refusals, and a peer out of reach. What
//! asking peers saves, and how it falls back, `crate::bucket::peers`' tests show, and two daemons
//! `tests/peers.rs`.

use super::*;

fn noise(len: u64) -> Vec<u8> {
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

fn holding(data: &[u8]) -> (Arc<RoutedBucket>, Location) {
    use bucket_cell::bucket::{Bucket as _, PutMode};
    use bucket_cell::object_bucket::ObjectBucket;
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let cell = layout::test_cell(1);
    let key = layout::segment(cell, 1, 1);
    let extent = storage_device::ExtentId::new(1, 1);
    let contents = layout::Contents {
        extents: vec![(extent, 0, data.len() as u64)],
        ..layout::Contents::default()
    };
    let footer = layout::Chain::new(cell, 1, 0, 0).close(contents);
    let object = layout::Segment { data: data.to_vec(), footer }
        .encode(bucket_cell::testing::encrypting(cell));
    futures::executor::block_on(bucket.put(&key, object.into(), PutMode::Create)).unwrap();
    (bucket, Location { key, offset: 0, len: data.len() as u64 })
}

fn gets(bucket: &RoutedBucket) -> u64 {
    use bucket_cell::object_bucket::Request;
    let stats = bucket.stats();
    stats.requests(Request::Get) + stats.requests(Request::GetRange)
}
use bucket_cell::cache::Cache;
use bucket_cell::layout::{self, Location};
use bucket_cell::object_bucket::RoutedBucket;
use std::time::Instant;

const URL: &str = "memory:///peers";

/// A daemon's peer server on the loopback, serving the parts of `bucket`, named [`URL`], kept in a
/// cache in `dir`: its address. Of no region, as a bucket of no number yet is.
fn serving(bucket: &Arc<RoutedBucket>, dir: &std::path::Path) -> String {
    serving_in(bucket, dir, None)
}

/// [`serving`], of `region` (TASK-633.4).
fn serving_in(bucket: &Arc<RoutedBucket>, dir: &std::path::Path, region: Option<Region>) -> String {
    let cache = Arc::new(Cache::open(dir, u64::MAX).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let serving = cladefs_core::bucket::serving_peers(bucket.clone(), cache).unwrap();
    let server = Server {
        daemon: Daemon::process().clone(),
        bucket: URL.to_owned(),
        region,
        serving,
        replicas: None,
        heads: bucket.clone(),
        appending: Arc::default(),
    };
    // On a runtime of its own, as the daemon serves its peers (`super::super::run::service`).
    let serving = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    std::thread::spawn(move || serving.block_on(serve(listener, server)));
    address
}

/// Asks the peer at `address`, as the host whose bucket is at `url`, of no region, for `asked`.
fn ask(address: &str, url: &str, asked: Asked) -> Result<Option<Vec<u8>>, Refusal> {
    ask_as(address, (url, None), asked)
}

/// [`ask`], as a host of `region` (TASK-633.4).
fn ask_as(
    address: &str,
    (url, region): (&str, Option<Region>),
    asked: Asked,
) -> Result<Option<Vec<u8>>, Refusal> {
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    runtime.block_on(link(Daemon::process(), address, (url, region)).get(asked))
}

/// Part `part` of the object at `location`'s key in `bucket`, asked for by its key and number, and
/// its stored bytes.
fn asked(bucket: &RoutedBucket, location: &Location, part: u64) -> (Asked, Vec<u8>) {
    let object = cladefs_core::bucket::bucket_runtime().unwrap().block_on(async {
        use bucket_cell::bucket::Bucket as _;
        bucket.get(&location.key).await.unwrap().bytes
    });
    let parts = extent_frame::Parts::of(&object, &extent_frame::NoKeys).unwrap();
    let stored = parts.part(part).unwrap().stored;
    let bytes = object[stored.range().start as usize..stored.range().end as usize].to_vec();
    (Asked { key: location.key.clone(), part }, bytes)
}

/// A part is served as stored, fetched from the bucket once, its object's tail and itself, and
/// then from the host's memory.
#[test]
fn peers_are_served_parts_over_grpc() {
    let (bucket, location) = holding(&noise(3 * layout::PART));
    let dir = tempfile::tempdir().unwrap();
    let address = serving(&bucket, dir.path());
    let (asked, bytes) = asked(&bucket, &location, 1);
    let before = gets(&bucket);
    assert_eq!(ask(&address, URL, asked.clone()), Ok(Some(bytes.clone())));
    assert_eq!(gets(&bucket) - before, 2, "its tail and the part");
    assert_eq!(ask(&address, URL, asked), Ok(Some(bytes)));
    assert_eq!(gets(&bucket) - before, 2, "fetched again");
    assert_eq!(SERVED.load(Ordering::Relaxed), 2);
}

/// A part the object holds not, past its end, isn't there to give: NOT_FOUND, no failure.
#[test]
fn parts_the_object_holds_not_are_not_found() {
    let (bucket, location) = holding(&noise(2 * layout::PART));
    let dir = tempfile::tempdir().unwrap();
    let address = serving(&bucket, dir.path());
    let past = Asked { key: location.key.clone(), part: 5 };
    assert_eq!(ask(&address, URL, past), Ok(None));
    assert_eq!(SERVED_MISSING.load(Ordering::Relaxed), 1);
}

/// Peers asking for what is not served, or of another bucket, are refused: failures, which put
/// the peer down for the asker.
#[test]
fn peers_asking_wrongly_are_refused() {
    let (bucket, location) = holding(&noise(layout::PART));
    let dir = tempfile::tempdir().unwrap();
    let address = serving(&bucket, dir.path());
    let (asked, _) = asked(&bucket, &location, 0);
    let refused = |url: &str, asked: Asked| ask(&address, url, asked).unwrap_err();

    let head = layout::head(layout::test_cell(1));
    let not_served = Asked { key: head.clone(), ..asked.clone() };
    let invalid = format!("InvalidArgument: {head}: no parts served");
    assert_eq!(refused(URL, not_served), Refusal::Failed(invalid));
    let other = "memory:///other";
    // Neither bucket's URL said (board 8437).
    let foreign = "asked of a bucket not this host's".to_owned();
    assert_eq!(refused(other, asked), Refusal::Foreign(foreign));
}

/// TASK-633.4: a host of one region serves hosts of its region, and refuses those of another, or of
/// another system of buckets, as foreign, whatever their buckets' URLs.
#[test]
fn peers_of_another_region_are_refused_as_foreign() {
    let (bucket, location) = holding(&noise(layout::PART));
    let dir = tempfile::tempdir().unwrap();
    let east = Region { home: 2, root: 7 };
    let address = serving_in(&bucket, dir.path(), Some(east));
    let (asked, bytes) = asked(&bucket, &location, 0);
    assert_eq!(ask_as(&address, (URL, Some(east)), asked.clone()), Ok(Some(bytes)));
    let west = Region { home: 3, root: 7 };
    let refused = ask_as(&address, (URL, Some(west)), asked.clone());
    assert!(matches!(refused, Err(Refusal::Foreign(_))), "{refused:?}");
    let elsewhere = Region { home: 2, root: 8 };
    let refused = ask_as(&address, (URL, Some(elsewhere)), asked);
    assert!(matches!(refused, Err(Refusal::Foreign(_))), "{refused:?}");
}

/// A `GetPart` whose home is past a u16, what no region's home can be, is refused as meaningless
/// (board 14354), whatever its root and bucket's URL: never taken for no region's, which the URL
/// alone would admit, as it was for homes of this region's root that wrap to this region's home.
/// Homes at a u16's edge are a region's like any other.
#[test]
fn homes_past_a_u16_are_refused() {
    let (bucket, location) = holding(&noise(layout::PART));
    let dir = tempfile::tempdir().unwrap();
    let east = Region { home: 2, root: 7 };
    let address = serving_in(&bucket, dir.path(), Some(east));
    let (asked, _) = asked(&bucket, &location, 0);
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let answer = |home: u32, root: u64| {
        let channel = channel_on_with_prune(Daemon::process(), &address, false).unwrap();
        let mut client = client(&channel);
        let Asked { key, part } = asked.clone();
        let request = GetPartRequest { bucket: URL.to_owned(), key, part, home, root };
        runtime.block_on(client.get_part(request)).map(drop).map_err(|status| status.code())
    };
    let past = u32::from(u16::MAX) + 1;
    let answers = [
        answer(2, 7),
        answer(u32::from(u16::MAX), 7),
        answer(past, 7),
        answer(past + 2, 7),
        answer(u32::MAX, 7),
        answer(past, 0),
    ];
    assert_eq!(
        answers,
        [
            Ok(()),
            Err(Code::FailedPrecondition),
            Err(Code::InvalidArgument),
            Err(Code::InvalidArgument),
            Err(Code::InvalidArgument),
            Err(Code::InvalidArgument),
        ]
    );
}

/// A peer out of reach fails within the time to connect, not the request's deadline.
#[test]
fn a_peer_out_of_reach_fails_fast() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(listener);
    let (bucket, location) = holding(&noise(layout::PART));
    let (asked, _) = asked(&bucket, &location, 0);
    let began = Instant::now();
    let Err(Refusal::Failed(failed)) = ask(&address, URL, asked) else { panic!("not failed") };
    assert!(failed.starts_with("Unavailable: "), "{failed}");
    assert!(began.elapsed() < CONNECT * 2, "took {:?}", began.elapsed());
}

/// Peers are named `HOST:PORT`, nothing more.
#[test]
fn peers_are_named_host_and_port() {
    for address in ["10.0.0.1:7070", "host-a.internal:7070", "[::1]:7070"] {
        assert_eq!(check_address(address), Ok(()), "{address}");
    }
    for address in ["10.0.0.1", "http://10.0.0.1:7070", "10.0.0.1:7070/x", "", ":7070"] {
        assert!(check_address(address).is_err(), "{address:?} taken");
    }
}

/// cladefs: `work`, failing the test if it takes more than 30 s.
async fn within(work: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(30), work).await.expect("a member's call hung");
}

/// cladefs: a daemon's peer server on the loopback, of the bucket [`URL`], with a replica directory
/// in `dir` (doc-21), or none: its address, and the directory.
fn member(dir: &std::path::Path, with: bool) -> (String, Option<Arc<Replicas<HostDir>>>) {
    let served = counted_member(dir, with);
    (served.address, served.replicas)
}

/// cladefs: a member served as [`member`] serves it, and its counts.
struct Served {
    address: String,
    replicas: Option<Arc<Replicas<HostDir>>>,
    /// The bucket its calls' cells' heads are read from (board 8445).
    bucket: Arc<RoutedBucket>,
    /// The writers' streams of appends it serves.
    appending: Arc<AtomicUsize>,
    /// The connections it accepted.
    accepted: Arc<AtomicUsize>,
}

/// cladefs: [`member`], counting what it serves (board 8575).
fn counted_member(dir: &std::path::Path, with: bool) -> Served {
    counted_member_in(dir, with, holding(&noise(1)).0)
}

/// cladefs: [`counted_member`] with a replica directory, its cells' heads read from `heads`.
fn counted_member_of(dir: &std::path::Path, heads: Arc<RoutedBucket>) -> Served {
    counted_member_in(dir, true, heads)
}

/// cladefs: [`counted_member`], its cells' heads read from `bucket`.
fn counted_member_in(dir: &std::path::Path, with: bool, bucket: Arc<RoutedBucket>) -> Served {
    use futures::StreamExt as _;
    let heads = bucket.clone();
    let cache = Arc::new(Cache::open(&dir.join("cache"), u64::MAX).unwrap());
    let replicas = with.then(|| {
        super::replicas(cladefs_core::Host::process().local(), &[dir.join("replicas")]).unwrap()
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let serving = cladefs_core::bucket::serving_peers(bucket, cache).unwrap();
    let appending = Arc::<AtomicUsize>::default();
    let server = Server {
        daemon: Daemon::process().clone(),
        bucket: URL.to_owned(),
        region: None,
        serving,
        replicas: replicas.clone(),
        heads: heads.clone(),
        appending: appending.clone(),
    };
    let accepted = Arc::<AtomicUsize>::default();
    let accepting = accepted.clone();
    // On a runtime of its own, as the daemon serves its peers (`super::super::run::service`), as
    // `serve` serves them, each connection it accepts counted.
    let serving = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    std::thread::spawn(move || {
        serving.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let incoming = tonic::transport::server::TcpIncoming::from(listener)
                .with_nodelay(Some(true))
                .inspect(move |_| {
                    accepting.fetch_add(1, Ordering::Relaxed);
                });
            serve_on(admitting(incoming, Arc::default()), server).await
        })
    });
    Served { address, replicas, bucket: heads, appending, accepted }
}

/// cladefs: has `bucket` hold a live head of `cell`, replicated, taken until its epoch is `epoch`,
/// naming segments to `end` and `active` active, as a member admits its calls by (board 8445).
async fn heading(
    bucket: &impl bucket_cell::bucket::Bucket,
    cell: cladefs_core::CellId,
    epoch: u64,
    end: u64,
    active: Option<Key>,
) {
    use bucket_cell::layout::{Configuration, Fsync, Replication, Wish};
    if bucket_cell::head::get(bucket, cell).await.unwrap().is_none() {
        bucket_cell::lifecycle::create(bucket, cell, 1).await.unwrap();
    }
    let wish = Wish::new(1, Fsync::Replicated);
    let taking = bucket_cell::head::Taking { holder: 1, replication: wish, ..Default::default() };
    loop {
        let read = bucket_cell::head::read(bucket, cell).await.unwrap();
        let held = bucket_cell::head::take(bucket, cell, read, &taking).await.unwrap();
        assert!(held.head.epoch <= epoch, "{cell:?}'s head of epoch {} already", held.head.epoch);
        if held.head.epoch < epoch {
            continue;
        }
        let mut next = held.head.clone().holding(epoch, end);
        next.replication = Some(Replication {
            wish: wish.unwrap(),
            configuration: active.map(|Key { number, epoch }| Configuration {
                number,
                epoch,
                f: 1,
                members: Vec::new(),
            }),
        });
        bucket_cell::head::swap(bucket, cell, &held, next).await.unwrap();
        return;
    }
}

fn of(incarnation: u64, number: u64, epoch: u64) -> Option<ReplicaOf> {
    Some(ReplicaOf {
        bucket: URL.to_owned(),
        tenant: 3,
        cell: 9,
        configuration: Some(proto::Configuration { number, epoch }),
        incarnation,
        #[allow(deprecated)]
        formats: String::new(),
        versions: Some(current_formats()),
    })
}

/// cladefs: what asks a member for its incarnation names: [`URL`], and this host's formats.
fn asking() -> IncarnationRequest {
    IncarnationRequest {
        bucket: URL.to_owned(),
        #[allow(deprecated)]
        formats: String::new(),
        versions: Some(current_formats()),
    }
}

/// Doc-28 (board 10054, hotbed review 89 finding 325): rolling-upgrade format compatibility checks
/// each call in the direction data flows:
/// - `Write` (`Incarnation`, `Append`) and `MigratePrepare`: receiver reads sender's write versions.
/// - `Read` (`Fence`, `ReadLog`): taker's readable range `[oldest..=current]` covers every version
///   the member may hold (`member.oldest..=member.current`).
/// - `Drop` (`DropLogs`): accepted when either the member reads the writer's versions (writer
///   dropping uploaded logs on a newer member) or the taker covers the member's versions (newer
///   taker dropping superseded logs on an older member).
#[test]
fn formats_within_supported_range_are_accepted_and_outside_refused() {
    let build_a_member = SupportedFormats {
        cell_directory: formats::Format { name: "cell directory", oldest: 18, current: 18 },
        cell_records: formats::Format { name: "cell records", oldest: 1, current: 1 },
        replica_directory: formats::Format { name: "replica directory", oldest: 4, current: 4 },
        extent_frame: formats::Format { name: "extent frame", oldest: 3, current: 3 },
        bucket_layout: formats::Format { name: "bucket layout", oldest: 31, current: 31 },
    };
    let build_b_member = SupportedFormats {
        cell_directory: formats::Format { name: "cell directory", oldest: 18, current: 19 },
        cell_records: formats::Format { name: "cell records", oldest: 1, current: 2 },
        replica_directory: formats::Format { name: "replica directory", oldest: 4, current: 5 },
        extent_frame: formats::Format { name: "extent frame", oldest: 3, current: 4 },
        bucket_layout: formats::Format { name: "bucket layout", oldest: 31, current: 32 },
    };
    let build_a = proto::Formats {
        cell_directory: 18,
        cell_records: 1,
        replica_directory: 4,
        extent_frame: 3,
        bucket_layout: 31,
        cell_directory_oldest: 18,
        cell_records_oldest: 1,
        replica_directory_oldest: 4,
        extent_frame_oldest: 3,
        bucket_layout_oldest: 31,
    };
    let build_b = proto::Formats {
        cell_directory: 19,
        cell_records: 2,
        replica_directory: 5,
        extent_frame: 4,
        bucket_layout: 32,
        cell_directory_oldest: 18,
        cell_records_oldest: 1,
        replica_directory_oldest: 4,
        extent_frame_oldest: 3,
        bucket_layout_oldest: 31,
    };

    // Build A writer -> Build B member: accepted for Write, Drop, and Migration.
    build_b_member.check_replication(Some(build_a), ReplicationCall::Write).unwrap();
    build_b_member.check_replication(Some(build_a), ReplicationCall::Drop).unwrap();
    build_b_member.check_migration(0x7, Some(build_a)).unwrap();

    // Build B writer -> Build A member: refused for Write and Migration.
    assert_eq!(
        build_a_member.check_replication(Some(build_b), ReplicationCall::Write).unwrap_err().code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        build_a_member.check_migration(0x7, Some(build_b)).unwrap_err().code(),
        Code::FailedPrecondition
    );

    // Build B taker -> Build A member (rolling-upgrade failover): accepted for Read and Drop!
    build_a_member.check_replication(Some(build_b), ReplicationCall::Read).unwrap();
    build_a_member.check_replication(Some(build_b), ReplicationCall::Drop).unwrap();

    // Build A taker -> Build B member (older taker must not fence/read newer member's logs):
    // refused for Read!
    assert_eq!(
        build_b_member.check_replication(Some(build_a), ReplicationCall::Read).unwrap_err().code(),
        Code::FailedPrecondition
    );

    // Same-build B <-> B: accepted for all calls.
    build_b_member.check_replication(Some(build_b), ReplicationCall::Write).unwrap();
    build_b_member.check_replication(Some(build_b), ReplicationCall::Read).unwrap();
    build_b_member.check_replication(Some(build_b), ReplicationCall::Drop).unwrap();
    build_b_member.check_migration(0x7, Some(build_b)).unwrap();

    // Families a call does not depend on may be any version.
    build_b_member
        .check_replication(
            Some(proto::Formats { cell_records: 99, ..build_a }),
            ReplicationCall::Write,
        )
        .unwrap();
    build_b_member
        .check_migration(0x7, Some(proto::Formats { replica_directory: 99, ..build_a }))
        .unwrap();

    // Disjoint / out-of-range versions are refused for every call.
    for bad in [
        proto::Formats { cell_directory: 17, cell_directory_oldest: 17, ..build_a },
        proto::Formats { cell_directory: 20, cell_directory_oldest: 20, ..build_b },
        proto::Formats { replica_directory: 3, replica_directory_oldest: 3, ..build_a },
        proto::Formats { replica_directory: 6, replica_directory_oldest: 6, ..build_b },
        proto::Formats { extent_frame: 2, extent_frame_oldest: 2, ..build_a },
        proto::Formats { extent_frame: 5, extent_frame_oldest: 5, ..build_b },
        proto::Formats { bucket_layout: 30, bucket_layout_oldest: 30, ..build_a },
        proto::Formats { bucket_layout: 33, bucket_layout_oldest: 33, ..build_b },
    ] {
        for call in [ReplicationCall::Write, ReplicationCall::Read, ReplicationCall::Drop] {
            assert_eq!(
                build_b_member.check_replication(Some(bad), call).unwrap_err().code(),
                Code::FailedPrecondition,
                "{call:?} {bad:?}"
            );
        }
    }
    // Invalid taker range (oldest == 0, oldest > current, or narrower than member) refused on Read.
    for bad_range in [
        proto::Formats { cell_directory_oldest: 0, ..build_b },
        proto::Formats { cell_directory_oldest: 20, cell_directory: 19, ..build_b },
        proto::Formats { extent_frame_oldest: 4, extent_frame: 4, ..build_b },
    ] {
        assert_eq!(
            build_b_member
                .check_replication(Some(bad_range), ReplicationCall::Read)
                .unwrap_err()
                .code(),
            Code::FailedPrecondition,
            "{bad_range:?}"
        );
    }
    for bad in [
        proto::Formats { cell_directory: 17, ..build_a },
        proto::Formats { cell_directory: 20, ..build_b },
        proto::Formats { cell_records: 0, ..build_a },
        proto::Formats { cell_records: 3, ..build_b },
        proto::Formats { extent_frame: 2, ..build_a },
        proto::Formats { extent_frame: 5, ..build_b },
        proto::Formats { bucket_layout: 33, ..build_b },
    ] {
        assert_eq!(
            build_b_member.check_migration(0x7, Some(bad)).unwrap_err().code(),
            Code::FailedPrecondition
        );
    }
}

/// cladefs: a writer streams appends to a member, each answered once synced; a take fences the
/// cell, learns the logs' ends and reads them back; the writer's appends after are refused,
/// ending its stream; and the logs the bucket holds are dropped (doc-21, TASK-537.3.2).
#[test]
fn members_take_appends_and_fences_over_grpc() {
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let (address, bucket, replicas) = (served.address, served.bucket, served.replicas.unwrap());
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let mut client = super::member(&runtime, &address).unwrap();
    let cell = cladefs_core::CellId { tenant: 3, id: 9 };
    // Bounded: a member and a writer waiting on each other fail here, not the run (a member
    // answering appends only once the first came deadlocked so, TASK-537.3.2).
    runtime.block_on(within(async move {
        // Its writer of epoch 4's configuration 1 active, taken at epoch 5 (board 8445).
        heading(&*bucket, cell, 5, 0, Some(Key { number: 1, epoch: 4 })).await;
        let incarnation = (client.incarnation(asking()).await).unwrap().into_inner().incarnation;
        let append = |log, offset, bytes: &[u8]| AppendRequest {
            of: of(incarnation, 1, 4),
            log,
            offset,
            bytes: bytes.to_vec(),
        };
        let (sending, sent) = tokio::sync::mpsc::channel(8);
        let mut answers = (client.append(tokio_stream::wrappers::ReceiverStream::new(sent)).await)
            .unwrap()
            .into_inner();
        sending.send(append(1, 0, b"first ")).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 1, end: 6 }));
        sending.send(append(1, 6, b"second")).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 1, end: 12 }));
        // An append past the end writes nothing and answers the end (not OUT_OF_RANGE, which a
        // message past its size limit is answered with); one of a writer's piece, 4 MiB, tonic's
        // default limit, is taken (TASK-537.3.2's review).
        sending.send(append(3, 4, b"past")).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 3, end: 0 }));
        let piece = local_cell::replicate::PIECE;
        sending.send(append(3, 0, &vec![5; piece])).await.unwrap();
        assert_eq!(
            answers.message().await.unwrap(),
            Some(AppendResponse { log: 3, end: piece as u64 })
        );

        // A take of epoch 5 fences the cell, and reads what the old writer's configuration holds.
        let fence = FenceRequest { of: of(incarnation, 1, 4), epoch: 5 };
        let fenced = client.fence(fence).await.unwrap().into_inner();
        assert_eq!(fenced.logs, [LogEnd { log: 1, end: 12 }, LogEnd { log: 3, end: piece as u64 }]);
        let read = ReadLogRequest { of: of(incarnation, 1, 4), log: 1, offset: 0 };
        let mut pieces = client.read_log(read).await.unwrap().into_inner();
        let mut bytes = Vec::new();
        while let Some(piece) = pieces.message().await.unwrap() {
            bytes.extend(piece.bytes);
        }
        assert_eq!(bytes, b"first second");

        // The old writer's next append is refused, its stream ended.
        sending.send(append(1, 12, b"late")).await.unwrap();
        let late = answers.message().await.unwrap_err();
        assert_eq!(late.code(), Code::FailedPrecondition, "{late:?}");
        assert!(late.message().contains("fenced at epoch 5"), "{late:?}");

        // Its configuration 2 active since, the logs of those before it dropped: its promise
        // alone left.
        heading(&*bucket, cell, 6, 0, Some(Key { number: 2, epoch: 5 })).await;
        let drop = DropLogsRequest { of: of(incarnation, 2, 5), through: None };
        client.drop_logs(drop).await.unwrap();
    }));
    assert_eq!(replicas.cells().unwrap(), [(cell, vec![])]);
}

/// cladefs: a member refuses calls naming another incarnation (ABORTED), asked of another bucket,
/// or of a host with no replica directory (FAILED_PRECONDITION); and, board 8457, every call
/// naming other formats than its own, or none (an asker of a build before them), and a writer's
/// segment log of another format, before it acks any of it (FAILED_PRECONDITION, which a writer
/// takes for a refusal of the member, not a fencing).
#[test]
fn members_refuse_other_incarnations_buckets_and_hosts_without_replicas() {
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let (address, bucket) = (served.address, served.bucket);
    let theirs = served.replicas.unwrap().incarnation();
    let without = tempfile::tempdir().unwrap();
    let (none, _) = member(without.path(), false);
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let (mut client, mut other) =
        (super::member(&runtime, &address).unwrap(), super::member(&runtime, &none).unwrap());
    // Bounded: a member and a writer waiting on each other fail here, not the run (a member
    // answering appends only once the first came deadlocked so, TASK-537.3.2).
    runtime.block_on(within(async move {
        let cell = cladefs_core::CellId { tenant: 3, id: 9 };
        heading(&*bucket, cell, 5, 0, Some(Key { number: 1, epoch: 4 })).await;
        let fence = |of| FenceRequest { of, epoch: 5 };
        let wrong = client.fence(fence(of(theirs.wrapping_add(1), 1, 4))).await.unwrap_err();
        assert_eq!(wrong.code(), Code::Aborted, "{wrong:?}");
        let mut elsewhere = of(theirs, 1, 4);
        elsewhere.as_mut().unwrap().bucket = "memory:///other".to_owned();
        let elsewhere = client.fence(fence(elsewhere)).await.unwrap_err();
        assert_eq!(elsewhere.code(), Code::FailedPrecondition, "{elsewhere:?}");
        let no = other.incarnation(asking()).await.unwrap_err();
        assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
        assert!(no.message().contains("CLADEFSD_REPLICAS"), "{no:?}");

        let ours = current_formats();
        // Absent `versions` (as a pre-10054 build sending only the deprecated string sends) and
        // any replication family outside this host's range are refused before any append is
        // acknowledged (doc-28, board 10054; hotbed review 88 finding 322).
        let cases: Vec<(Option<proto::Formats>, &str)> = vec![
            (None, "the asker named no format versions (older build): refused"),
            (
                Some(proto::Formats { cell_directory: 15, cell_directory_oldest: 15, ..ours }),
                "cell directory",
            ),
            (
                Some(proto::Formats { replica_directory: 3, replica_directory_oldest: 3, ..ours }),
                "replica directory",
            ),
            (
                Some(proto::Formats { extent_frame: 2, extent_frame_oldest: 2, ..ours }),
                "extent frame",
            ),
            (
                Some(proto::Formats { bucket_layout: 1, bucket_layout_oldest: 1, ..ours }),
                "bucket layout",
            ),
        ];
        for (asked_versions, expected) in cases {
            let asked = IncarnationRequest {
                bucket: URL.to_owned(),
                #[allow(deprecated)]
                formats: "legacy string".to_owned(),
                versions: asked_versions,
            };
            let no = client.incarnation(asked).await.unwrap_err();
            assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
            assert!(no.message().contains(expected), "{expected:?} not in {:?}", no.message());
            let mut named = of(theirs, 1, 4);
            named.as_mut().unwrap().versions = asked_versions;
            let no = client.fence(fence(named.clone())).await.unwrap_err();
            assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
            assert!(no.message().contains(expected), "{expected:?} not in {:?}", no.message());
            let read = ReadLogRequest { of: named.clone(), log: 1, offset: 0 };
            let no = client.read_log(read).await.unwrap_err();
            assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
            assert!(no.message().contains(expected), "{expected:?} not in {:?}", no.message());
            let dropped = DropLogsRequest { of: named.clone(), through: None };
            let no = client.drop_logs(dropped).await.unwrap_err();
            assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
            assert!(no.message().contains(expected), "{expected:?} not in {:?}", no.message());
            let (sending, sent) = tokio::sync::mpsc::channel(1);
            let mut answers = (client.append(tokio_stream::wrappers::ReceiverStream::new(sent)))
                .await
                .unwrap()
                .into_inner();
            let first = AppendRequest {
                of: named,
                log: local_cell::replicate::own_of(1),
                offset: 0,
                bytes: local_cell::replicate::own_frame(1, b"one"),
            };
            sending.send(first).await.unwrap();
            let no = answers.message().await.unwrap_err();
            assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
            assert!(no.message().contains(expected), "{expected:?} not in {:?}", no.message());
            assert!(matches!(link_error(&no), local_cell::replicate::LinkError::Refused(_)));
        }
        // A different `cell_records` version does not affect replication calls (members hold logs,
        // not cell records).
        let other_records = IncarnationRequest {
            bucket: URL.to_owned(),
            #[allow(deprecated)]
            formats: String::new(),
            versions: Some(proto::Formats { cell_records: 99, ..ours }),
        };
        assert_eq!(
            client.incarnation(other_records).await.unwrap().into_inner().incarnation,
            theirs
        );

        // A segment's log of another format, its first bytes the build before's.
        let (sending, sent) = tokio::sync::mpsc::channel(1);
        let mut answers = (client.append(tokio_stream::wrappers::ReceiverStream::new(sent)))
            .await
            .unwrap()
            .into_inner();
        let older = b"FXLOG\0\0\x03 and the rest of a header".to_vec();
        let first = AppendRequest { of: of(theirs, 1, 4), log: 2, offset: 0, bytes: older };
        sending.send(first).await.unwrap();
        let no = answers.message().await.unwrap_err();
        assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
        assert!(no.message().ends_with("begins a log of this build's format: refused"), "{no:?}");
        assert!(matches!(link_error(&no), local_cell::replicate::LinkError::Refused(_)));
        let fenced = client.fence(fence(of(theirs, 1, 4))).await.unwrap().into_inner();
        assert_eq!(fenced.logs, []);

        // Rolling-upgrade failover over gRPC (hotbed review 89 finding 325): a newer taker whose
        // readable range covers this member's (`oldest == ours.oldest`, `current == ours.current + 1`)
        // is refused on `Incarnation` (this member cannot hold its newer logs) but accepted on
        // `Fence` and `DropLogs`.
        let newer_taker = proto::Formats {
            cell_directory: ours.cell_directory + 1,
            cell_directory_oldest: ours.cell_directory_oldest,
            ..ours
        };
        let no = client
            .incarnation(IncarnationRequest {
                bucket: URL.to_owned(),
                #[allow(deprecated)]
                formats: String::new(),
                versions: Some(newer_taker),
            })
            .await
            .unwrap_err();
        assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
        let mut taker_of = of(theirs, 1, 4);
        taker_of.as_mut().unwrap().versions = Some(newer_taker);
        client.fence(fence(taker_of.clone())).await.unwrap();
        client.drop_logs(DropLogsRequest { of: taker_of, through: None }).await.unwrap();
    }));
}

/// cladefs: a sweep drops the logs of configurations a cell's head names no more, and all of a
/// cell whose head is gone (doc-21, Sweeping stale replica logs).
#[test]
fn sweeps_drop_what_heads_name_no_more() {
    use bucket_cell::layout::{Configuration, Fsync, Member, Replication, Wish};
    let dir = tempfile::tempdir().unwrap();
    let replicas =
        super::replicas(cladefs_core::Host::process().local(), &[dir.path().to_path_buf()])
            .unwrap();
    let ours = replicas.incarnation();
    let (cell, gone) =
        (cladefs_core::CellId { tenant: 3, id: 9 }, cladefs_core::CellId { tenant: 3, id: 10 });
    let unconfigured = cladefs_core::CellId { tenant: 3, id: 11 };
    let plain = cladefs_core::CellId { tenant: 3, id: 12 };
    for (of, number) in [(cell, 1), (cell, 2), (gone, 1), (unconfigured, 1), (plain, 1)] {
        replicas.append((of, Key { number, epoch: number }, ours), 1, 0, b"x").unwrap();
    }
    let bucket = bucket_cell::testing::Direct(bucket_cell::testing::SimBucket::new());
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    runtime.block_on(async {
        bucket_cell::lifecycle::create(&bucket, cell, 1).await.unwrap();
        let taking = bucket_cell::head::Taking {
            holder: 1,
            replication: Wish::new(1, Fsync::Replicated),
            ..Default::default()
        };
        let held = bucket_cell::head::take(
            &bucket,
            cell,
            bucket_cell::head::read(&bucket, cell).await.unwrap(),
            &taking,
        )
        .await
        .unwrap();
        let mut next = held.head.clone();
        let members = vec![Member { address: "a:1".into(), incarnation: ours }];
        next.replication = Some(Replication {
            wish: Wish::new(1, Fsync::Replicated).unwrap(),
            configuration: Some(Configuration { number: 2, epoch: 2, f: 1, members }),
        });
        bucket_cell::head::swap(&bucket, cell, &held, next).await.unwrap();
        // A replicated cell whose head names no configuration yet: its first may be installed
        // now, and nothing of it goes (TASK-537.3.2's review).
        bucket_cell::lifecycle::create(&bucket, unconfigured, 1).await.unwrap();
        let read = bucket_cell::head::read(&bucket, unconfigured).await.unwrap();
        bucket_cell::head::take(&bucket, unconfigured, read, &taking).await.unwrap();
        // Nor of one whose head, read, is not replicated: a take may make it so, and install its
        // first configuration, before the sweep acts (Codex's review).
        bucket_cell::lifecycle::create(&bucket, plain, 1).await.unwrap();
        sweep(&replicas, &bucket).await.unwrap();
    });
    assert_eq!(
        replicas.cells().unwrap(),
        [
            (cell, vec![Key { number: 2, epoch: 2 }]),
            (unconfigured, vec![Key { number: 1, epoch: 1 }]),
            (plain, vec![Key { number: 1, epoch: 1 }]),
        ]
    );
}

/// cladefs: a writer's link to a member over gRPC (TASK-537.3.3): its appends land in the member's
/// replica directory, each answered with the log's end; once a take fences the cell, the writer's
/// next append is answered Fenced, which fences the writer; a link naming another incarnation is
/// refused for good.
#[test]
fn writers_append_to_members_through_their_links() {
    use local_cell::replicate::{LinkError, own_of as log_of};
    use local_cell::upload::Members as _;
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let (address, replicas) = (served.address, served.replicas.unwrap());
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let active = Some(Key { number: 1, epoch: 4 });
    let heads = served.bucket.clone();
    runtime.block_on(heading(&*heads, cladefs_core::CellId { tenant: 3, id: 9 }, 4, 1, active));
    let members = ReplicaMembers {
        bucket: URL.to_owned(),
        me: "127.0.0.1:1".to_owned(),
        incarnation: 1,
        daemon: Daemon::process().clone(),
    };
    let cell = cladefs_core::CellId { tenant: 3, id: 9 };
    let configuration =
        bucket_cell::layout::Configuration { number: 1, epoch: 4, f: 1, members: Vec::new() };
    let ours = bucket_cell::layout::Member {
        address: address.clone(),
        incarnation: replicas.incarnation(),
    };
    let link = members.link(cell, &configuration, &ours);
    assert_eq!(runtime.block_on(link.append(log_of(1), 0, b"abc".to_vec())), Ok(3));
    assert_eq!(runtime.block_on(link.append(log_of(1), 3, b"def".to_vec())), Ok(6));
    assert_eq!(
        runtime.block_on(link.append(log_of(1), 9, b"gap".to_vec())),
        Ok(6),
        "past the end: the end"
    );
    let of = (cell, Key { number: 1, epoch: 4 }, replicas.incarnation());
    assert_eq!(replicas.read(of, log_of(1), 0, 100).unwrap(), b"abcdef");
    assert_eq!(runtime.block_on(link.drop_through(0)), Ok(()));

    replicas.fence(of, 5).unwrap();
    let fenced = runtime.block_on(link.append(log_of(1), 6, b"late".to_vec()));
    assert!(matches!(fenced, Err(LinkError::Fenced(_))), "{fenced:?}");

    let wrong = bucket_cell::layout::Member { address, incarnation: ours.incarnation ^ 1 };
    let refused = members.link(cell, &configuration, &wrong);
    let refused = runtime.block_on(refused.append(log_of(1), 0, b"x".to_vec()));
    assert!(matches!(refused, Err(LinkError::Refused(_))), "{refused:?}");
}

/// board 5416: a takeover's read of a member's log goes to the end its fence said; a member that
/// stops answering, its keepalives answered as its disk stalls, fails it within the deadline, a
/// piece's wait (50 ms here, `DEADLINE` for members); a log ending short of the end reads short,
/// as its caller refuses. Board 8453: a member sending past the end it fenced the log at, streaming
/// for ever, fails the read at the first piece past it, so that what a read holds is never more
/// than its end, which its caller counts.
#[tokio::test]
async fn logs_read_from_members_end_at_their_fences_or_a_stall() {
    use futures::StreamExt as _;
    let piece = |byte: u8| Ok(ReadLogResponse { bytes: vec![byte; 10] });
    let pieces = |n: u8| futures::stream::iter((1..=n).map(piece));
    const WAIT: Duration = Duration::from_millis(50);

    // To its end, the stream dropped there, whatever more it would send.
    let endless = || pieces(2).chain(futures::stream::repeat_with(|| piece(9)));
    let read = read_to(Box::pin(endless()), 20, WAIT).await;
    assert_eq!(read, Ok([vec![1; 10], vec![2; 10]].concat()));
    // Past its end: refused, holding no more than the end.
    let read = read_to(Box::pin(endless()), 15, WAIT).await;
    assert_eq!(read, Err("sent 20 bytes of a log it fenced at 15".to_owned()));
    let read = read_to(Box::pin(endless()), 0, WAIT).await;
    assert_eq!(read, Ok(Vec::new()));

    // Stalled after a piece.
    let start = tokio::time::Instant::now();
    let stalled = pieces(1).chain(futures::stream::pending());
    let read = read_to(Box::pin(stalled), 15, WAIT).await;
    assert_eq!(read, Err("no answer within 50ms".to_owned()));
    assert!(start.elapsed() >= WAIT, "{:?}", start.elapsed());

    // Ending short; refusing.
    assert_eq!(read_to(Box::pin(pieces(1)), 15, WAIT).await, Ok(vec![1; 10]));
    let refused = futures::stream::iter([Err(Status::not_found("no log"))]);
    let read = read_to(Box::pin(refused), 15, WAIT).await;
    assert_eq!(read, Err("NotFound: no log".to_owned()));
}

/// board 8453: a fence answer is a take's only if it names at most `replica::LOGS_MOST` logs, in
/// order, none twice, their ends within the bytes a member holds of a configuration; the edges of
/// counts, log numbers and ends drawn (0, 1, the bounds and past them, `u64::MAX`).
#[test]
fn fence_answers_past_what_a_member_holds_are_refused() {
    use replica::LOGS_MOST;
    const MOST: u64 = 1 << 30;
    let answer = |logs: &[(u64, u64)]| FenceResponse {
        logs: logs.iter().map(|&(log, end)| LogEnd { log, end }).collect(),
    };
    let many = |n: usize| (0..n as u64).map(|log| (log, 0)).collect::<Vec<_>>();
    for taken in
        [vec![], vec![(0, 0)], vec![(0, MOST)], vec![(1, MOST - 1), (u64::MAX, 1)], many(LOGS_MOST)]
    {
        assert_eq!(fence_answer(answer(&taken), MOST), Ok(taken.clone()));
    }
    let refused = |logs: &[(u64, u64)]| fence_answer(answer(logs), MOST).unwrap_err();
    let past = |log, end| {
        format!("log {log}'s end, {end}, takes the logs' past the {MOST} bytes a member holds")
    };
    assert_eq!(refused(&[(0, MOST + 1)]), past(0, MOST + 1));
    assert_eq!(refused(&[(4, u64::MAX)]), past(4, u64::MAX));
    assert_eq!(refused(&[(2, MOST), (3, 1)]), past(3, 1));
    assert_eq!(refused(&[(2, 1), (3, u64::MAX)]), past(3, u64::MAX));
    assert_eq!(refused(&[(3, 1), (2, 1)]), "log 2 answered after log 3: out of order, or twice");
    assert_eq!(refused(&[(3, 1), (3, 1)]), "log 3 answered after log 3: out of order, or twice");
    let too_many = format!(
        "{} logs answered, past the {LOGS_MOST} a member holds of a configuration",
        LOGS_MOST + 1
    );
    assert_eq!(refused(&many(LOGS_MOST + 1)), too_many);
    let a_million =
        format!("1000000 logs answered, past the {LOGS_MOST} a member holds of a configuration");
    assert_eq!(refused(&many(1_000_000)), a_million);
}

/// board 8453: a take's fence of a member holding more logs of the configuration than a member
/// may (as one of another build, or a hostile one, answers) fails that member's answer, through
/// the daemon's client; and a fence answer past [`FENCE_ANSWER_MOST`] bytes, a million logs, is
/// refused by its decoder before it is held.
#[test]
fn takes_refuse_members_answering_more_logs_than_a_member_holds() {
    use local_cell::upload::Members as _;
    let dir = tempfile::tempdir().unwrap();
    // Logs a member of this build never holds so many of, its files left as one would, before it
    // opens its directory again.
    drop(
        super::replicas(cladefs_core::Host::process().local(), &[dir.path().join("replicas")])
            .unwrap(),
    );
    for log in 0..=replica::LOGS_MOST as u64 {
        let [tenant, cell, number, epoch, log] = [3, 9, 1, 4, log * 2 + 1].map(base32_id::Padded);
        let path = dir.path().join(format!("replicas/{tenant}.{cell}.{number}.{epoch}.{log}"));
        std::fs::File::create(path).unwrap();
    }
    let served = counted_member(dir.path(), true);
    let (address, replicas) = (served.address, served.replicas.unwrap());
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let members = ReplicaMembers {
        bucket: URL.to_owned(),
        me: "127.0.0.1:1".to_owned(),
        incarnation: 1,
        daemon: Daemon::process().clone(),
    };
    let cell = cladefs_core::CellId { tenant: 3, id: 9 };
    runtime.block_on(heading(&*served.bucket, cell, 5, 0, Some(Key { number: 1, epoch: 4 })));
    let configuration =
        bucket_cell::layout::Configuration { number: 1, epoch: 4, f: 1, members: Vec::new() };
    let whom = bucket_cell::layout::Member { address, incarnation: replicas.incarnation() };
    let fenced = runtime.block_on(members.fence(cell, &configuration, &whom, 5));
    let refusal = format!(
        "{} logs answered, past the {} a member holds of a configuration",
        replica::LOGS_MOST + 1,
        replica::LOGS_MOST
    );
    assert_eq!(fenced, Err(refusal));
    // Answered by the million: past what the decoder takes.
    let logs = (0..1_000_000u64).map(|log| LogEnd { log: u64::MAX - log, end: u64::MAX });
    let answer = prost::Message::encoded_len(&FenceResponse { logs: logs.collect() });
    assert!(answer > FENCE_ANSWER_MOST, "{answer}");
}

/// board 8573: an accept that fails as the host may get over (EMFILE, as with every descriptor
/// open) is tried again after `ACCEPT_AGAIN`, not at once, over and over, and the connections
/// after it served; one gone before it was accepted is passed over at once; one of the listener's
/// own (EBADF) ends them, noted, as the daemon then stops saying why.
#[tokio::test]
async fn accepts_failing_are_tried_again_after_a_pause() {
    use futures::StreamExt as _;
    use std::io::{Error, ErrorKind};
    let failing = |errno| Err::<u32, _>(Error::from_raw_os_error(errno));
    let failed = Arc::new(std::sync::OnceLock::new());
    let start = tokio::time::Instant::now();
    let incoming = futures::stream::iter([failing(libc::EMFILE), failing(libc::ENFILE), Ok(1)]);
    let mut accepted = Box::pin(crate::run::accepting("a peer's", incoming, failed.clone()));
    assert_eq!(accepted.next().await.unwrap().unwrap(), 1);
    let waited = start.elapsed();
    assert!(waited >= 2 * crate::server::ACCEPT_AGAIN, "{waited:?}");

    let start = tokio::time::Instant::now();
    let aborted = Err(Error::from(ErrorKind::ConnectionAborted));
    let incoming = futures::stream::iter([aborted, Ok(2), failing(libc::EBADF), Ok(3)]);
    let mut accepted = Box::pin(crate::run::accepting("a peer's", incoming, failed.clone()));
    assert_eq!(accepted.next().await.unwrap().unwrap(), 2);
    let waited = start.elapsed();
    assert!(waited < crate::server::ACCEPT_AGAIN, "{waited:?}");
    assert!(accepted.next().await.is_none());
    assert_eq!(failed.get().unwrap().raw_os_error(), Some(libc::EBADF));
}

/// board 8573: the peer service goes on serving after an accept failed as the host may get over,
/// and ends, saying why, once accepting fails for good, so that the daemon stops saying so (it
/// passed every failure over, at once, again and again, a dead listener's included).
#[test]
fn peers_are_served_after_accepts_fail() {
    use futures::StreamExt as _;
    let serve_with = |first: std::io::Error| {
        let dir = tempfile::tempdir().unwrap();
        let bucket = holding(&noise(1)).0;
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), u64::MAX).unwrap());
        let serving = cladefs_core::bucket::serving_peers(bucket, cache).unwrap();
        let replicas = Some(
            super::replicas(cladefs_core::Host::process().local(), &[dir.path().join("replicas")])
                .unwrap(),
        );
        let server = Server {
            daemon: Daemon::process().clone(),
            bucket: URL.to_owned(),
            region: None,
            serving,
            replicas,
            heads: holding(&noise(1)).0,
            appending: Arc::default(),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let served = std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let incoming = futures::stream::iter([Err(first)])
                    .chain(tonic::transport::server::TcpIncoming::from(listener));
                let served = serve_on(admitting(Box::pin(incoming), Arc::default()), server).await;
                drop(dir);
                served
            })
        });
        (address, served)
    };
    let (address, _served) = serve_with(std::io::Error::from_raw_os_error(libc::EMFILE));
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let mut client = super::member(&runtime, &address).unwrap();
    runtime.block_on(within(async move {
        client.incarnation(asking()).await.unwrap();
    }));
    let (_, served) = serve_with(std::io::Error::from_raw_os_error(libc::EBADF));
    let ended = served.join().unwrap().unwrap_err();
    assert_eq!(ended.raw_os_error(), Some(libc::EBADF), "{ended}");
}

/// board 8575: a writer's host lost without a word (no FIN, no reset: its packets dropped) leaves
/// no stream of appends on its member: the member's server pings the connection, unanswered, and
/// closes it within `KEEPALIVE` and `CONNECT`, its stream's task ending with it. The writer here
/// pings not, as a host lost does not.
#[test]
fn streams_of_writers_lost_without_a_word_end() {
    use std::sync::atomic::AtomicBool;
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let incarnation = served.replicas.unwrap().incarnation();
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let cell = cladefs_core::CellId { tenant: 3, id: 9 };
    runtime.block_on(heading(&*served.bucket, cell, 4, 0, Some(Key { number: 1, epoch: 4 })));
    let cut = Arc::new(AtomicBool::new(false));
    let proxy = runtime.block_on(proxy(served.address, cut.clone()));
    let appending = served.appending;
    runtime.block_on(within(async move {
        let mut client = PeerClient::new(endpoint(&proxy).unwrap().connect_lazy());
        let (sending, sent) = tokio::sync::mpsc::channel(1);
        let mut answers = (client.append(tokio_stream::wrappers::ReceiverStream::new(sent)))
            .await
            .unwrap()
            .into_inner();
        let first =
            AppendRequest { of: of(incarnation, 1, 4), log: 1, offset: 0, bytes: b"x".to_vec() };
        sending.send(first).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 1, end: 1 }));
        assert_eq!(appending.load(Ordering::Relaxed), 1);
        cut.store(true, Ordering::Relaxed);
        let lost = tokio::time::Instant::now();
        while appending.load(Ordering::Relaxed) > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let ended = lost.elapsed();
        assert!(ended <= KEEPALIVE * 2 + CONNECT, "{ended:?}");
        drop((sending, answers));
    }));
}

/// cladefs: a proxy on the loopback to `to`, relaying each connection both ways until `cut`: then
/// relaying nothing more and keeping its connections open, as the network to a host lost without
/// a word does. Its address.
async fn proxy(to: String, cut: Arc<std::sync::atomic::AtomicBool>) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    async fn relay(
        mut from: tokio::net::tcp::OwnedReadHalf,
        mut to: tokio::net::tcp::OwnedWriteHalf,
        cut: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let mut bytes = vec![0; 64 << 10];
        loop {
            let read = from.read(&mut bytes).await;
            if cut.load(Ordering::Relaxed) {
                // Its halves held for ever: nothing more relayed, nothing closed.
                std::future::pending::<()>().await;
            }
            match read {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if to.write_all(&bytes[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (client, _) = listener.accept().await.unwrap();
            let server = tokio::net::TcpStream::connect(&to).await.unwrap();
            let ((client_reads, client_writes), (server_reads, server_writes)) =
                (client.into_split(), server.into_split());
            tokio::spawn(relay(client_reads, server_writes, cut.clone()));
            tokio::spawn(relay(server_reads, client_writes, cut.clone()));
        }
    });
    address
}

/// board 8575: every replicated cell's link to a member, and every fence, log read, drop and
/// incarnation asked of it, share one connection to it: the connections it accepts do not grow
/// with the cells.
#[test]
fn members_are_reached_on_one_connection_whatever_the_cells() {
    use local_cell::replicate::own_of;
    use local_cell::upload::Members as _;
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let incarnation = served.replicas.unwrap().incarnation();
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let members = ReplicaMembers {
        bucket: URL.to_owned(),
        me: "127.0.0.1:1".to_owned(),
        incarnation: 1,
        daemon: Daemon::process().clone(),
    };
    let configuration =
        bucket_cell::layout::Configuration { number: 1, epoch: 4, f: 1, members: Vec::new() };
    let whom = bucket_cell::layout::Member { address: served.address.clone(), incarnation };
    let cells = (0..32).map(|id| cladefs_core::CellId { tenant: 3, id });
    let links: Vec<_> =
        cells.clone().map(|cell| members.link(cell, &configuration, &whom)).collect();
    runtime.block_on(within(async {
        for cell in cells.clone() {
            heading(&*served.bucket, cell, 5, 0, Some(Key { number: 1, epoch: 4 })).await;
        }
        for link in &links {
            assert_eq!(link.append(own_of(1), 0, b"abc".to_vec()).await, Ok(3));
        }
        for cell in cells {
            assert_eq!(
                members.fence(cell, &configuration, &whom, 5).await,
                Ok(vec![(own_of(1), 3)])
            );
            assert_eq!(
                members.read(cell, &configuration, &whom, own_of(1), 3).await,
                Ok(b"abc".to_vec())
            );
            assert_eq!(members.drop_before(cell, &configuration, &whom).await, Ok(()));
        }
        assert_eq!(members.incarnation_of(&served.address).await, Ok(incarnation));
    }));
    assert_eq!(served.accepted.load(Ordering::Relaxed), 1);
}

/// cladefs: the tests' peer credentials, known to all, never a host's (`tests/peer-tls`): the
/// hosts' CA's, and another's.
fn fixtures(other: bool) -> std::path::PathBuf {
    // cladefs: the fixtures stayed in crates/cladefs as crates/cladefs split (board 13065).
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../cladefs/tests/peer-tls");
    if other { dir.join("other") } else { dir }
}

/// cladefs: a client of the peer at `address`: in the clear without `tls`.
fn client_of(address: &str, tls: Option<tonic::transport::ClientTlsConfig>) -> PeerClient<Channel> {
    let endpoint = match tls {
        None => Endpoint::from_shared(format!("http://{address}")).unwrap(),
        Some(tls) => {
            let endpoint = Endpoint::from_shared(format!("https://{address}")).unwrap();
            endpoint.tls_config(tls).unwrap()
        }
    };
    PeerClient::new(endpoint.connect_timeout(CONNECT).timeout(DEADLINE).connect_lazy())
}

/// cladefs: every call of the peer service asked by `client`, by its name, each's answer.
async fn every_call(client: &mut PeerClient<Channel>) -> Vec<(&'static str, Result<(), Status>)> {
    let appends = futures::stream::iter([AppendRequest { of: of(1, 1, 4), ..Default::default() }]);
    vec![
        ("GetPart", client.get_part(GetPartRequest::default()).await.map(drop)),
        ("Incarnation", client.incarnation(asking()).await.map(drop)),
        ("Append", client.append(appends).await.map(drop)),
        ("Fence", client.fence(FenceRequest { of: of(1, 1, 4), epoch: 5 }).await.map(drop)),
        ("ReadLog", client.read_log(ReadLogRequest::default()).await.map(drop)),
        ("DropLogs", client.drop_logs(DropLogsRequest::default()).await.map(drop)),
        (
            "MigratePrepare",
            client.migrate_prepare(proto::MigratePrepareRequest::default()).await.map(drop),
        ),
        ("MigrateFile", client.migrate_file(proto::MigrateFileRequest::default()).await.map(drop)),
        (
            "MigrateHandOver",
            client.migrate_hand_over(proto::MigrateHandOverRequest::default()).await.map(drop),
        ),
        (
            "MigrateAbort",
            client.migrate_abort(proto::MigrateAbortRequest::default()).await.map(drop),
        ),
        (
            "MigrateRenew",
            client.migrate_renew(proto::MigrateRenewRequest::default()).await.map(drop),
        ),
        (
            "MigrateRelease",
            client.migrate_release(proto::MigrateReleaseRequest::default()).await.map(drop),
        ),
    ]
}

/// board 8437: the peer service and its clients authenticate each other by mutual TLS before any
/// call is served: a client in the clear, one of no certificate, and one whose certificate another
/// CA signed are refused every call, none reaching the member (its replica directory as it was);
/// and a client refuses a service whose certificate another CA signed. A host's credentials are
/// served.
#[test]
fn peers_unauthenticated_are_refused_every_call() {
    use tonic::transport::{Certificate, ClientTlsConfig, Identity};
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let read = |dir: std::path::PathBuf, name: &str| std::fs::read(dir.join(name)).unwrap();
    let ca = |other| Certificate::from_pem(read(fixtures(other), "ca.crt"));
    let identity = |other| {
        Identity::from_pem(read(fixtures(other), "peer.crt"), read(fixtures(other), "peer.key"))
    };
    let tls = || ClientTlsConfig::new().domain_name(PEER_NAME);
    let clients = [
        ("in the clear", None),
        ("of no certificate", Some(tls().ca_certificate(ca(false)))),
        ("of another CA", Some(tls().ca_certificate(ca(false)).identity(identity(true)))),
        ("trusting another CA", Some(tls().ca_certificate(ca(true)).identity(identity(true)))),
    ];
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let _entered = runtime.enter();
    // Every call's answer, by its client, as its transport ends it: a client in the clear speaks
    // HTTP/2 to a TLS service, one the service refuses at its handshake's end finds its
    // connection closed as it sends (Unknown, or Cancelled as the race goes), and one refusing the
    // service fails its handshake (Unavailable). None is the service's own answer to the call,
    // which would be FAILED_PRECONDITION, INVALID_ARGUMENT or NOT_FOUND here.
    let transport = [Code::Unknown, Code::Cancelled, Code::Unavailable];
    let (mut answered, mut expected) = (Vec::new(), Vec::new());
    for (whose, config) in clients {
        let mut client = client_of(&served.address, config);
        let answers = runtime.block_on(within_answers(every_call(&mut client)));
        for (call, answer) in answers {
            let code = answer.map_err(|status| status.code()).err();
            answered.push((whose, call, code.filter(|code| transport.contains(code)).is_some()));
            expected.push((whose, call, true));
        }
    }
    assert_eq!(answered, expected, "true: refused by its transport");
    assert_eq!(SERVED.load(Ordering::Relaxed) + SERVED_MISSING.load(Ordering::Relaxed), 0);
    let replicas = served.replicas.unwrap();
    assert_eq!(replicas.cells().unwrap(), []);
    let mut ours = super::member(&runtime, &served.address).unwrap();
    let incarnation = runtime.block_on(ours.incarnation(asking())).unwrap().into_inner();
    assert_eq!(incarnation.incarnation, replicas.incarnation());
}

/// cladefs: `answers`, failing the test if they take more than 30 s.
async fn within_answers<T>(answers: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), answers).await.expect("a call hung")
}

/// board 8437: each call's message past what it needs is refused before it is decoded whole
/// (OUT_OF_RANGE), its own limit, not the service's one of 64 MiB: a fence of a few KiB more than
/// a call naming a few numbers needs, an append of more than a writer's piece; one within its
/// call's limit is taken.
#[test]
fn messages_past_what_their_calls_need_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let incarnation = served.replicas.unwrap().incarnation();
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let mut client = super::member(&runtime, &served.address).unwrap();
    runtime.block_on(within(async move {
        let mut large = of(incarnation, 1, 4);
        large.as_mut().unwrap().bucket = "x".repeat(SMALL_MOST);
        let refused = client.fence(FenceRequest { of: large, epoch: 5 }).await.unwrap_err();
        assert_eq!(refused.code(), Code::OutOfRange, "{refused:?}");
        let appends = |len: usize| {
            futures::stream::iter([AppendRequest {
                of: of(incarnation, 1, 4),
                log: 1,
                offset: 0,
                bytes: vec![1; len],
            }])
        };
        let mut answers = client.append(appends(APPEND_MOST)).await.unwrap().into_inner();
        let refused = answers.message().await.unwrap_err();
        assert_eq!(refused.code(), Code::OutOfRange, "{refused:?}");
        // Within it, the member's cell has no head: refused as such, decoded.
        let mut answers =
            client.append(appends(local_cell::replicate::PIECE)).await.unwrap().into_inner();
        let refused = answers.message().await.unwrap_err();
        assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
        assert!(
            refused.message().ends_with("no live replicated head of it: refused"),
            "{refused:?}"
        );
    }));
}

/// board 8437: the peer service holds at most `PEER_CONNECTIONS_MOST` connections of a peer's
/// address: the next is closed at once, those held kept.
#[test]
fn peers_connections_past_their_most_are_closed() {
    use std::io::Read as _;
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let held: Vec<std::net::TcpStream> = (0..PEER_CONNECTIONS_MOST)
        .map(|_| std::net::TcpStream::connect(&served.address).unwrap())
        .collect();
    let mut past = std::net::TcpStream::connect(&served.address).unwrap();
    past.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut byte = [0; 1];
    assert_eq!(past.read(&mut byte).unwrap(), 0, "the connection past the most kept");
    let mut first = &held[0];
    first.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let kept = first.read(&mut byte).unwrap_err();
    assert_eq!(kept.kind(), std::io::ErrorKind::WouldBlock, "{kept}");
}

#[test]
fn only_structured_fencing_refusals_fence_a_writer() {
    use local_cell::replicate::LinkError;

    let status = refused(replica::Error::Refused(Refused::Format {
        log: "log".into(),
        found: "fenced".into(),
    }));
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(matches!(link_error(&status), LinkError::Refused(_)), "{status:?}");

    let fenced = refused(replica::Error::Refused(Refused::Fenced { promise: 7 }));
    assert_eq!(fenced.code(), Code::FailedPrecondition);
    assert!(matches!(link_error(&fenced), LinkError::Fenced(_)), "{fenced:?}");
}

/// board 8445: a member admits a replication call only by its cell's head, read from the bucket
/// for it: a fence at `u64::MAX`, or past the head's epoch, and a drop through `u64::MAX`, refused
/// over gRPC, nothing promised or dropped, the writer appending still; a cell with no head refused
/// every call.
#[test]
fn members_admit_calls_by_their_cells_heads() {
    let dir = tempfile::tempdir().unwrap();
    let served = counted_member(dir.path(), true);
    let (bucket, replicas) = (served.bucket, served.replicas.unwrap());
    let incarnation = replicas.incarnation();
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let mut client = super::member(&runtime, &served.address).unwrap();
    let cell = cladefs_core::CellId { tenant: 3, id: 9 };
    let noted = replicas.clone();
    runtime.block_on(within(async move {
        let unasked = |status: Status| {
            assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
            assert!(status.message().ends_with(": refused"), "{status:?}");
            assert!(matches!(link_error(&status), local_cell::replicate::LinkError::Refused(_)));
        };
        // No head: every call refused.
        unasked(
            client.fence(FenceRequest { of: of(incarnation, 1, 4), epoch: 4 }).await.unwrap_err(),
        );
        let dropped = DropLogsRequest { of: of(incarnation, 1, 4), through: None };
        unasked(client.drop_logs(dropped).await.unwrap_err());
        heading(&*bucket, cell, 4, 1, Some(Key { number: 1, epoch: 4 })).await;
        let (sending, sent) = tokio::sync::mpsc::channel(4);
        let mut answers = (client.append(tokio_stream::wrappers::ReceiverStream::new(sent)))
            .await
            .unwrap()
            .into_inner();
        let append = |log, offset, bytes: &[u8]| AppendRequest {
            of: of(incarnation, 1, 4),
            log,
            offset,
            bytes: bytes.to_vec(),
        };
        sending.send(append(1, 0, b"told")).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 1, end: 4 }));
        // The head its stream was admitted by, noted once its first files were made (Codex's
        // review): what admits its writer's streams opened again with the bucket out of reach.
        let head = Seen { epoch: 4, end: 1, active: Some(Key { number: 1, epoch: 4 }) };
        assert_eq!(noted.seen(cell).unwrap(), Some(head));
        for epoch in [5, u64::MAX - 1, u64::MAX] {
            let fence = FenceRequest { of: of(incarnation, 1, 4), epoch };
            unasked(client.fence(fence).await.unwrap_err());
        }
        // A drop of a configuration no writer of the head's appends to, through any log.
        for (number, epoch) in [(0, 4), (1, 3), (2, 5), (u64::MAX, u64::MAX)] {
            for through in [0, u64::MAX] {
                let of = of(incarnation, number, epoch);
                unasked(
                    client
                        .drop_logs(DropLogsRequest { of, through: Some(through) })
                        .await
                        .unwrap_err(),
                );
            }
        }
        sending.send(append(1, 4, b" more")).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 1, end: 9 }));
    }));
    assert_eq!(replicas.cells().unwrap(), [(cell, vec![Key { number: 1, epoch: 4 }])]);
}

/// Codex's review of boards 8445 and 8451: with the bucket out of reach, a stream of appends to a
/// configuration after the head's active one is admitted by the head the member noted before, and
/// trims nothing by it: that head's active configuration may be one the bucket's names no more,
/// and the one it names since, holding commits told replicated, must stay. Its configurations'
/// logs are all kept.
#[test]
fn streams_admitted_by_noted_heads_trim_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bucket");
    std::fs::create_dir(&path).unwrap();
    let url = format!("file://{}/", path.display());
    let bucket =
        Arc::new(bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap().unrouted());
    let served = counted_member_of(dir.path(), bucket.clone());
    let replicas = served.replicas.unwrap();
    let incarnation = replicas.incarnation();
    let cell = cladefs_core::CellId { tenant: 3, id: 9 };
    let key = |number| Key { number, epoch: 5 };
    let frame = local_cell::replicate::own_frame(1, b"told");
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    // The head names configuration 2 active, the member noting configuration 1 so, as it read
    // before the swap, the bucket then out of reach.
    runtime.block_on(heading(&*bucket, cell, 5, 0, Some(key(2))));
    for number in [1, 2] {
        replicas.append((cell, key(number), incarnation), 1, 0, &frame).unwrap();
    }
    replicas.saw(cell, Seen { epoch: 5, end: 0, active: Some(key(1)) }).unwrap();
    std::fs::rename(&path, path.with_extension("aside")).unwrap();
    std::fs::write(&path, b"out of reach").unwrap();
    let mut client = super::member(&runtime, &served.address).unwrap();
    runtime.block_on(within(async move {
        let (sending, sent) = tokio::sync::mpsc::channel(1);
        let mut answers = (client.append(tokio_stream::wrappers::ReceiverStream::new(sent)))
            .await
            .unwrap()
            .into_inner();
        let first = AppendRequest { of: of(incarnation, 3, 5), log: 1, offset: 0, bytes: frame };
        sending.send(first).await.unwrap();
        let end = answers.message().await.unwrap().unwrap().end;
        assert_eq!(end, local_cell::replicate::own_frame(1, b"told").len() as u64);
    }));
    assert_eq!(replicas.cells().unwrap(), [(cell, vec![key(1), key(2), key(3)])]);
}

/// cladefs: the value of the series `series`, as the daemon's metrics render it now.
fn metric(series: &str) -> Option<u64> {
    let metrics = crate::metrics::render();
    let line = metrics.lines().find_map(|line| line.strip_prefix(series)?.strip_prefix(' '));
    line.map(|value| value.parse().unwrap())
}

/// board 8569, board 8565: a call a host makes of a peer within a span sends its trace's context
/// (W3C's `traceparent`), and the peer's span of the call is a child of the caller's, on its
/// trace: a span of each side, the client's a child of the span it was made in. Each side counts
/// it in its gRPC metrics, by method, a failure as failed. Before, no call carried context, every
/// span the peer made beginning a trace of its own, and no call was counted.
#[test]
fn peer_calls_carry_their_trace_and_are_counted_on_both_sides() {
    use opentelemetry::trace::{SpanKind, TraceContextExt as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SimpleSpanProcessor};
    let dir = tempfile::tempdir().unwrap();
    let (address, _replicas) = member(dir.path(), true);
    // Once the member serves, its bucket made: the in-memory exporter exports as each span ends,
    // which the bucket's making, on futures' executor, may not.
    let exporter = InMemorySpanExporter::default();
    let processor = SimpleSpanProcessor::new(exporter.clone());
    crate::otel::install(Sampler::AlwaysOn, processor);
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let mut client = super::member(&runtime, &address).unwrap();
    let asking_within = bucket_cell::trace::Span::start("a writer's work");
    let caller = asking_within.context().unwrap().span().span_context().clone();
    runtime.block_on(asking_within.around(client.incarnation(asking()))).unwrap();
    let fence = FenceRequest { of: None, epoch: 5 };
    runtime.block_on(asking_within.around(client.fence(fence))).unwrap_err();
    drop(asking_within);

    let name = "cladefs.peer.v1.Peer/Incarnation";
    let deadline = Instant::now() + Duration::from_secs(30);
    let spans = loop {
        let spans = exporter.get_finished_spans().unwrap();
        if spans.iter().filter(|span| span.name == name).count() == 2 {
            break spans;
        }
        assert!(
            Instant::now() < deadline,
            "{:?}",
            spans.iter().map(|span| &span.name).collect::<Vec<_>>()
        );
        std::thread::yield_now();
    };
    let of = |kind: SpanKind| {
        spans.iter().find(|span| span.name == name && span.span_kind == kind).unwrap()
    };
    let (made, served) = (of(SpanKind::Client), of(SpanKind::Server));
    assert_eq!(made.span_context.trace_id(), caller.trace_id());
    assert_eq!(made.parent_span_id, caller.span_id());
    assert_eq!(
        served.span_context.trace_id(),
        caller.trace_id(),
        "the peer's span on the caller's trace"
    );
    assert_eq!(served.parent_span_id, made.span_context.span_id());
    assert!(served.parent_span_is_remote);

    let labels = |method: &str| format!("{{service=\"cladefs.peer.v1.Peer\",method=\"{method}\"}}");
    for side in ["server", "client"] {
        let counted = |family: &str, method: &str| {
            metric(&format!("cladefsd_grpc_{side}_{family}{}", labels(method)))
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while counted("calls_total", "Fence") != Some(1) {
            assert!(Instant::now() < deadline, "{side}: {:?}", counted("calls_total", "Fence"));
            std::thread::yield_now();
        }
        assert_eq!(counted("calls_total", "Incarnation"), Some(1), "{side}");
        assert_eq!(counted("errors_total", "Incarnation"), Some(0), "{side}");
        assert_eq!(counted("errors_total", "Fence"), Some(1), "{side}");
        assert_eq!(counted("calls_total", "GetPart"), Some(0), "{side}: there from the start");
        assert_eq!(counted("calls_total", "unknown"), Some(0), "{side}");
    }
}

/// hotbed review 80, board 8569: what serving a peer's call hands to the bucket runtime goes on the
/// call's trace: a part fetched cold for a peer, its bucket requests' spans on the trace of the
/// asker's span, as the served call's span is. Before, the fetch was spawned with no context, its
/// spans each a trace of its own.
#[test]
fn a_parts_bucket_requests_for_a_peer_are_on_its_trace() {
    use opentelemetry::trace::{SpanKind, TraceContextExt as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SimpleSpanProcessor};
    let (bucket, location) = holding(&noise(3 * layout::PART));
    let dir = tempfile::tempdir().unwrap();
    let address = serving(&bucket, dir.path());
    let (asked, bytes) = asked(&bucket, &location, 1);
    // Once the bucket is made, on futures' executor, where the in-memory exporter may not export.
    let exporter = InMemorySpanExporter::default();
    let processor = SimpleSpanProcessor::new(exporter.clone());
    crate::otel::install(Sampler::AlwaysOn, processor);
    let asking = bucket_cell::trace::Span::start("a fetcher's read");
    let trace = asking.context().unwrap().span().span_context().trace_id();
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let link = link(Daemon::process(), &address, (URL, None));
    assert_eq!(runtime.block_on(asking.around(link.get(asked))), Ok(Some(bytes)));
    drop(asking);
    let deadline = Instant::now() + Duration::from_secs(30);
    let served = loop {
        let spans = exporter.get_finished_spans().unwrap();
        let served = spans.iter().find(|span| {
            span.name == "cladefs.peer.v1.Peer/GetPart" && span.span_kind == SpanKind::Server
        });
        if let Some(served) = served {
            break (served.clone(), spans);
        }
        assert!(Instant::now() < deadline, "no span of the served call");
        std::thread::yield_now();
    };
    let (served, spans) = served;
    assert_eq!(served.span_context.trace_id(), trace);
    let fetched: Vec<_> = spans.iter().filter(|span| span.name.starts_with("bucket.get")).collect();
    assert_eq!(fetched.len(), 2, "its tail and the part: {spans:?}");
    for span in fetched {
        assert_eq!(span.span_context.trace_id(), trace, "{} on a trace of its own", span.name);
    }
}

/// board 11659, board 11713: more than `PEER_CONNECTIONS_MOST` (16) concurrent migration/peer
/// clients, plus `GetPart` and replication links, all multiplex over one pooled HTTP/2 connection
/// per peer on `Daemon::peer_channels` instead of exhausting the peer connection limit.
#[test]
fn concurrent_peer_and_migration_clients_exceeding_connection_limit_share_one_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (bucket, location) = holding(&noise(3 * layout::PART));
    let served = counted_member_of(dir.path(), bucket.clone());
    let (part_asked, expected_bytes) = asked(&bucket, &location, 1);
    let incarnation = served.replicas.unwrap().incarnation();
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let daemon = Daemon::new();
    let count = PEER_CONNECTIONS_MOST + 8;
    let clients: Vec<_> =
        (0..count).map(|_| super::client_on(&daemon, &served.address).unwrap()).collect();
    let part_link = link(&daemon, &served.address, (URL, None));
    runtime.block_on(within(async {
        let mut handles = Vec::with_capacity(count);
        for (mut client, _channel) in clients {
            handles.push(tokio::spawn(async move {
                let asked = IncarnationRequest {
                    bucket: URL.to_owned(),
                    #[allow(deprecated)]
                    formats: String::new(),
                    versions: Some(super::current_formats()),
                };
                client.incarnation(asked).await.unwrap().into_inner().incarnation
            }));
        }
        assert_eq!(part_link.get(part_asked).await, Ok(Some(expected_bytes)));
        for handle in handles {
            assert_eq!(handle.await.unwrap(), incarnation);
        }
    }));
    assert_eq!(
        served.accepted.load(Ordering::Relaxed),
        1,
        "GetPart and {count} concurrent migration/peer clients should share one HTTP/2 connection"
    );
}

/// Hotbed review 140 finding 520: a racing `SetPeers` RPC completing after the events poller has
/// already installed a newer generation must not overwrite the daemon's peer list with the stale
/// generation.
#[test]
fn set_generation_ignores_stale_peer_list() {
    struct NoopLink;
    impl Link for NoopLink {
        fn get(&self, _asked: Asked) -> BoxFuture<'static, Result<Option<Vec<u8>>, Refusal>> {
            Box::pin(std::future::ready(Ok(None)))
        }
    }

    let daemon = Daemon::new();
    let bucket =
        Arc::new(RoutedBucket::unrouted(Arc::new(bucket_cell::object_bucket::ObjectBucket::new(
            Arc::new(object_store::memory::InMemory::new()),
            "peers-gen-test",
        ))));
    let peers = cladefs_core::bucket::peers::Peers::new("127.0.0.1:9000", |_| Arc::new(NoopLink));
    cladefs_core::bucket::ask_peers_on(&daemon.host, bucket, peers.clone()).unwrap();

    assert_eq!(
        super::set_generation_on(&daemon, 2, vec!["127.0.0.1:9002".into()]),
        Ok(true),
        "generation 2 should apply"
    );
    assert_eq!(
        super::set_generation_on(&daemon, 1, vec!["127.0.0.1:9001".into()]),
        Ok(false),
        "stale generation 1 must be ignored"
    );
    assert_eq!(peers.addresses(), vec!["127.0.0.1:9000".to_owned(), "127.0.0.1:9002".to_owned()]);
    assert_eq!(
        super::set_generation_on(&daemon, 3, vec!["127.0.0.1:9003".into()]),
        Ok(true),
        "newer generation 3 should apply"
    );
    assert_eq!(peers.addresses(), vec!["127.0.0.1:9000".to_owned(), "127.0.0.1:9003".to_owned()]);
}
