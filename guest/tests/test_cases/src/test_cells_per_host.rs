// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: how many cells a host holds (board 5583, of board 5581): VMs booted on forks of one base,
//! added until the host holds no more, and the count said.
//!
//! One extent daemon, its memory a page cache of what is free, as production's (TASK-358), imports
//! a small image as a base: Alpine's root and the guest agent. Each cell is a VM of 1 vCPU and
//! CLADEFS_CELLS_VM_MIB MiB (256), booted on a fork of the base as its root Fxfs, with a DAX window
//! of CLADEFS_CELLS_WINDOW_MIB MiB (1024) on it in cells-per-host-dax. Its guest runs its probes
//! once, its boot's working set, and says it is ready over vsock; then runs them every second, as
//! a sandbox's user does, timing each, and tells the host:
//!
//! - `tick`: how late the round started, its second's timer to its run: a guest short of CPU;
//! - `echo`: `/bin/echo`, a process started and ended;
//! - `cat`: `cat /etc/os-release`, a file of the base read;
//! - `fsync`: 4 KiB written over a file of its own and fsynced, a commit of its fork.
//!
//! The host adds VMs in steps, all of a step's booting at once. A step is as many as half the
//! room left would hold, at what each VM held so far took of it, CLADEFS_CELLS_STEP (16) at most,
//! one at least; the first is one, its cost not known yet ([`Ramp::next`]). The room is what the
//! daemon's governor finds available, the host's or its cgroups' (`governor::host_memory`), with
//! what the daemon's memory holds, which it gives back as VMs need it (a page a window maps
//! unmapped first, `crate::cache`), less the governor's headroom and the floor it keeps the
//! daemon's memory at (`governor::floor`, board 5662) ([`room`]). Once a step's VMs
//! are ready, the host lets one window of CLADEFS_CELLS_WINDOW_SECS (10) go by and judges each
//! probe's p99 over it, every VM's samples together, and the step's boots', against its budget
//! ([`PROBES`], [`BOOT_BUDGET_MS`]). A window past a budget is judged again, a window later and no
//! VM added: two in a row stop the ramp ([`Ramp::judged`]). It stops at:
//!
//! - **memory**: the room holds less than one VM more;
//! - **full**: the daemon refused a fork, the host's memory full (`governor::full`, board 5662);
//! - **latency**: a probe's p99, or the step's boots', past its budget two windows in a row, the
//!   VMs held those of the step before;
//! - **max**: CLADEFS_CELLS_MAX (1000) VMs held.
//!
//! What a host does past its limit is a bug, not a limit: the case fails if a VM exits, its guest
//! hangs up, says nothing for [`SILENT_SECS`], or isn't ready within CLADEFS_CELLS_READY_SECS
//! (120) of its spawn; if the daemon lets a VM process go; if a fork or a boot is refused; or if a
//! console complains (the runner's check). And if it held fewer than CLADEFS_CELLS_FLOOR VMs (1).
//!
//! It says each step on its standard error (`cells-per-host: step=...`, with a `memory:` line of
//! `crate::measure`'s), and how it ended (`cells-per-host: held=N limit=LIMIT`); and records what
//! it measured in the bench's shape (`common::Measured`, CLADEFS_BENCH_RESULTS). A step's line
//! says where a VM's memory goes, each the mean of the VMs running, in MiB (board 5649):
//!
//! - of its guest, as it last said (`crate::measure::itself`, each window): the RAM its kernel
//!   never had (`guest_reserved_mib`, its image and boot's reservations), what is not available
//!   (`guest_used_mib`), and of that its kernel's slab, stacks and page tables, its page cache,
//!   and the struct pages its DAX window took (`guest_memmap_mib`);
//! - of its VM process (/proc/PID/smaps_rollup and status): its share of its memory (`vmm_pss_mib`),
//!   of that its anonymous memory, the guest's RAM touched (`vmm_anon_mib`), and its shared
//!   memory, the daemon's pages its window maps (`vmm_shmem_mib`); and its page tables
//!   (`vmm_pt_mib`, VmPTE);
//! - KVM's page tables for it, the host's grown since the first VM (`kvm_pt_mib`, SecPageTables);
//! - the daemon's, its anonymous memory, its bookkeeping (`daemon_anon_mib`), and its extents'
//!   pages (`daemon_cache_mib`, `cladefsd_memory_bytes`), each over the VMs.
//!
//! cells-per-host-dax-zygote (board 7084): no VM boots but the zygote, a VM booted so on a fork
//! of the base, its probes run once, its root frozen, of which a version is made, its memory
//! imported into a memory cell and a version made of that ([`Zygote`]). Each VM of the ramp
//! restores it: its root a fork of the zygote's version, its memory a fork of the memory cell's
//! version, a copy-on-write window the daemon serves, its pages shared with every other VM's but
//! those it writes (TASK-456.3). Its guest, finding its connection to the zygote's host reset by
//! the restore, thaws and goes on as a booted one's does. A step's `boot_p99_ms` is then its
//! restores': each its `/snapshot/load`, and its guest's reset found to its word that it is ready.

use macros::{guest, host};

pub struct TestCellsPerHost {
    /// Whether each VM's root is mapped through a DAX window.
    #[cfg_attr(feature = "guest", allow(dead_code))]
    pub dax: bool,
    /// Whether each VM restores the zygote rather than boot (the module's docs).
    pub zygote: bool,
}

/// The vsock port a guest tells the host on (`crate::measure`'s is 5056).
const PORT: u32 = 5057;

/// The probes a guest runs each second, in the order it says them, each with its budget: the p99
/// past which, over a window, every VM's samples together, the host holds no more. Of what a
/// sandbox's user waits for at a prompt: a tenth of a second for a command, fsync's a commit's
/// quarter of a second, five of cladefsd's turns (doc-12).
#[cfg_attr(feature = "guest", allow(dead_code))]
const PROBES: [(&str, u64); 4] = [("tick", 100), ("echo", 100), ("cat", 100), ("fsync", 250)];

/// The p99 of a step's boots past which the host holds no more: a guest's kernel's start to its
/// probes' first round done, in ms.
#[cfg_attr(feature = "guest", allow(dead_code))]
const BOOT_BUDGET_MS: u64 = 10_000;

/// How long a guest may say nothing, once ready, before the case takes it as hung.
#[cfg_attr(feature = "guest", allow(dead_code))]
const SILENT_SECS: u64 = 60;

/// The line a guest says each round, of the probes' microseconds, in [`PROBES`]' order.
#[cfg_attr(feature = "host", allow(dead_code))]
fn probe_line(micros: &[u64; PROBES.len()]) -> String {
    let fields: Vec<String> =
        PROBES.iter().zip(micros).map(|((name, _), us)| format!("{name}={us}")).collect();
    format!("probe {}", fields.join(" "))
}

/// The probes' microseconds a `probe` line says, each [`PROBES`]' in order: none if it isn't one.
#[cfg_attr(feature = "guest", allow(dead_code))]
fn probe_of(line: &str) -> Option<[u64; PROBES.len()]> {
    let fields = line.strip_prefix("probe ")?.split(' ').collect::<Vec<_>>();
    if fields.len() != PROBES.len() {
        return None;
    }
    let mut micros = [0; PROBES.len()];
    for ((field, (name, _)), us) in fields.iter().zip(PROBES).zip(&mut micros) {
        *us = field.strip_prefix(name)?.strip_prefix('=')?.parse().ok()?;
    }
    Some(micros)
}

/// What a guest's `memory` line says of it (`crate::measure::itself`), each `guest_NAME=VALUE` by
/// NAME: none if it isn't one.
#[cfg_attr(feature = "guest", allow(dead_code))]
fn memory_of(line: &str) -> Option<std::collections::BTreeMap<String, u64>> {
    (line.strip_prefix("memory ")?.split(' '))
        .map(|field| {
            let (name, value) = field.split_once('=')?;
            Some((name.strip_prefix("guest_")?.to_owned(), value.parse().ok()?))
        })
        .collect()
}

#[host]
mod host {
    use super::*;
    use crate::common::{Measured, cladefs_guest_should_run, control, setup_rootfs};
    use crate::measure::HostMemory;
    use crate::vm::{Firecracker, Vm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::{Context, ensure};
    use cladefs::cladefsd::control::proto::{ForkCellRequest, ImportImageRequest};
    use cladefs::cladefsd::governor;
    use cladefs::import::testing::oci_layout;
    use cladefs::testing::control_path;
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const MIB: u64 = 1 << 20;

    /// The base: Alpine's root, BusyBox's tools, and the guest agent, which the runner adds.
    const CONTAINERFILE: &str = "FROM alpine:3.22\n";

    /// The base's capacity, and so each fork's.
    const CAPACITY: u64 = 2 << 30;

    /// What the ramp is told, from the environment.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Settings {
        /// Each VM's memory, in MiB.
        vm_mib: u32,
        /// Each root's DAX window, in bytes, in the -dax case.
        window: u64,
        /// The most VMs a step adds.
        step: usize,
        /// The most VMs it holds.
        max: usize,
        /// The fewest it may hold and pass.
        floor: usize,
        /// The window it judges each step over.
        window_secs: u64,
        /// How long a step's VMs have to say they are ready.
        ready_secs: u64,
    }

    impl Settings {
        fn from_env() -> anyhow::Result<Self> {
            let count = |name: &str, default: u64| -> anyhow::Result<u64> {
                match std::env::var(name) {
                    Ok(value) if !value.is_empty() => {
                        value.parse().with_context(|| format!("{name} is a count, not {value:?}"))
                    }
                    _ => Ok(default),
                }
            };
            let settings = Self {
                vm_mib: u32::try_from(count("CLADEFS_CELLS_VM_MIB", 256)?)?,
                window: count("CLADEFS_CELLS_WINDOW_MIB", 1024)? * MIB,
                step: usize::try_from(count("CLADEFS_CELLS_STEP", 16)?)?,
                max: usize::try_from(count("CLADEFS_CELLS_MAX", 1000)?)?,
                floor: usize::try_from(count("CLADEFS_CELLS_FLOOR", 1)?)?,
                window_secs: count("CLADEFS_CELLS_WINDOW_SECS", 10)?,
                ready_secs: count("CLADEFS_CELLS_READY_SECS", 120)?,
            };
            ensure!(settings.step >= 1, "CLADEFS_CELLS_STEP is 1 at least");
            ensure!(settings.max >= 1, "CLADEFS_CELLS_MAX is 1 at least");
            ensure!(settings.window_secs >= 1, "CLADEFS_CELLS_WINDOW_SECS is 1 at least");
            Ok(settings)
        }
    }

    /// What stopped the ramp.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Limit {
        /// The room left holds less than one VM more.
        Memory,
        /// The daemon refused a fork, the host's memory full (board 5662).
        Full,
        /// A probe's p99, or the step's boots' (`boot`), past its budget, by its name.
        Latency(&'static str),
        /// As many VMs as it was told to hold at most.
        Max,
    }

    impl std::fmt::Display for Limit {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Limit::Memory => write!(f, "memory"),
                Limit::Full => write!(f, "full"),
                Limit::Latency(probe) => write!(f, "latency:{probe}"),
                Limit::Max => write!(f, "max"),
            }
        }
    }

    /// What the ramp does next.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Next {
        /// Boots this many VMs more.
        Add(usize),
        /// Holds no more.
        Stop(Limit),
    }

    /// What a window's judgement says.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Verdict {
        /// Within every budget: the VMs held are held.
        Held,
        /// Past one, once: judged again, a window later, no VM added.
        Again,
        /// Past one twice in a row: the VMs held are those of the step before.
        Stop(Limit),
    }

    /// The ramp's state: the room it had before the first VM, the VMs held within every budget,
    /// and the budget a window was past, if the last was.
    #[derive(Debug)]
    struct Ramp {
        settings: Settings,
        room_before: u64,
        held: usize,
        past: Option<&'static str>,
    }

    impl Ramp {
        fn new(settings: Settings, room_before: u64) -> Self {
            Self { settings, room_before, held: 0, past: None }
        }

        /// What each VM held took of the room, `room` now: none before the first.
        fn per_vm(&self, room: u64) -> Option<u64> {
            (self.held > 0).then(|| self.room_before.saturating_sub(room) / self.held as u64)
        }

        /// What to do next, with `room` left, once the VMs held are judged held: as many VMs as
        /// half of `room` holds at what each took, from one to a step; one, if none is held yet.
        fn next(&self, room: u64) -> Next {
            let Settings { step, max, .. } = self.settings;
            if self.held >= max {
                return Next::Stop(Limit::Max);
            }
            let Some(per_vm) = self.per_vm(room) else { return Next::Add(1) };
            let fits = room / per_vm.max(1);
            if fits == 0 {
                return Next::Stop(Limit::Memory);
            }
            let half = usize::try_from(fits / 2).unwrap_or(usize::MAX).max(1);
            Next::Add(half.min(step).min(max - self.held))
        }

        /// Judges a window of `running` VMs, `past` the budget it was past, if any.
        fn judged(&mut self, running: usize, past: Option<&'static str>) -> Verdict {
            match (past, self.past) {
                (None, _) => {
                    (self.held, self.past) = (running, None);
                    Verdict::Held
                }
                (Some(probe), None) => {
                    self.past = Some(probe);
                    Verdict::Again
                }
                (Some(probe), Some(_)) => Verdict::Stop(Limit::Latency(probe)),
            }
        }
    }

    /// The room for VMs: what the governor finds `available`, with the daemon's memory, `cached`,
    /// which it gives back as they need it down to its floor, `kept`, less the governor's
    /// `headroom`.
    fn room(available: u64, cached: u64, headroom: u64, kept: u64) -> u64 {
        (available + cached).saturating_sub(headroom + kept)
    }

    /// The `p`th percentile of `values` (nearest rank), 0 of none.
    fn percentile(values: &mut [u64], p: u64) -> u64 {
        if values.is_empty() {
            return 0;
        }
        values.sort_unstable();
        let rank = (values.len() as u64 * p).div_ceil(100).max(1);
        values[rank as usize - 1]
    }

    /// The first budget a window's samples, `samples`, and its step's boots, `boots`, in ms, are
    /// past, by its probe's name or `boot`, and each probe's p99 and worst, in ms, in [`PROBES`]'
    /// order.
    fn past_budget(
        samples: &[[u64; PROBES.len()]],
        boots: &[u64],
    ) -> (Option<&'static str>, Vec<(u64, u64)>) {
        let mut past = None;
        let mut judged = Vec::new();
        for (at, (name, budget)) in PROBES.iter().enumerate() {
            let mut ms: Vec<u64> = samples.iter().map(|sample| sample[at] / 1000).collect();
            let worst = ms.iter().copied().max().unwrap_or(0);
            let p99 = percentile(&mut ms, 99);
            if p99 > *budget && past.is_none() {
                past = Some(*name);
            }
            judged.push((p99, worst));
        }
        let boot = percentile(&mut boots.to_vec(), 99);
        if boot > BOOT_BUDGET_MS && past.is_none() {
            past = Some("boot");
        }
        (past, judged)
    }

    /// A VM process's memory, in bytes ([`Host::breakdown`]).
    struct Vmm {
        pss: u64,
        anon: u64,
        shmem: u64,
        page_tables: u64,
    }

    /// Where each VM's memory goes, as a step's line says it (the module's docs): the mean of
    /// `vmms`, of `guests`, each a guest's memory as it said it, of VMs of `ram` bytes, and over
    /// `vmms`' count, KVM's page tables `kvm_tables` and the daemon's `daemon_anon` and
    /// `daemon_cache`.
    fn breakdown(
        ram: u64,
        guests: &[&BTreeMap<String, u64>],
        vmms: &[Vmm],
        kvm_tables: u64,
        daemon_anon: u64,
        daemon_cache: u64,
    ) -> anyhow::Result<String> {
        let mean = |sum: u64, count: usize| sum as f64 / count.max(1) as f64 / MIB as f64;
        let mut line = String::new();
        let said = |guest: &BTreeMap<String, u64>, name: &str| {
            guest.get(name).copied().with_context(|| format!("a guest said no {name}"))
        };
        let mut sums = [0; 7];
        for guest in guests {
            let total = said(guest, "total")?;
            let parts = [
                ram.saturating_sub(total),
                total.saturating_sub(said(guest, "available")?),
                said(guest, "slab")?,
                said(guest, "kernel_stack")?,
                said(guest, "page_tables")?,
                said(guest, "cached")?,
                said(guest, "memmap")?,
            ];
            for (sum, part) in sums.iter_mut().zip(parts) {
                *sum += part;
            }
        }
        let names = ["reserved", "used", "slab", "stack", "pt", "cached", "memmap"];
        for (name, sum) in names.iter().zip(sums) {
            line.push_str(&format!(" guest_{name}_mib={:.1}", mean(sum, guests.len())));
        }
        let of = |part: fn(&Vmm) -> u64| vmms.iter().map(part).sum::<u64>();
        for (name, sum) in [
            ("vmm_pss", of(|vmm| vmm.pss)),
            ("vmm_anon", of(|vmm| vmm.anon)),
            ("vmm_shmem", of(|vmm| vmm.shmem)),
            ("vmm_pt", of(|vmm| vmm.page_tables)),
            ("kvm_pt", kvm_tables),
            ("daemon_anon", daemon_anon),
            ("daemon_cache", daemon_cache),
        ] {
            line.push_str(&format!(" {name}_mib={:.1}", mean(sum, vmms.len())));
        }
        Ok(line)
    }

    /// The host's KVM page tables for its guests (/proc/meminfo's SecPageTables), none on a
    /// kernel that doesn't say them.
    fn kvm_tables() -> anyhow::Result<u64> {
        let host = crate::measure::meminfo(&std::fs::read_to_string("/proc/meminfo")?);
        Ok(host.get("SecPageTables").copied().unwrap_or(0))
    }

    /// What the VMs' guests said, by VM: kept by each's reader as it hears it.
    #[derive(Default)]
    struct Heard {
        /// Every sample since the host last took them, every VM's.
        samples: Vec<[u64; PROBES.len()]>,
        vms: BTreeMap<usize, Guest>,
    }

    /// What one guest said.
    #[derive(Default)]
    struct Guest {
        /// Its kernel's start to its first round done, in ms, once it said so; a restored one's,
        /// its restore's reset found to its first round done.
        ready_ms: Option<u64>,
        /// When it last said anything.
        last: Option<Instant>,
        /// Why its reader stopped hearing it: it hung up, or said what is no line of its.
        gone: Option<String>,
        /// Its memory, as it last said ([`memory_of`]).
        memory: Option<BTreeMap<String, u64>>,
    }

    /// Reads what VM `vm`'s guest says on `stream` into `heard` until it hangs up.
    fn hear(vm: usize, stream: UnixStream, heard: Arc<Mutex<Heard>>) {
        let gone = (|| -> Result<(), String> {
            for line in BufReader::new(stream).lines() {
                let line = line.map_err(|err| format!("reading it: {err}"))?;
                let mut heard = heard.lock().unwrap();
                if let Some(micros) = probe_of(&line) {
                    heard.samples.push(micros);
                } else if let Some(ms) = line.strip_prefix("ready uptime_ms=") {
                    let ms = ms.parse().map_err(|_| format!("it said {line:?}"))?;
                    heard.vms.entry(vm).or_default().ready_ms = Some(ms);
                } else if let Some(memory) = memory_of(&line) {
                    heard.vms.entry(vm).or_default().memory = Some(memory);
                } else {
                    return Err(format!("it said {line:?}"));
                }
                heard.vms.entry(vm).or_default().last = Some(Instant::now());
            }
            Err("it hung up".to_owned())
        })()
        .unwrap_err();
        heard.lock().unwrap().vms.entry(vm).or_default().gone = Some(gone);
    }

    /// A VM of the ramp, running.
    struct Cell {
        name: String,
        firecracker: Firecracker,
        /// The host's end of its guest's connection, to tell it to stop.
        to_guest: UnixStream,
    }

    /// Every VM the ramp boots, ended at once if the case ends before it stops them, as by an
    /// error.
    #[derive(Default)]
    struct Cells(Vec<Cell>);

    impl Drop for Cells {
        fn drop(&mut self) {
            for cell in self.0.drain(..) {
                if let Err(err) = cell.firecracker.kill() {
                    eprintln!("cells-per-host: killing {}: {err:#}", cell.name);
                }
            }
        }
    }

    /// The vsock socket a VM's guest connects to, named for `name`, as `crate::measure`'s are;
    /// removed once its guest connected.
    struct Listening {
        uds: PathBuf,
        path: PathBuf,
        listener: UnixListener,
    }

    impl Listening {
        fn new(name: &str) -> anyhow::Result<Self> {
            let uds = crate::measure::Requests::uds_of(&format!("cells-{name}"));
            let path = PathBuf::from(format!("{}_{PORT}", uds.display()));
            let listener = UnixListener::bind(&path)
                .with_context(|| format!("listening at {}", path.display()))?;
            Ok(Self { uds, path, listener })
        }
    }

    impl Drop for Listening {
        fn drop(&mut self) {
            for path in [&self.path, &self.uds] {
                match std::fs::remove_file(path) {
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                        eprintln!("cells-per-host: removing {}: {err}", path.display())
                    }
                    _ => {}
                }
            }
        }
    }

    /// The case's daemon and base, and what it asks of them.
    struct Host<'a> {
        test_setup: &'a TestSetup,
        root: PathBuf,
        socket: PathBuf,
        base: cladefs::CellId,
        daemon: std::process::Child,
        memory: HostMemory,
        settings: Settings,
        window: u64,
        /// What each VM restores, in cells-per-host-dax-zygote.
        zygote: Option<Zygote>,
    }

    /// The zygote, as its VMs restore it (the module's docs): its snapshot, and its memory
    /// cell's version.
    struct Zygote {
        snapshot: PathBuf,
        memory: cladefs::CellVersion,
    }

    /// A VM of the ramp, started: its name, its process, the socket its guest connects to, and,
    /// restored, how long its restore took, its guest's readiness after it.
    struct Started {
        name: String,
        firecracker: Firecracker,
        listening: Listening,
        restored: Option<Duration>,
    }

    impl Host<'_> {
        /// The room for VMs now ([`room`]).
        fn room(&self) -> anyhow::Result<u64> {
            let host = governor::host_memory().context("the host's memory")?;
            let headroom = governor::headroom(host.total).map_err(anyhow::Error::msg)?;
            let kept = governor::floor(governor::limit(host.total)).map_err(anyhow::Error::msg)?;
            let gauges = self.memory.gauges()?;
            let cached =
                *gauges.get("cladefsd_memory_bytes").context("no cladefsd_memory_bytes")?;
            Ok(room(host.available, cached, headroom, kept))
        }

        /// The directory the VMs' files are in: their consoles, logs and configurations.
        fn vm_dir(&self) -> anyhow::Result<&Path> {
            self.root.parent().context("the root has no parent")
        }

        /// Starts VM `vm`: restores the zygote, if the case has one, or boots: none if the daemon
        /// refused its fork, the host's memory full.
        fn start(&self, vm: usize) -> anyhow::Result<Option<Started>> {
            let name = format!("cell-{vm}");
            match &self.zygote {
                Some(zygote) => self.restore(name, zygote).map(Some),
                None => self.boot(name),
            }
        }

        /// Boots the VM `name` on a fork of the base of its own: none if the daemon refused the
        /// fork, the host's memory full.
        fn boot(&self, name: String) -> anyhow::Result<Option<Started>> {
            let fork = match control(&self.socket)?
                .call(async |client| {
                    client
                        .fork_cell(ForkCellRequest {
                            tenant: self.base.tenant,
                            cell: self.base.id,
                            ..Default::default()
                        })
                        .await
                })
                .map(|response| cladefs::CellId { tenant: response.tenant, id: response.cell })
            {
                Err(err) if err.errno() == Some(cladefs::Errno::ENOMEM) => {
                    eprintln!("cells-per-host: {name}'s fork refused: {err:#}");
                    return Ok(None);
                }
                forked => forked.with_context(|| format!("forking the base for {name}"))?,
            };
            let listening = Listening::new(&name)?;
            let mut machine = Vm::new(1, self.settings.vm_mib)?;
            machine.guest_kernel()?;
            machine.cell_id_served("root", fork, CAPACITY, self.window, &self.socket);
            machine.fxfs_root("root");
            machine.vsock(&listening.uds);
            let firecracker = machine
                .spawn_api(&self.root, &self.test_setup.test_case, &name, &Vm::api_socket(&name))
                .with_context(|| format!("booting {name}"))?;
            Ok(Some(Started { name, firecracker, listening, restored: None }))
        }

        /// Restores `zygote` as the VM `name`: its root a fork of the zygote's version, its
        /// memory a fork of the zygote's memory cell's, each placed by the daemon.
        fn restore(&self, name: String, zygote: &Zygote) -> anyhow::Result<Started> {
            let listening = Listening::new(&name)?;
            let api = Vm::api_socket(&name);
            let firecracker = Firecracker::spawn(self.vm_dir()?, &name, &api, None, &[])?;
            let version = zygote.memory;
            let load = serde_json::json!({
                "snapshot_path": zygote.snapshot,
                "mem_backend": {
                    "backend_type": "Cell",
                    "backend_path": "0/0",
                },
                "memory_cell": {
                    "tenant": version.tenant,
                    "cell": version.cell,
                    "version": version.version,
                    "cladefsd_socket": self.socket,
                },
                "resume_vm": true,
                "vsock_override": { "uds_path": listening.uds },
                "cladefs_overrides": [
                    { "id": "root", "path_on_host": "0/0" },
                ],
            });
            let restoring = Instant::now();
            (firecracker.request("PUT", "/snapshot/load", &load))
                .with_context(|| format!("restoring the zygote as {name}"))?;
            let restored = Some(restoring.elapsed());
            Ok(Started { name, firecracker, listening, restored })
        }

        /// Makes the zygote (the module's docs): boots it, and once its guest says it is frozen,
        /// makes its snapshot and its memory cell's version, and lets it go.
        fn make_zygote(&self) -> anyhow::Result<Zygote> {
            let made = Instant::now();
            let started = self.boot("zygote".to_owned())?.context("the zygote's fork refused")?;
            let Started { name, mut firecracker, listening, .. } = started;
            let within = Duration::from_secs(self.settings.ready_secs);
            let stream = (firecracker.accept_within(&listening.listener, Some(within)))
                .context("the zygote's guest")?;
            let mut said = String::new();
            BufReader::new(&stream).read_line(&mut said)?;
            ensure!(said == "frozen\n", "the zygote's guest said {said:?}");
            let dir = &self.test_setup.tmp_dir;
            let (snapshot, memory) = (dir.join("zygote.snapshot"), dir.join("zygote.memory"));
            firecracker.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            firecracker.request(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": snapshot,
                    "mem_file_path": memory,
                }),
            )?;
            // Its guest waits on `stream` until it goes: it never reads it reset, as the VMs
            // restored from it do.
            firecracker.kill()?;
            drop(stream);
            std::fs::remove_file(Vm::api_socket(&name))?;
            let version = crate::common::memory_version(&self.socket, &memory)?;
            std::fs::remove_file(&memory)?;
            eprintln!("cells-per-host: made the zygote in {:.1?}", made.elapsed());
            Ok(Zygote { snapshot, memory: version })
        }

        /// Whether the daemon let a VM process go, as its log says.
        fn let_go(&self) -> anyhow::Result<Vec<String>> {
            let log = self.test_setup.tmp_dir.join("cladefsd.log");
            let log = std::fs::read_to_string(&log)
                .with_context(|| format!("reading {}", log.display()))?;
            Ok((log.lines())
                .filter(|line| line.contains("letting a VM process go"))
                .map(str::to_owned)
                .collect())
        }

        /// Where each VM of `cells` goes, as [`breakdown`] says, its guests' memory as they said
        /// it in `heard`, KVM's page tables less `kvm_before`, before the first VM.
        fn breakdown(
            &self,
            cells: &Cells,
            heard: &Heard,
            kvm_before: u64,
        ) -> anyhow::Result<String> {
            let mut vmms = Vec::new();
            for cell in &cells.0 {
                let pid = cell.firecracker.pid();
                let rollup = crate::measure::rollup(pid)
                    .with_context(|| format!("{}'s VM process's memory", cell.name))?;
                let status = crate::measure::status(pid)
                    .with_context(|| format!("{}'s VM process's status", cell.name))?;
                let field = |of: &BTreeMap<String, u64>, name: &str| {
                    of.get(name).copied().with_context(|| format!("{}: no {name}", cell.name))
                };
                vmms.push(Vmm {
                    pss: field(&rollup, "Pss")?,
                    anon: field(&rollup, "Pss_Anon")?,
                    shmem: field(&rollup, "Pss_Shmem")?,
                    page_tables: field(&status, "VmPTE")?,
                });
            }
            let guests: Vec<_> =
                heard.vms.values().filter_map(|guest| guest.memory.as_ref()).collect();
            let daemon = crate::measure::rollup(self.daemon.id()).context("the daemon's memory")?;
            let daemon_anon = *daemon.get("Pss_Anon").context("the daemon: no Pss_Anon")?;
            let gauges = self.memory.gauges()?;
            let cache = *gauges.get("cladefsd_memory_bytes").context("no cladefsd_memory_bytes")?;
            let kvm = kvm_tables()?.saturating_sub(kvm_before);
            let mut line = breakdown(
                u64::from(self.settings.vm_mib) * MIB,
                &guests,
                &vmms,
                kvm,
                daemon_anon,
                cache,
            )?;
            // cladefs: what of the daemon's pages is in swap (zram, `cladefsd --setup-host`'s), its
            // extent cache's among them, outside its budget until it evicts them (board 14532);
            // and what its governor reclaimed of the VMs' cgroups (board 14533).
            let swapped = *daemon.get("Swap").context("the daemon: no Swap")?;
            let reclaimed = *gauges
                .get("cladefsd_vm_reclaim_bytes_total")
                .context("no cladefsd_vm_reclaim_bytes_total")?;
            line.push_str(&format!(
                " daemon_swap_mib={:.1} vm_reclaim_mib={:.1}",
                swapped as f64 / MIB as f64,
                reclaimed as f64 / MIB as f64
            ));
            Ok(line)
        }
    }

    /// Fails, at `step`, if the case does now, each check failed recorded in `measured` by its
    /// kind, the words before its first colon: a VM gone, a guest gone or silent, or the daemon
    /// having let a VM process go.
    fn check(
        step: usize,
        cells: &mut Cells,
        heard: &Mutex<Heard>,
        host: &Host,
        measured: &mut Measured,
    ) -> anyhow::Result<()> {
        let failed = failures(cells, &heard.lock().unwrap(), host)?;
        for failure in &failed {
            let kind = failure.split_once(':').map_or("check", |(kind, _)| kind);
            measured.failed(kind, failure.clone());
        }
        ensure!(failed.is_empty(), "step {step}: {}", failed.join("; "));
        Ok(())
    }

    /// Why the case fails now, if it does ([`check`]).
    fn failures(cells: &mut Cells, heard: &Heard, host: &Host) -> anyhow::Result<Vec<String>> {
        let mut failed = Vec::new();
        for (vm, cell) in cells.0.iter_mut().enumerate() {
            if !cell.firecracker.running()? {
                let console = cell.firecracker.console().display();
                failed.push(format!("vm: {} exited: see {console}", cell.name));
                continue;
            }
            let guest = heard.vms.get(&vm);
            if let Some(gone) = guest.and_then(|guest| guest.gone.as_ref()) {
                failed.push(format!("vm: {}'s guest: {gone}", cell.name));
            } else if let Some(last) = guest.and_then(|guest| guest.last)
                && last.elapsed() > Duration::from_secs(SILENT_SECS)
            {
                failed.push(format!(
                    "vm: {}'s guest said nothing for {:.0} s",
                    cell.name,
                    last.elapsed().as_secs_f64()
                ));
            }
        }
        failed.extend(host.let_go()?.into_iter().map(|line| format!("let go: {line}")));
        Ok(failed)
    }

    /// The image the base is imported from, in an OCI layout at `layout`: the root `root`
    /// holds, as one layer.
    fn image(root: &Path, layout: &Path) -> anyhow::Result<()> {
        // As root's, as an image's files are: the runner's user unpacked them.
        let tar = std::process::Command::new("tar")
            .args(["--numeric-owner", "--owner=0", "--group=0", "-C"])
            .arg(root)
            .args(["-cf", "-", "."])
            .output()
            .context("running tar")?;
        ensure!(tar.status.success(), "tar of {}: {}", root.display(), tar.status);
        oci_layout(layout, "cladefs/cells:1", &[tar.stdout])?;
        Ok(())
    }

    impl TestCellsPerHost {
        /// Ramps, saying each step and what it measured in `measured`.
        fn run(&self, test_setup: &TestSetup, measured: &mut Measured) -> anyhow::Result<()> {
            let settings = Settings::from_env()?;
            eprintln!("cells-per-host: {settings:?}, dax {}, zygote {}", self.dax, self.zygote);
            // Each VM takes a few of this process's files: its vsock's, its process's.
            cladefs::cladefsd::raise_open_files_limit().context("raising the open files' limit")?;
            let dir = &test_setup.tmp_dir;
            let root = setup_rootfs(test_setup)?;
            let layout = dir.join("cells-image");
            image(&root, &layout)?;
            // Short, as a Unix socket's path is.
            let sockets = tempfile::Builder::new().prefix("cladefs-cells").tempdir()?;
            let socket = sockets.path().join("cladefsd.sock");
            let metrics = crate::measure::metrics_address()?;
            let metrics_at = metrics.to_string();
            let env = [("CLADEFSD_METRICS", OsStr::new(&metrics_at))];
            // Governed, as production's: its memory what is free (TASK-358), unless
            // CLADEFSD_MEMORY fixes it (an A/B's fixed side, board 14529).
            let daemon = crate::common::spawn_daemon(test_setup, &socket, "cladefsd", &env, None)?;
            let imported = Instant::now();
            let base = control(&socket)?
                .call(async |client| {
                    client
                        .import_image(ImportImageRequest {
                            capacity: CAPACITY,
                            image: control_path(&layout),
                            reference: String::new(),
                            tenant: 0,
                        })
                        .await
                })
                .map(|response| cladefs::CellId { tenant: response.tenant, id: response.cell })?;
            eprintln!("cells-per-host: imported the base in {:.1?}", imported.elapsed());
            let memory = HostMemory::new(daemon.id(), metrics);
            let window = if self.dax { settings.window } else { 0 };
            let mut host = Host {
                test_setup,
                root,
                socket,
                base,
                daemon,
                memory,
                settings,
                window,
                zygote: None,
            };
            let ramped = match self.zygote {
                true => host.make_zygote().map(|zygote| host.zygote = Some(zygote)),
                false => Ok(()),
            }
            .and_then(|()| self.ramp(&host, measured));
            let mut daemon = host.daemon;
            daemon.kill()?;
            daemon.wait()?;
            ramped?;
            // Passed: the forks, the base and the daemon's caches go, many of them; the logs stay.
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let bulky =
                    [".root", "-cache", ".bucket", "-disk0"].iter().any(|end| name.ends_with(end))
                        || ["base", "cells-image", "rootfs"].contains(&&*name);
                if bulky && path.is_dir() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("removing {}", path.display()))?;
                }
            }
            Ok(())
        }

        fn ramp(&self, host: &Host, measured: &mut Measured) -> anyhow::Result<()> {
            let settings = host.settings;
            let heard = Arc::new(Mutex::new(Heard::default()));
            let mut cells = Cells::default();
            host.memory.say(0, "base", None, "")?;
            let kvm_before = kvm_tables()?;
            let mut ramp = Ramp::new(settings, host.room()?);
            let (mut step, mut last, mut full) = (0, None, false);
            let limit = loop {
                if full {
                    break Limit::Full;
                }
                let room = host.room()?;
                let added = match ramp.next(room) {
                    Next::Stop(limit) => break limit,
                    // A window past a budget is judged again with no VM added.
                    Next::Add(_) if ramp.past.is_some() => 0,
                    Next::Add(count) => count,
                };
                step += 1;
                let first = cells.0.len();
                let mut booting = Vec::new();
                let spawned = Instant::now();
                for vm in first..first + added {
                    // Each VM's budget runs from its own spawn: a step's later VMs spawn long
                    // after its first where spawning is slow (the jail's, board 5588), and were
                    // failed for it, by none of their own.
                    let began = Instant::now();
                    match host.start(vm)? {
                        Some(started) => booting.push((began, started)),
                        // The VMs booted are judged, then the ramp stops.
                        None => {
                            full = true;
                            break;
                        }
                    }
                }
                let spawn_secs = spawned.elapsed().as_secs_f64();
                let within = Duration::from_secs(settings.ready_secs);
                let began: Vec<Instant> = booting.iter().map(|(began, _)| *began).collect();
                let restored: Vec<Option<Duration>> =
                    booting.iter().map(|(_, started)| started.restored).collect();
                for (at, (began, started)) in booting.into_iter().enumerate() {
                    let Started { name, mut firecracker, listening, .. } = started;
                    let left = within.saturating_sub(began.elapsed());
                    let stream = (firecracker.accept_within(&listening.listener, Some(left)))
                        .with_context(|| format!("{name}'s guest"))?;
                    let to_guest = stream.try_clone()?;
                    let heard = heard.clone();
                    std::thread::spawn(move || hear(first + at, stream, heard));
                    cells.0.push(Cell { name, firecracker, to_guest });
                }
                let boots = loop {
                    // A booted VM's kernel's start to its first round done, as its guest says; a
                    // restored one's restore, and its guest's reset found to its first round done.
                    let ready: Vec<Option<u64>> = {
                        let heard = heard.lock().unwrap();
                        (first..cells.0.len())
                            .zip(&restored)
                            .map(|(vm, restored)| {
                                let said = heard.vms.get(&vm)?.ready_ms?;
                                let restore = restored.map_or(0, |took| took.as_millis() as u64);
                                Some(restore + said)
                            })
                            .collect()
                    };
                    if let Some(ready) = ready.iter().copied().collect::<Option<Vec<u64>>>() {
                        break ready;
                    }
                    check(step, &mut cells, &heard, host, measured)?;
                    let late = (ready.iter().zip(&began))
                        .position(|(ready, began)| ready.is_none() && began.elapsed() >= within);
                    if let Some(late) = late {
                        anyhow::bail!(
                            "step {step}: {} wasn't ready within {within:?} of its spawn",
                            cells.0[first + late].name
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                };
                let readied_secs = spawned.elapsed().as_secs_f64();
                // The window: every sample of it, none of the boots'.
                heard.lock().unwrap().samples.clear();
                std::thread::sleep(Duration::from_secs(settings.window_secs));
                let samples = std::mem::take(&mut heard.lock().unwrap().samples);
                check(step, &mut cells, &heard, host, measured)?;
                let (past, judged) = past_budget(&samples, &boots);
                let running = cells.0.len();
                let room = host.room()?;
                let per_vm = ramp.room_before.saturating_sub(room) / running as u64;
                let mut line = format!(
                    "cells-per-host: step={step} vms={running} added={added} room_mib={} \
                     per_vm_mib={:.1} spawn_s={spawn_secs:.1} ready_s={readied_secs:.1} \
                     boot_p99_ms={} samples={}",
                    room / MIB,
                    per_vm as f64 / MIB as f64,
                    percentile(&mut boots.clone(), 99),
                    samples.len(),
                );
                for ((name, _), (p99, worst)) in PROBES.iter().zip(&judged) {
                    line.push_str(&format!(" {name}_p99_ms={p99} {name}_max_ms={worst}"));
                }
                line.push_str(&host.breakdown(&cells, &heard.lock().unwrap(), kvm_before)?);
                if let Some(past) = past {
                    line.push_str(&format!(" past={past}"));
                }
                eprintln!("{line}");
                host.memory.say(running, &format!("step-{step}"), None, "")?;
                match ramp.judged(running, past) {
                    Verdict::Held => last = Some((per_vm, boots, judged)),
                    Verdict::Again => {}
                    Verdict::Stop(limit) => break limit,
                }
            };
            eprintln!("cells-per-host: held={} limit={limit}", ramp.held);
            // cladefs: a host whose memory ran out has had its governor reclaim its VMs' cold
            // memory first (`governor::reclaim_connected_vms`, board 14533), each VM in a cgroup
            // of its own (`crate::cgroups`).
            let reclaimed = *(host.memory.gauges()?)
                .get("cladefsd_vm_reclaim_bytes_total")
                .context("no cladefsd_vm_reclaim_bytes_total")?;
            let out_of_memory = matches!(limit, Limit::Memory | Limit::Full);
            measured.higher("cells.held", "VMs", ramp.held as f64);
            if let Some((per_vm, mut boots, judged)) = last {
                measured.lower("cells.per_vm", "MiB", per_vm as f64 / MIB as f64);
                measured.lower("cells.boot_p99", "ms", percentile(&mut boots, 99) as f64);
                for ((name, _), (p99, _)) in PROBES.iter().zip(judged) {
                    measured.lower(format!("cells.{name}_p99"), "ms", p99 as f64);
                }
            }
            self.stop(&mut cells, measured)?;
            ensure!(
                !out_of_memory || reclaimed > 0,
                "the host's memory ran out ({limit}) and the governor reclaimed none of its VMs'"
            );
            ensure!(
                ramp.held >= settings.floor,
                "held {} VMs, fewer than CLADEFS_CELLS_FLOOR's {}",
                ramp.held,
                settings.floor
            );
            Ok(())
        }

        /// Tells every VM of `cells` to stop, and fails unless each guest ends saying so.
        fn stop(&self, cells: &mut Cells, measured: &mut Measured) -> anyhow::Result<()> {
            for cell in &mut cells.0 {
                // One that can't be told is found below, as it doesn't end well.
                if let Err(err) = cell.to_guest.write_all(b"stop\n") {
                    eprintln!("cells-per-host: telling {} to stop: {err}", cell.name);
                }
            }
            let mut failed = Vec::new();
            for cell in cells.0.drain(..) {
                let (status, console) = cell.firecracker.wait()?;
                let said = crate::vm::guest_stdout(&std::fs::read(&console)?);
                if !status.success() || !said.lines().any(|line| line == "OK cell") {
                    failed.push(format!("vm: {} exited {status}, saying {said:?}", cell.name));
                }
                let api = Vm::api_socket(&cell.name);
                std::fs::remove_file(&api)
                    .with_context(|| format!("removing {}", api.display()))?;
            }
            for failure in &failed {
                measured.failed("vm", failure.clone());
            }
            ensure!(failed.is_empty(), "{}", failed.join("; "));
            Ok(())
        }
    }

    impl Test for TestCellsPerHost {
        fn linux_only(&self) -> Option<&'static str> {
            Some(crate::LINUX_ONLY_HOST_MEASURES)
        }

        fn should_run(&self) -> ShouldRun {
            if std::env::var_os("CLADEFSD_BIN").is_none() {
                return ShouldRun::No("CLADEFSD_BIN does not name the extent daemon");
            }
            cladefs_guest_should_run()
        }

        fn rootfs_image(&self) -> Option<crate::rootfs::Image> {
            Some(crate::rootfs::Image::new(CONTAINERFILE))
        }

        fn timeout_secs(&self) -> u64 {
            // Restored from a zygote, a host holds VMs past 500, its steps' forks slower as its
            // disk fills with their commits: 54 minutes to 497 VMs on ubu1 (board 7084).
            if self.zygote { 3 * 3600 } else { 5400 }
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut measured = Measured::new(&test_setup.test_case);
            let ran = self.run(&test_setup, &mut measured);
            if let Err(err) = &ran {
                eprintln!("{err:#}");
                if !measured.has_failed() {
                    measured.failed("error", format!("{err:#}"));
                }
            }
            if let Err(err) = measured.save() {
                eprintln!("{err:#}");
                std::process::exit(1);
            }
            std::process::exit(if ran.is_ok() { 0 } else { 1 });
        }

        fn check(self: Box<Self>, _stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
            TestOutcome::Pass
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn settings() -> Settings {
            Settings {
                vm_mib: 256,
                window: 0,
                step: 16,
                max: 1000,
                floor: 1,
                window_secs: 10,
                ready_secs: 120,
            }
        }

        const GIB: u64 = 1 << 30;

        /// The first step is one VM, its cost not known; then half the room at what each took, a
        /// step at most, one at least; and none once the room holds less than one.
        #[test]
        fn steps_are_half_the_room_at_each_vms_cost() {
            let before = 20 * GIB;
            // The room left once `vms` VMs took 100 MiB each.
            let left = |vms: u64| before - vms * 100 * MIB;
            let mut ramp = Ramp::new(settings(), before);
            assert_eq!(ramp.per_vm(before), None);
            assert_eq!(ramp.next(before), Next::Add(1));
            assert_eq!(ramp.judged(1, None), Verdict::Held);
            // 19.9 GiB holds 203 more: half of it is past a step.
            assert_eq!(ramp.per_vm(left(1)), Some(100 * MIB));
            assert_eq!(ramp.next(left(1)), Next::Add(16));
            assert_eq!(ramp.judged(200, None), Verdict::Held);
            // 480 MiB holds 4, half of it 2; 280 MiB holds 2, half of it 1; 180 MiB holds 1.
            assert_eq!(ramp.next(left(200)), Next::Add(2));
            assert_eq!(ramp.judged(202, None), Verdict::Held);
            assert_eq!(ramp.next(left(202)), Next::Add(1));
            assert_eq!(ramp.judged(203, None), Verdict::Held);
            assert_eq!(ramp.next(left(203)), Next::Add(1));
            assert_eq!(ramp.judged(204, None), Verdict::Held);
            // 80 MiB holds none.
            assert_eq!(ramp.next(left(204)), Next::Stop(Limit::Memory));
        }

        /// A step never takes the ramp past its most VMs, which stop it.
        #[test]
        fn the_most_vms_cap_a_step_and_stop_the_ramp() {
            let mut ramp = Ramp::new(Settings { max: 20, ..settings() }, 20 * GIB);
            assert_eq!(ramp.judged(17, None), Verdict::Held);
            assert_eq!(ramp.next(20 * GIB - 17 * MIB), Next::Add(3));
            assert_eq!(ramp.judged(20, None), Verdict::Held);
            assert_eq!(ramp.next(20 * GIB - 20 * MIB), Next::Stop(Limit::Max));
        }

        /// The room grown since the first VM, as the daemon gave memory back, sizes a step as
        /// though each VM took nothing: a step.
        #[test]
        fn a_room_grown_gives_a_whole_step() {
            let mut ramp = Ramp::new(settings(), 10 * GIB);
            assert_eq!(ramp.judged(1, None), Verdict::Held);
            assert_eq!(ramp.per_vm(11 * GIB), Some(0));
            assert_eq!(ramp.next(11 * GIB), Next::Add(16));
        }

        /// A window past a budget is judged again; two in a row stop the ramp at the VMs of the
        /// step before, and one within the budgets between clears the first.
        #[test]
        fn two_windows_past_a_budget_in_a_row_stop_the_ramp() {
            let mut ramp = Ramp::new(settings(), 20 * GIB);
            assert_eq!(ramp.judged(17, None), Verdict::Held);
            assert_eq!(ramp.judged(33, Some("fsync")), Verdict::Again);
            assert_eq!(ramp.held, 17);
            assert_eq!(ramp.judged(33, None), Verdict::Held);
            assert_eq!(ramp.held, 33);
            assert_eq!(ramp.judged(49, Some("tick")), Verdict::Again);
            assert_eq!(ramp.judged(49, Some("echo")), Verdict::Stop(Limit::Latency("echo")));
            assert_eq!(ramp.held, 33);
            assert_eq!(Limit::Latency("echo").to_string(), "latency:echo");
            assert_eq!(Limit::Full.to_string(), "full");
        }

        /// The room is what is available and the daemon's memory, less the headroom and the floor
        /// the daemon keeps, none if that is more.
        #[test]
        fn the_room_counts_the_daemons_memory_and_leaves_the_headroom() {
            assert_eq!(room(4 * GIB, 6 * GIB, GIB, 0), 9 * GIB);
            assert_eq!(room(4 * GIB, 6 * GIB, GIB, GIB / 2), 8 * GIB + GIB / 2);
            assert_eq!(room(GIB / 2, 0, GIB, 0), 0);
        }

        /// The nearest-rank percentile: of 100 values, the 99th; of 10, the largest.
        #[test]
        fn percentiles_are_nearest_rank() {
            let mut hundred: Vec<u64> = (1..=100).rev().collect();
            assert_eq!(percentile(&mut hundred, 99), 99);
            assert_eq!(percentile(&mut hundred, 50), 50);
            let mut ten: Vec<u64> = (1..=10).collect();
            assert_eq!(percentile(&mut ten, 99), 10);
            assert_eq!(percentile(&mut [], 99), 0);
        }

        /// A window is past the first budget its p99 is past, in the probes' order, then the
        /// boots'; each probe's p99 and worst said in ms.
        #[test]
        fn a_window_is_past_its_first_budget_past() {
            let quick = [5_000, 2_000, 3_000, 40_000];
            let mut samples = vec![quick; 99];
            samples.push([5_000, 2_000, 3_000, 900_000]);
            assert_eq!(
                past_budget(&samples, &[800]),
                (None, vec![(5, 5), (2, 2), (3, 3), (40, 900)])
            );
            let slow = [5_000, 150_000, 3_000, 300_000];
            let (past, _) = past_budget(&[slow; 10], &[800]);
            assert_eq!(past, Some("echo"));
            let (past, _) = past_budget(&[quick; 10], &[800, 12_000]);
            assert_eq!(past, Some("boot"));
        }

        /// What a guest says each round reads back as the probes' microseconds, and a line of
        /// another shape is no round.
        #[test]
        fn a_guests_probe_line_reads_back() {
            let line = probe_line(&[12, 3_400, 560, 78_000]);
            assert_eq!(line, "probe tick=12 echo=3400 cat=560 fsync=78000");
            assert_eq!(probe_of(&line), Some([12, 3_400, 560, 78_000]));
            assert_eq!(probe_of("probe tick=12 echo=3400 cat=560"), None);
            assert_eq!(probe_of("probe tick=12 echo=3400 fsync=1 cat=560"), None);
            assert_eq!(probe_of("ready uptime_ms=812"), None);
        }

        /// A guest's memory line reads back by its fields' names less `guest_`; a line of another
        /// shape, or a field of no number, is none.
        #[test]
        fn a_guests_memory_line_reads_back() {
            let memory = memory_of("memory guest_total=230686720 guest_slab=9437184").unwrap();
            assert_eq!(
                memory,
                BTreeMap::from([("slab".to_owned(), 9_437_184), ("total".to_owned(), 230_686_720)])
            );
            assert_eq!(memory_of("memory guest_total=230686720 slab=9437184"), None);
            assert_eq!(memory_of("memory guest_total=lots"), None);
            assert_eq!(memory_of("probe tick=12 echo=3400 cat=560 fsync=78000"), None);
        }

        /// A step's breakdown is each part's mean over the VMs, in MiB: the guests' RAM their
        /// kernels never had and what isn't available among them, the VM processes', and KVM's and
        /// the daemon's over the VM processes' count.
        #[test]
        fn a_breakdown_is_the_mean_of_the_vms() {
            let guest = |total: u64, available: u64| {
                BTreeMap::from(
                    [
                        ("total", total),
                        ("available", available),
                        ("slab", 8 * MIB),
                        ("kernel_stack", MIB),
                        ("page_tables", MIB / 2),
                        ("cached", 20 * MIB),
                        ("memmap", 4 * MIB),
                    ]
                    .map(|(name, bytes)| (name.to_owned(), bytes)),
                )
            };
            let (one, two) = (guest(220 * MIB, 180 * MIB), guest(218 * MIB, 168 * MIB));
            let vmm =
                |pss: u64| Vmm { pss, anon: pss - 10 * MIB, shmem: 10 * MIB, page_tables: MIB };
            let line = breakdown(
                256 * MIB,
                &[&one, &two],
                &[vmm(60 * MIB), vmm(80 * MIB)],
                6 * MIB,
                3 * MIB,
                50 * MIB,
            )
            .unwrap();
            assert_eq!(
                line,
                " guest_reserved_mib=37.0 guest_used_mib=45.0 guest_slab_mib=8.0 guest_stack_mib=1.0 \
                 guest_pt_mib=0.5 guest_cached_mib=20.0 guest_memmap_mib=4.0 vmm_pss_mib=70.0 \
                 vmm_anon_mib=60.0 vmm_shmem_mib=10.0 vmm_pt_mib=1.0 kvm_pt_mib=3.0 \
                 daemon_anon_mib=1.5 daemon_cache_mib=25.0"
            );
        }

        /// A guest that said no part of its memory fails the breakdown rather than count it none.
        #[test]
        fn a_guest_missing_a_part_fails_the_breakdown() {
            let guest = BTreeMap::from([("total".to_owned(), 220 * MIB)]);
            let err = breakdown(256 * MIB, &[&guest], &[], 0, 0, 0).unwrap_err();
            assert_eq!(err.to_string(), "a guest said no available");
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::FileExt as _;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// How many rounds go between the guest's `memory` lines ([`memory_of`]): one each window
    /// of the host's (CLADEFS_CELLS_WINDOW_SECS, 10 by default).
    const MEMORY_ROUNDS: u64 = 10;

    /// The file of its own each round writes over and fsyncs, on the fork's root.
    const PROBED: &str = "/var/cells-probe";

    /// The guest's probes, as [`PROBES`] names them.
    struct Probes {
        own: fs::File,
    }

    impl Probes {
        fn new() -> Self {
            let own = fs::OpenOptions::new().write(true).create(true).truncate(false).open(PROBED);
            Self { own: own.unwrap_or_else(|err| panic!("opening {PROBED}: {err}")) }
        }

        /// A round, `late` its start's lateness: each probe's microseconds, in [`PROBES`]' order.
        fn round(&mut self, late: Duration) -> [u64; PROBES.len()] {
            let timed = |run: &mut dyn FnMut()| {
                let began = Instant::now();
                run();
                began.elapsed().as_micros() as u64
            };
            let command = |argv: &[&str]| {
                let status = Command::new(argv[0])
                    .args(&argv[1..])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                assert!(
                    status.as_ref().is_ok_and(|status| status.success()),
                    "{argv:?}: {status:?}"
                );
            };
            let echo = timed(&mut || command(&["/bin/echo", "cell"]));
            let cat = timed(&mut || command(&["/bin/cat", "/etc/os-release"]));
            let own = &self.own;
            let fsync = timed(&mut || {
                own.write_all_at(&[0x5a; 4096], 0).expect("writing the probe's file");
                own.sync_all().expect("fsyncing the probe's file");
            });
            [late.as_micros() as u64, echo, cat, fsync]
        }
    }

    /// Milliseconds since the guest's kernel started (/proc/uptime).
    fn uptime_ms() -> u64 {
        let uptime = fs::read_to_string("/proc/uptime").expect("/proc/uptime");
        let secs: f64 = uptime.split(' ').next().unwrap().parse().expect("/proc/uptime's seconds");
        (secs * 1000.0) as u64
    }

    /// `FIFREEZE` and `FITHAW`: `_IOWR('X', 119, int)` and `_IOWR('X', 120, int)`.
    const FIFREEZE: nix::libc::c_ulong = 0xc004_5877;
    const FITHAW: nix::libc::c_ulong = 0xc004_5878;

    /// Freezes, or thaws, the root filesystem.
    fn freeze(request: nix::libc::c_ulong) {
        use std::os::fd::AsRawFd as _;
        let root = fs::File::open("/").expect("opening the root");
        // SAFETY: an ioctl of the root's descriptor, which outlives it, that takes no argument.
        let ret = unsafe { nix::libc::ioctl(root.as_raw_fd(), request as _, 0) };
        assert_eq!(ret, 0, "ioctl {request:#x}: {}", std::io::Error::last_os_error());
    }

    /// As the zygote (the module's docs): freezes the root, tells the host, and waits on the
    /// connection, which the zygote goes with; restored, a VM finds it reset, and thaws. Before a
    /// restore's reset no other connection is made: one made before it would be reset with it.
    /// Returns when the reset was found.
    fn be_the_zygote() -> Instant {
        use std::io::Read as _;
        freeze(FIFREEZE);
        let mut host = connect();
        host.write_all(b"frozen\n").expect("telling the host");
        let mut rest = Vec::new();
        let _ = host.read_to_end(&mut rest);
        let restored = Instant::now();
        freeze(FITHAW);
        restored
    }

    /// The connection to the host, on [`PORT`].
    fn connect() -> fs::File {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, VsockAddr, connect, socket};
        use std::os::fd::AsRawFd as _;
        let socket = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
            .expect("a vsock socket");
        connect(socket.as_raw_fd(), &VsockAddr::new(2, PORT)).expect("connecting to the host");
        fs::File::from(socket)
    }

    impl Test for TestCellsPerHost {
        fn in_guest(self: Box<Self>) {
            let mut probes = Probes::new();
            // The boot's working set: what the probes read, read once.
            probes.round(Duration::ZERO);
            // Restored, ready as soon as it thawed: its probes' working set read already.
            let ready_ms = match self.zygote {
                true => be_the_zygote().elapsed().as_millis() as u64,
                false => uptime_ms(),
            };
            let mut to_host = connect();
            writeln!(to_host, "ready uptime_ms={ready_ms}").expect("telling the host");
            let memory = |to_host: &mut fs::File| {
                writeln!(to_host, "memory {}", crate::measure::itself()).expect("telling the host")
            };
            memory(&mut to_host);
            let stop = Arc::new(AtomicBool::new(false));
            let told = std::thread::spawn({
                let (stop, from_host) = (stop.clone(), to_host.try_clone().unwrap());
                move || {
                    let mut said = String::new();
                    BufReader::new(from_host).read_line(&mut said).expect("hearing the host");
                    assert_eq!(said, "stop\n", "the host's word");
                    stop.store(true, Ordering::Relaxed);
                }
            });
            let mut due = Instant::now();
            for round in 1.. {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                due += Duration::from_secs(1);
                let now = Instant::now();
                if let Some(wait) = due.checked_duration_since(now) {
                    std::thread::sleep(wait);
                }
                let late = Instant::now().saturating_duration_since(due);
                // A round or more behind, it goes on from now rather than run rounds back to back.
                if late >= Duration::from_secs(1) {
                    due = Instant::now();
                }
                let micros = probes.round(late);
                writeln!(to_host, "{}", probe_line(&micros)).expect("telling the host");
                // A window's worth of rounds: the host's breakdown of a step, its last window's.
                if round % MEMORY_ROUNDS == 0 {
                    memory(&mut to_host);
                }
            }
            told.join().unwrap();
            println!("OK cell");
        }
    }
}
