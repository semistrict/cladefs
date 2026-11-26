// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What libmachine needs of its host OS beyond std, the same on Linux and macOS: an event counter
//! a thread can wait on with others ([`EventFd`]), the wait itself ([`wait_readable`]), and
//! entropy ([`getrandom`]).
//!
//! On Linux an [`EventFd`] is an eventfd, which KVM signals (ioeventfd) and takes signals from
//! (irqfd) itself. macOS has none: there it is a pipe, which the HVF backend signals and reads in
//! KVM's place (`backend::hvf`).

use std::io;
use std::os::fd::RawFd;

#[cfg(target_os = "linux")]
pub(crate) use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

#[cfg(target_os = "macos")]
pub(crate) use pipe_event::{EFD_NONBLOCK, EventFd};

#[cfg(target_os = "macos")]
mod pipe_event {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;

    /// The one flag libmachine passes, as on Linux: reads never block. Every event here is so.
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
            let (read, write) =
                unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
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
                let wrote =
                    unsafe { libc::write(self.ends.1.as_raw_fd(), ONES.as_ptr().cast(), n) };
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
                let n = unsafe {
                    libc::read(self.ends.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                };
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
}

/// Waits until one of `fds` is readable, `timeout_ms` at most (-1: no limit), and returns which,
/// by their index in `fds`; none if the wait timed out. poll(2), the same on Linux and macOS:
/// libmachine's waiters watch a handful of descriptors each.
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

/// Fills `buf` from the kernel's random number generator.
pub(crate) fn getrandom(buf: &mut [u8]) {
    #[cfg(target_os = "linux")]
    {
        let mut rest = buf;
        while !rest.is_empty() {
            // SAFETY: writes at most `rest.len()` bytes into `rest`.
            let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
            if n > 0 {
                rest = &mut rest[n as usize..];
            } else if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                // getrandom cannot fail on a booted Linux; never hand out zeroes as entropy if it
                // somehow does.
                panic!("getrandom failed: {}", io::Error::last_os_error());
            }
        }
    }
    #[cfg(target_os = "macos")]
    for chunk in buf.chunks_mut(256) {
        // SAFETY: getentropy writes at most 256 bytes into `chunk`, which has that many at most.
        if unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } != 0 {
            panic!("getentropy failed: {}", io::Error::last_os_error());
        }
    }
}
