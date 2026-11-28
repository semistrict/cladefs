// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: cells open in the daemon's own process, with no VM process (board 13909): what a
//! process embedding the daemon (libcladefs) uses a cell through when nothing in it is a guest, as
//! a SQLite database on its cell is not.
//!
//! A cell a VM process opens costs its process and the daemon a connection: a socket pair, the
//! daemon's memory and the VM process's transfer area as memfds, eventfds, a userfaultfd window,
//! a receiving thread and a fault thread, some 14 open files, two threads and a dozen mappings a
//! cell (sqlite_cladefs's density test), all to carry requests across a process boundary that an
//! embedding process doesn't have. An embedded cell is the open cell itself, read and written
//! through `OpenCellDevice` as the daemon's FUSE mount reads and writes its cells, with nothing
//! of its own beside.
//!
//! It is its cell's writer in the daemon's cell table ([`CellWriter`]), as a VM process's
//! connection is, so the control API syncs, stats, forks and resizes it as any cell a writer has
//! open, and no VM process opens it meanwhile (`EBUSY`). Its device holds it: once the device
//! goes, the last of it, the cell closes, awaited on the daemon's runtime, and its hold of the
//! cell's path goes after, as a connection's do (`server::Connection`'s drop).
//!
//! It is work a drain waits for, as a connection is (`drain`): its holder lets it go, closing its
//! database, for the daemon to drain; and a daemon draining opens none (`EBUSY`).
//!
//! Its commits are made local by its device's flushes (`OpenCellDevice::flush`, a SQLite
//! transaction's), so it needs no flusher behind it, as a connection's ordered commits do.

use crate::Daemon;
use crate::cells::{self, CellWriter, Hold};
/// What a cell holds, which an embedded cell's making names.
pub use bucket_cell::cell::Kind;
use cladefs_core::cell_table::Writer;
use cladefs_core::pages::Arena;
use cladefs_core::{CellId, Errno, OpenCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// A cell open in the daemon's process. See the module docs.
pub struct EmbeddedCell {
    daemon: Arc<Daemon>,
    /// Its cell, once open.
    open_cell: OnceLock<Arc<OpenCell>>,
    /// Its hold of its cell's path, once it has one: let go after its cell closes.
    hold: Mutex<Option<Hold>>,
    /// Whether it is going, its cell to be let go to the next that asks: from its drop on.
    going: AtomicBool,
    /// It, as work the daemon's drain waits for, until its cell is closed (`drain::Work`).
    doing: Option<crate::drain::Doing>,
}

impl EmbeddedCell {
    /// Its cell's ID.
    pub fn id(&self) -> CellId {
        self.open_cell().cell()
    }

    /// Its cell.
    pub(crate) fn open_cell(&self) -> &Arc<OpenCell> {
        self.open_cell.get().expect("an embedded cell is open")
    }

    /// A device over its cell, which holds it (see the module docs): its database's.
    pub fn device(self: &Arc<Self>) -> anyhow::Result<cladefs_fs::OpenCellDevice> {
        cladefs_fs::OpenCellDevice::holding(self.open_cell().clone(), self.clone())
    }
}

impl Writer for EmbeddedCell {
    fn going(&self) -> bool {
        self.going.load(Ordering::Acquire)
    }
}

impl CellWriter for EmbeddedCell {
    fn cell(&self) -> Option<&Arc<OpenCell>> {
        self.open_cell.get()
    }

    /// Its cell's capacity set, as the control API asks: what the cell holds it refuses to shrink
    /// below (`OpenCell::set_capacity`); its device reads its capacity as it opens.
    fn resize(
        &self,
        capacity: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Errno>> + Send + '_>> {
        Box::pin(async move {
            let open_cell = self.open_cell.get().ok_or(Errno::EBADF)?;
            open_cell.set_capacity(capacity).await
        })
    }

    #[cfg(test)]
    fn connection(self: Arc<Self>) -> Option<Arc<crate::server::Connection>> {
        None
    }
}

impl Drop for EmbeddedCell {
    /// Its cell let go, closed if this was its last holder, in a task of its own on the daemon's
    /// runtime, then its hold of the cell's path, which those waiting to open it may then: as a
    /// connection's drop does, whatever drops it last closing no cell in its drop, which blocks.
    fn drop(&mut self) {
        self.going.store(true, Ordering::Release);
        let open_cell = self.open_cell.take();
        let hold = self.hold.get_mut().unwrap_or_else(PoisonError::into_inner).take();
        let doing = self.doing.take();
        let table = self.daemon.table.clone();
        drop(self.daemon.spawn(async move {
            if let Some(open_cell) = open_cell {
                if let Some(hold) = &hold {
                    let key = hold.key().to_owned();
                    open_cell.on_close(move || table.closed(&key));
                }
                cladefs_core::open_cell::release(open_cell).await;
            }
            drop(hold);
            drop(doing);
        }));
    }
}

impl Daemon {
    /// cladefs: a new cell of `kind`, `tenant`'s, of `capacity` bytes, on a disk of the daemon's
    /// the daemon picks, its ID the bucket's to give, open in this process (board 13909). Its
    /// pages are of its tenant's arena ([`arena_of`]).
    pub async fn create_embedded(
        self: &Arc<Self>,
        tenant: u64,
        capacity: u64,
        kind: Kind,
    ) -> Result<Arc<EmbeddedCell>, Errno> {
        let daemon = Arc::clone(self);
        self.run(async move {
            let pool = daemon.disk_pool().ok_or(Errno::EINVAL)?.clone();
            let disk = pool.pick_disk()?;
            let (path, id) = cells::new_managed_cell(&daemon, &disk, tenant).await?;
            let new = cells::New { capacity, kind, tenant, cell: Some(id), ephemeral: false };
            let embedded = embed(&daemon, &path, Some(new), arena_of(&daemon, tenant)?).await?;
            pool.register_cell(id, path);
            Ok(embedded)
        })
        .await
    }

    /// cladefs: the existing cell `cell`, kept on a disk of the daemon's, open in this process
    /// (board 13909): `ENOENT` if none of its disks keeps it, `EBUSY` if a writer has it open.
    pub async fn open_embedded(self: &Arc<Self>, cell: CellId) -> Result<Arc<EmbeddedCell>, Errno> {
        let daemon = Arc::clone(self);
        self.run(async move {
            let pool = daemon.disk_pool().ok_or(Errno::EINVAL)?.clone();
            let path = cells::managed_cell_dir(&pool, cell).await?;
            embed(&daemon, &path, None, arena_of(&daemon, cell.tenant)?).await
        })
        .await
    }
}

/// The arena an embedded cell of `tenant` keeps its pages in, as a VM process of `tenant`'s would
/// (TASK-43): the tenant's own, alive on `daemon`, its limits counting them; the default tenant's
/// for the default tenant; otherwise, a tenant with no arena on the daemon, an arena of the
/// cell's own, which goes with it. Never another tenant's: the default tenant's arena is the
/// memory every VM process of the daemon's own socket maps, and a tenant's cell's pages there
/// were theirs to read (board 14449).
fn arena_of(daemon: &Arc<Daemon>, tenant: u64) -> Result<Arc<Arena>, Errno> {
    if tenant == cladefs_core::host_cell::DEFAULT_TENANT {
        return Ok(crate::server::default_tenant_on(daemon)?.memory_arena().clone());
    }
    if let Some(tenant) = crate::memory::tenant_on(daemon, tenant) {
        return Ok(tenant.memory_arena().clone());
    }
    Arena::new_on(&daemon.host).map_err(|err| {
        log::error!("cladefsd: an embedded cell's arena, tenant {tenant}'s: {err}");
        Errno::EIO
    })
}

/// cladefs: the existing cell at `path`, its pages in `arena`, open in this process: a VM's memory
/// cell, which its memory backend serves (`crate::memory_backend`, board 14630). `EBUSY` if a
/// writer has it open, past a writer going.
#[cfg(all(target_os = "linux", not(madsim)))]
pub(crate) async fn embed_at(
    daemon: &Arc<Daemon>,
    path: &std::path::Path,
    arena: Arc<Arena>,
) -> Result<Arc<EmbeddedCell>, Errno> {
    embed(daemon, path, None, arena).await
}

/// The cell at `path` on `daemon`, made as `new` says if given, its pages in `arena`, open in this
/// process: held to write, as a connection's opening holds it (`server::Connection::open`),
/// opened, and the hold given it, or let go of, with the cell closed, if anything fails.
async fn embed(
    daemon: &Arc<Daemon>,
    path: &std::path::Path,
    new: Option<cells::New>,
    arena: Arc<Arena>,
) -> Result<Arc<EmbeddedCell>, Errno> {
    // cladefs: none while the daemon drains, as no connection is taken then (board 13909).
    let doing = daemon.embedded_work.begin(daemon).ok_or(Errno::EBUSY)?;
    let embedded = Arc::new(EmbeddedCell {
        daemon: Arc::clone(daemon),
        open_cell: OnceLock::new(),
        hold: Mutex::new(None),
        going: AtomicBool::new(false),
        doing: Some(doing),
    });
    let writer: std::sync::Weak<dyn CellWriter> = Arc::downgrade(&embedded) as _;
    let hold = cells::hold_to_write(daemon, &cells::cell_dir_key(path), writer).await?;
    // Closed before its hold lets its path go, if it fails here.
    let open_cell = Arc::new(cells::open_cell_at(daemon, path, new, Some(&arena)).await?);
    if let Err(err) = open_cell.access().may_write().and_then(|()| hold.open(&open_cell)) {
        cladefs_core::open_cell::release(open_cell).await;
        return Err(err);
    }
    // cladefs: should a writer wait for it, its uploader stops at once rather than upload what it
    // holds first, as a connection's does (TASK-216).
    if let Some(cut_short) = open_cell.cut_short() {
        hold.when_wanted(cut_short);
    }
    assert!(embedded.open_cell.set(open_cell).is_ok(), "an embedded cell opened twice");
    *embedded.hold.lock().unwrap_or_else(PoisonError::into_inner) = Some(hold);
    Ok(embedded)
}
