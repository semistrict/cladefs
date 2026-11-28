// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: doc-4's stream transport (board 14663): a guest's data requests, `READ`, `WRITE`,
//! `FREE`, `COMMIT`, `ROOT` and `INFO`, come over vsock to the daemon, which serves them itself,
//! with the interpreter a VMM's device serves its queues with (`cladefs_core::extent_requests`), in
//! the guest's dialect, the connection's cell their backend. Its VMM copies them between guest
//! memory and the vsock connection, as a stock vsock device does; the daemon reaches no guest memory.
//!
//! The VM process binds a listening socket in its jail where its vsock device connects the guest's
//! connections to the device's port (Firecracker's `<uds_path>_<port>`), and hands it to the daemon
//! on its connection, once its cell is open ([`crate::wire::GUEST_STREAM`]): bound so to that
//! authorized cell and tenant. The daemon accepts on it, on a thread of the connection's own, as the
//! VM process may make the accept wait, the connections the VM process makes (`SO_PEERCRED`),
//! closing any other, as another's may reach the listener too. A guest connects again after its
//! connection resets, so each is served, until the connection's stop shuts the listener down, and
//! every stream with it.
//!
//! Each stream is read on a thread of its own, each request ([`StreamRequest`]) checked against the
//! protocol's bounds as it is read, a stream past them closed, as nothing after it could be read in
//! step; admitted in the connection's turns ([`super::Work`]); served as a future
//! ([`super::serve_future`]), its bytes held in its tenant's bytes in flight while it is; and
//! answered ([`StreamReply`]) on a thread of the stream's own, which writes replies in the order
//! their requests are done, so that a guest slow to read holds up no serving. A stream has
//! [`STREAM_REQUESTS`] requests in flight at most, its reading waiting for one to be done past them.
//! A request no transport's guest sends on it, any of the window's or of the device's state, is
//! refused (`EINVAL`): its VMM's device serves those on its queues. A panic serving one halts the
//! cell, and answers it with nothing written, as an unanswered request is.

use super::{Connection, Turn, Work, serve_future};
use crate::reads::Kind;
use crate::wire::{COMMIT, FREE, Header, INFO, READ, ROOT, WRITE};
use bucket_cell::trace;
use cladefs_core::extent_requests::protocol::{
    self, Bounds, MAX_EXTENT, MAX_READABLE, MAX_WRITABLE, RequestHeader, Response, StreamReply,
    StreamRequest,
};
use cladefs_core::extent_requests::{Backend, Buffers, Guest, GuestBackend, GuestNotices, handle};
use cladefs_core::{
    Commit, Errno, ExtentId, ExtentInfo, OpenCell, VolatileSlice, VolatileSliceMut, WindowMapping,
    WriteBarriers,
};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use zerocopy::{FromBytes, IntoBytes};

/// The bounds a guest's requests on a stream keep to: its device's, with no window, which its
/// VMM's device serves.
const BOUNDS: Bounds = Bounds {
    block_size: OpenCell::BLOCK_SIZE as u64,
    max_extent: MAX_EXTENT,
    map_alignment: OpenCell::BLOCK_SIZE as u64,
    window: 0,
};

/// The most requests a stream has in flight: its reading waits for one to be done past them, so
/// that a guest holds no more of the daemon's memory than that many requests' bytes. Two queues'
/// worth of a VMM's device (`QUEUE_SIZE`).
pub(super) const STREAM_REQUESTS: usize = 2 * protocol::QUEUE_SIZE as usize;

/// A connection's guest's stream: none yet; accepted on the VM process's listener, and the streams
/// it accepted, which a stop shuts down; or stopped as the connection ends, after which none
/// starts.
#[derive(Default)]
pub(super) enum GuestStream {
    #[default]
    Unstarted,
    Listening {
        listener: Arc<OwnedFd>,
        streams: Arc<Mutex<Vec<Arc<UnixStream>>>>,
    },
    Stopped,
}

/// A guest's request a stream received, to serve in its connection's turns: the bytes the device
/// reads, the bytes it writes, and the reply it is owed.
pub(super) struct Received {
    ordered_only: bool,
    readable: Vec<u8>,
    writable: usize,
    owed: Owed,
}

/// The reply a stream's request is owed, to the request tagged `tag`: sent once
/// ([`Owed::send`]), or, dropped unsent, as one of nothing written, which its guest takes for an
/// `EIO`, so that none waits for good, whatever ends its serving: a panic, the connection ending
/// first, or no thread to serve it.
struct Owed {
    tag: u64,
    replies: Option<Replies>,
}

impl Owed {
    /// Sends the reply, `written` the bytes the device wrote, and counts its request done.
    fn send(mut self, written: Vec<u8>) {
        let replies = self.replies.take().expect("a reply sent once");
        Self::reply(&replies, self.tag, written);
    }

    fn reply(replies: &Replies, tag: u64, written: Vec<u8>) {
        // At most MAX_WRITABLE, a u32's.
        let len = written.len() as u32;
        let head = StreamReply { tag: tag.to_le(), written: len.to_le(), reserved: 0 };
        // Its writer gone, the stream ended: no guest to tell.
        let _ = replies.sender.send((head, written));
        replies.credit.give();
    }
}

impl Drop for Owed {
    fn drop(&mut self) {
        if let Some(replies) = self.replies.take() {
            Self::reply(&replies, self.tag, Vec::new());
        }
    }
}

/// Where a stream's replies go, and its requests in flight, one of which each reply ends.
#[derive(Clone)]
struct Replies {
    sender: mpsc::Sender<(StreamReply, Vec<u8>)>,
    credit: Arc<Credit>,
}

/// A stream's requests in flight, [`STREAM_REQUESTS`] at most, and what its reading waits on.
#[derive(Default)]
struct Credit {
    in_flight: Mutex<usize>,
    done: Condvar,
}

impl Credit {
    /// Waits for room for a request more, and counts it.
    fn take(&self) {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(PoisonError::into_inner);
        while *in_flight >= STREAM_REQUESTS {
            in_flight = self.done.wait(in_flight).unwrap_or_else(PoisonError::into_inner);
        }
        *in_flight += 1;
    }

    /// A request is done.
    fn give(&self) {
        *self.in_flight.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.done.notify_one();
    }
}

/// The requests a guest sends on a stream: doc-4's data requests (`F_STREAM`).
fn on_stream(type_: u32) -> bool {
    matches!(
        type_,
        protocol::VIRTIO_EXTENT_T_READ
            | protocol::VIRTIO_EXTENT_T_WRITE
            | protocol::VIRTIO_EXTENT_T_FREE
            | protocol::VIRTIO_EXTENT_T_COMMIT
            | protocol::VIRTIO_EXTENT_T_ROOT
            | protocol::VIRTIO_EXTENT_T_INFO
    )
}

/// What a guest tells its device beside its cell's requests goes to its VMM's device, on its
/// queues: never on a stream, which refuses those requests before they are served ([`on_stream`]).
struct OnItsQueues;

impl GuestNotices for OnItsQueues {
    fn set_frozen(&self, _: bool) {}

    fn resize_answered(&self, _: u64, _: bool) {}
}

impl Connection {
    /// Serves a [`crate::wire::GUEST_STREAM`]: accepts its guest's streams on `listener`, once its
    /// cell is open, serving their requests with it, their uploaded commits answered once ordered
    /// if `ordered_only`.
    pub(super) fn serve_guest_stream(
        self: &Arc<Self>,
        ordered_only: bool,
        listener: OwnedFd,
        answer: super::Answer,
    ) {
        let reply = answer.reply_header();
        let started = self.serving().and_then(|_| {
            let listener = Arc::new(check_listener(listener)?);
            // cladefs: started under the lock its stop takes, so that none starts once the
            // connection stopped it, which no stop would then reach: its accepting would hold the
            // connection, and its cell, for good.
            let mut state = self.guest_stream.lock().unwrap_or_else(PoisonError::into_inner);
            match *state {
                GuestStream::Unstarted => {}
                GuestStream::Listening { .. } => return Err(Errno::EEXIST),
                GuestStream::Stopped => return Err(Errno::EBADF),
            }
            let streams = Arc::new(Mutex::new(Vec::new()));
            let (connection, accepting, accepted) =
                (self.clone(), listener.clone(), streams.clone());
            std::thread::Builder::new()
                .name("cladefsd stream accept".into())
                .spawn(move || connection.accept_streams(&accepting, &accepted, ordered_only))
                .map_err(|err| {
                    log::error!("cladefsd: accepting a guest's streams: {err}");
                    Errno::EAGAIN
                })?;
            *state = GuestStream::Listening { listener, streams };
            Ok(())
        });
        answer.send(started, reply, Vec::new(), Vec::new());
    }

    /// Accepts its guest's streams on `listener`, recording each in `streams`, and serves each on
    /// threads of its own: any connection from another process closed, until the connection's stop
    /// shuts the listener down, or it fails.
    fn accept_streams(
        self: Arc<Self>,
        listener: &OwnedFd,
        streams: &Mutex<Vec<Arc<UnixStream>>>,
        ordered_only: bool,
    ) {
        loop {
            // Waited for here, not in the accept, which the VM process may make return at once
            // (`O_NONBLOCK`, the listener's file its too), which would spin; woken by a shutdown.
            let mut poll =
                libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: Polls one descriptor this holds.
            if unsafe { libc::poll(&mut poll, 1, -1) } < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                log::error!("cladefsd: waiting for a guest's stream: {err}");
                return;
            }
            // SAFETY: Accepts on a listener this holds, into a descriptor owned below.
            let accepted = unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if accepted < 0 {
                let err = std::io::Error::last_os_error();
                match err.kind() {
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock => continue,
                    // Shut down by the connection's stop, or failing: no stream comes.
                    _ => {
                        log::info!("cladefsd: no more guest streams accepted: {err}");
                        return;
                    }
                }
            }
            // SAFETY: Just accepted, owned here.
            let socket = unsafe { OwnedFd::from_raw_fd(accepted) };
            let (theirs, ours) = (peer_pid(&socket), self.vm_pid());
            if theirs.is_none() || theirs != ours {
                log::warn!(
                    "cladefsd: refused a guest stream from pid {theirs:?}, not the VM process's \
                     ({ours:?})"
                );
                continue;
            }
            let stream = Arc::new(UnixStream::from(socket));
            {
                let mut streams = streams.lock().unwrap_or_else(PoisonError::into_inner);
                // Recorded under the lock the stop takes, or shut down here, the stop past.
                if !matches!(
                    *self.guest_stream.lock().unwrap_or_else(PoisonError::into_inner),
                    GuestStream::Listening { .. }
                ) {
                    return;
                }
                streams.push(stream.clone());
            }
            if let Err(err) = self.clone().serve_stream(stream.clone(), ordered_only) {
                log::error!("cladefsd: serving a guest's stream: {err}");
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// Serves `stream`, read on a thread of its own and answered on another.
    fn serve_stream(
        self: Arc<Self>,
        stream: Arc<UnixStream>,
        ordered_only: bool,
    ) -> std::io::Result<()> {
        let (sender, replies) = mpsc::channel::<(StreamReply, Vec<u8>)>();
        let writer = stream.clone();
        std::thread::Builder::new()
            .name("cladefsd stream replies".into())
            .spawn(move || write_replies(&writer, &replies))?;
        let replies = Replies { sender, credit: Arc::new(Credit::default()) };
        std::thread::Builder::new()
            .name("cladefsd stream requests".into())
            .spawn(move || self.read_requests(&stream, &replies, ordered_only))?;
        Ok(())
    }

    /// Reads `stream`'s requests, admitting each in the connection's turns, until it ends or
    /// breaks the protocol's bounds, when it is shut down.
    fn read_requests(self: Arc<Self>, stream: &UnixStream, replies: &Replies, ordered_only: bool) {
        let mut reading = stream;
        loop {
            let mut head = StreamRequest::default();
            if let Err(err) = reading.read_exact(head.as_mut_bytes()) {
                // Its guest's connection closed, or reset as its VM was snapshotted, or the
                // connection's stop shut it down.
                if err.kind() != std::io::ErrorKind::UnexpectedEof {
                    log::info!("cladefsd: a guest's stream ended: {err}");
                }
                return;
            }
            let StreamRequest { tag, readable, writable } = head;
            let (tag, readable, writable) = (
                u64::from_le(tag),
                u32::from_le(readable) as usize,
                u32::from_le(writable) as usize,
            );
            if !(size_of::<RequestHeader>()..=MAX_READABLE).contains(&readable)
                || !(size_of::<Response>()..=MAX_WRITABLE).contains(&writable)
            {
                log::warn!(
                    "cladefsd: a guest's request of {readable} bytes to read and {writable} to \
                     write, out of the protocol's bounds: its stream closed"
                );
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return;
            }
            let mut bytes = vec![0u8; readable];
            if let Err(err) = reading.read_exact(&mut bytes) {
                log::info!("cladefsd: a guest's stream ended within a request: {err}");
                return;
            }
            replies.credit.take();
            let owed = Owed { tag, replies: Some(replies.clone()) };
            let received = Received { ordered_only, readable: bytes, writable, owed };
            if let Some(work) = self.admit(Work::Stream(received)) {
                self.clone().serve_in_turn(work);
            }
        }
    }

    /// Its VM process's pid: this process's, a test's in-process VM; none once it exited.
    fn vm_pid(&self) -> Option<libc::pid_t> {
        match &self.peer.process {
            None => libc::pid_t::try_from(std::process::id()).ok(),
            Some(process) => process.pid(),
        }
    }

    /// Stops accepting its guest's streams, and reading them, as the connection ends: the requests
    /// they took are served still.
    pub(super) fn stop_guest_stream(&self) {
        let stopped = {
            let mut state = self.guest_stream.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *state, GuestStream::Stopped)
        };
        let GuestStream::Listening { listener, streams } = stopped else { return };
        // SAFETY: Shuts down a listener this holds: its accepting thread woken, which ends.
        unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) };
        for stream in streams.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Serves `received`, a request its stream read and admitted, in its turn: as a future
    /// ([`serve_future`]) that answers it.
    pub(super) fn serve_stream_in_turn(self: Arc<Self>, received: Received) {
        let turn = Turn::new(self.clone());
        let connection = self.clone();
        serve_future(turn, async move { connection.serve_received(received).await });
    }

    async fn serve_received(self: Arc<Self>, received: Received) {
        let Received { ordered_only, mut readable, writable, owed } = received;
        let mut answer = StreamAnswer::new(self.clone(), owed);
        let header = RequestHeader::read_from_prefix(&readable).ok().map(|(header, _)| header);
        let type_ = header.map_or(0, |header| u32::from_le(header.type_));
        let guest_request = header
            .and_then(|header| std::num::NonZeroU64::new(u64::from_le(header.unique)))
            .map(|guest| cladefs_core::request::RequestId {
                guest,
                open_cell: self.named.get().cloned(),
            });
        answer.named(kind_of(type_), guest_request.as_ref());
        let mut written = vec![0u8; writable];
        if !on_stream(type_) {
            // Its response alone, the last bytes of what it writes: the rest zeros.
            let response = Response { status: Errno::EINVAL.raw() as u32, reserved: 0 };
            let at = writable - size_of::<Response>();
            written[at..].copy_from_slice(response.as_bytes());
            return answer.complete(written, Err(Errno::EINVAL));
        }
        // cladefs: its bytes held in its tenant's bytes in flight, as a socket's request's data is
        // (TASK-149.35): answered with nothing written if the connection ends waiting for room.
        let bytes = (readable.len() + writable) as u64;
        if self.take_in_flight(bytes).await.is_err() {
            return;
        }
        let _held = InFlight { connection: self.clone(), bytes };
        let mut buffers = Buffers::new();
        // SAFETY: The future's own bytes, which outlive the request's serving, below, and which
        // nothing else accesses meanwhile.
        unsafe {
            buffers.push_readable(readable.as_mut_ptr(), readable.len());
            buffers.push_writable(written.as_mut_ptr(), writable).expect("within MAX_WRITABLE");
        }
        let dialect = Guest { notices: &OnItsQueues, ordered_only };
        let parent = answer.span.parent();
        let served = handle(&*self, &dialect, BOUNDS, buffers);
        let handled = cladefs_core::request::within(guest_request, parent.around(served)).await;
        // What the device wrote: every writable byte, or nothing (`handle`).
        written.truncate(handled.len as usize);
        answer.complete(written, handled.status);
    }

    /// Its cell, to serve a request with: `EBADF` before it opened one, its halting's errno once a
    /// panic halted it (`crate::halt`, TASK-153.16), or once its keys are forgotten (TASK-411.7).
    fn serving(&self) -> Result<&Arc<OpenCell>, Errno> {
        let open_cell = self.cell().ok_or(Errno::EBADF)?;
        match open_cell.halted() {
            Some(errno) => Err(errno),
            None => Ok(open_cell),
        }
    }
}

/// Writes a stream's replies as they come, each whole, until every sender is gone or the stream
/// fails, when it is shut down, its reading with it.
fn write_replies(stream: &UnixStream, replies: &mpsc::Receiver<(StreamReply, Vec<u8>)>) {
    let mut writing = stream;
    for (reply, written) in replies {
        if let Err(err) =
            writing.write_all(reply.as_bytes()).and_then(|()| writing.write_all(&written))
        {
            log::info!("cladefsd: replying on a guest's stream: {err}");
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

/// An integer option of the socket `fd`, if it is one.
fn socket_option(fd: &OwnedFd, name: libc::c_int) -> Option<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: Reads an integer socket option of `fd` into `value`, `len` its size.
    let got = unsafe {
        libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, name, (&raw mut value).cast(), &mut len)
    };
    (got == 0).then_some(value)
}

/// `fd`, the VM process's listener ([`crate::wire::GUEST_STREAM`]): `EINVAL` unless it is a
/// listening Unix stream socket's, as the VM process may send any descriptor at all.
fn check_listener(fd: OwnedFd) -> Result<OwnedFd, Errno> {
    let is = |name, value| socket_option(&fd, name) == Some(value);
    match is(libc::SO_DOMAIN, libc::AF_UNIX)
        && is(libc::SO_TYPE, libc::SOCK_STREAM)
        && is(libc::SO_ACCEPTCONN, 1)
    {
        true => Ok(fd),
        false => Err(Errno::EINVAL),
    }
}

/// The pid of the process at the other end of `socket` as it connected (`SO_PEERCRED`).
fn peer_pid(socket: &OwnedFd) -> Option<libc::pid_t> {
    let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: Reads the peer's credentials into `credentials`, of `len` bytes.
    let read = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut len,
        )
    };
    (read == 0 && credentials.pid > 0).then_some(credentials.pid)
}

/// A request's bytes, held in its tenant's bytes in flight until dropped.
struct InFlight {
    connection: Arc<Connection>,
    bytes: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.connection.tenant.in_flight().sub(self.bytes);
    }
}

/// The wire's kind of a doc-4 request of `type_`, which the daemon's metrics and spans count it as.
fn kind_of(type_: u32) -> Option<u32> {
    Some(match type_ {
        protocol::VIRTIO_EXTENT_T_READ => READ,
        protocol::VIRTIO_EXTENT_T_WRITE => WRITE,
        protocol::VIRTIO_EXTENT_T_FREE => FREE,
        protocol::VIRTIO_EXTENT_T_COMMIT => COMMIT,
        protocol::VIRTIO_EXTENT_T_ROOT => ROOT,
        protocol::VIRTIO_EXTENT_T_INFO => INFO,
        _ => return None,
    })
}

/// A stream's request's answer, owed until it is sent ([`StreamAnswer::complete`]): its span,
/// ended as it is, and what the daemon's metrics count of it; and, its serving panicking, the cell
/// halted, its reply one of nothing written ([`Owed`]).
struct StreamAnswer {
    connection: Arc<Connection>,
    owed: Option<Owed>,
    kind: Option<u32>,
    started: std::time::Instant,
    span: trace::Span,
}

impl StreamAnswer {
    fn new(connection: Arc<Connection>, owed: Owed) -> Self {
        let started = std::time::Instant::now();
        Self { connection, owed: Some(owed), kind: None, started, span: trace::Span::none() }
    }

    /// Its request is of the wire's `kind`, a call for the guest request `guest`: its span begun.
    fn named(&mut self, kind: Option<u32>, guest: Option<&cladefs_core::request::RequestId>) {
        self.kind = kind;
        if let Some(kind) = kind {
            let cell = self.connection.cell().map(|open_cell| open_cell.cell());
            self.span = crate::otel::request_span(&Header::new(kind, 0), cell, guest);
        }
    }

    /// Sends its reply, `written` the bytes the device wrote, its request having come to `status`.
    fn complete(mut self, written: Vec<u8>, status: Result<(), Errno>) {
        if let Err(errno) = status {
            self.span.set(crate::otel::ERRNO, errno.raw() as i64);
            self.span.fail(errno);
        }
        if let Some(kind) = self.kind {
            crate::metrics::served(kind, self.started.elapsed(), status.is_err());
        }
        // Ended before the reply is sent, ready to export.
        drop(std::mem::replace(&mut self.span, trace::Span::none()));
        self.owed.take().expect("a request answered once").send(written);
    }
}

impl Drop for StreamAnswer {
    fn drop(&mut self) {
        if self.owed.is_some()
            && std::thread::panicking()
            && let Some(open_cell) = self.connection.cell()
        {
            let kind = self.kind;
            open_cell.halt(|| format!("a panic serving a guest stream's request {kind:?}"));
        }
        // Unanswered, its reply goes as its owing drops, nothing written.
    }
}

/// A connection's cell, as the backend of its guest's requests on its stream (`extent_requests`):
/// each its cell's call, as the connection's socket's requests of the same are.
impl Backend for Connection {
    fn cell(&self) -> u64 {
        Connection::cell(self).map_or(0, |open_cell| open_cell.cell().id)
    }

    /// A stream serves no window's requests, which its VMM's device does: no `PRIVATE`, nor
    /// `MAP_ZEROS`.
    fn copies_on_write(&self) -> bool {
        false
    }

    async fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &mut [VolatileSliceMut<'_>],
    ) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        let began = std::time::Instant::now();
        // cladefs: in its turn, as a fault's, as what it pages in holds memory (TASK-149.34).
        let _turn = self.page_ins.acquire().await.map_err(|_| Errno::EIO)?;
        open_cell.read(extent, offset, buffers).await?;
        let len = buffers.iter().map(|buffer| buffer.len() as u64).sum();
        self.traced(Kind::Read, began, open_cell, extent, offset, len);
        Ok(())
    }

    async fn write(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> Result<(), Errno> {
        // cladefs: within its disk and its tenant's limits, as its cell admits it (doc-22).
        self.serving()?.write_with_barriers(extent, offset, buffers, barriers).await
    }

    async fn free_many(&self, extents: &[ExtentId]) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        extents.iter().try_for_each(|extent| open_cell.free(*extent))
    }

    /// Refused before it is served ([`on_stream`]).
    async fn map_many(&self, _: &[WindowMapping], _: bool) -> Result<(), Errno> {
        Err(Errno::EINVAL)
    }

    /// Refused before it is served ([`on_stream`]).
    async fn unmap(&self, _: u64, _: u64) -> Result<(), Errno> {
        Err(Errno::EINVAL)
    }

    async fn commit(&self, root: Option<&[u8]>, commit: Commit) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        match commit {
            Commit::Uploaded => open_cell.commit(root).await,
            Commit::Local => open_cell.commit_local(root).await,
            Commit::Ordered => open_cell.commit_ordered(root).await,
        }
    }

    async fn root(&self) -> Result<Vec<u8>, Errno> {
        self.serving()?.root()
    }

    async fn info(&self) -> Result<ExtentInfo, Errno> {
        self.serving()?.info()
    }
}

/// What a guest's requests ask of the cell besides: refused before they are served
/// ([`on_stream`]), its VMM's device's.
impl GuestBackend for Connection {
    fn prefault(&self, _: u64, _: u64) {}

    async fn make_private(&self, _: u64, _: u64) -> Result<(), Errno> {
        Err(Errno::EINVAL)
    }

    async fn answer_stats(&self, _: u64, _: Option<&str>) -> Result<(), Errno> {
        Err(Errno::EINVAL)
    }

    async fn resized(&self, _: u64, _: bool) -> Result<(), Errno> {
        Err(Errno::EINVAL)
    }
}
