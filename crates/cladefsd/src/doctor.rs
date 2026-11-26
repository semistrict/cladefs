// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's doctor (board 14217): the host's limits and kernel features, every one
//! checked in one place, at every boot (`boot_daemon`, `boot_embedded`) and by `cladefsd
//! --doctor`, which starts no daemon.
//!
//! Each check is a warning or a requirement. A warning is something the daemon runs with, worse:
//! too few open files cap the cells it serves; too few mappings cap the windows it gives VMs; no
//! memory pressure stall information leaves its governor guessing. A requirement is something
//! running without would be wrong: a kernel that can't say which process a VM process's socket
//! is would have the daemon kill another (board 8533); pages it makes no slabs of; in governed
//! mode, the memory governor's prerequisites (`governor::governed_checks`). Every check runs and
//! says what it found, passing or not, before any refusal, so that one start says all a host
//! lacks, not the first thing.
//!
//! What a check needs of the daemon's configuration, whether its memory is governed, the boot
//! says ([`Needs`]). What only one cell's opening finds (a cell directory's filesystem, its part
//! cache's) stays with that opening.

use std::fmt;

/// What the boot needs of the host, beside what every daemon does.
#[derive(Clone, Copy, Debug)]
pub struct Needs {
    /// Whether its memory is governed, no size given: the governor's prerequisites required.
    pub governed: bool,
}

/// How much a check failing matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The daemon runs, worse.
    Warning,
    /// The daemon refuses to start.
    Required,
}

/// A check's result: what it found, or what is wrong.
#[derive(Debug)]
pub struct Finding {
    pub check: &'static str,
    pub severity: Severity,
    pub found: Result<String, String>,
}

/// Every check's finding, in the order they ran.
#[derive(Debug)]
pub struct Report {
    pub findings: Vec<Finding>,
}

/// Checks the host for a daemon that needs `needs`.
pub fn examine(needs: Needs) -> Report {
    let mut findings = Vec::new();
    let mut check = |check, severity, found| findings.push(Finding { check, severity, found });
    check("page size", Severity::Required, page_size());
    check("open files", Severity::Warning, open_files());
    check("SO_PEERPIDFD", Severity::Required, peer_pidfds());
    check("vm.max_map_count", Severity::Warning, max_map_count());
    // Test binaries built with `tests-kms` (forbidden in release builds by `lib.rs`) run
    // unprivileged integration tests without host swap or CAP_DAC_OVERRIDE; production binaries
    // always require them.
    #[cfg(all(target_os = "linux", not(madsim), not(feature = "tests-kms")))]
    if needs.governed {
        match governed_floor() {
            Ok(floor) => {
                for (name, found) in crate::governor::governed_checks(floor) {
                    check(name, Severity::Required, found);
                }
            }
            Err(err) => check("memory governor's floor", Severity::Required, Err(err)),
        }
        check("memory pressure (PSI)", Severity::Warning, psi());
    }
    #[cfg(not(all(target_os = "linux", not(madsim), not(feature = "tests-kms"))))]
    let _ = needs;
    Report { findings }
}

impl Report {
    /// Logs each finding: those passing as information, a warning's problem as a warning, a
    /// requirement's as an error.
    pub fn log(&self) {
        for Finding { check, severity, found } in &self.findings {
            match (found, severity) {
                (Ok(found), _) => log::info!("cladefsd: doctor: {check}: {found}"),
                (Err(wrong), Severity::Warning) => log::warn!("cladefsd: doctor: {check}: {wrong}"),
                (Err(wrong), Severity::Required) => {
                    log::error!("cladefsd: doctor: {check}: {wrong}")
                }
            }
        }
    }

    /// What of the host the daemon refuses to start without, if anything: each requirement
    /// failing, named.
    pub fn refusal(&self) -> Option<String> {
        let failed: Vec<String> = (self.findings.iter())
            .filter(|finding| finding.severity == Severity::Required)
            .filter_map(|Finding { check, found, .. }| {
                found.as_ref().err().map(|wrong| format!("{check}: {wrong}"))
            })
            .collect();
        (!failed.is_empty()).then(|| failed.join("; "))
    }
}

/// One line a finding: `ok`, `warning` or `refused`, the check, and what it found.
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for Finding { check, severity, found } in &self.findings {
            let (mark, said) = match (found, severity) {
                (Ok(found), _) => ("ok", found),
                (Err(wrong), Severity::Warning) => ("warning", wrong),
                (Err(wrong), Severity::Required) => ("refused", wrong),
            };
            writeln!(f, "{mark:<8} {check}: {said}")?;
        }
        Ok(())
    }
}

/// Checks the host at a boot needing `needs`: every finding logged, and the boot refused if a
/// requirement failed.
pub(super) fn at_boot(needs: Needs) -> anyhow::Result<()> {
    let report = examine(needs);
    report.log();
    match report.refusal() {
        Some(refused) => anyhow::bail!("the host lacks what the extent daemon needs: {refused}"),
        None => Ok(()),
    }
}

/// The host's page size, which slabs must be a whole number of (`slabs::pages_fit`): anything
/// asking it panics otherwise.
fn page_size() -> Result<String, String> {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let size = u64::try_from(size).map_err(|_| format!("sysconf(_SC_PAGESIZE) said {size}"))?;
    match cladefs_core::slabs::pages_fit(size) {
        true => Ok(format!("{size} bytes")),
        false => Err(format!("pages of {size} bytes: slabs need pages of 4 KiB to 1 MiB")),
    }
}

/// The soft limit of open files, which the daemon raised to the hard one first
/// (`raise_open_files_limit`): a warning under [`crate::open_files_floor`], its own files and its
/// floor of cells' at their most, as it runs then, refusing connections past what the limit
/// leaves (`server::most_connections`). It refused to start under it before (board 8595); the
/// user, 2026-11-26: a warning, production's limit being far higher (`LimitNOFILE=1048576`).
fn open_files() -> Result<String, String> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: A plain system call writing the process's limit into `limit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(format!("reading RLIMIT_NOFILE: {}", std::io::Error::last_os_error()));
    }
    let (soft, floor) = (limit.rlim_cur, crate::open_files_floor());
    let cells = soft.saturating_sub(crate::OWN_OPEN_FILES) / crate::cell_open_files();
    match soft >= floor {
        true => Ok(format!("{soft} (room for {cells} cells' files at their most)")),
        false => Err(format!(
            "RLIMIT_NOFILE is {soft}, under the {floor} for {} cells' files at their most: the \
             daemon serves room for {cells}; raise its hard limit (LimitNOFILE= of its systemd \
             unit, or ulimit -Hn; on macOS, kern.maxfilesperproc too)",
            crate::FLOOR_CELLS
        )),
    }
}

/// Whether the kernel names a socket's peer process by a pidfd (`SO_PEERPIDFD`, Linux 6.5), which
/// the daemon knows a VM process by, to kill it and no other (board 8533).
fn peer_pidfds() -> Result<String, String> {
    match crate::server::require_peer_pidfds() {
        Ok(()) if cfg!(all(target_os = "linux", not(madsim))) => Ok("supported".into()),
        Ok(()) => Ok("not needed here".into()),
        Err(err) => Err(err.to_string()),
    }
}

/// The most mappings a process may make (`vm.max_map_count`), which VM processes' windows budget
/// their runs of pages by (`dax::uffd::budget`): a warning at Linux's default, which caps them,
/// none where the host limits none.
fn max_map_count() -> Result<String, String> {
    /// What a host of many VMs' windows wants at least.
    const WANTED: u64 = 262_144;
    match cladefs_core::pages::max_map_count() {
        Ok(None) => Ok("no limit here".into()),
        Ok(Some(count)) if count >= WANTED => Ok(count.to_string()),
        Ok(Some(count)) => Err(format!(
            "{count}, under the {WANTED} a host of many VMs' windows wants: their windows map \
             fewer runs of pages at once (sysctl vm.max_map_count=1048576)"
        )),
        Err(err) => Err(format!("reading /proc/sys/vm/max_map_count: {err}")),
    }
}

/// The memory governor's floor on this host (`governor::floor`).
#[cfg(all(target_os = "linux", not(madsim), not(feature = "tests-kms")))]
fn governed_floor() -> Result<u64, String> {
    let host =
        crate::governor::host_memory().map_err(|err| format!("reading /proc/meminfo: {err}"))?;
    crate::governor::floor(crate::governor::limit(host.total))
}

/// Whether the kernel says how long memory pressure stalled tasks (PSI, `CONFIG_PSI`), which the
/// governor goes by, or guesses from watermarks without, as VM reclaim passes VMs over.
#[cfg(all(target_os = "linux", not(madsim), not(feature = "tests-kms")))]
fn psi() -> Result<String, String> {
    match std::fs::read_to_string("/proc/pressure/memory") {
        Ok(_) => Ok("/proc/pressure/memory".into()),
        Err(err) => Err(format!(
            "reading /proc/pressure/memory: {err}: the governor goes by watermarks alone, and \
             reclaims no VM's memory (a kernel with CONFIG_PSI, not psi=0)"
        )),
    }
}
