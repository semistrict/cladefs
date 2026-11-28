// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Board 14663: doc-4's stream transport, the guest's data requests over vsock, which the daemon
//! serves itself: a test plays the guest's driver and its VMM's vsock device, connecting to the
//! listener the VM process hands the daemon. A guest's reads, writes, commits, frees, root and info
//! served end to end by a real daemon's cell, replies tagged as their requests, several in flight;
//! the window's and the device's state's requests refused; a stream past the protocol's bounds
//! closed, the daemon serving on, and a guest connecting again served; a listener that is none
//! refused, and a second refused; and every stream shut down as the connection ends.

use super::{BLOCK, connect, host};
use crate::client::CellClient;
use cladefs_core::Errno;
use cladefs_core::extent_requests::protocol as doc4;
use doc4::{
    CommitArgs, ExtentIdLe, Info, IoArgs, RequestHeader, Response, RootLen, StreamReply,
    StreamRequest,
};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread::JoinHandle;
use zerocopy::{FromBytes, Immutable, IntoBytes};

const RESPONSE: usize = size_of::<Response>();

/// A daemon's connection to a cell, and the listener its guest's streams come to.
struct Rig {
    cell: CellClient,
    served: JoinHandle<()>,
    /// Where the guest's streams connect, as a VMM's vsock device does.
    listener: PathBuf,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        host();
        let dir = tempfile::tempdir().unwrap();
        let (cell, served) = connect(&dir.path().join("store"), 64 << 20);
        let listener = dir.path().join("vsock.sock_5000");
        let bound = UnixListener::bind(&listener).unwrap();
        cell.guest_stream(false, bound.as_fd()).unwrap();
        // The daemon holds its own: the VM process's closes, as a VMM's does once handed over.
        drop(bound);
        Self { cell, served, listener, _dir: dir }
    }

    /// A guest's stream, connected.
    fn stream(&self) -> UnixStream {
        UnixStream::connect(&self.listener).unwrap()
    }

    /// Ends the connection, and checks the daemon served it to its end.
    fn finish(self) {
        drop(self.cell);
        self.served.join().unwrap();
    }
}

/// A doc-4 request's bytes: header, arguments, data.
fn bytes<T: IntoBytes + Immutable>(
    type_: u32,
    flags: u32,
    args: Option<T>,
    data: &[u8],
) -> Vec<u8> {
    let header = RequestHeader { type_: type_.to_le(), flags: flags.to_le(), unique: 7 };
    let mut bytes = header.as_bytes().to_vec();
    if let Some(args) = &args {
        bytes.extend_from_slice(args.as_bytes());
    }
    bytes.extend_from_slice(data);
    bytes
}

fn io(cell: u64, sequence: u64, offset: u64, len: usize) -> IoArgs {
    IoArgs {
        id: ExtentIdLe { cell: cell.to_le(), sequence: sequence.to_le() },
        offset: offset.to_le(),
        len: (len as u32).to_le(),
        root_len: 0,
    }
}

fn errno(errno: Errno) -> u32 {
    errno.raw().cast_unsigned()
}

/// Sends the request of `readable` bytes, with `writable` for the device, tagged `tag`.
fn send(stream: &mut UnixStream, tag: u64, readable: &[u8], writable: usize) {
    let head = StreamRequest {
        tag: tag.to_le(),
        readable: (readable.len() as u32).to_le(),
        writable: (writable as u32).to_le(),
    };
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(readable).unwrap();
}

/// The next reply: its tag, what the device wrote, the response apart, and its status.
fn reply(stream: &mut UnixStream) -> (u64, Vec<u8>, u32) {
    let mut head = StreamReply::default();
    stream.read_exact(head.as_mut_bytes()).unwrap();
    let mut written = vec![0u8; u32::from_le(head.written) as usize];
    stream.read_exact(&mut written).unwrap();
    let response = Response::read_from_bytes(&written[written.len() - RESPONSE..]).unwrap();
    written.truncate(written.len() - RESPONSE);
    (u64::from_le(head.tag), written, u32::from_le(response.status))
}

/// Serves one request on `stream`: what the device wrote, the response apart, and its status.
fn request(stream: &mut UnixStream, readable: &[u8], writable: usize) -> (Vec<u8>, u32) {
    send(stream, 1, readable, writable);
    let (tag, written, status) = reply(stream);
    assert_eq!(tag, 1);
    assert_eq!(written.len(), writable - RESPONSE, "the device wrote part of what it writes");
    (written, status)
}

/// A guest's data requests on its stream, served by a daemon's cell end to end: info; writes and
/// reads; a commit setting the root, and the root read; a write committed in one request; frees;
/// several requests in flight at once, each reply tagged as its request; and the requests the
/// stream doesn't take refused, the stream serving on.
#[test]
fn a_guests_data_requests_are_served_on_its_stream() {
    let rig = Rig::new();
    let cell = rig.cell.cell();
    let mut stream = rig.stream();

    let info = bytes::<()>(doc4::VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    let (written, status) = request(&mut stream, &info, size_of::<Info>() + RESPONSE);
    assert_eq!(status, 0);
    let info = Info::read_from_bytes(&written).unwrap();
    assert_eq!((u64::from_le(info.cell), u64::from_le(info.capacity)), (cell, 64 << 20));

    let data: Vec<u8> = (0..2 * BLOCK).map(|i| (i % 251) as u8).collect();
    let write = bytes(doc4::VIRTIO_EXTENT_T_WRITE, 0, Some(io(cell, 1, 0, data.len())), &data);
    assert_eq!(request(&mut stream, &write, RESPONSE), (vec![], 0));
    let read = bytes(doc4::VIRTIO_EXTENT_T_READ, 0, Some(io(cell, 1, 0, data.len())), &[]);
    assert_eq!(request(&mut stream, &read, data.len() + RESPONSE), (data.clone(), 0));
    // A commit setting the root, its write sealed: written again, refused.
    let root = b"the root".to_vec();
    let commit_args = CommitArgs { root_len: (root.len() as u32).to_le(), reserved: 0 };
    let commit = bytes(
        doc4::VIRTIO_EXTENT_T_COMMIT,
        doc4::VIRTIO_EXTENT_COMMIT_UPLOADED | doc4::VIRTIO_EXTENT_COMMIT_ROOT,
        Some(commit_args),
        &root,
    );
    assert_eq!(request(&mut stream, &commit, RESPONSE), (vec![], 0));
    let sealed = bytes(doc4::VIRTIO_EXTENT_T_WRITE, 0, Some(io(cell, 1, 0, BLOCK)), &data[..BLOCK]);
    assert_eq!(request(&mut stream, &sealed, RESPONSE).1, errno(Errno::EPERM));
    let read_root = bytes::<()>(doc4::VIRTIO_EXTENT_T_ROOT, 0, None, &[]);
    let returned = doc4::MAX_ROOT as usize + size_of::<RootLen>();
    let (written, status) = request(&mut stream, &read_root, returned + RESPONSE);
    assert_eq!(status, 0);
    let len = RootLen::read_from_bytes(&written[doc4::MAX_ROOT as usize..]).unwrap();
    assert_eq!(&written[..u32::from_le(len.len) as usize], &root[..]);
    // The cell holds it, as the socket reads it.
    let mut back = vec![0u8; data.len()];
    let extent = cladefs_core::ExtentId { cell, sequence: 1 };
    rig.cell.read(extent, 0, &mut [cladefs_core::VolatileSliceMut::from(&mut back[..])]).unwrap();
    assert_eq!(back, data);
    // A write committed in one request; freed, then not there.
    let committed = bytes(
        doc4::VIRTIO_EXTENT_T_WRITE,
        doc4::VIRTIO_EXTENT_WRITE_COMMIT,
        Some(io(cell, 2, 0, BLOCK)),
        &data[..BLOCK],
    );
    assert_eq!(request(&mut stream, &committed, RESPONSE), (vec![], 0));
    let id = ExtentIdLe { cell: cell.to_le(), sequence: 2u64.to_le() };
    let free = bytes(doc4::VIRTIO_EXTENT_T_FREE, 0, Some(id), &[]);
    assert_eq!(request(&mut stream, &free, RESPONSE), (vec![], 0));
    let freed = bytes(doc4::VIRTIO_EXTENT_T_READ, 0, Some(io(cell, 2, 0, BLOCK)), &[]);
    assert_eq!(request(&mut stream, &freed, BLOCK + RESPONSE).1, errno(Errno::ENOENT));

    // Many reads in flight at once, each reply tagged as its request, whatever their order.
    const IN_FLIGHT: u64 = 64;
    for tag in 0..IN_FLIGHT {
        send(&mut stream, 100 + tag, &read, data.len() + RESPONSE);
    }
    let mut tags: Vec<u64> = (0..IN_FLIGHT)
        .map(|_| {
            let (tag, written, status) = reply(&mut stream);
            assert_eq!((written == data, status), (true, 0), "tag {tag}");
            tag
        })
        .collect();
    tags.sort_unstable();
    assert_eq!(tags, (100..100 + IN_FLIGHT).collect::<Vec<_>>());

    // The window's, the device's state's and no type's requests refused: its VMM's device serves
    // the first two on its queues.
    let map = bytes(
        doc4::VIRTIO_EXTENT_T_MAP,
        0,
        Some(doc4::MapArgs {
            id: ExtentIdLe { cell: cell.to_le(), sequence: 1u64.to_le() },
            offset: 0,
            len: (BLOCK as u64).to_le(),
            window_offset: 0,
        }),
        &[],
    );
    let freeze =
        bytes::<()>(doc4::VIRTIO_EXTENT_T_FREEZE, doc4::VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    let resized = bytes(
        doc4::VIRTIO_EXTENT_T_RESIZED,
        0,
        Some(doc4::ResizedArgs { capacity: (32u64 << 20).to_le() }),
        &[],
    );
    let unknown = bytes::<()>(99, 0, None, &[]);
    for refused in [map, freeze, resized, unknown] {
        assert_eq!(request(&mut stream, &refused, RESPONSE), (vec![], errno(Errno::EINVAL)));
    }
    // Served on still.
    assert_eq!(request(&mut stream, &read, data.len() + RESPONSE), (data, 0));
    rig.finish();
}

/// A stream past the protocol's bounds is closed, as nothing after its request could be read in
/// step: one of more bytes to read than any request has, of more to write than any returns, or of
/// no room for its response. The daemon serves on, a guest connecting again served.
#[test]
fn streams_past_the_protocols_bounds_are_closed() {
    let rig = Rig::new();
    let info = bytes::<()>(doc4::VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    let past = [
        (doc4::MAX_READABLE + 1, RESPONSE),
        (info.len(), doc4::MAX_WRITABLE + 1),
        (info.len(), RESPONSE - 1),
        (size_of::<RequestHeader>() - 1, RESPONSE),
    ];
    for (readable, writable) in past {
        let mut stream = rig.stream();
        let head = StreamRequest {
            tag: 1,
            readable: (readable as u32).to_le(),
            writable: (writable as u32).to_le(),
        };
        stream.write_all(head.as_bytes()).unwrap();
        let mut rest = Vec::new();
        assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0, "{readable} {writable}");
    }
    // A guest connecting again is served.
    let mut stream = rig.stream();
    assert_eq!(request(&mut stream, &info, size_of::<Info>() + RESPONSE).1, 0);
    rig.finish();
}

/// A listener that is no listening Unix stream socket is refused, as is a second one, and a
/// stream before the cell is open; and every stream is shut down as its connection ends.
#[test]
fn listeners_are_checked_and_stop_with_their_connection() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cell, served) = connect(&dir.path().join("store"), 64 << 20);
    // Neither listening, nor a stream's.
    let (unconnected, _) = UnixStream::pair().unwrap();
    assert_eq!(cell.guest_stream(false, unconnected.as_fd()).err(), Some(Errno::EINVAL));
    let datagram = std::os::unix::net::UnixDatagram::unbound().unwrap();
    assert_eq!(cell.guest_stream(false, datagram.as_fd()).err(), Some(Errno::EINVAL));
    let path = dir.path().join("vsock.sock_5000");
    let bound = UnixListener::bind(&path).unwrap();
    cell.guest_stream(true, bound.as_fd()).unwrap();
    let again = UnixListener::bind(dir.path().join("again")).unwrap();
    assert_eq!(cell.guest_stream(false, again.as_fd()).err(), Some(Errno::EEXIST));
    drop(bound);
    let mut stream = UnixStream::connect(&path).unwrap();
    let info = bytes::<()>(doc4::VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    assert_eq!(request(&mut stream, &info, size_of::<Info>() + RESPONSE).1, 0);
    // The connection ends: its stream is shut down, and its listener accepts no more.
    drop(cell);
    served.join().unwrap();
    let mut rest = Vec::new();
    assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0);
    let refused = UnixStream::connect(&path).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused, "{refused}");
}
