// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Board 14537: the extent device's vhost-user back-end, against a minimal front-end
//! (`vhost_user::front_end`) that plays a stock VMM with memfd guest RAM and the guest's driver:
//! a guest's reads, writes, commits, frees and info served end to end by a real daemon's cell; a
//! hostile front-end's and a hostile guest's every malformed message and ring refused, at the
//! edges, nothing panicking nor hanging, the daemon serving on; the serving time per request; and
//! (board 14603) the cell's capacity changes and the daemon's asks for stats told the guest on the
//! back-end channel, which a front-end leaving it unread stalls nothing with.

use super::{BLOCK, connect, host};
use crate::client::CellClient;
use cladefs_core::Errno;
use cladefs_core::extent_requests::protocol as doc4;
use doc4::{
    CommitArgs, ConfigSpace, ExtentIdLe, Info, IoArgs, MAX_WRITABLE, RequestHeader, ResizedArgs,
    Response, RootLen, StatsArgs,
};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vhost_user::front_end::{
    FrontEnd, GuestMemory, Queue, QueueLayout, Returned, WAIT, backend_request, eventfd, mem_table,
    wait_signal,
};
use vhost_user::protocol::*;
use zerocopy::{FromBytes, Immutable, IntoBytes};

const RESPONSE: usize = size_of::<Response>();
/// A queue's buffers in guest RAM: room for a chain one byte past the protocol's bounds each way.
const BUFFERS: u64 =
    ((doc4::MAX_READABLE + doc4::MAX_WRITABLE) as u64 + (1 << 20)).next_multiple_of(1 << 20);

/// A daemon's connection to a cell, its vhost-user channel, and the front-end of it.
struct Rig {
    cell: CellClient,
    /// The cell's directory, which the control API names it by.
    path: PathBuf,
    served: JoinHandle<()>,
    front: FrontEnd,
    memory: Arc<GuestMemory>,
    _dir: tempfile::TempDir,
}

impl Rig {
    /// A cell's connection and the front-end of its device's channel, of `queues` queues, not
    /// set up.
    fn new(queues: u16) -> Self {
        host();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let (cell, served) = connect(&path, 64 << 20);
        let channel = cell.vhost_user(queues, false, "fxfs").unwrap();
        let memory = GuestMemory::new(u64::from(queues) * ((1 << 20) + BUFFERS)).unwrap();
        let (front, memory) = (FrontEnd::new(channel), Arc::new(memory));
        Self { cell, path, served, front, memory, _dir: dir }
    }

    /// The layouts of `queues` queues.
    fn layouts(queues: u16) -> Vec<QueueLayout> {
        (0..u64::from(queues)).map(|index| QueueLayout::nth(index, 256, BUFFERS)).collect()
    }

    /// Sets the device up as a stock front-end does: its queues.
    fn set_up(&self, queues: u16) -> Vec<Queue> {
        self.front.set_up(&self.memory, &Self::layouts(queues)).unwrap()
    }

    /// The device's configuration, as the guest reads it.
    fn config(&self) -> ConfigSpace {
        let len = size_of::<ConfigSpace>() as u32;
        let mut payload = ConfigHeader { offset: 0, size: len, flags: 0 }.as_bytes().to_vec();
        payload.resize(payload.len() + len as usize, 0);
        let reply = self.front.get(GET_CONFIG, &payload).unwrap();
        ConfigSpace::read_from_bytes(&reply[size_of::<ConfigHeader>()..]).unwrap()
    }

    /// Ends the connection, and checks the daemon served it to its end: nothing panicked, its
    /// channel's thread neither, which ended with the connection.
    fn finish(self) {
        drop(self.front);
        drop(self.cell);
        self.served.join().unwrap();
        assert_eq!(vhost_user::panics(), 0, "a channel's thread panicked");
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

/// Serves a request of `readable` bytes, with `writable` bytes for the device, in descriptors of
/// `split` bytes at most: what the device wrote, the response apart, and its status.
fn request(queue: &mut Queue, readable: &[u8], writable: usize, split: usize) -> (Vec<u8>, u32) {
    let Returned { len, mut written } =
        queue.request(readable, writable, split).expect("the request came back");
    assert_eq!(len as usize, writable, "the device wrote {len} of {writable} bytes");
    let response = Response::read_from_bytes(&written[writable - RESPONSE..]).unwrap();
    written.truncate(writable - RESPONSE);
    (written, u32::from_le(response.status))
}

/// The requests of each kind this process's daemons served, and their nanoseconds in all.
fn served() -> std::collections::BTreeMap<String, u64> {
    let line = crate::metrics::requests_line();
    let fields = line.trim_start_matches("requests:").split_whitespace();
    fields
        .filter_map(|field| field.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.parse().unwrap()))
        .collect()
}

/// A guest's requests on its vhost-user queues, served by a daemon's cell end to end: the
/// configuration and queues it reads; writes and reads, across descriptors of odd sizes and on
/// either queue; commits, a root set and read; frees; info; a freeze; each as the socket serves
/// it; and the queues stopped, saying how far the guest got.
#[test]
fn a_guests_requests_are_served_on_its_vhost_user_queues() {
    let rig = Rig::new(2);
    let cell = rig.cell.cell();
    // The configuration a VMM's device has: the protocol's version, its tag, its queues, its cell.
    let config = rig.config();
    assert_eq!(u32::from_le(config.version), doc4::VIRTIO_EXTENT_VERSION);
    assert_eq!(&config.tag[..5], b"fxfs\0");
    assert_eq!(u32::from_le(config.num_request_queues), 2);
    assert_eq!(u64::from_le(config.cell), cell);
    assert_eq!(u64::from_le(config.capacity), 64 << 20);
    assert_eq!(rig.front.get_u64(GET_QUEUE_NUM).unwrap(), 2);
    let mut queues = rig.set_up(2);

    let info = bytes::<()>(doc4::VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    let (written, status) = request(&mut queues[0], &info, size_of::<Info>() + RESPONSE, 4096);
    assert_eq!(status, 0);
    let info = Info::read_from_bytes(&written).unwrap();
    assert_eq!((u64::from_le(info.cell), u64::from_le(info.capacity)), (cell, 64 << 20));

    let data: Vec<u8> = (0..2 * BLOCK).map(|i| (i % 251) as u8).collect();
    let write = bytes(doc4::VIRTIO_EXTENT_T_WRITE, 0, Some(io(cell, 1, 0, data.len())), &data);
    assert_eq!(request(&mut queues[0], &write, RESPONSE, 777), (vec![], 0));
    let read = bytes(doc4::VIRTIO_EXTENT_T_READ, 0, Some(io(cell, 1, 0, data.len())), &[]);
    assert_eq!(request(&mut queues[1], &read, data.len() + RESPONSE, 1000), (data.clone(), 0));
    // A commit setting the root, its write sealed: written again, refused.
    let root = b"the root".to_vec();
    let commit_args = CommitArgs { root_len: (root.len() as u32).to_le(), reserved: 0 };
    let commit = bytes(
        doc4::VIRTIO_EXTENT_T_COMMIT,
        doc4::VIRTIO_EXTENT_COMMIT_UPLOADED | doc4::VIRTIO_EXTENT_COMMIT_ROOT,
        Some(commit_args),
        &root,
    );
    assert_eq!(request(&mut queues[1], &commit, RESPONSE, 4096), (vec![], 0));
    let sealed = bytes(doc4::VIRTIO_EXTENT_T_WRITE, 0, Some(io(cell, 1, 0, BLOCK)), &data[..BLOCK]);
    assert_eq!(request(&mut queues[0], &sealed, RESPONSE, 4096).1, errno(Errno::EPERM));
    let read_root = bytes::<()>(doc4::VIRTIO_EXTENT_T_ROOT, 0, None, &[]);
    let returned = doc4::MAX_ROOT as usize + size_of::<RootLen>();
    let (written, status) = request(&mut queues[0], &read_root, returned + RESPONSE, 4096);
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
    assert_eq!(request(&mut queues[1], &committed, RESPONSE, 4096), (vec![], 0));
    let id = ExtentIdLe { cell: cell.to_le(), sequence: 2u64.to_le() };
    let free = bytes(doc4::VIRTIO_EXTENT_T_FREE, 0, Some(id), &[]);
    assert_eq!(request(&mut queues[1], &free, RESPONSE, 4096), (vec![], 0));
    let freed = bytes(doc4::VIRTIO_EXTENT_T_READ, 0, Some(io(cell, 2, 0, BLOCK)), &[]);
    let (_, status) = request(&mut queues[0], &freed, BLOCK + RESPONSE, 4096);
    assert_eq!(status, errno(Errno::ENOENT));
    // A freeze, and a type no request is of, answered EINVAL; a window's MAP, with no window.
    let freeze =
        bytes::<()>(doc4::VIRTIO_EXTENT_T_FREEZE, doc4::VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(request(&mut queues[0], &freeze, RESPONSE, 4096), (vec![], 0));
    let unknown = bytes::<()>(99, 0, None, &[]);
    assert_eq!(request(&mut queues[0], &unknown, RESPONSE, 4096).1, errno(Errno::EINVAL));

    // Stopped, each queue says how many chains the guest made available: its next index.
    let base = rig.front.get(GET_VRING_BASE, VringState { index: 0, num: 0 }.as_bytes()).unwrap();
    assert_eq!(VringState::read_from_bytes(&base).unwrap(), VringState { index: 0, num: 7 });
    let base = rig.front.get(GET_VRING_BASE, VringState { index: 1, num: 0 }.as_bytes()).unwrap();
    assert_eq!(VringState::read_from_bytes(&base).unwrap(), VringState { index: 1, num: 4 });
    // One channel a connection.
    assert!(rig.cell.vhost_user(1, false, "fxfs").is_err());
    rig.finish();
}

/// What a hostile front-end's message comes to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Refused, and said so: a reply-ack of failure, the channel going on.
    Refused,
    /// The channel ended.
    Closed,
}

/// Sends `request` with `payload` and `fds`, asking for a reply-ack, on a front-end that
/// negotiated them, and says what came of it.
fn outcome(front: &FrontEnd, request: u32, payload: &[u8], fds: &[BorrowedFd<'_>]) -> Outcome {
    match front.acked(request, payload, fds) {
        Ok(0) => panic!("request {request} served: {payload:?}"),
        Ok(_) => Outcome::Refused,
        Err(_) => {
            assert!(front.closed(), "request {request}: no reply, and the channel open");
            Outcome::Closed
        }
    }
}

/// A rig whose front-end negotiated its features and protocol features, `REPLY_ACK` among them,
/// and set its owner: what a hostile message is sent on.
fn negotiated() -> Rig {
    let rig = Rig::new(2);
    let protocol = rig.front.get_u64(GET_PROTOCOL_FEATURES).unwrap();
    rig.front
        .request(SET_PROTOCOL_FEATURES, false, U64 { value: protocol }.as_bytes(), &[])
        .unwrap();
    let features = rig.front.get_u64(GET_FEATURES).unwrap();
    assert_eq!(rig.front.acked(SET_FEATURES, U64 { value: features }.as_bytes(), &[]).unwrap(), 0);
    assert_eq!(rig.front.acked(SET_OWNER, &[], &[]).unwrap(), 0);
    rig
}

/// A `SET_MEM_TABLE`'s payload of one region of `memory`'s, as `edit` changes it.
fn table(memory: &GuestMemory, edit: impl FnOnce(&mut RegionWire)) -> Vec<u8> {
    let mut region = memory.region();
    edit(&mut region);
    mem_table(&[region])
}

/// A memfd of `size` bytes, sealed with `seals`, unmapped.
fn memfd(size: u64, seals: libc::c_int) -> OwnedFd {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    // SAFETY: Makes a memfd, owned here.
    let fd = unsafe {
        libc::memfd_create(c"hostile".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
    };
    assert!(fd >= 0);
    // SAFETY: Just made.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: Sizes and seals the memfd.
    unsafe {
        assert_eq!(libc::ftruncate(fd.as_raw_fd(), size as libc::off_t), 0);
        assert_eq!(libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals), 0);
    }
    fd
}

/// `fd` reopened read-only.
fn read_only(fd: BorrowedFd<'_>) -> OwnedFd {
    use std::os::fd::AsRawFd as _;
    std::fs::File::open(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap().into()
}

/// Board 14537: a hostile front-end's messages, framed well but of values that mean nothing, at
/// the edges (0, 1, the limits and past them, `u64::MAX`), each refused and said so; and those
/// framed as no request, each ending its channel. Nothing panics, nothing hangs (each wait is
/// bounded), and the daemon serves the connection's cell on.
#[test]
fn a_hostile_front_ends_messages_are_refused() {
    let page = vhost_user::memory::page_size();
    let refused: Vec<(&str, Box<dyn Fn(&Rig) -> Outcome>)> = vec![
        // Memory tables.
        (
            "a region of no bytes",
            Box::new(|rig| {
                let payload = table(&rig.memory, |region| region.size = 0);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd()])
            }),
        ),
        (
            "a region unaligned",
            Box::new(|rig| {
                let payload = table(&rig.memory, |region| region.guest = 1);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd()])
            }),
        ),
        (
            "a region ending past a u64",
            Box::new(move |rig| {
                let payload = table(&rig.memory, |region| region.guest = u64::MAX - page + 1);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd()])
            }),
        ),
        (
            "a region past its memory's end",
            Box::new(move |rig| {
                let payload = table(&rig.memory, |region| region.offset = page);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd()])
            }),
        ),
        (
            "a region at offset u64::MAX's page",
            Box::new(move |rig| {
                let payload = table(&rig.memory, |region| region.offset = u64::MAX - page + 1);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd()])
            }),
        ),
        (
            "memory not sealed against shrinking",
            Box::new(|rig| {
                let unsealed = GuestMemory::with_seals(rig.memory.size(), 0).unwrap();
                let payload = mem_table(&[unsealed.region()]);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[unsealed.fd()])
            }),
        ),
        (
            "memory sealed against writes",
            Box::new(|rig| {
                let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
                let sealed = memfd(rig.memory.size(), seals);
                outcome(
                    &rig.front,
                    SET_MEM_TABLE,
                    &mem_table(&[rig.memory.region()]),
                    &[sealed.as_fd()],
                )
            }),
        ),
        (
            "memory that is no memfd",
            Box::new(|rig| {
                let not_memory = eventfd();
                outcome(
                    &rig.front,
                    SET_MEM_TABLE,
                    &mem_table(&[rig.memory.region()]),
                    &[not_memory.as_fd()],
                )
            }),
        ),
        (
            "memory open read-only",
            Box::new(|rig| {
                let read_only = read_only(rig.memory.fd());
                outcome(
                    &rig.front,
                    SET_MEM_TABLE,
                    &mem_table(&[rig.memory.region()]),
                    &[read_only.as_fd()],
                )
            }),
        ),
        (
            "regions overlapping",
            Box::new(|rig| {
                let region = rig.memory.region();
                let payload = mem_table(&[region, region]);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd(), rig.memory.fd()])
            }),
        ),
        (
            "a table of no regions",
            Box::new(|rig| outcome(&rig.front, SET_MEM_TABLE, &mem_table(&[]), &[])),
        ),
        (
            "a table of 9 regions",
            Box::new(|rig| {
                let regions = vec![rig.memory.region(); 9];
                let fds = vec![rig.memory.fd(); 9];
                outcome(&rig.front, SET_MEM_TABLE, &mem_table(&regions), &fds)
            }),
        ),
        // Rings.
        (
            "a ring of no entries",
            Box::new(|rig| {
                outcome(&rig.front, SET_VRING_NUM, VringState { index: 0, num: 0 }.as_bytes(), &[])
            }),
        ),
        (
            "a ring of 3 entries",
            Box::new(|rig| {
                outcome(&rig.front, SET_VRING_NUM, VringState { index: 0, num: 3 }.as_bytes(), &[])
            }),
        ),
        (
            "a ring of 512 entries",
            Box::new(|rig| {
                outcome(
                    &rig.front,
                    SET_VRING_NUM,
                    VringState { index: 0, num: 512 }.as_bytes(),
                    &[],
                )
            }),
        ),
        (
            "a ring of u32::MAX entries",
            Box::new(|rig| {
                let state = VringState { index: 0, num: u32::MAX };
                outcome(&rig.front, SET_VRING_NUM, state.as_bytes(), &[])
            }),
        ),
        (
            "a queue past the device's",
            Box::new(|rig| {
                outcome(
                    &rig.front,
                    SET_VRING_NUM,
                    VringState { index: 2, num: 256 }.as_bytes(),
                    &[],
                )
            }),
        ),
        (
            "queue u32::MAX",
            Box::new(|rig| {
                let state = VringState { index: u32::MAX, num: 256 };
                outcome(&rig.front, SET_VRING_BASE, state.as_bytes(), &[])
            }),
        ),
        (
            "a base past a u16",
            Box::new(|rig| {
                let state = VringState { index: 0, num: 1 << 16 };
                outcome(&rig.front, SET_VRING_BASE, state.as_bytes(), &[])
            }),
        ),
        (
            "a ring enabled 2",
            Box::new(|rig| {
                outcome(
                    &rig.front,
                    SET_VRING_ENABLE,
                    VringState { index: 0, num: 2 }.as_bytes(),
                    &[],
                )
            }),
        ),
        (
            "a ring logged",
            Box::new(|rig| {
                let addr = VringAddrWire { index: 0, flags: VRING_F_LOG, ..Default::default() };
                outcome(&rig.front, SET_VRING_ADDR, addr.as_bytes(), &[])
            }),
        ),
        (
            "a kick that is no eventfd",
            Box::new(|rig| {
                outcome(&rig.front, SET_VRING_KICK, U64 { value: 0 }.as_bytes(), &[rig.memory.fd()])
            }),
        ),
        (
            "a call that is no eventfd",
            Box::new(|rig| {
                outcome(&rig.front, SET_VRING_CALL, U64 { value: 0 }.as_bytes(), &[rig.memory.fd()])
            }),
        ),
        (
            "a kick polled",
            Box::new(|rig| {
                let word = U64 { value: VRING_NOFD_MASK };
                outcome(&rig.front, SET_VRING_KICK, word.as_bytes(), &[])
            }),
        ),
        (
            "a kick's word of unknown bits",
            Box::new(|rig| {
                let kick = eventfd();
                outcome(
                    &rig.front,
                    SET_VRING_KICK,
                    U64 { value: 1 << 9 }.as_bytes(),
                    &[kick.as_fd()],
                )
            }),
        ),
        (
            "a kick of queue 255",
            Box::new(|rig| {
                let kick = eventfd();
                outcome(&rig.front, SET_VRING_KICK, U64 { value: 255 }.as_bytes(), &[kick.as_fd()])
            }),
        ),
        (
            "rings outside the memory table",
            Box::new(|rig| {
                let ack = |request, payload: &[u8], fds: &[BorrowedFd<'_>]| {
                    assert_eq!(rig.front.acked(request, payload, fds).unwrap(), 0, "{request}");
                };
                ack(SET_MEM_TABLE, &mem_table(&[rig.memory.region()]), &[rig.memory.fd()]);
                ack(SET_VRING_NUM, VringState { index: 0, num: 256 }.as_bytes(), &[]);
                let far = VringAddrWire {
                    index: 0,
                    flags: 0,
                    desc: u64::MAX - 15,
                    used: 0,
                    avail: 0,
                    log: 0,
                };
                ack(SET_VRING_ADDR, far.as_bytes(), &[]);
                let kick = eventfd();
                outcome(&rig.front, SET_VRING_KICK, U64 { value: 0 }.as_bytes(), &[kick.as_fd()])
            }),
        ),
        (
            "rings misaligned",
            Box::new(|rig| {
                let ack = |request, payload: &[u8], fds: &[BorrowedFd<'_>]| {
                    assert_eq!(rig.front.acked(request, payload, fds).unwrap(), 0, "{request}");
                };
                ack(SET_MEM_TABLE, &mem_table(&[rig.memory.region()]), &[rig.memory.fd()]);
                ack(SET_VRING_NUM, VringState { index: 0, num: 256 }.as_bytes(), &[]);
                let user = rig.memory.user_base();
                let addr = VringAddrWire {
                    index: 0,
                    flags: 0,
                    desc: user + 8,
                    used: user + 8192,
                    avail: user + 4096,
                    log: 0,
                };
                ack(SET_VRING_ADDR, addr.as_bytes(), &[]);
                let kick = eventfd();
                outcome(&rig.front, SET_VRING_KICK, U64 { value: 0 }.as_bytes(), &[kick.as_fd()])
            }),
        ),
        // Features.
        (
            "fewer features than offered",
            Box::new(|rig| outcome(&rig.front, SET_FEATURES, U64 { value: 0 }.as_bytes(), &[])),
        ),
        (
            "a feature never offered",
            Box::new(|rig| {
                let features = rig.front.get_u64(GET_FEATURES).unwrap();
                outcome(&rig.front, SET_FEATURES, U64 { value: features | 1 << 63 }.as_bytes(), &[])
            }),
        ),
        ("an owner set again", Box::new(|rig| outcome(&rig.front, SET_OWNER, &[], &[]))),
        (
            "a protocol feature never offered",
            Box::new(|rig| {
                outcome(&rig.front, SET_PROTOCOL_FEATURES, U64 { value: u64::MAX }.as_bytes(), &[])
            }),
        ),
        // The back-end channel (board 14603).
        (
            "a back-end channel that is no socket",
            Box::new(|rig| {
                let not_socket = eventfd();
                outcome(&rig.front, SET_BACKEND_REQ_FD, &[], &[not_socket.as_fd()])
            }),
        ),
        (
            "a back-end channel of datagrams",
            Box::new(|rig| {
                let (datagrams, _) = std::os::unix::net::UnixDatagram::pair().unwrap();
                outcome(&rig.front, SET_BACKEND_REQ_FD, &[], &[datagrams.as_fd()])
            }),
        ),
        (
            "a back-end channel of a network socket",
            Box::new(|rig| {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                outcome(&rig.front, SET_BACKEND_REQ_FD, &[], &[listener.as_fd()])
            }),
        ),
        (
            "a back-end channel not negotiated",
            Box::new(|rig| {
                let protocol = U64 { value: 1 << PROTOCOL_F_REPLY_ACK };
                rig.front.request(SET_PROTOCOL_FEATURES, false, protocol.as_bytes(), &[]).unwrap();
                let (channel, _) = std::os::unix::net::UnixStream::pair().unwrap();
                outcome(&rig.front, SET_BACKEND_REQ_FD, &[], &[channel.as_fd()])
            }),
        ),
        // A GET refused, which no reply-ack answers, and those framed as no request: the channel
        // ends.
        (
            "a configuration read past 256 bytes",
            Box::new(|rig| {
                let mut payload =
                    ConfigHeader { offset: 0, size: 257, flags: 0 }.as_bytes().to_vec();
                payload.resize(payload.len() + 257, 0);
                outcome(&rig.front, GET_CONFIG, &payload, &[])
            }),
        ),
        (
            "a configuration read past a u32",
            Box::new(|rig| {
                let mut payload =
                    ConfigHeader { offset: u32::MAX, size: 1, flags: 0 }.as_bytes().to_vec();
                payload.push(0);
                outcome(&rig.front, GET_CONFIG, &payload, &[])
            }),
        ),
        (
            "a header of version 2",
            Box::new(|rig| {
                let header = Header { request: GET_FEATURES, flags: 2, size: 0 };
                rig.front.send(header, &[], &[]).unwrap();
                assert!(rig.front.closed());
                Outcome::Closed
            }),
        ),
        (
            "a header saying it is a reply",
            Box::new(|rig| {
                let header = Header { request: GET_FEATURES, flags: VERSION | REPLY_FLAG, size: 0 };
                rig.front.send(header, &[], &[]).unwrap();
                assert!(rig.front.closed());
                Outcome::Closed
            }),
        ),
        (
            "a payload of u32::MAX bytes",
            Box::new(|rig| {
                let header = Header { request: SET_FEATURES, flags: VERSION, size: u32::MAX };
                rig.front.send(header, &[], &[]).unwrap();
                assert!(rig.front.closed());
                Outcome::Closed
            }),
        ),
        (
            "a payload one past the most",
            Box::new(|rig| {
                let header =
                    Header { request: SET_FEATURES, flags: VERSION, size: MAX_PAYLOAD + 1 };
                rig.front.send(header, &[], &[]).unwrap();
                assert!(rig.front.closed());
                Outcome::Closed
            }),
        ),
        ("a request no back-end knows", Box::new(|rig| outcome(&rig.front, 999, &[], &[]))),
        ("a request of 0", Box::new(|rig| outcome(&rig.front, 0, &[], &[]))),
        ("a request of u32::MAX", Box::new(|rig| outcome(&rig.front, u32::MAX, &[], &[]))),
        ("a word of 7 bytes", Box::new(|rig| outcome(&rig.front, SET_FEATURES, &[0; 7], &[]))),
        ("a word of 9 bytes", Box::new(|rig| outcome(&rig.front, SET_FEATURES, &[0; 9], &[]))),
        (
            "a table of 2 regions and 1 descriptor",
            Box::new(|rig| {
                let region = rig.memory.region();
                let payload = mem_table(&[region, region]);
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd()])
            }),
        ),
        (
            "a table shorter than its regions",
            Box::new(|rig| {
                let mut payload = mem_table(&[rig.memory.region()]);
                payload.pop();
                outcome(&rig.front, SET_MEM_TABLE, &payload, &[rig.memory.fd()])
            }),
        ),
        (
            "a GET carrying a descriptor",
            Box::new(|rig| {
                let fd = eventfd();
                outcome(&rig.front, GET_FEATURES, &[], &[fd.as_fd()])
            }),
        ),
        (
            "a message of 9 descriptors",
            Box::new(|rig| {
                let fds: Vec<OwnedFd> = (0..9).map(|_| eventfd()).collect();
                let fds: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
                outcome(&rig.front, SET_OWNER, &[], &fds)
            }),
        ),
        (
            "a back-end channel of no descriptor",
            Box::new(|rig| outcome(&rig.front, SET_BACKEND_REQ_FD, &[], &[])),
        ),
        (
            "a back-end channel of a payload",
            Box::new(|rig| {
                let (channel, _) = std::os::unix::net::UnixStream::pair().unwrap();
                outcome(&rig.front, SET_BACKEND_REQ_FD, &[0; 8], &[channel.as_fd()])
            }),
        ),
        (
            "half a header, then nothing",
            Box::new(|rig| {
                let header = Header { request: GET_FEATURES, flags: VERSION, size: 8 };
                rig.front.send_bytes(&header.as_bytes()[..6]).unwrap();
                // The back-end waits for the rest on its own thread, holding no other's.
                assert!(rig.front.reply().is_err(), "a reply to half a header");
                Outcome::Closed
            }),
        ),
    ];
    let mut outcomes = Vec::new();
    for (what, case) in &refused {
        let rig = negotiated();
        let started = Instant::now();
        let came = case(&rig);
        assert!(
            started.elapsed() < WAIT + Duration::from_secs(5),
            "{what}: took {:?}",
            started.elapsed()
        );
        // The connection's cell serves on, whatever its channel's front-end did.
        assert_eq!(rig.cell.info().unwrap().capacity, 64 << 20, "{what}");
        outcomes.push((*what, came));
        rig.finish();
    }
    let closed: Vec<&str> = outcomes
        .iter()
        .filter(|(_, came)| *came == Outcome::Closed)
        .map(|(what, _)| *what)
        .collect();
    assert_eq!(
        closed,
        [
            "a table of 9 regions",
            "a configuration read past 256 bytes",
            "a configuration read past a u32",
            "a header of version 2",
            "a header saying it is a reply",
            "a payload of u32::MAX bytes",
            "a payload one past the most",
            "a request no back-end knows",
            "a request of 0",
            "a request of u32::MAX",
            "a word of 7 bytes",
            "a word of 9 bytes",
            "a table of 2 regions and 1 descriptor",
            "a table shorter than its regions",
            "a GET carrying a descriptor",
            "a message of 9 descriptors",
            "a back-end channel of no descriptor",
            "a back-end channel of a payload",
            "half a header, then nothing",
        ]
    );
}

/// Board 14537: a hostile guest's rings: descriptors past guest memory or the table, loops,
/// indirect tables, readable after writable, an available index past the ring's entries, heads
/// past it, each breaking its queue, its error notifier signalled, the other queue serving on,
/// and its stop answered; and chains past the protocol's bounds, returned with nothing written.
#[test]
fn a_hostile_guests_rings_break_their_queue_alone() {
    let size = 256u16;
    let gib = 1u64 << 30;
    let broken: Vec<(&str, Box<dyn Fn(&Queue)>)> = vec![
        (
            "a descriptor past guest memory",
            Box::new(move |queue| {
                queue.raw_descriptor(0, gib, 4096, 0, 0);
                queue.raw_offer(0, 0, 1);
            }),
        ),
        (
            "a descriptor ending past a u64",
            Box::new(|queue| {
                queue.raw_descriptor(0, u64::MAX - 7, 16, 0, 0);
                queue.raw_offer(0, 0, 1);
            }),
        ),
        (
            "a descriptor of u32::MAX bytes",
            Box::new(|queue| {
                queue.raw_descriptor(0, 0, u32::MAX, 0, 0);
                queue.raw_offer(0, 0, 1);
            }),
        ),
        (
            "a next past the table",
            Box::new(move |queue| {
                queue.raw_descriptor(0, queue.layout.buffers, 16, ring_flags::NEXT, size);
                queue.raw_offer(0, 0, 1);
            }),
        ),
        ("a head past the table", Box::new(move |queue| queue.raw_offer(0, size, 1))),
        ("head u16::MAX", Box::new(|queue| queue.raw_offer(0, u16::MAX, 1))),
        (
            "a loop",
            Box::new(|queue| {
                queue.raw_descriptor(0, queue.layout.buffers, 16, ring_flags::NEXT, 0);
                queue.raw_offer(0, 0, 1);
            }),
        ),
        (
            "an indirect table",
            Box::new(|queue| {
                queue.raw_descriptor(0, queue.layout.buffers, 16, ring_flags::INDIRECT, 0);
                queue.raw_offer(0, 0, 1);
            }),
        ),
        (
            "readable after writable",
            Box::new(|queue| {
                let at = queue.layout.buffers;
                queue.raw_descriptor(0, at, 16, ring_flags::WRITE | ring_flags::NEXT, 1);
                queue.raw_descriptor(1, at + 16, 16, 0, 0);
                queue.raw_offer(0, 0, 1);
            }),
        ),
        ("an index past the ring", Box::new(move |queue| queue.raw_offer(0, 0, size + 1))),
        ("index u16::MAX", Box::new(|queue| queue.raw_offer(0, 0, u16::MAX))),
    ];
    for (what, break_it) in &broken {
        let rig = Rig::new(2);
        let mut queues = rig.set_up(2);
        break_it(&queues[0]);
        assert!(wait_signal(queues[0].err.as_fd(), WAIT), "{what}: the ring's error not signalled");
        // The other queue serves on.
        let info = bytes::<()>(doc4::VIRTIO_EXTENT_T_INFO, 0, None, &[]);
        assert_eq!(
            request(&mut queues[1], &info, size_of::<Info>() + RESPONSE, 4096).1,
            0,
            "{what}"
        );
        // Stopped, the broken queue answers.
        let base = rig.front.get(GET_VRING_BASE, VringState { index: 0, num: 0 }.as_bytes());
        assert!(base.is_ok(), "{what}: {base:?}");
        rig.finish();
    }

    // Chains past the protocol's bounds, or with no room for a response: returned, nothing
    // written; the queue serving on.
    let rig = Rig::new(2);
    let mut queues = rig.set_up(1);
    let info = bytes::<()>(doc4::VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    for writable in [0, 1, RESPONSE - 1, MAX_WRITABLE + 1] {
        let returned = queues[0].request(&info, writable, usize::MAX).expect("returned");
        assert_eq!(returned.len, 0, "{writable} bytes to write");
    }
    let readable = vec![0u8; doc4::MAX_READABLE + 1];
    let returned = queues[0].request(&readable, RESPONSE, usize::MAX).expect("returned");
    assert_eq!(returned.len, 0);
    assert_eq!(request(&mut queues[0], &info, size_of::<Info>() + RESPONSE, 4096).1, 0);
    rig.finish();
}

/// The split ring's descriptor flags.
mod ring_flags {
    pub(super) const NEXT: u16 = vhost_user::ring::DESC_F_NEXT;
    pub(super) const WRITE: u16 = vhost_user::ring::DESC_F_WRITE;
    pub(super) const INDIRECT: u16 = vhost_user::ring::DESC_F_INDIRECT;
}

/// Board 14537: the daemon's serving time per request on its vhost-user back-end, as its metrics
/// count it, beside the round trip the test front-end's guest sees, for the board's record
/// against step 0's baseline (`docs/measurements/ubu1-fs-bench-vhost-baseline-2026-11-27`):
/// printed (`--no-capture`). Reads of an extent the daemon holds in memory, 4 KiB and 128 KiB;
/// writes; commits ordered and uploaded (to the test's bucket in memory).
#[test]
fn vhost_user_serving_times() {
    const ROUNDS: u64 = 400;
    let rig = Rig::new(1);
    let cell = rig.cell.cell();
    let mut queues = rig.set_up(1);
    let queue = &mut queues[0];
    let big = vec![0x5au8; 128 << 10];
    let write = bytes(doc4::VIRTIO_EXTENT_T_WRITE, 0, Some(io(cell, 1, 0, big.len())), &big);
    assert_eq!(request(queue, &write, RESPONSE, usize::MAX), (vec![], 0));
    let commit =
        |flags| bytes(doc4::VIRTIO_EXTENT_T_COMMIT, flags, Some(CommitArgs::default()), &[]);
    assert_eq!(
        request(queue, &commit(doc4::VIRTIO_EXTENT_COMMIT_UPLOADED), RESPONSE, usize::MAX).1,
        0
    );
    let mut lines = Vec::new();
    let mut measure =
        |name: &str, kind: &str, rounds: u64, mut one: Box<dyn FnMut(&mut Queue, u64) + '_>| {
            let before = served();
            let started = Instant::now();
            for round in 0..rounds {
                one(queue, round);
            }
            let round_trip = started.elapsed().as_nanos() as f64 / rounds as f64 / 1e3;
            let after = served();
            let delta = |key: &str| {
                after.get(key).copied().unwrap_or(0) - before.get(key).copied().unwrap_or(0)
            };
            let (count, nanos) = (delta(kind), delta(&format!("{kind}-ns")));
            assert_eq!(count, rounds, "{name}: {kind} requests served");
            let serving = nanos as f64 / count as f64 / 1e3;
            lines.push(format!(
                "{name}: daemon serving {serving:.1} us, guest round trip {round_trip:.1} us"
            ));
        };
    for len in [4usize << 10, 128 << 10] {
        let read = bytes(doc4::VIRTIO_EXTENT_T_READ, 0, Some(io(cell, 1, 0, len)), &[]);
        measure(
            &format!("read {} KiB", len >> 10),
            "read",
            ROUNDS,
            Box::new(move |queue, _| {
                assert_eq!(request(queue, &read, len + RESPONSE, usize::MAX).1, 0);
            }),
        );
    }
    for (len, rounds) in [(4usize << 10, ROUNDS), (128 << 10, ROUNDS / 4)] {
        let data = vec![7u8; len];
        let sequence = if len == 4096 { 2 } else { 3 };
        measure(
            &format!("write {} KiB", len >> 10),
            "write",
            rounds,
            Box::new(move |queue, round| {
                let write = bytes(
                    doc4::VIRTIO_EXTENT_T_WRITE,
                    0,
                    Some(io(cell, sequence, round * len as u64, len)),
                    &data,
                );
                assert_eq!(request(queue, &write, RESPONSE, usize::MAX).1, 0);
            }),
        );
    }
    let ordered = commit(0);
    measure(
        "commit ordered",
        "commit",
        ROUNDS / 4,
        Box::new(move |queue, _| {
            assert_eq!(request(queue, &ordered, RESPONSE, usize::MAX).1, 0);
        }),
    );
    let uploaded = commit(doc4::VIRTIO_EXTENT_COMMIT_UPLOADED);
    measure(
        "commit uploaded",
        "commit",
        ROUNDS / 4,
        Box::new(move |queue, _| {
            assert_eq!(request(queue, &uploaded, RESPONSE, usize::MAX).1, 0);
        }),
    );
    for line in &lines {
        eprintln!("vhost-user serving times: {line}");
    }
    assert_eq!(lines.len(), 6);
    drop(queues);
    rig.finish();
}

/// The Codex review of board 14537: a `VHOST_USER` whose start the connection's end overtakes,
/// its stop finding no channel yet, starts none after: its back-end would hold the connection and
/// its cell for good, the front-end keeping the channel's end it was sent. Held back between
/// failpoints: the start until the connection stopped its channel.
#[test]
fn a_channel_started_as_its_connection_ends_holds_nothing() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (theirs, ours) = std::os::unix::net::UnixStream::pair().unwrap();
    let half = theirs.try_clone().unwrap();
    let served = std::thread::spawn(move || {
        crate::server::serve_connection(ours, crate::server::default_tenant().unwrap())
    });
    let opening = crate::client::Opening::Create(64 << 20, bucket_cell::cell::Kind::Fxfs);
    let cell = CellClient::over(theirs, &dir.path().join("store"), opening).unwrap();
    let (hit, held) = std::sync::mpsc::channel();
    let (go, gone) = std::sync::mpsc::channel::<()>();
    let gone = std::sync::Mutex::new(gone);
    fail::cfg_callback("cladefsd::vhost-user-start", move || {
        hit.send(()).unwrap();
        gone.lock().unwrap().recv().unwrap();
    })
    .unwrap();
    let (stopped, stop) = std::sync::mpsc::channel();
    let stopped = std::sync::Mutex::new(stopped);
    fail::cfg_callback("cladefsd::vhost-user-stopped", move || {
        // The test's receiver gone: nothing to tell.
        let _ = stopped.lock().unwrap().send(());
    })
    .unwrap();
    let asking = std::thread::spawn(move || {
        let channel = cell.vhost_user(1, false, "fxfs");
        (channel, cell)
    });
    held.recv_timeout(WAIT).expect("the start held back");
    // The VM process closes its half of the connection: the connection ends, and stops its
    // channel, none started yet.
    half.shutdown(std::net::Shutdown::Write).unwrap();
    stop.recv_timeout(WAIT).expect("the connection stopped its channel");
    go.send(()).unwrap();
    let (channel, cell) = asking.join().unwrap();
    assert_eq!(channel.err(), Some(Errno::EBADF));
    // The connection ends, though the VM process holds its socket's end still.
    let (ended, end) = std::sync::mpsc::channel();
    std::thread::spawn(move || ended.send(served.join().is_ok()).unwrap());
    assert_eq!(end.recv_timeout(WAIT), Ok(true), "the connection never ended");
    fail::remove("cladefsd::vhost-user-start");
    fail::remove("cladefsd::vhost-user-stopped");
    drop(cell);
}

/// Says it heard a configuration change on the back-end channel `backend`, [`WAIT`] at most.
fn heard_a_change(backend: &std::os::unix::net::UnixStream) {
    let (header, payload) = backend_request(backend).expect("a configuration change said");
    assert_eq!(header, Header { request: BACKEND_CONFIG_CHANGE_MSG, flags: VERSION, size: 0 });
    assert_eq!(payload, Vec::<u8>::new());
}

/// Board 14603: the cell's capacity changes and the daemon's asks for its stats reach a guest on
/// its vhost-user device as configuration changes, which the back-end says on the back-end
/// channel: a grow; a shrink asked, answered and taken; a shrink refused; and a taking of stats,
/// the guest's answered with its own.
#[test]
fn a_guest_hears_of_its_configurations_changes_on_the_backend_channel() {
    let rig = Rig::new(1);
    let mut queues = rig.set_up(1);
    let backend = rig.front.backend_channel(None).unwrap();
    let told = |rig: &Rig| {
        let config = rig.config();
        (u64::from_le(config.capacity), u64::from_le(config.asked))
    };
    let resized = |capacity: u64, took: bool| {
        let flags = if took { doc4::VIRTIO_EXTENT_RESIZED_TOOK } else { 0 };
        let args = ResizedArgs { capacity: capacity.to_le() };
        bytes(doc4::VIRTIO_EXTENT_T_RESIZED, flags, Some(args), &[])
    };

    super::resize(&rig.path, 128 << 20).unwrap();
    heard_a_change(&backend);
    assert_eq!(told(&rig), (128 << 20, 0));

    let path = rig.path.clone();
    let shrinking = std::thread::spawn(move || super::resize(&path, 32 << 20));
    heard_a_change(&backend);
    assert_eq!(told(&rig), (128 << 20, 32 << 20));
    assert_eq!(request(&mut queues[0], &resized(32 << 20, true), RESPONSE, 4096), (vec![], 0));
    assert_eq!(shrinking.join().unwrap(), Ok(()));
    assert_eq!(told(&rig), (32 << 20, 0));
    assert_eq!(rig.cell.info().unwrap().capacity, 32 << 20);

    let path = rig.path.clone();
    let refusing = std::thread::spawn(move || super::resize(&path, 16 << 20));
    heard_a_change(&backend);
    assert_eq!(told(&rig), (32 << 20, 16 << 20));
    assert_eq!(request(&mut queues[0], &resized(16 << 20, false), RESPONSE, 4096), (vec![], 0));
    assert_eq!(refusing.join().unwrap(), Err(Errno::ENOSPC));
    assert_eq!(told(&rig), (32 << 20, 0));

    let taking = super::take_stats(WAIT);
    heard_a_change(&backend);
    let ask = u64::from_le(rig.config().stats_asked);
    assert_ne!(ask, 0, "no ask in the configuration");
    let answer = bytes(
        doc4::VIRTIO_EXTENT_T_STATS,
        0,
        Some(StatsArgs { asked: ask.to_le() }),
        b"seals 3\ncommit abc\n",
    );
    assert_eq!(request(&mut queues[0], &answer, RESPONSE, 4096), (vec![], 0));
    let taken = taking.join().unwrap();
    let guest = taken.vms[0].guest.as_ref().expect("the guest's stats");
    assert_eq!(guest.counters.get("seals"), Some(&3));
    assert_eq!(guest.build.as_deref(), Some("abc"));
    drop(queues);
    rig.finish();
}

/// Board 14603: a guest on a device whose front-end gave no back-end channel, which nothing could
/// tell of a change, is asked no shrink (`ENOTSUP`, the cell kept as it is), and no stats (its VM
/// answered at once, without the guest's); a grow it reads as it next reads its configuration.
#[test]
fn a_guest_with_no_backend_channel_is_asked_nothing() {
    let rig = Rig::new(1);
    let queues = rig.set_up(1);
    assert_eq!(super::resize(&rig.path, 32 << 20), Err(Errno::ENOTSUP));
    assert_eq!(rig.cell.info().unwrap().capacity, 64 << 20);
    super::resize(&rig.path, 128 << 20).unwrap();
    assert_eq!(u64::from_le(rig.config().capacity), 128 << 20);
    let taken = super::take_stats(WAIT).join().unwrap();
    assert!(taken.vms[0].answered.is_some(), "{:?}", taken.vms[0]);
    assert!(taken.vms[0].guest.is_none(), "{:?}", taken.vms[0]);
    drop(queues);
    rig.finish();
}

/// Board 14603: a front-end that never reads its back-end channel, its socket made as small as it
/// can be, stalls nothing: the cell's capacity changed hundreds of times, each change said, or
/// folded into the request waiting for room, the device's requests are served, its configuration
/// read as it is, and its channel stops; what came on the back-end channel, read at last, fewer
/// requests than changes.
#[test]
fn a_front_end_that_never_reads_its_backend_channel_stalls_nothing() {
    const CHANGES: usize = 300;
    let rig = Rig::new(1);
    let mut queues = rig.set_up(1);
    let backend = rig.front.backend_channel(Some(1)).unwrap();
    let mut capacity = 64u64 << 20;
    for _ in 0..CHANGES {
        capacity += BLOCK as u64;
        super::resize(&rig.path, capacity).unwrap();
    }
    let info = bytes::<()>(doc4::VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    assert_eq!(request(&mut queues[0], &info, size_of::<Info>() + RESPONSE, 4096).1, 0);
    assert_eq!(u64::from_le(rig.config().capacity), capacity);
    backend.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let mut heard = 0;
    while backend_request(&backend).is_ok() {
        heard += 1;
    }
    assert!((1..CHANGES).contains(&heard), "{heard} requests for {CHANGES} changes");
    drop(queues);
    rig.finish();
}
