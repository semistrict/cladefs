// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Daemon-backed integration tests for `cladefs::memory_cell::MemoryCell`.

#![cfg(target_os = "linux")]

use cladefs::Errno;
use cladefs::Prefault;
use cladefs::memory_cell::MemoryCell;
use cladefs::pages::page_size;
use std::os::unix::fs::MetadataExt as _;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, mpsc};

/// `len` bytes of `byte` plus their offset modulo 251.
fn pattern(byte: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| byte ^ (i % 251) as u8).collect()
}

/// The bytes of host memory a window's private pages hold: its private memory's blocks, the
/// one memory object of this process named so of `size` bytes.
fn private_memory_held(size: u64) -> u64 {
    let held: Vec<u64> = (std::fs::read_dir("/proc/self/fd").unwrap())
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            // A descriptor closed since it was listed: not the private memory's.
            let link = std::fs::read_link(&path).ok()?;
            let named = link.to_string_lossy().starts_with("/memfd:cladefs-private-pages");
            let metadata = std::fs::metadata(&path).ok().filter(|_| named)?;
            (metadata.len() == size).then(|| metadata.blocks() * 512)
        })
        .collect();
    assert_eq!(held.len(), 1, "a window's private memory: {held:?}");
    held[0]
}

/// Pauses the mapper after it names and installs a node's pages and before it wakes
/// the faults waiting for the node. A write fault can use an already named page meanwhile.
struct PausePrefault {
    first: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Prefault for PausePrefault {
    fn prefault(&self, _: u64, _: u64) {
        let first = self.first.lock().unwrap().take();
        if let Some((entered, resume)) = first {
            entered.send(()).unwrap();
            resume.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        }
    }
}

// board 8497: a window's pages the guest gave back, a balloon's inflation's, reclaimed, are
// private no more, their memory emptied, the host's again: the guest reads the image there
// again, as of the cell's last commit, and its next write makes a page private anew. A node no
// fault reached is left to its first fault, which maps it whole. Discarded with madvise, as
// Firecracker's balloon discarded them, they stayed private, their memory held.
#[test]
fn reclaimed_pages_end_private_and_read_the_image() {
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
    // the environment yet.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::server::serve(listener));
    let ps = page_size();
    // Three nodes of the tree's, the last part of one.
    const PAGES: u64 = 400;
    let size = PAGES * ps;
    let memory = MemoryCell::open(&socket, &dir.path().join("memory"), size).unwrap();
    let image: Vec<u8> = (0..PAGES).flat_map(|page| pattern(page as u8, ps as usize)).collect();
    memory.write(0, &image).unwrap();
    memory.commit().unwrap();
    let version = memory.make_version().unwrap();
    let page = memory.page_size();
    memory.close().unwrap();
    let fork = dir.path().join("fork");
    let memory_cell = MemoryCell::open_forked(&socket, &fork, version, size).unwrap();
    let reclaimed = memory_cell.reclaim(0, ps);
    assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL), "with no window");
    // SAFETY: Anonymous memory, reserving none, the test's: the window's.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    let base = NonNull::new(base.cast::<u8>()).unwrap();
    let (entered, named) = mpsc::channel();
    let (finished, resume) = mpsc::channel();
    let prefault = Arc::new(PausePrefault { first: Mutex::new(Some((entered, resume))) });
    // A failure starting the mapper leaves the cell with no window and permits a retry.
    fail::cfg("memory_cell::spawn_mapper", "1*return").unwrap();
    // SAFETY: The memory is the test's and outlives the memory cell.
    let failed = unsafe { memory_cell.window(base, size, None) };
    fail::remove("memory_cell::spawn_mapper");
    assert_eq!(failed.unwrap_err().errno(), Some(Errno::EAGAIN));
    let reclaimed = memory_cell.reclaim(0, ps);
    assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL));
    // SAFETY: The memory is the test's, and outlives the memory_cell (unmapped once it goes).
    unsafe { memory_cell.window(base, size, Some(prefault)) }.unwrap();
    let address = base.as_ptr() as usize;
    let at = |page: u64| address + (page * ps) as usize;
    // The window's bytes of pages `pages`, as the guest reads them.
    let read = |pages: std::ops::Range<u64>| -> Vec<u8> {
        // SAFETY: The window's memory, whose faults its memory_cell serves.
        (at(pages.start)..at(pages.end))
            .map(|addr| unsafe { (addr as *const u8).read_volatile() })
            .collect()
    };
    let image_of = |pages: std::ops::Range<u64>| {
        image[(pages.start * ps) as usize..(pages.end * ps) as usize].to_vec()
    };
    cladefs::guest_thread_started();
    let store = |page: u64, byte: u8| {
        // SAFETY: As above, a write of the guest's.
        unsafe { (at(page) as *mut u8).write_volatile(byte) }
    };
    std::thread::scope(|threads| {
        let first = threads.spawn(|| read(0..1));
        named.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        // This store copies locally because the mapper has already named the page.
        store(5, 0xee);
        let private = memory_cell.private_runs();
        let reclaimed = memory_cell.reclaim(5 * ps, ps);
        let remaining = memory_cell.private_runs();
        finished.send(()).unwrap();
        assert_eq!(first.join().unwrap(), image_of(0..1));
        assert_eq!(private.unwrap(), vec![5 * ps..6 * ps]);
        reclaimed.unwrap();
        assert!(remaining.unwrap().is_empty(), "reclaim skipped a named page");
    });
    // The first two nodes reached, the last not.
    assert!(read(0..340) == image_of(0..340), "the window holds the image");
    for page in [5, 6, 7, 300] {
        store(page, 0xee);
    }
    assert_eq!(memory_cell.private_runs().unwrap(), vec![5 * ps..8 * ps, 300 * ps..301 * ps]);
    assert_eq!(private_memory_held(size), 4 * ps);

    memory_cell.reclaim(4 * ps, 4 * ps).unwrap();
    assert_eq!(memory_cell.private_runs().unwrap(), vec![300 * ps..301 * ps]);
    assert_eq!(private_memory_held(size), ps);
    assert!(read(4..8) == image_of(4..8), "the pages reclaimed read the image");
    store(6, 0xdd);
    assert_eq!(memory_cell.private_runs().unwrap(), vec![6 * ps..7 * ps, 300 * ps..301 * ps]);
    assert_eq!(read(6..7)[0], 0xdd);

    // Over the last node, which no fault reached, and a page of the one before it.
    memory_cell.reclaim(300 * ps, 100 * ps).unwrap();
    assert_eq!(memory_cell.private_runs().unwrap(), vec![6 * ps..7 * ps]);
    assert_eq!(private_memory_held(size), ps);
    assert!(read(300..400) == image_of(300..400), "the last node reads the image");

    // On hosts with pages larger than the cell's (e.g. 16 KiB arm64), reclaiming one cell page
    // inside a private host page restores only that cell page while preserving sibling private
    // cell pages in the same host page; a subsequent write to the reclaimed cell page re-tracks
    // COW ownership so later sibling reclaims never clobber it (review 100 finding 342).
    if ps > page {
        memory_cell.reclaim(6 * ps + page, page).unwrap();
        assert_eq!(memory_cell.private_runs().unwrap(), vec![6 * ps..7 * ps]);
        assert_eq!(read(6..7)[0], 0xdd, "sibling cell page stayed private");
        assert_eq!(
            &read(6..7)[page as usize..(2 * page) as usize],
            &image_of(6..7)[page as usize..(2 * page) as usize],
            "reclaimed cell page reads the image again"
        );
        // Write into the reclaimed cell page (slot 1), then reclaim sibling slot 2: slot 1's
        // new write must survive rather than being overwritten by old image bytes.
        // SAFETY: Within the window's live mapping, a write of the guest's.
        unsafe { ((at(6) + page as usize) as *mut u8).write_volatile(0x55) };
        if ps >= 3 * page {
            memory_cell.reclaim(6 * ps + 2 * page, page).unwrap();
            assert_eq!(read(6..7)[0], 0xdd, "slot 0 stayed private");
            assert_eq!(
                read(6..7)[page as usize],
                0x55,
                "write to previously reclaimed slot 1 survived sibling reclaim"
            );
            assert_eq!(
                &read(6..7)[(2 * page) as usize..(3 * page) as usize],
                &image_of(6..7)[(2 * page) as usize..(3 * page) as usize],
                "reclaimed slot 2 reads the image again"
            );
        }
    }

    let reclaimed = memory_cell.reclaim(page, page / 2);
    assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL), "half a page");
    let reclaimed = memory_cell.reclaim(399 * ps, 2 * ps);
    assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL), "past the memory");
    memory_cell.close().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(base.as_ptr().cast(), size as usize) }, 0);
}

// TASK-456.4: snapshotting a memory cell's window writes only its private pages (zeroed ones
// discarded as holes), commits, reclaims them back to the newly committed extents so no
// private pages remain, and supports taking another snapshot after resuming writes.
#[test]
fn snapshot_window_writes_private_pages_reclaims_and_supports_subsequent_snapshots() {
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
    // the environment yet.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::server::serve(listener));
    let ps = page_size();
    const PAGES: u64 = 32;
    let size = PAGES * ps;
    let base_cell = MemoryCell::open(&socket, &dir.path().join("base"), size).unwrap();
    let mut expected: Vec<u8> =
        (0..PAGES).flat_map(|page| pattern(page as u8, ps as usize)).collect();
    base_cell.write(0, &expected).unwrap();
    base_cell.commit().unwrap();
    let v1 = base_cell.make_version().unwrap();
    let page = base_cell.page_size();
    base_cell.close().unwrap();

    let fork = dir.path().join("fork");
    let memory_cell = MemoryCell::open_forked(&socket, &fork, v1, size).unwrap();
    // SAFETY: Anonymous memory, reserving none, the test's: the window's.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    let base = NonNull::new(base.cast::<u8>()).unwrap();
    // SAFETY: The memory is the test's, and outlives `memory_cell`.
    unsafe { memory_cell.window(base, size, None) }.unwrap();
    let per_host_page = ps / page;
    cladefs::guest_thread_started();
    let write_page = |p: u64, data: &[u8]| {
        // SAFETY: Within the window's mapping of `size` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                base.as_ptr().add((p * ps) as usize),
                ps as usize,
            );
        }
    };
    // Modify page 2 with new data, and zero page 5 completely (should become a hole).
    let page2 = pattern(0x99, ps as usize);
    let zeros = vec![0u8; ps as usize];
    write_page(2, &page2);
    write_page(5, &zeros);
    expected[2 * ps as usize..3 * ps as usize].copy_from_slice(&page2);
    expected[5 * ps as usize..6 * ps as usize].copy_from_slice(&zeros);
    assert_eq!(memory_cell.private_pages().unwrap(), 2 * per_host_page);

    // SAFETY: `base` is the window's mapping and no other thread touches it.
    let v2 = unsafe { memory_cell.snapshot_window(base) }.unwrap();
    assert_eq!(memory_cell.private_pages().unwrap(), 0, "private pages reclaimed after snap");
    assert_eq!(private_memory_held(size), 0, "private memfd emptied after snap");

    // Verify a child forked from v2 reads the exact snapshot bytes (and still matches expected).
    let child1 = MemoryCell::open_forked(&socket, &dir.path().join("child1"), v2, size).unwrap();
    let mut got = vec![0u8; size as usize];
    child1.read(0, &mut got).unwrap();
    assert!(got == expected, "v2 bytes match");
    child1.close().unwrap();

    // Resume writing on the same window, then take a second snapshot v3.
    let page9 = pattern(0x42, ps as usize);
    write_page(9, &page9);
    expected[9 * ps as usize..10 * ps as usize].copy_from_slice(&page9);
    assert_eq!(memory_cell.private_pages().unwrap(), per_host_page);
    // SAFETY: `base` is the window's mapping and no other thread touches it.
    let v3 = unsafe { memory_cell.snapshot_window(base) }.unwrap();
    assert_eq!(memory_cell.private_pages().unwrap(), 0);

    let child2 = MemoryCell::open_forked(&socket, &dir.path().join("child2"), v3, size).unwrap();
    child2.read(0, &mut got).unwrap();
    assert!(got == expected, "v3 bytes match");
    child2.close().unwrap();

    memory_cell.close().unwrap();
    // SAFETY: Unmaps the test's memory after `memory_cell` dropped.
    assert_eq!(unsafe { libc::munmap(base.as_ptr().cast(), size as usize) }, 0);
}

#[test]
fn ephemeral_memory_cell_suspends_resumes_resuspends_and_deletes_without_bucket_uploads() {
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
    // the environment yet.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///ephemeral-memory-cells");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::server::serve(listener));
    let ps = page_size();
    const PAGES: u64 = 32;
    let size = PAGES * ps;
    let cell_dir = dir.path().join("ephemeral-mem");

    // Opening missing ephemeral memory cell fails with ENOENT.
    assert_eq!(
        MemoryCell::open_ephemeral_existing(&socket, &cell_dir, None, size)
            .err()
            .and_then(|err| err.errno()),
        Some(Errno::ENOENT)
    );

    // Cold-boot suspend: create standalone ephemeral memory cell and populate guest RAM.
    let mut expected: Vec<u8> =
        (0..PAGES).flat_map(|page| pattern(page as u8, ps as usize)).collect();
    let cold = MemoryCell::open_ephemeral(&socket, &cell_dir, size).unwrap();
    assert!(cold.is_ephemeral());
    assert_eq!(cold.make_version().unwrap_err().errno(), Some(Errno::ENOTSUP));
    let cell_id = cladefs::CellId { tenant: cold.tenant(), id: cold.cell() };
    let page = cold.page_size();
    // Record baseline after any one-time BucketKms tenant KEK creation (`Upload::Other`);
    // cell data (`Segments`/`Extents`/`Checkpoints`) must stay strictly zero, and zero
    // further bytes of any kind may be uploaded during writes, commits, re-suspend, or delete.
    let bucket = cladefs::cladefsd::cells::daemon_bucket().unwrap();
    let cell_data_uploads = |b: &bucket_cell::object_bucket::RoutedBucket| -> u64 {
        let stats = b.stats();
        stats.uploaded(bucket_cell::object_bucket::Upload::Segments)
            + stats.uploaded(bucket_cell::object_bucket::Upload::Extents)
            + stats.uploaded(bucket_cell::object_bucket::Upload::Checkpoints)
    };
    let uploaded_bytes = |b: &bucket_cell::object_bucket::RoutedBucket| -> u64 {
        let stats = b.stats();
        bucket_cell::object_bucket::Upload::ALL.into_iter().map(|kind| stats.uploaded(kind)).sum()
    };
    let baseline_uploads = uploaded_bytes(&bucket);
    assert_eq!(cell_data_uploads(&bucket), 0, "ephemeral create must upload no cell data");

    cold.write_sparse(0, &expected).unwrap();
    cold.commit().unwrap();
    cold.close().unwrap();

    assert_eq!(cell_data_uploads(&bucket), 0, "ephemeral memory cell must upload no cell data");
    assert_eq!(
        uploaded_bytes(&bucket),
        baseline_uploads,
        "ephemeral memory cell write/commit/close must upload nothing to bucket"
    );

    // Mismatched expected CellId is rejected with EEXIST.
    let wrong_id = cladefs::CellId { tenant: cell_id.tenant, id: cell_id.id + 1 };
    assert_eq!(
        MemoryCell::open_ephemeral_existing(&socket, &cell_dir, Some(wrong_id), size)
            .err()
            .and_then(|err| err.errno()),
        Some(Errno::EEXIST)
    );

    // Resume in place via open_ephemeral_existing + uffd window.
    let resumed =
        MemoryCell::open_ephemeral_existing(&socket, &cell_dir, Some(cell_id), size).unwrap();
    assert!(resumed.is_ephemeral());
    // SAFETY: Anonymous memory, reserving none, the test's: the window's.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    let base = NonNull::new(base.cast::<u8>()).unwrap();
    // SAFETY: The memory is the test's and outlives `resumed`.
    unsafe { resumed.window(base, size, None) }.unwrap();
    cladefs::guest_thread_started();
    let per_host_page = ps / page;
    // Read page 0 lazily via userfaultfd and verify contents.
    // SAFETY: Within the window's mapping of `size` bytes.
    let page0: Vec<u8> =
        (0..ps as usize).map(|i| unsafe { base.as_ptr().add(i).read_volatile() }).collect();
    assert_eq!(page0, &expected[..ps as usize]);

    // Modify page 3 with new data and zero page 7 completely (hole).
    let page3 = pattern(0xab, ps as usize);
    let zeros = vec![0u8; ps as usize];
    // SAFETY: Within the window's mapping of `size` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            page3.as_ptr(),
            base.as_ptr().add((3 * ps) as usize),
            ps as usize,
        );
        std::ptr::copy_nonoverlapping(
            zeros.as_ptr(),
            base.as_ptr().add((7 * ps) as usize),
            ps as usize,
        );
    }
    expected[3 * ps as usize..4 * ps as usize].copy_from_slice(&page3);
    expected[7 * ps as usize..8 * ps as usize].copy_from_slice(&zeros);
    assert_eq!(resumed.private_pages().unwrap(), 2 * per_host_page);

    // Re-suspend in place without creating any bucket version.
    // SAFETY: `base` is the window's mapping and no other thread touches it.
    let snapped = unsafe { resumed.snapshot_window_ephemeral(base) }.unwrap();
    assert_eq!(snapped, cell_id);
    assert_eq!(resumed.private_pages().unwrap(), 0);
    assert_eq!(private_memory_held(size), 0);
    resumed.close().unwrap();
    // SAFETY: Unmaps the test's memory after `resumed` dropped.
    assert_eq!(unsafe { libc::munmap(base.as_ptr().cast(), size as usize) }, 0);

    // Reopen again and verify every single byte persisted across re-suspend.
    let reopened =
        MemoryCell::open_ephemeral_existing(&socket, &cell_dir, Some(cell_id), size).unwrap();
    let mut got = vec![0u8; size as usize];
    reopened.read(0, &mut got).unwrap();
    assert!(got == expected, "re-suspended ephemeral memory cell bytes differ");
    reopened.close().unwrap();

    // Still zero bucket uploads across cold-boot suspend, resume, re-suspend, and reopen.
    assert_eq!(cell_data_uploads(&bucket), 0, "ephemeral memory cell uploaded cell data");
    assert_eq!(
        uploaded_bytes(&bucket),
        baseline_uploads,
        "ephemeral memory cell uploaded bytes to bucket"
    );

    // Deleting a standalone ephemeral memory cell succeeds without CLADEFSD_EPHEMERAL.
    // Wait for the daemon thread's asynchronous close of `reopened` to release the cell
    // directory claim before deleting directly (board 12317).
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        local_cell::unclaimed_on(cladefs::Host::process().local(), &cell_dir).await.unwrap();
        cladefs::OpenCell::delete_on(cladefs::Host::process(), &cell_dir, Some(&bucket))
            .await
            .unwrap();
    });
    assert!(!cell_dir.exists());
    assert_eq!(
        uploaded_bytes(&bucket),
        baseline_uploads,
        "deleting standalone ephemeral memory cell must upload nothing to bucket"
    );
}
