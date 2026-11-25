// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's core (doc-24, TASK-685.9): what a cladefsd is, configured by its
//! environment (`node_local::env`), as `cladefsd`'s `main` and its docs say, apart from what is
//! its process's: its logger, its signals, its panics and failpoints, its limits, its spans'
//! export. `main` sets those up and boots it; the full-stack simulation boots one in each
//! simulated host, each with its own environment.

use super::Daemon;
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;

/// Boots the process's default daemon serving VM processes at `socket`.
#[cfg(all(target_os = "linux", not(madsim)))]
pub async fn boot(socket: &Path, memory: Option<u64>) -> anyhow::Result<()> {
    boot_daemon(Daemon::process(), socket, memory).await
}

/// Boots `daemon` serving VM processes at `socket`, its memory `memory` bytes, fixed, or, if
/// none, a page cache as large as the host's, governed (TASK-358), configured by its environment:
/// its bucket and placement token, its KMS, its part cache, its disks, its peers and replicas,
/// its uploads' memory, its FUSE mount, its control API and its metrics. Returns once it serves,
/// or why it can't; it serves until it drains (`super::drain`), its VM processes on a thread of
/// its own.
///
/// cladefs: under madsim, a simulated host's (doc-24): no VM socket, the simulation's guests
/// calling its requests' handling in the process; no FUSE mount nor metrics, outside the
/// simulation; its control API (`CLADEFSD_GRPC`) and its peers (`CLADEFSD_PEER_ADDRESS`) at
/// addresses of the simulated network; its memory given, the host's being the simulation's.
pub(super) async fn boot_daemon(
    daemon: &Arc<Daemon>,
    socket: &Path,
    memory: Option<u64>,
) -> anyhow::Result<()> {
    // Unset, or not UTF-8, which no value it takes is: none.
    let set = |name| node_local::env::var(name).ok().filter(|value| !value.is_empty());
    let set_os = |name| node_local::env::var_os(name).filter(|value| !value.is_empty());
    // A host of the multi-node system: the bucket and this host's placement token named.
    let host = crate::bucket::Host::from_env().map_err(anyhow::Error::msg)?;
    // cladefs: its KMS, which every cell is encrypted under: none chosen, it refuses to start,
    // naming CLADEFS_KMS and CLADEFS_MASTER_KEY (TASK-411.28).
    crate::bucket::require_kms_on(&daemon.host).map_err(anyhow::Error::msg)?;
    // cladefs: what only failed once it served refused now (board 8595): too few open files for
    // its cells, outside the simulation, whose hosts share its process's; a kernel that can't say
    // which process to kill of a VM process's socket (board 8533); roots it can't resolve; a part
    // cache it can't open, missing, locked, or unable to bypass the page cache, whether or not it
    // has peers (TASK-406.1).
    #[cfg(not(madsim))]
    super::require_open_files()?;
    super::server::require_peer_pidfds()?;
    local_cell::upload::blocking(|| super::server::resolve_roots_on(daemon))
        .map_err(anyhow::Error::msg)?;
    local_cell::upload::blocking(|| super::storage::open_part_cache(daemon))
        .map_err(anyhow::Error::msg)?;
    // cladefs: its appendable zonal fast WAL bucket, if configured: opened and append-probed at
    // boot so misconfigured buckets fail hard before serving any cell (doc-29).
    super::storage::open_fast_wal(daemon).await.map_err(anyhow::Error::msg)?;
    // cladefs: where traces of what VMs read go, if anywhere (TASK-702.1).
    super::reads::configure_on(daemon).map_err(anyhow::Error::msg)?;
    // cladefs: how long its replicated cells' members are down before they are replaced (doc-21).
    super::replace_after().map_err(anyhow::Error::msg)?;
    // cladefs: its disks governed, each but a headroom of it its, or as much as CLADEFSD_DISK_BYTES
    // says (doc-22, TASK-539.9).
    let disks = super::disk::DiskConfig::from_env().map_err(anyhow::Error::msg)?;
    super::disk::start_on(daemon, disks).map_err(anyhow::Error::msg)?;
    // cladefs: how long a migration to this host is kept, heard nothing of (board 5734, board
    // 11369): validated whether or not CLADEFSD_EPHEMERAL is set, as bucket-backed cells migrate
    // too.
    super::migration::idle().map_err(anyhow::Error::msg)?;
    // cladefs: where ephemeral cells migrated to it go (board 8447), also used as fallback root
    // for bucket-backed migrations when CLADEFSD_ROOTS is unset (board 11530): validate at boot
    // whether or not CLADEFSD_EPHEMERAL is set.
    if set_os("CLADEFSD_EPHEMERAL_CELLS").is_some() {
        let roots = local_cell::upload::blocking(super::migration::target::cells_roots)
            .map_err(anyhow::Error::msg)?;
        let shown = roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(":");
        log::info!("cladefsd: migrated cells roots {shown}");
    }
    // cladefs: its ephemeral cells' directory, if it keeps any (doc-25, TASK-698.3): those it
    // handed over let go of (board 5730), and the references of those it holds no more retired,
    // before it serves.
    if let Some(dir) = set_os("CLADEFSD_EPHEMERAL") {
        let ephemeral = local_cell::upload::blocking(|| {
            crate::ephemeral_host::open_on(&daemon.host, Path::new(&dir))
        })?;
        let let_go = super::migration::let_go_handed_over_on(daemon, &ephemeral).await?;
        log::info!("cladefsd: let go of {let_go} ephemeral cells handed over");
        let retired = ephemeral.retire_stale().await?;
        log::info!("cladefsd: retired {retired} references of ephemeral cells held no more");
    }
    // cladefs: drop any orphaned bucket-backed migration staging directories left by a prior crash
    // before serving (board 11530).
    let reaped_staging =
        local_cell::upload::blocking(|| super::migration::target::reap_orphaned_staging_on(daemon));
    if reaped_staging > 0 {
        log::info!("cladefsd: removed {reaped_staging} orphaned migration staging directories");
    }
    // cladefs: the hosts sharing its bucket, which it serves parts and asks for theirs
    // (TASK-406.2).
    let peers = set("CLADEFSD_PEERS");
    if let Some(address) = set("CLADEFSD_PEER_ADDRESS") {
        let listening = listen(&address).with_context(|| format!("serving peers at {address}"))?;
        let peers = peers.iter().flat_map(|peers| peers.split(',')).filter(|peer| !peer.is_empty());
        super::peers::start_on(daemon, &address, listening, peers)
            .await
            .map_err(anyhow::Error::msg)?;
    } else if peers.is_some() {
        anyhow::bail!("CLADEFSD_PEERS is set, but CLADEFSD_PEER_ADDRESS names no address");
    }
    // cladefs: background poller of the root bucket's global control-plane `events` object
    // (shredded tenants/cells, decommissioned hosts, peer list updates).
    super::events::start_on(daemon)?;
    // cladefs: what its uploads may hold at once (TASK-317).
    if let Some(bytes) = set_os("CLADEFSD_UPLOAD_MEMORY") {
        // Not UTF-8, or not a number: the error below says what it must be.
        let bytes = bytes.to_str().and_then(|bytes| bytes.parse().ok());
        let bytes = bytes.context("CLADEFSD_UPLOAD_MEMORY must be a number of bytes")?;
        crate::bucket::set_upload_memory_on(&daemon.host, bytes).map_err(anyhow::Error::msg)?;
    }
    if let Some(memory) = memory {
        crate::pages::configure_on(&daemon.host, crate::pages::Config::with_memory(memory))?;
    } else {
        anyhow::ensure!(!cfg!(madsim), "a simulated host is given its memory");
        // cladefs: its memory a page cache, as large as the host's, governed by what is free
        // (TASK-358).
        let memory = super::governor::memory_bytes().map_err(anyhow::Error::msg)?;
        crate::pages::configure_on(&daemon.host, crate::pages::Config::with_memory(memory))?;
        super::governor::start_on(daemon).map_err(anyhow::Error::msg)?;
    }
    let control = set_os("CLADEFSD_GRPC");
    // cladefs: its FUSE mount of cells, whose Mount service is on the control socket (TASK-475.2).
    // Under madsim, refused (`mount::start_on`): FUSE is outside the simulation.
    if let Some(mount) = super::mount::Config::from_env().map_err(anyhow::Error::msg)? {
        anyhow::ensure!(
            control.is_some(),
            "CLADEFSD_MOUNT is set, but CLADEFSD_GRPC names no control socket to attach cells through"
        );
        super::mount::start_on(daemon, &mount, host.bucket)?;
    }
    if let Some(control) = control {
        let tenants = match set_os("CLADEFSD_TENANTS") {
            Some(dir) => std::path::PathBuf::from(dir),
            None => Path::new(&control).with_file_name("tenants"),
        };
        local_cell::upload::blocking(|| host_fs::create_dir_all_durably(&tenants))
            .with_context(|| format!("making {}", tenants.display()))?;
        serve_control(daemon, &control, tenants)?;
    }
    // cladefs: its metrics and z-pages over HTTP, where CLADEFSD_METRICS and CLADEFSD_ZPAGES say
    // (board 8559); outside the simulation (doc-24).
    #[cfg(not(madsim))]
    super::http::start_on(daemon)?;
    // cladefs: its VM socket bound last, once all else serves: that a VM process, or what waits
    // for the daemon, connects to it says the control API answers too.
    #[cfg(not(madsim))]
    let listener = super::bind_owner_only(socket)
        .with_context(|| format!("listening at {}", socket.display()))?;
    #[cfg(madsim)]
    let _ = socket;
    // Its VM processes, on a thread of their own, until it drains.
    // cladefs: outside the simulation: the VM socket, the simulation's guests calling the
    // requests' handling in the process (doc-24).
    #[cfg(not(madsim))]
    {
        let serving = Arc::clone(daemon);
        super::run::thread("cladefsd vms", move || super::server::serve_on(&serving, listener))?;
    }
    #[cfg(madsim)]
    daemon.serving.send_replace(true);
    Ok(())
}

/// cladefs: what serves at `address`: a TCP listener bound there; under madsim, the address itself,
/// on the simulated network, which its server binds.
fn listen(address: &str) -> anyhow::Result<super::peers::Listening> {
    #[cfg(not(madsim))]
    return Ok(std::net::TcpListener::bind(address)?);
    #[cfg(madsim)]
    return Ok(address.parse()?);
}

/// cladefs: serves `daemon`'s control API at `control`, its tenants' sockets in `tenants`: as a
/// supervised task on `daemon`'s consolidated runtime (outside madsim) or on the simulated node's
/// runtime (under madsim).
fn serve_control(
    daemon: &Arc<Daemon>,
    control: &std::ffi::OsStr,
    tenants: std::path::PathBuf,
) -> anyhow::Result<()> {
    #[cfg(not(madsim))]
    {
        // It can do everything: its user's alone, as the VM socket (TASK-159).
        let control = super::bind_owner_only(control)
            .with_context(|| format!("listening at {}", control.display()))?;
        control.set_nonblocking(true)?;
        let serving = Arc::clone(daemon);
        super::run::service(daemon, "cladefsd ctl", move || async move {
            if let Err(err) = super::control::serve_control_async(serving, control, tenants).await {
                log::error!("cladefsd: serving the control API: {err}");
                std::process::exit(1);
            }
        })?;
    }
    #[cfg(madsim)]
    {
        let address = control.to_str().context("CLADEFSD_GRPC must be an address")?.parse()?;
        let serving = Arc::clone(daemon);
        drop(tokio::spawn(async move {
            if let Err(err) = super::control::serve_control_on(serving, address, tenants).await {
                log::error!("cladefsd: serving the control API: {err}");
            }
        }));
    }
    Ok(())
}
