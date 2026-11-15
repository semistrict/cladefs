// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A branch's state, recovered from its objects: what a writer taking the branch starts from, and
//! what the collector keeps. Extents are read from where it says they are by fetchers
//! ([`crate::fetch`]).
//!
//! A branch's state is what its head names (doc-7, The head; TASK-407): its newest checkpoint, or,
//! with none, its parent's state where it was forked, then the footers of the segments the head
//! names after it, in order: read by their objects' tails, which hold them, many at once, every
//! key known. Each key is written once, so that what a reader reads at it is the one object it
//! names, ever. cladefs: a commit may span segments, each naming its runs of it, which a reader holds
//! until the segment ending it ([`Begun`], TASK-92.41): a head names whole commits alone. A state
//! recovered is recovered on by the segments the head names since ([`recover_on`]): a writer's,
//! once it holds the head, from what it found before.
//!
//! The collector deletes what no reader starting now needs: of what a head names, segments only
//! once its checkpoint covers them. A reader that finds something the head named gone reads the
//! head again: if it changed, a newer checkpoint may cover what went, and the reader starts again.
//!
//! cladefs: a recovery reads holding a ring ([`crate::keys::Ring`], TASK-411.3): its footers and
//! checkpoints are checked against their MACs under the keys it holds, and those of a branch it
//! holds sealed refused in the clear. The collector's holds none, reading them as it did.

use crate::bucket::{self, Bucket, Meta};
use crate::keys::Ring;
use crate::layout::{self, Branch, Cell, Contents, Footer, Fork, Head, Location, Origin};
use crate::record::Record;
use crate::{Error, head, object};
use extent_frame::spent::Spent;
use futures::future::{BoxFuture, FutureExt as _, Shared};
use futures::{StreamExt as _, TryStreamExt as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use storage_device::ExtentId;

/// What a branch holds as of a segment: where its extents are, `P`, and the rest of its state.
/// One replay makes it, [`State::apply`], whoever reads the branch's history: its recovery, so its
/// writers, its collector, its points and its deletion; a writer acknowledging its segments; and a
/// local store rebuilding it from the footers of its closed segments (TASK-165.3). Each places the
/// extents as it keeps them ([`Places`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State<P = Extents> {
    pub branch: Branch,
    pub extents: P,
    pub next_sequence: u64,
    pub root: Option<Vec<u8>>,
    /// The tag of the last commit, which may be the parent's, before the fork.
    pub last_commit: Option<u64>,
    pub forks: Vec<Fork>,
    /// The last segment applied.
    pub through: Option<u64>,
    /// The sequence numbers of the branch's own extents it freed, which it writes no more: one
    /// rule, every free of them a footer names (TASK-79.6, TASK-150.6, TASK-165.3).
    pub spent: Spent,
    /// The most bytes its extents may hold, as its last commit to set it set it, if one did: its
    /// own, or, before its first, its parent's at the fork (TASK-165.3).
    pub capacity: Option<u64>,
    /// cladefs: the runs of the commit the segments applied last went on with, which a later segment
    /// ends, if they did ([`Begun`], TASK-92.41): none in a state a head names, nor in any
    /// checkpoint, as writers acknowledge only segments ending their commits.
    pub begun: Option<Begun>,
}

/// cladefs: the runs a commit sealed in segments applied that a later segment ends
/// (`extent_frame::segment::Continued`, TASK-92.41): sealed with it, where they are, once it does;
/// or never, if the next segment applied neither goes on with it nor ends it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Begun {
    pub commit: u64,
    pub runs: Vec<(ExtentId, Location)>,
}

/// Where a branch's extents are in the bucket.
pub type Extents = imbl::OrdMap<ExtentId, Location>;

/// Where a branch's extents are, as a replay of its segments places them ([`State::apply`]).
pub trait Places {
    /// Seals `extent` where `seal` says, in the segment applied.
    fn seal(&mut self, extent: ExtentId, seal: Seal);

    /// Frees `extent`.
    fn free(&mut self, extent: ExtentId);
}

/// Where a replay seals an extent, in the bucket: in a run of its segment's data, or in an object
/// of its own, which its writer uploaded before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seal {
    Run(Location),
    Own(Location),
}

impl Places for Extents {
    fn seal(&mut self, extent: ExtentId, (Seal::Run(location) | Seal::Own(location)): Seal) {
        self.insert(extent, location);
    }

    fn free(&mut self, extent: ExtentId) {
        self.remove(&extent);
    }
}

impl State {
    pub fn new(branch: Branch) -> Self {
        Self::of(branch, Extents::new())
    }
}

impl<P> State<P> {
    /// The state of `branch` before its first segment, its extents placed in `extents`.
    pub fn of(branch: Branch, extents: P) -> Self {
        Self {
            branch,
            extents,
            next_sequence: 0,
            root: None,
            last_commit: None,
            forks: Vec::new(),
            through: None,
            spent: Spent::default(),
            capacity: None,
            begun: None,
        }
    }

    /// The state of `child`, forked from this one, before its first segment, its extents placed
    /// in `extents`: what it inherits but its extents, its root, its last commit and its capacity.
    fn inheriting<Q>(&self, child: Branch, extents: Q) -> State<Q> {
        State {
            root: self.root.clone(),
            last_commit: self.last_commit,
            capacity: self.capacity,
            ..State::of(child, extents)
        }
    }

    /// The state, its extents placed in what `place` makes of them.
    pub fn placed<Q>(self, place: impl FnOnce(P) -> Q) -> State<Q> {
        let Self {
            branch,
            extents,
            next_sequence,
            root,
            last_commit,
            forks,
            through,
            spent,
            capacity,
            begun,
        } = self;
        let extents = place(extents);
        State {
            branch,
            extents,
            next_sequence,
            root,
            last_commit,
            forks,
            through,
            spent,
            capacity,
            begun,
        }
    }
}

impl<P: Places> State<P> {
    /// Applies `footer`'s segment, the next its head names.
    pub fn apply(&mut self, footer: &Footer) {
        self.through = Some(footer.seq);
        self.apply_contents(footer.seq, footer.epoch, &footer.contents);
    }

    /// Applies `contents`, those of the branch's segment `seq`, of the writer of lease epoch
    /// `epoch`: what its commits sealed, where they sealed it, then what they freed; the root they
    /// set, their last commit, the branch's next sequence number, the largest named, the forks at
    /// its end, and the capacity they set. Every free of the branch's own extents spends it, as
    /// the store that freed it spent it: a footer frees only what a commit held, not what was
    /// freed while open (TASK-160.15), and one sealed and freed within the segment is a free and no
    /// seal (TASK-165.3). cladefs: first, the runs of a commit the segments before began ([`Begun`]),
    /// if this one ends it, or holding them on if it goes on with it; then, after its own, holding
    /// those of the commit it goes on with, if any (TASK-92.41).
    pub fn apply_contents(&mut self, seq: u64, epoch: u64, contents: &Contents) {
        let key = layout::segment(self.branch, seq, epoch);
        match self.begun.take() {
            Some(begun) if contents.ends(begun.commit) => {
                for (extent, location) in begun.runs {
                    self.extents.seal(extent, Seal::Run(location));
                }
            }
            Some(begun) if contents.continues(begun.commit) => self.begun = Some(begun),
            // Never ended: a writer's, never acknowledged. A head names none such: a store's
            // replay of its own segments, closed again, may (wal::recover).
            Some(_) | None => {}
        }
        for &(extent, offset, len) in &contents.extents {
            self.extents.seal(extent, Seal::Run(Location { key: key.clone(), offset, len }));
        }
        for (extent, location) in contents.own_at(self.branch, epoch, seq) {
            self.extents.seal(extent, Seal::Own(location));
        }
        for &extent in &contents.frees {
            self.extents.free(extent);
            if extent.branch == self.branch.id {
                self.spent.insert(extent.sequence);
            }
        }
        if let Some(root) = &contents.root {
            self.root = Some(root.clone());
        }
        if let Some(&commit) = contents.commits.last() {
            self.last_commit = Some(commit);
        }
        self.next_sequence = self.next_sequence.max(contents.next_sequence);
        self.forks.extend(contents.forks.iter().map(|child| Fork::at(seq, child)));
        if contents.capacity.is_some() {
            self.capacity = contents.capacity;
        }
        if let Some(continued) = &contents.continued {
            let runs = continued
                .extents
                .iter()
                .map(|&(extent, offset, len)| (extent, Location { key: key.clone(), offset, len }));
            match &mut self.begun {
                Some(begun) => begun.runs.extend(runs),
                None => {
                    let runs = runs.collect();
                    self.begun = Some(Begun { commit: continued.commit, runs });
                }
            }
        }
    }
}

/// A branch's state as its head names it, and what reading it took: its [`State`], or its
/// [`Layers`].
#[derive(Clone, Debug)]
pub struct Recovered<S = State> {
    pub state: S,
    /// The head it was recovered from, as read: none if the branch has none, as a fork's child or
    /// a point before any writer took it. Its end is where the next writer's segments go.
    pub head: Option<Head>,
    /// The keys it read, which the collector keeps.
    pub touched: BTreeSet<String>,
}

impl<S> Recovered<S> {
    /// The number of segments the branch holds, where the next writer's go.
    pub fn end(&self) -> u64 {
        self.head.as_ref().map_or(0, |head| head.end)
    }

    /// Its newest checkpoint, by its number and its writer's epoch, if it started from one.
    pub fn checkpoint(&self) -> Option<(u64, u64)> {
        self.head.as_ref().and_then(|head| head.checkpoint)
    }

    /// The epoch of its head as read: 0 if it has none.
    pub fn epoch(&self) -> u64 {
        self.head.as_ref().map_or(0, |head| head.epoch)
    }
}

impl Recovered<Layers> {
    /// The branch's state, its layers in one.
    pub(crate) fn flatten(self) -> Recovered {
        let Self { state, head, touched } = self;
        let state = state.placed(Layered::flatten);
        Recovered { state, head, touched }
    }
}

/// A branch's state in layers ([`Layered`]).
pub(crate) type Layers = State<Layered>;

/// Where a branch's extents are, in layers: what it inherited, if it has no checkpoint of its own,
/// its parent's state where it was forked, shared, as read, with the other forks of the parent
/// there; over it, what its segments made of it since. The collector, recovering many forks of a
/// base, takes what they inherited together once, not each fork's copy (TASK-92.7).
#[derive(Clone, Debug)]
pub(crate) struct Layered {
    pub inherited: Option<Arc<State>>,
    /// Where its extents are but those it inherited and holds still: all of them, with nothing
    /// inherited.
    pub own: Extents,
    /// The extents it inherited that it holds no longer where it inherited them: freed, or sealed
    /// again.
    pub shadowed: BTreeSet<ExtentId>,
}

impl Layered {
    /// `state`, in one layer.
    fn of(state: State) -> Layers {
        state.placed(|own| Self { inherited: None, own, shadowed: BTreeSet::new() })
    }

    /// The state of `child`, forked from `inherited`, before its segments.
    fn inheriting(inherited: Arc<State>, child: Branch) -> Layers {
        let extents = Self { inherited: None, own: Extents::new(), shadowed: BTreeSet::new() };
        let mut state = inherited.inheriting(child, extents);
        state.extents.inherited = Some(inherited);
        state
    }

    /// Shadows `extent` if it inherited it.
    fn shadow(&mut self, extent: ExtentId) {
        let inherited = self.inherited.as_ref();
        if inherited.is_some_and(|inherited| inherited.extents.contains_key(&extent)) {
            self.shadowed.insert(extent);
        }
    }

    /// Where its extents are, in one: what it inherited and holds still, and its own. What it
    /// inherited is shared, not copied: the map is persistent, so this costs what its own layer
    /// holds, not what it inherited (TASK-92.55).
    fn flatten(self) -> Extents {
        let Self { inherited, own, shadowed } = self;
        let Some(inherited) = inherited else { return own };
        let mut extents = inherited.extents.clone();
        for extent in &shadowed {
            extents.remove(extent);
        }
        own.union(extents)
    }
}

impl Places for Layered {
    fn seal(&mut self, extent: ExtentId, seal: Seal) {
        self.shadow(extent);
        self.own.seal(extent, seal);
    }

    fn free(&mut self, extent: ExtentId) {
        self.shadow(extent);
        self.own.free(extent);
    }
}

/// How often recovery starts again before it gives up on a branch as lost.
const RESTARTS: usize = 64;

/// The ring of a recovery holding no key.
static CLEAR: std::sync::LazyLock<Arc<Ring>> = std::sync::LazyLock::new(Ring::clear);

/// How many segments' footers recovery reads at once, at most: their keys are known, the head
/// naming them, so that a recovery reads none it needs not.
const AT_ONCE: usize = 32;

enum Attempt {
    Restart,
    Failed(Error),
}

impl From<Error> for Attempt {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

/// The state of `branch`, forked from `origin`, as its head names it, read holding no key.
pub async fn recover(
    bucket: &impl Bucket,
    branch: Branch,
    origin: Option<Origin>,
) -> Result<Recovered, Error> {
    recover_under(bucket, branch, origin, &Ring::clear()).await
}

/// cladefs: the state of `branch`, forked from `origin`, read holding `ring` (TASK-411.3).
pub async fn recover_under(
    bucket: &impl Bucket,
    branch: Branch,
    origin: Option<Origin>,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let checkpoints = Checkpoints::new(bucket, 0).holding(ring);
    Ok(recover_layers(&checkpoints, branch, origin).await?.flatten())
}

/// The state of `branch`, forked from `origin`, as `head` names it, a head its reader holds, as a
/// writer that took it does, or a deletion that ended it: nothing it names goes while it holds it,
/// but for a takeover since, which fails the writer at its next swap ([`Error::Fenced`]). What it
/// inherited may go meanwhile, a fork point's state its parent's checkpoint covers: it starts
/// again. cladefs: read holding `ring` (TASK-411.3).
pub async fn recover_from(
    bucket: &impl Bucket,
    branch: Branch,
    origin: Option<Origin>,
    head: Option<Head>,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let checkpoints = Checkpoints::new(bucket, 0).holding(ring);
    for _ in 0..RESTARTS {
        match attempt(&checkpoints, branch, origin, head.clone()).await {
            Ok(recovered) => return Ok(recovered.flatten()),
            Err(Attempt::Restart) if head::get(bucket, branch).await? != head => {
                return Err(Error::Fenced);
            }
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!("branch {branch}: what its recovery starts from keeps going")))
}

/// The state of `branch`, forked from `origin`, as [`recover`] finds it, in layers: reading the
/// checkpoint it starts from through `checkpoints`, which other recoveries share.
pub(crate) async fn recover_layers<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    branch: Branch,
    origin: Option<Origin>,
) -> Result<Recovered<Layers>, Error> {
    for _ in 0..RESTARTS {
        let head = head::get(checkpoints.bucket, branch).await?;
        match attempt(checkpoints, branch, origin, head).await {
            Ok(recovered) => return Ok(recovered),
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!("branch {branch}: what its recovery starts from keeps going")))
}

/// cladefs: the state of `branch` as of the end of its segment `at`, which its head names: a fork
/// point's, as a fork made there inherits it; and what reading it read, which the collector keeps
/// for the forks referring to it (doc-7, A pass over a cell; TASK-395.4). Its newest checkpoint up
/// to there, or its origin's state, then its segments to there, starting again where it finds one
/// gone that a newer checkpoint covers: [`Error::Lost`] once it keeps going, or what it needs is
/// gone.
pub(crate) async fn recover_at<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    branch: Branch,
    at: u64,
) -> Result<(Arc<State>, BTreeSet<String>), Error> {
    for _ in 0..RESTARTS {
        let mut touched = BTreeSet::new();
        match state_at(checkpoints, branch, at, &mut touched, branch.cell, true).await {
            Ok(state) => return Ok((state, touched)),
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!(
        "branch {branch} at {at:x}: what its recovery starts from keeps going"
    )))
}

/// Checkpoints read whole, for recoveries, each read once for those that start from it at once,
/// and for those after while it is among the last few read: a base's recovery and its forks',
/// those without checkpoints of their own starting from the base's, share one, as a pass of the
/// collector's recovering them all does (TASK-92.7).
pub(crate) struct Checkpoints<'a, B: Bucket> {
    bucket: &'a B,
    /// How many of those read it keeps, the last read.
    keep: usize,
    read: Mutex<Read<'a>>,
    /// cladefs: a listing of the cell, taken before the recoveries began, and those it gave a
    /// listing of checkpoints once: each branch's, and each fork point's a pass keeps (TASK-395.16,
    /// TASK-395.19, [`Checkpoints::listed`]).
    listed: Option<&'a BTreeMap<String, Meta>>,
    served: Mutex<BTreeSet<(Branch, Option<u64>)>>,
    /// cladefs: the keys it reads under ([`Checkpoints::holding`], TASK-411.3).
    keys: &'a Ring,
}

/// The reads of checkpoints [`Checkpoints`] keeps, or that are in flight, by their keys; and the
/// keys in the order they were read.
#[derive(Default)]
struct Read<'a> {
    flights: HashMap<String, Flight<'a>>,
    order: VecDeque<String>,
}

/// A read of a checkpoint, or what it found: the state it holds, if it is there.
type Flight<'a> = Shared<BoxFuture<'a, Result<Option<Arc<State>>, Error>>>;

impl<'a, B: Bucket> Checkpoints<'a, B> {
    /// Reads checkpoints from `bucket`, keeping the last `keep` read.
    pub(crate) fn new(bucket: &'a B, keep: usize) -> Self {
        let keys = &**CLEAR;
        Self { bucket, keep, read: Mutex::default(), listed: None, served: Mutex::default(), keys }
    }

    /// cladefs: these, reading their checkpoints and the footers of their segments holding `keys`
    /// (TASK-411.3).
    pub(crate) fn holding(self, keys: &'a Ring) -> Self {
        Self { keys, ..self }
    }

    /// cladefs: these, the first listing of each branch's checkpoints served by `listed`, a listing
    /// of the cell the collector's pass took before it read anything (TASK-395.16), if it has one:
    /// a LIST less a branch. A fork point's state ([`state_at`]) is as right from the newest
    /// checkpoint up to it listed then as from one listed now: it reads its segments after it, and,
    /// finding one gone, lists the checkpoints again, starting over if a newer one has come; and a
    /// recovery starting over lists them anew, from the bucket. Each branch's once: what started
    /// over may be what the listing shows.
    ///
    /// And the first of each fork point's state a pass keeps for the references and forks to other
    /// cells ([`recover_at`]), apart from its branch's and from one another's (TASK-395.19): a LIST
    /// less a fork point, which a pass over a cell whose branch was forked at many points made for
    /// each. A state as of a segment is the same from any checkpoint up to there, the newest listed
    /// then as the newest listed now, its segments to there read after it: one it finds gone, a
    /// newer checkpoint covering it, starts it over, listing anew. A checkpoint made since the
    /// listing, at or before the fork point, which the recovery doesn't read, the pass doesn't
    /// delete either: what it deletes it listed. An origin's state a branch's recovery starts from
    /// goes by the branch's listing, as before: served anew, it would read none of the checkpoints
    /// made during the pass, keeping their segments instead, safe but hiding what the test
    /// `children_restored_from_points_keeping_their_origins_need_recoveries_by_origins` shows.
    pub(crate) fn listed(self, listed: Option<&'a BTreeMap<String, Meta>>) -> Self {
        Self { listed, ..self }
    }

    /// cladefs: the checkpoints of `branch`, in the order of their keys, for a recovery of it, or of
    /// the state of it as of its segment `at` a pass keeps: as the listing of its cell shows them,
    /// the first time one of that is asked, if it has one; listed now otherwise.
    async fn list(&self, branch: Branch, at: Option<u64>) -> Vec<Meta> {
        let prefix = layout::checkpoints(branch);
        match self.listed {
            Some(listed) if self.served.lock().unwrap().insert((branch, at)) => {
                bucket::under(listed, &prefix)
            }
            _ => bucket::list(self.bucket, &prefix).await,
        }
    }

    /// The state the checkpoint `listed` of `branch` holds, if it is there ([`Checkpoints::read_key`]).
    async fn read(&self, listed: &Meta, branch: Branch) -> Result<Option<Arc<State>>, Error> {
        self.read_key(&listed.key, branch).await
    }

    /// The state the checkpoint at `key` of `branch` holds, if it is there: read now, or by the
    /// read of it in flight or kept, or as the bucket decoded it before ([`Bucket::decoded`]), by
    /// its key alone, written once (TASK-407). One gone, or that failed, is read anew the next time.
    async fn read_key(&self, key: &str, branch: Branch) -> Result<Option<Arc<State>>, Error> {
        let flight = {
            let mut read = self.read.lock().unwrap();
            match read.flights.get(key) {
                Some(flight) => flight.clone(),
                None => {
                    let (bucket, keys) = (self.bucket, self.keys);
                    let owned = key.to_owned();
                    let flight = async move {
                        let key = owned;
                        let decoded = bucket.decoded();
                        if let Some(state) = decoded.and_then(|decoded| decoded.get(&key)) {
                            return Ok(Some(state));
                        }
                        let Some(object) = bucket::get(bucket, &key).await else {
                            return Ok(None);
                        };
                        let state = Arc::new(checkpoint(&key, &object.bytes, branch, keys)?);
                        if let Some(decoded) = decoded {
                            decoded.keep(&key, &state);
                        }
                        Ok(Some(state))
                    };
                    let flight = flight.boxed().shared();
                    if self.keep > 0 {
                        read.flights.insert(key.to_owned(), flight.clone());
                        read.order.push_back(key.to_owned());
                        while read.order.len() > self.keep {
                            let oldest = read.order.pop_front().unwrap();
                            read.flights.remove(&oldest);
                        }
                    }
                    flight
                }
            }
        };
        let state = flight.await;
        if !matches!(state, Ok(Some(_))) {
            let mut read = self.read.lock().unwrap();
            read.flights.remove(key);
            read.order.retain(|kept| kept != key);
        }
        state
    }
}

/// cladefs: the states checkpoints hold, decoded, which the recoveries reading through a bucket
/// share ([`Bucket::decoded`]), by their checkpoints' keys, each written once (TASK-407). A
/// state's extents are a persistent map, which a recovery starting from it takes for nothing; the
/// last [`Decoded::KEEP`] decoded are kept: those the forks being opened start from (TASK-92.55).
#[derive(Default)]
pub struct Decoded {
    kept: Mutex<Kept>,
}

/// The states [`Decoded`] keeps, by their checkpoints' keys, and those, oldest first.
#[derive(Default)]
struct Kept {
    states: HashMap<String, Arc<State>>,
    order: VecDeque<String>,
}

impl Decoded {
    /// How many states it keeps: a host's forks open from a few bases' newest checkpoints at once.
    const KEEP: usize = 16;

    /// The state the checkpoint at `key` holds, if it kept it.
    fn get(&self, key: &str) -> Option<Arc<State>> {
        self.kept.lock().unwrap().states.get(key).cloned()
    }

    /// Keeps `state`, which the checkpoint at `key` holds.
    fn keep(&self, key: &str, state: &Arc<State>) {
        let mut kept = self.kept.lock().unwrap();
        if kept.states.insert(key.to_owned(), state.clone()).is_none() {
            kept.order.push_back(key.to_owned());
        }
        while kept.order.len() > Self::KEEP {
            let oldest = kept.order.pop_front().unwrap();
            kept.states.remove(&oldest);
        }
    }
}

/// The state of the branch `before` recovered, forked from `origin`, recovered on from it to what
/// `head` names, a head its reader holds or read since: the segments it names from `before`'s end
/// applied to its state, rather than its state read anew. As recovering anew would find; but if
/// the head names a checkpoint as of `before`'s end or later, which may cover segments gone, it
/// recovers anew.
/// cladefs: read holding `ring` (TASK-411.3).
pub async fn recover_on(
    bucket: &impl Bucket,
    origin: Option<Origin>,
    before: Recovered,
    head: Option<Head>,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let Recovered { state, head: was, touched } = before;
    let branch = state.branch;
    let from = was.as_ref().map_or(0, |was| was.end);
    // A checkpoint since, covering what it would follow on from: the collector, going by it, may
    // have deleted those segments, and it recovers anew.
    if head.as_ref().and_then(|head| head.checkpoint).is_some_and(|(seq, _)| seq >= from) {
        return recover_from(bucket, branch, origin, head, ring).await;
    }
    let mut recovered = Recovered { state: Layered::of(state), head: head.clone(), touched };
    match follow((bucket, ring), &mut recovered, from).await {
        Ok(()) => Ok(recovered.flatten()),
        Err(Attempt::Restart) => recover_from(bucket, branch, origin, head, ring).await,
        Err(Attempt::Failed(error)) => Err(error),
    }
}

/// The state `head`, `branch`'s, forked from `origin`, names: its checkpoint, or its origin's
/// state, then its segments after. Something it names gone, it starts again if the head changed.
async fn attempt<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    branch: Branch,
    origin: Option<Origin>,
    head: Option<Head>,
) -> Result<Recovered<Layers>, Attempt> {
    let bucket = checkpoints.bucket;
    let mut touched = BTreeSet::new();
    let checkpoint = head.as_ref().and_then(|head| head.checkpoint(branch));
    let state = match checkpoint {
        Some(key) => {
            let Some(state) = checkpoints.read_key(&key, branch).await? else {
                return Err(moved(bucket, branch, head.as_ref()).await);
            };
            touched.insert(key.clone());
            if state.through.is_none() {
                return Err(Attempt::Failed(Error::Corrupt(key)));
            }
            Layered::of(Arc::unwrap_or_clone(state))
        }
        None => match origin {
            None => Layered::of(State::new(branch)),
            Some(origin) => {
                let at = state_at(
                    checkpoints,
                    origin.parent,
                    origin.at,
                    &mut touched,
                    branch.cell,
                    false,
                )
                .await;
                match at {
                    Ok(at) => Layered::inheriting(at, branch),
                    // A checkpoint of its own since, which the collector going by may have let
                    // what it inherits go for: the head changed (TASK-177).
                    Err(Attempt::Failed(Error::Lost(lost))) => {
                        return Err(match moved(bucket, branch, head.as_ref()).await {
                            Attempt::Restart => Attempt::Restart,
                            Attempt::Failed(_) => Attempt::Failed(Error::Lost(lost)),
                        });
                    }
                    Err(failed) => return Err(failed),
                }
            }
        },
    };
    let from = state.through.map_or(0, |seq| seq + 1);
    let mut recovered = Recovered { state, head, touched };
    follow((bucket, checkpoints.keys), &mut recovered, from).await?;
    Ok(recovered)
}

/// `recovered`, the segments its head names from `from` on applied, read by their tails,
/// [`AT_ONCE`] at most at once, holding `keys`. One gone, the head is read again: changed, a checkpoint newer may
/// cover it, and the recovery starts again; else what it names is lost.
async fn follow<B: Bucket>(
    (bucket, keys): (&B, &Ring),
    recovered: &mut Recovered<Layers>,
    from: u64,
) -> Result<(), Attempt> {
    let Recovered { state, head, touched } = recovered;
    let Some(named) = head.as_ref() else { return Ok(()) };
    let branch = state.branch;
    let mut footers = futures::stream::iter(named.segments(from))
        .map(|(seq, epoch)| async move {
            let key = layout::segment(branch, seq, epoch);
            let footer = segment_footer((bucket, keys), branch, seq, epoch).await?;
            Ok::<_, Error>((key, footer))
        })
        .buffered(AT_ONCE);
    while let Some((key, footer)) = footers.try_next().await? {
        let Some(footer) = footer else {
            drop(footers);
            return Err(moved(bucket, branch, head.as_ref()).await);
        };
        state.apply(&footer);
        touched.insert(key);
    }
    Ok(())
}

/// What a recovery of `branch` from `head` that found something it names gone does: start again
/// if the head changed since, which a checkpoint newer may have; fail as lost otherwise.
async fn moved(bucket: &impl Bucket, branch: Branch, head: Option<&Head>) -> Attempt {
    match head::get(bucket, branch).await {
        Err(error) => Attempt::Failed(error),
        Ok(now) if now.as_ref() != head => Attempt::Restart,
        Ok(_) => Attempt::Failed(Error::Lost(format!("branch {branch}: what its head names"))),
    }
}

/// The footer of segment `seq` of `branch` by the writer of `epoch`, if it is there: read by its
/// object's tail, a ranged read of its last bytes, which hold it ([`object::tail`]), not by the
/// segment whole. cladefs: checked against its MAC if `keys` hold its key, and refused in the clear
/// if they hold its branch sealed (TASK-411.3).
async fn segment_footer(
    (bucket, keys): (&impl Bucket, &Ring),
    branch: Branch,
    seq: u64,
    epoch: u64,
) -> Result<Option<Footer>, Error> {
    let key = layout::segment(branch, seq, epoch);
    let Some(tail) = object::tail(bucket, &key).await? else { return Ok(None) };
    if !keys.is_empty() {
        tail.open(&keys.of(branch.id)).map_err(|err| err.reading(&key))?;
    }
    Ok(Some(placed(&key, layout::decode(tail.footer())?, branch, seq, epoch)?))
}

/// `footer`, that of the object at `key`, which holds segment `seq` of `branch` by the writer of
/// `epoch`, if it says so.
fn placed(
    key: &str,
    footer: Footer,
    branch: Branch,
    seq: u64,
    epoch: u64,
) -> Result<Footer, Error> {
    if footer.branch != branch || footer.seq != seq || footer.epoch != epoch {
        return Err(Error::Corrupt(format!(
            "{key} holds segment {:x} of {} by epoch {:x}",
            footer.seq, footer.branch, footer.epoch
        )));
    }
    Ok(footer)
}

/// The state checkpoint `key` of `branch`, whose object holds `bytes`, holds: cladefs: read holding
/// `keys` (TASK-411.3).
fn checkpoint(key: &str, bytes: &[u8], branch: Branch, keys: &Ring) -> Result<State, Error> {
    let state = crate::checkpoint::decode_under(bytes, &keys.of(branch.id))
        .map_err(|err| err.reading(key))?;
    if state.branch != branch {
        return Err(Error::Corrupt(format!("{key} holds a checkpoint of {}", state.branch)));
    }
    Ok(state)
}

/// The newest of `branch`'s checkpoints `listed` as of its segment `at` or before: by number, then
/// by its writer's epoch.
fn newest_to(listed: &[Meta], at: u64) -> Option<&Meta> {
    listed
        .iter()
        .filter_map(|meta| match layout::kind(&meta.key) {
            layout::Kind::Checkpoint { seq, epoch, .. } if seq <= at => Some(((seq, epoch), meta)),
            _ => None,
        })
        .max_by_key(|(place, _)| *place)
        .map(|(_, meta)| meta)
}

/// The state of `branch` as of the end of its segment `at`, which its head names: its newest
/// checkpoint up to there, or its origin's state, then the segments its head names up to there. A
/// checkpoint at `at` itself is the state, as read, which the forks made there share.
///
/// cladefs: for a fork in `from`, a cell of its own, of `branch` in another, its checkpoint at `at`
/// alone, of the epoch of the segment there, read by its key, listing nothing of the other cell's
/// (TASK-395.4): whoever records a fork across cells writes it first ([`checkpoint_at`]), and the
/// parent's cell keeps it, the fork point's state, while the fork's reference stands. Gone, what
/// the fork inherited is.
///
/// cladefs: `kept`, a fork point's state a pass keeps ([`recover_at`]), its own first listing of the
/// checkpoints served by the pass's listing of the cell ([`Checkpoints::listed`]); else, an
/// origin's state a recovery of a branch starts from, the branch's listing.
fn state_at<'a, B: Bucket>(
    checkpoints: &'a Checkpoints<'_, B>,
    branch: Branch,
    at: u64,
    touched: &'a mut BTreeSet<String>,
    from: Cell,
    kept: bool,
) -> BoxFuture<'a, Result<Arc<State>, Attempt>> {
    Box::pin(async move {
        let bucket = checkpoints.bucket;
        let head = head::get(bucket, branch).await?;
        let Some(epoch) = head.as_ref().and_then(|head| head.epoch_of(at)) else {
            return Err(Attempt::Failed(Error::Lost(format!("segment {at:x} of {branch}"))));
        };
        // cladefs: a fork point's a pass keeps for the forks to other cells is read as they read
        // it, by the key of its checkpoint alone, if there is one: a later writer's checkpoint at
        // the segment, at its own key, may name the extents along other moves, of other passes,
        // than the forks follow from it (TASK-407, TASK-407.1). None yet, no fork read it: the
        // newest checkpoint up to there, as whoever writes it reads it.
        if branch.cell != from || kept {
            let key = layout::checkpoint(branch, at, epoch);
            match checkpoints.read_key(&key, branch).await? {
                Some(state) if state.through != Some(at) => {
                    let corrupt = Error::Corrupt(format!("{key} holds another state"));
                    return Err(Attempt::Failed(corrupt));
                }
                Some(state) => {
                    touched.insert(key);
                    return Ok(state);
                }
                None if kept => {}
                None => return Err(Attempt::Failed(Error::Lost(key))),
            }
        }
        let listed = checkpoints.list(branch, kept.then_some(at)).await;
        let mut state = match newest_to(&listed, at) {
            Some(meta) => {
                let Some(state) = checkpoints.read(meta, branch).await? else {
                    return Err(Attempt::Restart);
                };
                touched.insert(meta.key.clone());
                if state.through == Some(at) {
                    return Ok(state);
                }
                Layered::of(Arc::unwrap_or_clone(state))
            }
            None => {
                let key = layout::record(branch);
                let inherited = match bucket::get(bucket, &key).await {
                    None => Err(Attempt::Failed(Error::Lost(key))),
                    Some(object) => {
                        touched.insert(key);
                        match Record::decode(branch, &object.bytes)?.origin() {
                            None => Ok(None),
                            Some(origin) => {
                                let from = branch.cell;
                                state_at(
                                    checkpoints,
                                    origin.parent,
                                    origin.at,
                                    touched,
                                    from,
                                    false,
                                )
                                .await
                                .map(Some)
                            }
                        }
                    }
                };
                match inherited {
                    Ok(None) => Layered::of(State::new(branch)),
                    Ok(Some(inherited)) => Layered::inheriting(inherited, branch),
                    Err(failed) => {
                        return Err(unless_checkpointed(bucket, branch, at, failed).await);
                    }
                }
            }
        };
        let from = state.through.map_or(0, |seq| seq + 1);
        let named = head.as_ref().expect("a head naming the segment at");
        let mut footers =
            futures::stream::iter(named.segments(from).take_while(|&(seq, _)| seq <= at))
                .map(|(seq, epoch)| async move {
                    let key = layout::segment(branch, seq, epoch);
                    Ok::<_, Error>((
                        key,
                        segment_footer((bucket, checkpoints.keys), branch, seq, epoch).await?,
                    ))
                })
                .buffered(AT_ONCE);
        while let Some((key, footer)) = footers.try_next().await? {
            // A segment the head names goes only once a checkpoint covers it.
            let Some(footer) = footer else { return Err(Attempt::Restart) };
            state.apply(&footer);
            touched.insert(key);
        }
        drop(footers);
        Ok(Arc::new(state.placed(Layered::flatten)))
    })
}

/// cladefs: makes sure `branch` has a checkpoint as of its segment `at`, which its head names: the
/// state a fork made there to another cell inherits, which the fork's recovery reads by its key
/// alone, so that a pass over the fork's cell lists nothing of `branch`'s ([`state_at`],
/// TASK-395.4). Written, create-only, at the key of the epoch of that segment's writer, by
/// whoever records such a fork, before its reference and its record
/// ([`crate::branch::record_fork`]), from the newest checkpoint up to there and the segments
/// after: the state any reader finds there, its extents named where they were sealed, never
/// further along their moves than a reader names them. One there stands, any writer's (TASK-407.6).
/// [`Error::Lost`] if the state there is gone: no reader recovers a fork from there either.
///
/// cladefs: authenticated under the DEK `keys` give, `branch`'s, if its objects are sealed, as its
/// writers' checkpoints are: a fork's child reading it holding `branch`'s keys refuses one in the
/// clear (TASK-411.4). Asked for only if it is to be written.
pub(crate) async fn checkpoint_at(
    bucket: &impl Bucket,
    branch: Branch,
    at: u64,
    keys: crate::keys::Parents<'_>,
) -> Result<(), Error> {
    let head = head::get(bucket, branch).await?;
    let Some(epoch) = head.as_ref().and_then(|head| head.epoch_of(at)) else {
        return Err(Error::Lost(format!("segment {at:x} of {branch}")));
    };
    let key = layout::checkpoint(branch, at, epoch);
    let listed = bucket::list(bucket, &layout::checkpoints(branch)).await;
    if listed.iter().any(|meta| meta.key == key) {
        return Ok(());
    }
    let keys = keys.ring(bucket, branch).await?;
    debug_assert!(keys.sealing().is_none_or(|(_, id)| id.branch == branch.id));
    let checkpoints = Checkpoints::new(bucket, 0);
    for _ in 0..RESTARTS {
        match state_at(&checkpoints, branch, at, &mut BTreeSet::new(), branch.cell, false).await {
            Ok(state) => {
                let checkpoint = crate::checkpoint::encode_under(&state, keys.sealing());
                bucket::create(bucket, &key, checkpoint).await;
                return Ok(());
            }
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!(
        "branch {branch} at {at:x}: what its recovery starts from keeps going"
    )))
}

/// What `failed` gives, a recovery of `branch` that found no checkpoint of it up to its segment
/// `at` and went to what it inherited instead: one that found that lost starts again if a
/// checkpoint of the branch up to there has come since, which the collector, going by it, may have
/// let what it found lost go for: the record of a parent deleted, which no other recovery reads
/// (doc-7, Checkpoints; TASK-177).
async fn unless_checkpointed(
    bucket: &impl Bucket,
    branch: Branch,
    at: u64,
    failed: Attempt,
) -> Attempt {
    if !matches!(failed, Attempt::Failed(Error::Lost(_))) {
        return failed;
    }
    let listed = bucket::list(bucket, &layout::checkpoints(branch)).await;
    if newest_to(&listed, at).is_some() { Attempt::Restart } else { failed }
}

/// Whether the branch `origin` names holds the fork made at the end of its segment there to
/// `child`, of `owner`: its head names the segment, whose footer names the fork, and the state as
/// of it reads whole, as the fork's recovery from there needs. Asked once: what is going, as a pass
/// of the collector takes it, reads as not held.
pub(crate) async fn holds_fork(
    bucket: &impl Bucket,
    origin: Origin,
    child: Branch,
    owner: u64,
) -> Result<bool, Error> {
    let Origin { parent, at } = origin;
    let Some(epoch) = head::get(bucket, parent).await?.and_then(|head| head.epoch_of(at)) else {
        return Ok(false);
    };
    let Some(footer) = segment_footer((bucket, &**CLEAR), parent, at, epoch).await? else {
        return Ok(false);
    };
    let forked = |fork: &layout::Child| fork.branch == child && fork.owner == owner;
    if !footer.contents.forks.iter().any(forked) {
        return Ok(false);
    }
    let checkpoints = Checkpoints::new(bucket, 0);
    match state_at(&checkpoints, parent, at, &mut BTreeSet::new(), parent.cell, false).await {
        Ok(_) => Ok(true),
        Err(Attempt::Restart | Attempt::Failed(Error::Lost(_))) => Ok(false),
        Err(Attempt::Failed(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Batch, Commit};
    use crate::branch;
    use crate::layout::test_branch;
    use crate::layout::{Child, Continued, Segment, child};
    use crate::store::Store;
    use crate::testing::SimBucket;
    use crate::writer::{Objects, Writer};
    use object_store::memory::InMemory;
    use std::sync::Arc;

    /// A segment's object holds the segment its key names: of its branch, of its number, by the
    /// writer of its epoch.
    #[test]
    fn segments_are_where_their_keys_say() {
        let footer =
            Footer { branch: test_branch(1), seq: 2, epoch: 1, contents: Contents::default() };
        assert_eq!(placed("here", footer.clone(), test_branch(1), 2, 1), Ok(footer.clone()));
        for (branch, seq, epoch) in [(1, 3, 1), (4, 2, 1), (1, 2, 2)] {
            let corrupt = Error::Corrupt("here holds segment 2 of 1/1/1 by epoch 1".into());
            let found = placed("here", footer.clone(), test_branch(branch), seq, epoch);
            assert_eq!(found, Err(corrupt));
        }
    }

    /// Commits `count` commits, tagged on from `tag`, each in a segment of its own, uploaded and
    /// acknowledged, its requests served by `bucket`.
    fn committed(bucket: &SimBucket, writer: &Writer<SimBucket>, tag: u64, count: u64) {
        for tag in tag..tag + count {
            let objects =
                Batch::from(Commit { tag, ..Commit::default() }).close(&mut writer.chain()).frame();
            let seq = writer.add_segment(objects).unwrap();
            bucket.served(writer.upload(seq)).0.unwrap();
            bucket.served(writer.acknowledge()).0.unwrap();
        }
    }

    /// TASK-92.6, TASK-407: recovery reads its branch's head, then the footers of the segments it
    /// names by their objects' tails, all at once, every key known. Recovered on, it reads only
    /// the segments the head names since, finding what recovering anew finds.
    #[test]
    fn recoveries_read_the_segments_their_heads_name_at_once() {
        let bucket = SimBucket::new();
        let (writer, _) = bucket.served(async {
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap()
        });
        committed(&bucket, &writer, 1, 5);
        let tails = |seqs: std::ops::Range<u64>| -> Vec<String> {
            seqs.map(|seq| {
                format!("get {} last {}", layout::segment(test_branch(1), seq, 1), object::TAIL)
            })
            .collect()
        };
        let head = vec![format!("get {}", layout::head(test_branch(1)))];
        let (recovered, rounds) = bucket.served_in_rounds(recover(&*bucket, test_branch(1), None));
        let recovered = recovered.unwrap();
        assert_eq!((recovered.end(), recovered.state.last_commit), (5, Some(5)));
        assert_eq!(rounds, [head.clone(), tails(0..5)]);
        let keys: Vec<_> = (0..5).map(|seq| layout::segment(test_branch(1), seq, 1)).collect();
        assert_eq!(recovered.touched.iter().cloned().collect::<Vec<_>>(), keys);

        committed(&bucket, &writer, 6, 2);
        let now = bucket.served(head::get(&*bucket, test_branch(1))).0.unwrap();
        let (on, rounds) =
            bucket.served_in_rounds(recover_on(&*bucket, None, recovered, now, &Ring::clear()));
        let on = on.unwrap();
        assert_eq!(rounds, [tails(5..7)]);
        let anew = bucket.served(recover(&*bucket, test_branch(1), None)).0.unwrap();
        assert_eq!((on.end(), on.state, on.touched), (anew.end(), anew.state, anew.touched));
    }

    /// TASK-92.6: a recovery recovered on once its head names a checkpoint as of where it would
    /// follow on from, or later, recovers anew, from it: the segments it covers may be gone.
    #[test]
    fn recoveries_recovered_on_past_a_newer_checkpoint_recover_anew() {
        let bucket = SimBucket::new();
        let (writer, _) = bucket.served(async {
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap()
        });
        committed(&bucket, &writer, 1, 2);
        let recovered = bucket.served(recover(&*bucket, test_branch(1), None)).0.unwrap();
        committed(&bucket, &writer, 3, 1);
        let seq = bucket.served(writer.checkpoint()).0.unwrap().unwrap();
        committed(&bucket, &writer, 4, 1);
        let now = bucket.served(head::get(&*bucket, test_branch(1))).0.unwrap();
        let (on, rounds) =
            bucket.served_in_rounds(recover_on(&*bucket, None, recovered, now, &Ring::clear()));
        let on = on.unwrap();
        assert_eq!((on.checkpoint(), on.end(), on.state.last_commit), (Some((seq, 1)), 4, Some(4)));
        let checkpoint = vec![format!("get {}", layout::checkpoint(test_branch(1), seq, 1))];
        let tail =
            |seq| format!("get {} last {}", layout::segment(test_branch(1), seq, 1), object::TAIL);
        assert_eq!(rounds, [checkpoint, vec![tail(3)]]);
    }

    /// cladefs: a commit spanning segments (TASK-92.41): the runs each segment going on with it names
    /// are sealed, where they are, once the segment ending it is applied, and not before; its
    /// writer acknowledges none of its segments until then, and no head names them. A writer taking
    /// the branch with a commit begun, its segments landed, finds none of it: the head names whole
    /// commits alone, the next writer's state and recovery's agreeing.
    #[test]
    fn commits_spanning_segments_seal_once_ended() {
        futures::executor::block_on(async {
            let bucket = Arc::new(Store::new(Arc::new(InMemory::new()), ""));
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let extent = |sequence| ExtentId::new(1, sequence);
            let tag = |epoch| layout::tag(writer.epoch(), epoch);
            let landed = async |writer: &Writer<Store>, contents: Contents| {
                let footer = writer.chain().close(contents);
                let data = vec![7; 8192];
                let segment = Segment { data, footer: footer.clone() }.encode().into();
                let seq = writer.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
                writer.upload(seq).await.unwrap();
                (seq, writer.acknowledge().await.unwrap())
            };
            let continuing = |commit, sequence| Contents {
                continued: Some(Continued {
                    commit,
                    extents: vec![(extent(sequence), 4096, 4096)],
                }),
                ..Contents::default()
            };
            let (first, acked) = landed(&writer, continuing(tag(1), 1)).await;
            assert_eq!(acked, None, "a segment going on with a commit, acknowledged");
            let (second, acked) = landed(&writer, continuing(tag(1), 2)).await;
            assert_eq!(acked, None, "a segment going on with a commit, acknowledged");
            let begun = recover(&*bucket, test_branch(1), None).await.unwrap().state;
            assert_eq!((begun.extents.len(), begun.last_commit), (0, None), "{begun:?}");
            let ending = Contents {
                commits: vec![tag(1)],
                extents: vec![(extent(3), 0, 4096)],
                next_sequence: 4,
                ..Contents::default()
            };
            let (ends, acked) = landed(&writer, ending).await;
            assert_eq!(acked, Some(ends));
            let at = |seq, offset| Location {
                key: layout::segment(test_branch(1), seq, 1),
                offset,
                len: 4096,
            };
            let sealed: Vec<_> = writer.state().extents.into_iter().collect();
            let expected = vec![
                (extent(1), at(first, 4096)),
                (extent(2), at(second, 4096)),
                (extent(3), at(ends, 0)),
            ];
            assert_eq!(sealed, expected);
            assert_eq!(writer.state().begun, None);
            let recovered = recover(&*bucket, test_branch(1), None).await.unwrap().state;
            assert_eq!(recovered.extents.into_iter().collect::<Vec<_>>(), expected);
            assert_eq!(recovered.last_commit, Some(tag(1)));

            // Begun, its writer gone: the next finds none of it.
            let (begun, _) = landed(&writer, continuing(tag(2), 4)).await;
            let next = Writer::open(bucket.clone(), test_branch(1), 11, 0).await.unwrap();
            assert_eq!(next.state().through, Some(ends), "the head's end, before it");
            assert_eq!(next.taken().chain.next_seq(), begun, "where its own go");
            assert_eq!(next.state().extents.into_iter().collect::<Vec<_>>(), expected);
            assert_eq!((next.state().begun, next.state().last_commit), (None, Some(tag(1))));
            let recovered = recover(&*bucket, test_branch(1), None).await.unwrap().state;
            assert_eq!(recovered, next.state());
        });
    }

    /// TASK-165.3: a branch's capacity is its state, set by its commits, recovered from its
    /// segments and its checkpoints wherever it is taken; a fork's child has its parent's at the
    /// fork, until it sets its own, and a point keeps it as it was, whatever the parent sets after.
    #[test]
    fn capacities_are_set_by_commits() {
        futures::executor::block_on(async {
            let bucket = Arc::new(Store::new(Arc::new(InMemory::new()), ""));
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            assert_eq!(parent.state().capacity, None);
            let resize = |tag, capacity: u64| {
                Batch::from(Commit { tag, capacity: Some(capacity), ..Commit::default() })
            };
            let landed = async |writer: &Writer<Store>, objects| {
                let seq = writer.add_segment(objects).unwrap();
                writer.upload(seq).await.unwrap();
                writer.acknowledge().await.unwrap();
                seq
            };
            let forked =
                resize(1, 1 << 20).fork(child(test_branch(2)), &mut parent.chain()).frame();
            let forked = Origin { parent: test_branch(1), at: landed(&parent, forked).await };
            let point = Child { branch: test_branch(3), owner: 0, point: true };
            let pointed = Batch::default().fork(point, &mut parent.chain()).frame();
            let pointed = Origin { parent: test_branch(1), at: landed(&parent, pointed).await };
            landed(&parent, resize(2, 2 << 20).close(&mut parent.chain()).frame()).await;
            let capacity = async |branch, origin| {
                let recovered = recover(&*bucket, branch, origin).await.unwrap();
                (recovered.checkpoint().is_some(), recovered.state.capacity)
            };
            assert_eq!(parent.state().capacity, Some(2 << 20));
            assert_eq!(capacity(test_branch(1), None).await, (false, Some(2 << 20)));
            assert_eq!(capacity(test_branch(2), Some(forked)).await, (false, Some(1 << 20)));
            assert_eq!(capacity(test_branch(3), Some(pointed)).await, (false, Some(1 << 20)));
            parent.checkpoint().await.unwrap();
            assert_eq!(capacity(test_branch(1), None).await, (true, Some(2 << 20)));

            let child = Writer::open(bucket.clone(), test_branch(2), 11, 0).await.unwrap();
            assert_eq!(child.state().capacity, Some(1 << 20));
            landed(&child, resize(3, 4 << 20).close(&mut child.chain()).frame()).await;
            child.checkpoint().await.unwrap();
            assert_eq!(capacity(test_branch(2), Some(forked)).await, (true, Some(4 << 20)));
            assert_eq!(capacity(test_branch(1), None).await, (true, Some(2 << 20)));
            assert_eq!(capacity(test_branch(3), Some(pointed)).await, (false, Some(1 << 20)));
        });
    }

    /// A fork's checkpoints hold its own state, from which it recovers; another branch's
    /// checkpoint in its place is corrupt.
    #[test]
    fn forks_checkpoint_their_own_states() {
        futures::executor::block_on(async {
            let memory = Arc::new(InMemory::new());
            let bucket = Arc::new(Store::new(memory.clone(), ""));
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let extent = ExtentId::new(1, parent.chain().next_sequence());
            let extents = vec![(extent, b"one".to_vec())];
            let batch = Batch::from(Commit { tag: 1, extents, ..Commit::default() });
            let at = parent
                .add_segment(batch.fork(child(test_branch(2)), &mut parent.chain()).frame())
                .unwrap();
            parent.upload(at).await.unwrap();
            parent.acknowledge().await.unwrap();
            let origin = Some(Origin { parent: test_branch(1), at });
            assert_eq!(
                branch::record(&*bucket, test_branch(2)).await.unwrap().unwrap().origin(),
                origin
            );

            // The fork's writer, a commit of its own, then its checkpoint: none before, of no
            // segment of its own.
            let child = Writer::open(bucket.clone(), test_branch(2), 11, 0).await.unwrap();
            assert_eq!(child.checkpoint().await, Ok(None));
            let commit = Batch::from(Commit { tag: 2, ..Commit::default() });
            let seq = child.add_segment(commit.close(&mut child.chain()).frame()).unwrap();
            child.upload(seq).await.unwrap();
            child.acknowledge().await.unwrap();
            assert_eq!(child.checkpoint().await, Ok(Some(seq)));
            let recovered = recover(&*bucket, test_branch(2), origin).await.unwrap();
            assert_eq!(recovered.checkpoint(), Some((seq, child.epoch())));
            assert_eq!(recovered.state.branch, test_branch(2));
            assert_eq!(recovered.state.extents.keys().collect::<Vec<_>>(), [&extent]);

            // The parent's checkpoint, at the key the fork's head names, as a bucket corrupting it.
            let parents = parent.checkpoint().await.unwrap().unwrap();
            let object = bucket::get(&*bucket, &layout::checkpoint(test_branch(1), parents, 1))
                .await
                .unwrap();
            let key = layout::checkpoint(test_branch(2), seq, child.epoch());
            let overwrite = crate::bucket::PutMode::Overwrite;
            crate::bucket::Bucket::put(&*bucket, &key, object.bytes.into(), overwrite)
                .await
                .unwrap();
            // Read by another host, which decoded nothing of it before: a key names one object,
            // which a host's bucket keeps decoded (TASK-407).
            let corrupt = format!("{key} holds a checkpoint of 1/1/1");
            let other = Store::new(memory, "");
            assert_eq!(
                recover(&other, test_branch(2), origin).await.unwrap_err(),
                Error::Corrupt(corrupt)
            );
        });
    }
}
