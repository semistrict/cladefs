// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The storage a filesystem lives on: an image, or memory.
//!
//! An image is a cell directory, its cell kept in the bucket its description names, an image's own
//! file bucket where it was made on no host of cladefs's system (`host_cell::create`), opened as an
//! [`OpenCell`] and mounted through it ([`OpenCellDevice`]): the extent daemon's own path, the
//! cell's page cache and the extent cache, which guests' filesystems take too (board 8260). An
//! image's own bucket's collector runs in the process, after commits ([`collect`]); a host's
//! bucket's is the host's.

pub use cladefs_core::collect;
mod device;
#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod tests;

use crate::filesystem::fxfs_runtime;
use anyhow::Context as _;
use cladefs_core::error::{Error, Result};
use cladefs_core::host_cell::{self, Opened};
use cladefs_core::open_cell::{OpenCell, Opening, release};
pub use device::OpenCellDevice;
use fxfs::filesystem::FxFilesystem;
use fxfs::fsck::errors::FsckIssue;
use fxfs::fsck::{FsckOptions, fsck_volume_with_options, fsck_with_options};
use fxfs::object_store::volume::{list_volumes, root_volume};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

/// The storage a filesystem lives on.
///
/// A filesystem takes its storage when it mounts it, and gives it back when it is closed, ready
/// to be mounted or checked again.
pub struct Storage {
    device: DeviceHolder,
    kind: Kind,
}

#[derive(Clone)]
pub enum Kind {
    /// An image: its directory, and its open cell, which forking the image forks. The open cell is
    /// its device's, which holds it: whoever closes the storage closes the open cell, however many
    /// copies of this name it, as a mounted image's does. Its files bypass the host's page cache,
    /// or the image opens not at all (TASK-433.6).
    Image {
        dir: PathBuf,
        cell: Weak<OpenCell>,
        /// The collector its device's commits start passes of, for tests to look at.
        #[cfg(test)]
        collecting: Option<Arc<collect::Collecting>>,
    },
    Memory,
}

impl fmt::Debug for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Image { dir, .. } => f.debug_struct("Image").field("dir", dir).finish(),
            Self::Memory => write!(f, "Memory"),
        }
    }
}

impl Storage {
    /// The size of the blocks storage is read and written in. Storage sizes are multiples of it.
    pub const BLOCK_SIZE: u32 = host_cell::BLOCK_SIZE;

    /// The image at `path` opened on `host`.
    pub async fn open_on(
        host: &Arc<cladefs_core::Host>,
        path: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let keeping = Self::keeping_on(host)?;
        Self::open_on_keeping(host, path, keeping).await
    }

    /// The image at `path` opened on `host` with explicit `keeping` options (e.g. a daemon's
    /// shared `CLADEFSD_PART_CACHE` and fetch budget, board 11363). Handles ephemeral cells as
    /// [`Opening::open`] does: opened with no uploader, reading inherited extents through `keeping`'s
    /// cache and fetch budgets.
    pub async fn open_on_keeping(
        host: &Arc<cladefs_core::Host>,
        path: impl AsRef<Path>,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        let dir = path.as_ref().to_owned();
        let task_host = host.clone();
        on_bucket_runtime_on(host, async move {
            let context = || format!("opening {}", dir.display());
            if local_cell::upload::blocking(|| host_cell::is_ephemeral(&dir)) {
                let opening = Opening::keeping_on(task_host.clone(), keeping).for_host();
                let cell =
                    Arc::new(opening.open(&dir).await.map_err(|err| err.context(context()))?);
                return Self::wrap_cell(dir, cell, None).await;
            }
            let opened = host_cell::open_on(&task_host, &dir, keeping.clone()).await;
            let opened = opened.map_err(|err| Error::from(err).context(context()))?;
            Self::opened_on(&task_host, dir, opened, keeping).await
        })
        .await
    }

    /// The image at `path`: a cell directory, a fork's or not, its cell kept in the bucket its
    /// description names ([`host_cell::open`]).
    ///
    /// Its cell's files bypass the host's page cache where the host filesystem allows, as the
    /// cell's page cache, and the in-process kernel's, cache their pages already (one memory
    /// budget, TASK-433). Its uploader takes its cell's head as a copy never placed takes it where
    /// this process is no host's daemon ([`host_cell::keeping`]). It fails for an image made by
    /// older code, which kept its cell in no bucket, and which nothing converts.
    #[cfg(not(madsim))]
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_on(cladefs_core::Host::process(), path).await
    }

    /// A new image at `path`, which must not exist, holding at most `size` bytes. It holds nothing
    /// until the filesystem writes to it, and must be formatted before it is mounted. Its cell
    /// is kept in a file bucket of its own, beside its directory (`<image>.bucket`), which its
    /// forks share and which outlives it; or, on a host of cladefs's system, in the host's bucket
    /// (`CLADEFSD_BUCKET`).
    #[cfg(not(madsim))]
    pub async fn create(path: impl AsRef<Path>, size: u64) -> Result<Self, Error> {
        let host = cladefs_core::Host::process();
        Self::make_on(
            host,
            path.as_ref(),
            (size, host_cell::DEFAULT_TENANT),
            bucket_cell::cell::Kind::Fxfs,
            true,
            Self::keeping_on(host)?,
        )
        .await
    }

    /// cladefs: the image at `path` of the cell `of` says, kept in the bucket at `bucket`
    /// (TASK-302), an Fxfs cell named as its directory is (TASK-364). If no
    /// image is at `path`, or what an image's making left unmade, a new one of `capacity` bytes:
    /// of a cell the bucket has, holding what it holds there, its head taken, as `cladefsctl
    /// take` takes a cell; or of a new cell, its ID the bucket's to give, which holds
    /// nothing: a filesystem must be formatted on it. Its cell is held with `token`'s placement,
    /// which it is placed at, if given, as `Storage::open` takes heads otherwise. If an image is at
    /// `path`, it is opened as [`Storage::open`] opens it, and must be of the cell taken:
    /// `EEXIST` otherwise, and for a new cell.
    #[cfg(not(madsim))]
    pub async fn take(
        path: impl AsRef<Path>,
        (of, capacity): (host_cell::CellOf, u64),
        bucket: &str,
        token: Option<u64>,
    ) -> Result<Self, Error> {
        let host = cladefs_core::Host::process().clone();
        let keeping = Self::keeping_on(&host)?;
        Self::take_on_keeping(&host, path, (of, capacity), bucket, token, keeping).await
    }

    /// cladefs: as [`Storage::take`], on `host` with explicit `keeping` options (so the daemon's
    /// FUSE mount shares `CLADEFSD_PART_CACHE`, fetch budgets, and held parts, board 11363).
    #[cfg(not(madsim))]
    pub async fn take_on_keeping(
        host: &Arc<cladefs_core::Host>,
        path: impl AsRef<Path>,
        (of, capacity): (host_cell::CellOf, u64),
        bucket: &str,
        token: Option<u64>,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        let (path, bucket) = (path.as_ref().to_owned(), bucket.to_owned());
        on_bucket_runtime_on(
            host,
            Self::taken_on(host.clone(), path, (of, capacity), bucket, token, keeping),
        )
        .await
    }

    /// [`Storage::take`]'s, on the bucket runtime.
    #[cfg(not(madsim))]
    async fn taken_on(
        host: Arc<cladefs_core::Host>,
        path: PathBuf,
        (of, capacity): (host_cell::CellOf, u64),
        bucket: String,
        token: Option<u64>,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        let (path, bucket) = (path.as_path(), bucket.as_str());
        let keeping =
            local_cell::upload::Options { token: token.unwrap_or(keeping.token), ..keeping };
        // What a making that died part way left is no cell: its directory goes, not the bucket's
        // cell, which may be another's, as the extent daemon's `CreateCell` has it (TASK-196).
        let exists = local_cell::upload::blocking(|| -> Result<bool, Error> {
            if host_cell::unmade_on(&host, path) {
                local_cell::remove_cell_dir_on(host.local(), path)
                    .with_context(|| {
                        format!("removing {}, a cell directory unmade", path.display())
                    })
                    .map_err(Error::new)?;
            }
            Ok(host_fs::exists(path))
        })?;
        if !exists {
            let kept = (bucket_cell::cell::Kind::Fxfs, Some(bucket), keeping.clone());
            let making =
                host_cell::create_unmade_on(&host, path, capacity, of, kept.0, kept.1, kept.2);
            let opened = making.await?;
            local_cell::upload::blocking(|| opened.made()).map_err(Error::new)?;
            // Closed before it is placed, as `CreateCell` places a cell, and opened again.
            opened.close().await;
            if let Some(token) = token {
                local_cell::upload::blocking(|| {
                    local_cell::place_host_on(host.local(), path, token, false)
                })
                .map_err(Error::new)?;
            }
        } else {
            let found =
                local_cell::upload::blocking(|| local_cell::host_cell(path)).map_err(Error::new)?;
            let not = match of {
                host_cell::CellOf::Taken(cell) if found == cell => None,
                host_cell::CellOf::Taken(cell) => Some(format!("{cell}")),
                _ => Some("a new cell".to_owned()),
            };
            if let Some(not) = not {
                let error = anyhow::anyhow!("{} holds cell {found}, not {not}", path.display());
                return Err(Error::new(error.context(cladefs_core::Errno::EEXIST)));
            }
            // cladefs: honor the caller's fencing token on an existing cell directory too, and
            // open through `host_cell::take` so a diverged local log is dropped (TASK-576,
            // board 2548, board 2702).
            if let Some(token) = token {
                local_cell::upload::blocking(|| {
                    local_cell::place_host_on(host.local(), path, token, false)
                })
                .map_err(Error::new)?;
            }
            let opened = host_cell::take_on(&host, path, keeping.clone()).await?;
            return Self::opened_on(&host, path.to_owned(), opened, keeping).await;
        }
        Self::open_on_keeping(&host, path, keeping).await
    }

    /// cladefs: a new image at `path`, as [`Storage::create`] makes it, unmade until its maker says
    /// it is whole ([`Storage::made`]): what opens it before, or after its maker died part way,
    /// finds no cell (TASK-196). As an import makes one, filling it first.
    #[cfg(not(madsim))]
    pub async fn create_unmade(path: impl AsRef<Path>, size: u64) -> Result<Self, Error> {
        let host = cladefs_core::Host::process();
        Self::make_on(
            host,
            path.as_ref(),
            (size, host_cell::DEFAULT_TENANT),
            bucket_cell::cell::Kind::Fxfs,
            false,
            Self::keeping_on(host)?,
        )
        .await
    }

    /// cladefs: a new image at `path` on `host`, unmade until its maker says it is whole.
    pub async fn create_unmade_keeping_on(
        host: &Arc<cladefs_core::Host>,
        path: impl AsRef<Path>,
        (size, tenant): (u64, u64),
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        Self::make_on(
            host,
            path.as_ref(),
            (size, tenant),
            bucket_cell::cell::Kind::Fxfs,
            false,
            keeping,
        )
        .await
    }

    /// cladefs: a new unmade cell of `kind` at `path` on `host` of `size` bytes, a new cell of
    /// `tenant`'s, kept as `keeping` says (board 11222): as [`Storage::create_unmade_keeping_on`]
    /// does for Fxfs images, for memory cells too.
    pub async fn create_unmade_kind_keeping_on(
        host: &Arc<cladefs_core::Host>,
        path: impl AsRef<Path>,
        (size, tenant): (u64, u64),
        kind: bucket_cell::cell::Kind,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        Self::make_on(host, path.as_ref(), (size, tenant), kind, false, keeping).await
    }

    /// cladefs: a new unmade standalone (parentless) host-local ephemeral cell of `kind` at `path`
    /// on `host` of `size` bytes, for `tenant`, writing nothing to the bucket.
    pub async fn create_unmade_ephemeral_kind_keeping_on(
        host: &Arc<cladefs_core::Host>,
        path: impl AsRef<Path>,
        (size, tenant): (u64, u64),
        kind: bucket_cell::cell::Kind,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        if size < Self::BLOCK_SIZE as u64 {
            return Err(Error::msg("the image is smaller than a block"));
        }
        let path = path.as_ref().to_owned();
        let task_host = host.clone();
        on_bucket_runtime_on(host, async move {
            let (local, bucket, keys) = host_cell::create_ephemeral_unmade_on(
                &task_host, &path, size, tenant, kind, &keeping,
            )
            .await?;
            let opening = Opening::keeping_on(task_host.clone(), keeping).for_host();
            let cell = Arc::new(
                OpenCell::from_ephemeral_parts(&path, local, bucket, keys, opening).await?,
            );
            Self::wrap_cell(path, cell, None).await
        })
        .await
    }

    /// How an image `host` makes keeps its cell.
    fn keeping_on(host: &cladefs_core::Host) -> Result<local_cell::upload::Options, Error> {
        host_cell::keeping_on(host).map_err(Error::new)
    }

    /// cladefs: the image at `path` on `host` is whole.
    pub fn made_on(host: &cladefs_core::Host, path: impl AsRef<Path>) -> Result<(), Error> {
        local_cell::made_host_on(host.local(), path.as_ref()).map_err(Error::new)
    }

    /// cladefs: the image at `path`, made by [`Storage::create_unmade`] and closed since, is whole.
    #[cfg(not(madsim))]
    pub fn made(path: impl AsRef<Path>) -> Result<(), Error> {
        Self::made_on(cladefs_core::Host::process(), path)
    }

    /// cladefs: whether `path` is unmade on `host`.
    pub fn is_unmade_on(host: &cladefs_core::Host, path: impl AsRef<Path>) -> bool {
        host_cell::unmade_on(host, path.as_ref())
    }

    /// cladefs: whether `path` is what a making of an image, or of a cell, left unmade, its maker
    /// dying part way: no cell, which deleting finishes (TASK-196).
    #[cfg(not(madsim))]
    pub fn is_unmade(path: impl AsRef<Path>) -> bool {
        Self::is_unmade_on(cladefs_core::Host::process(), path)
    }

    /// A new image at `path` on `host`, a new cell of `tenant`'s of `kind`, whole if `whole`.
    pub(super) async fn make_on(
        host: &Arc<cladefs_core::Host>,
        path: &Path,
        (size, tenant): (u64, u64),
        kind: bucket_cell::cell::Kind,
        whole: bool,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        if size < Self::BLOCK_SIZE as u64 {
            return Err(Error::msg("the image is smaller than a block"));
        }
        let path = path.to_owned();
        let task_host = host.clone();
        on_bucket_runtime_on(host, async move {
            let of = host_cell::CellOf::New(tenant);
            let opened = host_cell::create_unmade_on(
                &task_host,
                &path,
                size,
                of,
                kind,
                None,
                keeping.clone(),
            );
            let opened = opened.await?;
            if whole {
                local_cell::upload::blocking(|| opened.made()).map_err(Error::new)?;
            }
            Self::opened_on(&task_host, path, opened, keeping).await
        })
        .await
    }

    /// The storage of the image `opened` at `dir` on `host`.
    async fn opened_on(
        host: &Arc<cladefs_core::Host>,
        dir: PathBuf,
        opened: Opened,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        let collecting = host_cell::keeps_own_bucket(&opened.local).then(|| {
            let url = opened.local.bucket().unwrap_or_default().to_owned();
            collect::of_on(host, &url, &opened.bucket)
        });
        let opening = Opening::keeping_on(host.clone(), keeping).for_host();
        let cell = Arc::new(OpenCell::new(&dir, opened, opening).await?);
        Self::wrap_cell(dir, cell, collecting).await
    }

    /// Wraps an already-opened [`OpenCell`] at `dir` as a [`Storage`] device.
    async fn wrap_cell(
        dir: PathBuf,
        cell: Arc<OpenCell>,
        collecting: Option<Arc<collect::Collecting>>,
    ) -> Result<Self, Error> {
        let kind = Kind::Image {
            dir,
            cell: Arc::downgrade(&cell),
            #[cfg(test)]
            collecting: collecting.clone(),
        };
        let device = match OpenCellDevice::new(cell.clone(), collecting) {
            Ok(device) => device,
            Err(err) => {
                release(cell).await;
                return Err(Error::new(err));
            }
        };
        Ok(Self { device: DeviceHolder::new(device), kind })
    }

    /// `size` bytes of memory, which hold a filesystem until they are dropped, such as for a
    /// scratch volume or a test.
    ///
    /// # Panics
    ///
    /// If `size` is not a positive multiple of [`Storage::BLOCK_SIZE`].
    pub fn memory(size: u64) -> Self {
        let block_size = Self::BLOCK_SIZE as u64;
        assert!(size > 0 && size.is_multiple_of(block_size), "{size} is not a multiple of blocks");
        let device = FakeDevice::new(size / block_size, Self::BLOCK_SIZE).calm();
        Self { device: DeviceHolder::new(device), kind: Kind::Memory }
    }

    /// Storage in memory on `device`, a test's own: this crate's, and `cladefs_fuse`'s.
    #[cfg(any(test, feature = "testing"))]
    pub fn from_device(device: DeviceHolder) -> Self {
        Self { device, kind: Kind::Memory }
    }

    /// Whether the storage's I/O bypasses the host's page cache: an image's does, or it opens not
    /// at all (TASK-433.6); memory has none to bypass.
    pub fn bypasses_page_cache(&self) -> bool {
        matches!(self.kind, Kind::Image { .. })
    }

    /// cladefs: the cell this storage serves, if it is an image cell (TASK-562, board 2520).
    pub fn cell(&self) -> Option<cladefs_core::CellId> {
        match &self.kind {
            Kind::Image { cell, .. } => cell.upgrade().map(|cell| cell.cell()),
            Kind::Memory => None,
        }
    }

    /// Deletes the image at `path` on `host`.
    pub async fn delete_on(
        host: &Arc<cladefs_core::Host>,
        path: impl AsRef<Path>,
    ) -> Result<(), Error> {
        let path = path.as_ref().to_owned();
        let task_host = host.clone();
        on_bucket_runtime_on(host, async move {
            // As a copy never placed deletes its cell where this process is no host's daemon.
            let bucket =
                local_cell::upload::blocking(|| host_cell::bucket_at_on(&task_host, &path))
                    .map_err(Error::new)?;
            host_cell::delete_on(&task_host, &path, bucket.as_deref(), host_cell::token).await
        })
        .await
    }

    /// Deletes the image at `path`, which nothing in this process has open, a fork or not: its
    /// cell in its bucket, whose collector reclaims what no other cell inherited from it, and
    /// its directory. Its forks live on: their cells are the bucket's, which an image's own
    /// keeps beside its directory, not in it (TASK-165.7). Fails with `ENOENT` if there is none
    /// at `path`.
    #[cfg(not(madsim))]
    pub async fn delete(path: impl AsRef<Path>) -> Result<(), Error> {
        Self::delete_on(cladefs_core::Host::process(), path).await
    }

    /// cladefs: lets go of the storage, which nothing mounts: an image's open cell closed
    /// ([`OpenCell::close`]), its uploader's uploads done and stopped, holding no thread
    /// meanwhile, where dropping it waits on the dropping thread (board 5614).
    pub async fn close(self) {
        let Self { device, kind } = self;
        let cell = match kind {
            Kind::Image { cell, .. } => cell.upgrade(),
            Kind::Memory => None,
        };
        drop(device);
        if let Some(cell) = cell {
            release(cell).await;
        }
    }

    /// cladefs: an image's open cell, for tests of what it holds.
    #[cfg(test)]
    pub fn open_cell(&self) -> Arc<OpenCell> {
        let Kind::Image { cell, .. } = &self.kind else { panic!("memory") };
        cell.upgrade().expect("its device holds it")
    }

    /// cladefs: an image's own bucket's collector in this process, which its device's commits
    /// start passes of, if it keeps its cell in one.
    #[cfg(test)]
    pub fn collecting(&self) -> Option<Arc<collect::Collecting>> {
        let Kind::Image { collecting, .. } = &self.kind else { panic!("memory") };
        collecting.clone()
    }

    /// The most bytes the storage holds.
    pub fn size(&self) -> u64 {
        self.device.block_count() * self.device.block_size() as u64
    }

    /// Checks the image on the storage and every volume in it, failing on any inconsistency, or
    /// anything it warns about, and gives the storage back.
    pub async fn check(self) -> Result<Self, Error> {
        let Self { device, kind } = self;
        let runtime = fxfs_runtime(None);
        let device = runtime.spawn(async move {
            let filesystem = FxFilesystem::open(device).await?;
            // cladefs: what fsck found, which its error says, as well as how many (TASK-82).
            let issues = Mutex::new(Vec::new());
            let options = FsckOptions {
                fail_on_warning: true,
                on_error: Box::new(|issue: &FsckIssue| {
                    issue.log();
                    issues.lock().unwrap().push(issue.clone());
                }),
                ..Default::default()
            };
            let checked = async {
                fsck_with_options(filesystem.clone(), &options).await?;
                let store_ids =
                    list_volumes(root_volume(filesystem.clone()).await?.volume_directory()).await?;
                for store_id in store_ids {
                    fsck_volume_with_options(filesystem.as_ref(), &options, store_id).await?;
                }
                anyhow::Ok(())
            };
            checked.await.with_context(|| format!("fsck found {:?}", issues.lock().unwrap()))?;
            filesystem.close().await?;
            anyhow::Ok(filesystem.take_device().await)
        });
        let device = device.await.expect("fsck task panicked").map_err(Error::new)?;
        Ok(Self::closed(device, kind))
    }

    /// cladefs: the storage's device, for what keeps itself on a device in place of Fxfs, as a
    /// SQLite cell does (crates/sqlite_cladefs, TASK-296). Its user closes it: an image's
    /// close waits for its commits to be uploaded.
    pub fn into_device(self) -> DeviceHolder {
        self.device
    }

    pub fn into_parts(self) -> (DeviceHolder, Kind) {
        (self.device, self.kind)
    }

    /// The storage of `device`, which a filesystem closed.
    pub fn closed(device: DeviceHolder, kind: Kind) -> Self {
        match kind {
            // Closing an image only flushes it.
            Kind::Image { .. } => {}
            // Closing memory makes it refuse I/O until it is reopened.
            Kind::Memory => device.reopen(false),
        }
        Self { device, kind }
    }
}

impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Storage").field("kind", &self.kind).field("size", &self.size()).finish()
    }
}

/// cladefs: `work`, a task of `host`'s bucket runtime.
async fn on_bucket_runtime_on<T: Send + 'static>(
    host: &cladefs_core::Host,
    work: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> Result<T, Error> {
    let runtime = local_cell::upload::bucket_runtime_on(host.local()).map_err(Error::new)?;
    #[cfg(test)]
    let work = {
        let pace = collect::quickened();
        collect::with_quickened(pace, work)
    };
    // cladefs: `unconstrained` prevents coop-budget livelock when polled from `block_on` inside
    // a tokio task (board 6525).
    cladefs_core::bucket::unconstrained(runtime.spawn(work)).await.map_err(Error::new)?
}
