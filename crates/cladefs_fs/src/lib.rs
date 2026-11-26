// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host-side Fxfs filesystem, image/tar importer, and FUSE server for cladefs.

#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(test)]
#[global_allocator]
static ALLOCATOR: cladefs_core::heap::Bounded = cladefs_core::heap::Bounded;

pub mod filesystem;
pub mod fuse;
pub mod import;
pub mod node;
#[cfg(test)]
mod sparse_shrink_tests;
pub mod storage;

pub use filesystem::{Compactions, Filesystem, Node, Options, TreeCompactions, VolumeMetadata};
pub use node::{
    Attr, DirEntry, FileType, Inode, Owner, RenameFlags, SetAttr, SetTime, StatFs, XattrMode,
};
pub use storage::Storage;
