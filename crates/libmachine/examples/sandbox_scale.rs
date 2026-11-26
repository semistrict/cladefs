// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Measures many sandboxes in one process: how long they take to create,
//! the threads and memory they hold, and command latency, idle and with
//! all of them busy.
//!
//! ```sh
//! cargo run --release -p libmachine --example sandbox_scale -- 100
//! ```
//!
//! Uses the test guest (`crates/libmachine/scripts/build-test-guest.sh`, or
//! `LIBMACHINE_GUEST_DIR`), 256 MiB of RAM each unless `MEM_MIB` says
//! otherwise.

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

    use libmachine::LinuxBoot;
    use libmachine::agent::Command;
    use libmachine::sandbox::Sandbox;

    fn status(field: &str) -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with(field)).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    fn report(what: &str) {
        println!(
            "{what}: {} threads, {} MiB resident",
            status("Threads:"),
            status("VmRSS:") / 1024
        );
    }

    fn percentiles(what: &str, mut times: Vec<Duration>) {
        times.sort();
        let at = |p: f64| times[((times.len() - 1) as f64 * p) as usize];
        println!(
            "{what} ({} samples): median {:?}, p90 {:?}, p99 {:?}, worst {:?}",
            times.len(),
            at(0.5),
            at(0.9),
            at(0.99),
            times[times.len() - 1]
        );
    }

    #[tokio::main]
    pub async fn main() {
        let count: usize =
            std::env::args().nth(1).map_or(50, |n| n.parse().expect("a count of sandboxes"));
        let mem_mib: usize = std::env::var("MEM_MIB").map_or(256, |m| m.parse().unwrap());
        let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
            PathBuf::from,
        );
        let boot = LinuxBoot {
            kernel: dir.join("vmlinux"),
            initrd: Some(dir.join("initramfs.cpio")),
            cmdline: "quiet libmachine_action=agent".into(),
        };
        report("before");

        let start = Instant::now();
        let creates = (0..count).map(|_| {
            let builder = Sandbox::builder(boot.clone())
                .mem_size(mem_mib << 20)
                .ready_timeout(Duration::from_secs(120));
            tokio::spawn(async move {
                let t = Instant::now();
                let sb = builder.create().await.expect("create");
                (sb, t.elapsed())
            })
        });
        let mut sandboxes = Vec::new();
        let mut create_times = Vec::new();
        for task in creates.collect::<Vec<_>>() {
            let (sb, t) = task.await.unwrap();
            sandboxes.push(Arc::new(sb));
            create_times.push(t);
        }
        println!("{count} sandboxes created at once in {:?}", start.elapsed());
        percentiles("create, each", create_times);
        report(&format!("{count} sandboxes idle"));

        // One sandbox, the others idle: the async wrapper against the blocking
        // client underneath.
        let sb = sandboxes[0].clone();
        let mut asynced = Vec::new();
        for _ in 0..200 {
            let t = Instant::now();
            sb.exec(&Command::new("true")).await.unwrap();
            asynced.push(t.elapsed());
        }
        percentiles("exec `true`, async, one at a time", asynced);
        let agent = sb.agent().clone();
        let blocked = tokio::task::spawn_blocking(move || {
            (0..200)
                .map(|_| {
                    let t = Instant::now();
                    agent.exec(&Command::new("true")).unwrap();
                    t.elapsed()
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        percentiles("exec `true`, blocking client, one at a time", blocked);

        // Every sandbox busy at once.
        let start = Instant::now();
        let busy = sandboxes.iter().map(|sb| {
            let sb = sb.clone();
            tokio::spawn(async move {
                let mut times = Vec::new();
                for _ in 0..20 {
                    let t = Instant::now();
                    sb.exec(&Command::new("true")).await.unwrap();
                    times.push(t.elapsed());
                }
                times
            })
        });
        let mut all = Vec::new();
        for task in busy.collect::<Vec<_>>() {
            all.extend(task.await.unwrap());
        }
        let took = start.elapsed();
        println!(
            "{} execs on {count} sandboxes at once in {took:?}: {:.0} execs/s",
            all.len(),
            all.len() as f64 / took.as_secs_f64()
        );
        percentiles("exec `true`, all sandboxes at once", all);

        // A streaming process in each.
        let mut processes = Vec::new();
        for sb in &sandboxes {
            processes.push(sb.spawn(&Command::new("cat")).await.unwrap());
        }
        report(&format!("{count} sandboxes, each with a command running"));
        for p in &processes {
            p.close_stdin().await.unwrap();
            p.wait().await.unwrap();
        }
        drop(processes);

        let start = Instant::now();
        for sb in &sandboxes {
            sb.close().await;
        }
        println!("closed {count} sandboxes in {:?}", start.elapsed());
        drop(sandboxes);
        tokio::time::sleep(Duration::from_millis(200)).await;
        report("after closing");
    }
}
