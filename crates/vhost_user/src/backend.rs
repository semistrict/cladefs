// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The back-end of a vhost-user channel: the front-end's messages served as the specification has
//! them, on a thread of the channel's own ([`Channel`]), which also hears the rings' kicks, takes
//! their chains for the [`Device`] to serve, and writes the guest's calls.
//!
//! One thread a channel, of one tenant's VM: what the front-end makes wait (a call written to an
//! eventfd whose count it filled, a message it sends half of) waits on its own thread alone,
//! never on one other tenants' requests are served on. Its socket and kicks are read without ever
//! waiting; it waits only in `epoll_wait`, and, the front-end racing it, in a call's write, which
//! [`Channel::stop`] interrupts.
//!
//! A message the front-end frames as no request is ends the channel; one framed well whose values
//! the back-end refuses is answered so if it asked for a reply (`REPLY_ACK`), and ends the channel
//! otherwise, as the front-end would go on not knowing. A guest's ring that means nothing stops
//! that queue alone, its error notifier signalled.
//!
//! `GET_VRING_BASE` and `RESET_DEVICE` wait for the chains taken off the queues they stop: their
//! reply deferred, the thread goes on returning chains and writing calls, reading no message
//! until it is sent, as the front-end sends none before. So nothing a chain's serving waits for
//! waits on the thread (the later milestones' back-end requests among it, doc-32 section 7).
//!
//! The device tells the front-end of its configuration's changes from any thread
//! ([`Notifier`]); the channel's thread says each on the back-end channel, never waiting
//! ([`crate::backend_channel`], cladefs board 14603).

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::thread::JoinHandleExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use zerocopy::IntoBytes;

use crate::backend_channel::BackendChannel;
use crate::eventfd::{Call, Kick, Wake};
use crate::memory::MemoryTable;
use crate::protocol::*;
use crate::queue::{Broken, Chain, RingMaps, Run, Shared};
use crate::socket::{ReceiveError, Socket};

/// The device a channel's back-end serves.
pub trait Device: Send + Sync + 'static {
    /// The virtio features it offers, its transport's among them (`VIRTIO_F_VERSION_1`,
    /// `VIRTIO_RING_F_EVENT_IDX`): the back-end adds `VHOST_USER_F_PROTOCOL_FEATURES`.
    fn features(&self) -> u64;
    /// `Ok` if the driver may take `acked` of them; why not otherwise.
    fn check_acked(&self, acked: u64) -> Result<(), String>;
    /// Its queues, 64 at most.
    fn queues(&self) -> u16;
    /// The most entries a queue has, a power of two.
    fn queue_size(&self) -> u16;
    /// Reads `data.len()` bytes of its configuration at `offset`, past its end zeros.
    fn read_config(&self, offset: u32, data: &mut [u8]);
    /// Serves `chain`, taken off a queue on the channel's thread, returning it once served.
    fn serve(&self, chain: Chain);
}

/// What a device tells its front-end beside its queues, from any thread: that its configuration
/// changed, which the channel's thread says on the back-end channel (cladefs board 14603); and
/// the features its driver acked.
#[derive(Clone, Debug)]
pub struct Notifier(Arc<Shared>);

impl Notifier {
    /// The device's configuration changed, as it reads now: the front-end is told, on the back-end
    /// channel, for its guest to read it again. Whether the front-end gave one: without, the guest
    /// reads the change only as it next reads its configuration of its own accord.
    pub fn config_changed(&self) -> bool {
        // Released after the configuration changed, which the front-end reads after it hears.
        self.0.config_changed.store(true, Ordering::Release);
        self.0.wake();
        self.0.backend_channel.load(Ordering::Relaxed)
    }

    /// The virtio features the device's driver acked, 0 until it did.
    pub fn acked(&self) -> u64 {
        self.0.acked.load(Ordering::Relaxed)
    }
}

/// The signal [`Channel::stop`] interrupts the channel's thread with, out of a call's write the
/// front-end made wait: its handler does nothing, and restarts no call it interrupts.
fn interrupt_signal() -> libc::c_int {
    libc::SIGRTMIN() + 1
}

/// Installs the interrupt's handler, once a process.
fn install_interrupt() -> io::Result<()> {
    static INSTALLED: OnceLock<Result<(), i32>> = OnceLock::new();
    extern "C" fn nothing(_: libc::c_int) {}
    let installed = INSTALLED.get_or_init(|| {
        // SAFETY: A zeroed sigaction, filled in below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = nothing as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // No SA_RESTART: a call's write it interrupts returns EINTR.
        action.sa_flags = 0;
        // SAFETY: Installs a handler that does nothing, async-signal-safe, for a signal no other
        // part of the process uses.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(interrupt_signal(), &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL));
            }
        }
        Ok(())
    });
    installed.map_err(io::Error::from_raw_os_error)
}

/// How many channels' threads panicked, in this process: each a bug, which ends its channel, and
/// which the tests check none did. The process's count, as no simulation runs channels (not under
/// madsim, `cladefsd::server`).
static PANICS: AtomicU64 = AtomicU64::new(0);

/// How many channels' threads of this process's panicked.
pub fn panics() -> u64 {
    PANICS.load(Ordering::Relaxed)
}

/// A channel's back-end, running on its thread until the front-end closes the channel, breaks
/// it, or [`Channel::stop`] stops it.
pub struct Channel {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    exited: mpsc::Receiver<()>,
}

impl Channel {
    /// Starts the back-end of `device` on a thread named `name`, and returns it and the
    /// front-end's end of the channel, a socket pair's, for the front-end. Refused where kicks
    /// can't be read without waiting (Linux before 5.8).
    pub fn start<D: Device>(device: Arc<D>, name: &str) -> io::Result<(Self, OwnedFd)> {
        assert!(device.queues() <= 64, "a device of more queues than a call's bits");
        assert!(device.queue_size().is_power_of_two(), "a queue size not a power of two");
        install_interrupt()?;
        let mut fds = [0; 2];
        // SAFETY: Makes a socket pair into `fds`.
        let made = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if made != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Just made, owned here.
        let (ours, theirs) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let shared = Arc::new(Shared {
            wake: Wake::new()?,
            calls: AtomicU64::new(0),
            thread: OnceLock::new(),
            stop: AtomicBool::new(false),
            config_changed: AtomicBool::new(false),
            backend_channel: AtomicBool::new(false),
            acked: AtomicU64::new(0),
        });
        let backend = Backend::new(device, shared.clone(), Socket::new(ours)?)?;
        let (exits, exited) = mpsc::channel();
        let thread = std::thread::Builder::new().name(name.into()).spawn(move || {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| backend.run()));
            if ran.is_err() {
                PANICS.fetch_add(1, Ordering::Relaxed);
                log::error!("vhost-user: a channel's thread panicked: the channel ends");
            }
            // Its stopper gone: nothing to tell.
            let _ = exits.send(());
        })?;
        Ok((Self { shared, thread: Some(thread), exited }, theirs))
    }

    /// What its device tells the front-end with beside its queues.
    pub fn notifier(&self) -> Notifier {
        Notifier(self.shared.clone())
    }

    /// Whether its thread has ended: the front-end closed or broke the channel.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Stops it, and waits for its thread to end: interrupting the thread out of a call's write
    /// the front-end made wait, again until it ends, as the signal may come just before the write.
    /// Chains it took are served still, and returned to their rings.
    pub fn stop(mut self) {
        self.stop_now();
    }

    fn stop_now(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        // cladefs: dropped on its own thread, as the last holder of what holds it goes there
        // (the verifying review of board 14537): the thread ends on its own, no stop to wait for,
        // which would wait for good.
        if thread.thread().id() == std::thread::current().id() {
            return;
        }
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.wake();
        loop {
            if thread.is_finished() {
                break;
            }
            // SAFETY: Signals a thread not yet joined, whose handle this holds. `as
            // libc::pthread_t` bridges `RawPthread` (u64) to musl's `pthread_t` (`*mut c_void`).
            unsafe {
                libc::pthread_kill(thread.as_pthread_t() as libc::pthread_t, interrupt_signal())
            };
            match self.exited.recv_timeout(Duration::from_millis(10)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        // A panic of its thread's is caught there, counted ([`panics`]).
        drop(thread.join());
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.stop_now();
    }
}

/// The most messages the channel's thread serves before it looks at its stop, calls and kicks
/// again: the socket, level-triggered, is ready again at once if more came.
const MESSAGES_A_TURN: usize = 16;

/// The epoll tokens of the socket, the wake, the back-end channel, and each queue's kick (from
/// [`KICK`]).
const SOCKET: u64 = 0;
const WAKE: u64 = 1;
const BACKEND: u64 = 2;
const KICK: u64 = 3;

/// A queue as the front-end set it up.
#[derive(Debug, Default)]
struct Queue {
    num: Option<u16>,
    addr: Option<VringAddr>,
    base: u16,
    kick: Option<Kick>,
    call: Option<Call>,
    err: Option<Call>,
    enabled: bool,
    /// Started: its rings mapped.
    run: Option<Arc<Run>>,
    /// Whether its kick is heard: started and enabled, its ring not broken.
    heard: bool,
}

/// A reply the back-end owes once the chains taken off the queues it stops are returned.
#[derive(Debug)]
enum Deferred {
    VringBase { header: Header, index: u16 },
    Reset { header: Header, ack: bool },
}

/// Why the channel ends.
#[derive(Debug, thiserror::Error)]
enum End {
    #[error("{0}")]
    Receive(#[from] ReceiveError),
    #[error("refused {0}")]
    Refused(Refused),
    #[error("replying: {0}")]
    Send(io::Error),
    #[error("the front-end hung up")]
    HungUp,
}

/// A refusal of a value, which [`Backend::answer`] answers or ends the channel with.
type Served = Result<Option<Vec<u8>>, &'static str>;

struct Backend<D: Device> {
    device: Arc<D>,
    shared: Arc<Shared>,
    socket: Socket,
    epoll: OwnedFd,
    owner: bool,
    acked: Option<u64>,
    protocol: u64,
    memory: Option<Arc<MemoryTable>>,
    queues: Vec<Queue>,
    deferred: Option<Deferred>,
    /// The back-end channel, once the front-end gave one.
    backend: Option<BackendChannel>,
}

fn epoll_ctl(epoll: BorrowedFd<'_>, op: libc::c_int, fd: BorrowedFd<'_>, events: u32, token: u64) {
    let mut event = libc::epoll_event { events, u64: token };
    // SAFETY: Changes an epoll instance and a descriptor this process holds.
    let changed = unsafe { libc::epoll_ctl(epoll.as_raw_fd(), op, fd.as_raw_fd(), &mut event) };
    // Only on descriptors it holds, added once and removed once: an error is a bug of the back-end.
    assert_eq!(changed, 0, "epoll_ctl: {}", io::Error::last_os_error());
}

impl<D: Device> Backend<D> {
    fn new(device: Arc<D>, shared: Arc<Shared>, socket: Socket) -> io::Result<Self> {
        // SAFETY: Makes an epoll instance, owned here.
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Just made.
        let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };
        epoll_ctl(epoll.as_fd(), libc::EPOLL_CTL_ADD, socket.fd(), libc::EPOLLIN as u32, SOCKET);
        epoll_ctl(epoll.as_fd(), libc::EPOLL_CTL_ADD, shared.wake.fd(), libc::EPOLLIN as u32, WAKE);
        let queues = (0..device.queues()).map(|_| Queue::default()).collect();
        Ok(Self {
            device,
            shared,
            socket,
            epoll,
            owner: false,
            acked: None,
            protocol: 0,
            memory: None,
            queues,
            deferred: None,
            backend: None,
        })
    }

    fn run(mut self) {
        // Set once, by this thread alone.
        self.shared.thread.set(std::thread::current().id()).expect("one thread a channel");
        // The interrupt reaches this thread, whatever its creator blocks.
        // SAFETY: Signal sets of this function's, and this thread's mask.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, interrupt_signal());
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        }
        match self.serve() {
            Ok(()) => log::info!("vhost-user: a channel stopped"),
            Err(End::Receive(ReceiveError::Closed) | End::HungUp) => {
                log::info!("vhost-user: the front-end closed its channel");
            }
            Err(err) => log::warn!("vhost-user: ending a channel: {err}"),
        }
    }

    fn serve(&mut self) -> Result<(), End> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        loop {
            if self.shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            self.write_calls();
            self.say_config_changed();
            self.finish_deferred()?;
            if self.shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            // SAFETY: Waits for events into `events`.
            let ready =
                unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), 16, -1) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("epoll_wait on the back-end's own instance: {err}");
            }
            let ready = usize::try_from(ready).expect("not negative");
            for event in events.iter().take(ready) {
                let (flags, token) = (event.events, event.u64);
                match token {
                    SOCKET => self.receive(flags)?,
                    WAKE => self.shared.wake.clear(),
                    BACKEND => self.backend_ready(flags),
                    kick => self.kicked(u16::try_from(kick - KICK).expect("a queue's token")),
                }
            }
        }
    }

    /// Writes the calls asked for since it last did.
    fn write_calls(&self) {
        let calls = self.shared.calls.swap(0, Ordering::AcqRel);
        for (index, queue) in self.queues.iter().enumerate() {
            if calls & (1 << index) != 0
                && let Some(call) = &queue.call
            {
                call.signal();
            }
        }
    }

    /// Says the device's configuration changed on the back-end channel, if it did since it last
    /// said so, and there is one.
    fn say_config_changed(&mut self) {
        // Acquiring the change the device released ([`Notifier::config_changed`]).
        if !self.shared.config_changed.swap(false, Ordering::Acquire) {
            return;
        }
        if let Some(backend) = &mut self.backend {
            backend.config_changed();
            self.flush_backend();
        }
    }

    /// Sends what the back-end channel has yet to, hearing when it has room again if it is full;
    /// or lets go of it, gone.
    fn flush_backend(&mut self) {
        let Some(backend) = &mut self.backend else { return };
        match backend.flush() {
            Ok(done) => {
                let events = if done { 0 } else { libc::EPOLLOUT as u32 };
                epoll_ctl(self.epoll.as_fd(), libc::EPOLL_CTL_MOD, backend.fd(), events, BACKEND);
            }
            Err(err) => {
                log::warn!("vhost-user: the back-end channel failed: {err}");
                self.drop_backend();
            }
        }
    }

    /// The back-end channel has room, or the front-end closed it.
    fn backend_ready(&mut self, flags: u32) {
        if flags & (libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0 {
            log::info!("vhost-user: the front-end closed its back-end channel");
            self.drop_backend();
        } else {
            self.flush_backend();
        }
    }

    /// Lets go of the back-end channel, if there is one.
    fn drop_backend(&mut self) {
        if let Some(backend) = self.backend.take() {
            epoll_ctl(self.epoll.as_fd(), libc::EPOLL_CTL_DEL, backend.fd(), 0, 0);
        }
        self.shared.backend_channel.store(false, Ordering::Relaxed);
    }

    /// Receives and serves the messages that came, until one defers its reply.
    fn receive(&mut self, flags: u32) -> Result<(), End> {
        if self.deferred.is_some() {
            // Not listened to while a reply is deferred: the front-end hung up meanwhile.
            return match flags & (libc::EPOLLHUP | libc::EPOLLERR) as u32 {
                0 => Ok(()),
                _ => Err(End::HungUp),
            };
        }
        // cladefs: a few at a time, then back to the loop, which stops, writes calls and hears
        // kicks before it reads more: however the front-end floods it with messages that ask no
        // reply, the channel stops and its queues are served (the Codex review of board 14537).
        for _ in 0..MESSAGES_A_TURN {
            if self.deferred.is_some() || self.shared.stop.load(Ordering::Acquire) {
                break;
            }
            match self.socket.receive()? {
                Some(message) => self.message(message)?,
                None => break,
            }
        }
        if self.deferred.is_some() {
            epoll_ctl(self.epoll.as_fd(), libc::EPOLL_CTL_MOD, self.socket.fd(), 0, SOCKET);
        }
        Ok(())
    }

    /// Serves `message`, and answers it as it asked.
    fn message(&mut self, message: Message) -> Result<(), End> {
        let header = message.header;
        let request = match Request::decode(message, self.device.queues()) {
            Ok(request) => request,
            Err(refused) if refused.answerable() => {
                return self.answer(header, Err(refused));
            }
            Err(refused) => return Err(End::Refused(refused)),
        };
        let served = self.request(header, request);
        let served = served.map_err(|why| Refused::Value { request: header.request, why });
        self.answer(header, served)
    }

    /// Answers the request of `header` as it was served: its reply, if it has one, or its
    /// reply-ack if it asked for one; or its refusal so, or the channel ends.
    fn answer(
        &mut self,
        header: Header,
        served: Result<Option<Vec<u8>>, Refused>,
    ) -> Result<(), End> {
        let acking = header.need_reply() && self.protocol & (1 << PROTOCOL_F_REPLY_ACK) != 0;
        match served {
            Ok(Some(reply)) => self.send(header, &reply),
            // A deferred reply, `GET_VRING_BASE`'s or `RESET_DEVICE`'s, is sent once due.
            Ok(None) if acking && !is_get(header.request) && header.request != RESET_DEVICE => {
                self.send(header, U64 { value: 0 }.as_bytes())
            }
            Ok(None) => Ok(()),
            Err(refused) if acking && !is_get(header.request) => {
                log::debug!("vhost-user: refused, and said so: {refused}");
                self.send(header, U64 { value: 1u64.to_le() }.as_bytes())
            }
            Err(refused) => Err(End::Refused(refused)),
        }
    }

    fn send(&self, header: Header, payload: &[u8]) -> Result<(), End> {
        self.socket.send(header.reply(payload.len()), payload).map_err(End::Send)
    }

    /// Whether the front-end negotiated rings that start disabled.
    fn rings_start_disabled(&self) -> bool {
        self.acked.is_some_and(|acked| acked & (1 << F_PROTOCOL_FEATURES) != 0)
    }

    fn started(&self) -> bool {
        self.queues.iter().any(|queue| queue.run.is_some())
    }

    /// Serves `request`: its reply's payload, if it has one; or why it is refused.
    fn request(&mut self, header: Header, request: Request) -> Served {
        let word = |value: u64| Ok(Some(U64 { value: value.to_le() }.as_bytes().to_vec()));
        match request {
            Request::GetFeatures => word(self.offered()),
            Request::SetFeatures(features) => {
                if features & !self.offered() != 0 {
                    return Err("features the back-end doesn't offer");
                }
                if self.started() {
                    return Err("features changed while a queue runs");
                }
                if let Err(why) = self.device.check_acked(features & !(1 << F_PROTOCOL_FEATURES)) {
                    log::warn!("vhost-user: {why}");
                    return Err("features the device refuses");
                }
                self.acked = Some(features);
                self.shared.acked.store(features, Ordering::Relaxed);
                Ok(None)
            }
            Request::SetOwner => {
                if std::mem::replace(&mut self.owner, true) {
                    return Err("an owner set again");
                }
                Ok(None)
            }
            Request::GetProtocolFeatures => word(PROTOCOL_FEATURES),
            Request::SetProtocolFeatures(features) => {
                if features & !PROTOCOL_FEATURES != 0 {
                    return Err("protocol features the back-end doesn't offer");
                }
                self.protocol = features;
                Ok(None)
            }
            Request::GetQueueNum => word(u64::from(self.device.queues())),
            Request::SetMemTable(regions) => {
                let memory = MemoryTable::new(regions).map_err(|err| {
                    log::warn!("vhost-user: refusing a memory table: {err}");
                    "a memory table of regions the back-end refuses"
                })?;
                let memory = Arc::new(memory);
                // Its running queues' rings mapped from it, as the front-end moves them.
                let mut broken = Vec::new();
                for (index, queue) in self.queues.iter().enumerate() {
                    let (Some(run), Some(num), Some(addr)) = (&queue.run, queue.num, queue.addr)
                    else {
                        continue;
                    };
                    match RingMaps::map(&memory, num, addr) {
                        Ok(maps) => run.remap(maps),
                        Err(err) => broken.push((index, err.to_string())),
                    }
                }
                for (index, why) in broken {
                    self.broke(index, &why);
                }
                self.memory = Some(memory);
                Ok(None)
            }
            Request::SetVringNum { index, num } => {
                self.stopped(index)?;
                if num == 0 || !num.is_power_of_two() || num > self.device.queue_size() {
                    return Err("a ring's size not a power of two up to the device's");
                }
                self.queues[usize::from(index)].num = Some(num);
                Ok(None)
            }
            Request::SetVringAddr { index, addr } => {
                self.stopped(index)?;
                self.queues[usize::from(index)].addr = Some(addr);
                Ok(None)
            }
            Request::SetVringBase { index, base } => {
                self.stopped(index)?;
                self.queues[usize::from(index)].base = base;
                Ok(None)
            }
            Request::GetVringBase { index } => {
                self.stop_queue(index);
                self.deferred = Some(Deferred::VringBase { header, index });
                // Answered once its chains are back (`finish_deferred`).
                Ok(None)
            }
            Request::SetVringKick { index, fd } => {
                let kick = Kick::new(fd).ok_or("a kick that is no eventfd")?;
                let queue = &mut self.queues[usize::from(index)];
                if queue.heard
                    && let Some(old) = &queue.kick
                {
                    epoll_ctl(self.epoll.as_fd(), libc::EPOLL_CTL_DEL, old.fd(), 0, 0);
                    queue.heard = false;
                }
                queue.kick = Some(kick);
                if !self.rings_start_disabled() {
                    self.queues[usize::from(index)].enabled = true;
                }
                self.start(index)
            }
            Request::SetVringCall { index, fd } => {
                let call = Call::new(fd).ok_or("a call that is no eventfd")?;
                self.queues[usize::from(index)].call = Some(call);
                Ok(None)
            }
            Request::SetVringErr { index, fd } => {
                let err = Call::new(fd).ok_or("an error notifier that is no eventfd")?;
                self.queues[usize::from(index)].err = Some(err);
                Ok(None)
            }
            Request::SetVringEnable { index, enable } => {
                self.queues[usize::from(index)].enabled = enable;
                if enable {
                    self.start(index)
                } else {
                    self.unhear(index);
                    Ok(None)
                }
            }
            Request::GetConfig { offset, size } => {
                let config = ConfigHeader { offset: offset.to_le(), size: size.to_le(), flags: 0 };
                let mut reply = config.as_bytes().to_vec();
                let mut data = vec![0; usize::try_from(size).expect("256 at most")];
                self.device.read_config(offset, &mut data);
                reply.extend_from_slice(&data);
                Ok(Some(reply))
            }
            Request::SetBackendReqFd(fd) => {
                if self.protocol & (1 << PROTOCOL_F_BACKEND_REQ) == 0 {
                    return Err("a back-end channel, which the front-end didn't negotiate");
                }
                let backend = BackendChannel::new(fd)
                    .ok_or("a back-end channel that is no Unix stream socket")?;
                self.drop_backend();
                epoll_ctl(self.epoll.as_fd(), libc::EPOLL_CTL_ADD, backend.fd(), 0, BACKEND);
                self.backend = Some(backend);
                self.shared.backend_channel.store(true, Ordering::Relaxed);
                Ok(None)
            }
            Request::ResetDevice => {
                for index in 0..self.device.queues() {
                    self.stop_queue(index);
                }
                let ack = header.need_reply() && self.protocol & (1 << PROTOCOL_F_REPLY_ACK) != 0;
                self.deferred = Some(Deferred::Reset { header, ack });
                Ok(None)
            }
        }
    }

    /// The features offered: the device's, and protocol features.
    fn offered(&self) -> u64 {
        self.device.features() | (1 << F_PROTOCOL_FEATURES)
    }

    /// Queue `index`, if it isn't started: a ring's setup changes only before.
    fn stopped(&self, index: u16) -> Result<(), &'static str> {
        match self.queues[usize::from(index)].run {
            Some(_) => Err("a ring changed while it runs"),
            None => Ok(()),
        }
    }

    /// Starts queue `index` if it has all it needs, and hears its kicks if it is enabled.
    fn start(&mut self, index: u16) -> Served {
        let disabled = self.rings_start_disabled();
        let queue = &mut self.queues[usize::from(index)];
        if queue.run.is_none()
            && let (Some(memory), Some(_), Some(num), Some(addr), Some(_)) =
                (&self.memory, self.acked, queue.num, queue.addr, &queue.kick)
        {
            let maps = RingMaps::map(memory, num, addr).map_err(|err| {
                log::warn!("vhost-user: refusing ring {index}: {err}");
                "a ring the memory table doesn't hold"
            })?;
            queue.run = Some(Arc::new(Run::new(index, maps, queue.base, self.shared.clone())));
        }
        let queue = &mut self.queues[usize::from(index)];
        if queue.run.is_some() && (queue.enabled || !disabled) && !queue.heard {
            let kick = queue.kick.as_ref().expect("a started queue has its kick");
            let token = KICK + u64::from(index);
            epoll_ctl(
                self.epoll.as_fd(),
                libc::EPOLL_CTL_ADD,
                kick.fd(),
                libc::EPOLLIN as u32,
                token,
            );
            queue.heard = true;
            // Kicks before it was heard: what they made available, taken now.
            self.take(index);
        }
        Ok(None)
    }

    /// Hears queue `index`'s kicks no more.
    fn unhear(&mut self, index: u16) {
        let queue = &mut self.queues[usize::from(index)];
        if queue.heard {
            let kick = queue.kick.as_ref().expect("a heard queue has its kick");
            epoll_ctl(self.epoll.as_fd(), libc::EPOLL_CTL_DEL, kick.fd(), 0, 0);
            queue.heard = false;
        }
    }

    /// Stops queue `index`: its kicks heard no more, and forgotten, as a ring starts again with a
    /// kick (`SET_VRING_KICK`); its chains taken are served still.
    fn stop_queue(&mut self, index: u16) {
        self.unhear(index);
        self.queues[usize::from(index)].kick = None;
    }

    /// Queue `index`'s ring means nothing: it serves no more, and its error notifier says so.
    fn broke(&mut self, index: usize, why: &str) {
        log::warn!("vhost-user: queue {index} broke: {why}");
        self.unhear(u16::try_from(index).expect("a queue's index"));
        if let Some(err) = &self.queues[index].err {
            err.signal();
        }
    }

    /// Queue `index` was kicked: takes what it made available.
    fn kicked(&mut self, index: u16) {
        let queue = &self.queues[usize::from(index)];
        let Some(kick) = &queue.kick else { return };
        if let Err(err) = kick.hear() {
            self.broke(usize::from(index), &format!("reading its kick: {err}"));
            return;
        }
        self.take(index);
    }

    /// Takes the chains queue `index` made available, and has the device serve each.
    fn take(&mut self, index: u16) {
        let queue = &self.queues[usize::from(index)];
        let (Some(run), Some(memory)) = (&queue.run, &self.memory) else { return };
        match run.take(memory) {
            Ok(chains) => chains.into_iter().for_each(|chain| self.device.serve(chain)),
            Err(Broken(Some(err))) => self.broke(usize::from(index), &err.to_string()),
            Err(Broken(None)) => {}
        }
    }

    /// Sends the deferred reply once the chains of the queues it stops are back.
    fn finish_deferred(&mut self) -> Result<(), End> {
        let Some(deferred) = &self.deferred else { return Ok(()) };
        let in_flight = |queue: &Queue| queue.run.as_ref().map_or(0, |run| run.state().1);
        match *deferred {
            Deferred::VringBase { header, index } => {
                let queue = &mut self.queues[usize::from(index)];
                if in_flight(queue) > 0 {
                    return Ok(());
                }
                if let Some(run) = queue.run.take() {
                    queue.base = run.state().0;
                }
                let state = VringState {
                    index: u32::from(index).to_le(),
                    num: u32::from(queue.base).to_le(),
                };
                self.deferred = None;
                self.send(header, state.as_bytes())?;
            }
            Deferred::Reset { header, ack } => {
                if self.queues.iter().any(|queue| in_flight(queue) > 0) {
                    return Ok(());
                }
                self.acked = None;
                self.shared.acked.store(0, Ordering::Relaxed);
                self.protocol = 0;
                self.drop_backend();
                self.memory = None;
                self.queues.iter_mut().for_each(|queue| *queue = Queue::default());
                self.deferred = None;
                if ack {
                    self.send(header, U64 { value: 0 }.as_bytes())?;
                }
            }
        }
        epoll_ctl(
            self.epoll.as_fd(),
            libc::EPOLL_CTL_MOD,
            self.socket.fd(),
            libc::EPOLLIN as u32,
            SOCKET,
        );
        Ok(())
    }
}

/// Whether `request` is answered with a reply of its own, whatever it asks.
fn is_get(request: u32) -> bool {
    matches!(
        request,
        GET_FEATURES | GET_PROTOCOL_FEATURES | GET_QUEUE_NUM | GET_VRING_BASE | GET_CONFIG
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::front_end::{FrontEnd, WAIT};

    /// A device of one queue, which serves nothing it is given: chains return themselves.
    struct Idle;

    impl Device for Idle {
        fn features(&self) -> u64 {
            1 << 32
        }

        fn check_acked(&self, _: u64) -> Result<(), String> {
            Ok(())
        }

        fn queues(&self) -> u16 {
            1
        }

        fn queue_size(&self) -> u16 {
            256
        }

        fn read_config(&self, _: u32, data: &mut [u8]) {
            data.fill(0);
        }

        fn serve(&self, _: Chain) {}
    }

    /// The Codex review of board 14537: a front-end flooding the channel with messages that ask no
    /// reply, as fast as it can write them, a thousand to a write, doesn't keep the channel from
    /// stopping: its thread serves a few at a time, looking at its stop between.
    #[test]
    fn floods_of_messages_dont_keep_a_channel_from_stopping() {
        let (channel, theirs) = Channel::start(Arc::new(Idle), "flood").unwrap();
        let front = Arc::new(FrontEnd::new(theirs));
        let header = Header { request: SET_PROTOCOL_FEATURES, flags: VERSION, size: 8 };
        let mut message = header.as_bytes().to_vec();
        message.extend_from_slice(U64 { value: 0 }.as_bytes());
        let batch = message.repeat(1000);
        // The flood under way before the stop, from one writer alone, whose writes keep their
        // framing.
        front.send_bytes(&batch).unwrap();
        let flooding = std::thread::spawn({
            let front = front.clone();
            // Until the channel ends, which fails the write.
            move || while front.send_bytes(&batch).is_ok() {}
        });
        let (stopped, stop) = mpsc::channel();
        std::thread::spawn(move || {
            channel.stop();
            stopped.send(()).unwrap();
        });
        assert_eq!(stop.recv_timeout(WAIT), Ok(()), "the channel never stopped");
        flooding.join().unwrap();
    }
}
