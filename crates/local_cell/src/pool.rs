// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the threads every cell of the process does its background work on (board 2944): one
//! pool for the process, however many cells it holds, of [`threads`] at most, each started as work
//! first finds none free and kept for the process's life, taking jobs from one queue.
//!
//! Before it, each cell started a thread of its own for each piece of its work, its framer's, its
//! syncer's, its opener's, and threads of their own for a framing's parts and a flush's syncs:
//! thousands of open cells started and ended threads at every commit, and nothing bounded how
//! many ran. tokio's blocking pool bounds them no better: its threads idle 10 s before they go, and
//! hundreds of guests committing at once kept 112 of them
//! (`hundreds_of_writers_flush_on_threads_they_share`, board 744).
//!
//! What the pool runs never waits for a job queued behind it, so that a pool all of whose threads
//! are busy only delays work, never deadlocks: a cell's jobs ([`spawn_or_return`]) wait for no
//! other job, only for the cell's lock, which none holds across a wait; and what a job does on
//! threads beside its own ([`help`]) it does on its own thread too, on as many others as are free
//! then, never waiting for one to be.
//!
//! Under madsim, which runs one thread, none: a cell's job is a simulated task, and the caller
//! does what it would have had help with (doc-24). Under Shuttle, the pool's threads are Shuttle's,
//! each execution's pool its own, and a thread with nothing queued ends rather than waits, as one
//! waiting at an execution's end would be left behind it.

#[cfg(not(madsim))]
use crate::sync::{Condvar, Mutex, MutexGuard, thread};
#[cfg(not(madsim))]
use std::collections::VecDeque;
#[cfg(not(madsim))]
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
#[cfg(not(madsim))]
use std::sync::Arc;

/// How many threads the pool has at most: as many as every flush's syncs share
/// ([`crate::durability::HELPERS`]), which wait on the disk, and one for each of the host's CPUs,
/// for the framings' packing, which wants them. A cell's job queued past them waits for one to be
/// done. Not measured yet against a bound of another size (board 2944's benches).
pub fn threads() -> usize {
    // The process's CPUs, the hardware's: the same for every simulated host.
    static THREADS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THREADS.get_or_init(|| {
        let cpus = std::thread::available_parallelism().map_or(1, |cpus| cpus.get());
        crate::durability::HELPERS + cpus
    })
}

/// What the pool's threads are named, which a trace of the host's syncs tells from the threads that
/// queued their jobs (TASK-369).
pub const NAME: &str = "store pool";

/// Runs `run` of `work` on one of the pool's threads, once one is free; or gives `work` back, if
/// the pool has none and the host refuses it one, as a thread of its own was refused before: for
/// work that must be done, which the caller then does itself (TASK-79.80).
pub(crate) fn spawn_or_return<T: Send + 'static>(
    work: T,
    run: impl FnOnce(T) + Send + 'static,
) -> Option<T> {
    // Under madsim, a task of the simulated host's, which the simulation runs at a later turn, as
    // it chooses: so the cell's waits for this work find it pending, and await it, as they do for
    // the pool's built normally, rather than finding it done, as when it ran on the caller's
    // thread (doc-24).
    #[cfg(madsim)]
    {
        drop(tokio::spawn(async move { host_fs::may_block(|| run(work)) }));
        None
    }
    #[cfg(not(madsim))]
    {
        let pool = pool();
        let mut state = pool.state.lock().unwrap();
        if !pool.place(&mut state, true) {
            drop(state);
            log::warn!("extent store: running on the caller's thread, the host refusing one");
            return Some(work);
        }
        state.queue.push_back(Box::new(move || run(work)));
        None
    }
}

/// Runs `mine` on the calling thread, and `work` beside it on as many as `most` of the pool's
/// threads, those free now or that it may start: what `mine` returned, and what each of theirs
/// did, once each has. One not begun once `mine` returns is let go, never run: so the caller never
/// waits for a thread busy with another job, and a job of the pool's may call this. A panic of
/// theirs is the caller's, once each is done.
pub(crate) fn help<M, R: Send + 'static>(
    most: usize,
    work: &(dyn Fn() -> R + Sync),
    mine: impl FnOnce() -> M,
) -> (M, Vec<R>) {
    #[cfg(madsim)]
    {
        let _ = (most, work);
        (mine(), Vec::new())
    }
    #[cfg(not(madsim))]
    {
        let call = Arc::new(Call { slots: Mutex::new(Vec::new()), done: Condvar::new() });
        let work: *const (dyn Fn() -> R + Sync + '_) = work;
        // SAFETY: a helper runs `work` only while its slot says it runs, and this returns, or
        // unwinds, only once no slot does (`Helping`'s `finish` and drop): `work` outlives each
        // use.
        let work = Work(unsafe {
            std::mem::transmute::<
                *const (dyn Fn() -> R + Sync + '_),
                *const (dyn Fn() -> R + Sync + 'static),
            >(work)
        });
        let helping = Helping(call.clone());
        {
            let pool = pool();
            let mut state = pool.state.lock().unwrap();
            let mut slots = call.slots.lock().unwrap();
            while slots.len() < most && pool.place(&mut state, false) {
                let (index, call, work) = (slots.len(), call.clone(), Work(work.0));
                slots.push(Slot::Offered);
                state.queue.push_back(Box::new(move || call.run(index, work)));
            }
        }
        let mine = mine();
        (mine, helping.finish())
    }
}

/// A job queued.
#[cfg(not(madsim))]
type Job = Box<dyn FnOnce() + Send>;

#[cfg(not(madsim))]
struct Pool {
    state: Mutex<State>,
    /// Wakes a thread waiting for a job.
    ready: Condvar,
}

#[cfg(not(madsim))]
struct State {
    queue: VecDeque<Job>,
    /// Those started, running.
    threads: usize,
    /// Those of them waiting for a job, each to take the next queued once woken.
    idle: usize,
}

/// The process's pool; with the `shuttle` feature, each Shuttle execution's own, so that its
/// threads are of its own schedule alone (as `durability::helping`'s count is, TASK-221).
#[cfg(not(madsim))]
fn pool() -> &'static Pool {
    #[cfg(not(feature = "shuttle"))]
    {
        // The process's worker threads; under madsim the caller does the work and none start.
        static POOL: Pool = Pool {
            state: Mutex::new(State { queue: VecDeque::new(), threads: 0, idle: 0 }),
            ready: Condvar::new(),
        };
        &POOL
    }
    #[cfg(feature = "shuttle")]
    {
        shuttle::lazy_static! {
            static ref POOL: Pool = Pool {
                state: Mutex::new(State { queue: VecDeque::new(), threads: 0, idle: 0 }),
                ready: Condvar::new(),
            };
        }
        &POOL
    }
}

#[cfg(not(madsim))]
impl Pool {
    /// Whether a job queued now, after those queued, is taken at once: by a thread waiting and not
    /// yet woken for one queued before it, woken now; or by one started for it, if there are fewer
    /// than [`threads`] and the host gives one. Or, `waiting`, once a thread busy now is done.
    fn place(&'static self, state: &mut State, waiting: bool) -> bool {
        if state.queue.len() < state.idle {
            self.ready.notify_one();
            return true;
        }
        if state.threads < threads() {
            match thread::Builder::new().name(NAME.into()).spawn(move || self.take_jobs()) {
                Ok(_) => {
                    state.threads += 1;
                    return true;
                }
                Err(error) => {
                    log::warn!("extent store: the pool's threads stay {}: {error}", state.threads);
                }
            }
        }
        waiting && state.threads > 0
    }

    /// A thread of the pool's: takes the jobs queued, one after another, waiting for one while none
    /// is; under Shuttle, ending then.
    fn take_jobs(&'static self) {
        let panicking = Panicking(self);
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(job) = state.queue.pop_front() {
                drop(state);
                job();
                state = self.state.lock().unwrap();
                continue;
            }
            // One fewer as the lock that says none is queued is held still: a job queued after
            // counts on the others alone.
            if cfg!(feature = "shuttle") {
                state.threads -= 1;
                // Counted out: Shuttle's teardown may unwind the thread from the unlock, as it
                // yields, which `Panicking` would count out again (board 9543).
                std::mem::forget(panicking);
                return;
            }
            state.idle += 1;
            state = self.ready.wait(state).unwrap();
            state.idle -= 1;
        }
    }
}

/// A thread of the pool's ending as a cell's job of its panicked, as a thread of the cell's own
/// did: one fewer, and another started for the jobs queued, if any, so that they don't wait for
/// good.
#[cfg(not(madsim))]
struct Panicking(&'static Pool);

#[cfg(not(madsim))]
impl Drop for Panicking {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let mut state = self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.threads -= 1;
        if !state.queue.is_empty() {
            self.0.place(&mut state, false);
        }
    }
}

/// A [`help`]'s call: what each helper offered did.
#[cfg(not(madsim))]
struct Call<R> {
    slots: Mutex<Vec<Slot<R>>>,
    /// Said as a helper is done.
    done: Condvar,
}

#[cfg(not(madsim))]
enum Slot<R> {
    /// Queued, not begun.
    Offered,
    Running,
    Done(std::thread::Result<R>),
    /// Let go, as its caller's own run ended before it began.
    LetGo,
}

/// What a helper runs, its caller's.
#[cfg(not(madsim))]
struct Work<R>(*const (dyn Fn() -> R + Sync + 'static));

// SAFETY: what it points to is `Sync`, and used only while its caller waits (`help`).
#[cfg(not(madsim))]
unsafe impl<R> Send for Work<R> {}

#[cfg(not(madsim))]
impl<R> Call<R> {
    /// Helper `index`'s run of `work`, unless it was let go.
    fn run(&self, index: usize, work: Work<R>) {
        {
            let mut slots = self.slots.lock().unwrap();
            // Let go, or taken out by its caller's return, which let it go.
            if !matches!(slots.get(index), Some(Slot::Offered)) {
                return;
            }
            slots[index] = Slot::Running;
        }
        // SAFETY: running, as its slot says, which its caller waits for (`help`).
        let done = catch_unwind(AssertUnwindSafe(|| unsafe { (*work.0)() }));
        self.slots.lock().unwrap()[index] = Slot::Done(done);
        self.done.notify_all();
    }
}

/// A [`help`]'s helpers, offered: waited for as it returns, or unwinds.
#[cfg(not(madsim))]
struct Helping<R>(Arc<Call<R>>);

#[cfg(not(madsim))]
impl<R> Helping<R> {
    /// What the helpers that ran returned, in their order, once each has; those not begun let go.
    fn finish(self) -> Vec<R> {
        let done = std::mem::take(&mut *self.wait());
        let mut returned = Vec::with_capacity(done.len());
        for slot in done {
            match slot {
                Slot::Done(Ok(value)) => returned.push(value),
                Slot::Done(Err(panic)) => resume_unwind(panic),
                Slot::LetGo => {}
                Slot::Offered | Slot::Running => unreachable!("a helper not waited for"),
            }
        }
        returned
    }

    /// Lets go of the helpers not begun, and waits for those running.
    fn wait(&self) -> MutexGuard<'_, Vec<Slot<R>>> {
        let mut slots = self.0.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in slots.iter_mut() {
            if matches!(slot, Slot::Offered) {
                *slot = Slot::LetGo;
            }
        }
        while slots.iter().any(|slot| matches!(slot, Slot::Running)) {
            slots = self.0.done.wait(slots).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        slots
    }
}

#[cfg(not(madsim))]
impl<R> Drop for Helping<R> {
    fn drop(&mut self) {
        drop(self.wait());
    }
}

#[cfg(all(test, not(madsim), not(feature = "shuttle")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Helpers run beside the caller, each what it was given, and no more than were asked for: the
    /// caller's own run first.
    #[test]
    fn helpers_run_beside_their_caller() {
        let runs = AtomicUsize::new(0);
        let work = || runs.fetch_add(1, Ordering::Relaxed);
        let (mine, theirs) = help(3, &work, || "mine");
        assert_eq!(mine, "mine");
        assert!(theirs.len() <= 3, "{theirs:?}");
        assert_eq!(runs.load(Ordering::Relaxed), theirs.len());
    }

    /// A pool all of whose threads are busy leaves its caller to do the work alone, at once: it
    /// waits for none of them.
    #[test]
    fn a_busy_pool_leaves_its_caller_to_do_the_work() {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(std::sync::Mutex::new(released));
        let started = Arc::new(std::sync::Barrier::new(threads() + 1));
        for _ in 0..threads() {
            let (released, started) = (released.clone(), started.clone());
            let refused = spawn_or_return((), move |()| {
                started.wait();
                let _ = released.lock().unwrap().recv();
            });
            assert!(refused.is_none());
        }
        started.wait();
        let runs = AtomicUsize::new(0);
        let work = || runs.fetch_add(1, Ordering::Relaxed);
        let (mine, theirs) = help(2, &work, || "mine");
        assert_eq!((mine, theirs.len(), runs.load(Ordering::Relaxed)), ("mine", 0, 0));
        drop(release);
    }

    /// A helper's panic is its caller's, once its other helpers are done.
    #[test]
    fn a_helpers_panic_is_its_callers() {
        let (begun, beginning) = std::sync::mpsc::channel();
        let work = || {
            begun.send(()).unwrap();
            panic!("a helper's")
        };
        let caught =
            catch_unwind(AssertUnwindSafe(|| help(1, &work, || beginning.recv().unwrap())));
        let panic = caught.expect_err("the helper's panic, the caller's");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"a helper's"));
    }

    /// The pool's threads are [`threads`] at most however many jobs are queued at once, which each
    /// run once; and stay for the jobs after.
    #[test]
    fn jobs_beyond_its_threads_wait_for_one() {
        let jobs = 4 * threads();
        let (done, finished) = std::sync::mpsc::channel();
        for job in 0..jobs {
            let done = done.clone();
            let refused = spawn_or_return(job, move |job| {
                std::thread::sleep(std::time::Duration::from_millis(1));
                done.send(job).unwrap();
            });
            assert!(refused.is_none());
        }
        let mut ran: Vec<_> = (0..jobs).map(|_| finished.recv().unwrap()).collect();
        ran.sort();
        assert_eq!(ran, (0..jobs).collect::<Vec<_>>());
        let state = pool().state.lock().unwrap();
        assert!(state.threads <= threads(), "{} threads", state.threads);
        assert!(state.threads > 0);
    }
}
