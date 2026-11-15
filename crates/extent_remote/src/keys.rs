// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fxdax: the keys a cell's branches are kept under in the bucket (doc-16, Keys; TASK-411.3). A
//! tenant's KEK, in a KMS, wraps each of its cells' KEKs, which the cell's object keeps
//! ([`Kek`], [`crate::cell::Object::keks`]); a cell's KEK wraps each of its branches' DEKs, which
//! the branch's record keeps ([`Dek`], [`crate::record::Record::deks`]). A cell whose object keeps
//! no KEK is in the clear, and so are its branches' objects, their records keeping no DEK.
//!
//! **Taking a cell** unwraps its KEK with one call of the KMS, which a host's [`Kms`] keeps for
//! that taking, whatever number of the cell's volumes it opens: its branches' DEKs are unwrapped
//! with it locally ([`CellKeys`]). A cell made by a host gets a KEK the host wrapped before, which
//! it keeps in a pool for the cells it makes: none is unwrapped then. A cell is made encrypted iff
//! the host has a KMS and the cell's tenant has a KEK there ([`Kms::new_kek`]); a tenant with
//! none has cells in the clear, and any other refusal of the KMS fails the making, naming the
//! tenant (a revoked one makes no cell, in the clear least of all). That a tenant has none is
//! believed for [`KEKLESS_FOR`], asking nothing more meanwhile (TASK-411.10).
//!
//! **Opening a store** gives every writer and reader of it a [`Ring`]: its branch's DEKs, every
//! generation of them, and its ancestors', unwrapped under the cell's KEK ([`ring`]); and which of
//! those branches are sealed, whose objects in the clear its readers refuse ([`Ring::of`],
//! `extent_frame::Keys::sealed`): a downgrade. A host's fetchers, which share what they keep for
//! all its stores, open parts with the union of its stores' rings ([`HostRing`]).
//!
//! **Forks into other cells** (doc-16, Forks and shared points; TASK-411.4). A branch's record
//! keeps, beside its own DEKs, the DEKs of every ancestor of another cell whose objects it reads,
//! each generation, rewrapped under its own cell's KEK ([`Inherited`],
//! [`crate::record::Record::inherited`]), given with its first DEK, in one conditional update: a
//! record with DEKs holds all it inherits from other cells, so that its readers need no other
//! cell's KEK, nor another tenant's KMS, and shredding a parent leaves it what it inherited. A
//! fork's host gives them as it makes the fork, from the keys its parent's store holds, its
//! child's cell made with a KEK of its pool ([`inheriting`], [`give_inherited`]): no KMS call on
//! the fork's path. A record without them yet (a fork's child before its keys, a point, a branch
//! of a cell in the clear) is read by crossing into its ancestors' cells, their KEKs unwrapped by
//! the KMS, which its first writer then rewraps ([`with_dek`]); a writer of a cell in the clear
//! inherits nothing sealed ([`Error::Inherited`]): no sealed data goes into the clear.
//!
//! **Pools.** A host wraps new cell KEKs ahead, a pool a tenant, refilled in batches on its
//! runtime as a take leaves one low, off the fork's path ([`Pooling`]). A tenant whose KEK the KMS
//! says is revoked or destroyed, or the control API does ([`Kms::revoked`]), has its pool dropped,
//! and no fork inherits its keys, until the KMS answers for it again.
//!
//! **Generations.** A record's DEKs are a list of generations, each wrapped by a generation of its
//! cell's KEK, which it names. New objects are sealed under the newest ([`Ring::sealing`]); the
//! others stay readable. A generation is added by a conditional update of the record
//! ([`add_generation`]); rotating a cell's KEK adds a generation of it to the cell's object, then
//! rewraps every record's DEKs under it, each by a conditional update ([`rotate`]). Older
//! generations of the cell's KEK stay in its object: a writer that read the cell before a rotation
//! may wrap a new branch's DEK under the generation it read, after the rotation listed its
//! records; dropping them needs that fenced first.
//!
//! **Shredding** (doc-16, Shredding; TASK-411.7). A tenant is shredded by destroying its KEK in the
//! KMS, a cell by taking its wrapped KEKs out of its object ([`crate::cell::shred`]): a cell
//! shredded is sealed and refuses its KEKs, naming itself. A host told either forgets what it
//! holds of it ([`Host::forget`], a [`Scope`]): its KMS's unwrapped cell KEKs and pool, and its
//! fetchers' keys of the scope's stores; DEKs other tenants' stores hold, which they inherited,
//! stay theirs.
//!
//! A DEK is wrapped with AES-KW under a key HKDF derives from the cell's KEK and the DEK's place:
//! its tenant, cell, branch and generation. So a wrapped DEK copied to another branch's record, or
//! relabelled another generation, fails AES-KW's check.

use crate::bucket::{self, Bucket};
use crate::cell::Object;
use crate::layout::{self, Branch, Cell, Kind as Key};
use crate::record::Record;
use crate::{Error, branch};
use extent_frame::{KeyId, Keys};
use extent_keys::{SALT_LEN, Salt, Secret, Wrapped, kms};
use futures::future::{BoxFuture, FutureExt as _, Shared};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// A generation of a cell's KEK, wrapped by its tenant's KEK in the KMS: what the cell's object
/// keeps of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kek {
    pub generation: u32,
    pub wrapped: Wrapped,
}

/// A generation of a branch's DEK, wrapped by its cell's KEK of generation `kek`: what its record
/// keeps of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dek {
    pub generation: u32,
    pub kek: u32,
    pub wrapped: Wrapped,
}

/// The first generation of a key.
pub const FIRST: u32 = 1;

/// What HKDF derives the key a DEK is wrapped under with, before its place.
const DEK_INFO: &[u8] = b"fxdax dek wrap";

/// The key `kek`, a generation of `branch`'s cell's KEK, wraps `branch`'s DEK of `generation`
/// under: bound to its place.
fn wrapping_key(kek: &Secret, branch: Branch, generation: u32) -> Secret {
    let place = [
        &branch.cell.tenant.to_le_bytes()[..],
        &branch.cell.id.to_le_bytes(),
        &branch.id.to_le_bytes(),
        &generation.to_le_bytes(),
    ]
    .concat();
    kek.derive(&Salt([0; SALT_LEN]), &[DEK_INFO, &place])
}

/// `dek`, `branch`'s of `generation`, wrapped under `kek`, generation `kek_generation` of its
/// cell's KEK.
pub fn wrap_dek(
    (kek, kek_generation): (&Secret, u32),
    branch: Branch,
    generation: u32,
    dek: &Secret,
) -> Dek {
    let wrapped = wrapping_key(kek, branch, generation).wrap(dek);
    Dek { generation, kek: kek_generation, wrapped }
}

/// The DEK `dek` of `branch` holds, unwrapped under `kek`, the generation of its cell's KEK it
/// names.
pub fn unwrap_dek(kek: &Secret, branch: Branch, dek: &Dek) -> Result<Secret, Error> {
    (wrapping_key(kek, branch, dek.generation).unwrap(&dek.wrapped)).map_err(|err| {
        Error::Corrupt(format!("branch {branch}'s DEK of generation {}: {err}", dek.generation))
    })
}

/// A generation of the DEK of `branch`, an ancestor of another cell, as the record of a branch
/// that inherits from it keeps it: wrapped by that record's own cell's KEK of generation
/// `dek.kek` (TASK-411.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inherited {
    pub branch: Branch,
    pub dek: Dek,
}

/// What HKDF derives the key an inherited DEK is wrapped under with, before its places.
const INHERITED_INFO: &[u8] = b"fxdax inherited dek wrap";

/// The key `kek`, a generation of `holder`'s cell's KEK, wraps the DEK of `of`, of `generation`,
/// that `holder`'s record keeps under: bound to both places, so that it is copied to no other
/// record, nor relabelled.
fn inherited_key(kek: &Secret, holder: Branch, of: Branch, generation: u32) -> Secret {
    let place = |branch: Branch| {
        [branch.cell.tenant.to_le_bytes(), branch.cell.id.to_le_bytes(), branch.id.to_le_bytes()]
            .concat()
    };
    let places = [place(holder), place(of), generation.to_le_bytes().to_vec()].concat();
    kek.derive(&Salt([0; SALT_LEN]), &[INHERITED_INFO, &places])
}

/// `dek`, `of`'s of `generation`, as `holder`'s record keeps it, wrapped under `kek`, generation
/// `kek_generation` of `holder`'s cell's KEK.
pub fn wrap_inherited(
    (kek, kek_generation): (&Secret, u32),
    holder: Branch,
    of: Branch,
    generation: u32,
    dek: &Secret,
) -> Inherited {
    let wrapped = inherited_key(kek, holder, of, generation).wrap(dek);
    Inherited { branch: of, dek: Dek { generation, kek: kek_generation, wrapped } }
}

/// The DEK `inherited`, of `holder`'s record, unwrapped under `kek`, the generation of `holder`'s
/// cell's KEK it names.
pub fn unwrap_inherited(
    kek: &Secret,
    holder: Branch,
    inherited: &Inherited,
) -> Result<Secret, Error> {
    let Inherited { branch, dek } = inherited;
    let key = inherited_key(kek, holder, *branch, dek.generation);
    key.unwrap(&dek.wrapped).map_err(|err| {
        Error::Corrupt(format!(
            "branch {holder}'s DEK of branch {branch}, generation {}: {err}",
            dek.generation
        ))
    })
}

/// What a reader of a store's objects holds: the DEKs of its branch and its ancestors, by which
/// they are, and which of those branches are sealed, whose objects in the clear it refuses; and the
/// key its writer seals new objects under, its branch's newest, if it writes. Empty: a reader of
/// branches in the clear, or of metadata alone, as the collector's marking is ([`compacting`]
/// for its compaction's).
#[derive(Debug, Default)]
pub struct Ring {
    deks: HashMap<KeyId, Arc<Secret>>,
    /// The branch, cell and all, of each ID its DEKs are of: what rewrapping them for a branch of
    /// another cell binds them to ([`inherit`]).
    places: HashMap<u64, Branch>,
    sealed: BTreeSet<u64>,
    sealing: Option<KeyId>,
    /// The branch whose store it was opened for ([`ring`]): what a host forgetting a tenant's or a
    /// cell's keys goes by ([`HostRing::forget`], TASK-411.7). None for others'.
    owner: Option<Branch>,
}

/// The empty ring ([`Ring::clear`]).
static CLEAR: LazyLock<Arc<Ring>> = LazyLock::new(Arc::default);

impl Ring {
    /// The empty ring: of no key, sealing nothing, refusing nothing.
    pub fn clear() -> Arc<Self> {
        CLEAR.clone()
    }

    /// Its keys as a reader of the objects of `branch` holds them: that branch's DEKs alone, so
    /// that an object at a key of one branch sealed under another's DEK is not opened, and its
    /// objects in the clear refused if it is sealed.
    pub fn of(&self, branch: u64) -> View<'_> {
        View { ring: self, branch }
    }

    /// The DEK a writer seals new objects under, and which it is: its branch's newest, if the ring
    /// is its writer's and its branch sealed.
    pub fn sealing(&self) -> Option<(&Secret, KeyId)> {
        let id = self.sealing?;
        Some((&**self.deks.get(&id).expect("a ring holds the key it seals under"), id))
    }

    /// Whether `branch`'s objects are sealed, as its record says.
    pub fn is_sealed(&self, branch: u64) -> bool {
        self.sealed.contains(&branch)
    }

    /// Whether it holds no key.
    pub fn is_empty(&self) -> bool {
        self.deks.is_empty() && self.sealed.is_empty()
    }

    /// The generations of `branch`'s DEK it holds, in order.
    pub fn generations(&self, branch: u64) -> Vec<u32> {
        let mut generations: Vec<u32> =
            self.deks.keys().filter(|id| id.branch == branch).map(|id| id.generation).collect();
        generations.sort_unstable();
        generations
    }

    /// The branches whose keys it holds.
    pub fn branches(&self) -> BTreeSet<u64> {
        self.deks.keys().map(|id| id.branch).chain(self.sealed.iter().copied()).collect()
    }

    /// A writer's ring for tests that hold no KMS: of `branch`'s DEK of `generation`, `dek`, which
    /// it seals under, and of the others `deks` give, by which they are.
    #[cfg(any(test, feature = "testing"))]
    pub fn sealing_for_tests(
        branch: u64,
        generation: u32,
        dek: Secret,
        deks: impl IntoIterator<Item = (KeyId, Secret)>,
    ) -> Arc<Self> {
        let id = KeyId { branch, generation };
        let mut ring = Self { sealing: Some(id), ..Self::default() };
        ring.insert(Branch { cell: layout::TEST_CELL, id: branch }, generation, dek);
        for (id, dek) in deks {
            ring.insert(Branch { cell: layout::TEST_CELL, id: id.branch }, id.generation, dek);
        }
        Arc::new(ring)
    }

    /// Holds `dek`, `branch`'s of `generation`: the branch is sealed.
    fn insert(&mut self, branch: Branch, generation: u32, dek: Secret) {
        self.deks.insert(KeyId { branch: branch.id, generation }, Arc::new(dek));
        self.places.insert(branch.id, branch);
        self.sealed.insert(branch.id);
    }

    /// Whether it holds `branch`'s DEKs.
    fn holds(&self, branch: Branch) -> bool {
        self.sealed.contains(&branch.id)
    }

    /// Adds the keys `record`, `branch`'s, keeps, unwrapped under `keys`, its cell's: its own DEKs,
    /// the branch sealed if it has any, and those it keeps of ancestors of other cells
    /// ([`Inherited`]).
    async fn take(
        &mut self,
        keys: &CellKeys,
        branch: Branch,
        record: &Record,
    ) -> Result<(), Error> {
        let generations = record.deks.iter().chain(record.inherited.iter().map(|had| &had.dek));
        let keks = keys.keks(generations.map(|dek| dek.kek)).await?;
        let kek = |dek: &Dek| {
            keks.get(&dek.kek).ok_or_else(|| {
                Error::Corrupt(format!(
                    "branch {branch}'s record keeps a DEK wrapped by its cell's KEK of generation \
                     {}, which its cell has not",
                    dek.kek
                ))
            })
        };
        for dek in &record.deks {
            self.insert(branch, dek.generation, unwrap_dek(kek(dek)?, branch, dek)?);
        }
        for inherited in &record.inherited {
            let dek = unwrap_inherited(kek(&inherited.dek)?, branch, inherited)?;
            self.insert(inherited.branch, inherited.dek.generation, dek);
        }
        Ok(())
    }
}

/// Every DEK `source` holds of a branch of another cell than `child`'s, rewrapped under `kek`, a
/// generation of `child`'s cell's KEK, as `child`'s record keeps them ([`Inherited`]), in the
/// order of their branches and generations (TASK-411.4).
pub fn inherit(source: &Ring, child: Branch, kek: (&Secret, u32)) -> Vec<Inherited> {
    let mut inherited: Vec<Inherited> = (source.deks.iter())
        .filter_map(|(id, dek)| {
            let of = *source.places.get(&id.branch).expect("a ring knows its DEKs' branches");
            (of.cell != child.cell).then(|| wrap_inherited(kek, child, of, id.generation, dek))
        })
        .collect();
    inherited.sort_by_key(|had| (had.branch, had.dek.generation));
    inherited
}

/// A ring's keys as a reader of one branch's objects holds them ([`Ring::of`]).
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    ring: &'a Ring,
    branch: u64,
}

impl Keys for View<'_> {
    fn dek(&self, id: KeyId) -> Option<&Secret> {
        (id.branch == self.branch).then(|| self.ring.deks.get(&id).map(|dek| &**dek)).flatten()
    }

    fn sealed(&self) -> bool {
        self.ring.is_sealed(self.branch)
    }
}

/// The branch the object at `key` is of, by its key: the moves of an object are of that object's
/// branch, as the collector seals them under the DEK it was sealed under (TASK-411.6). None for one
/// of no branch's.
pub fn branch_of(key: &str) -> Option<u64> {
    match layout::kind(key) {
        Key::Segment { branch, .. }
        | Key::Extent { branch, .. }
        | Key::Checkpoint { branch, .. } => Some(branch.id),
        Key::Moves { .. } => branch_of(&layout::moved_from(key)?),
        _ => None,
    }
}

/// The keys of the reader of the object at `key`, holding `ring`: those of the object's branch,
/// as its key says ([`Ring::of`]); none for an object of no branch's.
pub fn of_key<'a>(ring: &'a Ring, key: &str) -> View<'a> {
    // A branch no ID names, whose keys no ring holds.
    ring.of(branch_of(key).unwrap_or(u64::MAX))
}

/// The keys a host's fetchers open parts with, which they share for all the host's stores, as they
/// share what they keep (TASK-92.21): the union of the rings of the stores the host serves, each
/// held for as long as its store is open ([`HostRing::hold`]).
#[derive(Default)]
pub struct HostRing {
    held: Mutex<Held>,
}

#[derive(Default)]
struct Held {
    rings: BTreeMap<u64, Arc<Ring>>,
    next: u64,
    union: Arc<Ring>,
}

impl Held {
    fn unite(&mut self) {
        let mut union = Ring::default();
        for ring in self.rings.values() {
            union.deks.extend(ring.deks.iter().map(|(id, dek)| (*id, dek.clone())));
            union.places.extend(ring.places.iter().map(|(id, branch)| (*id, *branch)));
            union.sealed.extend(ring.sealed.iter().copied());
        }
        self.union = Arc::new(union);
    }
}

impl HostRing {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The keys it holds now, of every store open.
    pub fn ring(&self) -> Arc<Ring> {
        self.held.lock().unwrap().union.clone()
    }

    /// Forgets the rings of the stores of `scope`'s branches ([`Ring`]'s owner), which its
    /// fetchers then open nothing with, as a host told a tenant's KEK is revoked or destroyed, or a
    /// cell shredded, does (TASK-411.7): the keys other stores hold stay, those of branches of the
    /// scope among them, as a fork of another tenant's keeps what it inherited. Returns how many it
    /// forgot.
    pub fn forget(&self, scope: Scope) -> usize {
        let mut held = self.held.lock().unwrap();
        let before = held.rings.len();
        held.rings.retain(|_, ring| !ring.owner.is_some_and(|owner| scope.covers(owner.cell)));
        let forgotten = before - held.rings.len();
        if forgotten > 0 {
            held.unite();
        }
        forgotten
    }

    /// Holds `ring`'s keys among its own until what it returns is dropped. One holding none
    /// changes nothing.
    pub fn hold(self: &Arc<Self>, ring: Arc<Ring>) -> RingHold {
        if ring.is_empty() {
            return RingHold { host: None, id: 0 };
        }
        let mut held = self.held.lock().unwrap();
        let id = held.next;
        held.next += 1;
        held.rings.insert(id, ring);
        held.unite();
        RingHold { host: Some(self.clone()), id }
    }
}

/// A store's ring held by its host's ([`HostRing::hold`]): let go of when dropped.
pub struct RingHold {
    host: Option<Arc<HostRing>>,
    id: u64,
}

impl Drop for RingHold {
    fn drop(&mut self) {
        if let Some(host) = self.host.take() {
            let mut held = host.held.lock().unwrap();
            held.rings.remove(&self.id);
            held.unite();
        }
    }
}

/// What an unwrap of a cell's KEK by the KMS found: the KEK, or the error, and whether it said the
/// tenant's KEK is revoked or destroyed.
type Unwrapped = Result<Arc<Secret>, (Error, bool)>;

/// An unwrap of a cell's KEK by the KMS, or what it found, which a taking's openings share.
type Unwrap = Shared<BoxFuture<'static, Unwrapped>>;

/// A cell's KEKs unwrapped, by the cell and generation, each with the taking it was unwrapped for:
/// none for a cell this host made and has not taken yet, whose first taking here it serves.
type Takings = HashMap<(Cell, u32), (Option<(u64, u64)>, Unwrap)>;

/// How a host keeps its pools of new cell KEKs (doc-16, Forks and shared points; TASK-411.4): a
/// tenant's refilled in batches, on `handle`, off the paths that take from it, once a take leaves
/// fewer than `low`, or a sealed store of the tenant opens.
#[derive(Clone, Debug)]
pub struct Pooling {
    pub low: usize,
    /// How many KEKs a refill wraps, all at once.
    pub batch: usize,
    pub handle: tokio::runtime::Handle,
}

impl Pooling {
    /// The pools of a host on `handle`: 8 KEKs a tenant at least, 16 more a refill, so that a
    /// zygote's restores, forking at once into new cells, find theirs.
    pub fn on(handle: tokio::runtime::Handle) -> Self {
        Self { low: 8, batch: 16, handle }
    }
}

/// A refill of a tenant's pool, going, which whoever waits for it shares ([`Kms::filled`]).
type Refill = Shared<BoxFuture<'static, ()>>;

/// A tenant's new cell KEKs, each wrapped by its KEK, and the refill of them going, if one is.
#[derive(Default)]
struct Pool {
    keks: Vec<(Arc<Secret>, Wrapped)>,
    filling: Option<Refill>,
}

/// The host's pools, by their tenants, and how many times each tenant's was dropped, its era, so
/// that a refill going as its pool is dropped adds nothing to the next.
#[derive(Default)]
struct Pools {
    pools: HashMap<u64, Pool>,
    eras: HashMap<u64, u64>,
}

/// How long a host believes the KMS that a tenant has no KEK, its cells made in the clear, asking
/// it nothing for the tenant meanwhile (TASK-411.10): so that a host opening its stores, each
/// asking for a KEK should its cell be made, calls the KMS once a minute at most, and a tenant
/// given a KEK has its cells made sealed from a minute on at most. Its cells made before stay in
/// the clear (doc-16, The KMS and how a cell chooses).
pub const KEKLESS_FOR: Duration = Duration::from_secs(60);

/// A host's KMS, and what it keeps of what it asked it: each cell's KEKs, by the taking of the cell
/// its object names, unwrapped once for all of it ([`CellKeys`]); a pool of new cell KEKs a tenant,
/// wrapped by their tenants' KEKs ahead of the cells made with them; the tenants whose KEKs it
/// last heard are revoked or destroyed; and those it heard have none ([`KEKLESS_FOR`]).
pub struct Kms {
    kms: kms::Shared,
    /// Each cell's KEKs unwrapped, by generation, and the taking they were unwrapped for: a
    /// taking since unwraps anew, so that a tenant whose KEK was revoked takes none of its cells
    /// again.
    takings: Mutex<Takings>,
    /// New cell KEKs by their tenants, wrapped, for the cells made.
    pools: Mutex<Pools>,
    /// How the pools are refilled: none, a KEK wrapped when one is wanted and its pool empty.
    pooling: Option<Pooling>,
    /// The tenants whose KEKs are revoked or destroyed, as last heard, and how it was said: until
    /// the KMS answers a call for one again ([`Kms::revoked`]).
    refused: Mutex<HashMap<u64, String>>,
    /// The tenants the KMS said have no KEK, and when: believed for [`KEKLESS_FOR`] from then,
    /// until it answers a call for one.
    kekless: Mutex<HashMap<u64, Instant>>,
    /// The cells it was told are shredded, whose KEKs it unwraps no more ([`Kms::forget`]).
    shredded: Mutex<BTreeSet<Cell>>,
}

/// The error of a KMS's refusal, naming its tenant.
fn refused(error: kms::Error) -> Error {
    Error::Kms(error.to_string())
}

/// Whether `error` says its tenant's KEK is revoked or destroyed.
fn revokes(error: &kms::Error) -> bool {
    matches!(error, kms::Error::Revoked { .. } | kms::Error::Destroyed { .. })
}

impl Kms {
    /// The host's over `kms`, its pools refilled a KEK at a time, as one is wanted: tests', which
    /// count the calls a path makes.
    pub fn new(kms: kms::Shared) -> Arc<Self> {
        Self::make(kms, None)
    }

    /// The host's over `kms`, its pools refilled as `pooling` says.
    pub fn pooled(kms: kms::Shared, pooling: Pooling) -> Arc<Self> {
        Self::make(kms, Some(pooling))
    }

    fn make(kms: kms::Shared, pooling: Option<Pooling>) -> Arc<Self> {
        Arc::new(Self {
            kms,
            takings: Mutex::default(),
            pools: Mutex::default(),
            pooling,
            refused: Mutex::default(),
            kekless: Mutex::default(),
            shredded: Mutex::default(),
        })
    }

    /// The KMS it asks.
    pub fn inner(&self) -> &kms::Shared {
        &self.kms
    }

    /// What the KMS answered a call for `tenant`: noted, a refusal saying its KEK is revoked or
    /// destroyed dropping what it keeps of the tenant ([`Kms::revoked`]), an answer forgetting
    /// that it was.
    fn heard<T>(&self, tenant: u64, answer: Result<T, kms::Error>) -> Result<T, kms::Error> {
        match &answer {
            Ok(_) => self.answered(tenant),
            Err(error) if revokes(error) => self.revoked(tenant, error.to_string()),
            Err(_) => {}
        }
        answer
    }

    /// The KMS answered a call for `tenant`: its KEK is not refused, nor missing, whatever was
    /// heard before.
    fn answered(&self, tenant: u64) {
        self.refused.lock().unwrap().remove(&tenant);
        self.kekless.lock().unwrap().remove(&tenant);
    }

    /// The KMS said `tenant` has no KEK: believed for [`KEKLESS_FOR`] from now.
    fn has_no_kek(&self, tenant: u64) {
        self.kekless.lock().unwrap().insert(tenant, Instant::now());
    }

    /// Whether the KMS said `tenant` has no KEK, less than [`KEKLESS_FOR`] ago.
    fn kekless(&self, tenant: u64) -> bool {
        let mut kekless = self.kekless.lock().unwrap();
        match kekless.get(&tenant) {
            Some(heard) if heard.elapsed() < KEKLESS_FOR => true,
            Some(_) => {
                kekless.remove(&tenant);
                false
            }
            None => false,
        }
    }

    /// `tenant`'s KEK is revoked or destroyed, as `why` says: the KMS said so, or the control API
    /// (doc-16, Forks and shared points). Its pool is dropped, a refill of it going adding nothing,
    /// so that it makes no cell more; its cells' KEKs unwrapped are forgotten, so that no taking
    /// of them goes on unasked; and no fork inherits its keys ([`inheriting`]), until the KMS
    /// answers a call for it again.
    pub fn revoked(&self, tenant: u64, why: impl Into<String>) {
        {
            let mut pools = self.pools.lock().unwrap();
            pools.pools.remove(&tenant);
            *pools.eras.entry(tenant).or_default() += 1;
        }
        self.takings.lock().unwrap().retain(|(cell, _), _| cell.tenant != tenant);
        self.refused.lock().unwrap().insert(tenant, why.into());
    }

    /// Forgets what it holds of `scope`, as a host told so does (TASK-411.7): a tenant's, its KEK
    /// revoked or destroyed, as `why` says ([`Kms::revoked`]); a cell's, shredded, its KEKs
    /// unwrapped dropped and refused from then on, whatever object of the cell its asker read
    /// before ([`crate::cell::shredded`]).
    pub fn forget(&self, scope: Scope, why: impl Into<String>) {
        match scope {
            Scope::Tenant(tenant) => self.revoked(tenant, why),
            Scope::Cell(cell) => {
                self.shredded.lock().unwrap().insert(cell);
                self.takings.lock().unwrap().retain(|(of, _), _| *of != cell);
            }
        }
    }

    /// Why `tenant`'s keys are refused, if its KEK is revoked or destroyed, as last heard.
    pub fn refuses(&self, tenant: u64) -> Option<Error> {
        self.refused.lock().unwrap().get(&tenant).cloned().map(Error::Kms)
    }

    /// A new KEK for a cell of `tenant`'s, and it wrapped by the tenant's KEK, from the pool, or
    /// wrapped by the KMS now: none if the tenant has no KEK there, its cells in the clear, which
    /// is believed for [`KEKLESS_FOR`], no call made for it meanwhile. Any other refusal fails it,
    /// naming the tenant: a tenant whose KEK is revoked makes no cell. A take leaving the pool low
    /// refills it, off this path ([`Pooling`]). Takes asking at once, before the KMS has said, ask
    /// it each: a KEK wrapped is the one cell's it makes, which no other take may share.
    pub async fn new_kek(
        self: &Arc<Self>,
        tenant: u64,
    ) -> Result<Option<(Arc<Secret>, Wrapped)>, Error> {
        let pooled =
            (self.pools.lock().unwrap().pools.get_mut(&tenant)).and_then(|pool| pool.keks.pop());
        if let Some(kek) = pooled {
            self.refill(tenant);
            return Ok(Some(kek));
        }
        if self.kekless(tenant) {
            return Ok(None);
        }
        let kek = Secret::random();
        match self.heard(tenant, self.kms.wrap_dyn(tenant, &kek).await) {
            Ok(wrapped) => {
                self.refill(tenant);
                Ok(Some((Arc::new(kek), wrapped)))
            }
            Err(kms::Error::NoSuchTenant { .. }) => {
                self.has_no_kek(tenant);
                Ok(None)
            }
            Err(error) => Err(refused(error)),
        }
    }

    /// Gives back `kek`, a new cell KEK of `tenant`'s that made no cell, to the pool: unless the
    /// pool was dropped since, the tenant's KEK revoked, the KEK then dropped with it.
    pub fn give_back(&self, tenant: u64, kek: (Arc<Secret>, Wrapped)) {
        if self.refused.lock().unwrap().contains_key(&tenant) {
            return;
        }
        self.pools.lock().unwrap().pools.entry(tenant).or_default().keks.push(kek);
    }

    /// How many new cell KEKs of `tenant`'s its pool holds.
    pub fn in_pool(&self, tenant: u64) -> usize {
        self.pools.lock().unwrap().pools.get(&tenant).map_or(0, |pool| pool.keks.len())
    }

    /// Refills `tenant`'s pool, if it holds fewer than [`Pooling::low`] and no refill goes: a batch
    /// of new KEKs wrapped at once on the pooling's runtime, which this returns before. Without
    /// pooling, nothing; nor for a tenant whose KEK was last heard revoked or destroyed, until the
    /// KMS answers a call for it again.
    pub fn refill(self: &Arc<Self>, tenant: u64) {
        let Some(pooling) = &self.pooling else { return };
        if self.refused.lock().unwrap().contains_key(&tenant) {
            return;
        }
        let refill = {
            let mut pools = self.pools.lock().unwrap();
            let era = pools.eras.get(&tenant).copied().unwrap_or(0);
            let pool = pools.pools.entry(tenant).or_default();
            if pool.keks.len() >= pooling.low || pool.filling.is_some() {
                return;
            }
            let refill: Refill = self.clone().fill(tenant, era, pooling.batch).boxed().shared();
            pool.filling = Some(refill.clone());
            refill
        };
        pooling.handle.spawn(refill);
    }

    /// Returns once the refill of `tenant`'s pool going, if one is, is done.
    pub fn filled(&self, tenant: u64) -> impl std::future::Future<Output = ()> + use<> {
        let pools = self.pools.lock().unwrap();
        let filling = pools.pools.get(&tenant).and_then(|pool| pool.filling.clone());
        async move {
            if let Some(filling) = filling {
                filling.await;
            }
        }
    }

    /// Wraps `batch` new KEKs of `tenant`'s at once, into its pool of era `era`: dropped since, they
    /// go with it. A refusal saying the tenant's KEK is revoked or destroyed drops its pool
    /// ([`Kms::heard`]); a tenant with no KEK, of cells in the clear, gets none, which is believed
    /// for [`KEKLESS_FOR`] ([`Kms::new_kek`]); any other refusal is logged, the next take
    /// refilling it again.
    async fn fill(self: Arc<Self>, tenant: u64, era: u64, batch: usize) {
        let wraps = (0..batch).map(|_| {
            let kms = self.kms.clone();
            async move {
                let kek = Secret::random();
                let wrapped = kms.wrap_dyn(tenant, &kek).await;
                wrapped.map(|wrapped| (Arc::new(kek), wrapped))
            }
        });
        let mut wrapped = Vec::new();
        let mut failed = None;
        for answer in futures::future::join_all(wraps).await {
            match answer {
                Ok(kek) => wrapped.push(kek),
                Err(error) => failed = Some(error),
            }
        }
        match failed {
            Some(error) if revokes(&error) => self.revoked(tenant, error.to_string()),
            Some(kms::Error::NoSuchTenant { .. }) => self.has_no_kek(tenant),
            Some(error) => log::warn!("fxdax: refilling tenant {tenant}'s pool of KEKs: {error}"),
            None => self.answered(tenant),
        }
        let mut pools = self.pools.lock().unwrap();
        if pools.eras.get(&tenant).copied().unwrap_or(0) != era {
            return;
        }
        let pool = pools.pools.entry(tenant).or_default();
        pool.filling = None;
        pool.keks.extend(wrapped);
    }

    /// KEK generation `kek` of `cell`, made by this host with that KEK, `secret`: kept for the
    /// taking `taking`, its epoch and holder, unwrapped by none; or, none given, for the first
    /// taking of it here.
    fn made(&self, cell: Cell, taking: Option<(u64, u64)>, kek: &Kek, secret: Arc<Secret>) {
        let ready: Unwrap = futures::future::ready(Ok(secret)).boxed().shared();
        self.takings.lock().unwrap().insert((cell, kek.generation), (taking, ready));
    }

    /// KEK generation `generation` of `cell`, of `keks`, its object's: unwrapped by the KMS once
    /// for the taking `taking`, however many ask; or none, this host having made it, and this its
    /// first taking here.
    async fn kek(
        &self,
        cell: Cell,
        (keks, taking): (&[Kek], (u64, u64)),
        generation: u32,
    ) -> Result<Arc<Secret>, Error> {
        if self.shredded.lock().unwrap().contains(&cell) {
            return Err(crate::cell::shredded(cell));
        }
        let unwrap = {
            let mut takings = self.takings.lock().unwrap();
            match takings.get_mut(&(cell, generation)) {
                Some((at, unwrap)) if *at == Some(taking) => unwrap.clone(),
                Some((at @ None, unwrap)) => {
                    *at = Some(taking);
                    unwrap.clone()
                }
                _ => {
                    let wrapped = (keks.iter())
                        .find(|kek| kek.generation == generation)
                        .ok_or_else(|| {
                            Error::Corrupt(format!(
                                "cell {cell} has no KEK of generation {generation}"
                            ))
                        })?
                        .wrapped
                        .clone();
                    let kms = self.kms.clone();
                    let unwrap: Unwrap = async move {
                        match kms.unwrap_dyn(cell.tenant, &wrapped).await {
                            Ok(kek) => Ok(Arc::new(kek)),
                            Err(error) => {
                                let revoked = revokes(&error);
                                Err((refused(error), revoked))
                            }
                        }
                    }
                    .boxed()
                    .shared();
                    takings.insert((cell, generation), (Some(taking), unwrap.clone()));
                    unwrap
                }
            }
        };
        match unwrap.await {
            Ok(kek) => {
                self.answered(cell.tenant);
                Ok(kek)
            }
            Err((error, revokes)) => {
                // Asked anew the next time: a tenant's KEK revoked may be given back.
                let mut takings = self.takings.lock().unwrap();
                if takings.get(&(cell, generation)).is_some_and(|(at, _)| *at == Some(taking)) {
                    takings.remove(&(cell, generation));
                }
                drop(takings);
                if revokes {
                    self.revoked(cell.tenant, error.to_string());
                }
                Err(error)
            }
        }
    }
}

/// The keys of a cell as a host took it, or read it: its object's KEKs, unwrapped as they are
/// asked for ([`CellKeys::kek`]), once a taking, by the host's KMS.
pub struct CellKeys {
    cell: Cell,
    keks: Vec<Kek>,
    /// The taking they are unwrapped for: its epoch and holder.
    taking: (u64, u64),
    kms: Option<Arc<Kms>>,
    /// Whether its object says it is shredded ([`crate::cell::shred`], TASK-411.7).
    shredded: bool,
}

impl CellKeys {
    /// The keys of `cell`, whose object, as taken, or read, is `object`, unwrapped by `kms`, the
    /// host's, if it has one, for the object's taking.
    pub fn of(cell: Cell, object: &Object, kms: Option<Arc<Kms>>) -> Self {
        Self::taken(cell, object, (object.epoch, object.holder), kms)
    }

    /// The keys of `cell`, whose object, as read, is `object`, unwrapped by `kms` for the taking
    /// the holder `placement` makes of it next ([`crate::cell::hold`]): the object's, if it is
    /// that placement's already, the next epoch's otherwise. Read before the cell is held, as a
    /// writer's recovery is, they are those it is held with then, unwrapped once.
    pub fn to_hold(cell: Cell, object: &Object, kms: Option<Arc<Kms>>, placement: u64) -> Self {
        let taking = match object.holder == placement {
            true => (object.epoch, object.holder),
            false => (object.epoch + 1, placement),
        };
        Self::taken(cell, object, taking, kms)
    }

    /// The keys of `cell`, which has no object: none, in the clear, as a point's cell, which no
    /// host makes, its ancestors' read under the cells they are of.
    pub fn none(cell: Cell, kms: Option<Arc<Kms>>) -> Self {
        Self { cell, keks: Vec::new(), taking: (0, 0), kms, shredded: false }
    }

    fn taken(cell: Cell, object: &Object, taking: (u64, u64), kms: Option<Arc<Kms>>) -> Self {
        Self { cell, keks: object.keks.clone(), taking, kms, shredded: object.shredded }
    }

    /// Whether the host reading under them has a KMS, which a cell sealed needs.
    pub fn has_kms(&self) -> bool {
        self.kms.is_some()
    }

    /// The cell's.
    pub fn cell(&self) -> Cell {
        self.cell
    }

    /// Whether its branches are sealed: its object keeps a KEK, or it was shredded, its KEKs
    /// refused then ([`CellKeys::kek`]): no branch of it is written in the clear.
    pub fn sealed(&self) -> bool {
        !self.keks.is_empty() || self.shredded
    }

    /// The newest generation of its KEK, if it has one.
    pub fn newest(&self) -> Option<u32> {
        self.keks.iter().map(|kek| kek.generation).max()
    }

    /// Its KEK of generation `generation`, unwrapped: refused without a KMS, and as the KMS
    /// refuses it, naming the cell's tenant; and if the cell is shredded, naming it.
    pub async fn kek(&self, generation: u32) -> Result<Arc<Secret>, Error> {
        if self.shredded {
            return Err(crate::cell::shredded(self.cell));
        }
        let Some(kms) = &self.kms else {
            return Err(Error::Kms(format!(
                "tenant {}: cell {} is encrypted, and this host has no KMS",
                self.cell.tenant, self.cell
            )));
        };
        kms.kek(self.cell, (&self.keks, self.taking), generation).await
    }

    /// Its KEKs of `generations`, unwrapped.
    async fn keks(
        &self,
        generations: impl IntoIterator<Item = u32>,
    ) -> Result<BTreeMap<u32, Arc<Secret>>, Error> {
        let mut keks = BTreeMap::new();
        for generation in generations.into_iter().collect::<BTreeSet<_>>() {
            keks.insert(generation, self.kek(generation).await?);
        }
        Ok(keks)
    }

    /// Its newest KEK, unwrapped, and its generation: a cell sealed has one, but a shredded one,
    /// refused.
    async fn newest_kek(&self) -> Result<(Arc<Secret>, u32), Error> {
        if self.shredded {
            return Err(crate::cell::shredded(self.cell));
        }
        let newest = self.newest().expect("a cell sealed has a KEK");
        Ok((self.kek(newest).await?, newest))
    }

    /// A new DEK of generation `generation` for `branch`, of this cell, wrapped under its newest
    /// KEK.
    async fn new_dek(&self, branch: Branch, generation: u32) -> Result<Dek, Error> {
        let (kek, newest) = self.newest_kek().await?;
        Ok(wrap_dek((&kek, newest), branch, generation, &Secret::random()))
    }
}

/// The KEK a cell made by a host gets: one the host wrapped ahead, by its KMS, if it has one and
/// the cell's tenant a KEK there ([`Kms::new_kek`]); none, the cell in the clear, otherwise.
pub async fn new_cell_kek(
    kms: Option<&Arc<Kms>>,
    tenant: u64,
) -> Result<Option<(Arc<Secret>, Kek)>, Error> {
    let Some(kms) = kms else { return Ok(None) };
    let made = kms.new_kek(tenant).await?;
    Ok(made.map(|(secret, wrapped)| (secret, Kek { generation: FIRST, wrapped })))
}

/// What a holding of a cell that may have made it ([`crate::cell::hold`]) did with `kek`, the KEK
/// it would have made it with: kept for the taking if the cell was made with it, and given back to
/// the pool otherwise. Returns the cell's keys.
pub fn held(
    kms: Option<&Arc<Kms>>,
    cell: Cell,
    object: Object,
    kek: Option<(Arc<Secret>, Kek)>,
) -> CellKeys {
    if let (Some(kms), Some((secret, kek))) = (kms, kek) {
        match object.keks.first() == Some(&kek) && object.keks.len() == 1 {
            true => kms.made(cell, Some((object.epoch, object.holder)), &kek, secret),
            false => kms.give_back(cell.tenant, (secret, kek.wrapped)),
        }
    }
    CellKeys::of(cell, &object, kms.cloned())
}

/// `branch`'s record, given its first DEK by a conditional update if its cell is sealed and it has
/// none: what the branch's writer does as it takes it, a fork's child's record, which its parent's
/// writers made, naming none (their bytes the same, whoever wrote them), or a new branch's. With
/// it, in the same update, the DEKs of every ancestor of another cell whose objects it reads,
/// rewrapped under its cell's KEK ([`Inherited`], TASK-411.4): read by crossing into their cells,
/// their KEKs unwrapped by the KMS, as a record without its keys is read ([`ring`]). A fork into
/// another cell gives them as it makes the fork, so that its child's writer finds them there
/// ([`give_inherited`]); a child whose fork's host failed to is given them here.
pub async fn with_dek(
    bucket: &impl Bucket,
    keys: &CellKeys,
    branch: Branch,
    record: Record,
) -> Result<Record, Error> {
    if !keys.sealed() || !record.deks.is_empty() {
        return Ok(record);
    }
    let (kek, newest) = keys.newest_kek().await?;
    let source = ring(bucket, keys, branch, &record, false).await?;
    let inherited = inherit(&source, branch, (&kek, newest));
    let deks = vec![wrap_dek((&kek, newest), branch, FIRST, &Secret::random())];
    give_keys(bucket, branch, (deks, inherited)).await
}

/// Gives `branch`'s record `keys`, its first DEK and those it inherits, by a conditional update,
/// unless it has its keys already, another's giving first. Returns the record as it left it.
async fn give_keys(
    bucket: &impl Bucket,
    branch: Branch,
    keys: (Vec<Dek>, Vec<Inherited>),
) -> Result<Record, Error> {
    branch::give_deks(bucket, branch, |found| found.deks.is_empty().then(|| keys.clone())).await
}

/// Adds a generation of `branch`'s DEK, the next, by a conditional update of its record: new
/// objects are sealed under it from the next opening of its store on, the others staying readable
/// under theirs. Returns the generation.
pub async fn add_generation(
    bucket: &impl Bucket,
    keys: &CellKeys,
    branch: Branch,
) -> Result<u32, Error> {
    if !keys.sealed() {
        return Err(Error::Corrupt(format!("branch {branch}'s cell is in the clear")));
    }
    let found = branch::record(bucket, branch).await?.ok_or(Error::NoBranch)?;
    let mut generation = next_generation(&found.deks);
    let mut dek = keys.new_dek(branch, generation).await?;
    loop {
        let mut raced = false;
        let given = branch::give_deks(bucket, branch, |found| {
            if next_generation(&found.deks) != generation {
                raced = true;
                return None;
            }
            Some(([found.deks.clone(), vec![dek.clone()]].concat(), found.inherited.clone()))
        })
        .await?;
        if !raced {
            debug_assert!(given.deks.iter().any(|had| had.generation == generation));
            return Ok(generation);
        }
        // Another added one meanwhile: the next after it.
        generation = next_generation(&given.deks);
        dek = keys.new_dek(branch, generation).await?;
    }
}

/// The generation after the newest of `deks`.
fn next_generation(deks: &[Dek]) -> u32 {
    deks.iter().map(|dek| dek.generation + 1).max().unwrap_or(FIRST)
}

/// Rotates the KEK of the cell `held` holds, its object `object` as taken: a new generation of it,
/// wrapped by its tenant's KEK, added to the cell's object, conditional on `held` holding it still;
/// then every record of the cell's branches with DEKs under older generations rewrapped under it,
/// each by a conditional update. Older generations stay in the cell's object (see the module's
/// docs). Returns the new generation and the records rewrapped.
pub async fn rotate(
    bucket: &impl Bucket,
    kms: &Arc<Kms>,
    held: crate::cell::Held,
    object: Object,
) -> Result<(u32, usize), Error> {
    let cell = held.cell;
    let old = CellKeys::of(cell, &object, Some(kms.clone()));
    let Some(newest) = old.newest() else {
        return Err(Error::Corrupt(format!("cell {cell} is in the clear")));
    };
    let generation = newest + 1;
    let secret = Secret::random();
    let wrapped = kms.inner().wrap_dyn(cell.tenant, &secret).await.map_err(refused)?;
    let kek = Kek { generation, wrapped };
    let object = crate::cell::add_kek(bucket, held, kek.clone()).await?;
    kms.made(cell, Some((object.epoch, object.holder)), &kek, Arc::new(secret));
    let keys = CellKeys::of(cell, &object, Some(kms.clone()));
    let new = keys.kek(generation).await?;
    let mut rewrapped = 0;
    for meta in bucket::list(bucket, &layout::records(cell)).await {
        let Key::Record { branch } = layout::kind(&meta.key) else { continue };
        let Some(found) = branch::record(bucket, branch).await? else { continue };
        let all_under = |record: &Record, generation: u32| {
            let inherited = record.inherited.iter().map(|had| &had.dek);
            record.deks.iter().chain(inherited).all(|dek| dek.kek == generation)
        };
        if all_under(&found, generation) {
            continue;
        }
        // Every generation of the cell's KEK its DEKs may be under, unwrapped first: the update's
        // step, which may run again on what it finds, unwraps nothing. Those it inherits are
        // rewrapped too (TASK-411.4).
        let inherited = found.inherited.iter().map(|had| &had.dek);
        let keks = keys.keks(found.deks.iter().chain(inherited).map(|dek| dek.kek)).await?;
        let kek = |dek: &Dek| {
            keks.get(&dek.kek).ok_or_else(|| {
                Error::Corrupt(format!("branch {branch}'s DEK under a KEK not read"))
            })
        };
        let mut failed = None;
        branch::give_deks(bucket, branch, |found| {
            if all_under(found, generation) {
                return None;
            }
            let rewrap = |dek: &Dek| {
                let secret = unwrap_dek(kek(dek)?, branch, dek)?;
                Ok(wrap_dek((&new, generation), branch, dek.generation, &secret))
            };
            let rewrap_inherited = |had: &Inherited| {
                let secret = unwrap_inherited(kek(&had.dek)?, branch, had)?;
                let of = had.branch;
                Ok(wrap_inherited((&new, generation), branch, of, had.dek.generation, &secret))
            };
            let deks = found.deks.iter().map(rewrap).collect::<Result<Vec<_>, Error>>();
            let inherited =
                found.inherited.iter().map(rewrap_inherited).collect::<Result<Vec<_>, Error>>();
            match deks.and_then(|deks| Ok((deks, inherited?))) {
                Ok(keys) => Some(keys),
                Err(error) => {
                    failed = Some(error);
                    None
                }
            }
        })
        .await?;
        if let Some(error) = failed {
            return Err(error);
        }
        rewrapped += 1;
    }
    Ok((generation, rewrapped))
}

/// The ring of a store of `branch`, whose record is `record`, of the cell `keys` are of: its
/// branch's DEKs, every generation, and the newest to seal new objects under if `writes`; those
/// its record keeps of ancestors of other cells ([`Inherited`]); and its ancestors', along its
/// origins, each record read. A host with no KMS reads nothing more of a cell in the clear, its
/// ring empty, its readers refusing what is sealed ([`Error::NoKey`]); one with a KMS walks its
/// origins as for a sealed one.
///
/// An ancestor of its cell's is unwrapped under its cell's KEK. One of another cell (TASK-411.4):
/// a record with its keys holds those of every one it reads, as they were given it with them
/// ([`with_dek`]), so that it needs no other cell's KEK, a sealed ancestor it holds not refused
/// ([`Error::Inherited`]). A record without them yet, a point's, a fork's child's before they are
/// given, or a branch's of a cell in the clear, is read by crossing into that ancestor's cell, its
/// object read, its KEK unwrapped by the KMS, its keys and those it inherits taken from there on;
/// but a writer of a cell in the clear inherits nothing sealed ([`Error::Inherited`]): it could
/// keep none of its keys, and no sealed data goes into the clear.
pub async fn ring(
    bucket: &impl Bucket,
    keys: &CellKeys,
    branch: Branch,
    record: &Record,
    writes: bool,
) -> Result<Arc<Ring>, Error> {
    if !keys.sealed() && !keys.has_kms() {
        return Ok(Ring::clear());
    }
    let mut ring = Ring { owner: Some(branch), ..Ring::default() };
    ring.take(keys, branch, record).await?;
    if writes {
        let newest = record.deks.iter().map(|dek| dek.generation).max();
        ring.sealing = newest.map(|generation| KeyId { branch: branch.id, generation });
    }
    // Whether its record has its keys, and with them all it inherits from other cells.
    let given = !record.deks.is_empty();
    let mut crossed: HashMap<Cell, CellKeys> = HashMap::new();
    let mut origin = record.origin();
    let mut seen = BTreeSet::from([branch]);
    while let Some(at) = origin {
        let parent = at.parent;
        if !seen.insert(parent) {
            return Err(Error::Corrupt(format!("branch {branch}'s origins go round at {parent}")));
        }
        let found = branch::record(bucket, parent).await?;
        let found = found.ok_or_else(|| Error::Lost(layout::record(parent)))?;
        origin = found.origin();
        if found.deks.is_empty() || ring.holds(parent) {
            continue;
        }
        if parent.cell == keys.cell() {
            ring.take(keys, parent, &found).await?;
            continue;
        }
        if given || (writes && !keys.sealed()) {
            return Err(Error::Inherited { branch, from: parent });
        }
        let theirs = match crossed.entry(parent.cell) {
            std::collections::hash_map::Entry::Occupied(theirs) => theirs.into_mut(),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                let object = crate::cell::read(bucket, parent.cell).await?.ok_or_else(|| {
                    Error::Corrupt(format!(
                        "branch {parent}'s record keeps DEKs, and its cell has no object"
                    ))
                })?;
                vacant.insert(CellKeys::of(parent.cell, &object, keys.kms.clone()))
            }
        };
        ring.take(theirs, parent, &found).await?;
    }
    Ok(match ring.is_empty() {
        true => Ring::clear(),
        false => Arc::new(ring),
    })
}

/// The ring the collector compacts a cell's objects under (TASK-411.6): of `records`, branches of
/// the cell and their records, those with any DEK sealed, their objects in the clear refused; and,
/// if `keys`, the cell's, are given, their DEKs and those they keep of ancestors of other cells
/// ([`Ring::take`]), unwrapped under the generations of its KEK they name, each unwrapped once
/// ([`CellKeys`] keeps what it unwrapped). It seals nothing of its own: the collector seals what it
/// compacts under the key its source was sealed under. Without `keys`, it unwraps nothing: what a
/// pass compacting only objects in the clear holds, asking its KMS nothing.
pub async fn compacting<'a>(
    records: impl IntoIterator<Item = (Branch, &'a Record)>,
    keys: Option<&CellKeys>,
) -> Result<Arc<Ring>, Error> {
    // Collected first: an iterator's adapters held across the awaits below would leave the
    // future `Send` for no one lifetime of theirs, which a sweep's spawned passes need.
    let records: Vec<(Branch, &Record)> = records.into_iter().collect();
    let mut ring = Ring::default();
    for (branch, record) in records {
        if record.deks.is_empty() {
            continue;
        }
        match keys {
            None => {
                ring.sealed.insert(branch.id);
            }
            Some(keys) => ring.take(keys, branch, record).await?,
        }
    }
    Ok(match ring.is_empty() {
        true => Ring::clear(),
        false => Arc::new(ring),
    })
}

/// The ring `branch`'s writer would hold, its record `record`, to seal what it writes of the
/// branch's under: its cell's object read, its KEK unwrapped by `kms`, the host's (refused with
/// none); empty for a branch in the clear, nothing read. What another than its writer seals the
/// branch's objects under: its deletion's checkpoints at the fork points of its forks to other
/// cells ([`crate::branch::delete_under`], TASK-411.4).
pub async fn writing(
    bucket: &impl Bucket,
    kms: Option<&Arc<Kms>>,
    branch: Branch,
    record: &Record,
) -> Result<Arc<Ring>, Error> {
    if record.deks.is_empty() {
        return Ok(Ring::clear());
    }
    let object = crate::cell::read(bucket, branch.cell).await?.ok_or_else(|| {
        Error::Corrupt(format!("branch {branch}'s record keeps DEKs, and its cell has no object"))
    })?;
    let keys = CellKeys::of(branch.cell, &object, kms.cloned());
    ring(bucket, &keys, branch, record, true).await
}

/// The keys of a fork's parent that whoever records the fork seals the parent's checkpoint at the
/// fork point under, if the child is of another cell and the parent sealed (TASK-411.4): those its
/// writer holds; or, for its deletion, or a failed fork's claim, the host's KMS, with the parent's
/// record if read, asked only once a checkpoint is to be written ([`writing`]).
#[derive(Clone, Copy)]
pub enum Parents<'a> {
    Held(&'a Arc<Ring>),
    Kms(Option<&'a Arc<Kms>>, Option<&'a Record>),
}

impl Parents<'_> {
    /// The ring of `parent`, the parent, to seal its checkpoint under: its record read if not
    /// given; refused if it is sealed and the KMS is none ([`Error::Kms`]), rather than write it
    /// in the clear, which the child refuses.
    pub async fn ring(self, bucket: &impl Bucket, parent: Branch) -> Result<Arc<Ring>, Error> {
        match self {
            Self::Held(ring) => Ok(ring.clone()),
            Self::Kms(kms, Some(record)) => writing(bucket, kms, parent, record).await,
            Self::Kms(kms, None) => {
                let record = branch::record(bucket, parent).await?;
                let record = record.ok_or_else(|| Error::Lost(layout::record(parent)))?;
                writing(bucket, kms, parent, &record).await
            }
        }
    }
}

/// What a fork into another cell gives its child (doc-16, Forks and shared points; TASK-411.4),
/// made before anything of the fork is ([`inheriting`]): the keys its parent's store holds, which
/// its child inherits, and a KEK for its child's cell from the host's pool, given back if no cell
/// is made with it.
pub struct Inheriting {
    kms: Arc<Kms>,
    child: Branch,
    /// An ancestor of another cell than the child's whose objects are sealed: what an error names.
    from: Branch,
    source: Arc<Ring>,
    kek: Option<(Arc<Secret>, Kek)>,
}

impl Drop for Inheriting {
    fn drop(&mut self) {
        if let Some((secret, kek)) = self.kek.take() {
            self.kms.give_back(self.child.cell.tenant, (secret, kek.wrapped));
        }
    }
}

/// What a fork of `parent`, whose store holds `source`, to `child` gives its child, if it is of
/// another cell, and `source` holds keys of branches of other cells than the child's: none for a
/// fork within a cell, whose child's writer gives its keys as it opens ([`with_dek`]); nor for a
/// point, which nothing writes, whose readers read what it names under the keys of the cells it is
/// of, and whose forks rewrap them ([`ring`]); nor for a fork of branches in the clear, which makes
/// no request more. Asked for before anything of the
/// fork is made, so that a fork refused makes nothing: refused if `kms`, the host's, last heard
/// that a tenant of those branches' KEK is revoked or destroyed ([`Kms::revoked`]); if the
/// child's tenant has no KEK, its cell in the clear ([`Error::Inherited`]: no sealed data goes
/// into the clear); or as the KMS refuses a KEK for the child's cell, which comes from the host's
/// pool, no call made then ([`Kms::new_kek`]).
pub async fn inheriting(
    kms: Option<&Arc<Kms>>,
    source: &Arc<Ring>,
    parent: Branch,
    child: layout::Child,
) -> Result<Option<Inheriting>, Error> {
    let layout::Child { branch: child, point, .. } = child;
    let outside: BTreeSet<Branch> =
        source.places.values().copied().filter(|of| of.cell != child.cell).collect();
    let Some(&from) = outside.first().filter(|_| child.cell != parent.cell && !point) else {
        return Ok(None);
    };
    let Some(kms) = kms else {
        return Err(Error::Kms(format!(
            "tenant {}: branch {from} is encrypted, and this host has no KMS",
            from.cell.tenant
        )));
    };
    for tenant in outside.iter().map(|of| of.cell.tenant).collect::<BTreeSet<_>>() {
        if let Some(refused) = kms.refuses(tenant) {
            return Err(refused);
        }
    }
    let kek = new_cell_kek(Some(kms), child.cell.tenant).await?;
    let kek = Some(kek.ok_or(Error::Inherited { branch: child, from })?);
    Ok(Some(Inheriting { kms: kms.clone(), child, from, source: source.clone(), kek }))
}

/// Gives the child of the fork `inheriting` is of its keys, once its record is made (doc-7, Forks;
/// TASK-411.4): makes its cell with the KEK from the pool, held by no placement, so that any takes
/// it, as `holder` made it, the host keeping the KEK for its first taking here, which unwraps
/// nothing then; then gives its record its first DEK and the keys it inherits rewrapped under that
/// KEK, in one conditional update ([`Inherited`]). A cell made there already, by another, is
/// left as it is, its own KEK used, and the pool's given back. A record given its keys already is
/// left so. Its record is read as the cell is made: two round trips, a create and an update.
/// Returns the record as it left it.
pub async fn give_inherited(
    bucket: &impl Bucket,
    mut inheriting: Inheriting,
    holder: u64,
) -> Result<Record, Error> {
    let child = inheriting.child;
    let (secret, kek) = inheriting.kek.clone().expect("a KEK until a cell is made with it");
    // The record read as the cell is made: what the update starts from, a round trip less.
    let record = layout::record(child);
    let making = crate::cell::make(bucket, child.cell, holder, &kek);
    let (made, found) = futures::join!(making, bucket::get(bucket, &record));
    let (object, made) = made?;
    let kms = inheriting.kms.clone();
    let kek = match made {
        true => {
            inheriting.kek = None;
            kms.made(child.cell, None, &kek, secret.clone());
            (secret, FIRST)
        }
        false => {
            let keys = CellKeys::of(child.cell, &object, Some(kms));
            if !keys.sealed() {
                return Err(Error::Inherited { branch: child, from: inheriting.from });
            }
            keys.newest_kek().await?
        }
    };
    let deks = vec![wrap_dek((&kek.0, kek.1), child, FIRST, &Secret::random())];
    let inherited = inherit(&inheriting.source, child, (&kek.0, kek.1));
    let keys = (deks, inherited);
    branch::give_deks_found(bucket, child, found, |had| had.deks.is_empty().then(|| keys.clone()))
        .await
}

/// What a host gives its stores to seal and read under (TASK-411.3): its KMS, if it has one, and
/// the keys its fetchers share, of the stores it serves ([`HostRing`], the host's fetchers'
/// `fetch::Memory::keys`).
#[derive(Clone, Default)]
pub struct Host {
    pub kms: Option<Arc<Kms>>,
    pub ring: Arc<HostRing>,
}

impl Host {
    /// Forgets what the host holds of `scope`'s keys, as it is told a tenant's KEK is revoked or
    /// destroyed, or a cell shredded, `why` saying which (TASK-411.7): its KMS's ([`Kms::forget`])
    /// and its fetchers' ([`HostRing::forget`]). Returns how many stores' rings its fetchers forgot.
    pub fn forget(&self, scope: Scope, why: impl Into<String>) -> usize {
        if let Some(kms) = &self.kms {
            kms.forget(scope, why);
        }
        self.ring.forget(scope)
    }
}

/// What a host is told to forget the keys of (doc-16, Shredding; TASK-411.7): a tenant's, whose KEK
/// is revoked or destroyed in the KMS, or a cell's, shredded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Tenant(u64),
    Cell(Cell),
}

impl Scope {
    /// Whether `cell` is of it.
    pub fn covers(self, cell: Cell) -> bool {
        match self {
            Self::Tenant(tenant) => cell.tenant == tenant,
            Self::Cell(of) => cell == of,
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tenant(tenant) => write!(f, "tenant {tenant}"),
            Self::Cell(cell) => write!(f, "tenant {}'s cell {cell}", cell.tenant),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{self, Kind as Volume};
    use crate::layout::{Origin, TEST_CELL, test_branch};
    use crate::record::Kind;
    use crate::testing::{Direct, SimBucket};
    use extent_keys::kms::LocalKms;
    use extent_keys::testing::Counted;
    use futures::executor::block_on;

    /// A KMS holding tenant 1's KEK, counting its calls, and the host's over it.
    fn kms() -> (tempfile::TempDir, Arc<Counted<LocalKms>>, Arc<Kms>) {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalKms::open(dir.path().join("kms")).unwrap();
        local.create(TEST_CELL.tenant).unwrap();
        let counted = Arc::new(Counted::new(local));
        let shared: kms::Shared = counted.clone();
        (dir, counted, Kms::new(shared))
    }

    /// A DEK wrapped under a cell's KEK unwraps for its place alone: another branch's, of the
    /// same cell, or another generation, fails AES-KW's check, a wrapped DEK copied there.
    #[test]
    fn deks_unwrap_at_their_place_alone() {
        let (kek, dek) = (Secret::random(), Secret::random());
        let wrapped = wrap_dek((&kek, 1), test_branch(7), 2, &dek);
        assert_eq!((wrapped.generation, wrapped.kek), (2, 1));
        assert_eq!(unwrap_dek(&kek, test_branch(7), &wrapped).unwrap().expose(), dek.expose());
        let refused = |branch: Branch, wrapped: &Dek| {
            unwrap_dek(&kek, branch, wrapped).map(|_| ()).unwrap_err().to_string()
        };
        assert_eq!(
            refused(test_branch(8), &wrapped),
            "corrupt: branch 1/1/8's DEK of generation 2: not authentic: a wrapped key, by its \
             integrity check"
        );
        let relabelled = Dek { generation: 3, ..wrapped.clone() };
        assert_eq!(
            refused(test_branch(7), &relabelled),
            "corrupt: branch 1/1/7's DEK of generation 3: not authentic: a wrapped key, by its \
             integrity check"
        );
        let other_cell = Branch { cell: Cell { tenant: 1, id: 2 }, id: 7 };
        assert!(unwrap_dek(&kek, other_cell, &wrapped).is_err());
    }

    /// A ring's keys, as a reader of one branch holds them: that branch's DEKs alone, its objects
    /// sealed if it has any; a branch of none, in the clear. A host's ring is the union of its
    /// stores', each held while its store is open.
    #[test]
    fn rings_hold_each_branchs_keys_for_its_objects_alone() {
        let one = KeyId { branch: 7, generation: 1 };
        let other = KeyId { branch: 8, generation: 1 };
        let ring = Ring::sealing_for_tests(7, 1, Secret::random(), [(other, Secret::random())]);
        assert!(ring.of(7).dek(one).is_some() && ring.of(8).dek(other).is_some());
        assert!(ring.of(7).dek(other).is_none() && ring.of(8).dek(one).is_none());
        assert!(ring.of(7).sealed() && ring.of(8).sealed() && !ring.of(9).sealed());
        assert_eq!(ring.sealing().map(|(_, id)| id), Some(one));
        assert!(of_key(&ring, &layout::segment(test_branch(8), 3, 1)).sealed());
        let moves = layout::moves(&layout::segment(test_branch(7), 3, 1), 1).unwrap();
        assert_eq!(branch_of(&moves), Some(7));
        assert!(!of_key(&ring, &layout::cell(TEST_CELL)).sealed());

        let host = HostRing::new();
        assert!(host.ring().is_empty());
        let held = host.hold(ring.clone());
        let another = Ring::sealing_for_tests(9, 2, Secret::random(), []);
        let also = host.hold(another);
        assert_eq!(host.ring().branches(), BTreeSet::from([7, 8, 9]));
        assert_eq!(host.ring().generations(9), [2]);
        drop(held);
        assert_eq!(host.ring().branches(), BTreeSet::from([9]));
        drop(also);
        assert!(host.ring().is_empty());
        drop(host.hold(Ring::clear()));
    }

    /// A cell made with a KEK from the host's pool keeps it wrapped; a taking of it by another
    /// host unwraps it once, however many of its branches it opens at once, and a taking after
    /// that once more; a fork's child of the cell and a point there read their parent's DEK, of
    /// the cell's; a writer of a cell in the clear inheriting from a sealed branch is refused.
    #[test]
    fn cell_keys_unwrap_once_a_taking_and_rings_follow_origins() {
        let (_dir, counted, kms) = kms();
        let bucket = Direct(SimBucket::new());
        let kek = block_on(new_cell_kek(Some(&kms), TEST_CELL.tenant)).unwrap();
        let (secret, wrapped) = kek.as_ref().map(|(secret, kek)| (secret.expose(), kek)).unwrap();
        let making = (true, Some(wrapped));
        let volume = Some(("root", cell::Volume { branch: 7, kind: Volume::Fxfs }));
        let (_, object) = block_on(cell::hold(&bucket, TEST_CELL, (5, 5), making, volume)).unwrap();
        assert_eq!(&object.keks, std::slice::from_ref(wrapped));
        assert!(!holds(&crate::layout::encode(&object), secret));
        let made = held(Some(&kms), TEST_CELL, object.clone(), kek);
        assert_eq!((counted.wraps(), counted.unwraps()), (1, 0));

        // Branches: 7, a root; 8 its fork's child; 9 a point of it; with DEKs given by writers.
        let origin = Origin { parent: test_branch(7), at: 3 };
        block_on(branch::create(&bucket, test_branch(7), 1)).unwrap();
        let child = crate::layout::Child { branch: test_branch(8), owner: 2, point: false };
        block_on(branch::record_fork(&bucket, child, origin)).unwrap();
        let point = crate::layout::Child { branch: test_branch(9), owner: 3, point: true };
        block_on(branch::record_fork(&bucket, point, origin)).unwrap();
        let record = |id| block_on(branch::record(&bucket, test_branch(id))).unwrap().unwrap();
        for id in [7, 8] {
            block_on(with_dek(&bucket, &made, test_branch(id), record(id))).unwrap();
        }
        let again = block_on(with_dek(&bucket, &made, test_branch(7), record(7))).unwrap();
        assert_eq!(again, record(7));
        assert_eq!((counted.wraps(), counted.unwraps()), (1, 0));

        // Another host takes the cell: one unwrap for the three.
        counted.reset();
        let (_, other) = kms_over(&counted);
        let (_, taken) =
            block_on(cell::hold(&bucket, TEST_CELL, (6, 6), (false, None), None)).unwrap();
        let keys = CellKeys::of(TEST_CELL, &taken, Some(other.clone()));
        let rings: Vec<_> = [(7, true), (8, true), (9, false)]
            .into_iter()
            .map(|(id, writes)| {
                block_on(ring(&bucket, &keys, test_branch(id), &record(id), writes))
            })
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 1));
        assert_eq!(rings[0].branches(), BTreeSet::from([7]));
        assert_eq!(rings[1].branches(), BTreeSet::from([7, 8]));
        assert_eq!(rings[1].sealing().map(|(_, id)| id), Some(KeyId { branch: 8, generation: 1 }));
        assert_eq!(rings[2].branches(), BTreeSet::from([7]));
        assert_eq!(rings[2].sealing().map(|(_, id)| id), None);
        let (dek, _) = rings[0].sealing().unwrap();
        assert_eq!(
            rings[1].of(7).dek(KeyId { branch: 7, generation: 1 }).unwrap().expose(),
            dek.expose()
        );
        // Read before it is held by placement 6 again, as a writer's recovery reads: the same
        // taking, no unwrap more; a taking by another, one more.
        let early = CellKeys::to_hold(TEST_CELL, &taken, Some(other.clone()), 6);
        block_on(ring(&bucket, &early, test_branch(7), &record(7), true)).unwrap();
        assert_eq!(counted.unwraps(), 1);
        let next = CellKeys::to_hold(TEST_CELL, &taken, Some(other.clone()), 7);
        block_on(ring(&bucket, &next, test_branch(7), &record(7), true)).unwrap();
        assert_eq!(counted.unwraps(), 2);

        // A fork of 8 into another cell of the tenant's.
        let elsewhere = Branch { cell: Cell { tenant: 1, id: 2 }, id: 10 };
        let child = crate::layout::Child { branch: elsewhere, owner: 4, point: false };
        let origin = Origin { parent: test_branch(8), at: 1 };
        block_on(branch::create(&bucket, test_branch(8), 1)).ok();
        block_on(branch::record_fork(&bucket, child, origin)).ok();
        let forked = Kind::Fork(origin).of(4);
        let none = CellKeys::none(elsewhere.cell, Some(other));
        let refused = block_on(ring(&bucket, &none, elsewhere, &forked, true));
        assert_eq!(
            refused.map(|_| ()),
            Err(Error::Inherited { branch: elsewhere, from: test_branch(8) })
        );
    }

    /// TASK-411.4: an inherited DEK, kept in a record of another cell, unwraps for its places
    /// alone: kept by another branch's record, or of another branch, or relabelled another
    /// generation, it fails AES-KW's check.
    #[test]
    fn inherited_deks_unwrap_at_their_places_alone() {
        let (kek, dek) = (Secret::random(), Secret::random());
        let holder = Branch { cell: Cell { tenant: 2, id: 5 }, id: 8 };
        let wrapped = wrap_inherited((&kek, 3), holder, test_branch(7), 2, &dek);
        assert_eq!(
            (wrapped.branch, wrapped.dek.generation, wrapped.dek.kek),
            (test_branch(7), 2, 3)
        );
        assert_eq!(unwrap_inherited(&kek, holder, &wrapped).unwrap().expose(), dek.expose());
        let refused = |holder: Branch, wrapped: &Inherited| {
            unwrap_inherited(&kek, holder, wrapped).map(|_| ()).unwrap_err().to_string()
        };
        assert_eq!(
            refused(Branch { id: 9, ..holder }, &wrapped),
            "corrupt: branch 2/5/9's DEK of branch 1/1/7, generation 2: not authentic: a wrapped \
             key, by its integrity check"
        );
        let other = Inherited { branch: test_branch(6), ..wrapped.clone() };
        assert!(unwrap_inherited(&kek, holder, &other).is_err());
        let relabelled = Inherited { dek: Dek { generation: 3, ..wrapped.dek.clone() }, ..wrapped };
        assert!(unwrap_inherited(&kek, holder, &relabelled).is_err());
        // Nor is it a DEK of the branch's own.
        assert!(unwrap_dek(&kek, test_branch(7), &relabelled.dek).is_err());
    }

    /// TASK-411.4: a fork's child in a cell of another tenant's, before its keys are given, is read
    /// by crossing into its parent's cell, unwrapping its KEK; its first writer gives it its DEK and
    /// the parent's, rewrapped under its own cell's KEK, in one update; from then on its ring needs
    /// its own cell's KEK alone, the parent's tenant's KEK destroyed, and refuses a sealed ancestor
    /// its record holds not. Rotating its cell's KEK rewraps what it inherits too.
    #[test]
    fn rings_cross_cells_until_records_hold_what_they_inherit() {
        let (_dir, counted, kms) = kms();
        counted.inner().create(2).unwrap();
        let bucket = Direct(SimBucket::new());
        // The parent, 7, sealed, in the test cell; a fork's child, 10, in a cell of tenant 2's.
        let parent = test_branch(7);
        let parents = made_cell(&bucket, &kms, TEST_CELL, 5);
        block_on(branch::create(&bucket, parent, 1)).unwrap();
        let record = |branch| block_on(branch::record(&bucket, branch)).unwrap().unwrap();
        let given = block_on(with_dek(&bucket, &parents, parent, record(parent))).unwrap();
        let (dek, _) = block_on(ring(&bucket, &parents, parent, &given, true))
            .unwrap()
            .sealing()
            .map(|(dek, id)| (Secret::from_bytes(dek.expose()), id))
            .unwrap();
        let child = Branch { cell: Cell { tenant: 2, id: 5 }, id: 10 };
        let origin = Origin { parent, at: 3 };
        // Its record as its parent's writers make it, of no state of the parent's here to
        // checkpoint.
        let forked = crate::layout::encode(&Kind::Fork(origin).of(4));
        block_on(crate::bucket::create(&bucket, &layout::record(child), forked));
        let childs = made_cell(&bucket, &kms, child.cell, 6);

        // Read before its keys are given: across, the parent's cell's KEK unwrapped here.
        counted.reset();
        let (_, other) = kms_over(&counted);
        let other_childs = CellKeys::of(child.cell, &read_cell(&bucket, child.cell), Some(other));
        let across = block_on(ring(&bucket, &other_childs, child, &record(child), false)).unwrap();
        assert_eq!(across.branches(), BTreeSet::from([7]));
        // The parent's cell's KEK alone: the child's record keeps none to unwrap yet.
        assert_eq!(counted.unwraps(), 1);
        // Given by its first writer: its DEK and the parent's.
        let given = block_on(with_dek(&bucket, &childs, child, record(child))).unwrap();
        assert_eq!(given.deks.len(), 1);
        assert_eq!(given.inherited.iter().map(|had| had.branch).collect::<Vec<_>>(), [parent]);
        let again = block_on(with_dek(&bucket, &childs, child, record(child))).unwrap();
        assert_eq!(again, given);

        // The parent's tenant shredded: the child's ring, on a host unwrapping anew, needs its own
        // cell's KEK alone.
        counted.inner().destroy(TEST_CELL.tenant).unwrap();
        counted.reset();
        let (_, fresh) = kms_over(&counted);
        let keys = CellKeys::of(child.cell, &read_cell(&bucket, child.cell), Some(fresh.clone()));
        let rung = block_on(ring(&bucket, &keys, child, &given, true)).unwrap();
        assert_eq!(rung.branches(), BTreeSet::from([7, 10]));
        let id = KeyId { branch: 7, generation: 1 };
        assert_eq!(rung.of(7).dek(id).unwrap().expose(), dek.expose());
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 1));
        // A record given its keys without its parent's: refused, not read across.
        let stripped = Record { inherited: Vec::new(), ..given.clone() };
        let refused = block_on(ring(&bucket, &keys, child, &stripped, true)).map(|_| ());
        assert_eq!(refused, Err(Error::Inherited { branch: child, from: parent }));

        // Its cell's KEK rotated: what it inherits rewrapped under the new generation, the same.
        counted.inner().create(TEST_CELL.tenant).ok();
        let held = crate::cell::Held { cell: child.cell, epoch: 1, holder: 6 };
        let rotated = block_on(rotate(&bucket, &fresh, held, read_cell(&bucket, child.cell)));
        assert_eq!(rotated, Ok((2, 1)));
        let rotated = record(child);
        assert_eq!(rotated.inherited.iter().map(|had| had.dek.kek).collect::<Vec<_>>(), [2]);
        assert_ne!(rotated.inherited[0].dek.wrapped, given.inherited[0].dek.wrapped);
        let keys = CellKeys::of(child.cell, &read_cell(&bucket, child.cell), Some(fresh));
        let rung = block_on(ring(&bucket, &keys, child, &rotated, true)).unwrap();
        assert_eq!(rung.of(7).dek(id).unwrap().expose(), dek.expose());
    }

    /// TASK-411.4: a host's pools. A take from a pooled host refills a pool left low, a batch of
    /// wraps on its runtime; the tenant heard revoked, its pool goes, a refill going adding
    /// nothing to the next, refills stop, and forks inheriting its keys are refused, until the KMS
    /// answers for it again. A cell made by the host keeps its KEK for its first taking here.
    #[test]
    fn pools_refill_in_batches_and_go_with_their_tenants_keks() {
        let (_dir, counted, _) = kms();
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let shared: kms::Shared = counted.clone();
        let pooling = Pooling { low: 2, batch: 3, handle: runtime.handle().clone() };
        let kms = Kms::pooled(shared, pooling);
        let tenant = TEST_CELL.tenant;
        let kek = runtime.block_on(kms.new_kek(tenant)).unwrap();
        assert!(kek.is_some());
        runtime.block_on(kms.filled(tenant));
        assert_eq!((counted.wraps(), kms.in_pool(tenant)), (1 + 3, 3));
        for _ in 0..2 {
            runtime.block_on(kms.new_kek(tenant)).unwrap();
        }
        runtime.block_on(kms.filled(tenant));
        assert_eq!((counted.wraps(), kms.in_pool(tenant)), (1 + 3 + 3, 4));

        kms.revoked(tenant, "tenant 1: its KEK is revoked");
        assert_eq!(kms.in_pool(tenant), 0);
        assert_eq!(kms.refuses(tenant), Some(Error::Kms("tenant 1: its KEK is revoked".into())));
        kms.refill(tenant);
        runtime.block_on(kms.filled(tenant));
        assert_eq!((counted.wraps(), kms.in_pool(tenant)), (7, 0));
        let source = Ring::sealing_for_tests(7, 1, Secret::random(), []);
        let child = crate::layout::Child {
            branch: Branch { cell: Cell { tenant: 2, id: 9 }, id: 8 },
            owner: 1,
            point: false,
        };
        let refused = runtime.block_on(inheriting(Some(&kms), &source, test_branch(7), child));
        assert_eq!(refused.map(|_| ()), Err(Error::Kms("tenant 1: its KEK is revoked".into())));
        // The KMS answering for it: refused no more, its pool refilled.
        assert!(runtime.block_on(kms.new_kek(tenant)).unwrap().is_some());
        assert_eq!(kms.refuses(tenant), None);
        runtime.block_on(kms.filled(tenant));
        assert_eq!(kms.in_pool(tenant), 3);
        // A point, or a child of the parent's cell, takes nothing.
        let point = crate::layout::Child { point: true, ..child };
        let none = runtime.block_on(inheriting(Some(&kms), &source, test_branch(7), point));
        assert!(none.unwrap().is_none());
        let within = crate::layout::Child { branch: test_branch(8), ..child };
        let none = runtime.block_on(inheriting(Some(&kms), &source, test_branch(7), within));
        assert!(none.unwrap().is_none());
        // Taken, then dropped unused: given back.
        let taken = runtime.block_on(inheriting(Some(&kms), &source, test_branch(7), child));
        assert_eq!(kms.in_pool(2), 0);
        drop(taken);
    }

    /// TASK-411.10: a tenant the KMS says has no KEK is believed to have none for
    /// [`KEKLESS_FOR`]: its cells made in the clear, one wrap asked of the KMS however many are,
    /// the KEK given it meanwhile heard of only then; from then on its cells are made sealed.
    #[test]
    fn tenants_without_a_kek_are_asked_for_one_once_a_while() {
        let (_dir, counted, kms) = kms();
        let runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build();
        let runtime = runtime.unwrap();
        let tenant = 2;
        let new = || runtime.block_on(new_cell_kek(Some(&kms), tenant)).unwrap();
        for _ in 0..3 {
            assert!(new().is_none());
        }
        assert_eq!((counted.wraps(), counted.unwraps()), (1, 0));
        // Given a KEK: its cells still made in the clear until the bound, asking nothing.
        counted.inner().create(tenant).unwrap();
        runtime.block_on(tokio::time::advance(KEKLESS_FOR - Duration::from_secs(1)));
        assert!(new().is_none());
        assert_eq!(counted.wraps(), 1);
        runtime.block_on(tokio::time::advance(Duration::from_secs(1)));
        let (_, kek) = new().unwrap();
        assert_eq!(kek.generation, FIRST);
        assert_eq!(counted.wraps(), 2);
        // Its pool takes on from here as any tenant's.
        assert!(new().is_some());
        assert_eq!(counted.wraps(), 3);
    }

    /// A cell made by the host, held by `holder`, of a KEK of `kms`'s pool, and its keys.
    /// TASK-411.7: a cell shredded keeps no KEK, by one conditional update whoever holds it, and
    /// is taken by no one again, its branches' rings and a new branch's DEK refused naming it. A
    /// host that took it before, told nothing, still reads under what it unwrapped; told, it
    /// forgets the cell's KEK, refusing it under the object it read before, and its fetchers forget
    /// the ring of the cell's store, the tenant's other cell's staying. Told the tenant's KEK is
    /// revoked, it forgets the rest.
    #[test]
    fn shredded_cells_refuse_their_keys_and_hosts_told_forget_them() {
        let (_dir, counted, kms) = kms();
        let bucket = Direct(SimBucket::new());
        let other_cell = Cell { tenant: TEST_CELL.tenant, id: 2 };
        let ours = test_branch(7);
        let theirs = Branch { cell: other_cell, id: 20 };
        for (cell, branch) in [(TEST_CELL, ours), (other_cell, theirs)] {
            let made = made_cell(&bucket, &kms, cell, 5);
            block_on(branch::create(&bucket, branch, 1)).unwrap();
            let record = block_on(branch::record(&bucket, branch)).unwrap().unwrap();
            block_on(with_dek(&bucket, &made, branch, record)).unwrap();
        }
        let record = |branch| block_on(branch::record(&bucket, branch)).unwrap().unwrap();

        // Another host takes both cells, and opens a store of each.
        counted.reset();
        let (_, other) = kms_over(&counted);
        let host = Host { kms: Some(other.clone()), ring: HostRing::new() };
        let taken = |cell| {
            let (_, object) =
                block_on(cell::hold(&bucket, cell, (6, 6), (false, None), None)).unwrap();
            CellKeys::of(cell, &object, Some(other.clone()))
        };
        let (our_keys, their_keys) = (taken(TEST_CELL), taken(other_cell));
        let opened = |keys: &CellKeys, branch| {
            block_on(ring(&bucket, keys, branch, &record(branch), true)).map(|ring| ring.branches())
        };
        let our_ring = block_on(ring(&bucket, &our_keys, ours, &record(ours), true)).unwrap();
        let their_ring =
            block_on(ring(&bucket, &their_keys, theirs, &record(theirs), true)).unwrap();
        let _held = [host.ring.hold(our_ring), host.ring.hold(their_ring)];
        assert_eq!(host.ring.ring().branches(), BTreeSet::from([7, 20]));
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 2));

        let before = read_cell(&bucket, TEST_CELL);
        assert_eq!(block_on(cell::shred(&bucket, TEST_CELL)), Ok(1));
        let after = read_cell(&bucket, TEST_CELL);
        let expected = Object { keks: Vec::new(), shredded: true, ..before.clone() };
        assert_eq!(after, expected);
        assert_eq!(block_on(cell::shred(&bucket, TEST_CELL)), Ok(0));
        assert_eq!(read_cell(&bucket, TEST_CELL), expected);
        let refused = Error::Kms("tenant 1: cell 1/1 is shredded, its KEK gone".into());
        assert_eq!(block_on(cell::take(&bucket, TEST_CELL, 7, 7)), Err(refused.clone()));
        let holding = block_on(cell::hold(&bucket, TEST_CELL, (6, 6), (false, None), None));
        assert_eq!(holding.map(|(held, _)| held), Err(refused.clone()));
        let made_again = block_on(cell::hold(&bucket, TEST_CELL, (7, 7), (true, None), None));
        assert_eq!(made_again.map(|(held, _)| held), Err(refused.clone()));
        let held = cell::Held { cell: TEST_CELL, epoch: before.epoch, holder: before.holder };
        let kek = before.keks[0].clone();
        let added = block_on(cell::add_kek(&bucket, held, Kek { generation: 2, ..kek }));
        assert_eq!(added, Err(refused.clone()));

        // Read anew, the cell's keys are refused, sealed though: no branch of it written in the
        // clear, a new one given no DEK.
        let fresh = CellKeys::of(TEST_CELL, &after, Some(other.clone()));
        assert!(fresh.sealed());
        assert_eq!(opened(&fresh, ours), Err(refused.clone()));
        let new = test_branch(8);
        block_on(branch::create(&bucket, new, 1)).unwrap();
        assert_eq!(block_on(with_dek(&bucket, &fresh, new, record(new))), Err(refused.clone()));
        assert_eq!(record(new).deks, Vec::new());

        // Told nothing, the host reads under what it unwrapped for its taking.
        assert_eq!(opened(&our_keys, ours), Ok(BTreeSet::from([7])));
        assert_eq!(host.forget(Scope::Cell(TEST_CELL), "shredded"), 1);
        assert_eq!(opened(&our_keys, ours), Err(refused.clone()));
        assert_eq!(host.ring.ring().branches(), BTreeSet::from([20]));
        assert_eq!(opened(&their_keys, theirs), Ok(BTreeSet::from([20])));
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 2));

        assert_eq!(host.forget(Scope::Tenant(TEST_CELL.tenant), "tenant 1: revoked"), 1);
        assert_eq!(host.ring.ring().branches(), BTreeSet::new());
        assert_eq!(other.refuses(TEST_CELL.tenant), Some(Error::Kms("tenant 1: revoked".into())));
        assert_eq!(opened(&their_keys, theirs).map(drop), Ok(()));
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 3));
    }

    fn made_cell(bucket: &impl Bucket, kms: &Arc<Kms>, cell: Cell, holder: u64) -> CellKeys {
        let kek = block_on(new_cell_kek(Some(kms), cell.tenant)).unwrap();
        let making = (true, kek.as_ref().map(|(_, kek)| kek));
        let (_, object) =
            block_on(crate::cell::hold(bucket, cell, (holder, holder), making, None)).unwrap();
        held(Some(kms), cell, object, kek)
    }

    /// `cell`'s object.
    fn read_cell(bucket: &impl Bucket, cell: Cell) -> Object {
        block_on(crate::cell::read(bucket, cell)).unwrap().unwrap()
    }

    /// A host's KMS over `counted`, unwrapping anew.
    fn kms_over(counted: &Arc<Counted<LocalKms>>) -> ((), Arc<Kms>) {
        let shared: kms::Shared = counted.clone();
        ((), Kms::new(shared))
    }

    /// Whether `bytes` hold `piece` anywhere.
    fn holds(bytes: &[u8], piece: &[u8]) -> bool {
        bytes.windows(piece.len()).any(|window| window == piece)
    }
}
