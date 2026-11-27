// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefsd, cladefs's daemon, one a host (TASK-475): serves the cells of the VMs on this
//! host, from one memory, to VM processes that connect to its socket (`cladefs::cladefsd`); and, if
//! told to, mounts tenants' cells with FUSE (`cladefs::cladefsd::mount`). A cell is held
//! by this one process, whichever serves them.
//!
//! ```sh
//! cladefsd SOCKET [MEMORY_BYTES]
//! ```
//!
//! Its memory, which the sealed extents it caches are kept in, and VMs' DAX windows map read-only,
//! is a page cache (`cladefs::cladefsd::governor`, TASK-358): as much of the host's as is free,
//! less a headroom (`CLADEFSD_HEADROOM` bytes, by default a sixteenth of the host's or cgroup's
//! limit, 1 GiB at least and half the limit at most), never below a shared working-set floor
//! (`CLADEFSD_MEMORY_FLOOR` bytes, by default an eighth of the limit), shedding derived caches and
//! idle extents on soft headroom dips and shrinking mapped extents gradually under real kernel
//! memory pressure while backing off on refault thrashing, each tenant kept its share by weight
//! (`SetMemoryWeight`). MEMORY_BYTES, if given, fixes it at that instead, ungoverned, as a test's
//! daemon may have it. A cell's private pages, which copy-on-write DAX gives a VM's writes, are of
//! the VM's memory, not of it (backlog doc-13). Out of it, or past a limit the control API sets,
//! the daemon evicts the cached extents asked for longest ago, and fails what it cannot make room
//! for with ENOMEM, rather than stop (TASK-50.3); but a page-in while the host's memory is full
//! waits for room (board 5662). A VMM's extent device connects to the socket its configuration
//! names (cladefs's Firecracker's `cladefsd_socket`).
//!
//! It runs as a host of cladefs's multi-node system: `CLADEFSD_BUCKET` names the bucket its
//! their cells are kept in, and `CLADEFSD_TOKEN` its placement's fencing token, which
//! the system's control plane gives it, and which fences earlier placements' writes and deletes;
//! without them, it refuses to start (`cladefs::bucket::Host`, TASK-45). Tests and development use
//! an in-memory bucket (`memory:///NAME`).
//!
//! One daemon serves every VM on a host. Connections to SOCKET are of the default tenant: with
//! `CLADEFSD_ROOTS` naming directories, separated by colons, they name cells in them alone
//! (`cladefs::cladefsd`, Isolation). SOCKET is the daemon's user's alone, mode 0600: a VM process
//! of another user, as a jail's, connects to a tenant's socket, granted to its user, and never to
//! this one, mounted or not (TASK-155). The VM manager runs jails as another user than the
//! daemon's, and hands them SOCKET's path no more than it does the control socket's.
//!
//! With `CLADEFSD_GRPC` naming a path, the daemon serves its control API there too, gRPC over a
//! Unix socket (`cladefs::cladefsd::control`;
//! `crates/cladefs/proto/cladefs/cladefsd/v1/cladefsd.proto`), through which the host's VM manager
//! makes, forks, syncs and deletes cells, and grants tenants VM sockets of their own: whoever
//! connects may do all of it, so that it is the daemon's user's alone, mode 0600, as SOCKET is,
//! whatever the umask (TASK-159): the VM manager runs as that user, or as root. The tenants'
//! sockets go in the directory `CLADEFSD_TENANTS` names, or `tenants` beside the control socket,
//! each mode 0660 from the first. The control socket serves the standard gRPC health service too
//! (`grpc.health.v1.Health`, TASK-48): SERVING while the daemon accepts VM processes, NOT_SERVING
//! as it starts and as it stops.
//!
//! SIGTERM drains it, and it exits (`cladefs::cladefsd::drain`, TASK-141.4); cladefs: and so do
//! SIGINT, as Ctrl-C in its terminal sends, and SIGHUP, as its terminal or ssh session closing
//! does, which ended it at once before, a crash for every cell (board 8567; hotbed question
//! 502). A second signal, its drain under way, changes nothing: SIGKILL ends it at once.
//! Once the health
//! service reports NOT_SERVING, it takes no more VM processes nor control calls (`UNAVAILABLE`),
//! closes every VM process's connection, whose VM goes as at the daemon's exit, and closes their
//! cells, durable, and the rest it has open, and waits for their uploads: within `CLADEFSD_DRAIN`
//! seconds (60 by default), exiting 0; past them, it logs what is left and exits 1. A host is
//! upgraded so, its VMs moved off or stopped first (`docs/upgrading-a-host.md`).
//!
//! With `CLADEFSD_MOUNT` naming a mount point, a directory, and `CLADEFSD_MOUNT_STORES` one or
//! more colon-separated JBOD directories for their cells, the daemon mounts a directory of
//! tenants' cells there with FUSE, none at first
//! (TASK-302, TASK-364, TASK-475.2), and serves its Mount service on the control socket
//! (`proto/cladefs/mount/v1/mount.proto`; `cladefsctl mount`), which `CLADEFSD_GRPC` must name.
//! Attaching a cell (`Attach`; a cell of 0, a new Fxfs one, its filesystem made empty) takes it
//! from the daemon's bucket, at the placement of the token the call gives, the daemon's if none,
//! into a cell directory at `CELLS/TENANT/CELL`, an Fxfs cell shown at `MOUNT/TENANT/CELL`
//! (doc-20), TENANT and CELL in base 32, padded (`base32_id::Padded`); a SQLite cell, or a VM's, is
//! not shown. `Detach` takes a cell away once nothing uses it, written back, or at once if forced;
//! `List` says what is attached. The mount is the daemon's user's; with
//! `CLADEFSD_MOUNT_ALLOW_OTHER=1`, other users may use it as its permissions say (for a user other
//! than root, `/etc/fuse.conf` must have `user_allow_other`). Root mounts with mount(2), any other
//! user through libfuse's `fusermount3`. The drain closes it: unmounted lazily, its cells written
//! back and their cells closed once nothing uses it, within the drain's bound.
//!
//! With `CLADEFSD_METRICS` naming an address (`HOST:PORT`), the daemon serves its metrics
//! there, over HTTP, at `/metrics`, in Prometheus' text format (`cladefs::cladefsd::metrics`,
//! TASK-47); cladefs: and its z-pages under `/z/`, its spans by latency, CPU profiles and what it
//! logs, which `POST /z/log?level=LEVELS` changes while it runs (`cladefs::cladefsd::zpages`,
//! TASK-672, board 8571): an address to bind to localhost, or a network only operators reach.
//! `CLADEFSD_ZPAGES` serves the z-pages elsewhere: `off`, nowhere; an address, there alone, the
//! metrics' address serving `/metrics` alone, for a scraper that may not profile the daemon nor
//! change its log (board 8559).
//!
//! `CLADEFSD_BUCKET` names the bucket (`s3://bucket/prefix`, `gs://bucket/prefix`,
//! `az://container/prefix`) each cell it opens keeps its cell in (backlog doc-7), with credentials
//! from the environment's `AWS_*`, `GOOGLE_*` or `AZURE_*` variables: durable commits are uploaded
//! behind them, within a second. With `CLADEFSD_PART_CACHE` naming one or more colon-separated
//! directories (or glob patterns such as `/mnt/nvme*/cladefsd/part-cache`) across JBOD disks,
//! what cells read from the bucket is kept there, sparse, weighted by disk capacity with
//! zero-copy rename adoption onto the cell's own disk, as much as its cells leave of each
//! filesystem, and kept across the daemon's restarts: one daemon at a time locks each directory.
//! `CLADEFSD_PART_CACHE_BYTES` caps what it keeps across all shards combined, in bytes
//! (TASK-406.1): those read least recently go first.
//!
//! The host's disks are governed as its memory is (backlog doc-22): every filesystem the daemon
//! keeps cells or its part cache on is its but for a headroom (`CLADEFSD_DISK_HEADROOM`, bytes or
//! `N%`; by default a twentieth of the filesystem, 4 GiB at least, half of it at most), and no more
//! than `CLADEFSD_DISK_BYTES` on all of them together, if set. The cells come first, the part cache
//! taking what they leave and giving it back at once; near the allowance, the cells' uploaders are
//! urged and each tenant held to its share by weight; past it, guests' writes are refused with
//! ENOSPC, while every cell goes on committing (`docs/operating-a-host.md`).
//!
//! With `CLADEFSD_PEER_ADDRESS` naming this host's `HOST:PORT` among the hosts sharing its
//! bucket, and `CLADEFSD_PART_CACHE` a cache, the daemon serves them parts of the bucket's objects
//! there, over gRPC (`proto/cladefs/peer/v1/peer.proto`), and its cells' fetchers ask the host that
//! owns a part, by rendezvous hashing over them, before the bucket (TASK-406.2): a part many hosts
//! read comes from the bucket about once for them all. `CLADEFSD_PEERS` names them at start,
//! comma-separated, each spelled as it names itself; the control API's `SetPeers` replaces them.
//! cladefs: peers authenticate each other by mutual TLS (board 8437): `CLADEFSD_PEER_TLS` names a
//! directory of the hosts' credentials, `ca.crt` (the hosts' CA), `peer.crt` and `peer.key` (this
//! host's certificate, for `cladefs-peer`, and its key, PEM), without which a daemon serving peers
//! refuses to start; every call, served or made, is of a peer whose certificate the CA signed. What
//! a peer gives is checked against the CRCs of the asker's own tail of the object; on any
//! failure a part comes from the bucket, and the peer is asked nothing for 30 seconds. cladefs:
//! peers are of one cloud region (TASK-633.4, backlog doc-23): a host's region is its home
//! bucket, and a peer refuses one of another region, or another system of buckets, which then
//! asks it nothing again, nor counts it among a part's owners, whatever its list says; within the
//! region, parts of any bucket are served, those of another region's buckets fetched from there
//! once by their owner here.
//! However far behind the bucket its cells fall, what waits for it is on the host's disk, each
//! cell's write-ahead log, bounded by its lag (backlog doc-7, Uploads); what of it the daemon
//! holds in memory is what it uploads at once, `CLADEFSD_UPLOAD_MEMORY` bytes at most (512 MiB
//! by default, 64 MiB at least: an extent, which goes alone), every cell's together (TASK-317).
//! With `CLADEFSD_STATS` naming a file too, the daemon keeps there what it did with the
//! bucket, its requests by kind and its bytes up and down
//! (`bucket_cell::object_bucket::Stats::line`), what serving the
//! cells cost it (`cladefs::stats::Stats`), and the VM processes' requests it served, by kind,
//! rewritten as they change.
//!
//! With `CLADEFSD_READ_TRACES` naming a directory, the daemon writes there a trace of what each VM
//! read of its cell in its first two minutes, its `READ`s and its DAX window's faults, as its VM
//! process goes (`cladefs::cladefsd::reads`, TASK-702.1): what `scripts/read_prediction.py`
//! compares predictions of a version's reads on.
//!
//! With `CLADEFSD_TRACES` set to `otlp`, the daemon exports spans of what it does, over OTLP,
//! to the collector `OTEL_EXPORTER_OTLP_ENDPOINT` names, OpenTelemetry's variables configuring
//! them (`cladefs::cladefsd::otel`, TASK-258; `docs/tracing.md`): off otherwise.
//!
//! It logs to standard error, a line a record (`cladefs::cladefsd::logging`): text by default, its
//! time first and its control characters escaped (board 8571), or, with `CLADEFS_LOG_FORMAT=json`,
//! a JSON object a line, of the record's `timestamp`, `level`, `target`, `message` and key-values
//! (TASK-259), and its span's `trace_id` and `span_id`. SIGQUIT logs its threads' stacks
//! (`cladefs::stacks`), on Linux.
//!
//! cladefs: it is a thin binary over libcladefs, the daemon a process embeds (board 13471), which
//! runs on Linux and macOS (board 13065): on macOS, without the FUSE mount, the memory governor's
//! cgroups and pressure, and VM processes' DAX windows (board 13069). `CLADEFS_LOG_LEVEL` says how much, `info` by
//! default, by target too: `info,bucket_cell::object_bucket=debug` logs each request of object
//! storage. A record about a VM's request names it, `request_id`, the guest's number of it, with
//! its `store` (TASK-260).

// cladefs: Linux release binaries of cladefsd are static musl builds only (with mimalloc) so one
// binary per architecture runs across Ubuntu, Debian, Fedora Asahi, etc. with zero glibc version
// coupling. Debug/sim/test builds still compile for the host target for nextest and clippy.
#[cfg(all(target_os = "linux", not(target_env = "musl"), not(debug_assertions)))]
compile_error!(
    "Linux release builds of cladefsd must target musl (use scripts/build-musl.sh or \
     --target <arch>-unknown-linux-musl)"
);

// cladefs: musl's default mallocng allocator contends heavily under multi-threaded allocation;
// mimalloc uses lock-free per-thread heaps and detects OS page size at runtime so a single static
// arm64 binary runs across 4 KiB (Ubuntu/Debian) and 16 KiB (Fedora Asahi) kernels.
#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(not(madsim))]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let config_path: Option<std::path::PathBuf> = match raw_args.as_slice() {
        [flag, path] if flag == "--config" || flag == "-c" => Some(path.into()),
        [flag] if flag.starts_with("--config=") => {
            Some(flag.trim_start_matches("--config=").into())
        }
        [] => std::env::var_os(cladefs::config::CONFIG_ENV)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from),
        _ => None,
    };
    let toml_config = match config_path {
        Some(path) => {
            let cfg = cladefs::config::Config::from_toml_file(&path).map_err(anyhow::Error::msg)?;
            cfg.validate_for_daemon().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                cfg.daemon.vm_socket.is_some(),
                "daemon.vm_socket is required in TOML config"
            );
            // SAFETY: single-threaded startup before any background thread or runtime is spawned.
            unsafe { cfg.apply_process_env() };
            Some(libcladefs::Config::from_file(&path)?)
        }
        None => None,
    };
    cladefs::cladefsd::logging::to_stderr()?;
    if std::env::args().nth(1).as_deref() == Some("--setup-host") {
        return setup_host();
    }
    if std::env::args().nth(1).as_deref() == Some("--doctor") {
        return doctor(std::env::args().nth(2));
    }
    // cladefs: its large buffers given back as they are freed, before any thread is made
    // (TASK-317).
    cladefs::cladefsd::memory::return_large_allocations();
    // cladefs: a panic holding a lock shared beyond one cell, which no halt contains, stops the
    // daemon, rather than poison the lock for every user after (TASK-149.24).
    local_cell::shared::abort_on_panics();
    // cladefs: the failpoints FAILPOINTS sets, in a daemon built with them (TASK-44.10).
    let _failpoints = fail::FailScenario::setup();
    cladefs::faults::from_seed().map_err(anyhow::Error::msg)?;
    // cladefs: before any thread is started, which SIGQUIT would end otherwise (TASK-44.13).
    #[cfg(target_os = "linux")]
    cladefs::stacks::on_sigquit().context("handling SIGQUIT")?;
    // cladefs: the daemon libcladefs embeds, this process's (board 13471), nothing of it started
    // before the signals that drain it are blocked.
    let daemon = libcladefs::Daemon::new()?;
    let drain = cladefs::cladefsd::drain::bound().map_err(anyhow::Error::msg)?;
    drain_on_signals(daemon.clone(), drain).context("handling SIGTERM, SIGINT and SIGHUP")?;
    // Start OTLP tracing if configured (TASK-258, TASK-707). Its exporter threads start after the
    // signals that drain it are blocked, as every thread but the drain's.
    use cladefs::cladefsd::otel::Tracing;
    match cladefs::cladefsd::otel::from_env().map_err(anyhow::Error::msg)? {
        Tracing::Off => {}
        Tracing::Exported => log::info!("cladefsd: exporting spans over OTLP"),
    }
    // cladefs: its boot run on the daemon's runtime, which raises the limit of open files first
    // (libcladefs).
    if let Some(cfg) = toml_config {
        futures::executor::block_on(daemon.boot(cfg))?;
    } else {
        let mut args = raw_args.into_iter();
        let socket = args
            .next()
            .context("usage: cladefsd --config PATH | cladefsd SOCKET [MEMORY_BYTES]")?;
        let memory = args.next().map(|memory| memory.parse::<u64>());
        let memory = memory.transpose().context("MEMORY_BYTES must be a number")?;
        futures::executor::block_on(daemon.boot_from_env(std::path::Path::new(&socket), memory))?;
    }
    // Serving stops only as the daemon drains, whose thread exits the process once it has
    // (`drain_on_signals`): not here, its cells still closing.
    loop {
        std::thread::park();
    }
}

/// `cladefsd --setup-host`: the host's settings the memory governor needs (Linux's).
#[cfg(not(madsim))]
fn setup_host() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return cladefs::cladefsd::governor::setup_host().map_err(anyhow::Error::msg);
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("cladefsd --setup-host: the memory governor's host settings are Linux's")
}

#[cfg(not(madsim))]
/// `cladefsd --doctor [MEMORY_BYTES]`: the host's limits and kernel features, as the daemon checks
/// them as it starts (`cladefsd::doctor`, board 14217), printed, the daemon started not: a memory
/// size given, as a daemon given one, the memory governor's prerequisites aren't checked. Fails if
/// the daemon would refuse to start.
fn doctor(memory: Option<String>) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let memory = memory.map(|memory| memory.parse::<u64>());
    let memory = memory.transpose().context("MEMORY_BYTES must be a number")?;
    // As a boot raises it first (libcladefs).
    cladefs::cladefsd::raise_open_files_limit().context("raising the limit of open files")?;
    let needs = cladefs::cladefsd::doctor::Needs { governed: memory.is_none() };
    let report = cladefs::cladefsd::doctor::examine(needs);
    print!("{report}");
    match report.refusal() {
        Some(refused) => anyhow::bail!("the extent daemon would refuse to start: {refused}"),
        None => Ok(()),
    }
}

/// Drains `daemon` on SIGTERM, as an orchestrator stops it, within `bound`, and exits
/// (`cladefs::cladefsd::drain`, TASK-141.4): 0 once it drained, 1 past its bound; cladefs: its last
/// spans exported within the bound too, as much as it leaves (board 8569). cladefs: on
/// SIGINT and SIGHUP too, whose default action ended it at once, a crash for every cell (board
/// 8567). They are blocked in every thread but one, made here first, which waits for them
/// (`cladefs::signals`), and, once draining, for none again.
#[cfg(not(madsim))]
fn drain_on_signals(daemon: libcladefs::Daemon, bound: std::time::Duration) -> std::io::Result<()> {
    let signals = cladefs::signals::Signals::block(&[libc::SIGTERM, libc::SIGINT, libc::SIGHUP])?;
    cladefs::signals::spawn_taking_no_signals("cladefsd signal", move || {
        // cladefs: its panic, as its drain's, aborts the daemon, which could drain no more
        // (board 8501).
        let _supervised = cladefs::cladefsd::supervised("cladefsd signal");
        // cladefs: unable to wait for them, it stops now, saying why (board 9414): its thread
        // returned, the daemon served on with nothing to drain it, which only SIGKILL then ended,
        // a crash for every cell, its log saying nothing of why at the time.
        let signal = match signals.wait() {
            Ok(signal) => signal,
            Err(error) => {
                log::error!(
                    "cladefsd: waiting for SIGTERM, SIGINT or SIGHUP: {error}: aborting, as \
                     nothing could drain it"
                );
                std::process::abort();
            }
        };
        let name = match signal {
            libc::SIGINT => "SIGINT",
            libc::SIGHUP => "SIGHUP",
            _ => "SIGTERM",
        };
        log::info!("cladefsd: draining, on {name}, within {}s", bound.as_secs());
        let began = std::time::Instant::now();
        let drained = futures::executor::block_on(daemon.stop(bound));
        // cladefs: the last spans exported within what is left of the bound, never past it,
        // whatever the collector (board 8569).
        cladefs::cladefsd::otel::shutdown(bound.saturating_sub(began.elapsed()));
        match drained {
            Ok(()) => {
                log::info!("cladefsd: drained in {:.1?}: every cell closed", began.elapsed());
                std::process::exit(0);
            }
            Err(left) => {
                log::error!("cladefsd: {left}");
                std::process::exit(1);
            }
        }
    })
}

#[cfg(madsim)]
fn main() {
    eprintln!("cladefsd runs outside the simulation only");
    std::process::exit(2);
}
