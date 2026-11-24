// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! DAX windows: guest physical memory into which an extent-store device maps extents, so that the
//! guest's Fxfs reads their pages directly instead of copying them through requests.
//!
//! A window is a range of guest physical memory, whose mappings each map a range of a [`Source`]'s
//! pages (`crate::cache`), read-only: a sealed extent's cached pages, shared by every window that
//! maps it, which are read in from the cell as they are first needed; or zeros. The guest's loads
//! fault pages in. The window keeps the pages it has mapped, each a page of physical memory
//! (`crate::pages`), as page tables do; a source evicting a page unmaps it from each window that
//! maps it first.
//!
//! The window maps its pages in its [`SecondStage`], if it has one: its translation outside the
//! daemon. A VM's window is the extent daemon's (`crate::cladefsd`), whose second stage is its
//! connection to the VM process, which makes each change in the VM's memory. KVM maps guest memory
//! from the host memory backing it, and follows changes to it: the VM process's own stage
//! (`dax::uffd`) takes that memory over, maps the window's pages into it, and reports the faults
//! KVM takes there itself, with userfaultfd, to the daemon, which passes them to the window
//! ([`DaxWindow::fault`]). A window without a second stage, as tests' are, has its pages mapped at
//! its host address.
//!
//! cladefs: a writer's window copies on write (backlog doc-13), in its second stage, the VM
//! process's (doc-26): a write to a page the window names there, a sealed extent's or zeros, gives
//! the page a private copy holding what the page held, mapped writable at the same offset, the VM
//! process's own memory (`dax::uffd`), which the window knows nothing of. The mapping stays the
//! source's. Only the guest's `MAP` or `UNMAP` over a private page ends it ([`Private::End`]):
//! a source evicting its pages and a free mapping zeros leave it ([`Private::Keep`]), and so does
//! a page mapped there anew. A window that doesn't copy, a host test's, refuses every write.

pub(crate) mod failures;
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests;
#[cfg(target_os = "linux")]
pub(crate) mod uffd;

use crate::error::{Errno, Result};
use crate::open_cell::{MappingContents, WindowMapping};
use crate::pages::{PhysLocation, page_size};
use crate::slabs::{PartSlab, VmProcessId};
use futures::future::BoxFuture;
use local_cell::shared::SharedLock;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use storage_device::ExtentId;

#[cfg(feature = "shuttle")]
use shuttle::sync::Mutex;
#[cfg(not(feature = "shuttle"))]
use std::sync::Mutex;

/// The access a guest's fault on a window was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// A load, or an instruction fetch.
    Read,
    /// A write.
    Write,
}

/// A window's second stage: a translation of it outside the daemon, which follows its mappings,
/// such as the extent daemon's connection to a VM process, which makes them in the VM's memory.
///
/// Each change the window makes to its pages is made here, rather than in this process's memory,
/// with offsets into the window. Calls for a window are serialized. They cannot fail: if the
/// hypervisor refuses one, the guest's memory no longer matches the window's, and the
/// implementation must panic.
pub trait SecondStage: Send + Sync {
    /// Maps the `len` bytes at `offset`, where nothing is mapped, read-only, to the pages at
    /// `phys` of physical memory, consecutive in one arena's object
    /// ([`arena_memory`](crate::pages::arena_memory)), from which another process can map them:
    /// a write to them must fault.
    fn map(&self, offset: u64, phys: PhysLocation, len: u64);

    /// Unmaps the mapped `len` bytes at `offset`. cladefs: the private pages there, a stage's that
    /// copies on write, go or stay as `private` says (doc-26): the guest's `MAP` or `UNMAP` ends
    /// them; the host's own changes, an eviction or a free's zeros, leave them. A window that
    /// copies sends a guest's over the whole range, as it knows nothing of which pages are private.
    fn unmap(&self, offset: u64, len: u64, private: Private);

    /// cladefs: the window maps the `len` bytes at `offset` anew; called once it has, before any of
    /// the pages it maps there now are mapped here, as the window maps a page here only once it is
    /// in memory. A stage that withholds pages a fault found nothing at (`dax::uffd`) gives them
    /// back, so that accesses to them fault again.
    fn window_mapped(&self, _offset: u64, _len: u64) {}

    /// cladefs: the window begins a change of its mappings, which may unmap what it maps, then map
    /// ranges anew ([`window_mapped`](Self::window_mapped)) and their pages in memory: what it
    /// makes here until [`end_change`](Self::end_change) is one change, which a stage in another
    /// process, sent it whole (`crate::cladefsd`), may make at once ([`remap`](Self::remap),
    /// TASK-91.16). The window makes a change under its lock, as it makes every call here, so
    /// that nothing else of its comes between: changes don't nest, and a stage may hold back
    /// what a change makes until it ends, which no wait comes before.
    fn begin_change(&self) {}

    /// cladefs: ends [`begin_change`](Self::begin_change).
    fn end_change(&self) {}

    /// cladefs: unmaps the mapped `len` bytes at `offset`, as `private` says, then makes `anew`,
    /// each within them, in order: as [`unmap`](Self::unmap), then
    /// [`window_mapped`](Self::window_mapped) or [`map`](Self::map) for each, would one after
    /// another, the window's change of a range it mapped, which a stage may make at once rather
    /// than in steps (TASK-91.16).
    fn remap(&self, offset: u64, len: u64, private: Private, anew: &[Anew]) {
        self.unmap(offset, len, private);
        for anew in anew {
            match *anew {
                Anew::Mapped { offset, len } => self.window_mapped(offset, len),
                Anew::Map { offset, phys, len } => self.map(offset, phys, len),
            }
        }
    }

    /// The VM process it is in, which makes its changes after they return: the runs the window
    /// maps here are staged to it (`PartSlab::stage_to`), and wait for it once freed (doc-26).
    /// None for a stage that makes each change before it returns, as tests' do.
    fn vm_process(&self) -> Option<VmProcessId> {
        None
    }
}

/// cladefs: one of the mappings [`Dax::map_many`] makes: `len` bytes at the window's `offset`, of
/// `source`'s bytes from its byte `from`, a page's multiple, or zeros, with no source (TASK-92.14).
pub(crate) struct DaxMapping {
    pub offset: u64,
    pub len: u64,
    pub source: Option<(Arc<dyn Source>, u64)>,
}

/// cladefs: one of the mappings [`Window::map_many`] makes: `len` bytes at the window's `offset`,
/// of `source`'s pages from its page `page`.
#[derive(Clone, Copy)]
struct Piece<'a> {
    offset: u64,
    len: u64,
    source: &'a Arc<dyn Source>,
    page: u64,
}

/// cladefs: what a window's change makes of a range it unmapped ([`SecondStage::remap`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anew {
    /// The window maps the range anew ([`SecondStage::window_mapped`]).
    Mapped { offset: u64, len: u64 },
    /// It maps the pages there ([`SecondStage::map`]).
    Map { offset: u64, phys: PhysLocation, len: u64 },
}

/// A hypervisor that maps a window's host memory for the guest itself, such as KVM, which the
/// window tells of the pages it maps there, so that it can map them for the guest ahead of the
/// guest's accesses rather than on a fault on each page, as KVM's `KVM_PRE_FAULT_MEMORY` does.
#[cfg(target_os = "linux")]
pub trait Prefault: Send + Sync {
    /// Maps the `len` bytes at `offset` into the window for the guest, which the window has mapped
    /// in its host memory: those the guest asks for to read next (TASK-112.1), and a private page
    /// as it is made (cladefs: the rest are left to the guest's accesses, TASK-109). It is called
    /// with the window's mappings locked, and must not wait on the window, or for the guest.
    fn prefault(&self, offset: u64, len: u64);
}

/// What resolves the faults on a window's second stage in host memory: the VM process's connection
/// to the extent daemon, which holds the window (`crate::cladefsd`), or the window itself, in tests
/// whose window is in this process's host memory (`Dax::in_host_memory`).
#[cfg(target_os = "linux")]
pub(crate) trait Faults: Send + Sync {
    /// The window's size in bytes.
    fn size(&self) -> u64;

    /// As [`DaxWindow::fault`], for a load of the page at `offset`, which the stage hasn't: cladefs:
    /// or a write's, which the stage copies once the window has named the page (doc-26), or
    /// refuses.
    fn fault_page(&self, offset: u64) -> BoxFuture<'_, Result<()>>;

    /// cladefs: maps the page at `offset`, to read, ahead of the guest's accesses, as
    /// [`fault_page`](Self::fault_page) does: a probe of the stage's own, of a page the
    /// guest may map nothing at, which fails then as quietly as it may (`ENOENT`), as no access of
    /// the guest's was refused.
    fn probe(&self, offset: u64) -> BoxFuture<'_, Result<()>>;

    /// cladefs: whether what resolves the faults went, as a VM process's daemon may: its faults
    /// fail then, and the process is stopping, CladefsdLost, as its device tells it (TASK-231).
    fn lost(&self) -> bool {
        false
    }
}

/// A test's window in this process's host memory resolves the faults on it itself.
#[cfg(all(test, target_os = "linux"))]
impl Faults for Window {
    fn size(&self) -> u64 {
        Window::size(self)
    }

    fn fault_page(&self, offset: u64) -> BoxFuture<'_, Result<()>> {
        Box::pin(Window::fault(self, offset, Access::Read))
    }

    fn probe(&self, offset: u64) -> BoxFuture<'_, Result<()>> {
        Box::pin(Window::fault(self, offset, Access::Read))
    }
}

/// cladefs: a change to a [`SecondStage`] begun ([`SecondStage::begin_change`]), which ends however
/// what makes it does.
struct Change<'a>(&'a dyn SecondStage);

impl<'a> Change<'a> {
    fn begin(stage: &'a dyn SecondStage) -> Self {
        stage.begin_change();
        Self(stage)
    }
}

impl Drop for Change<'_> {
    fn drop(&mut self) {
        self.0.end_change();
    }
}

/// What of a [`Source`]'s pages is in memory ([`Source::each_present`]): `count` pages from its
/// page `page`, consecutive in physical memory from `at`, of `slab`, which a window that maps them
/// in another process's second stage stages to it; or of memory never freed, which needs none
/// (zeros).
pub(crate) struct Present<'a> {
    pub page: u64,
    pub count: u64,
    pub at: PhysLocation,
    pub slab: Option<&'a PartSlab>,
}

/// What a window's mapping maps pages of (`crate::cache`): a sealed extent's pages, or zeros. Its
/// pages are numbered from its first, each a page of physical memory once it is in memory, which
/// the source keeps until it lets go of it: a page it evicts, it first unmaps from each window it
/// was told maps it ([`Window::unmap_pages_of`]).
///
/// Its lock is taken after a window's, never before: a window looks its pages up, and maps them,
/// under its own.
pub(crate) trait Source: Send + Sync {
    /// Calls `found` with each run of the pages of `pages` in memory, in order, under the source's
    /// lock, so that they stay in memory while `found` maps them.
    fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(Present<'_>));

    /// Brings page `page` into memory: completes once it is in, holding no thread meanwhile,
    /// returning whether it waited for it; or fails, the page not in memory.
    fn bring_in(self: Arc<Self>, page: u64) -> BoxFuture<'static, Result<bool>>;

    /// How many pages from `page` a fault on it maps, as far as they are in memory: those bringing
    /// it in brought in with it, which the guest reading on would fault on next.
    fn brought_in_with(&self, _page: u64) -> u64 {
        1
    }

    /// `window` maps it from now on, until [`Source::unmapped`]: a page it evicts, it unmaps there
    /// first. Told again, it is so once.
    fn mapped(&self, _window: &Arc<Window>) {}

    /// `window` maps it no more.
    fn unmapped(&self, _window: &Window) {}

    /// What its pages are: a sealed extent's, as a snapshot of a window's VM records its mappings
    /// ([`Dax::mappings`]); or zeros, cladefs: which a write's private page is made of without a
    /// copy, nor bringing anything in (backlog doc-13).
    fn origin(&self) -> Origin;
}

/// What a [`Source`]'s pages are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A sealed extent's, numbered from its first.
    Extent(ExtentId),
    /// Zeros.
    Zeros,
}

/// The identity of `source`, the same for each of its handles: what a window finds the mappings of
/// a source by.
pub(crate) fn source_identity<T: ?Sized>(source: &Arc<T>) -> usize {
    Arc::as_ptr(source) as *const () as usize
}

/// A DAX window of an extent-store device. See the module docs.
///
/// Handles to a window are cheap to clone, and may be used from any thread.
#[derive(Clone)]
pub struct DaxWindow(Arc<Window>);

impl DaxWindow {
    /// Resolves the guest's fault at `offset` into the window, for `access`: maps the page, in the
    /// window's second stage, or, without one, at its host address, so that the guest can retry
    /// the access; cladefs: a write's makes it private, in a window that copies on write.
    ///
    /// Completes once the page is in memory, holding no thread meanwhile. Fails if nothing is
    /// mapped there, with `ENOENT`, or with `EACCES` for a write the window refuses: to a window
    /// that doesn't copy on write, or past its private pages; the guest cannot go on.
    pub async fn fault(&self, offset: u64, access: Access) -> Result<()> {
        self.0.fault(offset, access).await
    }

    /// The memory in this process where the pages of a window without a second stage, as tests'
    /// are, are mapped.
    #[cfg(test)]
    pub(crate) fn host_base(&self) -> *mut u8 {
        let host_base = self.0.host_base().expect("a test's window has no second stage");
        host_base as *mut u8
    }

    /// The window's size in bytes.
    pub fn size(&self) -> u64 {
        self.0.size()
    }

    /// Whether anything is mapped at `offset`.
    pub fn is_mapped(&self, offset: u64) -> bool {
        self.0.is_mapped(offset)
    }

    /// cladefs: the sealed extent mapped at `offset`, and the offset in it of the page `offset` is
    /// in, if one is mapped there: what a trace of the guest's reads names a fault by (TASK-702.1).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn extent_at(&self, offset: u64) -> Option<(ExtentId, u64)> {
        let ps = page_size();
        let state = self.0.state.with(Mutex::lock);
        let (start, mapping) = state.mapping_at(offset)?;
        match mapping.source.origin() {
            Origin::Extent(extent) => Some((extent, (mapping.page + (offset - start) / ps) * ps)),
            Origin::Zeros => None,
        }
    }

    /// Whether it has pages mapped in its second stage, or in host memory, which reach them.
    pub fn maps_pages(&self) -> bool {
        !self.0.state.with(Mutex::lock).ptes.is_empty()
    }

    /// cladefs: its gauges, which follow its mappings as they change, and fall to nothing as it
    /// goes: one window's of what [`live`] counts of them all (TASK-111.5).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn gauges(&self) -> Arc<Gauges> {
        self.0.state.with(Mutex::lock).gauges.clone()
    }
}

impl std::fmt::Debug for DaxWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaxWindow")
            .field("host_base", &self.0.host_base().map(|base| base as *mut u8))
            .field("size", &self.size())
            .finish()
    }
}

/// Where a window's pages are mapped: in exactly one place.
enum Translation {
    /// In its second stage alone, another process's (a VMM's) or another range of this one's.
    Stage(Arc<dyn SecondStage>),
    /// In host memory: a reservation of host address space, at `base`, as long as the window.
    Host { base: usize },
}

/// A mapping of a window: a range of its source's pages, from `page`, up to the window's `end`,
/// read-only.
struct Mapping {
    end: u64,
    source: Arc<dyn Source>,
    page: u64,
}

/// A window's mappings and the pages it has mapped, under its lock.
struct WindowState {
    /// The mapped ranges of the window, by start. They do not overlap.
    mappings: BTreeMap<u64, Mapping>,
    /// The starts of the mappings, by the identity of their source ([`source_identity`]).
    by_source: HashMap<usize, BTreeSet<u64>>,
    /// The pages mapped, by offset, each within a mapping: where in physical memory its source's
    /// page is.
    ptes: BTreeMap<u64, PhysLocation>,
    /// cladefs: of a window whose stage copies on write, the runs of pages, by start and end, that
    /// the host's own changes took away from the stage keeping its private pages there
    /// ([`Private::Keep`]): where the stage may have a private page the window has mapped nothing
    /// at, which the guest's next change there ends (doc-26). Elsewhere, a private page is where
    /// the window has a page mapped.
    kept: BTreeMap<u64, u64>,
    /// cladefs: what those are, counted, for whoever reads them without the lock: the daemon's
    /// gauges (TASK-131.6, TASK-111.5).
    gauges: Arc<Gauges>,
}

/// cladefs: what a change to a window's range does to the private pages in it, its second stage's
/// that copies on write (backlog doc-13, doc-26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Private {
    /// They go, as the guest's `MAP` or `UNMAP` over them ends them.
    End,
    /// They stay, as the host's own changes leave them: an eviction, a free's zeros.
    Keep,
}

/// A window's mappings, their bytes, and the pages it has mapped, as its state changes: the extent
/// daemon's gauges (`crate::cladefsd::metrics`, TASK-131.6), read without the window's lock: one
/// window's, which the daemon reports of each VM's (TASK-111.5), and which add up to all windows'
/// ([`live`]). Nothing once the window goes.
#[derive(Default)]
pub(crate) struct Gauges {
    mappings: AtomicU64,
    mapped_bytes: AtomicU64,
    pages: AtomicU64,
}

impl Gauges {
    /// The window's mappings, as of now.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn live(&self) -> Live {
        Live {
            mappings: self.mappings.load(Ordering::Relaxed),
            mapped_bytes: self.mapped_bytes.load(Ordering::Relaxed),
            pages: self.pages.load(Ordering::Relaxed),
        }
    }
}

/// cladefs: the process's windows' gauges, each kept as long as its window, or whoever holds it.
/// The standard library's lock, as Shuttle's isn't made in a static, and nothing waits on it.
static WINDOWS: std::sync::Mutex<Vec<std::sync::Weak<Gauges>>> = std::sync::Mutex::new(Vec::new());

/// Windows' mappings, as of now: the process's, all windows' ([`live`]), or one window's
/// ([`Gauges::live`]).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Live {
    /// Their mapped ranges.
    pub mappings: u64,
    /// The bytes those ranges span.
    pub mapped_bytes: u64,
    /// The pages they have mapped in their second stages: of those bytes, those the guest reaches
    /// without the window, a fault on each, if any, resolved by the stage alone, which may install
    /// a page as the guest first touches it (`dax::uffd`, TASK-276.6).
    pub pages: u64,
}

/// The process's windows' mappings, as of now: every window's gauges, added up.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn live() -> Live {
    let mut windows = WINDOWS.lock().unwrap();
    windows.retain(|gauges| gauges.strong_count() > 0);
    (windows.iter().filter_map(std::sync::Weak::upgrade)).fold(Live::default(), |all, gauges| {
        let window = gauges.live();
        Live {
            mappings: all.mappings + window.mappings,
            mapped_bytes: all.mapped_bytes + window.mapped_bytes,
            pages: all.pages + window.pages,
        }
    })
}

impl Drop for WindowState {
    /// Its gauges fall to nothing, which whoever holds them reads from now on.
    fn drop(&mut self) {
        let gauges = &self.gauges;
        for gauge in [&gauges.mappings, &gauges.mapped_bytes, &gauges.pages] {
            gauge.store(0, Ordering::Relaxed);
        }
    }
}

impl WindowState {
    /// A window's state, mapping nothing, its gauges among the process's windows' ([`live`]).
    fn new() -> Self {
        let gauges = Arc::new(Gauges::default());
        WINDOWS.lock().unwrap().push(Arc::downgrade(&gauges));
        Self {
            mappings: BTreeMap::new(),
            by_source: HashMap::new(),
            ptes: BTreeMap::new(),
            kept: BTreeMap::new(),
            gauges,
        }
    }

    /// Records that the window has mapped the page at `at` at `offset`, where it had mapped nothing.
    fn insert_pte(&mut self, offset: u64, at: PhysLocation) {
        let previous = self.ptes.insert(offset, at);
        debug_assert!(previous.is_none(), "a page mapped over another at {offset:#x}");
        self.gauges.pages.fetch_add(1, Ordering::Relaxed);
    }

    /// cladefs: records that the pages at `offsets`, in order, were taken away keeping the private
    /// pages there ([`WindowState::kept`]), in runs, joined with those around them.
    fn keep(&mut self, offsets: &[u64]) {
        let ps = page_size();
        for &offset in offsets {
            let (mut start, mut end) = (offset, offset + ps);
            if let Some((&before, &before_end)) = self.kept.range(..=start).next_back()
                && before_end >= start
            {
                start = before;
                end = end.max(before_end);
            }
            while let Some((&after, &after_end)) = self.kept.range(start..).next()
                && after <= end
            {
                end = end.max(after_end);
                self.kept.remove(&after);
            }
            self.kept.insert(start, end);
        }
    }

    /// cladefs: the runs of [`WindowState::kept`] within `start..end`, forgotten: what the guest's
    /// change of `start..end` ends.
    /// The runs don't overlap: those within it are the last to start before it, if it ends past
    /// its start, and those starting in it, looked up so, whatever else is kept (no scan of what
    /// is kept before it, a lookup each).
    fn take_kept(&mut self, start: u64, end: u64) -> Vec<Range<u64>> {
        let before = (self.kept.range(..start).next_back())
            .filter(|(_, run_end)| **run_end > start)
            .map(|(run, run_end)| (*run, *run_end));
        let within = self.kept.range(start..end).map(|(run, run_end)| (*run, *run_end));
        let overlapping: Vec<(u64, u64)> = before.into_iter().chain(within).collect();
        let mut taken = Vec::new();
        for (run, run_end) in overlapping {
            self.kept.remove(&run);
            if run < start {
                self.kept.insert(run, start);
            }
            if run_end > end {
                self.kept.insert(end, run_end);
            }
            taken.push(run.max(start)..run_end.min(end));
        }
        taken
    }

    /// Records that the window has unmapped the page at `offset`, which it had mapped.
    fn remove_pte(&mut self, offset: u64) {
        if self.ptes.remove(&offset).is_some() {
            self.gauges.pages.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// The mapping `offset` is in, and its start.
    fn mapping_at(&self, offset: u64) -> Option<(u64, &Mapping)> {
        let (start, mapping) = self.mappings.range(..=offset).next_back()?;
        (mapping.end > offset).then_some((*start, mapping))
    }

    /// The starts of the mappings overlapping `start..end`: as they do not overlap each other,
    /// the last to start before it, and those starting in it.
    fn overlapping(&self, start: u64, end: u64) -> Vec<u64> {
        let before = self.mappings.range(..start).next_back();
        before
            .filter(|(_, mapping)| mapping.end > start)
            .map(|(start, _)| *start)
            .into_iter()
            .chain(self.mappings.range(start..end).map(|(start, _)| *start))
            .collect()
    }

    fn insert(&mut self, start: u64, mapping: Mapping) {
        let identity = source_identity(&mapping.source);
        self.by_source.entry(identity).or_default().insert(start);
        self.gauges.mappings.fetch_add(1, Ordering::Relaxed);
        self.gauges.mapped_bytes.fetch_add(mapping.end - start, Ordering::Relaxed);
        let previous = self.mappings.insert(start, mapping);
        debug_assert!(previous.is_none(), "a mapping over another at {start:#x}");
    }

    fn remove(&mut self, start: u64) -> Mapping {
        let mapping = self.mappings.remove(&start).expect("a mapping there");
        self.gauges.mappings.fetch_sub(1, Ordering::Relaxed);
        self.gauges.mapped_bytes.fetch_sub(mapping.end - start, Ordering::Relaxed);
        let identity = source_identity(&mapping.source);
        let starts = self.by_source.get_mut(&identity).expect("its source's mappings");
        starts.remove(&start);
        if starts.is_empty() {
            self.by_source.remove(&identity);
        }
        mapping
    }

    /// Forgets the mappings of `start..end`, keeping what of them is around it, and returns the
    /// sources of those it forgot.
    fn forget(&mut self, start: u64, end: u64) -> Vec<Arc<dyn Source>> {
        let ps = page_size();
        let mut forgotten = Vec::new();
        for mapping_start in self.overlapping(start, end) {
            let mapping = self.remove(mapping_start);
            if mapping_start < start {
                let before = Mapping { end: start, source: mapping.source.clone(), ..mapping };
                self.insert(mapping_start, before);
            }
            if mapping.end > end {
                let page = mapping.page + (end - mapping_start) / ps;
                let after = Mapping { page, source: mapping.source.clone(), ..mapping };
                self.insert(end, after);
            }
            forgotten.push(mapping.source);
        }
        forgotten
    }
}

/// A window. See the module docs.
pub(crate) struct Window {
    size: u64,
    translation: Translation,
    /// cladefs: shared with its connection's [`DaxWindow`] and the guest's faults, beyond what
    /// halts its cell: a panic holding it aborts the daemon (`local_cell::shared`, TASK-149.24).
    state: SharedLock<Mutex<WindowState>>,
    /// cladefs: whether its second stage copies on write (backlog doc-13, doc-26), as a writer's
    /// does: the guest's `MAP` and `UNMAP` then end the private pages over the whole range they
    /// change ([`Private::End`]), which the window can't tell from the rest.
    copying: bool,
}

impl Window {
    /// A window of `size` bytes, which must be a multiple of the page size, whose pages are mapped
    /// in `second_stage`, or, without one, in this process's memory; whose stage copies on write
    /// if `copying`, which a window in this process's memory has none to.
    fn new(size: u64, second_stage: Option<Arc<dyn SecondStage>>, copying: bool) -> Result<Self> {
        if size == 0 || !size.is_multiple_of(page_size()) {
            return Err(Errno::EINVAL);
        }
        let len = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
        if copying && second_stage.is_none() {
            return Err(Errno::EINVAL);
        }
        let translation = match second_stage {
            Some(stage) => Translation::Stage(stage),
            None => Translation::Host { base: reserve(None, len).map_err(|_| Errno::ENOMEM)? },
        };
        let state = SharedLock::new("window", Mutex::new(WindowState::new()));
        Ok(Self { size, translation, state, copying })
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn stage(&self) -> Option<&Arc<dyn SecondStage>> {
        match &self.translation {
            Translation::Stage(stage) => Some(stage),
            Translation::Host { .. } => None,
        }
    }

    /// cladefs: begins a change of its second stage, if it has one, which ends as what it returns
    /// is dropped: begun with the window's lock held, and dropped before it is let go of
    /// ([`SecondStage::begin_change`]).
    fn change(&self) -> Option<Change<'_>> {
        self.stage().map(|stage| Change::begin(stage.as_ref()))
    }

    fn host_base(&self) -> Option<usize> {
        match self.translation {
            Translation::Host { base } => Some(base),
            Translation::Stage(_) => None,
        }
    }

    fn is_mapped(&self, offset: u64) -> bool {
        self.state.with(Mutex::lock).mapping_at(offset).is_some()
    }

    /// Resolves the guest's fault at `offset` for `access`: a load's maps the page
    /// ([`Window::resolve`]); a write's is refused, as every page is mapped read-only: cladefs: a
    /// stage that copies on write makes its own private copies (doc-26), and asks the window only
    /// to read.
    pub(crate) async fn fault(&self, offset: u64, access: Access) -> Result<()> {
        use crate::stats::{self, Counter};
        let page = offset - offset % page_size();
        let start = std::time::Instant::now();
        let faulted = match access {
            Access::Read => self.resolve(page).await,
            Access::Write => match self.is_mapped(page) {
                true => Err(Errno::EACCES),
                false => Err(Errno::ENOENT),
            },
        };
        stats::count_since(Counter::Faults, Counter::FaultNs, start);
        if faulted == Ok(true) {
            stats::add(Counter::FaultWaits, 1);
        }
        faulted.map(|_| ())
    }

    /// Maps the page at `offset`, which a load's fault was on, bringing it into memory first, with
    /// those its source brought in with it (`Source::brought_in_with`), and returns whether it
    /// waited for the page to be brought in. A page evicted, or mapped anew, meanwhile is resolved
    /// as it is then.
    async fn resolve(&self, offset: u64) -> Result<bool> {
        let ps = page_size();
        let mut waited = false;
        loop {
            let (source, page, mapped) = {
                let state = self.state.with(Mutex::lock);
                let (start, mapping) = state.mapping_at(offset).ok_or(Errno::ENOENT)?;
                if state.ptes.contains_key(&offset) {
                    return Ok(waited);
                }
                let page = mapping.page + (offset - start) / ps;
                let left = (mapping.end - offset) / ps;
                (mapping.source.clone(), page, left.min(mapping.source.brought_in_with(page)))
            };
            waited |= source.clone().bring_in(page).await?;
            let mut state = self.state.with(Mutex::lock);
            let same = state.mapping_at(offset).is_some_and(|(start, mapping)| {
                source_identity(&mapping.source) == source_identity(&source)
                    && mapping.page + (offset - start) / ps == page
            });
            if same {
                self.install(&mut state, offset, offset + mapped * ps);
                if state.ptes.contains_key(&offset) {
                    return Ok(waited);
                }
            }
        }
    }

    /// Maps as [`Window::map_as`] does, as the guest's `MAP`, which ends the private pages there.
    #[cfg(test)]
    fn map(self: &Arc<Self>, offset: u64, source: &Arc<dyn Source>, source_offset: u64, len: u64) {
        self.map_as(offset, source, source_offset, len, Private::End);
    }

    /// Maps `len` bytes of `source`'s pages from `source_offset` into the window at `offset`,
    /// read-only, replacing what was mapped there, and then its pages in memory: one change to the
    /// second stage, if there is one (`SecondStage::begin_change`). cladefs: the private pages
    /// there go or stay as `private` says: the guest's `MAP` ends them (`Private::End`), the
    /// host's own changes leave them, a free's zeros.
    fn map_as(
        self: &Arc<Self>,
        offset: u64,
        source: &Arc<dyn Source>,
        source_offset: u64,
        len: u64,
        private: Private,
    ) {
        let mut state = self.state.with(Mutex::lock);
        let _change = self.change();
        self.map_locked(&mut state, offset, source, source_offset / page_size(), len, private);
    }

    /// Maps as [`Window::map_as`] says, from `source`'s page `page`, with the window's lock held
    /// and its stage's change begun.
    fn map_locked(
        self: &Arc<Self>,
        state: &mut WindowState,
        offset: u64,
        source: &Arc<dyn Source>,
        page: u64,
        len: u64,
        private: Private,
    ) {
        let end = offset + len;
        let forgotten = self.place(state, offset, source, page, len, private);
        if let Some(stage) = self.stage() {
            stage.window_mapped(offset, len);
        }
        // Its pages in memory are mapped now, so that the guest's first accesses to them do not
        // fault. cladefs: but where a private page stays.
        self.install(state, offset, end);
        self.unmapped(state, forgotten);
    }

    /// cladefs: maps each of `pieces`, in order, as [`Window::map_as`] maps the guest's `MAP`, in
    /// one change of the second stage: the ranges mapped anew told it in runs of them consecutive
    /// in the window, then their pages in memory mapped, in runs across the pieces, where a `MAP`
    /// each made a change of its own, of a run at least (TASK-92.14). What a snapshot's window
    /// held is mapped again so, as its VM is restored.
    fn map_many(self: &Arc<Self>, pieces: &[Piece<'_>]) {
        let mut state = self.state.with(Mutex::lock);
        let _change = self.change();
        let mut anew: Vec<Range<u64>> = Vec::new();
        for &Piece { offset, source, page, len } in pieces {
            let forgotten = self.place(&mut state, offset, source, page, len, Private::End);
            self.unmapped(&state, forgotten);
            match anew.last_mut() {
                Some(run) if run.end == offset => run.end += len,
                _ => anew.push(offset..offset + len),
            }
        }
        if let Some(stage) = self.stage() {
            for run in &anew {
                stage.window_mapped(run.start, run.end - run.start);
            }
        }
        let mut pages = Vec::new();
        for run in &anew {
            self.present(&mut state, run.start, run.end, &mut pages);
        }
        self.map_pages(&pages);
    }

    /// cladefs: puts the mapping of `len` bytes of `source`'s pages from `page` at `offset` in the
    /// window's map, in place of what was mapped there and the pages it had there, the private
    /// pages among them going or staying as `private` says; and returns the sources of the
    /// mappings it replaced, to tell of ([`Window::unmapped`]). The stage is told of nothing it
    /// maps anew, nor are its pages mapped, which is for its caller to do.
    fn place(
        self: &Arc<Self>,
        state: &mut WindowState,
        offset: u64,
        source: &Arc<dyn Source>,
        page: u64,
        len: u64,
        private: Private,
    ) -> Vec<Arc<dyn Source>> {
        let end = offset + len;
        // Whether the window mapped it before, whatever of it this replaces: the guest's MAP of
        // an extent over its own range tells it nothing (TASK-602).
        let first = !state.by_source.contains_key(&source_identity(source));
        self.unmap_ptes(state, offset, end, private);
        let forgotten = state.forget(offset, end);
        state.insert(offset, Mapping { end, source: source.clone(), page });
        if first {
            source.mapped(self);
        }
        forgotten
    }

    /// Maps zeros, `zeros`, read-only, at the window's ranges that map `source`, by its identity,
    /// which a freed extent's pages are, and returns those ranges, in order, as they were: one
    /// change to the second stage. The guest may still touch such a range, as a write racing the
    /// move of what it maps, which faults as on any read-only page; unmapped, its fault would find
    /// nothing to map. cladefs: the private pages there stay, the guest's, which only its own `MAP`
    /// or `UNMAP` ends (backlog doc-13).
    fn map_zeros_over(self: &Arc<Self>, source: usize, zeros: &Arc<dyn Source>) -> Vec<Range<u64>> {
        let mut state = self.state.with(Mutex::lock);
        let _change = self.change();
        let Some(starts) = state.by_source.get(&source) else { return Vec::new() };
        let ranges: Vec<Range<u64>> =
            starts.iter().map(|start| *start..state.mappings[start].end).collect();
        for range in &ranges {
            let len = range.end - range.start;
            if rules().free_maps_zeros {
                self.map_locked(&mut state, range.start, zeros, 0, len, Private::Keep);
            } else {
                self.unmap_locked(&mut state, range.start, len, Private::Keep);
            }
        }
        ranges
    }

    /// The window's mappings, in order, as a snapshot of its VM records them, to map them again
    /// where it is restored ([`crate::OpenCell::window_mappings`]).
    fn mappings(&self) -> Vec<WindowMapping> {
        let state = self.state.with(Mutex::lock);
        (state.mappings.iter())
            .map(|(start, mapping)| WindowMapping {
                window_offset: *start,
                len: mapping.end - start,
                contents: match mapping.source.origin() {
                    Origin::Extent(extent) => {
                        MappingContents::Extent { extent, offset: mapping.page * page_size() }
                    }
                    Origin::Zeros => MappingContents::Zeros,
                },
            })
            .collect()
    }

    /// Unmaps `len` bytes of the window at `offset`: cladefs: the guest's `UNMAP`, which ends the
    /// private pages there.
    fn unmap(&self, offset: u64, len: u64) {
        let mut state = self.state.with(Mutex::lock);
        self.unmap_locked(&mut state, offset, len, Private::End);
    }

    /// cladefs: unmaps as [`Window::unmap`] does, with the window's lock held, its private pages
    /// there going or staying as `private` says.
    fn unmap_locked(&self, state: &mut WindowState, offset: u64, len: u64, private: Private) {
        let end = offset + len;
        self.unmap_ptes(state, offset, end, private);
        let forgotten = state.forget(offset, end);
        self.unmapped(state, forgotten);
    }

    /// Tells the sources `forgotten` that no longer have a mapping in the window so.
    fn unmapped(&self, state: &WindowState, forgotten: Vec<Arc<dyn Source>>) {
        let mut told = BTreeSet::new();
        for source in forgotten {
            let identity = source_identity(&source);
            if !state.by_source.contains_key(&identity) && told.insert(identity) {
                source.unmapped(self);
            }
        }
    }

    /// Maps from `to` what it maps from `from`, the same pages of the same extent: `from`'s
    /// pages it has mapped unmapped, `to`'s mapped as they are faulted on, its private pages
    /// staying. cladefs: a tenant's copy of an extent moved to the default arena, which every VM
    /// process may read, so that the window faults on nothing of the copy, which goes (TASK-234).
    pub(crate) fn rebind(self: &Arc<Self>, from: &Arc<dyn Source>, to: Arc<dyn Source>) {
        let mut state = self.state.with(Mutex::lock);
        let Some(starts) = state.by_source.remove(&source_identity(from)) else { return };
        let mut gone = Vec::new();
        for start in &starts {
            let end = state.mappings[start].end;
            gone.extend(state.ptes.range(*start..end).map(|(offset, _)| *offset));
            state.mappings.get_mut(start).expect("a mapping's start").source = to.clone();
        }
        for offset in &gone {
            state.remove_pte(*offset);
        }
        self.kept(&mut state, &gone);
        state.by_source.entry(source_identity(&to)).or_default().extend(starts);
        drop(state);
        from.unmapped(self);
        to.mapped(self);
    }

    /// Unmaps the pages the window has mapped of `source`, by its identity, that `evicted` says
    /// are of the runs it evicts, by where they are: before it lets go of them. cladefs: a private
    /// page within its mappings, its stage's, stays (backlog doc-13, spec rule
    /// evictionKeepsPrivate).
    pub(crate) fn unmap_pages_of(&self, source: usize, evicted: &dyn Fn(PhysLocation) -> bool) {
        let mut state = self.state.with(Mutex::lock);
        let Some(starts) = state.by_source.get(&source) else { return };
        let ranges: Vec<(u64, u64)> =
            starts.iter().map(|start| (*start, state.mappings[start].end)).collect();
        let mut gone = Vec::new();
        for (start, end) in ranges {
            gone.extend(
                (state.ptes.range(start..end))
                    .filter(|(_, at)| evicted(**at))
                    .map(|(offset, _)| *offset),
            );
        }
        for offset in &gone {
            state.remove_pte(*offset);
        }
        self.kept(&mut state, &gone);
    }

    /// cladefs: unmaps the pages at `offsets`, in order, which the window took away keeping the
    /// private pages there ([`Private::Keep`]), and, if its stage copies, records them so
    /// ([`WindowState::kept`]).
    fn kept(&self, state: &mut WindowState, offsets: &[u64]) {
        if self.copying {
            state.keep(offsets);
        }
        self.unmap_runs(offsets, Private::Keep);
    }

    /// Maps, where nothing is mapped, the pages in memory of the mappings of `start..end`: in runs
    /// of pages consecutive in the window and in an arena's object, each one change of the second
    /// stage.
    fn install(&self, state: &mut WindowState, start: u64, end: u64) {
        let mut pages = Vec::new();
        self.present(state, start, end, &mut pages);
        self.map_pages(&pages);
    }

    /// Adds to `pages`, in order, the pages in memory of the mappings of `start..end` where
    /// nothing is mapped, each its offset in the window and where it is, which it takes as mapped
    /// from now on: those [`Window::map_pages`] is to map.
    fn present(
        &self,
        state: &mut WindowState,
        start: u64,
        end: u64,
        pages: &mut Vec<(u64, PhysLocation)>,
    ) {
        let ps = page_size();
        let vm = self.stage().and_then(|stage| stage.vm_process());
        for mapping_start in state.overlapping(start, end) {
            let mapping = &state.mappings[&mapping_start];
            let (from, to) = (start.max(mapping_start), end.min(mapping.end));
            let first = mapping.page + (from - mapping_start) / ps;
            let ptes = &state.ptes;
            let mut found = Vec::new();
            mapping.source.each_present(first..first + (to - from) / ps, &mut |present| {
                let mut staged = false;
                for page in present.page..present.page + present.count {
                    let offset = from + (page - first) * ps;
                    if ptes.contains_key(&offset) {
                        continue;
                    }
                    let within = (page - present.page) * ps;
                    found.push((
                        offset,
                        PhysLocation { offset: present.at.offset + within, ..present.at },
                    ));
                    staged = true;
                }
                // Once freed, it waits for the VM process to take it away
                // (`slabs::quarantine_with`, TASK-79.9, doc-26).
                if staged && let (Some(vm), Some(slab)) = (vm, present.slab) {
                    slab.stage_to(vm);
                }
            });
            for (offset, at) in found {
                state.insert_pte(offset, at);
                pages.push((offset, at));
            }
        }
    }

    /// Maps `pages`, as [`Window::present`] found them, in order: in runs of pages consecutive in
    /// the window and in an arena's object, each one change of the second stage.
    fn map_pages(&self, pages: &[(u64, PhysLocation)]) {
        let ps = page_size();
        // cladefs: the runs of pages consecutive in the window, which the maps below are made of
        // (`stage_segments`, TASK-131.1).
        let segments = 1 + pages.windows(2).filter(|pair| pair[0].0 + ps != pair[1].0).count();
        if self.stage().is_some() && !pages.is_empty() {
            crate::stats::add(crate::stats::Counter::StageSegments, segments as u64);
        }
        let mut run: Option<(u64, PhysLocation, u64)> = None;
        for &(offset, location) in pages {
            match &mut run {
                Some((at, first, len))
                    if *at + *len == offset
                        && first.arena == location.arena
                        && first.offset + *len == location.offset =>
                {
                    *len += ps;
                }
                _ => {
                    if let Some((at, first, len)) = run.replace((offset, location, ps)) {
                        self.map_run(at, first, len);
                    }
                }
            }
        }
        if let Some((at, first, len)) = run {
            self.map_run(at, first, len);
        }
    }

    /// Unmaps the pages the window has mapped of `start..end`. cladefs: its stage's private pages
    /// there go or stay as `private` says (doc-26). A private page is at a page the window mapped,
    /// or one the host's changes took away keeping it ([`WindowState::kept`]): the guest's changes
    /// ([`Private::End`]) unmap those too, which the window maps nothing at.
    fn unmap_ptes(&self, state: &mut WindowState, start: u64, end: u64, private: Private) {
        let gone: Vec<u64> = state.ptes.range(start..end).map(|(offset, _)| *offset).collect();
        for offset in &gone {
            state.remove_pte(*offset);
        }
        match private {
            Private::Keep => self.kept(state, &gone),
            Private::End if self.copying && rules().guests_changes_end_private => {
                let ps = page_size();
                let mut runs: Vec<Range<u64>> = state.take_kept(start, end);
                runs.extend(gone.iter().map(|offset| *offset..offset + ps));
                runs.sort_by_key(|run| run.start);
                let mut joined: Vec<Range<u64>> = Vec::new();
                for run in runs {
                    match joined.last_mut() {
                        Some(last) if last.end >= run.start => last.end = last.end.max(run.end),
                        _ => joined.push(run),
                    }
                }
                for run in joined {
                    self.unmap_range(run.start, run.end - run.start, private);
                }
            }
            Private::End => self.unmap_runs(&gone, private),
        }
    }

    /// Unmaps the pages at `offsets`, in order, in runs of consecutive pages, the private pages
    /// among them going or staying as `private` says.
    fn unmap_runs(&self, offsets: &[u64], private: Private) {
        let ps = page_size();
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for &offset in offsets {
            match runs.last_mut() {
                Some((start, len)) if *start + *len == offset => *len += ps,
                _ => runs.push((offset, ps)),
            }
        }
        for (start, len) in runs {
            self.unmap_range(start, len, private);
        }
    }

    /// Unmaps the `len` bytes at `start`, the private pages there going or staying as `private`
    /// says.
    fn unmap_range(&self, start: u64, len: u64, private: Private) {
        match &self.translation {
            Translation::Stage(stage) => stage.unmap(start, len, private),
            Translation::Host { base } => {
                let at = base + start as usize;
                reserve(Some(at), len as usize).expect("the window's memory reserved again");
            }
        }
    }

    /// Maps the `len` bytes at `offset` to the pages at `location`, read-only.
    fn map_run(&self, offset: u64, location: PhysLocation, len: u64) {
        match &self.translation {
            Translation::Stage(stage) => stage.map(offset, location, len),
            Translation::Host { base } => {
                let fd = crate::pages::arena_memory(location.arena).expect("the page's arena");
                let prot = libc::PROT_READ;
                let at = (base + offset as usize) as *mut libc::c_void;
                // SAFETY: Maps pages of physical memory, shared, over the window's reservation,
                // which only the guest, a test, accesses.
                let addr = unsafe {
                    use std::os::fd::AsRawFd as _;
                    let (flags, fd) = (libc::MAP_SHARED | libc::MAP_FIXED, fd.as_raw_fd());
                    libc::mmap(at, len as usize, prot, flags, fd, location.offset as libc::off_t)
                };
                assert_eq!(addr, at, "map pages: {}", std::io::Error::last_os_error());
            }
        }
    }

    /// Checks that `len` bytes at `offset` are page aligned and within the window.
    fn check_range(&self, offset: u64, len: u64) -> Result<()> {
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if len == 0
            || !offset.is_multiple_of(page_size())
            || !len.is_multiple_of(page_size())
            || end > self.size()
        {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Translation::Host { base } = self.translation {
            // SAFETY: The reservation is the window's own.
            let status = unsafe { libc::munmap(base as *mut libc::c_void, self.size as usize) };
            debug_assert_eq!(status, 0, "unreserve a window: {}", std::io::Error::last_os_error());
        }
    }
}

/// Reserves `len` bytes of host address space, at `at` if given, over what is there, inaccessible
/// until pages are mapped there.
fn reserve(at: Option<usize>, len: usize) -> std::io::Result<usize> {
    let flags = libc::MAP_PRIVATE
        | libc::MAP_ANON
        | libc::MAP_NORESERVE
        | if at.is_some() { libc::MAP_FIXED } else { 0 };
    let hint = at.unwrap_or(0) as *mut libc::c_void;
    // SAFETY: Either an address hint, or replacing part of a window's own reservation.
    let base = unsafe { libc::mmap(hint, len, libc::PROT_NONE, flags, -1, 0) };
    if base == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(base as usize)
}

/// The rules of how a window changes its mappings, each of which its Shuttle tests show to be
/// needed by turning it off (TASK-44.3).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rules {
    /// A freed extent's mappings map zeros rather than nothing: a write racing the free faults on
    /// them as on any read-only page.
    pub free_maps_zeros: bool,
    /// cladefs: the guest's `MAP` or `UNMAP` of a window that copies unmaps in its stage, ending the
    /// private pages there, the pages the host's changes took away keeping them too.
    pub guests_changes_end_private: bool,
}

impl Rules {
    pub const ALL: Self = Self { free_maps_zeros: true, guests_changes_end_private: true };
}

#[cfg(not(feature = "shuttle"))]
fn rules() -> Rules {
    Rules::ALL
}

#[cfg(feature = "shuttle")]
static RULES: std::sync::Mutex<Rules> = std::sync::Mutex::new(Rules::ALL);

#[cfg(feature = "shuttle")]
fn rules() -> Rules {
    *RULES.lock().unwrap()
}

/// A DAX window, which its cell maps extents into: its mappings are the window's own, one map
/// of them (TASK-160.11).
pub(crate) struct Dax {
    window: Arc<Window>,
    /// Zeros, which the window maps where nothing else is to be read ([`Dax::map_zeros`]): had
    /// before it maps anything, so that mapping them takes no memory.
    zeros: Arc<dyn Source>,
    /// The thread serving the faults on the host memory a test's window was created in
    /// ([`Dax::in_host_memory`]), which runs until the window is dropped.
    #[cfg(all(test, target_os = "linux"))]
    _faults: Option<uffd::FaultThread>,
}

impl Dax {
    /// A window of `size` bytes whose pages are mapped in `second_stage`, or, without one, in this
    /// process's memory, and a handle to it.
    pub fn new(size: u64, second_stage: Option<Arc<dyn SecondStage>>) -> Result<(Self, DaxWindow)> {
        Self::with_stage(size, second_stage, false)
    }

    /// cladefs: as [`Dax::new`], a window whose `second_stage` copies on write (backlog doc-13,
    /// doc-26): a writer's.
    pub fn copying(size: u64, second_stage: Arc<dyn SecondStage>) -> Result<(Self, DaxWindow)> {
        Self::with_stage(size, Some(second_stage), true)
    }

    fn with_stage(
        size: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
        copying: bool,
    ) -> Result<(Self, DaxWindow)> {
        let window = Arc::new(Window::new(size, second_stage, copying)?);
        let dax = Self {
            window: window.clone(),
            zeros: crate::cache::zeros()?,
            #[cfg(all(test, target_os = "linux"))]
            _faults: None,
        };
        Ok((dax, DaxWindow(window)))
    }

    /// A window whose second stage is the host memory `stage` takes over, and a handle to it. A
    /// thread of its own serves the faults on the memory, in this process, as tests have it: a
    /// VM's window is the extent daemon's, whose VM process serves them (`crate::cladefsd`).
    #[cfg(all(test, target_os = "linux"))]
    pub fn in_host_memory(
        stage: Arc<uffd::UffdSecondStage>,
    ) -> crate::error::Result<(Self, DaxWindow), crate::Error> {
        Self::in_host_memory_copying(stage, None)
    }

    /// cladefs: as [`Dax::in_host_memory`], a window whose stage copies on write, holding `most`
    /// private pages at most, if given (doc-26).
    #[cfg(all(test, target_os = "linux"))]
    pub fn in_host_memory_copying(
        stage: Arc<uffd::UffdSecondStage>,
        most: Option<u64>,
    ) -> crate::error::Result<(Self, DaxWindow), crate::Error> {
        if let Some(most) = most {
            stage.copy_on_write(most)?;
        }
        let copying = most.is_some();
        let (mut dax, window) = Self::with_stage(stage.size(), Some(stage.clone()), copying)?;
        dax._faults = Some(stage.serve(dax.window.clone())?);
        Ok((dax, window))
    }

    /// Checks the window's range for a mapping of `len` bytes at `offset`.
    pub fn check_range(&self, offset: u64, len: u64) -> Result<()> {
        self.window.check_range(offset, len)
    }

    /// Maps `len` bytes of `source` at `source_offset` into the window at `offset`, read-only:
    /// the guest's `MAP`, which ends the private pages there.
    pub fn map(&mut self, offset: u64, source: &Arc<dyn Source>, source_offset: u64, len: u64) {
        self.window.map_as(offset, source, source_offset, len, Private::End);
    }

    /// Maps zeros, read-only, at `len` bytes of the window at `offset`, as [`Dax::map`] does.
    pub fn map_zeros(&mut self, offset: u64, len: u64) {
        self.window.map_as(offset, &self.zeros, 0, len, Private::End);
    }

    /// cladefs: maps each of `mappings`, in order, as [`Dax::map`] would, or zeros, as
    /// [`Dax::map_zeros`] would, where it names no source, in one change of the second stage
    /// (TASK-92.14).
    pub fn map_many(&mut self, mappings: &[DaxMapping]) {
        let pieces: Vec<Piece<'_>> = (mappings.iter())
            .map(|DaxMapping { offset, len, source }| {
                let (source, page) = match source {
                    Some((source, from)) => (source, from / page_size()),
                    None => (&self.zeros, 0),
                };
                Piece { offset: *offset, len: *len, source, page }
            })
            .collect();
        self.window.map_many(&pieces);
    }

    /// Unmaps `len` bytes of the window at `offset`: the guest's `UNMAP`.
    pub fn unmap(&mut self, offset: u64, len: u64) {
        self.window.unmap(offset, len);
    }

    /// Maps zeros where the window maps `source`, a freed extent's pages, whose memory goes with
    /// those mappings, and returns where, as [`Window::map_zeros_over`] does.
    pub fn map_zeros_over<T: ?Sized>(&mut self, source: &Arc<T>) -> Vec<Range<u64>> {
        self.window.map_zeros_over(source_identity(source), &self.zeros)
    }

    /// The window's mappings, in order, as a snapshot of its VM records them.
    pub fn mappings(&self) -> Vec<WindowMapping> {
        self.window.mappings()
    }
}

/// cladefs: the latest of a window's MAPs and UNMAPs, as [`WindowChanges::MOST`] of them: which
/// mapped a page as it was refused, and what mapped it before (TASK-44.12).
#[derive(Default)]
pub(crate) struct WindowChanges {
    changes: std::collections::VecDeque<WindowChange>,
    /// cladefs: the pages whose accesses the window refused, until a change of the window covers
    /// them, which is logged then, with what it did (TASK-44.12).
    refused: Vec<Refused>,
}

/// A page whose access the window refused ([`WindowChanges::refused`]): when, and what the window
/// mapped there then.
struct Refused {
    at: std::time::Instant,
    window_offset: u64,
    what: String,
    mapped: String,
}

/// One of a window's changes: `len` bytes at `window_offset`, mapped or unmapped, when.
struct WindowChange {
    at: std::time::Instant,
    window_offset: u64,
    len: u64,
    change: WindowChangeKind,
}

/// What a [`WindowChange`] did.
pub(crate) enum WindowChangeKind {
    /// A MAP of `extent`'s bytes from `offset`, and how it ended.
    Map {
        extent: ExtentId,
        offset: u64,
        result: std::result::Result<(), Errno>,
    },
    /// A MAP of zeros.
    Zeros,
    /// Zeros mapped in place of what a freed extent's memory, which they kept, mapped.
    Freed,
    Unmap,
}

impl WindowChangeKind {
    /// What the change did, said for a log.
    fn describe(&self) -> String {
        match self {
            WindowChangeKind::Map { extent, offset, result } => {
                format!("MAP of {extent:?} from {offset:#x}, {result:?}")
            }
            WindowChangeKind::Zeros => "MAP of zeros".into(),
            WindowChangeKind::Freed => "zeros in place of a freed extent's".into(),
            WindowChangeKind::Unmap => "UNMAP".into(),
        }
    }
}

impl WindowChanges {
    /// Recent changes kept for DAX refusal diagnostics (board 10549).
    const MOST: usize = 256;

    /// The most refused pages awaiting a change ([`WindowChanges::refused`]): the oldest go first.
    const MOST_REFUSED: usize = 1024;

    pub(crate) fn record(&mut self, window_offset: u64, len: u64, change: WindowChangeKind) {
        if self.changes.len() == Self::MOST {
            self.changes.pop_front();
        }
        let at = std::time::Instant::now();
        if !self.refused.is_empty() {
            // What a VM process asked, which may be refused yet: past the end of a u64.
            let range = window_offset..window_offset.saturating_add(len);
            self.refused.retain(|refused| {
                if !range.contains(&refused.window_offset) {
                    return true;
                }
                log::warn!(
                    "cladefsd: {} refused at DAX window offset {:#x}, {:?} ago, mapping {}, \
                     met: {}, {len:#x} bytes from {window_offset:#x}",
                    refused.what,
                    refused.window_offset,
                    at.duration_since(refused.at),
                    refused.mapped,
                    change.describe(),
                );
                false
            });
        }
        self.changes.push_back(WindowChange { at, window_offset, len, change });
    }

    /// Notes the page at `window_offset` refused `what`, the window mapping `mapped` there: the
    /// next change covering it is logged.
    pub(crate) fn refuse(&mut self, window_offset: u64, what: &str, mapped: String) {
        if self.refused.len() == Self::MOST_REFUSED {
            self.refused.remove(0);
        }
        let at = std::time::Instant::now();
        self.refused.push(Refused { at, window_offset, what: what.to_owned(), mapped });
    }

    /// The changes of the page at `window_offset`, oldest first, with how long ago each was.
    pub(crate) fn of(&self, window_offset: u64) -> Vec<String> {
        let now = std::time::Instant::now();
        (self.changes.iter())
            .filter(|change| {
                let end = change.window_offset.saturating_add(change.len);
                (change.window_offset..end).contains(&window_offset)
            })
            .map(|change| {
                let (at, len, ago) =
                    (change.window_offset, change.len, now.duration_since(change.at));
                format!("{}, {len:#x} bytes from {at:#x}, {ago:?} ago", change.change.describe())
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures::FutureExt;
    use std::task::{Context, Poll, Waker};

    #[cfg(feature = "shuttle")]
    use shuttle::sync::Condvar;
    #[cfg(not(feature = "shuttle"))]
    use std::sync::Condvar;

    /// How a [`TestSource`]'s pages come into memory.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        /// Each is in memory from the start.
        Filled,
        /// Each is asked for, and comes in once the test supplies it ([`TestSource::supply`]).
        Paged,
        /// Each comes in, zeros, as it is first brought in, as a file's blocks never written do.
        Anonymous,
        /// cladefs: each is in memory from the start, its bytes the value, and comes in again so
        /// once evicted ([`TestSource::evict`]).
        Again(u8),
    }

    /// A test's source of pages, of physical memory's default arena.
    pub(crate) struct TestSource {
        kind: Kind,
        /// The extent it says its pages are, its own.
        extent: ExtentId,
        pages: Mutex<Vec<Option<PartSlab>>>,
        /// The pages asked for, and not yet taken ([`TestSource::request`]).
        requests: Mutex<Vec<u64>>,
        asked: Condvar,
        /// What waits for pages to be supplied, under the lock their supply takes, so that none
        /// misses it.
        wakers: Mutex<Vec<Waker>>,
    }

    impl TestSource {
        fn new(kind: Kind, pages: Vec<Option<PartSlab>>) -> Arc<Self> {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            Arc::new(Self {
                kind,
                extent: ExtentId { cell: 0, sequence },
                pages: Mutex::new(pages),
                requests: Mutex::new(Vec::new()),
                asked: Condvar::new(),
                wakers: Mutex::new(Vec::new()),
            })
        }

        /// A page of physical memory, its bytes `value`.
        fn page_of(value: u8) -> PartSlab {
            let page =
                crate::slabs::alloc(crate::slabs::ArenaId::DEFAULT, 1, crate::slabs::Wait::No);
            let page = page.unwrap();
            page.write(0, &vec![value; page_size() as usize]).unwrap();
            page
        }

        /// Where its page `page` is, in memory.
        pub fn location(&self, page: u64) -> PhysLocation {
            let pages = self.pages.lock().unwrap();
            pages[page as usize].as_ref().expect("the page in memory").location()
        }

        /// The first byte of its page `page`, in memory.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub fn byte(&self, page: u64) -> u8 {
            read_page(self.location(page))[0]
        }

        /// `pages` pages in memory, each its number plus `first`.
        pub fn numbered(pages: u64, first: u8) -> Arc<Self> {
            let pages = (0..pages).map(|page| Some(Self::page_of(first + page as u8))).collect();
            Self::new(Kind::Filled, pages)
        }

        /// `pages` pages in memory, each its bytes `value`.
        #[cfg_attr(not(any(target_os = "linux", feature = "shuttle")), allow(dead_code))]
        pub fn filled(pages: u64, value: u8) -> Arc<Self> {
            Self::new(Kind::Filled, (0..pages).map(|_| Some(Self::page_of(value))).collect())
        }

        /// cladefs: `pages` pages in memory, each its bytes `value`, which come in again so once
        /// evicted ([`TestSource::evict`]).
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub fn evictable(pages: u64, value: u8) -> Arc<Self> {
            Self::new(Kind::Again(value), (0..pages).map(|_| Some(Self::page_of(value))).collect())
        }

        /// cladefs: evicts its page `page`, as the cache evicts a part: in memory no more, which a
        /// window's fault finds from now on, then unmapped from `window`, then freed, its run free
        /// for the next allocation.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub fn evict(self: &Arc<Self>, window: &Window, page: u64) {
            let at = self.location(page);
            let slab = self.pages.lock().unwrap()[page as usize].take();
            window.unmap_pages_of(source_identity(self), &|location| location == at);
            drop(slab);
        }

        /// `pages` pages in memory, page `page`'s bytes `tag(page)`, scattered over physical
        /// memory: no two consecutive, so that a window maps a run of each.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub fn scattered(pages: u64, tag: impl Fn(u64) -> u8) -> Arc<Self> {
            let mut spacers = Vec::new();
            let pages = (0..pages)
                .map(|page| {
                    let frame = Self::page_of(tag(page));
                    spacers.push(Self::page_of(0));
                    Some(frame)
                })
                .collect();
            drop(spacers);
            Self::new(Kind::Filled, pages)
        }

        /// `pages` pages, none in memory, each brought in once the test supplies it.
        pub fn paged(pages: u64) -> Arc<Self> {
            Self::new(Kind::Paged, (0..pages).map(|_| None).collect())
        }

        /// `pages` pages, each zeros as it is first brought in.
        pub fn anonymous(pages: u64) -> Arc<Self> {
            Self::new(Kind::Anonymous, (0..pages).map(|_| None).collect())
        }

        /// It, as a window's mappings take it.
        pub fn source(self: &Arc<Self>) -> Arc<dyn Source> {
            self.clone()
        }

        /// Supplies `page` with bytes of `value`.
        pub fn supply(&self, page: u64, value: u8) {
            let frame = Self::page_of(value);
            let woken = {
                let mut pages = self.pages.lock().unwrap();
                pages[page as usize] = Some(frame);
                std::mem::take(&mut *self.wakers.lock().unwrap())
            };
            for waker in woken {
                waker.wake();
            }
        }

        /// The page asked for next, waiting for one.
        #[cfg_attr(not(any(target_os = "linux", feature = "shuttle")), allow(dead_code))]
        pub fn request(&self) -> u64 {
            let mut requests = self.requests.lock().unwrap();
            while requests.is_empty() {
                requests = self.asked.wait(requests).unwrap();
            }
            requests.remove(0)
        }

        /// The pages asked for, without waiting.
        pub fn requests(&self) -> Vec<u64> {
            std::mem::take(&mut *self.requests.lock().unwrap())
        }

        /// What a window's mapping of its pages from `page`, `len` bytes at `window_offset`, is
        /// recorded as ([`Dax::mappings`]).
        pub fn mapping_at(&self, window_offset: u64, page: u64, len: u64) -> WindowMapping {
            let contents =
                MappingContents::Extent { extent: self.extent, offset: page * page_size() };
            WindowMapping { window_offset, len, contents }
        }
    }

    /// The bytes of the page of physical memory at `at`, read through its arena's object, as a VM
    /// process mapping it reads them.
    pub(crate) fn read_page(at: PhysLocation) -> Vec<u8> {
        use std::os::unix::fs::FileExt;
        let fd = crate::slabs::arena_fd(at.arena).expect("the page's arena");
        let file = std::fs::File::from(fd.try_clone().expect("the arena's object, again"));
        let mut bytes = vec![0; page_size() as usize];
        file.read_exact_at(&mut bytes, at.offset).expect("the page read");
        bytes
    }

    /// A mapping of zeros, `len` bytes at `window_offset`, as [`Dax::mappings`] records it.
    pub(crate) fn zeros_at(window_offset: u64, len: u64) -> WindowMapping {
        WindowMapping { window_offset, len, contents: MappingContents::Zeros }
    }

    impl Source for TestSource {
        fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(Present<'_>)) {
            let held = self.pages.lock().unwrap();
            for page in pages {
                if let Some(Some(slab)) = held.get(page as usize) {
                    found(Present { page, count: 1, at: slab.location(), slab: Some(slab) });
                }
            }
        }

        fn bring_in(self: Arc<Self>, page: u64) -> BoxFuture<'static, Result<bool>> {
            let index = page as usize;
            {
                let mut pages = self.pages.lock().unwrap();
                if pages[index].is_some() {
                    return Box::pin(std::future::ready(Ok(false)));
                }
                match self.kind {
                    Kind::Filled => panic!("page {page} of a filled source is not in memory"),
                    Kind::Anonymous => {
                        pages[index] = Some(Self::page_of(0));
                        return Box::pin(std::future::ready(Ok(false)));
                    }
                    Kind::Again(value) => {
                        pages[index] = Some(Self::page_of(value));
                        return Box::pin(std::future::ready(Ok(false)));
                    }
                    Kind::Paged => {}
                }
            }
            self.requests.lock().unwrap().push(page);
            self.asked.notify_all();
            Box::pin(std::future::poll_fn(move |cx| {
                let pages = self.pages.lock().unwrap();
                if pages[index].is_some() {
                    return Poll::Ready(Ok(true));
                }
                self.wakers.lock().unwrap().push(cx.waker().clone());
                Poll::Pending
            }))
        }

        fn origin(&self) -> Origin {
            Origin::Extent(self.extent)
        }
    }

    /// The window's mappings of a freed extent are found by its pages' identity, as mapping over
    /// parts of them splits them, and map zeros; the window keeps its pages no more.
    #[test]
    fn freed_extents_are_found_where_mappings_over_them_left_them() {
        let ps = page_size();
        let (a, b) = (TestSource::anonymous(8), TestSource::anonymous(8));
        let (mut dax, _window) = Dax::new(8 * ps, None).unwrap();
        dax.map(0, &a.source(), 0, 4 * ps);
        dax.map(4 * ps, &b.source(), 4 * ps, 2 * ps);
        // Over the middle of `a`'s mapping, which splits around it.
        dax.map(ps, &b.source(), ps, ps);

        assert_eq!(dax.map_zeros_over(&a), [0..ps, 2 * ps..4 * ps]);
        let mappings = [
            zeros_at(0, ps),
            b.mapping_at(ps, 1, ps),
            zeros_at(2 * ps, 2 * ps),
            b.mapping_at(4 * ps, 4, 2 * ps),
        ];
        assert_eq!(dax.mappings(), mappings);
        assert_eq!(Arc::strong_count(&a), 1, "the window still keeps what it no longer maps");
        assert_eq!(dax.map_zeros_over(&a), [], "mapped once freed");
    }

    /// Faults wait for their pages together, on one thread, and each completes once its page is
    /// supplied.
    #[test]
    fn faults_wait_concurrently() {
        let pager = TestSource::paged(2);
        let (dax, window) = Dax::new(2 * page_size(), None).unwrap();
        dax.window.map(0, &pager.source(), 0, 2 * page_size());
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        let mut first = Box::pin(window.fault(0, Access::Read));
        let mut second = Box::pin(window.fault(page_size(), Access::Read));
        assert!(first.poll_unpin(&mut cx).is_pending());
        assert!(second.poll_unpin(&mut cx).is_pending());
        let mut requests = pager.requests();
        requests.sort();
        assert_eq!(requests, [0, 1]);

        pager.supply(1, 2);
        assert_eq!(second.poll_unpin(&mut cx), Poll::Ready(Ok(())));
        assert!(first.poll_unpin(&mut cx).is_pending());
        pager.supply(0, 1);
        assert_eq!(first.poll_unpin(&mut cx), Poll::Ready(Ok(())));
        assert!(window.is_mapped(0) && window.is_mapped(page_size()));
        // SAFETY: Both pages are mapped, and nothing else accesses them.
        let bytes = unsafe { [*window.host_base(), *window.host_base().add(page_size() as usize)] };
        assert_eq!(bytes, [1, 2]);
    }

    /// A second stage that keeps the window's pages as the window maps them in it, and the ranges
    /// the window maps anew. cladefs: and each unmap, with what it does to the private pages there
    /// (doc-26).
    #[derive(Default)]
    pub(crate) struct Recorded {
        pages: std::sync::Mutex<BTreeMap<u64, PhysLocation>>,
        mapped: std::sync::Mutex<Vec<(u64, u64)>>,
        unmaps: std::sync::Mutex<Vec<(u64, u64, Private)>>,
    }

    impl Recorded {
        /// The pages mapped, by their numbers.
        pub fn pages(&self) -> Vec<u64> {
            let pages = self.pages.lock().unwrap();
            pages.keys().map(|offset| offset / page_size()).collect()
        }

        /// cladefs: the unmaps since it was last asked, by their pages' numbers and how many.
        pub fn unmaps(&self) -> Vec<(u64, u64, Private)> {
            let ps = page_size();
            let unmaps = std::mem::take(&mut *self.unmaps.lock().unwrap());
            unmaps
                .into_iter()
                .map(|(offset, len, private)| (offset / ps, len / ps, private))
                .collect()
        }

        fn each_page(offset: u64, len: u64) -> impl Iterator<Item = u64> {
            (offset..offset + len).step_by(page_size() as usize)
        }
    }

    impl SecondStage for Recorded {
        fn map(&self, offset: u64, phys: PhysLocation, len: u64) {
            let mut pages = self.pages.lock().unwrap();
            for page in Self::each_page(offset, len) {
                let at = PhysLocation { arena: phys.arena, offset: phys.offset + (page - offset) };
                let before = pages.insert(page, at);
                assert!(before.is_none(), "{page:#x} mapped twice");
            }
        }

        fn unmap(&self, offset: u64, len: u64, private: Private) {
            let mut pages = self.pages.lock().unwrap();
            for page in Self::each_page(offset, len) {
                pages.remove(&page);
            }
            self.unmaps.lock().unwrap().push((offset, len, private));
        }

        fn window_mapped(&self, offset: u64, len: u64) {
            self.mapped.lock().unwrap().push((offset, len));
        }
    }

    /// The byte at the start of page `page` of the window, which a load faults in first: of a
    /// window with no second stage, whose host memory the window maps.
    fn load(window: &DaxWindow, page: u64) -> u8 {
        futures::executor::block_on(window.fault(page * page_size(), Access::Read)).unwrap();
        // SAFETY: The page is mapped, and nothing else accesses it.
        unsafe { *window.host_base().add((page * page_size()) as usize) }
    }

    /// Unmapping part of a mapping leaves what is around it.
    #[test]
    fn ranges_are_unmapped_where_they_are() {
        let ps = page_size();
        let pager = TestSource::paged(4);
        let stage = Arc::new(Recorded::default());
        let (mut dax, window) = Dax::new(4 * ps, Some(stage.clone())).unwrap();
        dax.map(0, &pager.source(), 0, 4 * ps);
        assert_eq!(stage.pages(), Vec::<u64>::new());
        pager.supply(2, 7);
        futures::executor::block_on(window.fault(2 * ps, Access::Read)).unwrap();
        assert_eq!(stage.pages(), [2]);

        dax.unmap(2 * ps, ps);
        assert_eq!(stage.pages(), Vec::<u64>::new());
        assert!(!window.is_mapped(2 * ps) && window.is_mapped(ps) && window.is_mapped(3 * ps));
        assert_eq!(
            dax.mappings(),
            [pager.mapping_at(0, 0, 2 * ps), pager.mapping_at(3 * ps, 3, ps)]
        );
    }

    /// The windows' gauges (`live`) follow their mappings, split and unmapped, and their pages,
    /// mapped and unmapped, and drop to nothing as the windows go; cladefs: and so do each window's
    /// own (`DaxWindow::gauges`, TASK-111.5), which add up to them, held past their window or not.
    /// Each test runs in a process of its own (nextest), whose windows are its own.
    #[test]
    fn live_gauges_follow_the_windows_mappings_and_pages() {
        let ps = page_size();
        let gauges = |mappings, mapped_pages: u64, pages| Live {
            mappings,
            mapped_bytes: mapped_pages * ps,
            pages,
        };
        let (source, other) = (TestSource::numbered(4, 1), TestSource::paged(2));
        let (mut dax, window) = Dax::new(6 * ps, Some(Arc::new(Recorded::default()))).unwrap();
        let (mut second, second_window) =
            Dax::new(2 * ps, Some(Arc::new(Recorded::default()))).unwrap();
        assert_eq!(live(), Live::default());
        assert_eq!(window.gauges().live(), Live::default());

        dax.map(0, &source.source(), 0, 4 * ps);
        dax.map(4 * ps, &other.source(), 0, 2 * ps);
        second.map(0, &TestSource::numbered(2, 5).source(), 0, 2 * ps);
        assert_eq!(live(), gauges(3, 8, 6));
        assert_eq!(window.gauges().live(), gauges(2, 6, 4));
        assert_eq!(second_window.gauges().live(), gauges(1, 2, 2));

        other.supply(1, 3);
        futures::executor::block_on(window.fault(5 * ps, Access::Read)).unwrap();
        assert_eq!(live(), gauges(3, 8, 7));
        assert_eq!(window.gauges().live(), gauges(2, 6, 5));

        // The first mapping split in two around the page unmapped.
        dax.unmap(ps, ps);
        assert_eq!(live(), gauges(4, 7, 6));
        assert_eq!(window.gauges().live(), gauges(3, 5, 4));
        assert_eq!(second_window.gauges().live(), gauges(1, 2, 2));

        let held = second_window.gauges();
        drop((second, second_window));
        assert_eq!(live(), gauges(3, 5, 4));
        assert_eq!(held.live(), Live::default());
        drop((dax, window));
        assert_eq!(live(), Live::default());
    }

    /// A source's pages unmapped by it, as it evicts them, are unmapped where a window maps them
    /// alone: the other pages, and other sources', stay mapped.
    #[test]
    fn a_sources_evicted_pages_are_unmapped_from_the_window() {
        let ps = page_size();
        let (source, other) = (TestSource::numbered(4, 1), TestSource::numbered(2, 9));
        let stage = Arc::new(Recorded::default());
        let (mut dax, _window) = Dax::new(6 * ps, Some(stage.clone())).unwrap();
        dax.map(0, &source.source(), 0, 4 * ps);
        dax.map(4 * ps, &other.source(), 0, 2 * ps);
        let evicted: Vec<PhysLocation> = {
            let mut evicted = Vec::new();
            source.each_present(1..3, &mut |present| evicted.push(present.at));
            evicted
        };
        dax.window.unmap_pages_of(source_identity(&source), &|at| evicted.contains(&at));
        assert_eq!(stage.pages(), [0, 3, 4, 5]);
    }

    /// TASK-234: a window rebound from one source to another, the same pages of the same extent
    /// moved, maps nothing of the first, the second's pages as they are faulted on, and its
    /// mappings are the second's: pages loaded before it read the first's bytes, after it the
    /// second's, never a page of the first left mapped.
    #[test]
    fn a_rebound_window_maps_nothing_of_its_old_source() {
        let ps = page_size();
        let (old, new) = (TestSource::numbered(4, 1), TestSource::numbered(4, 11));
        let (mut dax, window) = Dax::new(4 * ps, None).unwrap();
        dax.map(0, &old.source(), 0, 4 * ps);
        let loaded: Vec<u8> = (0..4).map(|page| load(&window, page)).collect();
        assert_eq!(loaded, [1, 2, 3, 4]);

        dax.window.rebind(&old.source(), new.source());
        let loaded: Vec<u8> = (0..4).map(|page| load(&window, page)).collect();
        assert_eq!(loaded, [11, 12, 13, 14]);
        assert_eq!(dax.mappings(), [new.mapping_at(0, 0, 4 * ps)]);

        let stage = Arc::new(Recorded::default());
        let (mut dax, window) = Dax::new(4 * ps, Some(stage.clone())).unwrap();
        dax.map(0, &old.source(), 0, 4 * ps);
        for page in 0..4 {
            futures::executor::block_on(window.fault(page * ps, Access::Read)).unwrap();
        }
        dax.window.rebind(&old.source(), new.source());
        assert_eq!(stage.pages(), Vec::<u64>::new());
    }

    /// Ranges of a window are page aligned, of some pages, and within it.
    #[test]
    fn ranges_are_whole_pages_within_the_window() {
        let ps = page_size();
        let (dax, window) = Dax::new(4 * ps, None).unwrap();
        assert_eq!(window.size(), 4 * ps);
        assert_eq!(dax.check_range(0, 4 * ps), Ok(()));
        assert_eq!(dax.check_range(3 * ps, ps), Ok(()));
        for (offset, len) in
            [(0, 0), (1, ps), (0, ps + 1), (ps, 4 * ps), (4 * ps, ps), (u64::MAX - ps + 1, ps)]
        {
            assert_eq!(dax.check_range(offset, len), Err(Errno::EINVAL), "{offset:#x}+{len:#x}");
        }
        assert_eq!(Dax::new(ps + 1, None).err(), Some(Errno::EINVAL));
        assert_eq!(Dax::new(0, None).err(), Some(Errno::EINVAL));
    }

    /// The write fault at `offset` of `window`, resolved.
    fn write_fault(window: &DaxWindow, offset: u64) -> Result<()> {
        futures::executor::block_on(window.fault(offset, Access::Write))
    }

    /// cladefs (doc-26): a window refuses every write, as its stage, a writer's, makes its own
    /// private copies: `EACCES` where it maps something, `ENOENT` where nothing.
    #[test]
    fn windows_refuse_writes() {
        let ps = page_size();
        let source = TestSource::numbered(2, 1);
        let (mut dax, window) = Dax::new(2 * ps, None).unwrap();
        dax.map(0, &source.source(), 0, ps);
        assert_eq!(write_fault(&window, 0), Err(Errno::EACCES));
        assert_eq!(write_fault(&window, ps), Err(Errno::ENOENT), "a write to nothing mapped");
        assert_eq!(load(&window, 0), 1, "the page as it was");
    }

    /// cladefs TASK-164.1, doc-26 (spec rule evictionKeepsPrivate): a window whose stage copies
    /// unmaps for a source evicting its pages, a free mapping zeros and a rebind only the pages it
    /// mapped, keeping the private pages there, and remembers them; for the guest's own `MAP` and
    /// `UNMAP`, ending the private pages there, the pages it mapped and those it remembers, which
    /// it maps nothing at, once. Shown red by `Rules::guests_changes_end_private` off, or
    /// map_zeros_over mapping with `Private::End`.
    #[test]
    fn guests_changes_end_private_pages_and_the_hosts_keep_them() {
        use Private::{End, Keep};
        let ps = page_size();
        let (old, new) = (TestSource::numbered(4, 1), TestSource::numbered(4, 0x11));
        let stage = Arc::new(Recorded::default());
        let (mut dax, window) = Dax::copying(8 * ps, stage.clone()).unwrap();
        dax.map(0, &old.source(), 0, 4 * ps);
        assert_eq!(stage.unmaps(), [], "the guest's MAP over nothing");
        for page in [0, 2] {
            futures::executor::block_on(window.fault(page * ps, Access::Read)).unwrap();
        }
        assert_eq!(stage.pages(), [0, 1, 2, 3]);

        dax.window.unmap_pages_of(source_identity(&old), &|_| true);
        assert_eq!(stage.unmaps(), [(0, 4, Keep)], "evicted");
        dax.unmap(0, ps);
        assert_eq!(stage.unmaps(), [(0, 1, End)], "the guest's UNMAP of a page an eviction left");
        dax.unmap(0, ps);
        assert_eq!(stage.unmaps(), [], "the guest's UNMAP of nothing");
        assert_eq!(dax.map_zeros_over(&old), std::slice::from_ref(&(ps..4 * ps)));
        assert_eq!(stage.unmaps(), [], "freed, nothing mapped");
        dax.map(ps, &new.source(), 0, 2 * ps);
        assert_eq!(stage.unmaps(), [(1, 2, End)], "the guest's MAP over zeros and what was left");

        let plain = Arc::new(Recorded::default());
        let (mut dax, _window) = Dax::new(4 * ps, Some(plain.clone())).unwrap();
        dax.map(0, &new.source(), 0, 2 * ps);
        dax.window.unmap_pages_of(source_identity(&new), &|_| true);
        let _ = plain.unmaps();
        dax.map(0, &new.source(), 0, 2 * ps);
        assert_eq!(plain.unmaps(), [], "a window that doesn't copy unmaps what it mapped alone");
    }

    /// TASK-44.12: a page refused waits for the next change of the window covering it, which
    /// meets it (and is logged), whatever the changes of other pages meanwhile.
    #[test]
    fn refused_pages_meet_the_next_change_covering_them() {
        let mut changes = WindowChanges::default();
        changes.refuse(0x3000, "a write", "an extent, read-only".into());
        changes.refuse(0x9000, "a write", "nothing mapped".into());
        changes.record(0x4000, 0x4000, WindowChangeKind::Unmap);
        let waiting = |changes: &WindowChanges| -> Vec<u64> {
            changes.refused.iter().map(|refused| refused.window_offset).collect()
        };
        assert_eq!(waiting(&changes), [0x3000, 0x9000]);
        changes.record(0x2000, 0x2000, WindowChangeKind::Zeros);
        assert_eq!(waiting(&changes), [0x9000]);
        changes.record(0x8000, 0x2000, WindowChangeKind::Unmap);
        assert_eq!(waiting(&changes), Vec::<u64>::new());
    }

    /// TASK-77: what a VM process asked of the window, recorded before it is refused, may reach
    /// past the end of a u64, with a page refused waiting.
    #[test]
    fn changes_past_the_end_of_a_u64_are_recorded() {
        let mut changes = WindowChanges::default();
        changes.refuse(0x3000, "a write", "an extent, read-only".into());
        changes.record(u64::MAX - 0xfff, 0x2000, WindowChangeKind::Unmap);
        assert_eq!(changes.refused.len(), 1);
        assert_eq!(changes.of(u64::MAX - 0xfff).len(), 1);
    }
}
