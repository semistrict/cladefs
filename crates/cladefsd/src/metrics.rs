// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's metrics, in Prometheus' text exposition format, served over HTTP to whoever scrapes
//! them (TASK-47): `GET /metrics` on the address `CLADEFSD_METRICS` names (`super::http`).
//!
//! cladefs: a cell is labelled one way in every family, `cell="TENANT/CELL"`, each in base 32, as
//! cells name it (`CellId`'s display; board 8563): `cladefsd_vm_*` joins `cladefsd_upload_*` and
//! `cladefsd_replication_*` on it. Those families are per cell, and every fork is a cell of its
//! own, so their series come and go with the cells opened: an operator aggregates them (`sum
//! without (cell)`, or recording rules) rather than keep each cell's for long. No family is per
//! connection: a VM process's number, new for each, would make series of every VM started.
//!
//! - `cladefsd_requests_total{kind}`, `cladefsd_request_errors_total{kind}`, and
//!   `cladefsd_request_duration_seconds{kind}`, a histogram: the VM processes' requests, by
//!   kind, as the daemon served them, from their dispatch to their reply's sending. cladefs: its
//!   `+Inf` bucket and `_count` are the sum of the buckets as a scrape read them, so that no
//!   bucket is ever past them (board 8561); its `_sum` is read apart, as Prometheus allows. Its
//!   bounds past 1 s are board 5750's.
//! - `cladefsd_build_info{commit}`, 1: the commit the daemon was built of
//!   (`crate::BUILD_COMMIT`); `cladefsd_vm_build_info{cell,commit}`: the VM processes
//!   connected, by their cell and the commit they said they were built of (`wire::BUILD`)
//!   (TASK-111.7).
//! - `cladefsd_connections` and `cladefsd_stores_open`: the VM processes connected, and the cells
//!   their connections write.
//! - `cladefsd_extent_cache_pinned_entries` and `cladefsd_extent_cache_idle_entries`: the sealed
//!   extents cached that something but the cache holds, and those nothing else does
//!   (`crate::cache::entries`); `cladefsd_window_mappings`, `cladefsd_window_mapped_bytes` and
//!   `cladefsd_window_pages`: the ranges the VMs' windows map, their bytes, and the pages of them
//!   mapped in the VM processes (`crate::dax::live`, TASK-131.6); cladefs:
//!   `cladefsd_window_private_pages`, the private copies of windows that copy on write, as their
//!   VM processes say (doc-26), and `cladefsd_window_open_maps_refused_total`, the `MAP`s of open extents refused, which no
//!   guest makes (backlog doc-13).
//! - cladefs: of the VM processes connected that opened a cell, by their cell, summed
//!   (TASK-111.5, board 8563): `cladefsd_vm_window_mappings`,
//!   `cladefsd_vm_window_mapped_bytes`, `cladefsd_vm_window_pages` and
//!   `cladefsd_vm_window_private_pages`, its window's, as the gauges above count all windows';
//!   `cladefsd_vm_host_mappings`, the runs of pages its window's second stage has installed in
//!   its memory, each a mapping the host's `vm.max_map_count` limits, as it last said with its
//!   stage operations, and `cladefsd_vm_host_mapping_budget`, the most it keeps, from the limit
//!   the daemon told it (TASK-172): its share of its VM process's windows', as it last said, again
//!   as another window of the process comes or goes (board 10098).
//! - `cladefsd_<counter>_total`: the counters the control API's `GetCounters` reports
//!   (`crate::stats::Stats::named`, its dashes underscores): maps, faults, page-ins, reads and
//!   their nanoseconds and bytes; the VM processes' changes to their windows (`stage_ops`) and
//!   their nanoseconds in them (`stage_ns`), whose ratio is what a window change costs a VM
//!   process.
//! - `cladefsd_memory_bytes`, `cladefsd_memory_limit_bytes` and
//!   `cladefsd_memory_capacity_bytes`: the memory the daemon's cached extents' pages
//!   hold, every tenant's together, its limit, if any, and the most the daemon's kernel has
//!   (TASK-50.3); cladefs: `cladefsd_memory_free_bytes`, `cladefsd_memory_quarantined_bytes` and
//!   `cladefsd_memory_exhausted_arenas`: of it, pages free, pages freed and waiting for VM
//!   processes to unmap them, and the arenas whose relief found nothing to free (board 5662); `cladefsd_tenant_memory_bytes{tenant}` and
//!   `cladefsd_tenant_memory_limit_bytes{tenant}`: each tenant's alive, by its ID, in
//!   base 32, as cells name it.
//! - `cladefsd_disk_bytes`, `cladefsd_disk_reserved_bytes` and `cladefsd_disk_limit_bytes`: the
//!   host's disk every tenant's cells take together, counted as they write, what they hold past
//!   it against the limit (their reserves and writes admitted), and the limit, if any (TASK-94,
//!   doc-22); `cladefsd_tenant_disk_bytes{tenant}`, `cladefsd_tenant_disk_reserved_bytes{tenant}`,
//!   `cladefsd_tenant_disk_limit_bytes{tenant}`, `cladefsd_tenant_disk_weight{tenant}` and,
//!   under pressure, `cladefsd_tenant_disk_share_bytes{tenant}`: each tenant's alive, by its ID
//!   (`super::disk`).
//! - cladefs: `cladefsd_host_disk_*_bytes{device}`, for each filesystem the daemon keeps cells or
//!   the part cache on, by its device as `MAJOR:MINOR` (doc-22, TASK-539.11): its `size`, `free`,
//!   the governor's `headroom` and `allowance`, what its `cells` take, of that `unframed`, their
//!   `reserved` and `pledged` bytes, what the `cache` keeps, the `spare` the cells leave of the
//!   allowance, and the `other` bytes used on it that the daemon counts as none of its own, an
//!   uncounted writer's on a dedicated disk; `cladefsd_host_disk_pressure{device}`, 1 while under
//!   pressure.
//! - `cladefsd_upload_pending_bytes{cell}`, `cladefsd_upload_oldest_pending_seconds
//!   {cell}` and `cladefsd_upload_stalls_total{cell}`: for each open cell whose commits
//!   upload, its commits' bytes not yet uploaded, how long the oldest of them has waited, and how
//!   many commits waited for the lag to shrink (TASK-91.3).
//! - cladefs: `cladefsd_replication_degraded{cell}` and `cladefsd_replication_members_up{cell}`:
//!   for each open cell of a replicated cell, 1 while it is degraded (fewer than f members of its
//!   configuration take its writer's commits, or it dropped to f = 0, its commits waiting for the
//!   bucket), and how many members do (doc-21, Degraded; TASK-537.3.5).
//! - cladefs: `cladefsd_upload_held_bytes`, `cladefsd_upload_held_max_bytes` and
//!   `cladefsd_upload_memory_bytes`: the bytes of objects the cells' uploads hold in memory,
//!   every cell's together, now and at most, and their budget, `CLADEFSD_UPLOAD_MEMORY`
//!   (`crate::bucket::upload_memory`, TASK-317).
//! - cladefs: `cladefsd_part_cache_bytes` and `cladefsd_part_cache_capacity_bytes`: what
//!   the host's part cache keeps and its budget, `CLADEFSD_PART_CACHE_BYTES` (TASK-406.1): 0
//!   until a cell opens it, or if the daemon keeps none (board 8561).
//! - cladefs: `cladefsd_peer_parts_total{outcome}`: the parts the cells' fetchers asked the
//!   peers that own them for, by what came of it (`given`, `missing`, `failed`), and
//!   `cladefsd_peer_served_parts_total{outcome}`, those the daemon's peers asked it for
//!   (`served`, `missing`), if it serves peers (TASK-406.2); `cladefsd_peer_asked_parts_total`,
//!   the parts asked, `cladefsd_peer_given_bytes_total`, the stored bytes of those `given`, counted
//!   once checked against their CRCs: the bytes the peers saved fetching from the bucket;
//!   and `cladefsd_peer_served_bytes_total`, those of the parts it `served`.
//! - cladefs: `cladefsd_part_fetches_total{source}` and `cladefsd_part_fetch_bytes_total{source}`:
//!   the parts the host's fetchers found, for its cells and its peers alike, and their stored
//!   bytes, by where (`memory`, `part_cache`, `peer`, `bucket`: a GET of a range); each found
//!   once, however many reads shared its fetch (`bucket_cell::fetch::Memory::fetches`).
//!   `cladefsd_tail_fetches_total{source}`, alike, the objects' tails they read (`part_cache`,
//!   `bucket`), which come from no peer: by no bytes, as one from the bucket may take two GETs, of
//!   more than it keeps.
//! - cladefs: `cladefsd_process_cpu_seconds_total`, the daemon's CPU time;
//!   `cladefsd_process_anon_bytes` and `cladefsd_process_anon_max_bytes`, its anonymous
//!   memory, as last sampled and at most; `cladefsd_process_anon_budget_bytes`, what its
//!   design bounds that by, and `cladefsd_process_anon_over_budget_total`, the samples past
//!   it, what an operator alerts on; `cladefsd_process_shmem_bytes`, its shared memory, the
//!   extent cache's pages (`super::process`, TASK-330.2). Each scrape samples them, so that they
//!   are the daemon's from its start, before any cell opens (board 8561).
//! - `cladefsd_object_storage_requests_total{request}`,
//!   `cladefsd_object_storage_uploaded_bytes_total{object}` and
//!   `cladefsd_object_storage_downloaded_bytes_total`: what the daemon did with its bucket;
//!   `cladefsd_object_storage_bucket_gets_total{bucket}` and
//!   `cladefsd_object_storage_bucket_downloaded_bytes_total{bucket}`, by bucket number, its own and
//!   those of other buckets' cells (TASK-633.4): 0 until a cell opens the bucket (board 8561),
//!   and each bucket's series once a cell asked it for something.
//!
//! cladefs: and, each from the daemon's start, 0 until it has any (board 8565):
//!
//! - `cladefsd_grpc_server_calls_total{service,method}`,
//!   `cladefsd_grpc_server_errors_total{service,method}` and
//!   `cladefsd_grpc_server_duration_seconds{service,method}`, a histogram: the gRPC calls the
//!   daemon served, of the control API (`cladefs.cladefsd.v1.Cladefsd`), its Mount service
//!   (`cladefs.mount.v1.Mount`) and the peer service (`cladefs.peer.v1.Peer`), by method, each of
//!   the .protos' and `unknown`, never one a caller names; those that failed, a status other than
//!   OK, or none, their stream cut; and how long they took, to their answer's end (a stream's
//!   whole life). `cladefsd_grpc_client_*`, alike, the calls it made of its peers
//!   (`super::grpc`).
//! - `cladefsd_migrations{state}`: live migrations under way, from this host `preparing` or
//!   `moving`, to it `taking`; `cladefsd_migrations_total{outcome}`, those from it `prepared`,
//!   `aborted` (their prepare failed), `accepted`, `refused` and `given_up` by their targets;
//!   `cladefsd_migration_rounds_total`, `cladefsd_migration_sent_bytes_total`,
//!   `cladefsd_migration_taken_bytes_total` and `cladefsd_migrations_taken_given_up_total`, their
//!   rounds, the bytes sent and taken, and those to it given up (`super::migration`).
//! - `cladefsd_mount_cells{state}`, the FUSE mount's cells `attached` and `detaching`;
//!   `cladefsd_mount_requests_total`, `cladefsd_mount_request_errors_total` (errors but ENOENT
//!   and ENOSYS), `cladefsd_mount_attaches_total` and `cladefsd_mount_detaches_total`
//!   (`super::mount`).
//! - `cladefsd_page_cache_bytes`, `cladefsd_page_cache_dirty_bytes`,
//!   `cladefsd_page_cache_writeback_bytes`, `cladefsd_page_cache_stuck_bytes`,
//!   `cladefsd_page_cache_limit_bytes` and `cladefsd_page_cache_dirty_limit_bytes`: the cells'
//!   page cache (`local_cell::pages`), its pages held, dirty, being written back and dirty of
//!   files failing, its size and the dirty bytes past which writers wait;
//!   `cladefsd_page_cache_flush_stalls_total` and `cladefsd_page_cache_flush_stalled_seconds_total`,
//!   the writers held so and how long; `cladefsd_page_cache_flusher_died`, 1 once its flusher died.
//! - `cladefsd_memory_full`, `cladefsd_memory_pressure`, `cladefsd_memory_thrashing`,
//!   `cladefsd_memory_floor_bytes`, `cladefsd_memory_target_bytes`,
//!   `cladefsd_memory_refaults_per_second`, `cladefsd_memory_psi_some_avg10_bps`,
//!   `cladefsd_memory_psi_full_avg10_bps`, `cladefsd_memory_full_entered_total`,
//!   `cladefsd_memory_refaults_total`, `cladefsd_memory_kernel_cold_harvested_bytes_total`,
//!   `cladefsd_vm_reclaim_bytes_total`, `cladefsd_vm_reclaim_attempts_total`, and
//!   `cladefsd_vm_reclaim_errors_total` (`super::governor`, `crate::cache`): also exposed in gRPC
//!   `GetStatsResponse.gauges` / `counters` so VM orchestrators can read live cache budget and
//!   backpressure state without scraping HTTP.
//! - `cladefsd_otel_spans_exported_total`, `cladefsd_otel_spans_dropped_total` and
//!   `cladefsd_otel_spans_failed_total`: the spans exported over OTLP, dropped (the exporter's
//!   queue full) and whose export failed (`super::otel::batch`).

use crate::Daemon;
use crate::wire::*;
use bucket_cell::trace::Source;
use std::fmt::Write as _;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The kinds of request metrics are kept of, and their names.
const KINDS: [(u32, &str); 16] = [
    (OPEN, "open"),
    (READ, "read"),
    (WRITE, "write"),
    (FREE, "free"),
    (MAP, "map"),
    (UNMAP, "unmap"),
    (COMMIT, "commit"),
    (ROOT, "root"),
    (INFO, "info"),
    (WINDOW, "window"),
    (FAULT, "fault"),
    (FORK, "fork"),
    (RESIZED, "resized"),
    (MAPPINGS, "mappings"),
    (TRANSFER, "transfer"),
    // cladefs: TASK-111.4.
    (STATS, "stats"),
];

/// The upper bounds of the requests' duration histograms' buckets, in seconds: from what the
/// fastest take (tens of microseconds) to what durable commits may (a second).
const BUCKETS: [f64; 14] = [
    10e-6, 25e-6, 50e-6, 100e-6, 250e-6, 500e-6, 1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 100e-3, 250e-3,
    1.0,
];

/// cladefs: what took some time, as a histogram's buckets count it, of the upper bounds `bounds`, in
/// seconds: how many took at most each bound, and longer than the last, their nanoseconds, and how
/// many failed: the VM processes' requests of a kind ([`REQUESTS`]), a gRPC method's calls
/// (`super::grpc`). No count of them all apart, which a scrape would read at another moment than
/// the buckets, a bucket's cumulative count then past it (board 8561): the count is the buckets'
/// sum, as one read of them has them ([`Timed::counts`]).
pub(super) struct Timed {
    bounds: &'static [f64],
    /// Each of `bounds`' (those over the bound before it), then those past them all.
    buckets: Box<[AtomicU64]>,
    nanos: AtomicU64,
    errors: AtomicU64,
}

impl Timed {
    /// None yet, in buckets of `bounds`, ascending.
    pub(super) fn new(bounds: &'static [f64]) -> Self {
        let buckets = (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect();
        Self { bounds, buckets, nanos: AtomicU64::new(0), errors: AtomicU64::new(0) }
    }

    /// Counts one that took `took`, and whether it `failed`.
    pub(super) fn record(&self, took: Duration, failed: bool) {
        let seconds = took.as_secs_f64();
        let bucket = self.bounds.iter().position(|bound| seconds <= *bound);
        self.buckets[bucket.unwrap_or(self.bounds.len())].fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
        if failed {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How many took at most each bound, of those over the bound before it, and how many in all,
    /// as of one read of each bucket.
    fn counts(&self) -> (Vec<u64>, u64) {
        let read: Vec<u64> =
            self.buckets.iter().map(|bucket| bucket.load(Ordering::Relaxed)).collect();
        let count = read.iter().sum();
        (read[..self.bounds.len()].to_vec(), count)
    }

    /// How many failed.
    fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Their nanoseconds.
    fn nanos(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }

    /// Its series of the histogram `name`, labelled `labels` (`KEY="VALUE",...`), appended to
    /// `out`, as of `counts`, one read of it ([`Timed::counts`]): `+Inf` and `_count` its count.
    fn render(
        &self,
        out: &mut String,
        name: &str,
        labels: &str,
        (within, count): &(Vec<u64>, u64),
    ) {
        let mut cumulative = 0;
        for (bound, bucket) in self.bounds.iter().zip(within) {
            cumulative += bucket;
            let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{bound}\"}} {cumulative}");
        }
        let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {count}");
        let sum = self.nanos() as f64 / 1e9;
        let _ = writeln!(out, "{name}_sum{{{labels}}} {sum}");
        let _ = writeln!(out, "{name}_count{{{labels}}} {count}");
    }
}

/// Each kind's requests, in [`KINDS`]' order.
// The process's count, every simulated host's together, as a process's metrics are.
static REQUESTS: LazyLock<[Timed; KINDS.len()]> =
    LazyLock::new(|| std::array::from_fn(|_| Timed::new(&BUCKETS)));

/// The name of the kind of request `kind` ([`KINDS`]), if it is one.
pub(super) fn kind_name(kind: u32) -> Option<&'static str> {
    KINDS.iter().find(|(known, _)| *known == kind).map(|(_, name)| *name)
}

/// cladefs: the name of the span of a request of `kind`, `cladefsd.NAME` ([`kind_name`]), or
/// `cladefsd.request` of a kind it isn't (`super::otel`, TASK-258).
pub(super) fn span_name(kind: u32) -> &'static str {
    // Immutable once made: the same for every simulated host of the process.
    static NAMES: LazyLock<[String; KINDS.len()]> =
        LazyLock::new(|| KINDS.map(|(_, name)| format!("cladefsd.{name}")));
    match KINDS.iter().position(|(known, _)| *known == kind) {
        Some(at) => &NAMES[at],
        None => "cladefsd.request",
    }
}

/// Counts a request of `kind` served, which took `took`, and whether it `failed`.
pub(super) fn served(kind: u32, took: Duration, failed: bool) {
    let Some(at) = KINDS.iter().position(|(known, _)| *known == kind) else { return };
    REQUESTS[at].record(took, failed);
}

/// The requests served, as a line of `NAME=COUNT` after `requests:`: of each kind served, how
/// many (`KIND`) and their nanoseconds (`KIND-ns`), which the daemon's stats file keeps
/// (`super::cells`).
pub(super) fn requests_line() -> String {
    let mut line = String::from("requests:");
    for ((_, kind), requests) in KINDS.iter().zip(REQUESTS.iter()) {
        let (_, count) = requests.counts();
        if count > 0 {
            let nanos = requests.nanos();
            let _ = write!(line, " {kind}={count} {kind}-ns={nanos}");
        }
    }
    line
}

/// cladefs: how long a render of the metrics takes, said by its parts once the whole took longer
/// than [`SLOW_RENDER`]: a scrape is answered at once, or its scraper gives up, as the case
/// measuring cells per host did after 10 s, 161 VMs restored on a loaded disk (board 7416).
const SLOW_RENDER: Duration = Duration::from_secs(1);

/// cladefs: the parts of a render of the metrics, each as long as it took ([`SLOW_RENDER`]).
struct Laps {
    began: Instant,
    last: Instant,
    parts: Vec<(&'static str, Duration)>,
}

impl Laps {
    fn new() -> Self {
        let now = Instant::now();
        Self { began: now, last: now, parts: Vec::new() }
    }

    /// The part `part` is done.
    fn lap(&mut self, part: &'static str) {
        let now = Instant::now();
        self.parts.push((part, now - self.last));
        self.last = now;
    }

    /// The last part, `part`, is done: the render's parts said if it took longer than
    /// [`SLOW_RENDER`].
    fn said(mut self, part: &'static str) {
        self.lap(part);
        let took = self.last - self.began;
        if took > SLOW_RENDER {
            let parts: Vec<String> =
                self.parts.iter().map(|(part, took)| format!("{part} {took:.1?}")).collect();
            log::warn!("cladefsd: rendering the metrics took {took:.1?}: {}", parts.join(", "));
        }
    }
}

/// The metrics of `daemon` as of now, in Prometheus' text format.
pub(super) fn render_on(daemon: &Daemon) -> String {
    let mut out = String::new();
    let mut laps = Laps::new();
    render_requests(&mut out);
    laps.lap("requests");
    render_rest(daemon, out, laps)
}

/// The metrics of the process's default daemon as of now, in Prometheus' text format.
#[cfg(not(madsim))]
pub fn render() -> String {
    render_on(Daemon::process())
}

/// The VM processes' requests' metrics, appended to `out`: each kind's, its histogram's buckets,
/// `+Inf` and count as of one read of its buckets.
fn render_requests(out: &mut String) {
    let counts: Vec<_> = REQUESTS.iter().map(Timed::counts).collect();
    family(out, "cladefsd_requests_total", "counter", "VM processes' requests served, by kind.");
    for ((_, kind), (_, count)) in KINDS.iter().zip(&counts) {
        let _ = writeln!(out, "cladefsd_requests_total{{kind=\"{kind}\"}} {count}");
    }
    let name = "cladefsd_request_errors_total";
    family(out, name, "counter", "VM processes' requests that failed, by kind.");
    for ((_, kind), requests) in KINDS.iter().zip(REQUESTS.iter()) {
        let _ = writeln!(out, "{name}{{kind=\"{kind}\"}} {}", requests.errors());
    }
    let name = "cladefsd_request_duration_seconds";
    let help = "How long VM processes' requests took to serve, from their dispatch to their reply, \
                by kind.";
    family(out, name, "histogram", help);
    for (((_, kind), requests), counts) in KINDS.iter().zip(REQUESTS.iter()).zip(&counts) {
        requests.render(out, name, &format!("kind=\"{kind}\""), counts);
    }
}

/// cladefs: the `# HELP` and `# TYPE` lines of the family `name`, of `kind`, appended to `out`.
fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// The metrics but the requests', appended to `out`, each part lapped: `out`.
fn render_rest(daemon: &Daemon, mut out: String, mut laps: Laps) -> String {
    let name = "cladefsd_build_info";
    let _ = writeln!(out, "# HELP {name} The daemon's build, by the commit it was built of.");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name}{{commit=\"{}\"}} 1", label_value(cladefs_core::BUILD_COMMIT));
    let name = "cladefsd_vm_build_info";
    let _ = writeln!(
        out,
        "# HELP {name} VM processes connected, by their cell and the commit they were \
         built of, as they said."
    );
    let _ = writeln!(out, "# TYPE {name} gauge");
    for ((cell, commit), count) in crate::stats::vm_builds_on(daemon) {
        // cladefs: the commit as its VM process said it, which it may have said with anything.
        let commit = label_value(&commit);
        let _ = writeln!(out, "{name}{{cell=\"{cell}\",commit=\"{commit}\"}} {count}");
    }

    laps.lap("builds");
    for (name, help, value) in gauges_on(daemon) {
        let _ = writeln!(out, "# HELP cladefsd_{name} {help}");
        let _ = writeln!(out, "# TYPE cladefsd_{name} gauge");
        let _ = writeln!(out, "cladefsd_{name} {value}");
    }
    laps.lap("gauges");
    // cladefs: a tripwire (backlog doc-13): no guest maps an open extent.
    let name = "cladefsd_window_open_maps_refused_total";
    let _ = writeln!(out, "# HELP {name} MAPs of open extents refused.");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {}", cladefs_core::open_cell::open_maps_refused());

    // cladefs: each cell's VM process's (TASK-111.5, board 8563).
    let vms = crate::stats::vms_on(daemon);
    let series: [(&str, &str, Gauge); 6] = [
        ("vm_window_mappings", "Ranges a VM's DAX window maps.", |vm| Some(vm.window?.mappings)),
        ("vm_window_mapped_bytes", "Bytes those ranges span.", |vm| Some(vm.window?.mapped_bytes)),
        ("vm_window_pages", "Pages a VM's DAX window has mapped in its VM process.", |vm| {
            Some(vm.window?.pages)
        }),
        (
            "vm_window_private_pages",
            "Private copies of pages the guest wrote, as its VM process last said.",
            |vm| vm.window.is_some().then_some(vm.stage.private),
        ),
        (
            "vm_host_mappings",
            "Runs of pages a VM process's window has installed in its memory, each a mapping, as \
             it last said.",
            |vm| vm.window.is_some().then_some(vm.stage.installed),
        ),
        (
            "vm_host_mapping_budget",
            "The most runs of pages a VM process's window keeps installed now, its share of the \
             process's windows', as it last said.",
            |vm| vm.budget,
        ),
    ];
    for (name, help, value) in series {
        let _ = writeln!(out, "# HELP cladefsd_{name} {help}");
        let _ = writeln!(out, "# TYPE cladefsd_{name} gauge");
        for (cell, value) in by_cell(&vms, value) {
            let _ = writeln!(out, "cladefsd_{name}{{cell=\"{cell}\"}} {value}");
        }
    }

    laps.lap("each VM's");
    let tenants = crate::memory::tenants_on(daemon);
    let name = "cladefsd_tenant_memory_bytes";
    let _ = writeln!(out, "# HELP {name} Memory a tenant's extents' pages hold, by its ID.");
    let _ = writeln!(out, "# TYPE {name} gauge");
    for tenant in &tenants {
        let _ = writeln!(
            out,
            "{name}{{tenant=\"{}\"}} {}",
            base32_id::Short(tenant.tenant),
            tenant.bytes
        );
    }
    let name = "cladefsd_tenant_memory_limit_bytes";
    let _ = writeln!(out, "# HELP {name} The limit of a tenant's memory, by its ID.");
    let _ = writeln!(out, "# TYPE {name} gauge");
    for tenant in &tenants {
        if let Some(limit) = tenant.limit {
            let _ =
                writeln!(out, "{name}{{tenant=\"{}\"}} {limit}", base32_id::Short(tenant.tenant));
        }
    }
    // cladefs: their disks' (TASK-94).
    let tenants = crate::disk::tenants_on(daemon);
    let name = "cladefsd_tenant_disk_bytes";
    let _ = writeln!(
        out,
        "# HELP {name} The host's disk a tenant's directory takes, as last counted, by its ID."
    );
    let _ = writeln!(out, "# TYPE {name} gauge");
    for tenant in &tenants {
        let _ = writeln!(
            out,
            "{name}{{tenant=\"{}\"}} {}",
            base32_id::Short(tenant.tenant),
            tenant.bytes
        );
    }
    let name = "cladefsd_tenant_disk_limit_bytes";
    let _ = writeln!(out, "# HELP {name} The limit of a tenant's disk, by its ID.");
    let _ = writeln!(out, "# TYPE {name} gauge");
    for tenant in &tenants {
        if let Some(limit) = tenant.limit {
            let _ =
                writeln!(out, "{name}{{tenant=\"{}\"}} {limit}", base32_id::Short(tenant.tenant));
        }
    }
    // cladefs: what they hold past it, their weights and shares (doc-22).
    type Of = fn(&crate::disk::TenantDisk) -> Option<u64>;
    let per_tenant: [(&str, &str, Of); 3] = [
        (
            "cladefsd_tenant_disk_reserved_bytes",
            "What a tenant's disk holds past what it takes against its limit, by its ID.",
            |tenant| Some(tenant.reserved),
        ),
        (
            "cladefsd_tenant_disk_weight",
            "A tenant's disk's weight against the others', by its ID.",
            |tenant| Some(tenant.weight),
        ),
        (
            "cladefsd_tenant_disk_share_bytes",
            "What a tenant's disk keeps while its disk is under pressure, by its ID.",
            |tenant| tenant.share,
        ),
    ];
    for (name, help, of) in per_tenant {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for tenant in &tenants {
            if let Some(value) = of(tenant) {
                let id = base32_id::Short(tenant.tenant);
                let _ = writeln!(out, "{name}{{tenant=\"{id}\"}} {value}");
            }
        }
    }
    laps.lap("tenants");
    // cladefs: the host's disks, as the disk governor sees them (doc-22).
    for (name, help, values) in crate::disk::host_disks_on(daemon) {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (device, value) in values {
            let _ = writeln!(out, "{name}{{device=\"{device}\"}} {value}");
        }
    }

    laps.lap("host disks");
    for (name, value) in cladefs_core::stats::stats().named() {
        let name = format!("cladefsd_{}_total", name.replace('-', "_"));
        let _ = writeln!(out, "# HELP {name} As the control API's GetCounters reports it.");
        let _ = writeln!(out, "# TYPE {name} counter");
        let _ = writeln!(out, "{name} {value}");
    }

    // cladefs: what the uploads hold of the objects they upload (TASK-317).
    let memory = cladefs_core::bucket::upload_memory_on(&daemon.host);
    for (name, help, value) in [
        (
            "cladefsd_upload_held_bytes",
            "Bytes of objects the cells' uploads hold in memory now, every cell's together.",
            memory.held,
        ),
        (
            "cladefsd_upload_held_max_bytes",
            "The most bytes of objects the cells' uploads held in memory at once.",
            memory.max_held,
        ),
        (
            "cladefsd_upload_memory_bytes",
            "The most bytes of objects the cells' uploads may hold in memory at once.",
            memory.budget,
        ),
    ] {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = writeln!(out, "{name} {value}");
    }

    // cladefs: what the part cache keeps, and its budget (TASK-406.1): 0 until a cell opens it, or
    // if there is none, so that its series are there from the start (board 8561).
    let cache = crate::storage::opened_cache(daemon);
    let (bytes, capacity) =
        cache.as_ref().map_or((0, 0), |cache| (cache.bytes(), cache.capacity()));
    for (name, help, value) in [
        ("cladefsd_part_cache_bytes", "Bytes of objects the host's part cache keeps.", bytes),
        (
            "cladefsd_part_cache_capacity_bytes",
            "The most bytes of objects the host's part cache keeps.",
            capacity,
        ),
    ] {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = writeln!(out, "{name} {value}");
    }
    let name = "cladefsd_part_cache_shard_state";
    let _ = writeln!(
        out,
        "# HELP {name} Health state of each JBOD part cache shard (0=online, 1=suspect, 2=offline, 3=returning)."
    );
    let _ = writeln!(out, "# TYPE {name} gauge");
    if let Some(cache) = &cache {
        for (index, (disk, state)) in cache.shard_states().into_iter().enumerate() {
            let Some(dev) = disk.dev() else { continue };
            let device =
                format!("{}:{}", libc::major(dev as libc::dev_t), libc::minor(dev as libc::dev_t));
            let _ =
                writeln!(out, "{name}{{device=\"{device}\",shard=\"{index}\"}} {}", state.as_u64());
        }
    }

    // cladefs: the parts asked of peers, and served them (TASK-406.2).
    if let Some((counts, served)) = crate::peers::counts_on(daemon) {
        let name = "cladefsd_peer_parts_total";
        let _ = writeln!(out, "# HELP {name} Parts asked of the peers that own them, by outcome.");
        let _ = writeln!(out, "# TYPE {name} counter");
        for (outcome, value) in
            [("given", counts.given), ("missing", counts.missing), ("failed", counts.failed)]
        {
            let _ = writeln!(out, "{name}{{outcome=\"{outcome}\"}} {value}");
        }
        let name = "cladefsd_peer_served_parts_total";
        let _ = writeln!(out, "# HELP {name} Parts the daemon's peers asked it for, by outcome.");
        let _ = writeln!(out, "# TYPE {name} counter");
        for (outcome, value) in [("served", served.parts), ("missing", served.missing)] {
            let _ = writeln!(out, "{name}{{outcome=\"{outcome}\"}} {value}");
        }
        for (name, help, value) in [
            (
                "cladefsd_peer_asked_parts_total",
                "Parts asked of the peers that own them.",
                counts.asked,
            ),
            (
                "cladefsd_peer_given_bytes_total",
                "Stored bytes of the parts the peers that own them gave, checked.",
                counts.given_bytes,
            ),
            (
                "cladefsd_peer_served_bytes_total",
                "Stored bytes of the parts the daemon served its peers.",
                served.bytes,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {value}");
        }
    }

    // cladefs: where the host's fetchers found the parts and tails they fetched.
    let fetches = cladefs_core::bucket::memory_on(&daemon.host).fetches();
    let parts = Source::ALL.map(|source| (source, fetches.parts(source)));
    let tails = [Source::PartCache, Source::Bucket].map(|source| (source, fetches.tails(source)));
    for (name, help, values) in [
        (
            "cladefsd_part_fetches_total",
            "Parts the host's fetchers found, by where.",
            parts.map(|(source, found)| (source, found.count)).to_vec(),
        ),
        (
            "cladefsd_part_fetch_bytes_total",
            "Stored bytes of the parts the host's fetchers found, by where.",
            parts.map(|(source, found)| (source, found.bytes)).to_vec(),
        ),
        (
            "cladefsd_tail_fetches_total",
            "Objects' tails the host's fetchers read, by where.",
            tails.to_vec(),
        ),
    ] {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} counter");
        for (source, value) in values {
            let _ = writeln!(out, "{name}{{source=\"{}\"}} {value}", source.name());
        }
    }

    // cladefs: what the process holds and spent, against its budget (TASK-330.2), sampled now: the
    // stats' thread samples it only once a cell opened the bucket, and until then it would read 0,
    // an idle daemon's, to whoever alerts on it (board 8561, hotbed question 504).
    let process = crate::process::sample_on(daemon);
    let series: [(&str, &str, &str, String); 6] = [
        (
            "cladefsd_process_cpu_seconds_total",
            "counter",
            "The daemon's CPU time, user and system, in seconds.",
            (process.cpu_ns as f64 / 1e9).to_string(),
        ),
        (
            "cladefsd_process_anon_bytes",
            "gauge",
            "The daemon's anonymous memory resident, as last sampled.",
            process.anon.to_string(),
        ),
        (
            "cladefsd_process_anon_max_bytes",
            "gauge",
            "The most anonymous memory the daemon held, of its samples.",
            process.max_anon.to_string(),
        ),
        (
            "cladefsd_process_anon_budget_bytes",
            "gauge",
            "What the daemon's design bounds its anonymous memory by, as last sampled.",
            process.anon_budget.to_string(),
        ),
        (
            "cladefsd_process_anon_over_budget_total",
            "counter",
            "Samples of the daemon's anonymous memory past its budget.",
            process.over_budget.to_string(),
        ),
        (
            "cladefsd_process_shmem_bytes",
            "gauge",
            "The daemon's shared memory resident, its extent cache's pages, as last sampled.",
            process.shmem.to_string(),
        ),
    ];
    for (name, kind, help, value) in series {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
        let _ = writeln!(out, "{name} {value}");
    }

    laps.lap("stats, uploads' memory, the part cache, peers and the process");
    let uploads = crate::cells::upload_statuses(daemon);
    let series: [(&str, &str, &str, fn(&local_cell::upload::UploadStatus) -> String); 3] = [
        (
            "upload_pending_bytes",
            "gauge",
            "Bytes of a cell's commits not yet uploaded, by its cell.",
            |status| status.pending_bytes.to_string(),
        ),
        (
            "upload_oldest_pending_seconds",
            "gauge",
            "How long a cell's oldest commit not yet uploaded has waited, by its cell.",
            |status| {
                // A clock gone back: none waited yet.
                let waited = status.oldest_pending.and_then(|since| since.elapsed().ok());
                waited.unwrap_or_default().as_secs_f64().to_string()
            },
        ),
        (
            "upload_stalls_total",
            "counter",
            "A cell's commits that waited for its upload lag to shrink, by its cell.",
            |status| status.stalls.to_string(),
        ),
    ];
    for (name, kind, help, value) in series {
        let _ = writeln!(out, "# HELP cladefsd_{name} {help}");
        let _ = writeln!(out, "# TYPE cladefsd_{name} {kind}");
        for (cell, status) in &uploads {
            let _ = writeln!(out, "cladefsd_{name}{{cell=\"{cell}\"}} {}", value(status));
        }
    }

    laps.lap("uploads");

    // cladefs: 0 until a cell opens the bucket, so that its series are there from the start
    // (board 8561).
    {
        let stats = crate::storage::bucket_stats(daemon).unwrap_or_default();
        use bucket_cell::object_bucket::{Request, Upload};
        let name = "cladefsd_object_storage_requests_total";
        let _ = writeln!(out, "# HELP {name} Requests to the bucket, by kind.");
        let _ = writeln!(out, "# TYPE {name} counter");
        for request in Request::ALL {
            let _ = writeln!(
                out,
                "{name}{{request=\"{}\"}} {}",
                request.name(),
                stats.requests(request)
            );
        }
        let name = "cladefsd_object_storage_failures_total";
        let _ = writeln!(
            out,
            "# HELP {name} Requests to the bucket that failed, unanswered, by kind: each retried, \
             its error in the log."
        );
        let _ = writeln!(out, "# TYPE {name} counter");
        for request in Request::ALL {
            let _ = writeln!(
                out,
                "{name}{{request=\"{}\"}} {}",
                request.name(),
                stats.failures(request)
            );
        }
        let name = "cladefsd_object_storage_uploaded_bytes_total";
        let _ = writeln!(out, "# HELP {name} Bytes uploaded to the bucket, by kind of object.");
        let _ = writeln!(out, "# TYPE {name} counter");
        for upload in Upload::ALL {
            let _ =
                writeln!(out, "{name}{{object=\"{}\"}} {}", upload.name(), stats.uploaded(upload));
        }
        let name = "cladefsd_object_storage_downloaded_bytes_total";
        let _ = writeln!(out, "# HELP {name} Bytes downloaded from the bucket.");
        let _ = writeln!(out, "# TYPE {name} counter");
        let _ = writeln!(out, "{name} {}", stats.downloaded);
        // cladefs: by bucket, the daemon's own and those of other buckets' cells (TASK-633.4):
        // what crosses to another cloud region shows here.
        let by_bucket = crate::storage::bucket_stats_by_bucket(daemon);
        let name = "cladefsd_object_storage_bucket_gets_total";
        let _ = writeln!(out, "# HELP {name} GETs of each bucket, by its number.");
        let _ = writeln!(out, "# TYPE {name} counter");
        for (number, stats) in &by_bucket {
            let gets = stats.requests(Request::Get) + stats.requests(Request::GetRange);
            let _ = writeln!(out, "{name}{{bucket=\"{number}\"}} {gets}");
        }
        let name = "cladefsd_object_storage_bucket_downloaded_bytes_total";
        let _ = writeln!(out, "# HELP {name} Bytes downloaded from each bucket, by its number.");
        let _ = writeln!(out, "# TYPE {name} counter");
        for (number, stats) in &by_bucket {
            let _ = writeln!(out, "{name}{{bucket=\"{number}\"}} {}", stats.downloaded);
        }
    }
    laps.lap("replication");
    render_grpc(&mut out);
    laps.lap("gRPC calls");
    render_subsystems(daemon, &mut out);
    laps.said("migrations, the mount, the page cache, the governor and spans");
    out
}

/// cladefs: the gRPC calls' metrics, appended to `out` (board 8565): each method's of each service,
/// served, and made of the service the daemon calls, its peers', from the daemon's start; each's
/// count and histogram as of one read of its buckets.
fn render_grpc(out: &mut String) {
    for (side, what) in [("server", "served"), ("client", "made")] {
        let series: Vec<_> = crate::grpc::calls()
            .flat_map(|calls| {
                calls.methods().filter_map(move |(method, served, made)| {
                    let timed = if side == "server" { served } else { made? };
                    let labels = format!("service=\"{}\",method=\"{method}\"", calls.service());
                    Some((labels, timed, timed.counts()))
                })
            })
            .collect();
        let name = format!("cladefsd_grpc_{side}_calls_total");
        family(out, &name, "counter", &format!("gRPC calls {what}, by service and method."));
        for (labels, _, (_, count)) in &series {
            let _ = writeln!(out, "{name}{{{labels}}} {count}");
        }
        let name = format!("cladefsd_grpc_{side}_errors_total");
        let help = format!(
            "gRPC calls {what} that failed, a status other than OK or none, by service and method."
        );
        family(out, &name, "counter", &help);
        for (labels, timed, _) in &series {
            let _ = writeln!(out, "{name}{{{labels}}} {}", timed.errors());
        }
        let name = format!("cladefsd_grpc_{side}_duration_seconds");
        let help = format!(
            "How long gRPC calls {what} took, to their answer's end, by service and method."
        );
        family(out, &name, "histogram", &help);
        for (labels, timed, counts) in &series {
            timed.render(out, &name, labels, counts);
        }
    }
}

/// cladefs: one series of no labels, `name`, of `kind`, with its family's lines, appended to `out`.
fn single(out: &mut String, name: &str, kind: &str, help: &str, value: impl std::fmt::Display) {
    family(out, name, kind, help);
    let _ = writeln!(out, "{name} {value}");
}

/// cladefs: the metrics of the daemon's live migrations, its FUSE mount of cells, its page cache,
/// its memory governor's state and its spans exported, appended to `out` (board 8565): each there
/// from the daemon's start, 0 until it has any.
fn render_subsystems(daemon: &Daemon, out: &mut String) {
    use std::sync::atomic::Ordering::Relaxed;
    let (preparing, moving, taking) = crate::migration::under_way_on(daemon);
    let name = "cladefsd_migrations";
    let help = "Migrations under way: from this host, being prepared, or moving (its rounds, then \
                its hand-over); to it, being taken.";
    family(out, name, "gauge", help);
    for (state, count) in [("preparing", preparing), ("moving", moving), ("taking", taking)] {
        let _ = writeln!(out, "{name}{{state=\"{state}\"}} {count}");
    }
    let counts = &crate::migration::COUNTS;
    let name = "cladefsd_migrations_total";
    let help = "Migrations from this host, by what came of them: prepared at their targets, \
                aborted as their prepare failed, accepted and refused at their hand-overs, and \
                given up by their targets.";
    family(out, name, "counter", help);
    for (outcome, count) in [
        ("prepared", &counts.prepared),
        ("aborted", &counts.aborted),
        ("accepted", &counts.accepted),
        ("refused", &counts.refused),
        ("given_up", &counts.given_up),
    ] {
        let _ = writeln!(out, "{name}{{outcome=\"{outcome}\"}} {}", count.load(Relaxed));
    }
    let migration: [(&str, &str, u64); 4] = [
        (
            "cladefsd_migration_rounds_total",
            "Rounds of migrations from this host, each sending the kept segments not sent yet.",
            counts.rounds.load(Relaxed),
        ),
        (
            "cladefsd_migration_sent_bytes_total",
            "Bytes of cells' files migrations from this host sent.",
            counts.sent_bytes.load(Relaxed),
        ),
        (
            "cladefsd_migration_taken_bytes_total",
            "Bytes of cells' files migrations to this host wrote.",
            counts.taken_bytes.load(Relaxed),
        ),
        (
            "cladefsd_migrations_taken_given_up_total",
            "Migrations to this host given up: aborted by their sources, or idle too long.",
            counts.taken_given_up.load(Relaxed),
        ),
    ];
    for (name, help, value) in migration {
        single(out, name, "counter", help, value);
    }

    let (attached, detaching, mount) = crate::mount::counts_on(daemon);
    let name = "cladefsd_mount_cells";
    family(out, name, "gauge", "Cells the FUSE mount of cells holds, attached or detaching.");
    for (state, count) in [("attached", attached), ("detaching", detaching)] {
        let _ = writeln!(out, "{name}{{state=\"{state}\"}} {count}");
    }
    let mounted: [(&str, &str, u64); 4] = [
        (
            "cladefsd_mount_requests_total",
            "The kernel's requests the FUSE mount served.",
            mount.requests,
        ),
        (
            "cladefsd_mount_request_errors_total",
            "The FUSE mount's requests answered with an error but ENOENT and ENOSYS.",
            mount.errors,
        ),
        ("cladefsd_mount_attaches_total", "Cells the FUSE mount attached.", mount.attaches),
        ("cladefsd_mount_detaches_total", "Cells the FUSE mount detached.", mount.detaches),
    ];
    for (name, help, value) in mounted {
        single(out, name, "counter", help, value);
    }

    let pages = daemon.host.local().pages();
    let (usage, options) = (pages.usage(), pages.options());
    let gauges: [(&str, &str, u64); 6] = [
        ("cladefsd_page_cache_bytes", "Bytes of pages the cells' page cache holds.", usage.held),
        (
            "cladefsd_page_cache_dirty_bytes",
            "Bytes of the page cache's pages dirty: written, not yet on the disk.",
            usage.dirty,
        ),
        (
            "cladefsd_page_cache_writeback_bytes",
            "Bytes of the page cache's pages being written back.",
            usage.writing,
        ),
        (
            "cladefsd_page_cache_stuck_bytes",
            "Bytes dirty of files whose write-back is failing, which no writer waits for.",
            usage.stuck,
        ),
        (
            "cladefsd_page_cache_limit_bytes",
            "The most bytes the page cache holds, as the governor sizes it.",
            options.most,
        ),
        (
            "cladefsd_page_cache_dirty_limit_bytes",
            "Bytes dirty past which writers wait for write-back: flush stalls.",
            options.hard,
        ),
    ];
    for (name, help, value) in gauges {
        single(out, name, "gauge", help, value);
    }
    single(
        out,
        "cladefsd_page_cache_flusher_died",
        "gauge",
        "1 once the page cache's flusher died: nothing writes back for it since.",
        u8::from(pages.flusher_died()),
    );
    single(
        out,
        "cladefsd_page_cache_flush_stalls_total",
        "counter",
        "Writers held back past the dirty limit, waiting for write-back.",
        usage.stalls,
    );
    single(
        out,
        "cladefsd_page_cache_flush_stalled_seconds_total",
        "counter",
        "How long writers were held back past the dirty limit, together, in seconds.",
        usage.stalled_ns as f64 / 1e9,
    );

    let governed = crate::governor::governed_on(daemon);
    single(
        out,
        "cladefsd_memory_full_entered_total",
        "counter",
        "Times the host's memory became full.",
        governed.fulls,
    );
    single(
        out,
        "cladefsd_memory_refaults_total",
        "counter",
        "Cached extent parts evicted within the refault window that faulted back into memory.",
        daemon.host.cache().refaults(),
    );
    single(
        out,
        "cladefsd_memory_kernel_cold_harvested_bytes_total",
        "counter",
        "Cached extent bytes harvested via cachestat(2) after MGLRU paged them out to swap.",
        governed.kernel_cold_harvested_bytes,
    );
    single(
        out,
        "cladefsd_vm_reclaim_bytes_total",
        "counter",
        "Bytes proactively reclaimed from low-PSI connected VM cgroups via memory.reclaim.",
        governed.vm_reclaim_bytes,
    );
    single(
        out,
        "cladefsd_vm_reclaim_attempts_total",
        "counter",
        "Proactive memory.reclaim writes issued against connected VM cgroups.",
        governed.vm_reclaim_attempts,
    );
    single(
        out,
        "cladefsd_vm_reclaim_errors_total",
        "counter",
        "Proactive memory.reclaim writes that failed with an error other than EAGAIN.",
        governed.vm_reclaim_errors,
    );

    let (exported, dropped, failed) = crate::otel::spans();
    let spans: [(&str, &str, u64); 3] = [
        ("cladefsd_otel_spans_exported_total", "Spans exported over OTLP.", exported),
        (
            "cladefsd_otel_spans_dropped_total",
            "Spans dropped unexported: the exporter's queue full, or shut down.",
            dropped,
        ),
        (
            "cladefsd_otel_spans_failed_total",
            "Spans whose export over OTLP failed: refused, or unanswered.",
            failed,
        ),
    ];
    for (name, help, value) in spans {
        single(out, name, "counter", help, value);
    }
}

/// cladefs: a gauge of each VM process, if it has it.
type Gauge = fn(&crate::stats::Vm) -> Option<u64>;

/// cladefs: `value` of `vms`, summed by their cell (board 8563): saturated, as what VM processes
/// said, which may be anything, a window's budget `u64::MAX`, would wrap, or panic a scrape.
fn by_cell(
    vms: &[crate::stats::Vm],
    value: Gauge,
) -> std::collections::BTreeMap<cladefs_core::CellId, u64> {
    let mut summed = std::collections::BTreeMap::new();
    for vm in vms {
        if let Some(value) = value(vm) {
            let sum: &mut u64 = summed.entry(vm.cell).or_insert(0);
            *sum = sum.saturating_add(value);
        }
    }
    summed
}

/// The gauges of `daemon` as of now.
pub(super) fn gauges_on(daemon: &Daemon) -> Vec<(&'static str, &'static str, u64)> {
    let slabs = daemon.host.slabs();
    let (entries, live) = (daemon.host.cache().entries(), cladefs_core::dax::live());
    let mut gauges = vec![
        ("connections", "VM processes connected.", crate::server::connections_on(daemon) as u64),
        (
            "stores_open",
            "Cells VM processes' connections write.",
            crate::cells::open_count(daemon) as u64,
        ),
        (
            "extent_cache_pinned_entries",
            "Sealed extents cached that a window's mapping, a read or a page-in holds.",
            entries.pinned,
        ),
        (
            "extent_cache_idle_entries",
            "Sealed extents cached that nothing but the cache holds.",
            entries.idle,
        ),
        ("window_mappings", "Ranges the VMs' DAX windows map, all windows'.", live.mappings),
        ("window_mapped_bytes", "Bytes those ranges span.", live.mapped_bytes),
        (
            "window_pages",
            "Pages the VMs' DAX windows have mapped in their VM processes, all windows'.",
            live.pages,
        ),
        (
            "window_private_pages",
            "Private copies of pages the guests wrote, as their VM processes last said.",
            // cladefs: as VM processes said, which may say anything: saturated, not wrapped.
            (crate::stats::vms_on(daemon).iter())
                .filter(|vm| vm.window.is_some())
                .fold(0, |sum, vm| sum.saturating_add(vm.stage.private)),
        ),
        ("memory_bytes", "Memory extents' parts hold, every tenant's.", slabs.arenas_bytes()),
        ("memory_capacity_bytes", "The most memory the daemon has.", slabs.count_total_bytes()),
        (
            "memory_leaked_bytes",
            "Memory of runs freed whose punch failed, counted still: what should never be.",
            slabs.leaked_bytes(),
        ),
        // cladefs: what a page-in finding no memory has to go on (board 5662).
        (
            "memory_free_bytes",
            "Of the daemon's memory, what no arena holds in use.",
            slabs.count_total_bytes().saturating_sub(slabs.arenas_bytes_in_use()),
        ),
        (
            "memory_quarantined_bytes",
            "Memory of runs freed and quarantined, waiting for VM processes to unmap them.",
            slabs.quarantined_bytes(),
        ),
        (
            "memory_exhausted_arenas",
            "Arenas whose relief found nothing to free: their allocations fail.",
            slabs.exhausted_arenas(),
        ),
        // cladefs: board 8535.
        (
            "vm_processes_wedged",
            "VM processes the daemon killed that have not exited, as in uninterruptible sleep.",
            crate::server::wedged_on(daemon),
        ),
        (
            "memory_wedged_bytes",
            "Memory quarantined for wedged VM processes, which comes back only once they exit.",
            slabs.wedged_bytes(),
        ),
        // cladefs: board 8465.
        (
            "in_flight_bytes",
            "What every tenant's VM processes' requests have the daemon hold: stream data, replies.",
            crate::memory::in_flight_held_on(daemon),
        ),
        (
            "in_flight_limit_bytes",
            "The most of that the daemon holds before requests wait for it.",
            crate::memory::total_in_flight_on(daemon).budget(),
        ),
    ];
    if let Some(limit) = slabs.total_cap() {
        gauges.push(("memory_limit_bytes", "The limit of every tenant's memory.", limit));
    }
    let governed = crate::governor::governed_on(daemon);
    gauges.extend([
        (
            "memory_full",
            "1 while the host's memory is full: the room for the daemon's under its floor, the \
             daemon making no cell more.",
            u64::from(governed.full),
        ),
        (
            "memory_pressure",
            "1 while the daemon is under memory pressure (holding past its headroom target, own \
             cgroup PSI pressured, or actively thrashing).",
            u64::from(governed.pressured),
        ),
        (
            "memory_thrashing",
            "1 while the extent cache is actively refault-thrashing (recently evicted parts \
             faulting back in rapidly).",
            u64::from(governed.thrashing),
        ),
        (
            "memory_floor_bytes",
            "The least memory the governor leaves the daemon, 0 for a daemon not governed.",
            governed.floor,
        ),
        (
            "memory_target_bytes",
            "The governor's headroom-adjusted memory target, 0 for a daemon not governed.",
            governed.target,
        ),
        (
            "memory_refaults_per_second",
            "Extent cache part refaults over the last second.",
            governed.refaults_per_sec,
        ),
        (
            "memory_psi_some_avg10_bps",
            "Last observed memory PSI some avg10 in basis points (hundredths of a percent).",
            governed.psi_some_avg10_bps,
        ),
        (
            "memory_psi_full_avg10_bps",
            "Last observed memory PSI full avg10 in basis points (hundredths of a percent).",
            governed.psi_full_avg10_bps,
        ),
    ]);
    // cladefs: TASK-94.
    let disk = crate::disk::total_on(daemon);
    gauges.push(("disk_bytes", "The host's disk every tenant's cells take.", disk));
    let reserved = crate::disk::count_on(daemon, None).map_or(0, |counted| counted.reserved);
    gauges.push((
        "disk_reserved_bytes",
        "What every tenant's cells hold past what they take against the limit.",
        reserved,
    ));
    if let Some(limit) = crate::disk::total_limit_on(daemon) {
        gauges.push(("disk_limit_bytes", "The limit of every tenant's disk together.", limit));
    }
    gauges
}

/// The gauges of the process's default daemon (tests).
#[cfg(not(madsim))]
#[cfg(test)]
pub(super) fn gauges() -> Vec<(&'static str, &'static str, u64)> {
    gauges_on(Daemon::process())
}

/// `value` as a label's value in Prometheus' text format: its backslashes, double quotes and
/// newlines escaped.
fn label_value(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', "\\\"").replace('\n', r"\n")
}

/// cladefs: `page`, a rendering of the metrics, as a scraper parses Prometheus' text format,
/// strictly (openmetrics-parser's, board 10482): its families' names, or what it found wrong,
/// which would have the scraper drop the whole scrape: a family's HELP or TYPE said twice, a family
/// named twice, a sample of a family declared of another type, a histogram without its `+Inf`
/// bucket, `_count` or `_sum`, a label value badly escaped, a series said twice.
#[cfg(test)]
pub(super) fn parsed(page: &str) -> Result<Vec<String>, String> {
    let exposition =
        openmetrics_parser::prometheus::parse_prometheus(page).map_err(|err| format!("{err}"))?;
    let mut families: Vec<String> = exposition.families.into_keys().collect();
    families.sort_unstable();
    Ok(families)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Board 10482: what [`parsed`] finds wrong with a page: a page of the daemon's whole as it
    /// renders it, but with a family's HELP written twice, or a histogram's `+Inf` bucket left
    /// out, either of which a scraper refuses, failing the whole scrape.
    #[test]
    fn pages_malformed_as_scrapers_refuse_them_are_refused() {
        let page = render();
        assert_eq!(parsed(&page).map(|families| families.is_empty()), Ok(false), "{page}");

        let help = page.lines().find(|line| line.starts_with("# HELP ")).unwrap();
        let twice = page.replacen(help, &format!("{help}\n{help}"), 1);
        assert_eq!(parsed(&twice), Err("Got two help lines in the same metric family".into()));

        let infinite = |line: &&str| line.contains("_bucket{") && line.contains("le=\"+Inf\"");
        let inf = page.lines().find(infinite).unwrap();
        let without: String =
            page.lines().filter(|line| line != &inf).flat_map(|line| [line, "\n"]).collect();
        let refused = parsed(&without).unwrap_err();
        assert!(refused.starts_with("Histograms must have a +INF bucket"), "{refused}: {inf}");
    }

    /// board 8563: a cell's gauges, each as its VM processes said, saturate as they are summed:
    /// two saying budgets of `u64::MAX` and 1, which a hostile VM process may, panicked every
    /// scrape (overflow checks) or summed to 0. Another cell's apart.
    #[test]
    fn said_gauges_saturate_as_they_are_summed() {
        let vm = |id, budget| crate::stats::Vm {
            connection: 0,
            cell: cladefs_core::CellId { tenant: 7, id },
            path: Default::default(),
            build: None,
            stage: Default::default(),
            budget: Some(budget),
            window: None,
            answered: None,
            guest: None,
        };
        let vms = [vm(9, u64::MAX), vm(9, 1), vm(10, 3)];
        let summed: Vec<_> = by_cell(&vms, |vm| vm.budget).into_iter().collect();
        let cell = |id| cladefs_core::CellId { tenant: 7, id };
        assert_eq!(summed, [(cell(9), u64::MAX), (cell(10), 3)]);
    }

    /// board 8561: scraped while requests are served, each kind's histogram is whole: its buckets'
    /// cumulative counts never fall, none is past `+Inf`, and `+Inf` is `_count`. Its count read
    /// apart from its buckets, a request served between the two reads put a bucket past them. The
    /// requests here take up to 1 s, the last bound, whose bucket a scrape read last before the
    /// count: none past it, whose bucket, counted in `+Inf` alone, would hide a count read early.
    #[test]
    fn histograms_scraped_while_requests_are_served_are_whole() {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let serving: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        served(READ, Duration::from_millis(500), false);
                        served(READ, Duration::from_micros(5), true);
                    }
                })
            })
            .collect();
        let name = "cladefsd_request_duration_seconds";
        for _ in 0..20_000 {
            let mut out = String::new();
            render_requests(&mut out);
            let value = |line: &str| line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap();
            let of_read = |prefix: &str| -> Vec<u64> {
                let prefix = format!("{name}_{prefix}{{kind=\"read\"");
                out.lines().filter(|line| line.starts_with(&prefix)).map(value).collect()
            };
            let buckets = of_read("bucket");
            assert_eq!(buckets.len(), BUCKETS.len() + 1, "{out}");
            assert!(buckets.is_sorted(), "a bucket's cumulative count fell: {buckets:?}");
            assert_eq!(Some(&of_read("count")[0]), buckets.last(), "{out}");
        }
        stop.store(true, Ordering::Relaxed);
        for thread in serving {
            thread.join().unwrap();
        }
    }
}
