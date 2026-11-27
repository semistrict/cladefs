// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: facade crate re-exporting `cladefs_core`, `cladefs_fs`, `cladefs_wire`, and `cladefsd`.

#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(all(feature = "tests-kms", not(debug_assertions)))]
compile_error!(
    "cladefs's `tests-kms` in a release build, whose hosts would encrypt under KEKs known to all:      build a package alone (`-p PACKAGE`), not across the workspace, whose dev-dependencies turn      it on (TASK-411.28)"
);

#[cfg(target_os = "linux")]
pub use cladefs_core::stacks;
pub use cladefs_core::{
    BUILD_COMMIT, bucket, config, extent_requests, faults, host, pages, protocol, request, slabs,
    stats,
};
pub use cladefs_core::{signals, thread};
pub use cladefs_fs::import;
pub use cladefs_wire::memory_cell;

pub use cladefsd;

/// FUSE filesystem server and (on Linux) multi-cell mount control API.
pub mod fuse {
    pub use cladefs_fs::fuse::*;
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub use cladefsd::fuse_control as control;
}

/// Test utilities shared across cladefs and sqlite_cladefs tests.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    pub use cladefs_core::testing::*;
    #[cfg(not(madsim))]
    pub use cladefsd::testing_control::{Control, control_path};
}

/// A DAX window's second stage in a guest's memory that its VMM maps, Hypervisor.framework's
/// (board 13069).
#[cfg(target_os = "macos")]
pub use cladefs_core::dax::hvf::{GuestMemory, GuestRam, HvfSecondStage};
pub use cladefs_core::{
    Access, AccessFailures, CellAccess, CellHolding, CellId, CellOf, CellRole, CellSnapshot,
    CellVersion, Commit, DaxWindow, Errno, Error, ExtentId, ExtentInfo, FailingRun, Failure, Host,
    MappingContents, OpenBase, OpenCell, Opening, Private, Result, SecondStage, VersionInfo,
    VolatileSlice, VolatileSliceMut, Whose, WindowMapping, WriteBarriers,
};
#[cfg(target_os = "linux")]
pub use cladefs_core::{
    Prefault, describe_windows, guest_access_failed, guest_thread_ended, guest_thread_started,
    userfaultfd_accessible,
};
pub use cladefs_fs::{
    Attr, Compactions, DirEntry, FileType, Filesystem, Inode, Node, Options, Owner, RenameFlags,
    SetAttr, SetTime, StatFs, Storage, TreeCompactions, VolumeMetadata, XattrMode,
};
pub use cladefs_wire::{CellClient, DaemonConnection, DaemonDevice, Unmapped};
