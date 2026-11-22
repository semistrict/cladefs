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
#[cfg(all(test, feature = "shuttle"))]
pub(crate) use quarantine::{QuarantineForTest, quarantine_for_test};
pub use quarantine::{Quarantined, quarantine_with};
pub use relief::{RELIEF_BYTES, relieve_with};
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
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        // SAFETY: sysconf has no preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let size = u64::try_from(size).expect("the host's page size");
        assert!(SLAB.is_multiple_of(size) && SLAB / size <= 256, "pages of {size} bytes");
        size
    })
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

node_local::node_local! {
    /// Physical memory's size, once configured, read or used: the process's, or, under madsim, the
    /// simulated host's, each its own memory (doc-24, TASK-685.14).
    static MEMORY_BYTES: OnceLock<u64> = OnceLock::new();
}

/// Sets physical memory's size, `memory_bytes`, before its first use: whole slabs, one at least.
/// Fails with [`ConfigError::Invalid`] if not, and with [`ConfigError::InUse`] once its size was
/// configured, read ([`memory_bytes`]) or used. The host's limit on mappings bounds none of it, as
/// nothing of it is mapped here (doc-26).
pub fn configure(memory_bytes: u64) -> Result<(), ConfigError> {
    if memory_bytes == 0 {
        return Err(ConfigError::Invalid("no memory".into()));
    }
    if !memory_bytes.is_multiple_of(SLAB) {
        let why = format!("{memory_bytes} bytes, not whole slabs of {SLAB}");
        return Err(ConfigError::Invalid(why));
    }
    MEMORY_BYTES.get().set(memory_bytes).map_err(|_| ConfigError::InUse)
}

/// Physical memory's size, in bytes: as [`configure`] gave it, or [`DEFAULT_MEMORY_BYTES`]. Read,
/// it is fixed: [`configure`] fails from then on.
pub fn memory_bytes() -> u64 {
    *MEMORY_BYTES.get().get_or_init(|| DEFAULT_MEMORY_BYTES)
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
struct Node {
    /// The arenas, by ID, the default always among them; and the ID of the next made.
    arenas: Mutex<(BTreeMap<ArenaId, Arc<Arena>>, u32)>,
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
pub(crate) struct Arena {
    id: ArenaId,
    /// The physical memory it is of, which it counts in: under madsim, the simulated host's that
    /// made it, whatever host's task frees its runs (doc-24, TASK-685.14).
    node: &'static Node,
    object: object::Object,
    state: Mutex<ArenaState>,
}

node_local::node_local! {
    /// Physical memory, of [`memory_bytes`]' size, made at its first use: the process's, or, under
    /// madsim, the simulated host's, whose arenas go once its simulation ended (board 2890).
    static NODE: node_local::NodeLocal<OnceLock<Node>> =
        node_local::NodeLocal::releasing(OnceLock::new, release);
}

/// Physical memory, made at its first use.
fn node() -> &'static Node {
    NODE.get().get().get_or_init(Node::new)
}

/// Lets a simulated host's arenas go once its simulation ended (board 2890): their objects close
/// as the last of their runs is dropped, which, its every task gone, is now.
fn release(node: &'static OnceLock<Node>) {
    if let Some(node) = node.get() {
        let arenas = std::mem::take(&mut node.arenas.lock().unwrap().0);
        drop(arenas);
    }
}

impl Node {
    fn new() -> Self {
        let capacity = memory_bytes() / page_size();
        Self {
            arenas: Mutex::new((BTreeMap::new(), ArenaId::DEFAULT.0 + 1)),
            capacity,
            total_cap: AtomicU64::new(u64::MAX),
            populated: AtomicU64::new(0),
            punching: AtomicU64::new(0),
            spares: AtomicU64::new(0),
            quarantined: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            keep_spares: AtomicBool::new(true),
            alloc_failed_no_mem: AtomicBool::new(false),
            room: Pulse::new(),
        }
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

    /// The arena `id`, while it is not removed; the default made at its first use.
    fn arena(&'static self, id: ArenaId) -> Option<Arc<Arena>> {
        let mut arenas = self.arenas.lock().unwrap();
        if id == ArenaId::DEFAULT && arenas.0.is_empty() {
            let default = Arena::new(self, ArenaId::DEFAULT);
            arenas.0.insert(ArenaId::DEFAULT, default);
        }
        arenas.0.get(&id).cloned()
    }

    /// The arenas, the default among them.
    fn all(&'static self) -> Vec<Arc<Arena>> {
        self.arena(ArenaId::DEFAULT);
        self.arenas.lock().unwrap().0.values().cloned().collect()
    }

    /// Gives arenas' spares back, punching them, until `pages` more keep the arenas within
    /// [`Node::limit`], or none is left: whether any was. Other arenas' spares starve them no
    /// less than they make this one room, as spares are taken only by whole slabs.
    fn release_spares_for(&'static self, pages: u64) -> bool {
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
                arena.punch(spare);
                released = true;
            }
        }
        released
    }

    /// Lets allocations that may find room now know.
    fn pulse(&self) {
        self.room.pulse();
    }
}

impl Arena {
    /// A new arena of `node`'s, `id`, holding nothing: its object made, of no bytes.
    fn new(node: &'static Node, id: ArenaId) -> Arc<Self> {
        let object = object::Object::new().expect("an arena's memory object");
        let state = Mutex::new(ArenaState::new(slab_pages()));
        Arc::new(Self { id, node, object, state })
    }

    /// Where `run` begins in its object, in bytes.
    fn offset(run: Run) -> u64 {
        u64::from(run.slab) * SLAB + u64::from(run.first) * page_size()
    }

    /// Allocates a run of `pages` pages: one that may `wait` waits, while it is starved, for
    /// relief to make it room, and tries again; and fails once nothing will. It waits for runs
    /// being punched that would make it room, whatever `wait` says, as they are as good as free.
    fn alloc(self: &Arc<Self>, pages: u16, wait: Wait) -> Result<PartSlab, NoMemory> {
        let node = self.node;
        let wait_for_punches = rules().allocations_wait_for_punches;
        loop {
            // What it waits for comes after it tries.
            let room = node.room.count();
            let (taken, grew) = {
                let mut state = self.state.lock().unwrap();
                let taken = state.take(pages, wait_for_punches, |need| node.reserve(need));
                // Grown under the lock, so that two growths can't cross: a smaller one's truncate
                // after a larger one's would cut off what the larger's slab holds, where its object
                // isn't sealed against shrinking (macOS's file).
                let grew = match &taken {
                    Ok(Taken { grown: Some(slots), .. }) => {
                        self.object.grow(u64::from(*slots) * SLAB)
                    }
                    _ => Ok(()),
                };
                (taken, grew)
            };
            let starved = match taken {
                Ok(Taken { run, spare, .. }) => {
                    if spare {
                        node.spares.fetch_sub(u64::from(run.pages), Ordering::Relaxed);
                    }
                    let slab = PartSlab::new(self.clone(), run);
                    if let Err(err) = grew {
                        log::error!("cladefs: growing {:?}'s memory object: {err}", self.id);
                        return Err(NoMemory);
                    }
                    return Ok(slab);
                }
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

    /// Frees `run`, which it gave: kept as a spare, or punched, here and now, outside its lock.
    fn free(&self, run: Run) {
        let node = self.node;
        let keep = node.keep_spares.load(Ordering::Relaxed)
            && node.populated.load(Ordering::Relaxed) <= node.limit();
        let freed = {
            let mut state = self.state.lock().unwrap();
            let freed = state.free(run, keep);
            let counted = match freed {
                Freed::Punch => &node.punching,
                Freed::Spare => &node.spares,
            };
            counted.fetch_add(u64::from(run.pages), Ordering::Relaxed);
            freed
        };
        match freed {
            Freed::Spare => node.pulse(),
            Freed::Punch => self.punch(run),
        }
    }

    /// Punches `run`, which its state has being punched, out of its object, and tells its state;
    /// a punch that fails leaks the run, counted still, and says so (doc-26, Failure modes).
    fn punch(&self, run: Run) {
        let ps = page_size();
        let punched = self.object.punch(Self::offset(run), u64::from(run.pages) * ps);
        if let Err(err) = &punched {
            log::error!("cladefs: punching {run:?} of {:?} out: {err}: its memory leaked", self.id);
        }
        let released = self.state.lock().unwrap().punched(run, punched.is_ok());
        self.node.released(released);
        self.node.punching.fetch_sub(u64::from(run.pages), Ordering::Relaxed);
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
            self.punch(spare);
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
}

impl Drop for Held {
    fn drop(&mut self) {
        self.arena.free(self.run);
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
    fn new(arena: Arc<Arena>, run: Run) -> Self {
        Self { held: Some(Held { arena, run }), staged: Default::default() }
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

    /// The VM processes it was staged to.
    #[cfg(all(test, not(feature = "shuttle")))]
    pub(crate) fn staged(&self) -> Vec<VmProcessId> {
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

/// Allocates a run of `pages` pages of `arena`, a slab's or fewer: whole, a slab of its own; fewer,
/// a run of a small slab's. Its arena starved, one that may [`Wait`] waits for relief to make it
/// room, and fails once relief makes none; one that may not, or one with no relief, fails. One of
/// an arena removed fails at once.
pub fn alloc(arena: ArenaId, pages: u64, wait: Wait) -> Result<PartSlab, NoMemory> {
    let pages = u16::try_from(pages).ok().filter(|pages| (1..=slab_pages()).contains(pages));
    let pages = pages.unwrap_or_else(|| panic!("a run of {pages:?} pages, not of a slab"));
    node().arena(arena).ok_or(NoMemory)?.alloc(pages, wait)
}

/// A new arena, which holds no page.
pub fn create_arena() -> ArenaId {
    let node = node();
    node.arena(ArenaId::DEFAULT);
    let mut arenas = node.arenas.lock().unwrap();
    let id = ArenaId(arenas.1);
    arenas.1 += 1;
    arenas.0.insert(id, Arena::new(node, id));
    id
}

/// Removes `arena`, not the default: its allocations fail from now on, whether or not they may wait
/// (TASK-79.36), its spares are punched and its cap lifted; its object goes once the last of its
/// runs still allocated is freed. One removed already is let be.
pub fn remove_arena(arena: ArenaId) {
    assert_ne!(arena, ArenaId::DEFAULT, "the default arena stays");
    let node = node();
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

/// The shared memory object of `arena`, while it exists, for mapping its pages elsewhere.
pub fn arena_fd(arena: ArenaId) -> Option<Arc<OwnedFd>> {
    Some(node().arena(arena)?.object.fd().clone())
}

/// The default arena's shared memory object, for mapping its pages elsewhere.
pub fn default_fd() -> Arc<OwnedFd> {
    arena_fd(ArenaId::DEFAULT).expect("the default arena")
}

/// Caps the memory `arena` may hold, in bytes, rounded down to pages, or lifts its cap. Lowered
/// under what it holds, it takes no more until it holds less (TASK-50.3).
pub fn set_arena_cap(arena: ArenaId, bytes: Option<u64>) {
    let node = node();
    if let Some(arena) = node.arena(arena) {
        arena.state.lock().unwrap().cap = bytes.map(|bytes| bytes / page_size());
    }
    caps_changed(node);
}

/// Caps the memory the arenas may hold together, in bytes, rounded down to pages, or lifts the
/// cap.
pub fn set_total_cap(bytes: Option<u64>) {
    let node = node();
    node.total_cap.store(bytes.map_or(u64::MAX, |bytes| bytes / page_size()), Ordering::Relaxed);
    caps_changed(node);
}

/// A cap changed: no arena is exhausted, and allocations waiting may find room.
fn caps_changed(node: &'static Node) {
    for arena in node.all() {
        arena.state.lock().unwrap().relief.exhausted = false;
    }
    node.pulse();
}

/// The cap of the memory the arenas may hold together, in bytes, if any.
pub fn total_cap() -> Option<u64> {
    let cap = node().total_cap.load(Ordering::Relaxed);
    (cap != u64::MAX).then(|| cap * page_size())
}

/// The memory `arena` holds, in bytes: the pages of its object that hold memory, its spares'
/// included; none once it is removed.
pub fn arena_bytes(arena: ArenaId) -> u64 {
    node().arena(arena).map_or(0, |arena| arena.bytes().0)
}

/// The memory `arena` holds in use, in bytes: what [`arena_bytes`] counts but its spares', which
/// [`release_spares`] gives back. What a shrink evicts down to before it gives them back, as its
/// evictions' frees may keep slabs spare.
pub fn arena_bytes_in_use(arena: ArenaId) -> u64 {
    node().arena(arena).map_or(0, |arena| {
        let (held, spares) = arena.bytes();
        held - spares
    })
}

/// The memory the arenas hold together, in bytes, those removed and not yet empty too.
pub fn arenas_bytes() -> u64 {
    node().populated.load(Ordering::Relaxed) * page_size()
}

/// The most memory the arenas held together at once so far, in bytes.
pub fn arenas_peak_bytes() -> u64 {
    node().peak.load(Ordering::Relaxed) * page_size()
}

/// The memory the arenas hold together in use, in bytes, as [`arena_bytes_in_use`] counts an
/// arena's.
pub fn arenas_bytes_in_use() -> u64 {
    let node = node();
    let populated = node.populated.load(Ordering::Relaxed);
    populated.saturating_sub(node.spares.load(Ordering::Relaxed)) * page_size()
}

/// Gives the spares of `arena`, or of every arena, back: memory they hold that nothing uses
/// (TASK-50.3).
pub fn release_spares(arena: Option<ArenaId>) {
    let node = node();
    for found in node.all() {
        if arena.is_none_or(|arena| found.id == arena) {
            found.release_spares();
        }
    }
}

/// Sets whether arenas keep spares, as they do unless told not to; those they have go back if not.
pub fn keep_spare_slabs(keep: bool) {
    node().keep_spares.store(keep, Ordering::Relaxed);
    if !keep {
        release_spares(None);
    }
}

/// The bytes of physical memory.
pub fn count_total_bytes() -> u64 {
    node().capacity * page_size()
}

/// The memory leaked, its punch failed (doc-26, Failure modes): what should never be.
pub fn leaked_bytes() -> u64 {
    let pages: u64 = node().all().iter().map(|arena| arena.state.lock().unwrap().leaked).sum();
    pages * page_size()
}

/// Whether an allocation has failed as memory is out: past the arenas' total, or memory's.
pub fn has_alloc_failed_no_mem() -> bool {
    node().alloc_failed_no_mem.load(Ordering::Relaxed)
}

/// Whether the page at `location` is free: neither held nor quarantined. `None` if its arena's
/// object holds no slab there.
pub fn is_free(location: PhysLocation) -> Option<bool> {
    let arena = node().arena(location.arena)?;
    let (slab, page) = (location.offset / SLAB, (location.offset % SLAB) / page_size());
    arena.state.lock().unwrap().is_free(u32::try_from(slab).ok()?, page as u16)
}
