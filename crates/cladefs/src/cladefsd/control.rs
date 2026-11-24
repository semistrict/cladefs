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

use super::Daemon;
use super::cells;
use super::disk;
use super::grants::Grants;
use super::grpc::Served;
use super::memory;
use super::storage;
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

/// Serves the control API for the process's default daemon ([`serve_control_on`]).
#[cfg(not(madsim))]
pub fn serve_control(
    listener: std::os::unix::net::UnixListener,
    tenants: impl Into<PathBuf>,
) -> std::io::Result<()> {
    serve_control_on(Daemon::process(), listener, tenants)
}

/// Serves the control API for `daemon` on `listener` synchronously, blocking on `daemon`'s
/// consolidated runtime.
#[cfg(not(madsim))]
pub(super) fn serve_control_on(
    daemon: &Arc<Daemon>,
    listener: std::os::unix::net::UnixListener,
    tenants: impl Into<PathBuf>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    // cladefs: outside the simulation: blocking entry point for synchronous callers and tests.
    daemon.block_on(serve_control_async(Arc::clone(daemon), listener, tenants.into()))
}

/// Serves the control API for `daemon` to the callers that connect to `listener`, until accepting
/// fails; the sockets of the tenants it grants go in the directory `tenants`. Runs on `daemon`'s
/// consolidated runtime (no separate Tokio runtime; Wave 4C.5, board 10774).
/// cladefs: and the Mount service of the daemon's FUSE mount of cells, if it has one
/// ([`super::mount`], TASK-475.2): its calls, counted and refused as the daemon drains as the
/// API's are; without a mount, they fail `UNIMPLEMENTED`.
#[cfg(not(madsim))]
pub(super) async fn serve_control_async(
    daemon: Arc<Daemon>,
    listener: std::os::unix::net::UnixListener,
    tenants: PathBuf,
) -> std::io::Result<()> {
    let control = Control {
        daemon: Arc::clone(&daemon),
        grants: Arc::new(Grants::new(Arc::clone(&daemon), tenants)),
    };
    let (reporter, health) = tonic_health::server::health_reporter();
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(
        tokio::net::UnixListener::from_std(listener)?,
    );
    // Its first status reported before it serves: the reporter starts with the server's, "",
    // SERVING, which a check served before a task of its own reported otherwise saw.
    let mut serving = daemon.serving.subscribe();
    let mut now = *serving.borrow_and_update();
    report_health(&daemon, &reporter, now).await;
    let reporting = Arc::clone(&daemon);
    tokio::spawn(async move {
        // cladefs: its panic aborts the daemon, as a service thread's does (board 8501): the
        // health service would say what it last said for ever, the drain waiting for it.
        let _supervised = local_cell::shared::Awaited::new(HEALTH_REPORTER);
        loop {
            // cladefs: a test's reporter gone once it reported SERVING, as no panic but its
            // runtime's going leaves it, which a drain bounds (board 8503). Guarding outside
            // the macro reads `now` even when `failpoints` is off (boards 10218, 10489).
            if now {
                fail::fail_point!("cladefsd::health_reporter_gone", |_| ());
            }
            if serving.changed().await.is_err() {
                return;
            }
            now = *serving.borrow_and_update();
            report_health(&reporting, &reporter, now).await;
        }
    });
    daemon.control_serving.store(true, std::sync::atomic::Ordering::Release);
    let mount =
        super::mount::service_on(&daemon).map(|mount| Served::new(Drained::new(&daemon, mount)));
    tonic::transport::Server::builder()
        .add_service(health)
        // cladefs: no answer encoded past MAX_MESSAGE_BYTES (board 8581): what grows refuses
        // first, saying what to leave out (`bounded`); any other, OUT_OF_RANGE, tonic's. Its
        // calls counted and traced, refusals as the daemon drains among them (board 8565,
        // board 8569).
        .add_service(Served::new(Drained::new(
            &daemon,
            CladefsdServer::new(control).max_encoding_message_size(MAX_MESSAGE_BYTES),
        )))
        // cladefs: its FUSE mount's, if it mounts cells (TASK-475.2).
        .add_optional_service(mount)
        .serve_with_incoming(incoming)
        .await
        .map_err(std::io::Error::other)
}

/// cladefs: serves `daemon`'s control API, as [`serve_control`] does, on the simulated network at
/// `address`, under madsim, which has no Unix sockets: the socket is a deployment detail (doc-24's
/// boundary, TASK-685.13). The health service, tonic's, which madsim's tonic doesn't serve, isn't;
/// its status is reported all the same, as the daemon's drain waits for it. No FUSE mount's
/// service: FUSE is outside the simulation.
#[cfg(madsim)]
pub(super) async fn serve_control_on(
    daemon: Arc<Daemon>,
    address: std::net::SocketAddr,
    tenants: impl Into<PathBuf>,
) -> std::io::Result<()> {
    let control = Control {
        daemon: Arc::clone(&daemon),
        grants: Arc::new(Grants::new(Arc::clone(&daemon), tenants.into())),
    };
    let mut serving = daemon.serving.subscribe();
    daemon.reported.send_replace(*serving.borrow_and_update());
    let reporting = Arc::clone(&daemon);
    tokio::spawn(async move {
        while serving.changed().await.is_ok() {
            reporting.reported.send_replace(*serving.borrow_and_update());
        }
    });
    daemon.control_serving.store(true, std::sync::atomic::Ordering::Release);
    tonic::transport::Server::builder()
        .add_service(Served::new(Drained::new(
            &daemon,
            CladefsdServer::new(control).max_encoding_message_size(MAX_MESSAGE_BYTES),
        )))
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
    host: &crate::Host,
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
        incarnation: crate::ephemeral_host::host_on(host).map_or(0, |host| host.incarnation()),
    }
}

/// cladefs: the API's service, `S`, its calls counted in `daemon.calls_work` until they end, and
/// refused (`UNAVAILABLE`) as `daemon` drains (TASK-141.4): each runs to its end in a task of its
/// own, whether or not its caller waits for its answer, as the API promises (board 5599,
/// [`crate::caller`]), so that what a call does is done once it ends, as none streams. The health
/// service's calls are not, as its watches never end.
#[derive(Clone)]
struct Drained<S> {
    daemon: Arc<Daemon>,
    inner: S,
}

impl<S> Drained<S> {
    fn new(daemon: &Arc<Daemon>, inner: S) -> Self {
        Self { daemon: Arc::clone(daemon), inner }
    }
}

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
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Call) -> Self::Future {
        let Some(call) = self.daemon.calls_work.begin(&self.daemon) else {
            let refused = Status::unavailable("the extent daemon drains, as it is about to exit");
            return Box::pin(std::future::ready(Refusal::refused(refused)));
        };
        // cladefs: in a task of its own, which runs to its end, counted until then, whether or not
        // its caller waits for the answer (board 5599): a caller's deadline, or its stream reset,
        // drops what awaits it, which stops at whatever await it was in otherwise. Its span, the
        // one current as it is called (`grpc::Served`), current as the task runs, the parent of
        // what the call does (board 8569).
        let answered = bucket_cell::trace::Parent::current().around(self.inner.call(request));
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

/// Reports, through the standard gRPC health service (grpc.health.v1.Health), whether `daemon`
/// serves (TASK-48), `serving`: SERVING while it accepts VM processes' connections, NOT_SERVING
/// as it starts and once it stops; the whole server's status, named "", and the control API's,
/// alike. Its callers have it reported once it returns (`Daemon::stop_serving` waits for that).
#[cfg(not(madsim))]
async fn report_health(
    daemon: &Daemon,
    reporter: &tonic_health::server::HealthReporter,
    serving: bool,
) {
    use tonic_health::ServingStatus;
    let status = if serving { ServingStatus::Serving } else { ServingStatus::NotServing };
    reporter.set_service_status("", status).await;
    reporter.set_service_status(<CladefsdServer<Control> as NamedService>::NAME, status).await;
    daemon.reported.send_replace(serving);
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

/// cladefs: refuses to make a cell while the host's memory is full (`super::governor::full_on`,
/// board 5662): RESOURCE_EXHAUSTED (ENOMEM), so that the VM manager places its VM on another host,
/// rather than this host's VMs lose to it the pages their windows use. A cell made already opens.
fn room_for_a_cell(daemon: &Daemon) -> Result<(), Status> {
    match super::governor::full_on(daemon) {
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
    #[allow(deprecated)]
    let (fsync, local) = match FsyncLevel::try_from(fsync) {
        Ok(FsyncLevel::Ordered) => (Fsync::Ordered, false),
        Ok(FsyncLevel::Local) => (Fsync::Ordered, true),
        Ok(FsyncLevel::Replicated) => (Fsync::Replicated, false),
        Err(_) => return Err(status(Errno::EINVAL)),
    };
    let Some(replicas) = replicas else {
        if !local && fsync == Fsync::Ordered {
            return Ok(None);
        }
        return Err(status(Errno::EINVAL));
    };
    let f = u8::try_from(replicas).map_err(|_| status(Errno::EINVAL))?;
    let wish = Wish::new(f, fsync).ok_or_else(|| status(Errno::EINVAL))?;
    if local {
        log::warn!(
            "cladefsd: FSYNC_LEVEL_LOCAL is deprecated and removed; using FSYNC_LEVEL_ORDERED"
        );
    }
    Ok(Some(wish))
}

/// What StatCell's FsyncLevel is of `fsync`.
fn fsync_level(fsync: bucket_cell::layout::Fsync) -> FsyncLevel {
    match fsync {
        bucket_cell::layout::Fsync::Ordered => FsyncLevel::Ordered,
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

/// The API's calls, served from `daemon`'s cells, and its grants.
struct Control {
    daemon: Arc<Daemon>,
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
        room_for_a_cell(&self.daemon)?;
        let replication = wish(replicas, fsync)?;
        // cladefs: a replicated cell's members are the daemon's peers, in their replica
        // directories (doc-21): without them, refused before anything is made.
        if replication.is_some_and(|wish| wish.f >= 1)
            && crate::bucket::members_on(&self.daemon.host).is_none()
        {
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
        let made = cells::create(
            &self.daemon,
            cell_dir,
            capacity,
            cell,
            (token, kind, replication, force),
        )
        .await;
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
        let made = cells::import(&self.daemon, cell_dir, capacity, image, reference, tenant).await;
        reply(made.map(|made| ImportImageResponse { tenant: made.tenant, cell: made.id }))
    }

    async fn import_memory(
        &self,
        request: Request<ImportMemoryRequest>,
    ) -> Result<Response<ImportMemoryResponse>, Status> {
        let ImportMemoryRequest { path: cell_dir, file, tenant, make_version } =
            request.into_inner();
        let (cell_dir, file) = (path(cell_dir)?, path(file)?);
        room_for_a_cell(&self.daemon)?;
        let tenant = self.new_cell_at(&cell_dir, tenant)?;
        let made = cells::import_memory(&self.daemon, cell_dir, file, tenant, make_version).await;
        reply(made.map(|(made, version)| ImportMemoryResponse {
            tenant: made.tenant,
            cell: made.id,
            version: version.unwrap_or(0),
        }))
    }

    async fn fork_cell(
        &self,
        request: Request<ForkCellRequest>,
    ) -> Result<Response<ForkCellResponse>, Status> {
        let ForkCellRequest { from, to, version, tenant: of, cell, ephemeral } =
            request.into_inner();
        let to = path(to)?;
        room_for_a_cell(&self.daemon)?;
        let tenant = self.grants.tenant_at(&to);
        let of = crate::CellId { tenant: of, id: cell };
        let forked = match version {
            // cladefs: ephemeral cells are forked from versions alone (doc-25, TASK-698.3).
            0 if ephemeral => Err(Errno::EINVAL),
            0 => cells::fork(&self.daemon, path(from)?, to, tenant).await,
            _ if !from.is_empty() || cell == 0 => Err(Errno::EINVAL),
            _ if ephemeral => {
                cells::fork_ephemeral_of_version(&self.daemon, of, version, to, tenant).await
            }
            _ => cells::fork_of_version(&self.daemon, of, version, to, tenant).await,
        };
        reply(forked.map(|forked| ForkCellResponse { tenant: forked.tenant, cell: forked.id }))
    }

    async fn migrate_cell(
        &self,
        request: Request<MigrateCellRequest>,
    ) -> Result<Response<MigrateCellResponse>, Status> {
        let MigrateCellRequest { path: cell_dir, peer, to } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        let migrating = super::migration::migrate_on(&self.daemon, cell_dir, peer, to).await;
        reply(migrating.map(|migration| MigrateCellResponse { migration }))
    }

    async fn hand_over_cell(
        &self,
        request: Request<HandOverCellRequest>,
    ) -> Result<Response<HandOverCellResponse>, Status> {
        let HandOverCellRequest { path: cell_dir, token } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        reply(
            super::migration::hand_over_on(&self.daemon, cell_dir, token)
                .await
                .map(|()| HandOverCellResponse {}),
        )
    }

    async fn abort_migration(
        &self,
        request: Request<AbortMigrationRequest>,
    ) -> Result<Response<AbortMigrationResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(
            super::migration::abort_on(&self.daemon, cell_dir)
                .await
                .map(|()| AbortMigrationResponse {}),
        )
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
            let own = crate::ephemeral_host::host_on(&self.daemon.host)
                .is_ok_and(|host| host.incarnation() == incarnation);
            if incarnation % 2 == 0 || own {
                log::warn!(
                    "cladefsd: decommissioning host {said}: refused, no host's to decommission"
                );
                return Err(Errno::EINVAL);
            }
            let bucket = storage::daemon_bucket_on(&self.daemon)?;
            let what = format!("decommissioning host {said}");
            let buckets =
                bucket.ephemeral_buckets().await.map_err(|err| bucket_errno(&what, err))?;
            let decommissioning =
                bucket_cell::lifecycle::decommission(&*bucket, &buckets, incarnation, most);
            let done = decommissioning.await.map_err(|err| bucket_errno(&what, err))?;
            let bucket_cell::lifecycle::Decommissioned { retired, left } = done;
            let auth = bucket_cell::events::authority(&bucket)
                .await
                .map_err(|err| bucket_errno(&what, err))?;
            bucket_cell::events::publish(
                &*auth,
                bucket_cell::events::Publish::HostDecommissioned(incarnation),
            )
            .await
            .map_err(|err| bucket_errno(&what, err))?;
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
            let bucket = storage::daemon_bucket_on(&self.daemon)?;
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
        reply(cells::delete(&self.daemon, cell_dir).await.map(|()| DeleteCellResponse {}))
    }

    async fn drop_cell(
        &self,
        request: Request<DropCellRequest>,
    ) -> Result<Response<DropCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(cells::drop_here(&self.daemon, cell_dir).await.map(|()| DropCellResponse {}))
    }

    async fn resize_cell(
        &self,
        request: Request<ResizeCellRequest>,
    ) -> Result<Response<ResizeCellResponse>, Status> {
        let ResizeCellRequest { path: cell_dir, capacity } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        reply(cells::resize(&self.daemon, cell_dir, capacity).await.map(|()| ResizeCellResponse {}))
    }

    async fn place_cell(
        &self,
        request: Request<PlaceCellRequest>,
    ) -> Result<Response<PlaceCellResponse>, Status> {
        let PlaceCellRequest { path: cell_dir, token, force } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        reply(
            cells::place(&self.daemon, cell_dir, token, force).await.map(|()| PlaceCellResponse {}),
        )
    }

    async fn make_public(
        &self,
        request: Request<MakePublicRequest>,
    ) -> Result<Response<MakePublicResponse>, Status> {
        let MakePublicRequest { path: cell_dir, attach_only } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        reply(
            cells::make_public(&self.daemon, cell_dir, attach_only)
                .await
                .map(|()| MakePublicResponse {}),
        )
    }

    async fn make_version(
        &self,
        request: Request<MakeVersionRequest>,
    ) -> Result<Response<MakeVersionResponse>, Status> {
        let MakeVersionRequest { path: cell_dir, keep_last } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        let keep_last = Some(keep_last).filter(|&keep| keep > 0);
        let made = cells::make_version(&self.daemon, cell_dir, keep_last).await;
        reply(made.map(|version| MakeVersionResponse { version }))
    }

    async fn list_versions(
        &self,
        request: Request<ListVersionsRequest>,
    ) -> Result<Response<ListVersionsResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        let versions = cells::versions(&self.daemon, cell_dir).await.map_err(status)?;
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
        let dropped =
            cells::drop_versions(&self.daemon, cell_dir, versions.into_iter().collect()).await;
        reply(dropped.map(|()| DropVersionsResponse {}))
    }

    async fn share_version(
        &self,
        request: Request<ShareVersionRequest>,
    ) -> Result<Response<ShareVersionResponse>, Status> {
        let ShareVersionRequest { path: cell_dir, version, shared } = request.into_inner();
        let cell_dir = path(cell_dir)?;
        let shared = cells::share_version(&self.daemon, cell_dir, version, shared).await;
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
        reply(
            cells::attach(&self.daemon, cell_dir, base, allowed)
                .await
                .map(|()| AttachBaseResponse {}),
        )
    }

    async fn sync_cell(
        &self,
        request: Request<SyncCellRequest>,
    ) -> Result<Response<SyncCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(cells::sync(&self.daemon, cell_dir).await.map(|()| SyncCellResponse {}))
    }

    async fn stat_cell(
        &self,
        request: Request<StatCellRequest>,
    ) -> Result<Response<StatCellResponse>, Status> {
        let cell_dir = path(request.into_inner().path)?;
        reply(cells::stat(&self.daemon, cell_dir).await.map(stat_response))
    }

    async fn list_cells(
        &self,
        _request: Request<ListCellsRequest>,
    ) -> Result<Response<ListCellsResponse>, Status> {
        let cells = cells::list(&self.daemon).map_err(status)?.into_iter().map(|(path, state)| {
            ListedCell {
                path: path.to_string_lossy().into_owned(),
                stat: Some(stat_response(state)),
            }
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
        let bucket = storage::daemon_bucket_on(&self.daemon).map_err(status)?.stats();
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
        let stats = super::stats::take_on(&self.daemon, wait, !leave_out_vms).await;
        let answer =
            stats_response(&self.daemon.host, stats, (leave_out_histograms, leave_out_guests));
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
        reply(memory::set_limit_on(&self.daemon, tenant, bytes).map(|()| SetMemoryLimitResponse {}))
    }

    async fn set_memory_weight(
        &self,
        request: Request<SetMemoryWeightRequest>,
    ) -> Result<Response<SetMemoryWeightResponse>, Status> {
        let SetMemoryWeightRequest { tenant, weight } = request.into_inner();
        reply(
            memory::set_weight_on(&self.daemon, tenant, weight)
                .map(|()| SetMemoryWeightResponse {}),
        )
    }

    async fn set_tenant_in_flight(
        &self,
        request: Request<SetTenantInFlightRequest>,
    ) -> Result<Response<SetTenantInFlightResponse>, Status> {
        let SetTenantInFlightRequest { tenant, bytes } = request.into_inner();
        reply(
            memory::set_in_flight_on(&self.daemon, tenant, bytes)
                .map(|()| SetTenantInFlightResponse {}),
        )
    }

    async fn shrink_memory(
        &self,
        request: Request<ShrinkMemoryRequest>,
    ) -> Result<Response<ShrinkMemoryResponse>, Status> {
        let ShrinkMemoryRequest { tenant, target_bytes } = request.into_inner();
        // cladefs: on the call's task: its evictions' passes are synchronous, and it waits for the
        // pages they evict holding no thread (board 2946).
        let shrunk = memory::shrink_on(&self.daemon, tenant, target_bytes).await;
        reply(shrunk.map(|bytes| ShrinkMemoryResponse { bytes }))
    }

    async fn set_disk_limit(
        &self,
        request: Request<SetDiskLimitRequest>,
    ) -> Result<Response<SetDiskLimitResponse>, Status> {
        let SetDiskLimitRequest { tenant, bytes } = request.into_inner();
        reply(disk::set_limit_on(&self.daemon, tenant, bytes).map(|()| SetDiskLimitResponse {}))
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
        reply(disk::count_on(&self.daemon, tenant).map(response))
    }

    async fn set_disk_weight(
        &self,
        request: Request<SetDiskWeightRequest>,
    ) -> Result<Response<SetDiskWeightResponse>, Status> {
        let SetDiskWeightRequest { tenant, weight } = request.into_inner();
        reply(disk::set_weight_on(&self.daemon, tenant, weight).map(|()| SetDiskWeightResponse {}))
    }

    async fn set_peers(
        &self,
        request: Request<SetPeersRequest>,
    ) -> Result<Response<SetPeersResponse>, Status> {
        let SetPeersRequest { addresses } = request.into_inner();
        reply(super::peers::set_on(&self.daemon, addresses).map(|()| SetPeersResponse {}))
    }

    async fn shred_cell(
        &self,
        request: Request<ShredCellRequest>,
    ) -> Result<Response<ShredCellResponse>, Status> {
        let ShredCellRequest { tenant, cell } = request.into_inner();
        let cell = crate::CellId { tenant, id: cell };
        let bucket = storage::daemon_bucket_on(&self.daemon).map_err(status)?;
        let shredded = crate::bucket::shred_cell_and_publish_on(&self.daemon.host, &bucket, cell)
            .await
            .map_err(|err| {
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
        let host = Arc::clone(&self.daemon.host);
        let forgotten =
            cells::blocking(move || Ok(crate::bucket::forget_on(&host, scope, &why))).await;
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
        use crate::bucket::ShredError;
        let ShredTenantRequest { tenant } = request.into_inner();
        let (destroyed, forgotten) =
            crate::bucket::shred_tenant_on(&self.daemon.host, tenant).await.map_err(|err| {
                let errno = match &err {
                    ShredError::InvalidTenant(_) => Errno::EINVAL,
                    ShredError::Unsupported(_) => Errno::ENOTSUP,
                    ShredError::Failed(_) => Errno::EIO,
                };
                log::error!("cladefsd: shredding tenant {tenant}: {err}");
                saying(errno, err.to_string())
            })?;
        let shredded_now = destroyed == bucket_cell::bucket_kms::Destroyed::Now;
        Ok(Response::new(ShredTenantResponse {
            shredded_now,
            forgotten: Some(forgotten_of(forgotten)),
        }))
    }

    async fn reload_kms(
        &self,
        request: Request<ReloadKmsRequest>,
    ) -> Result<Response<ReloadKmsResponse>, Status> {
        use bucket_cell::bucket_kms::Unreloaded;
        let ReloadKmsRequest {} = request.into_inner();
        let reloaded =
            crate::bucket::reload_kms_on(&self.daemon.host).await.map_err(|refused| {
                let errno = match &refused {
                    Unreloaded::Malformed(_) => Errno::EINVAL,
                    Unreloaded::InUse(_) => Errno::EBUSY,
                    Unreloaded::Unreached(_) => Errno::EIO,
                    Unreloaded::Unsupported(_) => Errno::ENOTSUP,
                };
                log::error!("cladefsd: reloading the KMS: {refused}");
                saying(errno, format!("reloading the KMS: {refused}"))
            })?;
        let ids = |ids: Vec<extent_keys::kms::master::MasterKeyId>| {
            ids.iter().map(ToString::to_string).collect()
        };
        let master_keys = (reloaded.master_keys.into_iter())
            .map(|changed| MasterKeysReloaded {
                bucket: changed.bucket.into(),
                added: ids(changed.added),
                removed: ids(changed.removed),
            })
            .collect();
        Ok(Response::new(ReloadKmsResponse { renamed: reloaded.renamed, master_keys }))
    }
}

/// cladefs: what a host forgot of a tenant's or a cell's keys, as the API has it (TASK-411.7).
fn forgotten_of(forgotten: crate::bucket::Forgotten) -> Forgotten {
    let count = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
    let crate::bucket::Forgotten { stores, rings, kept, extents, bytes } = forgotten;
    Forgotten { stores: count(stores), rings: count(rings), kept: count(kept), extents, bytes }
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
        let daemon = Daemon::new();
        super::super::governor::set_full_on(&daemon, true);
        let refused = room_for_a_cell(&daemon).unwrap_err();
        assert_eq!(refused.code(), Code::ResourceExhausted);
        assert_eq!(errno(&refused), Some(Errno::ENOMEM));
        assert_eq!(refused.message(), "the host's memory is full: the daemon makes no cell more");
        super::super::governor::set_full_on(&daemon, false);
        assert!(room_for_a_cell(&daemon).is_ok());
    }

    /// A call of the API whose caller gives up, dropping what awaits its answer, runs to its end,
    /// counted for the drain until then (board 5599): stopped at the await it was in, it left what
    /// it began half done (board 5382, board 5384).
    #[tokio::test]
    async fn calls_given_up_on_run_to_their_end() {
        let daemon = Daemon::new();
        let (go, going) = oneshot::channel();
        let (ended, end) = oneshot::channel();
        let mut service = Drained::new(&daemon, OneCall(Some((going, ended))));
        let answer = tower::Service::call(&mut service, ());
        assert_eq!(daemon.calls_work.count(), 1);
        drop(answer);
        assert_eq!(daemon.calls_work.count(), 1, "a call given up on stopped");
        go.send(()).unwrap();
        assert_eq!(end.await, Ok("its caller gone"));
        daemon.calls_work.done().await;

        // One whose caller waits is answered, its caller never heard gone.
        let (go, going) = oneshot::channel();
        let (ended, end) = oneshot::channel();
        let mut service = Drained::new(&daemon, OneCall(Some((going, ended))));
        let answer = tower::Service::call(&mut service, ());
        go.send(()).unwrap();
        assert!(answer.await.is_ok());
        assert_eq!(end.await, Ok("its caller waiting"));
        assert_eq!(daemon.calls_work.count(), 0);
    }
}
