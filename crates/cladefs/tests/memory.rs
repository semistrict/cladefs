// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-50.3: the daemon's memory, limited at runtime through the control API, a tenant's and
//! every tenant's together. Past a limit the daemon evicts the clean pages read longest ago; with
//! none left to evict, what would take more fails with ENOMEM, and the daemon goes on; cladefs: but
//! a page-in past every tenant's together, which waits for a page (board 5662). Its metrics
//! show each tenant's memory by its ID.

#![cfg(target_os = "linux")]

mod common;

use cladefs::cladefsd::metrics::render;
use cladefs::slabs::SLAB;
use cladefs::{CellClient, CellId, Errno, ExtentId, VolatileSlice};
use common::daemon::{CAPACITY, Daemon, failure};
use common::{memory, pattern, read_window};
use tonic::Code;

/// What the limits are set in: two slabs, each a part's at most.
const CHUNK: u64 = 2 * SLAB;

/// The extents a VM writes, of [`EXTENT`] bytes each.
const EXTENTS: u64 = 8;
const EXTENT: u64 = 1 << 20;

/// Extent `sequence`'s bytes.
fn contents(sequence: u64) -> Vec<u8> {
    (0..EXTENT / (64 << 10)).flat_map(|part| pattern(sequence << 8 | part)).collect()
}

/// Writes [`EXTENTS`] extents to `client`, commits, and reads each through a window mapping them
/// all, read-only, twice over: [`EXTENTS`] MiB of pages cached in the daemon's memory, as it
/// holds them. Returns the window.
fn read_through(client: &CellClient) -> std::ptr::NonNull<u8> {
    let extent = |sequence| ExtentId { cell: client.cell(), sequence };
    for sequence in 1..=EXTENTS {
        client.write(extent(sequence), 0, &[VolatileSlice::from(&contents(sequence)[..])]).unwrap();
    }
    client.commit(None).unwrap();
    let size = EXTENTS * EXTENT;
    let window = memory((size / cladefs::pages::page_size()) as usize);
    // SAFETY: The memory is this test's, which it leaks: the window maps it while the process
    // lives.
    unsafe { client.create_dax_window_in_host_memory(window, size, None, 1) }.unwrap();
    for sequence in 1..=EXTENTS {
        client.map(extent(sequence), 0, EXTENT, (sequence - 1) * EXTENT).unwrap();
    }
    for round in 0..2 {
        for sequence in 1..=EXTENTS {
            let at = ((sequence - 1) * EXTENT) as usize;
            // Evictions change the window's memory as it is read.
            // SAFETY: The window's memory, whose faults its cell serves.
            let mapped = read_window(unsafe { window.as_ptr().add(at) }, EXTENT as usize);
            let expected = contents(sequence);
            if mapped != expected {
                panic!("round {round}: extent {sequence} maps {}", whose(&mapped, &expected));
            }
        }
    }
    window
}

/// What `mapped`, an extent's bytes read through a window, holds where it is not `expected`: the
/// first page that differs, and whether it is zeros or which extent's page.
fn whose(mapped: &[u8], expected: &[u8]) -> String {
    let page = cladefs::pages::page_size() as usize;
    let differs = |at: &usize| mapped[*at..at + page] != expected[*at..at + page];
    let at = (0..mapped.len()).step_by(page).find(differs);
    let Some(at) = at else { return "the expected bytes".into() };
    let found = &mapped[at..at + page];
    if found.iter().all(|byte| *byte == 0) {
        return format!("zeros at {at:#x}");
    }
    for sequence in 1..=EXTENTS {
        let other = contents(sequence);
        let holds = |from: &usize| other[*from..from + page] == *found;
        if let Some(from) = (0..other.len()).step_by(page).find(holds) {
            return format!("extent {sequence}'s page at {from:#x} at {at:#x}");
        }
    }
    format!("bytes of no extent's at {at:#x}")
}

/// The value of the metric `name`, with its labels, in the daemon's metrics.
fn metric(name: &str) -> u64 {
    let metrics = render();
    let line = (metrics.lines())
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no {name} in {metrics}"));
    line.parse().unwrap()
}

/// Asserts that the memory the metric `name` shows is within `limit`, read past it. Within it, not
/// at it: the pages relief evicted come back as the VM process unmaps them, which may be after the
/// last page-in, and a slab they empty then goes back, with nothing to take it again (TASK-161).
fn assert_within(name: &str, limit: u64) {
    let held = metric(name);
    assert!(held <= limit, "{name}: {held} bytes, past the limit of {limit}");
}

/// A tenant whose VM reads through its window twice what its limit holds keeps within it, its
/// pages read longest ago evicted, read again as they are touched; its metrics name it by its ID,
/// in hexadecimal.
/// ShrinkMemory frees its pages, and returns once they are, its VM's window having unmapped them.
#[test]
fn tenants_past_their_limits_evict_then_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    // Limited before it is granted, as it is made.
    daemon.set_memory_limit(Some(0xac), Some(2 * CHUNK)).unwrap();
    let socket = daemon.grant(0xac, None).unwrap();
    let vm = CellClient::connect_new(&socket, 0xac, CAPACITY).unwrap();
    let cell = CellId { tenant: 0xac, id: vm.cell() };
    read_through(&vm);

    let tenant = "{tenant=\"5c\"}";
    assert_eq!(metric(&format!("cladefsd_tenant_memory_limit_bytes{tenant}")), 2 * CHUNK);
    assert_within(&format!("cladefsd_tenant_memory_bytes{tenant}"), 2 * CHUNK);

    assert_eq!(daemon.shrink_memory(Some(0xac), 0).unwrap(), 0);
    assert_eq!(metric(&format!("cladefsd_tenant_memory_bytes{tenant}")), 0);
    assert_eq!(failure(daemon.shrink_memory(Some(0xb), 0)), (Code::NotFound, Errno::ENOENT));
    // The default tenant has no limit of its own: every tenant's is its.
    let refused = daemon.set_memory_limit(Some(0), Some(2 * CHUNK));
    assert_eq!(failure(refused), (Code::InvalidArgument, Errno::EINVAL));
    assert_eq!(daemon.stat(cell).unwrap().open, true);
}

/// VMs of the daemon's own socket, and of tenants, read past the limit of every tenant's memory
/// together: the daemon keeps within it, evicting any tenant's pages; and shrinks below it.
#[test]
fn the_daemon_past_its_limit_evicts_any_tenants_pages() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    daemon.set_memory_limit(None, Some(4 * CHUNK)).unwrap();
    assert_eq!(metric("cladefsd_memory_limit_bytes"), 4 * CHUNK);
    let socket = daemon.grant(1, None).unwrap();
    let theirs = CellClient::connect_new(&socket, 1, CAPACITY).unwrap();
    read_through(&theirs);
    let (_, ours) = daemon.open_new(0, CAPACITY);
    read_through(&ours);
    assert_within("cladefsd_memory_bytes", 4 * CHUNK);

    // A part at a time, a slab's at most, until within it: less than a slab under it.
    let held = daemon.shrink_memory(None, CHUNK).unwrap();
    assert!((CHUNK - SLAB + 1..=CHUNK).contains(&held), "shrunk to {held} bytes");
    assert_eq!(metric("cladefsd_memory_bytes"), held);
}
