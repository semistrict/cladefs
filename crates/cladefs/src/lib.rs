// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: Fuchsia's Fxfs, and the cells a guest's own Fxfs keeps its filesystem in.
//!
//! cladefs has two uses of Fxfs:
//!
//! - **Open cells** ([`OpenCell`]), which a guest running Fxfs in its own kernel keeps its
//!   filesystem in, through a VMM's extent-store device (backlog doc-4). On Linux, cladefs's
//!   daemon, `cladefsd`, one a host (`cladefsd`), serves the cells of a host's VMs, and maps their
//!   extents into the VMs' DAX windows from one cache of their pages, which the VMs share; a VMM's
//!   device is a `CellClient`, a connection to it.
//! - **A filesystem on the host** ([`Filesystem`]), which formats, fills and checks images, such
//!   as the volumes a guest's Fxfs mounts. It is the guest's filesystem too: Fxfs's POSIX layer
//!   (crates/fxfs-posix), which the guest kernel's VFS calls, and the host's filesystem calls as a
//!   VFS would.
//!
//! # Using the filesystem
//!
//! A [`Filesystem`] is an Fxfs volume, whose nodes are [`Node`]s: handles, the first its
//! [`root`](Filesystem::root) directory, whose methods are the nodes' operations. A host can use
//! it as it would any filesystem:
//!
//! ```no_run
//! use cladefs::{Filesystem, Owner, Storage};
//!
//! # async fn serve() -> Result<(), Box<dyn std::error::Error>> {
//! let fs = Filesystem::format(Storage::create("disk.img", 1 << 30)?).await?;
//! let owner = Owner { uid: 1000, gid: 1000 };
//! let (file, _) = fs.root().create(b"hello", 0o644, owner).await?;
//! file.write(0, b"hello, guest").await?;
//! drop(file);
//! fs.close().await?.check().await?;
//! # Ok(())
//! # }
//! ```
//!
//! - Operations that find or make a node ([`lookup`](Node::lookup), [`create`](Node::create),
//!   [`mkdir`](Node::mkdir), [`mknod`](Node::mknod), [`symlink`](Node::symlink)) return a handle
//!   to it, with its attributes. A handle holds its node open, as an open file does, until it and
//!   its clones drop: a file unlinked stays readable while a handle to it lives. Nodes are the
//!   handles' own filesystem's: a rename or link to another's fails with `EXDEV`.
//! - Files need no opening; files are truncated with [`setattr`](Node::setattr). A directory's
//!   [`entries`](Node::entries) are listed as it holds them then. A node's [`Inode`] number is
//!   its Fxfs object ID.
//! - Errors are [`Errno`]s, Linux's values on every host.
//!
//! On Linux, [`fuse`] mounts a filesystem with FUSE, for any process to use; `cladefs-mount` mounts
//! a cell so, and cladefsd tenants' cells (`cladefsd::mount`, TASK-475.2).
//!
//! # Async
//!
//! Every operation that can wait is an `async fn`, and none holds a thread while it waits. Their
//! futures are `Send`, and may run on any executor, or be driven by any thread with a `block_on`,
//! any number at once:
//!
//! - Fxfs runs on threads of its own (see [`Options::executor_threads`]), which run every
//!   operation, reads and writes too, and wake their futures as they complete.
//!
//! Dropping a handle never waits. Dropping a future gives up waiting for its operation, which runs
//! to completion on Fxfs's threads regardless: a read leaves its buffers as they were. Dropping a
//! mounted filesystem blocks the dropping thread until it is unmounted; await
//! [`close`](Filesystem::close) instead. Either waits for the operations under way.
//!
//! # Images and volumes
//!
//! Fxfs keeps any number of volumes on one image, each a filesystem of its own, which share the
//! image's journal, its space and the memory caching it. An [`Image`] mounts the image, and
//! creates, lists, opens and deletes its volumes; each open volume is a [`Filesystem`]:
//!
//! ```no_run
//! use cladefs::{Image, Storage};
//!
//! # async fn serve() -> Result<(), Box<dyn std::error::Error>> {
//! let image = Image::format(Storage::create("disk.img", 1 << 30)?).await?;
//! let home = image.create_volume("home").await?;
//! let scratch = image.create_volume("scratch").await?;
//! # drop((home, scratch));
//! image.close().await?.check().await?;
//! # Ok(())
//! # }
//! ```
//!
//! A volume unmounts on its own; closing the image unmounts those still mounted. A filesystem
//! [mounted](Filesystem::mount) straight from storage is the `default` volume of an image of its
//! own, which closing it closes.
//!
//! [`Image::snapshot_volume`] snapshots a whole volume, mounted or not, into a new one, in time and
//! space independent of its size: the snapshot shares the source's metadata and data until
//! either changes them, as Fxfs writes changes to new blocks, and what they share is freed once
//! neither references it.
//!
//! # Storage
//!
//! An image lives on [`Storage`]: an image, which is a cell directory, a file
//! per extent, or memory. Closing an image, or a filesystem mounted from storage, gives its
//! storage back, which [`Storage::check`] checks.
//!
//! An image's I/O bypasses the host's page cache where the host filesystem allows, and runs on
//! threads of its own. Freeing an extent removes its file.
//!
//! An image keeps its cell in a bucket, as every cell directory does: a file bucket of its own,
//! in its directory, which its description names (or, on a host of cladefs's system, the host's).
//! Its commits are uploaded behind them, and closing it waits for them to be; what it frees, the
//! process collects there after its commits.
//!
//! An image can be forked: [`Storage::fork_to`] forks closed storage, and
//! [`Image::fork_storage_to`] a mounted image's, after writing back its volumes' dirty data.
//! Forking forks the image's cell directory, which shares the image's data rather than copies it,
//! whatever it holds, and its cell in its bucket, which the fork shares.

#![deny(missing_docs)]
// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]
// cladefs: the daemon's control calls' futures, an image's import's holding the store's, nest deeper than the
// default on macOS, whose madsim build of the daemon overflowed it (board 2912), as cladefsctl's did.
#![recursion_limit = "256"]

// cladefs: `tests-kms` has a host given no KMS encrypt under the tests' KMS, its KEKs known to all
// (`bucket::testing`, TASK-411.28): this crate's tests' and sqlite_cladefs', which their
// dev-dependencies turn on, never a host's. A release build is a host's, and tests are not (the dev
// and sim profiles have debug assertions).
#[cfg(all(feature = "tests-kms", not(debug_assertions)))]
compile_error!(
    "cladefs's `tests-kms` in a release build, whose hosts would encrypt under KEKs known to all: \
     build a package alone (`-p PACKAGE`), not across the workspace, whose dev-dependencies turn \
     it on (TASK-411.28)"
);

pub mod bucket;
mod cache;
#[cfg(any(target_os = "linux", madsim))]
pub mod cladefsd;
mod dax;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod ephemeral_host;
mod error;
mod evidence;
mod executor;
pub mod faults;
mod filesystem;
pub mod fuse;
mod halt;
#[cfg(test)]
mod heap;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod hearing;
mod host_cell;
mod image;
pub mod import;
mod memory;
#[cfg(target_os = "linux")]
pub mod memory_cell;
mod node;
mod open_cell;
pub mod pages;
pub mod protocol;
pub mod renamed;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod request;
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_testing;
pub mod slabs;
#[cfg(test)]
mod sparse_shrink_tests;
pub mod stats;
mod storage;
mod sync;
// The extent daemon's (Linux), and its tests' everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod cell_dir;
#[cfg(target_os = "linux")]
pub mod signals;
#[cfg(target_os = "linux")]
pub mod stacks;
// The extent daemon's (Linux), and its tests' everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod caller;
// The extent daemon's (Linux), and its tests' everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod cell_requests;
// The extent daemon's (Linux), and its tests' everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod cell_table;
#[cfg(test)]
mod test_checkpoints;
#[cfg(test)]
mod test_data;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(target_os = "linux")]
pub mod thread;
mod volume;

pub use bucket_cell::layout::CellId;
#[cfg(target_os = "linux")]
pub use cladefsd::{CellClient, DaemonConnection, DaemonDevice, Unmapped};
#[cfg(target_os = "linux")]
pub use dax::Prefault;
pub use dax::failures::{AccessFailures, FailingRun, Failure};
#[cfg(target_os = "linux")]
pub use dax::uffd::{
    describe_windows, guest_access_failed, guest_thread_started, userfaultfd_accessible,
};
pub use dax::{Access, DaxWindow, SecondStage};
pub use error::{Errno, Error, Result};
pub use filesystem::{Filesystem, Node, Options};
pub use host_cell::CellOf;
pub use image::{Compactions, Image, TreeCompactions, VolumeMetadata};
pub use memory::{VolatileSlice, VolatileSliceMut};
pub use node::{
    Attr, DirEntry, FileType, Inode, Owner, RenameFlags, SetAttr, SetTime, StatFs, XattrMode,
};
pub use open_cell::{
    CellAccess, CellHolding, CellRole, CellVersion, Commit, ExtentInfo, MappingContents, OpenBase,
    OpenCell, Opening, VersionInfo, Whose, WindowMapping, WriteBarriers,
};
pub use storage::Storage;
pub use storage_device::ExtentId;

/// cladefs: the commit this build is of, as scripts/build-commit.sh said at build time (build.rs):
/// a commit, with `-dirty` if a tracked file differed from it, or `unknown`. What cladefsd's
/// metrics, cladefsctl --version and a VM process's connection to cladefsd report (TASK-111.7).
pub const BUILD_COMMIT: &str = env!("CLADEFS_BUILD_COMMIT");

#[cfg(test)]
mod tests {
    use super::*;

    /// TASK-111.7: a build of this checkout is of its commit, `-dirty` or not, as builds here
    /// are from git; or of the commit the source a host run's worktree carries says, which is too.
    #[test]
    fn builds_are_stamped_with_their_commit() {
        let commit = BUILD_COMMIT.strip_suffix("-dirty").unwrap_or(BUILD_COMMIT);
        assert_eq!(commit.len(), 40, "{BUILD_COMMIT}");
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()), "{BUILD_COMMIT}");
    }
}
