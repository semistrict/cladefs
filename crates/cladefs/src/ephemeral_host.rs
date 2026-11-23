// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a host keeps of its ephemeral cells (doc-25, TASK-698.3): its incarnation, which
//! their references name, and a journal of them, by which it retires the references of those it
//! holds no more as it starts (specs/ephemeral, `retireOnlyUnheld`).
//!
//! Its directory (`CLADEFSD_EPHEMERAL`), on the disk its ephemeral cells' cell directories are on,
//! holds:
//!
//! - `incarnation`: a random number made with the directory, new when its disk is lost: the host's
//!   ephemeral cells, their references naming it, are lost with it, and decommissioning it
//!   (`DecommissionHost`) retires those references;
//! - `cells/<TENANT>.<CELL>`, one for each ephemeral cell it may hold, written, durably, before
//!   the cell's reference: its parent and origin, its cell's path, the bucket its parent is
//!   kept in, and the cell's epoch; and the migration it is being taken by from another host, if
//!   it is (TASK-698.4), or that took it from here, the cell let go of (board 5730). Removed once
//!   the reference is;
//! - `accepted/<MIGRATION>`, one for each migration it accepted, so that it answers yes to a
//!   hand-over it accepted however late it is asked again, the cell gone on since
//!   (specs/ephemeral, `remembersAccepted`); kept until the migration's source, having let the
//!   cell go, asks no more and releases it (board 5734).
//!
//! Each of these files begins with a line naming their format, [`FORMAT`], the rest JSON: one of
//! another format, or not whole, which their durable writes never leave, is refused, failing the
//! call reading it and the daemon's start, never deleted or taken for none (board 8461): it is of
//! another build's, its schema changed, or corrupt, and taken for none it lost a migration under
//! way, or a reference no start then retired. Nothing migrates (AGENTS.md, "No migrations").
//!
//! As the daemon starts, before it serves, each journal entry whose cell is not there, an
//! ephemeral cell's of its cell, has its reference retired and goes: a fork its host crashed in,
//! or a deletion; and each a migration was taking here and never accepted, what it took dropped
//! with it.

use crate::CellId;
use anyhow::{Context as _, Error, bail};
use bucket_cell::layout::Origin;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The host's ephemeral cells' directory (see the module's docs).
pub(crate) struct Host {
    dir: PathBuf,
    incarnation: u64,
}

/// The first line of each file of the host's ephemeral cells' directory: its format (board 8461).
/// A change to what they hold, [`Entry`] and [`Migrating`] among it, is a new one.
pub(crate) const FORMAT: &str = "cladefs ephemeral host 1";

/// The error of a file of the host's ephemeral cells' directory of another format than
/// [`FORMAT`], an older one included: refused, as nothing reads it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a file of format {found:?}, not {FORMAT:?}: refused")]
pub(crate) struct Format {
    /// Its first line, its first 64 characters.
    pub found: String,
}

/// A journal entry: an ephemeral cell the host may hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub child: CellId,
    pub origin: Origin,
    pub path: PathBuf,
    pub bucket: String,
    /// The cell's epoch: its last migration's, each newer than every one before it, as the
    /// prepares of the hosts it was on carry it, so that a target tells a late prepare from the
    /// cell's next (board 5732). 0 for a cell forked here, never migrated.
    pub epoch: u64,
    /// The migration it is being taken by, or that took it from here, if any.
    pub migrating: Option<Migrating>,
}

/// A migration of a journal entry's cell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Migrating {
    /// Being taken here by migration `id`, its files staged beside its path ([`staging`]), until
    /// this host accepts it; `token` made as this host took its prepare, new to that take, which
    /// its hand-over must name (specs/ephemeral, `boundToPrepare`).
    Receiving { id: u64, token: u64 },
    /// Handed over by migration `id` to the peer at `peer`, which accepted it: its cell directory
    /// goes, then its reference, then this entry, `peer` told it may forget it accepted `id`
    /// (board 5730, board 5734).
    HandedOver { id: u64, peer: String },
}

impl Entry {
    /// The migration taking its cell here, if one is: its ID and token.
    pub fn receiving(&self) -> Option<(u64, u64)> {
        match self.migrating {
            Some(Migrating::Receiving { id, token }) => Some((id, token)),
            _ => None,
        }
    }
}

/// Where a migration `migration` to `path` stages the cell's files until its hand-over.
pub(crate) fn staging(path: &Path, migration: u64) -> PathBuf {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.migrating-{migration:x}"))
}

node_local::node_local! {
    /// The host's ephemeral cells' directory, once the daemon opened it ([`open`]).
    static HOST: Mutex<Option<Arc<Host>>> = Mutex::new(None);
}

/// Opens the host's ephemeral cells' directory at `dir`, made if it is not there with a new
/// incarnation, as this process's from now on.
pub(crate) fn open(dir: &Path) -> Result<Arc<Host>, Error> {
    for made in [CELLS, ACCEPTED] {
        let made = dir.join(made);
        host_fs::create_dir_all_durably(&made)
            .with_context(|| format!("making {}", made.display()))?;
    }
    let path = dir.join(INCARNATION);
    let incarnation = match read(&path)? {
        Some(incarnation) => incarnation,
        None => {
            let incarnation = rand::random::<u64>() | 1;
            write_durably(&path, &encode(&incarnation)?)?;
            incarnation
        }
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
const ACCEPTED: &str = "accepted";
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
        write_durably(&self.entry_path(entry.child), &encode(entry)?)
    }

    /// Forgets `child`, its reference gone: durably, so that no crash brings its entry back
    /// (TASK-714). Failing, it says so (board 5721: an entry left, said nowhere, had this host
    /// refuse every later migration of the cell to it, the cell "here"), for its caller to try
    /// again: forgetting an entry gone already syncs its directory still, finishing a removal whose
    /// sync failed.
    pub fn forget(&self, child: CellId) -> Result<(), Error> {
        let path = self.entry_path(child);
        let forgotten = match host_fs::remove_file(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            removed => removed,
        };
        let forgotten = forgotten.and_then(|()| host_fs::sync_dir(self.dir.join(CELLS)));
        forgotten.with_context(|| format!("forgetting {}", path.display()))
    }

    /// Remembers, durably, that it accepted migration `migration`, of the cell `child`, until its
    /// source releases it.
    pub fn accept(&self, migration: u64, child: CellId) -> Result<(), Error> {
        let path = self.dir.join(ACCEPTED).join(format!("{migration:x}"));
        write_durably(&path, &encode(&child)?)
    }

    /// The cell migration `migration` took, if this host accepted it. Failing if its acceptance is
    /// there but unread, its disk failing, not whole, which its durable write never leaves, or of
    /// another format: taken for none, the cell would go unnoted as taken, and a crash lose it
    /// (boards 6908, 8461).
    pub fn accepted_cell(&self, migration: u64) -> Result<Option<CellId>, Error> {
        read(&self.dir.join(ACCEPTED).join(format!("{migration:x}")))
    }

    /// Whether it accepted migration `migration`.
    pub fn accepted(&self, migration: u64) -> bool {
        host_fs::exists(self.dir.join(ACCEPTED).join(format!("{migration:x}")))
    }

    /// Forgets, durably, that it accepted migration `migration`: its source let the cell go, and
    /// asks no more (board 5734).
    pub fn release(&self, migration: u64) -> Result<(), Error> {
        let path = self.dir.join(ACCEPTED).join(format!("{migration:x}"));
        match host_fs::remove_file(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            removed => Ok(removed.and_then(|()| host_fs::sync_dir(self.dir.join(ACCEPTED)))?),
        }
    }

    /// The entry of the cell migration `migration` is taking here, if one is.
    pub fn receiving(&self, migration: u64) -> Result<Option<Entry>, Error> {
        let taking = |entry: &Entry| entry.receiving().is_some_and(|(id, _)| id == migration);
        Ok(self.entries()?.into_iter().find(taking))
    }

    /// The entries of the cells handed over from `path`, not yet let go of.
    pub fn handed_over(&self, path: &Path) -> Result<Vec<Entry>, Error> {
        let handed = |entry: &Entry| {
            matches!(entry.migrating, Some(Migrating::HandedOver { .. })) && entry.path == path
        };
        Ok(self.entries()?.into_iter().filter(handed).collect())
    }

    /// The ephemeral cell `child`'s entry, if it has one.
    /// cladefs: failing if it is unread, its disk failing (board 5748), not whole or of another
    /// format (board 8461), rather than as none, which a migration's prepare would write over.
    pub fn entry(&self, child: CellId) -> Result<Option<Entry>, Error> {
        read(&self.entry_path(child))
    }

    /// Its journal's entries. A note staged (`.new`), which a crash in [`write_durably`] may
    /// leave, goes; one there but unread, not whole or of another format fails it, removed never
    /// (boards 5748, 8461): removed, its cell's entry went, the cell noted on no host, its
    /// reference never retired.
    pub fn entries(&self) -> Result<Vec<Entry>, Error> {
        let mut entries = Vec::new();
        for found in host_fs::read_dir(self.dir.join(CELLS))? {
            let path = found?.path();
            if path.extension().is_some_and(|extension| extension == "new") {
                host_fs::remove_file(&path)?;
                continue;
            }
            // Listed, then gone: forgotten meanwhile, no entry.
            entries.extend(read(&path)?);
        }
        Ok(entries)
    }

    /// Retires the references of the ephemeral cells its journal names that it holds no more: its
    /// cell at its path gone, or not an ephemeral cell's of its cell; or a migration's it was
    /// taking, never accepted, which the daemon's going stopped. Those handed over it leaves to
    /// the migration's letting go (`cladefsd::migration`), which runs first. Returns how many. An entry
    /// whose retiring fails, its disk failing, is said and left: to the next start, or to a
    /// migration's prepare here, which takes an entry of a cell this host holds not for none
    /// (board 5721); the daemon starts whatever one entry's disk does.
    pub async fn retire_stale(&self) -> Result<usize, Error> {
        let mut retired = 0;
        for entry in self.entries()? {
            let child = entry.child;
            match self.retire_if_stale(entry).await {
                Ok(gone) => retired += usize::from(gone),
                Err(err) => log::warn!("cladefsd: retiring {child}, held no more: {err:#}"),
            }
        }
        Ok(retired)
    }

    /// [`Host::retire_stale`]'s of `entry`: whether it retired it.
    async fn retire_if_stale(&self, entry: Entry) -> Result<bool, Error> {
        if let Some(Migrating::HandedOver { .. }) = entry.migrating {
            return Ok(false);
        }
        // A migration it accepted, crashed before it noted the cell taken: taken, as the
        // hand-over did (TASK-713); the cell its, as its source let it go.
        if entry.receiving().is_some_and(|(migration, _)| self.accepted(migration)) {
            let taken = Entry { migrating: None, ..entry };
            self.note(&taken)?;
            if self.holds(&taken)? {
                return Ok(false);
            }
            self.retire(&taken).await?;
            return Ok(true);
        }
        // A migration being taken, which the daemon's going stopped: given up (TASK-698.4).
        if let Some((migration, _)) = entry.receiving() {
            self.drop_moved(&entry)?;
            // Its entry and reference, then its files: dropped part way, a staging an entry names
            // is one a retry of its migration may take for whole (board 5721).
            self.retire(&entry).await?;
            let staged = staging(&entry.path, migration);
            if host_fs::exists(&staged)
                && let Err(err) = host_fs::remove_dir_all(&staged)
            {
                log::warn!("cladefsd: dropping {}, given up: {err}", staged.display());
            }
            return Ok(true);
        } else if self.holds(&entry)? {
            return Ok(false);
        }
        self.retire(&entry).await?;
        Ok(true)
    }

    /// Retires `entry`'s reference, then the entry: retiring it again, after either failed,
    /// finishes it.
    pub async fn retire(&self, entry: &Entry) -> Result<(), Error> {
        let bucket = crate::host_cell::bucket(&entry.bucket)?;
        let (origin, child) = (entry.origin, entry.child);
        let unreferred =
            bucket_cell::lifecycle::unrefer_ephemeral(&*bucket, origin, self.incarnation, child);
        unreferred.await.map_err(bucket_cell::Error::from)?;
        self.forget(child)
    }

    /// Removes the cell directory at the path of `entry`, a migration's being taken here, if it is
    /// its cell's: taken into place already, its acceptance not (boards 5721, 5728). Its source,
    /// letting the cell go only once it is accepted, holds it still; left, a retry took it after
    /// its reference was retired, and it kept the path from every later migration to it. Its
    /// removal no more held at its first step.
    pub fn drop_moved(&self, entry: &Entry) -> Result<(), Error> {
        if self.holds(entry)? {
            log::warn!(
                "cladefsd: {}: a migration's cell, never accepted, removed",
                entry.path.display()
            );
            let path = entry.path.clone();
            local_cell::upload::blocking(move || local_cell::remove_cell_dir(&path))?;
        }
        Ok(())
    }

    /// Whether it holds `entry`'s cell: its cell directory there, whole, its ephemeral cell's.
    /// Failing to read it says neither (board 5721): an entry retired as a reading of its cell
    /// failed, its disk failing, left a cell this host held with no entry and no reference.
    pub fn holds(&self, entry: &Entry) -> Result<bool, Error> {
        local_cell::upload::blocking(|| held(entry))
            .with_context(|| format!("reading {}", entry.path.display()))
    }
}

/// Why the cell directory at `dir` is not `entry`'s ephemeral cell, whole, if it is not: opened as
/// its opening opens it, its description's format checked (`local_cell::Format`), its cell, the
/// origin its local checkpoint names, and the bucket it names `entry`'s (board 8455). A migration's
/// target takes a staged cell into place only if it is (`cladefsd::migration`): taken otherwise,
/// a cell of another build's format, or one naming another bucket, would be accepted, its source
/// then letting go of the only copy, and refused at its first opening here.
pub(crate) fn not_its(dir: &Path, entry: &Entry) -> Result<Option<String>, Error> {
    if local_cell::unmade_host(dir) {
        return Ok(Some(format!("{} is no cell made whole", dir.display())));
    }
    let Some(checkpoint) = local_cell::host_local_checkpoint(dir)? else {
        return Ok(Some(format!("{} is no ephemeral cell's", dir.display())));
    };
    let cell = local_cell::host_cell(dir)?;
    if cell != entry.child {
        return Ok(Some(format!("{} is cell {cell}'s, not {}'s", dir.display(), entry.child)));
    }
    let parent = checkpoint.parent();
    let origin = Origin { parent: parent.cell, at: parent.at, epoch: parent.epoch };
    if origin != entry.origin {
        return Ok(Some(format!("its origin is {origin:?}, not {:?}", entry.origin)));
    }
    let bucket = local_cell::host_bucket(dir)?;
    if bucket.as_deref() != Some(entry.bucket.as_str()) {
        return Ok(Some(format!("it names bucket {bucket:?}, not its parent's")));
    }
    Ok(None)
}

/// Whether the cell directory at `entry`'s path is its ephemeral cell's, whole.
fn held(entry: &Entry) -> Result<bool, Error> {
    if !host_fs::exists(&entry.path) || local_cell::unmade_host(&entry.path) {
        return Ok(false);
    }
    if local_cell::host_local_checkpoint(&entry.path)?.is_none() {
        return Ok(false);
    }
    Ok(local_cell::host_cell(&entry.path)? == entry.child)
}

/// `value` as a file of the host's ephemeral cells' directory holds it: its format's line, then
/// its JSON.
fn encode(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    let mut bytes = format!("{FORMAT}\n").into_bytes();
    serde_json::to_writer(&mut bytes, value)?;
    Ok(bytes)
}

/// What the file at `path` of the host's ephemeral cells' directory holds, as [`encode`] wrote it:
/// none if it is not there. Failing if it is unread, of another format ([`Format`]), or not whole
/// (board 8461). Its bytes are untrusted (AGENTS.md, Trust boundaries): read whole, split once.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let bytes = match host_fs::read(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        read => read.with_context(|| format!("reading {}", path.display()))?,
    };
    let mut lines = bytes.splitn(2, |&byte| byte == b'\n');
    let found = lines.next().unwrap_or_default();
    if found != FORMAT.as_bytes() {
        let found = String::from_utf8_lossy(found).chars().take(64).collect();
        return Err(Error::new(Format { found }).context(format!("reading {}", path.display())));
    }
    let value = serde_json::from_slice(lines.next().unwrap_or_default());
    Ok(Some(value.with_context(|| format!("reading {}", path.display()))?))
}

/// Writes `bytes` to a file at `path`, whole, durably, over any there.
fn write_durably(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let staged = path.with_extension("new");
    let mut file = host_fs::File::create(&staged)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    host_fs::rename(&staged, path)?;
    let Some(parent) = path.parent() else { bail!("{} has no directory", path.display()) };
    host_fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's directory keeps its incarnation across openings, and its journal its notes, until
    /// forgotten; a note staged goes as it is listed. Board 6908: an acceptance there but not whole
    /// fails, never read as none.
    #[test]
    fn hosts_keep_their_incarnation_and_their_notes() {
        let dir = tempfile::tempdir().unwrap();
        let incarnation = open(dir.path()).unwrap().incarnation();
        let host = open(dir.path()).unwrap();
        assert_eq!(host.incarnation(), incarnation);
        let child = CellId { tenant: 3, id: 9 };
        let entry = entry(dir.path(), child);
        host.note(&entry).unwrap();
        host_fs::write(dir.path().join(CELLS).join("staged.new"), b"{}").unwrap();
        assert_eq!(host.entries().unwrap(), vec![entry.clone()]);
        assert_eq!(host_fs::read_dir(dir.path().join(CELLS)).unwrap().count(), 1);
        assert_eq!(host.entry(child).unwrap(), Some(entry));
        host.forget(child).unwrap();
        assert_eq!(host.entries().unwrap(), vec![]);
        assert!(!host.accepted(0x3c));
        host.accept(0x3c, child).unwrap();
        assert!(open(dir.path()).unwrap().accepted(0x3c));
        assert_eq!(host.accepted_cell(0x3c).unwrap(), Some(child));
        assert_eq!(host.accepted_cell(0x3d).unwrap(), None);
        let file = dir.path().join(ACCEPTED).join("3e");
        host_fs::write(&file, format!("{FORMAT}\n{{\"tenant").as_bytes()).unwrap();
        let unread = host.accepted_cell(0x3e).map_err(|err| format!("{err:#}"));
        assert_eq!(
            unread,
            Err(format!(
                "reading {}: EOF while parsing a string at line 1 column 8",
                file.display()
            ))
        );
    }

    /// An entry of `child`'s, its cell at `CELL` in `dir`.
    fn entry(dir: &Path, child: CellId) -> Entry {
        Entry {
            child,
            origin: Origin { parent: CellId { tenant: 3, id: 1 }, at: 4, epoch: 1 },
            path: dir.join("cell"),
            bucket: "memory:///e".into(),
            epoch: 0,
            migrating: None,
        }
    }

    /// Board 8461: a file of the host's directory of another format, an older build's, or not
    /// whole, which a durable write never leaves, fails what reads it, the daemon's start among
    /// them (its retiring of references, and its letting go of cells handed over, which list the
    /// entries as `entries` does, failing the boot): never deleted, never
    /// none, the cell directory it names untouched. The cases: each file without the format's line
    /// (as every build before it wrote them), of another format, of a schema this build's does
    /// not know (a field added) and torn after the format's line.
    #[test]
    fn files_of_another_format_are_refused_never_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let host = open(dir.path()).unwrap();
        let child = CellId { tenant: 3, id: 9 };
        let noted = entry(dir.path(), child);
        host_fs::create_dir(&noted.path).unwrap();
        host_fs::write(noted.path.join("store"), b"kept").unwrap();
        let json = serde_json::to_string(&noted).unwrap();
        let schema = json.replacen('{', "{\"tier\":2,", 1);
        let note = host.entry_path(child);
        let refusal = |found: &str| {
            format!(
                "reading {}: a file of format {found:?}, not {FORMAT:?}: refused",
                note.display()
            )
        };
        let cases = [
            (json.clone(), refusal(&json[..64])),
            (format!("cladefs ephemeral host 0\n{json}"), refusal("cladefs ephemeral host 0")),
            (
                format!("{FORMAT}\n{schema}"),
                format!(
                    "reading {}: unknown field `tier`, expected one of `child`, `origin`, `path`, \
                     `bucket`, `epoch`, `migrating` at line 1 column 7",
                    note.display()
                ),
            ),
            (
                format!("{FORMAT}\n{}", &json[..20]),
                format!(
                    "reading {}: EOF while parsing an object at line 1 column 20",
                    note.display()
                ),
            ),
        ];
        for (bytes, refused) in cases {
            host_fs::write(&note, &bytes).unwrap();
            fn said<T>(result: Result<T, Error>) -> Result<(), String> {
                result.map(drop).map_err(|err| format!("{err:#}"))
            }
            assert_eq!(said(host.entry(child)), Err(refused.clone()), "{bytes}");
            assert_eq!(said(host.entries()), Err(refused.clone()), "{bytes}");
            let starting = futures::executor::block_on(host.retire_stale());
            assert_eq!(said(starting), Err(refused), "{bytes}");
            assert_eq!(host_fs::read(&note).unwrap(), bytes.as_bytes(), "the note kept");
            assert_eq!(host_fs::read(noted.path.join("store")).unwrap(), b"kept");
        }

        // An acceptance, and an incarnation, of an older build's: refused alike.
        let accepted = dir.path().join(ACCEPTED).join("3e");
        host_fs::write(&accepted, serde_json::to_vec(&child).unwrap()).unwrap();
        let refused = host.accepted_cell(0x3e).map_err(|err| format!("{err:#}"));
        let found = serde_json::to_string(&child).unwrap();
        assert_eq!(
            refused,
            Err(format!(
                "reading {}: a file of format {found:?}, not {FORMAT:?}: refused",
                accepted.display()
            ))
        );
        let incarnation = dir.path().join(INCARNATION);
        host_fs::write(&incarnation, b"12345").unwrap();
        let refused = open(dir.path()).map(drop).map_err(|err| format!("{err:#}"));
        assert_eq!(
            refused,
            Err(format!(
                "reading {}: a file of format \"12345\", not {FORMAT:?}: refused",
                incarnation.display()
            ))
        );
        assert_eq!(host_fs::read(&incarnation).unwrap(), b"12345");
    }
}
