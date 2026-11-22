// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-539.13: the disk governor on a small real filesystem (backlog doc-22). An extent daemon's
//! cells and its part cache share a 1.5 GiB ext4, over a loop device on tmpfs, with a 256 MiB
//! headroom; its bucket is gated, as one out of reach is, while a tenant's VM fills the disk and a
//! neighbour takes a share of it. The filesystem never comes within a quarter of the headroom of
//! full; the part cache, copies of the bucket, gives back what the cells need first; the guest is
//! refused with ENOSPC on its writes alone, and its cell goes on committing; once the bucket is
//! back, what waited uploads, leaves the disk, and the guest writes again.
//!
//! Root's: it mounts the filesystem (`mount -o loop`), as the test host's runs do (`sudo`, under
//! fxjob); ignored otherwise.

#![cfg(target_os = "linux")]

mod common;

use cladefs::cladefsd::disk::{DiskConfig, start};
use cladefs::cladefsd::governor::{DiskHeadroom, INTERVAL};
use cladefs::{CellClient, Errno, ExtentId, VolatileSlice};
use common::daemon::Daemon;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;
/// The filesystem's size.
const SIZE: u64 = 1536 * MIB;
/// What the governor leaves free of it.
const HEADROOM: u64 = 256 * MIB;
/// The VMs' cells' capacities: more than the filesystem, which it is that fills.
const CAPACITY: u64 = 4 << 30;

/// An ext4 of [`SIZE`] over a loop device on tmpfs, mounted at a directory of its own: unmounted
/// and gone with it.
struct Disk {
    image: PathBuf,
    mount: tempfile::TempDir,
}

impl Disk {
    fn new() -> Self {
        let mount = tempfile::tempdir().unwrap();
        let image = PathBuf::from(format!("/dev/shm/cladefs-disk-full-{}.img", std::process::id()));
        let run = |command: &mut Command| {
            let status = command.status().unwrap();
            assert!(status.success(), "{command:?}: {status}");
        };
        run(Command::new("truncate").arg("-s").arg(SIZE.to_string()).arg(&image));
        run(Command::new("mkfs.ext4").args(["-q", "-F"]).arg(&image));
        run(Command::new("mount").args(["-o", "loop"]).arg(&image).arg(mount.path()));
        Self { image, mount }
    }

    fn path(&self) -> &Path {
        self.mount.path()
    }

    /// Its free bytes, an unprivileged writer's (`f_bavail`).
    fn free(path: &Path) -> u64 {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: statvfs is plain data, valid all zeros.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a C string and `stat` a statvfs, which the call fills.
        assert_eq!(unsafe { libc::statvfs(path.as_ptr(), &mut stat) }, 0);
        stat.f_bavail * stat.f_frsize
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        // Lazily: the daemon's threads hold its cells open until the process ends, the loop
        // device let go then.
        let _ = Command::new("umount").arg("-l").arg(self.mount.path()).status();
        let _ = std::fs::remove_file(&self.image);
    }
}

/// An extent of a MiB, `seed`'s bytes, which don't compress.
fn write(client: &CellClient, sequence: u64, seed: u64) -> Result<(), Errno> {
    let bytes: Vec<u8> = (0..16).flat_map(|part| common::pattern(seed << 8 | part)).collect();
    let extent = ExtentId { cell: client.cell(), sequence };
    client.write(extent, 0, &[VolatileSlice::from(&bytes[..])])
}

/// The value of the metric `series` in the daemon's metrics.
fn metric(series: &str) -> u64 {
    let rendered = cladefs::cladefsd::metrics::render();
    let line = rendered.lines().find(|line| line.split(' ').next() == Some(series));
    let line = line.unwrap_or_else(|| panic!("no {series} in\n{rendered}"));
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

/// Waits until `done`, a minute at most.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < Duration::from_secs(60), "{what}: not within a minute");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "root's: mounts an ext4 over a loop device"]
fn a_full_disk_refuses_guests_alone_and_the_cache_gives_way_first() {
    let disk = Disk::new();
    let root = disk.path().to_owned();
    // The bucket elsewhere, a filesystem of its own, as a bucket is.
    let dir = tempfile::tempdir().unwrap();
    let faults = dir.path().join("bucket-faults");
    std::fs::write(&faults, "failing=not").unwrap();
    std::fs::create_dir(root.join("cache")).unwrap();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe {
        std::env::set_var("CLADEFS_BUCKET_FAULTS", &faults);
        std::env::set_var("CLADEFSD_BUCKET", format!("file://{}/bucket/", dir.path().display()));
        std::env::set_var("CLADEFSD_PART_CACHE", root.join("cache"));
    }
    // Its governor first, leaving this test's headroom: the daemon's own start changes nothing.
    start(DiskConfig { headroom: Some(DiskHeadroom::Bytes(HEADROOM)), fixed: None }).unwrap();
    let mut daemon = Daemon::new(dir.path());
    let tenant = root.join("tenant");
    std::fs::create_dir(&tenant).unwrap();
    let socket = daemon.grant(1, &tenant, None).unwrap();
    let dev = std::fs::metadata(&root).unwrap().dev() as libc::dev_t;
    let device = format!("{{device=\"{}:{}\"}}", libc::major(dev), libc::minor(dev));

    // What the bucket has of a base, 600 MiB, adopted by the part cache as it uploads.
    let base = tenant.join("base");
    let vm = CellClient::connect(&socket, &base, CAPACITY).unwrap();
    // Committed as a guest's filesystem commits, every 16 MiB: what is written and not committed
    // is admitted with what its commit may copy of it, six times more (doc-22's reserve).
    for sequence in 1..=600 {
        write(&vm, sequence, sequence).unwrap();
        if sequence % 16 == 0 {
            vm.commit(None).unwrap();
        }
    }
    vm.commit(None).unwrap();
    drop(vm);
    daemon.sync(&base).unwrap();
    until("the cache holding the base", || metric("cladefsd_part_cache_bytes") >= 500 * MIB);
    // On a disk of its own, nothing but the daemon's: what it holds of others, its metadata.
    let other = metric(&format!("cladefsd_host_disk_other_bytes{device}"));
    assert!(other < 64 * MIB, "{other} bytes of others' on a disk of the daemon's");

    // The filesystem's free bytes, sampled every 10 ms from now on.
    let (sampling, lowest) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicU64::new(u64::MAX)));
    let sampler = {
        let (sampling, lowest, root) = (sampling.clone(), lowest.clone(), root.clone());
        std::thread::spawn(move || {
            while sampling.load(Ordering::Relaxed) {
                lowest.fetch_min(Disk::free(&root), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    // A neighbour takes 200 MiB: the cache gives it back within two of the governor's looks.
    let neighbour = std::fs::File::create(root.join("neighbour")).unwrap();
    // SAFETY: an fallocate of an open file.
    assert_eq!(unsafe { libc::fallocate(neighbour.as_raw_fd(), 0, 0, 200 * MIB as i64) }, 0);
    let took = Instant::now();
    until("the cache giving way to the neighbour", || {
        let spare = metric(&format!("cladefsd_host_disk_spare_bytes{device}"));
        metric("cladefsd_part_cache_bytes") <= spare
    });
    assert!(took.elapsed() <= 2 * INTERVAL + Duration::from_millis(50), "{:?}", took.elapsed());

    // The bucket out of reach: a VM writes until refused, committing as it goes, as a guest's
    // fsyncs do, ordered: a commit waiting for its upload would wait for the bucket.
    std::fs::write(&faults, "failing=unavailable").unwrap();
    let cell_dir = tenant.join("vm");
    let vm = CellClient::connect(&socket, &cell_dir, CAPACITY).unwrap();
    let mut written = 0;
    let (refused, cache) = loop {
        match write(&vm, 1 + written, 1000 + written) {
            Ok(()) => written += 1,
            Err(errno) => break (errno, metric("cladefsd_part_cache_bytes")),
        }
        if written % 16 == 0 {
            vm.commit_ordered(None).unwrap();
        }
    };
    assert_eq!(refused, Errno::ENOSPC, "after {written} MiB");
    assert!(written > 100, "refused after {written} MiB");
    // The cache gave way first: no more than a band of the headroom left of it.
    assert!(cache <= HEADROOM / 4, "{cache} bytes cached as the first write was refused");
    // Refused, its cell commits, not failed: its commit frees what its extents open held, which
    // a few more writes take, then it refuses again, and commits again.
    vm.commit_ordered(None).unwrap();
    let more = (0..64).take_while(|more| write(&vm, 1 + written + more, 3).is_ok()).count() as u64;
    assert!(more < 64, "refused no more after its commit");
    assert_eq!(write(&vm, 1 + written + more, 4), Err(Errno::ENOSPC));
    vm.commit_ordered(None).unwrap();
    written += more;

    // The bucket back: what waited uploads and leaves the disk; the VM writes again.
    std::fs::write(&faults, "failing=not").unwrap();
    until("writes admitted again", || write(&vm, 1 + written, 2).is_ok());
    vm.commit(None).unwrap();
    sampling.store(false, Ordering::Relaxed);
    sampler.join().unwrap();
    let lowest = lowest.load(Ordering::Relaxed);
    assert!(lowest >= HEADROOM / 4, "the filesystem came to {lowest} bytes free");
    drop(vm);
    drop(daemon);
}
