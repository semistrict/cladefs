// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.9 (doc-24): cladefsd under madsim, built with `RUSTFLAGS="--cfg madsim"`: two daemons
//! booted as simulated hosts, each its own node with its own environment, sharing a bucket in
//! memory, their control APIs on the simulated network. A cell made on one, its commits synced, is
//! taken by the other at a later placement's token. TASK-685.5: the bucket's requests delayed and
//! failed, before or after landing, and the bucket out of reach for stretches, as the seed says
//! (`bucket_cell::faults::Seeded`), which the daemons' retries ride out.

#![cfg(all(madsim, target_os = "linux"))]

use bucket_cell::object_bucket::ObjectBucket;
use cladefs::cladefsd::control::proto::{CreateCellRequest, StatCellRequest, SyncCellRequest};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The control API's port, on each host's address.
const CONTROL: u16 = 7000;

/// The address of host `host`'s daemon's control API.
fn control(host: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)), CONTROL)
}

/// The peer port, on each host's address, of the hosts that keep ephemeral cells.
const PEER: u16 = 7100;

/// The address of host `host`'s daemon's peer server.
fn peer(host: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)), PEER)
}

/// Boots host `host`'s daemon, its stores under `root`, its cells in `bucket`, at placement token
/// `host`, and serves for as long as the host runs. With `ephemeral`, of `ephemeral` hosts, it keeps
/// ephemeral cells (doc-25), and serves and asks its peers, the others of them.
async fn boot(host: u8, root: &Path, bucket: Arc<ObjectBucket>, ephemeral: Option<u8>) {
    // As a host's process boots: its statics anew, its node started again after a kill too.
    node_local::starting();
    let tenants = root.join("tenants").display().to_string();
    node_local::env::set_vars([
        ("CLADEFSD_BUCKET", "memory:///cells".to_owned()),
        ("CLADEFSD_TOKEN", host.to_string()),
        ("CLADEFSD_GRPC", control(host).to_string()),
        ("CLADEFSD_TENANTS", tenants),
    ]);
    if let Some(hosts) = ephemeral {
        let others =
            (1..=hosts).filter(|&other| other != host).map(|other| peer(other).to_string());
        let peers: Vec<String> = std::iter::once(peer(host).to_string()).chain(others).collect();
        node_local::env::set_vars([
            ("CLADEFSD_EPHEMERAL", root.join("ephemeral").display().to_string()),
            ("CLADEFSD_PEER_ADDRESS", peer(host).to_string()),
            ("CLADEFSD_PEERS", peers.join(",")),
        ]);
    }
    cladefs::cladefsd::use_bucket(bucket);
    cladefs::cladefsd::boot(&root.join("vms.sock"), Some(256 << 20)).await.unwrap();
    // As cladefsd's main parks once it booted: a node whose init returns exits, its tasks with it.
    std::future::pending::<()>().await;
}

/// The daemon at `host`'s control API, once it answers.
async fn connect(
    host: u8,
) -> cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient<tonic::transport::Channel> {
    let mut failed = None;
    for _ in 0..50 {
        match cladefs::cladefsd::control::connect(control(host)).await {
            Ok(client) => return client,
            Err(err) => failed = Some(err),
        }
        madsim::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("host {host}'s control API never answered: {failed:?}");
}

/// What every test's hosts share: their logger, their KMS's master key, and a bucket in memory,
/// its requests delayed and failed as the seed says, and what heals it.
fn world() -> (tempfile::TempDir, Arc<ObjectBucket>, Arc<bucket_cell::faults::Seeded>) {
    // Once a process: a determinism check runs the test twice in it.
    let _ = cladefs::cladefsd::logging::to_stderr();
    // SAFETY: the simulation's one thread, before any node reads the environment.
    unsafe { std::env::set_var("CLADEFS_MASTER_KEY", "ab".repeat(32)) };
    let odds = bucket_cell::faults::Odds {
        delay_most: Duration::from_millis(50),
        fail_before: 0.05,
        fail_after: 0.05,
        out_of_reach: 0.01,
        out_for: Duration::from_millis(500),
    };
    let objects = Arc::new(object_store::memory::InMemory::new());
    let seeded = Arc::new(bucket_cell::faults::Seeded::new(objects, madsim::rand::random(), odds));
    let store = bucket_cell::faults::SeededStore(seeded.clone());
    (tempfile::tempdir().unwrap(), Arc::new(ObjectBucket::new(Arc::new(store), "")), seeded)
}

/// Hosts 1 to `count`, booted, their stores under `dir`'s `hostN`, their cells in `bucket`: their
/// nodes, and their roots.
fn hosts(
    count: u8,
    dir: &Path,
    bucket: &Arc<ObjectBucket>,
) -> (Vec<madsim::runtime::NodeHandle>, Vec<PathBuf>) {
    hosts_keeping(count, dir, bucket, false)
}

/// Hosts as [`hosts`] boots them, keeping ephemeral cells and peers of each other, if `ephemeral`.
fn hosts_keeping(
    count: u8,
    dir: &Path,
    bucket: &Arc<ObjectBucket>,
    ephemeral: bool,
) -> (Vec<madsim::runtime::NodeHandle>, Vec<PathBuf>) {
    let handle = madsim::runtime::Handle::current();
    let roots: Vec<PathBuf> = (1..=count).map(|host| dir.join(format!("host{host}"))).collect();
    let nodes = (1..=count)
        .zip(&roots)
        .map(|(host, root)| {
            std::fs::create_dir_all(root).unwrap();
            let (root, bucket) = (root.clone(), bucket.clone());
            handle
                .create_node()
                .name(format!("host{host}"))
                .ip(control(host).ip())
                .init(move || {
                    let (root, bucket) = (root.clone(), bucket.clone());
                    async move { boot(host, &root, bucket, ephemeral.then_some(count)).await }
                })
                .build()
        })
        .collect();
    (nodes, roots)
}

/// A guest of this host's daemon, once it serves.
async fn guest() -> cladefs::cladefsd::guest::Guest {
    while !cladefs::cladefsd::serving() {
        madsim::time::sleep(Duration::from_millis(100)).await;
    }
    cladefs::cladefsd::guest::Guest::connect().unwrap()
}

/// The VM manager's node, which calls the hosts' control APIs.
fn manager() -> madsim::runtime::NodeHandle {
    let handle = madsim::runtime::Handle::current();
    handle.create_node().name("vm manager").ip("10.0.0.100".parse().unwrap()).build()
}

#[madsim::test]
async fn two_daemons_boot_as_hosts_and_one_takes_the_others_cell() {
    let (dir, bucket, _) = world();
    let (_nodes, roots) = hosts(2, dir.path(), &bucket);
    let client = manager();
    let (first, second) = (roots[0].join("store"), roots[1].join("store"));
    client
        .spawn(async move {
            let mut one = connect(1).await;
            let made = one
                .create_cell(CreateCellRequest {
                    path: first.display().to_string(),
                    capacity: 64 << 20,
                    ..CreateCellRequest::default()
                })
                .await
                .unwrap()
                .into_inner();
            let path = first.display().to_string();
            one.sync_cell(SyncCellRequest { path: path.clone() }).await.unwrap();
            let mut two = connect(2).await;
            let taken = two
                .create_cell(CreateCellRequest {
                    path: second.display().to_string(),
                    capacity: 64 << 20,
                    token: Some(2),
                    tenant: made.tenant,
                    cell: made.cell,
                    ..CreateCellRequest::default()
                })
                .await
                .unwrap()
                .into_inner();
            assert_eq!((taken.tenant, taken.cell), (made.tenant, made.cell));
            let stat = two.stat_cell(StatCellRequest { path: second.display().to_string() });
            assert_eq!(stat.await.unwrap().into_inner().capacity, 64 << 20);
        })
        .await
        .unwrap();
}

/// TASK-685.6: a simulated guest of host 1's writes a block and commits it, uploaded; once it is
/// gone and host 2 has taken its cell, a guest of host 2's reads the block as written: through the
/// daemons' own handling of a VM process's requests, its connection in memory.
#[madsim::test]
async fn a_guests_commit_uploaded_on_one_host_reads_back_on_another() {
    let (dir, bucket, _) = world();
    let (nodes, roots) = hosts(2, dir.path(), &bucket);
    let (first, second) = (roots[0].join("store"), roots[1].join("store"));
    let block: Vec<u8> = (0..4096u32).map(|at| (at * 7 % 251) as u8).collect();
    let written = block.clone();
    let cell = nodes[0]
        .spawn(async move {
            let guest = guest().await;
            let cell = guest.open(&first, 64 << 20).await.unwrap();
            let extent = cladefs::ExtentId { cell, sequence: 1 };
            guest.write(extent, 0, &written).await.unwrap();
            guest.commit(cladefs::Commit::Uploaded).await.unwrap();
            guest.close();
            cell
        })
        .await
        .unwrap();
    manager()
        .spawn(async move {
            let mut two = connect(2).await;
            let taking = CreateCellRequest {
                path: second.display().to_string(),
                capacity: 64 << 20,
                token: Some(2),
                cell,
                ..CreateCellRequest::default()
            };
            // Its first host's teardown may hold the cell's head a while yet: taken once it lets go.
            loop {
                match two.create_cell(taking.clone()).await {
                    Ok(_) => break,
                    Err(status) => {
                        log::info!("taking the cell: {status:?}");
                        madsim::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        })
        .await
        .unwrap();
    let second = roots[1].join("store");
    let read = nodes[1]
        .spawn(async move {
            let guest = guest().await;
            assert_eq!(guest.open(&second, 64 << 20).await.unwrap(), cell);
            let extent = cladefs::ExtentId { cell, sequence: 1 };
            guest.read(extent, 0, 4096).await.unwrap()
        })
        .await
        .unwrap();
    assert!(read == block, "host 2 read another block than host 1's guest committed");
}

/// What a writer guest was told uploaded, by extent sequence: the block it wrote there.
type Uploaded = Arc<std::sync::Mutex<std::collections::BTreeMap<u64, Vec<u8>>>>;

/// The block a writer writes at `sequence` of round `round`.
fn block_of(round: u64, sequence: u64) -> Vec<u8> {
    (0..4096u64).map(|at| ((round * 31 + sequence * 7 + at) % 251) as u8).collect()
}

/// A writer guest's round on its host: opens the store at `path`, of `cell` if given (taken there
/// already), and writes blocks from its store's next sequence on, each committed uploaded and
/// noted in `uploaded` once the daemon says so, `writes` of them, or until a write or commit
/// fails, as a writer fenced does. The cell's ID; or none, if its open was refused, as a later
/// placement's taking, before it opened, has it refused (`ESTALE`).
async fn write_round(path: PathBuf, round: u64, writes: u64, uploaded: Uploaded) -> Option<u64> {
    let guest = guest().await;
    let cell = match guest.open(&path, 64 << 20).await {
        Ok(cell) => cell,
        Err(errno) => {
            log::info!("round {round}: opening {path:?}: {errno:?}, a later placement's taking");
            return None;
        }
    };
    let Ok(first) = guest.next_sequence().await else { return Some(cell) };
    for sequence in first..first + writes {
        let block = block_of(round, sequence);
        let extent = cladefs::ExtentId { cell, sequence };
        if guest.write(extent, 0, &block).await.is_err() {
            break;
        }
        if guest.commit(cladefs::Commit::Uploaded).await.is_err() {
            break;
        }
        uploaded.lock().unwrap().insert(sequence, block);
    }
    guest.close();
    Some(cell)
}

/// Takes `cell` into the store at `path` on host `host`, at placement `token`, as the VM manager
/// does, trying again while the bucket refuses, as a manager would.
async fn take(host: u8, path: &Path, cell: u64, token: u64) {
    let mut daemon = connect(host).await;
    let taking = CreateCellRequest {
        path: path.display().to_string(),
        capacity: 64 << 20,
        token: Some(token),
        cell,
        ..CreateCellRequest::default()
    };
    for _ in 0..200 {
        match daemon.create_cell(taking.clone()).await {
            Ok(_) => return,
            Err(status) => {
                eprintln!(
                    "host {host} taking cell {cell}: {:?} {}",
                    status.code(),
                    status.message()
                );
                madsim::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    panic!("host {host} never took cell {cell}");
}

/// TASK-685.6, doc-24: attrition, FoundationDB's: a writer guest on one of three hosts writes and
/// commits uploaded, its rounds each on the next host, which takes the cell over at a later
/// placement, as the seed says: once the writer is done, or while it writes still, its host then
/// a zombie writer the bucket fences; or once its host was killed, started again after. The
/// bucket's requests fail and stall as the seed says. Then the bucket heals, a last host takes the
/// cell, and every block a writer was told uploaded reads there as it wrote it (doc-12's row: what
/// was Uploaded survives the loss of every host).
#[madsim::test]
async fn uploaded_commits_survive_attrition() {
    if let Err(lost) = attrition().await {
        panic!("{lost}");
    }
}

/// TASK-685.6: the attrition profile finds the protocol's rules needed: with a writer's head swap
/// unconditional (`bucket_cell::Rules::head_swap` off), a zombie writer writes the head over its
/// successor's, whose blocks told uploaded the head then names not, and a seed loses one. Each seed
/// on a thread of its own, as madsim runs them, whose node-local values are its own.
#[test]
fn attrition_needs_head_swaps() {
    let found = (1..=40u64).find_map(|seed| {
        let ran = std::thread::spawn(move || {
            let rules = bucket_cell::Rules { head_swap: false, ..bucket_cell::Rules::ALL };
            bucket_cell::set_rules(rules);
            let runtime = madsim::runtime::Runtime::with_seed_and_config(seed, Default::default());
            runtime.block_on(attrition())
        });
        match ran.join() {
            Ok(Err(lost)) => Some((seed, lost)),
            Ok(Ok(())) | Err(_) => None,
        }
    });
    let found = found.expect("no seed of 40 lost a block told uploaded, head swaps unconditional");
    eprintln!("seed {} lost: {}", found.0, found.1);
}

/// The attrition profile, as [`uploaded_commits_survive_attrition`] says: what was lost, if
/// anything.
async fn attrition() -> Result<(), String> {
    use madsim::rand::Rng as _;
    const ROUNDS: u64 = 5;
    const WRITES: u64 = 6;
    let (dir, bucket, faults) = world();
    let (nodes, roots) = hosts(3, dir.path(), &bucket);
    let handle = madsim::runtime::Handle::current();
    let uploaded: Uploaded = Arc::default();
    let manager = manager();
    let path = |round: u64, host: usize| roots[host].join(format!("store{round}"));
    let mut cell = None;
    // A host's node as it is now: one started again is a node anew, which a handle of the one
    // killed spawns nothing on.
    let node = |host: usize| handle.get_node(nodes[host].id()).unwrap();
    // A host killed, started again before the next round, a while after, as the seed says.
    let mut killed: Option<madsim::runtime::NodeHandle> = None;
    let restart = |killed: Option<madsim::runtime::NodeHandle>| {
        let handle = handle.clone();
        async move {
            let Some(node) = killed else { return };
            let after = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..5000));
            madsim::time::sleep(after).await;
            handle.restart(node.id());
        }
    };
    for round in 0..ROUNDS {
        restart(killed.take()).await;
        let host = (round % 3) as usize;
        let at = path(round, host);
        if let Some(cell) = cell {
            let at = at.clone();
            let taking = async move { take(host as u8 + 1, &at, cell, round + 10).await };
            manager.spawn(taking).await.unwrap();
        }
        let writing = write_round(at, round, WRITES, uploaded.clone());
        let mut written = node(host).spawn(writing);
        // As the seed says: the writer done; or the next round taking the cell as it writes; or
        // its host killed as it writes, started again a while after.
        let wait = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..3000));
        match madsim::time::timeout(wait, &mut written).await {
            Ok(done) => cell = cell.or(done.unwrap()),
            Err(_) => {
                if cell.is_none() {
                    cell = written.await.unwrap();
                    assert!(cell.is_some(), "the first round's writer opened no store");
                } else if madsim::rand::thread_rng().gen_bool(0.5) {
                    handle.kill(nodes[host].id());
                    killed = Some(nodes[host].clone());
                }
            }
        }
    }
    let cell = cell.unwrap();
    restart(killed.take()).await;
    // Healed: the last host takes the cell, its every block told uploaded there as written.
    faults.set(bucket_cell::faults::Odds::NONE);
    let last = path(ROUNDS, 0);
    let at = last.clone();
    manager.spawn(async move { take(1, &at, cell, ROUNDS + 10).await }).await.unwrap();
    let told = uploaded.lock().unwrap().clone();
    let read = node(0)
        .spawn(async move {
            let guest = guest().await;
            assert_eq!(guest.open(&last, 64 << 20).await.unwrap(), cell);
            let mut read = Vec::new();
            for &sequence in told.keys() {
                let extent = cladefs::ExtentId { cell, sequence };
                read.push((sequence, guest.read(extent, 0, 4096).await));
            }
            read
        })
        .await
        .unwrap();
    let told = uploaded.lock().unwrap().clone();
    assert!(!told.is_empty(), "no write was told uploaded");
    let lost: Vec<u64> = read
        .into_iter()
        .filter(|(sequence, block)| block.as_ref().ok() != told.get(sequence))
        .map(|(sequence, _)| sequence)
        .collect();
    match lost.is_empty() {
        true => Ok(()),
        false => Err(format!("blocks told uploaded, lost or other: {lost:?} of {:?}", told.keys())),
    }
}

/// TASK-698.5, doc-25: an ephemeral cell forked from a version on one of three hosts, its guest
/// writing and committing in rounds, each commit Local; between rounds, as the seed says, its host
/// killed as it writes, started again after, or the cell migrated live to the next host, that host
/// itself killed before the hand-over now and then, the hand-over refused then and the cell kept
/// where it was. The bucket's requests fail and stall meanwhile. Then the bucket heals, and every
/// block the guest was told committed reads as written where the cell is, one reference to its
/// origin left, its host's alone (doc-12's ephemeral rows).
#[madsim::test]
async fn ephemeral_cells_survive_crashes_and_migrations() {
    if let Err(lost) = ephemeral_attrition().await {
        panic!("{lost}");
    }
}

/// Calls `call` on host `host`'s control API until it answers otherwise than the bucket or a peer
/// being out of reach: what it answered.
async fn calling<T, F>(host: u8, call: impl Fn() -> F) -> Result<T, cladefs::Errno>
where
    F: std::future::Future<Output = Result<T, tonic::Status>>,
{
    for _ in 0..100 {
        match call().await {
            Ok(done) => return Ok(done),
            Err(status) => match cladefs::cladefsd::control::errno(&status) {
                Some(errno @ (cladefs::Errno::ENOTCONN | cladefs::Errno::EIO)) | None => {
                    log::info!("host {host}: {errno:?} {}", status.message());
                    madsim::time::sleep(Duration::from_millis(200)).await;
                }
                Some(errno) => return Err(errno),
            },
        }
    }
    panic!("host {host} never answered")
}

/// The ephemeral attrition profile, as [`ephemeral_cells_survive_crashes_and_migrations`] says:
/// what was lost, if anything.
async fn ephemeral_attrition() -> Result<(), String> {
    use cladefs::cladefsd::control::proto::{
        ForkCellRequest, HandOverCellRequest, MigrateCellRequest,
    };
    use madsim::rand::Rng as _;
    const ROUNDS: u64 = 6;
    const WRITES: u64 = 5;
    let (dir, bucket, faults) = world();
    let (nodes, roots) = hosts_keeping(3, dir.path(), &bucket, true);
    let handle = madsim::runtime::Handle::current();
    let manager = manager();
    let node = |host: usize| handle.get_node(nodes[host].id()).unwrap();
    let restart = |host: usize| {
        let handle = handle.clone();
        let id = nodes[host].id();
        async move {
            let after = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..3000));
            madsim::time::sleep(after).await;
            handle.restart(id);
        }
    };

    // The version on host 1, of a base whose block the ephemeral cell inherits.
    let base = roots[0].join("base");
    let (parent, version) = node(0)
        .spawn(async move {
            let guest = guest().await;
            let cell = guest.open(&base, 64 << 20).await.unwrap();
            let extent = cladefs::ExtentId { cell, sequence: 1 };
            guest.write(extent, 0, &block_of(99, 1)).await.unwrap();
            guest.commit(cladefs::Commit::Uploaded).await.unwrap();
            let version = guest.make_version().await.unwrap();
            guest.close();
            (cell, version)
        })
        .await
        .unwrap();
    let path = |host: usize, round: u64| roots[host].join(format!("ephemeral{round}"));
    let (mut host, mut at) = (0usize, path(0, 0));
    let to = at.display().to_string();
    let forked = manager.spawn(async move {
        calling(1, || {
            let request = ForkCellRequest {
                to: to.clone(),
                version,
                cell: parent,
                ephemeral: true,
                ..ForkCellRequest::default()
            };
            async move { connect(1).await.fork_cell(request).await }
        })
        .await
    });
    let child = forked.await.unwrap().map_err(|errno| format!("forking: {errno:?}"))?;
    let child = child.into_inner().cell;

    let told: Uploaded = Arc::default();
    for round in 0..ROUNDS {
        let writing = write_round(at.clone(), round, WRITES, told.clone());
        let mut written = node(host).spawn(writing);
        let wait = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..2000));
        if madsim::time::timeout(wait, &mut written).await.is_err() {
            // Killed as it writes: what it was told committed is Local, on the disk kept.
            if madsim::rand::thread_rng().gen_bool(0.5) {
                handle.kill(nodes[host].id());
                restart(host).await;
            } else {
                written.await.unwrap();
            }
        }
        if !madsim::rand::thread_rng().gen_bool(0.6) {
            continue;
        }
        // Migrated live to the next host, that host killed before the hand-over now and then.
        let target = (host + 1) % 3;
        let (from, there) = (at.display().to_string(), path(target, round + 1));
        let to = there.display().to_string();
        let source = host as u8 + 1;
        let migrating = manager.spawn(async move {
            let request =
                MigrateCellRequest { path: from, peer: peer(source % 3 + 1).to_string(), to };
            calling(source, || {
                let request = request.clone();
                async move { connect(source).await.migrate_cell(request).await }
            })
            .await
        });
        if migrating.await.unwrap().is_err() {
            continue;
        }
        madsim::time::sleep(Duration::from_millis(madsim::rand::thread_rng().gen_range(0..1000)))
            .await;
        if madsim::rand::thread_rng().gen_bool(0.3) {
            handle.kill(nodes[target].id());
            restart(target).await;
        }
        let from = at.display().to_string();
        let handing = manager.spawn(async move {
            calling(source, || {
                let request = HandOverCellRequest { path: from.clone() };
                async move { connect(source).await.hand_over_cell(request).await }
            })
            .await
        });
        match handing.await.unwrap() {
            Ok(_) => (host, at) = (target, there),
            Err(cladefs::Errno::EAGAIN) => log::info!("round {round}: the hand-over refused"),
            Err(errno) => return Err(format!("round {round}: handing over: {errno:?}")),
        }
    }

    faults.set(bucket_cell::faults::Odds::NONE);
    let expected = told.lock().unwrap().clone();
    let reading = at.clone();
    let read = node(host)
        .spawn(async move {
            let guest = guest().await;
            let cell = guest.open(&reading, 64 << 20).await.unwrap();
            let inherited = cladefs::ExtentId { cell: parent, sequence: 1 };
            let mut read = vec![(0, guest.read(inherited, 0, 4096).await)];
            for &sequence in expected.keys() {
                read.push((
                    sequence,
                    guest.read(cladefs::ExtentId { cell, sequence }, 0, 4096).await,
                ));
            }
            (cell, read)
        })
        .await
        .unwrap();
    let (cell, read) = read;
    assert_eq!(cell, child, "the cell read is the ephemeral one");
    let told = told.lock().unwrap().clone();
    assert!(!told.is_empty(), "no write was told committed");
    let mut lost = Vec::new();
    for (sequence, block) in read {
        let expected = match sequence {
            0 => Some(block_of(99, 1)),
            sequence => told.get(&sequence).cloned(),
        };
        if block.ok() != expected {
            lost.push(sequence);
        }
    }
    // One journal entry of the cell's, its holder's.
    let noted: Vec<usize> = (0..3)
        .filter(|&each| {
            let cells = roots[each].join("ephemeral/cells");
            std::fs::read_dir(cells).is_ok_and(|mut found| found.next().is_some())
        })
        .collect();
    match (lost.is_empty(), noted == vec![host]) {
        (true, true) => Ok(()),
        (false, _) => {
            Err(format!("blocks told committed, lost or other: {lost:?} of {:?}", told.keys()))
        }
        (true, false) => Err(format!("the cell noted on hosts {noted:?}, held on {host}")),
    }
}
