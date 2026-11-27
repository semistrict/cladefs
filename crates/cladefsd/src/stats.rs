// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the stats of the daemon, its VM processes and their guests, what they have counted as
//! of one moment, each with the commit it was built of (TASK-111.4): the control API's `GetStats`,
//! which `cladefsctl stats --json` prints.
//!
//! The daemon asks every VM process that opened a cell for its stats ([`super::wire::STATS_ASK`]);
//! each says its stage counts and answers with its guest's stats, which its device asks the guest's
//! driver for (backlog doc-4's `STATS`), or with none, if its guest takes no asks
//! ([`super::wire::STATS`]). Once every one answered, or the wait is over, the daemon reads its own
//! counters: the stats are as of then, each VM's within the wait before it, which says how long its
//! answer took. Separate reads of `/metrics`, the guests' `/proc/fs/cladefs` and the VM processes'
//! threads, which the sandbox team diffed, were taken at moments apart, and said no build.

use crate::Daemon;
use crate::codec::{StageCounts, Told};
use crate::wire::AsyncSender;
use cladefs_core::Errno;
use cladefs_core::dax::{Gauges, Live};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

/// How long a taking of stats waits for the VM processes' answers unless told: a guest's answer
/// takes a configuration change and a request, under a millisecond on an idle host.
pub(crate) const WAIT: Duration = Duration::from_secs(2);

/// The most a taking of stats waits for them, whatever it is told.
pub(crate) const MOST_WAIT: Duration = Duration::from_secs(60);

/// What the daemon and its VM processes counted, as of [`HostStats::taken`]: the host's stats, as
/// against the daemon's own counters alone ([`crate::stats::Stats`]).
#[derive(Debug)]
pub(crate) struct HostStats {
    /// When the daemon read its own counters, its VM processes having answered.
    pub taken: SystemTime,
    /// The commit the daemon was built of.
    pub build: &'static str,
    /// Its counters, which only grow, by the names its metrics give them (`Stats::named`, dashes
    /// as underscores).
    pub counters: Vec<(String, u64)>,
    /// Its gauges, by the names its metrics give them, without `cladefsd_`.
    pub gauges: Vec<(&'static str, u64)>,
    /// Its VM processes, those that opened a cell, in the order they connected.
    pub vms: Vec<Vm>,
}

/// A VM process, and its guest, as it answered, or as the daemon has it if it didn't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Vm {
    /// Its connection's number, which the daemon gives each in turn: what tells a cell's writers
    /// over time apart.
    pub connection: u64,
    /// Its cell, its tenant's and its ID (board 8563: the metrics label it `TENANT/CELL`).
    pub cell: cladefs_core::CellId,
    /// Its cell's directory, as the daemon has it: without symbolic links.
    pub path: PathBuf,
    /// The commit it said it was built of, if it did ([`super::wire::OPEN`]).
    pub build: Option<String>,
    /// Its window's second stage, as it said ([`super::wire::STAGE_COUNTS`]).
    pub stage: StageCounts,
    /// The runs of pages its second stage keeps installed at most, if it said: as it made its
    /// window ([`super::wire::WINDOW`]), or as it last said since ([`super::wire::STAGE_COUNTS`],
    /// board 10098).
    pub budget: Option<u64>,
    /// Its window's mappings, the daemon's, if it has a window.
    pub window: Option<Live>,
    /// How long after the ask it answered, if it did.
    pub answered: Option<Duration>,
    /// Its guest's stats, if a guest answered.
    pub guest: Option<GuestStats>,
}

impl Vm {
    /// Its counters, which only grow, by name: its window's stage operations, as
    /// `Stats::stage_ops` and the others count them of every VM process.
    pub fn counters(&self) -> [(&'static str, u64); 4] {
        let [ops, ns, overwrites, leave_outs] = self.stage.costs;
        [
            ("stage_ops", ops),
            ("stage_ns", ns),
            ("stage_overwrites", overwrites),
            ("stage_leave_outs", leave_outs),
        ]
    }

    /// Its gauges, by name: the runs of pages its second stage has installed in its memory, each a
    /// mapping the host's `vm.max_map_count` limits, and the most it keeps (TASK-111.5).
    pub fn gauges(&self) -> Vec<(&'static str, u64)> {
        let mut gauges = vec![("host_mappings", self.stage.installed)];
        gauges.extend(self.budget.map(|budget| ("host_mapping_budget", budget)));
        gauges
    }

    /// Its window's mappings, by name, if it has a window: as the daemon's metrics name all
    /// windows' together, without `window_`.
    pub fn window(&self) -> Option<[(&'static str, u64); 4]> {
        self.window.map(|live| {
            [
                ("mappings", live.mappings),
                ("mapped_bytes", live.mapped_bytes),
                ("pages", live.pages),
                // cladefs: as its VM process said, which alone knows of them (doc-26).
                ("private_pages", self.stage.private),
            ]
        })
    }
}

/// What a guest's Fxfs counted, as its driver answered ([`super::wire::STATS`]): what its
/// `/proc/fs/cladefs/TAG` says, and its build.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GuestStats {
    /// The commit it was built of (its `commit` line), if it said.
    pub build: Option<String>,
    /// Its counters and gauges, by name.
    pub counters: BTreeMap<String, u64>,
    /// Its histograms, by name, without `_hist`: each bucket's count, in order.
    pub histograms: BTreeMap<String, Vec<u64>>,
}

impl GuestStats {
    /// What `text` says, lines of a name and a value: `commit` and a build; or a number; or a
    /// histogram's counts between commas, its name ending in `_hist`. A line that is none of those
    /// is logged and left out: the guest and the daemon disagree on what it says.
    pub fn parse(text: &str) -> Self {
        let mut stats = Self::default();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let parsed = line.split_once(' ').and_then(|(name, value)| {
                if name == "commit" {
                    stats.build = Some(value.to_owned());
                    return Some(());
                }
                if let Some(histogram) = name.strip_suffix("_hist") {
                    // Not numbers: a line logged and left out, below.
                    let counts = value.split(',').map(str::parse).collect::<Result<_, _>>().ok()?;
                    stats.histograms.insert(histogram.to_owned(), counts);
                    return Some(());
                }
                // Not a number: a line logged and left out, below.
                stats.counters.insert(name.to_owned(), value.parse().ok()?);
                Some(())
            });
            if parsed.is_none() {
                log::warn!("cladefsd: a guest's stats line not understood, left out: {line:?}");
            }
        }
        stats
    }
}

/// The asks made so far, each a number of its own ([`super::wire::STATS_ASK`]).
// Numbers the process gives out: unique among simulated hosts too, all it asks.
static ASKS: AtomicU64 = AtomicU64::new(0);

/// What `daemon` and its VM processes counted, as of now: the VM processes asked first, and
/// waited for, for `wait` at most ([`MOST_WAIT`] at most); or, unless `vms`, the daemon alone, no
/// VM process asked (board 8581).
pub(crate) async fn take_on(daemon: &Daemon, wait: Duration, vms: bool) -> HostStats {
    if !vms {
        return of_daemon(daemon, Vec::new());
    }
    let ask = ASKS.fetch_add(1, Ordering::Relaxed) + 1;
    let deadline = tokio::time::Instant::now() + wait.min(MOST_WAIT);
    let asked: Vec<_> = (reports(daemon).into_iter())
        .filter_map(|report| {
            let answer = report.ask(ask, Instant::now())?;
            Some((report, answer))
        })
        .collect();
    let mut vms = Vec::with_capacity(asked.len());
    for (report, answer) in asked {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let vm = match tokio::time::timeout(left, answer).await {
            Ok(Ok(vm)) => Some(vm),
            // Not answered in time, or its connection went without answering: as the daemon has
            // it, the ask forgotten.
            Ok(Err(_)) | Err(_) => {
                report.forget(ask);
                report.vm()
            }
        };
        vms.extend(vm);
    }
    of_daemon(daemon, vms)
}

/// [`take_on`] for the process's default daemon (tests).
#[cfg(not(madsim))]
#[cfg(test)]
pub(crate) async fn take(wait: Duration, vms: bool) -> HostStats {
    take_on(Daemon::process(), wait, vms).await
}

/// The daemon's counters and gauges, as of now, and `vms`.
fn of_daemon(daemon: &Daemon, vms: Vec<Vm>) -> HostStats {
    let governed = crate::governor::governed_on(daemon);
    let counters = (cladefs_core::stats::stats().named().into_iter())
        .map(|(name, value)| (name.replace('-', "_"), value))
        .chain([
            ("window_open_maps_refused".to_owned(), cladefs_core::open_cell::open_maps_refused()),
            ("memory_full_entered".to_owned(), governed.fulls),
            ("memory_refaults".to_owned(), daemon.host.cache().refaults()),
            ("memory_kernel_cold_harvested_bytes".to_owned(), governed.kernel_cold_harvested_bytes),
            ("vm_reclaim_bytes".to_owned(), governed.vm_reclaim_bytes),
            ("vm_reclaim_attempts".to_owned(), governed.vm_reclaim_attempts),
            ("vm_reclaim_errors".to_owned(), governed.vm_reclaim_errors),
        ])
        .collect();
    let gauges =
        crate::metrics::gauges_on(daemon).into_iter().map(|(name, _, value)| (name, value));
    HostStats {
        taken: SystemTime::now(),
        build: cladefs_core::BUILD_COMMIT,
        counters,
        gauges: gauges.collect(),
        vms,
    }
}

/// The VM processes connected to `daemon` that opened a cell, as the daemon has them now, in the
/// order they connected (`metrics`, TASK-111.5).
pub(super) fn vms_on(daemon: &Daemon) -> Vec<Vm> {
    reports(daemon).iter().filter_map(|report| report.vm()).collect()
}

/// The builds the VM processes connected to `daemon` said, by their cells, each with how many said
/// it (`metrics`, TASK-111.7). In order of cell and commit.
pub(super) fn vm_builds_on(daemon: &Daemon) -> Vec<((cladefs_core::CellId, String), usize)> {
    let mut counted = BTreeMap::new();
    for vm in vms_on(daemon) {
        if let Some(build) = vm.build {
            *counted.entry((vm.cell, build)).or_insert(0) += 1;
        }
    }
    counted.into_iter().collect()
}

/// What the daemon reports of a connection's VM process, which the connection keeps up to date
/// (TASK-111.4, TASK-111.5): what the daemon's metrics and stats hold, rather than the
/// connection, the last to let go of which closes its cell.
pub(super) struct Report {
    /// The connection's number, the count of connections made as it was.
    id: u64,
    /// Where what the connection tells its VM process goes, as long as the connection is.
    sender: Weak<AsyncSender>,
    /// Its cell, once it opened one.
    opened: OnceLock<Opened>,
    /// The build its VM process said ([`super::wire::OPEN`]), once.
    build: OnceLock<String>,
    /// What its VM process last said its stage operations cost it, and its second stage has
    /// installed ([`super::wire::STAGE_COUNTS`]).
    costs: [AtomicU64; 4],
    installed: AtomicU64,
    /// cladefs: and its window's private pages (doc-26).
    private: AtomicU64,
    /// cladefs: and its runs of private pages, and the `PRIVATE`s it served
    /// ([`super::wire::STAGE_COUNTS`], TASK-164.10).
    private_costs: [AtomicU64; 5],
    /// Its window's gauges, once it has one.
    window: OnceLock<Arc<Gauges>>,
    /// The runs of pages its VM process said its window's second stage keeps installed at most, 0
    /// if it didn't: as it made it ([`super::wire::WINDOW`]), then as it last said
    /// ([`super::wire::STAGE_COUNTS`]), its share of its process's windows', which another window
    /// coming or going changes (board 10098).
    budget: AtomicU64,
    /// The asks for its VM process's stats it has yet to answer ([`super::wire::STATS_ASK`]).
    asked: Mutex<Vec<Asked>>,
    /// cladefs board 14603: who asks its guest in its VM process's stead, while the connection's
    /// vhost-user back-end serves the guest's device.
    guest: Mutex<Option<Arc<dyn AsksGuest>>>,
}

/// Who asks a VM process's guest for its stats in the VM process's stead: its connection's
/// vhost-user back-end, as the guest drives a device the back-end serves (cladefs board 14603).
pub(super) trait AsksGuest: Send + Sync {
    /// Asks the guest `ask`: whether it is asked, its answer to come as its `STATS`; one not asked
    /// is answered at once, without its stats, as a VM process answers for a guest that takes no
    /// asks.
    fn ask(&self, ask: u64) -> bool;
}

/// The cell a connection opened.
struct Opened {
    /// Its directory, without symbolic links.
    path: PathBuf,
    cell: cladefs_core::CellId,
}

/// An ask a VM process has yet to answer ([`Report::ask`]): its number, when it was made, and
/// where its answer goes.
struct Asked {
    ask: u64,
    at: Instant,
    answered: tokio::sync::oneshot::Sender<Vm>,
}

/// The connections' numbers, given in turn ([`Report::id`]).
// Numbers the process gives out: unique among simulated hosts too, all it asks.
static IDS: AtomicU64 = AtomicU64::new(0);

/// How many asks each connection's VM process has yet to answer, in the order they were made.
#[cfg(not(madsim))]
#[cfg(test)]
pub(super) fn asks_waiting() -> Vec<usize> {
    reports(Daemon::process()).iter().map(|report| report.asked.lock().unwrap().len()).collect()
}

/// The reports there are now on `daemon`, in the order their connections were made.
fn reports(daemon: &Daemon) -> Vec<Arc<Report>> {
    let mut reports = daemon.stats_reports.lock().unwrap();
    reports.retain(|report| report.strong_count() > 0);
    reports.iter().filter_map(Weak::upgrade).collect()
}

impl Report {
    /// The report of a new connection on `daemon`, which tells its VM process through `sender`,
    /// numbered next.
    pub fn new(daemon: &Daemon, sender: &Arc<AsyncSender>) -> Arc<Self> {
        let report = Arc::new(Self {
            id: IDS.fetch_add(1, Ordering::Relaxed) + 1,
            sender: Arc::downgrade(sender),
            opened: OnceLock::new(),
            build: OnceLock::new(),
            costs: Default::default(),
            installed: AtomicU64::new(0),
            private: AtomicU64::new(0),
            private_costs: Default::default(),
            window: OnceLock::new(),
            budget: AtomicU64::new(0),
            asked: Mutex::new(Vec::new()),
            guest: Mutex::new(None),
        });
        daemon.stats_reports.lock().unwrap().push(Arc::downgrade(&report));
        report
    }

    /// The connection opened the cell of `cell` at `path`, once for good.
    pub fn opened(&self, path: PathBuf, cell: cladefs_core::CellId) {
        let opened = self.opened.set(Opened { path, cell });
        assert!(opened.is_ok(), "a connection opened a second cell");
    }

    /// The connection made its window, whose gauges are `gauges`, its VM process keeping `budget`
    /// runs of pages installed at most, 0 if it didn't say.
    pub fn windowed(&self, gauges: Arc<Gauges>, budget: u64) {
        let windowed = self.window.set(gauges);
        assert!(windowed.is_ok(), "a connection made a second window");
        self.budget.store(budget, Ordering::Relaxed);
    }

    /// Records the build its VM process says, `commit` ([`super::wire::OPEN`], TASK-111.7):
    /// `EEXIST` said again.
    pub fn said_build(&self, commit: &str) -> Result<(), Errno> {
        self.build.set(commit.to_owned()).map_err(|_| Errno::EEXIST)
    }

    /// Counts what its VM process says its stage operations have cost it so far, `counts` as
    /// [`super::wire::STAGE_COUNTS`] has them, as the daemon's (`Stats::stage_ops`, TASK-91.16):
    /// what it didn't say before. A count only grows: one said again, or smaller, adds nothing.
    /// And keeps the runs of pages it says it has installed, as said last, in order, on the one
    /// task receiving its messages (TASK-111.5).
    pub fn stage_counted(&self, counts: StageCounts) {
        use cladefs_core::stats::Counter;
        let [ops, ns, overwrites, leave_outs] = counts.costs;
        let said = [
            (ops, Counter::StageOps),
            (ns, Counter::StageNs),
            (overwrites, Counter::StageOverwrites),
            (leave_outs, Counter::StageLeaveOuts),
        ];
        for (before, (count, counter)) in self.costs.iter().zip(said) {
            let added = count.saturating_sub(before.fetch_max(count, Ordering::Relaxed));
            cladefs_core::stats::add(counter, added);
        }
        self.installed.store(counts.installed, Ordering::Relaxed);
        self.private.store(counts.private, Ordering::Relaxed);
        // cladefs: its budget as it is now, if it says (board 10098).
        if counts.budget != 0 {
            self.budget.store(counts.budget, Ordering::Relaxed);
        }
    }

    /// cladefs: counts what its VM process says its runs of private pages have cost it so far,
    /// `counts` as [`super::wire::STAGE_COUNTS`] has them, as [`Report::stage_counted`]
    /// counts its stage operations (TASK-164.10).
    pub fn private_counted(&self, counts: crate::codec::PrivateCosts) {
        use cladefs_core::stats::Counter;
        let [runs, pages, ns, requests, waited] = counts;
        let said = [
            (runs, Counter::StagePrivateRuns),
            (pages, Counter::StagePrivatePages),
            (ns, Counter::StagePrivateNs),
            (requests, Counter::VmPrivateRequests),
            (waited, Counter::VmPrivateRequestNs),
        ];
        for (before, (count, counter)) in self.private_costs.iter().zip(said) {
            let added = count.saturating_sub(before.fetch_max(count, Ordering::Relaxed));
            cladefs_core::stats::add(counter, added);
        }
    }

    /// Its VM process, as the daemon has it now, if it opened a cell: not yet answered.
    fn vm(&self) -> Option<Vm> {
        let Opened { path, cell } = self.opened.get()?;
        let costs = self.costs.each_ref().map(|count| count.load(Ordering::Relaxed));
        let installed = self.installed.load(Ordering::Relaxed);
        let private = self.private.load(Ordering::Relaxed);
        let window = self.window.get();
        let budget = self.budget.load(Ordering::Relaxed);
        Some(Vm {
            connection: self.id,
            cell: *cell,
            path: path.clone(),
            build: self.build.get().cloned(),
            // Its budget [`Vm::budget`]'s.
            stage: StageCounts { costs, installed, private, budget: 0 },
            budget: window.and((budget != 0).then_some(budget)),
            window: window.map(|gauges| gauges.live()),
            answered: None,
            guest: None,
        })
    }

    /// Has `guest` ask its guest from here on, or, if none, its VM process.
    #[cfg_attr(not(all(target_os = "linux", not(madsim))), allow(dead_code))]
    pub fn asks_guest(&self, guest: Option<Arc<dyn AsksGuest>>) {
        *self.guest.lock().unwrap() = guest;
    }

    /// Asks its VM process for its stats, as ask `ask`, made at `at` ([`super::wire::STATS_ASK`]),
    /// or its guest, through whoever asks it ([`Report::asks_guest`]): what its answer will say,
    /// unless it opened no cell, or can't be asked.
    fn ask(&self, ask: u64, at: Instant) -> Option<tokio::sync::oneshot::Receiver<Vm>> {
        self.opened.get()?;
        let guest = self.guest.lock().unwrap().clone();
        if let Some(guest) = guest {
            let (answered, answer) = tokio::sync::oneshot::channel();
            self.asked.lock().unwrap().push(Asked { ask, at, answered });
            if !guest.ask(ask)
                && let Err(errno) = self.answered(ask, None)
            {
                log::warn!("cladefsd: answering for a guest not asked for its stats: {errno}");
            }
            return Some(answer);
        }
        let sender = self.sender.upgrade()?;
        let (answered, answer) = tokio::sync::oneshot::channel();
        self.asked.lock().unwrap().push(Asked { ask, at, answered });
        let (header, payload) = Told::Stats { ask }.encode();
        if let Err(err) = sender.send(header, payload, &[]) {
            log::warn!("cladefsd: asking a VM process for its stats: {err}");
            self.forget(ask);
            return None;
        }
        Some(answer)
    }

    /// Forgets ask `ask`, which its asker no longer waits for an answer to: a VM process that never
    /// answers, as one whose guest is paused, which its device asks all the same, holds none of
    /// the asks given up on, which grew one a taking of stats for as long as it lived.
    fn forget(&self, ask: u64) {
        self.asked.lock().unwrap().retain(|asked| asked.ask != ask);
    }

    /// Its VM process answered ask `ask` ([`super::wire::STATS`]), and those before it, with its
    /// guest's stats, `guest`, if it has a guest that answered: as of now, its stage counts said
    /// just before. `EBADF` before it opened a cell.
    pub fn answered(&self, ask: u64, guest: Option<&str>) -> Result<(), Errno> {
        let vm = self.vm().ok_or(Errno::EBADF)?;
        let answered: Vec<Asked> = {
            let mut asked = self.asked.lock().unwrap();
            let (answered, waiting) = asked.drain(..).partition(|asked| asked.ask <= ask);
            *asked = waiting;
            answered
        };
        let guest = guest.map(GuestStats::parse);
        for Asked { at, answered, .. } in answered {
            let vm = Vm { answered: Some(at.elapsed()), guest: guest.clone(), ..vm.clone() };
            // Whoever asked may have stopped waiting.
            let _ = answered.send(vm);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TASK-111.4: a guest's stats, as its Fxfs writes them (`/proc/fs/cladefs/TAG`): counters,
    /// histograms and the build, each line read; a line of neither, left out, the rest kept.
    #[test]
    fn guests_stats_are_their_lines() {
        let text = "device_reads 12\ndevice_reads_ns 3400\ndevice_reads_hist 0,1,2,0\n\
                    commit 0123456789abcdef0123456789abcdef01234567\nwindow_full 0\n\
                    nonsense\nnegative -1\nhalf_hist 1,x\n\n";
        let stats = GuestStats::parse(text);
        assert_eq!(stats.build.as_deref(), Some("0123456789abcdef0123456789abcdef01234567"));
        let counters: Vec<(&str, u64)> =
            stats.counters.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        assert_eq!(counters, [("device_reads", 12), ("device_reads_ns", 3400), ("window_full", 0)]);
        let histograms: Vec<(&str, &[u64])> =
            stats.histograms.iter().map(|(name, counts)| (name.as_str(), &counts[..])).collect();
        assert_eq!(histograms, [("device_reads", &[0, 1, 2, 0][..])]);
    }
}
