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
//! answered ([`StreamReply`]) as it is done: by the thread that served it, at once, without waiting,
//! while no one else writes, or by a thread of the stream's own, the rest ([`Writer`]), so that a
//! guest slow to read holds up no serving. A stream has [`STREAM_REQUESTS`] requests in flight at
//! most, and [`STREAM_BYTES`] of their bytes, its reading waiting for room past them.
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
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// The most requests a stream has in flight, from its reading to its reply's writing: its reading
/// waits for one to be done past them. A VMM's device's queue's worth (`QUEUE_SIZE`).
const STREAM_REQUESTS: usize = protocol::QUEUE_SIZE as usize;

/// The most bytes a stream's requests in flight hold, what the device reads and writes of each,
/// beside the tenant's bytes in flight that their serving holds too: its reading waits for room
/// past them, so that a guest holds no more of the daemon's memory however slowly it reads its
/// replies, or however long they wait for their turns. Room for one request whatever its size.
const STREAM_BYTES: u64 = 64 << 20;

/// The most streams a connection's guest has open at once: one connects past them is closed. Its
/// driver keeps one, connecting again once one resets; a guest connecting many holds no more of
/// the daemon's threads, descriptors and memory than these.
const MAX_STREAMS: usize = 4;

/// A connection's guest's stream: none yet; accepted on the VM process's listener, and the streams
/// it accepted, which a stop shuts down; or stopped as the connection ends, after which none
/// starts.
#[derive(Default)]
pub(super) enum GuestStream {
    #[default]
    Unstarted,
    Listening {
        listener: Arc<OwnedFd>,
        streams: Streams,
    },
    Stopped,
}

/// A connection's guest's streams open, which each one's reading removes as it ends.
type Streams = Arc<Mutex<Vec<Arc<UnixStream>>>>;

/// A guest's request a stream received, to serve in its connection's turns: the bytes the device
/// reads, the bytes it writes, and the reply it is owed.
pub(super) struct Received {
    ordered_only: bool,
    readable: Vec<u8>,
    writable: usize,
    owed: Owed,
}

/// The reply a stream's request is owed, to the request tagged `tag`, which holds `bytes` of its
/// stream's credit: sent once ([`Owed::send`]), or, dropped unsent, as one of nothing written,
/// which its guest takes for an `EIO`, so that none waits for good, whatever ends its serving: a
/// panic, the connection ending first, or no thread to serve it. Its credit is given back once its
/// reply is written ([`write_replies`]), or couldn't be.
struct Owed {
    tag: u64,
    bytes: u64,
    replies: Option<Replies>,
}

/// A reply to write: its head, the bytes the device wrote, the credit its request held, and how
/// much of it, head and bytes, is written already.
struct Reply {
    head: StreamReply,
    written: Vec<u8>,
    bytes: u64,
    skip: usize,
}

impl Reply {
    fn len(&self) -> usize {
        size_of::<StreamReply>() + self.written.len()
    }

    /// Writes what is left of it to `stream`, waiting for room.
    fn write_rest(&self, mut stream: &UnixStream) -> std::io::Result<()> {
        let head = self.head.as_bytes();
        stream.write_all(head.get(self.skip..).unwrap_or_default())?;
        stream.write_all(&self.written[self.skip.saturating_sub(head.len())..])
    }

    /// Writes what it can of it to `stream` now, without waiting (`MSG_DONTWAIT`): the bytes it
    /// wrote, 0 if the stream's socket is full.
    fn write_now(&self, stream: &UnixStream) -> std::io::Result<usize> {
        let iov = [
            libc::iovec {
                iov_base: self.head.as_bytes().as_ptr() as *mut _,
                iov_len: size_of::<StreamReply>(),
            },
            libc::iovec { iov_base: self.written.as_ptr() as *mut _, iov_len: self.written.len() },
        ];
        // SAFETY: Zeroes are no name, no control data, and no flags; its padding, on musl, private.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = iov.as_ptr() as *mut _;
        message.msg_iovlen = iov.len() as _;
        loop {
            let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
            // SAFETY: Sends the bytes `iov` describes, which `self` holds through the call.
            let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, flags) };
            if sent >= 0 {
                return Ok(sent as usize);
            }
            let err = std::io::Error::last_os_error();
            match err.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => return Ok(0),
                _ => return Err(err),
            }
        }
    }
}

/// What a stream's writer thread is told: a reply to write, or to write what a serving thread began
/// of one and the socket had no room for ([`Writing::partial`]).
enum Queued {
    Reply(Reply),
    Wake,
}

/// Who writes a stream's replies, one at a time, each whole, under its lock: a reply's serving
/// thread, at once, without waiting, while no one writes, rather than handing it to the stream's
/// writer thread, which costs a reply a wakeup (board 14663's A/B: one each fsync waits on); or
/// the writer thread, the rest.
struct Writer {
    stream: Arc<UnixStream>,
    writing: Mutex<Writing>,
}

/// What its writers share: a reply a serving thread began and the socket had no room for the rest
/// of, which the writer thread writes before any other; and whether the stream failed, after which
/// replies go unwritten, their credit given back.
#[derive(Default)]
struct Writing {
    partial: Option<Reply>,
    failed: bool,
}

impl Writer {
    /// Writes what is left of `reply`, waiting for room, with `writing` held, then gives back its
    /// credit: the stream shut down if it fails.
    fn write(&self, writing: &mut Writing, reply: Reply, credit: &Credit) {
        if !writing.failed
            && let Err(err) = reply.write_rest(&self.stream)
        {
            log::info!("cladefsd: replying on a guest's stream: {err}");
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
            writing.failed = true;
        }
        credit.give(reply.bytes);
    }
}

impl Owed {
    /// Sends the reply, `written` the bytes the device wrote.
    fn send(mut self, written: Vec<u8>) {
        let replies = self.replies.take().expect("a reply sent once");
        Self::reply(&replies, self.tag, self.bytes, written);
    }

    /// Writes the reply at once, if no one writes, or what of it the socket has room for, the rest
    /// the writer thread's; or hands it to the writer thread.
    fn reply(replies: &Replies, tag: u64, bytes: u64, written: Vec<u8>) {
        // At most MAX_WRITABLE, a u32's.
        let len = written.len() as u32;
        let head = StreamReply { tag: tag.to_le(), written: len.to_le(), reserved: 0 };
        let reply = Reply { head, written, bytes, skip: 0 };
        let writer = &replies.writer;
        if let Ok(mut writing) = writer.writing.try_lock() {
            if writing.failed {
                drop(writing);
                return replies.credit.give(bytes);
            }
            if writing.partial.is_none() {
                match reply.write_now(&writer.stream) {
                    Ok(wrote) if wrote == reply.len() => {
                        drop(writing);
                        return replies.credit.give(bytes);
                    }
                    Ok(wrote) => {
                        writing.partial = Some(Reply { skip: wrote, ..reply });
                        drop(writing);
                        // Its writer gone, the stream ended: the partial reply is no one's.
                        let _ = replies.sender.send(Queued::Wake);
                        return;
                    }
                    Err(err) => {
                        log::info!("cladefsd: replying on a guest's stream: {err}");
                        let _ = writer.stream.shutdown(std::net::Shutdown::Both);
                        writing.failed = true;
                        drop(writing);
                        return replies.credit.give(bytes);
                    }
                }
            }
        }
        // Its writer gone, the stream ended: no guest to tell, its credit given back here.
        if let Err(mpsc::SendError(Queued::Reply(reply))) =
            replies.sender.send(Queued::Reply(reply))
        {
            replies.credit.give(reply.bytes);
        }
    }
}

impl Drop for Owed {
    fn drop(&mut self) {
        if let Some(replies) = self.replies.take() {
            Self::reply(&replies, self.tag, self.bytes, Vec::new());
        }
    }
}

/// Where a stream's replies go, and its credit, which each reply gives back as it is written.
#[derive(Clone)]
struct Replies {
    sender: mpsc::Sender<Queued>,
    credit: Arc<Credit>,
    writer: Arc<Writer>,
}

/// A stream's requests in flight and their bytes, [`STREAM_REQUESTS`] and [`STREAM_BYTES`] at most,
/// what its reading waits on; and its connection's count of every stream's requests in flight,
/// which its VM process's own bound leaves out ([`Connection::stream_requests`]).
struct Credit {
    held: Mutex<(usize, u64)>,
    done: Condvar,
    connection: Arc<AtomicUsize>,
}

impl Credit {
    fn new(connection: Arc<AtomicUsize>) -> Self {
        Self { held: Mutex::new((0, 0)), done: Condvar::new(), connection }
    }

    /// Waits for room for a request more, of `bytes`, and counts it: room for one whatever its
    /// size, with none held.
    fn take(&self, bytes: u64) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        while held.0 >= STREAM_REQUESTS || (held.0 > 0 && held.1 + bytes > STREAM_BYTES) {
            held = self.done.wait(held).unwrap_or_else(PoisonError::into_inner);
        }
        *held = (held.0 + 1, held.1 + bytes);
        self.connection.fetch_add(1, Ordering::Relaxed);
    }

    /// A request of `bytes` is done.
    fn give(&self, bytes: u64) {
        {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            *held = (held.0 - 1, held.1 - bytes);
        }
        self.connection.fetch_sub(1, Ordering::Relaxed);
        self.done.notify_one();
    }
}

/// A stream its reading holds: shut down and forgotten as the reading ends, however, a panic
/// serving a request on its thread included, so that no guest waits on a stream no one reads.
struct Reading {
    stream: Arc<UnixStream>,
    streams: Streams,
}

impl Drop for Reading {
    /// Forgotten first, then shut down: a guest that sees its stream end may open another.
    fn drop(&mut self) {
        let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        streams.retain(|open| !Arc::ptr_eq(open, &self.stream));
        drop(streams);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
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
    fn accept_streams(self: Arc<Self>, listener: &OwnedFd, streams: &Streams, ordered_only: bool) {
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
                match err.raw_os_error() {
                    // The connection that came went first, or none came: the next.
                    Some(libc::EINTR | libc::EAGAIN | libc::ECONNABORTED | libc::EPROTO) => {
                        continue;
                    }
                    // Shut down by the connection's stop, or failing, as of no descriptors left:
                    // shut down, its guest's next connections refused rather than waiting for an
                    // accept that won't come.
                    _ => {
                        // As its connection's stop shuts it down, the end it should come to.
                        let stopped = matches!(
                            *self.guest_stream.lock().unwrap_or_else(PoisonError::into_inner),
                            GuestStream::Stopped
                        );
                        match stopped {
                            true => log::info!("cladefsd: no more guest streams accepted: {err}"),
                            false => log::warn!("cladefsd: no more guest streams accepted: {err}"),
                        }
                        // SAFETY: Shuts down a listener this holds.
                        unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) };
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
                let mut open = streams.lock().unwrap_or_else(PoisonError::into_inner);
                // Recorded under the lock the stop takes, or shut down here, the stop past.
                if !matches!(
                    *self.guest_stream.lock().unwrap_or_else(PoisonError::into_inner),
                    GuestStream::Listening { .. }
                ) {
                    return;
                }
                if open.len() >= MAX_STREAMS {
                    log::warn!(
                        "cladefsd: a guest's stream past the {MAX_STREAMS} it may have open: closed"
                    );
                    continue;
                }
                open.push(stream.clone());
            }
            let reading = Reading { stream, streams: streams.clone() };
            if let Err(err) = self.clone().serve_stream(reading, ordered_only) {
                log::error!("cladefsd: serving a guest's stream: {err}");
            }
        }
    }

    /// Serves the stream `reading` holds, read on a thread of its own and answered on another:
    /// shut down, and forgotten, if either can't start.
    fn serve_stream(self: Arc<Self>, reading: Reading, ordered_only: bool) -> std::io::Result<()> {
        let (sender, queued) = mpsc::channel::<Queued>();
        let credit = Arc::new(Credit::new(self.stream_requests.clone()));
        let writer = Arc::new(Writer { stream: reading.stream.clone(), writing: Mutex::default() });
        let (writing, giving) = (writer.clone(), credit.clone());
        std::thread::Builder::new()
            .name("cladefsd stream replies".into())
            .spawn(move || write_replies(&writing, &queued, &giving))?;
        let replies = Replies { sender, credit, writer };
        std::thread::Builder::new()
            .name("cladefsd stream requests".into())
            .spawn(move || self.read_requests(&reading, &replies, ordered_only))?;
        Ok(())
    }

    /// Reads the requests of the stream `reading` holds, admitting each in the connection's turns,
    /// until it ends or breaks the protocol's bounds; then it is shut down ([`Reading`]).
    fn read_requests(self: Arc<Self>, reading: &Reading, replies: &Replies, ordered_only: bool) {
        let stream = &*reading.stream;
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
                return;
            }
            // Its room had before its bytes are read, which it holds until its reply is written.
            let held = (readable + writable) as u64;
            replies.credit.take(held);
            let owed = Owed { tag, bytes: held, replies: Some(replies.clone()) };
            let mut bytes = vec![0u8; readable];
            if let Err(err) = reading.read_exact(&mut bytes) {
                log::info!("cladefsd: a guest's stream ended within a request: {err}");
                return;
            }
            let received = Received { ordered_only, readable: bytes, writable, owed };
            if let Some(work) = self.admit(Work::Stream(received)) {
                // Its first poll here: a panic in it, which halted the cell and answered it
                // (`StreamAnswer`), ends no more than it, as a task's ends no more than the task,
                // the stream read on, its requests refused by the halted cell.
                let serving = std::panic::AssertUnwindSafe(|| self.clone().serve_in_turn(work));
                if std::panic::catch_unwind(serving).is_err() {
                    log::error!("cladefsd: a panic serving a guest stream's request");
                }
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
        if let GuestStream::Listening { listener, streams } = stopped {
            // SAFETY: Shuts down a listener this holds: its accepting thread woken, which ends.
            unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) };
            for stream in streams.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        // Then its streams' requests read from now on admitted none ([`Connection::admit`]):
        // refused once their streams are shut down, none's reply of nothing written reaches its
        // guest, which sends it again as it connects again.
        self.requests.with(Mutex::lock).streams_stopped = true;
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

    /// Whether `extent`, an extent of its own cell's, holds the bytes of `buffers` at `offset`:
    /// written there already, as a write sent again finds it (doc-4's stream transport). Only its
    /// cell's own: another's extent, an ancestor's or a public base's, which a write is refused
    /// for whatever it holds, is never taken for written. The bytes it reads are held in its
    /// tenant's bytes in flight, and read in its turn at page-ins, as a `READ`'s are; compared with
    /// `buffers` a piece at a time.
    async fn holds(
        &self,
        open_cell: &OpenCell,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
    ) -> bool {
        if extent.cell != open_cell.cell().id {
            return false;
        }
        let len = buffers.iter().map(VolatileSlice::len).sum::<usize>();
        if self.take_in_flight(len as u64).await.is_err() {
            return false;
        }
        let _held =
            InFlight { connection: self.this.upgrade().expect("serving"), bytes: len as u64 };
        let Ok(_turn) = self.page_ins.acquire().await else { return false };
        let mut held = vec![0u8; len];
        let read =
            open_cell.read(extent, offset, &mut [VolatileSliceMut::from(&mut held[..])]).await;
        if read.is_err() {
            return false;
        }
        const PIECE: usize = 64 << 10;
        let mut wrote = vec![0u8; PIECE.min(len)];
        held.chunks(PIECE).enumerate().all(|(i, piece)| {
            let wrote = &mut wrote[..piece.len()];
            cladefs_core::memory::gather(buffers, i * PIECE, wrote);
            wrote == piece
        })
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

/// Writes a stream's replies as they come, each whole, giving back its request's credit once
/// written, until every sender is gone. A stream that fails is shut down, its reading with it, and
/// the replies after it go unwritten, their credit given back, so that a reading waiting for room
/// reads on, to the stream's end.
fn write_replies(writer: &Writer, queued: &mpsc::Receiver<Queued>, credit: &Credit) {
    for item in queued {
        let mut writing = writer.writing.lock().unwrap_or_else(PoisonError::into_inner);
        // A reply a serving thread began goes first, whole, before any other.
        if let Some(partial) = writing.partial.take() {
            writer.write(&mut writing, partial, credit);
        }
        if let Queued::Reply(reply) = item {
            writer.write(&mut writing, reply, credit);
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

    /// A write again of the bytes a sealed extent holds at its offset, which a write sent again
    /// after its stream's connection was lost is, its first send having written and sealed it
    /// (with `WRITE_COMMIT`, or another sent again with `WRITE_BARRIER` before it), succeeds, as
    /// it changes nothing: doc-4's stream transport. Its commit, if it has one, is made then.
    async fn write(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        // cladefs: within its disk and its tenant's limits, as its cell admits it (doc-22).
        match open_cell.write_with_barriers(extent, offset, buffers, barriers).await {
            Err(Errno::EPERM) if self.holds(open_cell, extent, offset, buffers).await => {}
            done => return done,
        }
        match barriers.after {
            None => Ok(()),
            Some(Commit::Uploaded) => open_cell.commit(barriers.root).await,
            Some(Commit::Local) => open_cell.commit_local(barriers.root).await,
            Some(Commit::Ordered) => open_cell.commit_ordered(barriers.root).await,
        }
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
