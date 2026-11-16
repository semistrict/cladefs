// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a store does once a panic went through it: it is halted (TASK-153.16). It serves
//! nothing more, each request failing with `EIO`, rather than panicking at its locks' poison, as
//! every request after the first did in TASK-104, TASK-105 and TASK-150.20; and nothing reads what
//! the panic left half done. Its guest's filesystem meets `EIO` and stops, its VM with it. The
//! connection's end closes it as a writer's that went with writes uncommitted, and the next to
//! open it has it as of its last commit: a store is usable again opened anew, as after a crash.
//!
//! A store's locks are [`StoreMutex`]es and [`StoreRwLock`]s, locked only through its [`Halt`]:
//! `EIO` once it is halted, and a guard ([`StoreGuard`]) dropped as a panic unwinds halts it. So no
//! lock of a store's is held across a panic without halting it, and none is locked poisoned. What
//! panics beneath a store, past its locks, the extent daemon halts it for, as it answers the
//! request that panicked.
//!
//! cladefs: a store is halted too as its host is told its tenant's KEK is revoked or destroyed, or
//! its cell shredded ([`halt_stores`], doc-16, Shredding; TASK-411.7): each request failing with
//! `EACCES` then, as a store of such a tenant or cell refuses to open. So a host finds the stores to
//! halt by their branches: every halt of the process is listed, weakly, with its store's.
//!
//! A halted store is not fenced: fencing is a later taking's cutting a writer off (`Error::Fenced`).
//!
//! A lock shared beyond one store, which no halt contains, as the process's cache of extents, a
//! window's, a connection's, or `extent_store`'s store's, which its followers share with its writer,
//! is an `extent_store::shared::SharedLock`: a panic holding one stops the daemon (TASK-149.24).

use crate::Branch;
use crate::error::{Errno, Result};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LockResult, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};

/// Whether a store is halted, the errno its requests fail with, and why: by the first panic to go
/// through it, or the first shredding of its tenant or cell its host is told of.
pub(crate) struct Halt(Arc<HaltState>);

struct HaltState {
    /// The store's path, which its log names.
    store: PathBuf,
    why: OnceLock<(Errno, String)>,
}

/// cladefs: the halts of the stores open in the process, each with its store's branch, held weakly,
/// so that a host told to forget a tenant's or a cell's keys halts theirs (TASK-411.7).
static HALTS: Mutex<Vec<(Branch, Weak<HaltState>)>> = Mutex::new(Vec::new());

impl Halt {
    /// The halt of the store at `store`, of `branch`, not halted: listed among the process's.
    pub(crate) fn new(store: &Path, branch: Branch) -> Self {
        let state = Arc::new(HaltState { store: store.to_owned(), why: OnceLock::new() });
        let mut halts = HALTS.lock().unwrap();
        halts.retain(|(_, halt)| halt.strong_count() > 0);
        halts.push((branch, Arc::downgrade(&state)));
        Self(state)
    }

    /// Halts the store, as `why` says, unless it is already: logged once. Its requests fail with
    /// `EIO`.
    pub(crate) fn halt(&self, why: impl FnOnce() -> String) {
        self.0.halt(Errno::EIO, why);
    }

    /// Whether the store is halted.
    pub(crate) fn is_halted(&self) -> bool {
        self.0.why.get().is_some()
    }

    /// The errno its requests fail with, if the store is halted.
    pub(crate) fn errno(&self) -> Option<Errno> {
        self.0.why.get().map(|(errno, _)| *errno)
    }

    /// The errno its requests fail with, if the store is halted: `EIO`, or `EACCES` for a store
    /// whose tenant's or cell's keys its host forgot.
    pub(crate) fn check(&self) -> Result<()> {
        self.errno().map_or(Ok(()), Err)
    }

    /// `locked`, the guard of the store's lock called `lock`, held: `EIO`, the lock let go of, if
    /// the store is halted, as it may have been while this waited for it; or if the lock is
    /// poisoned, which halts it, though no lock of a store's is poisoned unhalted.
    fn guard<G>(&self, lock: &'static str, locked: LockResult<G>) -> Result<StoreGuard<'_, G>> {
        let guard = locked.map_err(|_| {
            self.halt(|| format!("its {lock} lock found poisoned"));
            Errno::EIO
        })?;
        self.check()?;
        let guard = StoreGuard { guard, halt: self, lock };
        #[cfg(test)]
        tests::panic_if_asked(lock);
        Ok(guard)
    }
}

impl HaltState {
    /// Halts the store, its requests failing with `errno`, as `why` says, unless it is already:
    /// logged once. Returns whether this halted it.
    fn halt(&self, errno: Errno, why: impl FnOnce() -> String) -> bool {
        let mut said = None;
        self.why.get_or_init(|| (errno, said.insert(why()).clone()));
        let Some(why) = said else { return false };
        log::error!(
            "cladefs: the store at {} is halted, {why}: it serves nothing more, each request failing \
             with {errno:?}, until it is opened anew, as of its last commit",
            self.store.display()
        );
        true
    }
}

/// cladefs: halts every store open in the process whose branch `scope` says is of it, its requests
/// failing with `errno` from then on, as `why` says: what a host told a tenant's KEK is revoked or
/// destroyed, or a cell shredded, does to their stores (TASK-411.7), which it can't open again
/// either. Returns how many it halted, those halted already left out.
pub(crate) fn halt_stores(scope: impl Fn(Branch) -> bool, errno: Errno, why: &str) -> usize {
    let halts: Vec<Arc<HaltState>> = (HALTS.lock().unwrap().iter())
        .filter(|(branch, _)| scope(*branch))
        .filter_map(|(_, halt)| halt.upgrade())
        .collect();
    halts.iter().filter(|halt| halt.halt(errno, || why.to_owned())).count()
}

/// A guard of a store's lock: which halts the store if a panic drops it, holding the lock.
pub(crate) struct StoreGuard<'a, G> {
    guard: G,
    halt: &'a Halt,
    lock: &'static str,
}

impl<G: Deref> Deref for StoreGuard<'_, G> {
    type Target = G::Target;

    fn deref(&self) -> &G::Target {
        &self.guard
    }
}

impl<G: DerefMut> DerefMut for StoreGuard<'_, G> {
    fn deref_mut(&mut self) -> &mut G::Target {
        &mut self.guard
    }
}

// Before the guard is dropped, which lets the lock go: whoever takes it next finds the store halted.
impl<G> Drop for StoreGuard<'_, G> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.halt.halt(|| format!("a panic holding its {} lock", self.lock));
        }
    }
}

/// A mutex of a store's, locked only through its halt ([`StoreMutex::lock`]).
pub(crate) struct StoreMutex<T> {
    /// What the store's log calls it.
    name: &'static str,
    lock: Mutex<T>,
}

impl<T> StoreMutex<T> {
    pub(crate) const fn new(name: &'static str, value: T) -> Self {
        Self { name, lock: Mutex::new(value) }
    }

    /// Locks it, for the store `halt` halts: `EIO` if it is halted.
    pub(crate) fn lock<'a>(&'a self, halt: &'a Halt) -> Result<StoreGuard<'a, MutexGuard<'a, T>>> {
        halt.guard(self.name, self.lock.lock())
    }
}

/// A reader-writer lock of a store's, locked only through its halt, as a [`StoreMutex`] is.
pub(crate) struct StoreRwLock<T> {
    /// What the store's log calls it.
    name: &'static str,
    lock: RwLock<T>,
}

impl<T> StoreRwLock<T> {
    pub(crate) const fn new(name: &'static str, value: T) -> Self {
        Self { name, lock: RwLock::new(value) }
    }

    /// Locks it shared, for the store `halt` halts: `EIO` if it is halted. A panic holding it
    /// shared halts the store too, which poisons no reader-writer lock.
    pub(crate) fn read<'a>(
        &'a self,
        halt: &'a Halt,
    ) -> Result<StoreGuard<'a, RwLockReadGuard<'a, T>>> {
        halt.guard(self.name, self.lock.read())
    }

    /// Locks it alone, for the store `halt` halts: `EIO` if it is halted.
    pub(crate) fn write<'a>(
        &'a self,
        halt: &'a Halt,
    ) -> Result<StoreGuard<'a, RwLockWriteGuard<'a, T>>> {
        halt.guard(self.name, self.lock.write())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The store's lock a test asks to be panicked holding, once: as it is next taken.
    static PANIC_HOLDING: Mutex<Option<&'static str>> = Mutex::new(None);

    /// Panics holding the store's lock called `lock` as it is next taken, by any store: a bug's
    /// panic under it, as a lazily made resource's `expect` was in TASK-104 and TASK-150.20.
    pub(crate) fn panic_holding(lock: &'static str) {
        *PANIC_HOLDING.lock().unwrap() = Some(lock);
    }

    pub(super) fn panic_if_asked(lock: &'static str) {
        let mut asked = PANIC_HOLDING.lock().unwrap();
        if *asked == Some(lock) {
            *asked = None;
            drop(asked);
            panic!("a panic holding a store's {lock} lock, as a test asks");
        }
    }

    /// A panic holding a store's lock halts the store: that lock and every other of the store's
    /// refuse with `EIO` from then on, once let go of, rather than panic at its poison or let the
    /// store be read past it; another store's are untouched.
    #[test]
    fn a_panic_holding_a_lock_halts_its_store() {
        let branch = crate::host_store::new_alone();
        let (halt, other) =
            (Halt::new(Path::new("store"), branch), Halt::new(Path::new("other"), branch));
        let (lock, rw) = (StoreMutex::new("first", 1), StoreRwLock::new("second", 2));
        let panicked = std::panic::catch_unwind(|| {
            let mut guard = lock.lock(&halt).unwrap();
            *guard += 1;
            panic!("a bug, holding the lock");
        });
        assert!(panicked.is_err());
        assert!(halt.is_halted());
        assert_eq!(lock.lock(&halt).err(), Some(Errno::EIO));
        assert_eq!(rw.read(&halt).err(), Some(Errno::EIO));
        assert_eq!(rw.write(&halt).err(), Some(Errno::EIO));
        assert_eq!(halt.check(), Err(Errno::EIO));
        let unhalted = StoreMutex::new("third", 3);
        assert_eq!(*unhalted.lock(&other).unwrap(), 3);
        assert_eq!(other.check(), Ok(()));
    }

    /// A panic holding a store's reader-writer lock shared, which poisons no lock, halts the store
    /// too.
    #[test]
    fn a_panic_holding_a_lock_shared_halts_its_store() {
        let halt = Halt::new(Path::new("store"), crate::host_store::new_alone());
        let rw = StoreRwLock::new("shared", ());
        let panicked = std::panic::catch_unwind(|| {
            let _guard = rw.read(&halt).unwrap();
            panic!("a bug, holding the lock shared");
        });
        assert!(panicked.is_err());
        assert_eq!(rw.write(&halt).err(), Some(Errno::EIO));
    }

    /// A store's lock taken without a panic, and let go of, leaves it unhalted, however often.
    #[test]
    fn locks_let_go_of_leave_their_store_unhalted() {
        let halt = Halt::new(Path::new("store"), crate::host_store::new_alone());
        let lock = StoreMutex::new("lock", 0);
        for _ in 0..3 {
            *lock.lock(&halt).unwrap() += 1;
        }
        assert_eq!(*lock.lock(&halt).unwrap(), 3);
        assert!(!halt.is_halted());
    }
}
