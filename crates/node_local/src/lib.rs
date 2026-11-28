// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the OS/process boundary shim for the full-stack madsim simulation (doc-24, board 10962).
//!
//! Each daemon keeps its own state on an explicit host/daemon object (`Daemon`, `cladefs::Host`,
//! `local_cell::Host`, `bucket_cell::Host`), never in per-node statics. Only the OS/process
//! boundary that madsim does not virtualize lives here:
//!
//! - [`env`]: process environment variables, per simulated node under `--cfg madsim` and
//!   [`std::env`] otherwise.
//! - [`dying`]: whether the simulated node this runs in has been killed.
//! - [`generation`], [`starting`], [`kill`], [`restart`] (under `--cfg madsim`): simulated node
//!   lifecycle — draining a killed node's tasks before its restart runs, clearing its simulated
//!   environment, and advancing its generation so `host_fs` crashes its in-memory disk on next use.

pub mod env;

/// A simulated node, among those of every simulation the process ran: the thread its simulation
/// runs on, and its node there.
#[cfg(madsim)]
pub(crate) type Node = (std::thread::ThreadId, madsim::task::NodeId);

/// Each simulated node's boot generation, bumped on [`starting`] and [`kill`] so that `host_fs`
/// knows when a restarted node first touches its disk after a crash.
#[cfg(madsim)]
static GENERATIONS: std::sync::Mutex<Vec<(Node, u64)>> = std::sync::Mutex::new(Vec::new());

/// The current simulated node's boot generation: `0` outside a simulation node, incremented each
/// time the node is killed or started anew ([`kill`], [`starting`]).
#[cfg(madsim)]
pub fn generation() -> u64 {
    if madsim::runtime::Handle::try_current().is_err() {
        return 0;
    }
    let node = (std::thread::current().id(), madsim::plugin::node());
    GENERATIONS.lock().unwrap().iter().find(|(of, _)| *of == node).map_or(0, |(_, count)| *count)
}

/// Clears `node`'s simulated environment and advances its generation.
#[cfg(madsim)]
fn reset(node: Node) {
    env::clear(node);
    let mut gens = GENERATIONS.lock().unwrap();
    match gens.iter_mut().find(|(of, _)| *of == node) {
        Some((_, count)) => *count += 1,
        None => gens.push((node, 1)),
    }
}

/// cladefs: the incarnation of the simulated host something was made on (board 14567): what its
/// drop asks whether that incarnation died since, killed or started anew, so that what it had under
/// way goes as a killed process's does, waiting for nothing and closing nothing. madsim drops a
/// killed node's tasks only as they next run, so a task one of their drops spawned may be dropped
/// after the node started again, no longer [`dying`]. Built normally, never gone.
#[derive(Clone, Copy, Debug)]
pub struct Incarnation {
    #[cfg(madsim)]
    generation: u64,
}

impl Incarnation {
    /// The incarnation of the host this runs on.
    pub fn now() -> Self {
        Self {
            #[cfg(madsim)]
            generation: generation(),
        }
    }

    /// Whether it died: its host dying, or started anew since.
    pub fn is_gone(&self) -> bool {
        #[cfg(madsim)]
        if dying() || self.generation != generation() {
            return true;
        }
        false
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

/// Clears the simulated node's environment and advances its generation, as it starts: what a
/// node's `init` calls first, so that a node madsim starts again itself, as one whose panic its
/// `restart_on_panic` matches, starts afresh as a process does at its boot. One [`kill`] or
/// [`restart`] killed was reset already.
#[cfg(madsim)]
pub fn starting() {
    reset((std::thread::current().id(), madsim::plugin::node()));
}

/// Kills simulated `node` as a host's process dies: returns once its tasks are dropped, each seeing
/// it [`dying`], and its environment cleared and generation bumped, so that nothing of it is left
/// for its start to find. madsim drops a killed node's tasks only as it next runs them, which the
/// kill wakes, in its run queue's random order, moving its clock on 50 to 100 ns at each:
/// [`DRAINED`] is their dropping's bound, says no more of it than madsim does (0.2.34).
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
/// host's process restarts: no task of its start runs before its kill's are gone, nor sees the
/// previous boot's environment or generation (board 5908: madsim's own restart drops the killed
/// node's tasks as its start runs).
#[cfg(madsim)]
pub async fn restart(node: madsim::task::NodeId) {
    kill(node).await;
    madsim::runtime::Handle::current().restart(node);
}
