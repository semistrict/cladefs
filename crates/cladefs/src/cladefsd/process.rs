// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what the daemon's process holds and has spent, as its kernel counts it, against what
//! its design bounds it by (TASK-330.2): its anonymous memory, what a host's OOM killer weighs it
//! by (TASK-317: 5-6 GB under an upload backlog, past any budget, until the killer took it, and
//! every VM it served), sampled as its stats are, every [`super::cells`]' poll, and its most; its
//! shared memory, the extent cache's pages, which `MEMORY_BYTES` bounds; and its CPU time. The
//! stats line's `process:` says them ([`line`]), and the metrics.
//!
//! Its anonymous memory's budget ([`AnonBudget`]) is the sum of what the daemon holds by design,
//! each part bounded where it is held (doc-7, Memory): its uploads', its framings', its fetches',
//! its tenants' bytes in flight, the parts its cells pack ahead of their framings, held, its cells'
//! maps of their extents, and its own. A sample past it is memory none of those holds: a copy no
//! budget counts (TASK-317's per-try copy of each upload), what the allocator kept of what was
//! freed (TASK-317's glibc arenas), a leak. Each sample is weighed against the budget as of then,
//! [`over-budget`](Sample::over_budget) counting those past it; a soak's or a test's assertion that
//! none were is what holds the daemon to its design.

use std::sync::Mutex;

/// What the daemon holds and spent as of its last sample, and the most of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// Its CPU time, user and system, in nanoseconds.
    pub cpu_ns: u64,
    /// Its anonymous memory resident, in bytes (`RssAnon`).
    pub anon: u64,
    /// The most of it sampled.
    pub max_anon: u64,
    /// Its shared memory resident, the extent cache's pages that it maps (`RssShmem`).
    pub shmem: u64,
    /// The most of it sampled.
    pub max_shmem: u64,
    /// Its anonymous memory's budget as of the sample ([`AnonBudget`]).
    pub anon_budget: u64,
    /// The samples whose anonymous memory was past their budget.
    pub over_budget: u64,
    /// By how much, in bytes, at most.
    pub max_over_budget: u64,
}

/// The parts of the daemon's anonymous memory, each bounded where it is held: what its design
/// says the daemon holds (doc-7, Memory). Not its extent cache's, which is shared memory
/// (`physmem`'s memfd), bounded by `MEMORY_BYTES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnonBudget {
    /// Its uploads': its upload memory, `CLADEFSD_UPLOAD_MEMORY` (`crate::bucket::UPLOADS`),
    /// every cell's PUTs holding their objects within it, once each however often each is tried.
    pub uploads: u64,
    /// Its framings': [`FRAMING`] for each cell written at once, the most so far.
    pub framings: u64,
    /// Its fetches': [`FETCHES`].
    pub fetches: u64,
    /// Its tenants' bytes in flight: the most their requests' stream data and what the daemon
    /// queued for their VM processes held at once, so far (`super::memory::InFlight`), each
    /// tenant's within its own, as the protocol bounds it (TASK-149.35).
    pub in_flight: u64,
    /// The parts of segments its cells packed as they filled, held until their framings: the most
    /// held at once so far, which the memory governor bounds, letting them go before any tenant's
    /// pages as the host's memory runs short (`super::memory::held_parts`, TASK-400).
    pub held: u64,
    /// The cells' page cache's: the most it held at once so far, which the memory governor bounds
    /// as it bounds the parts held (`local_cell::pages`, TASK-433).
    pub pages: u64,
    /// The cells' maps of their extents: the most they held at once so far (TASK-453). Of each
    /// decoded state the cells open share, its map once, however many share it, the bucket's
    /// layer of each of them (`crate::host_cell::decoded_held`); and of each cell, what it holds
    /// of its own, its writes, frees and moves, with its writer's entries for them, and a bucket's
    /// layer that shares no decoded state, and what its writer's map, the bucket's layer's
    /// shared, copies of it as it changes it (`local_cell::own_held`). Each entry at its bound
    /// (`bucket_cell::recovery::ENTRY_BYTES`, `local_cell::OWN_ENTRY_BYTES`), and each run of
    /// changes to a shared map, a path of its nodes copied, at its
    /// (`bucket_cell::recovery::RUN_BYTES`), which `bucket::tests::bench_fork_extent_memory`
    /// holds them to. Not bounded otherwise: a host's
    /// cells' extents, as many as their guests write, are its density's limit, which the budget
    /// shows rather than hides, an idle fork adding what it writes, not its base (TASK-443.2).
    pub extents: u64,
    /// Its own: [`OWN`].
    pub own: u64,
}

impl AnonBudget {
    /// Its parts together: the budget.
    pub fn total(&self) -> u64 {
        self.uploads
            + self.framings
            + self.fetches
            + self.in_flight
            + self.held
            + self.pages
            + self.extents
            + self.own
    }
}

/// What a cell's framing and sealing hold at once, at most (TASK-330.2), from how they hold it.
/// A cell frames one closed segment at a time (`LocalCell::frame_behind`), reading its data whole,
/// at most a segment and an extent (`Options::segment_size` and `max_extent`: a segment closes
/// past its size once it holds an extent that doesn't fit), into a buffer, and framing it into
/// another, as long again and its tail (`extent_frame::framed`), which grows by doubling, but is
/// resident, as this budget counts it, only as far as it is written: twice that, TASK-325's bug,
/// which framing part by part would bound to a few parts.
/// Meanwhile a commit of the cell's seals its extents, copying one not in one run into a buffer
/// of its length, an extent at most, and framing it into its own file a chunk at a time
/// (`LocalCell::write_own`'s `OWN_CHUNK`, 16 MiB, and its framed bytes): an extent and two chunks.
pub const FRAMING: u64 = {
    let (segment, extent, chunk) = (32 << 20, crate::OpenCell::MAX_EXTENT, 16 << 20);
    2 * (segment + extent) + extent + 2 * chunk
};

/// What the host's fetches hold at once, at most: the GETs of extents' data in flight
/// (`crate::bucket::BUDGET`, 64 MiB), the objects' tails kept (`bucket_cell::fetch`'s `TAILS`,
/// 64 MiB) and the parts fetched last (its `RECENT`, 64 MiB), each bounded for the host, however
/// many of its cells read (TASK-92.21, TASK-92.22).
pub const FETCHES: u64 = 3 * (64 << 20);

/// What the daemon holds of its own, whatever it serves, at most: its threads' stacks, as deep as
/// they reached, its runtimes', its allocator's arenas' small allocations, its tables of cells,
/// connections and cached extents. TASK-317's heap profile of a daemon under an upload backlog
/// found all of its heap but uploads, framings and fetches under 10 MB at its peak; its threads,
/// some forty and one a connection, each touch tens of KiB of their stacks; and glibc keeps an
/// arena a thread, up to eight times the CPUs, each holding its small allocations: 64 MiB is
/// those four times over, a bound that a daemon growing with its connections or cells past what
/// its tables need passes.
pub const OWN: u64 = 64 << 20;

node_local::node_local! {
    /// The budget's parts and the samples, under one lock, as the stats' thread takes them.
    static STATE: Mutex<State> = Mutex::new(State {
        sample: Sample {
        cpu_ns: 0,
        anon: 0,
        max_anon: 0,
        shmem: 0,
        max_shmem: 0,
        anon_budget: 0,
        over_budget: 0,
        max_over_budget: 0,
        },
        writers: 0,
        in_flight: 0,
        extents: 0,
        });
}

struct State {
    sample: Sample,
    /// The most cells written at once so far, the most the tenants' connections held in flight
    /// at once, and the most the cells' maps of their extents held at once.
    writers: u64,
    in_flight: u64,
    extents: u64,
}

/// The budget of the daemon's anonymous memory as of now, its parts' most so far: so that what the
/// allocator keeps of what was freed, which it uses again, is within it as it was when held.
pub fn anon_budget() -> AnonBudget {
    budget(&mut STATE.get().lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

fn budget(state: &mut State) -> AnonBudget {
    state.writers = state.writers.max(super::cells::open_count() as u64);
    state.in_flight = state.in_flight.max(super::memory::in_flight_held());
    state.extents = state.extents.max(extents_held());
    AnonBudget {
        uploads: crate::bucket::upload_memory().budget,
        framings: state.writers * FRAMING,
        fetches: FETCHES,
        in_flight: state.in_flight,
        held: super::memory::held_parts().counts().peak,
        pages: local_cell::pages::shared().usage().peak,
        extents: state.extents,
        own: OWN,
    }
}

/// What the cells' maps of their extents hold now, at their entries' bounds ([`AnonBudget`]'s
/// `extents`): the decoded states they share, each once, and what each holds of its own.
pub fn extents_held() -> u64 {
    local_cell::own_held().bytes() + crate::host_cell::decoded_held().bytes()
}

/// Samples what the daemon holds and spent now, against its budget now; and returns the sample.
pub fn sample() -> Sample {
    let (anon, shmem) = resident();
    let mut state = STATE.get().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let parts = budget(&mut state);
    let budget = parts.total();
    let last = state.sample;
    let over = anon.saturating_sub(budget);
    state.sample = Sample {
        cpu_ns: cpu_ns(),
        anon,
        max_anon: last.max_anon.max(anon),
        shmem,
        max_shmem: last.max_shmem.max(shmem),
        anon_budget: budget,
        over_budget: last.over_budget + u64::from(over > 0),
        max_over_budget: last.max_over_budget.max(over),
    };
    if over > 0 && last.over_budget == 0 {
        log::warn!(
            "cladefsd: {} MiB of anonymous memory, past its budget of {} MiB: {:?}",
            anon >> 20,
            budget >> 20,
            parts,
        );
    }
    state.sample
}

/// The last sample.
pub fn last() -> Sample {
    STATE.get().lock().unwrap_or_else(std::sync::PoisonError::into_inner).sample
}

/// The last sample as a line of `NAME=COUNT` after `process:`, which the daemon's stats file
/// keeps (`super::cells`): CPU time, then anonymous and shared memory in bytes, now and at most,
/// the anonymous memory's budget, and the samples past it and by how much at most.
pub fn line() -> String {
    let sample = last();
    format!(
        "process: cpu-ns={} anon={} max-anon={} shmem={} max-shmem={} anon-budget={} \
         over-budget={} max-over-budget={}",
        sample.cpu_ns,
        sample.anon,
        sample.max_anon,
        sample.shmem,
        sample.max_shmem,
        sample.anon_budget,
        sample.over_budget,
        sample.max_over_budget,
    )
}

/// The process's anonymous and shared memory resident, in bytes, as /proc/self/status says them
/// (`RssAnon`, `RssShmem`): 0 of what it doesn't say.
fn resident() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    (kib_of(&status, "RssAnon:") << 10, kib_of(&status, "RssShmem:") << 10)
}

/// The KiB `status`, as /proc/PID/status has it, says of `field`: 0 if it doesn't.
fn kib_of(status: &str, field: &str) -> u64 {
    (status.lines())
        .find_map(|line| line.strip_prefix(field))
        .and_then(|value| value.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse().ok())
        .unwrap_or(0)
}

/// The process's CPU time, user and system, in nanoseconds.
fn cpu_ns() -> u64 {
    // SAFETY: getrusage writes the struct it is given, which is the caller's.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let ns = |time: libc::timeval| time.tv_sec as u64 * 1_000_000_000 + time.tv_usec as u64 * 1_000;
    ns(usage.ru_utime) + ns(usage.ru_stime)
}
