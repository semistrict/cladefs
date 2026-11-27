// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A guest's requests, as its extent device has them: doc-4 as it is ([`Guest`]), whoever serves
//! its virtqueues: its VMM (`virtio_extent`), or the extent daemon as the device's vhost-user
//! back-end (board 14537). What the guest tells the device beside the cell, that its filesystem
//! froze and how it answered a shrink, goes to whoever keeps the device's state
//! ([`GuestNotices`]).

use crate::Errno;

use super::backend::GuestBackend;
use super::buffers::{Reader, Writer};
use super::interpret::Dialect;
use super::protocol::*;

/// What a guest tells its device beside its cell's requests, to whoever keeps the device's state:
/// a VMM's device (`virtio_extent::Notices`), or the daemon's vhost-user back-end.
pub trait GuestNotices: Sync {
    /// `FREEZE`: the guest's filesystem is frozen, if `frozen`, or thawed: a versioned snapshot of
    /// the VM is taken frozen.
    fn set_frozen(&self, frozen: bool);
    /// `RESIZED`: the guest answered the shrink to `capacity` bytes it was asked to take, taking
    /// it if `took`, before the cell hears of it ([`GuestBackend::resized`]).
    fn resize_answered(&self, capacity: u64, took: bool);
}

/// A guest's requests, as its extent device has them: doc-4 as it is, with the guest's notices of
/// its freezes and resizes, and its uploaded commits answered once ordered if `ordered_only`: a
/// device whose cache is `Unsafe` (TASK-363). A local process's ring is another dialect
/// ([`super::Process`]).
pub struct Guest<'a, N: GuestNotices> {
    pub notices: &'a N,
    /// cladefs TASK-363: a `COMMIT`'s or a `WRITE`'s commit answered once ordered, not uploaded.
    pub ordered_only: bool,
}

impl<S: GuestBackend, N: GuestNotices> Dialect<S> for Guest<'_, N> {
    // Its commits complete once uploaded or ordered, its cell kept in the bucket: no local flag,
    // nor any warming, which its MAPs ask of no one.
    const COMMIT_LOCAL: Option<u32> = None;
    const WRITE_LOCAL: Option<u32> = None;
    const MAP_WARM: Option<u32> = None;

    fn known_flags(&self, type_: u32) -> Option<u32> {
        known_flags(type_)
    }

    fn served_flags(&self, type_: u32, flags: u32) -> u32 {
        match self.ordered_only {
            false => flags,
            true => ordered_only(type_, flags),
        }
    }

    fn prefault(&self, cell: &S, window_offset: u64, len: u64) {
        cell.prefault(window_offset, len);
    }

    async fn serve_own(
        &self,
        cell: &S,
        type_: u32,
        flags: u32,
        bounds: Bounds,
        reader: &mut Reader<'_>,
        _: &Writer<'_>,
        _: usize,
    ) -> Result<usize, Errno> {
        match type_ {
            // cladefs: a window that copies on write's (backlog doc-13), its arguments UNMAP's.
            VIRTIO_EXTENT_T_PRIVATE => {
                if !cell.copies_on_write() {
                    return Err(Errno::EINVAL);
                }
                let args: UnmapArgs = reader.obj()?;
                let (window_offset, len) =
                    (u64::from_le(args.window_offset), u64::from_le(args.len));
                bounds.window(window_offset, len)?;
                cell.make_private(window_offset, len).await?;
                Ok(0)
            }
            VIRTIO_EXTENT_T_FREEZE => {
                self.notices.set_frozen(flags & VIRTIO_EXTENT_FREEZE_FROZEN != 0);
                Ok(0)
            }
            // cladefs: the guest's stats, which the daemon asked for (TASK-111.4).
            VIRTIO_EXTENT_T_STATS => {
                let args: StatsArgs = reader.obj()?;
                if reader.available() > MAX_STATS {
                    return Err(Errno::EINVAL);
                }
                let text = reader.bytes(reader.available())?;
                let text = crate::protocol::check_stats_text(&text)?;
                cell.answer_stats(u64::from_le(args.asked), Some(text)).await?;
                Ok(0)
            }
            VIRTIO_EXTENT_T_RESIZED => {
                let args: ResizedArgs = reader.obj()?;
                let capacity = u64::from_le(args.capacity);
                let took = flags & VIRTIO_EXTENT_RESIZED_TOOK != 0;
                self.notices.resize_answered(capacity, took);
                cell.resized(capacity, took).await?;
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}
