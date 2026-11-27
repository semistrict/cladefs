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
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

/// cladefs: a VM process's connection in memory, under madsim (doc-24).
#[cfg(madsim)]
mod memory;

/// cladefs: a VM process's connection, as the daemon serves it: a Unix stream; under madsim, which
/// has no Unix sockets, an end of a connection in memory, a simulated guest's at the other
/// ([`memory::Stream::pair`], doc-24's boundary).
#[cfg(not(madsim))]
pub type Stream = UnixStream;
#[cfg(madsim)]
pub use memory::Stream;

/// cladefs: the socket the daemon's async sender and receiver use, the one place they differ
/// under madsim: a [`Stream`] on the runtime's reactor, sending with `sendmsg` and reading with
/// `recvmsg`; or the connection in memory, its calls those of [`memory::Stream`] (doc-24).
pub struct Socket {
    #[cfg(not(madsim))]
    fd: tokio::io::unix::AsyncFd<UnixStream>,
    #[cfg(madsim)]
    stream: memory::Stream,
}

/// cladefs: that a [`Socket`] takes more, until cleared: the reactor's readiness, or, in memory,
/// always, as it takes all it is sent.
pub struct Writable<'a> {
    #[cfg(not(madsim))]
    ready: tokio::io::unix::AsyncFdReadyGuard<'a, UnixStream>,
    #[cfg(madsim)]
    socket: std::marker::PhantomData<&'a Socket>,
}

impl Writable<'_> {
    /// It took what it could: to be waited for again.
    pub fn clear_ready(&mut self) {
        #[cfg(not(madsim))]
        self.ready.clear_ready();
    }
}

impl Socket {
    /// `stream`, the caller's, non-blocking, to write.
    fn writing(stream: Stream) -> io::Result<Self> {
        #[cfg(not(madsim))]
        {
            no_sigpipe(&stream)?;
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
pub const BODY_BYTES: usize = 40;

/// The byte offset inside [`Body`] where [`OpenBody::version`] and [`OpenedBody::version`] sit
/// (byte offset 48 of [`Header`]), fixed across every wire version so a version mismatch is
/// refused with `EPROTONOSUPPORT` before decoding anything else (TASK-141.3).
pub const VERSION_OFFSET: usize = 32;

/// The 40-byte, 8-byte-aligned inline body of a [`Header`].
#[repr(C, align(8))]
#[derive(Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Body(pub [u8; BODY_BYTES]);

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
pub struct Header {
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
    pub fn decode_body<T: FromBytes>(&self) -> Result<T, cladefs_core::Errno> {
        const { assert!(size_of::<T>() <= BODY_BYTES) };
        let (body, rest) =
            T::read_from_prefix(&self.body.0).map_err(|_| cladefs_core::Errno::EINVAL)?;
        if rest.iter().any(|&byte| byte != 0) {
            return Err(cladefs_core::Errno::EINVAL);
        }
        Ok(body)
    }

    /// Refuses (`EINVAL`) any non-zero bytes in `self.body` for kinds that carry no inline fields.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode_empty(&self) -> Result<(), cladefs_core::Errno> {
        if self.body.0.iter().any(|&byte| byte != 0) {
            return Err(cladefs_core::Errno::EINVAL);
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
pub struct OpenBody {
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
pub struct ReadBody {
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
pub struct WriteBody {
    pub cell: u64,
    pub sequence: u64,
    pub offset: u64,
    pub transfer_at: u64,
    pub data_len: u64,
}

/// Inline body of [`FREE`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct ExtentBody {
    pub cell: u64,
    pub sequence: u64,
}

/// Inline body of [`UNMAP`], [`STAGE_UNMAP`] and [`STAGE_MAPPED`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RangeBody {
    pub offset: u64,
    pub len: u64,
}

/// Inline body of [`WINDOW`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct WindowBody {
    pub size: u64,
    pub budget: u64,
}

/// Inline body of [`FAULT`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct FaultBody {
    pub offset: u64,
}

/// Inline body of [`FORK`] with [`FORK_DROP`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct DropVersionBody {
    pub version: u64,
}

/// Inline body of [`RESIZED`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct ResizedBody {
    pub capacity: u64,
    pub took: u64,
}

/// Inline body of [`TRANSFER`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct TransferBody {
    pub len: u64,
}

/// cladefs: inline body of [`VHOST_USER`]: the extent device's request queues (board 14537).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct VhostUserBody {
    pub queues: u64,
}

/// Inline body of [`STATS`] and [`STATS_ASK`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct StatsBody {
    pub ask: u64,
}

/// Inline body of [`CAPACITY`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct CapacityBody {
    pub capacity: u64,
}

/// Inline body of [`ACK`] and [`STAGE_COUNTS`].
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct StageCountsBody {
    pub ops: u64,
    pub ns: u64,
    pub overwrites: u64,
    pub leave_outs: u64,
    pub installed: u32,
    pub private: u32,
}

/// Payload of [`STAGE_COUNTS`] (board 11697): what a VM process's runs of private pages and
/// `PRIVATE` requests cost it so far.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct PrivateCountsBody {
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
pub struct StageMapBody {
    pub offset: u64,
    pub phys_offset: u64,
    pub len: u64,
    pub arena: u32,
    pub reserved: u32,
}

/// Entry in a [`STAGE_REMAP`]'s payload (board 11673): either [`STAGE_MAPPED`] (naming a newly
/// mapped window range, with `arena == 0` and `phys_offset == 0`) or [`STAGE_MAP`] (mapping
/// physical pages at `arena`/`phys_offset`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct StageAnewEntry {
    pub kind: u32,
    pub arena: u32,
    pub offset: u64,
    pub phys_offset: u64,
    pub len: u64,
}

/// Inline body of a reply to [`OPEN`]. `version` sits at [`VERSION_OFFSET`], fixed across every
/// wire version.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct OpenedBody {
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
pub struct InfoBody {
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
pub struct ForkedBody {
    pub version: u64,
    pub tenant: u64,
    pub committed: u64,
    pub flags: u32,
    pub reserved: u32,
}

/// Wire layout of one extent ID in a [`FREE`] payload (little-endian, unaligned).
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
pub struct ExtentIdWire {
    pub cell: zerocopy::byteorder::little_endian::U64,
    pub sequence: zerocopy::byteorder::little_endian::U64,
}

/// Wire layout of one window mapping in a [`MAPPINGS`] reply or [`MAP`] payload
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
pub struct MappingWire {
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
/// `MAP_ZEROS`, [`WRITE_UPLOADED`], [`WRITE_ROOT`], TASK-474). Version 10: an [`OPEN`] forks a
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
/// Version 19: a [`STAGE_COUNTS`]'s `id` is its window's budget now, said again as it changes
/// (board 10098).
/// Version 20: [`STAGE_REMAP`] sends coalesced Anew remap entries in a single message (board
/// 11673).
/// Version 21: [`MAPPINGS`] takes [`MAPPINGS_QUIESCENT`] to check that no extent is open before
/// returning the window's mappings (live migration snapshot/release without `make_version`).
/// Version 22: [`MAP_MANY`] takes [`MAP_MANY_WARM`] to warm each mapped extent range in the
/// daemon's extent cache before installing the mappings (board 11681).
/// Version 23: fold `BUILD` into `OPEN`'s `<path>\0<build>` payload and carry the cell's capacity
/// in `OPEN`'s 8-byte reply payload; fold `FREE_MANY` into `FREE`, `MAP_MANY` into `MAP` (dropping
/// `MapBody`/`MAP_ZEROS`, keeping [`MAP_WARM`]), and `STAGE_PRIVATE_COUNTS` into `STAGE_COUNTS`'s
/// 40-byte payload; drop always-true `FAULT_PAGE` (board 11697); register `TRANSFER` lazily on
/// first non-DAX I/O and verify hole-free once at registration (board 11707, hotbed question 575).
/// Version 24: [`VHOST_USER`] hands the VM process the vhost-user channel of its cell's extent
/// device, which the daemon serves as the device's back-end (board 14537).
pub const VERSION: u64 = 24;

/// cladefs: a flag of any request, which says the guest request it is a call for, the guest's
/// number of it (`crate::request`, TASK-260): little-endian, not 0, in the first
/// [`REQUEST_BYTES`] of its payload, before what its kind has there. The daemon serves the request
/// with it current, which its log records name.
pub const REQUEST: u32 = 1 << 31;
/// cladefs: the bytes of a [`REQUEST`]'s guest request.
pub const REQUEST_BYTES: usize = 8;

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
pub const OPEN: u32 = 1;
/// Reads `args[3]` bytes at `args[2]` in extent `args[0..2]` (`READ`): the reply's payload; or,
/// with [`READ_TRANSFER`], into the connection's transfer area at `args[4]` ([`TRANSFER`]).
pub const READ: u32 = 2;
/// Writes the payload at `args[2]` in extent `args[0..2]` (`WRITE`), after a barrier if
/// [`WRITE_BARRIER`], and followed by a commit if [`WRITE_COMMIT`]; or, with [`WRITE_TRANSFER`],
/// the `args[4]` bytes at `args[3]` in the connection's transfer area ([`TRANSFER`]). cladefs: with
/// [`WRITE_ROOT`] too, that commit sets the root to what of the payload follows the data, the
/// first `args[4]` bytes of it without [`WRITE_TRANSFER`] (TASK-153.13).
pub const WRITE: u32 = 3;
/// Frees extent `ExtentBody` and any trailing `ExtentIdWire`s in its payload (`FREE`), in order:
/// the first that fails fails the request, those before it freed (TASK-92.13, board 11697).
pub const FREE: u32 = 4;
/// Maps each [`MappingWire`] the payload holds (one at least, as a reply to [`MAPPINGS`] carries
/// them), in order, read-only, in one change of the window: the first that fails fails the
/// request, those before it mapped (TASK-92.14, board 11697).
pub const MAP: u32 = 5;
/// Unmaps `args[1]` bytes of the window at `args[0]` (`UNMAP`).
pub const UNMAP: u32 = 6;
/// Commits (`COMMIT`), with the payload as the root if [`COMMIT_ROOT`].
pub const COMMIT: u32 = 7;
/// Reads the root (`ROOT`): the reply's payload.
pub const ROOT: u32 = 8;
/// What the cell holds (`INFO`): the reply's `args`, as [`ExtentInfo`](crate::ExtentInfo)'s
/// fields in order.
pub const INFO: u32 = 9;
/// Creates the cell's window, of `args[0]` bytes, which the VM process maps in its own memory as
/// the daemon's [stage operations](STAGE_MAP) say. cladefs: a writer's copies on write (backlog
/// doc-13), [`WINDOW_COPYING`], in its VM process, which bounds its private pages, its own memory
/// (doc-26): a write to a page makes the page private there, which the daemon knows nothing of.
/// `args[1]` is 0. `args[2]` is how many runs of pages the VM process keeps installed at most, each a mapping
/// of its own, by the mappings the host lets it make ([`OPEN`]'s reply): 0 if it doesn't say, which
/// the daemon's metrics show (TASK-111.5).
pub const WINDOW: u32 = 10;
/// Resolves the guest's fault at `FaultBody::offset` in the window, a read: cladefs: a write's
/// too, which its VM process copies or refuses itself once the window names the page (doc-26). The
/// stage operations it takes come before the reply.
pub const FAULT: u32 = 11;
/// Acknowledges stage operation `id`, which the VM process has made; and says, as
/// [`STAGE_COUNTS`] does, what its stage operations have cost it so far.
pub const ACK: u32 = 12;
/// Makes or drops a version of the cell: with [`FORK_QUIESCENT`], no payload, it makes a version
/// of the cell, a VM snapshot's, which any number of hosts fork at once and none writes: `EBUSY`
/// unless nothing is open ([`OpenCell::is_quiescent`](crate::OpenCell::is_quiescent)); the
/// reply's [`ForkedBody`] carries the version's ID and the cell's tenant. cladefs: with
/// [`FORK_DROP`], no payload, it drops the version [`DropVersionBody::version`], one this
/// connection made: what a VM snapshot that failed past making its versions made (TASK-575).
/// Forking by path went in version 17 (`ForkCell` covers it; board 10612).
pub const FORK: u32 = 13;
/// The guest's answer to a [`CAPACITY`] that asked it to shrink to `args[0]` bytes: whether it
/// took that size, `args[1]` 1, or not, 0.
pub const RESIZED: u32 = 16;
/// The window's mappings ([`OpenCell::window_mappings`](crate::OpenCell::window_mappings)):
/// the reply's payload, each six little-endian words ([`MAPPING_BYTES`]): its offset in the
/// window, its length, the extent's cell and sequence and the offset in it, and 0 or
/// [`MAPPING_ZEROS`]. As many as a window of its size holds, a mapping a
/// page at most, which may be more than [`MAX_PAYLOAD`] holds (TASK-79.43).
pub const MAPPINGS: u32 = 17;
/// The bytes of a mapping in a [`MAPPINGS`] reply.
pub const MAPPING_BYTES: u64 = 6 * 8;
// 14, 15 and 18 to 20 were requests the host's tools made, of the daemon's stats, and to resize,
// fork, delete and sync cells by path: the control API's now (TASK-43.5), not reused.
// cladefs: 21 to 24, a follower's `ADVANCE`, `LET_GO`, `PIN` and `UNPIN`, are not reused (board
// 8237).
/// The connection's transfer area, the shared memory it carries, of `args[0]` bytes: what
/// `READ`s and `WRITE`s with [`READ_TRANSFER`] and [`WRITE_TRANSFER`] move their data through,
/// rather than the stream, whose sending and receiving copy it twice more (TASK-26.18.6). The VM
/// process says which part of it each uses, which it has no other use for meanwhile.
pub const TRANSFER: u32 = 25;
// 26 was `FREE_MANY`, folded into `FREE` in version 23 (board 11697), not reused.
/// The most bytes a transfer area holds.
pub const MAX_TRANSFER_AREA: u64 = 64 << 20;

/// cladefs: the most bytes a VM process's DAX window has (`WINDOW`, `EINVAL` past them): a number
/// it chooses, which sizes what the daemon keeps for it, a mapping a page, and the room it holds
/// its replies in, whatever else it asks (TASK-150.3). 750 MiB of those at most, of 4 KiB pages;
/// the suites' guests' 2 GiB windows, and their soak's, are far within it.
pub const MAX_WINDOW: u64 = cladefs_core::protocol::MAX_WINDOW;
/// cladefs: the stage operations the VM process has made of the connection's ([`StageCountsBody`])
/// and its runs of private pages and `PRIVATE` requests ([`PrivateCountsBody`], carried as the
/// 40-byte payload, board 11697), which the daemon counts as its own (`Stats::stage_ops`,
/// TASK-91.16, TASK-164.10): said once [`STAGE_COUNTS_EVERY`] has passed since it last said, as it
/// makes them; before each [`STATS`]; and as it closes the connection. Nothing replies. cladefs:
/// its `id` is the runs of pages its window keeps installed at most now, its share of its
/// process's windows', which [`WINDOW`]'s `budget` said as it was made, 0 before it has one; said
/// again at once as another window of its process comes or goes, changing it (board 10098).
pub const STAGE_COUNTS: u32 = 27;
// 28 was `BUILD`, folded into `OPEN`'s `<path>\0<build>` payload in version 23 (board 11697).
// 29 was the guest's answer to an ask to seal, writable DAX's (TASK-153.15), not reused.
/// cladefs: the most bytes an [`OPEN`] build commit string has (TASK-111.7).
pub const MAX_BUILD: usize = 64;
/// cladefs: the most bytes a root holds ([`COMMIT_ROOT`], [`WRITE_ROOT`]), `EINVAL` past it: the
/// extent device's (doc-4's `max_root`, Firecracker's `MAX_ROOT`), which a guest's root is within
/// (TASK-149.17). A VM process's was as large as a payload, 64 MiB, which its cell kept, and wrote
/// with its commits, and which each `ROOT` copied into its reply, uncounted: 16 of them at once
/// held 768 MiB.
#[cfg(any(test, feature = "testing"))]
pub const MAX_ROOT: usize = cladefs_core::protocol::MAX_ROOT;
// 30 made a window's pages private, doc-4's `PRIVATE`, which a VM process serves itself now
// (doc-26), not reused.
/// cladefs: the VM process's answer to the daemon's [`STATS_ASK`] `ask`: with [`STATS_GUEST`],
/// its guest's stats, the payload, UTF-8 text of [`MAX_STATS`] bytes at most, as the guest's
/// driver said them (backlog doc-4's `STATS`): lines of a name and a value, a number, or a
/// histogram's numbers between commas, and its build, `commit COMMIT`. Without, it has no guest to
/// ask: its device not activated, or its driver taking no asks. It says its stage counts
/// ([`STAGE_COUNTS`]) just before, so that the daemon has them as of the answer. A guest's answer
/// to an ask answers those before it too, the device asking the latest alone (TASK-111.4).
pub const STATS: u32 = 31;
/// cladefs: a [`STATS`] carries its guest's stats.
pub const STATS_GUEST: u32 = 1 << 0;
/// cladefs: the most bytes of a guest's stats a [`STATS`] carries, many times what the guest's
/// Fxfs says (a few KiB).
pub const MAX_STATS: usize = cladefs_core::protocol::MAX_STATS;
// 32 was `MAP_MANY`, folded into `MAP` in version 23 (board 11697), not reused.
// 33 was `STAGE_PRIVATE_COUNTS`, folded into `STAGE_COUNTS`'s payload in version 23 (board 11697).
/// cladefs: the vhost-user channel of the connection's extent device (board 14537), whose back-end
/// the daemon is, serving the guest's doc-4 requests with the connection's cell: the reply carries
/// the front-end's end of a socket pair the daemon made, which the VM process gives its vhost-user
/// device. The channel is bound to the connection's authorized cell and tenant, as a bare
/// vhost-user socket, with no `OPEN`, couldn't be. [`VhostUserBody::queues`] is how many request
/// queues the device has, 1 to [`MAX_VHOST_USER_QUEUES`]; [`VHOST_USER_UNSAFE`] answers the
/// guest's uploaded commits once ordered, as an `Unsafe` cache does (TASK-363); the payload is the
/// device's tag, the guest's mount source, 1 to [`MAX_VHOST_USER_TAG`] bytes, none NUL. Once,
/// after [`OPEN`]: `EBADF` before, `EEXIST` again; `ENOTSUP` where the daemon serves no vhost-user
/// (not Linux, or under madsim).
pub const VHOST_USER: u32 = 34;
/// cladefs: a [`VHOST_USER`] device's cache is `Unsafe`.
pub const VHOST_USER_UNSAFE: u32 = 1 << 0;
/// cladefs: the most request queues a [`VHOST_USER`] device has: its guest's chains in flight, a
/// queue's size (`QUEUE_SIZE`) each queue at most, are half the requests a connection keeps in
/// flight ([`MAX_REQUESTS`]), which counts them too, the other half its VM process's messages'.
pub const MAX_VHOST_USER_QUEUES: u64 =
    (MAX_REQUESTS / 2 / cladefs_core::extent_requests::protocol::QUEUE_SIZE as usize) as u64;
/// cladefs: the most bytes of a [`VHOST_USER`] device's tag (doc-4's).
pub const MAX_VHOST_USER_TAG: usize = cladefs_core::extent_requests::protocol::MAX_TAG_LEN;
/// cladefs: how long a VM process's stage operations go uncounted at most while it makes them
/// ([`STAGE_COUNTS`]): within a sample of the daemon's stats file (`CLADEFSD_STATS`).
pub const STAGE_COUNTS_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

pub const OPEN_CREATE: u32 = 1 << 0;
pub const OPEN_FORK: u32 = 1 << 1;
// cladefs: 1 << 2, a follower's `OPEN_READ_ONLY`, is not reused (board 8237).
/// cladefs: with [`OPEN_CREATE`], the cell made is a SQLite cell, which the
/// sqlite_cladefs VFS alone opens, not an Fxfs one (TASK-364).
pub const OPEN_SQLITE: u32 = 1 << 3;
/// cladefs: with [`OPEN_CREATE`], the cell made is a memory cell, a VM's memory
/// (TASK-456.2), not an Fxfs one. With [`OPEN_EPHEMERAL`] alone, opens an existing ephemeral
/// memory cell in place.
pub const OPEN_MEMORY: u32 = 1 << 4;
/// cladefs: host-local ephemeral cell open (doc-25): with [`OPEN_CREATE`] (and optionally
/// [`OPEN_SQLITE`] or [`OPEN_MEMORY`]), creates a standalone ephemeral cell with zero bucket
/// uploads; alone, reopens an existing ephemeral cell in place at `cell_version` (`committed`) for
/// local VM restore (`EEXIST` on mismatch); with [`OPEN_MEMORY`], reopens an existing ephemeral
/// memory cell in place.
pub const OPEN_EPHEMERAL: u32 = 1 << 5;
/// cladefs: [`OpenedBody::flags`] bit indicating the opened cell is host-local ephemeral.
pub const OPENED_EPHEMERAL: u32 = 1 << 0;
/// cladefs: [`ForkedBody::flags`] bit indicating a host-local quiesced ephemeral snapshot
/// (`committed` holds the latest commit number and `version` is 0).
pub const FORKED_EPHEMERAL: u32 = 1 << 0;
pub const FORK_QUIESCENT: u32 = 1 << 0;
/// cladefs: a [`FORK`] that drops a version the connection made (TASK-575).
pub const FORK_DROP: u32 = 1 << 1;
/// cladefs: a [`MAPPINGS`] that fails with `EBUSY` if any extent is open (`!is_quiescent`), used
/// when taking a live-migration snapshot or releasing a paused VM's cell without `make_version`.
pub const MAPPINGS_QUIESCENT: u32 = 1 << 0;
/// cladefs: a [`MAP`] that warms each mapped extent range in the daemon's extent cache before
/// installing the mappings, so lazy memory-cell node maps prefault without a throwaway `READ` IPC
/// copy (board 11681, board 11697).
pub const MAP_WARM: u32 = 1 << 0;
pub const MAPPING_ZEROS: u64 = 1 << 1;
/// cladefs: a `COMMIT` that completes once uploaded, not once ordered: doc-4's `COMMIT_UPLOADED`,
/// at its bit, as every flag doc-4 has too is here.
pub const COMMIT_UPLOADED: u32 = cladefs_core::protocol::COMMIT_UPLOADED;
/// A `COMMIT` that sets the root: doc-4's `COMMIT_ROOT`, at its bit.
pub const COMMIT_ROOT: u32 = cladefs_core::protocol::COMMIT_ROOT;
/// cladefs: a `COMMIT` that completes once local, synced to the host's disk, not waiting for the
/// bucket (TASK-301): not with [`COMMIT_UPLOADED`]. Not doc-4's.
pub const COMMIT_LOCAL: u32 = 1 << 2;
pub const WRITE_BARRIER: u32 = cladefs_core::protocol::WRITE_BARRIER;
pub const WRITE_COMMIT: u32 = cladefs_core::protocol::WRITE_COMMIT;
/// A `WRITE`'s commit completes once uploaded: doc-4's `WRITE_UPLOADED`, at its bit, as are
/// [`WRITE_BARRIER`], [`WRITE_COMMIT`] and [`WRITE_ROOT`].
pub const WRITE_UPLOADED: u32 = cladefs_core::protocol::WRITE_UPLOADED;
/// cladefs: a `WRITE`'s commit sets the root.
pub const WRITE_ROOT: u32 = cladefs_core::protocol::WRITE_ROOT;
/// Not doc-4's: a `WRITE`'s data is in the transfer area.
pub const WRITE_TRANSFER: u32 = 1 << 4;
/// cladefs: a `WRITE`'s commit completes once local, synced to the host's disk (TASK-301): not
/// with [`WRITE_UPLOADED`]. Not doc-4's.
pub const WRITE_LOCAL: u32 = 1 << 5;
pub const READ_TRANSFER: u32 = 1 << 0;
// `FAULT`'s bit 0 was `FAULT_WRITE` (doc-26) and bit 1 was always-true `FAULT_PAGE` (board 11697),
// neither reused.
/// cladefs: a [`FAULT`] on a page, to read, that is the VM process's own probe of the page,
/// mapping it ahead of the guest's accesses, not the guest's access: the daemon tells of no
/// refusal of it, as the guest may map nothing there.
pub const FAULT_PROBE: u32 = 1 << 2;
/// cladefs: the bytes an extent's ID takes in a [`FREE`]'s payload: its cell and its
/// sequence, little-endian words.
pub const EXTENT_ID_BYTES: usize = 16;

// From the daemon.

/// The reply to request `id`.
pub const REPLY: u32 = 100;
/// Maps `args[2]` bytes of physical memory at `args[1]` in arena `args[4]`'s object
/// into the window at `args[0]`, read-only.
pub const STAGE_MAP: u32 = 101;
// 102 changed a range's protection, as writable DAX's seals did, not reused.
/// Unmaps `args[1]` bytes of the window at `args[0]`: cladefs: ending the private pages there,
/// unless [`STAGE_KEEP_PRIVATE`] (doc-26).
pub const STAGE_UNMAP: u32 = 103;
/// cladefs: the window maps `args[1]` bytes at `args[0]` anew (`SecondStage::window_mapped`).
pub const STAGE_MAPPED: u32 = 106;
/// cladefs: atomically unmaps `RangeBody { offset, len }` (keeping private pages if
/// [`STAGE_KEEP_PRIVATE`]) and applies the [`StageAnewEntry`] array carried in its payload
/// (`SecondStage::remap`, board 11673).
pub const STAGE_REMAP: u32 = 110;
// 108 made a page private, the daemon's copy on write, which a VM process makes itself now (doc-26),
// not reused.
/// The cell's capacity is `args[0]` bytes now, which the guest takes (the control API's
/// `ResizeCell`); or, with [`CAPACITY_ASK`], the guest is asked to shrink to `args[0]` bytes, and
/// answers ([`RESIZED`]).
pub const CAPACITY: u32 = 104;
pub const CAPACITY_ASK: u32 = 1 << 0;
// cladefs: 105, a follower's `COMMITTED`, is not reused (board 8237).
/// cladefs: asks the VM process for its stats and its guest's, as of now: ask `args[0]`, which its
/// [`STATS`] answers, those asked before it among them (TASK-111.4).
pub const STATS_ASK: u32 = 109;
// 107 asked the guest to seal, writable DAX's (TASK-153.15), not reused.
/// A stage operation the daemon waits for the VM process to acknowledge ([`ACK`]), as it must for
/// one that takes access to a page away, before it frees the page or shares it.
pub const STAGE_ACK: u32 = 1 << 0;
/// cladefs: a [`STAGE_UNMAP`] of the host's own, an eviction's or a free's, which leaves the
/// private pages there (doc-26, spec rule evictionKeepsPrivate): without it, the guest's `MAP` or
/// `UNMAP`, which ends them.
pub const STAGE_KEEP_PRIVATE: u32 = 1 << 1;
/// cladefs: a [`WINDOW`] whose VM process copies on write, a writer's (doc-26).
pub const WINDOW_COPYING: u32 = 1 << 0;

/// The most bytes of payload a message carries: a `READ` or `WRITE` of a whole extent, or less.
pub const MAX_PAYLOAD: u64 = cladefs_core::OpenCell::MAX_EXTENT;

/// cladefs: the most requests a VM process keeps sent and not answered, its replies not yet
/// received (TASK-79.16): many times what its device's queues and its window's faults have at once.
/// A VM process past it the daemon lets go.
pub const MAX_REQUESTS: usize = 4096;

/// cladefs: the most bytes of data a VM process's requests not answered move through the stream
/// ([`stream_bytes`](super::codec::stream_bytes), TASK-79.16): `WRITE`s' payloads and `READ`s'
/// replies, but those through the transfer area ([`TRANSFER`]); a few times its largest. A VM
/// process past it, as the daemon finds what it holds of it, and what it has queued for it,
/// unread, the daemon lets go.
pub const MAX_STREAM_BYTES: u64 = 4 * MAX_PAYLOAD;

/// The most file descriptors a message carries.
pub const MAX_FDS: usize = 2;

/// cladefs: how long a VM process may leave a message half moved, no byte of it moving, before the
/// daemon ends its connection (board 8459): a payload it admitted none of which comes
/// ([`AsyncReceiver::receive`]), or what is queued for it that its socket takes none of
/// ([`AsyncSender::stalled`]). Either holds its tenant's bytes in flight, which an idle connection
/// held for good, every other VM process of the tenant's requests waiting for them. As long as a VM
/// process may leave a stage operation unacknowledged (`server::UNRESPONSIVE`): a VM process moves
/// a message whole, at once, as it sends and reads on threads of its own (hotbed question 481).
pub const STALL: std::time::Duration = std::time::Duration::from_secs(5);

/// A message received.
pub struct Message {
    pub header: Header,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// The sending half of a VM process's connection, which any of its threads sends whole messages
/// through, each waiting for room as the socket fills: the daemon's is an [`AsyncSender`].
pub struct Sender {
    inner: Mutex<SenderInner>,
    /// cladefs: its stream's descriptor, which [`Sender::shutdown`] shuts down without the lock a
    /// send waiting for room in the socket holds: the stream's, open as long as this is.
    raw: RawFd,
}

struct SenderInner {
    stream: UnixStream,
    /// Bounded scratch buffer for streaming volatile write payloads without gathering multi-MiB
    /// `Vec`s in the VMM (TASK-661).
    scratch: Vec<u8>,
}

/// The most bytes [`Sender::send_stream`] copies at once from guest memory into the socket.
const STREAM_CHUNK: usize = 64 << 10;

impl Sender {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        no_sigpipe(&stream)?;
        let raw = stream.as_raw_fd();
        Ok(Self { inner: Mutex::new(SenderInner { stream, scratch: Vec::new() }), raw })
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
        buffers: &[cladefs_core::VolatileSlice<'_>],
        suffix: &[&[u8]],
        fds: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        assert!(fds.len() <= MAX_FDS, "too many descriptors for a message");
        let prefix_len: u64 = prefix.iter().map(|part| part.len() as u64).sum();
        let stream_len: u64 = buffers.iter().map(|buf| buf.len() as u64).sum();
        let suffix_len: u64 = suffix.iter().map(|part| part.len() as u64).sum();
        header.len = prefix_len + stream_len + suffix_len;
        header.fds = fds.len() as u32;
        let mut inner = self.inner.lock().unwrap();
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
                let copied = cladefs_core::memory::gather(buffers, at, &mut scratch[..take]);
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

    /// Shuts the connection down, which ends its receiver's messages; cladefs: and fails a send
    /// waiting for room in the socket, whose lock it doesn't wait for.
    pub fn shutdown(&self) {
        // SAFETY: A plain system call on the stream's descriptor, open as long as `self` is.
        // Shut down already, or never connected (`ENOTCONN`): nothing more to end.
        let _ = unsafe { libc::shutdown(self.raw, libc::SHUT_RDWR) };
    }
}

/// cladefs: the sending half of one of the daemon's connections, which any thread sends whole
/// messages through, in order, none waiting for the VM process to take them (TASK-79.9): the
/// kernel sends its stage operations with its locks held, and a VM process that stops reading
/// would hold them for good. What the socket has no room for waits in a queue, which a task on the
/// runtime it was made on writes as the socket takes it; what is sent meanwhile queues behind.
pub struct AsyncSender {
    stream: Socket,
    queue: Mutex<Queue>,
    /// How many bytes the queue holds, which the socket has not taken (TASK-79.16).
    queued: std::sync::atomic::AtomicU64,
    /// cladefs: where those bytes are counted too, if anywhere: the room its tenant's connections
    /// share (TASK-149.35).
    counted: Option<std::sync::Arc<crate::in_flight::InFlight>>,
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
        counted: std::sync::Arc<crate::in_flight::InFlight>,
    ) -> io::Result<Self> {
        Self::made(stream, Some(counted))
    }

    fn made(
        stream: Stream,
        counted: Option<std::sync::Arc<crate::in_flight::InFlight>>,
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

    /// cladefs: sends `messages` as [`AsyncSender::send`] would one after another, but at once, in
    /// as few writes as the socket takes: the operations of one change to a window (TASK-91.16,
    /// board 11673). `messages` is called with the sender's queue locked, so that what it makes,
    /// such as the IDs it gives, is in the order it is sent in.
    pub fn send_all(
        self: &std::sync::Arc<Self>,
        messages: impl FnOnce() -> Vec<(Header, Vec<u8>)>,
    ) -> io::Result<()> {
        let mut queue = self.queue.lock().unwrap();
        for (header, payload) in messages() {
            self.push(&mut queue, header, payload, Vec::new())?;
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
#[cfg(all(target_os = "linux", not(madsim)))]
pub fn hung_up(fd: BorrowedFd<'_>) -> bool {
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
    // SAFETY: Polls a descriptor the caller keeps open, without waiting.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    ready > 0 && poll.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

/// Whether the other end of the stream `fd` hung up, as on Linux: macOS has no `POLLRDHUP`, but
/// its kqueue says the end of what the other end sends (`EV_EOF` of a read filter), whatever
/// this end has not read yet.
#[cfg(all(target_os = "macos", not(madsim)))]
pub fn hung_up(fd: BorrowedFd<'_>) -> bool {
    // SAFETY: Makes a kqueue, owned below.
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        log::warn!(
            "cladefsd: asking whether a connection hung up: kqueue: {}",
            io::Error::last_os_error()
        );
        return false;
    }
    // SAFETY: Just made, and owned here.
    let queue = unsafe { OwnedFd::from_raw_fd(queue) };
    // SAFETY: A zeroed kevent is a valid empty one.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = fd.as_raw_fd() as libc::uintptr_t;
    change.filter = libc::EVFILT_READ;
    change.flags = libc::EV_ADD;
    // SAFETY: As above.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: Registers `fd`, which the caller keeps open, and reads at most one event, without
    // waiting.
    let ready = unsafe { libc::kevent(queue.as_raw_fd(), &change, 1, &mut event, 1, &now) };
    if ready < 0 {
        log::warn!(
            "cladefsd: asking whether a connection hung up: kevent: {}",
            io::Error::last_os_error()
        );
        return false;
    }
    ready > 0 && event.flags & (libc::EV_EOF | libc::EV_ERROR) != 0
}

/// cladefs: what every `sendmsg` of a connection's asks: that a connection whose other end went
/// fail it (`EPIPE`) rather than signal the process (`SIGPIPE`), which ends one that doesn't
/// ignore it; on macOS, which has no such flag, the socket says so once ([`no_sigpipe`]).
#[cfg(target_os = "linux")]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const SEND_FLAGS: libc::c_int = 0;

/// cladefs: what every `recvmsg` of a connection's asks: its descriptors close-on-exec as they
/// come, on Linux; on macOS each once it came ([`receive_with_fds`]).
#[cfg(target_os = "linux")]
const RECEIVE_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const RECEIVE_FLAGS: libc::c_int = 0;

/// cladefs: what a connection's socket is given once, as its sending half is made: on macOS,
/// that its writes to a connection whose other end went fail (`EPIPE`) rather than signal the
/// process (`SIGPIPE`, `SO_NOSIGPIPE`), which on Linux each write's flag asks ([`SEND_FLAGS`]);
/// and buffers as large as Linux gives a Unix stream by default (`net.core.wmem_default`), where
/// macOS gives it 8 KiB each way (`net.local.stream.sendspace`), in which a run of a window's
/// stage operations, or a burst of requests, waits for every few messages (board 13065).
fn no_sigpipe(stream: &impl AsFd) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let set = |option: libc::c_int, value: libc::c_int| {
            // SAFETY: Sets an option of a socket the caller keeps open, from an int on the stack.
            let set = unsafe {
                libc::setsockopt(
                    stream.as_fd().as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const value).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if set != 0 {
                let err = io::Error::last_os_error();
                // macOS refuses a socket's options once its other end went (`EINVAL`).
                #[cfg(not(madsim))]
                if err.raw_os_error() == Some(libc::EINVAL) && hung_up(stream.as_fd()) {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "its other end went",
                    ));
                }
                return Err(err);
            }
            Ok(())
        };
        set(libc::SO_NOSIGPIPE, 1)?;
        set(libc::SO_SNDBUF, SOCKET_BUFFER)?;
        set(libc::SO_RCVBUF, SOCKET_BUFFER)?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = stream;
    Ok(())
}

/// cladefs: the bytes of each of a connection's socket's buffers on macOS ([`no_sigpipe`]).
#[cfg(target_os = "macos")]
const SOCKET_BUFFER: libc::c_int = 256 << 10;

/// cladefs: has the descriptor `fd`, just received, close on exec (macOS: [`receive_with_fds`]).
#[cfg(not(target_os = "linux"))]
fn close_on_exec(fd: RawFd) -> io::Result<()> {
    // SAFETY: Sets a flag of a descriptor this process owns.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Waits until `stream` takes more bytes.
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
fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
    send_parts(stream, &[bytes], fds)
}

/// Sends what of `parts`, in order, one `sendmsg` does, with `fds`: how many bytes it sent, at
/// least one.
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
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, SEND_FLAGS) };
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
/// that say they carry them ([`Header::fds`]).
pub struct Receiver {
    stream: UnixStream,
    /// What it read and has not received yet: `buffer[start..end]`.
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    /// The descriptors that came with what it read, for the messages not received yet.
    fds: std::collections::VecDeque<OwnedFd>,
}

/// cladefs: the bytes a [`Receiver`] reads at most at once.
const RECEIVE_BYTES: usize = 64 << 10;

impl Receiver {
    pub fn new(stream: UnixStream) -> Self {
        let buffer = vec![0; RECEIVE_BYTES].into_boxed_slice();
        Self { stream, buffer, start: 0, end: 0, fds: Default::default() }
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
pub struct AsyncReceiver(Socket);

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
/// cladefs: each close-on-exec: as it comes on Linux (`MSG_CMSG_CLOEXEC`); on macOS, which has no
/// such flag, once it came, a moment in which a thread of this process's forking and executing
/// another program would leak it to that program.
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
            unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, RECEIVE_FLAGS | flags) };
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
                    #[cfg(not(target_os = "linux"))]
                    close_on_exec(raw)?;
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((received, fds))
}

/// A connected pair of a sender and a receiver of each end, for tests.
#[cfg(test)]
pub fn pair() -> ((Sender, Receiver), (Sender, Receiver)) {
    let (a, b) = UnixStream::pair().unwrap();
    let ends = |stream: UnixStream| {
        (Sender::new(stream.try_clone().unwrap()).unwrap(), Receiver::new(stream))
    };
    (ends(a), ends(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Stage, StageOp};
    use cladefs_core::dax::Anew;
    use std::os::fd::AsFd;

    fn unmap(offset: u64, len: u64) -> StageOp {
        StageOp::Unmap { offset, len, keep_private: false }
    }

    fn map(offset: u64, len: u64) -> StageOp {
        let phys = cladefs_core::pages::PhysLocation {
            arena: cladefs_core::pages::ArenaId(0),
            offset: 1 << 20,
        };
        StageOp::Map { offset, phys, len }
    }

    fn mapped(offset: u64, len: u64) -> StageOp {
        StageOp::Mapped { offset, len }
    }

    fn anew(op: StageOp) -> Anew {
        match op {
            StageOp::Map { offset, phys, len } => Anew::Map { offset, phys, len },
            StageOp::Mapped { offset, len } => Anew::Mapped { offset, len },
            op => panic!("{op:?} is taken as no map"),
        }
    }

    #[test]
    fn unmaps_join_adjacent_ranges_into_atomic_stage_remap() {
        let coalesced = crate::codec::coalesce_stage_ops(vec![
            unmap(8192, 4096),
            unmap(4096, 4096),
            unmap(4096, 8192),
            unmap(12288, 4096),
            mapped(4096, 12288),
        ]);
        assert_eq!(
            coalesced,
            vec![StageOp::Remap {
                offset: 4096,
                len: 12288,
                keep_private: false,
                anew: vec![Anew::Mapped { offset: 4096, len: 12288 }],
            }]
        );
        let ((sender, _), (_, mut receiver)) = pair();
        let stage = Stage { id: 7, op: coalesced.into_iter().next().unwrap() };
        let (header, payload) = stage.encode();
        sender.send(header, &payload, &[]).unwrap();
        let reply = Header::new(REPLY, 100);
        let file = tempfile::tempfile().unwrap();
        sender.send(reply, &[], &[file.as_fd()]).unwrap();
        let msg = receiver.receive(|| 0).unwrap().unwrap();
        assert_eq!(Stage::decode(&msg.header, &msg.payload).unwrap(), stage);
        let message = receiver.receive(|| 0).unwrap().unwrap();
        assert_eq!(message.header, Header { fds: 1, ..reply }, "the header says its descriptor");
        assert_eq!(message.fds.len(), 1);
    }

    /// cladefs: an unmap coalesces the window's mapping anew of what it unmaps, and the maps of
    /// its pages, each within it, in order, up to the first that isn't (TASK-91.16, board 11673).
    #[test]
    fn unmaps_take_the_maps_within_them() {
        for (after, taken) in [(map(8192, 8192), 3), (unmap(4096, 4096), 3), (map(4096, 8192), 4)] {
            let sent = [mapped(4096, 8192), map(4096, 4096), map(8192, 4096), after.clone()];
            let mut ops = vec![unmap(4096, 8192)];
            ops.extend(sent.iter().cloned());
            let coalesced = crate::codec::coalesce_stage_ops(ops);
            let made: Vec<_> = sent[..taken].iter().cloned().map(anew).collect();
            let expected_first =
                StageOp::Remap { offset: 4096, len: 8192, keep_private: false, anew: made };
            assert_eq!(coalesced[0], expected_first);
            if taken < sent.len() {
                assert_eq!(coalesced.len(), 2);
                assert_eq!(coalesced[1], after);
            } else {
                assert_eq!(coalesced.len(), 1);
            }
        }
    }

    #[test]
    fn unmap_batches_leave_gaps_and_different_keep_private_separate() {
        // Gap between unmaps stays separate.
        assert_eq!(
            crate::codec::coalesce_stage_ops(vec![unmap(4096, 4096), unmap(12288, 4096)]),
            vec![unmap(4096, 4096), unmap(12288, 4096)]
        );
        // Different `keep_private` stays separate (doc-26).
        let keep = StageOp::Unmap { offset: 8192, len: 4096, keep_private: true };
        assert_eq!(
            crate::codec::coalesce_stage_ops(vec![unmap(4096, 4096), keep.clone()]),
            vec![unmap(4096, 4096), keep]
        );
    }

    /// board 11673: a remap batch with >64 `STAGE_MAP` entries arrives as one atomic `STAGE_REMAP`
    /// frame over the wire (even when written in partial chunks), never split across messages.
    #[test]
    fn stage_remap_batch_over_64_entries_is_atomic_over_wire() {
        let count = 128u64;
        let mut ops = vec![unmap(0, count * 4096), mapped(0, count * 4096)];
        for i in 0..count {
            ops.push(map(i * 4096, 4096));
        }
        let coalesced = crate::codec::coalesce_stage_ops(ops);
        assert_eq!(coalesced.len(), 1, "entire >64-entry remap must coalesce into one STAGE_REMAP");
        let stage = Stage { id: 1, op: coalesced.into_iter().next().unwrap() };
        let (header, payload) = stage.encode();
        assert_eq!(header.kind, STAGE_REMAP);
        let ((sender, _), (_, mut receiver)) = pair();
        // Write header + payload in small chunks across the socket to prove partial writes never
        // split the remap into separate unmap and map deliveries.
        let mut wire_bytes = header.as_bytes().to_vec();
        wire_bytes.extend_from_slice(&payload);
        let writer = std::thread::spawn(move || {
            for chunk in wire_bytes.chunks(17) {
                write_all(&sender.inner.lock().unwrap().stream, chunk).unwrap();
            }
        });
        let received = receiver.receive(|| 0).unwrap().unwrap();
        writer.join().unwrap();
        assert_eq!(Stage::decode(&received.header, &received.payload).unwrap(), stage);
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
        let batch = || {
            vec![(Header::new(STAGE_UNMAP, 2), Vec::new()), (Header::new(STAGE_MAP, 3), Vec::new())]
        };
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
                        let batch =
                            || (0..3).map(|_| (Header::new(STAGE_MAP, id()), Vec::new())).collect();
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
