// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: where the daemon's work runs, the one seam between its threads and the simulation's one
//! (doc-24 steps 11 and 12, TASK-685.3). The daemon's code is the same in both: what differs is
//! the executor. In production, its services run on threads of their own, each driving a runtime
//! of its own, so that one's calls keep to its threads; its periodic work on threads that sleep
//! between turns; and work that blocks on the host's disk on a runtime's blocking threads. Under
//! madsim, which runs one thread, each is a task of the simulated host's, on the simulation's
//! clock, and the disk's work runs inline: a simulated host's disk is memory.

use std::future::Future;
use std::time::Duration;

/// Runs `service`, which runs as long as the daemon does, named `name`: on a thread of its own
/// ([`thread`]), driving a runtime of its own of `threads` workers; under madsim, a task of the
/// node's. A runtime that can't be made, as with no threads to be had, stops the daemon: it would
/// serve without the service.
pub(super) fn service<F>(
    name: &'static str,
    threads: usize,
    service: impl FnOnce() -> F + Send + 'static,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    #[cfg(not(madsim))]
    thread(name, move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name(name)
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(service()),
            Err(err) => {
                log::error!("cladefsd: {name}'s runtime: {err}");
                std::process::exit(1);
            }
        }
    })?;
    #[cfg(madsim)]
    {
        let _ = (name, threads);
        drop(tokio::spawn(service()));
    }
    Ok(())
}

/// Runs `turn` now and every `period` after, until it says to stop (false), named `name`: on a
/// thread of its own ([`thread`]), which `wait` has wait `period` between turns (a sleep, or a wait something
/// may end early); under madsim, a task that sleeps `period` on the simulation's clock.
pub(super) fn every(
    name: &'static str,
    period: Duration,
    wait: fn(Duration),
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
/// the simulated disk being memory. Fails if `work` panicked.
pub(super) async fn off_the_runtime<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    #[cfg(not(madsim))]
    return tokio::task::spawn_blocking(work).await.map_err(|err| err.to_string());
    #[cfg(madsim)]
    return std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .map_err(|_| "it panicked".to_owned());
}
