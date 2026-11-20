// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.2: under madsim, a `NodeLocal` static is each simulated node's own; a node killed and
//! started again, its runtime holding `node_local::Plugin`, has its values made anew, as a process
//! does. Built with `RUSTFLAGS="--cfg madsim"`.

#![cfg(madsim)]

use node_local::node_local;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

node_local! {
    static COUNTED: AtomicU64 = AtomicU64::new(0);
}

#[test]
fn nodes_keep_their_own_statics_and_start_again_with_none() {
    let runtime = madsim::runtime::Runtime::new();
    runtime.add_simulator::<node_local::Plugin>();
    runtime.block_on(async {
        let handle = madsim::runtime::Handle::current();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut nodes = Vec::new();
        for host in 1..=2u8 {
            let seen = seen.clone();
            let node = handle
                .create_node()
                .name(format!("host{host}"))
                .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)))
                .init(move || {
                    let seen = seen.clone();
                    async move {
                        // Each counts to its own number: three for host 1, five for host 2.
                        let times = if host == 1 { 3 } else { 5 };
                        for _ in 0..times {
                            COUNTED.get().fetch_add(1, Ordering::Relaxed);
                            madsim::time::sleep(Duration::from_millis(10)).await;
                        }
                        seen.lock().unwrap().push((host, COUNTED.get().load(Ordering::Relaxed)));
                    }
                })
                .build();
            nodes.push(node);
        }
        madsim::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*seen.lock().unwrap(), [(1, 3), (2, 5)]);
        // The test's own, outside any node, untouched.
        assert_eq!(COUNTED.get().load(Ordering::Relaxed), 0);
        // host 2 killed and started again: its count starts from nothing.
        handle.kill(nodes[1].id());
        handle.restart(nodes[1].id());
        madsim::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*seen.lock().unwrap(), [(1, 3), (2, 5), (2, 5)]);
    });
}
