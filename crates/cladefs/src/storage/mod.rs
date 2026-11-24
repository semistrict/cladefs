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

mod collect;
mod device;
#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod tests;

use crate::error::{Error, Result};
use crate::executor::Executor;
use crate::filesystem::DEFAULT_EXECUTOR_THREADS;
use crate::host_cell::{self, Opened};
use crate::open_cell::{OpenCell, Opening, release};
use anyhow::Context as _;
use device::OpenCellDevice;
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
pub(crate) enum Kind {
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

    /// The image at `path`: a cell directory, a fork's or not, its cell kept in the bucket its
    /// description names ([`host_cell::open`]).
    ///
    /// Its cell's files bypass the host's page cache where the host filesystem allows, as the
    /// cell's page cache, and the in-process kernel's, cache their pages already (one memory
    /// budget, TASK-433). Its uploader takes its cell's head as a copy never placed takes it where
    /// this process is no host's daemon ([`host_cell::keeping`]). It fails for an image made by
    /// older code, which kept its cell in no bucket, and which nothing converts.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = path.as_ref().to_owned();
        on_bucket_runtime(async move {
            let keeping = Self::keeping()?;
            let opened = host_cell::open(&dir, keeping.clone()).await;
            let context = || format!("opening {}", dir.display());
            let opened = opened.map_err(|err| Error::from(err).context(context()))?;
            Self::opened(dir, opened, keeping).await
        })
        .await
    }

    /// A new image at `path`, which must not exist, holding at most `size` bytes. It holds nothing
    /// until the filesystem writes to it, and must be formatted before it is mounted. Its cell
    /// is kept in a file bucket of its own, beside its directory (`<image>.bucket`), which its
    /// forks share and which outlives it; or, on a host of cladefs's system, in the host's bucket
    /// (`CLADEFSD_BUCKET`).
    pub async fn create(path: impl AsRef<Path>, size: u64) -> Result<Self, Error> {
        Self::make(path.as_ref(), (size, host_cell::DEFAULT_TENANT), true, Self::keeping()?).await
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
    pub async fn take(
        path: impl AsRef<Path>,
        (of, capacity): (host_cell::CellOf, u64),
        bucket: &str,
        token: Option<u64>,
    ) -> Result<Self, Error> {
        let (path, bucket) = (path.as_ref().to_owned(), bucket.to_owned());
        on_bucket_runtime(Self::taken(path, (of, capacity), bucket, token)).await
    }

    /// [`Storage::take`]'s, on the bucket runtime.
    async fn taken(
        path: PathBuf,
        (of, capacity): (host_cell::CellOf, u64),
        bucket: String,
        token: Option<u64>,
    ) -> Result<Self, Error> {
        let (path, bucket) = (path.as_path(), bucket.as_str());
        // What a making that died part way left is no cell: its directory goes, not the bucket's
        // cell, which may be another's, as the extent daemon's `CreateCell` has it (TASK-196).
        if host_cell::unmade(path) {
            local_cell::remove_cell_dir(path)
                .with_context(|| format!("removing {}, a cell directory unmade", path.display()))
                .map_err(Error::new)?;
        }
        if !path.exists() {
            let keeping = Self::keeping()?;
            let keeping =
                local_cell::upload::Options { token: token.unwrap_or(keeping.token), ..keeping };
            let kept = (bucket_cell::cell::Kind::Fxfs, Some(bucket), keeping);
            let making = host_cell::create_unmade(path, capacity, of, kept.0, kept.1, kept.2);
            let opened = making.await?;
            opened.made().map_err(Error::new)?;
            // Closed before it is placed, as `CreateCell` places a cell, and opened again.
            opened.close().await;
            if let Some(token) = token {
                local_cell::place_host(path, token, false).map_err(Error::new)?;
            }
        } else {
            let found = local_cell::host_cell(path).map_err(Error::new)?;
            let not = match of {
                host_cell::CellOf::Taken(cell) if found == cell => None,
                host_cell::CellOf::Taken(cell) => Some(format!("{cell}")),
                _ => Some("a new cell".to_owned()),
            };
            if let Some(not) = not {
                let error = anyhow::anyhow!("{} holds cell {found}, not {not}", path.display());
                return Err(Error::new(error.context(crate::Errno::EEXIST)));
            }
        }
        Self::open(path).await
    }

    /// cladefs: a new image at `path`, as [`Storage::create`] makes it, unmade until its maker says
    /// it is whole ([`Storage::made`]): what opens it before, or after its maker died part way,
    /// finds no cell (TASK-196). As an import makes one, filling it first.
    pub async fn create_unmade(path: impl AsRef<Path>, size: u64) -> Result<Self, Error> {
        Self::make(path.as_ref(), (size, host_cell::DEFAULT_TENANT), false, Self::keeping()?).await
    }

    /// cladefs: a new image at `path`, as [`Storage::create_unmade`] makes it, the one cell of a
    /// new cell of `tenant`'s (TASK-364), its cell kept as `keeping` says: as the extent daemon's
    /// cells keep theirs, with the host's cache of the bucket's objects, for an import the daemon
    /// makes (TASK-237).
    pub(crate) async fn create_unmade_keeping(
        path: impl AsRef<Path>,
        (size, tenant): (u64, u64),
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        Self::make(path.as_ref(), (size, tenant), false, keeping).await
    }

    /// How an image this process makes, where no host's daemon serves it, keeps its cell
    /// ([`host_cell::keeping`]): with no cache of the bucket's objects.
    fn keeping() -> Result<local_cell::upload::Options, Error> {
        host_cell::keeping().map_err(Error::new)
    }

    /// cladefs: the image at `path`, made by [`Storage::create_unmade`] and closed since, is whole.
    pub fn made(path: impl AsRef<Path>) -> Result<(), Error> {
        local_cell::made_host(path.as_ref()).map_err(Error::new)
    }

    /// cladefs: whether `path` is what a making of an image, or of a cell, left unmade, its maker
    /// dying part way: no cell, which deleting finishes (TASK-196).
    pub fn is_unmade(path: impl AsRef<Path>) -> bool {
        host_cell::unmade(path.as_ref())
    }

    /// A new image at `path`, as [`Storage::create`] makes it, a new cell of
    /// `tenant`'s, whole if `whole`, its cell kept as `keeping` says.
    async fn make(
        path: &Path,
        (size, tenant): (u64, u64),
        whole: bool,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        if size < Self::BLOCK_SIZE as u64 {
            return Err(Error::msg("the image is smaller than a block"));
        }
        let path = path.to_owned();
        on_bucket_runtime(async move {
            let of = host_cell::CellOf::New(tenant);
            let kind = bucket_cell::cell::Kind::Fxfs;
            let opened = host_cell::create_unmade(&path, size, of, kind, None, keeping.clone());
            let opened = opened.await?;
            if whole {
                opened.made().map_err(Error::new)?;
            }
            Self::opened(path, opened, keeping).await
        })
        .await
    }

    /// The storage of the image `opened` at `dir`, its extents in its bucket read through
    /// `keeping`'s cache, if it has one: an open cell of it, for work that boots no guest
    /// ([`Opening::guest`]), as its device; and the collector of its own bucket, if it keeps its
    /// cell in one.
    async fn opened(
        dir: PathBuf,
        opened: Opened,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        let collecting = host_cell::keeps_own_bucket(&opened.local).then(|| {
            let url = opened.local.bucket().unwrap_or_default().to_owned();
            collect::of(&url, &opened.bucket)
        });
        let opening = Opening { arena: None, guest: false, keeping };
        let cell = Arc::new(OpenCell::new(&dir, opened, opening).await?);
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

    /// Whether the storage's I/O bypasses the host's page cache: an image's does, or it opens not
    /// at all (TASK-433.6); memory has none to bypass.
    pub fn bypasses_page_cache(&self) -> bool {
        matches!(self.kind, Kind::Image { .. })
    }

    /// Forks the image to a new image at `path`, which must not exist, and returns its storage.
    /// The fork's cell is a fork of the image's in its bucket, which the fork keeps its own
    /// cell in: it takes the same space whatever the image holds, inheriting the image's
    /// extents rather than copies, read from the bucket (TASK-165.7). `path` is the fork's own
    /// directory. Memory cannot be forked.
    ///
    /// The storage must not be mounted, which it is not while it is the caller's. To fork a mounted
    /// filesystem's, see [`Filesystem::fork_storage_to`](crate::Filesystem::fork_storage_to).
    pub async fn fork_to(&self, path: impl AsRef<Path>) -> Result<Storage, Error> {
        fork_image(&self.kind, path.as_ref()).await
    }

    /// Deletes the image at `path`, which nothing in this process has open, a fork or not: its
    /// cell in its bucket, whose collector reclaims what no other cell inherited from it, and
    /// its directory. Its forks live on: their cells are the bucket's, which an image's own
    /// keeps beside its directory, not in it (TASK-165.7). Fails with `ENOENT` if there is none
    /// at `path`.
    pub async fn delete(path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref().to_owned();
        on_bucket_runtime(async move {
            // As a copy never placed deletes its cell where this process is no host's daemon.
            let bucket = host_cell::bucket_at(&path).map_err(Error::new)?;
            host_cell::delete(&path, bucket.as_deref(), host_cell::token).await
        })
        .await
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
    pub(crate) fn open_cell(&self) -> Arc<OpenCell> {
        let Kind::Image { cell, .. } = &self.kind else { panic!("memory") };
        cell.upgrade().expect("its device holds it")
    }

    /// cladefs: an image's own bucket's collector in this process, which its device's commits
    /// start passes of, if it keeps its cell in one.
    #[cfg(test)]
    pub(crate) fn collecting(&self) -> Option<Arc<collect::Collecting>> {
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
        let executor = Executor::new(DEFAULT_EXECUTOR_THREADS);
        let device = executor.run(async move {
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
        Ok(Self::closed(device.await.map_err(Error::new)?, kind))
    }

    /// cladefs: the storage's device, for what keeps itself on a device in place of Fxfs, as a
    /// SQLite cell does (crates/sqlite_cladefs, TASK-296). Its user closes it: an image's
    /// close waits for its commits to be uploaded.
    pub fn into_device(self) -> DeviceHolder {
        self.device
    }

    pub(crate) fn into_parts(self) -> (DeviceHolder, Kind) {
        (self.device, self.kind)
    }

    /// The storage of `device`, which a filesystem closed.
    pub(crate) fn closed(device: DeviceHolder, kind: Kind) -> Self {
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

/// Forks the image of storage of `kind` to a new image at `path`, a fork of its cell directory,
/// and of its cell in its bucket, to a new cell of its tenant's ([`OpenCell::fork`]): through a
/// version of its last commit, as of one time, its writes after the version's commit its own.
/// Memory cannot be forked; nor an image closed (`ENOTCONN`).
pub(crate) async fn fork_image(kind: &Kind, path: &Path) -> Result<Storage, Error> {
    let Kind::Image { cell, .. } = kind else {
        return Err(Error::msg("memory cannot be forked to an image"));
    };
    let cell = cell.upgrade().ok_or(crate::Errno::ENOTCONN)?;
    let path = path.to_owned();
    let forking = async move {
        let forked = cell.fork(&path, cell.cell().tenant).await;
        let context = format!("forking to {}", path.display());
        forked.map(|_| path).map_err(|error| error.context(context))
    };
    let path = on_bucket_runtime(forking).await?;
    Storage::open(path).await
}

/// cladefs: `work`, a task of the bucket runtime's ([`local_cell::upload::bucket_runtime`]): what
/// makes, opens, forks and deletes images awaits the cell's timers and the bucket's
/// connections, which that runtime drives, whatever executor awaits it, FUSE's or a test's; as
/// sync calls of theirs did, waiting on it (board 5614).
async fn on_bucket_runtime<T: Send + 'static>(
    work: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> Result<T, Error> {
    let runtime = local_cell::upload::bucket_runtime().map_err(Error::new)?;
    #[cfg(test)]
    let work = {
        let pace = collect::quickened();
        collect::with_quickened(pace, work)
    };
    // cladefs: `unconstrained` prevents coop-budget livelock when polled from `block_on` inside
    // a tokio task (board 6525).
    crate::bucket::unconstrained(runtime.spawn(work)).await.map_err(Error::new)?
}
