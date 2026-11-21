// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.2: under madsim, a `NodeLocal` static is each simulated node's own; a node killed and
//! started again (`node_local::restart`) has its values made anew, as a process does. Built with
//! `RUSTFLAGS="--cfg madsim"`.

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
        node_local::restart(nodes[1].id()).await;
        madsim::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*seen.lock().unwrap(), [(1, 3), (2, 5), (2, 5)]);
    });
}

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

node_local! {
    /// Whether the node's boot has come up.
    static UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
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
/// a value one of its kill's made. A guest's task spawned on the node at its start, before its boot
/// ran, saw the boot killed up, and served by what that boot left, opened a cell beside the one
/// the new boot opened. Its tasks many, as madsim moves its clock as it drops each.
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
                    UP.get().store(true, Ordering::Relaxed);
                    // A node whose first task ends has exited, its tasks dropped.
                    std::future::pending::<()>().await
                }
            })
            .build();
        madsim::time::sleep(Duration::from_secs(1)).await;
        node_local::restart(node.id()).await;
        // Spawned on its start, as a guest's task is, before its boot runs.
        let started = handle.get_node(node.id()).unwrap();
        let up = started.spawn(async { UP.get().load(Ordering::Relaxed) }).await.unwrap();
        let dropped = dropped.lock().unwrap();
        let dying = dropped.iter().filter(|dying| **dying).count();
        assert_eq!((up, dropped.len(), dying), (false, TASKS, TASKS));
    });
}

/// Whether a value of [`MADE_SLOWLY`]'s is being made, and whether another simulation's thread let
/// its node's values go meanwhile.
static SLOWLY: (Mutex<(bool, bool)>, std::sync::Condvar) =
    (Mutex::new((false, false)), std::sync::Condvar::new());

node_local! {
    /// Made only once another simulation's thread has started a node, letting that node's values
    /// go, this static's among them.
    static MADE_SLOWLY: AtomicU64 = {
        let (state, changed) = &SLOWLY;
        state.lock().unwrap().0 = true;
        changed.notify_all();
        drop(changed.wait_while(state.lock().unwrap(), |state| !state.1).unwrap());
        AtomicU64::new(0)
    };
}

/// Board 5473: simulations sharing a process, as a test binary's run on threads of its own, are
/// independent. A node's start on one, letting its values go, waits for no value another is
/// making, which may itself wait on that start: made holding its static's lock, as the start
/// waited for, a value whose making used a static not yet registered waited for the registry, which
/// the start held, every simulation in the process stopping with them.
#[test]
fn a_value_being_made_holds_up_no_other_simulations_start() {
    let (done, finished) = std::sync::mpsc::channel();
    let making = std::thread::spawn({
        let done = done.clone();
        move || {
            let runtime = madsim::runtime::Runtime::new();
            runtime.block_on(async { MADE_SLOWLY.get().fetch_add(1, Ordering::Relaxed) });
            done.send("making").unwrap();
        }
    });
    let (state, changed) = &SLOWLY;
    drop(changed.wait_while(state.lock().unwrap(), |state| !state.0).unwrap());
    let starting = std::thread::spawn(move || {
        let runtime = madsim::runtime::Runtime::new();
        runtime.block_on(async { node_local::starting() });
        state.lock().unwrap().1 = true;
        changed.notify_all();
        done.send("starting").unwrap();
    });
    // Waited for here, outside any simulation, whose clock runs meanwhile, unlike the simulations':
    // a start waiting for the value fails the test rather than hang it. Far longer than it takes.
    let mut ended: Vec<_> = (0..2)
        .map(|_| finished.recv_timeout(Duration::from_secs(10)))
        .collect::<Result<_, _>>()
        .expect("a node's start, on another simulation's thread, waited for a value being made");
    ended.sort();
    assert_eq!(ended, ["making", "starting"]);
    making.join().unwrap();
    starting.join().unwrap();
}

/// What [`HELD`]'s values let go of, once their simulations ended: each value's node, as it said.
static RELEASED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// A static whose node values hold something outside the process's memory: their node's number.
static HELD: node_local::NodeLocal<AtomicU64> = node_local::NodeLocal::releasing(
    || AtomicU64::new(0),
    |held| {
        RELEASED.lock().unwrap().push(held.load(Ordering::Relaxed));
    },
);

/// Board 2890: a releasing static's node values let go of what they hold once their simulation
/// ended, as its thread exits: every node's, a killed one's too; none while it runs.
#[test]
fn values_let_go_once_their_simulation_ends() {
    let ran = std::thread::spawn(|| {
        let runtime = madsim::runtime::Runtime::new();
        runtime.block_on(async {
            let handle = madsim::runtime::Handle::current();
            let mut nodes = Vec::new();
            for host in 1..=2u8 {
                let node = handle
                    .create_node()
                    .name(format!("host{host}"))
                    .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)))
                    .init(move || async move {
                        HELD.get().store(u64::from(host), Ordering::Relaxed);
                        std::future::pending::<()>().await;
                    })
                    .build();
                nodes.push(node);
            }
            madsim::time::sleep(Duration::from_secs(1)).await;
            // host 1 killed: its value let go of nothing yet, its simulation running on.
            node_local::kill(nodes[0].id()).await;
            assert_eq!(*RELEASED.lock().unwrap(), Vec::<u64>::new());
        });
    });
    ran.join().unwrap();
    let mut released = RELEASED.lock().unwrap().clone();
    released.sort();
    assert_eq!(released, [1, 2]);
}
