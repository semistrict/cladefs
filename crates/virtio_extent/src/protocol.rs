// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's protocol, as backlog doc-4 specifies it: its features, requests, flags, the
//! structures the driver and the device exchange, and the configuration space. Its driver's half is
//! guest/linux/include/linux/virtio_extent.h, whose names these are.
//!
//! Every multi-byte field is little-endian: built with `to_le`, read with `from_le`.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// cladefs: the device's protocol version, the configuration's first field (TASK-141.3): a change to
/// any of its requests, flags, features, configuration or what they mean makes a new one, which
/// the driver refuses a device of another of at probe, naming both, as AGENTS.md's rule, no
/// migrations, has every format refuse another. Its features are not a negotiation: they say
/// what the device is configured as (a follower, a window), and a driver takes every one offered,
/// or its activation is refused ([`check_acked`]). Where it is, the configuration's first four
/// bytes, no version moves. Its driver's is `VIRTIO_EXTENT_VERSION`
/// (guest/linux/include/linux/virtio_extent.h).
pub const VIRTIO_EXTENT_VERSION: u32 = 1;

/// The device's virtio device type: `0x4658`, "FX" (backlog doc-4).
pub const VIRTIO_ID_EXTENT: u32 = 0x4658;

/// Shared memory region 0 is a DAX window, and `MAP`/`UNMAP` work. cladefs: the window maps sealed
/// extents and zeros alone, read-only (backlog doc-13): a `MAP` of an open extent fails `EPERM`. A
/// writer's copies on write: a store to a page of it gives the device a private page holding what
/// the page held, mapped writable at the same address, until a `MAP` or `UNMAP` over it; past the
/// configuration's `private_max` private pages a store is refused, and the VM stops. `MAP_ZEROS`
/// maps zeros, and `PRIVATE` makes pages private ahead of stores. A follower's (`READ_ONLY`) maps
/// no zeros, and refuses every store. Bit 1, writable DAX's, is not reused.
pub const VIRTIO_EXTENT_F_DAX: u32 = 0;
/// `WRITE` takes `WRITE_COMMIT`: a commit follows the write, in one request.
pub const VIRTIO_EXTENT_F_WRITE_COMMIT: u32 = 2;
/// cladefs: the configuration holds the store's capacity, which the host changes while the guest
/// runs, telling it with a configuration change; a shrink the guest is asked to take it answers
/// (`VIRTIO_EXTENT_T_RESIZED`).
pub const VIRTIO_EXTENT_F_RESIZE: u32 = 3;
/// cladefs: `VIRTIO_EXTENT_T_FREEZE` tells the device the guest's filesystem is frozen, or thawed: a
/// snapshot of the VM is taken frozen, and a VM restored from it, with a store of its own, learns
/// its branch, which `INFO` says, as its filesystem thaws (TASK-39.1).
pub const VIRTIO_EXTENT_F_FREEZE: u32 = 4;
/// cladefs: the device follows a store another VM writes, read-only (backlog doc-8): `ADVANCE`,
/// `LET_GO`, `PIN` and `UNPIN` work, and the configuration's `committed` changes as the writer
/// commits, telling the guest with a configuration change (TASK-42.3).
pub const VIRTIO_EXTENT_F_READ_ONLY: u32 = 5;
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
/// cladefs (`VIRTIO_EXTENT_F_READ_ONLY`): moves the follower's view to its writer's latest commit,
/// holding what the commits between held until `LET_GO`, and writes its number (`CommitNumber`).
pub const VIRTIO_EXTENT_T_ADVANCE: u32 = 11;
/// cladefs: lets go of what the follower's view held of the commits before the one a `CommitNumber`
/// names, which an `ADVANCE` wrote: served after the next `ADVANCE`, it lets go of no more.
pub const VIRTIO_EXTENT_T_LET_GO: u32 = 12;
/// cladefs: keeps an extent the view holds, whatever commit it advances to, until `UNPIN`.
pub const VIRTIO_EXTENT_T_PIN: u32 = 13;
pub const VIRTIO_EXTENT_T_UNPIN: u32 = 14;
// cladefs: 15 was the guest's answer to an ask to seal, writable DAX's, not reused.
/// cladefs (`VIRTIO_EXTENT_F_DAX`, a writer's): makes the window's `len` bytes at `window_offset`
/// private now, as stores would, mapped for the guest ahead of its accesses: `ENOMEM`, none made
/// private, past `private_max`; `EINVAL` where nothing is mapped, or of a follower's window
/// (`UnmapArgs` are its arguments).
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
pub const VIRTIO_EXTENT_WRITE_BARRIER: u32 = 1 << 0;
pub const VIRTIO_EXTENT_WRITE_COMMIT: u32 = 1 << 1;
pub const VIRTIO_EXTENT_WRITE_UPLOADED: u32 = 1 << 2;
pub const VIRTIO_EXTENT_WRITE_ROOT: u32 = 1 << 3;

/// cladefs: a `FREE`'s flags: its data names more extents to free, each a `virtio_extent_id`
/// (`F_FREE_MANY`), of [`MAX_MANY_DATA`] bytes at most.
pub const VIRTIO_EXTENT_FREE_MANY: u32 = 1 << 0;

/// A `COMMIT`'s flags: it completes once uploaded; it sets the root.
pub const VIRTIO_EXTENT_COMMIT_UPLOADED: u32 = 1 << 0;
pub const VIRTIO_EXTENT_COMMIT_ROOT: u32 = 1 << 1;
/// A `MAP`'s flags, cladefs: its data holds more maps (`F_MAP_MANY`), of [`MAX_MANY_DATA`] bytes at
/// most; and the guest accesses the pages next (`F_MAP_PREFAULT`); and zeros, its extent and offset
/// ignored, of a writer's window, as each of a `MAP_MANY`'s maps may be. Every map is read-only:
/// bit 0, writable DAX's `MAP_WRITE`, is not reused.
pub const VIRTIO_EXTENT_MAP_MANY: u32 = 1 << 1;
pub const VIRTIO_EXTENT_MAP_PREFAULT: u32 = 1 << 2;
pub const VIRTIO_EXTENT_MAP_ZEROS: u32 = 1 << 3;

/// The size of each of the device's queues. Its request queues are interchangeable, which a driver
/// may use one per CPU of (cladefs TASK-26.18.5): a VMM gives the device as many as its VM has vCPUs,
/// each with an interrupt of its own, so that no two of the guest's CPUs share one (TASK-276).
pub const QUEUE_SIZE: u16 = 256;

/// The ID of the DAX window's shared memory region.
pub const DAX_WINDOW_SHM_ID: u8 = 0;

/// The DAX window's sizes, and guest addresses, are multiples of this: huge pages'.
pub const DAX_WINDOW_ALIGNMENT: u64 = 2 << 20;

/// The most bytes the configuration's tag holds.
pub const MAX_TAG_LEN: usize = 36;

/// The most bytes one `READ` or `WRITE` moves.
pub const MAX_TRANSFER: u32 = 8 << 20;
/// The most bytes the root holds.
pub const MAX_ROOT: u32 = 4096;
/// The most bytes one extent holds.
pub const MAX_EXTENT: u64 = cladefs::ExtentStore::MAX_EXTENT;
/// cladefs: the most bytes of data a `FREE_MANY` or a `MAP_MANY` carries, its further extents or
/// maps: a page of the largest a guest's kernel has, as its drivers send a page's worth at most;
/// past it, the request is refused (`EINVAL`). The device lists what the data names in its own
/// memory, and a chain's descriptors, naming the same guest memory again and again, can name far
/// more than the guest has: unbounded, one request grew the VMM by what they named.
pub const MAX_MANY_DATA: usize = 64 << 10;
/// cladefs: the most bytes of text a `STATS` carries (TASK-111.4): the extent daemon's.
pub const MAX_STATS: usize = 64 << 10;

#[cfg(target_os = "linux")]
const _: () = assert!(MAX_STATS == cladefs::RemoteExtentStore::MAX_STATS);

/// cladefs: a request's `flags`, of type `type_`, as a device whose cache is `Unsafe` serves them
/// (TASK-363): a `COMMIT` or a `WRITE`'s commit ordered, not uploaded.
pub fn ordered_only(type_: u32, flags: u32) -> u32 {
    match type_ {
        VIRTIO_EXTENT_T_COMMIT => flags & !VIRTIO_EXTENT_COMMIT_UPLOADED,
        VIRTIO_EXTENT_T_WRITE => flags & !VIRTIO_EXTENT_WRITE_UPLOADED,
        _ => flags,
    }
}

/// The barriers a `WRITE`'s `flags` order it with, unless they are not a `WRITE`'s; the root that
/// `WRITE_ROOT` sets the caller reads, after the data.
pub fn write_barriers(flags: u32) -> Option<cladefs::WriteBarriers<'static>> {
    let known = VIRTIO_EXTENT_WRITE_BARRIER
        | VIRTIO_EXTENT_WRITE_COMMIT
        | VIRTIO_EXTENT_WRITE_UPLOADED
        | VIRTIO_EXTENT_WRITE_ROOT;
    let commit = flags & VIRTIO_EXTENT_WRITE_COMMIT != 0;
    if flags & !known != 0
        || (flags & (VIRTIO_EXTENT_WRITE_UPLOADED | VIRTIO_EXTENT_WRITE_ROOT) != 0 && !commit)
    {
        return None;
    }
    let after = commit.then_some(if flags & VIRTIO_EXTENT_WRITE_UPLOADED != 0 {
        cladefs::Commit::Uploaded
    } else {
        cladefs::Commit::Ordered
    });
    Some(cladefs::WriteBarriers {
        before: flags & VIRTIO_EXTENT_WRITE_BARRIER != 0,
        after,
        root: None,
    })
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
    pub branch: u64,
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
    /// cladefs: the branch the guest mints extents of.
    pub branch: u64,
}

/// What `ADVANCE` writes, the number of the commit the follower's view is as of now, and `LET_GO`
/// reads.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C)]
pub struct CommitNumber {
    pub commit: u64,
    pub reserved: u64,
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
    /// cladefs: the device's protocol version, [`VIRTIO_EXTENT_VERSION`], first, where every version
    /// has it (TASK-141.3).
    pub version: u32,
    /// The filesystem's name, NUL-padded: the mount's source.
    pub tag: [u8; MAX_TAG_LEN],
    pub num_request_queues: u32,
    pub block_size: u32,
    pub map_alignment: u32,
    pub max_transfer: u32,
    pub max_segments: u32,
    pub max_root: u32,
    pub branch: u64,
    pub max_extent: u64,
    /// cladefs: the store's capacity, and the shrink the guest is asked to take, if any, 0 if none
    /// (`VIRTIO_EXTENT_F_RESIZE`).
    pub capacity: u64,
    pub asked: u64,
    /// cladefs: the number of the latest commit of a follower's writer
    /// (`VIRTIO_EXTENT_F_READ_ONLY`).
    pub committed: u64,
    /// cladefs: the most private pages a writer's window holds (`VIRTIO_EXTENT_F_DAX`, backlog
    /// doc-13): 0 of a follower's, which copies nothing.
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

impl From<ExtentIdLe> for cladefs::ExtentId {
    fn from(id: ExtentIdLe) -> Self {
        Self { branch: u64::from_le(id.branch), sequence: u64::from_le(id.sequence) }
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
        assert_eq!(barriers.unwrap().after, Some(cladefs::Commit::Uploaded));
        let barriers = write_barriers(VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_ROOT);
        assert_eq!(barriers.unwrap().after, Some(cladefs::Commit::Ordered));
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
        assert_eq!(std::mem::size_of::<ConfigSpace>(), 120);
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
        assert_eq!(size_of::<CommitNumber>(), 16);
        assert_eq!(size_of::<ResizedArgs>(), 8);
    }

    /// cladefs TASK-141.3: a driver activating the device having taken fewer features than it
    /// offers, or others, is refused, naming them; one taking every one isn't. Before, it was
    /// served, as negotiation has it, its INFOs short of the branch without F_FREEZE.
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
            "the driver took features 0x120000001 of the 0x120000011 offered, where a driver of \
             protocol version 1 takes every one: it is of another"
        );
        assert!(check_acked(offered, offered | (1 << VIRTIO_EXTENT_F_READ_ONLY)).is_err());
    }

    /// cladefs TASK-141.3: a snapshot's device of another protocol version is refused, naming both.
    #[test]
    fn snapshots_of_other_protocols_are_refused() {
        check_version(VIRTIO_EXTENT_VERSION).unwrap();
        assert_eq!(
            check_version(VIRTIO_EXTENT_VERSION + 1).unwrap_err().to_string(),
            "The snapshot's extent device is of protocol version 2, this VMM's 1: its guest's \
             driver is of another"
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
        config.read(112, &mut data);
        assert_eq!(data[..8], 0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(data[8..], [0; 4]);
        let mut data = [0xa5; 4];
        config.read(u64::MAX, &mut data);
        assert_eq!(data, [0; 4]);
    }
}
