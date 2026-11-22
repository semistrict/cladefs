// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: a block device kept straight on an extent device, with no filesystem between: a
//! [`BlockDevice`] on a `storage_device::Device`, the interface Fxfs keeps itself on. It is what
//! a SQLite cell holds, its database (sqlite_cladefs, TASK-296), and a memory cell, a VM's memory
//! (TASK-456). Not a filesystem: one array of bytes, no names, no files; a cell needing those is
//! an Fxfs cell.
//!
//! The contents are blocks in write-once extents, which a tree in the same extents names
//! (`pages`), read as it is used (TASK-440); a flush is one commit of the extent device, the root
//! naming the tree (`block_device`). So a crash leaves the contents as of one of their flushes,
//! and what the extent device keeps of its commits, forks and views of them, it keeps of the
//! contents too.
//!
//! A block device may map its contents into a window of memory ([`BlockDevice::open_mapped`],
//! `window`): reads copy from it, and fetches read pages there in place.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

mod block_device;
mod format;
mod pages;
#[cfg(feature = "testing")]
pub mod testing;
mod window;

pub use block_device::{BlockDevice, Failed, Options};
pub use window::{Contents, Extents, Run, Window};
