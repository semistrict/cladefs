// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The storage a filesystem lives on: an image, or memory.

mod collect;
mod gate;
mod image;
mod io;
mod kept;
#[cfg(test)]
mod model_tests;

use crate::error::{Error, Result};
use crate::executor::Executor;
use crate::filesystem::DEFAULT_EXECUTOR_THREADS;
use crate::host_cell;
use anyhow::Context as _;
use fxfs::filesystem::FxFilesystem;
use fxfs::fsck::errors::FsckIssue;
use fxfs::fsck::{FsckOptions, fsck_volume_with_options, fsck_with_options};
use fxfs::object_store::volume::{list_volumes, root_volume};
use gate::WriteGate;
use image::ImageDevice;
use kept::KeptImage;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
    /// An image: its directory, the device's writes and frees, which forking the image pauses,
    /// and its store, kept in its bucket, which forking the image forks. Its device's I/O bypasses
    /// the host's page cache, or the image opens not at all (TASK-433.6).
    Image {
        dir: PathBuf,
        writes: Arc<WriteGate>,
        image: Arc<KeptImage>,
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

    /// The image at `path`: a directory holding an extent store (see `image`), a fork's or not,
    /// its cell kept in the bucket its description names (see `kept`).
    ///
    /// Its I/O bypasses the host's page cache where the host filesystem allows, as the in-process
    /// kernel caches files' pages already, and runs on threads of its own (io_uring on Linux).
    /// It fails for an image made by older code, which kept its cell in no bucket, and which
    /// nothing converts.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = path.as_ref().to_owned();
        on_bucket_runtime(async move {
            let writes = Arc::new(WriteGate::default());
            let device = ImageDevice::open(&dir, writes.clone()).await.map_err(Error::new)?;
            Ok(Self::image(dir, writes, device))
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
        // What a making that died part way left is no store: its directory goes, not the cell,
        // which may be another's, as the extent daemon's `CreateCell` has it (TASK-196).
        if host_cell::unmade(path) {
            local_cell::remove_store_dir(path)
                .with_context(|| format!("removing {}, a store unmade", path.display()))
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
            // Closed before it is placed, as `CreateCell` places a store, and opened again.
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
    /// finds no store (TASK-196). As an import makes one, filling it first.
    pub async fn create_unmade(path: impl AsRef<Path>, size: u64) -> Result<Self, Error> {
        Self::make(path.as_ref(), (size, host_cell::DEFAULT_TENANT), false, Self::keeping()?).await
    }

    /// cladefs: a new image at `path`, as [`Storage::create_unmade`] makes it, the one cell of a
    /// new cell of `tenant`'s (TASK-364), its cell kept as `keeping` says: as the extent daemon's
    /// stores keep theirs, with the host's cache of the bucket's objects, for an import the daemon
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

    /// cladefs: whether `path` is what a making of an image, or of a store, left unmade, its maker
    /// dying part way: no store, which deleting finishes (TASK-196).
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
            let writes = Arc::new(WriteGate::default());
            let made = (size, host_cell::CellOf::New(tenant), whole);
            let device = ImageDevice::create(&path, made, keeping, writes.clone()).await;
            let device = device.map_err(Error::new)?;
            Ok(Self::image(path, writes, device))
        })
        .await
    }

    /// The storage of the image at `dir` whose device is `device`, its writes going through
    /// `writes`.
    fn image(dir: PathBuf, writes: Arc<WriteGate>, device: ImageDevice) -> Self {
        let kind = Kind::Image { dir, writes, image: device.image() };
        Self { device: DeviceHolder::new(device), kind }
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
    /// The storage must not be mounted, which it is not while it is the caller's. To fork a
    /// mounted filesystem's, see [`Filesystem::fork_storage_to`](crate::Filesystem::fork_storage_to).
    pub async fn fork_to(&self, path: impl AsRef<Path>) -> Result<Storage, Error> {
        let Kind::Image { image, writes, .. } = &self.kind else {
            return Err(Error::msg("memory cannot be forked to an image"));
        };
        fork_image(image.clone(), writes.clone(), path.as_ref()).await
    }

    /// Deletes the image at `path`, which nothing in this process has open, a fork or not: its
    /// cell in its bucket, whose collector reclaims what no other cell inherited from it, and
    /// its directory. Its forks live on: their cells are the bucket's, which an image's own
    /// keeps beside its directory, not in it (TASK-165.7). Fails with `ENOENT` if there is none
    /// at `path`.
    pub async fn delete(path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref().to_owned();
        on_bucket_runtime(async move { kept::delete(&path).await }).await
    }

    /// cladefs: lets go of the storage, which nothing mounts: an image's uploader stopped first,
    /// its store's open segment closed and what it made durable uploaded, holding no thread
    /// meanwhile, where dropping it waits on the dropping thread (board 5614).
    pub async fn close(self) {
        if let Kind::Image { image, .. } = &self.kind {
            let image = image.clone();
            drop(self);
            image.close().await;
        }
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
                    fsck_volume_with_options(filesystem.as_ref(), &options, store_id, None).await?;
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

/// Forks `image` to a new image at `path`, a fork of its store, and of its cell in its bucket,
/// with a cell of its own ([`KeptImage::fork`]), on a thread of its own, as forking commits the
/// store durably and waits for the fork's upload. The fork pauses `writes`, the device's writes
/// and frees, until the fork's commit is made, so that it is the image as it was at one time.
pub(crate) async fn fork_image(
    image: Arc<KeptImage>,
    writes: Arc<WriteGate>,
    path: &Path,
) -> Result<Storage, Error> {
    let path = path.to_owned();
    let forking = async move {
        let forked = image.fork(&path, writes.pause()).await;
        let context = format!("forking to {}", path.display());
        forked.map(|()| path).map_err(|error| error.context(context))
    };
    let path = on_bucket_runtime(forking).await?;
    Storage::open(path).await
}

/// cladefs: `work`, a task of the bucket runtime's ([`local_cell::upload::bucket_runtime`]): what
/// makes, opens, forks and deletes images awaits the store's timers and the bucket's
/// connections, which that runtime drives, whatever executor awaits it, FUSE's or a test's; as
/// sync calls of theirs did, waiting on it (board 5614).
async fn on_bucket_runtime<T: Send + 'static>(
    work: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> Result<T, Error> {
    let runtime = local_cell::upload::bucket_runtime().map_err(Error::new)?;
    runtime.spawn(work).await.map_err(Error::new)?
}
