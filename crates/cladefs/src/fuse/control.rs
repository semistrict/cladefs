// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The control API of a FUSE mount of many stores (TASK-302): gRPC, whose calls and errors are
//! `proto/cladefs/mount/v1/mount.proto`'s, served on cladefsd's control socket (TASK-475.2). It
//! attaches tenants' cells of the mount's bucket (TASK-364), each one filesystem (doc-20), an Fxfs
//! one taken into a store in the mount's directory of stores, at `TENANT/CELL`, and shown at that
//! path in the mount; and detaches them.
//!
//! An attach or detach runs in a task of its own, which a caller giving up on it doesn't stop:
//! a cell is attached or detached whole, or not at all.

use super::server::Server;
use crate::cladefsd::control::{channel, errno, saying};
use crate::error::{Errno, Error};
use crate::{Branch, BranchOf, Cell, Filesystem, SetAttr, Storage};
use base32_id::Padded;
use extent_bucket::cell::{self, Kind};
use extent_bucket::keys;
use proto::mount_client::MountClient;
use proto::mount_server::MountServer;
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

/// Where a mount of many stores takes the cells it attaches from, and keeps their stores.
#[derive(Clone, Debug)]
pub struct Stores {
    /// The directory the stores are in, each at the path it is attached at.
    pub dir: PathBuf,
    /// The URL of the bucket the cells are in.
    pub bucket: String,
}

/// The Mount service of `server`, a server of many stores, which attaches cells as `stores` says:
/// what the daemon serves on its control socket, beside its other services, if it mounts cells
/// (`crate::cladefsd::mount`, TASK-475.2).
pub(crate) fn service(
    server: &Arc<Server>,
    stores: Stores,
) -> MountServer<impl proto::mount_server::Mount + use<>> {
    let (server, stores) = (Arc::downgrade(server), Arc::new(stores));
    MountServer::new(Control { server, stores, cells: Arc::default(), holds: Arc::default() })
}

/// The path `cell`'s filesystem has in the mount, and in its directory of stores: `TENANT/CELL`,
/// the tenant and the cell in base 32, padded, as the bucket's keys have them
/// (`base32_id::Padded`).
pub fn cell_path(cell: Cell) -> String {
    format!("{}/{}", Padded(cell.tenant), Padded(cell.id))
}

/// The cells attached, or being attached or detached, and the fencing token each was taken at.
type Cells = Arc<Mutex<BTreeMap<Cell, Attaching>>>;

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

/// The API's calls, served by a server of many stores: the mount's, which the daemon's drain lets
/// go of as it closes the mount, and its stores with it, which a call holding the server's last
/// reference would keep open, their uploaders running, past the drain (TASK-475.2). Gone, calls
/// fail with `UNAVAILABLE`.
struct Control {
    server: Weak<Server>,
    stores: Arc<Stores>,
    cells: Cells,
    holds: Holds,
}

/// cladefs: the daemon's holds of the stores attached, by their paths in the directory of stores
/// (TASK-475.3): while the mount has a store, a VM process opening it, or the daemon's control API
/// working on it, fails with `EBUSY` at once. Taken before a store opens, let go once it is
/// detached, written back and closed, or failed to attach.
type Holds = Arc<Mutex<BTreeMap<PathBuf, crate::cladefsd::stores::MountHold>>>;

/// Opens the store at `path` as [`mount_cell`] does, held for the mount first ([`Holds`]): `open`
/// opens it, given the hold, and keeps the hold in `holds` if it opens.
fn held_open(
    holds: &Holds,
    path: &Path,
    open: impl FnOnce() -> Result<Filesystem, Error>,
) -> Result<Filesystem, Error> {
    let hold = crate::cladefsd::stores::hold_mounted(path).map_err(|errno| {
        Error::new(
            anyhow::Error::new(errno).context(format!("holding {} for the mount", path.display())),
        )
    })?;
    let fs = open()?;
    holds.lock().unwrap().insert(path.to_owned(), hold);
    Ok(fs)
}

/// The cell `cell` of `cells` as `reserving` finds it: marked `marked` if it says so, else left be.
fn reserve(
    cells: &Cells,
    (cell, marked): (Cell, Attaching),
    reserving: impl FnOnce(Option<Attaching>) -> Result<(), Errno>,
) -> Result<(), Errno> {
    let mut cells = cells.lock().unwrap();
    reserving(cells.get(&cell).copied())?;
    cells.insert(cell, marked);
    Ok(())
}

/// Takes a cell's filesystem, the branch `of` says, into a store at `path` in the bucket at `url`,
/// of `capacity` bytes until its branch says its own, placed at `token` if given, and mounts it:
/// made empty if new, its root the mounting user's, as the mount is.
fn mount_cell(
    path: &Path,
    url: &str,
    (of, capacity): (BranchOf, u64),
    token: Option<u64>,
) -> Result<Filesystem, Error> {
    let new = matches!(of, BranchOf::New(_));
    // Its tenant's directory, made once for all its cells.
    if let Some(tenant) = path.parent() {
        std::fs::create_dir_all(tenant)
            .map_err(|err| Error::new(err).context(format!("making {}", tenant.display())))?;
    }
    let storage = Storage::take(path, (of, capacity), url, token)?;
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
    futures::executor::block_on(mounting)
        .map_err(|err| err.context(format!("mounting {}", path.display())))
}

/// The capacity a store taking a cell's filesystem is made with, until its branch says its own,
/// which every filesystem made here does as it is made: a block.
const TAKEN_CAPACITY: u64 = Storage::BLOCK_SIZE as u64;

impl Control {
    /// The mount's server, unless the mount is closed.
    fn server(&self) -> Result<Arc<Server>, Status> {
        self.server.upgrade().ok_or_else(|| Status::unavailable("the mount of cells is closed"))
    }

    /// Attaches `cell`'s filesystem at its path, as [`mount_cell`] takes and mounts it, placed at
    /// `token` if given: a new one of `capacity` bytes if `new`, else the one `object` names, if
    /// an Fxfs one. Returns its branch, or 0 if it shows none.
    async fn attach_cell(
        &self,
        cell: Cell,
        object: &cell::Object,
        (new, capacity): (bool, u64),
        token: Option<u64>,
    ) -> Result<u64, Error> {
        let Some(server) = self.server.upgrade() else {
            return Err(Errno::ENOTCONN.into());
        };
        let of = match (new, object.filesystem) {
            (true, _) => (BranchOf::New(cell), capacity),
            (false, Some(filesystem)) if filesystem.kind == Kind::Fxfs => {
                (BranchOf::Taken(Branch { cell, id: filesystem.branch }), TAKEN_CAPACITY)
            }
            (false, _) => return Ok(0),
        };
        let shown = cell_path(cell);
        let path = self.stores.dir.join(&shown);
        let (url, holds, held) = (self.stores.bucket.clone(), self.holds.clone(), path.clone());
        let opened = path.clone();
        let open = async move {
            let opened = tokio::task::spawn_blocking(move || {
                held_open(&holds, &path, || mount_cell(&path, &url, of, token))
            });
            opened.await.map_err(Error::new)?
        };
        if let Err(err) = server.attach(&shown, open).await {
            self.holds.lock().unwrap().remove(&held);
            return Err(err);
        }
        // Its branch's ID, the bucket's to give if new (TASK-364), as its description says.
        Ok(extent_store::host_branch(&opened).map_err(Error::new)?.id)
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
        let cell = match cell {
            0 => crate::host_store::new_cell(tenant),
            id => Cell { tenant, id },
        };
        reserve(&self.cells, (cell, Attaching::Busy), |found| match found {
            None => Ok(()),
            Some(Attaching::Attached(_)) => Err(Errno::EEXIST),
            Some(Attaching::Busy | Attaching::Detaching) => Err(Errno::EBUSY),
        })
        .map_err(|errno| failed(errno.into()))?;
        let control = Control {
            server: self.server.clone(),
            stores: self.stores.clone(),
            cells: self.cells.clone(),
            holds: self.holds.clone(),
        };
        let attached = whole(async move {
            let attached = async {
                let url = control.stores.bucket.clone();
                let held = tokio::task::spawn_blocking(move || {
                    let bucket = crate::host_store::bucket(&url).map_err(Error::new)?;
                    let placement = match token {
                        Some(token) => token,
                        None => crate::host_store::token().map_err(Error::new)?,
                    };
                    // cladefs: a cell made gets a KEK its tenant's wrapped: every cell is encrypted
                    // (TASK-411.3, TASK-411.28).
                    let kms = crate::bucket::kms()?;
                    let holding = async {
                        let kek = match make {
                            true => Some(keys::new_cell_kek(&kms, cell.tenant).await?),
                            false => None,
                        };
                        let making = kek.as_ref().map(|(_, kek)| kek);
                        let held = cell::hold(&*bucket, cell, (placement, placement), making, None);
                        let (held, object) = match held.await {
                            Ok(held) => held,
                            Err(err) => {
                                if let Some((secret, kek)) = kek {
                                    kms.give_back(cell.tenant, (secret, kek.wrapped));
                                }
                                return Err(err);
                            }
                        };
                        drop(keys::held(&kms, cell, object.clone(), kek));
                        Ok::<_, extent_bucket::Error>((held, object))
                    };
                    let held = extent_store::upload::block_on(holding).map_err(Error::new)?;
                    let (_, object) = held.map_err(|err| {
                        let errno = cell_errno(&err);
                        Error::new(anyhow::Error::new(err).context(errno))
                    })?;
                    Ok::<_, Error>(object)
                });
                let object = held.await.map_err(Error::new)??;
                control.attach_cell(cell, &object, (make, capacity), token).await
            }
            .await;
            let mut cells = control.cells.lock().unwrap();
            match &attached {
                Ok(_) => cells.insert(cell, Attaching::Attached(token)),
                Err(_) => cells.remove(&cell),
            };
            attached
        });
        let branch = attached.await?;
        Ok(Response::new(AttachResponse { cell: cell.id, branch }))
    }

    async fn detach(
        &self,
        request: Request<DetachRequest>,
    ) -> Result<Response<DetachResponse>, Status> {
        let DetachRequest { tenant, cell, force } = request.into_inner();
        let cell = Cell { tenant, id: cell };
        reserve(&self.cells, (cell, Attaching::Detaching), |found| match found {
            Some(Attaching::Attached(_)) => Ok(()),
            Some(Attaching::Busy | Attaching::Detaching) => Err(Errno::EBUSY),
            None => Err(Errno::ENOENT),
        })
        .map_err(|errno| failed(errno.into()))?;
        let (server, cells, holds) = (self.server()?, self.cells.clone(), self.holds.clone());
        let dir = self.stores.dir.clone();
        let path = cell_path(cell);
        let detached = whole(async move {
            let shown: Vec<String> = (server.attached().into_iter())
                .map(|attached| attached.name)
                .filter(|name| *name == path)
                .collect();
            let detaching = shown.iter().map(|name| server.detach(name, force));
            let detached = futures::future::join_all(detaching).await;
            // Their stores closed, or gone from the mount all the same: the daemon's again.
            let mut held = holds.lock().unwrap();
            for name in &shown {
                held.remove(&dir.join(name));
            }
            drop(held);
            // A store that failed to unmount is gone from the mount all the same
            // (`Server::detach`): the cell is no longer attached, and attaching it again opens its
            // store anew.
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
        let attached: Vec<Cell> = (self.cells.lock().unwrap().iter())
            .filter(|(_, attaching)| !matches!(attaching, Attaching::Busy))
            .map(|(cell, _)| *cell)
            .collect();
        let (shown, dir) = (self.server()?.attached(), self.stores.dir.clone());
        let listed = tokio::task::spawn_blocking(move || {
            (attached.into_iter())
                .map(|cell| {
                    let path = cell_path(cell);
                    let shown = shown.iter().find(|attached| attached.name == path);
                    let branch = shown.map_or(0, |attached| {
                        let store = dir.join(&attached.name);
                        match extent_store::host_branch(&store) {
                            Ok(branch) => branch.id,
                            Err(err) => {
                                log::error!("fuse: the branch of {}: {err:#}", store.display());
                                0
                            }
                        }
                    });
                    let detaching = shown.is_some_and(|attached| attached.detaching);
                    AttachedCell { tenant: cell.tenant, cell: cell.id, branch, detaching }
                })
                .collect()
        });
        let cells = listed.await.map_err(|err| failed(Error::new(err)))?;
        Ok(Response::new(ListResponse { cells }))
    }
}

/// The errno a failure to hold a cell says, as the extent daemon's API has it: `ENOENT` for a cell
/// the bucket has none of, `ESTALE` for one a later placement holds.
fn cell_errno(err: &extent_bucket::Error) -> Errno {
    let failure = extent_store::upload::Failure::Bucket(err.clone());
    crate::bucket::errno(&failure).unwrap_or(Errno::EIO)
}

/// Runs `work` in a task of its own, which completes though the caller gives up on it, and its
/// result as the API's.
async fn whole<T: Send + 'static>(
    work: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> Result<T, Status> {
    match tokio::spawn(work).await {
        Ok(done) => done.map_err(failed),
        Err(err) => Err(failed(Error::new(err))),
    }
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

/// A client of the control API of a FUSE mount of many stores that blocks, for callers without a
/// runtime of their own: `cladefsctl mount`'s commands, and tests. Its errors carry the errno a call
/// failed with, if it did ([`crate::Error::errno`]).
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
        let channel = runtime.block_on(channel(socket)).map_err(|err| {
            Error::new(anyhow::Error::new(err).context(format!(
                "connecting to the daemon's control socket at {}",
                socket.display()
            )))
        })?;
        Ok(Self { runtime, client: MountClient::new(channel) })
    }

    /// Attaches `cell`, or a new cell of its tenant if its ID is 0, its filesystem a new Fxfs one
    /// of `capacity` bytes, its lease taken at `token`'s placement if given, and returns it and the
    /// branch of its filesystem shown, 0 if none (`Attach`).
    pub fn attach(
        &self,
        cell: Cell,
        capacity: u64,
        token: Option<u64>,
    ) -> Result<(Cell, u64), Error> {
        let request = AttachRequest { tenant: cell.tenant, cell: cell.id, token, capacity };
        let response = self.call(format!("attaching cell {cell}"), |mut client| async move {
            client.attach(request).await
        })?;
        Ok((Cell { id: response.cell, ..cell }, response.branch))
    }

    /// Detaches `cell`, at once if `force` (`Detach`).
    pub fn detach(&self, cell: Cell, force: bool) -> Result<(), Error> {
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
