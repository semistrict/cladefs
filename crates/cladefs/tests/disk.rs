// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-94: the host's disk a tenant takes, what its cells' files take, counted as they write
//! (TASK-539.5), and limited at runtime through the control API, a tenant's and every tenant's
//! together. Past a limit the tenant's writes fail with ENOSPC, and the cells its VMs make; the
//! daemon goes on, and other tenants' writes do; what leaves the disk lets the tenant write again.
//! Its metrics show each tenant's disk by its ID.

#![cfg(target_os = "linux")]

mod common;

use cladefs::cladefsd::disk::STORE_MADE;
use cladefs::cladefsd::metrics::render;
use cladefs::{CellClient, CellId, Errno, ExtentId, VolatileSlice};
use common::daemon::{CAPACITY, Daemon, failure};
use common::pattern;
use std::path::Path;
use std::time::{Duration, Instant};
use tonic::Code;

/// The extents VMs write, of a MiB each.
const EXTENT: u64 = 1 << 20;

/// What a write of one, a new extent's, is admitted as until its commit: it, and what its cell's
/// own work may add for it as raw data and as an extent open (doc-22); once committed, the open
/// extent's share goes.
const ADMITTED: u64 = (1 + 2 * bucket_cell::host_disk::RESERVE_FACTOR) * EXTENT;

/// Extent `seed`'s bytes, which don't compress: as a closed segment's framing would otherwise
/// shrink them.
fn contents(seed: u64) -> Vec<u8> {
    (0..EXTENT / (64 << 10)).flat_map(|part| pattern(seed << 8 | part)).collect()
}

/// Writes extent `sequence` of `client`, `seed`'s bytes.
fn write(client: &CellClient, sequence: u64, seed: u64) -> Result<(), Errno> {
    let extent = ExtentId { cell: client.cell(), sequence };
    client.write(extent, 0, &[VolatileSlice::from(&contents(seed)[..])])
}

/// The value of the metric `series`, as the daemon renders its metrics.
fn metric(series: &str) -> u64 {
    let rendered = render();
    let line = rendered.lines().find(|line| line.split(' ').next() == Some(series));
    let line = line.unwrap_or_else(|| panic!("no {series} in\n{rendered}"));
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

/// The bytes of a segment's log's header, a block (`local_cell::wal::LOG_RECORDS`).
const LOG_HEADER: u64 = 4096;

/// What a cell's files under `dir` take, as its footprint counts them (`local_cell::footprint`):
/// each file's length rounded up to a granule, once however many names it has, and each
/// directory under `dir` a granule.
fn counted(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    let granule = bucket_cell::host_disk::GRANULE;
    let mut inodes = std::collections::HashSet::new();
    let (mut bytes, mut dirs) = (0, vec![dir.to_owned()]);
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let (path, metadata) =
                (entry.as_ref().unwrap().path(), entry.unwrap().metadata().unwrap());
            if metadata.is_dir() {
                bytes += granule;
                dirs.push(path);
            } else if inodes.insert(metadata.ino()) {
                bytes += bucket_cell::host_disk::counted(metadata.len());
            }
        }
    }
    bytes
}

/// What the cells in a tenant's directory, `dir`, take, as their footprints count them: each
/// directory in it a cell.
fn cells(dir: &Path) -> u64 {
    let cells = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path());
    cells.filter(|cell| cell.is_dir()).map(|cell| counted(&cell)).sum()
}

/// Waits until `done` says so, polling, a minute at most: what it waits for, as an upload's
/// landing, the daemon does on its own.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < Duration::from_secs(60), "{what}: not within a minute");
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

/// A tenant whose VMs, forks of a base of its, write past its limit is refused with ENOSPC, while
/// another tenant writes on, past what the first's limit would allow, and the daemon's own VMs do;
/// and the first's VMs still commit. What it takes is what its cells' files do, as their footprints
/// count them: a fork of a base takes little of its own. Once what it wrote leaves the disk, freed
/// and uploaded, it writes again. Every tenant's limit refuses any tenant past it.
#[test]
fn tenants_past_their_disk_limits_are_refused_while_others_write() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let acme_socket = daemon.grant(1, None).unwrap();
    let globex_socket = daemon.grant(2, None).unwrap();
    let acme_dir = dir.path().join("disk0/cells").join(base32_id::Padded(1).to_string());

    // Acme's base, which its VMs' cells are forks of, uploaded, as its VM manager makes them.
    let vm = CellClient::connect_new(&acme_socket, 1, CAPACITY).unwrap();
    let base = CellId { tenant: 1, id: vm.cell() };
    for sequence in 1..=8 {
        write(&vm, sequence, sequence).unwrap();
    }
    vm.commit(None).unwrap();
    drop(vm);
    daemon.sync(base).unwrap();
    let forks: Vec<_> = (0..4).map(|_| daemon.fork(base, None).unwrap()).collect();
    // What it takes is what its cells' files do.
    let (taken, limit) = daemon.tenant_disk(Some(1)).unwrap();
    assert_eq!((taken, limit), (cells(&acme_dir), None));
    // Each fork takes little of its own, the base's data in the bucket: no more than a cell made
    // is admitted as.
    for &fork in &forks {
        let path = daemon.cell_path(fork);
        assert!(counted(&path) <= STORE_MADE, "{} takes {} bytes", path.display(), counted(&path));
    }

    // 16 MiB more, its VMs writing its forks, uncommitted, which none uploads: room for them as
    // admitted, past what its cells, open, take and reserve.
    let vms: Vec<_> =
        forks.iter().map(|&fork| CellClient::connect_cell(&acme_socket, fork).unwrap()).collect();
    let (open, _) = daemon.tenant_disk(Some(1)).unwrap();
    let limit = open + daemon.tenant_reserved(Some(1)) + 16 * ADMITTED;
    daemon.set_disk_limit(Some(1), Some(limit)).unwrap();
    assert_eq!(daemon.tenant_disk(Some(1)).unwrap(), (open, Some(limit)));
    let mut written = 0;
    let refused = loop {
        let vm = &vms[written as usize % vms.len()];
        match write(vm, 100 + written, 100 + written) {
            Ok(()) => written += 1,
            Err(errno) => break errno,
        }
    };
    assert_eq!((refused, written), (Errno::ENOSPC, 16));
    // What it takes, counted again, is what it wrote, and the base of each fork's write-ahead
    // log, which its cell makes as it opens to write (backlog doc-14, `wal`), and the header of
    // its open segment's log (TASK-301): its metrics name it by its ID, with its limit.
    let (full, _) = daemon.tenant_disk(Some(1)).unwrap();
    let wal = |fork: CellId| std::fs::metadata(daemon.cell_path(fork).join("wal")).unwrap().len();
    let wals: u64 = forks.iter().map(|&fork| bucket_cell::host_disk::counted(wal(fork))).sum();
    let opened = wals + forks.len() as u64 * LOG_HEADER;
    assert_eq!(
        (full, cells(&acme_dir)),
        (taken + 16 * EXTENT + opened, taken + 16 * EXTENT + opened)
    );
    let tenant = "{tenant=\"1\"}";
    assert_eq!(metric(&format!("cladefsd_tenant_disk_limit_bytes{tenant}")), limit);
    assert_eq!(metric(&format!("cladefsd_tenant_disk_bytes{tenant}")), full);
    // A cell its VM makes is refused too.
    let before_count = std::fs::read_dir(&acme_dir).unwrap().count();
    let made = CellClient::connect_new(&acme_socket, 1, CAPACITY);
    assert_eq!(made.map(drop).map_err(|error| error.errno()), Err(Some(Errno::ENOSPC)));
    assert_eq!(std::fs::read_dir(&acme_dir).unwrap().count(), before_count);

    // Globex writes, past what acme's limit would allow, as the daemon's own VMs do.
    let theirs = CellClient::connect_new(&globex_socket, 2, CAPACITY).unwrap();
    for sequence in 1..=24 {
        write(&theirs, sequence, 1000 + sequence).unwrap();
    }
    theirs.commit(None).unwrap();
    let (_, ours) = daemon.open_new(0, CAPACITY);
    write(&ours, 1, 2000).unwrap();
    ours.commit(None).unwrap();
    // And acme's VMs commit, refused as they write: their commits, sealing their extents, free the
    // room their extents open held, 16 times what a commit may copy of each, for six more at
    // least; and what follows them, framing and uploading what they wrote, frees the rest, as fast
    // as it goes, for all 16 at most.
    for vm in &vms {
        vm.commit(None).unwrap();
    }
    let open_share = ADMITTED - (1 + bucket_cell::host_disk::RESERVE_FACTOR) * EXTENT;
    let mut more = 0;
    let refused = loop {
        let vm = &vms[more as usize % vms.len()];
        match write(vm, 200 + more, 200 + more) {
            Ok(()) => more += 1,
            Err(errno) => break errno,
        }
    };
    assert_eq!(refused, Errno::ENOSPC);
    assert!((16 * open_share / ADMITTED..=16).contains(&more), "{more} more admitted");

    // What it wrote, freed and uploaded, leaves the disk: it writes again.
    let connected = vms.len();
    for (index, vm) in vms.iter().enumerate() {
        let freeing = |first: u64, count: u64| {
            (0..count).filter(move |n| *n as usize % connected == index).map(move |n| first + n)
        };
        for sequence in freeing(100, written).chain(freeing(200, more)) {
            vm.free(ExtentId { cell: vm.cell(), sequence }).unwrap();
        }
        vm.commit(None).unwrap();
    }
    until("acme's writes leaving its disk", || {
        daemon.tenant_disk(Some(1)).unwrap().0 <= taken + 4 * EXTENT
    });
    write(&vms[0], 1, 3000).unwrap();
    vms[0].commit(None).unwrap();

    // Lifted, it writes on; every tenant's limit, reached, refuses any: room for two writes past
    // what every tenant takes and reserves, and for what their uploads let go of meanwhile, which
    // frames and leaves their cells, a few more at most.
    daemon.set_disk_limit(Some(1), None).unwrap();
    let (every, none) = daemon.tenant_disk(None).unwrap();
    assert_eq!(none, None);
    let total = every + daemon.tenant_reserved(None) + 2 * ADMITTED;
    daemon.set_disk_limit(None, Some(total)).unwrap();
    assert_eq!(metric("cladefsd_disk_limit_bytes"), total);
    write(&vms[1], 1, 3001).unwrap();
    write(&theirs, 100, 3002).unwrap();
    let refused = (0..8).find(|n| write(&vms[2], 1 + n, 3003 + n) == Err(Errno::ENOSPC));
    assert!(refused.is_some(), "every tenant's limit refused none of acme's writes");
    assert_eq!(write(&theirs, 101, 3020), Err(Errno::ENOSPC));
    // The daemon's own VMs, of the default tenant, aren't counted.
    write(&ours, 2, 3005).unwrap();

    assert_eq!(failure(daemon.tenant_disk(Some(3))), (Code::NotFound, Errno::ENOENT));
}
