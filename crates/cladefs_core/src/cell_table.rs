// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The extent daemon's cells (`crate::cladefsd`), by the keys that name them
//! (`cladefsd::cells::cell_dir_key`): one table, whose entry for a key says all there is of its
//! cell (TASK-160.8). Who holds it: a connection writing it, or the daemon working on it. And what
//! is open of it for others: the cell its writer's connection has open, which the daemon's work
//! through it shares, or a kept cell the daemon keeps open for forks. Five owners said so before,
//! kept in step by hand, which TASK-79.34, 150.5, 54, 79.84, 79.95, 92.5, 79.103 and 79.36 came of.
//!
//! A cell has one holder at a time, whose hold ([`Hold`]) is a value that lets the cell go as it
//! is dropped: a connection's, from its `OPEN` until it is gone and its cell closed; the daemon's,
//! while it forks, syncs or resizes a cell no connection writes. A connection whose VM process is
//! gone, still holding its cell, lets it go to the next that asks, as a VM restored from a
//! snapshot at once opens its VM's cell (TASK-39.1). What is open of a cell outlives its holder:
//! a cell others share, as long as one has it (none is made since shared volumes went, board 14527;
//! board 14535 removes the rest), which no writer, nor the mount, holds meanwhile (`EBUSY`), as one
//! cell is open once at a time, and writes go to no
//! cell others read; a writer's that went, which the daemon's work through it has still, closing,
//! which writers and the mount wait for to close ([`CellTable::closed`]), the daemon working
//! through it meanwhile; a kept
//! cell kept open for forks, until a writer holds the cell, which closes it first (TASK-92.5). A
//! connection holds no kept cell: its entry ([`Entry::Writer`]) has no room for one.
//!
//! Generic over its connections, cells and kept cells, so that its tests run it with models of
//! them: under Shuttle (awslabs/shuttle), with the `shuttle` feature, whose scheduler chooses every
//! interleaving of the threads writing, sharing and keeping kept cells of a cell as its writer's VM
//! process goes (TASK-44.6).

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
pub struct Rules {
    /// A cell held by a holder that isn't going holds no other.
    pub one_holder: bool,
    /// Letting a cell go wakes those waiting to hold it.
    pub release_wakes: bool,
    /// What is open of a cell, the cell others share or a cell kept, stays as its holder lets the
    /// cell go.
    pub open_outlives_holders: bool,
    /// A writer holds no cell others have open: it fails with `EBUSY` while they do.
    pub writers_refuse_shared: bool,
    /// A writer waits for the cell a writer that went left open, closing, to close.
    pub writers_wait_for_closing: bool,
    /// A shrink waits for the guest to agree.
    pub guest_agrees: bool,
    /// A connection's resizes of its cell run one at a time (TASK-79.41).
    pub resizes_one_at_a_time: bool,
    /// A resize goes through the cell's open writer or holds its path (TASK-79.34).
    pub through_holder: bool,
    /// A fork goes through the cell's open writer or holds its path (TASK-44.6).
    pub fork_through_holder: bool,
    /// A fork of a closed cell holds its path until the cell is closed (TASK-44.6).
    pub forks_hold_their_paths: bool,
}

impl Rules {
    pub const ALL: Self = Self {
        one_holder: true,
        release_wakes: true,
        open_outlives_holders: true,
        writers_refuse_shared: true,
        writers_wait_for_closing: true,
        guest_agrees: true,
        resizes_one_at_a_time: true,
        through_holder: true,
        fork_through_holder: true,
        forks_hold_their_paths: true,
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
pub fn set_rules(rules: Rules) {
    *RULES.lock().unwrap() = rules;
}

/// `future`'s output, unless `wait` passes first (`None`). Under Shuttle, where time doesn't
/// pass, it waits for ever: what never wakes it is a deadlock, which Shuttle reports.
pub async fn within<F: std::future::Future>(wait: Duration, future: F) -> Option<F::Output> {
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
#[cfg(any(test, feature = "testing"))]
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
pub trait Writer: Send + Sync {
    /// Whether it is going: its VM process gone, it lets its cell go to the next that asks.
    fn going(&self) -> bool;
}

/// The cells, by their keys: an entry each, while anything holds a cell or has it open.
pub struct CellTable<C: ?Sized, S, P> {
    inner: Arc<Mutex<Inner<C, S, P>>>,
}

impl<C: ?Sized, S, P> Clone for CellTable<C, S, P> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

struct Inner<C: ?Sized, S, P> {
    entries: HashMap<PathBuf, Entry<C, S, P>>,
    /// What says a cell is let go, to those waiting to hold it, on threads or not, by its key:
    /// woken as its holder lets it go, and none of another's, as every cell's were, at each
    /// cell's letting go (TASK-92.5). From the first that waits until then.
    waiting: HashMap<PathBuf, Event>,
    /// Under Shuttle: live [`Hold`]s per key and whether any key ever had two at once (board
    /// 10956). Counted under `inner`'s lock where holds are issued and released: counting outside
    /// it lets a releasing holder decrement before `Hold::drop` takes the lock, or a claim passing
    /// a going connection drop that connection (and its hold) before returning the new hold.
    #[cfg(all(test, feature = "shuttle"))]
    live_holds: HashMap<PathBuf, usize>,
    #[cfg(all(test, feature = "shuttle"))]
    two_holders: bool,
}

/// A key's entry: who holds its cell, and what is open of it meanwhile.
enum Entry<C: ?Sized, S, P> {
    /// A connection writes it, from its `OPEN` on: its cell, once open, which the daemon's work
    /// through it may share; and what to call as a
    /// writer waits for it, going ([`Hold::when_wanted`]).
    Writer { connection: Weak<C>, cell: Weak<S>, wanted: Option<Wanted> },
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
    /// The cell others have open, while one has it, which no writer holds meanwhile (`EBUSY`): none
    /// is made since shared volumes went (board 14527, board 14535).
    Shared(Weak<S>),
    /// A writer's cell its connection let go of, which the daemon's work through it, a fork's or
    /// a sync's, has still: closing, which writers wait for to close ([`CellTable::closed`]), with
    /// the writer's hurry callback (`wanted`, board 11530) preserved so a hurrying claim can still
    /// cut its uploader short.
    Closing(Weak<S>, Option<Wanted>),
    /// A cell kept open for forks (TASK-92.5).
    Kept(P),
}

/// What a connection holding a cell has done as a writer waits for it to let the cell go, its
/// VM process gone (TASK-216): a callback of its own, which outlives the connection, as letting
/// the cell go comes after its last reference does.
pub type Wanted = Arc<dyn Fn() + Send + Sync>;

/// Who asks to hold a cell.
enum Claim<C: ?Sized> {
    /// The daemon, to work on it.
    Daemon,
    /// The daemon, unless a connection not going writes it, which the daemon then works through.
    DaemonOrWriter,
    /// A connection, to write it.
    Writer(Weak<C>),
    /// cladefs: the daemon's FUSE mount, to attach it (TASK-475.3).
    #[cfg_attr(madsim, allow(dead_code))]
    Mount,
}

/// What a claim got: the cell held, with the cell kept open of it that a writer's claim took
/// out, to close, and, past [`Rules::one_holder`], the connection holding it, which it passed:
/// dropped once the table's lock is, as a [`Refused`]'s; or the connection writing it.
enum Claimed<C: ?Sized, P> {
    Held { kept: Option<P>, _passed: Option<Arc<C>> },
    Writer(Arc<C>),
}

/// Why a claim failed, with the connection holding the cell, if one does: dropped once the
/// table's lock is, as a connection, dropped last, lets its cell go, which takes the lock.
enum Refused<C: ?Sized> {
    /// A connection not going writes it: `EBUSY`.
    Busy(Arc<C>),
    /// A connection going holds it, or the daemon, which lets it go soon; and what a going
    /// connection has called as a writer waits for it, if anything.
    Wait(Option<Arc<C>>, Option<Wanted>),
    /// cladefs: the mount has it attached, which it lets go only as asked to (TASK-475.3); or, to a
    /// writer or the mount, others have it open: `EBUSY`, at once.
    Now,
}

/// A cell held, by a connection writing it or by the daemon working on it, which nothing else
/// holds until it is dropped: it lets the cell go then, and those waiting to hold it may. What is
/// open of the cell stays ([`Rules::open_outlives_holders`]): the cell its connection had open,
/// if others have it still.
#[must_use = "a cell held is let go as its hold is dropped"]
pub struct Hold<C: ?Sized, S, P> {
    table: CellTable<C, S, P>,
    key: PathBuf,
}

/// A cell held, with the cell kept open of it that the claim took out, to close, if one was:
/// what a writer's claim, and the mount's, get.
pub type HeldTaking<C, S, P> = (Hold<C, S, P>, Option<P>);

/// A cell a connection writes, or held by the daemon ([`CellTable::writer_or_hold`]).
pub enum Holding<C: ?Sized, S, P> {
    Writer(Arc<C>),
    Held(Hold<C, S, P>),
}

impl<S, P> Open<S, P> {
    /// As it is, but a cell none has open any more, which is nothing.
    fn live(self) -> Self {
        match self {
            Open::Shared(cell) | Open::Closing(cell, _) if cell.strong_count() == 0 => {
                Open::Nothing
            }
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
            Open::Shared(cell) | Open::Closing(cell, _) => cell.strong_count() == 0,
            Open::Kept(_) => false,
        }
    }

    /// The cell open, others' or closing, if one is or was.
    fn cell(&self) -> Option<&Weak<S>> {
        match self {
            Open::Shared(cell) | Open::Closing(cell, _) => Some(cell),
            Open::Nothing | Open::Kept(_) => None,
        }
    }
}

impl<C: Writer + ?Sized, S, P> Entry<C, S, P> {
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

impl<C: ?Sized, S, P> Entry<C, S, P> {
    /// What is open of the cell, its holder gone: the cell a writer had open, closing now.
    fn into_open(self) -> Open<S, P> {
        match self {
            Entry::Writer { cell, wanted, .. } => Open::Closing(cell, wanted).live(),
            Entry::Daemon(open) | Entry::Unheld(open) => open.live(),
            Entry::Mounted => Open::Nothing,
        }
    }

    /// The cell open of it, its writer's or others', if one is.
    fn instance(&self) -> Option<&Weak<S>> {
        match self {
            Entry::Writer { cell, .. } => Some(cell),
            Entry::Daemon(open) | Entry::Unheld(open) => open.cell(),
            Entry::Mounted => None,
        }
    }

    /// The cell kept open of the cell, if one is.
    fn kept(&mut self) -> Option<&mut P> {
        match self {
            Entry::Daemon(Open::Kept(kept)) | Entry::Unheld(Open::Kept(kept)) => Some(kept),
            _ => None,
        }
    }

    /// Whether it is others' cell, none having it any more: a stale entry.
    fn is_stale(&self) -> bool {
        matches!(self, Entry::Unheld(open) if open.cell().is_some() && open.is_closed())
    }
}

impl<C: Writer + ?Sized, S, P> Inner<C, S, P> {
    /// Holds the cell `key` names for `claim`, if it may now: unless a connection not going
    /// writes it, which fails, or a connection going holds it, or the daemon, which is waited for;
    /// or, to a writer or the mount, others have it open, which fails, or it is closing, which is
    /// waited for. A writer takes the cell kept open, to close.
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
        // A writer, and the mount, open a cell of their own, where others have none open, nor a
        // writer's closing.
        let (refuses_shared, waits_for_closing) = match claim {
            Claim::Writer(_) => (rules().writers_refuse_shared, rules().writers_wait_for_closing),
            Claim::Mount => (true, true),
            Claim::Daemon | Claim::DaemonOrWriter => (false, false),
        };
        let open = match self.entries.get(key) {
            Some(Entry::Daemon(open) | Entry::Unheld(open)) if !open.is_closed() => Some(open),
            _ => None,
        };
        match open {
            Some(Open::Shared(_)) if refuses_shared => return Err(Refused::Now),
            Some(Open::Closing(_, wanted)) if waits_for_closing => {
                return Err(Refused::Wait(None, wanted.clone()));
            }
            _ => {}
        }
        let open = self.entries.remove(key).map_or(Open::Nothing, Entry::into_open);
        let (entry, evicted) = match claim {
            Claim::Writer(connection) => {
                let (cell, evicted) = match open {
                    // Past the rules alone: a cell others have open, or closing.
                    Open::Shared(cell) | Open::Closing(cell, _) => (cell, None),
                    Open::Kept(kept) => (Weak::new(), Some(kept)),
                    Open::Nothing => (Weak::new(), None),
                };
                (Entry::Writer { connection: connection.clone(), cell, wanted: None }, evicted)
            }
            Claim::Daemon | Claim::DaemonOrWriter => (Entry::Daemon(open), None),
            Claim::Mount => match open {
                Open::Kept(kept) => (Entry::Mounted, Some(kept)),
                Open::Nothing | Open::Shared(_) | Open::Closing(_, _) => (Entry::Mounted, None),
            },
        };
        self.entries.insert(key.to_owned(), entry);
        #[cfg(all(test, feature = "shuttle"))]
        {
            let count = self.live_holds.entry(key.to_owned()).or_default();
            *count += 1;
            if *count > 1 {
                self.two_holders = true;
            }
        }
        Ok(Claimed::Held { kept: evicted, _passed: passed })
    }
}

impl<C: ?Sized, S, P> CellTable<C, S, P> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                waiting: HashMap::new(),
                #[cfg(all(test, feature = "shuttle"))]
                live_holds: HashMap::new(),
                #[cfg(all(test, feature = "shuttle"))]
                two_holders: false,
            })),
        }
    }

    /// The connection writing the cell `key` names, if one does, going or not.
    pub fn writer(&self, key: &Path) -> Option<Arc<C>> {
        match self.inner.lock().unwrap().entries.get(key)? {
            Entry::Writer { connection, .. } => connection.upgrade(),
            _ => None,
        }
    }

    /// The cell open at `key`, its writer's or others', if one is.
    pub fn instance(&self, key: &Path) -> Option<Arc<S>> {
        self.inner.lock().unwrap().entries.get(key)?.instance()?.upgrade()
    }

    /// `f`'s of the cell kept open at `key`, if one is: under the table's lock, which `f` takes
    /// for no longer than to look at it.
    pub fn kept<R>(&self, key: &Path, f: impl FnOnce(&mut P) -> R) -> Option<R> {
        self.inner.lock().unwrap().entries.get_mut(key)?.kept().map(f)
    }

    /// The cells open, writers' and others', by their keys.
    pub fn instances(&self) -> Vec<(PathBuf, Arc<S>)> {
        let inner = self.inner.lock().unwrap();
        (inner.entries.iter())
            .filter_map(|(key, entry)| Some((key.clone(), entry.instance()?.upgrade()?)))
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
    #[cfg(any(test, feature = "testing"))]
    pub fn is_waited_for(&self, key: &Path) -> bool {
        self.inner.lock().unwrap().waiting.contains_key(key)
    }

    /// Whether nothing holds a cell, nor has one open.
    #[cfg(any(test, feature = "testing"))]
    pub fn is_empty(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.entries.values().all(Entry::is_stale)
    }

    /// Whether two [`Hold`]s for any key ever overlapped under `inner`'s lock (board 10956).
    #[cfg(all(test, feature = "shuttle"))]
    pub fn two_holders(&self) -> bool {
        self.inner.lock().unwrap().two_holders
    }

    /// cladefs: the cell at `key`, a writer's that went, closing, is closed: those waiting for it may
    /// claim it ([`Open::Closing`]). Called as its last reference goes, its writer's connection
    /// having said so (`OpenCell::on_close`).
    pub fn closed(&self, key: &Path) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.retain(|_, entry| !entry.is_stale());
        if let Some(released) = inner.waiting.remove(key) {
            released.notify(usize::MAX);
        }
    }

    /// Lets the cell `key` names go, its hold dropped: what is open of it stays, but a cell none
    /// has open any more; those waiting for it alone are woken.
    fn release(&self, key: &Path) {
        let mut inner = self.inner.lock().unwrap();
        #[cfg(all(test, feature = "shuttle"))]
        if let Some(count) = inner.live_holds.get_mut(key) {
            *count -= 1;
            if *count == 0 {
                inner.live_holds.remove(key);
            }
        }
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

impl<C: Writer + ?Sized, S, P> CellTable<C, S, P> {
    /// Holds the cell `key` names, for the daemon: once its holder, if it has one, lets it go,
    /// if that holder is going, or is the daemon, waiting at most `wait` (for ever, if none);
    /// unless a connection not going writes it, which fails with `EBUSY`. What is open of it stays.
    /// It blocks: the table's tests', whose threads the models schedule; the daemon holds no thread
    /// as it waits ([`CellTable::hold_async`], doc-24).
    #[cfg(any(test, feature = "testing"))]
    pub fn hold(&self, key: &Path, wait: Option<Duration>) -> Result<Hold<C, S, P>, Errno> {
        let deadline = wait.map(|wait| Instant::now() + wait);
        match self.claim_blocking(key, &Claim::Daemon, deadline)? {
            Claimed::Held { .. } => Ok(self.held(key)),
            Claimed::Writer(_) => unreachable!("the daemon's claim got a writer"),
        }
    }

    /// Holds the cell `key` names, for the daemon, as [`CellTable::hold`] does, waiting at most
    /// `wait`: a future, which holds no thread while it waits (TASK-43.6).
    pub async fn hold_async(&self, key: &Path, wait: Duration) -> Result<Hold<C, S, P>, Errno> {
        match self.claim_async(key, Claim::Daemon, wait).await? {
            Claimed::Held { .. } => Ok(self.held(key)),
            Claimed::Writer(_) => unreachable!("the daemon's claim got a writer"),
        }
    }

    /// Holds the cell `key` names, for `connection` to write it, as [`CellTable::hold_async`]
    /// does; with the cell kept open of it, if one was, which the connection closes before it
    /// opens the cell. `EBUSY` at once while others have the cell open.
    pub async fn hold_to_write(
        &self,
        key: &Path,
        connection: Weak<C>,
        wait: Duration,
    ) -> Result<HeldTaking<C, S, P>, Errno> {
        match self.claim_async(key, Claim::Writer(connection), wait).await? {
            Claimed::Held { kept, .. } => Ok((self.held(key), kept)),
            Claimed::Writer(_) => unreachable!("a writer's claim got a writer"),
        }
    }

    /// The connection writing the cell `key` names, if one does that isn't going; or, if none
    /// does, the cell held for the daemon. `EBUSY` if a connection going holds it, or the
    /// daemon: one going would carry out no request of its guest's, which is gone (TASK-79.34).
    pub fn writer_or_hold(&self, key: &Path) -> Result<Holding<C, S, P>, Errno> {
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
    ) -> Result<Holding<C, S, P>, Errno> {
        let claimed = self.claim_async(key, Claim::DaemonOrWriter, wait).await?;
        Ok(self.holding(key, claimed))
    }

    /// Resizes the cell at `key` to `new_capacity` bytes: through its open writer if a connection
    /// writes it, else holding its path while `open_closed` opens it (taking ownership of the
    /// hold so cancellation still closes before releasing it), `capacity` checks that a closed
    /// cell is not shrunk (`EBUSY`), `set` updates its capacity, and `close` closes it before
    /// letting the hold go (TASK-44.6, TASK-79.34).
    pub async fn resize_cell<H, FutW, FutO, FutC>(
        &self,
        key: &Path,
        new_capacity: u64,
        resize_writer: impl FnOnce(Arc<C>, u64) -> FutW,
        open_closed: impl FnOnce(Option<Hold<C, S, P>>) -> FutO,
        capacity: impl FnOnce(&H) -> Result<(u64, u64), Errno>,
        set: impl AsyncFnOnce(&H, u64) -> Result<(), Errno>,
        close: impl FnOnce(H) -> FutC,
    ) -> Result<(), Errno>
    where
        C: Writer,
        FutW: std::future::Future<Output = Result<(), Errno>>,
        FutO: std::future::Future<Output = Result<H, Errno>>,
        FutC: std::future::Future<Output = ()>,
    {
        let hold = match rules().through_holder {
            true => match self.writer_or_hold(key)? {
                Holding::Writer(writer) => return resize_writer(writer, new_capacity).await,
                Holding::Held(hold) => Some(hold),
            },
            false => None,
        };
        let handle = open_closed(hold).await?;
        let set_result = match capacity(&handle) {
            Ok((current, _)) if new_capacity < current => Err(Errno::EBUSY),
            Ok(_) => set(&handle, new_capacity).await,
            Err(errno) => Err(errno),
        };
        close(handle).await;
        set_result
    }

    /// Acquires a handle to fork the cell at `key`: through its open writer if a connection writes
    /// it, else holding its path via [`Self::writer_or_hold_async`] and handing the hold to
    /// `from_hold` so the handle (or kept entry) releases it only after close (TASK-44.6).
    pub async fn fork_cell<H, FutW, FutH>(
        &self,
        key: &Path,
        wait: Duration,
        from_writer: impl FnOnce(Arc<C>) -> FutW,
        from_hold: impl FnOnce(Option<Hold<C, S, P>>) -> FutH,
    ) -> Result<H, Errno>
    where
        C: Writer,
        FutW: std::future::Future<Output = Result<H, Errno>>,
        FutH: std::future::Future<Output = Result<H, Errno>>,
    {
        let hold = match rules().fork_through_holder {
            true => match self.writer_or_hold_async(key, wait).await? {
                Holding::Writer(writer) => return from_writer(writer).await,
                Holding::Held(hold) => Some(hold),
            },
            false => None,
        };
        let hold = if rules().forks_hold_their_paths { hold } else { None };
        from_hold(hold).await
    }

    /// cladefs: holds the cell `key` names for the daemon's FUSE mount, to attach it (TASK-475.3),
    /// as [`CellTable::hold`] holds it for the daemon, waiting at most `wait`: `EBUSY` at once if a
    /// connection not going writes it, others have it open, or the mount has it already. With
    /// the cell kept open of it, if one was, to close. Until the hold is dropped, every claim of
    /// it fails with `EBUSY` at once, waiting for nothing: the mount lets it go only as it
    /// detaches its cell. Awaited, holding no thread (board 2946).
    #[cfg_attr(madsim, allow(dead_code))]
    pub async fn hold_mounted(
        &self,
        key: &Path,
        wait: Duration,
    ) -> Result<HeldTaking<C, S, P>, Errno> {
        match self.claim_async(key, Claim::Mount, wait).await? {
            Claimed::Held { kept, .. } => Ok((self.held(key), kept)),
            Claimed::Writer(_) => unreachable!("the mount's claim got a writer"),
        }
    }

    fn held(&self, key: &Path) -> Hold<C, S, P> {
        Hold { table: self.clone(), key: key.to_owned() }
    }

    fn holding(&self, key: &Path, claimed: Claimed<C, P>) -> Holding<C, S, P> {
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
    #[cfg(any(test, feature = "testing"))]
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

impl<C: ?Sized, S, P> Hold<C, S, P> {
    /// The key of the cell it holds.
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// The cell open at its key, if one is: its writer's, or others'.
    pub fn instance(&self) -> Option<Arc<S>> {
        self.table.instance(&self.key)
    }

    /// `cell` is the cell open at its key from now on, while one has it: the writer's holding it,
    /// or, held by the daemon, others' to share. `EBUSY`, and nothing changed, if a cell is open
    /// there, or a cell kept: what holds the cell opens it only where it found nothing open, or
    /// works on what it found ([`Hold::instance`]).
    ///
    /// Its methods change its entry only if it is its holder's, which nothing but a claim past
    /// [`Rules::one_holder`] takes from it, and only as they say, refusing otherwise: none panics,
    /// which, under the table's lock, would poison every cell's entry, as under Shuttle it would
    /// abort the run (TASK-55).
    pub fn open(&self, cell: &Arc<S>) -> Result<(), Errno> {
        let opened = Arc::downgrade(cell);
        let mut inner = self.table.inner.lock().unwrap();
        inner.entries.retain(|_, entry| !entry.is_stale());
        match inner.entries.get_mut(&self.key) {
            Some(Entry::Writer { cell, .. }) if cell.strong_count() == 0 => {
                *cell = opened;
                Ok(())
            }
            Some(Entry::Daemon(open)) if open.is_closed() => {
                *open = Open::Shared(opened);
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

impl<C: ?Sized, S, P> Drop for Hold<C, S, P> {
    fn drop(&mut self) {
        // Those waiting to hold it may.
        self.table.release(&self.key);
    }
}

/// The shrink a connection's guest was asked to take, and where its answer goes; and the resize of
/// its cell under way, which the next waits for ([`Shrinking::resize`], TASK-79.41).
pub struct Shrinking {
    asked: Mutex<Option<(u64, futures::channel::oneshot::Sender<bool>)>>,
    resizing: futures::lock::Mutex<()>,
}

impl Shrinking {
    pub fn new() -> Self {
        Self { asked: Mutex::new(None), resizing: futures::lock::Mutex::new(()) }
    }

    /// The guest's answer to the shrink to `capacity`: whether it took it. An answer to a shrink
    /// no longer waited for goes nowhere.
    pub fn answered(&self, capacity: u64, took: bool) {
        let mut shrinking = self.asked.lock().unwrap();
        if shrinking.as_ref().is_some_and(|(asked, _)| *asked == capacity) {
            let (_, answer) = shrinking.take().unwrap();
            // Its asker gone: nothing to tell.
            let _ = answer.send(took);
        }
    }

    /// Stops waiting for the guest's answer, its connection gone: the shrink fails as unanswered
    /// (TASK-79.34).
    pub fn stop(&self) {
        let stopped = self.asked.lock().unwrap().take();
        drop(stopped);
    }

    /// Whether a shrink waits for the guest's answer.
    #[cfg(any(test, feature = "testing"))]
    pub fn awaited(&self) -> bool {
        self.asked.lock().unwrap().is_some()
    }

    /// Sets a held cell's capacity to `capacity` bytes: grows it and tells the guest via `tell`;
    /// or asks the guest to shrink, waiting `wait` at most for its answer (`ETIMEDOUT`) or until
    /// its call's caller gives up (`ECONNABORTED`, [`crate::caller::gone`]), holding no thread
    /// meanwhile. Never below what the cell holds (`ENOSPC`). Serialized per connection so a grow
    /// cannot race between a shrink's answer and its commit (TASK-79.41).
    pub async fn resize<F>(
        &self,
        capacity: u64,
        wait: Duration,
        current: impl FnOnce() -> Result<(u64, u64), Errno>,
        set: impl FnOnce(u64) -> F,
        tell: impl Fn(u64, bool) -> Result<(), Errno>,
    ) -> Result<(), Errno>
    where
        F: std::future::Future<Output = Result<(), Errno>>,
    {
        let _resizing = match rules().resizes_one_at_a_time {
            true => Some(self.resizing.lock().await),
            false => None,
        };
        let (now, held) = current()?;
        if capacity < held {
            return Err(Errno::ENOSPC);
        }
        if capacity >= now || !rules().guest_agrees {
            set(capacity).await?;
            return tell(capacity, false);
        }
        let mut asked = Asked { tell: &tell, kept: now, settled: false };
        let mut waiting = Waiting::new(self, capacity);
        tell(capacity, true)?;
        // cladefs: a wait that changes nothing, which ends with its call's caller, whose call runs
        // to its end otherwise (board 5599): a shrink its caller gave up on stops waiting
        // (TASK-43.6), the guest told the capacity kept as `asked` goes.
        let took = {
            let answered = std::pin::pin!(within(wait, &mut waiting.answered));
            match futures::future::select(answered, std::pin::pin!(crate::caller::gone())).await {
                futures::future::Either::Left((took, _)) => Some(took),
                futures::future::Either::Right(((), _)) => None,
            }
        };
        drop(waiting);
        let Some(took) = took else {
            return Err(Errno::ECONNABORTED);
        };
        match took {
            Some(Ok(true)) => {
                set(capacity).await?;
                asked.settled = true;
                Ok(())
            }
            Some(Ok(false)) => {
                asked.settled = true;
                Err(Errno::ENOSPC)
            }
            // No answer in time, or its VM process gone without one.
            Some(Err(_)) | None => {
                log::warn!("cladefsd: no answer from the guest to a shrink to {capacity} bytes");
                Err(Errno::ETIMEDOUT)
            }
        }
    }
}

/// A shrink waiting for the guest's answer, in its connection's [`Shrinking`] until it stops
/// waiting, however it stops: answered, timed out, or its call dropped (TASK-43.6).
struct Waiting<'a> {
    shrinking: &'a Shrinking,
    answered: futures::channel::oneshot::Receiver<bool>,
}

impl<'a> Waiting<'a> {
    fn new(shrinking: &'a Shrinking, capacity: u64) -> Self {
        let (answer, answered) = futures::channel::oneshot::channel();
        *shrinking.asked.lock().unwrap() = Some((capacity, answer));
        Self { shrinking, answered }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut shrinking = self.shrinking.asked.lock().unwrap();
        // Unless a later shrink took its place.
        if shrinking.as_ref().is_some_and(|(_, answer)| answer.is_connected_to(&self.answered)) {
            shrinking.take();
        }
    }
}

/// A shrink the guest was asked to take, which, unless the cell took it or the guest refused it,
/// tells the guest the capacity the cell keeps as it is dropped, however it ends: unanswered in
/// time, its call dropped, or the cell's new capacity failing; so that the guest, having taken it
/// late, grows back (TASK-79.41).
struct Asked<'a, T: Fn(u64, bool) -> Result<(), Errno>> {
    tell: &'a T,
    kept: u64,
    settled: bool,
}

impl<T: Fn(u64, bool) -> Result<(), Errno>> Drop for Asked<'_, T> {
    fn drop(&mut self) {
        if !self.settled {
            // Which fails, logged, once the guest is gone.
            let _ = (self.tell)(self.kept, false);
        }
    }
}

/// The table under Shuttle: a VM's connection writing a cell, whose VM process goes; as it goes,
/// the VM restored from its snapshot opening the cell to write it; a cell attaching the cell as a
/// base, sharing what is open of it, before and after its writer went; and a fork of the cell at any
/// kept cell, which keeps it open as a kept cell if nothing has it open. Every interleaving of them
/// Shuttle's scheduler chooses (TASK-160.8). Waits are unbounded, so that a holder that never
/// wakes those waiting is a deadlock, which Shuttle reports, rather than a wait of `CLOSING_WAIT`.
/// Invariants: one holder at a time; the cell open once at a time, as a writer's, a base's or a
/// kept cell; no writer writing a cell others have open. Each rule turned off fails.
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests {
    use super::*;
    use crate::shuttle_testing::{config, fails_with, runs};
    use shuttle::future::block_on;
    use shuttle::scheduler::{PctScheduler, RandomScheduler};
    use shuttle::sync::atomic::{AtomicBool, Ordering};
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
            // Closing, as its connection's is (`OpenCell::on_close`): those waiting for
            // it told once its last reference goes, a base's that shares it perhaps.
            let table = hold.hold().table.clone();
            *cell.on_close.lock().unwrap() = Some(Box::new(move || table.closed(Path::new(KEY))));
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
    /// once, and whether ever two; whether a writer ever wrote a cell others had open. Checked once
    /// the run is done: a task panicking unwinds through the holds it has, whose drops take
    /// Shuttle's locks, which fail in a failed run with panics that abort (TASK-55). Not Shuttle's
    /// atomics, whose every access the scheduler may switch threads at: a cell's closing is counted
    /// as its last holder drops it, rather than after another thread found it gone and opened it
    /// anew, which the table doesn't order: the cell's own lock of its files does
    /// (`OpenCell::open`).
    #[derive(Default)]
    struct Counts {
        open: std::sync::atomic::AtomicUsize,
        most_open: std::sync::atomic::AtomicUsize,
        holders: std::sync::atomic::AtomicUsize,
        two_holders: std::sync::atomic::AtomicBool,
        wrote_shared: std::sync::atomic::AtomicBool,
    }

    /// The cell, opened: counted open until it is dropped, closed, which it then says, if told to.
    /// A kept cell's says it is closed, dropping what does.
    struct ModelCell {
        counts: Arc<Counts>,
        _closing: Option<mpsc::Sender<()>>,
        on_close: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl ModelCell {
        fn open(counts: &Arc<Counts>) -> Arc<Self> {
            Self::opened(counts, None)
        }

        fn opened(counts: &Arc<Counts>, closing: Option<mpsc::Sender<()>>) -> Arc<Self> {
            let others = counts.open.fetch_add(1, Ordering::SeqCst);
            counts.most_open.fetch_max(others + 1, Ordering::SeqCst);
            let on_close = std::sync::Mutex::new(None);
            Arc::new(ModelCell { counts: counts.clone(), _closing: closing, on_close })
        }
    }

    impl Drop for ModelCell {
        fn drop(&mut self) {
            self.counts.open.fetch_sub(1, Ordering::SeqCst);
            if let Some(closed) = self.on_close.get_mut().unwrap().take() {
                closed();
            }
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
    struct Counted {
        counts: Arc<Counts>,
        _hold: Hold<Conn, ModelCell, Kept>,
    }

    impl Counted {
        fn new(counts: &Arc<Counts>, hold: Hold<Conn, ModelCell, Kept>) -> Self {
            if counts.holders.fetch_add(1, Ordering::SeqCst) > 0 {
                counts.two_holders.store(true, Ordering::SeqCst);
            }
            Self { counts: counts.clone(), _hold: hold }
        }

        fn hold(&self) -> &Hold<Conn, ModelCell, Kept> {
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

    impl Drop for Counted {
        fn drop(&mut self) {
            self.counts.holders.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// What a connection writing the cell has: its cell, closed before its hold lets it go
    /// ([`Conn`]).
    struct Writing {
        cell: Arc<ModelCell>,
        hold: Counted,
    }

    /// Opens the cell for `conn` to write, as a connection does (`cladefsd::server`): holding it,
    /// closing the cell kept of it, and opening it: `EBUSY` while others have it open. The
    /// connection has it from then on. A cell found open, which only a claim past
    /// [`Rules::writers_refuse_shared`] finds, is counted as written while shared.
    fn write(table: &'static Table, counts: &Arc<Counts>, conn: &Arc<Conn>) -> Result<(), Errno> {
        let held =
            block_on(table.hold_to_write(Path::new(KEY), Arc::downgrade(conn), Duration::MAX));
        let (hold, kept) =
            held.inspect_err(|_| reached("a writer found the cell open for others"))?;
        let hold = Counted::new(counts, hold);
        if kept.is_some() {
            reached("a writer closed a cell kept");
        }
        close(kept);
        let cell = match hold.hold().instance() {
            Some(cell) => {
                counts.wrote_shared.store(true, Ordering::SeqCst);
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

    /// Attaches the cell as a base, as the daemon does (`cladefsd::cells::base`): the cell open,
    /// its writer's or others', shared; or, held by the daemon, its kept cell closed, the cell
    /// opened, for others to share.
    fn attach(table: &Table, counts: &Arc<Counts>) -> Result<Arc<ModelCell>, Errno> {
        if let Some(cell) = table.instance(Path::new(KEY)) {
            reached("a base shared the cell open");
            return Ok(cell);
        }
        let hold = Counted::new(counts, table.hold(Path::new(KEY), None)?);
        close(hold.hold().take_kept());
        if let Some(cell) = hold.hold().instance() {
            return Ok(cell);
        }
        reached("a base opened the cell");
        let cell = ModelCell::open(counts);
        hold.open(&cell)?;
        Ok(cell)
    }

    /// Forks the cell, as the daemon does (`cladefsd::cells::fork`): through its writer, or the
    /// cell others have open, live; or the cell kept, or else the cell opened, kept open as a kept
    /// cell, which it holds while it forks it, as a restore's fork does, its hold let go.
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

    /// The run.
    fn run() {
        let table: &'static Table = Box::leak(Box::new(CellTable::new()));
        let counts = Arc::new(Counts::default());
        let (wrote_tx, wrote) = mpsc::channel::<()>();
        let (going_tx, going) = mpsc::channel::<()>();
        // That the first went, to the base.
        let (gone_tx, gone) = mpsc::channel::<()>();

        // The VM's connection, whose VM process goes while it writes the cell.
        let first = {
            let counts = counts.clone();
            thread::spawn(move || {
                let conn = Conn::new();
                // Refused only if another holder took the cell from under it, which the run's
                // check of the holders reports.
                if let Err(refused) = write(table, &counts, &conn) {
                    let two = counts.two_holders.load(Ordering::SeqCst) || table.two_holders();
                    assert!(two, "the first writer was refused: {refused:?}");
                }
                wrote_tx.send(()).unwrap();
                conn.going.store(true, Ordering::SeqCst);
                going_tx.send(()).unwrap();
                // The last of it, unless a claim of the cell has it as it meets it: it lets the
                // cell go then, outside the table's lock.
                drop(conn);
                // Unheard by a base that attached nothing.
                let _ = gone_tx.send(());
            })
        };
        // The VM restored from its snapshot, opening the cell as the first goes: it gets it
        // once the first lets it go, unless a base has the cell open.
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
        // A cell attaching the cell as a base, once the first wrote it, until the first went.
        let base = {
            let counts = counts.clone();
            thread::spawn(move || {
                wrote.recv().unwrap();
                if let Ok(cell) = attach(table, &counts) {
                    let _ = gone.recv();
                    drop(cell);
                }
            })
        };
        // A fork of the cell, at any kept cell.
        let forking = {
            let counts = counts.clone();
            thread::spawn(move || fork(table, &counts).unwrap())
        };
        for thread in [first, second, base, forking] {
            thread.join().unwrap();
        }
        // The kept cell a fork kept, closed, as it is once no fork forks it.
        let hold = table.hold(Path::new(KEY), None).unwrap();
        close(hold.take_kept());
        drop(hold);
        let two = counts.two_holders.load(Ordering::SeqCst) || table.two_holders();
        assert!(!two, "two holders hold the cell at once");
        let most = counts.most_open.load(Ordering::SeqCst);
        assert!(most <= 1, "the cell is open twice at once: {most} at most");
        let shared = counts.wrote_shared.load(Ordering::SeqCst);
        assert!(!shared, "a writer wrote a cell others have open");
        assert!(table.is_empty(), "the cell is held or open once all let it go");
    }

    fn randomly() {
        Runner::new(RandomScheduler::new(runs(3000)), config()).run(run);
    }

    fn by_pct() {
        Runner::new(PctScheduler::new(3, runs(3000)), config()).run(run);
    }

    /// What runs reach: a cell shared by a base, a writer refused it, kept cells kept and closed.
    const WAYS: &[&str] = &[
        "a writer found the cell open for others",
        "a writer closed a cell kept",
        "a kept closed waited for a fork of it",
        "a claim waited for a writer gone",
        "a base shared the cell open",
        "a base opened the cell",
        "a fork kept a kept",
    ];

    #[test]
    fn writers_going_randomly() {
        set_rules(Rules::ALL);
        reaching(WAYS, randomly);
    }

    #[test]
    fn writers_going_by_pct() {
        set_rules(Rules::ALL);
        reaching(WAYS, by_pct);
    }

    /// Runs the schedules with `rules`, expecting one to fail with a panic saying `expected`.
    fn fails_without(rules: Rules, expected: &str) {
        set_rules(rules);
        fails_with(expected, randomly);
    }

    #[test]
    fn needs_one_holder() {
        fails_without(Rules { one_holder: false, ..Rules::ALL }, "two holders hold the cell");
    }

    /// The resize that let the cell go without waking those waiting for it (fixed by having it
    /// release through `release`): they waited `CLOSING_WAIT` out.
    #[test]
    fn needs_releases_to_wake() {
        fails_without(Rules { release_wakes: false, ..Rules::ALL }, "deadlock!");
    }

    /// A writer's cell a base shares, forgotten as its writer goes, is opened anew by the next.
    #[test]
    fn needs_what_is_open_to_outlive_its_holders() {
        let rules = Rules { open_outlives_holders: false, ..Rules::ALL };
        fails_without(rules, "open twice at once");
    }

    /// A writer that holds the cell a base shares writes what the base reads.
    #[test]
    fn needs_writers_to_refuse_cells_open_for_others() {
        let rules = Rules { writers_refuse_shared: false, ..Rules::ALL };
        fails_without(rules, "a writer wrote a cell others have open");
    }

    /// A writer that holds the cell its writer that went left to a base sharing it writes what
    /// the base reads.
    #[test]
    fn needs_writers_to_wait_for_cells_closing() {
        let rules = Rules { writers_wait_for_closing: false, ..Rules::ALL };
        fails_without(rules, "a writer wrote a cell others have open");
    }

    /// Resize and fork request interleavings migrated from `cell_requests.rs` (TASK-44.6,
    /// TASK-79.34, TASK-79.41, board 11701), exercising `CellTable::resize_cell`,
    /// `CellTable::fork_cell`, and `Shrinking::resize` directly without wrapper traits.
    mod requests {
        use super::super::*;
        use crate::shuttle_testing::{config, fails_with, runs};
        use shuttle::future::block_on;
        use shuttle::scheduler::{PctScheduler, RandomScheduler};
        use shuttle::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        use shuttle::sync::mpsc;
        use shuttle::{Runner, thread};
        use std::sync::Arc;

        const KEY: &str = "/cell";
        const CAPACITY: u64 = 8;
        const GUEST_HOLDS: u64 = 4;

        struct Model {
            capacity: AtomicU64,
            ordered: AtomicU64,
            durable: AtomicU64,
            open: AtomicUsize,
            most_open: AtomicUsize,
        }

        impl Model {
            fn new() -> Arc<Self> {
                Arc::new(Model {
                    capacity: CAPACITY.into(),
                    ordered: 0.into(),
                    durable: 0.into(),
                    open: 0.into(),
                    most_open: 0.into(),
                })
            }

            fn open(&self) {
                let others = self.open.fetch_add(1, Ordering::SeqCst);
                self.most_open.fetch_max(others + 1, Ordering::SeqCst);
            }

            fn opened_once_at_a_time(&self) {
                let most = self.most_open.load(Ordering::SeqCst);
                assert!(most <= 1, "the cell is open twice at once: {most} at most");
            }

            fn close(&self) {
                self.open.fetch_sub(1, Ordering::SeqCst);
            }
        }

        type Table = CellTable<Conn, (), ()>;
        type ModelHold = Hold<Conn, (), ()>;

        enum View {
            Live(Arc<Model>),
            Here(Arc<Model>, Option<ModelHold>),
        }

        impl View {
            fn forked(&self) -> u64 {
                match self {
                    View::Live(model) => model.ordered.load(Ordering::SeqCst),
                    View::Here(model, _) => model.durable.load(Ordering::SeqCst),
                }
            }

            fn close(self) {
                if let View::Here(model, held) = self {
                    model.close();
                    drop(held);
                }
            }
        }

        struct ModelGuest {
            view: AtomicU64,
            asks: std::sync::Mutex<Option<mpsc::Sender<u64>>>,
        }

        impl ModelGuest {
            fn tell(&self, capacity: u64, ask: bool) -> Result<(), Errno> {
                if ask {
                    let asks = self.asks.lock().unwrap();
                    asks.as_ref().ok_or(Errno::EIO)?.send(capacity).map_err(|_| Errno::EIO)?;
                    return Ok(());
                }
                assert!(capacity >= GUEST_HOLDS, "the guest was shrunk without agreeing");
                self.view.store(capacity, Ordering::SeqCst);
                Ok(())
            }
        }

        struct Conn {
            model: Arc<Model>,
            guest: ModelGuest,
            shrinking: Shrinking,
            going: AtomicBool,
        }

        impl Conn {
            fn new(model: Arc<Model>, asks: Option<mpsc::Sender<u64>>) -> Arc<Self> {
                Arc::new(Conn {
                    model,
                    guest: ModelGuest { view: CAPACITY.into(), asks: asks.into() },
                    shrinking: Shrinking::new(),
                    going: false.into(),
                })
            }

            async fn resize(&self, capacity: u64) -> Result<(), Errno> {
                let model = &self.model;
                self.shrinking
                    .resize(
                        capacity,
                        Duration::MAX,
                        || Ok((model.capacity.load(Ordering::SeqCst), 1)),
                        |cap| async move {
                            thread::yield_now();
                            model.capacity.store(cap, Ordering::SeqCst);
                            Ok(())
                        },
                        |cap, ask| self.guest.tell(cap, ask),
                    )
                    .await
            }
        }

        impl Writer for Conn {
            fn going(&self) -> bool {
                self.going.load(Ordering::SeqCst)
            }
        }

        fn write(table: &Table, conn: &Arc<Conn>) -> ModelHold {
            let held = table.hold_to_write(Path::new(KEY), Arc::downgrade(conn), Duration::MAX);
            block_on(held).unwrap().0
        }

        fn answer(conn: Arc<Conn>, asks: mpsc::Receiver<u64>) {
            while let Ok(capacity) = asks.recv() {
                let took = capacity >= GUEST_HOLDS;
                if took {
                    conn.guest.view.store(capacity, Ordering::SeqCst);
                }
                conn.shrinking.answered(capacity, took);
            }
        }

        async fn resize_through_table(
            table: &Table,
            key: &Path,
            capacity: u64,
            model: &Arc<Model>,
        ) -> Result<(), Errno> {
            table
                .resize_cell(
                    key,
                    capacity,
                    |writer, cap| async move { writer.resize(cap).await },
                    |hold| {
                        let model = Arc::clone(model);
                        async move {
                            model.open();
                            Ok((model, hold))
                        }
                    },
                    |(model, _)| Ok((model.capacity.load(Ordering::SeqCst), 1)),
                    async |(model, _), cap| {
                        thread::yield_now();
                        model.capacity.store(cap, Ordering::SeqCst);
                        Ok(())
                    },
                    |(model, hold)| async move {
                        model.close();
                        drop(hold);
                    },
                )
                .await
        }

        async fn fork_through_table(
            table: &Table,
            key: &Path,
            model: &Arc<Model>,
        ) -> Result<View, Errno> {
            table
                .fork_cell(
                    key,
                    Duration::MAX,
                    |writer| async move { Ok(View::Live(writer.model.clone())) },
                    |hold| {
                        let model = Arc::clone(model);
                        async move {
                            model.open();
                            Ok(View::Here(model, hold))
                        }
                    },
                )
                .await
        }

        fn run_with() {
            let key = Path::new(KEY);
            let table: &'static Table = Box::leak(Box::new(CellTable::new()));
            let model = Model::new();
            let committed = Arc::new(AtomicU64::new(0));
            let (opened_tx, opened) = mpsc::channel::<()>();
            let (resized_tx, resized) = mpsc::channel::<()>();
            let (going_tx, going) = mpsc::channel::<()>();

            let first = {
                let (model, committed) = (model.clone(), committed.clone());
                thread::spawn(move || {
                    let (asks_tx, asks) = mpsc::channel();
                    let conn = Conn::new(model.clone(), Some(asks_tx));
                    let held = write(table, &conn);
                    model.open();
                    let guest = {
                        let conn = conn.clone();
                        thread::spawn(move || answer(conn, asks))
                    };
                    opened_tx.send(()).unwrap();
                    for _ in 0..2 {
                        let epoch = model.ordered.fetch_add(1, Ordering::SeqCst) + 1;
                        committed.store(epoch, Ordering::SeqCst);
                        thread::yield_now();
                    }
                    resized.recv().unwrap();
                    model.opened_once_at_a_time();
                    let capacity = model.capacity.load(Ordering::SeqCst);
                    let view = conn.guest.view.load(Ordering::SeqCst);
                    assert_eq!(capacity, view, "the cell's capacity and its guest's view differ");
                    conn.going.store(true, Ordering::SeqCst);
                    going_tx.send(()).unwrap();
                    model.durable.store(model.ordered.load(Ordering::SeqCst), Ordering::SeqCst);
                    conn.guest.asks.lock().unwrap().take();
                    guest.join().unwrap();
                    model.close();
                    drop(held);
                })
            };

            let resizer = {
                let model = model.clone();
                thread::spawn(move || {
                    opened.recv().unwrap();
                    for (capacity, expected) in [(12, Ok(())), (6, Ok(())), (3, Err(Errno::ENOSPC))]
                    {
                        let resized = block_on(resize_through_table(table, key, capacity, &model));
                        model.opened_once_at_a_time();
                        assert_eq!(resized, expected, "resizing to {capacity}");
                    }
                    resized_tx.send(()).unwrap();
                })
            };

            let fork = {
                let (model, committed) = (model.clone(), committed.clone());
                thread::spawn(move || {
                    let before = committed.load(Ordering::SeqCst);
                    let forking = block_on(fork_through_table(table, key, &model)).unwrap();
                    let forked = forking.forked();
                    forking.close();
                    model.opened_once_at_a_time();
                    assert!(forked >= before, "the fork misses a commit made before it");
                })
            };

            let second = {
                let model = model.clone();
                thread::spawn(move || {
                    going.recv().unwrap();
                    let conn = Conn::new(model.clone(), None);
                    let held = write(table, &conn);
                    model.open();
                    model.close();
                    drop(held);
                })
            };
            for thread in [first, resizer, fork, second] {
                thread.join().unwrap();
            }
            model.opened_once_at_a_time();
            assert!(table.is_empty(), "the cell is held once all let it go");
        }

        struct ClosedBehind(Option<(Arc<Model>, Option<ModelHold>)>);

        impl Drop for ClosedBehind {
            fn drop(&mut self) {
                if let Some((model, held)) = self.0.take() {
                    thread::spawn(move || {
                        model.close();
                        drop(held);
                    });
                }
            }
        }

        fn cancelled_fork() {
            let key = Path::new(KEY);
            let table: &'static Table = Box::leak(Box::new(CellTable::new()));
            let model = Model::new();
            let (forked_tx, forked) = mpsc::channel::<()>();
            let fork = {
                let model = model.clone();
                thread::spawn(move || {
                    let forking = block_on(table.fork_cell(
                        key,
                        Duration::MAX,
                        |_| async { unreachable!() },
                        |hold| {
                            let model = Arc::clone(&model);
                            async move {
                                model.open();
                                Ok(ClosedBehind(Some((model, hold))))
                            }
                        },
                    ))
                    .unwrap();
                    forked_tx.send(()).unwrap();
                    drop(forking);
                })
            };
            let opening = {
                let model = model.clone();
                thread::spawn(move || {
                    forked.recv().unwrap();
                    let conn = Conn::new(model.clone(), None);
                    let held = write(table, &conn);
                    model.open();
                    model.close();
                    drop(held);
                })
            };
            fork.join().unwrap();
            opening.join().unwrap();
            model.opened_once_at_a_time();
        }

        #[test]
        fn cancelled_forks_let_their_paths_go_once_closed_randomly() {
            *RULES.lock().unwrap() = Rules::ALL;
            Runner::new(RandomScheduler::new(runs(2000)), config()).run(cancelled_fork);
        }

        #[test]
        fn cancelled_forks_let_their_paths_go_once_closed_by_pct() {
            *RULES.lock().unwrap() = Rules::ALL;
            Runner::new(PctScheduler::new(3, runs(2000)), config()).run(cancelled_fork);
        }

        #[test]
        fn needs_forks_to_hold_their_paths() {
            *RULES.lock().unwrap() = Rules { forks_hold_their_paths: false, ..Rules::ALL };
            fails_with("open twice at once", || {
                Runner::new(RandomScheduler::new(runs(2000)), config()).run(cancelled_fork);
            });
        }

        fn resizes_at_once() {
            let key = Path::new(KEY);
            let table: &'static Table = Box::leak(Box::new(CellTable::new()));
            let model = Model::new();
            let (asks_tx, asks) = mpsc::channel();
            let conn = Conn::new(model.clone(), Some(asks_tx));
            let held = write(table, &conn);
            let guest = {
                let conn = conn.clone();
                thread::spawn(move || answer(conn, asks))
            };
            let resizers: Vec<_> = [12, 6]
                .into_iter()
                .map(|capacity| {
                    let model = model.clone();
                    thread::spawn(move || {
                        block_on(resize_through_table(table, key, capacity, &model))
                    })
                })
                .collect();
            for resizer in resizers {
                assert_eq!(resizer.join().unwrap(), Ok(()));
            }
            conn.guest.asks.lock().unwrap().take();
            guest.join().unwrap();
            let capacity = model.capacity.load(Ordering::SeqCst);
            let view = conn.guest.view.load(Ordering::SeqCst);
            assert_eq!(capacity, view, "the cell's capacity and its guest's view differ");
            drop(held);
        }

        #[test]
        fn resizes_at_once_agree_randomly() {
            *RULES.lock().unwrap() = Rules::ALL;
            Runner::new(RandomScheduler::new(runs(2000)), config()).run(resizes_at_once);
        }

        #[test]
        fn resizes_at_once_agree_by_pct() {
            *RULES.lock().unwrap() = Rules::ALL;
            Runner::new(PctScheduler::new(3, runs(2000)), config()).run(resizes_at_once);
        }

        #[test]
        fn needs_resizes_one_at_a_time() {
            *RULES.lock().unwrap() = Rules { resizes_one_at_a_time: false, ..Rules::ALL };
            fails_with("the cell's capacity and its guest's view differ", || {
                Runner::new(RandomScheduler::new(runs(2000)), config()).run(resizes_at_once);
            });
        }

        #[test]
        fn requests_race_opens_and_closes_randomly() {
            *RULES.lock().unwrap() = Rules::ALL;
            Runner::new(RandomScheduler::new(runs(2000)), config()).run(run_with);
        }

        #[test]
        fn requests_race_opens_and_closes_by_pct() {
            *RULES.lock().unwrap() = Rules::ALL;
            Runner::new(PctScheduler::new(3, runs(2000)), config()).run(run_with);
        }

        #[test]
        fn needs_resizes_through_the_holder() {
            *RULES.lock().unwrap() = Rules { through_holder: false, ..Rules::ALL };
            fails_with("open twice at once", || {
                Runner::new(RandomScheduler::new(runs(2000)), config()).run(run_with);
            });
        }

        #[test]
        fn needs_the_guest_to_agree() {
            *RULES.lock().unwrap() = Rules { guest_agrees: false, ..Rules::ALL };
            fails_with("shrunk without agreeing", || {
                Runner::new(RandomScheduler::new(runs(2000)), config()).run(run_with);
            });
        }

        #[test]
        fn needs_forks_through_the_holder() {
            *RULES.lock().unwrap() = Rules { fork_through_holder: false, ..Rules::ALL };
            fails_with("open twice at once", || {
                Runner::new(RandomScheduler::new(runs(2000)), config()).run(run_with);
            });
        }

        #[test]
        fn needs_one_holder() {
            *RULES.lock().unwrap() = Rules { one_holder: false, ..Rules::ALL };
            fails_with("open twice at once", || {
                Runner::new(RandomScheduler::new(runs(2000)), config()).run(run_with);
            });
        }
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
    fn write(
        table: &Table,
        key: &Path,
        connection: &Arc<Connection>,
    ) -> Result<HeldTaking<Connection, &'static str, &'static str>, Errno> {
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

    /// A writer's cell outlives its writer while the daemon's work has it: closing, which the
    /// daemon works through, and which the next writer, and the mount, wait for, woken as it
    /// closes ([`CellTable::closed`]); gone, it is nothing, and the next opens its own.
    #[test]
    fn cells_closing_are_waited_for_by_writers() {
        let table = Table::new();
        let key = Path::new("cell");
        let (writer, next) = (connection(false), connection(false));
        let (held, _) = write(&table, key, &writer).unwrap();
        let cell = Arc::new("writer's");
        held.open(&cell).unwrap();
        let working = table.instance(key).unwrap();
        writer.0.store(true, Ordering::Relaxed);
        drop((held, cell));
        let held = table.hold(key, None).unwrap();
        assert_eq!(held.instance().map(|cell| *cell), Some("writer's"));
        drop(held);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let short = Duration::from_millis(10);
        let refused = runtime.block_on(table.hold_to_write(key, Arc::downgrade(&next), short));
        assert_eq!(refused.err(), Some(Errno::EBUSY));
        assert_eq!(runtime.block_on(table.hold_mounted(key, short)).err(), Some(Errno::EBUSY));
        let held = runtime.block_on(async {
            let wait = Duration::from_secs(60);
            let mut holding = std::pin::pin!(table.hold_to_write(key, Arc::downgrade(&next), wait));
            assert!(futures::poll!(&mut holding).is_pending());
            drop(working);
            table.closed(key);
            holding.await.unwrap()
        });
        assert_eq!(held.0.instance(), None);
        drop(held);
        assert!(table.is_empty());
    }

    /// A cell the daemon opened for others to share, a base attached to cells, outlives its hold
    /// while one has it, which writers and the mount are refused at once (`EBUSY`); gone, the next
    /// writer opens its own.
    #[test]
    fn cells_shared_are_refused_to_writers() {
        let table = Table::new();
        let key = Path::new("base");
        let held = table.hold(key, None).unwrap();
        let cell = Arc::new("base");
        held.open(&cell).unwrap();
        drop(held);
        let writer = connection(false);
        let wait = Duration::from_secs(60);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let started = Instant::now();
        let refused = runtime.block_on(table.hold_to_write(key, Arc::downgrade(&writer), wait));
        assert_eq!(refused.err(), Some(Errno::EBUSY));
        assert_eq!(runtime.block_on(table.hold_mounted(key, wait)).err(), Some(Errno::EBUSY));
        assert!(started.elapsed() < Duration::from_secs(5), "waited {:?}", started.elapsed());
        drop(cell);
        assert!(table.is_empty());
        let (held, _) = write(&table, key, &writer).unwrap();
        assert_eq!(held.instance(), None);
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
    /// holder works on what it found, rather than open it twice.
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
        assert!(Arc::ptr_eq(&held.instance().unwrap(), &first));
        drop((held, first));
        let held = table.hold(key, None).unwrap();
        held.keep("kept").unwrap();
        assert_eq!(held.open(&second), Err(Errno::EBUSY));
        assert_eq!(held.instance(), None);
        assert_eq!(held.take_kept(), Some("kept"));
        held.open(&second).unwrap();
        assert!(Arc::ptr_eq(&held.instance().unwrap(), &second));
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

    /// TASK-475.3: the mount attaches no cell a writer writes, nor one followers have open; it
    /// takes the cell kept open of a cell, to close, as a writer does.
    #[test]
    fn the_mount_attaches_cells_nothing_else_has() {
        let table = Table::new();
        let key = Path::new("cell");
        let writer = connection(false);
        let written = write(&table, key, &writer).unwrap().0;
        assert_eq!(block_on(table.hold_mounted(key, Duration::ZERO)).err(), Some(Errno::EBUSY));
        let cell = Arc::new("cell");
        written.open(&cell).unwrap();
        drop(written);
        assert_eq!(block_on(table.hold_mounted(key, Duration::ZERO)).err(), Some(Errno::EBUSY));
        drop(cell);
        let other = Path::new("other");
        let held = table.hold(other, None).unwrap();
        held.keep("kept").unwrap();
        drop(held);
        let (_mounted, kept) = block_on(table.hold_mounted(other, Duration::ZERO)).unwrap();
        assert_eq!(kept, Some("kept"));
        assert!(block_on(table.hold_mounted(key, Duration::ZERO)).is_ok());
    }

    /// TASK-79.41: a shrink its guest leaves unanswered tells the guest the cell's capacity as it
    /// stays, as an answer coming after, the guest having taken the shrink, shrinks nothing.
    #[test]
    fn shrinks_unanswered_tell_the_guest_the_capacity_kept() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let told = std::sync::Mutex::new(Vec::new());
        let shrinking = Shrinking::new();
        let wait = Duration::from_millis(10);
        let resized = runtime.block_on(shrinking.resize(
            6,
            wait,
            || Ok((8, 1)),
            |_| async { panic!("the guest never took the shrink") },
            |capacity, ask| {
                told.lock().unwrap().push((capacity, ask));
                Ok(())
            },
        ));
        assert_eq!(resized, Err(Errno::ETIMEDOUT));
        assert_eq!(*told.lock().unwrap(), [(6, true), (8, false)]);
    }
}
