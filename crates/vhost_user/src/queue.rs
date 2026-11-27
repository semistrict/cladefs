// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A started queue: its rings mapped from guest memory, the chains taken off it and not yet
//! returned, and how a chain returned interrupts the guest. Chains are taken on the channel's
//! thread and returned on whichever thread served them; a call the guest asked for is written on
//! the channel's thread ([`crate::Channel`]), woken unless it is the one returning the chain.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::eventfd::Wake;
use crate::memory::{Mapping, MemoryError, MemoryTable};
use crate::protocol::VringAddr;
use crate::ring::{self, Device, RingError, RingMemory, Rings, Segment};

/// What the channel's thread and the threads returning chains share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) wake: Wake,
    /// The queues whose guest asked for a call, a bit each, which the channel's thread writes.
    pub(crate) calls: AtomicU64,
    /// The channel's thread, which needs no waking to write calls it finds asked for.
    pub(crate) thread: OnceLock<std::thread::ThreadId>,
    /// Whether the channel stops ([`crate::Channel::stop`]).
    pub(crate) stop: AtomicBool,
}

impl Shared {
    /// Wakes the channel's thread, unless it is this one, which looks before it waits.
    pub(crate) fn wake(&self) {
        if self.thread.get() != Some(&std::thread::current().id()) {
            self.wake.wake();
        }
    }
}

/// A queue's rings, mapped from guest memory: its descriptor table and available ring read-only,
/// its used ring writable.
#[derive(Debug)]
pub(crate) struct RingMaps {
    size: u16,
    desc: Mapping,
    avail: Mapping,
    used: Mapping,
}

impl RingMaps {
    /// The rings of `size` entries at `addr`, mapped from `memory`'s regions, each aligned as
    /// virtio has them.
    pub(crate) fn map(
        memory: &MemoryTable,
        size: u16,
        addr: VringAddr,
    ) -> Result<Self, MemoryError> {
        let (desc_len, avail_len, used_len) = ring::ring_lens(size);
        let misaligned = |addr: u64, len: u64| MemoryError::User { addr, len };
        if !addr.desc.is_multiple_of(16) {
            return Err(misaligned(addr.desc, desc_len));
        }
        if !addr.avail.is_multiple_of(2) {
            return Err(misaligned(addr.avail, avail_len));
        }
        if !addr.used.is_multiple_of(4) {
            return Err(misaligned(addr.used, used_len));
        }
        Ok(Self {
            size,
            desc: memory.map_user(addr.desc, desc_len, false)?,
            avail: memory.map_user(addr.avail, avail_len, false)?,
            used: memory.map_user(addr.used, used_len, true)?,
        })
    }
}

impl RingMemory for RingMaps {
    fn rings(&self) -> Rings<'_> {
        // SAFETY: Each part mapped as long as `self`, as many bytes as `ring_lens` says, aligned
        // (`RingMaps::map`); the guest writes them at any time, which the ring reads through
        // atomics alone.
        unsafe { Rings::from_raw(self.size, self.desc.ptr(), self.avail.ptr(), self.used.ptr()) }
    }
}

/// The state of a started queue's ring.
struct RunRing {
    device: Device<Arc<RingMaps>>,
    /// Chains taken and not returned.
    in_flight: usize,
    /// cladefs: a ring that meant nothing, which serves no more.
    broken: bool,
}

/// A started queue: its ring, and what returning a chain needs.
pub(crate) struct Run {
    index: u16,
    ring: Mutex<RunRing>,
    shared: Arc<Shared>,
}

/// Why a queue took no chain: its ring broke, now or before.
#[derive(Debug)]
pub(crate) struct Broken(pub(crate) Option<RingError>);

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run").field("index", &self.index).finish_non_exhaustive()
    }
}

impl Run {
    pub(crate) fn new(index: u16, maps: RingMaps, base: u16, shared: Arc<Shared>) -> Self {
        let ring =
            RunRing { device: Device::new(Arc::new(maps), base), in_flight: 0, broken: false };
        Self { index, ring: Mutex::new(ring), shared }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RunRing> {
        self.ring.lock().expect("a queue's lock is never held across a panic")
    }

    /// Takes the chains the guest made available, each checked against `memory`, until none is
    /// left and the ring asked to be kicked for the next: its ring broken if they mean nothing.
    pub(crate) fn take(self: &Arc<Self>, memory: &Arc<MemoryTable>) -> Result<Vec<Chain>, Broken> {
        let mut ring = self.lock();
        if ring.broken {
            return Err(Broken(None));
        }
        let within = |addr: u64, len: u32| memory.check(addr, u64::from(len)).is_ok();
        let mut chains = Vec::new();
        loop {
            match ring.device.pop(within) {
                Ok(Some(chain)) => {
                    ring.in_flight += 1;
                    chains.push(Chain {
                        run: self.clone(),
                        memory: memory.clone(),
                        head: chain.head,
                        segments: chain.segments,
                        done: false,
                    });
                }
                Ok(None) if !ring.device.arm() => return Ok(chains),
                Ok(None) => {}
                Err(err) => {
                    ring.broken = true;
                    drop(ring);
                    // Those taken are served still, and returned to a ring that serves no more.
                    drop(chains);
                    return Err(Broken(Some(err)));
                }
            }
        }
    }

    /// The available index of the next chain to take, and how many are taken and not returned.
    pub(crate) fn state(&self) -> (u16, usize) {
        let ring = self.lock();
        (ring.device.last_avail(), ring.in_flight)
    }

    /// Its rings, in `maps` from now on, as a new memory table maps them.
    pub(crate) fn remap(&self, maps: RingMaps) {
        self.lock().device.set_memory(Arc::new(maps));
    }

    /// Returns the chain of `head` used, `len` bytes of it written; asks the channel's thread to
    /// call the guest if it asked to be, or to look at the queue if it stops and this was its last.
    fn complete(&self, head: u16, len: u32) {
        let mut ring = self.lock();
        ring.device.push(head, len);
        let call = ring.device.publish();
        ring.in_flight -= 1;
        let last = ring.in_flight == 0;
        drop(ring);
        if call {
            self.shared.calls.fetch_or(1 << self.index, Ordering::AcqRel);
        }
        if call || last {
            self.shared.wake();
        }
    }
}

/// A chain taken off a queue, owed back to it: returned as [`Chain::complete`] says, or, dropped
/// unreturned, as a panic serving it does, returned with nothing written, which the guest's driver
/// takes for `EIO`.
#[derive(Debug)]
pub struct Chain {
    run: Arc<Run>,
    /// The memory table its buffers were checked against, which holds them while it is served.
    memory: Arc<MemoryTable>,
    head: u16,
    segments: Vec<Segment>,
    done: bool,
}

impl Chain {
    /// The queue it is of.
    pub fn queue(&self) -> u16 {
        self.run.index
    }

    /// The bytes of its readable buffers, and of its writable ones, the response's among them.
    pub fn lens(&self) -> (u64, u64) {
        let sum = |writable: bool| {
            self.segments
                .iter()
                .filter(|segment| segment.writable == writable)
                .map(|segment| u64::from(segment.len))
                .sum()
        };
        (sum(false), sum(true))
    }

    /// The first `buf.len()` bytes of its readable buffers, read from guest memory: an error if
    /// they have fewer, or guest memory fails the read.
    pub fn read(&self, buf: &mut [u8]) -> Result<(), MemoryError> {
        let mut rest = buf;
        for segment in self.segments.iter().filter(|segment| !segment.writable) {
            if rest.is_empty() {
                break;
            }
            let len = rest.len().min(segment.len as usize);
            let (part, after) = rest.split_at_mut(len);
            self.memory.read(segment.offset, part)?;
            rest = after;
        }
        match rest.is_empty() {
            true => Ok(()),
            false => Err(MemoryError::Io("a chain's readable buffers end short".into())),
        }
    }

    /// Writes `bytes` into its writable buffers, from their start: an error if they have fewer, or
    /// guest memory fails the write.
    pub fn write(&self, bytes: &[u8]) -> Result<(), MemoryError> {
        let mut rest = bytes;
        for segment in self.segments.iter().filter(|segment| segment.writable) {
            if rest.is_empty() {
                break;
            }
            let len = rest.len().min(segment.len as usize);
            let (part, after) = rest.split_at(len);
            self.memory.write(segment.offset, part)?;
            rest = after;
        }
        match rest.is_empty() {
            true => Ok(()),
            false => Err(MemoryError::Io("a chain's writable buffers end short".into())),
        }
    }

    /// Returns it used, `len` bytes of its writable buffers written.
    pub fn complete(mut self, len: u32) {
        self.done = true;
        self.run.complete(self.head, len);
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        if !self.done {
            self.run.complete(self.head, 0);
        }
    }
}
