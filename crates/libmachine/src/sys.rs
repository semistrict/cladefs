// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What libmachine needs of its host OS beyond std: an event counter a thread can wait on with
//! others ([`EventFd`]), the wait itself ([`wait_readable`]), and entropy ([`getrandom`]).
//!
//! macOS has no eventfd: an [`EventFd`] is a pipe, which the HVF backend signals and reads
//! (`backend::hvf`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

/// The one flag libmachine passes: reads never block. Every event here is so.
pub(crate) const EFD_NONBLOCK: i32 = 1;

/// An eventfd's subset libmachine uses, over a pipe: each write a byte, a read drains them and
/// returns how many there were, as an eventfd's count (up to the pipe's room: a write to a
/// full pipe adds nothing, its reader having signals waiting anyway).
#[derive(Debug)]
pub(crate) struct EventFd {
    ends: Arc<(OwnedFd, OwnedFd)>,
}

impl EventFd {
    /// A new event; `flags` must be [`EFD_NONBLOCK`], the only kind libmachine makes: neither
    /// end blocks (a full pipe already says what another write would).
    pub(crate) fn new(flags: i32) -> io::Result<Self> {
        assert_eq!(flags, EFD_NONBLOCK, "libmachine's events never block");
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe returned two new descriptors this process owns alone.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [&read, &write] {
            set_flags(fd.as_raw_fd())?;
        }
        Ok(Self { ends: Arc::new((read, write)) })
    }

    /// Adds `value` to the event's count, as an eventfd's write: `value` bytes, as many as
    /// the pipe has room for (a full pipe's reader has signals waiting anyway).
    pub(crate) fn write(&self, value: u64) -> io::Result<()> {
        const ONES: [u8; 64] = [1; 64];
        let mut left = value;
        while left > 0 {
            let n = left.min(ONES.len() as u64) as usize;
            // SAFETY: writes at most `n` bytes from a static array of 64 to an open
            // descriptor.
            let wrote = unsafe { libc::write(self.ends.1.as_raw_fd(), ONES.as_ptr().cast(), n) };
            match wrote {
                w if w > 0 => left -= w as u64,
                _ => match io::Error::last_os_error() {
                    e if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    e if e.kind() == io::ErrorKind::Interrupted => {}
                    e => return Err(e),
                },
            }
        }
        Ok(())
    }

    /// Takes every signal waiting: how many, or `WouldBlock` if there was none, as a
    /// non-blocking eventfd's read.
    pub(crate) fn read(&self) -> io::Result<u64> {
        let mut buf = [0u8; 64];
        let mut got = 0u64;
        loop {
            // SAFETY: reads into a live local buffer of its length.
            let n =
                unsafe { libc::read(self.ends.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            match n {
                n if n > 0 => got += n as u64,
                // The write end is this event's own, open as long as it is.
                0 => unreachable!("an event's pipe has its write end"),
                _ => match io::Error::last_os_error() {
                    e if e.kind() == io::ErrorKind::WouldBlock && got > 0 => return Ok(got),
                    e if e.kind() == io::ErrorKind::Interrupted => {}
                    e => return Err(e),
                },
            }
        }
    }

    /// Another handle to the same event, as an eventfd's dup.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self { ends: self.ends.clone() })
    }
}

impl AsRawFd for EventFd {
    /// The read end, which a waiter polls.
    fn as_raw_fd(&self) -> RawFd {
        self.ends.0.as_raw_fd()
    }
}

/// Close-on-exec and non-blocking.
fn set_flags(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor this process owns; no pointers.
    unsafe {
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Waits until one of `fds` is readable, `timeout_ms` at most (-1: no limit), and returns which,
/// by their index in `fds`; none if the wait timed out. poll(2): libmachine's waiters watch a
/// handful of descriptors each.
pub(crate) fn wait_readable(fds: &[RawFd], timeout_ms: i32) -> io::Result<Vec<usize>> {
    let mut polled: Vec<libc::pollfd> =
        fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    loop {
        // SAFETY: `polled` is a live array of its length.
        let n =
            unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, timeout_ms) };
        if n >= 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(polled
        .iter()
        .enumerate()
        .filter(|(_, p)| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        .map(|(i, _)| i)
        .collect())
}

/// The host's page size: what a file's mapping's offset is a multiple of.
pub(crate) fn page_size() -> usize {
    // SAFETY: no pointers.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).expect("the host has a page size")
}

/// Fills `buf` from the kernel's random number generator.
pub(crate) fn getrandom(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(256) {
        // SAFETY: getentropy writes at most 256 bytes into `chunk`, which has that many at most.
        if unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } != 0 {
            panic!("getentropy failed: {}", io::Error::last_os_error());
        }
    }
}

/// What a [`Poller`] found ready.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Polled {
    pub(crate) token: u64,
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    /// Its peer hung up, or it failed.
    pub(crate) closed: bool,
}

/// A set of file descriptors one thread waits on, each with a token (kqueue). A descriptor is watched for reads ([`Poller::add_reads`], level-triggered: an event's
/// pipe or eventfd, read till empty when woken), or for reads, writes and hang-ups edge-triggered
/// ([`Poller::add_streams`]: a socket, its readiness taken till `WouldBlock`).
pub(crate) struct Poller {
    fd: OwnedFd,
}

impl Poller {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: a plain system call.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new descriptor, owned from here.
        Ok(Self { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    /// Watches `fd` for reads, level-triggered, as `token`.
    pub(crate) fn add_reads(&self, fd: RawFd, token: u64) -> io::Result<()> {
        self.add(fd, token, false)
    }

    /// Watches `fd` for reads, writes and hang-ups, edge-triggered, as `token`.
    pub(crate) fn add_streams(&self, fd: RawFd, token: u64) -> io::Result<()> {
        self.add(fd, token, true)
    }

    fn add(&self, fd: RawFd, token: u64, stream: bool) -> io::Result<()> {
        let change = |filter| libc::kevent {
            ident: fd as libc::uintptr_t,
            filter,
            flags: libc::EV_ADD | if stream { libc::EV_CLEAR } else { 0 },
            fflags: 0,
            data: 0,
            udata: token as *mut libc::c_void,
        };
        let changes: Vec<libc::kevent> = if stream {
            vec![change(libc::EVFILT_READ), change(libc::EVFILT_WRITE)]
        } else {
            vec![change(libc::EVFILT_READ)]
        };
        // SAFETY: `changes` are valid kevents; no events are asked back.
        let done = unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as libc::c_int,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if done < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Waits at most `timeout_ms` (-1: forever) for what is ready, into `out` (emptied first),
    /// one entry a token. An interrupted wait returns nothing.
    pub(crate) fn wait(&self, timeout_ms: i32, out: &mut Vec<Polled>) -> io::Result<()> {
        out.clear();
        match self.wait_raw(timeout_ms, out) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
            done => done,
        }
    }

    fn wait_raw(&self, timeout_ms: i32, out: &mut Vec<Polled>) -> io::Result<()> {
        // SAFETY: zeroed kevents are valid, to be filled.
        let mut events: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
        let timeout = (timeout_ms >= 0).then(|| libc::timespec {
            tv_sec: libc::time_t::from(timeout_ms / 1000),
            tv_nsec: libc::c_long::from(timeout_ms % 1000) * 1_000_000,
        });
        let timeout_ptr = timeout.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        // SAFETY: `events` has room for 64; no changes are given.
        let n = unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                64,
                timeout_ptr,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        for event in &events[..n as usize] {
            let token = event.udata as u64;
            let index = match out.iter().position(|p| p.token == token) {
                Some(index) => index,
                None => {
                    out.push(Polled { token, ..Polled::default() });
                    out.len() - 1
                }
            };
            let polled = &mut out[index];
            match event.filter {
                libc::EVFILT_READ => polled.readable = true,
                libc::EVFILT_WRITE => polled.writable = true,
                _ => {}
            }
            if event.flags & (libc::EV_EOF | libc::EV_ERROR) != 0 {
                polled.closed = true;
            }
        }
        Ok(())
    }
}
