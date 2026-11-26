// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine's helper process (board 13460): the process its VM runs in, one per machine, which
//! machined starts (`machined-vm`, beside it) and manages over the socket that is the helper's
//! standard input (`wire`). Hypervisor.framework allows one VM per process, and a VM's process
//! is what its failures, its memory and its limits are scoped to, as Firecracker's is; machined,
//! the long-lived daemon with the API, keeps no VM of its own, and outlives any helper.
//!
//! The helper makes its machine from the spec it is given (checked and planned again, as
//! machined did), serves the machine's vsock socket itself (`vsock`, Firecracker's protocol,
//! which machined's agent calls go through too), sends its console's bytes, and runs it on a
//! thread of its own that does between runs what machined asks: snapshots, suspensions. Its end,
//! or the socket's (machined gone), ends the helper.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(target_os = "linux")]
use std::time::Duration;

use libmachine::{ExitReason, Machine, StopHandle, Woken};
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot, watch};
use tonic::Status;

use crate::{checks, errors, vsock};

pub(crate) mod wire;

use wire::{Answer, Ended, Event, How, Request, event, request};

/// Events queued for machined: past that, the console's writer waits.
const QUEUED: usize = 256;
/// The run thread only waits: its stack needs little.
const THREAD_STACK: usize = 256 << 10;
/// How long a suspension waits for the guest to be idle.
#[cfg(target_os = "linux")]
const SUSPEND_IDLE_WAIT: Duration = Duration::from_secs(2);
/// How long the guest runs between looks while a suspension waits for it to be idle.
#[cfg(target_os = "linux")]
const IDLE_POLL: Duration = Duration::from_millis(2);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the run thread is asked to do between runs, each with its answer's sender.
enum Asked {
    #[cfg(target_os = "linux")]
    Snapshot(PathBuf, oneshot::Sender<Result<(), Status>>),
    #[cfg(target_os = "linux")]
    Suspend(PathBuf, oneshot::Sender<Result<(), Status>>),
    #[cfg(target_os = "linux")]
    Resume(oneshot::Sender<Result<(), Status>>),
}

/// What the run thread and the helper's tasks share.
struct Shared {
    /// Set before the stop that ends the machine, so that the run thread tells it from a request's.
    closing: AtomicBool,
    /// What the run thread is asked to do between runs; None once the machine is gone.
    asked: Mutex<Option<Vec<Asked>>>,
    /// Whether the guest is suspended, as the run thread last left it.
    suspended: watch::Sender<bool>,
}

/// The helper's main: serves machined on its standard input until its machine ends. A failure
/// to speak to machined at all is the process's error; the machine's are told to machined.
pub fn main() -> io::Result<()> {
    struct Stderr;
    impl log::Log for Stderr {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!(
                    "{} machined-vm[{}] {}",
                    record.level(),
                    std::process::id(),
                    record.args()
                );
            }
        }
        fn flush(&self) {}
    }
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|l| l.parse().ok())
        .unwrap_or(log::LevelFilter::Info);
    log::set_logger(&Stderr).map_err(io::Error::other)?;
    log::set_max_level(level);
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve())
}

/// Sends `kind` to machined; false once machined is gone.
async fn tell(events: &mpsc::Sender<Event>, kind: event::Kind) -> bool {
    events.send(Event { kind: Some(kind) }).await.is_ok()
}

fn ended(how: How, detail: impl Into<String>) -> event::Kind {
    event::Kind::Ended(Ended { how: how.into(), detail: detail.into(), code: 0 })
}

/// A machine that never ran, as `status` says.
fn boot_failed(status: &Status) -> event::Kind {
    let code = status.code().into();
    event::Kind::Ended(Ended { how: How::BootFailed.into(), detail: status.message().into(), code })
}

/// The socket to machined, which is standard input.
fn control() -> io::Result<tokio::net::UnixStream> {
    use std::os::fd::FromRawFd as _;
    // SAFETY: machined starts the helper with a socket as its standard input, which nothing else
    // of this process takes; it is owned from here on.
    let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(0) };
    socket.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(socket)
}

async fn serve() -> io::Result<()> {
    let (mut from, to) = control()?.into_split();
    let (events, queued) = mpsc::channel(QUEUED);
    let writer = tokio::spawn(write_events(to, queued));
    let Some(start) = wire::read::<_, wire::Start>(&mut from).await? else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a helper is started first"));
    };
    if start.protocol != wire::PROTOCOL {
        let why = format!(
            "machined speaks helper protocol {}, this helper {}",
            start.protocol,
            wire::PROTOCOL
        );
        tell(&events, boot_failed(&Status::failed_precondition(why))).await;
    } else {
        run(start, &mut from, events.clone()).await;
    }
    drop(events);
    writer.await.map_err(io::Error::other)?
}

/// Sends what is queued to machined, until the queue closes.
async fn write_events(mut to: OwnedWriteHalf, mut queued: mpsc::Receiver<Event>) -> io::Result<()> {
    while let Some(event) = queued.recv().await {
        wire::write(&mut to, &event).await?;
    }
    Ok(())
}

/// The machine's console: its bytes, sent to machined. It waits while machined's queue is full,
/// rather than drop what the guest wrote.
struct Console(mpsc::Sender<Event>);

impl Write for Console {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let event = Event { kind: Some(event::Kind::Console(buf.to_vec())) };
        // machined gone: the console has no reader, and the machine is stopping.
        let _ = self.0.blocking_send(event);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Makes the machine as `start` says and runs it to its end, doing what machined asks.
async fn run(
    start: wire::Start,
    from: &mut tokio::net::unix::OwnedReadHalf,
    events: mpsc::Sender<Event>,
) {
    let made = tokio::task::spawn_blocking({
        let events = events.clone();
        move || make(&start, events)
    })
    .await;
    let (machine, vsock_server) = match made {
        Ok(Ok(made)) => made,
        Ok(Err(status)) => {
            tell(&events, boot_failed(&status)).await;
            return;
        }
        Err(e) => {
            tell(
                &events,
                boot_failed(&Status::internal(format!("making the machine panicked: {e}"))),
            )
            .await;
            return;
        }
    };
    let generation = machine.vsock().map_or(0, |v| v.generation());
    let stop = machine.stop_handle();
    let shared = Arc::new(Shared {
        closing: AtomicBool::new(false),
        asked: Mutex::new(Some(Vec::new())),
        suspended: watch::channel(false).0,
    });
    let (ended_tx, mut ended_rx) = oneshot::channel();
    let thread_shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("machined-vm-run".into())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            // The helper waits for it: gone, it ends too.
            let _ = ended_tx.send(run_thread(machine, &thread_shared));
        });
    if let Err(e) = spawned {
        tell(
            &events,
            boot_failed(&Status::resource_exhausted(format!("a machine's run thread: {e}"))),
        )
        .await;
        return;
    }
    tell(&events, event::Kind::Started(generation)).await;
    tokio::spawn(follow_suspension(shared.suspended.subscribe(), events.clone()));
    let (answers, answering) = mpsc::unbounded_channel();
    tokio::spawn(answer_in_order(answering, events.clone()));
    let mut reading = true;
    let how = loop {
        tokio::select! {
            how = &mut ended_rx => break how.unwrap_or_else(|_| ended(How::Failed, "the machine's run thread panicked")),
            request = wire::read::<_, Request>(from), if reading => match request {
                Ok(Some(Request { kind: Some(kind) })) => take(kind, &shared, &stop, &answers),
                Ok(Some(Request { kind: None })) => log::warn!("an empty request from machined"),
                // machined gone, or its socket broken: the machine goes too.
                Ok(None) | Err(_) => {
                    reading = false;
                    close(&shared, &stop);
                }
            },
        }
    };
    vsock_server.stop();
    tell(&events, how).await;
}

/// Makes the machine and serves its vsock socket.
fn make(
    start: &wire::Start,
    events: mpsc::Sender<Event>,
) -> Result<(Machine, vsock::Server), Status> {
    let spec =
        start.spec.clone().ok_or_else(|| Status::invalid_argument("a start names its machine"))?;
    let plan = checks::plan(std::path::Path::new(&start.images), &spec)?;
    let mut machine = if start.snapshot.is_empty() {
        Machine::boot_linux(plan.config, &plan.boot).map_err(errors::status)?
    } else {
        restore(&start.snapshot, plan.config)?
    };
    machine.set_console(Console(events));
    let vsock =
        machine.vsock().ok_or_else(|| Status::internal("a machine booting Linux has a vsock"))?;
    let server =
        vsock::Server::start(PathBuf::from(&start.vsock_socket), &vsock, &spec.vsock_listen_ports)
            .map_err(|e| Status::internal(format!("the machine's vsock socket: {e}")))?;
    Ok((machine, server))
}

#[cfg(target_os = "linux")]
fn restore(snapshot: &str, config: libmachine::MachineConfig) -> Result<Machine, Status> {
    let snapshot = libmachine::Snapshot::open(snapshot).map_err(errors::status)?;
    Machine::restore(&snapshot, config).map_err(errors::status)
}

#[cfg(not(target_os = "linux"))]
fn restore(_: &str, _: libmachine::MachineConfig) -> Result<Machine, Status> {
    Err(Status::unimplemented(
        "machines on Hypervisor.framework can't be restored yet (board 13399)",
    ))
}

/// Stops the machine for good.
fn close(shared: &Shared, stop: &StopHandle) {
    shared.closing.store(true, Ordering::SeqCst);
    stop.stop();
}

/// Takes machined's request: a stop at once; what the run thread does between runs queued for
/// it, its answer's receiver queued to be sent in order.
fn take(
    kind: request::Kind,
    shared: &Shared,
    stop: &StopHandle,
    answers: &mpsc::UnboundedSender<oneshot::Receiver<Result<(), Status>>>,
) {
    let (reply, answer) = oneshot::channel();
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "requests are KVM's alone")
    )]
    let asked = match kind {
        request::Kind::Stop(_) => return close(shared, stop),
        #[cfg(target_os = "linux")]
        request::Kind::Snapshot(path) => Asked::Snapshot(path.into(), reply),
        #[cfg(target_os = "linux")]
        request::Kind::Suspend(dir) => Asked::Suspend(dir.into(), reply),
        #[cfg(target_os = "linux")]
        request::Kind::Resume(_) => Asked::Resume(reply),
        #[cfg(not(target_os = "linux"))]
        request::Kind::Snapshot(_) | request::Kind::Suspend(_) | request::Kind::Resume(_) => {
            let _ = reply.send(Err(Status::unimplemented(
                "machines on Hypervisor.framework can't be snapshotted or suspended yet (board 13399)",
            )));
            // The answer goes in its turn.
            let _ = answers.send(answer);
            return;
        }
    };
    #[cfg(target_os = "linux")]
    {
        // Gone: its sender dropped, which answers it "the machine ended".
        if let Some(queue) = lock(&shared.asked).as_mut() {
            queue.push(asked);
        }
        let _ = answers.send(answer);
        // The run thread takes it once its run stops.
        stop.stop();
    }
}

/// Sends each request's answer as it comes, in the order they were asked.
async fn answer_in_order(
    mut answering: mpsc::UnboundedReceiver<oneshot::Receiver<Result<(), Status>>>,
    events: mpsc::Sender<Event>,
) {
    while let Some(answer) = answering.recv().await {
        let result =
            answer.await.unwrap_or_else(|_| Err(Status::failed_precondition("the machine ended")));
        if !tell(&events, event::Kind::Answer(Answer::of(result))).await {
            return;
        }
    }
}

/// Tells machined each change of whether the guest is suspended.
async fn follow_suspension(mut suspended: watch::Receiver<bool>, events: mpsc::Sender<Event>) {
    while suspended.changed().await.is_ok() {
        let now = *suspended.borrow_and_update();
        if !tell(&events, event::Kind::Suspended(now)).await {
            return;
        }
    }
}

/// The run thread: runs the machine, doing what is asked between runs, until it ends; frees it,
/// then says how it ended.
fn run_thread(mut machine: Machine, shared: &Shared) -> event::Kind {
    let how = loop {
        match machine.run() {
            Ok(ExitReason::Stopped | ExitReason::TimedOut)
                if shared.closing.load(Ordering::SeqCst) =>
            {
                break ended(How::Stopped, "");
            }
            // Any other stop is a request's.
            Ok(ExitReason::Stopped | ExitReason::TimedOut) => {
                if let Some(how) = between_runs(&mut machine, shared) {
                    break how;
                }
            }
            Ok(reason) => break guest_ended(reason),
            Err(e) => break ended(How::Failed, e.to_string()),
        }
    };
    // What is still asked for fails, its senders dropped.
    lock(&shared.asked).take();
    shared.suspended.send_replace(false);
    drop(machine);
    how
}

fn guest_ended(reason: ExitReason) -> event::Kind {
    match reason {
        ExitReason::Poweroff => ended(How::Poweroff, ""),
        ExitReason::Reset => ended(How::Reset, ""),
        ExitReason::Shutdown => ended(How::Shutdown, ""),
        other => ended(How::Failed, format!("{other:?}")),
    }
}

/// Between runs: does what is asked, and while the guest is suspended waits until something wakes
/// it. None when the guest is to run again; how the machine ended if it did meanwhile.
fn between_runs(machine: &mut Machine, shared: &Shared) -> Option<event::Kind> {
    loop {
        let asked = lock(&shared.asked).as_mut().map(std::mem::take).unwrap_or_default();
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut, reason = "no resumes without KVM"))]
        let mut resumed: Vec<oneshot::Sender<Result<(), Status>>> = Vec::new();
        #[cfg_attr(
            not(target_os = "linux"),
            allow(clippy::never_loop, reason = "requests are KVM's alone (board 13399)")
        )]
        for asked in asked {
            match asked {
                #[cfg(target_os = "linux")]
                Asked::Snapshot(path, reply) => {
                    // The caller may have gone: the snapshot stays, as its file.
                    let _ = reply.send(machine.snapshot(&path).map_err(errors::status));
                }
                #[cfg(target_os = "linux")]
                Asked::Suspend(dir, reply) => match suspend_when_idle(machine, &dir, shared) {
                    // A caller gone leaves the guest suspended, as asked.
                    Ok(done) => drop(reply.send(done)),
                    Err(how) => return Some(how),
                },
                #[cfg(target_os = "linux")]
                Asked::Resume(reply) => resumed.push(reply),
            }
        }
        if !resumed.is_empty() || !machine.suspended() {
            shared.suspended.send_replace(false);
            for reply in resumed {
                let _ = reply.send(Ok(()));
            }
            return None;
        }
        // A request that came while this one was taken: take it first (its stop was used up).
        if lock(&shared.asked).as_ref().is_some_and(|r| !r.is_empty()) {
            continue;
        }
        shared.suspended.send_replace(true);
        match machine.wait_woken(None) {
            Woken::Device | Woken::TimedOut => {
                shared.suspended.send_replace(false);
                return None;
            }
            Woken::Stop if shared.closing.load(Ordering::SeqCst) => {
                return Some(ended(How::Stopped, ""));
            }
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
) -> Result<Result<(), Status>, event::Kind> {
    use libmachine::Error;
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
                return Ok(Err(errors::status(Error::Busy(format!(
                    "the guest was not idle within {SUSPEND_IDLE_WAIT:?}: {why}"
                )))));
            }
            Err(Error::Busy(_)) => match machine.run_for(IDLE_POLL) {
                Ok(ExitReason::Stopped) if shared.closing.load(Ordering::SeqCst) => {
                    return Err(ended(How::Stopped, ""));
                }
                // Another request's stop: it is still queued, taken after this one.
                Ok(ExitReason::Stopped | ExitReason::TimedOut) => {}
                Ok(reason) => return Err(guest_ended(reason)),
                Err(e) => return Err(ended(How::Failed, e.to_string())),
            },
            Err(e) => return Ok(Err(errors::status(e))),
        }
    }
}
