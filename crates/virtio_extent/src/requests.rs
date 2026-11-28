// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Serving the device's requests: a guest's, as doc-4 has them (`cladefs::extent_requests::Guest`),
//! with the interpreter every transport of doc-4's requests shares ([`handle`], board 14378). The device applies requests
//! in any order but across barriers (doc-4), so a guest's `MAP` doesn't wait for the writes and
//! uploaded commits queued before it: each is served as a future ([`handle`]), several at once. On
//! Linux, [`Requests`] serves them on the threads receiving the daemon's replies, which go on with
//! them, completing each request (`cladefs::cladefsd::spawn_on_replies`): no thread is woken
//! between the reply and the guest's interrupt, nor waits for the cell's I/O (TASK-41).

use std::sync::atomic::{AtomicU64, Ordering};

use cladefs::extent_requests::protocol::*;
use cladefs::extent_requests::{Guest, handle};
use log::info;

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

pub use self::serving::Requests;

mod serving {
    use std::fmt::Display;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use cladefs::CellClient;
    use log::error;

    use cladefs::extent_requests::Buffers;
    use cladefs::extent_requests::protocol::Bounds;

    use super::{Guest, Served, handle, type_of};
    use crate::config::CacheType;
    use crate::notices::Notices;

    /// The requests of one of the device's queues, which its VMM's worker takes off the queue:
    /// served on the thread taking them until they wait for the daemon, then on the threads
    /// receiving the daemon's replies, which complete them. Counts those in flight, which the
    /// worker waits for as it stops ([`Requests::wait_idle`]), and what it served of each type.
    pub struct Requests {
        cell: Arc<CellClient>,
        notices: Arc<Notices>,
        /// cladefs: what it does with the guest's asks for uploaded commits (TASK-363).
        cache: CacheType,
        /// cladefs: the device's, past which a request is refused (TASK-470).
        bounds: Bounds,
        served: Served,
        /// The requests taken off the queue and not yet returned to the driver.
        serving: Mutex<usize>,
        /// Notified as the last of them is returned.
        idle: Condvar,
    }

    impl std::fmt::Debug for Requests {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Requests")
                .field("cell", &self.cell.cell())
                .field("cache", &self.cache)
                .field("bounds", &self.bounds)
                .field("serving", &self.serving)
                .finish_non_exhaustive()
        }
    }

    impl Requests {
        /// Requests served with `cell`, as `notices`, `cache` and `bounds` say: those of a device
        /// ([`Device::requests`](crate::Device::requests)).
        pub fn new(
            cell: Arc<CellClient>,
            notices: Arc<Notices>,
            cache: CacheType,
            bounds: Bounds,
        ) -> Self {
            Self {
                cell,
                notices,
                cache,
                bounds,
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
            // No buffers: no header to name the request by; their failure is answered below.
            let header = buffers.as_ref().ok().and_then(Buffers::header);
            // cladefs: served with the guest's number of it current, which the daemon's client's
            // calls for it carry to the daemon's log (`cladefs::request`, TASK-260): 0, none, if
            // unread.
            let unique = header.map_or(0, |header| u64::from_le(header.unique));
            let requests = self.clone();
            let served = async move {
                let len = match buffers {
                    Ok(buffers) => {
                        let guest = Guest {
                            notices: &*requests.notices,
                            ordered_only: requests.cache.ordered_only(),
                        };
                        handle(&*requests.cell, &guest, requests.bounds, buffers).await.len
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
            cladefs::cladefsd::spawn_on_replies(cladefs::request::guest_request(unique, served));
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

    impl crate::workers::Serve for Requests {
        fn serve(
            self: Arc<Self>,
            buffers: Result<Buffers, crate::BoxError>,
            complete: crate::workers::Completion,
        ) {
            Requests::serve(&self, buffers, complete);
        }

        fn wait_idle(&self) {
            Requests::wait_idle(self);
        }

        fn log_served(&self) {
            self.served.log();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Requests served with a fake cell, in fake guest memory: what the device makes of each
    //! request, apart from any daemon (tests/daemon.rs serves them with one).

    use std::collections::BTreeMap;
    use std::sync::{Mutex, PoisonError};

    use cladefs::extent_requests::{Backend, Buffers, GuestBackend, Handled};
    use cladefs::{
        Commit, Errno, ExtentId, ExtentInfo, MappingContents, VolatileSlice, VolatileSliceMut,
        WindowMapping, WriteBarriers,
    };
    use zerocopy::{Immutable, IntoBytes};

    use super::*;
    use crate::config::CacheType;
    use crate::notices::Notices;

    const RESPONSE: usize = size_of::<Response>();
    /// The fake device's block size and map alignment: a 4 KiB page's, whatever the host's.
    const BLOCK: u64 = 4096;
    /// The fake device's window: 64 MiB.
    const WINDOW: u64 = 64 << 20;
    const BOUNDS: Bounds =
        Bounds { block_size: BLOCK, max_extent: MAX_EXTENT, map_alignment: BLOCK, window: WINDOW };

    /// A cell in memory, which records what it is asked: extents, sealed by a commit; a root; a
    /// window's maps; and the guest's stats and answers. Asked of a range, it answers as cladefs's
    /// does (local_cell's `check_io`, the window's `check_range`), and records any doc-4 calls
    /// out of range, which the device refuses before (TASK-470).
    #[derive(Default)]
    struct FakeBackend {
        copies_on_write: bool,
        state: Mutex<FakeState>,
        /// cladefs TASK-563: the extent whose MAPs fail at once (`EPERM`), as an open extent's do.
        refused: Option<u64>,
        /// cladefs TASK-563: an extent whose MAP waits for its gate to open, as one behind others
        /// in the daemon does, and the gate.
        held: Mutex<Option<(u64, futures::channel::oneshot::Receiver<()>)>>,
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
        /// cladefs TASK-470: the ranges it was asked of that doc-4 calls out of range, which no
        /// device passes on: what asked, its offset and its length.
        out_of_range: Vec<(&'static str, u64, u64)>,
        /// How many times it was asked of a range, in an extent or the window.
        ranges_asked: usize,
    }

    impl FakeState {
        /// `len` bytes at `offset` in an extent, as local_cell's `check_io` takes them: multiples
        /// of a block, some bytes, ending within the most an extent holds; `EINVAL` otherwise.
        fn extent_range(
            &mut self,
            asked: &'static str,
            offset: u64,
            len: u64,
        ) -> Result<(), Errno> {
            let end = u128::from(offset) + u128::from(len);
            let fits = offset % BLOCK == 0 && len % BLOCK == 0 && end <= u128::from(MAX_EXTENT);
            self.range(asked, offset, len, fits)?;
            if len == 0 { Err(Errno::EINVAL) } else { Ok(()) }
        }

        /// `len` bytes at `offset` of the window, as its `check_range` takes them: multiples of a
        /// page, some bytes, ending within the window; `EINVAL` otherwise.
        fn window_range(
            &mut self,
            asked: &'static str,
            offset: u64,
            len: u64,
        ) -> Result<(), Errno> {
            let end = u128::from(offset) + u128::from(len);
            let fits = offset % BLOCK == 0 && len % BLOCK == 0 && end <= u128::from(WINDOW);
            self.range(asked, offset, len, fits)?;
            if len == 0 { Err(Errno::EINVAL) } else { Ok(()) }
        }

        /// A range asked, which `fits` doc-4's bounds or is recorded and refused, `EINVAL`.
        fn range(
            &mut self,
            asked: &'static str,
            offset: u64,
            len: u64,
            fits: bool,
        ) -> Result<(), Errno> {
            self.ranges_asked += 1;
            if fits {
                return Ok(());
            }
            self.out_of_range.push((asked, offset, len));
            Err(Errno::EINVAL)
        }
    }

    impl FakeBackend {
        /// Its state, whatever case panicked holding it: a failed case reports its own failure,
        /// not every later one a poisoned lock (TASK-470).
        fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    const CELL: u64 = 7;

    impl Backend for FakeBackend {
        fn cell(&self) -> u64 {
            CELL
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
            let mut state = self.state();
            let len: usize = buffers.iter().map(VolatileSliceMut::len).sum();
            state.extent_range("READ", offset, len as u64)?;
            let (data, _) = state.extents.get(&extent).ok_or(Errno::ENOENT)?;
            // Past the extent's end, zeros, as cladefs's reads.
            let mut bytes = vec![0; len];
            let held = data.get(offset as usize..).unwrap_or_default();
            let held = &held[..held.len().min(len)];
            bytes[..held.len()].copy_from_slice(held);
            let mut at = 0;
            for buffer in buffers {
                at += buffer.copy_from(&bytes[at..]);
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
            let len: usize = buffers.iter().map(VolatileSlice::len).sum();
            state.extent_range("WRITE", offset, len as u64)?;
            if extent.cell != CELL {
                return Err(Errno::EPERM);
            }
            let (data, sealed) = state.extents.entry(extent).or_default();
            if *sealed {
                return Err(Errno::EPERM);
            }
            // Within MAX_EXTENT, as checked.
            let mut at = offset as usize;
            data.resize(data.len().max(at + len), 0);
            for buffer in buffers {
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

        /// As cladefs's: freeing an extent it doesn't hold succeeds (doc-4's `FREE`).
        async fn free_many(&self, extents: &[ExtentId]) -> Result<(), Errno> {
            let mut state = self.state();
            for extent in extents {
                state.extents.remove(extent);
            }
            Ok(())
        }

        /// As `OpenCell::map_many`: maps each entry in order, stopping at the first error,
        /// leaving entries before it mapped.
        async fn map_many(&self, mappings: &[WindowMapping], warm: bool) -> Result<(), Errno> {
            assert!(!warm, "a guest's MAP warms nothing: no flag of doc-4's asks it");
            let held = self.held.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((sequence, gate)) = held {
                let matches = mappings.iter().any(|m| {
                    matches!(m.contents, MappingContents::Extent { extent, .. } if extent.sequence == sequence)
                });
                if matches {
                    gate.await.unwrap();
                } else {
                    *self.held.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some((sequence, gate));
                }
            }
            let mut state = self.state();
            for mapping in mappings {
                let WindowMapping { window_offset, len, contents } = *mapping;
                match contents {
                    MappingContents::Extent { extent, offset } => {
                        if self.refused == Some(extent.sequence) {
                            return Err(Errno::EPERM);
                        }
                        state.window_range("MAP", window_offset, len)?;
                        let fits = offset % BLOCK == 0 && offset.checked_add(len).is_some();
                        state.range("MAP's extent", offset, len, fits)?;
                        state.maps.push((Some((extent, offset)), len, window_offset));
                    }
                    MappingContents::Zeros => {
                        state.window_range("MAP_ZEROS", window_offset, len)?;
                        state.maps.push((None, len, window_offset));
                    }
                }
            }
            Ok(())
        }

        async fn unmap(&self, window_offset: u64, len: u64) -> Result<(), Errno> {
            let mut state = self.state();
            state.window_range("UNMAP", window_offset, len)?;
            state.unmapped.push((window_offset, len));
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
    }

    impl GuestBackend for FakeBackend {
        fn prefault(&self, window_offset: u64, len: u64) {
            let mut state = self.state();
            // Of what a MAP mapped, whose range was checked: it answers nothing, but one out of
            // the window is recorded.
            if u128::from(window_offset) + u128::from(len) > u128::from(WINDOW) {
                state.out_of_range.push(("MAP_AHEAD", window_offset, len));
            }
            state.prefaulted.push((window_offset, len));
        }

        async fn make_private(&self, window_offset: u64, len: u64) -> Result<(), Errno> {
            let mut state = self.state();
            state.window_range("PRIVATE", window_offset, len)?;
            state.private.push((window_offset, len));
            Ok(())
        }

        async fn answer_stats(&self, ask: u64, guest: Option<&str>) -> Result<(), Errno> {
            self.state().stats.push((ask, guest.unwrap_or_default().to_owned()));
            Ok(())
        }

        async fn resized(&self, capacity: u64, took: bool) -> Result<(), Errno> {
            self.state().resized.push((capacity, took));
            Ok(())
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
        cell: &FakeBackend,
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
        let handled = futures::executor::block_on(handle(
            cell,
            &Guest { notices, ordered_only: cache.ordered_only() },
            BOUNDS,
            buffers,
        ));
        let used = handled.len as usize;
        assert_eq!(used, writable, "the device wrote {used} of {writable} bytes");
        let response: Response = read_obj(&written[writable - RESPONSE..]);
        let status = handled.status.err().map_or(0, |errno| errno.raw().cast_unsigned());
        assert_eq!(status, u32::from_le(response.status), "the status handled, the response's");
        written.truncate(writable - RESPONSE);
        (written, u32::from_le(response.status))
    }

    fn request(cell: &FakeBackend, readable: &[u8], writable: usize) -> (Vec<u8>, u32) {
        request_as(cell, &Notices::default(), CacheType::Writeback, readable, writable, usize::MAX)
    }

    fn errno(errno: Errno) -> u32 {
        errno.raw().cast_unsigned()
    }

    fn io(sequence: u64, offset: u64, len: usize) -> IoArgs {
        IoArgs {
            id: ExtentIdLe { cell: CELL.to_le(), sequence: sequence.to_le() },
            offset: offset.to_le(),
            len: (len as u32).to_le(),
            root_len: 0,
        }
    }

    fn id(sequence: u64) -> ExtentId {
        ExtentId { cell: CELL, sequence }
    }

    /// Writes and reads move the bytes the driver gave, across descriptors of any size; a commit
    /// seals what was written; a FREE frees.
    #[test]
    fn writes_and_reads_go_through_the_cell() {
        let cell = FakeBackend::default();
        let data: Vec<u8> = (0..3 * BLOCK).map(|i| i as u8).collect();
        let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(1, 0, data.len())), &data);
        let notices = Notices::default();
        let cache = CacheType::Writeback;
        assert_eq!(request_as(&cell, &notices, cache, &write, RESPONSE, 777), (vec![], 0));
        let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(1, 0, data.len())), &[]);
        let writable = data.len() + RESPONSE;
        assert_eq!(request(&cell, &read, writable), (data.clone(), 0));
        assert_eq!(request_as(&cell, &notices, cache, &read, writable, 1000), (data.clone(), 0));
        // A read with less room than it reads: refused, its room zeroed.
        assert_eq!(request(&cell, &read, 100 + RESPONSE), (vec![0; 100], errno(Errno::EINVAL)));
        let missing = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(2, 0, 4096)), &[]);
        let not_found = (vec![0; 4096], errno(Errno::ENOENT));
        assert_eq!(request(&cell, &missing, 4096 + RESPONSE), not_found);

        let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
        assert_eq!(request(&cell, &commit, RESPONSE), (vec![], 0));
        assert_eq!(request(&cell, &write, RESPONSE).1, errno(Errno::EPERM));
        let free = bytes(VIRTIO_EXTENT_T_FREE, 0, Some(io(1, 0, 0).id), &[]);
        assert_eq!(request(&cell, &free, RESPONSE), (vec![], 0));
        assert_eq!(request(&cell, &read, writable).1, errno(Errno::ENOENT));
    }

    /// A WRITE's flags order it: a barrier before; a commit after, uploaded or ordered, setting the
    /// root after the data; none but a WRITE's.
    #[test]
    fn writes_carry_their_barriers() {
        let cell = FakeBackend::default();
        let rooted = IoArgs { root_len: 6u32.to_le(), ..io(1, 0, 4096) };
        let flags = VIRTIO_EXTENT_WRITE_BARRIER
            | VIRTIO_EXTENT_WRITE_COMMIT
            | VIRTIO_EXTENT_WRITE_UPLOADED
            | VIRTIO_EXTENT_WRITE_ROOT;
        let data_and_root = [&[7; 4096][..], b"second"].concat();
        let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(rooted), &data_and_root);
        assert_eq!(request(&cell, &write, RESPONSE), (vec![], 0));
        assert_eq!(cell.state().commits, vec![Commit::Ordered, Commit::Uploaded]);
        assert_eq!(cell.state().root, b"second");
        assert_eq!(cell.state().extents[&id(1)].0, vec![7; 4096]);
        // A root, or an upload, without a commit; an unknown flag; a root past the most.
        for flags in [VIRTIO_EXTENT_WRITE_ROOT, VIRTIO_EXTENT_WRITE_UPLOADED, 1 << 4] {
            let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(rooted), &data_and_root);
            assert_eq!(request(&cell, &write, RESPONSE).1, errno(Errno::EINVAL), "{flags:#x}");
        }
        let long = IoArgs { root_len: (MAX_ROOT + 1).to_le(), ..io(2, 0, 0) };
        let root = vec![1; MAX_ROOT as usize + 1];
        let flags = VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_ROOT;
        let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(long), &root);
        assert_eq!(request(&cell, &write, RESPONSE).1, errno(Errno::EINVAL));
        // Data shorter than the write says.
        let short = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(3, 0, 4096)), &[1; 50]);
        assert_eq!(request(&cell, &short, RESPONSE).1, errno(Errno::EINVAL));
        let past = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(3, 0, MAX_TRANSFER as usize + 1)), &[]);
        assert_eq!(request(&cell, &past, RESPONSE).1, errno(Errno::EINVAL));
    }

    /// cladefs TASK-363: an Unsafe device's commits, and writes' commits, are ordered, whatever the
    /// driver asked.
    #[test]
    fn unsafe_devices_commit_ordered() {
        let cell = FakeBackend::default();
        let notices = Notices::default();
        let uploaded = bytes(
            VIRTIO_EXTENT_T_COMMIT,
            VIRTIO_EXTENT_COMMIT_UPLOADED,
            Some(CommitArgs::default()),
            &[],
        );
        let flags = VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_UPLOADED;
        let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(io(1, 0, 4096)), &[7; 4096]);
        for request in [&uploaded, &write] {
            let served = request_as(&cell, &notices, CacheType::Unsafe, request, RESPONSE, 64);
            assert_eq!(served, (vec![], 0));
        }
        let served =
            request_as(&cell, &notices, CacheType::Writeback, &uploaded, RESPONSE, usize::MAX);
        assert_eq!(served, (vec![], 0));
        assert_eq!(cell.state().commits, vec![Commit::Ordered, Commit::Ordered, Commit::Uploaded]);
    }

    /// ROOT writes the root, zeros to its most, and its length; COMMIT with COMMIT_ROOT sets it;
    /// INFO writes the whole of it, the cell ID too, refusing room for less (TASK-141.3).
    #[test]
    fn roots_and_info_are_written_whole() {
        let cell = FakeBackend::default();
        let commit = bytes(
            VIRTIO_EXTENT_T_COMMIT,
            VIRTIO_EXTENT_COMMIT_ROOT,
            Some(CommitArgs { root_len: 5u32.to_le(), reserved: 0 }),
            b"first",
        );
        assert_eq!(request(&cell, &commit, RESPONSE), (vec![], 0));
        let root = bytes::<RootLen>(VIRTIO_EXTENT_T_ROOT, 0, None, &[]);
        let root_len = MAX_ROOT as usize + size_of::<RootLen>();
        let (written, status) = request(&cell, &root, root_len + RESPONSE);
        assert_eq!(status, 0);
        assert_eq!(&written[..5], b"first");
        assert!(written[5..MAX_ROOT as usize].iter().all(|b| *b == 0));
        assert_eq!(read_obj::<RootLen>(&written[MAX_ROOT as usize..]).len, 5u32.to_le());
        assert_eq!(request(&cell, &root, root_len - 1 + RESPONSE).1, errno(Errno::EINVAL));

        let info = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
        let (written, status) = request(&cell, &info, size_of::<Info>() + RESPONSE);
        assert_eq!(status, 0);
        let expected = Info {
            capacity: (1u64 << 20).to_le(),
            used: 0,
            available: (1u64 << 20).to_le(),
            next_sequence: 3u64.to_le(),
            sequence_floor: (1u64 << 40).to_le(),
            cell: CELL.to_le(),
        };
        assert_eq!(read_obj::<Info>(&written), expected);
        let short = std::mem::offset_of!(Info, cell);
        assert_eq!(request(&cell, &info, short + RESPONSE).1, errno(Errno::EINVAL));
    }

    /// The pieces a MAP_MANY carries are mapped each with its own flags, zeros only in a window
    /// that copies on write, and those MAP_PREFAULT says mapped ahead; flags of neither, refused.
    #[test]
    fn maps_carry_their_pieces() {
        let cell = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        let map = |sequence: u64, offset: u64, len: u64, window_offset: u64| MapArgs {
            id: ExtentIdLe { cell: CELL.to_le(), sequence: sequence.to_le() },
            offset: offset.to_le(),
            len: len.to_le(),
            window_offset: window_offset.to_le(),
        };
        let entry = |map: MapArgs, flags: u32| MapEntry { map, flags: flags.to_le(), reserved: 0 };
        let pieces: Vec<u8> = [
            entry(map(2, 4096, 4096, 8192), 0),
            entry(map(0, 0, 4096, 16384), VIRTIO_EXTENT_MAP_ZEROS | VIRTIO_EXTENT_MAP_PREFAULT),
        ]
        .iter()
        .flat_map(|entry| entry.as_bytes().to_vec())
        .collect();
        let flags = VIRTIO_EXTENT_MAP_MANY | VIRTIO_EXTENT_MAP_PREFAULT;
        let request_ = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(map(1, 0, 4096, 0)), &pieces);
        assert_eq!(request(&cell, &request_, RESPONSE), (vec![], 0));
        let mut maps = cell.state().maps.clone();
        maps.sort_by_key(|(_, _, window_offset)| *window_offset);
        assert_eq!(
            maps,
            vec![
                (Some((id(1), 0)), 4096, 0),
                (Some((id(2), 4096)), 4096, 8192),
                (None, 4096, 16384)
            ]
        );
        let mut prefaulted = cell.state().prefaulted.clone();
        prefaulted.sort_unstable();
        assert_eq!(prefaulted, vec![(0, 4096), (16384, 4096)]);

        // Without a window that copies on write, zeros are refused; and a flag unknown, writable
        // DAX's MAP_WRITE among them (backlog doc-13).
        let unwindowed = FakeBackend::default();
        let zeros = bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_ZEROS, Some(map(0, 0, 1, 0)), &[]);
        assert_eq!(request(&unwindowed, &zeros, RESPONSE).1, errno(Errno::EINVAL));
        for unknown in [1 << 0, 1 << 4] {
            let map = bytes(VIRTIO_EXTENT_T_MAP, unknown, Some(map(1, 0, 1, 0)), &[]);
            assert_eq!(request(&cell, &map, RESPONSE).1, errno(Errno::EINVAL));
        }
        let piece = entry(map(1, 0, 1, 0), 1 << 1);
        let many = bytes(
            VIRTIO_EXTENT_T_MAP,
            VIRTIO_EXTENT_MAP_MANY,
            Some(map(1, 0, 1, 0)),
            piece.as_bytes(),
        );
        assert_eq!(request(&cell, &many, RESPONSE).1, errno(Errno::EINVAL));
    }

    /// cladefs TASK-563, board 6917: a MAP_MANY is forwarded as one `map_many` RPC, completing
    /// only once the daemon answers, mapping entries in order up to the first failure and none
    /// after it, and prefaulting nothing on failure.
    #[test]
    fn failed_map_manys_complete_once_every_piece_is_done() {
        use futures::FutureExt;
        let (open, gate) = futures::channel::oneshot::channel();
        let cell = FakeBackend {
            copies_on_write: true,
            refused: Some(2),
            held: Mutex::new(Some((2, gate))),
            ..FakeBackend::default()
        };
        let map = |sequence: u64, window_offset: u64| MapArgs {
            id: ExtentIdLe { cell: CELL.to_le(), sequence: sequence.to_le() },
            offset: 0,
            len: BLOCK.to_le(),
            window_offset: window_offset.to_le(),
        };
        let pieces: Vec<u8> = [
            MapEntry {
                map: map(2, 2 * BLOCK),
                flags: VIRTIO_EXTENT_MAP_PREFAULT.to_le(),
                reserved: 0,
            },
            MapEntry { map: map(3, 4 * BLOCK), flags: 0, reserved: 0 },
        ]
        .iter()
        .flat_map(|entry| entry.as_bytes().to_vec())
        .collect();
        let flags = VIRTIO_EXTENT_MAP_MANY | VIRTIO_EXTENT_MAP_PREFAULT;
        let mut readable = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(map(1, 0)), &pieces);
        let mut written = vec![0xa5; RESPONSE];
        let mut buffers = Buffers::new();
        // SAFETY: The test's memory, which outlives the request, and nothing else accesses
        // meanwhile.
        unsafe { buffers.push_readable(readable.as_mut_ptr(), readable.len()) };
        // SAFETY: As above.
        unsafe { buffers.push_writable(written.as_mut_ptr(), written.len()) }.unwrap();
        let notices = Notices::default();
        let guest = Guest { notices: &notices, ordered_only: false };
        let mut handling = Box::pin(handle(&cell, &guest, BOUNDS, buffers));
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        let polled = handling.poll_unpin(&mut context);
        assert!(polled.is_pending(), "completed with map_many still in flight: {polled:?}");
        assert_eq!(cell.state().maps, vec![]);
        open.send(()).unwrap();
        let polled = handling.poll_unpin(&mut context);
        assert_eq!(polled.map(|handled| handled.len), std::task::Poll::Ready(RESPONSE as u32));
        drop(handling);
        let response: Response = read_obj(&written);
        assert_eq!(u32::from_le(response.status), errno(Errno::EPERM));
        // Piece 1 mapped before piece 2 failed; piece 3 was not mapped; nothing was prefaulted.
        assert_eq!(cell.state().maps, vec![(Some((id(1), 0)), BLOCK, 0)]);
        assert_eq!(cell.state().prefaulted, vec![]);
    }

    /// cladefs: a FREE_MANY's or a MAP_MANY's data past MAX_MANY_DATA is refused, nothing done: the
    /// device lists what it names in its own memory, and a chain naming the same guest memory
    /// again and again names as much as it likes. Up to it, the request is served.
    #[test]
    fn many_past_their_most_are_refused() {
        let cell = FakeBackend::default();
        let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(1, 0, 4096)), &[7; 4096]);
        assert_eq!(request(&cell, &write, RESPONSE), (vec![], 0));
        let extent = io(1, 0, 0).id;
        let free = |data_len: usize| {
            let data = extent.as_bytes().repeat(data_len / size_of::<ExtentIdLe>());
            bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(extent), &data)
        };
        let past = free(MAX_MANY_DATA + size_of::<ExtentIdLe>());
        assert_eq!(request(&cell, &past, RESPONSE).1, errno(Errno::EINVAL));
        assert!(cell.state().extents.contains_key(&id(1)));
        // Up to the most: the extent freed, as often as it is named, as cladefs's cell frees
        // (doc-4: freeing an extent the cell doesn't hold succeeds; the fake said ENOENT before
        // TASK-470).
        assert_eq!(request(&cell, &free(MAX_MANY_DATA), RESPONSE), (vec![], 0));
        assert!(!cell.state().extents.contains_key(&id(1)));

        let page = MapArgs { len: BLOCK.to_le(), ..MapArgs::default() };
        let entry = MapEntry { map: page, ..MapEntry::default() };
        let map = |data_len: usize| {
            let data = entry.as_bytes().repeat(data_len / size_of::<MapEntry>());
            bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(page), &data)
        };
        let past = map(MAX_MANY_DATA + size_of::<MapEntry>());
        assert_eq!(request(&cell, &past, RESPONSE).1, errno(Errno::EINVAL));
        assert!(cell.state().maps.is_empty());
        assert_eq!(request(&cell, &map(MAX_MANY_DATA), RESPONSE), (vec![], 0));
        assert_eq!(cell.state().maps.len(), 1 + MAX_MANY_DATA / size_of::<MapEntry>());
    }

    /// FREEZE says whether the guest's filesystem is frozen; RESIZED answers the shrink asked,
    /// telling the cell; STATS gives the cell the guest's text, refusing what isn't text or is
    /// longer than the daemon takes; UNMAP and PRIVATE go to the cell, PRIVATE of a window that
    /// copies on write alone.
    #[test]
    fn notices_and_window_requests_reach_their_ends() {
        let cell = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        let notices = Notices::default();
        let serve = |request: &[u8]| {
            request_as(&cell, &notices, CacheType::Writeback, request, RESPONSE, usize::MAX)
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
        assert_eq!(cell.state().resized, vec![(1 << 19, true)]);

        let asked = Some(StatsArgs { asked: 3u64.to_le() });
        assert_eq!(serve(&bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals 1\n")), (vec![], 0));
        let not_text = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals \xff\n");
        assert_eq!(serve(&not_text).1, errno(Errno::EINVAL));
        let long = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, &vec![b'\n'; MAX_STATS + 1]);
        assert_eq!(serve(&long).1, errno(Errno::EINVAL));
        assert_eq!(cell.state().stats, vec![(3, "seals 1\n".to_owned())]);

        let range = UnmapArgs { window_offset: 4096u64.to_le(), len: 8192u64.to_le() };
        assert_eq!(serve(&bytes(VIRTIO_EXTENT_T_UNMAP, 0, Some(range), &[])), (vec![], 0));
        assert_eq!(serve(&bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(range), &[])), (vec![], 0));
        assert_eq!(
            serve(&bytes(VIRTIO_EXTENT_T_PRIVATE, 1, Some(range), &[])).1,
            errno(Errno::EINVAL)
        );
        assert_eq!(cell.state().unmapped, vec![(4096, 8192)]);
        assert_eq!(cell.state().private, vec![(4096, 8192)]);
        let unwindowed = FakeBackend::default();
        let private = bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(range), &[]);
        assert_eq!(request(&unwindowed, &private, RESPONSE).1, errno(Errno::EINVAL));
    }

    /// A request with no room for its response is returned with nothing written; one of an
    /// unknown type, or whose header is short, is refused.
    #[test]
    fn bad_requests_are_refused() {
        let cell = FakeBackend::default();
        let mut header = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
        let mut buffers = Buffers::new();
        // SAFETY: The test's memory, which outlives the request.
        unsafe { buffers.push_readable(header.as_mut_ptr(), header.len()) };
        let notices = Notices::default();
        let guest = Guest { notices: &notices, ordered_only: false };
        let served = handle(&cell, &guest, BOUNDS, buffers);
        let handled = futures::executor::block_on(served);
        assert_eq!(handled, Handled { len: 0, status: Err(Errno::EINVAL) });
        assert_eq!(
            request(&cell, &bytes::<Info>(99, 0, None, &[]), RESPONSE).1,
            errno(Errno::EINVAL)
        );
        assert_eq!(request(&cell, &[0; 4], RESPONSE).1, errno(Errno::EINVAL));
    }

    /// cladefs TASK-470: a request whose range doc-4 calls out of range is refused, `EINVAL`, its
    /// cell never asked: an extent's past `max_extent`, or past the end of a u64, aligned so that
    /// its end wraps to 0; a window's past the window, or wrapping. Found by the Linux run of
    /// `whatever_a_driver_asks_is_answered`, whose WRITE near u64::MAX overflowed the fake cell.
    #[test]
    fn ranges_past_their_ends_are_refused() {
        let cell = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        // Aligned, its end wrapping to 0: what the Linux run's WRITE was, nearly.
        let wrapping = u64::MAX - (BLOCK - 1);
        let blocks = vec![7; 2 * BLOCK as usize];
        let extents = [
            (wrapping, BLOCK),
            (u64::MAX, BLOCK),
            (MAX_EXTENT, BLOCK),
            (MAX_EXTENT - BLOCK, 2 * BLOCK),
            (BLOCK + 1, BLOCK),
            (0, BLOCK + 1),
        ];
        for (offset, len) in extents {
            let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(1, offset, len as usize)), &blocks);
            let status = request(&cell, &write, RESPONSE).1;
            assert_eq!(status, errno(Errno::EINVAL), "{offset:#x} {len:#x}");
            let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(1, offset, len as usize)), &[]);
            let refused = (vec![0; len as usize], errno(Errno::EINVAL));
            let writable = len as usize + RESPONSE;
            assert_eq!(request(&cell, &read, writable), refused, "{offset:#x} {len:#x}");
        }
        let map = |offset: u64, len: u64, window_offset: u64| MapArgs {
            id: ExtentIdLe { cell: CELL.to_le(), sequence: 1u64.to_le() },
            offset: offset.to_le(),
            len: len.to_le(),
            window_offset: window_offset.to_le(),
        };
        // An extent's range wrapping.
        let request_ = bytes(VIRTIO_EXTENT_T_MAP, 0, Some(map(wrapping, BLOCK, 0)), &[]);
        assert_eq!(request(&cell, &request_, RESPONSE).1, errno(Errno::EINVAL));
        let windows =
            [(wrapping, BLOCK), (WINDOW, BLOCK), (WINDOW - BLOCK, 2 * BLOCK), (1, BLOCK), (0, 1)];
        for (window_offset, len) in windows {
            for flags in [0, VIRTIO_EXTENT_MAP_ZEROS] {
                let request_ =
                    bytes(VIRTIO_EXTENT_T_MAP, flags, Some(map(0, len, window_offset)), &[]);
                let status = request(&cell, &request_, RESPONSE).1;
                assert_eq!(status, errno(Errno::EINVAL), "{flags} {window_offset:#x} {len:#x}");
            }
            let range = UnmapArgs { window_offset: window_offset.to_le(), len: len.to_le() };
            for type_ in [VIRTIO_EXTENT_T_UNMAP, VIRTIO_EXTENT_T_PRIVATE] {
                let status = request(&cell, &bytes(type_, 0, Some(range), &[]), RESPONSE).1;
                assert_eq!(status, errno(Errno::EINVAL), "{type_} {window_offset:#x} {len:#x}");
            }
        }
        // One of a MAP_MANY's maps out of the window: none of them mapped.
        let out = MapEntry { map: map(0, BLOCK, WINDOW), ..MapEntry::default() };
        let many = bytes(
            VIRTIO_EXTENT_T_MAP,
            VIRTIO_EXTENT_MAP_MANY,
            Some(map(0, BLOCK, 0)),
            out.as_bytes(),
        );
        assert_eq!(request(&cell, &many, RESPONSE).1, errno(Errno::EINVAL));
        let state = cell.state();
        assert_eq!(state.ranges_asked, 0, "{:?}", state.out_of_range);
        assert!(state.extents.is_empty());
        // At the bounds, served.
        drop(state);
        let last = MAX_EXTENT - BLOCK;
        let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(1, last, BLOCK as usize)), &blocks);
        assert_eq!(request(&cell, &write, RESPONSE), (vec![], 0));
        let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(1, last, BLOCK as usize)), &[]);
        assert_eq!(request(&cell, &read, BLOCK as usize + RESPONSE), (vec![7; BLOCK as usize], 0));
        let range = UnmapArgs { window_offset: (WINDOW - BLOCK).to_le(), len: BLOCK.to_le() };
        assert_eq!(
            request(&cell, &bytes(VIRTIO_EXTENT_T_UNMAP, 0, Some(range), &[]), RESPONSE),
            (vec![], 0)
        );
        let out_of_range = cell.state().out_of_range.clone();
        assert!(out_of_range.is_empty(), "the cell was asked {out_of_range:?}");
    }

    /// cladefs board 6917: `sequence == u64::MAX` has no successor and is refused (`EINVAL`) at
    /// the VMM boundary across every extent-taking request (including further entries of
    /// `FREE_MANY` and `MAP_MANY`), while `MAP_ZEROS` ignores the extent ID.
    #[test]
    fn last_sequence_is_refused() {
        let cell = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        let bad = ExtentIdLe { cell: CELL.to_le(), sequence: u64::MAX.to_le() };
        let ok = ExtentIdLe { cell: CELL.to_le(), sequence: 1u64.to_le() };
        let bad_io = IoArgs { id: bad, offset: 0, len: (BLOCK as u32).to_le(), root_len: 0 };
        let bad_map = MapArgs { id: bad, offset: 0, len: BLOCK.to_le(), window_offset: 0 };
        let ok_map = MapArgs { id: ok, offset: 0, len: BLOCK.to_le(), window_offset: 0 };

        let cases = [
            ("read", bytes(VIRTIO_EXTENT_T_READ, 0, Some(bad_io), &[]), BLOCK as usize),
            ("write", bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(bad_io), &[7; BLOCK as usize]), 0),
            ("free", bytes(VIRTIO_EXTENT_T_FREE, 0, Some(bad), &[]), 0),
            (
                "free_many first",
                bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(bad), ok.as_bytes()),
                0,
            ),
            (
                "free_many rest",
                bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(ok), bad.as_bytes()),
                0,
            ),
            ("map", bytes(VIRTIO_EXTENT_T_MAP, 0, Some(bad_map), &[]), 0),
            (
                "map_many first",
                bytes(
                    VIRTIO_EXTENT_T_MAP,
                    VIRTIO_EXTENT_MAP_MANY,
                    Some(bad_map),
                    MapEntry { map: ok_map, ..MapEntry::default() }.as_bytes(),
                ),
                0,
            ),
            (
                "map_many rest",
                bytes(
                    VIRTIO_EXTENT_T_MAP,
                    VIRTIO_EXTENT_MAP_MANY,
                    Some(ok_map),
                    MapEntry { map: bad_map, ..MapEntry::default() }.as_bytes(),
                ),
                0,
            ),
        ];
        for (name, req, room) in cases {
            assert_eq!(request(&cell, &req, room + RESPONSE).1, errno(Errno::EINVAL), "{name}");
        }
        assert_eq!(cell.state().ranges_asked, 0);
        // Zeros ignore `id`, so `sequence == u64::MAX` is still served.
        let zeros = bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_ZEROS, Some(bad_map), &[]);
        assert_eq!(request(&cell, &zeros, RESPONSE), (vec![], 0));
    }

    /// cladefs TASK-77: a guest's driver is untrusted: whatever a request says, of the types and
    /// flags it may name, of extents of its cell or another's, at offsets near the cell's ends
    /// or at the ends of a u64, the device answers it, with a status, no more than the room it was
    /// given written. Seeded by proptest, which says the case if one fails.
    #[test]
    fn whatever_a_driver_asks_is_answered() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestRunner};

        let cell = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        // cladefs TASK-470: and blocks' multiples, which reads, writes and maps take.
        let near_or_far = || {
            prop_oneof![
                0u64..(1 << 22),
                (0u64..16).prop_map(|blocks| blocks * BLOCK),
                any::<u64>(),
                (0u64..4096).prop_map(|back| u64::MAX - back)
            ]
        };
        let cell_id = prop_oneof![Just(CELL), any::<u64>()];
        let args = (cell_id, 0u64..64, near_or_far(), near_or_far(), any::<u32>());
        let word = || prop_oneof![0u32..18, any::<u32>()];
        let data = proptest::collection::vec(any::<u8>(), 0..8192);
        let requests = (word(), word(), args, data, 0usize..16384, 1usize..4096);
        // As many cases as PROPTEST_CASES says, 2000 at least.
        let cases = Config::default().cases.max(2000);
        let config = Config { cases, failure_persistence: None, ..Config::default() };
        TestRunner::new(config)
            .run(&requests, |(type_, flags, args, data, room, split)| {
                let (cell_id, sequence, first, second, last) = args;
                let mut readable = bytes::<Info>(type_, flags, None, &[]);
                for word in [cell_id, sequence, first, second, u64::from(last)] {
                    readable.extend_from_slice(&word.to_le_bytes());
                }
                readable.extend_from_slice(&data);
                let notices = Notices::default();
                let cache = CacheType::Writeback;
                let (written, status) =
                    request_as(&cell, &notices, cache, &readable, RESPONSE + room, split);
                prop_assert_eq!(written.len(), room);
                prop_assert!(status < 4096, "status {}", status);
                // cladefs board 2774: a flag its type doesn't know, or a type none is of, refused.
                if known_flags(type_).is_none_or(|known| flags & !known != 0) {
                    prop_assert_eq!(
                        status,
                        errno(Errno::EINVAL),
                        "type {} flags {:#x}",
                        type_,
                        flags
                    );
                }
                // cladefs TASK-470: nothing out of range reached the cell.
                let out_of_range = cell.state().out_of_range.clone();
                prop_assert!(out_of_range.is_empty(), "the cell was asked {:?}", out_of_range);
                Ok(())
            })
            .unwrap();
    }

    /// cladefs board 2774: a request of any type setting a flag its type doesn't know is refused,
    /// `EINVAL`, its cell asked of no range, as is one of a type no request is of. Each of the 32
    /// bits, beside each type's own.
    #[test]
    fn unknown_flags_are_refused_whatever_the_type() {
        let cell = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        for type_ in 0..=VIRTIO_EXTENT_T_LAST + 1 {
            let known = known_flags(type_);
            for bit in 0..32 {
                let flag = 1u32 << bit;
                if known.is_some_and(|known| known & flag != 0) {
                    continue;
                }
                let asked = cell.state().ranges_asked;
                let asking = bytes(type_, flag, Some(io(1, 0, 4096)), &[0; 4096]);
                let (_, status) = request(&cell, &asking, RESPONSE + 4096);
                assert_eq!(status, errno(Errno::EINVAL), "type {type_} flag {flag:#x}");
                assert_eq!(cell.state().ranges_asked, asked, "type {type_} flag {flag:#x} asked");
            }
        }
    }

    /// cladefs board 2768: a request with more room than any request returns into is refused,
    /// `EINVAL` in its response, and nothing else of its buffers written: not zeroed, as an error's
    /// room was, for as much as a chain's length names.
    #[test]
    fn requests_with_more_room_than_any_returns_are_refused_unzeroed() {
        let cell = FakeBackend::default();
        let reading = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(1, 0, 4096)), &[]);
        let (written, status) = request(&cell, &reading, MAX_WRITABLE + 1);
        assert_eq!(status, errno(Errno::EINVAL));
        assert!(written.iter().all(|&byte| byte == 0xa5), "the room was written");
        let (_, status) = request(&cell, &reading, MAX_WRITABLE);
        assert_ne!(status, errno(Errno::EINVAL), "the largest room a request may have, refused");
    }

    /// cladefs TASK-470: the class of the Linux run's failure. Of every request with a range
    /// (READ, WRITE, MAP of an extent or of zeros, a MAP_MANY's further map, UNMAP, PRIVATE), at
    /// offsets and lengths at, below and past each of doc-4's bounds, unaligned, near and at the
    /// end of a u64, aligned so that their ends wrap to 0, the device refuses those doc-4 calls
    /// out of range, `EINVAL`, its cell unasked, and asks its cell of every other.
    #[test]
    fn ranges_are_refused_exactly_past_their_bounds() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestRunner};

        /// What a request came to: its cell asked, or refused unasked with a status.
        #[derive(Debug, PartialEq, Eq)]
        enum Came {
            Asked,
            Refused(u32),
        }

        /// Whether `offset` and `len` are multiples of `alignment`, their end `most` at most:
        /// doc-4's rule, in arithmetic that does not wrap.
        fn fits(offset: u64, len: u64, alignment: u64, most: u64) -> bool {
            let end = u128::from(offset) + u128::from(len);
            offset % alignment == 0 && len % alignment == 0 && end <= u128::from(most)
        }

        let cell = FakeBackend { copies_on_write: true, ..FakeBackend::default() };
        // At, below and past `most`; small; unaligned; anywhere; aligned near u64::MAX.
        let edge = |most: u64| {
            prop_oneof![
                (0u64..4).prop_map(move |back| most - back * BLOCK),
                (1u64..3).prop_map(move |past| most + past * BLOCK),
                (0u64..8).prop_map(|blocks| blocks * BLOCK),
                (0u64..2 * BLOCK),
                any::<u64>(),
                (0u64..4).prop_map(|back| (u64::MAX - (BLOCK - 1)) - back * BLOCK),
            ]
        };
        // A READ's or WRITE's: as large as `MAX_TRANSFER` allows, or past it, refused for that.
        let io_len = prop_oneof![
            (0u64..4).prop_map(|blocks| blocks * BLOCK),
            0u64..3 * BLOCK,
            Just(u64::from(MAX_TRANSFER) + BLOCK),
        ];
        let kinds = prop_oneof![
            (edge(MAX_EXTENT), io_len.clone()).prop_map(|(offset, len)| (1, offset, len, 0)),
            (edge(MAX_EXTENT), io_len).prop_map(|(offset, len)| (2, offset, len, 0)),
            (edge(MAX_EXTENT), edge(WINDOW), edge(WINDOW)).prop_map(|(o, l, w)| (4, o, l, w)),
            // Zeros, whose extent's range is ignored, whatever it is.
            (edge(MAX_EXTENT), edge(WINDOW), edge(WINDOW)).prop_map(|(o, l, w)| (5, o, l, w)),
            (edge(MAX_EXTENT), edge(WINDOW), edge(WINDOW)).prop_map(|(o, l, w)| (6, o, l, w)),
            (edge(WINDOW), edge(WINDOW)).prop_map(|(len, w)| (7, 0, len, w)),
            (edge(WINDOW), edge(WINDOW)).prop_map(|(len, w)| (8, 0, len, w)),
        ];
        let cases = Config::default().cases.max(4000);
        let config = Config { cases, failure_persistence: None, ..Config::default() };
        TestRunner::new(config)
            .run(&kinds, |(kind, offset, len, window_offset)| {
                let id = ExtentIdLe { cell: CELL.to_le(), sequence: 1u64.to_le() };
                let map = MapArgs {
                    id,
                    offset: offset.to_le(),
                    len: len.to_le(),
                    window_offset: window_offset.to_le(),
                };
                let range = UnmapArgs { window_offset: window_offset.to_le(), len: len.to_le() };
                let within_window = fits(window_offset, len, BLOCK, WINDOW);
                let mapped = fits(offset, len, BLOCK, u64::MAX);
                // Its bytes; the room it gives; whether doc-4 has it within its bounds.
                let (readable, room, within) = match kind {
                    1 | 2 => {
                        let io = io(1, offset, len as usize);
                        let transfers = len <= u64::from(MAX_TRANSFER);
                        let within = transfers && fits(offset, len, BLOCK, MAX_EXTENT);
                        let room = if transfers { len as usize } else { 0 };
                        let readable = match kind {
                            1 => bytes(VIRTIO_EXTENT_T_READ, 0, Some(io), &[]),
                            _ => bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io), &vec![7; room]),
                        };
                        (readable, if kind == 1 { room } else { 0 }, within)
                    }
                    4 => {
                        (bytes(VIRTIO_EXTENT_T_MAP, 0, Some(map), &[]), 0, within_window && mapped)
                    }
                    5 => {
                        let zeros =
                            bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_ZEROS, Some(map), &[]);
                        (zeros, 0, within_window)
                    }
                    6 => {
                        // The first map within its bounds, the second as generated.
                        let first = MapArgs { offset: 0, len: BLOCK.to_le(), window_offset: 0, id };
                        let entry = MapEntry { map, ..MapEntry::default() };
                        let flags = VIRTIO_EXTENT_MAP_MANY;
                        let many = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(first), entry.as_bytes());
                        (many, 0, within_window && mapped)
                    }
                    7 => (bytes(VIRTIO_EXTENT_T_UNMAP, 0, Some(range), &[]), 0, within_window),
                    _ => (bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(range), &[]), 0, within_window),
                };
                let before = cell.state().ranges_asked;
                let (_, status) = request(&cell, &readable, room + RESPONSE);
                let came = if cell.state().ranges_asked > before {
                    Came::Asked
                } else {
                    Came::Refused(status)
                };
                let expected =
                    if within { Came::Asked } else { Came::Refused(errno(Errno::EINVAL)) };
                prop_assert_eq!(came, expected, "kind {} status {}", kind, status);
                let out_of_range = cell.state().out_of_range.clone();
                prop_assert!(out_of_range.is_empty(), "the cell was asked {:?}", out_of_range);
                Ok(())
            })
            .unwrap();
    }
}
