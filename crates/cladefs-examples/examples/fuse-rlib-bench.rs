// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Benchmarks multi-file strided/random FUSE reads through a governed `cladefsd` with
//! `CLADEFSD_MOUNT` and `CLADEFSD_PART_CACHE`, simulating a linker (`ld`) holding hundreds of
//! `.rlib` files open over cladefs-fuse (board 12313).
//!
//! Creates `FILES` compressible 1 MiB files in an attached cell, detaches and deletes the local
//! cell directory so every file is read back from compressed+encrypted bucket segments cached in
//! the disk part cache (`CLADEFSD_PART_CACHE`), then measures:
//!
//! 1. **Strided passes (1 thread & 4 threads):** 8 passes over all `FILES`, reading 16 KiB at
//!    offset `pass * 128 KiB` (offsets never touched before, so the kernel FUSE page cache misses
//!    every read and hits `cladefsd`).
//! 2. **Evicted FUSE page-cache passes:** 4 passes over all `FILES` after calling
//!    `posix_fadvise(POSIX_FADV_DONTNEED)` on every open file descriptor to simulate host memory
//!    pressure evicting kernel FUSE pages.
//!
//! ```sh
//! CLADEFSCTL=target/release/cladefsctl \
//!   target/release/examples/fuse-rlib-bench [FILES [PASSES]]
//! ```

#[cfg(target_os = "linux")]
fn main() {
    use cladefs::cladefsd::{Daemon, memory, process};
    use std::fs::{File, OpenOptions};
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::FileExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    assert!(!cfg!(debug_assertions), "measure an optimized build: run with --release");

    let mut args = std::env::args().skip(1);
    let files: usize = args.next().map_or(200, |n| n.parse().unwrap());
    let passes: usize = args.next().map_or(8, |n| n.parse().unwrap());
    let file_size: usize = 1 << 20; // 1 MiB per .rlib
    let read_len: usize = 16 << 10; // 16 KiB sub-part read
    let stride: usize = file_size / passes.max(1);

    let cladefsctl =
        std::env::var("CLADEFSCTL").unwrap_or_else(|_| "target/release/cladefsctl".to_owned());
    let base_dir = std::env::var_os("FUSE_RLIB_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let tmp = tempfile::tempdir_in(&base_dir).unwrap();
    let dir = tmp.path();
    let bucket = dir.join("bucket");
    let part_cache = dir.join("part-cache");
    let cells = dir.join("cells");
    let mountpoint = dir.join(".gvfs");
    let control = dir.join("control.sock");
    let vm_sock = dir.join("vm.sock");
    std::fs::create_dir_all(&bucket).unwrap();
    std::fs::create_dir_all(&part_cache).unwrap();
    std::fs::create_dir_all(&mountpoint).unwrap();

    let bucket_url = format!("file://{}", bucket.display());
    // SAFETY: Single-threaded before starting the daemon runtime or any worker threads.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", &bucket_url);
        std::env::set_var("CLADEFSD_TOKEN", "1");
        std::env::set_var("CLADEFSD_GRPC", &control);
        std::env::set_var("CLADEFSD_MOUNT", &mountpoint);
        std::env::set_var("CLADEFSD_MOUNT_STORES", &cells);
        // The disk it owns, whose cells directory is the mount's (board 14305).
        std::env::set_var("CLADEFSD_ROOTS", cells.parent().unwrap());
        std::env::set_var("CLADEFSD_PART_CACHE", &part_cache);
        std::env::set_var("CLADEFSD_PART_CACHE_BYTES", (4u64 << 30).to_string());
        std::env::set_var("CLADEFSD_VM_CGROUP_DELEGATED", "1");
        std::env::set_var(
            "CLADEFS_MASTER_KEY",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        );
    }
    memory::return_large_allocations();

    let daemon = Daemon::process();
    daemon.block_on(daemon.boot(&vm_sock, None)).expect("boot cladefsd");

    let run_ctl = |ctl_args: &[&str]| -> String {
        let out = Command::new(&cladefsctl)
            .env("CLADEFSD_GRPC", &control)
            .arg("mount")
            .args(ctl_args)
            .output()
            .expect("spawn cladefsctl");
        assert!(
            out.status.success(),
            "cladefsctl mount {ctl_args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };

    // Capacity roomy enough for all files + Fxfs journal/metadata.
    let capacity = ((files as u64 + 128) << 20).next_multiple_of(4096);
    let tenant_dir = "0000000000001";
    let cell_id = run_ctl(&["create", "1", &capacity.to_string()]);
    let cell_mnt = mountpoint.join(tenant_dir).join(&cell_id);

    // Compressible ELF/.rlib-like content (~3:1 zstd ratio), unique per file and per 4 KiB page.
    let make_file_bytes = |idx: usize| -> Vec<u8> {
        let mut buf = vec![0u8; file_size];
        for page in 0..(file_size / 4096) {
            let p = &mut buf[page * 4096..(page + 1) * 4096];
            let tag = ((idx as u32) << 12) | (page as u32);
            p[..4].copy_from_slice(&tag.to_le_bytes());
            // Repeating structured symbol/code patterns that compress realistically with zstd.
            for chunk in p[4..].chunks_mut(64) {
                let b0 = (idx & 0xff) as u8;
                let b1 = (page & 0xff) as u8;
                for (j, byte) in chunk.iter_mut().enumerate() {
                    *byte = if j < 8 {
                        b0.wrapping_add(j as u8)
                    } else if j < 16 {
                        b1.wrapping_mul(3).wrapping_add(j as u8)
                    } else {
                        ((j as u8) & 0x0f).wrapping_mul(17) ^ (b0 & 0x07)
                    };
                }
            }
        }
        buf
    };

    let t_setup = Instant::now();
    for i in 0..files {
        let path = cell_mnt.join(format!("libcrate_{i:04}.rlib"));
        let data = make_file_bytes(i);
        let mut f = File::create(&path).unwrap();
        f.write_all(&data).unwrap();
    }
    // Detach so everything flushes & uploads to the bucket, drop the local cell dir, and re-attach
    // so reads go through bucket_cell::fetch (backed by CLADEFSD_PART_CACHE), matching vm1.
    run_ctl(&["detach", "1", &cell_id]);
    std::fs::remove_dir_all(cells.join(tenant_dir).join(&cell_id)).unwrap();
    run_ctl(&["attach", "1", &cell_id]);
    // Give the memory governor one tick to observe the attached cell.
    std::thread::sleep(Duration::from_millis(250));
    println!(
        "setup: {} files x {} KiB ({} MiB) uploaded & re-attached in {:.2}s",
        files,
        file_size >> 10,
        (files * file_size) >> 20,
        t_setup.elapsed().as_secs_f64()
    );

    let open_all = |mnt: &Path| -> Vec<File> {
        (0..files)
            .map(|i| {
                OpenOptions::new()
                    .read(true)
                    .open(mnt.join(format!("libcrate_{i:04}.rlib")))
                    .unwrap()
            })
            .collect()
    };

    let evict_fuse_pages = |handles: &[File]| {
        for f in handles {
            // SAFETY: Valid open fd; advisory hint telling Linux to drop clean page-cache pages.
            let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
            assert_eq!(rc, 0, "posix_fadvise DONTNEED failed: {rc}");
        }
    };

    let check_page = |idx: usize, offset: usize, buf: &[u8]| {
        let page = offset / 4096;
        let expected = ((idx as u32) << 12) | (page as u32);
        let got = u32::from_le_bytes(buf[..4].try_into().unwrap());
        assert_eq!(got, expected, "mismatch in file {idx} at offset {offset}");
    };

    let handles = Arc::new(open_all(&cell_mnt));

    // Warm disk part cache once (and evict FUSE kernel pages so measurements start cold in FUSE).
    {
        let mut buf = vec![0u8; 4096];
        for (i, f) in handles.iter().enumerate() {
            f.read_exact_at(&mut buf, 0).unwrap();
            check_page(i, 0, &buf);
        }
        evict_fuse_pages(&handles);
    }

    // Workload 1: Single-threaded strided passes (simulating single-threaded `ld` scanning 200+ rlibs).
    {
        evict_fuse_pages(&handles);
        let u0 = bucket_cell::fetch::unpacks();
        let s0 = process::sample();
        let t0 = Instant::now();
        let mut buf = vec![0u8; read_len];
        for pass in 0..passes {
            let offset = (pass * stride) & !4095;
            for (i, f) in handles.iter().enumerate() {
                f.read_exact_at(&mut buf, offset as u64).unwrap();
                check_page(i, offset, &buf);
            }
        }
        let elapsed = t0.elapsed();
        let s1 = process::sample();
        let unpacks = bucket_cell::fetch::unpacks() - u0;
        let parts = bucket_cell::fetch::shared_parts();
        let cpu_ms = s1.cpu_ns.saturating_sub(s0.cpu_ns) as f64 / 1e6;
        let reads = files * passes;
        println!(
            "strided-1t:  {reads} reads ({passes}x{files}x{}K) wall={:.3}s cpu={:.1}ms \
             ({:.1} us/read) unpacks={unpacks} parts={}MiB/{}MiB anon={}MiB budget={}MiB over={}",
            read_len >> 10,
            elapsed.as_secs_f64(),
            cpu_ms,
            elapsed.as_secs_f64() * 1e6 / reads as f64,
            parts.bytes() >> 20,
            parts.limit() >> 20,
            s1.anon >> 20,
            s1.anon_budget >> 20,
            s1.over_budget,
        );
    }

    // Workload 2: Evicted FUSE page-cache passes (simulating tight RAM evicting kernel FUSE pages).
    {
        let evict_passes = 4;
        let u0 = bucket_cell::fetch::unpacks();
        let s0 = process::sample();
        let t0 = Instant::now();
        let mut buf = vec![0u8; read_len];
        for pass in 0..evict_passes {
            evict_fuse_pages(&handles);
            let offset = ((pass * 2 + 1) * 64 * 1024) % (file_size - read_len);
            for (i, f) in handles.iter().enumerate() {
                f.read_exact_at(&mut buf, offset as u64).unwrap();
                check_page(i, offset, &buf);
            }
        }
        let elapsed = t0.elapsed();
        let s1 = process::sample();
        let unpacks = bucket_cell::fetch::unpacks() - u0;
        let parts = bucket_cell::fetch::shared_parts();
        let cpu_ms = s1.cpu_ns.saturating_sub(s0.cpu_ns) as f64 / 1e6;
        let reads = files * evict_passes;
        println!(
            "evicted-1t:  {reads} reads ({evict_passes}x{files}x{}K) wall={:.3}s cpu={:.1}ms \
             ({:.1} us/read) unpacks={unpacks} parts={}MiB/{}MiB anon={}MiB budget={}MiB over={}",
            read_len >> 10,
            elapsed.as_secs_f64(),
            cpu_ms,
            elapsed.as_secs_f64() * 1e6 / reads as f64,
            parts.bytes() >> 20,
            parts.limit() >> 20,
            s1.anon >> 20,
            s1.anon_budget >> 20,
            s1.over_budget,
        );
    }

    // Workload 3: 4-thread parallel evicted passes (concurrent FUSE workers hitting shared parts).
    {
        let evict_passes = 4;
        let threads = 4usize;
        let u0 = bucket_cell::fetch::unpacks();
        let s0 = process::sample();
        let t0 = Instant::now();
        for pass in 0..evict_passes {
            evict_fuse_pages(&handles);
            let offset = ((pass * 3 + 2) * 64 * 1024) % (file_size - read_len);
            std::thread::scope(|scope| {
                for tid in 0..threads {
                    let handles = Arc::clone(&handles);
                    scope.spawn(move || {
                        let mut buf = vec![0u8; read_len];
                        for i in (tid..files).step_by(threads) {
                            handles[i].read_exact_at(&mut buf, offset as u64).unwrap();
                            check_page(i, offset, &buf);
                        }
                    });
                }
            });
        }
        let elapsed = t0.elapsed();
        let s1 = process::sample();
        let unpacks = bucket_cell::fetch::unpacks() - u0;
        let parts = bucket_cell::fetch::shared_parts();
        let cpu_ms = s1.cpu_ns.saturating_sub(s0.cpu_ns) as f64 / 1e6;
        let reads = files * evict_passes;
        println!(
            "evicted-4t:  {reads} reads ({evict_passes}x{files}x{}K) wall={:.3}s cpu={:.1}ms \
             ({:.1} us/read) unpacks={unpacks} parts={}MiB/{}MiB anon={}MiB budget={}MiB over={}",
            read_len >> 10,
            elapsed.as_secs_f64(),
            cpu_ms,
            elapsed.as_secs_f64() * 1e6 / reads as f64,
            parts.bytes() >> 20,
            parts.limit() >> 20,
            s1.anon >> 20,
            s1.anon_budget >> 20,
            s1.over_budget,
        );
    }

    drop(handles);
    run_ctl(&["detach", "1", &cell_id]);
    let _ = daemon.block_on(daemon.drain(Duration::from_secs(30)));
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("fuse-rlib-bench runs on Linux only");
    std::process::exit(2);
}
