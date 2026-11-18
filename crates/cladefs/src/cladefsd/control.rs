// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's control API (TASK-43.3): gRPC over a Unix socket, for the host's VM manager, which
//! launches the VMMs. Its calls and their errors are `proto/cladefs/cladefsd/v1/cladefsd.proto`'s.
//!
//! The store operations it serves are futures ([`super::stores`], TASK-43.6): their waits, for
//! uploads, guests' answers, and stores to be let go, hold no thread, and their stores' I/O runs on
//! the runtime's blocking threads, so that many calls waiting, syncs while object storage is slow,
//! keep no other call waiting.

use super::disk;
use super::grants::Grants;
use super::memory;
use super::stores;
use crate::Errno;
use extent_bucket::cell::Kind;
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

/// The metadata entry of a failed call's status holding the errno it failed with, a decimal
/// number.
pub const ERRNO: &str = "cladefs-errno";

/// How many threads serve the API's calls, and their waits, which hold none (TASK-43.6).
const THREADS: usize = 2;

/// How many threads the calls' stores' I/O runs on at most: what waits holds none of them, so that
/// many calls waiting, for uploads or guests, leave them to the others' I/O.
pub const BLOCKING_THREADS: usize = 32;

/// Serves the control API to the callers that connect to `listener`, until accepting fails; the
/// sockets of the tenants it grants go in the directory `tenants`. cladefs: and the Mount service of
/// the daemon's FUSE mount of cells, if it has one ([`super::mount`], TASK-475.2): its calls,
/// counted and refused as the daemon drains as the API's are; without a mount, they fail
/// `UNIMPLEMENTED`.
pub fn serve_control(
    listener: std::os::unix::net::UnixListener,
    tenants: impl Into<PathBuf>,
) -> std::io::Result<()> {
    let control = Control { grants: Arc::new(Grants::new(tenants.into())) };
    let (reporter, health) = tonic_health::server::health_reporter();
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(THREADS)
        .max_blocking_threads(BLOCKING_THREADS)
        .thread_name("cladefsd ctl")
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(
            tokio::net::UnixListener::from_std(listener)?,
        );
        // Its first status reported before it serves: the reporter starts with the server's, "",
        // SERVING, which a check served before a task of its own reported otherwise saw.
        let mut serving = super::SERVING.subscribe();
        let now = *serving.borrow_and_update();
        report_health(&reporter, now).await;
        tokio::spawn(async move {
            while serving.changed().await.is_ok() {
                let now = *serving.borrow_and_update();
                report_health(&reporter, now).await;
            }
        });
        CONTROL_SERVING.store(true, std::sync::atomic::Ordering::Release);
        tonic::transport::Server::builder()
            .add_service(health)
            .add_service(Drained(CladefsdServer::new(control)))
            // cladefs: its FUSE mount's, if it mounts cells (TASK-475.2).
            .add_optional_service(super::mount::service().map(Drained))
            .serve_with_incoming(incoming)
            .await
            .map_err(std::io::Error::other)
    })
}

/// cladefs: `stats`, as the API has it (TASK-111.4).
fn stats_response(stats: super::stats::HostStats) -> GetStatsResponse {
    let named = |pairs: &[(&str, u64)]| -> std::collections::HashMap<String, u64> {
        pairs.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect()
    };
    let vms = stats.vms.into_iter().map(|vm| VmStats {
        connection: vm.connection,
        branch: vm.branch,
        path: vm.path.to_string_lossy().into_owned(),
        follower: vm.follower,
        build: vm.build.clone().unwrap_or_default(),
        counters: named(&vm.counters()),
        gauges: named(&vm.gauges()),
        window: vm.window().map_or_else(Default::default, |window| named(&window)),
        answered: vm.answered.is_some(),
        answer_ns: vm.answered.map_or(0, |took| took.as_nanos() as u64),
        guest: vm.guest.map(|guest| GuestStats {
            build: guest.build.unwrap_or_default(),
            counters: guest.counters.into_iter().collect(),
            histograms: (guest.histograms.into_iter())
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
    }
}

/// cladefs: the API's calls in flight, which a drain waits for, refusing more (TASK-141.4).
pub(super) static CALLS: super::drain::Work = super::drain::Work::new();

/// cladefs: the API's service, `S`, its calls counted in [`CALLS`] until they are answered, and
/// refused (`UNAVAILABLE`) as the daemon drains (TASK-141.4): what a call does is done once it is
/// answered, as none streams. The health service's calls are not, as its watches never end.
#[derive(Clone)]
struct Drained<S>(S);

impl<S: tonic::server::NamedService> tonic::server::NamedService for Drained<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, B, Answer> tower::Service<http::Request<B>> for Drained<S>
where
    S: tower::Service<http::Request<B>, Response = http::Response<Answer>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    Answer: Default + Send + 'static,
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

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let Some(call) = CALLS.begin() else {
            let refused = Status::unavailable("the extent daemon drains, as it is about to exit");
            return Box::pin(std::future::ready(Ok(refused.into_http())));
        };
        let answered = self.0.call(request);
        Box::pin(async move {
            let answered = answered.await;
            drop(call);
            answered
        })
    }
}

/// Whether this process serves the control API ([`serve_control`]).
static CONTROL_SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn is_serving() -> bool {
    CONTROL_SERVING.load(std::sync::atomic::Ordering::Acquire)
}

/// Reports, through the standard gRPC health service (grpc.health.v1.Health), whether the daemon
/// serves (TASK-48), `serving`: SERVING while it accepts VM processes' connections, NOT_SERVING
/// as it starts and once it stops; the whole server's status, named "", and the control API's,
/// alike. Its callers have it reported once it returns ([`super::stop_serving`] waits for that).
async fn report_health(reporter: &tonic_health::server::HealthReporter, serving: bool) {
    use tonic::server::NamedService;
    use tonic_health::ServingStatus;
    let status = if serving { ServingStatus::Serving } else { ServingStatus::NotServing };
    reporter.set_service_status("", status).await;
    reporter.set_service_status(<CladefsdServer<Control> as NamedService>::NAME, status).await;
    super::REPORTED.send_replace(serving);
}

/// A channel to the control API of the daemon listening at `socket`: for its clients, the API's
/// and its health service's.
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

/// A client of the control API of the daemon listening at `socket`.
pub async fn connect(
    socket: impl AsRef<Path>,
) -> Result<CladefsdClient<tonic::transport::Channel>, tonic::transport::Error> {
    Ok(CladefsdClient::new(channel(socket).await?))
}

/// The errno a call failed with, as its status's [`ERRNO`] entry has it, if it has one.
pub fn errno(status: &Status) -> Option<Errno> {
    let errno = status.metadata().get(ERRNO)?.to_str().ok()?;
    errno.parse().ok().map(Errno::from_raw)
}

/// The status of a call that failed with `errno`.
fn status(errno: Errno) -> Status {
    saying(errno, format!("{errno:?}"))
}

/// The status of a call that failed with `errno`, saying `message`: as this API's errors are, and
/// those of the API of FUSE mounts of many stores (`crate::fuse::control`).
pub(crate) fn saying(errno: Errno, message: String) -> Status {
    let code = match errno {
        Errno::ENOENT => Code::NotFound,
        Errno::EEXIST => Code::AlreadyExists,
        Errno::EACCES | Errno::EPERM => Code::PermissionDenied,
        Errno::EINVAL => Code::InvalidArgument,
        Errno::ENOSPC => Code::ResourceExhausted,
        Errno::ETIMEDOUT => Code::DeadlineExceeded,
        Errno::EBUSY | Errno::ESTALE | Errno::ENOTSUP | Errno::EROFS => Code::FailedPrecondition,
        _ => Code::Internal,
    };
    let mut status = Status::new(code, message);
    status.metadata_mut().insert(ERRNO, errno.raw().into());
    status
}

/// The reply to a call that did, or failed with an errno.
/// What StatStore says of a store that is and holds `state`.
fn stat_response(state: stores::StoreState) -> StatStoreResponse {
    let upload = state.upload.unwrap_or(extent_store::upload::UploadStatus {
        taking: None,
        uploaded: 0,
        pending_bytes: 0,
        oldest_pending: None,
        stalls: 0,
    });
    let millis = |time: std::time::SystemTime| {
        time.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
    };
    let waited = |since: std::time::SystemTime| {
        since.elapsed().map_or(0, |waited| waited.as_millis() as u64)
    };
    StatStoreResponse {
        branch: state.branch.id,
        tenant: state.branch.cell.tenant,
        cell: state.branch.cell.id,
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
    }
}

/// The branch a response names by its tenant, cell and ID (TASK-364).
fn branched(tenant: u64, cell: u64, id: u64) -> crate::Branch {
    crate::Branch { cell: crate::Cell { tenant, id: cell }, id }
}

fn reply<T>(done: Result<T, Errno>) -> Result<Response<T>, Status> {
    done.map(Response::new).map_err(status)
}

/// The store a request names by `path`.
fn path(path: String) -> Result<PathBuf, Status> {
    if path.is_empty() {
        return Err(status(Errno::EINVAL));
    }
    Ok(PathBuf::from(path))
}

/// The API's calls, served from the daemon's stores, and its grants.
struct Control {
    grants: Arc<Grants>,
}

impl Control {
    /// The tenant of a new cell whose store is made at `path`, that the caller names `tenant`
    /// (TASK-364): in a granted tenant's directory, that tenant's, which `tenant` is or is 0 for
    /// (`EPERM` otherwise); elsewhere, `tenant`. A store of a cell there already, a point the VM
    /// manager gives a tenant to restore from (TASK-154), may be another tenant's: the tenant's VM
    /// processes write no store of it (`Tenant::reaches`), and its restores are of their own.
    fn new_cell_at(&self, path: &Path, tenant: u64) -> Result<u64, Status> {
        match self.grants.tenant_at(path) {
            Some(granted) if tenant == crate::host_store::DEFAULT_TENANT || tenant == granted => {
                Ok(granted)
            }
            Some(_) => Err(status(Errno::EPERM)),
            None => Ok(tenant),
        }
    }
}

#[tonic::async_trait]
impl Cladefsd for Control {
    async fn create_store(
        &self,
        request: Request<CreateStoreRequest>,
    ) -> Result<Response<CreateStoreResponse>, Status> {
        let CreateStoreRequest { path: store, capacity, branch, token, tenant, cell, kind } =
            request.into_inner();
        let store = path(store)?;
        let kind = match VdiskKind::try_from(kind) {
            Ok(VdiskKind::Fxfs) => Kind::Fxfs,
            Ok(VdiskKind::Sqlite) => Kind::Sqlite,
            Ok(VdiskKind::Memory) => Kind::Memory,
            Err(_) => return Err(status(Errno::EINVAL)),
        };
        let tenant = match cell {
            0 => self.new_cell_at(&store, tenant)?,
            _ => tenant,
        };
        let branch = crate::Branch { cell: crate::Cell { tenant, id: cell }, id: branch };
        let made = stores::create(store, capacity, branch, (token, kind)).await;
        reply(made.map(|made| CreateStoreResponse {
            branch: made.id,
            tenant: made.cell.tenant,
            cell: made.cell.id,
        }))
    }

    async fn import_image(
        &self,
        request: Request<ImportImageRequest>,
    ) -> Result<Response<ImportImageResponse>, Status> {
        let ImportImageRequest { path: store, capacity, image, reference, tenant } =
            request.into_inner();
        let (store, image) = (path(store)?, path(image)?);
        let reference = Some(reference).filter(|reference| !reference.is_empty());
        let tenant = self.new_cell_at(&store, tenant)?;
        let made = stores::import(store, capacity, image, reference, tenant).await;
        reply(made.map(|made| ImportImageResponse {
            branch: made.id,
            tenant: made.cell.tenant,
            cell: made.cell.id,
        }))
    }

    async fn fork_store(
        &self,
        request: Request<ForkStoreRequest>,
    ) -> Result<Response<ForkStoreResponse>, Status> {
        let ForkStoreRequest { from, to, point, shared } = request.into_inner();
        let (from, to) = (path(from)?, path(to)?);
        let tenant = self.grants.tenant_at(&to);
        let forked = stores::fork(from, to, tenant, (point, shared)).await;
        reply(forked.map(|forked| ForkStoreResponse {
            branch: forked.id,
            tenant: forked.cell.tenant,
            cell: forked.cell.id,
        }))
    }

    async fn delete_store(
        &self,
        request: Request<DeleteStoreRequest>,
    ) -> Result<Response<DeleteStoreResponse>, Status> {
        let store = path(request.into_inner().path)?;
        reply(stores::delete(store).await.map(|()| DeleteStoreResponse {}))
    }

    async fn drop_store(
        &self,
        request: Request<DropStoreRequest>,
    ) -> Result<Response<DropStoreResponse>, Status> {
        let store = path(request.into_inner().path)?;
        reply(stores::drop_copy(store).await.map(|()| DropStoreResponse {}))
    }

    async fn resize_store(
        &self,
        request: Request<ResizeStoreRequest>,
    ) -> Result<Response<ResizeStoreResponse>, Status> {
        let ResizeStoreRequest { path: store, capacity } = request.into_inner();
        let store = path(store)?;
        reply(stores::resize(store, capacity).await.map(|()| ResizeStoreResponse {}))
    }

    async fn place_store(
        &self,
        request: Request<PlaceStoreRequest>,
    ) -> Result<Response<PlaceStoreResponse>, Status> {
        let PlaceStoreRequest { path: store, token } = request.into_inner();
        let store = path(store)?;
        reply(stores::place(store, token).await.map(|()| PlaceStoreResponse {}))
    }

    async fn make_public(
        &self,
        request: Request<MakePublicRequest>,
    ) -> Result<Response<MakePublicResponse>, Status> {
        let MakePublicRequest { path: store, attach_only } = request.into_inner();
        let store = path(store)?;
        reply(stores::make_public(store, attach_only).await.map(|()| MakePublicResponse {}))
    }

    async fn share_point(
        &self,
        request: Request<SharePointRequest>,
    ) -> Result<Response<SharePointResponse>, Status> {
        let SharePointRequest { path: store, shared } = request.into_inner();
        let store = path(store)?;
        reply(stores::share(store, shared).await.map(|()| SharePointResponse {}))
    }

    async fn attach_base(
        &self,
        request: Request<AttachBaseRequest>,
    ) -> Result<Response<AttachBaseResponse>, Status> {
        let AttachBaseRequest { path: store, base } = request.into_inner();
        let (store, base) = (path(store)?, path(base)?);
        let grants = self.grants.clone();
        let allowed = move |store: &Path, base: &Path| grants.same_tenant(store, base);
        reply(stores::attach(store, base, allowed).await.map(|()| AttachBaseResponse {}))
    }

    async fn sync_store(
        &self,
        request: Request<SyncStoreRequest>,
    ) -> Result<Response<SyncStoreResponse>, Status> {
        let store = path(request.into_inner().path)?;
        reply(stores::sync(store).await.map(|()| SyncStoreResponse {}))
    }

    async fn stat_store(
        &self,
        request: Request<StatStoreRequest>,
    ) -> Result<Response<StatStoreResponse>, Status> {
        let store = path(request.into_inner().path)?;
        reply(stores::stat(store).await.map(stat_response))
    }

    async fn list_stores(
        &self,
        _request: Request<ListStoresRequest>,
    ) -> Result<Response<ListStoresResponse>, Status> {
        let listed = stores::list().map(|stores| {
            let stores = stores.into_iter().map(|(path, state)| ListedStore {
                path: path.to_string_lossy().into_owned(),
                stat: Some(stat_response(state)),
            });
            ListStoresResponse { stores: stores.collect() }
        });
        reply(listed)
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
        } = crate::stats::stats();
        use extent_bucket::object_bucket::{Request, Upload};
        let bucket = stores::daemon_bucket().map_err(status)?.stats();
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
        let wait = match request.into_inner().wait_ms {
            0 => super::stats::WAIT,
            ms => std::time::Duration::from_millis(ms.into()),
        };
        Ok(Response::new(stats_response(super::stats::take(wait).await)))
    }

    async fn grant(
        &self,
        request: Request<GrantRequest>,
    ) -> Result<Response<GrantResponse>, Status> {
        let GrantRequest { tenant, dir, uid, gid } = request.into_inner();
        let dir = path(dir)?;
        let grants = self.grants.clone();
        let granted = stores::blocking(move || {
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
        reply(stores::blocking(move || grants.revoke(tenant).map(|()| RevokeResponse {})).await)
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
        let shrunk = stores::blocking(move || memory::shrink(tenant, target_bytes));
        reply(shrunk.await.map(|bytes| ShrinkMemoryResponse { bytes }))
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
        let counted = stores::blocking(move || disk::count(tenant));
        let response = |(bytes, limit_bytes)| GetTenantDiskResponse { bytes, limit_bytes };
        reply(counted.await.map(response))
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
        let cell = crate::Cell { tenant, id: cell };
        let bucket = stores::daemon_bucket().map_err(status)?;
        let shredded = crate::bucket::shred_cell(&bucket, cell).await.map_err(|err| {
            let failure = extent_store::upload::Failure::Bucket(err);
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
        let scope = extent_bucket::keys::Scope::Tenant(tenant);
        let forgotten = stores::blocking(move || Ok(crate::bucket::forget(scope, &why))).await;
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
        let shredded_now = destroyed == extent_bucket::bucket_kms::Destroyed::Now;
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
}

impl Client {
    /// A client of the daemon whose control socket is at `socket`.
    pub fn connect(socket: impl AsRef<Path>) -> Result<Self, crate::Error> {
        let socket = socket.as_ref();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(crate::Error::new)?;
        let client = runtime.block_on(connect(socket)).map_err(|err| {
            crate::Error::new(anyhow::Error::new(err).context(format!(
                "connecting to the extent daemon's control socket at {}",
                socket.display()
            )))
        })?;
        Ok(Self { runtime, client })
    }

    /// Makes an empty store at `path` of `capacity` bytes, of `branch`, an Fxfs vdisk of its cell:
    /// of a new cell if its cell's ID is 0, and a new branch there if its ID is; placed at `token`
    /// if given; and returns its branch (`CreateStore`, TASK-364).
    pub fn create_store(
        &self,
        path: &Path,
        capacity: u64,
        branch: crate::Branch,
        token: Option<u64>,
    ) -> Result<crate::Branch, crate::Error> {
        let request = CreateStoreRequest {
            path: name(path)?,
            capacity,
            branch: branch.id,
            token,
            tenant: branch.cell.tenant,
            cell: branch.cell.id,
            kind: VdiskKind::Fxfs.into(),
        };
        let response = self
            .call(format!("making the store at {}", path.display()), |mut c| async move {
                c.create_store(request).await
            })?;
        Ok(branched(response.tenant, response.cell, response.branch))
    }

    /// Makes a store at `path` of `capacity` bytes holding the container image at `image`, the one
    /// `reference` names in it if given, the one vdisk of a new cell of the default tenant's, and
    /// returns its branch (`ImportImage`).
    pub fn import_image(
        &self,
        path: &Path,
        capacity: u64,
        image: &Path,
        reference: Option<&str>,
    ) -> Result<crate::Branch, crate::Error> {
        let request = ImportImageRequest {
            path: name(path)?,
            capacity,
            image: name(image)?,
            reference: reference.unwrap_or_default().to_owned(),
            tenant: crate::host_store::DEFAULT_TENANT,
        };
        let response = self.call(format!("importing {}", image.display()), |mut c| async move {
            c.import_image(request).await
        })?;
        Ok(branched(response.tenant, response.cell, response.branch))
    }

    /// Forks the store at `from` to a new branch at `to`, and returns the branch (`ForkStore`).
    pub fn fork_store(&self, from: &Path, to: &Path) -> Result<crate::Branch, crate::Error> {
        self.fork_or_point(from, to, (false, false))
    }

    /// Makes a point of the store at `from`, at `to`, and returns its branch (`ForkStore` with
    /// `point`): which any number of hosts fork at once, from copies of it (TASK-158); shared, if
    /// `shared`, as [`Client::share_point`] shares it (TASK-395.7).
    pub fn point_store(
        &self,
        from: &Path,
        to: &Path,
        shared: bool,
    ) -> Result<crate::Branch, crate::Error> {
        self.fork_or_point(from, to, (true, shared))
    }

    fn fork_or_point(
        &self,
        from: &Path,
        to: &Path,
        (point, shared): (bool, bool),
    ) -> Result<crate::Branch, crate::Error> {
        let request = ForkStoreRequest { from: name(from)?, to: name(to)?, point, shared };
        let what = format!("forking the store at {} to {}", from.display(), to.display());
        let response = self.call(what, |mut c| async move { c.fork_store(request).await })?;
        Ok(branched(response.tenant, response.cell, response.branch))
    }

    /// Deletes the store at `path` (`DeleteStore`).
    pub fn delete_store(&self, path: &Path) -> Result<(), crate::Error> {
        let request = DeleteStoreRequest { path: name(path)? };
        let what = format!("deleting the store at {}", path.display());
        self.call(what, |mut c| async move { c.delete_store(request).await }).map(drop)
    }

    /// Deletes this host's copy of the store at `path`, its branch staying in the daemon's bucket
    /// (`DropStore`).
    pub fn drop_store(&self, path: &Path) -> Result<(), crate::Error> {
        let request = DropStoreRequest { path: name(path)? };
        let what = format!("dropping the store at {}", path.display());
        self.call(what, |mut c| async move { c.drop_store(request).await }).map(drop)
    }

    /// Sets the capacity of the store at `path` to `capacity` bytes (`ResizeStore`).
    pub fn resize_store(&self, path: &Path, capacity: u64) -> Result<(), crate::Error> {
        let request = ResizeStoreRequest { path: name(path)?, capacity };
        let what = format!("resizing the store at {} to {capacity} bytes", path.display());
        self.call(what, |mut c| async move { c.resize_store(request).await }).map(drop)
    }

    /// Places the store at `path` at the placement whose fencing token is `token` (`PlaceStore`).
    pub fn place_store(&self, path: &Path, token: u64) -> Result<(), crate::Error> {
        let request = PlaceStoreRequest { path: name(path)?, token };
        let what = format!("placing the store at {}", path.display());
        self.call(what, |mut c| async move { c.place_store(request).await }).map(drop)
    }

    /// Makes the store at `path` a public base (`MakePublic`).
    pub fn make_public(&self, path: &Path) -> Result<(), crate::Error> {
        let request = MakePublicRequest { path: name(path)?, attach_only: false };
        let what = format!("making the store at {} public", path.display());
        self.call(what, |mut c| async move { c.make_public(request).await }).map(drop)
    }

    /// Makes the fork at `path` public, its branch alone, to attach to stores, not to fork
    /// (`MakePublic` with `attach_only`).
    pub fn make_public_attach_only(&self, path: &Path) -> Result<(), crate::Error> {
        let request = MakePublicRequest { path: name(path)?, attach_only: true };
        let what = format!("making the fork at {} public to attach", path.display());
        self.call(what, |mut c| async move { c.make_public(request).await }).map(drop)
    }

    /// Shares the point the store at `path` is of, if `shared`, or no longer (`SharePoint`,
    /// TASK-395.7): other tenants' forks of it are made while it is shared.
    pub fn share_point(&self, path: &Path, shared: bool) -> Result<(), crate::Error> {
        let request = SharePointRequest { path: name(path)?, shared };
        let what = match shared {
            true => format!("sharing the point at {}", path.display()),
            false => format!("unsharing the point at {}", path.display()),
        };
        self.call(what, |mut c| async move { c.share_point(request).await }).map(drop)
    }

    /// Attaches the fork at `base` to the store at `path` (`AttachBase`).
    pub fn attach_base(&self, path: &Path, base: &Path) -> Result<(), crate::Error> {
        let request = AttachBaseRequest { path: name(path)?, base: name(base)? };
        let what = format!("attaching {} to {}", base.display(), path.display());
        self.call(what, |mut c| async move { c.attach_base(request).await }).map(drop)
    }

    /// What the store at `path` is, and holds (`StatStore`).
    pub fn stat_store(&self, path: &Path) -> Result<StatStoreResponse, crate::Error> {
        let request = StatStoreRequest { path: name(path)? };
        let what = format!("asking what the store at {} holds", path.display());
        self.call(what, |mut c| async move { c.stat_store(request).await })
    }

    /// The stores the daemon serves, with what each is and holds (`ListStores`).
    pub fn list_stores(&self) -> Result<Vec<ListedStore>, crate::Error> {
        let what = "listing the daemon's stores".to_owned();
        let listed =
            self.call(what, |mut c| async move { c.list_stores(ListStoresRequest {}).await })?;
        Ok(listed.stores)
    }

    /// The branch of the store at `path`, in its cell (`StatStore`'s `tenant`, `cell` and
    /// `branch`, TASK-364).
    pub fn branch(&self, path: &Path) -> Result<crate::Branch, crate::Error> {
        let stat = self.stat_store(path)?;
        Ok(branched(stat.tenant, stat.cell, stat.branch))
    }

    /// The commit number of the latest commit of the store at `path` (`StatStore`'s `committed`).
    pub fn committed(&self, path: &Path) -> Result<u64, crate::Error> {
        Ok(self.stat_store(path)?.committed)
    }

    /// Waits until every commit of the store at `path` is in the daemon's bucket (`SyncStore`).
    pub fn sync_store(&self, path: &Path) -> Result<(), crate::Error> {
        let request = SyncStoreRequest { path: name(path)? };
        let what = format!("waiting for the store at {} to be uploaded", path.display());
        self.call(what, |mut c| async move { c.sync_store(request).await }).map(drop)
    }

    /// Grants the tenant of the ID `tenant` in the bucket (TASK-364), whose stores are in `dir`,
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
    /// their stores (`Revoke`).
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
    pub fn shred_cell(&self, cell: crate::Cell) -> Result<(u32, Forgotten), crate::Error> {
        let request = ShredCellRequest { tenant: cell.tenant, cell: cell.id };
        let what = format!("shredding cell {cell}");
        let response = self.call(what, |mut c| async move { c.shred_cell(request).await })?;
        Ok((response.keks_removed, response.forgotten.unwrap_or_default()))
    }

    /// cladefs: has the daemon forget what it holds of the keys of the tenant `tenant`, whose KEK is
    /// revoked or destroyed in the KMS, as `why` says (`ForgetTenantKeys`, TASK-411.7): what it
    /// forgot.
    pub fn forget_tenant_keys(&self, tenant: u64, why: &str) -> Result<Forgotten, crate::Error> {
        let request = ForgetTenantKeysRequest { tenant, why: why.to_owned() };
        let what = format!("having the daemon forget tenant {tenant}'s keys");
        let response =
            self.call(what, |mut c| async move { c.forget_tenant_keys(request).await })?;
        Ok(response.forgotten.unwrap_or_default())
    }

    /// cladefs: shreds the tenant `tenant` under the daemon's bucket KMS, its KEK object replaced by
    /// a tombstone, and has the daemon forget its keys (`ShredTenant`, TASK-411.27): whether this
    /// call shredded it (not already), and what the daemon forgot.
    pub fn shred_tenant(&self, tenant: u64) -> Result<(bool, Forgotten), crate::Error> {
        let request = ShredTenantRequest { tenant };
        let what = format!("shredding tenant {tenant}");
        let response = self.call(what, |mut c| async move { c.shred_tenant(request).await })?;
        Ok((response.shredded_now, response.forgotten.unwrap_or_default()))
    }

    /// cladefs: what the disk of the tenant `tenant`, or of every tenant without one, takes, counted
    /// now, in bytes, and its limit, if it has one (`GetTenantDisk`, TASK-94).
    pub fn tenant_disk(&self, tenant: Option<u64>) -> Result<(u64, Option<u64>), crate::Error> {
        let request = GetTenantDiskRequest { tenant };
        let what = format!("counting the disk of {}", whom(tenant, "every tenant"));
        let response = self.call(what, |mut c| async move { c.get_tenant_disk(request).await })?;
        Ok((response.bytes, response.limit_bytes))
    }

    /// cladefs: what the daemon, its VM processes and their guests counted, as of one moment, each
    /// with its build (`GetStats`, TASK-111.4): the VM processes' answers waited for `wait` at
    /// most, the daemon's default if none.
    pub fn stats(
        &self,
        wait: Option<std::time::Duration>,
    ) -> Result<GetStatsResponse, crate::Error> {
        let wait_ms = wait.map_or(0, |wait| u32::try_from(wait.as_millis()).unwrap_or(u32::MAX));
        self.call("asking the extent daemon for its stats".to_owned(), |mut c| async move {
            c.get_stats(GetStatsRequest { wait_ms }).await
        })
    }

    /// The daemon's counters (`GetCounters`).
    pub fn counters(&self) -> Result<crate::stats::Stats, crate::Error> {
        let response = self
            .call("asking the extent daemon for its counters".to_owned(), |mut c| async move {
                c.get_counters(GetCountersRequest {}).await
            })?;
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
        })
    }

    /// Makes the call `call` makes with a client, and returns its response; or, failed, an error
    /// saying it was `what`, with the errno it failed with, if any.
    fn call<T, F>(
        &self,
        what: String,
        call: impl FnOnce(CladefsdClient<tonic::transport::Channel>) -> F,
    ) -> Result<T, crate::Error>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        match self.runtime.block_on(call(self.client.clone())) {
            Ok(response) => Ok(response.into_inner()),
            Err(status) => Err(crate::Error::new(match errno(&status) {
                Some(errno) => anyhow::Error::new(errno).context(what),
                None => anyhow::Error::new(status).context(what),
            })),
        }
    }
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
