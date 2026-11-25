// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An image: a mounted Fxfs filesystem whose volumes are each a [`Filesystem`].
//!
//! Fxfs keeps any number of volumes on one device, which share its journal, its allocator and the
//! records it caches. An [`Image`] mounts the device and creates, lists, opens and deletes its
//! volumes; each open volume is a [`Filesystem`], which unmounts on its own. Closing the image
//! unmounts the volumes still open, and gives the storage back.

use crate::error::{Errno, Error};
use crate::filesystem::{Filesystem, Options};
use crate::storage::{Kind, Storage};
use crate::volume::{HostFxfs, Unmount, Volume};
use fxfs::errors::FxfsError;
use fxfs::filesystem::{FxFilesystemBuilder, OpenFxFilesystem};
use fxfs::lsm_tree::types::{ItemRef, LayerIterator, MergeableKey, Value};
use fxfs::lsm_tree::{LSMTree, Query};
use fxfs::object_store::NewChildStoreOptions;
use fxfs::object_store::allocator::merge::filter_tombstones;
use fxfs::object_store::allocator::{Allocator, AllocatorValue};
use fxfs::object_store::volume::{RootVolume, root_volume};
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use storage_device::DeviceHolder;

/// A mounted Fxfs image, whose named volumes are filesystems of their own.
///
/// Create volumes with [`create_volume`](Self::create_volume), and open them with
/// [`open_volume`](Self::open_volume). The volumes share the image's journal, allocator and
/// executor; each is unmounted when its [`Filesystem`] is, or when the image is closed.
pub struct Image {
    shared: Arc<ImageShared>,
}

/// What an image's volumes share with it.
pub(crate) struct ImageShared {
    runtime: tokio::runtime::Handle,
    pub block_size: u32,
    storage_kind: Kind,
    state: Mutex<State>,
    /// Holds volume creation and mounting to one at a time.
    volumes_lock: futures::lock::Mutex<()>,
}

struct State {
    /// The filesystem, until the image is closed.
    open: Option<Open>,
    /// The volumes mounted, while their filesystems live: closing the image unmounts them, or
    /// waits for their unmounts under way.
    volumes: Vec<Weak<Volume>>,
    /// The stores of the volumes mounted, until their unmounts are done: a volume is mounted once
    /// at a time.
    mounted: HashSet<u64>,
}

struct Open {
    filesystem: OpenFxFilesystem,
    /// The image's root volume, whose directory lists the volumes.
    root_volume: Arc<RootVolume>,
}

/// What compacting an image's trees has cost since it was mounted ([`Filesystem::compactions`]).
/// Fxfs compacts them as its journal fills, merging each tree's layers into new ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compactions {
    /// The allocator's tree's.
    pub allocator: TreeCompactions,
    /// The object stores' trees', those unlocked, together.
    pub stores: TreeCompactions,
}

/// What compacting a tree, or trees, has cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeCompactions {
    /// The compactions.
    pub count: u64,
    /// The bytes they wrote.
    pub bytes: u64,
    /// The time they took.
    pub time: Duration,
}

impl TreeCompactions {
    /// What compacting `tree` has cost.
    fn of<K: MergeableKey, V: Value>(tree: &LSMTree<K, V>) -> Self {
        let counters = tree.compaction_counters();
        Self {
            count: counters.compactions,
            bytes: counters.compaction_bytes_written,
            time: Duration::from_nanos(counters.compaction_time_ns),
        }
    }
}

impl std::ops::Add for TreeCompactions {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            count: self.count + other.count,
            bytes: self.bytes + other.bytes,
            time: self.time + other.time,
        }
    }
}

/// What a volume's metadata takes of its image ([`Filesystem::metadata`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeMetadata {
    /// The size of the layers of the volume's tree, which what its journal holds yet is in none of.
    pub layer_bytes: u64,
    /// The records of the volume's extents in the allocator's tree, which every mount of the image
    /// scans whole.
    pub allocator_records: u64,
}

impl VolumeMetadata {
    /// What the volume `fxfs` serves takes.
    pub(crate) async fn of(fxfs: &HostFxfs) -> Result<Self, anyhow::Error> {
        let filesystem = fxfs.filesystem();
        let store_id = fxfs.volume_id();
        let store = filesystem.object_manager().store(store_id).ok_or(FxfsError::NotFound)?;
        let layer_set = store.tree().layer_set();
        let layers = layer_set.layers.iter().filter_map(|layer| layer.handle());
        let allocator_records = allocator_records(&filesystem.allocator(), store_id).await?;
        Ok(Self { layer_bytes: layers.map(|layer| layer.get_size()).sum(), allocator_records })
    }
}

/// The records in `allocator`'s tree of the extents of the store `store_id`.
async fn allocator_records(allocator: &Allocator, store_id: u64) -> Result<u64, anyhow::Error> {
    let layer_set = allocator.tree().layer_set();
    let mut merger = layer_set.merger();
    let mut records = filter_tombstones(merger.query(Query::FullScan).await?).await?;
    let mut count = 0;
    while let Some(ItemRef { value, .. }) = records.get() {
        if matches!(value, AllocatorValue::Abs { owner_object_id, .. }
            if *owner_object_id == store_id)
        {
            count += 1;
        }
        records.advance().await?;
    }
    Ok(count)
}

impl Image {
    /// Mounts the image on `storage` with the default [`Options`].
    pub async fn mount(storage: Storage) -> Result<Self, Error> {
        Self::start(&Options::new(), storage, false).await
    }

    /// Formats `storage` with an empty image, which has no volumes, and mounts it with the default
    /// [`Options`].
    pub async fn format(storage: Storage) -> Result<Self, Error> {
        Self::start(&Options::new(), storage, true).await
    }

    pub(crate) async fn start(
        options: &Options,
        storage: Storage,
        format: bool,
    ) -> Result<Self, Error> {
        let (device, storage_kind) = storage.into_parts();
        let barriers = matches!(storage_kind, Kind::Image { .. });
        let runtime = options.fxfs_runtime();
        let open = runtime
            .spawn(async move {
                let filesystem = FxFilesystemBuilder::new()
                    .format(format)
                    .barriers_enabled(barriers)
                    .open(device)
                    .await?;
                let root_volume = Arc::new(root_volume(filesystem.clone()).await?);
                anyhow::Ok(Open { filesystem, root_volume })
            })
            .await
            .expect("Fxfs open task panicked")
            .map_err(Error::new)?;
        let block_size = open.filesystem.block_size().get() as u32;
        let state =
            Mutex::new(State { open: Some(open), volumes: Vec::new(), mounted: HashSet::new() });
        let volumes_lock = futures::lock::Mutex::new(());
        let shared = ImageShared { runtime, block_size, storage_kind, state, volumes_lock };
        Ok(Self { shared: Arc::new(shared) })
    }

    /// Creates the volume `name`, which must not exist, and mounts it.
    pub async fn create_volume(&self, name: &str) -> Result<Filesystem, Error> {
        self.shared.mount_volume(name, true).await
    }

    /// Mounts the volume `name`, which must not be mounted.
    pub async fn open_volume(&self, name: &str) -> Result<Filesystem, Error> {
        self.shared.mount_volume(name, false).await
    }

    /// Compacts the journal into the volumes' layers: a mount then has none of it to replay, and
    /// no volume to open for it.
    pub async fn compact(&self) -> Result<(), Error> {
        let filesystem = self.shared.with_open(|open| open.filesystem.clone())?;
        self.shared
            .run(async move { filesystem.journal().force_compact().await })
            .await
            .map_err(Error::new)
    }

    /// The names of the image's volumes.
    pub async fn volumes(&self) -> Result<Vec<String>, Error> {
        let root_volume = self.shared.with_open(|open| open.root_volume.clone())?;
        self.shared
            .run(async move {
                let directory = root_volume.volume_directory();
                let layer_set = directory.store().tree().layer_set();
                let mut merger = layer_set.merger();
                let mut entries = directory.iter(&mut merger).await?;
                let mut names = Vec::new();
                while let Some((name, _, _)) = entries.get() {
                    names.push(name.to_owned());
                    entries.advance().await?;
                }
                anyhow::Ok(names)
            })
            .await
            .map_err(Error::new)
    }

    /// Unmounts the volumes still mounted, writing back what is dirty, closes the image, and gives
    /// its storage back.
    pub async fn close(self) -> Result<Storage, Error> {
        // Once closed, dropping the image does nothing.
        self.shared.close().await
    }
}

impl Drop for Image {
    /// Closes the image, unmounting its volumes, blocking the dropping thread until it is done.
    /// Await [`close`](Image::close) rather than dropping an image.
    fn drop(&mut self) {
        let Some(closing) = self.shared.begin_close() else {
            return;
        };
        if self.shared.on_own_current_thread() && std::thread::panicking() {
            log::error!(
                "cladefs: skipping blocking image close during unwind on current_thread runtime"
            );
            return;
        }
        if let Err(error) = self.shared.run_blocking(closing) {
            log::error!("cladefs: failed to close the image: {error:#}");
        }
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let open = self.shared.state.lock().unwrap().open.is_some();
        f.debug_struct("Image").field("open", &open).finish_non_exhaustive()
    }
}

impl ImageShared {
    /// Spawns `future` on this image's Tokio runtime.
    pub(crate) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(future);
    }

    /// Runs `future` on this image's Tokio runtime; dropping the returned future detaches the task
    /// so operations run to completion.
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> T {
        self.runtime.spawn(future).await.expect("Fxfs task panicked or cancelled")
    }

    /// Whether the current thread is executing this image's own `current_thread` Tokio runtime
    /// (where blocking on a task spawned onto `self.runtime` would deadlock).
    pub(crate) fn on_own_current_thread(&self) -> bool {
        #[cfg(madsim)]
        return false;
        #[cfg(not(madsim))]
        matches!(
            tokio::runtime::Handle::try_current(),
            Ok(current)
                if current.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread
                    && current.id() == self.runtime.id()
        )
    }

    /// Runs `future` on this image's Tokio runtime, blocking the caller thread (for `Drop`).
    ///
    /// On a multi-thread Tokio worker, `block_in_place` runs before spawning so the task lands on
    /// the global queue rather than this worker's unstealable LIFO slot and cannot starve the
    /// shared runtime even under concurrent drops (hotbed reviews 111 and 112).
    pub(crate) fn run_blocking<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> T {
        local_cell::blocks!("waiting for an Fxfs task, an outermost entry's");
        let run = || {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            self.runtime.spawn(async move {
                // Its waiter gone: nothing to tell.
                let _ = sender.send(future.await);
            });
            receiver.recv().expect("Fxfs task dropped unfinished")
        };
        #[cfg(madsim)]
        return run();
        #[cfg(not(madsim))]
        match tokio::runtime::Handle::try_current() {
            Ok(current)
                if current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
            {
                #[allow(
                    clippy::disallowed_methods,
                    reason = "synchronous Drop cannot await; block_in_place yields the worker core before spawning so teardown runs on another worker without starving the shared runtime"
                )]
                tokio::task::block_in_place(run)
            }
            Ok(current) if current.id() == self.runtime.id() => {
                panic!(
                    "cladefs: dropped a mounted Filesystem or open Image on its own \
                     current_thread runtime; await unmount() or close() first"
                );
            }
            _ => run(),
        }
    }

    /// Runs `f` on the open filesystem, or fails with `ENOTCONN` once the image is closed.
    fn with_open<T>(&self, f: impl FnOnce(&Open) -> T) -> Result<T, Errno> {
        self.state.lock().unwrap().open.as_ref().map(f).ok_or(Errno::ENOTCONN)
    }

    /// What compacting the image's trees has cost since it was mounted, until it is closed.
    pub fn compactions(&self) -> Result<Compactions, Errno> {
        self.with_open(|open| {
            let object_manager = open.filesystem.object_manager();
            Compactions {
                allocator: TreeCompactions::of(object_manager.allocator().tree()),
                stores: object_manager
                    .stores()
                    .iter()
                    .map(|store| TreeCompactions::of(store.tree()))
                    .fold(TreeCompactions::default(), std::ops::Add::add),
            }
        })
    }

    /// Notes that the unmount of the volume whose store is `store_id` is done: it may be mounted
    /// again.
    pub fn unmounted(&self, store_id: u64) {
        self.state.lock().unwrap().mounted.remove(&store_id);
    }

    /// Creates the volume `name`, or mounts it, and serves it as a filesystem.
    async fn mount_volume(self: &Arc<Self>, name: &str, create: bool) -> Result<Filesystem, Error> {
        let _volumes = self.volumes_lock.lock().await;
        let root_volume = self.with_open(|open| open.root_volume.clone())?;
        let store = self
            .run({
                let name = name.to_owned();
                async move {
                    if create {
                        root_volume.new_volume(&name, NewChildStoreOptions::default()).await
                    } else {
                        root_volume.volume(&name).await
                    }
                }
            })
            .await
            .map_err(volume_error)?;
        {
            let mut state = self.state.lock().unwrap();
            // Its volumes' unmounts begin as the image closes, which it does no more once
            // closed.
            if state.open.is_none() {
                return Err(Errno::ENOTCONN.into());
            }
            if !state.mounted.insert(store.store_object_id()) {
                return Err(Errno::EBUSY.into());
            }
        }
        let filesystem = Filesystem::new(self.clone(), store);
        let mut state = self.state.lock().unwrap();
        state.volumes.retain(|volume| volume.strong_count() > 0);
        state.volumes.push(Arc::downgrade(filesystem.volume()));
        Ok(filesystem)
    }

    /// Begins unmounting `volume`, one of the image's, unless that has begun. Under the image's
    /// lock, so that the image doesn't close meanwhile, as its close begins its volumes' unmounts
    /// under it.
    pub fn unmount_volume(&self, volume: &Arc<Volume>) -> Unmount {
        let _state = self.state.lock().unwrap();
        volume.begin_unmount()
    }

    /// Unmounts the volumes still mounted, closes the filesystem, and gives the storage back.
    pub async fn close(&self) -> Result<Storage, Error> {
        let closing = self.begin_close().ok_or(Errno::ENOTCONN)?;
        let device = self.run(closing).await.map_err(Error::new)?;
        Ok(Storage::closed(device, self.storage_kind.clone()))
    }

    /// Begins closing the image, unless it is closed, and returns the future that unmounts its
    /// volumes and closes the filesystem on Fxfs's executor, giving its device back.
    fn begin_close(
        &self,
    ) -> Option<impl Future<Output = anyhow::Result<DeviceHolder>> + Send + 'static> {
        // The volumes' unmounts begin as the image's filesystem is taken, under one hold of the
        // lock: those under way already are waited for.
        let (Open { filesystem, root_volume }, unmounts) = {
            let mut state = self.state.lock().unwrap();
            let open = state.open.take()?;
            let volumes = std::mem::take(&mut state.volumes);
            let unmounts: Vec<Unmount> = volumes
                .iter()
                .filter_map(Weak::upgrade)
                .map(|volume| volume.begin_unmount())
                .collect();
            (open, unmounts)
        };
        Some(async move {
            for unmount in unmounts {
                if let Err(error) = unmount.wait().await {
                    log::error!("cladefs: failed to unmount a volume: {error:#}");
                }
            }
            drop(root_volume);
            filesystem.close().await?;
            anyhow::Ok(filesystem.take_device().await)
        })
    }
}

/// The error of a volume operation, with the errno of the Fxfs error causing it, if one did.
fn volume_error(error: anyhow::Error) -> Error {
    match error.chain().any(|cause| cause.downcast_ref::<FxfsError>().is_some()) {
        true => {
            let errno = fxfs_posix::errno(&error);
            Error::new(error.context(errno))
        }
        false => Error::new(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_backed_image_uses_atomic_barriers() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("image");
            let storage = Storage::create(&path, 32 << 20).await.unwrap();
            let check_barriers = |image: &Image| {
                image
                    .shared
                    .with_open(|open| {
                        assert!(open.filesystem.options().barriers_enabled);
                        assert!(open.filesystem.device().atomic_commits());
                    })
                    .unwrap();
            };
            let image = Image::format(storage).await.unwrap();
            check_barriers(&image);
            let volume = image.create_volume("data").await.unwrap();
            let owner = crate::Owner { uid: 1000, gid: 100 };
            let (file, _) = volume.root().create(b"file", 0o644, owner).await.unwrap();
            let data: Vec<_> = (0..8192).map(|i| (i % 251) as u8).collect();
            assert_eq!(file.write(0, &data).await, Ok(data.len()));
            file.fsync().await.unwrap();
            drop((file, volume));
            drop(image.close().await.unwrap());

            let image = Image::mount(Storage::open(&path).await.unwrap()).await.unwrap();
            check_barriers(&image);
            let volume = image.open_volume("data").await.unwrap();
            let (file, _) = volume.root().lookup(b"file").await.unwrap();
            let mut read = vec![0; data.len()];
            assert_eq!(file.read(0, &mut read).await, Ok(data.len()));
            assert_eq!(read, data);
            drop((file, volume));
            image.close().await.unwrap();
        });
    }
}
