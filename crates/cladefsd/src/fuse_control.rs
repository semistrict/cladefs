// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The control API of a FUSE mount of many cells (TASK-302): gRPC, whose calls and errors are
//! `proto/cladefs/mount/v1/mount.proto`'s, served on cladefsd's control socket (TASK-475.2). It
//! attaches tenants' cells of the mount's bucket (TASK-364), each of one kind (doc-20), an Fxfs
//! one taken into a cell in the mount's directory of cells, at `TENANT/CELL`, and shown at that
//! path in the mount; and detaches them.
//!
//! An attach or detach runs in a task of its own, which a caller giving up on it doesn't stop:
//! a cell is attached or detached whole, or not at all.

use crate::control::{MAX_MESSAGE_BYTES, saying};
use base32_id::Padded;
use bucket_cell::cell::Kind;
use cladefs_core::error::{Errno, Error};
use cladefs_core::{CellId, CellOf};
use cladefs_fs::fuse::server::Server;
use cladefs_fs::{Filesystem, SetAttr, Storage};
use proto::mount_client::MountClient;
use proto::*;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tonic::{Request, Response, Status};

/// The API's messages and services, generated from its .proto.
pub mod proto {
    pub use cladefs_proto::mount_v1::*;
}

/// Where a mount of many cells takes the cells it attaches from, and keeps their cells.
#[derive(Clone, Debug)]
pub struct CellSource {
    /// One or more JBOD root directories the cells live in, each at the path it is attached at.
    pub dirs: Vec<PathBuf>,
    /// The URL of the bucket the cells are in.
    pub bucket: String,
}

impl CellSource {
    /// Resolves the local cell directory path for `cell` (`shown` = `TENANT/CELL`): when the
    /// daemon has a managed `DiskPool`, reuses an existing directory (even on a draining disk) or
    /// delegates new placement to `DiskPool::pick_disk` (enforcing `ReadWrite`/cap/headroom policy).
    /// Without a `DiskPool` (a daemon booted of `CLADEFSD_MOUNT_STORES` alone), probes `self.dirs` and
    /// picking by filesystem free space.
    fn resolve_cell_path(
        &self,
        daemon: &crate::Daemon,
        cell: CellId,
        shown: &str,
    ) -> Result<PathBuf, Errno> {
        if let Some(managed) = daemon.resolve_or_pick_cell_path(cell)? {
            return Ok(managed);
        }
        if self.dirs.len() == 1 {
            return Ok(self.dirs[0].join(shown));
        }
        for dir in &self.dirs {
            let candidate = dir.join(shown);
            match host_fs::metadata(&candidate) {
                Ok(_) => return Ok(candidate),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    log::error!("cladefs: resolving mount store path for {shown}: {err}");
                    return Err(err.raw_os_error().map(Errno::from_raw).unwrap_or(Errno::EIO));
                }
            }
        }
        let best = self
            .dirs
            .iter()
            .max_by_key(|dir| {
                // Unsampled roots fall back to 0 so any healthy sampled root is preferred.
                bucket_cell::host_disk::HostDisk::of_on(daemon.host.bucket_host(), dir)
                    .ok()
                    .and_then(|disk| disk.sample().ok())
                    .map_or(0, |sample| sample.free)
            })
            .unwrap_or(&self.dirs[0]);
        Ok(best.join(shown))
    }
}

/// The Mount service of `server`, a server of many cells, which attaches cells as `cells` says:
/// what `daemon` serves on its control socket, beside its other services, if it mounts cells
/// (`crate::mount`, TASK-475.2).
#[cfg(not(madsim))]
pub(crate) fn service_on(
    daemon: Arc<crate::Daemon>,
    server: &Arc<Server>,
    source: CellSource,
) -> mount_server::MountServer<impl mount_server::Mount + use<>> {
    let (server, source) = (Arc::downgrade(server), Arc::new(source));
    served(Control { daemon, server, source, cells: Arc::default(), holds: Arc::default() })
}

/// The Mount service of `mount`, which encodes answers as large as the control API's at most
/// ([`MAX_MESSAGE_BYTES`], board 10297): where tonic's default, 4 MiB, refused a `List` of some
/// hundreds of thousands of cells attached, at the client.
fn served<M: mount_server::Mount>(mount: M) -> mount_server::MountServer<M> {
    mount_server::MountServer::new(mount).max_encoding_message_size(MAX_MESSAGE_BYTES)
}

/// A client of the Mount service over `channel`, the daemon's control socket's
/// (`crate::control::channel`), which takes answers as large as the daemon sends
/// ([`MAX_MESSAGE_BYTES`], board 10297), as the control API's client does.
pub fn client(channel: tonic::transport::Channel) -> MountClient<tonic::transport::Channel> {
    MountClient::new(channel).max_decoding_message_size(MAX_MESSAGE_BYTES)
}

/// `List`'s answer, of the cells `cells`, unless it is past [`MAX_MESSAGE_BYTES`] encoded: refused
/// then, RESOURCE_EXHAUSTED (EMSGSIZE), saying its size, as the control API refuses an answer past
/// it (board 8581, board 10297), rather than one its client fails to decode.
fn listed(cells: Vec<AttachedCell>) -> Result<Response<ListResponse>, Status> {
    let answer = ListResponse { cells };
    let bytes = prost::Message::encoded_len(&answer);
    if bytes <= MAX_MESSAGE_BYTES {
        return Ok(Response::new(answer));
    }
    log::warn!("fuse: refusing List's answer of {bytes} bytes, past {MAX_MESSAGE_BYTES}");
    Err(saying(
        Errno::EMSGSIZE,
        format!(
            "List's answer would be {bytes} bytes, past the most the control API sends, \
             {MAX_MESSAGE_BYTES}: {} cells attached",
            answer.cells.len()
        ),
    ))
}

/// The path `cell`'s filesystem has in the mount, and in its directory of cells: `TENANT/CELL`,
/// the tenant and the cell in base 32, padded, as the bucket's keys have them
/// (`base32_id::Padded`).
pub fn cell_path(cell: CellId) -> String {
    format!("{}/{}", Padded(cell.tenant), Padded(cell.id))
}

/// The cells attached, or being attached or detached, and the fencing token each was taken at.
type Cells = Arc<Mutex<BTreeMap<CellId, Attaching>>>;

/// A cell of the mount's.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Attaching {
    /// Being attached.
    Busy,
    /// Attached, taken at the placement of this token, if one was given, with the local cell
    /// directory held for the mount (in `CLADEFSD_MOUNT_STORES` for bucket cells, or the cell's
    /// own directory for ephemeral cells).
    Attached(Option<u64>, PathBuf),
    /// Being detached: listed, its filesystem as it goes.
    Detaching(PathBuf),
}

/// The API's calls, served by a server of many cells: the mount's, which the daemon's drain lets
/// go of as it closes the mount, and its cells with it, which a call holding the server's last
/// reference would keep open, their uploaders running, past the drain (TASK-475.2). Gone, calls
/// fail with `UNAVAILABLE`.
struct Control {
    daemon: Arc<crate::Daemon>,
    server: Weak<Server>,
    source: Arc<CellSource>,
    cells: Cells,
    holds: Holds,
}

/// cladefs: the daemon's holds of the cells attached, by their paths in the directory of cells
/// (TASK-475.3): while the mount has a cell, a VM process opening it, or the daemon's control API
/// working on it, fails with `EBUSY` at once. Taken before a cell opens, let go once it is
/// detached, written back and closed, or failed to attach.
type Holds = Arc<Mutex<BTreeMap<PathBuf, crate::cells::MountHold>>>;

/// Opens the cell directory at `path` as `open`, [`mount_cell`]'s, does, held for the mount first
/// ([`Holds`]), the hold awaited, holding no thread (board 2946), and kept in `holds` if it opens.
async fn held_open(
    daemon: Arc<crate::Daemon>,
    holds: Holds,
    path: PathBuf,
    open: impl Future<Output = Result<Filesystem, Error>>,
) -> Result<Filesystem, Error> {
    let hold = crate::cells::hold_mounted(&daemon, &path).await.map_err(|errno| {
        Error::new(
            anyhow::Error::new(errno).context(format!("holding {} for the mount", path.display())),
        )
    })?;
    let fs = open.await?;
    holds.lock().unwrap().insert(path, hold);
    Ok(fs)
}

/// The cell `cell` of `cells` as `reserving` finds it: marked as `reserving` says, else left be.
fn reserve<R>(
    cells: &Cells,
    cell: CellId,
    reserving: impl FnOnce(Option<&Attaching>) -> Result<(Attaching, R), Errno>,
) -> Result<R, Errno> {
    let mut cells = cells.lock().unwrap();
    let (marked, result) = reserving(cells.get(&cell))?;
    cells.insert(cell, marked);
    Ok(result)
}

/// Mounts an existing ephemeral Fxfs cell directory at `path` on `daemon`, with no uploader:
/// reads inherited extents through `daemon`'s part cache and keeps writes host-local.
async fn mount_ephemeral_cell(
    daemon: &Arc<crate::Daemon>,
    path: &Path,
) -> Result<Filesystem, Error> {
    let keeping = crate::cells::mount_keeping(daemon).map_err(Error::from)?;
    let storage = Storage::open_on_keeping(&daemon.host, path, keeping).await?;
    let mut options = cladefs_fs::Options::new();
    #[cfg(not(madsim))]
    options.runtime(daemon.runtime().handle().clone());
    options.mount(storage).await.map_err(|err| err.context(format!("mounting {}", path.display())))
}

/// Takes a cell's filesystem, the cell `of` says, into a cell directory at `path` in the bucket at
/// `url`, of `capacity` bytes until its cell says its own, placed at `token` if given, and mounts
/// it: made empty if new, its root the mounting user's, as the mount is. cladefs: awaited, as the
/// daemon's control API serves it, holding no thread: its disk work
/// [`local_cell::upload::blocking`]'s, its bucket's on the bucket runtime ([`Storage::take`]),
/// Fxfs's device I/O the I/O engine's.
async fn mount_cell(
    daemon: &Arc<crate::Daemon>,
    path: &Path,
    url: &str,
    (of, capacity): (CellOf, u64),
    token: Option<u64>,
    upload_interval: Option<std::time::Duration>,
) -> Result<Filesystem, Error> {
    let new = matches!(of, CellOf::New(_) | CellOf::Named(_));
    // Its tenant's directory, made once for all its cells.
    if let Some(tenant) = path.parent() {
        local_cell::upload::blocking(|| std::fs::create_dir_all(tenant))
            .map_err(|err| Error::new(err).context(format!("making {}", tenant.display())))?;
    }
    let mut keeping = crate::cells::mount_keeping(daemon).map_err(Error::from)?;
    if let Some(interval) = upload_interval {
        keeping.interval = interval;
    }
    let storage =
        Storage::take_on_keeping(&daemon.host, path, (of, capacity), url, token, keeping).await?;
    let mounting = async {
        let mut options = cladefs_fs::Options::new();
        #[cfg(not(madsim))]
        options.runtime(daemon.runtime().handle().clone());
        if !new {
            return options.mount(storage).await;
        }
        let fs = options.format(storage).await?;
        // SAFETY: Plain calls returning the process's IDs.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let owner = SetAttr { uid: Some(uid), gid: Some(gid), ..SetAttr::default() };
        if let Err(err) = fs.root().setattr(&owner).await {
            // Return the original setattr error; unmount asynchronously rather than hitting Drop.
            let _ = fs.unmount().await;
            return Err(err.into());
        }
        Ok(fs)
    };
    mounting.await.map_err(|err| err.context(format!("mounting {}", path.display())))
}

/// The capacity a cell taking a cell's filesystem is made with, until its cell says its own,
/// which every filesystem made here does as it is made: a block.
const TAKEN_CAPACITY: u64 = Storage::BLOCK_SIZE as u64;

impl Control {
    /// The mount's server, unless the mount is closed.
    fn server(&self) -> Result<Arc<Server>, Status> {
        self.server.upgrade().ok_or_else(|| Status::unavailable("the mount of cells is closed"))
    }

    /// Attaches `cell`'s filesystem at its path, as [`mount_cell`] takes and mounts it, placed at
    /// `token` if given: a new one of `capacity` bytes if `new`, else the one its head, of `kind`,
    /// names, if an Fxfs one. Returns whether it shows it and the local cell directory held.
    async fn attach_cell(
        &self,
        cell: CellId,
        kind: Option<Kind>,
        (new, capacity): (bool, u64),
        token: Option<u64>,
        upload_interval: Option<std::time::Duration>,
    ) -> Result<(bool, PathBuf), Error> {
        let Some(server) = self.server.upgrade() else {
            return Err(Errno::ENOTCONN.into());
        };
        let shown = cell_path(cell);
        let path = local_cell::upload::blocking(|| {
            self.source.resolve_cell_path(&self.daemon, cell, &shown)
        })?;
        let of = match (new, kind) {
            (true, _) => (CellOf::Named(cell), capacity),
            (false, Some(Kind::Fxfs)) => (CellOf::Taken(cell), TAKEN_CAPACITY),
            (false, _) => return Ok((false, path)),
        };
        let mounting =
            mount_cell(&self.daemon, &path, &self.source.bucket, of, token, upload_interval);
        let open = held_open(Arc::clone(&self.daemon), self.holds.clone(), path.clone(), mounting);
        if let Err(err) = server.attach(&shown, open).await {
            self.holds.lock().unwrap().remove(&path);
            return Err(err);
        }
        self.daemon.register_cell_path(cell, path.clone());
        Ok((true, path))
    }

    /// Attaches an existing host-local ephemeral `cell` at `path` (of `kind`) to the FUSE mount,
    /// without touching the bucket's head or starting an uploader.
    async fn attach_ephemeral_cell(
        &self,
        cell: CellId,
        path: PathBuf,
        kind: Option<Kind>,
    ) -> Result<(bool, PathBuf), Error> {
        let Some(server) = self.server.upgrade() else {
            return Err(Errno::ENOTCONN.into());
        };
        if kind != Some(Kind::Fxfs) {
            return Ok((false, path));
        }
        let shown = cell_path(cell);
        let mounting = mount_ephemeral_cell(&self.daemon, &path);
        let open = held_open(Arc::clone(&self.daemon), self.holds.clone(), path.clone(), mounting);
        if let Err(err) = server.attach(&shown, open).await {
            self.holds.lock().unwrap().remove(&path);
            return Err(err);
        }
        Ok((true, path))
    }
}

#[tonic::async_trait]
impl proto::mount_server::Mount for Control {
    async fn attach(
        &self,
        request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        let AttachRequest { tenant, cell, token, capacity, upload_interval_ms } =
            request.into_inner();
        let upload_interval = crate::cells::parse_upload_interval_ms(upload_interval_ms)
            .map_err(|errno| failed(errno.into()))?;
        // A cell of 0 is a new one, made as it is held, its filesystem with it; any other is an
        // ephemeral cell on this host, or a bucket cell, or none.
        let make = cell == 0;
        if make && capacity == 0 {
            return Err(failed(Errno::EINVAL.into()));
        }
        // Fail fast before allocating a bucket cell ID if managed disks refuse new cells.
        if make {
            self.daemon.check_disk_placement().map_err(|errno| failed(errno.into()))?;
        }
        // A new cell's ID is the bucket's to give, its cell's too (TASK-364).
        let cell = match cell {
            0 => {
                let made = cladefs_core::host_cell::new_cell_at(&self.source.bucket, tenant).await;
                made.map_err(Error::new).map_err(failed)?
            }
            id => CellId { tenant, id },
        };
        reserve(&self.cells, cell, |found| match found {
            None => Ok((Attaching::Busy, ())),
            Some(Attaching::Attached(..)) => Err(Errno::EEXIST),
            Some(Attaching::Busy | Attaching::Detaching(_)) => Err(Errno::EBUSY),
        })
        .map_err(|errno| failed(errno.into()))?;
        let control = Control {
            daemon: Arc::clone(&self.daemon),
            server: self.server.clone(),
            source: self.source.clone(),
            cells: self.cells.clone(),
            holds: self.holds.clone(),
        };
        let attached = whole(async move {
            let attached = async {
                if !make && let Some((path, kind)) = ephemeral_cell(&control.daemon, cell).await? {
                    return control.attach_ephemeral_cell(cell, path, kind).await;
                }
                // A cell of the bucket's, its head read for its kind: its cell's opening takes it,
                // as a new one's makes it, encrypted (TASK-411.28, TASK-515).
                let kind = match make {
                    true => None,
                    false => cell_kind(&control.source.bucket, cell).await?,
                };
                control.attach_cell(cell, kind, (make, capacity), token, upload_interval).await
            }
            .await;
            let mut cells = control.cells.lock().unwrap();
            match attached {
                Ok((shown, store_path)) => {
                    cells.insert(cell, Attaching::Attached(token, store_path));
                    Ok(shown)
                }
                // Not attached: forgotten here; the error is the caller's.
                Err(err) => {
                    cells.remove(&cell);
                    Err(err)
                }
            }
        });
        let shown = attached.await?;
        Ok(Response::new(AttachResponse { cell: cell.id, shown }))
    }

    async fn detach(
        &self,
        request: Request<DetachRequest>,
    ) -> Result<Response<DetachResponse>, Status> {
        let DetachRequest { tenant, cell, force } = request.into_inner();
        let cell = CellId { tenant, id: cell };
        let store_path = reserve(&self.cells, cell, |found| match found {
            Some(Attaching::Attached(_, path)) => {
                Ok((Attaching::Detaching(path.clone()), path.clone()))
            }
            Some(Attaching::Busy | Attaching::Detaching(_)) => Err(Errno::EBUSY),
            None => Err(Errno::ENOENT),
        })
        .map_err(|errno| failed(errno.into()))?;
        let (server, cells, holds) = (self.server()?, self.cells.clone(), self.holds.clone());
        let path = cell_path(cell);
        let detached = whole(async move {
            let shown: Vec<String> = (server.attached().into_iter())
                .map(|attached| attached.name)
                .filter(|name| *name == path)
                .collect();
            let detaching = shown.iter().map(|name| server.detach(name, force));
            let detached = futures::future::join_all(detaching).await;
            // Their cells closed, or gone from the mount all the same: the daemon's again.
            holds.lock().unwrap().remove(&store_path);
            // A cell that failed to unmount is gone from the mount all the same
            // (`Server::detach`): the cell is no longer attached, and attaching it again opens its
            // cell anew.
            cells.lock().unwrap().remove(&cell);
            match detached.into_iter().find_map(Result::err) {
                Some(err) => Err(err),
                None => Ok(()),
            }
        });
        detached.await?;
        Ok(Response::new(DetachResponse {}))
    }

    async fn list(&self, _: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let attached: Vec<CellId> = (self.cells.lock().unwrap().iter())
            .filter(|(_, attaching)| !matches!(attaching, Attaching::Busy))
            .map(|(cell, _)| *cell)
            .collect();
        let shown = self.server()?.attached();
        let cells = (attached.into_iter())
            .map(|cell| {
                let path = cell_path(cell);
                let shown = shown.iter().find(|attached| attached.name == path);
                let detaching = shown.is_some_and(|attached| attached.detaching);
                AttachedCell {
                    tenant: cell.tenant,
                    cell: cell.id,
                    shown: shown.is_some(),
                    detaching,
                }
            })
            .collect();
        listed(cells)
    }
}

/// Checks whether `cell` is an ephemeral cell recorded in `daemon`'s `CLADEFSD_EPHEMERAL` journal.
/// Returns `Ok(None)` when `CLADEFSD_EPHEMERAL` is unset or `cell` has no entry (so the caller
/// falls through to the bucket), `EBUSY` if it is migrating or handed over, and `ENOENT` if the
/// journaled cell directory is no longer on disk.
async fn ephemeral_cell(
    daemon: &Arc<crate::Daemon>,
    cell: CellId,
) -> Result<Option<(PathBuf, Option<Kind>)>, Error> {
    let host = match cladefs_core::ephemeral_host::host_on(&daemon.host) {
        Ok(host) => host,
        Err(Errno::ENOTSUP) => return Ok(None),
        Err(errno) => return Err(errno.into()),
    };
    local_cell::upload::blocking(move || -> Result<Option<(PathBuf, Option<Kind>)>, Error> {
        let Some(entry) = host.entry(cell).map_err(Error::new)? else {
            return Ok(None);
        };
        if entry.migrating.is_some() {
            return Err(Errno::EBUSY.into());
        }
        if !host_fs::exists(&entry.path) || !cladefs_core::host_cell::is_ephemeral(&entry.path) {
            return Err(Errno::ENOENT.into());
        }
        let kind = local_cell::host_kind(&entry.path).map_err(Error::new)?;
        Ok(Some((entry.path, kind)))
    })
}

/// The kind of `cell`, a cell of the bucket at `url`, if its head says one: `ENOENT` if it has no
/// head.
async fn cell_kind(url: &str, cell: CellId) -> Result<Option<Kind>, Error> {
    let bucket = cladefs_core::host_cell::bucket(url).map_err(Error::new)?;
    let found = bucket_cell::head::get(&*bucket, cell).await;
    let head = found.and_then(|found| found.ok_or(bucket_cell::Error::NoCell)).map_err(|err| {
        let errno = cell_errno(&err);
        Error::new(anyhow::Error::new(err).context(errno))
    })?;
    Ok(head.kind)
}

/// The errno a failure to hold a cell says, as the extent daemon's API has it: `ENOENT` for a cell
/// the bucket has none of, `ESTALE` for one a later placement holds.
fn cell_errno(err: &bucket_cell::Error) -> Errno {
    let failure = local_cell::upload::Failure::Bucket(err.clone());
    cladefs_core::bucket::errno(&failure).unwrap_or(Errno::EIO)
}

/// Runs `work`, and returns its result as the API's. cladefs: whole, though the caller gives up on
/// it, as the daemon serves every call of this service in a task of its own (`cladefsd::control`'s
/// `Drained`, board 5599).
async fn whole<T>(work: impl Future<Output = Result<T, Error>>) -> Result<T, Status> {
    work.await.map_err(failed)
}

/// The status of a call that failed with `err`: its errno's, `EIO` if it has none, saying what
/// failed.
fn failed(err: Error) -> Status {
    let errno = err.errno().unwrap_or(Errno::EIO);
    if errno == Errno::EIO {
        log::error!("fuse: {err:#}");
    }
    saying(errno, format!("{err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Mount service listing `cells` cells, each of the largest IDs, as [`Control::list`] answers
    /// ([`listed`]): attaching and detaching nothing.
    struct Listing {
        cells: usize,
    }

    #[tonic::async_trait]
    impl mount_server::Mount for Listing {
        async fn attach(
            &self,
            _: Request<AttachRequest>,
        ) -> Result<Response<AttachResponse>, Status> {
            Err(Status::unimplemented("a listing"))
        }

        async fn detach(
            &self,
            _: Request<DetachRequest>,
        ) -> Result<Response<DetachResponse>, Status> {
            Err(Status::unimplemented("a listing"))
        }

        async fn list(&self, _: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
            let cell =
                AttachedCell { tenant: u64::MAX, cell: u64::MAX, shown: true, detaching: false };
            listed(vec![cell; self.cells])
        }
    }

    /// Board 10297: a listing past gRPC's default 4 MiB, of 200,000 cells, is served and taken
    /// through [`client`], as the control API's answers are, up to [`MAX_MESSAGE_BYTES`]; past it,
    /// of 2,600,000, refused by the service, RESOURCE_EXHAUSTED (EMSGSIZE), saying its size. Until
    /// then the client failed the first ("decoded message length too large"), and the service
    /// sent the second, which no client took.
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_past_grpcs_default_are_taken_up_to_the_control_apis_most() {
        let dir = tempfile::tempdir().unwrap();
        let list = async |cells: usize| {
            let socket = dir.path().join(format!("{cells}.sock"));
            let incoming = tokio_stream::wrappers::UnixListenerStream::new(
                tokio::net::UnixListener::bind(&socket).unwrap(),
            );
            let serving = tonic::transport::Server::builder()
                .add_service(served(Listing { cells }))
                .serve_with_incoming(incoming);
            let serving = tokio::spawn(serving);
            let channel = crate::control::channel(&socket).await.unwrap();
            let listed = client(channel).list(ListRequest {}).await;
            serving.abort();
            listed.map(|listed| listed.into_inner().cells.len()).map_err(|status| {
                let failed = crate::control::failed("listing".to_owned(), status);
                (failed.errno(), format!("{failed:#}"))
            })
        };
        // Each cell 26 bytes: 5,200,000 bytes, then 67,600,000.
        assert_eq!(list(200_000).await, Ok(200_000));
        let (errno, said) = list(2_600_000).await.unwrap_err();
        assert_eq!(errno, Some(Errno::EMSGSIZE), "{said}");
        assert!(said.contains("List's answer would be 67600000 bytes, past the most"), "{said}");
    }
}
