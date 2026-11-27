// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: fxfs-guest-python-sandbox (TASK-665): how long a data-analysis sandbox takes from its
//! fork to its first answers, on hosts as a single-region fleet has them.
//!
//! The base is a Python project as an agent's sandbox starts from: Python 3.14 with uv, and a
//! project (`python-sandbox/`: its pyproject.toml, uv.lock and a marimo notebook) whose
//! dependencies `uv sync` installed, pandas, pyarrow and marimo among them; and a dataset, New
//! York's green-taxi trips of 2025, twelve Parquet files of about 14 MB, with the zones' names.
//! None of it is compiled to bytecode: the image's build deletes every `.pyc`, and the host checks
//! the root holds none before it imports it. So each sandbox compiles what it imports, into its
//! own fork, as a sandbox started from a base that was only installed into does.
//!
//! Three hosts, each an extent daemon of its own with a part cache, each the others' peer, asking
//! them for the parts they own before the bucket (TASK-406.2); one bucket, an S3 server's
//! (RustFS, as scripts/test-e2e-rustfs.sh starts it), through a proxy that delays each request
//! [`LATENCY_MS`] (faulty_s3), as object storage is a region's distance away. No other bucket, no
//! other region.
//!
//! The first host imports the image as a base, makes a version of it and syncs it to the bucket.
//! Then, round after round (CLADEFS_PYSANDBOX_ROUNDS, default 2), on each host in turn, a sandbox:
//! a fork of that version on the host, a VM booted on it with its root through a DAX window, whose
//! guest starts marimo's server on the notebook (`marimo run`), and once that serves HTTP, runs the
//! notebook (`marimo export html`), which reads the trips with pandas and answers questions about
//! them. The first round finds the second and third hosts cold, their memory and part caches
//! empty; the rounds after find each host warm. The first host is never cold: its import left the
//! base in its memory. Each round ends with the baseline (board 5519): the same VM, booted on an
//! ext4 image of the image's root directory on a virtio-blk disk, as a VM host's local disk holds
//! a root, the image just made, so in the host's page cache, as a warm host's base is in its
//! memory: what the sandbox takes on no cladefs.
//!
//! A sandbox runs one of two workloads (TASK-702.1), in turn as CLADEFS_PYSANDBOX_WORKLOADS lists
//! them (default `notebook` alone): `notebook`, marimo's server and the notebook, as above; or
//! `query`, the same trips counted with pyarrow alone (`python-sandbox/query.py`), no pandas and no
//! marimo, which reads another part of the base. Each host's daemon traces what each VM reads of
//! its cell (`CLADEFSD_READ_TRACES`), and each sandbox's trace is kept as
//! `traces/NN-SANDBOX-WORKLOAD.reads`, in the order the sandboxes ran, for
//! `scripts/read_prediction.py`.
//!
//! With CLADEFS_PYSANDBOX_COLD set, two sandboxes more follow the rounds, each a cold host's first
//! boot of the version, which the read evidence the rounds' sandboxes recorded predicts, fetched
//! into its part cache as it boots (TASK-702.3): host-b's, every host restarted, its memory and
//! part cache empty, so that its peers are cold too (`cold_peers`); then host-c's, it alone
//! restarted, its peers warm (`warm_peers`).
//!
//! Each sandbox is timed by phase, on the host's clock from just before its fork: the fork, the VM
//! process started, the guest's boot to its agent (the guest's own clock, from its kernel's start),
//! marimo serving (the notebook's alone), and the answers, each as the guest says it on its
//! console, which the host watches. And what it cost the hosts between them: their bucket GETs and
//! the bytes those downloaded, the parts asked of peers and given, the parts each served. It fails
//! if a sandbox's answers are not [`ANSWERS`], what the notebook answered outside any VM, on the
//! same files.

use macros::{guest, host};

pub struct TestFxfsGuestPythonSandbox;

/// What the notebook answers, as it writes them (JSON, its keys sorted): what it answered on a Mac
/// on the same files, with the same lock.
#[cfg_attr(feature = "guest", allow(dead_code))]
const ANSWERS: &str = r#"{"busiest_borough": "Manhattan", "busiest_hour": 17, "busiest_month": 5, "card_tip_percent": 22.86, "median_miles": 1.98, "top_pickup_zone": "East Harlem North", "trips": 591375, "trips_in_2025": 591354}"#;

/// What the query answers: two of the notebook's.
#[cfg_attr(feature = "guest", allow(dead_code))]
const QUERY_ANSWERS: &str = r#"{"trips": 591375, "trips_in_2025": 591354}"#;

/// The guest agent's environment variable naming its workload.
const WORKLOAD: &str = "PYSANDBOX_WORKLOAD";

/// What a sandbox runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    /// marimo's server, then the notebook.
    Notebook,
    /// The trips counted with pyarrow alone.
    Query,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Workload::Notebook => "notebook",
            Workload::Query => "query",
        }
    }

    /// The workload named `name`, if one is.
    fn named(name: &str) -> Option<Self> {
        [Workload::Notebook, Workload::Query].into_iter().find(|workload| workload.name() == name)
    }

    /// What it answers, as it writes them.
    #[cfg_attr(feature = "guest", allow(dead_code))]
    fn answers(self) -> &'static str {
        match self {
            Workload::Notebook => ANSWERS,
            Workload::Query => QUERY_ANSWERS,
        }
    }
}

/// What the guest says as it reaches each phase, its own clock's milliseconds since its boot after
/// it.
const AGENT: &str = "pysandbox: agent ";
const SERVING: &str = "pysandbox: serving ";
const ANSWERED: &str = "pysandbox: answered ";
/// What it says of the answers, and of the bytecode it compiled.
const SAID_ANSWERS: &str = "answers: ";
const COMPILED: &str = "compiled: ";
const OK: &str = "OK python-sandbox";

#[host]
mod host {
    use super::*;
    use cladefs::cladefsd::control::proto::{
        ForkCellRequest, ImportImageRequest, MakeVersionRequest, StatCellRequest, SyncCellRequest,
    };
    use cladefs::testing::control_path;

    use crate::common::{Measured, cladefs_guest_should_run, control, setup_rootfs};
    use crate::faulty_s3::FaultyS3;
    use crate::rootfs::{ContextFile, Download, Image};
    use crate::vm::Vm;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::{Context, ensure};
    use bucket_cell::faults::Fault;
    use cladefs::import::testing::oci_layout;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const GIB: u64 = 1 << 30;

    /// How long the bucket takes to answer each request, past the S3 server's own time: the
    /// sandbox team's runs' (TASK-61).
    const LATENCY_MS: u64 = 25;

    /// The base's capacity, and so each root's.
    const ROOT_CAPACITY: u64 = 8 * GIB;
    /// A root's DAX window.
    const WINDOW: u64 = GIB;
    /// Each host's memory, its daemon's to govern: the 2 GiB its daemon's cache was fixed at until
    /// board 14529, and the governor's headroom, 1 GiB at least (`governor::headroom`).
    const HOST_MEMORY: u64 = 3 * GIB;
    /// A sandbox's vCPUs and memory, in MiB: pandas reading a year of trips and marimo's server
    /// beside it.
    const VCPUS: u8 = 2;
    const MEMORY_MIB: u32 = 2048;
    /// How long the hosts' stats files may take to say what a sandbox did: their daemons write
    /// them within a poll of a change.
    const STATS_SETTLE: Duration = Duration::from_secs(2);

    const HOSTS: [&str; 3] = ["host-a", "host-b", "host-c"];

    /// The image: Python's slim one, uv copied in from its own, the project synced, the data beside
    /// it, and every `.pyc` deleted (Python's image has none of its own either).
    const CONTAINERFILE: &str = "\
FROM docker.io/library/python:3.14.10-slim-trixie@sha256:9aaa474097d094040649f2162a627c0049b408f52138d980916788d80f04802f
COPY --from=ghcr.io/astral-sh/uv:0.13.9@sha256:d3b601f0c7b118bc1753febf7c41cc80981df678445e0254284eecf84db79a7d /uv /usr/local/bin/uv
WORKDIR /app
COPY pyproject.toml uv.lock analysis.py query.py /app/
RUN UV_PYTHON_DOWNLOADS=never UV_LINK_MODE=copy uv sync --frozen --no-install-project --no-dev --no-cache
COPY green_tripdata_2025-01.parquet green_tripdata_2025-02.parquet green_tripdata_2025-03.parquet \
     green_tripdata_2025-04.parquet green_tripdata_2025-05.parquet green_tripdata_2025-06.parquet \
     green_tripdata_2025-07.parquet green_tripdata_2025-08.parquet green_tripdata_2025-09.parquet \
     green_tripdata_2025-10.parquet green_tripdata_2025-11.parquet green_tripdata_2025-12.parquet \
     taxi_zone_lookup.csv /app/data/
RUN find / -xdev -name '*.pyc' -delete
";

    /// The project's files, the case's own.
    const FILES: &[ContextFile] = &[
        ContextFile {
            name: "pyproject.toml",
            contents: include_str!("../python-sandbox/pyproject.toml"),
        },
        ContextFile { name: "uv.lock", contents: include_str!("../python-sandbox/uv.lock") },
        ContextFile {
            name: "analysis.py",
            contents: include_str!("../python-sandbox/analysis.py"),
        },
        ContextFile { name: "query.py", contents: include_str!("../python-sandbox/query.py") },
    ];

    /// The Taxi and Limousine Commission's green-taxi trips of a month of 2025, `MM`.
    macro_rules! trips {
        ($month:literal, $sha256:literal) => {
            Download {
                name: concat!("green_tripdata_2025-", $month, ".parquet"),
                sha256: $sha256,
                sources: &[concat!(
                    "https://d37ci6vzurychx.cloudfront.net/trip-data/green_tripdata_2025-",
                    $month,
                    ".parquet"
                )],
            }
        };
    }

    /// The dataset: New York's green-taxi trips of 2025, month by month, and the zones they name
    /// (NYC TLC trip records, NYC Open Data).
    const DOWNLOADS: &[Download] = &[
        trips!("01", "84f3a121667157efcbf012c3566a6065df6f8e0312c678cb2f29cd72cc9c0f10"),
        trips!("02", "b3ffe16a96ba0c68c8a8f8b10f049294ae6eb8d21f2e13b3fdbd452fa2e3498e"),
        trips!("03", "d58e8d08e5e8a34c12f0d3f5ea049ae25c7c8215218a6af7bf6b8124ecde1c79"),
        trips!("04", "6cf7ca2a4dd8907437e2e4b4728f62b353712ff09b322223a61e01aecee02411"),
        trips!("05", "dcbf4afce26d1adc544a89ee1bdac88089b1bbaa300685144d31eee646da58bb"),
        trips!("06", "451bc9f29382cca5dc33db05ff399cb137735d19c7449bbaa8b3348b0824f8e3"),
        trips!("07", "bbd6f9e5955570521df5c6dd40c1a75b37cccd730fcaff4fb50bcd32d2d35f69"),
        trips!("08", "ebaab192c199b850b0244e10c90622e472c1d4f2851168f950c9f75fe8db18d1"),
        trips!("09", "932ea2cb6a56f83c4389148ce73427b07a63106375b607a361cd107cf7a32042"),
        trips!("10", "b124d57fb63fe610d085ce9ebef1528c082932313d86133a33d9c7a000883fe6"),
        trips!("11", "efcd45635c7982aa7ac832e5922c2e5495ebb5ce44e35491fea58d63fa7eba05"),
        trips!("12", "d1e9def5085c3be62dc8f827c9bbb6d32455b8ba69921d3e823d95435ab155fc"),
        Download {
            name: "taxi_zone_lookup.csv",
            sha256: "1a99e105092230f8620f301edcca7f80d3080642ff404d28ed957d3fa222c8ed",
            sources: &["https://d37ci6vzurychx.cloudfront.net/misc/taxi_zone_lookup.csv"],
        },
    ];

    /// How long a VM's trace may take to be written once it exits: its daemon writes it as the
    /// VM's connection goes.
    const TRACE_SETTLE: Duration = Duration::from_secs(10);

    /// How a baseline sandbox's root image is named, after the sandbox, in the root's parent.
    const EXT4_IMAGE: &str = ".rootfs.ext4";

    /// A host of the run: its extent daemon, where it is reached, where it writes the traces of
    /// its VMs' reads, what its daemon is started with, and how often it was restarted.
    struct Host {
        name: &'static str,
        daemon: std::process::Child,
        socket: PathBuf,
        traces: PathBuf,
        env: Vec<(&'static str, std::ffi::OsString)>,
        restarts: u32,
    }

    impl Host {
        /// What its daemon is named, its log, part cache and stats file after it: a daemon
        /// restarted has them new.
        fn daemon_name(&self) -> String {
            match self.restarts {
                0 => self.name.to_owned(),
                restarts => format!("{}-r{restarts}", self.name),
            }
        }

        /// Starts its daemon, as [`Host::daemon_name`] names it.
        fn spawn(
            test_setup: &TestSetup,
            name: &str,
            socket: &Path,
            env: &[(&'static str, std::ffi::OsString)],
        ) -> anyhow::Result<std::process::Child> {
            let env: Vec<(&str, &std::ffi::OsStr)> =
                env.iter().map(|(var, value)| (*var, value.as_os_str())).collect();
            // Governed, as production's, within its host's memory, one of several hosts on one
            // machine (board 14529).
            crate::common::spawn_host_daemon(test_setup, socket, name, &env, HOST_MEMORY)
        }

        /// Restarts its daemon cold: its memory, and its part cache, a new one, empty.
        fn restart(&mut self, test_setup: &TestSetup) -> anyhow::Result<()> {
            crate::common::say_daemon_stats(test_setup, &self.daemon_name());
            self.daemon.kill()?;
            self.daemon.wait()?;
            // Its sockets, which a daemon killed leaves, and a daemon refuses to bind over.
            for socket in [self.socket.clone(), self.socket.with_extension("control")] {
                if let Err(err) = std::fs::remove_file(&socket)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(anyhow::Error::new(err).context(format!("{}", socket.display())));
                }
            }
            self.restarts += 1;
            self.daemon = Self::spawn(test_setup, &self.daemon_name(), &self.socket, &self.env)?;
            Ok(())
        }
    }

    /// What the hosts' daemons had done, as their stats files say, by host and counter.
    type Counts = Vec<BTreeMap<String, u64>>;

    /// The counters of the daemon `host`'s stats file in `dir` this case reports: its bucket's
    /// requests and downloads (`object-store:`) and its peers' (`peers:`), each named after its
    /// line, `LINE.COUNTER`.
    fn host_counts(dir: &Path, host: &str) -> anyhow::Result<BTreeMap<String, u64>> {
        let path = dir.join(format!("{host}-stats.txt"));
        let kept = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(counts_of(&kept))
    }

    /// [`host_counts`]' counters of a stats file that says `kept`.
    fn counts_of(kept: &str) -> BTreeMap<String, u64> {
        let mut counts = BTreeMap::new();
        for line in kept.lines() {
            let Some((kind, fields)) = line.split_once(": ") else { continue };
            if !["object-store", "peers"].contains(&kind) {
                continue;
            }
            for field in fields.split(' ') {
                if let Some((name, value)) = field.split_once('=')
                    && let Ok(value) = value.parse()
                {
                    counts.insert(format!("{kind}.{name}"), value);
                }
            }
        }
        counts
    }

    /// Every host's counters, once their stats files have settled.
    fn all_counts(dir: &Path, hosts: &[Host]) -> anyhow::Result<Counts> {
        std::thread::sleep(STATS_SETTLE);
        hosts.iter().map(|host| host_counts(dir, &host.daemon_name())).collect()
    }

    /// What the hosts did between `before` and `after`, a line of the counters this case reports,
    /// each host's in turn, `/` between them, bytes in MiB; and each summed over the hosts.
    fn cost(before: &Counts, after: &Counts) -> (String, BTreeMap<&'static str, u64>) {
        const REPORTED: [(&str, &[&str]); 5] = [
            ("bucket_gets", &["object-store.get", "object-store.get-range"]),
            ("bucket_mib", &["object-store.down"]),
            ("peer_asked", &["peers.asked"]),
            ("peer_given", &["peers.given"]),
            ("peer_served", &["peers.served"]),
        ];
        let mut line = Vec::new();
        let mut summed = BTreeMap::new();
        for (name, counters) in REPORTED {
            let mib = name.ends_with("_mib");
            let each: Vec<u64> = (before.iter().zip(after))
                .map(|(before, after)| {
                    let at = |counts: &BTreeMap<String, u64>| -> u64 {
                        counters.iter().filter_map(|counter| counts.get(*counter)).sum()
                    };
                    at(after).saturating_sub(at(before))
                })
                .collect();
            summed.insert(name, each.iter().sum());
            let each: Vec<String> = (each.iter())
                .map(|done| match mib {
                    true => format!("{:.1}", *done as f64 / (1 << 20) as f64),
                    false => done.to_string(),
                })
                .collect();
            line.push(format!("{name}={}", each.join("/")));
        }
        (line.join(" "), summed)
    }

    /// The `.pyc` files under `dir`, not following links.
    fn bytecode(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let mut dirs = vec![dir.to_owned()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).with_context(|| format!("{}", dir.display()))? {
                let entry = entry?;
                let kind = entry.file_type()?;
                let path = entry.path();
                if kind.is_dir() {
                    dirs.push(path);
                } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "pyc") {
                    found.push(path);
                }
            }
        }
        Ok(found)
    }

    /// The image's root, `root`, as an OCI layout at `layout`, one layer, its files root's as an
    /// image's are (the runner's user unpacked them).
    fn image(root: &Path, layout: &Path) -> anyhow::Result<()> {
        let tar = std::process::Command::new("tar")
            .args(["--numeric-owner", "--owner=0", "--group=0", "-C"])
            .arg(root)
            .args(["-cf", "-", "."])
            .output()
            .context("running tar")?;
        ensure!(tar.status.success(), "tar of {}: {}", root.display(), tar.status);
        oci_layout(layout, "cladefs/python-sandbox:1", &[tar.stdout])?;
        Ok(())
    }

    /// The milliseconds a guest said after `prefix` in `said`, if it has said it.
    fn said_ms(said: &str, prefix: &str) -> Option<u64> {
        said.lines().find_map(|line| line.strip_prefix(prefix)?.trim().parse().ok())
    }

    /// A sandbox's phases: the host's milliseconds from just before its fork to the end of each,
    /// but `boot`, the guest's own from its kernel's start to its agent's; `fork` a fork's alone,
    /// `serving` the notebook's alone.
    #[derive(Debug, PartialEq)]
    struct Phases {
        fork: Option<f64>,
        vm_started: f64,
        boot: u64,
        serving: Option<f64>,
        answered: f64,
    }

    /// A sandbox that ran: when its phases ended, if it reached them, how its VM exited, and what
    /// its guest said.
    struct Ran {
        workload: Workload,
        fork: Option<f64>,
        vm_started: f64,
        serving: Option<f64>,
        answered: Option<f64>,
        status: std::process::ExitStatus,
        said: String,
    }

    impl Ran {
        /// Its phases, if it reached every one and ended well.
        fn phases(&self) -> Option<Phases> {
            if !self.status.success() || !self.said.lines().any(|line| line == OK) {
                return None;
            }
            Some(Phases {
                fork: self.fork,
                vm_started: self.vm_started,
                boot: said_ms(&self.said, AGENT)?,
                serving: match self.workload {
                    Workload::Notebook => Some(self.serving?),
                    Workload::Query => None,
                },
                answered: self.answered?,
            })
        }

        /// What its guest said after `prefix`.
        fn said(&self, prefix: &str) -> Option<&str> {
            self.said.lines().find_map(|line| line.strip_prefix(prefix))
        }
    }

    /// The milliseconds since `began`.
    fn ms_since(began: Instant) -> f64 {
        began.elapsed().as_secs_f64() * 1000.0
    }

    /// Starts the hosts, each with a part cache, the others as its peers, its bucket through
    /// `endpoint`, their sockets in `sockets`.
    fn start_hosts(
        test_setup: &TestSetup,
        endpoint: &str,
        sockets: &Path,
    ) -> anyhow::Result<Vec<Host>> {
        // The peers' addresses, free as they are taken here, all at once so that they differ.
        let listeners: Vec<std::net::TcpListener> = (0..HOSTS.len())
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0"))
            .collect::<Result<_, _>>()?;
        let addresses: Vec<String> = (listeners.iter())
            .map(|listener| Ok(listener.local_addr()?.to_string()))
            .collect::<anyhow::Result<_>>()?;
        drop(listeners);
        let mut hosts = Vec::new();
        for (at, name) in HOSTS.into_iter().enumerate() {
            let socket = sockets.join(format!("{name}.sock"));
            // Each host's own placement.
            let token = (at + 1).to_string();
            let mut others = addresses.clone();
            others.remove(at);
            let others = others.join(",");
            let traces = test_setup.tmp_dir.join(format!("{name}-reads"));
            std::fs::create_dir_all(&traces)?;
            let env = vec![
                ("CLADEFSD_TOKEN", token.into()),
                ("AWS_ENDPOINT", endpoint.into()),
                ("CLADEFSD_PEER_ADDRESS", addresses[at].clone().into()),
                ("CLADEFSD_PEERS", others.into()),
                ("CLADEFSD_READ_TRACES", traces.clone().into()),
            ];
            let daemon = Host::spawn(test_setup, name, &socket, &env)?;
            hosts.push(Host { name, daemon, socket, traces, env, restarts: 0 });
        }
        Ok(hosts)
    }

    /// Imports the image at `layout` as the base on `host`, makes a version of it and
    /// syncs it to the bucket, for every host to fork; says how long each took in `measured`.
    fn make_base(
        host: &Host,
        layout: &Path,
        measured: &mut Measured,
    ) -> anyhow::Result<cladefs::CellVersion> {
        let control = control(&host.socket)?;
        let began = Instant::now();
        let cell = control
            .call(async |client| {
                client
                    .import_image(ImportImageRequest {
                        capacity: ROOT_CAPACITY,
                        image: control_path(layout),
                        reference: String::new(),
                        tenant: 0,
                    })
                    .await
            })
            .map(|response| cladefs::CellId { tenant: response.tenant, id: response.cell })?;
        let imported = began.elapsed().as_secs_f64();
        let began = Instant::now();
        let version = control
            .call(async |client| {
                client
                    .make_version(MakeVersionRequest {
                        tenant: cell.tenant,
                        cell: cell.id,
                        keep_last: 0,
                    })
                    .await
            })
            .map(|response| response.version)?;
        let _ = control.call(async |client| {
            client.stat_cell(StatCellRequest { tenant: cell.tenant, cell: cell.id }).await
        })?;
        control
            .call(async |client| {
                client.sync_cell(SyncCellRequest { tenant: cell.tenant, cell: cell.id }).await
            })
            .map(drop)?;
        let synced = began.elapsed().as_secs_f64();
        eprintln!(
            "pysandbox: {} imported the base in {imported:.1} s, versioned and synced it in \
             {synced:.1} s",
            host.name
        );
        measured.lower("pysandbox.base.import", "s", imported);
        measured.lower("pysandbox.base.sync", "s", synced);
        Ok(cladefs::CellVersion { tenant: cell.tenant, cell: cell.id, version })
    }

    /// Where a sandbox's root is.
    #[derive(Clone, Copy)]
    enum Disk<'a> {
        /// A fork of the version on the host, through a DAX window.
        Fork(&'a Host),
        /// The baseline: an ext4 image of the base's root directory on a virtio-blk disk, as a VM
        /// host's local disk holds a root, the image made just before the VM starts, so that the
        /// host's page cache holds it, as a warm host's memory holds its base (TASK-665, board
        /// 5519).
        Ext4,
    }

    impl Disk<'_> {
        /// What its sandboxes and metrics are named after: its host's letter, or `ext4`.
        fn label(&self) -> &str {
            match self {
                Disk::Fork(host) => &host.name["host-".len()..],
                Disk::Ext4 => "ext4",
            }
        }
    }

    /// Runs the sandbox `name` on `disk`, its guest running `workload`: forks `version` on its
    /// host, or makes its image, boots a VM on it, and watches its console as its guest runs, for
    /// the moments it says it serves and has answered. The clock starts just before the fork, or
    /// once the image is made: a host makes its forks as sandboxes start, but holds its local
    /// disks' images.
    fn sandbox(
        test_setup: &TestSetup,
        root: &Path,
        disk: Disk,
        name: &str,
        workload: Workload,
        version: cladefs::CellVersion,
    ) -> anyhow::Result<Ran> {
        let mut machine = Vm::new(VCPUS, MEMORY_MIB)?;
        machine.guest_kernel()?;
        machine.env(&std::ffi::CString::new(format!("{WORKLOAD}={}", workload.name()))?);
        let (began, fork) = match disk {
            Disk::Fork(host) => {
                let began = Instant::now();
                let fork_cell = control(&host.socket)?
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
                    })?;
                let fork = ms_since(began);
                machine.cell_id_served("root", fork_cell, ROOT_CAPACITY, WINDOW, &host.socket);
                machine.fxfs_root("root");
                (began, Some(fork))
            }
            Disk::Ext4 => {
                machine.root_image(&format!("{name}{EXT4_IMAGE}"));
                machine.make_root_image(root)?;
                (Instant::now(), None)
            }
        };
        let api = Vm::api_socket(name);
        let mut firecracker = machine.spawn_api(root, &test_setup.test_case, name, &api)?;
        let vm_started = ms_since(began);
        let console = firecracker.console().to_owned();
        let (mut serving, mut answered) = (None, None);
        while firecracker.running()? {
            if answered.is_none() {
                let said = crate::vm::guest_stdout(&std::fs::read(&console)?);
                if serving.is_none() && said.contains(SERVING) {
                    serving = Some(ms_since(began));
                }
                if said.contains(ANSWERED) {
                    answered = Some(ms_since(began));
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (status, console) = firecracker.wait()?;
        // Its API's socket, which Firecracker leaves.
        std::fs::remove_file(&api).with_context(|| format!("removing {}", api.display()))?;
        let said =
            crate::vm::without_kernel_lines(&crate::vm::guest_stdout(&std::fs::read(&console)?));
        Ok(Ran { workload, fork, vm_started, serving, answered, status, said })
    }

    /// Moves the trace of the reads of the one VM that ran on `host` since its last was kept to
    /// `to`, once its daemon has written it.
    fn keep_trace(host: &Host, to: &Path) -> anyhow::Result<()> {
        let deadline = Instant::now() + TRACE_SETTLE;
        loop {
            let written: Vec<PathBuf> = (std::fs::read_dir(&host.traces)?)
                .map(|entry| Ok(entry?.path()))
                .collect::<anyhow::Result<_>>()?;
            match written[..] {
                [] if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                [] => anyhow::bail!("{} wrote no trace in {TRACE_SETTLE:?}", host.name),
                [ref trace] => {
                    std::fs::rename(trace, to).with_context(|| {
                        format!("keeping {} as {}", trace.display(), to.display())
                    })?;
                    return Ok(());
                }
                _ => anyhow::bail!("{} wrote traces of several VMs: {written:?}", host.name),
            }
        }
    }

    /// The workloads CLADEFS_PYSANDBOX_WORKLOADS lists, comma-separated, which sandboxes run in
    /// turn: `notebook` alone if unset.
    fn workloads() -> anyhow::Result<Vec<Workload>> {
        let listed = std::env::var("CLADEFS_PYSANDBOX_WORKLOADS").unwrap_or_default();
        let listed = if listed.is_empty() { Workload::Notebook.name().to_owned() } else { listed };
        (listed.split(','))
            .map(|name| Workload::named(name).with_context(|| format!("no workload {name:?}")))
            .collect()
    }

    /// The sandboxes run so far, and what they need: the guests' root, the version they fork,
    /// where their traces are kept, how they failed, and the hosts' counters after the last.
    struct Run<'a> {
        root: &'a Path,
        version: cladefs::CellVersion,
        kept: &'a Path,
        failed: Vec<String>,
        counts: Counts,
        ran: usize,
    }

    impl Run<'_> {
        /// Runs a sandbox on `disk`, a fork on one of `hosts` or the baseline, running `workload`,
        /// `case` the case it is of (`r1`, the first round; `cold_peers`): says how long its
        /// phases took in `measured`, and what a fork cost the hosts, and keeps its trace; or says
        /// how it failed.
        fn sandbox(
            &mut self,
            test_setup: &TestSetup,
            measured: &mut Measured,
            hosts: &[Host],
            disk: Disk,
            case: &str,
            workload: Workload,
        ) -> anyhow::Result<()> {
            let label = disk.label();
            let name = match (disk, case.strip_prefix('r')) {
                (Disk::Fork(_), Some(round)) if round.parse::<usize>().is_ok() => {
                    format!("sandbox-{label}{round}")
                }
                _ => format!("sandbox-{label}-{case}"),
            };
            self.ran += 1;
            let ran = sandbox(test_setup, self.root, disk, &name, workload, self.version)?;
            let (on, cost, gets) = match disk {
                Disk::Fork(host) => {
                    let trace = format!("{:02}-{name}-{}.reads", self.ran, workload.name());
                    if let Err(err) = keep_trace(host, &self.kept.join(&trace)) {
                        self.failed.push(format!("{name}'s trace: {err:#}"));
                    }
                    let after = all_counts(&test_setup.tmp_dir, hosts)?;
                    let (cost, summed) = cost(&self.counts, &after);
                    self.counts = after;
                    (host.name, cost, Some(summed["bucket_gets"]))
                }
                Disk::Ext4 => ("a local ext4 image", "no daemon".to_owned(), None),
            };
            let Some(phases) = ran.phases() else {
                let (status, said) = (ran.status, &ran.said);
                self.failed.push(format!("{name} exited {status}, saying {said:?}"));
                return Ok(());
            };
            let (answers, expected) = (ran.said(SAID_ANSWERS), workload.answers());
            if answers != Some(expected) {
                self.failed.push(format!("{name} answered {answers:?}, not {expected}"));
            }
            let ms = |phase: Option<f64>| phase.map_or("-".to_owned(), |ms| format!("{ms:.0}ms"));
            eprintln!(
                "pysandbox: {name} on {on} running {}: fork={} vm_started={:.0}ms boot={}ms \
                 serving={} answered={:.0}ms compiled={} | {cost}",
                workload.name(),
                ms(phases.fork),
                phases.vm_started,
                phases.boot,
                ms(phases.serving),
                phases.answered,
                ran.said(COMPILED).unwrap_or("?"),
            );
            let metric = |phase: &str| format!("pysandbox.{label}.{case}.{phase}");
            if let Some(fork) = phases.fork {
                measured.lower(metric("fork"), "ms", fork);
            }
            measured.lower(metric("vm_started"), "ms", phases.vm_started);
            measured.lower(metric("boot"), "ms", phases.boot as f64);
            if let Some(serving) = phases.serving {
                measured.lower(metric("serving"), "ms", serving);
            }
            measured.lower(metric("answered"), "ms", phases.answered);
            if let Some(gets) = gets {
                measured.lower(metric("bucket_gets"), "count", gets as f64);
            }
            Ok(())
        }
    }

    impl TestFxfsGuestPythonSandbox {
        /// Runs the sandboxes, saying what it measured in `measured` and why they failed, if they
        /// did.
        fn run(&self, test_setup: &TestSetup, measured: &mut Measured) -> anyhow::Result<()> {
            let dir = &test_setup.tmp_dir;
            let rounds: usize = match std::env::var("CLADEFS_PYSANDBOX_ROUNDS") {
                Ok(rounds) => rounds.parse().context("CLADEFS_PYSANDBOX_ROUNDS is a count")?,
                Err(_) => 2,
            };
            let bucket_url = std::env::var("CLADEFSD_BUCKET").unwrap_or_default();
            ensure!(
                bucket_url.starts_with("s3://"),
                "CLADEFSD_BUCKET is {bucket_url:?}, not an S3 server's bucket: \
                 scripts/test-e2e-rustfs.sh starts one"
            );
            let server =
                std::env::var("AWS_ENDPOINT").context("AWS_ENDPOINT names no S3 server")?;
            let bucket = FaultyS3::start(&server)?;
            let latency = Fault::parse(&format!("latency-ms={LATENCY_MS}"));
            bucket.set(latency.map_err(anyhow::Error::msg)?);

            let root = setup_rootfs(test_setup)?;
            let compiled = bytecode(&root)?;
            ensure!(compiled.is_empty(), "the base's root holds bytecode: {compiled:?}");
            let layout = dir.join("sandbox-image");
            image(&root, &layout)?;
            // Short, as a Unix socket's path is: the test's directory, named after its case, isn't.
            let sockets = tempfile::Builder::new().prefix("cladefs-pysandbox").tempdir()?;
            let mut hosts = start_hosts(test_setup, &bucket.endpoint(), sockets.path())?;
            let version = make_base(&hosts[0], &layout, measured)?;

            let workloads = workloads()?;
            let kept = dir.join("traces");
            std::fs::create_dir_all(&kept)?;
            // host-a's import, which opened its base through no VM, traced nothing.
            let counts = all_counts(dir, &hosts)?;
            let mut run =
                Run { root: &root, version, kept: &kept, failed: Vec::new(), counts, ran: 0 };
            // Each round's sandboxes, then the baseline's.
            for round in 1..=rounds {
                let disks = hosts.iter().map(Disk::Fork).chain([Disk::Ext4]);
                for disk in disks {
                    let workload = workloads[run.ran % workloads.len()];
                    let case = format!("r{round}");
                    run.sandbox(test_setup, measured, &hosts, disk, &case, workload)?;
                }
            }
            // A cold host's first boot of the version, predicted, with cold peers, then warm ones.
            if std::env::var_os("CLADEFS_PYSANDBOX_COLD").is_some() {
                for (case, restarted, on) in
                    [("cold_peers", &[0, 1, 2][..], 1), ("warm_peers", &[2], 2)]
                {
                    for at in restarted {
                        hosts[*at].restart(test_setup)?;
                    }
                    run.counts = all_counts(dir, &hosts)?;
                    let workload = workloads[run.ran % workloads.len()];
                    let disk = Disk::Fork(&hosts[on]);
                    run.sandbox(test_setup, measured, &hosts, disk, case, workload)?;
                }
            }
            let failed = run.failed;
            for host in &mut hosts {
                crate::common::say_daemon_stats(test_setup, &host.daemon_name());
                host.daemon.kill()?;
                host.daemon.wait()?;
            }
            for failure in &failed {
                eprintln!("pysandbox: check failed: {failure}");
                measured.failed("sandbox", failure.clone());
            }
            ensure!(failed.is_empty(), "{} sandboxes failed", failed.len());
            // Passed: its cells, caches and image go; its logs stay.
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let bulky = [".root", ".base", "-cache"].iter().any(|end| name.ends_with(end))
                    || ["sandbox-image", "rootfs"].contains(&&*name);
                if bulky && path.is_dir() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("removing {}", path.display()))?;
                }
                if name.ends_with(EXT4_IMAGE) {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("removing {}", path.display()))?;
                }
            }
            Ok(())
        }
    }

    impl Test for TestFxfsGuestPythonSandbox {
        fn should_run(&self) -> ShouldRun {
            if std::env::var_os("CLADEFSD_BIN").is_none() {
                return ShouldRun::No("CLADEFSD_BIN does not name the extent daemon");
            }
            cladefs_guest_should_run()
        }

        fn rootfs_image(&self) -> Option<Image> {
            Some(Image { containerfile: CONTAINERFILE, downloads: DOWNLOADS, files: FILES })
        }

        fn timeout_secs(&self) -> u64 {
            1800
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

        /// A daemon's stats file's counters this case reads, by line, and the hosts' costs between
        /// two readings, each host's apart.
        #[test]
        fn the_hosts_costs_read_from_their_stats_files() {
            let kept = "\
object-store: get=3 get-range=10 head=1 down=4194304 get-ns=7
peers: asked=5 given=4 missing=1 failed=0 served=2 served-missing=0
daemon: maps=9 local_lag_p50_ms=1
uploads: held=0 max-held=0 budget=1
";
            let counts = counts_of(kept);
            assert_eq!(counts["object-store.get-range"], 10);
            assert_eq!(counts["peers.served"], 2);
            assert_eq!(counts.get("daemon.maps"), None);
            assert_eq!(counts.get("uploads.held"), None);
            let before = vec![BTreeMap::new(), counts.clone()];
            let mut later = counts.clone();
            later.insert("object-store.get".into(), 5);
            later.insert("peers.given".into(), 9);
            let after = vec![counts, later];
            let (line, summed) = cost(&before, &after);
            assert_eq!(
                line,
                "bucket_gets=13/2 bucket_mib=4.0/0.0 peer_asked=5/0 peer_given=4/5 peer_served=2/0"
            );
            assert_eq!(summed["bucket_gets"], 15);
        }

        /// TASK-702.1: CLADEFS_PYSANDBOX_WORKLOADS's workloads, in its order, the notebook alone
        /// unset; one it doesn't know refused.
        #[test]
        fn workloads_are_listed_in_turn() {
            // SAFETY: nextest runs each test in a process of its own, whose other threads don't
            // read the environment.
            let set =
                |listed: &str| unsafe { std::env::set_var("CLADEFS_PYSANDBOX_WORKLOADS", listed) };
            set("");
            assert_eq!(workloads().unwrap(), [Workload::Notebook]);
            set("notebook,query");
            assert_eq!(workloads().unwrap(), [Workload::Notebook, Workload::Query]);
            set("query,pandas");
            assert_eq!(workloads().unwrap_err().to_string(), "no workload \"pandas\"");
        }

        /// The milliseconds a guest says of a phase read back, and a phase it hasn't said is none.
        #[test]
        fn a_guests_phases_read_back() {
            let said = "pysandbox: agent 412\npysandbox: serving 3311\n";
            assert_eq!(said_ms(said, AGENT), Some(412));
            assert_eq!(said_ms(said, SERVING), Some(3311));
            assert_eq!(said_ms(said, ANSWERED), None);
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use nix::libc;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const MARIMO: &str = "/app/.venv/bin/marimo";
    const NOTEBOOK: &str = "/app/analysis.py";
    const PORT: u16 = 2718;
    /// How long marimo's server may take to serve: past it, it is hung.
    const SERVE_LIMIT: Duration = Duration::from_secs(600);

    /// The guest's clock, its milliseconds since its kernel started.
    fn since_boot_ms() -> u64 {
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: Fills `now`, which outlives the call.
        assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) }, 0);
        now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000
    }

    /// The loopback network, as a sandbox's init sets it up: the interface up, which marimo's
    /// server listens on, and `localhost` named, which the kernel marimo's export starts is
    /// reached at (the guest's init leaves every interface down, the image has no `ip`, and its
    /// /etc/hosts, a container runtime's to write, names nothing).
    fn loopback() {
        std::fs::write("/etc/hosts", "127.0.0.1 localhost\n::1 localhost\n").unwrap();
        loopback_up();
    }

    /// Brings the loopback interface up.
    fn loopback_up() {
        /// Linux's `struct ifreq`, its name and, of its union, the flags.
        #[repr(C)]
        struct IfReq {
            name: [u8; libc::IFNAMSIZ],
            flags: libc::c_short,
            _rest: [u8; 22],
        }
        // SAFETY: A socket of this process's, closed below.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        let mut request = IfReq { name: [0; libc::IFNAMSIZ], flags: 0, _rest: [0; 22] };
        request.name[..2].copy_from_slice(b"lo");
        // SAFETY: `request` is a `struct ifreq`, outliving each call.
        unsafe {
            assert_eq!(libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut request), 0, "SIOCGIFFLAGS");
            request.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            let set = libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &request);
            assert_eq!(set, 0, "SIOCSIFFLAGS: {}", std::io::Error::last_os_error());
            libc::close(fd);
        }
    }

    /// marimo, with `args`, as a sandbox's tool runs it.
    fn marimo(args: &[&str]) -> Command {
        let mut command = Command::new(MARIMO);
        command
            .arg("-y")
            .args(args)
            .current_dir("/app")
            .env("HOME", "/root")
            .env("PATH", "/app/.venv/bin:/usr/local/bin:/usr/bin:/bin")
            // It asks PyPI for a newer version, with no network.
            .env("MARIMO_SKIP_UPDATE_CHECK", "1");
        command
    }

    /// Whether marimo's server answers its health check.
    fn healthy() -> bool {
        let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", PORT)) else {
            return false;
        };
        let asked = stream.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
        let mut answer = String::new();
        asked.is_ok()
            && stream.read_to_string(&mut answer).is_ok()
            && answer.starts_with("HTTP/1.1 200")
    }

    /// Waits for `server` to serve; it must, within [`SERVE_LIMIT`], and not exit.
    fn serve(server: &mut Child, log: &Path) {
        let began = Instant::now();
        while !healthy() {
            if let Some(status) = server.try_wait().unwrap() {
                panic!(
                    "marimo's server exited {status}: {}",
                    std::fs::read_to_string(log).unwrap()
                );
            }
            assert!(
                began.elapsed() < SERVE_LIMIT,
                "marimo's server didn't serve in {SERVE_LIMIT:?}: {}",
                std::fs::read_to_string(log).unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The `.pyc` files under `dir`, not following links.
    fn bytecode(dir: &Path) -> usize {
        let mut found = 0;
        let mut dirs = vec![dir.to_owned()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                let path = entry.path();
                if kind.is_dir() {
                    dirs.push(path);
                } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "pyc") {
                    found += 1;
                }
            }
        }
        found
    }

    /// The query workload: pyarrow counting the trips, its answers in /tmp/answers.json.
    fn query() {
        let ran = Command::new("/app/.venv/bin/python")
            .arg("/app/query.py")
            .current_dir("/app")
            .env("HOME", "/root")
            .env("ANSWERS", "/tmp/answers.json")
            .stdin(Stdio::null())
            .output()
            .expect("running the query");
        assert!(
            ran.status.success(),
            "the query: {}: {}",
            ran.status,
            String::from_utf8_lossy(&ran.stderr)
        );
    }

    impl Test for TestFxfsGuestPythonSandbox {
        fn in_guest(self: Box<Self>) {
            println!("{AGENT}{}", since_boot_ms());
            let workload = std::env::var(WORKLOAD).unwrap_or_default();
            let server = match Workload::named(&workload) {
                Some(Workload::Notebook) => Some(notebook()),
                Some(Workload::Query) => {
                    query();
                    None
                }
                None => panic!("no workload {workload:?}"),
            };
            println!("{ANSWERED}{}", since_boot_ms());
            let answers = std::fs::read_to_string("/tmp/answers.json").unwrap();
            println!("{SAID_ANSWERS}{}", answers.trim());
            if let Some(mut server) = server {
                server.kill().unwrap();
                server.wait().unwrap();
            }
            let compiled = bytecode(Path::new("/app")) + bytecode(Path::new("/usr/local/lib"));
            println!("{COMPILED}{compiled}");
            println!("{OK}");
        }
    }

    /// The notebook workload: marimo's server on the notebook, which once it serves, the notebook
    /// run, its answers in /tmp/answers.json; the server returned, serving still.
    fn notebook() -> Child {
        loopback();
        let log = Path::new("/tmp/marimo-server.log");
        let output = std::fs::File::create(log).unwrap();
        let port = PORT.to_string();
        let mut server = marimo(&["run", NOTEBOOK, "--headless", "--no-token"])
            .args(["--host", "127.0.0.1", "--port", &port])
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .expect("starting marimo's server");
        serve(&mut server, log);
        println!("{SERVING}{}", since_boot_ms());
        let exported = marimo(&["export", "html", NOTEBOOK, "-o", "/tmp/analysis.html"])
            .env("ANSWERS", "/tmp/answers.json")
            .stdin(Stdio::null())
            .output()
            .expect("running the notebook");
        assert!(
            exported.status.success(),
            "marimo export: {}: {}",
            exported.status,
            String::from_utf8_lossy(&exported.stderr)
        );
        server
    }
}
