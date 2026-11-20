// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's core (doc-24, TASK-685.9): what a cladefsd is, configured by its
//! environment (`node_local::env`), as `cladefsd`'s `main` and its docs say, apart from what is
//! its process's: its logger, its signals, its panics and failpoints, its limits, its spans'
//! export. `main` sets those up and boots it; the full-stack simulation boots one in each
//! simulated host, each with its own environment.

use anyhow::Context;
use std::path::Path;

/// Boots a daemon serving VM processes at `socket`, its memory `memory` bytes, fixed, or, if
/// none, a page cache as large as the host's, governed (TASK-358), configured by its environment:
/// its bucket and placement token, its KMS, its part cache, its disks, its peers and replicas,
/// its uploads' memory, its FUSE mount, its control API and its metrics. Returns once it serves,
/// or why it can't; it serves until it drains (`super::drain`).
pub fn boot(socket: &Path, memory: Option<u64>) -> anyhow::Result<()> {
    let set = |name| node_local::env::var(name).ok().filter(|value| !value.is_empty());
    let set_os = |name| node_local::env::var_os(name).filter(|value| !value.is_empty());
    // A host of the multi-node system: the bucket and this host's placement token named.
    let host = crate::bucket::Host::from_env().map_err(anyhow::Error::msg)?;
    // cladefs: its KMS, which every store is encrypted under: none chosen, it refuses to start, naming
    // CLADEFS_KMS and CLADEFS_MASTER_KEY (TASK-411.28).
    crate::bucket::require_kms().map_err(anyhow::Error::msg)?;
    // cladefs: its part cache's directory and budget, if it keeps one (TASK-406.1).
    super::cache_config().map_err(anyhow::Error::msg)?;
    // cladefs: how long its replicated cells' members are down before they are replaced (doc-21).
    super::replace_after().map_err(anyhow::Error::msg)?;
    // cladefs: its disks governed, each but a headroom of it its, or as much as CLADEFSD_DISK_BYTES
    // says (doc-22, TASK-539.9).
    let disks = super::disk::DiskConfig::from_env().map_err(anyhow::Error::msg)?;
    super::disk::start(disks).map_err(anyhow::Error::msg)?;
    // cladefs: the hosts sharing its bucket, which it serves parts and asks for theirs (TASK-406.2).
    let peers = set("CLADEFSD_PEERS");
    if let Some(address) = set("CLADEFSD_PEER_ADDRESS") {
        let listener = std::net::TcpListener::bind(&address)
            .with_context(|| format!("serving peers at {address}"))?;
        let peers = peers.iter().flat_map(|peers| peers.split(',')).filter(|peer| !peer.is_empty());
        super::peers::start(&address, listener, peers).map_err(anyhow::Error::msg)?;
    } else if peers.is_some() {
        anyhow::bail!("CLADEFSD_PEERS is set, but CLADEFSD_PEER_ADDRESS names no address");
    }
    // cladefs: what its uploads may hold at once (TASK-317).
    if let Some(bytes) = set_os("CLADEFSD_UPLOAD_MEMORY") {
        let bytes = bytes.to_str().and_then(|bytes| bytes.parse().ok());
        let bytes = bytes.context("CLADEFSD_UPLOAD_MEMORY must be a number of bytes")?;
        crate::bucket::set_upload_memory(bytes).map_err(anyhow::Error::msg)?;
    }
    if let Some(memory) = memory {
        crate::pages::configure(crate::pages::Config::with_memory(memory))?;
    } else {
        // cladefs: its memory a page cache, as large as the host's, governed by what is free
        // (TASK-358).
        let memory = super::governor::memory_bytes().map_err(anyhow::Error::msg)?;
        crate::pages::configure(crate::pages::Config::with_memory(memory))?;
        super::governor::start().map_err(anyhow::Error::msg)?;
    }
    let listener = super::bind_owner_only(socket)
        .with_context(|| format!("listening at {}", socket.display()))?;
    let control = set_os("CLADEFSD_GRPC");
    // cladefs: its FUSE mount of cells, whose Mount service is on the control socket (TASK-475.2).
    if let Some(mount) = super::mount::Config::from_env().map_err(anyhow::Error::msg)? {
        anyhow::ensure!(
            control.is_some(),
            "CLADEFSD_MOUNT is set, but CLADEFSD_GRPC names no control socket to attach cells through"
        );
        super::mount::start(&mount, host.bucket)?;
    }
    if let Some(control) = control {
        let tenants = match set_os("CLADEFSD_TENANTS") {
            Some(dir) => std::path::PathBuf::from(dir),
            None => Path::new(&control).with_file_name("tenants"),
        };
        std::fs::create_dir_all(&tenants)
            .with_context(|| format!("making {}", tenants.display()))?;
        // It can do everything: its user's alone, as the VM socket (TASK-159).
        let control = super::bind_owner_only(&control)
            .with_context(|| format!("listening at {}", control.display()))?;
        std::thread::Builder::new().name("cladefsd ctl".into()).spawn(move || {
            if let Err(err) = super::serve_control(control, tenants) {
                log::error!("cladefsd: serving the control API: {err}");
                std::process::exit(1);
            }
        })?;
    }
    if let Some(address) = set_os("CLADEFSD_METRICS") {
        let address = address.to_str().context("CLADEFSD_METRICS must be an address")?;
        let metrics = std::net::TcpListener::bind(address)
            .with_context(|| format!("serving metrics and z-pages at {address}"))?;
        std::thread::Builder::new().name("cladefsd http".into()).spawn(move || {
            if let Err(err) = super::serve_http(metrics) {
                log::error!("cladefsd: serving HTTP: {err}");
            }
        })?;
    }
    super::serve(listener);
    Ok(())
}
