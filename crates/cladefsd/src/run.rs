// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: where the daemon's work runs, the one seam between its threads and the simulation's one
//! (doc-24 steps 11 and 12, TASK-685.3). The daemon's code is the same in both: what differs is
//! the executor. In production, its async services share the daemon's consolidated Tokio runtime
//! (`Daemon::runtime`), each supervised so a panic aborts the daemon (`Awaited`); its periodic
//! blocking work runs on threads that sleep between turns; and work that blocks on the host's disk
//! runs on the runtime's blocking threads. Under madsim, which runs one thread, each is a task of
//! the simulated host's, on the simulation's clock, and the disk's work runs inline: a simulated
//! host's disk is memory.

use crate::Daemon;
use std::future::Future;
use std::time::Duration;

/// Runs `service`, which runs as long as `daemon` does, named `name`: as a supervised task on
/// `daemon`'s shared runtime (outside madsim) or on the simulated node's runtime (under madsim).
/// A panic in `service` aborts the daemon naming `name` (`local_cell::shared::Awaited`, board
/// 8501), as a thread-based service's [`supervised`] does.
pub(super) fn service<F>(
    daemon: &Daemon,
    name: &'static str,
    service: impl FnOnce() -> F + Send + 'static,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    #[cfg(not(madsim))]
    drop(daemon.spawn(supervising(name, async move { service().await })));
    #[cfg(madsim)]
    {
        let _ = (daemon, name);
        drop(tokio::spawn(service()));
    }
    Ok(())
}

/// cladefs: `service`, a task of the daemon's that runs as long as it does, named `name`, wherever
/// it is spawned: a panic in it aborts the daemon naming `name` (`local_cell::shared::Awaited`,
/// board 8501), as [`service`]'s does; for a task spawned on a runtime of its own, as the bucket
/// runtime's replica sweep (board 9414). A test has it panic as it starts with the failpoint
/// `cladefsd::service::NAME`. Under madsim, `service` as it is: a panic there the simulation's.
pub(super) async fn supervising(name: &'static str, service: impl Future<Output = ()>) {
    #[cfg(not(madsim))]
    let _supervised = local_cell::shared::Awaited::new(name);
    #[cfg(not(madsim))]
    fail::fail_point!(&format!("cladefsd::service::{name}"));
    #[cfg(madsim)]
    let _ = name;
    service.await;
}

/// cladefs: the connections `incoming` accepts for the daemon's server `what` names (`a peer's`,
/// `a control API caller's`), a failure the host may get over (no descriptor free, EMFILE or
/// ENFILE, no buffers, a connection gone before it was accepted) logged once and tried again after
/// a pause, [`super::server::ACCEPT_AGAIN`], as the VM sockets are (board 8573, board 9760):
/// served by tonic as they come, such a failure was passed over at once, again and again, a busy
/// loop holding a thread while the descriptors stayed short. One of the listener's own (it is
/// closed, or no listener: EBADF, EINVAL, ENOTSOCK, EOPNOTSUPP) ends them, noted in `failed`,
/// which [`accepted_until`] returns.
#[cfg(not(madsim))]
pub(super) fn accepting<IO>(
    what: &'static str,
    incoming: impl futures::Stream<Item = std::io::Result<IO>> + Unpin,
    failed: std::sync::Arc<std::sync::OnceLock<std::io::Error>>,
) -> impl futures::Stream<Item = std::io::Result<IO>> {
    use futures::StreamExt as _;
    futures::stream::unfold((incoming, false), move |(mut incoming, mut failing)| {
        let failed = failed.clone();
        async move {
            loop {
                match incoming.next().await? {
                    Ok(accepted) => return Some((Ok(accepted), (incoming, false))),
                    // That connection's, gone before it was accepted.
                    Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted => {}
                    Err(err) if for_good(&err) => {
                        // The first error for good; there is no other, the stream ending here.
                        let _ = failed.set(err);
                        return None;
                    }
                    Err(err) => {
                        if !failing {
                            log::error!(
                                "cladefsd: accepting {what} connection: {err}: trying again \
                                 every {:?}",
                                crate::server::ACCEPT_AGAIN
                            );
                            failing = true;
                        }
                        tokio::time::sleep(crate::server::ACCEPT_AGAIN).await;
                    }
                }
            }
        }
    })
}

/// cladefs: what a server serving the connections of [`accepting`] ended with, `served`: its own
/// failure, or, its connections ended, the listener's failure for good that ended them, noted in
/// `failed`; or, if none was, that `what` ended. Never `Ok`: a server of the daemon's serves as
/// long as it does.
#[cfg(not(madsim))]
pub(super) fn accepted_until(
    what: &str,
    served: Result<(), tonic::transport::Error>,
    failed: std::sync::Arc<std::sync::OnceLock<std::io::Error>>,
) -> std::io::Result<()> {
    served.map_err(std::io::Error::other)?;
    // Its connections ended, their stream dropped with its note of why.
    let failed = std::sync::Arc::into_inner(failed).and_then(std::sync::OnceLock::into_inner);
    Err(failed.unwrap_or_else(|| std::io::Error::other(format!("{what} ended"))))
}

/// cladefs: whether `err`, an accept's, is of the listener itself, which no retry gets over.
#[cfg(not(madsim))]
fn for_good(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EOPNOTSUPP | libc::EFAULT)
    )
}

/// Runs `turn` now and every `period` after, until it says to stop (false), named `name`: on a
/// thread of its own ([`thread`]), which `wait` has wait `period` between turns (a sleep, or a wait something
/// may end early); under madsim, a task that sleeps `period` on the simulation's clock.
pub(super) fn every(
    name: &'static str,
    period: Duration,
    wait: impl Fn(Duration) + Send + 'static,
    mut turn: impl FnMut() -> bool + Send + 'static,
) -> std::io::Result<()> {
    #[cfg(not(madsim))]
    thread(name, move || {
        while turn() {
            wait(period);
        }
    })?;
    #[cfg(madsim)]
    {
        let _ = (name, wait);
        drop(tokio::spawn(async move {
            while turn() {
                tokio::time::sleep(period).await;
            }
        }));
    }
    Ok(())
}

/// cladefs: runs `work`, a service of the daemon's that runs as long as it does, on a thread of its
/// own named `name`, [`supervised`]: every service's thread is made here, or, made elsewhere, as
/// the signal thread is, says so first (board 8501).
pub(super) fn thread(
    name: &'static str,
    work: impl FnOnce() + Send + 'static,
) -> std::io::Result<()> {
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let _supervised = supervised(name);
        work()
    })?;
    Ok(())
}

/// cladefs: the calling thread doing the daemon's service `name`, a duty of the whole process's,
/// until what this returns drops (board 8501, hotbed question 446): a panic on it aborts the
/// process, its log naming the service, once the process asks for that, as `cladefsd` does
/// (`local_cell::shared::abort_on_panics`), as a panic on the page cache's flusher does (hotbed
/// question 112). Unwound instead, the thread gone, the daemon would serve on half-dead: its
/// memory or disk ungoverned, its VM sockets bound but accepting no more, its health SERVING, its
/// drain never to come. A test has a service panic as it starts with the failpoint
/// `cladefsd::service::NAME` (`FAILPOINTS`, in a daemon built with them). Not under madsim, whose
/// one thread every task of every simulated host shares: there a service is a task, whose panic
/// the simulation reports.
pub fn supervised(name: &'static str) -> local_cell::shared::Holding {
    let supervised = local_cell::shared::Holding::work(name);
    fail::fail_point!(&format!("cladefsd::service::{name}"));
    supervised
}

/// Runs `work`, which blocks on the host's disk, off the runtime's workers: on one of its
/// blocking threads, as it finishes whether or not what awaits it is dropped; under madsim, inline,
/// the simulated disk being memory. Fails if `work` panicked. cladefs: in the trace context current
/// where it is asked, the parent of its spans there, as a control or peer call's (board 8569).
pub(super) async fn off_the_runtime<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    #[cfg(not(madsim))]
    {
        let parent = bucket_cell::trace::Parent::current();
        let done =
            tokio::task::spawn_blocking(move || parent.run(|| host_fs::may_block(work))).await;
        return done.map_err(|err| err.to_string());
    }
    #[cfg(madsim)]
    return std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host_fs::may_block(work)))
        .map_err(|_| "it panicked".to_owned());
}
