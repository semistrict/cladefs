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

use super::server::Server;
use crate::cladefsd::control::{errno, saying};
use crate::error::{Errno, Error};
use crate::{CellId, CellOf, Filesystem, SetAttr, Storage};
use base32_id::Padded;
use bucket_cell::cell::Kind;
use proto::mount_client::MountClient;
use proto::*;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tonic::{Request, Response, Status};

/// The API's messages and services, generated from its .proto.
pub mod proto {
    tonic::include_proto!("cladefs.mount.v1");
}

/// Where a mount of many cells takes the cells it attaches from, and keeps their cells.
#[derive(Clone, Debug)]
pub struct CellSource {
    /// The directory the cells are in, each at the path it is attached at.
    pub dir: PathBuf,
    /// The URL of the bucket the cells are in.
    pub bucket: String,
}

/// The Mount service of `server`, a server of many cells, which attaches cells as `cells` says:
/// what the daemon serves on its control socket, beside its other services, if it mounts cells
/// (`crate::cladefsd::mount`, TASK-475.2).
#[cfg(not(madsim))]
pub(crate) fn service(
    server: &Arc<Server>,
    source: CellSource,
) -> mount_server::MountServer<impl mount_server::Mount + use<>> {
    let (server, source) = (Arc::downgrade(server), Arc::new(source));
    mount_server::MountServer::new(Control {
        server,
        source,
        cells: Arc::default(),
        holds: Arc::default(),
    })
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attaching {
    /// Being attached.
    Busy,
    /// Attached, taken at the placement of this token, if one was given.
    Attached(Option<u64>),
    /// Being detached: listed, its filesystem as it goes.
    Detaching,
}

/// The API's calls, served by a server of many cells: the mount's, which the daemon's drain lets
/// go of as it closes the mount, and its cells with it, which a call holding the server's last
/// reference would keep open, their uploaders running, past the drain (TASK-475.2). Gone, calls
/// fail with `UNAVAILABLE`.
struct Control {
    server: Weak<Server>,
    source: Arc<CellSource>,
    cells: Cells,
    holds: Holds,
}

/// cladefs: the daemon's holds of the cells attached, by their paths in the directory of cells
/// (TASK-475.3): while the mount has a cell, a VM process opening it, or the daemon's control API
/// working on it, fails with `EBUSY` at once. Taken before a cell opens, let go once it is
/// detached, written back and closed, or failed to attach.
type Holds = Arc<Mutex<BTreeMap<PathBuf, crate::cladefsd::cells::MountHold>>>;

/// Opens the cell directory at `path` as `open`, [`mount_cell`]'s, does, held for the mount first
/// ([`Holds`]), the hold awaited, holding no thread (board 2946), and kept in `holds` if it opens.
async fn held_open(
    holds: Holds,
    path: PathBuf,
    open: impl Future<Output = Result<Filesystem, Error>>,
) -> Result<Filesystem, Error> {
    let hold = crate::cladefsd::cells::hold_mounted(&path).await.map_err(|errno| {
        Error::new(
            anyhow::Error::new(errno).context(format!("holding {} for the mount", path.display())),
        )
    })?;
    let fs = open.await?;
    holds.lock().unwrap().insert(path, hold);
    Ok(fs)
}

/// The cell `cell` of `cells` as `reserving` finds it: marked `marked` if it says so, else left be.
fn reserve(
    cells: &Cells,
    (cell, marked): (CellId, Attaching),
    reserving: impl FnOnce(Option<Attaching>) -> Result<(), Errno>,
) -> Result<(), Errno> {
    let mut cells = cells.lock().unwrap();
    reserving(cells.get(&cell).copied())?;
    cells.insert(cell, marked);
    Ok(())
}

/// Takes a cell's filesystem, the cell `of` says, into a cell directory at `path` in the bucket at
/// `url`, of `capacity` bytes until its cell says its own, placed at `token` if given, and mounts
/// it: made empty if new, its root the mounting user's, as the mount is. cladefs: awaited, as the
/// daemon's control API serves it, holding no thread: its disk work
/// [`local_cell::upload::blocking`]'s, its bucket's on the bucket runtime ([`Storage::take`]),
/// Fxfs's device I/O the I/O engine's.
async fn mount_cell(
    path: &Path,
    url: &str,
    (of, capacity): (CellOf, u64),
    token: Option<u64>,
) -> Result<Filesystem, Error> {
    let new = matches!(of, CellOf::New(_) | CellOf::Named(_));
    // Its tenant's directory, made once for all its cells.
    if let Some(tenant) = path.parent() {
        local_cell::upload::blocking(|| std::fs::create_dir_all(tenant))
            .map_err(|err| Error::new(err).context(format!("making {}", tenant.display())))?;
    }
    let storage = Storage::take(path, (of, capacity), url, token).await?;
    let mounting = async {
        if !new {
            return Filesystem::mount(storage).await;
        }
        let fs = Filesystem::format(storage).await?;
        // SAFETY: Plain calls returning the process's IDs.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let owner = SetAttr { uid: Some(uid), gid: Some(gid), ..SetAttr::default() };
        fs.root().setattr(&owner).await?;
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
    /// names, if an Fxfs one. Returns whether it shows it.
    async fn attach_cell(
        &self,
        cell: CellId,
        kind: Option<Kind>,
        (new, capacity): (bool, u64),
        token: Option<u64>,
    ) -> Result<bool, Error> {
        let Some(server) = self.server.upgrade() else {
            return Err(Errno::ENOTCONN.into());
        };
        let of = match (new, kind) {
            (true, _) => (CellOf::Named(cell), capacity),
            (false, Some(Kind::Fxfs)) => (CellOf::Taken(cell), TAKEN_CAPACITY),
            (false, _) => return Ok(false),
        };
        let shown = cell_path(cell);
        let path = self.source.dir.join(&shown);
        let mounting = mount_cell(&path, &self.source.bucket, of, token);
        let open = held_open(self.holds.clone(), path.clone(), mounting);
        if let Err(err) = server.attach(&shown, open).await {
            self.holds.lock().unwrap().remove(&path);
            return Err(err);
        }
        Ok(true)
    }
}

#[tonic::async_trait]
impl proto::mount_server::Mount for Control {
    async fn attach(
        &self,
        request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        let AttachRequest { tenant, cell, token, capacity } = request.into_inner();
        // A cell of 0 is a new one, made as it is held, its filesystem with it; any other is the
        // bucket's, or none.
        let make = cell == 0;
        if make && capacity == 0 {
            return Err(failed(Errno::EINVAL.into()));
        }
        // A new cell's ID is the bucket's to give, its cell's too (TASK-364).
        let cell = match cell {
            0 => {
                let made = crate::host_cell::new_cell_at(&self.source.bucket, tenant).await;
                made.map_err(Error::new).map_err(failed)?
            }
            id => CellId { tenant, id },
        };
        reserve(&self.cells, (cell, Attaching::Busy), |found| match found {
            None => Ok(()),
            Some(Attaching::Attached(_)) => Err(Errno::EEXIST),
            Some(Attaching::Busy | Attaching::Detaching) => Err(Errno::EBUSY),
        })
        .map_err(|errno| failed(errno.into()))?;
        let control = Control {
            server: self.server.clone(),
            source: self.source.clone(),
            cells: self.cells.clone(),
            holds: self.holds.clone(),
        };
        let attached = whole(async move {
            let attached = async {
                // A cell of the bucket's, its head read for its kind: its cell's opening takes it,
                // as a new one's makes it, encrypted (TASK-411.28, TASK-515).
                let kind = match make {
                    true => None,
                    false => cell_kind(&control.source.bucket, cell).await?,
                };
                control.attach_cell(cell, kind, (make, capacity), token).await
            }
            .await;
            let mut cells = control.cells.lock().unwrap();
            match &attached {
                Ok(_) => cells.insert(cell, Attaching::Attached(token)),
                Err(_) => cells.remove(&cell),
            };
            attached
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
        reserve(&self.cells, (cell, Attaching::Detaching), |found| match found {
            Some(Attaching::Attached(_)) => Ok(()),
            Some(Attaching::Busy | Attaching::Detaching) => Err(Errno::EBUSY),
            None => Err(Errno::ENOENT),
        })
        .map_err(|errno| failed(errno.into()))?;
        let (server, cells, holds) = (self.server()?, self.cells.clone(), self.holds.clone());
        let dir = self.source.dir.clone();
        let path = cell_path(cell);
        let detached = whole(async move {
            let shown: Vec<String> = (server.attached().into_iter())
                .map(|attached| attached.name)
                .filter(|name| *name == path)
                .collect();
            let detaching = shown.iter().map(|name| server.detach(name, force));
            let detached = futures::future::join_all(detaching).await;
            // Their cells closed, or gone from the mount all the same: the daemon's again.
            let mut held = holds.lock().unwrap();
            for name in &shown {
                held.remove(&dir.join(name));
            }
            drop(held);
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
        Ok(Response::new(ListResponse { cells }))
    }
}

/// The kind of `cell`, a cell of the bucket at `url`, if its head says one: `ENOENT` if it has no
/// head.
async fn cell_kind(url: &str, cell: CellId) -> Result<Option<Kind>, Error> {
    let bucket = crate::host_cell::bucket(url).map_err(Error::new)?;
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
    crate::bucket::errno(&failure).unwrap_or(Errno::EIO)
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

/// A client of the control API of a FUSE mount of many cells that blocks, for callers without a
/// runtime of their own: `cladefsctl mount`'s commands, and tests. Its errors carry the errno a
/// call failed with, if it did ([`crate::Error::errno`]).
pub struct Client {
    runtime: tokio::runtime::Runtime,
    client: MountClient<tonic::transport::Channel>,
}

impl Client {
    /// A client of the mount of the daemon whose control socket is at `socket`.
    pub fn connect(socket: impl AsRef<Path>) -> Result<Self, Error> {
        let socket = socket.as_ref();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(Error::new)?;
        let channel =
            runtime.block_on(crate::cladefsd::control::channel_socket(socket)).map_err(|err| {
                Error::new(anyhow::Error::new(err).context(format!(
                    "connecting to the daemon's control socket at {}",
                    socket.display()
                )))
            })?;
        Ok(Self { runtime, client: MountClient::new(channel) })
    }

    /// Attaches `cell`, or a new cell of its tenant if its ID is 0, its filesystem a new Fxfs one
    /// of `capacity` bytes, its lease taken at `token`'s placement if given, and returns it and the
    /// (`Attach`).
    pub fn attach(&self, cell: CellId, capacity: u64, token: Option<u64>) -> Result<CellId, Error> {
        let request = AttachRequest { tenant: cell.tenant, cell: cell.id, token, capacity };
        let response = self.call(format!("attaching cell {cell}"), |mut client| async move {
            client.attach(request).await
        })?;
        Ok(CellId { id: response.cell, ..cell })
    }

    /// Detaches `cell`, at once if `force` (`Detach`).
    pub fn detach(&self, cell: CellId, force: bool) -> Result<(), Error> {
        let request = DetachRequest { tenant: cell.tenant, cell: cell.id, force };
        self.call(format!("detaching cell {cell}"), |mut client| async move {
            client.detach(request).await
        })?;
        Ok(())
    }

    /// The cells attached (`List`).
    pub fn list(&self) -> Result<Vec<proto::AttachedCell>, Error> {
        let response = self.call("listing the cells".to_owned(), |mut client| async move {
            client.list(ListRequest {}).await
        })?;
        Ok(response.cells)
    }

    fn call<T, F>(
        &self,
        what: String,
        call: impl FnOnce(MountClient<tonic::transport::Channel>) -> F,
    ) -> Result<T, Error>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        match self.runtime.block_on(call(self.client.clone())) {
            Ok(response) => Ok(response.into_inner()),
            Err(status) => Err(Error::new(match errno(&status) {
                Some(errno) => anyhow::Error::new(status).context(errno).context(what),
                None => anyhow::Error::new(status).context(what),
            })),
        }
    }
}
