// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's metrics, in Prometheus' text exposition format, served over HTTP to whoever scrapes
//! them (TASK-47): `GET /metrics` on the address `CLADEFSD_METRICS` names.
//!
//! - `cladefsd_requests_total{kind}`, `cladefsd_request_errors_total{kind}`, and
//!   `cladefsd_request_duration_seconds{kind}`, a histogram: the VM processes' requests, by
//!   kind, as the daemon served them, from their dispatch to their reply's sending.
//! - `cladefsd_build_info{commit}`, 1: the commit the daemon was built of
//!   (`crate::BUILD_COMMIT`); `cladefsd_vm_build_info{cell,commit}`: the VM processes
//!   connected, by their store's cell, in base 32, and the commit they said they were built
//!   of (`wire::BUILD`), a writer and its followers counted together (TASK-111.7).
//! - `cladefsd_connections`, `cladefsd_stores_open` and `cladefsd_follower_views`:
//!   the VM processes connected, the stores their connections write, and the followers' views.
//! - `cladefsd_extent_cache_pinned_entries` and `cladefsd_extent_cache_idle_entries`:
//!   the sealed extents cached that something but the cache holds, and those nothing else does
//!   (`crate::cache::entries`); `cladefsd_window_mappings`,
//!   `cladefsd_window_mapped_bytes` and `cladefsd_window_pages`: the ranges the VMs'
//!   windows map, their bytes, and the pages of them mapped in the VM processes (`crate::dax::live`,
//!   TASK-131.6); cladefs: `cladefsd_window_private_pages`, of those, the private copies of
//!   windows that copy on write, and `cladefsd_window_open_maps_refused_total`, the `MAP`s of
//!   open extents refused, which no guest makes (backlog doc-13).
//! - cladefs: of each VM process connected that opened a store, by its store's cell, in
//!   base 32, and its connection's number (`connection`), which tells a writer and its
//!   followers apart (TASK-111.5): `cladefsd_vm_window_mappings`,
//!   `cladefsd_vm_window_mapped_bytes`, `cladefsd_vm_window_pages` and
//!   `cladefsd_vm_window_private_pages`, its window's, as the gauges above count all windows';
//!   `cladefsd_vm_host_mappings`, the runs of pages its window's second stage has installed in
//!   its memory, each a mapping the host's `vm.max_map_count` limits, as it last said with its
//!   stage operations, and `cladefsd_vm_host_mapping_budget`, the most it keeps, as it said as
//!   it made its window, from the limit the daemon told it (TASK-172).
//! - `cladefsd_<counter>_total`: the counters the control API's `GetCounters` reports
//!   (`crate::stats::Stats::named`, its dashes underscores): maps, faults, page-ins, reads and
//!   their nanoseconds and bytes; the VM processes' changes to their windows (`stage_ops`) and
//!   their nanoseconds in them (`stage_ns`), whose ratio is what a window change costs a VM process.
//! - `cladefsd_memory_bytes`, `cladefsd_memory_limit_bytes` and
//!   `cladefsd_memory_capacity_bytes`: the memory the daemon's cached extents' pages
//!   hold, every tenant's together, its limit, if any, and the most the daemon's kernel has
//!   (TASK-50.3); `cladefsd_tenant_memory_bytes{tenant}` and
//!   `cladefsd_tenant_memory_limit_bytes{tenant}`: each tenant's alive, by its ID, in
//!   base 32, as cells name it.
//! - `cladefsd_disk_bytes`, `cladefsd_disk_reserved_bytes` and `cladefsd_disk_limit_bytes`: the
//!   host's disk every tenant's stores take together, counted as they write, what they hold past
//!   it against the limit (their reserves and writes admitted), and the limit, if any (TASK-94,
//!   doc-22); `cladefsd_tenant_disk_bytes{tenant}`, `cladefsd_tenant_disk_reserved_bytes{tenant}`,
//!   `cladefsd_tenant_disk_limit_bytes{tenant}`, `cladefsd_tenant_disk_weight{tenant}` and,
//!   under pressure, `cladefsd_tenant_disk_share_bytes{tenant}`: each tenant's alive, by its ID
//!   (`super::disk`).
//! - cladefs: `cladefsd_host_disk_*_bytes{device}`, for each filesystem the daemon keeps stores or
//!   the part cache on, by its device as `MAJOR:MINOR` (doc-22, TASK-539.11): its `size`, `free`,
//!   the governor's `headroom` and `allowance`, what its `stores` take, of that `unframed`, their
//!   `reserved` and `pledged` bytes, what the `cache` keeps, the `spare` the stores leave of the
//!   allowance, and the `other` bytes used on it that the daemon counts as none of its own, an
//!   uncounted writer's on a dedicated disk; `cladefsd_host_disk_pressure{device}`, 1 while under
//!   pressure.
//! - `cladefsd_upload_pending_bytes{cell}`, `cladefsd_upload_oldest_pending_seconds
//!   {cell}` and `cladefsd_upload_stalls_total{cell}`: for each open store whose commits
//!   upload, its commits' bytes not yet uploaded, how long the oldest of them has waited, and how
//!   many commits waited for the lag to shrink (TASK-91.3); its cell as `TENANT/CELL`, in
//!   base 32 (TASK-364).
//! - cladefs: `cladefsd_upload_held_bytes`, `cladefsd_upload_held_max_bytes` and
//!   `cladefsd_upload_memory_bytes`: the bytes of objects the stores' uploads hold in memory,
//!   every store's together, now and at most, and their budget, `CLADEFSD_UPLOAD_MEMORY`
//!   (`crate::bucket::upload_memory`, TASK-317).
//! - cladefs: `cladefsd_part_cache_bytes` and `cladefsd_part_cache_capacity_bytes`: what
//!   the host's part cache keeps, once a store opened it, and its budget,
//!   `CLADEFSD_PART_CACHE_BYTES` (TASK-406.1).
//! - cladefs: `cladefsd_peer_parts_total{outcome}`: the parts the stores' fetchers asked the
//!   peers that own them for, by what came of it (`given`, `missing`, `failed`), and
//!   `cladefsd_peer_served_parts_total{outcome}`, those the daemon's peers asked it for
//!   (`served`, `missing`), if it serves peers (TASK-406.2).
//! - cladefs: `cladefsd_process_cpu_seconds_total`, the daemon's CPU time;
//!   `cladefsd_process_anon_bytes` and `cladefsd_process_anon_max_bytes`, its anonymous
//!   memory, as last sampled and at most; `cladefsd_process_anon_budget_bytes`, what its
//!   design bounds that by, and `cladefsd_process_anon_over_budget_total`, the samples past
//!   it, what an operator alerts on; `cladefsd_process_shmem_bytes`, its shared memory, the
//!   extent cache's pages (`super::process`, TASK-330.2).
//! - `cladefsd_object_storage_requests_total{request}`,
//!   `cladefsd_object_storage_uploaded_bytes_total{object}` and
//!   `cladefsd_object_storage_downloaded_bytes_total`: what the daemon did with its bucket,
//!   once it opened a store.

use super::wire::*;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The kinds of request metrics are kept of, and their names.
const KINDS: [(u32, &str); 23] = [
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
    (ADVANCE, "advance"),
    (LET_GO, "let_go"),
    (PIN, "pin"),
    (UNPIN, "unpin"),
    (TRANSFER, "transfer"),
    (FREE_MANY, "free_many"),
    // cladefs: backlog doc-13.
    (PRIVATE, "private"),
    // cladefs: TASK-111.4.
    (STATS, "stats"),
    // cladefs: TASK-92.14.
    (MAP_MANY, "map_many"),
];

/// The upper bounds of the requests' duration histograms' buckets, in seconds: from what the
/// fastest take (tens of microseconds) to what durable commits may (a second).
const BUCKETS: [f64; 14] = [
    10e-6, 25e-6, 50e-6, 100e-6, 250e-6, 500e-6, 1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 100e-3, 250e-3,
    1.0,
];

/// The requests of a kind served: how many took at most each bucket's bound, how many in all,
/// their nanoseconds, and how many failed.
#[derive(Default)]
struct Requests {
    buckets: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    nanos: AtomicU64,
    errors: AtomicU64,
}

/// Each kind's requests, in [`KINDS`]' order.
static REQUESTS: LazyLock<[Requests; KINDS.len()]> = LazyLock::new(Default::default);

/// The name of the kind of request `kind` ([`KINDS`]), if it is one.
pub(super) fn kind_name(kind: u32) -> Option<&'static str> {
    KINDS.iter().find(|(known, _)| *known == kind).map(|(_, name)| *name)
}

/// cladefs: the name of the span of a request of `kind`, `cladefsd.NAME` ([`kind_name`]), or
/// `cladefsd.request` of a kind it isn't (`super::otel`, TASK-258).
pub(super) fn span_name(kind: u32) -> &'static str {
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
    let requests = &REQUESTS[at];
    let seconds = took.as_secs_f64();
    if let Some(bucket) = BUCKETS.iter().position(|bound| seconds <= *bound) {
        requests.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }
    requests.count.fetch_add(1, Ordering::Relaxed);
    requests.nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    if failed {
        requests.errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// The requests served, as a line of `NAME=COUNT` after `requests:`: of each kind served, how
/// many (`KIND`) and their nanoseconds (`KIND-ns`), which the daemon's stats file keeps
/// (`super::cells`).
pub(super) fn requests_line() -> String {
    let mut line = String::from("requests:");
    for ((_, kind), requests) in KINDS.iter().zip(REQUESTS.iter()) {
        let count = requests.count.load(Ordering::Relaxed);
        if count > 0 {
            let nanos = requests.nanos.load(Ordering::Relaxed);
            let _ = write!(line, " {kind}={count} {kind}-ns={nanos}");
        }
    }
    line
}

/// The metrics as of now, in Prometheus' text format.
pub fn render() -> String {
    let mut out = String::new();
    let load = |count: &AtomicU64| count.load(Ordering::Relaxed);

    out += "# HELP cladefsd_requests_total VM processes' requests served, by kind.\n";
    out += "# TYPE cladefsd_requests_total counter\n";
    for ((_, kind), requests) in KINDS.iter().zip(REQUESTS.iter()) {
        let _ =
            writeln!(out, "cladefsd_requests_total{{kind=\"{kind}\"}} {}", load(&requests.count));
    }
    out += "# HELP cladefsd_request_errors_total VM processes' requests that failed, by kind.\n";
    out += "# TYPE cladefsd_request_errors_total counter\n";
    for ((_, kind), requests) in KINDS.iter().zip(REQUESTS.iter()) {
        let _ = writeln!(
            out,
            "cladefsd_request_errors_total{{kind=\"{kind}\"}} {}",
            load(&requests.errors)
        );
    }
    out += "# HELP cladefsd_request_duration_seconds How long VM processes' requests took to \
            serve, from their dispatch to their reply, by kind.\n";
    out += "# TYPE cladefsd_request_duration_seconds histogram\n";
    for ((_, kind), requests) in KINDS.iter().zip(REQUESTS.iter()) {
        let name = "cladefsd_request_duration_seconds";
        let mut cumulative = 0;
        for (bound, bucket) in BUCKETS.iter().zip(&requests.buckets) {
            cumulative += load(bucket);
            let _ = writeln!(out, "{name}_bucket{{kind=\"{kind}\",le=\"{bound}\"}} {cumulative}");
        }
        let count = load(&requests.count);
        let _ = writeln!(out, "{name}_bucket{{kind=\"{kind}\",le=\"+Inf\"}} {count}");
        let sum = load(&requests.nanos) as f64 / 1e9;
        let _ = writeln!(out, "{name}_sum{{kind=\"{kind}\"}} {sum}");
        let _ = writeln!(out, "{name}_count{{kind=\"{kind}\"}} {count}");
    }

    let name = "cladefsd_build_info";
    let _ = writeln!(out, "# HELP {name} The daemon's build, by the commit it was built of.");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name}{{commit=\"{}\"}} 1", label_value(crate::BUILD_COMMIT));
    let name = "cladefsd_vm_build_info";
    let _ = writeln!(
        out,
        "# HELP {name} VM processes connected, by their store's cell and the commit they were \
         built of, as they said."
    );
    let _ = writeln!(out, "# TYPE {name} gauge");
    for ((cell, commit), count) in super::stats::vm_builds() {
        let _ = writeln!(
            out,
            "{name}{{cell=\"{cell}\",commit=\"{commit}\"}} {count}",
            cell = base32_id::Short(cell)
        );
    }

    for (name, help, value) in gauges() {
        let _ = writeln!(out, "# HELP cladefsd_{name} {help}");
        let _ = writeln!(out, "# TYPE cladefsd_{name} gauge");
        let _ = writeln!(out, "cladefsd_{name} {value}");
    }
    // cladefs: a tripwire (backlog doc-13): no guest maps an open extent.
    let name = "cladefsd_window_open_maps_refused_total";
    let _ = writeln!(out, "# HELP {name} MAPs of open extents refused.");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {}", crate::open_cell::open_maps_refused());

    // cladefs: each VM process's (TASK-111.5).
    let vms = super::stats::vms();
    type Gauge = fn(&super::stats::Vm) -> Option<u64>;
    let series: [(&str, &str, Gauge); 6] = [
        ("vm_window_mappings", "Ranges a VM's DAX window maps.", |vm| Some(vm.window?.mappings)),
        ("vm_window_mapped_bytes", "Bytes those ranges span.", |vm| Some(vm.window?.mapped_bytes)),
        ("vm_window_pages", "Pages a VM's DAX window has mapped in its VM process.", |vm| {
            Some(vm.window?.pages)
        }),
        ("vm_window_private_pages", "Of those, private copies of pages stores made.", |vm| {
            Some(vm.window?.private_pages)
        }),
        (
            "vm_host_mappings",
            "Runs of pages a VM process's window has installed in its memory, each a mapping, as \
             it last said.",
            |vm| vm.window.is_some().then_some(vm.stage.installed),
        ),
        (
            "vm_host_mapping_budget",
            "The most runs of pages a VM process's window keeps installed, as it said.",
            |vm| vm.budget,
        ),
    ];
    for (name, help, value) in series {
        let _ = writeln!(out, "# HELP cladefsd_{name} {help}");
        let _ = writeln!(out, "# TYPE cladefsd_{name} gauge");
        for vm in &vms {
            if let Some(value) = value(vm) {
                let (cell, connection) = (vm.cell, vm.connection);
                let labels = format!(
                    "cell=\"{cell}\",connection=\"{connection}\"",
                    cell = base32_id::Short(cell)
                );
                let _ = writeln!(out, "cladefsd_{name}{{{labels}}} {value}");
            }
        }
    }

    let tenants = super::memory::tenants();
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
    let tenants = super::disk::tenants();
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
    type Of = fn(&super::disk::TenantDisk) -> Option<u64>;
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
    // cladefs: the host's disks, as the disk governor sees them (doc-22).
    for (name, help, values) in super::disk::host_disks() {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (device, value) in values {
            let _ = writeln!(out, "{name}{{device=\"{device}\"}} {value}");
        }
    }

    for (name, value) in crate::stats::stats().named() {
        let name = format!("cladefsd_{}_total", name.replace('-', "_"));
        let _ = writeln!(out, "# HELP {name} As the control API's GetCounters reports it.");
        let _ = writeln!(out, "# TYPE {name} counter");
        let _ = writeln!(out, "{name} {value}");
    }

    // cladefs: what the uploads hold of the objects they upload (TASK-317).
    let memory = crate::bucket::upload_memory();
    for (name, help, value) in [
        (
            "cladefsd_upload_held_bytes",
            "Bytes of objects the stores' uploads hold in memory now, every store's together.",
            memory.held,
        ),
        (
            "cladefsd_upload_held_max_bytes",
            "The most bytes of objects the stores' uploads held in memory at once.",
            memory.max_held,
        ),
        (
            "cladefsd_upload_memory_bytes",
            "The most bytes of objects the stores' uploads may hold in memory at once.",
            memory.budget,
        ),
    ] {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = writeln!(out, "{name} {value}");
    }

    // cladefs: what the part cache keeps, and its budget (TASK-406.1).
    if let Some(cache) = super::cells::opened_cache() {
        for (name, help, value) in [
            (
                "cladefsd_part_cache_bytes",
                "Bytes of objects the host's part cache keeps.",
                cache.bytes(),
            ),
            (
                "cladefsd_part_cache_capacity_bytes",
                "The most bytes of objects the host's part cache keeps.",
                cache.capacity(),
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            let _ = writeln!(out, "{name} {value}");
        }
    }

    // cladefs: the parts asked of peers, and served them (TASK-406.2).
    if let Some((counts, served, served_missing)) = super::peers::counts() {
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
        for (outcome, value) in [("served", served), ("missing", served_missing)] {
            let _ = writeln!(out, "{name}{{outcome=\"{outcome}\"}} {value}");
        }
    }

    // cladefs: what the process holds and spent, against its budget (TASK-330.2).
    let process = super::process::last();
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

    let uploads = super::cells::upload_statuses();
    let series: [(&str, &str, &str, fn(&local_cell::upload::UploadStatus) -> String); 3] = [
        (
            "upload_pending_bytes",
            "gauge",
            "Bytes of a store's commits not yet uploaded, by its cell.",
            |status| status.pending_bytes.to_string(),
        ),
        (
            "upload_oldest_pending_seconds",
            "gauge",
            "How long a store's oldest commit not yet uploaded has waited, by its cell.",
            |status| {
                let waited = status.oldest_pending.and_then(|since| since.elapsed().ok());
                waited.unwrap_or_default().as_secs_f64().to_string()
            },
        ),
        (
            "upload_stalls_total",
            "counter",
            "A store's commits that waited for its upload lag to shrink, by its cell.",
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

    if let Some(stats) = super::cells::bucket_stats() {
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
    }
    out
}

/// The daemon's gauges, as of now, each with its name, without `cladefsd_`, and what it is:
/// what its metrics and stats report (TASK-111.4). The memory's limit only if it has one, and
/// the disk's.
pub(super) fn gauges() -> Vec<(&'static str, &'static str, u64)> {
    let (entries, live) = (crate::cache::entries(), crate::dax::live());
    let mut gauges = vec![
        ("connections", "VM processes connected.", super::server::connections() as u64),
        (
            "stores_open",
            "Stores VM processes' connections write.",
            super::cells::open_count() as u64,
        ),
        ("follower_views", "Followers' views of stores.", super::cells::follower_views() as u64),
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
            "Of those, private copies of pages stores made, of windows that copy on write.",
            live.private_pages,
        ),
        ("memory_bytes", "Memory extents' pages hold, every tenant's.", physmem::arenas_bytes()),
        ("memory_capacity_bytes", "The most memory the daemon has.", physmem::count_total_bytes()),
    ];
    if let Some(limit) = physmem::total_cap() {
        gauges.push(("memory_limit_bytes", "The limit of every tenant's memory.", limit));
    }
    // cladefs: TASK-94.
    let disk = super::disk::total();
    gauges.push(("disk_bytes", "The host's disk every tenant's stores take.", disk));
    let reserved = super::disk::count(None).map_or(0, |counted| counted.reserved);
    gauges.push((
        "disk_reserved_bytes",
        "What every tenant's stores hold past what they take against the limit.",
        reserved,
    ));
    if let Some(limit) = super::disk::total_limit() {
        gauges.push(("disk_limit_bytes", "The limit of every tenant's disk together.", limit));
    }
    gauges
}

/// `value` as a label's value in Prometheus' text format: its backslashes, double quotes and
/// newlines escaped.
fn label_value(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', "\\\"").replace('\n', r"\n")
}

/// How long a client has to send its request and take the response: one deadline for the whole
/// exchange, as a client sending a byte at a time, or taking nothing, would hold its turn for good
/// against a timeout of each read or write (TASK-79.27).
const EXCHANGE: Duration = Duration::from_secs(5);

/// The most bytes of a request's line and headers read: a scraper's are a line and a few headers.
/// More are refused, unread, as a line read whole would grow without end.
const HEAD_MAX: u64 = 8 << 10;

/// How many clients are served at once: those after wait to be accepted, holding nothing, so that
/// clients slow to send or take what they asked for hold up no scrape but past these.
const AT_ONCE: usize = 4;

/// Serves the metrics to whoever connects to `listener` and asks for `/metrics` over HTTP, until
/// it listens no more (`EINVAL`): each client on a thread of its own, [`AT_ONCE`] at most, within
/// [`EXCHANGE`]. An accept that fails otherwise, as with as many files open as the daemon may, is
/// tried again after a while (`server::ACCEPT_AGAIN`), as the daemon's VM sockets' are: given up
/// on, the daemon's metrics would be served no more, the daemon serving on (TASK-79.35).
pub fn serve_metrics(listener: TcpListener) -> std::io::Result<()> {
    let serving = std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
    let mut failing = false;
    loop {
        {
            let (count, done) = &*serving;
            let mut count = count.lock().unwrap();
            while *count == AT_ONCE {
                count = done.wait(count).unwrap();
            }
            *count += 1;
        }
        let accepted = listener.accept();
        if accepted.is_err() {
            *serving.0.lock().unwrap() -= 1;
        }
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => return Err(err),
            // That client's, gone before it was accepted.
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted => continue,
            Err(err) => {
                if !failing {
                    log::error!("cladefsd: accepting a metrics client: {err}: trying again");
                    failing = true;
                }
                std::thread::sleep(super::server::ACCEPT_AGAIN);
                continue;
            }
        };
        failing = false;
        let served = serving.clone();
        let spawned = std::thread::Builder::new().name("cladefsd metric".into()).spawn(move || {
            if let Err(err) = answer(stream) {
                log::warn!("cladefsd: serving metrics: {err}");
            }
            let (count, done) = &*served;
            *count.lock().unwrap() -= 1;
            done.notify_one();
        });
        if let Err(err) = spawned {
            log::warn!("cladefsd: serving metrics: {err}");
            *serving.0.lock().unwrap() -= 1;
        }
    }
}

/// A client's connection within its exchange's deadline: each read and write waits at most for
/// what is left of it, and fails with `TimedOut` once nothing is.
struct Deadlined<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Deadlined<'_> {
    fn left(&self) -> std::io::Result<Duration> {
        match self.deadline.saturating_duration_since(Instant::now()) {
            Duration::ZERO => Err(std::io::ErrorKind::TimedOut.into()),
            left => Ok(left),
        }
    }
}

impl Read for Deadlined<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.left()?))?;
        (&*self.stream).read(bytes)
    }
}

impl Write for Deadlined<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.left()?))?;
        (&*self.stream).write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Answers the HTTP request `stream` carries, within [`EXCHANGE`]: the metrics, for `GET
/// /metrics`; `431`, for a request whose line and headers are more than [`HEAD_MAX`] bytes.
fn answer(stream: TcpStream) -> std::io::Result<()> {
    let deadline = Instant::now() + EXCHANGE;
    let mut reader = BufReader::new(Deadlined { stream: &stream, deadline }.take(HEAD_MAX));
    let mut request = String::new();
    let mut whole = read_line(&mut reader, &mut request)?;
    // The headers, which say nothing it needs.
    while whole {
        let mut line = String::new();
        whole = read_line(&mut reader, &mut line)?;
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let too_large = !whole && reader.get_ref().limit() == 0;
    let mut parts = request.split_whitespace();
    let (status, body) = match (parts.next(), parts.next()) {
        _ if too_large => ("431 Request Header Fields Too Large", "request too large\n".to_owned()),
        (Some("GET"), Some("/metrics")) => ("200 OK", render()),
        _ => ("404 Not Found", "not found\n".to_owned()),
    };
    let mut connection = Deadlined { stream: &stream, deadline };
    write!(
        connection,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    if too_large {
        // What it sent past its head is read and dropped, until it is done or its time is: the
        // connection closed with bytes unread would be reset, and the response lost with it.
        stream.shutdown(std::net::Shutdown::Write)?;
        std::io::copy(&mut Deadlined { stream: &stream, deadline }, &mut std::io::sink())?;
    }
    Ok(())
}

/// Reads a line into `line` as `read_line` does: whether it is whole, ended by a newline; not, at
/// the end of what `reader` reads.
fn read_line(reader: &mut impl BufRead, line: &mut String) -> std::io::Result<bool> {
    reader.read_line(line)?;
    Ok(line.ends_with('\n'))
}
