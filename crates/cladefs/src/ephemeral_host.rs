// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a host keeps of its ephemeral cells (doc-25, TASK-698.3): its incarnation, which
//! their references name, and a journal of them, by which it retires the references of those it
//! holds no more as it starts (specs/ephemeral, `retireOnlyUnheld`).
//!
//! Its directory (`CLADEFSD_EPHEMERAL`), on the disk its ephemeral cells' stores are on, holds:
//!
//! - `incarnation`: a random number made with the directory, new when its disk is lost: the host's
//!   ephemeral cells, their references naming it, are lost with it, and decommissioning it
//!   (`DecommissionHost`) retires those references;
//! - `cells/<TENANT>.<CELL>`, one for each ephemeral cell it may hold, written, durably, before
//!   the cell's reference: its parent and origin, its store's path, and the bucket its parent is
//!   kept in. Removed once the reference is.
//!
//! As the daemon starts, before it serves, each journal entry whose store is not there, an
//! ephemeral cell's of its cell, has its reference retired and goes: a fork its host crashed in,
//! or a deletion.

use crate::CellId;
use anyhow::{Context as _, Error, bail};
use bucket_cell::layout::Origin;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The host's ephemeral cells' directory (see the module's docs).
pub(crate) struct Host {
    dir: PathBuf,
    incarnation: u64,
}

/// A journal entry: an ephemeral cell the host may hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub child: CellId,
    pub origin: Origin,
    pub path: PathBuf,
    pub bucket: String,
}

node_local::node_local! {
    /// The host's ephemeral cells' directory, once the daemon opened it ([`open`]).
    static HOST: Mutex<Option<Arc<Host>>> = Mutex::new(None);
}

/// Opens the host's ephemeral cells' directory at `dir`, made if it is not there with a new
/// incarnation, as this process's from now on.
pub(crate) fn open(dir: &Path) -> Result<Arc<Host>, Error> {
    std::fs::create_dir_all(dir.join(CELLS))
        .with_context(|| format!("making {}", dir.display()))?;
    let path = dir.join(INCARNATION);
    let incarnation = match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().parse().with_context(|| format!("{}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let incarnation = rand::random::<u64>() | 1;
            write_durably(&path, incarnation.to_string().as_bytes())?;
            incarnation
        }
        Err(err) => return Err(Error::new(err).context(format!("reading {}", path.display()))),
    };
    let host = Arc::new(Host { dir: dir.to_owned(), incarnation });
    *HOST.get().lock().unwrap() = Some(host.clone());
    log::info!("cladefsd: ephemeral cells at {}, incarnation {incarnation:x}", dir.display());
    Ok(host)
}

/// The host's ephemeral cells' directory, if the daemon opened one: `ENOTSUP` else, as a host
/// without one keeps no ephemeral cell.
pub(crate) fn host() -> Result<Arc<Host>, crate::Errno> {
    HOST.get().lock().unwrap().clone().ok_or(crate::Errno::ENOTSUP)
}

const CELLS: &str = "cells";
const INCARNATION: &str = "incarnation";

impl Host {
    /// Its incarnation, which its ephemeral cells' references name.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    fn entry_path(&self, child: CellId) -> PathBuf {
        self.dir.join(CELLS).join(format!("{:x}.{:x}", child.tenant, child.id))
    }

    /// Notes, durably, that it may hold `entry`'s ephemeral cell: before its reference is written.
    pub fn note(&self, entry: &Entry) -> Result<(), Error> {
        write_durably(&self.entry_path(entry.child), &serde_json::to_vec(entry)?)
    }

    /// Forgets `child`, its reference gone.
    pub fn forget(&self, child: CellId) {
        let path = self.entry_path(child);
        if let Err(err) = std::fs::remove_file(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("cladefsd: forgetting {}: {err}", path.display());
        }
    }

    /// The ephemeral cell `child`'s entry, if it has one.
    pub fn entry(&self, child: CellId) -> Option<Entry> {
        let bytes = std::fs::read(self.entry_path(child)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Its journal's entries, those that read whole: one torn by a crash, after which no reference
    /// was written, goes.
    pub fn entries(&self) -> Result<Vec<Entry>, Error> {
        let mut entries = Vec::new();
        for found in std::fs::read_dir(self.dir.join(CELLS))? {
            let path = found?.path();
            if path.extension().is_some_and(|extension| extension == "new") {
                std::fs::remove_file(&path)?;
                continue;
            }
            let read = std::fs::read(&path).map_err(Error::new);
            match read.and_then(|bytes| serde_json::from_slice::<Entry>(&bytes).map_err(Error::new))
            {
                Ok(entry) => entries.push(entry),
                Err(err) => {
                    log::warn!("cladefsd: {} is no whole note, removed: {err:#}", path.display());
                    std::fs::remove_file(&path)?;
                }
            }
        }
        Ok(entries)
    }

    /// Retires the references of the ephemeral cells its journal names that it holds no more: its
    /// store at its path gone, or not an ephemeral cell's of its cell. Returns how many.
    pub async fn retire_stale(&self) -> Result<usize, Error> {
        let mut retired = 0;
        for entry in self.entries()? {
            if local_cell::upload::blocking(|| held(&entry)) {
                continue;
            }
            self.retire(&entry).await?;
            retired += 1;
        }
        Ok(retired)
    }

    /// Retires `entry`'s reference, then the entry.
    pub async fn retire(&self, entry: &Entry) -> Result<(), Error> {
        let bucket = crate::host_cell::bucket(&entry.bucket)?;
        let (origin, child) = (entry.origin, entry.child);
        bucket_cell::lifecycle::unrefer_ephemeral(&*bucket, origin, self.incarnation, child).await;
        self.forget(child);
        Ok(())
    }
}

/// Whether the store at `entry`'s path is its ephemeral cell's, whole.
fn held(entry: &Entry) -> bool {
    if !entry.path.exists() || local_cell::unmade_host(&entry.path) {
        return false;
    }
    let ephemeral = local_cell::host_base(&entry.path).is_ok_and(|base| base.is_some());
    ephemeral && local_cell::host_cell(&entry.path).is_ok_and(|cell| cell == entry.child)
}

/// Writes `bytes` to a file at `path`, whole, durably, over any there.
fn write_durably(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let staged = path.with_extension("new");
    let mut file = std::fs::File::create(&staged)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&staged, path)?;
    let Some(parent) = path.parent() else { bail!("{} has no directory", path.display()) };
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's directory keeps its incarnation across openings, and its journal its notes, until
    /// forgotten; a torn note, or one staged, goes as it is listed.
    #[test]
    fn hosts_keep_their_incarnation_and_their_notes() {
        let dir = tempfile::tempdir().unwrap();
        let incarnation = open(dir.path()).unwrap().incarnation();
        let host = open(dir.path()).unwrap();
        assert_eq!(host.incarnation(), incarnation);
        let child = CellId { tenant: 3, id: 9 };
        let origin = Origin { parent: CellId { tenant: 3, id: 1 }, at: 4 };
        let entry =
            Entry { child, origin, path: dir.path().join("store"), bucket: "memory:///e".into() };
        host.note(&entry).unwrap();
        std::fs::write(dir.path().join(CELLS).join("torn"), b"{\"child\"").unwrap();
        std::fs::write(dir.path().join(CELLS).join("staged.new"), b"{}").unwrap();
        assert_eq!(host.entries().unwrap(), vec![entry.clone()]);
        assert_eq!(std::fs::read_dir(dir.path().join(CELLS)).unwrap().count(), 1);
        assert_eq!(host.entry(child), Some(entry));
        host.forget(child);
        assert_eq!(host.entries().unwrap(), vec![]);
    }
}
