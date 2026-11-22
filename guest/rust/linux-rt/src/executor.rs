// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Futures in the kernel: `block_on` polls one on the calling thread, which sleeps between polls,
//! and `spawn` polls one on the kernel's workers, a poll per wake.

use crate::ffi;
use alloc::boxed::Box;
use alloc::sync::Arc;
use async_task::Runnable;
use core::ffi::c_void;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// Futures up to this size are pinned on the caller's stack rather than boxed in `block_on`,
/// avoiding per-VFS-call heap allocation while keeping large futures off 16 KiB kernel stacks
/// (board 6014, TASK-26.9).
const INLINE_FUTURE_MAX: usize = 512;

/// Polls `future` to completion on the calling thread, which sleeps while the future waits.
///
/// Allocates neither a `Parker` nor a heap box when a small future resolves on its first poll
/// without registering a waker (board 6014). Large futures are boxed off the 16 KiB kernel stack.
pub fn block_on<F: Future>(future: F) -> F::Output {
    if const { core::mem::size_of::<F>() <= INLINE_FUTURE_MAX } {
        let mut future = core::pin::pin!(future);
        poll_pinned(future.as_mut())
    } else {
        let mut future = Box::pin(future);
        poll_pinned(future.as_mut())
    }
}

#[inline(never)]
fn poll_pinned<F: Future>(mut future: Pin<&mut F>) -> F::Output {
    // Declared before `waker` so `waker` drops before `lazy` does.
    let lazy = LazyParker::new();
    // SAFETY: `waker` borrows `&lazy` for the duration of this stack frame only. Any `clone()`
    // on `waker` promotes to an owned `Arc<Parker>` (`PARKER_VTABLE`), so no cloned waker holds a
    // pointer to `lazy`.
    let waker = unsafe {
        Waker::from_raw(RawWaker::new((&lazy as *const LazyParker).cast(), &LAZY_VTABLE))
    };
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        lazy.park();
    }
}

/// Stack-resident slot that allocates an `Arc<Parker>` on demand only if a future clones or wakes
/// its waker or yields `Poll::Pending` (board 6014).
struct LazyParker(AtomicPtr<Parker>);

impl LazyParker {
    const fn new() -> Self {
        Self(AtomicPtr::new(ptr::null_mut()))
    }

    fn get_or_init(&self) -> *const Parker {
        let ptr = self.0.load(Ordering::Acquire);
        if !ptr.is_null() {
            return ptr;
        }
        let raw = Arc::into_raw(Arc::new(Parker::new())) as *mut Parker;
        match self.0.compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => raw,
            Err(existing) => {
                // SAFETY: `raw` was just created by `Arc::into_raw` and never published.
                unsafe { drop(Arc::from_raw(raw)) };
                existing
            }
        }
    }

    fn park(&self) {
        let ptr = self.get_or_init();
        // SAFETY: `self` owns a strong `Arc` reference to `ptr` until `LazyParker::drop`.
        unsafe { (&*ptr).park() };
    }
}

impl Drop for LazyParker {
    fn drop(&mut self) {
        let ptr = *self.0.get_mut();
        if !ptr.is_null() {
            // SAFETY: `ptr` was stored from `Arc::into_raw` and `self` holds that strong count.
            unsafe { drop(Arc::from_raw(ptr)) };
        }
    }
}

static LAZY_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |data| {
        // SAFETY: `data` points to the live `LazyParker` on `poll_pinned`'s stack while the
        // borrowed `Waker` is in scope.
        let ptr = unsafe { &*data.cast::<LazyParker>() }.get_or_init();
        // SAFETY: `LazyParker` holds a live `Arc` reference to `ptr`; increment for the clone.
        unsafe { Arc::increment_strong_count(ptr) };
        RawWaker::new(ptr.cast(), &PARKER_VTABLE)
    },
    |data| {
        // SAFETY: Same as clone; unreachable through `Context::waker(&self)` but sound if called.
        let ptr = unsafe { &*data.cast::<LazyParker>() }.get_or_init();
        // SAFETY: `LazyParker` holds a live `Arc` reference to `ptr`.
        unsafe { (&*ptr).unpark() };
    },
    |data| {
        // SAFETY: Same as clone.
        let ptr = unsafe { &*data.cast::<LazyParker>() }.get_or_init();
        // SAFETY: `LazyParker` holds a live `Arc` reference to `ptr`.
        unsafe { (&*ptr).unpark() };
    },
    |_| {},
);

static PARKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |data| {
        let ptr = data.cast::<Parker>();
        // SAFETY: `data` is a live `Arc::into_raw` pointer owned by this `Waker`.
        unsafe { Arc::increment_strong_count(ptr) };
        RawWaker::new(data, &PARKER_VTABLE)
    },
    |data| {
        // SAFETY: `data` is an owned `Arc::into_raw` pointer; consume it after unparking.
        let parker = unsafe { Arc::from_raw(data.cast::<Parker>()) };
        parker.unpark();
    },
    |data| {
        // SAFETY: `data` is a live `Arc::into_raw` pointer borrowed for `wake_by_ref`.
        unsafe { (&*data.cast::<Parker>()).unpark() };
    },
    |data| {
        // SAFETY: `data` is an owned `Arc::into_raw` pointer.
        unsafe { drop(Arc::from_raw(data.cast::<Parker>())) };
    },
);

/// Lets a thread wait until it is woken, spinning briefly before it sleeps (the kernel's side,
/// `cladefs_rt_park`): a wake lets a `park` return, and wakes meanwhile are taken with it.
struct Parker(NonNull<ffi::Parker>);

// SAFETY: A parker may be parked on by one thread and unparked from any context.
unsafe impl Send for Parker {}
// SAFETY: As above.
unsafe impl Sync for Parker {}

impl Parker {
    fn new() -> Self {
        // SAFETY: Allocates a parker, which `drop` frees.
        let parker = unsafe { ffi::cladefs_rt_parker_new() };
        Self(NonNull::new(parker).expect("no memory for a parker"))
    }

    fn park(&self) {
        // SAFETY: The parker is live.
        unsafe { ffi::cladefs_rt_park(self.0.as_ptr()) }
    }

    fn unpark(&self) {
        // SAFETY: The parker is live; unparking does not sleep.
        unsafe { ffi::cladefs_rt_unpark(self.0.as_ptr()) }
    }
}

impl Drop for Parker {
    fn drop(&mut self) {
        // SAFETY: Nothing else holds the parker.
        unsafe { ffi::cladefs_rt_parker_free(self.0.as_ptr()) }
    }
}

/// A future running on the kernel's workers. Dropping it cancels the future; `detach` lets it run
/// on.
#[must_use = "dropping a task cancels it"]
pub struct Task<T>(async_task::Task<T, Work>);

impl<T> Task<T> {
    /// Lets the future run to completion without waiting for it.
    pub fn detach(self) {
        self.0.detach();
    }

    /// Whether the future is done.
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    /// Cancels the future, and returns its output if it completed first.
    pub async fn cancel(self) -> Option<T> {
        self.0.cancel().await
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        Pin::new(&mut self.0).poll(cx)
    }
}

/// Runs `future` on the kernel's workers.
pub fn spawn<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_on(future, false)
}

/// Runs `future`, which tasks wait for, on the kernel's workers at a high priority, as a
/// filesystem's kernel threads run.
pub fn spawn_urgent<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_on(future, true)
}

/// Runs `future` on the kernel's workers, at a high priority if `urgent`, its polls queued on a
/// work item of its own ([`Work`]).
fn spawn_on<F>(future: F, urgent: bool) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (runnable, task) =
        async_task::Builder::new().metadata(Work::new(urgent)).spawn(|_| future, schedule);
    runnable.schedule();
    Task(task)
}

/// A task's work item (`cladefs_rt_work`), which its polls are queued on, one at a time, as a task is
/// scheduled again only once its poll has begun: made with the task, where the kernel may sleep,
/// so that a waker, in any context, queues a poll without allocating, and can't fail to, which
/// would leave the task waiting for ever (TASK-60.1). Freed with the task, in its last poll, or
/// wherever its last waker or handle is dropped.
struct Work(NonNull<ffi::Work>);

// SAFETY: A work item may be queued from any context, and freed from any once queued no more.
unsafe impl Send for Work {}
// SAFETY: As above; it is queued once at a time (`schedule`).
unsafe impl Sync for Work {}

impl Work {
    fn new(urgent: bool) -> Self {
        // SAFETY: Makes a work item, without failing, which `drop` frees.
        let work = unsafe { ffi::cladefs_rt_work_new(run, urgent) };
        Self(NonNull::new(work).expect("a work item"))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        // SAFETY: Its task, gone, holds no runnable, so it is queued no more.
        unsafe { ffi::cladefs_rt_work_free(self.0.as_ptr()) }
    }
}

/// Queues a task's next poll on its work item. Wakers call this from any context: it allocates
/// nothing.
fn schedule(runnable: Runnable<Work>) {
    let work = runnable.metadata().0;
    let runnable = runnable.into_raw();
    // SAFETY: `run` takes back the runnable, once; the task, alive while it is held, holds the
    // work item.
    unsafe { ffi::cladefs_rt_work_queue(work.as_ptr(), runnable.as_ptr().cast()) };
}

extern "C" fn run(runnable: *mut c_void) {
    // SAFETY: `schedule` gave up the runnable, which is not null, for this call.
    let runnable = unsafe { Runnable::<Work>::from_raw(NonNull::new_unchecked(runnable.cast())) };
    runnable.run();
}
