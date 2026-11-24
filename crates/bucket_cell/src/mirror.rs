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
//! 1. In B, its **pin** ([`layout::pin`]), a reference of its own under the parent's prefix, which
//!    keeps what it copies from B's collector while it copies
//!    ([`crate::collector::Collector::collect_copy`]). One mirror of a version holds it at a time,
//!    its bytes its own: another's standing, the mirror is under way, and this one does nothing
//!    ([`Mirrored::UnderWay`]). A pin unrefreshed for [`PIN_STALE`] is its mirror's no longer, its
//!    host dead: the next mirror takes it over.
//! 2. Its **marker read in B**: there already, marked by a mirror begun before, on to step 7
//!    (`mirrorResumesMarked`), with no hold: the marking mirror's stands until B's collector
//!    deletes it, with the marker, and one written again after that would stand for good
//!    (`mirrorHoldsUnmarked`).
//! 3. In A, its **hold** ([`layout::hold`]), a reference of its own under the parent's prefix
//!    there (`mirrorHolds`), before it reads the head: it keeps the version whole in A, and the
//!    parent's cell from being removed with what the version inherits from other cells, which the
//!    mirror does not copy, while the copy stands. B's collector deletes it as the copy goes.
//! 4. The parent's **head read in A**, naming the version, or the mirror is given up, its hold and
//!    pin dropped (`mirrorChecks`): [`Mirrored::Unnamed`].
//! 5. **Every object of the version under the parent's prefix copied** into B, create-only: the
//!    checkpoint at the version, what recovering it read and the objects its extents are in, read
//!    where the router sends them. An object gone from A gives the mirror up, whatever order the
//!    copies end in (board 8439). Its pin refreshed as it goes, by its host's clock (board 8469),
//!    the mirror given up if it is no longer its own.
//! 6. In B, the create-only **marker** ([`layout::mirror`]), once every object is copied
//!    (`markerAfterCopy`), its pin refreshed right before, naming the epoch of the checkpoint it
//!    copied, written while its pin is its own ([`PIN_HELD`]): from then on, reads of the version
//!    in B are served there ([`crate::object_bucket::ObjectBucket`]).
//! 7. **The children of B referring in A listed**, after the marker (`mirrorListsAfterMark`), and
//!    their references written in B; then, the head read again in A naming the version still
//!    (`mirrorRereadsBeforeRetiring`), retired in A.
//! 8. The pin dropped, if still its own.
//!
//! A pin is a lease: a mirror creates its marker, which its pin guards, only within [`PIN_HELD`] of
//! a write of its pin that landed, so that it lands before the pin is stale and another mirror may
//! take it over (board 8441). Letting go, it deletes its hold, and its pin, only if they are still
//! of its own write ([`Bucket::delete_if`], board 9229, `releasesOwn`): a delete it sent landing
//! however late, after the next mirror took the pin over and wrote the hold again, deletes neither.
//! Its hold's bytes are its own, its pin's nonce, so that the next mirror's write of the hold is of
//! another version even where a version is the bytes' hash (S3's e-tag).
//!
//! Every step is create-only, idempotent, or a deletion: a mirror stopped anywhere is begun again
//! from the start, its pin taken over once stale, what it wrote kept. B's collector begins it
//! again, finding its pin stale. The next fork of the version into B, and the next opening of a
//! child of it in B, begin one stopped before its marker, once its pin is stale: one stopped after
//! it, they find marked, and leave to B's collector, the children not yet moved referring in A.
//!
//! A fork into B refers in B once the marker is there ([`Here`], [`crate::lifecycle`]).

use crate::bucket::{self, Bucket, Created, Meta, PutMode, Refused, Updated, Version};
use crate::layout::{self, CellId, Kind, Mirror, Reference};
use crate::recovery::{self, Checkpoints};
use crate::{Error, head, lifecycle};
use futures::future::BoxFuture;
use std::collections::BTreeSet;
use std::future::Future;
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
/// ([`crate::collector::Collector::collect_copy`]), and the next mirror of its version takes it
/// over. A mirror refreshes its pin a tenth of that apart while it copies ([`PIN_REFRESH`]), and
/// right before its marker, and writes what its pin guards only within [`PIN_HELD`] of a refresh.
pub const PIN_STALE: u64 = 10 * 60 * 1000;

/// How long a mirror goes from one refresh of its pin to the next while it copies, by its host's
/// monotonic clock (board 8469): the bucket's, which a pin's staleness is told by, may be stepped,
/// back or forward, as NTP steps a host's.
const PIN_REFRESH: Duration = Duration::from_millis(PIN_STALE / 10);

/// cladefs: how long after it sent a write of its pin that landed a mirror holds the pin for its
/// own still, by its host's monotonic clock (board 8441): its marker, which its pin guards, it
/// sends only within this. Half of [`PIN_STALE`]: the other half for a request so sent to land, and
/// for its host's clock and its bucket's to run apart, so that it lands before another mirror may
/// take the pin over. A host stopped past it, its pin taken over meanwhile, finds so as it writes
/// its pin again, conditionally, and writes nothing more. Not a fence of its deletes, which nothing
/// bounds the flight of: those are conditional on its own writes' versions (board 9229).
const PIN_HELD: Duration = Duration::from_millis(PIN_STALE / 2);

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

/// A mirror's pin, held: its bytes, its own, of its `nonce`, and the version of its last write.
struct Pin {
    key: String,
    nonce: u64,
    bytes: Vec<u8>,
    version: Version,
    /// When the write of it that landed last was sent, by the host's monotonic clock (board 8469):
    /// before its bucket took it, so that the pin is no older than this says, whatever the
    /// bucket's clock is stepped to.
    sent: Instant,
}

impl Pin {
    /// The pin of the mirror of `parent`'s version at `at` in `own`, made, or taken over from a
    /// mirror whose pin is stale: none if another mirror's stands.
    async fn take(own: &impl Bucket, parent: CellId, at: u64) -> Result<Option<Self>, Refused> {
        let key = layout::pin(parent, at);
        let nonce = rand::random();
        let bytes = layout::encode(&Reference::of(nonce));
        let sent = Instant::now();
        let version = match bucket::create(own, &key, bytes.clone()).await? {
            Created::Created(version) => version,
            Created::Exists(found) if stale(own, &found.meta) => {
                match bucket::update(own, &key, bytes.clone(), &found.meta.version).await? {
                    Updated::Updated(version) => version,
                    Updated::Changed => return Ok(None),
                }
            }
            Created::Exists(_) => return Ok(None),
        };
        Ok(Some(Self { key, nonce, bytes, version, sent }))
    }

    /// The bytes of its mirror's hold: its own, by its nonce, as its pin's are (board 9229).
    fn hold(&self) -> Vec<u8> {
        layout::encode(&Reference::of(self.nonce))
    }

    /// Writes it again, if [`PIN_REFRESH`] passed since it was last written, by its host's clock,
    /// or `now`: false if it is no longer this mirror's, taken over or deleted as stale.
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

    /// Sends `request`, of `key` in `bucket`, while the pin is this mirror's ([`PIN_HELD`]), and
    /// again while it fails, as [`bucket`]'s calls do: before each try, the pin written again once
    /// [`PIN_REFRESH`] passed, and again while that write landed past its lease. What it answered;
    /// none, and `request` sent no more, once the pin is no longer this mirror's (board 8441): a
    /// mirror stopped past its lease, its pin taken over since, then writes nothing the next
    /// mirror's pin guards.
    async fn holding<T, F: Future<Output = Result<T, bucket::Error>>>(
        &mut self,
        own: &impl Bucket,
        bucket: &impl Bucket,
        key: &str,
        mut request: impl FnMut() -> F,
    ) -> Result<Option<T>, Refused> {
        let mut failures = 0;
        loop {
            if !self.refresh(own, false).await? {
                return Ok(None);
            }
            // Written again, but landing past its lease: written again once more first.
            if self.sent.elapsed() >= PIN_HELD {
                continue;
            }
            match request().await {
                Ok(answer) => return Ok(Some(answer)),
                Err(error) => bucket::failed(bucket, &mut failures, error, key).await?,
            }
        }
    }

    /// Deletes it, if it is still of this mirror's last write (board 9229): taken over since, of
    /// another's, it stands, however late the delete lands.
    async fn release(self, own: &impl Bucket) -> Result<(), Refused> {
        bucket::delete_if(own, &self.key, &self.version).await
    }

    /// [`Pin::release`], deleting `hold` in `bucket` first if the pin is still this mirror's,
    /// written again first, conditionally, and the hold still of this mirror's write, its bytes
    /// its own (board 9229): read, and deleted only if of the version read. A mirror whose pin
    /// another took over since leaves both: that mirror's copy may be marked, its hold this one's
    /// bytes, written over it as this one went on, paused past its lease (hotbed review 53). The
    /// next mirror taking the pin over after the refresh writes the hold again, of its own bytes:
    /// it stands, the delete landing however late.
    async fn release_holding(
        mut self,
        own: &impl Bucket,
        bucket: &impl Bucket,
        hold: &str,
    ) -> Result<(), Refused> {
        if !self.refresh(own, true).await? {
            return Ok(());
        }
        if let Some(held) = bucket::get(bucket, hold).await?
            && held.bytes == self.hold()
        {
            bucket::delete_if(bucket, hold, &held.meta.version).await?;
        }
        self.release(own).await
    }

    /// Creates the marker at `marker` in `own`, while the pin is its own ([`Pin::holding`]):
    /// false, and none created, if it is no longer. One there already, written by this call or
    /// another mirror's, marks the version all the same.
    async fn mark(&mut self, own: &impl Bucket, marker: &str) -> Result<bool, Refused> {
        let marked = bytes::Bytes::from(layout::encode(&Mirror::NOW));
        let created = self.holding(own, own, marker, move || {
            let marked = marked.clone();
            async move {
                match own.put(marker, marked, PutMode::Create).await {
                    Err(bucket::Error::AlreadyExists) => Ok(()),
                    put => put.map(drop),
                }
            }
        });
        Ok(created.await?.is_some())
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
    // A backend that refuses conditional deletes (Azure through object_store) cannot safely
    // release a pin or hold. Check both buckets before either reference is written. Nothing
    // creates the probe's key, which routes as the hold does. GCS requires a nonzero generation
    // on deletes.
    let nonce = base32_id::Padded(rand::random());
    let probe = format!("{}probe/mirror-cleanup-{nonce}", parent.prefix());
    let unmatched =
        Version { e_tag: Some("\"cladefs-probe\"".to_owned()), version: Some("1".to_owned()) };
    bucket::delete_if(own, &probe, &unmatched).await?;
    bucket::delete_if(bucket, &probe, &unmatched).await?;
    let Some(mut pin) = Pin::take(own, parent, at).await? else {
        return Ok(Mirrored::UnderWay);
    };
    failpoint!("mirror::pinned");
    let (marker, hold) = (layout::mirror(parent, at), layout::hold(parent, at, number));
    let copied = match bucket::get(own, &marker).await? {
        // Marked already, by a mirror begun before: its copy whole, and the hold that mirror wrote
        // standing until B's collector deletes it, with the marker (`mirrorHoldsUnmarked`). One
        // written again here, after that collector listed the copy before this pin and deleted
        // the holds, would stand for good.
        Some(object) => {
            layout::Mirror::decode(&object.bytes)?;
            0
        }
        // Its hold in the parent's bucket, before it reads the head there (`mirrorHolds`).
        None => match hold_then_copy(bucket, own, &mut pin, (parent, at), &hold).await {
            Ok(Copied::Whole { count })
                if pin.refresh(own, true).await? && pin.mark(own, &marker).await? =>
            {
                failpoint!("mirror::marked");
                count
            }
            // Its pin another's now, and its hold that mirror's too.
            Ok(Copied::Whole { .. } | Copied::Taken) => {
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
        },
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

/// Writes `hold`, of `pin`'s mirror's own bytes, over any there (board 9229): a hold another
/// mirror of the version wrote, its pin since taken over, is this one's now, of another version,
/// which that mirror's delete, landing late, leaves. Retried while it fails.
async fn write_hold(bucket: &impl Bucket, hold: &str, pin: &Pin) -> Result<(), Refused> {
    let bytes = bytes::Bytes::from(pin.hold());
    let mut failures = 0;
    loop {
        match bucket.put(hold, bytes.clone(), PutMode::Overwrite).await {
            Ok(_) => return Ok(()),
            Err(error) => bucket::failed(bucket, &mut failures, error, hold).await?,
        }
    }
}

/// Gives a mirror up: its hold deleted in the parent's bucket, then its pin, each only if still of
/// its own write, its pin its own still, written again first. A pin another mirror took over, the
/// hold is that mirror's too, which it leaves.
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
    use crate::testing::{Direct, Fault, SimBucket};
    use futures::executor::block_on;
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
        let written = format!("@1 put {}", layout::pin(PARENT, 3));
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

    /// board 8441, board 9229: a mirror letting go sends its hold's delete; its host stops past
    /// its lease, the delete in flight all that time (nothing bounds it: no whole-request timeout,
    /// a file bucket's lock); the next mirror of the version takes its pin over and writes the hold
    /// again; then the old delete lands, and the old mirror goes on to delete its pin. Neither
    /// deletes what the next mirror wrote: each is conditional on the old one's own write. Before,
    /// a lease alone fenced them, and the late delete took the next mirror's hold, its copy then
    /// marked with nothing keeping the parent's cell in A.
    #[test]
    fn late_deletes_of_mirrors_letting_go_leave_the_next_ones_hold_and_pin() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let (pin_key, hold) = (layout::pin(PARENT, 3), layout::hold(PARENT, 3, 1));
        let first = pinned(&sim, &own);
        sim.served(write_hold(&*bucket, &hold, &first)).0.unwrap();

        // Its hold's delete sent, and held in flight.
        let deleting = format!("@2 delete {hold}");
        let in_flight = |asked: &str| asked.starts_with(&deleting);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut letting_go = std::pin::pin!(first.release_holding(&*own, &*bucket, &hold));
        loop {
            assert!(letting_go.as_mut().poll(&mut context).is_pending());
            if (0..sim.waiting()).any(|index| in_flight(&sim.describe(index))) {
                break;
            }
            sim.complete(0, Fault::None);
        }

        // Its host stopped past its lease; the next mirror takes the pin over, and holds.
        sim.advance(PIN_STALE + 1);
        advance(&runtime, Duration::from_millis(PIN_STALE + 1));
        let next = served_but(&sim, Pin::take(&*own, PARENT, 3), in_flight);
        let next = next.expect("the pin taken").unwrap().expect("the pin stale, taken over");
        served_but(&sim, write_hold(&*bucket, &hold, &next), in_flight).unwrap().unwrap();

        // The old delete lands; the old mirror goes on, and ends.
        let held = (0..sim.waiting()).find(|&index| in_flight(&sim.describe(index))).unwrap();
        sim.complete(held, Fault::None);
        let (released, _) = sim.served(letting_go);
        assert_eq!(released, Ok(()));
        let hold =
            block_on(Direct(bucket.clone()).get(&hold)).expect("the next mirror's hold gone");
        assert_eq!(hold.bytes, next.hold(), "the hold");
        let pin = block_on(Direct(own.clone()).get(&pin_key)).expect("the next mirror's pin gone");
        assert_eq!(pin.bytes, next.bytes, "the pin");
    }

    /// hotbed review 53: a mirror paused past its lease after it found no marker, before its hold,
    /// while the next mirror took its pin over, marked and let its pin go, its hold standing for
    /// the marked copy; going on, it writes the hold over, of its own bytes, and gives up (its
    /// version named no more): letting go, it finds its pin another's, gone, and leaves the hold,
    /// the marked copy's still. Deleting a hold of its own bytes, its pin not its own, took the
    /// marked copy's.
    #[test]
    fn mirrors_resumed_past_their_lease_leave_a_marked_copys_hold() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let hold = layout::hold(PARENT, 3, 1);
        let paused_one = pinned(&sim, &own);
        sim.advance(PIN_STALE + 1);
        advance(&runtime, Duration::from_millis(PIN_STALE + 1));
        let next = pinned(&sim, &own);
        sim.served(write_hold(&*bucket, &hold, &next)).0.unwrap();
        assert_eq!(sim.served(next.release(&*own)).0, Ok(()));

        sim.served(write_hold(&*bucket, &hold, &paused_one)).0.unwrap();
        assert_eq!(sim.served(paused_one.release_holding(&*own, &*bucket, &hold)).0, Ok(()));
        assert!(sim.stored().contains(&format!("@2/{hold}")), "the marked copy's hold deleted");
    }

    /// board 9229: a mirror letting go deletes its own hold and pin, with nothing else in the way:
    /// the conditions leave nothing standing that is its own.
    #[test]
    fn mirrors_letting_go_delete_their_own_hold_and_pin() {
        let runtime = paused();
        let _entered = runtime.enter();
        let sim = SimBucket::new();
        let (bucket, own) = (sim.at(1), sim.at(1).own());
        let hold = layout::hold(PARENT, 3, 1);
        let pin = pinned(&sim, &own);
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
        let bad_marker = layout::encode(&layout::Mirror { format: future });
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
}
