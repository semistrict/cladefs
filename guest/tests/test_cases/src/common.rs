// Copyright The libkrun Authors.
// Copyright 2026 The cladefs Authors: changed for cladefs's guest tests.
// SPDX-License-Identifier: Apache-2.0

//! Common utilities used by multiple test

use anyhow::Context;
use std::fs;
use std::fs::create_dir;
use std::path::{Path, PathBuf};

use crate::{ShouldRun, TestSetup};

fn copy_guest_agent(dir: &Path) -> anyhow::Result<()> {
    let path = std::env::var_os("CLADEFS_GUEST_AGENT")
        .context("CLADEFS_GUEST_AGENT does not name the guest agent")?;

    let output_path = dir.join("guest-agent");
    fs::copy(path, output_path).context("Failed to copy executable into vm")?;
    Ok(())
}

/// Creates the root filesystem directory, if the runner didn't, and copies the guest agent into
/// it. Returns the path to the root directory.
pub fn setup_rootfs(test_setup: &TestSetup) -> anyhow::Result<PathBuf> {
    let root_dir = test_setup.tmp_dir.join("rootfs");
    if !root_dir.exists() {
        create_dir(&root_dir).context("Failed to create rootfs directory")?;
    }
    copy_guest_agent(&root_dir)?;
    Ok(root_dir)
}

/// Whether a test of cladefs's guest kernel's Fxfs runs: CLADEFS_FIRECRACKER names the VMM, and
/// CLADEFS_GUEST_KERNEL the kernel, and CLADEFSD the socket of the extent daemon that serves
/// the device's cell (as cladefs's scripts/test-e2e.sh has them).
pub fn cladefs_guest_should_run() -> ShouldRun {
    if !crate::vm::has_vmm() {
        return ShouldRun::No("CLADEFS_FIRECRACKER does not name cladefs's Firecracker");
    }
    if std::env::var_os("CLADEFS_GUEST_KERNEL").is_none() {
        return ShouldRun::No("CLADEFS_GUEST_KERNEL does not name a guest kernel");
    }
    if std::env::var_os("CLADEFSD").is_none() {
        return ShouldRun::No("CLADEFSD does not name the extent daemon's socket");
    }
    ShouldRun::Yes
}

/// cladefs: how many times slower than a release one the guest kernel is, by its kind,
/// CLADEFS_GUEST_KERNEL_BUILD's, as scripts/lib/test-linux-host.sh built it (a release one if
/// unset): what a test's time limits scale by (TASK-168). The debug kernel's measured: its checks
/// of a second or more took 1.37 times the release kernel's at most (GCE r561, r562 against r560,
/// r563). The others' guessed, KASAN's from its outrunning the checks' 180 s where the release
/// kernel took 140 s or so (TASK-44.8).
pub fn guest_kernel_slowness() -> anyhow::Result<f64> {
    match std::env::var("CLADEFS_GUEST_KERNEL_BUILD").unwrap_or_default().as_str() {
        "" | "release" => Ok(1.0),
        "debug" | "rust-checks" | "coverage" => Ok(1.5),
        "kasan" => Ok(4.0),
        kind => anyhow::bail!(
            "CLADEFS_GUEST_KERNEL_BUILD is release, debug, kasan, coverage or rust-checks, not {kind:?}"
        ),
    }
}

/// cladefs: says in the repeat's log what the daemon named `name` the test spawned
/// ([`spawn_daemon`]) kept in its stats file, how long its commits waited to be made durable among
/// it: a passing repeat's directory, and the file in it, doesn't outlive the run (TASK-165.9).
pub fn say_daemon_stats(test_setup: &TestSetup, name: &str) {
    let path = test_setup.tmp_dir.join(format!("{name}-stats.txt"));
    match fs::read_to_string(&path) {
        Ok(kept) => kept.lines().for_each(|line| eprintln!("{name} stats: {line}")),
        Err(err) => eprintln!("{name} stats: reading {}: {err}", path.display()),
    }
    // And its cells' warnings, of flushes and framings past cladefsd's turn among them, in its
    // log, as the stats file, gone with a repeat's directory that passed.
    let log = test_setup.tmp_dir.join(format!("{name}.log"));
    let warned = fs::read_to_string(&log).unwrap_or_default();
    let warned = warned.lines().filter(|line| line.contains("WARN local_cell"));
    for line in warned.take(200) {
        eprintln!("{name} log: {line}");
    }
}

/// cladefs: the `daemon:` line of a daemon's stats file at `path`, its fields by name: what
/// fxfs-guest-shared-disk and fxfs-guest-sandboxes report and check of their daemons' writes.
pub fn daemon_stats(path: &Path) -> anyhow::Result<std::collections::BTreeMap<String, u64>> {
    let kept = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let line = kept.lines().find_map(|line| line.strip_prefix("daemon: "));
    Ok((line.context("no daemon line")?.split(' '))
        .filter_map(|field| {
            let (name, value) = field.split_once('=')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect())
}

/// cladefs: what a process cost its host, as its kernel counts it, whatever the process says of
/// itself: what an A/B of LangSmith's pin and a candidate compares of their daemons, the pin's
/// daemon from before it counted its own writes and memory among them (TASK-330.4).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessCosts {
    /// What its kernel wrote to disks for it (/proc/PID/io's `write_bytes`; macOS's
    /// `ri_diskio_byteswritten`).
    pub disk_written: u64,
    /// The most memory it held at once (/proc/PID/status's `VmHWM`; macOS's
    /// `ri_lifetime_max_phys_footprint`).
    pub peak_rss: u64,
    /// Its CPU time, its user and system time, in seconds (/proc/PID/stat's; macOS's).
    pub cpu_secs: f64,
}

impl ProcessCosts {
    /// The process `pid`'s, as it runs.
    #[cfg(target_os = "linux")]
    pub fn of(pid: u32) -> anyhow::Result<Self> {
        let read = |file: &str| {
            let path = format!("/proc/{pid}/{file}");
            fs::read_to_string(&path).with_context(|| format!("reading {path}"))
        };
        Self::parse(&read("io")?, &read("status")?, &read("stat")?, clock_ticks())
    }

    /// The process `pid`'s, as it runs: what macOS's kernel counts of it ([`usage`]).
    #[cfg(target_os = "macos")]
    #[allow(deprecated, reason = "libc's mach_timebase_info, which mach2 has too: the same call")]
    pub fn of(pid: u32) -> anyhow::Result<Self> {
        use nix::libc;
        let usage = usage(pid)?;
        // Its times are in Mach's absolute time units, of the timebase's fraction of a nanosecond.
        let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: Writes the timebase into `timebase`.
        let read = unsafe { libc::mach_timebase_info(&mut timebase) };
        anyhow::ensure!(read == 0, "the Mach timebase: {read}");
        let ticks = (usage.ri_user_time + usage.ri_system_time) as f64;
        Ok(Self {
            disk_written: usage.ri_diskio_byteswritten,
            peak_rss: usage.ri_lifetime_max_phys_footprint,
            cpu_secs: ticks * f64::from(timebase.numer) / f64::from(timebase.denom) / 1e9,
        })
    }

    /// From a process's /proc files, `io`, `status` and `stat`, and its kernel's clock ticks a
    /// second, `ticks`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux's, and its test's"))]
    fn parse(io: &str, status: &str, stat: &str, ticks: f64) -> anyhow::Result<Self> {
        let field = |text: &str, name: &str| -> anyhow::Result<u64> {
            let line = text.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(':'));
            let value = line.with_context(|| format!("no {name}"))?.trim();
            Ok(value.trim_end_matches(" kB").trim().parse()?)
        };
        // Past the command's parentheses, which may hold spaces: state is the third field, utime
        // the fourteenth, stime the fifteenth.
        let after = &stat[stat.rfind(')').context("no command in stat")? + 2..];
        let fields: Vec<&str> = after.split(' ').collect();
        let ticked = |at: usize| -> anyhow::Result<f64> {
            Ok(fields.get(at - 3).context("a short stat")?.parse::<u64>()? as f64)
        };
        Ok(Self {
            disk_written: field(io, "write_bytes")?,
            peak_rss: field(status, "VmHWM")? << 10,
            cpu_secs: (ticked(14)? + ticked(15)?) / ticks,
        })
    }
}

/// cladefs: what macOS's kernel counts of process `pid` (`proc_pid_rusage`): its memory, resident
/// and its footprint, its disk's writes and its CPU time.
#[cfg(target_os = "macos")]
pub fn usage(pid: u32) -> anyhow::Result<nix::libc::rusage_info_v4> {
    use nix::libc;
    // SAFETY: Plain data, which the call writes.
    let mut usage: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: Writes the process's counts into `usage`, of the flavour's size.
    let read = unsafe {
        libc::proc_pid_rusage(pid as libc::c_int, libc::RUSAGE_INFO_V4, (&raw mut usage).cast())
    };
    anyhow::ensure!(read == 0, "process {pid}'s usage: {}", std::io::Error::last_os_error());
    Ok(usage)
}

#[cfg(target_os = "linux")]
fn clock_ticks() -> f64 {
    // SAFETY: sysconf reads a constant of the system's.
    (unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) }) as f64
}

/// cladefs: a version of a new memory cell of the default tenant's, on the extent daemon at
/// `socket`, holding the memory file `memory` of a VM's snapshot, imported (TASK-456.2): what VMs
/// restore their memory from, sharing its pages.
#[cfg(target_os = "linux")]
pub fn memory_version(socket: &Path, memory: &Path) -> anyhow::Result<cladefs::CellVersion> {
    let size = fs::metadata(memory)?.len();
    let cell = cladefs::memory_cell::MemoryCell::create_cell(socket, 0, size)?;
    cell.import(memory)?;
    let version = cell.make_version()?;
    cell.close()?;
    Ok(version)
}

/// As Linux's, where cladefs's memory cells are not yet: `cladefs_wire::memory_cell` is Linux's
/// (board 14343).
#[cfg(not(target_os = "linux"))]
pub fn memory_version(_socket: &Path, _memory: &Path) -> anyhow::Result<cladefs::CellVersion> {
    anyhow::bail!("memory cells are Linux's yet: cladefs_wire::memory_cell (board 14343)")
}

/// cladefs: the memory limit nearest this process in its cgroups (memory.max, its own cgroup's or
/// the nearest ancestor's that sets one), and what that cgroup holds now that reclaim can't take
/// back (memory.stat's anon and shmem: no swap takes them): what the kernel's OOM killer goes by.
/// None where no cgroup sets a limit.
pub fn cgroup_memory() -> Option<(u64, u64)> {
    let cgroup = fs::read_to_string("/proc/self/cgroup").ok()?;
    let mut dir = Path::new("/sys/fs/cgroup")
        .join(cgroup.lines().find_map(|line| line.strip_prefix("0::"))?.trim_start_matches('/'));
    loop {
        if let Ok(max) = fs::read_to_string(dir.join("memory.max")) {
            let stat = fs::read_to_string(dir.join("memory.stat")).ok()?;
            if let Some(held) = unreclaimable(&max, &stat) {
                return Some(held);
            }
        }
        // The root's, which sets none, is the last.
        if dir == Path::new("/sys/fs/cgroup") || !dir.pop() {
            return None;
        }
    }
}

/// A cgroup's limit, `max` (memory.max's, `max` for none), and what of it its `stat`
/// (memory.stat's) says reclaim can't take back: None for no limit.
fn unreclaimable(max: &str, stat: &str) -> Option<(u64, u64)> {
    let max = max.trim().parse().ok()?;
    let field = |name: &str| {
        let value = stat.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(' '));
        value.and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(0)
    };
    Some((max, field("anon") + field("shmem")))
}

/// cladefs: the memory process `pid` holds now (macOS's resident size, [`usage`]), 0 if it says
/// none, as Linux's.
#[cfg(target_os = "macos")]
pub fn resident(pid: u32) -> u64 {
    usage(pid).map_or(0, |usage| usage.ri_resident_size)
}

/// cladefs: the memory process `pid` holds now (/proc/PID/status's `VmRSS`), 0 if it says none.
#[cfg(target_os = "linux")]
pub fn resident(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let kib = status.lines().find_map(|line| line.strip_prefix("VmRSS:"));
    kib.and_then(|kib| kib.trim().trim_end_matches(" kB").trim().parse::<u64>().ok()).unwrap_or(0)
        << 10
}

/// cladefs: a case's measurements, in the fs bench's shape (test_fs_bench.rs), which
/// scripts/bench-ab.py compares two sides' rounds of (TASK-330.4): each a workload of one value
/// a run, its unit and whether higher is better; and the checks the case failed, how many of each
/// kind and what each said. [`Measured::save`] writes them to CLADEFS_BENCH_RESULTS, if set, as
/// `bench-CASE.json`.
#[derive(serde::Serialize)]
pub struct Measured {
    case: String,
    backend: String,
    results: Vec<Measure>,
    failures: std::collections::BTreeMap<String, u32>,
    failed: Vec<String>,
}

#[derive(serde::Serialize)]
struct Measure {
    workload: String,
    unit: &'static str,
    higher_is_better: bool,
    values: Vec<f64>,
}

impl Measured {
    pub fn new(case: &str) -> Self {
        Self {
            case: case.to_owned(),
            backend: case.to_owned(),
            results: Vec::new(),
            failures: Default::default(),
            failed: Vec::new(),
        }
    }

    /// Records `workload`'s `value`, in `unit`, lower better.
    pub fn lower(&mut self, workload: impl Into<String>, unit: &'static str, value: f64) {
        self.record(workload.into(), unit, false, value);
    }

    /// Records `workload`'s `value`, in `unit`, higher better.
    pub fn higher(&mut self, workload: impl Into<String>, unit: &'static str, value: f64) {
        self.record(workload.into(), unit, true, value);
    }

    fn record(&mut self, workload: String, unit: &'static str, higher_is_better: bool, value: f64) {
        self.results.push(Measure { workload, unit, higher_is_better, values: vec![value] });
    }

    /// Records a check failed, of `kind`, saying `said`.
    pub fn failed(&mut self, kind: &str, said: impl Into<String>) {
        *self.failures.entry(kind.to_owned()).or_default() += 1;
        self.failed.push(said.into());
    }

    /// Whether a check failed.
    pub fn has_failed(&self) -> bool {
        !self.failed.is_empty()
    }

    /// Records a process's costs ([`ProcessCosts`]), each a workload named `name.` and the cost.
    pub fn costs(&mut self, name: &str, costs: ProcessCosts) {
        let (gib, mib) = ((1u64 << 30) as f64, (1u64 << 20) as f64);
        self.lower(format!("{name}.disk_written"), "GiB", costs.disk_written as f64 / gib);
        self.lower(format!("{name}.peak_rss"), "MiB", costs.peak_rss as f64 / mib);
        self.lower(format!("{name}.cpu"), "s", costs.cpu_secs);
    }

    /// Writes them to CLADEFS_BENCH_RESULTS, if set.
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(dir) = std::env::var_os("CLADEFS_BENCH_RESULTS") else { return Ok(()) };
        let path = Path::new(&dir).join(format!("bench-{}.json", self.case));
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
    }
}

/// cladefs: fails unless the daemon named `name` the test spawned made half its cells' commits
/// durable within cladefsd's turn, 50 ms, at least, as its stats file says: doc-12's median
/// (`local_lag_within_50ms`, TASK-221).
pub fn check_durable_median(test_setup: &TestSetup, name: &str) -> anyhow::Result<()> {
    let path = test_setup.tmp_dir.join(format!("{name}-stats.txt"));
    let kept = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let daemon = kept.lines().find_map(|line| line.strip_prefix("daemon: "));
    let daemon = daemon.context("no daemon line")?;
    let field = |name: &str| -> anyhow::Result<u64> {
        let value = daemon.split(' ').find_map(|field| field.strip_prefix(name)?.strip_prefix('='));
        Ok(value.with_context(|| format!("no {name}"))?.parse()?)
    };
    let (count, within) = (field("local_lag_count")?, field("local_lag_within_50ms")?);
    anyhow::ensure!(
        2 * within >= count,
        "{within} of {count} commits durable within 50 ms: doc-12's median is 50 ms"
    );
    Ok(())
}

/// cladefs: the most bytes a DAX window has, the extent daemon's (`cladefsd::wire::MAX_WINDOW`):
/// what it keeps for a window grows with the pages it maps, which it bounds so (TASK-150.3).
pub const MAX_DAX_WINDOW: u64 = 64 << 30;

/// cladefs: a case's DAX window: as large as the data, the host's memory, or
/// `CLADEFS_DAX_WINDOW_MIB`'s if set ([`Window::Large`]); or a small window's, whose cases check it
/// fills ([`Window::Small`]), as fxfs-guest-checks-dax-small's, which a large one fails (TASK-227).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    Large,
    Small(u64),
}

/// cladefs: the bytes of a case's DAX window `window` asks for ([`Window`]): the window is address
/// space, not memory, the daemon's pages it maps one copy whatever the windows' sizes, as
/// virtio-pmem maps a whole image; its guest makes struct pages for what it reaches alone
/// (TASK-92.8), and its VM process keeps changes for those pages alone (TASK-227).
pub fn dax_window(window: Window) -> anyhow::Result<u64> {
    let given = std::env::var("CLADEFS_DAX_WINDOW_MIB").ok();
    window_bytes(window, given.as_deref(), host_memory()?)
}

/// cladefs: [`dax_window`]'s bytes, of `window`, `given` CLADEFS_DAX_WINDOW_MIB's, and the host's
/// memory, `memory` bytes: a large window's `given`'s, or the host's memory, rounded down to a
/// window's 2 MiB, at most [`MAX_DAX_WINDOW`].
fn window_bytes(window: Window, given: Option<&str>, memory: u64) -> anyhow::Result<u64> {
    const ALIGNMENT: u64 = 2 << 20;
    match window {
        Window::Small(bytes) => Ok(bytes),
        Window::Large if let Some(mib) = given.filter(|mib| !mib.is_empty()) => {
            Ok(mib.parse::<u64>().context("CLADEFS_DAX_WINDOW_MIB")? << 20)
        }
        Window::Large => Ok((memory / ALIGNMENT * ALIGNMENT).min(MAX_DAX_WINDOW)),
    }
}

/// cladefs: the bytes of the host's memory: /proc/meminfo's MemTotal on Linux, `hw.memsize` on
/// macOS.
pub fn host_memory() -> anyhow::Result<u64> {
    #[cfg(target_os = "linux")]
    return mem_total(&fs::read_to_string("/proc/meminfo")?);
    #[cfg(target_os = "macos")]
    {
        let mut memory: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: Reads a u64-sized sysctl into `memory`, of `len` bytes.
        let read = unsafe {
            nix::libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut memory).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        anyhow::ensure!(read == 0, "hw.memsize: {}", std::io::Error::last_os_error());
        Ok(memory)
    }
}

/// cladefs: the bytes of memory /proc/meminfo's text, `meminfo`, says the host has, its MemTotal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux's, and its test's"))]
fn mem_total(meminfo: &str) -> anyhow::Result<u64> {
    let line = (meminfo.lines().find(|line| line.starts_with("MemTotal:")))
        .context("/proc/meminfo has no MemTotal")?;
    let kib: u64 = (line.split_whitespace().nth(1)).context("MemTotal has no value")?.parse()?;
    Ok(kib << 10)
}

/// An extent daemon of the test's own, CLADEFSD_BIN, serving at `socket`, and its control API
/// beside it ([`control`]), once this returns, with `env` in its environment, and `memory` bytes of
/// memory if given; otherwise, as production's, its memory a page cache of what of the host's is
/// free (TASK-358). It logs to `<name>.log` in the test's directory.
/// cladefs: it keeps what its cells read from their bucket in a part cache of its own,
/// `<name>-cache` in the test's directory, as production's daemon keeps it on the host's disk
/// (`CLADEFSD_PART_CACHE`, doc-7), unless `env` or this process says otherwise, empty for none: one
/// daemon at a time has a cache's directory (TASK-164.12). And it keeps what it did in
/// `<name>-stats.txt` beside its log, unless `env` names a file (`CLADEFSD_STATS`): what it
/// downloaded against what it read, which the run's summary says (scripts/guest_results.py,
/// TASK-165.12).
/// Default explicit memory budget (1 GiB) passed to test daemons launched via [`spawn_daemon`]
/// when the host does not support Linux governed mode (e.g. hotbed VMs with MGLRU disabled,
/// board 13373 / board 13393). Production `cladefsd` still fails hard in governed mode; tests
/// exercising governed mode itself use [`spawn_governed_daemon`].
const DEFAULT_TEST_DAEMON_MEMORY: u64 = 1 << 30;

/// Spawns `cladefsd` in governed mode (no fixed memory budget argument) so tests that specifically
/// exercise the memory governor fail hard if the host lacks governor support (board 13373).
pub fn spawn_governed_daemon(
    test_setup: &TestSetup,
    socket: &Path,
    name: &str,
    env: &[(&str, &std::ffi::OsStr)],
) -> anyhow::Result<std::process::Child> {
    spawn_daemon_inner(test_setup, socket, name, env, None, true)
}

/// Spawns `cladefsd` for a test case. When `memory` is `None`, honors `CLADEFSD_MEMORY` if set;
/// otherwise runs governed when the host supports it, or falls back to
/// [`DEFAULT_TEST_DAEMON_MEMORY`] on hosts without MGLRU/governor support (board 13373).
pub fn spawn_daemon(
    test_setup: &TestSetup,
    socket: &Path,
    name: &str,
    env: &[(&str, &std::ffi::OsStr)],
    memory: Option<u64>,
) -> anyhow::Result<std::process::Child> {
    spawn_daemon_inner(test_setup, socket, name, env, memory, false)
}

fn spawn_daemon_inner(
    test_setup: &TestSetup,
    socket: &Path,
    name: &str,
    env: &[(&str, &std::ffi::OsStr)],
    memory: Option<u64>,
    require_governed: bool,
) -> anyhow::Result<std::process::Child> {
    use std::time::{Duration, Instant};

    let bin = std::env::var_os("CLADEFSD_BIN").context("CLADEFSD_BIN")?;
    let log = fs::File::create(test_setup.tmp_dir.join(format!("{name}.log")))?;
    // A host keeps cells in a bucket, `env`'s or this process's, or a file bucket of its own in
    // the test's directory, as of placement 1, unless one is named. Not one in its memory: the
    // cases open the cells it made in processes of their own, after it, which keep a cell's
    // cell in the bucket its description names, and opening one whose cell the bucket lacks,
    // its commits acknowledged, is refused (TASK-165.9), as scripts/test-e2e.sh's daemon's.
    let named = |var: &str| {
        env.iter().any(|(given, value)| *given == var && !value.is_empty())
            || std::env::var_os(var).is_some_and(|value| !value.is_empty())
    };
    let mut command = std::process::Command::new(bin);
    command.arg(socket).env("CLADEFSD_GRPC", socket.with_extension("control"));
    if let Some(memory) = memory {
        command.arg(memory.to_string());
    } else if !require_governed {
        let env_memory = env
            .iter()
            .find(|(var, _)| *var == "CLADEFSD_MEMORY")
            .map(|(_, value)| value.to_os_string())
            .or_else(|| std::env::var_os("CLADEFSD_MEMORY"))
            .filter(|value| !value.is_empty());
        if let Some(memory) = env_memory {
            command.arg(memory);
        } else if !cladefs::cladefsd::governor::host_supported() {
            command.arg(DEFAULT_TEST_DAEMON_MEMORY.to_string());
        }
    }
    if !named("CLADEFSD_BUCKET") {
        let bucket = test_setup.tmp_dir.join(format!("{name}.bucket"));
        command.env("CLADEFSD_BUCKET", format!("file://{}/", bucket.display()));
    }
    if !named("CLADEFSD_TOKEN") {
        command.env("CLADEFSD_TOKEN", "1");
    }
    let given = env.iter().find(|(var, _)| *var == "CLADEFSD_PART_CACHE").map(|(_, value)| *value);
    let ours = std::env::var_os("CLADEFSD_PART_CACHE");
    if let Some(cache) = part_cache(given, ours.as_deref(), &test_setup.tmp_dir, name) {
        fs::create_dir_all(&cache)?;
        command.env("CLADEFSD_PART_CACHE", cache);
    }
    let given = env.iter().any(|(var, _)| *var == "CLADEFSD_STATS");
    if let Some(stats) = stats_file(given, &test_setup.tmp_dir, name) {
        command.env("CLADEFSD_STATS", stats);
    }
    if !env.iter().any(|(var, _)| *var == "CLADEFSD_ROOTS") {
        let disk_root = disk_root_for(&test_setup.tmp_dir, name);
        fs::create_dir_all(&disk_root)?;
        command.env("CLADEFSD_ROOTS", &disk_root);
    }
    let daemon = command.envs(env.iter().copied()).stderr(log).spawn()?;
    test_setup.register_cleanup_pid(daemon.id());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match control(socket) {
            Ok(_) => {
                give_to_the_jail(socket)?;
                return Ok(daemon);
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => return Err(anyhow::anyhow!("the daemon {name}: {err:#}")),
        }
    }
}

/// cladefs: the part cache a test's daemon `name` is given, in `dir`, the test's directory, as
/// production's daemon keeps one on the host's disk (`CLADEFSD_PART_CACHE`, doc-7), unless
/// `CLADEFSD_PART_CACHE` is said, `given` the daemon or `ours` this process, which it then takes as
/// it is, empty for none. Without one, a fork paged in what it inherited from the bucket a ranged
/// GET at a time: forked-dax 0.304 in the perf gate, 0.849 with it (TASK-164.12, GCE r790, r792).
fn part_cache(
    given: Option<&std::ffi::OsStr>,
    ours: Option<&std::ffi::OsStr>,
    dir: &Path,
    name: &str,
) -> Option<PathBuf> {
    (given.is_none() && ours.is_none()).then(|| dir.join(format!("{name}-cache")))
}

/// cladefs: the stats file a test's daemon `name` keeps what it did in, `<name>-stats.txt` in
/// `dir`, the test's directory, beside its log, unless `env` names one (`given`): what the run's
/// summary reads of each daemon, its downloads against its reads (scripts/guest_results.py,
/// TASK-165.12). Not this process's, which scripts/test-e2e.sh's own daemon alone has, its file the
/// run's.
fn stats_file(given: bool, dir: &Path, name: &str) -> Option<PathBuf> {
    (!given).then(|| dir.join(format!("{name}-stats.txt")))
}

/// Gives the extent daemon's socket at `socket`, its user's alone (TASK-155), to the user of the
/// jails Firecracker runs in, if it does: scripts/test-e2e.sh's CLADEFS_FIRECRACKER_JAIL. Each
/// jailed Firecracker here uses the default tenant's socket, as production's use a tenant's.
pub fn give_to_the_jail(socket: &Path) -> anyhow::Result<()> {
    let id = |name: &str| -> anyhow::Result<Option<u32>> {
        let Ok(value) = std::env::var(name) else { return Ok(None) };
        Ok(Some(value.parse().with_context(|| format!("{name} is not a number"))?))
    };
    if let (Some(uid), Some(gid)) = (id("CLADEFS_JAIL_UID")?, id("CLADEFS_JAIL_GID")?) {
        std::os::unix::fs::chown(socket, Some(uid), Some(gid))
            .with_context(|| format!("giving {} to the jail's user", socket.display()))?;
    }
    Ok(())
}

/// A client of the control API of the extent daemon whose VM socket is at `socket`: at its control
/// socket, beside it, `<socket>.control` for `<socket>.sock`, as cladefs's scripts/test-e2e.sh and
/// [`spawn_daemon`] place it.
pub fn control(socket: &Path) -> Result<cladefs::testing::Control, cladefs::Error> {
    cladefs::testing::Control::connect(socket.with_extension("control"))
}

/// The managed disk root [`spawn_daemon`] creates for daemon `name` under `dir`.
pub fn disk_root_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}-disk0"))
}

/// The first managed disk root configured for the run's shared extent daemon (`CLADEFSD_ROOTS`).
pub fn shared_disk_root() -> anyhow::Result<PathBuf> {
    let raw = std::env::var_os("CLADEFSD_ROOTS").context("CLADEFSD_ROOTS not set")?;
    std::env::split_paths(&raw)
        .find(|p| !p.as_os_str().is_empty())
        .context("CLADEFSD_ROOTS is empty")
}

/// The canonical on-disk directory a managed disk rooted at `disk_root` holds for `cell`.
pub fn cell_disk_path(disk_root: &Path, cell: cladefs::CellId) -> PathBuf {
    disk_root
        .join("cells")
        .join(base32_id::Padded(cell.tenant).to_string())
        .join(base32_id::Padded(cell.id).to_string())
}

/// Creates a new Fxfs cell of `capacity` bytes on the daemon at `cladefsd` (default tenant 0) and
/// returns its assigned [`cladefs::CellId`].
pub fn create_cell(cladefsd: &Path, capacity: u64) -> anyhow::Result<cladefs::CellId> {
    use cladefs::cladefsd::control::proto::{CellKind, CreateCellRequest};
    let response = control(cladefsd)?.call(async |client| {
        client
            .create_cell(CreateCellRequest {
                capacity,
                kind: CellKind::Fxfs.into(),
                ..Default::default()
            })
            .await
    })?;
    Ok(cladefs::CellId { tenant: response.tenant, id: response.cell })
}

/// Creates a new Fxfs cell of `capacity` bytes on the shared `CLADEFSD` daemon, saves its cell ID
/// under `dir/<name>.cell`, and returns `(cell_id, canonical_on_disk_path)`.
pub fn create_shared_cell(
    dir: &Path,
    name: &str,
    capacity: u64,
) -> anyhow::Result<(cladefs::CellId, PathBuf)> {
    let cladefsd = PathBuf::from(std::env::var_os("CLADEFSD").context("CLADEFSD not set")?);
    let cell = create_cell(&cladefsd, capacity)?;
    save_cell_id(dir, name, cell)?;
    let path = cell_disk_path(&shared_disk_root()?, cell);
    Ok((cell, path))
}

/// Persists `cell` under `dir/<name>.cell` so `Test::check` can locate the cell on disk.
pub fn save_cell_id(dir: &Path, name: &str, cell: cladefs::CellId) -> anyhow::Result<()> {
    fs::write(dir.join(format!("{name}.cell")), cell.to_string())
        .with_context(|| format!("writing {name}.cell"))
}

/// Reads a cell ID previously persisted by [`save_cell_id`].
pub fn load_cell_id(dir: &Path, name: &str) -> anyhow::Result<cladefs::CellId> {
    let path = dir.join(format!("{name}.cell"));
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    text.trim().parse().map_err(|err: String| anyhow::anyhow!("{}: {err}", path.display()))
}

/// Locates the canonical on-disk path for a cell previously persisted via [`save_cell_id`] on the
/// run's shared `CLADEFSD` daemon.
pub fn shared_cell_path(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    Ok(cell_disk_path(&shared_disk_root()?, load_cell_id(dir, name)?))
}

/// Copies the extent store at `from` to `to`, as [`copy_dir`] does, once no process has it open:
/// its directory taken, as a process opening it takes it, which the extent daemon serving it holds
/// until its last user, an upload too, lets it go. Copied while the daemon held it, an upload's
/// marker's `.new` file was renamed away between listing it and copying it (TASK-129).
pub fn copy_cell_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    use local_cell::Dir;
    let dir =
        local_cell::HostDir::new(from).with_context(|| format!("opening {}", from.display()))?;
    dir.take().with_context(|| format!("taking {}", from.display()))?;
    let copied = copy_dir(from, to);
    dir.let_go();
    copied
}

/// Copies the directory `from` to `to`, which must not exist, with everything in it.
pub fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    create_dir(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// TASK-330.4: a cgroup's limit and what reclaim can't take back of what it holds, its anon
    /// and shmem; none without a limit.
    #[test]
    fn a_cgroups_unreclaimable_memory_reads_from_its_files() {
        let stat = "anon 1000\nfile 9000\nkernel 5\nshmem 200\nfile_dirty 7\n";
        assert_eq!(unreclaimable("17179869184\n", stat), Some((17179869184, 1200)));
        assert_eq!(unreclaimable("max\n", stat), None);
    }

    /// TASK-330.4: a process's costs are its kernel's counts, its command's spaces and all.
    #[test]
    fn a_processs_costs_read_from_its_proc_files() {
        let io = "rchar: 10\nwchar: 20\nsyscr: 1\nsyscw: 2\nread_bytes: 4096\nwrite_bytes: 8192\n\
                  cancelled_write_bytes: 0\n";
        let status = "Name:\tcladefsd\nVmPeak:\t  900 kB\nVmHWM:\t    512 kB\nVmRSS:\t 400 kB\n";
        let stat = "42 (cladefsd) S 1 42 42 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9 0";
        let costs = ProcessCosts::parse(io, status, stat, 100.0).unwrap();
        assert_eq!(costs, ProcessCosts { disk_written: 8192, peak_rss: 512 << 10, cpu_secs: 3.0 });
        assert_eq!(
            format!("{:#}", ProcessCosts::parse("rchar: 1\n", status, stat, 100.0).unwrap_err()),
            "no write_bytes"
        );
    }

    /// TASK-164.12: a test's daemon keeps a part cache of its own in the test's directory, as
    /// production's does, unless CLADEFSD_PART_CACHE is said, by the daemon's environment or this
    /// process's, which it takes as it is, empty for none.
    #[test]
    fn daemons_keep_a_part_cache_unless_told_otherwise() {
        let dir = Path::new("/tmp/case");
        let cache = |given: Option<&str>, ours: Option<&str>| {
            part_cache(given.map(OsStr::new), ours.map(OsStr::new), dir, "cladefsd")
        };
        assert_eq!(cache(None, None), Some(PathBuf::from("/tmp/case/cladefsd-cache")));
        assert_eq!(cache(Some(""), None), None);
        assert_eq!(cache(Some("/nvme/cache"), None), None);
        assert_eq!(cache(None, Some("")), None);
        assert_eq!(cache(None, Some("/nvme/cache")), None);
    }

    /// TASK-227: a case's DAX window is the host's memory, rounded down to 2 MiB, at most a
    /// window's 64 GiB, or CLADEFS_DAX_WINDOW_MIB's, unless it is a small window's, which stays so:
    /// fxfs-guest-checks-dax-small's window_filled failed in 64 GiB (GCE r886).
    #[test]
    fn windows_are_as_large_as_the_hosts_memory() {
        let host = |kib: u64| mem_total(&format!("MemTotal:       {kib} kB\nMemFree:  1 kB\n"));
        let (gib, mib) = (1 << 30, 1 << 20);
        let large = |kib| window_bytes(Window::Large, None, host(kib).unwrap()).unwrap();
        assert_eq!(large(32_853_144), 31 * gib + 338 * mib);
        assert_eq!(large(256 << 20), MAX_DAX_WINDOW);
        let small = Window::Small(64 * mib);
        assert_eq!(window_bytes(small, None, 32 * gib).unwrap(), 64 * mib);
        assert_eq!(window_bytes(small, Some("8192"), 32 * gib).unwrap(), 64 * mib);
        assert_eq!(window_bytes(Window::Large, Some("1024"), 32 * gib).unwrap(), gib);
        assert_eq!(window_bytes(Window::Large, Some(""), 4 * gib).unwrap(), 4 * gib);
        assert!(mem_total("MemFree: 1 kB\n").is_err());
    }

    /// TASK-165.12: a test's daemon keeps its stats beside its log, where the run's summary reads
    /// them, unless its environment names a file, as pgbench's daemons' does.
    #[test]
    fn daemons_keep_their_stats_beside_their_logs() {
        let dir = Path::new("/tmp/case");
        assert_eq!(
            stats_file(false, dir, "cladefsd"),
            Some(PathBuf::from("/tmp/case/cladefsd-stats.txt"))
        );
        assert_eq!(stats_file(true, dir, "cladefsd"), None);
    }
}
