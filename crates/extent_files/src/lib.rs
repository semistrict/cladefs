// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: named files kept straight on an extent device, with no filesystem between: a
//! [`Volume`] on a `storage_device::Device`, the interface Fxfs keeps itself on. sqlite_extents
//! keeps SQLite's databases in one (TASK-296), and a cell's memory volume a VM's memory
//! (TASK-456).
//!
//! A file is pages in write-once extents, which a tree in the same extents names (`pages`), read
//! as it is used (TASK-440); a file's commit is one commit of the device, the root naming each
//! file's tree (`volume`). So a crash leaves each file as of one of its commits, and what the
//! device keeps of its commits, forks and views of them, it keeps of the files too.
//!
//! A volume may map its files into a window of memory ([`Volume::open_mapped`], `window`): reads
//! copy from it, and fetches read pages there in place.

mod format;
mod pages;
mod volume;
mod window;

pub use volume::{Failed, NotFound, Options, Volume};
pub use window::{Contents, Run, Window};
