// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-43.4: tenants granted VM sockets of their own through the control API. A connection's
//! tenant is the socket it came in on: its VM process names cells in the tenant's directory
//! alone. A revoked tenant's socket takes no more connections. A tenant's socket works
//! bind-mounted into a jail, as the host's VM manager mounts it. TASK-154: a tenant reaches of the
//! bucket what the VM manager puts in its directory. TASK-155: the daemon's own socket is its
//! user's alone, mounted or not.

#![cfg(target_os = "linux")]

mod common;

use cladefs::{CellClient, Errno, ExtentId, VolatileSliceMut};
use common::daemon::{CAPACITY, Daemon, failure, write};
use common::{cache, copies, pattern};
use std::path::{Path, PathBuf};
use tonic::Code;

/// The errno a VM process connecting to `socket` to open the cell at `path` is refused with.
fn refused(socket: &Path, path: &Path) -> Option<Errno> {
    CellClient::connect(socket, path, CAPACITY).err().and_then(|err| err.errno())
}

/// Each tenant's VM processes open cells in its directory, and are refused another's, by
/// whatever path; granting again returns the socket, named by the tenant's ID; the default tenant
/// is granted nothing.
#[test]
fn tenants_name_cells_in_their_directory_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (acme, globex) = (dir.path().join("acme"), dir.path().join("globex"));
    std::fs::create_dir(&acme).unwrap();
    std::fs::create_dir(&globex).unwrap();
    let ours = daemon.grant(1, &acme, None).unwrap();
    assert_eq!(ours, dir.path().join("tenants/1.sock"));
    let theirs = daemon.grant(2, &globex, None).unwrap();

    let vm = CellClient::connect(&ours, acme.join("vm"), CAPACITY).unwrap();
    write(&vm, &pattern(1), b"acme");
    let their_vm = CellClient::connect(&theirs, globex.join("vm"), CAPACITY).unwrap();
    std::os::unix::fs::symlink(&globex, acme.join("link")).unwrap();
    for path in [
        globex.join("vm"),
        globex.join("new"),
        acme.join("../globex/new"),
        acme.join("link/new"),
        acme.clone(),
        dir.path().join("outside"),
    ] {
        assert_eq!(refused(&ours, &path), Some(Errno::EACCES), "{}", path.display());
    }
    // A VM snapshot's version to restore from, another tenant's, not shared.
    let version = their_vm.make_version().unwrap();
    let restored = CellClient::connect_forked(&ours, acme.join("restored"), version, CAPACITY);
    assert_eq!(restored.err().and_then(|err| err.errno()), Some(Errno::EACCES));

    assert_eq!(daemon.grant(1, &acme, None).unwrap(), ours);
    assert_eq!(failure(daemon.grant(1, &globex, None)), (Code::AlreadyExists, Errno::EEXIST));
    assert_eq!(failure(daemon.grant(0, &acme, None)), (Code::InvalidArgument, Errno::EINVAL));
    let none = dir.path().join("none");
    assert_eq!(failure(daemon.grant(3, &none, None)), (Code::NotFound, Errno::ENOENT));
}

/// TASK-364: a granted tenant's ID is its tenant in the bucket. The cells its VM processes make,
/// and those its VM manager makes or forks in its directory, are cells of its own; its
/// VM manager makes it no new cell of another tenant's.
#[test]
fn tenants_cells_are_their_own() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (acme, globex) = (dir.path().join("acme"), dir.path().join("globex"));
    std::fs::create_dir(&acme).unwrap();
    std::fs::create_dir(&globex).unwrap();
    let ours = daemon.grant(0xa, &acme, None).unwrap();
    daemon.grant(0xb, &globex, None).unwrap();

    let vm = CellClient::connect(&ours, acme.join("vm"), CAPACITY).unwrap();
    write(&vm, &pattern(1), b"acme");
    assert_eq!(daemon.cell(&acme.join("vm")).unwrap().tenant, 0xa);
    let new = |tenant| cladefs::CellId { tenant, id: 0 };
    assert_eq!(daemon.create(&acme.join("made"), new(0), None).unwrap().tenant, 0xa);
    assert_eq!(daemon.create(&acme.join("named"), new(0xa), None).unwrap().tenant, 0xa);
    let theirs = daemon.create(&acme.join("theirs"), new(0xb), None);
    assert_eq!(failure(theirs), (Code::PermissionDenied, Errno::EPERM));
    assert!(!acme.join("theirs").exists(), "a cell of another tenant was made");
    // Forked from a cell of the default tenant's: into its directory, a cell of its own;
    // elsewhere, of the source's tenant.
    let base = dir.path().join("base");
    daemon.create(&base, new(0), None).unwrap();
    assert_eq!(daemon.fork(&base, &acme.join("fork")).unwrap().tenant, 0xa);
    assert_eq!(daemon.fork(&base, &dir.path().join("loose")).unwrap().tenant, 0);

    assert_eq!(daemon.grant(0xa, &acme, None).unwrap(), ours);
    let initech = dir.path().join("initech");
    std::fs::create_dir(&initech).unwrap();
    assert_eq!(failure(daemon.grant(0, &initech, None)), (Code::InvalidArgument, Errno::EINVAL));
}

/// TASK-364: a tenant's VM processes reach no cell of another tenant, though it be in
/// their tenant's directory, put there by other means than the daemon's: refused before it is
/// opened, the other tenant's cell stays theirs, written as it was.
#[test]
fn tenants_reach_no_cell_of_another_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (acme, globex) = (dir.path().join("acme"), dir.path().join("globex"));
    std::fs::create_dir(&acme).unwrap();
    std::fs::create_dir(&globex).unwrap();
    let ours = daemon.grant(0xa, &acme, None).unwrap();
    let theirs = daemon.grant(0xb, &globex, None).unwrap();
    let their_vm = CellClient::connect(&theirs, globex.join("vm"), CAPACITY).unwrap();
    write(&their_vm, &pattern(2), b"globex");
    drop(their_vm);

    let moved = acme.join("moved");
    std::fs::rename(globex.join("vm"), &moved).unwrap();
    assert_eq!(refused(&ours, &moved), Some(Errno::EACCES));
    std::fs::rename(&moved, globex.join("vm")).unwrap();
    let their_vm = CellClient::connect(&theirs, globex.join("vm"), CAPACITY).unwrap();
    assert_eq!(their_vm.root().unwrap(), b"globex");
    assert_eq!(daemon.cell(&globex.join("vm")).unwrap().tenant, 0xb);
}

/// TASK-456.2: a tenant's memory cell, a VM's memory in a cell of the tenant's, opens through
/// the tenant's socket alone: another tenant's VM processes, given its path, are refused
/// (`EACCES`), and it reads as its tenant wrote it.
#[test]
fn tenants_reach_no_memory_cell_of_another_tenants_cell() {
    use cladefs::memory_cell::MemoryCell;
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (acme, globex) = (dir.path().join("acme"), dir.path().join("globex"));
    std::fs::create_dir(&acme).unwrap();
    std::fs::create_dir(&globex).unwrap();
    let ours = daemon.grant(0xa, &acme, None).unwrap();
    let theirs = daemon.grant(0xb, &globex, None).unwrap();
    let size = 64 * 4096;
    let their_memory = MemoryCell::open(&theirs, &globex.join("memory"), size).unwrap();
    their_memory.write(0, &[7; 4096]).unwrap();
    their_memory.commit().unwrap();
    their_memory.close().unwrap();
    assert_eq!(daemon.cell(&globex.join("memory")).unwrap().tenant, 0xb);

    let moved = acme.join("moved");
    std::fs::rename(globex.join("memory"), &moved).unwrap();
    let refused = MemoryCell::open(&ours, &moved, size).err().and_then(|err| err.errno());
    assert_eq!(refused, Some(Errno::EACCES));
    std::fs::rename(&moved, globex.join("memory")).unwrap();
    let their_memory = MemoryCell::open(&theirs, &globex.join("memory"), size).unwrap();
    let mut page = [0; 4096];
    their_memory.read(0, &mut page).unwrap();
    assert_eq!(page, [7; 4096]);
    their_memory.close().unwrap();
}

/// TASK-364: a version of another tenant's cell restores into a cell of the restoring tenant's
/// own. TASK-395.7: while it is shared: before, and once unshared, a restore is refused (EACCES),
/// making nothing, and the restores made live on.
#[test]
fn tenants_restore_shared_versions_of_others_into_cells_of_their_own() {
    let dir = tempfile::tempdir().unwrap();
    let (mut here, mut there) = (Host::new(dir.path(), "here"), Host::new(dir.path(), "there"));
    let globex = dir.path().join("here/globex");
    std::fs::create_dir(&globex).unwrap();
    let socket = here.daemon.grant(2, &globex, None).unwrap();
    let vm = CellClient::connect(&socket, globex.join("vm"), CAPACITY).unwrap();
    write(&vm, &pattern(5), b"globex");
    let version = vm.make_version().unwrap();
    drop(vm);
    here.daemon.sync(&globex.join("vm")).unwrap();
    assert_eq!(version.tenant, 2);

    let refused = there.acme.join("refused");
    assert_eq!(restore(&there.socket, &refused, version).map(drop), Err(Some(Errno::EACCES)));
    assert!(!refused.exists(), "a refused restore made a store");
    here.daemon.share(&globex.join("vm"), version.version, true).unwrap();
    let restored = restore(&there.socket, &there.acme.join("restored"), version).unwrap();
    assert_eq!(restored.root().unwrap(), b"globex");
    let made = there.daemon.cell(&there.acme.join("restored")).unwrap();
    assert_eq!((made.tenant, made.id), (1, restored.cell()));
    here.daemon.share(&globex.join("vm"), version.version, false).unwrap();
    assert_eq!(restore(&there.socket, &refused, version).map(drop), Err(Some(Errno::EACCES)));
    assert!(!refused.exists(), "a refused restore made a store");
    assert_eq!(restored.root().unwrap(), b"globex");
}

/// TASK-395.7: a version of a base forks into another tenant's directory, a cell of that
/// tenant's, only while shared (ShareVersion): refused otherwise (EACCES), making nothing. Into
/// its own tenant's it forks either way. A version the cell's head names not is not shared
/// (NOT_FOUND).
#[test]
fn shared_versions_of_bases_fork_into_other_tenants() {
    let dir = tempfile::tempdir().unwrap();
    let mut here = Host::new(dir.path(), "here");
    let globex = dir.path().join("here/globex");
    std::fs::create_dir(&globex).unwrap();
    let theirs = here.daemon.grant(2, &globex, None).unwrap();
    let base = here.acme.join("base");
    let vm = CellClient::connect(&here.socket, &base, CAPACITY).unwrap();
    write(&vm, &pattern(8), b"base");
    drop(vm);
    let version = here.daemon.version(&base).unwrap();
    here.daemon.sync(&base).unwrap();
    assert_eq!(version.tenant, 1);
    let fork = |daemon: &mut Daemon, to: &Path| daemon.fork_version(version, to).map(drop);

    let refused = globex.join("refused");
    let forked = fork(&mut here.daemon, &refused);
    assert_eq!(failure(forked), (Code::PermissionDenied, Errno::EACCES));
    assert!(!refused.exists(), "a refused fork made a cell");
    fork(&mut here.daemon, &here.acme.join("ours")).unwrap();
    here.daemon.share(&base, version.version, true).unwrap();
    assert_eq!(here.daemon.fork_version(version, &globex.join("theirs")).unwrap().tenant, 2);
    let vm = CellClient::connect(&theirs, globex.join("theirs"), CAPACITY).unwrap();
    assert_eq!(vm.root().unwrap(), b"base");
    drop(vm);
    here.daemon.share(&base, version.version, false).unwrap();
    let forked = fork(&mut here.daemon, &refused);
    assert_eq!(failure(forked), (Code::PermissionDenied, Errno::EACCES));
    let unnamed = here.daemon.share(&base, version.version + 1, true);
    assert_eq!(failure(unnamed), (Code::NotFound, Errno::ENOENT));
}

/// TASK-395.9: a tenant's cell with a writer forks into no other tenant's directory (ForkCell),
/// refused with EACCES, making nothing; into its own tenant's it forks. A cell
/// of the default tenant's (0) forks into both tenants' directories, a cell of each's.
#[test]
fn tenants_cells_with_writers_fork_into_no_other_tenants_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut here = Host::new(dir.path(), "here");
    let globex = dir.path().join("here/globex");
    std::fs::create_dir(&globex).unwrap();
    here.daemon.grant(2, &globex, None).unwrap();
    let vm = here.acme.join("vm");
    write(&CellClient::connect(&here.socket, &vm, CAPACITY).unwrap(), &pattern(9), b"vm");

    let refused = globex.join("refused");
    let forked = here.daemon.fork(&vm, &refused);
    assert_eq!(failure(forked), (Code::PermissionDenied, Errno::EACCES));
    assert!(!refused.exists(), "a refused fork made a cell");
    assert_eq!(here.daemon.fork(&vm, &here.acme.join("ours")).unwrap().tenant, 1);

    let base = dir.path().join("here/base");
    let new = cladefs::CellId { tenant: 0, id: 0 };
    assert_eq!(here.daemon.create(&base, new, None).unwrap().tenant, 0);
    for (to, tenant) in [(here.acme.join("base"), 1), (globex.join("base"), 2)] {
        assert_eq!(here.daemon.fork(&base, &to).unwrap().tenant, tenant);
    }
}

/// A revoked tenant's socket goes, and takes no more connections, while its VM processes
/// connected keep their cells; granted again, its VM processes open its cells again.
#[test]
fn revoked_tenants_take_no_new_connections() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let acme = dir.path().join("acme");
    std::fs::create_dir(&acme).unwrap();
    let socket = daemon.grant(1, &acme, None).unwrap();
    let vm = CellClient::connect(&socket, acme.join("vm"), CAPACITY).unwrap();
    write(&vm, &pattern(2), b"before");

    daemon.revoke(1).unwrap();
    assert!(!socket.exists(), "the socket is there");
    let connected = std::os::unix::net::UnixStream::connect(&socket);
    assert_eq!(connected.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    vm.commit(Some(b"after")).unwrap();
    assert_eq!(vm.root().unwrap(), b"after");
    assert_eq!(failure(daemon.revoke(1)), (Code::NotFound, Errno::ENOENT));
    drop(vm);

    let socket = daemon.grant(1, &acme, None).unwrap();
    let vm = CellClient::connect(&socket, acme.join("vm"), CAPACITY).unwrap();
    assert_eq!(vm.root().unwrap(), b"after");
}

/// TASK-79.36: a tenant revoked whose VM processes live on, granted again, is the tenant they are
/// of: the VM processes it connects share its arena with them, and with it the pages of the cells
/// they have open.
#[test]
fn tenants_granted_again_while_alive_go_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let acme = dir.path().join("acme");
    std::fs::create_dir(&acme).unwrap();
    let socket = daemon.grant(1, &acme, None).unwrap();
    let vm = CellClient::connect(&socket, acme.join("vm"), CAPACITY).unwrap();
    let bytes = pattern(4);
    cache(&vm, &bytes);

    daemon.revoke(1).unwrap();
    let socket = daemon.grant(1, &acme, None).unwrap();
    let other = CellClient::connect(&socket, acme.join("other"), CAPACITY).unwrap();
    assert_eq!(copies(other.physical_memory(), &bytes), 1, "the VM's pages, in its memory");
}

/// The environment of this test's process run in a jail ([`jailed`]): the socket's path there,
/// the tenant's socket's path on the host, and the cell directory to open.
const JAIL_SOCKET: &str = "CLADEFS_TEST_JAIL_SOCKET";
const HOST_SOCKET: &str = "CLADEFS_TEST_HOST_SOCKET";
const JAIL_CELL_DIR: &str = "CLADEFS_TEST_JAIL_CELL_DIR";

/// A tenant's socket, given to its jails' user and bind-mounted into a jail, as the host's VM
/// manager does with Firecracker's jailer: in a mount namespace of its own, where the tenants'
/// sockets' directory is hidden, a VM process of that user opens the tenant's cell through it.
/// Needs `sudo -n`, as Lima's VMs have it.
#[test]
fn tenant_sockets_work_bind_mounted_into_jails() {
    if let Some(socket) = std::env::var_os(JAIL_SOCKET) {
        return jailed(socket.as_ref());
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let acme = dir.path().join("acme");
    std::fs::create_dir(&acme).unwrap();
    // SAFETY: Plain system calls.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let socket = daemon.grant(1, &acme, Some(uid)).unwrap();
    let jail = dir.path().join("jail");
    std::fs::create_dir(&jail).unwrap();
    let jail_socket = jail.join("cladefsd.sock");
    std::fs::write(&jail_socket, b"").unwrap();
    let cell_dir = acme.join("vm");

    let script = r#"mount --bind "$1" "$2" && mount -t tmpfs none "$3" &&
        exec setpriv --reuid "$4" --regid "$5" --clear-groups \
            env "$6" "$7" "$8" "$9" --exact "${10}" --nocapture"#;
    let test = "tenant_sockets_work_bind_mounted_into_jails";
    let environment = [
        format!("{JAIL_SOCKET}={}", jail_socket.display()),
        format!("{HOST_SOCKET}={}", socket.display()),
        format!("{JAIL_CELL_DIR}={}", cell_dir.display()),
    ];
    let status = std::process::Command::new("sudo")
        .args(["-n", "unshare", "--mount", "--propagation", "private", "sh", "-c", script, "sh"])
        .arg(&socket)
        .arg(&jail_socket)
        .arg(socket.parent().unwrap())
        .args([uid.to_string(), gid.to_string()])
        .args(environment)
        .arg(std::env::current_exe().unwrap())
        .arg(test)
        .status()
        .unwrap();
    assert!(status.success(), "the jailed VM process failed: {status}");
    assert_eq!(daemon.stat(&cell_dir).unwrap().used, 64 << 10);
}

/// The VM process in the jail: the tenants' sockets hidden, it opens its cell through the one
/// bind-mounted at `socket`, and writes it.
fn jailed(socket: &Path) {
    let host_socket = std::env::var_os(HOST_SOCKET).unwrap();
    assert!(!Path::new(&host_socket).exists(), "the tenants' sockets are not hidden");
    let cell_dir = std::env::var_os(JAIL_CELL_DIR).unwrap();
    let vm = CellClient::connect(socket, cell_dir, CAPACITY).unwrap();
    write(&vm, &pattern(3), b"jailed");
}

/// A host of two over one bucket: a daemon of its own, and the tenant acme's directory and socket
/// on it. The two are threads of one process, as a bucket in memory is one process's: they share
/// their extent cache, which [`Host::forget_what_it_read`] empties, and the bucket alone.
struct Host {
    daemon: Daemon,
    acme: PathBuf,
    socket: PathBuf,
}

impl Host {
    fn new(dir: &Path, name: &str) -> Self {
        let dir = dir.join(name);
        let acme = dir.join("acme");
        std::fs::create_dir_all(&acme).unwrap();
        let mut daemon = Daemon::new(&dir);
        let socket = daemon.grant(1, &acme, None).unwrap();
        Self { daemon, acme, socket }
    }

    /// What the host read goes from memory, as it never had it on another host.
    fn forget_what_it_read(&mut self) {
        self.daemon.shrink_memory(None, 0).unwrap();
    }

    /// A snapshot of a VM of the tenant, as Firecracker takes it: a version of its cell, in the
    /// bucket once synced, for other hosts.
    fn snapshot(&mut self) -> Snapshot {
        let vm = CellClient::connect(&self.socket, self.acme.join("vm"), CAPACITY).unwrap();
        write(&vm, &pattern(5), b"snapshotted");
        let extent = ExtentId { cell: vm.cell(), sequence: 1 };
        let version = vm.make_version().unwrap();
        drop(vm);
        self.daemon.sync(&self.acme.join("vm")).unwrap();
        Snapshot { version, extent }
    }
}

/// What a snapshot holds of its VM: the version of its cell (TASK-364), and the extent the
/// VM wrote.
struct Snapshot {
    version: cladefs::CellVersion,
    extent: ExtentId,
}

/// A VM process's restore through `socket`, into a cell of its own at `cell_dir`, of `version`: the
/// request Firecracker's restore makes.
fn restore(
    socket: &Path,
    cell_dir: &Path,
    version: cladefs::CellVersion,
) -> Result<CellClient, Option<Errno>> {
    CellClient::connect_forked(socket, cell_dir, version, CAPACITY).map_err(|err| err.errno())
}

/// The bytes of the VM's `extent`, as the snapshot holds them, read through `client`.
fn snapshotted(client: &CellClient, extent: ExtentId) -> bool {
    read(client, extent) == pattern(5)
}

/// What `client` holds of `extent`, a block.
fn read(client: &CellClient, extent: ExtentId) -> Vec<u8> {
    let mut read = vec![0; 64 << 10];
    client.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
    read
}

/// TASK-154: a tenant's snapshot restores on another host, which has nothing of it, through the
/// request every restore makes: what it reads comes from the bucket, as many restores as there
/// are, each a cell of its own.
#[test]
fn tenants_restore_on_another_host() {
    let dir = tempfile::tempdir().unwrap();
    let (mut here, mut there) = (Host::new(dir.path(), "here"), Host::new(dir.path(), "there"));
    let snapshot = here.snapshot();
    here.forget_what_it_read();
    let fetched = there.daemon.counters().fetched_bytes;

    let restored = restore(&there.socket, &there.acme.join("vm"), snapshot.version).unwrap();
    assert_ne!(restored.cell(), snapshot.version.cell);
    assert_eq!(restored.root().unwrap(), b"snapshotted");
    assert!(snapshotted(&restored, snapshot.extent), "the restored VM reads other bytes");
    let read = there.daemon.counters().fetched_bytes - fetched;
    assert!(read >= 64 << 10, "{read} bytes came from the bucket");
    write(&restored, &pattern(6), b"restored");

    // A zygote's other child, and the first's writes none of its.
    let other = restore(&there.socket, &there.acme.join("other"), snapshot.version).unwrap();
    assert_eq!(other.root().unwrap(), b"snapshotted");
    assert_eq!(restored.root().unwrap(), b"restored");
}

/// TASK-154, TASK-395.7: a tenant's VM processes restore no version of another tenant's cell that
/// isn't shared (EACCES), making nothing; the version's own tenant's do.
#[test]
fn tenants_restore_no_unshared_version_of_another() {
    let dir = tempfile::tempdir().unwrap();
    let (mut here, mut there) = (Host::new(dir.path(), "here"), Host::new(dir.path(), "there"));
    let snapshot = here.snapshot();
    let globex = dir.path().join("there/globex");
    std::fs::create_dir(&globex).unwrap();
    let socket = there.daemon.grant(2, &globex, None).unwrap();
    let vm = globex.join("vm");
    assert_eq!(restore(&socket, &vm, snapshot.version).map(drop), Err(Some(Errno::EACCES)));
    assert!(!vm.exists(), "a refused restore made a store");

    let restored = restore(&there.socket, &there.acme.join("vm"), snapshot.version).unwrap();
    assert_eq!(restored.root().unwrap(), b"snapshotted");
}

/// TASK-154: a VM snapshot's version is of its VM's own cell: a VM process makes no version of
/// another's, and a restore of it reads what the VM wrote.
#[test]
fn vm_processes_make_no_version_of_another_cell() {
    let dir = tempfile::tempdir().unwrap();
    let mut here = Host::new(dir.path(), "here");
    let vm = CellClient::connect(&here.socket, here.acme.join("vm"), CAPACITY).unwrap();
    write(&vm, &pattern(1), b"ours");
    let theirs_path = dir.path().join("theirs");
    let theirs = here.daemon.open(&theirs_path, CAPACITY);
    write(&theirs, &pattern(2), b"theirs");

    let made = vm.make_version().unwrap();
    assert_eq!(made.cell, vm.cell());
    assert_ne!(made.cell, here.daemon.cell(&theirs_path).unwrap().id);
    let restored = restore(&here.socket, &here.acme.join("restored"), made).unwrap();
    assert_eq!(restored.root().unwrap(), b"ours");
}

/// TASK-154: a version dropped (DropVersions) restores no more, on its host or another (ENOENT),
/// and what was restored from it lives on.
#[test]
fn versions_dropped_restore_no_more_and_leave_their_restores() {
    let dir = tempfile::tempdir().unwrap();
    let (mut here, there) = (Host::new(dir.path(), "here"), Host::new(dir.path(), "there"));
    let snapshot = here.snapshot();
    let restored = restore(&there.socket, &there.acme.join("vm"), snapshot.version).unwrap();
    here.daemon.drop_versions(&here.acme.join("vm"), &[snapshot.version.version]).unwrap();
    here.daemon.sync(&here.acme.join("vm")).unwrap();

    for (socket, other) in
        [(&here.socket, here.acme.join("other")), (&there.socket, there.acme.join("other"))]
    {
        let refused = restore(socket, &other, snapshot.version);
        assert_eq!(refused.map(drop), Err(Some(Errno::ENOENT)));
    }
    assert!(snapshotted(&restored, snapshot.extent), "what was restored went with the version");
    write(&restored, &pattern(6), b"restored");
}

/// TASK-158: a version is read by any number of hosts and written by none: restores of it on hosts
/// in any order, the snapshot's own host's too, read it, and none went stale.
#[test]
fn versions_restore_on_every_host_in_any_order() {
    let dir = tempfile::tempdir().unwrap();
    let (mut here, there) = (Host::new(dir.path(), "here"), Host::new(dir.path(), "there"));
    let snapshot = here.snapshot();
    let restored = restore(&there.socket, &there.acme.join("vm"), snapshot.version).unwrap();
    let again = restore(&here.socket, &here.acme.join("restored"), snapshot.version).unwrap();
    assert_eq!(again.root().unwrap(), b"snapshotted");
    let other = restore(&there.socket, &there.acme.join("other"), snapshot.version).unwrap();
    assert_eq!(other.root().unwrap(), b"snapshotted");
    assert!(snapshotted(&restored, snapshot.extent), "the first restore lost what it holds");
}

/// TASK-158: one snapshot restored on several hosts at once: the requests of each race, on threads
/// of their own, and each host's VMs read the snapshot, and write on their own.
#[test]
fn several_hosts_restore_one_snapshot_at_once() {
    const HOSTS: usize = 3;
    const RESTORES: usize = 4;
    let dir = tempfile::tempdir().unwrap();
    let mut hosts: Vec<_> =
        (0..HOSTS).map(|host| Host::new(dir.path(), &format!("host{host}"))).collect();
    let snapshot = hosts[0].snapshot();
    let restored: Vec<_> = std::thread::scope(|scope| {
        let hosts = &hosts;
        let restores: Vec<_> = (0..HOSTS * RESTORES)
            .map(|at| {
                let host = &hosts[at % HOSTS];
                let vm = host.acme.join(format!("restored{at}"));
                scope.spawn(move || restore(&host.socket, &vm, snapshot.version))
            })
            .collect();
        restores.into_iter().map(|restore| restore.join().unwrap().unwrap()).collect()
    });
    let mut cells: Vec<_> = restored.iter().map(CellClient::cell).collect();
    cells.sort();
    cells.dedup();
    assert_eq!(cells.len(), HOSTS * RESTORES, "two restores of one cell");
    for (at, vm) in restored.iter().enumerate() {
        assert_eq!(vm.root().unwrap(), b"snapshotted", "restore {at}");
        assert!(snapshotted(vm, snapshot.extent), "restore {at} reads other bytes");
        write(vm, &pattern(at as u64), format!("restored {at}").as_bytes());
    }
    for (at, vm) in restored.iter().enumerate() {
        assert_eq!(vm.root().unwrap(), format!("restored {at}").as_bytes());
    }
}

/// TASK-158: a version needs no placement to restore, however late the placement that took its
/// cell.
#[test]
fn versions_restore_whatever_placement_took_them() {
    let dir = tempfile::tempdir().unwrap();
    let (mut here, there) = (Host::new(dir.path(), "here"), Host::new(dir.path(), "there"));
    let snapshot = here.snapshot();
    here.daemon.place(&here.acme.join("vm"), 7).unwrap();
    here.daemon.sync(&here.acme.join("vm")).unwrap();

    let restored = restore(&there.socket, &there.acme.join("vm"), snapshot.version).unwrap();
    assert_eq!(restored.root().unwrap(), b"snapshotted");
    let own = restore(&here.socket, &here.acme.join("restored"), snapshot.version);
    assert_eq!(own.unwrap().root().unwrap(), b"snapshotted");
}

/// TASK-158: a version of a base, as of any cell, forks on several hosts at once, as a
/// snapshot's restores do (ForkCell's `version`): into cells of their own, which read the base
/// and write on their own. The base is its writer's still: what it writes after reaches no fork of
/// the version.
#[test]
fn versions_of_bases_fork_on_several_hosts_at_once() {
    const HOSTS: usize = 3;
    const FORKS: usize = 3;
    let dir = tempfile::tempdir().unwrap();
    let mut hosts: Vec<_> =
        (0..HOSTS).map(|host| Host::new(dir.path(), &format!("host{host}"))).collect();
    let base = hosts[0].acme.join("base");
    let vm = hosts[0].daemon.open(&base, CAPACITY);
    write(&vm, &pattern(7), b"base");
    let extent = ExtentId { cell: vm.cell(), sequence: 1 };
    drop(vm);
    let version = hosts[0].daemon.version(&base).unwrap();
    hosts[0].daemon.sync(&base).unwrap();

    let forks: Vec<(usize, PathBuf, cladefs::CellId)> = std::thread::scope(|scope| {
        let forking: Vec<_> = (hosts.iter_mut().enumerate())
            .map(|(at, host)| {
                scope.spawn(move || {
                    let fork = |n| {
                        let to = host.acme.join(format!("fork{n}"));
                        let cell = host.daemon.fork_version(version, &to).unwrap();
                        (at, to, cell)
                    };
                    (0..FORKS).map(fork).collect::<Vec<_>>()
                })
            })
            .collect();
        forking.into_iter().flat_map(|forking| forking.join().unwrap()).collect()
    });
    let mut cells: Vec<_> = forks.iter().map(|&(_, _, cell)| cell).collect();
    cells.sort();
    cells.dedup();
    assert_eq!(cells.len(), HOSTS * FORKS, "two forks of one cell");
    let opened: Vec<_> =
        forks.iter().map(|(at, to, _)| hosts[*at].daemon.open(to, CAPACITY)).collect();
    for (n, vm) in opened.iter().enumerate() {
        assert_eq!(vm.root().unwrap(), b"base", "fork {n}");
        assert!(read(vm, extent) == pattern(7), "fork {n} reads other bytes");
        write(vm, &pattern(10 + n as u64), format!("fork {n}").as_bytes());
    }
    for (n, vm) in opened.iter().enumerate() {
        assert_eq!(vm.root().unwrap(), format!("fork {n}").as_bytes());
    }

    let vm = hosts[0].daemon.open(&base, CAPACITY);
    vm.commit(Some(b"base, later")).unwrap();
    drop(vm);
    let other = &mut hosts[1];
    let later = other.acme.join("later");
    other.daemon.fork_version(version, &later).unwrap();
    assert_eq!(other.daemon.open(&later, CAPACITY).root().unwrap(), b"base");
}

/// The environment of this test's process as another user ([`as_another_user`]): a socket of the
/// daemon's, mounted where a jail would have it, and a socket beside it anyone may connect to.
const JAILED_SOCKET: &str = "CLADEFS_TEST_JAILED_SOCKET";
const OPEN_SOCKET: &str = "CLADEFS_TEST_OPEN_SOCKET";

/// The user a jail's VM process runs as, as scripts/test-e2e.sh has it.
const JAIL_USER: u32 = 64000;

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// TASK-155: the real daemon's own socket, whatever its umask, even none, is its user's alone: a
/// VM process of another user, with it bind-mounted into its mount namespace, is refused by the
/// kernel, where a socket beside it connects. Needs `sudo -n`, as Lima's VMs have it.
#[test]
fn the_default_socket_refuses_other_users() {
    refuses_other_users("the_default_socket_refuses_other_users", |sockets| sockets.vms);
}

/// TASK-159: so is its control socket, which can do everything.
#[test]
fn the_control_socket_refuses_other_users() {
    refuses_other_users("the_control_socket_refuses_other_users", |sockets| sockets.control);
}

/// The sockets of a daemon [`refuses_other_users`] runs.
struct Sockets {
    vms: PathBuf,
    control: PathBuf,
}

/// That the real daemon, under no umask, refuses a process of another user the socket of its that
/// `jailed` picks, mounted into the process's mount namespace: the test `test`, which this process
/// runs again as that user.
fn refuses_other_users(test: &str, jailed: fn(Sockets) -> PathBuf) {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    if let Some(socket) = std::env::var_os(JAILED_SOCKET) {
        return as_another_user(socket.as_ref());
    }
    // Where the other user reaches, so that what refuses it is the socket.
    let dir = tempfile::Builder::new().prefix("cladefs-").tempdir_in("/tmp").unwrap();
    let everyone = |path: &Path, mode| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    everyone(dir.path(), 0o755);
    let (vms, control) = (dir.path().join("cladefsd.sock"), dir.path().join("cladefsd.control"));
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    daemon.arg(&vms).env("CLADEFSD_GRPC", &control);
    daemon.env("CLADEFSD_BUCKET", "memory:///cells").env("CLADEFSD_TOKEN", "1");
    // SAFETY: umask is async-signal-safe.
    unsafe {
        daemon.pre_exec(|| {
            libc::umask(0);
            Ok(())
        })
    };
    let _daemon = KillOnDrop(daemon.spawn().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while UnixStream::connect(&vms).is_err() || UnixStream::connect(&control).is_err() {
        assert!(std::time::Instant::now() < deadline, "the daemon never served");
        std::thread::yield_now();
    }
    let socket = jailed(Sockets { vms, control });

    let open = dir.path().join("open.sock");
    let _open = std::os::unix::net::UnixListener::bind(&open).unwrap();
    everyone(&open, 0o666);
    let jail = dir.path().join("jail");
    std::fs::create_dir(&jail).unwrap();
    everyone(&jail, 0o755);
    let jail_socket = jail.join("cladefsd.sock");
    std::fs::write(&jail_socket, b"").unwrap();
    let script = r#"mount --bind "$1" "$2" && exec env "$3" "$4" "$5" --exact "$6" --nocapture"#;
    let status = std::process::Command::new("sudo")
        .args(["-n", "unshare", "--mount", "--propagation", "private", "sh", "-c", script, "sh"])
        .arg(&socket)
        .arg(&jail_socket)
        .arg(format!("{JAILED_SOCKET}={}", jail_socket.display()))
        .arg(format!("{OPEN_SOCKET}={}", open.display()))
        .arg(std::env::current_exe().unwrap())
        .arg(test)
        .status()
        .unwrap();
    assert!(status.success(), "the other user connected, or failed to try: {status}");
    let metadata = std::fs::metadata(&socket).unwrap();
    // SAFETY: A plain system call.
    assert_eq!((metadata.mode() & 0o777, metadata.uid()), (0o600, unsafe { libc::getuid() }));
}

/// Root in a mount namespace of its own, it turns into the jail's user: what it runs needn't be
/// that user's to run. Then the kernel refuses it the daemon's socket, and not the one beside it.
fn as_another_user(socket: &Path) {
    use std::os::unix::net::UnixStream;
    // SAFETY: Plain system calls: this process does nothing else as root.
    unsafe {
        assert_eq!(libc::setgroups(0, std::ptr::null()), 0);
        assert_eq!(libc::setgid(JAIL_USER), 0);
        assert_eq!(libc::setuid(JAIL_USER), 0);
    }
    UnixStream::connect(std::env::var_os(OPEN_SOCKET).unwrap()).unwrap();
    let refused = UnixStream::connect(socket).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
}
