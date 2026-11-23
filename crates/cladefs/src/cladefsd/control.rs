// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's control API (TASK-43.3): gRPC over a Unix socket, for the host's VM manager, which
//! launches the VMMs. Its calls and their errors are `proto/cladefs/cladefsd/v1/cladefsd.proto`'s.
//!
//! The cell operations it serves are futures ([`super::cells`], TASK-43.6): their waits, for
//! uploads, guests' answers, and cells to be let go, hold no thread, and their cells' I/O runs on
//! the runtime's blocking threads, so that many calls waiting, syncs while object storage is slow,
//! keep no other call waiting.

use super::cells;
use super::disk;
use super::grants::Grants;
use super::memory;
use crate::Errno;
use bucket_cell::cell::Kind;
use proto::cladefsd_client::CladefsdClient;
use proto::cladefsd_server::{Cladefsd, CladefsdServer};
use proto::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::codegen::http;
use tonic::{Code, Request, Response, Status};

/// The API's messages and services, generated from its .proto.
pub mod proto {
    tonic::include_proto!("cladefs.cladefsd.v1");
}

/// cladefs: what a gRPC service names itself by: tonic's trait, or, under madsim, its tonic's
/// (doc-24, TASK-685.13).
#[cfg(not(madsim))]
use tonic::server::NamedService;
#[cfg(madsim)]
use tonic::transport::NamedService;

/// The metadata entry of a failed call's status holding the errno it failed with, a decimal
/// number.
pub const ERRNO: &str = "cladefs-errno";

/// How many threads serve the API's calls, and their waits, which hold none (TASK-43.6). cladefs:
/// under madsim, the simulated host's one, its runtime's.
#[cfg(not(madsim))]
const THREADS: usize = 2;

/// How many threads the calls' cells' I/O runs on at most: what waits holds none of them, so that
/// many calls waiting, for uploads or guests, leave them to the others' I/O.
pub const BLOCKING_THREADS: usize = 32;

/// cladefs: the most bytes an answer of the API is, encoded, as its .proto says (board 8581): the
/// daemon refuses to send a larger one ([`bounded`]), and its clients, this module's and the Go
/// one's, take one of this size, where gRPC's default, 4 MiB, failed `GetStats` at some hundreds
/// of VMs at the most (each VM process some 2 KiB of it, 40 KiB with a guest saying near the most
/// it may, as `GetStats` says). 64 MiB is some 30,000 VMs at what a guest says today, 1,600 at the
/// most each may say; past it, `GetStats` leaves out what its caller chooses. The client holds an
/// answer whole, once.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Serves the control API to the callers that connect to `listener`, until accepting fails; the
/// sockets of the tenants it grants go in the directory `tenants`. cladefs: and the Mount service
/// of the daemon's FUSE mount of cells, if it has one ([`super::mount`], TASK-475.2): its calls,
/// counted and refused as the daemon drains as the API's are; without a mount, they fail
/// `UNIMPLEMENTED`.
#[cfg(not(madsim))]
pub fn serve_control(
    listener: std::os::unix::net::UnixListener,
    tenants: impl Into<PathBuf>,
) -> std::io::Result<()> {
    let control = Control { grants: Arc::new(Grants::new(tenants.into())) };
    let (reporter, health) = tonic_health::server::health_reporter();
    listener.set_nonblocking(true)?;
    // cladefs: outside the simulation: the control API on its Unix socket, on its thread's own
    // runtime; under madsim the async serve_control below, a task (doc-24).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(THREADS)
        .max_blocking_threads(BLOCKING_THREADS)
        .thread_name("cladefsd ctl")
        .enable_all()
        .build()?;
    // cladefs: outside the simulation, as above.
    runtime.block_on(async {
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(
            tokio::net::UnixListener::from_std(listener)?,
        );
        // Its first status reported before it serves: the reporter starts with the server's, "",
        // SERVING, which a check served before a task of its own reported otherwise saw.
        let mut serving = super::SERVING.get().subscribe();
        let mut now = *serving.borrow_and_update();
        report_health(&reporter, now).await;
        tokio::spawn(async move {
            // cladefs: its panic aborts the daemon, as a service thread's does (board 8501): the
            // health service would say what it last said for ever, the drain waiting for it.
            let _supervised = local_cell::shared::Awaited::new(HEALTH_REPORTER);
            loop {
                // cladefs: a test's reporter gone once it reported SERVING, as no panic but its
                // runtime's going leaves it, which a drain bounds (board 8503).
                fail::fail_point!("cladefsd::health_reporter_gone", now, |_| ());
                // The sender lives as long as the process.
                if serving.changed().await.is_err() {
                    return;
                }
                now = *serving.borrow_and_update();
                report_health(&reporter, now).await;
            }
        });
        CONTROL_SERVING.get().store(true, std::sync::atomic::Ordering::Release);
        tonic::transport::Server::builder()
            .add_service(health)
            // cladefs: no answer encoded past MAX_MESSAGE_BYTES (board 8581): what grows refuses
            // first, saying what to leave out (`bounded`); any other, OUT_OF_RANGE, tonic's.
            .add_service(Drained(
                CladefsdServer::new(control).max_encoding_message_size(MAX_MESSAGE_BYTES),
            ))
            // cladefs: its FUSE mount's, if it mounts cells (TASK-475.2).
            .add_optional_service(super::mount::service().map(Drained))
            .serve_with_incoming(incoming)
            .await
            .map_err(std::io::Error::other)
    })
}

/// cladefs: serves the control API, as [`serve_control`] does, on the simulated network at
/// `address`, under madsim, which has no Unix sockets: the socket is a deployment detail (doc-24's
/// boundary, TASK-685.13). The health service, tonic's, which madsim's tonic doesn't serve, isn't;
/// its status is reported all the same, as the daemon's drain waits for it. No FUSE mount's
/// service: FUSE is outside the simulation.
#[cfg(madsim)]
pub async fn serve_control(
    address: std::net::SocketAddr,
    tenants: impl Into<PathBuf>,
) -> std::io::Result<()> {
    let control = Control { grants: Arc::new(Grants::new(tenants.into())) };
    let mut serving = super::SERVING.get().subscribe();
    super::REPORTED.get().send_replace(*serving.borrow_and_update());
    tokio::spawn(async move {
        while serving.changed().await.is_ok() {
            super::REPORTED.get().send_replace(*serving.borrow_and_update());
        }
    });
    CONTROL_SERVING.get().store(true, std::sync::atomic::Ordering::Release);
    tonic::transport::Server::builder()
        .add_service(Drained(
            CladefsdServer::new(control).max_encoding_message_size(MAX_MESSAGE_BYTES),
        ))
        .serve(address)
        .await
        .map_err(std::io::Error::other)
}

/// cladefs: a client of the API over `channel`, which takes answers as large as the daemon sends
/// ([`MAX_MESSAGE_BYTES`], board 8581).
fn client(channel: tonic::transport::Channel) -> CladefsdClient<tonic::transport::Channel> {
    CladefsdClient::new(channel).max_decoding_message_size(MAX_MESSAGE_BYTES)
}

/// cladefs: `answer`, the answer of the API's call `call`, unless it is past [`MAX_MESSAGE_BYTES`]
/// encoded: refused then, RESOURCE_EXHAUSTED (EMSGSIZE), saying its size, and `smaller`, what the
/// caller may ask for instead (board 8581).
fn bounded<T: prost::Message>(call: &str, answer: T, smaller: &str) -> Result<Response<T>, Status> {
    let (bytes, most) = (answer.encoded_len(), most_answer());
    if bytes <= most {
        return Ok(Response::new(answer));
    }
    log::warn!("cladefsd: refusing {call}'s answer of {bytes} bytes, past {most}");
    Err(saying(
        Errno::EMSGSIZE,
        format!(
            "{call}'s answer would be {bytes} bytes, past the most the control API sends, {most}: \
             {smaller}"
        ),
    ))
}

/// cladefs: the most bytes an answer is: [`MAX_MESSAGE_BYTES`], or in a test, what the
/// `cladefsd::most_answer` failpoint says, so that one refuses an answer of a few VMs' stats rather
/// than of a thousand's.
fn most_answer() -> usize {
    fail::fail_point!("cladefsd::most_answer", |most: Option<String>| {
        // Not a number: the test's configuration wrong, which the panic says.
        most.and_then(|most| most.parse().ok()).expect("cladefsd::most_answer=return(BYTES)")
    });
    MAX_MESSAGE_BYTES
}

/// cladefs: `stats`, as the API has it (TASK-111.4), the guests' histograms left out if
/// `leave_out_histograms`, and their stats if `leave_out_guests` (board 8581).
fn stats_response(
    stats: super::stats::HostStats,
    (leave_out_histograms, leave_out_guests): (bool, bool),
) -> GetStatsResponse {
    let named = |pairs: &[(&str, u64)]| -> std::collections::HashMap<String, u64> {
        pairs.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect()
    };
    let vms = stats.vms.into_iter().map(|vm| VmStats {
        connection: vm.connection,
        cell: vm.cell.id,
        path: vm.path.to_string_lossy().into_owned(),
        follower: vm.follower,
        build: vm.build.clone().unwrap_or_default(),
        counters: named(&vm.counters()),
        gauges: named(&vm.gauges()),
        window: vm.window().map_or_else(Default::default, |window| named(&window)),
        answered: vm.answered.is_some(),
        answer_ns: vm.answered.map_or(0, |took| took.as_nanos() as u64),
        guest: vm.guest.filter(|_| !leave_out_guests).map(|guest| GuestStats {
            build: guest.build.unwrap_or_default(),
            counters: guest.counters.into_iter().collect(),
            histograms: (guest.histograms.into_iter())
                .filter(|_| !leave_out_histograms)
                .map(|(name, buckets)| (name, Histogram { buckets }))
                .collect(),
        }),
    });
    let taken = stats.taken.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    GetStatsResponse {
        taken_unix_ns: taken.as_nanos() as u64,
        build: stats.build.to_owned(),
        counters: stats.counters.into_iter().collect(),
        gauges: named(&stats.gauges),
        vms: vms.collect(),
        // cladefs: its ephemeral cells' directory's incarnation (board 8521); none opened
        // (ENOTSUP), it keeps no ephemeral cells, which 0 says.
        incarnation: crate::ephemeral_host::host().map_or(0, |host| host.incarnation()),
    }
}

/// cladefs: the API's calls in flight, which a drain waits for, refusing more (TASK-141.4).
// Per node: drain::Work keeps a node_local count.
pub(super) static CALLS: super::drain::Work = super::drain::Work::new();

/// cladefs: the API's service, `S`, its calls counted in [`CALLS`] until they end, and refused
/// (`UNAVAILABLE`) as the daemon drains (TASK-141.4): each runs to its end in a task of its own,
/// whether or not its caller waits for its answer, as the API promises (board 5599,
/// [`crate::caller`]), so that what a call does is done once it ends, as none streams. The health
/// service's calls are not, as its watches never end.
#[derive(Clone)]
struct Drained<S>(S);

impl<S: NamedService> NamedService for Drained<S> {
    const NAME: &'static str = S::NAME;
}

/// cladefs: what a service answers a call it refuses with: tonic's, an HTTP response saying the
/// status; madsim's tonic's, the status itself (TASK-685.13).
trait Refusal {
    fn refused(status: Status) -> Self;
}

impl<Answer: Default, E> Refusal for Result<http::Response<Answer>, E> {
    fn refused(status: Status) -> Self {
        Ok(status.into_http())
    }
}

#[cfg(madsim)]
impl<M> Refusal for Result<tonic::Response<M>, Status> {
    fn refused(status: Status) -> Self {
        Err(status)
    }
}

impl<S, Call> tower::Service<Call> for Drained<S>
where
    S: tower::Service<Call>,
    S::Future: Send + 'static,
    Result<S::Response, S::Error>: Refusal + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures::future::BoxFuture<'static, Result<S::Response, S::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: Call) -> Self::Future {
        let Some(call) = CALLS.begin() else {
            let refused = Status::unavailable("the extent daemon drains, as it is about to exit");
            return Box::pin(std::future::ready(Refusal::refused(refused)));
        };
        // cladefs: in a task of its own, which runs to its end, counted until then, whether or not
        // its caller waits for the answer (board 5599): a caller's deadline, or its stream reset,
        // drops what awaits it, which stops at whatever await it was in otherwise.
        let answered = self.0.call(request);
        let detached = crate::caller::detached(async move {
            let answered = answered.await;
            drop(call);
            answered
        });
        Box::pin(async move {
            // Its call unfinished, the runtime gone: said, and refused.
            detached.await.unwrap_or_else(|unfinished| {
                log::error!("cladefsd: a control call: {unfinished}");
                Refusal::refused(Status::unavailable(unfinished.to_string()))
            })
        })
    }
}

/// cladefs: the task that has the health service report whether the daemon serves, as the log
/// names it if it panics (board 8501).
#[cfg(not(madsim))]
const HEALTH_REPORTER: &str = "health reporter";

node_local::node_local! {
    /// Whether this process serves the control API ([`serve_control`]).
    static CONTROL_SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
}

pub(super) fn is_serving() -> bool {
    CONTROL_SERVING.get().load(std::sync::atomic::Ordering::Acquire)
}

/// Reports, through the standard gRPC health service (grpc.health.v1.Health), whether the daemon
/// serves (TASK-48), `serving`: SERVING while it accepts VM processes' connections, NOT_SERVING
/// as it starts and once it stops; the whole server's status, named "", and the control API's,
/// alike. Its callers have it reported once it returns ([`super::stop_serving`] waits for that).
#[cfg(not(madsim))]
async fn report_health(reporter: &tonic_health::server::HealthReporter, serving: bool) {
    use tonic_health::ServingStatus;
    let status = if serving { ServingStatus::Serving } else { ServingStatus::NotServing };
    reporter.set_service_status("", status).await;
    reporter.set_service_status(<CladefsdServer<Control> as NamedService>::NAME, status).await;
    super::REPORTED.get().send_replace(serving);
}

/// A channel to the control API of the daemon listening at `socket`: for its clients, the API's
/// and its health service's.
#[cfg(not(madsim))]
pub async fn channel(
    socket: impl AsRef<Path>,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    let socket = socket.as_ref().to_owned();
    // The URI names no host: the connector connects to the socket, whatever it asks for.
    tonic::transport::Endpoint::from_static("http://cladefsd")
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}

/// cladefs: a channel to the control API of the daemon serving it at `address` on the simulated
/// network ([`serve_control`], TASK-685.13).
#[cfg(madsim)]
pub async fn channel(
    address: std::net::SocketAddr,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    tonic::transport::Endpoint::from_shared(format!("http://{address}"))?.connect().await
}

/// cladefs: a channel to the control API at the Unix socket `socket`, as [`channel`] makes it: a
/// blocking client's, cladefsctl's, outside the simulation, which has no Unix sockets (doc-24).
pub(crate) async fn channel_socket(
    socket: &Path,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    #[cfg(not(madsim))]
    return channel(socket).await;
    #[cfg(madsim)]
    panic!("{}: a Unix socket, which the simulation has none of", socket.display());
}

/// cladefs: a client of the control API at the Unix socket `socket` ([`channel_socket`]).
async fn connect_socket(
    socket: &Path,
) -> Result<CladefsdClient<tonic::transport::Channel>, tonic::transport::Error> {
    Ok(client(channel_socket(socket).await?))
}

/// A client of the control API of the daemon listening at `socket`, which takes answers as large
/// as the daemon sends ([`MAX_MESSAGE_BYTES`]).
#[cfg(not(madsim))]
pub async fn connect(
    socket: impl AsRef<Path>,
) -> Result<CladefsdClient<tonic::transport::Channel>, tonic::transport::Error> {
    Ok(client(channel(socket).await?))
}

/// cladefs: a client of the control API of the daemon serving it at `address` on the simulated
/// network ([`serve_control`], TASK-685.13).
#[cfg(madsim)]
pub async fn connect(
    address: std::net::SocketAddr,
) -> Result<CladefsdClient<tonic::transport::Channel>, tonic::transport::Error> {
    Ok(client(channel(address).await?))
}

/// The errno a call failed with, as its status's [`ERRNO`] entry has it, if it has one.
pub fn errno(status: &Status) -> Option<Errno> {
    // Not text: no errno said.
    let errno = status.metadata().get(ERRNO)?.to_str().ok()?;
    // Not a number: no errno said.
    errno.parse().ok().map(Errno::from_raw)
}

/// cladefs: refuses to make a cell while the host's memory is full (`super::governor::full`, board
/// 5662): RESOURCE_EXHAUSTED (ENOMEM), so that the VM manager places its VM on another host, rather
/// than this host's VMs lose to it the pages their windows use. A cell made already opens.
fn room_for_a_cell() -> Result<(), Status> {
    match super::governor::full() {
        true => Err(saying(
            Errno::ENOMEM,
            "the host's memory is full: the daemon makes no cell more".to_owned(),
        )),
        false => Ok(()),
    }
}

/// cladefs: the most references one `DecommissionHost` retires unless asked for another count
/// (board 8523): its batches' requests, references then index entries, in one call.
const DECOMMISSION_MOST: usize = 10_000;

/// cladefs: the errno of a call that failed with `error`, a bucket's, `what` it was doing, which
/// it logs.
fn bucket_errno(what: &str, error: bucket_cell::Error) -> Errno {
    let failure = local_cell::upload::Failure::Bucket(error);
    log::error!("cladefsd: {what}: {failure}");
    crate::bucket::errno(&failure).unwrap_or(Errno::EIO)
}

/// The status of a call that failed with `errno`.
fn status(errno: Errno) -> Status {
    saying(errno, format!("{errno:?}"))
}

/// The status of a call that failed with `errno`, saying `message`: as this API's errors are, and
/// those of the API of FUSE mounts of many cells (`crate::fuse::control`).
pub(crate) fn saying(errno: Errno, message: String) -> Status {
    let code = match errno {
        Errno::ENOENT => Code::NotFound,
        Errno::EEXIST => Code::AlreadyExists,
        Errno::EACCES | Errno::EPERM => Code::PermissionDenied,
        Errno::EINVAL => Code::InvalidArgument,
        // cladefs: ENOMEM, the host's memory full (board 5662); EMSGSIZE, an answer past the
        // most the API sends (board 8581).
        Errno::ENOSPC | Errno::ENOMEM | Errno::EMSGSIZE => Code::ResourceExhausted,
        Errno::ETIMEDOUT => Code::DeadlineExceeded,
        Errno::EBUSY | Errno::ESTALE | Errno::ENOTSUP | Errno::EROFS => Code::FailedPrecondition,
        _ => Code::Internal,
    };
    let mut status = Status::new(code, message);
    status.metadata_mut().insert(ERRNO, errno.raw().into());
    status
}

/// cladefs: the replication CreateCell's `replicas` and `fsync` ask for (doc-21, TASK-537.3.1):
/// none, `replicas` unset, which keeps what the cell's head says; refused (`EINVAL`) past
/// `bucket_cell::layout::MAX_F`, with an fsync level and no `replicas`, or waiting for copies an f
/// of 0 never makes.
fn wish(replicas: Option<u32>, fsync: i32) -> Result<Option<bucket_cell::layout::Wish>, Status> {
    use bucket_cell::layout::{Fsync, Wish};
    let fsync = match FsyncLevel::try_from(fsync) {
        Ok(FsyncLevel::Ordered) => Fsync::Ordered,
        Ok(FsyncLevel::Local) => Fsync::Local,
        Ok(FsyncLevel::Replicated) => Fsync::Replicated,
        Err(_) => return Err(status(Errno::EINVAL)),
    };
    let Some(replicas) = replicas else {
        return match fsync {
            Fsync::Ordered => Ok(None),
            _ => Err(status(Errno::EINVAL)),
        };
    };
    let f = u8::try_from(replicas).map_err(|_| status(Errno::EINVAL))?;
    Wish::new(f, fsync).map(Some).ok_or_else(|| status(Errno::EINVAL))
}

/// What StatCell's FsyncLevel is of `fsync`.
fn fsync_level(fsync: bucket_cell::layout::Fsync) -> FsyncLevel {
    match fsync {
        bucket_cell::layout::Fsync::Ordered => FsyncLevel::Ordered,
        bucket_cell::layout::Fsync::Local => FsyncLevel::Local,
        bucket_cell::layout::Fsync::Replicated => FsyncLevel::Replicated,
    }
}

/// The reply to a call that did, or failed with an errno.
/// What StatCell says of a cell that is and holds `state`.
fn stat_response(state: cells::CellState) -> StatCellResponse {
    let upload = state.upload.unwrap_or(local_cell::upload::UploadStatus {
        taking: None,
        uploaded: 0,
        pending_bytes: 0,
        oldest_pending: None,
        stalls: 0,
        replication: None,
    });
    let millis = |time: std::time::SystemTime| {
        time.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
    };
    let waited = |since: std::time::SystemTime| {
        since.elapsed().map_or(0, |waited| waited.as_millis() as u64)
    };
    let replicated = upload.taking.and_then(|taking| taking.replication);
    StatCellResponse {
        tenant: state.cell.tenant,
        cell: state.cell.id,
        capacity: state.info.capacity,
        used: state.info.used,
        available: state.info.available,
        public: state.public,
        open: state.open,
        own: state.holding.bytes.own,
        inherited: state.holding.bytes.inherited,
        own_extents: state.holding.extents.own,
        inherited_extents: state.holding.extents.inherited,
        committed: state.committed,
        uploading: state.upload.is_some(),
        taken_token: upload.taking.map_or(0, |taking| taking.token),
        taken_epoch: upload.taking.map_or(0, |taking| taking.epoch),
        taken_unix_ms: upload.taking.map_or(0, |taking| millis(taking.taken)),
        uploaded: upload.uploaded,
        pending_upload_bytes: upload.pending_bytes,
        oldest_pending_ms: upload.oldest_pending.map_or(0, waited),
        replicas: replicated.map_or(0, |replicated| u32::from(replicated.f)),
        fsync: replicated.map_or(FsyncLevel::Ordered, |replicated| fsync_level(replicated.fsync))
            as i32,
        configuration: replicated.and_then(|replicated| replicated.configuration).unwrap_or(0),
        degraded: upload.replication.is_some_and(|replication| replication.degraded),
        members_up: upload.replication.map_or(0, |replication| replication.members_up),
        active_replicas: upload.replication.map_or(0, |replication| u32::from(replication.f)),
        forced_after: replicated.and_then(|replicated| replicated.forced_after),
    }
}

fn reply<T>(done: Result<T, Errno>) -> Result<Response<T>, Status> {
    done.map(Response::new).map_err(status)
}

/// The cell a request names by `path`.
fn path(path: String) -> Result<PathBuf, Status> {
    if path.is_empty() {
        return Err(status(Errno::EINVAL));
    }
    Ok(PathBuf::from(path))
}

/// The API's calls, served from the daemon's cells, and its grants.
struct Control {
    grants: Arc<Grants>,
}

impl Control {
    /// The tenant of a new cell made at `path`, that the caller names `tenant`
    /// (TASK-364): in a granted tenant's directory, that tenant's, which `tenant` is or is 0 for
    /// (`EPERM` otherwise); elsewhere, `tenant`. A cell there already, a version the VM
    /// manager gives a tenant to restore from (TASK-154), may be another tenant's: the tenant's VM
    /// processes write none of it (`Tenant::reaches`), and its restores are of their own.
    fn new_cell_at(&self, path: &Path, tenant: u64) -> Result<u64, Status> {
        match self.grants.tenant_at(path) {
            Some(granted) if tenant == crate::host_cell::DEFAULT_TENANT || tenant == granted => {
                Ok(granted)
            }
            Some(_) => Err(status(Errno::EPERM)),
            None => Ok(tenant),
        }
    }
}

#[tonic::async_trait]
impl Cladefsd for Control {
    async fn create_cell(
        &self,
        request: Request<CreateCellRequest>,
    ) -> Result<Response<CreateCellResponse>, Status> {
        let CreateCellRequest {
            path: cell_dir,
            capacity,
            token,
            tenant,
            cell,
            kind,
            replicas,
            fsync,
            force,
        } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        room_for_a_cell()?;
        let replication = wish(replicas, fsync)?;
        // cladefs: a replicated cell's members are the daemon's peers, in their replica
        // directories (doc-21): without them, refused before anything is made.
        if replication.is_some_and(|wish| wish.f >= 1) && crate::bucket::members().is_none() {
            return Err(saying(
                Errno::ENOTSUP,
                "a replicated cell needs the daemon's peers and replica directory \
                 (CLADEFSD_PEER_ADDRESS, CLADEFSD_PEERS, CLADEFSD_REPLICAS)"
                    .to_owned(),
            ));
        }
        let kind = match CellKind::try_from(kind) {
            Ok(CellKind::Fxfs) => Kind::Fxfs,
            Ok(CellKind::Sqlite) => Kind::Sqlite,
            Ok(CellKind::Memory) => Kind::Memory,
            Err(_) => return Err(status(Errno::EINVAL)),
        };
        let tenant = match cell {
            0 => self.new_cell_at(&cell_dir, tenant)?,
            _ => tenant,
        };
        let cell = crate::CellId { tenant, id: cell };
        let made = cells::create(cell_dir, capacity, cell, (token, kind, replication, force)).await;
        reply(made.map(|made| CreateCellResponse { tenant: made.tenant, cell: made.id }))
    }

    async fn import_image(
        &self,
        request: Request<ImportImageRequest>,
    ) -> Result<Response<ImportImageResponse>, Status> {
        let ImportImageRequest { path: cell_dir, capacity, image, reference, tenant } =
            request.into_inner();
        let (cell_dir, image) = (path(cell_dir)?, path(image)?);
        let reference = Some(reference).filter(|reference| !reference.is_empty());
        let tenant = self.new_cell_at(&cell_dir, tenant)?;
        let made = cells::import(cell_dir, capacity, image, reference, tenant).await;
        reply(made.map(|made| ImportImageResponse { tenant: made.tenant, cell: made.id }))
    }

    async fn fork_cell(
        &self,
        request: Request<ForkCellRequest>,
    ) -> Result<Response<ForkCellResponse>, Status> {
        let ForkCellRequest { from, to, version, tenant: of, cell, ephemeral } =
            request.into_inner();
        let to = path(to)?;
        room_for_a_cell()?;
        let tenant = self.grants.tenant_at(&to);
        let of = crate::CellId { tenant: of, id: cell };
        let forked = match version {
            // cladefs: ephemeral cells are forked from versions alone (doc-25, TASK-698.3).
            0 if ephemeral => Err(Errno::EINVAL),
            0 => cells::fork(path(from)?, to, tenant).await,
            _ if !from.is_empty() || cell == 0 => Err(Errno::EINVAL),
            _ if ephemeral => cells::fork_ephemeral_of_version(of, version, to, tenant).await,
            _ => cells::fork_of_version(of, version, to, tenant).await,
        };
        reply(forked.map(|forked| ForkCellResponse { tenant: forked.tenant, cell: forked.id }))
    }

    async fn migrate_cell(
        &self,
        request: Request<MigrateCellRequest>,
    ) -> Result<Response<MigrateCellResponse>, Status> {
        let MigrateCellRequest { path: cell_dir, peer, to } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        let migrating = super::migration::migrate(cell_dir, peer, to).await;
        reply(migrating.map(|migration| MigrateCellResponse { migration }))
    }

    async fn hand_over_cell(
        &self,
        request: Request<HandOverCellRequest>,
    ) -> Result<Response<HandOverCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(super::migration::hand_over(cell_dir).await.map(|()| HandOverCellResponse {}))
    }

    async fn decommission_host(
        &self,
        request: Request<DecommissionHostRequest>,
    ) -> Result<Response<DecommissionHostResponse>, Status> {
        let DecommissionHostRequest { incarnation, most } = request.into_inner();
        let most = match most {
            0 => DECOMMISSION_MOST,
            most => usize::try_from(most).unwrap_or(usize::MAX),
        };
        let said = base32_id::Short(incarnation);
        let decommissioned = async {
            // cladefs: an incarnation no host makes (each makes an odd one, `ephemeral_host`), or
            // this daemon's own, whose live cells' references it would retire (board 8519): a
            // mistyped ID, refused before anything is written. Its directory not opened (ENOTSUP),
            // the daemon has no incarnation to be.
            let own =
                crate::ephemeral_host::host().is_ok_and(|host| host.incarnation() == incarnation);
            if incarnation % 2 == 0 || own {
                log::warn!(
                    "cladefsd: decommissioning host {said}: refused, no host's to decommission"
                );
                return Err(Errno::EINVAL);
            }
            let bucket = cells::daemon_bucket()?;
            let what = format!("decommissioning host {said}");
            let buckets =
                bucket.ephemeral_buckets().await.map_err(|err| bucket_errno(&what, err))?;
            let decommissioning =
                bucket_cell::lifecycle::decommission(&*bucket, &buckets, incarnation, most);
            let done = decommissioning.await.map_err(|err| bucket_errno(&what, err))?;
            let bucket_cell::lifecycle::Decommissioned { retired, left } = done;
            match retired + left {
                0 => log::warn!("cladefsd: decommissioning host {said}: no references of its"),
                _ => log::info!(
                    "cladefsd: decommissioning host {said}: retired {retired}, left {left}"
                ),
            }
            Ok(DecommissionHostResponse { retired: retired as u64, left: left as u64 })
        };
        reply(decommissioned.await)
    }

    async fn list_ephemeral_hosts(
        &self,
        _: Request<ListEphemeralHostsRequest>,
    ) -> Result<Response<ListEphemeralHostsResponse>, Status> {
        let listed = async {
            let bucket = cells::daemon_bucket()?;
            let what = "listing the ephemeral cells' hosts";
            let buckets =
                bucket.ephemeral_buckets().await.map_err(|err| bucket_errno(what, err))?;
            let hosts = bucket_cell::lifecycle::ephemeral_hosts(&*bucket, &buckets).await;
            let hosts = hosts.map_err(|refused| bucket_errno(what, refused.into()))?;
            let hosts = hosts.into_iter().map(|(incarnation, host)| EphemeralHost {
                incarnation,
                references: host.references as u64,
                decommissioned: host.decommissioned,
            });
            Ok(ListEphemeralHostsResponse { hosts: hosts.collect() })
        };
        let listed = listed.await.map_err(status)?;
        bounded("ListEphemeralHosts", listed, "decommission hosts gone for good")
    }

    async fn delete_cell(
        &self,
        request: Request<DeleteCellRequest>,
    ) -> Result<Response<DeleteCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(cells::delete(cell_dir).await.map(|()| DeleteCellResponse {}))
    }

    async fn drop_cell(
        &self,
        request: Request<DropCellRequest>,
    ) -> Result<Response<DropCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(cells::drop_here(cell_dir).await.map(|()| DropCellResponse {}))
    }

    async fn resize_cell(
        &self,
        request: Request<ResizeCellRequest>,
    ) -> Result<Response<ResizeCellResponse>, Status> {
        let ResizeCellRequest { path: cell_dir, capacity } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        reply(cells::resize(cell_dir, capacity).await.map(|()| ResizeCellResponse {}))
    }

    async fn place_cell(
        &self,
        request: Request<PlaceCellRequest>,
    ) -> Result<Response<PlaceCellResponse>, Status> {
        let PlaceCellRequest { path: cell_dir, token, force } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        reply(cells::place(cell_dir, token, force).await.map(|()| PlaceCellResponse {}))
    }

    async fn make_public(
        &self,
        request: Request<MakePublicRequest>,
    ) -> Result<Response<MakePublicResponse>, Status> {
        let MakePublicRequest { path: cell_dir, attach_only } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        reply(cells::make_public(cell_dir, attach_only).await.map(|()| MakePublicResponse {}))
    }

    async fn make_version(
        &self,
        request: Request<MakeVersionRequest>,
    ) -> Result<Response<MakeVersionResponse>, Status> {
        let MakeVersionRequest { path: cell_dir, keep_last } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        let keep_last = Some(keep_last).filter(|&keep| keep > 0);
        let made = cells::make_version(cell_dir, keep_last).await;
        reply(made.map(|version| MakeVersionResponse { version }))
    }

    async fn list_versions(
        &self,
        request: Request<ListVersionsRequest>,
    ) -> Result<Response<ListVersionsResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        let versions = cells::versions(cell_dir).await.map_err(status)?;
        let listed = ListVersionsResponse {
            versions: (versions.into_iter())
                .map(|crate::VersionInfo { id, uploaded, shared }| Version { id, uploaded, shared })
                .collect(),
        };
        bounded("ListVersions", listed, "drop versions (DropVersions)")
    }

    async fn drop_versions(
        &self,
        request: Request<DropVersionsRequest>,
    ) -> Result<Response<DropVersionsResponse>, Status> {
        let DropVersionsRequest { path: cell_dir, versions } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        let dropped = cells::drop_versions(cell_dir, versions.into_iter().collect()).await;
        reply(dropped.map(|()| DropVersionsResponse {}))
    }

    async fn share_version(
        &self,
        request: Request<ShareVersionRequest>,
    ) -> Result<Response<ShareVersionResponse>, Status> {
        let ShareVersionRequest { path: cell_dir, version, shared } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        let shared = cells::share_version(cell_dir, version, shared).await;
        reply(shared.map(|()| ShareVersionResponse {}))
    }

    async fn attach_base(
        &self,
        request: Request<AttachBaseRequest>,
    ) -> Result<Response<AttachBaseResponse>, Status> {
        let AttachBaseRequest { path: cell_dir, base } = request.into_inner();
        let (cell_dir, base) = (path(cell_dir)?, path(base)?);
        let grants = self.grants.clone();
        let allowed = move |cell_dir: &Path, base: &Path| grants.same_tenant(cell_dir, base);
        reply(cells::attach(cell_dir, base, allowed).await.map(|()| AttachBaseResponse {}))
    }

    async fn sync_cell(
        &self,
        request: Request<SyncCellRequest>,
    ) -> Result<Response<SyncCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(cells::sync(cell_dir).await.map(|()| SyncCellResponse {}))
    }

    async fn stat_cell(
        &self,
        request: Request<StatCellRequest>,
    ) -> Result<Response<StatCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(cells::stat(cell_dir).await.map(stat_response))
    }

    async fn list_cells(
        &self,
        _request: Request<ListCellsRequest>,
    ) -> Result<Response<ListCellsResponse>, Status> {
        let cells = cells::list().map_err(status)?.into_iter().map(|(path, state)| ListedCell {
            path: path.to_string_lossy().into_owned(),
            stat: Some(stat_response(state)),
        });
        let listed = ListCellsResponse { stores: cells.collect() };
        bounded("ListCells", listed, "StatCell each cell instead")
    }

    async fn get_counters(
        &self,
        _request: Request<GetCountersRequest>,
    ) -> Result<Response<GetCountersResponse>, Status> {
        let crate::stats::Stats {
            maps,
            map_ns,
            faults,
            fault_ns,
            page_ins,
            page_in_bytes,
            fetched_bytes,
            reads,
            read_bytes,
            read_stored_bytes,
            prefetches,
            unpacks,
            // The VM processes' changes to their windows, as they told the daemon, which its
            // stats file and metrics report: not the API.
            stage_ops: _,
            stage_ns: _,
            commits,
            commit_ns,
            commit_store_ns,
            commit_wait_ns,
            segment_closes,
            segment_close_ns,
            // cladefs: the maps the daemon sent the VM processes' windows, which its stats file and
            // metrics report: not the API (TASK-108.1).
            stage_maps: _,
            stage_map_bytes: _,
            // And the VM processes' changes that took access away, as they told it (TASK-91.16).
            stage_overwrites: _,
            stage_leave_outs: _,
            // And the extent cache's trims (TASK-109.2).
            cache_trims: _,
            cache_trim_ns: _,
            // And the faults by kind (TASK-111.1).
            fault_stores: _,
            fault_refusals: _,
            fault_waits: _,
            // And the windows' runs of pages (TASK-131.1).
            stage_segments: _,
            // And the runs of private pages they made (TASK-164.10).
            stage_private_runs: _,
            stage_private_pages: _,
            stage_private_ns: _,
            vm_private_requests: _,
            vm_private_request_ns: _,
            // How well forks' boots were predicted, and what of it was read into memory ahead of
            // them, which its stats file and metrics report.
            predicted_read: _,
            predicted_unread: _,
            read_unpredicted: _,
            prefetch_page_ins: _,
        } = crate::stats::stats();
        use bucket_cell::object_bucket::{Request, Upload};
        let bucket = cells::daemon_bucket().map_err(status)?.stats();
        let object_storage = Some(ObjectStorageStats {
            requests: (Request::ALL.into_iter())
                .map(|request| (request.name().to_owned(), bucket.requests(request)))
                .collect(),
            uploaded_bytes: (Upload::ALL.into_iter())
                .map(|upload| (upload.name().to_owned(), bucket.uploaded(upload)))
                .collect(),
            downloaded_bytes: bucket.downloaded,
            failures: (Request::ALL.into_iter())
                .map(|request| (request.name().to_owned(), bucket.failures(request)))
                .collect(),
        });
        Ok(Response::new(GetCountersResponse {
            maps,
            map_ns,
            faults,
            fault_ns,
            page_ins,
            page_in_bytes,
            fetched_bytes,
            reads,
            read_bytes,
            read_stored_bytes,
            prefetches,
            unpacks,
            object_storage,
            commits,
            commit_ns,
            commit_store_ns,
            commit_wait_ns,
            segment_closes,
            segment_close_ns,
        }))
    }

    async fn get_stats(
        &self,
        request: Request<GetStatsRequest>,
    ) -> Result<Response<GetStatsResponse>, Status> {
        let GetStatsRequest { wait_ms, leave_out_histograms, leave_out_guests, leave_out_vms } =
            request.into_inner();
        let wait = match wait_ms {
            0 => super::stats::WAIT,
            ms => std::time::Duration::from_millis(ms.into()),
        };
        let stats = super::stats::take(wait, !leave_out_vms).await;
        let answer = stats_response(stats, (leave_out_histograms, leave_out_guests));
        let smaller = match (leave_out_histograms, leave_out_guests, leave_out_vms) {
            (false, false, _) => "leave out the guests' histograms (leave_out_histograms)",
            (true, false, _) => "leave out the guests' stats (leave_out_guests)",
            (_, true, _) => "leave out the VM processes (leave_out_vms)",
        };
        bounded("GetStats", answer, smaller)
    }

    async fn grant(
        &self,
        request: Request<GrantRequest>,
    ) -> Result<Response<GrantResponse>, Status> {
        let GrantRequest { tenant, dir, uid, gid } = request.into_inner();
        let dir = path(dir)?;
        let grants = self.grants.clone();
        let granted = cells::blocking(move || {
            let socket = grants.grant(tenant, &dir, (uid, gid))?;
            let socket = socket.into_os_string().into_string().map_err(|_| Errno::EINVAL)?;
            Ok(GrantResponse { socket })
        });
        reply(granted.await)
    }

    async fn revoke(
        &self,
        request: Request<RevokeRequest>,
    ) -> Result<Response<RevokeResponse>, Status> {
        let tenant = request.into_inner().tenant;
        let grants = self.grants.clone();
        reply(cells::blocking(move || grants.revoke(tenant).map(|()| RevokeResponse {})).await)
    }

    async fn set_memory_limit(
        &self,
        request: Request<SetMemoryLimitRequest>,
    ) -> Result<Response<SetMemoryLimitResponse>, Status> {
        let SetMemoryLimitRequest { tenant, bytes } = request.into_inner();
        reply(memory::set_limit(tenant, bytes).map(|()| SetMemoryLimitResponse {}))
    }

    async fn set_memory_weight(
        &self,
        request: Request<SetMemoryWeightRequest>,
    ) -> Result<Response<SetMemoryWeightResponse>, Status> {
        let SetMemoryWeightRequest { tenant, weight } = request.into_inner();
        reply(memory::set_weight(tenant, weight).map(|()| SetMemoryWeightResponse {}))
    }

    async fn set_tenant_in_flight(
        &self,
        request: Request<SetTenantInFlightRequest>,
    ) -> Result<Response<SetTenantInFlightResponse>, Status> {
        let SetTenantInFlightRequest { tenant, bytes } = request.into_inner();
        reply(memory::set_in_flight(tenant, bytes).map(|()| SetTenantInFlightResponse {}))
    }

    async fn shrink_memory(
        &self,
        request: Request<ShrinkMemoryRequest>,
    ) -> Result<Response<ShrinkMemoryResponse>, Status> {
        let ShrinkMemoryRequest { tenant, target_bytes } = request.into_inner();
        // cladefs: on the call's task: its evictions' passes are synchronous, and it waits for the
        // pages they evict holding no thread (board 2946).
        let shrunk = memory::shrink(tenant, target_bytes).await;
        reply(shrunk.map(|bytes| ShrinkMemoryResponse { bytes }))
    }

    async fn set_disk_limit(
        &self,
        request: Request<SetDiskLimitRequest>,
    ) -> Result<Response<SetDiskLimitResponse>, Status> {
        let SetDiskLimitRequest { tenant, bytes } = request.into_inner();
        reply(disk::set_limit(tenant, bytes).map(|()| SetDiskLimitResponse {}))
    }

    async fn get_tenant_disk(
        &self,
        request: Request<GetTenantDiskRequest>,
    ) -> Result<Response<GetTenantDiskResponse>, Status> {
        let tenant = request.into_inner().tenant;
        let response = |disk::Counted { bytes, limit, reserved }| GetTenantDiskResponse {
            bytes,
            limit_bytes: limit,
            reserved_bytes: reserved,
        };
        reply(disk::count(tenant).map(response))
    }

    async fn set_disk_weight(
        &self,
        request: Request<SetDiskWeightRequest>,
    ) -> Result<Response<SetDiskWeightResponse>, Status> {
        let SetDiskWeightRequest { tenant, weight } = request.into_inner();
        reply(disk::set_weight(tenant, weight).map(|()| SetDiskWeightResponse {}))
    }

    async fn set_peers(
        &self,
        request: Request<SetPeersRequest>,
    ) -> Result<Response<SetPeersResponse>, Status> {
        let SetPeersRequest { addresses } = request.into_inner();
        reply(super::peers::set(addresses).map(|()| SetPeersResponse {}))
    }

    async fn shred_cell(
        &self,
        request: Request<ShredCellRequest>,
    ) -> Result<Response<ShredCellResponse>, Status> {
        let ShredCellRequest { tenant, cell } = request.into_inner();
        let cell = crate::CellId { tenant, id: cell };
        let bucket = cells::daemon_bucket().map_err(status)?;
        let shredded = crate::bucket::shred_cell(&*bucket, cell).await.map_err(|err| {
            let failure = local_cell::upload::Failure::Bucket(err);
            let errno = crate::bucket::errno(&failure).unwrap_or(Errno::EIO);
            log::error!("cladefsd: shredding cell {cell}: {failure}");
            saying(errno, format!("shredding cell {cell}: {failure}"))
        })?;
        let (removed, forgotten) = shredded;
        let keks_removed = u32::try_from(removed).unwrap_or(u32::MAX);
        Ok(Response::new(ShredCellResponse {
            keks_removed,
            forgotten: Some(forgotten_of(forgotten)),
        }))
    }

    async fn forget_tenant_keys(
        &self,
        request: Request<ForgetTenantKeysRequest>,
    ) -> Result<Response<ForgetTenantKeysResponse>, Status> {
        let ForgetTenantKeysRequest { tenant, why } = request.into_inner();
        let why = match why.is_empty() {
            true => {
                format!("tenant {tenant}: its KEK is revoked or destroyed, the control API says")
            }
            false => why,
        };
        let scope = bucket_cell::keys::Scope::Tenant(tenant);
        let forgotten = cells::blocking(move || Ok(crate::bucket::forget(scope, &why))).await;
        reply(
            forgotten.map(|forgotten| ForgetTenantKeysResponse {
                forgotten: Some(forgotten_of(forgotten)),
            }),
        )
    }

    async fn shred_tenant(
        &self,
        request: Request<ShredTenantRequest>,
    ) -> Result<Response<ShredTenantResponse>, Status> {
        let ShredTenantRequest { tenant } = request.into_inner();
        let (destroyed, forgotten) = crate::bucket::shred_tenant(tenant).await.map_err(|why| {
            log::error!("cladefsd: shredding tenant {tenant}: {why}");
            saying(Errno::ENOTSUP, why)
        })?;
        let shredded_now = destroyed == bucket_cell::bucket_kms::Destroyed::Now;
        Ok(Response::new(ShredTenantResponse {
            shredded_now,
            forgotten: Some(forgotten_of(forgotten)),
        }))
    }
}

/// cladefs: what a host forgot of a tenant's or a cell's keys, as the API has it (TASK-411.7).
fn forgotten_of(forgotten: crate::bucket::Forgotten) -> Forgotten {
    let count = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
    let crate::bucket::Forgotten { stores, rings, kept, extents, bytes } = forgotten;
    Forgotten { stores: count(stores), rings: count(rings), kept: count(kept), extents, bytes }
}

/// A client of a daemon's control API that blocks, for callers without a runtime of their own:
/// cladefsctl, and tests. Its errors carry the errno a call failed with, if it did
/// ([`crate::Error::errno`]).
pub struct Client {
    runtime: tokio::runtime::Runtime,
    client: CladefsdClient<tonic::transport::Channel>,
    /// cladefs: how long each call, and the connecting, may take, if bounded (board 8585).
    timeout: Option<std::time::Duration>,
}

impl Client {
    /// A client of the daemon whose control socket is at `socket`.
    pub fn connect(socket: impl AsRef<Path>) -> Result<Self, crate::Error> {
        Self::connect_within(socket, None)
    }

    /// cladefs: a client of the daemon whose control socket is at `socket`, connecting, and then
    /// each of its calls, within `timeout` if given (board 8585): one that takes longer fails with
    /// `ETIMEDOUT`, the call's work going on in the daemon, which may still complete it, as the
    /// API's calls run to their end whether or not their callers wait.
    pub fn connect_within(
        socket: impl AsRef<Path>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Self, crate::Error> {
        let socket = socket.as_ref();
        // cladefs: outside the simulation: a blocking client, cladefsctl's, at its entry.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(crate::Error::new)?;
        let what =
            format!("connecting to the extent daemon's control socket at {}", socket.display());
        // cladefs: outside the simulation, as above.
        let connected = runtime.block_on(within(timeout, connect_socket(socket)));
        let client = match connected {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => return Err(crate::Error::new(anyhow::Error::new(err).context(what))),
            Err(timeout) => return Err(timed_out(what, timeout)),
        };
        Ok(Self { runtime, client, timeout })
    }

    /// Makes an empty cell at `path` of `capacity` bytes, of `cell`, an Fxfs cell:
    /// of a new cell if its cell's ID is 0, and a new cell there if its ID is; placed at `token`
    /// if given, its taking forced if `force` (TASK-681); and returns its cell (`CreateCell`,
    /// TASK-364).
    pub fn create_cell(
        &self,
        path: &Path,
        capacity: u64,
        cell: crate::CellId,
        (token, force): (Option<u64>, bool),
    ) -> Result<crate::CellId, crate::Error> {
        let request = CreateCellRequest {
            path: name(path)?,
            capacity,
            token,
            tenant: cell.tenant,
            cell: cell.id,
            kind: CellKind::Fxfs.into(),
            force,
            ..CreateCellRequest::default()
        };
        let response = self
            .call(format!("making the cell at {}", path.display()), |mut c| async move {
                c.create_cell(request).await
            })?;
        Ok(crate::CellId { tenant: response.tenant, id: response.cell })
    }

    /// Makes a cell at `path` of `capacity` bytes holding the container image at `image`, the one
    /// `reference` names in it if given, a new cell of the default tenant's, and
    /// returns its cell (`ImportImage`).
    pub fn import_image(
        &self,
        path: &Path,
        capacity: u64,
        image: &Path,
        reference: Option<&str>,
    ) -> Result<crate::CellId, crate::Error> {
        let request = ImportImageRequest {
            path: name(path)?,
            capacity,
            image: name(image)?,
            reference: reference.unwrap_or_default().to_owned(),
            tenant: crate::host_cell::DEFAULT_TENANT,
        };
        let response = self.call(format!("importing {}", image.display()), |mut c| async move {
            c.import_image(request).await
        })?;
        Ok(crate::CellId { tenant: response.tenant, id: response.cell })
    }

    /// Forks the cell at `from` to a new cell at `to`, and returns its cell (`ForkCell`).
    pub fn fork_cell(&self, from: &Path, to: &Path) -> Result<crate::CellId, crate::Error> {
        let request = ForkCellRequest { from: name(from)?, to: name(to)?, ..Default::default() };
        let what = format!("forking the cell at {} to {}", from.display(), to.display());
        let response = self.call(what, |mut c| async move { c.fork_cell(request).await })?;
        Ok(crate::CellId { tenant: response.tenant, id: response.cell })
    }

    /// Forks the version `from` to a new cell at `to`, on any host, and returns its cell
    /// (`ForkCell`'s `version`).
    pub fn fork_version(
        &self,
        from: crate::CellVersion,
        to: &Path,
    ) -> Result<crate::CellId, crate::Error> {
        let crate::CellVersion { tenant, cell, version } = from;
        let request =
            ForkCellRequest { to: name(to)?, version, tenant, cell, ..Default::default() };
        let what = format!("forking version {version} of cell {cell} to {}", to.display());
        let response = self.call(what, |mut c| async move { c.fork_cell(request).await })?;
        Ok(crate::CellId { tenant: response.tenant, id: response.cell })
    }

    /// cladefs: forks the version `from` to a new ephemeral cell at `to`, kept on the daemon's host
    /// alone (doc-25), and returns its cell (`ForkCell`'s `ephemeral`).
    pub fn fork_ephemeral(
        &self,
        from: crate::CellVersion,
        to: &Path,
    ) -> Result<crate::CellId, crate::Error> {
        let crate::CellVersion { tenant, cell, version } = from;
        let request = ForkCellRequest {
            to: name(to)?,
            version,
            tenant,
            cell,
            ephemeral: true,
            ..Default::default()
        };
        let what =
            format!("forking version {version} of cell {cell} ephemerally to {}", to.display());
        let response = self.call(what, |mut c| async move { c.fork_cell(request).await })?;
        Ok(crate::CellId { tenant: response.tenant, id: response.cell })
    }

    /// cladefs: begins the live migration of the ephemeral cell at `path` to the daemon
    /// whose peer address is `peer`, its cell going at `to` there (`MigrateCell`): returns its ID.
    pub fn migrate_cell(&self, path: &Path, peer: &str, to: &Path) -> Result<u64, crate::Error> {
        let request =
            MigrateCellRequest { path: name(path)?, peer: peer.to_owned(), to: name(to)? };
        let what = format!("migrating the cell at {} to {peer}", path.display());
        let response = self.call(what, |mut c| async move { c.migrate_cell(request).await })?;
        Ok(response.migration)
    }

    /// cladefs: hands the ephemeral cell at `path` over to its migration's target
    /// (`HandOverCell`).
    pub fn hand_over_cell(&self, path: &Path) -> Result<(), crate::Error> {
        let request = HandOverCellRequest { path: name(path)? };
        let what = format!("handing the cell at {} over", path.display());
        self.call(what, |mut c| async move { c.hand_over_cell(request).await })?;
        Ok(())
    }

    /// cladefs: decommissions the host of incarnation `incarnation`, gone for good: its ephemeral
    /// cells' references retired, `most` of them at most, 0 for the daemon's default (doc-25,
    /// `DecommissionHost`, board 8523). Returns how many it retired, and how many it left.
    pub fn decommission_host(
        &self,
        incarnation: u64,
        most: u64,
    ) -> Result<DecommissionHostResponse, crate::Error> {
        let request = DecommissionHostRequest { incarnation, most };
        let what = format!("decommissioning incarnation {}", base32_id::Short(incarnation));
        self.call(what, |mut c| async move { c.decommission_host(request).await })
    }

    /// cladefs: the incarnations of hosts with ephemeral cells' references in the buckets, or
    /// decommissioned there, in order (`ListEphemeralHosts`, board 8521).
    pub fn ephemeral_hosts(&self) -> Result<Vec<EphemeralHost>, crate::Error> {
        let what = "listing the ephemeral cells' hosts".to_owned();
        let request = ListEphemeralHostsRequest {};
        let response =
            self.call(what, |mut c| async move { c.list_ephemeral_hosts(request).await })?;
        Ok(response.hosts)
    }

    /// Makes a version of the cell at `path`, keeping its newest `keep_last` if given,
    /// and returns its ID (`MakeVersion`).
    pub fn make_version(&self, path: &Path, keep_last: Option<u32>) -> Result<u64, crate::Error> {
        let request = MakeVersionRequest { path: name(path)?, keep_last: keep_last.unwrap_or(0) };
        let what = format!("making a version of the cell at {}", path.display());
        let response = self.call(what, |mut c| async move { c.make_version(request).await })?;
        Ok(response.version)
    }

    /// The versions of the cell at `path` (`ListVersions`).
    pub fn list_versions(&self, path: &Path) -> Result<Vec<crate::VersionInfo>, crate::Error> {
        let request = ListVersionsRequest { path: name(path)? };
        let what = format!("listing the versions of the cell at {}", path.display());
        let response = self.call(what, |mut c| async move { c.list_versions(request).await })?;
        Ok((response.versions.into_iter())
            .map(|Version { id, uploaded, shared }| crate::VersionInfo { id, uploaded, shared })
            .collect())
    }

    /// Drops the versions `ids` of the cell at `path` (`DropVersions`).
    pub fn drop_versions(&self, path: &Path, ids: &[u64]) -> Result<(), crate::Error> {
        let request = DropVersionsRequest { path: name(path)?, versions: ids.to_vec() };
        let what = format!("dropping versions of the cell at {}", path.display());
        self.call(what, |mut c| async move { c.drop_versions(request).await }).map(drop)
    }

    /// Shares the version `id` of the cell at `path`, or no longer (`ShareVersion`).
    pub fn share_version(&self, path: &Path, id: u64, shared: bool) -> Result<(), crate::Error> {
        let request = ShareVersionRequest { path: name(path)?, version: id, shared };
        let what = format!("sharing version {id} of the cell at {}", path.display());
        self.call(what, |mut c| async move { c.share_version(request).await }).map(drop)
    }

    /// Deletes the cell at `path` (`DeleteCell`).
    pub fn delete_cell(&self, path: &Path) -> Result<(), crate::Error> {
        let request = DeleteCellRequest { path: name(path)? };
        let what = format!("deleting the cell at {}", path.display());
        self.call(what, |mut c| async move { c.delete_cell(request).await }).map(drop)
    }

    /// Deletes this host's copy of the cell at `path`, its branch staying in the daemon's bucket
    /// (`DropCell`).
    pub fn drop_cell(&self, path: &Path) -> Result<(), crate::Error> {
        let request = DropCellRequest { path: name(path)? };
        let what = format!("dropping the cell at {}", path.display());
        self.call(what, |mut c| async move { c.drop_cell(request).await }).map(drop)
    }

    /// Sets the capacity of the cell at `path` to `capacity` bytes (`ResizeCell`).
    pub fn resize_cell(&self, path: &Path, capacity: u64) -> Result<(), crate::Error> {
        let request = ResizeCellRequest { path: name(path)?, capacity };
        let what = format!("resizing the cell at {} to {capacity} bytes", path.display());
        self.call(what, |mut c| async move { c.resize_cell(request).await }).map(drop)
    }

    /// Places the cell at `path` at the placement whose fencing token is `token` (`PlaceCell`),
    /// its takings forced if `force`.
    pub fn place_cell(&self, path: &Path, token: u64, force: bool) -> Result<(), crate::Error> {
        let request = PlaceCellRequest { path: name(path)?, token, force };
        let what = format!("placing the cell at {}", path.display());
        self.call(what, |mut c| async move { c.place_cell(request).await }).map(drop)
    }

    /// Makes the cell at `path` a public base (`MakePublic`).
    pub fn make_public(&self, path: &Path) -> Result<(), crate::Error> {
        let request = MakePublicRequest { path: name(path)?, attach_only: false };
        let what = format!("making the cell at {} public", path.display());
        self.call(what, |mut c| async move { c.make_public(request).await }).map(drop)
    }

    /// Makes the fork at `path` public, its cell alone, to attach to cells, not to fork
    /// (`MakePublic` with `attach_only`).
    pub fn make_public_attach_only(&self, path: &Path) -> Result<(), crate::Error> {
        let request = MakePublicRequest { path: name(path)?, attach_only: true };
        let what = format!("making the fork at {} public to attach", path.display());
        self.call(what, |mut c| async move { c.make_public(request).await }).map(drop)
    }

    /// Attaches the fork at `base` to the cell at `path` (`AttachBase`).
    pub fn attach_base(&self, path: &Path, base: &Path) -> Result<(), crate::Error> {
        let request = AttachBaseRequest { path: name(path)?, base: name(base)? };
        let what = format!("attaching {} to {}", base.display(), path.display());
        self.call(what, |mut c| async move { c.attach_base(request).await }).map(drop)
    }

    /// What the cell at `path` is, and holds (`StatCell`).
    pub fn stat_cell(&self, path: &Path) -> Result<StatCellResponse, crate::Error> {
        let request = StatCellRequest { path: name(path)? };
        let what = format!("asking what the cell at {} holds", path.display());
        self.call(what, |mut c| async move { c.stat_cell(request).await })
    }

    /// The cells the daemon serves, with what each is and holds (`ListCells`).
    pub fn list_cells(&self) -> Result<Vec<ListedCell>, crate::Error> {
        let what = "listing the daemon's cells".to_owned();
        let listed =
            self.call(what, |mut c| async move { c.list_cells(ListCellsRequest {}).await })?;
        Ok(listed.stores)
    }

    /// The ID of the cell at `path` (`StatCell`'s `tenant`, `cell` and
    /// `cell`, TASK-364).
    pub fn cell(&self, path: &Path) -> Result<crate::CellId, crate::Error> {
        let stat = self.stat_cell(path)?;
        Ok(crate::CellId { tenant: stat.tenant, id: stat.cell })
    }

    /// The commit number of the latest commit of the cell at `path` (`StatCell`'s `committed`).
    pub fn committed(&self, path: &Path) -> Result<u64, crate::Error> {
        Ok(self.stat_cell(path)?.committed)
    }

    /// Waits until every commit of the cell at `path` is in the daemon's bucket (`SyncCell`).
    pub fn sync_cell(&self, path: &Path) -> Result<(), crate::Error> {
        let request = SyncCellRequest { path: name(path)? };
        let what = format!("waiting for the cell at {} to be uploaded", path.display());
        self.call(what, |mut c| async move { c.sync_cell(request).await }).map(drop)
    }

    /// Grants the tenant of the ID `tenant` in the bucket (TASK-364), whose cells are in `dir`,
    /// its VM socket, given to `uid` and `gid` where named, and returns its path (`Grant`).
    pub fn grant(
        &self,
        tenant: u64,
        dir: &Path,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<PathBuf, crate::Error> {
        let request = GrantRequest { tenant, dir: name(dir)?, uid, gid };
        let what = format!("granting tenant {tenant}", tenant = base32_id::Short(tenant));
        let response = self.call(what, |mut c| async move { c.grant(request).await })?;
        Ok(response.socket.into())
    }

    /// Revokes the tenant `tenant`'s grant: its VM socket goes, its VM processes connected keep
    /// their cells (`Revoke`).
    pub fn revoke(&self, tenant: u64) -> Result<(), crate::Error> {
        let request = RevokeRequest { tenant };
        let what = format!("revoking tenant {tenant}", tenant = base32_id::Short(tenant));
        self.call(what, |mut c| async move { c.revoke(request).await }).map(drop)
    }

    /// Limits the memory of the tenant `tenant`, or of every tenant without one, to `bytes`, or
    /// lifts the limit without them (`SetMemoryLimit`).
    pub fn set_memory_limit(
        &self,
        tenant: Option<u64>,
        bytes: Option<u64>,
    ) -> Result<(), crate::Error> {
        let request = SetMemoryLimitRequest { tenant, bytes };
        let what = format!("limiting the memory of {}", whom(tenant, "every tenant"));
        self.call(what, |mut c| async move { c.set_memory_limit(request).await }).map(drop)
    }

    /// cladefs: weighs the memory of the tenant `tenant` against the others' (`SetMemoryWeight`,
    /// TASK-358).
    pub fn set_memory_weight(&self, tenant: u64, weight: u64) -> Result<(), crate::Error> {
        let request = SetMemoryWeightRequest { tenant, weight };
        let what =
            format!("weighing the memory of tenant {tenant}", tenant = base32_id::Short(tenant));
        self.call(what, |mut c| async move { c.set_memory_weight(request).await }).map(drop)
    }

    /// cladefs: sets the bytes in flight the connections of the tenant `tenant`, or of every tenant
    /// without one, share to `bytes`, or to the default without them (`SetTenantInFlight`,
    /// TASK-149.35).
    pub fn set_tenant_in_flight(
        &self,
        tenant: Option<u64>,
        bytes: Option<u64>,
    ) -> Result<(), crate::Error> {
        let request = SetTenantInFlightRequest { tenant, bytes };
        let what = format!("setting the bytes in flight of {}", whom(tenant, "every tenant"));
        self.call(what, |mut c| async move { c.set_tenant_in_flight(request).await }).map(drop)
    }

    /// Evicts pages until the tenant `tenant`, or the daemon without one, holds at most `target`
    /// bytes, or none is left to evict, and returns what it holds then (`ShrinkMemory`).
    pub fn shrink_memory(&self, tenant: Option<u64>, target: u64) -> Result<u64, crate::Error> {
        let request = ShrinkMemoryRequest { tenant, target_bytes: target };
        let what = format!("shrinking the memory of {}", whom(tenant, "the daemon"));
        let response = self.call(what, |mut c| async move { c.shrink_memory(request).await })?;
        Ok(response.bytes)
    }

    /// cladefs: limits the disk of the tenant `tenant`, or of every tenant without one, to `bytes`,
    /// or lifts the limit without them (`SetDiskLimit`, TASK-94).
    pub fn set_disk_limit(
        &self,
        tenant: Option<u64>,
        bytes: Option<u64>,
    ) -> Result<(), crate::Error> {
        let request = SetDiskLimitRequest { tenant, bytes };
        let what = format!("limiting the disk of {}", whom(tenant, "every tenant"));
        self.call(what, |mut c| async move { c.set_disk_limit(request).await }).map(drop)
    }

    /// cladefs: has the hosts sharing the daemon's bucket be those at `addresses`, each `HOST:PORT`
    /// (`SetPeers`, TASK-406.2).
    pub fn set_peers(&self, addresses: &[&str]) -> Result<(), crate::Error> {
        let addresses = addresses.iter().map(|address| (*address).to_owned()).collect();
        let request = SetPeersRequest { addresses };
        let what = "setting the daemon's peers".to_owned();
        self.call(what, |mut c| async move { c.set_peers(request).await }).map(drop)
    }

    /// cladefs: shreds the cell `cell` in the daemon's bucket, its wrapped KEKs taken out of its
    /// object, and has the daemon forget its keys (`ShredCell`, TASK-411.7): how many generations
    /// of its KEK it took out, 0 if it was shredded already, and what the daemon forgot.
    pub fn shred_cell(&self, cell: crate::CellId) -> Result<(u32, Forgotten), crate::Error> {
        let request = ShredCellRequest { tenant: cell.tenant, cell: cell.id };
        let what = format!("shredding cell {cell}");
        let response = self.call(what, |mut c| async move { c.shred_cell(request).await })?;
        Ok((response.keks_removed, response.forgotten.unwrap_or_default()))
    }

    /// cladefs: has the daemon forget what it holds of the keys of the tenant `tenant`, whose KEK
    /// is revoked or destroyed in the KMS, as `why` says (`ForgetTenantKeys`, TASK-411.7): what it
    /// forgot.
    pub fn forget_tenant_keys(&self, tenant: u64, why: &str) -> Result<Forgotten, crate::Error> {
        let request = ForgetTenantKeysRequest { tenant, why: why.to_owned() };
        let what = format!("having the daemon forget tenant {tenant}'s keys");
        let response =
            self.call(what, |mut c| async move { c.forget_tenant_keys(request).await })?;
        Ok(response.forgotten.unwrap_or_default())
    }

    /// cladefs: shreds the tenant `tenant` under the daemon's bucket KMS, its KEK object replaced
    /// by a tombstone, and has the daemon forget its keys (`ShredTenant`, TASK-411.27): whether
    /// this call shredded it (not already), and what the daemon forgot.
    pub fn shred_tenant(&self, tenant: u64) -> Result<(bool, Forgotten), crate::Error> {
        let request = ShredTenantRequest { tenant };
        let what = format!("shredding tenant {tenant}");
        let response = self.call(what, |mut c| async move { c.shred_tenant(request).await })?;
        Ok((response.shredded_now, response.forgotten.unwrap_or_default()))
    }

    /// cladefs: what the disk of the tenant `tenant`, or of every tenant without one, takes,
    /// counted now, in bytes, and its limit, if it has one (`GetTenantDisk`, TASK-94).
    pub fn tenant_disk(&self, tenant: Option<u64>) -> Result<(u64, Option<u64>), crate::Error> {
        let response = self.get_tenant_disk(tenant)?;
        Ok((response.bytes, response.limit_bytes))
    }

    /// cladefs: what the disk of the tenant `tenant`, or of every tenant without one, takes,
    /// counted now, its limit and what is reserved of it, as `GetTenantDisk` answers (board 8585).
    pub fn get_tenant_disk(
        &self,
        tenant: Option<u64>,
    ) -> Result<GetTenantDiskResponse, crate::Error> {
        let request = GetTenantDiskRequest { tenant };
        let what = format!("counting the disk of {}", whom(tenant, "every tenant"));
        self.call(what, |mut c| async move { c.get_tenant_disk(request).await })
    }

    /// cladefs: weighs the disk of the tenant `tenant` against the others' (`SetDiskWeight`, board
    /// 8585).
    pub fn set_disk_weight(&self, tenant: u64, weight: u64) -> Result<(), crate::Error> {
        let request = SetDiskWeightRequest { tenant, weight };
        let what =
            format!("weighing the disk of tenant {tenant}", tenant = base32_id::Short(tenant));
        self.call(what, |mut c| async move { c.set_disk_weight(request).await }).map(drop)
    }

    /// cladefs: what the daemon, its VM processes and their guests counted, as of one moment, each
    /// with its build (`GetStats`, TASK-111.4): the VM processes' answers waited for `wait` at
    /// most, the daemon's default if none.
    pub fn stats(
        &self,
        wait: Option<std::time::Duration>,
    ) -> Result<GetStatsResponse, crate::Error> {
        let wait_ms = wait.map_or(0, |wait| u32::try_from(wait.as_millis()).unwrap_or(u32::MAX));
        self.get_stats(GetStatsRequest { wait_ms, ..GetStatsRequest::default() })
    }

    /// cladefs: what the daemon, its VM processes and their guests counted, as `request` asks
    /// (`GetStats`): what to wait for, and what to leave out (board 8581).
    pub fn get_stats(&self, request: GetStatsRequest) -> Result<GetStatsResponse, crate::Error> {
        self.call("asking the extent daemon for its stats".to_owned(), |mut c| async move {
            c.get_stats(request).await
        })
    }

    /// cladefs: the daemon's counters, as `GetCounters` answers, its bucket's with them (board
    /// 8585).
    pub fn get_counters(&self) -> Result<GetCountersResponse, crate::Error> {
        self.call("asking the extent daemon for its counters".to_owned(), |mut c| async move {
            c.get_counters(GetCountersRequest {}).await
        })
    }

    /// The daemon's counters (`GetCounters`).
    pub fn counters(&self) -> Result<crate::stats::Stats, crate::Error> {
        let response = self.get_counters()?;
        Ok(crate::stats::Stats {
            maps: response.maps,
            map_ns: response.map_ns,
            faults: response.faults,
            fault_ns: response.fault_ns,
            page_ins: response.page_ins,
            page_in_bytes: response.page_in_bytes,
            fetched_bytes: response.fetched_bytes,
            reads: response.reads,
            read_bytes: response.read_bytes,
            read_stored_bytes: response.read_stored_bytes,
            prefetches: response.prefetches,
            unpacks: response.unpacks,
            // The VM processes' changes to their windows, which the API doesn't report
            // (`Stats::stage_ops`).
            stage_ops: 0,
            stage_ns: 0,
            commits: response.commits,
            commit_ns: response.commit_ns,
            commit_store_ns: response.commit_store_ns,
            commit_wait_ns: response.commit_wait_ns,
            segment_closes: response.segment_closes,
            segment_close_ns: response.segment_close_ns,
            // The maps the daemon sent the VM processes' windows, which the API doesn't report.
            stage_maps: 0,
            stage_map_bytes: 0,
            stage_overwrites: 0,
            stage_leave_outs: 0,
            cache_trims: 0,
            cache_trim_ns: 0,
            fault_stores: 0,
            fault_refusals: 0,
            fault_waits: 0,
            stage_segments: 0,
            stage_private_runs: 0,
            stage_private_pages: 0,
            stage_private_ns: 0,
            vm_private_requests: 0,
            vm_private_request_ns: 0,
            predicted_read: 0,
            predicted_unread: 0,
            read_unpredicted: 0,
            prefetch_page_ins: 0,
        })
    }

    /// Makes the call `call` makes with a client, and returns its response; or, failed, an error
    /// saying it was `what` ([`failed`]).
    fn call<T, F>(
        &self,
        what: String,
        call: impl FnOnce(CladefsdClient<tonic::transport::Channel>) -> F,
    ) -> Result<T, crate::Error>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        // cladefs: outside the simulation: the blocking client's call.
        match self.runtime.block_on(within(self.timeout, call(self.client.clone()))) {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(failed(what, status)),
            Err(timeout) => Err(timed_out(what, timeout)),
        }
    }
}

/// cladefs: what `future` gives, within `timeout` if given; or, once that is over, the timeout
/// (board 8585).
async fn within<T>(
    timeout: Option<std::time::Duration>,
    future: impl Future<Output = T>,
) -> Result<T, std::time::Duration> {
    match timeout {
        // The timeout's error says no more than that it is over.
        Some(timeout) => tokio::time::timeout(timeout, future).await.map_err(|_| timeout),
        None => Ok(future.await),
    }
}

/// cladefs: the error of a call, `what`, that took longer than `timeout` (board 8585):
/// `ETIMEDOUT`, saying that the daemon may still complete it, as it runs its calls to their end.
fn timed_out(what: String, timeout: std::time::Duration) -> crate::Error {
    let said = format!(
        "no answer within {timeout:?}: the extent daemon may still complete the call, as it \
         completes every call whether or not its caller waits"
    );
    crate::Error::new(anyhow::Error::new(Errno::ETIMEDOUT).context(said).context(what))
}

/// The error of a call that failed with `status`, saying it was `what`: with the errno it failed
/// with, if any ([`crate::Error::errno`] finds it), and what the daemon said of it (board 8583),
/// unless it said no more than the errno's name ([`status`]); without an errno, the status whole.
pub fn failed(what: String, status: Status) -> crate::Error {
    crate::Error::new(match errno(&status) {
        Some(errno) => match status.message() {
            said if said.is_empty() || said == format!("{errno:?}") => {
                anyhow::Error::new(errno).context(what)
            }
            said => anyhow::Error::new(errno).context(said.to_owned()).context(what),
        },
        None => anyhow::Error::new(status).context(what),
    })
}

/// The tenant `tenant`, as an error says it, in base 32 as cells say it; or, without one,
/// `none`.
fn whom(tenant: Option<u64>, none: &str) -> String {
    tenant.map_or_else(
        || none.to_owned(),
        |tenant| format!("tenant {tenant}", tenant = base32_id::Short(tenant)),
    )
}

/// `path`, as a request names it: `EINVAL` unless it is UTF-8.
fn name(path: &Path) -> Result<String, crate::Error> {
    path.to_str().map(str::to_owned).ok_or_else(|| crate::Error::from(Errno::EINVAL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::oneshot;

    /// A service of one call, which waits for `go`, then says whether its caller had given up.
    struct OneCall(Option<(oneshot::Receiver<()>, oneshot::Sender<&'static str>)>);

    impl tower::Service<()> for OneCall {
        type Response = http::Response<()>;
        type Error = std::convert::Infallible;
        type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, (): ()) -> Self::Future {
            let (go, ended) = self.0.take().expect("one call");
            Box::pin(async move {
                go.await.unwrap();
                let heard = match futures::FutureExt::now_or_never(crate::caller::gone()) {
                    Some(()) => "its caller gone",
                    None => "its caller waiting",
                };
                ended.send(heard).unwrap();
                Ok(http::Response::new(()))
            })
        }
    }

    /// cladefs: board 5662: while the host's memory is full, a cell is refused, RESOURCE_EXHAUSTED
    /// with ENOMEM, which the client reads back; once it isn't, made.
    #[test]
    fn cells_are_refused_while_the_hosts_memory_is_full() {
        super::super::governor::set_full(true);
        let refused = room_for_a_cell().unwrap_err();
        assert_eq!(refused.code(), Code::ResourceExhausted);
        assert_eq!(errno(&refused), Some(Errno::ENOMEM));
        assert_eq!(refused.message(), "the host's memory is full: the daemon makes no cell more");
        super::super::governor::set_full(false);
        assert!(room_for_a_cell().is_ok());
    }

    /// A call of the API whose caller gives up, dropping what awaits its answer, runs to its end,
    /// counted for the drain until then (board 5599): stopped at the await it was in, it left what
    /// it began half done (board 5382, board 5384).
    #[tokio::test]
    async fn calls_given_up_on_run_to_their_end() {
        let (go, going) = oneshot::channel();
        let (ended, end) = oneshot::channel();
        let mut service = Drained(OneCall(Some((going, ended))));
        let answer = tower::Service::call(&mut service, ());
        assert_eq!(CALLS.count(), 1);
        drop(answer);
        assert_eq!(CALLS.count(), 1, "a call given up on stopped");
        go.send(()).unwrap();
        assert_eq!(end.await, Ok("its caller gone"));
        CALLS.done().await;

        // One whose caller waits is answered, its caller never heard gone.
        let (go, going) = oneshot::channel();
        let (ended, end) = oneshot::channel();
        let mut service = Drained(OneCall(Some((going, ended))));
        let answer = tower::Service::call(&mut service, ());
        go.send(()).unwrap();
        assert!(answer.await.is_ok());
        assert_eq!(end.await, Ok("its caller waiting"));
        assert_eq!(CALLS.count(), 0);
    }
}
