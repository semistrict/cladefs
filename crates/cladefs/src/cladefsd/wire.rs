// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The messages between a VM process and the extent daemon, and their framing on a Unix stream.
//!
//! Each message is a [`Header`], followed by `len` bytes of payload. A message may carry up to
//! [`MAX_FDS`] file descriptors, as `SCM_RIGHTS`, with its first bytes: the daemon's reply to
//! [`OPEN`] carries its physical memory, the VM process's tenant's arena of it (`OpenedBody::arena`),
//! the default tenant's if it is of none, and the default arena, read-only (TASK-43, TASK-79.17).
//!
//! cladefs: each message's fields, its flags and arguments, payload and descriptors, are encoded
//! and decoded in [`super::codec`] alone (TASK-160.9), which refuses what a kind gives no meaning.
//!
//! cladefs: the wire has a version, [`VERSION`], and a VM process and a daemon of different ones
//! refuse each other as the VM process opens its cell (TASK-141.3), rather than serve with what
//! requests both happen to know: AGENTS.md's rule, no migrations, which has a format change
//! outright and the old one refused. Every request is required of a daemon of the VM process's
//! version, so none is optional: a refusal of one fails the connection.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Mutex;
// cladefs: a Unix stream's, Linux's alone: the VM process's half, as its client is
// (`super::client`), and the daemon's outside the simulation, whose guests' connections are in
// memory, macOS's included (board 2912).
#[cfg(target_os = "linux")]
use super::codec::{Stage, StageOp};
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;

/// cladefs: a VM process's connection in memory, under madsim (doc-24).
#[cfg(madsim)]
mod memory;

/// cladefs: a VM process's connection, as the daemon serves it: a Unix stream; under madsim, which
/// has no Unix sockets, an end of a connection in memory, a simulated guest's at the other
/// ([`memory::Stream::pair`], doc-24's boundary).
#[cfg(not(madsim))]
pub(crate) type Stream = UnixStream;
#[cfg(madsim)]
pub(crate) use memory::Stream;

/// cladefs: the socket the daemon's async sender and receiver use, the one place they differ
/// under madsim: a [`Stream`] on the runtime's reactor, sending with `sendmsg` and reading with
/// `recvmsg`; or the connection in memory, its calls those of [`memory::Stream`] (doc-24).
pub(crate) struct Socket {
    #[cfg(not(madsim))]
    fd: tokio::io::unix::AsyncFd<UnixStream>,
    #[cfg(madsim)]
    stream: memory::Stream,
}

/// cladefs: that a [`Socket`] takes more, until cleared: the reactor's readiness, or, in memory,
/// always, as it takes all it is sent.
pub(crate) struct Writable<'a> {
    #[cfg(not(madsim))]
    ready: tokio::io::unix::AsyncFdReadyGuard<'a, UnixStream>,
    #[cfg(madsim)]
    socket: std::marker::PhantomData<&'a Socket>,
}

impl Writable<'_> {
    /// It took what it could: to be waited for again.
    pub(crate) fn clear_ready(&mut self) {
        #[cfg(not(madsim))]
        self.ready.clear_ready();
    }
}

impl Socket {
    /// `stream`, the caller's, non-blocking, to write.
    fn writing(stream: Stream) -> io::Result<Self> {
        #[cfg(not(madsim))]
        {
            // SAFETY: `stream` is moved into `AsyncFd` and owns the socket descriptor, so it stays
            // valid and open until `AsyncFd` drops and deregisters it before closing `stream`.
            let fd = unsafe {
                tokio::io::unix::AsyncFd::register_with_interest(
                    stream,
                    tokio::io::Interest::WRITABLE,
                )?
            };
            return Ok(Self { fd });
        }
        #[cfg(madsim)]
        return Ok(Self { stream });
    }

    /// `stream`, made non-blocking, to read.
    fn reading(stream: Stream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        #[cfg(not(madsim))]
        {
            // SAFETY: `stream` is moved into `AsyncFd` and owns the socket descriptor, so it stays
            // valid and open until `AsyncFd` drops and deregisters it before closing `stream`.
            let fd = unsafe { tokio::io::unix::AsyncFd::register(stream)? };
            return Ok(Self { fd });
        }
        #[cfg(madsim)]
        return Ok(Self { stream });
    }

    /// Once it takes more bytes.
    async fn writable(&self) -> io::Result<Writable<'_>> {
        #[cfg(not(madsim))]
        return Ok(Writable { ready: self.fd.writable().await? });
        #[cfg(madsim)]
        return Ok(Writable { socket: std::marker::PhantomData });
    }

    /// Sends what of `parts`, in order, one write does, with `fds`: how many bytes it sent.
    fn send_parts(&self, parts: &[&[u8]], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
        #[cfg(not(madsim))]
        return send_parts(self.fd.get_ref(), parts, fds);
        #[cfg(madsim)]
        return self.stream.send_parts(parts, fds);
    }

    /// Reads what of `bytes` it has, once it has any: with the descriptors that came with them,
    /// if `with_fds`.
    async fn read(&self, bytes: &mut [u8], with_fds: bool) -> io::Result<(usize, Vec<OwnedFd>)> {
        #[cfg(not(madsim))]
        loop {
            let mut ready = self.fd.readable().await?;
            let read = ready.try_io(|stream| match with_fds {
                true => receive_with_fds(stream.get_ref(), bytes, 0),
                false => (&*stream.get_ref()).read(bytes).map(|read| (read, Vec::new())),
            });
            match read {
                Ok(read) => return read,
                // Not ready after all: waited for again.
                Err(_) => continue,
            }
        }
        #[cfg(madsim)]
        return self.stream.read(bytes, with_fds).await;
    }

    /// Shuts the connection down both ways.
    fn shutdown(&self) -> io::Result<()> {
        #[cfg(not(madsim))]
        return self.fd.get_ref().shutdown(std::net::Shutdown::Both);
        #[cfg(madsim)]
        return self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Whether the other end hung up.
    fn hung_up(&self) -> bool {
        #[cfg(not(madsim))]
        return hung_up(self.fd.get_ref().as_fd());
        #[cfg(madsim)]
        return self.stream.hung_up();
    }
}
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// The fixed-size inline body of a [`Header`], holding one typed `#[repr(C)]` wire struct (up to
/// `BODY_BYTES` bytes), zero-padded to the end.
pub(crate) const BODY_BYTES: usize = 40;

/// The byte offset inside [`Body`] where [`OpenBody::version`] and [`OpenedBody::version`] sit
/// (byte offset 48 of [`Header`]), fixed across every wire version so a version mismatch is
/// refused with `EPROTONOSUPPORT` before decoding anything else (TASK-141.3).
pub(crate) const VERSION_OFFSET: usize = 32;

/// The 40-byte, 8-byte-aligned inline body of a [`Header`].
#[repr(C, align(8))]
#[derive(Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub(crate) struct Body(pub [u8; BODY_BYTES]);

impl Default for Body {
    fn default() -> Self {
        Self([0; BODY_BYTES])
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let words: [u64; 5] =
            FromBytes::read_from_bytes(&self.0).expect("BODY_BYTES is five u64 words");
        f.debug_list().entries(words).finish()
    }
}

/// A message's header: what it is, the request it is for, and its typed inline [`Body`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct Header {
    /// One of the kinds below.
    pub kind: u32,
    /// The kind's flags.
    pub flags: u32,
    /// The request a reply answers, or the stage operation an acknowledgement does.
    pub id: u64,
    /// The kind's typed inline body (`#[repr(C)]` `zerocopy` struct, zero-padded to [`BODY_BYTES`]).
    pub body: Body,
    /// A reply's status: 0, or a -errno.
    pub status: i32,
    /// cladefs: how many descriptors the message carries, which come with its first bytes
    /// ([`MAX_FDS`] at most): what a receiver that reads many messages at once gives it of those
    /// that came with them, in order (TASK-91.16). The senders set it.
    pub fds: u32,
    /// The bytes of payload after the header.
    pub len: u64,
}

impl Header {
    pub fn new(kind: u32, id: u64) -> Self {
        Self { kind, id, ..Default::default() }
    }

    pub fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Writes `body` into the start of `self.body`, zeroing any trailing bytes.
    pub fn with_body<T: IntoBytes + Immutable>(mut self, body: &T) -> Self {
        self.set_body(body);
        self
    }

    /// Replaces `self.body` with `body`, zeroing any trailing bytes.
    pub fn set_body<T: IntoBytes + Immutable>(&mut self, body: &T) {
        const { assert!(size_of::<T>() <= BODY_BYTES) };
        self.body = Body::default();
        self.body.0[..size_of::<T>()].copy_from_slice(body.as_bytes());
    }

    /// Reads `T` from the start of `self.body`, ignoring trailing bytes (for pre-payload
    /// admission such as [`super::codec::stream_bytes`]).
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn read_body<T: FromBytes>(&self) -> T {
        const { assert!(size_of::<T>() <= BODY_BYTES) };
        T::read_from_prefix(&self.body.0).expect("body holds T").0
    }

    /// Decodes `T` from the start of `self.body`, refusing (`EINVAL`) any non-zero trailing bytes
    /// past `T`.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode_body<T: FromBytes>(&self) -> Result<T, crate::Errno> {
        const { assert!(size_of::<T>() <= BODY_BYTES) };
        let (body, rest) = T::read_from_prefix(&self.body.0).map_err(|_| crate::Errno::EINVAL)?;
        if rest.iter().any(|&byte| byte != 0) {
            return Err(crate::Errno::EINVAL);
        }
        Ok(body)
    }

    /// Refuses (`EINVAL`) any non-zero bytes in `self.body` for kinds that carry no inline fields.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode_empty(&self) -> Result<(), crate::Errno> {
        if self.body.0.iter().any(|&byte| byte != 0) {
            return Err(crate::Errno::EINVAL);
        }
        Ok(())
    }
}

// Typed #[repr(C)] zerocopy wire structs for Header::body and variable-length payloads.

/// Inline body of [`OPEN`]. `version` sits at [`VERSION_OFFSET`], fixed across every wire version.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct OpenBody {
    pub capacity: u64,
    pub cell: u64,
    pub cell_version: u64,
    pub tenant: u64,
    pub version: u64,
}

/// Inline body of [`READ`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct ReadBody {
    pub cell: u64,
    pub sequence: u64,
    pub offset: u64,
    pub len: u64,
    pub transfer_at: u64,
}

/// Inline body of [`WRITE`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct WriteBody {
    pub cell: u64,
    pub sequence: u64,
    pub offset: u64,
    pub transfer_at: u64,
    pub data_len: u64,
}

/// Inline body of [`FREE`] and [`FREE_MANY`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct ExtentBody {
    pub cell: u64,
    pub sequence: u64,
}

/// Inline body of [`MAP`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct MapBody {
    pub cell: u64,
    pub sequence: u64,
    pub offset: u64,
    pub len: u64,
    pub window_offset: u64,
}

/// Inline body of [`UNMAP`], [`STAGE_UNMAP`] and [`STAGE_MAPPED`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct RangeBody {
    pub offset: u64,
    pub len: u64,
}

/// Inline body of [`WINDOW`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct WindowBody {
    pub size: u64,
    pub budget: u64,
}

/// Inline body of [`FAULT`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct FaultBody {
    pub offset: u64,
}

/// Inline body of [`FORK`] with [`FORK_DROP`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct DropVersionBody {
    pub version: u64,
}

/// Inline body of [`RESIZED`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct ResizedBody {
    pub capacity: u64,
    pub took: u64,
}

/// Inline body of [`TRANSFER`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct TransferBody {
    pub len: u64,
}

/// Inline body of [`STATS`] and [`STATS_ASK`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct StatsBody {
    pub ask: u64,
}

/// Inline body of [`CAPACITY`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct CapacityBody {
    pub capacity: u64,
}

/// Inline body of [`ACK`] and [`STAGE_COUNTS`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct StageCountsBody {
    pub ops: u64,
    pub ns: u64,
    pub overwrites: u64,
    pub leave_outs: u64,
    pub installed: u32,
    pub private: u32,
}

/// Inline body of [`STAGE_PRIVATE_COUNTS`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct PrivateCountsBody {
    pub runs: u64,
    pub pages: u64,
    pub ns: u64,
    pub requests: u64,
    pub request_ns: u64,
}

/// Inline body of [`STAGE_MAP`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct StageMapBody {
    pub offset: u64,
    pub phys_offset: u64,
    pub len: u64,
    pub arena: u32,
    pub reserved: u32,
}

/// Inline body of a reply to [`OPEN`]. `version` sits at [`VERSION_OFFSET`], fixed across every
/// wire version.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct OpenedBody {
    pub cell: u64,
    pub tenant: u64,
    pub max_map_count: u64,
    pub arena: u32,
    pub flags: u32,
    pub version: u64,
}

/// Inline body of a reply to [`INFO`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct InfoBody {
    pub capacity: u64,
    pub used: u64,
    pub available: u64,
    pub next_sequence: u64,
    pub sequence_floor: u64,
}

/// Inline body of a reply to [`FORK`] with [`FORK_QUIESCENT`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub(crate) struct ForkedBody {
    pub version: u64,
    pub tenant: u64,
    pub committed: u64,
    pub flags: u32,
    pub reserved: u32,
}

/// Wire layout of one extent ID in a [`FREE_MANY`] payload (little-endian, unaligned).
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    FromBytes,
    IntoBytes,
    Immutable,
    KnownLayout,
    Unaligned,
)]
pub(crate) struct ExtentIdWire {
    pub cell: zerocopy::byteorder::little_endian::U64,
    pub sequence: zerocopy::byteorder::little_endian::U64,
}

/// Wire layout of one window mapping in a [`MAPPINGS`] reply or [`MAP_MANY`] payload
/// (little-endian, unaligned).
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    FromBytes,
    IntoBytes,
    Immutable,
    KnownLayout,
    Unaligned,
)]
pub(crate) struct MappingWire {
    pub window_offset: zerocopy::byteorder::little_endian::U64,
    pub len: zerocopy::byteorder::little_endian::U64,
    pub cell: zerocopy::byteorder::little_endian::U64,
    pub sequence: zerocopy::byteorder::little_endian::U64,
    pub offset: zerocopy::byteorder::little_endian::U64,
    pub flags: zerocopy::byteorder::little_endian::U64,
}

const _: () = {
    assert!(size_of::<Header>() == 72);
    assert!(std::mem::offset_of!(Header, body) == 16);
    assert!(std::mem::offset_of!(OpenBody, version) == VERSION_OFFSET);
    assert!(std::mem::offset_of!(OpenedBody, version) == VERSION_OFFSET);
    assert!(size_of::<OpenBody>() == BODY_BYTES);
    assert!(size_of::<OpenedBody>() == BODY_BYTES);
    assert!(size_of::<StageCountsBody>() == BODY_BYTES);
    assert!(size_of::<PrivateCountsBody>() == BODY_BYTES);
    assert!(size_of::<InfoBody>() == BODY_BYTES);
    assert!(size_of::<ExtentIdWire>() == EXTENT_ID_BYTES);
    assert!(size_of::<MappingWire>() as u64 == MAPPING_BYTES);
};

/// cladefs: the wire's version (TASK-141.3), which a change to any message, its kinds, flags,
/// arguments, payloads, bounds or what they mean, makes a new one: the VM process says its own in
/// its [`OPEN`], which a daemon of another refuses (`EPROTONOSUPPORT`), logging both, before
/// anything else of it; and the daemon says its own in its reply, which the VM process refuses if
/// another, naming both. Where it is said, `args[4]` of an `OPEN` and of its reply, with the
/// [`Header`]'s layout, [`OPEN`]'s and [`REPLY`]'s kinds and a reply's status, is what no version
/// moves. `the_wire_is_its_version` (codec.rs) fails on a change to the wire without a new one.
/// Version 4: a request may say the guest request it is a call for ([`REQUEST`], TASK-260).
/// Version 5: a commit may complete once local, synced to the host's disk ([`COMMIT_LOCAL`],
/// [`WRITE_LOCAL`], TASK-301). Version 6: an [`OPEN`] may make a SQLite cell
/// ([`OPEN_SQLITE`], TASK-364), and cells are kept in their prefixes. Version
/// 7: a [`FORK`] names no cell: its reply says the one the daemon gave (TASK-364). Version 8:
/// an [`OPEN`] may make a memory cell ([`OPEN_MEMORY`], TASK-456.2). Version 9: the
/// flags doc-4 has too are at doc-4's bits, under its names ([`COMMIT_UPLOADED`], [`COMMIT_ROOT`],
/// [`MAP_ZEROS`], [`WRITE_UPLOADED`], [`WRITE_ROOT`], TASK-474). Version 10: an [`OPEN`] forks a
/// version of a cell it names ([`OPEN_FORK`]), and a [`FORK`] of a VM snapshot's makes a version,
/// which its reply names (doc-20). Version 11: a [`FORK`] with [`FORK_DROP`] drops a version its
/// connection made, as a VM snapshot failing after it does (TASK-575). Version 12: a `FREE_MANY`'s
/// and a `MAP_MANY`'s stream bytes count what the daemon decodes of them (TASK-614); a transfer
/// area is written whole by its VM process, which seals it against no more, before it gives it
/// (TASK-547); a payload on an `ACK`, `STAGE_COUNTS` or `STAGE_PRIVATE_COUNTS` ends its connection
/// (TASK-581). Version 13: a VM process copies on write itself (doc-26): no `FAULT_WRITE`,
/// `PRIVATE` or `STAGE_PRIVATE`; a [`WINDOW`] says it copies with [`WINDOW_COPYING`], not its
/// private pages; a [`STAGE_UNMAP`] keeps the private pages there with [`STAGE_KEEP_PRIVATE`];
/// [`STAGE_COUNTS`] says the private pages. Version 14: typed `#[repr(C)]` `zerocopy` inline
/// message bodies replace positional `args: [u64; 5]`, refusing non-zero trailing/reserved bytes
/// (board 6917). Version 15: a payload past what its kind holds ends its connection at its header,
/// as one on an `ACK` did (`codec::most_payload`), and so does a payload, or what the daemon queued
/// for a VM process, that moves no byte for [`STALL`] (board 8459); an `ACK` of a stage operation
/// never sent lets its VM process go (board 8463); a `ROOT`'s reply is counted in its stream bytes
/// as it is admitted (board 8465).
/// Version 16: no followers (board 8237): no `OPEN_READ_ONLY`, `ADVANCE`, `LET_GO`, `PIN`, `UNPIN`
/// nor `COMMITTED`, and an [`OPEN`]'s reply says no commit.
/// Version 17: [`FORK`] takes no path (`ForkCell` covers it; board 10612): only [`FORK_QUIESCENT`]
/// and [`FORK_DROP`], and its reply carries no cell ID.
/// Version 18: [`OPEN_EPHEMERAL`] for creating standalone ephemeral cells and reopening host-local
/// ephemeral cells (rootfs at committed epoch, memory cells in place); [`OpenedBody`] carries
/// `tenant` and `flags` ([`OPENED_EPHEMERAL`]); [`ForkedBody`] carries `committed` and `flags`
/// ([`FORKED_EPHEMERAL`]) for local-only quiesced ephemeral snapshots.
pub(crate) const VERSION: u64 = 18;

/// cladefs: a flag of any request, which says the guest request it is a call for, the guest's
/// number of it (`crate::request`, TASK-260): little-endian, not 0, in the first
/// [`REQUEST_BYTES`] of its payload, before what its kind has there. The daemon serves the request
/// with it current, which its log records name.
pub(crate) const REQUEST: u32 = 1 << 31;
/// cladefs: the bytes of a [`REQUEST`]'s guest request.
pub(crate) const REQUEST_BYTES: usize = 8;

// Requests, from a VM process. A reply ([`REPLY`]) answers each, but for [`ACK`].

/// Opens the cell at the path the payload holds, creating it with `args[0]` bytes of capacity if
/// [`OPEN_CREATE`] and it doesn't exist; or, with [`OPEN_FORK`], forking it from the version
/// `args[2]` of the cell `args[1]` of tenant `args[3]` if it doesn't exist (TASK-39.1: a VM
/// restored from a snapshot), or is a fork that failed, whose deletion it finishes first
/// (TASK-79.84), of `args[0]` bytes: a fork into a cell of the connection's tenant, of another
/// tenant's version only if it is shared (`EACCES` otherwise), `ENOENT` if the bucket has no such
/// version. The reply's `args[0]` is its cell, and it carries the kernel's physical memory.
/// cladefs: the reply says how many mappings
/// the host lets a process make, 0 if it doesn't limit them, which a VM process in a jail without
/// /proc can't read (TASK-172). cladefs: its `args[4]` is the VM process's wire [`VERSION`], and
/// its reply's the daemon's.
pub(crate) const OPEN: u32 = 1;
/// Reads `args[3]` bytes at `args[2]` in extent `args[0..2]` (`READ`): the reply's payload; or,
/// with [`READ_TRANSFER`], into the connection's transfer area at `args[4]` ([`TRANSFER`]).
pub(crate) const READ: u32 = 2;
/// Writes the payload at `args[2]` in extent `args[0..2]` (`WRITE`), after a barrier if
/// [`WRITE_BARRIER`], and followed by a commit if [`WRITE_COMMIT`]; or, with [`WRITE_TRANSFER`],
/// the `args[4]` bytes at `args[3]` in the connection's transfer area ([`TRANSFER`]). cladefs: with
/// [`WRITE_ROOT`] too, that commit sets the root to what of the payload follows the data, the
/// first `args[4]` bytes of it without [`WRITE_TRANSFER`] (TASK-153.13).
pub(crate) const WRITE: u32 = 3;
/// Frees extent `args[0..2]` (`FREE`).
pub(crate) const FREE: u32 = 4;
/// Maps `args[3]` bytes at `args[2]` in extent `args[0..2]`, sealed, into the window at `args[4]`,
/// read-only (`MAP`); or zeros there, with [`MAP_ZEROS`], as a snapshot's mappings may
/// ([`MAPPINGS`]). cladefs: flag 1, writably, was writable DAX's, not reused (backlog doc-13).
pub(crate) const MAP: u32 = 5;
/// Unmaps `args[1]` bytes of the window at `args[0]` (`UNMAP`).
pub(crate) const UNMAP: u32 = 6;
/// Commits (`COMMIT`), with the payload as the root if [`COMMIT_ROOT`].
pub(crate) const COMMIT: u32 = 7;
/// Reads the root (`ROOT`): the reply's payload.
pub(crate) const ROOT: u32 = 8;
/// What the cell holds (`INFO`): the reply's `args`, as [`ExtentInfo`](crate::ExtentInfo)'s
/// fields in order.
pub(crate) const INFO: u32 = 9;
/// Creates the cell's window, of `args[0]` bytes, which the VM process maps in its own memory as
/// the daemon's [stage operations](STAGE_MAP) say. cladefs: a writer's copies on write (backlog
/// doc-13), [`WINDOW_COPYING`], in its VM process, which bounds its private pages, its own memory
/// (doc-26): a write to a page makes the page private there, which the daemon knows nothing of.
/// `args[1]` is 0. `args[2]` is how many runs of pages the VM process keeps installed at most, each a mapping
/// of its own, by the mappings the host lets it make ([`OPEN`]'s reply): 0 if it doesn't say, which
/// the daemon's metrics show (TASK-111.5).
pub(crate) const WINDOW: u32 = 10;
/// Resolves the guest's fault at `args[0]` in the window, a read, on that page alone if
/// [`FAULT_PAGE`]: cladefs: a write's too, which its VM process copies or refuses itself once the
/// window names the page (doc-26). The stage operations it takes come before the reply.
pub(crate) const FAULT: u32 = 11;
/// Acknowledges stage operation `id`, which the VM process has made; and says, as
/// [`STAGE_COUNTS`] does, what its stage operations have cost it so far.
pub(crate) const ACK: u32 = 12;
/// Makes or drops a version of the cell: with [`FORK_QUIESCENT`], no payload, it makes a version
/// of the cell, a VM snapshot's, which any number of hosts fork at once and none writes: `EBUSY`
/// unless nothing is open ([`OpenCell::is_quiescent`](crate::OpenCell::is_quiescent)); the
/// reply's [`ForkedBody`] carries the version's ID and the cell's tenant. cladefs: with
/// [`FORK_DROP`], no payload, it drops the version [`DropVersionBody::version`], one this
/// connection made: what a VM snapshot that failed past making its versions made (TASK-575).
/// Forking by path went in version 17 (`ForkCell` covers it; board 10612).
pub(crate) const FORK: u32 = 13;
/// The guest's answer to a [`CAPACITY`] that asked it to shrink to `args[0]` bytes: whether it
/// took that size, `args[1]` 1, or not, 0.
pub(crate) const RESIZED: u32 = 16;
/// The window's mappings ([`OpenCell::window_mappings`](crate::OpenCell::window_mappings)):
/// the reply's payload, each six little-endian words ([`MAPPING_BYTES`]): its offset in the
/// window, its length, the extent's cell and sequence and the offset in it, and 0 or
/// [`MAPPING_ZEROS`]. As many as a window of its size holds, a mapping a
/// page at most, which may be more than [`MAX_PAYLOAD`] holds (TASK-79.43).
pub(crate) const MAPPINGS: u32 = 17;
/// The bytes of a mapping in a [`MAPPINGS`] reply.
pub(crate) const MAPPING_BYTES: u64 = 6 * 8;
// 14, 15 and 18 to 20 were requests the host's tools made, of the daemon's stats, and to resize,
// fork, delete and sync cells by path: the control API's now (TASK-43.5), not reused.
// cladefs: 21 to 24, a follower's `ADVANCE`, `LET_GO`, `PIN` and `UNPIN`, are not reused (board
// 8237).
/// The connection's transfer area, the shared memory it carries, of `args[0]` bytes: what
/// `READ`s and `WRITE`s with [`READ_TRANSFER`] and [`WRITE_TRANSFER`] move their data through,
/// rather than the stream, whose sending and receiving copy it twice more (TASK-26.18.6). The VM
/// process says which part of it each uses, which it has no other use for meanwhile.
pub(crate) const TRANSFER: u32 = 25;
/// cladefs: frees extent `args[0..2]`, and those the payload names after it ([`EXTENT_ID_BYTES`]),
/// each as [`FREE`] frees it, in order: the first that fails fails the request, those before it
/// freed, as a guest's `FREE` with doc-4's `FREE_MANY` does, a flush's dead extents in one round
/// trip, not one each (TASK-92.13).
pub(crate) const FREE_MANY: u32 = 26;
/// The most bytes a transfer area holds.
pub(crate) const MAX_TRANSFER_AREA: u64 = 64 << 20;

/// cladefs: the most bytes a VM process's DAX window has (`WINDOW`, `EINVAL` past them): a number
/// it chooses, which sizes what the daemon keeps for it, a mapping a page, and the room it holds
/// its replies in, whatever else it asks (TASK-150.3). 750 MiB of those at most, of 4 KiB pages;
/// the suites' guests' 2 GiB windows, and their soak's, are far within it.
pub(crate) const MAX_WINDOW: u64 = crate::protocol::MAX_WINDOW;
/// cladefs: the stage operations the VM process has made of the connection's, `args[0]`, the
/// nanoseconds they took it, `args[1]`, and those of them that took access away from pages it had
/// installed, by how, `args[2..4]` (`Stats::stage_overwrites`), so far, which the daemon counts as
/// its own (`Stats::stage_ops`, TASK-91.16); and the runs of pages it has installed now, `args[4]`'s
/// low 32 bits, which [`WINDOW`]'s `args[2]` bounds (TASK-111.5), and its window's private pages,
/// its high 32 (doc-26): said with each [`ACK`]; otherwise once
/// [`STAGE_COUNTS_EVERY`] has passed since it last said, as it makes them; before each [`STATS`];
/// and as it closes the connection. Nothing replies.
pub(crate) const STAGE_COUNTS: u32 = 27;
/// cladefs: the VM process's build, the commit it was built of ([`crate::BUILD_COMMIT`]), the
/// payload, [`MAX_BUILD`] bytes at most, of letters, digits, `.`, `_` and `-`: which the daemon's
/// metrics show by its cell (TASK-111.7). Said once, after [`OPEN`], as every VM process
/// does: refused, the VM process's connection fails (TASK-141.3).
pub(crate) const BUILD: u32 = 28;
// 29 was the guest's answer to an ask to seal, writable DAX's (TASK-153.15), not reused.
/// cladefs: the most bytes a [`BUILD`]'s commit has.
pub(crate) const MAX_BUILD: usize = 64;
/// cladefs: the most bytes a root holds ([`COMMIT_ROOT`], [`WRITE_ROOT`]), `EINVAL` past it: the
/// extent device's (doc-4's `max_root`, Firecracker's `MAX_ROOT`), which a guest's root is within
/// (TASK-149.17). A VM process's was as large as a payload, 64 MiB, which its cell kept, and wrote
/// with its commits, and which each `ROOT` copied into its reply, uncounted: 16 of them at once
/// held 768 MiB.
#[cfg(test)]
pub(crate) const MAX_ROOT: usize = crate::protocol::MAX_ROOT;
// 30 made a window's pages private, doc-4's `PRIVATE`, which a VM process serves itself now
// (doc-26), not reused.
/// cladefs: the VM process's answer to the daemon's [`STATS_ASK`] `ask`: with [`STATS_GUEST`],
/// its guest's stats, the payload, UTF-8 text of [`MAX_STATS`] bytes at most, as the guest's
/// driver said them (backlog doc-4's `STATS`): lines of a name and a value, a number, or a
/// histogram's numbers between commas, and its build, `commit COMMIT`. Without, it has no guest to
/// ask: its device not activated, or its driver taking no asks. It says its stage counts
/// ([`STAGE_COUNTS`]) just before, so that the daemon has them as of the answer. A guest's answer
/// to an ask answers those before it too, the device asking the latest alone (TASK-111.4).
pub(crate) const STATS: u32 = 31;
/// cladefs: a [`STATS`] carries its guest's stats.
pub(crate) const STATS_GUEST: u32 = 1 << 0;
/// cladefs: the most bytes of a guest's stats a [`STATS`] carries, many times what the guest's
/// Fxfs says (a few KiB).
#[cfg(any(target_os = "linux", test))]
pub(crate) const MAX_STATS: usize = crate::protocol::MAX_STATS;
/// cladefs: maps each mapping the payload holds, as a reply to [`MAPPINGS`] has them, in order,
/// each as [`MAP`] would, in one change of the window: the first that fails fails the request,
/// those before it mapped. A VM restored from a snapshot maps its window's mappings again so, the
/// whole window in a request, where a `MAP` each took a round trip and a change of the window's
/// second stage apiece (TASK-92.14). A request of its own rather than a flag of `MAP`'s, as
/// [`FREE_MANY`] is: a daemon older than it refuses it (`EINVAL`).
pub(crate) const MAP_MANY: u32 = 32;
/// cladefs: the runs of private pages the VM process made, each at once, `args[0]`, the pages
/// they made, `args[1]`, and the nanoseconds they took it, `args[2]`; and the guest's `PRIVATE`s
/// it served, `args[3]`, and the nanoseconds they took it, `args[4]`, so far (`Stats::stage_private_runs`,
/// TASK-164.10): said once [`STAGE_COUNTS_EVERY`] has passed since it last said, as it makes them,
/// and as it closes the connection. Nothing replies.
pub(crate) const STAGE_PRIVATE_COUNTS: u32 = 33;
/// cladefs: how long a VM process's stage operations go uncounted at most while it makes them
/// ([`STAGE_COUNTS`]): within a sample of the daemon's stats file (`CLADEFSD_STATS`).
#[cfg(target_os = "linux")]
pub(crate) const STAGE_COUNTS_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

pub(crate) const OPEN_CREATE: u32 = 1 << 0;
pub(crate) const OPEN_FORK: u32 = 1 << 1;
// cladefs: 1 << 2, a follower's `OPEN_READ_ONLY`, is not reused (board 8237).
/// cladefs: with [`OPEN_CREATE`], the cell made is a SQLite cell, which the
/// sqlite_cladefs VFS alone opens, not an Fxfs one (TASK-364).
pub(crate) const OPEN_SQLITE: u32 = 1 << 3;
/// cladefs: with [`OPEN_CREATE`], the cell made is a memory cell, a VM's memory
/// (TASK-456.2), not an Fxfs one. With [`OPEN_EPHEMERAL`] alone, opens an existing ephemeral
/// memory cell in place.
pub(crate) const OPEN_MEMORY: u32 = 1 << 4;
/// cladefs: host-local ephemeral cell open (doc-25): with [`OPEN_CREATE`] (and optionally
/// [`OPEN_SQLITE`] or [`OPEN_MEMORY`]), creates a standalone ephemeral cell with zero bucket
/// uploads; alone, reopens an existing ephemeral cell in place at `cell_version` (`committed`) for
/// local VM restore (`EEXIST` on mismatch); with [`OPEN_MEMORY`], reopens an existing ephemeral
/// memory cell in place.
pub(crate) const OPEN_EPHEMERAL: u32 = 1 << 5;
/// cladefs: [`OpenedBody::flags`] bit indicating the opened cell is host-local ephemeral.
pub(crate) const OPENED_EPHEMERAL: u32 = 1 << 0;
/// cladefs: [`ForkedBody::flags`] bit indicating a host-local quiesced ephemeral snapshot
/// (`committed` holds the latest commit number and `version` is 0).
pub(crate) const FORKED_EPHEMERAL: u32 = 1 << 0;
/// cladefs: doc-4's `MAP_ZEROS`, at its bit. Bit 0 was writable DAX's; doc-4's bits 1 and 2,
/// `MAP_MANY` and `MAP_PREFAULT`, are no flags here: [`MAP_MANY`] is a request of its own, and a VM
/// process prefaults by itself.
pub(crate) const MAP_ZEROS: u32 = crate::protocol::MAP_ZEROS;
pub(crate) const FORK_QUIESCENT: u32 = 1 << 0;
/// cladefs: a [`FORK`] that drops a version the connection made (TASK-575).
pub(crate) const FORK_DROP: u32 = 1 << 1;
pub(crate) const MAPPING_ZEROS: u64 = 1 << 1;
/// cladefs: a `COMMIT` that completes once uploaded, not once ordered: doc-4's `COMMIT_UPLOADED`,
/// at its bit, as every flag doc-4 has too is here.
pub(crate) const COMMIT_UPLOADED: u32 = crate::protocol::COMMIT_UPLOADED;
/// A `COMMIT` that sets the root: doc-4's `COMMIT_ROOT`, at its bit.
pub(crate) const COMMIT_ROOT: u32 = crate::protocol::COMMIT_ROOT;
/// cladefs: a `COMMIT` that completes once local, synced to the host's disk, not waiting for the
/// bucket (TASK-301): not with [`COMMIT_UPLOADED`]. Not doc-4's.
pub(crate) const COMMIT_LOCAL: u32 = 1 << 2;
pub(crate) const WRITE_BARRIER: u32 = crate::protocol::WRITE_BARRIER;
pub(crate) const WRITE_COMMIT: u32 = crate::protocol::WRITE_COMMIT;
/// A `WRITE`'s commit completes once uploaded: doc-4's `WRITE_UPLOADED`, at its bit, as are
/// [`WRITE_BARRIER`], [`WRITE_COMMIT`] and [`WRITE_ROOT`].
pub(crate) const WRITE_UPLOADED: u32 = crate::protocol::WRITE_UPLOADED;
/// cladefs: a `WRITE`'s commit sets the root.
pub(crate) const WRITE_ROOT: u32 = crate::protocol::WRITE_ROOT;
/// Not doc-4's: a `WRITE`'s data is in the transfer area.
pub(crate) const WRITE_TRANSFER: u32 = 1 << 4;
/// cladefs: a `WRITE`'s commit completes once local, synced to the host's disk (TASK-301): not
/// with [`WRITE_UPLOADED`]. Not doc-4's.
pub(crate) const WRITE_LOCAL: u32 = 1 << 5;
pub(crate) const READ_TRANSFER: u32 = 1 << 0;
// `FAULT`'s bit 0 said a write (`FAULT_WRITE`), which a VM process copies itself now (doc-26).
pub(crate) const FAULT_PAGE: u32 = 1 << 1;
/// cladefs: a [`FAULT`] on a page alone, to read, that is the VM process's own probe of the page,
/// mapping it ahead of the guest's accesses, not the guest's access: the daemon tells of no
/// refusal of it, as the guest may map nothing there.
pub(crate) const FAULT_PROBE: u32 = 1 << 2;
/// cladefs: the bytes an extent's ID takes in a [`FREE_MANY`]'s payload: its cell and its
/// sequence, little-endian words.
pub(crate) const EXTENT_ID_BYTES: usize = 16;

// From the daemon.

/// The reply to request `id`.
pub(crate) const REPLY: u32 = 100;
/// Maps `args[2]` bytes of physical memory at `args[1]` in arena `args[4]`'s object
/// into the window at `args[0]`, read-only.
pub(crate) const STAGE_MAP: u32 = 101;
// 102 changed a range's protection, as writable DAX's seals did, not reused.
/// Unmaps `args[1]` bytes of the window at `args[0]`: cladefs: ending the private pages there,
/// unless [`STAGE_KEEP_PRIVATE`] (doc-26).
pub(crate) const STAGE_UNMAP: u32 = 103;
/// cladefs: the window maps `args[1]` bytes at `args[0]` anew (`SecondStage::window_mapped`).
pub(crate) const STAGE_MAPPED: u32 = 106;
// 108 made a page private, the daemon's copy on write, which a VM process makes itself now (doc-26),
// not reused.
/// The cell's capacity is `args[0]` bytes now, which the guest takes (the control API's
/// `ResizeCell`); or, with [`CAPACITY_ASK`], the guest is asked to shrink to `args[0]` bytes, and
/// answers ([`RESIZED`]).
pub(crate) const CAPACITY: u32 = 104;
pub(crate) const CAPACITY_ASK: u32 = 1 << 0;
// cladefs: 105, a follower's `COMMITTED`, is not reused (board 8237).
/// cladefs: asks the VM process for its stats and its guest's, as of now: ask `args[0]`, which its
/// [`STATS`] answers, those asked before it among them (TASK-111.4).
pub(crate) const STATS_ASK: u32 = 109;
// 107 asked the guest to seal, writable DAX's (TASK-153.15), not reused.
/// A stage operation the daemon waits for the VM process to acknowledge ([`ACK`]), as it must for
/// one that takes access to a page away, before it frees the page or shares it.
pub(crate) const STAGE_ACK: u32 = 1 << 0;
/// cladefs: a [`STAGE_UNMAP`] of the host's own, an eviction's or a free's, which leaves the
/// private pages there (doc-26, spec rule evictionKeepsPrivate): without it, the guest's `MAP` or
/// `UNMAP`, which ends them.
pub(crate) const STAGE_KEEP_PRIVATE: u32 = 1 << 1;
/// cladefs: a [`WINDOW`] whose VM process copies on write, a writer's (doc-26).
pub(crate) const WINDOW_COPYING: u32 = 1 << 0;

/// The most bytes of payload a message carries: a `READ` or `WRITE` of a whole extent, or less.
pub(crate) const MAX_PAYLOAD: u64 = crate::OpenCell::MAX_EXTENT;

/// cladefs: the most requests a VM process keeps sent and not answered, its replies not yet
/// received (TASK-79.16): many times what its device's queues and its window's faults have at once.
/// A VM process past it the daemon lets go.
pub(crate) const MAX_REQUESTS: usize = 4096;

/// cladefs: the most bytes of data a VM process's requests not answered move through the stream
/// ([`stream_bytes`](super::codec::stream_bytes), TASK-79.16): `WRITE`s' payloads and `READ`s'
/// replies, but those through the transfer area ([`TRANSFER`]); a few times its largest. A VM
/// process past it, as the daemon finds what it holds of it, and what it has queued for it,
/// unread, the daemon lets go.
pub(crate) const MAX_STREAM_BYTES: u64 = 4 * MAX_PAYLOAD;

/// The most file descriptors a message carries.
pub(crate) const MAX_FDS: usize = 2;

/// cladefs: how long a VM process may leave a message half moved, no byte of it moving, before the
/// daemon ends its connection (board 8459): a payload it admitted none of which comes
/// ([`AsyncReceiver::receive`]), or what is queued for it that its socket takes none of
/// ([`AsyncSender::stalled`]). Either holds its tenant's bytes in flight, which an idle connection
/// held for good, every other VM process of the tenant's requests waiting for them. As long as a VM
/// process may leave a stage operation unacknowledged (`server::UNRESPONSIVE`): a VM process moves
/// a message whole, at once, as it sends and reads on threads of its own (hotbed question 481).
pub(crate) const STALL: std::time::Duration = std::time::Duration::from_secs(5);

/// A message received.
pub(crate) struct Message {
    pub header: Header,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// The sending half of a VM process's connection, which any of its threads sends whole messages
/// through, each waiting for room as the socket fills: the daemon's is an [`AsyncSender`].
/// cladefs: Linux's alone, as its client is.
#[cfg(target_os = "linux")]
pub(crate) struct Sender(Mutex<SenderInner>);

#[cfg(target_os = "linux")]
struct SenderInner {
    stream: UnixStream,
    /// Bounded scratch buffer for streaming volatile write payloads without gathering multi-MiB
    /// `Vec`s in the VMM (TASK-661).
    scratch: Vec<u8>,
}

/// The most bytes [`Sender::send_stream`] copies at once from guest memory into the socket.
#[cfg(target_os = "linux")]
const STREAM_CHUNK: usize = 64 << 10;

#[cfg(target_os = "linux")]
impl Sender {
    pub fn new(stream: UnixStream) -> Self {
        Self(Mutex::new(SenderInner { stream, scratch: Vec::new() }))
    }

    /// Sends `header`, with `payload` as its payload, and `fds`, at most [`MAX_FDS`].
    pub fn send(&self, header: Header, payload: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        self.send_parts(header, &[payload], fds)
    }

    /// cladefs: sends `header`, with `payload`'s parts, one after another, as its payload, and
    /// `fds`, at most [`MAX_FDS`]: a write's data and its root, which no copy joins (TASK-160.9).
    pub fn send_parts(
        &self,
        header: Header,
        payload: &[&[u8]],
        fds: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        self.send_stream(header, payload, &[], &[], fds)
    }

    /// cladefs: sends `header`, followed by `prefix`, the bytes of `buffers` streamed through one
    /// bounded scratch buffer ([`STREAM_CHUNK`]), and `suffix`, with `fds`, at most [`MAX_FDS`]:
    /// avoids gathering multi-MiB `WRITE` payloads into VMM heap `Vec`s (TASK-661).
    pub fn send_stream(
        &self,
        mut header: Header,
        prefix: &[&[u8]],
        buffers: &[crate::VolatileSlice<'_>],
        suffix: &[&[u8]],
        fds: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        assert!(fds.len() <= MAX_FDS, "too many descriptors for a message");
        let prefix_len: u64 = prefix.iter().map(|part| part.len() as u64).sum();
        let stream_len: u64 = buffers.iter().map(|buf| buf.len() as u64).sum();
        let suffix_len: u64 = suffix.iter().map(|part| part.len() as u64).sum();
        header.len = prefix_len + stream_len + suffix_len;
        header.fds = fds.len() as u32;
        let mut inner = self.0.lock().unwrap();
        let SenderInner { stream, scratch } = &mut *inner;
        let header_bytes = header.as_bytes();
        // The daemon's connections are non-blocking, for their receivers (TASK-34): a full one is
        // waited for.
        let sent = loop {
            match send_with_fds(stream, header_bytes, fds) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_writable(stream)?,
                sent => break sent?,
            }
        };
        write_all(stream, &header_bytes[sent..])?;
        for part in prefix {
            write_all(stream, part)?;
        }
        if stream_len > 0 {
            let total = stream_len as usize;
            let chunk_len = total.min(STREAM_CHUNK);
            if scratch.len() < chunk_len {
                scratch.resize(chunk_len, 0);
            }
            let mut at = 0;
            while at < total {
                let take = (total - at).min(chunk_len);
                let copied = crate::memory::gather(buffers, at, &mut scratch[..take]);
                debug_assert_eq!(copied, take);
                write_all(stream, &scratch[..copied])?;
                at += copied;
            }
        }
        for part in suffix {
            write_all(stream, part)?;
        }
        Ok(())
    }

    /// Shuts the connection down, which ends its receiver's messages.
    pub fn shutdown(&self) {
        // Shut down already, or never connected: nothing more to end.
        let _ = self.0.lock().unwrap().stream.shutdown(std::net::Shutdown::Both);
    }
}

/// cladefs: the sending half of one of the daemon's connections, which any thread sends whole
/// messages through, in order, none waiting for the VM process to take them (TASK-79.9): the
/// kernel sends its stage operations with its locks held, and a VM process that stops reading
/// would hold them for good. What the socket has no room for waits in a queue, which a task on the
/// runtime it was made on writes as the socket takes it; what is sent meanwhile queues behind.
pub(crate) struct AsyncSender {
    stream: Socket,
    queue: Mutex<Queue>,
    /// How many bytes the queue holds, which the socket has not taken (TASK-79.16).
    queued: std::sync::atomic::AtomicU64,
    /// cladefs: where those bytes are counted too, if anywhere: the room its tenant's connections
    /// share (TASK-149.35).
    counted: Option<std::sync::Arc<super::memory::InFlight>>,
    runtime: tokio::runtime::Handle,
}

/// What an [`AsyncSender`]'s socket has not taken yet.
#[derive(Default)]
struct Queue {
    /// The messages, and their descriptors, which they carry with their first bytes.
    messages: std::collections::VecDeque<(Queued, Vec<OwnedFd>)>,
    /// Whether a task writes them.
    draining: bool,
    /// Why the socket takes no more, once it doesn't.
    failed: Option<io::ErrorKind>,
    /// cladefs: when the socket last took any of them, or, if it took none since, the first came:
    /// how long the VM process has read none of what waits for it ([`AsyncSender::stalled`]).
    moved: Option<std::time::Instant>,
}

/// A message, as much of it as the socket has not taken.
struct Queued {
    header: [u8; size_of::<Header>()],
    payload: Vec<u8>,
    /// How many of its bytes, its header's then its payload's, were sent.
    sent: usize,
}

impl Queued {
    /// How many of its bytes are not sent.
    fn left(&self) -> u64 {
        (self.header.len() + self.payload.len() - self.sent) as u64
    }

    /// What of its header, and of its payload, is not sent.
    fn unsent(&self) -> (&[u8], &[u8]) {
        match self.sent.checked_sub(self.header.len()) {
            None => (&self.header[self.sent..], &self.payload[..]),
            Some(at) => (&[][..], &self.payload[at..]),
        }
    }
}

impl AsyncSender {
    /// Sends through `stream`, which the caller made non-blocking, on the runtime it is made on.
    /// cladefs: and a simulated guest's (`super::guest`).
    #[cfg(any(test, madsim))]
    pub fn new(stream: Stream) -> io::Result<Self> {
        Self::made(stream, None)
    }

    /// cladefs: sends through `stream` as [`AsyncSender::new`] does, the bytes it queues counted in
    /// `counted` too, as long as they are queued (TASK-149.35).
    pub fn counting(
        stream: Stream,
        counted: std::sync::Arc<super::memory::InFlight>,
    ) -> io::Result<Self> {
        Self::made(stream, Some(counted))
    }

    fn made(
        stream: Stream,
        counted: Option<std::sync::Arc<super::memory::InFlight>>,
    ) -> io::Result<Self> {
        let stream = Socket::writing(stream)?;
        let runtime = tokio::runtime::Handle::current();
        let queued = Default::default();
        Ok(Self { stream, queue: Mutex::default(), queued, counted, runtime })
    }

    /// cladefs: `bytes` more queued.
    fn queue_more(&self, bytes: u64) {
        self.queued.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        if let Some(counted) = &self.counted {
            counted.add(bytes);
        }
    }

    /// cladefs: `bytes` fewer queued: all of them, without `bytes`.
    fn queue_less(&self, bytes: Option<u64>) {
        use std::sync::atomic::Ordering::Relaxed;
        let bytes = match bytes {
            Some(bytes) => {
                self.queued.fetch_sub(bytes, Relaxed);
                bytes
            }
            None => self.queued.swap(0, Relaxed),
        };
        if let Some(counted) = &self.counted {
            counted.sub(bytes);
        }
    }

    /// How many bytes of what was sent through it its socket has not taken yet.
    pub fn queued(&self) -> u64 {
        self.queued.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the other end hung up, as a VM process that went does, though the receiving end
    /// may not have read so yet.
    pub fn peer_gone(&self) -> bool {
        self.stream.hung_up()
    }

    /// Sends `header`, with `payload` as its payload, and `fds`, at most [`MAX_FDS`], after what
    /// was sent before: at once, what the socket takes, and the rest as it takes it. Fails once
    /// the socket failed, as the other end gone.
    pub fn send(
        self: &std::sync::Arc<Self>,
        header: Header,
        payload: Vec<u8>,
        fds: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        self.queue(header, payload, fds)
    }

    /// cladefs: sends `headers`, messages of no payload, as [`AsyncSender::send`] would one after
    /// another, but at once, in as few writes as the socket takes: the operations of one change to
    /// a window, which its VM process then reads, and makes, together (TASK-91.16). `headers` is
    /// called with the sender's queue locked, so that what it makes, such as the IDs it gives,
    /// is in the order it is sent in.
    pub fn send_all(
        self: &std::sync::Arc<Self>,
        headers: impl FnOnce() -> Vec<Header>,
    ) -> io::Result<()> {
        let mut queue = self.queue.lock().unwrap();
        for header in headers() {
            self.push(&mut queue, header, Vec::new(), Vec::new())?;
        }
        self.flush(&mut queue)
    }

    /// Sends `header`, `payload` and `fds`, as [`AsyncSender::send`] does.
    fn queue(
        self: &std::sync::Arc<Self>,
        header: Header,
        payload: Vec<u8>,
        fds: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        assert!(fds.len() <= MAX_FDS, "too many descriptors for a message");
        // The descriptors are the queue's until it sends them, with the message's first bytes.
        let fds = fds.iter().map(|fd| fd.try_clone_to_owned()).collect::<io::Result<_>>()?;
        let mut queue = self.queue.lock().unwrap();
        self.push(&mut queue, header, payload, fds)?;
        self.flush(&mut queue)
    }

    /// Puts `header`, `payload` and `fds` at the end of `queue`; fails once the socket failed.
    fn push(
        &self,
        queue: &mut Queue,
        mut header: Header,
        payload: Vec<u8>,
        fds: Vec<OwnedFd>,
    ) -> io::Result<()> {
        header.len = payload.len() as u64;
        header.fds = fds.len() as u32;
        let header = header.as_bytes().try_into().unwrap();
        let message = Queued { header, payload, sent: 0 };
        if let Some(failed) = queue.failed {
            return Err(failed.into());
        }
        self.queue_more(message.left());
        if queue.messages.is_empty() {
            queue.moved = Some(std::time::Instant::now());
        }
        queue.messages.push_back((message, fds));
        Ok(())
    }

    /// cladefs: how long, as of `now`, its socket has taken none of what is queued: none with
    /// nothing queued (board 8459). A VM process past [`STALL`] the daemon lets go.
    pub fn stalled(&self, now: std::time::Instant) -> std::time::Duration {
        let queue = self.queue.lock().unwrap();
        match (queue.messages.is_empty(), queue.moved) {
            (false, Some(moved)) => now.saturating_duration_since(moved),
            _ => std::time::Duration::ZERO,
        }
    }

    /// Writes what `queue` holds, unless the task draining it does: fails once the socket failed.
    fn flush(self: &std::sync::Arc<Self>, queue: &mut Queue) -> io::Result<()> {
        if !queue.draining && queue.failed.is_none() {
            self.write_queued(queue);
        }
        queue.failed.map_or(Ok(()), |failed| Err(failed.into()))
    }

    /// Writes what of the messages in `queue` the socket takes now, and has a task write the rest
    /// as it takes it; or, the socket failed, fails the queue, which the sends say from now on, as
    /// a VM process gone makes them fail.
    fn write_queued(self: &std::sync::Arc<Self>, queue: &mut Queue) {
        match self.send_queued(queue) {
            Ok(true) => {}
            Ok(false) => {
                queue.draining = true;
                self.runtime.spawn(self.clone().drain());
            }
            Err(err) => {
                queue.failed = Some(err.kind());
                queue.messages.clear();
                self.queue_less(None);
            }
        }
    }

    /// Sends what of the messages in `queue` the socket takes, many in each write: whether it took
    /// them all. A message's descriptors go with its first bytes, which begin a write.
    fn send_queued(&self, queue: &mut Queue) -> io::Result<bool> {
        /// The most messages a write sends: their headers and payloads, each an `iovec`.
        const MOST: usize = 64;
        while !queue.messages.is_empty() {
            let fds: Vec<BorrowedFd<'_>> = match queue.messages.front() {
                Some((message, fds)) if message.sent == 0 => fds.iter().map(AsFd::as_fd).collect(),
                _ => Vec::new(),
            };
            let mut parts: Vec<&[u8]> = Vec::with_capacity(2 * MOST);
            for (at, (message, more_fds)) in queue.messages.iter().take(MOST).enumerate() {
                // Another message's descriptors begin a write of their own.
                if at > 0 && !more_fds.is_empty() {
                    break;
                }
                let (header, payload) = message.unsent();
                parts.extend([header, payload]);
            }
            let sent = match self.stream.send_parts(&parts, &fds) {
                Ok(sent) => sent,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(err) => return Err(err),
            };
            drop(parts);
            drop(fds);
            self.queue_less(Some(sent as u64));
            queue.moved = Some(std::time::Instant::now());
            let mut left = sent;
            while left > 0 {
                let (message, _) = queue.messages.front_mut().expect("bytes sent of a message");
                let taken = (message.left() as usize).min(left);
                message.sent += taken;
                left -= taken;
                if message.left() == 0 {
                    queue.messages.pop_front();
                }
            }
        }
        Ok(true)
    }

    /// Writes the messages queued as the socket takes them, until none is left, or it fails.
    async fn drain(self: std::sync::Arc<Self>) {
        loop {
            let mut ready = match self.stream.writable().await {
                Ok(ready) => ready,
                Err(err) => return self.fail(err),
            };
            let mut queue = self.queue.lock().unwrap();
            match self.send_queued(&mut queue) {
                Ok(true) => {
                    queue.draining = false;
                    return;
                }
                Ok(false) => {}
                Err(err) => {
                    drop(queue);
                    return self.fail(err);
                }
            }
            ready.clear_ready();
        }
    }

    /// The socket failed with `err`: what is queued goes, and nothing is sent from now on.
    fn fail(&self, err: io::Error) {
        log::warn!("cladefsd: sending to a VM process: {err}");
        let mut queue = self.queue.lock().unwrap();
        queue.failed = Some(err.kind());
        queue.draining = false;
        let dropped = std::mem::take(&mut queue.messages);
        self.queue_less(None);
        drop(queue);
        drop(dropped);
    }

    /// Shuts the connection down, which ends its receiver's messages: a failure logged, but for
    /// the connection's being gone already (`ENOTCONN`).
    pub fn shutdown(&self) {
        if let Err(err) = self.stream.shutdown()
            && err.kind() != io::ErrorKind::NotConnected
        {
            log::warn!("cladefsd: shutting a VM process's connection down: {err}");
        }
    }
}

/// cladefs: what it has queued, gone with it, is held no more where it was counted (TASK-149.35).
impl Drop for AsyncSender {
    fn drop(&mut self) {
        self.queue_less(None);
    }
}

/// Whether the other end of the stream `fd` hung up, though its receiving end may not have read so
/// yet. cladefs: a Unix stream's, outside the simulation, whose connections are in memory.
#[cfg(not(madsim))]
pub(crate) fn hung_up(fd: BorrowedFd<'_>) -> bool {
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
    // SAFETY: Polls a descriptor the caller keeps open, without waiting.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    ready > 0 && poll.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

/// Waits until `stream` takes more bytes.
#[cfg(target_os = "linux")]
fn wait_writable(stream: &UnixStream) -> io::Result<()> {
    let mut poll = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
    loop {
        // SAFETY: Polls the stream's descriptor, which it keeps open.
        if unsafe { libc::poll(&mut poll, 1, -1) } >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Writes all of `bytes` to `stream`, blocking or not, waiting for room as it fills.
#[cfg(target_os = "linux")]
fn write_all(stream: &UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match (&*stream).write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_writable(stream)?,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Sends what of `bytes` one `sendmsg` does, with `fds`: how many bytes it sent, at least one.
#[cfg(target_os = "linux")]
fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
    send_parts(stream, &[bytes], fds)
}

/// Sends what of `parts`, in order, one `sendmsg` does, with `fds`: how many bytes it sent, at
/// least one.
#[cfg(target_os = "linux")]
fn send_parts(stream: &UnixStream, parts: &[&[u8]], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
    let mut iov: Vec<libc::iovec> = (parts.iter())
        .map(|part| libc::iovec { iov_base: part.as_ptr() as *mut _, iov_len: part.len() })
        .collect();
    let len = (fds.len() * size_of::<RawFd>()) as u32;
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(len) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: A zeroed msghdr is a valid empty one.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = iov.len() as _;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: `msg`'s control buffer has room for one header and the descriptors.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(len) as _;
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for (index, fd) in fds.iter().enumerate() {
                std::ptr::write_unaligned(data.add(index), fd.as_raw_fd());
            }
        }
    }
    loop {
        // SAFETY: `msg` points at live buffers for the call.
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if sent >= 0 {
            return Ok(sent as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// The receiving half of a connection, which one thread receives messages through. cladefs: it
/// reads what the socket holds, many messages at once, rather than a message's header and payload
/// each a read (TASK-91.16): a VM process's stage operations come in runs, a window change's
/// together. The descriptors that come with what it reads it keeps, in order, for the messages
/// that say they carry them ([`Header::fds`]). cladefs: a VM process's, Linux's alone, as its client
/// is (`super::client`).
#[cfg(target_os = "linux")]
pub(crate) struct Receiver {
    stream: UnixStream,
    /// What it read and has not received yet: `buffer[start..end]`.
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    /// The descriptors that came with what it read, for the messages not received yet.
    fds: std::collections::VecDeque<OwnedFd>,
}

/// cladefs: the bytes a [`Receiver`] reads at most at once.
#[cfg(target_os = "linux")]
const RECEIVE_BYTES: usize = 64 << 10;

#[cfg(target_os = "linux")]
impl Receiver {
    pub fn new(stream: UnixStream) -> Self {
        let buffer = vec![0; RECEIVE_BYTES].into_boxed_slice();
        Self { stream, buffer, start: 0, end: 0, fds: Default::default() }
    }

    /// cladefs: takes what follows the stage unmap `unmap`, already waiting whole in the socket, of
    /// the window's change it begins (TASK-91.16): first the acknowledged unmaps adjacent to it or
    /// overlapping it, which it joins, then the window's mapping anew of the range they unmap
    /// ([`STAGE_MAPPED`]) and its pages ([`STAGE_MAP`]), each within it, which it returns, in
    /// order, for the VM process to make with the unmap at once. Up to 63 messages, without
    /// crossing another, a gap, or partial input. The unmap's ID becomes the largest of those it
    /// joined, whose acknowledgement is of every ID before it, as IDs reach the wire in order
    /// (TASK-160.11): not a later map's, which takes no access away. Each message is taken as
    /// [`Stage::decode`] has it: one it refuses, it leaves.
    /// cladefs: unmaps join only those that keep the private pages, or end them, as it does
    /// (doc-26).
    pub fn take_change(&mut self, unmap: &mut Stage) -> io::Result<Vec<crate::dax::Anew>> {
        let StageOp::Unmap { offset, len, keep_private } = unmap.op else { return Ok(Vec::new()) };
        let (mut start, Some(mut end)) = (offset, offset.checked_add(len)) else {
            return Ok(Vec::new());
        };
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut id = unmap.id;
        let mut anew = false;
        let taken = self.take_waiting(|next| {
            // One it refuses, it leaves, as the doc says.
            let next = Stage::decode(next).ok()?;
            let (at, len, made) = match next.op {
                StageOp::Unmap { offset, len, keep_private: keep }
                    if !anew && len != 0 && keep == keep_private =>
                {
                    let next_end = offset.checked_add(len)?;
                    if offset > end || next_end < start {
                        return None;
                    }
                    (start, end) = (start.min(offset), end.max(next_end));
                    id = id.max(next.id);
                    return Some(None);
                }
                StageOp::Mapped { offset, len } => {
                    (offset, len, crate::dax::Anew::Mapped { offset, len })
                }
                StageOp::Map { offset, phys, len } => {
                    (offset, len, crate::dax::Anew::Map { offset, phys, len })
                }
                _ => return None,
            };
            anew = true;
            let within = at >= start && at.checked_add(len).is_some_and(|at_end| at_end <= end);
            (len != 0 && within).then_some(Some(made))
        })?;
        unmap.id = id;
        unmap.op = StageOp::Unmap { offset: start, len: end - start, keep_private };
        Ok(taken.into_iter().flatten().collect())
    }

    /// cladefs: receives the messages already read or waiting whole in the socket, up to 63, as
    /// long as `take` takes each, in order, none with a payload or descriptors: a read of the
    /// socket's, at most, that waits for nothing. Bounded, so that what is made of them together,
    /// and acknowledged once all is, waits for no more. What `take` makes of each, it returns.
    fn take_waiting<T>(
        &mut self,
        mut take: impl FnMut(&Header) -> Option<T>,
    ) -> io::Result<Vec<T>> {
        const MOST: usize = 63;
        self.read(libc::MSG_DONTWAIT)?;
        let mut taken = Vec::new();
        while taken.len() < MOST
            && let Some(next) = self.buffered_header()
        {
            if next.len != 0 || next.fds != 0 {
                break;
            }
            let Some(made) = take(&next) else { break };
            self.start += size_of::<Header>();
            taken.push(made);
        }
        Ok(taken)
    }

    /// The next message, or `None` once the other end has closed the connection: one whose payload
    /// is past [`MAX_PAYLOAD`] only if `most`, asked then, allows that many bytes, as a reply to
    /// [`MAPPINGS`] of a large window may hold (TASK-79.43).
    pub fn receive(&mut self, most: impl FnOnce() -> u64) -> io::Result<Option<Message>> {
        let header = loop {
            if let Some(header) = self.buffered_header() {
                break header;
            }
            if self.read(0)? == 0 {
                return match self.start == self.end {
                    true => Ok(None),
                    false => Err(io::ErrorKind::UnexpectedEof.into()),
                };
            }
        };
        self.start += size_of::<Header>();
        if header.len > MAX_PAYLOAD + size_of::<Header>() as u64 && header.len > most() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a message too large"));
        }
        // Its descriptors came with its first bytes, which were read.
        let carried = header.fds as usize;
        if carried > MAX_FDS || carried > self.fds.len() {
            let what = format!("a message of {carried} descriptors, {} received", self.fds.len());
            return Err(io::Error::new(io::ErrorKind::InvalidData, what));
        }
        let fds = self.fds.drain(..carried).collect();
        let mut payload = vec![0; header.len as usize];
        let buffered = payload.len().min(self.end - self.start);
        payload[..buffered].copy_from_slice(&self.buffer[self.start..self.start + buffered]);
        self.start += buffered;
        // The rest straight from the socket: no more than the payload, which carries no
        // descriptors, as they come with a message's first bytes.
        self.stream.read_exact(&mut payload[buffered..])?;
        Ok(Some(Message { header, payload, fds }))
    }

    /// The next header, if what was read holds it whole.
    fn buffered_header(&self) -> Option<Header> {
        let bytes = self.buffer.get(self.start..self.start + size_of::<Header>())?;
        (self.start + size_of::<Header>() <= self.end)
            .then(|| Header::read_from_bytes(bytes).unwrap())
    }

    /// Reads what the socket holds into the buffer, after what it holds already, moved to its
    /// start, with the descriptors that come with it, with `flags` (`MSG_DONTWAIT`, or none to wait
    /// for some): how many bytes, 0 at the end of the stream, or with nothing waiting.
    fn read(&mut self, flags: libc::c_int) -> io::Result<usize> {
        self.buffer.copy_within(self.start..self.end, 0);
        (self.start, self.end) = (0, self.end - self.start);
        if self.end == self.buffer.len() {
            return Ok(0);
        }
        match receive_with_fds(&self.stream, &mut self.buffer[self.end..], flags) {
            Ok((read, fds)) => {
                self.end += read;
                self.fds.extend(fds);
                Ok(read)
            }
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock && flags & libc::MSG_DONTWAIT != 0 =>
            {
                Ok(0)
            }
            Err(err) => Err(err),
        }
    }
}

/// cladefs: the receiving half of one of the daemon's connections, on a runtime, which receives
/// every connection's messages on a few threads rather than a thread each (TASK-34): its socket
/// non-blocking, read as it is ready.
pub(crate) struct AsyncReceiver(Socket);

impl AsyncReceiver {
    /// Receives from `stream`, which it makes non-blocking, as the runtime it is made on is ready
    /// to.
    pub fn new(stream: Stream) -> io::Result<Self> {
        Ok(Self(Socket::reading(stream)?))
    }

    /// The next message, or `None` once the other end has closed the connection, as
    /// [`Receiver::receive`] gives it, a payload of [`MAX_PAYLOAD`] at most: if `admit`, given its
    /// header, admits it, before its payload is read (TASK-79.16), with what admitting it gave, as
    /// what the daemon holds for it counted: which, its payload never read whole, is dropped
    /// with it (TASK-548). cladefs: `TimedOut` once no byte of a payload it admitted has come for
    /// [`STALL`], as a VM process sending a header and nothing more held what it was admitted
    /// with for good (board 8459).
    pub async fn receive<A, F: std::future::Future<Output = io::Result<A>>>(
        &mut self,
        admit: impl FnOnce(&Header) -> F,
    ) -> io::Result<Option<(Message, A)>> {
        let mut header = Header::default();
        let mut fds = Vec::new();
        {
            let bytes = header.as_mut_bytes();
            let mut at = 0;
            while at < bytes.len() {
                // The descriptors come with the message's first bytes.
                let (received, mut more) = self.read(&mut bytes[at..], at == 0).await?;
                if received == 0 {
                    if at == 0 {
                        return Ok(None);
                    }
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                fds.append(&mut more);
                at += received;
            }
        }
        if header.len > MAX_PAYLOAD + size_of::<Header>() as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a message too large"));
        }
        let admitted = admit(&header).await?;
        let mut payload = vec![0; header.len as usize];
        let mut at = 0;
        while at < payload.len() {
            let Ok(read) = tokio::time::timeout(STALL, self.read(&mut payload[at..], false)).await
            else {
                let what = format!(
                    "a payload stalled, {at} of its {} bytes received and none for {STALL:?}",
                    payload.len()
                );
                return Err(io::Error::new(io::ErrorKind::TimedOut, what));
            };
            match read? {
                (0, _) => return Err(io::ErrorKind::UnexpectedEof.into()),
                (received, _) => at += received,
            }
        }
        Ok(Some((Message { header, payload, fds }, admitted)))
    }

    /// Reads what of `bytes` the socket has, once it has any: with the descriptors that came with
    /// them, if `with_fds`.
    async fn read(&self, bytes: &mut [u8], with_fds: bool) -> io::Result<(usize, Vec<OwnedFd>)> {
        self.0.read(bytes, with_fds).await
    }
}

/// Receives what of `bytes` one `recvmsg` does, and the descriptors that came with them.
/// cladefs: Linux's (`MSG_CMSG_CLOEXEC`), as a Unix stream's VM process is.
#[cfg(target_os = "linux")]
fn receive_with_fds(
    stream: &UnixStream,
    bytes: &mut [u8],
    flags: libc::c_int,
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() };
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: A zeroed msghdr is a valid empty one.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let received = loop {
        // SAFETY: `msg` points at live buffers for the call.
        let received =
            unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC | flags) };
        if received >= 0 {
            break received as usize;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };
    let mut fds = Vec::new();
    // SAFETY: The kernel filled in `msg`'s control buffer, which the macros walk.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let header = libc::CMSG_LEN(0) as usize;
                let count = ((*cmsg).cmsg_len as usize - header) / size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..count {
                    let raw = std::ptr::read_unaligned(data.add(index));
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((received, fds))
}

/// A connected pair of a sender and a receiver of each end, for tests.
#[cfg(test)]
pub(crate) fn pair() -> ((Sender, Receiver), (Sender, Receiver)) {
    let (a, b) = UnixStream::pair().unwrap();
    let ends =
        |stream: UnixStream| (Sender::new(stream.try_clone().unwrap()), Receiver::new(stream));
    (ends(a), ends(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dax::Anew;
    use std::os::fd::AsFd;

    fn unmap(id: u64, offset: u64, len: u64) -> Stage {
        Stage { id, op: StageOp::Unmap { offset, len, keep_private: false } }
    }

    fn map(id: u64, offset: u64, len: u64) -> Stage {
        let phys = crate::pages::PhysLocation { arena: crate::pages::ArenaId(0), offset: 1 << 20 };
        Stage { id, op: StageOp::Map { offset, phys, len } }
    }

    fn mapped(id: u64, offset: u64, len: u64) -> Stage {
        Stage { id, op: StageOp::Mapped { offset, len } }
    }

    /// What a change takes of `stage`, a map or a mapping anew.
    fn anew(stage: Stage) -> Anew {
        match stage.op {
            StageOp::Map { offset, phys, len } => Anew::Map { offset, phys, len },
            StageOp::Mapped { offset, len } => Anew::Mapped { offset, len },
            op => panic!("{op:?} is taken as no map"),
        }
    }

    #[test]
    fn unmaps_join_adjacent_ranges_and_ack_only_consumed_taking_ids() {
        let ((sender, _), (_, mut receiver)) = pair();
        // Includes reversed address order, overlap, and IDs allocated in a different order.
        for stage in [
            unmap(7, 8192, 4096),
            unmap(3, 4096, 4096),
            unmap(5, 4096, 8192),
            unmap(6, 12288, 4096),
        ] {
            sender.send(stage.encode(), &[], &[]).unwrap();
        }
        let mapped = mapped(99, 4096, 12288);
        sender.send(mapped.encode(), &[], &[]).unwrap();
        let reply = Header::new(REPLY, 100);
        let file = tempfile::tempfile().unwrap();
        sender.send(reply, &[], &[file.as_fd()]).unwrap();
        let header = receiver.receive(|| 0).unwrap().unwrap().header;
        let mut stage = Stage::decode(&header).unwrap();
        assert_eq!(receiver.take_change(&mut stage).unwrap(), [anew(mapped)]);
        assert_eq!(stage, unmap(7, 4096, 12288));
        let message = receiver.receive(|| 0).unwrap().unwrap();
        assert_eq!(message.header, Header { fds: 1, ..reply }, "the header says its descriptor");
        assert_eq!(message.fds.len(), 1);
    }

    /// cladefs: an unmap takes the window's mapping anew of what it unmaps, and the maps of its
    /// pages, each within it, in order, up to the first that isn't (TASK-91.16): one outside it,
    /// one of another kind, or an unmap after them, which begins the next change.
    #[test]
    fn unmaps_take_the_maps_within_them() {
        for (after, taken) in
            [(map(5, 8192, 8192), 3), (unmap(5, 4096, 4096), 3), (map(5, 4096, 8192), 4)]
        {
            let ((sender, _), (_, mut receiver)) = pair();
            let sent = [mapped(2, 4096, 8192), map(3, 4096, 4096), map(4, 8192, 4096), after];
            for stage in sent {
                sender.send(stage.encode(), &[], &[]).unwrap();
            }
            let mut first = unmap(1, 4096, 8192);
            let made: Vec<_> = sent[..taken].iter().copied().map(anew).collect();
            assert_eq!(receiver.take_change(&mut first).unwrap(), made);
            assert_eq!(first, unmap(1, 4096, 8192), "the unmap's ID is never a map's");
            if taken < sent.len() {
                assert_eq!(receiver.receive(|| 0).unwrap().unwrap().header, after.encode());
            }
        }
    }

    #[test]
    fn unmap_batches_leave_gaps_other_operations_and_partial_headers_untouched() {
        for variation in 0..6 {
            let ((sender, _), (_, mut receiver)) = pair();
            let mut first = unmap(1, 4096, 4096);
            assert_eq!(receiver.take_change(&mut first).unwrap(), Vec::<Anew>::new());
            let mut next = unmap(2, 8192, 4096).encode();
            match variation {
                0 => {
                    let mut body = next.read_body::<RangeBody>();
                    body.offset += 4096;
                    next.set_body(&body);
                }
                // An unmap keeping the private pages, after one ending them (doc-26).
                1 => next.flags |= STAGE_KEEP_PRIVATE,
                2 => next.flags = 0,
                3 => next.set_body(&RangeBody { offset: u64::MAX - 1024, len: 4096 }),
                4 => {}
                5 => next.len = 3,
                _ => unreachable!(),
            }
            if variation == 4 {
                write_all(&sender.0.lock().unwrap().stream, &next.as_bytes()[..17]).unwrap();
                assert_eq!(receiver.take_change(&mut first).unwrap(), Vec::<Anew>::new());
                assert_eq!(first, unmap(1, 4096, 4096));
                write_all(&sender.0.lock().unwrap().stream, &next.as_bytes()[17..]).unwrap();
                assert_eq!(receiver.take_change(&mut first).unwrap(), Vec::<Anew>::new());
                assert_eq!(first, unmap(2, 4096, 8192));
            } else {
                let payload = vec![9; next.len as usize];
                sender.send(next, &payload, &[]).unwrap();
                assert_eq!(receiver.take_change(&mut first).unwrap(), Vec::<Anew>::new());
                assert_eq!(first, unmap(1, 4096, 4096));
                let received = receiver.receive(|| 0).unwrap().unwrap();
                assert_eq!((received.header, received.payload), (next, payload));
            }
        }
    }

    #[test]
    fn unmap_batches_bound_the_work_taken_from_the_socket() {
        let ((sender, _), (_, mut receiver)) = pair();
        for id in 1..=128 {
            sender.send(unmap(id, id * 4096, 4096).encode(), &[], &[]).unwrap();
        }
        let mut next_id = 1;
        let mut total_joined = 0;
        while next_id <= 128 {
            let header = receiver.receive(|| 0).unwrap().unwrap().header;
            let mut stage = Stage::decode(&header).unwrap();
            assert_eq!(stage.id, next_id);
            assert_eq!(receiver.take_change(&mut stage).unwrap(), Vec::<Anew>::new());
            let StageOp::Unmap { len, .. } = stage.op else { panic!("{stage:?} is no unmap") };
            let count = len / 4096;
            assert!(count <= 64, "{count} unmaps joined");
            assert_eq!(stage, unmap(next_id + count - 1, next_id * 4096, count * 4096));
            total_joined += count - 1;
            next_id += count;
        }
        assert!(total_joined > 0);
    }

    /// Messages arrive whole and in order, with their payloads, and descriptors sent with one
    /// arrive with it.
    #[test]
    fn messages_carry_payloads_and_descriptors() {
        let ((sender, _), (_, mut receiver)) = pair();
        let (file, other) = (tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap());
        let big = vec![7u8; 3 << 20];
        let sent = std::thread::spawn(move || {
            let body = ReadBody { cell: 1, sequence: 2, offset: 3, len: 0, transfer_at: 0 };
            sender.send(Header::new(READ, 1).with_body(&body), b"", &[]).unwrap();
            let fds = [file.as_fd(), other.as_fd()];
            sender.send(Header::new(WRITE, 2).with_flags(5), &big, &fds).unwrap();
        });
        let first = receiver.receive(|| 0).unwrap().unwrap();
        let body = ReadBody { cell: 1, sequence: 2, offset: 3, len: 0, transfer_at: 0 };
        assert_eq!(first.header, Header::new(READ, 1).with_body(&body));
        assert!(first.payload.is_empty() && first.fds.is_empty());
        let second = receiver.receive(|| 0).unwrap().unwrap();
        assert_eq!((second.header.kind, second.header.id, second.header.flags), (WRITE, 2, 5));
        assert_eq!(second.payload, vec![7u8; 3 << 20]);
        assert_eq!(second.fds.len(), 2, "the descriptors didn't arrive");
        sent.join().unwrap();
    }

    /// cladefs: the daemon's sends wait for no room (TASK-79.9): what its socket has none for
    /// queues, and arrives as the other end reads, whole and in order, with its descriptors; once
    /// the other end is gone, sends fail.
    #[test]
    fn async_sends_queue_what_the_socket_has_no_room_for() {
        let runtime =
            tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build();
        let runtime = runtime.unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let sender = std::sync::Arc::new({
            let _entered = runtime.enter();
            AsyncSender::new(ours).unwrap()
        });
        let file = tempfile::tempfile().unwrap();
        // Many times what the socket holds, none read meanwhile.
        let big = vec![7u8; 4 << 20];
        for id in 0..8 {
            let fds = if id == 5 { vec![file.as_fd()] } else { Vec::new() };
            sender.send(Header::new(READ, id), big.clone(), &fds).unwrap();
        }
        let mut receiver = Receiver::new(theirs);
        for id in 0..8 {
            let message = receiver.receive(|| 0).unwrap().unwrap();
            assert_eq!((message.header.id, message.payload.len()), (id, big.len()));
            assert!(message.payload == big, "message {id} holds other bytes");
            assert_eq!(message.fds.len(), usize::from(id == 5), "message {id}'s descriptors");
        }
        drop(receiver);
        let gone = sender.send(Header::new(READ, 8), Vec::new(), &[]);
        assert_eq!(gone.map_err(|err| err.kind()), Err(io::ErrorKind::BrokenPipe));
    }

    /// cladefs: what one `send_all` sends arrives together, in order, after what was sent before
    /// it (TASK-91.16); IDs it gives as it makes the messages, under the queue's lock, reach the
    /// other end in the order they were given, whatever other threads send meanwhile
    /// (TASK-160.11): a stage operation's ID sent behind a later one's would be acknowledged by it.
    #[test]
    fn sends_of_a_batch_arrive_in_the_order_made() {
        let runtime =
            tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build();
        let runtime = runtime.unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let sender = std::sync::Arc::new({
            let _entered = runtime.enter();
            AsyncSender::new(ours).unwrap()
        });
        let mut receiver = Receiver::new(theirs);
        let mut ids = |count: usize| -> Vec<u64> {
            (0..count).map(|_| receiver.receive(|| 0).unwrap().unwrap().header.id).collect()
        };
        let file = tempfile::tempfile().unwrap();
        sender.send(Header::new(REPLY, 1), Vec::new(), &[file.as_fd()]).unwrap();
        let batch = || vec![Header::new(STAGE_UNMAP, 2), Header::new(STAGE_MAP, 3)];
        sender.send_all(batch).unwrap();
        assert_eq!(ids(3), [1, 2, 3]);

        let (threads, batches) = (8, 100);
        let next = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let senders: Vec<_> = (0..threads)
            .map(|_| {
                let (sender, next) = (sender.clone(), next.clone());
                std::thread::spawn(move || {
                    for _ in 0..batches {
                        let id = || next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let batch = || (0..3).map(|_| Header::new(STAGE_MAP, id())).collect();
                        sender.send_all(batch).unwrap();
                    }
                })
            })
            .collect();
        let received = ids(threads * batches * 3);
        senders.into_iter().for_each(|sender| sender.join().unwrap());
        let expected: Vec<u64> = (0..(threads * batches * 3) as u64).collect();
        assert!(received == expected, "IDs out of order: {received:?}");
    }
}
