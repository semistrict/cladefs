// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The cell's locks, condition variables and threads: std's, or with the `shuttle` feature
//! Shuttle's (awslabs/shuttle), whose scheduler then chooses every interleaving of the threads
//! using a cell, and replays a failing one from its seed (tests/shuttle/, TASK-44.4). Arcs are
//! std's either way.
//!
//! And the rules the cell's concurrency relies on, which those tests turn off one at a time to
//! see a test fail: a test that never fails proves nothing.
//!
//! cladefs: and the salts of its raw files ([`salt`]): the system's, or Shuttle's, seeded.

#[cfg(feature = "shuttle")]
pub(crate) use shuttle::sync::{Condvar, Mutex, MutexGuard};

/// cladefs: the salt of a new raw file (`raw::RawEncryption::new`, TASK-411.5): drawn from the
/// system's generator; under Shuttle, from its generator, seeded as its schedule is, so that a
/// test's run is a function of its seed (tests/shuttle/crash.rs). What a crash leaves of a raw file
/// unwritten, a page the cache held dirty, reads as the decryption of zeros under the file's key,
/// as unlike from one salt to another as noise: its framed segment, uploaded, holds it,
/// unreferenced.
pub(crate) fn salt() -> extent_keys::Salt {
    #[cfg(feature = "shuttle")]
    {
        use shuttle::rand::RngCore as _;
        let mut salt = [0; extent_keys::SALT_LEN];
        shuttle::rand::thread_rng().fill_bytes(&mut salt);
        extent_keys::Salt(salt)
    }
    #[cfg(not(feature = "shuttle"))]
    extent_keys::Salt::random()
}
use std::sync::Arc;
#[cfg(not(feature = "shuttle"))]
pub(crate) use std::sync::{Condvar, Mutex, MutexGuard};

// The lock its awaiting takers race for: a Shuttle atomic under Shuttle, so that its scheduler
// chooses who wins it.
#[cfg(feature = "shuttle")]
use shuttle::sync::atomic::AtomicBool;
#[cfg(not(feature = "shuttle"))]
use std::sync::atomic::AtomicBool;

/// cladefs: what the cell's waits await, holding no thread (board 2944): said as what they wait
/// for changes. A wait listens, then looks, then, if it is to wait, lets its locks go and awaits
/// the listener, so that a change after its look, made under the lock it looked under, wakes it.
/// event_listener's, built normally, under madsim and under Shuttle alike: its listeners' wakers
/// are those of the futures awaiting them, which Shuttle's executor schedules.
pub(crate) use event_listener::Event;

/// cladefs: runs `future` to its end on the calling thread, which it blocks meanwhile: for an
/// outermost entry alone, as a cell dropped unclosed closes there (board 2944). Shuttle's own
/// under Shuttle, which schedules the thread's waiting; else [`wait_here`].
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    #[cfg(feature = "shuttle")]
    return shuttle::future::block_on(future);
    #[cfg(not(feature = "shuttle"))]
    return wait_here(future);
}

/// cladefs: runs `future` to its end on the calling thread, parked while it waits, for what it
/// waits on to wake it (board 2944): what a drop of what was never closed closes with, within
/// whatever polls the drop, an executor's task or a test's thread. Not
/// `futures::executor::block_on`, which refuses to run within another of its executors, as a drop
/// within one does.
pub fn wait_here<F: std::future::Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::park();
    }
}

/// cladefs: a lock its takers await, holding no thread meanwhile (board 2944): for the cell's
/// locks held across work others wait for, its commit's and its flush's. Taken by one at a time;
/// its guard, dropped, lets the next take it. Shared beyond one cell, as its followers share its
/// writer's: a panic holding it aborts the extent daemon, as one holding a `SharedLock` does
/// ([`crate::shared::Awaited`], board 1228).
pub(crate) struct AsyncMutex<T> {
    /// What the log calls it.
    name: &'static str,
    held: AtomicBool,
    let_go: Event,
    value: std::cell::UnsafeCell<T>,
}

// SAFETY: one guard at a time reaches the value, as `held` says, across threads as a mutex's.
unsafe impl<T: Send> Send for AsyncMutex<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for AsyncMutex<T> {}

impl<T> AsyncMutex<T> {
    pub(crate) fn new(name: &'static str, value: T) -> Self {
        Self {
            name,
            held: AtomicBool::new(false),
            let_go: Event::new(),
            value: std::cell::UnsafeCell::new(value),
        }
    }

    /// The lock, once no other holds it.
    pub(crate) async fn lock(&self) -> AsyncMutexGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            let let_go = self.let_go.listen();
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            let_go.await;
        }
    }

    /// The lock, if no other holds it.
    pub(crate) fn try_lock(&self) -> Option<AsyncMutexGuard<'_, T>> {
        use std::sync::atomic::Ordering;
        // Taken, Acquire, as a mutex's lock: what the last holder wrote, its Release, is seen.
        // Refused, Relaxed: nothing is read of the value then, and the guard is not made.
        let taken = self.held.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed);
        // Held by another: none, as try_lock answers.
        taken.ok().map(|_| AsyncMutexGuard {
            mutex: self,
            _awaited: crate::shared::Awaited::new(self.name),
        })
    }
}

/// An [`AsyncMutex`] held.
pub(crate) struct AsyncMutexGuard<'a, T> {
    mutex: &'a AsyncMutex<T>,
    /// Dropped after the lock is let go of, as a `Shared`'s holding is.
    _awaited: crate::shared::Awaited,
}

impl<T> std::ops::Deref for AsyncMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard is the one holder (`AsyncMutex::try_lock`).
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> std::ops::DerefMut for AsyncMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, `&mut self` its one use.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for AsyncMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.held.store(false, std::sync::atomic::Ordering::Release);
        self.mutex.let_go.notify(1);
    }
}

/// std's threads; under madsim, which runs one thread, used for their identities alone: the cell's
/// work runs on the pool's (`crate::pool`), which is simulated tasks there (doc-24).
#[cfg(not(feature = "shuttle"))]
pub(crate) use std::thread;

/// Shuttle's threads, refused as a host short of them refuses std's, if a test has it refuse them
/// (`refuse_threads`): `spawn` panics, and `Builder`'s returns the refusal, so that the pool starts
/// its threads as on the host, and the Shuttle tests of such a host find any thread started by
/// `spawn` (TASK-79.80). `Scope::spawn` is Shuttle's, never refused: only tests start scoped
/// threads.
#[cfg(feature = "shuttle")]
pub(crate) mod thread {
    pub(crate) use shuttle::thread::*;

    /// Starts a thread running `f`, as Shuttle's `spawn` does; or panics, as std's does, if the
    /// host refuses it. The cell starts none so, as one refused would panic through whatever
    /// locks its caller holds: this stands for std's, so that the Shuttle tests of a host refusing
    /// threads find any thread started so on the paths they take.
    #[expect(dead_code, reason = "the cell starts no thread by `spawn`")]
    #[track_caller]
    pub(crate) fn spawn<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Builder::new().spawn(f).expect("failed to spawn thread")
    }

    /// Starts threads as std's `Builder` does, returning the host's refusal of one.
    pub(crate) struct Builder;

    impl Builder {
        pub(crate) fn new() -> Self {
            Self
        }

        /// Names the thread, as std's does: Shuttle's threads have none.
        pub(crate) fn name(self, _name: String) -> Self {
            self
        }

        /// Starts a thread running `f`, as Shuttle's `spawn` does, or returns the host's refusal.
        #[track_caller]
        pub(crate) fn spawn<F, T>(self, f: F) -> std::io::Result<JoinHandle<T>>
        where
            F: FnOnce() -> T + Send + 'static,
            T: Send + 'static,
        {
            super::refused()?;
            Ok(shuttle::thread::spawn(f))
        }
    }
}

/// The host's refusal of a thread, `EAGAIN`, if a test has it refuse them (`refuse_threads`).
#[cfg(feature = "shuttle")]
fn refused() -> std::io::Result<()> {
    match REFUSING.load(std::sync::atomic::Ordering::Relaxed) {
        true => Err(std::io::Error::from_raw_os_error(libc::EAGAIN)),
        false => Ok(()),
    }
}

/// Whether the host refuses the threads [`thread`] would start.
#[cfg(feature = "shuttle")]
static REFUSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Has the host refuse the threads [`thread`] would start, or start them again: for the Shuttle
/// tests of a host short of threads.
#[cfg(all(feature = "shuttle", test))]
pub(crate) fn refuse_threads(refusing: bool) {
    REFUSING.store(refusing, std::sync::atomic::Ordering::Relaxed);
}

/// The rules the cell's concurrency relies on, each of which its Shuttle tests show to be needed
/// by turning it off.
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// A free keeps the extent's data for the uploader until it releases the commit that freed it.
    pub keep_frees: bool,
    /// Closing the open segment between commits waits for the writes in it to finish.
    pub close_waits_for_writers: bool,
    /// A commit waits for the writes in progress to finish before it seals the open extents.
    pub commit_waits_for_writes: bool,
    /// A commit, or a closing between commits, that waited for the writes in progress goes on
    /// only if the cell did not fail, nor the cell close, meanwhile (TASK-83).
    pub recheck_after_waiting: bool,
    /// A write to an extent freed since it began is dropped as it finishes (TASK-79.1).
    pub drop_writes_to_freed_extents: bool,
    /// Closing a cell waits for a flush in flight to finish.
    pub close_waits_for_flushes: bool,
    /// Closing a cell waits for every segment being framed, whatever closed it, a commit no
    /// flush took or one that failed among them (TASK-41.1, TASK-79.99).
    pub close_waits_for_framings: bool,
    /// A segment closed is framed at once, and its framing ends itself, whatever closed it: those
    /// filled between commits, which the commit after closes, wait for no commit after that, which
    /// may never come, to frame them, as the commit an uploader waits for is in them (TASK-150.2,
    /// TASK-153.8, TASK-92.41).
    pub framings_end_themselves: bool,
    /// A free of an extent sealed before keeps its data while a view older than the free lives.
    pub keep_frees_for_views: bool,
    /// A free of an extent a view pins keeps its data while it does.
    pub keep_pins: bool,
    /// A view refuses extents sealed after its commit.
    pub views_refuse_later_extents: bool,
    /// A view reads, of the frees kept, only those after its commit, and those it pins.
    pub views_hold_their_commit: bool,
}

impl Rules {
    pub const ALL: Self = Self {
        keep_frees: true,
        close_waits_for_writers: true,
        commit_waits_for_writes: true,
        recheck_after_waiting: true,
        drop_writes_to_freed_extents: true,
        close_waits_for_flushes: true,
        close_waits_for_framings: true,
        framings_end_themselves: true,
        keep_frees_for_views: true,
        keep_pins: true,
        views_refuse_later_extents: true,
        views_hold_their_commit: true,
    };
}

/// The rules in force: all of them, but in a Shuttle test that turned one off.
#[cfg(not(feature = "shuttle"))]
pub(crate) fn rules() -> Rules {
    Rules::ALL
}

#[cfg(feature = "shuttle")]
static RULES: std::sync::Mutex<Rules> = std::sync::Mutex::new(Rules::ALL);

/// The rules in force: all of them, but in a Shuttle test that turned one off.
#[cfg(feature = "shuttle")]
pub fn rules() -> Rules {
    *RULES.lock().unwrap()
}

/// Sets the rules in force, for the Shuttle tests that turn one off.
#[cfg(feature = "shuttle")]
pub fn set_rules(rules: Rules) {
    *RULES.lock().unwrap() = rules;
}
