// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A filesystem: an Fxfs `"default"` volume served directly through Fxfs's POSIX layer
//! (`crates/fxfs-posix`), whose nodes are reached through handles ([`Node`]).
//!
//! The POSIX layer is the guest kernel's too, whose VFS checks what it can before calling it,
//! keeps its inodes, and locks its directories' names while it changes them. The host's
//! filesystem does as the VFS does (`Inner::namespace`, `HeldNode`).
//!
//! Operations run inline on the caller's future while entering the filesystem's Tokio runtime
//! handle (`EnterFuture`), avoiding per-operation task spawns and per-read/write bounce copies
//! (board 11691). Unmounting runs to completion on the runtime handle, waiting for in-flight
//! operations, evicting held nodes, closing the `"default"` volume and `OpenFxFilesystem`, and
//! returning the underlying [`Storage`].

use crate::node::*;
use crate::storage::{Kind, Storage};
use cladefs_core::error::{Errno, Error, Result};
use cladefs_core::memory::{VolatileSlice, VolatileSliceMut};
use event_listener::Event;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::Shared;
use fxfs::errors::FxfsError;
use fxfs::filesystem::{FxFilesystemBuilder, OpenFxFilesystem, SyncOptions};
use fxfs::lsm_tree::LSMTree;
use fxfs::lsm_tree::Query;
use fxfs::lsm_tree::types::{ItemRef, LayerIterator, MergeableKey, Value};
use fxfs::object_store::allocator::merge::filter_tombstones;
use fxfs::object_store::allocator::{Allocator, AllocatorValue};
use fxfs::object_store::volume::root_volume;
use fxfs::object_store::{NewChildStoreOptions, SetExtendedAttributeMode};
use fxfs_posix::{Fxfs, Platform, ReaddirStart};
use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroU8;
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

/// cladefs: counts entries emitted by `fxfs.readdir` across `Node::entries` and
/// `Node::entries_page` for tests verifying bounded streaming `READDIR` (TASK-619, board 2634).
#[cfg(test)]
pub static READDIR_EMITS: AtomicU64 = AtomicU64::new(0);

/// Where a bounded directory listing (`Node::entries_page`) begins (TASK-619, board 2634).
#[derive(Clone, Copy, Debug)]
pub enum EntriesStart<'a> {
    Index(u64),
    After(&'a [u8]),
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

/// What the host gives Fxfs's POSIX layer: the host's VFS keeps nothing the layer writes back,
/// and its devices make each commit durable as they write it.
struct HostPlatform;

impl Platform for HostPlatform {
    const KEEPS_DIRECTORY_TIMES: bool = false;

    type Durable = ();

    fn durable(&self) {}

    fn spawn_urgent(&self, future: impl Future<Output = ()> + Send + 'static) {
        fuchsia_async::Task::spawn(future).detach();
    }
}

type HostFxfs = Fxfs<HostPlatform>;

/// Polls `future` with `handle` entered so Fxfs's background task spawns (`fuchsia_async`) and
/// Tokio timers/primitives work even when driven by `futures::executor::block_on` (board 11691).
struct EnterFuture<F> {
    #[cfg_attr(madsim, allow(dead_code))]
    handle: tokio::runtime::Handle,
    future: F,
}

impl<F: Future> Future for EnterFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural projection to the pinned `future` field; `handle` is never pinned.
        let (handle, future) = unsafe {
            let this = self.get_unchecked_mut();
            (&this.handle, Pin::new_unchecked(&mut this.future))
        };
        #[cfg(not(madsim))]
        {
            let already_mt = matches!(
                tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
                Ok(tokio::runtime::RuntimeFlavor::MultiThread)
            );
            let _guard = handle.enter();
            if already_mt {
                future.poll(cx)
            } else {
                local_cell::upload::without_block_in_place(|| future.poll(cx))
            }
        }
        #[cfg(madsim)]
        {
            let _ = handle;
            future.poll(cx)
        }
    }
}

fn enter<F: Future>(handle: &tokio::runtime::Handle, future: F) -> EnterFuture<F> {
    EnterFuture { handle: handle.clone(), future }
}

/// The errno of `error`, an Fxfs operation's, as Fxfs's VFSs give it (`fxfs_posix::errno`).
fn errno(error: anyhow::Error) -> Errno {
    fxfs_posix::errno(&error)
}

/// The errno of `error`, an extended attribute's operation's (`fxfs_posix::xattr_errno`).
fn xattr_errno(error: anyhow::Error) -> Errno {
    fxfs_posix::xattr_errno(&error)
}

/// The error of an operation on volumes or the whole filesystem, with the context Fxfs gave it
/// and the errno of the Fxfs error causing it, if one did.
fn volume_error(error: anyhow::Error) -> Error {
    match error.chain().any(|cause| cause.downcast_ref::<FxfsError>().is_some()) {
        true => {
            let errno = fxfs_posix::errno(&error);
            Error::new(error.context(errno))
        }
        false => Error::new(error),
    }
}

/// The longest name a directory holds, as Linux's `NAME_MAX`.
pub const NAME_MAX: usize = 255;

/// How much of a write Fxfs writes in a transaction at most: a write longer is written a piece at
/// a time.
const WRITE_CHUNK: usize = 1 << 20;

/// `name`, a name of a directory's: valid, which `.`, `..`, and those holding `/` or NUL are not,
/// and in UTF-8, as Fxfs keeps names.
pub fn valid_name(name: &[u8]) -> Result<String> {
    if name.len() > NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    if matches!(name, b"" | b"." | b"..") || name.iter().any(|&byte| byte == b'/' || byte == 0) {
        return Err(Errno::EINVAL);
    }
    Ok(std::str::from_utf8(name).map_err(|_| Errno::EINVAL)?.to_owned())
}

/// The attributes of the node `name` names in the directory `dir`, if it names one.
async fn find(fxfs: &HostFxfs, dir: u64, name: &str) -> Result<Option<fxfs_posix::Attr>> {
    match fxfs.lookup(dir, name.as_bytes()).await {
        Ok(attr) => Ok(Some(attr)),
        Err(error) if FxfsError::NotFound.matches(&error) => Ok(None),
        Err(error) => Err(errno(error)),
    }
}

/// The type of the node whose attributes are `attr`.
fn type_of(attr: &fxfs_posix::Attr) -> Result<FileType> {
    FileType::from_mode(attr.mode).ok_or(Errno::EIO)
}

/// The attributes of a node, from Fxfs's, on an image of `block_size` blocks.
fn attr(block_size: u32, attr: &fxfs_posix::Attr) -> Result<Attr> {
    Ok(Attr {
        inode: Inode(attr.ino),
        size: attr.size,
        blocks: attr.blocks,
        atime: time_from_nanos(attr.atime_ns),
        mtime: time_from_nanos(attr.mtime_ns),
        ctime: time_from_nanos(attr.ctime_ns),
        file_type: type_of(attr)?,
        perm: (attr.mode & PERM_MASK) as u16,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev as u32,
        blksize: block_size,
    })
}

/// The time of a change at `now` of a node's status alone: its status change time.
fn status_changed_at(now: u64) -> fxfs_posix::SetAttr {
    fxfs_posix::SetAttr { valid: fxfs_posix::SET_CTIME, ctime_ns: now, ..Default::default() }
}

/// Now, in nanoseconds since the Unix epoch.
fn now() -> Result<u64> {
    nanos_from_time(std::time::SystemTime::now())
}

/// The name of the single volume a host filesystem serves.
pub const VOLUME_NAME: &str = "default";

/// Returns a Tokio runtime handle for Fxfs tasks: the caller-supplied handle if any; under
/// `madsim`, the current simulated host runtime; otherwise one shared single-threaded runtime
/// lazily created for standalone callers (`Storage::check`, and
/// tests driving futures with `futures::executor::block_on`) (board 11689, board 11679).
pub fn fxfs_runtime(handle: Option<&tokio::runtime::Handle>) -> tokio::runtime::Handle {
    if let Some(handle) = handle {
        return handle.clone();
    }
    #[cfg(madsim)]
    {
        tokio::runtime::Handle::current()
    }
    #[cfg(not(madsim))]
    {
        // Outside the simulation: one shared single-threaded runtime for standalone Fxfs images
        // when no caller runtime is supplied (avoids spawning a runtime + OS thread per image).
        static SHARED: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("cladefs fxfs")
                    .enable_all()
                    .build()
                    .expect("failed to start cladefs fxfs runtime")
            })
            .handle()
            .clone()
    }
}

/// What a volume takes of its filesystem's metadata ([`Filesystem::metadata`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolumeMetadata {
    /// The size of the layers of the volume's tree, which what its journal holds yet is in none of.
    pub layer_bytes: u64,
    /// The records of the volume's extents in the allocator's tree, which every mount of the
    /// filesystem scans whole.
    pub allocator_records: u64,
}

/// The records in `allocator`'s tree of the extents of the store `store_id`.
async fn allocator_records(allocator: &Allocator, store_id: u64) -> anyhow::Result<u64> {
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

/// How a filesystem is mounted.
#[derive(Clone, Debug)]
pub struct Options {
    runtime: Option<tokio::runtime::Handle>,
    volume: String,
}

impl Options {
    /// The default options (opens the `"default"` volume).
    pub fn new() -> Self {
        Self { runtime: None, volume: VOLUME_NAME.to_owned() }
    }

    /// Selects the named volume to open or create instead of [`VOLUME_NAME`] (`"default"`).
    pub fn volume(&mut self, name: impl Into<String>) -> &mut Self {
        self.volume = name.into();
        self
    }

    /// Runs Fxfs's tasks on `runtime` instead of the shared fallback runtime (board 11689).
    #[cfg_attr(madsim, allow(dead_code))]
    pub fn runtime(&mut self, runtime: tokio::runtime::Handle) -> &mut Self {
        self.runtime = Some(runtime);
        self
    }

    /// Kept for compatibility with callers that previously sized a per-image executor; Fxfs
    /// filesystems now share the caller's runtime or the shared fallback runtime (board 11689).
    pub fn executor_threads(&mut self, _threads: NonZeroU8) -> &mut Self {
        self
    }

    pub fn fxfs_runtime(&self) -> tokio::runtime::Handle {
        fxfs_runtime(self.runtime.as_ref())
    }

    /// Mounts the filesystem on `storage`, which [`format`](Self::format) formatted.
    pub async fn mount(&self, storage: Storage) -> Result<Filesystem, Error> {
        Filesystem::start(self, storage, false).await
    }

    /// Formats `storage` with an empty filesystem, and mounts it.
    pub async fn format(&self, storage: Storage) -> Result<Filesystem, Error> {
        Filesystem::start(self, storage, true).await
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared state of a mounted host Fxfs `"default"` filesystem (collapses former `ImageShared` +
/// `Volume` + `Filesystem::Backing` into one layer, board 11691).
struct Inner {
    runtime: tokio::runtime::Handle,
    block_size: u32,
    storage_kind: Kind,
    /// Locked while a name in the filesystem changes, and shared while one is looked up, as a VFS
    /// locks a directory's names so lookups hold their node before an unlink/rename can evict it.
    namespace: tokio::sync::RwLock<()>,
    /// In-flight operations, which unmount waits for before closing the volume and filesystem.
    operations: Arc<Operations>,
    /// Set once unmount + filesystem close has finished so `Drop` returns immediately without
    /// blocking on already-unmounted/closed filesystems.
    finished: AtomicBool,
    state: Mutex<State>,
}

struct State {
    mounted: Option<Mounted>,
    unmounting: Option<Shared<oneshot::Receiver<()>>>,
    closed: Option<Storage>,
}

impl State {
    fn mounted(&mut self) -> Result<&mut Mounted> {
        match self.unmounting {
            None => self.open(),
            Some(_) => Err(Errno::ENOTCONN),
        }
    }

    fn open(&mut self) -> Result<&mut Mounted> {
        self.mounted.as_mut().ok_or(Errno::ENOTCONN)
    }
}

struct Mounted {
    filesystem: OpenFxFilesystem,
    fxfs: Arc<HostFxfs>,
    nodes: HashMap<u64, HeldNode>,
}

impl Mounted {
    fn held(&mut self, object_id: u64) -> &mut HeldNode {
        self.nodes.get_mut(&object_id).expect("a node is held while a hold on it lives")
    }

    fn release_child(&mut self, parent: u64) {
        let held = self.held(parent);
        held.children -= 1;
        if held.holds == 0 && held.children == 0 {
            self.let_go(parent);
        }
    }

    /// Lets go of `object_id` and any parent directory it alone kept alive. Iterative so releasing
    /// a deep directory chain never overflows the stack (TASK-541, board 2478).
    fn let_go(&mut self, object_id: u64) {
        let mut next = Some(object_id);
        while let Some(id) = next {
            let held = self.nodes.remove(&id).expect("a node let go of is held");
            self.fxfs.evict(id, held.unlinked);
            next = held.parent.and_then(|parent| {
                let p = self.held(parent);
                p.children -= 1;
                (p.holds == 0 && p.children == 0).then_some(parent)
            });
        }
    }
}

/// A node that handles hold: an inode of the host VFS. Fxfs's POSIX layer keeps what it opened of
/// it until evicted (`Fxfs::evict`), which frees it if no name links it.
struct HeldNode {
    file_type: FileType,
    unlinked: bool,
    /// Generation counter for `security.capability` presence: 0 means definitely absent; nonzero
    /// means the inode may have a `security.capability` xattr (incremented on setxattr, cleared to
    /// 0 only if unchanged after confirming/removing absence).
    security_cap_gen: u64,
    parent: Option<u64>,
    holds: u64,
    children: u64,
}

/// A handle's hold on one of a filesystem's nodes, released on drop. Holds the filesystem weakly
/// so handles never keep an unmounted filesystem alive.
struct Hold {
    inner: Weak<Inner>,
    object_id: u64,
}

impl Hold {
    fn object_id(&self) -> u64 {
        self.object_id
    }

    fn inner(&self) -> Result<Arc<Inner>> {
        self.inner.upgrade().ok_or(Errno::ENOTCONN)
    }

    fn same_fs(&self, other: &Hold) -> bool {
        Weak::ptr_eq(&self.inner, &other.inner)
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.release(self.object_id);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperationHolder {
    #[cfg(not(madsim))]
    Task(tokio::task::Id),
    Thread(std::thread::ThreadId),
}

impl OperationHolder {
    fn current() -> Self {
        #[cfg(not(madsim))]
        if let Some(task) = tokio::task::try_id() {
            return Self::Task(task);
        }
        Self::Thread(std::thread::current().id())
    }
}

#[derive(Default)]
struct Operations {
    holders: Mutex<Vec<OperationHolder>>,
    idle: Event,
}

impl Operations {
    fn held_by_current(&self) -> bool {
        let current = OperationHolder::current();
        self.holders.lock().unwrap().contains(&current)
    }

    async fn idle(&self) {
        loop {
            let idle = self.idle.listen();
            if self.holders.lock().unwrap().is_empty() {
                return;
            }
            idle.await;
        }
    }
}

struct Operation {
    operations: Arc<Operations>,
    holder: OperationHolder,
}

impl Drop for Operation {
    fn drop(&mut self) {
        let mut holders = self.operations.holders.lock().unwrap();
        if let Some(pos) = holders.iter().rposition(|&holder| holder == self.holder) {
            holders.remove(pos);
        }
        if holders.is_empty() {
            self.operations.idle.notify(usize::MAX);
        }
    }
}

enum Unmount {
    Began(oneshot::Receiver<anyhow::Result<()>>),
    Waits(Shared<oneshot::Receiver<()>>),
}

impl Unmount {
    #[cfg(test)]
    fn began(&self) -> bool {
        matches!(self, Self::Began(_))
    }

    async fn wait(self) -> Result<(), Error> {
        match self {
            Self::Began(result) => {
                result.await.expect("the runtime dropped an unmount task").map_err(Error::new)
            }
            Self::Waits(unmounted) => {
                // Sender dropped unsent if the unmount task panicked: done either way.
                let _ = unmounted.await;
                Ok(())
            }
        }
    }
}

impl Inner {
    fn is_mounted(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.unmounting.is_none() && state.mounted.is_some()
    }

    /// Runs `f` inline against `HostFxfs` while entering `self.runtime`, tracking the operation
    /// so unmount waits for it before closing the volume (board 11691).
    async fn run<T, F>(&self, f: impl FnOnce(Arc<HostFxfs>) -> F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let (fxfs, _operation) = {
            let mut state = self.state.lock().unwrap();
            let fxfs = state.mounted()?.fxfs.clone();
            let holder = OperationHolder::current();
            self.operations.holders.lock().unwrap().push(holder);
            (fxfs, Operation { operations: Arc::clone(&self.operations), holder })
        };
        // `f(fxfs)` owns `fxfs`, so `fxfs` is dropped before `_operation` even if cancelled.
        enter(&self.runtime, f(fxfs)).await
    }

    fn hold(
        self: &Arc<Self>,
        object_id: u64,
        file_type: FileType,
        parent: Option<u64>,
        fresh: bool,
    ) -> Result<Hold> {
        let mut state = self.state.lock().unwrap();
        let mounted = state.open()?;
        let held = mounted.nodes.entry(object_id).or_insert_with(|| HeldNode {
            file_type,
            unlinked: false,
            security_cap_gen: if fresh { 0 } else { 1 },
            parent: None,
            holds: 0,
            children: 0,
        });
        held.holds += 1;
        let new = held.holds == 1 && held.children == 0;
        if new && held.file_type == FileType::Directory {
            held.parent = parent;
            if let Some(parent) = parent {
                mounted.held(parent).children += 1;
            }
        }
        Ok(Hold { inner: Arc::downgrade(self), object_id })
    }

    fn with_held<T>(&self, hold: &Hold, f: impl FnOnce(&HeldNode) -> T) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        Ok(f(state.mounted()?.held(hold.object_id)))
    }

    /// Marks `hold`'s node as possibly having a `security.capability` xattr, bumping its
    /// generation counter so a racing privilege-clear cannot clear this update.
    fn mark_security_cap(&self, hold: &Hold) {
        let mut state = self.state.lock().unwrap();
        if let Ok(mounted) = state.open()
            && let Some(held) = mounted.nodes.get_mut(&hold.object_id)
        {
            held.security_cap_gen = held.security_cap_gen.wrapping_add(1).max(1);
        }
    }

    /// Clears `hold`'s `security.capability` presence bit if its generation still matches
    /// `expected_gen`.
    fn clear_security_cap_if(&self, hold: &Hold, expected_gen: u64) {
        if expected_gen == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if let Ok(mounted) = state.open()
            && let Some(held) = mounted.nodes.get_mut(&hold.object_id)
            && held.security_cap_gen == expected_gen
        {
            held.security_cap_gen = 0;
        }
    }

    fn is_unlinked(&self, hold: &Hold) -> Result<bool> {
        let mut state = self.state.lock().unwrap();
        let mounted = state.open()?;
        Ok(mounted.nodes.get(&hold.object_id).is_some_and(|held| held.unlinked))
    }

    async fn unlinked(&self, fxfs: &HostFxfs, object_id: u64, replaced: u32) -> Result<()> {
        let gone = match replaced {
            fxfs_posix::REPLACED_DIRECTORY => true,
            fxfs_posix::REPLACED_OBJECT => match fxfs.in_graveyard(object_id).await {
                Ok(in_graveyard) => in_graveyard,
                Err(error) => return Err(fxfs_posix::errno(&error)),
            },
            _ => false,
        };
        if !gone {
            return Ok(());
        }
        let mut state = self.state.lock().unwrap();
        match state.open()?.nodes.get_mut(&object_id) {
            Some(held) => held.unlinked = true,
            None => fxfs.evict(object_id, true),
        }
        Ok(())
    }

    fn moved(&self, object_id: u64, parent: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let mounted = state.open()?;
        let Some(held) = mounted.nodes.get_mut(&object_id) else { return Ok(()) };
        let Some(old) = held.parent.replace(parent) else { return Ok(()) };
        mounted.held(parent).children += 1;
        mounted.release_child(old);
        Ok(())
    }

    fn is_within(&self, directory: u64, ancestor: u64) -> Result<bool> {
        let mut state = self.state.lock().unwrap();
        let mounted = state.open()?;
        let mut at = Some(directory);
        while let Some(directory) = at {
            if directory == ancestor {
                return Ok(true);
            }
            at = mounted.nodes.get(&directory).and_then(|held| held.parent);
        }
        Ok(false)
    }

    fn release(&self, object_id: u64) {
        let mut state = self.state.lock().unwrap();
        let Ok(mounted) = state.open() else {
            return;
        };
        let held = mounted.held(object_id);
        held.holds -= 1;
        if held.holds == 0 && held.children == 0 {
            mounted.let_go(object_id);
        }
    }

    /// Begins unmounting the volume and closing the Fxfs filesystem once, returning the handle to
    /// wait for completion. Concurrent callers share the single unmount/close task.
    fn begin_unmount(self: &Arc<Self>) -> Unmount {
        let mut state = self.state.lock().unwrap();
        if let Some(unmounted) = &state.unmounting {
            return Unmount::Waits(unmounted.clone());
        }
        if state.mounted.is_none() {
            let (done, unmounted) = oneshot::channel();
            // Already closed; wake any waiter immediately.
            let _ = done.send(());
            return Unmount::Waits(unmounted.shared());
        }
        let (done, unmounted) = oneshot::channel();
        state.unmounting = Some(unmounted.shared());
        drop(state);
        let (result, began) = oneshot::channel();
        let inner = Arc::clone(self);
        self.runtime.spawn(async move {
            let closed = async {
                inner.operations.idle().await;
                let Mounted { filesystem, fxfs, nodes } =
                    inner.state.lock().unwrap().mounted.take().expect("unmounted once");
                let fxfs = Arc::into_inner(fxfs).expect("an operation holds the volume after it");
                for (object_id, held) in nodes {
                    fxfs.evict(object_id, held.unlinked);
                }
                let HostPlatform = fxfs.close().await;
                filesystem.graveyard().flush().await;
                filesystem.sync(SyncOptions { flush_device: true, ..Default::default() }).await?;
                filesystem.close().await?;
                let device = filesystem.take_device().await;
                anyhow::Ok(Storage::closed(device, inner.storage_kind.clone()))
            }
            .await;
            let outcome = match closed {
                Ok(storage) => {
                    inner.state.lock().unwrap().closed = Some(storage);
                    Ok(())
                }
                Err(err) => Err(err),
            };
            inner.finished.store(true, Ordering::Release);
            // Its beginner may have given up on it.
            let _ = result.send(outcome);
            // Its waiter gone: nothing to tell.
            let _ = done.send(());
        });
        Unmount::Began(began)
    }

    /// Whether the current thread is executing this filesystem's own `current_thread` Tokio
    /// runtime (where blocking on a task spawned onto `self.runtime` would deadlock).
    fn on_own_current_thread(&self) -> bool {
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

    /// Runs `future` on `self.runtime`, blocking the calling thread (for `Drop`).
    ///
    /// On a multi-thread Tokio worker, `block_in_place` runs before spawning so the task lands on
    /// the global queue rather than this worker's unstealable LIFO slot and cannot starve the
    /// shared runtime even under concurrent drops (hotbed reviews 111 and 112).
    fn run_blocking<T: Send + 'static>(
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
                    "cladefs: dropped a mounted Filesystem on its own current_thread runtime; \
                     await unmount() or close() first"
                );
            }
            _ => run(),
        }
    }

    #[cfg(test)]
    fn fxfs(&self) -> Result<Arc<HostFxfs>> {
        Ok(self.state.lock().unwrap().mounted()?.fxfs.clone())
    }

    #[cfg(test)]
    fn held(&self) -> std::collections::BTreeMap<u64, u64> {
        match self.state.lock().unwrap().mounted() {
            Ok(mounted) => {
                mounted.nodes.iter().map(|(object_id, held)| (*object_id, held.holds)).collect()
            }
            Err(_) => Default::default(),
        }
    }
}

/// A mounted Fxfs filesystem serving the `"default"` volume on [`Storage`]. Its nodes are reached
/// from its [`root`](Self::root) directory. See the crate docs.
///
/// Its methods, and its nodes', may be called from any number of tasks or threads at once.
pub struct Filesystem {
    inner: Arc<Inner>,
    /// The root directory, which the filesystem holds while it lives.
    root: Node,
    /// cladefs: the cell this filesystem is backed by, if mounted on an image cell (TASK-562,
    /// board 2520, board 2704).
    cell: Option<cladefs_core::CellId>,
}

impl Filesystem {
    /// Mounts the filesystem on `storage` with the default [`Options`].
    pub async fn mount(storage: Storage) -> Result<Self, Error> {
        Options::new().mount(storage).await
    }

    /// Formats `storage` with an empty filesystem, and mounts it with the default [`Options`].
    pub async fn format(storage: Storage) -> Result<Self, Error> {
        Options::new().format(storage).await
    }

    /// Mounts or formats `storage` and opens/creates the configured volume directly.
    async fn start(options: &Options, storage: Storage, format: bool) -> Result<Self, Error> {
        let cell = storage.cell();
        let (device, storage_kind) = storage.into_parts();
        let barriers = matches!(storage_kind, Kind::Image { .. });
        let runtime = options.fxfs_runtime();
        let volume_name = options.volume.clone();
        let (filesystem, store) = enter(&runtime, async move {
            let filesystem = FxFilesystemBuilder::new()
                .format(format)
                .barriers_enabled(barriers)
                .open(device)
                .await
                .map_err(Error::new)?;
            let opened = async {
                let root = root_volume(filesystem.clone()).await?;
                if format {
                    root.new_volume(&volume_name, NewChildStoreOptions::default()).await
                } else {
                    root.volume(&volume_name).await
                }
            }
            .await;
            match opened {
                Ok(store) => Ok((filesystem, store)),
                Err(error) => {
                    if let Err(close_error) = filesystem.close().await {
                        log::error!(
                            "cladefs: failed to close filesystem after open error: {close_error:#}"
                        );
                    }
                    Err(volume_error(error))
                }
            }
        })
        .await?;
        let block_size = filesystem.block_size().get() as u32;
        let root_id = store.root_directory_object_id();
        let fxfs = HostFxfs::new(store.filesystem(), store, false, HostPlatform);
        let root_held = HeldNode {
            file_type: FileType::Directory,
            unlinked: false,
            security_cap_gen: 1,
            parent: None,
            holds: 1,
            children: 0,
        };
        let mounted = Mounted {
            filesystem,
            fxfs: Arc::new(fxfs),
            nodes: HashMap::from([(root_id, root_held)]),
        };
        let inner = Arc::new(Inner {
            runtime,
            block_size,
            storage_kind,
            namespace: tokio::sync::RwLock::new(()),
            operations: Arc::default(),
            finished: AtomicBool::new(false),
            state: Mutex::new(State { mounted: Some(mounted), unmounting: None, closed: None }),
        });
        let root = Node(Arc::new(Hold { inner: Arc::downgrade(&inner), object_id: root_id }));
        Ok(Self { inner, root, cell })
    }

    /// cladefs: the cell this filesystem is backed by, if mounted on an image cell (TASK-562).
    pub fn cell(&self) -> Option<cladefs_core::CellId> {
        self.cell
    }

    /// The filesystem's root directory.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Unmounts the filesystem, unless it is unmounted, writing back what is dirty, closes Fxfs,
    /// and returns the underlying [`Storage`].
    pub async fn close(self) -> Result<Storage, Error> {
        self.unmount().await?;
        self.inner
            .state
            .lock()
            .unwrap()
            .closed
            .take()
            .ok_or_else(|| Error::msg("closing the filesystem failed, or its storage was taken"))
    }

    /// Unmounts the filesystem, unless it is unmounted, writing back what is dirty and closing
    /// Fxfs, keeping the storage for [`close`](Self::close) to give back. Every operation fails
    /// with `ENOTCONN` from then on, its nodes' too.
    ///
    /// Operations under way as it begins complete before it writes the volume back. Once
    /// unmounting begins, it completes on the runtime even if this future is dropped; concurrent
    /// calls wait for the same unmount, and only the first receives any error.
    pub async fn unmount(&self) -> Result<(), Error> {
        self.inner.begin_unmount().wait().await
    }

    /// The filesystem's usage.
    pub async fn statfs(&self) -> Result<StatFs> {
        let statfs =
            self.inner.run(|fxfs| async move { fxfs.statfs().await.map_err(errno) }).await?;
        Ok(StatFs {
            block_size: statfs.block_size as u32,
            blocks: statfs.blocks,
            blocks_free: statfs.free_blocks,
            name_max: NAME_MAX as u32,
        })
    }

    /// What compacting the filesystem's trees has cost since it was mounted, for benchmarks to
    /// compare.
    pub fn compactions(&self) -> Result<Compactions> {
        let mut state = self.inner.state.lock().unwrap();
        let mounted = state.mounted()?;
        let object_manager = mounted.filesystem.object_manager();
        Ok(Compactions {
            allocator: TreeCompactions::of(object_manager.allocator().tree()),
            stores: object_manager
                .stores()
                .iter()
                .map(|store| TreeCompactions::of(store.tree()))
                .fold(TreeCompactions::default(), std::ops::Add::add),
        })
    }

    /// Creates an additional empty volume named `name` on this Fxfs filesystem (board 13920), and
    /// compacts the journal, so that the filesystem's trees hold the new volume whole.
    ///
    /// Without the compaction, creating many volumes runs the filesystem out of room: a store's
    /// creation takes a few hundred bytes of the journal, and Fxfs keeps room for flushing what the
    /// journal holds at four times its size (`reserved_space_from_journal_usage`), but the first
    /// flush of a new store writes it a layer file and rewrites its info, some 10 KiB. Compacted
    /// at once, the store is flushed alone while that room is there; 256 left to the next
    /// compaction needed more than its metadata reservation held, and failed with ENOSPC on a
    /// 128 MiB cell (board 14222). The unmount of each new volume the deleted `Image` did hid this:
    /// its sync padded the journal to a block per volume, which raised the reservation enough.
    pub async fn create_volume(&self, name: &str) -> Result<(), Error> {
        let name = name.to_owned();
        self.inner
            .run(|fxfs| async move {
                let filesystem = fxfs.filesystem();
                let created = async {
                    let root = root_volume(filesystem.clone()).await?;
                    root.new_volume(&name, NewChildStoreOptions::default()).await?;
                    filesystem.journal().force_compact().await
                };
                Ok(created.await)
            })
            .await
            .map_err(Error::new)?
            .map_err(volume_error)
    }

    /// Compacts the journal into the filesystem's trees (board 13920).
    pub async fn compact(&self) -> Result<(), Error> {
        self.inner
            .run(|fxfs| async move { Ok(fxfs.filesystem().journal().force_compact().await) })
            .await
            .map_err(Error::new)?
            .map_err(volume_error)
    }

    /// What this filesystem's volume takes of its metadata (board 13920).
    pub async fn metadata(&self) -> Result<VolumeMetadata> {
        self.inner
            .run(|fxfs| async move {
                let filesystem = fxfs.filesystem();
                let store_id = fxfs.volume_id();
                let store = filesystem
                    .object_manager()
                    .store(store_id)
                    .ok_or_else(|| errno(FxfsError::NotFound.into()))?;
                let layer_set = store.tree().layer_set();
                let layers = layer_set.layers.iter().filter_map(|layer| layer.handle());
                let layer_bytes = layers.map(|layer| layer.get_size()).sum();
                let allocator_records =
                    allocator_records(&filesystem.allocator(), store_id).await.map_err(errno)?;
                Ok(VolumeMetadata { layer_bytes, allocator_records })
            })
            .await
    }
}

impl Drop for Filesystem {
    /// Unmounts and closes the filesystem if it has not finished unmounting, blocking the
    /// dropping thread until teardown completes. Await `close` or `unmount` rather than dropping
    /// a mounted filesystem, and await or drop any in-flight `Node` operation futures owned by
    /// the same task first (inline zero-copy operations cannot advance while their owning task
    /// blocks in `Drop`).
    ///
    /// # Panics
    ///
    /// If a mounted filesystem is dropped on its own `current_thread` Tokio runtime, or while
    /// the dropping task itself holds an in-flight `Node` operation future.
    fn drop(&mut self) {
        if self.inner.finished.load(Ordering::Acquire) {
            return;
        }
        if self.inner.operations.held_by_current() {
            if std::thread::panicking() {
                log::error!(
                    "cladefs: skipping blocking unmount during unwind while current task holds \
                     an in-flight Node operation"
                );
                return;
            }
            panic!(
                "cladefs: dropped a mounted Filesystem while this task holds an in-flight Node \
                 operation; await or drop the operation before dropping Filesystem"
            );
        }
        if self.inner.on_own_current_thread() && std::thread::panicking() {
            log::error!(
                "cladefs: skipping blocking unmount during unwind on current_thread runtime"
            );
            return;
        }
        let unmount = self.inner.begin_unmount();
        if let Err(error) = self.inner.run_blocking(unmount.wait()) {
            log::error!("cladefs: failed to unmount: {error:#}");
        }
    }
}

impl std::fmt::Debug for Filesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mounted = self.inner.is_mounted();
        f.debug_struct("Filesystem").field("mounted", &mounted).finish_non_exhaustive()
    }
}

/// A handle to a node of a [`Filesystem`]: a file, directory, symlink or special file, whose
/// operations are its methods.
///
/// A handle holds its node open, as an open file does: a file unlinked stays, and can be read and
/// written, until the last handle to it drops. Clones of a handle are handles too, and a node may
/// have any number, from separate lookups too. A handle does not keep its filesystem mounted: once
/// it is unmounted, every operation fails with `ENOTCONN`.
#[derive(Clone)]
pub struct Node(Arc<Hold>);

impl Node {
    /// The node's inode number.
    pub fn inode(&self) -> Inode {
        Inode(self.0.object_id())
    }

    fn ino(&self) -> u64 {
        self.0.object_id()
    }

    fn inner(&self) -> Result<Arc<Inner>> {
        self.0.inner()
    }

    fn same_fs(&self, other: &Node) -> Result<Arc<Inner>> {
        if !self.0.same_fs(&other.0) {
            return Err(Errno::EXDEV);
        }
        self.inner()
    }

    fn file_type_and_cap(&self) -> Result<(FileType, u64, Arc<Inner>)> {
        let inner = self.inner()?;
        let (file_type, cap_gen) =
            inner.with_held(&self.0, |held| (held.file_type, held.security_cap_gen))?;
        Ok((file_type, cap_gen, inner))
    }

    fn file_type(&self) -> Result<(FileType, Arc<Inner>)> {
        let (file_type, _, inner) = self.file_type_and_cap()?;
        Ok((file_type, inner))
    }

    fn directory(&self) -> Result<Arc<Inner>> {
        match self.file_type()? {
            (FileType::Directory, inner) => Ok(inner),
            _ => Err(Errno::ENOTDIR),
        }
    }

    fn file_and_cap(&self) -> Result<(Arc<Inner>, u64)> {
        match self.file_type_and_cap()? {
            (FileType::Directory, _, _) => Err(Errno::EISDIR),
            (FileType::Symlink, _, _) => Err(Errno::EINVAL),
            (_, cap_gen, inner) => Ok((inner, cap_gen)),
        }
    }

    fn file(&self) -> Result<Arc<Inner>> {
        self.file_and_cap().map(|(inner, _)| inner)
    }

    fn exists(inner: &Inner, dir: &Node) -> Result<()> {
        match inner.is_unlinked(&dir.0)? {
            true => Err(Errno::ENOENT),
            false => Ok(()),
        }
    }

    /// The inode of the directory this directory is in, as `..` names it: its own, for the root.
    /// A directory removed keeps the one it was removed from. `ENOTDIR` if the node is not a
    /// directory.
    pub fn parent(&self) -> Result<Inode> {
        let inner = self.directory()?;
        let parent = inner.with_held(&self.0, |held| held.parent)?;
        Ok(Inode(parent.unwrap_or(self.ino())))
    }

    /// Looks up `name` in this directory, and returns a handle to the node it names and its
    /// attributes.
    pub async fn lookup(&self, name: &[u8]) -> Result<(Node, Attr)> {
        let inner = self.directory()?;
        let (dir, name) = (self.ino(), valid_name(name)?);
        let held = Arc::clone(&inner);
        inner
            .run(|fxfs| async move {
                // Held before the name can go (`Inner::namespace`).
                let _names = held.namespace.read().await;
                let found = find(&fxfs, dir, &name).await?.ok_or(Errno::ENOENT)?;
                handle(&held, &found, dir, false)
            })
            .await
    }

    /// The node's attributes.
    pub async fn getattr(&self) -> Result<Attr> {
        let inner = self.inner()?;
        let (ino, block_size) = (self.ino(), inner.block_size);
        let attr = inner
            .run(|fxfs| async move { attr(block_size, &fxfs.attr(ino).await.map_err(errno)?) })
            .await?;
        self.linked(&inner, attr)
    }

    fn linked(&self, inner: &Inner, mut attr: Attr) -> Result<Attr> {
        if inner.is_unlinked(&self.0)? {
            attr.nlink = 0;
        }
        Ok(attr)
    }

    /// Changes the node's attributes, and its status change time, whatever it changes, as Linux's
    /// `notify_change` does: a chown to the owner and group it has too (TASK-149.37). Returns them.
    pub async fn setattr(&self, set: &SetAttr) -> Result<Attr> {
        self.setattr_with(set, None, false).await
    }

    /// cladefs: FUSE `SETATTR` entry point supporting an explicit `ctime` (flushed by Linux's
    /// `FUSE_WRITEBACK_CACHE`) and optional `security.capability` removal (`FUSE_HANDLE_KILLPRIV_V2`).
    pub async fn setattr_fuse(
        &self,
        set: &SetAttr,
        explicit_ctime: Option<std::time::SystemTime>,
        kill_security_cap: bool,
    ) -> Result<Attr> {
        self.setattr_with(set, explicit_ctime, kill_security_cap).await
    }

    async fn setattr_with(
        &self,
        set: &SetAttr,
        explicit_ctime: Option<std::time::SystemTime>,
        kill_security_cap: bool,
    ) -> Result<Attr> {
        let (file_type, cap_gen, inner) = self.file_type_and_cap()?;
        let now = now()?;
        let ctime_ns = match explicit_ctime {
            Some(ctime) => nanos_from_time(ctime)?,
            None => now,
        };
        let mut change = status_changed_at(ctime_ns);
        if let Some(size) = set.size {
            match file_type {
                FileType::Directory => return Err(Errno::EISDIR),
                FileType::Symlink => return Err(Errno::EINVAL),
                _ => {}
            }
            // A truncation changes the file's contents, as a write does.
            change.valid |= fxfs_posix::SET_MTIME;
            change.mtime_ns = now;
            change.valid |= fxfs_posix::SET_SIZE;
            change.size = size;
        }
        if let Some(uid) = set.uid {
            change.valid |= fxfs_posix::SET_UID;
            change.uid = uid;
        }
        if let Some(gid) = set.gid {
            change.valid |= fxfs_posix::SET_GID;
            change.gid = gid;
        }
        if let Some(atime) = set.atime {
            change.valid |= fxfs_posix::SET_ATIME;
            change.atime_ns = nanos_from_time(atime.resolve())?;
        }
        if let Some(mtime) = set.mtime {
            change.valid |= fxfs_posix::SET_MTIME;
            change.mtime_ns = nanos_from_time(mtime.resolve())?;
        }
        let kill_cap = kill_security_cap && file_type != FileType::Directory && cap_gen != 0;
        if kill_cap {
            change.valid |= fxfs_posix::SET_KILL_SECURITY_CAP;
        }
        let (perm, kill_suid_sgid) = (set.perm, set.kill_suid_sgid);
        let (ino, block_size) = (self.ino(), inner.block_size);
        let attr = inner
            .run(|fxfs| async move {
                // As Linux's, a directory's setuid and setgid are never killed: its setgid says
                // what its new nodes' group is.
                let kill_suid_sgid = kill_suid_sgid && file_type != FileType::Directory;
                if perm.is_some() || kill_suid_sgid {
                    let mode = match file_type {
                        // A symlink's permissions are always all of them.
                        FileType::Symlink => 0o777,
                        _ => {
                            let mut mode = fxfs.attr(ino).await.map_err(errno)?.mode & PERM_MASK;
                            if kill_suid_sgid {
                                mode &= !S_ISUID;
                                if mode & S_IXGRP != 0 {
                                    mode &= !S_ISGID;
                                }
                            }
                            perm.map_or(mode, |perm| perm as u32 & PERM_MASK)
                        }
                    };
                    change.valid |= fxfs_posix::SET_MODE;
                    change.mode = file_type.mode_bits() | mode;
                }
                fxfs.setattr(ino, &change).await.map_err(errno)?;
                attr(block_size, &fxfs.attr(ino).await.map_err(errno)?)
            })
            .await?;
        if kill_cap {
            inner.clear_security_cap_if(&self.0, cap_gen);
        }
        self.linked(&inner, attr)
    }

    /// The target of this symlink.
    pub async fn readlink(&self) -> Result<Vec<u8>> {
        let inner = match self.file_type()? {
            (FileType::Symlink, inner) => inner,
            _ => return Err(Errno::EINVAL),
        };
        let ino = self.ino();
        inner.run(|fxfs| async move { fxfs.readlink(ino).await.map_err(errno) }).await
    }

    /// Makes `name` in this directory with `make`, unless it names something already (`EEXIST`),
    /// and returns a handle to what it made, and its attributes.
    async fn make<'a, F>(
        &'a self,
        name: &[u8],
        make: impl FnOnce(Arc<HostFxfs>, u64, String) -> F + 'a,
    ) -> Result<(Node, Attr)>
    where
        F: Future<Output = anyhow::Result<fxfs_posix::Attr>> + 'a,
    {
        let inner = self.directory()?;
        let (dir, name) = (self.ino(), valid_name(name)?);
        let held = Arc::clone(&inner);
        inner
            .run(|fxfs| async move {
                let _names = held.namespace.write().await;
                Self::exists(&held, self)?;
                if find(&fxfs, dir, &name).await?.is_some() {
                    return Err(Errno::EEXIST);
                }
                let made = make(fxfs, dir, name).await.map_err(errno)?;
                handle(&held, &made, dir, true)
            })
            .await
    }

    async fn create_node(
        &self,
        name: &[u8],
        mode: u32,
        rdev: u32,
        owner: Owner,
    ) -> Result<(Node, Attr)> {
        self.make(name, move |fxfs, dir, name| async move {
            let [made, _] = fxfs
                .create(dir, name.as_bytes(), mode, rdev.into(), owner.uid, owner.gid, &[])
                .await?;
            anyhow::Ok(made)
        })
        .await
    }

    /// Creates the regular file `name` in this directory with `perm`, owned by `owner`, and
    /// returns a handle to it and its attributes.
    pub async fn create(&self, name: &[u8], perm: u16, owner: Owner) -> Result<(Node, Attr)> {
        let mode = FileType::File.mode_bits() | (perm as u32 & PERM_MASK);
        self.create_node(name, mode, 0, owner).await
    }

    /// Creates the directory `name` in this directory with `perm`, owned by `owner`, and returns a
    /// handle to it and its attributes.
    pub async fn mkdir(&self, name: &[u8], perm: u16, owner: Owner) -> Result<(Node, Attr)> {
        let mode = FileType::Directory.mode_bits() | (perm as u32 & PERM_MASK);
        self.create_node(name, mode, 0, owner).await
    }

    /// Creates the node `name` in this directory of `file_type` with `perm`, owned by `owner`, and
    /// returns a handle to it and its attributes. A device node stands for device `rdev`.
    ///
    /// It may be a regular file, or a special file: a FIFO, socket, or character or block device,
    /// which is an Fxfs file whose mode says what it is, as Starnix makes them. Directories and
    /// symlinks fail with `EINVAL`.
    pub async fn mknod(
        &self,
        name: &[u8],
        file_type: FileType,
        perm: u16,
        rdev: u32,
        owner: Owner,
    ) -> Result<(Node, Attr)> {
        if matches!(file_type, FileType::Directory | FileType::Symlink) {
            return Err(Errno::EINVAL);
        }
        let mode = file_type.mode_bits() | (perm as u32 & PERM_MASK);
        self.create_node(name, mode, rdev, owner).await
    }

    /// Creates the symlink `name` in this directory to `target`, owned by `owner`, and returns a
    /// handle to it and its attributes.
    pub async fn symlink(&self, name: &[u8], target: &[u8], owner: Owner) -> Result<(Node, Attr)> {
        self.make(name, move |fxfs, dir, name| async move {
            let [made, _] =
                fxfs.symlink(dir, name.as_bytes(), target, owner.uid, owner.gid).await?;
            anyhow::Ok(made)
        })
        .await
    }

    /// Links the node into directory `dir` as `name`, and returns its attributes. Directories
    /// fail with `EPERM`, and a directory of another filesystem with `EXDEV`.
    pub async fn link(&self, dir: &Node, name: &[u8]) -> Result<Attr> {
        let inner = self.same_fs(dir)?;
        if self.file_type()?.0 == FileType::Directory {
            return Err(Errno::EPERM);
        }
        dir.directory()?;
        let (ino, dir_ino, name) = (self.ino(), dir.ino(), valid_name(name)?);
        let held = Arc::clone(&inner);
        let block_size = inner.block_size;
        inner
            .run(|fxfs| async move {
                let _names = held.namespace.write().await;
                Self::exists(&held, dir)?;
                // A node no name links any more is freed once it is let go of.
                Self::exists(&held, self)?;
                if find(&fxfs, dir_ino, &name).await?.is_some() {
                    return Err(Errno::EEXIST);
                }
                let [linked, _] =
                    fxfs.link(ino, dir_ino, name.as_bytes(), false).await.map_err(errno)?;
                attr(block_size, &linked)
            })
            .await
    }

    /// Removes the non-directory `name` from this directory. A directory fails with `EISDIR`.
    pub async fn unlink(&self, name: &[u8]) -> Result<()> {
        self.remove(name, false).await
    }

    /// Removes the empty directory `name` from this directory. A non-directory fails with
    /// `ENOTDIR`.
    pub async fn rmdir(&self, name: &[u8]) -> Result<()> {
        self.remove(name, true).await
    }

    async fn remove(&self, name: &[u8], directory: bool) -> Result<()> {
        let inner = self.directory()?;
        let (dir, name) = (self.ino(), valid_name(name)?);
        let held = Arc::clone(&inner);
        inner
            .run(|fxfs| async move {
                let _names = held.namespace.write().await;
                let found = find(&fxfs, dir, &name).await?.ok_or(Errno::ENOENT)?;
                match (type_of(&found)? == FileType::Directory, directory) {
                    (true, false) => return Err(Errno::EISDIR),
                    (false, true) => return Err(Errno::ENOTDIR),
                    _ => {}
                }
                let (replaced, _) =
                    fxfs.unlink(dir, name.as_bytes(), found.ino).await.map_err(errno)?;
                held.unlinked(&fxfs, found.ino, replaced).await
            })
            .await
    }

    /// Renames `name` in this directory to `new_name` in directory `new_dir`, replacing what is
    /// there unless `flags` say not to, as rename(2) does: a directory only with an empty
    /// directory, and anything else only with something else, but a directory into itself never.
    /// A directory of another filesystem fails with `EXDEV`. Returns the node renamed's inode.
    pub async fn rename(
        &self,
        name: &[u8],
        new_dir: &Node,
        new_name: &[u8],
        flags: RenameFlags,
    ) -> Result<Inode> {
        if flags.contains(RenameFlags::EXCHANGE) || !RenameFlags::all().contains(flags) {
            return Err(Errno::EINVAL);
        }
        let inner = self.same_fs(new_dir)?;
        self.directory()?;
        new_dir.directory()?;
        let (dir, name) = (self.ino(), valid_name(name)?);
        let (new_dir_ino, new_name) = (new_dir.ino(), valid_name(new_name)?);
        let no_replace = flags.contains(RenameFlags::NOREPLACE);
        let held = Arc::clone(&inner);
        inner
            .run(|fxfs| async move {
                let _names = held.namespace.write().await;
                let moved = find(&fxfs, dir, &name).await?.ok_or(Errno::ENOENT)?;
                let replaced = find(&fxfs, new_dir_ino, &new_name).await?;
                // As Linux's renameat2 does, a rename that must not replace fails once the source
                // is found if the new name holds anything, the source itself too.
                if no_replace && replaced.is_some() {
                    return Err(Errno::EEXIST);
                }
                Self::exists(&held, new_dir)?;
                let moves_directory = type_of(&moved)? == FileType::Directory;
                if let Some(replaced) = &replaced {
                    match (moves_directory, type_of(replaced)? == FileType::Directory) {
                        (true, false) => return Err(Errno::ENOTDIR),
                        (false, true) => return Err(Errno::EISDIR),
                        _ => {}
                    }
                    // Names of the same node: nothing to do.
                    if replaced.ino == moved.ino {
                        return Ok(Inode(moved.ino));
                    }
                }
                if moves_directory && held.is_within(new_dir_ino, moved.ino)? {
                    return Err(Errno::EINVAL);
                }
                let replaced_ino = replaced.as_ref().map(|replaced| replaced.ino);
                let (replaced_kind, _) = fxfs
                    .rename(
                        dir,
                        name.as_bytes(),
                        moved.ino,
                        new_dir_ino,
                        new_name.as_bytes(),
                        replaced_ino,
                    )
                    .await
                    .map_err(errno)?;
                if moves_directory {
                    held.moved(moved.ino, new_dir_ino)?;
                }
                if let Some(replaced) = replaced_ino {
                    held.unlinked(&fxfs, replaced, replaced_kind).await?;
                }
                Ok(Inode(moved.ino))
            })
            .await
    }

    /// The entries of this directory, but `.` and `..`, as it holds them now.
    pub async fn entries(&self) -> Result<Vec<DirEntry>> {
        let inner = self.directory()?;
        let dir = self.ino();
        inner
            .run(|fxfs| async move {
                let mut entries = Vec::new();
                let listed = fxfs.readdir(dir, ReaddirStart::Index(0), |name, ino, kind| {
                    #[cfg(test)]
                    READDIR_EMITS.fetch_add(1, Ordering::Relaxed);
                    let file_type = FileType::from_mode(kind).unwrap_or(FileType::File);
                    entries.push(DirEntry { inode: Inode(ino), file_type, name: name.into() });
                    true
                });
                listed.await.map_err(errno)?;
                Ok(entries)
            })
            .await
    }

    /// cladefs: reads at most `limit` entries of this directory starting at `start` (plus at most
    /// one extra Fxfs emit to detect whether more entries follow), returning `(entries, has_more)`
    /// (TASK-619, board 2634).
    pub async fn entries_page(
        &self,
        start: EntriesStart<'_>,
        limit: usize,
    ) -> Result<(Vec<DirEntry>, bool)> {
        let inner = self.directory()?;
        let dir = self.ino();
        let start = match start {
            EntriesStart::Index(index) => ReaddirStart::Index(index),
            EntriesStart::After(name) => ReaddirStart::After(name),
        };
        inner
            .run(|fxfs| async move {
                let mut entries = Vec::with_capacity(limit.min(256));
                let mut has_more = false;
                let listed = fxfs.readdir(dir, start, |name, ino, kind| {
                    #[cfg(test)]
                    READDIR_EMITS.fetch_add(1, Ordering::Relaxed);
                    if entries.len() >= limit {
                        has_more = true;
                        return false;
                    }
                    let file_type = FileType::from_mode(kind).unwrap_or(FileType::File);
                    entries.push(DirEntry { inode: Inode(ino), file_type, name: name.into() });
                    true
                });
                listed.await.map_err(errno)?;
                Ok((entries, has_more))
            })
            .await
    }

    /// Reads from this file at `offset` directly into `buf` without an intermediate bounce copy
    /// (board 11691), returning how many bytes were read (less than asked only at EOF).
    pub async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inner = self.file()?;
        let ino = self.ino();
        inner
            .run(|fxfs| async move { fxfs.read_into(ino, offset, &mut [buf]).await.map_err(errno) })
            .await
    }

    /// Reads up to `len` bytes from this file at `offset`, returning them directly in a single
    /// buffer (board 12313): fewer than `len` only at the end of the file.
    pub async fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut data = vec![0; len];
        let read = self.read(offset, &mut data).await?;
        data.truncate(read);
        Ok(data)
    }

    /// Reads from this file at `offset` into `bufs` in turn, memory that may change meanwhile, and
    /// returns how much was read: less than asked only at the end of the file.
    pub async fn read_vectored(
        &self,
        offset: u64,
        bufs: &mut [VolatileSliceMut<'_>],
    ) -> Result<usize> {
        let len = bufs.iter().map(VolatileSliceMut::len).sum::<usize>();
        let data = self.read_vec(offset, len).await?;
        let mut rest = &data[..];
        for buf in bufs {
            rest = &rest[buf.copy_from(rest)..];
        }
        Ok(data.len())
    }

    /// Reads up to `len` bytes from this file at `offset` into a single `Vec<u8>` whose first
    /// `prefix` bytes are zeroed for a framing header (such as FUSE's `OutHeader`), avoiding
    /// intermediate allocations and copies (board 12092).
    pub async fn read_prefixed(&self, offset: u64, len: usize, prefix: usize) -> Result<Vec<u8>> {
        let inner = self.file()?;
        let ino = self.ino();
        inner
            .run(|fxfs| async move {
                fxfs.read_prefixed(ino, offset, len, prefix).await.map_err(errno)
            })
            .await
    }

    /// Writes `data` directly to this file at `offset` without an intermediate bounce copy
    /// (board 11691), extending it if past its end, and returning how many bytes were written.
    pub async fn write(&self, offset: u64, data: &[u8]) -> Result<usize> {
        let (inner, _) = self.file_and_cap()?;
        if data.is_empty() {
            return Ok(0);
        }
        let (written, _) = self.write_slice(inner, 0, offset, data, true, false).await?;
        Ok(written)
    }

    /// cladefs: FUSE `WRITE` entry point with optional timestamp update (skipped for writeback
    /// cache flushes where the kernel owns mtime/ctime) and optional privilege clearing
    /// (`FUSE_HANDLE_KILLPRIV_V2`). Returns `(written, mode_changed)`.
    pub async fn write_fuse(
        &self,
        offset: u64,
        data: &[u8],
        update_times: bool,
        kill_priv: bool,
    ) -> Result<(usize, bool)> {
        let (inner, cap_gen) = self.file_and_cap()?;
        if data.is_empty() {
            return Ok((0, false));
        }
        self.write_slice(inner, cap_gen, offset, data, update_times, kill_priv).await
    }

    /// Writes `bufs` in turn to this file at `offset`, memory that may change meanwhile, and
    /// returns how much was written, changing the file's modification time.
    pub async fn write_vectored(&self, offset: u64, bufs: &[VolatileSlice<'_>]) -> Result<usize> {
        let (inner, _) = self.file_and_cap()?;
        let len: usize = bufs.iter().map(VolatileSlice::len).sum();
        if len == 0 {
            return Ok(0);
        }
        let mut data = vec![0; len];
        let mut at = 0;
        for buf in bufs {
            at += buf.copy_to(&mut data[at..]);
        }
        let (written, _) = self.write_slice(inner, 0, offset, &data, true, false).await?;
        Ok(written)
    }

    async fn write_slice(
        &self,
        inner: Arc<Inner>,
        cap_gen: u64,
        offset: u64,
        data: &[u8],
        update_times: bool,
        kill_priv: bool,
    ) -> Result<(usize, bool)> {
        offset.checked_add(data.len() as u64).ok_or(Errno::EFBIG)?;
        let (ino, now) = (self.ino(), if update_times { Some(now()?) } else { None });
        let kill_cap = kill_priv && cap_gen != 0;
        let (written, mode_changed) = inner
            .run(|fxfs| async move {
                let chunks = data.len().div_ceil(WRITE_CHUNK);
                let mut mode_changed = false;
                for (index, (chunk, written)) in
                    data.chunks(WRITE_CHUNK).zip((0..).step_by(WRITE_CHUNK)).enumerate()
                {
                    let is_last = index + 1 == chunks;
                    mode_changed |= fxfs
                        .write_at(
                            ino,
                            offset + written as u64,
                            chunk,
                            if is_last { now } else { None },
                            is_last && kill_priv,
                            is_last && kill_cap,
                        )
                        .await
                        .map_err(errno)?;
                }
                Ok((data.len(), mode_changed))
            })
            .await?;
        if kill_cap {
            inner.clear_security_cap_if(&self.0, cap_gen);
        }
        Ok((written, mode_changed))
    }

    /// Allocates `len` bytes of this file at `offset`, extending it if they are past its end, as
    /// an extent store can: the filesystem must have room for them, but none is set aside, as each
    /// write takes new blocks (fxfs-posix's `allocate`). It changes the file's modification time,
    /// as Linux's fallocate does (`file_modified`, the guest's too).
    pub async fn allocate(&self, offset: u64, len: u64) -> Result<()> {
        self.allocate_fuse(offset, len, false, false).await?;
        Ok(())
    }

    /// cladefs: FUSE `FALLOCATE` entry point with optional privilege clearing
    /// (`FUSE_HANDLE_KILLPRIV_V2`, since `fuse_fallocate_in` carries no `KILL_SUIDGID` flag).
    /// Returns whether `S_ISUID` or `S_ISGID` was cleared.
    pub async fn allocate_fuse(
        &self,
        offset: u64,
        len: u64,
        kill_suid_sgid: bool,
        kill_security_cap: bool,
    ) -> Result<bool> {
        let (inner, cap_gen) = self.file_and_cap()?;
        let end = offset.checked_add(len).filter(|_| len > 0).ok_or(Errno::EINVAL)?;
        let (ino, now) = (self.ino(), now()?);
        let kill_cap = kill_security_cap && cap_gen != 0;
        let mode_changed = inner
            .run(|fxfs| async move {
                fxfs.allocate_at(ino, offset..end, now, kill_suid_sgid, kill_cap)
                    .await
                    .map_err(errno)
            })
            .await?;
        if kill_cap {
            inner.clear_security_cap_if(&self.0, cap_gen);
        }
        Ok(mode_changed)
    }

    /// Writes back the node's data and metadata, and for a directory, the filesystem's.
    pub async fn fsync(&self) -> Result<()> {
        let (file_type, inner) = self.file_type()?;
        inner
            .run(|fxfs| async move {
                match file_type {
                    FileType::Directory => fxfs.sync().await,
                    FileType::Symlink => Ok(()),
                    _ => fxfs.fsync().await,
                }
                .map_err(errno)
            })
            .await
    }

    /// The value of the node's extended attribute `name`. Fails with `ENODATA` if it has none of
    /// that name.
    pub async fn getxattr(&self, name: &[u8]) -> Result<Vec<u8>> {
        let (_, cap_gen, inner) = self.file_type_and_cap()?;
        let is_cap = name == b"security.capability";
        let ino = self.ino();
        let res = inner
            .run(|fxfs| async move { fxfs.get_xattr(ino, name).await.map_err(xattr_errno) })
            .await;
        if is_cap && res == Err(Errno::ENODATA) {
            inner.clear_security_cap_if(&self.0, cap_gen);
        }
        res
    }

    /// Sets the node's extended attribute `name` to `value`, as `mode` says, changing its status
    /// change time, as Linux's filesystems do (the guest's `cladefs_vfs_xattr_set`).
    pub async fn setxattr(&self, name: &[u8], value: &[u8], mode: XattrMode) -> Result<()> {
        let inner = self.inner()?;
        if name == b"security.capability" {
            inner.mark_security_cap(&self.0);
        }
        let (ino, now) = (self.ino(), now()?);
        let mode = match mode {
            XattrMode::Set => SetExtendedAttributeMode::Set,
            XattrMode::Create => SetExtendedAttributeMode::Create,
            XattrMode::Replace => SetExtendedAttributeMode::Replace,
        };
        inner
            .run(|fxfs| async move {
                fxfs.set_xattr_at(ino, name, value, mode, now).await.map_err(xattr_errno)
            })
            .await
    }

    /// The names of the node's extended attributes.
    pub async fn listxattr(&self) -> Result<Vec<Vec<u8>>> {
        let inner = self.inner()?;
        let ino = self.ino();
        inner.run(|fxfs| async move { fxfs.list_xattrs(ino).await.map_err(xattr_errno) }).await
    }

    /// Removes the node's extended attribute `name`, changing its status change time. Fails with
    /// `ENODATA` if it has none of that name.
    pub async fn removexattr(&self, name: &[u8]) -> Result<()> {
        let (_, cap_gen, inner) = self.file_type_and_cap()?;
        let is_cap = name == b"security.capability";
        let (ino, now) = (self.ino(), now()?);
        inner
            .run(|fxfs| async move {
                fxfs.remove_xattr_at(ino, name, now).await.map_err(xattr_errno)
            })
            .await?;
        if is_cap {
            inner.clear_security_cap_if(&self.0, cap_gen);
        }
        Ok(())
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node").field("inode", &self.inode()).finish_non_exhaustive()
    }
}

fn handle(
    inner: &Arc<Inner>,
    found: &fxfs_posix::Attr,
    dir: u64,
    fresh: bool,
) -> Result<(Node, Attr)> {
    let attr = attr(inner.block_size, found)?;
    let hold = inner.hold(found.ino, attr.file_type, Some(dir), fresh)?;
    Ok((Node(Arc::new(hold)), attr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use fxfs::object_store::transaction::{LockKey, lock_keys};
    use std::collections::BTreeMap;

    const OWNER: Owner = Owner { uid: 1000, gid: 100 };

    #[test]
    fn cell_backed_filesystem_uses_atomic_barriers() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("image");
            let storage = Storage::create(&path, 32 << 20).await.unwrap();
            let check_barriers = |fs: &Filesystem| {
                let mut state = fs.inner.state.lock().unwrap();
                let mounted = state.mounted().unwrap();
                assert!(mounted.filesystem.options().barriers_enabled);
                assert!(mounted.filesystem.device().atomic_commits());
            };
            let fs = Filesystem::format(storage).await.unwrap();
            check_barriers(&fs);
            let (file, _) = fs.root().create(b"file", 0o644, OWNER).await.unwrap();
            let data: Vec<_> = (0..8192).map(|i| (i % 251) as u8).collect();
            assert_eq!(file.write(0, &data).await, Ok(data.len()));
            file.fsync().await.unwrap();
            drop(file);
            drop(fs.close().await.unwrap());

            let fs = Filesystem::mount(Storage::open(&path).await.unwrap()).await.unwrap();
            check_barriers(&fs);
            let (file, _) = fs.root().lookup(b"file").await.unwrap();
            let mut read = vec![0; data.len()];
            assert_eq!(file.read(0, &mut read).await, Ok(data.len()));
            assert_eq!(read, data);
            drop(file);
            fs.close().await.unwrap();
        });
    }

    /// A cell takes as many volumes as the guest's snapshot tests format on theirs, 256 and more on
    /// 128 MiB, one after another with nothing between them, then compacted (board 14222).
    #[test]
    fn hundreds_of_volumes_fit_a_small_cell() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("image");
            let fs =
                Filesystem::format(Storage::create(&path, 128 << 20).await.unwrap()).await.unwrap();
            for volume in 0..257 {
                if let Err(error) = fs.create_volume(&format!("v{volume}")).await {
                    panic!("creating volume {volume}: {error:#}");
                }
            }
            fs.compact().await.unwrap();
            fs.close().await.unwrap().check().await.unwrap();
            let storage = Storage::open(&path).await.unwrap();
            let fs = Options::new().volume("v256").mount(storage).await.unwrap();
            fs.close().await.unwrap();
        });
    }

    #[test]
    fn the_first_unmount_gives_its_result_and_later_ones_wait() {
        block_on(async {
            let fs = Filesystem::format(Storage::memory(8 << 20)).await.unwrap();
            let inner = Arc::clone(&fs.inner);
            let first = inner.begin_unmount();
            let second = inner.begin_unmount();
            assert_eq!((first.began(), second.began()), (true, false));
            assert_eq!(inner.fxfs().err(), Some(Errno::ENOTCONN));
            assert_eq!(fs.root().getattr().await.err(), Some(Errno::ENOTCONN));
            second.wait().await.expect("waiting failed");
            first.wait().await.expect("unmounting failed");
            drop(inner);
            fs.close().await.unwrap().check().await.unwrap();
        })
    }

    /// A node is held while a handle to it lives: one entry, however many handles, which goes
    /// with the last of them. Clones are the same handle.
    #[test]
    fn handles_hold_nodes_until_the_last_drops() {
        block_on(async {
            let fs = Filesystem::format(Storage::memory(8 << 20)).await.unwrap();
            let root = fs.root().inode().0;
            let held = || fs.inner.held();
            assert_eq!(held(), BTreeMap::from([(root, 1)]));

            let (file, _) = fs.root().create(b"file", 0o644, OWNER).await.unwrap();
            assert_eq!(file.write(0, b"contents").await, Ok(8));
            let id = file.inode().0;
            let clone = file.clone();
            assert_eq!(held(), BTreeMap::from([(root, 1), (id, 1)]));
            let (again, _) = fs.root().lookup(b"file").await.unwrap();
            assert_eq!(again.inode().0, id);
            assert_eq!(held(), BTreeMap::from([(root, 1), (id, 2)]));

            fs.root().unlink(b"file").await.unwrap();
            drop(file);
            assert_eq!(held(), BTreeMap::from([(root, 1), (id, 2)]));
            drop(clone);
            assert_eq!(held(), BTreeMap::from([(root, 1), (id, 1)]));
            let mut buf = [0; 16];
            assert_eq!(again.read(0, &mut buf).await, Ok(8));
            assert_eq!(&buf[..8], b"contents");
            drop(again);
            assert_eq!(held(), BTreeMap::from([(root, 1)]));
            fs.close().await.unwrap().check().await.unwrap();
        })
    }

    /// A directory held holds the directory it is in, which a handle to it alone keeps, until the
    /// last handle to it, or to a directory in it, drops: a rename moves the hold.
    #[test]
    fn held_directories_hold_their_parents() {
        block_on(async {
            let fs = Filesystem::format(Storage::memory(8 << 20)).await.unwrap();
            let root = fs.root().inode().0;
            let held = || fs.inner.held();
            let (outer, _) = fs.root().mkdir(b"outer", 0o755, OWNER).await.unwrap();
            let (inner, _) = outer.mkdir(b"inner", 0o755, OWNER).await.unwrap();
            let (other, _) = fs.root().mkdir(b"other", 0o755, OWNER).await.unwrap();
            let ids = (outer.inode().0, inner.inode().0, other.inode().0);
            drop(outer);
            assert_eq!(held(), BTreeMap::from([(root, 1), (ids.0, 0), (ids.1, 1), (ids.2, 1)]));
            let (outer, _) = fs.root().lookup(b"outer").await.unwrap();
            outer.rename(b"inner", &other, b"inner", RenameFlags::empty()).await.unwrap();
            drop(outer);
            assert_eq!(held(), BTreeMap::from([(root, 1), (ids.1, 1), (ids.2, 1)]));
            drop(other);
            assert_eq!(held(), BTreeMap::from([(root, 1), (ids.1, 1), (ids.2, 0)]));
            drop(inner);
            assert_eq!(held(), BTreeMap::from([(root, 1)]));
            fs.close().await.unwrap().check().await.unwrap();
        })
    }

    /// An unmounted filesystem dropped on Fxfs's runtime neither blocks it nor panics: only a
    /// mounted one's drop unmounts it.
    #[test]
    fn dropping_an_unmounted_filesystem_on_the_executor_neither_blocks_nor_panics() {
        let fs = block_on(Filesystem::format(Storage::memory(8 << 20))).unwrap();
        block_on(fs.unmount()).unwrap();
        let inner = Arc::clone(&fs.inner);
        let dropped = inner.run_blocking(async move {
            drop(fs);
            true
        });
        assert!(dropped);
    }

    /// Dropping mounted filesystems from worker threads of a shared multi-thread Tokio runtime
    /// (including all workers simultaneously or a 1-worker multi-thread runtime) hands off the
    /// worker core via `block_in_place` so teardown completes without deadlocking (hotbed reviews
    /// 111 and 112).
    #[test]
    fn dropping_mounted_filesystems_on_shared_multithread_runtime_completes() {
        for workers in [4usize, 1] {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .unwrap();
            let barrier = Arc::new(tokio::sync::Barrier::new(workers));
            let (tx, rx) = std::sync::mpsc::channel();
            for _ in 0..workers {
                let handle = rt.handle().clone();
                let barrier = Arc::clone(&barrier);
                let tx = tx.clone();
                rt.spawn(async move {
                    let mut options = Options::new();
                    options.runtime(handle);
                    let fs = options.format(Storage::memory(8 << 20)).await.unwrap();
                    barrier.wait().await;
                    drop(fs);
                    tx.send(()).unwrap();
                });
            }
            for _ in 0..workers {
                rx.recv_timeout(std::time::Duration::from_secs(30))
                    .expect("mounted filesystem drop deadlocked shared multi-thread runtime");
            }
        }
    }

    /// Dropping a mounted filesystem on its own `current_thread` runtime panics immediately rather
    /// than deadlocking forever.
    #[test]
    #[should_panic(expected = "current_thread runtime")]
    fn dropping_mounted_filesystem_on_own_current_thread_runtime_panics() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let handle = rt.handle().clone();
        rt.block_on(async move {
            let mut options = Options::new();
            options.runtime(handle);
            let fs = options.format(Storage::memory(8 << 20)).await.unwrap();
            drop(fs);
        });
    }

    /// A filesystem dropped while an unmount given up on is under way waits for that unmount to
    /// finish before `drop` returns, leaving the image clean and readable.
    #[test]
    fn dropping_a_filesystem_mid_unmount_closes_its_image_after_the_unmount() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("image");
            let fs =
                Filesystem::format(Storage::create(&path, 64 << 20).await.unwrap()).await.unwrap();
            let (file, _) = fs.root().create(b"file", 0o644, OWNER).await.unwrap();
            assert_eq!(file.write(0, b"written").await, Ok(7));
            drop(file);
            // Polled once, the unmount begins on the runtime, and is dropped while running.
            assert!(fs.unmount().now_or_never().is_none(), "the filesystem unmounted at once");
            let unmounted = fs.inner.begin_unmount();
            assert!(!unmounted.began());
            drop(fs);
            assert!(unmounted.wait().now_or_never().is_some(), "unmount not finished on drop");

            let fs = Filesystem::mount(Storage::open(&path).await.unwrap().check().await.unwrap())
                .await
                .unwrap();
            let (file, attr) = fs.root().lookup(b"file").await.unwrap();
            let mut contents = vec![0; attr.size as usize];
            assert_eq!(file.read(0, &mut contents).await, Ok(7));
            assert_eq!(contents, b"written");
            drop(file);
            fs.close().await.unwrap().check().await.unwrap();
        })
    }

    /// Holds the lock of Fxfs's that a transaction changing `object_id` takes until released.
    async fn hold_lock(inner: &Inner, object_id: u64) -> oneshot::Sender<()> {
        let fxfs = inner.fxfs().unwrap();
        let store = fxfs.filesystem().object_manager().store(fxfs.volume_id()).unwrap();
        drop(fxfs);
        let (locked, held) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        inner.runtime.spawn(async move {
            let keys = lock_keys![LockKey::object(store.store_object_id(), object_id)];
            let transaction = store.new_transaction(keys, Default::default()).await.unwrap();
            let _ = locked.send(());
            let _ = released.await;
            drop(transaction);
        });
        held.await.unwrap();
        release
    }

    /// Unmounting waits for an operation under way: an rmdir held by Fxfs's lock keeps the
    /// unmount waiting while new operations return `ENOTCONN`; released, it completes cleanly.
    #[test]
    fn unmounting_waits_for_an_operation_under_way() {
        block_on(async {
            let fs = Filesystem::format(Storage::memory(8 << 20)).await.unwrap();
            fs.root().mkdir(b"dir", 0o755, OWNER).await.unwrap();
            let inner = Arc::clone(&fs.inner);

            let release = hold_lock(&inner, fs.root().inode().0).await;
            let mut rmdir = Box::pin(fs.root().rmdir(b"dir"));
            assert!(futures::poll!(&mut rmdir).is_pending());
            let mut unmounted = Box::pin(inner.begin_unmount().wait());
            assert!(futures::poll!(&mut unmounted).is_pending(), "unmounted under an rmdir");
            assert_eq!(fs.root().getattr().await, Err(Errno::ENOTCONN));

            release.send(()).unwrap();
            assert_eq!(rmdir.await, Ok(()));
            unmounted.await.expect("unmounting failed");
            drop(inner);
            let fs = Filesystem::mount(fs.close().await.unwrap()).await.unwrap();
            assert_eq!(fs.root().lookup(b"dir").await.err(), Some(Errno::ENOENT));
            fs.close().await.unwrap().check().await.unwrap();
        })
    }

    /// Unmounting waits for writes under way and writes back what they wrote.
    #[test]
    fn unmounting_waits_for_writes_under_way() {
        const BLOCK: usize = Storage::BLOCK_SIZE as usize;
        block_on(async {
            let fs = Filesystem::format(Storage::memory(8 << 20)).await.unwrap();
            let (overwritten, _) = fs.root().create(b"overwritten", 0o644, OWNER).await.unwrap();
            let (extended, _) = fs.root().create(b"extended", 0o644, OWNER).await.unwrap();
            for file in [&overwritten, &extended] {
                assert_eq!(file.write(0, &[1; BLOCK]).await, Ok(BLOCK));
            }
            let inner = Arc::clone(&fs.inner);

            let data = [2; BLOCK];
            let releases = [
                hold_lock(&inner, overwritten.inode().0).await,
                hold_lock(&inner, extended.inode().0).await,
            ];
            let mut overwrite = Box::pin(overwritten.write(0, &data));
            assert!(futures::poll!(&mut overwrite).is_pending());
            let mut extend = Box::pin(extended.write(BLOCK as u64, &data));
            assert!(futures::poll!(&mut extend).is_pending());
            let mut unmounted = Box::pin(inner.begin_unmount().wait());
            assert!(futures::poll!(&mut unmounted).is_pending(), "unmounted under writes");

            for release in releases {
                release.send(()).unwrap();
            }
            assert_eq!(overwrite.await, Ok(BLOCK));
            assert_eq!(extend.await, Ok(BLOCK));
            unmounted.await.expect("unmounting failed");
            drop((overwritten, extended, inner));
            let fs = Filesystem::mount(fs.close().await.unwrap()).await.unwrap();
            let written =
                [(&b"overwritten"[..], data.to_vec()), (b"extended", [[1; BLOCK], data].concat())];
            for (name, written) in written {
                let (file, attr) = fs.root().lookup(name).await.unwrap();
                assert_eq!(attr.size, written.len() as u64);
                let mut contents = vec![0; written.len()];
                assert_eq!(file.read(0, &mut contents).await, Ok(written.len()));
                assert_eq!(contents, written);
            }
            fs.close().await.unwrap().check().await.unwrap();
        })
    }

    /// Dropping an inline zero-copy write future mid-lock-wait cancels it cleanly, releases its
    /// operation slot immediately so unmount proceeds without hanging, and leaves the filesystem
    /// consistent (board 11691).
    #[test]
    fn a_write_given_up_on_cancels_cleanly_before_unmount() {
        const BLOCK: usize = Storage::BLOCK_SIZE as usize;
        block_on(async {
            let fs = Filesystem::format(Storage::memory(8 << 20)).await.unwrap();
            let (file, _) = fs.root().create(b"file", 0o644, OWNER).await.unwrap();
            assert_eq!(file.write(0, &[1; BLOCK]).await, Ok(BLOCK));
            let inner = Arc::clone(&fs.inner);

            let release = hold_lock(&inner, file.inode().0).await;
            let data = [2; BLOCK];
            let mut write = Box::pin(file.write(0, &data));
            assert!(futures::poll!(&mut write).is_pending());
            drop(write);
            release.send(()).unwrap();
            fs.unmount().await.expect("unmounting failed");
            drop((file, inner));
            let fs =
                Filesystem::mount(fs.close().await.unwrap().check().await.unwrap()).await.unwrap();
            let (file, _) = fs.root().lookup(b"file").await.unwrap();
            let mut contents = [0; BLOCK];
            assert_eq!(file.read(0, &mut contents).await, Ok(BLOCK));
            assert_eq!(contents, [1; BLOCK]);
            drop(file);
            fs.close().await.unwrap().check().await.unwrap();
        })
    }

    /// Names are refused as a VFS refuses them: too long, or `.`, `..`, empty, or holding `/` or
    /// NUL, or not in UTF-8, which Fxfs keeps names in.
    #[test]
    fn invalid_names_are_refused() {
        assert_eq!(valid_name(&[b'a'; 256]), Err(Errno::ENAMETOOLONG));
        assert_eq!(valid_name(&[b'a'; 255]), Ok("a".repeat(255)));
        for name in [&b""[..], b".", b"..", b"a/b", b"a\0b", b"\xff"] {
            assert_eq!(valid_name(name), Err(Errno::EINVAL), "{name:?}");
        }
    }

    /// board 11691: inline `EnterFuture` entering a multi-thread handle on a `current_thread`
    /// Tokio runtime (or `futures::executor::block_on`) must not panic inside
    /// `local_cell::upload::blocking` (`tokio::task::block_in_place`) on cell-backed storage.
    #[test]
    fn cell_backed_filesystem_works_from_current_thread_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let storage = Storage::create(&dir.path().join("cell"), 64 << 20).await.unwrap();
            let fs = Filesystem::format(storage).await.unwrap();
            let (file, _) = fs.root().create(b"hello", 0o644, OWNER).await.unwrap();
            assert_eq!(file.write(0, b"inline").await, Ok(6));
            drop(file);
            fs.close().await.unwrap().check().await.unwrap();
        });
    }

    /// Dropping a mounted `Filesystem` on the same task that holds a suspended inline `Node`
    /// operation future panics immediately rather than deadlocking in `operations.idle()`
    /// (hotbed review 140 finding 518).
    #[test]
    #[should_panic(expected = "in-flight Node operation")]
    fn dropping_mounted_filesystem_with_same_task_write_panics_instead_of_deadlocking() {
        const BLOCK: usize = Storage::BLOCK_SIZE as usize;
        block_on(async {
            let fs = Filesystem::format(Storage::memory(8 << 20)).await.unwrap();
            let (file, _) = fs.root().create(b"file", 0o644, OWNER).await.unwrap();
            let inner = Arc::clone(&fs.inner);
            let _release = hold_lock(&inner, file.inode().0).await;
            let data = [3; BLOCK];
            let mut write = Box::pin(file.write(0, &data));
            assert!(futures::poll!(&mut write).is_pending());
            drop(fs);
        });
    }
}
