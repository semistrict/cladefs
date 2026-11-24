// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.9 / board 10962: under madsim, each simulated node reads its own environment, tracks
//! its own boot generation, and has all of its tasks dropped (seeing `dying()`) before its restart
//! runs. Built with `RUSTFLAGS="--cfg madsim"`.

#![cfg(madsim)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// TASK-685.9: each node reads the environment it was given, and, killed and started again,
/// what its restart gives it; the test's own code, the process's.
#[test]
fn nodes_read_the_environments_they_are_given() {
    let runtime = madsim::runtime::Runtime::new();
    runtime.block_on(async {
        let handle = madsim::runtime::Handle::current();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let boots = Arc::new(AtomicU64::new(0));
        let mut nodes = Vec::new();
        for host in 1..=2u8 {
            let (seen, boots) = (seen.clone(), boots.clone());
            let node = handle
                .create_node()
                .name(format!("host{host}"))
                .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)))
                .init(move || {
                    let (seen, boot) = (seen.clone(), boots.fetch_add(1, Ordering::Relaxed));
                    async move {
                        // Before it is given one, the process's.
                        let before = node_local::env::var("NODE_LOCAL_HOST").ok();
                        node_local::env::set_vars([("NODE_LOCAL_HOST", format!("{host}.{boot}"))]);
                        let after = node_local::env::var("NODE_LOCAL_HOST").ok();
                        seen.lock().unwrap().push((before, after));
                    }
                })
                .build();
            nodes.push(node);
        }
        madsim::time::sleep(Duration::from_secs(1)).await;
        // host 2 killed and started again: it reads nothing given before its restart gives it.
        node_local::restart(nodes[1].id()).await;
        madsim::time::sleep(Duration::from_secs(1)).await;
        let given = |value: &str| (None, Some(value.to_owned()));
        assert_eq!(*seen.lock().unwrap(), [given("1.0"), given("2.1"), given("2.2")]);
        assert_eq!(node_local::env::var_os("NODE_LOCAL_HOST"), None);
    });
}

/// Says, as it is dropped, whether its node was dying then.
struct Dropped(Arc<Mutex<Vec<bool>>>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(node_local::dying());
    }
}

/// Board 5908: a node killed and started again is so as a host's process is: its tasks all
/// dropped, each seeing its node dying, before any task of its start runs; and none of these sees
/// the previous boot's environment or generation. Its tasks many, as madsim moves its clock as it
/// drops each.
#[test]
fn a_node_starts_again_once_what_its_kill_left_is_gone() {
    const TASKS: usize = 50_000;
    let runtime = madsim::runtime::Runtime::new();
    runtime.block_on(async {
        let handle = madsim::runtime::Handle::current();
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let held = dropped.clone();
        let node = handle
            .create_node()
            .name("host")
            .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
            .init(move || {
                let held = held.clone();
                async move {
                    for _ in 0..TASKS {
                        let dropped = Dropped(held.clone());
                        drop(madsim::task::spawn(async move {
                            std::future::pending::<()>().await;
                            drop(dropped);
                        }));
                    }
                    madsim::time::sleep(Duration::from_millis(100)).await;
                    node_local::env::set_vars([("UP", "1")]);
                    // A node whose first task ends has exited, its tasks dropped.
                    std::future::pending::<()>().await
                }
            })
            .build();
        madsim::time::sleep(Duration::from_secs(1)).await;
        let before = handle
            .get_node(node.id())
            .unwrap()
            .spawn(async { node_local::generation() })
            .await
            .unwrap();
        node_local::restart(node.id()).await;
        // Spawned on its start, as a guest's task is, before its boot runs.
        let started = handle.get_node(node.id()).unwrap();
        let (up, after) = started
            .spawn(async { (node_local::env::var_os("UP"), node_local::generation()) })
            .await
            .unwrap();
        let dropped = dropped.lock().unwrap();
        let dying = dropped.iter().filter(|dying| **dying).count();
        assert_eq!((up, dropped.len(), dying), (None, TASKS, TASKS));
        assert!(after > before, "generation must advance across restart: {before} -> {after}");
    });
}
