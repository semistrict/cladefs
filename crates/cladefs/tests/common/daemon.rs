// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An extent daemon serving on threads of a test's process, and a client of its control API.

use cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient;
use cladefs::cladefsd::control::proto::*;
use cladefs::cladefsd::control::{connect, errno};
use cladefs::cladefsd::{serve, serve_control};
use cladefs::{CellClient, Errno, ExtentId, VolatileSlice};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use tonic::Code;
use tonic::transport::Channel;

/// A daemon serving in `dir`: its VM socket's path, and a client of its control API, which
/// `runtime` runs.
pub struct Daemon {
    pub vms: PathBuf,
    pub client: CladefsdClient<Channel>,
    pub runtime: tokio::runtime::Runtime,
}

impl Daemon {
    pub fn new(dir: &Path) -> Self {
        super::host();
        let (vms, control) = (dir.join("vms.sock"), dir.join("control.sock"));
        let listener = UnixListener::bind(&vms).unwrap();
        std::thread::spawn(move || serve(listener));
        let listener = UnixListener::bind(&control).unwrap();
        let tenants = dir.join("tenants");
        std::fs::create_dir(&tenants).unwrap();
        std::thread::spawn(move || serve_control(listener, tenants));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let client = runtime.block_on(connect(&control)).unwrap();
        Self { vms, client, runtime }
    }

    /// A VM process's store at `path`, created with `capacity` bytes if it doesn't exist.
    pub fn open(&self, path: &Path, capacity: u64) -> CellClient {
        CellClient::connect(&self.vms, path, capacity).unwrap()
    }

    /// Makes a store at `path` of `branch`, an Fxfs cell: a new cell if its cell's
    /// ID is 0, a new branch there if its ID is (TASK-364).
    pub fn create(
        &mut self,
        path: &Path,
        branch: cladefs::CellId,
        token: Option<u64>,
    ) -> Result<cladefs::CellId, tonic::Status> {
        let request = CreateCellRequest {
            path: name(path),
            capacity: CAPACITY,
            branch: branch.id,
            token,
            tenant: branch.tenant,
            cell: branch.id,
            kind: 0,
        };
        let response = self.runtime.block_on(self.client.create_cell(request))?.into_inner();
        let cell = cladefs::CellId { tenant: response.tenant, id: response.cell };
        Ok(cladefs::CellId { tenant: cell.tenant, id: response.branch })
    }

    /// The branch of the store at `path`, in its cell, as StatCell says it (TASK-364).
    pub fn branch(&mut self, path: &Path) -> Result<cladefs::CellId, tonic::Status> {
        let stat = self.stat(path)?;
        let cell = cladefs::CellId { tenant: stat.tenant, id: stat.cell };
        Ok(cladefs::CellId { tenant: cell.tenant, id: stat.branch })
    }

    pub fn stat(&mut self, path: &Path) -> Result<StatCellResponse, tonic::Status> {
        let request = StatCellRequest { path: name(path) };
        Ok(self.runtime.block_on(self.client.stat_cell(request))?.into_inner())
    }

    /// Forks the store at `from` to `to` (ForkCell).
    pub fn fork(&mut self, from: &Path, to: &Path) -> Result<cladefs::CellId, tonic::Status> {
        let request = ForkCellRequest { from: name(from), to: name(to), ..Default::default() };
        let response = self.runtime.block_on(self.client.fork_cell(request))?.into_inner();
        let cell = cladefs::CellId { tenant: response.tenant, id: response.cell };
        Ok(cladefs::CellId { tenant: cell.tenant, id: response.branch })
    }

    /// Makes a version of the cell of the store at `from` (MakeVersion), and returns it.
    pub fn version(&mut self, from: &Path) -> Result<cladefs::CellVersion, tonic::Status> {
        let request = MakeVersionRequest { path: name(from), keep_last: 0 };
        let version = self.runtime.block_on(self.client.make_version(request))?.into_inner();
        let cell = self.branch(from)?;
        Ok(cladefs::CellVersion { tenant: cell.tenant, cell: cell.id, version: version.version })
    }

    /// Forks the version `from` to `to`, on this host, whichever made it (ForkCell's `version`).
    pub fn fork_version(
        &mut self,
        from: cladefs::CellVersion,
        to: &Path,
    ) -> Result<cladefs::CellId, tonic::Status> {
        let cladefs::CellVersion { tenant, cell, version } = from;
        let request = ForkCellRequest { to: name(to), version, tenant, cell, ..Default::default() };
        let response = self.runtime.block_on(self.client.fork_cell(request))?.into_inner();
        let cell = cladefs::CellId { tenant: response.tenant, id: response.cell };
        Ok(cladefs::CellId { tenant: cell.tenant, id: response.branch })
    }

    /// Drops the versions `versions` of the cell of the store at `path` (DropVersions).
    pub fn drop_versions(&mut self, path: &Path, versions: &[u64]) -> Result<(), tonic::Status> {
        let request = DropVersionsRequest { path: name(path), versions: versions.to_vec() };
        self.runtime.block_on(self.client.drop_versions(request)).map(drop)
    }

    pub fn resize(&mut self, path: &Path, capacity: u64) -> Result<(), tonic::Status> {
        let request = ResizeCellRequest { path: name(path), capacity };
        self.runtime.block_on(self.client.resize_cell(request)).map(drop)
    }

    pub fn place(&mut self, path: &Path, token: u64) -> Result<(), tonic::Status> {
        let request = PlaceCellRequest { path: name(path), token };
        self.runtime.block_on(self.client.place_cell(request)).map(drop)
    }

    /// Shares the version `version` of the cell of the store at `path`, or no longer
    /// (ShareVersion, TASK-395.7).
    pub fn share(&mut self, path: &Path, version: u64, shared: bool) -> Result<(), tonic::Status> {
        let request = ShareVersionRequest { path: name(path), version, shared };
        self.runtime.block_on(self.client.share_version(request)).map(drop)
    }

    pub fn make_public(&mut self, path: &Path) -> Result<(), tonic::Status> {
        let request = MakePublicRequest { path: name(path), attach_only: false };
        self.runtime.block_on(self.client.make_public(request)).map(drop)
    }

    pub fn sync(&mut self, path: &Path) -> Result<(), tonic::Status> {
        let request = SyncCellRequest { path: name(path) };
        self.runtime.block_on(self.client.sync_cell(request)).map(drop)
    }

    pub fn delete(&mut self, path: &Path) -> Result<(), tonic::Status> {
        let request = DeleteCellRequest { path: name(path) };
        self.runtime.block_on(self.client.delete_cell(request)).map(drop)
    }

    pub fn drop_cell(&mut self, path: &Path) -> Result<(), tonic::Status> {
        let request = DropCellRequest { path: name(path) };
        self.runtime.block_on(self.client.drop_cell(request)).map(drop)
    }

    pub fn import(&mut self, path: &Path, image: &Path) -> Result<cladefs::CellId, tonic::Status> {
        let request = ImportImageRequest {
            path: name(path),
            capacity: CAPACITY,
            image: name(image),
            reference: String::new(),
            tenant: 0,
        };
        let response = self.runtime.block_on(self.client.import_image(request))?.into_inner();
        let cell = cladefs::CellId { tenant: response.tenant, id: response.cell };
        Ok(cladefs::CellId { tenant: cell.tenant, id: response.branch })
    }

    /// Grants the tenant of the ID `tenant` in the bucket (TASK-364), whose stores are in `dir`,
    /// its socket, given to `uid`, if named.
    pub fn grant(
        &mut self,
        tenant: u64,
        dir: &Path,
        uid: Option<u32>,
    ) -> Result<PathBuf, tonic::Status> {
        let request = GrantRequest { tenant, dir: name(dir), uid, gid: None };
        Ok(self.runtime.block_on(self.client.grant(request))?.into_inner().socket.into())
    }

    pub fn revoke(&mut self, tenant: u64) -> Result<(), tonic::Status> {
        let request = RevokeRequest { tenant };
        self.runtime.block_on(self.client.revoke(request)).map(drop)
    }

    /// The stores the daemon serves, by their paths, in order.
    pub fn list(&mut self) -> Vec<(PathBuf, StatCellResponse)> {
        let listed = self.runtime.block_on(self.client.list_cells(ListCellsRequest {}));
        let mut stores: Vec<_> = (listed.unwrap().into_inner().stores.into_iter())
            .map(|store| (PathBuf::from(store.path), store.stat.unwrap()))
            .collect();
        stores.sort_by(|a, b| a.0.cmp(&b.0));
        stores
    }

    /// Limits the memory of `tenant`, or of every tenant, to `bytes`, or lifts the limit.
    pub fn set_memory_limit(
        &mut self,
        tenant: Option<u64>,
        bytes: Option<u64>,
    ) -> Result<(), tonic::Status> {
        let request = SetMemoryLimitRequest { tenant, bytes };
        self.runtime.block_on(self.client.set_memory_limit(request)).map(drop)
    }

    /// Shrinks the memory of `tenant`, or of every tenant, toward `target` bytes, and returns what
    /// is held then.
    pub fn shrink_memory(
        &mut self,
        tenant: Option<u64>,
        target: u64,
    ) -> Result<u64, tonic::Status> {
        let request = ShrinkMemoryRequest { tenant, target_bytes: target };
        Ok(self.runtime.block_on(self.client.shrink_memory(request))?.into_inner().bytes)
    }

    /// Limits the disk of `tenant`, or of every tenant, to `bytes`, or lifts the limit (TASK-94).
    pub fn set_disk_limit(
        &mut self,
        tenant: Option<u64>,
        bytes: Option<u64>,
    ) -> Result<(), tonic::Status> {
        let request = SetDiskLimitRequest { tenant, bytes };
        self.runtime.block_on(self.client.set_disk_limit(request)).map(drop)
    }

    /// What the disk of `tenant`, or of every tenant, takes, counted now, and its limit.
    pub fn tenant_disk(
        &mut self,
        tenant: Option<u64>,
    ) -> Result<(u64, Option<u64>), tonic::Status> {
        let request = GetTenantDiskRequest { tenant };
        let response = self.runtime.block_on(self.client.get_tenant_disk(request))?.into_inner();
        Ok((response.bytes, response.limit_bytes))
    }

    pub fn counters(&mut self) -> GetCountersResponse {
        let counters = self.client.get_counters(GetCountersRequest {});
        self.runtime.block_on(counters).unwrap().into_inner()
    }
}

/// A branch to make anew: of a new cell of the default tenant's, a new ID there (TASK-364).
pub const NEW: cladefs::CellId = cladefs::CellId { tenant: 0, id: 0 };

pub const CAPACITY: u64 = 1 << 26;

pub fn name(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

/// The code and errno of a call that failed.
pub fn failure<T: std::fmt::Debug>(result: Result<T, tonic::Status>) -> (Code, Errno) {
    let status = result.expect_err("the call succeeded");
    (status.code(), errno(&status).expect("the status carries an errno"))
}

/// Writes `bytes` as the first extent of `store` and commits, setting its root to `root`.
pub fn write(store: &CellClient, bytes: &[u8], root: &[u8]) {
    let extent = ExtentId { branch: store.branch(), sequence: 1 };
    store.write(extent, 0, &[VolatileSlice::from(bytes)]).unwrap();
    store.commit(Some(root)).unwrap();
}
