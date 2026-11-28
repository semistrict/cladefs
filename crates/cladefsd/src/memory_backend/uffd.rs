// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The userfaultfd a VM process hands its memory backend: the faults it reads, and the pages it
//! installs, each by an ioctl on the descriptor (`include/uapi/linux/userfaultfd.h`).

use std::fs::File;
use std::os::fd::AsRawFd;

const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_PAGEFAULT_FLAG_MINOR: u64 = 1 << 2;

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (0xaa << 8) | nr
}
const IOC_READ: u64 = 2;
const IOC_READ_WRITE: u64 = 3;
const UFFDIO_WAKE: u64 = ioc(IOC_READ, 0x02, size_of::<Range>());
const UFFDIO_COPY: u64 = ioc(IOC_READ_WRITE, 0x03, size_of::<Copy>());
const UFFDIO_CONTINUE: u64 = ioc(IOC_READ_WRITE, 0x07, size_of::<Continue>());

#[repr(C)]
#[derive(Clone, Copy)]
struct Range {
    start: u64,
    len: u64,
}

#[repr(C)]
struct Copy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

#[repr(C)]
struct Continue {
    range: Range,
    mode: u64,
    mapped: i64,
}

/// `struct uffd_msg`, of a page fault.
#[repr(C)]
#[derive(Default)]
struct Msg {
    event: u8,
    reserved1: u8,
    reserved2: u16,
    reserved3: u32,
    flags: u64,
    address: u64,
    ptid: u32,
    reserved4: u32,
}

const _: () = assert!(size_of::<Msg>() == 32);

/// A page fault: where, and whether minor (the page in the file, not in the page table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fault {
    pub address: u64,
    pub minor: bool,
}

/// A VM process's userfaultfd, handed over.
pub(super) struct Uffd(File);

impl Uffd {
    pub fn new(file: File) -> Self {
        Self(file)
    }

    /// The next page fault, waiting up to `timeout_ms` for one: none if none came, or the event
    /// was another's. Firecracker requests no events but faults.
    ///
    /// The descriptor's file is the VM process's too, which may make it blocking: a poll of it
    /// fails then, at once, which ends the VM; and the read waits for nothing, whatever its flags
    /// (`RWF_NOWAIT`), so that no read waits for ever on a VM process gone, holding its memory.
    pub fn next(&self, timeout_ms: i32) -> std::io::Result<Option<Fault>> {
        let mut poll = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: Polls the one descriptor it is given.
        match unsafe { libc::poll(&mut poll, 1, timeout_ms) } {
            0 => return Ok(None),
            polled if polled < 0 => {
                let err = std::io::Error::last_os_error();
                return match err.kind() {
                    std::io::ErrorKind::Interrupted => Ok(None),
                    _ => Err(err),
                };
            }
            _ => {}
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::other("the userfaultfd polls as an error: made blocking"));
        }
        let mut msg = Msg::default();
        let mut iov =
            libc::iovec { iov_base: (&mut msg as *mut Msg).cast(), iov_len: size_of::<Msg>() };
        // SAFETY: Reads one message, plain data, as large as the kernel writes one, at once.
        let read = unsafe { libc::preadv2(self.0.as_raw_fd(), &mut iov, 1, -1, libc::RWF_NOWAIT) };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            return match err.kind() {
                // Woken before it was read, or read by the VM process.
                std::io::ErrorKind::WouldBlock => Ok(None),
                _ => Err(err),
            };
        }
        if read as usize != size_of::<Msg>() {
            return Ok(None);
        }
        if msg.event != UFFD_EVENT_PAGEFAULT {
            return Ok(None);
        }
        Ok(Some(Fault { address: msg.address, minor: msg.flags & UFFD_PAGEFAULT_FLAG_MINOR != 0 }))
    }

    /// Installs the file's pages at `start`, `len` bytes, read-only in a private mapping (a store
    /// copies them in the kernel), waking what waits on them: `EEXIST` of a page there already.
    pub fn continue_(&self, start: u64, len: u64) -> std::io::Result<()> {
        let mut arg = Continue { range: Range { start, len }, mode: 0, mapped: 0 };
        self.ioctl(UFFDIO_CONTINUE, (&mut arg as *mut Continue).cast())
    }

    /// Installs a private copy of `bytes` at `start`, waking what waits on it.
    pub fn copy(&self, start: u64, bytes: &[u8]) -> std::io::Result<()> {
        let mut arg = Copy {
            dst: start,
            src: bytes.as_ptr() as u64,
            len: bytes.len() as u64,
            mode: 0,
            copy: 0,
        };
        self.ioctl(UFFDIO_COPY, (&mut arg as *mut Copy).cast())
    }

    /// Wakes what faulted on the `len` bytes at `start`, which retries its access.
    pub fn wake(&self, start: u64, len: u64) -> std::io::Result<()> {
        let mut arg = Range { start, len };
        self.ioctl(UFFDIO_WAKE, (&mut arg as *mut Range).cast())
    }

    fn ioctl(&self, request: u64, arg: *mut libc::c_void) -> std::io::Result<()> {
        // SAFETY: An ioctl of the userfaultfd's, with the argument its request takes.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), request as _, arg) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
