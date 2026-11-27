// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: mirrors (backlog doc-23, Mirrors; TASK-633.8). A version of a cell forked into another
//! bucket is copied there, byte for byte at the same keys, so that the children there read it from
//! their own bucket, cheaply, and with the cell's own bucket out of reach. One mirror serves every
//! child of the version in that bucket.
//!
//! A mirror of `parent`'s version at `at` into this bucket, B, from the parent's, A, in the order
//! `specs/buckets` checks:
//!
//! 1. In B, a best-effort prefix `LIST` on [`layout::pins`] checks for an active (non-stale) pin
//!    from another mirror run to avoid duplicate cross-region copy egress ([`Mirrored::UnderWay`]).
//!    If none stands, this run draws a fresh random run ID and creates its own write-once **pin**
//!    ([`layout::pin`], `PutMode::Create`), a reference under the parent's prefix that keeps what
//!    it copies from B's collector while it copies ([`crate::collector::Collector::collect_copy`]).
//!    A pin unrefreshed for [`PIN_STALE`] is stale, its host dead: another mirror run proceeds with
//!    its own fresh run ID, and B's collector reaps stale pins.
//! 2. Its **marker slot claimed in B** ([`layout::mirror`]): if already
//!    [`layout::MirrorState::Marked`] (marked by a mirror begun before), on to step 7
//!    (`mirrorResumesMarked`), with no hold: the marking mirror's stands until B's collector
//!    deletes all holds for this bucket via prefix `LIST`, with the marker, and one written again
//!    after that would stand for good (`mirrorHoldsUnmarked`). Otherwise this run claims the slot
//!    in [`layout::MirrorState::Copying`] (`PutMode::Create`, or conditional `PutMode::Update`
//!    over a prior `Copying` slot whose owner's pin is absent or stale, board 9498).
//! 3. In A, its own write-once **hold** ([`layout::hold`], `PutMode::Create`), a reference under
//!    the parent's prefix there (`mirrorHolds`), before it reads the head: it keeps the version
//!    whole in A, and the parent's cell from being removed with what the version inherits from
//!    other cells, which the mirror does not copy, while the copy stands. B's collector deletes
//!    all holds matching [`layout::holds`] as the copy goes.
//! 4. The parent's **head read in A**, naming the version, or the mirror is given up, its own hold
//!    and pin dropped (`mirrorChecks`): [`Mirrored::Unnamed`].
//! 5. **Every object of the version under the parent's prefix copied** into B, create-only: the
//!    checkpoint at the version, what recovering it read and the objects its extents are in, read
//!    where the router sends them. An object gone from A gives the mirror up, whatever order the
//!    copies end in (board 8439). Its pin refreshed as it goes, by its host's clock (board 8469),
//!    the mirror given up if its pin was reaped as stale.
//! 6. In B, the **marker** ([`layout::mirror`]) transitioned conditionally (`PutMode::Update`)
//!    from `Copying` to [`layout::MirrorState::Marked`] once every object is copied
//!    (`markerAfterCopy`, `marksOwn`, board 9498): if another run took over `Copying` after this
//!    run's pin went stale or B's collector deleted the copy meanwhile, the conditional update is
//!    rejected however late it lands. From then on, reads of the version in B are served there
//!    ([`crate::object_bucket::ObjectBucket`]).
//! 7. **The children of B referring in A listed**, after the marker (`mirrorListsAfterMark`), and
//!    their references written in B; then, the head read again in A naming the version still
//!    (`mirrorRereadsBeforeRetiring`), retired in A.
//! 8. Its own pin unconditionally deleted.
//!
//! Each run's pin and hold keys are write-once and disjoint per run (board 11699, `releasesOwn`):
//! letting go deletes its own hold (first) and pin with plain unconditional `DELETE`
//! ([`bucket::delete`]), so a delete landing however late never removes another run's pin or hold
//! and requires zero conditional-DELETE support on any backend. Marking is fenced by the
//! conditional `Copying -> Marked` version transition on [`layout::mirror`] (`marksOwn`, board
//! 9498), with no lease-window assumption on request flight time.
//!
//! Every step is create-only, idempotent, or a deletion of this run's own key: a mirror stopped
//! anywhere is resumed by a fresh run, what earlier runs wrote kept, and B's collector reaps stale
//! pins (deleting their matching hold in A first when unmarked). The next fork of the version into
//! B, and the next opening of a child of it in B, begin one stopped before its marker once its pin
//! is stale: one stopped after it, they find marked, and leave to B's collector, the children not
//! yet moved referring in A.
//!
//! A fork into B refers in B once the marker is there ([`Here`], [`crate::lifecycle`]).

use crate::bucket::{self, Bucket, Created, Meta, Refused, Updated, Version};
use crate::layout::{self, CellId, Kind, Mirror, Reference};
use crate::recovery::{self, Checkpoints};
use crate::{Error, head, lifecycle};
use futures::future::BoxFuture;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::time::Instant;

/// What a mirror came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mirrored {
    /// Marked: the objects it copied (those there already counted too), the references of this
    /// bucket's children it wrote here, and how many of them it retired in the parent's bucket,
    /// whether there or not: all, if its head named the version still, else none.
    Done { copied: usize, referred: usize, retired: usize },
    /// Given up, its version named no more by its cell's head, or its cell deleted: no mirror of it
    /// is wanted again.
    Unnamed,
    /// Given up, as the reason says: an object of it gone, or its pin taken over.
    GaveUp(String),
    /// Another mirror of the version holds its pin: under way, or its host dead not long enough
    /// ago to tell ([`PIN_STALE`]).
    UnderWay,
}

/// cladefs: how long, by its bucket's clock, a mirror's pin stands unrefreshed before it is held
/// stale: its mirror's host dead, or stopped as long. A stale pin keeps nothing from B's collector
/// ([`crate::collector::Collector::collect_copy`]), and the next mirror of its version takes over
/// the marker slot. A mirror refreshes its pin a tenth of that apart while it copies
/// ([`PIN_REFRESH`]), and right before its conditional marker transition.
pub const PIN_STALE: u64 = 10 * 60 * 1000;

/// How long a mirror goes from one refresh of its pin to the next while it copies, by its host's
/// monotonic clock (board 8469): the bucket's, which a pin's staleness is told by, may be stepped,
/// back or forward, as NTP steps a host's.
const PIN_REFRESH: Duration = Duration::from_millis(PIN_STALE / 10);

/// Whether the pin `meta` lists in `own` is stale ([`PIN_STALE`]).
pub fn stale(own: &impl Bucket, meta: &Meta) -> bool {
    meta.modified.saturating_add(PIN_STALE) < own.now()
}

/// cladefs: a bucket as forks into it see the mirrors there of versions of other buckets' cells
/// (doc-23, Mirrors; TASK-633.8): [`Bucket::mirrors`]. A fork refers in this bucket once the
/// version's marker is there, and in the parent's before; having referred there, it reads the
/// marker again after the parent's head, and refers here if it is there then (`forksRecheck`,
/// `forksLeaveA`).
pub trait Here: Send + Sync {
    /// Whether the mirror of `parent`'s version at `at` is marked in this bucket, read there now:
    /// never for a cell of this bucket's own. Refuses a marker of an unsupported format
    /// ([`Error::BucketFormat`]).
    fn marked(&self, parent: CellId, at: u64) -> BoxFuture<'_, Result<bool, Error>>;

    /// Creates `bytes` at `key` in this bucket, whatever cell `key` is of ([`bucket::create`]).
    fn create<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refused>>;

    /// Retires the references at `keys` of cells of other buckets in this bucket, marked first
    /// ([`lifecycle::retire`]): those of children of this bucket a mirror moved here, or forks
    /// wrote here, their cells dead. Others it leaves. How many it retired, whether there or not,
    /// as a pass counts its retirements ([`crate::collector::Report::retired`]).
    fn retire<'a>(&'a self, keys: &'a [&'a str]) -> BoxFuture<'a, Result<usize, Refused>>;
}

/// A mirror run's write-once pin, held: its disjoint `run` ID, key, bytes, and the version of its
/// last refresh.
struct Pin {
    run: u64,
    key: String,
    bytes: Vec<u8>,
    version: Version,
    /// When the write of it that landed last was sent, by the host's monotonic clock (board 8469):
    /// before its bucket took it, so that the pin is no older than this says, whatever the
    /// bucket's clock is stepped to.
    sent: Instant,
}

/// What [`claim_marker`] found at [`layout::mirror`].
enum Claimed {
    /// This run owns [`layout::mirror`] in [`layout::MirrorState::Copying`] at this version.
    Copying(Version),
    /// [`layout::mirror`] is already [`layout::MirrorState::Marked`].
    Marked,
    /// Another mirror actively owns [`layout::mirror`] in `Copying` (or won a concurrent takeover).
    UnderWay,
}

/// Claims [`layout::mirror`] in `own` in [`layout::MirrorState::Copying`] for `run`, taking over
/// a prior `Copying` slot conditionally when its owner's pin is absent or stale, or returns
/// [`Claimed::Marked`] / [`Claimed::UnderWay`] (board 9498).
async fn claim_marker(
    own: &impl Bucket,
    parent: CellId,
    at: u64,
    marker: &str,
    run: u64,
) -> Result<Claimed, Error> {
    let copying = layout::encode(&Mirror::copying(run));
    match bucket::create(own, marker, copying.clone()).await? {
        Created::Created(version) => Ok(Claimed::Copying(version)),
        Created::Exists(found) => match Mirror::decode(&found.bytes)?.state {
            layout::MirrorState::Marked => Ok(Claimed::Marked),
            layout::MirrorState::Copying { owner } => {
                if let Some(meta) = bucket::head(own, &layout::pin(parent, at, owner)).await?
                    && !stale(own, &meta)
                {
                    return Ok(Claimed::UnderWay);
                }
                match bucket::update(own, marker, copying, &found.meta.version).await? {
                    Updated::Updated(version) => Ok(Claimed::Copying(version)),
                    Updated::Changed => Ok(Claimed::UnderWay),
                }
            }
        },
    }
}

/// Transitions `marker` from `Copying` to [`layout::MirrorState::Marked`] conditionally on
/// `version` (`PutMode::Update`, board 9498): returns `false` if another mirror took over the
/// marker slot or B's collector deleted the copy meanwhile, however late the request lands.
async fn mark(own: &impl Bucket, marker: &str, version: &Version) -> Result<bool, Refused> {
    let marked = layout::encode(&Mirror::NOW);
    Ok(matches!(bucket::update(own, marker, marked, version).await?, Updated::Updated(_)))
}

impl Pin {
    /// Creates a fresh per-run pin of `parent`'s version at `at` in `own` (`PutMode::Create`),
    /// after a best-effort prefix `LIST` on [`layout::pins`] to skip copying when another mirror
    /// already holds a non-stale pin ([`Mirrored::UnderWay`], board 11699).
    async fn take(own: &impl Bucket, parent: CellId, at: u64) -> Result<Option<Self>, Refused> {
        let existing = bucket::list(own, &layout::pins(parent, at)).await?;
        if existing
            .iter()
            .any(|meta| matches!(layout::kind(&meta.key), Kind::Pin { .. }) && !stale(own, meta))
        {
            return Ok(None);
        }
        let run = own.random_u64();
        let key = layout::pin(parent, at, run);
        let bytes = layout::encode(&Reference::of(run));
        let sent = Instant::now();
        let Created::Created(version) = bucket::create(own, &key, bytes.clone()).await? else {
            return Ok(None);
        };
        Ok(Some(Self { run, key, bytes, version, sent }))
    }

    /// The write-once hold key of this mirror run in the parent's bucket (board 11699).
    fn hold_key(&self, parent: CellId, at: u64, number: u16) -> String {
        layout::hold(parent, at, number, self.run)
    }

    /// The bytes of this mirror run's hold.
    fn hold(&self) -> Vec<u8> {
        layout::encode(&Reference::of(self.run))
    }

    /// Writes it again, if [`PIN_REFRESH`] passed since it was last written, by its host's clock,
    /// or `now`: false if it was reaped as stale or overwritten.
    async fn refresh(&mut self, own: &impl Bucket, now: bool) -> Result<bool, Refused> {
        if !now && self.sent.elapsed() < PIN_REFRESH {
            return Ok(true);
        }
        let sent = Instant::now();
        Ok(match bucket::update(own, &self.key, self.bytes.clone(), &self.version).await? {
            Updated::Updated(version) => {
                (self.version, self.sent) = (version, sent);
                true
            }
            Updated::Changed => false,
        })
    }

    /// Unconditionally deletes this run's own pin (board 11699): disjoint per run, so even a
    /// delete landing arbitrarily late never removes another run's pin (`releasesOwn`).
    async fn release(self, own: &impl Bucket) -> Result<(), Refused> {
        bucket::delete(own, &self.key).await
    }

    /// Unconditionally deletes this run's own `hold` in `bucket` first, then this run's own pin
    /// in `own` (board 11699): disjoint per run, so neither delete can remove another run's hold
    /// or pin however late it lands. Deleting `hold` before `pin` ensures that a crash between
    /// the two leaves the pin in B for B's collector to reap.
    async fn release_holding(
        self,
        own: &impl Bucket,
        bucket: &impl Bucket,
        hold: &str,
    ) -> Result<(), Refused> {
        bucket::delete(bucket, hold).await?;
        self.release(own).await
    }
}

/// How many objects a mirror copies at once.
const COPIES_AT_ONCE: usize = 8;

/// A version, by its cell and the segment it ends.
type Of = (CellId, u64);

/// What a process knows of the mirrors into its bucket (doc-23, Mirrors): the versions marked
/// here, or found named no more, of which it wants no mirror, and its mirrors' turns, its mirrors
/// of one version made one after another, so that one asked for later finds the version marked
/// rather than another mirror under way. Shared by its clones; a process's own, which its death
/// takes with it.
#[derive(Clone, Default)]
pub struct Mirrors {
    known: std::sync::Arc<std::sync::Mutex<Known>>,
}

#[derive(Default)]
struct Known {
    marked: std::collections::HashSet<Of>,
    /// Those whose mirrors found them named no more: none of them is wanted again.
    unnamed: std::collections::HashSet<Of>,
    making: std::collections::HashMap<Of, std::sync::Arc<tokio::sync::Mutex<()>>>,
}

impl Mirrors {
    /// Notes `parent`'s version at `at` marked here, as a read of its marker found it.
    pub fn marked(&self, parent: CellId, at: u64) {
        self.known.lock().unwrap().marked.insert((parent, at));
    }

    /// The turn of a mirror of `origin`'s version, taken as it is asked for, if free, else to be
    /// waited for ([`Turn::mirror`]): none if the version is known marked or unnamed.
    pub fn turn(&self, origin: layout::Origin) -> Option<Turn> {
        let version = (origin.parent, origin.at);
        let mut known = self.known.lock().unwrap();
        if known.marked.contains(&version) || known.unnamed.contains(&version) {
            return None;
        }
        let one = known.making.entry(version).or_default().clone();
        // Held by another mirror of the version: this one waits its turn (Turn::mirror).
        let held = one.clone().try_lock_owned().ok();
        Some(Turn { mirrors: self.clone(), version, one: Some(one), held })
    }

    /// Whether no mirror holds or waits for a turn.
    pub fn idle(&self) -> bool {
        self.known.lock().unwrap().making.is_empty()
    }
}

/// A mirror's turn among its process's mirrors of one version ([`Mirrors::turn`]): let go as it
/// drops, the version's lock with it once no other turn holds it.
pub struct Turn {
    mirrors: Mirrors,
    version: Of,
    one: Option<std::sync::Arc<tokio::sync::Mutex<()>>>,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Turn {
    /// Waits for its turn, then the mirror its child `child` wants ([`wanted`]): the version found
    /// unnamed noted so, wanted no more.
    pub async fn mirror(
        mut self,
        bucket: &impl Bucket,
        own: &impl Bucket,
        number: u16,
        child: CellId,
        origin: layout::Origin,
    ) -> Result<Option<Mirrored>, Error> {
        if self.held.is_none()
            && let Some(one) = &self.one
        {
            self.held = Some(one.clone().lock_owned().await);
        }
        let mirrored = wanted(bucket, own, number, child, origin).await?;
        if mirrored == Some(Mirrored::Unnamed) {
            self.mirrors.known.lock().unwrap().unnamed.insert(self.version);
        }
        Ok(mirrored)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.held = None;
        let mut known = self.mirrors.known.lock().unwrap();
        self.one = None;
        if known.making.get(&self.version).is_some_and(|one| std::sync::Arc::strong_count(one) == 1)
        {
            known.making.remove(&self.version);
        }
    }
}

/// The mirror into this bucket, numbered `number`, whose own bucket is `own`, of the version
/// `child` was forked from, `origin`, which the child's fork, or its cell's opening, asks for
/// (doc-23: the first fork of a version into a bucket starts its mirror there): none unless `child`
/// is of this bucket and `origin`'s parent of another, as a host of a third bucket opening `child`
/// copies nothing its own bucket's children read; none if `bucket` routes nothing, or the version
/// is marked here already. Else [`mirror`].
pub async fn wanted(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
    child: CellId,
    origin: layout::Origin,
) -> Result<Option<Mirrored>, Error> {
    let layout::Origin { parent, at, .. } = origin;
    let ours = layout::routed_to(&child.prefix()) == Some(number);
    let theirs = layout::routed_to(&parent.prefix()).is_some_and(|of| of != number);
    let Some(here) = bucket.mirrors().filter(|_| ours && theirs) else { return Ok(None) };
    if here.marked(parent, at).await? {
        return Ok(None);
    }
    mirror(bucket, own, number, parent, at).await.map(Some)
}

/// Mirrors `parent`'s version at `at` into this bucket, whose own bucket is `own` and number
/// `number`, through `bucket`, routed, which reads the parent's bucket (see the module). Begun
/// again after a crash, it goes on from where it was, once its pin is stale.
pub async fn mirror(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
    parent: CellId,
    at: u64,
) -> Result<Mirrored, Error> {
    let Some(mut pin) = Pin::take(own, parent, at).await? else {
        return Ok(Mirrored::UnderWay);
    };
    let hold = pin.hold_key(parent, at, number);
    failpoint!("mirror::pinned");
    let marker = layout::mirror(parent, at);
    let copied = match claim_marker(own, parent, at, &marker, pin.run).await {
        // Another active run holds the marker slot (or won a concurrent takeover): release our
        // pin (no hold was written yet) and step aside.
        Ok(Claimed::UnderWay) => {
            pin.release(own).await?;
            return Ok(Mirrored::UnderWay);
        }
        // Marked already, by a mirror begun before: its copy whole, and the hold that mirror wrote
        // standing until B's collector deletes it, with the marker (`mirrorHoldsUnmarked`). One
        // written again here, after that collector listed the copy before this pin and deleted
        // the holds, would stand for good.
        Ok(Claimed::Marked) => 0,
        // Its hold in the parent's bucket, before it reads the head there (`mirrorHolds`).
        Ok(Claimed::Copying(marker_version)) => {
            match hold_then_copy(bucket, own, &mut pin, (parent, at), &hold).await {
                Ok(Copied::Whole { count }) => {
                    if !pin.refresh(own, true).await? {
                        let_go(bucket, own, pin, &hold).await?;
                        return Ok(Mirrored::GaveUp(TAKEN.to_owned()));
                    }
                    // A host stopped here, its pin its own still as it last heard, marks late (a
                    // test's).
                    failpoint!("mirror::refreshed");
                    // Transition `Copying -> Marked` conditionally on `marker_version` (`marksOwn`,
                    // board 9498). On `Err`, the conditional update may have landed before the
                    // transient failure, so leave this run's hold and pin for `collect_copy` to
                    // reap (`MarkedHeld`).
                    if mark(own, &marker, &marker_version).await? {
                        failpoint!("mirror::marked");
                        count
                    } else {
                        let_go(bucket, own, pin, &hold).await?;
                        return Ok(Mirrored::GaveUp(TAKEN.to_owned()));
                    }
                }
                // Pin was lost during copy, before `PUT marker` was ever sent: release our own hold
                // and pin so neither leaks.
                Ok(Copied::Taken) => {
                    let_go(bucket, own, pin, &hold).await?;
                    return Ok(Mirrored::GaveUp(TAKEN.to_owned()));
                }
                Ok(Copied::Unnamed) => {
                    let_go(bucket, own, pin, &hold).await?;
                    return Ok(Mirrored::Unnamed);
                }
                Ok(Copied::Short(why)) => {
                    let_go(bucket, own, pin, &hold).await?;
                    return Ok(Mirrored::GaveUp(why));
                }
                Err(error) => {
                    // cladefs: the copy's failure what it fails with, whatever letting go came to.
                    if let Err(refused) = let_go(bucket, own, pin, &hold).await {
                        log::warn!("cladefs: a mirror of {parent} at {at}, letting go: {refused}");
                    }
                    return Err(error);
                }
            }
        }
        Err(error) => {
            if let Err(refused) = pin.release(own).await {
                log::warn!("cladefs: a mirror of {parent} at {at}, releasing pin: {refused}");
            }
            return Err(error);
        }
    };
    // Its pin let go whatever the listing came to: the marker, and the references written here,
    // stand, and a mirror begun again lists them again.
    let referred = refer_here(bucket, own, number, parent, at).await;
    failpoint!("mirror::referred");
    pin.release(own).await?;
    let (referred, retired) = referred?;
    Ok(Mirrored::Done { copied, referred, retired })
}

/// Writes `hold`, then copies (`mirrorHolds`).
async fn hold_then_copy(
    bucket: &impl Bucket,
    own: &impl Bucket,
    pin: &mut Pin,
    (parent, at): (CellId, u64),
    hold: &str,
) -> Result<Copied, Error> {
    write_hold(bucket, hold, pin).await?;
    failpoint!("mirror::held");
    copy(bucket, own, pin, parent, at).await
}

/// Creates this run's write-once `hold` (`PutMode::Create`, board 11699): disjoint per run, never
/// colliding with another mirror run's hold.
async fn write_hold(bucket: &impl Bucket, hold: &str, pin: &Pin) -> Result<(), Refused> {
    bucket::create(bucket, hold, pin.hold()).await.map(drop)
}

/// Gives a mirror run up: unconditionally deletes this run's own `hold` in the parent's bucket,
/// then its own `pin` in `own` (board 11699).
async fn let_go(
    bucket: &impl Bucket,
    own: &impl Bucket,
    pin: Pin,
    hold: &str,
) -> Result<(), Refused> {
    pin.release_holding(own, bucket, hold).await
}

/// Why a mirror whose pin is no longer its own gives up.
const TAKEN: &str = "its pin, stale, taken over";

/// What a mirror's copy came to.
#[derive(Debug, PartialEq, Eq)]
enum Copied {
    /// Every object copied, this many.
    Whole { count: usize },
    /// Its pin taken over, as it refreshed it.
    Taken,
    /// Short of the version, its cell's head naming it no more.
    Unnamed,
    /// Short of an object, as it says.
    Short(String),
}

/// Copies every object of `parent`'s version at `at` under its prefix into `own`, after reading
/// its head (`mirrorChecks`), refreshing `pin` as it goes.
///
/// What it copies: the checkpoint at the version and what recovering it read, and the object each
/// extent is in, which the parent's bucket's collector keeps for the version (doc-7, A pass over a
/// cell): one gone gives the mirror up.
async fn copy(
    bucket: &impl Bucket,
    own: &impl Bucket,
    pin: &mut Pin,
    parent: CellId,
    at: u64,
) -> Result<Copied, Error> {
    let head = head::get(bucket, parent).await?;
    let named = head.as_ref().filter(|head| !head.deleted()).and_then(|head| {
        head.versions.iter().any(|version| version.at == at).then(|| head.epoch_of(at)).flatten()
    });
    let Some(epoch) = named else { return Ok(Copied::Unnamed) };
    failpoint!("mirror::checked");
    let checkpoints = Checkpoints::new(bucket, 1);
    let (state, touched) = match recovery::recover_at(&checkpoints, parent, at).await {
        Ok(recovered) => recovered,
        Err(Error::Lost(lost)) => {
            return Ok(Copied::Short(format!("{lost}: gone from its bucket")));
        }
        Err(error) => return Err(error),
    };
    let ours = |key: &String| layout::owner(key) == Some(parent);
    let mut needed: BTreeSet<String> = touched.into_iter().filter(ours).collect();
    needed.insert(layout::checkpoint(parent, at, epoch));
    needed.extend(state.extents.values().map(|location| location.key.clone()).filter(ours));
    let keys: Vec<String> = needed.into_iter().collect();
    copy_objects(bucket, own, pin, &keys).await
}

/// Copies the objects at `keys` into `own`, [`COPIES_AT_ONCE`] at once, refreshing `pin` as each
/// copy ends. Short of the first found gone, as each copy says so of its own key (board 8439): what
/// it comes to never depends on the order the copies end in.
async fn copy_objects(
    bucket: &impl Bucket,
    own: &impl Bucket,
    pin: &mut Pin,
    keys: &[String],
) -> Result<Copied, Error> {
    use futures::StreamExt as _;
    if !pin.refresh(own, false).await? {
        return Ok(Copied::Taken);
    }
    // The copies' futures made first: a stream mapping them in holds the closure in its type,
    // which a future awaiting this one is then not seen to be `Send` with.
    let copies: Vec<_> = keys.iter().map(|key| copy_one(bucket, own, key)).collect();
    let mut copies = futures::stream::iter(copies).buffer_unordered(COPIES_AT_ONCE);
    let mut count = 0;
    while let Some(copied) = copies.next().await {
        match copied? {
            One::Copied => count += 1,
            One::Gone(key) => return Ok(Copied::Short(format!("{key}: gone from its bucket"))),
        }
        if !pin.refresh(own, false).await? {
            return Ok(Copied::Taken);
        }
    }
    failpoint!("mirror::copied");
    Ok(Copied::Whole { count })
}

/// What a copy of one object came to ([`copy_one`]).
enum One<'a> {
    /// Copied, or there already.
    Copied,
    /// Gone from where the mirror reads it: the object at its key.
    Gone(&'a str),
}

/// Copies the object at `key` into `own`, unless it is there, which a HEAD tells
/// (board 8435): none of its bytes read here. One there already, from an earlier copy, B's
/// collector keeps while the parent's cell lives
/// ([`crate::collector::Collector::collect_copy`]): no version of it this mirror copies outlives
/// it.
async fn copy_one<'a>(
    bucket: &impl Bucket,
    own: &impl Bucket,
    key: &'a str,
) -> Result<One<'a>, Refused> {
    if bucket::head(own, key).await?.is_some() {
        return Ok(One::Copied);
    }
    let Some(object) = bucket::get(bucket, key).await? else {
        return Ok(One::Gone(key));
    };
    bucket::create(own, key, object.bytes).await?;
    // A crash with some objects copied, others not (a test's).
    fail::fail_point!("mirror::copying");
    Ok(One::Copied)
}

/// Moves the references of the children of this bucket, numbered `number`, to `parent`'s version
/// at `at` from the parent's bucket to `own` (`mirrorListsAfterMark`): each written here; then, the
/// parent's head read again naming the version still (`mirrorRereadsBeforeRetiring`), retired
/// there. A version dropped by then, they stay there too: B's collector, which reads the head
/// before it lists the copy, may have listed it before they were written here, and goes on to
/// delete it, its hold with it. How many it wrote here, and how many it retired there.
async fn refer_here(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
    parent: CellId,
    at: u64,
) -> Result<(usize, usize), Error> {
    let listed = bucket::list(bucket, &layout::origin(parent, at)).await?;
    let mut moved = Vec::new();
    for meta in listed {
        let Kind::Reference { child, .. } = layout::kind(&meta.key) else { continue };
        if crate::objectid::bucket_of(child.id) != number {
            continue;
        }
        let Some(object) = bucket::get(bucket, &meta.key).await? else { continue };
        layout::Reference::decode(&object.bytes)?;
        bucket::create(own, &meta.key, object.bytes).await?;
        moved.push(meta.key);
    }
    // A crash with the references written here, still standing there (a test's).
    fail::fail_point!("mirror::moved");
    if moved.is_empty() {
        return Ok((0, 0));
    }
    let head = head::get(bucket, parent).await?;
    let named = head.as_ref().filter(|head| !head.deleted());
    if named.is_some_and(|head| head.versions.iter().any(|version| version.at == at)) {
        let keys: Vec<&str> = moved.iter().map(String::as_str).collect();
        lifecycle::retire(bucket, &keys).await?;
        return Ok((moved.len(), moved.len()));
    }
    Ok((moved.len(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::PutMode;
    use crate::testing::{Direct, Fault, SimBucket};
    use futures::executor::block_on;
    use std::future::Future;
    use std::task::{Context, Poll};

    /// A cell of bucket 2, whose version at 3 a host of bucket 1 mirrors there.
    const PARENT: CellId = CellId { tenant: 1, id: 2 << 48 | 5 };

    /// A runtime whose clock, the hosts' monotonic clock pins are refreshed by, stands until a test
    /// moves it ([`advance`]).
    fn paused() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
    }

    fn advance(runtime: &tokio::runtime::Runtime, by: Duration) {
        runtime.block_on(tokio::time::advance(by));
    }

    /// Runs `future` on `sim`, completing its requests one at a time, each the one `pick` chooses
    /// of those waiting, by their descriptions ([`SimBucket::describe`]): what it gives, and what
    /// was asked, in order.
    fn driven<T>(
        sim: &SimBucket,
        future: impl Future<Output = T>,
        mut pick: impl FnMut(&[String]) -> usize,
    ) -> (T, Vec<String>) {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut asked = Vec::new();
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return (output, asked);
            }
            let waiting: Vec<String> =
                (0..sim.waiting()).map(|index| sim.describe(index)).collect();
            assert!(!waiting.is_empty(), "waiting for no request");
            let index = pick(&waiting);
            asked.push(waiting[index].clone());
            sim.complete(index, Fault::None);
        }
    }

    /// The pin of [`PARENT`]'s version at 3 in `own`, taken.
    fn pinned(sim: &SimBucket, own: &SimBucket) -> Pin {
        sim.served(Pin::take(own, PARENT, 3)).0.unwrap().expect("its pin another's")
    }

    /// board 8439: what a mirror's copy comes to depends on which objects are gone alone, whatever
    /// order the copies end in: one gone gives it up, its copy ending first or last; none gone,
    /// every one is copied. board 8435: copied again into the same bucket, it reads none of the
    /// bytes of what is there already, a HEAD telling it is.
    #[test]
    fn copies_come_to_what_is_gone_whatever_order_they_end_in() {
        let key = |n| layout::segment(PARENT, n, 1);
        let keys = [key(1), key(2), key(3)];
        for (gone, first) in [(Some(key(2)), true), (Some(key(2)), false), (None, true)] {
            let sim = SimBucket::new();
            let (bucket, own) = (sim.at(1), sim.at(1).own());
            let there: Vec<&String> =
                keys.iter().filter(|stored| Some(*stored) != gone.as_ref()).collect();
            for stored in &there {
                let bytes = bytes::Bytes::from(stored.as_bytes().to_vec());
                block_on(Direct(sim.at(2)).put(stored, bytes, PutMode::Create)).unwrap();
            }
            let mut pin = pinned(&sim, &own);
            // The gone object's requests answered before any other's, or after every other's.
            let order = |waiting: &[String]| {
                let Some(gone) = &gone else { return 0 };
                (waiting.iter().position(|asked| asked.contains(gone) == first)).unwrap_or(0)
            };
            let copying = copy_objects(&*bucket, &*own, &mut pin, &keys);
            let (copied, _) = driven(&sim, copying, order);
            if let Some(gone) = &gone {
                let case =
                    format!("{gone} gone, answering {}", if first { "first" } else { "last" });
                let short = Copied::Short(format!("{gone}: gone from its bucket"));
                assert_eq!(copied.unwrap(), short, "{case}");
                continue;
            }
            assert_eq!(copied.unwrap(), Copied::Whole { count: 3 });
            let copies: Vec<String> = there.iter().map(|key| format!("@1/{key}")).collect();
            let stored = sim.stored();
            assert!(copies.iter().all(|copy| stored.contains(copy)), "{stored:?}");

            let again = copy_objects(&*bucket, &*own, &mut pin, &keys);
            let (copied, mut asked) = driven(&sim, again, |_| 0);
            assert_eq!(copied.unwrap(), Copied::Whole { count: 3 }, "again");
            asked.sort();
            let mut heads: Vec<String> = keys.iter().map(|key| format!("@1 head {key}")).collect();
            heads.sort();
            assert_eq!(asked, heads, "again");
        }
    }

    /// board 8469: a pin is written again once [`PIN_REFRESH`] passed by its host's monotonic
    /// clock, whatever its bucket's clock does: stepped back, as NTP steps a host's, its refresh
    /// comes on time all the same, and none before. board 8439: and so as the copies of its
    /// objects end, not only before they start.
    #[test]
    fn pins_are_refreshed_on_time_by_their_hosts_clock() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        sim.advance(2 * PIN_STALE);
        let own = sim.at(1).own();
        let mut pin = pinned(&sim, &own);
        let written = format!("@1 put {}", pin.key);
        sim.step_back(PIN_STALE);
        let refresh = |pin: &mut Pin| sim.served(pin.refresh(&*own, false));
        assert_eq!(refresh(&mut pin), (Ok(true), vec![]), "refreshed early");
        advance(&runtime, PIN_REFRESH);
        assert_eq!(refresh(&mut pin), (Ok(true), vec![written.clone()]), "not refreshed on time");

        let bucket = sim.at(1);
        let keys: Vec<String> = (1..=3).map(|n| layout::segment(PARENT, n, 1)).collect();
        for key in &keys {
            let bytes = bytes::Bytes::from(key.as_bytes().to_vec());
            block_on(Direct(sim.at(2)).put(key, bytes, PutMode::Create)).unwrap();
        }
        // A refresh due as the first copy ends.
        let mut moved = false;
        let order = |waiting: &[String]| {
            if !moved && waiting[0].starts_with("@1 put") {
                advance(&runtime, PIN_REFRESH);
                moved = true;
            }
            0
        };
        let (copied, asked) = driven(&sim, copy_objects(&*bucket, &*own, &mut pin, &keys), order);
        assert_eq!(copied.unwrap(), Copied::Whole { count: 3 });
        let refreshed = asked.iter().filter(|asked| **asked == written).count();
        assert_eq!(refreshed, 1, "{asked:?}");
    }

    /// Runs `future` on `sim`, completing every request it asks in turn but those `held` picks,
    /// which it leaves waiting: what it gives, if it ends.
    fn served_but<T>(
        sim: &SimBucket,
        future: impl Future<Output = T>,
        held: impl Fn(&str) -> bool,
    ) -> Option<T> {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return Some(output);
            }
            let free = (0..sim.waiting()).find(|&index| !held(&sim.describe(index)))?;
            sim.complete(free, Fault::None);
        }
    }

    /// board 8441, board 9229, board 11699: a mirror letting go sends its hold's plain
    /// unconditional delete; its host stops past its lease, the delete in flight all that time;
    /// the next mirror of the version starts with its own fresh per-run pin and hold keys; then
    /// the old delete lands, and the old mirror goes on to delete its own pin. Neither deletes
    /// what the next mirror wrote because each run has disjoint write-once pin and hold keys.
    #[test]
    fn late_deletes_of_mirrors_letting_go_leave_the_next_ones_hold_and_pin() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let first = pinned(&sim, &own);
        let first_hold = first.hold_key(PARENT, 3, 1);
        sim.served(write_hold(&*bucket, &first_hold, &first)).0.unwrap();

        // Its hold's unconditional delete sent, and held in flight.
        let deleting = format!("@2 delete {first_hold}");
        let in_flight = |asked: &str| asked.starts_with(&deleting);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut letting_go = std::pin::pin!(first.release_holding(&*own, &*bucket, &first_hold));
        loop {
            assert!(letting_go.as_mut().poll(&mut context).is_pending());
            if (0..sim.waiting()).any(|index| in_flight(&sim.describe(index))) {
                break;
            }
            sim.complete(0, Fault::None);
        }

        // Its host stopped past its lease; the next mirror starts with fresh disjoint pin & hold.
        sim.advance(PIN_STALE + 1);
        advance(&runtime, Duration::from_millis(PIN_STALE + 1));
        let next = served_but(&sim, Pin::take(&*own, PARENT, 3), in_flight);
        let next = next.expect("the pin created").unwrap().expect("the old pin stale, new run");
        let next_hold = next.hold_key(PARENT, 3, 1);
        served_but(&sim, write_hold(&*bucket, &next_hold, &next), in_flight).unwrap().unwrap();

        // The old delete lands; the old mirror goes on, and ends.
        let held = (0..sim.waiting()).find(|&index| in_flight(&sim.describe(index))).unwrap();
        sim.complete(held, Fault::None);
        let (released, _) = sim.served(letting_go);
        assert_eq!(released, Ok(()));
        let hold =
            block_on(Direct(bucket.clone()).get(&next_hold)).expect("the next mirror's hold gone");
        assert_eq!(hold.bytes, next.hold(), "the hold");
        let pin = block_on(Direct(own.clone()).get(&next.key)).expect("the next mirror's pin gone");
        assert_eq!(pin.bytes, next.bytes, "the pin");
    }

    /// hotbed review 53, board 11699: a mirror paused past its lease after it found no marker,
    /// before its hold, while the next mirror wrote its own hold, marked and dropped its pin;
    /// going on, the paused mirror creates its own per-run hold and gives up, deleting its own
    /// hold and pin without touching the marked copy's hold.
    #[test]
    fn mirrors_resumed_past_their_lease_leave_a_marked_copys_hold() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let paused_one = pinned(&sim, &own);
        let paused_hold = paused_one.hold_key(PARENT, 3, 1);
        sim.advance(PIN_STALE + 1);
        advance(&runtime, Duration::from_millis(PIN_STALE + 1));
        let next = pinned(&sim, &own);
        let next_hold = next.hold_key(PARENT, 3, 1);
        sim.served(write_hold(&*bucket, &next_hold, &next)).0.unwrap();
        assert_eq!(sim.served(next.release(&*own)).0, Ok(()));

        sim.served(write_hold(&*bucket, &paused_hold, &paused_one)).0.unwrap();
        assert_eq!(sim.served(paused_one.release_holding(&*own, &*bucket, &paused_hold)).0, Ok(()));
        assert!(sim.stored().contains(&format!("@2/{next_hold}")), "the marked copy's hold gone");
        assert!(
            !sim.stored().contains(&format!("@2/{paused_hold}")),
            "the paused run's own hold left behind"
        );
    }

    /// board 9229, board 11699: a mirror letting go deletes its own hold and pin, with nothing
    /// else in the way.
    #[test]
    fn mirrors_letting_go_delete_their_own_hold_and_pin() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let pin = pinned(&sim, &own);
        let hold = pin.hold_key(PARENT, 3, 1);
        sim.served(write_hold(&*bucket, &hold, &pin)).0.unwrap();
        assert_eq!(sim.served(pin.release_holding(&*own, &*bucket, &hold)).0, Ok(()));
        assert_eq!(sim.keys(), Vec::<String>::new());
    }

    /// doc-28, board 10038: a mirror marker or reference of an unsupported format is refused by
    /// `Mirror::decode` / `Reference::decode` (`FORMAT.check`), never silently accepted.
    #[test]
    fn mirrors_refuse_markers_and_references_of_other_formats() {
        let sim = SimBucket::new();
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let future = layout::FORMAT.current + 1;
        let bad_marker =
            layout::encode(&layout::Mirror { format: future, state: layout::MirrorState::Marked });
        sim.served(bucket::create(&*own, &layout::mirror(PARENT, 3), bad_marker)).0.unwrap();
        assert_eq!(
            sim.served(bucket.marked(PARENT, 3)).0,
            Err(Error::BucketFormat(future)),
            "Here::marked must check Mirror::decode"
        );
        assert_eq!(
            sim.served(mirror(&*bucket, &*own, 1, PARENT, 3)).0,
            Err(Error::BucketFormat(future)),
            "mirror::mirror must check Mirror::decode"
        );
        // A reference of an unsupported format in the parent's bucket is refused by refer_here.
        let child = CellId { tenant: 1, id: (1 << 48) | 9 };
        let bad_ref = layout::encode(&layout::Reference { format: future, owner: 1 });
        sim.served(bucket::create(&*bucket, &layout::reference(PARENT, 4, child), bad_ref))
            .0
            .unwrap();
        assert_eq!(
            sim.served(refer_here(&*bucket, &*own, 1, PARENT, 4)).0,
            Err(Error::BucketFormat(future)),
            "refer_here must check Reference::decode"
        );
    }

    /// Creates a versioned [`PARENT`] cell in bucket 2 on `sim` with a checkpoint and segment,
    /// returning `(at, epoch)`.
    fn create_versioned_parent(sim: &SimBucket) -> (u64, u64) {
        let bucket2 = sim.at(2);
        sim.served(lifecycle::create(&*bucket2, PARENT, 0)).0.unwrap();
        let writer =
            sim.served(crate::writer::Writer::open(bucket2.clone(), PARENT, 10, 0)).0.unwrap();
        let extent = storage_device::ExtentId::new(PARENT.id, writer.chain().next_sequence());
        let at = sim
            .served(crate::testing::versioned_by(
                &writer,
                vec![(extent, b"versioned".to_vec())],
                crate::testing::VERSION,
            ))
            .0;
        sim.served(writer.checkpoint()).0.unwrap();
        let head = sim.served(head::get(&*bucket2, PARENT)).0.unwrap().unwrap();
        (at, head.epoch_of(at).unwrap())
    }

    /// Drives `future` on `sim` while keeping request index 0 held in flight.
    fn serve_behind_held<F: Future>(sim: &SimBucket, future: F) -> F::Output {
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
            assert!(sim.waiting() > 1, "expected new request behind held late mark");
            sim.complete(1, Fault::None);
        }
    }

    /// board 9498: a mirror copies every object and sends its conditional marker update, which
    /// stays in flight past [`PIN_STALE`] while the parent is deleted and B's collector resumes
    /// the stale pin (`Unnamed`, taking over the marker slot, deleting hold and pin) and then
    /// deletes the copy's objects. When the held marker update finally lands, its conditional
    /// `PutMode::Update` is rejected, leaving the deleted copy unmarked (`MarkersWhole`).
    #[test]
    fn late_marker_after_stale_pin_collection_is_rejected_and_leaves_copy_unmarked() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (at, _epoch) = create_versioned_parent(&sim);
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let marker = layout::mirror(PARENT, at);
        let marking = format!("@1 put {marker}");

        // Drive Mirror 0 past `claim_marker` (1st `@1 put <marker>`) and object copies until its
        // conditional `mark` (2nd `@1 put <marker>`) is waiting in flight at queue index 0.
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut first = std::pin::pin!(mirror(&*bucket, &*own, 1, PARENT, at));
        let mut marker_puts = 0usize;
        loop {
            assert!(first.as_mut().poll(&mut context).is_pending());
            let waiting: Vec<String> =
                (0..sim.waiting()).map(|index| sim.describe(index)).collect();
            if let Some(pos) = waiting.iter().position(|d| d == &marking) {
                marker_puts += 1;
                if marker_puts == 2 {
                    break;
                }
                sim.complete(pos, Fault::None);
            } else {
                sim.complete(0, Fault::None);
            }
        }
        assert_eq!(marker_puts, 2);
        assert_eq!(sim.waiting(), 1);
        assert_eq!(sim.describe(0), marking);
        let copied_before = sim
            .stored()
            .into_iter()
            .filter(|s| {
                s.starts_with("@1/")
                    && !s.ends_with(&marker)
                    && !s.contains(&layout::pins(PARENT, at))
            })
            .count();
        assert!(copied_before >= 1, "objects copied before mark");

        // Delete PARENT in bucket 2 and advance past PIN_STALE.
        serve_behind_held(&sim, lifecycle::delete(&*sim.at(2), PARENT, 1, u64::MAX)).unwrap();
        sim.advance(PIN_STALE + 1);
        advance(&runtime, Duration::from_millis(PIN_STALE + 1));

        // Pass 1: collector sees stale pin -> resumes -> Unnamed (overwrites marker slot) ->
        // reaps stale hold and stale pin.
        let report1 = serve_behind_held(
            &sim,
            crate::collector::Collector::new().collect_copy(&*bucket, &*own, 1, PARENT),
        )
        .unwrap();
        assert_eq!(report1.deleted, 0);

        // Pass 2: collector sees deleted parent, no pins -> deletes marker slot & copied objects.
        let report2 = serve_behind_held(
            &sim,
            crate::collector::Collector::new().collect_copy(&*bucket, &*own, 1, PARENT),
        )
        .unwrap();
        assert_eq!(report2.deleted, copied_before + 1, "collector deleted marker slot + objects");

        // Complete the held late marker update: rejected because its version is gone.
        assert_eq!(sim.waiting(), 1);
        assert_eq!(sim.describe(0), marking);
        sim.complete(0, Fault::None);
        let (result, _) = sim.served(first);
        assert_eq!(result, Ok(Mirrored::GaveUp(TAKEN.to_owned())));
        assert_eq!(sim.served(bucket.marked(PARENT, at)).0, Ok(false), "copy must stay unmarked");
        let b_objects: Vec<String> =
            sim.stored().into_iter().filter(|s| s.starts_with("@1/")).collect();
        assert_eq!(b_objects, Vec::<String>::new(), "no orphan marker or objects left in B");
    }

    /// board 9498 (`MarkedHeld`): even while PARENT stays alive and copied objects remain in B,
    /// if Mirror 1 takes over the marker slot after Mirror 0's pin goes stale, fails, and releases
    /// its hold and pin while B's collector reaps Mirror 0's stale hold and pin, Mirror 0's late
    /// conditional marker update is rejected rather than marking an unheld copy.
    #[test]
    fn late_marker_after_failed_takeover_is_rejected_and_leaves_unheld_copy_unmarked() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (at, _epoch) = create_versioned_parent(&sim);
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let marker = layout::mirror(PARENT, at);
        let marking = format!("@1 put {marker}");

        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut first = std::pin::pin!(mirror(&*bucket, &*own, 1, PARENT, at));
        let mut marker_puts = 0usize;
        loop {
            assert!(first.as_mut().poll(&mut context).is_pending());
            let waiting: Vec<String> =
                (0..sim.waiting()).map(|index| sim.describe(index)).collect();
            if let Some(pos) = waiting.iter().position(|d| d == &marking) {
                marker_puts += 1;
                if marker_puts == 2 {
                    break;
                }
                sim.complete(pos, Fault::None);
            } else {
                sim.complete(0, Fault::None);
            }
        }
        assert_eq!(sim.waiting(), 1);
        assert_eq!(sim.describe(0), marking);

        // Advance past PIN_STALE; Mirror 1 takes over pin + marker slot, writes hold, fails and
        // releases its hold + pin, and Mirror 0's stale hold + pin are reaped (zero holds left).
        sim.advance(PIN_STALE + 1);
        advance(&runtime, Duration::from_millis(PIN_STALE + 1));
        let next = serve_behind_held(&sim, Pin::take(&*own, PARENT, at)).unwrap().unwrap();
        let next_hold = next.hold_key(PARENT, at, 1);
        assert!(matches!(
            serve_behind_held(&sim, claim_marker(&*own, PARENT, at, &marker, next.run)).unwrap(),
            Claimed::Copying(_)
        ));
        serve_behind_held(&sim, write_hold(&*bucket, &next_hold, &next)).unwrap();
        serve_behind_held(&sim, next.release_holding(&*own, &*bucket, &next_hold)).unwrap();
        // Reap Mirror 0's stale hold and pin (as `collect_copy` does after a failed resume).
        for meta in serve_behind_held(&sim, bucket::list(&*own, &layout::pins(PARENT, at))).unwrap()
        {
            if let Some(hold) = layout::hold_of_pin(&meta.key, 1) {
                serve_behind_held(&sim, bucket::delete(&*bucket, &hold)).unwrap();
            }
            serve_behind_held(&sim, bucket::delete(&*own, &meta.key)).unwrap();
        }
        assert!(
            serve_behind_held(&sim, bucket::list(&*bucket, &layout::holds(PARENT, at, 1)))
                .unwrap()
                .is_empty(),
            "zero holds stand in A"
        );

        // Now let Mirror 0's late mark land: rejected because its marker slot version was taken.
        assert_eq!(sim.waiting(), 1);
        assert_eq!(sim.describe(0), marking);
        sim.complete(0, Fault::None);
        let (result, _) = sim.served(first);
        assert_eq!(result, Ok(Mirrored::GaveUp(TAKEN.to_owned())));
        assert_eq!(sim.served(bucket.marked(PARENT, at)).0, Ok(false), "unheld copy not marked");
    }
}
