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

pub mod failures;
// Its guest's only second stage is Linux's (`uffd::shuttle_tests`): elsewhere nothing runs it.
#[cfg(target_os = "macos")]
pub mod hvf;
#[cfg(all(test, feature = "shuttle", target_os = "linux"))]
mod shuttle_tests;
#[cfg(target_os = "linux")]
pub mod uffd;

/// Reads `bytes.len()` bytes of `fd`'s memory object at `offset` into `bytes`, retrying `EINTR`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_at(fd: std::os::fd::RawFd, bytes: &mut [u8], offset: u64) {
    let mut done = 0;
    while done < bytes.len() {
        let rest = &mut bytes[done..];
        let at = (offset + done as u64) as libc::off_t;
        // SAFETY: Reads into `rest`, which is as long as asked.
        let read = unsafe { libc::pread(fd, rest.as_mut_ptr().cast(), rest.len(), at) };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "read {offset:#x}: {err}");
            continue;
        }
        assert!(read > 0, "read {offset:#x}: past the memory's end");
        done += read as usize;
    }
}

use crate::error::{Errno, Result};
use crate::open_cell::{MappingContents, WindowMapping};
use crate::pages::{ArenaId, PhysLocation, page_size};
use crate::slabs::{PartSlab, VmProcessId};
use futures::future::BoxFuture;
use local_cell::shared::SharedLock;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use storage_device::ExtentId;

/// The logical alignment of DAX window `MAP`, `UNMAP`, and `PRIVATE` ranges: the cell's block size
/// (4 KiB), or the host page size if smaller. On hosts with larger pages (such as 16 KiB arm64),
/// `Window` tracks mappings at this granularity, sharing host-page-aligned runs directly from
/// cached extents and composing unaligned or mixed host pages into budgeted `PartSlab`s.
pub fn map_unit() -> u64 {
    u64::from(crate::OpenCell::BLOCK_SIZE).min(page_size())
}

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
pub struct DaxMapping {
    pub offset: u64,
    pub len: u64,
    pub source: Option<(Arc<dyn Source>, u64)>,
}

/// cladefs: one of the mappings [`Window::map_many`] makes: `len` bytes at the window's `offset`,
/// of `source`'s bytes from `source_offset`.
#[derive(Clone, Copy)]
struct Piece<'a> {
    offset: u64,
    len: u64,
    source: &'a Arc<dyn Source>,
    source_offset: u64,
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

/// What resolves the faults on a window's second stage, in host memory (`dax::uffd`) or a guest's
/// (`dax::hvf`): the VM process's connection to the extent daemon, which holds the window
/// (`crate::cladefsd`), or the window itself, in tests whose window is in this process's host
/// memory (`Dax::in_host_memory`).
pub trait Faults: Send + Sync {
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
pub struct Present<'a> {
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
pub trait Source: Send + Sync {
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

    /// Whether this source has been revoked (e.g. tenant KEK destroyed or cell shredded) and must
    /// not be published into a composed DAX page.
    fn is_revoked(&self) -> bool {
        false
    }
}

/// What a [`Source`]'s pages are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A sealed extent's, numbered from its first.
    Extent(ExtentId),
    /// Zeros.
    Zeros,
}

/// The identity of `source`, the same for each of its handles: what a window finds the mappings of
/// a source by.
pub fn source_identity<T: ?Sized>(source: &Arc<T>) -> usize {
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
    #[cfg(any(test, feature = "testing"))]
    pub fn host_base(&self) -> *mut u8 {
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

    /// cladefs: the sealed extent mapped at `offset`, and the offset in it of the logical block
    /// `offset` is in, if one is mapped there: what a trace of the guest's reads names a fault by
    /// (TASK-702.1).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn extent_at(&self, offset: u64) -> Option<(ExtentId, u64)> {
        let unit = self.0.unit;
        let state = self.0.state.with(Mutex::lock);
        let (start, mapping) = state.mapping_at(offset)?;
        match mapping.source.origin() {
            Origin::Extent(extent) => {
                Some((extent, mapping.source_offset + (offset - start) / unit * unit))
            }
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
    pub fn gauges(&self) -> Arc<Gauges> {
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
    /// In host memory: a reservation of host address space, at `base`, as long as the window
    /// (tests only; production windows always have a [`SecondStage`], board 11697).
    #[cfg(any(test, feature = "testing"))]
    Host { base: usize },
}

/// A mapping of a window: a range of its source's bytes, from `source_offset` (a multiple of the
/// window's logical unit), up to the window's `end`, read-only.
struct Mapping {
    end: u64,
    source: Arc<dyn Source>,
    source_offset: u64,
}

/// A contiguous run of host pages staged in the window (`offset..offset + len`), backed by
/// contiguous physical pages in one arena (`phys_offset..phys_offset + len`), or a single composed
/// host page (`composed == true`, `len == page_size()`). Replaces per-page PTE entries (board
/// 11697).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StagedRun {
    len: u64,
    phys_offset: u64,
    arena: ArenaId,
    composed: bool,
}

impl StagedRun {
    fn phys(self) -> PhysLocation {
        PhysLocation { arena: self.arena, offset: self.phys_offset }
    }
}

/// Coalesces overlapping or adjacent byte ranges, returning them sorted and non-overlapping.
fn coalesce_ranges(mut ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
    ranges.retain(|r| r.start < r.end);
    ranges.sort_by_key(|r| r.start);
    let mut joined: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match joined.last_mut() {
            Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
            _ => joined.push(range),
        }
    }
    joined
}

/// A composed host page in a window whose host page size exceeds its logical mapping unit (e.g.
/// 4 KiB blocks on a 16 KiB arm64 host): a budgeted 1-page [`PartSlab`] holding the assembled
/// bytes of sub-page or unaligned mappings, together with the `(source_identity, source_phys)`
/// pages it was copied from so eviction or revocation of any constituent extent unmaps and drops
/// it.
struct ComposedPage {
    slab: PartSlab,
    deps: Vec<(usize, PhysLocation)>,
    /// Monotonic touch counter for LRU eviction under arena/host memory relief.
    touch: u64,
}

/// A window's mappings and the pages it has mapped, under its lock.
struct WindowState {
    /// The mapped ranges of the window, by start. They do not overlap.
    mappings: BTreeMap<u64, Mapping>,
    /// The starts of the mappings, by the identity of their source ([`source_identity`]).
    by_source: HashMap<usize, BTreeSet<u64>>,
    /// The contiguous runs of host pages mapped, by host-page-aligned start offset: where in
    /// physical memory each run is (either directly in a source's slab or in a [`ComposedPage`]
    /// below; board 11697).
    ptes: BTreeMap<u64, StagedRun>,
    /// Composed host pages owned by this window, keyed by host-page-aligned window offset (empty
    /// when `unit == page_size()` or when all mapped host pages are directly aligned).
    composed: BTreeMap<u64, ComposedPage>,
    /// Clock incremented whenever a composed host page is installed.
    composed_clock: u64,
    /// Shared count of `composed.len()` so `Window::evict_composed` can skip locking `state` when
    /// no composed host pages exist (always true on 4 KiB hosts; board 12315).
    composed_count: Arc<AtomicUsize>,
    /// cladefs: of a window whose stage copies on write, the runs of bytes (logical-unit aligned),
    /// by start and end, that the host's own changes took away from the stage keeping its private
    /// pages there ([`Private::Keep`]), or untouched sibling sub-pages of a host page removed by a
    /// partial [`Private::End`]: where the stage may have a private sub-page the window has mapped
    /// nothing at, which the guest's next change there ends (doc-26). Elsewhere, a private page is
    /// where the window has a page mapped.
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
pub struct Gauges {
    mappings: AtomicU64,
    mapped_bytes: AtomicU64,
    pages: AtomicU64,
}

impl Gauges {
    /// The window's mappings, as of now.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn live(&self) -> Live {
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
pub struct Live {
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
pub fn live() -> Live {
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
    fn new(composed_count: Arc<AtomicUsize>) -> Self {
        let gauges = Arc::new(Gauges::default());
        WINDOWS.lock().unwrap().push(Arc::downgrade(&gauges));
        Self {
            mappings: BTreeMap::new(),
            by_source: HashMap::new(),
            ptes: BTreeMap::new(),
            composed: BTreeMap::new(),
            composed_clock: 0,
            composed_count,
            kept: BTreeMap::new(),
            gauges,
        }
    }

    fn insert_composed(&mut self, offset: u64, page: ComposedPage) {
        if self.composed.insert(offset, page).is_none() {
            self.composed_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether host page `offset` (host-page-aligned) is currently staged in [`WindowState::ptes`].
    fn has_pte(&self, offset: u64) -> bool {
        self.ptes.range(..=offset).next_back().is_some_and(|(&start, run)| start + run.len > offset)
    }

    /// The sub-ranges of `start..end` (host-page-aligned) not covered by any staged run in `ptes`.
    fn uncovered_runs(&self, start: u64, end: u64) -> Vec<Range<u64>> {
        if start >= end {
            return Vec::new();
        }
        let mut cur = start;
        let mut gaps = Vec::new();
        if let Some((&r_start, run)) = self.ptes.range(..start).next_back() {
            let r_end = r_start + run.len;
            if r_end > cur {
                cur = r_end.min(end);
            }
        }
        for (&r_start, run) in self.ptes.range(start..end) {
            if r_start > cur {
                gaps.push(cur..r_start);
            }
            cur = cur.max((r_start + run.len).min(end));
            if cur >= end {
                break;
            }
        }
        if cur < end {
            gaps.push(cur..end);
        }
        gaps
    }

    /// Records that the window has mapped the contiguous `len`-byte host-page run at `phys` at
    /// `start` (where nothing was mapped), coalescing with physically adjacent direct runs.
    fn insert_run(&mut self, mut start: u64, mut phys: PhysLocation, mut len: u64, composed: bool) {
        let ps = page_size();
        debug_assert!(len > 0 && start.is_multiple_of(ps) && len.is_multiple_of(ps));
        debug_assert!(!composed || len == ps);
        debug_assert_eq!(
            self.uncovered_runs(start, start + len).as_slice(),
            std::slice::from_ref(&(start..start + len)),
            "a page mapped over another at {start:#x}"
        );
        self.gauges.pages.fetch_add(len / ps, Ordering::Relaxed);
        if !composed {
            if let Some((&prev_start, &prev)) = self.ptes.range(..start).next_back()
                && !prev.composed
                && prev_start + prev.len == start
                && prev.arena == phys.arena
                && prev.phys_offset + prev.len == phys.offset
            {
                self.ptes.remove(&prev_start);
                start = prev_start;
                phys = prev.phys();
                len += prev.len;
            }
            if let Some(&next) = self.ptes.get(&(start + len))
                && !next.composed
                && next.arena == phys.arena
                && phys.offset + len == next.phys_offset
            {
                self.ptes.remove(&(start + len));
                len += next.len;
            }
        }
        self.ptes.insert(
            start,
            StagedRun { len, phys_offset: phys.offset, arena: phys.arena, composed },
        );
    }

    /// cladefs: records that the byte range `start..end` was taken away keeping private pages there
    /// ([`WindowState::kept`]), merged with overlapping or adjacent runs.
    fn keep_range(&mut self, mut start: u64, mut end: u64) {
        if start >= end {
            return;
        }
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

    /// Removes all staged host pages in `h_start..h_end` (host-page-aligned), splitting any
    /// straddling runs, and returns the coalesced host-page ranges removed together with any
    /// composed [`PartSlab`]s dropped (to be released only after sending second-stage unmaps).
    fn remove_pte_range(&mut self, h_start: u64, h_end: u64) -> (Vec<Range<u64>>, Vec<PartSlab>) {
        if h_start >= h_end {
            return (Vec::new(), Vec::new());
        }
        let ps = page_size();
        let before = self
            .ptes
            .range(..h_start)
            .next_back()
            .filter(|&(start, run)| *start + run.len > h_start)
            .map(|(&start, &run)| (start, run));
        let within = self.ptes.range(h_start..h_end).map(|(&start, &run)| (start, run));
        let overlapping: Vec<(u64, StagedRun)> = before.into_iter().chain(within).collect();
        let mut removed_runs: Vec<Range<u64>> = Vec::new();
        let mut removed_pages = 0u64;
        for (r_start, run) in overlapping {
            self.ptes.remove(&r_start);
            let r_end = r_start + run.len;
            if r_start < h_start {
                self.ptes.insert(r_start, StagedRun { len: h_start - r_start, ..run });
            }
            if r_end > h_end {
                let shift = h_end - r_start;
                self.ptes.insert(
                    h_end,
                    StagedRun { len: r_end - h_end, phys_offset: run.phys_offset + shift, ..run },
                );
            }
            let c_start = r_start.max(h_start);
            let c_end = r_end.min(h_end);
            removed_pages += (c_end - c_start) / ps;
            match removed_runs.last_mut() {
                Some(last) if last.end == c_start => last.end = c_end,
                _ => removed_runs.push(c_start..c_end),
            }
        }
        if removed_pages > 0 {
            self.gauges.pages.fetch_sub(removed_pages, Ordering::Relaxed);
        }
        let mut dropped = Vec::new();
        if !self.composed.is_empty() {
            let composed_keys: Vec<u64> =
                self.composed.range(h_start..h_end).map(|(&k, _)| k).collect();
            for offset in composed_keys {
                if let Some(composed) = self.composed.remove(&offset) {
                    self.composed_count.fetch_sub(1, Ordering::Relaxed);
                    dropped.push(composed.slab);
                }
            }
        }
        (removed_runs, dropped)
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
        let mut forgotten = Vec::new();
        for mapping_start in self.overlapping(start, end) {
            let mapping = self.remove(mapping_start);
            if mapping_start < start {
                let before = Mapping { end: start, source: mapping.source.clone(), ..mapping };
                self.insert(mapping_start, before);
            }
            if mapping.end > end {
                let source_offset = mapping.source_offset + (end - mapping_start);
                let after = Mapping { source_offset, source: mapping.source.clone(), ..mapping };
                self.insert(end, after);
            }
            forgotten.push(mapping.source);
        }
        forgotten
    }
}

/// One extent slice contributing to a composed host page.
struct ComposeSlice {
    win_within: usize,
    len: usize,
    source: Arc<dyn Source>,
    source_offset: u64,
}

/// How one host page of a DAX window is backed by its logical mappings.
enum HostPagePlan {
    /// No logical mapping overlaps this host page.
    Unmapped,
    /// Every overlapping logical mapping is zeros (so the shared zeros page backs it directly).
    DirectZeros,
    /// One extent mapping covers the full host page at a host-page-aligned source offset.
    Direct { source: Arc<dyn Source>, src_page: u64 },
    /// Unaligned extent offset, multiple mappings, or partial host page: assembled into a
    /// budgeted 1-page [`PartSlab`].
    Composed(Vec<ComposeSlice>),
}

/// Assembled host-page bytes, source-page dependencies, and target arena for a composed DAX page.
type ComposedBytes = (Vec<u8>, Vec<(usize, PhysLocation)>, ArenaId);

/// Reads all slices of a composed host page from their sources' present slabs into a host-page
/// buffer, returning `(bytes, deps, arena)` if every constituent source page is present in memory,
/// or `None` if at least one source host page still needs `bring_in`.
///
/// Composed slabs are per-window unshared memory, so a tenant window (`window_arena != DEFAULT`)
/// always charges its own tenant arena—even when composing from public base extents in
/// `ArenaId::DEFAULT`—preventing tenant memory cap bypasses.
fn read_composed_slices(
    slices: &[ComposeSlice],
    window_arena: ArenaId,
) -> Result<Option<ComposedBytes>> {
    let ps = page_size();
    let mut buf = vec![0u8; ps as usize];
    let mut deps = Vec::new();
    let mut arena = window_arena;
    for slice in slices {
        let p_start = slice.source_offset / ps;
        let p_end = (slice.source_offset + slice.len as u64).div_ceil(ps);
        let mut covered = 0u64;
        let mut read_err = None;
        slice.source.each_present(p_start..p_end, &mut |present| {
            for page in present.page..present.page + present.count {
                let page_start = page * ps;
                let page_end = page_start + ps;
                let from = slice.source_offset.max(page_start);
                let to = (slice.source_offset + slice.len as u64).min(page_end);
                let copy_len = (to - from) as usize;
                let dst_off = slice.win_within + (from - slice.source_offset) as usize;
                let phys = PhysLocation {
                    offset: present.at.offset + (page - present.page) * ps,
                    ..present.at
                };
                if let Some(slab) = present.slab {
                    let slab_off = (phys.offset - slab.location().offset) + (from - page_start);
                    if let Err(err) = slab.read(slab_off, &mut buf[dst_off..dst_off + copy_len]) {
                        log::error!("cladefs: reading slab for composed DAX page: {err}");
                        read_err = Some(Errno::EIO);
                    }
                }
                if phys.arena != ArenaId::DEFAULT {
                    debug_assert!(
                        arena == ArenaId::DEFAULT || arena == phys.arena,
                        "composed DAX page spans two tenant arenas: {arena:?} vs {:?}",
                        phys.arena
                    );
                    if arena == ArenaId::DEFAULT {
                        arena = phys.arena;
                    }
                }
                deps.push((source_identity(&slice.source), phys));
                covered += 1;
            }
        });
        if let Some(err) = read_err {
            return Err(err);
        }
        if covered < p_end - p_start {
            return Ok(None);
        }
    }
    Ok(Some((buf, deps, arena)))
}

/// Whether two composed host-page slice plans refer to the exact same sources and byte ranges.
fn same_compose_slices(a: &[ComposeSlice], b: &[ComposeSlice]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.win_within == y.win_within
                && x.len == y.len
                && source_identity(&x.source) == source_identity(&y.source)
                && x.source_offset == y.source_offset
        })
}

/// Assembles `slices` into one host-page buffer, bringing in each missing source host page one at
/// a time so even a 1-slab tenant cap can compose pages from full-slab parts without needing
/// multiple parts (or a source part and destination composed slab) resident simultaneously
/// (review 102 finding 351).
async fn assemble_composed_slices(
    slices: &[ComposeSlice],
    window_arena: ArenaId,
) -> Result<(ComposedBytes, bool)> {
    let ps = page_size();
    let mut buf = vec![0u8; ps as usize];
    let mut deps = Vec::new();
    let mut arena = window_arena;
    let mut waited = false;
    for slice in slices {
        let p_start = slice.source_offset / ps;
        let p_end = (slice.source_offset + slice.len as u64).div_ceil(ps);
        for page in p_start..p_end {
            let page_start = page * ps;
            let page_end = page_start + ps;
            let from = slice.source_offset.max(page_start);
            let to = (slice.source_offset + slice.len as u64).min(page_end);
            let copy_len = (to - from) as usize;
            let dst_off = slice.win_within + (from - slice.source_offset) as usize;
            loop {
                if slice.source.is_revoked() {
                    return Err(Errno::EACCES);
                }
                let mut copied = false;
                let mut read_err = None;
                let mut found_phys = None;
                slice.source.each_present(page..page + 1, &mut |present| {
                    let phys = PhysLocation {
                        offset: present.at.offset + (page - present.page) * ps,
                        ..present.at
                    };
                    if let Some(slab) = present.slab {
                        let slab_off = (phys.offset - slab.location().offset) + (from - page_start);
                        if let Err(err) = slab.read(slab_off, &mut buf[dst_off..dst_off + copy_len])
                        {
                            log::error!("cladefs: reading slab for composed DAX page: {err}");
                            read_err = Some(Errno::EIO);
                        }
                    }
                    found_phys = Some(phys);
                    copied = true;
                });
                if let Some(err) = read_err {
                    return Err(err);
                }
                if copied {
                    let phys = found_phys.expect("present page has location");
                    if phys.arena != ArenaId::DEFAULT {
                        debug_assert!(
                            arena == ArenaId::DEFAULT || arena == phys.arena,
                            "composed DAX page spans two tenant arenas: {arena:?} vs {:?}",
                            phys.arena
                        );
                        if arena == ArenaId::DEFAULT {
                            arena = phys.arena;
                        }
                    }
                    deps.push((source_identity(&slice.source), phys));
                    break;
                }
                waited |= slice.source.clone().bring_in(page).await?;
            }
        }
    }
    Ok(((buf, deps, arena), waited))
}

/// A window. See the module docs.
pub struct Window {
    size: u64,
    /// Logical mapping alignment in bytes (normally [`map_unit()`], 4 KiB).
    unit: u64,
    /// Owning cell's tenant arena (`ArenaId::DEFAULT` for tenant 0 / public cells).
    arena: ArenaId,
    cache: Arc<crate::cache::ExtentCache>,
    slabs: Arc<crate::slabs::HostSlabs>,
    zeros: Arc<dyn Source>,
    translation: Translation,
    /// cladefs: shared with its connection's [`DaxWindow`] and the guest's faults, beyond what
    /// halts its cell: a panic holding it aborts the daemon (`local_cell::shared`, TASK-149.24).
    state: SharedLock<Mutex<WindowState>>,
    /// Lock-free mirror of `state.composed.len()` so relief avoids locking `state` when empty.
    composed_count: Arc<AtomicUsize>,
    /// cladefs: whether its second stage copies on write (backlog doc-13, doc-26), as a writer's
    /// does: the guest's `MAP` and `UNMAP` then end the private pages over the whole range they
    /// change ([`Private::End`]), which the window can't tell from the rest.
    copying: bool,
}

impl Window {
    /// A window of `size` bytes, which must be a multiple of the host page size, whose pages are
    /// mapped in `second_stage`, or, without one, in this process's memory; whose stage copies on
    /// write if `copying`, which a window in this process's memory has none to.
    #[allow(clippy::too_many_arguments)]
    fn new(
        cache: Arc<crate::cache::ExtentCache>,
        slabs: Arc<crate::slabs::HostSlabs>,
        arena: ArenaId,
        zeros: Arc<dyn Source>,
        size: u64,
        unit: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
        copying: bool,
    ) -> Result<Self> {
        let ps = page_size();
        if size == 0 || !size.is_multiple_of(ps) || unit == 0 || !ps.is_multiple_of(unit) {
            return Err(Errno::EINVAL);
        }
        let len = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
        if copying && second_stage.is_none() {
            return Err(Errno::EINVAL);
        }
        let translation = match second_stage {
            Some(stage) => Translation::Stage(stage),
            #[cfg(any(test, feature = "testing"))]
            None => Translation::Host { base: reserve(None, len).map_err(|_| Errno::ENOMEM)? },
            #[cfg(not(any(test, feature = "testing")))]
            None => {
                let _ = len;
                return Err(Errno::EINVAL);
            }
        };
        let composed_count = Arc::new(AtomicUsize::new(0));
        let state = SharedLock::new("window", Mutex::new(WindowState::new(composed_count.clone())));
        Ok(Self {
            size,
            unit,
            arena,
            cache,
            slabs,
            zeros,
            translation,
            state,
            composed_count,
            copying,
        })
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn stage(&self) -> Option<&Arc<dyn SecondStage>> {
        match &self.translation {
            Translation::Stage(stage) => Some(stage),
            #[cfg(any(test, feature = "testing"))]
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
        match &self.translation {
            #[cfg(any(test, feature = "testing"))]
            Translation::Host { base } => Some(*base),
            Translation::Stage(_) => None,
        }
    }

    fn is_mapped(&self, offset: u64) -> bool {
        self.state.with(Mutex::lock).mapping_at(offset).is_some()
    }

    /// How the host page at `h_start` (multiple of `page_size()`) is backed by `state.mappings`.
    fn host_page_plan(&self, state: &WindowState, h_start: u64) -> HostPagePlan {
        let ps = page_size();
        let h_end = h_start + ps;
        let starts = state.overlapping(h_start, h_end);
        if starts.is_empty() {
            return HostPagePlan::Unmapped;
        }
        if starts.len() == 1 {
            let m_start = starts[0];
            let mapping = &state.mappings[&m_start];
            if mapping.source.origin() == Origin::Zeros {
                return HostPagePlan::DirectZeros;
            }
            if m_start <= h_start && mapping.end >= h_end {
                let src_off = mapping.source_offset + (h_start - m_start);
                if src_off.is_multiple_of(ps) {
                    return HostPagePlan::Direct {
                        source: mapping.source.clone(),
                        src_page: src_off / ps,
                    };
                }
            }
        }
        let mut slices = Vec::with_capacity(starts.len());
        for m_start in starts {
            let mapping = &state.mappings[&m_start];
            if mapping.source.origin() == Origin::Zeros {
                continue;
            }
            let from = h_start.max(m_start);
            let to = h_end.min(mapping.end);
            slices.push(ComposeSlice {
                win_within: (from - h_start) as usize,
                len: (to - from) as usize,
                source: mapping.source.clone(),
                source_offset: mapping.source_offset + (from - m_start),
            });
        }
        if slices.is_empty() { HostPagePlan::DirectZeros } else { HostPagePlan::Composed(slices) }
    }

    /// Resolves the guest's fault at `offset` for `access`: a load's maps the host page
    /// ([`Window::resolve`]); a write's is refused, as every page is mapped read-only: cladefs: a
    /// stage that copies on write makes its own private copies (doc-26), and asks the window only
    /// to read.
    pub async fn fault(&self, offset: u64, access: Access) -> Result<()> {
        use crate::stats::{self, Counter};
        let ps = page_size();
        let h_start = offset - offset % ps;
        let start = std::time::Instant::now();
        let faulted = match access {
            Access::Read => self.resolve(h_start).await,
            Access::Write => {
                let has_any =
                    !self.state.with(Mutex::lock).overlapping(h_start, h_start + ps).is_empty();
                match has_any {
                    true => Err(Errno::EACCES),
                    false => Err(Errno::ENOENT),
                }
            }
        };
        stats::count_since(Counter::Faults, Counter::FaultNs, start);
        if faulted == Ok(true) {
            stats::add(Counter::FaultWaits, 1);
        }
        faulted.map(|_| ())
    }

    /// Maps the host page at `offset` (host-page-aligned), which a load's fault was on, bringing
    /// its source page(s) into memory first — and for a direct mapping, those its source brought
    /// in with it (`Source::brought_in_with`) — and returns whether it waited for a page-in. A
    /// page evicted or mapped anew meanwhile is resolved as it is then.
    async fn resolve(&self, offset: u64) -> Result<bool> {
        let ps = page_size();
        let mut waited = false;
        loop {
            let step = {
                let mut state = self.state.with(Mutex::lock);
                if state.has_pte(offset) {
                    return Ok(waited);
                }
                match self.host_page_plan(&state, offset) {
                    HostPagePlan::Unmapped => return Err(Errno::ENOENT),
                    HostPagePlan::DirectZeros => {
                        self.install(&mut state, offset, offset + ps);
                        if state.has_pte(offset) {
                            return Ok(waited);
                        }
                        continue;
                    }
                    HostPagePlan::Direct { source, src_page } => {
                        let (_, mapping) = state.mapping_at(offset).expect("direct mapping");
                        let left = (mapping.end - offset) / ps;
                        let mapped = left.min(source.brought_in_with(src_page));
                        ResolveStep::Direct { source, src_page, mapped }
                    }
                    HostPagePlan::Composed(slices) => {
                        match read_composed_slices(&slices, self.arena)? {
                            Some((buf, deps, arena)) => {
                                if let Ok(slab) = self.slabs.alloc(arena, 1, crate::slabs::Wait::No)
                                {
                                    self.install_composed(&mut state, offset, slab, &buf, deps)?;
                                    return Ok(waited);
                                }
                                ResolveStep::Compose { slices, ready: Some((buf, deps, arena)) }
                            }
                            None => ResolveStep::Compose { slices, ready: None },
                        }
                    }
                }
            };
            match step {
                ResolveStep::Direct { source, src_page, mapped } => {
                    waited |= source.clone().bring_in(src_page).await?;
                    let mut state = self.state.with(Mutex::lock);
                    if state.has_pte(offset) {
                        return Ok(waited);
                    }
                    let same = matches!(
                        self.host_page_plan(&state, offset),
                        HostPagePlan::Direct { source: ref now, src_page: now_page }
                            if source_identity(now) == source_identity(&source)
                                && now_page == src_page
                    );
                    if same {
                        self.install(&mut state, offset, offset + mapped * ps);
                        if state.has_pte(offset) {
                            return Ok(waited);
                        }
                    }
                }
                ResolveStep::Compose { slices, ready } => {
                    let (buf, deps, arena) = match ready {
                        Some(composed) => composed,
                        None => {
                            let (composed, slice_waited) =
                                assemble_composed_slices(&slices, self.arena).await?;
                            waited |= slice_waited;
                            composed
                        }
                    };
                    let slab = self.cache.alloc_composed(arena).await?;
                    let mut state = self.state.with(Mutex::lock);
                    if state.has_pte(offset) {
                        drop(state);
                        drop(slab);
                        return Ok(waited);
                    }
                    let same = matches!(
                        self.host_page_plan(&state, offset),
                        HostPagePlan::Composed(ref now) if same_compose_slices(&slices, now)
                    );
                    if same {
                        if slices.iter().any(|slice| slice.source.is_revoked()) {
                            drop(state);
                            drop(slab);
                            return Err(Errno::EACCES);
                        }
                        self.install_composed(&mut state, offset, slab, &buf, deps)?;
                        return Ok(waited);
                    }
                    drop(state);
                    drop(slab);
                }
            }
        }
    }

    /// Writes `buf` into `slab`, stages it to the VM process if any, records the composed host page
    /// at `offset` in `state`, and maps it in the second stage.
    fn install_composed(
        &self,
        state: &mut WindowState,
        offset: u64,
        slab: PartSlab,
        buf: &[u8],
        deps: Vec<(usize, PhysLocation)>,
    ) -> Result<()> {
        let ps = page_size();
        if let Err(err) = slab.write(0, buf) {
            log::error!("cladefs: writing composed DAX page at {offset:#x}: {err}");
            return Err(Errno::EIO);
        }
        if let Some(vm) = self.stage().and_then(|stage| stage.vm_process()) {
            slab.stage_to(vm);
        }
        let phys = slab.location();
        state.composed_clock += 1;
        let touch = state.composed_clock;
        state.insert_composed(offset, ComposedPage { slab, deps, touch });
        state.insert_run(offset, phys, ps, true);
        self.map_runs(&[(offset, phys, ps)]);
        Ok(())
    }

    /// Evicts least-recently-used composed host pages of `arena` (or any arena if `None`) until
    /// `enough(evicted)` returns true, unmapping them with [`Private::Keep`] and dropping their
    /// slabs only after the stage change is sent and the lock released. Returns bytes freed.
    pub fn evict_composed(
        &self,
        arena: Option<ArenaId>,
        mut enough: impl FnMut(u64) -> bool,
    ) -> u64 {
        if self.composed_count.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        let ps = page_size();
        let (evicted, dropped) = {
            let mut state = self.state.with(Mutex::lock);
            if state.composed.is_empty() {
                return 0;
            }
            let mut victims: Vec<(u64, u64)> = state
                .composed
                .iter()
                .filter(|(_, c)| arena.is_none_or(|a| c.slab.location().arena == a))
                .map(|(&offset, c)| (c.touch, offset))
                .collect();
            if victims.is_empty() {
                return 0;
            }
            victims.sort_unstable();
            let mut gone = Vec::new();
            let mut evicted = 0u64;
            for (_, offset) in victims {
                if enough(evicted) {
                    break;
                }
                gone.push(offset..offset + ps);
                evicted += ps;
            }
            if gone.is_empty() {
                return 0;
            }
            let gone = coalesce_ranges(gone);
            let _change = self.change();
            let mut dropped = Vec::with_capacity(gone.len());
            for range in &gone {
                let (_, slabs) = state.remove_pte_range(range.start, range.end);
                dropped.extend(slabs);
            }
            self.kept_ranges(&mut state, &gone);
            (evicted, dropped)
        };
        drop(dropped);
        evicted
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
        let dropped = {
            let mut state = self.state.with(Mutex::lock);
            let _change = self.change();
            self.map_locked(&mut state, offset, source, source_offset, len, private)
        };
        // Dropped after `_change` sends the stage unmaps and releases the lock, so quarantine parks
        // behind those unmaps.
        drop(dropped);
    }

    /// Maps as [`Window::map_as`] says, from `source`'s byte `source_offset`, with the window's
    /// lock held and its stage's change begun; returns composed slabs removed, to be dropped only
    /// once the stage change has been sent.
    fn map_locked(
        self: &Arc<Self>,
        state: &mut WindowState,
        offset: u64,
        source: &Arc<dyn Source>,
        source_offset: u64,
        len: u64,
        private: Private,
    ) -> Vec<PartSlab> {
        let ps = page_size();
        let end = offset + len;
        let (dropped, forgotten) = self.place(state, offset, source, source_offset, len, private);
        let (h_start, h_end) = (offset - offset % ps, end.next_multiple_of(ps));
        if let Some(stage) = self.stage() {
            stage.window_mapped(offset, len);
        }
        // Its pages in memory are mapped now, so that the guest's first accesses to them do not
        // fault. cladefs: but where a private page stays.
        self.install(state, h_start, h_end);
        self.unmapped(state, forgotten);
        dropped
    }

    /// cladefs: maps each of `pieces`, in order, as [`Window::map_as`] maps the guest's `MAP`, in
    /// one change of the second stage: the ranges mapped anew told it in runs of them consecutive
    /// in the window, then their pages in memory mapped, in runs across the pieces, where a `MAP`
    /// each made a change of its own, of a run at least (TASK-92.14). What a snapshot's window
    /// held is mapped again so, as its VM is restored.
    fn map_many<'a>(self: &Arc<Self>, pieces: impl IntoIterator<Item = Piece<'a>>) {
        let ps = page_size();
        let dropped = {
            let mut state = self.state.with(Mutex::lock);
            let _change = self.change();
            let mut mapped_runs: Vec<Range<u64>> = Vec::new();
            let mut anew: Vec<Range<u64>> = Vec::new();
            let mut dropped = Vec::new();
            for Piece { offset, source, source_offset, len } in pieces {
                let (removed, forgotten) =
                    self.place(&mut state, offset, source, source_offset, len, Private::End);
                dropped.extend(removed);
                self.unmapped(&state, forgotten);
                match mapped_runs.last_mut() {
                    Some(run) if offset >= run.start && offset <= run.end => {
                        run.end = run.end.max(offset + len);
                    }
                    _ => mapped_runs.push(offset..offset + len),
                }
                let h_start = offset - offset % ps;
                let h_end = (offset + len).next_multiple_of(ps);
                match anew.last_mut() {
                    Some(run) if h_start >= run.start && h_start <= run.end => {
                        run.end = run.end.max(h_end);
                    }
                    _ => anew.push(h_start..h_end),
                }
            }
            if let Some(stage) = self.stage() {
                for run in &mapped_runs {
                    stage.window_mapped(run.start, run.end - run.start);
                }
            }
            let mut runs = Vec::new();
            for run in &anew {
                self.present(&mut state, run.start, run.end, &mut runs);
            }
            self.map_runs(&runs);
            dropped
        };
        drop(dropped);
    }

    /// cladefs: puts the mapping of `len` bytes of `source` from `source_offset` at `offset` in
    /// the window's map, in place of what was mapped there and the pages it had there, the private
    /// pages among them going or staying as `private` says; and returns the composed slabs removed
    /// and the sources of the mappings it replaced, to tell of ([`Window::unmapped`]). The stage
    /// is told of nothing it maps anew, nor are its pages mapped, which is for its caller to do.
    fn place(
        self: &Arc<Self>,
        state: &mut WindowState,
        offset: u64,
        source: &Arc<dyn Source>,
        source_offset: u64,
        len: u64,
        private: Private,
    ) -> (Vec<PartSlab>, Vec<Arc<dyn Source>>) {
        let end = offset + len;
        // Whether the window mapped it before, whatever of it this replaces: the guest's MAP of
        // an extent over its own range tells it nothing (TASK-602).
        let first = !state.by_source.contains_key(&source_identity(source));
        let dropped = self.unmap_ptes(state, offset, end, private, false);
        let forgotten = state.forget(offset, end);
        state.insert(offset, Mapping { end, source: source.clone(), source_offset });
        if first {
            source.mapped(self);
        }
        (dropped, forgotten)
    }

    /// Maps zeros, `zeros`, read-only, at the window's ranges that map `source`, by its identity,
    /// which a freed extent's pages are, and returns those ranges, in order, as they were: one
    /// change to the second stage. The guest may still touch such a range, as a write racing the
    /// move of what it maps, which faults as on any read-only page; unmapped, its fault would find
    /// nothing to map. cladefs: the private pages there stay, the guest's, which only its own `MAP`
    /// or `UNMAP` ends (backlog doc-13).
    fn map_zeros_over(self: &Arc<Self>, source: usize, zeros: &Arc<dyn Source>) -> Vec<Range<u64>> {
        let (ranges, dropped) = {
            let mut state = self.state.with(Mutex::lock);
            let _change = self.change();
            let Some(starts) = state.by_source.get(&source) else { return Vec::new() };
            let ranges: Vec<Range<u64>> =
                starts.iter().map(|start| *start..state.mappings[start].end).collect();
            let mut dropped = Vec::new();
            for range in &ranges {
                let len = range.end - range.start;
                if rules().free_maps_zeros {
                    dropped.extend(self.map_locked(
                        &mut state,
                        range.start,
                        zeros,
                        0,
                        len,
                        Private::Keep,
                    ));
                } else {
                    dropped.extend(self.unmap_locked(&mut state, range.start, len, Private::Keep));
                }
            }
            (ranges, dropped)
        };
        drop(dropped);
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
                        MappingContents::Extent { extent, offset: mapping.source_offset }
                    }
                    Origin::Zeros => MappingContents::Zeros,
                },
            })
            .collect()
    }

    /// Unmaps `len` bytes of the window at `offset`: cladefs: the guest's `UNMAP`, which ends the
    /// private pages there.
    fn unmap(&self, offset: u64, len: u64) {
        let dropped = {
            let mut state = self.state.with(Mutex::lock);
            self.unmap_locked(&mut state, offset, len, Private::End)
        };
        drop(dropped);
    }

    /// cladefs: unmaps as [`Window::unmap`] does, with the window's lock held, its private pages
    /// there going or staying as `private` says.
    fn unmap_locked(
        &self,
        state: &mut WindowState,
        offset: u64,
        len: u64,
        private: Private,
    ) -> Vec<PartSlab> {
        let end = offset + len;
        let dropped = self.unmap_ptes(state, offset, end, private, true);
        let forgotten = state.forget(offset, end);
        self.unmapped(state, forgotten);
        dropped
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
    pub fn rebind(self: &Arc<Self>, from: &Arc<dyn Source>, to: Arc<dyn Source>) {
        let ps = page_size();
        let dropped = {
            let mut state = self.state.with(Mutex::lock);
            let Some(starts) = state.by_source.remove(&source_identity(from)) else { return };
            let mut gone = Vec::new();
            let mut dropped = Vec::new();
            for start in &starts {
                let end = state.mappings[start].end;
                let (h_start, h_end) = (start - start % ps, end.next_multiple_of(ps));
                let (removed, slabs) = state.remove_pte_range(h_start, h_end);
                gone.extend(removed);
                dropped.extend(slabs);
                state.mappings.get_mut(start).expect("a mapping's start").source = to.clone();
            }
            let gone = coalesce_ranges(gone);
            self.kept_ranges(&mut state, &gone);
            state.by_source.entry(source_identity(&to)).or_default().extend(starts);
            dropped
        };
        drop(dropped);
        from.unmapped(self);
        to.mapped(self);
    }

    /// Unmaps the pages the window has mapped of `source`, by its identity, that `evicted` says
    /// are of the runs it evicts, by where they are: before it lets go of them. cladefs: a private
    /// page within its mappings, its stage's, stays (backlog doc-13, spec rule
    /// evictionKeepsPrivate).
    pub fn unmap_pages_of(&self, source: usize, evicted: &dyn Fn(PhysLocation) -> bool) {
        let ps = page_size();
        let dropped = {
            let mut state = self.state.with(Mutex::lock);
            let Some(starts) = state.by_source.get(&source) else { return };
            let ranges: Vec<(u64, u64)> =
                starts.iter().map(|start| (*start, state.mappings[start].end)).collect();
            let mut gone: Vec<Range<u64>> = Vec::new();
            for (start, end) in ranges {
                let (h_start, h_end) = (start - start % ps, end.next_multiple_of(ps));
                let before = state
                    .ptes
                    .range(..h_start)
                    .next_back()
                    .filter(|&(r_start, run)| *r_start + run.len > h_start)
                    .map(|(&r_start, &run)| (r_start, run));
                let within =
                    state.ptes.range(h_start..h_end).map(|(&r_start, &run)| (r_start, run));
                for (r_start, run) in before.into_iter().chain(within) {
                    if run.composed {
                        if state.composed.get(&r_start).is_some_and(|c| {
                            c.deps.iter().any(|&(src, dep)| src == source && evicted(dep))
                        }) {
                            gone.push(r_start..r_start + ps);
                        }
                        continue;
                    }
                    let c_start = r_start.max(h_start);
                    let c_end = (r_start + run.len).min(h_end);
                    let mut cur = c_start;
                    while cur < c_end {
                        let phys = PhysLocation {
                            arena: run.arena,
                            offset: run.phys_offset + (cur - r_start),
                        };
                        if evicted(phys) {
                            match gone.last_mut() {
                                Some(last) if last.end == cur => last.end = cur + ps,
                                _ => gone.push(cur..cur + ps),
                            }
                        }
                        cur += ps;
                    }
                }
            }
            let gone = coalesce_ranges(gone);
            let mut dropped = Vec::new();
            for range in &gone {
                let (_, slabs) = state.remove_pte_range(range.start, range.end);
                dropped.extend(slabs);
            }
            self.kept_ranges(&mut state, &gone);
            dropped
        };
        drop(dropped);
    }

    /// cladefs: unmaps the host-page runs `ranges`, sorted and non-overlapping, which the window
    /// took away keeping the private pages there ([`Private::Keep`]), and, if its stage copies,
    /// records them so ([`WindowState::kept`]).
    fn kept_ranges(&self, state: &mut WindowState, ranges: &[Range<u64>]) {
        for range in ranges {
            if self.copying {
                state.keep_range(range.start, range.end);
            }
            self.unmap_range(range.start, range.end - range.start, Private::Keep);
        }
    }

    /// Maps, where nothing is mapped, the host pages in memory of the mappings of `start..end`
    /// (host-page-aligned): in runs of pages consecutive in the window and in an arena's object,
    /// each one change of the second stage.
    fn install(&self, state: &mut WindowState, start: u64, end: u64) {
        let mut runs = Vec::new();
        self.present(state, start, end, &mut runs);
        self.map_runs(&runs);
    }

    /// Adds to `runs`, in order, the contiguous host-page runs in memory of the mappings of
    /// `start..end` (host-page-aligned) where nothing is mapped, each `(window_offset, phys, len)`,
    /// recording them as mapped from now on for [`Window::map_runs`].
    fn present(
        &self,
        state: &mut WindowState,
        start: u64,
        end: u64,
        runs: &mut Vec<(u64, PhysLocation, u64)>,
    ) {
        let ps = page_size();
        let vm = self.stage().and_then(|stage| stage.vm_process());
        if self.unit == ps {
            for mapping_start in state.overlapping(start, end) {
                let mapping = &state.mappings[&mapping_start];
                let (from, to) = (start.max(mapping_start), end.min(mapping.end));
                let first = (mapping.source_offset + (from - mapping_start)) / ps;
                let mut found: Vec<(u64, PhysLocation, u64)> = Vec::new();
                mapping.source.each_present(first..first + (to - from) / ps, &mut |present| {
                    let run_start = from + (present.page - first) * ps;
                    let run_end = run_start + present.count * ps;
                    let gaps = state.uncovered_runs(run_start, run_end);
                    if gaps.is_empty() {
                        return;
                    }
                    for gap in gaps {
                        let within = gap.start - run_start;
                        let phys =
                            PhysLocation { offset: present.at.offset + within, ..present.at };
                        found.push((gap.start, phys, gap.end - gap.start));
                    }
                    // Once freed, it waits for the VM process to take it away
                    // (`slabs::quarantine_with`, TASK-79.9, doc-26).
                    if let (Some(vm), Some(slab)) = (vm, present.slab) {
                        slab.stage_to(vm);
                    }
                });
                for (offset, at, len) in found {
                    state.insert_run(offset, at, len, false);
                    runs.push((offset, at, len));
                }
            }
            return;
        }
        let h_start = start - start % ps;
        let h_end = end.next_multiple_of(ps);
        let mut h_page = h_start;
        while h_page < h_end {
            if state.has_pte(h_page) {
                h_page += ps;
                continue;
            }
            match self.host_page_plan(state, h_page) {
                HostPagePlan::Unmapped => h_page += ps,
                HostPagePlan::DirectZeros => {
                    let mut at = None;
                    self.zeros.each_present(0..1, &mut |present| at = Some(present.at));
                    if let Some(at) = at {
                        state.insert_run(h_page, at, ps, false);
                        runs.push((h_page, at, ps));
                    }
                    h_page += ps;
                }
                HostPagePlan::Direct { source, src_page } => {
                    // Batch consecutive aligned host pages of the same mapping in one lookup.
                    let (_, mapping) = state.mapping_at(h_page).expect("direct mapping");
                    let limit = mapping.end.min(h_end);
                    let run_pages = (limit - h_page) / ps;
                    let mut found: Vec<(u64, PhysLocation, u64)> = Vec::new();
                    source.each_present(src_page..src_page + run_pages, &mut |present| {
                        let run_start = h_page + (present.page - src_page) * ps;
                        let run_end = run_start + present.count * ps;
                        let gaps = state.uncovered_runs(run_start, run_end);
                        if gaps.is_empty() {
                            return;
                        }
                        for gap in gaps {
                            let within = gap.start - run_start;
                            let phys =
                                PhysLocation { offset: present.at.offset + within, ..present.at };
                            found.push((gap.start, phys, gap.end - gap.start));
                        }
                        if let (Some(vm), Some(slab)) = (vm, present.slab) {
                            slab.stage_to(vm);
                        }
                    });
                    for (offset, at, len) in found {
                        state.insert_run(offset, at, len, false);
                        runs.push((offset, at, len));
                    }
                    h_page += run_pages * ps;
                }
                HostPagePlan::Composed(slices) => {
                    // Best-effort eager composition when all source pages are in memory and a
                    // budgeted slab is immediately available; otherwise resolved lazily on fault.
                    if let Ok(Some((buf, deps, arena))) = read_composed_slices(&slices, self.arena)
                        && let Ok(slab) = self.slabs.alloc(arena, 1, crate::slabs::Wait::No)
                        && slab.write(0, &buf).is_ok()
                    {
                        if let Some(vm) = vm {
                            slab.stage_to(vm);
                        }
                        let phys = slab.location();
                        state.composed_clock += 1;
                        let touch = state.composed_clock;
                        state.insert_composed(h_page, ComposedPage { slab, deps, touch });
                        state.insert_run(h_page, phys, ps, true);
                        runs.push((h_page, phys, ps));
                    }
                    h_page += ps;
                }
            }
        }
    }

    /// Maps `runs`, as [`Window::present`] found them, in order: coalescing runs consecutive in
    /// the window and in an arena's object, each one change of the second stage.
    fn map_runs(&self, runs: &[(u64, PhysLocation, u64)]) {
        // cladefs: the runs of pages consecutive in the window, which the maps below are made of
        // (`stage_segments`, TASK-131.1).
        if self.stage().is_some() && !runs.is_empty() {
            let segments =
                1 + runs.windows(2).filter(|pair| pair[0].0 + pair[0].2 != pair[1].0).count();
            crate::stats::add(crate::stats::Counter::StageSegments, segments as u64);
        }
        let mut current: Option<(u64, PhysLocation, u64)> = None;
        for &(offset, location, run_len) in runs {
            match &mut current {
                Some((at, first, len))
                    if *at + *len == offset
                        && first.arena == location.arena
                        && first.offset + *len == location.offset =>
                {
                    *len += run_len;
                }
                _ => {
                    if let Some((at, first, len)) = current.replace((offset, location, run_len)) {
                        self.map_run(at, first, len);
                    }
                }
            }
        }
        if let Some((at, first, len)) = current {
            self.map_run(at, first, len);
        }
    }

    /// Unmaps the host pages the window has mapped overlapping `start..end` (logical-unit aligned).
    /// cladefs: its stage's private pages there go or stay as `private` says (doc-26). A private
    /// page is at a page the window mapped, or one the host's changes took away keeping it
    /// ([`WindowState::kept`]): the guest's changes ([`Private::End`]) unmap those too, which the
    /// window maps nothing at, clipped to `start..end` so sibling sub-pages in a larger host page
    /// keep their private data. Returns composed slabs removed, to be dropped after sending unmaps.
    fn unmap_ptes(
        &self,
        state: &mut WindowState,
        start: u64,
        end: u64,
        private: Private,
        clear_cold_mapped: bool,
    ) -> Vec<PartSlab> {
        let ps = page_size();
        let (h_start, h_end) = (start - start % ps, end.next_multiple_of(ps));
        let (gone, dropped) = state.remove_pte_range(h_start, h_end);
        match private {
            Private::Keep => self.kept_ranges(state, &gone),
            Private::End if self.copying && rules().guests_changes_end_private => {
                let mut runs: Vec<Range<u64>> = state.take_kept(start, end);
                for range in &gone {
                    if range.start < start {
                        state.keep_range(range.start, start);
                    }
                    if range.end > end {
                        state.keep_range(end, range.end);
                    }
                    runs.push(range.start.max(start)..range.end.min(end));
                }
                // On guest UNMAP of a copying window, also clear logical presence (`mapped_mask`)
                // for never-resident cold mappings so removed sub-page slots cannot later be made
                // private alongside a warm sibling slot (review 102 finding 348).
                if clear_cold_mapped {
                    for m_start in state.overlapping(start, end) {
                        let m_end = state.mappings[&m_start].end;
                        runs.push(m_start.max(start)..m_end.min(end));
                    }
                }
                for run in coalesce_ranges(runs) {
                    self.unmap_range(run.start, run.end - run.start, private);
                }
            }
            Private::End => {
                for range in &gone {
                    self.unmap_range(range.start, range.end - range.start, private);
                }
            }
        }
        dropped
    }

    /// Unmaps the `len` bytes at `start`, the private pages there going or staying as `private`
    /// says.
    fn unmap_range(&self, start: u64, len: u64, private: Private) {
        match &self.translation {
            Translation::Stage(stage) => stage.unmap(start, len, private),
            #[cfg(any(test, feature = "testing"))]
            Translation::Host { base } => {
                let ps = page_size();
                let h_start = start - start % ps;
                let h_end = (start + len).next_multiple_of(ps);
                let at = base + h_start as usize;
                reserve(Some(at), (h_end - h_start) as usize)
                    .expect("the window's memory reserved again");
            }
        }
    }

    /// Maps the `len` bytes at `offset` to the pages at `location`, read-only.
    fn map_run(&self, offset: u64, location: PhysLocation, len: u64) {
        match &self.translation {
            Translation::Stage(stage) => stage.map(offset, location, len),
            #[cfg(any(test, feature = "testing"))]
            Translation::Host { base } => {
                let fd = self.slabs.arena_fd(location.arena).expect("the page's arena");
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

    /// Checks that `len` bytes at `offset` are aligned to the window's logical unit and within
    /// the window.
    fn check_range(&self, offset: u64, len: u64) -> Result<()> {
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if len == 0
            || !offset.is_multiple_of(self.unit)
            || !len.is_multiple_of(self.unit)
            || end > self.size()
        {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }
}

/// Next async action for [`Window::resolve`].
enum ResolveStep {
    Direct { source: Arc<dyn Source>, src_page: u64, mapped: u64 },
    Compose { slices: Vec<ComposeSlice>, ready: Option<ComposedBytes> },
}

impl Drop for Window {
    fn drop(&mut self) {
        // Notify mapped sources that this window is gone so `ExtentCache`'s eviction index
        // demotes their extents from mapped to unmapped immediately (board 12315).
        let sources: Vec<Arc<dyn Source>> = {
            let state = self.state.get_mut().get_mut().unwrap_or_else(|err| err.into_inner());
            let mut seen = BTreeSet::new();
            let mut sources = Vec::new();
            for mapping in std::mem::take(&mut state.mappings).into_values() {
                if seen.insert(source_identity(&mapping.source)) {
                    sources.push(mapping.source);
                }
            }
            state.by_source.clear();
            sources
        };
        for source in sources {
            source.unmapped(self);
        }
        #[cfg(any(test, feature = "testing"))]
        if let Translation::Host { base } = self.translation {
            // SAFETY: The reservation is the window's own.
            let status = unsafe { libc::munmap(base as *mut libc::c_void, self.size as usize) };
            debug_assert_eq!(status, 0, "unreserve a window: {}", std::io::Error::last_os_error());
        }
    }
}

/// Reserves `len` bytes of host address space, at `at` if given, over what is there, inaccessible
/// until pages are mapped there.
#[cfg(any(test, feature = "testing"))]
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
pub struct Rules {
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
pub struct Dax {
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
    /// A window of `size` bytes on `host` (owned by `arena`) whose pages are mapped in
    /// `second_stage`, or, without one, in this process's memory, and a handle to it.
    pub fn new_on(
        host: &crate::Host,
        arena: ArenaId,
        size: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
    ) -> Result<(Self, DaxWindow)> {
        Self::with_stage_on(host, arena, size, second_stage, false)
    }

    /// A window of `size` bytes whose pages are mapped in `second_stage`, or, without one, in this
    /// process's memory, and a handle to it.
    #[cfg(all(test, not(madsim)))]
    pub fn new(size: u64, second_stage: Option<Arc<dyn SecondStage>>) -> Result<(Self, DaxWindow)> {
        Self::new_on(crate::Host::process(), ArenaId::DEFAULT, size, second_stage)
    }

    /// cladefs: as [`Dax::new_on`], a window whose `second_stage` copies on write.
    pub fn copying_on(
        host: &crate::Host,
        arena: ArenaId,
        size: u64,
        second_stage: Arc<dyn SecondStage>,
    ) -> Result<(Self, DaxWindow)> {
        Self::with_stage_on(host, arena, size, Some(second_stage), true)
    }

    /// cladefs: as [`Dax::new`], a window whose `second_stage` copies on write (backlog doc-13,
    /// doc-26): a writer's.
    #[cfg(all(test, not(madsim)))]
    pub fn copying(size: u64, second_stage: Arc<dyn SecondStage>) -> Result<(Self, DaxWindow)> {
        Self::copying_on(crate::Host::process(), ArenaId::DEFAULT, size, second_stage)
    }

    fn with_stage_on(
        host: &crate::Host,
        arena: ArenaId,
        size: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
        copying: bool,
    ) -> Result<(Self, DaxWindow)> {
        Self::with_stage_and_unit_on(host, arena, size, map_unit(), second_stage, copying)
    }

    pub fn with_stage_and_unit_on(
        host: &crate::Host,
        arena: ArenaId,
        size: u64,
        unit: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
        copying: bool,
    ) -> Result<(Self, DaxWindow)> {
        let zeros = host.cache().zeros()?;
        let window = Arc::new(Window::new(
            Arc::clone(host.cache()),
            Arc::clone(host.slabs()),
            arena,
            zeros.clone(),
            size,
            unit,
            second_stage,
            copying,
        )?);
        host.cache().register_window(&window);
        let dax = Self {
            window: window.clone(),
            zeros,
            #[cfg(all(test, target_os = "linux"))]
            _faults: None,
        };
        Ok((dax, DaxWindow(window)))
    }

    #[cfg(all(test, not(madsim)))]
    pub fn new_with_unit(
        size: u64,
        unit: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
    ) -> Result<(Self, DaxWindow)> {
        Self::with_stage_and_unit_on(
            crate::Host::process(),
            ArenaId::DEFAULT,
            size,
            unit,
            second_stage,
            false,
        )
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn in_host_memory_copying_with_unit(
        stage: Arc<uffd::UffdSecondStage>,
        unit: u64,
        most: Option<u64>,
    ) -> crate::error::Result<(Self, DaxWindow), crate::Error> {
        if let Some(most) = most {
            stage.copy_on_write(most)?;
        }
        let copying = most.is_some();
        let (mut dax, window) = Self::with_stage_and_unit_on(
            crate::Host::process(),
            ArenaId::DEFAULT,
            stage.size(),
            unit,
            Some(stage.clone()),
            copying,
        )?;
        dax._faults = Some(stage.serve(dax.window.clone())?);
        Ok((dax, window))
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
        let (mut dax, window) = Self::with_stage_on(
            crate::Host::process(),
            ArenaId::DEFAULT,
            stage.size(),
            Some(stage.clone()),
            copying,
        )?;
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
        let zeros = &self.zeros;
        let pieces = mappings.iter().map(|DaxMapping { offset, len, source }| {
            let (source, source_offset) = match source {
                Some((source, from)) => (source, *from),
                None => (zeros, 0),
            };
            Piece { offset: *offset, len: *len, source, source_offset }
        });
        self.window.map_many(pieces);
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
pub struct WindowChanges {
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
pub enum WindowChangeKind {
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
    pub const MOST: usize = 256;

    /// The most refused pages awaiting a change ([`WindowChanges::refused`]): the oldest go first.
    const MOST_REFUSED: usize = 1024;

    pub fn has_refused(&self) -> bool {
        !self.refused.is_empty()
    }

    pub fn record(&mut self, window_offset: u64, len: u64, change: WindowChangeKind) {
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
    pub fn refuse(&mut self, window_offset: u64, what: &str, mapped: String) {
        if self.refused.len() == Self::MOST_REFUSED {
            self.refused.remove(0);
        }
        let at = std::time::Instant::now();
        self.refused.push(Refused { at, window_offset, what: what.to_owned(), mapped });
    }

    /// The changes of the page at `window_offset`, oldest first, with how long ago each was.
    pub fn of(&self, window_offset: u64) -> Vec<String> {
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
pub mod tests {
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
    pub struct TestSource {
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
    pub fn read_page(at: PhysLocation) -> Vec<u8> {
        use std::os::unix::fs::FileExt;
        let fd = crate::slabs::arena_fd(at.arena).expect("the page's arena");
        let file = std::fs::File::from(fd.try_clone().expect("the arena's object, again"));
        let mut bytes = vec![0; page_size() as usize];
        file.read_exact_at(&mut bytes, at.offset).expect("the page read");
        bytes
    }

    /// A mapping of zeros, `len` bytes at `window_offset`, as [`Dax::mappings`] records it.
    pub fn zeros_at(window_offset: u64, len: u64) -> WindowMapping {
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
    pub struct Recorded {
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

    /// Ranges of a window are logical-unit aligned, non-empty, and within it.
    #[test]
    fn ranges_are_whole_pages_within_the_window() {
        let ps = page_size();
        let unit = map_unit();
        let (dax, window) = Dax::new(4 * ps, None).unwrap();
        assert_eq!(window.size(), 4 * ps);
        assert_eq!(dax.check_range(0, 4 * ps), Ok(()));
        assert_eq!(dax.check_range(3 * ps, ps), Ok(()));
        assert_eq!(dax.check_range(unit, unit), Ok(()));
        for (offset, len) in [
            (0, 0),
            (1, unit),
            (0, unit + 1),
            (ps, 4 * ps),
            (4 * ps, unit),
            (u64::MAX - unit + 1, unit),
        ] {
            assert_eq!(dax.check_range(offset, len), Err(Errno::EINVAL), "{offset:#x}+{len:#x}");
        }
        assert_eq!(Dax::new(ps + 1, None).err(), Some(Errno::EINVAL));
        assert_eq!(Dax::new(0, None).err(), Some(Errno::EINVAL));
    }

    /// Sub-host-page mappings (e.g. 4 KiB blocks on a 16 KiB arm64 host, exercised on every host
    /// with `unit = page_size() / 4`): unaligned source offsets, mixed sources + zeros + unmapped
    /// slots, eviction, rebind, and free->zeros all produce the exact composed host page bytes
    /// while preserving host-page-aligned zero-copy mappings and exact logical `window_mappings`.
    #[test]
    fn sub_host_page_mappings_compose_evict_rebind_and_free() {
        let ps = page_size();
        let unit = ps / 4;
        // Source A: 2 host pages, each sub-unit tagged with a distinct byte.
        let make_source = |tags: &[[u8; 4]]| {
            let pages: Vec<Option<PartSlab>> = tags
                .iter()
                .map(|page_tags| {
                    let slab = crate::slabs::alloc(
                        crate::slabs::ArenaId::DEFAULT,
                        1,
                        crate::slabs::Wait::No,
                    )
                    .unwrap();
                    let mut bytes = vec![0u8; ps as usize];
                    for (slot, &tag) in page_tags.iter().enumerate() {
                        bytes[slot * unit as usize..(slot + 1) * unit as usize].fill(tag);
                    }
                    slab.write(0, &bytes).unwrap();
                    Some(slab)
                })
                .collect();
            TestSource::new(Kind::Filled, pages)
        };
        let a = make_source(&[[0x10, 0x11, 0x12, 0x13], [0x14, 0x15, 0x16, 0x17]]);
        let b = make_source(&[[0x20, 0x21, 0x22, 0x23]]);
        let (mut dax, window) = Dax::new_with_unit(3 * ps, unit, None).unwrap();

        // Host page 0: [A@unit (0x11), Zeros (0), B@2*unit (0x22), unmapped (0)].
        dax.map(0, &a.source(), unit, unit);
        dax.map_zeros(unit, unit);
        dax.map(2 * unit, &b.source(), 2 * unit, unit);
        // Host page 1: unaligned 1-host-page span crossing A's page 0 -> page 1 (0x12, 0x13, 0x14, 0x15).
        dax.map(ps, &a.source(), 2 * unit, ps);
        // Host page 2: aligned direct zero-copy mapping of B page 0 (0x20, 0x21, 0x22, 0x23).
        dax.map(2 * ps, &b.source(), 0, ps);

        let read_slots = |host_page: u64| -> [u8; 4] {
            futures::executor::block_on(window.fault(host_page * ps, Access::Read)).unwrap();
            let mut out = [0u8; 4];
            for (slot, byte) in out.iter_mut().enumerate() {
                // SAFETY: `host_page` is mapped in `window`'s host memory.
                *byte = unsafe {
                    *window.host_base().add((host_page * ps + slot as u64 * unit) as usize)
                };
            }
            out
        };
        assert_eq!(read_slots(0), [0x11, 0x00, 0x22, 0x00]);
        assert_eq!(read_slots(1), [0x12, 0x13, 0x14, 0x15]);
        assert_eq!(read_slots(2), [0x20, 0x21, 0x22, 0x23]);

        // Logical mappings preserve exact sub-page offsets and lengths (for VM snapshots).
        assert_eq!(
            dax.mappings(),
            vec![
                WindowMapping {
                    window_offset: 0,
                    len: unit,
                    contents: MappingContents::Extent { extent: a.extent, offset: unit },
                },
                zeros_at(unit, unit),
                WindowMapping {
                    window_offset: 2 * unit,
                    len: unit,
                    contents: MappingContents::Extent { extent: b.extent, offset: 2 * unit },
                },
                WindowMapping {
                    window_offset: ps,
                    len: ps,
                    contents: MappingContents::Extent { extent: a.extent, offset: 2 * unit },
                },
                WindowMapping {
                    window_offset: 2 * ps,
                    len: ps,
                    contents: MappingContents::Extent { extent: b.extent, offset: 0 },
                },
            ]
        );

        // Host page 2 is directly mapped (no composed slab), while 0 and 1 are composed.
        {
            let state = dax.window.state.with(Mutex::lock);
            assert!(state.composed.contains_key(&0));
            assert!(state.composed.contains_key(&ps));
            assert!(!state.composed.contains_key(&(2 * ps)));
        }

        // Rebind B -> C: both composed page 0 and direct page 2 unmap and read C on next fault.
        let c = make_source(&[[0x30, 0x31, 0x32, 0x33]]);
        dax.window.rebind(&b.source(), c.source());
        assert_eq!(read_slots(0), [0x11, 0x00, 0x32, 0x00]);
        assert_eq!(read_slots(2), [0x30, 0x31, 0x32, 0x33]);

        // Evict A's page 1: composed page 1 (which spans A's page 0 and 1) is unmapped, while
        // composed page 0 (which only depends on A's page 0 and C's page 0) stays mapped.
        let a_page1 = a.location(1);
        dax.window.unmap_pages_of(source_identity(&a), &|at| at == a_page1);
        {
            let state = dax.window.state.with(Mutex::lock);
            assert!(state.has_pte(0));
            assert!(!state.has_pte(ps));
            assert!(!state.composed.contains_key(&ps));
        }

        // Free A -> zeros: page 0's slot 0 becomes 0 while C's slot 2 remains 0x32; page 1 is all 0.
        assert_eq!(dax.map_zeros_over(&a), vec![0..unit, ps..2 * ps]);
        assert_eq!(read_slots(0), [0x00, 0x00, 0x32, 0x00]);
        assert_eq!(read_slots(1), [0x00, 0x00, 0x00, 0x00]);
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

    /// Review 100 finding 345: composed DAX pages built from public-base extents in
    /// `ArenaId::DEFAULT` inside a tenant-owned window must charge the tenant's own arena (never
    /// bypassing tenant memory caps via `ArenaId::DEFAULT`) and be evicted by arena relief when
    /// the tenant's cap is reached.
    #[test]
    fn composed_pages_from_default_arena_charge_and_evict_in_window_arena() {
        let ps = page_size();
        let unit = ps / 4;
        let host = crate::Host::process();
        host.cache().relieve_pressure();
        let tenant = crate::pages::Arena::new_on(host).unwrap();
        let tenant_id = tenant.id();
        // Cap the tenant arena to 4 host pages.
        host.slabs().set_arena_cap(tenant_id, Some(4 * ps));

        let public_src = TestSource::filled(1, 0x5a).source();

        // Tenant window maps 1 sub-page of the public (`ArenaId::DEFAULT`) source at 10 distinct
        // host-page offsets (requiring 10 composed slabs > 4-page tenant cap).
        let (mut dax, window) =
            Dax::with_stage_and_unit_on(host, tenant_id, 10 * ps, unit, None, false).unwrap();
        // Snapshot default-arena usage after window creation (which lazily initializes the shared
        // 1-page `Zeros` slab in `ArenaId::DEFAULT`).
        let default_after_window = host.slabs().arena_bytes_in_use(ArenaId::DEFAULT);
        for p in 0..10u64 {
            dax.map(p * ps, &public_src, 0, unit);
        }
        for p in 0..10u64 {
            futures::executor::block_on(window.fault(p * ps, Access::Read)).unwrap();
            // SAFETY: `p * ps` was just faulted in `window`'s host memory.
            let got = unsafe { *window.host_base().add((p * ps) as usize) };
            assert_eq!(got, 0x5a, "host page {p}");
        }
        // Default arena must not have absorbed any composed slabs; tenant arena stayed within cap.
        assert_eq!(
            host.slabs().arena_bytes_in_use(ArenaId::DEFAULT),
            default_after_window,
            "composed slabs leaked into ArenaId::DEFAULT"
        );
        assert!(
            host.slabs().arena_bytes_in_use(tenant_id) <= 4 * ps,
            "tenant arena exceeded cap: {} > {}",
            host.slabs().arena_bytes_in_use(tenant_id),
            4 * ps
        );
        assert!(
            host.slabs().arena_bytes_in_use(tenant_id) > 0,
            "composed slabs were not charged to tenant arena"
        );
        host.slabs().set_arena_cap(tenant_id, None);
        drop((dax, window, public_src, tenant));
    }

    /// Review 100 finding 346 (board 5662): when a governed host's total memory cap is transiently
    /// full (`total_is_the_hosts == true`), faulting a composed DAX page waits for memory to return
    /// rather than failing immediately with `ENOMEM` (which would abort the VM process).
    #[test]
    fn composed_fault_waits_when_host_total_memory_is_full() {
        let ps = page_size();
        let unit = ps / 4;
        let host = crate::Host::process();
        host.cache().relieve_pressure();
        host.cache().set_total_is_the_hosts(true);
        let src = TestSource::filled(1, 0x3c).source();
        let (mut dax, window) =
            Dax::with_stage_and_unit_on(host, ArenaId::DEFAULT, 2 * ps, unit, None, false).unwrap();
        let blocker = host.slabs().alloc(ArenaId::DEFAULT, 1, crate::slabs::Wait::No).unwrap();
        let used = host.slabs().arenas_bytes();
        // Cap total memory at exactly `used`, so allocating 1 composed page blocks until `blocker`
        // is dropped by another thread.
        host.slabs().set_total_cap(Some(used));
        dax.map(0, &src, 0, unit);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(blocker);
        });
        futures::executor::block_on(window.fault(0, Access::Read))
            .expect("composed fault must wait for transient host memory instead of failing ENOMEM");
        releaser.join().unwrap();
        // SAFETY: Host page 0 was just faulted in `window`'s host memory.
        assert_eq!(unsafe { *window.host_base() }, 0x3c);
        host.slabs().set_total_cap(None);
        host.cache().set_total_is_the_hosts(false);
        drop((dax, window, src));
    }

    /// Board 11697: `WindowState::ptes` stores contiguous staged runs rather than one entry per
    /// host page, coalescing physically adjacent runs, splitting runs on partial unmap/eviction,
    /// and preserving `kept` ranges for copy-on-write private pages across run splits.
    #[test]
    fn slab_runs_coalesce_split_and_preserve_kept_ranges() {
        struct ContiguousSource {
            slab: PartSlab,
            pages: u64,
        }
        impl Source for ContiguousSource {
            fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(Present<'_>)) {
                let start = pages.start.min(self.pages);
                let end = pages.end.min(self.pages);
                if start < end {
                    let at = PhysLocation {
                        arena: self.slab.location().arena,
                        offset: self.slab.location().offset + start * page_size(),
                    };
                    found(Present { page: start, count: end - start, at, slab: Some(&self.slab) });
                }
            }
            fn bring_in(self: Arc<Self>, _: u64) -> BoxFuture<'static, Result<bool>> {
                Box::pin(std::future::ready(Ok(false)))
            }
            fn origin(&self) -> Origin {
                Origin::Extent(ExtentId { cell: 99, sequence: 1 })
            }
        }
        let ps = page_size();
        let slab = crate::slabs::alloc(ArenaId::DEFAULT, 8, crate::slabs::Wait::No).unwrap();
        let p0 = slab.location().offset;
        let src: Arc<dyn Source> = Arc::new(ContiguousSource { slab, pages: 8 });
        let stage = Arc::new(Recorded::default());
        let (mut dax, _window) = Dax::copying(8 * ps, stage.clone()).unwrap();
        dax.map(0, &src, 0, 8 * ps);
        // Contiguous source pages collapse into a single StagedRun.
        {
            let state = dax.window.state.with(Mutex::lock);
            assert_eq!(state.ptes.len(), 1, "8 contiguous pages should coalesce into 1 run");
            let (&start, run) = state.ptes.iter().next().unwrap();
            assert_eq!((start, run.len, run.composed), (0, 8 * ps, false));
            assert_eq!(state.gauges.live().pages, 8);
        }
        let _ = stage.unmaps();
        // Partial eviction in the middle splits the run into [0..2*ps) and [5*ps..8*ps) and keeps
        // private pages over [2*ps..5*ps).
        dax.window.unmap_pages_of(source_identity(&src), &|at| {
            let rel = at.offset.saturating_sub(p0);
            rel >= 2 * ps && rel < 5 * ps
        });
        assert_eq!(stage.unmaps(), [(2, 3, Private::Keep)]);
        {
            let state = dax.window.state.with(Mutex::lock);
            let runs: Vec<(u64, u64)> = state.ptes.iter().map(|(&s, r)| (s, r.len)).collect();
            assert_eq!(runs, [(0, 2 * ps), (5 * ps, 3 * ps)]);
            assert_eq!(state.gauges.live().pages, 5);
            assert_eq!(
                state.kept.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(),
                [(2 * ps, 5 * ps)]
            );
        }
        // Unmapping across part of the kept range ends those private pages while splitting the run.
        dax.unmap(ps, 3 * ps);
        assert_eq!(stage.unmaps(), [(1, 3, Private::End)]);
        {
            let state = dax.window.state.with(Mutex::lock);
            let runs: Vec<(u64, u64)> = state.ptes.iter().map(|(&s, r)| (s, r.len)).collect();
            assert_eq!(runs, [(0, ps), (5 * ps, 3 * ps)]);
            assert_eq!(state.gauges.live().pages, 4);
            assert_eq!(
                state.kept.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(),
                [(4 * ps, 5 * ps)]
            );
        }
    }
}
