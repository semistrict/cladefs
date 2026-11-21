// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: SQLite databases kept straight on an extent device, with no filesystem between
//! (TASK-296): a [`FileSet`] on a `storage_device::Device`, the interface Fxfs keeps itself on,
//! registered as a SQLite VFS ([`register`]).
//!
//! A database's file is a file of an extent_files file set, pages in write-once extents, which a
//! tree in the same extents names; a transaction is one commit of the device, the root naming
//! each file's tree (TASK-456.1 moved them there, to keep VMs' memory too). So a crash leaves each
//! database as of one of its transactions, with no journal to roll back, and what the device keeps
//! of its commits, forks and views of them, it keeps of the databases too. This crate keeps what
//! SQLite needs besides: its VFS, its lock levels, and its journals in memory (`file_set`).
//!
//! A file set may map its files into a window of memory ([`FileSet::open_mapped`], extent_files'
//! `window`): reads copy from it, and SQLite's memory-mapped I/O reads pages there in place. On
//! Linux, `daemon`
//! opens a file set on a cell the host's extent daemon serves, whose window holds the daemon's
//! cache's pages themselves, one copy for every cell and VM on the host (TASK-300).

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "linux")]
pub mod daemon;
mod file_set;
mod locks;
mod vfs;

pub use extent_files::{Contents, Failed, FileSetWindow, NotFound, Options, Run};
pub use file_set::FileSet;
pub use vfs::{Registration, register};
