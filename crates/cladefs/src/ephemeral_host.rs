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
//!   (`DecommissionHost`, `cladefsctl decommission-host`) retires those references. Written in
//!   base 32 wherever it is printed ([`said`]): the daemon's log, `GetStats`, cladefsctl (board
//!   8521, hotbed question 480);
//! - `cells/<TENANT>.<CELL>`, padded base 32 ([`base32_id::Padded`]), one for each ephemeral cell it may hold, written, durably, before
//!   the cell's reference: its parent and origin, its cell's path, the bucket its parent is
//!   kept in, and the cell's epoch; and the migration it is being taken by from another host, if
//!   it is (TASK-698.4), or that took it from here, the cell let go of (board 5730). Removed once
//!   the reference is;
//! - `accepted/<MIGRATION>`, padded base 32, one for each migration it accepted, so that it answers yes to a
//!   hand-over it accepted however late it is asked again, the cell gone on since
//!   (specs/ephemeral, `remembersAccepted`); kept until the migration's source, having let the
//!   cell go, asks no more and releases it (board 5734).
//!
//! Each of these files begins with a line naming its version of the family [`FORMAT`], the rest
//! JSON. The family is frozen (doc-28, board 10048): every later build reads every version of it
//! from 2 on, its corpus under `formats/ephemeral-host/`. One of a version this build doesn't
//! read, or not whole, which their durable writes never leave, is refused, failing the call
//! reading it and the daemon's start, never deleted or taken for none (board 8461): taken for
//! none it lost a migration under way, or a reference no start then retired.
//!
//! As the daemon starts, before it serves, each journal entry whose cell is not there, an
//! ephemeral cell's of its cell, has its reference retired and goes: a fork its host crashed in,
//! or a deletion; and each a migration was taking here and never accepted, what it took dropped
//! with it. First, its incarnation's tombstone is looked for in the bucket of each entry's
//! reference: found, the host was decommissioned, its cells' references retired, what they read
//! perhaps collected, and the daemon refuses to start (board 8519) until its operator moves the
//! directory aside, its cells with it, for a new incarnation; as each ephemeral fork of it is
//! refused (`bucket_cell::lifecycle::refer_ephemeral`, `ESTALE`).

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

/// The family of the files of the host's ephemeral cells' directory (doc-28, board 10048), whose
/// first line is [`HEADER`] and the version. A change to what they hold, [`Entry`] and
/// [`Migrating`] among it, is a new version, whose reader still reads every one before.
/// 1: board 8461, its names in hexadecimal. 2: its names in padded base 32
/// ([`base32_id::Padded`]), as every other ID is written; frozen, nothing reads 1's.
pub(crate) const FORMAT: formats::Format = formats::Format::new("ephemeral host", 2, 2);

/// What each file's first line says before its version.
const HEADER: &str = "cladefs ephemeral host ";

/// The error of a file of the host's ephemeral cells' directory whose first line names no
/// version of [`FORMAT`]: refused (a version named, but not read, is [`formats::Refused`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a file of no ephemeral host format, its first line {found:?}: refused")]
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
    log::info!("{}", opened(dir, incarnation));
    Ok(host)
}

/// What the daemon logs as it opens the host's ephemeral cells' directory at `dir`, of
/// incarnation `incarnation`: its last word the incarnation, as an operator gives it to
/// `cladefsctl decommission-host` ([`said`]).
pub(crate) fn opened(dir: &Path, incarnation: u64) -> String {
    format!("cladefsd: ephemeral cells at {}, incarnation {}", dir.display(), said(incarnation))
}

/// An incarnation as everything prints it, and an operator types it (board 8521, hotbed question
/// 480): in base 32, as cells' and tenants' IDs are ([`base32_id::Short`]).
pub(crate) fn said(incarnation: u64) -> base32_id::Short {
    base32_id::Short(incarnation)
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
        let (tenant, id) = (base32_id::Padded(child.tenant), base32_id::Padded(child.id));
        self.dir.join(CELLS).join(format!("{tenant}.{id}"))
    }

    fn accepted_path(&self, migration: u64) -> PathBuf {
        self.dir.join(ACCEPTED).join(base32_id::Padded(migration).to_string())
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
        write_durably(&self.accepted_path(migration), &encode(&child)?)
    }

    /// The cell migration `migration` took, if this host accepted it. Failing if its acceptance is
    /// there but unread, its disk failing, not whole, which its durable write never leaves, or of
    /// another format: taken for none, the cell would go unnoted as taken, and a crash lose it
    /// (boards 6908, 8461).
    pub fn accepted_cell(&self, migration: u64) -> Result<Option<CellId>, Error> {
        read(&self.accepted_path(migration))
    }

    /// Whether it accepted migration `migration`.
    pub fn accepted(&self, migration: u64) -> bool {
        host_fs::exists(self.accepted_path(migration))
    }

    /// Forgets, durably, that it accepted migration `migration`: its source let the cell go, and
    /// asks no more (board 5734).
    pub fn release(&self, migration: u64) -> Result<(), Error> {
        match host_fs::remove_file(self.accepted_path(migration)) {
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
    /// cladefs: fails first if the host's incarnation was decommissioned, as the bucket of any
    /// entry's reference says ([`Host::refuse_decommissioned`], board 8519).
    pub async fn retire_stale(&self) -> Result<usize, Error> {
        let entries = self.entries()?;
        self.refuse_decommissioned(&entries).await?;
        let mut retired = 0;
        for entry in entries {
            let child = entry.child;
            match self.retire_if_stale(entry).await {
                Ok(gone) => retired += usize::from(gone),
                Err(err) => log::warn!("cladefsd: retiring {child}, held no more: {err:#}"),
            }
        }
        Ok(retired)
    }

    /// Fails if its incarnation was decommissioned, as the tombstone in the bucket of any of
    /// `entries`' references says (board 8519): its cells' references retired, what they read
    /// may be collected, and its reads of it fail or find it gone; going on, it would serve them
    /// on as if whole. Its bucket failing to say fails it too: not knowing is no answer (AGENTS.md,
    /// No silent fallbacks). One request a bucket.
    async fn refuse_decommissioned(&self, entries: &[Entry]) -> Result<(), Error> {
        let mut asked = std::collections::BTreeSet::new();
        for entry in entries {
            let parent = entry.origin.parent;
            if !asked.insert(bucket_cell::layout::ephemeral_bucket(parent)) {
                continue;
            }
            let bucket = crate::host_cell::bucket(&entry.bucket)?;
            let found =
                bucket_cell::lifecycle::decommissioned(&*bucket, self.incarnation, parent).await;
            if found.map_err(bucket_cell::Error::from)? {
                bail!(
                    "incarnation {} of {} was decommissioned (DecommissionHost): its ephemeral \
                     cells' references are retired, and what they read may be collected; move \
                     the directory aside, its cells with it, for the daemon to start of a new \
                     incarnation",
                    said(self.incarnation),
                    self.dir.display()
                );
            }
        }
        Ok(())
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

/// The largest description of a cell directory [`not_its`] reads: a description is a few lines.
const DESCRIPTION_MOST: u64 = 64 << 10;

/// Why the cell directory at `dir` is not `entry`'s ephemeral cell, whole, if it is not: its
/// description read as its opening reads it, its format checked (`local_cell::Format`), an
/// ephemeral cell's, of `entry`'s cell, naming `entry`'s bucket (board 8455). A migration's target
/// takes a staged cell into place only if it is (`cladefsd::migration`): taken otherwise, a cell
/// of another build's format, or one naming another bucket, would be accepted, its source then
/// letting go of the only copy, and refused at its first opening here.
///
/// What a peer staged is untrusted (AGENTS.md, Trust boundaries): only its description is read,
/// and only if it is no larger than [`DESCRIPTION_MOST`], never opened as a `HostDir` (which would
/// count its files in the host's disk, board 8455's review) nor its local checkpoint read whole,
/// whose length the peer chose.
pub(crate) fn not_its(dir: &Path, entry: &Entry) -> Result<Option<String>, Error> {
    if local_cell::unmade_host(dir) {
        return Ok(Some(format!("{} is no cell made whole", dir.display())));
    }
    let description = dir.join(local_cell::DESCRIPTION);
    let len = host_fs::metadata(&description)?.len();
    if len > DESCRIPTION_MOST {
        return Ok(Some(format!("its description is {len} bytes, past {DESCRIPTION_MOST}")));
    }
    if !local_cell::host_ephemeral(dir)? {
        return Ok(Some(format!("{} is no ephemeral cell's", dir.display())));
    }
    let cell = local_cell::host_cell(dir)?;
    if cell != entry.child {
        return Ok(Some(format!("{} is cell {cell}'s, not {}'s", dir.display(), entry.child)));
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

/// `value` as a file of the host's ephemeral cells' directory holds it: its version's line, then
/// its JSON.
fn encode(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    let mut bytes = format!("{HEADER}{}\n", FORMAT.current).into_bytes();
    serde_json::to_writer(&mut bytes, value)?;
    Ok(bytes)
}

/// What the file at `path` of the host's ephemeral cells' directory holds, as [`encode`] wrote it:
/// none if it is not there. Failing if it is unread, or as [`decode`] fails.
fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let bytes = match host_fs::read(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        read => read.with_context(|| format!("reading {}", path.display()))?,
    };
    Ok(Some(decode(&bytes).with_context(|| format!("reading {}", path.display()))?))
}

/// What a file of the host's ephemeral cells' directory holds, `bytes`: failing if its first line
/// names no version of [`FORMAT`] ([`Format`]), one this build doesn't read
/// ([`formats::Refused`]), or if it is not whole (board 8461). Its bytes are untrusted (AGENTS.md,
/// Trust boundaries): split once, the version's digits alone parsed.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    let mut lines = bytes.splitn(2, |&byte| byte == b'\n');
    let found = lines.next().unwrap_or_default();
    // Not UTF-8, or no number: no version, refused below, naming what was found.
    let version = std::str::from_utf8(found)
        .ok()
        .and_then(|line| line.strip_prefix(HEADER))
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| digits.parse::<u32>().ok());
    let Some(version) = version else {
        let found = String::from_utf8_lossy(found).chars().take(64).collect();
        return Err(Error::new(Format { found }));
    };
    let json = lines.next().unwrap_or_default();
    FORMAT.check(version)?;
    // Version 2's types are the current ones. A later version keeps them, as they are, decodes
    // the files of 2 into them here, and converts them to its own.
    Ok(serde_json::from_slice(json)?)
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
        let file = host.accepted_path(0x3e);
        assert_eq!(file, dir.path().join(ACCEPTED).join("000000000001y"));
        host_fs::write(&file, format!("{HEADER}2\n{{\"tenant").as_bytes()).unwrap();
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
        assert_eq!(note, dir.path().join(CELLS).join("0000000000003.0000000000009"));
        let refusal = |found: &str| {
            format!(
                "reading {}: a file of no ephemeral host format, its first line {found:?}: refused",
                note.display()
            )
        };
        let unread = |version: u32| {
            format!(
                "reading {}: ephemeral host version {version}: this build reads version 2 alone",
                note.display()
            )
        };
        let cases = [
            (json.clone(), refusal(&json[..64])),
            (format!("cladefs ephemeral host\n{json}"), refusal("cladefs ephemeral host")),
            (format!("cladefs ephemeral host +2\n{json}"), refusal("cladefs ephemeral host +2")),
            (
                format!("cladefs ephemeral host 4294967296\n{json}"),
                refusal("cladefs ephemeral host 4294967296"),
            ),
            (format!("cladefs ephemeral host 0\n{json}"), unread(0)),
            (format!("cladefs ephemeral host 1\n{json}"), unread(1)),
            (format!("cladefs ephemeral host 3\n{json}"), unread(3)),
            (format!("cladefs ephemeral host {}\n{json}", u32::MAX), unread(u32::MAX)),
            (
                format!("{HEADER}2\n{schema}"),
                format!(
                    "reading {}: unknown field `tier`, expected one of `child`, `origin`, `path`, \
                     `bucket`, `epoch`, `migrating` at line 1 column 7",
                    note.display()
                ),
            ),
            (
                format!("{HEADER}2\n{}", &json[..20]),
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
        let accepted = host.accepted_path(0x3e);
        host_fs::write(&accepted, serde_json::to_vec(&child).unwrap()).unwrap();
        let refused = host.accepted_cell(0x3e).map_err(|err| format!("{err:#}"));
        let found = serde_json::to_string(&child).unwrap();
        assert_eq!(
            refused,
            Err(format!(
                "reading {}: a file of no ephemeral host format, its first line {found:?}: refused",
                accepted.display()
            ))
        );
        let incarnation = dir.path().join(INCARNATION);
        host_fs::write(&incarnation, b"12345").unwrap();
        let refused = open(dir.path()).map(drop).map_err(|err| format!("{err:#}"));
        assert_eq!(
            refused,
            Err(format!(
                "reading {}: a file of no ephemeral host format, its first line \"12345\": \
                 refused",
                incarnation.display()
            ))
        );
        assert_eq!(host_fs::read(&incarnation).unwrap(), b"12345");
    }

    /// The family's golden corpus (doc-28): each kind of file, each kind of entry, as every
    /// version from 2 wrote them, read by this build.
    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<Files>(env!("CARGO_MANIFEST_DIR"));
    }

    struct Files;

    /// An entry of the corpus's, migrating as `migrating` says.
    fn golden_entry(migrating: Option<Migrating>) -> Entry {
        Entry {
            child: CellId { tenant: 0x2f, id: 0x1_0000_0001 },
            origin: Origin { parent: CellId { tenant: 0x2f, id: 7 }, at: 12, epoch: 3 },
            path: "/var/lib/cladefs/cells/c1".into(),
            bucket: "gs://cladefs-golden/cells".into(),
            epoch: 5,
            migrating,
        }
    }

    impl formats::golden::Family for Files {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            let receiving = Migrating::Receiving { id: 0x51, token: u64::MAX };
            let handed = Migrating::HandedOver { id: 0x52, peer: "10.0.0.7:7777".into() };
            fn encoded(value: &impl Serialize) -> Vec<u8> {
                encode(value).expect("encoding a corpus's file")
            }
            vec![
                ("incarnation", encoded(&0xfedc_ba98_7654_3211_u64)),
                ("cell", encoded(&golden_entry(None))),
                ("cell-receiving", encoded(&golden_entry(Some(receiving)))),
                ("cell-handed-over", encoded(&golden_entry(Some(handed)))),
                ("accepted", encoded(&CellId { tenant: 0x2f, id: 9 })),
            ]
        }

        fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
            // Version 2's decoder, of files saying 2: its types are the current ones.
            if version != 2 || !bytes.starts_with(format!("{HEADER}{version}\n").as_bytes()) {
                return Err(format!("not a file of version {version}"));
            }
            Self::read_as(case, bytes)
        }

        fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
            Self::read_as(case, bytes).map(drop)
        }
    }

    impl Files {
        /// `bytes` decoded as the file `case` names, by this build's [`decode`].
        fn read_as(case: &str, bytes: &[u8]) -> Result<String, String> {
            fn said<T: std::fmt::Debug>(decoded: Result<T, Error>) -> Result<String, String> {
                decoded.map(|value| format!("{value:#?}")).map_err(|err| format!("{err:#}"))
            }
            match case {
                "incarnation" => said(decode::<u64>(bytes)),
                "accepted" => said(decode::<CellId>(bytes)),
                _ if case.starts_with("cell") => said(decode::<Entry>(bytes)),
                _ => Err(format!("no file {case}")),
            }
        }
    }
}
