// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: SQLite databases kept straight on an extent device, with no filesystem between
//! (TASK-296): a [`Volume`] on a `storage_device::Device`, the interface Fxfs keeps itself on,
//! registered as a SQLite VFS ([`register`]).
//!
//! A database's file is a file of an extent_files volume, pages in write-once extents, which a
//! tree in the same extents names; a transaction is one commit of the device, the root naming
//! each file's tree (TASK-456.1 moved them there, to keep VMs' memory too). So a crash leaves each
//! database as of one of its transactions, with no journal to roll back, and what the device keeps
//! of its commits, forks and views of them, it keeps of the databases too. This crate keeps what
//! SQLite needs besides: its VFS, its lock levels, and its journals in memory (`volume`).
//!
//! A volume may map its files into a window of memory ([`Volume::open_mapped`], extent_files'
//! `window`): reads copy from it, and SQLite's memory-mapped I/O reads pages there in place. On
//! Linux, `daemon`
//! opens a volume on a store the host's extent daemon serves, whose window holds the daemon's
//! cache's pages themselves, one copy for every store and VM on the host (TASK-300).

#[cfg(target_os = "linux")]
pub mod daemon;
mod locks;
mod vfs;
mod volume;

pub use extent_files::{Contents, Failed, NotFound, Options, Run, Window};
pub use vfs::{Registration, register};
pub use volume::Volume;
