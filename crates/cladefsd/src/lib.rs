// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The extent daemon: one process on a host serving the extent stores of the VMs in other
//! processes, so that each sealed extent is one copy in memory, shared by every VM that maps it
//! (doc-2, doc-4: forks of a VM, VMs of one base image).
//!
//! A VMM (cladefs's Firecracker) runs one VM per process, and an extent cached in a VM process
//! would be shared by that VM's windows alone. The daemon owns them instead: its cells, its extent
//! cache (`crate::cache`), and each VM's window (`crate::dax`). Physical memory is shared memory
//! objects (memfds: `crate::pages`), which a VM process receives once, as it connects: the window's
//! pages are pages of them, which the VM process maps into its own memory, where KVM maps them for
//! the guest, at the offsets the daemon names. Two VMs mapping one extent map the same pages.
//!
//! - A VM process's device sends the guest's requests ([`wire`]) to the daemon, which serves them
//!   from the VM's cell as an in-process device would, and replies. Their data is copied through
//!   the connection.
//! - The window's changes go to the VM process as stage operations ([`wire::STAGE_MAP`] and the
//!   others), which it makes in order, before it takes the replies sent after them: a fault's or a
//!   `MAP`'s mappings are made before the guest is told they are. One that takes access to a page
//!   away (an unmap, or a page made private in its place) is acknowledged, so that no VM has access
//!   to a page the kernel frees or reuses: what the kernel frees waits for it. The daemon waits for
//!   none, as the kernel sends them with its locks held: a VM process that stopped answering would
//!   hold them, and every VM's faults, for good; one that leaves one unacknowledged for long, it
//!   kills (TASK-79.9).
//! - The guest's faults on the window reach the VM process through userfaultfd, which sends them
//!   to the daemon to resolve ([`wire::FAULT`]).
//! - A VM process that exits, or disconnects, takes its cell and window with it: the daemon drops
//!   them, and frees the cell for another connection to open.
//!
//! **Hundreds of cells (TASK-34).** A host runs a few hundred VMs, forks of a few bases, whose
//! cells one daemon serves, so nothing a connection costs grows with the base it is a fork of,
//! nor threads with connections:
//!
//! - **Threads and runtimes.** One `Arc<Daemon>` ([`Daemon`]) owns the daemon's registries and
//!   one consolidated Tokio runtime (`cladefsd`, 4 worker threads, 32 blocking threads) shared by
//!   VM socket I/O, control gRPC, peer gRPC, the memory governor, and background cell flushes
//!   (commits made durable within 50 ms, 32 at most; TASK-92.49, board 10774). Blocking VM
//!   requests run on the daemon's thread pool (`pool`), a connection's 8 at most, the rest waiting
//!   their turn; a fault waiting for the pager holds neither a thread nor one of the 8 (TASK-92.20),
//!   nor does an opening waiting for a cell or a restore's version (TASK-92.5). Two specialized
//!   runtimes remain intentionally separate for latency isolation: `crate::cache`'s `cladefs pager`
//!   (dedicated to synchronous decompression/disk page-ins so control/peer RPCs cannot delay DAX
//!   faults) and `crate::bucket`'s bucket runtime (object storage uploads and fetches).
//! - **Files.** A cell keeps its own segments' and extents' files open, the most
//!   `local_cell::Options::open_files` of each (64) but for those reads use: a fork, which writes
//!   little, few; as many raw files of extents written large since its last commit at most, those
//!   past them going to segments (board 12555); and its open segment's, and its commit log. The files of the segments a fork
//!   inherited are its cell's, which every fork of it shares, the most 64 open but for those in
//!   use (`local_cell::Lineage`). A VM process's connection is two sockets here.
//! - **Memory.** A fork holds what it changed over its base, and the segments it tracks: its base's
//!   state is a layer every fork of it shares, one copy (TASK-31); its base's pages are cached
//!   once, for every fork reading or mapping them (TASK-32).
//!
//! `serves_hundreds_of_forks_of_a_base` (tests.rs) opens 300 forks of a base at once, each reading
//! it, and bounds the daemon's threads, and each fork's open files and resident memory.
//!
//! **Control (TASK-43.3).** The host's VM manager, which launches the VMMs, makes, imports, forks,
//! places, syncs, resizes and deletes cells through the daemon's control API, gRPC over a Unix
//! socket of its own ([`control`]; `proto/cladefs/cladefsd/v1/cladefsd.proto`), which a cell a VM
//! process has open refuses what would change it under its guest's feet. VM processes keep the
//! extent protocol, which carries their memory as file descriptors.
//!
//! **Isolation.** Guest isolation is the security boundary: a guest reaches only what its window
//! maps, so every VM of one tenant, or of none, is secure. As defence in depth against a VM process
//! escaped from its guest, the daemon serves tenants (TASK-43): the control API grants each a VM
//! socket of its own ([`Tenant`], `grants`), whose connections name cells in the tenant's
//! directory alone, and hold the tenant's arena of physical memory, with the default
//! arena, whose pages are public bases', read-only, and nothing of another tenant's. The daemon's
//! own VM socket is the default tenant's, whose pages are of an arena of its own, as a tenant's
//! are, the default arena holding public bases' alone (TASK-79.17): its connections name cells in
//! the daemon's roots alone (`CLADEFSD_ROOTS`), which it owns, and boots owning one at least
//! (board 14305). One rule, a
//! directory, for every tenant: what a VM process's user may read or write does not matter (it did
//! before, TASK-39.2), as nothing below a tenant is separated. Its physical memory goes to a
//! connection only once it opens a cell.
//!
//! That directory is all a tenant's VM processes reach of the bucket: the cell they name is
//! ignored, and a cell comes into a tenant's directory only by the VM manager's making a cell of
//! it there, as it restores the tenant's snapshot on another host, and goes by its dropping that
//! cell (TASK-154; `proto/cladefs/cladefsd/v1/cladefsd.proto` says what the VM manager does). The
//! default tenant's VM processes name any cell: its socket is the daemon's user's alone
//! ([`bind_owner_only`], TASK-155), whatever is mounted where, so that a jailed VM process, of
//! another user, reaches its tenant's socket alone, which the VM manager gives it (`Grant`).
//! What differs between tenants is a tenant's policy (`server::Policy`, TASK-160.10): where its VM
//! processes name cells, whether the cell one names counts, and whether they follow public
//! bases. What may be done to a cell is the cell's, fixed as it opens ([`crate::CellAccess`]).

#![deny(clippy::undocumented_unsafe_blocks)]
#![recursion_limit = "256"]

#[cfg(test)]
#[global_allocator]
static ALLOCATOR: cladefs_core::heap::Bounded = cladefs_core::heap::Bounded;

mod boot;
pub mod cells;
pub use cladefs_wire::client;
pub use cladefs_wire::codec;
pub mod control;
mod daemon;
pub use cladefs_wire::device;
pub mod disk;
pub mod disks;
pub mod doctor;
pub mod drain;
// cladefs: libcladefs's, outside the simulation (board 13909).
#[cfg(not(madsim))]
pub mod embedded;
pub mod events;
pub mod governor;
mod grants;
mod grpc;
/// cladefs: a simulated guest's connection, under madsim (doc-24, TASK-685.6).
#[cfg(madsim)]
pub mod guest;
mod http;
pub mod logging;
pub mod memory;
pub mod metrics;
pub mod migration;
pub mod mount;
pub mod otel;
pub mod peers;
mod pool;
pub mod process;
mod quota;
mod reads;
mod run;
pub mod server;
mod stats;
mod storage;
mod telemetry;
pub use cladefs_wire::wire;
#[cfg(all(feature = "mount", target_os = "linux", not(madsim)))]
pub mod fuse_control;
#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub mod testing_control;
pub mod zpages;

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

#[cfg(all(target_os = "linux", not(madsim)))]
pub use boot::boot;
pub use client::{CellClient, DaemonConnection, Unmapped, spawn_on_replies};
#[cfg(not(madsim))]
pub use control::serve_control;
pub use daemon::Daemon;
pub use device::DaemonDevice;
pub use http::Pages;
#[cfg(not(madsim))]
pub use http::serve_http;
pub use run::supervised;
pub use server::Tenant;
#[cfg(not(madsim))]
pub use server::{serve, serve_tenant};
pub use storage::cache_config;

/// cladefs: whether the process's default daemon serves VM processes: booted, and not draining.
#[cfg(not(madsim))]
pub fn serving() -> bool {
    Daemon::process().serving()
}

/// cladefs: `daemon` serving VM processes at its own socket (`serve_on`), until this drops,
/// however serving ends: its listener shut down, or a panic unwinding (board 8501), which left the
/// health service saying SERVING, no VM process served.
pub(crate) struct Serving(Arc<Daemon>);

impl Serving {
    pub(crate) fn begin(daemon: &Arc<Daemon>) -> Self {
        daemon.serving.send_replace(true);
        Self(Arc::clone(daemon))
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.0.serving.send_replace(false);
    }
}

/// Says the process's default daemon stops serving, as it is about to exit: once the control API's
/// health service, if it serves, reports so to its callers.
#[cfg(not(madsim))]
pub async fn stop_serving() {
    Daemon::process().stop_serving().await;
}

/// cladefs: shuts `listener`, a listening socket's descriptor, down, which takes no more
/// connections: on Linux, its accepts fail with `EINVAL`, those of every descriptor of it, which
/// stop serving it (`server::serve_as`), and connecting to it is refused. As a tenant's grant is
/// revoked, and the daemon drains (`server::StopAccepting`).
pub(crate) fn shut_down(listener: std::os::fd::BorrowedFd<'_>) {
    use std::os::fd::AsRawFd;
    // cladefs: macOS shuts no listener down (`ENOTCONN`): its accepts stop otherwise
    // (`server::StopAccepting`, board 13065).
    if cfg!(not(target_os = "linux")) {
        return;
    }
    // SAFETY: A plain system call on a descriptor borrowed, open.
    if unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) } != 0 {
        log::warn!(
            "cladefsd: shutting a VM socket's listener down: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// A listener at `socket`, which only this process's user may connect to (TASK-155): mode 0600
/// from the moment it is there, whatever the process's umask, rather than open until it is
/// changed. The daemon's own VM socket and its control socket (TASK-159) are.
pub fn bind_owner_only(socket: impl AsRef<Path>) -> std::io::Result<UnixListener> {
    bind_given(socket.as_ref(), 0o600, None, None)
}

/// A listener at `socket`, of mode `mode` and given to `uid` and `gid` where given, from the moment
/// it is there, whatever the process's umask and whatever its other threads do (TASK-159): bound
/// in a directory beside it that this process's user alone may enter, given its mode and owner
/// there, and only then linked at `socket`, which fails with `EEXIST` if anything is there already,
/// as a running daemon's socket. A directory a process stopped mid-way left goes first.
///
/// Not under a umask set around the bind, which is the process's, all its threads': another
/// thread's file would take it, and a grant comes while they serve.
pub(crate) fn bind_given(
    socket: &Path,
    mode: u32,
    uid: Option<u32>,
    gid: Option<u32>,
) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let name = socket.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
    // `socket`'s path and 3 bytes more, as a socket's path is short (108 bytes, with its NUL).
    let mut hidden = std::ffi::OsString::from(".");
    hidden.push(name);
    let private = socket.with_file_name(hidden);
    let bound = private.join("s");
    remove_unfinished(&private, &bound)?;
    std::fs::DirBuilder::new().mode(0o700).create(&private)?;
    let listener = UnixListener::bind(&bound).and_then(|listener| {
        std::os::unix::fs::chown(&bound, uid, gid)?;
        std::fs::set_permissions(&bound, std::fs::Permissions::from_mode(mode))?;
        std::fs::hard_link(&bound, socket)?;
        // The moment it is at `socket`, which tests look at.
        fail::fail_point!("cladefsd::bound");
        Ok(listener)
    });
    // At `socket` alone, or nowhere if it failed: where it was bound goes.
    if let Err(err) = remove_unfinished(&private, &bound) {
        log::warn!("cladefsd: removing {}: {err}", private.display());
    }
    listener
}

/// Removes the directory `private` that [`bind_given`] binds in, and the socket `bound` in it,
/// whichever of them is there.
fn remove_unfinished(private: &Path, bound: &Path) -> std::io::Result<()> {
    let gone = |removed: std::io::Result<()>| match removed {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    };
    gone(std::fs::remove_file(bound))?;
    gone(std::fs::remove_dir(private))
}

/// Raises the process's limit of open files to the most it may: each cell it serves keeps its
/// own segments' and extents' files open, and its connection's (see the module's docs, TASK-34),
/// for every VM on the host, which the usual default of 1024 runs out of after a hundred or so.
/// cladefs: its doctor then warns of a limit under [`open_files_floor`] (board 14217).
pub fn raise_open_files_limit() -> std::io::Result<()> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: Plain system calls on a limit the process owns.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        limit.rlim_cur = most_open_files(limit.rlim_max)?;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// cladefs: the most open files a process may set its limit to, given its hard limit `hard`:
/// that, on Linux; on macOS, whose hard limit is often unlimited, at most the kernel's most a
/// process (`kern.maxfilesperproc`), past which `setrlimit` refuses (board 13065).
fn most_open_files(hard: libc::rlim_t) -> std::io::Result<libc::rlim_t> {
    #[cfg(target_os = "macos")]
    {
        let mut most: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>();
        // SAFETY: Reads an int-sized sysctl into `most`, of `len` bytes.
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut most).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if read != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let most = libc::rlim_t::try_from(most).map_err(std::io::Error::other)?;
        return Ok(hard.min(most));
    }
    #[cfg(not(target_os = "macos"))]
    Ok(hard)
}

/// cladefs: the cells a daemon has room to keep open at least, their files at their most (board
/// 8595): a host of a few hundred VMs, each a cell or more. On macOS a developer's machine, whose
/// VMs are few and whose processes macOS lets keep 10,240 files by default
/// (`kern.maxfilesperproc`): 32, so that a daemon a process embeds (libcladefs) starts there
/// without the machine's settings changed (board 13850).
pub const FLOOR_CELLS: u64 = if cfg!(target_os = "macos") { 32 } else { 256 };

/// cladefs: what a daemon keeps open of its own, beside its cells' files, at most: its sockets and
/// listeners, its log, its part cache's and its bucket's files and connections, its arenas' memfds.
pub(crate) const OWN_OPEN_FILES: u64 = 1024;

/// cladefs: the open files a cell keeps at most beside its segments', its sealed extents' and its
/// open extents' raw files (each `local_cell::Options::open_files`): its open segment's raw + log
/// (2), its prepared next segment's raw + log (2), up to 2 full segments kept open between commits
/// (board 12887), its directory handle and flock (2), its VM process's connection sockets and pidfd
/// (3), and transient flush syncs (`AT_ONCE` = 8) plus an active framing (1).
const CELL_OPEN_FILES_BESIDE: u64 = 20;

/// cladefs: the open files a cell keeps at most, as the daemon opens cells
/// (`cladefs_core::host_cell::options`), its VM process's connection's among them: its segments',
/// its sealed extents', its open extents' raw files (board 12555), and those beside
/// ([`CELL_OPEN_FILES_BESIDE`]). What the daemon's limit of open files is shared out by: its floor
/// ([`open_files_floor`]), and each connection's (`server::most_connections`, board 9975), which
/// opens one cell at most.
pub(crate) fn cell_open_files() -> u64 {
    let segments_and_extents = 3 * cladefs_core::host_cell::options().open_files as u64;
    segments_and_extents + CELL_OPEN_FILES_BESIDE
}

/// cladefs: the limit of open files (`RLIMIT_NOFILE`) under which the daemon's doctor warns (board
/// 14217; it refused to start before, board 8595): its own, and [`FLOOR_CELLS`] cells' at their
/// most. Under it, the daemon serves fewer connections at once (`server::most_connections`), not
/// answering `EMFILE` once its host is busy (TASK-79.35).
pub fn open_files_floor() -> u64 {
    OWN_OPEN_FILES + FLOOR_CELLS * cell_open_files()
}

#[cfg(test)]
mod tests;
