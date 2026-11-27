// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The channel's socket, the back-end's end of it: messages received as they come, never waiting,
//! however the front-end sends them (a byte at a time, or half a message and then nothing), each
//! header checked before its payload is read; and replies sent at once, the channel ending if the
//! front-end leaves them unread until its socket's buffer fills. The back-end's end is its own
//! file, nonblocking: the front-end holds the other.

#![deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use zerocopy::{FromBytes, IntoBytes};

use crate::protocol::{Header, MAX_FDS, Message, Refused};

/// Why the channel ends as it receives.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReceiveError {
    #[error("the front-end closed the channel")]
    Closed,
    #[error("{0}")]
    Refused(#[from] Refused),
    #[error("descriptors past a message's header, or more than {MAX_FDS}")]
    Fds,
    #[error("receiving: {0}")]
    Io(#[from] io::Error),
}

/// The back-end's end of the channel, and the message it is receiving.
pub(crate) struct Socket {
    fd: OwnedFd,
    /// The header's bytes so far, then the payload's.
    header: Vec<u8>,
    payload: Option<(Header, Vec<u8>)>,
    fds: Vec<OwnedFd>,
}

const HEADER: usize = size_of::<Header>();

impl Socket {
    /// The back-end's end `fd`, made nonblocking.
    pub(crate) fn new(fd: OwnedFd) -> io::Result<Self> {
        // SAFETY: Sets this end's own file's flags.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, header: Vec::with_capacity(HEADER), payload: None, fds: Vec::new() })
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.fd)
    }

    /// The next message, once received whole; `None` until then, all that came received.
    pub(crate) fn receive(&mut self) -> Result<Option<Message>, ReceiveError> {
        loop {
            match &mut self.payload {
                None => {
                    let want = HEADER.checked_sub(self.header.len()).expect("a header at most");
                    let first = self.header.is_empty();
                    let Some(bytes) = self.recv(want, first)? else { return Ok(None) };
                    self.header.extend_from_slice(&bytes);
                    if self.header.len() < HEADER {
                        continue;
                    }
                    let header = Header::read_from_bytes(&self.header).expect("a header's bytes");
                    self.header.clear();
                    header.check()?;
                    let size = usize::try_from(header.size).expect("at most MAX_PAYLOAD");
                    self.payload = Some((header, Vec::with_capacity(size)));
                }
                Some((header, payload)) => {
                    let size = usize::try_from(header.size).expect("at most MAX_PAYLOAD");
                    let want = size.checked_sub(payload.len()).expect("a payload at most");
                    if want > 0 {
                        let Some(bytes) = self.recv(want, false)? else { return Ok(None) };
                        let (_, payload) = self.payload.as_mut().expect("receiving a payload");
                        payload.extend_from_slice(&bytes);
                        continue;
                    }
                    let (header, payload) = self.payload.take().expect("receiving a payload");
                    let fds = std::mem::take(&mut self.fds);
                    return Ok(Some(Message { header, payload, fds }));
                }
            }
        }
    }

    /// Up to `want` bytes, one at least, with the descriptors they carry if `first`, a message's
    /// first bytes: `None` if none came yet. Descriptors anywhere else end the channel.
    fn recv(&mut self, want: usize, first: bool) -> Result<Option<Vec<u8>>, ReceiveError> {
        let mut bytes = vec![0u8; want];
        let mut iov = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: want };
        // Room for one more than a message carries, to tell more from enough.
        let space = cmsg_space(MAX_FDS.checked_add(1).expect("small"));
        let mut control = vec![0u64; space.div_ceil(8)];
        // SAFETY: A zeroed msghdr is a valid one to fill in.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let flags = libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC;
        // SAFETY: Receives into `bytes` and `control`, which `msg` describes.
        let got = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut msg, flags) };
        if got < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(err.into()),
            };
        }
        let fds = received_fds(&msg);
        if got == 0 {
            return Err(ReceiveError::Closed);
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 || (!first && !fds.is_empty()) {
            return Err(ReceiveError::Fds);
        }
        self.fds.extend(fds);
        if self.fds.len() > MAX_FDS {
            return Err(ReceiveError::Fds);
        }
        bytes.truncate(usize::try_from(got).expect("not negative"));
        Ok(Some(bytes))
    }

    /// Sends `header` and `payload`, a reply, at once: an error if the socket takes less, which
    /// ends the channel.
    pub(crate) fn send(&self, header: Header, payload: &[u8]) -> io::Result<()> {
        let mut message = header.as_bytes().to_vec();
        message.extend_from_slice(payload);
        let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
        // SAFETY: Sends the bytes of `message`.
        let sent = unsafe {
            libc::send(self.fd.as_raw_fd(), message.as_ptr().cast(), message.len(), flags)
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if usize::try_from(sent).ok() != Some(message.len()) {
            return Err(io::Error::other("the front-end leaves its replies unread"));
        }
        Ok(())
    }
}

/// The bytes of control data `fds` descriptors take.
fn cmsg_space(fds: usize) -> usize {
    let len =
        u32::try_from(fds.checked_mul(size_of::<libc::c_int>()).expect("small")).expect("small");
    // SAFETY: Pure arithmetic of the C library's.
    unsafe { libc::CMSG_SPACE(len) as usize }
}

/// The descriptors `msg` received, owned.
fn received_fds(msg: &libc::msghdr) -> Vec<OwnedFd> {
    let mut fds = Vec::new();
    // SAFETY: Walks the control data the kernel wrote into the buffer `msg` describes.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(msg) };
    while !cmsg.is_null() {
        // SAFETY: A header within the control data.
        let header = unsafe { &*cmsg };
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            // SAFETY: Pure arithmetic of the C library's.
            let data_len =
                (header.cmsg_len as usize).saturating_sub(unsafe { libc::CMSG_LEN(0) } as usize);
            let count = data_len.checked_div(size_of::<libc::c_int>()).expect("not zero");
            // SAFETY: The descriptors the header carries, `count` of them, unaligned.
            let data = unsafe { libc::CMSG_DATA(cmsg) }.cast::<libc::c_int>();
            for i in 0..count {
                // SAFETY: Within the header's data; each a descriptor this process now owns.
                let fd = unsafe { data.add(i).read_unaligned() };
                // SAFETY: As above.
                fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
        // SAFETY: The next header within the control data, or null.
        cmsg = unsafe { libc::CMSG_NXTHDR(msg, cmsg) };
    }
    fds
}
