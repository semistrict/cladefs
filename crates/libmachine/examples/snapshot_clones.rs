// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Measures sandboxes created from a snapshot, many at once: how long each
//! takes to be ready (restored, its agent answering after the RNG reseed)
//! and to run its first command, and how much memory each costs when
//! their RAM is a copy-on-write mapping of one file.
//!
//! ```sh
//! cargo run --release -p libmachine --example snapshot_clones -- 10 100
//! ```
//!
//! Boots a template sandbox of the test guest (`crates/libmachine/scripts/build-test-guest.sh`,
//! or `LIBMACHINE_GUEST_DIR`; `KERNEL` overrides its vmlinux, e.g. cladefs's),
//! snapshots it into `SNAPSHOT_DIR` (default target/snapshot-bench), then,
//! for each count given (default 10 and 100): restores one clone at a time
//! (the baseline), then that many at once, each running a command; all stay
//! up while memory is measured, then close. 256 MiB of RAM each unless
//! `MEM_MIB` says otherwise.
//!
//! Memory: the process's resident set counts a page of the file once per
//! clone that maps it; what a clone really costs is its private pages
//! (what its guest wrote, copied on write: Private_Dirty) plus its share of
//! the file's page cache, held once for all (Pss divides it among them).
//! Run it in a memory-capped scope on a shared host:
//! `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0 cargo run ...`

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kvm::main;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("libmachine runs on x86-64 Linux alone, for now (board 13063)");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod kvm {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use libmachine::agent::Command;
    use libmachine::sandbox::Sandbox;
    use libmachine::{LinuxBoot, Snapshot};

    /// The kB fields of /proc/self/smaps_rollup.
    fn rollup(field: &str) -> u64 {
        let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
        let line = rollup
            .lines()
            .find(|l| l.starts_with(field))
            .unwrap_or_else(|| panic!("no {field} in smaps_rollup"));
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    /// The memory charged to this process's cgroup (its page cache included),
    /// if it can be read.
    fn cgroup_memory() -> Option<u64> {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// How much of `path` is in the page cache, in bytes (mincore over a map).
    fn page_cache_of(path: &std::path::Path) -> u64 {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        if len == 0 {
            return 0;
        }
        let page = 4096;
        // SAFETY: a fresh read-only shared mapping of the whole file, unmapped
        // below; mincore only reads its page table state into `vec`.
        unsafe {
            let addr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(addr, libc::MAP_FAILED);
            let mut vec = vec![0u8; len.div_ceil(page)];
            let ok = libc::mincore(addr, len, vec.as_mut_ptr()) == 0;
            libc::munmap(addr, len);
            assert!(ok, "mincore");
            vec.iter().filter(|&&b| b & 1 != 0).count() as u64 * page as u64
        }
    }

    fn percentiles(what: &str, mut times: Vec<Duration>) {
        times.sort();
        let at = |p: f64| times[((times.len() - 1) as f64 * p) as usize];
        println!(
            "  {what} ({}): median {:.1} ms, p90 {:.1} ms, worst {:.1} ms, best {:.1} ms",
            times.len(),
            at(0.5).as_secs_f64() * 1e3,
            at(0.9).as_secs_f64() * 1e3,
            times[times.len() - 1].as_secs_f64() * 1e3,
            times[0].as_secs_f64() * 1e3,
        );
    }

    /// Creates a sandbox from `snapshot` and runs its first command: how long
    /// until it was ready, and until the command had run.
    async fn clone(snapshot: Arc<Snapshot>) -> (Sandbox, Duration, Duration) {
        let start = Instant::now();
        let sb = Sandbox::from_snapshot(snapshot)
            .ready_timeout(Duration::from_secs(120))
            .create()
            .await
            .expect("create from the snapshot");
        let ready = start.elapsed();
        let out = sb.exec(Command::new("cat").arg("/tmp/marker")).await.expect("first command");
        assert_eq!(out.stdout, b"template\n");
        (sb, ready, start.elapsed())
    }

    #[tokio::main]
    pub async fn main() {
        let counts: Vec<usize> =
            std::env::args().skip(1).map(|n| n.parse().expect("counts of clones")).collect();
        let counts = if counts.is_empty() { vec![10, 100] } else { counts };
        let mem_mib: usize = std::env::var("MEM_MIB").map_or(256, |m| m.parse().unwrap());
        let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
            PathBuf::from,
        );
        let kernel = std::env::var_os("KERNEL").map_or_else(|| dir.join("vmlinux"), PathBuf::from);
        let snapshot_dir = std::env::var_os("SNAPSHOT_DIR").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/snapshot-bench"),
            PathBuf::from,
        );
        std::fs::create_dir_all(&snapshot_dir).unwrap();
        let path = snapshot_dir.join("template");
        let boot = LinuxBoot {
            kernel: kernel.clone(),
            initrd: Some(dir.join("initramfs.cpio")),
            cmdline: "quiet libmachine_action=agent".into(),
        };

        let start = Instant::now();
        let template = Sandbox::builder(boot)
            .mem_size(mem_mib << 20)
            .ready_timeout(Duration::from_secs(120))
            .create()
            .await
            .expect("boot the template");
        let booted = start.elapsed();
        template.write_file("/tmp/marker", b"template\n".to_vec(), 0o644).await.unwrap();
        let start = Instant::now();
        let snapshot = Arc::new(template.snapshot(&path).await.expect("snapshot"));
        let snapshotted = start.elapsed();
        template.close().await;
        let file = std::fs::metadata(&path).unwrap();
        println!(
            "template ({}, {mem_mib} MiB, 1 vCPU): booted and ready in {:.1} ms; snapshot in \
             {:.1} ms, {} MiB file, {} MiB of it on disk",
            kernel.display(),
            booted.as_secs_f64() * 1e3,
            snapshotted.as_secs_f64() * 1e3,
            file.len() >> 20,
            {
                use std::os::unix::fs::MetadataExt;
                (file.blocks() * 512) >> 20
            }
        );

        for &count in &counts {
            println!("{count} clones:");
            // One at a time first: what a clone costs on its own.
            let mut alone = Vec::new();
            for _ in 0..count.min(10) {
                let (sb, ready, _) = clone(snapshot.clone()).await;
                alone.push(ready);
                sb.close().await;
            }
            percentiles("ready, one at a time", alone);

            // Machine::restore alone, `count` threads at once (no machine runs).
            let barrier = std::sync::Barrier::new(count);
            let restores: Vec<Duration> = std::thread::scope(|s| {
                let threads: Vec<_> = (0..count)
                    .map(|_| {
                        s.spawn(|| {
                            barrier.wait();
                            let t = Instant::now();
                            let m = libmachine::Machine::restore(&snapshot, snapshot.config())
                                .expect("restore");
                            let took = t.elapsed();
                            // Held until all are restored.
                            barrier.wait();
                            drop(m);
                            took
                        })
                    })
                    .collect();
                threads.into_iter().map(|t| t.join().unwrap()).collect()
            });
            percentiles("Machine::restore, all at once", restores);

            let rss_before = rollup("Rss:");
            let private_before = rollup("Private_Dirty:");
            let cgroup_before = cgroup_memory();
            let start = Instant::now();
            let tasks: Vec<_> = (0..count).map(|_| tokio::spawn(clone(snapshot.clone()))).collect();
            let mut sandboxes = Vec::new();
            let (mut ready, mut first) = (Vec::new(), Vec::new());
            for task in tasks {
                let (sb, r, f) = task.await.unwrap();
                sandboxes.push(Arc::new(sb));
                ready.push(r);
                first.push(f);
            }
            let all = start.elapsed();
            println!(
                "  all {count} created at once and ran a command in {:.1} ms",
                all.as_secs_f64() * 1e3
            );
            percentiles("ready (restored, agent answered after reseed), all at once", ready);
            percentiles("first command done, from the start of its create", first);

            // A command in every clone at once.
            let start = Instant::now();
            let execs: Vec<_> = sandboxes
                .iter()
                .map(|sb| {
                    let sb = sb.clone();
                    tokio::spawn(async move {
                        let t = Instant::now();
                        sb.exec(&Command::new("true")).await.unwrap();
                        t.elapsed()
                    })
                })
                .collect();
            let mut exec_times = Vec::new();
            for e in execs {
                exec_times.push(e.await.unwrap());
            }
            println!(
                "  a command in each at once: all done in {:.1} ms",
                start.elapsed().as_secs_f64() * 1e3
            );
            percentiles("exec true with all up", exec_times);

            let rss = rollup("Rss:") - rss_before;
            let private = rollup("Private_Dirty:") - private_before;
            let pss = rollup("Pss:");
            let cached = page_cache_of(&path);
            println!(
                "  memory with {count} up: resident {} MiB (a page of the file counted once per \
                 clone mapping it), private dirty {} MiB = {:.1} MiB per clone (written by its guest, \
                 or its own), process PSS {} MiB, the snapshot file's page cache {} MiB (shared \
                 by all)",
                rss >> 10,
                private >> 10,
                private as f64 / 1024.0 / count as f64,
                pss >> 10,
                cached >> 20
            );
            if let (Some(before), Some(now)) = (cgroup_before, cgroup_memory()) {
                println!(
                    "  cgroup memory: +{} MiB with {count} up = {:.1} MiB per clone (page cache and \
                     kernel memory included)",
                    now.saturating_sub(before) >> 20,
                    now.saturating_sub(before) as f64 / (1 << 20) as f64 / count as f64
                );
            }
            let start = Instant::now();
            for sb in &sandboxes {
                sb.close().await;
            }
            println!("  closed one after another in {:.1} ms", start.elapsed().as_secs_f64() * 1e3);
        }
        drop(snapshot);
        let _ = std::fs::remove_file(&path);
    }
}
