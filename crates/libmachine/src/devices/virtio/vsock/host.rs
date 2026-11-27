// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The host's end of a machine's vsock: in-process handles, usable from any
//! thread while another runs the machine.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::sys::EventFd;

use super::packet::{SHUTDOWN_BOTH, SHUTDOWN_RCV, SHUTDOWN_SEND};
use super::state::{Phase, Poll, State};

/// What the device thread and the host's handles share.
#[derive(Debug)]
pub(crate) struct Shared {
    state: Mutex<State>,
    /// Notified whenever the device thread changed the state.
    changed: Condvar,
    /// Signalled when the host side has packets for the guest: the eventfd
    /// of the device that serves it.
    wake: EventFd,
}

impl Shared {
    pub(crate) fn new(guest_cid: u64, wake: EventFd) -> Self {
        Self { state: Mutex::new(State::new(guest_cid)), changed: Condvar::new(), wake }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// After the device thread changed the state: wakes the host's waiters.
    pub(crate) fn notify(&self) {
        self.changed.notify_all();
    }

    /// Wakes the device thread if the host side gave it something to send.
    fn wake_device(&self, state: &mut State) {
        if std::mem::take(&mut state.wake) {
            // An eventfd write fails only when the counter would overflow,
            // in which case the thread has a wakeup pending anyway.
            let _ = self.wake.write(1);
        }
    }

    /// Runs `op` on the state until it is ready, waiting for the device
    /// thread between tries, at most until `timeout` has passed.
    fn wait<T>(
        &self,
        timeout: Option<Duration>,
        mut op: impl FnMut(&mut State) -> Poll<T>,
    ) -> io::Result<T> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut state = self.lock();
        loop {
            let result = op(&mut state);
            self.wake_device(&mut state);
            if let Poll::Ready(result) = result {
                return result;
            }
            state = match deadline {
                None => self.changed.wait(state).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out waiting for the guest",
                        ));
                    }
                    self.changed.wait_timeout(state, left).unwrap_or_else(|e| e.into_inner()).0
                }
            };
        }
    }

    /// Runs `op` on the state once, waking the device thread if needed.
    fn with<T>(&self, op: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.lock();
        let result = op(&mut state);
        self.wake_device(&mut state);
        result
    }
}

/// The host's end of a machine's virtio-vsock device: connects to ports in
/// the guest and listens for the guest's connections, in this process (no
/// host sockets: the guest's peer is the host, CID 2, and each machine has
/// its own ports).
///
/// Cheap to clone and usable from any thread, including while another
/// thread is in [`Machine::run`](crate::Machine::run): the guest only makes
/// progress while its machine runs.
///
/// Every connection is reset when the guest's driver resets the device,
/// when [`reset_connections`](Vsock::reset_connections) is called (as a
/// restore does), when the guest stops and when the machine is dropped.
///
/// When a run of the machine ends with the guest powering off, resetting or
/// triple-faulting, every operation, waiting or new, fails at once with
/// `ConnectionReset` ([`is_guest_stopped`](crate::devices::virtio::vsock::is_guest_stopped)
/// says why), until the guest's driver starts the device again: a guest
/// that boots again has connections as before. Once the machine is
/// dropped, operations fail with `NotConnected`.
#[derive(Debug, Clone)]
pub struct Vsock {
    shared: Arc<Shared>,
}

impl Vsock {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// The guest's CID.
    pub fn guest_cid(&self) -> u64 {
        self.shared.lock().guest_cid()
    }

    /// Connects to `port` in the guest, waiting at most `timeout` for the
    /// guest to answer. During a transport reset the guest has not taken
    /// yet (as right after a restore), waits for it to take it first,
    /// within the same `timeout`: a connection made before would be reset.
    ///
    /// Fails with `ConnectionRefused` if nothing listens there or the
    /// guest's driver is not up yet, `TimedOut` if the guest does not answer
    /// (it is not running), `ConnectionReset` if the guest has stopped (see
    /// [`Vsock`]) and `NotConnected` once the machine is gone.
    pub fn connect(&self, port: u32, timeout: Duration) -> io::Result<VsockStream> {
        let deadline = Instant::now() + timeout;
        let id = self.after_a_reset(timeout, |s| s.connect(port))?;
        let stream = VsockStream::new(self.shared.clone(), id);
        let left = deadline.saturating_duration_since(Instant::now());
        let connected = self.shared.wait(Some(left), |s| match s.phase(id) {
            Phase::Connecting => Poll::Wait,
            Phase::Connected => Poll::Ready(Ok(())),
            Phase::Closed(kind) => {
                Poll::Ready(Err(s.failed(kind.unwrap_or(io::ErrorKind::ConnectionReset))))
            }
        });
        match connected {
            Ok(()) => Ok(stream),
            Err(e) => {
                if e.kind() == io::ErrorKind::TimedOut {
                    self.shared.with(|s| s.abandon(id));
                }
                Err(e)
            }
        }
    }

    /// Runs `op` once no transport reset is under way: during one (the
    /// guest has not taken it yet, and would reset what is made meanwhile)
    /// waits until the guest has taken it, at most `timeout`. The device
    /// hears of that at once (the guest's repost of its event buffer
    /// notifies it, see `look_at_events`), so nothing polls; it is also
    /// asked to look once at the start, in case the guest reposted before
    /// the device could ask to be told.
    ///
    /// Before (board 13177), a connect during a reset was refused and the
    /// agent client tried again after 2, 4, 8 ms: after a restore, its
    /// first answer came up to 8 ms after the guest had taken the reset.
    fn after_a_reset<T>(
        &self,
        timeout: Duration,
        mut op: impl FnMut(&mut State) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut asked = false;
        let result = self.shared.wait(Some(timeout), |s| {
            // A guest that stopped, or a machine that is gone, has no reset
            // under way: `op` says what fails.
            if s.resetting {
                if !std::mem::replace(&mut asked, true) {
                    s.wake = true;
                }
                return Poll::Wait;
            }
            Poll::Ready(op(s))
        });
        result.map_err(|e| {
            if e.kind() == io::ErrorKind::TimedOut && self.shared.lock().resetting {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the guest to take a transport reset",
                )
            } else {
                e
            }
        })
    }

    /// Listens on host port `port` for connections from the guest
    /// (to CID 2, port `port`). Fails with `AddrInUse` if something already
    /// listens there.
    pub fn listen(&self, port: u32) -> io::Result<VsockListener> {
        self.shared.with(|s| s.listen(port))?;
        Ok(VsockListener { shared: self.shared.clone(), port })
    }

    /// Listens on every host port no other listener holds, as Firecracker passes the guest's
    /// connections to any port on (its `uds_path` + `_PORT`): each connection accepted says the
    /// port it was to ([`VsockStream::local_port`]). Fails with `AddrInUse` if one listens so
    /// already.
    pub fn listen_any(&self) -> io::Result<VsockListener> {
        self.listen(super::state::PORT_ANY)
    }

    /// Which restore of a snapshot the machine is (0: it booted), for the
    /// agent's reseed (`agent`).
    pub fn generation(&self) -> u64 {
        self.shared.lock().generation
    }

    /// The guest stopped (the machine's run ended with it powering off,
    /// resetting or triple-faulting): every connection and every waiting
    /// operation fails at once.
    pub(crate) fn guest_stopped(&self) {
        self.shared.with(|s| s.guest_stopped());
        self.shared.notify();
    }

    /// Resets every connection, and has the guest do the same (a transport
    /// reset event), as a restore from a snapshot does. Listeners on both
    /// sides stay. For testing how guest agents and host code handle it.
    pub fn reset_connections(&self) {
        self.shared.with(|s| s.transport_reset());
        self.shared.notify();
    }
}

/// A host port listening for the guest's connections; stops listening when
/// dropped, resetting connections not accepted yet.
#[derive(Debug)]
pub struct VsockListener {
    shared: Arc<Shared>,
    port: u32,
}

impl VsockListener {
    /// The host port.
    pub fn port(&self) -> u32 {
        self.port
    }

    /// Waits for the guest's next connection, at most `timeout` (forever
    /// with `None`). Fails at once with `ConnectionReset` while the guest
    /// is stopped (see [`Vsock`]).
    pub fn accept(&self, timeout: Option<Duration>) -> io::Result<VsockStream> {
        let id = self.shared.wait(timeout, |s| s.accept(self.port))?;
        Ok(VsockStream::new(self.shared.clone(), id))
    }
}

impl Drop for VsockListener {
    fn drop(&mut self) {
        self.shared.with(|s| s.unlisten(self.port));
    }
}

/// A connected vsock stream to the guest, blocking like a `TcpStream`:
/// reads wait for data and return 0 once the guest has shut down its
/// sending; a reset fails reads (after the data received before it) and
/// writes with `ConnectionReset`. `&VsockStream` reads and writes too, so
/// that one thread can read while another writes.
///
/// Dropping it closes the connection: what was written still goes to the
/// guest, then a shutdown.
#[derive(Debug)]
pub struct VsockStream {
    shared: Arc<Shared>,
    id: u64,
    ports: (u32, u32),
    /// Timeouts in nanoseconds, 0 for none.
    read_timeout: AtomicU64,
    write_timeout: AtomicU64,
}

fn to_nanos(timeout: Option<Duration>) -> io::Result<u64> {
    match timeout {
        None => Ok(0),
        Some(t) if t.is_zero() => {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "a zero timeout is not allowed"))
        }
        Some(t) => Ok(u64::try_from(t.as_nanos()).unwrap_or(u64::MAX)),
    }
}

fn from_nanos(nanos: u64) -> Option<Duration> {
    (nanos != 0).then(|| Duration::from_nanos(nanos))
}

impl VsockStream {
    fn new(shared: Arc<Shared>, id: u64) -> Self {
        let ports = shared.lock().ports(id).unwrap_or_default();
        Self {
            shared,
            id,
            ports,
            read_timeout: AtomicU64::new(0),
            write_timeout: AtomicU64::new(0),
        }
    }

    /// The host's port.
    pub fn local_port(&self) -> u32 {
        self.ports.0
    }

    /// The guest's port.
    pub fn peer_port(&self) -> u32 {
        self.ports.1
    }

    /// How long a read waits before failing with `TimedOut` (`None`:
    /// forever). Zero is refused, as for a `TcpStream`.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.read_timeout.store(to_nanos(timeout)?, Ordering::Relaxed);
        Ok(())
    }

    /// How long a write (or flush) waits for the guest's credit before
    /// failing with `TimedOut` (`None`: forever).
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.write_timeout.store(to_nanos(timeout)?, Ordering::Relaxed);
        Ok(())
    }

    pub fn read_timeout(&self) -> Option<Duration> {
        from_nanos(self.read_timeout.load(Ordering::Relaxed))
    }

    pub fn write_timeout(&self) -> Option<Duration> {
        from_nanos(self.write_timeout.load(Ordering::Relaxed))
    }

    /// Shuts down reading (what the guest sends is dropped), writing (the
    /// guest reads to its end once what was written has gone) or both.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => SHUTDOWN_RCV,
            Shutdown::Write => SHUTDOWN_SEND,
            Shutdown::Both => SHUTDOWN_BOTH,
        };
        self.shared.with(|s| s.shutdown(self.id, how));
        Ok(())
    }
}

impl Read for &VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.shared.wait(self.read_timeout(), |s| s.read(self.id, buf))
    }
}

impl Write for &VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.shared.wait(self.write_timeout(), |s| s.write(self.id, buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.shared.wait(self.write_timeout(), |s| s.flush(self.id))
    }
}

impl Read for VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        self.shared.with(|s| s.release(self.id));
    }
}
