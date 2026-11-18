// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: named files kept straight on an extent device, with no filesystem between: a
//! [`FileSet`] on a `storage_device::Device`, the interface Fxfs keeps itself on. sqlite_extents
//! keeps SQLite's databases in one (TASK-296), and a cell's memory cell a VM's memory
//! (TASK-456).
//!
//! A file is pages in write-once extents, which a tree in the same extents names (`pages`), read
//! as it is used (TASK-440); a file's commit is one commit of the device, the root naming each
//! file's tree (`file_set`). So a crash leaves each file as of one of its commits, and what the
//! device keeps of its commits, forks and views of them, it keeps of the files too.
//!
//! A file set may map its files into a window of memory ([`FileSet::open_mapped`], `window`): reads
//! copy from it, and fetches read pages there in place.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

mod file_set;
mod format;
mod pages;
mod window;

pub use file_set::{Failed, FileSet, NotFound, Options};
pub use window::{Contents, FileSetWindow, Run};
