// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A cell's state, recovered from its objects: what a writer taking the cell starts from, and
//! what the collector keeps. Extents are read from where it says they are by fetchers
//! ([`crate::fetch`]).
//!
//! A cell's state is what its head names (doc-7, The head; TASK-407): its newest checkpoint, or,
//! with none, its parent's state where it was forked, then the footers of the segments the head
//! names after it, in order: read by their objects' tails, which hold them, many at once, every key
//! known. Each key is written once, so that what a reader reads at it is the one object it names,
//! ever. cladefs: a commit may span segments, each naming its runs of it, which a reader holds
//! until the segment ending it ([`Begun`], TASK-92.41): a head names whole commits alone. A state
//! recovered is recovered on by the segments the head names since ([`recover_on`]): a writer's,
//! once it holds the head, from what it found before.
//!
//! The collector deletes what no reader starting now needs: of what a head names, segments only
//! once its checkpoint covers them. A reader that finds something the head named gone reads the
//! head again: if it changed, a newer checkpoint may cover what went, and the reader starts again.
//!
//! cladefs: a recovery reads holding a ring ([`crate::keys::Ring`], TASK-411.3): its footers and
//! checkpoints are checked against their MACs under the keys it holds, and one whose encrypt says
//! the clear is corrupt (TASK-411.28). The collector's holds none, reading them as it did.

use crate::bucket::{self, Bucket, BucketFuture, ByteRange, Meta};
use crate::keys::Ring;
use crate::layout::{self, CellId, Contents, Footer, Head, Location, Origin};
use crate::{Error, head, object};
use extent_frame::spent::Spent;
use extent_frame::table::TableFooter;
use fprint::TypeFingerprint;
use futures::future::{FutureExt as _, Shared};
use futures::{StreamExt as _, TryStreamExt as _};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use storage_device::ExtentId;

/// What a cell holds as of a segment: where its extents are, `P`, and the rest of its state. One
/// replay makes it, [`State::apply`], whoever reads the cell's history: its recovery, so its
/// writers, its collector, its versions and its deletion; a writer acknowledging its segments; and
/// a local cell rebuilding it from the footers of its closed segments (TASK-165.3). Each places the
/// extents as it keeps them ([`Places`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State<P = Extents> {
    pub cell: CellId,
    pub extents: P,
    pub next_sequence: u64,
    pub root: Option<Vec<u8>>,
    /// The tag of the last commit, which may be the parent's, before the fork.
    pub last_commit: Option<u64>,
    /// The last segment applied.
    pub through: Option<u64>,
    /// The sequence numbers of the cell's own extents it freed, which it writes no more: one
    /// rule, every free of them a footer names (TASK-79.6, TASK-150.6, TASK-165.3).
    pub spent: Spent,
    /// The most bytes its extents may hold, as its last commit to set it set it, if one did: its
    /// own, or, before its first, its parent's at the fork (TASK-165.3).
    pub capacity: Option<u64>,
    /// cladefs: the runs of the commit the segments applied last went on with, which a later
    /// segment ends, if they did ([`Begun`], TASK-92.41): none in a state a head names, nor in any
    /// checkpoint, as writers acknowledge only segments ending their commits.
    pub begun: Option<Begun>,
    /// cladefs: the length of the data of each of the cell's own segments, by key, of those applied
    /// and those a checkpoint it started from kept ([`State::named_lengths`]): what its writer
    /// measures their density against, the bytes it holds in each, to choose what to relocate
    /// (doc-27), with no request to learn it. Kept in a checkpoint by number, not by key
    /// ([`crate::checkpoint`]).
    #[serde(skip)]
    pub lengths: Lengths,
}

/// cladefs: the length of the data of each of a cell's own segments, by key ([`State::lengths`]).
pub type Lengths = imbl::OrdMap<String, u64>;

/// cladefs: the runs a commit sealed in segments applied that a later segment ends
/// (`extent_frame::segment::Continued`, TASK-92.41): sealed with it, where they are, once it does;
/// or never, if the next segment applied neither goes on with it nor ends it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Begun {
    pub commit: u64,
    pub runs: Vec<(ExtentId, Location)>,
}

impl TypeFingerprint for State {
    fn fingerprint() -> String {
        // Exhaustive destructure so adding, removing, or renaming any field fails to compile.
        let _fields = |s: &State| {
            let State {
                cell,
                extents,
                next_sequence,
                root,
                last_commit,
                through,
                spent,
                capacity,
                begun,
                lengths,
            } = s;
            let _: (
                &CellId,
                &Extents,
                &u64,
                &Option<Vec<u8>>,
                &Option<u64>,
                &Option<u64>,
                &Spent,
                &Option<u64>,
                &Option<Begun>,
                &Lengths,
            ) = (
                cell,
                extents,
                next_sequence,
                root,
                last_commit,
                through,
                spent,
                capacity,
                begun,
                lengths,
            );
        };
        format!(
            "struct {{cell:{},extents:OrdMap<{},{}>,next_sequence:{},root:{},last_commit:{},through:{},spent:{},capacity:{},begun:{}}}",
            CellId::fingerprint(),
            ExtentId::fingerprint(),
            Location::fingerprint(),
            u64::fingerprint(),
            Option::<Vec<u8>>::fingerprint(),
            Option::<u64>::fingerprint(),
            Option::<u64>::fingerprint(),
            Spent::fingerprint(),
            Option::<u64>::fingerprint(),
            Option::<Begun>::fingerprint(),
        )
    }
}

/// Where a cell's extents are in the bucket.
pub type Extents = imbl::OrdMap<ExtentId, Location>;

/// Where a cell's extents are, as a replay of its segments places them ([`State::apply`]).
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
    pub fn new(cell: CellId) -> Self {
        Self::of(cell, Extents::new())
    }
}

impl<P> State<P> {
    /// The state of `cell` before its first segment, its extents placed in `extents`.
    pub fn of(cell: CellId, extents: P) -> Self {
        Self {
            cell,
            extents,
            next_sequence: 0,
            root: None,
            last_commit: None,
            through: None,
            spent: Spent::default(),
            capacity: None,
            begun: None,
            lengths: Lengths::new(),
        }
    }

    /// The state of `child`, forked from this one, before its first segment, its extents placed
    /// in `extents`: what it inherits but its extents, its root, its last commit and its capacity.
    fn inheriting<Q>(&self, child: CellId, extents: Q) -> State<Q> {
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
            cell,
            extents,
            next_sequence,
            root,
            last_commit,
            through,
            spent,
            capacity,
            begun,
            lengths,
        } = self;
        let extents = place(extents);
        State {
            cell,
            extents,
            next_sequence,
            root,
            last_commit,
            through,
            spent,
            capacity,
            begun,
            lengths,
        }
    }
}

impl State {
    /// cladefs: its lengths, but those of segments it holds no extent in: those a checkpoint of it
    /// keeps ([`State::lengths`]).
    pub fn named_lengths(&self) -> Lengths {
        let named: HashSet<&str> = self.extents.values().map(|at| at.key.as_str()).collect();
        let lengths = self.lengths.iter().filter(|(key, _)| named.contains(key.as_str()));
        lengths.map(|(key, len)| (key.clone(), *len)).collect()
    }
}

impl<P: Places> State<P> {
    /// Applies `footer`'s segment, the next its head names, its data `len` bytes long.
    pub fn apply(&mut self, footer: &Footer, len: u64) {
        self.through = Some(footer.segment);
        self.apply_contents(footer.segment, footer.epoch, &footer.contents);
        self.lengths.insert(layout::segment(self.cell, footer.segment, footer.epoch), len);
    }

    /// Applies `contents`, those of the cell's segment `segment`, of the writer of epoch `epoch`:
    /// what its commits sealed, where they sealed it, then what they freed; the root they set,
    /// their last commit, the cell's next sequence number, the largest named, and the capacity they
    /// set. Every free of the cell's own extents spends it, as the cell that freed it spent it: a
    /// footer frees only what a commit held, not what was freed while open (TASK-160.15), and one
    /// sealed and freed within the segment is a free and no seal (TASK-165.3). cladefs: first, the
    /// runs of a commit the segments before began ([`Begun`]), if this one ends it, or holding them
    /// on if it goes on with it; then, after its own, holding those of the commit it goes on with,
    /// if any (TASK-92.41).
    pub fn apply_contents(&mut self, segment: u64, epoch: u64, contents: &Contents) {
        let key = layout::segment(self.cell, segment, epoch);
        match self.begun.take() {
            Some(begun) if contents.ends(begun.commit) => {
                for (extent, location) in begun.runs {
                    self.extents.seal(extent, Seal::Run(location));
                }
            }
            Some(begun) if contents.continues(begun.commit) => self.begun = Some(begun),
            // Never ended: a writer's, never acknowledged. A head names none such: a cell's
            // replay of its own segments, closed again, may (wal::recover).
            Some(_) | None => {}
        }
        for &(extent, offset, len) in &contents.extents {
            self.extents.seal(extent, Seal::Run(Location { key: key.clone(), offset, len }));
        }
        for (extent, location) in contents.own_at(self.cell, epoch, segment) {
            self.extents.seal(extent, Seal::Own(location));
        }
        for &extent in &contents.frees {
            self.extents.free(extent);
            if extent.cell == self.cell.id {
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

/// A cell's state as its head names it, and what reading it took: its [`State`], or its
/// [`Layers`].
#[derive(Clone, Debug)]
pub struct Recovered<S = State> {
    pub state: S,
    /// The head it was recovered from, as read: none if the cell has none, as a fork's child or
    /// a version before any writer took it. Its end is where the next writer's segments go.
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
    /// The number of segments the cell holds, where the next writer's go.
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
    /// The cell's state, its layers in one.
    pub(crate) fn flatten(self) -> Recovered {
        let Self { state, head, touched, layer } = self;
        let state = state.placed(Layered::flatten);
        Recovered { state, head, touched, layer }
    }
}

/// A cell's state in layers ([`Layered`]).
pub(crate) type Layers = State<Layered>;

/// Where a cell's extents are, in layers: what it inherited, if it has no checkpoint of its own,
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
    fn inheriting(inherited: Arc<State>, child: CellId) -> Layers {
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

/// How often recovery starts again before it gives up on a cell as lost.
const RESTARTS: usize = 64;

/// The ring of a recovery holding no key.
// Immutable once made: the same for every simulated host of the process.
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

impl From<crate::bucket::Refused> for Attempt {
    fn from(refused: crate::bucket::Refused) -> Self {
        Self::Failed(refused.into())
    }
}

/// The state of `cell`, forked from `origin`, as its head names it, read holding no key.
pub async fn recover(
    bucket: &impl Bucket,
    cell: CellId,
    origin: Option<Origin>,
) -> Result<Recovered, Error> {
    recover_under(bucket, cell, origin, &Ring::keyless()).await
}

/// cladefs: the state of `cell`, forked from `origin`, read holding `ring` (TASK-411.3).
pub async fn recover_under(
    bucket: &impl Bucket,
    cell: CellId,
    origin: Option<Origin>,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let checkpoints = Checkpoints::new(bucket, 0).holding(ring);
    Ok(recover_layers(&checkpoints, cell, origin).await?.flatten())
}

/// cladefs: the state of `cell` as [`recover_under`] finds it, starting from `head`, what a read of
/// its head found, rather than a read of its own (TASK-449): as an opening cell reads its head at
/// once with its record, and a cell at a version knows it has none. The head read before is as good
/// as one read now: it names what the cell held as of that read, the recovery is of that, and
/// whatever takes the cell after goes on from what is there then (`Writer::take`). Something it
/// names gone, it starts again from a read of the head, as [`recover_under`] does.
pub async fn recover_under_from(
    bucket: &impl Bucket,
    cell: CellId,
    origin: Option<Origin>,
    head: Option<Head>,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let checkpoints = Checkpoints::new(bucket, 0).holding(ring);
    Ok(recover_layers_from(&checkpoints, cell, origin, Some(head)).await?.flatten())
}

/// The state of `cell`, forked from `origin`, as `head` names it, a head its reader holds, as a
/// writer that took it does, or a deletion that ended it: nothing it names goes while it holds it,
/// but for a takeover since, which fails the writer at its next swap ([`Error::Fenced`]). What it
/// inherited may go meanwhile, an origin's state its parent's checkpoint covers: it starts
/// again. cladefs: read holding `ring` (TASK-411.3).
pub async fn recover_from(
    bucket: &impl Bucket,
    cell: CellId,
    origin: Option<Origin>,
    head: Option<Head>,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let checkpoints = Checkpoints::new(bucket, 0).holding(ring);
    for _ in 0..RESTARTS {
        match attempt(&checkpoints, cell, origin, head.clone()).await {
            Ok(recovered) => return Ok(recovered.flatten()),
            Err(Attempt::Restart) if head::get(bucket, cell).await? != head => {
                return Err(Error::Fenced);
            }
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!("cell {cell}: what its recovery starts from keeps going")))
}

/// The state of `cell`, forked from `origin`, as [`recover`] finds it, in layers: reading the
/// checkpoint it starts from through `checkpoints`, which other recoveries share.
pub(crate) async fn recover_layers<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    origin: Option<Origin>,
) -> Result<Recovered<Layers>, Error> {
    recover_layers_from(checkpoints, cell, origin, None).await
}

/// [`recover_layers`], its first attempt from `first`, a head read before, if given
/// ([`recover_under_from`]).
async fn recover_layers_from<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    origin: Option<Origin>,
    mut first: Option<Option<Head>>,
) -> Result<Recovered<Layers>, Error> {
    for _ in 0..RESTARTS {
        let head = match first.take() {
            Some(head) => head,
            None => head::get(checkpoints.bucket, cell).await?,
        };
        match attempt(checkpoints, cell, origin, head).await {
            Ok(recovered) => return Ok(recovered),
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!("cell {cell}: what its recovery starts from keeps going")))
}

/// Retries a recovery walk up to [`RESTARTS`] times when `Attempt::Restart` signals a concurrently
/// deleted checkpoint/segment, returning `(value, touched)` or [`Error::Lost`].
macro_rules! retry_recovery {
    ($attempt:expr, $lost:expr) => {{
        for _ in 0..RESTARTS {
            let mut touched = BTreeSet::new();
            match ($attempt)(&mut touched).await {
                Ok(value) => return Ok((value, touched)),
                Err(Attempt::Restart) => {}
                Err(Attempt::Failed(error)) => return Err(error),
            }
        }
        Err(Error::Lost($lost))
    }};
}

/// cladefs: the state of `cell` as of the end of its segment `at`, which its head names: a fork
/// version's, as a fork made there inherits it; and what reading it read, which a mirror copies
/// (doc-23) and tests check what a pass kept against. Its newest checkpoint up to there, or its
/// origin's state, then its segments to there, starting again where it finds one gone that a
/// newer checkpoint covers: [`Error::Lost`] once it keeps going, or what it needs is gone. The
/// collector keeps such states keys-only ([`keys_at`]).
#[cfg(feature = "full")]
pub(crate) async fn recover_at<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    at: u64,
) -> Result<(Arc<State>, BTreeSet<String>), Error> {
    retry_recovery!(
        |touched| state_at(checkpoints, cell, at, touched, true),
        format!(
            "cell {cell} at {at}: what its recovery starts from keeps going",
            at = base32_id::Short(at)
        )
    )
}

/// cladefs: the state of `cell` as of its segment `at`, a version's (doc-18, TASK-476), as a copy
/// of the version holds it: its newest checkpoint up to there, then the segments its head names to
/// there, read holding `ring`; the decoded state it is, shared, its layer. No head: nothing writes
/// a version.
pub async fn recover_version(
    bucket: &impl Bucket,
    cell: CellId,
    at: u64,
    ring: &Ring,
) -> Result<Recovered, Error> {
    let checkpoints = Checkpoints::new(bucket, 0).holding(ring);
    for _ in 0..RESTARTS {
        let mut touched = BTreeSet::new();
        match state_at(&checkpoints, cell, at, &mut touched, false).await {
            Ok(state) => {
                let recovered = State::clone(&state);
                return Ok(Recovered { state: recovered, head: None, touched, layer: Some(state) });
            }
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!(
        "cell {cell} at {at}: what its recovery starts from keeps going",
        at = base32_id::Short(at)
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
    /// listing of checkpoints once: each cell's, and each origin's a pass keeps (TASK-395.16,
    /// TASK-395.19, [`Checkpoints::listed`]).
    listed: Option<&'a BTreeMap<String, Meta>>,
    served: Mutex<BTreeSet<(CellId, Option<u64>)>>,
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
type Flight<'a> = Shared<BucketFuture<'a, Result<Option<Arc<State>>, Error>>>;

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

    /// cladefs: these, the first listing of each cell's checkpoints served by `listed`, a listing
    /// of the cell the collector's pass took before it read anything (TASK-395.16), if it has one:
    /// a LIST less a cell. An origin's state ([`state_at`]) is as right from the newest
    /// checkpoint up to it listed then as from one listed now: it reads its segments after it, and,
    /// finding one gone, lists the checkpoints again, starting over if a newer one has come; and a
    /// recovery starting over lists them anew, from the bucket. Each cell's once: what started
    /// over may be what the listing shows.
    ///
    /// And the first of each origin's state a pass keeps for the references and forks to other
    /// cells ([`recover_at`]), apart from its cell's and from one another's (TASK-395.19): a LIST
    /// less an origin, which a pass over a cell whose cell was forked at many versions made for
    /// each. A state as of a segment is the same from any checkpoint up to there, the newest listed
    /// then as the newest listed now, its segments to there read after it: one it finds gone, a
    /// newer checkpoint covering it, starts it over, listing anew. A checkpoint made since the
    /// listing, at or before the origin, which the recovery doesn't read, the pass doesn't
    /// delete either: what it deletes it listed. An origin's state a cell's recovery starts from
    /// goes by the cell's listing, as before: served anew, it would read none of the checkpoints
    /// made during the pass, keeping their segments instead, safe but hiding what the test
    /// `children_restored_from_points_keeping_their_origins_need_recoveries_by_origins` shows.
    pub(crate) fn listed(self, listed: Option<&'a BTreeMap<String, Meta>>) -> Self {
        Self { listed, ..self }
    }

    /// cladefs: the checkpoints of `cell`, in the order of their keys, for a recovery of it, or of
    /// the state of it as of its segment `at` a pass keeps: as the listing of its cell shows them,
    /// the first time one of that is asked, if it has one; listed now otherwise.
    async fn list(&self, cell: CellId, at: Option<u64>) -> Result<Vec<Meta>, bucket::Refused> {
        let prefix = layout::checkpoints(cell);
        match self.listed {
            Some(listed) if self.served.lock().unwrap().insert((cell, at)) => {
                Ok(bucket::under(listed, &prefix))
            }
            _ => bucket::list(self.bucket, &prefix).await,
        }
    }

    /// The state the checkpoint `listed` of `cell` holds, if it is there
    /// ([`Checkpoints::read_key`]).
    async fn read(&self, listed: &Meta, cell: CellId) -> Result<Option<Arc<State>>, Error> {
        self.read_key(&listed.key, cell).await
    }

    /// The state the checkpoint at `key` of `cell` holds, if it is there: read now, or by the read
    /// of it in flight or kept, or as the bucket decoded it before ([`Bucket::decoded`]), by its
    /// key alone, written once (TASK-407). One gone, or that failed, is read anew the next time.
    async fn read_key(&self, key: &str, cell: CellId) -> Result<Option<Arc<State>>, Error> {
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
                        let Some(object) = bucket::get(bucket, &key).await? else {
                            return Ok(None);
                        };
                        let state = checkpoint(&key, &object.bytes, cell, keys)?;
                        Ok(Some(match decoded {
                            Some(decoded) => decoded.keep(&key, state),
                            None => Arc::new(state),
                        }))
                    };
                    #[cfg(not(target_arch = "wasm32"))]
                    let flight = flight.boxed().shared();
                    #[cfg(target_arch = "wasm32")]
                    let flight = flight.boxed_local().shared();
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
/// cladefs: and every state decoded that something still holds, as an open cell holds the one it
/// started from ([`Recovered::layer`]), however many were decoded since (TASK-453): the forks of a
/// base opened after it find it, and share it, while one is open. And a state decoded anew shares
/// what it holds alike with the state held still that has the most of its extents' cells' in
/// common with it, its map made of that one's, changed where it differs ([`rebased`]): the
/// checkpoints a live cell's writer writes for each of its forks hold the same extents, and a
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
struct States {
    states: HashMap<String, Arc<State>>,
    order: VecDeque<String>,
    live: HashMap<String, Live>,
    /// Maximum estimated bytes held by strong references in `states` (board 11554).
    max_bytes: u64,
}

impl Default for States {
    fn default() -> Self {
        Self {
            states: HashMap::new(),
            order: VecDeque::new(),
            live: HashMap::new(),
            max_bytes: Decoded::DEFAULT_KEEP_BYTES,
        }
    }
}

impl States {
    /// Estimated heap bytes of the strong references currently retained in `self.states` if every
    /// external caller dropped its handle: a rebased state only counts its delta when the state it
    /// was rebased on is also retained strongly in `self.states`, so a small fork of an oversized
    /// base cannot pin that base's shared persistent map nodes in `self.states` after the base
    /// closes (board 11554).
    fn kept_bytes(&self) -> u64 {
        let mut maps: Vec<(&Arc<State>, Changes)> = Vec::new();
        for (key, state) in &self.states {
            let len = state.extents.len();
            let made_of =
                self.live.get(key).and_then(|live| live.made_of.as_ref()).filter(|(made_of, _)| {
                    made_of.strong_count() > 0
                        && self
                            .states
                            .values()
                            .any(|kept| std::ptr::eq(Arc::as_ptr(kept), made_of.as_ptr()))
                });
            let own = made_of.map_or(Changes::all(len), |(_, changes)| changes.within(len));
            match maps.iter_mut().find(|(other, _)| other.extents.ptr_eq(&state.extents)) {
                Some((_, most)) if own.bytes() > most.bytes() => *most = own,
                Some(_) => {}
                None => maps.push((state, own)),
            }
        }
        maps.iter().map(|(_, own)| own.bytes()).sum()
    }

    /// Evicts oldest strong entries from `self.states` until both `order.len() <= Decoded::KEEP`
    /// and `kept_bytes() <= target`. Returns the removed `Arc<State>`s so callers can drop them
    /// outside the mutex lock.
    fn evict_to(&mut self, target: u64) -> Vec<Arc<State>> {
        let mut evicted = Vec::new();
        while self.order.len() > Decoded::KEEP
            || (!self.order.is_empty() && self.kept_bytes() > target)
        {
            let oldest = self.order.pop_front().unwrap();
            if let Some(state) = self.states.remove(&oldest) {
                evicted.push(state);
            }
        }
        evicted
    }
}

/// A state decoded, which others may hold still ([`Decoded`]): how many of its extents each cell
/// has, by its ID, which its likeness to another's is reckoned by; and, if it was made of another
/// ([`rebased`]), that one and what it changed of it.
struct Live {
    state: Weak<State>,
    cells: BTreeMap<u64, usize>,
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
/// to the leaf it is in, the leaf (16 entries, their locations' keys copied too) and the cells
/// over it, unless a change before copied them, and, a leaf left too small, its sibling. Changes
/// side by side share one path; scattered, each copies its own: measured 1.9-2.1 KB a change,
/// every 32nd entry of 32,768 changed (`bench_fork_extent_memory`, TASK-453), and bounded at every
/// spacing and size by `tests::changes_bound_what_maps_made_of_others_copy`. What
/// [`Changes::bytes`], and local_cell's count of what its cells' writers change of the maps
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

    /// Default byte cap on idle decoded states kept strongly in [`States::states`] (board 11554):
    /// large checkpoints (such as multi-million-extent workspace bases) are still shared across
    /// concurrent open cells via [`States::live`] weak references, but are freed once their last
    /// open cell closes rather than pinning gigabytes in the daemon indefinitely.
    pub const DEFAULT_KEEP_BYTES: u64 = 256 << 20;

    #[cfg(test)]
    fn with_max_bytes(max_bytes: u64) -> Self {
        Self { kept: Mutex::new(States { max_bytes, ..States::default() }) }
    }

    /// Estimated heap bytes of strong entries currently retained in the idle decoded cache.
    pub fn kept_bytes(&self) -> u64 {
        self.kept.lock().unwrap().kept_bytes()
    }

    /// Evicts oldest strong entries from the idle decoded cache until at most `target` bytes
    /// remain, returning the bytes evicted. Open cells still share live states via weak refs.
    pub fn evict_to(&self, target: u64) -> u64 {
        let (before, after, evicted) = {
            let mut kept = self.kept.lock().unwrap();
            let before = kept.kept_bytes();
            let evicted = kept.evict_to(target);
            let after = kept.kept_bytes();
            (before, after, evicted)
        };
        drop(evicted);
        before.saturating_sub(after)
    }

    /// The state the checkpoint at `key` holds, if it kept it, or another holds it still.
    fn get(&self, key: &str) -> Option<Arc<State>> {
        let kept = self.kept.lock().unwrap();
        let held = || kept.live.get(key).and_then(|live| live.state.upgrade());
        kept.states.get(key).cloned().or_else(held)
    }

    /// Keeps `state`, which the checkpoint at `key` holds, just decoded: its extents made of the
    /// most alike of the states others hold still, if one is alike ([`rebased`]). Returns it, or
    /// the one at `key` another decoded meanwhile, if one did. Made holding no lock: one walk of
    /// both.
    fn keep(&self, key: &str, mut state: State) -> Arc<State> {
        let cells = cells_of(&state.extents);
        let alike = {
            let kept = self.kept.lock().unwrap();
            let likeness = |live: &Live| -> usize {
                let common = live
                    .cells
                    .iter()
                    .filter_map(|(cell, held)| cells.get(cell).map(|holds| (*holds).min(*held)));
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
        let (state, evicted) = {
            let mut kept = self.kept.lock().unwrap();
            if let Some(held) = kept.live.get(key).and_then(|live| live.state.upgrade()) {
                return held;
            }
            let state = Arc::new(state);
            kept.live.retain(|_, live| live.state.strong_count() > 0);
            let live = Live { state: Arc::downgrade(&state), cells, made_of };
            kept.live.insert(key.to_owned(), live);
            if kept.states.insert(key.to_owned(), state.clone()).is_none() {
                kept.order.push_back(key.to_owned());
            }
            let max_bytes = kept.max_bytes;
            let evicted = kept.evict_to(max_bytes);
            (state, evicted)
        };
        drop(evicted);
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

/// How many of `extents` each cell has, by its ID.
fn cells_of(extents: &Extents) -> BTreeMap<u64, usize> {
    let mut cells = BTreeMap::new();
    for extent in extents.keys() {
        *cells.entry(extent.cell).or_default() += 1;
    }
    cells
}

/// cladefs: `fresh`, a state's extents just decoded, made of `alike`'s, another's of its cell, as
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

/// The state of the cell `before` recovered, forked from `origin`, recovered on from it to what
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
    let cell = state.cell;
    let from = was.as_ref().map_or(0, |was| was.end);
    // A checkpoint since, covering what it would follow on from: the collector, going by it, may
    // have deleted those segments, and it recovers anew.
    if head.as_ref().and_then(|head| head.checkpoint).is_some_and(|(segment, _)| segment >= from) {
        return recover_from(bucket, cell, origin, head, ring).await;
    }
    let mut recovered = Recovered { state: Layered::of(state), head: head.clone(), touched, layer };
    match follow((bucket, ring), &mut recovered, from).await {
        Ok(()) => Ok(recovered.flatten()),
        Err(Attempt::Restart) => recover_from(bucket, cell, origin, head, ring).await,
        Err(Attempt::Failed(error)) => Err(error),
    }
}

/// The state `head`, `cell`'s, forked from `origin`, names: its checkpoint, or its origin's
/// state, then its segments after. Something it names gone, it starts again if the head changed.
async fn attempt<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    origin: Option<Origin>,
    head: Option<Head>,
) -> Result<Recovered<Layers>, Attempt> {
    let bucket = checkpoints.bucket;
    let mut touched = BTreeSet::new();
    let checkpoint = head.as_ref().and_then(|head| head.checkpoint(cell));
    let mut layer = None;
    let state = match checkpoint {
        Some(key) => {
            let Some(state) = checkpoints.read_key(&key, cell).await? else {
                return Err(moved(bucket, cell, head.as_ref()).await);
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
            None => Layered::of(State::new(cell)),
            Some(origin) => {
                let at = state_of_origin(checkpoints, origin, cell, &mut touched).await;
                match at {
                    Ok(at) => {
                        layer = Some(at.clone());
                        Layered::inheriting(at, cell)
                    }
                    // A checkpoint of its own since, which the collector going by may have let
                    // what it inherits go for: the head changed (TASK-177).
                    Err(Attempt::Failed(Error::Lost(lost))) => {
                        return Err(match moved(bucket, cell, head.as_ref()).await {
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
/// [`AT_ONCE`] at most at once, holding `keys`. One gone, the head is read again: changed, a
/// checkpoint newer may cover it, and the recovery starts again; else what it names is lost.
async fn follow<B: Bucket>(
    (bucket, keys): (&B, &Ring),
    recovered: &mut Recovered<Layers>,
    from: u64,
) -> Result<(), Attempt> {
    let Recovered { state, head, touched, .. } = recovered;
    let Some(named) = head.as_ref() else { return Ok(()) };
    let cell = state.cell;
    let mut footers = futures::stream::iter(named.segments(from))
        .map(|(segment, epoch)| async move {
            let key = layout::segment(cell, segment, epoch);
            let footer = segment_footer((bucket, keys), cell, segment, epoch).await?;
            Ok::<_, Error>((key, footer))
        })
        .buffered(AT_ONCE);
    while let Some((key, footer)) = footers.try_next().await? {
        let Some((footer, len)) = footer else {
            drop(footers);
            return Err(moved(bucket, cell, head.as_ref()).await);
        };
        state.apply(&footer, len);
        touched.insert(key);
    }
    Ok(())
}

/// What a recovery of `cell` from `head` that found something it names gone does: start again
/// if the head changed since, which a checkpoint newer may have; fail as lost otherwise.
async fn moved(bucket: &impl Bucket, cell: CellId, head: Option<&Head>) -> Attempt {
    match head::get(bucket, cell).await {
        Err(error) => Attempt::Failed(error),
        Ok(now) if now.as_ref() != head => Attempt::Restart,
        Ok(_) => Attempt::Failed(Error::Lost(format!("cell {cell}: what its head names"))),
    }
}

/// The footer of segment `seq` of `cell` by the writer of `epoch`, if it is there, and the length
/// of its data, as its trailer says: read by its object's tail, a ranged read of its last bytes,
/// which hold them ([`object::tail`]), not by the segment whole. cladefs: checked against its MAC
/// if `keys` hold its key (TASK-411.3); in the clear, corrupt (TASK-411.28).
async fn segment_footer(
    (bucket, keys): (&impl Bucket, &Ring),
    cell: CellId,
    segment: u64,
    epoch: u64,
) -> Result<Option<(Footer, u64)>, Error> {
    let key = layout::segment(cell, segment, epoch);
    let Some(tail) = object::tail(bucket, &key).await? else { return Ok(None) };
    if !keys.is_empty() {
        tail.open(&keys.of(cell.id))?;
    }
    let footer = placed(&key, layout::decode_segment_footer(tail.footer())?, cell, segment, epoch)?;
    Ok(Some((footer, tail.parts.frame.data_len)))
}

/// `footer`, that of the object at `key`, which holds segment `segment` of `cell` by the writer
/// of `epoch`, if it says so.
fn placed(
    key: &str,
    footer: Footer,
    cell: CellId,
    segment: u64,
    epoch: u64,
) -> Result<Footer, Error> {
    if footer.cell != cell || footer.segment != segment || footer.epoch != epoch {
        return Err(Error::Corrupt(format!(
            "{key} holds segment {} of {} by epoch {}",
            base32_id::Short(footer.segment),
            footer.cell,
            base32_id::Short(footer.epoch)
        )));
    }
    Ok(footer)
}

/// The state checkpoint `key` of `cell`, whose object holds `bytes`, holds: cladefs: read holding
/// `keys` (TASK-411.3).
fn checkpoint(key: &str, bytes: &[u8], cell: CellId, keys: &Ring) -> Result<State, Error> {
    let state = crate::checkpoint::decode_under(bytes, &keys.of(cell.id))?;
    if state.cell != cell {
        return Err(Error::Corrupt(format!("{key} holds a checkpoint of {}", state.cell)));
    }
    Ok(state)
}

/// The newest of `cell`'s checkpoints `listed` as of its segment `at` or before: by number, then
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

/// cladefs: recovers the state of `origin` for a child `from`: when crossing cells
/// (`origin.parent != from`), GETs `checkpoint(origin.parent, origin.at, origin.epoch)` directly
/// without touching `origin.parent`'s head (board 6017).
async fn state_of_origin<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    origin: Origin,
    from: CellId,
    touched: &mut BTreeSet<String>,
) -> Result<Arc<State>, Attempt> {
    if origin.parent == from {
        return state_at(checkpoints, origin.parent, origin.at, touched, false).await;
    }
    let key = layout::checkpoint(origin.parent, origin.at, origin.epoch);
    match checkpoints.read_key(&key, origin.parent).await? {
        Some(state) if state.through != Some(origin.at) => {
            Err(Attempt::Failed(Error::Corrupt(format!("{key} holds another state"))))
        }
        Some(state) => {
            touched.insert(key);
            Ok(state)
        }
        None => Err(Attempt::Failed(Error::Lost(key))),
    }
}

/// The state of `cell` as of the end of its segment `at`, which its head names: its newest
/// checkpoint up to there, or its origin's state, then the segments its head names up to there. A
/// checkpoint at `at` itself is the state, as read, which the forks made there share.
///
/// cladefs: `kept`, an origin's state a pass keeps ([`recover_at`]), checks the checkpoint at `at`'s
/// exact key first and uses the pass's listing of the cell ([`Checkpoints::listed`]).
fn state_at<'a, B: Bucket>(
    checkpoints: &'a Checkpoints<'_, B>,
    cell: CellId,
    at: u64,
    touched: &'a mut BTreeSet<String>,
    kept: bool,
) -> BucketFuture<'a, Result<Arc<State>, Attempt>> {
    Box::pin(async move {
        let bucket = checkpoints.bucket;
        // cladefs: no head, no segment named, whatever is kept (board 8543): the cell removed, or
        // being removed, its checkpoints listed still.
        let Some(head) = head::get(bucket, cell).await? else {
            return Err(Attempt::Failed(Error::Lost(layout::head(cell))));
        };
        if kept {
            let Some(epoch) = head.epoch_of(at) else {
                return Err(Attempt::Failed(Error::Lost(format!(
                    "segment {at} of {cell}",
                    at = base32_id::Short(at)
                ))));
            };
            let key = layout::checkpoint(cell, at, epoch);
            match checkpoints.read_key(&key, cell).await? {
                Some(state) if state.through != Some(at) => {
                    let corrupt = Error::Corrupt(format!("{key} holds another state"));
                    return Err(Attempt::Failed(corrupt));
                }
                Some(state) => {
                    touched.insert(key);
                    return Ok(state);
                }
                None => {}
            }
        }
        let listed = checkpoints.list(cell, kept.then_some(at)).await?;
        walk(checkpoints, cell, &head, newest_to(&listed, at), at, touched).await
    })
}

/// cladefs: the state of `cell` as of its segment `at`: from `start`, the newest checkpoint up to
/// it, or, with none, its origin's state, then the segments `head` names up to it ([`state_at`]).
async fn walk<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    head: &Head,
    start: Option<&Meta>,
    at: u64,
    touched: &mut BTreeSet<String>,
) -> Result<Arc<State>, Attempt> {
    let bucket = checkpoints.bucket;
    let mut state = match start {
        Some(meta) => {
            let Some(state) = checkpoints.read(meta, cell).await? else {
                return Err(Attempt::Restart);
            };
            touched.insert(meta.key.clone());
            // At the checkpoint itself: the state, as read, which the forks made there share.
            if state.through == Some(at) {
                return Ok(state);
            }
            Layered::of(Arc::unwrap_or_clone(state))
        }
        None => {
            touched.insert(layout::head(cell));
            let inherited = match head.origin() {
                None => Ok(None),
                Some(origin) => state_of_origin(checkpoints, origin, cell, touched).await.map(Some),
            };
            match inherited {
                Ok(None) => Layered::of(State::new(cell)),
                Ok(Some(inherited)) => Layered::inheriting(inherited, cell),
                Err(failed) => return Err(unless_checkpointed(bucket, cell, at, failed).await),
            }
        }
    };
    let from = state.through.map_or(0, |segment| segment + 1);
    let mut footers =
        futures::stream::iter(head.segments(from).take_while(|&(segment, _)| segment <= at))
            .map(|(segment, epoch)| async move {
                let key = layout::segment(cell, segment, epoch);
                Ok::<_, Error>((
                    key,
                    segment_footer((bucket, checkpoints.keys), cell, segment, epoch).await?,
                ))
            })
            .buffered(AT_ONCE);
    while let Some((key, footer)) = footers.try_next().await? {
        // A segment the head names goes only once a checkpoint covers it.
        let Some((footer, len)) = footer else { return Err(Attempt::Restart) };
        state.apply(&footer, len);
        touched.insert(key);
    }
    drop(footers);
    // As of `at`; or, the head naming no segment that far, as of its end, as [`state_at`] has it.
    Ok(Arc::new(state.placed(Layered::flatten)))
}

/// cladefs: makes sure `cell` has a checkpoint as of its segment `at`, which its head names: the
/// state a fork made there to another cell inherits, which the fork's recovery reads by its key
/// alone, so that a pass over the fork's cell lists nothing of `cell`'s ([`state_at`],
/// TASK-395.4). Written, create-only, at the key of the epoch of that segment's writer, by
/// whoever records such a fork, before its reference and its record
/// ([`crate::lifecycle::record_fork`]), from the newest checkpoint up to there and the segments
/// after: the state any reader finds there, its extents named where they were sealed, never
/// further along their moves than a reader names them. One there stands, any writer's (TASK-407.6).
/// [`Error::Lost`] if the state there is gone: no reader recovers a fork from there either.
///
/// cladefs: authenticated under the DEK `keys` give, `cell`'s, if its objects are encrypted, as
/// its writers' checkpoints are: a fork's child reading it holding `cell`'s keys refuses one in
/// the clear (TASK-411.4). Asked for only if it is to be written.
#[cfg(feature = "full")]
pub async fn checkpoint_at(
    bucket: &impl Bucket,
    cell: CellId,
    at: u64,
    keys: crate::keys::Parents<'_>,
) -> Result<(), Error> {
    let head = head::get(bucket, cell).await?;
    let Some(epoch) = head.as_ref().and_then(|head| head.epoch_of(at)) else {
        return Err(Error::Lost(format!("segment {at} of {cell}", at = base32_id::Short(at))));
    };
    let key = layout::checkpoint(cell, at, epoch);
    let listed = bucket::list(bucket, &layout::checkpoints(cell)).await?;
    if listed.iter().any(|meta| meta.key == key) {
        return Ok(());
    }
    let keys = keys.ring(bucket, cell).await?;
    debug_assert_eq!(keys.encrypting().1.cell, cell.id);
    let checkpoints = Checkpoints::new(bucket, 0);
    for _ in 0..RESTARTS {
        match state_at(&checkpoints, cell, at, &mut BTreeSet::new(), false).await {
            Ok(state) => {
                let checkpoint = crate::checkpoint::encode(&state, keys.encrypting());
                bucket::create(bucket, &key, checkpoint).await?;
                return Ok(());
            }
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!(
        "cell {cell} at {at}: what its recovery starts from keeps going",
        at = base32_id::Short(at)
    )))
}

/// What `failed` gives, a recovery of `cell` that found no checkpoint of it up to its segment
/// `at` and went to what it inherited instead: one that found that lost starts again if a
/// checkpoint of the cell up to there has come since, which the collector, going by it, may have
/// let what it found lost go for: the record of a parent deleted, which no other recovery reads
/// (doc-7, Checkpoints; TASK-177).
async fn unless_checkpointed(
    bucket: &impl Bucket,
    cell: CellId,
    at: u64,
    failed: Attempt,
) -> Attempt {
    if !matches!(failed, Attempt::Failed(Error::Lost(_))) {
        return failed;
    }
    let listed = match bucket::list(bucket, &layout::checkpoints(cell)).await {
        Ok(listed) => listed,
        Err(refused) => return Attempt::Failed(refused.into()),
    };
    if newest_to(&listed, at).is_some() { Attempt::Restart } else { failed }
}

/// cladefs: what the collector needs of a live cell's recovery (doc-7, A pass over a cell): its
/// head, the objects read to recover it, and the keys of the objects holding its extents; never the
/// map of its extents ([`State::extents`]), which a checkpoint of millions decodes to hundreds of
/// MiB. Keys-only recovery, the collector's only one, natively and in the Cloudflare Worker
/// (`gc_wasm`, whose isolate has 128 MiB): [`recover_keys`], [`keys_at`].
pub(crate) struct RecoveredKeys {
    pub head: Option<Head>,
    pub touched: BTreeSet<String>,
    pub named: FxHashSet<String>,
}

impl RecoveredKeys {
    pub fn end(&self) -> u64 {
        self.head.as_ref().map_or(0, |head| head.end)
    }

    pub fn checkpoint(&self) -> Option<(u64, u64)> {
        self.head.as_ref().and_then(|head| head.checkpoint)
    }

    pub fn epoch(&self) -> u64 {
        self.head.as_ref().map_or(0, |head| head.epoch)
    }
}

/// What keys-only recovery keeps of the segments it applies after its base checkpoint, the
/// [`Places`] of its [`State`]: the key of each extent they seal, until one frees it; and, if
/// `track_shadowed`, every extent they seal or free, whose place in the base checkpoint is then no
/// longer the cell's.
struct KeyReplay {
    own: FxHashMap<ExtentId, String>,
    shadowed: FxHashSet<ExtentId>,
    track_shadowed: bool,
}

impl KeyReplay {
    fn new(track_shadowed: bool) -> Self {
        Self { own: FxHashMap::default(), shadowed: FxHashSet::default(), track_shadowed }
    }
}

impl Places for KeyReplay {
    fn seal(&mut self, extent: ExtentId, (Seal::Run(location) | Seal::Own(location)): Seal) {
        if self.track_shadowed {
            self.shadowed.insert(extent);
        }
        self.own.insert(extent, location.key);
    }

    fn free(&mut self, extent: ExtentId) {
        if self.track_shadowed {
            self.shadowed.insert(extent);
        }
        self.own.remove(&extent);
    }
}

/// Adds `keys` to `named`, cloning only those it holds not yet: a pass's versions name mostly the
/// same objects.
fn name_all<'a>(named: &mut FxHashSet<String>, keys: impl IntoIterator<Item = &'a String>) {
    for key in keys {
        if !named.contains(key) {
            named.insert(key.clone());
        }
    }
}

/// cladefs: what keys-only recovery refuses, where the full one would recurse without bound
/// (board 14525): a cell whose head names the cell itself as its origin, with no checkpoint of its
/// own up to where it needs one. No writer makes such a head since forks are of versions alone, into
/// other cells (2026-11-18): a crafted or corrupt one.
fn own_origin(cell: CellId) -> Attempt {
    Attempt::Failed(Error::Corrupt(format!("{cell}'s head names the cell itself as its origin")))
}

/// Reads only a checkpoint's tail (`object::tail`) and decodes its `TableFooter` and extent-less
/// `State`, verifying `state.cell == cell`.
async fn checkpoint_tail(
    bucket: &impl Bucket,
    key: &str,
    cell: CellId,
) -> Result<Option<(object::Tail, TableFooter, State)>, Error> {
    let Some(tail) = object::tail(bucket, key).await? else { return Ok(None) };
    let (footer, state) = crate::checkpoint::Table::decode(tail.footer())?.into_parts();
    if state.cell != cell {
        return Err(Error::Corrupt(format!("{key} holds a checkpoint of {}", state.cell)));
    }
    Ok(Some((tail, footer, state)))
}

/// Returns the object keys referenced by entries of the checkpoint at `key` whose `ExtentId` is
/// not in `shadowed`.
///
/// When `shadowed` is empty, returns `footer.keys` directly without fetching any data parts.
/// Otherwise streams the checkpoint one `extent_frame::PART` (1 MiB) at a time via ranged reads,
/// keeping a `<= TABLE_BLOCK` (64 KiB) carry buffer across part boundaries and stopping early once
/// every key in `footer.keys` has been observed live.
async fn checkpoint_live_keys(
    bucket: &impl Bucket,
    key: &str,
    tail: &object::Tail,
    footer: &TableFooter,
    shadowed: &FxHashSet<ExtentId>,
) -> Result<Option<Vec<String>>, Error> {
    if shadowed.is_empty() || footer.keys.is_empty() {
        return Ok(Some(footer.keys.clone()));
    }
    let mut alive = vec![false; footer.keys.len()];
    let mut remaining = footer.keys.len();
    let mut block_idx = 0usize;
    let mut carry: Vec<u8> = Vec::new();
    let mut part_offset = 0u64;
    let part_count = tail.parts.frame.parts();

    for part_idx in 0..part_count {
        if block_idx >= footer.blocks.len() || remaining == 0 {
            break;
        }
        let part = tail.parts.part(part_idx)?;
        let Some(stored) =
            bucket::get_range(bucket, key, ByteRange::Bounded(part.stored.range())).await?
        else {
            return Ok(None);
        };
        let unpacked = part.unpack(&stored)?;
        let part_end = part_offset
            .checked_add(unpacked.len() as u64)
            .ok_or_else(|| Error::Corrupt(format!("{key}: part offset overflow")))?;

        while block_idx < footer.blocks.len() && remaining > 0 {
            let blk = &footer.blocks[block_idx];
            let blk_len = usize::try_from(blk.len)
                .map_err(|_| Error::Corrupt(format!("{key}: block length overflow")))?;
            let blk_end = blk
                .offset
                .checked_add(u64::from(blk.len))
                .ok_or_else(|| Error::Corrupt(format!("{key}: block end overflow")))?;
            if blk.offset >= part_end {
                break;
            }
            if carry.is_empty() && blk.offset >= part_offset && blk_end <= part_end {
                let start = (blk.offset - part_offset) as usize;
                let end = (blk_end - part_offset) as usize;
                footer.visit_block(&unpacked[start..end], blk, |id, kidx| {
                    if !shadowed.contains(&id) && !alive[kidx] {
                        alive[kidx] = true;
                        remaining -= 1;
                    }
                })?;
                block_idx += 1;
                continue;
            }
            let need_from = blk
                .offset
                .checked_add(carry.len() as u64)
                .ok_or_else(|| Error::Corrupt(format!("{key}: carry offset overflow")))?;
            if need_from < part_offset || need_from > part_end {
                return Err(Error::Corrupt(format!("{key}: block {block_idx} misaligned")));
            }
            let take_start = (need_from - part_offset) as usize;
            let take_end = (blk_end.min(part_end) - part_offset) as usize;
            carry.extend_from_slice(&unpacked[take_start..take_end]);
            if carry.len() < blk_len {
                break;
            }
            footer.visit_block(&carry, blk, |id, kidx| {
                if !shadowed.contains(&id) && !alive[kidx] {
                    alive[kidx] = true;
                    remaining -= 1;
                }
            })?;
            carry.clear();
            block_idx += 1;
        }
        part_offset = part_end;
    }

    if remaining > 0 && block_idx < footer.blocks.len() {
        return Err(Error::Corrupt(format!("{key}: truncated checkpoint blocks")));
    }
    let kept = footer
        .keys
        .iter()
        .enumerate()
        .filter_map(|(idx, k)| alive[idx].then(|| k.clone()))
        .collect();
    Ok(Some(kept))
}

/// cladefs: keys-only recovery of `cell`, forked from `origin`, as the collector needs it
/// ([`RecoveredKeys`]): as [`recover_layers`] recovers it, from its head, starting again where it
/// finds what its head names gone and its head changed; reading its base checkpoint's tail alone,
/// and its parts, a ranged read each, only if a segment since frees or seals again an extent it
/// names ([`checkpoint_live_keys`]).
pub(crate) async fn recover_keys<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    origin: Option<Origin>,
) -> Result<RecoveredKeys, Error> {
    for _ in 0..RESTARTS {
        let head = head::get(checkpoints.bucket, cell).await?;
        match attempt_keys(checkpoints, cell, origin, head).await {
            Ok(recovered) => return Ok(recovered),
            Err(Attempt::Restart) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Lost(format!("cell {cell}: what its recovery starts from keeps going")))
}

/// [`recover_keys`]'s attempt from `head`, as [`attempt`] is [`recover_layers`]'s: its base the
/// checkpoint the head names, or the one of its origin's parent at the origin, read by its key
/// (board 6017); then the segments the head names after it.
async fn attempt_keys<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    origin: Option<Origin>,
    head: Option<Head>,
) -> Result<RecoveredKeys, Attempt> {
    let bucket = checkpoints.bucket;
    let mut touched = BTreeSet::new();
    let mut base: Option<(String, object::Tail, TableFooter)> = None;
    let mut from = 0u64;
    let mut replay_state = State::of(cell, KeyReplay::new(false));

    match (head.as_ref().and_then(|head| head.checkpoint(cell)), origin) {
        (Some(key), _) => {
            let Some((tail, footer, cp_state)) = checkpoint_tail(bucket, &key, cell).await? else {
                return Err(moved(bucket, cell, head.as_ref()).await);
            };
            touched.insert(key.clone());
            let Some(through) = cp_state.through else {
                return Err(Attempt::Failed(Error::Corrupt(key)));
            };
            from = through + 1;
            replay_state = State {
                begun: cp_state.begun,
                through: Some(through),
                ..State::of(cell, KeyReplay::new(true))
            };
            base = Some((key, tail, footer));
        }
        (None, None) => {}
        (None, Some(origin)) if origin.parent == cell => return Err(own_origin(cell)),
        (None, Some(origin)) => {
            let key = layout::checkpoint(origin.parent, origin.at, origin.epoch);
            match checkpoint_tail(bucket, &key, origin.parent).await? {
                Some((tail, footer, cp_state)) if cp_state.through == Some(origin.at) => {
                    touched.insert(key.clone());
                    replay_state = State::of(cell, KeyReplay::new(true));
                    base = Some((key, tail, footer));
                }
                Some(_) => {
                    return Err(Attempt::Failed(Error::Corrupt(format!(
                        "{key} holds another state"
                    ))));
                }
                None => {
                    return Err(match moved(bucket, cell, head.as_ref()).await {
                        Attempt::Restart => Attempt::Restart,
                        Attempt::Failed(_) => Attempt::Failed(Error::Lost(key)),
                    });
                }
            }
        }
    }

    if let Some(named_head) = head.as_ref() {
        let mut footers = futures::stream::iter(named_head.segments(from))
            .map(|(segment, epoch)| async move {
                let key = layout::segment(cell, segment, epoch);
                let footer =
                    segment_footer((bucket, checkpoints.keys), cell, segment, epoch).await?;
                Ok::<_, Error>((key, footer))
            })
            .buffered(AT_ONCE);
        while let Some((key, footer)) = footers.try_next().await? {
            let Some((footer, len)) = footer else {
                drop(footers);
                return Err(moved(bucket, cell, head.as_ref()).await);
            };
            replay_state.apply(&footer, len);
            touched.insert(key);
        }
    }

    let KeyReplay { own, shadowed, .. } = replay_state.extents;
    // One at a time: collecting, or extending an empty set, reserves room for every extent, where
    // the set holds their objects' keys, far fewer (hotbed review 184).
    let mut named: FxHashSet<String> = FxHashSet::default();
    for key in own.into_values() {
        named.insert(key);
    }
    if let Some((cp_key, cp_tail, cp_footer)) = base {
        let Some(kept) =
            checkpoint_live_keys(bucket, &cp_key, &cp_tail, &cp_footer, &shadowed).await?
        else {
            return Err(moved(bucket, cell, head.as_ref()).await);
        };
        named.extend(kept);
    }
    Ok(RecoveredKeys { head, touched, named })
}

/// cladefs: keys-only recovery of the states of `cell` as of each of its segments `ats`, which
/// its head names, a pass keeps (its versions', the origins its references name): the keys of the
/// objects holding the extents any of them names, read in one walk where they share a checkpoint,
/// and what reading them read, as [`recover_at`] has each. [`Error::Lost`] if any is.
pub(crate) async fn keys_at<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    ats: &BTreeSet<u64>,
) -> Result<(FxHashSet<String>, BTreeSet<String>), Error> {
    retry_recovery!(
        |touched| attempt_keys_at(checkpoints, cell, ats, touched),
        format!("cell {cell} at {ats:x?}: what its recovery starts from keeps going")
    )
}

/// [`keys_at`]'s attempt: the checkpoint at each of `ats`, by its exact key, its tail alone; the
/// rest walked from the newest checkpoint up to them ([`walk_keys`]).
async fn attempt_keys_at<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    ats: &BTreeSet<u64>,
    touched: &mut BTreeSet<String>,
) -> Result<FxHashSet<String>, Attempt> {
    let bucket = checkpoints.bucket;
    let mut named = FxHashSet::default();
    let Some(head) = head::get(bucket, cell).await? else {
        return Err(Attempt::Failed(Error::Lost(layout::head(cell))));
    };

    let mut keys = Vec::with_capacity(ats.len());
    for &at in ats {
        let Some(epoch) = head.epoch_of(at) else {
            return Err(Attempt::Failed(Error::Lost(format!(
                "segment {at} of {cell}",
                at = base32_id::Short(at)
            ))));
        };
        keys.push((at, layout::checkpoint(cell, at, epoch)));
    }
    let mut reads = futures::stream::iter(keys)
        .map(|(at, key)| async move {
            let tail = checkpoint_tail(bucket, &key, cell).await;
            (at, key, tail)
        })
        .buffered(AT_ONCE);
    let mut unread = Vec::new();
    while let Some((at, key, tail)) = reads.next().await {
        match tail? {
            Some((_, _, cp_state)) if cp_state.through != Some(at) => {
                return Err(Attempt::Failed(Error::Corrupt(format!("{key} holds another state"))));
            }
            Some((_, cp_footer, _)) => {
                touched.insert(key);
                named.extend(cp_footer.keys);
            }
            None => unread.push(at),
        }
    }

    let Some(&first) = unread.first() else { return Ok(named) };
    let listed = checkpoints.list(cell, Some(first)).await?;
    // The states sharing their newest checkpoint up to them, in order: the newest only grows.
    let mut walks: Vec<(Option<&Meta>, Vec<u64>)> = Vec::new();
    for at in unread {
        let newest = newest_to(&listed, at);
        match walks.last_mut() {
            Some((start, ats)) if start.map(|meta| &meta.key) == newest.map(|meta| &meta.key) => {
                ats.push(at);
            }
            _ => walks.push((newest, vec![at])),
        }
    }
    for (start, group_ats) in walks {
        walk_keys(checkpoints, cell, &head, start, &group_ats, touched, &mut named).await?;
    }
    Ok(named)
}

/// cladefs: the keys of the objects holding the extents the states of `cell` as of its segments
/// `ats`, ascending, name, into `named`: from `start`, the newest checkpoint up to each, or, with
/// none, its origin's parent's checkpoint at the origin; then the segments `head` names up to the
/// last, each footer read once ([`attempt_keys_at`]).
async fn walk_keys<B: Bucket>(
    checkpoints: &Checkpoints<'_, B>,
    cell: CellId,
    head: &Head,
    start: Option<&Meta>,
    ats: &[u64],
    touched: &mut BTreeSet<String>,
    named: &mut FxHashSet<String>,
) -> Result<(), Attempt> {
    let bucket = checkpoints.bucket;
    let (Some(&first), Some(&last)) = (ats.first(), ats.last()) else { return Ok(()) };
    let mut ats_iter = ats.iter().copied().peekable();
    let mut base: Option<(String, object::Tail, TableFooter)> = None;
    let mut from = 0u64;
    let mut replay_state = State::of(cell, KeyReplay::new(false));

    match start {
        Some(meta) => {
            let Some((tail, footer, cp_state)) = checkpoint_tail(bucket, &meta.key, cell).await?
            else {
                return Err(Attempt::Restart);
            };
            touched.insert(meta.key.clone());
            if cp_state.through == Some(first) {
                named.extend(footer.keys);
                ats_iter.next();
                if ats_iter.peek().is_none() {
                    return Ok(());
                }
                from = first + 1;
                replay_state = State {
                    begun: cp_state.begun,
                    through: Some(first),
                    ..State::of(cell, KeyReplay::new(false))
                };
            } else {
                from = cp_state.through.map_or(0, |s| s + 1);
                replay_state = State {
                    begun: cp_state.begun,
                    through: cp_state.through,
                    ..State::of(cell, KeyReplay::new(true))
                };
                base = Some((meta.key.clone(), tail, footer));
            }
        }
        None => {
            touched.insert(layout::head(cell));
            match head.origin() {
                None => {}
                Some(origin) if origin.parent == cell => return Err(own_origin(cell)),
                Some(origin) => {
                    let key = layout::checkpoint(origin.parent, origin.at, origin.epoch);
                    match checkpoint_tail(bucket, &key, origin.parent).await? {
                        Some((tail, footer, cp_state)) if cp_state.through == Some(origin.at) => {
                            touched.insert(key.clone());
                            replay_state = State::of(cell, KeyReplay::new(true));
                            base = Some((key, tail, footer));
                        }
                        Some(_) => {
                            return Err(Attempt::Failed(Error::Corrupt(format!(
                                "{key} holds another state"
                            ))));
                        }
                        None => {
                            let lost = Attempt::Failed(Error::Lost(key));
                            return Err(unless_checkpointed(bucket, cell, first, lost).await);
                        }
                    }
                }
            }
        }
    }

    let mut footers =
        futures::stream::iter(head.segments(from).take_while(|&(segment, _)| segment <= last))
            .map(|(segment, epoch)| async move {
                let key = layout::segment(cell, segment, epoch);
                Ok::<_, Error>((
                    key,
                    segment_footer((bucket, checkpoints.keys), cell, segment, epoch).await?,
                ))
            })
            .buffered(AT_ONCE);

    while let Some((key, footer)) = footers.try_next().await? {
        let Some((footer, len)) = footer else { return Err(Attempt::Restart) };
        replay_state.apply(&footer, len);
        touched.insert(key);
        if ats_iter.next_if_eq(&footer.segment).is_some() {
            name_all(named, replay_state.extents.own.values());
            // Any free/overwrite strictly after `first` cannot hide an extent still live at `first`.
            replay_state.extents.track_shadowed = false;
        }
    }
    drop(footers);
    if ats_iter.peek().is_some() {
        name_all(named, replay_state.extents.own.values());
    }
    if let Some((cp_key, cp_tail, cp_footer)) = base {
        let shadowed = &replay_state.extents.shadowed;
        let Some(kept) =
            checkpoint_live_keys(bucket, &cp_key, &cp_tail, &cp_footer, shadowed).await?
        else {
            return Err(Attempt::Restart);
        };
        named.extend(kept);
    }
    Ok(())
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;
    use crate::batch::{Commit, SegmentBuilder};
    use crate::layout::test_cell;
    use crate::layout::{Continued, Segment};
    use crate::lifecycle;
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
        let cell = test_cell(1);
        // The base's extents, every other sequence number, so that new ones go between; each its
        // segment's key, 64 extents a segment, a string of its own.
        let extent = |sequence: u64| ExtentId::new(cell.id, sequence);
        let at = |sequence: u64, epoch: u64| Location {
            key: crate::layout::segment(cell, sequence / 128 + 1, epoch),
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

    /// A segment's object holds the segment its key names: of its cell, of its number, by the
    /// writer of its epoch.
    #[test]
    fn segments_are_where_their_keys_say() {
        let footer =
            Footer { cell: test_cell(1), segment: 2, epoch: 1, contents: Contents::default() };
        assert_eq!(placed("here", footer.clone(), test_cell(1), 2, 1), Ok(footer.clone()));
        for (cell, segment, epoch) in [(1, 3, 1), (4, 2, 1), (1, 2, 2)] {
            let corrupt = Error::Corrupt("here holds segment 2 of 1/1 by epoch 1".into());
            let found = placed("here", footer.clone(), test_cell(cell), segment, epoch);
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

    /// TASK-92.6, TASK-407: recovery reads its cell's head, then the footers of the segments it
    /// names by their objects' tails, all at once, every key known. Recovered on, it reads only
    /// the segments the head names since, finding what recovering anew finds.
    #[test]
    fn recoveries_read_the_segments_their_heads_name_at_once() {
        let bucket = SimBucket::new();
        let (writer, _) = bucket.served(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap()
        });
        committed(&bucket, &writer, 1, 5);
        let tails = |segments: std::ops::Range<u64>| -> Vec<String> {
            segments
                .map(|segment| {
                    format!(
                        "get {} last {}",
                        layout::segment(test_cell(1), segment, 1),
                        object::TAIL
                    )
                })
                .collect()
        };
        let head = vec![format!("get {}", layout::head(test_cell(1)))];
        let (recovered, rounds) = bucket.served_in_rounds(recover(&*bucket, test_cell(1), None));
        let recovered = recovered.unwrap();
        assert_eq!((recovered.end(), recovered.state.last_commit), (5, Some(5)));
        assert_eq!(rounds, [head.clone(), tails(0..5)]);
        let keys: Vec<_> =
            (0..5).map(|segment| layout::segment(test_cell(1), segment, 1)).collect();
        assert_eq!(recovered.touched.iter().cloned().collect::<Vec<_>>(), keys);

        committed(&bucket, &writer, 6, 2);
        let now = bucket.served(head::get(&*bucket, test_cell(1))).0.unwrap();
        let (on, rounds) =
            bucket.served_in_rounds(recover_on(&*bucket, None, recovered, now, &Ring::keyless()));
        let on = on.unwrap();
        assert_eq!(rounds, [tails(5..7)]);
        let anew = bucket.served(recover(&*bucket, test_cell(1), None)).0.unwrap();
        assert_eq!((on.end(), on.state, on.touched), (anew.end(), anew.state, anew.touched));
    }

    /// TASK-92.6: a recovery recovered on once its head names a checkpoint as of where it would
    /// follow on from, or later, recovers anew, from it: the segments it covers may be gone.
    #[test]
    fn recoveries_recovered_on_past_a_newer_checkpoint_recover_anew() {
        let bucket = SimBucket::new();
        let (writer, _) = bucket.served(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap()
        });
        committed(&bucket, &writer, 1, 2);
        let recovered = bucket.served(recover(&*bucket, test_cell(1), None)).0.unwrap();
        committed(&bucket, &writer, 3, 1);
        let segment = bucket.served(writer.checkpoint()).0.unwrap().unwrap();
        committed(&bucket, &writer, 4, 1);
        let now = bucket.served(head::get(&*bucket, test_cell(1))).0.unwrap();
        let (on, rounds) =
            bucket.served_in_rounds(recover_on(&*bucket, None, recovered, now, &Ring::keyless()));
        let on = on.unwrap();
        assert_eq!(
            (on.checkpoint(), on.end(), on.state.last_commit),
            (Some((segment, 1)), 4, Some(4))
        );
        let checkpoint = vec![format!("get {}", layout::checkpoint(test_cell(1), segment, 1))];
        let tail = |segment| {
            format!("get {} last {}", layout::segment(test_cell(1), segment, 1), object::TAIL)
        };
        assert_eq!(rounds, [checkpoint, vec![tail(3)]]);
    }

    /// cladefs: a commit spanning segments (TASK-92.41): the runs each segment going on with it
    /// names are sealed, where they are, once the segment ending it is applied, and not before; its
    /// writer acknowledges none of its segments until then, and no head names them. A writer taking
    /// the cell with a commit begun, its segments landed, finds none of it: the head names whole
    /// commits alone, the next writer's state and recovery's agreeing.
    #[test]
    fn commits_spanning_segments_seal_once_ended() {
        futures::executor::block_on(async {
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let extent = |sequence| ExtentId::new(1, sequence);
            let tag = |epoch| layout::tag(writer.epoch(), epoch);
            let landed = async |writer: &Writer<ObjectBucket>, contents: Contents| {
                let footer = writer.chain().close(contents);
                let data = vec![7; 8192];
                let framed = Segment { data, footer: footer.clone() }
                    .encode(writer.keys().encrypting())
                    .into();
                let segment = writer
                    .add_segment(Objects {
                        footer,
                        segment: framed,
                        own: Vec::new(),
                        data_len: 8192,
                    })
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
            let begun = recover(&*bucket, test_cell(1), None).await.unwrap().state;
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
                key: layout::segment(test_cell(1), segment, 1),
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
            let recovered = recover(&*bucket, test_cell(1), None).await.unwrap().state;
            assert_eq!(recovered.extents.into_iter().collect::<Vec<_>>(), expected);
            assert_eq!(recovered.last_commit, Some(tag(1)));

            // Begun, its writer gone: the next finds none of it.
            let (begun, _) = landed(&writer, continuing(tag(2), 4)).await;
            let next = Writer::open(bucket.clone(), test_cell(1), 11, 0).await.unwrap();
            assert_eq!(next.state().through, Some(ends), "the head's end, before it");
            assert_eq!(next.taken().chain.next_segment(), begun, "where its own go");
            assert_eq!(next.state().extents.into_iter().collect::<Vec<_>>(), expected);
            assert_eq!((next.state().begun, next.state().last_commit), (None, Some(tag(1))));
            let recovered = recover(&*bucket, test_cell(1), None).await.unwrap().state;
            assert_eq!(recovered, next.state());
        });
    }

    /// TASK-165.3: a cell's capacity is its state, set by its commits, recovered from its segments
    /// and its checkpoints wherever it is taken; a fork's child has its parent's at the fork, until
    /// it sets its own, and a version keeps it as it was, whatever the parent sets after.
    #[test]
    fn capacities_are_set_by_commits() {
        futures::executor::block_on(async {
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
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
            let forked =
                resize(1, 1 << 20).close(&mut parent.chain()).frame(parent.keys().encrypting());
            let forked = Origin {
                parent: test_cell(1),
                at: landed(&parent, forked).await,
                epoch: parent.epoch(),
            };
            crate::testing::forked_at(&*bucket, test_cell(2), forked).await;
            let pointed = SegmentBuilder::default()
                .close(&mut parent.chain())
                .frame(parent.keys().encrypting());
            let pointed = Origin {
                parent: test_cell(1),
                at: landed(&parent, pointed).await,
                epoch: parent.epoch(),
            };
            crate::testing::forked_at(&*bucket, test_cell(3), pointed).await;
            landed(
                &parent,
                resize(2, 2 << 20).close(&mut parent.chain()).frame(parent.keys().encrypting()),
            )
            .await;
            let capacity = async |cell, origin| {
                let recovered = recover(&*bucket, cell, origin).await.unwrap();
                (recovered.checkpoint().is_some(), recovered.state.capacity)
            };
            assert_eq!(parent.state().capacity, Some(2 << 20));
            assert_eq!(capacity(test_cell(1), None).await, (false, Some(2 << 20)));
            assert_eq!(capacity(test_cell(2), Some(forked)).await, (false, Some(1 << 20)));
            assert_eq!(capacity(test_cell(3), Some(pointed)).await, (false, Some(1 << 20)));
            parent.checkpoint().await.unwrap();
            assert_eq!(capacity(test_cell(1), None).await, (true, Some(2 << 20)));

            let child = Writer::open(bucket.clone(), test_cell(2), 11, 0).await.unwrap();
            assert_eq!(child.state().capacity, Some(1 << 20));
            landed(
                &child,
                resize(3, 4 << 20).close(&mut child.chain()).frame(child.keys().encrypting()),
            )
            .await;
            child.checkpoint().await.unwrap();
            assert_eq!(capacity(test_cell(2), Some(forked)).await, (true, Some(4 << 20)));
            assert_eq!(capacity(test_cell(1), None).await, (true, Some(2 << 20)));
            assert_eq!(capacity(test_cell(3), Some(pointed)).await, (false, Some(1 << 20)));
        });
    }

    /// A fork's checkpoints hold its own state, from which it recovers; another cell's
    /// checkpoint in its place is corrupt.
    #[test]
    fn forks_checkpoint_their_own_states() {
        futures::executor::block_on(async {
            let memory = Arc::new(InMemory::new());
            let bucket = Arc::new(ObjectBucket::new(memory.clone(), ""));
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let extent = ExtentId::new(1, parent.chain().next_sequence());
            let extents = vec![(extent, b"one".to_vec())];
            let batch = SegmentBuilder::from(Commit { tag: 1, extents, ..Commit::default() });
            let at = parent
                .add_segment(batch.close(&mut parent.chain()).frame(parent.keys().encrypting()))
                .unwrap();
            parent.upload(at).await.unwrap();
            parent.acknowledge().await.unwrap();
            let origin = Origin { parent: test_cell(1), at, epoch: parent.epoch() };
            crate::testing::forked_at(&*bucket, test_cell(2), origin).await;
            let origin = Some(origin);
            assert_eq!(
                crate::head::get(&*bucket, test_cell(2)).await.unwrap().unwrap().origin(),
                origin
            );

            // The fork's writer, a commit of its own, then its checkpoint: none before, of no
            // segment of its own.
            let child = Writer::open(bucket.clone(), test_cell(2), 11, 0).await.unwrap();
            assert_eq!(child.checkpoint().await, Ok(None));
            let commit = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
            let segment = child
                .add_segment(commit.close(&mut child.chain()).frame(child.keys().encrypting()))
                .unwrap();
            child.upload(segment).await.unwrap();
            child.acknowledge().await.unwrap();
            assert_eq!(child.checkpoint().await, Ok(Some(segment)));
            let recovered = recover(&*bucket, test_cell(2), origin).await.unwrap();
            assert_eq!(recovered.checkpoint(), Some((segment, child.epoch())));
            assert_eq!(recovered.state.cell, test_cell(2));
            assert_eq!(recovered.state.extents.keys().collect::<Vec<_>>(), [&extent]);

            // The parent's checkpoint, at the key the fork's head names, as a bucket corrupting it.
            let parents = parent.checkpoint().await.unwrap().unwrap();
            let object = bucket::get(&*bucket, &layout::checkpoint(test_cell(1), parents, 1))
                .await
                .unwrap()
                .unwrap();
            let key = layout::checkpoint(test_cell(2), segment, child.epoch());
            let overwrite = crate::bucket::PutMode::Overwrite;
            crate::bucket::Bucket::put(&*bucket, &key, object.bytes.into(), overwrite)
                .await
                .unwrap();
            // Read by another host, which decoded nothing of it before: a key names one object,
            // which a host's bucket keeps decoded (TASK-407).
            let corrupt = format!("{key} holds a checkpoint of 1/1");
            let other = ObjectBucket::new(memory, "");
            assert_eq!(
                recover(&other, test_cell(2), origin).await.unwrap_err(),
                Error::Corrupt(corrupt)
            );
        });
    }

    /// board 8543: a version recovered as a copy holds it, its cell's head gone (removed, or being
    /// removed, its checkpoints listed still) and a checkpoint older than the version's segment
    /// standing, is lost: never a walk from that checkpoint on with no head to name the segments,
    /// which panicked.
    #[test]
    fn versions_of_cells_whose_head_is_gone_are_lost() {
        futures::executor::block_on(async {
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            let cell = test_cell(1);
            lifecycle::create(&*bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
            let extent = || ExtentId::new(cell.id, writer.chain().next_sequence());
            let first = crate::testing::versioned_by(&writer, vec![(extent(), vec![1])], 1).await;
            assert_eq!(writer.checkpoint().await, Ok(Some(first)));
            let at = crate::testing::versioned_by(&writer, vec![(extent(), vec![2])], 2).await;
            assert!(at > first);
            let ring = writer.keys().clone();
            recover_version(&*bucket, cell, at, &ring).await.unwrap();
            bucket.delete(&layout::head(cell)).await.unwrap();
            let lost = recover_version(&*bucket, cell, at, &ring).await.map(|_| ());
            assert_eq!(lost, Err(Error::Lost(layout::head(cell))));
        });
    }

    /// Board 11554: `Decoded` bounds its strong idle cache by bytes and supports governor eviction,
    /// while still sharing any state held by an open caller through its weak `live` table and
    /// freeing oversized states immediately once their last caller drops.
    #[test]
    fn decoded_bounds_idle_states_by_bytes_and_evicts() {
        let make = |cell: u64, count: u64| {
            let mut state = State::new(test_cell(cell));
            for seq in 1..=count {
                state.extents.insert(
                    ExtentId::new(cell, seq),
                    Location { key: format!("seg/{cell}/{seq}"), offset: 0, len: 4096 },
                );
            }
            state
        };
        // Budget fits 4 extents (4 * ENTRY_BYTES), so two 3-extent states exceed it.
        let decoded = Decoded::with_max_bytes(4 * ENTRY_BYTES);
        let first = decoded.keep("cp/1", make(1, 3));
        assert_eq!(decoded.kept_bytes(), 3 * ENTRY_BYTES);
        let second = decoded.keep("cp/2", make(2, 3));
        // `cp/1` evicted from strong cache to stay within 4 * ENTRY_BYTES, but still shared while
        // `first` is held.
        assert_eq!(decoded.kept_bytes(), 3 * ENTRY_BYTES);
        assert!(Arc::ptr_eq(&decoded.get("cp/1").unwrap(), &first));
        assert!(Arc::ptr_eq(&decoded.get("cp/2").unwrap(), &second));
        drop(first);
        assert!(decoded.get("cp/1").is_none());
        // Oversized state (6 extents > 4-extent budget) is shared while held, then gone on drop.
        // A small fork rebased onto it while it is live must not stay pinned in `states` either,
        // or its persistent map would retain the oversized base after both close.
        let huge = decoded.keep("cp/huge", make(3, 6));
        assert_eq!(decoded.kept_bytes(), 0);
        assert!(Arc::ptr_eq(&decoded.get("cp/huge").unwrap(), &huge));
        let mut fork_state = make(3, 6);
        fork_state.extents.insert(
            ExtentId::new(30, 1),
            Location { key: "seg/30/1".into(), offset: 0, len: 4096 },
        );
        let fork = decoded.keep("cp/huge-fork", fork_state);
        assert_eq!(decoded.kept_bytes(), 0);
        assert!(Arc::ptr_eq(&decoded.get("cp/huge-fork").unwrap(), &fork));
        drop(huge);
        drop(fork);
        assert!(decoded.get("cp/huge").is_none());
        assert!(decoded.get("cp/huge-fork").is_none());
        // Governor eviction drops remaining strong idle entries down to target.
        drop(second);
        let _warm = decoded.keep("cp/warm", make(4, 2));
        drop(_warm);
        assert_eq!(decoded.kept_bytes(), 2 * ENTRY_BYTES);
        assert_eq!(decoded.evict_to(0), 2 * ENTRY_BYTES);
        assert_eq!(decoded.kept_bytes(), 0);
        assert!(decoded.get("cp/warm").is_none());
    }
}
