// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Physical memory in part slabs (doc-26): the memory the extent cache's parts, and the zeros
//! windows map, are of. Physical memory is in arenas, each a shared memory object of its own (a
//! memfd on Linux) that a VM process given it maps pages of by their [`PhysLocation`]: one per
//! tenant, and the default one, which public bases' parts are of and every VM process maps
//! (TASK-43).
//!
//! An arena is a sequence of slabs of 1 MiB ([`SLAB`], a part's size), at slab-aligned offsets of
//! its object. A whole part takes a whole slab; a part shorter (an extent's tail, or a small
//! extent) takes a run of a small slab's pages, consecutive, so that a VM process maps a part with
//! one mapping either way. A run is a [`PartSlab`], its holder's, which frees it as it is dropped:
//! at once, or, if it was staged to a VM process ([`PartSlab::stage_to`]) and a quarantine is given
//! ([`quarantine_with`]), once those VM processes have made the changes taking it away. A run freed
//! is punched out of its object (`fallocate(PUNCH_HOLE)`), its memory back to the host, but for
//! whole slabs kept as spares ([`arena`]'s `SPARE_SLABS`).
//!
//! The daemon maps none of it: it writes a part in with `pwrite`, reads it with `pread`, copies one
//! to another arena with `copy_file_range`, and punches it out. So it reserves no address space,
//! and the host's limit on its mappings (`vm.max_map_count`) bounds nothing here.
//!
//! The memory is bounded: in all, by its size ([`configure`], or [`DEFAULT_MEMORY_BYTES`]), and by
//! the total cap ([`set_total_cap`]); each arena by its own ([`set_arena_cap`]), in pages. What an
//! arena holds is the pages of its object that hold memory, exactly: its whole slabs', its
//! spares', its small slabs' runs ([`arena_bytes`]), what the governor counts. An allocation that
//! finds its arena starved, past a cap, asks for relief ([`relieve_with`]), which evicts parts, and
//! may wait for it (`relief`).

mod arena;
mod object;
mod quarantine;
mod relief;
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests;
mod sync;
#[cfg(all(test, not(feature = "shuttle")))]
mod tests;

use arena::{ArenaState, Freed, Refused, Run, Starved, Taken};
pub use object::SlabResidency;
/// Verifies at startup that the Linux host kernel supports `cachestat(2)` (Linux 6.5+).
#[cfg(all(target_os = "linux", not(madsim)))]
pub fn require_cachestat() -> std::io::Result<()> {
    object::Object::require_cachestat()
}

/// Queries Linux `cachestat(2)` on a previously snapshotted `(fd, offset, len)` range outside any
/// extent lock.
#[cfg_attr(any(not(target_os = "linux"), madsim), allow(dead_code))]
pub fn cachestat_range(fd: &OwnedFd, offset: u64, len: u64) -> Option<SlabResidency> {
    object::Object::cachestat_fd(fd, offset, len)
}
#[cfg(all(test, feature = "shuttle"))]
pub use quarantine::{QuarantineForTest, quarantine_for_test};
pub use quarantine::{Quarantined, quarantine_with};
pub use relief::RELIEF_BYTES;
#[cfg(not(madsim))]
pub use relief::relieve_with;
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use sync::{Mutex, Pulse, rules};

/// A slab's size: a part's (`extent_frame::PART`), which the extent cache brings in and evicts
/// whole.
pub const SLAB: u64 = 1 << 20;
const _: () = assert!(SLAB == extent_frame::PART, "a slab is a part");

/// The size of physical memory unless [`configure`] gives another: 1 GiB.
pub const DEFAULT_MEMORY_BYTES: u64 = 1 << 30;

/// An arena of physical memory: its own shared memory object (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArenaId(pub u32);

impl ArenaId {
    /// The arena of every page not of a tenant's: the public one.
    pub const DEFAULT: Self = Self(0);
}

/// Where a page of physical memory is: its arena, and its offset in the arena's object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PhysLocation {
    /// Its arena.
    pub arena: ArenaId,
    /// Its offset in the arena's object, in bytes.
    pub offset: u64,
}

/// A VM process a run is staged to ([`PartSlab::stage_to`]): the daemon's number for its
/// connection, which its quarantine knows it by (doc-26, Quarantine scoped to what was staged),
/// never another's ([`VmProcessId::connected`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VmProcessId(pub u64);

/// The VM processes connected, by ID: those a run staged to one gone forgets as it is staged to
/// another ([`PartSlab::stage_to`]).
static CONNECTED: std::sync::Mutex<std::collections::BTreeSet<u64>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

impl VmProcessId {
    /// The ID of a VM process connecting, which no other ever had: one reused would have a run
    /// staged to the VM process gone wait for the next, another tenant's perhaps.
    pub fn connected() -> Self {
        // Numbers the process gives out: unique among simulated hosts too, all it asks.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        CONNECTED.lock().unwrap().insert(id);
        Self(id)
    }

    /// Its VM process is gone, its mappings with it: a run staged to it forgets it.
    pub fn gone(self) {
        CONNECTED.lock().unwrap().remove(&self.0);
    }
}

/// The host's page size (sysconf), which pages are: such as 16 KiB on Apple silicon.
pub fn page_size() -> u64 {
    // The process's page size, the hardware's: the same for every simulated host.
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        // SAFETY: sysconf has no preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let size = u64::try_from(size).expect("the host's page size");
        assert!(pages_fit(size), "pages of {size} bytes");
        size
    })
}

/// Whether a host's pages of `size` bytes are pages slabs are made of: a slab a whole number of
/// them, 256 at most, so 4 KiB to 1 MiB. What the daemon's doctor checks of the host's before
/// anything asks its page size, which panics otherwise (board 14217).
pub fn pages_fit(size: u64) -> bool {
    size > 0 && SLAB.is_multiple_of(size) && SLAB / size <= 256
}

/// The pages of a slab.
fn slab_pages() -> u16 {
    (SLAB / page_size()) as u16
}

/// Why [`configure`] failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Physical memory's size was configured already, or read, or physical memory is in use:
    /// EBUSY.
    InUse,
    /// The size is not whole slabs, or is zero: EINVAL. Says which.
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InUse => f.write_str("physical memory has already been configured or used"),
            Self::Invalid(why) => write!(f, "invalid physical memory size: {why}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Explicit per-host physical memory state (board 10962), replacing `MEMORY_BYTES` and `NODE`
/// `node_local!` statics. Dropping a [`HostSlabs`] releases all of its arenas (closing their
/// shared memory objects once the last allocated run drops) and stops its relief thread if started.
pub struct HostSlabs {
    memory_bytes: OnceLock<u64>,
    node: OnceLock<Arc<Node>>,
    reliever: Arc<OnceLock<Option<Arc<relief::Reliever>>>>,
    pending_governor: std::sync::Mutex<Option<relief::GovernorTick>>,
}

impl HostSlabs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            memory_bytes: OnceLock::new(),
            node: OnceLock::new(),
            reliever: Arc::new(OnceLock::new()),
            pending_governor: std::sync::Mutex::new(None),
        })
    }

    pub fn configure(&self, memory_bytes: u64) -> Result<(), ConfigError> {
        if memory_bytes == 0 {
            return Err(ConfigError::Invalid("no memory".into()));
        }
        if !memory_bytes.is_multiple_of(SLAB) {
            let why = format!("{memory_bytes} bytes, not whole slabs of {SLAB}");
            return Err(ConfigError::Invalid(why));
        }
        self.memory_bytes.set(memory_bytes).map_err(|_| ConfigError::InUse)
    }

    pub fn memory_bytes(&self) -> u64 {
        *self.memory_bytes.get_or_init(|| DEFAULT_MEMORY_BYTES)
    }

    pub(super) fn node(&self) -> &Arc<Node> {
        self.node.get_or_init(|| Arc::new(Node::new(self.memory_bytes(), self.reliever.clone())))
    }

    pub(super) fn pulse_if_init(&self) {
        if let Some(node) = self.node.get() {
            node.pulse();
        }
    }
}

impl Drop for HostSlabs {
    fn drop(&mut self) {
        if let Some(Some(reliever)) = self.reliever.get() {
            reliever.stop();
        }
        if let Some(node) = self.node.get() {
            let arenas = std::mem::take(&mut node.arenas.lock().unwrap().0);
            drop(arenas);
        }
    }
}

/// Sets physical memory's size, `memory_bytes`, before its first use: whole slabs, one at least.
/// Fails with [`ConfigError::Invalid`] if not, and with [`ConfigError::InUse`] once its size was
/// configured, read ([`memory_bytes`]) or used. The host's limit on mappings bounds none of it, as
/// nothing of it is mapped here (doc-26).
#[cfg(not(madsim))]
pub fn configure(memory_bytes: u64) -> Result<(), ConfigError> {
    crate::Host::process().slabs().configure(memory_bytes)
}

/// Physical memory's size, in bytes: as [`configure`] gave it, or [`DEFAULT_MEMORY_BYTES`]. Read,
/// it is fixed: [`configure`] fails from then on.
#[cfg(not(madsim))]
pub fn memory_bytes() -> u64 {
    crate::Host::process().slabs().memory_bytes()
}

/// Whether an allocation may wait for room: for relief ([`relieve_with`]) to make its arena some,
/// while it is starved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// It fails at once.
    No,
    /// It waits for relief, as long as relief may make it room.
    Yes,
}

/// An allocation failed: nothing makes its arena room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMemory;

impl std::fmt::Display for NoMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("out of physical memory")
    }
}

impl std::error::Error for NoMemory {}

/// Physical memory: its arenas, and what bounds them together.
pub(super) struct Node {
    /// The arenas, by ID, the default always among them; and the ID of the next made.
    arenas: Mutex<(BTreeMap<ArenaId, Arc<Arena>>, u32)>,
    /// The relief thread slot shared with [`HostSlabs`].
    reliever: Arc<OnceLock<Option<Arc<relief::Reliever>>>>,
    /// The cap currently being relieved on the eviction thread, keyed by thread ID so no
    /// process-wide thread-local is needed (TASK-161, board 12315, board 11703).
    relieving: std::sync::Mutex<Option<(std::thread::ThreadId, relief::Relieving)>>,
    /// The most pages the arenas may populate together: physical memory's size.
    capacity: u64,
    /// The most the total cap lets them, or `u64::MAX`.
    total_cap: AtomicU64,
    /// The pages they populate together, every arena's, those removed and not yet empty too.
    populated: AtomicU64,
    /// Of those, the pages being punched.
    punching: AtomicU64,
    /// Of those, the pages of spares.
    spares: AtomicU64,
    /// Of those, the pages of runs quarantined, every arena's, those removed too: room on its way,
    /// which relief exhausts no arena starved at the total while any is to come back (TASK-132).
    quarantined: AtomicU64,
    /// cladefs: of those, the pages of runs held for a wedged VM process (board 8535): not on their
    /// way, as they come back only once it exits, if ever ([`Quarantined::wedged`]).
    wedged: AtomicU64,
    /// Pages evicted by relief for `Starved::Total` that are still quarantined and not wedged:
    /// shared across all arenas so multiple arenas starved at the total cap do not trigger
    /// redundant eviction passes (board 12315).
    total_owed: AtomicU64,
    /// Whether the last `Starved::Total` relief pass freed nothing with nothing returning from
    /// quarantine: cleared when any slab is freed, a total relief pass frees bytes, or a cap
    /// changes (board 12315).
    total_exhausted: AtomicBool,
    /// Fast-path flag mirroring whether `total_backoff` has an active deadline, so `Arena::free`
    /// avoids taking `total_backoff`'s mutex on the common unbacked-off path (board 12315).
    total_backoff_active: AtomicBool,
    /// Exponential backoff after a 0-byte `Starved::Total` relief pass (board 12315).
    total_backoff: Mutex<relief::Backoff>,
    /// The most pages they populated together at once, so far.
    peak: AtomicU64,
    /// Whether arenas keep spares.
    keep_spares: AtomicBool,
    /// Whether an allocation has failed as memory is out.
    alloc_failed_no_mem: AtomicBool,
    /// Pulsed as an allocation of a starved arena may find room, or must fail now: a run freed or
    /// punched, a cap changed, relief's pass for an arena done, relief gone (TASK-50.3). Every
    /// waiter wakes, as the waiters of one arena's may not be those of the next's.
    room: Pulse,
}

/// An arena (see the module docs): its memory object, and its slots, under its lock, which is
/// taken after any other and holds none.
pub struct Arena {
    id: ArenaId,
    /// The physical memory it is of, which it counts in: whatever host's task frees its runs
    /// (doc-24, TASK-685.14, board 10962).
    node: Arc<Node>,
    object: object::Object,
    state: Mutex<ArenaState>,
}

/// Physical memory of the process's default host, made at its first use.
#[cfg(all(test, not(madsim)))]
pub(super) fn node() -> &'static Arc<Node> {
    crate::Host::process().slabs().node()
}

impl Node {
    fn new(memory_bytes: u64, reliever: Arc<OnceLock<Option<Arc<relief::Reliever>>>>) -> Self {
        let capacity = memory_bytes / page_size();
        Self {
            arenas: Mutex::new((BTreeMap::new(), ArenaId::DEFAULT.0 + 1)),
            reliever,
            relieving: std::sync::Mutex::new(None),
            capacity,
            total_cap: AtomicU64::new(u64::MAX),
            populated: AtomicU64::new(0),
            punching: AtomicU64::new(0),
            spares: AtomicU64::new(0),
            quarantined: AtomicU64::new(0),
            wedged: AtomicU64::new(0),
            total_owed: AtomicU64::new(0),
            total_exhausted: AtomicBool::new(false),
            total_backoff_active: AtomicBool::new(false),
            total_backoff: Mutex::new(relief::Backoff::default()),
            peak: AtomicU64::new(0),
            keep_spares: AtomicBool::new(true),
            alloc_failed_no_mem: AtomicBool::new(false),
            room: Pulse::new(),
        }
    }

    fn reliever(&self) -> Option<&relief::Reliever> {
        self.reliever.get()?.as_deref()
    }

    fn relieving(&self) -> Option<relief::Relieving> {
        let tid = std::thread::current().id();
        self.relieving.lock().unwrap().and_then(|(id, target)| (id == tid).then_some(target))
    }

    fn set_relieving(&self, target: Option<relief::Relieving>) {
        let tid = std::thread::current().id();
        *self.relieving.lock().unwrap() = target.map(|t| (tid, t));
    }

    /// The most pages the arenas may populate together, as the caps stand.
    fn limit(&self) -> u64 {
        self.capacity.min(self.total_cap.load(Ordering::Relaxed))
    }

    /// Reserves `pages` more for an arena to populate, if they keep the arenas within
    /// [`Node::limit`]: whether they do.
    fn reserve(&self, pages: u64) -> bool {
        let limit = self.limit();
        let reserved = self.populated.try_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
            (now + pages <= limit).then_some(now + pages)
        });
        if let Ok(before) = reserved {
            self.peak.fetch_max(before + pages, Ordering::Relaxed);
        }
        reserved.is_ok()
    }

    /// `pages` an arena populated no longer are.
    fn released(&self, pages: u64) {
        self.populated.fetch_sub(pages, Ordering::Relaxed);
    }

    /// Whether `pages` more would keep the arenas within [`Node::limit`].
    fn fits(&self, pages: u64) -> bool {
        self.populated.load(Ordering::Relaxed) + pages <= self.limit()
    }

    /// The arena `id`, while it is not removed; the default made at its first use, or none if its
    /// memory object can't be made, which [`Node::default_arena`] logs.
    pub(super) fn arena(self: &Arc<Self>, id: ArenaId) -> Option<Arc<Arena>> {
        if id == ArenaId::DEFAULT {
            // Its error logged by `default_arena`: none, which fails what asked for it (NoMemory).
            return self.default_arena().ok();
        }
        self.arenas.lock().unwrap().0.get(&id).cloned()
    }

    /// cladefs: the default arena, made at its first use: an error, logged, if its memory object
    /// can't be made, as with as many files open as the process may (board 8491). Made holding no
    /// lock, so that a failure leaves the arenas as they were, and the next use tries again.
    fn default_arena(self: &Arc<Self>) -> std::io::Result<Arc<Arena>> {
        if let Some(default) = self.arenas.lock().unwrap().0.get(&ArenaId::DEFAULT) {
            return Ok(default.clone());
        }
        let made = Arena::new(self.clone(), ArenaId::DEFAULT).inspect_err(|err| {
            log::error!("cladefs: making the default arena's memory object: {err}");
        })?;
        let mut arenas = self.arenas.lock().unwrap();
        Ok(arenas.0.entry(ArenaId::DEFAULT).or_insert(made).clone())
    }

    /// The arenas, the default among them, if it could be made.
    pub(super) fn all(self: &Arc<Self>) -> Vec<Arc<Arena>> {
        // Made here if it can be, or left out, which `default_arena` logged.
        let _ = self.default_arena();
        self.arenas.lock().unwrap().0.values().cloned().collect()
    }

    /// Gives arenas' spares back, punching them, until `pages` more keep the arenas within
    /// [`Node::limit`], or none is left: whether any was. Other arenas' spares starve them no
    /// less than they make this one room, as spares are taken only by whole slabs.
    fn release_spares_for(self: &Arc<Self>, pages: u64) -> bool {
        let mut released = false;
        for arena in self.all() {
            while !self.fits(pages) {
                let spare = {
                    let mut state = arena.state.lock().unwrap();
                    let spare = state.release_spare();
                    if spare.is_some() {
                        self.spares.fetch_sub(u64::from(slab_pages()), Ordering::Relaxed);
                        self.punching.fetch_add(u64::from(slab_pages()), Ordering::Relaxed);
                    }
                    spare
                };
                let Some(spare) = spare else { break };
                arena.punch(spare, true);
                released = true;
            }
        }
        released
    }

    /// Lets allocations that may find room now know.
    pub(super) fn pulse(&self) {
        self.room.pulse();
    }

    pub(super) fn record_total_empty_backoff(&self, now: std::time::Instant) {
        let mut backoff = self.total_backoff.lock().unwrap();
        backoff.record_empty(now);
        self.total_backoff_active.store(true, Ordering::Release);
    }

    pub(super) fn clear_total_backoff(&self) {
        if self.total_backoff_active.load(Ordering::Acquire) {
            let mut backoff = self.total_backoff.lock().unwrap();
            backoff.clear();
            self.total_backoff_active.store(false, Ordering::Release);
        }
    }

    pub(super) fn total_backoff_active_at(&self, now: std::time::Instant) -> bool {
        if !self.total_backoff_active.load(Ordering::Acquire) {
            return false;
        }
        self.total_backoff.lock().unwrap().active(now)
    }
}

impl Arena {
    /// A new arena of `node`'s, `id`, holding nothing: its object made, of no bytes; or why it
    /// couldn't be (board 8491).
    fn new(node: Arc<Node>, id: ArenaId) -> std::io::Result<Arc<Self>> {
        let object = object::Object::new()?;
        let state = Mutex::new(ArenaState::new(slab_pages()));
        Ok(Arc::new(Self { id, node, object, state }))
    }

    /// Where `run` begins in its object, in bytes.
    fn offset(run: Run) -> u64 {
        u64::from(run.slab) * SLAB + u64::from(run.first) * page_size()
    }

    /// Allocates a run of `pages` pages: one that may `wait` waits, while it is starved, for
    /// relief to make it room, and tries again; and fails once nothing will. It waits for runs
    /// being punched that would make it room, whatever `wait` says, as they are as good as free.
    fn alloc(self: &Arc<Self>, pages: u16, wait: Wait) -> Result<PartSlab, NoMemory> {
        let node = &self.node;
        let (Taken { run, spare, .. }, grew) = self.with_room(pages, wait, |state, waits| {
            let taken = state.take(pages, waits, |need| node.reserve(need))?;
            let grew = self.grow_for(&taken);
            Ok((taken, grew))
        })?;
        if spare {
            node.spares.fetch_sub(u64::from(run.pages), Ordering::Relaxed);
        }
        let slab = PartSlab::new(self.clone(), run, true);
        if let Err(err) = grew {
            log::error!("cladefs: growing {:?}'s memory object: {err}", self.id);
            return Err(NoMemory);
        }
        Ok(slab)
    }

    /// cladefs: places a run of `pages` pages (board 14629): its place in the object, holding no
    /// memory, which neither cap counts, until it is populated ([`PartSlab::populate`]). Fails
    /// only of an arena removed, or an object that can't grow.
    fn place(self: &Arc<Self>, pages: u16) -> Result<PartSlab, NoMemory> {
        let (taken, grew) = {
            let mut state = self.state.lock().unwrap();
            let taken = state.place(pages).map_err(|_| NoMemory)?;
            let grew = self.grow_for(&taken);
            (taken, grew)
        };
        let slab = PartSlab::new(self.clone(), taken.run, false);
        if let Err(err) = grew {
            log::error!("cladefs: growing {:?}'s memory object: {err}", self.id);
            return Err(NoMemory);
        }
        Ok(slab)
    }

    /// Grows its object to the slots `taken` says it grows to, if it grows: under its lock, so
    /// that two growths can't cross: a smaller one's truncate after a larger one's would cut off
    /// what the larger's slab holds, where its object isn't sealed against shrinking (macOS's
    /// file).
    fn grow_for(&self, taken: &Taken) -> std::io::Result<()> {
        match taken.grown {
            Some(slots) => self.object.grow(u64::from(slots) * SLAB),
            None => Ok(()),
        }
    }

    /// cladefs: populates `run`, placed, as a part is to be paged in there: as [`Arena::alloc`]
    /// waits for room for a run, waiting for room for its pages.
    fn populate(self: &Arc<Self>, run: Run, wait: Wait) -> Result<(), NoMemory> {
        let node = &self.node;
        self.with_room(run.pages, wait, |state, waits| {
            state.populate(run, waits, |need| node.reserve(need))
        })
    }

    /// cladefs: punches `run`, held still, out of its object, its pages populating it no more and
    /// its place kept (board 14629): a punch that fails leaks them, counted still.
    fn depopulate(&self, run: Run) {
        let ps = page_size();
        let punched = self.object.punch(Self::offset(run), u64::from(run.pages) * ps);
        if let Err(err) = &punched {
            log::error!("cladefs: punching {run:?} of {:?} out: {err}: its memory leaked", self.id);
        }
        // Room made, as a free makes it: relief's verdicts of before stand no more.
        let released = {
            let mut state = self.state.lock().unwrap();
            state.relief.exhausted = false;
            state.relief.backoff.clear();
            state.depopulated(run, punched.is_ok())
        };
        self.node.released(released);
        self.node.total_exhausted.store(false, Ordering::Release);
        self.node.clear_total_backoff();
        self.node.pulse();
    }

    /// Does what `under_lock` does under its lock, which may find it starved for `pages`: one
    /// that may `wait` waits, while it is starved, for relief to make it room, and tries again;
    /// and fails once nothing will. It waits for runs being punched that would make it room,
    /// whatever `wait` says, as they are as good as free. `under_lock` is told whether to wait
    /// for punches.
    fn with_room<T>(
        self: &Arc<Self>,
        pages: u16,
        wait: Wait,
        mut under_lock: impl FnMut(&mut ArenaState, bool) -> Result<T, Refused>,
    ) -> Result<T, NoMemory> {
        let node = &self.node;
        let wait_for_punches = rules().allocations_wait_for_punches;
        loop {
            // What it waits for comes after it tries.
            let room = node.room.count();
            let tried = {
                let mut state = self.state.lock().unwrap();
                under_lock(&mut state, wait_for_punches)
            };
            let starved = match tried {
                Ok(done) => return Ok(done),
                Err(Refused::Punching) => {
                    node.room.wait_past(room);
                    continue;
                }
                Err(Refused::Starved(Starved::Removed)) => return Err(NoMemory),
                Err(Refused::Starved(Starved::Total)) => {
                    let need = u64::from(pages);
                    if wait_for_punches && node.punching.load(Ordering::Relaxed) > 0 {
                        node.room.wait_past(room);
                        continue;
                    }
                    if node.release_spares_for(need) {
                        continue;
                    }
                    Starved::Total
                }
                // Its spares, which a run shorter than a slab takes none of, given back first.
                Err(Refused::Starved(Starved::Arena)) if self.release_spares() > 0 => continue,
                Err(Refused::Starved(Starved::Arena)) => Starved::Arena,
            };
            match relief::starved(self, starved, pages, wait) {
                relief::Asked::Wait => node.room.wait_past(room),
                relief::Asked::Fail => {
                    if starved == Starved::Total {
                        node.alloc_failed_no_mem.store(true, Ordering::Relaxed);
                    }
                    return Err(NoMemory);
                }
            }
        }
    }

    /// Why it is starved for a run of `pages` now, if it is: past its own cap, or, the pages it
    /// would populate past the total's, with nothing being punched; or removed.
    fn starved(&self, pages: u16) -> Option<Starved> {
        let wait_for_punches = rules().allocations_wait_for_punches;
        let state = self.state.lock().unwrap();
        match state.refusal(pages, wait_for_punches) {
            Some(Refused::Starved(starved)) => Some(starved),
            Some(Refused::Punching) => None,
            None => {
                let need = state.need(pages);
                let punching = wait_for_punches && self.node.punching.load(Ordering::Relaxed) > 0;
                (need > 0 && !self.node.fits(need) && !punching).then_some(Starved::Total)
            }
        }
    }

    /// Frees `run`, which it gave: kept as a spare if it `may_spare`, or punched, here and now,
    /// outside its lock. cladefs: one not `populated` makes no room (board 14629).
    fn free(&self, run: Run, populated: bool, may_spare: bool) {
        let node = &self.node;
        node.total_exhausted.store(false, Ordering::Release);
        node.clear_total_backoff();
        let keep = may_spare
            && node.keep_spares.load(Ordering::Relaxed)
            && node.populated.load(Ordering::Relaxed) <= node.limit();
        let freed = {
            let mut state = self.state.lock().unwrap();
            let freed = state.free(run, keep, populated);
            let counted = match freed {
                Freed::Punch => &node.punching,
                Freed::Spare => &node.spares,
            };
            if populated {
                counted.fetch_add(u64::from(run.pages), Ordering::Relaxed);
            }
            freed
        };
        match freed {
            Freed::Spare => node.pulse(),
            Freed::Punch => self.punch(run, populated),
        }
    }

    /// Punches `run`, which its state has being punched, out of its object, and tells its state;
    /// a punch that fails leaks the run, counted still, and says so (doc-26, Failure modes).
    /// cladefs: a run not `populated` was counted by nothing (board 14629).
    fn punch(&self, run: Run, populated: bool) {
        let ps = page_size();
        let punched = self.object.punch(Self::offset(run), u64::from(run.pages) * ps);
        if let Err(err) = &punched {
            log::error!("cladefs: punching {run:?} of {:?} out: {err}: its memory leaked", self.id);
        }
        let released = self.state.lock().unwrap().punched(run, punched.is_ok(), populated);
        self.node.released(released);
        if populated {
            self.node.punching.fetch_sub(u64::from(run.pages), Ordering::Relaxed);
        }
        self.node.pulse();
    }

    /// Gives its spares back, punching them: how many it had.
    fn release_spares(&self) -> usize {
        let spares = {
            let mut state = self.state.lock().unwrap();
            let spares = state.release_spares();
            let pages = spares.iter().map(|run| u64::from(run.pages)).sum();
            self.node.spares.fetch_sub(pages, Ordering::Relaxed);
            self.node.punching.fetch_add(pages, Ordering::Relaxed);
            spares
        };
        let released = spares.len();
        for spare in spares {
            self.punch(spare, true);
        }
        released
    }

    /// The bytes of memory it holds: the pages of its object that hold memory; and, of those, the
    /// bytes its spares hold.
    fn bytes(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap();
        let spares = state.spares() as u64 * u64::from(slab_pages());
        (state.populated * page_size(), spares * page_size())
    }
}

/// A run of an arena's pages, allocated: a whole slab, a part's, or a run of a small slab's pages,
/// consecutive in the arena's object, its holder's alone, not to be cloned. Dropping it frees it:
/// at once, or, if it was staged to VM processes ([`PartSlab::stage_to`]) and a quarantine is given
/// ([`quarantine_with`]), once they have made the changes taking it away. Its contents are not
/// zeroed as it is allocated.
pub struct PartSlab {
    /// Its run, freed as it is dropped (taken as it is: always there until then).
    held: Option<Held>,
    /// The VM processes it was staged to since it was allocated, each once: its own, not its
    /// slab's, as a small slab holds several parts' runs (doc-26). A lock of its own, taken under
    /// any other, holding none.
    staged: std::sync::Mutex<SmallVec<[VmProcessId; 4]>>,
}

/// A run of an arena's, held: freed to the arena as it is dropped, whoever holds it then, its
/// [`PartSlab`] or the quarantine.
struct Held {
    arena: Arc<Arena>,
    run: Run,
    /// cladefs: whether its pages are populated, counted against the caps: a run placed is not
    /// until it is populated, nor one depopulated after (board 14629).
    populated: AtomicBool,
    /// cladefs: whether a VM process's memory backend installed its pages in the VM's page tables
    /// ([`PartSlab::list`], board 14630): freed, it is punched, which takes them out of those.
    listed: AtomicBool,
}

impl Drop for Held {
    fn drop(&mut self) {
        let listed = self.listed.load(Ordering::Acquire);
        self.arena.free(self.run, self.populated.load(Ordering::Acquire), !listed);
    }
}

impl std::fmt::Debug for PartSlab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.held();
        f.debug_struct("PartSlab")
            .field("arena", &held.arena.id)
            .field("run", &held.run)
            .finish_non_exhaustive()
    }
}

impl PartSlab {
    fn new(arena: Arc<Arena>, run: Run, populated: bool) -> Self {
        let held = Held {
            arena,
            run,
            populated: AtomicBool::new(populated),
            listed: AtomicBool::new(false),
        };
        Self { held: Some(held), staged: Default::default() }
    }

    /// cladefs: whether its pages are populated: a run allocated is; one placed is not until
    /// populated, nor one depopulated after (board 14629).
    pub fn is_populated(&self) -> bool {
        self.held().populated.load(Ordering::Acquire)
    }

    /// cladefs: populates it, placed ([`HostSlabs::place`]) or depopulated, before a part is
    /// paged in here: its pages counted against the caps, waiting for room as an allocation would
    /// if it may `wait`. Nothing to do of one populated.
    pub fn populate(&self, wait: Wait) -> Result<(), NoMemory> {
        let held = self.held();
        if held.populated.load(Ordering::Acquire) {
            return Ok(());
        }
        held.arena.populate(held.run, wait)?;
        held.populated.store(true, Ordering::Release);
        Ok(())
    }

    /// cladefs: punches its memory out, keeping its place (board 14629): what a part placed by a
    /// list leaves as it is evicted, so that it is paged in here again. Nothing to do of one not
    /// populated. Its holder makes sure nothing maps it shared meanwhile that would read the
    /// hole: no VM process it was staged to.
    pub fn depopulate(&self) {
        let held = self.held();
        if held.populated.swap(false, Ordering::AcqRel) {
            held.arena.depopulate(held.run);
        }
    }

    /// cladefs: a VM process's memory backend installs its pages in the VM's page tables, a
    /// private mapping of its arena's object (board 14630): which only a punch takes out of them.
    /// Freed, it is punched therefore, never kept a spare, populated still, that another part fills
    /// for the VM to read: as it is freed at once, or once out of quarantine. Called under its
    /// part's lock, which no eviction passes, as the pages are installed.
    pub fn list(&self) {
        self.held().listed.store(true, Ordering::Release);
    }

    fn held(&self) -> &Held {
        self.held.as_ref().expect("a slab held until dropped")
    }

    /// Where it begins: its arena, and its offset in the arena's object.
    pub fn location(&self) -> PhysLocation {
        let held = self.held();
        PhysLocation { arena: held.arena.id, offset: Arena::offset(held.run) }
    }

    /// Its arena.
    pub fn arena(&self) -> ArenaId {
        self.held().arena.id
    }

    /// How many pages it is.
    pub fn pages(&self) -> u64 {
        u64::from(self.held().run.pages)
    }

    /// Its length, in bytes.
    pub fn len(&self) -> u64 {
        self.pages() * page_size()
    }

    /// Kernel page-cache residency of this slab's range via Linux `cachestat(2)`, if supported.
    pub fn residency(&self) -> Option<SlabResidency> {
        self.held().arena.object.cachestat(self.location().offset, self.len())
    }

    /// Cloned handle to `(arena_fd, offset, len)` so callers can invoke [`cachestat_range`]
    /// without holding any extent lock across the syscall.
    pub fn residency_handle(&self) -> (Arc<OwnedFd>, u64, u64) {
        let held = self.held();
        (Arc::clone(held.arena.object.fd()), Arena::offset(held.run), self.len())
    }

    /// Whether this slab still occupies the exact `(offset, len)` byte range within its arena
    /// backing object (used to ignore stale `cachestat(2)` samples after an extent part was
    /// evicted and refilled while the lock was dropped).
    #[cfg_attr(any(not(target_os = "linux"), madsim), allow(dead_code))]
    pub fn same_range(&self, offset: u64, len: u64) -> bool {
        Arena::offset(self.held().run) == offset && self.len() == len
    }

    /// Whether the page at `at` is one of its.
    pub fn contains(&self, at: PhysLocation) -> bool {
        let start = self.location();
        at.arena == start.arena && (start.offset..start.offset + self.len()).contains(&at.offset)
    }

    /// Writes `data` into it at its byte `at`, within it.
    pub fn write(&self, at: u64, data: &[u8]) -> std::io::Result<()> {
        assert!(at + data.len() as u64 <= self.len(), "a write past a slab");
        self.held().arena.object.write_at(self.location().offset + at, data)
    }

    /// Reads its bytes at its byte `at` into `buf`, within it.
    pub fn read(&self, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
        assert!(at + buf.len() as u64 <= self.len(), "a read past a slab");
        self.held().arena.object.read_at(self.location().offset + at, buf)
    }

    /// Writes zeros into it from its byte `at` to its end.
    pub fn zero(&self, at: u64) -> std::io::Result<()> {
        static ZEROS: [u8; 64 << 10] = [0; 64 << 10];
        let mut at = at;
        while at < self.len() {
            let len = (self.len() - at).min(ZEROS.len() as u64);
            self.write(at, &ZEROS[..len as usize])?;
            at += len;
        }
        Ok(())
    }

    /// Copies `from`, as long, into it, in the kernel where it can (TASK-234: a tenant's copy of a
    /// part promoted into the default arena, with no bounce through this process's memory).
    pub fn copy_from(&self, from: &PartSlab) -> std::io::Result<()> {
        assert_eq!(self.len(), from.len(), "a copy between slabs of other lengths");
        let to = self.location().offset;
        let (source, at) = (&from.held().arena.object, from.location().offset);
        source.copy_to(at, &self.held().arena.object, to, self.len())
    }

    /// It is staged to `vm`: named to its window by a stage operation, which may map it there
    /// until `vm` has made the operation taking it away. Freed, it waits for that (doc-26).
    /// It forgets the VM processes gone it was staged to as it is staged to another, so that it is
    /// staged to as many as were connected at once, at most, however many came and went.
    pub fn stage_to(&self, vm: VmProcessId) {
        let mut staged = self.staged.lock().unwrap();
        if !staged.contains(&vm) {
            let connected = CONNECTED.lock().unwrap();
            staged.retain(|staged| connected.contains(&staged.0));
            staged.push(vm);
        }
    }

    /// cladefs: whether it was staged to a VM process since it was allocated, which may map it
    /// shared until that VM process has made the change taking it away (board 14629).
    pub fn is_staged(&self) -> bool {
        !self.staged.lock().unwrap().is_empty()
    }

    /// The VM processes it was staged to.
    #[cfg(all(test, not(feature = "shuttle")))]
    pub fn staged(&self) -> Vec<VmProcessId> {
        self.staged.lock().unwrap().to_vec()
    }
}

impl Drop for PartSlab {
    fn drop(&mut self) {
        let staged =
            std::mem::take(&mut *self.staged.lock().unwrap_or_else(|err| err.into_inner()));
        let held = self.held.take().expect("a slab dropped once");
        quarantine::free(held, staged);
    }
}

/// `pages` as a run's: of a slab at most, one at least. Past a u16, or out of that: a run of no
/// slab, which panics.
fn run_pages(pages: u64) -> u16 {
    let run = u16::try_from(pages).ok().filter(|pages| (1..=slab_pages()).contains(pages));
    run.unwrap_or_else(|| panic!("a run of {pages} pages, not of a slab"))
}

impl HostSlabs {
    pub fn alloc(&self, arena: ArenaId, pages: u64, wait: Wait) -> Result<PartSlab, NoMemory> {
        self.node().arena(arena).ok_or(NoMemory)?.alloc(run_pages(pages), wait)
    }

    /// cladefs: places a run of `pages` pages in `arena` (board 14629): a place in its object a
    /// part is to be paged in to, holding no memory, so neither cap counts it, until populated
    /// ([`PartSlab::populate`]). Fails only of an arena removed, or one whose object can't grow.
    pub fn place(&self, arena: ArenaId, pages: u64) -> Result<PartSlab, NoMemory> {
        self.node().arena(arena).ok_or(NoMemory)?.place(run_pages(pages))
    }

    pub fn create_arena(&self) -> std::io::Result<ArenaId> {
        let node = self.node();
        node.default_arena()?;
        let id = {
            let mut arenas = node.arenas.lock().unwrap();
            let id = ArenaId(arenas.1);
            arenas.1 += 1;
            id
        };
        let made = Arena::new(node.clone(), id)?;
        node.arenas.lock().unwrap().0.insert(id, made);
        Ok(id)
    }

    pub fn remove_arena(&self, arena: ArenaId) {
        assert_ne!(arena, ArenaId::DEFAULT, "the default arena stays");
        let node = self.node();
        let Some(removed) = node.arenas.lock().unwrap().0.remove(&arena) else { return };
        {
            let mut state = removed.state.lock().unwrap();
            state.retired = true;
            state.cap = None;
        }
        removed.release_spares();
        // Its allocations waiting for room fail now.
        node.pulse();
    }

    pub fn arena_fd(&self, arena: ArenaId) -> Option<Arc<OwnedFd>> {
        Some(self.node().arena(arena)?.object.fd().clone())
    }

    pub fn default_fd(&self) -> std::io::Result<Arc<OwnedFd>> {
        Ok(self.node().default_arena()?.object.fd().clone())
    }

    pub fn set_arena_cap(&self, arena: ArenaId, bytes: Option<u64>) {
        let node = self.node();
        if let Some(arena) = node.arena(arena) {
            arena.state.lock().unwrap().cap = bytes.map(|bytes| bytes / page_size());
        }
        caps_changed(node);
    }

    pub fn set_total_cap(&self, bytes: Option<u64>) {
        let node = self.node();
        node.total_cap
            .store(bytes.map_or(u64::MAX, |bytes| bytes / page_size()), Ordering::Relaxed);
        caps_changed(node);
    }

    pub fn total_cap(&self) -> Option<u64> {
        let cap = self.node().total_cap.load(Ordering::Relaxed);
        (cap != u64::MAX).then(|| cap * page_size())
    }

    pub fn arena_bytes(&self, arena: ArenaId) -> u64 {
        self.node().arena(arena).map_or(0, |arena| arena.bytes().0)
    }

    pub fn arena_bytes_in_use(&self, arena: ArenaId) -> u64 {
        self.node().arena(arena).map_or(0, |arena| {
            let (held, spares) = arena.bytes();
            held - spares
        })
    }

    pub fn arenas_bytes(&self) -> u64 {
        self.node().populated.load(Ordering::Relaxed) * page_size()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn arenas_peak_bytes(&self) -> u64 {
        self.node().peak.load(Ordering::Relaxed) * page_size()
    }

    pub fn arenas_bytes_in_use(&self) -> u64 {
        let node = self.node();
        let populated = node.populated.load(Ordering::Relaxed);
        populated.saturating_sub(node.spares.load(Ordering::Relaxed)) * page_size()
    }

    pub fn release_spares(&self, arena: Option<ArenaId>) {
        let node = self.node();
        for found in node.all() {
            if arena.is_none_or(|arena| found.id == arena) {
                found.release_spares();
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn keep_spare_slabs(&self, keep: bool) {
        self.node().keep_spares.store(keep, Ordering::Relaxed);
        if !keep {
            self.release_spares(None);
        }
    }

    pub fn count_total_bytes(&self) -> u64 {
        self.node().capacity * page_size()
    }

    pub fn leaked_bytes(&self) -> u64 {
        let pages: u64 =
            self.node().all().iter().map(|arena| arena.state.lock().unwrap().leaked).sum();
        pages * page_size()
    }

    pub fn quarantined_bytes(&self) -> u64 {
        let pages: u64 =
            self.node().all().iter().map(|arena| arena.state.lock().unwrap().quarantined).sum();
        pages * page_size()
    }

    pub fn wedged_bytes(&self) -> u64 {
        self.node().wedged.load(Ordering::Relaxed) * page_size()
    }

    pub fn exhausted_arenas(&self) -> u64 {
        let all = self.node().all();
        all.iter().filter(|arena| arena.state.lock().unwrap().relief.exhausted).count() as u64
    }

    pub fn room_may_come(&self, arena: ArenaId) -> bool {
        self.node().arena(arena).is_some_and(|arena| {
            let state = arena.state.lock().unwrap();
            !state.retired && state.cap.is_none_or(|cap| state.populated < cap)
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn has_alloc_failed_no_mem(&self) -> bool {
        self.node().alloc_failed_no_mem.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_free(&self, location: PhysLocation) -> Option<bool> {
        let arena = self.node().arena(location.arena)?;
        let (slab, page) = (location.offset / SLAB, (location.offset % SLAB) / page_size());
        // A slab past a u32: none of the arena's.
        arena.state.lock().unwrap().is_free(u32::try_from(slab).ok()?, page as u16)
    }

    pub fn relieve_with(
        self: &Arc<Self>,
        evict: impl FnMut(Option<ArenaId>, u64) -> u64 + Send + 'static,
    ) -> std::io::Result<()> {
        relief::relieve_with_on(self, evict)
    }

    /// Attaches a periodic memory-governor tick onto this host's single OS eviction thread,
    /// returning `true` if attached to a live eviction thread or `false` if no OS thread runs
    /// (Shuttle / madsim) so the caller can fall back to an async task (board 11703).
    #[cfg(not(madsim))]
    pub fn attach_governor(
        &self,
        interval: std::time::Duration,
        tick: impl FnMut(&mut dyn FnMut()) + Send + 'static,
    ) -> bool {
        relief::attach_governor_on(self, interval, tick)
    }

    /// Wakes the eviction thread immediately (used when counted quarantined slabs return,
    /// board 11703).
    #[cfg(not(madsim))]
    pub fn signal_eviction(&self) {
        if let Some(Some(reliever)) = self.reliever.get() {
            reliever.signal();
        }
    }

    /// Waits on the eviction thread's signal up to `duration` (used inside synchronous governor
    /// quarantine waits so starved arena asks and returning slabs wake immediately, board 11703).
    #[cfg(not(madsim))]
    pub fn wait_eviction(&self, duration: std::time::Duration) {
        if let Some(Some(reliever)) = self.reliever.get() {
            reliever.wait_timeout(duration);
        } else {
            std::thread::sleep(duration);
        }
    }
}

/// A cap changed: no arena is exhausted, and allocations waiting may find room.
fn caps_changed(node: &Arc<Node>) {
    node.total_exhausted.store(false, Ordering::Release);
    node.clear_total_backoff();
    for arena in node.all() {
        let mut state = arena.state.lock().unwrap();
        state.relief.exhausted = false;
        state.relief.backoff.clear();
    }
    node.pulse();
}

/// Allocates a run of `pages` pages of `arena`, a slab's or fewer: whole, a slab of its own; fewer,
/// a run of a small slab's. Its arena starved, one that may [`Wait`] waits for relief to make it
/// room, and fails once relief makes none; one that may not, or one with no relief, fails. One of
/// an arena removed fails at once.
#[cfg(not(madsim))]
pub fn alloc(arena: ArenaId, pages: u64, wait: Wait) -> Result<PartSlab, NoMemory> {
    crate::Host::process().slabs().alloc(arena, pages, wait)
}

/// cladefs: places a run of `pages` pages of `arena` (board 14629), holding no memory until it is
/// populated ([`PartSlab::populate`]): as [`HostSlabs::place`], the process's host's.
#[cfg(not(madsim))]
pub fn place(arena: ArenaId, pages: u64) -> Result<PartSlab, NoMemory> {
    crate::Host::process().slabs().place(arena, pages)
}

/// A new arena, which holds no page; or why its memory object, or the default arena's, couldn't
/// be made, as with as many files open as the process may (board 8491): made holding no lock, so
/// that a failure leaves the arenas as they were.
#[cfg(not(madsim))]
pub fn create_arena() -> std::io::Result<ArenaId> {
    crate::Host::process().slabs().create_arena()
}

/// Removes `arena`, not the default: its allocations fail from now on, whether or not they may wait
/// (TASK-79.36), its spares are punched and its cap lifted; its object goes once the last of its
/// runs still allocated is freed. One removed already is let be.
#[cfg(not(madsim))]
pub fn remove_arena(arena: ArenaId) {
    crate::Host::process().slabs().remove_arena(arena)
}

/// The shared memory object of `arena`, while it exists, for mapping its pages elsewhere.
#[cfg(not(madsim))]
pub fn arena_fd(arena: ArenaId) -> Option<Arc<OwnedFd>> {
    crate::Host::process().slabs().arena_fd(arena)
}

/// The default arena's shared memory object, for mapping its pages elsewhere; or why it couldn't
/// be made (board 8491).
#[cfg(not(madsim))]
pub fn default_fd() -> std::io::Result<Arc<OwnedFd>> {
    crate::Host::process().slabs().default_fd()
}

/// Caps the memory `arena` may hold, in bytes, rounded down to pages, or lifts its cap. Lowered
/// under what it holds, it takes no more until it holds less (TASK-50.3).
#[cfg(not(madsim))]
pub fn set_arena_cap(arena: ArenaId, bytes: Option<u64>) {
    crate::Host::process().slabs().set_arena_cap(arena, bytes)
}

/// Caps the memory the arenas may hold together, in bytes, rounded down to pages, or lifts the
/// cap.
#[cfg(not(madsim))]
pub fn set_total_cap(bytes: Option<u64>) {
    crate::Host::process().slabs().set_total_cap(bytes)
}

/// The cap of the memory the arenas may hold together, in bytes, if any.
#[cfg(not(madsim))]
pub fn total_cap() -> Option<u64> {
    crate::Host::process().slabs().total_cap()
}

/// The memory `arena` holds, in bytes: the pages of its object that hold memory, its spares'
/// included; none once it is removed.
#[cfg(not(madsim))]
pub fn arena_bytes(arena: ArenaId) -> u64 {
    crate::Host::process().slabs().arena_bytes(arena)
}

/// The memory `arena` holds in use, in bytes: what [`arena_bytes`] counts but its spares', which
/// [`release_spares`] gives back. What a shrink evicts down to before it gives them back, as its
/// evictions' frees may keep slabs spare.
#[cfg(not(madsim))]
pub fn arena_bytes_in_use(arena: ArenaId) -> u64 {
    crate::Host::process().slabs().arena_bytes_in_use(arena)
}

/// The memory the arenas hold together, in bytes, those removed and not yet empty too.
#[cfg(not(madsim))]
pub fn arenas_bytes() -> u64 {
    crate::Host::process().slabs().arenas_bytes()
}

/// The most memory the arenas held together at once so far, in bytes.
#[cfg(not(madsim))]
pub fn arenas_peak_bytes() -> u64 {
    crate::Host::process().slabs().arenas_peak_bytes()
}

/// The memory the arenas hold together in use, in bytes, as [`arena_bytes_in_use`] counts an
/// arena's.
#[cfg(not(madsim))]
pub fn arenas_bytes_in_use() -> u64 {
    crate::Host::process().slabs().arenas_bytes_in_use()
}

/// Gives the spares of `arena`, or of every arena, back: memory they hold that nothing uses
/// (TASK-50.3).
#[cfg(not(madsim))]
pub fn release_spares(arena: Option<ArenaId>) {
    crate::Host::process().slabs().release_spares(arena)
}

/// Sets whether arenas keep spares, as they do unless told not to; those they have go back if not.
#[cfg(not(madsim))]
pub fn keep_spare_slabs(keep: bool) {
    crate::Host::process().slabs().keep_spare_slabs(keep)
}

/// The bytes of physical memory.
#[cfg(not(madsim))]
pub fn count_total_bytes() -> u64 {
    crate::Host::process().slabs().count_total_bytes()
}

/// The memory leaked, its punch failed (doc-26, Failure modes): what should never be.
#[cfg(not(madsim))]
pub fn leaked_bytes() -> u64 {
    crate::Host::process().slabs().leaked_bytes()
}

/// cladefs: the memory of runs freed and quarantined, in bytes, every arena's: what comes back
/// once every VM process it was staged to unmaps it (board 5662's gauges).
#[cfg(not(madsim))]
pub fn quarantined_bytes() -> u64 {
    crate::Host::process().slabs().quarantined_bytes()
}

/// cladefs: the memory of runs held for wedged VM processes, in bytes, every arena's (board 8535):
/// of [`quarantined_bytes`], what comes back only once those VM processes exit, if ever.
#[cfg(not(madsim))]
pub fn wedged_bytes() -> u64 {
    crate::Host::process().slabs().wedged_bytes()
}

/// cladefs: how many arenas' relief found nothing to free while they were starved, so that their
/// allocations fail rather than wait.
#[cfg(not(madsim))]
pub fn exhausted_arenas() -> u64 {
    crate::Host::process().slabs().exhausted_arenas()
}

/// cladefs: whether room may come for `arena`, an allocation of which found none: it is below its
/// own cap, so that what starved it is the arenas' total, the host's memory, which the governor
/// raises as VMs give theirs back (board 5662). An arena at its own cap, a tenant at its limit,
/// gets none but by evicting its own; one removed, never.
#[cfg(not(madsim))]
pub fn room_may_come(arena: ArenaId) -> bool {
    crate::Host::process().slabs().room_may_come(arena)
}

/// Whether an allocation has failed as memory is out: past the arenas' total, or memory's.
#[cfg(not(madsim))]
pub fn has_alloc_failed_no_mem() -> bool {
    crate::Host::process().slabs().has_alloc_failed_no_mem()
}

/// Whether the page at `location` is free: neither held nor quarantined. `None` if its arena's
/// object holds no slab there.
#[cfg(not(madsim))]
pub fn is_free(location: PhysLocation) -> Option<bool> {
    crate::Host::process().slabs().is_free(location)
}
