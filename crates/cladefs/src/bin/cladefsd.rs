// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefsd, cladefs's daemon, one a host (TASK-475): serves the extent stores of the VMs on this
//! host, from one memory, to VM processes that connect to its socket (`cladefs::cladefsd`); and, if
//! told to, mounts tenants' cells with FUSE (`cladefs::cladefsd::mount`). A cell is held
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
//! stores' cells are kept in, and `CLADEFSD_TOKEN` its placement's fencing token, which
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
//! Attaching a cell (`Attach`; a cell of 0, a new Fxfs one, its filesystem made empty) takes it
//! from the daemon's bucket, at the placement of the token the call gives, the daemon's if none,
//! into a store at `STORES/TENANT/CELL`, an Fxfs cell shown at `MOUNT/TENANT/CELL` (doc-20),
//! TENANT and CELL in base 32, padded (`base32_id::Padded`); a
//! SQLite cell, or a VM's, is not shown. `Detach` takes a cell away once nothing uses
//! it, written back, or at once if forced; `List` says what is attached. The mount is the daemon's user's; with `CLADEFSD_MOUNT_ALLOW_OTHER=1`, other users may
//! use it as its permissions say (for a user other than root, `/etc/fuse.conf` must have
//! `user_allow_other`). Root mounts with mount(2), any other user through libfuse's
//! `fusermount3`. The drain closes it: unmounted lazily, its cells written back and their
//! stores closed once nothing uses it, within the drain's bound.
//!
//! With `CLADEFSD_METRICS` naming an address (`HOST:PORT`), the daemon serves its metrics
//! there, over HTTP, at `/metrics`, in Prometheus' text format (`cladefs::cladefsd::metrics`,
//! TASK-47); cladefs: and its z-pages under `/z/`, its spans by latency and CPU profiles
//! (`cladefs::cladefsd::zpages`, TASK-672): an address to bind to localhost, or a network only
//! operators reach.
//!
//! `CLADEFSD_BUCKET` names the bucket (`s3://bucket/prefix`, `gs://bucket/prefix`,
//! `az://container/prefix`) each store it opens keeps its cell in (backlog doc-7), with
//! credentials from the environment's `AWS_*`, `GOOGLE_*` or `AZURE_*` variables: durable commits are uploaded behind them, within a second. With
//! `CLADEFSD_PART_CACHE` naming a directory too, what stores read from the bucket is kept there,
//! sparse, as much as its stores leave of its filesystem, and kept across the daemon's restarts:
//! one daemon at a time has the directory. `CLADEFSD_PART_CACHE_BYTES` caps what it keeps there, in
//! bytes (TASK-406.1): those read least recently go first.
//!
//! The host's disks are governed as its memory is (backlog doc-22): every filesystem the daemon
//! keeps stores or its part cache on is its but for a headroom (`CLADEFSD_DISK_HEADROOM`, bytes or
//! `N%`; by default a twentieth of the filesystem, 4 GiB at least, half of it at most), and no more
//! than `CLADEFSD_DISK_BYTES` on all of them together, if set. The stores come first, the part cache
//! taking what they leave and giving it back at once; near the allowance, the stores' uploaders are
//! urged and each tenant held to its share by weight; past it, guests' writes are refused with
//! ENOSPC, while every store goes on committing (`docs/operating-a-host.md`).
//!
//! With `CLADEFSD_PEER_ADDRESS` naming this host's `HOST:PORT` among the hosts sharing its
//! bucket, and `CLADEFSD_PART_CACHE` a cache, the daemon serves them parts of the bucket's objects
//! there, over gRPC (`proto/cladefs/peer/v1/peer.proto`), and its stores' fetchers ask the host that
//! owns a part, by rendezvous hashing over them, before the bucket (TASK-406.2): a part many hosts
//! read comes from the bucket about once for them all. `CLADEFSD_PEERS` names them at start,
//! comma-separated, each spelled as it names itself; the control API's `SetPeers` replaces them.
//! The network is trusted: peers are unauthenticated, as every host reads the whole bucket anyway,
//! and what a peer gives is checked against the CRCs of the asker's own tail of the object; on any
//! failure a part comes from the bucket, and the peer is asked nothing for 30 seconds. cladefs:
//! peers are of one cloud region (TASK-633.4, backlog doc-23): a host's region is its home
//! bucket, and a peer refuses one of another region, or another system of buckets, which then
//! asks it nothing again, nor counts it among a part's owners, whatever its list says; within the
//! region, parts of any bucket are served, those of another region's buckets fetched from there
//! once by their owner here.
//! With `CLADEFSD_REPLICAS` naming a directory too, made if it is not there, the daemon is a
//! member of other hosts' replicated cells' configurations (backlog doc-21, TASK-537): it keeps
//! their writers' logs there until the bucket does, each cell's to the lag bound in bytes, and
//! sweeps what their heads name no more every ten minutes. The directory's incarnation, made with
//! it, names this host in their configurations: one made anew, its disk lost, is another member.
//! A writer of a replicated cell replaces a member down for `CLADEFSD_REPLACE_AFTER` seconds
//! (default 30), and, its configuration holding no quorum that long, none to be had, drops it to
//! f = 0 once the bucket holds what it told.
//! However far behind the bucket its stores fall, what waits for it is on the host's disk, each
//! store's write-ahead log, bounded by its lag (backlog doc-7, Uploads); what of it the daemon
//! holds in memory is what it uploads at once, `CLADEFSD_UPLOAD_MEMORY` bytes at most (512 MiB
//! by default, 64 MiB at least: an extent, which goes alone), every store's together (TASK-317).
//! With `CLADEFSD_STATS` naming a file too, the daemon keeps there what it did with the
//! bucket, its requests by kind and its bytes up and down
//! (`bucket_cell::object_bucket::Stats::line`), what the stores' followers are, what serving the
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
//! default, by target too: `info,bucket_cell::object_bucket=debug` logs each request of object
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
    local_cell::shared::abort_on_panics();
    // cladefs: the failpoints FAILPOINTS sets, in a daemon built with them (TASK-44.10).
    let _failpoints = fail::FailScenario::setup();
    cladefs::faults::from_seed().map_err(anyhow::Error::msg)?;
    // cladefs: before any thread is started, which SIGQUIT would end otherwise (TASK-44.13).
    cladefs::stacks::on_sigquit().context("handling SIGQUIT")?;
    let drain = cladefs::cladefsd::drain::bound().map_err(anyhow::Error::msg)?;
    drain_on_sigterm(drain).context("handling SIGTERM")?;
    // cladefs: its spans, recorded for /z/tracez (TASK-672.1), and over OTLP too if
    // CLADEFSD_TRACES says so (TASK-258): its exporter's threads made after SIGTERM is blocked, as
    // every thread but the drain's.
    if cladefs::cladefsd::otel::from_env().map_err(anyhow::Error::msg)? {
        log::info!("cladefsd: exporting spans over OTLP");
    }
    cladefs::cladefsd::raise_open_files_limit().context("raising the limit of open files")?;
    let mut args = std::env::args().skip(1);
    let socket = args.next().context("usage: cladefsd SOCKET [MEMORY_BYTES]")?;
    let memory = args.next().map(|memory| memory.parse::<u64>());
    let memory = memory.transpose().context("MEMORY_BYTES must be a number")?;
    // cladefs: its boot awaited at its outermost entry, the one place it blocks (doc-24).
    local_cell::upload::wait_on(cladefs::cladefsd::boot(std::path::Path::new(&socket), memory))?;
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
        let drained = local_cell::upload::wait_on(cladefs::cladefsd::drain::drain(bound));
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
