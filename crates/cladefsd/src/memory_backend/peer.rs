// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The VM process at the other end of a memory backend's socket, by a pidfd taken as it
//! connected (`SO_PEERPIDFD`): what reads its memory names it by its PID, which, the process gone,
//! another may take; so each such read is checked after, against the pidfd, which names it alone,
//! and a read of a process gone is thrown away.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// `SO_PEERPIDFD` (`include/uapi/asm-generic/socket.h`).
const SO_PEERPIDFD: libc::c_int = 77;

/// A VM process, connected.
pub(super) struct Peer {
    pid: libc::pid_t,
    pidfd: OwnedFd,
}

impl Peer {
    /// The process that connected `stream`.
    pub fn of(stream: &UnixStream) -> std::io::Result<Self> {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        getsockopt(stream, libc::SO_PEERCRED, &mut cred)?;
        let mut pidfd: libc::c_int = -1;
        getsockopt(stream, SO_PEERPIDFD, &mut pidfd)?;
        // SAFETY: The descriptor the kernel just made, this process's alone.
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
        let peer = Self { pid: cred.pid, pidfd };
        peer.check()?;
        Ok(peer)
    }

    /// It again: its own pidfd, for another thread.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self { pid: self.pid, pidfd: self.pidfd.try_clone()? })
    }

    /// Fails if the process is gone (`ESRCH`): what was read of its PID since may be another's.
    /// A zombie holds its PID still, which no other takes.
    pub fn check(&self) -> std::io::Result<()> {
        self.signal(0)
    }

    /// Kills it, through its pidfd, which a PID recycled doesn't fool.
    pub fn kill(&self) -> std::io::Result<()> {
        self.signal(libc::SIGKILL)
    }

    fn signal(&self, signal: libc::c_int) -> std::io::Result<()> {
        // SAFETY: pidfd_send_signal, of `signal` (none only checks), on the pidfd it holds.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Its pagemap, opened, checked to be its own.
    pub fn pagemap(&self) -> std::io::Result<File> {
        let pagemap = File::open(format!("/proc/{}/pagemap", self.pid))?;
        self.check()?;
        Ok(pagemap)
    }

    /// Reads its memory at `addr` into `buf`, all of it, checked to be its own.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let local = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let remote = libc::iovec { iov_base: addr as *mut libc::c_void, iov_len: buf.len() };
        // SAFETY: Reads into `buf`, which outlives the call, from the process's memory.
        let read = unsafe { libc::process_vm_readv(self.pid, &local, 1, &remote, 1, 0) };
        if read < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if read as usize != buf.len() {
            return Err(std::io::Error::other(format!(
                "{read} bytes of {} read at {addr:#x}",
                buf.len()
            )));
        }
        self.check()
    }
}

fn getsockopt<T>(stream: &UnixStream, option: libc::c_int, value: &mut T) -> std::io::Result<()> {
    let mut len = size_of::<T>() as libc::socklen_t;
    // SAFETY: Fills `value`, as large as `len` says, from the socket's option.
    let got = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (value as *mut T).cast(),
            &mut len,
        )
    };
    if got < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len as usize != size_of::<T>() {
        return Err(std::io::Error::other(format!("socket option {option}: {len} bytes")));
    }
    Ok(())
}
