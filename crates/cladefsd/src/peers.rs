// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's part in the hosts sharing its bucket (TASK-406.2, `crate::bucket::peers`):
//! it serves them parts of the bucket's objects over gRPC, at `CLADEFSD_PEER_ADDRESS`
//! (`proto/cladefs/peer/v1/peer.proto`), and its cells' fetchers ask them for the parts they own,
//! `CLADEFSD_PEERS` naming them at its start and the control API's `SetPeers` at any time.
//! What it serves comes from the host's memory and part cache, which it needs, or from the bucket
//! once, however many hosts ask at once ([`crate::bucket::serving_peers`]).
//!
//! cladefs: peers are untrusted until they authenticate (board 8437, AGENTS.md's Trust boundaries):
//! the service and its clients authenticate each other by mutual TLS ([`Credentials`]), every call
//! GetPart's too, before any is served; a host holds at most so many connections of a peer and of
//! all ([`Connections`]), so many streams on each, and each call's messages to what it needs
//! ([`Limited`]). A replication call is admitted only if its cell's head in the bucket could have
//! asked it (board 8445, `local_cell::replica::admit`), read for the call; and a refusal says
//! nothing of this host's bucket.

use crate::Daemon;
use crate::grpc::Served;
use bucket_cell::object_bucket::RoutedBucket;
use cladefs_core::bucket::peers::{Asked, Link, Peers, Refusal, Region, admits};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use local_cell::HostDir;
use local_cell::replica::{self, Key, Refused, Replicas, Seen, Swept};
use proto::peer_client::PeerClient;
use proto::peer_server::PeerServer;
use proto::{
    AppendRequest, AppendResponse, DropLogsRequest, DropLogsResponse, FenceRequest, FenceResponse,
    GetPartRequest, GetPartResponse, IncarnationRequest, IncarnationResponse, LogEnd,
    ReadLogRequest, ReadLogResponse, ReplicaOf,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Response, Status};

/// What hosts sharing a bucket serve each other, generated from its .proto.
pub mod proto {
    pub use cladefs_proto::peer_v1::*;
}

/// How long a host waits to connect to a peer: on the hosts' own network, a peer slower than this
/// is as good as down.
const CONNECT: Duration = Duration::from_secs(1);

/// How long a host waits for a part a peer serves: one the peer fetches from the bucket itself,
/// behind others in its budget, takes the bucket's latency and more.
const DEADLINE: Duration = Duration::from_secs(5);

/// cladefs: how often a member's stream to or from a peer pings it, unanswered past [`CONNECT`]
/// a stream to a peer down (doc-21): a writer's appends have no deadline, a stream's life being
/// the configuration's. The server pings its connections as often (board 8575): a writer's host
/// lost without a word, its stream of appends, which the server only reads, ends so.
const KEEPALIVE: Duration = Duration::from_secs(5);

/// cladefs: how much of a log a member sends a take at once (`ReadLog`).
const READ_PIECE: u64 = 1 << 20;

/// cladefs: the largest piece of a log a take takes from a member (`ReadLog`, board 8453): a
/// member sends [`READ_PIECE`] at most, with its message's few bytes more.
const READ_PIECE_MOST: usize = READ_PIECE as usize + 64;

/// cladefs: the largest fence answer a take takes (board 8453): [`replica::LOGS_MOST`] logs, each
/// 24 bytes at most as protobuf encodes it (a tag and a length, then two tagged varints), and a
/// few bytes more. A member answering past it, by the million, is refused by the decoder before
/// anything of it is held.
const FENCE_ANSWER_MOST: usize = replica::LOGS_MOST * 24 + 64;

/// cladefs: the largest message the peer service sends, and takes of a hand-over, which names every
/// file of its cell (doc-21, doc-25): tonic's default, 4 MiB, refused one a little larger as
/// OUT_OF_RANGE (TASK-537.3.2's review).
pub(crate) const MESSAGE_MOST: usize = 64 << 20;

/// cladefs: the largest message of each call the peer service takes (board 8437), refused by its
/// decoder past it, before it is held whole ([`Limited`]): the calls naming a few numbers, a key or
/// a path (`PATH_MAX`), 64 KiB; a writer's append, a piece of a log
/// (`local_cell::replicate::PIECE`), with as much more; a migration's piece of a file
/// (`migration::PIECE`), the holes of a file's last piece beside it, as many as 12 MiB name; a
/// hand-over, [`MESSAGE_MOST`].
#[cfg(not(madsim))]
const SMALL_MOST: usize = 64 << 10;
#[cfg(not(madsim))]
const APPEND_MOST: usize = local_cell::replicate::PIECE + SMALL_MOST;
#[cfg(not(madsim))]
const FILE_MOST: usize = crate::migration::PIECE + (12 << 20);
#[cfg(not(madsim))]
const HAND_OVER_MOST: usize = MESSAGE_MOST;

/// cladefs: the most streams a peer's connection carries at once (board 8437): a writer host's
/// every replicated cell with this host for a member has a stream of appends on its one
/// connection (board 8575), which this leaves room for, its hundreds of cells and more.
#[cfg(not(madsim))]
const STREAMS_MOST: u32 = 8192;

/// cladefs: the most connections the peer service holds of one peer's address, and of all (board
/// 8437): a host reaches another on one channel for its members and one for parts (board 8575),
/// a few more as they are made again.
#[cfg(not(madsim))]
const PEER_CONNECTIONS_MOST: usize = 16;
#[cfg(not(madsim))]
const CONNECTIONS_MOST: usize = 1024;

/// cladefs: how long a member reads a cell's head for a call, at most (board 8445): past it the
/// call fails UNAVAILABLE, its writer trying it again, as a member down.
const HEAD_WITHIN: Duration = Duration::from_secs(2);

/// cladefs: the name every peer's certificate is for (board 8437): the hosts' CA signs only their
/// daemons', so that a certificate it signed is a host's, whatever its address.
#[cfg(not(madsim))]
const PEER_NAME: &str = "cladefs-peer";

/// cladefs: how often a member sweeps its replica directory, reading each cell's head (doc-21,
/// Sweeping stale replica logs).
const SWEEP_EVERY: Duration = Duration::from_secs(600);

/// cladefs: the format family versions (current write version and oldest readable version) this
/// daemon advertises when talking to peers (doc-28, board 10054): its cell directories'
/// (`local_cell::FORMAT`), its cell records' (`crate::cell_dir::FORMAT`), its replica directory's
/// (`local_cell::replica::FORMAT`), its extent frames' (`extent_frame::FORMAT`), and its bucket's
/// layout's (`bucket_cell::layout::FORMAT`). Receivers check each family in the direction data
/// flows ([`SupportedFormats`]), so a rolling upgrade across a version bump keeps serving whenever
/// the reading side reads what the writing/holding side produces.
pub(crate) fn current_formats() -> proto::Formats {
    formats_for_ratchet(formats::Ratchet::default_current())
}

/// cladefs: the format family versions (active gated write versions and oldest readable versions)
/// `daemon` advertises to peers (doc-28, board 10054): advertising the gated write versions lets
/// an upgraded host gated at version `V` replicate cleanly with older peers still on `V` until
/// the bucket is ratcheted.
pub(crate) fn current_formats_on(daemon: &Daemon) -> proto::Formats {
    formats_for_ratchet(daemon.host.bucket_host().ratchet())
}

fn formats_for_ratchet(ratchet: &formats::Ratchet) -> proto::Formats {
    // Branch on `ratchet.storage()` here whenever a future `storage >= 2` bump introduces a new
    // write version for any replicated family.
    let _storage = ratchet.storage();
    let mut versions = proto::Formats {
        cell_directory: local_cell::FORMAT.current,
        cell_records: cladefs_core::cell_dir::FORMAT.current,
        replica_directory: local_cell::replica::FORMAT.current,
        extent_frame: extent_frame::FORMAT.current,
        bucket_layout: bucket_cell::layout::FORMAT.current,
        cell_directory_oldest: local_cell::FORMAT.oldest,
        cell_records_oldest: cladefs_core::cell_dir::FORMAT.oldest,
        replica_directory_oldest: local_cell::replica::FORMAT.oldest,
        extent_frame_oldest: extent_frame::FORMAT.oldest,
        bucket_layout_oldest: bucket_cell::layout::FORMAT.oldest,
    };
    // A test's daemon of another build's cell directories: `peers::formats=return(15)`.
    if let Some(cell) = cell_directory_override() {
        versions.cell_directory = cell;
        versions.cell_directory_oldest = cell;
    }
    versions
}

/// cladefs: a test failpoint (`peers::formats=return(N)`) overriding this host's cell directory
/// version for both sending and receiving, simulating a peer of another build.
fn cell_directory_override() -> Option<u32> {
    fail::fail_point!("peers::formats", |value: Option<String>| {
        // Unparseable failpoint text falls back to 0, which is outside every format's range.
        Some(value.and_then(|v| v.parse::<u32>().ok()).unwrap_or(0))
    });
    None
}

/// Direction of a replication RPC for format compatibility checking (board 10054, hotbed review 89
/// finding 325).
#[derive(Clone, Copy, Debug)]
enum ReplicationCall {
    /// `Incarnation` and `Append`: writer -> member. This member must read the writer's write
    /// versions.
    Write,
    /// `Fence` and `ReadLog`: member -> taker. The taker's readable range `[oldest..=current]`
    /// must cover every version this member may hold (`self.oldest..=self.current`).
    Read,
    /// `DropLogs`: cleanup, called either by a writer dropping uploaded logs on a newer member
    /// (`Write`) or by a newer taker dropping superseded logs on an older member (`Read`).
    Drop,
}

/// cladefs: the format family ranges a peer receiver reads (doc-28, board 10054).
#[derive(Clone, Copy, Debug)]
struct SupportedFormats {
    cell_directory: formats::Format,
    cell_records: formats::Format,
    replica_directory: formats::Format,
    extent_frame: formats::Format,
    bucket_layout: formats::Format,
}

impl SupportedFormats {
    fn current() -> Self {
        let cell_directory = match cell_directory_override() {
            // Struct literal rather than `Format::new`: `scripts/format-freeze.py` requires
            // literal arguments to `Format::new`, and this is a test-only failpoint override.
            Some(version) => {
                formats::Format { name: local_cell::FORMAT.name, oldest: version, current: version }
            }
            None => local_cell::FORMAT,
        };
        Self {
            cell_directory,
            cell_records: cladefs_core::cell_dir::FORMAT,
            replica_directory: local_cell::replica::FORMAT,
            extent_frame: extent_frame::FORMAT,
            bucket_layout: bucket_cell::layout::FORMAT,
        }
    }

    /// Verifies that a taker's declared readable range `theirs_oldest..=theirs_current` is valid
    /// and covers every version this member may hold (`ours.oldest..=ours.current`).
    fn check_taker_covers(
        ours: formats::Format,
        theirs_oldest: u32,
        theirs_current: u32,
    ) -> Result<(), String> {
        if theirs_oldest == 0 || theirs_oldest > theirs_current {
            return Err(format!(
                "{} readable range {theirs_oldest}..={theirs_current} is invalid",
                ours.name
            ));
        }
        if theirs_oldest <= ours.oldest && ours.current <= theirs_current {
            Ok(())
        } else {
            Err(format!(
                "{} readable range {theirs_oldest}..={theirs_current} does not cover this \
                 member's {}..={}",
                ours.name, ours.oldest, ours.current
            ))
        }
    }

    /// Checks a writer -> member replication call (`Incarnation`, `Append`): this member must read
    /// the writer's `cell_directory`, `replica_directory`, `extent_frame`, and `bucket_layout`
    /// versions (not `cell_records`, which replica logs never carry).
    fn check_writer(&self, theirs: Option<proto::Formats>) -> Result<(), Status> {
        let Some(theirs) = theirs else {
            return Err(Status::failed_precondition(
                "the asker named no format versions (older build): refused",
            ));
        };
        let reject = |why: String| {
            Status::failed_precondition(format!("the asker's formats are not read here: {why}"))
        };
        self.cell_directory.check(theirs.cell_directory).map_err(|e| reject(e.to_string()))?;
        self.replica_directory
            .check(theirs.replica_directory)
            .map_err(|e| reject(e.to_string()))?;
        self.extent_frame.check(theirs.extent_frame).map_err(|e| reject(e.to_string()))?;
        self.bucket_layout.check(theirs.bucket_layout).map_err(|e| reject(e.to_string()))?;
        Ok(())
    }

    /// Checks a member -> taker replication call (`Fence`, `ReadLog`): the taker's readable range
    /// must cover every version this member may hold of `cell_directory`, `replica_directory`,
    /// `extent_frame`, and `bucket_layout` (hotbed review 89 finding 325).
    fn check_taker(&self, theirs: Option<proto::Formats>) -> Result<(), Status> {
        let Some(theirs) = theirs else {
            return Err(Status::failed_precondition(
                "the asker named no format versions (older build): refused",
            ));
        };
        let reject = |why: String| {
            Status::failed_precondition(format!(
                "the asker does not read all formats this member holds: {why}"
            ))
        };
        Self::check_taker_covers(
            self.cell_directory,
            theirs.cell_directory_oldest,
            theirs.cell_directory,
        )
        .map_err(reject)?;
        Self::check_taker_covers(
            self.replica_directory,
            theirs.replica_directory_oldest,
            theirs.replica_directory,
        )
        .map_err(reject)?;
        Self::check_taker_covers(
            self.extent_frame,
            theirs.extent_frame_oldest,
            theirs.extent_frame,
        )
        .map_err(reject)?;
        Self::check_taker_covers(
            self.bucket_layout,
            theirs.bucket_layout_oldest,
            theirs.bucket_layout,
        )
        .map_err(reject)?;
        Ok(())
    }

    /// Checks a `DropLogs` call: accepted when either this member reads the writer's formats (a
    /// writer dropping uploaded logs on a newer member) or the taker reads every format this
    /// member holds (a newer taker dropping superseded logs on an older member).
    fn check_drop(&self, theirs: Option<proto::Formats>) -> Result<(), Status> {
        if self.check_writer(theirs).is_ok() || self.check_taker(theirs).is_ok() {
            Ok(())
        } else {
            self.check_writer(theirs)
        }
    }

    fn check_replication(
        &self,
        theirs: Option<proto::Formats>,
        call: ReplicationCall,
    ) -> Result<(), Status> {
        match call {
            ReplicationCall::Write => self.check_writer(theirs),
            ReplicationCall::Read => self.check_taker(theirs),
            ReplicationCall::Drop => self.check_drop(theirs),
        }
    }

    /// Checks a live migration's prepare versions: `cell_directory`, `cell_records`,
    /// `extent_frame`, and `bucket_layout` (not `replica_directory`, which ephemeral cells do not
    /// use).
    fn check_migration(
        &self,
        migration: u64,
        theirs: Option<proto::Formats>,
    ) -> Result<(), Status> {
        let Some(theirs) = theirs else {
            return Err(Status::failed_precondition(format!(
                "migration {migration:x}: the source named no format versions (older build): \
                 refused"
            )));
        };
        let reject = |why: String| {
            Status::failed_precondition(format!(
                "migration {migration:x}: the source's formats are not read here: {why}"
            ))
        };
        self.cell_directory.check(theirs.cell_directory).map_err(|e| reject(e.to_string()))?;
        self.cell_records.check(theirs.cell_records).map_err(|e| reject(e.to_string()))?;
        self.extent_frame.check(theirs.extent_frame).map_err(|e| reject(e.to_string()))?;
        self.bucket_layout.check(theirs.bucket_layout).map_err(|e| reject(e.to_string()))?;
        Ok(())
    }
}

/// cladefs: whether `theirs`, a replication call's format versions, are compatible with this host
/// for `call` (`FAILED_PRECONDITION` if absent or incompatible; doc-28, board 10054).
fn check_replication_formats(
    theirs: Option<proto::Formats>,
    call: ReplicationCall,
) -> Result<(), Status> {
    SupportedFormats::current().check_replication(theirs, call)
}

/// cladefs: whether `theirs`, a live migration prepare's format versions, are within what this
/// host reads (`FAILED_PRECONDITION` if absent or outside range; doc-28, board 10054).
pub(crate) fn check_migration_formats(
    migration: u64,
    theirs: Option<proto::Formats>,
) -> Result<(), Status> {
    SupportedFormats::current().check_migration(migration, theirs)
}

/// cladefs: the most a member holds of a configuration's logs, in bytes: the lag bound a writer's
/// bytes are kept within, which its replica directory holds each configuration to, and a take
/// holds each member's fence answer to ([`fence_answer`], board 8453).
fn members_most() -> u64 {
    local_cell::upload::Options::default().max_lag_bytes
}

/// The parts the daemon served its peers, and those it had not as they asked.
// The process's count, every simulated host's together, as a process's metrics are.
static SERVED: AtomicU64 = AtomicU64::new(0);
// The process's count, every simulated host's together, as a process's metrics are.
static SERVED_MISSING: AtomicU64 = AtomicU64::new(0);
/// cladefs: the stored bytes of the parts the daemon served its peers.
// The process's count, every simulated host's together, as a process's metrics are.
static SERVED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Has `daemon` serve the hosts sharing its bucket, at `address` (`CLADEFSD_PEER_ADDRESS`),
/// on `listening`, bound there (or, under madsim, its address on the simulated network), and its
/// cells' fetchers ask them, `peers` at first (`CLADEFSD_PEERS`), for the parts they own: before
/// any cell opens. It needs the host's part cache, which keeps what it serves.
pub(super) async fn start_on(
    daemon: &Arc<Daemon>,
    address: &str,
    listening: Listening,
    peers: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(), String> {
    let peers: Vec<String> = peers.into_iter().map(|peer| peer.as_ref().to_owned()).collect();
    for peer in std::iter::once(address).chain(peers.iter().map(String::as_str)) {
        check_address(peer)?;
    }
    let url = cladefs_core::bucket::Host::from_env()?.bucket;
    let failed = |what: &str| format!("serving peers: {what}");
    // cladefs: the peers' credentials, which every connection's both sides authenticate with: none
    // given, or none that parse, the daemon refuses to start (board 8437).
    credentials_on(daemon).map_err(|err| failed(&err))?;
    let bucket =
        crate::storage::daemon_bucket_on(daemon).map_err(|err| failed(&format!("{err:?}")))?;
    let cache = crate::storage::part_cache(daemon).map_err(|err| failed(&format!("{err:?}")))?;
    let cache = cache.ok_or_else(|| {
        "CLADEFSD_PEER_ADDRESS is set, but CLADEFSD_PART_CACHE names no directory for the \
         part cache, which keeps what the daemon serves its peers"
            .to_owned()
    })?;
    // cladefs: its region, which peers of another never serve, nor it them (TASK-633.4).
    let region = cladefs_core::bucket::peers::region(&*bucket).await;
    let region = region.map_err(|err| failed(&err.to_string()))?;
    let linked = url.clone();
    let runtime = cladefs_core::bucket::bucket_runtime_on(&daemon.host)
        .map_err(|err| failed(&err.to_string()))?;
    let linking_daemon = Arc::clone(daemon);
    let asking = Peers::new(address, move |peer| link(&linking_daemon, peer, (&linked, region)));
    asking.set(peers);
    cladefs_core::bucket::ask_peers_on(&daemon.host, bucket.clone(), asking)?;
    let replicas = match node_local::env::var_os("CLADEFSD_REPLICAS").filter(|v| !v.is_empty()) {
        Some(raw) => {
            let dirs: Vec<std::path::PathBuf> =
                std::env::split_paths(&raw).filter(|p| !p.as_os_str().is_empty()).collect();
            if dirs.is_empty() {
                None
            } else {
                Some(
                    local_cell::upload::blocking(|| replicas(daemon.host.local(), &dirs))
                        .map_err(|err| failed(&err))?,
                )
            }
        }
        None => {
            let pool_dirs = daemon.disk_pool().map(|pool| pool.replica_dirs()).unwrap_or_default();
            if pool_dirs.is_empty() {
                None
            } else {
                Some(
                    local_cell::upload::blocking(|| replicas(daemon.host.local(), &pool_dirs))
                        .map_err(|err| failed(&err))?,
                )
            }
        }
    };
    if let Some(replicas) = &replicas {
        sweeping(replicas.clone(), bucket.clone(), &runtime);
        let members = ReplicaMembers {
            bucket: url.clone(),
            me: address.to_owned(),
            incarnation: replicas.incarnation(),
            daemon: Arc::clone(daemon),
        };
        cladefs_core::bucket::replicate_with_on(&daemon.host, Arc::new(members))?;
    }
    let heads = bucket.clone();
    let serving = cladefs_core::bucket::serving_peers_on(&daemon.host, bucket, cache);
    let serving = serving.map_err(|err| failed(&err.to_string()))?;
    let server = Server {
        daemon: Arc::clone(daemon),
        bucket: url,
        region,
        serving,
        replicas,
        heads,
        appending: Arc::default(),
    };
    #[cfg(not(madsim))]
    listening.set_nonblocking(true).map_err(|err| failed(&err.to_string()))?;
    // cladefs: on `daemon`'s consolidated runtime, or under madsim a task (`super::run`, doc-24
    // step 12). An accept that fails for good, the listener's, stops the daemon, saying so (board
    // 8573).
    let serving = crate::run::service(daemon, "cladefsd peers", move || async move {
        if let Err(err) = serve(listening, server).await {
            log::error!("cladefsd: serving peers: {err}");
            std::process::exit(1);
        }
    });
    serving.map_err(|err| failed(&err.to_string()))?;
    // cladefs: the migrations to it their sources abandoned, given up (board 5734, board 11369):
    // always spawned whenever peers are served, for both ephemeral and bucket-backed migrations.
    // Supervised: its panic aborts the daemon, which would keep them for ever (board 9414).
    let idle = crate::migration::idle().map_err(|err| failed(&err))?;
    let reaping = crate::migration::target::reap(Arc::clone(daemon), idle);
    drop(runtime.spawn(crate::run::supervising(REAPING, reaping)));
    Ok(())
}

/// Has the process's default daemon serve the hosts sharing its bucket ([`start_on`]).
#[cfg(not(madsim))]
pub async fn start(
    address: &str,
    listening: Listening,
    peers: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(), String> {
    start_on(Daemon::process(), address, listening, peers).await
}

/// cladefs: what the daemon serves its peers on: a TCP listener, bound; under madsim, its address
/// on the simulated network (doc-24's boundary).
#[cfg(not(madsim))]
pub type Listening = std::net::TcpListener;
/// cladefs: what the daemon serves its peers on, under madsim: its address on the simulated
/// network.
#[cfg(madsim)]
pub type Listening = std::net::SocketAddr;

/// Validates that `daemon` serves peers and every entry in `addresses` is `HOST:PORT`
/// (`SetPeers`, board 11711).
pub(super) fn check_on(daemon: &Daemon, addresses: &[String]) -> Result<(), cladefs_core::Errno> {
    let _peers =
        cladefs_core::bucket::peers_on(&daemon.host).ok_or(cladefs_core::Errno::ENOTSUP)?;
    for address in addresses {
        check_address(address).map_err(|err| {
            log::warn!("cladefsd: SetPeers: {err}");
            cladefs_core::Errno::EINVAL
        })?;
    }
    Ok(())
}

/// Has the hosts sharing the bucket on `daemon` be those at `addresses` if `generation` is newer
/// than the generation already installed locally (`SetPeers`, `events` poller; hotbed review 140
/// finding 520). Serialized under `daemon.peers_generation` so a racing `SetPeers` RPC cannot
/// overwrite a newer generation already applied by the poller. Returns `true` if applied.
pub(super) fn set_generation_on(
    daemon: &Daemon,
    generation: u64,
    addresses: Vec<String>,
) -> Result<bool, cladefs_core::Errno> {
    check_on(daemon, &addresses)?;
    let peers = cladefs_core::bucket::peers_on(&daemon.host).ok_or(cladefs_core::Errno::ENOTSUP)?;
    let mut current = daemon.peers_generation.lock().unwrap();
    if generation > 0 && generation <= *current {
        return Ok(false);
    }
    log::info!("cladefsd: peers: {}", addresses.join(","));
    peers.set(addresses.clone());
    if let Some(channels) = daemon.peer_channels.get() {
        channels.prune(&addresses);
    }
    if generation > *current {
        *current = generation;
    }
    Ok(true)
}

/// Whether `address` is a host's `HOST:PORT`, as a peer is reached at.
fn check_address(address: &str) -> Result<(), String> {
    let uri: tonic::codegen::http::Uri =
        format!("http://{address}").parse().map_err(|err| format!("peer {address:?}: {err}"))?;
    match (uri.host(), uri.port_u16(), uri.path()) {
        (Some(host), Some(_), "/")
            if !host.is_empty() && uri.authority().is_some_and(|at| at.as_str() == address) =>
        {
            Ok(())
        }
        _ => Err(format!("peer {address:?}: not HOST:PORT")),
    }
}

/// What a daemon's peers asked it, since its start.
pub(super) struct ServedCounts {
    /// Parts it served them.
    pub parts: u64,
    /// cladefs: the stored bytes of those parts.
    pub bytes: u64,
    /// Parts it had not as they asked.
    pub missing: u64,
}

/// What `daemon`'s cells' fetchers asked its peers, and what its peers asked it, if it serves
/// peers.
pub(super) fn counts_on(
    daemon: &Daemon,
) -> Option<(cladefs_core::bucket::peers::Counts, ServedCounts)> {
    let counts = cladefs_core::bucket::peers_on(&daemon.host)?.counts();
    let served = ServedCounts {
        parts: SERVED.load(Ordering::Relaxed),
        bytes: SERVED_BYTES.load(Ordering::Relaxed),
        missing: SERVED_MISSING.load(Ordering::Relaxed),
    };
    Some((counts, served))
}

/// `daemon`'s line in its stats file (`CLADEFSD_STATS`): its [`counts_on`].
pub(super) fn line_on(daemon: &Daemon) -> Option<String> {
    let (counts, served) = counts_on(daemon)?;
    let cladefs_core::bucket::peers::Counts { asked, given, given_bytes, missing, failed } = counts;
    let ServedCounts { parts: served, bytes: served_bytes, missing: served_missing } = served;
    Some(format!(
        "peers: asked={asked} given={given} given-bytes={given_bytes} missing={missing} \
         failed={failed} served={served} served-bytes={served_bytes} \
         served-missing={served_missing}"
    ))
}

/// Serves `server` to the peers that connect to `listening`, until accepting fails for good
/// (`super::run::accepting`): each connection with no delay, TCP's keepalives on, and pinged as a
/// member's client pings it (board 8575); so many of each peer's address and of all
/// ([`admitting`]).
async fn serve(listening: Listening, server: Server) -> std::io::Result<()> {
    #[cfg(not(madsim))]
    {
        let listener = tokio::net::TcpListener::from_std(listening)?;
        let incoming = tonic::transport::server::TcpIncoming::from(listener)
            .with_nodelay(Some(true))
            .with_keepalive(Some(KEEPALIVE))
            .with_keepalive_interval(Some(KEEPALIVE));
        serve_on(admitting(incoming, Arc::default()), server).await
    }
    #[cfg(madsim)]
    router(server)?.serve(listening).await.map_err(std::io::Error::other)
}

/// cladefs: the peer service of `server`, its connections pinged every [`KEEPALIVE`] and closed if
/// unanswered within [`CONNECT`], as a member's client pings its member (board 8575): a writer's
/// host lost without a word leaves no stream of appends, its task and its connection, for good.
/// Each connection authenticated both ways by mutual TLS ([`Credentials`], board 8437) before any
/// call of it is served, its streams [`STREAMS_MOST`] at most, each call's messages to what it
/// needs ([`Limited`]).
#[cfg(not(madsim))]
fn router(server: Server) -> std::io::Result<tonic::transport::server::Router> {
    let credentials = credentials_on(&server.daemon).map_err(std::io::Error::other)?;
    let mut builder = tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE))
        .http2_keepalive_timeout(Some(CONNECT))
        .max_concurrent_streams(Some(STREAMS_MOST))
        .tls_config(credentials.server)
        .map_err(std::io::Error::other)?;
    // cladefs: its calls counted and traced (board 8565, board 8569).
    Ok(builder.add_service(Served::new(Limited::new(server))))
}

/// cladefs: the peer service of `server` under madsim, whose network has no TLS, nor limits of
/// tonic's transport (doc-24's boundary): the credentials read as a host's are.
#[cfg(madsim)]
fn router(server: Server) -> std::io::Result<tonic::transport::server::Router> {
    credentials_on(&server.daemon).map_err(std::io::Error::other)?;
    let service = PeerServer::new(server)
        .max_decoding_message_size(MESSAGE_MOST)
        .max_encoding_message_size(MESSAGE_MOST);
    Ok(tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE))
        .http2_keepalive_timeout(Some(CONNECT))
        .add_service(Served::new(service)))
}

/// cladefs: the peer service, each call's messages decoded to what it needs at most (board 8437):
/// the service's one limit was 64 MiB, a fence's or a part's asking of that size decoded whole
/// before it was refused. Each call goes to the service of its limit, by its path.
#[cfg(not(madsim))]
#[derive(Clone)]
struct Limited {
    small: PeerServer<Server>,
    append: PeerServer<Server>,
    file: PeerServer<Server>,
    hand_over: PeerServer<Server>,
}

#[cfg(not(madsim))]
impl Limited {
    fn new(server: Server) -> Self {
        let server = Arc::new(server);
        let limited = |most: usize| {
            PeerServer::from_arc(server.clone())
                .max_decoding_message_size(most)
                .max_encoding_message_size(MESSAGE_MOST)
        };
        Self {
            small: limited(SMALL_MOST),
            append: limited(APPEND_MOST),
            file: limited(FILE_MOST),
            hand_over: limited(HAND_OVER_MOST),
        }
    }
}

#[cfg(not(madsim))]
impl tonic::server::NamedService for Limited {
    const NAME: &'static str = <PeerServer<Server> as tonic::server::NamedService>::NAME;
}

#[cfg(not(madsim))]
impl<B> tower::Service<tonic::codegen::http::Request<B>> for Limited
where
    PeerServer<Server>: tower::Service<tonic::codegen::http::Request<B>>,
{
    type Response =
        <PeerServer<Server> as tower::Service<tonic::codegen::http::Request<B>>>::Response;
    type Error = <PeerServer<Server> as tower::Service<tonic::codegen::http::Request<B>>>::Error;
    type Future = <PeerServer<Server> as tower::Service<tonic::codegen::http::Request<B>>>::Future;

    /// Ready, as each service is, always.
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: tonic::codegen::http::Request<B>) -> Self::Future {
        let service = match request.uri().path() {
            "/cladefs.peer.v1.Peer/Append" => &mut self.append,
            "/cladefs.peer.v1.Peer/MigrateFile" => &mut self.file,
            "/cladefs.peer.v1.Peer/MigrateHandOver" => &mut self.hand_over,
            _ => &mut self.small,
        };
        service.call(request)
    }
}

/// cladefs: how the daemon's peer service and its clients authenticate each other (board 8437,
/// hotbed question 486): by mutual TLS, each side's certificate signed by the hosts' CA, for
/// [`PEER_NAME`], from the directory `CLADEFSD_PEER_TLS` names: `ca.crt`, the CA's certificate,
/// `peer.crt` and `peer.key`, the host's certificate and key, in PEM. Required of a daemon serving
/// peers: none, or none that parse, refuses its start, as no call of a peer's is served or made in
/// the clear. GetPart's too: its parts are the bucket's, but each a fetch from the bucket a peer
/// asks this host to make for it.
#[derive(Clone)]
pub(crate) struct Credentials {
    #[cfg(not(madsim))]
    server: tonic::transport::ServerTlsConfig,
    #[cfg(not(madsim))]
    client: tonic::transport::ClientTlsConfig,
}

impl Credentials {
    /// The credentials in `dir`, as [`Credentials`] says.
    pub(crate) fn from_dir(dir: &std::path::Path) -> Result<Self, String> {
        let read = |name: &str| {
            let path = dir.join(name);
            // The host's configuration, read once, outside the simulation as its environment is.
            std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))
        };
        Self::of(&read("ca.crt")?, &read("peer.crt")?, &read("peer.key")?)
    }

    /// The credentials of the CA's certificate `ca`, the host's certificate `cert` and its key
    /// `key`, in PEM: parsed now, so that ones that don't parse refuse the start, not each call.
    #[cfg(not(madsim))]
    fn of(ca: &[u8], cert: &[u8], key: &[u8]) -> Result<Self, String> {
        use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};
        let identity = Identity::from_pem(cert, key);
        let ca = Certificate::from_pem(ca);
        let server = ServerTlsConfig::new()
            .identity(identity.clone())
            .client_ca_root(ca.clone())
            .timeout(DEADLINE);
        let client =
            ClientTlsConfig::new().ca_certificate(ca).identity(identity).domain_name(PEER_NAME);
        tonic::transport::Server::builder()
            .tls_config(server.clone())
            .map_err(|err| format!("the peer service's credentials: {err:?}"))?;
        Endpoint::from_static("https://peer")
            .tls_config(client.clone())
            .map_err(|err| format!("the peer clients' credentials: {err:?}"))?;
        Ok(Self { server, client })
    }

    /// The credentials under madsim, whose network has no TLS (doc-24's boundary): read as a
    /// host's are, used not.
    #[cfg(madsim)]
    fn of(_: &[u8], _: &[u8], _: &[u8]) -> Result<Self, String> {
        Ok(Self {})
    }
}

/// cladefs: `daemon`'s peer credentials, from the directory `CLADEFSD_PEER_TLS` names, read the
/// first time they are asked for: why not, if it names none, or what it holds is no credentials.
pub(crate) fn credentials_on(daemon: &Daemon) -> Result<Credentials, String> {
    let mut read = daemon.peer_credentials.lock().unwrap();
    if let Some(credentials) = &*read {
        return Ok(credentials.clone());
    }
    let dir = node_local::env::var_os("CLADEFSD_PEER_TLS").filter(|dir| !dir.is_empty());
    let dir = dir.ok_or_else(|| {
        "CLADEFSD_PEER_TLS names no directory of the hosts' peer credentials (ca.crt, peer.crt, \
         peer.key): peers authenticate each other by mutual TLS, never in the clear (board 8437)"
            .to_owned()
    })?;
    let dir = std::path::Path::new(&dir);
    let credentials = Credentials::from_dir(dir)
        .map_err(|err| format!("the peer credentials in {}: {err}", dir.display()))?;
    *read = Some(credentials.clone());
    Ok(credentials)
}

/// cladefs: an endpoint of the peer at `address`, authenticating it, and authenticated by it, with
/// `daemon`'s credentials ([`Credentials`]): under madsim, whose network has no TLS, in the
/// clear.
fn endpoint_on(daemon: &Daemon, address: &str) -> Result<Endpoint, String> {
    let credentials = credentials_on(daemon)?;
    #[cfg(not(madsim))]
    {
        let endpoint = Endpoint::from_shared(format!("https://{address}"))
            .map_err(|err| format!("peer {address:?}: {err}"))?;
        endpoint.tls_config(credentials.client).map_err(|err| format!("peer {address:?}: {err}"))
    }
    #[cfg(madsim)]
    {
        // Under madsim `Credentials` is a non-Drop unit struct; `credentials()?` above still
        // enforces `CLADEFSD_PEER_TLS`.
        let _ = credentials;
        Endpoint::from_shared(format!("http://{address}"))
            .map_err(|err| format!("peer {address:?}: {err}"))
    }
}

#[cfg(all(test, not(madsim)))]
fn endpoint(address: &str) -> Result<Endpoint, String> {
    endpoint_on(Daemon::process(), address)
}

/// cladefs: the connections the peer service holds, by their peers' addresses (board 8437).
#[cfg(not(madsim))]
#[derive(Default)]
struct Connections {
    open: Mutex<HashMap<std::net::IpAddr, usize>>,
    refused: AtomicU64,
}

/// cladefs: a connection the peer service holds, counted until it goes.
#[cfg(not(madsim))]
struct Admitted<IO> {
    io: IO,
    peer: std::net::IpAddr,
    connections: Arc<Connections>,
}

#[cfg(not(madsim))]
impl Connections {
    /// Admits a connection of `peer`, if it holds fewer than [`PEER_CONNECTIONS_MOST`] of its
    /// and [`CONNECTIONS_MOST`] in all.
    fn admit<IO>(self: &Arc<Self>, io: IO, peer: std::net::IpAddr) -> Option<Admitted<IO>> {
        let mut open = self.open.lock().unwrap();
        let all: usize = open.values().sum();
        let of = open.entry(peer).or_default();
        if *of >= PEER_CONNECTIONS_MOST || all >= CONNECTIONS_MOST {
            if *of == 0 {
                open.remove(&peer);
            }
            drop(open);
            // Said as refusals begin, and each time they double, a peer's by the million.
            let refused = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
            if refused.is_power_of_two() {
                log::warn!(
                    "cladefsd: a connection of {peer} refused: {PEER_CONNECTIONS_MOST} of a peer's, \
                     {CONNECTIONS_MOST} of all, held ({refused} refused)"
                );
            }
            return None;
        }
        *of += 1;
        Some(Admitted { io, peer, connections: self.clone() })
    }
}

#[cfg(not(madsim))]
impl<IO> Drop for Admitted<IO> {
    fn drop(&mut self) {
        let mut open = self.connections.open.lock().unwrap();
        if let Some(of) = open.get_mut(&self.peer) {
            *of -= 1;
            if *of == 0 {
                open.remove(&self.peer);
            }
        }
    }
}

#[cfg(not(madsim))]
impl<IO: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Admitted<IO> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

#[cfg(not(madsim))]
impl<IO: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Admitted<IO> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(not(madsim))]
impl<IO: tonic::transport::server::Connected> tonic::transport::server::Connected for Admitted<IO> {
    type ConnectInfo = IO::ConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.io.connect_info()
    }
}

/// cladefs: the connections `incoming` accepts that [`Connections::admit`] admits, each counted
/// in `connections` until it goes; the others closed at once (board 8437).
#[cfg(not(madsim))]
fn admitting(
    incoming: impl futures::Stream<Item = std::io::Result<tokio::net::TcpStream>> + Unpin,
    connections: Arc<Connections>,
) -> impl futures::Stream<Item = std::io::Result<Admitted<tokio::net::TcpStream>>> + Unpin {
    use futures::StreamExt as _;
    incoming.filter_map(move |accepted| {
        let admitted = match accepted {
            // One whose peer's address can't be read is gone already.
            Ok(stream) => match stream.peer_addr() {
                Ok(peer) => connections.admit(stream, peer.ip()).map(Ok),
                Err(err) => {
                    log::info!("cladefsd: a peer's connection gone as it was accepted: {err}");
                    None
                }
            },
            Err(err) => Some(Err(err)),
        };
        std::future::ready(admitted)
    })
}

/// cladefs: serves `server` to the connections `incoming` accepts (`super::run::accepting`): until
/// it fails for good, whose error it returns.
#[cfg(not(madsim))]
async fn serve_on<IO>(
    incoming: impl futures::Stream<Item = std::io::Result<IO>> + Unpin + Send + 'static,
    server: Server,
) -> std::io::Result<()>
where
    IO: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + tonic::transport::server::Connected
        + Unpin
        + Send
        + 'static,
{
    let failed = Arc::new(std::sync::OnceLock::new());
    let accepted = crate::run::accepting("a peer's", incoming, failed.clone());
    let served = router(server)?.serve_with_incoming(accepted).await;
    crate::run::accepted_until("the peer listener", served, failed)
}

/// cladefs: the daemon's replica directories (`CLADEFSD_REPLICAS`, colon-separated for JBOD,
/// doc-21), each made if it is not there: each cell's logs held to the lag bound in bytes, which
/// bounds a writer's.
fn replicas(
    local: &Arc<local_cell::Host>,
    dirs: &[std::path::PathBuf],
) -> Result<Arc<Replicas<HostDir>>, String> {
    if dirs.is_empty() {
        return Err("CLADEFSD_REPLICAS names no directory".to_owned());
    }
    let mut seen = std::collections::HashSet::new();
    let mut host_dirs = Vec::with_capacity(dirs.len());
    for dir in dirs {
        // cladefs: made durable, its promises with it (TASK-708).
        host_fs::create_dir_all_durably(dir)
            .map_err(|err| format!("the replica directory {}: {err}", dir.display()))?;
        let canon = host_fs::canonicalize(dir)
            .map_err(|err| format!("the replica directory {}: {err}", dir.display()))?;
        if !seen.insert(canon.clone()) {
            return Err(format!(
                "CLADEFSD_REPLICAS: duplicate replica directory {}",
                dir.display()
            ));
        }
        let opened = HostDir::new_on(local, &canon)
            .map_err(|err| format!("the replica directory {}: {err}", dir.display()))?;
        host_dirs.push(opened);
    }
    let replicas = Replicas::open_all(host_dirs, members_most())
        .map_err(|err| format!("the replica directory: {err}"))?;
    log::info!(
        "cladefsd: replica store ({} shard(s)), incarnation {}",
        dirs.len(),
        replicas.incarnation()
    );
    Ok(Arc::new(replicas))
}

/// cladefs: sweeps `replicas` against the heads of `bucket`'s cells now and every
/// [`SWEEP_EVERY`] after, a task on `runtime`, the bucket runtime (doc-24 step 12): supervised,
/// its panic aborting the daemon (`super::run::supervising`, board 9414), as its replicas would
/// be swept never again, the daemon serving on.
fn sweeping(
    replicas: Arc<Replicas<HostDir>>,
    bucket: Arc<bucket_cell::object_bucket::RoutedBucket>,
    runtime: &tokio::runtime::Handle,
) {
    drop(runtime.spawn(crate::run::supervising(SWEEPING, async move {
        loop {
            if let Err(err) = sweep(&replicas, &*bucket).await {
                log::warn!("cladefsd: sweeping the replica directory: {err}");
            }
            tokio::time::sleep(SWEEP_EVERY).await;
        }
    })));
}

/// cladefs: what the replica directory's sweep is called, as a panic in it names it (board 9414).
const SWEEPING: &str = "cladefsd sweep";

/// cladefs: what the reaping of the migrations to it their sources abandoned is called, as a panic
/// in it names it (board 9414).
const REAPING: &str = "cladefsd reap";

/// cladefs: drops from `replicas` what the heads of `bucket`'s cells name no more: every
/// configuration's logs before the active one, and all of a cell gone or deleted; nothing of a
/// cell whose head names no configuration (doc-21). A head that can't be read keeps what its cell
/// has. Its directory's scans, reads and unlinks run off the runtime's workers, as they block for as
/// long as the disk takes (board 5378).
pub(super) async fn sweep(
    replicas: &Arc<Replicas<HostDir>>,
    bucket: &impl bucket_cell::bucket::Bucket,
) -> Result<(), String> {
    let listing = replicas.clone();
    let listed =
        crate::run::off_the_runtime(move || listing.cells().map_err(|err| err.to_string()));
    for (cell, _) in listed.await?? {
        let head = match bucket_cell::head::get(bucket, cell).await {
            Ok(head) => head,
            Err(err) => {
                log::warn!("cladefsd: sweeping {cell:?}: its head: {err}");
                continue;
            }
        };
        // Gone for good only if its head is, or is deleted: a head read without a configuration,
        // or not replicated, may name one by the time the sweep acts, its first installed and
        // active since (Codex's review of TASK-537.3.2).
        let swept = match head.filter(|head| !head.deleted()) {
            None => Swept::Gone,
            Some(head) => {
                match head.replication.and_then(|replication| replication.configuration) {
                    None => Swept::Unconfigured,
                    Some(configuration) => Swept::Before(Key {
                        number: configuration.number,
                        epoch: configuration.epoch,
                    }),
                }
            }
        };
        let sweeping = replicas.clone();
        let swept = move || sweeping.sweep(cell, swept).map_err(|err| err.to_string());
        crate::run::off_the_runtime(swept).await??;
    }
    Ok(())
}

/// What serves the daemon's peers, of its region alone: the parts `serving` reads, of any bucket;
/// and, if it has a replica directory, the logs of the cells it is a member of (doc-21).
struct Server {
    daemon: Arc<Daemon>,
    bucket: String,
    region: Option<Region>,
    serving: cladefs_core::bucket::Serving,
    replicas: Option<Arc<Replicas<HostDir>>>,
    /// cladefs: the bucket its replication calls' cells' heads are read from, as each call is
    /// admitted by its cell's head (board 8445).
    heads: Arc<RoutedBucket>,
    /// cladefs: how many writers' streams of appends it serves, each a task, for as long as it
    /// lives (board 8575).
    appending: Arc<AtomicUsize>,
}

/// cladefs: one more of a count, for as long as it lives.
struct Counted(Arc<AtomicUsize>);

impl Counted {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count.clone())
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Server {
    /// The replica directory, if the asker's bucket is this host's and the host has one.
    fn replicas(&self, bucket: &str) -> Result<Arc<Replicas<HostDir>>, Status> {
        replicas_of(&self.bucket, &self.replicas, bucket)
    }

    /// The replica directory, and what a call of `of` is of.
    fn of(
        &self,
        of: Option<ReplicaOf>,
        call: ReplicationCall,
    ) -> Result<(Arc<Replicas<HostDir>>, Of), Status> {
        of_in(&self.bucket, &self.replicas, of, call)
    }
}

/// What a call to a member is of: the cell, the configuration, and the incarnation it names.
type Of = (cladefs_core::CellId, Key, u64);

/// `replicas`, the replica directory of a host of the bucket `ours`, if the asker's bucket is it,
/// and the host has one.
fn replicas_of(
    ours: &str,
    replicas: &Option<Arc<Replicas<HostDir>>>,
    bucket: &str,
) -> Result<Arc<Replicas<HostDir>>, Status> {
    // Its bucket's URL never said: no peer's to learn (board 8437).
    if bucket != ours {
        return Err(Status::failed_precondition("asked of a bucket not this host's"));
    }
    let replicas = replicas.clone();
    replicas.ok_or_else(|| Status::failed_precondition("no replica directory (CLADEFSD_REPLICAS)"))
}

/// The replica directory of a host of the bucket `ours`, and what a call of `of` is of: refused
/// unless its formats are compatible for `call` (board 8457, board 10054).
fn of_in(
    ours: &str,
    replicas: &Option<Arc<Replicas<HostDir>>>,
    of: Option<ReplicaOf>,
    call: ReplicationCall,
) -> Result<(Arc<Replicas<HostDir>>, Of), Status> {
    let of = of.ok_or_else(|| Status::invalid_argument("of what: none said"))?;
    #[allow(deprecated)]
    let ReplicaOf { bucket, tenant, cell, configuration, incarnation, formats: _, versions } = of;
    check_replication_formats(versions, call)?;
    let configuration =
        configuration.ok_or_else(|| Status::invalid_argument("no configuration"))?;
    let key = Key { number: configuration.number, epoch: configuration.epoch };
    let replicas = replicas_of(ours, replicas, &bucket)?;
    // Of another incarnation: refused before anything is read for it (board 8445).
    let ours = replicas.incarnation();
    if incarnation != ours {
        return Err(refused(Refused::Incarnation { asked: incarnation, ours }.into()));
    }
    Ok((replicas, (cladefs_core::CellId { tenant, id: cell }, key, incarnation)))
}

/// cladefs: what `cell`'s head in `bucket` says, as a member admits a call of it by it (board
/// 8445, `replica::admit`): none if the bucket holds no live head of it, replicated. Read on the
/// bucket runtime within [`HEAD_WITHIN`]: UNAVAILABLE if it can't be, as of a member down, its
/// asker trying again.
async fn seen(
    host: &cladefs_core::Host,
    bucket: &Arc<RoutedBucket>,
    cell: cladefs_core::CellId,
) -> Result<Option<Seen>, Status> {
    let bucket = bucket.clone();
    let runtime = cladefs_core::bucket::bucket_runtime_on(host)
        .map_err(|err| Status::internal(format!("the bucket's runtime: {err}")))?;
    let reading = async move {
        tokio::time::timeout(HEAD_WITHIN, bucket_cell::head::get(&*bucket, cell)).await
    };
    // cladefs: in the call's trace context, the parent of its bucket requests' spans (board 8569).
    let reading = bucket_cell::trace::Parent::current().around(reading);
    let head = match runtime.spawn(reading).await {
        Ok(Ok(Ok(head))) => head,
        Ok(Ok(Err(err))) => {
            return Err(Status::unavailable(format!("{cell:?}'s head can't be read: {err}")));
        }
        Ok(Err(_)) => {
            return Err(Status::unavailable(format!("{cell:?}'s head unread in {HEAD_WITHIN:?}")));
        }
        Err(err) => return Err(Status::internal(format!("reading {cell:?}'s head: {err}"))),
    };
    let head = head.filter(|head| !head.deleted());
    Ok(head.and_then(|head| {
        let replication = head.replication?;
        let active = (replication.configuration)
            .map(|configuration| Key { number: configuration.number, epoch: configuration.epoch });
        Some(Seen { epoch: head.epoch, end: head.end, active })
    }))
}

/// cladefs: admits `asked` of `cell` by its head in `bucket`, read now ([`seen`]) and noted in
/// `replicas`; or, the bucket out of reach, by the newest of it `replicas` noted
/// ([`Replicas::seen`]), as replicated commits are for when the bucket is out of reach (doc-21): a
/// writer's streams, opened again, and drops go on. The head read now, if it was ([`Read`]), or
/// the refusal (board 8445).
async fn admitted(
    host: &cladefs_core::Host,
    bucket: &Arc<RoutedBucket>,
    replicas: &Arc<Replicas<HostDir>>,
    cell: cladefs_core::CellId,
    asked: replica::Asked,
) -> Result<Option<Read>, Status> {
    let (read, admitted) = match seen(host, bucket, cell).await {
        Ok(Some(head)) => {
            noted(replicas, cell, Some(Read(head))).await?;
            (Some(Read(head)), replica::admit(cell, Some(&head), asked))
        }
        Ok(None) => (None, replica::admit(cell, None, asked)),
        // Out of reach: what the bucket held once, if the cell holds files here, its writer since
        // perhaps another's (`replica::admit_noted`); else refused so, its asker trying again.
        Err(unread) if unread.code() == Code::Unavailable => {
            let noted = replicas.clone();
            match blocking(move || noted.seen(cell)).await? {
                Some(noted) => (None, replica::admit_noted(cell, &noted, asked)),
                None => return Err(unread),
            }
        }
        Err(failed) => return Err(failed),
    };
    admitted.map_err(|refusal| refused(refusal.into()))?;
    Ok(read)
}

/// cladefs: a cell's head as the bucket held it as a call was admitted: read for it, never one
/// noted before (board 8445), as what a call trims needs the head now (`Replicas::trim`).
#[derive(Clone, Copy)]
struct Read(Seen);

/// cladefs: notes `read`, a head read for a call of `cell`, in `replicas`, if there is one: again
/// once the call made the cell's first files there, as a cell holding none keeps nothing (Codex's
/// review of board 8445: a first append's stream noted it before its files, and kept nothing).
async fn noted(
    replicas: &Arc<Replicas<HostDir>>,
    cell: cladefs_core::CellId,
    read: Option<Read>,
) -> Result<(), Status> {
    let Some(Read(head)) = read else { return Ok(()) };
    let noting = replicas.clone();
    blocking(move || noting.saw(cell, head)).await
}

/// Marks a replica's fencing refusal on the existing FailedPrecondition status (board 10575).
const FENCED: &str = "cladefs-fenced";

/// cladefs: the status a member's refusal, or failure, answers with (peer.proto).
fn refused(err: replica::Error) -> Status {
    match err {
        replica::Error::Refused(refused @ Refused::Incarnation { .. }) => {
            Status::aborted(refused.to_string())
        }
        replica::Error::Refused(refused @ Refused::Fenced { .. }) => {
            let mut status = Status::failed_precondition(refused.to_string());
            status.metadata_mut().insert(FENCED, tonic::metadata::MetadataValue::from_static("1"));
            status
        }
        replica::Error::Refused(
            refused @ (Refused::Full { .. }
            | Refused::Logs { .. }
            | Refused::Disk { .. }
            | Refused::Configurations { .. }),
        ) => Status::resource_exhausted(refused.to_string()),
        // A call its cell's head could not have made (board 8445): never "fenced", which a writer
        // takes for its fencing.
        replica::Error::Refused(refused @ (Refused::Unheaded { .. } | Refused::Unasked { .. })) => {
            log::warn!("cladefsd: a peer's call refused: {refused}");
            Status::failed_precondition(refused.to_string())
        }
        // A writer's log, or one held, of another format: this member holds none of it for the
        // writer (board 8457).
        replica::Error::Refused(refused @ Refused::Format { .. }) => {
            log::warn!("cladefsd: the replica directory: {refused}");
            Status::failed_precondition(refused.to_string())
        }
        replica::Error::Io(err) => {
            log::error!("cladefsd: the replica directory: {err:#}");
            Status::internal(format!("{err:#}"))
        }
    }
}

/// cladefs: runs `work` on a blocking thread, the replica directory's I/O being blocking.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, replica::Error> + Send + 'static,
) -> Result<T, Status> {
    match crate::run::off_the_runtime(work).await {
        Ok(done) => done.map_err(refused),
        Err(err) => Err(Status::internal(format!("the replica directory's work: {err}"))),
    }
}

type Answers<T> = std::pin::Pin<Box<dyn futures::Stream<Item = Result<T, Status>> + Send>>;

/// cladefs: spawns `work`, a streaming call's, on the runtime it is served on, in the call's trace
/// context, the parent of its spans (board 8569).
fn spawn_in_call<F>(work: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(bucket_cell::trace::Parent::current().around(work))
}

#[tonic::async_trait]
impl proto::peer_server::Peer for Server {
    type AppendStream = Answers<AppendResponse>;
    type ReadLogStream = Answers<ReadLogResponse>;

    async fn incarnation(
        &self,
        request: Request<IncarnationRequest>,
    ) -> Result<Response<IncarnationResponse>, Status> {
        #[allow(deprecated)]
        let IncarnationRequest { bucket, formats: _, versions } = request.into_inner();
        check_replication_formats(versions, ReplicationCall::Write)?;
        let replicas = self.replicas(&bucket)?;
        Ok(Response::new(IncarnationResponse { incarnation: replicas.incarnation() }))
    }

    async fn append(
        &self,
        request: Request<tonic::Streaming<AppendRequest>>,
    ) -> Result<Response<Self::AppendStream>, Status> {
        // Answered at once, the stream's first append read after: a writer's client waits for
        // the answer's headers before it sends anything.
        let mut appends = request.into_inner();
        let (ours, replicas) = (self.bucket.clone(), self.replicas.clone());
        let heads = self.heads.clone();
        let host = Arc::clone(&self.daemon.host);
        let (answer, answers) = tokio::sync::mpsc::channel(16);
        let appending = Counted::new(&self.appending);
        spawn_in_call(async move {
            let _appending = appending;
            let mut next = match appends.message().await {
                Ok(next) => next,
                Err(status) => {
                    // Its asker gone: nothing to tell.
                    let _ = answer.send(Err(status)).await;
                    return;
                }
            };
            let Some(first) = &next else { return };
            // Admitted by its cell's head, read as the stream begins, once (board 8445); and what
            // its writer left behind trimmed then (board 8451).
            let begun = async {
                let (replicas, of) =
                    of_in(&ours, &replicas, first.of.clone(), ReplicationCall::Write)?;
                let read =
                    admitted(&host, &heads, &replicas, of.0, replica::Asked::Append(of.1)).await?;
                // Trimmed by the head read now alone: a head noted before may name an active
                // configuration the bucket's names no more, and trimming by it would drop the
                // one it names since (Codex's review of board 8451).
                if let Some(Read(head)) = read {
                    let trimming = replicas.clone();
                    blocking(move || trimming.trim(of, head.active)).await?;
                }
                Ok::<_, Status>((replicas, of, read))
            };
            let (replicas, of, mut unnoted) = match begun.await {
                Ok(found) => found,
                Err(status) => {
                    // Its asker gone: nothing to tell.
                    let _ = answer.send(Err(status)).await;
                    return;
                }
            };
            let noting = replicas.clone();
            while let Some(AppendRequest { log, offset, bytes, .. }) = next.take() {
                let replicas = replicas.clone();
                let appended = blocking(move || replicas.append(of, log, offset, &bytes)).await;
                if appended.is_ok()
                    && let Err(status) = noted(&noting, of.0, unnoted.take()).await
                {
                    // Its asker gone: nothing to tell.
                    let _ = answer.send(Err(status)).await;
                    return;
                }
                let failed = appended.is_err();
                let answered = appended.map(|end| AppendResponse { log, end });
                if answer.send(answered).await.is_err() || failed {
                    return;
                }
                next = match appends.message().await {
                    Ok(next) => next,
                    Err(status) => {
                        // Its asker gone: nothing to tell.
                        let _ = answer.send(Err(status)).await;
                        return;
                    }
                };
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(answers))))
    }

    async fn fence(
        &self,
        request: Request<FenceRequest>,
    ) -> Result<Response<FenceResponse>, Status> {
        let FenceRequest { of, epoch } = request.into_inner();
        let (replicas, of) = self.of(of, ReplicationCall::Read)?;
        let read = admitted(
            &self.daemon.host,
            &self.heads,
            &replicas,
            of.0,
            replica::Asked::Fence(of.1, epoch),
        )
        .await?;
        let fencing = replicas.clone();
        let logs = blocking(move || fencing.fence(of, epoch)).await?;
        // Noted again, its promise made: kept now (Codex's review of board 8445).
        noted(&replicas, of.0, read).await?;
        let logs = logs.into_iter().map(|(log, end)| LogEnd { log, end }).collect();
        Ok(Response::new(FenceResponse { logs }))
    }

    async fn migrate_prepare(
        &self,
        request: Request<proto::MigratePrepareRequest>,
    ) -> Result<Response<proto::MigratePrepareResponse>, Status> {
        let (incarnation, token) =
            crate::migration::target::prepare(&self.daemon, request.into_inner()).await?;
        Ok(Response::new(proto::MigratePrepareResponse { incarnation, token }))
    }

    async fn migrate_file(
        &self,
        request: Request<proto::MigrateFileRequest>,
    ) -> Result<Response<proto::MigrateFileResponse>, Status> {
        crate::migration::target::file(&self.daemon, request.into_inner()).await?;
        Ok(Response::new(proto::MigrateFileResponse {}))
    }

    async fn migrate_hand_over(
        &self,
        request: Request<proto::MigrateHandOverRequest>,
    ) -> Result<Response<proto::MigrateHandOverResponse>, Status> {
        let accepted =
            crate::migration::target::hand_over(&self.daemon, request.into_inner()).await?;
        Ok(Response::new(proto::MigrateHandOverResponse { accepted }))
    }

    async fn migrate_abort(
        &self,
        request: Request<proto::MigrateAbortRequest>,
    ) -> Result<Response<proto::MigrateAbortResponse>, Status> {
        crate::migration::target::abort(&self.daemon, request.into_inner()).await?;
        Ok(Response::new(proto::MigrateAbortResponse {}))
    }

    async fn migrate_renew(
        &self,
        request: Request<proto::MigrateRenewRequest>,
    ) -> Result<Response<proto::MigrateRenewResponse>, Status> {
        crate::migration::target::renew(&self.daemon, request.into_inner().migration)?;
        Ok(Response::new(proto::MigrateRenewResponse {}))
    }

    async fn migrate_release(
        &self,
        request: Request<proto::MigrateReleaseRequest>,
    ) -> Result<Response<proto::MigrateReleaseResponse>, Status> {
        crate::migration::target::release(&self.daemon, request.into_inner().migration).await?;
        Ok(Response::new(proto::MigrateReleaseResponse {}))
    }

    async fn read_log(
        &self,
        request: Request<ReadLogRequest>,
    ) -> Result<Response<Self::ReadLogStream>, Status> {
        let ReadLogRequest { of, log, offset } = request.into_inner();
        let (replicas, of) = self.of(of, ReplicationCall::Read)?;
        let (answer, answers) = tokio::sync::mpsc::channel(4);
        spawn_in_call(async move {
            let mut at = offset;
            loop {
                let replicas = replicas.clone();
                let read = blocking(move || replicas.read(of, log, at, READ_PIECE)).await;
                let (done, answered) = match read {
                    Ok(bytes) if bytes.is_empty() => return,
                    Ok(bytes) => {
                        at += bytes.len() as u64;
                        (false, Ok(ReadLogResponse { bytes }))
                    }
                    Err(status) => (true, Err(status)),
                };
                if answer.send(answered).await.is_err() || done {
                    return;
                }
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(answers))))
    }

    async fn drop_logs(
        &self,
        request: Request<DropLogsRequest>,
    ) -> Result<Response<DropLogsResponse>, Status> {
        let DropLogsRequest { of, through } = request.into_inner();
        let (replicas, of) = self.of(of, ReplicationCall::Drop)?;
        admitted(
            &self.daemon.host,
            &self.heads,
            &replicas,
            of.0,
            replica::Asked::Drop(of.1, through),
        )
        .await?;
        blocking(move || replicas.drop_logs(of, through)).await?;
        Ok(Response::new(DropLogsResponse {}))
    }

    async fn get_part(
        &self,
        request: Request<GetPartRequest>,
    ) -> Result<Response<GetPartResponse>, Status> {
        let GetPartRequest { bucket, key, part, home, root } = request.into_inner();
        // A home is a u16 (`Region`): one past it means nothing, refused as it comes in (board
        // 14354), never taken for no region's, which the bucket's URL alone would admit.
        let home = u16::try_from(home)
            .map_err(|_| Status::invalid_argument(format!("home {home}: past a u16")))?;
        let theirs = (root != 0).then_some(Region { home, root });
        if let Err(refused) = admits((self.region, &self.bucket), (theirs, &bucket)) {
            return Err(Status::failed_precondition(refused));
        }
        if !bucket_cell::peer::served(&key) {
            return Err(Status::invalid_argument(format!("{key}: no parts served")));
        }
        let asked = Asked { key, part };
        match self.serving.part(asked).await {
            Ok(Some(bytes)) => {
                SERVED.fetch_add(1, Ordering::Relaxed);
                SERVED_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(Response::new(GetPartResponse { bytes: bytes.to_vec() }))
            }
            Ok(None) => {
                SERVED_MISSING.fetch_add(1, Ordering::Relaxed);
                Err(Status::not_found(format!("part {part}: none")))
            }
            Err(err) => {
                log::warn!("cladefsd: serving a peer part {part}: {err}");
                Err(Status::unavailable(err.to_string()))
            }
        }
    }
}

/// The link to the peer at `address`, a daemon serving the hosts of its region, asked as the host
/// whose bucket is at `bucket`, of `region`, sharing `daemon`'s pooled HTTP/2 channel (board
/// 11713). Called while `Peers` holds its lock, so it must not re-enter `Peers::addresses`.
fn link(daemon: &Daemon, address: &str, (bucket, region): (&str, Option<Region>)) -> Arc<dyn Link> {
    // Its address checked, and the credentials as the daemon started.
    let channel = channel_on_with_prune(daemon, address, false)
        .expect("a peer's address checked, the credentials read");
    Arc::new(Grpc { channel, bucket: bucket.to_owned(), region })
}

/// Shared handle to a pooled peer HTTP/2 channel.
pub(super) type PeerChannel = Arc<Channel>;

/// cladefs: a pooled client and channel handle for the peer at `address` on `daemon` (board 11713,
/// board 11659): callers keeping the handle keep the shared HTTP/2 connection pinned in `daemon`'s
/// pool while active.
pub(super) fn client_on(daemon: &Daemon, address: &str) -> Result<(Client, PeerChannel), String> {
    let channel = channel_on(daemon, address)?;
    Ok((client(&channel), channel))
}

#[cfg(all(test, not(madsim)))]
pub(crate) fn member(_runtime: &tokio::runtime::Handle, address: &str) -> Result<Client, String> {
    Ok(client(&*channel_on(Daemon::process(), address)?))
}

/// cladefs: returns the pooled HTTP/2 channel to `address` from `daemon.peer_channels`, creating
/// the pool lazily on `daemon`'s bucket runtime.
pub(super) fn channel_on(daemon: &Daemon, address: &str) -> Result<PeerChannel, String> {
    channel_on_with_prune(daemon, address, true)
}

fn channel_on_with_prune(
    daemon: &Daemon,
    address: &str,
    prune: bool,
) -> Result<PeerChannel, String> {
    let channels = match daemon.peer_channels.get() {
        Some(channels) => channels,
        None => {
            let runtime = cladefs_core::bucket::bucket_runtime_on(&daemon.host)
                .map_err(|err| err.to_string())?;
            daemon.peer_channels.get_or_init(|| Channels::new(runtime))
        }
    };
    channels.of(daemon, address, prune)
}

/// cladefs: a channel to the peer at `address` (doc-21, board 11713), on `runtime`, the bucket
/// runtime, connected when first used: with no endpoint-level deadline on a request, as a writer's
/// stream of appends lasts as long as its configuration (unary calls apply per-call
/// `tokio::time::timeout`), pinging it so a down peer ends its stream within [`KEEPALIVE`] and
/// [`CONNECT`].
fn channel(
    daemon: &Daemon,
    runtime: &tokio::runtime::Handle,
    address: &str,
) -> Result<Channel, String> {
    check_address(address)?;
    let _entered = runtime.enter();
    let endpoint = endpoint_on(daemon, address)?
        .connect_timeout(CONNECT)
        .http2_keep_alive_interval(KEEPALIVE)
        .keep_alive_timeout(CONNECT)
        .keep_alive_while_idle(true)
        .tcp_nodelay(true);
    Ok(endpoint.connect_lazy())
}

/// cladefs: a client of a peer on `channel`, its messages [`MESSAGE_MOST`] at most.
fn client(channel: &Channel) -> Client {
    let client = made(channel.clone());
    client.max_decoding_message_size(MESSAGE_MOST).max_encoding_message_size(MESSAGE_MOST)
}

/// cladefs: a client of a peer, its calls counted and traced, each sending its trace's context to
/// the peer (`super::grpc::Made`, board 8565, board 8569); under madsim, whose tonic's clients take
/// its channel alone, a plain one.
#[cfg(not(madsim))]
pub(crate) type Client = PeerClient<crate::grpc::Made<Channel>>;
#[cfg(madsim)]
pub(crate) type Client = PeerClient<Channel>;

/// cladefs: a [`Client`] on `channel`.
fn made(channel: Channel) -> Client {
    #[cfg(not(madsim))]
    return PeerClient::new(crate::grpc::Made::new(channel, crate::grpc::PEER));
    #[cfg(madsim)]
    PeerClient::new(channel)
}

/// cladefs: the daemon's pooled peer HTTP/2 channels, one per peer address (board 8575, board
/// 11713, board 11659): GetPart, replication (append streams, fences, log reads, drops,
/// incarnations), and live migration all multiplex over the same HTTP/2 connection and keepalive
/// pings per peer. A channel is dropped once its address is neither in the current peer set nor
/// held by any active link, migration, or RPC.
pub(super) struct Channels {
    runtime: tokio::runtime::Handle,
    open: Mutex<HashMap<String, Arc<Channel>>>,
}

impl Channels {
    fn new(runtime: tokio::runtime::Handle) -> Self {
        Self { runtime, open: Mutex::default() }
    }

    /// Drops unheld channels whose addresses are no longer in `peers`.
    fn prune(&self, peers: &[String]) {
        let mut open = self.open.lock().unwrap();
        open.retain(|at, channel| Arc::strong_count(channel) > 1 || peers.contains(at));
    }

    /// The pooled channel to the peer at `address`, made if there is none.
    fn of(&self, daemon: &Daemon, address: &str, prune: bool) -> Result<Arc<Channel>, String> {
        // When invoked outside `Peers`'s lock (`prune == true`), evict stale unheld channels;
        // `link()` runs under `Peers`'s lock and must not re-enter `peers_on`.
        let peers = if prune {
            Some(
                cladefs_core::bucket::peers_on(&daemon.host)
                    .map(|peers| peers.addresses())
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        let mut open = self.open.lock().unwrap();
        if let Some(peers) = &peers {
            open.retain(|at, channel| {
                at == address || Arc::strong_count(channel) > 1 || peers.contains(at)
            });
        }
        if let Some(channel) = open.get(address) {
            return Ok(channel.clone());
        }
        let channel = Arc::new(channel(daemon, &self.runtime, address)?);
        open.insert(address.to_owned(), channel.clone());
        Ok(channel)
    }
}

/// cladefs: the hosts sharing the bucket, as a replicated cell's members (doc-21, TASK-537.3.3):
/// this host, at `me`, of its replica directory's incarnation, and its peers, reached through
/// `daemon`'s shared peer channel pool. Members are picked by rendezvous hashing of the cell and
/// the configuration's number over the peers (`crate::bucket::peers::ranked`), each that answers
/// its incarnation, distinct hosts.
pub(crate) struct ReplicaMembers {
    bucket: String,
    me: String,
    incarnation: u64,
    daemon: Arc<Daemon>,
}

impl local_cell::upload::Members for ReplicaMembers {
    fn me(&self) -> Result<bucket_cell::layout::Member, String> {
        Ok(bucket_cell::layout::Member { address: self.me.clone(), incarnation: self.incarnation })
    }

    /// cladefs: this host, or one of its peers now (board 8579): one `SetPeers` removed is no more.
    fn is_peer(&self, address: &str) -> bool {
        let peers = cladefs_core::bucket::peers_on(&self.daemon.host);
        address == self.me
            || peers.is_some_and(|peers| peers.addresses().iter().any(|at| at == address))
    }

    fn pick<'a>(
        &'a self,
        cell: cladefs_core::CellId,
        number: u64,
        f: u8,
        avoid: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<bucket_cell::layout::Member>, String>> {
        Box::pin(self.picked(cell, number, f, avoid))
    }

    fn link(
        &self,
        cell: cladefs_core::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Arc<dyn local_cell::replicate::MemberLink> {
        let of = self.of(cell, configuration, member);
        let channel = channel_on(&self.daemon, &member.address).expect("a peer's address checked");
        Arc::new(GrpcMember { channel, of, stream: tokio::sync::Mutex::default() })
    }

    fn fence<'a>(
        &'a self,
        cell: cladefs_core::CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        whom: &'a bucket_cell::layout::Member,
        epoch: u64,
    ) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>> {
        Box::pin(async move {
            let request = FenceRequest { of: Some(self.of(cell, configuration, whom)), epoch };
            let channel = channel_on(&self.daemon, &whom.address)?;
            let mut client = client(&channel).max_decoding_message_size(FENCE_ANSWER_MOST);
            match tokio::time::timeout(DEADLINE, client.fence(request)).await {
                Ok(Ok(answer)) => fence_answer(answer.into_inner(), members_most()),
                Ok(Err(status)) => Err(said(&status)),
                Err(_) => Err(format!("no answer within {DEADLINE:?}")),
            }
        })
    }

    fn read<'a>(
        &'a self,
        cell: cladefs_core::CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        whom: &'a bucket_cell::layout::Member,
        log: u64,
        end: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let request =
                ReadLogRequest { of: Some(self.of(cell, configuration, whom)), log, offset: 0 };
            let channel = channel_on(&self.daemon, &whom.address)?;
            let mut client = client(&channel).max_decoding_message_size(READ_PIECE_MOST);
            let answered = tokio::time::timeout(DEADLINE, client.read_log(request)).await;
            let answered = answered.map_err(|_| format!("no answer within {DEADLINE:?}"))?;
            read_to(answered.map_err(|status| said(&status))?.into_inner(), end, DEADLINE).await
        })
    }

    fn drop_before<'a>(
        &'a self,
        cell: cladefs_core::CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        whom: &'a bucket_cell::layout::Member,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // Named in the configuration the drop keeps, of the member's own incarnation.
            let mut of = self.of(cell, configuration, whom);
            of.incarnation = whom.incarnation;
            let request = DropLogsRequest { of: Some(of), through: None };
            let mut client = client(&*channel_on(&self.daemon, &whom.address)?);
            match tokio::time::timeout(DEADLINE, client.drop_logs(request)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(status)) => Err(said(&status)),
                Err(_) => Err(format!("no answer within {DEADLINE:?}")),
            }
        })
    }
}

/// What `status`, a member's answer, says.
fn said(status: &Status) -> String {
    format!("{:?}: {}", status.code(), status.message())
}

/// cladefs: a member's fence answer, `answer`, as a take relies on it (board 8453): the logs it
/// holds of the configuration, in order, none twice, each with its end; refused, as no member's
/// answer, if it names more than [`replica::LOGS_MOST`] logs, or ends summing past `most`, what a
/// member holds of a configuration ([`members_most`]). What a take then reads of a member, to the
/// ends it answered, is within what a member may hold.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn fence_answer(answer: FenceResponse, most: u64) -> Result<Vec<(u64, u64)>, String> {
    let FenceResponse { logs } = answer;
    if logs.len() > replica::LOGS_MOST {
        return Err(format!(
            "{} logs answered, past the {} a member holds of a configuration",
            logs.len(),
            replica::LOGS_MOST
        ));
    }
    let (mut ends, mut total) = (Vec::with_capacity(logs.len()), 0u64);
    for LogEnd { log, end } in logs {
        if let Some(&(last, _)) = ends.last()
            && log <= last
        {
            return Err(format!("log {log} answered after log {last}: out of order, or twice"));
        }
        total = total.checked_add(end).filter(|&total| total <= most).ok_or_else(|| {
            format!("log {log}'s end, {end}, takes the logs' past the {most} bytes a member holds")
        })?;
        ends.push((log, end));
    }
    Ok(ends)
}

/// cladefs: the bytes of a log a member streams, `pieces`, to `end`, where its fence said the log
/// ends; each piece within `deadline` of the one before, as a member's keepalives are answered
/// however long its disk takes (board 5416). A piece past `end` fails it (board 8453): a member
/// that fenced the log at its end holds no more of it, and what a read holds is no more than its
/// end, which its caller counts. Fewer if the stream ends before, which its caller refuses.
async fn read_to(
    mut pieces: impl futures::Stream<Item = Result<ReadLogResponse, Status>> + Unpin,
    end: u64,
    deadline: Duration,
) -> Result<Vec<u8>, String> {
    use futures::StreamExt as _;
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < end {
        let piece = tokio::time::timeout(deadline, pieces.next()).await;
        let Some(piece) = piece.map_err(|_| format!("no answer within {deadline:?}"))? else {
            break;
        };
        let piece = piece.map_err(|status| said(&status))?.bytes;
        let read = (bytes.len() as u64).saturating_add(piece.len() as u64);
        if read > end {
            return Err(format!("sent {read} bytes of a log it fenced at {end}"));
        }
        bytes.extend(piece);
    }
    Ok(bytes)
}

impl ReplicaMembers {
    /// 2f members for configuration `number` of `cell`, as [`local_cell::upload::Members::pick`]
    /// picks them: by rendezvous hashing, each that answers its incarnation.
    async fn picked(
        &self,
        cell: cladefs_core::CellId,
        number: u64,
        f: u8,
        avoid: &[String],
    ) -> Result<Vec<bucket_cell::layout::Member>, String> {
        let peers =
            cladefs_core::bucket::peers_on(&self.daemon.host).ok_or("the daemon has no peers")?;
        let addresses = peers.addresses();
        let others = (addresses.iter().map(String::as_str))
            .filter(|address| *address != self.me && !avoid.iter().any(|avoid| avoid == address));
        let key = format!("{}.{}", cell.tenant, cell.id);
        let wanted = 2 * usize::from(f);
        let mut picked = Vec::with_capacity(wanted);
        for address in cladefs_core::bucket::peers::ranked(others, &key, number) {
            if picked.len() == wanted {
                break;
            }
            match self.incarnation_of(address).await {
                Ok(incarnation) => picked
                    .push(bucket_cell::layout::Member { address: address.to_owned(), incarnation }),
                Err(err) => log::warn!("cladefsd: {address} as a member of {key}: {err}"),
            }
        }
        if picked.len() < wanted {
            return Err(format!(
                "{} of the {wanted} members a configuration of f {f} needs answered, of {} peers",
                picked.len(),
                addresses.len().saturating_sub(1)
            ));
        }
        Ok(picked)
    }
}

impl ReplicaMembers {
    /// What a call to `member` of `cell`'s `configuration` is of.
    fn of(
        &self,
        cell: cladefs_core::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> ReplicaOf {
        ReplicaOf {
            bucket: self.bucket.clone(),
            tenant: cell.tenant,
            cell: cell.id,
            configuration: Some(proto::Configuration {
                number: configuration.number,
                epoch: configuration.epoch,
            }),
            incarnation: member.incarnation,
            #[allow(deprecated)]
            formats: String::new(),
            versions: Some(current_formats_on(&self.daemon)),
        }
    }
}

impl ReplicaMembers {
    /// The incarnation of the replica directory of the host at `address`, as it answers within
    /// [`DEADLINE`].
    async fn incarnation_of(&self, address: &str) -> Result<u64, String> {
        let mut client = client(&*channel_on(&self.daemon, address)?);
        let asked = IncarnationRequest {
            bucket: self.bucket.clone(),
            #[allow(deprecated)]
            formats: String::new(),
            versions: Some(current_formats_on(&self.daemon)),
        };
        match tokio::time::timeout(DEADLINE, client.incarnation(asked)).await {
            Ok(Ok(answer)) => Ok(answer.into_inner().incarnation),
            Ok(Err(status)) => Err(said(&status)),
            Err(_) => Err(format!("no answer within {DEADLINE:?}")),
        }
    }
}

/// cladefs: a member of a replicated cell's configuration, reached over gRPC (doc-21), on the
/// channel to its address every cell's links share ([`Channels`]): one stream of appends, opened
/// as it is first needed, and again after any failure.
struct GrpcMember {
    channel: Arc<Channel>,
    of: ReplicaOf,
    /// Its one task's (`local_cell::replicate`), so never waited for.
    stream: tokio::sync::Mutex<Option<Appending>>,
}

/// An open stream of appends to a member: where they go, and its answers.
type Appending = (tokio::sync::mpsc::Sender<AppendRequest>, tonic::Streaming<AppendResponse>);

/// cladefs: what the replicator makes of a member's refusal (peer.proto).
fn link_error(status: &Status) -> local_cell::replicate::LinkError {
    use local_cell::replicate::LinkError;
    let what = said(&status);
    match status.code() {
        Code::FailedPrecondition
            if status.metadata().get(FENCED).is_some_and(|fenced| fenced == "1") =>
        {
            LinkError::Fenced(what)
        }
        // A member full for the configuration refuses it until a replacement picks another
        // (doc-21, Choosing members; TASK-537.3.5), rather than be tried again.
        Code::Aborted | Code::FailedPrecondition | Code::ResourceExhausted => {
            LinkError::Refused(what)
        }
        _ => LinkError::Down(what),
    }
}

impl local_cell::replicate::MemberLink for GrpcMember {
    fn append(
        &self,
        log: u64,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<u64, local_cell::replicate::LinkError>> {
        Box::pin(self.appended(log, offset, bytes))
    }

    fn drop_through(
        &self,
        through: u64,
    ) -> BoxFuture<'_, Result<(), local_cell::replicate::LinkError>> {
        let request = DropLogsRequest { of: Some(self.of.clone()), through: Some(through) };
        let mut client = client(&self.channel);
        Box::pin(async move {
            match tokio::time::timeout(DEADLINE, client.drop_logs(request)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(status)) => Err(link_error(&status)),
                Err(_) => Err(local_cell::replicate::LinkError::Down(format!(
                    "no answer within {DEADLINE:?}"
                ))),
            }
        })
    }
}

impl GrpcMember {
    /// Appends `bytes` at `offset` of log `log` on the member, through its stream of appends,
    /// opened if it isn't: the log's end there, as it answers.
    async fn appended(
        &self,
        log: u64,
        offset: u64,
        bytes: Vec<u8>,
    ) -> Result<u64, local_cell::replicate::LinkError> {
        use local_cell::replicate::LinkError;
        let mut stream = self.stream.lock().await;
        let request = AppendRequest { of: Some(self.of.clone()), log, offset, bytes };
        let answered = async {
            if stream.is_none() {
                let (sending, sent) = tokio::sync::mpsc::channel(4);
                let mut client = client(&self.channel);
                let answers = client
                    .append(tokio_stream::wrappers::ReceiverStream::new(sent))
                    .await
                    .map_err(|status| link_error(&status))?
                    .into_inner();
                *stream = Some((sending, answers));
            }
            let (sending, answers) = stream.as_mut().unwrap();
            if sending.send(request).await.is_err() {
                return Err(LinkError::Down("the stream of appends ended".into()));
            }
            match answers.message().await {
                Ok(Some(answer)) => Ok(answer.end),
                Ok(None) => Err(LinkError::Down("the member ended the stream".into())),
                Err(status) => Err(link_error(&status)),
            }
        }
        .await;
        if answered.is_err() {
            *stream = None;
        }
        answered
    }
}

/// A peer reached over gRPC, sharing the daemon's pooled HTTP/2 channel (board 11713).
struct Grpc {
    channel: PeerChannel,
    bucket: String,
    region: Option<Region>,
}

impl Link for Grpc {
    fn get(&self, asked: Asked) -> BoxFuture<'static, Result<Option<Vec<u8>>, Refusal>> {
        let Asked { key, part } = asked;
        let (home, root) =
            self.region.map_or((0, 0), |region| (u32::from(region.home), region.root));
        let request = GetPartRequest { bucket: self.bucket.clone(), key, part, home, root };
        let mut client = client(&self.channel);
        async move {
            match tokio::time::timeout(DEADLINE, client.get_part(request)).await {
                Ok(Ok(response)) => Ok(Some(response.into_inner().bytes)),
                Ok(Err(status)) if status.code() == Code::NotFound => Ok(None),
                // Of another region, or another bucket: asked nothing again (TASK-633.4).
                Ok(Err(status)) if status.code() == Code::FailedPrecondition => {
                    Err(Refusal::Foreign(status.message().to_owned()))
                }
                Ok(Err(status)) => Err(Refusal::Failed(said(&status))),
                Err(_) => Err(Refusal::Failed(format!("no answer within {DEADLINE:?}"))),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests;
