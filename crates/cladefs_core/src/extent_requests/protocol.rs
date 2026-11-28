// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The extent device's protocol, as backlog doc-4 specifies it: its features, requests, flags, the
//! structures the driver and the device exchange, and the configuration space. Its driver's half is
//! guest/linux/include/linux/virtio_extent.h, whose names these are. cladefs: a local process's
//! ring to the extent daemon carries its requests too, with flags of its own besides (board 14378).
//!
//! Every multi-byte field is little-endian: built with `to_le`, read with `from_le`.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// cladefs: the device's protocol version, the configuration's first field (TASK-141.3): a change
/// to any of its requests, flags, features, configuration or what they mean makes a new one, which
/// the driver refuses a device of another of at probe, naming both, as AGENTS.md's rule, no
/// migrations, has every format refuse another. Its features are not a negotiation: they say what
/// the device is configured as (a window, its stats asked), and a driver takes every one offered, or its
/// activation is refused ([`check_acked`]). Where it is, the configuration's first four bytes, no
/// version moves. Its driver's is `VIRTIO_EXTENT_VERSION`
/// (guest/linux/include/linux/virtio_extent.h).
/// cladefs: version 2 dropped the followers' `F_READ_ONLY`, `ADVANCE`, `LET_GO`, `PIN`, `UNPIN`
/// and the configuration's `committed` (board 8237).
pub const VIRTIO_EXTENT_VERSION: u32 = 2;

/// The device's virtio device type: `0x4658`, "FX" (backlog doc-4).
pub const VIRTIO_ID_EXTENT: u32 = 0x4658;

/// Shared memory region 0 is a DAX window, and `MAP`/`UNMAP` work. cladefs: the window maps sealed
/// extents and zeros alone, read-only (backlog doc-13): a `MAP` of an open extent fails `EPERM`. A
/// writer's copies on write: a write to a page of it gives the device a private page holding what
/// the page held, mapped writable at the same address, until a `MAP` or `UNMAP` over it; past the
/// configuration's `private_max` private pages a write is refused, and the VM stops. `MAP_ZEROS`
/// maps zeros, and `PRIVATE` makes pages private ahead of writes. Bit 1, writable DAX's, is not
/// reused.
pub const VIRTIO_EXTENT_F_DAX: u32 = 0;
/// `WRITE` takes `WRITE_COMMIT`: a commit follows the write, in one request.
pub const VIRTIO_EXTENT_F_WRITE_COMMIT: u32 = 2;
/// cladefs: the configuration holds the cell's capacity, which the host changes while the guest
/// runs, telling it with a configuration change; a shrink the guest is asked to take it answers
/// (`VIRTIO_EXTENT_T_RESIZED`).
pub const VIRTIO_EXTENT_F_RESIZE: u32 = 3;
/// cladefs: `VIRTIO_EXTENT_T_FREEZE` tells the device the guest's filesystem is frozen, or thawed:
/// a snapshot of the VM is taken frozen, and a VM restored from it, with a cell of its own, learns
/// its cell, which `INFO` says, as its filesystem thaws (TASK-39.1).
pub const VIRTIO_EXTENT_F_FREEZE: u32 = 4;
// cladefs: bit 5, a follower's `READ_ONLY` (backlog doc-8), is not reused (board 8237).
/// cladefs: `FREE` frees, with `FREE_MANY`, the extents its data names too, after the one its
/// arguments do: a flush's dead extents in one request rather than one each (TASK-41).
pub const VIRTIO_EXTENT_F_FREE_MANY: u32 = 6;
/// cladefs: `MAP` maps, with `MAP_MANY`, the maps its data holds too, each a `MapEntry`, after the
/// one its arguments say: the pieces of a file's chunk in one request rather than one each
/// (TASK-91.4).
pub const VIRTIO_EXTENT_F_MAP_MANY: u32 = 7;
/// cladefs: `MAP` takes `MAP_PREFAULT`, as each of its further maps does: the guest accesses the
/// pages next, as it maps them to read, and the device maps them for it ahead of its accesses, as
/// it does writable ones, rather than the hypervisor one fault at a time (TASK-112.1).
pub const VIRTIO_EXTENT_F_MAP_PREFAULT: u32 = 8;
// cladefs: bits 9 and 10, writable DAX's asks to seal and stage 1's copy on write beside it, are
// not reused (backlog doc-13).
/// cladefs: the host asks the guest for its stats, the configuration's `stats_asked` changing, and
/// telling it with a configuration change; the guest answers with `STATS`, its stats as text: what
/// the extent daemon's snapshots report (TASK-111.4).
pub const VIRTIO_EXTENT_F_STATS: u32 = 11;

pub const VIRTIO_EXTENT_T_READ: u32 = 1;
pub const VIRTIO_EXTENT_T_WRITE: u32 = 2;
pub const VIRTIO_EXTENT_T_FREE: u32 = 3;
pub const VIRTIO_EXTENT_T_MAP: u32 = 4;
pub const VIRTIO_EXTENT_T_UNMAP: u32 = 5;
pub const VIRTIO_EXTENT_T_COMMIT: u32 = 6;
pub const VIRTIO_EXTENT_T_ROOT: u32 = 7;
pub const VIRTIO_EXTENT_T_INFO: u32 = 8;
/// cladefs: the guest's answer to a shrink it was asked to take (`ConfigSpace::asked`): whether it
/// took it, `VIRTIO_EXTENT_RESIZED_TOOK`.
pub const VIRTIO_EXTENT_T_RESIZED: u32 = 9;
pub const VIRTIO_EXTENT_RESIZED_TOOK: u32 = 1 << 0;
/// cladefs: the guest's filesystem is frozen, with `VIRTIO_EXTENT_FREEZE_FROZEN`, nothing open and
/// nothing written until it thaws; or thawed, without.
pub const VIRTIO_EXTENT_T_FREEZE: u32 = 10;
pub const VIRTIO_EXTENT_FREEZE_FROZEN: u32 = 1 << 0;
// cladefs: 11 to 14, a follower's `ADVANCE`, `LET_GO`, `PIN` and `UNPIN`, are not reused (board
// 8237).
// cladefs: 15 was the guest's answer to an ask to seal, writable DAX's, not reused.
/// cladefs (`VIRTIO_EXTENT_F_DAX`, a writer's): makes the window's `len` bytes at `window_offset`
/// private now, as writes would, mapped for the guest ahead of its accesses: `ENOMEM`, none made
/// private, past `private_max`; `EINVAL` where nothing is mapped (`UnmapArgs` are its arguments).
pub const VIRTIO_EXTENT_T_PRIVATE: u32 = 16;
/// cladefs (`VIRTIO_EXTENT_F_STATS`): the guest's answer to the ask its `StatsArgs` name, and those
/// before it: its stats, the request's data, UTF-8 text of [`MAX_STATS`] bytes at most (`EINVAL`
/// past it), lines of a name and a value (TASK-111.4).
pub const VIRTIO_EXTENT_T_STATS: u32 = 17;
/// The last request type.
pub const VIRTIO_EXTENT_T_LAST: u32 = VIRTIO_EXTENT_T_STATS;

/// A `WRITE`'s flags: a barrier precedes it, as if a `COMMIT` did; a `COMMIT` follows it (with
/// `WRITE_COMMIT`); that commit completes once uploaded; cladefs: that commit sets the root, which
/// follows the data, of `IoArgs::root_len` bytes.
pub const VIRTIO_EXTENT_WRITE_BARRIER: u32 = crate::protocol::WRITE_BARRIER;
pub const VIRTIO_EXTENT_WRITE_COMMIT: u32 = crate::protocol::WRITE_COMMIT;
pub const VIRTIO_EXTENT_WRITE_UPLOADED: u32 = crate::protocol::WRITE_UPLOADED;
pub const VIRTIO_EXTENT_WRITE_ROOT: u32 = crate::protocol::WRITE_ROOT;

/// cladefs: a `FREE`'s flags: its data names more extents to free, each a `virtio_extent_id`
/// (`F_FREE_MANY`), of [`MAX_MANY_DATA`] bytes at most.
pub const VIRTIO_EXTENT_FREE_MANY: u32 = 1 << 0;

/// A `COMMIT`'s flags: it completes once uploaded; it sets the root.
pub const VIRTIO_EXTENT_COMMIT_UPLOADED: u32 = crate::protocol::COMMIT_UPLOADED;
pub const VIRTIO_EXTENT_COMMIT_ROOT: u32 = crate::protocol::COMMIT_ROOT;
/// A `MAP`'s flags, cladefs: its data holds more maps (`F_MAP_MANY`), of [`MAX_MANY_DATA`] bytes at
/// most; and the guest accesses the pages next (`F_MAP_PREFAULT`); and zeros, its extent and offset
/// ignored, of a writer's window, as each of a `MAP_MANY`'s maps may be. Every map is read-only:
/// bit 0, writable DAX's `MAP_WRITE`, is not reused.
pub const VIRTIO_EXTENT_MAP_MANY: u32 = 1 << 1;
pub const VIRTIO_EXTENT_MAP_PREFAULT: u32 = 1 << 2;
pub const VIRTIO_EXTENT_MAP_ZEROS: u32 = crate::protocol::MAP_ZEROS;

/// cladefs (board 14378): the flags of a local process's requests alone, on its ring to the extent
/// daemon (`Process`), which a guest's device refuses as it knows none of them: a `COMMIT` that
/// completes once local, synced to the host's disk, as a database's on a local filesystem do
/// (TASK-301); a `WRITE` whose commit does; and a `MAP` that warms each mapped range in the
/// daemon's extent cache first (board 11681). Each at a bit its type's doc-4 flags leave free.
pub const EXTENT_RING_COMMIT_LOCAL: u32 = 1 << 2;
pub const EXTENT_RING_WRITE_LOCAL: u32 = 1 << 4;
pub const EXTENT_RING_MAP_WARM: u32 = 1 << 4;
const _: () = {
    assert!(EXTENT_RING_COMMIT_LOCAL & crate::protocol::COMMIT_SHARED_FLAGS == 0);
    assert!(EXTENT_RING_WRITE_LOCAL & crate::protocol::WRITE_SHARED_FLAGS == 0);
    assert!(
        EXTENT_RING_MAP_WARM
            & (VIRTIO_EXTENT_MAP_MANY | VIRTIO_EXTENT_MAP_PREFAULT | VIRTIO_EXTENT_MAP_ZEROS | 1)
            == 0
    );
};

/// The size of each of the device's queues. Its request queues are interchangeable, which a driver
/// may use one per CPU of (cladefs TASK-26.18.5): a VMM gives the device as many as its VM has
/// vCPUs, each with an interrupt of its own, so that no two of the guest's CPUs share one
/// (TASK-276).
pub const QUEUE_SIZE: u16 = 256;

/// The ID of the DAX window's shared memory region.
pub const DAX_WINDOW_SHM_ID: u8 = 0;

/// The DAX window's sizes, and guest addresses, are multiples of this: huge pages'.
pub const DAX_WINDOW_ALIGNMENT: u64 = 2 << 20;

/// The most bytes the configuration's tag holds.
pub const MAX_TAG_LEN: usize = 36;

/// The most bytes one `READ` or `WRITE` moves.
pub const MAX_TRANSFER: u32 = crate::protocol::MAX_TRANSFER;
/// The most bytes the root holds.
pub const MAX_ROOT: u32 = crate::protocol::MAX_ROOT as u32;
/// The most bytes one extent holds.
pub const MAX_EXTENT: u64 = crate::protocol::MAX_EXTENT;
/// cladefs: the most bytes of data a `FREE_MANY` or a `MAP_MANY` carries, its further extents or
/// maps: a page of the largest a guest's kernel has, as its drivers send a page's worth at most;
/// past it, the request is refused (`EINVAL`). The device lists what the data names in its own
/// memory, and a chain's descriptors, naming the same guest memory again and again, can name far
/// more than the guest has: unbounded, one request grew the VMM by what they named.
pub const MAX_MANY_DATA: usize = crate::protocol::MAX_MANY_DATA;
/// cladefs: the most bytes of text a `STATS` carries (TASK-111.4): the extent daemon's.
pub const MAX_STATS: usize = crate::protocol::MAX_STATS;

/// cladefs TASK-470: the ranges a request's offsets and lengths keep to (doc-4), as the device's
/// configuration and window say: a request out of them is refused, `EINVAL`, before its cell sees
/// it, whatever the cell would make of it (a fake's arithmetic overflowed, a real cell's checks
/// are its own). An end past the end of a u64 is past every bound, an aligned one wrapping to 0
/// too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// `block_size`: what a `READ`'s or `WRITE`'s offset and length are multiples of.
    pub block_size: u64,
    /// `max_extent`: where a `READ`'s or `WRITE`'s range in its extent ends at most.
    pub max_extent: u64,
    /// `map_alignment`: what a `MAP`'s, `UNMAP`'s or `PRIVATE`'s offsets and length are multiples
    /// of.
    pub map_alignment: u64,
    /// The DAX window's length: where a `MAP`'s, `UNMAP`'s or `PRIVATE`'s range in it ends at
    /// most; 0 without one.
    pub window: u64,
}

impl Bounds {
    /// The bounds of a device whose configuration space is `config`, and whose DAX window is
    /// `window` bytes long, 0 without one.
    pub fn of(config: &ConfigSpace, window: u64) -> Self {
        Self {
            block_size: u64::from(u32::from_le(config.block_size)),
            max_extent: u64::from_le(config.max_extent),
            map_alignment: u64::from(u32::from_le(config.map_alignment)),
            window,
        }
    }

    /// A `READ`'s or `WRITE`'s `len` bytes at `offset` in its extent: multiples of `block_size`,
    /// ending within `max_extent`.
    pub fn io(&self, offset: u64, len: u64) -> Result<(), crate::Errno> {
        crate::protocol::check_range_within(offset, len, self.block_size, self.max_extent)
    }

    /// A `MAP`'s `len` bytes at `offset` in its extent: multiples of `map_alignment`, ending
    /// within a u64. What lies past the extent, `max_extent` included, maps zeros.
    pub fn mapped(&self, offset: u64, len: u64) -> Result<(), crate::Errno> {
        crate::protocol::check_range_within(offset, len, self.map_alignment, u64::MAX)
    }

    /// A `MAP`'s, `UNMAP`'s or `PRIVATE`'s `len` bytes of the window at `window_offset`: multiples
    /// of `map_alignment`, ending within the window.
    pub fn window(&self, window_offset: u64, len: u64) -> Result<(), crate::Errno> {
        crate::protocol::check_range_within(window_offset, len, self.map_alignment, self.window)
    }
}

/// cladefs: a request's `flags`, of type `type_`, as a device whose cache is `Unsafe` serves them
/// (TASK-363): a `COMMIT` or a `WRITE`'s commit ordered, not uploaded.
/// cladefs: the flags requests of `type_` may set (doc-4): `None` for a type no request is of. The
/// device refuses a request setting any other (`EINVAL`) before anything of it is served, in one
/// place for every type, so that a type added later is checked as the others are (board 2774).
pub fn known_flags(type_: u32) -> Option<u32> {
    Some(match type_ {
        VIRTIO_EXTENT_T_WRITE => {
            VIRTIO_EXTENT_WRITE_BARRIER
                | VIRTIO_EXTENT_WRITE_COMMIT
                | VIRTIO_EXTENT_WRITE_UPLOADED
                | VIRTIO_EXTENT_WRITE_ROOT
        }
        VIRTIO_EXTENT_T_FREE => VIRTIO_EXTENT_FREE_MANY,
        VIRTIO_EXTENT_T_MAP => {
            VIRTIO_EXTENT_MAP_MANY | VIRTIO_EXTENT_MAP_PREFAULT | VIRTIO_EXTENT_MAP_ZEROS
        }
        VIRTIO_EXTENT_T_COMMIT => VIRTIO_EXTENT_COMMIT_UPLOADED | VIRTIO_EXTENT_COMMIT_ROOT,
        VIRTIO_EXTENT_T_RESIZED => VIRTIO_EXTENT_RESIZED_TOOK,
        VIRTIO_EXTENT_T_FREEZE => VIRTIO_EXTENT_FREEZE_FROZEN,
        VIRTIO_EXTENT_T_READ
        | VIRTIO_EXTENT_T_UNMAP
        | VIRTIO_EXTENT_T_ROOT
        | VIRTIO_EXTENT_T_INFO
        | VIRTIO_EXTENT_T_PRIVATE
        | VIRTIO_EXTENT_T_STATS => 0,
        _ => return None,
    })
}

/// cladefs: the most bytes a request's writable buffers hold: the most it returns, a `READ`'s, and
/// its response (board 2768). The device refuses a request with more, and never writes, nor zeroes,
/// past it: what a request costs the device is bounded by the protocol, not by the `u32` a chain's
/// length fits in.
pub const MAX_WRITABLE: usize = MAX_TRANSFER as usize + size_of::<Response>();

pub fn ordered_only(type_: u32, flags: u32) -> u32 {
    match type_ {
        VIRTIO_EXTENT_T_COMMIT => flags & !VIRTIO_EXTENT_COMMIT_UPLOADED,
        VIRTIO_EXTENT_T_WRITE => flags & !VIRTIO_EXTENT_WRITE_UPLOADED,
        _ => flags,
    }
}

/// The barriers a `WRITE`'s `flags` order it with, unless they are not a `WRITE`'s; the root that
/// `WRITE_ROOT` sets the caller reads, after the data.
pub fn write_barriers(flags: u32) -> Option<crate::WriteBarriers<'static>> {
    // Refused flags, a WRITE's no barriers say: None, what the caller refuses the request with.
    crate::protocol::decode_write_barriers(flags, None).ok().map(|(barriers, _)| barriers)
}

/// A driver took `acked` of the features the device offered, `offered`, with all of them: as a
/// driver of the device's protocol does (TASK-141.3).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "the driver took features {acked:#x} of the {offered:#x} offered, where a driver of protocol \
     version {VIRTIO_EXTENT_VERSION} takes every one: it is of another"
)]
pub struct FeaturesRefused {
    pub offered: u64,
    pub acked: u64,
}

/// cladefs: `Ok` if a driver that took `acked` of the features the device offers, `offered`, took
/// every one, as a driver of the device's protocol does (TASK-141.3): its features are no
/// negotiation, but what the device is configured as, which a driver serving it otherwise, as it
/// would one of another protocol's, would serve wrong. Refused, naming both. `offered` is every
/// feature the device offers, its transport's (`VIRTIO_F_VERSION_1`, `VIRTIO_RING_F_EVENT_IDX`)
/// among them.
pub fn check_acked(offered: u64, acked: u64) -> Result<(), FeaturesRefused> {
    if acked == offered {
        return Ok(());
    }
    Err(FeaturesRefused { offered, acked })
}

/// A snapshot's device is of another protocol version than this build's.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "The snapshot's extent device is of protocol version {0}, this VMM's {VIRTIO_EXTENT_VERSION}: \
     its guest's driver is of another"
)]
pub struct VersionRefused(pub u32);

/// cladefs: `Ok` if a snapshot's device, of protocol version `protocol`, is of this build's
/// (TASK-141.3): its guest's driver, which checked the device's version at probe, goes on as of
/// that one. A VMM refuses to restore a device whose version its snapshot doesn't say is this.
pub fn check_version(protocol: u32) -> Result<(), VersionRefused> {
    match protocol {
        VIRTIO_EXTENT_VERSION => Ok(()),
        other => Err(VersionRefused(other)),
    }
}

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C)]
pub struct ExtentIdLe {
    pub cell: u64,
    pub sequence: u64,
}

/// Every request's header.
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct RequestHeader {
    pub type_: u32,
    pub flags: u32,
    /// The driver's own, for tracing.
    pub unique: u64,
}

/// Every request's response, the last bytes the device writes.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C)]
pub struct Response {
    /// 0, or an errno.
    pub status: u32,
    pub reserved: u32,
}

/// `READ`'s and `WRITE`'s arguments.
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct IoArgs {
    pub id: ExtentIdLe,
    pub offset: u64,
    pub len: u32,
    /// cladefs: a `WRITE`'s with `WRITE_ROOT`: the root's bytes, after the data.
    pub root_len: u32,
}

/// `MAP`'s arguments.
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct MapArgs {
    pub id: ExtentIdLe,
    pub offset: u64,
    pub len: u64,
    pub window_offset: u64,
}

/// cladefs: one of a `MAP_MANY`'s further maps, its flags `MAP_PREFAULT` and `MAP_ZEROS` alone
/// (`F_MAP_MANY`).
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct MapEntry {
    pub map: MapArgs,
    pub flags: u32,
    pub reserved: u32,
}

/// `UNMAP`'s arguments.
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct UnmapArgs {
    pub window_offset: u64,
    pub len: u64,
}

/// `COMMIT`'s arguments.
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct CommitArgs {
    pub root_len: u32,
    pub reserved: u32,
}

/// What follows the root `ROOT` writes.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C)]
pub struct RootLen {
    pub len: u32,
    pub reserved: u32,
}

/// cladefs: `STATS`'s arguments: the ask it answers (`ConfigSpace::stats_asked`).
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C)]
pub struct StatsArgs {
    pub asked: u64,
}

/// What `INFO` writes.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C)]
pub struct Info {
    pub capacity: u64,
    pub used: u64,
    pub available: u64,
    pub next_sequence: u64,
    /// cladefs: the lowest sequence number the guest may mint from.
    pub sequence_floor: u64,
    /// cladefs: the cell the guest mints extents of.
    pub cell: u64,
}

/// `VIRTIO_EXTENT_T_RESIZED`'s arguments: the capacity the guest was asked to shrink to.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C, packed)]
pub struct ResizedArgs {
    pub capacity: u64,
}

/// The device's configuration space.
#[derive(Copy, Clone, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct ConfigSpace {
    /// cladefs: the device's protocol version, [`VIRTIO_EXTENT_VERSION`], first, where every
    /// version has it (TASK-141.3).
    pub version: u32,
    /// The filesystem's name, NUL-padded: the mount's source.
    pub tag: [u8; MAX_TAG_LEN],
    pub num_request_queues: u32,
    pub block_size: u32,
    pub map_alignment: u32,
    pub max_transfer: u32,
    pub max_segments: u32,
    pub max_root: u32,
    pub cell: u64,
    pub max_extent: u64,
    /// cladefs: the cell's capacity, and the shrink the guest is asked to take, if any, 0 if none
    /// (`VIRTIO_EXTENT_F_RESIZE`).
    pub capacity: u64,
    pub asked: u64,
    /// cladefs: the most private pages the window holds (`VIRTIO_EXTENT_F_DAX`, backlog doc-13).
    pub private_max: u64,
    /// cladefs: the latest of the extent daemon's asks for the guest's stats, 0 before the first
    /// (`VIRTIO_EXTENT_F_STATS`, TASK-111.4).
    pub stats_asked: u64,
}

impl ConfigSpace {
    /// The `data.len()` bytes of the configuration at `offset`, those past its end zeros, as a
    /// driver reads them.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        let config = self.as_bytes();
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if let Some(config_from_offset) = config.get(offset..) {
            let len = config_from_offset.len().min(data.len());
            data[..len].copy_from_slice(&config_from_offset[..len]);
            data[len..].fill(0);
        } else {
            data.fill(0);
        }
    }
}

impl ExtentIdLe {
    /// Decodes a guest-supplied extent ID at the VMM trust boundary, refusing `sequence ==
    /// u64::MAX` (`EINVAL`, board 2762).
    pub fn decode(self) -> Result<crate::ExtentId, crate::Errno> {
        crate::protocol::check_extent_id(u64::from_le(self.cell), u64::from_le(self.sequence))
    }
}

impl From<ExtentIdLe> for crate::ExtentId {
    fn from(id: ExtentIdLe) -> Self {
        Self { cell: u64::from_le(id.cell), sequence: u64::from_le(id.sequence) }
    }
}

#[cfg(test)]
mod tests {
    use zerocopy::FromZeros;

    use super::*;

    /// cladefs TASK-363: an Unsafe device's commits and writes' commits are ordered, not uploaded,
    /// and nothing else of their flags, nor any other request's, changes.
    #[test]
    fn unsafe_requests_are_ordered_only() {
        let commit = VIRTIO_EXTENT_COMMIT_UPLOADED | VIRTIO_EXTENT_COMMIT_ROOT;
        assert_eq!(ordered_only(VIRTIO_EXTENT_T_COMMIT, commit), VIRTIO_EXTENT_COMMIT_ROOT);
        let write =
            VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_UPLOADED | VIRTIO_EXTENT_WRITE_ROOT;
        assert_eq!(
            ordered_only(VIRTIO_EXTENT_T_WRITE, write),
            VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_ROOT
        );
        assert_eq!(ordered_only(VIRTIO_EXTENT_T_READ, u32::MAX), u32::MAX);
    }

    /// A WRITE's flags: an upload and a root only with a commit, nothing unknown.
    #[test]
    fn writes_barriers() {
        let barriers = write_barriers(VIRTIO_EXTENT_WRITE_BARRIER).unwrap();
        assert!(barriers.before);
        assert_eq!(barriers.after, None);
        let barriers = write_barriers(VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_UPLOADED);
        assert_eq!(barriers.unwrap().after, Some(crate::Commit::Uploaded));
        let barriers = write_barriers(VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_ROOT);
        assert_eq!(barriers.unwrap().after, Some(crate::Commit::Ordered));
        assert!(write_barriers(VIRTIO_EXTENT_WRITE_UPLOADED).is_none());
        assert!(write_barriers(VIRTIO_EXTENT_WRITE_ROOT).is_none());
        assert!(write_barriers(1 << 4).is_none());
    }

    /// cladefs TASK-141.3: the configuration says the device's protocol version first, where a
    /// driver of every version reads it.
    #[test]
    fn the_configuration_says_its_version_first() {
        assert_eq!(std::mem::offset_of!(ConfigSpace, version), 0);
        assert_eq!(std::mem::offset_of!(ConfigSpace, tag), 4);
        assert_eq!(std::mem::size_of::<ConfigSpace>(), 112);
    }

    /// The wire structures' sizes, as the driver's header has them.
    #[test]
    fn structures_are_the_drivers_sizes() {
        assert_eq!(size_of::<RequestHeader>(), 16);
        assert_eq!(size_of::<Response>(), 8);
        assert_eq!(size_of::<IoArgs>(), 32);
        assert_eq!(size_of::<MapArgs>(), 40);
        assert_eq!(size_of::<MapEntry>(), 48);
        assert_eq!(size_of::<UnmapArgs>(), 16);
        assert_eq!(size_of::<Info>(), 48);
        assert_eq!(size_of::<ResizedArgs>(), 8);
    }

    /// cladefs TASK-141.3: a driver activating the device having taken fewer features than it
    /// offers, or others, is refused, naming them; one taking every one isn't. Before, it was
    /// served, as negotiation has it, its INFOs short of the cell without F_FREEZE.
    #[test]
    fn drivers_take_every_feature_offered() {
        let offered = (1u64 << 32)
            | (1u64 << 29)
            | (1 << VIRTIO_EXTENT_F_FREEZE)
            | (1 << VIRTIO_EXTENT_F_DAX);
        check_acked(offered, offered).unwrap();
        let fewer = offered & !(1 << VIRTIO_EXTENT_F_FREEZE);
        assert_eq!(
            check_acked(offered, fewer).unwrap_err().to_string(),
            format!(
                "the driver took features 0x120000001 of the 0x120000011 offered, where a driver \
                 of protocol version {VIRTIO_EXTENT_VERSION} takes every one: it is of another"
            )
        );
        // One the device didn't offer: F_MAP_MANY, which this offer leaves out.
        assert!(check_acked(offered, offered | (1 << VIRTIO_EXTENT_F_MAP_MANY)).is_err());
    }

    /// cladefs TASK-141.3: a snapshot's device of another protocol version is refused, naming both.
    #[test]
    fn snapshots_of_other_protocols_are_refused() {
        check_version(VIRTIO_EXTENT_VERSION).unwrap();
        let other = VIRTIO_EXTENT_VERSION + 1;
        assert_eq!(
            check_version(other).unwrap_err().to_string(),
            format!(
                "The snapshot's extent device is of protocol version {other}, this VMM's \
                 {VIRTIO_EXTENT_VERSION}: its guest's driver is of another"
            )
        );
    }

    /// A driver reads the configuration at any offset and length: what is past its end reads as
    /// zeros.
    #[test]
    fn configurations_read_past_their_end_as_zeros() {
        let mut config = ConfigSpace::new_zeroed();
        config.version = VIRTIO_EXTENT_VERSION.to_le();
        config.stats_asked = 0x1122_3344_5566_7788u64.to_le();
        let mut data = [0xa5; 4];
        config.read(0, &mut data);
        assert_eq!(data, VIRTIO_EXTENT_VERSION.to_le_bytes());
        let mut data = [0xa5; 12];
        config.read(104, &mut data);
        assert_eq!(data[..8], 0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(data[8..], [0; 4]);
        let mut data = [0xa5; 4];
        config.read(u64::MAX, &mut data);
        assert_eq!(data, [0; 4]);
    }
}
