// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the keys a cell is kept under in the bucket (doc-16, Keys; TASK-411.3). A
//! tenant's KEK, in a KMS, wraps each of its cells' KEKs, which the cell's object keeps
//! ([`Kek`], [`crate::cell::Object::keks`]); a cell's KEK wraps each of its cells' DEKs, which
//! the cell's record keeps ([`Dek`], [`crate::record::Record::deks`]). Every cell's object keeps
//! a KEK, but a shredded cell's, and every cell that writes has a DEK: nothing is in the clear
//! (TASK-411.28), and every host has a KMS ([`Host::kms`]).
//!
//! **Taking a cell** unwraps its KEK with one call of the KMS, which a host's [`CachingKms`] keeps
//! for that taking, whatever number of the cell's volumes it opens: its cells' DEKs are unwrapped
//! with it locally ([`CellKeys`]). A cell made by a host gets a KEK the host wrapped before, which
//! it keeps in a pool for the cells it makes: none is unwrapped then. Every cell is made encrypted
//! ([`new_cell_kek`]): any refusal of the KMS fails the making, naming the tenant, a KMS saying it
//! has no KEK for the tenant among them, as destroyed (a real KMS forgets a deleted key, answering
//! as for one never made: AWS KMS, OpenBao, TASK-411.19). (Before TASK-411.28, a tenant with no KEK
//! had its cells made in the clear, that answer believed for a minute, TASK-411.10, and an
//! encrypted marker in the bucket told a forgotten KEK from none, TASK-411.19: both went with the
//! clear cells.)
//!
//! **Opening a cell** gives every writer and reader of it a [`Ring`]: its cell's DEKs, every
//! generation of them, and its ancestors', unwrapped under the cell's KEK ([`ring`]); and which of
//! those cells are encrypted, whose objects authenticated under keys it lacks its readers refuse
//! ([`Ring::of`], `extent_frame::Keys::encrypted`). A host's fetchers, which share what they keep
//! for all its cells, open parts with the union of its cells' rings ([`HostRing`]).
//!
//! **Forks into other cells** (doc-16, Forks and shared versions; TASK-411.4). A cell's record
//! keeps, beside its own DEKs, the DEKs of every ancestor of another cell whose objects it reads,
//! each generation, rewrapped under its own cell's KEK ([`InheritedDek`],
//! [`crate::record::Record::inherited`]), given with its first DEK, in one conditional update: a
//! record with DEKs holds all it inherits from other cells, so that its readers need no other
//! cell's KEK, nor another tenant's KMS, and shredding a parent leaves it what it inherited. A
//! fork's host gives them as it makes the fork, from the keys its parent's cell holds, its child's
//! cell made with a KEK of its pool ([`inheriting`], [`give_inherited`]): no KMS call on the fork's
//! path. A record without them yet (a fork's child before its keys, a version) is read by crossing
//! into its ancestors' cells, their KEKs unwrapped by the KMS, which its first writer then rewraps
//! ([`with_dek`]).
//!
//! **Pools.** A host wraps new cell KEKs ahead, a pool a tenant, refilled in batches on its
//! runtime as a take leaves one low, off the fork's path ([`Pooling`]). A tenant whose KEK the KMS
//! says is revoked or destroyed, or the control API does ([`CachingKms::revoked`]), has its pool
//! dropped, and no fork inherits its keys, until the KMS answers for it again.
//!
//! **Generations.** A record's DEKs are a list of generations, each wrapped by a generation of its
//! cell's KEK, which it names. New objects are encrypted under the newest ([`Ring::encrypting`]);
//! the others stay readable. A generation is added by a conditional update of the record
//! ([`add_generation`]); rotating a cell's KEK adds a generation of it to the cell's object, then
//! rewraps every record's DEKs under it, each by a conditional update ([`rotate`]). Older
//! generations of the cell's KEK stay in its object: a writer that read the cell before a rotation
//! may wrap a new cell's DEK under the generation it read, after the rotation listed its
//! records; dropping them needs that fenced first.
//!
//! **Shredding** (doc-16, Shredding; TASK-411.7). A tenant is shredded by destroying its KEK in the
//! KMS, a cell by taking its wrapped KEKs out of its object ([`crate::cell::shred`]): a cell
//! shredded is encrypted and refuses its KEKs, naming itself. A host told either forgets what it
//! holds of it ([`Host::forget`], a [`Scope`]): its KMS's unwrapped cell KEKs and pool, and its
//! fetchers' keys of the scope's cells; DEKs other tenants' cells hold, which they inherited,
//! stay theirs.
//!
//! A DEK is wrapped with AES-KW under a key HKDF derives from the cell's KEK and the DEK's place:
//! its tenant, cell and generation. So a wrapped DEK copied to another cell's record, or
//! relabelled another generation, fails AES-KW's check.

use crate::Error;
use crate::bucket::Bucket;
use crate::head;
use crate::layout::{self, CellId, Head, Kind as Key};
use crate::lifecycle::{self, Start, Step};
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

/// A generation of a cell's DEK, wrapped by its cell's KEK of generation `kek`: what its record
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
const DEK_INFO: &[u8] = b"cladefs dek wrap";

/// The key `kek`, a generation of `cell`'s cell's KEK, wraps `cell`'s DEK of `generation`
/// under: bound to its place.
fn wrapping_key(kek: &Secret, cell: CellId, generation: u32) -> Secret {
    let place = [
        &cell.tenant.to_le_bytes()[..],
        &cell.id.to_le_bytes(),
        &cell.id.to_le_bytes(),
        &generation.to_le_bytes(),
    ]
    .concat();
    kek.derive(&Salt([0; SALT_LEN]), &[DEK_INFO, &place])
}

/// `dek`, `cell`'s of `generation`, wrapped under `kek`, generation `kek_generation` of its
/// cell's KEK.
pub fn wrap_dek(
    (kek, kek_generation): (&Secret, u32),
    cell: CellId,
    generation: u32,
    dek: &Secret,
) -> Dek {
    let wrapped = wrapping_key(kek, cell, generation).wrap(dek);
    Dek { generation, kek: kek_generation, wrapped }
}

/// The DEK `dek` of `cell` holds, unwrapped under `kek`, the generation of its cell's KEK it
/// names.
pub fn unwrap_dek(kek: &Secret, cell: CellId, dek: &Dek) -> Result<Secret, Error> {
    (wrapping_key(kek, cell, dek.generation).unwrap(&dek.wrapped)).map_err(|err| {
        Error::Corrupt(format!("cell {cell}'s DEK of generation {}: {err}", dek.generation))
    })
}

/// A generation of the DEK of `cell`, an ancestor of another cell, as the record of a cell
/// that inherits from it keeps it: wrapped by that record's own cell's KEK of generation
/// `dek.kek` (TASK-411.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedDek {
    pub cell: CellId,
    pub dek: Dek,
}

/// What HKDF derives the key an inherited DEK is wrapped under with, before its places.
const INHERITED_INFO: &[u8] = b"cladefs inherited dek wrap";

/// The key `kek`, a generation of `holder`'s cell's KEK, wraps the DEK of `of`, of `generation`,
/// that `holder`'s record keeps under: bound to both places, so that it is copied to no other
/// record, nor relabelled.
fn inherited_key(kek: &Secret, holder: CellId, of: CellId, generation: u32) -> Secret {
    let place = |cell: CellId| {
        [cell.tenant.to_le_bytes(), cell.id.to_le_bytes(), cell.id.to_le_bytes()].concat()
    };
    let places = [place(holder), place(of), generation.to_le_bytes().to_vec()].concat();
    kek.derive(&Salt([0; SALT_LEN]), &[INHERITED_INFO, &places])
}

/// `dek`, `of`'s of `generation`, as `holder`'s record keeps it, wrapped under `kek`, generation
/// `kek_generation` of `holder`'s cell's KEK.
pub fn wrap_inherited(
    (kek, kek_generation): (&Secret, u32),
    holder: CellId,
    of: CellId,
    generation: u32,
    dek: &Secret,
) -> InheritedDek {
    let wrapped = inherited_key(kek, holder, of, generation).wrap(dek);
    InheritedDek { cell: of, dek: Dek { generation, kek: kek_generation, wrapped } }
}

/// The DEK `inherited`, of `holder`'s record, unwrapped under `kek`, the generation of `holder`'s
/// cell's KEK it names.
pub fn unwrap_inherited(
    kek: &Secret,
    holder: CellId,
    inherited: &InheritedDek,
) -> Result<Secret, Error> {
    let InheritedDek { cell, dek } = inherited;
    let key = inherited_key(kek, holder, *cell, dek.generation);
    key.unwrap(&dek.wrapped).map_err(|err| {
        Error::Corrupt(format!(
            "cell {holder}'s DEK of cell {cell}, generation {}: {err}",
            dek.generation
        ))
    })
}

/// What a reader of a cell's objects holds: the DEKs of its cell and its ancestors, by which
/// they are; and the key its writer encrypts new objects under, its cell's newest, if it writes.
/// Holding any key, it refuses every object authenticated under a key it lacks ([`View`]). Empty
/// ([`Ring::keyless`]): a reader of metadata alone, as the collector's marking is ([`compacting`]
/// for its compaction's), checking CRCs.
#[derive(Debug, Default)]
pub struct Ring {
    deks: HashMap<KeyId, Arc<Secret>>,
    /// The cell, tenant and all, of each ID its DEKs are of: what rewrapping them for a cell of
    /// another cell binds them to ([`inherit`]).
    places: HashMap<u64, CellId>,
    encrypted: BTreeSet<u64>,
    encrypting: Option<KeyId>,
    /// cladefs: a cell at a version's, the DEK it encrypts what nothing writes under
    /// ([`Ring::for_version`]): none of its DEKs, which nothing inherits nor any host's fetchers
    /// hold.
    unwritten: Option<Arc<Secret>>,
    /// The cell it was opened for ([`ring`]): what a host forgetting a tenant's or a
    /// cell's keys goes by ([`HostRing::forget`], TASK-411.7). None for others'.
    owner: Option<CellId>,
}

/// The empty ring ([`Ring::keyless`]).
static KEYLESS: LazyLock<Arc<Ring>> = LazyLock::new(Arc::default);

impl Ring {
    /// The empty ring: of no key, encrypting nothing, refusing nothing: a reader of metadata alone,
    /// checking CRCs, as the collector's marking and a recovery before keys are known are. It reads
    /// no data, all encrypted (TASK-411.28), and no writer holds it.
    pub fn keyless() -> Arc<Self> {
        KEYLESS.clone()
    }

    /// Its keys as a reader of the objects of `cell` holds them: that cell's DEKs alone, so
    /// that an object at a key of one cell encrypted under another's DEK is not opened.
    pub fn of(&self, cell: u64) -> RingView<'_> {
        RingView { ring: self, cell }
    }

    /// The DEK a writer encrypts new objects under, and which it is: its cell's newest. Every
    /// writer's ring has it ([`ring`] of a writer); a reader's has none, and writes nothing.
    pub fn encrypting(&self) -> (&Secret, KeyId) {
        let id = self.encrypting.expect("a writer's ring, which encrypts");
        if let Some(unwritten) = &self.unwritten {
            return (unwritten, id);
        }
        (&**self.deks.get(&id).expect("a ring holds the key it encrypts under"), id)
    }

    /// cladefs: the DEK of `cell`'s it holds of the newest generation, and which it is, if it holds
    /// any: what a fork's host encrypts what it writes under a version of an ancestor's cell
    /// under, which every fork of the version holds (`crate::evidence`, TASK-702.2).
    pub fn encrypting_as(&self, cell: u64) -> Option<(&Secret, KeyId)> {
        let generation = *self.generations(cell).last()?;
        let id = KeyId { cell, generation };
        Some((&**self.deks.get(&id)?, id))
    }

    /// Whether it is a writer's, which encrypts ([`Ring::encrypting`]).
    pub fn encrypts(&self) -> bool {
        self.encrypting.is_some()
    }

    /// Whether it holds no key.
    pub fn is_empty(&self) -> bool {
        self.deks.is_empty() && self.encrypted.is_empty()
    }

    /// The generations of `cell`'s DEK it holds, in order.
    pub fn generations(&self, cell: u64) -> Vec<u32> {
        let mut generations: Vec<u32> =
            self.deks.keys().filter(|id| id.cell == cell).map(|id| id.generation).collect();
        generations.sort_unstable();
        generations
    }

    /// The cells whose keys it holds.
    pub fn cells(&self) -> BTreeSet<u64> {
        self.deks.keys().map(|id| id.cell).chain(self.encrypted.iter().copied()).collect()
    }

    /// A writer's ring of `cell`, encrypting under `dek` as its DEK of the first generation, given
    /// by its maker rather than read from a record (TASK-411.28): a cell's kept in no bucket, whose
    /// maker keeps its DEK (`local_cell`'s unkept cells: tests', benches', simulations').
    pub fn writing_under(cell: CellId, dek: Secret) -> Arc<Self> {
        let id = KeyId { cell: cell.id, generation: FIRST };
        let mut ring = Self { encrypting: Some(id), owner: Some(cell), ..Self::default() };
        ring.insert(cell, FIRST, dek);
        Arc::new(ring)
    }

    /// cladefs: this ring, a cell's at a version of `cell`, given a key to encrypt what nothing
    /// writes under (TASK-411.28): its open segment, which no write reaches, is encrypted as every
    /// cell's is, under a DEK drawn for it alone, of generation 0, which no record keeps, never
    /// written out.
    pub fn for_version(&self, cell: CellId) -> Arc<Self> {
        let id = KeyId { cell: cell.id, generation: 0 };
        let unwritten = Some(Arc::new(Secret::random()));
        let (deks, places, encrypted) =
            (self.deks.clone(), self.places.clone(), self.encrypted.clone());
        Arc::new(Self {
            deks,
            places,
            encrypted,
            encrypting: Some(id),
            unwritten,
            owner: self.owner,
        })
    }

    /// A writer's ring for tests that hold no KMS: of `cell`'s DEK of `generation`, `dek`, which
    /// it encrypts under, and of the others `deks` give, by which they are.
    #[cfg(any(test, feature = "testing"))]
    pub fn encrypting_for_tests(
        cell: u64,
        generation: u32,
        dek: Secret,
        deks: impl IntoIterator<Item = (KeyId, Secret)>,
    ) -> Arc<Self> {
        let id = KeyId { cell, generation };
        let mut ring = Self { encrypting: Some(id), ..Self::default() };
        ring.insert(CellId { tenant: (layout::TEST_CELL).tenant, id: cell }, generation, dek);
        for (id, dek) in deks {
            ring.insert(
                CellId { tenant: (layout::TEST_CELL).tenant, id: id.cell },
                id.generation,
                dek,
            );
        }
        Arc::new(ring)
    }

    /// Holds `dek`, `cell`'s of `generation`: the cell is encrypted.
    fn insert(&mut self, cell: CellId, generation: u32, dek: Secret) {
        self.deks.insert(KeyId { cell: cell.id, generation }, Arc::new(dek));
        self.places.insert(cell.id, cell);
        self.encrypted.insert(cell.id);
    }

    /// Whether it holds `cell`'s DEKs.
    fn holds(&self, cell: CellId) -> bool {
        self.encrypted.contains(&cell.id)
    }

    /// Adds the keys `record`, `cell`'s, keeps, unwrapped under `keys`, its cell's: its own DEKs,
    /// the cell encrypted if it has any, and those it keeps of ancestors of other cells
    /// ([`InheritedDek`]).
    async fn take(&mut self, keys: &CellKeys, cell: CellId, head: &Head) -> Result<(), Error> {
        let generations = head.deks.iter().chain(head.inherited.iter().map(|had| &had.dek));
        let keks = keys.keks(generations.map(|dek| dek.kek)).await?;
        let kek = |dek: &Dek| {
            keks.get(&dek.kek).ok_or_else(|| {
                Error::Corrupt(format!(
                    "cell {cell}'s head keeps a DEK wrapped by its KEK of generation {}, which \
                     it has not",
                    dek.kek
                ))
            })
        };
        for dek in &head.deks {
            self.insert(cell, dek.generation, unwrap_dek(kek(dek)?, cell, dek)?);
        }
        for inherited in &head.inherited {
            let dek = unwrap_inherited(kek(&inherited.dek)?, cell, inherited)?;
            self.insert(inherited.cell, inherited.dek.generation, dek);
        }
        Ok(())
    }
}

/// Every DEK `source` holds of a cell of another cell than `child`'s, rewrapped under `kek`, a
/// generation of `child`'s cell's KEK, as `child`'s record keeps them ([`InheritedDek`]), in the
/// order of their cells and generations (TASK-411.4).
pub fn inherit(source: &Ring, child: CellId, kek: (&Secret, u32)) -> Vec<InheritedDek> {
    let mut inherited: Vec<InheritedDek> = (source.deks.iter())
        .filter_map(|(id, dek)| {
            let of = *source.places.get(&id.cell).expect("a ring knows its DEKs' cells");
            (of != child).then(|| wrap_inherited(kek, child, of, id.generation, dek))
        })
        .collect();
    inherited.sort_by_key(|had| (had.cell, had.dek.generation));
    inherited
}

/// A ring's keys as a reader of one cell's objects holds them ([`Ring::of`]).
#[derive(Clone, Copy, Debug)]
pub struct RingView<'a> {
    ring: &'a Ring,
    cell: u64,
}

impl Keys for RingView<'_> {
    fn dek(&self, id: KeyId) -> Option<&Secret> {
        (id.cell == self.cell).then(|| self.ring.deks.get(&id).map(|dek| &**dek)).flatten()
    }

    /// Every cell is encrypted (TASK-411.28), so a ring holding any key refuses an object
    /// authenticated under a key it lacks, whatever the object's cell's record says: a record,
    /// which nothing authenticates, rewritten without its DEKs must not have its cell's objects
    /// read as stored, a bucket's writer's own bytes passing for them. The keyless ring alone
    /// reads them so, checking CRCs.
    fn encrypted(&self) -> bool {
        !self.ring.is_empty()
    }
}

/// The cell the object at `key` is of, by its key: the moves of an object are of that object's
/// cell, as the collector encrypts them under the DEK it was encrypted under (TASK-411.6). None
/// for one of no cell's.
pub fn cell_of(key: &str) -> Option<u64> {
    match layout::kind(key) {
        Key::Segment { cell, .. } | Key::Extent { cell, .. } | Key::Checkpoint { cell, .. } => {
            Some(cell.id)
        }
        Key::Moves { .. } => cell_of(&layout::moved_from(key)?),
        _ => None,
    }
}

/// The keys of the reader of the object at `key`, holding `ring`: those of the object's cell,
/// as its key says ([`Ring::of`]); none for an object of no cell's.
pub fn of_key<'a>(ring: &'a Ring, key: &str) -> RingView<'a> {
    // A cell no ID names, whose keys no ring holds.
    ring.of(cell_of(key).unwrap_or(u64::MAX))
}

/// The keys a host's fetchers open parts with, which they share for all the host's cells, as they
/// share what they keep (TASK-92.21): the union of the rings of the cells the host serves, each
/// held for as long as its cell is open ([`HostRing::hold`]).
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
            union.places.extend(ring.places.iter().map(|(id, cell)| (*id, *cell)));
            union.encrypted.extend(ring.encrypted.iter().copied());
        }
        self.union = Arc::new(union);
    }
}

impl HostRing {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The keys it holds now, of every cell open.
    pub fn ring(&self) -> Arc<Ring> {
        self.held.lock().unwrap().union.clone()
    }

    /// Forgets the rings of `scope`'s cells ([`Ring`]'s owner), which its
    /// fetchers then open nothing with, as a host told a tenant's KEK is revoked or destroyed, or a
    /// cell shredded, does (TASK-411.7): the keys other cells hold stay, those of cells of the
    /// scope among them, as a fork of another tenant's keeps what it inherited. Returns how many it
    /// forgot.
    pub fn forget(&self, scope: Scope) -> usize {
        let mut held = self.held.lock().unwrap();
        let before = held.rings.len();
        held.rings.retain(|_, ring| !ring.owner.is_some_and(|owner| scope.covers(owner)));
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

/// A cell's ring held by its host's ([`HostRing::hold`]): let go of when dropped.
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
/// none for a cell this host made and has not taken yet, whose first taking here it serves; and,
/// of one the KMS unwrapped, the revocation epoch it was asked in ([`Epoch`]): none for a KEK this
/// host made, which is no answer of the KMS.
type Takings = HashMap<(CellId, u32), (Option<(u64, u64)>, Unwrap, Option<Epoch>)>;

/// cladefs: when a call of a KMS for a tenant was asked (TASK-633.6's review): how many times the
/// control API had revoked the tenant, and the KMS itself had. An answer counts only if it comes
/// back in the epoch it was asked in: one asked before a revocation, answering after it, lifts
/// nothing; a KMS's own refusal discards no other KMS's answers.
type Epoch = (u64, u64);

/// How a host keeps its pools of new cell KEKs (doc-16, Forks and shared versions; TASK-411.4): a
/// tenant's refilled in batches, on `handle`, off the paths that take from it, once a take leaves
/// fewer than `low`, or an encrypted cell of the tenant opens.
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

/// A refill of a tenant's pool, going, which whoever waits for it shares ([`CachingKms::filled`]).
type Refill = Shared<BoxFuture<'static, ()>>;

/// A tenant's new cell KEKs, each wrapped by its KEK in the revocation epoch it carries
/// ([`Epoch`]), and the refill of them going, if one is.
#[derive(Default)]
struct Pool {
    keks: Vec<(Arc<Secret>, Wrapped, Epoch)>,
    filling: Option<Refill>,
}

/// cladefs: a tenant of a KMS (TASK-633.6): what a host keeps of a KMS's answers is kept by both.
type Tenancy = (KmsKey, u64);

/// The host's pools, by their KMSes' tenants, and how many times each one's was dropped, its era,
/// so that a refill going as its pool is dropped adds nothing to the next.
#[derive(Default)]
struct Pools {
    pools: HashMap<Tenancy, Pool>,
    eras: HashMap<Tenancy, u64>,
}

/// cladefs: a KMS a host asks, by its name (TASK-633.6): the URL a KMS is named by, or the bucket
/// a bucket KMS keeps its tenants' KEKs in. What a host keeps of a KMS's answers, its refusals and
/// its pools of new cell KEKs, is kept by it, so that a KMS refusing a tenant refuses it for the
/// cells whose KEKs that KMS wraps alone.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KmsKey(pub Arc<str>);

impl std::fmt::Display for KmsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// cladefs: the KMSes a host asks (TASK-633.6; backlog doc-23): the one wrapping the KEKs of the
/// cells of each bucket, by the bucket's number ([`crate::objectid::bucket_of`]), and its name; or
/// why it has none to be had, a refusal of each call for the bucket's cells, never a revocation.
pub trait Kmses: Send + Sync + 'static {
    fn of(&self, bucket: u16) -> BoxFuture<'_, Result<(KmsKey, kms::Shared), String>>;
}

/// cladefs: one KMS for the cells of every bucket: a host's of one bucket, and tests'.
pub struct OneKms(pub kms::Shared);

impl Kmses for OneKms {
    fn of(&self, _: u16) -> BoxFuture<'_, Result<(KmsKey, kms::Shared), String>> {
        futures::future::ready(Ok((KmsKey("one".into()), self.0.clone()))).boxed()
    }
}

/// A host's KMSes, and what it keeps of what it asked them: each cell's KEKs, by the taking of the
/// cell its object names, unwrapped once for all of it ([`CellKeys`]); a pool of new cell KEKs a
/// tenant of each KMS, wrapped by their tenants' KEKs ahead of the cells made with them; and the
/// tenants whose KEKs it last heard are revoked or destroyed, or none, by each KMS.
pub struct CachingKms {
    kmses: Arc<dyn Kmses>,
    /// Each bucket's KMS, as `kmses` named it: asked once a bucket (TASK-633.6).
    resolved: Mutex<HashMap<u16, (KmsKey, kms::Shared)>>,
    /// Each cell's KEKs unwrapped, by generation, and the taking they were unwrapped for: a
    /// taking since unwraps anew, so that a tenant whose KEK was revoked takes none of its cells
    /// again. Locked before the [`Ledger`], never after.
    takings: Mutex<Takings>,
    /// Its pools, refusals and the KEKs it gave out ([`Ledger`]).
    ledger: Mutex<Ledger>,
    /// How the pools are refilled: none, a KEK wrapped when one is wanted and its pool empty.
    pooling: Option<Pooling>,
    /// The cells it was told are shredded, whose KEKs it unwraps no more ([`Kms::forget`]).
    shredded: Mutex<BTreeSet<CellId>>,
}

/// cladefs: what a host keeps of its KMSes' answers, under one lock (TASK-633.6's review): so that
/// a revocation, which refuses a tenant, moves its epoch and drops its pools at once, and what
/// checks the refusal or the epoch and then adds to a pool, or gives out or takes back a KEK, are
/// each one step, none of them landing between a revocation's.
#[derive(Default)]
struct Ledger {
    /// New cell KEKs by their KMSes' tenants, wrapped, for the cells made.
    pools: Pools,
    /// The tenants whose KEKs are revoked or destroyed, or none, as last heard, and how it was
    /// said ([`Refused`]).
    refused: Refused,
    /// The new cell KEKs given out, by their wrapped bytes, each with the KMS's tenant it was
    /// wrapped for and the revocation epoch it was wrapped in: given back, or kept for a made
    /// cell's first taking, only if no revocation came since, though one lifted since. A
    /// revocation takes its tenant's off, as no longer to be given back or kept.
    issued: HashMap<Vec<u8>, (Tenancy, Epoch)>,
}

impl Ledger {
    /// Takes the new cell KEK `wrapped` off those given out: the KMS's tenant it was given out for,
    /// if it was wrapped in the revocation epoch of now and the tenant is not refused there. Every
    /// KEK made into a cell is given out here first ([`CachingKms::new_kek`],
    /// [`CachingKms::wrap`]): one not among them, taken off by a revocation since, is neither given
    /// back nor kept.
    fn issued_now(&mut self, wrapped: &Wrapped) -> Option<Tenancy> {
        let ((key, tenant), epoch) = self.issued.remove(&wrapped.0)?;
        let current = epoch == self.refused.epoch(&key, tenant)
            && self.refused.of(Some(&key), tenant).is_none();
        current.then_some((key, tenant))
    }

    /// Drops every pool of `tenant`'s, of the KMSes named `keys` and any other, a refill of one
    /// going adding nothing to the next.
    fn drop_pools(&mut self, tenant: u64, keys: impl IntoIterator<Item = KmsKey>) {
        let mut tenancies: BTreeSet<Tenancy> = keys.into_iter().map(|key| (key, tenant)).collect();
        tenancies.extend(self.pools.pools.keys().filter(|(_, of)| *of == tenant).cloned());
        for tenancy in tenancies {
            self.pools.pools.remove(&tenancy);
            *self.pools.eras.entry(tenancy).or_default() += 1;
        }
    }
}

/// cladefs: the tenants whose KEKs are refused (TASK-633.6): by a KMS, as it said, until it answers
/// a call for one again; and by every KMS, as the control API said ([`CachingKms::revoked`]), until
/// each answers a call for one again: so that a KMS the host has not asked since, whose cells'
/// keys a fork would inherit, still refuses it.
#[derive(Default)]
struct Refused {
    by: HashMap<Tenancy, String>,
    /// The control API's: why, and the KMSes that answered since.
    told: HashMap<u64, (String, BTreeSet<KmsKey>)>,
    /// How many times the control API revoked each tenant, and each KMS did ([`Epoch`]).
    told_epochs: HashMap<u64, u64>,
    by_epochs: HashMap<Tenancy, u64>,
}

impl Refused {
    /// The revocation epoch of `tenant` in the KMS named `key` now.
    fn epoch(&self, key: &KmsKey, tenant: u64) -> Epoch {
        let told = self.told_epochs.get(&tenant).copied().unwrap_or(0);
        (told, self.by_epochs.get(&(key.clone(), tenant)).copied().unwrap_or(0))
    }

    /// Whether `tenant` is refused by any KMS, or as the control API said.
    fn anywhere(&self, tenant: u64) -> bool {
        self.told.contains_key(&tenant) || self.by.keys().any(|(_, of)| *of == tenant)
    }

    /// Why `tenant`'s keys are refused by the KMS named `key`, or by one not asked for (none).
    fn of(&self, key: Option<&KmsKey>, tenant: u64) -> Option<String> {
        let by = key.and_then(|key| self.by.get(&(key.clone(), tenant)));
        let told = self
            .told
            .get(&tenant)
            .filter(|(_, answered)| key.is_none_or(|key| !answered.contains(key)));
        by.or(told.map(|(why, _)| why)).cloned()
    }
}

/// The error of a KMS's refusal, naming its tenant.
fn refused(error: kms::Error) -> Error {
    Error::Kms(error.to_string())
}

/// Whether `error` says its tenant's KEK is revoked or destroyed; or that the KMS has none of it,
/// as destroyed (TASK-411.28): a real KMS forgets a deleted key, answering as for one never made,
/// and no cell is made in the clear for want of one.
fn revokes(error: &kms::Error) -> bool {
    matches!(
        error,
        kms::Error::Revoked { .. } | kms::Error::Destroyed { .. } | kms::Error::NoSuchTenant { .. }
    )
}

impl CachingKms {
    /// The host's over `kms`, for every bucket's cells, its pools refilled a KEK at a time, as one
    /// is wanted: tests', which count the calls a path makes.
    pub fn new(kms: kms::Shared) -> Arc<Self> {
        Self::over(Arc::new(OneKms(kms)), None)
    }

    /// The host's over `kms`, for every bucket's cells, its pools refilled as `pooling` says.
    pub fn pooled(kms: kms::Shared, pooling: Pooling) -> Arc<Self> {
        Self::over(Arc::new(OneKms(kms)), Some(pooling))
    }

    /// cladefs: the host's over `kmses`, a KMS a bucket (TASK-633.6), its pools refilled as
    /// `pooling` says, if given; a KEK at a time, as one is wanted, if not.
    pub fn over(kmses: Arc<dyn Kmses>, pooling: Option<Pooling>) -> Arc<Self> {
        Arc::new(Self {
            kmses,
            resolved: Mutex::default(),
            takings: Mutex::default(),
            ledger: Mutex::default(),
            pooling,
            shredded: Mutex::default(),
        })
    }

    /// The KMSes it asks.
    pub fn kmses(&self) -> &Arc<dyn Kmses> {
        &self.kmses
    }

    /// The KMS of `cell`'s bucket, and its name: asked of [`Kmses`] once a bucket. None to be had
    /// refuses the call, naming the tenant and the bucket.
    async fn kms_of(&self, cell: CellId) -> Result<(KmsKey, kms::Shared), Error> {
        let bucket = crate::objectid::bucket_of(cell.id);
        if let Some(found) = self.resolved.lock().unwrap().get(&bucket) {
            return Ok(found.clone());
        }
        let found = self.kmses.of(bucket).await.map_err(|why| {
            Error::Kms(format!("tenant {}: the KMS of bucket {bucket}: {why}", cell.tenant))
        })?;
        Ok(self.resolved.lock().unwrap().entry(bucket).or_insert(found).clone())
    }

    /// The name of the KMS of `cell`'s bucket, if asked for.
    fn key_of(&self, cell: CellId) -> Option<KmsKey> {
        let resolved = self.resolved.lock().unwrap();
        resolved.get(&crate::objectid::bucket_of(cell.id)).map(|(key, _)| key.clone())
    }

    /// Every KMS asked for so far, once each.
    fn known(&self) -> BTreeMap<KmsKey, kms::Shared> {
        self.resolved.lock().unwrap().values().cloned().collect()
    }

    /// Why `tenant`'s keys are refused by the KMS named `key`, or by one not asked for (none), if
    /// they are, as last heard.
    fn refusal(&self, key: Option<&KmsKey>, tenant: u64) -> Option<String> {
        self.ledger.lock().unwrap().refused.of(key, tenant)
    }

    /// What `kms`, named `key`, answered a call for `tenant`: noted, a refusal saying its KEK is
    /// revoked or destroyed, or none, dropping what it keeps of the tenant there
    /// ([`Kms::revoked`]), an answer forgetting that it was.
    fn heard<T>(
        &self,
        (key, kms): (&KmsKey, &kms::Shared),
        (tenant, asked): (u64, Epoch),
        answer: Result<T, kms::Error>,
    ) -> Result<T, kms::Error> {
        match &answer {
            Ok(_) => self.answered(key, tenant, asked),
            Err(error) if revokes(error) => self.revoked_at((key, kms), tenant, error.to_string()),
            Err(_) => {}
        }
        answer
    }

    /// The revocation epoch of `tenant` in the KMS named `key` now: what a call of it asked now is
    /// answered in.
    fn epoch(&self, key: &KmsKey, tenant: u64) -> Epoch {
        self.ledger.lock().unwrap().refused.epoch(key, tenant)
    }

    /// The KMS named `key` answered a call for `tenant`, asked in its revocation epoch `asked`:
    /// its KEK is not refused there, whatever was heard before; unless it was revoked since the
    /// call was asked, the answer then of before. Refused by every KMS, as the control API said,
    /// it stays so by every other, those not asked for since among them.
    fn answered(&self, key: &KmsKey, tenant: u64, asked: Epoch) {
        let mut ledger = self.ledger.lock().unwrap();
        let refused = &mut ledger.refused;
        if refused.epoch(key, tenant) != asked {
            return;
        }
        refused.by.remove(&(key.clone(), tenant));
        if let Some((_, answered)) = refused.told.get_mut(&tenant) {
            answered.insert(key.clone());
        }
    }

    /// `tenant`'s KEK is revoked or destroyed, or none, in `kms`, named `key`, as `why` says: the
    /// KMS said so (doc-16, Forks and shared versions). Its pool of that KMS is dropped, a refill
    /// of it going adding nothing, so that it makes no cell more there; the KEKs unwrapped of its
    /// cells whose KEKs that KMS wraps are forgotten, so that no taking of them goes on unasked,
    /// and what the KMS keeps of its KEK (`kms::Kms::forget`); and no fork inherits keys of those
    /// cells ([`inheriting`]), until the KMS answers a call for it again. What other KMSes
    /// answered for the tenant, for other buckets, stands (TASK-633.6).
    fn revoked_at(&self, (key, kms): (&KmsKey, &kms::Shared), tenant: u64, why: String) {
        // Refused, its epoch moved and its pool dropped in one step ([`Ledger`]); then its takings
        // dropped, what a made cell's keeping checks under their lock first (TASK-633.6's review).
        {
            let mut ledger = self.ledger.lock().unwrap();
            ledger.refused.by.insert((key.clone(), tenant), why);
            *ledger.refused.by_epochs.entry((key.clone(), tenant)).or_default() += 1;
            let tenancy = (key.clone(), tenant);
            ledger.pools.pools.remove(&tenancy);
            ledger.issued.retain(|_, (of, _)| *of != tenancy);
            *ledger.pools.eras.entry(tenancy).or_default() += 1;
        }
        // What the KMS itself keeps of the tenant's KEK: the bucket KMS's (TASK-411.27).
        kms.forget_dyn(tenant);
        let buckets: BTreeSet<u16> = (self.resolved.lock().unwrap().iter())
            .filter(|(_, (of, _))| of == key)
            .map(|(bucket, _)| *bucket)
            .collect();
        self.takings.lock().unwrap().retain(|(cell, _), _| {
            cell.tenant != tenant || !buckets.contains(&crate::objectid::bucket_of(cell.id))
        });
    }

    /// `tenant`'s KEK is revoked or destroyed, or none, as `why` says, in every KMS: the control
    /// API says so (doc-16, Forks and shared versions). As [`CachingKms::revoked_at`], of every
    /// KMS, until one answers a call for it again.
    pub fn revoked(&self, tenant: u64, why: impl Into<String>) {
        let known = self.known();
        // In one step, then the takings, as by [`CachingKms::revoked_at`].
        {
            let mut ledger = self.ledger.lock().unwrap();
            ledger.refused.told.insert(tenant, (why.into(), BTreeSet::new()));
            *ledger.refused.told_epochs.entry(tenant).or_default() += 1;
            ledger.drop_pools(tenant, known.keys().cloned());
            ledger.issued.retain(|_, ((_, of), _)| *of != tenant);
        }
        for kms in known.values() {
            kms.forget_dyn(tenant);
        }
        self.takings.lock().unwrap().retain(|(cell, _), _| cell.tenant != tenant);
    }

    /// Forgets what it holds of `scope`, as a host told so does (TASK-411.7): a tenant's, its KEK
    /// revoked or destroyed, as `why` says ([`CachingKms::revoked`]); a cell's, shredded, its KEKs
    /// unwrapped dropped and refused from then on, whatever object of the cell its asker read
    /// before ([`shredded`]).
    pub fn forget(&self, scope: Scope, why: impl Into<String>) {
        match scope {
            Scope::Tenant(tenant) => self.revoked(tenant, why),
            Scope::Cell(cell) => {
                self.shredded.lock().unwrap().insert(cell);
                self.takings.lock().unwrap().retain(|(of, _), _| *of != cell);
            }
        }
    }

    /// Why the keys of `cell`'s tenant are refused, if its KEK is revoked or destroyed in the KMS
    /// of `cell`'s bucket, as last heard; or in every KMS, as the control API said.
    pub fn refuses(&self, cell: CellId) -> Option<Error> {
        self.refusal(self.key_of(cell).as_ref(), cell.tenant).map(Error::Kms)
    }

    /// cladefs: as [`CachingKms::refuses`], the KMS of `cell`'s bucket looked up first, if it is
    /// refused at all (TASK-633.6's review): so that what that KMS answered since a revocation the
    /// control API told counts for the cells of a bucket this host takes no KEK of, whose keys
    /// forks inherit, as it does for the buckets it does. Nothing is asked of the KMS: a revocation
    /// stops the fork it exists to stop until the KMS answers another call. Refused by none,
    /// nothing is looked up, as forks go; a KMS not to be looked up leaves the refusal as it
    /// stands.
    async fn refuses_resolving(&self, cell: CellId) -> Option<Error> {
        if self.key_of(cell).is_some() {
            return self.refuses(cell);
        }
        // Not looked up: refused by a KMS that may be its bucket's, or as the control API said.
        if !self.ledger.lock().unwrap().refused.anywhere(cell.tenant) {
            return None;
        }
        match self.kms_of(cell).await {
            Ok((key, _)) => self.refusal(Some(&key), cell.tenant).map(Error::Kms),
            // Its KMS not to be looked up: refused as the control API said, if it did.
            Err(_) => self.refusal(None, cell.tenant).map(Error::Kms),
        }
    }

    /// A new KEK for `cell`, and it wrapped by its tenant's KEK in the KMS of its bucket, from the
    /// pool, or wrapped by the KMS now. Any refusal fails it, naming the tenant: a tenant whose
    /// KEK is revoked or destroyed, or that the KMS has none of, makes no cell (TASK-411.28: none
    /// is made in the clear). A take leaving the pool low refills it, off this path ([`Pooling`]).
    /// Takes asking at once, before the KMS has said, ask it each: a KEK wrapped is the one cell's
    /// it makes, which no other take may share.
    pub async fn new_kek(self: &Arc<Self>, cell: CellId) -> Result<(Arc<Secret>, Wrapped), Error> {
        let (key, kms) = self.kms_of(cell).await?;
        let tenancy = (key.clone(), cell.tenant);
        // From the pool, in one step with giving it out: none of a tenant refused there, whose new
        // KEK the KMS is asked for (TASK-633.6's review).
        let pooled = {
            let mut ledger = self.ledger.lock().unwrap();
            let pooled = match ledger.refused.of(Some(&key), cell.tenant) {
                Some(_) => None,
                None => ledger.pools.pools.get_mut(&tenancy).and_then(|pool| pool.keks.pop()),
            };
            pooled.map(|(secret, wrapped, epoch)| {
                ledger.issued.insert(wrapped.0.clone(), (tenancy.clone(), epoch));
                (secret, wrapped)
            })
        };
        if let Some(kek) = pooled {
            self.refill_at((&key, &kms), cell.tenant);
            return Ok(kek);
        }
        let kek = Secret::random();
        let asked = self.epoch(&key, cell.tenant);
        let answer = kms.wrap_dyn(cell.tenant, &kek).await;
        let wrapped = self.heard((&key, &kms), (cell.tenant, asked), answer).map_err(refused)?;
        self.ledger.lock().unwrap().issued.insert(wrapped.0.clone(), (tenancy, asked));
        self.refill_at((&key, &kms), cell.tenant);
        Ok((Arc::new(kek), wrapped))
    }

    /// Gives back `kek`, a new KEK for `cell` that made no cell, to the pool it came from: unless
    /// the pool was dropped since, the tenant's KEK revoked, the KEK then dropped with it.
    ///
    /// cladefs: nor if it was given out before a revocation, though one lifted since (TASK-633.6's
    /// review): checked and given back in one step ([`Ledger`]).
    pub fn give_back(&self, cell: CellId, (secret, wrapped): (Arc<Secret>, Wrapped)) {
        let _ = cell;
        let mut ledger = self.ledger.lock().unwrap();
        let Some(tenancy) = ledger.issued_now(&wrapped) else { return };
        let epoch = ledger.refused.epoch(&tenancy.0, tenancy.1);
        ledger.pools.pools.entry(tenancy).or_default().keks.push((secret, wrapped, epoch));
    }

    /// cladefs: takes `wrapped`, a KEK given out that made no cell and goes back to no pool (a
    /// rotation's that failed), off those given out.
    fn abandoned(&self, wrapped: &Wrapped) {
        self.ledger.lock().unwrap().issued.remove(&wrapped.0);
    }

    /// How many new cell KEKs of `tenant`'s its pools hold, of every KMS.
    pub fn in_pool(&self, tenant: u64) -> usize {
        let ledger = self.ledger.lock().unwrap();
        (ledger.pools.pools.iter())
            .filter(|((_, of), _)| *of == tenant)
            .map(|(_, pool)| pool.keks.len())
            .sum()
    }

    /// Refills the pool of `cell`'s tenant of the KMS of its bucket, if it holds fewer than
    /// [`Pooling::low`] and no refill goes: a batch of new KEKs wrapped at once on the pooling's
    /// runtime, which this returns before; the KMS asked for there first, if it was not.
    /// Without pooling, nothing; nor for a tenant whose KEK was last heard revoked or destroyed
    /// there, until the KMS answers a call for it again.
    pub fn refill(self: &Arc<Self>, cell: CellId) {
        let Some(pooling) = &self.pooling else { return };
        let resolved =
            self.resolved.lock().unwrap().get(&crate::objectid::bucket_of(cell.id)).cloned();
        match resolved {
            Some((key, kms)) => self.refill_at((&key, &kms), cell.tenant),
            None => {
                let this = self.clone();
                pooling.handle.spawn(async move {
                    match this.kms_of(cell).await {
                        Ok(_) => this.refill(cell),
                        Err(error) => log::warn!("cladefs: refilling a pool of KEKs: {error}"),
                    }
                });
            }
        }
    }

    /// Refills `tenant`'s pool of `kms`, named `key` ([`CachingKms::refill`]).
    fn refill_at(self: &Arc<Self>, (key, kms): (&KmsKey, &kms::Shared), tenant: u64) {
        let Some(pooling) = &self.pooling else { return };
        let tenancy = (key.clone(), tenant);
        let refill = {
            // Checked in one step with the refill's start ([`Ledger`]).
            let mut ledger = self.ledger.lock().unwrap();
            if ledger.refused.of(Some(key), tenant).is_some() {
                return;
            }
            let asked = ledger.refused.epoch(key, tenant);
            let pools = &mut ledger.pools;
            let era = pools.eras.get(&tenancy).copied().unwrap_or(0);
            let pool = pools.pools.entry(tenancy.clone()).or_default();
            if pool.keks.len() >= pooling.low || pool.filling.is_some() {
                return;
            }
            let fill =
                self.clone().fill((key.clone(), kms.clone()), (tenant, asked), era, pooling.batch);
            let refill: Refill = fill.boxed().shared();
            pool.filling = Some(refill.clone());
            refill
        };
        pooling.handle.spawn(refill);
    }

    /// Returns once the refills of `tenant`'s pools going, of every KMS, are done.
    pub fn filled(&self, tenant: u64) -> impl std::future::Future<Output = ()> + use<> {
        let ledger = self.ledger.lock().unwrap();
        let filling: Vec<Refill> = (ledger.pools.pools.iter())
            .filter(|((_, of), _)| *of == tenant)
            .filter_map(|(_, pool)| pool.filling.clone())
            .collect();
        futures::future::join_all(filling).map(drop)
    }

    /// Wraps `batch` new KEKs of `tenant`'s at once by `kms`, named `key`, into its pool there of
    /// era `era`: dropped since, they go with it. A refusal saying the tenant's KEK is revoked or
    /// destroyed, or none, drops its pool ([`Kms::revoked`]); any other refusal is logged, the
    /// next take refilling it again.
    async fn fill(
        self: Arc<Self>,
        (key, kms): (KmsKey, kms::Shared),
        (tenant, asked): (u64, Epoch),
        era: u64,
        batch: usize,
    ) {
        let wraps = (0..batch).map(|_| {
            let kms = kms.clone();
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
            Some(error) if revokes(&error) => {
                self.revoked_at((&key, &kms), tenant, error.to_string())
            }
            Some(error) => log::warn!("cladefs: refilling tenant {tenant}'s pool of KEKs: {error}"),
            None => self.answered(&key, tenant, asked),
        }
        let mut ledger = self.ledger.lock().unwrap();
        // Revoked since it was asked, though lifted since: none of them pooled.
        let current = ledger.refused.epoch(&key, tenant) == asked;
        let tenancy = (key, tenant);
        if ledger.pools.eras.get(&tenancy).copied().unwrap_or(0) != era {
            return;
        }
        let pool = ledger.pools.pools.entry(tenancy).or_default();
        pool.filling = None;
        if current {
            pool.keks.extend(wrapped.into_iter().map(|(secret, kek)| (secret, kek, asked)));
        }
    }

    /// `secret` wrapped by the KEK of `cell`'s tenant in the KMS of its bucket: a new generation
    /// of the cell's KEK ([`rotate`]). Refused as the KMS refuses it.
    ///
    /// cladefs: recorded as given out in the revocation epoch it was asked in, as a new cell KEK
    /// is ([`Ledger::issued`]): kept for the cell's first taking only if no revocation came since.
    async fn wrap(&self, cell: CellId, secret: &Secret) -> Result<Wrapped, Error> {
        let (key, kms) = self.kms_of(cell).await?;
        let asked = self.epoch(&key, cell.tenant);
        // Heard as every call of the KMS is: a revocation it says is noted.
        let answer = kms.wrap_dyn(cell.tenant, secret).await;
        let wrapped = self.heard((&key, &kms), (cell.tenant, asked), answer).map_err(refused)?;
        let tenancy = (key, cell.tenant);
        self.ledger.lock().unwrap().issued.insert(wrapped.0.clone(), (tenancy, asked));
        Ok(wrapped)
    }

    /// KEK generation `kek` of `cell`, made by this host with that KEK, `secret`: kept for the
    /// taking `taking`, its epoch and holder, unwrapped by none; or, none given, for the first
    /// taking of it here.
    ///
    /// cladefs: none kept of a tenant refused now (TASK-633.6's review): a revocation landing while
    /// the cell was made, from a KEK of the pool, unwraps it again at its first taking, asking the
    /// KMS, rather than taking it on a KEK no KMS was asked for since.
    fn made(&self, cell: CellId, taking: Option<(u64, u64)>, kek: &Kek, secret: Arc<Secret>) {
        // Under the takings' lock, then the ledger's: a revocation moves the epoch, then drops its
        // takings, so this either sees it, or keeps what it drops after.
        let mut takings = self.takings.lock().unwrap();
        if self.ledger.lock().unwrap().issued_now(&kek.wrapped).is_none() {
            return;
        }
        let ready: Unwrap = futures::future::ready(Ok(secret)).boxed().shared();
        takings.insert((cell, kek.generation), (taking, ready, None));
    }

    /// KEK generation `generation` of `cell`, of `keks`, its object's: unwrapped by the KMS of its
    /// bucket once for the taking `taking`, however many ask; or none, this host having made it,
    /// and this its first taking here.
    async fn kek(
        &self,
        cell: CellId,
        (keks, taking): (&[Kek], (u64, u64)),
        generation: u32,
    ) -> Result<Arc<Secret>, Error> {
        if self.shredded.lock().unwrap().contains(&cell) {
            return Err(shredded(cell));
        }
        let (key, kms) = self.kms_of(cell).await?;
        let (unwrap, asked) = {
            let mut takings = self.takings.lock().unwrap();
            // Under the takings' lock, then the ledger's, as by [`CachingKms::made`]: a revocation
            // moves the epoch, then drops its takings, so the entry made here either carries the
            // epoch after it or goes with them.
            let now = self.ledger.lock().unwrap().refused.epoch(&key, cell.tenant);
            match takings.get_mut(&(cell, generation)) {
                Some((at, unwrap, asked)) if *at == Some(taking) => (unwrap.clone(), *asked),
                Some((at @ None, unwrap, asked)) => {
                    *at = Some(taking);
                    (unwrap.clone(), *asked)
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
                    let kms = kms.clone();
                    let asked = now;
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
                    takings.insert((cell, generation), (Some(taking), unwrap.clone(), Some(asked)));
                    (unwrap, Some(asked))
                }
            }
        };
        match unwrap.await {
            Ok(kek) => {
                // A KEK this host made is no answer of the KMS's.
                if let Some(asked) = asked {
                    self.answered(&key, cell.tenant, asked);
                }
                Ok(kek)
            }
            Err((error, revokes)) => {
                // Asked anew the next time: a tenant's KEK revoked may be given back.
                let mut takings = self.takings.lock().unwrap();
                if takings.get(&(cell, generation)).is_some_and(|(at, ..)| *at == Some(taking)) {
                    takings.remove(&(cell, generation));
                }
                drop(takings);
                if revokes {
                    self.revoked_at((&key, &kms), cell.tenant, error.to_string());
                }
                Err(error)
            }
        }
    }
}

/// The keys of a cell as a host took it, or read it: its head's KEKs, unwrapped as they are
/// asked for ([`CellKeys::kek`]), once a taking, by the host's KMS.
pub struct CellKeys {
    cell: CellId,
    keks: Vec<Kek>,
    /// The taking they are unwrapped for: its epoch and holder.
    taking: (u64, u64),
    kms: Arc<CachingKms>,
    /// Whether its head says it is shredded ([`shred`], TASK-411.7).
    shredded: bool,
}

impl CellKeys {
    /// The keys of `cell`, whose head, as taken, or read, is `head`, unwrapped by `kms`, the
    /// host's, for the head's taking.
    pub fn of(cell: CellId, head: &Head, kms: Arc<CachingKms>) -> Self {
        Self::taken(cell, head, (head.epoch, head.holder), kms)
    }

    /// The keys of `cell`, whose head, as read, is `head`, unwrapped by `kms` for the taking
    /// `holder` makes of it next ([`crate::head::take`]). Read before the cell is taken, as a
    /// writer's recovery is, they are those it is taken with then, unwrapped once.
    pub fn to_take(cell: CellId, head: &Head, kms: Arc<CachingKms>, holder: u64) -> Self {
        Self::taken(cell, head, (head.epoch + 1, holder), kms)
    }

    /// The keys of `cell`, which has no head: none, nothing writing it. Not a cell in the clear:
    /// none is (TASK-411.28).
    pub fn headless(cell: CellId, kms: Arc<CachingKms>) -> Self {
        Self { cell, keks: Vec::new(), taking: (0, 0), kms, shredded: false }
    }

    fn taken(cell: CellId, head: &Head, taking: (u64, u64), kms: Arc<CachingKms>) -> Self {
        Self { cell, keks: head.keks.clone(), taking, kms, shredded: head.shredded }
    }

    /// The cell's.
    pub fn cell(&self) -> CellId {
        self.cell
    }

    /// Whether the cell is encrypted: its head keeps a KEK, or it was shredded, its KEKs refused
    /// then ([`CellKeys::kek`]). Every cell's head is so once taken; one with none was never
    /// taken, nothing writing it.
    pub fn encrypted(&self) -> bool {
        !self.keks.is_empty() || self.shredded
    }

    /// The newest generation of its KEK, if it has one.
    pub fn newest(&self) -> Option<u32> {
        self.keks.iter().map(|kek| kek.generation).max()
    }

    /// Its KEK of generation `generation`, unwrapped: refused as the KMS refuses it, naming the
    /// cell's tenant; and if the cell is shredded, naming it.
    pub async fn kek(&self, generation: u32) -> Result<Arc<Secret>, Error> {
        if self.shredded {
            return Err(shredded(self.cell));
        }
        self.kms.kek(self.cell, (&self.keks, self.taking), generation).await
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

    /// Its newest KEK, unwrapped, and its generation: refused for a shredded cell's, and for one
    /// with none, never taken, which nothing writes under.
    async fn newest_kek(&self) -> Result<(Arc<Secret>, u32), Error> {
        if self.shredded {
            return Err(shredded(self.cell));
        }
        let newest = self.newest().ok_or_else(|| {
            Error::Corrupt(format!(
                "cell {} keeps no KEK, which a taken cell's head does",
                self.cell
            ))
        })?;
        Ok((self.kek(newest).await?, newest))
    }

    /// A new DEK of generation `generation` for its cell, wrapped under its newest KEK.
    async fn new_dek(&self, generation: u32) -> Result<Dek, Error> {
        let (kek, newest) = self.newest_kek().await?;
        Ok(wrap_dek((&kek, newest), self.cell, generation, &Secret::random()))
    }
}

/// The refusal of a shredded cell, `cell`: as a KMS's refusal, naming its tenant, its cells
/// opening no more (`EACCES`), and the collector leaving its encrypted objects uncompacted.
pub fn shredded(cell: CellId) -> Error {
    Error::Kms(format!("tenant {}: cell {cell} is shredded, its KEK gone", cell.tenant))
}

/// cladefs: shreds `cell` (doc-16, Shredding; TASK-411.7): takes every generation of its KEK out of
/// its head and marks it shredded, by one conditional update, whoever holds it: its DEKs, wrapped
/// by those KEKs, unwrap no more, and no one takes it again ([`crate::head::take`]); its writer, if
/// any, fails at its next swap. A host that unwrapped its KEK before keeps it until told
/// ([`CachingKms::forget`]). Shredded already, nothing changes. Returns how many generations of
/// its KEK it took out: [`Error::NoCell`] if it has no head.
///
/// Only as strong as the bucket keeps no older version of the cell's head: with versioning on, a
/// lifecycle rule must expire non-current versions, or theirs unwrap while the tenant's KEK lives.
pub async fn shred(bucket: &impl Bucket, cell: CellId) -> Result<usize, Error> {
    lifecycle::change(bucket, cell, Start::Read, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.shredded => Ok(Step::Stop(0)),
        Some(found) => {
            let removed = found.keks.len();
            Ok(Step::Write(Head { keks: Vec::new(), shredded: true, ..found }, removed))
        }
    })
    .await
}

/// The KEK a cell made by a host gets: one the host wrapped ahead, by its KMS
/// ([`CachingKms::new_kek`]), refused as the KMS refuses it, naming the tenant: a tenant the KMS
/// says has no KEK among them, as destroyed (TASK-411.28: no cell is made in the clear). What every
/// maker of a cell asks before making it: a cell's opening, a fork into a new cell
/// ([`inheriting`]), the control API's attaching.
pub async fn new_cell_kek(
    kms: &Arc<CachingKms>,
    cell: CellId,
) -> Result<(Arc<Secret>, Kek), Error> {
    let (secret, wrapped) = kms.new_kek(cell).await?;
    Ok((secret, Kek { generation: FIRST, wrapped }))
}

/// cladefs: makes `cell`, a root of `owner`'s, never taken, its KEK from `kms`'s pool, the host's
/// ([`new_cell_kek`]): what a host making a new cell does before its cell first opens, so that a
/// tenant the KMS refuses gets no cell, nothing made (TASK-411.28), and the cell's first taking
/// here unwraps nothing. Returns whether it made it: a head there already, whoever's, is left as it
/// is, the KEK given back.
pub async fn make(
    bucket: &impl Bucket,
    kms: &Arc<CachingKms>,
    cell: CellId,
    owner: u64,
) -> Result<bool, Error> {
    let (secret, kek) = new_cell_kek(kms, cell).await?;
    let head = Head { keks: vec![kek.clone()], ..Head::of(crate::record::Made::Root, owner) };
    let made = lifecycle::change(bucket, cell, Start::Create, |found| match found {
        None => Ok(Step::Write(head.clone(), true)),
        Some(_) => Ok(Step::Stop(false)),
    })
    .await;
    match made {
        Ok(true) => kms.made(cell, None, &kek, secret),
        _ => kms.give_back(cell, (secret, kek.wrapped)),
    }
    made
}

/// Takes `cell` as `taking` says ([`head::take`]), from `read`, what a read of its head found,
/// giving it what keys it lacks in the same write ([`head::Given`]): a KEK from `kms`'s pool if it
/// has none, a new cell's, never taken; its first DEK, and those it inherits from ancestors of
/// other cells rewrapped under its KEK, if it has none ([`Inherited`]): read by crossing into their
/// cells, their KEKs unwrapped by the KMS ([`ring`]). A fork's host gives them as it makes the fork
/// ([`inheriting`]), so that its child's first taking gives none. Its keys changed meanwhile by
/// another's taking, made anew and taken again. Returns the head as taken, and its keys.
pub async fn taking(
    bucket: &impl Bucket,
    kms: &Arc<CachingKms>,
    cell: CellId,
    mut read: head::Read,
    taking: head::Taking,
) -> Result<(head::Held, CellKeys), Error> {
    loop {
        let basis = read.head(cell)?.ok_or(Error::NoCell)?;
        let kek = match basis.keks.is_empty() {
            true => Some(new_cell_kek(kms, cell).await?),
            false => None,
        };
        let given = match giving(bucket, kms, cell, &basis, &taking, kek.clone()).await {
            Ok(given) => given,
            Err(err) => return Err(given_back(kms, cell, kek, err)),
        };
        let taking = head::Taking { keys: given, ..taking.clone() };
        match head::take(bucket, cell, read, &taking).await {
            Ok(held) => return Ok((held.clone(), kept(kms, cell, &held.head, kek))),
            Err(Error::Raced) => {
                drop(given_back(kms, cell, kek, Error::Raced));
                read = head::read(bucket, cell).await?;
            }
            Err(err) => return Err(given_back(kms, cell, kek, err)),
        }
    }
}

/// `err`, `kek` given back to `kms`'s pool, unused.
fn given_back(
    kms: &Arc<CachingKms>,
    cell: CellId,
    kek: Option<(Arc<Secret>, Kek)>,
    err: Error,
) -> Error {
    if let Some((secret, kek)) = kek {
        kms.give_back(cell, (secret, kek.wrapped));
    }
    err
}

/// The keys a taking of `cell`, by `taking`, gives its head `head`, as found ([`head::Given`]):
/// `kek` if it has none, its first DEK under its newest KEK, and those it inherits, if it has
/// none. None if it lacks nothing.
async fn giving(
    bucket: &impl Bucket,
    kms: &Arc<CachingKms>,
    cell: CellId,
    head: &Head,
    taking: &head::Taking,
    kek: Option<(Arc<Secret>, Kek)>,
) -> Result<Option<head::Given>, Error> {
    if kek.is_none() && !head.deks.is_empty() {
        return Ok(None);
    }
    let deks = match head.deks.is_empty() {
        false => None,
        true => {
            let keys = CellKeys::to_take(cell, head, kms.clone(), taking.holder);
            let (secret, newest) = match &kek {
                Some((secret, kek)) => (secret.clone(), kek.generation),
                None => keys.newest_kek().await?,
            };
            let source = ring(bucket, &keys, cell, head, false).await?;
            let inherited = inherit(&source, cell, (&secret, newest));
            Some((vec![wrap_dek((&secret, newest), cell, FIRST, &Secret::random())], inherited))
        }
    };
    Ok(Some(head::Given { kek: kek.map(|(_, kek)| kek), deks }))
}

/// What a taking that may have given its cell `kek`, the KEK it would have given it, did with it:
/// kept for the taking if the cell's head took it, and given back to the pool otherwise. Returns
/// the cell's keys, for that taking.
pub fn kept(
    kms: &Arc<CachingKms>,
    cell: CellId,
    head: &Head,
    kek: Option<(Arc<Secret>, Kek)>,
) -> CellKeys {
    if let Some((secret, kek)) = kek {
        match head.keks == [kek.clone()] {
            true => kms.made(cell, Some((head.epoch, head.holder)), &kek, secret),
            false => kms.give_back(cell, (secret, kek.wrapped)),
        }
    }
    CellKeys::of(cell, head, kms.clone())
}

/// Adds a generation of `cell`'s DEK, the next, by a conditional update of its head, `keys` its
/// KEKs: new objects are encrypted under it from the next taking on, the others staying readable
/// under theirs; its writer, if any, fails at its next swap. Returns the generation.
pub async fn add_generation(
    bucket: &impl Bucket,
    keys: &CellKeys,
    cell: CellId,
) -> Result<u32, Error> {
    let found = head::get(bucket, cell).await?.ok_or(Error::NoCell)?;
    let mut generation = next_generation(&found.deks);
    let mut dek = keys.new_dek(generation).await?;
    loop {
        let added = lifecycle::change(bucket, cell, Start::Read, |found| match found {
            None => Err(Error::NoCell),
            Some(found) if next_generation(&found.deks) != generation => {
                Ok(Step::Stop(Some(next_generation(&found.deks))))
            }
            Some(mut found) => {
                found.deks.push(dek.clone());
                Ok(Step::Write(found, None))
            }
        })
        .await?;
        let Some(next) = added else { return Ok(generation) };
        // Another added one meanwhile: the next after it.
        generation = next;
        dek = keys.new_dek(generation).await?;
    }
}

/// The generation after the newest of `deks`.
fn next_generation(deks: &[Dek]) -> u32 {
    deks.iter().map(|dek| dek.generation + 1).max().unwrap_or(FIRST)
}

/// Rotates `cell`'s KEK: a new generation of it, wrapped by its tenant's KEK, added to its head,
/// and its DEKs, its own and those it inherits, rewrapped under it, in one conditional update; its
/// writer, if any, fails at its next swap. Older generations stay in its head. Returns the new
/// generation and whether its DEKs were rewrapped.
pub async fn rotate(
    bucket: &impl Bucket,
    kms: &Arc<CachingKms>,
    cell: CellId,
) -> Result<(u32, bool), Error> {
    let found = head::get(bucket, cell).await?.ok_or(Error::NoCell)?;
    let old = CellKeys::of(cell, &found, kms.clone());
    let Some(newest) = old.newest() else {
        return Err(Error::Corrupt(format!("cell {cell} keeps no KEK, which a taken cell does")));
    };
    let generation = newest + 1;
    let secret = Secret::random();
    // Every generation of the cell's KEK its DEKs may be under, unwrapped first: the update's
    // step, which may run again on what it finds, unwraps nothing.
    let inherited = found.inherited.iter().map(|had| &had.dek);
    let keks = old.keks(found.deks.iter().chain(inherited).map(|dek| dek.kek)).await?;
    let wrapped = kms.wrap(cell, &secret).await?;
    let kek = Kek { generation, wrapped };
    let kek_of = |dek: &Dek| {
        keks.get(&dek.kek)
            .ok_or_else(|| Error::Corrupt(format!("cell {cell}'s DEK under a KEK not read")))
    };
    let rewrapped = lifecycle::change(bucket, cell, Start::Read, |found| {
        let mut found = found.ok_or(Error::NoCell)?;
        if found.shredded {
            return Err(shredded(cell));
        }
        if found.keks.iter().any(|had| had.generation >= generation) {
            return Err(Error::Corrupt(format!(
                "cell {cell} has a KEK of generation {generation} or later"
            )));
        }
        let rewrap = |dek: &Dek| {
            let was = unwrap_dek(kek_of(dek)?, cell, dek)?;
            Ok::<_, Error>(wrap_dek((&secret, generation), cell, dek.generation, &was))
        };
        let rewrap_inherited = |had: &InheritedDek| {
            let was = unwrap_inherited(kek_of(&had.dek)?, cell, had)?;
            let of = had.cell;
            Ok::<_, Error>(wrap_inherited(
                (&secret, generation),
                cell,
                of,
                had.dek.generation,
                &was,
            ))
        };
        let rewraps = !(found.deks.is_empty() && found.inherited.is_empty());
        found.deks = found.deks.iter().map(rewrap).collect::<Result<_, _>>()?;
        found.inherited = found.inherited.iter().map(rewrap_inherited).collect::<Result<_, _>>()?;
        found.keks.push(kek.clone());
        Ok(Step::Write(found, rewraps))
    })
    .await
    .inspect_err(|_| kms.abandoned(&kek.wrapped))?;
    kms.made(cell, None, &kek, Arc::new(secret));
    Ok((generation, rewrapped))
}

/// The ring of an open cell of `cell`, whose head is `head`, of the cell `keys` are of: its DEKs,
/// every generation, and the newest to encrypt new objects under if `writes`, a writer's head
/// having its first ([`taking`]: refused otherwise); those its head keeps of ancestors of other
/// cells ([`Inherited`]); and its ancestors', along its origins, each head read.
///
/// An ancestor of another cell (TASK-411.4): a head with its keys holds those of every one it
/// reads, as they were given it with them ([`taking`]), so that it needs no other cell's KEK, and
/// reads no ancestor's head (TASK-633.5): its parent's keys not among them, it is refused
/// ([`Error::Inherited`]), as a fork is that would give it its own without them ([`inheriting`]),
/// which gives every ancestor's at once. So a child is taken with its parent's bucket, in
/// another cloud region perhaps, out of reach (backlog doc-23). A head without them yet, a
/// fork's child's before they are given, is read by crossing into each ancestor's cell, its head
/// read, its KEK unwrapped by the KMS, its keys and those it inherits taken from there on.
pub async fn ring(
    bucket: &impl Bucket,
    keys: &CellKeys,
    cell: CellId,
    head: &Head,
    writes: bool,
) -> Result<Arc<Ring>, Error> {
    let mut ring = Ring { owner: Some(cell), ..Ring::default() };
    ring.take(keys, cell, head).await?;
    if writes {
        let newest = head.deks.iter().map(|dek| dek.generation).max().ok_or_else(|| {
            Error::Corrupt(format!(
                "cell {cell}'s head keeps no DEK, which its writer encrypts under"
            ))
        })?;
        ring.encrypting = Some(KeyId { cell: cell.id, generation: newest });
    }
    // Whether its head has its keys, and with them all it inherits from other cells: its parent's
    // checked, no ancestor's head read.
    if !head.deks.is_empty() {
        return match head.origin() {
            Some(at) if !ring.holds(at.parent) => Err(Error::Inherited { cell, from: at.parent }),
            _ => Ok(Arc::new(ring)),
        };
    }
    let mut origin = head.origin();
    let mut seen = BTreeSet::from([cell]);
    while let Some(at) = origin {
        let parent = at.parent;
        if !seen.insert(parent) {
            return Err(Error::Corrupt(format!("cell {cell}'s origins go round at {parent}")));
        }
        let found = head::get(bucket, parent).await?;
        let found = found.ok_or_else(|| Error::Lost(layout::heads(parent)))?;
        origin = found.origin();
        if found.deks.is_empty() || ring.holds(parent) {
            continue;
        }
        ring.take(&CellKeys::of(parent, &found, keys.kms.clone()), parent, &found).await?;
    }
    Ok(Arc::new(ring))
}

/// The ring the collector compacts `cell`'s objects under (TASK-411.6): its head's DEKs and those
/// it keeps of ancestors of other cells ([`Ring::take`]), unwrapped under the generations of
/// `keys`, the cell's, KEK they name, each unwrapped once ([`CellKeys`] keeps what it unwrapped).
/// It encrypts nothing of its own: the collector encrypts what it compacts under the key its source
/// was encrypted under.
pub async fn compacting(cell: CellId, head: &Head, keys: &CellKeys) -> Result<Arc<Ring>, Error> {
    let mut ring = Ring::default();
    ring.take(keys, cell, head).await?;
    Ok(Arc::new(ring))
}

/// The ring `cell`'s writer would hold, its head `head`, to encrypt what it writes of the cell's
/// under, its KEK unwrapped by `kms`, the host's: what another than its writer encrypts the
/// cell's objects under, a fork's checkpoint at its origin (TASK-411.4). Refused for a head with no
/// DEK, never taken, which no fork is of.
pub async fn writing(
    bucket: &impl Bucket,
    kms: &Arc<CachingKms>,
    cell: CellId,
    head: &Head,
) -> Result<Arc<Ring>, Error> {
    ring(bucket, &CellKeys::of(cell, head, kms.clone()), cell, head, true).await
}

/// The keys of a fork's parent that whoever makes the fork authenticates the parent's checkpoint at
/// the origin under, if the child is of another cell (TASK-411.4): those its writer holds; or, for
/// a resumed fork, the host's KMS, with the parent's head if read, asked only once a checkpoint is
/// to be written ([`writing`]).
#[derive(Clone, Copy)]
pub enum Parents<'a> {
    Held(&'a Arc<Ring>),
    Kms(&'a Arc<CachingKms>, Option<&'a Head>),
}

impl Parents<'_> {
    /// The ring of `parent`, the parent, to authenticate its checkpoint under: its head read if
    /// not given.
    pub async fn ring(self, bucket: &impl Bucket, parent: CellId) -> Result<Arc<Ring>, Error> {
        match self {
            Self::Held(ring) => Ok(ring.clone()),
            Self::Kms(kms, Some(head)) => writing(bucket, kms, parent, head).await,
            Self::Kms(kms, None) => {
                let head = head::get(bucket, parent).await?;
                let head = head.ok_or_else(|| Error::Lost(layout::heads(parent)))?;
                writing(bucket, kms, parent, &head).await
            }
        }
    }
}

/// What a fork into another cell gives its child (doc-16, Forks and shared versions; TASK-411.4),
/// made before anything of the fork is ([`inheriting`]): the keys its parent's cell holds, which
/// its child inherits, and a KEK for its child's cell from the host's pool, given back if no head
/// is made with it.
pub struct Inheriting {
    kms: Arc<CachingKms>,
    child: CellId,
    source: Arc<Ring>,
    kek: Option<(Arc<Secret>, Kek)>,
}

impl Inheriting {
    /// What the child's head is made with ([`crate::lifecycle::fork_version_keyed`]): the KEK from
    /// the pool, its first DEK wrapped under it, and the keys it inherits rewrapped under it.
    pub fn given(&self) -> head::Given {
        let (secret, kek) = self.kek.clone().expect("a KEK until a head is made with it");
        let deks = vec![wrap_dek((&secret, kek.generation), self.child, FIRST, &Secret::random())];
        let inherited = inherit(&self.source, self.child, (&secret, kek.generation));
        head::Given { kek: Some(kek), deks: Some((deks, inherited)) }
    }

    /// The child's head made with what it gave: the host keeps its KEK for the child's first
    /// taking here, which unwraps nothing then.
    pub fn made(mut self) {
        if let Some((secret, kek)) = self.kek.take() {
            self.kms.made(self.child, None, &kek, secret);
        }
    }
}

impl Drop for Inheriting {
    fn drop(&mut self) {
        if let Some((secret, kek)) = self.kek.take() {
            self.kms.give_back(self.child, (secret, kek.wrapped));
        }
    }
}

/// What a fork of `parent`, whose cell holds `source`, to `child` gives its child ([`Inheriting`]).
/// Asked for before anything of the fork is made, so that a fork refused makes nothing: refused if
/// `kms`, the host's, last heard that a tenant of the cells its keys are of has its KEK revoked or
/// destroyed ([`Kms::revoked`]), by the KMS of that cell's bucket
/// ([`CachingKms::refuses_resolving`]); or as the KMS refuses a KEK for the child's cell, which
/// comes from the host's pool, no call made then ([`new_cell_kek`]).
pub async fn inheriting(
    kms: &Arc<CachingKms>,
    source: &Arc<Ring>,
    child: layout::Child,
) -> Result<Inheriting, Error> {
    let layout::Child { cell: child, .. } = child;
    let outside: BTreeSet<CellId> =
        source.places.values().filter(|&&of| of != child).copied().collect();
    for cell in outside {
        if let Some(refused) = kms.refuses_resolving(cell).await {
            return Err(refused);
        }
    }
    let kek = Some(new_cell_kek(kms, child).await?);
    Ok(Inheriting { kms: kms.clone(), child, source: source.clone(), kek })
}

/// What a host gives its cells to encrypt and read under (TASK-411.3): its KMS, which every host
/// has (TASK-411.28: every cell is encrypted), and the keys its fetchers share, of the cells it
/// serves ([`HostRing`], the host's fetchers' `fetch::Memory::keys`).
#[derive(Clone)]
pub struct Host {
    pub kms: Arc<CachingKms>,
    pub ring: Arc<HostRing>,
}

impl Host {
    /// A host over `kms`, its fetchers' keys `ring`.
    pub fn new(kms: Arc<CachingKms>, ring: Arc<HostRing>) -> Self {
        Self { kms, ring }
    }

    /// Forgets what the host holds of `scope`'s keys, as it is told a tenant's KEK is revoked or
    /// destroyed, or a cell shredded, `why` saying which (TASK-411.7): its KMS's
    /// ([`CachingKms::forget`]) and its fetchers' ([`HostRing::forget`]). Returns how many cells'
    /// rings its fetchers forgot.
    pub fn forget(&self, scope: Scope, why: impl Into<String>) -> usize {
        self.kms.forget(scope, why);
        self.ring.forget(scope)
    }
}

/// cladefs: what tests need of keys (TASK-411.28: every test encrypted): a KMS of their own, making
/// every tenant's KEK on its first use ([`extent_keys::testing::MemoryKms`]), shared by the hosts
/// of a process, as hosts of one deployment share a KMS.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;

    /// The tests' KMS, the process's: every tenant's KEK made on its first wrap.
    pub fn kms() -> kms::Shared {
        static KMS: LazyLock<kms::Shared> =
            LazyLock::new(|| Arc::new(extent_keys::testing::MemoryKms::new()));
        KMS.clone()
    }

    /// A host's KMS over the tests' ([`kms`]): a host's own, keeping what it unwrapped apart.
    pub fn host_kms() -> Arc<CachingKms> {
        CachingKms::new(kms())
    }

    /// A host of the tests: its KMS over the tests' ([`kms`]), its fetchers' keys its own.
    pub fn host() -> Host {
        Host::new(host_kms(), HostRing::new())
    }
}

/// What a host is told to forget the keys of (doc-16, Shredding; TASK-411.7): a tenant's, whose KEK
/// is revoked or destroyed in the KMS, or a cell's, shredded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Tenant(u64),
    Cell(CellId),
}

impl Scope {
    /// Whether `cell` is of it.
    pub fn covers(self, cell: CellId) -> bool {
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
    use crate::cell;
    use crate::layout::{Origin, TEST_CELL, test_cell};
    use crate::record::Made;
    use crate::testing::{Direct, SimBucket};
    use extent_keys::kms::LocalKms;
    use extent_keys::testing::{Counted, Forgetting};
    use futures::executor::block_on;

    /// A KMS holding tenant 1's KEK, counting its calls, and the host's over it.
    fn kms() -> (tempfile::TempDir, Arc<Counted<LocalKms>>, Arc<CachingKms>) {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalKms::open(dir.path().join("kms")).unwrap();
        local.create(TEST_CELL.tenant).unwrap();
        let counted = Arc::new(Counted::new(local));
        let shared: kms::Shared = counted.clone();
        (dir, counted, CachingKms::new(shared))
    }

    /// A DEK wrapped under a cell's KEK unwraps for its place alone: another cell's, of the
    /// same cell, or another generation, fails AES-KW's check, a wrapped DEK copied there.
    #[test]
    fn deks_unwrap_at_their_place_alone() {
        let (kek, dek) = (Secret::random(), Secret::random());
        let wrapped = wrap_dek((&kek, 1), test_cell(7), 2, &dek);
        assert_eq!((wrapped.generation, wrapped.kek), (2, 1));
        assert_eq!(unwrap_dek(&kek, test_cell(7), &wrapped).unwrap().expose(), dek.expose());
        let refused = |cell: CellId, wrapped: &Dek| {
            unwrap_dek(&kek, cell, wrapped).map(|_| ()).unwrap_err().to_string()
        };
        assert_eq!(
            refused(test_cell(8), &wrapped),
            "corrupt: cell 1/8's DEK of generation 2: not authentic: a wrapped key, by its \
             integrity check"
        );
        let relabelled = Dek { generation: 3, ..wrapped.clone() };
        assert_eq!(
            refused(test_cell(7), &relabelled),
            "corrupt: cell 1/7's DEK of generation 3: not authentic: a wrapped key, by its \
             integrity check"
        );
        let other_cell = CellId { tenant: 2, id: 7 };
        assert!(unwrap_dek(&kek, other_cell, &wrapped).is_err());
    }

    /// TASK-411.28: an object authenticated under a key a reader holding keys lacks is refused,
    /// whatever its cell: a bucket's writer that strips an ancestor's record of its DEKs, so
    /// that no ring holds that cell's, and puts a checkpoint of its own authenticated under a
    /// key it made up at the ancestor's key, has it refused, not read as stored. The keyless
    /// ring, the collector's marking's, reads it, checking CRCs.
    #[test]
    fn objects_authenticated_under_keys_unheld_are_refused_by_every_ring_holding_keys() {
        let made_up = KeyId { cell: 8, generation: 99 };
        let state = crate::State::new(test_cell(8));
        let forged = crate::checkpoint::encode(&state, (&Secret::random(), made_up));
        let ring = Ring::encrypting_for_tests(7, 1, Secret::random(), []);
        let read = crate::checkpoint::decode_under(&forged, &ring.of(8));
        assert!(matches!(read, Err(crate::Error::NoKey(key)) if key == made_up), "{read:?}");
        let marked = crate::checkpoint::decode_under(&forged, &Ring::keyless().of(8)).unwrap();
        assert_eq!(marked.cell, test_cell(8));
    }

    /// TASK-411.4: an inherited DEK, kept in a record of another cell, unwraps for its places
    /// alone: kept by another cell's record, or of another cell, or relabelled another
    /// generation, it fails AES-KW's check.
    #[test]
    fn inherited_deks_unwrap_at_their_places_alone() {
        let (kek, dek) = (Secret::random(), Secret::random());
        let holder = CellId { tenant: 2, id: 8 };
        let wrapped = wrap_inherited((&kek, 3), holder, test_cell(7), 2, &dek);
        assert_eq!((wrapped.cell, wrapped.dek.generation, wrapped.dek.kek), (test_cell(7), 2, 3));
        assert_eq!(unwrap_inherited(&kek, holder, &wrapped).unwrap().expose(), dek.expose());
        let refused = |holder: CellId, wrapped: &InheritedDek| {
            unwrap_inherited(&kek, holder, wrapped).map(|_| ()).unwrap_err().to_string()
        };
        assert_eq!(
            refused(CellId { id: 9, ..holder }, &wrapped),
            "corrupt: cell 2/9's DEK of cell 1/7, generation 2: not authentic: a wrapped \
             key, by its integrity check"
        );
        let other = InheritedDek { cell: test_cell(6), ..wrapped.clone() };
        assert!(unwrap_inherited(&kek, holder, &other).is_err());
        let relabelled =
            InheritedDek { dek: Dek { generation: 3, ..wrapped.dek.clone() }, ..wrapped };
        assert!(unwrap_inherited(&kek, holder, &relabelled).is_err());
        // Nor is it a DEK of the cell's own.
        assert!(unwrap_dek(&kek, test_cell(7), &relabelled.dek).is_err());
    }

    /// TASK-411.4: a fork's child in a cell of another tenant's, before its keys are given, is read
    /// by crossing into its parent's cell, unwrapping its KEK; its first taking gives it its KEK,
    /// its DEK and the parent's, rewrapped under its own KEK, in the taking's one write; from then
    /// on its ring needs its own KEK alone, the parent's tenant's KEK destroyed, and refuses an
    /// encrypted ancestor its head holds not. Rotating its KEK rewraps what it inherits too.
    #[test]
    fn rings_cross_cells_until_heads_hold_what_they_inherit() {
        let (_dir, counted, kms) = kms();
        counted.inner().create(2).unwrap();
        let bucket = Direct(SimBucket::new());
        // The parent, the test cell, encrypted; a fork's child, 10, in a cell of tenant 2's.
        let parent = TEST_CELL;
        block_on(lifecycle::create(&bucket, parent, 1)).unwrap();
        let (held, parents) = taken(&bucket, &kms, parent, 5, None);
        let parents_ring = block_on(ring(&bucket, &parents, parent, &held.head, true)).unwrap();
        let dek = Secret::from_bytes(parents_ring.encrypting().0.expose());
        let child = CellId { tenant: 2, id: 10 };
        let origin = Origin { parent, at: 3 };
        // Its head as a fork that gave it no keys makes it.
        let forked = Head::of(Made::Clone(origin), 4);
        block_on(crate::testing::make_head(&bucket, child, forked)).unwrap();
        let head = |cell| block_on(head::get(&bucket, cell)).unwrap().unwrap();

        // Read before its keys are given: across, the parent's cell's KEK unwrapped here.
        counted.reset();
        let (_, other) = kms_over(&counted);
        let keys = CellKeys::of(child, &head(child), other);
        let across = block_on(ring(&bucket, &keys, child, &head(child), false)).unwrap();
        assert_eq!(across.cells(), BTreeSet::from([TEST_CELL.id]));
        assert_eq!(counted.unwraps(), 1);
        // Given by its first taking: its KEK, its DEK and the parent's.
        let (given, _) = taken(&bucket, &kms, child, 6, None);
        assert_eq!((given.head.keks.len(), given.head.deks.len()), (1, 1));
        let inherited: Vec<_> = given.head.inherited.iter().map(|had| had.cell).collect();
        assert_eq!(inherited, [parent]);

        // The parent's tenant shredded, and its head out of reach (TASK-633.5): the child's ring,
        // on a host unwrapping anew, needs its own KEK alone, and reads no ancestor's head.
        counted.inner().destroy(TEST_CELL.tenant).unwrap();
        for slot in 0..crate::identity::DEFAULT_HEAD_SLOTS {
            block_on(crate::bucket::delete(&bucket, &layout::head(parent, slot))).unwrap();
        }
        counted.reset();
        let (_, fresh) = kms_over(&counted);
        let keys = CellKeys::of(child, &head(child), fresh.clone());
        let rung = block_on(ring(&bucket, &keys, child, &head(child), true)).unwrap();
        assert_eq!(rung.cells(), BTreeSet::from([TEST_CELL.id, 10]));
        let id = KeyId { cell: TEST_CELL.id, generation: 1 };
        assert_eq!(rung.of(TEST_CELL.id).dek(id).unwrap().expose(), dek.expose());
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 1));
        // A head given its keys without its parent's: refused, not read across.
        let stripped = Head { inherited: Vec::new(), ..head(child) };
        let refused = block_on(ring(&bucket, &keys, child, &stripped, true)).map(|_| ());
        assert_eq!(refused, Err(Error::Inherited { cell: child, from: parent }));

        // Its KEK rotated: what it inherits rewrapped under the new generation, the same.
        counted.inner().create(TEST_CELL.tenant).ok();
        assert_eq!(block_on(rotate(&bucket, &fresh, child)), Ok((2, true)));
        let rotated = head(child);
        assert_eq!(rotated.inherited.iter().map(|had| had.dek.kek).collect::<Vec<_>>(), [2]);
        assert_ne!(rotated.inherited[0].dek.wrapped, given.head.inherited[0].dek.wrapped);
        let keys = CellKeys::of(child, &rotated, fresh);
        let rung = block_on(ring(&bucket, &keys, child, &rotated, true)).unwrap();
        assert_eq!(rung.of(TEST_CELL.id).dek(id).unwrap().expose(), dek.expose());
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
        let kms = CachingKms::pooled(shared, pooling);
        let tenant = TEST_CELL.tenant;
        runtime.block_on(kms.new_kek(TEST_CELL)).unwrap();
        runtime.block_on(kms.filled(tenant));
        assert_eq!((counted.wraps(), kms.in_pool(tenant)), (1 + 3, 3));
        for _ in 0..2 {
            runtime.block_on(kms.new_kek(TEST_CELL)).unwrap();
        }
        runtime.block_on(kms.filled(tenant));
        assert_eq!((counted.wraps(), kms.in_pool(tenant)), (1 + 3 + 3, 4));

        kms.revoked(tenant, "tenant 1: its KEK is revoked");
        assert_eq!(kms.in_pool(tenant), 0);
        assert_eq!(kms.refuses(TEST_CELL), Some(Error::Kms("tenant 1: its KEK is revoked".into())));
        kms.refill(TEST_CELL);
        runtime.block_on(kms.filled(tenant));
        assert_eq!((counted.wraps(), kms.in_pool(tenant)), (7, 0));
        let source = Ring::encrypting_for_tests(7, 1, Secret::random(), []);
        let child = crate::layout::Child { cell: CellId { tenant: 2, id: 8 }, owner: 1 };
        let refused = runtime.block_on(inheriting(&kms, &source, child));
        assert_eq!(refused.map(|_| ()), Err(Error::Kms("tenant 1: its KEK is revoked".into())));
        // The KMS answering for it: refused no more, its pool refilled.
        runtime.block_on(kms.new_kek(TEST_CELL)).unwrap();
        assert_eq!(kms.refuses(TEST_CELL), None);
        runtime.block_on(kms.filled(tenant));
        assert_eq!(kms.in_pool(tenant), 3);
        // Taken, then dropped unused: given back.
        let taken = runtime.block_on(inheriting(&kms, &source, child));
        assert_eq!(kms.in_pool(2), 0);
        drop(taken);
    }

    /// TASK-633.6: a host's KMSes, a bucket's each: bucket 2's cells under the second, every other
    /// bucket's under the first.
    struct TwoKmses(kms::Shared, kms::Shared);

    impl Kmses for TwoKmses {
        fn of(&self, bucket: u16) -> BoxFuture<'_, Result<(KmsKey, kms::Shared), String>> {
            let chosen = match bucket {
                2 => (KmsKey("second".into()), self.1.clone()),
                _ => (KmsKey("first".into()), self.0.clone()),
            };
            futures::future::ready(Ok(chosen)).boxed()
        }
    }

    /// TASK-633.6 (review): a tenant the control API says is revoked stays refused by every KMS
    /// until each answers for it again: the first answering for a cell of bucket 1 lifts it there
    /// alone. A fork inheriting keys of a cell of bucket 2, whose KMS the host has not asked since,
    /// is refused, nothing asked of that KMS; once it answers a call for a cell of bucket 2, forks
    /// go on.
    #[test]
    fn revocations_told_hold_for_every_kms_until_each_answers() {
        let (_dir, second, _) = kms();
        let first: kms::Shared = Arc::new(extent_keys::testing::MemoryKms::new());
        let kms = CachingKms::over(Arc::new(TwoKmses(first, second.clone())), None);
        let tenant = TEST_CELL.tenant;
        let (ours, theirs) =
            (CellId { tenant, id: 1 << 48 | 1 }, CellId { tenant, id: 2 << 48 | 1 });
        let told = Error::Kms("tenant 1: revoked".into());
        kms.revoked(tenant, "tenant 1: revoked");
        assert_eq!(
            (kms.refuses(ours), kms.refuses(theirs)),
            (Some(told.clone()), Some(told.clone()))
        );
        block_on(kms.new_kek(ours)).unwrap();
        assert_eq!((kms.refuses(ours), kms.refuses(theirs)), (None, Some(told.clone())));
        let source = Ring::encrypting_for_tests(theirs.id, 1, Secret::random(), []);
        let child = crate::layout::Child { cell: CellId { tenant: 2, id: 1 << 48 | 8 }, owner: 1 };
        let forking = block_on(inheriting(&kms, &source, child)).map(|_| ());
        assert_eq!((forking, second.wraps()), (Err(told), 0));
        block_on(kms.new_kek(theirs)).unwrap();
        assert!(block_on(inheriting(&kms, &source, child)).is_ok());
    }

    /// TASK-633.6 (review): a new cell KEK given out before a revocation goes back to no pool, nor
    /// is kept for its cell's first taking, though the revocation lifted since.
    #[test]
    fn keks_given_out_before_a_revocation_come_back_to_nothing() {
        let (_dir, counted, kms) = kms();
        let tenant = TEST_CELL.tenant;
        let other = CellId { tenant, id: 9 };
        let (secret, wrapped) = block_on(kms.new_kek(TEST_CELL)).unwrap();
        let (made, made_wrapped) = block_on(kms.new_kek(other)).unwrap();
        kms.revoked(tenant, "tenant 1: revoked");
        // Lifted by an answer of the KMS asked since.
        block_on(kms.new_kek(CellId { tenant, id: 10 })).unwrap();
        assert_eq!(kms.refuses(TEST_CELL), None);
        kms.give_back(TEST_CELL, (secret, wrapped));
        assert_eq!(kms.in_pool(tenant), 0);
        let kek = Kek { generation: FIRST, wrapped: made_wrapped };
        kms.made(other, None, &kek, made.clone());
        let head = Head { keks: vec![kek], ..Head::of(Made::Root, 4) };
        let unwraps = counted.unwraps();
        let keys = CellKeys::to_take(other, &head, kms.clone(), 4);
        let unwrapped = block_on(keys.kek(FIRST)).unwrap();
        assert_eq!((unwrapped.expose(), counted.unwraps()), (made.expose(), unwraps + 1));
    }

    /// TASK-633.6 (review): bucket 2's KMS not to be had (a master key this host has not).
    struct SecondUnreachable(kms::Shared);

    impl Kmses for SecondUnreachable {
        fn of(&self, bucket: u16) -> BoxFuture<'_, Result<(KmsKey, kms::Shared), String>> {
            let chosen = match bucket {
                2 => Err("its tenants' KEKs are kept under a master key this host has not".into()),
                _ => Ok((KmsKey("first".into()), self.0.clone())),
            };
            futures::future::ready(chosen).boxed()
        }
    }

    /// TASK-633.6 (review): a fork inheriting keys of a cell of a bucket whose KMS can't be looked
    /// up is refused as the control API said alone: not for another KMS's refusal, nor for the
    /// lookup's failure.
    #[test]
    fn forks_from_buckets_of_kmses_not_to_be_had_are_refused_as_told_alone() {
        let (_dir, counted, _) = kms();
        let kms = CachingKms::over(Arc::new(SecondUnreachable(counted.clone())), None);
        let tenant = TEST_CELL.tenant;
        let (ours, theirs) =
            (CellId { tenant, id: 1 << 48 | 1 }, CellId { tenant, id: 2 << 48 | 1 });
        counted.inner().revoke(tenant).unwrap();
        assert!(block_on(kms.new_kek(ours)).is_err());
        counted.inner().unrevoke(tenant).unwrap();
        counted.inner().create(2).unwrap();
        let source = Ring::encrypting_for_tests(theirs.id, 1, Secret::random(), []);
        let child = crate::layout::Child { cell: CellId { tenant: 2, id: 1 << 48 | 8 }, owner: 1 };
        assert!(block_on(inheriting(&kms, &source, child)).is_ok());
        kms.revoked(tenant, "tenant 1: revoked");
        let refused = block_on(inheriting(&kms, &source, child)).map(drop);
        assert_eq!(refused, Err(Error::Kms("tenant 1: revoked".into())));
    }

    /// A KMS holding tenant 1's KEK, forgetting it once destroyed as a real KMS forgets a deleted
    /// key, counting its calls (TASK-411.19).
    fn forgetting_kms() -> (tempfile::TempDir, Arc<Counted<Forgetting<LocalKms>>>) {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalKms::open(dir.path().join("kms")).unwrap();
        local.create(TEST_CELL.tenant).unwrap();
        (dir, Arc::new(Counted::new(Forgetting::new(local))))
    }

    /// TASK-411.28, AC #3: a KMS answering that it has no KEK for a tenant, one never given one
    /// or one whose KEK it forgot (a real KMS's deleted key, `Forgetting`), refuses the tenant's
    /// new cells as destroyed, naming it, asked again each time (no answer is cached as the clear
    /// cells' was, TASK-411.10); its pool goes as soon as a refill hears it, and forks into its new
    /// cells are refused. Nothing is made in the clear: the cell's making fails.
    #[test]
    fn kms_having_no_kek_refuses_new_cells_as_destroyed() {
        let (_dir, counted) = forgetting_kms();
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let shared: kms::Shared = counted.clone();
        let pooling = Pooling { low: 2, batch: 3, handle: runtime.handle().clone() };
        let kms = CachingKms::pooled(shared, pooling);
        let new = |cell| runtime.block_on(new_cell_kek(&kms, cell)).map(|(_, kek)| kek);
        // Tenant 2, never given a KEK.
        let two = CellId { tenant: 2, id: 1 };
        let none = Error::Kms("tenant 2: no KEK in the KMS".into());
        for wraps in 1..=3 {
            assert_eq!(new(two), Err(none.clone()));
            assert_eq!(counted.wraps(), wraps);
        }
        assert_eq!(kms.refuses(two), Some(none));
        // Tenant 1, encrypted, its pool filled; its KEK forgotten: a refill hearing it drops the
        // pool.
        let tenant = TEST_CELL.tenant;
        assert_eq!(new(TEST_CELL).map(|kek| kek.generation), Ok(FIRST));
        runtime.block_on(kms.filled(tenant));
        assert_eq!(kms.in_pool(tenant), 3);
        counted.inner().inner().destroy(tenant).unwrap();
        // Takes from the pool: the KMS, which forgot the KEK, would refuse a wrap now. The first
        // leaves the pool at its low mark, refilling nothing; the second leaves it under, and its
        // refill, on the pool's runtime, may have heard the KMS and dropped the pool by the time
        // this looks (board 2360): its count is looked at once the refill is done.
        new(TEST_CELL).unwrap();
        assert_eq!(kms.in_pool(tenant), 2);
        new(TEST_CELL).unwrap();
        runtime.block_on(kms.filled(tenant));
        assert_eq!(kms.in_pool(tenant), 0);
        let forgotten = Error::Kms("tenant 1: no KEK in the KMS".into());
        assert_eq!(new(TEST_CELL), Err(forgotten.clone()));
        assert_eq!(kms.refuses(TEST_CELL), Some(forgotten.clone()));
        // A fork of an encrypted cell into a new cell of the tenant's: refused before anything is
        // made.
        let source = Ring::encrypting_for_tests(7, 1, Secret::random(), []);
        let child = crate::layout::Child { cell: CellId { tenant, id: 8 }, owner: 1 };
        let forking = runtime.block_on(inheriting(&kms, &source, child));
        assert_eq!(forking.map(|_| ()), Err(forgotten));
    }

    /// TASK-411.7: a cell shredded keeps no KEK, by one conditional update whoever holds it, and
    /// is taken by no one again, its rings refused naming it. A host that took it before, told
    /// nothing, still reads under what it unwrapped; told, it forgets the cell's KEK, refusing it
    /// under the head it read before, and its fetchers forget the ring of the cell, the
    /// tenant's other cell's staying. Told the tenant's KEK is revoked, it forgets the rest.
    #[test]
    fn shredded_cells_refuse_their_keys_and_hosts_told_forget_them() {
        let (_dir, counted, kms) = kms();
        let bucket = Direct(SimBucket::new());
        let (ours, theirs) = (TEST_CELL, CellId { tenant: TEST_CELL.tenant, id: 2 });
        for cell in [ours, theirs] {
            block_on(lifecycle::create(&bucket, cell, 1)).unwrap();
            taken(&bucket, &kms, cell, 5, None);
        }
        let head = |cell| block_on(head::get(&bucket, cell)).unwrap().unwrap();

        // Another host takes both cells, and opens a cell of each.
        counted.reset();
        let (_, other) = kms_over(&counted);
        let host = Host::new(other.clone(), HostRing::new());
        let (our_keys, their_keys) =
            (taken(&bucket, &other, ours, 6, None).1, taken(&bucket, &other, theirs, 6, None).1);
        let opened = |keys: &CellKeys, cell| {
            block_on(ring(&bucket, keys, cell, &head(cell), true)).map(|ring| ring.cells())
        };
        let our_ring = block_on(ring(&bucket, &our_keys, ours, &head(ours), true)).unwrap();
        let their_ring = block_on(ring(&bucket, &their_keys, theirs, &head(theirs), true)).unwrap();
        let _held = [host.ring.hold(our_ring), host.ring.hold(their_ring)];
        assert_eq!(host.ring.ring().cells(), BTreeSet::from([1, 2]));
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 2));

        let before = head(ours);
        assert_eq!(block_on(shred(&bucket, ours)), Ok(1));
        let swaps = before.swaps + 1;
        let expected = Head { keks: Vec::new(), shredded: true, swaps, ..before.clone() };
        assert_eq!(head(ours), expected);
        assert_eq!(block_on(shred(&bucket, ours)), Ok(0));
        assert_eq!(head(ours), expected);
        let refused = Error::Kms("tenant 1: cell 1/1 is shredded, its KEK gone".into());
        let taking = head::Taking { holder: 7, token: 7, ..head::Taking::default() };
        let took = block_on(async {
            head::take(&bucket, ours, head::read(&bucket, ours).await.unwrap(), &taking).await
        });
        assert_eq!(took.map(|held| held.head), Err(refused.clone()));
        assert_eq!(
            block_on(rotate(&bucket, &other, ours)),
            Err(Error::Corrupt("cell 1/1 keeps no KEK, which a taken cell does".into()))
        );

        // Read anew, the cell's keys are refused, encrypted though.
        let fresh = CellKeys::of(ours, &head(ours), other.clone());
        assert!(fresh.encrypted());
        assert_eq!(opened(&fresh, ours), Err(refused.clone()));

        // Told nothing, the host reads under what it unwrapped for its taking.
        assert_eq!(opened(&our_keys, ours), Ok(BTreeSet::from([1])));
        assert_eq!(host.forget(Scope::Cell(ours), "shredded"), 1);
        assert_eq!(opened(&our_keys, ours), Err(refused.clone()));
        assert_eq!(host.ring.ring().cells(), BTreeSet::from([2]));
        assert_eq!(opened(&their_keys, theirs), Ok(BTreeSet::from([2])));
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 2));

        assert_eq!(host.forget(Scope::Tenant(TEST_CELL.tenant), "tenant 1: revoked"), 1);
        assert_eq!(host.ring.ring().cells(), BTreeSet::new());
        assert_eq!(other.refuses(TEST_CELL), Some(Error::Kms("tenant 1: revoked".into())));
        assert_eq!(opened(&their_keys, theirs).map(drop), Ok(()));
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 3));
    }

    /// Takes `cell` as `holder`, of token 0, by `kms`, the host's, giving it `kind`: its head as
    /// taken, and its keys ([`taking`]).
    fn taken(
        bucket: &impl Bucket,
        kms: &Arc<CachingKms>,
        cell: CellId,
        holder: u64,
        kind: Option<cell::Kind>,
    ) -> (head::Held, CellKeys) {
        let taking = head::Taking { holder, kind, ..head::Taking::default() };
        block_on(async { taking_(bucket, kms, cell, taking).await }).unwrap()
    }

    async fn taking_(
        bucket: &impl Bucket,
        kms: &Arc<CachingKms>,
        cell: CellId,
        taking: head::Taking,
    ) -> Result<(head::Held, CellKeys), Error> {
        super::taking(bucket, kms, cell, head::read(bucket, cell).await.unwrap(), taking).await
    }

    /// A host's KMS over `counted`, unwrapping anew.
    fn kms_over(counted: &Arc<Counted<LocalKms>>) -> ((), Arc<CachingKms>) {
        let shared: kms::Shared = counted.clone();
        ((), CachingKms::new(shared))
    }
}
