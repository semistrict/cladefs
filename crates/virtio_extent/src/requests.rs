// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Serving the device's requests: each request's buffers ([`Buffers`]) read, the store asked
//! ([`Backend`]), and what it returns and the response written back. The device applies requests in
//! any order but across barriers (doc-4), so a guest's `MAP` doesn't wait for the writes and
//! durable commits queued before it: each is served as a future ([`handle`]), several at once. On
//! Linux, [`Requests`] serves them on the threads receiving the daemon's replies, which go on with
//! them, completing each request (`cladefs::extentd::spawn_on_replies`): no thread is woken between
//! the reply and the guest's interrupt, nor waits for the store's I/O (TASK-41).

use std::sync::atomic::{AtomicU64, Ordering};

use cladefs::{Commit, Errno};
use log::{error, info};

use crate::backend::Backend;
use crate::buffers::{Buffers, Reader, Writer};
use crate::config::CacheType;
use crate::notices::Notices;
use crate::protocol::*;

/// The requests served, and the time serving them took, by type, logged as a VMM's worker stops:
/// against what the guest waits for a request (its driver's request_ns), what the transport
/// costs.
#[derive(Debug, Default)]
pub struct Served {
    count: [AtomicU64; VIRTIO_EXTENT_T_LAST as usize + 1],
    nanos: [AtomicU64; VIRTIO_EXTENT_T_LAST as usize + 1],
}

impl Served {
    /// A request of type `type_`, if known, took `nanos` to serve.
    pub fn add(&self, type_: Option<u32>, nanos: u64) {
        if let Some(type_) = type_.filter(|t| *t <= VIRTIO_EXTENT_T_LAST) {
            self.count[type_ as usize].fetch_add(1, Ordering::Relaxed);
            self.nanos[type_ as usize].fetch_add(nanos, Ordering::Relaxed);
        }
    }

    /// How many requests of type `type_` were served, and in how many nanoseconds in all.
    pub fn of(&self, type_: u32) -> (u64, u64) {
        let type_ = type_ as usize;
        match (self.count.get(type_), self.nanos.get(type_)) {
            (Some(count), Some(nanos)) => {
                (count.load(Ordering::Relaxed), nanos.load(Ordering::Relaxed))
            }
            _ => (0, 0),
        }
    }

    /// Logs the requests served of each type, and the mean time each took.
    pub fn log(&self) {
        for type_ in 0..=VIRTIO_EXTENT_T_LAST {
            let (count, nanos) = self.of(type_);
            if count > 0 {
                #[allow(clippy::cast_precision_loss)]
                let mean = nanos as f64 / count as f64;
                info!(
                    "extent device: served {count} requests of type {type_}, {:.1} us each",
                    mean / 1000.0
                );
            }
        }
    }
}

/// Serves the request whose buffers are `buffers` with `store`, its guest's notices `notices`, and
/// returns the length of what the device writes to it: its writable buffers, whose last bytes hold
/// the response. The VMM returns the request to the driver with that length.
pub async fn handle<S: Backend>(
    store: &S,
    notices: &Notices,
    cache: CacheType,
    buffers: Buffers,
) -> u32 {
    // The response is the chain's last bytes, after what the request returns.
    let writable = buffers.writable_len();
    let Some(room) = writable.checked_sub(size_of::<Response>()) else {
        error!("extent device: a request has no room for its response");
        return 0;
    };
    let mut reader = Reader::new(&buffers);
    let writer = Writer::new(&buffers);
    let (status, written) = match serve(store, notices, cache, &mut reader, &writer, room).await {
        Ok(written) => (0, written),
        Err(errno) => (errno.raw().cast_unsigned(), 0),
    };
    // What the request didn't return reads as zeros, as drivers read the buffers whole.
    writer.zero(written, room - written);
    writer.obj(room, &Response { status: status.to_le(), reserved: 0 });
    u32::try_from(writable).expect("a request's buffers' writable length fits in u32 (Buffers)")
}

/// Serves the request `reader` holds, writing what it returns with `writer`, which has `room`
/// bytes for it, and returns how many it wrote.
async fn serve<S: Backend>(
    store: &S,
    notices: &Notices,
    cache: CacheType,
    reader: &mut Reader<'_>,
    writer: &Writer<'_>,
    room: usize,
) -> Result<usize, Errno> {
    let header: RequestHeader = reader.obj()?;
    let type_ = u32::from_le(header.type_);
    let flags = match cache {
        CacheType::Writeback => u32::from_le(header.flags),
        CacheType::Unsafe => undurable(type_, u32::from_le(header.flags)),
    };
    match type_ {
        VIRTIO_EXTENT_T_READ => {
            let args: IoArgs = reader.obj()?;
            let len = u32::from_le(args.len);
            if len > MAX_TRANSFER || room < len as usize {
                return Err(Errno::EINVAL);
            }
            let mut slices = writer.slices(0, len as usize).ok_or(Errno::EINVAL)?;
            store.read(args.id.into(), u64::from_le(args.offset), &mut slices).await?;
            Ok(len as usize)
        }
        VIRTIO_EXTENT_T_WRITE => {
            let args: IoArgs = reader.obj()?;
            let len = u32::from_le(args.len);
            if len > MAX_TRANSFER {
                return Err(Errno::EINVAL);
            }
            let data = reader.slices(len as usize)?;
            // A barrier before orders, without waiting for durability (doc-4); one after, with
            // WRITE_COMMIT, waits as a COMMIT does. One round trip to the daemon either way.
            let mut barriers = write_barriers(flags).ok_or(Errno::EINVAL)?;
            // cladefs: the root that commit sets, after the data (WRITE_ROOT).
            let root;
            if flags & VIRTIO_EXTENT_WRITE_ROOT != 0 {
                let len = u32::from_le(args.root_len);
                if len > MAX_ROOT {
                    return Err(Errno::EINVAL);
                }
                root = reader.bytes(len as usize)?;
                barriers.root = Some(&root);
            }
            store.write(args.id.into(), u64::from_le(args.offset), &data, barriers).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_FREE => {
            if flags & !VIRTIO_EXTENT_FREE_MANY != 0 {
                return Err(Errno::EINVAL);
            }
            let id: ExtentIdLe = reader.obj()?;
            let mut extents = vec![id.into()];
            // cladefs: the rest of a flush's dead extents, each freed as it would be alone, in one
            // request of the daemon's, not one each (TASK-92.13): the first error fails the
            // request, the extents before it freed (`F_FREE_MANY`); refused past `MAX_MANY_DATA`.
            if flags & VIRTIO_EXTENT_FREE_MANY != 0 {
                if reader.available() > MAX_MANY_DATA {
                    return Err(Errno::EINVAL);
                }
                while reader.available() > 0 {
                    let id: ExtentIdLe = reader.obj()?;
                    extents.push(id.into());
                }
            }
            store.free_many(&extents).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_MAP => {
            let args: MapArgs = reader.obj()?;
            // cladefs: every map is read-only; a window that copies on write, a writer's, maps
            // zeros, and a follower's none (backlog doc-13).
            let map_flags = VIRTIO_EXTENT_MAP_AHEAD
                | if store.copies_on_write() { VIRTIO_EXTENT_MAP_ZEROS } else { 0 };
            if flags & !(map_flags | VIRTIO_EXTENT_MAP_MANY) != 0 {
                return Err(Errno::EINVAL);
            }
            let mut maps = vec![(args, flags & map_flags)];
            // cladefs: the rest of a chunk's pieces (`F_MAP_MANY`), each with its own flags; refused
            // past `MAX_MANY_DATA`.
            if flags & VIRTIO_EXTENT_MAP_MANY != 0 {
                if reader.available() > MAX_MANY_DATA {
                    return Err(Errno::EINVAL);
                }
                while reader.available() > 0 {
                    let entry: MapEntry = reader.obj()?;
                    let flags = u32::from_le(entry.flags);
                    if flags & !map_flags != 0 {
                        return Err(Errno::EINVAL);
                    }
                    maps.push((entry.map, flags));
                }
            }
            // Sent to the daemon at once, each as it would be alone: the first error fails the
            // request, whichever others it mapped (`F_MAP_MANY`). cladefs: what the guest reads
            // next (`F_MAP_AHEAD`) is then mapped for it (TASK-112.1).
            futures::future::try_join_all(maps.iter().map(|(args, flags)| async move {
                let (window_offset, len) =
                    (u64::from_le(args.window_offset), u64::from_le(args.len));
                if flags & VIRTIO_EXTENT_MAP_ZEROS != 0 {
                    store.map_zeros(window_offset, len).await?;
                } else {
                    let offset = u64::from_le(args.offset);
                    store.map(args.id.into(), offset, len, window_offset).await?;
                }
                if flags & VIRTIO_EXTENT_MAP_AHEAD != 0 {
                    store.prefault(window_offset, len);
                }
                Ok::<_, Errno>(())
            }))
            .await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_UNMAP => {
            let args: UnmapArgs = reader.obj()?;
            store.unmap(u64::from_le(args.window_offset), u64::from_le(args.len)).await?;
            Ok(0)
        }
        // cladefs: a window that copies on write's, a writer's (backlog doc-13), its arguments UNMAP's.
        VIRTIO_EXTENT_T_PRIVATE => {
            if flags != 0 || !store.copies_on_write() {
                return Err(Errno::EINVAL);
            }
            let args: UnmapArgs = reader.obj()?;
            store.make_private(u64::from_le(args.window_offset), u64::from_le(args.len)).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_COMMIT => {
            let args: CommitArgs = reader.obj()?;
            let mut root = None;
            if flags & VIRTIO_EXTENT_COMMIT_ROOT != 0 {
                let len = u32::from_le(args.root_len);
                if len > MAX_ROOT {
                    return Err(Errno::EINVAL);
                }
                root = Some(reader.bytes(len as usize)?);
            }
            // Without COMMIT_DURABLE, the commit completes once ordered: the daemon makes it
            // durable soon after (doc-4).
            let commit = if flags & VIRTIO_EXTENT_COMMIT_DURABLE != 0 {
                Commit::Durable
            } else {
                Commit::Ordered
            };
            store.commit(root.as_deref(), commit).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_ROOT => {
            let returned = MAX_ROOT as usize + size_of::<RootLen>();
            let root = store.root().await?;
            if room < returned || root.len() > MAX_ROOT as usize {
                return Err(Errno::EINVAL);
            }
            writer.write(0, &root);
            writer.zero(root.len(), MAX_ROOT as usize - root.len());
            let len = u32::try_from(root.len()).expect("the root is short");
            writer.obj(MAX_ROOT as usize, &RootLen { len: len.to_le(), reserved: 0 });
            Ok(returned)
        }
        VIRTIO_EXTENT_T_INFO => {
            // cladefs: the whole of it, the branch too, which a driver without F_FREEZE had no room
            // for, served before: a driver of this protocol has (TASK-141.3).
            if room < size_of::<Info>() {
                return Err(Errno::EINVAL);
            }
            let info = store.info().await?;
            let info = Info {
                capacity: info.capacity.to_le(),
                used: info.used.to_le(),
                available: info.available.to_le(),
                next_sequence: info.next_sequence.to_le(),
                sequence_floor: info.sequence_floor.to_le(),
                branch: store.branch().to_le(),
            };
            writer.obj(0, &info);
            Ok(size_of::<Info>())
        }
        VIRTIO_EXTENT_T_FREEZE => {
            notices.set_frozen(flags & VIRTIO_EXTENT_FREEZE_FROZEN != 0);
            Ok(0)
        }
        // cladefs: the guest's stats, which the daemon asked for (TASK-111.4).
        VIRTIO_EXTENT_T_STATS => {
            let args: StatsArgs = reader.obj()?;
            if reader.available() > MAX_STATS {
                return Err(Errno::EINVAL);
            }
            let text = reader.bytes(reader.available())?;
            let text = std::str::from_utf8(&text).map_err(|_| Errno::EINVAL)?;
            store.answer_stats(u64::from_le(args.asked), Some(text)).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_RESIZED => {
            let args: ResizedArgs = reader.obj()?;
            let capacity = u64::from_le(args.capacity);
            let took = flags & VIRTIO_EXTENT_RESIZED_TOOK != 0;
            notices.resizing.answered(capacity, took);
            store.resized(capacity, took).await?;
            Ok(0)
        }
        // cladefs: a follower's (doc-4's F_READ_ONLY); a writer's store refuses them (EINVAL).
        VIRTIO_EXTENT_T_ADVANCE => {
            if room < size_of::<Epoch>() {
                return Err(Errno::EINVAL);
            }
            let epoch = Epoch { epoch: store.advance().await?.to_le(), reserved: 0 };
            writer.obj(0, &epoch);
            Ok(size_of::<Epoch>())
        }
        VIRTIO_EXTENT_T_LET_GO => {
            let epoch: Epoch = reader.obj()?;
            store.let_go(u64::from_le(epoch.epoch)).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_PIN => {
            let id: ExtentIdLe = reader.obj()?;
            store.pin(id.into()).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_UNPIN => {
            let id: ExtentIdLe = reader.obj()?;
            store.unpin(id.into()).await?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// The type of a request whose header is `header`, as its stats count it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn type_of(header: Option<RequestHeader>) -> Option<u32> {
    header.map(|header| u32::from_le(header.type_))
}

/// An object `T` from the start of `bytes`, which holds one: for tests reading what the device
/// wrote.
#[cfg(test)]
pub(crate) fn read_obj<T: zerocopy::FromBytes>(bytes: &[u8]) -> T {
    T::read_from_prefix(bytes).expect("room for the object").0
}

#[cfg(target_os = "linux")]
pub use self::linux::Requests;

#[cfg(target_os = "linux")]
mod linux {
    use std::fmt::Display;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use cladefs::DaemonExtentStore;
    use log::error;

    use super::{Served, handle, type_of};
    use crate::buffers::Buffers;
    use crate::config::CacheType;
    use crate::notices::Notices;

    /// The requests of one of the device's queues, which its VMM's worker takes off the queue:
    /// served on the thread taking them until they wait for the daemon, then on the threads
    /// receiving the daemon's replies, which complete them. Counts those in flight, which the
    /// worker waits for as it stops ([`Requests::wait_idle`]), and what it served of each type.
    pub struct Requests {
        store: Arc<DaemonExtentStore>,
        notices: Arc<Notices>,
        /// cladefs: what it does with the guest's asks for durability (TASK-363).
        cache: CacheType,
        served: Served,
        /// The requests taken off the queue and not yet returned to the driver.
        serving: Mutex<usize>,
        /// Notified as the last of them is returned.
        idle: Condvar,
    }

    impl std::fmt::Debug for Requests {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Requests")
                .field("branch", &self.store.branch())
                .field("cache", &self.cache)
                .field("serving", &self.serving)
                .finish_non_exhaustive()
        }
    }

    impl Requests {
        /// Requests served with `store`, as `notices` and `cache` say: those of a device
        /// ([`Device::requests`](crate::Device::requests)).
        pub fn new(store: Arc<DaemonExtentStore>, notices: Arc<Notices>, cache: CacheType) -> Self {
            Self {
                store,
                notices,
                cache,
                served: Served::default(),
                serving: Mutex::new(0),
                idle: Condvar::new(),
            }
        }

        /// Serves the request whose buffers are `buffers`, or that are no request's, the error
        /// saying why, on this thread until it waits for the daemon, and then on the thread
        /// receiving the daemon's reply, and calls `complete` with the length the device wrote
        /// to it ([`handle`]), 0 if they are no request's: the VMM returns it to the driver.
        pub fn serve<E: Display + Send + 'static>(
            self: &Arc<Self>,
            buffers: Result<Buffers, E>,
            complete: impl FnOnce(u32) + Send + 'static,
        ) {
            *self.serving.lock().expect("Poisoned lock") += 1;
            let start = Instant::now();
            let header = buffers.as_ref().ok().and_then(Buffers::header);
            // cladefs: served with the guest's number of it current, which the daemon's client's
            // calls for it carry to the daemon's log (`cladefs::request`, TASK-260): 0, none, if
            // unread.
            let unique = header.map_or(0, |header| u64::from_le(header.unique));
            let requests = self.clone();
            let served = async move {
                let len = match buffers {
                    Ok(buffers) => {
                        handle(&*requests.store, &requests.notices, requests.cache, buffers).await
                    }
                    Err(err) => {
                        error!("extent device: {err}");
                        0
                    }
                };
                #[allow(clippy::cast_possible_truncation)]
                requests.served.add(type_of(header), start.elapsed().as_nanos() as u64);
                complete(len);
                let mut serving = requests.serving.lock().expect("Poisoned lock");
                *serving -= 1;
                if *serving == 0 {
                    requests.idle.notify_all();
                }
            };
            cladefs::extentd::spawn_on_replies(cladefs::request::guest_request(unique, served));
        }

        /// Waits until the requests taken are returned to the driver.
        pub fn wait_idle(&self) {
            let serving = self.serving.lock().expect("Poisoned lock");
            drop(self.idle.wait_while(serving, |serving| *serving > 0).expect("Poisoned lock"));
        }

        /// What was served of each type.
        pub fn served(&self) -> &Served {
            &self.served
        }
    }
}

#[cfg(test)]
mod tests {
    //! Requests served with a fake store, in fake guest memory: what the device makes of each
    //! request, apart from any daemon (tests/daemon.rs serves them with one).

    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use cladefs::{ExtentId, ExtentInfo, VolatileSlice, VolatileSliceMut, WriteBarriers};
    use zerocopy::{Immutable, IntoBytes};

    use super::*;

    const RESPONSE: usize = size_of::<Response>();

    /// A store in memory, which records what it is asked: extents, sealed by a commit; a root; a
    /// window's maps; and the guest's stats and answers.
    #[derive(Default)]
    struct FakeBackend {
        copies_on_write: bool,
        state: Mutex<FakeState>,
    }

    /// A map of the window: an extent and the offset in it, or zeros; its length; and where in the
    /// window it is.
    type Map = (Option<(ExtentId, u64)>, u64, u64);

    #[derive(Default)]
    struct FakeState {
        extents: BTreeMap<ExtentId, (Vec<u8>, bool)>,
        root: Vec<u8>,
        commits: Vec<Commit>,
        maps: Vec<Map>,
        prefaulted: Vec<(u64, u64)>,
        unmapped: Vec<(u64, u64)>,
        private: Vec<(u64, u64)>,
        stats: Vec<(u64, String)>,
        resized: Vec<(u64, bool)>,
    }

    impl FakeBackend {
        fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.state.lock().unwrap()
        }
    }

    const BRANCH: u64 = 7;

    impl Backend for FakeBackend {
        fn branch(&self) -> u64 {
            BRANCH
        }

        fn copies_on_write(&self) -> bool {
            self.copies_on_write
        }

        async fn read(
            &self,
            extent: ExtentId,
            offset: u64,
            buffers: &mut [VolatileSliceMut<'_>],
        ) -> Result<(), Errno> {
            let state = self.state();
            let (data, _) = state.extents.get(&extent).ok_or(Errno::ENOENT)?;
            let mut at = offset as usize;
            for buffer in buffers {
                let end = (at + buffer.len()).min(data.len());
                at += buffer.copy_from(data.get(at..end).ok_or(Errno::EINVAL)?);
            }
            Ok(())
        }

        async fn write(
            &self,
            extent: ExtentId,
            offset: u64,
            buffers: &[VolatileSlice<'_>],
            barriers: WriteBarriers<'_>,
        ) -> Result<(), Errno> {
            let mut state = self.state();
            if barriers.before {
                state.commits.push(Commit::Ordered);
            }
            let (data, sealed) = state.extents.entry(extent).or_default();
            if *sealed {
                return Err(Errno::EPERM);
            }
            let mut at = offset as usize;
            for buffer in buffers {
                data.resize(data.len().max(at + buffer.len()), 0);
                at += buffer.copy_to(&mut data[at..]);
            }
            if let Some(commit) = barriers.after {
                state.extents.values_mut().for_each(|(_, sealed)| *sealed = true);
                state.commits.push(commit);
                if let Some(root) = barriers.root {
                    state.root = root.to_vec();
                }
            }
            Ok(())
        }

        async fn free_many(&self, extents: &[ExtentId]) -> Result<(), Errno> {
            let mut state = self.state();
            for extent in extents {
                state.extents.remove(extent).ok_or(Errno::ENOENT)?;
            }
            Ok(())
        }

        async fn map(
            &self,
            extent: ExtentId,
            offset: u64,
            len: u64,
            window_offset: u64,
        ) -> Result<(), Errno> {
            self.state().maps.push((Some((extent, offset)), len, window_offset));
            Ok(())
        }

        async fn map_zeros(&self, window_offset: u64, len: u64) -> Result<(), Errno> {
            self.state().maps.push((None, len, window_offset));
            Ok(())
        }

        fn prefault(&self, window_offset: u64, len: u64) {
            self.state().prefaulted.push((window_offset, len));
        }

        async fn unmap(&self, window_offset: u64, len: u64) -> Result<(), Errno> {
            self.state().unmapped.push((window_offset, len));
            Ok(())
        }

        async fn make_private(&self, window_offset: u64, len: u64) -> Result<(), Errno> {
            self.state().private.push((window_offset, len));
            Ok(())
        }

        async fn commit(&self, root: Option<&[u8]>, commit: Commit) -> Result<(), Errno> {
            let mut state = self.state();
            state.extents.values_mut().for_each(|(_, sealed)| *sealed = true);
            state.commits.push(commit);
            if let Some(root) = root {
                state.root = root.to_vec();
            }
            Ok(())
        }

        async fn root(&self) -> Result<Vec<u8>, Errno> {
            Ok(self.state().root.clone())
        }

        async fn info(&self) -> Result<ExtentInfo, Errno> {
            let used = self.state().extents.values().map(|(data, _)| data.len() as u64).sum();
            Ok(ExtentInfo {
                capacity: 1 << 20,
                used,
                available: (1 << 20) - used,
                next_sequence: 3,
                sequence_floor: 1 << 40,
            })
        }

        async fn answer_stats(&self, ask: u64, guest: Option<&str>) -> Result<(), Errno> {
            self.state().stats.push((ask, guest.unwrap_or_default().to_owned()));
            Ok(())
        }

        async fn resized(&self, capacity: u64, took: bool) -> Result<(), Errno> {
            self.state().resized.push((capacity, took));
            Ok(())
        }

        async fn advance(&self) -> Result<u64, Errno> {
            Err(Errno::EINVAL)
        }

        async fn let_go(&self, _epoch: u64) -> Result<(), Errno> {
            Err(Errno::EINVAL)
        }

        async fn pin(&self, _extent: ExtentId) -> Result<(), Errno> {
            Err(Errno::EINVAL)
        }

        async fn unpin(&self, _extent: ExtentId) -> Result<(), Errno> {
            Err(Errno::EINVAL)
        }
    }

    /// The bytes of a request's header and arguments, followed by `data`.
    fn bytes<T: IntoBytes + Immutable>(
        type_: u32,
        flags: u32,
        args: Option<T>,
        data: &[u8],
    ) -> Vec<u8> {
        let header = RequestHeader { type_: type_.to_le(), flags: flags.to_le(), unique: 7 };
        let mut bytes = header.as_bytes().to_vec();
        if let Some(args) = &args {
            bytes.extend_from_slice(args.as_bytes());
        }
        bytes.extend_from_slice(data);
        bytes
    }

    /// Serves a request of `readable` bytes, with `writable` bytes for the device to write, the
    /// response included, in fake guest memory split into descriptors of at most `split` bytes,
    /// as `cache` says; returns what the device wrote, the response apart, and the response's
    /// status.
    fn request_as(
        store: &FakeBackend,
        notices: &Notices,
        cache: CacheType,
        readable: &[u8],
        writable: usize,
        split: usize,
    ) -> (Vec<u8>, u32) {
        let mut readable = readable.to_vec();
        let mut written = vec![0xa5; writable];
        let mut buffers = Buffers::new();
        for part in readable.chunks_mut(split) {
            // SAFETY: The test's memory, which outlives the request.
            unsafe { buffers.push_readable(part.as_mut_ptr(), part.len()) };
        }
        for part in written.chunks_mut(split) {
            // SAFETY: As above, which nothing else accesses meanwhile.
            unsafe { buffers.push_writable(part.as_mut_ptr(), part.len()) }.unwrap();
        }
        let used = futures::executor::block_on(handle(store, notices, cache, buffers)) as usize;
        assert_eq!(used, writable, "the device wrote {used} of {writable} bytes");
        let response: Response = read_obj(&written[writable - RESPONSE..]);
        written.truncate(writable - RESPONSE);
        (written, u32::from_le(response.status))
    }

    fn request(store: &FakeBackend, readable: &[u8], writable: usize) -> (Vec<u8>, u32) {
        request_as(store, &Notices::default(), CacheType::Writeback, readable, writable, usize::MAX)
    }

    fn errno(errno: Errno) -> u32 {
        errno.raw().cast_unsigned()
    }

    fn io(sequence: u64, offset: u64, len: usize) -> IoArgs {
        IoArgs {
            id: ExtentIdLe { branch: BRANCH.to_le(), sequence: sequence.to_le() },
            offset: offset.to_le(),
            len: (len as u32).to_le(),
            root_len: 0,
        }
    }

    fn id(sequence: u64) -> ExtentId {
        ExtentId { branch: BRANCH, sequence }
    }

    /// Writes and reads move the bytes the driver gave, across descriptors of any size; a commit
    /// seals what was written; a FREE frees.
    #[test]
    fn writes_and_reads_go_through_the_store() {
        let store = FakeBackend::default();
        let data: Vec<u8> = (0..3000).map(|i| i as u8).collect();
        let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(1, 0, data.len())), &data);
        let notices = Notices::default();
        let cache = CacheType::Writeback;
        assert_eq!(request_as(&store, &notices, cache, &write, RESPONSE, 777), (vec![], 0));
        let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(1, 0, data.len())), &[]);
        let writable = data.len() + RESPONSE;
        assert_eq!(request(&store, &read, writable), (data.clone(), 0));
        assert_eq!(request_as(&store, &notices, cache, &read, writable, 1000), (data.clone(), 0));
        // A read with less room than it reads: refused, its room zeroed.
        assert_eq!(request(&store, &read, 100 + RESPONSE), (vec![0; 100], errno(Errno::EINVAL)));
        let missing = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(2, 0, 10)), &[]);
        assert_eq!(request(&store, &missing, 10 + RESPONSE), (vec![0; 10], errno(Errno::ENOENT)));

        let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
        assert_eq!(request(&store, &commit, RESPONSE), (vec![], 0));
        assert_eq!(request(&store, &write, RESPONSE).1, errno(Errno::EPERM));
        let free = bytes(VIRTIO_EXTENT_T_FREE, 0, Some(io(1, 0, 0).id), &[]);
        assert_eq!(request(&store, &free, RESPONSE), (vec![], 0));
        assert_eq!(request(&store, &read, writable).1, errno(Errno::ENOENT));
    }

    /// A WRITE's flags order it: a barrier before; a commit after, durable or ordered, setting the
    /// root after the data; none but a WRITE's.
    #[test]
    fn writes_carry_their_barriers() {
        let store = FakeBackend::default();
        let rooted = IoArgs { root_len: 6u32.to_le(), ..io(1, 0, 4) };
        let flags = VIRTIO_EXTENT_WRITE_BARRIER
            | VIRTIO_EXTENT_WRITE_COMMIT
            | VIRTIO_EXTENT_WRITE_DURABLE
            | VIRTIO_EXTENT_WRITE_ROOT;
        let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(rooted), b"datasecond");
        assert_eq!(request(&store, &write, RESPONSE), (vec![], 0));
        assert_eq!(store.state().commits, vec![Commit::Ordered, Commit::Durable]);
        assert_eq!(store.state().root, b"second");
        assert_eq!(store.state().extents[&id(1)].0, b"data");
        // A root, or durability, without a commit; an unknown flag; a root past the most.
        for flags in [VIRTIO_EXTENT_WRITE_ROOT, VIRTIO_EXTENT_WRITE_DURABLE, 1 << 4] {
            let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(rooted), b"datasecond");
            assert_eq!(request(&store, &write, RESPONSE).1, errno(Errno::EINVAL), "{flags:#x}");
        }
        let long = IoArgs { root_len: (MAX_ROOT + 1).to_le(), ..io(2, 0, 0) };
        let root = vec![1; MAX_ROOT as usize + 1];
        let flags = VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_ROOT;
        let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(long), &root);
        assert_eq!(request(&store, &write, RESPONSE).1, errno(Errno::EINVAL));
        // Data shorter than the write says.
        let short = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(3, 0, 100)), &[1; 50]);
        assert_eq!(request(&store, &short, RESPONSE).1, errno(Errno::EINVAL));
        let past = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(3, 0, MAX_TRANSFER as usize + 1)), &[]);
        assert_eq!(request(&store, &past, RESPONSE).1, errno(Errno::EINVAL));
    }

    /// cladefs TASK-363: an Unsafe device's commits, and writes' commits, are ordered, whatever the
    /// driver asked.
    #[test]
    fn unsafe_devices_commit_ordered() {
        let store = FakeBackend::default();
        let notices = Notices::default();
        let durable = bytes(
            VIRTIO_EXTENT_T_COMMIT,
            VIRTIO_EXTENT_COMMIT_DURABLE,
            Some(CommitArgs::default()),
            &[],
        );
        let flags = VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_DURABLE;
        let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(io(1, 0, 1)), b"x");
        for request in [&durable, &write] {
            let served = request_as(&store, &notices, CacheType::Unsafe, request, RESPONSE, 64);
            assert_eq!(served, (vec![], 0));
        }
        let served =
            request_as(&store, &notices, CacheType::Writeback, &durable, RESPONSE, usize::MAX);
        assert_eq!(served, (vec![], 0));
        assert_eq!(store.state().commits, vec![Commit::Ordered, Commit::Ordered, Commit::Durable]);
    }

    /// ROOT writes the root, zeros to its most, and its length; COMMIT with COMMIT_ROOT sets it;
    /// INFO writes the store's whole, the branch too, refusing room for less (TASK-141.3).
    #[test]
    fn roots_and_info_are_written_whole() {
        let store = FakeBackend::default();
        let commit = bytes(
            VIRTIO_EXTENT_T_COMMIT,
            VIRTIO_EXTENT_COMMIT_ROOT,
            Some(CommitArgs { root_len: 5u32.to_le(), reserved: 0 }),
            b"first",
        );
        assert_eq!(request(&store, &commit, RESPONSE), (vec![], 0));
        let root = bytes::<RootLen>(VIRTIO_EXTENT_T_ROOT, 0, None, &[]);
        let root_len = MAX_ROOT as usize + size_of::<RootLen>();
        let (written, status) = request(&store, &root, root_len + RESPONSE);
        assert_eq!(status, 0);
        assert_eq!(&written[..5], b"first");
        assert!(written[5..MAX_ROOT as usize].iter().all(|b| *b == 0));
        assert_eq!(read_obj::<RootLen>(&written[MAX_ROOT as usize..]).len, 5u32.to_le());
        assert_eq!(request(&store, &root, root_len - 1 + RESPONSE).1, errno(Errno::EINVAL));

        let info = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
        let (written, status) = request(&store, &info, size_of::<Info>() + RESPONSE);
        assert_eq!(status, 0);
        let expected = Info {
            capacity: (1u64 << 20).to_le(),
            used: 0,
            available: (1u64 << 20).to_le(),
            next_sequence: 3u64.to_le(),
            sequence_floor: (1u64 << 40).to_le(),
            branch: BRANCH.to_le(),
        };
        assert_eq!(read_obj::<Info>(&written), expected);
        let short = std::mem::offset_of!(Info, branch);
        assert_eq!(request(&store, &info, short + RESPONSE).1, errno(Errno::EINVAL));
    }

    /// The pieces a MAP_MANY carries are mapped each with its own flags, zeros only in a window
    /// that copies on write, and those MAP_AHEAD says mapped ahead; flags of neither, refused.
    #[test]
    fn maps_carry_their_pieces() {
        let store = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        let map = |sequence: u64, offset: u64, len: u64, window_offset: u64| MapArgs {
            id: ExtentIdLe { branch: BRANCH.to_le(), sequence: sequence.to_le() },
            offset: offset.to_le(),
            len: len.to_le(),
            window_offset: window_offset.to_le(),
        };
        let entry = |map: MapArgs, flags: u32| MapEntry { map, flags: flags.to_le(), reserved: 0 };
        let pieces: Vec<u8> = [
            entry(map(2, 4096, 4096, 8192), 0),
            entry(map(0, 0, 4096, 16384), VIRTIO_EXTENT_MAP_ZEROS | VIRTIO_EXTENT_MAP_AHEAD),
        ]
        .iter()
        .flat_map(|entry| entry.as_bytes().to_vec())
        .collect();
        let flags = VIRTIO_EXTENT_MAP_MANY | VIRTIO_EXTENT_MAP_AHEAD;
        let request_ = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(map(1, 0, 4096, 0)), &pieces);
        assert_eq!(request(&store, &request_, RESPONSE), (vec![], 0));
        let mut maps = store.state().maps.clone();
        maps.sort_by_key(|(_, _, window_offset)| *window_offset);
        assert_eq!(
            maps,
            vec![
                (Some((id(1), 0)), 4096, 0),
                (Some((id(2), 4096)), 4096, 8192),
                (None, 4096, 16384)
            ]
        );
        let mut prefaulted = store.state().prefaulted.clone();
        prefaulted.sort_unstable();
        assert_eq!(prefaulted, vec![(0, 4096), (16384, 4096)]);

        // Without a window that copies on write, zeros are refused; and a flag unknown, writable
        // DAX's MAP_WRITE among them (backlog doc-13).
        let follower = FakeBackend::default();
        let zeros = bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_ZEROS, Some(map(0, 0, 1, 0)), &[]);
        assert_eq!(request(&follower, &zeros, RESPONSE).1, errno(Errno::EINVAL));
        for unknown in [1 << 0, 1 << 4] {
            let map = bytes(VIRTIO_EXTENT_T_MAP, unknown, Some(map(1, 0, 1, 0)), &[]);
            assert_eq!(request(&store, &map, RESPONSE).1, errno(Errno::EINVAL));
        }
        let piece = entry(map(1, 0, 1, 0), 1 << 1);
        let many = bytes(
            VIRTIO_EXTENT_T_MAP,
            VIRTIO_EXTENT_MAP_MANY,
            Some(map(1, 0, 1, 0)),
            piece.as_bytes(),
        );
        assert_eq!(request(&store, &many, RESPONSE).1, errno(Errno::EINVAL));
    }

    /// cladefs: a FREE_MANY's or a MAP_MANY's data past MAX_MANY_DATA is refused, nothing done: the
    /// device lists what it names in its own memory, and a chain naming the same guest memory
    /// again and again names as much as it likes. Up to it, the request is served.
    #[test]
    fn many_past_their_most_are_refused() {
        let store = FakeBackend::default();
        let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(1, 0, 1)), b"x");
        assert_eq!(request(&store, &write, RESPONSE), (vec![], 0));
        let extent = io(1, 0, 0).id;
        let free = |data_len: usize| {
            let data = extent.as_bytes().repeat(data_len / size_of::<ExtentIdLe>());
            bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(extent), &data)
        };
        let past = free(MAX_MANY_DATA + size_of::<ExtentIdLe>());
        assert_eq!(request(&store, &past, RESPONSE).1, errno(Errno::EINVAL));
        assert!(store.state().extents.contains_key(&id(1)));
        // Up to the most: the extent freed once, then not found as it is named again.
        assert_eq!(request(&store, &free(MAX_MANY_DATA), RESPONSE).1, errno(Errno::ENOENT));
        assert!(!store.state().extents.contains_key(&id(1)));

        let entry = MapEntry::default();
        let map = |data_len: usize| {
            let data = entry.as_bytes().repeat(data_len / size_of::<MapEntry>());
            bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(MapArgs::default()), &data)
        };
        let past = map(MAX_MANY_DATA + size_of::<MapEntry>());
        assert_eq!(request(&store, &past, RESPONSE).1, errno(Errno::EINVAL));
        assert!(store.state().maps.is_empty());
        assert_eq!(request(&store, &map(MAX_MANY_DATA), RESPONSE), (vec![], 0));
        assert_eq!(store.state().maps.len(), 1 + MAX_MANY_DATA / size_of::<MapEntry>());
    }

    /// FREEZE says whether the guest's filesystem is frozen; RESIZED answers the shrink asked,
    /// telling the store; STATS gives the store the guest's text, refusing what isn't text or is
    /// longer than the daemon takes; UNMAP and PRIVATE go to the store, PRIVATE of a window that
    /// copies on write alone.
    #[test]
    fn notices_and_window_requests_reach_their_ends() {
        let store = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        let notices = Notices::default();
        let serve = |request: &[u8]| {
            request_as(&store, &notices, CacheType::Writeback, request, RESPONSE, usize::MAX)
        };
        let frozen = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
        assert_eq!(serve(&frozen), (vec![], 0));
        assert!(notices.frozen());
        assert_eq!(serve(&bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, 0, None, &[])), (vec![], 0));
        assert!(!notices.frozen());

        notices.resizing.changed(1 << 20, false);
        notices.resizing.changed(1 << 19, true);
        let took = ResizedArgs { capacity: (1u64 << 19).to_le() };
        let resized = bytes(VIRTIO_EXTENT_T_RESIZED, VIRTIO_EXTENT_RESIZED_TOOK, Some(took), &[]);
        assert_eq!(serve(&resized), (vec![], 0));
        assert_eq!((notices.resizing.capacity(), notices.resizing.asked()), (1 << 19, 0));
        assert_eq!(store.state().resized, vec![(1 << 19, true)]);

        let asked = Some(StatsArgs { asked: 3u64.to_le() });
        assert_eq!(serve(&bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals 1\n")), (vec![], 0));
        let not_text = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals \xff\n");
        assert_eq!(serve(&not_text).1, errno(Errno::EINVAL));
        let long = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, &vec![b'\n'; MAX_STATS + 1]);
        assert_eq!(serve(&long).1, errno(Errno::EINVAL));
        assert_eq!(store.state().stats, vec![(3, "seals 1\n".to_owned())]);

        let range = UnmapArgs { window_offset: 4096u64.to_le(), len: 8192u64.to_le() };
        assert_eq!(serve(&bytes(VIRTIO_EXTENT_T_UNMAP, 0, Some(range), &[])), (vec![], 0));
        assert_eq!(serve(&bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(range), &[])), (vec![], 0));
        assert_eq!(
            serve(&bytes(VIRTIO_EXTENT_T_PRIVATE, 1, Some(range), &[])).1,
            errno(Errno::EINVAL)
        );
        assert_eq!(store.state().unmapped, vec![(4096, 8192)]);
        assert_eq!(store.state().private, vec![(4096, 8192)]);
        let follower = FakeBackend::default();
        let private = bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(range), &[]);
        assert_eq!(request(&follower, &private, RESPONSE).1, errno(Errno::EINVAL));
    }

    /// A request with no room for its response is returned with nothing written; one of an
    /// unknown type, or whose header is short, is refused.
    #[test]
    fn bad_requests_are_refused() {
        let store = FakeBackend::default();
        let mut header = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
        let mut buffers = Buffers::new();
        // SAFETY: The test's memory, which outlives the request.
        unsafe { buffers.push_readable(header.as_mut_ptr(), header.len()) };
        let notices = Notices::default();
        let used =
            futures::executor::block_on(handle(&store, &notices, CacheType::Writeback, buffers));
        assert_eq!(used, 0);
        assert_eq!(
            request(&store, &bytes::<Info>(99, 0, None, &[]), RESPONSE).1,
            errno(Errno::EINVAL)
        );
        assert_eq!(request(&store, &[0; 4], RESPONSE).1, errno(Errno::EINVAL));
    }

    /// cladefs TASK-77: a guest's driver is untrusted: whatever a request says, of the types and
    /// flags it may name, of extents of its branch or another's, at offsets near the store's ends
    /// or at the ends of a u64, the device answers it, with a status, no more than the room it was
    /// given written. Seeded by proptest, which says the case if one fails.
    #[test]
    fn whatever_a_driver_asks_is_answered() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestRunner};

        let store = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        let near_or_far = || {
            prop_oneof![
                0u64..(1 << 22),
                any::<u64>(),
                (0u64..4096).prop_map(|back| u64::MAX - back)
            ]
        };
        let branch = prop_oneof![Just(BRANCH), any::<u64>()];
        let args = (branch, 0u64..64, near_or_far(), near_or_far(), any::<u32>());
        let word = || prop_oneof![0u32..18, any::<u32>()];
        let data = proptest::collection::vec(any::<u8>(), 0..8192);
        let requests = (word(), word(), args, data, 0usize..16384, 1usize..4096);
        let config = Config { cases: 2000, failure_persistence: None, ..Config::default() };
        TestRunner::new(config)
            .run(&requests, |(type_, flags, args, data, room, split)| {
                let (branch, sequence, first, second, last) = args;
                let mut readable = bytes::<Info>(type_, flags, None, &[]);
                for word in [branch, sequence, first, second, u64::from(last)] {
                    readable.extend_from_slice(&word.to_le_bytes());
                }
                readable.extend_from_slice(&data);
                let notices = Notices::default();
                let cache = CacheType::Writeback;
                let (written, status) =
                    request_as(&store, &notices, cache, &readable, RESPONSE + room, split);
                prop_assert_eq!(written.len(), room);
                prop_assert!(status < 4096, "status {}", status);
                Ok(())
            })
            .unwrap();
    }
}
