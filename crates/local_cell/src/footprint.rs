// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a cell's files take of the host's disk, counted where the cell writes them
//! (backlog doc-22, TASK-539.4): its **footprint**. Every change a cell makes to its directory
//! goes through one layer ([`crate::Metered`]), which counts it here as the cell makes it: a file
//! grown by a write, made, linked, renamed, given away or removed; a directory made. Nothing walks
//! a directory to count it, nor writes back the cell page cache to: a write is counted as the
//! cache takes it, and a file removed uncounted as it goes, whether or not its pages reached the
//! disk.
//!
//! **What is counted.** Each file's length, rounded up to a granule
//! ([`bucket_cell::host_disk::counted`]), once however many names it has, and each directory a
//! granule; cladefs: but for what an ephemeral cell's kept segments gave back of theirs
//! ([`crate::File::punch`], doc-25, TASK-698.2), which their blocks allocated say; and, of that,
//! its **unframed** bytes: files with a name under `open-segments/` or `open-extents/`, raw data
//! whose framing, or sealing in place, is still to come, which the cell's own work, never refused,
//! may add to. A footprint charges its disk's counters as its own ([`HostDisk::cells`],
//! [`HostDisk::unframed`]).
//!
//! **While a cell is open, and closed.** The first opening of a cell, while no other opening of
//! it is alive, lists its directory once and counts what it finds, against what was counted of
//! it before; the last lets its names go but keeps its totals, so that a cell closed stays
//! counted, and one changed while closed, by another process, is counted anew at its next
//! opening. A cell of the host's has one footprint however many times it is opened, by its
//! directory's device and inode ([`of_host`]); its removal ([`crate::remove_cell_dir`]) forgets
//! it. A directory in memory has one of its own.
//!
//! **Allotments.** A directory a tenant is granted is an [`Allotment`]: the footprints of the cells
//! in it summed, with the tenant's limit, and every allotment's together ([`total`]) with every
//! tenant's limit (TASK-539.5). A cell's footprint is its allotment's whose directory holds it,
//! the nearest; one in no allotment's is no tenant's, counted on its disk alone. An allotment made
//! counts the cells in its directory not counted yet, closed, by one listing of each.

use crate::dir::{Dir, DirFile};
use crate::layout::{OPEN_EXTENTS, OPEN_SEGMENTS, SEGMENTS};
use crate::sync::Mutex;
use bucket_cell::host_disk::{self, HostDisk, Tally};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What a cell's files take of its disk. See the module docs.
pub struct Footprint {
    /// Its directory's path, canonical, if it is the host's.
    path: Option<PathBuf>,
    /// The disk it charges as it is charged.
    disk: Option<Arc<HostDisk>>,
    /// Its files' lengths and its directories, counted.
    tally: Tally,
    /// The most `tally` counted since it was last asked ([`Footprint::take_peak`]).
    peak: AtomicU64,
    /// The allotment it charges as it is charged, if any's directory holds it.
    allotment: Mutex<Option<Arc<Allotment>>>,
    /// Its files and directories by name, while its cell is open.
    opened: Mutex<Opened>,
}

/// A footprint's names, while any opening of its cell is alive.
#[derive(Default)]
struct Opened {
    /// The openings alive.
    openings: usize,
    /// The takings of its cell directory alive in this process ([`Dir::take`]): the first, which
    /// takes it from other processes, which may have changed it since, lists it anew.
    takings: usize,
    /// Its files, by name: a file at two names is one.
    files: HashMap<String, Arc<FileLen>>,
    /// Its directories, by path.
    dirs: BTreeSet<String>,
    /// Whether its cell directory went while it was open, which counted it closed.
    forgotten: bool,
}

impl Footprint {
    /// A footprint charging `disk`, of a cell whose directory is at `path`, if it is the host's:
    /// counting nothing until its cell's first opening lists it.
    pub(crate) fn new(path: Option<PathBuf>, disk: Option<Arc<HostDisk>>) -> Arc<Self> {
        Arc::new(Self {
            path,
            disk,
            tally: Tally::default(),
            peak: AtomicU64::new(0),
            allotment: Mutex::new(None),
            opened: Mutex::default(),
        })
    }

    /// The bytes its cell's files take, counted.
    pub fn bytes(&self) -> u64 {
        self.tally.bytes()
    }

    /// Of those, the bytes of its unframed files.
    pub fn unframed(&self) -> u64 {
        self.tally.unframed()
    }

    /// The bytes of its cell's extents open, each as far as it goes.
    pub fn open(&self) -> u64 {
        self.tally.open()
    }

    /// What its cell's own work may still add to it, never refused ([`Tally::reserved`]).
    pub fn reserved(&self) -> u64 {
        self.tally.reserved()
    }

    /// The allotment it is in, if any is.
    pub fn allotment(&self) -> Option<Arc<Allotment>> {
        self.allotment.lock().unwrap().clone()
    }

    /// Its directory's path, canonical, if it is the host's.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The disk its cell is on.
    pub fn disk(&self) -> Option<&Arc<HostDisk>> {
        self.disk.as_ref()
    }

    /// Has `change` count in its tally, its allotment's and every allotment's, if it is in one,
    /// and its disk's: under its allotment's lock, so that a cell moved to another allotment moves
    /// what it counted whole.
    fn counting(&self, change: impl Fn(&Tally)) {
        let allotment = self.allotment.lock().unwrap();
        change(&self.tally);
        if let Some(allotment) = &*allotment {
            change(&allotment.tally);
            change(&allotment.total.tally);
        }
        if let Some(disk) = &self.disk {
            change(&disk.cells);
        }
    }

    /// Counts `bytes` more, `unframed` if so.
    fn charge(&self, bytes: u64, unframed: bool) {
        self.counting(|tally| tally.charge(bytes, unframed));
        self.peak.fetch_max(self.bytes(), Ordering::Relaxed);
    }

    /// The most its cell's files took since this was last asked, or it was made: what its
    /// cell's own work, between, took at its peak.
    pub fn take_peak(&self) -> u64 {
        self.peak.swap(self.bytes(), Ordering::Relaxed).max(self.bytes())
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
        if let Some(was) = allotted.take() {
            moved(&self.tally, &was.tally, false);
            moved(&self.tally, &was.total.tally, false);
        }
        moved(&self.tally, &allotment.tally, true);
        moved(&self.tally, &allotment.total.tally, true);
        *allotted = Some(allotment.clone());
    }

    /// cladefs: its cell's extents open take `open` bytes, each as far as it goes: what the commit
    /// sealing them may copy, which its reserve covers ([`host_disk::RESERVE_FACTOR`]). Reads and
    /// updates the tally under the allotment lock so concurrent updates never race and underflow
    /// (board 5442).
    pub(crate) fn set_open(&self, open: u64) {
        let allotment = self.allotment.lock().unwrap();
        let was = self.tally.open();
        if open == was {
            return;
        }
        let delta = open.abs_diff(was);
        let more = open > was;
        self.tally.opened(delta, more);
        if let Some(allotment) = &*allotment {
            allotment.tally.opened(delta, more);
            allotment.total.tally.opened(delta, more);
        }
        if let Some(disk) = &self.disk {
            disk.cells.opened(delta, more);
        }
    }

    /// Admits a write of `len` bytes, which grows its extent by `grows`, if the cell's disk, its
    /// allotment, if it is in one, and every allotment have room for them and what its cell's own
    /// work may add for them: held until the returned pledge goes, the write landed, counted as it
    /// lands ([`DiskFull`] otherwise, logged as refusals begin). Every one admitted leaves room for
    /// every commit, flush and framing of every cell, which nothing refuses (doc-22).
    pub fn admit(self: &Arc<Self>, len: u64, grows: u64) -> Result<Pledge, DiskFull> {
        self.admit_bytes(len + host_disk::RESERVE_FACTOR * (len + grows))
    }

    /// Admits the making of a cell, as [`Footprint::admit`] does a write: what its files take
    /// made, [`STORE_MADE`].
    pub fn admit_made(self: &Arc<Self>) -> Result<Pledge, DiskFull> {
        self.admit_bytes(STORE_MADE)
    }

    /// Admits `bytes` more, as [`Footprint::admit`] says.
    fn admit_bytes(self: &Arc<Self>, bytes: u64) -> Result<Pledge, DiskFull> {
        // Pledged first, so that two admitted at once each count the other.
        let pledge = Pledge::new(self.clone(), bytes);
        let allotment = self.allotment();
        let over =
            |tally: &Tally, limit: &AtomicU64| tally.committed() > limit.load(Ordering::Relaxed);
        let full = match &allotment {
            Some(allotment)
                if over(&allotment.tally, &allotment.limit)
                    || allotment.group().is_some_and(|group| {
                        group.committed() > group.limit.load(Ordering::Relaxed)
                    }) =>
            {
                Some(DiskFull::Allotment)
            }
            Some(allotment) if over(&allotment.total.tally, &allotment.total.limit) => {
                Some(DiskFull::Total)
            }
            Some(allotment) if over(&allotment.tally, &allotment.share) => Some(DiskFull::Share),
            _ => self.disk.as_ref().and_then(|disk| disk_full(disk)),
        };
        let Some(full) = full else {
            if let Some(allotment) = &allotment {
                allotment.full.store(false, Ordering::Relaxed);
            }
            self.disk.iter().for_each(|disk| disk.admitted());
            return Ok(pledge);
        };
        let begins = match (full, &allotment) {
            (DiskFull::Disk, _) => self.disk.as_ref().is_some_and(|disk| disk.refused()),
            (_, Some(allotment)) => !allotment.full.swap(true, Ordering::Relaxed),
            (_, None) => false,
        };
        drop(pledge);
        if begins {
            let total_bytes = allotment.as_ref().map_or(0, |a| a.total.tally.bytes());
            let total_lim = allotment
                .as_ref()
                .and_then(|a| Some(a.total.limit.load(Ordering::Relaxed)).filter(|l| *l != NONE));
            log::warn!(
                "extent store: {} refuses writes (ENOSPC): {full}; it takes {} bytes, its \
                 allotment {:?} of {:?}, every allotment {total_bytes} of {total_lim:?}, its disk {:?}",
                self.path.as_deref().map_or("a cell".into(), |path| path.display().to_string()),
                self.bytes(),
                allotment.as_ref().map(|allotment| allotment.bytes()),
                allotment.as_ref().and_then(|allotment| allotment.limit()),
                self.disk,
            );
        }
        if let Some(disk) = &self.disk {
            disk.wake_governor();
        }
        Err(full)
    }

    /// Counts its cell opened once more, in `dir`: the first opening while none other is alive
    /// lists the directory, and counts what it finds in place of what was counted before, and
    /// counts the cell open, reserving what each cell open may add
    /// ([`host_disk::RESERVE_PER_STORE`]).
    pub(crate) fn attach<D: Dir>(&self, dir: &D) -> io::Result<()> {
        let mut opened = self.opened.lock().unwrap();
        if opened.openings == 0 {
            let found = dir.entries()?;
            let (files, dirs) = self.found(found);
            opened.files = files;
            opened.dirs = dirs;
            opened.forgotten = false;
            self.counting(|tally| tally.cell(true));
        }
        opened.openings += 1;
        Ok(())
    }

    /// Counts its cell directory, `dir`, taken once more in this process: the first taking, from
    /// other processes, which may have changed it while they had it, lists it, and counts what it
    /// finds in place of what was counted before; the files counted before, at names it may have
    /// changed, counted no more.
    pub(crate) fn taken<D: Dir>(&self, dir: &D) -> io::Result<()> {
        let mut opened = self.opened.lock().unwrap();
        if opened.takings == 0 {
            let found = dir.entries()?;
            for file in opened.files.values() {
                file.forgotten();
            }
            let (files, dirs) = self.found(found);
            opened.files = files;
            opened.dirs = dirs;
        }
        opened.takings += 1;
        Ok(())
    }

    /// Counts a taking of its cell directory in this process let go.
    pub(crate) fn let_go(&self) {
        self.opened.lock().unwrap().takings -= 1;
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
                // cladefs: an ephemeral segment's blocks allocated, its holes given back
                // (TASK-698.2, board 6905).
                let counted = match segment_dir_path(&found.path) {
                    true => found.allocated.min(found.len),
                    false => found.len,
                };
                state.size = state.size.max(host_disk::counted(found.len));
                state.len = state.len.max(host_disk::counted(counted));
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

    /// Counts an opening of its cell gone: the last lets its names go, its totals kept.
    pub(crate) fn detach(&self) {
        let mut opened = self.opened.lock().unwrap();
        opened.openings -= 1;
        if opened.openings == 0 {
            opened.files.clear();
            opened.dirs.clear();
            if !opened.forgotten {
                self.counting(|tally| tally.cell(false));
            }
        }
    }

    /// Counts nothing more of it: its cell directory gone, as a deletion holding it removes it.
    fn forget(&self) {
        let mut opened = self.opened.lock().unwrap();
        for file in std::mem::take(&mut opened.files).into_values() {
            file.forgotten();
        }
        opened.dirs.clear();
        self.reframe(self.unframed(), false);
        self.discharge(self.bytes(), false);
        self.set_open(0);
        if opened.openings > 0 && !opened.forgotten {
            self.counting(|tally| tally.cell(false));
        }
        opened.forgotten = true;
        *self.allotment.lock().unwrap() = None;
    }

    /// The file at `path`, made anew, its length nothing yet.
    pub(crate) fn made(&self, path: &str) -> Arc<FileLen> {
        let file = Arc::new(FileLen { state: Mutex::new(LenState::default()) });
        let mut opened = self.opened.lock().unwrap();
        self.name(&mut opened, path, &file);
        file
    }

    /// The file at `path`, opened: as counted, or, if no change through its cell made it,
    /// counted as `len` says, which only a change around its cell makes so.
    pub(crate) fn opened(
        &self,
        path: &str,
        len: impl FnOnce() -> io::Result<u64>,
    ) -> io::Result<Arc<FileLen>> {
        let mut opened = self.opened.lock().unwrap();
        if let Some(file) = opened.files.get(path) {
            return Ok(file.clone());
        }
        debug_assert!(false, "{path}: made around its cell directory");
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

/// What a cell made is admitted as ([`Footprint::admit_made`]): a cell's files, a fork's made and
/// an uploaded base's, take 12 to 16 KiB of a filesystem's 4 KiB blocks (cladefs's
/// `tests/disk.rs`), four times which leaves room for larger blocks.
pub const STORE_MADE: u64 = 64 << 10;

/// Aggregate footprint and limit shared by a tenant granted multiple cell directories across JBOD
/// host disks: each directory keeps its own per-disk [`Allotment`] for per-disk weighted shares,
/// while the tenant's configured limit is enforced against the sum across all member allotments.
/// Because each member's `Tally` remains authoritative on the [`Allotment`] itself, re-granting a
/// tenant with an expanded or contracted directory set rebinds existing allotments to a fresh
/// [`Group`] with zero tally-migration drift.
pub struct Group {
    members: Mutex<Vec<Arc<Allotment>>>,
    limit: AtomicU64,
}

impl Group {
    /// A new tenant group binding `members` under one aggregate limit (clearing each member's
    /// individual allotment limit so the group limit is the single source of truth).
    pub fn for_allotments(members: Vec<Arc<Allotment>>) -> Arc<Self> {
        let group =
            Arc::new(Self { members: Mutex::new(members.clone()), limit: AtomicU64::new(NONE) });
        for member in &members {
            member.set_limit(None);
            member.set_group(Some(group.clone()));
        }
        group
    }

    /// What all allotments in this group take together, counted.
    pub fn bytes(&self) -> u64 {
        self.members.lock().unwrap().iter().map(|m| m.bytes()).sum()
    }

    /// Of those, unframed files'.
    pub fn unframed(&self) -> u64 {
        self.members.lock().unwrap().iter().map(|m| m.unframed()).sum()
    }

    /// What writes admitted and not yet landed hold across the group.
    pub fn pledged(&self) -> u64 {
        self.members.lock().unwrap().iter().map(|m| m.pledged()).sum()
    }

    /// What all cells in the group take, reserve and hold admitted.
    pub fn committed(&self) -> u64 {
        self.members.lock().unwrap().iter().map(|m| m.committed()).sum()
    }

    /// What all cells' own work may still add, and writes admitted hold.
    pub fn reserved(&self) -> u64 {
        self.members.lock().unwrap().iter().map(|m| m.reserved()).sum()
    }

    /// Its aggregate limit, if set.
    pub fn limit(&self) -> Option<u64> {
        Some(self.limit.load(Ordering::Relaxed)).filter(|limit| *limit != NONE)
    }

    /// Limits the group to `bytes`, or lifts its limit, without.
    pub fn set_limit(&self, bytes: Option<u64>) {
        self.limit.store(bytes.unwrap_or(NONE), Ordering::Relaxed);
    }
}

/// A directory a tenant is granted, as its cells' files take the host's disk: their footprints
/// summed, and its tenant's limit (TASK-539.5). See the module docs.
pub struct Allotment {
    /// Its directory, canonical.
    dir: PathBuf,
    /// Its cells' footprints, summed.
    tally: Tally,
    /// Its limit, or [`NONE`].
    limit: AtomicU64,
    /// Optional multi-disk tenant group sharing an aggregate limit across several directories.
    group: Mutex<Option<Arc<Group>>>,
    /// Whether the last write refused was refused by it: logged as it begins to be.
    full: AtomicBool,
    /// The disk of its directory's filesystem, whose governor shares what it leaves by weight.
    disk: Arc<HostDisk>,
    /// Its weight against the other allotments on its disk ([`DEFAULT_WEIGHT`] until set).
    weight: AtomicU64,
    /// What it keeps while its disk is under pressure, its share by weight, or [`NONE`]
    /// otherwise: past it, its writes are refused (doc-22, TASK-539.10).
    share: AtomicU64,
    /// Every allotment on its host together.
    total: Arc<Total>,
}

/// An allotment's weight unless it is set: each alike.
pub const DEFAULT_WEIGHT: u64 = 1;

/// Every allotment's together, and every tenant's limit.
struct Total {
    tally: Tally,
    limit: AtomicU64,
}

/// Per-host footprint state (total tally/limit, allotments, and cell directory ledger), owned by
/// [`crate::Host`].
pub(crate) struct State {
    total: Arc<Total>,
    allotments: std::sync::Mutex<Vec<Arc<Allotment>>>,
    ledger: std::sync::Mutex<Option<HashMap<DirId, Arc<Footprint>>>>,
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            total: Arc::new(Total { tally: Tally::new(), limit: AtomicU64::new(NONE) }),
            allotments: std::sync::Mutex::new(Vec::new()),
            ledger: std::sync::Mutex::new(None),
        }
    }
}

/// The allotment of the directory `dir` on `host`, a tenant's: the one made already, or one made
/// now, counting the cells in it as they were counted, and those not counted yet, closed, as one
/// listing of each finds them.
pub fn allotment_on(host: &Arc<crate::Host>, dir: &Path) -> io::Result<Arc<Allotment>> {
    let dir = host_fs::canonicalize(dir)?;
    let made = {
        let mut allotments = host.footprints.allotments.lock().unwrap();
        if let Some(made) = allotments.iter().find(|allotment| allotment.dir == dir) {
            return Ok(made.clone());
        }
        let disk = HostDisk::of_on(host.bucket(), &dir)?;
        let made = Arc::new(Allotment {
            dir,
            tally: Tally::new(),
            limit: AtomicU64::new(NONE),
            group: Mutex::new(None),
            full: AtomicBool::new(false),
            disk,
            weight: AtomicU64::new(DEFAULT_WEIGHT),
            share: AtomicU64::new(NONE),
            total: Arc::clone(&host.footprints.total),
        });
        allotments.push(made.clone());
        made
    };
    let counted: Vec<_> = {
        let ledger = host.footprints.ledger.lock().unwrap();
        ledger.iter().flat_map(|ledger| ledger.values()).cloned().collect()
    };
    for footprint in counted.iter().filter(|footprint| made.holds(footprint)) {
        allot_nearest_on(host, footprint);
    }
    count_cells_in_on(host, &made.dir)?;
    Ok(made)
}

/// [`allotment_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn allotment(dir: &Path) -> io::Result<Arc<Allotment>> {
    allotment_on(crate::Host::process(), dir)
}

/// Counts the footprint of each cell in `dir` on `host` not counted yet, at any depth: one listing
/// of each. A cell directory is one holding its description, or what its deletion or making leaves
/// of it.
fn count_cells_in_on(host: &Arc<crate::Host>, dir: &Path) -> io::Result<()> {
    for entry in host_fs::read_dir(dir)? {
        let path = entry?.path();
        if !host_fs::is_dir(&path) {
            continue;
        }
        let cell = [crate::layout::DESCRIPTION, crate::host::DELETED, crate::cell::UNMADE]
            .iter()
            .any(|name| host_fs::exists(path.join(name)));
        if !cell {
            count_cells_in_on(host, &path)?;
            continue;
        }
        let id = dir_id(&path)?;
        let counted = host
            .footprints
            .ledger
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|ledger| ledger.contains_key(&id));
        if !counted {
            drop(crate::HostDir::new_on(host, &path)?);
        }
    }
    Ok(())
}

/// Counts `footprint` in the allotment on `host` whose directory holds it, the nearest, if any does.
fn allot_nearest_on(host: &crate::Host, footprint: &Footprint) {
    let nearest = {
        let allotments = host.footprints.allotments.lock().unwrap();
        let holding = allotments.iter().filter(|allotment| allotment.holds(footprint));
        holding.max_by_key(|allotment| allotment.dir.as_os_str().len()).cloned()
    };
    let Some(nearest) = nearest else { return };
    if footprint.allotment().is_none_or(|was| !Arc::ptr_eq(&was, &nearest)) {
        footprint.allot(&nearest);
    }
}

impl Allotment {
    /// Whether its directory holds `footprint`'s cell.
    fn holds(&self, footprint: &Footprint) -> bool {
        footprint.path().is_some_and(|path| path.starts_with(&self.dir) && path != self.dir)
    }

    /// Its directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What its cells' files take, counted.
    pub fn bytes(&self) -> u64 {
        self.tally.bytes()
    }

    /// Of those, unframed files'.
    pub fn unframed(&self) -> u64 {
        self.tally.unframed()
    }

    /// What writes admitted and not yet landed hold.
    pub fn pledged(&self) -> u64 {
        self.tally.pledged()
    }

    /// Its limit, if it has one.
    pub fn limit(&self) -> Option<u64> {
        Some(self.limit.load(Ordering::Relaxed)).filter(|limit| *limit != NONE)
    }

    /// Limits it to `bytes`, or lifts its limit, without.
    pub fn set_limit(&self, bytes: Option<u64>) {
        self.limit.store(bytes.unwrap_or(NONE), Ordering::Relaxed);
    }

    /// The disk of its directory's filesystem.
    pub fn disk(&self) -> &Arc<HostDisk> {
        &self.disk
    }

    /// Its weight against the other allotments on its disk.
    pub fn weight(&self) -> u64 {
        self.weight.load(Ordering::Relaxed)
    }

    /// Weighs it `weight` against the other allotments on its disk.
    pub fn set_weight(&self, weight: u64) {
        self.weight.store(weight, Ordering::Relaxed);
    }

    /// What it keeps while its disk is under pressure, if it is.
    pub fn share(&self) -> Option<u64> {
        Some(self.share.load(Ordering::Relaxed)).filter(|share| *share != NONE)
    }

    /// Has it keep `bytes` while its disk is under pressure, its share by weight; or, without, no
    /// share, its disk under pressure no more.
    pub fn set_share(&self, bytes: Option<u64>) {
        self.share.store(bytes.unwrap_or(NONE), Ordering::Relaxed);
    }

    /// What its cells take, reserve and hold admitted: what its limit and share weigh.
    pub fn committed(&self) -> u64 {
        self.tally.committed()
    }

    /// What its cells' own work may still add, and its writes admitted hold.
    pub fn reserved(&self) -> u64 {
        self.tally.reserved() + self.tally.pledged()
    }

    /// Whether the last write refused it was refused by its limit or every tenant's.
    pub fn is_full(&self) -> bool {
        self.full.load(Ordering::Relaxed)
    }

    /// Its multi-disk tenant group, if it belongs to one.
    pub fn group(&self) -> Option<Arc<Group>> {
        self.group.lock().unwrap().clone()
    }

    /// Rebinds this allotment to `group` (or detaches it from any group when `None`).
    pub fn set_group(&self, group: Option<Arc<Group>>) {
        *self.group.lock().unwrap() = group;
    }
}

/// What refused a write ([`Footprint::admit`]): `ENOSPC` to its guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DiskFull {
    /// Its cell's disk: what the disk governor allows its cells and the part cache.
    #[error("its disk's allowance")]
    Disk,
    /// Its tenant's limit.
    #[error("its tenant's limit")]
    Allotment,
    /// Every tenant's limit.
    #[error("every tenant's limit")]
    Total,
    /// Its tenant's share by weight of its disk, under pressure.
    #[error("its tenant's share of its disk under pressure")]
    Share,
}

/// Whether `disk` has no room for what is pledged on it: its cells' bytes, reserves and pledges,
/// and its part cache past a band of it, which the governor evicts at once, past its allowance.
fn disk_full(disk: &HostDisk) -> Option<DiskFull> {
    if !disk.accepts_writes() {
        return Some(DiskFull::Disk);
    }
    let (allowance, cache) = (disk.allowance(), disk.cache.get());
    let committed = disk.cells.committed();
    if committed.saturating_add(cache) > allowance {
        disk.wake_governor();
    }
    let band = disk.headroom() / 4;
    (committed.saturating_add(cache.saturating_sub(band)) > allowance).then_some(DiskFull::Disk)
}

/// A write admitted ([`Footprint::admit`]): what it holds of its cell's disk, allotment and every
/// allotment until it goes.
pub struct Pledge {
    footprint: Arc<Footprint>,
    bytes: u64,
}

impl Pledge {
    fn new(footprint: Arc<Footprint>, bytes: u64) -> Self {
        footprint.counting(|tally| tally.pledge(bytes, true));
        Self { footprint, bytes }
    }
}

impl Drop for Pledge {
    fn drop(&mut self) {
        self.footprint.counting(|tally| tally.pledge(self.bytes, false));
    }
}

/// Every allotment made on `host`.
pub fn allotments_on(host: &crate::Host) -> Vec<Arc<Allotment>> {
    host.footprints.allotments.lock().unwrap().clone()
}

/// Every allotment made on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn allotments() -> Vec<Arc<Allotment>> {
    allotments_on(crate::Host::process())
}

/// What every allotment's cells' files on `host` take together.
pub fn total_on(host: &crate::Host) -> &Tally {
    &host.footprints.total.tally
}

/// What every allotment's cells' files on the process's default host take together outside
/// `madsim`.
#[cfg(not(madsim))]
pub fn total() -> &'static Tally {
    total_on(crate::Host::process())
}

/// Every tenant's limit on `host`, if set.
pub fn total_limit_on(host: &crate::Host) -> Option<u64> {
    Some(host.footprints.total.limit.load(Ordering::Relaxed)).filter(|limit| *limit != NONE)
}

/// Every tenant's limit on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn total_limit() -> Option<u64> {
    total_limit_on(crate::Host::process())
}

/// Limits every allotment on `host` together to `bytes`, or lifts the limit, without.
pub fn set_total_limit_on(host: &crate::Host, bytes: Option<u64>) {
    host.footprints.total.limit.store(bytes.unwrap_or(NONE), Ordering::Relaxed);
}

/// `own`, a footprint's tally, counted in `tally`, or no longer, if not `more`.
fn moved(own: &Tally, tally: &Tally, more: bool) {
    let (bytes, unframed) = (own.bytes(), own.unframed());
    match more {
        true => {
            tally.charge(bytes - unframed, false);
            tally.charge(unframed, true);
        }
        false => {
            tally.discharge(bytes - unframed, false);
            tally.discharge(unframed, true);
        }
    }
    tally.opened(own.open(), more);
    (0..own.cells()).for_each(|_| tally.cell(more));
    tally.pledge(own.pledged(), more);
}

/// Whether `path` is an unframed file's: one under `open-segments/` or `open-extents/`.
fn unframed_path(path: &str) -> bool {
    let kind = path.split('/').next().unwrap_or_default();
    kind == OPEN_SEGMENTS || kind == OPEN_EXTENTS
}

/// cladefs: whether `path` is a segment's: one under `segments/`, whose holes an ephemeral cell
/// may have given back (doc-25, TASK-698.2, board 6905).
fn segment_dir_path(path: &str) -> bool {
    path.split('/').next().unwrap_or_default() == SEGMENTS
}

/// A file of a cell, as its footprint counts it: shared by all its names and openings.
pub struct FileLen {
    state: Mutex<LenState>,
}

#[derive(Default)]
struct LenState {
    /// Its length, counted: cladefs: but for the holes it gave back (TASK-698.2).
    len: u64,
    /// cladefs: its length, holes and all, counted: what a write grows it from.
    size: u64,
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
        let size = host_disk::counted(end);
        if size <= state.size {
            return;
        }
        let more = size - std::mem::replace(&mut state.size, size);
        state.len += more;
        if state.counted() {
            footprint.charge(more, state.unframed());
        }
    }

    /// cladefs: it takes `allocated` bytes of its disk, holes given back ([`crate::File::punch`],
    /// TASK-698.2): counted so, if it has a name, never past its length.
    pub(crate) fn settle(&self, footprint: &Footprint, allocated: u64) {
        let mut state = self.state.lock().unwrap();
        let len = host_disk::counted(allocated).min(state.size);
        let was = std::mem::replace(&mut state.len, len);
        if !state.counted() {
            return;
        }
        match len.cmp(&was) {
            std::cmp::Ordering::Less => footprint.discharge(was - len, state.unframed()),
            std::cmp::Ordering::Greater => footprint.charge(len - was, state.unframed()),
            std::cmp::Ordering::Equal => {}
        }
    }

    /// It is counted no more, whatever writes it, its cell directory gone.
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

/// A cell directory of the host's, by its device and inode.
type DirId = (u64, u64);

/// The directory at `path`'s device and inode.
fn dir_id(path: &Path) -> io::Result<DirId> {
    use host_fs::MetadataExt as _;
    let metadata = host_fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// The footprint of the cell whose directory is at `path` on `host`, on `disk`: the one counted
/// already, if it was opened on `host` before.
pub(crate) fn of_host_on(
    host: &crate::Host,
    path: &Path,
    disk: &Arc<HostDisk>,
) -> io::Result<Arc<Footprint>> {
    let id = dir_id(path)?;
    let mut ledger = host.footprints.ledger.lock().unwrap();
    let footprints = ledger.get_or_insert_with(HashMap::new);
    if let Some(footprint) = footprints.get(&id) {
        return Ok(footprint.clone());
    }
    let footprint = Footprint::new(Some(host_fs::canonicalize(path)?), Some(disk.clone()));
    footprints.insert(id, footprint.clone());
    drop(ledger);
    allot_nearest_on(host, &footprint);
    Ok(footprint)
}

/// Forgets the footprint of the cell whose directory is at `path` on `host`, if it was counted:
/// removed. `forgotten` runs between, the cell directory removed by it if it is `Ok`. A directory
/// that is not there was counted by none; one whose identity can't be read fails it, never left
/// counted for good.
pub(crate) fn removing_on<T>(
    host: &crate::Host,
    path: &Path,
    forgotten: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let id = match dir_id(path) {
        Ok(id) => Some(id),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(anyhow::Error::new(err).context(format!("{}", path.display()))),
    };
    let removed = forgotten()?;
    let mut ledger = host.footprints.ledger.lock().unwrap();
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

    /// The directories a cell keeps its files in, and names in them, some at the top.
    const DIRS: [&str; 5] = ["open-segments", "segments", "open-extents", "extents", "logs"];

    /// What `dir` holds, as its listing says, what its page cache holds dirty written back first:
    /// its files' and directories' bytes, counted, and of them its unframed files'. What a
    /// footprint must say of it after every change. Written back, as the page cache keeps a file's
    /// pages by the name it was opened at, and a file at two names opened at each would hold its
    /// pages in two places: no cell opens one so, but these tests do.
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

    /// Whether the file at `path` holds extents' data, as a cell opens it, the same way every
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

    /// Changes `dir` at random, a step of `seed`'s, as a cell would: files made, written past
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
                        host_fs::remove_file(path)?;
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
        let counted = (disk.cells.bytes() - before.0, disk.cells.unframed() - before.1);
        assert_eq!(counted, (bytes, unframed), "{at}: its disk");
    }

    /// TASK-539.4: a cell's footprint is what its directory's listing counts after every change,
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
            let before = (disk.cells.bytes(), disk.cells.unframed());
            DIRS.iter().for_each(|path| dir.create_dir(path).unwrap());
            let mut rng = StdRng::seed_from_u64(seed);
            let mut open = Vec::new();
            for at in 0..60 {
                let did = step(&dir, &mut rng, &mut open);
                agrees(&dir, dir.footprint(), before, &format!("seed {seed}, step {at}, {did}"));
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

    /// TASK-539.4: a cell of the host's has one footprint however many times it is opened at
    /// once, which it keeps while closed; a change made while closed, around it, is counted at its
    /// next opening; its removal forgets it, its disk counting nothing of it.
    #[test]
    fn host_cells_are_counted_once_while_open_and_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store");
        host_fs::create_dir(&path).unwrap();
        let dir = HostDir::new(&path).unwrap();
        let disk = dir.footprint().disk().unwrap().clone();
        let before = disk.cells.bytes();
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
        host_fs::write(path.join("f"), vec![3; 2 * GRANULE as usize]).unwrap();
        host_fs::remove_file(path.join("segments/1")).unwrap();
        let dir = HostDir::new(&path).unwrap();
        assert_eq!(dir.footprint().bytes(), 3 * GRANULE);
        assert_eq!(disk.cells.bytes() - before, 3 * GRANULE);
        drop(dir);
        crate::remove_cell_dir(&path).unwrap();
        assert_eq!((footprint.bytes(), disk.cells.bytes()), (0, before));
    }

    /// board 5442: a closed `LocalCell` kept alive by a reader must not clear a reopened store's
    /// open-extent reserve on drop, and concurrent `set_open` updates must not underflow the
    /// footprint, allotment, or disk tallies.
    #[test]
    fn closed_cell_drop_keeps_reopened_open_reserve_and_concurrent_set_open_matches() {
        use crate::{Description, LocalCell, Options};
        use bucket_cell::layout::test_cell;
        use storage_device::ExtentId;

        let temp = tempfile::tempdir().unwrap();
        let allotment = allotment(temp.path()).unwrap();
        let path = temp.path().join("store");
        host_fs::create_dir(&path).unwrap();
        let desc = Description { block_size: 4096, capacity: 16 << 20, cell: test_cell(42) };
        let dir1 = HostDir::new(&path).unwrap();
        let cell1 = Arc::new(LocalCell::create(dir1, desc, Options::default()).unwrap());
        let footprint = cell1.dir().footprint().clone();
        let disk = footprint.disk().unwrap().clone();

        let ext1 = ExtentId { cell: desc.cell.id, sequence: cell1.info().unwrap().next_sequence };
        futures::executor::block_on(cell1.write(ext1, 0, &vec![1u8; 8192])).unwrap();
        assert!(footprint.open() > 0, "uncommitted write must reserve open bytes");

        // Keep an extra Arc alive (like a reader holding the closed LocalCell) across close+reopen.
        let stale_reader = Arc::clone(&cell1);
        futures::executor::block_on(cell1.close()).unwrap();
        assert_eq!(footprint.open(), 0, "close must release open reserve before reopen");

        let dir2 = HostDir::new(&path).unwrap();
        let cell2 = LocalCell::open(dir2, Options::default()).unwrap();
        let ext2 = ExtentId { cell: desc.cell.id, sequence: cell2.info().unwrap().next_sequence };
        futures::executor::block_on(cell2.write(ext2, 0, &vec![2u8; 8192])).unwrap();
        let reopened_open = footprint.open();
        assert!(reopened_open > 0, "reopened cell must reserve open bytes");

        // Dropping the stale closed LocalCell must leave cell2's open reserve intact.
        drop(cell1);
        drop(stale_reader);
        assert_eq!(footprint.open(), reopened_open, "stale drop must not zero reopened reserve");
        assert!(allotment.tally.open() >= reopened_open);

        futures::executor::block_on(cell2.close()).unwrap();
        assert_eq!(footprint.open(), 0);
        drop(cell2);

        // Concurrent `set_open` on a shared footprint in an allotment must not underflow.
        let disk_open_before = disk.cells.open();
        let a = Arc::clone(&footprint);
        let b = Arc::clone(&footprint);
        let t1 = std::thread::spawn(move || {
            for i in 0..2_000u64 {
                a.set_open(if i % 2 == 0 { 64 << 10 } else { 0 });
            }
        });
        let t2 = std::thread::spawn(move || {
            for i in 0..2_000u64 {
                b.set_open(if i % 3 == 0 { 128 << 10 } else { 16 << 10 });
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        footprint.set_open(0);
        assert_eq!(footprint.open(), 0);
        assert_eq!(allotment.tally.open(), 0);
        assert_eq!(disk.cells.open(), disk_open_before);
        crate::remove_cell_dir(&path).unwrap();
    }

    #[test]
    fn group_enforces_aggregate_limit_across_multiple_allotments() {
        let host = crate::Host::new();
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let root_c = tempfile::tempdir().unwrap();
        let allot_a = allotment_on(&host, root_a.path()).unwrap();
        let allot_b = allotment_on(&host, root_b.path()).unwrap();
        let group = Group::for_allotments(vec![allot_a.clone(), allot_b.clone()]);

        let cell_a = root_a.path().join("cell-a");
        let cell_b = root_b.path().join("cell-b");
        let cell_c = root_c.path().join("cell-c");
        host_fs::create_dir(&cell_a).unwrap();
        host_fs::create_dir(&cell_b).unwrap();
        host_fs::create_dir(&cell_c).unwrap();
        let dir_a = HostDir::new_on(&host, &cell_a).unwrap();
        let dir_b = HostDir::new_on(&host, &cell_b).unwrap();
        let dir_c = HostDir::new_on(&host, &cell_c).unwrap();

        dir_a.create("a", false).unwrap().write_at(&vec![1; 4 * GRANULE as usize], 0).unwrap();
        dir_b.create("b", false).unwrap().write_at(&vec![2; 4 * GRANULE as usize], 0).unwrap();
        assert_eq!(allot_a.bytes(), 4 * GRANULE);
        assert_eq!(allot_b.bytes(), 4 * GRANULE);
        assert_eq!(group.bytes(), 8 * GRANULE);

        // Set aggregate limit just above current committed bytes so an additional write on either
        // directory is refused by the shared group limit.
        let committed = group.committed();
        group.set_limit(Some(committed + GRANULE));
        assert_eq!(
            dir_a.footprint().admit(2 * GRANULE, 0).err(),
            Some(DiskFull::Allotment),
            "write pushing aggregate over group limit must be refused"
        );
        assert_eq!(
            dir_b.footprint().admit(2 * GRANULE, 0).err(),
            Some(DiskFull::Allotment),
            "write on second directory must also observe group limit"
        );

        // Rebind existing allotments plus a newly added third directory into a new group (simulating
        // revoke + re-grant with [A, B, C]): all three must enforce the new group's limit.
        let allot_c = allotment_on(&host, root_c.path()).unwrap();
        let group2 = Group::for_allotments(vec![allot_a.clone(), allot_b.clone(), allot_c.clone()]);
        let committed2 = group2.committed();
        group2.set_limit(Some(committed2 + GRANULE));
        assert_eq!(
            dir_c.footprint().admit(2 * GRANULE, 0).err(),
            Some(DiskFull::Allotment),
            "newly added third root must enforce the shared group limit"
        );
        assert_eq!(
            dir_a.footprint().admit(2 * GRANULE, 0).err(),
            Some(DiskFull::Allotment),
            "reused root A must enforce the rebound group limit"
        );

        group2.set_limit(None);
        assert!(dir_a.footprint().admit(2 * GRANULE, 0).is_ok());
        assert!(dir_c.footprint().admit(2 * GRANULE, 0).is_ok());
    }
}
