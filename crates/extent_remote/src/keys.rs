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
//! tenant (a revoked one makes no cell, in the clear least of all).
//!
//! **Opening a store** gives every writer and reader of it a [`Ring`]: its branch's DEKs, every
//! generation of them, and its ancestors' in its cell, unwrapped under the cell's KEK
//! ([`ring`]); and which of those branches are sealed, whose objects in the clear its readers
//! refuse ([`Ring::of`], `extent_frame::Keys::sealed`): a downgrade. An ancestor of another cell
//! whose record has DEKs is refused ([`Error::Inherited`]): forks into other cells rewrap them
//! under the child's cell KEK, which is TASK-411.4's. A host's fetchers, which share what they
//! keep for all its stores, open parts with the union of its stores' rings ([`HostRing`]).
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

/// What a reader of a store's objects holds: the DEKs of its branch and its ancestors, by which
/// they are, and which of those branches are sealed, whose objects in the clear it refuses; and the
/// key its writer seals new objects under, its branch's newest, if it writes. Empty: a reader of
/// branches in the clear, or of metadata alone, as the collector is.
#[derive(Debug, Default)]
pub struct Ring {
    deks: HashMap<KeyId, Arc<Secret>>,
    sealed: BTreeSet<u64>,
    sealing: Option<KeyId>,
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
        ring.deks.insert(id, Arc::new(dek));
        for (id, dek) in deks {
            ring.deks.insert(id, Arc::new(dek));
            ring.sealed.insert(id.branch);
        }
        ring.sealed.insert(branch);
        Arc::new(ring)
    }

    /// Adds `branch`'s DEKs, `deks`, unwrapped by `keks`, its cell's: it is sealed if it has any.
    fn add(
        &mut self,
        branch: Branch,
        deks: &[Dek],
        keks: &BTreeMap<u32, Arc<Secret>>,
    ) -> Result<(), Error> {
        for dek in deks {
            let kek = keks.get(&dek.kek).ok_or_else(|| {
                Error::Corrupt(format!(
                    "branch {branch}'s DEK of generation {} is wrapped by its cell's KEK of \
                     generation {}, which its cell has not",
                    dek.generation, dek.kek
                ))
            })?;
            let id = KeyId { branch: branch.id, generation: dek.generation };
            self.deks.insert(id, Arc::new(unwrap_dek(kek, branch, dek)?));
            self.sealed.insert(branch.id);
        }
        Ok(())
    }
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
/// branch, as the collector seals them under its DEK (TASK-411.6). None for one of no branch's.
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

/// An unwrap of a cell's KEK by the KMS, or what it found, which a taking's openings share.
type Unwrap = Shared<BoxFuture<'static, Result<Arc<Secret>, Error>>>;

/// A host's KMS, and what it keeps of what it asked it: each cell's KEKs, by the taking of the cell
/// its object names, unwrapped once for all of it ([`CellKeys`]); and a pool of new cell KEKs,
/// wrapped by their tenants' KEKs ahead of the cells made with them.
pub struct Kms {
    kms: kms::Shared,
    /// Each cell's KEKs unwrapped, by generation, and the taking they were unwrapped for: a
    /// taking since unwraps anew, so that a tenant whose KEK was revoked takes none of its cells
    /// again.
    takings: Mutex<HashMap<(Cell, u32), ((u64, u64), Unwrap)>>,
    /// New cell KEKs by their tenants, wrapped, for the cells made.
    pool: Mutex<HashMap<u64, Vec<(Arc<Secret>, Wrapped)>>>,
}

/// The error of a KMS's refusal, naming its tenant.
fn refused(error: kms::Error) -> Error {
    Error::Kms(error.to_string())
}

impl Kms {
    pub fn new(kms: kms::Shared) -> Arc<Self> {
        Arc::new(Self { kms, takings: Mutex::default(), pool: Mutex::default() })
    }

    /// The KMS it asks.
    pub fn inner(&self) -> &kms::Shared {
        &self.kms
    }

    /// A new KEK for a cell of `tenant`'s, and it wrapped by the tenant's KEK, from the pool, or
    /// wrapped by the KMS now: none if the tenant has no KEK there, its cells in the clear. Any
    /// other refusal fails it, naming the tenant: a tenant whose KEK is revoked makes no cell.
    pub async fn new_kek(&self, tenant: u64) -> Result<Option<(Arc<Secret>, Wrapped)>, Error> {
        if let Some(kek) = self.pool.lock().unwrap().get_mut(&tenant).and_then(Vec::pop) {
            return Ok(Some(kek));
        }
        let kek = Secret::random();
        match self.kms.wrap_dyn(tenant, &kek).await {
            Ok(wrapped) => Ok(Some((Arc::new(kek), wrapped))),
            Err(kms::Error::NoSuchTenant { .. }) => Ok(None),
            Err(error) => Err(refused(error)),
        }
    }

    /// Gives back `kek`, a new cell KEK of `tenant`'s that made no cell, to the pool.
    pub fn give_back(&self, tenant: u64, kek: (Arc<Secret>, Wrapped)) {
        self.pool.lock().unwrap().entry(tenant).or_default().push(kek);
    }

    /// Drops the pool of `tenant`'s new cell KEKs: its KEK was revoked or destroyed, and its pool
    /// makes no cell more (doc-16, Forks and shared points).
    pub fn drop_pool(&self, tenant: u64) {
        self.pool.lock().unwrap().remove(&tenant);
    }

    /// KEK generation `kek` of `cell`, made by this host with that KEK, `secret`: kept for the
    /// taking `taking`, its epoch and holder, unwrapped by none.
    fn made(&self, cell: Cell, taking: (u64, u64), kek: &Kek, secret: Arc<Secret>) {
        let ready: Unwrap = futures::future::ready(Ok(secret)).boxed().shared();
        self.takings.lock().unwrap().insert((cell, kek.generation), (taking, ready));
    }

    /// KEK generation `generation` of `cell`, of `keks`, its object's: unwrapped by the KMS once
    /// for the taking `taking`, however many ask.
    async fn kek(
        &self,
        cell: Cell,
        (keks, taking): (&[Kek], (u64, u64)),
        generation: u32,
    ) -> Result<Arc<Secret>, Error> {
        let unwrap = {
            let mut takings = self.takings.lock().unwrap();
            match takings.get(&(cell, generation)) {
                Some((at, unwrap)) if *at == taking => unwrap.clone(),
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
                        let unwrapped = kms.unwrap_dyn(cell.tenant, &wrapped).await;
                        Ok(Arc::new(unwrapped.map_err(refused)?))
                    }
                    .boxed()
                    .shared();
                    takings.insert((cell, generation), (taking, unwrap.clone()));
                    unwrap
                }
            }
        };
        let unwrapped = unwrap.await;
        if unwrapped.is_err() {
            // Asked anew the next time: a tenant's KEK revoked may be given back.
            let mut takings = self.takings.lock().unwrap();
            if takings.get(&(cell, generation)).is_some_and(|(at, _)| *at == taking) {
                takings.remove(&(cell, generation));
            }
        }
        unwrapped
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
        Self { cell, keks: Vec::new(), taking: (0, 0), kms }
    }

    fn taken(cell: Cell, object: &Object, taking: (u64, u64), kms: Option<Arc<Kms>>) -> Self {
        Self { cell, keks: object.keks.clone(), taking, kms }
    }

    /// Whether the host reading under them has a KMS, which a cell sealed needs.
    pub fn has_kms(&self) -> bool {
        self.kms.is_some()
    }

    /// The cell's.
    pub fn cell(&self) -> Cell {
        self.cell
    }

    /// Whether its branches are sealed: its object keeps a KEK.
    pub fn sealed(&self) -> bool {
        !self.keks.is_empty()
    }

    /// The newest generation of its KEK, if it has one.
    pub fn newest(&self) -> Option<u32> {
        self.keks.iter().map(|kek| kek.generation).max()
    }

    /// Its KEK of generation `generation`, unwrapped: refused without a KMS, and as the KMS
    /// refuses it, naming the cell's tenant.
    pub async fn kek(&self, generation: u32) -> Result<Arc<Secret>, Error> {
        let Some(kms) = &self.kms else {
            return Err(Error::Kms(format!(
                "tenant {}: cell {} is encrypted, and this host has no KMS",
                self.cell.tenant, self.cell
            )));
        };
        kms.kek(self.cell, (&self.keks, self.taking), generation).await
    }

    /// Its KEKs of the generations `deks` name, unwrapped.
    async fn keks(&self, deks: &[Dek]) -> Result<BTreeMap<u32, Arc<Secret>>, Error> {
        let mut keks = BTreeMap::new();
        for generation in deks.iter().map(|dek| dek.kek).collect::<BTreeSet<_>>() {
            keks.insert(generation, self.kek(generation).await?);
        }
        Ok(keks)
    }

    /// A new DEK of generation `generation` for `branch`, of this cell, wrapped under its newest
    /// KEK.
    async fn new_dek(&self, branch: Branch, generation: u32) -> Result<Dek, Error> {
        let newest = self.newest().expect("a cell sealed has a KEK");
        let kek = self.kek(newest).await?;
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
            true => kms.made(cell, (object.epoch, object.holder), &kek, secret),
            false => kms.give_back(cell.tenant, (secret, kek.wrapped)),
        }
    }
    CellKeys::of(cell, &object, kms.cloned())
}

/// `branch`'s record, given its first DEK by a conditional update if its cell is sealed and it has
/// none: what the branch's writer does as it takes it, a fork's child's record, which its parent's
/// writers made, naming none (their bytes the same, whoever wrote them), or a new branch's.
pub async fn with_dek(
    bucket: &impl Bucket,
    keys: &CellKeys,
    branch: Branch,
    record: Record,
) -> Result<Record, Error> {
    if !keys.sealed() || !record.deks.is_empty() {
        return Ok(record);
    }
    let dek = keys.new_dek(branch, FIRST).await?;
    branch::give_deks(bucket, branch, |found| match found.deks.is_empty() {
        true => Some(vec![dek.clone()]),
        false => None,
    })
    .await
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
            Some([found.deks.clone(), vec![dek.clone()]].concat())
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
    kms.made(cell, (object.epoch, object.holder), &kek, Arc::new(secret));
    let keys = CellKeys::of(cell, &object, Some(kms.clone()));
    let new = keys.kek(generation).await?;
    let mut rewrapped = 0;
    for meta in bucket::list(bucket, &layout::records(cell)).await {
        let Key::Record { branch } = layout::kind(&meta.key) else { continue };
        let Some(found) = branch::record(bucket, branch).await? else { continue };
        if found.deks.iter().all(|dek| dek.kek == generation) {
            continue;
        }
        // Every generation of the cell's KEK its DEKs may be under, unwrapped first: the update's
        // step, which may run again on what it finds, unwraps nothing.
        let keks = keys.keks(&found.deks).await?;
        let mut failed = None;
        branch::give_deks(bucket, branch, |found| {
            if found.deks.iter().all(|dek| dek.kek == generation) {
                return None;
            }
            let rewrap = |dek: &Dek| {
                let kek = keks.get(&dek.kek).ok_or_else(|| {
                    Error::Corrupt(format!("branch {branch}'s DEK under a KEK not read"))
                })?;
                let secret = unwrap_dek(kek, branch, dek)?;
                Ok(wrap_dek((&new, generation), branch, dek.generation, &secret))
            };
            match found.deks.iter().map(rewrap).collect::<Result<Vec<_>, Error>>() {
                Ok(deks) => Some(deks),
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
/// branch's DEKs, every generation, and the newest to seal new objects under if `writes`; and its
/// ancestors', along its origins, each record read, unwrapped under its cell's KEK, of its cell's
/// alone ([`Error::Inherited`] for one of another cell with DEKs, TASK-411.4). A host with no KMS
/// reads nothing more of a cell in the clear, its ring empty, its readers refusing what is sealed
/// ([`Error::NoKey`]); one with a KMS walks its origins as for a sealed one, refusing a sealed
/// ancestor of another cell.
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
    let mut ring = Ring::default();
    ring.add(branch, &record.deks, &keys.keks(&record.deks).await?)?;
    if writes {
        let newest = record.deks.iter().map(|dek| dek.generation).max();
        ring.sealing = newest.map(|generation| KeyId { branch: branch.id, generation });
    }
    let mut origin = record.origin();
    let mut seen = BTreeSet::from([branch]);
    while let Some(at) = origin {
        let parent = at.parent;
        if !seen.insert(parent) {
            return Err(Error::Corrupt(format!("branch {branch}'s origins go round at {parent}")));
        }
        let found = branch::record(bucket, parent).await?;
        let found = found.ok_or_else(|| Error::Lost(layout::record(parent)))?;
        if !found.deks.is_empty() {
            if parent.cell != keys.cell() {
                return Err(Error::Inherited { branch, from: parent });
            }
            ring.add(parent, &found.deks, &keys.keks(&found.deks).await?)?;
        }
        origin = found.origin();
    }
    Ok(match ring.is_empty() {
        true => Ring::clear(),
        false => Arc::new(ring),
    })
}

/// What a host gives its stores to seal and read under (TASK-411.3): its KMS, if it has one, and
/// the keys its fetchers share, of the stores it serves ([`HostRing`], the host's fetchers'
/// `fetch::Memory::keys`).
#[derive(Clone, Default)]
pub struct Host {
    pub kms: Option<Arc<Kms>>,
    pub ring: Arc<HostRing>,
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
        assert_eq!(of_key(&ring, &layout::segment(test_branch(8), 3)).sealed(), true);
        let moves = layout::moves(&layout::segment(test_branch(7), 3)).unwrap();
        assert_eq!(branch_of(&moves), Some(7));
        assert_eq!(of_key(&ring, &layout::cell(TEST_CELL)).sealed(), false);

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
    /// the cell's, a fork into another cell of a sealed branch none.
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
