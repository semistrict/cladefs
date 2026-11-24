// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Live migration of an ephemeral cell between two extent daemons sharing a bucket (backlog doc-25,
//! TASK-698.4): forked from a version on the first, written there while the second takes its kept
//! segments in rounds, handed over once its guest is gone, read whole on the second; its
//! reference then the second's alone. A hand-over while its cell is open is refused, and one
//! asked again after it was accepted answers so again.

#![cfg(target_os = "linux")]

use cladefs::cladefsd::control::proto::{
    AbortMigrationRequest, ForkCellRequest, HandOverCellRequest, MakeVersionRequest,
    MigrateCellRequest, StatCellRequest, SyncCellRequest,
};
use cladefs::testing::Control;
use cladefs::testing::control_path;
use cladefs::{CellClient, ExtentId, VolatileSlice, VolatileSliceMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
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
        .env("CLADEFSD_PART_CACHE", dir.join("cache"))
        .env("CLADEFSD_GRPC", &control)
        .env("CLADEFSD_EPHEMERAL", &ephemeral)
        .env("CLADEFSD_EPHEMERAL_CELLS", dir)
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
    Daemon { process, vms, control, ephemeral }
}

impl Daemon {
    fn migrate(&self, path: &Path, peer: &str, to: &Path) -> Result<u64, cladefs::Error> {
        let request = MigrateCellRequest {
            path: control_path(path),
            peer: peer.to_owned(),
            to: control_path(to),
        };
        self.control
            .call(async |client| client.migrate_cell(request).await)
            .map(|response| response.migration)
    }

    fn hand_over(&self, path: &Path) -> Result<(), cladefs::Error> {
        let request = HandOverCellRequest { path: control_path(path) };
        self.control.call(async |client| client.hand_over_cell(request).await).map(drop)
    }

    fn abort_migration(&self, path: &Path) -> Result<(), cladefs::Error> {
        let request = AbortMigrationRequest { path: control_path(path) };
        self.control.call(async |client| client.abort_migration(request).await).map(drop)
    }

    /// Kills its process, as a crash does.
    fn kill(&mut self) {
        let _ = self.process.0.kill();
        let _ = self.process.0.wait();
    }
}

/// Where the migration `migration` of a cell to `host`'s `ephemeral-cell` stages its files.
fn staging(host: &Path, migration: u64) -> PathBuf {
    host.join(format!(".ephemeral-cell.migrating-{migration:x}"))
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
    let base = hosts[0].join("base");
    let client = CellClient::connect(&source.vms, &base, CAPACITY).unwrap();
    let inherited = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, inherited);
    drop(client);
    let version = source
        .control
        .call(async |client| {
            client
                .make_version(MakeVersionRequest { path: control_path(&base), keep_last: 0 })
                .await
        })
        .map(|response| response.version)
        .unwrap();
    source
        .control
        .call(async |client| client.sync_cell(SyncCellRequest { path: control_path(&base) }).await)
        .map(drop)
        .unwrap();
    let stated = source
        .control
        .call(async |client| client.stat_cell(StatCellRequest { path: control_path(&base) }).await)
        .unwrap();
    let from = cladefs::CellVersion { tenant: stated.tenant, cell: stated.cell, version };
    let here = hosts[0].join("ephemeral-cell");
    let child = source
        .control
        .call(async |client| {
            let cladefs::CellVersion { tenant, cell, version } = from;
            client
                .fork_cell(ForkCellRequest {
                    to: control_path(&here),
                    tenant,
                    cell,
                    version,
                    ephemeral: true,
                    ..Default::default()
                })
                .await
        })
        .map(|response| cladefs::CellId { tenant: response.tenant, id: response.cell })
        .unwrap();

    let client = CellClient::connect(&source.vms, &here, CAPACITY).unwrap();
    let own = |sequence| ExtentId { cell: child.id, sequence };
    for sequence in 1..=BEFORE {
        write(&client, own(sequence));
    }
    let there = hosts[1].join("ephemeral-cell");
    source.migrate(&here, &target_at, &there).unwrap();
    for sequence in BEFORE + 1..=BEFORE + DURING {
        write(&client, own(sequence));
    }
    std::thread::sleep(Duration::from_secs(2));
    // Open still, the cell is not handed over.
    let busy = source.hand_over(&here).unwrap_err();
    assert_eq!(busy.errno(), Some(cladefs::Errno::EBUSY));
    drop(client);
    source.hand_over(&here).unwrap();
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

    let client = CellClient::connect(&target.vms, &there, CAPACITY).unwrap();
    assert_eq!(client.cell(), child.id);
    assert!(read(&client, inherited) == data(1), "the parent's extent read otherwise");
    for sequence in 1..=BEFORE + DURING {
        assert!(read(&client, own(sequence)) == data(sequence), "extent {sequence} read otherwise");
    }
    let next = own(BEFORE + DURING + 1);
    write(&client, next);
    drop(client);
    let client = CellClient::connect(&target.vms, &there, CAPACITY).unwrap();
    assert!(read(&client, next) == data(next.sequence));
    drop(client);
    println!(
        "{} MiB migrated, the target's incarnation {incarnation:x}, its reference {named:?}",
        BEFORE + DURING
    );
}

/// A cell forked on the source, written there, closed: the directory the bucket and both
/// hosts are in, each host's peers, the source's and target's daemons, the cell's path on the
/// source and its ID.
struct Forked {
    dir: tempfile::TempDir,
    peers: [[String; 2]; 2],
    source: Daemon,
    target: Daemon,
    here: PathBuf,
    child: cladefs::CellId,
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
    let base = hosts[0].join("base");
    let client = CellClient::connect(&source.vms, &base, CAPACITY).unwrap();
    let inherited = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, inherited);
    drop(client);
    let version = source
        .control
        .call(async |client| {
            client
                .make_version(MakeVersionRequest { path: control_path(&base), keep_last: 0 })
                .await
        })
        .map(|response| response.version)
        .unwrap();
    source
        .control
        .call(async |client| client.sync_cell(SyncCellRequest { path: control_path(&base) }).await)
        .map(drop)
        .unwrap();
    let stated = source
        .control
        .call(async |client| client.stat_cell(StatCellRequest { path: control_path(&base) }).await)
        .unwrap();
    let from = cladefs::CellVersion { tenant: stated.tenant, cell: stated.cell, version };
    let here = hosts[0].join("ephemeral-cell");
    let child = source
        .control
        .call(async |client| {
            let cladefs::CellVersion { tenant, cell, version } = from;
            client
                .fork_cell(ForkCellRequest {
                    to: control_path(&here),
                    tenant,
                    cell,
                    version,
                    ephemeral: true,
                    ..Default::default()
                })
                .await
        })
        .map(|response| cladefs::CellId { tenant: response.tenant, id: response.cell })
        .unwrap();
    let client = CellClient::connect(&source.vms, &here, CAPACITY).unwrap();
    for sequence in 1..=extents {
        write(&client, ExtentId { cell: child.id, sequence });
    }
    drop(client);
    Forked { dir, peers, source, target, here, child, inherited }
}

impl Forked {
    fn bucket(&self) -> PathBuf {
        self.dir.path().join("bucket")
    }

    /// The directory of host `host`: 0 the source's, 1 the target's.
    fn host(&self, host: usize) -> PathBuf {
        self.dir.path().join(["source", "target"][host])
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
        let (dir, bucket) = (self.host(host), self.bucket());
        let killed = match host {
            0 => &mut self.source,
            _ => &mut self.target,
        };
        killed.kill();
        *killed = daemon_with(&dir, &bucket, &self.peers[host], env);
    }

    /// How many cells each host's journal notes: the source's, then the target's.
    fn noted(&self) -> (usize, usize) {
        let noted =
            |daemon: &Daemon| std::fs::read_dir(daemon.ephemeral.join("cells")).unwrap().count();
        (noted(&self.source), noted(&self.target))
    }

    /// Asserts the cell served on the source again, the target holding nothing of it: noted on
    /// the source alone, referred to by it alone, its `extents` reading as written.
    fn assert_served_again(&self, there: &Path, extents: u64) {
        assert!(!there.exists());
        assert_eq!(self.noted(), (1, 0), "the cell is the source's alone");
        assert_eq!(referring(&self.bucket()).len(), 1, "the source's reference alone");
        let client = CellClient::connect(&self.source.vms, &self.here, CAPACITY).unwrap();
        assert!(read(&client, self.inherited) == data(1), "the parent's extent read otherwise");
        for sequence in [1, extents / 2, extents] {
            let extent = ExtentId { cell: self.child.id, sequence };
            assert!(read(&client, extent) == data(sequence), "extent {sequence} read otherwise");
        }
    }

    /// Asserts the cell handed over to the target, at `there`: noted there alone, referred to by
    /// it alone, its `extents` reading as written.
    fn assert_handed_over(&self, there: &Path, extents: u64) {
        assert!(!self.here.exists(), "the source's cell directory goes once handed over");
        assert_eq!(self.noted(), (0, 1), "the cell is the target's alone");
        assert_eq!(referring(&self.bucket()).len(), 1, "the target's reference alone");
        let client = CellClient::connect(&self.target.vms, there, CAPACITY).unwrap();
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
    let there = forked.host(1).join("ephemeral-cell");
    forked.source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    assert_eq!(referring(&forked.bucket()).len(), 2, "the source's and the target's");

    forked.target.kill();
    let unanswered = forked.source.hand_over(&forked.here).unwrap_err();
    assert_eq!(unanswered.errno(), Some(cladefs::Errno::ENOTCONN));
    let opens = |forked: &Forked| CellClient::connect(&forked.source.vms, &forked.here, CAPACITY);
    assert!(opens(&forked).is_err(), "given up, it opens not");
    // The source restarted keeps the cell given up, its reference with it.
    forked.restart(0);
    assert!(opens(&forked).is_err(), "given up, it opens not");
    assert_eq!(referring(&forked.bucket()).len(), 2, "the source's kept as it started again");

    forked.restart(1);
    assert_eq!(referring(&forked.bucket()).len(), 1, "the target's given up as it started again");
    let refused = forked.source.hand_over(&forked.here).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN));
    forked.assert_served_again(&there, 4);
}

/// A target rejects malformed hole ranges over gRPC without creating the path they name.
#[test]
fn migration_file_rpc_refuses_holes_past_the_file() {
    use cladefs::cladefsd::peers::proto::{MigrateFileRequest, Range, peer_client::PeerClient};
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

    let forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    let migration = forked.source.migrate(&forked.here, forked.target_at(), &there).unwrap();

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
    let staged = staging(&forked.host(1), migration);
    assert!(!staged.join("invalid-hole").exists());
}

/// A target missing a file the hand-over names refuses it, dropping what it took and its
/// reference, and the source serves the cell again: a kept segment the rounds sent, gone from the
/// target's staging before the hand-over.
#[test]
fn hand_overs_naming_files_the_target_lacks_are_refused() {
    let forked = forked(BEFORE);
    let (here, there) = (&forked.here, forked.host(1).join("ephemeral-cell"));
    let migration = forked.source.migrate(here, forked.target_at(), &there).unwrap();
    let staged = staging(&forked.host(1), migration);
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
        found
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while kept(&staged).len() < kept(here).len() || kept(here).is_empty() {
        assert!(Instant::now() < deadline, "the rounds never sent the kept segments");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::fs::remove_file(&kept(&staged)[0]).unwrap();
    let refused = forked.source.hand_over(here).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN));
    assert!(!staged.exists(), "the target dropped what it took");
    forked.assert_served_again(&there, BEFORE);
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
    let there = forked.host(1).join("ephemeral-cell");
    let source = &forked.source;
    let refused = source.migrate(&forked.here, forked.target_at(), &there).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::ENOTSUP), "{refused}");
    let staged: Vec<String> = std::fs::read_dir(forked.host(1))
        .unwrap()
        .map(|found| found.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".migrating-"))
        .collect();
    assert_eq!(staged, Vec::<String>::new(), "staged at the target");
    forked.assert_served_again(&there, 4);

    forked.restart(1);
    let source = &forked.source;
    source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    source.hand_over(&forked.here).unwrap();
    forked.assert_handed_over(&there, 4);
}

/// board 2934: a source restarted between a migration's prepare and its hand-over, which pursues
/// that migration no more, migrates the cell to the same target again: the target gives the older
/// migration up at the new one's prepare (specs/ephemeral, `supersedes`), rather than refusing the
/// cell's migrations there for as long as it runs.
#[test]
fn a_migration_its_source_lost_is_superseded_by_the_next() {
    let mut forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    let source = &forked.source;
    let older = source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    let target_dir = forked.host(1);
    let staged = |migration: u64| staging(&target_dir, migration);
    assert!(staged(older).exists());
    forked.restart(0);
    let source = &forked.source;
    let newer = source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    assert_ne!(newer, older);
    assert!(!staged(older).exists(), "the older migration's files dropped");
    source.hand_over(&forked.here).unwrap();
    forked.assert_handed_over(&there, 4);
}

/// board 2934: a hand-over whose last files failed to land, asked again, sends them again and
/// names every file of the cell directory, so that the target takes it whole, never a
/// hand-over naming nothing, which the target would take as whole.
#[test]
fn hand_overs_asked_again_send_and_name_every_file() {
    let forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    let source = &forked.source;
    let migration = source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    let staged = staging(&forked.host(1), migration);
    // A file of the cell directory's, not a kept segment, that the hand-over sends: a directory in
    // its place in the target's staging, so that writing it fails.
    let mut files = Vec::new();
    let mut dirs = vec![PathBuf::new()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(forked.here.join(&dir)).unwrap() {
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
    let unanswered = source.hand_over(&forked.here).unwrap_err();
    assert_eq!(unanswered.errno(), Some(cladefs::Errno::ENOTCONN));
    std::fs::remove_dir(&blocked).unwrap();
    source.hand_over(&forked.here).unwrap();
    forked.assert_handed_over(&there, 4);
}

/// Board 5728: a target crashed between renaming a migration's cell into place and remembering the
/// migration accepted removes that cell as it starts again, never accepted, and retires its
/// reference; the source, asked again, is refused and serves the cell again; and the cell migrates
/// to the same path after, rather than finding it taken.
#[test]
fn a_target_crashed_between_its_rename_and_accepting_is_clean_as_it_starts() {
    let mut forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    forked.restart_with(1, &[("FAILPOINTS", "migration::renamed=return")]);
    forked.source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    let unanswered = forked.source.hand_over(&forked.here).unwrap_err();
    assert_eq!(unanswered.errno(), Some(cladefs::Errno::ENOTCONN));
    assert!(there.exists(), "renamed into place as the target crashed");
    forked.restart(1);
    assert!(!there.exists(), "the cell, never accepted, removed as the target started again");
    let refused = forked.source.hand_over(&forked.here).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN));
    forked.assert_served_again(&there, 4);
    forked.source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    forked.source.hand_over(&forked.here).unwrap();
    forked.assert_handed_over(&there, 4);
}

/// Board 5730: a source crashed letting go of a cell it handed over, its cell directory gone,
/// finishes as it starts again: its reference retired, its journal entry gone. The cell migrated
/// back to the same path, then on again, is served there while its migration runs, and goes only
/// once handed over.
#[test]
fn a_source_crashed_letting_a_cell_go_finishes_as_it_starts() {
    let mut forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    forked.restart_with(0, &[("FAILPOINTS", "migration::let_go=return")]);
    forked.source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    forked.source.hand_over(&forked.here).unwrap_err();
    assert!(!forked.here.exists(), "its cell directory gone as the source crashed");
    assert_eq!(forked.noted(), (1, 1), "the source's entry kept, noting the cell handed over");
    forked.restart(0);
    forked.assert_handed_over(&there, 4);

    let target = &forked.target;
    target.migrate(&there, forked.source_at(), &forked.here).unwrap();
    target.hand_over(&there).unwrap();
    assert_eq!(forked.noted(), (1, 0), "back on the source alone");
    forked.source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    let client = CellClient::connect(&forked.source.vms, &forked.here, CAPACITY).unwrap();
    for sequence in 1..=4 {
        let extent = ExtentId { cell: forked.child.id, sequence };
        assert!(read(&client, extent) == data(sequence), "extent {sequence} read otherwise");
    }
    drop(client);
    forked.source.hand_over(&forked.here).unwrap();
    forked.assert_handed_over(&there, 4);
}

/// Board 5734: a target gives up a migration whose source it hears nothing of for
/// CLADEFSD_MIGRATION_IDLE, its source crashed: what it took, its journal entry and its reference.
/// The source started again serves the cell.
#[test]
fn targets_give_up_migrations_their_sources_abandoned() {
    let mut forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    forked.restart_with(1, &[("CLADEFSD_MIGRATION_IDLE", "2")]);
    let source = &forked.source;
    let migration = source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    let staged = staging(&forked.host(1), migration);
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
    forked.assert_served_again(&there, 4);
}

/// Board 5734: a migration whose source renews it outlasts CLADEFSD_MIGRATION_IDLE, its guest
/// writing nothing meanwhile, and is handed over.
#[test]
fn migrations_their_sources_renew_outlast_the_idle_bound() {
    let mut forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    forked.restart_with(1, &[("CLADEFSD_MIGRATION_IDLE", "2")]);
    forked.source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    std::thread::sleep(Duration::from_secs(6));
    forked.source.hand_over(&forked.here).unwrap();
    forked.assert_handed_over(&there, 4);
}

/// Board 8577: a migration whose target restarted during its rounds, giving it up, keeps the cell
/// from migrating elsewhere (`EBUSY`) until AbortMigration ends it, its guest writing on
/// meanwhile; the cell then migrates to a second target while its guest runs, that target's
/// hand-over accepted, and the first holds nothing of it.
#[test]
fn migrations_their_targets_gave_up_are_aborted_and_go_elsewhere() {
    let mut forked = forked(4);
    let there = forked.host(1).join("ephemeral-cell");
    let source = &forked.source;
    let first = source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    let client = CellClient::connect(&forked.source.vms, &forked.here, CAPACITY).unwrap();
    let cell = forked.child.id;
    let own = move |sequence| ExtentId { cell, sequence };
    write(&client, own(5));
    forked.restart(1);
    assert!(!staging(&forked.host(1), first).exists(), "the target restarted gives it up");

    let second_dir = forked.dir.path().join("second");
    std::fs::create_dir(&second_dir).unwrap();
    let [second_at, _] = addresses();
    let peers = [second_at.clone(), forked.source_at().to_owned()];
    let second = daemon(&second_dir, &forked.bucket(), &peers);
    let elsewhere = second_dir.join("ephemeral-cell");
    let source = &forked.source;
    let busy = source.migrate(&forked.here, &second_at, &elsewhere).unwrap_err();
    assert_eq!(busy.errno(), Some(cladefs::Errno::EBUSY), "{busy}");
    source.abort_migration(&forked.here).unwrap();
    write(&client, own(6));
    // Ended: nothing of it to end, the cell open or not.
    let again = source.abort_migration(&forked.here).unwrap_err();
    assert_eq!(again.errno(), Some(cladefs::Errno::ENOENT), "{again}");

    source.migrate(&forked.here, &second_at, &elsewhere).unwrap();
    write(&client, own(7));
    drop(client);
    source.hand_over(&forked.here).unwrap();
    assert!(!forked.here.exists(), "the source's cell directory goes once handed over");
    assert!(!there.exists(), "the first target took it");
    let noted =
        |daemon: &Daemon| std::fs::read_dir(daemon.ephemeral.join("cells")).unwrap().count();
    assert_eq!(forked.noted(), (0, 0), "the cell is neither the source's nor the first target's");
    assert_eq!(noted(&second), 1, "the cell is the second target's");
    assert_eq!(referring(&forked.bucket()).len(), 1, "the second target's reference alone");
    let client = CellClient::connect(&second.vms, &elsewhere, CAPACITY).unwrap();
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
    let there = forked.host(1).join("ephemeral-cell");
    forked.restart_with(0, &[("FAILPOINTS", "migration::marking=1*return")]);
    let source = &forked.source;
    source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    let failed = source.hand_over(&forked.here).unwrap_err();
    assert_eq!(failed.errno(), Some(cladefs::Errno::EIO), "{failed}");
    let client = CellClient::connect(&forked.source.vms, &forked.here, CAPACITY).unwrap();
    let cell = forked.child.id;
    write(&client, ExtentId { cell, sequence: 5 });
    source.abort_migration(&forked.here).unwrap();
    source.migrate(&forked.here, forked.target_at(), &there).unwrap();
    write(&client, ExtentId { cell, sequence: 6 });
    drop(client);
    source.hand_over(&forked.here).unwrap();
    forked.assert_handed_over(&there, 6);
}
