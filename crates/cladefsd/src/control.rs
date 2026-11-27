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

use crate::Daemon;
use crate::cells;
use crate::disk;
use crate::grants::Grants;
use crate::grpc::Served;
use crate::memory;
use crate::storage;
use bucket_cell::cell::Kind;
use cladefs_core::Errno;
use proto::cladefsd_client::CladefsdClient;
use proto::cladefsd_server::{Cladefsd, CladefsdServer};
use proto::*;
use std::path::PathBuf;
// The real channels' sockets' (`unix_channel`, `channel`, `connect`), which madsim's has none of.
#[cfg(not(madsim))]
use std::path::Path;
use std::sync::Arc;
use tonic::codegen::http;
use tonic::{Code, Request, Response, Status};

/// The API's messages and services, generated from its .proto.
#[allow(missing_docs)]
pub mod proto {
    pub use cladefs_proto::cladefsd_v1::*;
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
/// fails for good, whose error it returns; the sockets of the tenants it grants go in the
/// directory `tenants`. Runs on `daemon`'s consolidated runtime (no separate Tokio runtime; Wave
/// 4C.5, board 10774).
#[cfg(not(madsim))]
pub(super) async fn serve_control_async(
    daemon: Arc<Daemon>,
    listener: std::os::unix::net::UnixListener,
    tenants: PathBuf,
) -> std::io::Result<()> {
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(
        tokio::net::UnixListener::from_std(listener)?,
    );
    serve_control_with(daemon, incoming, tenants).await
}

/// cladefs: serves the control API for `daemon`, as [`serve_control_async`] does, to the
/// connections `incoming` accepts: through `crate::run::accepting`, an accept failing as the host
/// may get over tried again after a pause, logged once (board 9760), where tonic passed each
/// over at once, a busy loop holding one of the runtime's threads while the descriptors stayed
/// short; one of the listener's own ends it, returned. Its services are [`routes`]'.
#[cfg(not(madsim))]
pub(super) async fn serve_control_with(
    daemon: Arc<Daemon>,
    incoming: impl futures::Stream<Item = std::io::Result<tokio::net::UnixStream>>
    + Unpin
    + Send
    + 'static,
    tenants: PathBuf,
) -> std::io::Result<()> {
    let routes = routes(daemon, tenants).await;
    let failed = Arc::new(std::sync::OnceLock::new());
    let incoming = crate::run::accepting("a control API caller's", incoming, failed.clone());
    let served =
        tonic::transport::Server::builder().add_routes(routes).serve_with_incoming(incoming).await;
    crate::run::accepted_until("the control API's listener", served, failed)
}

/// cladefs: the control API's services for `daemon`, the sockets of the tenants it grants in the
/// directory `tenants`: Cladefsd, tonic's health service, its status reported as the daemon
/// serves VM processes or stops, and the Mount service of the daemon's FUSE mount of cells, if it
/// has one ([`crate::mount`], TASK-475.2), whose calls, without a mount, fail `UNIMPLEMENTED`:
/// saying why, of a daemon that can mount none (built without `mount`, or not on Linux).
/// Each call counted and refused as the daemon drains. What cladefsd serves on its control socket
/// ([`serve_control_with`]), and an embedder on an endpoint of its own (libcladefs, board 13471).
#[cfg(not(madsim))]
pub(crate) async fn routes(daemon: Arc<Daemon>, tenants: PathBuf) -> tonic::service::Routes {
    let control = Control {
        daemon: Arc::clone(&daemon),
        grants: Arc::new(Grants::new(Arc::clone(&daemon), tenants)),
    };
    let (reporter, health) = tonic_health::server::health_reporter();
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
    let routes = tonic::service::Routes::new(health)
        // cladefs: no answer encoded past MAX_MESSAGE_BYTES (board 8581): what grows refuses
        // first, saying what to leave out (`bounded`); any other, OUT_OF_RANGE, tonic's. Its
        // calls counted and traced, refusals as the daemon drains among them (board 8565,
        // board 8569).
        .add_service(Served::new(Drained::new(
            &daemon,
            CladefsdServer::new(control).max_encoding_message_size(MAX_MESSAGE_BYTES),
        )));
    // cladefs: its FUSE mount's, if it mounts cells (TASK-475.2); of a daemon that can mount none,
    // one that refuses its calls, saying why (board 14536).
    let routes = match crate::mount::service_on(&daemon) {
        Some(mount) => routes.add_service(Served::new(Drained::new(&daemon, mount))),
        None => routes,
    };
    routes
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
    host: &cladefs_core::Host,
    stats: crate::stats::HostStats,
    (leave_out_histograms, leave_out_guests): (bool, bool),
) -> GetStatsResponse {
    let named = |pairs: &[(&str, u64)]| -> std::collections::HashMap<String, u64> {
        pairs.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect()
    };
    let vms = stats.vms.into_iter().map(|vm| VmStats {
        connection: vm.connection,
        tenant: vm.cell.tenant,
        cell: vm.cell.id,
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
        incarnation: cladefs_core::ephemeral_host::host_on(host)
            .map_or(0, |host| host.incarnation()),
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
        let detached = cladefs_core::caller::detached(async move {
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

/// A tonic channel over Unix domain socket `socket`, using dummy endpoint URI `uri`.
#[cfg(not(madsim))]
pub async fn unix_channel(
    socket: impl AsRef<Path>,
    uri: &'static str,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    let socket = socket.as_ref().to_owned();
    // The URI names no host: the connector connects to the socket, whatever it asks for.
    tonic::transport::Endpoint::from_static(uri)
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}

/// A channel to the control API of the daemon listening at `socket`: for its clients, the API's
/// and its health service's.
#[cfg(not(madsim))]
pub async fn channel(
    socket: impl AsRef<Path>,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    unix_channel(socket, "http://cladefsd").await
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
    match crate::governor::full_on(daemon) {
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
    cladefs_core::bucket::errno(&failure).unwrap_or(Errno::EIO)
}

/// The status of a call that failed with `errno`.
fn status(errno: Errno) -> Status {
    saying(errno, format!("{errno:?}"))
}

/// The status of a call that failed with `errno`, saying `message`: as this API's errors are, and
/// those of the API of FUSE mounts of many cells (`crate::fuse_control`).
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

/// cladefs: the Fast WAL mode CreateCell's `fast_wal` and `fsync` ask for (doc-29): refused
/// (`EINVAL`) if `fsync` names a level its mode cannot satisfy, or if `fsync` is set without
/// `fast_wal`.
fn durability_mode(
    fast_wal: Option<bool>,
    fsync: i32,
) -> Result<Option<Option<bucket_cell::layout::Fsync>>, Status> {
    use bucket_cell::layout::Fsync;
    #[allow(deprecated)]
    let (fsync, local) = match FsyncLevel::try_from(fsync) {
        Ok(FsyncLevel::Ordered) => (Fsync::Ordered, false),
        Ok(FsyncLevel::Local) => (Fsync::Ordered, true),
        Ok(FsyncLevel::FastWal) => (Fsync::FastWal, false),
        Err(_) => return Err(status(Errno::EINVAL)),
    };
    if fast_wal == Some(true) {
        if local {
            return Err(status(Errno::EINVAL));
        }
        return Ok(Some(Some(fsync)));
    }
    if fsync == Fsync::FastWal {
        return Err(status(Errno::EINVAL));
    }
    if local {
        log::warn!(
            "cladefsd: FSYNC_LEVEL_LOCAL is deprecated and removed; using FSYNC_LEVEL_ORDERED"
        );
    }
    let fast_wal = match fast_wal {
        Some(false) => Some(None),
        None => None,
        Some(true) => unreachable!(),
    };
    Ok(fast_wal)
}

/// What StatCell's FsyncLevel is of `fsync`.
fn fsync_level(fsync: bucket_cell::layout::Fsync) -> FsyncLevel {
    match fsync {
        bucket_cell::layout::Fsync::Ordered | bucket_cell::layout::Fsync::Replicated => {
            FsyncLevel::Ordered
        }
        bucket_cell::layout::Fsync::FastWal => FsyncLevel::FastWal,
    }
}

/// The reply to a call that did, or failed with an errno.
/// What StatCell says of a cell that is and holds `state`.
fn stat_response(state: cells::CellState) -> StatCellResponse {
    let uploading = state.upload.is_some();
    let upload = state.upload.unwrap_or(local_cell::upload::UploadStatus {
        taking: None,
        uploaded: 0,
        pending_bytes: 0,
        oldest_pending: None,
        stalls: 0,
        fast_wal: None,
    });
    let millis = |time: std::time::SystemTime| {
        time.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
    };
    let waited = |since: std::time::SystemTime| {
        since.elapsed().map_or(0, |waited| waited.as_millis() as u64)
    };
    let fast_wal_fsync = upload.taking.and_then(|taking| taking.fast_wal);
    let fsync = fast_wal_fsync.map_or(FsyncLevel::Ordered, fsync_level);
    let fast_wal_active = fast_wal_fsync.is_some() || upload.fast_wal.is_some();
    let (fast_wal_bucket, fast_wal_degraded) = match upload.fast_wal {
        Some(fw) => (fw.bucket, fw.degraded),
        None => (String::new(), false),
    };
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
        uploading,
        taken_token: upload.taking.map_or(0, |taking| taking.token),
        taken_epoch: upload.taking.map_or(0, |taking| taking.epoch),
        taken_unix_ms: upload.taking.map_or(0, |taking| millis(taking.taken)),
        uploaded: upload.uploaded,
        pending_upload_bytes: upload.pending_bytes,
        oldest_pending_ms: upload.oldest_pending.map_or(0, waited),
        fsync: fsync as i32,
        fast_wal: fast_wal_active,
        fast_wal_bucket,
        fast_wal_degraded,
        ephemeral: state.ephemeral,
        placement_token: state.placement.unwrap_or(0),
    }
}

fn reply<T>(done: Result<T, Errno>) -> Result<Response<T>, Status> {
    done.map(Response::new).map_err(status)
}

/// A non-empty host path (used for input image/memory files on import, never cell directories).
fn input_path(path: String) -> Result<PathBuf, Status> {
    if path.is_empty() {
        return Err(status(Errno::EINVAL));
    }
    Ok(PathBuf::from(path))
}

/// Validates a `(tenant, cell)` cell identifier from a control RPC (`cell == 0` -> `EINVAL`).
fn cell_id(tenant: u64, cell: u64) -> Result<cladefs_core::CellId, Status> {
    if cell == 0 {
        return Err(status(Errno::EINVAL));
    }
    Ok(cladefs_core::CellId { tenant, id: cell })
}

/// Best-effort removal of a failed staging directory under `daemon`.
async fn cleanup_staging(daemon: &Daemon, staging: PathBuf) {
    let local = Arc::clone(daemon.host.local());
    // Best-effort cleanup after an earlier failure; the primary error is returned to the caller.
    let _ = cells::blocking(move || {
        if host_fs::exists(&staging) {
            // Best-effort cleanup of failed staging directory.
            let _ = local_cell::remove_cell_dir_on(&local, &staging);
        }
        Ok(())
    })
    .await;
}

/// The API's calls, served from `daemon`'s cells, its managed disk pool, and its grants.
struct Control {
    daemon: Arc<Daemon>,
    grants: Arc<Grants>,
}

impl Control {
    /// Returns the daemon's managed [`super::disks::DiskPool`], or `ENOTSUP` if unconfigured.
    fn disk_pool(&self) -> Result<&Arc<crate::disks::DiskPool>, Status> {
        self.daemon.disk_pool().ok_or_else(|| {
            saying(
                Errno::ENOTSUP,
                "no managed disks configured (configure [[disks]] in TOML)".to_owned(),
            )
        })
    }

    /// Resolves `(tenant, cell)` to its canonical local cell directory across the managed disk
    /// pool, returning `ENOENT` if no disk on this host holds it.
    async fn resolve_cell(
        &self,
        tenant: u64,
        cell: u64,
    ) -> Result<(cladefs_core::CellId, PathBuf), Status> {
        let id = cell_id(tenant, cell)?;
        let pool = Arc::clone(self.disk_pool()?);
        let path = cells::blocking(move || pool.find_cell(id).ok_or(Errno::ENOENT))
            .await
            .map_err(status)?;
        Ok((id, path))
    }

    async fn remove_cell<R>(
        &self,
        tenant: u64,
        cell: u64,
        drop_only: bool,
        response: R,
    ) -> Result<Response<R>, Status> {
        let (id, cell_dir) = self.resolve_cell(tenant, cell).await?;
        let done = if drop_only {
            cells::drop_here(&self.daemon, cell_dir).await
        } else {
            cells::delete(&self.daemon, cell_dir).await
        };
        if done.is_ok()
            && let Some(pool) = self.daemon.disk_pool()
        {
            pool.forget_cell(id);
        }
        reply(done.map(|()| response))
    }

    /// Resolves `(tenant, cell)` for migration hand-over / abort, falling back to the ephemeral
    /// host journal's recorded path when a prior crash already deleted the local cell directory.
    async fn resolve_migrating_cell(
        &self,
        tenant: u64,
        cell: u64,
    ) -> Result<(cladefs_core::CellId, PathBuf), Status> {
        let id = cell_id(tenant, cell)?;
        let pool = Arc::clone(self.disk_pool()?);
        let host = Arc::clone(&self.daemon.host);
        let path = cells::blocking(move || {
            if let Some(found) = pool.find_cell(id) {
                return Ok(found);
            }
            if let Ok(ephemeral) = cladefs_core::ephemeral_host::host_on(&host)
                && let Ok(Some(entry)) = ephemeral.entry(id)
            {
                return Ok(entry.path);
            }
            Err(Errno::ENOENT)
        })
        .await
        .map_err(status)?;
        Ok((id, path))
    }
}

/// The daemon's managed [`super::disks::DiskPool`], or `ENOTSUP` if unconfigured.
fn disk_pool_of(daemon: &Daemon) -> Result<&Arc<crate::disks::DiskPool>, Status> {
    daemon.disk_pool().ok_or_else(|| {
        saying(
            Errno::ENOTSUP,
            "no managed disks configured (configure [[disks]] in TOML)".to_owned(),
        )
    })
}

/// Picks a managed disk of `daemon`'s and allocates a fresh staging path inside
/// `<disk>/cells/<TENANT>`.
async fn stage_for(
    daemon: &Daemon,
    tenant: u64,
) -> Result<(Arc<crate::disks::ManagedDisk>, PathBuf), Status> {
    let pool = Arc::clone(disk_pool_of(daemon)?);
    let disk = pool.pick_disk().map_err(status)?;
    let disk_for_stage = Arc::clone(&disk);
    let staging =
        cells::blocking(move || crate::disks::DiskPool::staging_path(&disk_for_stage, tenant))
            .await
            .map_err(status)?;
    Ok((disk, staging))
}

/// Atomically moves `staging` into `<disk>/cells/<TENANT>/<CELL>`, registering `made` in the
/// pool and migrating any per-cell upload interval entry.
async fn commit_staged(
    daemon: &Daemon,
    disk: Arc<crate::disks::ManagedDisk>,
    made: cladefs_core::CellId,
    staging: PathBuf,
) -> Result<PathBuf, Status> {
    let pool = Arc::clone(disk_pool_of(daemon)?);
    let staging_for_commit = staging.clone();
    match cells::blocking(move || pool.commit_staged(&disk, made, &staging_for_commit)).await {
        Ok(final_path) => {
            cells::move_upload_interval(daemon, &staging, &final_path);
            Ok(final_path)
        }
        Err(err) => {
            cleanup_staging(daemon, staging).await;
            Err(status(err))
        }
    }
}

/// Imports the container image at `image` (a `docker export` tarball, or an OCI image layout and
/// its `reference`) into a new managed cell of `tenant`'s, of `capacity` bytes, on a disk of
/// `daemon`'s: as `ImportImage` does, and `cladefsctl import`.
async fn import_image(
    daemon: &Arc<Daemon>,
    tenant: u64,
    capacity: u64,
    image: PathBuf,
    reference: Option<String>,
) -> Result<cladefs_core::CellId, Status> {
    room_for_a_cell(daemon)?;
    let (disk, staging) = stage_for(daemon, tenant).await?;
    let made =
        match cells::import(daemon, staging.clone(), capacity, image, reference, tenant).await {
            Ok(made) => made,
            Err(err) => {
                cleanup_staging(daemon, staging).await;
                return Err(status(err));
            }
        };
    commit_staged(daemon, disk, made, staging).await?;
    Ok(made)
}

impl Daemon {
    /// Imports the container image at `image` (a `docker export` tarball, or an OCI image layout
    /// and its `reference`) into a new managed cell of `tenant`'s, of `capacity` bytes: what
    /// `ImportImage` does, for a process embedding the daemon (libcladefs), which boots VMs on
    /// forks of it.
    pub async fn import_image(
        self: &Arc<Self>,
        tenant: u64,
        capacity: u64,
        image: PathBuf,
        reference: Option<String>,
    ) -> anyhow::Result<cladefs_core::CellId> {
        import_image(self, tenant, capacity, image, reference)
            .await
            .map_err(|status| anyhow::anyhow!("importing an image: {}", status.message()))
    }
}

#[tonic::async_trait]
impl Cladefsd for Control {
    async fn create_cell(
        &self,
        request: Request<CreateCellRequest>,
    ) -> Result<Response<CreateCellResponse>, Status> {
        let CreateCellRequest {
            capacity,
            token,
            tenant,
            cell,
            kind,
            fsync,
            upload_interval_ms,
            fast_wal,
            ephemeral,
        } = request.into_inner();
        if ephemeral
            && (cell != 0
                || token.is_some()
                || fast_wal.is_some()
                || fsync != FsyncLevel::Ordered as i32
                || upload_interval_ms.is_some())
        {
            return Err(status(Errno::EINVAL));
        }
        let upload_interval =
            cells::parse_upload_interval_ms(upload_interval_ms).map_err(status)?;
        room_for_a_cell(&self.daemon)?;
        let fast_wal = durability_mode(fast_wal, fsync)?;
        // cladefs: a fast WAL cell needs a configured appendable zonal bucket (doc-29):
        // refused before anything is made.
        if fast_wal.is_some_and(|fw| fw.is_some()) {
            let configured = storage::fast_wal_config(&self.daemon).map_err(status)?;
            if configured.is_none() {
                return Err(saying(
                    Errno::ENOTSUP,
                    "a fast WAL cell needs the daemon's appendable zonal bucket \
                     (CLADEFSD_FAST_WAL_BUCKET)"
                        .to_owned(),
                ));
            }
        }
        let kind = match CellKind::try_from(kind) {
            Ok(CellKind::Fxfs) => Kind::Fxfs,
            Ok(CellKind::Sqlite) => Kind::Sqlite,
            Ok(CellKind::Memory) => Kind::Memory,
            Err(_) => return Err(status(Errno::EINVAL)),
        };
        let pool = Arc::clone(self.disk_pool()?);
        if cell != 0 {
            let target_id = cladefs_core::CellId { tenant, id: cell };
            let _reserved = pool.reserve_cell(target_id).map_err(status)?;
            let pool_check = Arc::clone(&pool);
            let host = Arc::clone(&self.daemon.host);
            let exists = cells::blocking(move || {
                if let Some(existing) = pool_check.find_cell(target_id) {
                    cells::removed_if_unmade_on(&host, &existing)?;
                    if host_fs::exists(&existing) {
                        return Ok(true);
                    }
                    pool_check.forget_cell(target_id);
                }
                Ok(false)
            })
            .await
            .map_err(status)?;
            if exists {
                return Err(status(Errno::EEXIST));
            }
            let disk = pool.pick_disk().map_err(status)?;
            let disk_dir = Arc::clone(&disk);
            let final_path = cells::blocking(move || {
                crate::disks::DiskPool::ensure_tenant_dir(&disk_dir, tenant)?;
                Ok(crate::disks::DiskPool::cell_path(&disk_dir, target_id))
            })
            .await
            .map_err(status)?;
            let made = cells::create(
                &self.daemon,
                final_path.clone(),
                capacity,
                target_id,
                (token, kind, fast_wal, upload_interval, false),
            )
            .await
            .map_err(status)?;
            pool.register_cell(made, final_path);
            return Ok(Response::new(CreateCellResponse { tenant: made.tenant, cell: made.id }));
        }
        let (disk, staging) = stage_for(&self.daemon, tenant).await?;
        let made = match cells::create(
            &self.daemon,
            staging.clone(),
            capacity,
            cladefs_core::CellId { tenant, id: 0 },
            (token, kind, fast_wal, upload_interval, ephemeral),
        )
        .await
        {
            Ok(made) => made,
            Err(err) => {
                cleanup_staging(&self.daemon, staging).await;
                return Err(status(err));
            }
        };
        let _ = commit_staged(&self.daemon, disk, made, staging).await?;
        Ok(Response::new(CreateCellResponse { tenant: made.tenant, cell: made.id }))
    }

    async fn import_image(
        &self,
        request: Request<ImportImageRequest>,
    ) -> Result<Response<ImportImageResponse>, Status> {
        let ImportImageRequest { capacity, image, reference, tenant } = request.into_inner();
        let image = input_path(image)?;
        let reference = Some(reference).filter(|reference| !reference.is_empty());
        let made = import_image(&self.daemon, tenant, capacity, image, reference).await?;
        Ok(Response::new(ImportImageResponse { tenant: made.tenant, cell: made.id }))
    }

    async fn import_memory(
        &self,
        request: Request<ImportMemoryRequest>,
    ) -> Result<Response<ImportMemoryResponse>, Status> {
        let ImportMemoryRequest { file, tenant, make_version, ephemeral } = request.into_inner();
        let file = input_path(file)?;
        if ephemeral && make_version {
            return Err(status(Errno::EINVAL));
        }
        room_for_a_cell(&self.daemon)?;
        let (disk, staging) = stage_for(&self.daemon, tenant).await?;
        let (made, version) = match cells::import_memory(
            &self.daemon,
            staging.clone(),
            file,
            tenant,
            make_version,
            ephemeral,
        )
        .await
        {
            Ok(imported) => imported,
            Err(err) => {
                cleanup_staging(&self.daemon, staging).await;
                return Err(status(err));
            }
        };
        let final_path = commit_staged(&self.daemon, disk, made, staging).await?;
        if let Some(ver) = version {
            let named = std::collections::BTreeSet::from([ver]);
            cladefs_core::host_cell::made_version_on(
                &self.daemon.host,
                made,
                ver,
                &final_path,
                &named,
            );
        }
        Ok(Response::new(ImportMemoryResponse {
            tenant: made.tenant,
            cell: made.id,
            version: version.unwrap_or(0),
        }))
    }

    async fn fork_cell(
        &self,
        request: Request<ForkCellRequest>,
    ) -> Result<Response<ForkCellResponse>, Status> {
        #[allow(deprecated)]
        let ForkCellRequest {
            version,
            tenant: of_tenant,
            cell,
            ephemeral,
            target_tenant,
            persistent,
        } = request.into_inner();
        let of = cell_id(of_tenant, cell)?;
        // Contradictory request: deprecated `ephemeral: true` alongside `persistent: Some(true)`.
        if ephemeral && persistent == Some(true) {
            return Err(status(Errno::EINVAL));
        }
        let is_ephemeral = match persistent {
            Some(persistent) => !persistent,
            None => ephemeral,
        };
        room_for_a_cell(&self.daemon)?;
        if version == 0 && is_ephemeral {
            return Err(status(Errno::EINVAL));
        }
        let explicit_tenant = target_tenant.filter(|&t| t != 0);
        let dest_tenant = explicit_tenant.unwrap_or(of.tenant);
        let pool = Arc::clone(self.disk_pool()?);
        if version != 0 && is_ephemeral {
            let disk = pool.pick_disk().map_err(status)?;
            let disk_dir = Arc::clone(&disk);
            let tenant_dir = cells::blocking(move || {
                crate::disks::DiskPool::ensure_tenant_dir(&disk_dir, dest_tenant)
            })
            .await
            .map_err(status)?;
            let (forked, final_path) = cells::fork_ephemeral_of_version_in(
                &self.daemon,
                of,
                version,
                tenant_dir,
                explicit_tenant,
            )
            .await
            .map_err(status)?;
            pool.register_cell(forked, final_path);
            return Ok(Response::new(ForkCellResponse { tenant: forked.tenant, cell: forked.id }));
        }
        let (disk, staging) = stage_for(&self.daemon, dest_tenant).await?;
        let forked = match version {
            0 => {
                let (_, from_path) = self.resolve_cell(of.tenant, of.id).await?;
                cells::fork(&self.daemon, from_path, staging.clone(), explicit_tenant).await
            }
            _ => {
                cells::fork_of_version(&self.daemon, of, version, staging.clone(), explicit_tenant)
                    .await
            }
        };
        let forked = match forked {
            Ok(forked) => forked,
            Err(err) => {
                cleanup_staging(&self.daemon, staging).await;
                return Err(status(err));
            }
        };
        let _ = commit_staged(&self.daemon, disk, forked, staging).await?;
        Ok(Response::new(ForkCellResponse { tenant: forked.tenant, cell: forked.id }))
    }

    async fn migrate_ephemeral_cell(
        &self,
        request: Request<MigrateEphemeralCellRequest>,
    ) -> Result<Response<MigrateEphemeralCellResponse>, Status> {
        let MigrateEphemeralCellRequest { peer, tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        let migration = crate::migration::migrate_on(&self.daemon, cell_dir, peer).await?;
        Ok(Response::new(MigrateEphemeralCellResponse { migration }))
    }

    async fn hand_over_ephemeral_cell(
        &self,
        request: Request<HandOverEphemeralCellRequest>,
    ) -> Result<Response<HandOverEphemeralCellResponse>, Status> {
        let HandOverEphemeralCellRequest { tenant, cell } = request.into_inner();
        let (id, cell_dir) = self.resolve_migrating_cell(tenant, cell).await?;
        crate::migration::hand_over_on(&self.daemon, cell_dir).await?;
        if let Some(pool) = self.daemon.disk_pool() {
            pool.forget_cell(id);
        }
        Ok(Response::new(HandOverEphemeralCellResponse {}))
    }

    async fn abort_ephemeral_migration(
        &self,
        request: Request<AbortEphemeralMigrationRequest>,
    ) -> Result<Response<AbortEphemeralMigrationResponse>, Status> {
        let AbortEphemeralMigrationRequest { tenant, cell } = request.into_inner();
        let (id, cell_dir) = self.resolve_migrating_cell(tenant, cell).await?;
        let done = crate::migration::abort_on(&self.daemon, cell_dir).await;
        if let Err(ref err) = done
            && errno(err) == Some(Errno::EEXIST)
            && let Some(pool) = self.daemon.disk_pool()
        {
            pool.forget_cell(id);
        }
        done.map(|()| Response::new(AbortEphemeralMigrationResponse {}))
    }

    async fn wait_ephemeral_hand_over(
        &self,
        request: Request<WaitEphemeralHandOverRequest>,
    ) -> Result<Response<WaitEphemeralHandOverResponse>, Status> {
        let WaitEphemeralHandOverRequest { tenant, cell, migration } = request.into_inner();
        let id = cell_id(tenant, cell)?;
        let waited = super::migration::wait_hand_over_on(&self.daemon, id, migration).await?;
        Ok(Response::new(WaitEphemeralHandOverResponse {
            migration: waited.migration,
            tenant: waited.cell.tenant,
            cell: waited.cell.id,
        }))
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
            let own = cladefs_core::ephemeral_host::host_on(&self.daemon.host)
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
        let DeleteCellRequest { tenant, cell } = request.into_inner();
        self.remove_cell(tenant, cell, false, DeleteCellResponse {}).await
    }

    async fn drop_cell(
        &self,
        request: Request<DropCellRequest>,
    ) -> Result<Response<DropCellResponse>, Status> {
        let DropCellRequest { tenant, cell } = request.into_inner();
        self.remove_cell(tenant, cell, true, DropCellResponse {}).await
    }

    async fn resize_cell(
        &self,
        request: Request<ResizeCellRequest>,
    ) -> Result<Response<ResizeCellResponse>, Status> {
        let ResizeCellRequest { capacity, tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        reply(cells::resize(&self.daemon, cell_dir, capacity).await.map(|()| ResizeCellResponse {}))
    }

    async fn place_cell(
        &self,
        request: Request<PlaceCellRequest>,
    ) -> Result<Response<PlaceCellResponse>, Status> {
        let PlaceCellRequest { token, tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        reply(cells::place(&self.daemon, cell_dir, token).await.map(|()| PlaceCellResponse {}))
    }

    async fn make_public(
        &self,
        request: Request<MakePublicRequest>,
    ) -> Result<Response<MakePublicResponse>, Status> {
        let MakePublicRequest { tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        reply(cells::make_public(&self.daemon, cell_dir).await.map(|()| MakePublicResponse {}))
    }

    async fn make_version(
        &self,
        request: Request<MakeVersionRequest>,
    ) -> Result<Response<MakeVersionResponse>, Status> {
        let MakeVersionRequest { keep_last, tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        let keep_last = Some(keep_last).filter(|&keep| keep > 0);
        let made = cells::make_version(&self.daemon, cell_dir, keep_last).await;
        reply(made.map(|version| MakeVersionResponse { version }))
    }

    async fn list_versions(
        &self,
        request: Request<ListVersionsRequest>,
    ) -> Result<Response<ListVersionsResponse>, Status> {
        let ListVersionsRequest { tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        let versions = cells::versions(&self.daemon, cell_dir).await.map_err(status)?;
        let listed = ListVersionsResponse {
            versions: (versions.into_iter())
                .map(|cladefs_core::VersionInfo { id, uploaded, shared }| Version {
                    id,
                    uploaded,
                    shared,
                })
                .collect(),
        };
        bounded("ListVersions", listed, "drop versions (DropVersions)")
    }

    async fn drop_versions(
        &self,
        request: Request<DropVersionsRequest>,
    ) -> Result<Response<DropVersionsResponse>, Status> {
        let DropVersionsRequest { versions, tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        let dropped =
            cells::drop_versions(&self.daemon, cell_dir, versions.into_iter().collect()).await;
        reply(dropped.map(|()| DropVersionsResponse {}))
    }

    async fn share_version(
        &self,
        request: Request<ShareVersionRequest>,
    ) -> Result<Response<ShareVersionResponse>, Status> {
        let ShareVersionRequest { version, shared, tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        let shared = cells::share_version(&self.daemon, cell_dir, version, shared).await;
        reply(shared.map(|()| ShareVersionResponse {}))
    }

    async fn sync_cell(
        &self,
        request: Request<SyncCellRequest>,
    ) -> Result<Response<SyncCellResponse>, Status> {
        let SyncCellRequest { tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        reply(cells::sync(&self.daemon, cell_dir).await.map(|()| SyncCellResponse {}))
    }

    async fn stat_cell(
        &self,
        request: Request<StatCellRequest>,
    ) -> Result<Response<StatCellResponse>, Status> {
        let StatCellRequest { tenant, cell } = request.into_inner();
        let (_, cell_dir) = self.resolve_cell(tenant, cell).await?;
        reply(cells::stat(&self.daemon, cell_dir).await.map(stat_response))
    }

    async fn list_cells(
        &self,
        _request: Request<ListCellsRequest>,
    ) -> Result<Response<ListCellsResponse>, Status> {
        let pool = self.daemon.disk_pool();
        let cells = cells::list(&self.daemon).map_err(status)?.into_iter().map(|(path, state)| {
            let disk_id = pool.and_then(|pool| pool.disk_id_for_path(&path)).unwrap_or_default();
            ListedCell {
                tenant: state.cell.tenant,
                cell: state.cell.id,
                disk_id,
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
        let cladefs_core::stats::Stats {
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
        } = cladefs_core::stats::stats();
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
            0 => crate::stats::WAIT,
            ms => std::time::Duration::from_millis(ms.into()),
        };
        let stats = crate::stats::take_on(&self.daemon, wait, !leave_out_vms).await;
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
        let GrantRequest { tenant, uid, gid } = request.into_inner();
        let pool = Arc::clone(self.disk_pool()?);
        let grants = self.grants.clone();
        let granted = cells::blocking(move || {
            let all_dirs = pool.ensure_tenant_dirs(tenant)?;
            let socket = grants.grant_many(tenant, &all_dirs, (uid, gid))?;
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
        if let Err(errno) = crate::peers::check_on(&self.daemon, &addresses) {
            return reply(Err(errno));
        }
        // cladefs: thin v1 gRPC shim over global `events` (board 11711): publish `PeersUpdated`
        // so all daemons sharing the bucket converge without fan-out, then apply locally.
        let bucket = storage::daemon_bucket_on(&self.daemon).map_err(status)?;
        let published = async {
            let number = bucket_cell::events::bucket_number(&bucket).await?;
            let auth = bucket_cell::events::authority(&bucket).await?;
            let log = bucket_cell::events::publish(
                &*auth,
                bucket_cell::events::Publish::PeersUpdated {
                    bucket: number,
                    addresses: addresses.clone(),
                },
            )
            .await?;
            Ok::<_, bucket_cell::Error>((number, log))
        }
        .await;
        let (number, log) = match published {
            Ok(published) => published,
            Err(err) => {
                let failure = local_cell::upload::Failure::Bucket(err);
                let errno = cladefs_core::bucket::errno(&failure).unwrap_or(Errno::EIO);
                log::error!("cladefsd: publishing SetPeers to events: {failure}");
                return Err(saying(errno, format!("publishing SetPeers to events: {failure}")));
            }
        };
        let generation = log.peers_for(number).map_or(0, |peers| peers.generation);
        reply(
            crate::peers::set_generation_on(&self.daemon, generation, addresses)
                .map(|_| SetPeersResponse {}),
        )
    }

    async fn shred_cell(
        &self,
        request: Request<ShredCellRequest>,
    ) -> Result<Response<ShredCellResponse>, Status> {
        let ShredCellRequest { tenant, cell } = request.into_inner();
        let cell = cladefs_core::CellId { tenant, id: cell };
        let bucket = storage::daemon_bucket_on(&self.daemon).map_err(status)?;
        let shredded = cladefs_core::bucket::shred_cell_on(&self.daemon.host, &bucket, cell)
            .await
            .map_err(|err| {
                let failure = local_cell::upload::Failure::Bucket(err);
                let errno = cladefs_core::bucket::errno(&failure).unwrap_or(Errno::EIO);
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
            cells::blocking(move || Ok(cladefs_core::bucket::forget_on(&host, scope, &why))).await;
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
        use cladefs_core::bucket::ShredError;
        let ShredTenantRequest { tenant } = request.into_inner();
        let (destroyed, forgotten) =
            cladefs_core::bucket::shred_tenant_on(&self.daemon.host, tenant).await.map_err(
                |err| {
                    let errno = match &err {
                        ShredError::InvalidTenant(_) => Errno::EINVAL,
                        ShredError::Unsupported(_) => Errno::ENOTSUP,
                        ShredError::Failed(_) => Errno::EIO,
                    };
                    log::error!("cladefsd: shredding tenant {tenant}: {err}");
                    saying(errno, err.to_string())
                },
            )?;
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
            cladefs_core::bucket::reload_kms_on(&self.daemon.host).await.map_err(|refused| {
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

    async fn list_disks(
        &self,
        _request: Request<ListDisksRequest>,
    ) -> Result<Response<ListDisksResponse>, Status> {
        use bucket_cell::host_disk::{DiskHealthState, DiskPlacement};
        let pool = Arc::clone(self.disk_pool()?);
        let disks = cells::blocking(move || {
            let mut out = Vec::with_capacity(pool.disks().len());
            for disk in pool.disks() {
                let hd = &disk.host_disk;
                let device = hd.dev().map_or_else(String::new, |dev| {
                    format!(
                        "{}:{}",
                        libc::major(dev as libc::dev_t),
                        libc::minor(dev as libc::dev_t)
                    )
                });
                // Sample failure on an offline/broken disk reports 0 size/free without failing the
                // whole ListDisks RPC.
                let (size_bytes, free_bytes) =
                    hd.sample().map_or((0, 0), |sample| (sample.size, sample.free));
                let state = match hd.health() {
                    DiskHealthState::Online => DiskHealthMode::DiskHealthOnline,
                    DiskHealthState::Suspect => DiskHealthMode::DiskHealthSuspect,
                    DiskHealthState::Offline => DiskHealthMode::DiskHealthOffline,
                    DiskHealthState::Returning => DiskHealthMode::DiskHealthReturning,
                } as i32;
                let placement = match disk.placement() {
                    DiskPlacement::ReadWrite => DiskPlacementMode::DiskPlacementReadWrite,
                    DiskPlacement::Drain => DiskPlacementMode::DiskPlacementDrain,
                    DiskPlacement::ReadOnly => DiskPlacementMode::DiskPlacementReadOnly,
                } as i32;
                out.push(DiskInfo {
                    id: disk.id.clone(),
                    device,
                    state,
                    placement,
                    size_bytes,
                    free_bytes,
                    cap_bytes: disk.cap().unwrap_or(0),
                    headroom_bytes: hd.headroom(),
                    allowance_bytes: hd.allowance(),
                    cells_bytes: hd.cells.bytes(),
                    reserved_bytes: hd.cells.reserved() + hd.cells.pledged(),
                    cache_bytes: hd.cache.get(),
                    cache_cap_bytes: disk.cache_cap().unwrap_or(0),
                    spare_bytes: disk.spare(),
                    under_pressure: hd.under_pressure(),
                    dirty_budget_bytes: disk.dirty_budget().unwrap_or(0),
                });
            }
            Ok(out)
        })
        .await
        .map_err(status)?;
        bounded("ListDisks", ListDisksResponse { disks }, "fewer managed disks")
    }

    async fn set_disk_governance(
        &self,
        request: Request<SetDiskGovernanceRequest>,
    ) -> Result<Response<SetDiskGovernanceResponse>, Status> {
        use bucket_cell::host_disk::{DiskHeadroom, DiskPlacement};
        let SetDiskGovernanceRequest {
            id,
            cap_bytes,
            clear_cap,
            cache_cap_bytes,
            clear_cache_cap,
            headroom_bytes,
            headroom_percent,
            clear_headroom,
            dirty_budget_bytes,
            clear_dirty_budget,
            placement,
        } = request.into_inner();
        if id.is_empty()
            || (cap_bytes.is_some() && clear_cap)
            || (cache_cap_bytes.is_some() && clear_cache_cap)
            || (headroom_bytes.is_some() && headroom_percent.is_some())
            || ((headroom_bytes.is_some() || headroom_percent.is_some()) && clear_headroom)
            || (dirty_budget_bytes.is_some() && clear_dirty_budget)
        {
            return Err(status(Errno::EINVAL));
        }
        let placement = match placement {
            None => None,
            Some(raw) => Some(match DiskPlacementMode::try_from(raw) {
                Ok(DiskPlacementMode::DiskPlacementReadWrite) => DiskPlacement::ReadWrite,
                Ok(DiskPlacementMode::DiskPlacementDrain) => DiskPlacement::Drain,
                Ok(DiskPlacementMode::DiskPlacementReadOnly) => DiskPlacement::ReadOnly,
                Err(_) => return Err(status(Errno::EINVAL)),
            }),
        };
        let headroom = if clear_headroom {
            Some(None)
        } else if let Some(bytes) = headroom_bytes {
            Some(Some(DiskHeadroom::Bytes(bytes)))
        } else {
            headroom_percent.map(|pct| Some(DiskHeadroom::Percent(u64::from(pct))))
        };
        let update = crate::disks::GovernanceUpdate {
            cap: if clear_cap { Some(None) } else { cap_bytes.map(Some) },
            cache_cap: if clear_cache_cap { Some(None) } else { cache_cap_bytes.map(Some) },
            headroom,
            dirty_budget: if clear_dirty_budget {
                Some(None)
            } else {
                dirty_budget_bytes.map(Some)
            },
            placement,
        };
        let pool = self.disk_pool()?;
        reply(pool.update_governance(&id, update).map(|()| SetDiskGovernanceResponse {}))
    }
}

/// cladefs: what a host forgot of a tenant's or a cell's keys, as the API has it (TASK-411.7).
fn forgotten_of(forgotten: cladefs_core::bucket::Forgotten) -> Forgotten {
    let count = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
    let cladefs_core::bucket::Forgotten { stores, rings, kept, extents, bytes } = forgotten;
    Forgotten { stores: count(stores), rings: count(rings), kept: count(kept), extents, bytes }
}

/// The error of a call that failed with `status`, saying it was `what`: with the errno it failed
/// with, if any ([`crate::Error::errno`] finds it), and what the daemon said of it (board 8583),
/// unless it said no more than the errno's name ([`status`]); without an errno, the status whole.
pub fn failed(what: String, status: Status) -> cladefs_core::Error {
    cladefs_core::Error::new(match errno(&status) {
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
                let heard = match futures::FutureExt::now_or_never(cladefs_core::caller::gone()) {
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
        crate::governor::set_full_on(&daemon, true);
        let refused = room_for_a_cell(&daemon).unwrap_err();
        assert_eq!(refused.code(), Code::ResourceExhausted);
        assert_eq!(errno(&refused), Some(Errno::ENOMEM));
        assert_eq!(refused.message(), "the host's memory is full: the daemon makes no cell more");
        crate::governor::set_full_on(&daemon, false);
        assert!(room_for_a_cell(&daemon).is_ok());
    }

    /// cladefs: board 11669: `ImportImage` enforces full-host memory admission control
    /// (`room_for_a_cell`), returning `RESOURCE_EXHAUSTED` with `ENOMEM` when the host is full.
    #[tokio::test]
    async fn import_image_is_refused_while_the_hosts_memory_is_full() {
        let daemon = Daemon::new();
        crate::governor::set_full_on(&daemon, true);
        let grants = Arc::new(Grants::new(Arc::clone(&daemon), PathBuf::new()));
        let control = Control { daemon, grants };
        let refused = Cladefsd::import_image(
            &control,
            Request::new(ImportImageRequest {
                capacity: 1 << 24,
                image: "/nonexistent/image".into(),
                reference: String::new(),
                tenant: 0,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(refused.code(), Code::ResourceExhausted);
        assert_eq!(errno(&refused), Some(Errno::ENOMEM));
        assert_eq!(refused.message(), "the host's memory is full: the daemon makes no cell more");
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
