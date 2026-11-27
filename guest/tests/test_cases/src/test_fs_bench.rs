// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Benchmark the guest kernel's own Fxfs (cladefs's guest kernel, CLADEFS_GUEST_KERNEL), on an
// extent-store device that cladefs's extent daemon serves, with a DAX window and without, against
// ext4 booted on the same kernel, on a virtio-blk disk backed by a raw image file on the same
// host filesystem, and with DAX on writable virtio-pmem, where file data and metadata are stored
// in place in memory. Every case boots cladefs's Firecracker.
//
// Each case boots an Alpine guest, with fio, a compiler and e2fsprogs, onto one filesystem mounted
// at /mnt/bench, and runs the same workloads on it several times: sequential and random reads and
// writes, cold and warm, fsync-heavy writes, I/O through mmap (and SQLite's reads through it),
// metadata operations on many small files, and a build. The guest prints a measurement per workload
// and iteration, and the host reports their medians and spread. The workloads are sized for an
// iteration of each case to take well under a minute.
//
// The cases only run when CLADEFS_BENCH_ITERATIONS is set. CLADEFS_BENCH_GROUPS limits them to some
// of the groups of workloads: io, metadata and build, separated by commas, or parts of metadata:
// files, fsync, tree, large-dir and concurrent; pgbench, PostgreSQL's benchmark, runs only when it
// names it. The sqlite group adds rollback-journal and WAL commits, batches, checkpoints, process
// recovery and reader isolation; it also runs the mmap scan independently of io. The scaling group,
// which also runs only when named, runs random reads cold and warm, random writes (to pages read
// before, and to pages the guest never read, rand-write-unread), fsynced appends and metadata
// operations at each of CLADEFS_BENCH_JOBS's jobs at once (1, 2, 4 and 8), with their latencies'
// percentiles and how the device's queues took their requests; CLADEFS_BENCH_VCPUS sets the VMs'
// vCPUs (2), to measure how the filesystems scale (TASK-276). CLADEFS_BENCH_SQLITE_TXNS sizes each
// transaction workload (200 commits). The sqlite-transactions group skips scans;
// CLADEFS_BENCH_SQLITE_WORKLOAD selects one transaction workload by its result name (such as
// sqlite-wal-normal-batch100), keeping the recovery and snapshot checks. cladefs: the working-set
// group, run only when named, reads 4 KiB at random from files of half, one, two and four times the
// extent daemon's memory (CLADEFSD_MEMORY, as test-e2e.sh gives it), IOPS and latencies once the
// caches settle (TASK-350.2); the sync group, also only when named, writes 4 KiB and then syncs,
// syncfs's or freezes and thaws the filesystem, CLADEFS_BENCH_SYNCS times each (50) (TASK-350.5).
// CLADEFS_BENCH_FILES sets how many files the metadata workloads make (2000), and
// CLADEFS_BENCH_PGBENCH_SECS how long pgbench's transactions run (20), and
// CLADEFS_BENCH_PGBENCH_SCALE the scale of its database (10, about 160 MiB). With
// CLADEFS_BENCH_RESULTS set to a directory, the host also writes each case's measurements there as
// JSON, which scripts/bench-report.py turns into tables comparing the filesystems. With
// CLADEFS_BENCH_PERF set to `record`, perf profiles each case's VMM into perf.data in its directory
// (`record-all`, the whole host, cladefs's extent daemon too), or with `record:EVENTS` records the
// events, such as the scheduler's tracepoints, with stacks; set to `stat:EVENTS`, perf counts the
// events, such as KVM's tracepoints, each 100 ms of the run, into the case's log, where they line
// up with the guest's results. With CLADEFS_BENCH_GUEST_PERF set, perf in the guest samples its
// kernel through each metadata workload, and the case's log says where each one's time went; set to
// `record:EVENTS`, it samples the events instead.
//
// cladefs: the fork cases measure what a VM booted on a fork of a base image pays to start and run,
// on the base's data, and what the extent daemon having that data in memory already saves it. A
// base VM, on a cell of its own, copies the guest's /usr there and makes a PostgreSQL database
// (pgbench -i), once. Then each iteration forks the base through the daemon (its ForkCell RPC)
// and boots a VM on the fork, which mounts it, reads the tree whole, and starts PostgreSQL on the
// database and runs pgbench's read-only transactions. The fork is served by the daemon that served
// the base, whose extent cache holds the base's data (warm), or by a daemon of its own, started
// for it, whose cache holds nothing (cold): CLADEFSD_BIN names the daemon's binary; or, the forks
// those of one version of the base, by a daemon of its own that fetches what the version's read
// evidence, which a fork before the iterations recorded, predicts into its cache as the fork boots
// (predicted, TASK-702.3). The host reports the daemon's counters of
// what serving the fork cost, as observations. cladefs: so too a fork without a DAX window, its
// files read through the guest's page cache; and, against them, a base on ext4 on virtio-pmem,
// with DAX and without, each iteration's VM booted on a copy of the base's image, which the host's
// page cache holds as the extent daemon holds a warm base, as a static DAX device of a base layer
// is (TASK-112.2).

use macros::{guest, host};

/// The filesystem a case benchmarks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// ext4 on a virtio-blk disk, with cladefs's guest kernel, which the guest's Fxfs compares
    /// with.
    Ext4GuestKernel,
    /// cladefs: ext4 on a virtio-blk disk on a loop device over a raw image file, as a VM host's
    /// local disk images are, each flush a host fsync (cache_type Writeback): the baseline cladefs
    /// is measured against where it should lose (TASK-350.1).
    Ext4Loop,
    /// cladefs: as `Ext4Loop`, its flushes dropped (cache_type Unsafe).
    Ext4LoopUnsafe,
    /// ext4 with DAX on writable virtio-pmem, with cladefs's guest kernel: storage as memory.
    Ext4DaxPmem,
    /// ext4 on the same virtio-pmem without DAX, its files in the page cache: what DAX on pmem
    /// costs or saves, against the same device (TASK-91.16).
    Ext4Pmem,
    /// The guest kernel's Fxfs on an extent-store device, with files in the page cache.
    FxfsGuest,
    /// The guest kernel's Fxfs on an extent-store device, mapping files from its DAX window.
    FxfsGuestDax,
    /// cladefs: as `FxfsGuestDax`, its device answering the guest's asks for durability once
    /// ordered (`cache_type` Unsafe, TASK-363), as production's image drives drop their flushes.
    FxfsGuestDaxUnsafe,
    /// cladefs: as `FxfsGuestDax`, booted on a fork of a base image whose data the extent daemon
    /// serving it has in memory already: the daemon that served the base.
    FxfsGuestDaxForkWarm,
    /// cladefs: as `FxfsGuestDaxForkWarm`, but served by a daemon of its own, which has nothing of
    /// the base's in memory.
    FxfsGuestDaxForkCold,
    /// cladefs: as `FxfsGuestDaxForkCold`, the forks of one version of the base, whose read
    /// evidence, which a fork recorded, the daemon fetches ahead as the fork boots (TASK-702.3).
    FxfsGuestDaxForkPredicted,
    /// cladefs: as `FxfsGuestDaxForkWarm`, without a DAX window: the fork's files read through the
    /// guest's page cache, which each VM fills with a copy of its own (TASK-112.2).
    FxfsGuestForkWarm,
    /// cladefs: the fork cases' base on ext4 with DAX on virtio-pmem, each VM booted on a copy of
    /// the base's image, which the host's page cache holds: a static DAX device, as a base layer's
    /// (TASK-112.2).
    Ext4DaxPmemFork,
    /// cladefs: as `Ext4DaxPmemFork`, without DAX, the files in the guest's page cache.
    Ext4PmemFork,
}

#[cfg_attr(feature = "guest", allow(dead_code))]
impl Backend {
    /// Whether the case boots VMs on forks of a base image.
    fn forks(self) -> bool {
        self.forks_cell() || self.forks_image()
    }

    /// Whether the case's forks are forks of a cell, which an extent daemon serves.
    fn forks_cell(self) -> bool {
        matches!(
            self,
            Backend::FxfsGuestDaxForkWarm
                | Backend::FxfsGuestDaxForkCold
                | Backend::FxfsGuestDaxForkPredicted
                | Backend::FxfsGuestForkWarm
        )
    }

    /// cladefs: whether the case's forks are copies of the base's virtio-pmem image.
    fn forks_image(self) -> bool {
        matches!(self, Backend::Ext4DaxPmemFork | Backend::Ext4PmemFork)
    }
}

/// cladefs: what a VM of a fork case is: the base, or a fork (`CLADEFS_FORK_BENCH_ROLE`).
#[cfg_attr(feature = "host", allow(dead_code))]
const ROLE: &str = "CLADEFS_FORK_BENCH_ROLE";

/// cladefs: the cell of a fork case's VM, the base's or a fork's.
#[cfg_attr(feature = "guest", allow(dead_code))]
const CELL_DIR: &str = "CLADEFS_FORK_BENCH_STORE";

pub struct TestFsBench {
    pub backend: Backend,
}

/// The size of the filesystems' images, which are sparse.
#[cfg_attr(feature = "guest", allow(dead_code))]
const IMAGE_SIZE: u64 = 8 << 30;

/// The source tree the build workload untars and builds.
#[cfg_attr(feature = "host", allow(dead_code))]
const SOURCE_TARBALL: &str = "/opt/lua-5.4.7.tar.gz";

#[host]
mod host {
    use super::*;
    use cladefs::cladefsd::control::proto::{ForkCellRequest, GetCountersRequest};

    use crate::common::{cladefs_guest_should_run, setup_rootfs};
    use crate::loop_device::LoopIo;
    use crate::rootfs::{Download, Image};
    use crate::vm::{DiskCache, Vm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::Context as _;
    use std::collections::BTreeMap;
    use std::ffi::CString;

    const CONTAINERFILE: &str = "\
FROM alpine:3.22
RUN apk add --no-cache fio gcc make musl-dev e2fsprogs perf postgresql postgresql-contrib sqlite
COPY lua-5.4.7.tar.gz /opt/lua-5.4.7.tar.gz
";

    /// cladefs: the build workload's source, from the host's cache: lua.org's, or its mirrors',
    /// PUC-Rio's and Buildroot's, where lua.org can't be reached; fetched by the build from
    /// lua.org alone, a timeout there skipped every case (TASK-297).
    const LUA: Download = Download {
        name: "lua-5.4.7.tar.gz",
        sha256: "9fbf5e28ef86c69858f6d3d34eccc32e911c1a28b4120ff3e84aaa70cfbf1e30",
        sources: &[
            "https://www.lua.org/ftp/lua-5.4.7.tar.gz",
            "https://www.tecgraf.puc-rio.br/lua/ftp/lua-5.4.7.tar.gz",
            "https://sources.buildroot.net/lua/lua-5.4.7.tar.gz",
        ],
    };

    fn iterations() -> Option<u32> {
        std::env::var("CLADEFS_BENCH_ITERATIONS").ok()?.parse().ok()
    }

    /// A workload's measurements, one per iteration.
    #[derive(serde::Serialize)]
    struct Measurements {
        workload: String,
        unit: String,
        higher_is_better: bool,
        values: Vec<f64>,
    }

    impl Measurements {
        /// Returns the median, minimum and maximum.
        fn summary(&self) -> (f64, f64, f64) {
            let mut values = self.values.clone();
            values.sort_by(f64::total_cmp);
            let n = values.len();
            let median =
                if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) / 2.0 };
            (median, values[0], values[n - 1])
        }
    }

    #[derive(Default, serde::Serialize)]
    struct BenchReport {
        case: String,
        backend: String,
        host_os: &'static str,
        /// The VMM: Firecracker.
        vmm: &'static str,
        iterations: usize,
        results: Vec<Measurements>,
        /// How many iterations of each workload failed, which have no measurement.
        failures: BTreeMap<String, u32>,
        /// What the guest observed besides the workloads' measurements, per iteration.
        observations: BTreeMap<String, Vec<f64>>,
    }

    /// Parses the guest's `RESULT <workload> <unit> <higher|lower> <value>`, `FAILED <workload>`
    /// and `OBSERVED <name> <value>` lines.
    fn parse(stdout: &str, report: &mut BenchReport) -> anyhow::Result<()> {
        let results = &mut report.results;
        let mut index = BTreeMap::new();
        for line in stdout.lines() {
            if let Some(workload) = line.strip_prefix("FAILED ") {
                *report.failures.entry(workload.to_string()).or_default() += 1;
                continue;
            }
            if let Some(observed) = line.strip_prefix("OBSERVED ") {
                let Some((name, value)) = observed.split_once(' ') else {
                    anyhow::bail!("malformed observation {line:?}");
                };
                report.observations.entry(name.to_string()).or_default().push(value.parse()?);
                continue;
            }
            let Some(result) = line.strip_prefix("RESULT ") else {
                continue;
            };
            let fields: Vec<&str> = result.split(' ').collect();
            let [workload, unit, better, value] = fields[..] else {
                anyhow::bail!("malformed result {line:?}");
            };
            let value: f64 = value.parse()?;
            let i = *index.entry(workload.to_string()).or_insert_with(|| {
                results.push(Measurements {
                    workload: workload.to_string(),
                    unit: unit.to_string(),
                    higher_is_better: better == "higher",
                    values: Vec::new(),
                });
                results.len() - 1
            });
            results[i].values.push(value);
        }
        anyhow::ensure!(!results.is_empty(), "no results");
        report.iterations = results.iter().map(|m| m.values.len()).max().unwrap_or(0);
        Ok(())
    }

    impl crate::ReportImpl for BenchReport {
        fn fmt_text(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let i = f.width().unwrap_or(0);
            writeln!(f, "{:i$}{} ({} iterations)\n", "", self.backend, self.iterations)?;
            writeln!(
                f,
                "{:i$}{:<22} {:>12} {:>12} {:>12}  unit",
                "", "workload", "median", "min", "max"
            )?;
            for m in &self.results {
                let (median, min, max) = m.summary();
                writeln!(
                    f,
                    "{:i$}{:<22} {:>12.1} {:>12.1} {:>12.1}  {}",
                    "", m.workload, median, min, max, m.unit
                )?;
            }
            for (workload, count) in &self.failures {
                writeln!(f, "{:i$}{workload} failed {count} times", "")?;
            }
            for (name, values) in &self.observations {
                writeln!(f, "{:i$}{name}: {values:?}", "")?;
            }
            Ok(())
        }

        fn fmt_gh_markdown(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            writeln!(f, "### {} ({} iterations)\n", self.backend, self.iterations)?;
            writeln!(f, "| Workload | Median | Min | Max | Unit |")?;
            write!(f, "|---|---|---|---|---|")?;
            for m in &self.results {
                let (median, min, max) = m.summary();
                write!(f, "\n| {} | {median:.1} | {min:.1} | {max:.1} | {} |", m.workload, m.unit)?;
            }
            for (workload, count) in &self.failures {
                write!(f, "\n\n{workload} failed {count} times.")?;
            }
            Ok(())
        }
    }

    /// cladefs: the fork cases' VMs, each run as a process of its own: this program again, as the
    /// role it is given.
    mod forks {
        use super::*;
        use std::path::{Path, PathBuf};
        use std::process::{Child, Command};
        use std::time::{Duration, Instant};

        /// Runs the base's VM, then, each iteration, forks the base and runs a VM on the fork, and
        /// reports what the daemon serving the fork counted.
        pub fn run(backend: Backend, test_setup: &TestSetup) -> anyhow::Result<()> {
            if backend.forks_image() {
                return run_images(test_setup);
            }
            let cladefsd = PathBuf::from(std::env::var_os("CLADEFSD").context("CLADEFSD")?);
            let base = crate::common::create_cell(&cladefsd, IMAGE_SIZE)?;
            let base_spec = base.to_string();
            role(test_setup, "base", Path::new(&base_spec), &cladefsd, None)?;
            let iterations = iterations().unwrap_or(1);
            // The version the forks are of, if they are of one (predicted).
            let version = match backend {
                Backend::FxfsGuestDaxForkPredicted => {
                    Some(opened(&cladefsd, base)?.make_version()?)
                }
                _ => None,
            };
            let fork_on = |target_socket: &Path| -> anyhow::Result<cladefs::CellId> {
                match version {
                    Some(version) => crate::common::control(target_socket)?
                        .call(async |client| {
                            let cladefs::CellVersion { tenant, cell, version } = version;
                            client
                                .fork_cell(ForkCellRequest {
                                    tenant,
                                    cell,
                                    version,
                                    ..Default::default()
                                })
                                .await
                        })
                        .map(|response| cladefs::CellId {
                            tenant: response.tenant,
                            id: response.cell,
                        })
                        .context("forking the base version"),
                    None => crate::common::control(target_socket)?
                        .call(async |client| {
                            client
                                .fork_cell(ForkCellRequest {
                                    tenant: base.tenant,
                                    cell: base.id,
                                    ..Default::default()
                                })
                                .await
                        })
                        .map(|response| cladefs::CellId {
                            tenant: response.tenant,
                            id: response.cell,
                        })
                        .context("forking the base"),
                }
            };
            if backend == Backend::FxfsGuestDaxForkPredicted {
                // A fork, before the iterations, whose boot is the version's evidence: its output
                // is not the case's.
                let sockets = tempfile::Builder::new().prefix("cladefs-fork").tempdir()?;
                let (socket, mut daemon) = daemon(test_setup, sockets.path(), iterations)?;
                let fork = fork_on(&socket)?;
                let fork_spec = fork.to_string();
                let output = std::fs::File::create(test_setup.tmp_dir.join("recorded-fork.txt"))?;
                role(test_setup, "fork", Path::new(&fork_spec), &socket, Some(output))?;
                // Written as the daemon closes the fork's cell, once it has seen its VM go.
                drop(opened(&socket, fork)?);
                daemon.kill()?;
                daemon.wait()?;
            }
            for i in 0..iterations {
                // A socket's path is short: the daemon's directory is a temporary one of its own.
                let sockets = tempfile::Builder::new().prefix("cladefs-fork").tempdir()?;
                let (socket, daemon) = match backend {
                    Backend::FxfsGuestDaxForkCold | Backend::FxfsGuestDaxForkPredicted => {
                        let (socket, daemon) = daemon(test_setup, sockets.path(), i)?;
                        (socket, Some(daemon))
                    }
                    _ => (cladefsd.clone(), None),
                };
                let fork = if matches!(backend, Backend::FxfsGuestDaxForkCold) {
                    // Cold daemon has its own disk pool; make a version on base's daemon if needed
                    // or fork the base cell from the bucket via version.
                    let ver = opened(&cladefsd, base)?.make_version()?;
                    crate::common::control(&socket)?
                        .call(async |client| {
                            client
                                .fork_cell(ForkCellRequest {
                                    tenant: ver.tenant,
                                    cell: ver.cell,
                                    version: ver.version,
                                    ..Default::default()
                                })
                                .await
                        })
                        .map(|r| cladefs::CellId { tenant: r.tenant, id: r.cell })
                        .context("forking the base on cold daemon")?
                } else {
                    fork_on(&socket)?
                };
                let fork_spec = fork.to_string();
                let daemon_stats = crate::common::control(&socket)?;
                let counters = || {
                    daemon_stats
                        .call(async |client| client.get_counters(GetCountersRequest {}).await)
                };
                let before = counters()?;
                role(test_setup, "fork", Path::new(&fork_spec), &socket, None)?;
                let after = counters()?;
                macro_rules! report {
                    ($($field:ident),+ $(,)?) => {
                        $(
                            let name = stringify!($field).replace('_', "-");
                            let value = after.$field.saturating_sub(before.$field);
                            println!("OBSERVED cladefsd-{name} {value}");
                        )+
                    };
                }
                report!(
                    maps,
                    map_ns,
                    faults,
                    fault_ns,
                    page_ins,
                    page_in_bytes,
                    fetched_bytes,
                    reads,
                    read_bytes,
                    read_stored_bytes,
                    prefetches,
                    unpacks,
                    commits,
                    commit_ns,
                    commit_store_ns,
                    commit_wait_ns,
                    segment_closes,
                    segment_close_ns,
                );
                if let Some(mut daemon) = daemon {
                    daemon.kill()?;
                    daemon.wait()?;
                }
            }
            println!("DONE");
            Ok(())
        }

        /// cladefs: runs the base's VM on an image of its own, then, each iteration, a VM on a copy
        /// of the image, made as it boots, which the host's page cache holds then (TASK-112.2).
        fn run_images(test_setup: &TestSetup) -> anyhow::Result<()> {
            // Under `extents`, which what keeps a run's results leaves out (bench-linux-host.sh).
            let images = test_setup.tmp_dir.join("extents");
            std::fs::create_dir_all(&images)?;
            let base = images.join("base.img");
            std::fs::File::create(&base)?.set_len(IMAGE_SIZE)?;
            role(test_setup, "base", &base, Path::new(""), None)?;
            for i in 0..iterations().unwrap_or(1) {
                let fork = images.join(format!("fork-{i}.img"));
                crate::common::copy_sparse(&base, &fork)?;
                role(test_setup, "fork", &fork, Path::new(""), None)?;
                std::fs::remove_file(&fork)?;
            }
            println!("DONE");
            Ok(())
        }

        /// Runs this program again, as `role`'s VM, on the cell at `cell_dir`, which the daemon at
        /// `cladefsd` serves, and waits for it: its output is this one's, or goes to `output`.
        fn role(
            test_setup: &TestSetup,
            role: &str,
            cell_dir: &Path,
            cladefsd: &Path,
            output: Option<std::fs::File>,
        ) -> anyhow::Result<()> {
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args(std::env::args_os().skip(1))
                .env(ROLE, role)
                .env(CELL_DIR, cell_dir)
                .env("CLADEFSD", cladefsd);
            if let Some(output) = output {
                command.stdout(output);
            }
            let mut child = command.spawn()?;
            // The runner's timeout ends it, and its VM with it, which later cases would run
            // beside (TASK-79.61).
            test_setup.register_cleanup_pid(child.id());
            let status = child.wait()?;
            anyhow::ensure!(
                status.success(),
                "the {role}'s VM in {}: {status}",
                test_setup.tmp_dir.display()
            );
            Ok(())
        }

        /// `cell`, which the daemon at `cladefsd` serves: once the daemon has seen
        /// the VM that had it open go.
        fn opened(cladefsd: &Path, cell: cladefs::CellId) -> anyhow::Result<cladefs::CellClient> {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match cladefs::CellClient::connect_cell(cladefsd, cell) {
                    Ok(client) => return Ok(client),
                    Err(err)
                        if Instant::now() < deadline
                            && err.errno() == Some(cladefs::Errno::EBUSY) =>
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(err) => return Err(anyhow::anyhow!("{err:#}")),
                }
            }
        }

        /// A daemon of its own for iteration `i`'s fork, and its socket in `dir`, once it serves
        /// there.
        /// A daemon of the fork's own, on the run's bucket (CLADEFS_E2E_BUCKET), where the version
        /// it forks is: on a bucket of its own it found none, and waited for ever on a bucket its
        /// root doesn't record (board 14542).
        fn daemon(test_setup: &TestSetup, dir: &Path, i: u32) -> anyhow::Result<(PathBuf, Child)> {
            let socket = dir.join("cladefsd.sock");
            let bucket = std::env::var_os("CLADEFS_E2E_BUCKET")
                .context("CLADEFS_E2E_BUCKET, the run's bucket (scripts/test-e2e.sh)")?;
            let daemon = crate::common::spawn_daemon(
                test_setup,
                &socket,
                &format!("cladefsd-{i}"),
                &[("CLADEFSD_BUCKET", &bucket)],
                None,
            )?;
            Ok((socket, daemon))
        }
    }

    /// cladefs: the vCPUs of a case's VM: CLADEFS_BENCH_VCPUS, or 2 (TASK-276).
    fn vcpus() -> anyhow::Result<u8> {
        match std::env::var("CLADEFS_BENCH_VCPUS") {
            Ok(vcpus) => vcpus.parse().context("CLADEFS_BENCH_VCPUS is a count of vCPUs"),
            Err(_) => Ok(2),
        }
    }

    /// Sets what the guest's workloads take from the host's environment in `vm`'s: the iterations,
    /// the groups, the workloads' sizes, and the paths the guest's programs need.
    fn workloads_env(vm: &mut Vm) -> anyhow::Result<()> {
        vm.env(&CString::new(format!("CLADEFS_BENCH_ITERATIONS={}", iterations().unwrap_or(1)))?);
        for var in ["CLADEFS_BENCH_GROUPS", "CLADEFS_BENCH_GUEST_PERF"] {
            vm.env(&CString::new(format!("{var}={}", std::env::var(var).unwrap_or_default()))?);
        }
        for var in [
            "CLADEFS_BENCH_FILES",
            "CLADEFS_BENCH_SQLITE_TXNS",
            "CLADEFS_BENCH_SQLITE_WORKLOAD",
            "CLADEFS_BENCH_PGBENCH_SECS",
            "CLADEFS_BENCH_PGBENCH_SCALE",
            "CLADEFS_BENCH_WATCHDOG_SECS",
            "CLADEFS_BENCH_JOBS",
            "CLADEFS_BENCH_SYNCS",
        ] {
            if let Ok(value) = std::env::var(var) {
                vm.env(&CString::new(format!("{var}={value}"))?);
            }
        }
        // cladefs: what the working-set workloads size their files by (TASK-350.2): the extent
        // daemon's fixed memory, CLADEFSD_MEMORY, if it has one, or 1 GiB, its old default: a
        // governed daemon, as production's (board 14529), has no size of its own, its memory a
        // page cache of what the host's is free (TASK-358), so that a governed run's sets are of
        // 1 GiB's multiples, the same in every run, its 2x and 4x past a fixed 1 GiB daemon's but
        // not past a governed one on a large host. Every case gets it, the baselines too, so that
        // each reads files of the same sizes.
        let daemon_memory =
            std::env::var("CLADEFSD_MEMORY").unwrap_or_else(|_| (1u64 << 30).to_string());
        vm.env(&CString::new(format!("CLADEFS_BENCH_DAEMON_MEMORY={daemon_memory}"))?);
        // The compiler finds the assembler and linker through PATH.
        vm.env(c"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        vm.env(c"HOME=/root");
        Ok(())
    }

    impl TestFsBench {
        fn name(&self) -> &'static str {
            match self.backend {
                Backend::Ext4GuestKernel => "ext4-guest-kernel",
                Backend::Ext4Loop => "ext4-loop",
                Backend::Ext4LoopUnsafe => "ext4-loop-unsafe",
                Backend::Ext4DaxPmem => "ext4-dax-pmem",
                Backend::Ext4Pmem => "ext4-pmem",
                Backend::FxfsGuest => "fxfs-guest",
                Backend::FxfsGuestDax => "fxfs-guest-dax",
                Backend::FxfsGuestDaxUnsafe => "fxfs-guest-dax-unsafe",
                Backend::FxfsGuestDaxForkWarm => "fxfs-guest-dax-fork-warm",
                Backend::FxfsGuestDaxForkCold => "fxfs-guest-dax-fork-cold",
                Backend::FxfsGuestDaxForkPredicted => "fxfs-guest-dax-fork-predicted",
                Backend::FxfsGuestForkWarm => "fxfs-guest-fork-warm",
                Backend::Ext4DaxPmemFork => "ext4-dax-pmem-fork",
                Backend::Ext4PmemFork => "ext4-pmem-fork",
            }
        }
    }

    impl Test for TestFsBench {
        fn linux_only(&self) -> Option<&'static str> {
            matches!(self.backend, Backend::Ext4Loop | Backend::Ext4LoopUnsafe)
                .then_some(crate::LINUX_ONLY_LOOP)
        }

        fn should_run(&self) -> ShouldRun {
            if iterations().is_none() {
                return ShouldRun::No("CLADEFS_BENCH_ITERATIONS not set");
            }
            if std::env::var_os("CLADEFS_GUEST_KERNEL").is_none() {
                return ShouldRun::No("CLADEFS_GUEST_KERNEL does not name a guest kernel");
            }
            if !crate::vm::has_vmm() {
                return ShouldRun::No("CLADEFS_FIRECRACKER does not name cladefs's Firecracker");
            }
            if matches!(
                self.backend,
                Backend::FxfsGuestDaxForkCold | Backend::FxfsGuestDaxForkPredicted
            ) && std::env::var_os("CLADEFSD_BIN").is_none()
            {
                return ShouldRun::No("CLADEFSD_BIN does not name the extent daemon's binary");
            }
            match self.backend {
                Backend::FxfsGuest
                | Backend::FxfsGuestDax
                | Backend::FxfsGuestDaxUnsafe
                | Backend::FxfsGuestDaxForkWarm
                | Backend::FxfsGuestDaxForkCold
                | Backend::FxfsGuestDaxForkPredicted
                | Backend::FxfsGuestForkWarm => cladefs_guest_should_run(),
                Backend::Ext4GuestKernel
                | Backend::Ext4Loop
                | Backend::Ext4LoopUnsafe
                | Backend::Ext4DaxPmem
                | Backend::Ext4Pmem
                | Backend::Ext4DaxPmemFork
                | Backend::Ext4PmemFork => ShouldRun::Yes,
            }
        }

        fn rootfs_image(&self) -> Option<Image> {
            Some(Image { containerfile: CONTAINERFILE, downloads: &[LUA], files: &[] })
        }

        fn timeout_secs(&self) -> u64 {
            1800 * u64::from(iterations().unwrap_or(1))
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            // A fork case runs its VMs, each a process of its own, from here.
            if self.backend.forks() && std::env::var_os(ROLE).is_none() {
                return forks::run(self.backend, &test_setup);
            }
            // With CLADEFS_BENCH_PERF set, this process, which becomes the VMM, runs again under
            // perf, which samples all its threads, its
            // vCPUs', its devices' and cladefs's, or counts events.
            let perf = std::env::var("CLADEFS_BENCH_PERF").unwrap_or_default();
            if !perf.is_empty() && std::env::var_os("CLADEFS_BENCH_PERF_RUNNING").is_none() {
                use std::os::unix::process::CommandExt;
                let mut command = std::process::Command::new("perf");
                match perf.strip_prefix("stat:") {
                    Some(events) => command.args(["stat", "-I", "100", "-e", events]),
                    None => {
                        let record = match perf.strip_prefix("record:") {
                            Some(events) => vec!["record", "-g", "-e", events],
                            None if perf == "record" => vec!["record", "-g", "-F", "999"],
                            // The whole host, cladefs's extent daemon included.
                            None if perf == "record-all" => {
                                vec!["record", "-a", "-g", "-F", "999"]
                            }
                            None => anyhow::bail!(
                                "CLADEFS_BENCH_PERF is record, record-all, record:EVENTS or \
                                 stat:EVENTS"
                            ),
                        };
                        command.args(record).arg("-o").arg(test_setup.tmp_dir.join("perf.data"))
                    }
                };
                let error = command
                    .arg("--")
                    .arg(std::env::current_exe()?)
                    .args(std::env::args_os().skip(1))
                    .env("CLADEFS_BENCH_PERF_RUNNING", "1")
                    .exec();
                return Err(error.into());
            }
            let root_dir = setup_rootfs(&test_setup)?;

            let mut vm = Vm::new(vcpus()?, 2048)?;
            vm.guest_kernel()?;
            // The filesystem's device in the guest, for those on one.
            let device = match self.backend {
                Backend::FxfsGuest | Backend::FxfsGuestDax | Backend::FxfsGuestDaxUnsafe => {
                    // A new cell, which the guest's Fxfs formats as it mounts it.
                    let window = if self.backend == Backend::FxfsGuest {
                        0
                    } else {
                        crate::common::dax_window(crate::common::Window::Large)?
                    };
                    vm.new_cell("bench", IMAGE_SIZE, window)?;
                    if self.backend == Backend::FxfsGuestDaxUnsafe {
                        vm.cell_unsafe();
                    }
                    None
                }
                // The base's cell, new, or a fork of it.
                Backend::FxfsGuestDaxForkWarm
                | Backend::FxfsGuestDaxForkCold
                | Backend::FxfsGuestDaxForkPredicted
                | Backend::FxfsGuestForkWarm => {
                    let cell_dir = std::env::var_os(CELL_DIR).context("no cell for a fork case")?;
                    let window = if self.backend == Backend::FxfsGuestForkWarm {
                        0
                    } else {
                        crate::common::dax_window(crate::common::Window::Large)?
                    };
                    vm.cell("bench", std::path::Path::new(&cell_dir), IMAGE_SIZE, window)?;
                    vm.env(&CString::new(format!("{ROLE}={}", std::env::var(ROLE)?))?);
                    None
                }
                // cladefs: the base's image, or a copy of it (TASK-112.2).
                Backend::Ext4DaxPmemFork | Backend::Ext4PmemFork => {
                    let image = std::env::var_os(CELL_DIR).context("no image for a fork case")?;
                    vm.env(&CString::new(format!("{ROLE}={}", std::env::var(ROLE)?))?);
                    Some(vm.pmem("bench", std::path::Path::new(&image))?)
                }
                Backend::Ext4DaxPmem | Backend::Ext4Pmem => {
                    let image = test_setup.tmp_dir.join("pmem.img");
                    std::fs::File::create(&image)?.set_len(IMAGE_SIZE)?;
                    Some(vm.pmem("bench", &image)?)
                }
                Backend::Ext4GuestKernel => {
                    let image = test_setup.tmp_dir.join("ext4.img");
                    std::fs::File::create(&image)?.set_len(IMAGE_SIZE)?;
                    Some(vm.disk("bench", &image)?)
                }
                Backend::Ext4Loop | Backend::Ext4LoopUnsafe => {
                    let image = test_setup.tmp_dir.join("ext4.img");
                    std::fs::File::create(&image)?.set_len(IMAGE_SIZE)?;
                    let cache = if self.backend == Backend::Ext4Loop {
                        DiskCache::Writeback
                    } else {
                        DiskCache::Unsafe
                    };
                    Some(vm.loop_disk("bench", &image, cache, LoopIo::Buffered)?)
                }
            };
            if let Some(device) = device {
                vm.env(&CString::new(format!("CLADEFS_BENCH_DEVICE={device}"))?);
            }
            workloads_env(&mut vm)?;
            vm.enter(&root_dir, &test_setup.test_case)
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            // cladefs: the loop cases' image goes, whatever the outcome, its loop device gone with
            // the VM (TASK-350.1).
            if matches!(self.backend, Backend::Ext4Loop | Backend::Ext4LoopUnsafe)
                && let Err(err) = std::fs::remove_file(test_setup.tmp_dir.join("ext4.img"))
            {
                return TestOutcome::Fail(format!("removing the loop device's image: {err}"));
            }
            let stdout = String::from_utf8_lossy(&stdout);
            if !stdout.lines().any(|line| line == "DONE") {
                return TestOutcome::Fail(format!("the guest did not finish: {stdout}"));
            }
            let mut report = BenchReport {
                case: test_setup.test_case.clone(),
                backend: self.name().to_string(),
                host_os: std::env::consts::OS,
                vmm: "firecracker",
                ..BenchReport::default()
            };
            if let Err(err) = parse(&stdout, &mut report) {
                return TestOutcome::Fail(format!("{err}: {stdout}"));
            }
            if let Some(dir) = std::env::var_os("CLADEFS_BENCH_RESULTS") {
                let path = std::path::Path::new(&dir).join(format!("{}.json", report.case));
                let json = serde_json::to_string_pretty(&report).unwrap();
                if let Err(err) = std::fs::write(&path, json) {
                    return TestOutcome::Fail(format!("writing {}: {err}", path.display()));
                }
            }
            TestOutcome::Report(Box::new(report))
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use nix::libc;
    use std::ffi::CString;
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::time::Instant;

    const MNT: &str = "/mnt/bench";
    /// The file the I/O workloads use.
    const DATA: &str = "/mnt/bench/data";
    /// Small enough for cladefs's extent daemon, of 1 GiB by default, to cache whole.
    const DATA_SIZE: u64 = 256 << 20;
    /// What the random I/O workloads that are not timed read or write, and the timed ones' time.
    /// What the random writes write: 16384 writes, which take seconds.
    const RANDOM_SIZE: u64 = 64 << 20;
    /// What the fsynced appends write: 2048 appends of 4 KiB, which take a second or more where an
    /// fsync takes half a millisecond or more, so that fio's runtime, in milliseconds, measures
    /// them finely.
    const APPENDED_SIZE: u64 = 8 << 20;
    const RUNTIME: &str = "--runtime=2";
    const MIB: f64 = (1 << 20) as f64;
    /// The number of files the metadata workloads create, stat, list and remove:
    /// CLADEFS_BENCH_FILES, or 2000, whose workloads take milliseconds.
    fn files() -> usize {
        std::env::var("CLADEFS_BENCH_FILES").ok().and_then(|n| n.parse().ok()).unwrap_or(2_000)
    }
    /// The files created and fsynced one at a time.
    const FSYNCED: usize = 200;
    /// The tree's directories, and the files in each.
    const TREE_DIRS: usize = 50;
    const TREE_FILES: usize = 40;
    /// The files in the large directory.
    const LARGE_DIR: usize = 20_000;
    /// The threads of the concurrent workloads, each making `files()` files.
    const THREADS: usize = 4;

    fn run(program: &str, args: &[&str]) {
        let status = Command::new(program)
            .args(args)
            .status()
            .unwrap_or_else(|err| panic!("{program}: {err}"));
        assert!(status.success(), "{program} {args:?}: {status}");
    }

    fn mount(source: &str, fstype: &str, options: &str) {
        fs::create_dir_all(MNT).unwrap();
        let source = CString::new(source).unwrap();
        let target = CString::new(MNT).unwrap();
        let fstype = CString::new(fstype).unwrap();
        let options = CString::new(options).unwrap();
        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                options.as_ptr().cast(),
            )
        };
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    /// The device of the filesystem the host made, which it names.
    fn bench_device() -> String {
        std::env::var("CLADEFS_BENCH_DEVICE").expect("CLADEFS_BENCH_DEVICE names the device")
    }

    /// Writes back and drops the guest's caches, so that the next reads miss them.
    fn drop_caches() {
        unsafe { libc::sync() };
        fs::write("/proc/sys/vm/drop_caches", b"3").unwrap();
    }

    /// When the last result was reported, in seconds since the epoch: what the watchdog watches.
    static PROGRESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// Watches for a workload making no progress for CLADEFS_BENCH_WATCHDOG_SECS (180): then the
    /// kernel's blocked tasks' stacks (sysrq w), and its log, go to standard error, once, so that
    /// a hang says where.
    fn start_watchdog() {
        let limit: u64 = std::env::var("CLADEFS_BENCH_WATCHDOG_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        PROGRESS.store(now_secs(), std::sync::atomic::Ordering::Relaxed);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let last = PROGRESS.load(std::sync::atomic::Ordering::Relaxed);
                if now_secs().saturating_sub(last) >= limit {
                    eprintln!("WATCHDOG: no progress for {limit} s: blocked tasks follow");
                    let _ = fs::write("/proc/sysrq-trigger", b"w");
                    crate::kernel_log::print();
                    return;
                }
            }
        });
    }

    fn result(workload: &str, unit: &str, higher_is_better: bool, value: f64) {
        PROGRESS.store(now_secs(), std::sync::atomic::Ordering::Relaxed);
        let better = if higher_is_better { "higher" } else { "lower" };
        println!("RESULT {workload} {unit} {better} {value:.3}");
        std::io::stdout().flush().unwrap();
    }

    /// What fio measured of a job: its reads, and its writes.
    struct Fio {
        read: Rates,
        write: Rates,
    }

    /// A job's operations and bytes per second, in one direction, over fio's runtime of it: from
    /// the job's first I/O to its last, a write job's fsyncs among them, its final one
    /// (`--end_fsync`) too. Not fio's starting up, laying out the file and exiting, on a 250 ms
    /// tick, which a clock around fio would take in (TASK-92.11).
    struct Rates {
        iops: f64,
        bytes_per_sec: f64,
        /// cladefs: the median and 99th percentile of its operations' completion latencies, in
        /// microseconds, where it had any (TASK-350.2).
        p50_us: Option<f64>,
        p99_us: Option<f64>,
    }

    impl Rates {
        /// The rates of fio's JSON of a job's direction (`jobs[].read`, `jobs[].write`).
        fn of(direction: &serde_json::Value) -> Self {
            let percentile =
                |p: &str| direction["clat_ns"]["percentile"][p].as_f64().map(|ns| ns / 1000.0);
            Self {
                iops: direction["iops"].as_f64().expect("fio's iops"),
                bytes_per_sec: direction["bw_bytes"].as_f64().expect("fio's bw_bytes"),
                p50_us: percentile("50.000000"),
                p99_us: percentile("99.000000"),
            }
        }
    }

    /// Runs fio on the data file.
    fn fio(args: &[&str]) -> Fio {
        fio_on(DATA, DATA_SIZE, args)
    }

    /// Runs fio on `file`, of `size` bytes.
    fn fio_on(file: &str, size: u64, args: &[&str]) -> Fio {
        let output = "/tmp/fio.json";
        let size = format!("--size={size}");
        let mut all = vec![
            "--name=bench",
            "--filename",
            file,
            &size,
            // fio would drop the file's cached pages before each job: drop_caches makes the cold
            // jobs cold.
            "--invalidate=0",
            "--output-format=json",
            "--output",
            output,
        ];
        all.extend_from_slice(args);
        run("fio", &all);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(output).unwrap()).expect("fio's JSON");
        let job = &json["jobs"][0];
        Fio { read: Rates::of(&job["read"]), write: Rates::of(&job["write"]) }
    }

    /// Each reports fio's own rates (`Rates`): a write's include the fsyncs that make it durable.
    fn io_workloads() {
        let blocks = (DATA_SIZE / 4096) as usize;
        let random_blocks = (RANDOM_SIZE / 4096) as usize;
        let job =
            observed("seq-write", blocks, || fio(&["--rw=write", "--bs=1M", "--end_fsync=1"]));
        result("seq-write", "MiB/s", true, job.write.bytes_per_sec / MIB);

        let random = format!("--io_size={RANDOM_SIZE}");
        drop_caches();
        let job = observed("rand-read-cold", random_blocks, || {
            fio(&["--rw=randread", "--bs=4k", &random])
        });
        result("rand-read-cold", "IOPS", true, job.read.iops);
        drop_caches();
        let job = fio(&["--rw=read", "--bs=1M"]);
        result("seq-read-cold", "MiB/s", true, job.read.bytes_per_sec / MIB);
        // A warm read of the file takes milliseconds, which fio's runtimes would quantize, so
        // it reads for a while.
        let job = fio(&["--rw=read", "--bs=1M", "--time_based", RUNTIME]);
        result("seq-read-warm", "MiB/s", true, job.read.bytes_per_sec / MIB);
        let job = fio(&["--rw=randread", "--bs=4k", "--time_based", RUNTIME]);
        result("rand-read-warm", "IOPS", true, job.read.iops);
        // cladefs: the file read once through a new mapping, which faults each part in
        // (TASK-26.18.8): before the random writes, which would leave it in small extents.
        let faults = page_faults();
        let job = fio(&["--ioengine=mmap", "--rw=read", "--bs=1M"]);
        result("mmap-seq-read", "MiB/s", true, job.read.bytes_per_sec / MIB);
        faults_per_mib("mmap-seq-read", page_faults() - faults, DATA_SIZE);

        let job = observed("rand-write", random_blocks, || {
            fio(&["--rw=randwrite", "--bs=4k", &random, "--end_fsync=1"])
        });
        result("rand-write", "IOPS", true, job.write.iops);

        let job = fio(&["--ioengine=mmap", "--rw=randread", "--bs=4k", "--time_based", RUNTIME]);
        result("mmap-rand-read", "IOPS", true, job.read.iops);
        let job = observed("mmap-rand-write", random_blocks, || {
            fio(&["--ioengine=mmap", "--rw=randwrite", "--bs=4k", &random, "--end_fsync=1"])
        });
        result("mmap-rand-write", "IOPS", true, job.write.iops);
        fs::remove_file(DATA).unwrap();

        // A log: each 4 KiB append is fsynced.
        let log = format!("{MNT}/log");
        let job = observed("fsync-append", (APPENDED_SIZE / 4096) as usize, || {
            fio_on(&log, APPENDED_SIZE, &["--rw=write", "--bs=4k", "--fsync=1"])
        });
        result("fsync-append", "IOPS", true, job.write.iops);
        fs::remove_file(&log).unwrap();
    }

    /// cladefs: the working sets' sizes, as multiples of the extent daemon's memory (TASK-350.2):
    /// half of it, all of it, and two and four times it.
    const WORKING_SETS: [(&str, f64); 4] = [("0.5x", 0.5), ("1x", 1.0), ("2x", 2.0), ("4x", 4.0)];
    /// cladefs: how long each working set's random reads run, once they have run as long again to
    /// settle (fio's ramp): what they reach once the caches hold what they can of the set.
    const WORKING_SET_RUNTIME: &str = "--runtime=10";
    const WORKING_SET_RAMP: &str = "--ramp_time=10";

    /// cladefs: random 4 KiB reads over working sets sized by the extent daemon's memory, which is
    /// a DAX guest's only cache of its files, and whose misses read and decompress a whole part
    /// each (TASK-350.2). Each set is a file written whole and synced, the guest's caches then
    /// dropped; its reads, uniform over the file, run until the caches settle, then are measured:
    /// IOPS and their latencies' median and 99th percentile.
    fn working_set_workloads() {
        let memory: u64 = std::env::var("CLADEFS_BENCH_DAEMON_MEMORY")
            .expect("CLADEFS_BENCH_DAEMON_MEMORY: the host sets it")
            .parse()
            .expect("CLADEFS_BENCH_DAEMON_MEMORY is a count of bytes");
        let file = format!("{MNT}/working-set");
        for (name, multiple) in WORKING_SETS {
            let size = ((memory as f64 * multiple) as u64).next_multiple_of(1 << 20);
            fio_on(&file, size, &["--rw=write", "--bs=1M", "--end_fsync=1"]);
            drop_caches();
            let job = fio_on(
                &file,
                size,
                &[
                    "--rw=randread",
                    "--bs=4k",
                    "--time_based",
                    WORKING_SET_RAMP,
                    WORKING_SET_RUNTIME,
                ],
            );
            let workload = format!("ws-{name}-rand-read");
            result(&workload, "IOPS", true, job.read.iops);
            let p50 = job.read.p50_us.expect("fio's read latencies");
            let p99 = job.read.p99_us.expect("fio's read latencies");
            result(&format!("{workload}-p50"), "us", false, p50);
            result(&format!("{workload}-p99"), "us", false, p99);
            println!("OBSERVED {workload}-bytes {size}");
            fs::remove_file(&file).unwrap();
        }
    }

    /// cladefs: how many times the sync workloads write and sync: CLADEFS_BENCH_SYNCS, or 50.
    fn syncs() -> u64 {
        std::env::var("CLADEFS_BENCH_SYNCS").ok().and_then(|n| n.parse().ok()).unwrap_or(50)
    }

    /// cladefs: a 4 KiB write, then the whole filesystem made durable, again and again, three ways
    /// (TASK-350.5): sync(2), syncfs(2) on the file, and a freeze and thaw of the filesystem
    /// (FIFREEZE, FITHAW), as a snapshot's tooling does. cladefs's wait for its bucket, where ext4
    /// on a disk whose flushes the host drops waits for nothing past the guest. Each reports its
    /// rounds per second.
    fn sync_workloads() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::FileExt;
        // linux/fs.h's _IOWR('X', 119, int) and _IOWR('X', 120, int).
        const FIFREEZE: libc::c_ulong = 0xC004_5877;
        const FITHAW: libc::c_ulong = 0xC004_5878;
        let path = format!("{MNT}/synced");
        let file =
            fs::OpenOptions::new().create(true).truncate(true).write(true).open(&path).unwrap();
        let mount = fs::File::open(MNT).unwrap();
        let block = [0x5au8; 4096];
        let syncs = syncs();
        {
            let ways: [(&str, &dyn Fn()); 3] = [
                ("sync", &|| unsafe { libc::sync() }),
                ("syncfs", &|| {
                    // SAFETY: a plain system call on a file this function holds open.
                    let ret = unsafe { libc::syncfs(file.as_raw_fd()) };
                    assert_eq!(ret, 0, "syncfs: {}", std::io::Error::last_os_error());
                }),
                ("freeze", &|| {
                    let fd = mount.as_raw_fd();
                    // SAFETY: ioctls without arguments on the mount's root this function holds
                    // open.
                    let frozen = unsafe { libc::ioctl(fd, FIFREEZE as _, 0) };
                    assert_eq!(frozen, 0, "FIFREEZE: {}", std::io::Error::last_os_error());
                    let thawed = unsafe { libc::ioctl(fd, FITHAW as _, 0) };
                    assert_eq!(thawed, 0, "FITHAW: {}", std::io::Error::last_os_error());
                }),
            ];
            // Observed, as the metadata workloads are: the device's requests per round, and
            // where the guest's time went, with CLADEFS_BENCH_GUEST_PERF set.
            for (name, sync) in ways {
                let workload = format!("{name}-loop");
                let secs = observed(&workload, syncs as usize, || {
                    let start = Instant::now();
                    for i in 0..syncs {
                        file.write_all_at(&block, i * 4096).unwrap();
                        sync();
                    }
                    start.elapsed().as_secs_f64()
                });
                result(&workload, "ops/s", true, syncs as f64 / secs);
            }
        }
        drop(file);
        fs::remove_file(&path).unwrap();
    }

    /// cladefs: the page faults the guest took so far (`/proc/vmstat`'s pgfault).
    fn page_faults() -> u64 {
        let vmstat = fs::read_to_string("/proc/vmstat").unwrap();
        let line = vmstat.lines().find_map(|line| line.strip_prefix("pgfault "));
        line.expect("/proc/vmstat has pgfault").parse().unwrap()
    }

    /// cladefs: reports `faults`, taken over `bytes`, per MiB of them, as an observation of
    /// `workload`.
    fn faults_per_mib(workload: &str, faults: u64, bytes: u64) {
        println!("OBSERVED {workload}-faults-per-mib {:.1}", faults as f64 / (bytes as f64 / MIB));
    }

    /// cladefs: the rows of the SQLite database the mmap workload scans, of a KiB each: about
    /// 256 MiB.
    const SQLITE_ROWS: u64 = 256 << 10;

    /// cladefs: SQLite reading through mmap (`PRAGMA mmap_size`), as databases and key-value stores
    /// built on it do (TASK-26.18.8): a table of [`SQLITE_ROWS`] rows made, then scanned whole by a
    /// new process, whose mapping faults each part of the database in.
    fn sqlite_workload() {
        let db = format!("{MNT}/sqlite.db");
        let make = format!(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, payload BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {SQLITE_ROWS})
             INSERT INTO t SELECT i, randomblob(1000) FROM n;"
        );
        run("sqlite3", &[&db, &make]);
        let bytes = fs::metadata(&db).unwrap().len();
        for (engine, mmap) in [("pread", 0), ("mmap", 1073741824)] {
            drop_caches();
            for warm in [false, true] {
                let workload = format!("sqlite-{engine}-scan{}", if warm { "-warm" } else { "" });
                let scan = format!(
                    "PRAGMA mmap_size={mmap}; PRAGMA cache_size=-2048;
                     SELECT count(*),sum(length(payload)) FROM t;"
                );
                let faults = page_faults();
                let profiler = Profiler::start();
                let start = Instant::now();
                let output =
                    Command::new("sqlite3").args(["-batch", "-bail", &db, &scan]).output().unwrap();
                let secs = start.elapsed().as_secs_f64();
                let faults = page_faults() - faults;
                if let Some(profiler) = profiler {
                    profiler.report(&workload);
                }
                assert!(output.status.success(), "sqlite3: {output:?}");
                // The pragma prints the size it set, then the scan its row.
                let stdout = String::from_utf8_lossy(&output.stdout);
                let expected = format!("{mmap}\n{SQLITE_ROWS}|{}", SQLITE_ROWS * 1000);
                assert_eq!(stdout.trim(), expected);
                result(&workload, "MiB/s", true, bytes as f64 / MIB / secs);
                faults_per_mib(&workload, faults, bytes);
            }
        }
        fs::remove_file(&db).unwrap();
    }

    fn sqlite_transactions() {
        use crate::sqlite::{self, Mode, Phase};
        let count: usize = std::env::var("CLADEFS_BENCH_SQLITE_TXNS")
            .map_or(200, |n| n.parse().expect("CLADEFS_BENCH_SQLITE_TXNS is a count"));
        assert!((1..=100_000).contains(&count), "SQLite transaction count must be 1..=100000");
        let selected = std::env::var("CLADEFS_BENCH_SQLITE_WORKLOAD").unwrap_or_default();
        assert!(
            selected.is_empty()
                || Mode::ALL.iter().any(|mode| {
                    [1, 100]
                        .iter()
                        .any(|batch| selected == format!("sqlite-{}-batch{batch}", mode.name()))
                }),
            "CLADEFS_BENCH_SQLITE_WORKLOAD must name a SQLite transaction workload"
        );
        // Keep sidecars isolated, including those SQLite leaves after a killed writer.
        let root = tempfile::Builder::new().prefix("sqlite").tempdir_in(MNT).unwrap();
        for mode in Mode::ALL {
            sqlite::recovery(root.path(), mode);
            for batch in [1, 100] {
                let workload = format!("sqlite-{}-batch{batch}", mode.name());
                if !selected.is_empty() && selected != workload {
                    continue;
                }
                let measurement =
                    sqlite::transactions(root.path(), mode, count, batch, |phase, run| {
                        let (name, operations) = match phase {
                            Phase::Commits => (workload.clone(), count),
                            Phase::Checkpoint => (format!("{workload}-checkpoint"), 1),
                        };
                        observed(&name, operations, run)
                    });
                result(&workload, "txn/s", true, count as f64 / measurement.commit_seconds);
                println!(
                    "OBSERVED {workload}-updates-per-second {:.1}",
                    (count * batch) as f64 / measurement.commit_seconds
                );
                if let Some(seconds) = measurement.checkpoint_seconds {
                    result(&format!("{workload}-checkpoint"), "ms", false, seconds * 1000.0);
                }
            }
        }
        sqlite::snapshot(root.path());
    }

    /// Reports the rate of `count` operations done in `secs`.
    fn ops(workload: &str, count: usize, secs: f64) {
        result(workload, "ops/s", true, count as f64 / secs);
    }

    /// Times `f`, which does `count` operations, as `workload`: with CLADEFS_BENCH_GUEST_PERF set,
    /// sampled by perf throughout, whose report of the kernel's hottest symbols goes to the log
    /// (perf's own overhead is then in the measurement).
    fn timed<T>(workload: &str, count: usize, f: impl FnOnce() -> T) -> T {
        observed(workload, count, || {
            let start = Instant::now();
            let value = f();
            ops(workload, count, start.elapsed().as_secs_f64());
            value
        })
    }

    /// Runs `f`, which does `count` operations, as `workload`: with CLADEFS_BENCH_GUEST_PERF set,
    /// sampled by perf throughout, whose report goes to the log, and with the extent-store
    /// device's requests per operation logged.
    fn observed<T>(workload: &str, count: usize, f: impl FnOnce() -> T) -> T {
        let requests = device_requests().zip(device_request_ns());
        let waited = cladefs_stats();
        let profiler = Profiler::start();
        let value = f();
        if let Some(profiler) = profiler {
            profiler.report(workload);
        }
        // cladefs: what the guest's Fxfs waited on and did meanwhile, those that changed
        // (/proc/fs/cladefs/<tag>, TASK-91.7).
        if let (Some(before), Some(after)) = (waited, cladefs_stats()) {
            let changed: Vec<String> = (after.iter())
                .filter_map(|(name, after)| {
                    let delta = after.checked_sub(*before.get(name)?)?;
                    (delta > 0).then(|| format!("{name} {delta}"))
                })
                .collect();
            eprintln!("FXFS {workload}: {}", changed.join(", "));
        }
        if let (Some((before, before_ns)), Some(after), Some(after_ns)) =
            (requests, device_requests(), device_request_ns())
        {
            let per_op: Vec<String> = after
                .iter()
                .zip(&before)
                .zip(after_ns.iter().zip(&before_ns))
                .filter(|(((_, after), (_, before)), _)| after > before)
                .map(|(((kind, after), (_, before)), ((_, after_ns), (_, before_ns)))| {
                    let requests = after - before;
                    format!(
                        "{kind} {:.2} ({:.0} µs each)",
                        requests as f64 / count as f64,
                        (after_ns - before_ns) as f64 / requests as f64 / 1000.0
                    )
                })
                .collect();
            eprintln!("REQUESTS {workload}: {} per operation", per_op.join(", "));
        }
        value
    }

    /// cladefs: what the guest's Fxfs counts (/proc/fs/cladefs/<tag>, TASK-91.7), by name, if a
    /// filesystem of it is mounted: the first's.
    fn cladefs_stats() -> Option<std::collections::BTreeMap<String, u64>> {
        let stats = fs::read_dir("/proc/fs/cladefs").ok()?.find_map(|entry| entry.ok())?.path();
        let text = fs::read_to_string(stats).ok()?;
        Some(
            (text.lines())
                .filter_map(|line| {
                    let (name, value) = line.split_once(' ')?;
                    Some((name.to_owned(), value.parse().ok()?))
                })
                .collect(),
        )
    }

    /// The extent-store device's count of the requests it has taken, by type, if the guest's
    /// filesystem is on one.
    fn device_requests() -> Option<Vec<(String, u64)>> {
        device_request_counters("requests")
    }

    /// The time the extent-store device's requests took, queued to completed, by type, in
    /// nanoseconds.
    fn device_request_ns() -> Option<Vec<(String, u64)>> {
        device_request_counters("request_ns")
    }

    /// The extent-store device's attribute `name`: a counter for each type of request.
    fn device_request_counters(name: &str) -> Option<Vec<(String, u64)>> {
        let device =
            fs::read_dir("/sys/bus/virtio/drivers/virtio_extent").ok()?.find_map(|entry| {
                let path = entry.ok()?.path().join(name);
                path.exists().then_some(path)
            })?;
        let counts = fs::read_to_string(device).ok()?;
        Some(
            counts
                .lines()
                .filter_map(|line| {
                    let (kind, count) = line.split_once(' ')?;
                    Some((kind.to_string(), count.parse().ok()?))
                })
                .collect(),
        )
    }

    /// perf, recording the whole guest into a tmpfs, enabled through its control FIFO once it is
    /// ready, so that it samples exactly what runs until `report`.
    struct Profiler {
        perf: std::process::Child,
        control: fs::File,
        ack: fs::File,
    }

    impl Profiler {
        const DIR: &str = "/run/bench-perf";

        fn start() -> Option<Self> {
            if !std::env::var("CLADEFS_BENCH_GUEST_PERF").is_ok_and(|v| !v.is_empty()) {
                return None;
            }
            if !std::path::Path::new(Self::DIR).exists() {
                fs::create_dir_all(Self::DIR).unwrap();
                mount_tmpfs(Self::DIR);
            }
            let (control, ack) = (format!("{}/control", Self::DIR), format!("{}/ack", Self::DIR));
            for fifo in [&control, &ack] {
                let _ = fs::remove_file(fifo);
                let c = CString::new(fifo.as_str()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo {fifo}");
            }
            let data = format!("{}/perf.data", Self::DIR);
            let _ = fs::remove_file(&data);
            // `record:EVENTS` samples the events, such as the scheduler's wakeups, rather than
            // the time.
            let setting = std::env::var("CLADEFS_BENCH_GUEST_PERF").unwrap();
            let events = match setting.strip_prefix("record:") {
                Some(events) => vec!["-e".to_string(), events.to_string()],
                None => vec!["-F".to_string(), "4999".to_string()],
            };
            let perf = Command::new("perf")
                .args(["record", "-a", "-g"])
                .args(&events)
                .args(["-D", "-1", "-q", "-o", &data])
                .arg(format!("--control=fifo:{control},{ack}"))
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("perf record");
            // Opening the FIFOs waits for perf to open them too.
            let mut control = fs::OpenOptions::new().write(true).open(&control).unwrap();
            let mut ack = fs::File::open(&ack).unwrap();
            Self::command(&mut control, &mut ack, "enable");
            Some(Self { perf, control, ack })
        }

        /// Sends perf `command`, and waits for it to acknowledge it.
        fn command(control: &mut fs::File, ack: &mut fs::File, command: &str) {
            use std::io::Read;
            control.write_all(format!("{command}\n").as_bytes()).unwrap();
            let mut reply = [0u8; 5];
            ack.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"ack\n\0", "perf's reply to {command}");
        }

        fn report(mut self, workload: &str) {
            Self::command(&mut self.control, &mut self.ack, "disable");
            unsafe { libc::kill(self.perf.id() as i32, libc::SIGINT) };
            self.perf.wait().expect("perf record");
            let data = format!("{}/perf.data", Self::DIR);
            let report = Command::new("perf")
                .args([
                    "report",
                    "-i",
                    &data,
                    "--stdio",
                    "--sort",
                    "sym",
                    "--no-children",
                    "-g",
                    "none",
                    "--percent-limit",
                    "1",
                    "-q",
                ])
                .output()
                .expect("perf report");
            // And the functions the time is spent in, callees included, by thread: the workload's
            // own apart from the kernel's workers'.
            let callers = Command::new("perf")
                .args([
                    "report",
                    "-i",
                    &data,
                    "--stdio",
                    "--sort",
                    "comm,sym",
                    "--children",
                    "-g",
                    "none",
                    "--percent-limit",
                    "0.5",
                    "-q",
                ])
                .output()
                .expect("perf report");
            // And what calls the functions taking the most time themselves.
            let chains = Command::new("perf")
                .args([
                    "report",
                    "-i",
                    &data,
                    "--stdio",
                    "--sort",
                    "sym",
                    "--no-children",
                    "-g",
                    "caller,2,callee,function,percent",
                    "--max-stack",
                    "24",
                    "--percent-limit",
                    "4",
                    "-q",
                ])
                .output()
                .expect("perf report");
            eprintln!(
                "PROFILE {workload}\n{}\nINCLUSIVE {workload}\n{}\nCALLERS {workload}\n{}",
                String::from_utf8_lossy(&report.stdout),
                String::from_utf8_lossy(&callers.stdout),
                String::from_utf8_lossy(&chains.stdout)
            );
        }
    }

    fn mount_tmpfs(target: &str) {
        let (source, target, fstype) = (c"tmpfs", CString::new(target).unwrap(), c"tmpfs");
        let ret = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
        };
        assert_eq!(ret, 0, "mount tmpfs: {}", std::io::Error::last_os_error());
    }

    /// Runs `f(thread)` on `THREADS` threads at once, and times them all as `workload`, of `count`
    /// operations in all.
    fn parallel(workload: &str, count: usize, f: impl Fn(usize) + Sync) {
        parallel_on(THREADS, workload, count, f)
    }

    /// Runs `f(thread)` on `threads` threads at once, and times them all as `workload`, of `count`
    /// operations in all.
    fn parallel_on(threads: usize, workload: &str, count: usize, f: impl Fn(usize) + Sync) {
        timed(workload, count, || {
            std::thread::scope(|scope| {
                for thread in 0..threads {
                    let f = &f;
                    scope.spawn(move || f(thread));
                }
            })
        });
    }

    fn setxattr(path: &str, name: &str, value: &[u8]) -> std::io::Result<()> {
        let path = CString::new(path).unwrap();
        let name = CString::new(name).unwrap();
        let ret = unsafe {
            libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0)
        };
        if ret == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }

    /// Operations on the names and attributes of many small files in one directory.
    fn files_workloads() {
        let files = files();
        let dir = format!("{MNT}/meta");
        let other = format!("{MNT}/meta-other");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&other).unwrap();
        let paths: Vec<String> = (0..files).map(|i| format!("{dir}/file-{i}")).collect();

        timed("create", files, || {
            for path in &paths {
                fs::write(path, b"a small file").unwrap();
            }
        });
        drop_caches();
        timed("stat-cold", files, || {
            for path in &paths {
                fs::metadata(path).unwrap();
            }
        });
        timed("stat-warm", files, || {
            for path in &paths {
                fs::metadata(path).unwrap();
            }
        });
        timed("lookup-missing", files, || {
            for i in 0..files {
                assert!(fs::metadata(format!("{dir}/missing-{i}")).is_err());
            }
        });
        timed("readdir", files, || {
            assert_eq!(fs::read_dir(&dir).unwrap().count(), files);
        });
        drop_caches();
        timed("read-small-cold", files, || {
            for path in &paths {
                assert_eq!(fs::read(path).unwrap().len(), 12);
            }
        });
        timed("chmod", files, || {
            for path in &paths {
                fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                    .unwrap();
            }
        });
        // A filesystem without extended attributes fails the workload.
        match setxattr(&paths[0], "user.bench", b"a value") {
            Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => {
                println!("FAILED setxattr");
            }
            first => {
                first.unwrap();
                timed("setxattr", files, || {
                    for path in &paths {
                        setxattr(path, "user.bench", b"a value").unwrap();
                    }
                });
            }
        }
        timed("link", files, || {
            for (i, path) in paths.iter().enumerate() {
                fs::hard_link(path, format!("{dir}/link-{i}")).unwrap();
            }
        });
        timed("rename", files, || {
            for i in 0..files {
                fs::rename(format!("{dir}/link-{i}"), format!("{dir}/renamed-{i}")).unwrap();
            }
        });
        timed("rename-across", files, || {
            for i in 0..files {
                fs::rename(format!("{dir}/renamed-{i}"), format!("{other}/moved-{i}")).unwrap();
            }
        });
        timed("unlink", files, || {
            for path in &paths {
                fs::remove_file(path).unwrap();
            }
        });
        for i in 0..files {
            fs::remove_file(format!("{other}/moved-{i}")).unwrap();
        }
        fs::remove_dir(&dir).unwrap();
        fs::remove_dir(&other).unwrap();
    }

    /// Small files created and fsynced one at a time, each with its directory, as a database or
    /// a mail server makes them durable.
    fn fsync_workloads() {
        let dir = format!("{MNT}/fsynced");
        fs::create_dir(&dir).unwrap();
        let handle = fs::File::open(&dir).unwrap();
        timed("create-fsync", FSYNCED, || {
            for i in 0..FSYNCED {
                let mut file = fs::File::create(format!("{dir}/file-{i}")).unwrap();
                file.write_all(b"a small file").unwrap();
                file.sync_all().unwrap();
                handle.sync_all().unwrap();
            }
        });
        run("rm", &["-rf", &dir]);
    }

    /// A tree of directories, made, walked cold, and removed.
    fn tree_workloads() {
        let root = format!("{MNT}/tree");
        let entries = TREE_DIRS * (TREE_FILES + 1);
        timed("mkdir-tree", entries, || {
            fs::create_dir(&root).unwrap();
            for d in 0..TREE_DIRS {
                let dir = format!("{root}/dir-{d}");
                fs::create_dir(&dir).unwrap();
                for f in 0..TREE_FILES {
                    fs::write(format!("{dir}/file-{f}"), b"x").unwrap();
                }
            }
        });
        drop_caches();
        timed("find-cold", entries, || {
            let mut found = 0;
            for dir in fs::read_dir(&root).unwrap() {
                let dir = dir.unwrap().path();
                fs::symlink_metadata(&dir).unwrap();
                for file in fs::read_dir(&dir).unwrap() {
                    fs::symlink_metadata(file.unwrap().path()).unwrap();
                    found += 1;
                }
                found += 1;
            }
            assert_eq!(found, entries);
        });
        timed("rm-tree", entries, || run("rm", &["-rf", &root]));
    }

    /// One large directory.
    fn large_dir_workloads() {
        let dir = format!("{MNT}/large");
        fs::create_dir(&dir).unwrap();
        timed("large-dir-create", LARGE_DIR, || {
            for i in 0..LARGE_DIR {
                fs::File::create(format!("{dir}/f{i}")).unwrap();
            }
        });
        drop_caches();
        timed("large-dir-stat-cold", LARGE_DIR, || {
            for i in 0..LARGE_DIR {
                fs::metadata(format!("{dir}/f{i}")).unwrap();
            }
        });
        timed("large-dir-unlink", LARGE_DIR, || {
            for i in 0..LARGE_DIR {
                fs::remove_file(format!("{dir}/f{i}")).unwrap();
            }
        });
        fs::remove_dir(&dir).unwrap();
    }

    /// `THREADS` threads creating, statting and removing files at once: each in a directory of its
    /// own, and all in one.
    fn concurrent_workloads() {
        let files = files();
        let count = THREADS * files;
        let own = |thread: usize| format!("{MNT}/thread-{thread}");
        for thread in 0..THREADS {
            fs::create_dir(own(thread)).unwrap();
        }
        parallel("create-parallel", count, |thread| {
            for i in 0..files {
                fs::File::create(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        parallel("stat-parallel", count, |thread| {
            for i in 0..files {
                fs::metadata(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        parallel("unlink-parallel", count, |thread| {
            for i in 0..files {
                fs::remove_file(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        for thread in 0..THREADS {
            fs::remove_dir(own(thread)).unwrap();
        }

        let shared = format!("{MNT}/shared");
        fs::create_dir(&shared).unwrap();
        parallel("create-shared-dir", count, |thread| {
            for i in 0..files {
                fs::File::create(format!("{shared}/t{thread}-f{i}")).unwrap();
            }
        });
        parallel("unlink-shared-dir", count, |thread| {
            for i in 0..files {
                fs::remove_file(format!("{shared}/t{thread}-f{i}")).unwrap();
            }
        });
        fs::remove_dir(&shared).unwrap();
    }

    /// cladefs: where the scaling workloads' files are, a file of [`SCALE_FILE_SIZE`] bytes per
    /// job, fio's `scale.<job>.0` (TASK-276).
    const SCALE_DIR: &str = "/mnt/bench/scale";
    const SCALE_FILE_SIZE: u64 = 64 << 20;
    /// cladefs: the files of the random writes to pages never read (`rand-write-unread`), one per
    /// job of each job count, `unread<jobs>.<job>.0`, whose writes reach a quarter of their pages
    /// (TASK-276.6).
    const UNREAD_FILE_SIZE: u64 = 16 << 20;

    /// cladefs: the jobs the scaling workloads run at, each in turn: CLADEFS_BENCH_JOBS, separated
    /// by commas, or 1, 2, 4 and 8.
    fn scale_jobs() -> Vec<usize> {
        let jobs = std::env::var("CLADEFS_BENCH_JOBS").unwrap_or_default();
        if jobs.is_empty() {
            return vec![1, 2, 4, 8];
        }
        jobs.split(',').map(|n| n.parse().expect("CLADEFS_BENCH_JOBS is counts")).collect()
    }

    /// cladefs: runs fio's `jobs` jobs named `name` at once, each on a file of its own of `size`
    /// bytes in [`SCALE_DIR`], and returns what it measured of them together (`group_reporting`).
    fn fio_jobs(name: &str, jobs: usize, size: u64, args: &[&str]) -> serde_json::Value {
        let output = "/tmp/fio.json";
        let (name, jobs, size) =
            (format!("--name={name}"), format!("--numjobs={jobs}"), format!("--size={size}"));
        let mut all = vec![
            &name[..],
            "--directory",
            SCALE_DIR,
            &jobs,
            &size,
            "--group_reporting",
            // fio would drop the files' cached pages before each job: drop_caches makes the cold
            // jobs cold.
            "--invalidate=0",
            "--output-format=json",
            "--output",
            output,
        ];
        all.extend_from_slice(args);
        run("fio", &all);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(output).unwrap()).expect("fio's JSON");
        json["jobs"][0].clone()
    }

    /// cladefs: reports what fio measured of `workload`'s `direction` (`read`, `write`), `job`: its
    /// operations a second, and the 50th and 99th percentiles of their completion latency; and of
    /// its fsyncs, if it made any.
    fn fio_result(workload: &str, job: &serde_json::Value, direction: &str) {
        let rates = Rates::of(&job[direction]);
        result(workload, "IOPS", true, rates.iops);
        let percentiles = |what: &serde_json::Value, of: &str| {
            for (name, key) in [("p50", "50.000000"), ("p99", "99.000000")] {
                if let Some(ns) = what["percentile"][key].as_f64() {
                    result(&format!("{workload}-{of}{name}"), "us", false, ns / 1000.0);
                }
            }
        };
        percentiles(&job[direction]["clat_ns"], "");
        if job["sync"]["total_ios"].as_u64().is_some_and(|ios| ios > 0) {
            percentiles(&job["sync"]["lat_ns"], "fsync-");
        }
    }

    /// cladefs: the request queues' interrupts by CPU, from /proc/interrupts: each queue's line, by
    /// its name, of the extent device's queues (`virtioN-requests`) and virtio-blk's
    /// (`virtioN-req.Q`).
    fn queue_interrupts() -> Vec<(String, Vec<u64>)> {
        let text = fs::read_to_string("/proc/interrupts").unwrap();
        let mut lines = text.lines();
        let cpus = lines.next().map_or(0, |header| header.split_whitespace().count());
        lines
            .filter_map(|line| {
                let name = line.split_whitespace().last()?;
                let queue = name.starts_with("virtio")
                    && (name.ends_with("-requests") || name.contains("-req."));
                if !queue {
                    return None;
                }
                let counts = line.split_whitespace().skip(1).take(cpus);
                Some((name.to_owned(), counts.map(|n| n.parse().unwrap_or(0)).collect()))
            })
            .collect()
    }

    /// cladefs: the extent device's attribute `name`'s lines, if the guest's filesystem is on one.
    fn device_attribute(name: &str) -> Option<String> {
        let device =
            fs::read_dir("/sys/bus/virtio/drivers/virtio_extent").ok()?.find_map(|entry| {
                let path = entry.ok()?.path().join(name);
                path.exists().then_some(path)
            })?;
        fs::read_to_string(device).ok()
    }

    /// cladefs: runs `f`, `workload`, as [`observed`] does, and says how its requests spread over
    /// the device's queues and where they completed (TASK-276): the requests each queue took, and
    /// the queues' interrupts by CPU, to the log; how many queues took requests, and the share of
    /// the requests completed on a CPU other than the one that queued them, as observations.
    fn scaled<T>(workload: &str, f: impl FnOnce() -> T) -> T {
        let counts = |text: Option<String>| -> Vec<u64> {
            let text = text.unwrap_or_default();
            text.lines().filter_map(|line| line.rsplit(' ').next()?.parse().ok()).collect()
        };
        let completions = || -> Option<std::collections::BTreeMap<String, u64>> {
            let text = device_attribute("completions")?;
            Some(
                (text.lines())
                    .filter_map(|line| {
                        let (name, n) = line.split_once(' ')?;
                        Some((name.to_owned(), n.parse().ok()?))
                    })
                    .collect(),
            )
        };
        let (queues, done, interrupts) =
            (counts(device_attribute("queues")), completions(), queue_interrupts());
        let value = observed(workload, 1, f);
        let taken: Vec<u64> = counts(device_attribute("queues"))
            .iter()
            .zip(queues.iter().chain(std::iter::repeat(&0)))
            .map(|(after, before)| after.saturating_sub(*before))
            .collect();
        if !taken.is_empty() {
            eprintln!("QUEUES {workload}: {taken:?}");
            println!(
                "OBSERVED {workload}-queues-used {}",
                taken.iter().filter(|n| **n > 0).count()
            );
        }
        if let (Some(before), Some(after)) = (done, completions()) {
            let delta = |name: &str| {
                after.get(name).unwrap_or(&0).saturating_sub(*before.get(name).unwrap_or(&0))
            };
            let requests: u64 = taken.iter().sum();
            if requests > 0 {
                let cross = delta("cross") as f64 * 100.0 / requests as f64;
                println!("OBSERVED {workload}-cross-cpu-pct {cross:.1}");
                println!(
                    "OBSERVED {workload}-interrupts-per-request {:.3}",
                    delta("interrupts") as f64 / requests as f64
                );
            }
        }
        // By position: a device's queues share a name, its lines in the queues' order.
        for (queue, (name, after)) in queue_interrupts().into_iter().enumerate() {
            let before = interrupts.get(queue).filter(|(n, _)| *n == name).map(|(_, c)| &c[..]);
            let by_cpu: Vec<u64> = after
                .iter()
                .zip(before.unwrap_or_default().iter().chain(std::iter::repeat(&0)))
                .map(|(after, before)| after.saturating_sub(*before))
                .collect();
            if by_cpu.iter().any(|n| *n > 0) {
                eprintln!("IRQS {workload}: {name} (line {queue}) by CPU {by_cpu:?}");
            }
        }
        value
    }

    /// cladefs: how the filesystem's throughput and latency scale with the jobs doing I/O at once,
    /// and with the guest's vCPUs, which the host sets (TASK-276): random reads cold and warm,
    /// random writes, fsynced appends and metadata operations, each at each of [`scale_jobs`]'s
    /// jobs, the reads first, on files of their own, which the writes then change. With them, how
    /// the device's queues took the requests and where they completed ([`scaled`]).
    fn scaling_workloads() {
        let all = scale_jobs();
        let most = *all.iter().max().expect("CLADEFS_BENCH_JOBS names jobs");
        fs::create_dir_all(SCALE_DIR).unwrap();
        // Every job's file laid out once, untimed, as fio would as each job starts.
        fio_jobs("scale", most, SCALE_FILE_SIZE, &["--rw=write", "--bs=1M", "--end_fsync=1"]);
        // cladefs: and those whose pages the guest never reads through the DAX window before
        // rand-write-unread writes them, a set for each job count, read in after the drop of caches
        // below, as a database's files are that a VM opens cold, which the host's memory holds
        // (another VM's, a base's, read them) and the VM writes at random (TASK-276.6): read once
        // here as created files are, through the page cache, emptied first (fio's invalidate),
        // so that the extent daemon's READs bring their extents into its memory.
        for &jobs in &all {
            let name = format!("unread{jobs}");
            fio_jobs(&name, jobs, UNREAD_FILE_SIZE, &["--rw=write", "--bs=1M", "--end_fsync=1"]);
            fio_jobs(&name, jobs, UNREAD_FILE_SIZE, &["--rw=read", "--bs=1M", "--invalidate=1"]);
        }
        for &jobs in &all {
            let name = |workload: &str| format!("scale-{workload}-j{jobs}");
            drop_caches();
            let workload = name("rand-read-cold");
            let job = scaled(&workload, || {
                fio_jobs(
                    "scale",
                    jobs,
                    SCALE_FILE_SIZE,
                    &["--rw=randread", "--bs=4k", "--io_size=8M"],
                )
            });
            fio_result(&workload, &job, "read");
            // Each file read whole first: the random reads then find it cached.
            fio_jobs("scale", jobs, SCALE_FILE_SIZE, &["--rw=read", "--bs=1M"]);
            let workload = name("rand-read-warm");
            let job = scaled(&workload, || {
                fio_jobs(
                    "scale",
                    jobs,
                    SCALE_FILE_SIZE,
                    &["--rw=randread", "--bs=4k", "--time_based", RUNTIME],
                )
            });
            fio_result(&workload, &job, "read");
        }
        // One job keeping 16 reads in flight, through io_uring, which the kernel's workers serve.
        drop_caches();
        let workload = "scale-rand-read-cold-qd16";
        let job = scaled(workload, || {
            fio_jobs(
                "scale",
                1,
                SCALE_FILE_SIZE,
                &[
                    "--rw=randread",
                    "--bs=4k",
                    "--io_size=8M",
                    "--ioengine=io_uring",
                    "--iodepth=16",
                ],
            )
        });
        fio_result(workload, &job, "read");
        for &jobs in &all {
            let name = |workload: &str| format!("scale-{workload}-j{jobs}");
            let workload = name("rand-write");
            let job = scaled(&workload, || {
                fio_jobs(
                    "scale",
                    jobs,
                    SCALE_FILE_SIZE,
                    &["--rw=randwrite", "--bs=4k", "--io_size=4M", "--end_fsync=1"],
                )
            });
            fio_result(&workload, &job, "write");
            // cladefs: the same writes to files whose pages nothing read since they were read in,
            // each write a page's first touch (TASK-276.6).
            let workload = name("rand-write-unread");
            let job = scaled(&workload, || {
                fio_jobs(
                    &format!("unread{jobs}"),
                    jobs,
                    UNREAD_FILE_SIZE,
                    &["--rw=randwrite", "--bs=4k", "--io_size=4M", "--end_fsync=1"],
                )
            });
            fio_result(&workload, &job, "write");
            // Each job appending to a log of its own, each 4 KiB fsynced.
            let workload = name("fsync-append");
            let job = scaled(&workload, || {
                fio_jobs("fsync", jobs, 2 << 20, &["--rw=write", "--bs=4k", "--fsync=1"])
            });
            fio_result(&workload, &job, "write");
            for job in 0..jobs {
                fs::remove_file(format!("{SCALE_DIR}/fsync.{job}.0")).unwrap();
            }
            // Each thread creating, statting and removing files in a directory of its own.
            let files = files();
            let own = |thread: usize| format!("{SCALE_DIR}/thread-{thread}");
            for thread in 0..jobs {
                fs::create_dir(own(thread)).unwrap();
            }
            let workload = name("create");
            scaled(&workload, || {
                parallel_on(jobs, &workload, jobs * files, |thread| {
                    for i in 0..files {
                        fs::File::create(format!("{}/f{i}", own(thread))).unwrap();
                    }
                })
            });
            drop_caches();
            let workload = name("stat-cold");
            scaled(&workload, || {
                parallel_on(jobs, &workload, jobs * files, |thread| {
                    for i in 0..files {
                        fs::metadata(format!("{}/f{i}", own(thread))).unwrap();
                    }
                })
            });
            let workload = name("unlink");
            scaled(&workload, || {
                parallel_on(jobs, &workload, jobs * files, |thread| {
                    for i in 0..files {
                        fs::remove_file(format!("{}/f{i}", own(thread))).unwrap();
                    }
                })
            });
            for thread in 0..jobs {
                fs::remove_dir(own(thread)).unwrap();
            }
        }
        run("rm", &["-rf", SCALE_DIR]);
    }

    /// The metadata workloads, those of the parts of them that `runs` says run, or all.
    fn metadata_workloads(runs: impl Fn(&str) -> bool) {
        let all = runs("metadata");
        let parts: [(&str, fn()); 5] = [
            ("files", files_workloads),
            ("fsync", fsync_workloads),
            ("tree", tree_workloads),
            ("large-dir", large_dir_workloads),
            ("concurrent", concurrent_workloads),
        ];
        for (part, workloads) in parts {
            if all || runs(part) {
                workloads();
            }
        }
    }

    /// Observes how far ahead of the guest's clock a file it writes is timestamped, in
    /// milliseconds: what `make` compares the guest's clock with.
    fn observe_mtime() {
        let path = format!("{MNT}/mtime");
        fs::write(&path, b"now").unwrap();
        let now = std::time::SystemTime::now();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let ahead = match modified.duration_since(now) {
            Ok(ahead) => ahead.as_secs_f64(),
            Err(behind) => -behind.duration().as_secs_f64(),
        };
        println!("OBSERVED mtime-ahead-of-guest-ms {:.3}", ahead * 1000.0);
        fs::remove_file(&path).unwrap();
    }

    /// Untars and builds Lua: small files, short-lived processes and a compiler.
    fn build_workload() {
        let dir = format!("{MNT}/build");
        fs::create_dir(&dir).unwrap();
        let source = format!("{dir}/lua-5.4.7");
        let timed = |workload: &str, program: &str, args: &[&str], cwd: &str| {
            let start = Instant::now();
            let status = Command::new(program)
                .args(args)
                .current_dir(cwd)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap_or_else(|err| panic!("{program}: {err}"));
            assert!(status.success(), "{program} {args:?}: {status}");
            let secs = start.elapsed().as_secs_f64();
            result(workload, "s", false, secs);
            secs
        };
        let total = timed("untar", "tar", &["-xzf", SOURCE_TARBALL], &dir)
            + timed("make", "make", &["-j2", "linux"], &source);
        result("build-total", "s", false, total);
        run("rm", &["-rf", &dir]);
    }

    /// The scale of pgbench's database: CLADEFS_BENCH_PGBENCH_SCALE, or 10 (about 160 MiB). The
    /// sandbox team's load, which broke Fxfs, is 50 (TASK-57).
    fn pgbench_scale() -> u32 {
        // Set and empty, as scripts pass what they were not given, it is the default too.
        std::env::var("CLADEFS_BENCH_PGBENCH_SCALE")
            .ok()
            .filter(|scale| !scale.is_empty())
            .map_or(10, |scale| scale.parse().expect("CLADEFS_BENCH_PGBENCH_SCALE is a count"))
    }

    /// PostgreSQL's pgbench, its TPC-B-like transactions, on a database on the filesystem: the
    /// database made (`pgbench -i`, of [`pgbench_scale`]) and CLADEFS_BENCH_PGBENCH_SECS (default
    /// 20) of transactions from 4 clients, each committing durably.
    fn pgbench_workload() {
        let data = format!("{MNT}/pgdata");
        let socket = "/run/pgbench";
        // Set and empty, it is the default too.
        let secs: u64 = std::env::var("CLADEFS_BENCH_PGBENCH_SECS")
            .ok()
            .filter(|secs| !secs.is_empty())
            .map_or(20, |secs| secs.parse().expect("CLADEFS_BENCH_PGBENCH_SECS is a count"));
        // PostgreSQL refuses to run as root.
        let as_postgres = |command: &str| {
            Command::new("su")
                .args(["postgres", "-s", "/bin/sh", "-c", command])
                .output()
                .expect("su")
        };
        let check = |what: &str, output: &std::process::Output| {
            assert!(
                output.status.success(),
                "{what}: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        /// Stops the server and removes its data, however the workload ends.
        struct Server<'a>(&'a str, &'a dyn Fn(&str) -> std::process::Output);
        impl Drop for Server<'_> {
            fn drop(&mut self) {
                let stop = (self.1)(&format!("pg_ctl -D {} -w -m fast stop", self.0));
                if !stop.status.success() {
                    eprintln!("pg_ctl stop: {}", String::from_utf8_lossy(&stop.stderr));
                    if let Ok(log) = fs::read_to_string(format!("{}/server.log", self.0)) {
                        eprintln!("server.log:\n{log}");
                    }
                }
                run("rm", &["-rf", self.0]);
            }
        }
        fs::create_dir_all(socket).unwrap();
        fs::create_dir(&data).unwrap();
        run("chown", &["postgres:postgres", &data, socket]);
        check("initdb", &as_postgres(&format!("initdb -D {data} -U postgres -A trust")));
        check(
            "pg_ctl start",
            &as_postgres(&format!(
                "pg_ctl -D {data} -w -l {data}/server.log start -o \"-c listen_addresses='' -c \
                 unix_socket_directories={socket}\""
            )),
        );
        let _server = Server(&data, &as_postgres);
        let start = Instant::now();
        let scale = pgbench_scale();
        check(
            "pgbench -i",
            &as_postgres(&format!("pgbench -h {socket} -i -s {scale} -q postgres")),
        );
        result("pgbench-init", "s", false, start.elapsed().as_secs_f64());
        // Observed per transaction, at about the rate ext4 commits them.
        let expected = secs as usize * 1600;
        let output = observed("pgbench-tps", expected, || {
            as_postgres(&format!("pgbench -h {socket} -c 4 -j 2 -T {secs} postgres"))
        });
        check("pgbench", &output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // "tps = 1234.5 (without initial connection time)"
        let tps = stdout
            .lines()
            .find_map(|line| line.strip_prefix("tps = "))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|tps| tps.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no tps in pgbench's output: {stdout}"));
        result("pgbench-tps", "tps", true, tps);
    }

    /// cladefs: a fork case's VM's work: the base's, which makes the data its forks use, or a
    /// fork's, which uses it.
    fn fork_workloads(backend: Backend) {
        let pgdata = format!("{MNT}/pgdata");
        let socket = "/run/pgbench";
        let as_postgres = |command: &str| {
            let output = Command::new("su")
                .args(["postgres", "-s", "/bin/sh", "-c", command])
                .output()
                .expect("su");
            assert!(
                output.status.success(),
                "{command}: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        let start_server = || {
            as_postgres(&format!(
                "pg_ctl -D {pgdata} -w -l {pgdata}/server.log start -o \"-c listen_addresses='' -c \
                 unix_socket_directories={socket}\""
            ))
        };
        fs::create_dir_all(socket).unwrap();
        run("chown", &["postgres:postgres", socket]);
        let role = std::env::var(ROLE).expect("a fork case's role");
        // cladefs: the base's filesystem, made as it mounts, or a fork's, its copy (TASK-112.2).
        let mount_bench = |base: bool| match backend {
            Backend::Ext4DaxPmemFork | Backend::Ext4PmemFork => {
                let device = bench_device();
                if base {
                    run("mkfs.ext4", &["-q", "-F", &device]);
                }
                let dax = if backend == Backend::Ext4DaxPmemFork { "always" } else { "never" };
                mount(&device, "ext4", &format!("dax={dax}"));
            }
            _ => mount("bench", "cladefs", ""),
        };
        if role == "base" {
            mount_bench(true);
            let start = Instant::now();
            run("cp", &["-a", "/usr", &format!("{MNT}/usr")]);
            result("base-copy-usr", "s", false, start.elapsed().as_secs_f64());
            let start = Instant::now();
            fs::create_dir(&pgdata).unwrap();
            run("chown", &["postgres:postgres", &pgdata]);
            as_postgres(&format!("initdb -D {pgdata} -U postgres -A trust"));
            start_server();
            as_postgres(&format!("pgbench -h {socket} -i -s 10 -q postgres"));
            as_postgres(&format!("pg_ctl -D {pgdata} -w -m fast stop"));
            result("base-pgbench-init", "s", false, start.elapsed().as_secs_f64());
            unsafe { libc::sync() };
            let target = CString::new(MNT).unwrap();
            assert_eq!(unsafe { libc::umount(target.as_ptr()) }, 0, "umount");
            return;
        }
        let requests = device_requests().zip(device_request_ns());
        let start = Instant::now();
        mount_bench(false);
        result("fork-mount", "s", false, start.elapsed().as_secs_f64());
        // Read whole, then again: the first read maps the base's data into the window, which the
        // daemon has in memory already when it is warm; the second finds it mapped, as pages
        // static DAX on pmem would hold from the start (TASK-112.1). What the guest's Fxfs did
        // meanwhile, its window's maps among it, and its requests, each read's whole, go to the
        // log.
        let read_usr = format!("find {MNT}/usr -type f -exec cat {{}} + > /dev/null");
        for workload in ["fork-read-usr", "fork-reread-usr"] {
            observed(workload, 1, || {
                let start = Instant::now();
                run("sh", &["-c", &read_usr]);
                result(workload, "s", false, start.elapsed().as_secs_f64());
            });
        }
        let start = Instant::now();
        start_server();
        result("fork-pg-start", "s", false, start.elapsed().as_secs_f64());
        // Set and empty, it is the default too.
        let secs: u64 = std::env::var("CLADEFS_BENCH_PGBENCH_SECS")
            .ok()
            .filter(|secs| !secs.is_empty())
            .map_or(10, |secs| secs.parse().expect("CLADEFS_BENCH_PGBENCH_SECS is a count"));
        let output = as_postgres(&format!("pgbench -h {socket} -S -c 4 -j 2 -T {secs} postgres"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let tps = stdout
            .lines()
            .find_map(|line| line.strip_prefix("tps = "))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|tps| tps.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no tps in pgbench's output: {stdout}"));
        result("fork-pgbench-select", "tps", true, tps);
        as_postgres(&format!("pg_ctl -D {pgdata} -w -m fast stop"));
        // What the fork's device asked of the daemon, as the guest saw it.
        if let (Some((before, before_ns)), Some(after), Some(after_ns)) =
            (requests, device_requests(), device_request_ns())
        {
            for (((kind, after), (_, before)), ((_, after_ns), (_, before_ns))) in
                after.iter().zip(&before).zip(after_ns.iter().zip(&before_ns))
            {
                println!("OBSERVED guest-{kind}-requests {}", after - before);
                println!("OBSERVED guest-{kind}-ms {:.3}", (after_ns - before_ns) as f64 / 1e6);
            }
        }
    }

    impl Test for TestFsBench {
        fn in_guest(self: Box<Self>) {
            start_watchdog();
            let default_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                default_hook(info);
                crate::kernel_log::print();
            }));
            match self.backend {
                Backend::Ext4GuestKernel | Backend::Ext4Loop | Backend::Ext4LoopUnsafe => {
                    let device = bench_device();
                    run("mkfs.ext4", &["-q", "-F", &device]);
                    mount(&device, "ext4", "");
                }
                Backend::FxfsGuest | Backend::FxfsGuestDax | Backend::FxfsGuestDaxUnsafe => {
                    mount("bench", "cladefs", "")
                }
                Backend::FxfsGuestDaxForkWarm
                | Backend::FxfsGuestDaxForkCold
                | Backend::FxfsGuestDaxForkPredicted
                | Backend::FxfsGuestForkWarm
                | Backend::Ext4DaxPmemFork
                | Backend::Ext4PmemFork => {
                    return fork_workloads(self.backend);
                }
                Backend::Ext4DaxPmem => {
                    let device = bench_device();
                    run("mkfs.ext4", &["-q", "-F", &device]);
                    mount(&device, "ext4", "dax=always");
                }
                Backend::Ext4Pmem => {
                    let device = bench_device();
                    run("mkfs.ext4", &["-q", "-F", &device]);
                    mount(&device, "ext4", "dax=never");
                }
            }
            let iterations: u32 = std::env::var("CLADEFS_BENCH_ITERATIONS")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            let groups = std::env::var("CLADEFS_BENCH_GROUPS").unwrap_or_default();
            let runs = |group: &str| groups.is_empty() || groups.split(',').any(|g| g == group);
            for i in 0..iterations {
                eprintln!("iteration {}/{iterations}", i + 1);
                observe_mtime();
                if runs("io") {
                    io_workloads();
                }
                if runs("io") || runs("sqlite") {
                    sqlite_workload();
                }
                if groups.split(',').any(|g| matches!(g, "sqlite" | "sqlite-transactions")) {
                    sqlite_transactions();
                }
                metadata_workloads(runs);
                // A failed build is recorded, and the other iterations still run.
                if runs("build") && std::panic::catch_unwind(build_workload).is_err() {
                    println!("FAILED build");
                    run("rm", &["-rf", &format!("{MNT}/build")]);
                }
                // cladefs: the scaling workloads run only when named, as pgbench does (TASK-276).
                if groups.split(',').any(|g| g == "scaling") {
                    scaling_workloads();
                }
                // cladefs: so too the working sets' and the syncs' (TASK-350).
                if groups.split(',').any(|g| g == "working-set") {
                    working_set_workloads();
                }
                if groups.split(',').any(|g| g == "sync") {
                    sync_workloads();
                }
                // pgbench runs only when named, which keeps a run of every other group short.
                if groups.split(',').any(|g| g == "pgbench")
                    && std::panic::catch_unwind(pgbench_workload).is_err()
                {
                    println!("FAILED pgbench");
                }
                drop_caches();
            }
            println!("DONE");
        }
    }
}
