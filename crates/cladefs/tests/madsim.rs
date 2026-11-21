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
//! while as the seed says. TASK-685.7: fault profiles, each a family of FoundationDB's (partitions
//! one way, both and shifting, loss, forced takes, peers changed, drains, frees and the collector,
//! bases forked and deleted, memory and disk pressure), a swarm of them per seed, and a search per
//! rule of the protocol's and the open cells' that the profiles reach, which finds it needed.

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
#[derive(Clone, Copy, Debug)]
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

    /// Of `count` hosts, peers serving each other parts of the bucket's objects (TASK-406.2).
    const fn peers(count: u8) -> Self {
        Self { peers: Some(count), replicas: false, ephemeral: false }
    }

    /// Of `count` hosts, peers replicating each other's cells.
    const fn replicas(count: u8) -> Self {
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
    // As a host's process boots: its statics anew, its node started again by madsim itself too, as
    // after a boot failing (`BOOT_FAILED`), which `node_local::restart` doesn't start.
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

/// How long a host's control API may not answer, its daemon started again: madsim starts a node
/// whose boot failed ([`BOOT_FAILED`]) again 1 to 10 s after (`restart_on_panic_matching`), and
/// a boot of the next may fail too, as a disk's fault may fail any (board 5721: a caller giving
/// up within 5 s took a host whose boot failed once for one never started again).
const STARTED_WITHIN: Duration = Duration::from_secs(60);

/// The daemon at `host`'s control API, once it answers.
async fn connect(
    host: u8,
) -> cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient<tonic::transport::Channel> {
    const AGAIN: Duration = Duration::from_millis(100);
    let mut failed = None;
    for _ in 0..STARTED_WITHIN.as_millis() / AGAIN.as_millis() {
        match cladefs::cladefsd::control::connect(control(host)).await {
            Ok(client) => return client,
            Err(err) => failed = Some(err),
        }
        madsim::time::sleep(AGAIN).await;
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

/// Board 5806: a host's drain waits for its own VM processes' connections and control calls
/// alone: host 2's guest, connected, its cell open, holds host 1's drain up not at all.
#[madsim::test]
async fn a_hosts_drain_waits_for_its_own_work_alone() {
    let (bucket, _) = world();
    let (nodes, roots) = hosts(2, &bucket);
    let cell = roots[1].join("cell");
    let (opened, open) = futures::channel::oneshot::channel();
    let held = nodes[1].spawn(async move {
        let guest = guest().await;
        guest.open(&cell, 64 << 20).await.unwrap();
        opened.send(()).unwrap();
        std::future::pending::<()>().await;
        guest.close();
    });
    open.await.unwrap();
    let drained = nodes[0].spawn(async {
        while !cladefs::cladefsd::serving() {
            madsim::time::sleep(Duration::from_millis(100)).await;
        }
        cladefs::cladefsd::drain::drain(Duration::from_secs(5)).await
    });
    assert_eq!(drained.await.unwrap(), Ok(()), "host 1's drain waited for host 2's guest");
    held.abort();
}

/// Blocks, by their extents' sequences.
type Blocks = std::collections::BTreeMap<u64, Vec<u8>>;

/// What attrition's writers were told.
#[derive(Default)]
struct Told {
    /// The blocks told uploaded, not freed since.
    uploaded: std::sync::Mutex<Blocks>,
    /// The blocks whose frees were told uploaded: none reads as it was written any more.
    freed: std::sync::Mutex<Blocks>,
    /// Whether writers make versions, as the seed says, after a commit.
    versioning: bool,
    /// Whether writers free blocks told uploaded, as the seed says, after a commit.
    freeing: bool,
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
/// is told uploaded, and frees a block told uploaded ([`free_one`]). The cell's ID; or none, if its open was refused, as a later placement's
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
        if told.freeing
            && madsim::rand::thread_rng().gen_bool(0.3)
            && !free_one(&guest, cell, &told).await
        {
            break;
        }
    }
    // Its last versions uploaded, if its writes went on to the end.
    if !made.is_empty() && guest.commit(cladefs::Commit::Uploaded).await.is_ok() {
        told.versions.lock().unwrap().append(&mut made);
    }
    guest.close();
    Some(cell)
}

/// Frees one of the blocks `told` says were told uploaded, as the seed picks, in `cell`, and
/// commits uploaded: noted freed once that commit is told uploaded, and no longer noted uploaded
/// from the start, as its free may have taken though the commit was not told. Whether both were
/// told done.
async fn free_one(guest: &cladefs::cladefsd::guest::Guest, cell: u64, told: &Told) -> bool {
    use madsim::rand::Rng as _;
    let (sequence, block) = {
        let mut uploaded = told.uploaded.lock().unwrap();
        let Some(&sequence) =
            uploaded.keys().nth(madsim::rand::thread_rng().gen_range(0..uploaded.len().max(1)))
        else {
            return true;
        };
        uploaded.remove_entry(&sequence).unwrap()
    };
    let extent = cladefs::ExtentId { cell, sequence };
    if guest.free(extent).await.is_err() || guest.commit(cladefs::Commit::Uploaded).await.is_err() {
        return false;
    }
    told.freed.lock().unwrap().insert(sequence, block);
    true
}

/// Takes `cell` into the cell directory at `path` on host `host`, at placement `token`, forced if `force`
/// (doc-21: going on with the members that answer, fewer than a take needs), as the VM manager
/// does, trying again while the bucket refuses, as a manager would.
async fn take(host: u8, path: &Path, cell: u64, token: u64, force: bool) {
    let mut daemon = connect(host).await;
    let taking = CreateCellRequest {
        path: path.display().to_string(),
        capacity: 64 << 20,
        token: Some(token),
        cell,
        force,
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
    survives(Profile::PLAIN).await;
}

/// TASK-685.7, doc-21: the attrition profile in a replicated cell, of f 1, its three hosts each
/// other's peers: a writer's commit told uploaded is on two hosts' disks, or in the bucket, and
/// each round's host takes the cell from its members, one of them killed as the seed says, the
/// bucket failing and stalling meanwhile. Every block told uploaded reads back as written.
#[madsim::test]
async fn replicated_commits_survive_attrition() {
    survives(Profile { kept: Kept::replicas(3), ..Profile::PLAIN }).await;
}

/// TASK-685.7: the replicated attrition profile, its hosts cut off the network a while too, as the
/// seed says, both ways: the writer's from its members and the VM manager, its guest writing on,
/// or a member's. Every block told uploaded reads back as written.
#[madsim::test]
async fn replicated_commits_survive_attrition_and_partitions() {
    survives(Profile { kept: Kept::replicas(3), net: Net::Cut, ..Profile::PLAIN }).await;
}

/// TASK-685.7: the replicated attrition profile, a host's packets lost one way a while, as the
/// seed says: it hears its peers and the manager but is not heard, or is heard but hears not.
#[madsim::test]
async fn replicated_commits_survive_one_way_partitions() {
    survives(Profile { kept: Kept::replicas(3), net: Net::OneWay, ..Profile::PLAIN }).await;
}

/// TASK-685.7: the replicated attrition profile, its network's shape shifting (FoundationDB's
/// swizzle): links between the nodes cut one after another, then mended one after another, in
/// another order, and the network losing and delaying packets throughout.
#[madsim::test]
async fn replicated_commits_survive_shifting_partitions_and_loss() {
    let profile =
        Profile { kept: Kept::replicas(3), net: Net::Shifting, loss: true, ..Profile::PLAIN };
    survives(profile).await;
}

/// TASK-685.7, doc-21 (TASK-681): the replicated attrition profile, a member killed with the
/// writer's host, more than a take can do without, and down still as the next host takes the
/// cell, forced: commits told uploaded are none of those a forced take may lose.
#[madsim::test]
async fn replicated_commits_survive_forced_takes() {
    survives(Profile { kept: Kept::replicas(3), forced: true, ..Profile::PLAIN }).await;
}

/// TASK-685.7, doc-20: the attrition profile, its writers making versions as they go, as a VM's
/// snapshots do: once the bucket heals, each version, forked, holds every block told uploaded
/// before it was begun.
#[madsim::test]
async fn versions_made_through_attrition_hold_what_came_before() {
    survives(Profile { versions: true, ..Profile::PLAIN }).await;
}

/// TASK-685.7: the attrition profile, its writers freeing blocks told uploaded as they go, versions
/// made among them, the collector passing over the bucket meanwhile and at the end, compacting
/// what frees left sparse: every block told uploaded and not freed reads back as written, none
/// freed does, and each version holds what came before it.
#[madsim::test]
async fn frees_and_collection_through_attrition_keep_what_is_live() {
    let profile = Profile { frees: true, versions: true, collector: true, ..Profile::PLAIN };
    survives(profile).await;
}

/// TASK-685.7, doc-18: the attrition profile, a base made each round on one host, a version of it
/// forked on another, then the base deleted, as a VM image is deleted under the VMs restored from
/// it, the collector passing meanwhile and at the end: each fork reads the block it inherited.
#[madsim::test]
async fn forks_of_deleted_bases_keep_what_they_inherit() {
    survives(Profile { bases: true, collector: true, ..Profile::PLAIN }).await;
}

/// TASK-685.7: the attrition profile, the hosts peers that are told other peers as they go
/// (SetPeers), each its own list, and a host drained as SIGTERM has it rather than killed, now and
/// then, before it starts again.
#[madsim::test]
async fn uploaded_commits_survive_peer_changes_and_drains() {
    survives(Profile { kept: Kept::peers(3), peers: true, drains: true, ..Profile::PLAIN }).await;
}

/// TASK-685.7: the attrition profile, the hosts' memory and disk limited a while, as the seed
/// says, writes refused past them (ENOMEM, ENOSPC): what was told uploaded reads back once they are
/// lifted.
#[madsim::test]
async fn uploaded_commits_survive_memory_and_disk_pressure() {
    survives(Profile { pressure: true, ..Profile::PLAIN }).await;
}

/// Board 5654: the attrition profile, the hosts' disks failing calls too, as the seed says, each
/// with EIO or ENOSPC before it changes anything: every block told uploaded reads back as written
/// once the bucket and the disks heal.
#[madsim::test]
async fn uploaded_commits_survive_disk_faults() {
    survives(Profile { disk: true, ..Profile::PLAIN }).await;
}

/// Board 5654: the replicated attrition profile, the hosts' disks failing calls too: their
/// replica logs, fences and promises among them.
#[madsim::test]
async fn replicated_commits_survive_disk_faults() {
    survives(Profile { kept: Kept::replicas(3), disk: true, ..Profile::PLAIN }).await;
}

/// TASK-685.7, doc-24: FoundationDB's swarm: each seed its own mix of the profiles' families,
/// drawn as it says ([`Profile::swarm`]), so that a seed tries what no named profile puts
/// together.
#[madsim::test]
async fn swarms_of_faults_keep_what_was_told() {
    survives(Profile::swarm()).await;
}

/// Runs `profile`'s attrition, as [`attrition`] does, and fails on what it lost.
async fn survives(profile: Profile) {
    if let Err(lost) = attrition(profile).await {
        panic!("{lost}");
    }
}

/// How many seeds a search runs at most, unless it says, before it says its rule is not needed.
const SEARCHED: u64 = 2000;

/// TASK-685.6, TASK-685.7: a test that a rule of the protocol is needed: that one
/// of `$seeds` seeds ([`SEARCHED`] unless given) of `$profile`'s attrition, the rule off, loses
/// what a guest was told (FoundationDB's negative tests). Each seed in a process of its own
/// ([`needed`]).
macro_rules! needs {
    ($name:ident, $rule:ident, $profile:expr) => {
        needs!($name, $rule, $profile, SEARCHED);
    };
    ($name:ident, $rule:ident, $profile:expr, $seeds:expr) => {
        #[test]
        fn $name() {
            let off = || {
                bucket_cell::set_rules(bucket_cell::Rules {
                    $rule: false,
                    ..bucket_cell::Rules::ALL
                })
            };
            needed(stringify!($name), off, $profile, $seeds);
        }
    };
}

/// The seed a search's process runs alone, as [`needed`] starts it.
const SEARCH_SEED: &str = "CLADEFS_MADSIM_SEARCH_SEED";

/// What a search's process says on its standard output before what its seed lost, on a line.
const LOST: &str = "cladefs-madsim-search lost: ";

/// How long a search's seed may take, in real time, before its process is killed, as having found
/// nothing: a seed runs in a second or so, but a rule off may have it never end.
const SEARCH_SEED_TIME: Duration = Duration::from_secs(120);

/// Runs `seed` of the search `name` in a process of `binary`'s own: what it said on its standard
/// output; nothing if it ran past [`SEARCH_SEED_TIME`].
fn searched(binary: &Path, name: &str, seed: u64) -> Option<String> {
    use std::io::Read as _;
    let mut child = std::process::Command::new(binary)
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env(SEARCH_SEED, seed.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reading = std::thread::spawn(move || {
        let mut said = String::new();
        let _ = stdout.read_to_string(&mut said);
        said
    });
    let deadline = std::time::Instant::now() + SEARCH_SEED_TIME;
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            eprintln!("seed {seed} of {name} ran past {SEARCH_SEED_TIME:?}: killed");
            child.kill().unwrap();
            child.wait().unwrap();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(reading.join().unwrap())
}

/// The search `name`: runs seeds 1 to `seeds` of `profile`'s attrition, the rules `off` turns off,
/// until one loses what a guest was told; failing if none does. Each seed in a process of its own,
/// this test's binary started again for it (`SEARCH_SEED`), as scripts/test.sh runs each seed:
/// what a simulation's hosts held, leaked as their node-local values are, stays in its process
/// (board 2890), which thousands of seeds in one would fill. A seed that panics finds nothing: the
/// search asks for the checker's finding, not for a failure of any kind.
fn needed(name: &str, off: fn(), profile: Profile, seeds: u64) {
    if let Ok(seed) = std::env::var(SEARCH_SEED) {
        off();
        let seed = seed.parse().expect("a search's seed is a number");
        let runtime = madsim::runtime::Runtime::with_seed_and_config(seed, Default::default());
        if let Err(lost) = runtime.block_on(attrition(profile)) {
            println!("{LOST}{lost}");
        }
        return;
    }
    let binary = std::env::current_exe().unwrap();
    let found = (1..=seeds).find_map(|seed| {
        let said = searched(&binary, name, seed)?;
        // Where in its line: libtest's own `test NAME ... ` comes before it there.
        let (_, lost) = said.split_once(LOST)?;
        Some((seed, lost.lines().next().unwrap_or_default().to_owned()))
    });
    let (seed, lost) =
        found.unwrap_or_else(|| panic!("no seed of {seeds} of {profile:?} lost what it was told"));
    eprintln!("seed {seed} lost: {lost}");
}

// The searches that find their rule needed within [`SEARCHED`] seeds (2026-11-21, 2000 seeds of
// each rule's profile, each rule of bucket_cell::Rules and of local_cell::Rules that the profiles
// reach, board 2830). `slots_at_birth` found its in Linux's seeds (92) but in none of 2000 of
// macOS's, whose schedules differ: no search, as a search that one platform's seeds miss fails
// scripts/test.sh there. The rest of bucket_cell::Rules found none: local_cell::sim's searches find
// each, its steps aimed at the protocol's races with 20,000 seeds, where a full-stack seed reaches
// them seldom. local_cell::Rules' framings, flushes and frees found none either; its views' rules
// are a follower's, and its writes' a writer's racing its own commits, which one simulated guest,
// its requests one at a time, never does: Shuttle's searches find those (tests/shuttle).
//
// A zombie writer, its head swap unconditional, writes the head over its successor's, whose blocks
// told uploaded the head then names not.
needs!(attrition_needs_head_swaps, head_swap, Profile::PLAIN);
// The collector deletes, of a live cell, objects of the head's epoch it may yet name.
needs!(collection_needs_epochs, collector_epochs, Profile::COLLECTED);
// The collector lists objects after it read what cells need: what a writer made between, deleted.
needs!(collection_needs_candidates_first, candidates_first, Profile::COLLECTED);
// A fork of a version, its base deleted, without a reference its base's passes keep it by.
needs!(forking_needs_references, references, Profile::BASES);
// A pass lists its cell's references with its candidates, missing a fork's made since.
needs!(forking_needs_references_relisted, references_relisted, Profile::BASES);

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

/// How a profile cuts its hosts off the network a while, at each round, as the seed says (doc-24,
/// FoundationDB's partitions and swizzle).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Net {
    /// Never.
    Whole,
    /// A host cut off both ways: its links' packets lost, either way.
    Cut,
    /// A host's packets lost one way: it hears but is not heard, or is heard but hears not.
    OneWay,
    /// FoundationDB's swizzle: links between the nodes cut one after another, then mended one
    /// after another in another order, the network's shape shifting as it goes.
    Shifting,
}

/// What an attrition run does beside its writers, its kills and the bucket's faults: its fault
/// families (doc-24, TASK-685.7).
#[derive(Clone, Copy, Debug)]
struct Profile {
    /// What its hosts keep: its cell replicated if they replicate.
    kept: Kept,
    /// How its hosts are cut off the network.
    net: Net,
    /// Whether the network loses and delays packets throughout, which TCP's streams stall on as
    /// they are sent again.
    loss: bool,
    /// Whether writers make versions, each forked once the bucket heals, and read there.
    versions: bool,
    /// Whether the hosts' disks fail calls, as the seed says, once the cell is made, until the
    /// bucket heals ([`DISK_FAULTS`], board 5654).
    disk: bool,
    /// Whether a member of the replicated cell is killed with the writer's host, more than a take
    /// does without, and down still as the next host takes the cell, forced (doc-21, TASK-681).
    forced: bool,
    /// Whether the hosts, peers, are told other peers between rounds, each its own list
    /// (SetPeers, TASK-406.2).
    peers: bool,
    /// Whether a host killed is drained first, now and then, as SIGTERM has it (TASK-141.4).
    drains: bool,
    /// Whether writers free blocks told uploaded ([`free_one`]).
    frees: bool,
    /// Whether a base is made each round, a version of it forked on another host, then deleted
    /// ([`base_round`]): each fork read at the end.
    bases: bool,
    /// Whether the collector passes over the bucket meanwhile, and twice once the bucket heals,
    /// its grace gone between ([`collect_pass`]).
    collector: bool,
    /// Whether a host's memory and disk are limited a while at each round, as the seed says
    /// ([`press`]).
    pressure: bool,
}

impl Profile {
    /// Hosts keeping nothing more, whole, making no versions: the bucket's faults and the kills
    /// alone.
    const PLAIN: Self = Self {
        kept: Kept::NONE,
        net: Net::Whole,
        loss: false,
        versions: false,
        disk: false,
        forced: false,
        peers: false,
        drains: false,
        frees: false,
        bases: false,
        collector: false,
        pressure: false,
    };

    /// What the collector's rules need: frees leaving objects sparse, which it compacts, moving
    /// their extents, versions it keeps, and passes meanwhile and at the end.
    const COLLECTED: Self = Self { frees: true, versions: true, collector: true, ..Self::PLAIN };

    /// What forks' and references' rules need: bases forked and deleted, collected.
    const BASES: Self = Self { bases: true, collector: true, ..Self::PLAIN };

    /// FoundationDB's swarm (doc-24, its `addFailureInjection`): each family on or off as the
    /// simulation's seed says, its hosts plain, peers or replicating, so that each seed runs its
    /// own mix. Families that need peers or replicas only with them.
    fn swarm() -> Self {
        use madsim::rand::Rng as _;
        let mut rng = madsim::rand::thread_rng();
        let kept = match rng.gen_range(0..3) {
            0 => Kept::NONE,
            1 => Kept::peers(3),
            _ => Kept::replicas(3),
        };
        let net = match rng.gen_range(0..4) {
            0 => Net::Whole,
            1 => Net::Cut,
            2 => Net::OneWay,
            _ => Net::Shifting,
        };
        let profile = Self {
            kept,
            net,
            loss: rng.gen_bool(0.3),
            versions: rng.gen_bool(0.5),
            disk: rng.gen_bool(0.3),
            forced: kept.replicas && rng.gen_bool(0.5),
            peers: kept.peers.is_some() && rng.gen_bool(0.5),
            drains: rng.gen_bool(0.3),
            frees: rng.gen_bool(0.5),
            bases: rng.gen_bool(0.3),
            collector: rng.gen_bool(0.5),
            pressure: rng.gen_bool(0.3),
        };
        eprintln!("swarm: {profile:?}");
        profile
    }
}

/// Reads the blocks of `told`, `cell`'s, in the cell directory at `path` on this host, which a guest opens,
/// and those of `freed`: those of `told` lost, or read other than they were written, and those of
/// `freed` read as they were written, by their sequences.
async fn lost_in(path: PathBuf, cell: u64, told: Blocks, freed: Blocks) -> Vec<u64> {
    let guest = guest().await;
    guest.open(&path, 64 << 20).await.unwrap();
    let mut lost = Vec::new();
    for (&sequence, block) in &told {
        let extent = cladefs::ExtentId { cell, sequence };
        if guest.read(extent, 0, 4096).await.as_ref().ok() != Some(block) {
            lost.push(sequence);
        }
    }
    for (&sequence, block) in &freed {
        let extent = cladefs::ExtentId { cell, sequence };
        if guest.read(extent, 0, 4096).await.as_ref().ok() == Some(block) {
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

/// A fork of a base's version, the base deleted since: where it is, and what it inherits.
struct Inherited {
    /// The host its cell directory is on.
    host: usize,
    /// Its cell directory there.
    path: PathBuf,
    /// Its base's cell, whose extent it inherits.
    base: u64,
    /// The block that extent holds.
    block: Vec<u8>,
}

/// Bases' round `round` (doc-18, doc-20): on host `source`'s node `node`, a base made at `base`, a
/// block committed uploaded, a version made of it, uploaded; that version forked on host `target`
/// at `child`; then the base deleted, its version with it but for what the fork inherits, as a VM
/// image is deleted under the VMs restored from it. The fork, if it was told made: the base's
/// deletion may fail, which leaves the base as it was.
async fn base_round(
    round: u64,
    node: madsim::runtime::NodeHandle,
    manager: &madsim::runtime::NodeHandle,
    (source, base): (u8, PathBuf),
    (target, child): (u8, PathBuf),
) -> Option<Inherited> {
    use cladefs::cladefsd::control::proto::DeleteCellRequest;
    let block = block_of(1000 + round, 1);
    let (making, written) = (base.clone(), block.clone());
    let made = node.spawn(async move {
        let guest = guest().await;
        let cell = guest.open(&making, 64 << 20).await.ok()?;
        let extent = cladefs::ExtentId { cell, sequence: 1 };
        guest.write(extent, 0, &written).await.ok()?;
        guest.commit(cladefs::Commit::Uploaded).await.ok()?;
        let version = guest.make_version().await.ok()?;
        guest.close();
        Some((cell, version))
    });
    let Some((cell, version)) = made.await.unwrap() else {
        log::info!("round {round}: a base not made");
        return None;
    };
    let to = child.display().to_string();
    let forked = manager.spawn(async move {
        let path = base.display().to_string();
        let syncing = || {
            let path = path.clone();
            async move { connect(source).await.sync_cell(SyncCellRequest { path }).await }
        };
        calling(source, syncing).await?;
        calling(target, || {
            let request = ForkCellRequest { to: to.clone(), version, cell, ..Default::default() };
            async move { connect(target).await.fork_cell(request).await }
        })
        .await?;
        let deleting = || {
            let path = path.clone();
            async move { connect(source).await.delete_cell(DeleteCellRequest { path }).await }
        };
        if let Err(errno) = calling(source, deleting).await {
            log::info!("round {round}: base {cell} not deleted: {errno:?}");
        }
        Ok::<_, cladefs::Errno>(())
    });
    if let Err(errno) = forked.await.unwrap() {
        log::info!("round {round}: base {cell}'s version not forked: {errno:?}");
        return None;
    }
    Some(Inherited { host: usize::from(target) - 1, path: child, base: cell, block })
}

/// The collector's node, which passes over the hosts' bucket, `bucket`, as `cladefs-gc` does, under
/// the hosts' KMS: its environment a host's, as far as the bucket goes, which the KMS keeps
/// tenants' KEKs in.
fn collector(bucket: &Arc<ObjectBucket>) -> madsim::runtime::NodeHandle {
    let handle = madsim::runtime::Handle::current();
    let bucket = bucket.clone();
    let node = handle.create_node().name("collector").ip("10.0.0.101".parse().unwrap());
    node.init(move || {
        let bucket = bucket.clone();
        async move {
            // A host's token the daemon's bucket asks for, which no placement of the hosts' is.
            let vars = [("CLADEFSD_BUCKET", "memory:///cells"), ("CLADEFSD_TOKEN", "1000")];
            node_local::env::set_vars(vars);
            cladefs::cladefsd::use_bucket(bucket);
            std::future::pending::<()>().await;
        }
    })
    .build()
}

/// How long the collector keeps what it compacted, by the bucket's clock, in milliseconds: short,
/// so that the passes once the bucket heals delete it.
const GRACE: u64 = 2_000;

/// A pass of the collector over `bucket`, as `holder`, which no other pass is, under the hosts' KMS
/// (TASK-411.28): what it did, logged.
async fn collect_pass(bucket: &ObjectBucket, holder: u64) {
    let options = bucket_cell::collector::Options { compact_below: 0.5, grace: GRACE };
    let kmses = cladefs::bucket::kms().ok().map(|kms| kms.kmses().clone());
    let mut collector = bucket_cell::collector::Collector::new(options).kmses(kmses);
    match collector.collect(bucket, holder).await {
        Ok(report) => log::info!("collector pass {holder}: {report:?}"),
        Err(err) => log::info!("collector pass {holder}: {err:?}"),
    }
}

/// Limits host `host`'s memory and disk, as the seed says, through `manager`, as a host short of
/// both does (SetMemoryLimit, ShrinkMemory, SetDiskLimit), then lifts the limits a while after:
/// the task that does, which the heal aborts.
fn press(manager: &madsim::runtime::NodeHandle, host: u8) -> madsim::task::JoinHandle<()> {
    use cladefs::cladefsd::control::proto::{
        SetDiskLimitRequest, SetMemoryLimitRequest, ShrinkMemoryRequest,
    };
    use madsim::rand::Rng as _;
    let mut rng = madsim::rand::thread_rng();
    let memory = (2u64 << 20) << rng.gen_range(0..5);
    let disk = 1u64 << rng.gen_range(16..27);
    let lifted = Duration::from_millis(rng.gen_range(0..4000));
    manager.spawn(async move {
        let Ok(mut daemon) = cladefs::cladefsd::control::connect(control(host)).await else {
            return;
        };
        let memory = SetMemoryLimitRequest { tenant: None, bytes: Some(memory) };
        let shrunk = ShrinkMemoryRequest { tenant: None, target_bytes: 0 };
        let pressed = async {
            daemon.set_memory_limit(memory).await?;
            daemon.shrink_memory(shrunk).await?;
            daemon.set_disk_limit(SetDiskLimitRequest { tenant: None, bytes: Some(disk) }).await
        }
        .await;
        log::info!("host {host}: limited: {:?}", pressed.map(drop));
        madsim::time::sleep(lifted).await;
        log::info!("host {host}: limits lifted: {:?}", lift(&mut daemon).await);
    })
}

/// Lifts the limits of the host whose control API `daemon` calls ([`press`]).
async fn lift(
    daemon: &mut cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient<
        tonic::transport::Channel,
    >,
) -> Result<(), tonic::Status> {
    use cladefs::cladefsd::control::proto::{SetDiskLimitRequest, SetMemoryLimitRequest};
    daemon.set_memory_limit(SetMemoryLimitRequest { tenant: None, bytes: None }).await?;
    daemon.set_disk_limit(SetDiskLimitRequest { tenant: None, bytes: None }).await?;
    Ok(())
}

/// Shuffles `items` as the seed says.
fn shuffled<T>(mut items: Vec<T>) -> Vec<T> {
    use madsim::rand::seq::SliceRandom as _;
    items.shuffle(&mut madsim::rand::thread_rng());
    items
}

/// Cuts `nodes` off the network a while as `net` says, as the seed says: the task that mends it,
/// which the heal aborts, if any.
fn partition(net: Net, nodes: &[madsim::task::NodeId]) -> Option<madsim::task::JoinHandle<()>> {
    use madsim::rand::Rng as _;
    let sim = madsim::net::NetSim::current();
    let mut rng = madsim::rand::thread_rng();
    let cut = nodes[rng.gen_range(0..nodes.len())];
    let mended = Duration::from_millis(rng.gen_range(0..4000));
    match net {
        Net::Whole => return None,
        Net::Cut => sim.clog_node(cut),
        Net::OneWay if rng.gen_bool(0.5) => sim.clog_node_in(cut),
        Net::OneWay => sim.clog_node_out(cut),
        Net::Shifting => {
            let links: Vec<_> = (nodes.iter())
                .flat_map(|&from| nodes.iter().map(move |&to| (from, to)))
                .filter(|(from, to)| from != to)
                .collect();
            let mut links = shuffled(links);
            links.truncate(rng.gen_range(1..=links.len() / 2));
            return Some(madsim::task::spawn(async move {
                let step = || Duration::from_millis(madsim::rand::thread_rng().gen_range(0..500));
                for &(from, to) in &links {
                    madsim::time::sleep(step()).await;
                    sim.clog_link(from, to);
                }
                madsim::time::sleep(mended).await;
                for (from, to) in shuffled(links) {
                    madsim::time::sleep(step()).await;
                    sim.unclog_link(from, to);
                }
            }));
        }
    }
    Some(madsim::task::spawn(async move {
        madsim::time::sleep(mended).await;
        sim.unclog_node(cut);
    }))
}

/// Has each of hosts 1 to `count` told other peers, each its own list, as the seed says, through
/// `manager` (SetPeers): the tasks that do, which the heal aborts.
fn reshuffle_peers(
    manager: &madsim::runtime::NodeHandle,
    count: u8,
) -> Vec<madsim::task::JoinHandle<()>> {
    use cladefs::cladefsd::control::proto::SetPeersRequest;
    use madsim::rand::Rng as _;
    (1..=count)
        .map(|host| {
            let mut addresses = shuffled((1..=count).map(|of| peer(of).to_string()).collect());
            addresses.truncate(madsim::rand::thread_rng().gen_range(1..=addresses.len()));
            manager.spawn(async move {
                let Ok(mut daemon) = cladefs::cladefsd::control::connect(control(host)).await
                else {
                    return;
                };
                let told = daemon.set_peers(SetPeersRequest { addresses: addresses.clone() });
                log::info!("host {host}: peers {addresses:?}: {:?}", told.await.map(drop));
            })
        })
        .collect()
}

/// The attrition profile, as [`uploaded_commits_survive_attrition`] says, as `profile` says: what
/// was lost, if anything.
async fn attrition(profile: Profile) -> Result<(), String> {
    use madsim::rand::Rng as _;
    const ROUNDS: u64 = 5;
    const WRITES: u64 = 6;
    let Profile { kept, net, loss, versions, disk, forced, peers, drains, frees, bases, .. } =
        profile;
    let (bucket, faults) = world();
    let (nodes, roots) = hosts_keeping(3, &bucket, kept);
    let handle = madsim::runtime::Handle::current();
    let told = Arc::new(Told { versioning: versions, freeing: frees, ..Told::default() });
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
    // Hosts killed, started again before the next round, a while after, as the seed says; and one
    // held down through the next round's take, which it forces, then started again.
    let mut killed: Vec<madsim::runtime::NodeHandle> = Vec::new();
    let mut held: Option<madsim::runtime::NodeHandle> = None;
    let restart = |killed: Vec<madsim::runtime::NodeHandle>| async move {
        if killed.is_empty() {
            return;
        }
        let after = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..5000));
        madsim::time::sleep(after).await;
        for node in killed {
            node_local::restart(node.id()).await;
        }
    };
    // The collector's passes meanwhile, each its own holder.
    let collector = profile.collector.then(|| collector(&bucket));
    let passes = collector.as_ref().map(|collector| {
        let bucket = bucket.clone();
        collector.spawn(async move {
            for holder in 1.. {
                let after = madsim::rand::thread_rng().gen_range(500..3000);
                madsim::time::sleep(Duration::from_millis(after)).await;
                collect_pass(&bucket, holder).await;
            }
        })
    });
    // The tasks that cut, limit or reconfigure hosts a while, which the heal aborts.
    let mut meanwhile: Vec<madsim::task::JoinHandle<()>> = Vec::new();
    let sim = madsim::net::NetSim::current();
    if loss {
        let mut rng = madsim::rand::thread_rng();
        let (rate, most) = (rng.gen_range(0.01..0.1), rng.gen_range(10..200));
        sim.update_config(|config| {
            config.packet_loss_rate = rate;
            config.send_latency = Duration::from_millis(1)..Duration::from_millis(most);
        });
    }
    let reachable: Vec<_> = nodes.iter().map(|node| node.id()).chain([manager.id()]).collect();
    let mut inherited = Vec::new();
    for round in 0..ROUNDS {
        restart(std::mem::take(&mut killed)).await;
        // The disks failing once the cell is made: its making is not what the profile tries.
        if disk && cell.is_some() {
            host_fs::faults::set(DISK_FAULTS);
        }
        // As the seed says: hosts cut off the network a while, a host's memory and disk limited,
        // the hosts told other peers.
        if madsim::rand::thread_rng().gen_bool(0.5) {
            let hosts = match net {
                Net::Shifting => &reachable[..],
                _ => &reachable[..nodes.len()],
            };
            meanwhile.extend(partition(net, hosts));
        }
        if profile.pressure && madsim::rand::thread_rng().gen_bool(0.5) {
            meanwhile.push(press(&manager, madsim::rand::thread_rng().gen_range(1..=3)));
        }
        if peers && madsim::rand::thread_rng().gen_bool(0.5) {
            meanwhile.extend(reshuffle_peers(&manager, 3));
        }
        let host = (round % 3) as usize;
        let at = path(round, host);
        if let Some(cell) = cell
            && !std::mem::take(&mut made)
        {
            let at = at.clone();
            let force = held.is_some();
            let taking = async move { take(host as u8 + 1, &at, cell, round + 10, force).await };
            manager.spawn(taking).await.unwrap();
        }
        restart(held.take().into_iter().collect()).await;
        // Its bases' round once every host is up: a host held down is no base's.
        if bases {
            let (source, target) = (host, (host + 1) % 3);
            let base = (source as u8 + 1, roots[source].join(format!("base{round}")));
            let child = (target as u8 + 1, roots[target].join(format!("child{round}")));
            inherited.extend(base_round(round, node(source), &manager, base, child).await);
        }
        let writing = write_round(at, round, WRITES, told.clone());
        let mut written = node(host).spawn(writing);
        // As the seed says: the writer done; or the next round taking the cell as it writes; or
        // its host killed as it writes, drained first now and then, started again a while after,
        // a member killed with it if takes are forced.
        let wait = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..3000));
        match madsim::time::timeout(wait, &mut written).await {
            Ok(done) => cell = cell.or(done.unwrap()),
            Err(_) => {
                if cell.is_none() {
                    cell = written.await.unwrap();
                    assert!(cell.is_some(), "the first round's writer opened no cell");
                } else if madsim::rand::thread_rng().gen_bool(0.5) {
                    if drains && madsim::rand::thread_rng().gen_bool(0.5) {
                        let within = Duration::from_secs(10);
                        let drained = node(host).spawn(cladefs::cladefsd::drain::drain(within));
                        log::info!("host {}: drained: {:?}", host + 1, drained.await);
                    }
                    node_local::kill(nodes[host].id()).await;
                    killed.push(nodes[host].clone());
                    if forced {
                        let member = (host + 2) % 3;
                        node_local::kill(nodes[member].id()).await;
                        held = Some(nodes[member].clone());
                    }
                }
            }
        }
    }
    let cell = cell.unwrap();
    restart(killed.into_iter().chain(held).collect()).await;
    // Healed: the bucket, the disks and the network whole, every host's limits lifted and its
    // peers all, and the last host takes the cell.
    for task in meanwhile {
        task.abort();
    }
    faults.set(bucket_cell::faults::Odds::NONE);
    host_fs::faults::set(host_fs::faults::Odds::NONE);
    sim.update_config(|config| *config = madsim::net::Config::default());
    // Every node and every link mended: a shifting partition aborted midway leaves links cut,
    // which a node's mending leaves as they are.
    for &node in &reachable {
        sim.unclog_node(node);
        for &other in &reachable {
            sim.unclog_link(node, other);
        }
    }
    let healing = manager.spawn(async move {
        for host in 1..=3 {
            if profile.pressure {
                calling(host, || async move {
                    let mut daemon = connect(host).await;
                    lift(&mut daemon).await
                })
                .await?;
            }
        }
        Ok::<_, cladefs::Errno>(())
    });
    healing.await.unwrap().map_err(|errno| format!("lifting the limits: {errno:?}"))?;
    let last = path(ROUNDS, 0);
    let at = last.clone();
    manager.spawn(async move { take(1, &at, cell, ROUNDS + 10, false).await }).await.unwrap();
    // The collector's last passes, its grace gone before each: what it compacted is deleted.
    if let (Some(collector), Some(passes)) = (&collector, passes) {
        passes.abort();
        let bucket = bucket.clone();
        let last = collector.spawn(async move {
            for holder in [u64::MAX - 1, u64::MAX] {
                madsim::time::sleep(Duration::from_millis(GRACE + 1000)).await;
                collect_pass(&bucket, holder).await;
            }
        });
        last.await.unwrap();
    }
    let uploaded = told.uploaded.lock().unwrap().clone();
    let freed = told.freed.lock().unwrap().clone();
    // Some write told uploaded, unless the network was cut or lossy: then every take may have
    // failed, a replicated cell's members out of reach, which is no loss.
    if uploaded.is_empty() {
        assert!(net != Net::Whole || loss, "no write was told uploaded");
        eprintln!("no write was told uploaded: every round's take or open failed, cut off");
    }
    let sequences: Vec<u64> = uploaded.keys().copied().collect();
    let freed_sequences: Vec<u64> = freed.keys().copied().collect();
    let lost = node(0).spawn(lost_in(last, cell, uploaded, freed)).await.unwrap();
    if !lost.is_empty() {
        return Err(format!(
            "blocks told uploaded lost or other, or told freed read back: {lost:?} of \
             {sequences:?}, freed {freed_sequences:?}"
        ));
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
        let lost = node(0).spawn(lost_in(fork, cell, before, Blocks::new())).await.unwrap();
        if !lost.is_empty() {
            return Err(format!(
                "version {version}'s fork lost blocks told uploaded before it: {lost:?} of \
                 {sequences:?}"
            ));
        }
    }
    // Each fork of a base deleted reads the block it inherited.
    if bases {
        eprintln!("forks of bases deleted, each read: {}", inherited.len());
    }
    for Inherited { host, path, base, block } in inherited {
        let reading = path.clone();
        let read = node(host).spawn(async move {
            let guest = guest().await;
            guest.open(&reading, 64 << 20).await?;
            let read = guest.read(cladefs::ExtentId { cell: base, sequence: 1 }, 0, 4096).await;
            guest.close();
            read
        });
        let read = read.await.unwrap();
        if read.as_ref().ok() != Some(&block) {
            return Err(format!(
                "the fork at {path:?} of base {base}, deleted, read its block otherwise: {:?}",
                read.map(|read| read.len())
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
        let id = nodes[host].id();
        async move {
            let after = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..3000));
            madsim::time::sleep(after).await;
            node_local::restart(id).await;
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
                node_local::kill(nodes[host].id()).await;
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
            node_local::kill(nodes[target].id()).await;
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
    // Read on its host, again if the host's node starts anew meanwhile, as one whose boot failed on
    // a disk fault does, 1 to 10 s after (board 5721): the read's task goes with it, cancelled.
    let mut read = None;
    for _ in 0..10 {
        let (reading, expected) = (at.clone(), expected.clone());
        let reading = node(host).spawn(async move {
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
        });
        match reading.await {
            Ok(done) => {
                read = Some(done);
                break;
            }
            Err(join) if join.is_cancelled() => {
                log::info!("the read's host started anew: reading again");
                madsim::time::sleep(Duration::from_secs(1)).await;
            }
            Err(join) => panic!("the read on host {host} failed: {join}"),
        }
    }
    let read = read.expect("the read's host started anew ten times");
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
