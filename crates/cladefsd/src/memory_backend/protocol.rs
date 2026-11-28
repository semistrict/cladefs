// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The `Mappings` memory backend's protocol, as Firecracker speaks it
//! (vendor/firecracker/src/vmm/src/memory_backend.rs): frames of a little-endian `u32` length and
//! that many bytes of JSON, a frame's file descriptors attached to its first bytes
//! (`SCM_RIGHTS`). What the VM process sends is untrusted: checked as it is used.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

/// A message of the protocol's, one way or the other.
pub(super) trait Message: Serialize + DeserializeOwned {
    /// The most bytes its frame's JSON may have.
    const MAX: usize;
}

/// The most file descriptors a frame may carry.
pub(super) const MAX_FDS: usize = 64;

/// A region the VM's memory backend serves, as Firecracker's handshake describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct GuestRegion {
    /// Where its host memory starts in the VM process.
    pub base_host_virt_addr: u64,
    /// Its size.
    pub size: usize,
    /// Where it starts in the backend's layout: guest memory's regions laid out one after
    /// another, as a memory file is, then the pmem devices'.
    pub offset: u64,
    /// Its page size.
    pub page_size: usize,
    /// Of a pmem device's memory, the device's ID; none of guest memory's.
    #[serde(default)]
    pub pmem_id: Option<String>,
}

/// What Firecracker sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum FromVmm {
    /// The regions, with the userfaultfd attached.
    Handshake { regions: Vec<GuestRegion> },
    /// A map applied.
    Mapped { id: u64 },
    /// A map refused, nothing of it applied.
    MapRefused { id: u64, error: String },
    /// Device state saved, the vCPUs paused: persist guest memory.
    Persist,
}

/// What the VM process sends is small: its regions, and answers. A frame past this is refused, as
/// it is read, so that a VM process can't make the daemon hold much of its making.
impl Message for FromVmm {
    const MAX: usize = 1 << 20;
}

/// What the handler sends Firecracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ToVmm {
    /// Map `entries` over region `region`.
    Map { id: u64, region: usize, entries: Vec<Entry> },
    /// Every region mapped: the VM may start.
    Ready,
    /// Guest memory persisted, as `result` says.
    Persisted { result: serde_json::Value },
}

/// A map has an entry for each run of an image node: many, of a fragmented one.
impl Message for ToVmm {
    const MAX: usize = 64 << 20;
}

/// A mapping of `len` bytes at `offset` in a region, both whole host pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Entry {
    /// The frame's `fd`th descriptor's file from `file_offset`, mapped private.
    File { fd: usize, file_offset: u64, offset: u64, len: u64 },
    /// Anonymous memory.
    Zeros { offset: u64, len: u64 },
    /// Nothing: an access fails.
    None { offset: u64, len: u64 },
    /// Anonymous memory whose first touch faults here: mapped anew then.
    Lazy { offset: u64, len: u64 },
}

/// A frame's error: the socket's, the frame's, or the peer gone.
#[derive(Debug)]
pub(super) enum FrameError {
    Io(std::io::Error),
    Size(usize),
    Message(serde_json::Error),
    Gone,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(err) => write!(f, "the memory backend's socket: {err}"),
            FrameError::Size(len) => write!(f, "a frame of {len} bytes, past the most"),
            FrameError::Message(err) => write!(f, "a message not understood: {err}"),
            FrameError::Gone => write!(f, "the VM process went"),
        }
    }
}

/// Sends `message` as a frame, with `fds` attached.
pub(super) fn send<M: Message>(
    stream: &UnixStream,
    message: &M,
    fds: &[RawFd],
) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message).map_err(FrameError::Message)?;
    if body.len() > M::MAX {
        return Err(FrameError::Size(body.len()));
    }
    let header = body.len() as u32;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&header.to_le_bytes());
    frame.extend_from_slice(&body);
    let sent = send_with_fds(stream, &frame, fds).map_err(FrameError::Io)?;
    // The descriptors went with the first bytes: the rest go as a stream's do.
    (&*stream).write_all(&frame[sent..]).map_err(FrameError::Io)
}

/// Receives a frame: its message, and the descriptors attached to it.
pub(super) fn recv<M: Message>(stream: &UnixStream) -> Result<(M, Vec<File>), FrameError> {
    let mut header = [0u8; 4];
    let (read, fds) = recv_with_fds(stream, &mut header).map_err(FrameError::Io)?;
    if read == 0 {
        return Err(FrameError::Gone);
    }
    (&*stream).read_exact(&mut header[read..]).map_err(eof_gone)?;
    let len = u32::from_le_bytes(header) as usize;
    if len > M::MAX {
        return Err(FrameError::Size(len));
    }
    let mut body = vec![0u8; len];
    (&*stream).read_exact(&mut body).map_err(eof_gone)?;
    let message = serde_json::from_slice(&body).map_err(FrameError::Message)?;
    Ok((message, fds))
}

fn eof_gone(err: std::io::Error) -> FrameError {
    match err.kind() {
        std::io::ErrorKind::UnexpectedEof => FrameError::Gone,
        _ => FrameError::Io(err),
    }
}

/// `sendmsg` of `bytes` with `fds` attached: how many bytes went.
fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> std::io::Result<usize> {
    assert!(fds.len() <= MAX_FDS, "{} descriptors in a frame", fds.len());
    let mut iov =
        libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    // SAFETY: The size of the control message for the descriptors, computed by the C macro.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: A zeroed msghdr, filled in below with buffers that outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: The control buffer holds one message of the descriptors' size, as computed.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
        }
    }
    // SAFETY: Sends the message built above on the stream's socket.
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(sent as usize)
}

/// `recvmsg` into `buf`: how many bytes came, and the descriptors attached, each this process's.
fn recv_with_fds(stream: &UnixStream, buf: &mut [u8]) -> std::io::Result<(usize, Vec<File>)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: The size of a control message of MAX_FDS descriptors, computed by the C macro.
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: A zeroed msghdr, filled in below with buffers that outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: Receives into the buffers above; descriptors close on exec.
    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if read < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: Walks the control messages the kernel wrote, within their lengths.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let header = libc::CMSG_LEN(0) as usize;
                let count = ((*cmsg).cmsg_len as usize - header) / size_of::<RawFd>();
                for at in 0..count {
                    fds.push(File::from_raw_fd(data.add(at).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((read as usize, fds))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Messages read as Firecracker writes them, and a frame's descriptors come with it.
    #[test]
    fn frames_carry_messages_and_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let handshake: FromVmm = serde_json::from_str(
            r#"{"type": "handshake", "regions": [{"base_host_virt_addr": 4096, "size": 8192,
                "offset": 0, "page_size": 4096, "page_size_kib": 4096},
                {"base_host_virt_addr": 65536, "size": 4096, "offset": 8192, "page_size": 4096,
                "page_size_kib": 4096, "pmem_id": "window"}]}"#,
        )
        .unwrap();
        let FromVmm::Handshake { regions } = &handshake else { panic!("{handshake:?}") };
        assert_eq!(regions[1].pmem_id.as_deref(), Some("window"));
        let file = tempfile::tempfile().unwrap();
        let map =
            ToVmm::Map { id: 3, region: 0, entries: vec![Entry::Lazy { offset: 0, len: 8192 }] };
        send(&a, &map, &[file.as_raw_fd()]).unwrap();
        let (got, fds) = recv::<ToVmm>(&b).unwrap();
        assert_eq!(got, map);
        assert_eq!(fds.len(), 1);
        assert_eq!(
            serde_json::to_value(&map).unwrap(),
            serde_json::json!({"type": "map", "id": 3, "region": 0,
                "entries": [{"kind": "lazy", "offset": 0, "len": 8192}]})
        );
        drop(a);
        assert!(matches!(recv::<ToVmm>(&b), Err(FrameError::Gone)));
    }
}
