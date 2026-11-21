// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a daemon's process state, as the full-stack simulation needs it (doc-24, TASK-685.2).
//!
//! cladefsd keeps its state in statics: its bucket, its peers, its cell table, its pools and
//! governors. madsim runs every simulated host, a cladefsd each, in one process, where they would
//! share them. A [`NodeLocal`] static is one value per process, built normally: a [`OnceLock`],
//! made at its first use, which costs a daemon nothing more than the static it was. Under
//! `--cfg madsim`, it is one value per simulated node, each node's made at that node's first use,
//! so that two daemons in one simulation keep their own; and a node killed ([`kill`], [`restart`])
//! has its values let go of, its tasks gone first, so that it starts again with none, as a process
//! does. Outside a node, as a test's own code is, the process's.
//!
//! A node's value is leaked, as a static's is: a value let go of at its node's kill stays where
//! its holders, the node's tasks, which the kill dropped, left it. A simulation leaks a value of
//! each static its killed nodes used, no more. What such a value holds outside the process's memory
//! (mappings, descriptors) a static made [`NodeLocal::releasing`] lets go of once the simulation
//! ends, as its thread exits, no task of its left to use it: else a process running simulation
//! after simulation runs out of descriptors (board 2890).
//!
//! A process's environment is its state too, which a daemon's configuration is read from: [`env`]
//! reads it, the process's normally, and, in a simulated node given one, the node's.

use std::sync::OnceLock;

pub mod env;

/// A static of a daemon's, one value per process, or, in the simulation, per node: see the crate's
/// docs. Declared with [`node_local!`].
pub struct NodeLocal<T: 'static> {
    init: fn() -> T,
    value: OnceLock<T>,
    /// Each node's, by the simulation's thread and the node: a simulation runs on one thread,
    /// and the next one, a check of its determinism among them, on another, which starts with none.
    #[cfg(madsim)]
    nodes: std::sync::Mutex<Vec<(Node, &'static T)>>,
    #[cfg(madsim)]
    registered: std::sync::Once,
    /// What lets a node's value go of what it holds outside the process's memory, once its
    /// simulation ended ([`NodeLocal::releasing`]).
    #[cfg(madsim)]
    release: Option<fn(&'static T)>,
}

impl<T: Send + Sync + 'static> NodeLocal<T> {
    /// A static whose value `init` makes, at its first use, once for the process, or for each node.
    pub const fn new(init: fn() -> T) -> Self {
        Self {
            init,
            value: OnceLock::new(),
            #[cfg(madsim)]
            nodes: std::sync::Mutex::new(Vec::new()),
            #[cfg(madsim)]
            registered: std::sync::Once::new(),
            #[cfg(madsim)]
            release: None,
        }
    }

    /// A static as [`NodeLocal::new`] makes, each of whose node values `release` lets go of what it
    /// holds outside the process's memory, its mappings and descriptors, once the simulation it was
    /// made in ended: as the simulation's thread exits, its runtime and every task of its gone,
    /// whatever node it was of, killed or not (board 2890). The process's value it never is: built
    /// normally, as `new`.
    pub const fn releasing(init: fn() -> T, release: fn(&'static T)) -> Self {
        #[cfg(not(madsim))]
        let _ = release;
        Self {
            init,
            value: OnceLock::new(),
            #[cfg(madsim)]
            nodes: std::sync::Mutex::new(Vec::new()),
            #[cfg(madsim)]
            registered: std::sync::Once::new(),
            #[cfg(madsim)]
            release: Some(release),
        }
    }

    /// Its value: the process's, or, in a simulated node, the node's.
    pub fn get(&'static self) -> &'static T {
        #[cfg(madsim)]
        if madsim::runtime::Handle::try_current().is_ok() {
            return self.node_value((std::thread::current().id(), madsim::plugin::node()));
        }
        self.value.get_or_init(self.init)
    }

    /// The value of `node`'s, made if it has none: made holding none of this crate's locks, as
    /// its `init` may use other statics, registering them, while another simulation's thread lets
    /// its node's values go (board 5473: made holding `nodes`, an `init` registering a static
    /// waited for [`REGISTRY`], whose holder, letting its node's values go, waited for `nodes`,
    /// every simulation in the process stopping with them). Only `node`'s thread makes or lets go
    /// of `node`'s value, so none is made meanwhile.
    #[cfg(madsim)]
    fn node_value(&'static self, node: Node) -> &'static T {
        self.registered.call_once(|| REGISTRY.lock().unwrap().push(self));
        let found = self.nodes.lock().unwrap().iter().find(|(of, _)| *of == node).map(|(_, v)| *v);
        if let Some(value) = found {
            return value;
        }
        let value: &'static T = Box::leak(Box::new((self.init)()));
        self.nodes.lock().unwrap().push((node, value));
        if let Some(release) = self.release {
            ENDING.with(|ending| ending.0.borrow_mut().push(Box::new(move || release(value))));
        }
        value
    }
}

/// cladefs: what lets the values a simulation's thread made go of what they hold outside the
/// process's memory, as the thread exits ([`NodeLocal::releasing`]): the last made first.
#[cfg(madsim)]
struct Ending(std::cell::RefCell<Vec<Box<dyn FnOnce()>>>);

#[cfg(madsim)]
impl Drop for Ending {
    fn drop(&mut self) {
        for release in self.0.get_mut().drain(..).rev() {
            release();
        }
    }
}

#[cfg(madsim)]
thread_local! {
    static ENDING: Ending = const { Ending(std::cell::RefCell::new(Vec::new())) };
}

/// Declares a static of a daemon's, one value per process, or per simulated node ([`NodeLocal`]),
/// made at its first use by `$init`, which may not capture anything:
///
/// ```
/// node_local::node_local! {
///     /// What it counts.
///     static COUNTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// }
/// COUNTED.get().fetch_add(1, std::sync::atomic::Ordering::Relaxed);
/// ```
#[macro_export]
macro_rules! node_local {
    ($(#[$attr:meta])* $vis:vis static $name:ident: $ty:ty = $init:expr;) => {
        $(#[$attr])*
        $vis static $name: $crate::NodeLocal<$ty> = $crate::NodeLocal::new(|| $init);
    };
}

/// A simulated node, among those of every simulation the process ran: the thread its simulation
/// runs on, and its node there.
#[cfg(madsim)]
type Node = (std::thread::ThreadId, madsim::task::NodeId);

/// What lets a node's values go: each `NodeLocal` a node used, registered at its first use in one.
#[cfg(madsim)]
trait Reset: Sync {
    fn reset(&self, node: Node);
}

#[cfg(madsim)]
impl<T: Send + Sync + 'static> Reset for NodeLocal<T> {
    fn reset(&self, node: Node) {
        self.nodes.lock().unwrap().retain(|(of, _)| *of != node);
    }
}

#[cfg(madsim)]
static REGISTRY: std::sync::Mutex<Vec<&'static dyn Reset>> = std::sync::Mutex::new(Vec::new());

/// Lets every value of `node`'s go, holding [`REGISTRY`] only to list the statics: no lock of this
/// crate's is held while another is taken (board 5473).
#[cfg(madsim)]
fn reset(node: Node) {
    let locals = REGISTRY.lock().unwrap().clone();
    for local in locals {
        local.reset(node);
    }
}

/// Whether the simulated node this runs in is dying: killed, its tasks dropped, as a host's process
/// dies, closing nothing, waiting for nothing. Never, built normally, nor outside a node. A node
/// [`restart`] starts again is dying until every task of its kill is dropped.
pub fn dying() -> bool {
    #[cfg(madsim)]
    if let Ok(handle) = madsim::runtime::Handle::try_current() {
        return handle.is_exit(madsim::plugin::node());
    }
    false
}

/// Lets the values of the simulated node this runs in go, as it starts: what a node's `init` calls
/// first, so that a node madsim starts again itself, as one whose panic its `restart_on_panic`
/// matches, makes its values anew, as a process does at its boot. One [`kill`] or [`restart`]
/// killed has none already.
#[cfg(madsim)]
pub fn starting() {
    reset((std::thread::current().id(), madsim::plugin::node()));
}

/// Kills simulated `node` as a host's process dies: returns once its tasks are dropped, each seeing
/// it [`dying`], and its values let go of, so that nothing of it is left for its start to find.
/// madsim drops a killed node's tasks only as it next runs them, which the kill wakes, in its run
/// queue's random order, moving its clock on 50 to 100 ns at each: [`DRAINED`] is their dropping's
/// bound, says no more of it than madsim does (0.2.34).
#[cfg(madsim)]
pub async fn kill(node: madsim::task::NodeId) {
    madsim::runtime::Handle::current().kill(node);
    madsim::time::sleep(DRAINED).await;
    reset((std::thread::current().id(), node));
}

/// How long a killed node's tasks take to be dropped, at most ([`kill`]): ten million tasks' worth,
/// more than a simulation holds.
#[cfg(madsim)]
const DRAINED: std::time::Duration = std::time::Duration::from_secs(1);

/// Kills simulated `node` ([`kill`]), killed or exited already or not, and starts it again, as a
/// host's process restarts: no task of its start runs before its kill's are gone, nor finds a
/// value they made (board 5908: madsim's own restart drops the killed node's tasks as its start
/// runs, which, its values not yet let go of, takes up what they left).
#[cfg(madsim)]
pub async fn restart(node: madsim::task::NodeId) {
    kill(node).await;
    madsim::runtime::Handle::current().restart(node);
}
