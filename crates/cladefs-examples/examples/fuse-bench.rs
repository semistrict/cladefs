// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Benchmarks host FUSE mount throughput and latency across read, write, metadata, and parallel
//! workloads (board 12092).
//!
//! ```sh
//! cargo run --release -p cladefs-examples --example fuse-bench -- [--writeback] [--threads N] [--rounds N]
//! ```

#[cfg(target_os = "linux")]
fn main() {
    use cladefs::fuse::MountOptions;
    use cladefs::{Filesystem, SetAttr, Storage};
    use futures::executor::block_on;
    use std::fs::File;
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
    use std::num::NonZeroUsize;
    use std::os::fd::AsRawFd as _;
    use std::path::Path;
    use std::time::Instant;

    assert!(!cfg!(debug_assertions), "measure an optimized build: run with --release");

    let mut threads = MountOptions::default_threads();
    let mut rounds = 3usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threads" => {
                let n: usize = args.next().expect("--threads N").parse().unwrap();
                threads = NonZeroUsize::new(n).expect("threads > 0");
            }
            "--rounds" => {
                rounds = args.next().expect("--rounds N").parse().unwrap();
            }
            other => panic!("unknown arg: {other}"),
        }
    }

    let drop_page_cache = |file: &File| {
        // SAFETY: `posix_fadvise` on a valid open file descriptor.
        let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(rc, 0, "posix_fadvise failed: {rc}");
    };

    let format_mem = || {
        let storage = Storage::memory(256 << 20);
        let fs = block_on(Filesystem::format(storage)).unwrap();
        // SAFETY: Plain calls returning the process's IDs.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let owner = SetAttr { uid: Some(uid), gid: Some(gid), ..Default::default() };
        block_on(fs.root().setattr(&owner)).unwrap();
        fs
    };

    let mut results: [Vec<f64>; 7] = Default::default();
    let names_list = [
        "seq_read_1m_mib_s",
        "seq_write_1m_mib_s",
        "stream_write_4k_mib_s",
        "rand_write_4k_ops_s",
        "create_write_4k_files_s",
        "unlink_files_s",
        "par8_cold_read_mib_s",
    ];

    for _ in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join(".gvfs");
        std::fs::create_dir(&m).unwrap();
        let mut opts = MountOptions::new();
        opts.threads(threads);
        let mount = opts.mount(format_mem(), &m).expect("mount failed");

        // 1. Large sequential write (64 MiB in 1 MiB chunks)
        let big_bytes: usize = 64 << 20;
        let chunk = vec![0x5a_u8; 1 << 20];
        let big_path = m.join("big.bin");
        let t0 = Instant::now();
        {
            let mut f = File::create(&big_path).unwrap();
            for _ in 0..(big_bytes / chunk.len()) {
                f.write_all(&chunk).unwrap();
            }
        }
        let dt = t0.elapsed().as_secs_f64();
        results[1].push((big_bytes as f64 / (1 << 20) as f64) / dt);

        // 2. Cold sequential read (drop kernel page cache first so every read hits FUSE READ)
        {
            let mut f = File::open(&big_path).unwrap();
            drop_page_cache(&f);
            let mut buf = vec![0u8; 1 << 20];
            let t0 = Instant::now();
            let mut total = 0usize;
            loop {
                let n = f.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                total += n;
            }
            let dt = t0.elapsed().as_secs_f64();
            assert_eq!(total, big_bytes);
            results[0].push((total as f64 / (1 << 20) as f64) / dt);
        }

        // 3. Parallel 8-thread cold read (each thread reads 16 MiB after DONTNEED)
        let par_files: Vec<_> = (0..8)
            .map(|i| {
                let p = m.join(format!("par-{i}.bin"));
                let mut f = File::create(&p).unwrap();
                for _ in 0..16 {
                    f.write_all(&chunk).unwrap();
                }
                drop_page_cache(&f);
                p
            })
            .collect();
        let t0 = Instant::now();
        std::thread::scope(|s| {
            for p in &par_files {
                s.spawn(move || {
                    let mut f = File::open(p).unwrap();
                    let mut buf = vec![0u8; 1 << 20];
                    let mut total = 0usize;
                    loop {
                        let n = f.read(&mut buf).unwrap();
                        if n == 0 {
                            break;
                        }
                        total += n;
                    }
                    assert_eq!(total, 16 << 20);
                });
            }
        });
        let dt = t0.elapsed().as_secs_f64();
        results[6].push((128.0_f64) / dt);
        for p in &par_files {
            std::fs::remove_file(p).unwrap();
        }
        std::fs::remove_file(&big_path).unwrap();

        // 4. Small-chunk streaming write (16 MiB in 4 KiB writes)
        let small_chunk = vec![0x3c_u8; 4096];
        let stream_path = m.join("stream.bin");
        let stream_count = 4096usize;
        let t0 = Instant::now();
        {
            let mut f = File::create(&stream_path).unwrap();
            for _ in 0..stream_count {
                f.write_all(&small_chunk).unwrap();
            }
        }
        let dt = t0.elapsed().as_secs_f64();
        results[2].push(((stream_count * 4096) as f64 / (1 << 20) as f64) / dt);

        // 5. Random 4 KiB overwrites (2048 ops over 16 MiB file)
        let rand_ops = 2048usize;
        {
            let mut f = File::options().read(true).write(true).open(&stream_path).unwrap();
            let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
            let t0 = Instant::now();
            for _ in 0..rand_ops {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let page = rng % (stream_count as u64);
                f.seek(SeekFrom::Start(page * 4096)).unwrap();
                f.write_all(&small_chunk).unwrap();
            }
            let dt = t0.elapsed().as_secs_f64();
            results[3].push(rand_ops as f64 / dt);
        }
        std::fs::remove_file(&stream_path).unwrap();

        // 6. Small-file create + 4 KiB write + close (500 files across 8 threads)
        let small_dir = m.join("small");
        std::fs::create_dir(&small_dir).unwrap();
        let n_files = 500usize;
        let t0 = Instant::now();
        std::thread::scope(|s| {
            let dir_ref: &Path = &small_dir;
            let buf_ref: &[u8] = &small_chunk;
            for tid in 0..8 {
                s.spawn(move || {
                    for i in (tid..n_files).step_by(8) {
                        std::fs::write(dir_ref.join(format!("f-{i}")), buf_ref).unwrap();
                    }
                });
            }
        });
        let dt = t0.elapsed().as_secs_f64();
        results[4].push(n_files as f64 / dt);

        // 7. Unlink those 500 files across 8 threads
        let t0 = Instant::now();
        std::thread::scope(|s| {
            let dir_ref: &Path = &small_dir;
            for tid in 0..8 {
                s.spawn(move || {
                    for i in (tid..n_files).step_by(8) {
                        std::fs::remove_file(dir_ref.join(format!("f-{i}"))).unwrap();
                    }
                });
            }
        });
        let dt = t0.elapsed().as_secs_f64();
        results[5].push(n_files as f64 / dt);

        mount.unmount(true).unwrap();
        let _ = mount.join().unwrap();
    }

    println!("fuse-bench: threads={} rounds={}", threads, rounds);
    for (name, mut vals) in names_list.into_iter().zip(results) {
        vals.sort_by(f64::total_cmp);
        let med = vals[vals.len() / 2];
        let all: Vec<String> = vals.iter().map(|v| format!("{v:.1}")).collect();
        println!("  {name:<26} median={med:>8.1}  ({})", all.join(", "));
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("fuse-bench runs on Linux only");
}
