// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: locks shared beyond one store, whose poison no halt contains (TASK-149.24). A store's
//! own locks halt the store a panic holding one went through (cladefs's `halt`): it fails each
//! request after with `EIO`, the others serve on. A lock shared beyond it, as the process's cache
//! of extents, a store's state, which its followers share with its writer, or a window's, which its
//! connection and the guest's faults share, would be found poisoned by every user after, one panic
//! each, or read half changed. So the extent daemon stops instead: a panic on a thread holding one
//! of these aborts the process, at the panic, before anything unwinds, once [`abort_on_panics`]
//! asks, as the daemon does as it starts. Its VMs' stores open anew as of their last commits, as
//! after a crash, once systemd has it running again.
//!
//! Such a lock is a [`SharedLock`], locked only through it ([`SharedLock::with`]), which has its
//! guard a [`Shared`]: the thread's locks held are what the panic hook looks at. A thread whose
//! work is the process's, as the page cache's flusher, which every store's files wait on, holds
//! that work as it would such a lock, for as long as it does it ([`Holding::work`]): its death too
//! no halt contains (TASK-493). Elsewhere, as in tests, a panic holding one unwinds as any panic
//! does.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Once};

thread_local! {
    /// The shared locks this thread holds, by name, as it took them.
    static HELD: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// Makes the calling thread's list of the shared locks it holds, room for as many as its first
/// lock would make: what a pool's threads call as they start ([`tokio::runtime::Builder`]'s
/// `on_thread_start`), so that the first lock a thread takes, whenever that is, allocates nothing,
/// and a heap counted to the byte holds the same whichever of the pool's threads took locks.
pub fn prepare_thread() {
    HELD.with(|held| held.borrow_mut().reserve(4));
}

/// The shared locks the calling thread holds, by name: those it took, and those awaited the task it
/// polls holds ([`Awaited`]).
pub fn held() -> Vec<&'static str> {
    let mut held = HELD.with(|held| held.borrow().clone());
    let holder = Holder::current();
    let awaited = AWAITED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    held.extend(awaited.iter().filter(|(by, _)| *by == holder).map(|(_, name)| *name));
    held
}

/// cladefs: who holds a shared lock taken by awaiting it, which it holds across its awaits
/// (board 2944): the task, which any of its runtime's threads may poll from one await to the next;
/// or, polled by no tokio task's, the thread, as a store's drop's close waits on it. Under madsim,
/// whose tasks name no ids, the simulation's one thread.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Holder {
    #[cfg(not(madsim))]
    Task(tokio::task::Id),
    Thread(std::thread::ThreadId),
}

impl Holder {
    fn current() -> Self {
        #[cfg(not(madsim))]
        if let Some(task) = tokio::task::try_id() {
            return Self::Task(task);
        }
        Self::Thread(std::thread::current().id())
    }
}

/// The shared locks awaited, by their holders ([`Awaited`]).
static AWAITED: std::sync::Mutex<Vec<(Holder, &'static str)>> = std::sync::Mutex::new(Vec::new());

/// cladefs: a holding of a shared lock taken by awaiting it, as a store's commit's and flush's are
/// (`AsyncMutex`, board 2944), by its holder: what the panic hook sees held by the task it panics
/// in, wherever it was polled, from its making to its drop. A thread's [`Holding`] cannot say so,
/// as the task moves between threads, holding it, at its awaits.
pub struct Awaited {
    holder: Holder,
    name: &'static str,
}

impl Awaited {
    pub fn new(name: &'static str) -> Self {
        let holder = Holder::current();
        AWAITED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((holder, name));
        let awaited = Self { holder, name };
        #[cfg(any(test, feature = "testing"))]
        testing::panic_if_asked(name);
        awaited
    }
}

impl Drop for Awaited {
    fn drop(&mut self) {
        let mut awaited = AWAITED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let held = (self.holder, self.name);
        if let Some(at) = awaited.iter().rposition(|each| *each == held) {
            awaited.remove(at);
        }
    }
}

/// What the process says as it aborts, before the locks held, in parentheses, by name.
pub const ABORTING: &str = "a panic holding the process's shared locks";

/// Whether `said`, a process's output, says it aborted for a panic holding `lock`, among others.
pub fn aborted_holding(said: &str, lock: &str) -> bool {
    said.lines().any(|line| {
        line.split_once(&format!("{ABORTING} ("))
            .and_then(|(_, locks)| locks.strip_suffix("): aborting"))
            .is_some_and(|locks| locks.split(", ").any(|held| held == lock))
    })
}

/// Has a panic on a thread holding a shared lock abort the process, once the panic is said, as the
/// hook before this one says it: the process's log names the locks, and nothing unwinds past them.
/// Once a process; the extent daemon asks as it starts.
pub fn abort_on_panics() {
    static HOOKED: Once = Once::new();
    HOOKED.call_once(|| {
        let said = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let held = held();
            said(info);
            if !held.is_empty() {
                let locks = held.join(", ");
                log::error!(
                    "cladefs: {ABORTING} ({locks}), shared beyond one store: aborting, as no halt \
                     contains it (TASK-149.24)"
                );
                // As the log may not reach stderr, nor be flushed before the abort.
                eprintln!("cladefs: {ABORTING} ({locks}): aborting");
                std::process::abort();
            }
        }));
    });
}

/// A lock shared beyond one store (see the module docs): `L`, a mutex, locked only through
/// [`SharedLock::with`].
pub struct SharedLock<L> {
    /// What the log calls it.
    name: &'static str,
    lock: L,
}

impl<L> SharedLock<L> {
    pub const fn new(name: &'static str, lock: L) -> Self {
        Self { name, lock }
    }

    /// The lock itself, which `&mut` has alone: as a drop takes what it guards, locking nothing.
    pub fn get_mut(&mut self) -> &mut L {
        &mut self.lock
    }

    /// The lock, locked as `lock` locks it, such as `Mutex::lock`: held, it is the thread's. A lock
    /// found poisoned, which none is in the extent daemon, panics.
    pub fn with<'a, G>(&'a self, lock: impl FnOnce(&'a L) -> LockResult<G>) -> Shared<G> {
        let name = self.name;
        let guard = lock(&self.lock).unwrap_or_else(|_| {
            panic!("the process's {name} lock, shared beyond one store, is poisoned")
        });
        Shared::new(name, guard)
    }
}

/// The calling thread's holding of a shared lock, or of the process's work, by name: what the
/// panic hook sees it hold, from its making to its drop, on the thread that made it alone.
pub struct Holding {
    name: &'static str,
    _thread: PhantomData<*const ()>,
}

impl Holding {
    fn new(name: &'static str) -> Self {
        HELD.with(|held| held.borrow_mut().push(name));
        let holding = Self { name, _thread: PhantomData };
        #[cfg(any(test, feature = "testing"))]
        testing::panic_if_asked(name);
        holding
    }

    /// The calling thread doing the process's work called `name`, until this drops: work every
    /// store waits on, as the page cache's flusher's, whose death no store's halt contains, so
    /// that a panic doing it aborts the process as one holding a [`SharedLock`] does.
    pub fn work(name: &'static str) -> Self {
        Self::new(name)
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(at) = held.iter().rposition(|name| *name == self.name) {
                held.remove(at);
            }
        });
    }
}

/// The guard of a [`SharedLock`], `G`, held by the thread that locked it: not sent to another.
pub struct Shared<G> {
    /// None only as it is let go of ([`Shared::unlocked`]).
    guard: Option<G>,
    /// Dropped after the guard: the lock is let go of before the thread says it holds it no more.
    holding: Holding,
}

impl<G> Shared<G> {
    fn new(name: &'static str, guard: G) -> Self {
        Self { guard: Some(guard), holding: Holding::new(name) }
    }

    /// Lets the lock go while `unlocked` runs with its guard, as a condition variable's wait does,
    /// returning it, and holds it again.
    pub fn unlocked(mut self, unlocked: impl FnOnce(G) -> G) -> Self {
        let guard = self.guard.take().expect("the lock is held");
        let name = self.holding.name;
        drop(self);
        Self::new(name, unlocked(guard))
    }
}

/// What the lock guards, as its guard has it.
impl<G: Deref> Deref for Shared<G> {
    type Target = G::Target;

    fn deref(&self) -> &G::Target {
        self.guard.as_ref().expect("the lock is held")
    }
}

impl<G: DerefMut> DerefMut for Shared<G> {
    fn deref_mut(&mut self) -> &mut G::Target {
        self.guard.as_mut().expect("the lock is held")
    }
}

impl<G> Drop for Shared<G> {
    fn drop(&mut self) {
        // The guard first, then its holding, a field, as this returns.
        drop(self.guard.take());
    }
}

/// Tests' panics holding a shared lock.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::Mutex;

    /// The shared lock a test asks to be panicked holding, once: as it is next taken.
    static PANIC_HOLDING: Mutex<Option<&'static str>> = Mutex::new(None);

    /// Panics holding the shared lock called `lock` as it is next taken, by any thread: a bug's
    /// panic under it.
    pub fn panic_holding(lock: &'static str) {
        *PANIC_HOLDING.lock().unwrap() = Some(lock);
    }

    pub(super) fn panic_if_asked(lock: &'static str) {
        let mut asked = PANIC_HOLDING.lock().unwrap();
        if *asked == Some(lock) {
            *asked = None;
            drop(asked);
            panic!("a panic holding the process's {lock} lock, as a test asks");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Condvar, Mutex};

    /// cladefs: a shared lock taken by awaiting it is its task's, which the panic hook sees it hold
    /// whichever thread polls the task, across its awaits, and not once its guard is dropped
    /// (board 2944): another task's, the same threads polling it, holds none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn awaited_locks_are_their_tasks_across_awaits() {
        let mutex = std::sync::Arc::new(crate::sync::AsyncMutex::new("awaited", ()));
        let guard = mutex.lock().await;
        let others = tokio::spawn(async {
            tokio::task::yield_now().await;
            held()
        });
        for _ in 0..32 {
            tokio::task::yield_now().await;
            assert_eq!(held(), ["awaited"]);
        }
        assert_eq!(others.await.unwrap(), Vec::<&str>::new());
        drop(guard);
        assert_eq!(held(), Vec::<&str>::new());
    }

    /// What [`child_panics`] does, as its parent says: `holding`, panic holding a shared lock;
    /// `let-go`, panic having let it go.
    const CHILD: &str = "EXTENT_STORE_SHARED_CHILD";

    /// Run by [`a_panic_holding_a_shared_lock_aborts`], in a process of its own: aborting on panics,
    /// panics as [`CHILD`] says, and catches the panic, which it says it did.
    #[test]
    #[ignore = "run by a_panic_holding_a_shared_lock_aborts, in a process of its own"]
    fn child_panics() {
        let Ok(what) = std::env::var(CHILD) else { return };
        abort_on_panics();
        let lock = SharedLock::new("test", Mutex::new(0));
        let caught = std::panic::catch_unwind(|| {
            let mut held = lock.with(Mutex::lock);
            *held += 1;
            if what == "let-go" {
                drop(held);
            }
            panic!("a bug");
        });
        assert!(caught.is_err());
        println!("CAUGHT");
    }

    /// The child's status and its output, stdout then stderr, doing `what`.
    fn child(what: &str) -> (std::process::ExitStatus, String) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "shared::tests::child_panics", "--ignored", "--nocapture"])
            .env(CHILD, what)
            .output()
            .unwrap();
        let said =
            String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
        (output.status, said.into_owned())
    }

    /// A panic holding a shared lock aborts the process, once it asks, at the panic, saying which
    /// lock: nothing unwinds to the catch. A panic having let it go unwinds as any does.
    #[test]
    fn a_panic_holding_a_shared_lock_aborts() {
        let (status, said) = child("holding");
        assert_eq!(status.signal(), Some(libc::SIGABRT), "{said}");
        assert!(aborted_holding(&said, "test"), "{said}");
        assert!(!said.contains("CAUGHT"), "{said}");
        let (status, said) = child("let-go");
        assert!(status.success(), "{said}");
        assert!(said.contains("CAUGHT"), "{said}");
        assert!(!said.contains("aborting"), "{said}");
    }

    /// A thread holds the shared locks it took, until it lets them go: a condition variable's wait
    /// lets go of its lock meanwhile; another thread's are its own.
    #[test]
    fn threads_hold_what_they_took() {
        let (first, second) = (
            SharedLock::new("first", Mutex::new(())),
            SharedLock::new("second", Mutex::new(false)),
        );
        let condvar = Condvar::new();
        let held = first.with(Mutex::lock);
        let mut ready = second.with(Mutex::lock);
        assert_eq!(super::held(), ["first", "second"]);
        let ready = std::thread::scope(|scope| {
            scope.spawn(|| {
                assert_eq!(super::held(), Vec::<&str>::new());
                *second.with(Mutex::lock) = true;
                condvar.notify_all();
            });
            while !*ready {
                ready = ready.unlocked(|guard| {
                    assert_eq!(super::held(), ["first"]);
                    condvar.wait(guard).unwrap()
                });
            }
            ready
        });
        assert_eq!(super::held(), ["first", "second"]);
        drop(held);
        assert_eq!(super::held(), ["second"]);
        drop(ready);
        assert_eq!(super::held(), Vec::<&str>::new());
    }
}
