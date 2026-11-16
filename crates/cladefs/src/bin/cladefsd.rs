// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefsd, cladefs's daemon, one a host (TASK-475): serves the extent stores of the VMs on this
//! host, from one memory, to VM processes that connect to its socket (`cladefs::cladefsd`); and, if
//! told to, mounts tenants' cells with FUSE (`cladefs::cladefsd::mount`). A cell's vdisks are held
//! by this one process, whichever serves them.
//!
//! ```sh
//! cladefsd SOCKET [MEMORY_BYTES]
//! ```
//!
//! Its memory, which the sealed extents it caches are kept in, and VMs' DAX windows map read-only,
//! is a page cache (`cladefs::cladefsd::governor`, TASK-358): as much of the host's as is free, less a
//! headroom (`CLADEFSD_HEADROOM` bytes, by default a sixteenth of the host's memory, 1 GiB at
//! least), given back as the host's free memory falls, each tenant kept its share by weight
//! (`SetMemoryWeight`). MEMORY_BYTES, if given, fixes it at that instead, ungoverned, as a test's
//! daemon may have it. A store's
//! private pages, which copy-on-write DAX gives a VM's stores, are of the VM's memory, not of it
//! (backlog doc-13). Out of it, or past a limit the control API sets, the daemon evicts the cached
//! extents asked for longest ago, and fails what it cannot make room for with ENOMEM, rather than
//! stop (TASK-50.3). A VMM's extent device
//! connects to the socket its configuration names (cladefs's Firecracker's `cladefsd_socket`).
//!
//! It runs as a host of cladefs's multi-node system: `CLADEFSD_BUCKET` names the bucket its
//! stores' branches are kept in, and `CLADEFSD_TOKEN` its placement's fencing token, which
//! the system's control plane gives it, and which fences earlier placements' writes and deletes;
//! without them, it refuses to start (`cladefs::bucket::Host`, TASK-45). Tests and development use
//! an in-memory bucket (`memory:///NAME`).
//!
//! One daemon serves every VM on a host. Connections to SOCKET are of the default tenant: with
//! `CLADEFSD_ROOTS` naming directories, separated by colons, they name stores in them alone
//! (`cladefs::cladefsd`, Isolation). SOCKET is the daemon's user's alone, mode 0600: a VM process of
//! another user, as a jail's, connects to a tenant's socket, granted to its user, and never to this
//! one, mounted or not (TASK-155). The VM manager runs jails as another user than the daemon's, and
//! hands them SOCKET's path no more than it does the control socket's.
//!
//! With `CLADEFSD_GRPC` naming a path, the daemon serves its control API there too, gRPC over a
//! Unix socket (`cladefs::cladefsd::control`; `crates/cladefs/proto/cladefs/cladefsd/v1/cladefsd.proto`),
//! through which the host's VM manager makes, forks, syncs and deletes stores, and grants tenants
//! VM sockets of their own: whoever connects may do all of it, so that it is the daemon's user's
//! alone, mode 0600, as SOCKET is, whatever the umask (TASK-159): the VM manager runs as that
//! user, or as root. The tenants' sockets go in the directory `CLADEFSD_TENANTS` names, or
//! `tenants` beside the control socket, each mode 0660 from the first. The control socket serves
//! the standard gRPC health service too (`grpc.health.v1.Health`, TASK-48): SERVING while the
//! daemon accepts VM processes, NOT_SERVING as it starts and as it stops.
//!
//! SIGTERM drains it, and it exits (`cladefs::cladefsd::drain`, TASK-141.4): once the health service
//! reports NOT_SERVING, it takes no more VM processes nor control calls (`UNAVAILABLE`), closes
//! every VM process's connection, whose VM goes as at the daemon's exit, and closes their stores,
//! durable, and the rest it has open, and waits for their uploads: within `CLADEFSD_DRAIN`
//! seconds (60 by default), exiting 0; past them, it logs what is left and exits 1. A host is
//! upgraded so, its VMs moved off or stopped first (`docs/upgrading-a-host.md`).
//!
//! With `CLADEFSD_MOUNT` naming a mount point, a directory, and `CLADEFSD_MOUNT_STORES` a directory
//! for their stores, the daemon mounts a directory of tenants' cells there with FUSE, none at first
//! (TASK-302, TASK-364, TASK-475.2), and serves its Mount service on the control socket
//! (`proto/cladefs/mount/v1/mount.proto`; `cladefsctl mount`), which `CLADEFSD_GRPC` must name.
//! Attaching a cell (`Attach`; a cell of 0, a new one) takes it from the daemon's bucket, at the
//! placement of the token the call gives, the daemon's if none, and every Fxfs vdisk of it into
//! a store at `STORES/TENANT/CELL/VDISK`, shown at `MOUNT/TENANT/CELL/VDISK`, TENANT and CELL
//! in 16 hexadecimal digits; its other vdisks, its SQLite databases and what VMs attach, are not
//! shown. `CreateVdisk` makes a new Fxfs vdisk of a cell attached; `Detach` takes a cell's
//! vdisks away once nothing uses them, written back, or at once if forced; `List` says what is
//! attached. The mount is the daemon's user's; with `CLADEFSD_MOUNT_ALLOW_OTHER=1`, other users may
//! use it as its permissions say (for a user other than root, `/etc/fuse.conf` must have
//! `user_allow_other`). Root mounts with mount(2), any other user through libfuse's
//! `fusermount3`. The drain closes it: unmounted lazily, its vdisks written back and their
//! stores closed once nothing uses it, within the drain's bound.
//!
//! With `CLADEFSD_METRICS` naming an address (`HOST:PORT`), the daemon serves its metrics
//! there, over HTTP, at `/metrics`, in Prometheus' text format (`cladefs::cladefsd::metrics`,
//! TASK-47).
//!
//! `CLADEFSD_BUCKET` names the bucket (`s3://bucket/prefix`, `gs://bucket/prefix`,
//! `az://container/prefix`) each store it opens keeps its branch in (backlog doc-7), with
//! credentials from the environment's `AWS_*`, `GOOGLE_*` or `AZURE_*` variables: durable commits are uploaded behind them, within a second. With
//! `CLADEFSD_PART_CACHE` naming a directory too, what stores read from the bucket is kept there,
//! sparse, leaving 4 GiB of its filesystem free, and kept across the daemon's restarts: one daemon
//! at a time has the directory. `CLADEFSD_PART_CACHE_BYTES` caps what it keeps there, in bytes, as
//! a host whose cache shares a disk with its stores needs (TASK-406.1): those read least recently
//! go first.
//!
//! With `CLADEFSD_PEER_ADDRESS` naming this host's `HOST:PORT` among the hosts sharing its
//! bucket, and `CLADEFSD_PART_CACHE` a cache, the daemon serves them parts of the bucket's objects
//! there, over gRPC (`proto/cladefs/peer/v1/peer.proto`), and its stores' fetchers ask the host that
//! owns a part, by rendezvous hashing over them, before the bucket (TASK-406.2): a part many hosts
//! read comes from the bucket about once for them all. `CLADEFSD_PEERS` names them at start,
//! comma-separated, each spelled as it names itself; the control API's `SetPeers` replaces them.
//! The network is trusted: peers are unauthenticated, as every host reads the whole bucket anyway,
//! and what a peer gives is checked against the CRCs of the asker's own tail of the object; on any
//! failure a part comes from the bucket, and the peer is asked nothing for 30 seconds.
//! However far behind the bucket its stores fall, what waits for it is on the host's disk, each
//! store's write-ahead log, bounded by its lag (backlog doc-7, Uploads); what of it the daemon
//! holds in memory is what it uploads at once, `CLADEFSD_UPLOAD_MEMORY` bytes at most (512 MiB
//! by default, 64 MiB at least: an extent, which goes alone), every store's together (TASK-317).
//! With `CLADEFSD_STATS` naming a file too, the daemon keeps there what it did with the
//! bucket, its requests by kind and its bytes up and down
//! (`extent_bucket::object_bucket::Stats::line`), what the stores' followers are, what serving the
//! stores cost it (`cladefs::stats::Stats`), and the VM processes' requests it served, by kind,
//! rewritten as they change.
//!
//! With `CLADEFSD_TRACES` set to `otlp`, the daemon exports spans of what it does, over OTLP,
//! to the collector `OTEL_EXPORTER_OTLP_ENDPOINT` names, OpenTelemetry's variables configuring
//! them (`cladefs::cladefsd::otel`, TASK-258; `docs/tracing.md`): off otherwise.
//!
//! It logs to standard error, a line a record (`cladefs::cladefsd::logging`): text by default, or,
//! with `CLADEFS_LOG_FORMAT=json`, a JSON object a line, of the record's `timestamp`, `level`,
//! `target`, `message` and key-values (TASK-259). `CLADEFS_LOG_LEVEL` says how much, `info` by
//! default, by target too: `info,extent_bucket::object_bucket=debug` logs each request of object
//! storage. A record about a VM's request names it, `request_id`, the guest's number of it, with
//! its `store` (TASK-260).

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    cladefs::renamed::refuse_old_variables().map_err(anyhow::Error::msg)?;
    cladefs::cladefsd::logging::to_stderr()?;
    // cladefs: its large buffers given back as they are freed, before any thread is made (TASK-317).
    cladefs::cladefsd::memory::return_large_allocations();
    // cladefs: a panic holding a lock shared beyond one store, which no halt contains, stops the
    // daemon, rather than poison the lock for every user after (TASK-149.24).
    extent_store::shared::abort_on_panics();
    // cladefs: the failpoints FAILPOINTS sets, in a daemon built with them (TASK-44.10).
    let _failpoints = fail::FailScenario::setup();
    cladefs::faults::from_seed().map_err(anyhow::Error::msg)?;
    // cladefs: before any thread is started, which SIGQUIT would end otherwise (TASK-44.13).
    cladefs::stacks::on_sigquit().context("handling SIGQUIT")?;
    let drain = cladefs::cladefsd::drain::bound().map_err(anyhow::Error::msg)?;
    drain_on_sigterm(drain).context("handling SIGTERM")?;
    // cladefs: its spans, over OTLP, if CLADEFSD_TRACES says so (TASK-258): its exporter's
    // threads made after SIGTERM is blocked, as every thread but the drain's.
    if cladefs::cladefsd::otel::from_env().map_err(anyhow::Error::msg)? {
        log::info!("cladefsd: exporting spans over OTLP");
    }
    cladefs::cladefsd::raise_open_files_limit().context("raising the limit of open files")?;
    // A host of the multi-node system: the bucket and this host's placement token named.
    let host = cladefs::bucket::Host::from_env().map_err(anyhow::Error::msg)?;
    // cladefs: its KMS, which every store is encrypted under: none chosen, it refuses to start, naming
    // CLADEFS_KMS and CLADEFS_MASTER_KEY (TASK-411.28).
    cladefs::bucket::require_kms().map_err(anyhow::Error::msg)?;
    // cladefs: its part cache's directory and budget, if it keeps one (TASK-406.1).
    cladefs::cladefsd::cache_config().map_err(anyhow::Error::msg)?;
    // cladefs: the hosts sharing its bucket, which it serves parts and asks for theirs (TASK-406.2).
    let set = |var| std::env::var(var).ok().filter(|value| !value.is_empty());
    let peers = set("CLADEFSD_PEERS");
    if let Some(address) = set("CLADEFSD_PEER_ADDRESS") {
        let listener = std::net::TcpListener::bind(&address)
            .with_context(|| format!("serving peers at {address}"))?;
        let peers = peers.iter().flat_map(|peers| peers.split(',')).filter(|peer| !peer.is_empty());
        cladefs::cladefsd::peers::start(&address, listener, peers).map_err(anyhow::Error::msg)?;
    } else if peers.is_some() {
        anyhow::bail!("CLADEFSD_PEERS is set, but CLADEFSD_PEER_ADDRESS names no address");
    }
    // cladefs: what its uploads may hold at once (TASK-317).
    if let Some(bytes) = std::env::var_os("CLADEFSD_UPLOAD_MEMORY").filter(|b| !b.is_empty()) {
        let bytes = bytes.to_str().and_then(|bytes| bytes.parse().ok());
        let bytes = bytes.context("CLADEFSD_UPLOAD_MEMORY must be a number of bytes")?;
        cladefs::bucket::set_upload_memory(bytes).map_err(anyhow::Error::msg)?;
    }
    let mut args = std::env::args().skip(1);
    let socket = args.next().context("usage: cladefsd SOCKET [MEMORY_BYTES]")?;
    if let Some(memory) = args.next() {
        let memory: u64 = memory.parse().context("MEMORY_BYTES must be a number")?;
        cladefs::pages::configure(cladefs::pages::Config::with_memory(memory))?;
    } else {
        // cladefs: its memory a page cache, as large as the host's, governed by what is free
        // (TASK-358).
        let memory = cladefs::cladefsd::governor::memory_bytes().map_err(anyhow::Error::msg)?;
        cladefs::pages::configure(cladefs::pages::Config::with_memory(memory))?;
        cladefs::cladefsd::governor::start().map_err(anyhow::Error::msg)?;
    }
    let listener = cladefs::cladefsd::bind_owner_only(&socket)
        .with_context(|| format!("listening at {socket}"))?;
    let control = std::env::var_os("CLADEFSD_GRPC").filter(|path| !path.is_empty());
    // cladefs: its FUSE mount of cells, whose Mount service is on the control socket (TASK-475.2).
    if let Some(mount) = cladefs::cladefsd::mount::Config::from_env().map_err(anyhow::Error::msg)? {
        anyhow::ensure!(
            control.is_some(),
            "CLADEFSD_MOUNT is set, but CLADEFSD_GRPC names no control socket to attach cells through"
        );
        cladefs::cladefsd::mount::start(&mount, host.bucket)?;
    }
    if let Some(control) = control {
        let tenants = match std::env::var_os("CLADEFSD_TENANTS").filter(|dir| !dir.is_empty()) {
            Some(dir) => std::path::PathBuf::from(dir),
            None => std::path::Path::new(&control).with_file_name("tenants"),
        };
        std::fs::create_dir_all(&tenants)
            .with_context(|| format!("making {}", tenants.display()))?;
        // It can do everything: its user's alone, as the VM socket (TASK-159).
        let control = cladefs::cladefsd::bind_owner_only(&control)
            .with_context(|| format!("listening at {}", control.display()))?;
        std::thread::Builder::new().name("cladefsd ctl".into()).spawn(move || {
            if let Err(err) = cladefs::cladefsd::serve_control(control, tenants) {
                log::error!("cladefsd: serving the control API: {err}");
                std::process::exit(1);
            }
        })?;
    }
    if let Some(address) =
        std::env::var_os("CLADEFSD_METRICS").filter(|address| !address.is_empty())
    {
        let address = address.to_str().context("CLADEFSD_METRICS must be an address")?;
        let metrics = std::net::TcpListener::bind(address)
            .with_context(|| format!("serving metrics at {address}"))?;
        std::thread::Builder::new().name("cladefsd metric".into()).spawn(move || {
            if let Err(err) = cladefs::cladefsd::serve_metrics(metrics) {
                log::error!("cladefsd: serving metrics: {err}");
            }
        })?;
    }
    cladefs::cladefsd::serve(listener);
    // Serving stops only as the daemon drains, whose thread exits the process once it has
    // (`drain_on_sigterm`): not here, its stores still closing.
    loop {
        std::thread::park();
    }
}

/// Drains the daemon on SIGTERM, as an orchestrator stops it, within `bound`, and exits
/// (`cladefs::cladefsd::drain`, TASK-141.4): 0 once it drained, 1 past its bound. SIGTERM is blocked
/// in every thread but one, made here first, which waits for it (`cladefs::signals`).
#[cfg(target_os = "linux")]
fn drain_on_sigterm(bound: std::time::Duration) -> std::io::Result<()> {
    let sigterm = cladefs::signals::Signals::block(&[libc::SIGTERM])?;
    cladefs::signals::spawn_taking_no_signals("cladefsd signal", move || {
        if let Err(error) = sigterm.wait() {
            log::error!("cladefsd: waiting for SIGTERM: {error}");
            return;
        }
        log::info!("cladefsd: draining, on SIGTERM, within {}s", bound.as_secs());
        let began = std::time::Instant::now();
        let drained = cladefs::cladefsd::drain::drain(bound);
        cladefs::cladefsd::otel::shutdown();
        match drained {
            Ok(()) => {
                log::info!("cladefsd: drained in {:.1?}: every store closed", began.elapsed());
                std::process::exit(0);
            }
            Err(left) => {
                log::error!("cladefsd: not drained within {}s: {left}", bound.as_secs());
                std::process::exit(1);
            }
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("cladefsd runs on Linux only");
    std::process::exit(2);
}
