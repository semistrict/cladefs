// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The eventfds a front-end gives a ring, each checked to be one: its kicks, which the back-end
//! reads without ever waiting (`preadv2` with `RWF_NOWAIT`, Linux 5.8, as the front-end may clear
//! `O_NONBLOCK` on the file they share); its calls, which the back-end writes on the channel's own
//! thread, never one shared with other tenants, as the front-end can make a write wait for good;
//! and the back-end's own, which wakes that thread.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// Whether `fd` is an eventfd: as `/proc` names it, the one way to tell an anonymous inode's kind.
pub(crate) fn is_eventfd(fd: BorrowedFd<'_>) -> bool {
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    link.is_ok_and(|link| link.as_os_str() == "anon_inode:[eventfd]")
}

/// Reads `fd`'s count, an eventfd's, without waiting: whether it was signalled.
fn read_now(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut count = 0u64;
    let iov = libc::iovec { iov_base: (&raw mut count).cast(), iov_len: size_of::<u64>() };
    loop {
        // SAFETY: Reads 8 bytes into `count`, at the file's own offset (-1), which an eventfd
        // ignores, never waiting.
        let read = unsafe { libc::preadv2(fd.as_raw_fd(), &iov, 1, -1, libc::RWF_NOWAIT) };
        match read {
            8 => return Ok(true),
            -1 => {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock => return Ok(false),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(err),
                }
            }
            other => return Err(io::Error::other(format!("an eventfd read of {other} bytes"))),
        }
    }
}

/// A ring's kick, the front-end's eventfd.
#[derive(Debug)]
pub(crate) struct Kick(OwnedFd);

impl Kick {
    /// `fd`, if an eventfd.
    pub(crate) fn new(fd: OwnedFd) -> Option<Self> {
        is_eventfd(std::os::fd::AsFd::as_fd(&fd)).then_some(Self(fd))
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.0)
    }

    /// Hears the kicks since it last did, never waiting.
    pub(crate) fn hear(&self) -> io::Result<bool> {
        read_now(self.fd())
    }
}

/// A ring's call, or its error notifier: the front-end's eventfd.
#[derive(Debug)]
pub(crate) struct Call(OwnedFd);

impl Call {
    /// `fd`, if an eventfd.
    pub(crate) fn new(fd: OwnedFd) -> Option<Self> {
        is_eventfd(std::os::fd::AsFd::as_fd(&fd)).then_some(Self(fd))
    }

    /// Signals it, unless its count is as high as an eventfd's goes, which only the front-end
    /// makes it, its own interrupt made meaningless. On the channel's thread alone: the front-end
    /// may raise the count between the look and the write, which then waits until the thread is
    /// interrupted ([`crate::Channel::stop`]).
    pub(crate) fn signal(&self) {
        let fd = self.0.as_raw_fd();
        let mut poll = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
        // SAFETY: Polls one descriptor this holds, at once.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        if ready != 1 || poll.revents & libc::POLLOUT == 0 {
            return;
        }
        let one = 1u64;
        // SAFETY: Writes 8 bytes of `one`.
        let wrote = unsafe { libc::write(fd, (&raw const one).cast(), size_of::<u64>()) };
        if wrote != 8 {
            // Interrupted to stop, or the front-end's O_NONBLOCK at a full count: nothing to tell.
            log::debug!("vhost-user: a call not written: {}", io::Error::last_os_error());
        }
    }
}

/// The back-end's own eventfd, which wakes the channel's thread: never blocking, as no one else
/// holds it.
#[derive(Debug)]
pub(crate) struct Wake(OwnedFd);

impl Wake {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: Makes an eventfd, owned here.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Just made, owned here.
        let wake = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        // cladefs: kicks are read with `RWF_NOWAIT`, which a host before Linux 5.8 refuses for an
        // eventfd: refused here, at once, rather than every kick failing (AGENTS.md, no silent
        // fallbacks).
        read_now(wake.fd()).map_err(|err| {
            io::Error::new(err.kind(), format!("eventfds read with RWF_NOWAIT (Linux 5.8): {err}"))
        })?;
        Ok(wake)
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.0)
    }

    /// Wakes the thread.
    pub(crate) fn wake(&self) {
        let one = 1u64;
        // SAFETY: Writes 8 bytes of `one` to an eventfd of the back-end's, which never blocks.
        let wrote = unsafe { libc::write(self.0.as_raw_fd(), (&raw const one).cast(), 8) };
        // Its count full: the thread wakes whatever.
        debug_assert!(wrote == 8 || io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock);
    }

    /// Clears its wakes.
    pub(crate) fn clear(&self) {
        if let Err(err) = read_now(self.fd()) {
            log::error!("vhost-user: clearing the channel's wake: {err}");
        }
    }
}
