// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's drain, what SIGTERM has it do before it exits (TASK-141.4): its cells
//! closed, so durable, and their commits uploaded, as a host upgrading its daemon needs before it
//! starts the new one (backlog doc-11, `docs/upgrading-a-host.md`). Before, SIGTERM had the health
//! service say NOT_SERVING and the daemon exit, its cells as a crash leaves them: their last
//! ordered commits not durable, and none of what was durable since the last upload uploaded.
//!
//! [`drain`] does, in order:
//!
//! 1. The health service says NOT_SERVING ([`super::stop_serving`]), within [`REPORTING`]: past
//!    it, its reporter gone, the drain goes on, and fails (board 8503).
//! 2. The daemon drains: its VM sockets take no more connections, the daemon's own and every
//!    tenant's, their listeners shut down; the control API refuses calls (`UNAVAILABLE`); and
//!    every VM process's connection is closed, its VM told its daemon went, as at any daemon's
//!    exit (`CladefsdLost`), its requests received served, its commits made durable and uploaded,
//!    the open segment's too, which a cell closed leaves for the next to open it, and its cell
//!    closed, as a connection whose VM process goes closes it.
//! 3. It waits for the connections to be gone and the control API's calls in flight to be done
//!    ([`Work`]); closes its FUSE mount of cells, if it has one, unmounted lazily, its cells
//!    written back and their cells closed once nothing uses it ([`super::mount::close`],
//!    TASK-475.2); closes the versions it keeps open for forks; and waits for every cell's uploader
//!    to have uploaded what its cell made durable, and stopped, as each does as its cell closes
//!    ([`crate::bucket::uploaders`]).
//!
//! All of it within a bound, `CLADEFSD_DRAIN` seconds ([`bound`]): past it, what is left is
//! logged, and the daemon exits anyway, failing; its cells' commits that weren't durable are
//! lost, as at a crash, and those durable that weren't uploaded are uploaded by the next daemon to
//! open their cells. cladefs: the daemon's last spans, if it exports them, are exported in what is
//! left of the bound once the drain is done, or not at all (`super::otel::shutdown`, board 8569):
//! a collector that never answers holds its exit no longer.

use crate::Daemon;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Whether `daemon` drains.
pub(super) fn draining(daemon: &Daemon) -> bool {
    *daemon.draining.borrow()
}

/// Returns once `daemon` drains: at once if it does.
pub(super) async fn started(daemon: &Daemon) {
    let mut draining = daemon.draining.subscribe();
    // The sender lives as long as `daemon`.
    let _ = draining.wait_for(|draining| *draining).await;
}

/// How long a drain takes at most when `CLADEFSD_DRAIN` names none: long enough for a few
/// cells' last uploads, short of an orchestrator's patience before it kills.
pub const DEFAULT_BOUND: Duration = Duration::from_secs(60);

/// How long a drain takes at most: `CLADEFSD_DRAIN` seconds, or [`DEFAULT_BOUND`].
pub fn bound() -> Result<Duration, String> {
    match node_local::env::var("CLADEFSD_DRAIN") {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_BOUND),
        Ok(secs) => secs
            .parse()
            .map(Duration::from_secs)
            .map_err(|_| format!("CLADEFSD_DRAIN {secs:?} is no number of seconds")),
        Err(err) => Err(format!("CLADEFSD_DRAIN: {err}")),
    }
}

/// cladefs: how long a drain waits at most, of its bound, for the health service to report
/// NOT_SERVING ([`super::stop_serving`]): in the process, it takes a moment; past this, its
/// reporter is gone, which would report it never, and the drain goes on without, so as to close
/// the cells all the same, failing (board 8503, hotbed question 447). Before, it waited for ever,
/// outside its bound, the daemon neither drained nor exited.
pub const REPORTING: Duration = Duration::from_secs(1);

/// What a drain left, past its bound: nothing of it if it drained.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Left {
    /// cladefs: whether the health service was left saying SERVING, not having reported
    /// NOT_SERVING within [`REPORTING`] (board 8503).
    pub health: bool,
    /// VM processes' connections not yet gone, their cells open.
    pub connections: usize,
    /// The control API's calls in flight.
    pub calls: usize,
    /// Cells whose uploaders have not stopped: open, or closing, uploading still.
    pub uploaders: usize,
    /// Whether the FUSE mount of cells was left: not closed, as a file open in it holds it, or
    /// closed failing, as a cell's write-back failed (logged).
    pub mount: bool,
}

impl std::fmt::Display for Left {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { health, connections, calls, uploaders, mount } = self;
        write!(
            f,
            "{connections} VM processes' connections, {calls} control calls and {uploaders} \
             cells' uploaders left"
        )?;
        if *mount {
            write!(f, ", and the FUSE mount of cells")?;
        }
        match health {
            true => write!(f, "; the health service never reported NOT_SERVING"),
            false => Ok(()),
        }
    }
}

/// Drains `daemon`, as the module says, within `within`: `Err` with what is left past it, or
/// if the health service never said it stopped serving.
/// cladefs: a future, awaited where the daemon is told to drain, its signal's thread blocking on it
/// there, its outermost entry (doc-24 step 12).
pub async fn drain_daemon(daemon: &Arc<Daemon>, within: Duration) -> Result<(), Left> {
    let began = tokio::time::Instant::now();
    let reported = tokio::time::timeout(REPORTING.min(within), daemon.stop_serving()).await;
    let health = reported.is_err();
    if health {
        log::error!(
            "cladefsd: draining: the health service reported NOT_SERVING not within \
             {REPORTING:?}: draining all the same"
        );
    }
    daemon.draining.send_replace(true);
    crate::server::stop_accepting(daemon);
    let drained = async {
        tokio::time::timeout(within.saturating_sub(began.elapsed()), async {
            daemon.connections_work.done().await;
            daemon.calls_work.done().await;
            let mount = crate::mount::close_on(daemon).await;
            if let Err(err) = &mount {
                log::error!("cladefsd: draining: {err:#}");
            }
            crate::cells::close_all_kept(daemon).await;
            let mut uploaders = cladefs_core::bucket::uploaders_on(&daemon.host);
            let uploaded = uploaders.wait_for(|uploading| *uploading == 0).await.is_ok();
            mount.is_ok() && uploaded
        })
        .await
    };
    match drained.await {
        Ok(true) if !health => Ok(()),
        _ => Err(Left {
            health,
            connections: daemon.connections_work.count(),
            calls: daemon.calls_work.count(),
            uploaders: *cladefs_core::bucket::uploaders_on(&daemon.host).borrow(),
            mount: crate::mount::left_on(daemon),
        }),
    }
}

/// Drains the process's default daemon within `within`.
#[cfg(not(madsim))]
pub async fn drain(within: Duration) -> Result<(), Left> {
    drain_daemon(Daemon::process(), within).await
}

/// cladefs: work of a kind that a drain waits for, counted as it begins and ends ([`Work::begin`]):
/// VM processes' connections, the control API's calls. Owned by [`Daemon`], so each daemon counts
/// its own (board 5806).
pub(super) struct Work(Arc<watch::Sender<usize>>);

impl Work {
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }

    /// Begins work of the kind on `daemon`, until what it returns is dropped; or none, `None`,
    /// once `daemon` drains: counted first, and given up if then the daemon drains, so that a
    /// drain that finds none counted finds none begun after, whatever their order.
    pub fn begin(&self, daemon: &Daemon) -> Option<Doing> {
        self.0.send_modify(|count| *count += 1);
        let doing = Doing(Arc::clone(&self.0));
        (!draining(daemon)).then_some(doing)
    }

    /// How much of it is under way.
    pub fn count(&self) -> usize {
        *self.0.borrow()
    }

    /// Returns once none of it is under way.
    pub(super) async fn done(&self) {
        let mut count = self.0.subscribe();
        // The sender lives while `Work` does.
        let _ = count.wait_for(|count| *count == 0).await;
    }
}

/// Work under way ([`Work::begin`]), until dropped: of the count it began in, wherever it is
/// dropped.
pub(super) struct Doing(Arc<watch::Sender<usize>>);

impl Drop for Doing {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}
