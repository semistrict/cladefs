// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a store's files take of the host's disk, counted where the store writes them
//! (backlog doc-22, TASK-539.4): its **footprint**. Every change a store makes to its directory
//! goes through one layer ([`crate::Metered`]), which counts it here as the store makes it: a file
//! grown by a write, made, linked, renamed, given away or removed; a directory made. Nothing walks
//! a directory to count it, nor writes back the store page cache to: a write is counted as the
//! cache takes it, and a file removed uncounted as it goes, whether or not its pages reached the
//! disk.
//!
//! **What is counted.** Each file's length, rounded up to a granule
//! ([`bucket_cell::host_disk::counted`]), once however many names it has, and each directory a
//! granule; and, of that, its **unframed** bytes: files with a name under `open-segments/` or
//! `open-extents/`, raw data whose framing, or sealing in place, is still to come, which the
//! store's own work, never refused, may add to. A footprint charges its disk's counters as its own
//! ([`HostDisk::stores`], [`HostDisk::unframed`]).
//!
//! **While a store is open, and closed.** The first opening of a store, while no other opening of
//! it is alive, lists its directory once and counts what it finds, against what was counted of
//! it before; the last lets its names go but keeps its totals, so that a store closed stays
//! counted, and one changed while closed, by another process, is counted anew at its next
//! opening. A store of the host's has one footprint however many times it is opened, by its
//! directory's device and inode ([`of_host`]); its removal ([`crate::remove_store_dir`]) forgets
//! it. A directory in memory has one of its own.
//!
//! **Allotments.** A directory a tenant is granted is an [`Allotment`]: the footprints of the stores
//! in it summed, with the tenant's limit, and every allotment's together ([`total`]) with every
//! tenant's limit (TASK-539.5). A store's footprint is its allotment's whose directory holds it,
//! the nearest; one in no allotment's is no tenant's, counted on its disk alone. An allotment made
//! counts the stores in its directory not counted yet, closed, by one listing of each.

use crate::dir::{Dir, DirFile};
use crate::layout::{OPEN_EXTENTS, OPEN_SEGMENTS};
use crate::sync::Mutex;
use bucket_cell::host_disk::{self, Count, HostDisk, Tally};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What a store's files take of its disk. See the module docs.
pub struct Footprint {
    /// Its directory's path, canonical, if it is the host's.
    path: Option<PathBuf>,
    /// The disk it charges as it is charged.
    disk: Option<Arc<HostDisk>>,
    /// Its files' lengths and its directories, counted.
    tally: Tally,
    /// The allotment it charges as it is charged, if any's directory holds it.
    allotment: Mutex<Option<Arc<Allotment>>>,
    /// Its files and directories by name, while its store is open.
    opened: Mutex<Opened>,
}

/// A footprint's names, while any opening of its store is alive.
#[derive(Default)]
struct Opened {
    /// The openings alive.
    openings: usize,
    /// Its files, by name: a file at two names is one.
    files: HashMap<String, Arc<FileLen>>,
    /// Its directories, by path.
    dirs: BTreeSet<String>,
}

impl Footprint {
    /// A footprint charging `disk`, of a store whose directory is at `path`, if it is the host's:
    /// counting nothing until its store's first opening lists it.
    pub(crate) fn new(path: Option<PathBuf>, disk: Option<Arc<HostDisk>>) -> Arc<Self> {
        Arc::new(Self {
            path,
            disk,
            tally: Tally::default(),
            allotment: Mutex::new(None),
            opened: Mutex::default(),
        })
    }

    /// The bytes its store's files take, counted.
    pub fn bytes(&self) -> u64 {
        self.tally.bytes()
    }

    /// Of those, the bytes of its unframed files.
    pub fn unframed(&self) -> u64 {
        self.tally.unframed()
    }

    /// The allotment it is in, if any is.
    pub fn allotment(&self) -> Option<Arc<Allotment>> {
        self.allotment.lock().unwrap().clone()
    }

    /// Its directory's path, canonical, if it is the host's.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The disk its store is on.
    pub fn disk(&self) -> Option<&Arc<HostDisk>> {
        self.disk.as_ref()
    }

    /// Has `change` count in its tally, its allotment's and every allotment's, if it is in one,
    /// and its disk's: under its allotment's lock, so that a store moved to another allotment
    /// moves what it counted whole.
    fn counting(&self, change: impl Fn(&Tally)) {
        let allotment = self.allotment.lock().unwrap();
        change(&self.tally);
        if let Some(allotment) = &*allotment {
            change(&allotment.tally);
            change(&TOTAL.tally);
        }
        if let Some(disk) = &self.disk {
            change(&disk.stores);
        }
    }

    /// Counts `bytes` more, `unframed` if so.
    fn charge(&self, bytes: u64, unframed: bool) {
        self.counting(|tally| tally.charge(bytes, unframed));
    }

    /// Counts `bytes` fewer, `unframed` if so.
    fn discharge(&self, bytes: u64, unframed: bool) {
        self.counting(|tally| tally.discharge(bytes, unframed));
    }

    /// Counts `bytes` of it unframed, if `unframed`, or framed: they are counted either way.
    fn reframe(&self, bytes: u64, unframed: bool) {
        self.counting(|tally| tally.reframe(bytes, unframed));
    }

    /// Counts it in `allotment`'s, in place of the one it was in.
    fn allot(&self, allotment: &Arc<Allotment>) {
        let mut allotted = self.allotment.lock().unwrap();
        let (bytes, unframed) = (self.bytes(), self.unframed());
        if let Some(was) = allotted.take() {
            was.tally.discharge(bytes - unframed, false);
            was.tally.discharge(unframed, true);
            TOTAL.tally.discharge(bytes - unframed, false);
            TOTAL.tally.discharge(unframed, true);
        }
        allotment.tally.charge(bytes - unframed, false);
        allotment.tally.charge(unframed, true);
        TOTAL.tally.charge(bytes - unframed, false);
        TOTAL.tally.charge(unframed, true);
        *allotted = Some(allotment.clone());
    }

    /// Counts its store opened once more, in `dir`: the first opening while none other is alive
    /// lists the directory, and counts what it finds in place of what was counted before.
    pub(crate) fn attach<D: Dir>(&self, dir: &D) -> io::Result<()> {
        let mut opened = self.opened.lock().unwrap();
        if opened.openings == 0 {
            let found = dir.entries()?;
            let (files, dirs) = self.found(found);
            opened.files = files;
            opened.dirs = dirs;
        }
        opened.openings += 1;
        Ok(())
    }

    /// Its files and directories as `found` lists them, counted in place of what was.
    fn found(&self, found: Vec<DirFile>) -> (HashMap<String, Arc<FileLen>>, BTreeSet<String>) {
        let mut by_inode: HashMap<u64, Arc<FileLen>> = HashMap::new();
        let (mut files, mut dirs) = (HashMap::new(), BTreeSet::new());
        let (mut bytes, mut unframed) = (0, 0);
        for found in found {
            if found.dir {
                bytes += host_disk::GRANULE;
                dirs.insert(found.path);
                continue;
            }
            let file = by_inode
                .entry(found.inode)
                .or_insert_with(|| Arc::new(FileLen { state: Mutex::new(LenState::default()) }));
            {
                let mut state = file.state.lock().unwrap();
                state.len = state.len.max(host_disk::counted(found.len));
                state.names += 1;
                state.open_names += usize::from(unframed_path(&found.path));
            }
            files.insert(found.path, file.clone());
        }
        for file in by_inode.values() {
            let state = file.state.lock().unwrap();
            bytes += state.len;
            unframed += if state.unframed() { state.len } else { 0 };
        }
        // What it finds in place of what was counted, each count moved by the difference.
        let (was, was_unframed) = (self.bytes(), self.unframed());
        self.charge(bytes.saturating_sub(was), false);
        self.discharge(was.saturating_sub(bytes), false);
        self.reframe(unframed.saturating_sub(was_unframed), true);
        self.reframe(was_unframed.saturating_sub(unframed), false);
        (files, dirs)
    }

    /// Counts an opening of its store gone: the last lets its names go, its totals kept.
    pub(crate) fn detach(&self) {
        let mut opened = self.opened.lock().unwrap();
        opened.openings -= 1;
        if opened.openings == 0 {
            opened.files.clear();
            opened.dirs.clear();
        }
    }

    /// Counts nothing more of it: its store's directory gone, as a deletion holding it removes it.
    fn forget(&self) {
        let mut opened = self.opened.lock().unwrap();
        for file in std::mem::take(&mut opened.files).into_values() {
            file.forgotten();
        }
        opened.dirs.clear();
        self.reframe(self.unframed(), false);
        self.discharge(self.bytes(), false);
        *self.allotment.lock().unwrap() = None;
    }

    /// The file at `path`, made anew, its length nothing yet.
    pub(crate) fn made(&self, path: &str) -> Arc<FileLen> {
        let file = Arc::new(FileLen { state: Mutex::new(LenState::default()) });
        let mut opened = self.opened.lock().unwrap();
        self.name(&mut opened, path, &file);
        file
    }

    /// The file at `path`, opened: as counted, or, if no change through its store made it,
    /// counted as `len` says, which only a change around its store makes so.
    pub(crate) fn opened(
        &self,
        path: &str,
        len: impl FnOnce() -> io::Result<u64>,
    ) -> io::Result<Arc<FileLen>> {
        let mut opened = self.opened.lock().unwrap();
        if let Some(file) = opened.files.get(path) {
            return Ok(file.clone());
        }
        debug_assert!(false, "{path}: made around its store's directory");
        let file = Arc::new(FileLen { state: Mutex::new(LenState::default()) });
        self.name(&mut opened, path, &file);
        file.grow(self, len()?);
        Ok(file)
    }

    /// The file at `path` removed: uncounted once at no name.
    pub(crate) fn removed(&self, path: &str) {
        let mut opened = self.opened.lock().unwrap();
        self.unname(&mut opened, path);
    }

    /// The file at `from` renamed `to`, over any there: but for one there already, which a
    /// rename leaves as it is, at both names, as POSIX's does.
    pub(crate) fn renamed(&self, from: &str, to: &str) {
        let mut opened = self.opened.lock().unwrap();
        if let (Some(from), Some(to)) = (opened.files.get(from), opened.files.get(to))
            && Arc::ptr_eq(from, to)
        {
            return;
        }
        self.unname(&mut opened, to);
        if let Some(file) = opened.files.get(from).cloned() {
            self.unname(&mut opened, from);
            self.name(&mut opened, to, &file);
        }
    }

    /// The file at `from` given another name, `to`.
    pub(crate) fn linked(&self, from: &str, to: &str) {
        let mut opened = self.opened.lock().unwrap();
        if let Some(file) = opened.files.get(from).cloned() {
            self.name(&mut opened, to, &file);
        }
    }

    /// The file at `path` replaced by one of `len` bytes.
    pub(crate) fn replaced(&self, path: &str, len: u64) {
        let file = Arc::new(FileLen { state: Mutex::new(LenState::default()) });
        let mut opened = self.opened.lock().unwrap();
        self.unname(&mut opened, path);
        self.name(&mut opened, path, &file);
        drop(opened);
        file.grow(self, len);
    }

    /// The directory at `path` made.
    pub(crate) fn made_dir(&self, path: &str) {
        let mut opened = self.opened.lock().unwrap();
        if opened.dirs.insert(path.to_owned()) {
            self.charge(host_disk::GRANULE, false);
        }
    }

    /// Names `file` `path`, which names nothing: counted once at a name.
    fn name(&self, opened: &mut Opened, path: &str, file: &Arc<FileLen>) {
        let replaced = opened.files.insert(path.to_owned(), file.clone());
        debug_assert!(replaced.is_none(), "{path} named twice");
        file.renamed(self, 1, unframed_path(path));
    }

    /// The name `path` gone, if it names a file: uncounted at no name.
    fn unname(&self, opened: &mut Opened, path: &str) {
        if let Some(file) = opened.files.remove(path) {
            file.renamed(self, -1, unframed_path(path));
        }
    }
}

/// No limit.
const NONE: u64 = u64::MAX;

/// A directory a tenant is granted, as its stores' files take the host's disk: their footprints
/// summed, and its tenant's limit (TASK-539.5). See the module docs.
pub struct Allotment {
    /// Its directory, canonical.
    dir: PathBuf,
    /// Its stores' footprints, summed.
    tally: Tally,
    /// What writes admitted and not yet landed hold.
    pledged: Count,
    /// Its limit, or [`NONE`].
    limit: AtomicU64,
    /// Whether the last write refused was refused by it: logged as it begins to be.
    full: AtomicBool,
}

/// Every allotment's together, and every tenant's limit.
pub struct Total {
    tally: Tally,
    pledged: Count,
    limit: AtomicU64,
}

/// Every allotment's, together.
static TOTAL: Total =
    Total { tally: Tally::new(), pledged: Count::new(), limit: AtomicU64::new(NONE) };

/// Every allotment made: kept for the process's life, as their stores' files are on its disk,
/// granted or not.
static ALLOTMENTS: std::sync::Mutex<Vec<Arc<Allotment>>> = std::sync::Mutex::new(Vec::new());

/// The allotment of the directory `dir`, a tenant's: the one made already, or one made now,
/// counting the stores in it as they were counted, and those not counted yet, closed, as one
/// listing of each finds them.
pub fn allotment(dir: &Path) -> io::Result<Arc<Allotment>> {
    let dir = std::fs::canonicalize(dir)?;
    let made = {
        let mut allotments = ALLOTMENTS.lock().unwrap();
        if let Some(made) = allotments.iter().find(|allotment| allotment.dir == dir) {
            return Ok(made.clone());
        }
        let made = Arc::new(Allotment {
            dir,
            tally: Tally::new(),
            pledged: Count::new(),
            limit: AtomicU64::new(NONE),
            full: AtomicBool::new(false),
        });
        allotments.push(made.clone());
        made
    };
    let counted: Vec<_> = {
        let ledger = LEDGER.lock().unwrap();
        ledger.iter().flat_map(|ledger| ledger.values()).cloned().collect()
    };
    for footprint in counted.iter().filter(|footprint| made.holds(footprint)) {
        allot_nearest(footprint);
    }
    count_stores_in(&made.dir)?;
    Ok(made)
}

/// Counts the footprint of each store in `dir` not counted yet, at any depth: one listing of each.
/// A store's directory is one holding its description, or what its deletion or making leaves of it.
fn count_stores_in(dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let store = [crate::layout::DESCRIPTION, crate::host::DELETED, crate::cell::UNMADE]
            .iter()
            .any(|name| path.join(name).exists());
        if !store {
            count_stores_in(&path)?;
            continue;
        }
        let id = dir_id(&path)?;
        let counted =
            LEDGER.lock().unwrap().as_ref().is_some_and(|ledger| ledger.contains_key(&id));
        if !counted {
            drop(crate::HostDir::new(&path)?);
        }
    }
    Ok(())
}

/// Counts `footprint` in the allotment whose directory holds it, the nearest, if any does.
fn allot_nearest(footprint: &Footprint) {
    let nearest = {
        let allotments = ALLOTMENTS.lock().unwrap();
        let holding = allotments.iter().filter(|allotment| allotment.holds(footprint));
        holding.max_by_key(|allotment| allotment.dir.as_os_str().len()).cloned()
    };
    let Some(nearest) = nearest else { return };
    if footprint.allotment().is_none_or(|was| !Arc::ptr_eq(&was, &nearest)) {
        footprint.allot(&nearest);
    }
}

impl Allotment {
    /// Whether its directory holds `footprint`'s store.
    fn holds(&self, footprint: &Footprint) -> bool {
        footprint.path().is_some_and(|path| path.starts_with(&self.dir) && path != self.dir)
    }

    /// Its directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What its stores' files take, counted.
    pub fn bytes(&self) -> u64 {
        self.tally.bytes()
    }

    /// Of those, unframed files'.
    pub fn unframed(&self) -> u64 {
        self.tally.unframed()
    }

    /// What writes admitted and not yet landed hold.
    pub fn pledged(&self) -> u64 {
        self.pledged.get()
    }

    /// Its limit, if it has one.
    pub fn limit(&self) -> Option<u64> {
        Some(self.limit.load(Ordering::Relaxed)).filter(|limit| *limit != NONE)
    }

    /// Limits it to `bytes`, or lifts its limit, without.
    pub fn set_limit(&self, bytes: Option<u64>) {
        self.limit.store(bytes.unwrap_or(NONE), Ordering::Relaxed);
    }

    /// Whether the last write refused it was refused by its limit or every tenant's.
    pub fn is_full(&self) -> bool {
        self.full.load(Ordering::Relaxed)
    }

    /// Admits a write of `bytes` more, if what it takes and holds admitted leaves room for them
    /// within its limit, and every allotment's within every tenant's: held until the returned
    /// pledge goes, the write landed, and counted as it lands. [`Full`] otherwise, logged as its
    /// refusals begin.
    pub fn admit(self: &Arc<Self>, bytes: u64) -> Result<Pledge, Full> {
        // Pledged first, so that two admitted at once each count the other.
        let pledge = Pledge::new(self.clone(), bytes);
        let within = |tally: &Tally, pledged: &Count, limit: &AtomicU64| {
            tally.bytes() + pledged.get() <= limit.load(Ordering::Relaxed)
        };
        let full = match () {
            () if !within(&self.tally, &self.pledged, &self.limit) => Some(Full::Allotment),
            () if !within(&TOTAL.tally, &TOTAL.pledged, &TOTAL.limit) => Some(Full::Total),
            () => None,
        };
        let Some(full) = full else {
            self.full.store(false, Ordering::Relaxed);
            return Ok(pledge);
        };
        drop(pledge);
        if !self.full.swap(true, Ordering::Relaxed) {
            log::warn!(
                "cladefs: {} at its disk's limit, its writes refused (ENOSPC): {full}; it takes {} \
                 bytes of {:?}, every tenant's {} of {:?}",
                self.dir.display(),
                self.bytes(),
                self.limit(),
                total().bytes(),
                total_limit()
            );
        }
        Err(full)
    }
}

/// What refused a write ([`Allotment::admit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Full {
    /// Its tenant's limit.
    #[error("its tenant's limit")]
    Allotment,
    /// Every tenant's limit.
    #[error("every tenant's limit")]
    Total,
}

/// A write admitted ([`Allotment::admit`]): what it holds of its allotment until it goes.
pub struct Pledge {
    allotment: Arc<Allotment>,
    bytes: u64,
}

impl Pledge {
    fn new(allotment: Arc<Allotment>, bytes: u64) -> Self {
        allotment.pledged.add(bytes);
        TOTAL.pledged.add(bytes);
        Self { allotment, bytes }
    }
}

impl Drop for Pledge {
    fn drop(&mut self) {
        self.allotment.pledged.sub(self.bytes);
        TOTAL.pledged.sub(self.bytes);
    }
}

/// What every allotment's stores' files take together.
pub fn total() -> &'static Tally {
    &TOTAL.tally
}

/// Every tenant's limit, if set.
pub fn total_limit() -> Option<u64> {
    Some(TOTAL.limit.load(Ordering::Relaxed)).filter(|limit| *limit != NONE)
}

/// Limits every allotment together to `bytes`, or lifts the limit, without.
pub fn set_total_limit(bytes: Option<u64>) {
    TOTAL.limit.store(bytes.unwrap_or(NONE), Ordering::Relaxed);
}

/// Whether `path` is an unframed file's: one under `open-segments/` or `open-extents/`.
fn unframed_path(path: &str) -> bool {
    let kind = path.split('/').next().unwrap_or_default();
    kind == OPEN_SEGMENTS || kind == OPEN_EXTENTS
}

/// A file of a store, as its footprint counts it: shared by all its names and openings.
pub struct FileLen {
    state: Mutex<LenState>,
}

#[derive(Default)]
struct LenState {
    /// Its length, counted.
    len: u64,
    /// Its names: counted while it has one.
    names: usize,
    /// Of those, under `open-segments/` or `open-extents/`: unframed while it has one.
    open_names: usize,
}

impl LenState {
    fn counted(&self) -> bool {
        self.names > 0
    }

    fn unframed(&self) -> bool {
        self.names > 0 && self.open_names > 0
    }
}

impl FileLen {
    /// It goes as far as `end` at least: counted, if it has a name, as far as that.
    pub(crate) fn grow(&self, footprint: &Footprint, end: u64) {
        let mut state = self.state.lock().unwrap();
        let len = host_disk::counted(end);
        if len <= state.len {
            return;
        }
        let more = len - std::mem::replace(&mut state.len, len);
        if state.counted() {
            footprint.charge(more, state.unframed());
        }
    }

    /// It is counted no more, whatever writes it, its store's directory gone.
    fn forgotten(&self) {
        let mut state = self.state.lock().unwrap();
        (state.names, state.open_names) = (0, 0);
    }

    /// It has `names` names more (or fewer), `open` of them under `open-*` if so: counted or
    /// uncounted, framed or unframed, as that leaves it.
    fn renamed(&self, footprint: &Footprint, names: isize, open: bool) {
        let mut state = self.state.lock().unwrap();
        let (counted, unframed) = (state.counted(), state.unframed());
        state.names = state.names.checked_add_signed(names).expect("a file's names below none");
        if open {
            state.open_names =
                state.open_names.checked_add_signed(names).expect("a file's names below none");
        }
        match (counted, state.counted()) {
            (true, false) => footprint.discharge(state.len, unframed),
            (false, true) => footprint.charge(state.len, state.unframed()),
            (true, true) if unframed != state.unframed() => {
                footprint.reframe(state.len, state.unframed())
            }
            _ => {}
        }
    }
}

/// A store directory of the host's, by its device and inode.
type DirId = (u64, u64);

/// The footprints of the stores of the host's this process has opened, by their directories:
/// kept while their stores are closed, until their directories are removed.
static LEDGER: std::sync::Mutex<Option<HashMap<DirId, Arc<Footprint>>>> =
    std::sync::Mutex::new(None);

/// The directory at `path`'s device and inode.
fn dir_id(path: &Path) -> io::Result<DirId> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// The footprint of the store whose directory is at `path`, on `disk`: the one counted already,
/// if it was opened in this process before.
pub(crate) fn of_host(path: &Path, disk: &Arc<HostDisk>) -> io::Result<Arc<Footprint>> {
    let id = dir_id(path)?;
    let mut ledger = LEDGER.lock().unwrap();
    let footprints = ledger.get_or_insert_with(HashMap::new);
    if let Some(footprint) = footprints.get(&id) {
        return Ok(footprint.clone());
    }
    let footprint = Footprint::new(Some(std::fs::canonicalize(path)?), Some(disk.clone()));
    footprints.insert(id, footprint.clone());
    drop(ledger);
    allot_nearest(&footprint);
    Ok(footprint)
}

/// Forgets the footprint of the store whose directory is at `path`, if it was counted: removed.
/// `forgotten` runs between, the store's directory removed by it if it is `Ok`.
pub(crate) fn removing<T>(
    path: &Path,
    forgotten: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let id = dir_id(path).ok();
    let removed = forgotten()?;
    let mut ledger = LEDGER.lock().unwrap();
    let footprint = id.and_then(|id| ledger.as_mut().and_then(|ledger| ledger.remove(&id)));
    if let Some(footprint) = footprint {
        footprint.forget();
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::File as _;
    use crate::{HostDir, MemDir};
    use bucket_cell::host_disk::{GRANULE, counted};
    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng as _};
    use std::collections::BTreeMap;

    /// The directories a store keeps its files in, and names in them, some at the top.
    const DIRS: [&str; 5] = ["open-segments", "segments", "open-extents", "extents", "logs"];

    /// What `dir` holds, as its listing says, what its page cache holds dirty written back first:
    /// its files' and directories' bytes, counted, and of them its unframed files'. What a
    /// footprint must say of it after every change. Written back, as the page cache keeps a file's
    /// pages by the name it was opened at, and a file at two names opened at each would hold its
    /// pages in two places: no store opens one so, but these tests do.
    fn listed<D: Dir>(dir: &crate::Metered<crate::CachedDir<D>>) -> (u64, u64) {
        dir.cache().write_back_all().unwrap();
        let mut inodes: BTreeMap<u64, (u64, bool)> = BTreeMap::new();
        let mut bytes = 0;
        for found in dir.entries().unwrap() {
            if found.dir {
                bytes += GRANULE;
                continue;
            }
            let inode = inodes.entry(found.inode).or_default();
            inode.0 = inode.0.max(counted(found.len));
            inode.1 |= unframed_path(&found.path);
        }
        let unframed = inodes.values().filter(|(_, open)| *open).map(|(len, _)| len).sum();
        (bytes + inodes.values().map(|(len, _)| len).sum::<u64>(), unframed)
    }

    /// Whether the file at `path` holds extents' data, as a store opens it, the same way every
    /// time: a log's, and one at the top, don't.
    fn data(path: &str) -> bool {
        path.contains('/') && !path.starts_with("logs/")
    }

    /// A name of a file, in one of [`DIRS`] or at the top.
    fn name(rng: &mut StdRng) -> String {
        let file = rng.random_range(0..4);
        match rng.random_range(0..=DIRS.len()) {
            0 => format!("f{file}"),
            dir => format!("{}/{file}", DIRS[dir - 1]),
        }
    }

    /// Changes `dir` at random, a step of `seed`'s, as a store would: files made, written past
    /// their ends and within them, linked, renamed over others and not, removed, replaced, given;
    /// written again after their last name went; its file system failing what it changes, some
    /// steps. Each step's own errors are a file system's, which the footprint counts after as
    /// before. Returns what it did, and keeps the files opened, which outlive their names.
    fn step<D: Dir>(dir: &D, rng: &mut StdRng, open: &mut Vec<D::File>) -> String {
        let (from, to) = (name(rng), name(rng));
        let (what, done) = match rng.random_range(0..9) {
            0 => ("create", dir.create(&from, data(&from)).map(|file| open.push(file))),
            1 | 2 => {
                let (len, at) =
                    (rng.random_range(1..3 * GRANULE as usize), rng.random_range(0..4 * GRANULE));
                let written = dir.open(&from, data(&from)).and_then(|file| {
                    file.write_at(&vec![7; len], at)?;
                    open.push(file);
                    Ok(())
                });
                ("write", written)
            }
            3 => ("link", dir.link(&from, &to)),
            4 => ("rename", dir.rename(&from, &to)),
            5 => ("remove", dir.remove(&from)),
            6 => {
                ("replace", dir.replace(&from, &vec![1; rng.random_range(0..2 * GRANULE as usize)]))
            }
            7 => {
                let taken = rng.random();
                let given = dir.give(&from, &mut |path| {
                    if taken {
                        std::fs::remove_file(path)?;
                    }
                    Ok(taken)
                });
                ("give", given.map(drop))
            }
            // A file whose last name went, written still: counted no more.
            _ => match open.get(rng.random_range(0..open.len().max(1))) {
                Some(file) => {
                    ("write open", file.write_at(&[9; 100], rng.random_range(0..8 * GRANULE)))
                }
                None => ("nothing", Ok(())),
            },
        };
        format!("{what} {from} {to}: {done:?}")
    }

    /// Its footprint says what its listing does, and its disk, past what it counted of others,
    /// `before`.
    fn agrees<D: Dir>(
        dir: &crate::Metered<crate::CachedDir<D>>,
        footprint: &Footprint,
        before: (u64, u64),
        at: &str,
    ) {
        let (bytes, unframed) = listed(dir);
        assert_eq!((footprint.bytes(), footprint.unframed()), (bytes, unframed), "{at}");
        let disk = footprint.disk().unwrap();
        let counted = (disk.stores.bytes() - before.0, disk.stores.unframed() - before.1);
        assert_eq!(counted, (bytes, unframed), "{at}: its disk");
    }

    /// TASK-539.4: a store's footprint is what its directory's listing counts after every change,
    /// in memory, its file system failing some, crashes listed anew.
    #[test]
    fn footprints_count_what_directories_in_memory_hold() {
        for seed in 0..200 {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut dir = MemDir::new();
            DIRS.iter().for_each(|path| dir.create_dir(path).unwrap());
            let mut open = Vec::new();
            for at in 0..100 {
                if rng.random_range(0..20) == 0 {
                    dir.fail_after(Some(rng.random_range(0..3)));
                }
                let did = step(&dir, &mut rng, &mut open);
                dir.fail_after(None);
                agrees(&dir, dir.footprint(), (0, 0), &format!("seed {seed}, step {at}, {did}"));
                if rng.random_range(0..30) == 0 {
                    open.clear();
                    dir = dir.crash(rng.random());
                    agrees(&dir, dir.footprint(), (0, 0), &format!("seed {seed}, crashed at {at}"));
                }
            }
        }
    }

    /// TASK-539.4: so on the host's filesystem, given files leaving, a write through a file's
    /// descriptor counted as far as it was settled for.
    #[test]
    fn footprints_count_what_directories_of_the_hosts_hold() {
        for seed in 0..40 {
            let temp = tempfile::tempdir().unwrap();
            let dir = HostDir::new(temp.path()).unwrap();
            let disk = dir.footprint().disk().unwrap();
            let before = (disk.stores.bytes(), disk.stores.unframed());
            DIRS.iter().for_each(|path| dir.create_dir(path).unwrap());
            let mut rng = StdRng::seed_from_u64(seed);
            let mut open = Vec::new();
            for at in 0..60 {
                let did = step(&dir, &mut rng, &mut open);
                agrees(&dir, dir.footprint(), before, &format!("seed {seed}, step {at}, {did}"));
            }
            if let Some(file) = open.iter().find(|file| file.size().unwrap() > 0) {
                let before = dir.footprint().bytes();
                file.settle_for_write(0..file.size().unwrap() + 3 * GRANULE).unwrap();
                assert!(dir.footprint().bytes() >= before, "seed {seed}");
            }
        }
    }

    /// A write that fails part way is counted as far as the file went; one past a file's last
    /// name, not at all; a file at two names, once.
    #[test]
    fn failed_writes_count_what_they_grew_and_names_count_once() {
        let dir = MemDir::with_capacity(4 * GRANULE);
        let footprint = dir.footprint().clone();
        let file = dir.create("f", true).unwrap();
        file.write_at(&[1; 100], 0).unwrap();
        dir.link("f", "g").unwrap();
        assert_eq!(footprint.bytes(), GRANULE);
        dir.remove("f").unwrap();
        dir.remove("g").unwrap();
        file.write_at(&[1; 100], GRANULE).unwrap();
        assert_eq!(footprint.bytes(), 0);
    }

    /// TASK-539.4: a store of the host's has one footprint however many times it is opened at
    /// once, which it keeps while closed; a change made while closed, around it, is counted at its
    /// next opening; its removal forgets it, its disk counting nothing of it.
    #[test]
    fn host_stores_are_counted_once_while_open_and_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store");
        std::fs::create_dir(&path).unwrap();
        let dir = HostDir::new(&path).unwrap();
        let disk = dir.footprint().disk().unwrap().clone();
        let before = disk.stores.bytes();
        dir.create_dir("segments").unwrap();
        dir.create("segments/1", true).unwrap().write_at(&[1; 100], 0).unwrap();
        let other = HostDir::new(&path).unwrap();
        assert!(Arc::ptr_eq(dir.footprint(), other.footprint()));
        other.create("f", false).unwrap().write_at(&[2; 100], 0).unwrap();
        assert_eq!(dir.footprint().bytes(), 3 * GRANULE);
        let footprint = dir.footprint().clone();
        drop((dir, other));
        assert_eq!(footprint.bytes(), 3 * GRANULE, "closed, still counted");
        // Changed around it while closed.
        std::fs::write(path.join("f"), vec![3; 2 * GRANULE as usize]).unwrap();
        std::fs::remove_file(path.join("segments/1")).unwrap();
        let dir = HostDir::new(&path).unwrap();
        assert_eq!(dir.footprint().bytes(), 3 * GRANULE);
        assert_eq!(disk.stores.bytes() - before, 3 * GRANULE);
        drop(dir);
        crate::remove_store_dir(&path).unwrap();
        assert_eq!((footprint.bytes(), disk.stores.bytes()), (0, before));
    }
}
