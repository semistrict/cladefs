// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Board 2890: under madsim, each simulated host's physical memory is its own (TASK-685.14), and is
//! let go of once its simulation ends: its mapping and its arenas' shared memory objects, so that a
//! process running simulation after simulation, as madsim's MADSIM_TEST_NUM does, keeps no
//! descriptor of the ones before. Built with `RUSTFLAGS="--cfg madsim"`.

#![cfg(madsim)]

use std::net::{IpAddr, Ipv4Addr};

/// The descriptors the process has open.
fn open_descriptors() -> usize {
    let listed = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
    std::fs::read_dir(listed).unwrap().count()
}

/// A simulation, on a thread of its own as madsim runs each: three hosts, each its memory, a page
/// of its default arena and of an arena of its own allocated, those of host 3 held as its node is
/// killed.
fn simulation() {
    let ran = std::thread::spawn(|| {
        let runtime = madsim::runtime::Runtime::new();
        runtime.block_on(async {
            let handle = madsim::runtime::Handle::current();
            for host in 1..=3u8 {
                let node = handle
                    .create_node()
                    .name(format!("host{host}"))
                    .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)))
                    .build();
                let allocating = node.spawn(async {
                    physmem::configure(8 << 20).unwrap();
                    let page = physmem::alloc(physmem::ArenaId::DEFAULT, physmem::Wait::No);
                    let arena = physmem::create_arena();
                    let other = physmem::alloc(arena, physmem::Wait::No);
                    (page.unwrap(), other.unwrap())
                });
                let held = allocating.await.unwrap();
                if host == 3 {
                    handle.kill(node.id());
                    // Held past its node's kill, as a killed host's tasks leave what they held.
                    std::mem::forget(held);
                }
            }
        });
    });
    ran.join().unwrap();
}

#[test]
fn simulations_one_after_another_keep_no_descriptor_of_their_hosts_memory() {
    // The first makes what the process keeps for good: its runtime's own, the logger's.
    simulation();
    let before = open_descriptors();
    for _ in 0..10 {
        simulation();
    }
    assert_eq!(open_descriptors(), before, "simulations left their hosts' memory's descriptors");
}
