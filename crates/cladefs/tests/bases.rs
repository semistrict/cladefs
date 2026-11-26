// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-136.5: forks attached to cells through the control API: a VM reads and maps what they
//! hold, read-only, through its one device, as a stopped cell's next VM does, and a fork's, and a
//! tenant reaches no other tenant's.

// cladefs: the daemon's, on Linux and macOS (board 13065).
#![cfg(not(madsim))]

mod common;

use cladefs::cladefsd::control::proto::{AttachBaseRequest, MakePublicRequest};
use cladefs::{CellClient, CellId, Errno, ExtentId, VolatileSliceMut};
use common::daemon::{CAPACITY, Daemon, failure, write};
use common::{copies, pattern};
use tonic::Code;

fn attach(daemon: &mut Daemon, cell: CellId, base: CellId) -> Result<(), tonic::Status> {
    let request = AttachBaseRequest {
        tenant: cell.tenant,
        cell: cell.id,
        base_tenant: base.tenant,
        base_cell: base.id,
    };
    daemon.runtime.block_on(daemon.client.attach_base(request)).map(drop)
}

fn make_public_attach_only(daemon: &mut Daemon, cell: CellId) -> Result<(), tonic::Status> {
    let request = MakePublicRequest { tenant: cell.tenant, cell: cell.id, attach_only: true };
    daemon.runtime.block_on(daemon.client.make_public(request)).map(drop)
}

fn read(client: &CellClient, extent: ExtentId) -> Result<Vec<u8>, Errno> {
    let mut data = pattern(0);
    data.fill(0);
    client.read(extent, 0, &mut [VolatileSliceMut::from(&mut data[..])])?;
    Ok(data)
}

/// A VM's cell holding `pattern(seed)` in its first extent, and the generation forked from it,
/// which outlasts the VM: the extent, the publisher's cell ID, and the fork's cell ID.
fn publish(daemon: &mut Daemon, seed: u64) -> (ExtentId, CellId, CellId) {
    let (publisher, vm) = daemon.open_new(0, CAPACITY);
    let extent = ExtentId { cell: vm.cell(), sequence: 1 };
    write(&vm, &pattern(seed), b"published");
    let fork = daemon.fork(publisher, None).unwrap();
    (extent, publisher, fork)
}

/// A VM reads what is attached to its cell as it runs, and nothing before, nor of what isn't.
#[test]
fn vms_read_what_is_attached_as_they_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (extent, _pub1, base) = publish(&mut daemon, 1);
    let (other_extent, _pub2, _other) = publish(&mut daemon, 2);
    let (sandbox, vm) = daemon.open_new(0, CAPACITY);
    assert_eq!(read(&vm, extent), Err(Errno::ENOENT));

    attach(&mut daemon, sandbox, base).unwrap();
    assert!(read(&vm, extent).unwrap() == pattern(1), "the VM reads other bytes");
    assert_eq!(read(&vm, other_extent), Err(Errno::ENOENT));
}

/// What is attached to a cell stopped is there for its next VM, as for the VM after, for its
/// forks' and for a VM restored from a version of it.
#[test]
fn cells_keep_what_is_attached() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (extent, _pub1, base) = publish(&mut daemon, 3);
    let (sandbox, vm) = daemon.open_new(0, CAPACITY);
    write(&vm, &pattern(4), b"sandbox");
    drop(vm);
    daemon.sync(sandbox).unwrap();

    attach(&mut daemon, sandbox, base).unwrap();
    let vm = daemon.open(sandbox);
    assert!(read(&vm, extent).unwrap() == pattern(3), "the VM reads other bytes");
    drop(vm);
    daemon.sync(sandbox).unwrap();
    let vm = daemon.open(sandbox);
    assert!(read(&vm, extent).unwrap() == pattern(3), "the VM after reads other bytes");

    let fork = daemon.fork(sandbox, None).unwrap();
    let forked = daemon.open(fork);
    assert!(read(&forked, extent).unwrap() == pattern(3), "the fork's VM reads other bytes");
    drop(forked);

    let version = vm.make_version().unwrap();
    drop(vm);
    daemon.sync(sandbox).unwrap();
    let vm_restored = CellClient::connect_forked_cell(&daemon.vms, 0, version, CAPACITY).unwrap();
    assert!(read(&vm_restored, extent).unwrap() == pattern(3), "the restored VM reads other bytes");
}

/// A cell that isn't public attaches only to cells of its tenant, and a public generation to
/// any, whose VMs share its pages, of the default arena, once.
#[test]
fn tenants_reach_their_own_bases_and_public_ones() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let acme_socket = daemon.grant(1, None).unwrap();
    let globex_socket = daemon.grant(2, None).unwrap();

    // A private generation of acme's.
    let vm = CellClient::connect_new(&acme_socket, 1, CAPACITY).unwrap();
    let acme_pub = CellId { tenant: 1, id: vm.cell() };
    let private = ExtentId { cell: vm.cell(), sequence: 1 };
    write(&vm, &pattern(5), b"private");
    let generation = daemon.fork(acme_pub, None).unwrap();

    let acme_vm = CellClient::connect_new(&acme_socket, 1, CAPACITY).unwrap();
    let acme_cell = CellId { tenant: 1, id: acme_vm.cell() };
    let globex_vm = CellClient::connect_new(&globex_socket, 2, CAPACITY).unwrap();
    let globex_cell = CellId { tenant: 2, id: globex_vm.cell() };
    let refused = attach(&mut daemon, globex_cell, generation);
    assert_eq!(failure(refused), (Code::PermissionDenied, Errno::EACCES));
    assert_eq!(read(&globex_vm, private), Err(Errno::ENOENT));
    attach(&mut daemon, acme_cell, generation).unwrap();
    assert!(read(&acme_vm, private).unwrap() == pattern(5), "acme reads other bytes");

    // A public one, of a cell of the default tenant's that goes on being written.
    let (public, publisher, base) = publish(&mut daemon, 6);
    make_public_attach_only(&mut daemon, base).unwrap();
    assert!(daemon.stat(base).unwrap().public);
    for (cell_id, vm) in [(acme_cell, &acme_vm), (globex_cell, &globex_vm)] {
        attach(&mut daemon, cell_id, base).unwrap();
        assert!(read(vm, public).unwrap() == pattern(6), "a tenant reads other bytes");
    }
    let writer = daemon.open(publisher);
    let next = ExtentId { cell: writer.cell(), sequence: 2 };
    writer.write(next, 0, &[cladefs::VolatileSlice::from(&pattern(7)[..])]).unwrap();
    writer.commit(None).unwrap();
    let memory = cladefs::pages::physical_memory().unwrap();
    assert_eq!(copies(std::os::fd::AsFd::as_fd(&*memory), &pattern(6)), 1, "of one copy");
    assert_eq!(copies(acme_vm.physical_memory(), &pattern(6)), 0, "in acme's arena");
    assert_eq!(copies(globex_vm.physical_memory(), &pattern(6)), 0, "in globex's arena");
}

/// A generation goes as a cell does, once no VM reads through to it, and stays until then
/// whatever its cell does.
#[test]
fn generations_go_once_nothing_reads_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (extent, publisher, base) = publish(&mut daemon, 8);
    let moved_on = daemon.open(publisher);
    moved_on.free(extent).unwrap();
    moved_on.commit(None).unwrap();
    drop(moved_on);
    daemon.sync(publisher).unwrap();

    let (sandbox, vm) = daemon.open_new(0, CAPACITY);
    attach(&mut daemon, sandbox, base).unwrap();
    assert!(read(&vm, extent).unwrap() == pattern(8), "the VM reads other bytes");
    assert_eq!(failure(daemon.delete(base)), (Code::FailedPrecondition, Errno::EBUSY));

    drop(vm);
    daemon.wait_closed(sandbox);
    daemon.sync(sandbox).unwrap();
    daemon.delete(base).unwrap();
    // The cell it was attached to says so as it reads, and opens all the same.
    let vm = daemon.open(sandbox);
    assert_eq!(read(&vm, extent), Err(Errno::ENOENT));
}
