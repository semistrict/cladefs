// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine's helper process (board 13460): the process its VM runs in, one per machine, which
//! machined starts (`machined-vm`, beside it) and manages over the socket that is the helper's
//! standard input (`wire`). Hypervisor.framework allows one VM per process, and a VM's process
//! is what its failures, its memory and its limits are scoped to, as Firecracker's is; machined,
//! the long-lived daemon with the API, keeps no VM of its own, and outlives any helper.
//!
//! The helper makes its machine from the spec it is given (checked and planned again, as machined
//! did), serves the machine's vsock socket itself (`vsock`, Firecracker's protocol), sends its
//! console's bytes, and runs it on a thread of its own. machined's stop, or its end (the socket's
//! gone), ends the machine, and so the helper.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use libmachine::{ExitReason, Machine};
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};
use tonic::Status;

use crate::{checks, errors, vsock};

pub(crate) mod wire;

use wire::{Ended, Event, How, event};

/// Events queued for machined: past that, the console's writer waits.
const QUEUED: usize = 256;
/// The run thread only waits: its stack needs little.
const THREAD_STACK: usize = 256 << 10;

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

/// Makes the machine as `start` says and runs it to its end, stopping it when machined says, or
/// goes.
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
    let stop = machine.stop_handle();
    let (ended_tx, mut ended_rx) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("machined-vm-run".into())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            // The helper waits for it: gone, it ends too.
            let _ = ended_tx.send(run_thread(machine));
        });
    if let Err(e) = spawned {
        tell(
            &events,
            boot_failed(&Status::resource_exhausted(format!("a machine's run thread: {e}"))),
        )
        .await;
        return;
    }
    tell(&events, event::Kind::Started(wire::Empty {})).await;
    let mut reading = true;
    let how = loop {
        tokio::select! {
            how = &mut ended_rx => break how.unwrap_or_else(|_| ended(How::Failed, "the machine's run thread panicked")),
            // machined's stop, which says nothing more than that, or machined gone, or its socket
            // broken: the machine goes either way.
            _ = wire::read::<_, wire::Empty>(from), if reading => {
                reading = false;
                stop.stop();
            }
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
    let mut plan = checks::plan(std::path::Path::new(&start.images), &spec)?;
    plan.config.extent_daemon =
        Some(Arc::new(libmachine::ExtentDaemon::at_socket(&start.cladefs_socket)));
    let mut machine = Machine::boot_linux(plan.config, &plan.boot).map_err(errors::status)?;
    machine.set_console(Console(events));
    let vsock =
        machine.vsock().ok_or_else(|| Status::internal("a machine booting Linux has a vsock"))?;
    let server =
        vsock::Server::start(PathBuf::from(&start.vsock_socket), &vsock, &spec.vsock_listen_ports)
            .map_err(|e| Status::internal(format!("the machine's vsock socket: {e}")))?;
    Ok((machine, server))
}

/// The run thread: runs the machine until it ends; frees it, then says how it ended.
fn run_thread(mut machine: Machine) -> event::Kind {
    let how = match machine.run() {
        // Only machined's stop stops it.
        Ok(ExitReason::Stopped) => ended(How::Stopped, ""),
        Ok(ExitReason::Poweroff) => ended(How::Poweroff, ""),
        Ok(ExitReason::Reset) => ended(How::Reset, ""),
        Ok(ExitReason::Shutdown) => ended(How::Shutdown, ""),
        Ok(other) => ended(How::Failed, format!("{other:?}")),
        Err(e) => ended(How::Failed, e.to_string()),
    };
    drop(machine);
    how
}
