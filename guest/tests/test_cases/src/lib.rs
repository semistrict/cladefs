// Copyright The libkrun Authors.
// Copyright 2026 The cladefs Authors: changed for cladefs's guest tests.
// SPDX-License-Identifier: Apache-2.0

//! cladefs's guest tests: VMs booted with cladefs's Firecracker and guest kernel, each a test case
//! run by the runner (`runner test --test-case NAME`), its host half here under the `host` feature
//! and its guest half, in the guest agent, under `guest`. Moved from libkrun's tests (TASK-40),
//! whose framework this is.

/// A POSIX ACL xattr's value: its version, then each entry's tag, permissions and id.
pub fn posix_acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut value = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        value.extend(tag.to_le_bytes());
        value.extend(perm.to_le_bytes());
        value.extend(id.to_le_bytes());
    }
    value
}

// cladefs: the known plaintext cases write, and the check that a run's cells keep none of it in the
// clear (TASK-411.15).
pub mod canary;
#[cfg(feature = "host")]
pub mod encrypted;

mod test_pjdfstest;
use test_pjdfstest::TestPjdfstest;

mod test_xfstests;
use test_xfstests::TestXfstests;

mod test_fxfs_guest;
use test_fxfs_guest::TestFxfsGuest;
mod test_fxfs_guest_resize;
use test_fxfs_guest_resize::TestFxfsGuestResize;
mod test_fxfs_guest_pressure;
use test_fxfs_guest_pressure::TestFxfsGuestPressure;
mod test_fxfs_guest_stats;
use test_fxfs_guest_stats::TestFxfsGuestStats;
mod test_fxfs_guest_live_fork;
use test_fxfs_guest_live_fork::TestFxfsGuestLiveFork;
mod test_fxfs_guest_pgbench;
use test_fxfs_guest_pgbench::{
    Database as PgbenchDatabase, Probe, Storage as PgbenchStorage, TestFxfsGuestPgbench,
};
mod test_fxfs_guest_bigmap;
mod test_fxfs_guest_ephemeral_suspend;
use test_fxfs_guest_ephemeral_suspend::TestFxfsGuestEphemeralSuspend;
mod test_fxfs_guest_memory_cell;
use test_fxfs_guest_memory_cell::TestFxfsGuestMemoryCell;
mod test_fxfs_guest_park_resume;
use test_fxfs_guest_park_resume::TestFxfsGuestParkResume;
mod test_fxfs_guest_live_migration;
use test_fxfs_guest_live_migration::TestFxfsGuestLiveMigration;
// cladefs: what cladefsd asks of a disk it shares with many VMs and the bucket (TASK-310).
mod test_fxfs_guest_shared_disk;
use test_fxfs_guest_shared_disk::TestFxfsGuestSharedDisk;
mod test_fxfs_guest_bucket_soak;
use test_fxfs_guest_bucket_soak::TestFxfsGuestBucketSoak;
// cladefs: sandboxes as LangSmith's CI runs them, two hosts and a bucket on one disk (TASK-330.1).
mod test_fxfs_guest_sandboxes;
// cladefs: a data-analysis sandbox timed from its fork to its answers, three hosts, one bucket
// (TASK-665).
mod test_fxfs_guest_python_sandbox;
use test_fxfs_guest_bigmap::TestFxfsGuestBigmap;
use test_fxfs_guest_python_sandbox::TestFxfsGuestPythonSandbox;
use test_fxfs_guest_sandboxes::TestFxfsGuestSandboxes;
// cladefs: what a VM costs its host's memory, measured when its guest asks (TASK-164.5).
mod measure;
// cladefs: how many VMs on forks of a base a host holds (board 5583).
mod test_cells_per_host;
use test_cells_per_host::TestCellsPerHost;
mod test_fxfs_guest_daemon_lost;
use test_fxfs_guest_daemon_lost::TestFxfsGuestDaemonLost;
mod test_fxfs_guest_import;
use test_fxfs_guest_import::TestFxfsGuestImport;
// cladefs: the guest Fxfs's tracepoints under bpftrace (TASK-44.16).
mod test_fxfs_guest_bpftrace;
use test_fxfs_guest_bpftrace::TestFxfsGuestBpftrace;
// cladefs: what a checkout the size of Linux's costs (TASK-136.1).
mod test_linux_checkout;
use test_linux_checkout::{Checkout, TestLinuxCheckout};
mod test_fxfs_guest_volumes;
use test_fxfs_guest_volumes::TestFxfsGuestVolumes;
mod test_fxfs_guest_docker;
use test_fxfs_guest_docker::TestFxfsGuestDocker;
mod test_fxfs_guest_snapshot;
use test_fxfs_guest_snapshot::{
    Memory as SnapshotMemory, Mode as SnapshotMode, ReadOnly as SnapshotReadOnly,
    TestFxfsGuestSnapshot, VOLUMES,
};
mod test_extent_guest;
use test_extent_guest::{
    TestExtentGuest, TestExtentGuestBadConfig, TestExtentGuestOtherVersion,
    TestExtentGuestSelftestMounted,
};
mod test_runner_deadlock;
use test_runner_deadlock::TestRunnerDeadlock;

#[cfg(any(feature = "host", target_os = "linux"))]
mod test_fxfs_guest_checks;
#[cfg(any(feature = "host", target_os = "linux"))]
use test_fxfs_guest_checks::{PAGE_CACHE_CHECKS, TestFxfsGuestChecks};
#[cfg(any(feature = "host", target_os = "linux"))]
mod test_fxfs_guest_script;
#[cfg(any(feature = "host", target_os = "linux"))]
use test_fxfs_guest_script::TestFxfsGuestScript;
#[cfg(any(feature = "host", target_os = "linux"))]
mod test_fxfs_guest_fsx;
#[cfg(any(feature = "host", target_os = "linux"))]
use test_fxfs_guest_fsx::TestFxfsGuestFsx;
// cladefs: hours of mixed load through every crash doc-12 names (TASK-67), its plans and verdicts
// in `soak`.
pub mod soak;
#[cfg(any(feature = "host", target_os = "linux"))]
mod test_fxfs_guest_soak;
#[cfg(any(feature = "host", target_os = "linux"))]
use test_fxfs_guest_soak::TestFxfsGuestSoak;

// The scripts of operations of fxfs-guest-checks' random_ops and fxfs-guest-script: what they are,
// the guest's running of them, and the host's replays of them in VMs.
pub mod script;
#[cfg(all(feature = "guest", target_os = "linux"))]
mod script_run;
// cladefs: a script on two filesystems at once, step by step (TASK-44.15).
#[cfg(all(feature = "guest", target_os = "linux"))]
mod differential;
#[cfg(feature = "host")]
mod script_vm;

// What the guest's checks print as they fail.
#[cfg(feature = "guest")]
mod kernel_log;

// cladefs: what the guest halves of the guest's Fxfs's tests share: evicting, DAX, a mount's
// counts, and copying on write's files read in from sealed extents (doc-13).
#[cfg(feature = "guest")]
mod guest_fxfs;

// What the guest's tests unbind the extent driver with.
#[cfg(feature = "guest")]
mod extent_driver;

#[cfg(any(feature = "guest", test))]
mod sqlite;
#[cfg(any(feature = "host", target_os = "linux"))]
mod test_fs_bench;
// cladefs: the loop-device baseline's disk keeps what a guest fsyncs (TASK-350.1).
#[cfg(any(feature = "host", target_os = "linux"))]
mod test_ext4_loop_persists;
#[cfg(any(feature = "host", target_os = "linux"))]
use test_ext4_loop_persists::TestExt4LoopPersists;
#[cfg(any(feature = "host", target_os = "linux"))]
use test_fs_bench::{Backend as BenchBackend, TestFsBench};

pub enum TestOutcome {
    Pass,
    Fail(String),
    Timeout,
    Skip(&'static str),
    Report(Box<dyn ReportImpl>),
}

pub enum ShouldRun {
    Yes,
    No(&'static str),
    /// cladefs: not on this host, which isn't Linux, as the case needs what only Linux has
    /// ([`Test::linux_only`]).
    LinuxOnly(&'static str),
}

// cladefs: what the cases that run on Linux alone need of their host that only Linux has
// ([`Test::linux_only`], board 14336). Each is something macOS has no counterpart of for the case,
// not a feature its VMM, or cladefs on macOS, lacks yet: those cases run there, and fail, until it
// has it.

/// A loop device, the Linux host's block device over an image file, as production's local disk
/// images are: what the case measures or checks (TASK-350.1).
pub const LINUX_ONLY_LOOP: &str = "a loop device over an image, as production's local disks are";
/// What a Linux host, the production host, says of itself and its VM processes in /proc, which the
/// case measures: their proportional shares of memory (smaps_rollup), KVM's page tables, its
/// disks' I/O and pressure (PSI).
pub const LINUX_ONLY_HOST_MEASURES: &str =
    "measures a Linux host's /proc: PSS (smaps_rollup), KVM's page tables, disk I/O and PSI";

/// cladefs: declares [`Tag`], each tag's variant and name, its doc its help (`runner list --help`).
macro_rules! tags {
    ($($(#[doc = $doc:literal])+ $tag:ident = $name:literal,)+) => {
        /// cladefs: the suites a case is in, which the scripts select it by (`runner list --tag`),
        /// none of them keeping a list of names: each list a script kept missed cases the others
        /// had, and a case new to the harness was in none (TASK-160.29). Every case has one at
        /// least, [`Tag::Manual`] if no suite runs it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Tag {
            $($(#[doc = $doc])+ $tag,)+
        }

        impl Tag {
            pub const ALL: &[Tag] = &[$(Tag::$tag,)+];

            /// The tag as the scripts name it.
            pub fn name(self) -> &'static str {
                match self {
                    $(Tag::$tag => $name,)+
                }
            }

            /// What it selects: its doc, on a line.
            pub fn about(self) -> &'static str {
                match self {
                    $(Tag::$tag => concat!($($doc),+).trim_start(),)+
                }
            }

            /// The tag named `name`.
            pub fn named(name: &str) -> Option<Tag> {
                Self::ALL.iter().copied().find(|tag| tag.name() == name)
            }
        }
    };
}

tags! {
    /// scripts/test-e2e.sh's cases unless it is told others, which scripts/test-host.sh runs
    /// unless CLADEFS_E2E_TEST says otherwise: the guest's Fxfs, its crash tests and checks, and
    /// pgbench, with a DAX window and without, in minutes.
    E2e = "e2e",
    /// scripts/test-guest-kernel.sh's: the guest kernel's Fxfs, as its every feature is used.
    Kernel = "kernel",
    /// The nightly's (.github/workflows/nightly.yml), as production runs them: e2e's and
    /// kernel's, every other case cheap enough on a release kernel, and xfstests.
    Nightly = "nightly",
    /// scripts/dax-gate.sh's suites, repeats of each, in its first run.
    DaxGate = "dax-gate",
    /// scripts/soak-workloads-host.sh's crash group: the VM stopped mid-workload.
    SoakCrash = "soak-crash",
    /// scripts/soak-workloads-host.sh's checks group: with delays and on a small window.
    SoakChecks = "soak-checks",
    /// scripts/soak-workloads-host.sh's sanitized group, on the debug kernel.
    SoakSanitized = "soak-sanitized",
    /// scripts/soak-workloads-host.sh's replicas group: snapshots and migrations.
    SoakReplicas = "soak-replicas",
    /// scripts/soak-workloads-host.sh's mixed group: VMs on forks of a base under mixed load for
    /// hours, through VMs, the daemon and the host crashing and uploads failing.
    SoakMixed = "soak-mixed",
    /// scripts/perf-gate-host.sh's cases of one VM each.
    PerfSingle = "perf-single",
    /// scripts/perf-gate-host.sh's density run, VMs on forks of one base.
    PerfDensity = "perf-density",
    /// scripts/cells-per-host-host.sh's: VMs on forks of one base, added until the host holds no
    /// more (board 5581).
    CellsPerHost = "cells-per-host",
    /// scripts/bench.sh's cases unless CLADEFS_BENCH_CASES says others.
    Bench = "bench",
    /// In no suite, run by name alone: measurements of one thing (a profile, a lock's
    /// contention, a fork's), and runner-deadlock, which fails on purpose.
    Manual = "manual",
}

/// cladefs: the cases of `cases` named by `names` or tagged with one of `tags`, each once, in their
/// order there: those a script runs (`runner list`). It fails on a name no case has.
pub fn select(
    cases: Vec<TestCase>,
    names: &[String],
    tags: &[Tag],
) -> Result<Vec<TestCase>, String> {
    if let Some(unknown) = names.iter().find(|name| cases.iter().all(|case| case.name != *name)) {
        return Err(format!("no such test case: {unknown}"));
    }
    Ok(cases
        .into_iter()
        .filter(|case| {
            names.iter().any(|name| name == case.name)
                || case.tags.iter().any(|tag| tags.contains(tag))
        })
        .collect())
}

/// cladefs: one of a run's parts, the `index`th of `count` (`runner list --shard K/N`): a
/// selection's every `count`th case from its `index`th, as registered. The parts of a selection
/// are it, each case in one of them, and a case's part is its place's alone, so that jobs of a
/// matrix, each a part, run a suite together, no list of names kept (TASK-284).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shard {
    index: usize,
    count: usize,
}

impl std::str::FromStr for Shard {
    type Err = String;

    /// `K/N`, the `K`th part of `N`, from 1.
    fn from_str(text: &str) -> Result<Shard, String> {
        let number = |part: &str| part.parse::<usize>().ok();
        match text.split_once('/').and_then(|(index, count)| Some((number(index)?, number(count)?)))
        {
            Some((index, count)) if 1 <= index && index <= count => Ok(Shard { index, count }),
            _ => Err(format!("a shard is K/N, its Kth part of N, 1 <= K <= N: not {text}")),
        }
    }
}

impl Shard {
    /// The cases of `cases` in this part.
    pub fn of(self, cases: Vec<TestCase>) -> Vec<TestCase> {
        cases.into_iter().skip(self.index - 1).step_by(self.count).collect()
    }
}

pub fn test_cases() -> Vec<TestCase> {
    // Register your test here, with the suites it is in (cladefs: TASK-160.29):
    vec![
        TestCase::new(
            "pjdfstest-fxfs-guest",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestPjdfstest { dax: false }),
        ),
        TestCase::new(
            "pjdfstest-fxfs-guest-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestPjdfstest { dax: true }),
        ),
        // cladefs: the kernel's own filesystem suite, nightly, its known failures checked each
        // night: in no suite, it ran only when someone named it (TASK-149.20.1).
        TestCase::new(
            "xfstests-fxfs-guest",
            &[Tag::Nightly],
            Box::new(TestXfstests { dax: false }),
        ),
        TestCase::new(
            "xfstests-fxfs-guest-dax",
            &[Tag::Nightly],
            Box::new(TestXfstests { dax: true }),
        ),
        TestCase::new(
            "fxfs-guest",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuest::host_formats()),
        ),
        TestCase::new(
            "fxfs-guest-format",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuest::guest_formats()),
        ),
        TestCase::new(
            "fxfs-guest-crash",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly, Tag::SoakCrash],
            Box::new(TestFxfsGuest::crash()),
        ),
        TestCase::new(
            "fxfs-guest-resize",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestResize),
        ),
        TestCase::new(
            "fxfs-guest-pressure",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestPressure),
        ),
        TestCase::new(
            "fxfs-guest-stats",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestStats),
        ),
        TestCase::new(
            "fxfs-guest-shared-disk",
            &[Tag::Manual],
            Box::new(TestFxfsGuestSharedDisk { dax: false }),
        ),
        TestCase::new(
            "fxfs-guest-shared-disk-dax",
            &[Tag::Manual],
            Box::new(TestFxfsGuestSharedDisk { dax: true }),
        ),
        // cladefs: guests writing against a degraded bucket for half an hour, TASK-330's
        // qualification's (TASK-330.3).
        TestCase::new("fxfs-guest-bucket-soak", &[Tag::Manual], Box::new(TestFxfsGuestBucketSoak)),
        // cladefs: sandboxes as LangSmith's CI runs them, TASK-330's qualification's, which
        // scripts/sandboxes.sh runs with the S3 server it needs (TASK-330.1).
        TestCase::new("fxfs-guest-sandboxes", &[Tag::Manual], Box::new(TestFxfsGuestSandboxes)),
        // cladefs: a data-analysis sandbox, uv-installed and uncompiled, timed from its fork to its
        // answers on three hosts sharing one bucket, which scripts/test-e2e-rustfs.sh starts
        // (TASK-665).
        TestCase::new(
            "fxfs-guest-python-sandbox",
            &[Tag::Manual],
            Box::new(TestFxfsGuestPythonSandbox),
        ),
        TestCase::new(
            "fxfs-guest-live-fork",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestLiveFork),
        ),
        TestCase::new(
            "fxfs-guest-volumes",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestVolumes { dax: false }),
        ),
        TestCase::new(
            "fxfs-guest-volumes-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestVolumes { dax: true }),
        ),
        TestCase::new(
            "fxfs-guest-bpftrace-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestBpftrace),
        ),
        TestCase::new(
            "linux-checkout-ext4",
            &[Tag::Manual],
            Box::new(TestLinuxCheckout { on: Checkout::Ext4 }),
        ),
        TestCase::new(
            "linux-checkout-ext4-pmem",
            &[Tag::Manual],
            Box::new(TestLinuxCheckout { on: Checkout::Ext4Pmem }),
        ),
        TestCase::new(
            "linux-checkout-fxfs",
            &[Tag::Manual],
            Box::new(TestLinuxCheckout { on: Checkout::Fxfs }),
        ),
        TestCase::new(
            "linux-checkout-fxfs-dax",
            &[Tag::Manual],
            Box::new(TestLinuxCheckout { on: Checkout::FxfsDax }),
        ),
        TestCase::new(
            "fxfs-guest-pgbench",
            &[Tag::E2e, Tag::Nightly, Tag::PerfSingle],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Fxfs)),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-dax",
            &[Tag::E2e, Tag::Nightly, Tag::PerfSingle],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::FxfsDax)),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-profile",
            &[Tag::Manual],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Fxfs).probed(Probe::Profile)),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-dax-profile",
            &[Tag::Manual],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::FxfsDax).probed(Probe::Profile)),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-locks",
            &[Tag::Manual],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Fxfs).probed(Probe::Locks)),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-nofsync",
            &[Tag::Manual],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Fxfs).without_fsync()),
        ),
        TestCase::new(
            "pgbench-ext4-dax-pmem",
            &[Tag::PerfSingle],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Ext4DaxPmem)),
        ),
        TestCase::new(
            "pgbench-ext4-blk-unsafe",
            &[Tag::PerfSingle],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Ext4BlkUnsafe)),
        ),
        TestCase::new(
            "pgbench-ext4-blk-writeback",
            &[Tag::PerfSingle],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Ext4BlkWriteback)),
        ),
        TestCase::new(
            "pgbench-ext4-loop-writeback",
            &[Tag::PerfSingle],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Ext4LoopWriteback)),
        ),
        TestCase::new(
            "pgbench-ext4-loop-unsafe",
            &[Tag::PerfSingle],
            Box::new(TestFxfsGuestPgbench::on(PgbenchStorage::Ext4LoopUnsafe)),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-forked",
            &[Tag::PerfSingle],
            Box::new(
                TestFxfsGuestPgbench::on(PgbenchStorage::Fxfs)
                    .with_database(PgbenchDatabase::Forked),
            ),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-dax-forked",
            &[Tag::PerfSingle],
            Box::new(
                TestFxfsGuestPgbench::on(PgbenchStorage::FxfsDax)
                    .with_database(PgbenchDatabase::Forked),
            ),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-density",
            &[Tag::PerfDensity],
            Box::new(
                TestFxfsGuestPgbench::on(PgbenchStorage::Fxfs)
                    .with_database(PgbenchDatabase::Density),
            ),
        ),
        TestCase::new(
            "fxfs-guest-pgbench-dax-density",
            &[Tag::PerfDensity],
            Box::new(
                TestFxfsGuestPgbench::on(PgbenchStorage::FxfsDax)
                    .with_database(PgbenchDatabase::Density),
            ),
        ),
        // cladefs: how many cells a host holds, without DAX and with it (board 5583); and with
        // each VM restoring a zygote, its memory shared (board 7084).
        TestCase::new(
            "cells-per-host",
            &[Tag::CellsPerHost],
            Box::new(TestCellsPerHost { dax: false, zygote: false }),
        ),
        TestCase::new(
            "cells-per-host-dax",
            &[Tag::CellsPerHost],
            Box::new(TestCellsPerHost { dax: true, zygote: false }),
        ),
        TestCase::new(
            "cells-per-host-dax-zygote",
            &[Tag::CellsPerHost],
            Box::new(TestCellsPerHost { dax: true, zygote: true }),
        ),
        TestCase::new(
            "fxfs-guest-bigmap",
            &[Tag::PerfSingle],
            Box::new(TestFxfsGuestBigmap { dax: false }),
        ),
        TestCase::new(
            "fxfs-guest-bigmap-dax",
            &[Tag::PerfSingle],
            Box::new(TestFxfsGuestBigmap { dax: true }),
        ),
        TestCase::new(
            "fxfs-guest-daemon-lost",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestDaemonLost),
        ),
        TestCase::new("runner-deadlock", &[Tag::Manual], Box::new(TestRunnerDeadlock)),
        TestCase::new(
            "fxfs-guest-import",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestImport { dax: false }),
        ),
        TestCase::new(
            "fxfs-guest-import-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestImport { dax: true }),
        ),
        TestCase::new(
            "fxfs-guest-snapshot",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestSnapshot {
                dax: false,
                mode: SnapshotMode::Zygote,
                volumes: 0,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-snapshot-dax",
            &[Tag::Kernel, Tag::Nightly, Tag::SoakReplicas],
            Box::new(TestFxfsGuestSnapshot {
                dax: true,
                mode: SnapshotMode::Zygote,
                volumes: 0,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-snapshot-memory-cell-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestSnapshot {
                dax: true,
                mode: SnapshotMode::Zygote,
                volumes: 0,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::Cell,
            }),
        ),
        TestCase::new(
            "fxfs-guest-memory-cell-snapshot-and-dirty",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestMemoryCell),
        ),
        TestCase::new(
            "fxfs-guest-ephemeral-local-suspend",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestEphemeralSuspend),
        ),
        TestCase::new(
            "fxfs-guest-park-resume-vsock-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestParkResume),
        ),
        TestCase::new(
            "fxfs-guest-live-migration",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestLiveMigration),
        ),
        TestCase::new(
            "fxfs-guest-suspend-dax",
            &[Tag::Kernel, Tag::Nightly, Tag::SoakReplicas],
            Box::new(TestFxfsGuestSnapshot {
                dax: true,
                mode: SnapshotMode::Suspend,
                volumes: 0,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-suspend-read-only-volume",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestSnapshot {
                dax: false,
                mode: SnapshotMode::Suspend,
                volumes: 0,
                read_only: SnapshotReadOnly::Volume,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-suspend-remounted-read-only",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestSnapshot {
                dax: false,
                mode: SnapshotMode::Suspend,
                volumes: 0,
                read_only: SnapshotReadOnly::Remounted,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-snapshot-volumes",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestSnapshot {
                dax: false,
                mode: SnapshotMode::Suspend,
                volumes: VOLUMES,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-snapshot-volumes-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestSnapshot {
                dax: true,
                mode: SnapshotMode::Suspend,
                volumes: VOLUMES,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-migrate-dax",
            &[Tag::Kernel, Tag::Nightly, Tag::SoakReplicas],
            Box::new(TestFxfsGuestSnapshot {
                dax: true,
                mode: SnapshotMode::Migrate,
                volumes: 0,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::File,
            }),
        ),
        TestCase::new(
            "fxfs-guest-migrate-tenant-dax",
            &[Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestSnapshot {
                dax: true,
                mode: SnapshotMode::TenantMigrate,
                volumes: 0,
                read_only: SnapshotReadOnly::Nothing,
                memory: SnapshotMemory::File,
            }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-checks",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly, Tag::SoakChecks, Tag::SoakSanitized],
            Box::new(PAGE_CACHE_CHECKS),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-checks-dax",
            &[
                Tag::E2e,
                Tag::Kernel,
                Tag::Nightly,
                Tag::DaxGate,
                Tag::SoakChecks,
                Tag::SoakSanitized,
            ],
            Box::new(TestFxfsGuestChecks { dax: true, ..PAGE_CACHE_CHECKS }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-checks-dax-delay",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly, Tag::DaxGate, Tag::SoakChecks],
            Box::new(TestFxfsGuestChecks { dax: true, delay: true, ..PAGE_CACHE_CHECKS }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-checks-dax-small",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly, Tag::DaxGate, Tag::SoakChecks],
            Box::new(TestFxfsGuestChecks { dax: true, small_window: true, ..PAGE_CACHE_CHECKS }),
        ),
        // cladefs: a check outrunning its budget fails the run by name (TASK-168).
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-checks-overrun",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestFxfsGuestChecks { overrun: true, ..PAGE_CACHE_CHECKS }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new("fxfs-guest-soak", &[Tag::SoakMixed], Box::new(TestFxfsGuestSoak)),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: false, full: false, killed: false }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script-dax",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: true, full: false, killed: false }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script-full",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: false, full: true, killed: false }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script-full-dax",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: true, full: true, killed: false }),
        ),
        // cladefs: the script's VM killed part way, its files checked against what a kill may leave
        // (TASK-149.2).
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script-killed",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: false, full: false, killed: true }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script-killed-dax",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: true, full: false, killed: true }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script-killed-full",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: false, full: true, killed: true }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-script-killed-full-dax",
            &[Tag::Nightly],
            Box::new(TestFxfsGuestScript { dax: true, full: true, killed: true }),
        ),
        // cladefs: fsx-style scripts on Fxfs and ext4 in one guest (TASK-44.15).
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-fsx",
            &[Tag::Kernel, Tag::Nightly, Tag::SoakChecks],
            Box::new(TestFxfsGuestFsx { dax: false, delay: false, faults: false }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-fsx-dax",
            &[Tag::Kernel, Tag::Nightly, Tag::SoakChecks],
            Box::new(TestFxfsGuestFsx { dax: true, delay: false, faults: false }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-fsx-delay-dax",
            &[Tag::Nightly, Tag::SoakChecks],
            Box::new(TestFxfsGuestFsx { dax: true, delay: true, faults: false }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "fxfs-guest-fsx-faults-dax",
            &[Tag::SoakSanitized],
            Box::new(TestFxfsGuestFsx { dax: true, delay: false, faults: true }),
        ),
        TestCase::new(
            "fxfs-guest-dax",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly, Tag::DaxGate],
            Box::new(TestFxfsGuest::host_formats().with_dax()),
        ),
        TestCase::new(
            "fxfs-guest-crash-dax",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly, Tag::DaxGate, Tag::SoakCrash],
            Box::new(TestFxfsGuest::crash().with_dax()),
        ),
        TestCase::new(
            "fxfs-guest-crash-dax-killed",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly, Tag::DaxGate, Tag::SoakCrash],
            Box::new(TestFxfsGuest::crash_killed()),
        ),
        TestCase::new(
            "fxfs-guest-crash-dax-daemon-killed",
            &[Tag::Nightly],
            Box::new(TestFxfsGuest::crash_daemon_killed()),
        ),
        TestCase::new(
            "fxfs-guest-crash-stray-dax",
            &[Tag::E2e, Tag::Nightly],
            Box::new(TestFxfsGuest::crash_stray()),
        ),
        TestCase::new(
            "fxfs-guest-faults-dax",
            &[Tag::SoakSanitized],
            Box::new(TestFxfsGuest::faults()),
        ),
        TestCase::new(
            "fxfs-guest-docker-dax",
            &[Tag::E2e, Tag::Nightly],
            Box::new(TestFxfsGuestDocker { build: false }),
        ),
        // cladefs: LangSmith's builds, on 8 vCPUs, which oopsed 2 in 17 times (TASK-311).
        TestCase::new(
            "fxfs-guest-docker-build-dax",
            &[Tag::Manual],
            Box::new(TestFxfsGuestDocker { build: true }),
        ),
        TestCase::new(
            "extent-guest",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuest),
        ),
        // cladefs board 14352: the self-test refused while a filesystem is mounted.
        TestCase::new(
            "extent-guest-selftest-mounted",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestSelftestMounted),
        ),
        TestCase::new(
            "extent-guest-other-version",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestOtherVersion),
        ),
        // cladefs board 8557: a configuration the driver refuses, a case for each field it checks.
        TestCase::new(
            "extent-guest-bad-block-size-zero",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestBadConfig {
                fault: "block_size=0",
                refusal: "block_size: the device says 0, not a power of two",
            }),
        ),
        TestCase::new(
            "extent-guest-bad-block-size-odd",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestBadConfig {
                fault: "block_size=3000",
                refusal: "block_size: the device says 3000, not a power of two",
            }),
        ),
        TestCase::new(
            "extent-guest-bad-map-alignment-zero",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestBadConfig {
                fault: "map_alignment=0",
                refusal: "map_alignment: the device says 0, not a power of two",
            }),
        ),
        TestCase::new(
            "extent-guest-bad-max-transfer",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestBadConfig {
                fault: "max_transfer=2048",
                refusal: "max_transfer: the device says 2048, less than a block",
            }),
        ),
        TestCase::new(
            "extent-guest-bad-max-segments",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestBadConfig {
                fault: "max_segments=1",
                refusal: "max_segments: the device says 1, fewer than a block takes, unaligned",
            }),
        ),
        TestCase::new(
            "extent-guest-bad-ring",
            &[Tag::E2e, Tag::Kernel, Tag::Nightly],
            Box::new(TestExtentGuestBadConfig {
                fault: "ring=5",
                refusal: "ring: the request queues' rings hold 5 descriptors, fewer than a request of 2 data segments takes, 6",
            }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-ext4-guest-kernel",
            &[Tag::Bench],
            Box::new(TestFsBench { backend: BenchBackend::Ext4GuestKernel }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new("ext4-loop-persists", &[Tag::Nightly], Box::new(TestExt4LoopPersists)),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-fxfs-guest-dax-unsafe",
            &[Tag::Bench],
            Box::new(TestFsBench { backend: BenchBackend::FxfsGuestDaxUnsafe }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-ext4-loop",
            &[Tag::Bench],
            Box::new(TestFsBench { backend: BenchBackend::Ext4Loop }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-ext4-loop-unsafe",
            &[Tag::Bench],
            Box::new(TestFsBench { backend: BenchBackend::Ext4LoopUnsafe }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-ext4-dax-pmem",
            &[Tag::Bench],
            Box::new(TestFsBench { backend: BenchBackend::Ext4DaxPmem }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-ext4-pmem",
            &[Tag::Manual],
            Box::new(TestFsBench { backend: BenchBackend::Ext4Pmem }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-fxfs-guest",
            &[Tag::Bench],
            Box::new(TestFsBench { backend: BenchBackend::FxfsGuest }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fs-fxfs-guest-dax",
            &[Tag::Bench],
            Box::new(TestFsBench { backend: BenchBackend::FxfsGuestDax }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fork-fxfs-guest-dax-warm",
            &[Tag::Manual],
            Box::new(TestFsBench { backend: BenchBackend::FxfsGuestDaxForkWarm }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fork-fxfs-guest-dax-cold",
            &[Tag::Manual],
            Box::new(TestFsBench { backend: BenchBackend::FxfsGuestDaxForkCold }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fork-fxfs-guest-warm",
            &[Tag::Manual],
            Box::new(TestFsBench { backend: BenchBackend::FxfsGuestForkWarm }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fork-ext4-dax-pmem",
            &[Tag::Manual],
            Box::new(TestFsBench { backend: BenchBackend::Ext4DaxPmemFork }),
        ),
        #[cfg(any(feature = "host", target_os = "linux"))]
        TestCase::new(
            "bench-fork-ext4-pmem",
            &[Tag::Manual],
            Box::new(TestFsBench { backend: BenchBackend::Ext4PmemFork }),
        ),
        TestCase::new(
            "bench-fork-fxfs-guest-dax-predicted",
            &[Tag::Manual],
            Box::new(TestFsBench { backend: BenchBackend::FxfsGuestDaxForkPredicted }),
        ),
    ]
}

////////////////////
// Implementation details:
//////////////////

pub trait ReportImpl {
    fn fmt_text(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
    fn fmt_gh_markdown(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

pub trait Report: ReportImpl {
    fn text(&self) -> ReportText<'_, Self> {
        ReportText(self)
    }

    fn gh_markdown(&self) -> ReportGhMarkdown<'_, Self> {
        ReportGhMarkdown(self)
    }
}

impl<T: ReportImpl + ?Sized> Report for T {}

pub struct ReportText<'a, T: ReportImpl + ?Sized>(pub &'a T);

impl<T: ReportImpl + ?Sized> std::fmt::Display for ReportText<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt_text(f)
    }
}

pub struct ReportGhMarkdown<'a, T: ReportImpl + ?Sized>(pub &'a T);

impl<T: ReportImpl + ?Sized> std::fmt::Display for ReportGhMarkdown<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt_gh_markdown(f)
    }
}

use macros::{guest, host};
#[host]
use std::path::PathBuf;

#[cfg(all(feature = "guest", feature = "host"))]
compile_error!("Cannot enable both guest and host in the same binary!");

#[cfg(feature = "host")]
mod common;

#[cfg(all(feature = "host", target_os = "linux"))]
pub mod cgroups;

#[cfg(feature = "host")]
mod faulty_s3;

#[cfg(feature = "host")]
pub mod rootfs;

mod checkout_vms;
pub mod reconnect;

#[cfg(feature = "host")]
pub mod loop_device;
#[cfg(feature = "host")]
pub mod vm;

#[cfg(all(feature = "guest", target_os = "linux"))]
pub mod guest_init;

/// Under a VMM whose guest has a single console (Firecracker's serial port), the guest agent's
/// standard output goes there between these bytes (start and end of text), and its standard error
/// between the next two (shift out, shift in), which the host splits apart, and from what the
/// kernel says there.
pub const STDOUT_START: u8 = 0x02;
pub const STDOUT_END: u8 = 0x03;
pub const STDERR_START: u8 = 0x0e;
pub const STDERR_END: u8 = 0x0f;

/// cladefs: in a test's environment, when the runner kills it if it runs on, in seconds since the
/// epoch: so that its VMs' guests say where they are stuck before then ([`GUEST_WATCHDOG`]).
pub const TEST_DEADLINE: &str = "CLADEFS_TEST_DEADLINE";

/// cladefs: in the guest agent's environment, the seconds after which the guest, still running,
/// dumps its tasks' and CPUs' stacks, shortly before the runner's deadline (`guest_init`).
pub const GUEST_WATCHDOG: &str = "CLADEFS_GUEST_WATCHDOG_SECS";

#[host]
#[derive(Clone, Debug)]
pub struct TestSetup {
    pub test_case: String,
    // A tmp directory for misc. artifacts used be the test (e.g. sockets)
    pub tmp_dir: PathBuf,
}

#[host]
impl TestSetup {
    /// Register a PID to be killed after the test finishes.
    ///
    /// The runner will SIGKILL these PIDs after check() returns, even if the
    /// test crashed. Use this for background processes (e.g. gvproxy) that
    /// outlive the VM.
    pub fn register_cleanup_pid(&self, pid: u32) {
        use std::io::Write;
        let path = self.tmp_dir.join("cleanup.pids");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("Failed to open cleanup.pids");
        writeln!(file, "{}", pid).expect("Failed to write cleanup PID");
    }
}

#[host]
pub trait Test {
    /// Start the VM
    fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()>;

    /// Checks the output of the (host) process which started the VM
    fn check(self: Box<Self>, stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
        let output = String::from_utf8(stdout).unwrap();
        if output == "OK\n" {
            TestOutcome::Pass
        } else {
            TestOutcome::Fail(format!("expected exactly {:?}, got {:?}", "OK\n", output))
        }
    }

    /// Check if this test should run on this platform.
    fn should_run(&self) -> ShouldRun {
        ShouldRun::Yes
    }

    /// cladefs: what of its host the case needs that only Linux has, one of the `LINUX_ONLY_*`
    /// reasons, if any: elsewhere it doesn't run, and says so, whatever its environment.
    fn linux_only(&self) -> Option<&'static str> {
        None
    }

    /// Return the image (its Containerfile, cladefs: and what its build takes from the host) if
    /// this test needs a custom rootfs image.
    /// The runner will build the image via podman (cladefs: or docker, `rootfs::engine`) and
    /// extract it before launching the VM. If neither runs, the test is skipped.
    fn rootfs_image(&self) -> Option<rootfs::Image> {
        None
    }

    /// Per-test timeout in seconds. The runner kills the test if it exceeds this.
    fn timeout_secs(&self) -> u64 {
        15
    }

    /// cladefs: what a test that ran out of time, its VM gone, has to add to that.
    fn timed_out(self: Box<Self>, _test_setup: TestSetup) -> Option<String> {
        None
    }

    /// cladefs: whether the runner fails the test, once it passed, on its VMs' kernels complaining
    /// on their consoles (`vm::complaints`, TASK-44.8): unless it means them to.
    fn checks_console(&self) -> bool {
        true
    }
}

#[guest]
pub trait Test {
    /// This will be executed in the guest, you can panic! if the test failed!
    fn in_guest(self: Box<Self>) {}
}

pub struct TestCase {
    pub name: &'static str,
    /// cladefs: the suites it is in.
    pub tags: &'static [Tag],
    pub test: Box<dyn Test>,
}

impl TestCase {
    // Your test can be parametrized, so you can add the same test multiple times constructed with
    // different parameters with and specify a different name here.
    pub fn new(name: &'static str, tags: &'static [Tag], test: Box<dyn Test>) -> Self {
        Self { name, tags, test }
    }

    /// Check if this test should run on this platform.
    #[host]
    pub fn should_run(&self) -> ShouldRun {
        match self.test.linux_only() {
            Some(why) if !cfg!(target_os = "linux") => ShouldRun::LinuxOnly(why),
            _ => self.test.should_run(),
        }
    }

    #[host]
    pub fn rootfs_image(&self) -> Option<rootfs::Image> {
        self.test.rootfs_image()
    }

    #[host]
    pub fn timeout_secs(&self) -> u64 {
        self.test.timeout_secs()
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_testcases_have_unique_names() {
        let test_cases = test_cases();
        let mut names: HashSet<&str> = HashSet::new();

        for test_case in test_cases {
            let name = test_case.name();
            let was_inserted = names.insert(name);
            if !was_inserted {
                panic!("test_cases() contains multiple items named `{name}`")
            }

            if name == "all" {
                panic!("test_cases() contains test named {name}, but the name is reseved")
            }
        }
    }

    /// cladefs: the names of the cases of `cases` in no suite, not even [`Tag::Manual`].
    fn untagged(cases: &[TestCase]) -> Vec<&'static str> {
        cases.iter().filter(|case| case.tags.is_empty()).map(|case| case.name).collect()
    }

    /// cladefs: a case new to the harness says where it runs, or where it doesn't: one in no suite
    /// was run by none of the scripts, as xfstests' and the scripts' cases were (TASK-160.29).
    #[test]
    fn every_case_is_in_a_suite() {
        assert_eq!(untagged(&test_cases()), Vec::<&str>::new());
        let mut cases = test_cases();
        cases.push(TestCase::new("untagged", &[], Box::new(TestRunnerDeadlock)));
        assert_eq!(untagged(&cases), ["untagged"]);
    }

    /// cladefs: no suite is empty, and each tag's name is its own.
    #[test]
    fn every_tag_selects_cases() {
        let cases = test_cases();
        for &tag in Tag::ALL {
            assert!(cases.iter().any(|case| case.tags.contains(&tag)), "{tag:?} selects nothing");
            assert_eq!(Tag::named(tag.name()), Some(tag));
            assert!(!tag.about().is_empty() && !tag.about().starts_with(' '), "{tag:?}");
        }
        assert_eq!(Tag::named("nope"), None);
    }

    /// cladefs: the cases that run on Linux alone, each saying why, and no others: one joins them
    /// here, as what it needs of Linux, or runs on macOS too (board 14336).
    #[cfg(feature = "host")]
    #[test]
    fn linux_only_cases_say_why() {
        let linux_only: Vec<(&str, &str)> = (test_cases().iter())
            .filter_map(|case| Some((case.name, case.test.linux_only()?)))
            .collect();
        let (looped, measured) = (LINUX_ONLY_LOOP, LINUX_ONLY_HOST_MEASURES);
        assert_eq!(
            linux_only,
            [
                ("fxfs-guest-shared-disk", measured),
                ("fxfs-guest-shared-disk-dax", measured),
                ("pgbench-ext4-loop-writeback", looped),
                ("pgbench-ext4-loop-unsafe", looped),
                ("cells-per-host", measured),
                ("cells-per-host-dax", measured),
                ("cells-per-host-dax-zygote", measured),
                ("ext4-loop-persists", looped),
                ("bench-fs-ext4-loop", looped),
                ("bench-fs-ext4-loop-unsafe", looped),
            ]
        );
    }

    fn names_of(cases: Vec<TestCase>) -> Vec<&'static str> {
        cases.into_iter().map(|case| case.name).collect()
    }

    /// cladefs: a selection is of the cases named and tagged, once each, as registered.
    #[test]
    fn a_selection_is_of_cases_named_and_tagged() {
        let select = |names: &[&str], tags: &[Tag]| {
            let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
            select(test_cases(), &names, tags).map(names_of)
        };
        assert_eq!(
            select(&[], &[Tag::PerfDensity]),
            Ok(vec!["fxfs-guest-pgbench-density", "fxfs-guest-pgbench-dax-density"])
        );
        // Named before the tag's, but run as registered, and once.
        assert_eq!(
            select(&["fxfs-guest-dax", "extent-guest"], &[Tag::DaxGate, Tag::PerfDensity]),
            Ok(vec![
                "fxfs-guest-pgbench-density",
                "fxfs-guest-pgbench-dax-density",
                "fxfs-guest-checks-dax",
                "fxfs-guest-checks-dax-delay",
                "fxfs-guest-checks-dax-small",
                "fxfs-guest-dax",
                "fxfs-guest-crash-dax",
                "fxfs-guest-crash-dax-killed",
                "extent-guest",
            ])
        );
        assert_eq!(select(&[], &[]), Ok(vec![]));
        assert_eq!(
            select(&["fxfs-guest-dax", "fxfs-guest-nope"], &[]),
            Err("no such test case: fxfs-guest-nope".to_string())
        );
    }

    /// cladefs: a suite's parts are it, each case in one part, every Nth from the Kth (TASK-284).
    #[test]
    fn a_suites_parts_are_it_each_case_in_one() {
        let nightly = || select(test_cases(), &[], &[Tag::Nightly]).unwrap();
        let whole = names_of(nightly());
        let parts: Vec<Vec<&str>> = (1..=4)
            .map(|index| names_of(format!("{index}/4").parse::<Shard>().unwrap().of(nightly())))
            .collect();
        for (index, part) in parts.iter().enumerate() {
            let expected: Vec<&str> = whole.iter().copied().skip(index).step_by(4).collect();
            assert_eq!(part, &expected, "part {}", index + 1);
        }
        let mut joined: Vec<&str> = parts.concat();
        let mut sorted_whole = whole.clone();
        joined.sort_unstable();
        sorted_whole.sort_unstable();
        assert_eq!(joined, sorted_whole);
        assert_eq!(names_of("1/1".parse::<Shard>().unwrap().of(nightly())), whole);
        assert_eq!(
            names_of(
                "2/3"
                    .parse::<Shard>()
                    .unwrap()
                    .of(select(test_cases(), &[], &[Tag::PerfDensity]).unwrap())
            ),
            vec!["fxfs-guest-pgbench-dax-density"]
        );
    }

    /// cladefs: a shard is K/N, 1 <= K <= N.
    #[test]
    fn a_shard_is_its_part_of_a_count() {
        assert_eq!("3/8".parse(), Ok(Shard { index: 3, count: 8 }));
        for bad in ["0/4", "5/4", "4", "a/4", "1/0", "1/4/2", ""] {
            assert_eq!(
                bad.parse::<Shard>(),
                Err(format!("a shard is K/N, its Kth part of N, 1 <= K <= N: not {bad}")),
                "{bad}"
            );
        }
    }
}
