// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the messages of the wire ([`super::wire`]) as typed `#[repr(C)]` `zerocopy` structs,
//! each encoded and decoded here alone (TASK-160.9, board 6917): the requests a VM process makes
//! ([`Request`]), what it says of the stage operations it made ([`Made`]), what the daemon tells
//! it unasked ([`Told`]), its stage operations among that ([`Stage`]), and what replies say
//! besides their status ([`Opened`], [`Info`], [`Forked`], [`mappings`]).
//!
//! An [`OPEN`] and its reply carry their sender's wire [`VERSION`] at [`VERSION_OFFSET`] inside
//! [`Header::body`] ([`version`], TASK-141.3): one of another version is refused
//! (`EPROTONOSUPPORT`) before anything else of it is decoded.
//!
//! Decoding refuses (`EINVAL`) what a message's kind gives no meaning: a flag it doesn't know, or
//! flags it knows together that it doesn't, non-zero trailing or reserved body bytes, an argument
//! out of its range, a payload or descriptors it has no use for, a non-zero status on a request,
//! and a path that is empty or holds a NUL. Shared limits, doc-4 flag bits, and checked range /
//! extent-ID / root / stats validators come from [`crate::protocol`], enforced independently at
//! this trust boundary (AGENTS.md, board 5702).

use crate::wire::*;
use bucket_cell::cell::Kind;
use cladefs_core::pages::{ArenaId, PhysLocation};
use cladefs_core::protocol::{
    check_extent_id, check_mapping, check_range_ends, check_root_len, check_stats_text,
    check_window_range, decode_commit_flags, decode_write_barriers,
};
use cladefs_core::{
    Commit, Errno, ExtentId, ExtentInfo, MappingContents, WindowMapping, WriteBarriers,
};
use std::borrow::Cow;
use std::num::NonZeroU64;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use zerocopy::{FromBytes, IntoBytes};

/// A request, of a VM process to the daemon, which a reply ([`REPLY`]) answers.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum Request<'a> {
    /// [`OPEN`]: the cell at `path`, opened as `opening` says, by a VM process built of `build`
    /// (board 11697). Its paths are owned: the daemon opens a cell to write holding no thread,
    /// beyond the message.
    Open { path: PathBuf, opening: Opening, build: &'a str },
    /// [`READ`]: `len` bytes at `offset` in `extent`, into its reply's payload, [`MAX_PAYLOAD`]
    /// at most, or the connection's transfer area.
    Read { extent: ExtentId, offset: u64, len: u64, into: ReadInto },
    /// [`WRITE`]: `data` at `offset` in `extent`, ordered with `barriers`.
    Write { extent: ExtentId, offset: u64, data: Data<'a>, barriers: WriteBarriers<'a> },
    /// [`FREE`]: `first`, then `rest`, in order (board 11697).
    Free { first: ExtentId, rest: Cow<'a, [ExtentId]> },
    /// [`MAP`]: the mappings into the window, in order, one at least (board 11697). With `warm`
    /// ([`MAP_WARM`]), warms each mapped extent range in the daemon's extent cache before
    /// installing the mappings (board 11681).
    Map { mappings: Cow<'a, [WindowMapping]>, warm: bool },
    /// [`UNMAP`].
    Unmap { window_offset: u64, len: u64 },
    /// [`COMMIT`], completing as `commit` says, setting the root to `root` if given.
    Commit { commit: Commit, root: Option<&'a [u8]> },
    /// [`ROOT`].
    Root,
    /// [`INFO`].
    Info,
    /// [`WINDOW`]: of `size` bytes, [`MAX_WINDOW`] at most; cladefs: whose VM process copies on
    /// write if `copying` ([`WINDOW_COPYING`]). Its VM process
    /// keeps `budget` runs of pages installed at most, 0 if it doesn't say (TASK-111.5).
    Window { size: u64, copying: bool, budget: u64 },
    /// [`FAULT`].
    Fault(Fault),
    /// [`FORK`] with [`FORK_QUIESCENT`]: a version of the cell, a VM snapshot's.
    Version,
    /// cladefs: [`FORK`] with [`FORK_DROP`]: drops the version `version`, one the connection made.
    DropVersion { version: u64 },
    /// [`RESIZED`]: the guest's answer to a shrink to `capacity` bytes.
    Resized { capacity: u64, took: bool },
    /// [`MAPPINGS`]: if `quiescent` ([`MAPPINGS_QUIESCENT`]), refuses with `EBUSY` while any
    /// extent is open.
    Mappings { quiescent: bool },
    /// [`TRANSFER`]: the area, of `len` bytes.
    Transfer { len: u64, area: Area<'a> },
    /// cladefs: [`STATS`]: the answer to ask `ask`, with its guest's stats if it has a guest that
    /// answers, [`MAX_STATS`] bytes at most.
    Stats { ask: u64, guest: Option<&'a str> },
}

/// How an [`OPEN`] opens its cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Opening {
    /// To write, if it exists (no flags).
    Existing,
    /// To write, created with this many bytes of capacity if it doesn't exist ([`OPEN_CREATE`]),
    /// a new cell of this kind ([`OPEN_SQLITE`], TASK-364).
    Create(u64, Kind),
    /// To write, created as a standalone host-local ephemeral cell with this many bytes of
    /// capacity if it doesn't exist ([`OPEN_CREATE`] | [`OPEN_EPHEMERAL`]), or reopened if it
    /// already exists as an ephemeral cell of the same kind.
    CreateEphemeral(u64, Kind),
    /// To write, forked from the version `version` of the cell `cell` of `tenant`'s if it doesn't
    /// exist, of `capacity` bytes ([`OPEN_FORK`]): a VM restored from a snapshot.
    ForkOf { tenant: u64, cell: u64, version: u64, capacity: u64 },
    /// To write, reopening an existing host-local ephemeral cell in place at its quiesced commit
    /// `committed` ([`OPEN_EPHEMERAL`]): `ENOENT` if missing, `EINVAL` if not ephemeral, `EEXIST`
    /// if the cell ID or latest commit number differs.
    LocalEphemeral { tenant: u64, cell: u64, committed: u64, capacity: u64 },
    /// To write, reopening an existing host-local ephemeral memory cell in place
    /// ([`OPEN_EPHEMERAL`] | [`OPEN_MEMORY`]): `ENOENT` if missing, `EINVAL` if not an ephemeral
    /// memory cell, `EEXIST` if `cell != 0` and the cell ID differs.
    ExistingEphemeralMemory { tenant: u64, cell: u64, capacity: u64 },
}

/// Where a [`READ`]'s bytes go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadInto {
    /// The reply's payload.
    Reply,
    /// The connection's transfer area, at `at` ([`READ_TRANSFER`]).
    Transfer { at: u64 },
}

/// The bytes a [`WRITE`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Data<'a> {
    /// In its payload.
    Inline(&'a [u8]),
    /// `len` bytes of the connection's transfer area at `at` ([`WRITE_TRANSFER`]).
    Transfer { at: u64, len: u64 },
}

/// A guest's fault on the window ([`FAULT`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    /// Where in the window.
    pub offset: u64,
    /// cladefs: the VM process's probe of the page, to read, not the guest's access
    /// ([`FAULT_PROBE`]).
    pub probe: bool,
}

/// The shared memory a [`TRANSFER`] carries, its descriptor.
#[derive(Debug)]
pub struct Area<'a>(pub BorrowedFd<'a>);

/// cladefs: areas are one if their descriptors, numbers of their processes', not of the wire's,
/// name one file.
#[cfg(test)]
impl PartialEq for Area<'_> {
    fn eq(&self, other: &Self) -> bool {
        use std::os::fd::AsRawFd;
        let file = |fd: &BorrowedFd<'_>| {
            // SAFETY: A zeroed stat is a valid one to fill in.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: Fills `stat` in, of a descriptor the area borrows.
            assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) }, 0);
            (stat.st_dev, stat.st_ino)
        };
        file(&self.0) == file(&other.0)
    }
}

/// A message encoded: its header, but for a request's ID, which its sender gives it, and the
/// lengths, which the sending sets; its payload, the parts sent one after another, as a write's
/// data and the root after it, which no copy joins; and the descriptor it carries, if any.
pub struct Encoded<'a> {
    pub header: Header,
    pub payload: [Cow<'a, [u8]>; 2],
    pub fd: Option<BorrowedFd<'a>>,
}

impl<'a> Encoded<'a> {
    fn empty(kind: u32, flags: u32) -> Self {
        let header = Header::new(kind, 0).with_flags(flags);
        Self { header, payload: Default::default(), fd: None }
    }

    fn with_body<T: IntoBytes + zerocopy::Immutable>(kind: u32, flags: u32, body: &T) -> Self {
        let header = Header::new(kind, 0).with_flags(flags).with_body(body);
        Self { header, payload: Default::default(), fd: None }
    }

    /// With `part` as its payload.
    fn carrying(mut self, part: impl Into<Cow<'a, [u8]>>) -> Self {
        self.payload[0] = part.into();
        self
    }

    /// With `part` after what it carries.
    fn followed_by(mut self, part: impl Into<Cow<'a, [u8]>>) -> Self {
        self.payload[1] = part.into();
        self
    }
}

/// The flags requests of `kind` know: `EINVAL` for a kind no request is of.
fn known_flags(kind: u32) -> Result<u32, Errno> {
    Ok(match kind {
        OPEN => OPEN_CREATE | OPEN_FORK | OPEN_SQLITE | OPEN_MEMORY | OPEN_EPHEMERAL,
        READ => READ_TRANSFER,
        WRITE => cladefs_core::protocol::WRITE_SHARED_FLAGS | WRITE_TRANSFER | WRITE_LOCAL,
        COMMIT => cladefs_core::protocol::COMMIT_SHARED_FLAGS | COMMIT_LOCAL,
        FAULT => FAULT_PROBE,
        WINDOW => WINDOW_COPYING,
        FORK => FORK_QUIESCENT | FORK_DROP,
        MAPPINGS => MAPPINGS_QUIESCENT,
        MAP => MAP_WARM,
        STATS => STATS_GUEST,
        FREE | UNMAP | ROOT | INFO | RESIZED | TRANSFER => 0,
        _ => return Err(Errno::EINVAL),
    })
}

/// Encodes `barriers` into `WRITE` flags (excluding `WRITE_TRANSFER`).
fn write_barrier_flags(barriers: WriteBarriers<'_>) -> u32 {
    let mut flags = if barriers.before { WRITE_BARRIER } else { 0 };
    flags |= match barriers.after {
        None => 0,
        Some(Commit::Ordered) => WRITE_COMMIT,
        Some(Commit::Local) => WRITE_COMMIT | WRITE_LOCAL,
        Some(Commit::Uploaded) => WRITE_COMMIT | WRITE_UPLOADED,
    };
    if barriers.root.is_some() {
        flags |= WRITE_ROOT;
    }
    flags
}

impl<'a> Request<'a> {
    /// Its kind.
    pub fn kind(&self) -> u32 {
        match self {
            Request::Open { .. } => OPEN,
            Request::Read { .. } => READ,
            Request::Write { .. } => WRITE,
            Request::Free { .. } => FREE,
            Request::Map { .. } => MAP,
            Request::Unmap { .. } => UNMAP,
            Request::Commit { .. } => COMMIT,
            Request::Root => ROOT,
            Request::Info => INFO,
            Request::Window { .. } => WINDOW,
            Request::Fault(_) => FAULT,
            Request::Version | Request::DropVersion { .. } => FORK,
            Request::Resized { .. } => RESIZED,
            Request::Mappings { .. } => MAPPINGS,
            Request::Transfer { .. } => TRANSFER,
            Request::Stats { .. } => STATS,
        }
    }

    /// The paths it names, of cells: which the daemon authorizes, all of them, before it serves
    /// it.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        match self {
            Request::Open { path, .. } => Some(path.as_path()),
            _ => None,
        }
        .into_iter()
    }

    /// Header and optional root slice for an inline [`WRITE`] of `data_len` bytes, shared between
    /// [`Request::encode`] and streaming writes (TASK-661).
    pub fn write_inline_header<'r>(
        extent: ExtentId,
        offset: u64,
        data_len: u64,
        barriers: WriteBarriers<'r>,
    ) -> (Header, &'r [u8]) {
        let flags = write_barrier_flags(barriers);
        let body = WriteBody {
            cell: extent.cell,
            sequence: extent.sequence,
            offset,
            transfer_at: 0,
            data_len,
        };
        let header = Header::new(WRITE, 0).with_flags(flags).with_body(&body);
        (header, barriers.root.unwrap_or_default())
    }

    /// It, as the wire has it.
    pub fn encode(&self) -> Encoded<'_> {
        let flag = |set: bool, bit: u32| if set { bit } else { 0 };
        match self {
            Request::Open { path, opening, build } => {
                let path = path.as_os_str().as_bytes();
                let mut payload = Vec::with_capacity(path.len() + 1 + build.len());
                payload.extend_from_slice(path);
                payload.push(0);
                payload.extend_from_slice(build.as_bytes());
                let (flags, body) = match opening {
                    Opening::Existing => (0, OpenBody { version: VERSION, ..Default::default() }),
                    Opening::Create(capacity, kind) => {
                        let flags = OPEN_CREATE
                            | flag(*kind == Kind::Sqlite, OPEN_SQLITE)
                            | flag(*kind == Kind::Memory, OPEN_MEMORY);
                        (
                            flags,
                            OpenBody {
                                capacity: *capacity,
                                version: VERSION,
                                ..Default::default()
                            },
                        )
                    }
                    Opening::CreateEphemeral(capacity, kind) => {
                        let flags = OPEN_CREATE
                            | OPEN_EPHEMERAL
                            | flag(*kind == Kind::Sqlite, OPEN_SQLITE)
                            | flag(*kind == Kind::Memory, OPEN_MEMORY);
                        (
                            flags,
                            OpenBody {
                                capacity: *capacity,
                                version: VERSION,
                                ..Default::default()
                            },
                        )
                    }
                    Opening::ForkOf { tenant, cell, version, capacity } => (
                        OPEN_FORK,
                        OpenBody {
                            capacity: *capacity,
                            cell: *cell,
                            cell_version: *version,
                            tenant: *tenant,
                            version: VERSION,
                        },
                    ),
                    Opening::LocalEphemeral { tenant, cell, committed, capacity } => (
                        OPEN_EPHEMERAL,
                        OpenBody {
                            capacity: *capacity,
                            cell: *cell,
                            cell_version: *committed,
                            tenant: *tenant,
                            version: VERSION,
                        },
                    ),
                    Opening::ExistingEphemeralMemory { tenant, cell, capacity } => (
                        OPEN_EPHEMERAL | OPEN_MEMORY,
                        OpenBody {
                            capacity: *capacity,
                            cell: *cell,
                            cell_version: 0,
                            tenant: *tenant,
                            version: VERSION,
                        },
                    ),
                };
                Encoded::with_body(OPEN, flags, &body).carrying(payload)
            }
            Request::Read { extent, offset, len, into } => {
                let (flags, transfer_at) = match *into {
                    ReadInto::Reply => (0, 0),
                    ReadInto::Transfer { at } => (READ_TRANSFER, at),
                };
                let body = ReadBody {
                    cell: extent.cell,
                    sequence: extent.sequence,
                    offset: *offset,
                    len: *len,
                    transfer_at,
                };
                Encoded::with_body(READ, flags, &body)
            }
            Request::Write { extent, offset, data, barriers } => match *data {
                Data::Inline(data) => {
                    let (header, root) =
                        Self::write_inline_header(*extent, *offset, data.len() as u64, *barriers);
                    Encoded { header, payload: Default::default(), fd: None }
                        .carrying(data)
                        .followed_by(root)
                }
                Data::Transfer { at, len } => {
                    let flags = write_barrier_flags(*barriers) | WRITE_TRANSFER;
                    let body = WriteBody {
                        cell: extent.cell,
                        sequence: extent.sequence,
                        offset: *offset,
                        transfer_at: at,
                        data_len: len,
                    };
                    Encoded::with_body(WRITE, flags, &body)
                        .carrying(barriers.root.unwrap_or_default())
                }
            },
            Request::Free { first, rest } => {
                let body = ExtentBody { cell: first.cell, sequence: first.sequence };
                let encoded = Encoded::with_body(FREE, 0, &body);
                if rest.is_empty() { encoded } else { encoded.carrying(extent_ids_payload(rest)) }
            }
            Request::Map { mappings, warm } => {
                Encoded::empty(MAP, flag(*warm, MAP_WARM)).carrying(mappings_payload(mappings))
            }
            Request::Unmap { window_offset, len } => {
                let body = RangeBody { offset: *window_offset, len: *len };
                Encoded::with_body(UNMAP, 0, &body)
            }
            Request::Commit { commit, root } => {
                let flags = flag(root.is_some(), COMMIT_ROOT)
                    | flag(*commit == Commit::Local, COMMIT_LOCAL)
                    | flag(*commit == Commit::Uploaded, COMMIT_UPLOADED);
                Encoded::empty(COMMIT, flags).carrying(root.unwrap_or_default())
            }
            Request::Root | Request::Info => Encoded::empty(self.kind(), 0),
            Request::Mappings { quiescent } => {
                Encoded::empty(MAPPINGS, flag(*quiescent, MAPPINGS_QUIESCENT))
            }
            Request::Window { size, copying, budget } => {
                let body = WindowBody { size: *size, budget: *budget };
                Encoded::with_body(WINDOW, flag(*copying, WINDOW_COPYING), &body)
            }
            Request::Fault(Fault { offset, probe }) => {
                Encoded::with_body(FAULT, flag(*probe, FAULT_PROBE), &FaultBody { offset: *offset })
            }
            Request::Version => Encoded::empty(FORK, FORK_QUIESCENT),
            Request::DropVersion { version } => {
                Encoded::with_body(FORK, FORK_DROP, &DropVersionBody { version: *version })
            }
            Request::Resized { capacity, took } => {
                let body = ResizedBody { capacity: *capacity, took: u64::from(*took) };
                Encoded::with_body(RESIZED, 0, &body)
            }
            Request::Transfer { len, area } => Encoded {
                fd: Some(area.0),
                ..Encoded::with_body(TRANSFER, 0, &TransferBody { len: *len })
            },
            Request::Stats { ask, guest } => Encoded::with_body(
                STATS,
                flag(guest.is_some(), STATS_GUEST),
                &StatsBody { ask: *ask },
            )
            .carrying(guest.unwrap_or_default().as_bytes()),
        }
    }

    /// The request `message` is: `EINVAL` unless it is one its kind allows; cladefs:
    /// `EPROTONOSUPPORT` for an [`OPEN`] of another wire version, whatever else it holds
    /// (TASK-141.3).
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(message: &'a Message) -> Result<Self, Errno> {
        let Message { header, fds, .. } = message;
        let Header { kind, flags, status, .. } = *header;
        if kind == OPEN && version(header) != VERSION {
            return Err(Errno::EPROTONOSUPPORT);
        }
        if status != 0 || flags & !(known_flags(kind)? | REQUEST) != 0 {
            return Err(Errno::EINVAL);
        }
        // cladefs: what its kind has, after the guest request it says, if any (TASK-260).
        let (_, payload) = guest_request(message)?;
        let flags = flags & !REQUEST;
        // A request's descriptors: a TRANSFER's one, and none of any other.
        let fd = match (kind, fds.as_slice()) {
            (TRANSFER, [fd]) => Some(fd.as_fd()),
            (_, []) if kind != TRANSFER => None,
            _ => return Err(Errno::EINVAL),
        };
        let has = |flag: u32| flags & flag != 0;
        let nothing = || if payload.is_empty() { Ok(()) } else { Err(Errno::EINVAL) };
        // A body's words its flags give no meaning are 0: decoded into what it means, a request
        // holds nothing it ignores.
        let unused = |words: &[u64]| match words.iter().all(|&word| word == 0) {
            true => Ok(()),
            false => Err(Errno::EINVAL),
        };
        Ok(match kind {
            OPEN => {
                let body: OpenBody = header.decode_body()?;
                let OpenBody { capacity, cell, cell_version, tenant, .. } = body;
                let opening = match flags {
                    0 => {
                        unused(&[capacity, cell, cell_version, tenant])?;
                        Opening::Existing
                    }
                    OPEN_CREATE => {
                        unused(&[cell, cell_version, tenant])?;
                        Opening::Create(capacity, Kind::Fxfs)
                    }
                    flags if flags == OPEN_CREATE | OPEN_SQLITE => {
                        unused(&[cell, cell_version, tenant])?;
                        Opening::Create(capacity, Kind::Sqlite)
                    }
                    flags if flags == OPEN_CREATE | OPEN_MEMORY => {
                        unused(&[cell, cell_version, tenant])?;
                        Opening::Create(capacity, Kind::Memory)
                    }
                    flags if flags == OPEN_CREATE | OPEN_EPHEMERAL => {
                        unused(&[cell, cell_version, tenant])?;
                        Opening::CreateEphemeral(capacity, Kind::Fxfs)
                    }
                    flags if flags == OPEN_CREATE | OPEN_EPHEMERAL | OPEN_SQLITE => {
                        unused(&[cell, cell_version, tenant])?;
                        Opening::CreateEphemeral(capacity, Kind::Sqlite)
                    }
                    flags if flags == OPEN_CREATE | OPEN_EPHEMERAL | OPEN_MEMORY => {
                        unused(&[cell, cell_version, tenant])?;
                        Opening::CreateEphemeral(capacity, Kind::Memory)
                    }
                    OPEN_FORK => Opening::ForkOf { tenant, cell, version: cell_version, capacity },
                    OPEN_EPHEMERAL => {
                        Opening::LocalEphemeral { tenant, cell, committed: cell_version, capacity }
                    }
                    flags if flags == OPEN_EPHEMERAL | OPEN_MEMORY => {
                        unused(&[cell_version])?;
                        if cell == 0 && tenant != 0 {
                            return Err(Errno::EINVAL);
                        }
                        Opening::ExistingEphemeralMemory { tenant, cell, capacity }
                    }
                    // Two ways of opening at once.
                    _ => return Err(Errno::EINVAL),
                };
                let nul = payload.iter().position(|&b| b == 0).ok_or(Errno::EINVAL)?;
                let (path_bytes, rest) = payload.split_at_checked(nul).ok_or(Errno::EINVAL)?;
                let (_, build_bytes) = rest.split_first().ok_or(Errno::EINVAL)?;
                Request::Open { path: path(path_bytes)?, opening, build: build(build_bytes)? }
            }
            READ => {
                nothing()?;
                let body: ReadBody = header.decode_body()?;
                let into = match has(READ_TRANSFER) {
                    true => ReadInto::Transfer { at: body.transfer_at },
                    // cladefs: a reply holds no more (TASK-79.16).
                    false if body.len > MAX_PAYLOAD => return Err(Errno::EINVAL),
                    false => {
                        unused(&[body.transfer_at])?;
                        ReadInto::Reply
                    }
                };
                check_range_ends(body.offset, body.len)?;
                let extent = check_extent_id(body.cell, body.sequence)?;
                Request::Read { extent, offset: body.offset, len: body.len, into }
            }
            WRITE => {
                let body: WriteBody = header.decode_body()?;
                let (mut barriers, has_root) =
                    decode_write_barriers(flags & !WRITE_TRANSFER, Some(WRITE_LOCAL))?;
                // The data, and the root after it in the payload, if any.
                let (data, root) = match has(WRITE_TRANSFER) {
                    true => (Data::Transfer { at: body.transfer_at, len: body.data_len }, payload),
                    false => {
                        unused(&[body.transfer_at])?;
                        // An inline write's data is data_len bytes, its root what follows.
                        let len = usize::try_from(body.data_len).map_err(|_| Errno::EINVAL)?;
                        let (data, root) = payload.split_at_checked(len).ok_or(Errno::EINVAL)?;
                        (Data::Inline(data), root)
                    }
                };
                if !has_root && !root.is_empty() {
                    return Err(Errno::EINVAL);
                }
                check_root_len(root.len())?;
                barriers.root = has_root.then_some(root);
                let len = match data {
                    Data::Inline(data) => data.len() as u64,
                    Data::Transfer { len, .. } => len,
                };
                check_range_ends(body.offset, len)?;
                let extent = check_extent_id(body.cell, body.sequence)?;
                Request::Write { extent, offset: body.offset, data, barriers }
            }
            FREE => {
                let body: ExtentBody = header.decode_body()?;
                let first = check_extent_id(body.cell, body.sequence)?;
                let rest = Cow::Owned(extent_ids(payload)?);
                Request::Free { first, rest }
            }
            // cladefs: one mapping at least (TASK-92.14, board 11697).
            MAP => {
                header.decode_empty()?;
                match mappings(payload)? {
                    mappings if mappings.is_empty() => return Err(Errno::EINVAL),
                    mappings => {
                        Request::Map { mappings: Cow::Owned(mappings), warm: has(MAP_WARM) }
                    }
                }
            }
            UNMAP => {
                nothing()?;
                let body: RangeBody = header.decode_body()?;
                check_window_range(body.offset, body.len)?;
                Request::Unmap { window_offset: body.offset, len: body.len }
            }
            COMMIT => {
                header.decode_empty()?;
                let (commit, has_root) = decode_commit_flags(flags, Some(COMMIT_LOCAL))?;
                let root = if has_root {
                    // cladefs: no more than a root holds (TASK-149.17).
                    check_root_len(payload.len())?;
                    Some(payload)
                } else {
                    nothing()?;
                    None
                };
                Request::Commit { commit, root }
            }
            ROOT | INFO => {
                nothing()?;
                header.decode_empty()?;
                match kind {
                    ROOT => Request::Root,
                    _ => Request::Info,
                }
            }
            MAPPINGS => {
                nothing()?;
                header.decode_empty()?;
                Request::Mappings { quiescent: has(MAPPINGS_QUIESCENT) }
            }
            WINDOW => {
                nothing()?;
                let body: WindowBody = header.decode_body()?;
                // cladefs: past it, the daemon's room for its VM process's replies (TASK-150.3).
                if body.size > MAX_WINDOW {
                    return Err(Errno::EINVAL);
                }
                Request::Window {
                    size: body.size,
                    copying: has(WINDOW_COPYING),
                    budget: body.budget,
                }
            }
            FAULT => {
                nothing()?;
                let body: FaultBody = header.decode_body()?;
                Request::Fault(Fault { offset: body.offset, probe: has(FAULT_PROBE) })
            }
            FORK if has(FORK_QUIESCENT) && has(FORK_DROP) => return Err(Errno::EINVAL),
            FORK if has(FORK_QUIESCENT) => {
                nothing()?;
                header.decode_empty()?;
                Request::Version
            }
            FORK if has(FORK_DROP) => {
                nothing()?;
                let body: DropVersionBody = header.decode_body()?;
                Request::DropVersion { version: body.version }
            }
            FORK => return Err(Errno::EINVAL),
            RESIZED => {
                nothing()?;
                let body: ResizedBody = header.decode_body()?;
                let took = match body.took {
                    0 => false,
                    1 => true,
                    _ => return Err(Errno::EINVAL),
                };
                Request::Resized { capacity: body.capacity, took }
            }
            TRANSFER => {
                nothing()?;
                let body: TransferBody = header.decode_body()?;
                Request::Transfer { len: body.len, area: Area(fd.ok_or(Errno::EINVAL)?) }
            }
            STATS => {
                let body: StatsBody = header.decode_body()?;
                let guest = match has(STATS_GUEST) {
                    true => Some(check_stats_text(payload)?),
                    false => {
                        nothing()?;
                        None
                    }
                };
                Request::Stats { ask: body.ask, guest }
            }
            _ => return Err(Errno::EINVAL),
        })
    }
}

/// cladefs: the bytes of data the request `header` is of moves through the stream: its payload,
/// and, a `READ`'s, its reply's: [`MAX_PAYLOAD`] at most, as one asking for more is refused
/// (`EINVAL`), its reply's payload empty, whatever the VM process asks for: so a word that would
/// overflow the bytes counted is counted as no more (TASK-79.16). Of the header alone, as the
/// daemon admits a request before it reads its payload, and decodes it. cladefs: and what the
/// daemon makes of a payload besides, as long as it holds the payload, counted too (TASK-614): a
/// `FREE_MANY`'s extents, decoded, as many bytes again; a `MAP_MANY`'s mappings, and the window's
/// pieces mapping them makes, [`MAP_MANY_HELD`] times its payload in all.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn stream_bytes(header: &Header) -> u64 {
    match header.kind {
        READ if header.flags & READ_TRANSFER == 0 => {
            header.len.saturating_add(header.read_body::<ReadBody>().len.min(MAX_PAYLOAD))
        }
        FREE => header.len.saturating_mul(2),
        MAP => header.len.saturating_mul(MAP_MANY_HELD),
        // cladefs: its reply's root, as a READ's reply, counted as it is admitted: uncounted, queued
        // replies of roots passed every tenant's bytes in flight (the Codex review of board 8465).
        ROOT => header.len.saturating_add(cladefs_core::protocol::MAX_ROOT as u64),
        _ => header.len,
    }
}

/// cladefs: the most bytes a path a request names has, as [`path`] takes it: shorter than
/// Linux's `PATH_MAX`, on every host, so that the wire is one on Linux and macOS (whose
/// `PATH_MAX` is 1024: a longer path decodes there, then fails to open, `ENAMETOOLONG`; board
/// 13065).
const MOST_PATH: u64 = 4096 - 1;

/// cladefs: the most bytes of payload a message of `header` carries, as its kind and flags decode
/// one, [`REQUEST_BYTES`] more if it says its guest request (board 8459): of the header alone, as
/// the daemon admits a message before it reads its payload, which one past it, refused, the daemon
/// never reads, nor holds any of. None for a kind no request is of, and for an [`ACK`] (TASK-581);
/// [`PrivateCountsBody`] bytes for [`STAGE_COUNTS`] (board 11697). One limit per kind, as
/// [`known_flags`] has one mask per kind; [`Request::decode`] refuses what it holds within them
/// that means nothing.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn most_payload(header: &Header) -> u64 {
    const OPEN_MAX: u64 = MOST_PATH + 1 + MAX_BUILD as u64;
    const ROOT: u64 = cladefs_core::protocol::MAX_ROOT as u64;
    let has = |flag: u32| header.flags & flag != 0;
    let most = match header.kind {
        OPEN => OPEN_MAX,
        // Its data and its root after it; or, through the transfer area, its root alone.
        WRITE if has(WRITE_TRANSFER) => ROOT,
        WRITE => MAX_PAYLOAD.saturating_add(ROOT),
        COMMIT if has(COMMIT_ROOT) => ROOT,
        FREE | MAP => MAX_PAYLOAD,
        STAGE_COUNTS => size_of::<PrivateCountsBody>() as u64,
        STATS if has(STATS_GUEST) => cladefs_core::protocol::MAX_STATS as u64,
        _ => 0,
    };
    match has(REQUEST) && known_flags(header.kind).is_ok() {
        true => most.saturating_add(REQUEST_BYTES as u64),
        false => most,
    }
}

/// cladefs: what the daemon holds of a `MAP_MANY`, in times its payload: the payload; a
/// [`WindowMapping`] for each of its mappings, decoded; and the window's pieces mapping each makes,
/// two at most (TASK-614).
pub const MAP_MANY_HELD: u64 = 4;

/// cladefs: the guest request the request `message` is a call for, if it says one ([`REQUEST`]),
/// and the rest of its payload, its kind's: `EINVAL` if it says one it doesn't hold, or 0
/// (TASK-260).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn guest_request(message: &Message) -> Result<(Option<NonZeroU64>, &[u8]), Errno> {
    if message.header.flags & REQUEST == 0 {
        return Ok((None, &message.payload));
    }
    let (guest, rest) =
        message.payload.split_first_chunk::<REQUEST_BYTES>().ok_or(Errno::EINVAL)?;
    let guest = NonZeroU64::new(u64::from_le_bytes(*guest)).ok_or(Errno::EINVAL)?;
    Ok((Some(guest), rest))
}

/// cladefs: the bytes a [`REQUEST`] says the guest request `guest` in (TASK-260): a VM process's
/// client's, Linux's alone.
pub fn request_bytes(guest: NonZeroU64) -> [u8; REQUEST_BYTES] {
    guest.get().to_le_bytes()
}

/// The path `bytes` hold: `EINVAL` if empty, or holding a NUL, which no path does.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn path(bytes: &[u8]) -> Result<PathBuf, Errno> {
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(Errno::EINVAL);
    }
    // cladefs: no longer than a path is, before it is copied: a payload of up to MAX_PAYLOAD
    // bytes would be held twice, its copy uncounted (TASK-614).
    if bytes.len() as u64 > MOST_PATH {
        return Err(Errno::ENAMETOOLONG);
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// cladefs: the commit an [`OPEN`]'s payload says: `EINVAL` unless a label's characters, as many as
/// [`MAX_BUILD`] at most (TASK-111.7).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn build(payload: &[u8]) -> Result<&str, Errno> {
    let commit = std::str::from_utf8(payload).map_err(|_| Errno::EINVAL)?;
    let label = |byte: u8| byte.is_ascii_alphanumeric() || b"._-".contains(&byte);
    if commit.is_empty() || commit.len() > MAX_BUILD || !commit.bytes().all(label) {
        return Err(Errno::EINVAL);
    }
    Ok(commit)
}

/// cladefs: `extents`, as a [`FREE`]'s payload names them.
fn extent_ids_payload(extents: &[ExtentId]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(extents.len() * EXTENT_ID_BYTES);
    for extent in extents {
        let wire = ExtentIdWire { cell: extent.cell.into(), sequence: extent.sequence.into() };
        bytes.extend_from_slice(wire.as_bytes());
    }
    bytes
}

/// cladefs: the extents a [`FREE`]'s payload names: `EINVAL` unless it is whole
/// [`ExtentIdWire`]s, each passing [`check_extent_id`].
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn extent_ids(payload: &[u8]) -> Result<Vec<ExtentId>, Errno> {
    let ids = <[ExtentIdWire]>::ref_from_bytes(payload).map_err(|_| Errno::EINVAL)?;
    ids.iter().map(|id| check_extent_id(id.cell.get(), id.sequence.get())).collect()
}

/// What a VM process says of the stage operations it made, which nothing replies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Made {
    /// [`ACK`]: it made stage operation `id`, and those before it; and its second stage is as
    /// `counts` says, as [`STAGE_COUNTS`] says it.
    Ack { id: u64, counts: StageCounts },
    /// cladefs: [`STAGE_COUNTS`] (TASK-91.16, TASK-164.10, board 11697): its stage operations and
    /// its runs of private pages and `PRIVATE` requests carried in its 40-byte payload.
    Counts { stage: StageCounts, private: PrivateCosts },
}

/// cladefs: what a VM process says of its window's second stage, as [`STAGE_COUNTS`] says it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageCounts {
    /// What its stage operations cost it so far, counts that only grow: how many, their
    /// nanoseconds, and those that took access away from pages it had installed, by how: installed
    /// over, left out (TASK-91.16).
    pub costs: [u64; 4],
    /// The runs of pages it has installed now, each a mapping of its own (TASK-111.5).
    pub installed: u64,
    /// cladefs: its window's private pages now (backlog doc-13), which the VM process alone knows
    /// of (doc-26).
    pub private: u64,
    /// cladefs: the runs of pages its window keeps installed at most now, its share of its
    /// process's windows', as a [`STAGE_COUNTS`] says it in its `id` (board 10098); 0 if it
    /// doesn't say, as an [`ACK`]'s doesn't, its `id` the stage operation's.
    pub budget: u64,
}

impl StageCounts {
    fn to_body(self) -> StageCountsBody {
        let [ops, ns, overwrites, leave_outs] = self.costs;
        StageCountsBody {
            ops,
            ns,
            overwrites,
            leave_outs,
            installed: u32::try_from(self.installed).unwrap_or(u32::MAX),
            private: u32::try_from(self.private).unwrap_or(u32::MAX),
        }
    }

    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn from_body(body: StageCountsBody) -> Self {
        Self {
            costs: [body.ops, body.ns, body.overwrites, body.leave_outs],
            installed: u64::from(body.installed),
            private: u64::from(body.private),
            budget: 0,
        }
    }
}

/// cladefs: what a VM process's runs of private pages cost it so far, as [`STAGE_COUNTS`]'s
/// payload says it (board 11697): how many, their pages, their nanoseconds; and its `PRIVATE`
/// requests, and the nanoseconds it waited for their replies.
pub type PrivateCosts = [u64; 5];

impl Made {
    pub fn encode(&self) -> (Header, Vec<u8>) {
        match *self {
            Made::Ack { id, counts } => {
                (Header::new(ACK, id).with_body(&counts.to_body()), Vec::new())
            }
            Made::Counts { stage, private: [runs, pages, ns, requests, request_ns] } => {
                let body = PrivateCountsBody { runs, pages, ns, requests, request_ns };
                let payload = body.as_bytes().to_vec();
                let mut header =
                    Header::new(STAGE_COUNTS, stage.budget).with_body(&stage.to_body());
                header.len = payload.len() as u64;
                (header, payload)
            }
        }
    }

    /// What `message` says: `EINVAL` unless it is an [`ACK`] (no payload) or a [`STAGE_COUNTS`]
    /// (carrying [`PrivateCountsBody`] as its payload), of no flags or descriptors.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(message: &Message) -> Result<Self, Errno> {
        let Header { kind, flags, id, status, .. } = message.header;
        if flags != 0 || status != 0 || !message.fds.is_empty() {
            return Err(Errno::EINVAL);
        }
        match kind {
            ACK => {
                if !message.payload.is_empty() {
                    return Err(Errno::EINVAL);
                }
                Ok(Made::Ack { id, counts: StageCounts::from_body(message.header.decode_body()?) })
            }
            STAGE_COUNTS => {
                let stage = StageCounts::from_body(message.header.decode_body()?);
                let PrivateCountsBody { runs, pages, ns, requests, request_ns } =
                    PrivateCountsBody::read_from_bytes(&message.payload)
                        .map_err(|_| Errno::EINVAL)?;
                Ok(Made::Counts {
                    stage: StageCounts { budget: id, ..stage },
                    private: [runs, pages, ns, requests, request_ns],
                })
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

/// What the daemon tells a VM process unasked: carries a payload only for [`STAGE_REMAP`] and no
/// descriptors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Told {
    /// A stage operation, which the VM process makes of its window.
    Stage(Stage),
    /// [`CAPACITY`]: the cell's capacity is `capacity` bytes now; or, if `ask`, the guest is asked
    /// to shrink to it ([`CAPACITY_ASK`]).
    Capacity { capacity: u64, ask: bool },
    /// cladefs: [`STATS_ASK`].
    Stats { ask: u64 },
}

impl Told {
    pub fn encode(&self) -> (Header, Vec<u8>) {
        match self {
            Told::Stage(stage) => stage.encode(),
            Told::Capacity { capacity, ask } => (
                Header::new(CAPACITY, 0)
                    .with_flags(if *ask { CAPACITY_ASK } else { 0 })
                    .with_body(&CapacityBody { capacity: *capacity }),
                Vec::new(),
            ),
            Told::Stats { ask } => {
                (Header::new(STATS_ASK, 0).with_body(&StatsBody { ask: *ask }), Vec::new())
            }
        }
    }

    /// What the message of `header` and `payload` says: `EINVAL` unless one of these, as its kind
    /// allows.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(header: &Header, payload: &[u8]) -> Result<Self, Errno> {
        let Header { kind, flags, status, fds, len, .. } = *header;
        if status != 0 || fds != 0 || len != payload.len() as u64 {
            return Err(Errno::EINVAL);
        }
        let known = |known: u32| if flags & !known == 0 { Ok(()) } else { Err(Errno::EINVAL) };
        match kind {
            CAPACITY => {
                if !payload.is_empty() {
                    return Err(Errno::EINVAL);
                }
                known(CAPACITY_ASK)?;
                let body: CapacityBody = header.decode_body()?;
                Ok(Told::Capacity { capacity: body.capacity, ask: flags & CAPACITY_ASK != 0 })
            }
            STATS_ASK => {
                if !payload.is_empty() {
                    return Err(Errno::EINVAL);
                }
                known(0)?;
                let body: StatsBody = header.decode_body()?;
                Ok(Told::Stats { ask: body.ask })
            }
            _ => Stage::decode(header, payload).map(Told::Stage),
        }
    }
}

/// A stage operation, of the daemon's window's second stage in a VM process's memory, which the
/// VM process makes, and acknowledges ([`ACK`]) if it takes access to pages away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    /// Its ID, which an acknowledgement names.
    pub id: u64,
    pub op: StageOp,
}

/// What a [`Stage`] operation does, to the window's bytes at `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageOp {
    /// [`STAGE_MAP`]: maps `len` bytes of physical memory at `phys`, read-only.
    Map { offset: u64, phys: PhysLocation, len: u64 },
    /// [`STAGE_UNMAP`]: cladefs: the private pages there kept if `keep_private`
    /// ([`STAGE_KEEP_PRIVATE`], doc-26), as the host's own changes keep them; else ended, as the
    /// guest's own do.
    Unmap { offset: u64, len: u64, keep_private: bool },
    /// cladefs: [`STAGE_MAPPED`].
    Mapped { offset: u64, len: u64 },
    /// cladefs: [`STAGE_REMAP`]: atomically unmaps `offset..offset + len` and applies `anew`
    /// (`SecondStage::remap`, board 11673).
    Remap { offset: u64, len: u64, keep_private: bool, anew: Vec<cladefs_core::dax::Anew> },
}

impl StageOp {
    /// Whether it takes access to pages away, which its VM process acknowledges ([`STAGE_ACK`]).
    pub fn taking(&self) -> bool {
        match self {
            StageOp::Unmap { .. } | StageOp::Remap { .. } => true,
            StageOp::Map { .. } | StageOp::Mapped { .. } => false,
        }
    }
}

/// Coalesces a window change's sequence of [`StageOp`]s on the sender side before writing them to
/// the wire (TASK-91.16, board 11673): joins adjacent/overlapping `Unmap`s with matching
/// `keep_private` and folds all immediately following `Mapped` and `Map` entries within the joined
/// range into a single atomic [`StageOp::Remap`], with no 64-message split limit.
pub fn coalesce_stage_ops(ops: Vec<StageOp>) -> Vec<StageOp> {
    use cladefs_core::dax::Anew;
    let mut out = Vec::with_capacity(ops.len());
    let mut iter = ops.into_iter().peekable();
    while let Some(op) = iter.next() {
        let StageOp::Unmap { offset, len, keep_private } = op else {
            out.push(op);
            continue;
        };
        let (mut start, Some(mut end)) = (offset, offset.checked_add(len)) else {
            out.push(StageOp::Unmap { offset, len, keep_private });
            continue;
        };
        if len == 0 {
            out.push(StageOp::Unmap { offset, len, keep_private });
            continue;
        }
        let mut anew = Vec::new();
        while let Some(next) = iter.peek() {
            match next {
                StageOp::Unmap { offset, len, keep_private: keep }
                    if anew.is_empty() && *len != 0 && *keep == keep_private =>
                {
                    let Some(next_end) = offset.checked_add(*len) else { break };
                    if *offset > end || next_end < start {
                        break;
                    }
                    start = start.min(*offset);
                    end = end.max(next_end);
                    iter.next();
                }
                StageOp::Mapped { offset, len }
                    if *len != 0
                        && *offset >= start
                        && offset.checked_add(*len).is_some_and(|e| e <= end) =>
                {
                    let (offset, len) = (*offset, *len);
                    iter.next();
                    anew.push(Anew::Mapped { offset, len });
                }
                StageOp::Map { offset, phys, len }
                    if *len != 0
                        && *offset >= start
                        && offset.checked_add(*len).is_some_and(|e| e <= end) =>
                {
                    let (offset, phys, len) = (*offset, *phys, *len);
                    iter.next();
                    anew.push(Anew::Map { offset, phys, len });
                }
                _ => break,
            }
        }
        if anew.is_empty() {
            out.push(StageOp::Unmap { offset: start, len: end - start, keep_private });
        } else {
            out.push(StageOp::Remap { offset: start, len: end - start, keep_private, anew });
        }
    }
    out
}

impl Stage {
    pub fn encode(&self) -> (Header, Vec<u8>) {
        let mut payload = Vec::new();
        let mut header = match &self.op {
            StageOp::Map { offset, phys, len } => {
                let body = StageMapBody {
                    offset: *offset,
                    phys_offset: phys.offset,
                    len: *len,
                    arena: phys.arena.0,
                    reserved: 0,
                };
                Header::new(STAGE_MAP, self.id).with_body(&body)
            }
            StageOp::Unmap { offset, len, keep_private } => {
                let keep = if *keep_private { STAGE_KEEP_PRIVATE } else { 0 };
                Header::new(STAGE_UNMAP, self.id)
                    .with_flags(keep)
                    .with_body(&RangeBody { offset: *offset, len: *len })
            }
            StageOp::Mapped { offset, len } => Header::new(STAGE_MAPPED, self.id)
                .with_body(&RangeBody { offset: *offset, len: *len }),
            StageOp::Remap { offset, len, keep_private, anew } => {
                payload.reserve(anew.len() * size_of::<StageAnewEntry>());
                for entry in anew {
                    let wire = match *entry {
                        cladefs_core::dax::Anew::Mapped { offset, len } => StageAnewEntry {
                            kind: STAGE_MAPPED,
                            arena: 0,
                            offset,
                            phys_offset: 0,
                            len,
                        },
                        cladefs_core::dax::Anew::Map { offset, phys, len } => StageAnewEntry {
                            kind: STAGE_MAP,
                            arena: phys.arena.0,
                            offset,
                            phys_offset: phys.offset,
                            len,
                        },
                    };
                    payload.extend_from_slice(wire.as_bytes());
                }
                let keep = if *keep_private { STAGE_KEEP_PRIVATE } else { 0 };
                let mut h = Header::new(STAGE_REMAP, self.id)
                    .with_flags(keep)
                    .with_body(&RangeBody { offset: *offset, len: *len });
                h.len = payload.len() as u64;
                h
            }
        };
        if self.op.taking() {
            header.flags |= STAGE_ACK;
        }
        (header, payload)
    }

    /// The stage operation the message of `header` and `payload` is: `EINVAL` unless one,
    /// acknowledged as it takes access away or not, with no descriptors, and with a payload only
    /// for [`STAGE_REMAP`].
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(header: &Header, payload: &[u8]) -> Result<Self, Errno> {
        let Header { kind, flags, id, status, fds, len, .. } = *header;
        if status != 0 || fds != 0 || len != payload.len() as u64 {
            return Err(Errno::EINVAL);
        }
        let op = match kind {
            STAGE_MAP => {
                if !payload.is_empty() {
                    return Err(Errno::EINVAL);
                }
                let body: StageMapBody = header.decode_body()?;
                if body.reserved != 0 {
                    return Err(Errno::EINVAL);
                }
                let phys = PhysLocation { arena: ArenaId(body.arena), offset: body.phys_offset };
                StageOp::Map { offset: body.offset, phys, len: body.len }
            }
            STAGE_UNMAP => {
                if !payload.is_empty() {
                    return Err(Errno::EINVAL);
                }
                let body: RangeBody = header.decode_body()?;
                let keep_private = flags & STAGE_KEEP_PRIVATE != 0;
                StageOp::Unmap { offset: body.offset, len: body.len, keep_private }
            }
            STAGE_MAPPED => {
                if !payload.is_empty() {
                    return Err(Errno::EINVAL);
                }
                let body: RangeBody = header.decode_body()?;
                StageOp::Mapped { offset: body.offset, len: body.len }
            }
            STAGE_REMAP => {
                if payload.is_empty() {
                    return Err(Errno::EINVAL);
                }
                let body: RangeBody = header.decode_body()?;
                if body.len == 0 {
                    return Err(Errno::EINVAL);
                }
                let end = body.offset.checked_add(body.len).ok_or(Errno::EINVAL)?;
                let entries =
                    <[StageAnewEntry]>::ref_from_bytes(payload).map_err(|_| Errno::EINVAL)?;
                let mut anew = Vec::with_capacity(entries.len());
                for entry in entries {
                    if entry.len == 0 || entry.offset < body.offset {
                        return Err(Errno::EINVAL);
                    }
                    let entry_end = entry.offset.checked_add(entry.len).ok_or(Errno::EINVAL)?;
                    if entry_end > end {
                        return Err(Errno::EINVAL);
                    }
                    match entry.kind {
                        STAGE_MAPPED => {
                            if entry.arena != 0 || entry.phys_offset != 0 {
                                return Err(Errno::EINVAL);
                            }
                            anew.push(cladefs_core::dax::Anew::Mapped {
                                offset: entry.offset,
                                len: entry.len,
                            });
                        }
                        STAGE_MAP => {
                            let phys = PhysLocation {
                                arena: ArenaId(entry.arena),
                                offset: entry.phys_offset,
                            };
                            anew.push(cladefs_core::dax::Anew::Map {
                                offset: entry.offset,
                                phys,
                                len: entry.len,
                            });
                        }
                        _ => return Err(Errno::EINVAL),
                    }
                }
                let keep_private = flags & STAGE_KEEP_PRIVATE != 0;
                StageOp::Remap { offset: body.offset, len: body.len, keep_private, anew }
            }
            _ => return Err(Errno::EINVAL),
        };
        let acknowledged = if op.taking() { STAGE_ACK } else { 0 };
        let keep = match op {
            StageOp::Unmap { keep_private: true, .. }
            | StageOp::Remap { keep_private: true, .. } => STAGE_KEEP_PRIVATE,
            _ => 0,
        };
        if flags != acknowledged | keep {
            return Err(Errno::EINVAL);
        }
        Ok(Stage { id, op })
    }
}

/// cladefs: the wire version the [`OPEN`], or the reply to one, of `header` says: its sender's
/// (TASK-141.3), whatever the rest of it holds, as no version moves [`VERSION_OFFSET`].
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn version(header: &Header) -> u64 {
    let Some((_, tail)) = header.body.0.split_at_checked(VERSION_OFFSET) else {
        unreachable!("VERSION_OFFSET is within BODY_BYTES");
    };
    u64::read_from_prefix(tail).expect("8 bytes at VERSION_OFFSET").0
}

/// cladefs: `header`, an [`OPEN`] or a reply to one, saying this end's wire [`VERSION`]: as a
/// daemon's reply refusing an `OPEN` of another version does too (TASK-141.3).
pub fn with_version(mut header: Header) -> Header {
    header.body.0[VERSION_OFFSET..VERSION_OFFSET + size_of::<u64>()]
        .copy_from_slice(VERSION.as_bytes());
    header
}

/// What a reply to an [`OPEN`] says, with the physical memory it carries (TASK-43, TASK-79.17)
/// and the cell's capacity in its 8-byte reply payload (board 11697).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The cell.
    pub cell: u64,
    /// The cell's tenant.
    pub tenant: u64,
    /// The arena of the memory it carries first: the VM process's tenant's.
    pub arena: ArenaId,
    /// cladefs: how many mappings the host lets a process make, if it limits them
    /// (`pages::max_map_count`), which the VM process's window budgets its own by: the daemon
    /// reads it for the VM process, which can't in a jail without /proc (TASK-172). 0 if it
    /// doesn't limit them.
    pub max_map_count: Option<u64>,
    /// Whether the opened cell is a host-local ephemeral cell (doc-25).
    pub ephemeral: bool,
    /// The cell's capacity in bytes (board 11697).
    pub capacity: u64,
}

impl Opened {
    /// Its typed wire body, with this daemon's wire [`VERSION`] at [`VERSION_OFFSET`].
    pub fn to_body(self) -> OpenedBody {
        OpenedBody {
            cell: self.cell,
            tenant: self.tenant,
            max_map_count: self.max_map_count.unwrap_or(0),
            arena: self.arena.0,
            flags: if self.ephemeral { OPENED_EPHEMERAL } else { 0 },
            version: VERSION,
        }
    }

    /// Its 8-byte little-endian capacity reply payload (board 11697).
    pub fn capacity_bytes(self) -> [u8; 8] {
        self.capacity.to_le_bytes()
    }

    /// What an `OPEN` reply `header` and `payload` say: `EPROTONOSUPPORT` from a daemon of another
    /// wire version, before anything else of it (TASK-141.3); `EINVAL` on unknown flags or wrong
    /// payload length.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(header: &Header, payload: &[u8]) -> Result<Self, Errno> {
        if version(header) != VERSION {
            return Err(Errno::EPROTONOSUPPORT);
        }
        let body: OpenedBody = header.decode_body()?;
        if body.flags & !OPENED_EPHEMERAL != 0 {
            return Err(Errno::EINVAL);
        }
        let capacity: &[u8; 8] = payload.try_into().map_err(|_| Errno::EINVAL)?;
        let capacity = u64::from_le_bytes(*capacity);
        let max_map_count = (body.max_map_count != 0).then_some(body.max_map_count);
        Ok(Self {
            cell: body.cell,
            tenant: body.tenant,
            arena: ArenaId(body.arena),
            max_map_count,
            ephemeral: body.flags & OPENED_EPHEMERAL != 0,
            capacity,
        })
    }
}

/// What a reply to an [`INFO`] says: the cell's [`ExtentInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info(pub ExtentInfo);

impl Info {
    pub fn to_body(self) -> InfoBody {
        let ExtentInfo { capacity, used, available, next_sequence, sequence_floor } = self.0;
        InfoBody { capacity, used, available, next_sequence, sequence_floor }
    }

    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(header: &Header) -> Result<Self, Errno> {
        let InfoBody { capacity, used, available, next_sequence, sequence_floor } =
            header.decode_body()?;
        Ok(Self(ExtentInfo { capacity, used, available, next_sequence, sequence_floor }))
    }
}

/// cladefs: a quiescent [`FORK`]'s reply: either a bucket-backed version (`!ephemeral`,
/// `committed == 0`), or a quiesced host-local ephemeral cell checkpoint (`ephemeral`,
/// `version == 0`, `committed` set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forked {
    pub version: u64,
    pub tenant: u64,
    pub committed: u64,
    pub ephemeral: bool,
}

impl Forked {
    pub fn to_body(self) -> ForkedBody {
        ForkedBody {
            version: self.version,
            tenant: self.tenant,
            committed: self.committed,
            flags: if self.ephemeral { FORKED_EPHEMERAL } else { 0 },
            reserved: 0,
        }
    }

    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(header: &Header) -> Result<Self, Errno> {
        let ForkedBody { version, tenant, committed, flags, reserved } = header.decode_body()?;
        if reserved != 0 || flags & !FORKED_EPHEMERAL != 0 {
            return Err(Errno::EINVAL);
        }
        let ephemeral = flags & FORKED_EPHEMERAL != 0;
        if (ephemeral && version != 0) || (!ephemeral && committed != 0) {
            return Err(Errno::EINVAL);
        }
        Ok(Self { version, tenant, committed, ephemeral })
    }
}

/// `mappings`, as a reply to [`MAPPINGS`], or a [`MAP`], carries them: each
/// [`MAPPING_BYTES`].
pub fn mappings_payload(mappings: &[WindowMapping]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(mappings.len() * size_of::<MappingWire>());
    for mapping in mappings {
        let wire = match mapping.contents {
            MappingContents::Extent { extent, offset } => MappingWire {
                window_offset: mapping.window_offset.into(),
                len: mapping.len.into(),
                cell: extent.cell.into(),
                sequence: extent.sequence.into(),
                offset: offset.into(),
                flags: 0u64.into(),
            },
            MappingContents::Zeros => MappingWire {
                window_offset: mapping.window_offset.into(),
                len: mapping.len.into(),
                cell: 0u64.into(),
                sequence: 0u64.into(),
                offset: 0u64.into(),
                flags: MAPPING_ZEROS.into(),
            },
        };
        bytes.extend_from_slice(wire.as_bytes());
    }
    bytes
}

/// The mappings a reply to [`MAPPINGS`], or a [`MAP`], carries, `payload`: `EINVAL` unless
/// whole [`MappingWire`]s, of flags known, passing [`check_mapping`].
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn mappings(payload: &[u8]) -> Result<Vec<WindowMapping>, Errno> {
    let entries = <[MappingWire]>::ref_from_bytes(payload).map_err(|_| Errno::EINVAL)?;
    entries
        .iter()
        .map(|entry| {
            let contents = match entry.flags.get() {
                0 => MappingContents::Extent {
                    extent: check_extent_id(entry.cell.get(), entry.sequence.get())?,
                    offset: entry.offset.get(),
                },
                // cladefs: zeros name no extent.
                MAPPING_ZEROS
                    if [entry.cell, entry.sequence, entry.offset].iter().all(|w| w.get() == 0) =>
                {
                    MappingContents::Zeros
                }
                _ => return Err(Errno::EINVAL),
            };
            check_mapping(WindowMapping {
                window_offset: entry.window_offset.get(),
                len: entry.len.get(),
                contents,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::os::fd::OwnedFd;

    /// TASK-614, board 11697: a `FREE`'s and a `MAP`'s stream bytes count what the daemon decodes
    /// of their payloads besides them; every other request's, its payload alone, a `READ`'s reply
    /// apart.
    #[test]
    fn many_requests_count_what_they_decode() {
        let of = |kind| Header { len: 4800, ..Header::new(kind, 1) };
        assert_eq!(stream_bytes(&of(FREE)), 2 * 4800);
        assert_eq!(stream_bytes(&of(MAP)), MAP_MANY_HELD * 4800);
        assert_eq!(stream_bytes(&of(WRITE)), 4800);
    }

    /// TASK-614: what a `MAP_MANY`'s stream bytes count holds what the daemon makes of its
    /// payload: a decoded mapping, and two pieces of the window, for each mapping it carries.
    #[test]
    fn map_manys_count_what_they_hold() {
        let made = size_of::<WindowMapping>() + 2 * size_of::<cladefs_core::dax::DaxMapping>();
        assert!(made as u64 <= (MAP_MANY_HELD - 1) * MAPPING_BYTES, "{made} bytes a mapping");
    }

    /// What one of each shape of request is made of.
    #[derive(Clone, Debug)]
    struct Fields {
        words: [u64; 5],
        flags: [bool; 3],
        data: Vec<u8>,
        root: Vec<u8>,
        path: PathBuf,
        extents: Vec<ExtentId>,
        build: String,
        /// cladefs: a guest's stats (TASK-111.4).
        stats: String,
        commit: Commit,
        copying: bool,
    }

    /// Edge-heavy u64 strategy reaching 0, 1, protocol limits, their neighbours, and u64::MAX
    /// (AGENTS.md: trust boundaries).
    fn edge_u64() -> BoxedStrategy<u64> {
        prop_oneof![
            Just(0),
            Just(1),
            Just(MAX_ROOT as u64),
            Just(MAX_STATS as u64),
            Just(MAX_PAYLOAD),
            Just(MAX_PAYLOAD - 1),
            Just(MAX_WINDOW),
            Just(MAX_WINDOW - 1),
            Just(u64::MAX - 1),
            Just(u64::MAX),
            any::<u64>(),
        ]
        .boxed()
    }

    fn fields() -> impl Strategy<Value = Fields> {
        let extent =
            (edge_u64(), edge_u64()).prop_map(|(cell, sequence)| ExtentId { cell, sequence });
        let path = || "[a-z0-9/._-]{1,24}".prop_map(PathBuf::from);
        let bytes = || proptest::collection::vec(any::<u8>(), 0..64);
        (
            (
                [edge_u64(), edge_u64(), edge_u64(), edge_u64(), edge_u64()],
                any::<[bool; 3]>(),
                bytes(),
                bytes(),
            ),
            (path(), proptest::collection::vec(extent, 0..8)),
            ("[a-zA-Z0-9._-]{1,64}", "(\\PC{0,16}\n){0,8}"),
            prop_oneof![Just(Commit::Ordered), Just(Commit::Local), Just(Commit::Uploaded)],
            any::<bool>(),
        )
            .prop_map(
                |((words, flags, data, root), (path, extents), (build, stats), commit, copying)| {
                    Fields {
                        words,
                        flags,
                        data,
                        root,
                        path,
                        extents,
                        build,
                        stats,
                        commit,
                        copying,
                    }
                },
            )
    }

    /// A request of every shape there is, of `fields`, with `area` as a transfer area: each kind,
    /// each of its ways.
    fn requests<'a>(fields: &'a Fields, area: BorrowedFd<'a>) -> Vec<Request<'a>> {
        let Fields { words: [a, b, c, d, e], flags: [one, two, three], .. } = *fields;
        let (extent, other) = (known(a, b), known(c, d));
        let (window_offset, window_len) = windowed(d, e);
        let path = fields.path.clone();
        let commit = fields.commit;
        let root = (one && two).then_some(&fields.root[..]);
        let barriers = WriteBarriers { before: three, after: two.then_some(commit), root };
        let read = |len, into| Request::Read { extent, offset: ending(c, len), len, into };
        let write = |data: Data<'a>| {
            let len = match data {
                Data::Inline(data) => data.len() as u64,
                Data::Transfer { len, .. } => len,
            };
            Request::Write { extent, offset: ending(c, len), data, barriers }
        };
        let map = |contents| {
            let contents = match contents {
                MappingContents::Extent { extent, offset } => {
                    MappingContents::Extent { extent, offset: ending(offset, window_len) }
                }
                zeros => zeros,
            };
            Request::Map {
                mappings: Cow::Owned(vec![WindowMapping {
                    window_offset,
                    len: window_len,
                    contents,
                }]),
                warm: false,
            }
        };
        let rest: Vec<ExtentId> =
            fields.extents.iter().map(|id| known(id.cell, id.sequence)).collect();
        let (unmapped, unmapped_len) = windowed(a, b);
        let open = |opening| Request::Open { path: path.clone(), opening, build: &fields.build };
        vec![
            open(Opening::Existing),
            open(Opening::Create(a, Kind::Fxfs)),
            open(Opening::Create(a, Kind::Sqlite)),
            open(Opening::Create(a, Kind::Memory)),
            open(Opening::CreateEphemeral(a, Kind::Fxfs)),
            open(Opening::CreateEphemeral(a, Kind::Sqlite)),
            open(Opening::CreateEphemeral(a, Kind::Memory)),
            open(Opening::ForkOf { tenant: c, cell: b, version: d, capacity: a }),
            open(Opening::LocalEphemeral { tenant: c, cell: b, committed: d, capacity: a }),
            open(Opening::ExistingEphemeralMemory { tenant: 0, cell: 0, capacity: a }),
            open(Opening::ExistingEphemeralMemory { tenant: c, cell: b.max(1), capacity: a }),
            read(d % (MAX_PAYLOAD + 1), ReadInto::Reply),
            read(d, ReadInto::Transfer { at: e }),
            write(Data::Inline(&fields.data[..])),
            write(Data::Transfer { at: d, len: e }),
            Request::Free { first: extent, rest: Cow::Borrowed(&[]) },
            Request::Free { first: extent, rest: Cow::Owned(rest) },
            map(MappingContents::Extent { extent: other, offset: a }),
            map(MappingContents::Zeros),
            Request::Map {
                mappings: Cow::Owned(vec![
                    WindowMapping {
                        window_offset,
                        len: window_len,
                        contents: MappingContents::Extent {
                            extent: other,
                            offset: ending(a, window_len),
                        },
                    },
                    WindowMapping {
                        window_offset: unmapped,
                        len: unmapped_len,
                        contents: MappingContents::Zeros,
                    },
                ]),
                warm: one,
            },
            Request::Unmap { window_offset: unmapped, len: unmapped_len },
            Request::Commit { commit, root: one.then_some(&fields.data[..]) },
            Request::Root,
            Request::Info,
            Request::Window { size: a % (MAX_WINDOW + 1), copying: fields.copying, budget: c },
            Request::Window { size: MAX_WINDOW, copying: true, budget: d },
            Request::Fault(Fault { offset: a, probe: false }),
            Request::Fault(Fault { offset: a, probe: true }),
            Request::Version,
            Request::DropVersion { version: a },
            Request::Resized { capacity: a, took: one },
            Request::Mappings { quiescent: false },
            Request::Mappings { quiescent: true },
            Request::Transfer { len: a, area: Area(area) },
            Request::Stats { ask: a, guest: None },
            Request::Stats { ask: b, guest: Some(&fields.stats) },
        ]
    }

    fn known(cell: u64, sequence: u64) -> ExtentId {
        ExtentId { cell, sequence: sequence.min(u64::MAX - 1) }
    }

    fn ending(offset: u64, len: u64) -> u64 {
        offset.min(u64::MAX - len)
    }

    fn windowed(offset: u64, len: u64) -> (u64, u64) {
        let len = len % (MAX_WINDOW + 1);
        (offset % (MAX_WINDOW - len + 1), len)
    }

    /// cladefs board 2762, 2776: a request's values that mean nothing are refused, `EINVAL`, where
    /// they are decoded: an extent's sequence the last, of a request's own extent, a `FREE_MANY`'s
    /// further one or a mapping's; a range whose end is past a u64's; a window's range past
    /// [`MAX_WINDOW`]. The same requests a step inside, decoded.
    #[test]
    fn values_meaning_nothing_are_refused() {
        let decoded = |header: Header, payload: Vec<u8>| {
            Request::decode(&Message { header, payload, fds: vec![] }).err()
        };
        let refused = Some(Errno::EINVAL);
        let last = u64::MAX;
        let check = |ok_h: Header, past_h: Header, inside_h: Header, payload: Vec<u8>| {
            assert_eq!(decoded(ok_h, payload.clone()), None, "{ok_h:?}");
            assert_eq!(decoded(past_h, payload.clone()), refused, "{past_h:?}");
            assert_eq!(decoded(inside_h, payload), None, "{inside_h:?}");
        };
        check(
            Header::new(WRITE, 1).with_body(&WriteBody {
                cell: 1,
                sequence: 1,
                data_len: 4096,
                ..Default::default()
            }),
            Header::new(WRITE, 1).with_body(&WriteBody {
                cell: 1,
                sequence: last,
                data_len: 4096,
                ..Default::default()
            }),
            Header::new(WRITE, 1).with_body(&WriteBody {
                cell: 1,
                sequence: last - 1,
                data_len: 4096,
                ..Default::default()
            }),
            vec![7; 4096],
        );
        check(
            Header::new(FREE, 1).with_body(&ExtentBody { cell: 1, sequence: 1 }),
            Header::new(FREE, 1).with_body(&ExtentBody { cell: 1, sequence: last }),
            Header::new(FREE, 1).with_body(&ExtentBody { cell: 1, sequence: last - 1 }),
            vec![],
        );
        check(
            Header::new(READ, 1).with_body(&ReadBody {
                cell: 1,
                sequence: 1,
                offset: 0,
                len: 4096,
                transfer_at: 0,
            }),
            Header::new(READ, 1).with_body(&ReadBody {
                cell: 1,
                sequence: 1,
                offset: last - 4095,
                len: 4096,
                transfer_at: 0,
            }),
            Header::new(READ, 1).with_body(&ReadBody {
                cell: 1,
                sequence: 1,
                offset: last - 4096,
                len: 4096,
                transfer_at: 0,
            }),
            vec![],
        );
        check(
            Header::new(UNMAP, 1).with_body(&RangeBody { offset: 0, len: 4096 }),
            Header::new(UNMAP, 1).with_body(&RangeBody { offset: MAX_WINDOW, len: 1 }),
            Header::new(UNMAP, 1).with_body(&RangeBody { offset: MAX_WINDOW - 1, len: 1 }),
            vec![],
        );
        let ids = |sequence: u64| {
            ExtentIdWire { cell: 1u64.into(), sequence: sequence.into() }.as_bytes().to_vec()
        };
        let freeing = Header::new(FREE, 1).with_body(&ExtentBody { cell: 1, sequence: 1 });
        assert_eq!(decoded(freeing, ids(last)), refused);
        assert_eq!(decoded(freeing, ids(last - 1)), None);
        let mapping = |window_offset: u64, len: u64, offset: u64, sequence: u64| {
            MappingWire {
                window_offset: window_offset.into(),
                len: len.into(),
                cell: 1u64.into(),
                sequence: sequence.into(),
                offset: offset.into(),
                flags: 0u64.into(),
            }
            .as_bytes()
            .to_vec()
        };
        for (bad, why) in [
            (mapping(0, last, 0, 1), "its length"),
            (mapping(0, 1, 0, last), "its sequence"),
            (mapping(0, 1, last, 1), "its extent offset"),
            (mapping(MAX_WINDOW - 1, 2, 0, 1), "its window end"),
        ] {
            assert_eq!(mappings(&bad).err(), refused, "{why}");
            assert_eq!(decoded(Header::new(MAP, 1), bad), refused, "{why}");
        }
        assert_eq!(decoded(Header::new(MAP, 1), mapping(0, 1, last - 1, 1)), None);
        assert_eq!(decoded(Header::new(MAP, 1), mapping(MAX_WINDOW - 1, 1, 0, 1)), None);
        // As many as a payload holds, each the largest a window holds: their sum within a u64.
        let many = mapping(0, MAX_WINDOW, 0, 1).repeat((MAX_PAYLOAD / MAPPING_BYTES) as usize);
        let sum: u64 = mappings(&many).unwrap().iter().map(|mapping| mapping.len).sum();
        assert_eq!(sum, MAX_WINDOW * (MAX_PAYLOAD / MAPPING_BYTES));
    }

    /// Every kind of request.
    const KINDS: [u32; 16] = [
        OPEN, READ, WRITE, FREE, MAP, UNMAP, COMMIT, ROOT, INFO, WINDOW, FAULT, FORK, RESIZED,
        MAPPINGS, TRANSFER, STATS,
    ];

    /// `request`, as its receiver receives it: of ID `id`.
    fn sent(request: &Request<'_>, id: u64) -> Message {
        let Encoded { mut header, payload, fd } = request.encode();
        let payload: Vec<u8> = payload.concat();
        header.id = id;
        header.len = payload.len() as u64;
        header.fds = u32::from(fd.is_some());
        let fds = fd.map(|fd| fd.try_clone_to_owned().unwrap()).into_iter().collect();
        Message { header, payload, fds }
    }

    /// cladefs TASK-260: every request decodes as it is whether or not it says its guest request,
    /// which [`guest_request`] reads; one that says it and doesn't hold it, or says 0, is refused.
    #[test]
    fn requests_say_their_guest_request() {
        let fields = Fields {
            words: [3, 5, 4096, 8192, 1 << 20],
            flags: [true, false, false],
            data: (0..64).collect(),
            root: b"root".to_vec(),
            path: PathBuf::from("store"),
            extents: vec![ExtentId { cell: 7, sequence: 9 }],
            build: "build".into(),
            stats: "stats".into(),
            commit: Commit::Ordered,
            copying: false,
        };
        let area = area();
        let guest = NonZeroU64::new(0x1234_5678_9abc).unwrap();
        for request in requests(&fields, area.as_fd()) {
            let plain = sent(&request, 1);
            assert_eq!(guest_request(&plain).map(|(guest, _)| guest), Ok(None));
            let mut saying = sent(&request, 1);
            saying.header.flags |= REQUEST;
            saying.payload.splice(0..0, request_bytes(guest));
            saying.header.len += REQUEST_BYTES as u64;
            assert_eq!(guest_request(&saying).map(|(guest, _)| guest), Ok(Some(guest)));
            assert!(saying.header.len <= most_payload(&saying.header), "{request:?}");
            assert_eq!(Request::decode(&saying).as_ref(), Ok(&request), "{request:?}");
            let mut zero = sent(&request, 1);
            zero.header.flags |= REQUEST;
            zero.payload.splice(0..0, [0; REQUEST_BYTES]);
            assert_eq!(Request::decode(&zero).err(), Some(Errno::EINVAL), "{request:?}");
        }
        let mut short = sent(&Request::Info, 1);
        short.header.flags |= REQUEST;
        short.payload = vec![1; REQUEST_BYTES - 1];
        assert_eq!(Request::decode(&short).err(), Some(Errno::EINVAL));
    }

    /// Board 8459: the most payload each kind's header is admitted with ([`most_payload`]) is the
    /// most it decodes, at the edges: a root, a path, a build and a guest's stats each as long as
    /// they may be, with a guest request's bytes before them or not, decode, and a byte more is
    /// refused as it is decoded too. So the refusal at the header refuses nothing a request holds,
    /// and admits nothing it doesn't. Red with a kind's limit a byte short, or long.
    #[test]
    fn requests_payloads_reach_their_kinds_most_and_no_more() {
        let open = || {
            let body = OpenBody { version: VERSION, ..Default::default() };
            Header::new(OPEN, 1).with_body(&body)
        };
        let root = |flags| Header::new(COMMIT, 1).with_flags(flags);
        let stats =
            || Header::new(STATS, 1).with_flags(STATS_GUEST).with_body(&StatsBody::default());
        let open_at: Vec<u8> =
            [vec![b'a'; MOST_PATH as usize], vec![0], vec![b'b'; MAX_BUILD]].concat();
        let open_past: Vec<u8> =
            [vec![b'a'; MOST_PATH as usize], vec![0], vec![b'b'; MAX_BUILD + 1]].concat();
        let cases: [(Header, Vec<u8>, Vec<u8>); 3] = [
            (open(), open_at, open_past),
            (root(COMMIT_ROOT), vec![b'r'; MAX_ROOT], vec![b'r'; MAX_ROOT + 1]),
            (stats(), vec![b's'; MAX_STATS], vec![b's'; MAX_STATS + 1]),
        ];
        let decoded = |mut header: Header, saying: bool, payload: Vec<u8>| {
            let payload = match saying {
                true => {
                    header.flags |= REQUEST;
                    [request_bytes(NonZeroU64::new(9).unwrap()).to_vec(), payload].concat()
                }
                false => payload,
            };
            header.len = payload.len() as u64;
            let admitted = header.len <= most_payload(&header);
            (admitted, Request::decode(&Message { header, payload, fds: vec![] }).is_ok())
        };
        for (header, at_payload, past_payload) in cases {
            for saying in [false, true] {
                let at = decoded(header, saying, at_payload.clone());
                assert_eq!(
                    at,
                    (true, true),
                    "{header:?} of {} bytes, saying {saying}",
                    at_payload.len()
                );
                let past = decoded(header, saying, past_payload.clone());
                assert_eq!(
                    past,
                    (false, false),
                    "{header:?} of {} bytes, saying {saying}",
                    past_payload.len()
                );
            }
        }
        assert_eq!(
            most_payload(&Header::new(STAGE_COUNTS, 1)),
            size_of::<PrivateCountsBody>() as u64
        );
        // Kinds that carry nothing: a byte is past their most, and refused decoded.
        for kind in [READ, UNMAP, ROOT, INFO, WINDOW, FAULT, ACK, FORK, 0xffff] {
            let mut header = Header::new(kind, 1);
            header.len = 1;
            assert_eq!(most_payload(&header), 0, "kind {kind}");
        }
    }

    /// A transfer area's descriptor, for requests to carry.
    fn area() -> OwnedFd {
        tempfile::tempfile().unwrap().into()
    }

    /// TASK-160.9: each kind of request, each of its ways, decodes as what was encoded, whatever
    /// its fields; and nothing but those kinds are requests.
    #[test]
    fn requests_decode_as_encoded() {
        let area = area();
        proptest!(|(drawn in fields(), id in any::<u64>())| {
            let requests = requests(&drawn, area.as_fd());
            let kinds: std::collections::BTreeSet<u32> =
                requests.iter().map(Request::kind).collect();
            let all: std::collections::BTreeSet<u32> = KINDS.into_iter().collect();
            prop_assert_eq!(kinds, all);
            for request in requests {
                let message = sent(&request, id);
                prop_assert_eq!(message.header.kind, request.kind());
                // cladefs: admitted at its header (board 8459).
                prop_assert!(message.header.len <= most_payload(&message.header), "{request:?}");
                prop_assert_eq!(Request::decode(&message), Ok(request));
            }
        });
        for kind in (0..256).filter(|kind| !KINDS.contains(kind)) {
            let message = Message { header: Header::new(kind, 1), payload: vec![], fds: vec![] };
            assert_eq!(Request::decode(&message).err(), Some(Errno::EINVAL), "kind {kind}");
        }
    }

    /// TASK-160.9: a request with a flag its kind doesn't know, any of them, is refused, as is one
    /// with flags it knows that mean nothing together.
    #[test]
    fn requests_with_flags_their_kinds_dont_know_are_refused() {
        let known = |kind: u32| -> u32 {
            match kind {
                OPEN => 0b10_0111,
                READ => 0b1,
                WRITE => 0b11_1111,
                COMMIT => 0b111,
                FAULT => 0b100,
                WINDOW => 0b1,
                FORK => 0b11,
                STATS => 0b1,
                MAPPINGS => 0b1,
                MAP => 0b1,
                _ => 0,
            }
        };
        let fields = Fields {
            words: [1, 2, 4096, 4096, 8192],
            flags: [false; 3],
            data: vec![7; 4096],
            root: Vec::new(),
            path: PathBuf::from("store"),
            extents: vec![ExtentId { cell: 1, sequence: 3 }],
            build: "abc".into(),
            stats: "seals 1\n".into(),
            commit: Commit::Ordered,
            copying: false,
        };
        let area = area();
        let requests = requests(&fields, area.as_fd());
        for kind in KINDS {
            let request = requests.iter().find(|request| request.kind() == kind).unwrap();
            let mut message = sent(request, 1);
            assert!(Request::decode(&message).is_ok(), "{request:?}");
            let unknown = |bit: &u32| (known(kind) | REQUEST) & *bit == 0;
            for bit in (0..32).map(|bit| 1u32 << bit).filter(unknown) {
                message.header.flags = bit;
                let decoded = Request::decode(&message);
                assert_eq!(decoded.err(), Some(Errno::EINVAL), "kind {kind}, flag {bit:#x}");
            }
        }
        let refused = |kind, flags, payload: &[u8]| {
            let header = if kind == OPEN {
                with_version(Header::new(kind, 1).with_flags(flags))
            } else {
                Header::new(kind, 1).with_flags(flags)
            };
            let message = Message { header, payload: payload.to_vec(), fds: vec![] };
            assert_eq!(Request::decode(&message).err(), Some(Errno::EINVAL), "{kind}, {flags:#x}");
        };
        refused(OPEN, OPEN_CREATE | OPEN_FORK, b"store\0abc");
        // cladefs: a follower's `OPEN_READ_ONLY`, gone (board 8237).
        refused(OPEN, 1 << 2, b"store\0abc");
        refused(WRITE, WRITE_UPLOADED, b"data");
        refused(WRITE, WRITE_LOCAL, b"data");
        refused(WRITE, WRITE_ROOT, b"data");
        refused(WRITE, WRITE_COMMIT | WRITE_UPLOADED | WRITE_LOCAL, b"data");
        refused(COMMIT, COMMIT_UPLOADED | COMMIT_LOCAL, b"");
        refused(FAULT, 1 << 0, b"");
        refused(FAULT, 1 << 1, b"");
        refused(FORK, 0, b"");
        refused(FORK, 0, b"store");
        refused(FORK, FORK_QUIESCENT | FORK_DROP, b"");
        let phys = PhysLocation { arena: ArenaId::DEFAULT, offset: 0 };
        let ops = [
            StageOp::Map { offset: 0, phys, len: 4096 },
            StageOp::Unmap { offset: 0, len: 4096, keep_private: false },
            StageOp::Unmap { offset: 0, len: 4096, keep_private: true },
            StageOp::Mapped { offset: 0, len: 4096 },
            StageOp::Remap {
                offset: 0,
                len: 4096,
                keep_private: false,
                anew: vec![
                    cladefs_core::dax::Anew::Mapped { offset: 0, len: 4096 },
                    cladefs_core::dax::Anew::Map { offset: 0, phys, len: 4096 },
                ],
            },
            StageOp::Remap {
                offset: 0,
                len: 4096,
                keep_private: true,
                anew: vec![cladefs_core::dax::Anew::Mapped { offset: 0, len: 4096 }],
            },
        ];
        let told = ops
            .into_iter()
            .map(|op| Told::Stage(Stage { id: 1, op }))
            .chain([Told::Capacity { capacity: 4096, ask: false }, Told::Stats { ask: 1 }]);
        for told in told {
            let (header, payload) = told.encode();
            assert_eq!(Told::decode(&header, &payload), Ok(told.clone()));
            for bit in (0..32).map(|bit| 1u32 << bit) {
                let flipped = Header { flags: header.flags ^ bit, ..header };
                let known = matches!(told, Told::Capacity { .. }) && bit == CAPACITY_ASK
                    || matches!(
                        told,
                        Told::Stage(Stage {
                            op: StageOp::Unmap { .. } | StageOp::Remap { .. },
                            ..
                        })
                    ) && bit == STAGE_KEEP_PRIVATE;
                assert_eq!(
                    Told::decode(&flipped, &payload).is_ok(),
                    known,
                    "{told:?}, flag {bit:#x}"
                );
            }
        }
        for bit in (0..32).map(|bit| 1u32 << bit) {
            let header = Header::new(ACK, 1).with_flags(bit);
            let made = Message { header, payload: vec![], fds: vec![] };
            assert_eq!(Made::decode(&made).err(), Some(Errno::EINVAL), "{bit:#x}");
        }
    }

    /// TASK-160.9, board 6917: what is refused besides: a payload, or descriptors, a request has
    /// no use for, arguments out of range, non-zero trailing or reserved body bytes, a status, and
    /// paths that name nothing.
    #[test]
    fn requests_malformed_otherwise_are_refused() {
        let refused = |header: Header, payload: &[u8], fds: Vec<OwnedFd>| {
            let message = Message { header, payload: payload.to_vec(), fds };
            assert_eq!(Request::decode(&message).err(), Some(Errno::EINVAL), "{header:?}");
        };
        let read_body = ReadBody { cell: 1, sequence: 1, offset: 0, len: 4096, transfer_at: 0 };
        refused(Header::new(READ, 1).with_body(&read_body), b"extra", vec![]);
        let too_big = ReadBody { len: MAX_PAYLOAD + 1, ..read_body };
        refused(Header::new(READ, 1).with_body(&too_big), b"", vec![]);
        refused(Header::new(INFO, 1), b"", vec![area()]);
        refused(Header::new(TRANSFER, 1).with_body(&TransferBody { len: 4096 }), b"", vec![]);
        refused(
            Header::new(TRANSFER, 1).with_body(&TransferBody { len: 4096 }),
            b"",
            vec![area(), area()],
        );
        refused(
            Header::new(WINDOW, 1).with_body(&WindowBody { size: MAX_WINDOW + 1, budget: 0 }),
            b"",
            vec![],
        );
        // Non-zero trailing bytes past a typed body struct (or on an empty-body kind) are refused.
        let mut dirty_window =
            Header::new(WINDOW, 1).with_body(&WindowBody { size: 1 << 20, budget: 0 });
        dirty_window.body.0[size_of::<WindowBody>()] = 1;
        refused(dirty_window, b"", vec![]);
        let mut dirty_info = Header::new(INFO, 1);
        dirty_info.body.0[0] = 1;
        refused(dirty_info, b"", vec![]);
        refused(
            Header::new(RESIZED, 1).with_body(&ResizedBody { capacity: 4096, took: 2 }),
            b"",
            vec![],
        );
        refused(Header { status: -1, ..Header::new(INFO, 1) }, b"", vec![]);
        refused(
            Header::new(FREE, 1).with_body(&ExtentBody { cell: 1, sequence: 1 }),
            &[0; EXTENT_ID_BYTES + 8],
            vec![],
        );
        refused(Header::new(MAP, 1), b"", vec![]);
        refused(Header::new(MAP, 1), &[0; MAPPING_BYTES as usize - 8], vec![]);
        let unknown = MappingWire {
            window_offset: 0u64.into(),
            len: 4096u64.into(),
            cell: 1u64.into(),
            sequence: 1u64.into(),
            offset: 0u64.into(),
            flags: 1u64.into(),
        };
        refused(Header::new(MAP, 1), unknown.as_bytes(), vec![]);
        let write = Header::new(WRITE, 1)
            .with_body(&WriteBody { cell: 1, sequence: 1, offset: 0, transfer_at: 0, data_len: 8 })
            .with_flags(WRITE_COMMIT | WRITE_ROOT);
        refused(write, b"data", vec![]);
        let transfer = Header::new(WRITE, 1)
            .with_body(&WriteBody { cell: 1, sequence: 1, offset: 0, transfer_at: 0, data_len: 8 })
            .with_flags(WRITE_TRANSFER);
        refused(transfer, b"no root", vec![]);
        refused(Header::new(COMMIT, 1), b"no root", vec![]);
        // board 6917: words a request's flags give no meaning are 0.
        let inline = WriteBody { cell: 1, sequence: 1, offset: 0, transfer_at: 0, data_len: 4 };
        refused(Header::new(WRITE, 1).with_body(&inline), b"data and more", vec![]);
        let aimed = WriteBody { transfer_at: 0xdead, ..inline };
        refused(Header::new(WRITE, 1).with_body(&aimed), b"data", vec![]);
        let aimed = ReadBody { transfer_at: 0xdead, ..read_body };
        refused(Header::new(READ, 1).with_body(&aimed), b"", vec![]);
        let zeros_many = MappingWire {
            window_offset: 0u64.into(),
            len: 4096u64.into(),
            cell: 0u64.into(),
            sequence: 0u64.into(),
            offset: 4096u64.into(),
            flags: MAPPING_ZEROS.into(),
        };
        refused(Header::new(MAP, 1), zeros_many.as_bytes(), vec![]);
        for (flags, body) in [
            (0, OpenBody { tenant: 1, ..Default::default() }),
            (OPEN_CREATE, OpenBody { capacity: 4096, cell: 1, ..Default::default() }),
        ] {
            let body = OpenBody { version: VERSION, ..body };
            refused(Header::new(OPEN, 1).with_flags(flags).with_body(&body), b"s\0abc", vec![]);
        }
        let open = |flags| with_version(Header::new(OPEN, 1).with_flags(flags));
        for payload in
            [&b""[..], &b"s"[..], &b"\0abc"[..], &b"s\0"[..], &b"a\0b\0c"[..], &b"s\0a\"b"[..]]
        {
            refused(open(OPEN_CREATE), payload, vec![]);
        }
        refused(open(OPEN_CREATE), &[&b"s\0"[..], &[b'a'; MAX_BUILD + 1]].concat(), vec![]);
        refused(Header::new(FORK, 1), b"s", vec![]);
        refused(open(OPEN_FORK), b"a\0b\0c", vec![]);
        refused(
            Header::new(FORK, 1).with_flags(FORK_QUIESCENT),
            b"a version names no path",
            vec![],
        );
        refused(Header::new(STATS, 1), b"seals 1\n", vec![]);
        let guest = Header::new(STATS, 1).with_flags(STATS_GUEST);
        refused(guest, &[b'\n'; MAX_STATS + 1], vec![]);
        refused(guest, b"seals \xff\n", vec![]);
    }

    /// board 6917: each request, stage op, told and made message encodes into its exact typed
    /// `#[repr(C)]` wire struct.
    #[test]
    fn requests_encode_into_their_typed_bodies() {
        let extent = ExtentId { cell: 7, sequence: 9 };
        let area = area();
        let root =
            WriteBarriers { before: true, after: Some(Commit::Uploaded), root: Some(&b"r"[..]) };
        let cases: Vec<(Request<'_>, Header, Vec<u8>)> = vec![
            (
                Request::Open {
                    path: "s".into(),
                    opening: Opening::Create(4096, Kind::Fxfs),
                    build: "abc",
                },
                Header::new(OPEN, 0).with_flags(OPEN_CREATE).with_body(&OpenBody {
                    capacity: 4096,
                    version: VERSION,
                    ..Default::default()
                }),
                b"s\0abc".to_vec(),
            ),
            (
                Request::Open {
                    path: "s".into(),
                    opening: Opening::ForkOf { tenant: 2, cell: 3, version: 9, capacity: 4096 },
                    build: "abc",
                },
                Header::new(OPEN, 0).with_flags(OPEN_FORK).with_body(&OpenBody {
                    capacity: 4096,
                    cell: 3,
                    cell_version: 9,
                    tenant: 2,
                    version: VERSION,
                }),
                b"s\0abc".to_vec(),
            ),
            (
                Request::Read { extent, offset: 1, len: 2, into: ReadInto::Reply },
                Header::new(READ, 0).with_body(&ReadBody {
                    cell: 7,
                    sequence: 9,
                    offset: 1,
                    len: 2,
                    transfer_at: 0,
                }),
                vec![],
            ),
            (
                Request::Read { extent, offset: 1, len: 2, into: ReadInto::Transfer { at: 3 } },
                Header::new(READ, 0).with_flags(READ_TRANSFER).with_body(&ReadBody {
                    cell: 7,
                    sequence: 9,
                    offset: 1,
                    len: 2,
                    transfer_at: 3,
                }),
                vec![],
            ),
            (
                Request::Write {
                    extent,
                    offset: 1,
                    data: Data::Inline(&b"dd"[..]),
                    barriers: root,
                },
                Header::new(WRITE, 0).with_flags(0b1111).with_body(&WriteBody {
                    cell: 7,
                    sequence: 9,
                    offset: 1,
                    transfer_at: 0,
                    data_len: 2,
                }),
                b"ddr".to_vec(),
            ),
            (
                Request::Write {
                    extent,
                    offset: 1,
                    data: Data::Transfer { at: 3, len: 2 },
                    barriers: WriteBarriers::default(),
                },
                Header::new(WRITE, 0).with_flags(WRITE_TRANSFER).with_body(&WriteBody {
                    cell: 7,
                    sequence: 9,
                    offset: 1,
                    transfer_at: 3,
                    data_len: 2,
                }),
                vec![],
            ),
            (
                Request::Free { first: extent, rest: Cow::Owned(vec![extent]) },
                Header::new(FREE, 0).with_body(&ExtentBody { cell: 7, sequence: 9 }),
                ExtentIdWire { cell: 7u64.into(), sequence: 9u64.into() }.as_bytes().to_vec(),
            ),
            (
                Request::Map {
                    mappings: Cow::Owned(vec![
                        WindowMapping {
                            window_offset: 5,
                            len: 4,
                            contents: MappingContents::Extent { extent, offset: 3 },
                        },
                        WindowMapping {
                            window_offset: 6,
                            len: 4,
                            contents: MappingContents::Zeros,
                        },
                    ]),
                    warm: false,
                },
                Header::new(MAP, 0),
                [5u64, 4, 7, 9, 3, 0, 6, 4, 0, 0, 0, MAPPING_ZEROS].map(u64::to_le_bytes).concat(),
            ),
            (
                Request::Map {
                    mappings: Cow::Owned(vec![WindowMapping {
                        window_offset: 5,
                        len: 4,
                        contents: MappingContents::Extent { extent, offset: 3 },
                    }]),
                    warm: true,
                },
                Header::new(MAP, 0).with_flags(MAP_WARM),
                [5u64, 4, 7, 9, 3, 0].map(u64::to_le_bytes).concat(),
            ),
            (
                Request::Commit { commit: Commit::Uploaded, root: Some(&b"r"[..]) },
                Header::new(COMMIT, 0).with_flags(COMMIT_ROOT | COMMIT_UPLOADED),
                b"r".to_vec(),
            ),
            (
                Request::Fault(Fault { offset: 5, probe: false }),
                Header::new(FAULT, 0).with_body(&FaultBody { offset: 5 }),
                vec![],
            ),
            (
                Request::Fault(Fault { offset: 5, probe: true }),
                Header::new(FAULT, 0).with_flags(FAULT_PROBE).with_body(&FaultBody { offset: 5 }),
                vec![],
            ),
            (Request::Version, Header::new(FORK, 0).with_flags(FORK_QUIESCENT), vec![]),
            (
                Request::DropVersion { version: 5 },
                Header::new(FORK, 0)
                    .with_flags(FORK_DROP)
                    .with_body(&DropVersionBody { version: 5 }),
                vec![],
            ),
            (
                Request::Resized { capacity: 4096, took: true },
                Header::new(RESIZED, 0).with_body(&ResizedBody { capacity: 4096, took: 1 }),
                vec![],
            ),
            (
                Request::Transfer { len: 4096, area: Area(area.as_fd()) },
                Header::new(TRANSFER, 0).with_body(&TransferBody { len: 4096 }),
                vec![],
            ),
            (
                Request::Window { size: 8192, copying: false, budget: 0 },
                Header::new(WINDOW, 0).with_body(&WindowBody { size: 8192, budget: 0 }),
                vec![],
            ),
            (
                Request::Window { size: 8192, copying: true, budget: 3 },
                Header::new(WINDOW, 0)
                    .with_flags(WINDOW_COPYING)
                    .with_body(&WindowBody { size: 8192, budget: 3 }),
                vec![],
            ),
            (
                Request::Stats { ask: 5, guest: Some("seals 1\n") },
                Header::new(STATS, 0).with_flags(STATS_GUEST).with_body(&StatsBody { ask: 5 }),
                b"seals 1\n".to_vec(),
            ),
            (
                Request::Stats { ask: 5, guest: None },
                Header::new(STATS, 0).with_body(&StatsBody { ask: 5 }),
                vec![],
            ),
        ];
        for (request, header, payload) in cases {
            let encoded = request.encode();
            let bytes = (encoded.header, encoded.payload.concat());
            assert_eq!(bytes, (header, payload), "{request:?}");
            assert_eq!(encoded.fd.is_some(), request.kind() == TRANSFER);
        }
        let map = Stage {
            id: 4,
            op: StageOp::Map {
                offset: 1,
                phys: PhysLocation { arena: ArenaId(6), offset: 2 },
                len: 3,
            },
        };
        assert_eq!(
            map.encode().0,
            Header::new(STAGE_MAP, 4).with_body(&StageMapBody {
                offset: 1,
                phys_offset: 2,
                len: 3,
                arena: 6,
                reserved: 0,
            })
        );
        let unmap = Stage { id: 4, op: StageOp::Unmap { offset: 1, len: 2, keep_private: false } };
        let acked = Header::new(STAGE_UNMAP, 4)
            .with_flags(STAGE_ACK)
            .with_body(&RangeBody { offset: 1, len: 2 });
        assert_eq!(unmap.encode().0, acked);
        let kept = Stage { id: 4, op: StageOp::Unmap { offset: 1, len: 2, keep_private: true } };
        assert_eq!(kept.encode().0, acked.with_flags(STAGE_ACK | STAGE_KEEP_PRIVATE));
        let (ask, ask_payload) = Told::Capacity { capacity: 4096, ask: true }.encode();
        assert!(ask_payload.is_empty());
        assert_eq!(
            ask,
            Header::new(CAPACITY, 0)
                .with_flags(CAPACITY_ASK)
                .with_body(&CapacityBody { capacity: 4096 })
        );
        let counts = StageCounts { costs: [1, 2, 3, 4], installed: 5, private: 6, budget: 0 };
        let (ack, ack_payload) = Made::Ack { id: 4, counts }.encode();
        assert!(ack_payload.is_empty());
        assert_eq!(
            ack,
            Header::new(ACK, 4).with_body(&StageCountsBody {
                ops: 1,
                ns: 2,
                overwrites: 3,
                leave_outs: 4,
                installed: 5,
                private: 6,
            })
        );
        // cladefs: a STAGE_COUNTS says its window's budget in its id (board 10098) and carries its
        // private counts in its 40-byte payload (board 11697).
        let (counted, counted_payload) =
            Made::Counts { stage: StageCounts { budget: 7, ..counts }, private: [1, 64, 5, 2, 9] }
                .encode();
        let mut expected_counted = Header::new(STAGE_COUNTS, 7).with_body(&ack.body.0);
        expected_counted.len = size_of::<PrivateCountsBody>() as u64;
        assert_eq!(counted, expected_counted);
        assert_eq!(
            counted_payload,
            PrivateCountsBody { runs: 1, pages: 64, ns: 5, requests: 2, request_ns: 9 }.as_bytes()
        );
        let (asked, asked_payload) = Told::Stats { ask: 6 }.encode();
        assert!(asked_payload.is_empty());
        assert_eq!(asked, Header::new(STATS_ASK, 0).with_body(&StatsBody { ask: 6 }));
    }

    proptest! {
        /// TASK-160.9: what the daemon tells, each stage operation among it, and what VM processes
        /// say of them, decode as encoded.
        #[test]
        fn told_and_made_decode_as_encoded(
            id in any::<u64>(),
            [a, b, c, d, e] in any::<[u64; 5]>(),
            arena in any::<u32>(),
            ask in any::<bool>(),
        ) {
            let phys = PhysLocation { arena: ArenaId(arena), offset: b };
            let stages = [
                StageOp::Map { offset: a, phys, len: c },
                StageOp::Unmap { offset: a, len: b, keep_private: ask },
                StageOp::Mapped { offset: a, len: b },
            ];
            let told = stages.into_iter().map(|op| Told::Stage(Stage { id, op })).chain([
                Told::Capacity { capacity: a, ask },
                Told::Stats { ask: b },
            ]);
            for told in told {
                let (h, p) = told.encode();
                prop_assert_eq!(Told::decode(&h, &p), Ok(told));
            }
            let low = |word: u64| word & u64::from(u32::MAX);
            let counts =
                StageCounts { costs: [a, b, c, d], installed: low(e), private: low(a), budget: 0 };
            let other =
                StageCounts { costs: [b, c, d, e], installed: low(a), private: low(b), budget: id };
            let made = [
                Made::Ack { id, counts },
                Made::Counts { stage: other, private: [a, b, c, d, e] },
            ];
            for made in made {
                let (header, payload) = made.encode();
                let message = Message { header, payload, fds: vec![] };
                prop_assert_eq!(Made::decode(&message), Ok(made));
            }
        }

        /// TASK-160.9: what replies say besides their status decodes as encoded.
        #[test]
        fn replies_decode_as_encoded(
            [a, b, c, d, e] in any::<[u64; 5]>(),
            arena in any::<u32>(),
            mappings_of in proptest::collection::vec((any::<[u64; 5]>(), any::<bool>()), 0..16),
        ) {
            let max_map_count = (c != 0).then_some(c);
            for ephemeral in [false, true] {
                let opened = Opened {
                    cell: a,
                    tenant: b,
                    arena: ArenaId(arena),
                    max_map_count,
                    ephemeral,
                    capacity: d,
                };
                let header = Header::new(REPLY, 1).with_body(&opened.to_body());
                prop_assert_eq!(Opened::decode(&header, &opened.capacity_bytes()), Ok(opened));
            }
            let info = Info(ExtentInfo {
                capacity: a,
                used: b,
                available: c,
                next_sequence: d,
                sequence_floor: e,
            });
            let header = Header::new(REPLY, 1).with_body(&info.to_body());
            prop_assert_eq!(Info::decode(&header), Ok(info));
            let forked = Forked { version: a, tenant: b, committed: 0, ephemeral: false };
            let header = Header::new(REPLY, 1).with_body(&forked.to_body());
            prop_assert_eq!(Forked::decode(&header), Ok(forked));
            let forked_ephemeral = Forked { version: 0, tenant: b, committed: d, ephemeral: true };
            let header = Header::new(REPLY, 1).with_body(&forked_ephemeral.to_body());
            prop_assert_eq!(Forked::decode(&header), Ok(forked_ephemeral));
            let mapped: Vec<WindowMapping> = (mappings_of.into_iter())
                .map(|([window_offset, len, cell, sequence, offset], zeros)| {
                    let (window_offset, len) = windowed(window_offset, len);
                    WindowMapping {
                        window_offset,
                        len,
                        contents: match zeros {
                            true => MappingContents::Zeros,
                            false => {
                                let extent = known(cell, sequence);
                                MappingContents::Extent { extent, offset: ending(offset, len) }
                            }
                        },
                    }
                })
                .collect();
            let payload = mappings_payload(&mapped);
            prop_assert_eq!(payload.len() as u64, mapped.len() as u64 * MAPPING_BYTES);
            prop_assert_eq!(mappings(&payload), Ok(mapped));
        }
    }

    /// Replies and stage operations malformed: non-zero reserved/trailing bytes, mappings of part
    /// of one, or of flags unknown or meaning nothing together.
    #[test]
    fn replies_malformed_are_refused() {
        let mut bad_opened = OpenedBody {
            cell: 1,
            tenant: 2,
            max_map_count: 0,
            arena: 1,
            flags: 1 << 1,
            version: VERSION,
        };
        let cap = 4096u64.to_le_bytes();
        assert_eq!(
            Opened::decode(&Header::new(REPLY, 1).with_body(&bad_opened), &cap).err(),
            Some(Errno::EINVAL)
        );
        bad_opened.flags = OPENED_EPHEMERAL;
        assert!(Opened::decode(&Header::new(REPLY, 1).with_body(&bad_opened), &cap).is_ok());
        assert_eq!(
            Opened::decode(&Header::new(REPLY, 1).with_body(&bad_opened), &[]).err(),
            Some(Errno::EINVAL)
        );

        let bad_stage = Header::new(STAGE_MAP, 1).with_body(&StageMapBody {
            offset: 0,
            phys_offset: 0,
            len: 4096,
            arena: 0,
            reserved: 1,
        });
        assert_eq!(Stage::decode(&bad_stage, &[]).err(), Some(Errno::EINVAL));
        // STAGE_REMAP malformed cases: empty payload, out-of-range entry, unknown entry kind.
        let mut empty_remap = Header::new(STAGE_REMAP, 1)
            .with_flags(STAGE_ACK)
            .with_body(&RangeBody { offset: 0, len: 4096 });
        assert_eq!(Stage::decode(&empty_remap, &[]).err(), Some(Errno::EINVAL));
        let oob_entry = StageAnewEntry {
            kind: STAGE_MAPPED,
            arena: 0,
            offset: 4096,
            phys_offset: 0,
            len: 4096,
        };
        empty_remap.len = size_of::<StageAnewEntry>() as u64;
        assert_eq!(Stage::decode(&empty_remap, oob_entry.as_bytes()).err(), Some(Errno::EINVAL));
        let bad_kind =
            StageAnewEntry { kind: STAGE_UNMAP, arena: 0, offset: 0, phys_offset: 0, len: 4096 };
        assert_eq!(Stage::decode(&empty_remap, bad_kind.as_bytes()).err(), Some(Errno::EINVAL));

        let mut dirty_forked = Header::new(REPLY, 1).with_body(&ForkedBody {
            version: 2,
            tenant: 3,
            committed: 0,
            flags: 0,
            reserved: 0,
        });
        dirty_forked.body.0[size_of::<ForkedBody>()] = 1;
        assert_eq!(Forked::decode(&dirty_forked).err(), Some(Errno::EINVAL));
        for bad_forked in [
            ForkedBody { version: 2, tenant: 3, committed: 1, flags: 0, reserved: 0 },
            ForkedBody {
                version: 2,
                tenant: 3,
                committed: 1,
                flags: FORKED_EPHEMERAL,
                reserved: 0,
            },
            ForkedBody { version: 0, tenant: 3, committed: 1, flags: 1 << 1, reserved: 0 },
            ForkedBody {
                version: 0,
                tenant: 3,
                committed: 1,
                flags: FORKED_EPHEMERAL,
                reserved: 1,
            },
        ] {
            assert_eq!(
                Forked::decode(&Header::new(REPLY, 1).with_body(&bad_forked)).err(),
                Some(Errno::EINVAL)
            );
        }

        let mapping = |flags: u64| {
            MappingWire {
                window_offset: 0u64.into(),
                len: 4096u64.into(),
                cell: 1u64.into(),
                sequence: 1u64.into(),
                offset: 0u64.into(),
                flags: flags.into(),
            }
            .as_bytes()
            .to_vec()
        };
        assert_eq!(mappings(&mapping(0)[..40]).err(), Some(Errno::EINVAL));
        for flags in [1, MAPPING_ZEROS | 1, 1 << 2, 1 << 63] {
            assert_eq!(mappings(&mapping(flags)).err(), Some(Errno::EINVAL), "{flags:#x}");
        }
    }

    /// TASK-141.3: an `OPEN` of another wire version is refused as such, whatever else it holds,
    /// as is a reply to one from a daemon of another, an older one's, saying none, among them.
    #[test]
    fn opens_of_other_wire_versions_are_refused() {
        let area = area();
        proptest!(|(drawn in fields(), id in any::<u64>(), flags in any::<u32>())| {
            let opens = requests(&drawn, area.as_fd()).into_iter().filter(|r| r.kind() == OPEN);
            for open in opens {
                let mut message = sent(&open, id);
                prop_assert_eq!(version(&message.header), VERSION);
                for other in [0, VERSION - 1, VERSION + 1, u64::MAX] {
                    message.header.body.0[VERSION_OFFSET..VERSION_OFFSET + 8]
                        .copy_from_slice(other.as_bytes());
                    let refused = Some(Errno::EPROTONOSUPPORT);
                    prop_assert_eq!(Request::decode(&message).err(), refused);
                    message.header.flags = flags;
                    prop_assert_eq!(Request::decode(&message).err(), refused);
                }
            }
        });
        let opened = Opened {
            cell: 1,
            tenant: 2,
            arena: ArenaId::DEFAULT,
            max_map_count: None,
            ephemeral: false,
            capacity: 4096,
        };
        assert_eq!(version(&Header::new(REPLY, 1).with_body(&opened.to_body())), VERSION);
        for other in [0, VERSION - 1, VERSION + 1] {
            let body = OpenedBody { version: other, ..opened.to_body() };
            let header = Header::new(REPLY, 1).with_body(&body);
            assert_eq!(Opened::decode(&header, &[]).err(), Some(Errno::EPROTONOSUPPORT), "{other}");
            // Before its flags field, which another version may use otherwise.
            let body = OpenedBody { flags: u32::MAX, version: other, ..opened.to_body() };
            let header = Header::new(REPLY, 1).with_body(&body);
            assert_eq!(Opened::decode(&header, &[]).err(), Some(Errno::EPROTONOSUPPORT), "{other}");
        }
    }

    /// TASK-141.3: the wire as this [`VERSION`] has it, as a fingerprint of everything its two ends
    /// must agree on: what each kind of message encodes to, of fixed fields, and decodes from (the
    /// flags each kind knows), the replies' bodies, the [`Header`]'s layout, and the bounds.
    /// A change to any of it without a new `VERSION` fails here: a VM process and a daemon of the
    /// two would meet as of one version, and serve each other with what neither means.
    #[test]
    fn the_wire_is_its_version() {
        /// The version, and the fingerprint of the wire it is. A change to the wire is a new
        /// version, both changed together, never the fingerprint alone.
        const WIRE: (u64, u64) = (23, 0xab82a34c3729a705);
        let fingerprint = fingerprint();
        assert_eq!(
            (VERSION, fingerprint),
            WIRE,
            "the wire changed, its fingerprint {fingerprint:#x}: wire::VERSION goes up with it, to \
             {}, and WIRE here to ({}, {fingerprint:#x}) (AGENTS.md: no migrations)",
            WIRE.0 + 1,
            WIRE.0 + 1,
        );
    }

    /// The wire's fingerprint ([`the_wire_is_its_version`]): FNV-1a of what it is made of.
    fn fingerprint() -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut hashed = |bytes: &[u8]| {
            for byte in bytes {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
            }
            // Where each part ends.
            hash = (hash ^ 0xff).wrapping_mul(0x0100_0000_01b3);
        };
        let header = |hashed: &mut dyn FnMut(&[u8]), header: Header| {
            hashed(zerocopy::IntoBytes::as_bytes(&header))
        };
        hashed(&std::mem::size_of::<Header>().to_le_bytes());
        let fields = Fields {
            words: [3, 5, 4096, 8192, 1 << 20],
            flags: [true, true, false],
            data: (0..64).collect(),
            root: b"root".to_vec(),
            path: PathBuf::from("store"),
            extents: vec![ExtentId { cell: 7, sequence: 9 }, ExtentId { cell: 7, sequence: 11 }],
            build: "build".into(),
            stats: "stats".into(),
            commit: Commit::Uploaded,
            copying: true,
        };
        let area = area();
        for request in requests(&fields, area.as_fd()) {
            let message = sent(&request, 1);
            header(&mut hashed, message.header);
            hashed(&message.payload);
        }
        for kind in 0..256 {
            hashed(&known_flags(kind).map_or(u64::MAX, u64::from).to_le_bytes());
        }
        hashed(&REQUEST.to_le_bytes());
        hashed(&request_bytes(NonZeroU64::new(5).unwrap()));
        let phys = PhysLocation { arena: ArenaId(6), offset: 1 << 21 };
        let told = [
            StageOp::Map { offset: 4096, phys, len: 8192 },
            StageOp::Unmap { offset: 4096, len: 8192, keep_private: false },
            StageOp::Unmap { offset: 4096, len: 8192, keep_private: true },
            StageOp::Mapped { offset: 4096, len: 8192 },
            StageOp::Remap {
                offset: 4096,
                len: 8192,
                keep_private: false,
                anew: vec![
                    cladefs_core::dax::Anew::Mapped { offset: 4096, len: 8192 },
                    cladefs_core::dax::Anew::Map { offset: 4096, phys, len: 4096 },
                ],
            },
        ]
        .into_iter()
        .map(|op| Told::Stage(Stage { id: 2, op }))
        .chain([Told::Capacity { capacity: 1 << 30, ask: true }, Told::Stats { ask: 6 }]);
        for told in told {
            let (h, p) = told.encode();
            header(&mut hashed, h);
            hashed(&p);
        }
        let (counts, other) = (
            StageCounts { costs: [1, 2, 3, 4], installed: 5, private: 6, budget: 0 },
            StageCounts { costs: [5, 6, 7, 8], installed: 9, private: 10, budget: 11 },
        );
        for made in
            [Made::Ack { id: 2, counts }, Made::Counts { stage: other, private: [1, 64, 5, 2, 9] }]
        {
            let (h, p) = made.encode();
            header(&mut hashed, h);
            hashed(&p);
        }
        let opened = Opened {
            cell: 3,
            tenant: 5,
            arena: ArenaId(6),
            max_map_count: Some(65530),
            ephemeral: true,
            capacity: 1 << 30,
        };
        let info = ExtentInfo {
            capacity: 1 << 30,
            used: 4096,
            available: (1 << 30) - 4096,
            next_sequence: 12,
            sequence_floor: 1 << 40,
        };
        let forked = Forked { version: 4, tenant: 5, committed: 0, ephemeral: false };
        let forked_ephemeral = Forked { version: 0, tenant: 5, committed: 7, ephemeral: true };
        hashed(opened.to_body().as_bytes());
        hashed(&opened.capacity_bytes());
        hashed(Info(info).to_body().as_bytes());
        hashed(forked.to_body().as_bytes());
        hashed(forked_ephemeral.to_body().as_bytes());
        let extent =
            MappingContents::Extent { extent: ExtentId { cell: 3, sequence: 4 }, offset: 8192 };
        hashed(&mappings_payload(&[
            WindowMapping { window_offset: 0, len: 4096, contents: extent },
            WindowMapping { window_offset: 4096, len: 8192, contents: MappingContents::Zeros },
        ]));
        let bounds = [
            MAX_PAYLOAD,
            MAX_TRANSFER_AREA,
            MAX_WINDOW,
            MAX_REQUESTS as u64,
            MAX_STREAM_BYTES,
            MAX_FDS as u64,
            MAX_BUILD as u64,
            MAX_ROOT as u64,
            MAX_STATS as u64,
            MAPPING_BYTES,
            EXTENT_ID_BYTES as u64,
            STAGE_COUNTS_EVERY.as_nanos() as u64,
            STALL.as_nanos() as u64,
            MOST_PATH,
        ];
        hashed(&bounds.iter().flat_map(|bound| bound.to_le_bytes()).collect::<Vec<_>>());
        hash
    }
}
