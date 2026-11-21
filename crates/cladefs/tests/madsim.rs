// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.9 (doc-24): cladefsd under madsim, built with `RUSTFLAGS="--cfg madsim"`: two daemons
//! booted as simulated hosts, each its own node with its own environment, sharing a bucket in
//! memory, their control APIs on the simulated network. A cell made on one, its commits synced, is
//! taken by the other at a later placement's token. TASK-685.5: the bucket's requests delayed and
//! failed, before or after landing, and the bucket out of reach for stretches, as the seed says
//! (`bucket_cell::faults::Seeded`), which the daemons' retries ride out. TASK-685.7: the hosts
//! peers too, replicating each other's cells over the simulated network (doc-21), cut off it a
//! while as the seed says.

#![cfg(madsim)]

use bucket_cell::object_bucket::ObjectBucket;
use cladefs::cladefsd::control::proto::{
    CreateCellRequest, ForkCellRequest, StatCellRequest, SyncCellRequest,
};
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

/// The port of the daemons' peers' service (doc-21), on each host's address.
const PEERS: u16 = 7001;

/// The address of host `host`'s daemon's peers' service.
fn peer(host: u8) -> SocketAddr {
    SocketAddr::new(control(host).ip(), PEERS)
}

/// What hosts' daemons keep, beyond their cells and their control API.
#[derive(Clone, Copy)]
struct Kept {
    /// Of how many hosts, 1 to it, all serving their peers and asking each other: every host given
    /// the same list, itself among them, so that each finds a part's owner the same (doc-21).
    peers: Option<u8>,
    /// Whether the peers replicate each other's cells (doc-21).
    replicas: bool,
    /// Whether they keep ephemeral cells (doc-25).
    ephemeral: bool,
}

impl Kept {
    /// Nothing more.
    const NONE: Self = Self { peers: None, replicas: false, ephemeral: false };

    /// Of `count` hosts, peers replicating each other's cells.
    fn replicas(count: u8) -> Self {
        Self { peers: Some(count), replicas: true, ephemeral: false }
    }

    /// Of `count` hosts, peers keeping ephemeral cells.
    fn ephemeral(count: u8) -> Self {
        Self { peers: Some(count), replicas: false, ephemeral: true }
    }
}

/// Boots host `host`'s daemon, its cell directories under `root`, its cells in `bucket`, at
/// placement token `host`, keeping what `kept` says, and serves for as long as the host runs.
async fn boot(host: u8, root: &Path, bucket: Arc<ObjectBucket>, kept: Kept) {
    // As a host's process boots: its statics anew, its node started again after a kill too.
    node_local::starting();
    // Its root on its own disk, in memory, which a kill crashes (host_fs, TASK-685.4): as a host's
    // root is, there before its daemon, durable.
    host_fs::create_dir_all_durably(root).unwrap();
    let tenants = root.join("tenants").display().to_string();
    // One call: it sets the node's whole environment.
    let mut vars = vec![
        ("CLADEFSD_BUCKET", "memory:///cells".to_owned()),
        ("CLADEFSD_TOKEN", host.to_string()),
        ("CLADEFSD_GRPC", control(host).to_string()),
        ("CLADEFSD_TENANTS", tenants),
    ];
    if let Some(count) = kept.peers {
        let peers: Vec<String> = (1..=count).map(|host| peer(host).to_string()).collect();
        vars.extend([
            ("CLADEFSD_PEER_ADDRESS", peer(host).to_string()),
            ("CLADEFSD_PEERS", peers.join(",")),
            ("CLADEFSD_PART_CACHE", root.join("cache").display().to_string()),
        ]);
    }
    if kept.replicas {
        let replicas = root.join("replicas").display().to_string();
        vars.push(("CLADEFSD_REPLICAS", replicas));
    }
    if kept.ephemeral {
        vars.push(("CLADEFSD_EPHEMERAL", root.join("ephemeral").display().to_string()));
    }
    node_local::env::set_vars(vars);
    cladefs::cladefsd::use_bucket(bucket);
    if let Err(err) = cladefs::cladefsd::boot(&root.join("vms.sock"), Some(256 << 20)).await {
        // As its supervisor starts a daemon whose boot failed again: its node, started anew.
        panic!("{BOOT_FAILED}: {err:#}");
    }
    // As cladefsd's main parks once it booted: a node whose init returns exits, its tasks with it.
    std::future::pending::<()>().await;
}

/// What a host's daemon whose boot failed says as it exits, which has its node started again, as a
/// host's supervisor starts its daemon (board 5654: a disk's fault may fail a boot).
const BOOT_FAILED: &str = "the daemon's boot failed";

/// The odds a call changing a host's disk fails at in the profiles with disk faults (board 5654).
const DISK_FAULTS: host_fs::faults::Odds = host_fs::faults::Odds { fail: 0.002 };

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

/// Where the hosts' roots are: on their own disks, simulated, each simulation's own (`host_fs`),
/// the same every run. Board 5471: a temporary directory of the real host's named them before,
/// its name drawn from the test's thread's ID, which libtest gives its threads in the order the
/// real host ran them: something outside the simulation, a seed's outcome changing with it.
const ROOTS: &str = "/hosts";

/// What every test's hosts share: their logger, their KMS's master key, and a bucket in memory,
/// its requests delayed and failed as the seed says, and what heals it.
fn world() -> (Arc<ObjectBucket>, Arc<bucket_cell::faults::Seeded>) {
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
    let backend = bucket_cell::faults::SeededBackend(seeded.clone());
    (Arc::new(ObjectBucket::new(Arc::new(backend), "")), seeded)
}

/// Hosts 1 to `count`, booted, their cell directories under [`ROOTS`]' `hostN`, their cells in
/// `bucket`: their nodes, and their roots.
fn hosts(
    count: u8,
    bucket: &Arc<ObjectBucket>,
) -> (Vec<madsim::runtime::NodeHandle>, Vec<PathBuf>) {
    hosts_keeping(count, bucket, Kept::NONE)
}

/// Hosts as [`hosts`] makes them, keeping what `kept` says.
fn hosts_keeping(
    count: u8,
    bucket: &Arc<ObjectBucket>,
    kept: Kept,
) -> (Vec<madsim::runtime::NodeHandle>, Vec<PathBuf>) {
    let handle = madsim::runtime::Handle::current();
    let roots: Vec<PathBuf> =
        (1..=count).map(|host| Path::new(ROOTS).join(format!("host{host}"))).collect();
    let nodes = (1..=count)
        .zip(&roots)
        .map(|(host, root)| {
            let (root, bucket) = (root.clone(), bucket.clone());
            handle
                .create_node()
                .name(format!("host{host}"))
                .ip(control(host).ip())
                .restart_on_panic_matching(BOOT_FAILED)
                .init(move || {
                    let (root, bucket) = (root.clone(), bucket.clone());
                    async move { boot(host, &root, bucket, kept).await }
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
    let (bucket, _) = world();
    let (_nodes, roots) = hosts(2, &bucket);
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

/// Board 5687: an image imported through the daemon's control API (`ImportImage`), a layer of a
/// directory, a file and a symlink, makes a cell of the capacity asked for, whose filesystem holds
/// the file as the layer had it, and the symlink: the image's device reading and writing its
/// cell's files in place on the simulated host's disk (host_fs), not through threads of its own.
#[madsim::test]
async fn an_image_imported_through_the_control_api_reads_back() {
    use cladefs::cladefsd::control::proto::ImportImageRequest;
    use cladefs::import::testing::{Entry, Kind, layer};
    const HOSTNAME: &[u8] = b"simulated\n";
    let (bucket, _) = world();
    let (nodes, roots) = hosts(1, &bucket);
    // The image, on the daemon's host, as its caller has it there: outside its disk (host_fs), as
    // the import reads it, on the real host's, in a directory of its own.
    let images = tempfile::tempdir().unwrap();
    let image = images.path().join("image.tar");
    let entries = [
        Entry::new("etc", Kind::Dir),
        Entry::new("etc/hostname", Kind::File(HOSTNAME)),
        Entry::new("bin/sh", Kind::Symlink("busybox")),
    ];
    std::fs::write(&image, layer(&entries).unwrap()).unwrap();
    let cell = roots[0].join("store");
    let path = cell.display().to_string();
    manager()
        .spawn(async move {
            let mut one = connect(1).await;
            let made = one.import_image(ImportImageRequest {
                path: path.clone(),
                capacity: 64 << 20,
                image: image.display().to_string(),
                ..ImportImageRequest::default()
            });
            assert_ne!(made.await.unwrap().into_inner().cell, 0);
            let stat = one.stat_cell(StatCellRequest { path });
            assert_eq!(stat.await.unwrap().into_inner().capacity, 64 << 20);
        })
        .await
        .unwrap();
    let (hostname, target) = nodes[0]
        .spawn(async move {
            let storage = cladefs::Storage::open(&cell).await.unwrap();
            let fs = cladefs::Filesystem::mount(storage).await.unwrap();
            let (etc, _) = fs.root().lookup(b"etc").await.unwrap();
            let (file, attr) = etc.lookup(b"hostname").await.unwrap();
            let mut hostname = vec![0; attr.size as usize];
            assert_eq!(file.read(0, &mut hostname).await.unwrap(), hostname.len());
            let (bin, _) = fs.root().lookup(b"bin").await.unwrap();
            let (sh, _) = bin.lookup(b"sh").await.unwrap();
            let target = sh.readlink().await.unwrap();
            drop((etc, file, bin, sh));
            fs.close().await.unwrap().close().await;
            (hostname, target)
        })
        .await
        .unwrap();
    assert_eq!(hostname, HOSTNAME);
    assert_eq!(target, b"busybox");
}

/// TASK-685.6: a simulated guest of host 1's writes a block and commits it, uploaded; once it is
/// gone and host 2 has taken its cell, a guest of host 2's reads the block as written: through the
/// daemons' own handling of a VM process's requests, its connection in memory.
#[madsim::test]
async fn a_guests_commit_uploaded_on_one_host_reads_back_on_another() {
    let (bucket, _) = world();
    let (nodes, roots) = hosts(2, &bucket);
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
            // Its first host's teardown may hold the cell's head a while yet: taken once it lets
            // go.
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

/// Blocks, by their extents' sequences.
type Blocks = std::collections::BTreeMap<u64, Vec<u8>>;

/// What attrition's writers were told.
#[derive(Default)]
struct Told {
    /// The blocks told uploaded.
    uploaded: std::sync::Mutex<Blocks>,
    /// Whether writers make versions, as the seed says, after a commit.
    versioning: bool,
    /// The versions made, each with the blocks told uploaded before it was begun: once a commit
    /// after it was told uploaded, which uploads it too, as a version is told made once committed
    /// at rest, and named in the head once uploaded (doc-20).
    versions: std::sync::Mutex<Vec<(u64, Blocks)>>,
}

/// The block a writer writes at `sequence` of round `round`.
fn block_of(round: u64, sequence: u64) -> Vec<u8> {
    (0..4096u64).map(|at| ((round * 31 + sequence * 7 + at) % 251) as u8).collect()
}

/// A writer guest's round on its host: opens the cell directory at `path`, of `cell` if given
/// (taken there already), and writes blocks from its cell's next sequence on, each committed
/// uploaded and noted in `told` once the daemon says so, `writes` of them, or until a write or
/// commit fails, as a writer fenced does; and, if `told` says writers do, makes a version after a
/// commit, as the seed says, noted there with what was told uploaded before once a commit after it
/// is told uploaded. The cell's ID; or none, if its open was refused, as a later placement's
/// taking, before it opened, has it refused (`ESTALE`).
async fn write_round(path: PathBuf, round: u64, writes: u64, told: Arc<Told>) -> Option<u64> {
    use madsim::rand::Rng as _;
    let guest = guest().await;
    let cell = match guest.open(&path, 64 << 20).await {
        Ok(cell) => cell,
        Err(errno) => {
            log::info!("round {round}: opening {path:?}: {errno:?}, a later placement's taking");
            return None;
        }
    };
    let Ok(first) = guest.next_sequence().await else { return Some(cell) };
    // Versions made, which a commit told uploaded after them uploads.
    let mut made = Vec::new();
    for sequence in first..first + writes {
        let block = block_of(round, sequence);
        let extent = cladefs::ExtentId { cell, sequence };
        if guest.write(extent, 0, &block).await.is_err() {
            break;
        }
        if guest.commit(cladefs::Commit::Uploaded).await.is_err() {
            break;
        }
        told.uploaded.lock().unwrap().insert(sequence, block);
        told.versions.lock().unwrap().append(&mut made);
        if told.versioning && madsim::rand::thread_rng().gen_bool(0.3) {
            let before = told.uploaded.lock().unwrap().clone();
            match guest.make_version().await {
                Ok(version) => made.push((version, before)),
                Err(errno) => log::info!("round {round}: a version refused: {errno:?}"),
            }
        }
    }
    // Its last versions uploaded, if its writes went on to the end.
    if !made.is_empty() && guest.commit(cladefs::Commit::Uploaded).await.is_ok() {
        told.versions.lock().unwrap().append(&mut made);
    }
    guest.close();
    Some(cell)
}

/// Takes `cell` into the cell directory at `path` on host `host`, at placement `token`, as the VM
/// manager does, trying again while the bucket refuses, as a manager would.
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
    if let Err(lost) = attrition(Profile::PLAIN).await {
        panic!("{lost}");
    }
}

/// TASK-685.7, doc-21: the attrition profile in a replicated cell, of f 1, its three hosts each
/// other's peers: a writer's commit told uploaded is on two hosts' disks, or in the bucket, and
/// each round's host takes the cell from its members, one of them killed as the seed says, the
/// bucket failing and stalling meanwhile. Every block told uploaded reads back as written.
#[madsim::test]
async fn replicated_commits_survive_attrition() {
    let profile = Profile { kept: Kept::replicas(3), ..Profile::PLAIN };
    if let Err(lost) = attrition(profile).await {
        panic!("{lost}");
    }
}

/// TASK-685.7: the replicated attrition profile, its hosts cut off the network a while too, as the
/// seed says: the writer's from its members and the VM manager, its guest writing on, or a
/// member's. Every block told uploaded reads back as written.
#[madsim::test]
async fn replicated_commits_survive_attrition_and_partitions() {
    let profile = Profile { kept: Kept::replicas(3), partitions: true, ..Profile::PLAIN };
    if let Err(lost) = attrition(profile).await {
        panic!("{lost}");
    }
}

/// TASK-685.7, doc-20: the attrition profile, its writers making versions as they go, as a VM's
/// snapshots do: once the bucket heals, each version, forked, holds every block told uploaded
/// before it was begun.
#[madsim::test]
async fn versions_made_through_attrition_hold_what_came_before() {
    if let Err(lost) = attrition(Profile { versions: true, ..Profile::PLAIN }).await {
        panic!("{lost}");
    }
}

/// Board 5654: the attrition profile, the hosts' disks failing calls too, as the seed says, each
/// with EIO or ENOSPC before it changes anything: every block told uploaded reads back as written
/// once the bucket and the disks heal.
#[madsim::test]
async fn uploaded_commits_survive_disk_faults() {
    if let Err(lost) = attrition(Profile { disk: true, ..Profile::PLAIN }).await {
        panic!("{lost}");
    }
}

/// Board 5654: the replicated attrition profile, the hosts' disks failing calls too: their
/// replica logs, fences and promises among them.
#[madsim::test]
async fn replicated_commits_survive_disk_faults() {
    let profile = Profile { kept: Kept::replicas(3), disk: true, ..Profile::PLAIN };
    if let Err(lost) = attrition(profile).await {
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
            runtime.block_on(attrition(Profile::PLAIN))
        });
        match ran.join() {
            Ok(Err(lost)) => Some((seed, lost)),
            Ok(Ok(())) | Err(_) => None,
        }
    });
    let found = found.expect("no seed of 40 lost a block told uploaded, head swaps unconditional");
    eprintln!("seed {} lost: {}", found.0, found.1);
}

/// Makes a cell replicated at f 1 (doc-21) in the cell directory at `path` on host `host`, at
/// placement `token`, trying again while the bucket refuses: its ID.
async fn create_replicated(host: u8, path: &Path, token: u64) -> u64 {
    let mut daemon = connect(host).await;
    let creating = CreateCellRequest {
        path: path.display().to_string(),
        capacity: 64 << 20,
        token: Some(token),
        replicas: Some(1),
        ..CreateCellRequest::default()
    };
    for _ in 0..200 {
        match daemon.create_cell(creating.clone()).await {
            Ok(made) => return made.into_inner().cell,
            Err(status) => {
                eprintln!("host {host} making a cell: {:?} {}", status.code(), status.message());
                madsim::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    panic!("host {host} never made a replicated cell");
}

/// What an attrition run does beside its kills and the bucket's faults.
#[derive(Clone, Copy)]
struct Profile {
    /// What its hosts keep: its cell replicated if they replicate.
    kept: Kept,
    /// Whether a host is cut off the network a while, as the seed says, at each round.
    partitions: bool,
    /// Whether writers make versions, each forked once the bucket heals, and read there.
    versions: bool,
    /// Whether the hosts' disks fail calls, as the seed says, once the cell is made, until the
    /// bucket heals ([`DISK_FAULTS`], board 5654).
    disk: bool,
}

impl Profile {
    /// Hosts keeping nothing more, never cut off, making no versions.
    const PLAIN: Self = Self { kept: Kept::NONE, partitions: false, versions: false, disk: false };
}

/// Reads the blocks of `told`, `cell`'s, in the cell directory at `path` on this host, which a
/// guest opens: those lost, or read other than they were written, by their sequences.
async fn lost_in(path: PathBuf, cell: u64, told: Blocks) -> Vec<u64> {
    let guest = guest().await;
    guest.open(&path, 64 << 20).await.unwrap();
    let mut lost = Vec::new();
    for (&sequence, block) in &told {
        let extent = cladefs::ExtentId { cell, sequence };
        if guest.read(extent, 0, 4096).await.as_ref().ok() != Some(block) {
            lost.push(sequence);
        }
    }
    guest.close();
    lost
}

/// Forks version `version` of `cell` into the cell directory at `path` on host `host`, as the VM
/// manager does, trying again while the bucket refuses.
async fn fork_version(host: u8, path: &Path, cell: u64, version: u64) {
    let mut daemon = connect(host).await;
    let forking = ForkCellRequest {
        to: path.display().to_string(),
        version,
        cell,
        ..ForkCellRequest::default()
    };
    for _ in 0..200 {
        match daemon.fork_cell(forking.clone()).await {
            Ok(_) => return,
            Err(status) => {
                eprintln!(
                    "host {host} forking version {version}: {:?} {}",
                    status.code(),
                    status.message()
                );
                madsim::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    panic!("host {host} never forked version {version} of cell {cell}");
}

/// The attrition profile, as [`uploaded_commits_survive_attrition`] says, as `profile` says: what
/// was lost, if anything.
async fn attrition(Profile { kept, partitions, versions, disk }: Profile) -> Result<(), String> {
    use madsim::rand::Rng as _;
    const ROUNDS: u64 = 5;
    const WRITES: u64 = 6;
    let (bucket, faults) = world();
    let (nodes, roots) = hosts_keeping(3, &bucket, kept);
    let handle = madsim::runtime::Handle::current();
    let told = Arc::new(Told { versioning: versions, ..Told::default() });
    let manager = manager();
    let path = |round: u64, host: usize| roots[host].join(format!("store{round}"));
    let mut cell = None;
    // A replicated cell, made by the manager as a guest's open makes none: the first round's
    // writer opens it.
    let mut made = false;
    if kept.replicas {
        let at = path(0, 0);
        let making = async move { create_replicated(1, &at, 10).await };
        cell = Some(manager.spawn(making).await.unwrap());
        made = true;
    }
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
    let net = madsim::net::NetSim::current();
    for round in 0..ROUNDS {
        restart(killed.take()).await;
        // The disks failing once the cell is made: its making is not what the profile tries.
        if disk && cell.is_some() {
            host_fs::faults::set(DISK_FAULTS);
        }
        // As the seed says: a host cut off the network a while, its links' packets lost.
        if partitions && madsim::rand::thread_rng().gen_bool(0.5) {
            let cut = nodes[madsim::rand::thread_rng().gen_range(0..nodes.len())].id();
            let after = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..4000));
            net.clog_node(cut);
            let net = net.clone();
            drop(madsim::task::spawn(async move {
                madsim::time::sleep(after).await;
                net.unclog_node(cut);
            }));
        }
        let host = (round % 3) as usize;
        let at = path(round, host);
        if let Some(cell) = cell
            && !std::mem::take(&mut made)
        {
            let at = at.clone();
            let taking = async move { take(host as u8 + 1, &at, cell, round + 10).await };
            manager.spawn(taking).await.unwrap();
        }
        let writing = write_round(at, round, WRITES, told.clone());
        let mut written = node(host).spawn(writing);
        // As the seed says: the writer done; or the next round taking the cell as it writes; or
        // its host killed as it writes, started again a while after.
        let wait = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..3000));
        match madsim::time::timeout(wait, &mut written).await {
            Ok(done) => cell = cell.or(done.unwrap()),
            Err(_) => {
                if cell.is_none() {
                    cell = written.await.unwrap();
                    assert!(cell.is_some(), "the first round's writer opened no cell");
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
    host_fs::faults::set(host_fs::faults::Odds::NONE);
    for node in &nodes {
        net.unclog_node(node.id());
    }
    let last = path(ROUNDS, 0);
    let at = last.clone();
    manager.spawn(async move { take(1, &at, cell, ROUNDS + 10).await }).await.unwrap();
    let uploaded = told.uploaded.lock().unwrap().clone();
    assert!(!uploaded.is_empty(), "no write was told uploaded");
    let sequences: Vec<u64> = uploaded.keys().copied().collect();
    let lost = node(0).spawn(lost_in(last, cell, uploaded)).await.unwrap();
    if !lost.is_empty() {
        return Err(format!("blocks told uploaded, lost or other: {lost:?} of {sequences:?}"));
    }
    // Each version, forked on host 1, holds what was told uploaded before it was begun.
    let made = told.versions.lock().unwrap().clone();
    if versions {
        eprintln!("versions made, each forked and read: {}", made.len());
    }
    for (at, (version, before)) in made.into_iter().enumerate() {
        let fork = roots[0].join(format!("fork{at}"));
        let to = fork.clone();
        manager.spawn(async move { fork_version(1, &to, cell, version).await }).await.unwrap();
        let sequences: Vec<u64> = before.keys().copied().collect();
        let lost = node(0).spawn(lost_in(fork, cell, before)).await.unwrap();
        if !lost.is_empty() {
            return Err(format!(
                "version {version}'s fork lost blocks told uploaded before it: {lost:?} of \
                 {sequences:?}"
            ));
        }
    }
    Ok(())
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
    if let Err(lost) = ephemeral_attrition(false).await {
        panic!("{lost}");
    }
}

/// Board 5654: the ephemeral profile, the hosts' disks failing calls too, as the seed says, once
/// the cell is forked: its host journal, its migrations' staging, notes and markers, its own
/// checkpoints among them.
#[madsim::test]
async fn ephemeral_cells_survive_disk_faults() {
    if let Err(lost) = ephemeral_attrition(true).await {
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
                Some(errno) if !matches!(errno, cladefs::Errno::ENOTCONN | cladefs::Errno::EIO) => {
                    return Err(errno);
                }
                errno => {
                    log::info!("host {host}: {errno:?} {}", status.message());
                    madsim::time::sleep(Duration::from_millis(200)).await;
                }
            },
        }
    }
    panic!("host {host} never answered")
}

/// The ephemeral attrition profile, as [`ephemeral_cells_survive_crashes_and_migrations`] says,
/// the hosts' disks failing calls once the cell is forked if `disk`: what was lost, if anything.
async fn ephemeral_attrition(disk: bool) -> Result<(), String> {
    use cladefs::cladefsd::control::proto::{HandOverCellRequest, MigrateCellRequest};
    use madsim::rand::Rng as _;
    const ROUNDS: u64 = 6;
    const WRITES: u64 = 5;
    let (bucket, faults) = world();
    let (nodes, roots) = hosts_keeping(3, &bucket, Kept::ephemeral(3));
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
    let base = roots[0].join("base").display().to_string();
    let forked = manager.spawn(async move {
        // The version uploaded first, which a fork of it reads in the bucket.
        let syncing = || {
            let path = base.clone();
            async move { connect(1).await.sync_cell(SyncCellRequest { path }).await }
        };
        calling(1, syncing).await.unwrap();
        let stating = || {
            let path = base.clone();
            async move { connect(1).await.stat_cell(StatCellRequest { path }).await }
        };
        let stated = calling(1, stating).await.unwrap().into_inner();
        assert_eq!(stated.cell, parent);
        calling(1, || {
            let request = ForkCellRequest {
                to: to.clone(),
                version,
                tenant: stated.tenant,
                cell: stated.cell,
                ephemeral: true,
                ..ForkCellRequest::default()
            };
            async move { connect(1).await.fork_cell(request).await }
        })
        .await
    });
    let child = forked.await.unwrap().map_err(|errno| format!("forking: {errno:?}"))?;
    let child = child.into_inner().cell;

    let told = Arc::new(Told::default());
    if disk {
        host_fs::faults::set(DISK_FAULTS);
    }
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
    host_fs::faults::set(host_fs::faults::Odds::NONE);
    let expected = told.uploaded.lock().unwrap().clone();
    let reading = at.clone();
    let read = node(host)
        .spawn(async move {
            let guest = guest().await;
            let cell = guest.open(&reading, 64 << 20).await.unwrap();
            let inherited = cladefs::ExtentId { cell: parent, sequence: 1 };
            let inherited = guest.read(inherited, 0, 4096).await;
            let mut read = Vec::new();
            for &sequence in expected.keys() {
                let extent = cladefs::ExtentId { cell, sequence };
                read.push((sequence, guest.read(extent, 0, 4096).await));
            }
            (cell, inherited, read)
        })
        .await
        .unwrap();
    let (cell, inherited, read) = read;
    assert_eq!(cell, child, "the cell read is the ephemeral one");
    if inherited.ok() != Some(block_of(99, 1)) {
        return Err("the block the cell inherits read otherwise".to_owned());
    }
    let told = told.uploaded.lock().unwrap().clone();
    assert!(!told.is_empty(), "no write was told committed");
    let lost: Vec<u64> = (read.into_iter())
        .filter(|(sequence, block)| block.as_ref().ok() != told.get(sequence))
        .map(|(sequence, _)| sequence)
        .collect();
    // One journal entry of the cell's, its holder's: on each host's own disk, read there.
    let mut noted = Vec::new();
    for each in 0..3 {
        let cells = roots[each].join("ephemeral/cells");
        let listed =
            async move { host_fs::read_dir(cells).is_ok_and(|mut found| found.next().is_some()) };
        if node(each).spawn(listed).await.unwrap() {
            noted.push(each);
        }
    }
    match (lost.is_empty(), noted == vec![host]) {
        (true, true) => Ok(()),
        (false, _) => {
            Err(format!("blocks told committed, lost or other: {lost:?} of {:?}", told.keys()))
        }
        (true, false) => Err(format!("the cell noted on hosts {noted:?}, held on {host}")),
    }
}
