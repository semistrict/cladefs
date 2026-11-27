// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Core host-side cell runtime for cladefs: open cells, extent cache, DAX windows, slabs,
//! arenas, buckets, and host state.

#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(all(feature = "tests-kms", not(debug_assertions)))]
compile_error!(
    "cladefs_core's `tests-kms` in a release build, whose hosts would encrypt under KEKs known      to all (TASK-411.28)"
);

#[cfg(test)]
#[global_allocator]
static ALLOCATOR: heap::Bounded = heap::Bounded;

pub mod bucket;
pub mod cache;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod caller;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod cell_dir;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod cell_table;
pub mod collect;
pub mod config;
pub mod dax;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod ephemeral_host;
pub mod error;
pub mod evidence;
pub mod extent_requests;
pub mod faults;
pub mod halt;
#[cfg(any(test, feature = "testing"))]
pub mod heap;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod hearing;
pub mod host;
pub mod host_cell;
pub mod memory;
pub mod open_cell;
pub mod pages;
pub mod protocol;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod request;
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_testing;
pub mod signals;
pub mod slabs;
#[cfg(target_os = "linux")]
pub mod stacks;
pub mod stats;
pub mod sync;
#[cfg(test)]
pub mod test_checkpoints;
#[cfg(any(test, feature = "testing"))]
pub mod test_data;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod thread;

pub use bucket_cell::layout::CellId;
#[cfg(target_os = "linux")]
pub use dax::Prefault;
pub use dax::failures::{AccessFailures, FailingRun, Failure};
#[cfg(target_os = "linux")]
pub use dax::uffd::{
    describe_windows, guest_access_failed, guest_thread_ended, guest_thread_started,
    userfaultfd_accessible,
};
pub use dax::{Access, DaxWindow, Private, SecondStage};
pub use error::{Errno, Error, Result};
pub use host::Host;
pub use host_cell::CellOf;
pub use memory::{VolatileSlice, VolatileSliceMut};
pub use open_cell::{
    CellAccess, CellHolding, CellRole, CellSnapshot, CellVersion, Commit, ExtentInfo,
    MappingContents, OpenCell, Opening, VersionInfo, Whose, WindowMapping, WriteBarriers,
};
pub use storage_device::ExtentId;

/// The git commit this build is of (`scripts/build-commit.sh`, TASK-111.7).
pub const BUILD_COMMIT: &str = env!("CLADEFS_BUILD_COMMIT");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_are_stamped_with_their_commit() {
        let commit = BUILD_COMMIT.strip_suffix("-dirty").unwrap_or(BUILD_COMMIT);
        assert_eq!(commit.len(), 40, "{BUILD_COMMIT}");
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()), "{BUILD_COMMIT}");
    }
}
