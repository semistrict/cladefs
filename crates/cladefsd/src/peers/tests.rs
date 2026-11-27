// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's peers over gRPC: parts served, refusals, and a peer out of reach. What
//! asking peers saves, and how it falls back, `crate::bucket::peers`' tests show, and two daemons
//! `tests/peers.rs`.

use super::*;
use std::sync::atomic::AtomicUsize;

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
    let server =
        Server { daemon: Daemon::process().clone(), bucket: URL.to_owned(), region, serving };
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
    tokio::time::timeout(Duration::from_secs(30), work).await.expect("a peer call hung");
}

/// cladefs: a peer served as [`counted_peer`] serves it, and its accepted-connection count.
struct Served {
    address: String,
    /// The connections it accepted.
    accepted: Arc<AtomicUsize>,
}

/// cladefs: a daemon's peer server on the loopback, of the bucket [`URL`], counting accepted
/// connections (board 8575).
fn counted_peer(dir: &std::path::Path, bucket: Arc<RoutedBucket>) -> Served {
    use futures::StreamExt as _;
    let cache = Arc::new(Cache::open(&dir.join("cache"), u64::MAX).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let serving = cladefs_core::bucket::serving_peers(bucket, cache).unwrap();
    let server =
        Server { daemon: Daemon::process().clone(), bucket: URL.to_owned(), region: None, serving };
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
    Served { address, accepted }
}

/// Doc-28 (board 10054): rolling-upgrade format compatibility for `MigratePrepare`: receiver reads
/// sender's write versions.
#[test]
fn formats_within_supported_range_are_accepted_and_outside_refused() {
    let build_a_member = SupportedFormats {
        cell_directory: formats::Format { name: "cell directory", oldest: 18, current: 18 },
        cell_records: formats::Format { name: "cell records", oldest: 1, current: 1 },
        extent_frame: formats::Format { name: "extent frame", oldest: 3, current: 3 },
        bucket_layout: formats::Format { name: "bucket layout", oldest: 31, current: 31 },
    };
    let build_b_member = SupportedFormats {
        cell_directory: formats::Format { name: "cell directory", oldest: 18, current: 19 },
        cell_records: formats::Format { name: "cell records", oldest: 1, current: 2 },
        extent_frame: formats::Format { name: "extent frame", oldest: 3, current: 4 },
        bucket_layout: formats::Format { name: "bucket layout", oldest: 31, current: 32 },
    };
    let build_a = proto::Formats {
        cell_directory: 18,
        cell_records: 1,
        extent_frame: 3,
        bucket_layout: 31,
        cell_directory_oldest: 18,
        cell_records_oldest: 1,
        extent_frame_oldest: 3,
        bucket_layout_oldest: 31,
    };
    let build_b = proto::Formats {
        cell_directory: 19,
        cell_records: 2,
        extent_frame: 4,
        bucket_layout: 32,
        cell_directory_oldest: 18,
        cell_records_oldest: 1,
        extent_frame_oldest: 3,
        bucket_layout_oldest: 31,
    };

    // Build A -> Build B: accepted for Migration.
    build_b_member.check_migration(0x7, Some(build_a)).unwrap();

    // Build B -> Build A: refused for Migration.
    assert_eq!(
        build_a_member.check_migration(0x7, Some(build_b)).unwrap_err().code(),
        Code::FailedPrecondition
    );

    // Same-build B <-> B: accepted.
    build_b_member.check_migration(0x7, Some(build_b)).unwrap();

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
    let (bucket, location) = holding(&noise(layout::PART));
    let (part_asked, expected_bytes) = asked(&bucket, &location, 0);
    let serve_with = |first: std::io::Error| {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), u64::MAX).unwrap());
        let serving = cladefs_core::bucket::serving_peers(bucket.clone(), cache).unwrap();
        let server = Server {
            daemon: Daemon::process().clone(),
            bucket: URL.to_owned(),
            region: None,
            serving,
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
    assert_eq!(ask(&address, URL, part_asked), Ok(Some(expected_bytes)));
    let (_, served) = serve_with(std::io::Error::from_raw_os_error(libc::EBADF));
    let ended = served.join().unwrap().unwrap_err();
    assert_eq!(ended.raw_os_error(), Some(libc::EBADF), "{ended}");
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
    vec![
        ("GetPart", client.get_part(GetPartRequest::default()).await.map(drop)),
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
/// CA signed are refused every call; and a client refuses a service whose certificate another CA
/// signed. A host's credentials are served.
#[test]
fn peers_unauthenticated_are_refused_every_call() {
    use tonic::transport::{Certificate, ClientTlsConfig, Identity};
    let (bucket, location) = holding(&noise(layout::PART));
    let (part_asked, expected_bytes) = asked(&bucket, &location, 0);
    let dir = tempfile::tempdir().unwrap();
    let served = counted_peer(dir.path(), bucket);
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
    assert_eq!(ask(&served.address, URL, part_asked), Ok(Some(expected_bytes)));
}

/// cladefs: `answers`, failing the test if they take more than 30 s.
async fn within_answers<T>(answers: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), answers).await.expect("a call hung")
}

/// board 8437: each call's message past what it needs is refused before it is decoded whole
/// (OUT_OF_RANGE), its own limit, not the service's one of 64 MiB; one within its call's limit is
/// taken.
#[test]
fn messages_past_what_their_calls_need_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let served = counted_peer(dir.path(), holding(&noise(1)).0);
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let (mut client, _channel) = super::client_on(Daemon::process(), &served.address).unwrap();
    runtime.block_on(within(async move {
        let refused = client
            .get_part(GetPartRequest {
                bucket: URL.to_owned(),
                key: "x".repeat(SMALL_MOST),
                part: 0,
                ..GetPartRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::OutOfRange, "{refused:?}");
        // Within SMALL_MOST: decoded and refused by the handler as not a served part key.
        let decoded = client
            .get_part(GetPartRequest {
                bucket: URL.to_owned(),
                key: "not-a-segment".to_owned(),
                part: 0,
                ..GetPartRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(decoded.code(), Code::InvalidArgument, "{decoded:?}");
    }));
}

/// board 8437: the peer service holds at most `PEER_CONNECTIONS_MOST` connections of a peer's
/// address: the next is closed at once, those held kept.
#[test]
fn peers_connections_past_their_most_are_closed() {
    use std::io::Read as _;
    let dir = tempfile::tempdir().unwrap();
    let served = counted_peer(dir.path(), holding(&noise(1)).0);
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
    let (bucket, location) = holding(&noise(layout::PART));
    let (part_asked, expected_bytes) = asked(&bucket, &location, 0);
    let dir = tempfile::tempdir().unwrap();
    let address = serving(&bucket, dir.path());
    // Once the peer serves, its bucket made: the in-memory exporter exports as each span ends,
    // which the bucket's making, on futures' executor, may not.
    let exporter = InMemorySpanExporter::default();
    let processor = SimpleSpanProcessor::new(exporter.clone());
    crate::otel::install(Sampler::AlwaysOn, processor);
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let (mut client, _channel) = super::client_on(Daemon::process(), &address).unwrap();
    let asking_within = bucket_cell::trace::Span::start("a peer's work");
    let caller = asking_within.context().unwrap().span().span_context().clone();
    let ok_req = GetPartRequest {
        bucket: URL.to_owned(),
        key: part_asked.key,
        part: part_asked.part,
        ..GetPartRequest::default()
    };
    let got = runtime.block_on(asking_within.around(client.get_part(ok_req))).unwrap().into_inner();
    assert_eq!(got.bytes, expected_bytes);
    let bad_req = proto::MigratePrepareRequest::default();
    runtime.block_on(asking_within.around(client.migrate_prepare(bad_req))).unwrap_err();
    drop(asking_within);

    let name = "cladefs.peer.v1.Peer/GetPart";
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
        while counted("calls_total", "MigratePrepare") != Some(1) {
            assert!(
                Instant::now() < deadline,
                "{side}: {:?}",
                counted("calls_total", "MigratePrepare")
            );
            std::thread::yield_now();
        }
        assert_eq!(counted("calls_total", "GetPart"), Some(1), "{side}");
        assert_eq!(counted("errors_total", "GetPart"), Some(0), "{side}");
        assert_eq!(counted("errors_total", "MigratePrepare"), Some(1), "{side}");
        assert_eq!(counted("calls_total", "MigrateAbort"), Some(0), "{side}: there from the start");
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
/// clients, plus `GetPart` links, all multiplex over one pooled HTTP/2 connection per peer on
/// `Daemon::peer_channels` instead of exhausting the peer connection limit.
#[test]
fn concurrent_peer_and_migration_clients_exceeding_connection_limit_share_one_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (bucket, location) = holding(&noise(3 * layout::PART));
    let served = counted_peer(dir.path(), bucket.clone());
    let (part_asked, expected_bytes) = asked(&bucket, &location, 1);
    let runtime = cladefs_core::bucket::bucket_runtime().unwrap();
    let daemon = Daemon::new();
    let count = PEER_CONNECTIONS_MOST + 8;
    let clients: Vec<_> =
        (0..count).map(|_| super::client_on(&daemon, &served.address).unwrap()).collect();
    let part_link = link(&daemon, &served.address, (URL, None));
    runtime.block_on(within(async {
        let mut handles = Vec::with_capacity(count);
        for (mut client, _channel) in clients {
            let key = part_asked.key.clone();
            let part = part_asked.part;
            handles.push(tokio::spawn(async move {
                client
                    .get_part(GetPartRequest {
                        bucket: URL.to_owned(),
                        key,
                        part,
                        ..GetPartRequest::default()
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .bytes
            }));
        }
        assert_eq!(part_link.get(part_asked).await, Ok(Some(expected_bytes.clone())));
        for handle in handles {
            assert_eq!(handle.await.unwrap(), expected_bytes);
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
