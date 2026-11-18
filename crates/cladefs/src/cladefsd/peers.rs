// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's part in the hosts sharing its bucket (TASK-406.2, `crate::bucket::peers`):
//! it serves them parts of the bucket's objects over gRPC, at `CLADEFSD_PEER_ADDRESS`
//! (`proto/cladefs/peer/v1/peer.proto`), and its stores' fetchers ask them for the parts they own,
//! `CLADEFSD_PEERS` naming them at its start and the control API's `SetPeers` at any time.
//! What it serves comes from the host's memory and part cache, which it needs, or from the bucket
//! once, however many hosts ask at once ([`crate::bucket::serving_peers`]).

use crate::bucket::peers::{Asked, Link, Peers};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use local_cell::HostDir;
use local_cell::replica::{self, Key, Refused, Replicas};
use proto::peer_client::PeerClient;
use proto::peer_server::PeerServer;
use proto::{
    AppendRequest, AppendResponse, DropLogsRequest, DropLogsResponse, FenceRequest, FenceResponse,
    GetPartRequest, GetPartResponse, IncarnationRequest, IncarnationResponse, LogEnd,
    ReadLogRequest, ReadLogResponse, ReplicaOf,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
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
/// the configuration's.
// TASK-537.3.3: the writer's side connects to its members so; until it lands, tests alone do.
#[cfg_attr(not(test), allow(dead_code))]
const KEEPALIVE: Duration = Duration::from_secs(5);

/// cladefs: how much of a log a member sends a take at once (`ReadLog`).
const READ_PIECE: u64 = 1 << 20;

/// cladefs: how often a member sweeps its replica directory, reading each cell's head (doc-21,
/// Sweeping stale replica logs).
const SWEEP_EVERY: Duration = Duration::from_secs(600);

/// The parts the daemon served its peers, and those it had not as they asked.
static SERVED: AtomicU64 = AtomicU64::new(0);
static SERVED_MISSING: AtomicU64 = AtomicU64::new(0);

/// Has the daemon serve the hosts sharing its bucket, at `address` (`CLADEFSD_PEER_ADDRESS`),
/// on `listener`, bound there, and its stores' fetchers ask them, `peers` at first
/// (`CLADEFSD_PEERS`), for the parts they own: before any store opens. It needs the host's
/// part cache, which keeps what it serves.
pub fn start(
    address: &str,
    listener: std::net::TcpListener,
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
    let linked = url.clone();
    let runtime = crate::bucket::bucket_runtime().map_err(|err| failed(&err.to_string()))?;
    let linking = runtime.clone();
    let asking = Peers::new(address, move |peer| link(&linking, peer, &linked));
    asking.set(peers);
    crate::bucket::ask_peers(bucket.clone(), asking)?;
    let replicas = match std::env::var_os("CLADEFSD_REPLICAS") {
        Some(dir) => Some(replicas(std::path::Path::new(&dir)).map_err(|err| failed(&err))?),
        None => None,
    };
    if let Some(replicas) = &replicas {
        sweeping(replicas.clone(), bucket.clone(), &runtime)?;
    }
    let serving = crate::bucket::serving_peers(bucket, cache);
    let serving = serving.map_err(|err| failed(&err.to_string()))?;
    let server = Server { bucket: url, serving, replicas };
    listener.set_nonblocking(true).map_err(|err| failed(&err.to_string()))?;
    let serving = std::thread::Builder::new().name("cladefsd peers".into()).spawn(move || {
        if let Err(err) = serve(listener, server) {
            log::error!("cladefsd: serving peers: {err}");
            std::process::exit(1);
        }
    });
    serving.map(drop).map_err(|err| failed(&err.to_string()))
}

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

/// What the daemon's stores' fetchers asked its peers, and the parts it served them and had not as
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

/// Serves `server` to the peers that connect to `listener`, until accepting fails.
fn serve(listener: std::net::TcpListener, server: Server) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(THREADS)
        .thread_name("cladefsd peers")
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .tcp_nodelay(true)
            .add_service(PeerServer::new(server))
            .serve_with_incoming(incoming)
            .await
            .map_err(std::io::Error::other)
    })
}

/// cladefs: the daemon's replica directory at `dir` (`CLADEFSD_REPLICAS`, doc-21), made if it is
/// not there: each cell's logs held to the lag bound in bytes, which bounds a writer's.
fn replicas(dir: &std::path::Path) -> Result<Arc<Replicas<HostDir>>, String> {
    let made = std::fs::create_dir_all(dir).and_then(|()| HostDir::new(dir));
    let dir = made.map_err(|err| format!("the replica directory {}: {err}", dir.display()))?;
    let most = local_cell::upload::Options::default().max_lag_bytes;
    let replicas =
        Replicas::open(dir, most).map_err(|err| format!("the replica directory: {err}"))?;
    log::info!("cladefsd: replica directory, incarnation {}", replicas.incarnation());
    Ok(Arc::new(replicas))
}

/// cladefs: sweeps `replicas` against the heads of `bucket`'s cells now and every
/// [`SWEEP_EVERY`] after, on a thread of its own, its reads on `runtime`.
fn sweeping(
    replicas: Arc<Replicas<HostDir>>,
    bucket: Arc<bucket_cell::object_bucket::ObjectBucket>,
    runtime: &tokio::runtime::Handle,
) -> Result<(), String> {
    let runtime = runtime.clone();
    let spawned = std::thread::Builder::new().name("cladefsd sweep".into()).spawn(move || {
        loop {
            if let Err(err) = runtime.block_on(sweep(&replicas, &*bucket)) {
                log::warn!("cladefsd: sweeping the replica directory: {err}");
            }
            std::thread::sleep(SWEEP_EVERY);
        }
    });
    spawned.map(drop).map_err(|err| format!("sweeping the replica directory: {err}"))
}

/// cladefs: drops from `replicas` what the heads of `bucket`'s cells name no more: every
/// configuration's logs before the active one, and all of a cell gone, deleted, or no longer
/// replicated (doc-21). A head that can't be read keeps what its cell has.
pub(super) async fn sweep(
    replicas: &Replicas<HostDir>,
    bucket: &impl bucket_cell::bucket::Bucket,
) -> Result<(), String> {
    for (cell, _) in replicas.cells().map_err(|err| err.to_string())? {
        let head = match bucket_cell::head::get(bucket, cell).await {
            Ok(head) => head,
            Err(err) => {
                log::warn!("cladefsd: sweeping {cell:?}: its head: {err}");
                continue;
            }
        };
        let active = (head.filter(|head| !head.deleted()))
            .and_then(|head| head.replication)
            .and_then(|replication| replication.configuration)
            .map(|configuration| Key { number: configuration.number, epoch: configuration.epoch });
        // On the sweep's own thread, which nothing else waits on: its file I/O blocks it alone.
        replicas.sweep(cell, active).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// What serves the daemon's peers: the parts of `bucket`, by URL, which `serving` reads; and, if
/// it has a replica directory, the logs of the cells it is a member of (doc-21).
struct Server {
    bucket: String,
    serving: crate::bucket::Serving,
    replicas: Option<Arc<Replicas<HostDir>>>,
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

/// The replica directory of a host of the bucket `ours`, and what a call of `of` is of.
fn of_in(
    ours: &str,
    replicas: &Option<Arc<Replicas<HostDir>>>,
    of: Option<ReplicaOf>,
) -> Result<(Arc<Replicas<HostDir>>, Of), Status> {
    let of = of.ok_or_else(|| Status::invalid_argument("of what: none said"))?;
    let ReplicaOf { bucket, tenant, cell, configuration, incarnation } = of;
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
        replica::Error::Refused(refused @ Refused::Gap { .. }) => {
            Status::out_of_range(refused.to_string())
        }
        replica::Error::Refused(refused @ Refused::Full { .. }) => {
            Status::resource_exhausted(refused.to_string())
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
    match tokio::task::spawn_blocking(work).await {
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
        let replicas = self.replicas(&request.into_inner().bucket)?;
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
        tokio::spawn(async move {
            let mut next = match appends.message().await {
                Ok(next) => next,
                Err(status) => {
                    let _ = answer.send(Err(status)).await;
                    return;
                }
            };
            let Some(first) = &next else { return };
            let (replicas, of) = match of_in(&ours, &replicas, first.of.clone()) {
                Ok(found) => found,
                Err(status) => {
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
        let GetPartRequest { bucket, key, part } = request.into_inner();
        if bucket != self.bucket {
            let theirs = format!("asked of {bucket}, this host's bucket being {}", self.bucket);
            return Err(Status::failed_precondition(theirs));
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

/// The link to the peer at `address`, a daemon serving the parts of the bucket at `bucket`, by URL,
/// connected when first asked, on `runtime`, the bucket runtime.
fn link(runtime: &tokio::runtime::Handle, address: &str, bucket: &str) -> Arc<dyn Link> {
    let _entered = runtime.enter();
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .expect("a peer's address checked")
        .connect_timeout(CONNECT)
        .timeout(DEADLINE)
        .tcp_nodelay(true);
    let client = PeerClient::new(endpoint.connect_lazy());
    Arc::new(Grpc { client, bucket: bucket.to_owned() })
}

/// cladefs: a client of the member at `address` (doc-21), on `runtime`, the bucket runtime: with
/// no deadline on a request, as a writer's stream of appends lasts as long as its configuration,
/// but pinging it, so that a member down ends its stream within [`KEEPALIVE`] and [`CONNECT`].
// TASK-537.3.3: the writer's side connects to its members so; until it lands, tests alone do.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn member(
    runtime: &tokio::runtime::Handle,
    address: &str,
) -> Result<PeerClient<tonic::transport::Channel>, String> {
    check_address(address)?;
    let _entered = runtime.enter();
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .map_err(|err| format!("peer {address:?}: {err}"))?
        .connect_timeout(CONNECT)
        .http2_keep_alive_interval(KEEPALIVE)
        .keep_alive_timeout(CONNECT)
        .keep_alive_while_idle(true)
        .tcp_nodelay(true);
    Ok(PeerClient::new(endpoint.connect_lazy()))
}

/// A peer reached over gRPC.
struct Grpc {
    client: PeerClient<tonic::transport::Channel>,
    bucket: String,
}

impl Link for Grpc {
    fn get(&self, asked: Asked) -> BoxFuture<'static, Result<Option<Vec<u8>>, String>> {
        let Asked { key, part } = asked;
        let request = GetPartRequest { bucket: self.bucket.clone(), key, part };
        let mut client = self.client.clone();
        async move {
            match client.get_part(request).await {
                Ok(response) => Ok(Some(response.into_inner().bytes)),
                Err(status) if status.code() == Code::NotFound => Ok(None),
                Err(status) => Err(format!("{:?}: {}", status.code(), status.message())),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests;
