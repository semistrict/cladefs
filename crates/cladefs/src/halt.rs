// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what an open cell does once a panic went through it: it is halted (TASK-153.16). It
//! serves nothing more, each request failing with `EIO`, rather than panicking at its locks'
//! poison, as every request after the first did in TASK-104, TASK-105 and TASK-150.20; and nothing
//! reads what the panic left half done. Its guest's filesystem meets `EIO` and stops, its VM with
//! it. The connection's end closes it, and the next to open it has it as of its last commit: an open cell is usable again opened anew, as after a
//! crash.
//!
//! An open cell's locks are [`CellMutex`]es and [`CellRwLock`]s, locked only through its [`Halt`]:
//! `EIO` once it is halted, and a guard ([`CellGuard`]) dropped as a panic unwinds halts it. So no
//! lock of an open cell's is held across a panic without halting it, and none is locked poisoned.
//! What panics beneath an open cell, past its locks, the extent daemon halts it for, as it answers
//! the request that panicked.
//!
//! cladefs: an open cell is halted too as its host is told its tenant's KEK is revoked or
//! destroyed, or its cell shredded ([`halt_cells`], doc-16, Shredding; TASK-411.7): each request
//! failing with `EACCES` then, as a cell of such a tenant or a shredded cell refuses to open. So a
//! host finds the open cells to halt by their cells: every halt of the process is listed, weakly,
//! with its open cell's.
//!
//! A halted open cell is not fenced: fencing is a later taking's cutting a writer off
//! (`Error::Fenced`).
//!
//! A lock shared beyond one open cell, which no halt contains, as the process's cache of extents, a
//! window's, a connection's, or `local_cell`'s open cell's, which the cells a base is attached to
//! share, is an `local_cell::shared::SharedLock`: a panic holding one stops the daemon
//! (TASK-149.24).

use crate::CellId;
use crate::error::{Errno, Result};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LockResult, Mutex, MutexGuard, OnceLock, Weak};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Whether an open cell is halted, the errno its requests fail with, and why: by the first panic to
/// go through it, or the first shredding of its tenant or cell its host is told of.
pub(crate) struct Halt(Arc<HaltState>);

struct HaltState {
    /// The open cell's path, which its log names.
    cell_dir: PathBuf,
    why: OnceLock<(Errno, String)>,
}

node_local::node_local! {
    /// cladefs: the halts of the open cells in the process, each with its cell, held weakly,
    /// so that a host told to forget a tenant's or a cell's keys halts theirs (TASK-411.7).
    static HALTS: Mutex<Vec<(CellId, Weak<HaltState>)>> = Mutex::new(Vec::new());
}

impl Halt {
    /// The halt of the open cell at `cell_dir`, of `cell`, not halted: listed among the process's.
    pub(crate) fn new(cell_dir: &Path, cell: CellId) -> Self {
        let state = Arc::new(HaltState { cell_dir: cell_dir.to_owned(), why: OnceLock::new() });
        let mut halts = HALTS.get().lock().unwrap();
        halts.retain(|(_, halt)| halt.strong_count() > 0);
        halts.push((cell, Arc::downgrade(&state)));
        Self(state)
    }

    /// Halts the open cell, as `why` says, unless it is already: logged once. Its requests fail
    /// with `EIO`.
    pub(crate) fn halt(&self, why: impl FnOnce() -> String) {
        self.0.halt(Errno::EIO, why);
    }

    /// Whether the open cell is halted.
    pub(crate) fn is_halted(&self) -> bool {
        self.0.why.get().is_some()
    }

    /// The errno its requests fail with, if the open cell is halted.
    pub(crate) fn errno(&self) -> Option<Errno> {
        self.0.why.get().map(|(errno, _)| *errno)
    }

    /// The errno its requests fail with, if the open cell is halted: `EIO`, or `EACCES` for an open
    /// cell whose tenant's or cell's keys its host forgot.
    pub(crate) fn check(&self) -> Result<()> {
        self.errno().map_or(Ok(()), Err)
    }

    /// `locked`, the guard of the open cell's lock called `lock`, held: `EIO`, the lock let go of,
    /// if the open cell is halted, as it may have been while this waited for it; or if the lock is
    /// poisoned, which halts it, though no lock of an open cell's is poisoned unhalted.
    fn guard<G>(&self, lock: &'static str, locked: LockResult<G>) -> Result<CellGuard<'_, G>> {
        let guard = locked.map_err(|_| {
            self.halt(|| format!("its {lock} lock found poisoned"));
            Errno::EIO
        })?;
        self.check()?;
        let guard = CellGuard { guard, halt: self, lock };
        #[cfg(test)]
        tests::panic_if_asked(lock);
        Ok(guard)
    }
}

impl HaltState {
    /// Halts the open cell, its requests failing with `errno`, as `why` says, unless it is already:
    /// logged once. Returns whether this halted it.
    fn halt(&self, errno: Errno, why: impl FnOnce() -> String) -> bool {
        let mut said = None;
        self.why.get_or_init(|| (errno, said.insert(why()).clone()));
        let Some(why) = said else { return false };
        log::error!(
            "cladefs: the open cell at {} is halted, {why}: it serves nothing more, each request \
             failing with {errno:?}, until it is opened anew, as of its last commit",
            self.cell_dir.display()
        );
        true
    }
}

/// cladefs: halts every open cell in the process whose cell `scope` says is of it, its requests
/// failing with `errno` from then on, as `why` says: what a host told a tenant's KEK is revoked or
/// destroyed, or a cell shredded, does to their open cells (TASK-411.7), which it can't open again
/// either. Returns how many it halted, those halted already left out.
pub(crate) fn halt_cells(scope: impl Fn(CellId) -> bool, errno: Errno, why: &str) -> usize {
    let halts: Vec<Arc<HaltState>> = (HALTS.get().lock().unwrap().iter())
        .filter(|(cell, _)| scope(*cell))
        .filter_map(|(_, halt)| halt.upgrade())
        .collect();
    halts.iter().filter(|halt| halt.halt(errno, || why.to_owned())).count()
}

/// A guard of an open cell's lock: which halts the open cell if a panic drops it, holding the lock.
pub(crate) struct CellGuard<'a, G> {
    guard: G,
    halt: &'a Halt,
    lock: &'static str,
}

impl<G: Deref> Deref for CellGuard<'_, G> {
    type Target = G::Target;

    fn deref(&self) -> &G::Target {
        &self.guard
    }
}

impl<G: DerefMut> DerefMut for CellGuard<'_, G> {
    fn deref_mut(&mut self) -> &mut G::Target {
        &mut self.guard
    }
}

// Before the guard is dropped, which lets the lock go: whoever takes it next finds the open cell
// halted.
impl<G> Drop for CellGuard<'_, G> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.halt.halt(|| format!("a panic holding its {} lock", self.lock));
        }
    }
}

/// A mutex of an open cell's, locked only through its halt ([`CellMutex::lock`]).
pub(crate) struct CellMutex<T> {
    /// What the open cell's log calls it.
    name: &'static str,
    lock: Mutex<T>,
}

impl<T> CellMutex<T> {
    pub(crate) const fn new(name: &'static str, value: T) -> Self {
        Self { name, lock: Mutex::new(value) }
    }

    /// Locks it, for the open cell `halt` halts: `EIO` if it is halted.
    pub(crate) fn lock<'a>(&'a self, halt: &'a Halt) -> Result<CellGuard<'a, MutexGuard<'a, T>>> {
        halt.guard(self.name, self.lock.lock())
    }
}

/// A reader-writer lock of an open cell's, locked only through its halt, as a [`CellMutex`] is.
/// cladefs: awaited, holding no thread while another holds it, and held across the open cell's own
/// awaits, as its holders' work waits for the open cell's (board 2946): tokio's, whose guards a
/// task sends; poisoned never, a panic holding it halting the open cell as its guard drops.
pub(crate) struct CellRwLock<T> {
    /// What the open cell's log calls it.
    name: &'static str,
    lock: RwLock<T>,
}

impl<T> CellRwLock<T> {
    pub(crate) fn new(name: &'static str, value: T) -> Self {
        Self { name, lock: RwLock::new(value) }
    }

    /// Locks it shared, for the open cell `halt` halts: `EIO` if it is halted. A panic holding it
    /// shared halts the open cell too, which poisons no reader-writer lock.
    pub(crate) async fn read<'a>(
        &'a self,
        halt: &'a Halt,
    ) -> Result<CellGuard<'a, RwLockReadGuard<'a, T>>> {
        halt.guard(self.name, Ok(self.lock.read().await))
    }

    /// Locks it alone, for the open cell `halt` halts: `EIO` if it is halted.
    pub(crate) async fn write<'a>(
        &'a self,
        halt: &'a Halt,
    ) -> Result<CellGuard<'a, RwLockWriteGuard<'a, T>>> {
        halt.guard(self.name, Ok(self.lock.write().await))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The open cell's lock a test asks to be panicked holding, once: as it is next taken.
    static PANIC_HOLDING: Mutex<Option<&'static str>> = Mutex::new(None);

    /// Panics holding the open cell's lock called `lock` as it is next taken, by any open cell: a
    /// bug's panic under it, as a lazily made resource's `expect` was in TASK-104 and TASK-150.20.
    pub(crate) fn panic_holding(lock: &'static str) {
        *PANIC_HOLDING.lock().unwrap() = Some(lock);
    }

    pub(super) fn panic_if_asked(lock: &'static str) {
        let mut asked = PANIC_HOLDING.lock().unwrap();
        if *asked == Some(lock) {
            *asked = None;
            drop(asked);
            panic!("a panic holding an open cell's {lock} lock, as a test asks");
        }
    }

    /// A panic holding an open cell's lock halts the open cell: that lock and every other of the
    /// open cell's refuse with `EIO` from then on, once let go of, rather than panic at its poison
    /// or let the open cell be read past it; another open cell's are untouched.
    #[test]
    fn a_panic_holding_a_lock_halts_its_open_cell() {
        let cell = crate::host_cell::new_alone();
        let (halt, other) =
            (Halt::new(Path::new("cell"), cell), Halt::new(Path::new("other"), cell));
        let (lock, rw) = (CellMutex::new("first", 1), CellRwLock::new("second", 2));
        let panicked = std::panic::catch_unwind(|| {
            let mut guard = lock.lock(&halt).unwrap();
            *guard += 1;
            panic!("a bug, holding the lock");
        });
        assert!(panicked.is_err());
        assert!(halt.is_halted());
        assert_eq!(lock.lock(&halt).err(), Some(Errno::EIO));
        assert_eq!(futures::executor::block_on(rw.read(&halt)).err(), Some(Errno::EIO));
        assert_eq!(futures::executor::block_on(rw.write(&halt)).err(), Some(Errno::EIO));
        assert_eq!(halt.check(), Err(Errno::EIO));
        let unhalted = CellMutex::new("third", 3);
        assert_eq!(*unhalted.lock(&other).unwrap(), 3);
        assert_eq!(other.check(), Ok(()));
    }

    /// A panic holding an open cell's reader-writer lock shared, which poisons no lock, halts the
    /// open cell too.
    #[test]
    fn a_panic_holding_a_lock_shared_halts_its_open_cell() {
        let halt = Halt::new(Path::new("cell"), crate::host_cell::new_alone());
        let rw = CellRwLock::new("shared", ());
        // Its guard a tokio lock's, which is no `RefUnwindSafe`: the halt is what it checks.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = futures::executor::block_on(rw.read(&halt)).unwrap();
            panic!("a bug, holding the lock shared");
        }));
        assert!(panicked.is_err());
        assert_eq!(futures::executor::block_on(rw.write(&halt)).err(), Some(Errno::EIO));
    }
}
