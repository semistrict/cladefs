// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: a SQLite database kept straight on an extent device, with no filesystem between
//! (TASK-296): a [`Database`] on a `storage_device::Device`, the interface Fxfs keeps itself on,
//! registered as a SQLite VFS ([`register`]).
//!
//! A SQLite cell holds one database, whose file is the cell's block device (`block_device`),
//! blocks in write-once extents, which a tree in the same extents names; a transaction is one
//! flush of the block device, one commit of the extent device. So a crash leaves the database as
//! of one of its transactions, with no journal to roll back, and what the device keeps of its
//! commits, forks and views of them, it keeps of the database too. This crate keeps what SQLite
//! needs besides: its VFS, its lock levels, and its journals in memory (`database`).
//!
//! A database may map its file into a window of memory ([`Database::open_mapped`], block_device's
//! `window`): reads copy from it, and SQLite's memory-mapped I/O reads pages there in place. On
//! Linux, `daemon` opens a database on a cell the host's extent daemon serves, whose window holds
//! the daemon's cache's pages themselves, one copy for every cell and VM on the host (TASK-300).

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "linux")]
pub mod daemon;
mod database;
mod locks;
mod vfs;

pub use block_device::{Contents, Failed, Options, Run, Window};
pub use database::{Database, NotFound, OneDatabase};
pub use vfs::{Registration, register};
