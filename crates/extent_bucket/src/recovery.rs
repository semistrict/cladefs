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
//! checkpoints are checked against their MACs under the keys it holds, and one whose encrypt says the
//! clear is corrupt (TASK-411.28). The collector's holds none, reading them as it did.

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
use std::sync::{Arc, Mutex, Weak};
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
        self.through = Some(footer.segment);
        self.apply_contents(footer.segment, footer.epoch, &footer.contents);
    }

    /// Applies `contents`, those of the branch's segment `segment`, of the writer of epoch
    /// `epoch`: what its commits sealed, where they sealed it, then what they freed; the root they
    /// set, their last commit, the branch's next sequence number, the largest named, the forks at
    /// its end, and the capacity they set. Every free of the branch's own extents spends it, as
    /// the store that freed it spent it: a footer frees only what a commit held, not what was
    /// freed while open (TASK-160.15), and one sealed and freed within the segment is a free and no
    /// seal (TASK-165.3). cladefs: first, the runs of a commit the segments before began ([`Begun`]),
    /// if this one ends it, or holding them on if it goes on with it; then, after its own, holding
    /// those of the commit it goes on with, if any (TASK-92.41).
    pub fn apply_contents(&mut self, segment: u64, epoch: u64, contents: &Contents) {
        let key = layout::segment(self.branch, segment, epoch);
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
        for (extent, location) in contents.own_at(self.branch, epoch, segment) {
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
        self.forks.extend(contents.forks.iter().map(|child| Fork::at(segment, child)));
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
    /// cladefs: the decoded state it started from, if it started from one: its newest checkpoint's,
    /// or the state its origin had where it was forked, which its extents' map shares, a
    /// persistent one, with every other recovery that started from it (TASK-453). Whoever keeps
    /// what it recovered holds it, so that what reads through the bucket after finds it decoded
    /// while it does ([`Decoded`]), and shares it too.
    pub layer: Option<Arc<State>>,
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
        let Self { state, head, touched, layer } = self;
        let state = state.placed(Layered::flatten);
        Recovered { state, head, touched, layer }
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
static KEYLESS: std::sync::LazyLock<Arc<Ring>> = std::sync::LazyLock::new(Ring::keyless);

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
    recover_under(bucket, branch, origin, &Ring::keyless()).await
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

/// cladefs: the state of `branch` as [`recover_under`] finds it, starting from `head`, what a read of
/// its head found, rather than a read of its own (TASK-449): as an opening store reads its head at
/// once with its record, and a point's copy knows it has none. The head read before is as good as
/// one read now: it names what the branch held as of that read, the recovery is of that, and
/// whatever takes the branch after goes on from what is there then (`Writer::take`). Something it
/// names gone, it starts again from a read of the head, as [`recover_under`] does.
pub async fn recover_under_from(
    bucket: &impl Bucket,
    branch: Branch,
    origin: Option<Origin>,
    head: Option<Head>,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let checkpoints = Checkpoints::new(bucket, 0).holding(ring);
    Ok(recover_layers_from(&checkpoints, branch, origin, Some(head)).await?.flatten())
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
    recover_layers_from(checkpoints, branch, origin, None).await
}

/// [`recover_layers`], its first attempt from `first`, a head read before, if given
/// ([`recover_under_from`]).
async fn recover_layers_from<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    branch: Branch,
    origin: Option<Origin>,
    mut first: Option<Option<Head>>,
) -> Result<Recovered<Layers>, Error> {
    for _ in 0..RESTARTS {
        let head = match first.take() {
            Some(head) => head,
            None => head::get(checkpoints.bucket, branch).await?,
        };
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
        let keys = &**KEYLESS;
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
                        let state = checkpoint(&key, &object.bytes, branch, keys)?;
                        Ok(Some(match decoded {
                            Some(decoded) => decoded.keep(&key, state),
                            None => Arc::new(state),
                        }))
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
///
/// cladefs: and every state decoded that something still holds, as an open store holds the one it
/// started from ([`Recovered::layer`]), however many were decoded since (TASK-453): the forks of a
/// base opened after it find it, and share it, while one is open. And a state decoded anew shares
/// what it holds alike with the state held still that has the most of its extents' branches' in
/// common with it, its map made of that one's, changed where it differs ([`rebased`]): the
/// checkpoints a live store's writer writes for each of its forks hold the same extents, and a
/// fork's own checkpoint, as it opens again, its base's but its own, each decoded apart a copy of
/// them all, 230 bytes an extent a fork (TASK-443.2). What they hold, each map once, and each made
/// of another held still by what it changed ([`Changes`]), it says ([`Decoded::held`]), which the
/// extent daemon counts.
#[derive(Default)]
pub struct Decoded {
    kept: Mutex<States>,
}

/// The states [`Decoded`] keeps, by their checkpoints' keys, and those, oldest first; and every one
/// it decoded others may hold still.
#[derive(Default)]
struct States {
    states: HashMap<String, Arc<State>>,
    order: VecDeque<String>,
    live: HashMap<String, Live>,
}

/// A state decoded, which others may hold still ([`Decoded`]): how many of its extents each branch
/// has, by its ID, which its likeness to another's is reckoned by; and, if it was made of another
/// ([`rebased`]), that one and what it changed of it.
struct Live {
    state: Weak<State>,
    branches: BTreeMap<u64, usize>,
    made_of: Option<(Weak<State>, Changes)>,
}

/// What the states a [`Decoded`] keeps, or others hold still, hold ([`Decoded::held`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    /// How many states, each map counted once.
    pub states: usize,
    /// The entries of their extents' maps, each held once however many share it: of a state made
    /// of another held still, those it changed; of the others, all of them.
    pub entries: usize,
    /// The runs the entries changed of maps made of others make ([`Changes`]).
    pub runs: usize,
}

impl Held {
    /// The most bytes of the heap they hold: [`ENTRY_BYTES`] an entry, [`RUN_BYTES`] a run.
    pub fn bytes(&self) -> u64 {
        self.entries as u64 * ENTRY_BYTES + self.runs as u64 * RUN_BYTES
    }
}

impl std::ops::Add for Held {
    type Output = Held;

    fn add(self, other: Held) -> Held {
        Held {
            states: self.states + other.states,
            entries: self.entries + other.entries,
            runs: self.runs + other.runs,
        }
    }
}

/// The most bytes of the process's heap an entry of a decoded state's map of its extents holds, its
/// share of the persistent map's nodes and its location's, or of the nodes a change of a map made
/// of another's copies: what [`Held::bytes`] counts each as. A bound, measured by cladefs's
/// `bench_fork_extent_memory` (TASK-453), which fails past it.
pub const ENTRY_BYTES: u64 = 512;

/// The most bytes of the process's heap a run of changes to a map of extents shared with another
/// copies, past [`ENTRY_BYTES`] for each entry changed: a persistent map's change copies the path
/// to the leaf it is in, the leaf (16 entries, their locations' keys copied too) and the branches
/// over it, unless a change before copied them, and, a leaf left too small, its sibling. Changes
/// side by side share one path; scattered, each copies its own: measured 1.9-2.1 KB a change,
/// every 32nd entry of 32,768 changed (`bench_fork_extent_memory`, TASK-453), and bounded at every
/// spacing and size by `tests::changes_bound_what_maps_made_of_others_copy`. What
/// [`Changes::bytes`], and extent_store's count of what its stores' writers change of the maps
/// they share, count each run as.
pub const RUN_BYTES: u64 = 4096;

/// What a map of extents made of another changed of it ([`rebased`]): the entries, removed, added
/// or placed anew, and the runs they make, in order, no entry of the other's it kept between those
/// of a run, each copying a path of the persistent map's nodes ([`RUN_BYTES`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub entries: usize,
    pub runs: usize,
}

impl Changes {
    /// All of a map of `len` entries: made of none, or sharing nothing.
    pub fn all(len: usize) -> Self {
        Self { entries: len, runs: 0 }
    }

    /// The most bytes of the heap they hold: [`ENTRY_BYTES`] an entry, [`RUN_BYTES`] a run.
    pub fn bytes(&self) -> u64 {
        self.entries as u64 * ENTRY_BYTES + self.runs as u64 * RUN_BYTES
    }

    /// What a map of `len` entries made of another with these changes holds of its own: these,
    /// or all of it, if less: its changes copy each of its nodes once at most, the map made then
    /// one sharing nothing.
    pub fn within(self, len: usize) -> Self {
        let all = Self::all(len);
        if self.bytes() > all.bytes() { all } else { self }
    }
}

impl Decoded {
    /// How many states it keeps: a host's forks open from a few bases' newest checkpoints at once.
    const KEEP: usize = 16;

    /// The state the checkpoint at `key` holds, if it kept it, or another holds it still.
    fn get(&self, key: &str) -> Option<Arc<State>> {
        let kept = self.kept.lock().unwrap();
        let held = || kept.live.get(key).and_then(|live| live.state.upgrade());
        kept.states.get(key).cloned().or_else(held)
    }

    /// Keeps `state`, which the checkpoint at `key` holds, just decoded: its extents made of the
    /// most alike of the states others hold still, if one is alike ([`rebased`]). Returns it, or the
    /// one at `key` another decoded meanwhile, if one did. Made holding no lock: one walk of both.
    fn keep(&self, key: &str, mut state: State) -> Arc<State> {
        let branches = branches_of(&state.extents);
        let alike = {
            let kept = self.kept.lock().unwrap();
            let likeness = |live: &Live| -> usize {
                let common = live.branches.iter().filter_map(|(branch, held)| {
                    branches.get(branch).map(|holds| (*holds).min(*held))
                });
                common.sum()
            };
            (kept.live.values())
                .filter_map(|live| Some((likeness(live), live.state.upgrade()?)))
                .filter(|(likeness, _)| *likeness > 0)
                .max_by_key(|(likeness, held)| (*likeness, held.through))
                .map(|(_, held)| held)
        };
        let mut made_of = None;
        if let Some(alike) = alike
            && let Some((extents, changes)) = rebased(&state.extents, &alike.extents)
        {
            state.extents = extents;
            made_of = Some((Arc::downgrade(&alike), changes));
        }
        let mut kept = self.kept.lock().unwrap();
        if let Some(held) = kept.live.get(key).and_then(|live| live.state.upgrade()) {
            return held;
        }
        let state = Arc::new(state);
        kept.live.retain(|_, live| live.state.strong_count() > 0);
        let live = Live { state: Arc::downgrade(&state), branches, made_of };
        kept.live.insert(key.to_owned(), live);
        if kept.states.insert(key.to_owned(), state.clone()).is_none() {
            kept.order.push_back(key.to_owned());
        }
        while kept.order.len() > Self::KEEP {
            let oldest = kept.order.pop_front().unwrap();
            kept.states.remove(&oldest);
        }
        state
    }

    /// What the states it keeps, and those others hold still, hold ([`Held`]): each map once,
    /// however many share it; of a state made of another held still, what it changed of it
    /// ([`Changes::within`] the map), counted with that one; of the others, made of none or of one
    /// no one holds any more, which they hold the rest of, all of it. And it lets go of what it
    /// knew of those no one holds, as each decoding does: their keys and weak references.
    pub fn held(&self) -> Held {
        let mut kept = self.kept.lock().unwrap();
        kept.live.retain(|_, live| live.state.strong_count() > 0);
        // Each map, with the most of what its states hold of their own: one made of none, or of
        // one held no more, all of it.
        let mut maps: Vec<(Arc<State>, Changes)> = Vec::new();
        for live in kept.live.values() {
            let Some(state) = live.state.upgrade() else { continue };
            let len = state.extents.len();
            let made_of = live.made_of.as_ref().filter(|(made_of, _)| made_of.strong_count() > 0);
            let own = made_of.map_or(Changes::all(len), |(_, changes)| changes.within(len));
            match maps.iter_mut().find(|(other, _)| other.extents.ptr_eq(&state.extents)) {
                Some((_, most)) if own.bytes() > most.bytes() => *most = own,
                Some(_) => {}
                None => maps.push((state, own)),
            }
        }
        let (entries, runs) = maps
            .iter()
            .map(|(_, own)| (own.entries, own.runs))
            .fold((0, 0), |(entries, runs), (more, others)| (entries + more, runs + others));
        Held { states: maps.len(), entries, runs }
    }
}

/// How many of `extents` each branch has, by its ID.
fn branches_of(extents: &Extents) -> BTreeMap<u64, usize> {
    let mut branches = BTreeMap::new();
    for extent in extents.keys() {
        *branches.entry(extent.branch).or_default() += 1;
    }
    branches
}

/// cladefs: `fresh`, a state's extents just decoded, made of `alike`'s, another's of its branch, as
/// persistent maps make one of another: `alike`'s changed where `fresh` differs from it, sharing
/// with it the rest, with what it changed ([`Changes`], TASK-453). None if it changes as many as
/// half of them, which share little. One walk of both in order, their keys the extents'.
fn rebased(fresh: &Extents, alike: &Extents) -> Option<(Extents, Changes)> {
    use std::cmp::Ordering;
    let most = fresh.len() / 2;
    let mut made = alike.clone();
    let mut changes = Changes::default();
    // Whether the last entry walked was a change: one after it is of its run.
    let mut changing = false;
    let (mut old, mut new) = (alike.iter().peekable(), fresh.iter().peekable());
    loop {
        let order = match (old.peek(), new.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((was, _)), Some((is, _))) => was.cmp(is),
        };
        match order {
            Ordering::Less => {
                let (extent, _) = old.next().unwrap();
                made.remove(extent);
            }
            Ordering::Greater => {
                let (extent, location) = new.next().unwrap();
                made.insert(*extent, location.clone());
            }
            Ordering::Equal => {
                let ((_, was), (extent, is)) = (old.next().unwrap(), new.next().unwrap());
                if was == is {
                    changing = false;
                    continue;
                }
                made.insert(*extent, is.clone());
            }
        }
        changes.runs += usize::from(!changing);
        changing = true;
        changes.entries += 1;
        if changes.entries > most {
            return None;
        }
    }
    Some((made, changes))
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
    let Recovered { state, head: was, touched, layer } = before;
    let branch = state.branch;
    let from = was.as_ref().map_or(0, |was| was.end);
    // A checkpoint since, covering what it would follow on from: the collector, going by it, may
    // have deleted those segments, and it recovers anew.
    if head.as_ref().and_then(|head| head.checkpoint).is_some_and(|(segment, _)| segment >= from) {
        return recover_from(bucket, branch, origin, head, ring).await;
    }
    let mut recovered = Recovered { state: Layered::of(state), head: head.clone(), touched, layer };
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
    let mut layer = None;
    let state = match checkpoint {
        Some(key) => {
            let Some(state) = checkpoints.read_key(&key, branch).await? else {
                return Err(moved(bucket, branch, head.as_ref()).await);
            };
            touched.insert(key.clone());
            if state.through.is_none() {
                return Err(Attempt::Failed(Error::Corrupt(key)));
            }
            let of = Layered::of(State::clone(&state));
            layer = Some(state);
            of
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
                    Ok(at) => {
                        layer = Some(at.clone());
                        Layered::inheriting(at, branch)
                    }
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
    let from = state.through.map_or(0, |segment| segment + 1);
    let mut recovered = Recovered { state, head, touched, layer };
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
    let Recovered { state, head, touched, .. } = recovered;
    let Some(named) = head.as_ref() else { return Ok(()) };
    let branch = state.branch;
    let mut footers = futures::stream::iter(named.segments(from))
        .map(|(segment, epoch)| async move {
            let key = layout::segment(branch, segment, epoch);
            let footer = segment_footer((bucket, keys), branch, segment, epoch).await?;
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
/// segment whole. cladefs: checked against its MAC if `keys` hold its key (TASK-411.3); in the
/// clear, corrupt (TASK-411.28).
async fn segment_footer(
    (bucket, keys): (&impl Bucket, &Ring),
    branch: Branch,
    segment: u64,
    epoch: u64,
) -> Result<Option<Footer>, Error> {
    let key = layout::segment(branch, segment, epoch);
    let Some(tail) = object::tail(bucket, &key).await? else { return Ok(None) };
    if !keys.is_empty() {
        tail.open(&keys.of(branch.id))?;
    }
    Ok(Some(placed(&key, layout::decode(tail.footer())?, branch, segment, epoch)?))
}

/// `footer`, that of the object at `key`, which holds segment `segment` of `branch` by the writer
/// of `epoch`, if it says so.
fn placed(
    key: &str,
    footer: Footer,
    branch: Branch,
    segment: u64,
    epoch: u64,
) -> Result<Footer, Error> {
    if footer.branch != branch || footer.segment != segment || footer.epoch != epoch {
        return Err(Error::Corrupt(format!(
            "{key} holds segment {:x} of {} by epoch {:x}",
            footer.segment, footer.branch, footer.epoch
        )));
    }
    Ok(footer)
}

/// The state checkpoint `key` of `branch`, whose object holds `bytes`, holds: cladefs: read holding
/// `keys` (TASK-411.3).
fn checkpoint(key: &str, bytes: &[u8], branch: Branch, keys: &Ring) -> Result<State, Error> {
    let state = crate::checkpoint::decode_under(bytes, &keys.of(branch.id))?;
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
            layout::Kind::Checkpoint { segment, epoch, .. } if segment <= at => {
                Some(((segment, epoch), meta))
            }
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
        let by_key = branch.cell != from || kept;
        // cladefs: read by the key of the epoch a head was seen naming the segment by, if one was:
        // the head's to read only if the checkpoint isn't there (TASK-449, `known`).
        let known = bucket.known().filter(|_| by_key).and_then(|known| known.epoch(branch, at));
        let mut head = None;
        let epoch = match known {
            Some(epoch) => epoch,
            None => {
                head = Some(head::get(bucket, branch).await?);
                named_epoch(bucket, branch, head.as_ref().and_then(Option::as_ref), at)?
            }
        };
        // cladefs: a fork point's a pass keeps for the forks to other cells is read as they read
        // it, by the key of its checkpoint alone, if there is one: a later writer's checkpoint at
        // the segment, at its own key, may name the extents along other moves, of other passes,
        // than the forks follow from it (TASK-407, TASK-407.1). None yet, no fork read it: the
        // newest checkpoint up to there, as whoever writes it reads it.
        if by_key {
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
        let head = match head {
            Some(head) => head,
            None => head::get(bucket, branch).await?,
        };
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
        let from = state.through.map_or(0, |segment| segment + 1);
        let named = head.as_ref().expect("a head naming the segment at");
        let mut footers =
            futures::stream::iter(named.segments(from).take_while(|&(segment, _)| segment <= at))
                .map(|(segment, epoch)| async move {
                    let key = layout::segment(branch, segment, epoch);
                    Ok::<_, Error>((
                        key,
                        segment_footer((bucket, checkpoints.keys), branch, segment, epoch).await?,
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

/// cladefs: the epoch of the writer of segment `at` of `branch`, as `head`, its head, names it:
/// noted as known for good (`known`, TASK-449), a head's runs of epochs only growing; or
/// [`Error::Lost`] if it names none.
fn named_epoch<B: Bucket>(
    bucket: &B,
    branch: Branch,
    head: Option<&Head>,
    at: u64,
) -> Result<u64, Attempt> {
    let Some(epoch) = head.and_then(|head| head.epoch_of(at)) else {
        return Err(Attempt::Failed(Error::Lost(format!("segment {at:x} of {branch}"))));
    };
    if let Some(known) = bucket.known() {
        known.named(branch, at, epoch);
    }
    Ok(epoch)
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
/// cladefs: authenticated under the DEK `keys` give, `branch`'s, if its objects are encrypted, as
/// its writers' checkpoints are: a fork's child reading it holding `branch`'s keys refuses one in
/// the clear (TASK-411.4). Asked for only if it is to be written.
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
    debug_assert_eq!(keys.encrypting().1.branch, branch.id);
    let checkpoints = Checkpoints::new(bucket, 0);
    for _ in 0..RESTARTS {
        match state_at(&checkpoints, branch, at, &mut BTreeSet::new(), branch.cell, false).await {
            Ok(state) => {
                let checkpoint = crate::checkpoint::encode(&state, keys.encrypting());
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
    let Some(footer) = segment_footer((bucket, &**KEYLESS), parent, at, epoch).await? else {
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
    use crate::batch::{Commit, SegmentBuilder};
    use crate::branch;
    use crate::layout::test_branch;
    use crate::layout::{Child, Continued, Segment, child};
    use crate::object_bucket::ObjectBucket;
    use crate::testing::SimBucket;
    use crate::writer::{Objects, Writer};
    use object_store::memory::InMemory;
    use std::sync::Arc;

    /// TASK-453: a map of extents made of another, `rebased`, holds no more of the heap past the
    /// other's than its changes count (`Changes::within` it): of maps of 1,024, 32,768 and 300,000
    /// entries, each its location's key a string of its own, as a checkpoint decodes them, changed
    /// every 3rd to every 16,384th entry, by turns removed, placed anew and joined by a new one
    /// after it; in one block of an eighth of them; and grown by as many new at its end.
    #[test]
    fn changes_bound_what_maps_made_of_others_copy() {
        let branch = test_branch(1);
        // The base's extents, every other sequence number, so that new ones go between; each its
        // segment's key, 64 extents a segment, a string of its own.
        let extent = |sequence: u64| ExtentId::new(branch.id, sequence);
        let at = |sequence: u64, epoch: u64| Location {
            key: crate::layout::segment(branch, sequence / 128 + 1, epoch),
            offset: sequence % 128 * 4096,
            len: 4096,
        };
        for len in [1_024u64, 32_768, 300_000] {
            let alike: Extents = (0..len).map(|n| (extent(2 * n), at(2 * n, 1))).collect();
            let scattered = [3u64, 4, 8, 32, 1024, 16_384].map(|every| {
                let mut fresh = alike.clone();
                for (turn, n) in (0..len).step_by(every as usize).enumerate() {
                    match turn % 3 {
                        0 => drop(fresh.remove(&extent(2 * n))),
                        1 => drop(fresh.insert(extent(2 * n), at(2 * n, 2))),
                        _ => drop(fresh.insert(extent(2 * n + 1), at(2 * n + 1, 2))),
                    }
                }
                (format!("one in {every}"), fresh)
            });
            let mut block = alike.clone();
            for n in len / 2..len / 2 + len / 8 {
                block.insert(extent(2 * n), at(2 * n, 2));
            }
            let mut grown = alike.clone();
            for n in len..len + len / 8 {
                grown.insert(extent(2 * n), at(2 * n, 1));
            }
            let cases = scattered.into_iter().chain([
                ("a block of an eighth".to_owned(), block),
                ("an eighth more at its end".to_owned(), grown),
            ]);
            for (case, fresh) in cases {
                // Decoded apart, as a checkpoint is: sharing nothing with `alike`.
                let fresh: Extents =
                    fresh.iter().map(|(extent, at)| (*extent, at.clone())).collect();
                let before = crate::heap::held();
                let (made, changes) = rebased(&fresh, &alike).expect(&case);
                let held = (crate::heap::held() - before) as u64;
                assert_eq!(made, fresh, "{len} entries, {case}: the map made");
                let counted = changes.within(made.len());
                println!(
                    "{len} entries, {case}: {changes:?}, {held} bytes held, {} a change, counted \
                     {counted:?}, {} bytes",
                    held / changes.entries as u64,
                    counted.bytes()
                );
                assert!(held <= counted.bytes(), "{len} entries, {case}: {held} bytes held");
            }
        }
    }

    /// A segment's object holds the segment its key names: of its branch, of its number, by the
    /// writer of its epoch.
    #[test]
    fn segments_are_where_their_keys_say() {
        let footer =
            Footer { branch: test_branch(1), segment: 2, epoch: 1, contents: Contents::default() };
        assert_eq!(placed("here", footer.clone(), test_branch(1), 2, 1), Ok(footer.clone()));
        for (branch, segment, epoch) in [(1, 3, 1), (4, 2, 1), (1, 2, 2)] {
            let corrupt = Error::Corrupt("here holds segment 2 of 1/1/1 by epoch 1".into());
            let found = placed("here", footer.clone(), test_branch(branch), segment, epoch);
            assert_eq!(found, Err(corrupt));
        }
    }

    /// Commits `count` commits, tagged on from `tag`, each in a segment of its own, uploaded and
    /// acknowledged, its requests served by `bucket`.
    fn committed(bucket: &SimBucket, writer: &Writer<SimBucket>, tag: u64, count: u64) {
        for tag in tag..tag + count {
            let objects = SegmentBuilder::from(Commit { tag, ..Commit::default() })
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting());
            let segment = writer.add_segment(objects).unwrap();
            bucket.served(writer.upload(segment)).0.unwrap();
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
        let tails = |segments: std::ops::Range<u64>| -> Vec<String> {
            segments
                .map(|segment| {
                    format!(
                        "get {} last {}",
                        layout::segment(test_branch(1), segment, 1),
                        object::TAIL
                    )
                })
                .collect()
        };
        let head = vec![format!("get {}", layout::head(test_branch(1)))];
        let (recovered, rounds) = bucket.served_in_rounds(recover(&*bucket, test_branch(1), None));
        let recovered = recovered.unwrap();
        assert_eq!((recovered.end(), recovered.state.last_commit), (5, Some(5)));
        assert_eq!(rounds, [head.clone(), tails(0..5)]);
        let keys: Vec<_> =
            (0..5).map(|segment| layout::segment(test_branch(1), segment, 1)).collect();
        assert_eq!(recovered.touched.iter().cloned().collect::<Vec<_>>(), keys);

        committed(&bucket, &writer, 6, 2);
        let now = bucket.served(head::get(&*bucket, test_branch(1))).0.unwrap();
        let (on, rounds) =
            bucket.served_in_rounds(recover_on(&*bucket, None, recovered, now, &Ring::keyless()));
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
        let segment = bucket.served(writer.checkpoint()).0.unwrap().unwrap();
        committed(&bucket, &writer, 4, 1);
        let now = bucket.served(head::get(&*bucket, test_branch(1))).0.unwrap();
        let (on, rounds) =
            bucket.served_in_rounds(recover_on(&*bucket, None, recovered, now, &Ring::keyless()));
        let on = on.unwrap();
        assert_eq!(
            (on.checkpoint(), on.end(), on.state.last_commit),
            (Some((segment, 1)), 4, Some(4))
        );
        let checkpoint = vec![format!("get {}", layout::checkpoint(test_branch(1), segment, 1))];
        let tail = |segment| {
            format!("get {} last {}", layout::segment(test_branch(1), segment, 1), object::TAIL)
        };
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
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let extent = |sequence| ExtentId::new(1, sequence);
            let tag = |epoch| layout::tag(writer.epoch(), epoch);
            let landed = async |writer: &Writer<ObjectBucket>, contents: Contents| {
                let footer = writer.chain().close(contents);
                let data = vec![7; 8192];
                let framed = Segment { data, footer: footer.clone() }
                    .encode(writer.keys().encrypting())
                    .into();
                let segment = writer
                    .add_segment(Objects { footer, segment: framed, own: Vec::new() })
                    .unwrap();
                writer.upload(segment).await.unwrap();
                (segment, writer.acknowledge().await.unwrap())
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
            let at = |segment, offset| Location {
                key: layout::segment(test_branch(1), segment, 1),
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
            assert_eq!(next.taken().chain.next_segment(), begun, "where its own go");
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
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            assert_eq!(parent.state().capacity, None);
            let resize = |tag, capacity: u64| {
                SegmentBuilder::from(Commit { tag, capacity: Some(capacity), ..Commit::default() })
            };
            let landed = async |writer: &Writer<ObjectBucket>, objects| {
                let segment = writer.add_segment(objects).unwrap();
                writer.upload(segment).await.unwrap();
                writer.acknowledge().await.unwrap();
                segment
            };
            let forked = resize(1, 1 << 20)
                .fork(child(test_branch(2)), &mut parent.chain())
                .frame(parent.keys().encrypting());
            let forked = Origin { parent: test_branch(1), at: landed(&parent, forked).await };
            let point = Child { branch: test_branch(3), owner: 0, point: true };
            let pointed = SegmentBuilder::default()
                .fork(point, &mut parent.chain())
                .frame(parent.keys().encrypting());
            let pointed = Origin { parent: test_branch(1), at: landed(&parent, pointed).await };
            landed(
                &parent,
                resize(2, 2 << 20).close(&mut parent.chain()).frame(parent.keys().encrypting()),
            )
            .await;
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
            landed(
                &child,
                resize(3, 4 << 20).close(&mut child.chain()).frame(child.keys().encrypting()),
            )
            .await;
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
            let bucket = Arc::new(ObjectBucket::new(memory.clone(), ""));
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let extent = ExtentId::new(1, parent.chain().next_sequence());
            let extents = vec![(extent, b"one".to_vec())];
            let batch = SegmentBuilder::from(Commit { tag: 1, extents, ..Commit::default() });
            let at = parent
                .add_segment(
                    batch
                        .fork(child(test_branch(2)), &mut parent.chain())
                        .frame(parent.keys().encrypting()),
                )
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
            let commit = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
            let segment = child
                .add_segment(commit.close(&mut child.chain()).frame(child.keys().encrypting()))
                .unwrap();
            child.upload(segment).await.unwrap();
            child.acknowledge().await.unwrap();
            assert_eq!(child.checkpoint().await, Ok(Some(segment)));
            let recovered = recover(&*bucket, test_branch(2), origin).await.unwrap();
            assert_eq!(recovered.checkpoint(), Some((segment, child.epoch())));
            assert_eq!(recovered.state.branch, test_branch(2));
            assert_eq!(recovered.state.extents.keys().collect::<Vec<_>>(), [&extent]);

            // The parent's checkpoint, at the key the fork's head names, as a bucket corrupting it.
            let parents = parent.checkpoint().await.unwrap().unwrap();
            let object = bucket::get(&*bucket, &layout::checkpoint(test_branch(1), parents, 1))
                .await
                .unwrap();
            let key = layout::checkpoint(test_branch(2), segment, child.epoch());
            let overwrite = crate::bucket::PutMode::Overwrite;
            crate::bucket::Bucket::put(&*bucket, &key, object.bytes.into(), overwrite)
                .await
                .unwrap();
            // Read by another host, which decoded nothing of it before: a key names one object,
            // which a host's bucket keeps decoded (TASK-407).
            let corrupt = format!("{key} holds a checkpoint of 1/1/1");
            let other = ObjectBucket::new(memory, "");
            assert_eq!(
                recover(&other, test_branch(2), origin).await.unwrap_err(),
                Error::Corrupt(corrupt)
            );
        });
    }
}
