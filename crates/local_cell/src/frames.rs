// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the frames a page cache's pages are held in ([`crate::pages`], TASK-433.9), as Linux's
//! page allocator gives its page cache pages (mm/page_alloc.c): memory the process allocates in
//! slabs of [`SLAB`], aligned to them, advised to be transparent huge pages where the host has
//! them; from its global allocator, which counts them as it counted pages boxed (a test's heap
//! budget, cladefs's `heap`). Each page boxed on its own, as before, was memory the process had
//! never touched, which faulted in a 4 KiB page at a time: 81 µs of a 256 KiB write's 237 to copy
//! it into the cache, on the test host, against 20 with malloc's heap in huge pages, and 25 with
//! these frames.
//!
//! A frame is taken from the slab lowest in memory that has one free, a frame used before first,
//! so that the frames in use stay packed in few slabs, as a slab allocator packs them. Slabs are
//! freed whole, once every frame of theirs is free, but for [`SPARE_SLABS`] of them kept to hold
//! the next pages: never a frame given to the host alone (`MADV_DONTNEED`), which is less than a
//! page of a host with pages of 16 or 64 KiB, and which khugepaged would make resident again, its
//! huge page whole.
//! What a cache holds, what the extent daemon's governor counts, is its pages, as it was with
//! malloc's: the free frames of slabs with pages in use, as malloc's free chunks, and the spare
//! slabs, are the process's, outside it.
//!
//! A slab the allocator refuses aborts the process, as its failure did a page boxed: a panic would
//! unwind a write half done (TASK-433.10).

use std::alloc::Layout;
use std::collections::{BTreeMap, BTreeSet};
use std::ptr::NonNull;
use std::sync::Arc;

use crate::pages::PAGE;

/// What the frames are allocated in, a transparent huge page's worth on x86-64 and ARM64 Linux with
/// 4 KiB pages, and aligned to it.
pub const SLAB: usize = 2 << 20;

/// The slabs whose every frame is free a cache keeps, allocated, for the next pages: what a burst of
/// writes after its pages went takes before it allocates more.
pub const SPARE_SLABS: usize = 8;

const PER_SLAB: usize = SLAB / PAGE as usize;

const SLAB_LAYOUT: Layout = match Layout::from_size_align(SLAB, SLAB) {
    Ok(layout) => layout,
    Err(_) => panic!("a slab's layout"),
};

/// A page's memory, [`PAGE`] bytes, its frames' own until dropped, when they have it back.
pub struct Frame {
    bytes: NonNull<[u8; PAGE as usize]>,
    frames: Arc<Frames>,
}

// SAFETY: A frame is the one owner of its bytes, which it hands out as `&` and `&mut` as a box
// does; its frames' slabs are behind a lock.
unsafe impl Send for Frame {}
// SAFETY: as Send's: `&Frame` gives only `&[u8]`.
unsafe impl Sync for Frame {}

impl std::ops::Deref for Frame {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: the bytes are this frame's alone while it lives, and allocated while their slab
        // has a frame in use, as this one is.
        unsafe { self.bytes.as_ref() }
    }
}

impl std::ops::DerefMut for Frame {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as Deref's, and `&mut self` is the one way to them.
        unsafe { self.bytes.as_mut() }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        self.frames.put(self.bytes);
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame").field("at", &self.bytes).finish()
    }
}

/// A cache's frames: its slabs, and the frames of each in use and free. Its lock is std's under
/// Shuttle too: no thread waits holding it, nor yields, so no schedule is one of its
/// interleavings.
#[derive(Default)]
pub struct Frames {
    slabs: std::sync::Mutex<Slabs>,
}

#[derive(Default)]
struct Slabs {
    /// Each slab by its address.
    slabs: BTreeMap<usize, Slab>,
    /// The slabs with a frame to hand out, used before or not.
    with_free: BTreeSet<usize>,
    /// The slabs whose every frame is free.
    idle: BTreeSet<usize>,
}

/// A slab's frames: those it handed out, the first `handed` of it, some of them free again.
#[derive(Default)]
struct Slab {
    /// Its frames handed out, some of them since given back.
    handed: usize,
    /// Its frames given back, by their places in it: handed out again first, the last given
    /// back first.
    free: Vec<usize>,
}

impl Slab {
    fn has_free(&self) -> bool {
        !self.free.is_empty() || self.handed < PER_SLAB
    }

    /// Whether every frame it handed out is back.
    fn idle(&self) -> bool {
        self.free.len() == self.handed
    }
}

impl Frames {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// A frame holding `bytes`, [`PAGE`] of them.
    pub fn copied(self: &Arc<Self>, bytes: &[u8]) -> Frame {
        let mut frame = self.take();
        frame.copy_from_slice(bytes);
        frame
    }

    /// A frame of zeros.
    pub fn zeroed(self: &Arc<Self>) -> Frame {
        let mut frame = self.take();
        frame.fill(0);
        frame
    }

    /// A frame holding what it held, any frame's bytes of before: its caller's to fill whole.
    fn take(self: &Arc<Self>) -> Frame {
        let mut slabs = self.lock();
        let base = match slabs.with_free.first() {
            Some(&base) => base,
            None => {
                let base = allocate_slab();
                slabs.slabs.insert(base, Slab::default());
                slabs.with_free.insert(base);
                base
            }
        };
        slabs.idle.remove(&base);
        let slab = slabs.slabs.get_mut(&base).expect("a slab with a free frame");
        let at = slab.free.pop().unwrap_or_else(|| {
            slab.handed += 1;
            slab.handed - 1
        });
        if !slab.has_free() {
            slabs.with_free.remove(&base);
        }
        Frame { bytes: frame_at(base, at), frames: self.clone() }
    }

    fn put(&self, bytes: NonNull<[u8; PAGE as usize]>) {
        let mut slabs = self.lock();
        let address = bytes.as_ptr() as usize;
        let base = address & !(SLAB - 1);
        let slab = slabs.slabs.get_mut(&base).expect("a frame's slab");
        slab.free.push((address - base) / PAGE as usize);
        let idle = slab.idle();
        slabs.with_free.insert(base);
        if idle {
            slabs.idle.insert(base);
            // Past the spares, those highest in memory go.
            while slabs.idle.len() > SPARE_SLABS {
                let highest = slabs.idle.pop_last().expect("an idle slab");
                slabs.free(highest);
            }
        }
    }

    /// Its slabs, allocated.
    #[cfg(test)]
    fn slabs(&self) -> usize {
        self.lock().slabs.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slabs> {
        self.slabs.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Slabs {
    fn free(&mut self, base: usize) {
        let slab = self.slabs.remove(&base).expect("a slab to free");
        debug_assert!(slab.idle());
        self.with_free.remove(&base);
        // SAFETY: a slab these frames allocated, whose every frame is free: no frame holds a
        // pointer into it.
        unsafe { std::alloc::dealloc(base as *mut u8, SLAB_LAYOUT) };
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // Every frame holds its frames, so none is in use once they are dropped.
        let slabs = self.slabs.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        for &base in slabs.slabs.keys() {
            // SAFETY: a slab these frames allocated, which no frame uses any more.
            unsafe { std::alloc::dealloc(base as *mut u8, SLAB_LAYOUT) };
        }
    }
}

fn frame_at(base: usize, at: usize) -> NonNull<[u8; PAGE as usize]> {
    NonNull::new((base + at * PAGE as usize) as *mut [u8; PAGE as usize]).expect("a slab's frame")
}

/// A slab of [`SLAB`] bytes, aligned to them, advised to be huge pages where the host has them.
fn allocate_slab() -> usize {
    // SAFETY: a layout of a size other than zero.
    let base = unsafe { std::alloc::alloc(SLAB_LAYOUT) };
    if base.is_null() {
        std::alloc::handle_alloc_error(SLAB_LAYOUT);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: advice on memory just allocated, aligned to a huge page and as long. Refused, as a
    // host without transparent huge pages refuses it, the slab is in small pages, as malloc's
    // memory was.
    unsafe {
        libc::madvise(base.cast(), SLAB, libc::MADV_HUGEPAGE);
    }
    base as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_freed_hold_the_next() {
        let frames = Frames::new();
        let first = frames.copied(&[7; PAGE as usize]);
        let at = first.as_ptr();
        drop(first);
        let second = frames.zeroed();
        assert_eq!(second.as_ptr(), at, "the frame freed is used first");
        assert_eq!(&second[..], &[0; PAGE as usize][..], "zeroed, not what it held");
    }

    #[test]
    fn frames_are_pages_of_aligned_slabs() {
        let frames = Frames::new();
        let held: Vec<Frame> = (0..PER_SLAB + 1).map(|_| frames.zeroed()).collect();
        assert!((held[0].as_ptr() as usize).is_multiple_of(SLAB), "a slab's first frame first");
        for (at, frame) in held[..PER_SLAB].iter().enumerate() {
            assert_eq!(frame.as_ptr() as usize, held[0].as_ptr() as usize + at * PAGE as usize);
        }
        assert!((held[PER_SLAB].as_ptr() as usize).is_multiple_of(SLAB), "a second slab's");
        assert_eq!(frames.slabs(), 2);
    }

    /// Frames come from the slab lowest in memory with one free, so that those in use stay packed.
    #[test]
    fn frames_are_taken_from_the_lowest_slab_with_one_free() {
        let frames = Frames::new();
        let mut held: Vec<Frame> = (0..3 * PER_SLAB).map(|_| frames.zeroed()).collect();
        held.sort_by_key(|frame| frame.as_ptr() as usize);
        let lowest = held[0].as_ptr() as usize;
        // A frame freed in the highest slab, then one in the lowest: the lowest's is used again.
        drop(held.remove(3 * PER_SLAB - 1));
        let low = held.remove(5);
        let low_at = low.as_ptr();
        drop(low);
        let again = frames.zeroed();
        assert_eq!(again.as_ptr(), low_at);
        assert!((again.as_ptr() as usize) < lowest + SLAB);
    }

    /// Slabs go whole, once every frame of theirs is free, past the spares kept: those highest in
    /// memory first; never one with a frame in use, whatever is free of it.
    #[test]
    fn idle_slabs_past_the_spares_are_freed() {
        let frames = Frames::new();
        let slabs = SPARE_SLABS + 3;
        let mut held: Vec<Frame> = (0..slabs * PER_SLAB).map(|_| frames.zeroed()).collect();
        held.sort_by_key(|frame| frame.as_ptr() as usize);
        let lowest = held[0].as_ptr() as usize;
        // A frame of the lowest slab kept: it stays, its other frames free.
        let survivor = held.remove(PER_SLAB - 1);
        drop(held);
        assert_eq!(frames.slabs(), SPARE_SLABS + 1, "the spares and the one with a frame in use");
        drop(survivor);
        assert_eq!(frames.slabs(), SPARE_SLABS);
        let next = frames.zeroed();
        assert!((next.as_ptr() as usize) < lowest + SLAB, "the lowest slab kept, used first");
    }
}
