// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The process's physical memory (`crate::slabs`), which the parts of cached sealed extents
//! (`crate::cache`) and the zeros DAX windows map are of: host shared memory, in arenas, each its
//! own object (a memfd on Linux), which a process given an arena's object maps the arena's pages
//! from (TASK-43). As an arena runs out, its relief evicts cached extents' parts. Open extents are
//! written to their cells' segments (`local_cell`), and a window's private pages, copy-on-write
//! DAX's, are of its VM's memory (`crate::dax`): neither is of it.
//!
//! Its size is set once, with [`configure`], before its first page is allocated, or is the
//! default.

use crate::error::{Errno, Error};

pub use crate::slabs::{ArenaId, PhysLocation};

/// The physical memory's configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Bytes of physical memory: the most host memory extents' parts take.
    pub memory_bytes: u64,
}

impl Default for Config {
    /// 1 GiB of memory.
    fn default() -> Self {
        Self { memory_bytes: 1 << 30 }
    }
}

impl Config {
    /// `memory_bytes` of memory.
    pub fn with_memory(memory_bytes: u64) -> Self {
        Self { memory_bytes }
    }
}

/// Configures the physical memory, which fails with `EBUSY` if it has already been configured or
/// used, and with `EINVAL` if the memory is not whole slabs ([`crate::slabs::SLAB`]).
pub fn configure(config: Config) -> Result<(), Error> {
    // cladefs: under madsim, each simulated host's own memory (doc-24, TASK-685.14).
    crate::slabs::configure(config.memory_bytes).map_err(|error| {
        let errno = match error {
            crate::slabs::ConfigError::InUse => Errno::EBUSY,
            crate::slabs::ConfigError::Invalid(_) => Errno::EINVAL,
        };
        Error::new(anyhow::Error::new(errno).context(error.to_string()))
    })
}

/// The physical memory of the default arena: a shared memory object (a memfd on Linux), which the
/// pages of everything but tenants' extents are of (TASK-43), and which a process sharing it maps
/// them from.
pub fn physical_memory() -> std::sync::Arc<std::os::fd::OwnedFd> {
    crate::slabs::default_fd()
}

/// The shared memory object of the physical memory of `arena`, while it exists.
pub fn arena_memory(arena: ArenaId) -> Option<std::sync::Arc<std::os::fd::OwnedFd>> {
    crate::slabs::arena_fd(arena)
}

/// An arena of physical memory of its own (TASK-43), a tenant's, held by what allocates in it: its
/// tenant, the cells opened in it ([`OpenCell::open_in`]), and what reads into the extent cache
/// for them (TASK-79.36). Once none of them holds it, the extents cached in it go, and it goes,
/// its memory once its runs still allocated, those page-ins in flight hold, are freed: nothing
/// allocates in it after, which would fail.
///
/// [`OpenCell::open_in`]: crate::OpenCell::open_in
pub struct Arena(ArenaId);

impl Arena {
    /// A new arena, which holds no page.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(crate::slabs::create_arena()))
    }

    /// Its id, which its pages' locations name ([`PhysLocation`]).
    pub fn id(&self) -> ArenaId {
        self.0
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        crate::cache::evict_arena(self.0);
        crate::slabs::remove_arena(self.0);
    }
}

/// The page size, which DAX windows and mappings are aligned to: the host's, such as 16 KiB on
/// Apple silicon.
pub fn page_size() -> u64 {
    crate::slabs::page_size()
}

/// Linux's `vm.max_map_count` unless raised.
pub const DEFAULT_MAX_MAP_COUNT: u64 = 65_530;

/// How many mappings the host lets a process make, if it limits them, as Linux does:
/// `vm.max_map_count` ([`DEFAULT_MAX_MAP_COUNT`] unless raised), which only /proc says; macOS does
/// not. A process without /proc can't learn it, and fails: a VM process in its jail is told it by
/// its daemon, which reads it here (TASK-172), as the default in its place would be wrong wherever
/// the host raised it. What a VM process's window budgets its runs by (`dax::uffd::budget`); the
/// daemon maps none of its memory, which it bounds nothing of (doc-26).
pub fn max_map_count() -> std::io::Result<Option<u64>> {
    if !cfg!(target_os = "linux") {
        return Ok(None);
    }
    // procfs, the host kernel's: outside the simulation.
    let max = std::fs::read_to_string("/proc/sys/vm/max_map_count")?;
    let invalid = |err| std::io::Error::new(std::io::ErrorKind::InvalidData, err);
    Ok(Some(max.trim().parse().map_err(invalid)?))
}

/// Fails with `EINVAL` unless a window of `block`-byte blocks, as a cell's extents hold them, maps
/// on a host of `host_page`-byte pages ([`page_size`]): one whose pages are no larger, as mmap maps
/// whole host pages alone, at offsets of whole ones in an extent. A host of larger pages (an arm64
/// kernel of 16 KiB, a cell's blocks 4 KiB) maps no extent's blocks into a window: a memory cell
/// restores none there, and a SQLite cell's block device reads from its device instead (board 5230,
/// board 5232).
pub fn window_fits_host(block: u64, host_page: u64) -> Result<(), Error> {
    match host_page <= block {
        true => Ok(()),
        false => Err(Error::new(Errno::EINVAL).context(format!(
            "a window maps {block}-byte blocks, and this host's pages are {host_page} bytes: mmap \
             maps none smaller"
        ))),
    }
}
