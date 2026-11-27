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
//! ([`Limited`]). A refusal says nothing of this host's bucket.

use crate::Daemon;
use crate::grpc::Served;
use cladefs_core::bucket::peers::{Asked, Link, Peers, Refusal, Region, admits};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use proto::peer_client::PeerClient;
use proto::peer_server::PeerServer;
use proto::{GetPartRequest, GetPartResponse};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// How often a peer HTTP/2 connection pings its peer; unanswered past [`CONNECT`] ends it.
const KEEPALIVE: Duration = Duration::from_secs(5);

/// cladefs: the largest message the peer service sends, and takes of a hand-over, which names every
/// file of its cell (doc-25).
pub(crate) const MESSAGE_MOST: usize = 64 << 20;

/// cladefs: the largest message of each call the peer service takes (board 8437), refused by its
/// decoder past it, before it is held whole ([`Limited`]): the calls naming a few numbers, a key or
/// a path (`PATH_MAX`), 64 KiB; a migration's piece of a file (`migration::PIECE`), the holes of a
/// file's last piece beside it, as many as 12 MiB name; a hand-over, [`MESSAGE_MOST`].
#[cfg(not(madsim))]
const SMALL_MOST: usize = 64 << 10;
#[cfg(not(madsim))]
const FILE_MOST: usize = crate::migration::PIECE + (12 << 20);
#[cfg(not(madsim))]
const HAND_OVER_MOST: usize = MESSAGE_MOST;

/// cladefs: the most streams a peer's connection carries at once (board 8437).
#[cfg(not(madsim))]
const STREAMS_MOST: u32 = 8192;

/// cladefs: the most connections the peer service holds of one peer's address, and of all (board
/// 8437).
#[cfg(not(madsim))]
const PEER_CONNECTIONS_MOST: usize = 16;
#[cfg(not(madsim))]
const CONNECTIONS_MOST: usize = 1024;

/// cladefs: the name every peer's certificate is for (board 8437): the hosts' CA signs only their
/// daemons', so that a certificate it signed is a host's, whatever its address.
#[cfg(not(madsim))]
const PEER_NAME: &str = "cladefs-peer";

/// cladefs: the format family versions (current write version and oldest readable version) this
/// daemon advertises when talking to peers (doc-28, board 10054): its cell directories'
/// (`local_cell::FORMAT`), its cell records' (`crate::cell_dir::FORMAT`), its extent frames'
/// (`extent_frame::FORMAT`), and its bucket's layout's (`bucket_cell::layout::FORMAT`).
pub(crate) fn current_formats() -> proto::Formats {
    formats_for_ratchet(formats::Ratchet::default_current())
}

fn formats_for_ratchet(ratchet: &formats::Ratchet) -> proto::Formats {
    // Branch on `ratchet.storage()` here whenever a future `storage >= 2` bump introduces a new
    // write version for any migrated family.
    let _storage = ratchet.storage();
    let mut versions = proto::Formats {
        cell_directory: local_cell::FORMAT.current,
        cell_records: cladefs_core::cell_dir::FORMAT.current,
        extent_frame: extent_frame::FORMAT.current,
        bucket_layout: bucket_cell::layout::FORMAT.current,
        cell_directory_oldest: local_cell::FORMAT.oldest,
        cell_records_oldest: cladefs_core::cell_dir::FORMAT.oldest,
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

/// cladefs: the format family ranges a peer receiver reads (doc-28, board 10054).
#[derive(Clone, Copy, Debug)]
struct SupportedFormats {
    cell_directory: formats::Format,
    cell_records: formats::Format,
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
            extent_frame: extent_frame::FORMAT,
            bucket_layout: bucket_cell::layout::FORMAT,
        }
    }

    /// Checks a live migration's prepare versions: `cell_directory`, `cell_records`,
    /// `extent_frame`, and `bucket_layout`.
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

/// cladefs: whether `theirs`, a live migration prepare's format versions, are within what this
/// host reads (`FAILED_PRECONDITION` if absent or outside range; doc-28, board 10054).
pub(crate) fn check_migration_formats(
    migration: u64,
    theirs: Option<proto::Formats>,
) -> Result<(), Status> {
    SupportedFormats::current().check_migration(migration, theirs)
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
    let serving = cladefs_core::bucket::serving_peers_on(&daemon.host, bucket, cache);
    let serving = serving.map_err(|err| failed(&err.to_string()))?;
    let server = Server { daemon: Arc::clone(daemon), bucket: url, region, serving };
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

/// cladefs: what the reaping of the migrations to it their sources abandoned is called, as a panic
/// in it names it (board 9414).
const REAPING: &str = "cladefsd reap";

/// What serves the daemon's peers, of its region alone: the parts `serving` reads, of any bucket,
/// and live migration RPCs.
struct Server {
    daemon: Arc<Daemon>,
    bucket: String,
    region: Option<Region>,
    serving: cladefs_core::bucket::Serving,
}

#[tonic::async_trait]
impl proto::peer_server::Peer for Server {
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

/// cladefs: a channel to the peer at `address` (board 11713), on `runtime`, the bucket
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
/// 11713, board 11659): GetPart and migrations all multiplex over this single HTTP/2 connection
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

/// What `status`, a peer's answer, says.
fn said(status: &Status) -> String {
    format!("{:?}: {}", status.code(), status.message())
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
