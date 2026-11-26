// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined's one seam over libmachine: a running machine ([`Vm`]), made from a kernel or a
//! snapshot, its guest's agent and vsock, stopped, snapshotted, suspended. Nothing else of
//! machined touches a [`Machine`].
//!
//! Today each machine is a [`Machine`] in this process, run by a thread of its own, which owns it
//! and does between runs what is asked of it, as libmachine's `Sandbox` does (snapshots,
//! suspensions). On macOS, Hypervisor.framework allows one VM per process, so until each machine
//! runs in a helper process (board 13460, which replaces this module's insides behind the same
//! [`Vm`]) machined runs one at a time there ([`MAX_RUNNING`]).
//!
//! machined uses libmachine's synchronous `Machine` rather than its `Sandbox`, which waits for
//! libmachine's agent before it is made (a VM manager's guest may run an agent of its own, never
//! libmachine's), keeps its console to itself, and is KVM's alone (board 13399): hotbed
//! question 569.

use std::io::Write;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(target_os = "linux")]
use std::time::Duration;

use libmachine::agent::Agent;
use libmachine::{ExitReason, LinuxBoot, Machine, MachineConfig, StopHandle, Vsock, Woken};
use tokio::sync::{oneshot, watch};

#[cfg(target_os = "linux")]
use libmachine::{Error, Result, Snapshot};

/// How many machines may run at once in this process: on macOS one, Hypervisor.framework's limit,
/// until board 13460 runs each in a helper process.
pub(crate) const MAX_RUNNING: Option<usize> =
    if cfg!(target_os = "macos") { Some(1) } else { None };

/// Whether machines here can have a network: on KVM alone (board 13399).
pub(crate) const NETWORK: bool = cfg!(target_os = "linux");

/// The largest file read from a guest through its agent: what a guest sends is untrusted.
pub(crate) const MAX_FILE: usize = 64 << 20;

/// How long a suspension waits for the guest to be idle.
#[cfg(target_os = "linux")]
const SUSPEND_IDLE_WAIT: Duration = Duration::from_secs(2);
/// How long the guest runs between looks while a suspension waits for it to be idle.
#[cfg(target_os = "linux")]
const IDLE_POLL: Duration = Duration::from_millis(2);
/// The run thread only waits: its stack needs little.
const THREAD_STACK: usize = 256 << 10;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a machine starts from.
pub(crate) enum Source {
    /// A kernel, booted from scratch.
    Boot(LinuxBoot),
    /// A snapshot, restored.
    #[cfg(target_os = "linux")]
    Snapshot(Arc<Snapshot>),
}

/// How a machine ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// [`Vm::stop`] stopped it.
    Stopped,
    /// Its guest stopped it: powered off, reset, triple-faulted, or made an exit libmachine does
    /// not handle.
    Guest(ExitReason),
    /// Running it failed: the hypervisor's error.
    Failed(String),
}

/// A running machine. Dropping it stops it.
pub(crate) struct Vm {
    vsock: Vsock,
    agent: Agent,
    stop: StopHandle,
    shared: Arc<Shared>,
    ended: watch::Receiver<Option<Ended>>,
}

/// What a [`Vm`] and its run thread share.
struct Shared {
    /// Set before the stop that ends the machine, so that the run thread tells it from a request's.
    closing: AtomicBool,
    /// What the run thread is asked to do between runs; None once the machine is gone.
    requests: Mutex<Option<Vec<Request>>>,
    /// Whether the guest is suspended, as the run thread last left it.
    suspended: watch::Sender<bool>,
}

/// What the run thread is asked to do between runs.
enum Request {
    #[cfg(target_os = "linux")]
    Snapshot { path: PathBuf, reply: oneshot::Sender<Result<Snapshot>> },
    #[cfg(target_os = "linux")]
    Suspend { dir: PathBuf, reply: oneshot::Sender<Result<()>> },
    #[cfg(target_os = "linux")]
    Resume { reply: oneshot::Sender<()> },
}

impl Vm {
    /// Makes the machine and runs it on a thread of its own, its console written to `console`.
    /// Blocks while the kernel loads (or the snapshot maps): call it off async code.
    pub(crate) fn start(
        config: MachineConfig,
        source: Source,
        console: impl Write + Send + 'static,
    ) -> libmachine::Result<Self> {
        let mut machine = match source {
            Source::Boot(boot) => Machine::boot_linux(config, &boot)?,
            #[cfg(target_os = "linux")]
            Source::Snapshot(snapshot) => Machine::restore(&snapshot, config)?,
        };
        machine.set_console(console);
        let vsock = machine.vsock().expect("a machine booting Linux has a vsock");
        let agent = Agent::new(vsock.clone()).max_file_size(MAX_FILE);
        let stop = machine.stop_handle();
        let shared = Arc::new(Shared {
            closing: AtomicBool::new(false),
            requests: Mutex::new(Some(Vec::new())),
            suspended: watch::channel(false).0,
        });
        let (ended_tx, ended) = watch::channel(None);
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("machined-run".into())
            .stack_size(THREAD_STACK)
            .spawn(move || run(machine, &thread_shared, &ended_tx))
            .map_err(|source| libmachine::Error::Hypervisor {
                op: "start a machine's run thread",
                source,
            })?;
        Ok(Self { vsock, agent, stop, shared, ended })
    }

    /// The host's end of the guest's vsock.
    pub(crate) fn vsock(&self) -> &Vsock {
        &self.vsock
    }

    /// libmachine's agent in the guest (blocking calls).
    pub(crate) fn agent(&self) -> Agent {
        self.agent.clone()
    }

    /// Whether the guest is suspended, and its changes.
    pub(crate) fn suspended(&self) -> watch::Receiver<bool> {
        self.shared.suspended.subscribe()
    }

    /// Stops the machine (its run thread then frees it): [`wait`](Self::wait) says when.
    pub(crate) fn stop(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.stop.stop();
    }

    /// Waits until the machine has ended and is freed; says how.
    pub(crate) async fn wait(&self) -> Ended {
        let mut ended = self.ended.clone();
        let outcome = match ended.wait_for(Option::is_some).await {
            Ok(ended) => ended.clone(),
            Err(_) => None,
        };
        // The run thread drops its sender only after saying how it ended, unless it panicked.
        outcome.unwrap_or_else(|| Ended::Failed("the machine's run thread panicked".into()))
    }

    /// Writes a snapshot of the machine to `path`, and opens it: the guest is paused for it, and
    /// runs on.
    #[cfg(target_os = "linux")]
    pub(crate) async fn snapshot(&self, path: PathBuf) -> Result<Snapshot> {
        self.ask(|reply| Request::Snapshot { path, reply })
            .await
            .unwrap_or_else(|| Err(gone("no snapshot")))
    }

    /// Suspends the guest once it is idle, within [`SUSPEND_IDLE_WAIT`], into a file in `dir`.
    #[cfg(target_os = "linux")]
    pub(crate) async fn suspend(&self, dir: PathBuf) -> Result<()> {
        self.ask(|reply| Request::Suspend { dir, reply })
            .await
            .unwrap_or_else(|| Err(gone("no suspension")))
    }

    /// Wakes a suspended guest; returns once it runs again.
    #[cfg(target_os = "linux")]
    pub(crate) async fn resume(&self) -> Result<()> {
        self.ask(|reply| Request::Resume { reply }).await.ok_or_else(|| gone("not resumed"))
    }

    /// Hands the run thread a request and stops the guest, so that the thread takes it at once:
    /// the answer, or None if the machine ended first. The reply's sender lives only in the
    /// queue, which the run thread empties or drops, so no request waits on a machine gone.
    #[cfg(target_os = "linux")]
    async fn ask<T>(&self, request: impl FnOnce(oneshot::Sender<T>) -> Request) -> Option<T> {
        let answer = lock(&self.shared.requests).as_mut().map(|requests| {
            let (reply, answer) = oneshot::channel();
            requests.push(request(reply));
            answer
        })?;
        self.stop.stop();
        answer.await.ok()
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "linux")]
fn gone(what: &str) -> Error {
    Error::GuestGone(format!("{what}: the machine ended"))
}

/// The run thread: runs the machine, doing what is asked between runs, until it ends; frees it,
/// then says how it ended.
fn run(mut machine: Machine, shared: &Shared, ended: &watch::Sender<Option<Ended>>) {
    let how = loop {
        match machine.run() {
            Ok(ExitReason::Stopped | ExitReason::TimedOut)
                if shared.closing.load(Ordering::SeqCst) =>
            {
                break Ended::Stopped;
            }
            // Any other stop is a request's.
            Ok(ExitReason::Stopped | ExitReason::TimedOut) => {
                if let Some(how) = between_runs(&mut machine, shared) {
                    break how;
                }
            }
            Ok(reason) => break Ended::Guest(reason),
            Err(e) => break Ended::Failed(e.to_string()),
        }
    };
    // What is still asked for fails, its senders dropped.
    lock(&shared.requests).take();
    shared.suspended.send_replace(false);
    drop(machine);
    ended.send_replace(Some(how));
}

/// Between runs: does what is asked, and while the guest is suspended waits until something wakes
/// it. None when the guest is to run again; how the machine ended if it did meanwhile.
fn between_runs(machine: &mut Machine, shared: &Shared) -> Option<Ended> {
    loop {
        let requests = lock(&shared.requests).as_mut().map(std::mem::take).unwrap_or_default();
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut, reason = "no resumes without KVM"))]
        let mut resumed: Vec<oneshot::Sender<()>> = Vec::new();
        #[cfg_attr(
            not(target_os = "linux"),
            allow(clippy::never_loop, reason = "requests are KVM's alone (board 13399)")
        )]
        for request in requests {
            match request {
                #[cfg(target_os = "linux")]
                Request::Snapshot { path, reply } => {
                    let taken = machine.snapshot(&path).and_then(|()| Snapshot::open(&path));
                    // The caller may have gone: the snapshot stays, as its file.
                    let _ = reply.send(taken);
                }
                #[cfg(target_os = "linux")]
                Request::Suspend { dir, reply } => match suspend_when_idle(machine, &dir, shared) {
                    Ok(done) => {
                        // A caller gone leaves the guest suspended, as asked.
                        let _ = reply.send(done);
                    }
                    Err(how) => return Some(how),
                },
                #[cfg(target_os = "linux")]
                Request::Resume { reply } => resumed.push(reply),
            }
        }
        if !resumed.is_empty() || !machine.suspended() {
            shared.suspended.send_replace(false);
            // Answered once `suspended` says so; a caller gone needs no answer.
            for reply in resumed {
                let _ = reply.send(());
            }
            return None;
        }
        // A request that came while this one was taken: take it first (its stop was used up).
        if lock(&shared.requests).as_ref().is_some_and(|r| !r.is_empty()) {
            continue;
        }
        shared.suspended.send_replace(true);
        match machine.wait_woken(None) {
            Woken::Device | Woken::TimedOut => {
                shared.suspended.send_replace(false);
                return None;
            }
            Woken::Stop if shared.closing.load(Ordering::SeqCst) => return Some(Ended::Stopped),
            // A request: take it.
            Woken::Stop => {}
        }
    }
}

/// Suspends the guest into a file in `dir` once it is idle, running it in short slices meanwhile,
/// for at most [`SUSPEND_IDLE_WAIT`]: the suspension's outcome, or how the machine ended if it did
/// meanwhile.
#[cfg(target_os = "linux")]
fn suspend_when_idle(
    machine: &mut Machine,
    dir: &std::path::Path,
    shared: &Shared,
) -> std::result::Result<Result<()>, Ended> {
    if machine.suspended() {
        return Ok(Ok(()));
    }
    let until = std::time::Instant::now() + SUSPEND_IDLE_WAIT;
    loop {
        match machine.suspend(dir) {
            Ok(()) => {
                shared.suspended.send_replace(true);
                return Ok(Ok(()));
            }
            Err(Error::Busy(why)) if std::time::Instant::now() >= until => {
                return Ok(Err(Error::Busy(format!(
                    "the guest was not idle within {SUSPEND_IDLE_WAIT:?}: {why}"
                ))));
            }
            Err(Error::Busy(_)) => match machine.run_for(IDLE_POLL) {
                Ok(ExitReason::Stopped) if shared.closing.load(Ordering::SeqCst) => {
                    return Err(Ended::Stopped);
                }
                // Another request's stop: it is still queued, taken after this one.
                Ok(ExitReason::Stopped | ExitReason::TimedOut) => {}
                Ok(reason) => return Err(Ended::Guest(reason)),
                Err(e) => return Err(Ended::Failed(e.to_string())),
            },
            Err(e) => return Ok(Err(e)),
        }
    }
}
