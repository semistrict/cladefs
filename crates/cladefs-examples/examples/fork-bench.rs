// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what a fork into a new cell costs, in the clear and encrypted (TASK-411.4,
//! backlog doc-16): N forks (200) of a store of 64 extents of 64 KiB, each into a new cell of its
//! tenant's, as every fork a host makes is, timed each, on a memory bucket of the process's, so
//! that what is timed is the host's own work, the protocol's and the keys'; and the bucket's
//! requests each fork makes, which a real bucket's round trips multiply. Then the first opening of
//! ten of the forks. Say:
//!
//! ```sh
//! cargo run --release -p cladefs-examples --example fork-bench -- [N] [--no-kms]
//! ```
//!
//! With a KMS (a `local:///` one of its own, `CLADEFS_KMS`, keeping tenant 5's KEK, not 6's), it
//! forks a store of tenant 5, encrypted, whose forks keep the DEKs they inherit rewrapped under
//! their own cells' KEKs, taken from the host's pool, and one of tenant 6, in the clear. With
//! `--no-kms`, a host with none: tenant 6's alone, as hosts forked before TASK-411.3. Each request
//! of the bucket's waits `CLADEFS_BUCKET_LATENCY_MS` more, if set, as a bucket elsewhere's round
//! trips do.

use cladefs::{ExtentId, ExtentStore, VolatileSlice};
use std::time::{Duration, Instant};

const EXTENTS: u64 = 64;
const EXTENT: usize = 64 << 10;
const OPENED: usize = 10;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kms = !args.iter().any(|arg| arg == "--no-kms");
    let forks: usize =
        args.iter().find(|arg| !arg.starts_with("--")).map_or(200, |n| n.parse().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let kms_dir = dir.path().join("kms");
    if kms {
        let local = extent_keys::kms::LocalKms::open(&kms_dir).unwrap();
        local.create(5).unwrap();
    }
    // SAFETY: before any thread of the process's reads the environment.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///fork-bench");
        std::env::set_var("CLADEFSD_TOKEN", "1");
        if kms {
            std::env::set_var("CLADEFS_KMS", format!("local://{}", kms_dir.display()));
        }
    }
    let tenants: &[(u64, &str)] = match kms {
        true => &[(5, "encrypted"), (6, "clear, host with a KMS")],
        false => &[(6, "clear, host with no KMS")],
    };
    println!(
        "{forks} forks into new cells of a store of {EXTENTS} extents of {} KiB",
        EXTENT >> 10
    );
    for &(tenant, name) in tenants {
        bench(&dir.path().join(format!("tenant-{tenant}")), tenant, name, forks);
    }
}

/// Forks a store of `tenant`'s `forks` times into new cells, under `dir`, and prints what it took.
fn bench(dir: &std::path::Path, tenant: u64, name: &str, forks: usize) {
    std::fs::create_dir(dir).unwrap();
    let capacity = 2 * EXTENTS * EXTENT as u64;
    let store = ExtentStore::create_in(dir.join("parent"), capacity, cladefs::new_cell(tenant));
    let store = store.unwrap();
    let data = vec![0x5a; EXTENT];
    for sequence in 1..=EXTENTS {
        let extent = ExtentId { branch: store.branch().id, sequence };
        store.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
    }
    store.commit(Some(b"root")).unwrap();
    if let Some(kms) = cladefs::bucket::kms().unwrap() {
        // The pool of the tenant's new cell KEKs, filled as its store opened, off the forks' path.
        futures::executor::block_on(kms.filled(tenant));
    }
    let mut took = Vec::with_capacity(forks);
    let before = store.bucket().stats();
    for n in 0..forks {
        let started = Instant::now();
        store.fork(dir.join(format!("fork-{n}")), cladefs::new_cell(tenant)).unwrap();
        took.push(started.elapsed());
    }
    let after = store.bucket().stats();
    let requests: u64 = after.requests.iter().zip(&before.requests).map(|(a, b)| a - b).sum();
    let mut opened = Vec::with_capacity(OPENED);
    for n in 0..OPENED.min(forks) {
        let started = Instant::now();
        drop(ExtentStore::open(dir.join(format!("fork-{n}"))).unwrap());
        opened.push(started.elapsed());
    }
    took.sort();
    opened.sort();
    let ms = |at: Duration| at.as_secs_f64() * 1e3;
    let at = |of: &[Duration], q: f64| ms(of[((of.len() - 1) as f64 * q) as usize]);
    println!(
        "{name:>24}: fork median {:.2} ms, p90 {:.2} ms, max {:.2} ms; {:.1} bucket requests a \
         fork; a fork's first opening median {:.2} ms",
        at(&took, 0.5),
        at(&took, 0.9),
        ms(*took.last().unwrap()),
        requests as f64 / forks as f64,
        at(&opened, 0.5),
    );
}
