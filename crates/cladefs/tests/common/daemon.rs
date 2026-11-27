// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An extent daemon serving on threads of a test's process, and a client of its control API.

use cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient;
use cladefs::cladefsd::control::proto::*;
use cladefs::cladefsd::control::{connect, errno};
use cladefs::{CellClient, CellId, Errno, ExtentId, VolatileSlice};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::Code;
use tonic::transport::Channel;

/// A daemon serving in `dir`: its VM socket's path, and a client of its control API, which
/// `runtime` runs.
pub struct Daemon {
    pub inner: Arc<cladefs::cladefsd::Daemon>,
    pub vms: PathBuf,
    pub client: CladefsdClient<Channel>,
    pub runtime: tokio::runtime::Runtime,
}

impl Daemon {
    /// Starts the process's default daemon in `dir` with a single managed disk (`dir/disk0`).
    pub fn new(dir: &Path) -> Self {
        Self::with_disks(dir, &[dir.join("disk0")])
    }

    /// Starts the process's default daemon in `dir` managing the disk roots `disks`.
    pub fn with_disks(dir: &Path, disks: &[PathBuf]) -> Self {
        super::host();
        let inner = Arc::clone(cladefs::cladefsd::Daemon::process());
        Self::start_inner(inner, dir, disks)
    }

    /// Starts an isolated daemon instance in `dir` with its own managed disk (`dir/disk0`),
    /// `CellTable`, and `Grants`, while sharing [`cladefs::Host::process`] (and thus the
    /// in-process `memory://` bucket and extent cache) with other daemons in the same test.
    pub fn isolated(dir: &Path) -> Self {
        super::host();
        let inner = cladefs::cladefsd::Daemon::with_host(Arc::clone(cladefs::Host::process()));
        Self::start_inner(inner, dir, &[dir.join("disk0")])
    }

    fn start_inner(inner: Arc<cladefs::cladefsd::Daemon>, dir: &Path, disks: &[PathBuf]) -> Self {
        inner.configure_disks(disks).unwrap();
        // Its disks governed, as a host's daemon's are, leaving a test's headroom (doc-22).
        let headroom = Some(cladefs::cladefsd::governor::DiskHeadroom::Bytes(64 << 20));
        let disk_cfg = cladefs::cladefsd::disk::DiskConfig { headroom, fixed: None };
        inner.start_disk_governor(disk_cfg).unwrap();
        let (vms, control) = (dir.join("vms.sock"), dir.join("control.sock"));
        let listener = UnixListener::bind(&vms).unwrap();
        let serving = Arc::clone(&inner);
        std::thread::spawn(move || serving.serve(listener));
        let listener = UnixListener::bind(&control).unwrap();
        let tenants = dir.join("tenants");
        std::fs::create_dir(&tenants).unwrap();
        let controlling = Arc::clone(&inner);
        std::thread::spawn(move || controlling.serve_control(listener, tenants));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let client = runtime.block_on(connect(&control)).unwrap();
        Self { inner, vms, client, runtime }
    }

    /// Returns the daemon-owned on-disk directory for `cell` (for white-box tests).
    pub fn cell_path(&self, cell: CellId) -> PathBuf {
        self.inner.cell_path(cell).unwrap_or_else(|| panic!("cell {cell} not found in DiskPool"))
    }

    /// Opens an existing managed cell `cell` over the daemon's default VM socket.
    pub fn open(&self, cell: CellId) -> CellClient {
        CellClient::connect_cell(&self.vms, cell).unwrap()
    }

    /// Creates and opens a new managed cell for `tenant` with `capacity` bytes on a daemon-chosen
    /// disk over the daemon's default VM socket.
    pub fn open_new(&self, tenant: u64, capacity: u64) -> (CellId, CellClient) {
        let client = CellClient::connect_new(&self.vms, tenant, capacity).unwrap();
        (CellId { tenant, id: client.cell() }, client)
    }

    /// Makes a new cell (if `cell.id == 0`) or takes an existing cell from the bucket on a
    /// daemon-chosen disk (TASK-364, hotbed question 547).
    pub fn create(&mut self, cell: CellId, token: Option<u64>) -> Result<CellId, tonic::Status> {
        let request = CreateCellRequest {
            capacity: CAPACITY,
            token,
            tenant: cell.tenant,
            cell: cell.id,
            kind: 0,
            ..CreateCellRequest::default()
        };
        let response = self.runtime.block_on(self.client.create_cell(request))?.into_inner();
        Ok(CellId { tenant: response.tenant, id: response.cell })
    }

    pub fn stat(&mut self, cell: CellId) -> Result<StatCellResponse, tonic::Status> {
        let request = StatCellRequest { tenant: cell.tenant, cell: cell.id };
        Ok(self.runtime.block_on(self.client.stat_cell(request))?.into_inner())
    }

    /// StatCell of `cell` once it says no VM process has it open: the daemon lets it go once it
    /// finds its VM process's connection gone, on its own side, after that end is shut down, not
    /// with it (board 5885). Fails if it is open still ten seconds on.
    pub fn wait_closed(&mut self, cell: CellId) -> StatCellResponse {
        self.stat_once_closed(cell).unwrap()
    }

    /// StatCell of `cell`, as [`Daemon::wait_closed`] waits for it, or its failure: a cell whose
    /// keys are forgotten is stated while it is open, and refused once closed (board 8282).
    pub fn stat_once_closed(&mut self, cell: CellId) -> Result<StatCellResponse, tonic::Status> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let stat = self.stat(cell);
            if !stat.as_ref().is_ok_and(|stat| stat.open) {
                return stat;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{cell} still open ten seconds after its VM process went"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Forks `from` onto a daemon-chosen disk (`ForkCell`), optionally into `target_tenant`.
    pub fn fork(
        &mut self,
        from: CellId,
        target_tenant: Option<u64>,
    ) -> Result<CellId, tonic::Status> {
        let request = ForkCellRequest {
            tenant: from.tenant,
            cell: from.id,
            target_tenant,
            ..Default::default()
        };
        let response = self.runtime.block_on(self.client.fork_cell(request))?.into_inner();
        Ok(CellId { tenant: response.tenant, id: response.cell })
    }

    /// Makes a version of `from` (`MakeVersion`), and returns it.
    pub fn version(&mut self, from: CellId) -> Result<cladefs::CellVersion, tonic::Status> {
        let request = MakeVersionRequest { tenant: from.tenant, cell: from.id, keep_last: 0 };
        let version = self.runtime.block_on(self.client.make_version(request))?.into_inner();
        Ok(cladefs::CellVersion { tenant: from.tenant, cell: from.id, version: version.version })
    }

    /// Forks the version `from` onto a daemon-chosen disk, optionally into `target_tenant`.
    pub fn fork_version(
        &mut self,
        from: cladefs::CellVersion,
        target_tenant: Option<u64>,
    ) -> Result<CellId, tonic::Status> {
        let cladefs::CellVersion { tenant, cell, version } = from;
        let request =
            ForkCellRequest { version, tenant, cell, target_tenant, ..Default::default() };
        let response = self.runtime.block_on(self.client.fork_cell(request))?.into_inner();
        Ok(CellId { tenant: response.tenant, id: response.cell })
    }

    /// Drops the versions `versions` of `cell` (`DropVersions`).
    pub fn drop_versions(&mut self, cell: CellId, versions: &[u64]) -> Result<(), tonic::Status> {
        let request =
            DropVersionsRequest { tenant: cell.tenant, cell: cell.id, versions: versions.to_vec() };
        self.runtime.block_on(self.client.drop_versions(request)).map(drop)
    }

    pub fn resize(&mut self, cell: CellId, capacity: u64) -> Result<(), tonic::Status> {
        let request = ResizeCellRequest { tenant: cell.tenant, cell: cell.id, capacity };
        self.runtime.block_on(self.client.resize_cell(request)).map(drop)
    }

    pub fn place(&mut self, cell: CellId, token: u64) -> Result<(), tonic::Status> {
        let request = PlaceCellRequest { tenant: cell.tenant, cell: cell.id, token };
        self.runtime.block_on(self.client.place_cell(request)).map(drop)
    }

    /// Shares the version `version` of `cell`, or no longer (`ShareVersion`, TASK-395.7).
    pub fn share(&mut self, cell: CellId, version: u64, shared: bool) -> Result<(), tonic::Status> {
        let request = ShareVersionRequest { tenant: cell.tenant, cell: cell.id, version, shared };
        self.runtime.block_on(self.client.share_version(request)).map(drop)
    }

    pub fn make_public(&mut self, cell: CellId) -> Result<(), tonic::Status> {
        let request = MakePublicRequest { tenant: cell.tenant, cell: cell.id };
        self.runtime.block_on(self.client.make_public(request)).map(drop)
    }

    pub fn sync(&mut self, cell: CellId) -> Result<(), tonic::Status> {
        let request = SyncCellRequest { tenant: cell.tenant, cell: cell.id };
        self.runtime.block_on(self.client.sync_cell(request)).map(drop)
    }

    pub fn delete(&mut self, cell: CellId) -> Result<(), tonic::Status> {
        let request = DeleteCellRequest { tenant: cell.tenant, cell: cell.id };
        self.runtime.block_on(self.client.delete_cell(request)).map(drop)
    }

    pub fn drop_cell(&mut self, cell: CellId) -> Result<(), tonic::Status> {
        let request = DropCellRequest { tenant: cell.tenant, cell: cell.id };
        self.runtime.block_on(self.client.drop_cell(request)).map(drop)
    }

    pub fn import(&mut self, tenant: u64, image: &Path) -> Result<CellId, tonic::Status> {
        let request = ImportImageRequest {
            capacity: CAPACITY,
            image: name(image),
            reference: String::new(),
            tenant,
        };
        let response = self.runtime.block_on(self.client.import_image(request))?.into_inner();
        Ok(CellId { tenant: response.tenant, id: response.cell })
    }

    /// Grants `tenant` its VM socket, given to `uid`, if named.
    pub fn grant(&mut self, tenant: u64, uid: Option<u32>) -> Result<PathBuf, tonic::Status> {
        self.grant_to(tenant, (uid, None))
    }

    /// Grants as [`Daemon::grant`] does, the socket given to the uid and gid `to` names.
    pub fn grant_to(
        &mut self,
        tenant: u64,
        (uid, gid): (Option<u32>, Option<u32>),
    ) -> Result<PathBuf, tonic::Status> {
        let request = GrantRequest { tenant, uid, gid };
        Ok(self.runtime.block_on(self.client.grant(request))?.into_inner().socket.into())
    }

    pub fn revoke(&mut self, tenant: u64) -> Result<(), tonic::Status> {
        let request = RevokeRequest { tenant };
        self.runtime.block_on(self.client.revoke(request)).map(drop)
    }

    /// The cells the daemon serves, by their `(tenant, cell)` IDs, in order.
    pub fn list(&mut self) -> Vec<(CellId, StatCellResponse)> {
        let listed = self.runtime.block_on(self.client.list_cells(ListCellsRequest {}));
        let mut cells: Vec<_> = (listed.unwrap().into_inner().stores.into_iter())
            .map(|cell| (CellId { tenant: cell.tenant, id: cell.cell }, cell.stat.unwrap()))
            .collect();
        cells.sort_by_key(|(id, _)| (id.tenant, id.id));
        cells
    }

    /// Lists all managed disks and their live governance state.
    pub fn list_disks(&mut self) -> Result<Vec<DiskInfo>, tonic::Status> {
        let response =
            self.runtime.block_on(self.client.list_disks(ListDisksRequest {}))?.into_inner();
        Ok(response.disks)
    }

    /// Updates live governance on one managed disk.
    pub fn set_disk_governance(
        &mut self,
        request: SetDiskGovernanceRequest,
    ) -> Result<(), tonic::Status> {
        self.runtime.block_on(self.client.set_disk_governance(request)).map(drop)
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

    /// What the disk of `tenant`, or of every tenant, holds past what it takes against its limit:
    /// its cells' reserves and its writes admitted (doc-22).
    pub fn tenant_reserved(&mut self, tenant: Option<u64>) -> u64 {
        let request = GetTenantDiskRequest { tenant };
        let response = self.runtime.block_on(self.client.get_tenant_disk(request)).unwrap();
        response.into_inner().reserved_bytes
    }

    pub fn counters(&mut self) -> GetCountersResponse {
        let counters = self.client.get_counters(GetCountersRequest {});
        self.runtime.block_on(counters).unwrap().into_inner()
    }
}

/// A cell to make anew: of a new cell of the default tenant's, a new ID there (TASK-364).
pub const NEW: CellId = CellId { tenant: 0, id: 0 };

pub const CAPACITY: u64 = 1 << 26;

pub fn name(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

/// The code and errno of a call that failed.
pub fn failure<T: std::fmt::Debug>(result: Result<T, tonic::Status>) -> (Code, Errno) {
    let status = result.expect_err("the call succeeded");
    (status.code(), errno(&status).expect("the status carries an errno"))
}

/// Writes `bytes` as the first extent of `client` and commits, setting its root to `root`.
pub fn write(client: &CellClient, bytes: &[u8], root: &[u8]) {
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    client.write(extent, 0, &[VolatileSlice::from(bytes)]).unwrap();
    client.commit(Some(root)).unwrap();
}
