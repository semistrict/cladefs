// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's peers over gRPC: parts served, refusals, and a peer out of reach. What
//! asking peers saves, and how it falls back, `crate::bucket::peers`' tests show, and two daemons
//! `tests/peers.rs`.

use super::*;
use crate::bucket::peers::tests::{gets, holding, noise};
use bucket_cell::cache::Cache;
use bucket_cell::layout::{self, Location};
use bucket_cell::object_bucket::ObjectBucket;
use std::time::Instant;

const URL: &str = "memory:///peers";

/// A daemon's peer server on the loopback, serving the parts of `bucket`, named [`URL`], kept in a
/// cache in `dir`: its address.
fn serving(bucket: &Arc<ObjectBucket>, dir: &std::path::Path) -> String {
    let cache = Arc::new(Cache::open(dir, u64::MAX).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let serving = crate::bucket::serving_peers(bucket.clone(), cache).unwrap();
    let server = Server { bucket: URL.to_owned(), serving, replicas: None };
    std::thread::spawn(move || serve(listener, server));
    address
}

/// Asks the peer at `address`, serving the bucket at `url`, for `asked`.
fn ask(address: &str, url: &str, asked: Asked) -> Result<Option<Vec<u8>>, String> {
    let runtime = crate::bucket::bucket_runtime().unwrap();
    runtime.block_on(link(&runtime, address, url).get(asked))
}

/// Part `part` of the object at `location`'s key in `bucket`, asked for by its key and number, and
/// its stored bytes.
fn asked(bucket: &ObjectBucket, location: &Location, part: u64) -> (Asked, Vec<u8>) {
    let object = crate::bucket::bucket_runtime().unwrap().block_on(async {
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
    assert_eq!(refused(URL, not_served), format!("InvalidArgument: {head}: no parts served"));
    let other = "memory:///other";
    assert_eq!(
        refused(other, asked),
        format!("FailedPrecondition: asked of {other}, this host's bucket being {URL}")
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
    let failed = ask(&address, URL, asked).unwrap_err();
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
    let bucket = crate::bucket::peers::tests::holding(&noise(1)).0;
    let cache = Arc::new(Cache::open(&dir.join("cache"), u64::MAX).unwrap());
    let replicas = with.then(|| super::replicas(&dir.join("replicas")).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let serving = crate::bucket::serving_peers(bucket, cache).unwrap();
    let server = Server { bucket: URL.to_owned(), serving, replicas: replicas.clone() };
    std::thread::spawn(move || serve(listener, server));
    (address, replicas)
}

fn of(incarnation: u64, number: u64, epoch: u64) -> Option<ReplicaOf> {
    Some(ReplicaOf {
        bucket: URL.to_owned(),
        tenant: 3,
        cell: 9,
        configuration: Some(proto::Configuration { number, epoch }),
        incarnation,
    })
}

/// cladefs: a writer streams appends to a member, each answered once synced; a take fences the
/// cell, learns the logs' ends and reads them back; the writer's appends after are refused,
/// ending its stream; and the logs the bucket holds are dropped (doc-21, TASK-537.3.2).
#[test]
fn members_take_appends_and_fences_over_grpc() {
    let dir = tempfile::tempdir().unwrap();
    let (address, _) = member(dir.path(), true);
    let runtime = crate::bucket::bucket_runtime().unwrap();
    let mut client = super::member(&runtime, &address).unwrap();
    // Bounded: a member and a writer waiting on each other fail here, not the run (a member
    // answering appends only once the first came deadlocked so, TASK-537.3.2).
    runtime.block_on(within(async move {
        let incarnation = (client.incarnation(IncarnationRequest { bucket: URL.to_owned() }).await)
            .unwrap()
            .into_inner()
            .incarnation;
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
        // message past tonic's size limit is answered with); one of 5 MiB, past tonic's default
        // limit, is taken (TASK-537.3.2's review).
        sending.send(append(2, 4, b"past")).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 2, end: 0 }));
        sending.send(append(2, 0, &vec![5; 5 << 20])).await.unwrap();
        assert_eq!(answers.message().await.unwrap(), Some(AppendResponse { log: 2, end: 5 << 20 }));

        // A take of epoch 5 fences the cell, and reads what the old writer's configuration holds.
        let fence = FenceRequest { of: of(incarnation, 1, 4), epoch: 5 };
        let fenced = client.fence(fence).await.unwrap().into_inner();
        assert_eq!(fenced.logs, [LogEnd { log: 1, end: 12 }, LogEnd { log: 2, end: 5 << 20 }]);
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

        let drop = DropLogsRequest { of: of(incarnation, 2, 5), through: None };
        client.drop_logs(drop).await.unwrap();
        let fence = FenceRequest { of: of(incarnation, 1, 4), epoch: 5 };
        assert_eq!(client.fence(fence).await.unwrap().into_inner().logs, []);
    }));
}

/// cladefs: a member refuses calls naming another incarnation (ABORTED), asked of another bucket,
/// or of a host with no replica directory (FAILED_PRECONDITION).
#[test]
fn members_refuse_other_incarnations_buckets_and_hosts_without_replicas() {
    let dir = tempfile::tempdir().unwrap();
    let (address, replicas) = member(dir.path(), true);
    let theirs = replicas.unwrap().incarnation();
    let without = tempfile::tempdir().unwrap();
    let (none, _) = member(without.path(), false);
    let runtime = crate::bucket::bucket_runtime().unwrap();
    let (mut client, mut other) =
        (super::member(&runtime, &address).unwrap(), super::member(&runtime, &none).unwrap());
    // Bounded: a member and a writer waiting on each other fail here, not the run (a member
    // answering appends only once the first came deadlocked so, TASK-537.3.2).
    runtime.block_on(within(async move {
        let fence = |of| FenceRequest { of, epoch: 5 };
        let wrong = client.fence(fence(of(theirs.wrapping_add(1), 1, 4))).await.unwrap_err();
        assert_eq!(wrong.code(), Code::Aborted, "{wrong:?}");
        let mut elsewhere = of(theirs, 1, 4);
        elsewhere.as_mut().unwrap().bucket = "memory:///other".to_owned();
        let elsewhere = client.fence(fence(elsewhere)).await.unwrap_err();
        assert_eq!(elsewhere.code(), Code::FailedPrecondition, "{elsewhere:?}");
        let asked = IncarnationRequest { bucket: URL.to_owned() };
        let no = other.incarnation(asked).await.unwrap_err();
        assert_eq!(no.code(), Code::FailedPrecondition, "{no:?}");
        assert!(no.message().contains("CLADEFSD_REPLICAS"), "{no:?}");
    }));
}

/// cladefs: a sweep drops the logs of configurations a cell's head names no more, and all of a
/// cell whose head is gone (doc-21, Sweeping stale replica logs).
#[test]
fn sweeps_drop_what_heads_name_no_more() {
    use bucket_cell::layout::{Configuration, Fsync, Member, Replication, Wish};
    let dir = tempfile::tempdir().unwrap();
    let replicas = super::replicas(dir.path()).unwrap();
    let ours = replicas.incarnation();
    let (cell, gone) = (crate::CellId { tenant: 3, id: 9 }, crate::CellId { tenant: 3, id: 10 });
    let unconfigured = crate::CellId { tenant: 3, id: 11 };
    let plain = crate::CellId { tenant: 3, id: 12 };
    for (of, number) in [(cell, 1), (cell, 2), (gone, 1), (unconfigured, 1), (plain, 1)] {
        replicas.append((of, Key { number, epoch: number }, ours), 1, 0, b"x").unwrap();
    }
    let bucket = bucket_cell::testing::Direct(bucket_cell::testing::SimBucket::new());
    let runtime = crate::bucket::bucket_runtime().unwrap();
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
            bucket_cell::head::read(&bucket, cell).await,
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
        let read = bucket_cell::head::read(&bucket, unconfigured).await;
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
