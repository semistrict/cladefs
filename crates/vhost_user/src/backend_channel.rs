// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The back-end channel (`VHOST_USER_PROTOCOL_F_BACKEND_REQ`, cladefs board 14603): the socket the
//! front-end gives the back-end (`SET_BACKEND_REQ_FD`) to send its own requests on, here the
//! device's configuration changes (`BACKEND_CONFIG_CHANGE_MSG`).
//!
//! Sent without ever waiting, on the channel's thread alone: a front-end that leaves the channel
//! unread fills its socket, after which the request yet to go waits for room, the thread serving
//! on, and the changes meanwhile are said by that one request, which has the guest read the whole
//! configuration again. So the back-end holds one request at most, whatever the front-end does;
//! and it asks for no reply (`NEED_REPLY`), which it would have to wait for. doc-32 asks the same of
//! every back-end request.

#![deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use zerocopy::IntoBytes;

use crate::protocol::{BACKEND_CONFIG_CHANGE_MSG, Header, VERSION};

/// The back-end channel: the back-end's end, and the request yet to go, if any.
pub(crate) struct BackendChannel {
    fd: OwnedFd,
    /// What is left to send of the request under way: empty if none is.
    pending: Vec<u8>,
}

impl BackendChannel {
    /// The back-end channel whose end `fd` is: `None` unless it is a Unix stream socket's, as the
    /// front-end's could be any descriptor at all, a network socket's among them.
    pub(crate) fn new(fd: OwnedFd) -> Option<Self> {
        let option = |name| {
            let mut value: libc::c_int = 0;
            let mut len = size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: Reads an integer socket option of `fd` into `value`, `len` its size.
            let got = unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    name,
                    (&raw mut value).cast(),
                    &mut len,
                )
            };
            (got == 0).then_some(value)
        };
        // Not a socket, getsockopt fails (ENOTSOCK).
        if option(libc::SO_DOMAIN)? != libc::AF_UNIX || option(libc::SO_TYPE)? != libc::SOCK_STREAM
        {
            return None;
        }
        Some(Self { fd, pending: Vec::new() })
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The device's configuration changed: a `BACKEND_CONFIG_CHANGE_MSG` goes, unless one is under
    /// way still, which says it as well, the guest reading the configuration as it is then.
    pub(crate) fn config_changed(&mut self) {
        if self.pending.is_empty() {
            let header = Header { request: BACKEND_CONFIG_CHANGE_MSG, flags: VERSION, size: 0 };
            self.pending = header.as_bytes().to_vec();
        }
    }

    /// Sends what is left of the request under way, never waiting: whether it is all gone, or the
    /// front-end's socket is full; an error if the channel is gone.
    pub(crate) fn flush(&mut self) -> io::Result<bool> {
        while !self.pending.is_empty() {
            let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
            // SAFETY: Sends the bytes of `pending`.
            let sent = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    self.pending.as_ptr().cast(),
                    self.pending.len(),
                    flags,
                )
            };
            if sent < 0 {
                let err = io::Error::last_os_error();
                return match err.kind() {
                    io::ErrorKind::WouldBlock => Ok(false),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(err),
                };
            }
            let sent = usize::try_from(sent).expect("not negative").min(self.pending.len());
            self.pending.drain(..sent);
        }
        Ok(true)
    }
}
