// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's core (doc-24, TASK-685.9): what a cladefsd is, configured by its
//! environment (`node_local::env`), as `cladefsd`'s `main` and its docs say, apart from what is
//! its process's: its logger, its signals, its panics and failpoints, its limits, its spans'
//! export. `main` sets those up and boots it; the full-stack simulation boots one in each
//! simulated host, each with its own environment.

use crate::Daemon;
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
    let host = cladefs_core::bucket::Host::from_env().map_err(anyhow::Error::msg)?;
    // cladefs: its KMS, which every cell is encrypted under: none chosen, it refuses to start,
    // naming CLADEFS_KMS and CLADEFS_MASTER_KEY (TASK-411.28).
    cladefs_core::bucket::require_kms_on(&daemon.host).map_err(anyhow::Error::msg)?;
    // cladefs: load and validate the home bucket's storage-version ratchet (`formats`, doc-28)
    // before opening ephemeral host state, peers, or serving any cell so a host never writes a
    // format newer than the bucket's gate and refuses to boot against a bucket ratcheted past it.
    load_format_gates(daemon).await?;
    // cladefs: the host checked by its doctor, every limit and kernel feature at once, refusing
    // what running without would make wrong (board 14217), outside the simulation, whose hosts
    // share its process's; then roots it can't resolve, and a part cache it can't open, missing,
    // locked, or unable to bypass the page cache, whether or not it has peers (TASK-406.1).
    #[cfg(not(madsim))]
    crate::doctor::at_boot(crate::doctor::Needs { governed: memory.is_none() })?;
    local_cell::upload::blocking(|| crate::server::resolve_roots_on(daemon))
        .map_err(anyhow::Error::msg)?;
    if daemon.disk_pool.get().is_none() {
        let legacy_roots: Option<Vec<std::path::PathBuf>> = if let Ok(Some(roots)) =
            daemon.roots.get().cloned().unwrap_or(Ok(None))
            && !roots.is_empty()
        {
            Some(roots.to_vec())
        } else if set_os("CLADEFSD_EPHEMERAL_CELLS").is_some() {
            let roots = local_cell::upload::blocking(crate::migration::target::cells_roots)
                .map_err(anyhow::Error::msg)?;
            Some(roots)
        } else {
            None
        };
        if let Some(roots) = legacy_roots {
            let pool = local_cell::upload::blocking(|| {
                crate::disks::DiskPool::from_roots(daemon.host.bucket_host(), &roots)
            })
            .map_err(anyhow::Error::msg)?;
            let reaped = local_cell::upload::blocking(|| pool.reap_staging());
            if reaped > 0 {
                log::info!("cladefsd: removed {reaped} leftover staging cell directories");
            }
            // OnceLock: keeps any pool already configured before boot_daemon ran.
            let _ = daemon.disk_pool.set(Arc::new(pool));
        }
    }
    // cladefs: a host's one daemon owns the disks its cells are on, each root locked as it
    // opens (`disks::own`), which a second daemon is refused: one owning none would serve cells at
    // any path, another daemon's too (review 170; board 14305).
    anyhow::ensure!(
        daemon.disk_pool.get().is_some(),
        "the extent daemon owns no disk: name its disks, CLADEFSD_ROOTS or [[disks]] of --config; \
         a host's one daemon owns the directories its cells are kept in"
    );
    local_cell::upload::blocking(|| crate::storage::open_part_cache(daemon))
        .map_err(anyhow::Error::msg)?;
    // cladefs: its appendable zonal fast WAL bucket, if configured: opened and append-probed at
    // boot so misconfigured buckets fail hard before serving any cell (doc-29).
    crate::storage::open_fast_wal(daemon).await.map_err(anyhow::Error::msg)?;
    // cladefs: where traces of what VMs read go, if anywhere (TASK-702.1).
    crate::reads::configure_on(daemon).map_err(anyhow::Error::msg)?;
    // cladefs: its disks governed, each but a headroom of it its, or as much as CLADEFSD_DISK_BYTES
    // says (doc-22, TASK-539.9).
    let disks = crate::disk::DiskConfig::from_env().map_err(anyhow::Error::msg)?;
    crate::disk::start_on(daemon, disks).map_err(anyhow::Error::msg)?;
    // cladefs: how long an ephemeral migration to this host is kept, heard nothing of (board 5734).
    crate::migration::idle().map_err(anyhow::Error::msg)?;
    // cladefs: where ephemeral cells migrated to it go (board 8447).
    if set_os("CLADEFSD_EPHEMERAL_CELLS").is_some() {
        let roots = local_cell::upload::blocking(crate::migration::target::cells_roots)
            .map_err(anyhow::Error::msg)?;
        let shown = roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(":");
        log::info!("cladefsd: migrated cells roots {shown}");
    }
    // cladefs: its ephemeral cells' directory, if it keeps any (doc-25, TASK-698.3): those it
    // handed over let go of (board 5730), and the references of those it holds no more retired,
    // before it serves.
    if let Some(dir) = set_os("CLADEFSD_EPHEMERAL") {
        let ephemeral = local_cell::upload::blocking(|| {
            cladefs_core::ephemeral_host::open_on(&daemon.host, Path::new(&dir))
        })?;
        let let_go = crate::migration::let_go_handed_over_on(daemon, &ephemeral).await?;
        log::info!("cladefsd: let go of {let_go} ephemeral cells handed over");
        let retired = ephemeral.retire_stale().await?;
        log::info!("cladefsd: retired {retired} references of ephemeral cells held no more");
    }
    // cladefs: the hosts sharing its bucket, which it serves parts and asks for theirs
    // (TASK-406.2).
    let peers = set("CLADEFSD_PEERS");
    if let Some(address) = set("CLADEFSD_PEER_ADDRESS") {
        let listening = listen(&address).with_context(|| format!("serving peers at {address}"))?;
        let peers = peers.iter().flat_map(|peers| peers.split(',')).filter(|peer| !peer.is_empty());
        crate::peers::start_on(daemon, &address, listening, peers)
            .await
            .map_err(anyhow::Error::msg)?;
    } else if peers.is_some() {
        anyhow::bail!("CLADEFSD_PEERS is set, but CLADEFSD_PEER_ADDRESS names no address");
    }
    // cladefs: background poller of the root bucket's global control-plane `events` object
    // (shredded tenants/cells, decommissioned hosts, peer list updates).
    crate::events::start_on(daemon)?;
    // cladefs: what its uploads may hold at once (TASK-317).
    if let Some(bytes) = set_os("CLADEFSD_UPLOAD_MEMORY") {
        // Not UTF-8, or not a number: the error below says what it must be.
        let bytes = bytes.to_str().and_then(|bytes| bytes.parse().ok());
        let bytes = bytes.context("CLADEFSD_UPLOAD_MEMORY must be a number of bytes")?;
        cladefs_core::bucket::set_upload_memory_on(&daemon.host, bytes)
            .map_err(anyhow::Error::msg)?;
    }
    if let Some(memory) = memory {
        cladefs_core::pages::configure_on(
            &daemon.host,
            cladefs_core::pages::Config::with_memory(memory),
        )?;
    } else {
        anyhow::ensure!(!cfg!(madsim), "a simulated host is given its memory");
        // cladefs: its memory a page cache, as large as the host's, governed by what is free
        // (TASK-358).
        let memory = crate::governor::memory_bytes().map_err(anyhow::Error::msg)?;
        cladefs_core::pages::configure_on(
            &daemon.host,
            cladefs_core::pages::Config::with_memory(memory),
        )?;
        crate::governor::start_on(daemon).map_err(anyhow::Error::msg)?;
    }
    let control = set_os("CLADEFSD_GRPC");
    // cladefs: its FUSE mount of cells, whose Mount service is on the control socket (TASK-475.2).
    // Under madsim, refused (`mount::start_on`): FUSE is outside the simulation.
    if let Some(mount) = crate::mount::Config::from_env().map_err(anyhow::Error::msg)? {
        anyhow::ensure!(
            control.is_some(),
            "CLADEFSD_MOUNT is set, but CLADEFSD_GRPC names no control socket to attach cells through"
        );
        crate::mount::start_on(daemon, &mount, host.bucket)?;
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
    crate::http::start_on(daemon)?;
    // cladefs: its VM socket bound last, once all else serves: that a VM process, or what waits
    // for the daemon, connects to it says the control API answers too.
    #[cfg(not(madsim))]
    let listener = crate::bind_owner_only(socket)
        .with_context(|| format!("listening at {}", socket.display()))?;
    #[cfg(madsim)]
    let _ = socket;
    // Its VM processes, on a thread of their own, until it drains.
    // cladefs: outside the simulation: the VM socket, the simulation's guests calling the
    // requests' handling in the process (doc-24).
    #[cfg(not(madsim))]
    {
        let serving = Arc::clone(daemon);
        crate::run::thread("cladefsd vms", move || crate::server::serve_on(&serving, listener))?;
    }
    #[cfg(madsim)]
    daemon.serving.send_replace(true);
    Ok(())
}

/// Boots `daemon` from a validated TOML [`crate::config::Config`] (hotbed question 547): opens its
/// managed [`super::disks::DiskPool`], part-cache shards, per-disk governor, peers/replicas,
/// optional FUSE mount, control API, HTTP metrics/z-pages, and VM socket.
pub(super) async fn boot_with_config(
    daemon: &Arc<Daemon>,
    config: cladefs_core::config::Config,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.daemon.vm_socket.is_some(),
        "daemon.vm_socket is required in TOML config"
    );
    boot_embedded(daemon, config).await
}

/// cladefs: boots `daemon` as [`boot_with_config`] does, embedded in a process of another's
/// (libcladefs, board 13471): a VM socket only if `daemon.vm_socket` names one, the process
/// serving VM processes in its own ways otherwise (`Daemon::connect_in_process`,
/// `Daemon::serve_vm_process`), and serving from its boot on.
pub(super) async fn boot_embedded(
    daemon: &Arc<Daemon>,
    config: cladefs_core::config::Config,
) -> anyhow::Result<()> {
    config.validate_for_daemon().map_err(anyhow::Error::msg)?;
    let socket = config.daemon.vm_socket.clone();
    let host = cladefs_core::bucket::Host::from_env().map_err(anyhow::Error::msg)?;
    cladefs_core::bucket::require_kms_on(&daemon.host).map_err(anyhow::Error::msg)?;
    load_format_gates(daemon).await?;
    // cladefs: the host checked by its doctor (board 14217), as `boot_daemon` has it.
    #[cfg(not(madsim))]
    crate::doctor::at_boot(crate::doctor::Needs { governed: config.memory.bytes.is_none() })?;

    let enable_part_cache = config.disk.part_cache;
    let disk_entries = config.disks.clone();
    let disk_defaults = config.disk.clone();
    let pool = Arc::new(
        local_cell::upload::blocking(|| {
            crate::disks::DiskPool::open(
                daemon.host.bucket_host(),
                &disk_entries,
                &disk_defaults,
                enable_part_cache,
            )
        })
        .map_err(anyhow::Error::msg)?,
    );
    let reaped = local_cell::upload::blocking(|| pool.reap_staging());
    if reaped > 0 {
        log::info!("cladefsd: removed {reaped} leftover staging cell directories");
    }
    let cell_roots: Arc<[std::path::PathBuf]> = pool.cell_roots().into();
    // OnceLock: boot_with_config initializes roots once for this Daemon instance.
    let _ = daemon.roots.set(Ok(Some(cell_roots)));
    // OnceLock: boot_with_config initializes disk_pool once for this Daemon instance.
    let _ = daemon.disk_pool.set(Arc::clone(&pool));

    if enable_part_cache {
        let cache_dirs = pool.part_cache_dirs();
        let cache_cap = config.disk.cache_cap.map_or(u64::MAX, |b| b.bytes());
        local_cell::upload::blocking(|| {
            crate::storage::open_part_cache_with(daemon, &cache_dirs, cache_cap)
        })
        .map_err(anyhow::Error::msg)?;
    }
    crate::storage::open_fast_wal(daemon).await.map_err(anyhow::Error::msg)?;

    crate::reads::configure_on(daemon).map_err(anyhow::Error::msg)?;
    let disk_cfg = crate::disk::DiskConfig::from_toml(&config.disk);
    crate::disk::start_on(daemon, disk_cfg).map_err(anyhow::Error::msg)?;
    crate::migration::idle().map_err(anyhow::Error::msg)?;

    if let Some(dir) = &config.disk.ephemeral_journal {
        let ephemeral = local_cell::upload::blocking(|| {
            cladefs_core::ephemeral_host::open_on(&daemon.host, dir)
        })?;
        let let_go = crate::migration::let_go_handed_over_on(daemon, &ephemeral).await?;
        log::info!("cladefsd: let go of {let_go} ephemeral cells handed over");
        let retired = ephemeral.retire_stale().await?;
        log::info!("cladefsd: retired {retired} references of ephemeral cells held no more");
    }

    if let Some(address) = &config.peers.address {
        let listening = listen(address).with_context(|| format!("serving peers at {address}"))?;
        crate::peers::start_on(daemon, address, listening, &config.peers.peers)
            .await
            .map_err(anyhow::Error::msg)?;
    }

    crate::events::start_on(daemon)?;

    if let Some(upload_mem) = config.bucket.upload_memory {
        cladefs_core::bucket::set_upload_memory_on(&daemon.host, upload_mem.bytes())
            .map_err(anyhow::Error::msg)?;
    }

    if let Some(memory) = config.memory.bytes {
        cladefs_core::pages::configure_on(
            &daemon.host,
            cladefs_core::pages::Config::with_memory(memory.bytes()),
        )?;
    } else {
        anyhow::ensure!(!cfg!(madsim), "a simulated host is given its memory");
        let memory = crate::governor::memory_bytes().map_err(anyhow::Error::msg)?;
        cladefs_core::pages::configure_on(
            &daemon.host,
            cladefs_core::pages::Config::with_memory(memory),
        )?;
        crate::governor::start_on(daemon).map_err(anyhow::Error::msg)?;
    }

    if let Some(mount_cfg) = &config.mount {
        anyhow::ensure!(
            config.daemon.control_socket.is_some(),
            "mount is configured, but daemon.control_socket is unset"
        );
        let mount = crate::mount::Config {
            mountpoint: mount_cfg.mountpoint.clone(),
            cell_dirs: pool.cell_roots(),
            allow_other: mount_cfg.allow_other,
        };
        crate::mount::start_on(daemon, &mount, host.bucket)?;
    }

    if let Some(control) = &config.daemon.control_socket {
        let tenants = match &config.daemon.tenants_dir {
            Some(dir) => dir.clone(),
            None => control.with_file_name("tenants"),
        };
        local_cell::upload::blocking(|| host_fs::create_dir_all_durably(&tenants))
            .with_context(|| format!("making {}", tenants.display()))?;
        serve_control(daemon, control.as_os_str(), tenants)?;
    }

    #[cfg(not(madsim))]
    crate::http::start_on(daemon)?;

    // cladefs: under madsim, which has no Unix sockets, its guests connect in memory.
    #[cfg(madsim)]
    let socket: Option<std::path::PathBuf> = socket.and(None);
    match socket {
        #[cfg(not(madsim))]
        Some(socket) => daemon.serve_vm_socket(&socket)?,
        _ => {
            daemon.serving.send_replace(true);
        }
    }
    Ok(())
}

/// Loads and validates the home bucket's `formats` ratchet (doc-28) before serving any cells.
async fn load_format_gates(daemon: &Arc<Daemon>) -> anyhow::Result<()> {
    let home_bucket = crate::storage::daemon_bucket_on(daemon)
        .map_err(|errno| anyhow::anyhow!("opening home bucket for format gates: {errno}"))?;
    match bucket_cell::gates::load_for_host(
        &*home_bucket.own(),
        daemon.host.bucket_host().ratchet(),
    )
    .await
    {
        Ok(Some(gates)) => {
            log::info!(
                "cladefsd: loaded home bucket format gates (storage={}, fxfs={})",
                gates.storage,
                gates.fxfs
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(err) => anyhow::bail!("loading home bucket format gates: {err}"),
    }
}

/// cladefs: what serves at `address`: a TCP listener bound there; under madsim, the address itself,
/// on the simulated network, which its server binds.
fn listen(address: &str) -> anyhow::Result<crate::peers::Listening> {
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
        let control = crate::bind_owner_only(control)
            .with_context(|| format!("listening at {}", control.display()))?;
        control.set_nonblocking(true)?;
        let serving = Arc::clone(daemon);
        crate::run::service(daemon, "cladefsd ctl", move || async move {
            if let Err(err) = crate::control::serve_control_async(serving, control, tenants).await {
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
            if let Err(err) = crate::control::serve_control_on(serving, address, tenants).await {
                log::error!("cladefsd: serving the control API: {err}");
            }
        }));
    }
    Ok(())
}
