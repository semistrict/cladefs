// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

struct WriteFault<F> {
    file: F,
    failing: AtomicBool,
}

impl<F: File> File for WriteFault<F> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected write-back failure"));
        }
        self.file.write_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }

    fn sync_cached(&self) -> io::Result<()> {
        self.file.sync_cached()
    }
}

/// Board 8553: a write-back failing on one disk fails that disk's file's writers held at the
/// cache's bound, and no other's: a file on a healthy disk, the cache past its bound with the
/// failing file's pages, writes on, held only until its own pages are written back. The cache is
/// the process's, every tenant's cells' files in it, and the failure of the file that failed last
/// in the flusher's pass was every writer's held at the bound, as `EIO` from a cell's write.
#[test]
fn a_write_back_failing_on_one_disk_fails_no_writer_of_another() {
    let (failing_disk, healthy_disk) = (MemDisk::new(), MemDisk::new());
    let cache = cache(Options { background: PAGE, hard: 2 * PAGE, ..Options::default() });
    let broken = Cached::new(
        WriteFault {
            file: failing_disk.create("broken", true).unwrap(),
            failing: AtomicBool::new(true),
        },
        &cache,
    )
    .unwrap();
    let healthy = Cached::new(healthy_disk.create("healthy", true).unwrap(), &cache).unwrap();
    let data = pattern(7, 4 * PAGE as usize);
    broken.write_at(&data, 0).unwrap();
    assert_eq!(cache.usage().dirty, 4 * PAGE, "past the bound, with the failing file's pages");

    // The healthy file's writes go on, its own pages past the bound too, which they wait for the
    // flusher to write back, while the other disk fails still.
    let (send, recv) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let wrote: io::Result<()> = (0..8u64).try_for_each(|page| {
            healthy.write_at(&pattern(page as u8, PAGE as usize), page * PAGE)
        });
        send.send(wrote).unwrap();
        healthy
    });
    let wrote = recv.recv_timeout(Duration::from_secs(10));
    assert_eq!(
        wrote.expect("the healthy file's writer held").map_err(|err| err.to_string()),
        Ok(())
    );
    let healthy = writer.join().unwrap();
    let mut read = vec![0; PAGE as usize];
    healthy.read_at(&mut read, 7 * PAGE).unwrap();
    assert_eq!(read, pattern(7, PAGE as usize));

    // The failing file's own writer, held at the bound, gets its failure.
    let refused = broken.write_at(&[1], 0).unwrap_err();
    assert_eq!(refused.to_string(), "injected write-back failure");

    // Its disk taking writes again, its sync writes its pages back, and nothing is stuck.
    broken.file().failing.store(false, Ordering::SeqCst);
    broken.sync().unwrap();
    healthy.sync().unwrap();
    let mut bytes = vec![0; data.len()];
    broken.file().read_at(&mut bytes, 0).unwrap();
    assert_eq!(bytes, data);
    let usage = cache.usage();
    assert_eq!((usage.dirty, cache.stuck.load(Ordering::Relaxed)), (0, 0));
}

/// TASK-539.12: a write-back that failed, as a full disk fails it, then succeeds, fails nothing:
/// the sync after it writes back its pages, dirty still, and the file holds them. Before, the sync
/// reported the earlier failure once, which failed a cell's flush, and the cell, for good.
#[test]
fn a_write_back_that_failed_then_succeeds_fails_no_sync() {
    let disk = MemDisk::new();
    let cache = PageCache::without_flusher(Options::default());
    let file = Cached::new(
        WriteFault { file: disk.create("full", true).unwrap(), failing: AtomicBool::new(true) },
        &cache,
    )
    .unwrap();
    let data = pattern(3, 2 * PAGE as usize);
    file.write_at(&data, 0).unwrap();
    assert!(cache.write_back_all().is_err(), "the disk was full");
    assert_eq!(cache.usage().dirty, 2 * PAGE, "its pages kept dirty");
    file.file().failing.store(false, Ordering::SeqCst);
    file.sync().unwrap();
    let mut bytes = vec![0; data.len()];
    file.file().read_at(&mut bytes, 0).unwrap();
    assert_eq!((bytes, cache.usage().dirty), (data, 0));
}

#[test]
#[ignore = "sparse write-back scaling benchmark (TASK-524); run alone"]
fn bench_sparse_write_back() {
    for pages in [256, 512, 1024, 2048] {
        for stride in [1, 2] {
            let disk = MemDisk::new();
            let raw = disk.create("file", true).unwrap();
            let cache = PageCache::without_flusher(Options::default());
            let file = Cached::new(raw, &cache).unwrap();
            let page = [0x51; PAGE as usize];
            for index in 0..pages {
                file.write_at(&page, index * stride * PAGE).unwrap();
            }
            let start = Instant::now();
            file.sync().unwrap();
            eprintln!("{pages} dirty pages, stride {stride}: {:?}", start.elapsed());
            assert_eq!(cache.usage().dirty, 0);
            let mut read = [0; PAGE as usize];
            for index in 0..pages {
                file.file().read_at(&mut read, index * stride * PAGE).unwrap();
                assert_eq!(read, page);
            }
        }
    }
}

#[test]
fn sparse_write_back_visits_scale_with_page_count() {
    fn visits(pages: usize) -> usize {
        let disk = MemDisk::new();
        let cache = PageCache::without_flusher(Options::default());
        let file = Cached::new(disk.create("file", true).unwrap(), &cache).unwrap();
        for index in 0..pages {
            file.write_at(&[index as u8; PAGE as usize], 2 * index as u64 * PAGE).unwrap();
        }

        super::WRITE_BACK_VISITS.set(0);
        file.sync().unwrap();
        let visits = super::WRITE_BACK_VISITS.get();
        assert!(visits >= pages, "counted {visits} visits for {pages} isolated dirty pages");
        assert_eq!(cache.usage().dirty, 0);
        let mut read = [0; PAGE as usize];
        for index in 0..pages {
            file.file().read_at(&mut read, 2 * index as u64 * PAGE).unwrap();
            assert_eq!(read, [index as u8; PAGE as usize]);
        }
        visits
    }

    // TASK-524: quadrupling isolated runs used to cost about sixteen times as many visits.
    let small = visits(32);
    let large = visits(128);
    assert!(large <= 5 * small, "range entries inspected grew from {small} to {large}");
}
