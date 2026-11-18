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

#[test]
fn a_write_back_error_reaches_a_writer_held_at_the_shared_bound() {
    let disk = MemDisk::new();
    let cache = cache(Options { background: PAGE, hard: 2 * PAGE, ..Options::default() });
    let broken = Cached::new(
        WriteFault { file: disk.create("broken", true).unwrap(), failing: AtomicBool::new(true) },
        &cache,
    )
    .unwrap();
    let healthy = Cached::new(disk.create("healthy", true).unwrap(), &cache).unwrap();
    let data = pattern(7, 4 * PAGE as usize);
    broken.write_at(&data, 0).unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let wrote = healthy.write_at(&[9], 0);
        send.send(wrote).unwrap();
        healthy
    });
    let result = recv.recv_timeout(Duration::from_secs(2));
    let dirty = cache.usage().dirty;
    broken.file().failing.store(false, Ordering::SeqCst);
    cache.wake();
    let healthy = writer.join().unwrap();
    assert_eq!(dirty, 4 * PAGE, "failed pages are preserved without exceeding the bound further");
    match result.expect("the writer was held throughout the failure") {
        Ok(()) => {
            let mut byte = [0];
            healthy.read_at(&mut byte, 0).unwrap();
            assert_eq!(byte, [9]);
        }
        Err(err) => assert!(err.to_string().contains("injected write-back failure"), "{err}"),
    }
    // A sync can also report the earlier error. Once reported, the original data is durable.
    if broken.sync().is_err() {
        broken.sync().unwrap();
    }
    let mut bytes = vec![0; data.len()];
    broken.file().read_at(&mut bytes, 0).unwrap();
    assert_eq!(bytes, data);
}

/// TASK-539.12: a write-back that failed, as a full disk fails it, then succeeds, fails nothing: the
/// sync after it writes back its pages, dirty still, and the file holds them. Before, the sync
/// reported the earlier failure once, which failed a store's flush, and the store, for good.
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
