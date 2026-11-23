// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's part in the hosts sharing its bucket (TASK-406.2, `crate::bucket::peers`):
//! it serves them parts of the bucket's objects over gRPC, at `CLADEFSD_PEER_ADDRESS`
//! (`proto/cladefs/peer/v1/peer.proto`), and its cells' fetchers ask them for the parts they own,
//! `CLADEFSD_PEERS` naming them at its start and the control API's `SetPeers` at any time.
//! What it serves comes from the host's memory and part cache, which it needs, or from the bucket
//! once, however many hosts ask at once ([`crate::bucket::serving_peers`]).

use crate::bucket::peers::{Asked, Link, Peers, Refusal, Region, admits};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use local_cell::HostDir;
use local_cell::replica::{self, Key, Refused, Replicas, Swept};
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
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};

/// What hosts sharing a bucket serve each other, generated from its .proto.
pub mod proto {
    tonic::include_proto!("cladefs.peer.v1");
}

/// How long a host waits to connect to a peer: on the hosts' own network, a peer slower than this
/// is as good as down.
const CONNECT: Duration = Duration::from_secs(1);

/// How long a host waits for a part a peer serves: one the peer fetches from the bucket itself,
/// behind others in its budget, takes the bucket's latency and more.
const DEADLINE: Duration = Duration::from_secs(5);

/// How many threads serve the peers' requests, whose fetches run on the bucket runtime.
const THREADS: usize = 2;

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

/// cladefs: the largest message a member takes or sends (doc-21): a writer's append carries its
/// flush's records, their data inline, and a writer sends 4 MiB at most at once, well within it;
/// tonic's default, 4 MiB, refused one a little larger as OUT_OF_RANGE (TASK-537.3.2's review).
pub(crate) const MESSAGE_MOST: usize = 64 << 20;

/// cladefs: how often a member sweeps its replica directory, reading each cell's head (doc-21,
/// Sweeping stale replica logs).
const SWEEP_EVERY: Duration = Duration::from_secs(600);

/// cladefs: the version of the peer service's protocol, `peer.proto`'s (board 8455): a change to
/// what a call means, or to a message's fields, is a new one, which [`formats`] names.
pub(crate) const PROTOCOL: u32 = 2;

/// cladefs: the formats of what this daemon hands its peers, in a line: its cell directories'
/// (`local_cell::FORMAT`), its extent frames' (`extent_frame::FORMAT`), its bucket's layout's
/// (`bucket_cell::layout::FORMAT`) and the protocol's ([`PROTOCOL`]). Two daemons hand each other
/// a cell (a migration's prepare names them), or a writer's logs (every replication call names
/// them, [`same_formats`]), only if theirs are the same (boards 8455, 8457): nothing of another
/// build's format is taken, as nothing migrates (AGENTS.md, "No migrations").
pub(crate) fn formats() -> String {
    let named = |cell: &str| {
        let (frames, layout) = (extent_frame::FORMAT, bucket_cell::layout::FORMAT);
        format!(
            "cell directories {cell:?}, extent frames {frames}, bucket layout {layout}, peers {PROTOCOL}"
        )
    };
    // A test's daemon of another build's cell directories: `peers::formats=return(FORMAT)`.
    fail::fail_point!("peers::formats", |cell: Option<String>| named(&cell.unwrap_or_default()));
    named(local_cell::FORMAT)
}

/// cladefs: whether `theirs`, the formats a replication call names, are this host's ([`formats`],
/// board 8457): FAILED_PRECONDITION if not, an asker of a build before them naming none (""), so
/// that no writer of another format is acked, nor a take of another given logs it would misread.
fn same_formats(theirs: &str) -> Result<(), Status> {
    let ours = formats();
    if theirs == ours {
        return Ok(());
    }
    // Its first 256 characters: a peer's string, said in a log line and an answer.
    let theirs: String = theirs.chars().take(256).collect();
    Err(Status::failed_precondition(format!(
        "the asker's formats ({theirs:?}) are not this host's ({ours:?}): refused"
    )))
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

/// Has the daemon serve the hosts sharing its bucket, at `address` (`CLADEFSD_PEER_ADDRESS`),
/// on `listening`, bound there (or, under madsim, its address on the simulated network), and its
/// cells' fetchers ask them, `peers` at first (`CLADEFSD_PEERS`), for the parts they own: before
/// any cell opens. It needs the host's part cache, which keeps what it serves.
pub async fn start(
    address: &str,
    listening: Listening,
    peers: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(), String> {
    let peers: Vec<String> = peers.into_iter().map(|peer| peer.as_ref().to_owned()).collect();
    for peer in std::iter::once(address).chain(peers.iter().map(String::as_str)) {
        check_address(peer)?;
    }
    let url = crate::bucket::Host::from_env()?.bucket;
    let failed = |what: &str| format!("serving peers: {what}");
    let bucket = super::cells::daemon_bucket().map_err(|err| failed(&format!("{err:?}")))?;
    let cache = super::cells::part_cache().map_err(|err| failed(&format!("{err:?}")))?;
    let cache = cache.ok_or_else(|| {
        "CLADEFSD_PEER_ADDRESS is set, but CLADEFSD_PART_CACHE names no directory for the \
         part cache, which keeps what the daemon serves its peers"
            .to_owned()
    })?;
    // cladefs: its region, which peers of another never serve, nor it them (TASK-633.4).
    let region = crate::bucket::peers::region(&*bucket).await;
    let region = region.map_err(|err| failed(&err.to_string()))?;
    let linked = url.clone();
    let runtime = crate::bucket::bucket_runtime().map_err(|err| failed(&err.to_string()))?;
    let linking = runtime.clone();
    let asking = Peers::new(address, move |peer| link(&linking, peer, (&linked, region)));
    asking.set(peers);
    crate::bucket::ask_peers(bucket.clone(), asking)?;
    let replicas = match node_local::env::var_os("CLADEFSD_REPLICAS") {
        Some(dir) => Some(replicas(std::path::Path::new(&dir)).map_err(|err| failed(&err))?),
        None => None,
    };
    if let Some(replicas) = &replicas {
        sweeping(replicas.clone(), bucket.clone(), &runtime);
        let members = ReplicaMembers {
            bucket: url.clone(),
            me: address.to_owned(),
            incarnation: replicas.incarnation(),
            channels: Channels::new(runtime.clone()),
        };
        crate::bucket::replicate_with(Arc::new(members))?;
    }
    let serving = crate::bucket::serving_peers(bucket, cache);
    let serving = serving.map_err(|err| failed(&err.to_string()))?;
    let server = Server { bucket: url, region, serving, replicas, appending: Arc::default() };
    #[cfg(not(madsim))]
    listening.set_nonblocking(true).map_err(|err| failed(&err.to_string()))?;
    // cladefs: on threads of its own, or under madsim a task (`super::run`, doc-24 step 12).
    // An accept that fails for good, the listener's, stops the daemon, saying so (board 8573).
    let serving = super::run::service("cladefsd peers", THREADS, move || async move {
        if let Err(err) = serve(listening, server).await {
            log::error!("cladefsd: serving peers: {err}");
            std::process::exit(1);
        }
    });
    serving.map_err(|err| failed(&err.to_string()))?;
    // cladefs: the migrations to it their sources abandoned, given up (board 5734).
    if crate::ephemeral_host::host().is_ok() {
        let idle = super::migration::idle().map_err(|err| failed(&err))?;
        runtime.spawn(super::migration::target::reap(idle));
    }
    Ok(())
}

/// cladefs: what the daemon serves its peers on: a TCP listener, bound; under madsim, its address
/// on the simulated network (doc-24's boundary).
#[cfg(not(madsim))]
pub type Listening = std::net::TcpListener;
/// cladefs: what the daemon serves its peers on, under madsim: its address on the simulated
/// network.
#[cfg(madsim)]
pub type Listening = std::net::SocketAddr;

/// Has the hosts sharing the bucket be those at `addresses` (`SetPeers`): `EINVAL` if one is no
/// `HOST:PORT`, `ENOTSUP` if the daemon serves no peers.
pub(super) fn set(addresses: Vec<String>) -> Result<(), crate::Errno> {
    let peers = crate::bucket::peers().ok_or(crate::Errno::ENOTSUP)?;
    for address in &addresses {
        check_address(address).map_err(|err| {
            log::warn!("cladefsd: SetPeers: {err}");
            crate::Errno::EINVAL
        })?;
    }
    log::info!("cladefsd: peers: {}", addresses.join(","));
    peers.set(addresses);
    Ok(())
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

/// What the daemon's cells' fetchers asked its peers, and the parts it served them and had not as
/// they asked, if it serves peers.
pub(super) fn counts() -> Option<(crate::bucket::peers::Counts, u64, u64)> {
    let counts = crate::bucket::peers()?.counts();
    Some((counts, SERVED.load(Ordering::Relaxed), SERVED_MISSING.load(Ordering::Relaxed)))
}

/// The daemon's line in its stats file (`CLADEFSD_STATS`): its [`counts`].
pub(super) fn line() -> Option<String> {
    let (counts, served, served_missing) = counts()?;
    let crate::bucket::peers::Counts { asked, given, missing, failed } = counts;
    Some(format!(
        "peers: asked={asked} given={given} missing={missing} failed={failed} served={served} \
         served-missing={served_missing}"
    ))
}

/// Serves `server` to the peers that connect to `listening`, until accepting fails for good
/// ([`accepting`]): each connection with no delay, TCP's keepalives on, and pinged as a member's
/// client pings it (board 8575).
async fn serve(listening: Listening, server: Server) -> std::io::Result<()> {
    #[cfg(not(madsim))]
    {
        let listener = tokio::net::TcpListener::from_std(listening)?;
        let incoming = tonic::transport::server::TcpIncoming::from(listener)
            .with_nodelay(Some(true))
            .with_keepalive(Some(KEEPALIVE))
            .with_keepalive_interval(Some(KEEPALIVE));
        serve_on(incoming, server).await
    }
    #[cfg(madsim)]
    router(server).serve(listening).await.map_err(std::io::Error::other)
}

/// cladefs: the peer service of `server`, its connections pinged every [`KEEPALIVE`] and closed if
/// unanswered within [`CONNECT`], as a member's client pings its member (board 8575): a writer's
/// host lost without a word leaves no stream of appends, its task and its connection, for good.
fn router(server: Server) -> tonic::transport::server::Router {
    let service = PeerServer::new(server)
        .max_decoding_message_size(MESSAGE_MOST)
        .max_encoding_message_size(MESSAGE_MOST);
    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE))
        .http2_keepalive_timeout(Some(CONNECT))
        .add_service(service)
}

/// cladefs: serves `server` to the connections `incoming` accepts ([`accepting`]): until it fails
/// for good, whose error it returns.
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
    let served = router(server).serve_with_incoming(accepting(incoming, failed.clone())).await;
    served.map_err(std::io::Error::other)?;
    // Its connections ended, their stream dropped with its note of why.
    let failed = Arc::into_inner(failed).and_then(std::sync::OnceLock::into_inner);
    Err(failed.unwrap_or_else(|| std::io::Error::other("the peer listener ended")))
}

/// cladefs: the connections `incoming` accepts, a failure the host may get over (no descriptor
/// free, EMFILE or ENFILE, no buffers, a connection gone before it was accepted) logged and tried
/// again after a pause, [`super::server::ACCEPT_AGAIN`], as the VM sockets are (board 8573):
/// served by tonic as they come, such a failure was passed over at once, again and again, a busy
/// loop holding a thread while the descriptors stayed short. One of the listener's own (it is
/// closed, or no listener: EBADF, EINVAL, ENOTSOCK, EOPNOTSUPP) ends them, noted in `failed`.
#[cfg(not(madsim))]
fn accepting<IO>(
    incoming: impl futures::Stream<Item = std::io::Result<IO>> + Unpin,
    failed: Arc<std::sync::OnceLock<std::io::Error>>,
) -> impl futures::Stream<Item = std::io::Result<IO>> {
    use futures::StreamExt as _;
    futures::stream::unfold((incoming, false), move |(mut incoming, mut failing)| {
        let failed = failed.clone();
        async move {
            loop {
                match incoming.next().await? {
                    Ok(accepted) => return Some((Ok(accepted), (incoming, false))),
                    // That connection's, gone before it was accepted.
                    Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted => {}
                    Err(err) if for_good(&err) => {
                        // The first error for good; there is no other, the stream ending here.
                        let _ = failed.set(err);
                        return None;
                    }
                    Err(err) => {
                        if !failing {
                            log::error!(
                                "cladefsd: accepting a peer's connection: {err}: trying again \
                                 every {:?}",
                                super::server::ACCEPT_AGAIN
                            );
                            failing = true;
                        }
                        tokio::time::sleep(super::server::ACCEPT_AGAIN).await;
                    }
                }
            }
        }
    })
}

/// cladefs: whether `err`, an accept's, is of the listener itself, which no retry gets over.
#[cfg(not(madsim))]
fn for_good(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EOPNOTSUPP | libc::EFAULT)
    )
}

/// cladefs: the daemon's replica directory at `dir` (`CLADEFSD_REPLICAS`, doc-21), made if it is
/// not there: each cell's logs held to the lag bound in bytes, which bounds a writer's.
fn replicas(dir: &std::path::Path) -> Result<Arc<Replicas<HostDir>>, String> {
    // cladefs: made durable, its promises with it (TASK-708).
    let made = host_fs::create_dir_all_durably(dir).and_then(|()| HostDir::new(dir));
    let dir = made.map_err(|err| format!("the replica directory {}: {err}", dir.display()))?;
    let replicas = Replicas::open(dir, members_most())
        .map_err(|err| format!("the replica directory: {err}"))?;
    log::info!("cladefsd: replica directory, incarnation {}", replicas.incarnation());
    Ok(Arc::new(replicas))
}

/// cladefs: sweeps `replicas` against the heads of `bucket`'s cells now and every
/// [`SWEEP_EVERY`] after, a task on `runtime`, the bucket runtime (doc-24 step 12).
fn sweeping(
    replicas: Arc<Replicas<HostDir>>,
    bucket: Arc<bucket_cell::object_bucket::RoutedBucket>,
    runtime: &tokio::runtime::Handle,
) {
    drop(runtime.spawn(async move {
        loop {
            if let Err(err) = sweep(&replicas, &*bucket).await {
                log::warn!("cladefsd: sweeping the replica directory: {err}");
            }
            tokio::time::sleep(SWEEP_EVERY).await;
        }
    }));
}

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
        super::run::off_the_runtime(move || listing.cells().map_err(|err| err.to_string()));
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
        super::run::off_the_runtime(swept).await??;
    }
    Ok(())
}

/// What serves the daemon's peers, of its region alone: the parts `serving` reads, of any bucket;
/// and, if it has a replica directory, the logs of the cells it is a member of (doc-21).
struct Server {
    bucket: String,
    region: Option<Region>,
    serving: crate::bucket::Serving,
    replicas: Option<Arc<Replicas<HostDir>>>,
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
    fn of(&self, of: Option<ReplicaOf>) -> Result<(Arc<Replicas<HostDir>>, Of), Status> {
        of_in(&self.bucket, &self.replicas, of)
    }
}

/// What a call to a member is of: the cell, the configuration, and the incarnation it names.
type Of = (crate::CellId, Key, u64);

/// `replicas`, the replica directory of a host of the bucket `ours`, if the asker's bucket is it,
/// and the host has one.
fn replicas_of(
    ours: &str,
    replicas: &Option<Arc<Replicas<HostDir>>>,
    bucket: &str,
) -> Result<Arc<Replicas<HostDir>>, Status> {
    if bucket != ours {
        return Err(Status::failed_precondition(format!(
            "asked of {bucket}, this host's bucket being {ours}"
        )));
    }
    let replicas = replicas.clone();
    replicas.ok_or_else(|| Status::failed_precondition("no replica directory (CLADEFSD_REPLICAS)"))
}

/// The replica directory of a host of the bucket `ours`, and what a call of `of` is of: refused
/// unless it names this host's formats (board 8457).
fn of_in(
    ours: &str,
    replicas: &Option<Arc<Replicas<HostDir>>>,
    of: Option<ReplicaOf>,
) -> Result<(Arc<Replicas<HostDir>>, Of), Status> {
    let of = of.ok_or_else(|| Status::invalid_argument("of what: none said"))?;
    let ReplicaOf { bucket, tenant, cell, configuration, incarnation, formats } = of;
    same_formats(&formats)?;
    let configuration =
        configuration.ok_or_else(|| Status::invalid_argument("no configuration"))?;
    let key = Key { number: configuration.number, epoch: configuration.epoch };
    Ok((
        replicas_of(ours, replicas, &bucket)?,
        (crate::CellId { tenant, id: cell }, key, incarnation),
    ))
}

/// cladefs: the status a member's refusal, or failure, answers with (peer.proto).
fn refused(err: replica::Error) -> Status {
    match err {
        replica::Error::Refused(refused @ Refused::Incarnation { .. }) => {
            Status::aborted(refused.to_string())
        }
        replica::Error::Refused(refused @ Refused::Fenced { .. }) => {
            Status::failed_precondition(refused.to_string())
        }
        replica::Error::Refused(refused @ (Refused::Full { .. } | Refused::Logs { .. })) => {
            Status::resource_exhausted(refused.to_string())
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
    match super::run::off_the_runtime(work).await {
        Ok(done) => done.map_err(refused),
        Err(err) => Err(Status::internal(format!("the replica directory's work: {err}"))),
    }
}

type Answers<T> = std::pin::Pin<Box<dyn futures::Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl proto::peer_server::Peer for Server {
    type AppendStream = Answers<AppendResponse>;
    type ReadLogStream = Answers<ReadLogResponse>;

    async fn incarnation(
        &self,
        request: Request<IncarnationRequest>,
    ) -> Result<Response<IncarnationResponse>, Status> {
        let IncarnationRequest { bucket, formats } = request.into_inner();
        same_formats(&formats)?;
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
        let (answer, answers) = tokio::sync::mpsc::channel(16);
        let appending = Counted::new(&self.appending);
        tokio::spawn(async move {
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
            let (replicas, of) = match of_in(&ours, &replicas, first.of.clone()) {
                Ok(found) => found,
                Err(status) => {
                    // Its asker gone: nothing to tell.
                    let _ = answer.send(Err(status)).await;
                    return;
                }
            };
            while let Some(AppendRequest { log, offset, bytes, .. }) = next.take() {
                let replicas = replicas.clone();
                let appended = blocking(move || replicas.append(of, log, offset, &bytes)).await;
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
        let (replicas, of) = self.of(of)?;
        let logs = blocking(move || replicas.fence(of, epoch)).await?;
        let logs = logs.into_iter().map(|(log, end)| LogEnd { log, end }).collect();
        Ok(Response::new(FenceResponse { logs }))
    }

    async fn migrate_prepare(
        &self,
        request: Request<proto::MigratePrepareRequest>,
    ) -> Result<Response<proto::MigratePrepareResponse>, Status> {
        let (incarnation, token) = super::migration::target::prepare(request.into_inner()).await?;
        Ok(Response::new(proto::MigratePrepareResponse { incarnation, token }))
    }

    async fn migrate_file(
        &self,
        request: Request<proto::MigrateFileRequest>,
    ) -> Result<Response<proto::MigrateFileResponse>, Status> {
        super::migration::target::file(request.into_inner()).await?;
        Ok(Response::new(proto::MigrateFileResponse {}))
    }

    async fn migrate_hand_over(
        &self,
        request: Request<proto::MigrateHandOverRequest>,
    ) -> Result<Response<proto::MigrateHandOverResponse>, Status> {
        let accepted = super::migration::target::hand_over(request.into_inner()).await?;
        Ok(Response::new(proto::MigrateHandOverResponse { accepted }))
    }

    async fn migrate_abort(
        &self,
        request: Request<proto::MigrateAbortRequest>,
    ) -> Result<Response<proto::MigrateAbortResponse>, Status> {
        super::migration::target::abort(request.into_inner()).await?;
        Ok(Response::new(proto::MigrateAbortResponse {}))
    }

    async fn migrate_renew(
        &self,
        request: Request<proto::MigrateRenewRequest>,
    ) -> Result<Response<proto::MigrateRenewResponse>, Status> {
        super::migration::target::renew(request.into_inner().migration)?;
        Ok(Response::new(proto::MigrateRenewResponse {}))
    }

    async fn migrate_release(
        &self,
        request: Request<proto::MigrateReleaseRequest>,
    ) -> Result<Response<proto::MigrateReleaseResponse>, Status> {
        super::migration::target::release(request.into_inner().migration).await?;
        Ok(Response::new(proto::MigrateReleaseResponse {}))
    }

    async fn read_log(
        &self,
        request: Request<ReadLogRequest>,
    ) -> Result<Response<Self::ReadLogStream>, Status> {
        let ReadLogRequest { of, log, offset } = request.into_inner();
        let (replicas, of) = self.of(of)?;
        let (answer, answers) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
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
        let (replicas, of) = self.of(of)?;
        blocking(move || replicas.drop_logs(of, through)).await?;
        Ok(Response::new(DropLogsResponse {}))
    }

    async fn get_part(
        &self,
        request: Request<GetPartRequest>,
    ) -> Result<Response<GetPartResponse>, Status> {
        let GetPartRequest { bucket, key, part, home, root } = request.into_inner();
        // A home past a u16: no region's, refused as one of another region.
        let theirs = (root != 0)
            .then_some(())
            .and_then(|()| Some(Region { home: u16::try_from(home).ok()?, root }));
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
/// whose bucket is at `bucket`, of `region`, connected when first asked, on `runtime`, the bucket
/// runtime.
fn link(
    runtime: &tokio::runtime::Handle,
    address: &str,
    (bucket, region): (&str, Option<Region>),
) -> Arc<dyn Link> {
    let _entered = runtime.enter();
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .expect("a peer's address checked")
        .connect_timeout(CONNECT)
        .timeout(DEADLINE)
        .tcp_nodelay(true);
    let client = PeerClient::new(endpoint.connect_lazy());
    Arc::new(Grpc { client, bucket: bucket.to_owned(), region })
}

/// cladefs: a client of the member at `address` (doc-21), on `runtime`, the bucket runtime, on a
/// channel of its own ([`channel`]).
pub(crate) fn member(
    runtime: &tokio::runtime::Handle,
    address: &str,
) -> Result<PeerClient<Channel>, String> {
    Ok(client(&channel(runtime, address)?))
}

/// cladefs: a channel to the member at `address` (doc-21), on `runtime`, the bucket runtime,
/// connected when first used: with no deadline on a request, as a writer's stream of appends lasts
/// as long as its configuration, but pinging it, so that a member down ends its stream within
/// [`KEEPALIVE`] and [`CONNECT`].
fn channel(runtime: &tokio::runtime::Handle, address: &str) -> Result<Channel, String> {
    check_address(address)?;
    let _entered = runtime.enter();
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .map_err(|err| format!("peer {address:?}: {err}"))?
        .connect_timeout(CONNECT)
        .http2_keep_alive_interval(KEEPALIVE)
        .keep_alive_timeout(CONNECT)
        .keep_alive_while_idle(true)
        .tcp_nodelay(true);
    Ok(endpoint.connect_lazy())
}

/// cladefs: a client of a member on `channel`, its messages [`MESSAGE_MOST`] at most.
fn client(channel: &Channel) -> PeerClient<Channel> {
    let client = PeerClient::new(channel.clone());
    client.max_decoding_message_size(MESSAGE_MOST).max_encoding_message_size(MESSAGE_MOST)
}

/// cladefs: the channels to the members, one per address (board 8575): every replicated cell's
/// link to a member and every call to it (fences, log reads, drops, incarnations) share its one
/// HTTP/2 connection and its pings, where each had a connection of its own, a host of N replicated
/// cells holding 2N connections to its peers and pinging each every [`KEEPALIVE`]. A channel goes,
/// its connection with it, once its address is no peer's and nothing holds it (a link of a
/// configuration's, [`GrpcMember`], or a call): the next asks make another.
struct Channels {
    runtime: tokio::runtime::Handle,
    open: Mutex<HashMap<String, Arc<Channel>>>,
}

impl Channels {
    fn new(runtime: tokio::runtime::Handle) -> Self {
        Self { runtime, open: Mutex::default() }
    }

    /// The channel to the member at `address`, made if there is none.
    fn of(&self, address: &str) -> Result<Arc<Channel>, String> {
        // No peers asked yet: none kept but those held.
        let peers = crate::bucket::peers().map(|peers| peers.addresses()).unwrap_or_default();
        let mut open = self.open.lock().unwrap();
        open.retain(|at, channel| Arc::strong_count(channel) > 1 || peers.contains(at));
        if let Some(channel) = open.get(address) {
            return Ok(channel.clone());
        }
        let channel = Arc::new(channel(&self.runtime, address)?);
        open.insert(address.to_owned(), channel.clone());
        Ok(channel)
    }
}

/// cladefs: the hosts sharing the bucket, as a replicated cell's members (doc-21, TASK-537.3.3):
/// this host, at `me`, of its replica directory's incarnation, and its peers, reached on
/// `runtime`, the bucket runtime. Members are picked by rendezvous hashing of the cell and the
/// configuration's number over the peers (`crate::bucket::peers::ranked`), each that answers its
/// incarnation, distinct hosts.
pub(crate) struct ReplicaMembers {
    bucket: String,
    me: String,
    incarnation: u64,
    channels: Channels,
}

impl local_cell::upload::Members for ReplicaMembers {
    fn me(&self) -> Result<bucket_cell::layout::Member, String> {
        Ok(bucket_cell::layout::Member { address: self.me.clone(), incarnation: self.incarnation })
    }

    /// cladefs: this host, or one of its peers now (board 8579): one `SetPeers` removed is no more.
    fn is_peer(&self, address: &str) -> bool {
        let peers = crate::bucket::peers();
        address == self.me
            || peers.is_some_and(|peers| peers.addresses().iter().any(|at| at == address))
    }

    fn pick<'a>(
        &'a self,
        cell: crate::CellId,
        number: u64,
        f: u8,
        avoid: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<bucket_cell::layout::Member>, String>> {
        Box::pin(self.picked(cell, number, f, avoid))
    }

    fn link(
        &self,
        cell: crate::CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Arc<dyn local_cell::replicate::MemberLink> {
        let of = self.of(cell, configuration, member);
        let channel = self.channels.of(&member.address).expect("a peer's address checked");
        Arc::new(GrpcMember { channel, of, stream: tokio::sync::Mutex::default() })
    }

    fn fence<'a>(
        &'a self,
        cell: crate::CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        whom: &'a bucket_cell::layout::Member,
        epoch: u64,
    ) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>> {
        Box::pin(async move {
            let request = FenceRequest { of: Some(self.of(cell, configuration, whom)), epoch };
            let channel = self.channels.of(&whom.address)?;
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
        cell: crate::CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        whom: &'a bucket_cell::layout::Member,
        log: u64,
        end: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let request =
                ReadLogRequest { of: Some(self.of(cell, configuration, whom)), log, offset: 0 };
            let channel = self.channels.of(&whom.address)?;
            let mut client = client(&channel).max_decoding_message_size(READ_PIECE_MOST);
            let answered = tokio::time::timeout(DEADLINE, client.read_log(request)).await;
            let answered = answered.map_err(|_| format!("no answer within {DEADLINE:?}"))?;
            read_to(answered.map_err(|status| said(&status))?.into_inner(), end, DEADLINE).await
        })
    }

    fn drop_before<'a>(
        &'a self,
        cell: crate::CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        whom: &'a bucket_cell::layout::Member,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // Named in the configuration the drop keeps, of the member's own incarnation.
            let mut of = self.of(cell, configuration, whom);
            of.incarnation = whom.incarnation;
            let request = DropLogsRequest { of: Some(of), through: None };
            let mut client = client(&*self.channels.of(&whom.address)?);
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
        cell: crate::CellId,
        number: u64,
        f: u8,
        avoid: &[String],
    ) -> Result<Vec<bucket_cell::layout::Member>, String> {
        let peers = crate::bucket::peers().ok_or("the daemon has no peers")?;
        let addresses = peers.addresses();
        let others = (addresses.iter().map(String::as_str))
            .filter(|address| *address != self.me && !avoid.iter().any(|avoid| avoid == address));
        let key = format!("{}.{}", cell.tenant, cell.id);
        let wanted = 2 * usize::from(f);
        let mut picked = Vec::with_capacity(wanted);
        for address in crate::bucket::peers::ranked(others, &key, number) {
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
        cell: crate::CellId,
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
            formats: formats(),
        }
    }
}

impl ReplicaMembers {
    /// The incarnation of the replica directory of the host at `address`, as it answers within
    /// [`DEADLINE`].
    async fn incarnation_of(&self, address: &str) -> Result<u64, String> {
        let mut client = client(&*self.channels.of(address)?);
        let asked = IncarnationRequest { bucket: self.bucket.clone(), formats: formats() };
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
        Code::FailedPrecondition if status.message().contains("fenced") => LinkError::Fenced(what),
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

/// A peer reached over gRPC.
struct Grpc {
    client: PeerClient<tonic::transport::Channel>,
    bucket: String,
    region: Option<Region>,
}

impl Link for Grpc {
    fn get(&self, asked: Asked) -> BoxFuture<'static, Result<Option<Vec<u8>>, Refusal>> {
        let Asked { key, part } = asked;
        let (home, root) =
            self.region.map_or((0, 0), |region| (u32::from(region.home), region.root));
        let request = GetPartRequest { bucket: self.bucket.clone(), key, part, home, root };
        let mut client = self.client.clone();
        async move {
            match client.get_part(request).await {
                Ok(response) => Ok(Some(response.into_inner().bytes)),
                Err(status) if status.code() == Code::NotFound => Ok(None),
                // Of another region, or another bucket: asked nothing again (TASK-633.4).
                Err(status) if status.code() == Code::FailedPrecondition => {
                    Err(Refusal::Foreign(status.message().to_owned()))
                }
                Err(status) => Err(Refusal::Failed(said(&status))),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests;
