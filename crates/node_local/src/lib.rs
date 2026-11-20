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
//! so that two daemons in one simulation keep their own; and, the simulation's runtime holding the
//! [`Plugin`], a node killed has its values let go of, so that it starts again with none, as a
//! process does. Outside a node, as a test's own code is, the process's.
//!
//! A node's value is leaked, as a static's is: a value let go of at its node's kill stays where
//! its holders, the node's tasks, which the kill dropped, left it. A simulation leaks a value of
//! each static its killed nodes used, no more.
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
    #[cfg(madsim)]
    nodes: std::sync::Mutex<Vec<(madsim::task::NodeId, &'static T)>>,
    #[cfg(madsim)]
    registered: std::sync::Once,
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
        }
    }

    /// Its value: the process's, or, in a simulated node, the node's.
    pub fn get(&'static self) -> &'static T {
        #[cfg(madsim)]
        if madsim::runtime::Handle::try_current().is_ok() {
            return self.node_value(madsim::plugin::node());
        }
        self.value.get_or_init(self.init)
    }

    /// The value of `node`'s, made if it has none.
    #[cfg(madsim)]
    fn node_value(&'static self, node: madsim::task::NodeId) -> &'static T {
        self.registered.call_once(|| REGISTRY.lock().unwrap().push(self));
        let mut nodes = self.nodes.lock().unwrap();
        if let Some((_, value)) = nodes.iter().find(|(of, _)| *of == node) {
            return value;
        }
        let value: &'static T = Box::leak(Box::new((self.init)()));
        nodes.push((node, value));
        value
    }
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

/// What lets a node's values go: each `NodeLocal` a node used, registered at its first use in one.
#[cfg(madsim)]
trait Reset: Sync {
    fn reset(&self, node: madsim::task::NodeId);
}

#[cfg(madsim)]
impl<T: Send + Sync + 'static> Reset for NodeLocal<T> {
    fn reset(&self, node: madsim::task::NodeId) {
        self.nodes.lock().unwrap().retain(|(of, _)| *of != node);
    }
}

#[cfg(madsim)]
static REGISTRY: std::sync::Mutex<Vec<&'static dyn Reset>> = std::sync::Mutex::new(Vec::new());

/// madsim's simulator plugin that lets a killed node's values go, so that it starts again with
/// none: a simulation's runtime adds it (`Runtime::add_simulator::<node_local::Plugin>()`).
#[cfg(madsim)]
pub struct Plugin;

#[cfg(madsim)]
impl madsim::plugin::Simulator for Plugin {
    fn new(_: &madsim::rand::GlobalRng, _: &madsim::time::TimeHandle, _: &madsim::Config) -> Self {
        Plugin
    }

    fn reset_node(&self, node: madsim::task::NodeId) {
        for local in REGISTRY.lock().unwrap().iter() {
            local.reset(node);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    static MADE: AtomicU64 = AtomicU64::new(0);

    node_local! {
        static COUNTER: AtomicU64 = { MADE.fetch_add(1, Ordering::Relaxed); AtomicU64::new(0) };
    }

    /// Built normally, one value for the process, made once, at its first use.
    #[test]
    fn statics_are_the_processs_built_normally() {
        COUNTER.get().fetch_add(2, Ordering::Relaxed);
        COUNTER.get().fetch_add(3, Ordering::Relaxed);
        assert_eq!(COUNTER.get().load(Ordering::Relaxed), 5);
        assert_eq!(MADE.load(Ordering::Relaxed), 1);
    }
}
