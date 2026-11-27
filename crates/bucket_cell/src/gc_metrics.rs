// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Composable, streaming GC metrics and aggregation mini-framework.
//!
//! During a collector sweep (`Collector`, `Sweep`, `cladefs-gc`, `gc_wasm`), each cell pass emits
//! a [`CellSample`] constructed solely from metadata the collector already fetched (zero extra
//! bucket requests). Callers plug any [`Metric`] (or multiple metrics via [`FanOut`]) into
//! [`Collector::observer`](crate::collector::Collector::observer) or
//! [`Limits::observer`](crate::sweep::Limits::observer).
//!
//! # Writing a custom metric
//!
//! Implement [`Metric`] (just `observe` + associative `merge`), or compose directly from the
//! built-in primitives [`Stats`], [`Histogram`], and [`GroupBy`]:
//!
//! ```ignore
//! #[derive(Default)]
//! struct LargeSegmentCount(u64);
//!
//! impl Merge for LargeSegmentCount {
//!     fn merge(&mut self, other: Self) { self.0 += other.0; }
//! }
//!
//! impl Metric for LargeSegmentCount {
//!     fn observe(&mut self, cell: &CellSample<'_>) {
//!         for obj in cell.kept {
//!             if obj.kind == "segment" && obj.size >= (16 << 20) {
//!                 self.0 += 1;
//!             }
//!         }
//!     }
//! }
//!
//! let metric = Arc::new(Shared::<LargeSegmentCount>::default());
//! let collector = Collector::new().observer(Some(metric.clone()));
//! ```

use crate::layout::{self, CellId};
use crate::passed::Sum;
use base32_id::Short;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

/// Maximum sample cell IDs retained per lagging format version in [`FormatStats::lagging_heads`].
pub const MAX_LAGGING_SAMPLES: usize = 64;

/// Default byte-size upper bounds for individual object histograms (1 KiB .. 1 GiB).
pub const OBJECT_SIZE_BOUNDS: &[u64] = &[
    1 << 10,   // 1 KiB
    4 << 10,   // 4 KiB
    16 << 10,  // 16 KiB
    64 << 10,  // 64 KiB
    256 << 10, // 256 KiB
    1 << 20,   // 1 MiB
    4 << 20,   // 4 MiB
    16 << 20,  // 16 MiB
    64 << 20,  // 64 MiB
    256 << 20, // 256 MiB
    1 << 30,   // 1 GiB
];

/// Default byte-size upper bounds for total surviving cell size histograms (4 KiB .. 1 TiB).
pub const CELL_SIZE_BOUNDS: &[u64] = &[
    4 << 10,   // 4 KiB
    64 << 10,  // 64 KiB
    1 << 20,   // 1 MiB
    16 << 20,  // 16 MiB
    256 << 20, // 256 MiB
    1 << 30,   // 1 GiB
    16 << 30,  // 16 GiB
    256 << 30, // 256 GiB
    1 << 40,   // 1 TiB
];

/// Default upper bounds for surviving object counts per cell.
pub const CELL_OBJECT_BOUNDS: &[u64] = &[1, 4, 16, 64, 256, 1_024, 4_096, 16_384, 65_536];

/// One surviving or deleted object observed during a cell pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectSample {
    /// Object kind name ([`layout::Kind::name`], e.g. `"segment"`, `"checkpoint"`, `"head"`).
    pub kind: &'static str,
    /// Object byte length as listed by the bucket.
    pub size: u64,
    /// Object last-modified timestamp in milliseconds (by the backend's clock).
    pub modified: u64,
}

/// Summary of one cell pass fed to every registered [`Observer`].
///
/// Built from objects and metadata the collector already read during its pass (zero extra I/O).
#[derive(Clone, Copy, Debug)]
pub struct CellSample<'a> {
    pub cell: CellId,
    /// True when this pass ran over a mirrored foreign-bucket copy (`Collector::collect_copy`).
    pub foreign_copy: bool,
    /// True when the cell was unchanged since its last pass and skipped full recovery.
    pub skipped: bool,
    /// True when this pass removed a dead cell whole.
    pub removed: bool,
    /// Backend clock timestamp (milliseconds) when the pass observed this cell.
    pub now: u64,
    /// `Head::format` when the pass read the cell's head (`None` on skipped or headless cells).
    pub head_format: Option<u32>,
    /// `CollectorLease::format` when a collector lease was read or written.
    pub lease_format: Option<u32>,
    /// `Some(true)` if the head is live, `Some(false)` if deleted, `None` if unread/missing.
    pub live: Option<bool>,
    /// Count of named versions on `Head`, when read.
    pub versions: Option<u64>,
    /// Whether `Head` has an origin (fork), when read.
    pub is_fork: Option<bool>,
    /// Whether `Head` has active peer replication configured, when read.
    pub replicated: Option<bool>,
    /// Count of fork/pin/hold references listed under the cell.
    pub references: u64,
    /// Surviving objects under this cell's prefix after this pass's deletions.
    pub kept: &'a [ObjectSample],
    /// Objects deleted by this pass.
    pub deleted: &'a [ObjectSample],
}

/// Associative merge operation for composable metric aggregates.
pub trait Merge: Default {
    /// Folds `other` into `self` (associative and commutative).
    fn merge(&mut self, other: Self);
}

impl Merge for u64 {
    fn merge(&mut self, other: Self) {
        *self = self.saturating_add(other);
    }
}

impl Merge for Sum {
    fn merge(&mut self, other: Self) {
        self.objects = self.objects.saturating_add(other.objects);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
}

/// A composable streaming aggregate over GC cell passes.
///
/// Implement `observe` and [`Merge::merge`] to fold observations from concurrent passes or
/// distributed workers (`gc_wasm`).
pub trait Metric: Merge + Send + Sync {
    /// Folds one cell pass into this aggregate.
    fn observe(&mut self, cell: &CellSample<'_>);
}

/// Thread-safe receiver attached to a [`Collector`](crate::collector::Collector) or
/// [`Sweep`](crate::sweep::Sweep).
pub trait Observer: Send + Sync {
    fn observe(&self, cell: &CellSample<'_>);
}

/// Thread-safe [`Observer`] wrapping any [`Metric`] behind a mutex.
#[derive(Debug, Default)]
pub struct Shared<M> {
    inner: Mutex<M>,
}

impl<M> Shared<M> {
    pub fn new(metric: M) -> Self {
        Self { inner: Mutex::new(metric) }
    }

    /// Extracts the accumulated metric, resetting the shared state to `M::default()`.
    pub fn take(&self) -> M
    where
        M: Default,
    {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    }

    /// Returns a cloned snapshot of the current aggregate without resetting it.
    pub fn snapshot(&self) -> M
    where
        M: Clone,
    {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Runs `f` with mutable access to the inner metric (e.g. to attach sweep-level metadata).
    pub fn update<R>(&self, f: impl FnOnce(&mut M) -> R) -> R {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut *guard)
    }
}

impl<M: Metric> Observer for Shared<M> {
    fn observe(&self, cell: &CellSample<'_>) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.observe(cell);
    }
}

/// Broadcasts every [`CellSample`] to multiple independent [`Observer`]s.
#[derive(Default)]
pub struct FanOut {
    observers: Vec<Arc<dyn Observer>>,
}

impl FanOut {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, observer: Arc<dyn Observer>) {
        self.observers.push(observer);
    }

    pub fn with(mut self, observer: Arc<dyn Observer>) -> Self {
        self.push(observer);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.observers.is_empty()
    }
}

impl Observer for FanOut {
    fn observe(&self, cell: &CellSample<'_>) {
        for observer in &self.observers {
            observer.observe(cell);
        }
    }
}

/// Adapts a closure into an [`Observer`] for ad-hoc instrumentation and tests.
pub struct FnObserver<F>(pub F);

impl<F> Observer for FnObserver<F>
where
    F: Fn(&CellSample<'_>) + Send + Sync,
{
    fn observe(&self, cell: &CellSample<'_>) {
        (self.0)(cell);
    }
}

/// Exact streaming summary statistics (`count`, `sum`, `min`, `max`) over `u64` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub count: u64,
    pub sum: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
}

impl Stats {
    pub fn observe(&mut self, value: u64) {
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(value);
        self.min = Some(self.min.map_or(value, |cur| cur.min(value)));
        self.max = Some(self.max.map_or(value, |cur| cur.max(value)));
    }

    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum as f64 / self.count as f64)
    }
}

impl Merge for Stats {
    fn merge(&mut self, other: Self) {
        if other.count == 0 {
            return;
        }
        self.count = self.count.saturating_add(other.count);
        self.sum = self.sum.saturating_add(other.sum);
        self.min = match (self.min, other.min) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.max = match (self.max, other.max) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

/// Fixed-bucket distribution over `u64` values (byte sizes, object counts, ages, etc.).
///
/// `bounds` holds strictly increasing inclusive upper bounds; `counts` has `bounds.len() + 1`
/// exclusive bucket counts where the last slot counts values strictly greater than `bounds.last()`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    pub bounds: Vec<u64>,
    pub counts: Vec<u64>,
    pub stats: Stats,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::with_bounds(OBJECT_SIZE_BOUNDS)
    }
}

impl Histogram {
    /// Creates an empty histogram with the given strictly increasing inclusive upper `bounds`.
    pub fn with_bounds(bounds: &[u64]) -> Self {
        debug_assert!(bounds.windows(2).all(|w| w[0] < w[1]), "histogram bounds must increase");
        Self { bounds: bounds.to_vec(), counts: vec![0; bounds.len() + 1], stats: Stats::default() }
    }

    pub fn object_sizes() -> Self {
        Self::with_bounds(OBJECT_SIZE_BOUNDS)
    }

    pub fn cell_sizes() -> Self {
        Self::with_bounds(CELL_SIZE_BOUNDS)
    }

    pub fn cell_objects() -> Self {
        Self::with_bounds(CELL_OBJECT_BOUNDS)
    }

    /// Records one observation `value`.
    pub fn observe(&mut self, value: u64) {
        let idx = self.bounds.partition_point(|&bound| value > bound);
        if let Some(slot) = self.counts.get_mut(idx) {
            *slot = slot.saturating_add(1);
        }
        self.stats.observe(value);
    }

    /// Iterates cumulative Prometheus-style `(Some(bound), cumulative_count)` buckets followed by
    /// `(None, total_count)` for `+Inf`.
    pub fn cumulative_buckets(&self) -> Vec<(Option<u64>, u64)> {
        let mut out = Vec::with_capacity(self.bounds.len() + 1);
        let mut running = 0u64;
        for (idx, &bound) in self.bounds.iter().enumerate() {
            running = running.saturating_add(self.counts.get(idx).copied().unwrap_or(0));
            out.push((Some(bound), running));
        }
        out.push((None, self.stats.count));
        out
    }

    /// Estimates quantile `q` in `[0.0, 1.0]` as the upper bound of the bucket containing the
    /// `q`-th observation, clamped to `[stats.min, stats.max]`.
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.stats.count == 0 || !q.is_finite() {
            return None;
        }
        let q = q.clamp(0.0, 1.0);
        let target = ((self.stats.count as f64) * q).ceil().max(1.0) as u64;
        let mut running = 0u64;
        for (idx, &count) in self.counts.iter().enumerate() {
            running = running.saturating_add(count);
            if running >= target {
                let raw =
                    self.bounds.get(idx).copied().unwrap_or_else(|| self.stats.max.unwrap_or(0));
                let lo = self.stats.min.unwrap_or(0);
                let hi = self.stats.max.unwrap_or(raw);
                return Some(raw.clamp(lo, hi));
            }
        }
        self.stats.max
    }
}

impl Merge for Histogram {
    fn merge(&mut self, other: Self) {
        if other.stats.count == 0 {
            return;
        }
        if self.stats.count == 0 && self.bounds != other.bounds {
            *self = other;
            return;
        }
        if self.bounds == other.bounds && self.counts.len() == other.counts.len() {
            for (dst, src) in self.counts.iter_mut().zip(other.counts) {
                *dst = dst.saturating_add(src);
            }
        }
        self.stats.merge(other.stats);
    }
}

/// Generic associative map grouping any [`Merge`] aggregate by key `K` (tenant, kind, version, …).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupBy<K: Ord, V>(pub BTreeMap<K, V>);

impl<K: Ord, V> Default for GroupBy<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Ord, V: Merge> GroupBy<K, V> {
    pub fn observe_key(&mut self, key: K, f: impl FnOnce(&mut V)) {
        f(self.0.entry(key).or_default());
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.0.get(key)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, K, V> {
        self.0.iter()
    }
}

impl<K: Ord, V: Merge> Merge for GroupBy<K, V> {
    fn merge(&mut self, other: Self) {
        for (k, v) in other.0 {
            self.0.entry(k).or_default().merge(v);
        }
    }
}

/// Aggregate cell state counts across a sweep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellCounts {
    pub total: u64,
    pub live: u64,
    pub deleted: u64,
    pub removed: u64,
    pub skipped: u64,
    pub foreign_copy: u64,
    pub forked: u64,
    pub replicated: u64,
    pub versions_total: u64,
    pub references_total: u64,
}

impl Merge for CellCounts {
    fn merge(&mut self, other: Self) {
        self.total = self.total.saturating_add(other.total);
        self.live = self.live.saturating_add(other.live);
        self.deleted = self.deleted.saturating_add(other.deleted);
        self.removed = self.removed.saturating_add(other.removed);
        self.skipped = self.skipped.saturating_add(other.skipped);
        self.foreign_copy = self.foreign_copy.saturating_add(other.foreign_copy);
        self.forked = self.forked.saturating_add(other.forked);
        self.replicated = self.replicated.saturating_add(other.replicated);
        self.versions_total = self.versions_total.saturating_add(other.versions_total);
        self.references_total = self.references_total.saturating_add(other.references_total);
    }
}

/// Bucket-level storage-version ratchet snapshot included in [`FormatStats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketGates {
    pub storage: u32,
    pub fxfs: u32,
}

impl From<formats::Gates> for BucketGates {
    fn from(gates: formats::Gates) -> Self {
        Self { storage: gates.storage, fxfs: gates.fxfs }
    }
}

/// Format version inventory gathered during GC sweeps (for upgrade & migration visibility).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatStats {
    /// Bucket's `formats` ratchet (`storage` + `fxfs`), when present and read by the sweep.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket_gates: Option<BucketGates>,
    /// Count of observed cells by `Head::format` (from full passes that read `head`).
    pub head_versions: GroupBy<u32, u64>,
    /// Count of observed cells by `CollectorLease::format` (includes skipped cells).
    pub lease_versions: GroupBy<u32, u64>,
    /// Up to [`MAX_LAGGING_SAMPLES`] sample cell IDs per `Head::format` strictly older than
    /// `layout::FORMAT.current`, so operators or migration workers know which cells lag.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lagging_heads: BTreeMap<u32, Vec<String>>,
}

impl FormatStats {
    /// Total observed heads strictly older than `target`.
    pub fn heads_older_than(&self, target: u32) -> u64 {
        self.head_versions
            .iter()
            .filter(|&(&version, _)| version < target)
            .map(|(_, &count)| count)
            .fold(0u64, u64::saturating_add)
    }

    fn observe(&mut self, cell: &CellSample<'_>) {
        if let Some(version) = cell.head_format {
            self.head_versions.observe_key(version, |n| *n = n.saturating_add(1));
            if version < layout::FORMAT.current {
                let list = self.lagging_heads.entry(version).or_default();
                if list.len() < MAX_LAGGING_SAMPLES {
                    list.push(cell.cell.to_string());
                }
            }
        }
        if let Some(version) = cell.lease_format {
            self.lease_versions.observe_key(version, |n| *n = n.saturating_add(1));
        }
    }
}

impl Merge for FormatStats {
    fn merge(&mut self, other: Self) {
        if self.bucket_gates.is_none() {
            self.bucket_gates = other.bucket_gates;
        }
        self.head_versions.merge(other.head_versions);
        self.lease_versions.merge(other.lease_versions);
        for (version, samples) in other.lagging_heads {
            let dst = self.lagging_heads.entry(version).or_default();
            let room = MAX_LAGGING_SAMPLES.saturating_sub(dst.len());
            dst.extend(samples.into_iter().take(room));
        }
    }
}

/// Per-object-kind storage and size-distribution breakdown (`segment`, `checkpoint`, `head`, …).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindStats {
    pub stored: Sum,
    pub reclaimed: Sum,
    pub sizes: Histogram,
}

impl Default for KindStats {
    fn default() -> Self {
        Self { stored: Sum::default(), reclaimed: Sum::default(), sizes: Histogram::object_sizes() }
    }
}

impl Merge for KindStats {
    fn merge(&mut self, other: Self) {
        self.stored.merge(other.stored);
        self.reclaimed.merge(other.reclaimed);
        self.sizes.merge(other.sizes);
    }
}

/// Per-tenant storage accounting (ready for billing / cost attribution).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantStats {
    pub cells: u64,
    pub live_cells: u64,
    pub deleted_cells: u64,
    pub removed_cells: u64,
    pub skipped_cells: u64,
    pub foreign_copy_cells: u64,
    pub stored: Sum,
    pub reclaimed: Sum,
    pub versions: u64,
    pub kinds: GroupBy<String, Sum>,
    pub cell_sizes: Histogram,
}

impl Default for TenantStats {
    fn default() -> Self {
        Self {
            cells: 0,
            live_cells: 0,
            deleted_cells: 0,
            removed_cells: 0,
            skipped_cells: 0,
            foreign_copy_cells: 0,
            stored: Sum::default(),
            reclaimed: Sum::default(),
            versions: 0,
            kinds: GroupBy::default(),
            cell_sizes: Histogram::cell_sizes(),
        }
    }
}

impl Merge for TenantStats {
    fn merge(&mut self, other: Self) {
        self.cells = self.cells.saturating_add(other.cells);
        self.live_cells = self.live_cells.saturating_add(other.live_cells);
        self.deleted_cells = self.deleted_cells.saturating_add(other.deleted_cells);
        self.removed_cells = self.removed_cells.saturating_add(other.removed_cells);
        self.skipped_cells = self.skipped_cells.saturating_add(other.skipped_cells);
        self.foreign_copy_cells = self.foreign_copy_cells.saturating_add(other.foreign_copy_cells);
        self.stored.merge(other.stored);
        self.reclaimed.merge(other.reclaimed);
        self.versions = self.versions.saturating_add(other.versions);
        self.kinds.merge(other.kinds);
        self.cell_sizes.merge(other.cell_sizes);
    }
}

/// Standard built-in GC metrics bundle combining format-version inventory, object/cell size
/// histograms, per-kind storage breakdown, and per-tenant billing accounting.
///
/// Serializable to JSON and associative under [`Merge::merge`], so it works identically for
/// local multi-threaded sweeps (`cladefs-gc`) and fan-out/fan-in Cloudflare Workers (`gc_wasm`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepReport {
    pub cells: CellCounts,
    pub stored: Sum,
    pub reclaimed: Sum,
    pub formats: FormatStats,
    pub object_sizes: Histogram,
    pub cell_sizes: Histogram,
    pub cell_objects: Histogram,
    pub kinds: GroupBy<String, KindStats>,
    pub tenants: GroupBy<String, TenantStats>,
}

impl Default for SweepReport {
    fn default() -> Self {
        Self {
            cells: CellCounts::default(),
            stored: Sum::default(),
            reclaimed: Sum::default(),
            formats: FormatStats::default(),
            object_sizes: Histogram::object_sizes(),
            cell_sizes: Histogram::cell_sizes(),
            cell_objects: Histogram::cell_objects(),
            kinds: GroupBy::default(),
            tenants: GroupBy::default(),
        }
    }
}

impl Merge for SweepReport {
    fn merge(&mut self, other: Self) {
        self.cells.merge(other.cells);
        self.stored.merge(other.stored);
        self.reclaimed.merge(other.reclaimed);
        self.formats.merge(other.formats);
        self.object_sizes.merge(other.object_sizes);
        self.cell_sizes.merge(other.cell_sizes);
        self.cell_objects.merge(other.cell_objects);
        self.kinds.merge(other.kinds);
        self.tenants.merge(other.tenants);
    }
}

impl Metric for SweepReport {
    fn observe(&mut self, cell: &CellSample<'_>) {
        self.cells.total = self.cells.total.saturating_add(1);
        if cell.foreign_copy {
            self.cells.foreign_copy = self.cells.foreign_copy.saturating_add(1);
        }
        if cell.skipped {
            self.cells.skipped = self.cells.skipped.saturating_add(1);
        }
        if cell.removed {
            self.cells.removed = self.cells.removed.saturating_add(1);
        }
        match cell.live {
            Some(true) => self.cells.live = self.cells.live.saturating_add(1),
            Some(false) => self.cells.deleted = self.cells.deleted.saturating_add(1),
            None => {}
        }
        if cell.is_fork == Some(true) {
            self.cells.forked = self.cells.forked.saturating_add(1);
        }
        if cell.replicated == Some(true) {
            self.cells.replicated = self.cells.replicated.saturating_add(1);
        }
        if let Some(versions) = cell.versions {
            self.cells.versions_total = self.cells.versions_total.saturating_add(versions);
        }
        self.cells.references_total = self.cells.references_total.saturating_add(cell.references);
        self.formats.observe(cell);

        let mut cell_stored = Sum::default();
        let mut cell_reclaimed = Sum::default();
        let tenant_key = Short(cell.cell.tenant).to_string();

        for obj in cell.kept {
            let one = Sum { objects: 1, bytes: obj.size };
            cell_stored += one;
            self.stored += one;
            self.object_sizes.observe(obj.size);
            self.kinds.observe_key(obj.kind.to_owned(), |kind| {
                kind.stored += one;
                kind.sizes.observe(obj.size);
            });
            self.tenants.observe_key(tenant_key.clone(), |tenant| {
                tenant.kinds.observe_key(obj.kind.to_owned(), |sum| *sum += one);
            });
        }
        for obj in cell.deleted {
            let one = Sum { objects: 1, bytes: obj.size };
            cell_reclaimed += one;
            self.reclaimed += one;
            self.kinds.observe_key(obj.kind.to_owned(), |kind| {
                kind.reclaimed += one;
            });
        }

        self.cell_sizes.observe(cell_stored.bytes);
        self.cell_objects.observe(cell_stored.objects);

        self.tenants.observe_key(tenant_key, |tenant| {
            tenant.cells = tenant.cells.saturating_add(1);
            if cell.foreign_copy {
                tenant.foreign_copy_cells = tenant.foreign_copy_cells.saturating_add(1);
            }
            if cell.skipped {
                tenant.skipped_cells = tenant.skipped_cells.saturating_add(1);
            }
            if cell.removed {
                tenant.removed_cells = tenant.removed_cells.saturating_add(1);
            }
            match cell.live {
                Some(true) => tenant.live_cells = tenant.live_cells.saturating_add(1),
                Some(false) => tenant.deleted_cells = tenant.deleted_cells.saturating_add(1),
                None => {}
            }
            if let Some(versions) = cell.versions {
                tenant.versions = tenant.versions.saturating_add(versions);
            }
            tenant.stored += cell_stored;
            tenant.reclaimed += cell_reclaimed;
            tenant.cell_sizes.observe(cell_stored.bytes);
        });
    }
}

impl SweepReport {
    /// Formats bounded-cardinality Prometheus exposition metrics from this sweep report.
    ///
    /// Per-tenant rows are intentionally excluded from Prometheus output to avoid unbounded label
    /// cardinality; use the JSON report (`cladefs-gc --report`) for per-tenant billing data.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::new();
        if self.cells.total == 0 && self.formats.bucket_gates.is_none() {
            return out;
        }
        if let Some(gates) = self.formats.bucket_gates {
            let _ = writeln!(
                out,
                "# HELP cladefs_gc_bucket_gate_version Active bucket format ratchet version.\n\
                 # TYPE cladefs_gc_bucket_gate_version gauge\n\
                 cladefs_gc_bucket_gate_version{{knob=\"storage\"}} {}\n\
                 cladefs_gc_bucket_gate_version{{knob=\"fxfs\"}} {}",
                gates.storage, gates.fxfs
            );
        }
        if self.cells.total == 0 {
            return out;
        }
        let _ = writeln!(
            out,
            "# HELP cladefs_gc_tenants_observed Distinct tenants observed in the last sweep.\n\
             # TYPE cladefs_gc_tenants_observed gauge\n\
             cladefs_gc_tenants_observed {}",
            self.tenants.len()
        );
        let _ = writeln!(
            out,
            "# HELP cladefs_gc_cell_states Observed cells in the last sweep, by state.\n\
             # TYPE cladefs_gc_cell_states gauge\n\
             cladefs_gc_cell_states{{state=\"live\"}} {}\n\
             cladefs_gc_cell_states{{state=\"deleted\"}} {}\n\
             cladefs_gc_cell_states{{state=\"removed\"}} {}\n\
             cladefs_gc_cell_states{{state=\"skipped\"}} {}\n\
             cladefs_gc_cell_states{{state=\"foreign_copy\"}} {}",
            self.cells.live,
            self.cells.deleted,
            self.cells.removed,
            self.cells.skipped,
            self.cells.foreign_copy
        );
        if !self.formats.head_versions.is_empty() || !self.formats.lease_versions.is_empty() {
            let _ = writeln!(
                out,
                "# HELP cladefs_gc_format_cells Observed cells by on-disk format version.\n\
                 # TYPE cladefs_gc_format_cells gauge"
            );
            for (version, count) in self.formats.head_versions.iter() {
                let _ = writeln!(
                    out,
                    "cladefs_gc_format_cells{{scope=\"head\",version=\"{version}\"}} {count}"
                );
            }
            for (version, count) in self.formats.lease_versions.iter() {
                let _ = writeln!(
                    out,
                    "cladefs_gc_format_cells{{scope=\"lease\",version=\"{version}\"}} {count}"
                );
            }
            let _ = writeln!(
                out,
                "# HELP cladefs_gc_lagging_format_cells Observed cell heads older than this build's current format.\n\
                 # TYPE cladefs_gc_lagging_format_cells gauge\n\
                 cladefs_gc_lagging_format_cells {}",
                self.formats.heads_older_than(layout::FORMAT.current)
            );
        }
        if !self.kinds.is_empty() {
            let _ = writeln!(
                out,
                "# HELP cladefs_gc_stored_objects Surviving objects in the last sweep, by kind.\n\
                 # TYPE cladefs_gc_stored_objects gauge"
            );
            for (kind, stats) in self.kinds.iter() {
                let _ = writeln!(
                    out,
                    "cladefs_gc_stored_objects{{kind=\"{kind}\"}} {}",
                    stats.stored.objects
                );
            }
            let _ = writeln!(
                out,
                "# HELP cladefs_gc_stored_bytes Surviving bytes in the last sweep, by kind.\n\
                 # TYPE cladefs_gc_stored_bytes gauge"
            );
            for (kind, stats) in self.kinds.iter() {
                let _ = writeln!(
                    out,
                    "cladefs_gc_stored_bytes{{kind=\"{kind}\"}} {}",
                    stats.stored.bytes
                );
            }
        }
        write_prometheus_histogram(
            &mut out,
            "cladefs_gc_object_size_bytes",
            "Size distribution of surviving objects in the last sweep, in bytes.",
            &self.object_sizes,
        );
        write_prometheus_histogram(
            &mut out,
            "cladefs_gc_cell_size_bytes",
            "Total surviving bytes per cell in the last sweep.",
            &self.cell_sizes,
        );
        write_prometheus_histogram(
            &mut out,
            "cladefs_gc_cell_objects",
            "Surviving object count per cell in the last sweep.",
            &self.cell_objects,
        );
        out
    }
}

fn write_prometheus_histogram(out: &mut String, name: &str, help: &str, hist: &Histogram) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
    for (bound, cumulative) in hist.cumulative_buckets() {
        match bound {
            Some(le) => {
                let _ = writeln!(out, "{name}_bucket{{le=\"{le}\"}} {cumulative}");
            }
            None => {
                let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {cumulative}");
            }
        }
    }
    let _ = writeln!(out, "{name}_sum {}", hist.stats.sum);
    let _ = writeln!(out, "{name}_count {}", hist.stats.count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_observe_merge_and_quantiles() {
        let mut a = Histogram::with_bounds(&[100, 1_000, 10_000]);
        for v in [10, 50, 500, 50_000] {
            a.observe(v);
        }
        let mut b = Histogram::with_bounds(&[100, 1_000, 10_000]);
        for v in [100, 2_000] {
            b.observe(v);
        }
        a.merge(b);
        assert_eq!(a.stats.count, 6);
        assert_eq!(a.stats.sum, 52_660);
        assert_eq!(a.stats.min, Some(10));
        assert_eq!(a.stats.max, Some(50_000));
        assert_eq!(a.counts, vec![3, 1, 1, 1]);
        assert_eq!(
            a.cumulative_buckets(),
            vec![(Some(100), 3), (Some(1_000), 4), (Some(10_000), 5), (None, 6)]
        );
        assert_eq!(a.quantile(0.5), Some(100));
        assert_eq!(a.quantile(0.95), Some(50_000));
    }

    #[test]
    fn custom_metric_and_fanout_compose_cleanly() {
        #[derive(Default)]
        struct LargeSegments {
            count: u64,
            bytes: u64,
        }
        impl Merge for LargeSegments {
            fn merge(&mut self, other: Self) {
                self.count += other.count;
                self.bytes += other.bytes;
            }
        }
        impl Metric for LargeSegments {
            fn observe(&mut self, cell: &CellSample<'_>) {
                for obj in cell.kept {
                    if obj.kind == "segment" && obj.size >= (1 << 20) {
                        self.count += 1;
                        self.bytes += obj.size;
                    }
                }
            }
        }

        let report = Arc::new(Shared::<SweepReport>::default());
        let large = Arc::new(Shared::<LargeSegments>::default());
        let fan = FanOut::new().with(report.clone()).with(large.clone());

        let kept_a = [
            ObjectSample { kind: "head", size: 512, modified: 10 },
            ObjectSample { kind: "segment", size: 2 << 20, modified: 20 },
            ObjectSample { kind: "checkpoint", size: 8 << 10, modified: 30 },
        ];
        let deleted_a = [ObjectSample { kind: "segment", size: 4 << 10, modified: 5 }];
        fan.observe(&CellSample {
            cell: CellId { tenant: 7, id: 1 },
            foreign_copy: false,
            skipped: false,
            removed: false,
            now: 100,
            head_format: Some(layout::FORMAT.current - 1),
            lease_format: Some(layout::FORMAT.current),
            live: Some(true),
            versions: Some(3),
            is_fork: Some(false),
            replicated: Some(true),
            references: 2,
            kept: &kept_a,
            deleted: &deleted_a,
        });

        let kept_b = [ObjectSample { kind: "segment", size: 64 << 10, modified: 40 }];
        fan.observe(&CellSample {
            cell: CellId { tenant: 9, id: 2 },
            foreign_copy: false,
            skipped: true,
            removed: false,
            now: 100,
            head_format: None,
            lease_format: Some(layout::FORMAT.current),
            live: None,
            versions: None,
            is_fork: None,
            replicated: None,
            references: 0,
            kept: &kept_b,
            deleted: &[],
        });

        let large = large.take();
        assert_eq!(large.count, 1);
        assert_eq!(large.bytes, 2 << 20);

        let mut report = report.take();
        report.formats.bucket_gates = Some(BucketGates { storage: 1, fxfs: 66 });
        assert_eq!(report.cells.total, 2);
        assert_eq!(report.cells.live, 1);
        assert_eq!(report.cells.skipped, 1);
        assert_eq!(report.cells.versions_total, 3);
        assert_eq!(report.stored.objects, 4);
        assert_eq!(report.stored.bytes, 512 + (2 << 20) + (8 << 10) + (64 << 10));
        assert_eq!(report.reclaimed, Sum { objects: 1, bytes: 4 << 10 });
        assert_eq!(report.formats.heads_older_than(layout::FORMAT.current), 1);
        assert_eq!(
            report.formats.lagging_heads.get(&(layout::FORMAT.current - 1)),
            Some(&vec![CellId { tenant: 7, id: 1 }.to_string()])
        );
        assert_eq!(report.tenants.len(), 2);
        let t7 = report.tenants.get(&Short(7).to_string()).unwrap();
        assert_eq!(t7.stored.bytes, 512 + (2 << 20) + (8 << 10));
        assert_eq!(t7.versions, 3);

        // Associative merge check: splitting and merging gives the exact same report.
        let mut merged = SweepReport::default();
        merged.merge(report.clone());
        assert_eq!(merged, report);

        let prom = report.to_prometheus();
        assert!(prom.contains("cladefs_gc_bucket_gate_version{knob=\"storage\"} 1"));
        assert!(prom.contains("cladefs_gc_tenants_observed 2"));
        assert!(prom.contains("cladefs_gc_lagging_format_cells 1"));
        assert!(prom.contains("cladefs_gc_stored_bytes{kind=\"segment\"}"));
        assert!(prom.contains("cladefs_gc_object_size_bytes_count 4"));
        assert!(prom.contains("cladefs_gc_cell_size_bytes_count 2"));
        // High-cardinality tenant IDs must never appear in Prometheus output.
        assert!(!prom.contains("tenant="));
    }

    #[cfg(feature = "full")]
    #[test]
    fn collector_and_sweep_populate_standard_and_custom_metrics() {
        use crate::collector::Collector;
        use crate::object_bucket::ObjectBucket;
        use crate::sweep::{Limits, Mode, Sweep};
        use crate::{identity, lifecycle};

        let tokio = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio.block_on(async {
            let bucket =
                Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
            let _ = identity::format(&*bucket, &identity::Details::default()).await.unwrap();

            let cell_a = CellId { tenant: 10, id: 1 };
            let cell_b = CellId { tenant: 20, id: 2 };
            let cell_dead = CellId { tenant: 20, id: 3 };
            for cell in [cell_a, cell_b, cell_dead] {
                lifecycle::create(&*bucket, cell, 0).await.unwrap();
            }
            // Mark cell_dead deleted so GC removes it whole on the sweep.
            lifecycle::delete(&*bucket, cell_dead, 1, u64::MAX).await.unwrap();

            let shared = Arc::new(Shared::<SweepReport>::default());
            let seen_cells = Arc::new(Mutex::new(Vec::new()));
            let seen_ref = seen_cells.clone();
            let custom = Arc::new(FnObserver(move |sample: &CellSample<'_>| {
                seen_ref.lock().unwrap().push((sample.cell, sample.skipped, sample.removed));
            }));
            let fan = Arc::new(FanOut::new().with(shared.clone()).with(custom));

            let sweep =
                Sweep { mode: Mode::Delete, at_once: 2, lease: 60_000, slowest: 5, spawned: false };
            let limits = Limits { observer: Some(fan), ..Limits::default() };
            let swept =
                sweep.run_limited(&bucket, None::<&Arc<ObjectBucket>>, None, &limits, |_| {}).await;
            assert!(swept.complete(), "{swept:?}");

            let report = shared.take();
            assert_eq!(report.cells.total, 3);
            assert_eq!(report.cells.live, 2);
            assert_eq!(report.cells.removed, 1);
            assert_eq!(report.cells.skipped, 0);
            assert_eq!(report.formats.head_versions.get(&layout::FORMAT.current), Some(&3));
            assert_eq!(report.formats.heads_older_than(layout::FORMAT.current), 0);
            assert_eq!(report.tenants.len(), 2);
            assert!(report.stored.bytes > 0);
            assert!(report.reclaimed.bytes > 0);
            assert_eq!(seen_cells.lock().unwrap().len(), 3);

            // Second pass over unchanged live cells takes the skipped fast path and still emits
            // stored size / lease-format observations with zero extra GETs.
            let shared2 = Arc::new(Shared::<SweepReport>::default());
            let mut collector = Collector::new().observer(Some(shared2.clone()));
            let rep2 = collector.collect(&*bucket, 99).await.unwrap();
            assert_eq!(rep2.skipped, 2);
            let report2 = shared2.take();
            assert_eq!(report2.cells.total, 2);
            assert_eq!(report2.cells.skipped, 2);
            assert_eq!(report2.formats.lease_versions.get(&layout::FORMAT.current), Some(&2));
            assert_eq!(report2.tenants.len(), 2);
            assert!(report2.stored.bytes > 0);
        });
    }
}
