// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The network thread: one per machine with a network, running its
//! [`Stack`] against real host sockets.
//!
//! It waits in epoll on the frames the virtio-net device hands it (the
//! link's eventfd), on every host socket it relays (edge-triggered), on
//! DNS answers from lookup threads, on the decider thread's answers, and on
//! the stack's next timer (smoltcp's, or a decider's deadline). The
//! device thread only moves frames between the virtqueues and the link,
//! so a busy network never delays the other devices' queues, and nothing
//! here depends on a vCPU running: the network keeps working while another
//! thread is in `Machine::run`, or none is.
//!
//! The thread can be stopped and its stack saved ([`NetThread::save`]): the
//! host sockets come out beside the state, and a thread started from the
//! state ([`NetThread::spawn`] with a [`Restore`]) takes them back. It can
//! also be stopped with its stack kept whole ([`NetThread::park`]), to run
//! on as it was or be saved: a suspension looks at it first, and holds the
//! host sockets of the saved stack meanwhile ([`HeldStreams`]).

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use smoltcp::time::Instant;
use socket2::{Domain, SockRef, Socket, Type};
use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use super::Shared;
use super::decide::{ConnectionRequest, Decider};
use super::policy::Verdict;
use super::stack::{HostNet, HostStream, LookupError, Options, Ready, Stack, StackState, WakeOn};
use crate::error::{Error, Result};

/// The most frames waiting in each direction of the link; past it, the
/// side that fills it waits for the other to drain it.
pub(crate) const LINK_FRAMES: usize = 256;

const STOP: u64 = 0;
const WAKE: u64 = 1;
const LOOKUP: u64 = 2;
const DECIDE: u64 = 3;
/// Host connection `id` has the epoll token `CONN_BASE + id`.
const CONN_BASE: u64 = 16;

fn io_err(op: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::Hypervisor { op, source }
}

/// The frames between the virtio-net device and the network thread.
#[derive(Debug, Default)]
pub(crate) struct Frames {
    pub(crate) to_stack: VecDeque<Vec<u8>>,
    pub(crate) to_guest: VecDeque<Vec<u8>>,
    /// The device left guest frames on its tx queue: `to_stack` was full.
    pub(crate) device_waiting: bool,
    /// The stack holds frames for the guest: `to_guest` was full.
    pub(crate) stack_waiting: bool,
}

/// What the device and the network thread share.
#[derive(Debug)]
pub(crate) struct Link {
    frames: Mutex<Frames>,
    /// Signalled when the network thread has something to do.
    stack_wake: EventFd,
    /// The device's wake eventfd, signalled when it has something to do.
    device_wake: EventFd,
}

impl Link {
    pub(crate) fn new() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            frames: Mutex::default(),
            stack_wake: EventFd::new(EFD_NONBLOCK)
                .map_err(io_err("create the network's eventfd"))?,
            device_wake: EventFd::new(EFD_NONBLOCK)
                .map_err(io_err("create the virtio-net device's eventfd"))?,
        }))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Frames> {
        self.frames.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn wake_stack(&self) {
        // Fails only if the counter would overflow: a wakeup is pending.
        let _ = self.stack_wake.write(1);
    }

    pub(crate) fn wake_device(&self) {
        let _ = self.device_wake.write(1);
    }

    pub(crate) fn device_wake(&self) -> &EventFd {
        &self.device_wake
    }
}

/// A DNS answer from a lookup thread.
type Lookup = (u64, std::result::Result<Vec<Ipv4Addr>, LookupError>);

/// The decider's answer about a connection.
pub(crate) type Decided = (u64, Option<Verdict>);

/// A question for the decider thread.
struct Question {
    id: u64,
    request: ConnectionRequest,
    decider: Decider,
    /// Past it, the stack has applied the default: not worth asking.
    deadline: std::time::Instant,
}

/// The thread that asks the caller's deciders, one question at a time
/// (see [`super::decide`]). It is never joined: a callback may be stuck
/// for good. It ends when its question channel closes (the network thread
/// is gone) and the callback it is in, if any, returns.
pub(crate) struct DeciderThread {
    questions: Sender<Question>,
}

impl DeciderThread {
    /// Starts the thread; answers go to `answers`, and `wake` is signalled
    /// after each.
    pub(crate) fn spawn(answers: Sender<Decided>, wake: Arc<EventFd>) -> io::Result<Self> {
        let (questions, rx) = mpsc::channel::<Question>();
        std::thread::Builder::new().name("libmachine-net-decide".into()).spawn(move || {
            for q in rx {
                if std::time::Instant::now() >= q.deadline {
                    continue;
                }
                let answer = q.decider.call(&q.request);
                if answers.send((q.id, answer)).is_err() {
                    return;
                }
                let _ = wake.write(1);
            }
        })?;
        Ok(Self { questions })
    }

    /// Asks `decider` about `request` once the questions before it are
    /// answered, unless `timeout` has passed by then.
    pub(crate) fn ask(
        &self,
        id: u64,
        request: ConnectionRequest,
        decider: Decider,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        self.questions.send(Question { id, request, decider, deadline }).is_ok()
    }
}

/// Real host sockets: non-blocking connections registered with the network
/// thread's epoll, names resolved by the host's resolver on threads of
/// their own, and the decider asked on its thread. Without an epoll or a
/// lookup or decision channel (unit tests), sockets are not registered and
/// lookups and decisions never answer.
#[derive(Default)]
pub(crate) struct RealStreams {
    epoll: Option<Epoll>,
    lookups: Option<(Sender<Lookup>, Arc<EventFd>)>,
    decisions: Option<(Sender<Decided>, Arc<EventFd>)>,
    /// Started at the first question.
    decider: Option<DeciderThread>,
}

impl HostStream for TcpStream {
    fn connect_result(&self) -> io::Result<()> {
        if let Some(e) = self.take_error()? {
            return Err(e);
        }
        self.peer_addr().map(|_| ())
    }

    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }

    fn reset_on_drop(&self) {
        let _ = SockRef::from(self).set_linger(Some(std::time::Duration::ZERO));
    }
}

impl HostNet for RealStreams {
    type Stream = TcpStream;

    fn connect(&mut self, id: u64, to: SocketAddrV4) -> io::Result<TcpStream> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
        socket.set_nonblocking(true)?;
        // The guest's own stack decides how to batch its writes.
        socket.set_tcp_nodelay(true)?;
        match socket.connect(&to.into()) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(e) => return Err(e),
        }
        let stream = TcpStream::from(socket);
        self.watch(id, &stream)?;
        Ok(stream)
    }

    fn adopt(&mut self, id: u64, stream: &TcpStream) -> io::Result<()> {
        // Edge-triggered, as for a new connection: adding a socket that is
        // already readable or writable reports it at once, so nothing that
        // happened while the stack was saved is missed.
        self.watch(id, stream)
    }

    fn resolve(&mut self, id: u64, name: String) {
        let Some((tx, wake)) = &self.lookups else {
            return;
        };
        let (thread_tx, thread_wake) = (tx.clone(), wake.clone());
        let spawned = std::thread::Builder::new().name("libmachine-dns".into()).spawn(move || {
            let result = lookup(&name);
            // The machine may be gone; then nobody is waiting.
            if thread_tx.send((id, result)).is_ok() {
                let _ = thread_wake.write(1);
            }
        });
        if spawned.is_err() {
            let _ = tx.send((id, Err(LookupError::Failed)));
            let _ = wake.write(1);
        }
    }

    fn decide(&mut self, id: u64, request: ConnectionRequest, decider: Decider) {
        let Some((tx, wake)) = &self.decisions else {
            return;
        };
        if self.decider.is_none() {
            self.decider = DeciderThread::spawn(tx.clone(), wake.clone()).ok();
        }
        let timeout = decider.timeout();
        let asked = self.decider.as_ref().is_some_and(|t| t.ask(id, request, decider, timeout));
        if !asked {
            // No thread to ask on: the decider failed, and its default
            // applies.
            self.decider = None;
            let _ = tx.send((id, None));
            let _ = wake.write(1);
        }
    }
}

impl RealStreams {
    /// Watches host connection `id` in the network thread's epoll.
    fn watch(&self, id: u64, stream: &TcpStream) -> io::Result<()> {
        if let Some(epoll) = &self.epoll {
            let events =
                EventSet::IN | EventSet::OUT | EventSet::READ_HANG_UP | EventSet::EDGE_TRIGGERED;
            epoll.ctl(
                ControlOperation::Add,
                stream.as_raw_fd(),
                EpollEvent::new(events, CONN_BASE + id),
            )?;
        }
        Ok(())
    }
}

/// The IPv4 addresses the host's resolver gives for `name`.
fn lookup(name: &str) -> std::result::Result<Vec<Ipv4Addr>, LookupError> {
    let Ok(cname) = CString::new(name) else {
        return Err(LookupError::NoName);
    };
    // SAFETY: an all-zero addrinfo is a valid "no hints" value.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_family = libc::AF_INET;
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut list: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: the name is NUL-terminated, the hints initialized, and the
    // list is freed below.
    let rc = unsafe { libc::getaddrinfo(cname.as_ptr(), std::ptr::null(), &hints, &mut list) };
    match rc {
        0 => {}
        // The name exists, without an IPv4 address.
        libc::EAI_NODATA => return Ok(Vec::new()),
        libc::EAI_NONAME => return Err(LookupError::NoName),
        _ => return Err(LookupError::Failed),
    }
    let mut addrs = Vec::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: getaddrinfo returned a valid list, not yet freed.
        let ai = unsafe { &*entry };
        if ai.ai_family == libc::AF_INET && !ai.ai_addr.is_null() {
            // SAFETY: an AF_INET entry's address is a sockaddr_in.
            let sin = unsafe { &*(ai.ai_addr as *const libc::sockaddr_in) };
            let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        entry = ai.ai_next;
    }
    // SAFETY: the list came from getaddrinfo and is freed once.
    unsafe { libc::freeaddrinfo(list) };
    Ok(addrs)
}

/// A random seed for smoltcp's TCP sequence numbers and ports.
fn random_seed() -> u64 {
    let mut seed = [0u8; 8];
    // SAFETY: the kernel writes at most 8 bytes to `seed`. If it fails
    // (it cannot on a booted Linux), the seed is the time.
    let n = unsafe { libc::getrandom(seed.as_mut_ptr().cast(), seed.len(), 0) };
    if n == 8 {
        u64::from_ne_bytes(seed)
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    }
}

/// A saved stack to start a network thread from, and the host streams
/// handed back to it.
pub(crate) struct Restore {
    pub(crate) state: StackState,
    pub(crate) streams: HashMap<u64, TcpStream>,
}

/// The running network thread; dropping it stops and joins the thread,
/// closing every host connection.
pub(crate) struct NetThread {
    /// None once [parked](NetThread::park).
    stop: Option<EventFd>,
    handle: Option<JoinHandle<(Stack<RealStreams>, Wakes)>>,
    link: Arc<Link>,
    /// The stack's clock counts from here.
    start: std::time::Instant,
}

/// A network thread stopped with its stack kept whole, host side and all
/// (lookups and decider questions in flight still answer to it): to go on
/// as it was ([`resume`](Parked::resume)) or to be saved.
pub(crate) struct Parked {
    stack: Stack<RealStreams>,
    wakes: Wakes,
    stop: EventFd,
    link: Arc<Link>,
    start: std::time::Instant,
}

impl NetThread {
    /// Starts the thread, with a new stack, or the one `restore` gives.
    pub(crate) fn spawn(
        link: Arc<Link>,
        shared: Arc<Shared>,
        restore: Option<Restore>,
    ) -> Result<Self> {
        let epoll = Epoll::new().map_err(io_err("create the network thread's epoll"))?;
        let stop =
            EventFd::new(EFD_NONBLOCK).map_err(io_err("create the network's stop eventfd"))?;
        let lookup_wake = Arc::new(
            EventFd::new(EFD_NONBLOCK).map_err(io_err("create the network's lookup eventfd"))?,
        );
        let decide_wake = Arc::new(
            EventFd::new(EFD_NONBLOCK).map_err(io_err("create the network's decider eventfd"))?,
        );
        for (fd, token) in [
            (stop.as_raw_fd(), STOP),
            (link.stack_wake.as_raw_fd(), WAKE),
            (lookup_wake.as_raw_fd(), LOOKUP),
            (decide_wake.as_raw_fd(), DECIDE),
        ] {
            epoll
                .ctl(ControlOperation::Add, fd, EpollEvent::new(EventSet::IN, token))
                .map_err(io_err("watch the network's eventfds"))?;
        }
        let (tx, rx) = mpsc::channel();
        let (decided_tx, decided_rx) = mpsc::channel();
        let host = RealStreams {
            epoll: Some(epoll),
            lookups: Some((tx, lookup_wake.clone())),
            decisions: Some((decided_tx, decide_wake.clone())),
            decider: None,
        };
        let wakes = Wakes { lookups: rx, lookup_wake, decisions: decided_rx, decide_wake };
        let start = std::time::Instant::now();
        let now = Instant::from_micros(0);
        let stack = match restore {
            None => Stack::new(shared, host, random_seed(), Options::default(), now),
            // A new seed: a clone's new connections differ from its
            // template's.
            Some(r) => Stack::restore(
                shared,
                host,
                random_seed(),
                Options::default(),
                now,
                r.state,
                r.streams,
            )?,
        };
        Self::start(Parked { stack, wakes, stop, link, start })
    }

    /// Runs `parked`'s stack on a new thread.
    fn start(parked: Parked) -> Result<Self> {
        let Parked { stack, wakes, stop, link, start } = parked;
        // Frames the stack holds for the guest, or answers it owes, go out
        // at the thread's first round.
        link.wake_stack();
        let thread_link = link.clone();
        let handle = std::thread::Builder::new()
            .name("libmachine-net".into())
            .spawn(move || {
                let stack = run(&thread_link, stack, start, &wakes);
                (stack, wakes)
            })
            .map_err(io_err("start the network thread"))?;
        Ok(Self { stop: Some(stop), handle: Some(handle), link, start })
    }

    /// Stops the thread, keeping its stack as it is.
    pub(crate) fn park(mut self) -> Result<Parked> {
        let stop = self.stop.take().expect("taken only here");
        let _ = stop.write(1);
        let handle = self.handle.take().expect("joined only here and on drop");
        let (stack, wakes) = handle
            .join()
            .map_err(|_| crate::error::Error::Snapshot("the network thread panicked".into()))?;
        Ok(Parked { stack, wakes, stop, link: self.link.clone(), start: self.start })
    }

    /// Stops the thread and saves its stack: the state, and the host
    /// streams of its connections by ID.
    pub(crate) fn save(self) -> Result<(StackState, HashMap<u64, TcpStream>)> {
        Ok(self.park()?.save())
    }
}

impl Parked {
    pub(crate) fn stack_mut(&mut self) -> &mut Stack<RealStreams> {
        &mut self.stack
    }

    /// Runs the stack on again, on a new thread, from where it stopped.
    pub(crate) fn resume(self) -> Result<NetThread> {
        // The request that stopped the thread before.
        let _ = self.stop.read();
        NetThread::start(self)
    }

    /// Saves the stack: its state, and the host streams of its connections
    /// by ID. Its host side goes (its epoll, its decider thread).
    pub(crate) fn save(self) -> (StackState, HashMap<u64, TcpStream>) {
        let (state, streams, _host) = self.stack.save();
        (state, streams)
    }
}

impl Drop for NetThread {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            let _ = stop.write(1);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The host streams of a suspended machine's guest connections, held while
/// no network thread runs ([`crate::suspend`]), with what each must be
/// watched for: what would make the stack act for the guest. Nothing reads
/// or writes them here: a watcher gets their file descriptors, and only a
/// restored stack gets the streams back ([`into_streams`]), refused if one
/// lost bytes meanwhile ([`check_unread`]).
///
/// [`into_streams`]: HeldStreams::into_streams
/// [`check_unread`]: HeldStreams::check_unread
pub(crate) struct HeldStreams {
    held: Vec<Held>,
}

struct Held {
    id: u64,
    stream: TcpStream,
    wake_on: WakeOn,
    /// The bytes waiting to be read when it was handed over (SIOCINQ),
    /// if the kernel said.
    unread: Option<u32>,
}

/// The place a network device puts its host streams in when it hands them
/// over, for its machine to take ([`crate::Machine::suspend`]).
pub(crate) type NetSlot = Arc<Mutex<Option<HeldStreams>>>;

impl HeldStreams {
    /// Holds `streams`, each to be watched as `watch` says (not at all if
    /// it is not there).
    pub(crate) fn new(streams: HashMap<u64, TcpStream>, watch: &[(u64, WakeOn)]) -> Self {
        let mut held: Vec<Held> = streams
            .into_iter()
            .map(|(id, stream)| Held {
                id,
                wake_on: watch
                    .iter()
                    .find(|(w, _)| *w == id)
                    .map_or_else(WakeOn::default, |&(_, w)| w),
                unread: unread(&stream),
                stream,
            })
            .collect();
        held.sort_by_key(|h| h.id);
        Self { held }
    }

    /// How many streams it holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }

    /// The file descriptor of each stream to watch, and what for.
    pub(crate) fn watched(&self) -> Vec<(std::os::fd::RawFd, WakeOn)> {
        self.held
            .iter()
            .filter(|h| h.wake_on.data || h.wake_on.room)
            .map(|h| (h.stream.as_raw_fd(), h.wake_on))
            .collect()
    }

    /// Checks that no stream lost bytes it had waiting since it was handed
    /// over: bytes only arrive while nothing reads, so fewer waiting means
    /// something read them, and the restored guest would never see them.
    /// A socket the peer reset is not checked (the kernel may drop what it
    /// had waiting).
    pub(crate) fn check_unread(&self) -> Result<()> {
        for h in &self.held {
            let (Some(then), Some(now)) = (h.unread, unread(&h.stream)) else {
                continue;
            };
            if now < then && !reset(&h.stream) {
                return Err(Error::Snapshot(format!(
                    "the host socket of guest connection {} had {then} bytes waiting when the \
                     machine was suspended and has {now} now: something read it, so the \
                     restored guest would miss bytes; refused",
                    h.id
                )));
            }
        }
        Ok(())
    }

    /// The streams, by connection ID, for a restored stack.
    pub(crate) fn into_streams(self) -> HashMap<u64, TcpStream> {
        self.held.into_iter().map(|h| (h.id, h.stream)).collect()
    }

    /// Stream `id`, for tests that tamper with it.
    #[cfg(test)]
    pub(crate) fn stream(&self, id: u64) -> Option<&TcpStream> {
        self.held.iter().find(|h| h.id == id).map(|h| &h.stream)
    }
}

/// The bytes waiting to be read on `stream` (SIOCINQ), without reading.
fn unread(stream: &TcpStream) -> Option<u32> {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int, to `n`.
    let rc = unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut n) };
    (rc == 0).then(|| u32::try_from(n).unwrap_or(0))
}

/// Whether the connection is closed in the kernel (TCP_CLOSE: reset, or
/// closed both ways), from TCP_INFO, which changes nothing (unlike
/// SO_ERROR, which takes the error away from the stack that will need it).
fn reset(stream: &TcpStream) -> bool {
    const TCP_CLOSE: u8 = 7;
    // SAFETY: an all-zero tcp_info is valid, and getsockopt fills at most
    // `len` bytes of it.
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
    // SAFETY: as above, on an open socket.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            (&raw mut info).cast(),
            &mut len,
        )
    };
    rc == 0 && info.tcpi_state == TCP_CLOSE
}

/// The answers that come back to the network thread from other threads.
struct Wakes {
    lookups: Receiver<Lookup>,
    lookup_wake: Arc<EventFd>,
    decisions: Receiver<Decided>,
    decide_wake: Arc<EventFd>,
}

/// Runs `stack` until told to stop, then hands it back, its clock at the
/// time it stopped.
fn run(
    link: &Link,
    mut stack: Stack<RealStreams>,
    start: std::time::Instant,
    wakes: &Wakes,
) -> Stack<RealStreams> {
    let now = || Instant::from_micros(start.elapsed().as_micros() as i64);
    serve(link, &mut stack, &now, wakes);
    stack.set_now(now());
    stack
}

fn serve(link: &Link, stack: &mut Stack<RealStreams>, now: &dyn Fn() -> Instant, wakes: &Wakes) {
    let mut events = vec![EpollEvent::default(); 64];
    loop {
        // While frames wait for room on the link, nothing can go out, and
        // the device's wakeup (or the host's) is what to wait for:
        // smoltcp's timer would only spin. A decider's deadline still
        // counts.
        let delay =
            if stack.has_frames() { stack.decision_delay(now()) } else { stack.poll_delay(now()) };
        let timeout = delay
            .map_or(-1, |d| i32::try_from(d.total_micros().div_ceil(1000)).unwrap_or(i32::MAX));
        let Some(epoll) = stack.host().epoll.as_ref() else {
            return;
        };
        let n = match epoll.wait(timeout, &mut events) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => 0,
            Err(_) => return,
        };
        stack.set_now(now());
        for event in &events[..n] {
            match event.data() {
                STOP => return,
                WAKE => {
                    let _ = link.stack_wake.read();
                }
                LOOKUP => {
                    let _ = wakes.lookup_wake.read();
                    while let Ok((id, result)) = wakes.lookups.try_recv() {
                        stack.resolved(id, result);
                    }
                }
                DECIDE => {
                    let _ = wakes.decide_wake.read();
                    while let Ok((id, answer)) = wakes.decisions.try_recv() {
                        stack.decided(id, answer);
                    }
                }
                token => {
                    let set = event.event_set();
                    stack.host_event(
                        token - CONN_BASE,
                        Ready {
                            readable: set.contains(EventSet::IN),
                            writable: set.contains(EventSet::OUT),
                            closed: set.intersects(
                                EventSet::ERROR | EventSet::HANG_UP | EventSet::READ_HANG_UP,
                            ),
                        },
                    );
                }
            }
        }

        let (frames, wake_device) = {
            let mut f = link.lock();
            (std::mem::take(&mut f.to_stack), std::mem::take(&mut f.device_waiting))
        };
        if wake_device {
            // The device left guest frames on its queue; there is room now.
            link.wake_device();
        }
        for frame in frames {
            stack.guest_frame(frame);
        }
        stack.poll(now());

        if stack.has_frames() {
            let mut f = link.lock();
            let mut moved = false;
            while f.to_guest.len() < LINK_FRAMES {
                let Some(frame) = stack.take_frame() else {
                    break;
                };
                f.to_guest.push_back(frame);
                moved = true;
            }
            f.stack_waiting = stack.has_frames();
            drop(f);
            if moved {
                link.wake_device();
            }
            // Room the move made lets smoltcp send more now.
            if !stack.has_frames() {
                stack.poll(now());
                if stack.has_frames() {
                    link.wake_stack();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The thread signals the eventfd just after sending the answer (so
    /// that the network thread, woken, finds it): wait for the signal.
    fn wait_for_wake(wake: &EventFd) {
        let start = std::time::Instant::now();
        while wake.read().is_err() {
            assert!(start.elapsed() < Duration::from_secs(10), "no wakeup");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn request(port: u16) -> ConnectionRequest {
        ConnectionRequest { guest_port: 1, addr: Ipv4Addr::LOCALHOST, port, names: vec![], rule: 0 }
    }

    /// The decider thread survives a panicking callback (answered None),
    /// and skips questions whose time ran out while a stuck callback held
    /// it, then goes on with the next.
    #[test]
    fn the_decider_thread_survives_panics_and_stuck_callbacks() {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let thread = DeciderThread::spawn(tx, wake.clone()).unwrap();
        let (release, stuck) = mpsc::channel::<()>();
        let stuck = Mutex::new(stuck);
        let decider = Decider::new(move |r| match r.port {
            1 => panic!("a failing decider"),
            2 => {
                // Stuck until the test releases it.
                let _ = stuck.lock().unwrap().recv();
                Some(Verdict::Allow)
            }
            _ => Some(Verdict::Deny),
        });
        let long = Duration::from_secs(30);
        assert!(thread.ask(1, request(1), decider.clone(), long));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (1, None));
        wait_for_wake(&wake);
        assert!(thread.ask(2, request(2), decider.clone(), long));
        // Asked while the callback is stuck: its time runs out first.
        assert!(thread.ask(3, request(3), decider.clone(), Duration::from_millis(1)));
        assert!(thread.ask(4, request(4), decider, long));
        std::thread::sleep(Duration::from_millis(20));
        assert!(rx.try_recv().is_err(), "nothing while the callback is stuck");
        release.send(()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (2, Some(Verdict::Allow)));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            (4, Some(Verdict::Deny)),
            "question 3 was not asked"
        );
    }

    /// RealStreams asks on the decider thread and the answer comes back
    /// through its channel, with the eventfd signalled.
    #[test]
    fn real_streams_ask_on_the_decider_thread() {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let mut host = RealStreams { decisions: Some((tx, wake.clone())), ..Default::default() };
        let main = std::thread::current().id();
        let decider = Decider::new(move |_| {
            assert_ne!(std::thread::current().id(), main, "not on the caller's thread");
            Some(Verdict::Allow)
        });
        host.decide(7, request(80), decider);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (7, Some(Verdict::Allow)));
        wait_for_wake(&wake);
    }
}
