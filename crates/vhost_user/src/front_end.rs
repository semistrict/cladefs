// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A minimal vhost-user front-end, for tests: it plays a stock VMM, with guest RAM a plain memfd
//! (sealed against shrinking and growing, as QEMU's and Firecracker's are), and the guest's
//! driver, putting chains on split rings in that RAM, kicking the back-end and waiting for its
//! calls. It trusts the back-end no more than a test does: every wait is bounded
//! ([`WAIT`]), failing the test rather than hanging it; and it sends what a hostile front-end
//! would as readily as what a stock one does ([`FrontEnd::send`]).

use std::io::{self, Read as _};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use zerocopy::{FromBytes, IntoBytes};

use crate::protocol::*;
use crate::ring::{Driver, RingMemory, Rings, Segment, ring_lens};

/// How long the front-end waits at most for a reply or a call.
pub const WAIT: Duration = Duration::from_secs(10);

/// Guest RAM: a memfd, mapped in the front-end, at guest address 0.
pub struct GuestMemory {
    fd: OwnedFd,
    base: *mut u8,
    size: u64,
}

// SAFETY: Shared memory, which the tests access through atomics (rings) or copies.
unsafe impl Send for GuestMemory {}
// SAFETY: As above.
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// `size` bytes of guest RAM, a memfd sealed against shrinking and growing.
    pub fn new(size: u64) -> io::Result<Self> {
        Self::with_seals(size, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW)
    }

    /// `size` bytes of guest RAM, a memfd sealed with `seals`: for hostile front-ends' memory.
    pub fn with_seals(size: u64, seals: libc::c_int) -> io::Result<Self> {
        // SAFETY: Makes a memfd, owned here.
        let fd = unsafe {
            libc::memfd_create(
                c"vhost-user-guest".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Just made.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let len = libc::off_t::try_from(size).map_err(io::Error::other)?;
        // SAFETY: Sizes and seals a memfd of this function's.
        unsafe {
            if libc::ftruncate(fd.as_raw_fd(), len) != 0
                || (seals != 0 && libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) != 0)
            {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: Maps the memfd whole, shared.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, base: base.cast(), size })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Where guest address 0 is in the front-end's address space.
    pub fn user_base(&self) -> u64 {
        self.base as u64
    }

    /// The front-end's pointer to guest address `addr`, within it.
    fn ptr(&self, addr: u64) -> *mut u8 {
        assert!(addr < self.size, "guest address {addr:#x} past the guest's RAM");
        // SAFETY: Within the mapping.
        unsafe { self.base.add(addr as usize) }
    }

    /// Writes `bytes` at guest address `addr`.
    pub fn write(&self, addr: u64, bytes: &[u8]) {
        assert!(addr + bytes.len() as u64 <= self.size);
        // SAFETY: Within the mapping; the back-end writes no bytes a test writes meanwhile.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr(addr), bytes.len()) };
    }

    /// The `len` bytes at guest address `addr`.
    pub fn read(&self, addr: u64, len: usize) -> Vec<u8> {
        assert!(addr + len as u64 <= self.size);
        let mut bytes = vec![0; len];
        // SAFETY: Within the mapping.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr(addr), bytes.as_mut_ptr(), len) };
        bytes
    }

    /// The memory table's one region of it, as `SET_MEM_TABLE` sends it.
    pub fn region(&self) -> RegionWire {
        RegionWire { guest: 0, size: self.size, user: self.user_base(), offset: 0 }
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        // SAFETY: Unmaps the mapping, which nothing uses any more.
        unsafe { libc::munmap(self.base.cast(), self.size as usize) };
    }
}

/// An eventfd of the front-end's: a kick it writes, or a call it reads.
pub fn eventfd() -> OwnedFd {
    // SAFETY: Makes an eventfd, owned here.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    assert!(fd >= 0, "eventfd: {}", io::Error::last_os_error());
    // SAFETY: Just made.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Signals an eventfd.
pub fn signal(fd: BorrowedFd<'_>) {
    let one = 1u64;
    // SAFETY: Writes 8 bytes of `one`.
    let wrote = unsafe { libc::write(fd.as_raw_fd(), (&raw const one).cast(), 8) };
    assert_eq!(wrote, 8, "signalling an eventfd: {}", io::Error::last_os_error());
}

/// Waits for an eventfd's signal, [`WAIT`] at most: whether it came.
pub fn wait_signal(fd: BorrowedFd<'_>, wait: Duration) -> bool {
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let ms = libc::c_int::try_from(wait.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: Polls one descriptor of the caller's.
    if unsafe { libc::poll(&mut poll, 1, ms) } != 1 {
        return false;
    }
    let mut count = 0u64;
    // SAFETY: Reads 8 bytes into `count`, the eventfd nonblocking.
    unsafe { libc::read(fd.as_raw_fd(), (&raw mut count).cast(), 8) };
    true
}

/// The front-end's end of a channel.
pub struct FrontEnd {
    socket: UnixStream,
}

impl FrontEnd {
    /// The front-end of the channel whose end `fd` is, every reply waited for [`WAIT`] at most.
    pub fn new(fd: OwnedFd) -> Self {
        let socket = UnixStream::from(fd);
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        Self { socket }
    }

    /// Sends `header`, `payload` as it is, and `fds`: whatever a hostile front-end sends.
    pub fn send(&self, header: Header, payload: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(payload);
        send_with_fds(self.socket.as_fd(), &bytes, fds)
    }

    /// Sends raw `bytes`, no header made: a hostile front-end's framing.
    pub fn send_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        send_with_fds(self.socket.as_fd(), bytes, &[])
    }

    /// Sends request `request` with `payload` and `fds`, asking for a reply-ack if `need_reply`.
    pub fn request(
        &self,
        request: u32,
        need_reply: bool,
        payload: &[u8],
        fds: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        let flags = VERSION | if need_reply { NEED_REPLY_FLAG } else { 0 };
        let size = u32::try_from(payload.len()).unwrap();
        self.send(Header { request, flags, size }, payload, fds)
    }

    /// The next reply, [`WAIT`] at most: an error if the channel ends, or none comes.
    pub fn reply(&self) -> io::Result<(Header, Vec<u8>)> {
        let mut socket = &self.socket;
        let mut header = [0u8; size_of::<Header>()];
        socket.read_exact(&mut header)?;
        let header = Header::read_from_bytes(&header).unwrap();
        let mut payload = vec![0; header.size as usize];
        socket.read_exact(&mut payload)?;
        Ok((header, payload))
    }

    /// Sends request `request`, asking for a reply-ack, and returns the ack: 0 if served.
    pub fn acked(&self, request: u32, payload: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<u64> {
        self.request(request, true, payload, fds)?;
        let (header, payload) = self.reply()?;
        assert_eq!(header.request, request, "a reply to another request");
        assert_eq!(header.flags, VERSION | REPLY_FLAG);
        Ok(U64::read_from_bytes(&payload).expect("an ack's word").value)
    }

    /// Sends GET request `request` with `payload`, and returns its reply's payload.
    pub fn get(&self, request: u32, payload: &[u8]) -> io::Result<Vec<u8>> {
        self.request(request, false, payload, &[])?;
        let (header, payload) = self.reply()?;
        assert_eq!(header.request, request, "a reply to another request");
        Ok(payload)
    }

    /// GET request `request`'s word.
    pub fn get_u64(&self, request: u32) -> io::Result<u64> {
        let payload = self.get(request, &[])?;
        Ok(U64::read_from_bytes(&payload).map_err(|_| io::Error::other("no word"))?.value)
    }

    /// Whether the back-end closed the channel: a read finds its end, [`WAIT`] at most.
    pub fn closed(&self) -> bool {
        let mut byte = [0u8];
        match (&self.socket).read(&mut byte) {
            Ok(0) => true,
            Err(err) => err.kind() == io::ErrorKind::ConnectionReset,
            Ok(_) => false,
        }
    }

    /// Sets the device up as a stock front-end does, with `memory` its guest RAM and `queues`
    /// request queues: features, protocol features, owner, memory table, and each queue's rings at
    /// `layout`'s addresses, kicks and calls; and enables them. Returns the queues.
    pub fn set_up(
        &self,
        memory: &Arc<GuestMemory>,
        queues: &[QueueLayout],
    ) -> io::Result<Vec<Queue>> {
        let features = self.get_u64(GET_FEATURES)?;
        let ack = |request, payload: &[u8], fds: &[BorrowedFd<'_>]| -> io::Result<()> {
            match self.acked(request, payload, fds)? {
                0 => Ok(()),
                failed => Err(io::Error::other(format!("request {request} refused: {failed}"))),
            }
        };
        let protocol = self.get_u64(GET_PROTOCOL_FEATURES)?;
        // Protocol features first, so that the rest are acked.
        self.request(SET_PROTOCOL_FEATURES, false, U64 { value: protocol }.as_bytes(), &[])?;
        ack(SET_FEATURES, U64 { value: features }.as_bytes(), &[])?;
        ack(SET_OWNER, &[], &[])?;
        ack(SET_MEM_TABLE, &mem_table(&[memory.region()]), &[memory.fd()])?;
        let mut made = Vec::new();
        for (index, layout) in queues.iter().enumerate() {
            let index = index as u32;
            let state = |num: u32| VringState { index, num };
            ack(SET_VRING_NUM, state(u32::from(layout.size)).as_bytes(), &[])?;
            ack(SET_VRING_BASE, state(0).as_bytes(), &[])?;
            let user = memory.user_base();
            let addr = VringAddrWire {
                index,
                flags: 0,
                desc: user + layout.desc,
                used: user + layout.used,
                avail: user + layout.avail,
                log: 0,
            };
            ack(SET_VRING_ADDR, addr.as_bytes(), &[])?;
            let (kick, call, err) = (eventfd(), eventfd(), eventfd());
            ack(SET_VRING_CALL, U64 { value: u64::from(index) }.as_bytes(), &[call.as_fd()])?;
            ack(SET_VRING_ERR, U64 { value: u64::from(index) }.as_bytes(), &[err.as_fd()])?;
            ack(SET_VRING_KICK, U64 { value: u64::from(index) }.as_bytes(), &[kick.as_fd()])?;
            ack(SET_VRING_ENABLE, state(1).as_bytes(), &[])?;
            made.push(Queue::new(memory.clone(), *layout, kick, call, err));
        }
        Ok(made)
    }
}

/// A `SET_MEM_TABLE`'s payload of `regions`.
pub fn mem_table(regions: &[RegionWire]) -> Vec<u8> {
    let header = MemoryHeader { regions: regions.len() as u32, padding: 0 };
    let mut payload = header.as_bytes().to_vec();
    regions.iter().for_each(|region| payload.extend_from_slice(region.as_bytes()));
    payload
}

/// Sends `bytes` with `fds` on `socket`, whole.
fn send_with_fds(socket: BorrowedFd<'_>, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    let raw: Vec<libc::c_int> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let mut iov = libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
    // SAFETY: Pure arithmetic of the C library's.
    let space = unsafe { libc::CMSG_SPACE((raw.len() * size_of::<libc::c_int>()) as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: A zeroed msghdr is a valid one to fill in.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    if !raw.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: The first header of the control buffer `msg` describes, filled in here.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((raw.len() * size_of::<libc::c_int>()) as u32) as _;
            std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(cmsg).cast(), raw.len());
        }
    }
    // SAFETY: Sends what `msg` describes.
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != bytes.len() {
        return Err(io::Error::other("a short send"));
    }
    Ok(())
}

/// Where a queue's rings and buffers are in guest RAM: its size, the guest addresses of its
/// descriptor table, available and used rings, and the buffers its chains use, of `buffers_len`
/// bytes.
#[derive(Clone, Copy, Debug)]
pub struct QueueLayout {
    pub size: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub buffers: u64,
    pub buffers_len: u64,
}

impl QueueLayout {
    /// Queue `index`'s layout of `size` entries in a guest of 1 MiB a queue's rings and `buffers`
    /// bytes of buffers each, from guest address 0.
    pub fn nth(index: u64, size: u16, buffers: u64) -> Self {
        let rings = 1 << 20;
        let at = index * (rings + buffers);
        let (desc_len, avail_len, _) = ring_lens(size);
        let avail = (at + desc_len).next_multiple_of(4096);
        let used = (avail + avail_len).next_multiple_of(4096);
        Self { size, desc: at, avail, used, buffers: at + rings, buffers_len: buffers }
    }
}

/// A queue's rings in guest RAM, as the guest's driver has them.
pub struct GuestRings {
    memory: Arc<GuestMemory>,
    layout: QueueLayout,
}

impl RingMemory for GuestRings {
    fn rings(&self) -> Rings<'_> {
        let memory = &self.memory;
        let layout = self.layout;
        // SAFETY: Within guest RAM, mapped as long as `memory`, aligned (`QueueLayout::nth`),
        // accessed through atomics alone.
        unsafe {
            Rings::from_raw(
                layout.size,
                memory.ptr(layout.desc),
                memory.ptr(layout.avail),
                memory.ptr(layout.used),
            )
        }
    }
}

/// A queue, the guest's driver's half of it.
pub struct Queue {
    pub memory: Arc<GuestMemory>,
    pub layout: QueueLayout,
    pub driver: Driver<GuestRings>,
    pub kick: OwnedFd,
    pub call: OwnedFd,
    /// The ring's error notifier, which the back-end signals as the ring breaks.
    pub err: OwnedFd,
}

/// What a request came back with: the bytes the back-end says it wrote, and the writable buffers'
/// bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Returned {
    pub len: u32,
    pub written: Vec<u8>,
}

impl Queue {
    fn new(
        memory: Arc<GuestMemory>,
        layout: QueueLayout,
        kick: OwnedFd,
        call: OwnedFd,
        err: OwnedFd,
    ) -> Self {
        let driver = Driver::new(GuestRings { memory: memory.clone(), layout });
        Self { memory, layout, driver, kick, call, err }
    }

    /// A hostile driver's descriptor `id`: whatever it likes, past the table too, which the
    /// back-end must refuse.
    pub fn raw_descriptor(&self, id: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let rings = self.driver.memory().rings();
        let descriptor = &rings.desc[usize::from(id) % rings.desc.len()];
        descriptor.addr.store(addr, std::sync::atomic::Ordering::Relaxed);
        descriptor.len.store(len, std::sync::atomic::Ordering::Relaxed);
        descriptor.flags.store(flags, std::sync::atomic::Ordering::Relaxed);
        descriptor.next.store(next, std::sync::atomic::Ordering::Relaxed);
    }

    /// A hostile driver makes `head` available at available index `at`, and moves the index to
    /// `idx`, whatever they are; and kicks.
    pub fn raw_offer(&self, at: u16, head: u16, idx: u16) {
        let rings = self.driver.memory().rings();
        let slot = usize::from(at) % rings.avail_ring.len();
        rings.avail_ring[slot].store(head, std::sync::atomic::Ordering::Relaxed);
        rings.avail_idx.store(idx, std::sync::atomic::Ordering::Release);
        signal(self.kick.as_fd());
    }

    /// The used ring's index, as the back-end left it.
    pub fn used_idx(&self) -> u16 {
        self.driver.memory().rings().used_idx.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Adds a chain of `readable` bytes, split into descriptors of `split` bytes at most, and
    /// `writable` bytes for the back-end, in the queue's buffers from offset `at`, published and
    /// kicked if the back-end asked: its head. The writable bytes are filled with 0xa5 first.
    pub fn add(&mut self, at: u64, readable: &[u8], writable: usize, split: usize) -> u16 {
        let base = self.layout.buffers + at;
        assert!(at + readable.len() as u64 + writable as u64 <= self.layout.buffers_len);
        self.memory.write(base, readable);
        let writable_at = base + readable.len() as u64;
        self.memory.write(writable_at, &vec![0xa5; writable]);
        let mut segments = Vec::new();
        for (i, part) in readable.chunks(split).enumerate() {
            let offset = base + (i * split) as u64;
            segments.push(Segment { offset, len: part.len() as u32, writable: false });
        }
        let mut done = 0;
        while done < writable {
            let len = split.min(writable - done);
            let offset = writable_at + done as u64;
            segments.push(Segment { offset, len: len as u32, writable: true });
            done += len;
        }
        let head = self.driver.add(&segments).expect("room for a chain");
        if self.driver.publish() {
            signal(self.kick.as_fd());
        }
        head
    }

    /// The next chain returned, waiting for the back-end's call [`WAIT`] at most: `None` if none
    /// came.
    pub fn used(&mut self) -> Option<crate::ring::Used> {
        loop {
            if let Some(used) = self.driver.used().expect("the back-end returns chains it took") {
                return Some(used);
            }
            if self.driver.arm() {
                continue;
            }
            if !wait_signal(self.call.as_fd(), WAIT) {
                return None;
            }
        }
    }

    /// A request of `readable` bytes with `writable` bytes for the back-end, at the start of the
    /// queue's buffers: what came back, waiting [`WAIT`] at most; `None` if nothing did.
    pub fn request(&mut self, readable: &[u8], writable: usize, split: usize) -> Option<Returned> {
        let head = self.add(0, readable, writable, split);
        let used = self.used()?;
        assert_eq!(used.head, head, "another chain came back");
        let at = self.layout.buffers + readable.len() as u64;
        Some(Returned { len: used.len, written: self.memory.read(at, writable) })
    }
}
