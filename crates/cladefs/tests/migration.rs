// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Live migration of an ephemeral or bucket-backed cell between two extent daemons sharing a
//! bucket (backlog doc-25, TASK-698.4, board 11369): forked from a version on the first, written
//! there while the second takes its kept segments in rounds, handed over once its guest is gone,
//! read whole on the second; its reference then the second's alone. A hand-over while its cell is
//! open is refused, and one asked again after it was accepted answers so again.

#![cfg(target_os = "linux")]

use cladefs::cladefsd::control::proto::{
    AbortMigrationRequest, CreateCellRequest, DeleteCellRequest, DropCellRequest, ForkCellRequest,
    HandOverCellRequest, MakeVersionRequest, MigrateCellRequest, PlaceCellRequest, StatCellRequest,
    SyncCellRequest, WaitHandOverRequest, WaitHandOverResponse,
};
use cladefs::testing::Control;
use cladefs::{CellClient, CellId, ExtentId, VolatileSlice, VolatileSliceMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CAPACITY: u64 = 1 << 31;
const EXTENT: usize = 1 << 20;
/// Extents written before the migration begins: past eight segments of 32 MiB, so that some are
/// kept, which the rounds send while the guest runs.
const BEFORE: u64 = 300;
/// Extents written while the migration's rounds run.
const DURING: u64 = 40;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Canonical on-disk cell directory for `cell` on a host rooted at `host_dir`.
fn cell_dir(host_dir: &Path, cell: CellId) -> PathBuf {
    host_dir
        .join("disk0/cells")
        .join(base32_id::Padded(cell.tenant).to_string())
        .join(base32_id::Padded(cell.id).to_string())
}

/// Where migration `migration` of `cell` stages its files on `host_dir`.
fn staging(host_dir: &Path, cell: CellId, migration: u64) -> PathBuf {
    host_dir
        .join("disk0/cells")
        .join(base32_id::Padded(cell.tenant).to_string())
        .join(format!(".{}.migrating-{migration:x}", base32_id::Padded(cell.id)))
}

/// A daemon of a host: its process, its VM socket, a client of its control API, and its
/// ephemeral cells' directory.
struct Daemon {
    process: KillOnDrop,
    vms: PathBuf,
    control: Control,
    ephemeral: PathBuf,
}

/// A daemon serving in `dir`, keeping its cells in the file bucket at `bucket`, its
/// ephemeral cells' directory in `dir`, serving its peers at `peers`' first address.
fn daemon(dir: &Path, bucket: &Path, peers: &[String]) -> Daemon {
    daemon_with(dir, bucket, peers, &[])
}

/// A daemon as [`daemon`] starts one, its environment `env` besides: failpoints (FAILPOINTS) its
/// build has, as this crate's tests build it with the `failpoints` feature.
fn daemon_with(dir: &Path, bucket: &Path, peers: &[String], env: &[(&str, &str)]) -> Daemon {
    daemon_logging(dir, bucket, peers, env, Stdio::inherit())
}

/// A daemon as [`daemon_with`] starts one, what it logs going to `log`.
fn daemon_logging(
    dir: &Path,
    bucket: &Path,
    peers: &[String],
    env: &[(&str, &str)],
    log: Stdio,
) -> Daemon {
    let disk = dir.join("disk0");
    std::fs::create_dir_all(&disk).unwrap();
    let (vms, control, ephemeral) =
        (dir.join("cladefsd.sock"), dir.join("control.sock"), dir.join("ephemeral"));
    // A daemon killed before leaves its sockets, which one started again would bind.
    for socket in [&vms, &control] {
        let _ = std::fs::remove_file(socket);
    }
    let process = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(&vms)
        .env("CLADEFSD_BUCKET", format!("file://{}/", bucket.display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_ROOTS", &disk)
        .env("CLADEFSD_PART_CACHE", dir.join("cache"))
        .env("CLADEFSD_GRPC", &control)
        .env("CLADEFSD_EPHEMERAL", &ephemeral)
        .env("CLADEFSD_PEER_ADDRESS", &peers[0])
        .env("CLADEFSD_PEERS", peers.join(","))
        .envs(env.iter().copied())
        .stderr(log)
        .spawn()
        .unwrap();
    let process = KillOnDrop(process);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&vms).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    let control = Control::connect(&control).unwrap();
    Daemon { process, vms, control, ephemeral }
}

impl Daemon {
    fn migrate(&self, cell: CellId, peer: &str) -> Result<u64, cladefs::Error> {
        let request =
            MigrateCellRequest { tenant: cell.tenant, cell: cell.id, peer: peer.to_owned() };
        self.control
            .call(async |client| client.migrate_cell(request).await)
            .map(|response| response.migration)
    }

    fn hand_over(&self, cell: CellId) -> Result<(), cladefs::Error> {
        let request =
            HandOverCellRequest { tenant: cell.tenant, cell: cell.id, ..Default::default() };
        self.control.call(async |client| client.hand_over_cell(request).await).map(drop)
    }

    fn abort_migration(&self, cell: CellId) -> Result<(), cladefs::Error> {
        let request = AbortMigrationRequest { tenant: cell.tenant, cell: cell.id };
        self.control.call(async |client| client.abort_migration(request).await).map(drop)
    }

    fn wait_hand_over(
        &self,
        cell: CellId,
        migration: u64,
    ) -> Result<WaitHandOverResponse, cladefs::Error> {
        let request = WaitHandOverRequest { tenant: cell.tenant, cell: cell.id, migration };
        self.control.call(async |client| client.wait_hand_over(request).await)
    }

    /// Kills its process, as a crash does.
    fn kill(&mut self) {
        let _ = self.process.0.kill();
        let _ = self.process.0.wait();
    }
}

/// Two loopback addresses no one listens at yet.
fn addresses() -> [String; 2] {
    let listeners = [(); 2].map(|()| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    listeners.map(|listener| listener.local_addr().unwrap().to_string())
}

/// Bytes of extent `sequence` that don't compress.
fn data(sequence: u64) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ sequence;
    (0..EXTENT)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn write(client: &CellClient, extent: ExtentId) {
    client.write(extent, 0, &[VolatileSlice::from(&data(extent.sequence)[..])]).unwrap();
    client.commit(None).unwrap();
}

fn read(client: &CellClient, extent: ExtentId) -> Vec<u8> {
    let mut read = vec![0; EXTENT];
    client.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
    read
}

/// The incarnations the ephemeral cells' references in the bucket at `bucket` name.
fn referring(bucket: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![bucket.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                dirs.push(path);
            } else if let Some(rest) = name.strip_prefix("ephemeral.") {
                found.push(rest.split('.').next().unwrap().to_owned());
            }
        }
    }
    found.sort();
    found
}

#[test]
fn ephemeral_cells_migrate_live_between_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let hosts = [dir.path().join("source"), dir.path().join("target")];
    for path in [&bucket, &hosts[0], &hosts[1]] {
        std::fs::create_dir(path).unwrap();
    }
    let [source_at, target_at] = addresses();
    let source = daemon(&hosts[0], &bucket, &[source_at.clone(), target_at.clone()]);
    let target = daemon(&hosts[1], &bucket, &[target_at.clone(), source_at]);

    // A version on the source, its parent's extent inherited by the ephemeral cell forked from it.
    let client = CellClient::connect_new(&source.vms, 0, CAPACITY).unwrap();
    let base = CellId { tenant: 0, id: client.cell() };
    let inherited = ExtentId { cell: base.id, sequence: 1 };
    write(&client, inherited);
    drop(client);
    let version = source
        .control
        .call(async |client| {
            client.make_version(MakeVersionRequest { tenant: 0, cell: base.id, keep_last: 0 }).await
        })
        .map(|response| response.version)
        .unwrap();
    source
        .control
        .call(async |client| client.sync_cell(SyncCellRequest { tenant: 0, cell: base.id }).await)
        .map(drop)
        .unwrap();
    let stated = source
        .control
        .call(async |client| client.stat_cell(StatCellRequest { tenant: 0, cell: base.id }).await)
        .unwrap();
    let from = cladefs::CellVersion { tenant: stated.tenant, cell: stated.cell, version };
    let child = source
        .control
        .call(async |client| {
            let cladefs::CellVersion { tenant, cell, version } = from;
            client
                .fork_cell(ForkCellRequest {
                    tenant,
                    cell,
                    version,
                    persistent: Some(false),
                    ..Default::default()
                })
                .await
        })
        .map(|response| CellId { tenant: response.tenant, id: response.cell })
        .unwrap();
    let here = cell_dir(&hosts[0], child);

    let client = CellClient::connect_cell(&source.vms, child).unwrap();
    let own = |sequence| ExtentId { cell: child.id, sequence };
    for sequence in 1..=BEFORE {
        write(&client, own(sequence));
    }
    source.migrate(child, &target_at).unwrap();
    for sequence in BEFORE + 1..=BEFORE + DURING {
        write(&client, own(sequence));
    }
    std::thread::sleep(Duration::from_secs(2));
    // Open still, the cell is not handed over.
    let busy = source.hand_over(child).unwrap_err();
    assert_eq!(busy.errno(), Some(cladefs::Errno::EBUSY));
    drop(client);
    source.hand_over(child).unwrap();
    assert!(!here.exists(), "the source's cell directory goes once handed over");
    let incarnation = std::fs::read_to_string(target.ephemeral.join("incarnation")).unwrap();
    // Its version's line (`cladefs ephemeral host 2`, board 8461, doc-28), then the number.
    let (format, incarnation) = incarnation.split_once('\n').unwrap();
    assert_eq!(format, "cladefs ephemeral host 2");
    let incarnation: u64 = incarnation.trim().parse().unwrap();
    let named = referring(&bucket);
    assert_eq!(named.len(), 1, "one reference: {named:?}");
    let noted =
        |daemon: &Daemon| std::fs::read_dir(daemon.ephemeral.join("cells")).unwrap().count();
    assert_eq!((noted(&source), noted(&target)), (0, 1), "the cell is the target's alone");

    let client = CellClient::connect_cell(&target.vms, child).unwrap();
    assert_eq!(client.cell(), child.id);
    assert!(read(&client, inherited) == data(1), "the parent's extent read otherwise");
    for sequence in 1..=BEFORE + DURING {
        assert!(read(&client, own(sequence)) == data(sequence), "extent {sequence} read otherwise");
    }
    let next = own(BEFORE + DURING + 1);
    write(&client, next);
    drop(client);
    let client = CellClient::connect_cell(&target.vms, child).unwrap();
    assert!(read(&client, next) == data(next.sequence));
    drop(client);
    println!(
        "{} MiB migrated, the target's incarnation {incarnation:x}, its reference {named:?}",
        BEFORE + DURING
    );
}

/// A cell forked on the source, written there, closed: the directory the bucket and both
/// hosts are in, each host's peers, the source's and target's daemons, and the cell's ID.
struct Forked {
    dir: tempfile::TempDir,
    peers: [[String; 2]; 2],
    source: Daemon,
    target: Daemon,
    parent: CellId,
    version: u64,
    child: CellId,
    inherited: ExtentId,
}

/// Forks an ephemeral cell on a source, of a version of a base there, and writes `extents` of its
/// own, closed after.
fn forked(extents: u64) -> Forked {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let hosts = [dir.path().join("source"), dir.path().join("target")];
    for path in [&bucket, &hosts[0], &hosts[1]] {
        std::fs::create_dir(path).unwrap();
    }
    let [source_at, target_at] = addresses();
    let peers = [[source_at.clone(), target_at.clone()], [target_at, source_at]];
    let source = daemon(&hosts[0], &bucket, &peers[0]);
    let target = daemon(&hosts[1], &bucket, &peers[1]);
    let client = CellClient::connect_new(&source.vms, 0, CAPACITY).unwrap();
    let base = CellId { tenant: 0, id: client.cell() };
    let inherited = ExtentId { cell: base.id, sequence: 1 };
    write(&client, inherited);
    drop(client);
    let version = source
        .control
        .call(async |client| {
            client.make_version(MakeVersionRequest { tenant: 0, cell: base.id, keep_last: 0 }).await
        })
        .map(|response| response.version)
        .unwrap();
    source
        .control
        .call(async |client| client.sync_cell(SyncCellRequest { tenant: 0, cell: base.id }).await)
        .map(drop)
        .unwrap();
    let stated = source
        .control
        .call(async |client| client.stat_cell(StatCellRequest { tenant: 0, cell: base.id }).await)
        .unwrap();
    let from = cladefs::CellVersion { tenant: stated.tenant, cell: stated.cell, version };
    let child = source
        .control
        .call(async |client| {
            let cladefs::CellVersion { tenant, cell, version } = from;
            client
                .fork_cell(ForkCellRequest {
                    tenant,
                    cell,
                    version,
                    persistent: Some(false),
                    ..Default::default()
                })
                .await
        })
        .map(|response| CellId { tenant: response.tenant, id: response.cell })
        .unwrap();
    let client = CellClient::connect_cell(&source.vms, child).unwrap();
    for sequence in 1..=extents {
        write(&client, ExtentId { cell: child.id, sequence });
    }
    drop(client);
    Forked { dir, peers, source, target, parent: base, version, child, inherited }
}

impl Forked {
    fn bucket(&self) -> PathBuf {
        self.dir.path().join("bucket")
    }

    /// The directory of host `host`: 0 the source's, 1 the target's.
    fn host(&self, host: usize) -> PathBuf {
        self.dir.path().join(["source", "target"][host])
    }

    fn here(&self) -> PathBuf {
        cell_dir(&self.host(0), self.child)
    }

    fn there(&self) -> PathBuf {
        cell_dir(&self.host(1), self.child)
    }

    /// The target's peer address.
    fn target_at(&self) -> &str {
        &self.peers[1][0]
    }

    /// The source's peer address.
    fn source_at(&self) -> &str {
        &self.peers[0][0]
    }

    /// Host `host`'s daemon, killed, as a crash does, and started again.
    fn restart(&mut self, host: usize) {
        self.restart_with(host, &[]);
    }

    /// Host `host`'s daemon, killed, as a crash does, and started again with `env` besides.
    fn restart_with(&mut self, host: usize, env: &[(&str, &str)]) {
        self.restart_logging(host, env, Stdio::inherit());
    }

    /// Host `host`'s daemon, killed, as a crash does, and started again with `env` besides, what
    /// it logs going to `log`.
    fn restart_logging(&mut self, host: usize, env: &[(&str, &str)], log: Stdio) {
        let (dir, bucket) = (self.host(host), self.bucket());
        let killed = match host {
            0 => &mut self.source,
            _ => &mut self.target,
        };
        killed.kill();
        *killed = daemon_logging(&dir, &bucket, &self.peers[host], env, log);
    }

    /// How many cells each host's journal notes: the source's, then the target's.
    fn noted(&self) -> (usize, usize) {
        let noted =
            |daemon: &Daemon| std::fs::read_dir(daemon.ephemeral.join("cells")).unwrap().count();
        (noted(&self.source), noted(&self.target))
    }

    /// Asserts the cell served on the source again, the target holding nothing of it: noted on
    /// the source alone, referred to by it alone, its `extents` reading as written.
    fn assert_served_again(&self, extents: u64) {
        assert!(!self.there().exists());
        assert_eq!(self.noted(), (1, 0), "the cell is the source's alone");
        assert_eq!(referring(&self.bucket()).len(), 1, "the source's reference alone");
        let client = CellClient::connect_cell(&self.source.vms, self.child).unwrap();
        assert!(read(&client, self.inherited) == data(1), "the parent's extent read otherwise");
        for sequence in [1, extents / 2, extents] {
            let extent = ExtentId { cell: self.child.id, sequence };
            assert!(read(&client, extent) == data(sequence), "extent {sequence} read otherwise");
        }
    }

    /// Asserts the cell handed over to the target: noted there alone, referred to by it alone, its
    /// `extents` reading as written.
    fn assert_handed_over(&self, extents: u64) {
        assert!(!self.here().exists(), "the source's cell directory goes once handed over");
        assert_eq!(self.noted(), (0, 1), "the cell is the target's alone");
        assert_eq!(referring(&self.bucket()).len(), 1, "the target's reference alone");
        let client = CellClient::connect_cell(&self.target.vms, self.child).unwrap();
        for sequence in 1..=extents {
            let extent = ExtentId { cell: self.child.id, sequence };
            assert!(read(&client, extent) == data(sequence), "extent {sequence} read otherwise");
        }
    }
}

/// A hand-over the target never answers leaves the cell given up on the source, open
/// there no more, the source restarted too; the target restarted gives up the migration it was
/// taking, and its reference;
/// asked again, it refuses, and the source serves the cell again, holding all it held, the only
/// one referring to its origin.
#[test]
fn hand_overs_refused_leave_the_source_serving() {
    let mut forked = forked(4);
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    assert_eq!(referring(&forked.bucket()).len(), 2, "the source's and the target's");

    forked.target.kill();
    let unanswered = forked.source.hand_over(forked.child).unwrap_err();
    assert_eq!(unanswered.errno(), Some(cladefs::Errno::ENOTCONN));
    let opens = |forked: &Forked| CellClient::connect_cell(&forked.source.vms, forked.child);
    assert!(opens(&forked).is_err(), "given up, it opens not");
    // The source restarted keeps the cell given up, its reference with it.
    forked.restart(0);
    assert!(opens(&forked).is_err(), "given up, it opens not");
    assert_eq!(referring(&forked.bucket()).len(), 2, "the source's kept as it started again");

    forked.restart(1);
    assert_eq!(referring(&forked.bucket()).len(), 1, "the target's given up as it started again");
    let refused = forked.source.hand_over(forked.child).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN));
    forked.assert_served_again(4);
}

/// A target rejects malformed hole ranges over gRPC without creating the path they name.
#[test]
fn migration_file_rpc_refuses_holes_past_the_file() {
    use cladefs::cladefsd::peers::proto::{MigrateFileRequest, Range, peer_client::PeerClient};
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

    let forked = forked(4);
    let migration = forked.source.migrate(forked.child, forked.target_at()).unwrap();

    let tls_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/peer-tls");
    let ca = Certificate::from_pem(std::fs::read(tls_dir.join("ca.crt")).unwrap());
    let identity = Identity::from_pem(
        std::fs::read(tls_dir.join("peer.crt")).unwrap(),
        std::fs::read(tls_dir.join("peer.key")).unwrap(),
    );
    let tls =
        ClientTlsConfig::new().ca_certificate(ca).identity(identity).domain_name("cladefs-peer");

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let status = runtime.block_on(async {
        let channel = Endpoint::from_shared(format!("https://{}", forked.target_at()))
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut peer = PeerClient::new(channel);
        peer.migrate_file(MigrateFileRequest {
            migration,
            path: "invalid-hole/file".to_owned(),
            offset: 0,
            bytes: vec![5],
            len: 1,
            holes: vec![Range { offset: 1, len: 1 }],
        })
        .await
        .unwrap_err()
    });
    assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status:?}");
    let staged = staging(&forked.host(1), forked.child, migration);
    assert!(!staged.join("invalid-hole").exists());
}

/// A target missing a file the hand-over names refuses it, dropping what it took and its
/// reference, and the source serves the cell again: a kept segment the rounds sent, gone from the
/// target's staging before the hand-over.
#[test]
fn hand_overs_naming_files_the_target_lacks_are_refused() {
    let forked = forked(BEFORE);
    let here = forked.here();
    let migration = forked.source.migrate(forked.child, forked.target_at()).unwrap();
    let staged = staging(&forked.host(1), forked.child, migration);
    let kept = |dir: &Path| -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut dirs = vec![dir.join("segments")];
        while let Some(at) = dirs.pop() {
            for entry in std::fs::read_dir(&at).into_iter().flatten() {
                let path = entry.unwrap().path();
                match path.is_dir() {
                    true => dirs.push(path),
                    false
                        if !path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .starts_with("framing-") =>
                    {
                        found.push(path)
                    }
                    false => {}
                }
            }
        }
        // Sort so `kept(&staged)[0]` is the earliest segment, already finished and recorded in
        // `outgoing.sent` by the time later segments arrive (board 11541).
        found.sort();
        found
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while kept(&staged).len() < kept(&here).len() || kept(&here).is_empty() {
        assert!(Instant::now() < deadline, "the rounds never sent the kept segments");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::fs::remove_file(&kept(&staged)[0]).unwrap();
    let refused = forked.source.hand_over(forked.child).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN));
    assert!(!staged.exists(), "the target dropped what it took");
    forked.assert_served_again(BEFORE);
}

/// Board 8455: a target of another build's cell directories (here a failpoint naming another
/// `local_cell` format among its formats) refuses a migration at its prepare, noting, referring
/// to and staging nothing; the source, which gave nothing up, serves the cell on, and migrates it
/// once the target is of its formats again.
#[test]
fn targets_of_another_format_refuse_migrations_at_their_prepare() {
    let mut forked = forked(4);
    let other = [("FAILPOINTS", "peers::formats=return(15)")];
    forked.restart_with(1, &other);
    let source = &forked.source;
    let refused = source.migrate(forked.child, forked.target_at()).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::ENOTSUP), "{refused}");
    let tenant0 = forked.host(1).join("disk0/cells").join(base32_id::Padded(0).to_string());
    let staged: Vec<String> = std::fs::read_dir(&tenant0)
        .into_iter()
        .flatten()
        .map(|found| found.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".migrating-"))
        .collect();
    assert_eq!(staged, Vec::<String>::new(), "staged at the target");
    forked.assert_served_again(4);

    forked.restart(1);
    let source = &forked.source;
    source.migrate(forked.child, forked.target_at()).unwrap();
    source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(4);
}

/// board 2934: a source restarted between a migration's prepare and its hand-over, which pursues
/// that migration no more, migrates the cell to the same target again: the target gives the older
/// migration up at the new one's prepare (specs/ephemeral, `supersedes`), rather than refusing the
/// cell's migrations there for as long as it runs.
#[test]
fn a_migration_its_source_lost_is_superseded_by_the_next() {
    let mut forked = forked(4);
    let source = &forked.source;
    let older = source.migrate(forked.child, forked.target_at()).unwrap();
    let target_dir = forked.host(1);
    let child = forked.child;
    let staged = |migration: u64| staging(&target_dir, child, migration);
    assert!(staged(older).exists());
    forked.restart(0);
    let source = &forked.source;
    let newer = source.migrate(forked.child, forked.target_at()).unwrap();
    assert_ne!(newer, older);
    assert!(!staged(older).exists(), "the older migration's files dropped");
    source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(4);
}

/// board 2934: a hand-over whose last files failed to land, asked again, sends them again and
/// names every file of the cell directory, so that the target takes it whole, never a
/// hand-over naming nothing, which the target would take as whole.
#[test]
fn hand_overs_asked_again_send_and_name_every_file() {
    let forked = forked(4);
    let here = forked.here();
    let source = &forked.source;
    let migration = source.migrate(forked.child, forked.target_at()).unwrap();
    let staged = staging(&forked.host(1), forked.child, migration);
    // A file of the cell directory's, not a kept segment, that the hand-over sends: a directory in
    // its place in the target's staging, so that writing it fails.
    let mut files = Vec::new();
    let mut dirs = vec![PathBuf::new()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(here.join(&dir)).unwrap() {
            let entry = entry.unwrap();
            let relative = dir.join(entry.file_name());
            match entry.file_type().unwrap().is_dir() {
                true if relative != Path::new("segments") => dirs.push(relative),
                true => {}
                false if relative != Path::new("store.deleted") => files.push(relative),
                false => {}
            }
        }
    }
    files.sort();
    let blocked = staged.join(files.last().expect("the cell directory's files"));
    std::fs::create_dir_all(&blocked).unwrap();
    let unanswered = source.hand_over(forked.child).unwrap_err();
    assert_eq!(unanswered.errno(), Some(cladefs::Errno::ENOTCONN));
    std::fs::remove_dir(&blocked).unwrap();
    source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(4);
}

/// Board 5728: a target crashed between renaming a migration's cell into place and remembering the
/// migration accepted removes that cell as it starts again, never accepted, and retires its
/// reference; the source, asked again, is refused and serves the cell again; and the cell migrates
/// again after, rather than finding it taken.
#[test]
fn a_target_crashed_between_its_rename_and_accepting_is_clean_as_it_starts() {
    let mut forked = forked(4);
    let there = forked.there();
    forked.restart_with(1, &[("FAILPOINTS", "migration::renamed=return")]);
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    let unanswered = forked.source.hand_over(forked.child).unwrap_err();
    assert_eq!(unanswered.errno(), Some(cladefs::Errno::ENOTCONN));
    assert!(there.exists(), "renamed into place as the target crashed");
    forked.restart(1);
    assert!(!there.exists(), "the cell, never accepted, removed as the target started again");
    let refused = forked.source.hand_over(forked.child).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN));
    forked.assert_served_again(4);
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    forked.source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(4);
}

/// Board 10705 (hotbed review 86): a target whose acceptance of a hand-over is renamed into place
/// but never synced into its directory (failpoint `ephemeral_host::accepted_renamed`), its reaper
/// queued behind that hand-over then finding the migration accepted, notes the cell taken only once
/// the acceptance is durable: never while its sync fails. Crashed then, the acceptance lost as a
/// power cut may lose a rename never synced, the target removes the cell as it starts again, never
/// accepted; the source, asking again, is refused and serves the cell, the only host to; and the
/// cell migrates there after.
#[test]
fn a_target_noting_a_cell_taken_makes_its_acceptance_durable_first() {
    let mut forked = forked(4);
    let there = forked.there();
    let log = forked.dir.path().join("target.log");
    // The hand-over held at its accepting past the idle bound, the reaper waiting its turn behind.
    let failpoints = "migration::accepting=sleep(5000);ephemeral_host::accepted_renamed=return";
    let env = [("FAILPOINTS", failpoints), ("CLADEFSD_MIGRATION_IDLE", "2")];
    forked.restart_logging(1, &env, std::fs::File::create(&log).unwrap().into());
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    let failed = forked.source.hand_over(forked.child).unwrap_err();
    assert_eq!(failed.errno(), Some(cladefs::Errno::ENOTCONN), "{failed}");
    let deadline = Instant::now() + Duration::from_secs(30);
    // Wait until the reaper has both logged that it found the migration accepted AND finished its
    // `host.accept` call (hitting `accepted_renamed`), so killing the target does not interrupt a
    // concurrent `.new.<hex>` staging file mid-rename.
    loop {
        let logged = std::fs::read_to_string(&log).unwrap();
        if logged.contains("accepted: noted taken, not given up")
            && logged.contains("giving up migrations idle too long")
        {
            break;
        }
        assert!(Instant::now() < deadline, "the reaper never found the migration accepted");
        std::thread::sleep(Duration::from_millis(100));
    }
    forked.target.kill();
    // Lost to a power cut: renamed into its directory, which no sync made durable.
    let accepted = forked.target.ephemeral.join("accepted");
    let renamed: Vec<PathBuf> =
        std::fs::read_dir(&accepted).unwrap().map(|found| found.unwrap().path()).collect();
    assert_eq!(renamed.len(), 1, "the acceptance renamed into place: {renamed:?}");
    std::fs::remove_file(&renamed[0]).unwrap();
    forked.restart(1);
    assert!(!there.exists(), "the cell, never accepted, removed as the target started again");
    let refused = forked.source.hand_over(forked.child).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN), "{refused}");
    forked.assert_served_again(4);
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    forked.source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(4);
}

/// Board 5730: a source crashed letting go of a cell it handed over, its cell directory gone,
/// finishes as it starts again: its reference retired, its journal entry gone. The cell migrated
/// back, then on again, is served there while its migration runs, and goes only once handed over.
#[test]
fn a_source_crashed_letting_a_cell_go_finishes_as_it_starts() {
    let mut forked = forked(4);
    let here = forked.here();
    forked.restart_with(0, &[("FAILPOINTS", "migration::let_go=return")]);
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    forked.source.hand_over(forked.child).unwrap_err();
    assert!(!here.exists(), "its cell directory gone as the source crashed");
    assert_eq!(forked.noted(), (1, 1), "the source's entry kept, noting the cell handed over");
    forked.restart(0);
    forked.assert_handed_over(4);

    let target = &forked.target;
    target.migrate(forked.child, forked.source_at()).unwrap();
    target.hand_over(forked.child).unwrap();
    assert_eq!(forked.noted(), (1, 0), "back on the source alone");
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    let client = CellClient::connect_cell(&forked.source.vms, forked.child).unwrap();
    for sequence in 1..=4 {
        let extent = ExtentId { cell: forked.child.id, sequence };
        assert!(read(&client, extent) == data(sequence), "extent {sequence} read otherwise");
    }
    drop(client);
    forked.source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(4);
}

/// Board 5734: a target gives up a migration whose source it hears nothing of for
/// CLADEFSD_MIGRATION_IDLE, its source crashed: what it took, its journal entry and its reference.
/// The source started again serves the cell.
#[test]
fn targets_give_up_migrations_their_sources_abandoned() {
    let mut forked = forked(4);
    forked.restart_with(1, &[("CLADEFSD_MIGRATION_IDLE", "2")]);
    let source = &forked.source;
    let migration = source.migrate(forked.child, forked.target_at()).unwrap();
    let staged = staging(&forked.host(1), forked.child, migration);
    assert!(staged.exists());
    forked.source.kill();
    let deadline = Instant::now() + Duration::from_secs(30);
    while staged.exists() {
        assert!(Instant::now() < deadline, "the abandoned migration was never given up");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(forked.noted(), (1, 0), "nothing of it left in the target's journal");
    assert_eq!(referring(&forked.bucket()).len(), 1, "the source's reference alone");
    forked.restart(0);
    forked.assert_served_again(4);
}

/// Board 5734: a migration whose source renews it outlasts CLADEFSD_MIGRATION_IDLE, its guest
/// writing nothing meanwhile, and is handed over.
#[test]
fn migrations_their_sources_renew_outlast_the_idle_bound() {
    let mut forked = forked(4);
    forked.restart_with(1, &[("CLADEFSD_MIGRATION_IDLE", "2")]);
    forked.source.migrate(forked.child, forked.target_at()).unwrap();
    std::thread::sleep(Duration::from_secs(6));
    forked.source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(4);
}

/// Board 8577: a migration whose target restarted during its rounds, giving it up, keeps the cell
/// from migrating elsewhere (`EBUSY`) until AbortMigration ends it, its guest writing on
/// meanwhile; the cell then migrates to a second target while its guest runs, that target's
/// hand-over accepted, and the first holds nothing of it.
#[test]
fn migrations_their_targets_gave_up_are_aborted_and_go_elsewhere() {
    let mut forked = forked(4);
    let source = &forked.source;
    let first = source.migrate(forked.child, forked.target_at()).unwrap();
    let client = CellClient::connect_cell(&forked.source.vms, forked.child).unwrap();
    let cell = forked.child.id;
    let own = move |sequence| ExtentId { cell, sequence };
    write(&client, own(5));
    forked.restart(1);
    assert!(
        !staging(&forked.host(1), forked.child, first).exists(),
        "the target restarted gives it up"
    );

    let second_dir = forked.dir.path().join("second");
    std::fs::create_dir(&second_dir).unwrap();
    let [second_at, _] = addresses();
    let peers = [second_at.clone(), forked.source_at().to_owned()];
    let second = daemon(&second_dir, &forked.bucket(), &peers);
    let source = &forked.source;
    let busy = source.migrate(forked.child, &second_at).unwrap_err();
    assert_eq!(busy.errno(), Some(cladefs::Errno::EBUSY), "{busy}");
    source.abort_migration(forked.child).unwrap();
    write(&client, own(6));
    // Ended: nothing of it to end, the cell open or not.
    let again = source.abort_migration(forked.child).unwrap_err();
    assert_eq!(again.errno(), Some(cladefs::Errno::ENOENT), "{again}");

    source.migrate(forked.child, &second_at).unwrap();
    write(&client, own(7));
    drop(client);
    source.hand_over(forked.child).unwrap();
    assert!(!forked.here().exists(), "the source's cell directory goes once handed over");
    assert!(!forked.there().exists(), "the first target took it");
    let noted =
        |daemon: &Daemon| std::fs::read_dir(daemon.ephemeral.join("cells")).unwrap().count();
    assert_eq!(forked.noted(), (0, 0), "the cell is neither the source's nor the first target's");
    assert_eq!(noted(&second), 1, "the cell is the second target's");
    assert_eq!(referring(&forked.bucket()).len(), 1, "the second target's reference alone");
    let client = CellClient::connect_cell(&second.vms, forked.child).unwrap();
    assert!(read(&client, forked.inherited) == data(1), "the parent's extent read otherwise");
    for sequence in 1..=7 {
        assert!(read(&client, own(sequence)) == data(sequence), "extent {sequence} read otherwise");
    }
}

/// Board 8577 (the verifying review, hotbed review 84): a hand-over whose marker's write fails
/// (failpoint `migration::marking`) gives the cell up not, and leaves its migration under way, its
/// rounds going on: AbortMigration then ends it while the guest runs, where it answered EBUSY, and
/// the cell migrates again and is handed over.
#[test]
fn hand_overs_failing_to_give_the_cell_up_leave_it_migrating() {
    let mut forked = forked(4);
    forked.restart_with(0, &[("FAILPOINTS", "migration::marking=1*return")]);
    let source = &forked.source;
    source.migrate(forked.child, forked.target_at()).unwrap();
    let failed = source.hand_over(forked.child).unwrap_err();
    assert_eq!(failed.errno(), Some(cladefs::Errno::EIO), "{failed}");
    let client = CellClient::connect_cell(&forked.source.vms, forked.child).unwrap();
    let cell = forked.child.id;
    write(&client, ExtentId { cell, sequence: 5 });
    source.abort_migration(forked.child).unwrap();
    source.migrate(forked.child, forked.target_at()).unwrap();
    write(&client, ExtentId { cell, sequence: 6 });
    drop(client);
    source.hand_over(forked.child).unwrap();
    forked.assert_handed_over(6);
}

/// Starts a daemon configured for bucket-backed cells only (no `CLADEFSD_EPHEMERAL`; managed
/// disk pool at `dir/disk0`).
fn daemon_bucket_only(dir: &Path, bucket: &Path, peers: &[String], env: &[(&str, &str)]) -> Daemon {
    let disk = dir.join("disk0");
    std::fs::create_dir_all(&disk).unwrap();
    let (vms, control) = (dir.join("cladefsd.sock"), dir.join("control.sock"));
    for socket in [&vms, &control] {
        let _ = std::fs::remove_file(socket);
    }
    let process = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(&vms)
        .env("CLADEFSD_BUCKET", format!("file://{}/", bucket.display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_PART_CACHE", dir.join("cache"))
        .env("CLADEFSD_GRPC", &control)
        .env("CLADEFSD_ROOTS", &disk)
        .env("CLADEFSD_PEER_ADDRESS", &peers[0])
        .env("CLADEFSD_PEERS", peers.join(","))
        .envs(env.iter().copied())
        .spawn()
        .unwrap();
    let process = KillOnDrop(process);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&vms).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    let control = Control::connect(&control).unwrap();
    Daemon { process, vms, control, ephemeral: dir.join("ephemeral-unused") }
}

/// Board 14457: `MigrateCell`, `HandOverCell`, `AbortMigration`, and `WaitHandOver` serve
/// ephemeral cells only and refuse bucket-backed cells with `ENOTSUP` naming the bucket flow;
/// moving a bucket-backed cell goes through `SyncCell` (source) -> `CreateCell` + `PlaceCell`
/// (target) -> open on target -> `DropCell` (fenced source).
#[test]
fn bucket_backed_cells_refuse_peer_migration_and_move_via_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let hosts = [dir.path().join("source"), dir.path().join("target")];
    for path in [&bucket, &hosts[0], &hosts[1]] {
        std::fs::create_dir(path).unwrap();
    }
    let [source_at, target_at] = addresses();
    let source = daemon_bucket_only(
        &hosts[0],
        &bucket,
        &[source_at.clone(), target_at.clone()],
        &[("CLADEFSD_UPLOAD_INTERVAL", "3600")],
    );
    let target = daemon_bucket_only(
        &hosts[1],
        &bucket,
        &[target_at.clone(), source_at],
        &[("CLADEFSD_UPLOAD_INTERVAL", "1")],
    );

    let client = CellClient::connect_new(&source.vms, 0, CAPACITY).unwrap();
    let cell = CellId { tenant: 0, id: client.cell() };
    let here = cell_dir(&hosts[0], cell);
    let there = cell_dir(&hosts[1], cell);
    let own = |sequence| ExtentId { cell: cell.id, sequence };
    for sequence in 1..=8 {
        write(&client, own(sequence));
    }

    // Peer migration RPCs refuse bucket-backed cells with ENOTSUP naming the bucket flow.
    let assert_bucket_refusal = |err: cladefs::Error, rpc: &str| {
        assert_eq!(err.errno(), Some(cladefs::Errno::ENOTSUP), "{rpc}: {err:#}");
        let msg = format!("{err:#}");
        for expected in ["SyncCell", "CreateCell", "PlaceCell", "DropCell"] {
            assert!(msg.contains(expected), "{rpc} error missing {expected:?}: {msg}");
        }
    };
    assert_bucket_refusal(source.migrate(cell, &target_at).unwrap_err(), "MigrateCell");
    assert_bucket_refusal(source.abort_migration(cell).unwrap_err(), "AbortMigration");
    assert_bucket_refusal(target.wait_hand_over(cell, 0).unwrap_err(), "WaitHandOver");

    // HandOverCell with a deprecated fencing token is refused with EINVAL.
    let with_token = source
        .control
        .call(async |c| {
            #[allow(deprecated)]
            let req = HandOverCellRequest { tenant: cell.tenant, cell: cell.id, token: Some(2) };
            c.hand_over_cell(req).await
        })
        .unwrap_err();
    assert_eq!(
        with_token.errno(),
        Some(cladefs::Errno::EINVAL),
        "HandOverCell token: {with_token}"
    );

    // Close client on source, verify HandOverCell also refuses with ENOTSUP.
    drop(client);
    assert_bucket_refusal(source.hand_over(cell).unwrap_err(), "HandOverCell");

    // Move through the bucket: SyncCell -> CreateCell + PlaceCell(2) -> open on target -> DropCell.
    source
        .control
        .call(async |c| c.sync_cell(SyncCellRequest { tenant: cell.tenant, cell: cell.id }).await)
        .unwrap();
    target
        .control
        .call(async |c| {
            c.create_cell(CreateCellRequest {
                capacity: CAPACITY,
                tenant: cell.tenant,
                cell: cell.id,
                ..Default::default()
            })
            .await
        })
        .unwrap();
    target
        .control
        .call(async |c| {
            c.place_cell(PlaceCellRequest { tenant: cell.tenant, cell: cell.id, token: 2 }).await
        })
        .unwrap();
    let target_client = CellClient::connect_cell(&target.vms, cell).unwrap();
    assert!(there.is_dir());
    for sequence in 1..=8 {
        assert!(read(&target_client, own(sequence)) == data(sequence), "extent {sequence}");
    }
    write(&target_client, own(9));
    drop(target_client);

    // Source is fenced; DropCell removes only its local directory, leaving the bucket cell intact.
    source
        .control
        .call(async |c| c.drop_cell(DropCellRequest { tenant: cell.tenant, cell: cell.id }).await)
        .unwrap();
    assert!(!here.exists(), "source cell dir removed after DropCell");

    let verify_client = CellClient::connect_cell(&target.vms, cell).unwrap();
    for sequence in 1..=9 {
        assert!(read(&verify_client, own(sequence)) == data(sequence), "verify extent {sequence}");
    }
}

/// board 5376: `MigrateCell` enforces at most one outgoing migration per cell, refuses
/// `DeleteCell`/`DropCell` with `EBUSY` while a migration is active pre-hand-over, and ephemeral
/// `HandOverCell` prunes staged files not named in the final manifest.
#[test]
fn migrating_cell_refuses_concurrent_migrate_and_delete_canonicalizes_path_and_prunes_reaped_segments()
 {
    let forked = forked(4);
    let migration = forked.source.migrate(forked.child, forked.target_at()).unwrap();

    // AC #1: A second MigrateCell for the same cell is refused with EBUSY.
    let dup = forked.source.migrate(forked.child, forked.target_at()).unwrap_err();
    assert_eq!(dup.errno(), Some(cladefs::Errno::EBUSY), "duplicate MigrateCell: {dup}");

    // AC #2: DeleteCell and DropCell are refused with EBUSY while the migration is moving
    // (before any `migrating` marker is written).
    let del_busy = forked
        .source
        .control
        .call(async |c| {
            c.delete_cell(DeleteCellRequest { tenant: forked.child.tenant, cell: forked.child.id })
                .await
        })
        .unwrap_err();
    assert_eq!(
        del_busy.errno(),
        Some(cladefs::Errno::EBUSY),
        "DeleteCell mid-migration: {del_busy}"
    );
    let drop_busy = forked
        .source
        .control
        .call(async |c| {
            c.drop_cell(DropCellRequest { tenant: forked.child.tenant, cell: forked.child.id })
                .await
        })
        .unwrap_err();
    assert_eq!(
        drop_busy.errno(),
        Some(cladefs::Errno::EBUSY),
        "DropCell mid-migration: {drop_busy}"
    );

    // AC #4: Simulate a kept segment pre-copied in an earlier round and later reaped on the source
    // by planting an extra file in the target's staging directory.
    let staged = staging(&forked.host(1), forked.child, migration);
    let extra_dir = staged.join("segments");
    std::fs::create_dir_all(&extra_dir).unwrap();
    let reaped = extra_dir.join("reaped-kept-segment");
    std::fs::write(&reaped, b"stale-segment-data").unwrap();

    // HandOverCell prunes unlisted files.
    forked.source.hand_over(forked.child).unwrap();
    assert!(
        !forked.there().join("segments").join("reaped-kept-segment").exists(),
        "ephemeral hand-over must prune staged files absent from the final manifest"
    );
    forked.assert_handed_over(4);
}

/// `WaitHandOver` on the target daemon blocks until an incoming ephemeral hand-over is accepted
/// or fails, returns immediately if already accepted or already on disk, and refuses bucket-backed
/// cells even when `CLADEFSD_EPHEMERAL` is configured.
#[test]
fn wait_hand_over_wakes_on_accept_and_failure_and_returns_immediately_when_ready() {
    let mut forked = forked(4);
    let target_control_sock = forked.host(1).join("control.sock");
    let cell1 = forked.child;

    // 1. Waiting BEFORE MigrateCell wakes immediately when HandOverCell is accepted.
    let sock_clone = target_control_sock.clone();
    let waiter = std::thread::spawn(move || {
        let ctrl = Control::connect(&sock_clone).unwrap();
        ctrl.call(async |c| {
            c.wait_hand_over(WaitHandOverRequest {
                tenant: cell1.tenant,
                cell: cell1.id,
                migration: 0,
            })
            .await
        })
    });
    std::thread::sleep(Duration::from_millis(50));
    let mig1 = forked.source.migrate(cell1, forked.target_at()).unwrap();
    forked.source.hand_over(cell1).unwrap();

    let ready = waiter.join().unwrap().unwrap();
    assert_eq!(ready.migration, mig1);
    assert_eq!(ready.tenant, cell1.tenant);
    assert_eq!(ready.cell, cell1.id);

    // 2. Calling WaitHandOver AFTER acceptance returns immediately (for both matching id and 0).
    let cached = forked.target.wait_hand_over(cell1, mig1).unwrap();
    assert_eq!((cached.migration, cached.tenant, cached.cell), (mig1, cell1.tenant, cell1.id));
    let cached_any = forked.target.wait_hand_over(cell1, 0).unwrap();
    assert_eq!(
        (cached_any.migration, cached_any.tenant, cached_any.cell),
        (mig1, cell1.tenant, cell1.id)
    );

    // 3. Waiting on a bucket-backed cell (the parent cell) is refused with ENOTSUP.
    let parent_err = forked.target.wait_hand_over(forked.parent, 0).unwrap_err();
    assert_eq!(parent_err.errno(), Some(cladefs::Errno::ENOTSUP), "bucket cell: {parent_err:#}");

    // 4. Waiting on a migration that is aborted on the source wakes with an error (EAGAIN).
    let cell2_resp = forked
        .source
        .control
        .call(async |c| {
            c.fork_cell(ForkCellRequest {
                tenant: forked.parent.tenant,
                cell: forked.parent.id,
                version: forked.version,
                persistent: Some(false),
                ..Default::default()
            })
            .await
        })
        .unwrap();
    let cell2 = CellId { tenant: cell2_resp.tenant, id: cell2_resp.cell };
    let mig2 = forked.source.migrate(cell2, forked.target_at()).unwrap();

    let sock_clone = target_control_sock.clone();
    let abort_waiter = std::thread::spawn(move || {
        let ctrl = Control::connect(&sock_clone).unwrap();
        ctrl.call(async |c| {
            c.wait_hand_over(WaitHandOverRequest {
                tenant: cell2.tenant,
                cell: cell2.id,
                migration: mig2,
            })
            .await
        })
    });
    std::thread::sleep(Duration::from_millis(50));
    forked.source.abort_migration(cell2).unwrap();
    let err = abort_waiter.join().unwrap().unwrap_err();
    assert_eq!(err.errno(), Some(cladefs::Errno::EAGAIN), "aborted waiter: {err}");

    // 5. Waiting for an already-accepted ephemeral cell after target restart returns immediately.
    forked.restart(1);
    let from_disk = forked.target.wait_hand_over(cell1, 0).unwrap();
    assert_eq!(from_disk.migration, 0);
    assert_eq!(from_disk.tenant, cell1.tenant);
    assert_eq!(from_disk.cell, cell1.id);
}
