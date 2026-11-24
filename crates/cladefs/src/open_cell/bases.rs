// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Attached-base routing for [`OpenCell`] (backlog doc-10).
//!
//! Forks of other open cells attached to an [`OpenCell`] are indexed by the cell IDs whose extents
//! they hold and opened lazily on the first read or map through to them.

use super::{Changing, OpenCell, Opening, errno, release};
use crate::cell_dir;
use crate::error::{Errno, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use storage_device::ExtentId;

/// cladefs: what opens an open cell attached to another, the first time it is read through to,
/// awaited (doc-24, step 5).
pub type OpenBase =
    Arc<dyn Fn(&Path) -> futures::future::BoxFuture<'static, Result<Arc<OpenCell>>> + Send + Sync>;

/// cladefs: a fork attached to an open cell (backlog doc-10).
struct Base {
    path: PathBuf,
    /// Opened once, by the first read through to it; those reading meanwhile wait for it, holding
    /// no thread.
    cell: tokio::sync::OnceCell<Arc<OpenCell>>,
}

impl Base {
    fn new(path: &Path) -> Self {
        Self { path: path.to_owned(), cell: tokio::sync::OnceCell::new() }
    }

    /// The fork, opened for `attached_to`, which is attached to it, once.
    async fn cell(&self, attached_to: &OpenCell) -> Result<Arc<OpenCell>> {
        // cladefs: nothing a panic went through opens what it reads through to (`crate::halt`).
        attached_to.halt.check()?;
        let open = async || match attached_to.bases.open.get() {
            Some(open) => open(&self.path).await,
            None => {
                let arena = attached_to.arenas.own.clone();
                let host = attached_to.host.clone();
                // cladefs: boxed, its opening once, which every read's and map's future, and so
                // every request's, would hold otherwise, some 11 KB (TASK-707).
                let opening = Box::pin(async move {
                    Opening::here_on(host)?.arena(arena).open_base(&self.path).await
                });
                let opened = opening.await.map_err(|err| {
                    log::warn!("cladefs: opening {}, attached: {err:#}", self.path.display());
                    err.errno().unwrap_or(Errno::EIO)
                })?;
                Ok(Arc::new(opened))
            }
        };
        let base = self.cell.get_or_try_init(open).await?.clone();
        // cladefs: a shredded or halted attached base serves nothing more (board 5542,
        // board 2704).
        base.halt.check()?;
        Ok(base)
    }
}

/// cladefs: what is attached to an open cell: by the cells of the extents each holds, so that
/// finding what reads an extent takes the same time however many are attached. A cell has more than
/// one only for generations of one open cell, attached at once, the newest last.
#[derive(Default)]
pub(super) struct Bases {
    open: OnceLock<OpenBase>,
    /// Unlike the open cell's locks, not halted ([`crate::halt`]): nothing that may panic is
    /// called holding it.
    by_cell: RwLock<HashMap<u64, Vec<Arc<Base>>>>,
}

impl Bases {
    /// Routes `cells` to the cell directory at `path`, if they aren't.
    fn add(&self, cells: Vec<u64>, path: &Path) {
        let mut by_cell = self.by_cell.write().unwrap();
        let attached = cells
            .iter()
            .filter_map(|cell| by_cell.get(cell))
            .flatten()
            .find(|base| base.path == path)
            .cloned();
        let base = attached.unwrap_or_else(|| Arc::new(Base::new(path)));
        for cell in cells {
            let routed = by_cell.entry(cell).or_default();
            if !routed.iter().any(|routed| Arc::ptr_eq(routed, &base)) {
                routed.push(base.clone());
            }
        }
    }

    /// Whether extents of `cell` are read through to what is attached.
    #[cfg(test)]
    pub(super) fn routes(&self, cell: u64) -> bool {
        self.by_cell.read().unwrap().contains_key(&cell)
    }

    /// How many cells are routed to attached bases.
    #[cfg(test)]
    pub(super) fn routed_cells(&self) -> usize {
        self.by_cell.read().unwrap().len()
    }

    /// What is attached that may hold extents of `cell`, newest first.
    fn of(&self, cell: u64) -> Option<Vec<Arc<Base>>> {
        let by_cell = self.by_cell.read().unwrap();
        by_cell.get(&cell).map(|bases| bases.iter().rev().cloned().collect())
    }

    /// How many of those attached are open.
    #[cfg(test)]
    fn opened(&self) -> usize {
        let mut opened: Vec<_> = (self.by_cell.read().unwrap().values().flatten())
            .filter(|base| base.cell.initialized())
            .map(Arc::as_ptr)
            .collect();
        opened.sort();
        opened.dedup();
        opened.len()
    }

    /// cladefs: closes each base attached that this was the last reference to (doc-24, step 9).
    pub(super) async fn close(self: Arc<Self>) {
        if let Some(bases) = Arc::into_inner(self) {
            let by_cell = bases.by_cell.into_inner().unwrap_or_else(PoisonError::into_inner);
            for base in by_cell.into_values().flatten().filter_map(Arc::into_inner) {
                if let Some(cell) = base.cell.into_inner() {
                    release(cell).await;
                }
            }
        }
    }
}

impl OpenCell {
    /// cladefs: attaches the cell directory at `base`, a fork pinning what it holds, to this one,
    /// which `cells` of ([`OpenCell::ancestry`]), recorded in its directory: `READ` and `MAP` of
    /// its extents read through to it from now on, and in forks of this open cell, which a VM
    /// restored is. Attached already, it does nothing. The guest has no part in this (backlog
    /// doc-10).
    pub fn attach_base(&self, base: &Path, cells: &[u64]) -> Result<()> {
        let _admitted = self.admit(Changing::Cell)?;
        cell_dir::attach_on(self.host.local(), &self.path, base, cells)?;
        self.bases.add(self.routed(cells), base);
        Ok(())
    }

    /// cladefs: of `cells`, those of a base's that its reads are routed by: not the open cell's own
    /// and those it inherited, whose extents it holds, or freed.
    fn routed(&self, cells: &[u64]) -> Vec<u64> {
        let own = self.ancestry();
        cells.iter().copied().filter(|cell| !own.contains(cell)).collect()
    }

    /// cladefs: routes reads to what is attached to the open cell, as recorded, each opened by
    /// `open` when first read: however many are attached, they cost it nothing until then.
    pub fn open_bases(&self, open: OpenBase) -> Result<()> {
        assert!(self.bases.open.set(open).is_ok(), "an open cell opens its bases one way");
        for (cells, base) in cell_dir::attached(&self.path)? {
            self.bases.add(self.routed(&cells), &base);
        }
        Ok(())
    }

    /// cladefs: how many of what is attached to it are open.
    #[cfg(test)]
    pub(crate) fn bases_opened(&self) -> usize {
        self.bases.opened()
    }

    /// cladefs: the base attached that holds `extent`, with its length, if one does.
    pub(super) async fn base_holding(
        &self,
        extent: ExtentId,
    ) -> Result<Option<(Arc<OpenCell>, u64)>> {
        let Some(bases) = self.bases.of(extent.cell) else { return Ok(None) };
        for base in bases {
            let cell = base.cell(self).await?;
            if let Some((len, false)) = cell.local.extent(extent).map_err(errno)? {
                return Ok(Some((cell, len)));
            }
        }
        Ok(None)
    }
}
