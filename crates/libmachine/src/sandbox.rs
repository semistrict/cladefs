// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The high-level, async API: a [`Sandbox`] is a Linux guest with
//! libmachine's agent in it, ready to run commands and take files, driven
//! from tokio without blocking the caller's tasks.
//!
//! ```no_run
//! # async fn demo() -> libmachine::Result<()> {
//! use libmachine::LinuxBoot;
//! use libmachine::agent::Command;
//! use libmachine::sandbox::Sandbox;
//!
//! let boot = LinuxBoot {
//!     kernel: "vmlinux".into(),
//!     initrd: Some("initramfs.cpio".into()),
//!     cmdline: String::new(),
//! };
//! let sandbox = Sandbox::builder(boot).create().await?;
//! let out = sandbox.exec(Command::new("uname").arg("-r")).await?;
//! sandbox.write_file("/tmp/x", b"data".to_vec(), 0o644).await?;
//! sandbox.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! # How it runs
//!
//! Underneath is the synchronous [`Machine`] and the blocking
//! [`Agent`] client; the Sandbox puts their waits on threads, so that an
//! async caller never blocks:
//!
//! - **One run thread per sandbox** owns its machine and runs it (the vCPUs
//!   have threads of their own, as for every machine) until the guest stops
//!   or the sandbox is closed, then frees it. A run's end must be waited on
//!   for what follows it (failing every call waiting on a stopped guest), so
//!   every machine needs a waiter; this one is not on tokio's blocking pool,
//!   which hundreds of sandboxes would otherwise fill.
//! - **Short calls** (starting a command, reading or writing a file, feeding
//!   stdin, sending a signal) run on tokio's blocking pool, a pool thread
//!   each while it waits for the guest.
//! - **A running command** has a thread that takes its output as it comes
//!   into a small queue (backpressure reaches the command), so that reading
//!   its next event is a cancel-safe await.
//!
//! So a sandbox costs one thread beyond its machine's vCPU and device
//! threads, plus one per command running and per call in flight. A
//! suspended one costs its run thread and a watcher, and no machine
//! threads.
//!
//! # Ending
//!
//! [`close`](Sandbox::close) stops the guest and waits until the machine is
//! freed; dropping the Sandbox stops it without waiting. A guest that stops
//! by itself (it powers off, resets, triple-faults) frees its machine too.
//! Either way, every call waiting on the guest, and every call after,
//! fails with [`Error::GuestGone`], and [`wait`](Sandbox::wait) says how it
//! ended.
//!
//! Dropping a future cancels what it waits for where that can be undone: a
//! dropped [`exec`](Sandbox::exec) or [`SandboxProcess`] kills its command
//! if it still runs. A short call already sent to the guest (a file
//! written, stdin fed) completes in the background.
//!
//! # From a snapshot
//!
//! Booting is the slow part of creating a sandbox. A template sandbox,
//! booted once and prepared, is [snapshotted](Sandbox::snapshot) while it
//! keeps running; [`Sandbox::from_snapshot`] then creates sandboxes from the
//! file in milliseconds, as many as wanted, each its own guest: its RAM a
//! copy-on-write mapping of the file, its RNG reseeded and its clock
//! moved on before any command runs ([`Machine::restore`]).
//!
//! ```no_run
//! # async fn demo(boot: libmachine::LinuxBoot) -> libmachine::Result<()> {
//! use std::sync::Arc;
//! use libmachine::agent::Command;
//! use libmachine::sandbox::Sandbox;
//!
//! let template = Sandbox::builder(boot).create().await?;
//! template.write_file("/etc/motd", b"prepared".to_vec(), 0o644).await?;
//! let snapshot = Arc::new(template.snapshot("template.snap").await?);
//! template.close().await;
//!
//! let clone = Sandbox::from_snapshot(snapshot.clone()).create().await?;
//! let out = clone.exec(Command::new("cat").arg("/etc/motd")).await?;
//! # Ok(())
//! # }
//! ```

//!
//! # Suspended while it waits
//!
//! A guest waiting for something from outside (an answer from a slow
//! server, the next call) needs no CPU and little memory meanwhile.
//! [`suspend`](Sandbox::suspend) writes its whole machine to a file (its
//! vCPUs, its devices, every network connection's TCP state, its RAM) and
//! frees the machine: no VM, no vCPU, device or network thread, no guest
//! RAM. Only what cannot go in a file stays in this process: the host's end
//! of each network connection, its vsock's host end, its network's policy
//! and log, its console, and a thread that watches them. It wakes by
//! itself, where it was, as soon as something comes for it: the server's
//! answer on the same connection, any call on the sandbox.
//!
//! ```no_run
//! # async fn demo(sandbox: libmachine::sandbox::Sandbox) -> libmachine::Result<()> {
//! use libmachine::agent::Command;
//!
//! // The guest asks a slow LLM; nothing to do until it answers.
//! let call = sandbox.spawn(Command::new("python3").arg("ask.py")).await?;
//! sandbox.suspend("/var/tmp").await?;
//! let answer = call.wait().await?; // the answer woke the guest
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::Vsock;
use crate::agent::{Agent, Command, Event, Output, Process};
use crate::error::{Error, Result};
use crate::machine::{ExitReason, LinuxBoot, Machine, MachineConfig};
use crate::net::{Network, NetworkConfig};
use crate::snapshot::Snapshot;
use crate::stop::StopHandle;
use crate::suspend::Woken;

/// How long [`SandboxBuilder::create`] waits for the agent, by default.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Guest RAM of a sandbox unless [`SandboxBuilder::mem_size`] says
/// otherwise.
pub const DEFAULT_MEM_SIZE: usize = 256 << 20;
/// How much of the guest's console a Sandbox keeps
/// ([`Sandbox::console_output`]): the end of it.
pub const CONSOLE_KEPT: usize = 64 << 10;
/// The sandbox's own threads (run and output threads) only wait; their
/// stacks need little.
const THREAD_STACK: usize = 256 << 10;
/// Events of a command queued for its reader, each at most
/// [`libmachine_agent::CHUNK`] bytes: past that, its output waits in the
/// agent, and then the command.
const EVENTS_QUEUED: usize = 8;
/// How long [`Sandbox::suspend`] waits for the guest to be idle.
pub const SUSPEND_IDLE_WAIT: Duration = Duration::from_secs(2);
/// How long the guest runs between looks while a suspension waits for it
/// to be idle.
const IDLE_POLL: Duration = Duration::from_millis(2);
/// How much of the console an error about a failed boot quotes.
const CONSOLE_QUOTED: usize = 2 << 10;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `f` on tokio's blocking pool and waits for it without blocking.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(Error::Agent("the tokio runtime is shutting down".into())),
    }
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(THREAD_STACK)
        .spawn(f)
        .map(drop)
        .map_err(|source| Error::Hypervisor { op: "start a sandbox thread", source })
}

/// What a sandbox starts from.
#[derive(Debug, Clone)]
enum Source {
    /// A Linux kernel booted from scratch.
    Boot(LinuxBoot),
    /// A machine restored from a snapshot.
    Snapshot(Arc<Snapshot>),
}

/// Configures a [`Sandbox`]; from [`Sandbox::builder`].
///
/// The guest must run libmachine's agent (`libmachine-agent`) on its
/// default vsock port: the sandbox is ready once the agent answers.
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until create() is awaited"]
pub struct SandboxBuilder {
    config: MachineConfig,
    source: Source,
    ready_timeout: Duration,
    agent_timeout: Option<Duration>,
}

impl SandboxBuilder {
    /// The whole machine configuration (by default [`MachineConfig`]'s,
    /// with [`DEFAULT_MEM_SIZE`] of RAM).
    pub fn config(mut self, config: MachineConfig) -> Self {
        self.config = config;
        self
    }

    /// Guest RAM in bytes. (A sandbox from a snapshot has the snapshot's;
    /// another size is refused.)
    pub fn mem_size(mut self, bytes: usize) -> Self {
        self.config.mem_size = bytes;
        self
    }

    /// How many vCPUs.
    pub fn vcpus(mut self, vcpus: u32) -> Self {
        self.config.vcpus = vcpus;
        self
    }

    /// Gives the guest a network, its connections filtered by the
    /// configuration's policy (none by default). Not for a sandbox from a
    /// snapshot: networked machines are never snapshotted.
    pub fn network(mut self, network: NetworkConfig) -> Self {
        self.config.network = Some(network);
        self
    }

    /// How long [`create`](Self::create) waits for the agent to answer
    /// ([`DEFAULT_READY_TIMEOUT`] by default).
    pub fn ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    /// The longest any one wait for the guest may take in a call
    /// ([`Agent::timeout`]): past it the call fails with
    /// [`Error::TimedOut`]. None, the default, waits as long as the guest
    /// runs.
    pub fn agent_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.agent_timeout = timeout;
        self
    }

    /// Creates the machine (boots it, or restores it from its snapshot),
    /// starts it, and waits until the agent answers: for a restored one,
    /// once it has reseeded the guest's RNG.
    ///
    /// A guest that stops while booting fails at once with
    /// [`Error::GuestGone`], quoting the end of its console. Dropping the
    /// future stops and frees the machine.
    pub async fn create(self) -> Result<Sandbox> {
        let SandboxBuilder { config, source, ready_timeout, agent_timeout } = self;
        let machine = blocking(move || match source {
            Source::Boot(boot) => Machine::boot_linux(config, &boot),
            Source::Snapshot(snapshot) => Machine::restore(&snapshot, config),
        })
        .await?;
        let sandbox = Sandbox::start(machine, agent_timeout)?;
        sandbox.wait_ready(ready_timeout).await?;
        Ok(sandbox)
    }
}

/// How a sandbox's machine ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ended {
    /// The sandbox was closed or dropped.
    Closed,
    /// The guest stopped by itself: [`ExitReason::Poweroff`],
    /// [`ExitReason::Reset`], [`ExitReason::Shutdown`], or another exit
    /// that ends a run.
    Guest(ExitReason),
    /// Running the machine failed (the hypervisor's error).
    Failed(String),
}

/// The end of the guest's console, kept as it is written.
#[derive(Debug, Default)]
struct Console(Mutex<VecDeque<u8>>);

impl Console {
    fn tail(&self, max: usize) -> Vec<u8> {
        let kept = lock(&self.0);
        kept.range(kept.len().saturating_sub(max)..).copied().collect()
    }
}

struct ConsoleWriter(Arc<Console>);

impl Write for ConsoleWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut kept = lock(&self.0.0);
        kept.extend(buf);
        let over = kept.len().saturating_sub(CONSOLE_KEPT);
        kept.drain(..over);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A Linux guest running libmachine's agent, used from async code: run
/// commands (to their end, or streaming), copy files in and out (see the
/// [module docs](self)).
///
/// Methods take `&self`, so tasks can share a sandbox (e.g. in an `Arc`)
/// and call it concurrently. Dropping it stops the guest and frees its
/// machine.
pub struct Sandbox {
    agent: Agent,
    vsock: Vsock,
    network: Option<Network>,
    stop: StopHandle,
    /// Set before the stop that closes the sandbox, so that the run thread
    /// tells a close from a guest's own stop.
    closing: Arc<AtomicBool>,
    /// What the run thread is asked to do between runs (snapshots,
    /// suspensions); None once the machine is gone.
    requests: Requests,
    /// Whether the guest is suspended, as the run thread last left it.
    suspended: Arc<AtomicBool>,
    /// How the machine ended, once it has and is freed.
    ended: watch::Receiver<Option<Ended>>,
    console: Arc<Console>,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox").field("ended", &*self.ended.borrow()).finish_non_exhaustive()
    }
}

impl Sandbox {
    /// A sandbox booting `boot`'s kernel; configure it, then
    /// [`create`](SandboxBuilder::create) it.
    pub fn builder(boot: LinuxBoot) -> SandboxBuilder {
        SandboxBuilder {
            config: MachineConfig { mem_size: DEFAULT_MEM_SIZE, ..MachineConfig::default() },
            source: Source::Boot(boot),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            agent_timeout: None,
        }
    }

    /// A sandbox restored from `snapshot` (of a sandbox, from
    /// [`snapshot`](Sandbox::snapshot), or of any PC machine running the
    /// agent, from [`Machine::snapshot`]): its machine is the snapshot's
    /// ([`Snapshot::config`]); configure the rest, then
    /// [`create`](SandboxBuilder::create) it, as many times as wanted.
    ///
    /// Each is its own guest, independent of the others and of the
    /// template: its RAM is a copy-on-write mapping of the file, its RNG is
    /// reseeded (VMGENID, and the agent's reseed with host entropy before
    /// [`create`](SandboxBuilder::create) returns) and its clock moved on to
    /// the present ([`Machine::restore`]). Commands that ran in the
    /// template when it was snapshotted are not in any clone's reach.
    pub fn from_snapshot(snapshot: Arc<Snapshot>) -> SandboxBuilder {
        SandboxBuilder {
            config: snapshot.config(),
            source: Source::Snapshot(snapshot),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            agent_timeout: None,
        }
    }

    /// Takes `machine` (a PC machine, not yet run) and runs it on a thread
    /// of its own.
    fn start(mut machine: Machine, agent_timeout: Option<Duration>) -> Result<Self> {
        let console = Arc::new(Console::default());
        machine.set_console(ConsoleWriter(console.clone()));
        let vsock = machine.vsock().expect("a PC machine has a vsock");
        let agent = Agent::new(vsock.clone()).timeout(agent_timeout);
        let network = machine.network();
        let stop = machine.stop_handle();
        let closing = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Some(Vec::new())));
        let suspended = Arc::new(AtomicBool::new(false));
        let (ended_tx, ended) = watch::channel(None);
        let control = RunControl {
            closing: closing.clone(),
            requests: requests.clone(),
            suspended: suspended.clone(),
        };
        spawn_thread("libmachine-run", move || run(machine, &control, &ended_tx))?;
        Ok(Self { agent, vsock, network, stop, closing, requests, suspended, ended, console })
    }

    /// Waits until the agent answers, at most `timeout`.
    async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let agent = self.agent.clone();
        let ready = blocking(move || agent.wait_ready(timeout)).await;
        let console = || String::from_utf8_lossy(&self.console.tail(CONSOLE_QUOTED)).into_owned();
        match ready {
            Ok(()) => Ok(()),
            // The machine has stopped or is gone: its run thread is about
            // to say how.
            Err(Error::GuestGone(e)) => {
                let ended = self.wait().await;
                Err(Error::GuestGone(format!(
                    "the guest stopped while booting ({ended:?}): {e}; its console ended with:\n{}",
                    console()
                )))
            }
            Err(Error::TimedOut(e)) => {
                Err(Error::TimedOut(format!("{e}; the guest's console ended with:\n{}", console())))
            }
            Err(e) => Err(e),
        }
    }

    /// The blocking agent client of this sandbox's guest, for what the
    /// async methods do not offer. Its calls block the calling thread.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// The host's end of the guest's vsock (blocking), e.g. to reach a
    /// service of the caller's own in the guest.
    pub fn vsock(&self) -> &Vsock {
        &self.vsock
    }

    /// The guest's network, if it has one: its policy can change at any
    /// time.
    pub fn network(&self) -> Option<&Network> {
        self.network.as_ref()
    }

    /// The end of what the guest wrote to its console (at most
    /// [`CONSOLE_KEPT`] bytes).
    pub fn console_output(&self) -> Vec<u8> {
        self.console.tail(CONSOLE_KEPT)
    }

    /// How the machine ended, if it has (and is freed).
    pub fn ended(&self) -> Option<Ended> {
        self.ended.borrow().clone()
    }

    /// Waits until the machine ends (the guest stops by itself, or the
    /// sandbox is closed) and is freed; says how.
    pub async fn wait(&self) -> Ended {
        let mut ended = self.ended.clone();
        let outcome = match ended.wait_for(Option::is_some).await {
            Ok(ended) => ended.clone(),
            Err(_) => None,
        };
        // The run thread drops its sender only after saying how it ended,
        // unless it panicked.
        outcome.unwrap_or_else(|| Ended::Failed("the sandbox's run thread panicked".into()))
    }

    /// Writes a snapshot of the sandbox's machine to `path` (replacing a
    /// file there) and opens it, for [`Sandbox::from_snapshot`]: the guest
    /// is paused for it (calls waiting on it wait a little longer) and runs
    /// on after. This is how a template is made: create a sandbox, prepare
    /// it (files, warm caches), snapshot it, then keep or close it.
    ///
    /// Snapshot an idle guest: a command running in it now runs on in
    /// every clone, out of any client's reach, and its connection is reset.
    /// A sandbox with a network cannot be snapshotted ([`Machine::snapshot`]);
    /// a sandbox closed meanwhile, or whose guest stopped, fails with
    /// [`Error::GuestGone`].
    pub async fn snapshot(&self, path: impl AsRef<Path>) -> Result<Snapshot> {
        let path = path.as_ref().to_owned();
        self.ask(|reply| Request::Snapshot { path, reply })
            .await
            .unwrap_or_else(|| Err(self.gone("no snapshot")))
    }

    /// Suspends the guest while it waits for something from outside, such
    /// as a slow server's answer to a request it made: its whole machine
    /// goes to a file in `dir` (vCPUs, devices, every connection's TCP
    /// state, RAM) and is freed, and its connections stay open, the host's
    /// end of each held here ([`Machine::suspend`]). It wakes by itself,
    /// restored from the file, and carries on where it was, as soon as
    /// something comes for it: data or a close on any of its connections, a
    /// call to its agent (any call on this sandbox), or
    /// [`resume`](Self::resume). Returns once the guest is suspended; it
    /// may have woken again already. The sandbox's handles ([`agent`],
    /// [`vsock`], [`network`], the console) carry on across suspensions.
    ///
    /// [`agent`]: Self::agent
    /// [`vsock`]: Self::vsock
    /// [`network`]: Self::network
    ///
    /// The guest must be idle, as it is while it waits (with no DNS lookup
    /// or decider question in flight on its network): this waits up to
    /// [`SUSPEND_IDLE_WAIT`] for it to be, then fails with [`Error::Busy`].
    /// Its clocks run on while it is suspended, so it sees the time it
    /// waited (a read timeout still expires), and its own timers fire late,
    /// when it wakes. `dir` must be on a disk (not tmpfs), with room for
    /// the RAM the guest uses; the file has no name and goes by itself.
    pub async fn suspend(&self, dir: impl AsRef<Path>) -> Result<()> {
        let dir = dir.as_ref().to_owned();
        let until = std::time::Instant::now() + SUSPEND_IDLE_WAIT;
        self.ask(|reply| Request::Suspend { dir, until, reply })
            .await
            .unwrap_or_else(|| Err(self.gone("no suspension")))
    }

    /// Wakes a suspended guest; returns once it runs again (at once if it
    /// was not suspended).
    pub async fn resume(&self) -> Result<()> {
        self.ask(|reply| Request::Resume { reply }).await.ok_or_else(|| self.gone("not resumed"))
    }

    /// Whether the guest is suspended now.
    pub fn suspended(&self) -> bool {
        self.suspended.load(Ordering::SeqCst)
    }

    /// Hands the run thread a request (made with its reply channel) and
    /// stops the guest, so that the thread takes it at once; the answer,
    /// or None if the machine ended first.
    ///
    /// The reply's sender only ever lives in the queue, which the run
    /// thread empties or drops: no request can wait on a machine that is
    /// gone. (A sender kept here when the queue was gone left the call
    /// waiting for good: found by the snapshot test.)
    async fn ask<T>(&self, request: impl FnOnce(oneshot::Sender<T>) -> Request) -> Option<T> {
        let answer = lock(&self.requests).as_mut().map(|requests| {
            let (reply, answer) = oneshot::channel();
            requests.push(request(reply));
            answer
        })?;
        // The run thread takes every request at its next stop (a stop
        // asked for between runs is kept for the next).
        self.stop.stop();
        answer.await.ok()
    }

    fn gone(&self, what: &str) -> Error {
        Error::GuestGone(format!("{what}: the sandbox's guest ended ({:?})", self.ended()))
    }

    /// Stops the guest, if it still runs, and waits until its machine is
    /// freed: how it ended ([`Ended::Closed`] unless it had already ended
    /// another way). Every call waiting on the guest fails with
    /// [`Error::GuestGone`]. Closing again returns the same.
    pub async fn close(&self) -> Ended {
        self.closing.store(true, Ordering::SeqCst);
        self.stop.stop();
        self.wait().await
    }

    /// Runs `cmd` to its end: its output and how it ended. Its stdin is
    /// what [`Command::stdin`] gave, else /dev/null.
    ///
    /// Dropping the future kills the command if it still runs.
    pub async fn exec(&self, cmd: &Command) -> Result<Output> {
        let stdin = cmd.stdin_data().map(<[u8]>::to_vec);
        let process = self.start_command(cmd.clone(), stdin.is_some()).await?;
        if let Some(data) = stdin {
            let p = process.process.clone();
            // In the background: the command may end without reading it
            // all, and its output must be taken meanwhile.
            drop(tokio::task::spawn_blocking(move || {
                if p.write_stdin(&data).is_ok() {
                    let _ = p.close_stdin();
                }
            }));
        }
        process.wait().await
    }

    /// Starts `cmd`, its stdin a pipe fed through the [`SandboxProcess`].
    pub async fn spawn(&self, cmd: &Command) -> Result<SandboxProcess> {
        self.start_command(cmd.clone(), true).await
    }

    async fn start_command(&self, cmd: Command, stdin: bool) -> Result<SandboxProcess> {
        let agent = self.agent.clone();
        // A process the caller no longer waits for (the future was
        // dropped) is dropped with the task's output, which releases it.
        let process = blocking(move || agent.start(&cmd, stdin)).await?;
        SandboxProcess::new(process)
    }

    /// The contents of the guest's file `path`.
    pub async fn read_file(&self, path: impl AsRef<OsStr>) -> Result<Vec<u8>> {
        let agent = self.agent.clone();
        let path = path.as_ref().to_owned();
        blocking(move || agent.read_file(&path)).await
    }

    /// Makes the guest's file `path` hold `data`, with permissions `mode`:
    /// whole, or not at all.
    pub async fn write_file(
        &self,
        path: impl AsRef<OsStr>,
        data: impl Into<Vec<u8>>,
        mode: u32,
    ) -> Result<()> {
        let agent = self.agent.clone();
        let path = path.as_ref().to_owned();
        let data = data.into();
        blocking(move || agent.write_file(&path, &data, mode)).await
    }

    /// Copies the host's file `from` to the guest's `to`, with its
    /// permissions.
    pub async fn copy_to_guest(&self, from: impl AsRef<Path>, to: impl AsRef<OsStr>) -> Result<()> {
        let agent = self.agent.clone();
        let (from, to) = (from.as_ref().to_owned(), to.as_ref().to_owned());
        blocking(move || agent.copy_to_guest(&from, &to)).await
    }

    /// Copies the guest's file `from` to the host's `to`.
    pub async fn copy_from_guest(
        &self,
        from: impl AsRef<OsStr>,
        to: impl AsRef<Path>,
    ) -> Result<()> {
        let agent = self.agent.clone();
        let (from, to) = (from.as_ref().to_owned(), to.as_ref().to_owned());
        blocking(move || agent.copy_from_guest(&from, &to)).await
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        self.stop.stop();
    }
}

/// What a sandbox asks of its run thread, between runs.
enum Request {
    /// [`Sandbox::snapshot`].
    Snapshot { path: PathBuf, reply: oneshot::Sender<Result<Snapshot>> },
    /// [`Sandbox::suspend`]: as soon as the guest is idle, unless `until`
    /// passes first.
    Suspend { dir: PathBuf, until: std::time::Instant, reply: oneshot::Sender<Result<()>> },
    /// [`Sandbox::resume`].
    Resume { reply: oneshot::Sender<()> },
}

/// The requests to a sandbox's run thread; None once its machine is gone.
type Requests = Arc<Mutex<Option<Vec<Request>>>>;

/// How the run thread takes its requests, and what it tells the Sandbox.
struct RunControl {
    closing: Arc<AtomicBool>,
    requests: Requests,
    suspended: Arc<AtomicBool>,
}

/// A suspension waiting for the guest to be idle.
struct Suspension {
    dir: PathBuf,
    until: std::time::Instant,
    reply: oneshot::Sender<Result<()>>,
}

/// The run thread: runs the machine, doing what is asked of it between
/// runs, until its run ends otherwise; frees it, then says how it ended.
fn run(mut machine: Machine, control: &RunControl, ended: &watch::Sender<Option<Ended>>) {
    // Suspensions asked for, waiting for an idle guest: while there are
    // any, the guest runs in short slices, looked at between them.
    let mut waiting = Vec::new();
    let how = loop {
        let outcome = if waiting.is_empty() { machine.run() } else { machine.run_for(IDLE_POLL) };
        match outcome {
            // Only the Sandbox has the stop handle, and it sets `closing`
            // before the stop that closes it. Any other stop is a request's:
            // take those asked for, and run on. (One may find none: a stop
            // asked for after the requests it came with were taken.)
            Ok(ExitReason::Stopped | ExitReason::TimedOut)
                if control.closing.load(Ordering::SeqCst) =>
            {
                break Ended::Closed;
            }
            Ok(ExitReason::Stopped | ExitReason::TimedOut) => {
                if let Some(how) = between_runs(&mut machine, control, &mut waiting) {
                    break how;
                }
            }
            Ok(reason) => break Ended::Guest(reason),
            Err(e) => break Ended::Failed(e.to_string()),
        }
    };
    // What is still asked for fails (its senders dropped), and so do calls
    // still waiting on the guest, if the run's end did not already fail
    // them.
    lock(&control.requests).take();
    drop(waiting);
    control.suspended.store(false, Ordering::SeqCst);
    drop(machine);
    ended.send_replace(Some(how));
}

/// Between runs: does what is asked, suspends the guest if asked to and it
/// is idle, and while it is suspended waits until something wakes it.
/// Returns when the guest is to run again, or how the sandbox ended if it
/// was closed meanwhile.
fn between_runs(
    machine: &mut Machine,
    control: &RunControl,
    waiting: &mut Vec<Suspension>,
) -> Option<Ended> {
    loop {
        let requests = lock(&control.requests).as_mut().map(std::mem::take).unwrap_or_default();
        let mut resumes = Vec::new();
        for request in requests {
            match request {
                Request::Snapshot { path, reply } => {
                    let taken = machine.snapshot(&path).and_then(|()| Snapshot::open(&path));
                    let _ = reply.send(taken);
                }
                Request::Suspend { dir, until, reply } => {
                    waiting.push(Suspension { dir, until, reply });
                }
                Request::Resume { reply } => resumes.push(reply),
            }
        }
        let resume = !resumes.is_empty();
        if resume {
            for s in waiting.drain(..) {
                let _ = s.reply.send(Err(Error::Busy("resumed before the guest was idle".into())));
            }
        } else if !waiting.is_empty() {
            suspend(machine, waiting, &control.suspended);
        }
        if resume || !machine.suspended() {
            control.suspended.store(false, Ordering::SeqCst);
            // Answered once `suspended` says so.
            for reply in resumes {
                let _ = reply.send(());
            }
            return None;
        }
        control.suspended.store(true, Ordering::SeqCst);
        match machine.wait_woken(None) {
            Woken::Device | Woken::TimedOut => {
                control.suspended.store(false, Ordering::SeqCst);
                return None;
            }
            // A request, or the close.
            Woken::Stop if control.closing.load(Ordering::SeqCst) => return Some(Ended::Closed),
            Woken::Stop => {}
        }
    }
}

/// Suspends the guest if it is idle, answering every suspension waiting
/// (once `suspended` says so): those whose time is up fail if it is not.
fn suspend(machine: &mut Machine, waiting: &mut Vec<Suspension>, suspended: &AtomicBool) {
    let done = if machine.suspended() { Ok(()) } else { machine.suspend(&waiting[0].dir) };
    match done {
        Ok(()) => {
            suspended.store(true, Ordering::SeqCst);
            for s in waiting.drain(..) {
                let _ = s.reply.send(Ok(()));
            }
        }
        Err(Error::Busy(why)) => {
            let now = std::time::Instant::now();
            for s in std::mem::take(waiting) {
                if now < s.until {
                    waiting.push(s);
                } else {
                    let _ = s.reply.send(Err(Error::Busy(format!(
                        "the guest was not idle within {SUSPEND_IDLE_WAIT:?}: {why}"
                    ))));
                }
            }
        }
        // About this suspension's directory: the next may do better.
        Err(e) => {
            let _ = waiting.remove(0).reply.send(Err(e));
        }
    }
}

/// A command running in a sandbox's guest, from [`Sandbox::spawn`].
///
/// Methods take `&self`: one task can feed its stdin while another reads
/// its output. Its output is taken as it comes, a few events ahead of the
/// reader; past that the command waits. Dropping it kills the command if
/// it still runs.
pub struct SandboxProcess {
    process: Arc<Process>,
    events: tokio::sync::Mutex<mpsc::Receiver<Result<Event>>>,
    /// The next event, taken off `events` by [`event_ready`] and not yet
    /// handed out by [`take_event`]: where an event waits when the call
    /// that received it was abandoned after receiving (e.g. a cancelled
    /// Python awaitable whose task still finished).
    ///
    /// [`event_ready`]: SandboxProcess::event_ready
    /// [`take_event`]: SandboxProcess::take_event
    stash: std::sync::Mutex<Option<Result<Option<Event>>>>,
    /// Its output thread still reads: the command has not been seen to end.
    pumping: Arc<AtomicBool>,
}

impl std::fmt::Debug for SandboxProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxProcess").field("pid", &self.process.pid()).finish_non_exhaustive()
    }
}

impl SandboxProcess {
    fn new(process: Process) -> Result<Self> {
        let process = Arc::new(process);
        let (tx, rx) = mpsc::channel(EVENTS_QUEUED);
        let pumping = Arc::new(AtomicBool::new(true));
        let (p, flag) = (process.clone(), pumping.clone());
        spawn_thread("libmachine-out", move || pump(p, &tx, &flag))?;
        Ok(Self {
            process,
            events: tokio::sync::Mutex::new(rx),
            stash: std::sync::Mutex::new(None),
            pumping,
        })
    }

    /// Its process ID in the guest (also its process group's).
    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    /// What it does next; `None` once its exit has been returned.
    ///
    /// Cancel-safe: a dropped call loses no event.
    pub async fn next_event(&self) -> Result<Option<Event>> {
        loop {
            self.event_ready().await;
            if let Some(event) = self.take_event() {
                return event;
            }
            // Another caller took it between the two steps: wait again.
        }
    }

    /// Waits until the next event is ready to [`take`](Self::take_event),
    /// without handing it out. Two steps, so that an event is never lost
    /// between being received and reaching its caller, even when the call
    /// that received it is abandoned afterwards (across a language
    /// boundary, cancelling an awaitable does not stop the Rust task that
    /// serves it): it stays here for the next call.
    pub async fn event_ready(&self) {
        if lock(&self.stash).is_some() {
            return;
        }
        let mut events = self.events.lock().await;
        if lock(&self.stash).is_some() {
            return;
        }
        let event = match events.recv().await {
            Some(event) => event.map(Some),
            // The output thread has ended: after the exit, or a failure
            // the process keeps answering with (so a result lost here, if
            // this future is dropped mid-call, is answered again).
            None => {
                drop(events);
                let p = self.process.clone();
                blocking(move || p.next_event()).await
            }
        };
        *lock(&self.stash) = Some(event);
    }

    /// The event [`event_ready`](Self::event_ready) found, if it is still
    /// there.
    pub fn take_event(&self) -> Option<Result<Option<Event>>> {
        lock(&self.stash).take()
    }

    /// Waits for it to end, collecting the output not taken yet.
    ///
    /// Dropping the future loses the output it collected, not the command.
    pub async fn wait(&self) -> Result<Output> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            match self.next_event().await? {
                Some(Event::Stdout(d)) => stdout.extend(d),
                Some(Event::Stderr(d)) => stderr.extend(d),
                Some(Event::Exit(status)) => {
                    return Ok(Output { status, stdout, stderr });
                }
                None => {
                    return Err(Error::Agent(format!(
                        "{:?}: its exit was already returned",
                        self.process
                    )));
                }
            }
        }
    }

    /// Feeds `data` to its stdin; waits while the guest has no room for it.
    /// If the future is dropped, the data may still be written.
    pub async fn write_stdin(&self, data: impl Into<Vec<u8>>) -> Result<()> {
        let p = self.process.clone();
        let data = data.into();
        blocking(move || p.write_stdin(&data)).await
    }

    /// Closes its stdin (once what was written has reached it).
    pub async fn close_stdin(&self) -> Result<()> {
        let p = self.process.clone();
        blocking(move || p.close_stdin()).await
    }

    /// Sends `signal` to its process group.
    pub async fn signal(&self, signal: i32) -> Result<()> {
        let p = self.process.clone();
        blocking(move || p.signal(signal)).await
    }

    /// Kills its process group (SIGKILL).
    pub async fn kill(&self) -> Result<()> {
        self.signal(9).await
    }
}

impl Drop for SandboxProcess {
    fn drop(&mut self) {
        if self.pumping.load(Ordering::SeqCst) {
            // Its output thread waits for the command: killing it ends the
            // wait (the thread then drops the process, which releases it).
            // Off this thread: the kill waits for the guest.
            let p = self.process.clone();
            let _ = spawn_thread("libmachine-kill", move || {
                let _ = p.kill();
            });
        }
    }
}

/// A process's output thread: takes its events into `tx` until its exit, a
/// failure, or the reader is gone.
///
/// It lets go of the process before the reader can see its last event, so
/// that a call that has seen it (an exec returning) leaves nothing running
/// behind it: the reader's handle is the last, and the connection closes
/// when it drops. (Released here after the exit was sent, the connection
/// closed after exec returned, and its last packets woke a guest suspended
/// just after: board 13203.)
fn pump(process: Arc<Process>, tx: &mpsc::Sender<Result<Event>>, pumping: &AtomicBool) {
    loop {
        let event = process.next_event();
        let last = !matches!(event, Ok(Some(Event::Stdout(_) | Event::Stderr(_))));
        if last {
            // Before the reader can see it: a process dropped after its exit
            // has nothing to kill.
            pumping.store(false, Ordering::SeqCst);
            drop(process);
            if let Ok(Some(event)) = event {
                let _ = tx.blocking_send(Ok(event));
            } else if let Err(e) = event {
                let _ = tx.blocking_send(Err(e));
            }
            return;
        }
        let event = match event {
            Ok(Some(event)) => event,
            _ => unreachable!("not the last event"),
        };
        if tx.blocking_send(Ok(event)).is_err() {
            break;
        }
    }
    pumping.store(false, Ordering::SeqCst);
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Sandbox>();
    assert_send_sync::<SandboxProcess>();
};
