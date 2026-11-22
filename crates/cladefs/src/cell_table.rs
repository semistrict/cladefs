// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The extent daemon's cells (`crate::cladefsd`), by the keys that name them
//! (`cladefsd::cells::cell_dir_key`): one table, whose entry for a key says all there is of its
//! cell (TASK-160.8). Who holds it: a connection writing it, or the daemon working on it. And what
//! is open of it for others: the cell its writer's connection, or its followers', have open, or a
//! kept cell the daemon keeps open for forks. Five owners said so before, kept in step by hand: the
//! holders, the cells open for connections and whether retired, the cells kept, a connection's
//! own fields, and a cell's following of its writer's; which TASK-79.34, 150.5, 54, 79.84,
//! 79.95, 92.5, 79.103 and 79.36 came of.
//!
//! A cell has one holder at a time, whose hold ([`Hold`]) is a value that lets the cell go as it
//! is dropped: a connection's, from its `OPEN` until it is gone and its cell closed; the daemon's,
//! while it forks, syncs or resizes a cell no connection writes. A connection whose VM process is
//! gone, still holding its cell, lets it go to the next that asks, as a VM restored from a
//! snapshot at once opens its VM's cell (TASK-39.1). What is open of a cell outlives its holder:
//! a cell followers follow, read-only (backlog doc-8), as long as one has it, which a writer
//! holding it then takes over, unless its writer retired it, gone with writes uncommitted; a kept
//! cell kept open for forks, until a writer holds the cell, which closes it first (TASK-92.5). A
//! connection holds no kept cell: its entry ([`Entry::Writer`]) has no room for one.
//!
//! Generic over its connections, cells and kept cells, so that its tests run it with models of
//! them: under Shuttle (awslabs/shuttle), with the `shuttle` feature, whose scheduler chooses every
//! interleaving of the threads writing, following, retiring and keeping kept cells of a cell as its
//! writer's VM process goes (TASK-44.6).

use crate::Errno;
use event_listener::{Event, EventListener};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

#[cfg(feature = "shuttle")]
use shuttle::sync::Mutex;
#[cfg(not(feature = "shuttle"))]
use std::sync::Mutex;

/// The rules the table relies on, each of which its Shuttle tests show to be needed by turning it
/// off.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rules {
    /// A cell held by a holder that isn't going holds no other.
    pub one_holder: bool,
    /// Letting a cell go wakes those waiting to hold it.
    pub release_wakes: bool,
    /// What is open of a cell, the cell followers have open or a cell kept, stays as its holder
    /// lets the cell go.
    pub open_outlives_holders: bool,
    /// A cell retired stays so while followers have it.
    pub retirement: bool,
    /// A cell is marked retired before its followers are cut off.
    pub marks_before_cutting: bool,
    /// A follower made looks again for the cell retired, as it was made.
    pub followers_look_again: bool,
}

impl Rules {
    pub const ALL: Self = Self {
        one_holder: true,
        release_wakes: true,
        open_outlives_holders: true,
        retirement: true,
        marks_before_cutting: true,
        followers_look_again: true,
    };
}

#[cfg(not(feature = "shuttle"))]
fn rules() -> Rules {
    Rules::ALL
}

#[cfg(feature = "shuttle")]
static RULES: std::sync::Mutex<Rules> = std::sync::Mutex::new(Rules::ALL);

#[cfg(feature = "shuttle")]
fn rules() -> Rules {
    *RULES.lock().unwrap()
}

/// Sets the rules, for the Shuttle tests of what uses the table.
#[cfg(all(test, feature = "shuttle"))]
pub(crate) fn set_rules(rules: Rules) {
    *RULES.lock().unwrap() = rules;
}

/// `future`'s output, unless `wait` passes first (`None`). Under Shuttle, where time doesn't
/// pass, it waits for ever: what never wakes it is a deadlock, which Shuttle reports.
pub(crate) async fn within<F: std::future::Future>(wait: Duration, future: F) -> Option<F::Output> {
    #[cfg(feature = "shuttle")]
    {
        let _ = wait;
        Some(future.await)
    }
    #[cfg(not(feature = "shuttle"))]
    {
        // Timed out: none, as its callers ask.
        tokio::time::timeout(wait, future).await.ok()
    }
}

/// Waits for `released`, on this thread, until `deadline` (for ever, if none): whether it came.
/// Under Shuttle, where time doesn't pass, it waits for ever, as [`within`] does, the scheduler's
/// thread blocked on Shuttle's terms. cladefs: the table's tests' claims alone, on their threads
/// ([`CellTable::hold`]); the daemon's and the FUSE mount's await theirs ([`within`], board 2946).
#[cfg(test)]
fn wait_blocking(released: EventListener, deadline: Option<Instant>) -> bool {
    #[cfg(feature = "shuttle")]
    {
        let _ = deadline;
        shuttle::future::block_on(released);
        true
    }
    #[cfg(not(feature = "shuttle"))]
    {
        use event_listener::Listener as _;
        local_cell::blocks!("a hold of a cell, waiting for its holder on this thread");
        match deadline {
            None => {
                released.wait();
                true
            }
            Some(deadline) => released.wait_deadline(deadline).is_some(),
        }
    }
}

/// A connection writing a cell.
pub(crate) trait Writer: Send + Sync {
    /// Whether it is going: its VM process gone, it lets its cell go to the next that asks.
    fn going(&self) -> bool;
}

/// The cells, by their keys: an entry each, while anything holds a cell or has it open.
pub(crate) struct CellTable<C, S, P> {
    inner: Mutex<Inner<C, S, P>>,
}

struct Inner<C, S, P> {
    entries: HashMap<PathBuf, Entry<C, S, P>>,
    /// What says a cell is let go, to those waiting to hold it, on threads or not, by its key:
    /// woken as its holder lets it go, and none of another's, as every cell's were, at each
    /// cell's letting go (TASK-92.5). From the first that waits until then.
    waiting: HashMap<PathBuf, Event>,
}

/// A key's entry: who holds its cell, and what is open of it meanwhile.
enum Entry<C, S, P> {
    /// A connection writes it, from its `OPEN` on: its cell, once open, which followers may follow
    /// too; and what to call as a writer waits for it, going ([`Hold::when_wanted`]).
    Writer { connection: Weak<C>, cell: Instance<S>, wanted: Option<Wanted> },
    /// The daemon holds it, to work on it, over what is open of it, which stays.
    Daemon(Open<S, P>),
    /// None holds it: what is open of it, never [`Open::Nothing`].
    Unheld(Open<S, P>),
    /// cladefs: the daemon's FUSE mount has it attached, a cell it shows (TASK-475.3):
    /// nothing else holds it, nor waits for it, until the mount lets it go, as it detaches the
    /// cell. Nothing else is open of it.
    Mounted,
}

/// What is open of a cell, for others than its holder.
enum Open<S, P> {
    Nothing,
    /// The cell connections have open, its followers', its writer gone or never there.
    Followed(Instance<S>),
    /// A cell kept open for forks (TASK-92.5).
    Kept(P),
}

/// What a connection holding a cell has done as a writer waits for it to let the cell go, its
/// VM process gone (TASK-216): a callback of its own, which outlives the connection, as letting
/// the cell go comes after its last reference does.
pub(crate) type Wanted = Arc<dyn Fn() + Send + Sync>;

/// The cell connections have open, a writer's or followers', while one of them has it: retired
/// once its writer went with writes uncommitted, which neither a writer nor a follower opens then.
struct Instance<S> {
    cell: Weak<S>,
    retired: bool,
}

/// Who asks to hold a cell.
enum Claim<C> {
    /// The daemon, to work on it.
    Daemon,
    /// The daemon, unless a connection not going writes it, which the daemon then works through.
    DaemonOrWriter,
    /// A connection, to write it.
    Writer(Weak<C>),
    /// cladefs: the daemon's FUSE mount, to attach it (TASK-475.3).
    Mount,
}

/// What a claim got: the cell held, with the cell kept open of it that a writer's claim took
/// out, to close, and, past [`Rules::one_holder`], the connection holding it, which it passed:
/// dropped once the table's lock is, as a [`Refused`]'s; or the connection writing it.
enum Claimed<C, P> {
    Held { kept: Option<P>, _passed: Option<Arc<C>> },
    Writer(Arc<C>),
}

/// Why a claim failed, with the connection holding the cell, if one does: dropped once the
/// table's lock is, as a connection, dropped last, lets its cell go, which takes the lock.
enum Refused<C> {
    /// A connection not going writes it: `EBUSY`.
    Busy(Arc<C>),
    /// A connection going holds it, or the daemon, which lets it go soon; and what a going
    /// connection has called as a writer waits for it, if anything.
    Wait(Option<Arc<C>>, Option<Wanted>),
    /// cladefs: the mount has it attached, which it lets go only as asked to; or, to the mount,
    /// followers have it open: `EBUSY`, at once (TASK-475.3).
    Now,
}

/// A cell held, by a connection writing it or by the daemon working on it, which nothing else
/// holds until it is dropped: it lets the cell go then, and those waiting to hold it may. What is
/// open of the cell stays ([`Rules::open_outlives_holders`]): the cell its connection had open,
/// if followers have it still.
#[must_use = "a cell held is let go as its hold is dropped"]
pub(crate) struct Hold<'t, C, S, P> {
    table: &'t CellTable<C, S, P>,
    key: PathBuf,
}

/// A cell held, with the cell kept open of it that the claim took out, to close, if one was:
/// what a writer's claim, and the mount's, get.
pub(crate) type HeldTaking<'t, C, S, P> = (Hold<'t, C, S, P>, Option<P>);

/// A cell a connection writes, or held by the daemon ([`CellTable::writer_or_hold`]).
pub(crate) enum Holding<'t, C, S, P> {
    Writer(Arc<C>),
    Held(Hold<'t, C, S, P>),
}

impl<S> Instance<S> {
    /// No cell yet: a writer's, as it opens it.
    fn none() -> Self {
        Self { cell: Weak::new(), retired: false }
    }

    /// The cell, if one has it still, and whether retired.
    fn get(&self) -> Option<(Arc<S>, bool)> {
        Some((self.cell.upgrade()?, self.retired))
    }
}

impl<S, P> Open<S, P> {
    /// As it is, but a cell none has open any more, which is nothing.
    fn live(self) -> Self {
        match self {
            Open::Followed(instance) if instance.cell.strong_count() == 0 => Open::Nothing,
            open => open,
        }
    }

    fn is_nothing(&self) -> bool {
        matches!(self, Open::Nothing)
    }

    /// Whether nothing is open: nothing, or a cell none has open any more.
    fn is_closed(&self) -> bool {
        match self {
            Open::Nothing => true,
            Open::Followed(instance) => instance.cell.strong_count() == 0,
            Open::Kept(_) => false,
        }
    }
}

impl<C: Writer, S, P> Entry<C, S, P> {
    /// What a claim of the cell meets, if anything holds it: a connection not going, or a
    /// connection going, or the daemon, which let it go soon.
    fn holder(&self) -> Option<Refused<C>> {
        match self {
            Entry::Writer { connection, wanted, .. } => Some(match connection.upgrade() {
                Some(writer) if !writer.going() => Refused::Busy(writer),
                going => Refused::Wait(going, wanted.clone()),
            }),
            Entry::Daemon(_) => Some(Refused::Wait(None, None)),
            Entry::Unheld(_) => None,
            Entry::Mounted => Some(Refused::Now),
        }
    }
}

impl<C, S, P> Entry<C, S, P> {
    /// What is open of the cell, its holder gone: the cell a writer had open, followers' now.
    fn into_open(self) -> Open<S, P> {
        match self {
            Entry::Writer { cell, .. } => Open::Followed(cell).live(),
            Entry::Daemon(open) | Entry::Unheld(open) => open.live(),
            Entry::Mounted => Open::Nothing,
        }
    }

    /// The cell connections have open, its writer's or followers', if one is.
    fn instance(&mut self) -> Option<&mut Instance<S>> {
        match self {
            Entry::Writer { cell, .. }
            | Entry::Daemon(Open::Followed(cell))
            | Entry::Unheld(Open::Followed(cell)) => Some(cell),
            _ => None,
        }
    }

    /// The cell kept open of the cell, if one is.
    fn kept(&mut self) -> Option<&mut P> {
        match self {
            Entry::Daemon(Open::Kept(kept)) | Entry::Unheld(Open::Kept(kept)) => Some(kept),
            _ => None,
        }
    }

    /// Whether it is followers' cell, none having it any more: a stale entry.
    fn is_stale(&self) -> bool {
        matches!(self, Entry::Unheld(Open::Followed(instance)) if instance.cell.strong_count() == 0)
    }
}

impl<C: Writer, S, P> Inner<C, S, P> {
    /// Holds the cell `key` names for `claim`, if it may now: unless a connection not going
    /// writes it, which fails, or a connection going holds it, or the daemon, which is waited for.
    /// A writer takes over the cell followers have open, and the cell kept open, to close.
    fn claim(&mut self, key: &Path, claim: &Claim<C>) -> Result<Claimed<C, P>, Refused<C>> {
        let passed = match (self.entries.get(key).and_then(Entry::holder), claim) {
            (Some(Refused::Busy(writer)), Claim::DaemonOrWriter) => {
                return Ok(Claimed::Writer(writer));
            }
            (Some(Refused::Busy(writer)), _) if !rules().one_holder => Some(writer),
            (Some(Refused::Wait(going, _)), _) if !rules().one_holder => going,
            (Some(refused), _) => return Err(refused),
            (None, _) => None,
        };
        // cladefs: the mount opens a cell of its own, where followers have none open.
        let followed = |entry: &Entry<C, S, P>| match entry {
            Entry::Daemon(open) | Entry::Unheld(open) => {
                matches!(open, Open::Followed(_)) && !open.is_closed()
            }
            _ => false,
        };
        if matches!(claim, Claim::Mount) && self.entries.get(key).is_some_and(followed) {
            return Err(Refused::Now);
        }
        let open = self.entries.remove(key).map_or(Open::Nothing, Entry::into_open);
        let (entry, evicted) = match claim {
            Claim::Writer(connection) => {
                let connection = connection.clone();
                match open {
                    Open::Followed(cell) => {
                        (Entry::Writer { connection, cell, wanted: None }, None)
                    }
                    Open::Kept(kept) => {
                        let cell = Instance::none();
                        (Entry::Writer { connection, cell, wanted: None }, Some(kept))
                    }
                    Open::Nothing => {
                        let cell = Instance::none();
                        (Entry::Writer { connection, cell, wanted: None }, None)
                    }
                }
            }
            Claim::Daemon | Claim::DaemonOrWriter => (Entry::Daemon(open), None),
            Claim::Mount => match open {
                Open::Kept(kept) => (Entry::Mounted, Some(kept)),
                Open::Nothing | Open::Followed(_) => (Entry::Mounted, None),
            },
        };
        self.entries.insert(key.to_owned(), entry);
        Ok(Claimed::Held { kept: evicted, _passed: passed })
    }
}

impl<C, S, P> CellTable<C, S, P> {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { entries: HashMap::new(), waiting: HashMap::new() }) }
    }

    /// The connection writing the cell `key` names, if one does, going or not.
    pub fn writer(&self, key: &Path) -> Option<Arc<C>> {
        match self.inner.lock().unwrap().entries.get(key)? {
            Entry::Writer { connection, .. } => connection.upgrade(),
            _ => None,
        }
    }

    /// The cell connections have open at `key`, its writer's or followers', if one is, and
    /// whether it is retired.
    pub fn instance(&self, key: &Path) -> Option<(Arc<S>, bool)> {
        self.inner.lock().unwrap().entries.get_mut(key)?.instance()?.get()
    }

    /// A follower of the cell connections have open at `key`, as `follow` makes it, if one is:
    /// `EBUSY` if it is retired, or was retired as `follow` made it, which its writer's retirement
    /// may not have cut off ([`Hold::retire`]).
    pub fn follow<F>(
        &self,
        key: &Path,
        follow: impl FnOnce(&Arc<S>) -> Result<F, Errno>,
    ) -> Result<Option<F>, Errno> {
        let Some((cell, retired)) = self.instance(key) else { return Ok(None) };
        if retired {
            return Err(Errno::EBUSY);
        }
        let follower = follow(&cell)?;
        let retired = |(now, retired): (Arc<S>, bool)| retired && Arc::ptr_eq(&now, &cell);
        if rules().followers_look_again && self.instance(key).is_some_and(retired) {
            return Err(Errno::EBUSY);
        }
        Ok(Some(follower))
    }

    /// `f`'s of the cell kept open at `key`, if one is: under the table's lock, which `f` takes
    /// for no longer than to look at it.
    pub fn kept<R>(&self, key: &Path, f: impl FnOnce(&mut P) -> R) -> Option<R> {
        self.inner.lock().unwrap().entries.get_mut(key)?.kept().map(f)
    }

    /// The cells connections have open, writers' and followers', by their keys.
    pub fn instances(&self) -> Vec<(PathBuf, Arc<S>)> {
        let mut inner = self.inner.lock().unwrap();
        (inner.entries.iter_mut())
            .filter_map(|(key, entry)| Some((key.clone(), entry.instance()?.cell.upgrade()?)))
            .collect()
    }

    /// cladefs: the keys of the cells a kept cell is kept open of (TASK-141.4).
    pub fn kept_keys(&self) -> Vec<PathBuf> {
        let mut inner = self.inner.lock().unwrap();
        let kept = inner.entries.iter_mut().filter_map(|(key, entry)| entry.kept().map(|_| key));
        kept.cloned().collect()
    }

    /// How many cells connections write.
    pub fn writers(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        (inner.entries.values())
            .filter(|entry| matches!(entry, Entry::Writer { connection, .. } if connection.strong_count() > 0))
            .count()
    }

    /// Whether something waits to hold the cell `key` names.
    #[cfg(test)]
    pub fn is_waited_for(&self, key: &Path) -> bool {
        self.inner.lock().unwrap().waiting.contains_key(key)
    }

    /// Whether nothing holds a cell, nor has one open.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.entries.values().all(Entry::is_stale)
    }

    /// Lets the cell `key` names go, its hold dropped: what is open of it stays, but a cell none
    /// has open any more; those waiting for it alone are woken.
    fn release(&self, key: &Path) {
        let mut inner = self.inner.lock().unwrap();
        let left = inner.entries.remove(key).map_or(Open::Nothing, Entry::into_open);
        let discarded = match rules().open_outlives_holders {
            true if !left.is_nothing() => {
                inner.entries.insert(key.to_owned(), Entry::Unheld(left));
                None
            }
            _ => Some(left),
        };
        if rules().release_wakes
            && let Some(released) = inner.waiting.remove(key)
        {
            released.notify(usize::MAX);
        }
        drop(inner);
        // Without the lock: a kept cell closes as it is dropped.
        drop(discarded);
    }
}

impl<C: Writer, S, P> CellTable<C, S, P> {
    /// Holds the cell `key` names, for the daemon: once its holder, if it has one, lets it go,
    /// if that holder is going, or is the daemon, waiting at most `wait` (for ever, if none);
    /// unless a connection not going writes it, which fails with `EBUSY`. What is open of it stays.
    /// It blocks: the table's tests', whose threads the models schedule; the daemon holds no thread
    /// as it waits ([`CellTable::hold_async`], doc-24).
    #[cfg(test)]
    pub fn hold(&self, key: &Path, wait: Option<Duration>) -> Result<Hold<'_, C, S, P>, Errno> {
        let deadline = wait.map(|wait| Instant::now() + wait);
        match self.claim_blocking(key, &Claim::Daemon, deadline)? {
            Claimed::Held { .. } => Ok(self.held(key)),
            Claimed::Writer(_) => unreachable!("the daemon's claim got a writer"),
        }
    }

    /// Holds the cell `key` names, for the daemon, as [`CellTable::hold`] does, waiting at most
    /// `wait`: a future, which holds no thread while it waits (TASK-43.6).
    pub async fn hold_async(&self, key: &Path, wait: Duration) -> Result<Hold<'_, C, S, P>, Errno> {
        match self.claim_async(key, Claim::Daemon, wait).await? {
            Claimed::Held { .. } => Ok(self.held(key)),
            Claimed::Writer(_) => unreachable!("the daemon's claim got a writer"),
        }
    }

    /// Holds the cell `key` names, for `connection` to write it, as [`CellTable::hold_async`]
    /// does; with the cell kept open of it, if one was, which the connection closes before it
    /// opens the cell. The cell followers have open, it takes over, or finds retired
    /// ([`Hold::instance`]).
    pub async fn hold_to_write(
        &self,
        key: &Path,
        connection: Weak<C>,
        wait: Duration,
    ) -> Result<HeldTaking<'_, C, S, P>, Errno> {
        match self.claim_async(key, Claim::Writer(connection), wait).await? {
            Claimed::Held { kept, .. } => Ok((self.held(key), kept)),
            Claimed::Writer(_) => unreachable!("a writer's claim got a writer"),
        }
    }

    /// The connection writing the cell `key` names, if one does that isn't going; or, if none
    /// does, the cell held for the daemon. `EBUSY` if a connection going holds it, or the
    /// daemon: one going would carry out no request of its guest's, which is gone (TASK-79.34).
    pub fn writer_or_hold(&self, key: &Path) -> Result<Holding<'_, C, S, P>, Errno> {
        match self.try_claim(key, &Claim::DaemonOrWriter, Some(Instant::now()))? {
            Ok(claimed) => Ok(self.holding(key, claimed)),
            Err(_) => Err(Errno::EBUSY),
        }
    }

    /// The connection writing the cell `key` names, if one does that isn't going; or, if none
    /// does, the cell held for the daemon: once a connection going, or the daemon, lets it go,
    /// waiting at most `wait` (`EBUSY`), holding no thread meanwhile.
    pub async fn writer_or_hold_async(
        &self,
        key: &Path,
        wait: Duration,
    ) -> Result<Holding<'_, C, S, P>, Errno> {
        let claimed = self.claim_async(key, Claim::DaemonOrWriter, wait).await?;
        Ok(self.holding(key, claimed))
    }

    /// cladefs: holds the cell `key` names for the daemon's FUSE mount, to attach it (TASK-475.3),
    /// as [`CellTable::hold`] holds it for the daemon, waiting at most `wait`: `EBUSY` at once if a
    /// connection not going writes it, followers have it open, or the mount has it already. With
    /// the cell kept open of it, if one was, to close. Until the hold is dropped, every claim of
    /// it fails with `EBUSY` at once, waiting for nothing: the mount lets it go only as it
    /// detaches its cell. Awaited, holding no thread (board 2946).
    pub async fn hold_mounted(
        &self,
        key: &Path,
        wait: Duration,
    ) -> Result<HeldTaking<'_, C, S, P>, Errno> {
        match self.claim_async(key, Claim::Mount, wait).await? {
            Claimed::Held { kept, .. } => Ok((self.held(key), kept)),
            Claimed::Writer(_) => unreachable!("the mount's claim got a writer"),
        }
    }

    fn held(&self, key: &Path) -> Hold<'_, C, S, P> {
        Hold { table: self, key: key.to_owned() }
    }

    fn holding(&self, key: &Path, claimed: Claimed<C, P>) -> Holding<'_, C, S, P> {
        match claimed {
            Claimed::Held { .. } => Holding::Held(self.held(key)),
            Claimed::Writer(writer) => Holding::Writer(writer),
        }
    }

    /// Claims the cell `key` names for `claim` now; or, if its holder lets it go soon, says what
    /// tells it did, unless `deadline` passed: `EBUSY` then, and if a connection not going writes
    /// it.
    fn try_claim(
        &self,
        key: &Path,
        claim: &Claim<C>,
        deadline: Option<Instant>,
    ) -> Result<Result<Claimed<C, P>, EventListener>, Errno> {
        let mut inner = self.inner.lock().unwrap();
        let (tried, refused, wanted) = match inner.claim(key, claim) {
            Ok(claimed) => (Ok(Ok(claimed)), None, None),
            Err(Refused::Wait(going, wanted))
                if deadline.is_none_or(|deadline| Instant::now() < deadline) =>
            {
                let released = inner.waiting.entry(key.to_owned()).or_default().listen();
                (Ok(Err(released)), going, wanted)
            }
            Err(Refused::Wait(going, _)) => (Err(Errno::EBUSY), going, None),
            Err(Refused::Busy(writer)) => (Err(Errno::EBUSY), Some(writer), None),
            Err(Refused::Now) => (Err(Errno::EBUSY), None, None),
        };
        drop(inner);
        drop(refused);
        // A writer waiting for a going connection has it hurry (TASK-216), without the lock, as
        // what it calls may take it.
        if let (Some(wanted), Claim::Writer(_)) = (wanted, claim) {
            wanted();
        }
        tried
    }

    /// Claims the cell `key` names, waiting on this thread, until `deadline` at most (for ever,
    /// if none): `EBUSY` then. The tests' ([`CellTable::hold`]).
    #[cfg(test)]
    fn claim_blocking(
        &self,
        key: &Path,
        claim: &Claim<C>,
        deadline: Option<Instant>,
    ) -> Result<Claimed<C, P>, Errno> {
        loop {
            let released = match self.try_claim(key, claim, deadline)? {
                Ok(claimed) => return Ok(claimed),
                Err(released) => released,
            };
            if !wait_blocking(released, deadline) {
                return Err(Errno::EBUSY);
            }
        }
    }

    /// Claims the cell `key` names, waiting at most `wait` (`EBUSY` then), holding no thread
    /// meanwhile.
    async fn claim_async(
        &self,
        key: &Path,
        claim: Claim<C>,
        wait: Duration,
    ) -> Result<Claimed<C, P>, Errno> {
        let deadline = Instant::now().checked_add(wait);
        loop {
            let released = match self.try_claim(key, &claim, deadline)? {
                Ok(claimed) => return Ok(claimed),
                Err(released) => released,
            };
            let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            match left {
                None => released.await,
                Some(left) => within(left, released).await.ok_or(Errno::EBUSY)?,
            }
        }
    }
}

impl<C, S, P> Hold<'_, C, S, P> {
    /// The key of the cell it holds.
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// The cell connections have open at its key, if one is, and whether it is retired: its
    /// writer's, or followers', which a writer holding it takes over.
    pub fn instance(&self) -> Option<(Arc<S>, bool)> {
        self.table.instance(&self.key)
    }

    /// `cell` is the cell connections have open at its key from now on, while one has it: the
    /// writer's holding it, or, held by the daemon, followers'. `EBUSY`, and nothing changed, if
    /// a cell is open there, or a cell kept: what holds the cell opens it only where it found
    /// nothing open, or takes over what it found ([`Hold::instance`]).
    ///
    /// Its methods change its entry only if it is its holder's, which nothing but a claim past
    /// [`Rules::one_holder`] takes from it, and only as they say, refusing otherwise: none panics,
    /// which, under the table's lock, would poison every cell's entry, as under Shuttle it would
    /// abort the run (TASK-55).
    pub fn open(&self, cell: &Arc<S>) -> Result<(), Errno> {
        let instance = Instance { cell: Arc::downgrade(cell), retired: false };
        let mut inner = self.table.inner.lock().unwrap();
        inner.entries.retain(|_, entry| !entry.is_stale());
        match inner.entries.get_mut(&self.key) {
            Some(Entry::Writer { cell, .. }) if cell.cell.strong_count() == 0 => {
                *cell = instance;
                Ok(())
            }
            Some(Entry::Daemon(open)) if open.is_closed() => {
                *open = Open::Followed(instance);
                Ok(())
            }
            _ => Err(Errno::EBUSY),
        }
    }

    /// Has `wanted` called as a writer waits for it to let the cell go, its connection going
    /// (TASK-216): a writer's hold alone, whose connection's cell, closing, may take as long as
    /// its bucket. Nothing for the daemon's.
    pub fn when_wanted(&self, wanted: Wanted) {
        let mut inner = self.table.inner.lock().unwrap();
        if let Some(Entry::Writer { wanted: slot, .. }) = inner.entries.get_mut(&self.key) {
            *slot = Some(wanted);
        }
    }

    /// Retires the cell connections have open at its key, its writer gone with writes
    /// uncommitted: `cut` cuts its followers off, without the table's lock, and neither a writer
    /// nor a follower opens it while followers have it.
    ///
    /// Marked first, then cut (TASK-160.8): a follower made before the cut is cut off, and one made
    /// after finds it marked as it looks again ([`CellTable::follow`]). Cut first, a follower
    /// finding it unmarked between the two followed a cell retired, never cut off.
    pub fn retire(&self, cut: impl FnOnce()) {
        let mut cut = Some(cut);
        if !rules().marks_before_cutting {
            cut.take().map(|cut| cut());
        }
        if rules().retirement {
            let mut inner = self.table.inner.lock().unwrap();
            if let Some(instance) = inner.entries.get_mut(&self.key).and_then(Entry::instance) {
                instance.retired = true;
            }
        }
        cut.map(|cut| cut());
    }

    /// Keeps `kept` open at its key for forks, after its hold too: the daemon's, which found
    /// nothing open there. Refused, `kept` given back, if a cell is open there or a cell kept
    /// already, or a writer holds it.
    pub fn keep(&self, kept: P) -> Result<(), P> {
        let mut inner = self.table.inner.lock().unwrap();
        match inner.entries.get_mut(&self.key) {
            Some(Entry::Daemon(open)) if open.is_closed() => {
                *open = Open::Kept(kept);
                Ok(())
            }
            // Dropped without the lock: a kept cell closes as it is dropped.
            _ => Err(kept),
        }
    }

    /// Takes the cell kept open at its key, if one is, to close it.
    pub fn take_kept(&self) -> Option<P> {
        let mut inner = self.table.inner.lock().unwrap();
        match inner.entries.get_mut(&self.key) {
            Some(Entry::Daemon(open)) => match std::mem::replace(open, Open::Nothing) {
                Open::Kept(kept) => Some(kept),
                other => {
                    *open = other;
                    None
                }
            },
            _ => None,
        }
    }
}

impl<C, S, P> Drop for Hold<'_, C, S, P> {
    fn drop(&mut self) {
        // Those waiting to hold it may.
        self.table.release(&self.key);
    }
}

/// The table under Shuttle: a VM's connection writing a cell, whose VM process goes, with
/// writes uncommitted or not; as it goes, the VM restored from its snapshot opening the cell to
/// write it, and followers following it, before and after it went; and a fork of the cell at any
/// kept cell, which keeps it open as a kept cell if nothing has it open. Every interleaving of them
/// Shuttle's scheduler chooses (TASK-160.8). Waits are unbounded, so that a holder that never
/// wakes those waiting is a deadlock, which Shuttle reports, rather than a wait of `CLOSING_WAIT`.
/// Invariants: one holder at a time; the cell open once at a time, as a writer's, followers' or a
/// kept cell; no follower following the cell once its writer retired it. Each rule turned off
/// fails.
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests {
    use super::*;
    use crate::shuttle_testing::{config, fails_with, runs};
    use shuttle::future::block_on;
    use shuttle::scheduler::{PctScheduler, RandomScheduler};
    use shuttle::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use shuttle::sync::mpsc;
    use shuttle::{Runner, thread};

    const KEY: &str = "/cell";

    /// A connection: going once its VM process is; and, if it writes the cell, the cell and its
    /// hold of it, let go as the last of the connection is dropped, the cell first, as
    /// `cladefsd::server::Connection`'s are: wherever that is, but under the table's lock, which
    /// letting the hold go takes.
    struct Conn {
        going: AtomicBool,
        writing: std::sync::Mutex<Option<Writing>>,
    }

    impl Conn {
        fn new() -> Arc<Self> {
            Arc::new(Conn { going: AtomicBool::new(false), writing: Default::default() })
        }
    }

    impl Writer for Conn {
        fn going(&self) -> bool {
            self.going.load(Ordering::SeqCst)
        }
    }

    impl Drop for Conn {
        fn drop(&mut self) {
            let writing = self.writing.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(Writing { cell, hold }) = writing.take() else { return };
            drop(cell);
            // Gone, its hold not let go yet: what claims the cell meets a writer whose
            // connection is gone, and waits.
            if hold.hold().table.is_waited_for(Path::new(KEY)) {
                reached("a claim waited for a writer gone");
            }
            drop(hold);
        }
    }

    /// What a run counts: the cell's openings open at once, and the most at once; its holders at
    /// once, and whether ever two. Checked once the run is done: a task panicking unwinds through
    /// the holds it has, whose drops take Shuttle's locks, which fail in a failed run with panics
    /// that abort (TASK-55). Not Shuttle's atomics, whose every access the scheduler may switch
    /// threads at: a cell's closing is counted as its last holder drops it, rather than after
    /// another thread found it gone and opened it anew, which the table doesn't order: the cell's
    /// own lock of its files does (`OpenCell::open`).
    #[derive(Default)]
    struct Counts {
        open: std::sync::atomic::AtomicUsize,
        most_open: std::sync::atomic::AtomicUsize,
        holders: std::sync::atomic::AtomicUsize,
        two_holders: std::sync::atomic::AtomicBool,
    }

    /// The cell, opened: counted open until it is dropped, closed; dirty once its writer went
    /// with writes uncommitted; and how many times its followers were cut off. A kept cell's says
    /// it is closed, dropping what does.
    struct ModelCell {
        counts: Arc<Counts>,
        dirty: AtomicBool,
        cuts: AtomicUsize,
        _closing: Option<mpsc::Sender<()>>,
    }

    impl ModelCell {
        fn open(counts: &Arc<Counts>) -> Arc<Self> {
            Self::opened(counts, None)
        }

        fn opened(counts: &Arc<Counts>, closing: Option<mpsc::Sender<()>>) -> Arc<Self> {
            let others = counts.open.fetch_add(1, Ordering::SeqCst);
            counts.most_open.fetch_max(others + 1, Ordering::SeqCst);
            let (dirty, cuts) = (AtomicBool::new(false), AtomicUsize::new(0));
            Arc::new(ModelCell { counts: counts.clone(), dirty, cuts, _closing: closing })
        }
    }

    impl Drop for ModelCell {
        fn drop(&mut self) {
            self.counts.open.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// A cell kept open, as `cladefsd::cells::Kept` is: the cell opened, which each fork of it
    /// holds while it forks it, and what says it is closed, once the last lets it go.
    struct Kept {
        cell: Arc<ModelCell>,
        closed: mpsc::Receiver<()>,
    }

    impl Kept {
        fn open(counts: &Arc<Counts>) -> Self {
            let (closing, closed) = mpsc::channel();
            Kept { cell: ModelCell::opened(counts, Some(closing)), closed }
        }
    }

    /// Closes `kept`, if given, and returns once it is closed: once the forks forking it are
    /// done, as `cladefsd::cells::close_kept` does.
    fn close(kept: Option<Kept>) {
        let Some(Kept { cell, closed }) = kept else { return };
        drop(cell);
        if closed.try_recv() == Err(mpsc::TryRecvError::Empty) {
            reached("a kept closed waited for a fork of it");
        }
        let _ = closed.recv();
    }

    type Table = CellTable<Conn, ModelCell, Kept>;

    /// What the runs reached, of the ways through the table a test is to take
    /// ([`reaching`]): across runs, outside Shuttle's.
    static REACHED: std::sync::Mutex<std::collections::BTreeSet<&'static str>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());

    fn reached(what: &'static str) {
        REACHED.lock().unwrap().insert(what);
    }

    /// Runs `runs`, checking they reached each of `ways`: schedules that never reach them would
    /// pass, testing nothing.
    fn reaching(ways: &[&'static str], runs: impl Fn()) {
        runs();
        let reached = REACHED.lock().unwrap();
        let missed: Vec<_> = ways.iter().filter(|way| !reached.contains(*way)).collect();
        assert!(missed.is_empty(), "no run reached {missed:?}, only {reached:?}");
    }

    /// A hold counted among the cell's holders, which none other may be, until dropped.
    struct Counted<'t> {
        counts: Arc<Counts>,
        _hold: Hold<'t, Conn, ModelCell, Kept>,
    }

    impl<'t> Counted<'t> {
        fn new(counts: &Arc<Counts>, hold: Hold<'t, Conn, ModelCell, Kept>) -> Self {
            if counts.holders.fetch_add(1, Ordering::SeqCst) > 0 {
                counts.two_holders.store(true, Ordering::SeqCst);
            }
            Self { counts: counts.clone(), _hold: hold }
        }

        fn hold(&self) -> &Hold<'t, Conn, ModelCell, Kept> {
            &self._hold
        }

        /// Opens `cell` where it holds, as [`Hold::open`] does, which refuses (`EBUSY`) what holds
        /// a cell found with nothing open only if another holder took it from under it: a claim
        /// past [`Rules::one_holder`], and nothing else, does. Counted as two holders as it shows,
        /// rather than left to the refusal, which the holder meets first and the run would report
        /// in its place, as a schedule may have it before its check of the holders.
        fn open(&self, cell: &Arc<ModelCell>) -> Result<(), Errno> {
            let opened = self._hold.open(cell);
            if opened.is_err() {
                self.counts.two_holders.store(true, Ordering::SeqCst);
            }
            opened
        }
    }

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.counts.holders.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// What a connection writing the cell has: its cell, closed before its hold lets it go
    /// ([`Conn`]).
    struct Writing {
        cell: Arc<ModelCell>,
        hold: Counted<'static>,
    }

    /// Opens the cell for `conn` to write, as a connection does (`cladefsd::server`): holding it,
    /// closing the cell kept of it, taking over the cell followers have open, unless it is
    /// retired, or not quiescent, its writer gone with writes uncommitted (`OpenCell::take_over`);
    /// or opening it. The connection has it from then on.
    fn write(table: &'static Table, counts: &Arc<Counts>, conn: &Arc<Conn>) -> Result<(), Errno> {
        let (hold, kept) =
            block_on(table.hold_to_write(Path::new(KEY), Arc::downgrade(conn), Duration::MAX))?;
        let hold = Counted::new(counts, hold);
        if kept.is_some() {
            reached("a writer closed a cell kept");
        }
        close(kept);
        let cell = match hold.hold().instance() {
            Some((_, true)) => {
                reached("a writer found the cell retired");
                return Err(Errno::EBUSY);
            }
            Some((cell, false)) if cell.dirty.load(Ordering::SeqCst) => {
                return Err(Errno::EBUSY);
            }
            Some((cell, false)) => {
                reached("a writer took the cell over");
                cell
            }
            None => {
                let cell = ModelCell::open(counts);
                hold.open(&cell)?;
                cell
            }
        };
        *conn.writing.lock().unwrap() = Some(Writing { cell, hold });
        Ok(())
    }

    /// A follower of the cell, as `OpenCell::follow` makes one: its view of it, which a cut of
    /// its followers made after it cuts off.
    struct Follower {
        cell: Arc<ModelCell>,
        cuts: usize,
    }

    impl Follower {
        fn of(cell: Arc<ModelCell>) -> Self {
            let cuts = cell.cuts.load(Ordering::SeqCst);
            Follower { cell, cuts }
        }

        /// Whether it follows a cell its writer retired, gone with writes uncommitted, without
        /// having been cut off.
        fn uncut_of_retired(&self) -> bool {
            self.cell.dirty.load(Ordering::SeqCst)
                && self.cell.cuts.load(Ordering::SeqCst) == self.cuts
        }
    }

    /// Follows the cell, as a follower does (`cladefsd::cells::followed`): the cell connections
    /// have open, unless retired ([`CellTable::follow`]); or, held by the daemon, which no writer
    /// retires a cell under, its kept cell closed, the cell opened.
    fn follow(table: &Table, counts: &Arc<Counts>) -> Result<Follower, Errno> {
        let open = |found: Option<(Arc<ModelCell>, bool)>| match found {
            Some((_, true)) => Err(Errno::EBUSY),
            Some((cell, false)) => Ok(Some(cell)),
            None => Ok(None),
        };
        let follow = |cell: &Arc<ModelCell>| Ok(Follower::of(cell.clone()));
        if let Some(follower) = table.follow(Path::new(KEY), follow)? {
            reached("a follower followed the cell open");
            return Ok(follower);
        }
        let hold = Counted::new(counts, table.hold(Path::new(KEY), None)?);
        close(hold.hold().take_kept());
        if let Some(cell) = open(hold.hold().instance())? {
            return Ok(Follower::of(cell));
        }
        reached("a follower opened the cell");
        let cell = ModelCell::open(counts);
        hold.open(&cell)?;
        Ok(Follower::of(cell))
    }

    /// Forks the cell, as the daemon does (`cladefsd::cells::fork`): through its writer, or the
    /// cell followers have open, live; or the cell kept, or else the cell opened, kept open as
    /// a kept cell, which it holds while it forks it, as a restore's fork does, its hold let go.
    fn fork(table: &Table, counts: &Arc<Counts>) -> Result<(), Errno> {
        let kept = |kept: &mut Kept| kept.cell.clone();
        let forking = match table.kept(Path::new(KEY), kept) {
            Some(forking) => forking,
            None => {
                let hold =
                    match block_on(table.writer_or_hold_async(Path::new(KEY), Duration::MAX))? {
                        Holding::Writer(_) => return Ok(()),
                        Holding::Held(hold) => Counted::new(counts, hold),
                    };
                if hold.hold().instance().is_some() {
                    return Ok(());
                }
                match table.kept(Path::new(KEY), kept) {
                    Some(forking) => forking,
                    None => {
                        reached("a fork kept a kept");
                        let kept = Kept::open(counts);
                        let forking = kept.cell.clone();
                        // Refused only past one holder, which the run then fails for.
                        let _ = hold.hold().keep(kept);
                        forking
                    }
                }
            }
        };
        thread::yield_now();
        drop(forking);
        Ok(())
    }

    /// The run, its first writer going with writes uncommitted if `dirty`.
    fn run(dirty: bool) {
        let table: &'static Table = Box::leak(Box::new(CellTable::new()));
        let counts = Arc::new(Counts::default());
        let (followed_tx, followed) = mpsc::channel::<()>();
        let (later_tx, later) = mpsc::channel::<()>();
        let (going_tx, going) = mpsc::channel::<()>();
        let released = Arc::new(AtomicBool::new(false));
        // That the first went, its cell retired if it is to be, to each follower.
        let (gone_to_follower, gone_follower) = mpsc::channel::<()>();
        let (gone_to_later, gone_later) = mpsc::channel::<()>();

        // The VM's connection, whose VM process goes while it writes the cell.
        let first = {
            let (counts, released) = (counts.clone(), released.clone());
            thread::spawn(move || {
                let conn = Conn::new();
                // Refused only if another holder took the cell from under it, which the run's
                // check of the holders reports.
                let wrote = match write(table, &counts, &conn) {
                    Ok(()) => true,
                    Err(refused) => {
                        let two = counts.two_holders.load(Ordering::SeqCst);
                        assert!(two, "the first writer was refused: {refused:?}");
                        false
                    }
                };
                followed_tx.send(()).unwrap();
                later_tx.send(()).unwrap();
                conn.going.store(true, Ordering::SeqCst);
                going_tx.send(()).unwrap();
                if dirty && wrote {
                    let writing = conn.writing.lock().unwrap();
                    let Writing { cell, hold } = writing.as_ref().unwrap();
                    cell.dirty.store(true, Ordering::SeqCst);
                    hold.hold().retire(|| {
                        cell.cuts.fetch_add(1, Ordering::SeqCst);
                    });
                }
                // The last of it, unless a claim of the cell has it as it meets it: it lets the
                // cell go then, outside the table's lock.
                drop(conn);
                released.store(true, Ordering::SeqCst);
                // Unheard by a follower that follows nothing.
                let _ = gone_to_follower.send(());
                let _ = gone_to_later.send(());
            })
        };
        // The VM restored from its snapshot, opening the cell as the first goes: it gets it
        // once the first lets it go, unless followers have the first's cell, retired.
        let second = {
            let counts = counts.clone();
            thread::spawn(move || {
                going.recv().unwrap();
                let conn = Conn::new();
                if write(table, &counts, &conn).is_ok() {
                    thread::yield_now();
                }
                drop(conn);
            })
        };
        // A follower of the first's cell, following it until the first went: cut off, if it
        // retired it.
        let follower = {
            let counts = counts.clone();
            thread::spawn(move || {
                followed.recv().unwrap();
                if let Ok(follower) = follow(table, &counts) {
                    gone_follower.recv().unwrap();
                    let uncut = follower.uncut_of_retired();
                    drop(follower);
                    assert!(!uncut, "a follower of a cell its writer retired wasn't cut off");
                }
            })
        };
        // A follower, following the cell as the first goes: never its cell once it retired it,
        // and, before or as it does, cut off.
        let following_later = {
            let (counts, released) = (counts.clone(), released.clone());
            thread::spawn(move || {
                later.recv().unwrap();
                let after = released.load(Ordering::SeqCst);
                if let Ok(follower) = follow(table, &counts) {
                    let retired = after && follower.cell.dirty.load(Ordering::SeqCst);
                    gone_later.recv().unwrap();
                    let uncut = follower.uncut_of_retired();
                    drop(follower);
                    assert!(!retired, "a follower followed a cell its writer retired");
                    assert!(!uncut, "a follower of a cell its writer retired wasn't cut off");
                }
            })
        };
        // A fork of the cell, at any kept cell.
        let forking = {
            let counts = counts.clone();
            thread::spawn(move || fork(table, &counts).unwrap())
        };
        for thread in [first, second, follower, following_later, forking] {
            thread.join().unwrap();
        }
        // The kept cell a fork kept, closed, as it is once no fork forks it.
        let hold = table.hold(Path::new(KEY), None).unwrap();
        close(hold.take_kept());
        drop(hold);
        let two = counts.two_holders.load(Ordering::SeqCst);
        assert!(!two, "two holders hold the cell at once");
        let most = counts.most_open.load(Ordering::SeqCst);
        assert!(most <= 1, "the cell is open twice at once: {most} at most");
        assert!(table.is_empty(), "the cell is held or open once all let it go");
    }

    fn randomly(dirty: bool) {
        Runner::new(RandomScheduler::new(runs(3000)), config()).run(move || run(dirty));
    }

    fn by_pct(dirty: bool) {
        Runner::new(PctScheduler::new(3, runs(3000)), config()).run(move || run(dirty));
    }

    /// What runs of a writer going quiescent reach: its cell taken over, followed, and kept cells
    /// kept and closed.
    const QUIESCENT: &[&str] = &[
        "a writer took the cell over",
        "a writer closed a cell kept",
        "a kept closed waited for a fork of it",
        "a claim waited for a writer gone",
        "a follower followed the cell open",
        "a follower opened the cell",
        "a fork kept a kept",
    ];

    /// What runs of a writer going with writes uncommitted reach besides: its cell retired.
    const UNCOMMITTED: &[&str] =
        &["a writer found the cell retired", "a follower followed the cell open"];

    #[test]
    fn writers_going_quiescent_randomly() {
        set_rules(Rules::ALL);
        reaching(QUIESCENT, || randomly(false));
    }

    #[test]
    fn writers_going_quiescent_by_pct() {
        set_rules(Rules::ALL);
        reaching(QUIESCENT, || by_pct(false));
    }

    #[test]
    fn writers_going_with_writes_uncommitted_randomly() {
        set_rules(Rules::ALL);
        reaching(UNCOMMITTED, || randomly(true));
    }

    #[test]
    fn writers_going_with_writes_uncommitted_by_pct() {
        set_rules(Rules::ALL);
        reaching(UNCOMMITTED, || by_pct(true));
    }

    /// Runs the schedules with `rules`, expecting one to fail with a panic saying `expected`.
    fn fails_without(rules: Rules, dirty: bool, expected: &str) {
        set_rules(rules);
        fails_with(expected, || randomly(dirty));
    }

    #[test]
    fn needs_one_holder() {
        let rules = Rules { one_holder: false, ..Rules::ALL };
        fails_without(rules, false, "two holders hold the cell");
    }

    /// CI run 42234149254's schedule: a claim past the rule took the first writer's cell from
    /// under it, whose opening it then refused (`EBUSY`), which the writer met, and the run
    /// reported, before its check of the holders. Counted as two holders as it shows, it is.
    #[test]
    fn needs_one_holder_where_the_first_writer_meets_the_second() {
        set_rules(Rules { one_holder: false, ..Rules::ALL });
        fails_with("two holders hold the cell", || {
            let scheduler = RandomScheduler::new_from_seed(4791423815028538899, 1);
            Runner::new(scheduler, config()).run(|| run(false));
        });
    }

    /// The resize that let the cell go without waking those waiting for it (fixed by having it
    /// release through `release`): they waited `CLOSING_WAIT` out.
    #[test]
    fn needs_releases_to_wake() {
        fails_without(Rules { release_wakes: false, ..Rules::ALL }, false, "deadlock!");
    }

    /// A writer's cell followers have, forgotten as it goes, is opened anew by the next.
    #[test]
    fn needs_what_is_open_to_outlive_its_holders() {
        let rules = Rules { open_outlives_holders: false, ..Rules::ALL };
        fails_without(rules, false, "open twice at once");
    }

    /// A cell its writer retired, forgotten as such, is followed after, and on, uncut, by a
    /// follower made as it was retired: whichever a schedule meets first.
    #[test]
    fn needs_retirement() {
        let rules = Rules { retirement: false, ..Rules::ALL };
        fails_without(rules, true, "a cell its writer retired");
    }

    /// TASK-160.8: a cell's followers cut off before it is marked retired, a follower finding it
    /// unmarked between the two is followed on, uncut (`cladefsd::cells::retire` did so).
    #[test]
    fn needs_retirement_to_mark_before_cutting() {
        let rules = Rules { marks_before_cutting: false, ..Rules::ALL };
        fails_without(rules, true, "retired wasn't cut off");
    }

    /// A follower that found the cell unmarked before its writer retired it, and was made after
    /// its followers were cut off, follows a retired cell on, uncut, unless it looks again.
    #[test]
    fn needs_followers_to_look_again() {
        let rules = Rules { followers_look_again: false, ..Rules::ALL };
        fails_without(rules, true, "retired wasn't cut off");
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Connection(AtomicBool);

    impl Writer for Connection {
        fn going(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn connection(going: bool) -> Arc<Connection> {
        Arc::new(Connection(AtomicBool::new(going)))
    }

    /// A cell, and a cell kept open, each named.
    type Table = CellTable<Connection, &'static str, &'static str>;

    /// Holds `key` for `connection` to write it, at once.
    fn write<'t>(
        table: &'t Table,
        key: &Path,
        connection: &Arc<Connection>,
    ) -> Result<HeldTaking<'t, Connection, &'static str, &'static str>, Errno> {
        block_on(table.hold_to_write(key, Arc::downgrade(connection), Duration::ZERO))
    }

    /// One waiting for a cell a going holder never lets go gives up once its wait is over.
    #[test]
    fn waits_for_cells_never_let_go_end() {
        let table = Table::new();
        let (going, next) = (connection(true), connection(false));
        let key = Path::new("cell");
        let _held = write(&table, key, &going).unwrap();
        let wait = Duration::from_millis(10);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let held = runtime.block_on(table.hold_to_write(key, Arc::downgrade(&next), wait));
        assert_eq!(held.err(), Some(Errno::EBUSY));
        let held = runtime.block_on(table.writer_or_hold_async(key, wait));
        assert_eq!(held.err(), Some(Errno::EBUSY));
        assert_eq!(table.hold(key, Some(wait)).err(), Some(Errno::EBUSY));
    }

    /// TASK-79.34: a cell whose holder is going, its VM process gone, is the daemon's neither to
    /// work on through its holder, nor here, until it lets it go: `EBUSY`.
    #[test]
    fn cells_of_going_holders_are_busy() {
        let table = Table::new();
        let going = connection(true);
        let key = Path::new("cell");
        let held = write(&table, key, &going).unwrap();
        assert_eq!(table.writer_or_hold(key).err(), Some(Errno::EBUSY));
        drop(held);
        assert!(matches!(table.writer_or_hold(key), Ok(Holding::Held(_))));
    }

    /// A cell a connection not going writes is its: the daemon works through it, and no other
    /// holds it, but fails at once with `EBUSY`.
    #[test]
    fn cells_writers_write_are_theirs() {
        let table = Table::new();
        let (writer, next) = (connection(false), connection(false));
        let key = Path::new("cell");
        let _held = write(&table, key, &writer).unwrap();
        let through = table.writer_or_hold(key);
        assert!(matches!(through, Ok(Holding::Writer(found)) if Arc::ptr_eq(&found, &writer)));
        assert_eq!(write(&table, key, &next).err(), Some(Errno::EBUSY));
        assert_eq!(table.hold(key, None).err(), Some(Errno::EBUSY));
    }

    /// TASK-92.5: a cell let go wakes those waiting to hold it, whose wait goes with it: those
    /// waiting for another cell wait on.
    #[test]
    fn releases_wake_those_waiting_for_their_cell() {
        let table = Table::new();
        let going = connection(true);
        let (first, second) = (Path::new("first"), Path::new("second"));
        let first_held = write(&table, first, &going).unwrap();
        let _second_held = write(&table, second, &going).unwrap();
        let (ours, theirs) = (connection(false), connection(false));
        let wait = Duration::from_secs(60);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let ours_held = runtime.block_on(async {
            let mut holding_first =
                std::pin::pin!(table.hold_to_write(first, Arc::downgrade(&ours), wait));
            let mut holding_second =
                std::pin::pin!(table.hold_to_write(second, Arc::downgrade(&theirs), wait));
            assert!(futures::poll!(&mut holding_first).is_pending());
            assert!(futures::poll!(&mut holding_second).is_pending());
            assert_eq!(table.inner.lock().unwrap().waiting.len(), 2);
            drop(first_held);
            assert_eq!(table.inner.lock().unwrap().waiting.len(), 1);
            let (held, _) = holding_first.await.unwrap();
            assert!(futures::poll!(&mut holding_second).is_pending());
            held
        });
        assert!(Arc::ptr_eq(&table.writer(first).unwrap(), &ours));
        drop(ours_held);
    }

    /// The cells connections write are counted, not the daemon's, nor those of connections gone.
    #[test]
    fn cells_writers_write_are_counted() {
        let table = Table::new();
        let (first, second) = (connection(false), connection(false));
        let _first = write(&table, Path::new("first"), &first).unwrap();
        let _second = write(&table, Path::new("second"), &second).unwrap();
        let _daemons = table.hold(Path::new("daemon's"), None).unwrap();
        assert_eq!(table.writers(), 2);
        drop(second);
        assert_eq!(table.writers(), 1);
    }

    /// A writer's cell outlives its writer while followers have it: the next writer takes it
    /// over; gone once none has it, it is nothing, and the next opens its own.
    #[test]
    fn cells_followers_have_outlive_their_writers() {
        let table = Table::new();
        let key = Path::new("cell");
        let (writer, next) = (connection(false), connection(false));
        let (held, _) = write(&table, key, &writer).unwrap();
        let cell = Arc::new("writer's");
        held.open(&cell).unwrap();
        let followed = table.instance(key).unwrap().0;
        writer.0.store(true, Ordering::Relaxed);
        drop((held, cell));
        assert_eq!(
            table.instance(key).map(|(cell, retired)| (*cell, retired)),
            Some(("writer's", false))
        );
        let (held, _) = write(&table, key, &next).unwrap();
        assert!(Arc::ptr_eq(&held.instance().unwrap().0, &followed));
        drop(held);
        drop(followed);
        assert_eq!(table.instance(key), None);
        assert!(table.is_empty());
        let (held, _) = write(&table, key, &next).unwrap();
        assert_eq!(held.instance(), None);
    }

    /// A cell retired, its writer gone with writes uncommitted, is so while followers have it:
    /// to the next writer, and to the daemon.
    #[test]
    fn cells_retired_stay_retired_while_followed() {
        let table = Table::new();
        let key = Path::new("cell");
        let writer = connection(false);
        let (held, _) = write(&table, key, &writer).unwrap();
        let cell = Arc::new("writer's");
        held.open(&cell).unwrap();
        held.retire(|| ());
        drop(held);
        assert_eq!(table.instance(key).map(|(_, retired)| retired), Some(true));
        let held = table.hold(key, None).unwrap();
        assert_eq!(held.instance().map(|(_, retired)| retired), Some(true));
        drop(held);
        let (held, _) = write(&table, key, &connection(false)).unwrap();
        assert_eq!(held.instance().map(|(_, retired)| retired), Some(true));
        drop((held, cell));
        assert!(table.is_empty());
    }

    /// A cell kept open outlives the daemon's hold, which forks see; the daemon's next holds keep
    /// it, unless they take it; a writer's takes it, to close.
    #[test]
    fn copies_kept_go_to_writers() {
        let table = Table::new();
        let key = Path::new("kept");
        let held = table.hold(key, None).unwrap();
        held.keep("kept").unwrap();
        drop(held);
        assert_eq!(table.kept(key, |kept| *kept), Some("kept"));
        let held = table.hold(key, None).unwrap();
        assert_eq!(table.kept(key, |kept| *kept), Some("kept"));
        drop(held);
        let (held, evicted) = write(&table, key, &connection(false)).unwrap();
        assert_eq!(evicted, Some("kept"));
        assert_eq!(table.kept(key, |kept| *kept), None);
        drop(held);
        assert!(table.is_empty());
        let held = table.hold(key, None).unwrap();
        held.keep("again").unwrap();
        assert_eq!(held.take_kept(), Some("again"));
        drop(held);
        assert!(table.is_empty());
    }

    /// A cell opened where one is open, or a cell kept, is refused, and changes nothing: its
    /// holder takes over what it found, or works on it, rather than open it twice.
    #[test]
    fn cells_opened_where_something_is_open_are_refused() {
        let table = Table::new();
        let key = Path::new("cell");
        let (held, _) = write(&table, key, &connection(false)).unwrap();
        let (first, second) = (Arc::new("first"), Arc::new("second"));
        held.open(&first).unwrap();
        assert_eq!(held.open(&second), Err(Errno::EBUSY));
        drop(held);
        let held = table.hold(key, None).unwrap();
        assert_eq!(held.open(&second), Err(Errno::EBUSY));
        assert!(Arc::ptr_eq(&held.instance().unwrap().0, &first));
        drop((held, first));
        let held = table.hold(key, None).unwrap();
        held.keep("kept").unwrap();
        assert_eq!(held.open(&second), Err(Errno::EBUSY));
        assert_eq!(held.instance(), None);
        assert_eq!(held.take_kept(), Some("kept"));
        held.open(&second).unwrap();
        assert!(Arc::ptr_eq(&held.instance().unwrap().0, &second));
    }

    /// A cell kept where one is kept already, or a cell is open, or by a writer, is refused,
    /// given back, and changes nothing.
    #[test]
    fn copies_kept_where_something_is_open_are_refused() {
        let table = Table::new();
        let key = Path::new("kept");
        let held = table.hold(key, None).unwrap();
        held.keep("first").unwrap();
        assert_eq!(held.keep("second"), Err("second"));
        assert_eq!(table.kept(key, |kept| *kept), Some("first"));
        assert_eq!(held.take_kept(), Some("first"));
        let cell = Arc::new("cell");
        held.open(&cell).unwrap();
        assert_eq!(held.keep("second"), Err("second"));
        drop((held, cell));
        let (held, _) = write(&table, key, &connection(false)).unwrap();
        assert_eq!(held.keep("second"), Err("second"));
        assert_eq!(table.kept(key, |kept| *kept), None);
    }

    /// TASK-475.3: a cell the mount has attached is its alone: a writer, the daemon, and the
    /// daemon working through a writer each fail at once with `EBUSY`, however long they would
    /// wait; let go, a writer holds it.
    #[test]
    fn cells_mounted_are_the_mounts_alone() {
        let table = Table::new();
        let key = Path::new("cell");
        let (held, kept) = block_on(table.hold_mounted(key, Duration::ZERO)).unwrap();
        assert_eq!(kept, None);
        let writer = connection(false);
        let wait = Duration::from_secs(60);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let started = Instant::now();
        let written = runtime.block_on(table.hold_to_write(key, Arc::downgrade(&writer), wait));
        assert_eq!(written.err(), Some(Errno::EBUSY));
        assert_eq!(table.hold(key, Some(wait)).err(), Some(Errno::EBUSY));
        assert_eq!(table.writer_or_hold(key).err(), Some(Errno::EBUSY));
        assert_eq!(runtime.block_on(table.hold_mounted(key, wait)).err(), Some(Errno::EBUSY));
        assert!(started.elapsed() < Duration::from_secs(5), "waited {:?}", started.elapsed());
        drop(held);
        assert!(write(&table, key, &writer).is_ok());
    }
}
