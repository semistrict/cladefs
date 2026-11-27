// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the bucket KMS (doc-16, The bucket KMS; TASK-411.27): each tenant's KEK kept in the
//! bucket itself, at `tenants/<tenant>/kek` ([`layout::kek`]), wrapped by a master key the host is
//! given in its environment (`CLADEFS_MASTER_KEY`, `CLADEFS_MASTER_KEY_FILE`;
//! `extent_keys::kms::master`). What the objects and keys hold is `extent_keys::kms::bucket`'s;
//! here is where they are kept, and how hosts agree on them.
//!
//! - **Made on a tenant's first wrap** ([`BucketKms`]): no setup a tenant. A host finding no KEK
//!   object makes a random KEK and creates its object, create-only; hosts racing to make one all
//!   take the one that landed, a create finding another's object there using it, its own KEK
//!   dropped unused. Every tenant is so encrypted.
//! - **Shredding a tenant** ([`destroy`]) replaces its KEK object by a tombstone: every key wrapped
//!   under its KEK unwraps no more, and the tenant reads as destroyed, never made a KEK again, on
//!   every host. Only as strong as the bucket keeps no older version of the object, as for a cell
//!   shredded (doc-16, Shredding). A KEK object deleted outright, not by [`destroy`], loses what
//!   its KEK wrapped as shredding does, its keys refused as another KEK's; the tenant's next wrap
//!   makes it a new KEK, never a cell in the clear (TASK-411.28: no cell is; the encrypted marker
//!   that refused such a tenant, TASK-411.19, went). But a host's cells made after from KEKs it
//!   pooled, or wrapped under the old KEK it held, are under the old KEK, and lost with it at
//!   their next taking, what was written to them too (TASK-411.29).
//! - **Rotating the master keys** ([`rotate`]): a host given several unwraps under any of them and
//!   makes KEK objects under the first, the newest; [`rotate`] rewraps every tenant's KEK object
//!   under it, each by a conditional update, after which the older keys are no longer needed. The
//!   KEKs themselves stay, and every key they wrapped with them.
//! - **What a host keeps:** each tenant's KEK unwrapped, zeroed when dropped, read from the bucket
//!   again once [`FRESH_FOR`] old, so that a tenant shredded on another host is refused here within
//!   that time even if this host is not told; dropped at once by `Kms::forget`, which a host told
//!   the tenant's keys are revoked or destroyed calls (`keys::CachingKms::revoked`).
//! - **Which KMS a host uses** ([`from_env`]): `CLADEFS_KMS`'s if it names one; else, given a
//!   master key, this one, over the host's bucket; else none, which every process opening cells
//!   refuses to start with, naming both (TASK-411.28: every cell is encrypted). The cells of other
//!   buckets, of other cloud regions, are each under their own bucket's KMS ([`ByBucket`],
//!   TASK-633.6): the one the registry of buckets names for it, or its own variables do, or the
//!   bucket KMS over that bucket, its tenants' KEKs kept there.

use crate::bucket::{self, Bucket, Created, Updated};
use crate::identity::{self, NAMED};
use crate::keys::{KmsKey, Kmses, Unfound};
use crate::layout;
use crate::object_bucket::{ObjectBucket, RoutedBucket};
use extent_keys::kms::bucket::{self as formats, KekObject, TenantKek};
use extent_keys::kms::master::{MASTER_KEY_FILE_VAR, MASTER_KEY_VAR, MasterKeyId, MasterKeys};
use extent_keys::kms::{self, Error, Kms, Origin, Trust, Vars};
use extent_keys::{Secret, Unauthentic, Wrapped};
use futures::StreamExt as _;
use futures::future::BoxFuture;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;

/// The variable naming a host's KMS by its URL (`extent_keys::kms::from_url`), which [`from_env`]
/// takes over a master key.
pub use extent_keys::kms::KMS_VAR;

/// How long a host uses a tenant's KEK as it last read it before reading its object again: so
/// that a tenant shredded elsewhere is refused here within it, told or not, and the bucket is
/// asked once a tenant that long at most however many cells' KEKs the host wraps and unwraps.
pub const FRESH_FOR: Duration = Duration::from_secs(60);

/// Shared master-key cell and its sealing fence (board 10466, hotbed review 117): every
/// [`BucketKms`] on a host naming the same master-key file shares both, so a reload removing a
/// master key fences sealing across every existing and lazily-opened bucket sharing that file.
struct SharedKeys {
    /// Its master keys: as given, or as a reload gave them since ([`ByBucket::reload`], board
    /// 8487).
    keys: Mutex<Arc<MasterKeys>>,
    /// Held shared by each KEK object's making, from its master key taken until its create is
    /// answered, and whole by a reload removing master keys from its checks until its keys are
    /// in use ([`BucketKms::fenced`]): so that no object sealed under a key removed lands unseen
    /// by the check (board 8487, hotbed review 79, hotbed review 117).
    sealing: Arc<tokio::sync::RwLock<()>>,
}

impl SharedKeys {
    fn new(keys: MasterKeys) -> Arc<Self> {
        Arc::new(Self { keys: Mutex::new(Arc::new(keys)), sealing: Arc::default() })
    }
}

/// A KMS over a bucket: tenants' KEKs kept in it, each wrapped by a master key (see the module).
pub struct BucketKms<B> {
    bucket: Arc<B>,
    shared: Arc<SharedKeys>,
    held: Mutex<HeldKeks>,
    fresh_for: Duration,
    /// The identity of its bucket (TASK-633.6), which the keys it wraps name, once read: 0 for a
    /// bucket of none, its keys bound to no bucket.
    identity: Mutex<Option<u64>>,
}

#[derive(Default)]
struct HeldKeks {
    tenants: HashMap<u64, Held>,
    /// Advanced under the same lock that publishes reads, even if no KEK was held.
    generation: u64,
}

/// A tenant's KEK as a host read it.
struct Held {
    kek: Arc<TenantKek>,
    read: Instant,
}

impl<B: Bucket> BucketKms<B> {
    /// The KMS keeping tenants' KEKs in `bucket`, under `keys`, the newest first.
    pub fn new(bucket: Arc<B>, keys: MasterKeys) -> Self {
        Self::with_shared_keys(bucket, SharedKeys::new(keys))
    }

    /// cladefs: the same, sharing `shared` (master keys and sealing fence) with other bucket
    /// KMSes on the same host naming the same master-key file (board 10466, hotbed review 117).
    fn with_shared_keys(bucket: Arc<B>, shared: Arc<SharedKeys>) -> Self {
        Self {
            bucket,
            shared,
            held: Mutex::default(),
            fresh_for: FRESH_FOR,
            identity: Mutex::default(),
        }
    }

    /// Its shared master-key and fence cell (board 10466, hotbed review 117).
    fn shared_keys(&self) -> Arc<SharedKeys> {
        self.shared.clone()
    }

    /// Has it take `identity` as its bucket's, known from elsewhere (the registry of buckets).
    pub fn identify(&self, identity: u64) {
        *self.identity.lock().unwrap() = Some(identity);
    }

    /// The identity of its bucket, which the keys it wraps name, read once (or again if `afresh`):
    /// 0 for a bucket of none.
    async fn identity(&self, tenant: u64, afresh: bool) -> Result<u64, Error> {
        if let Some(identity) = *self.identity.lock().unwrap()
            && !afresh
        {
            return Ok(identity);
        }
        let read = crate::identity::get(&*self.bucket).await.map_err(|err| Error::Io {
            tenant,
            source: std::io::Error::other(format!("the bucket's identity: {err}")),
        })?;
        let identity = read.map_or(0, |identity| identity.id);
        *self.identity.lock().unwrap() = Some(identity);
        Ok(identity)
    }

    /// The same, reading a tenant's KEK object again once it is `fresh_for` old: tests'.
    pub fn fresh_for(self, fresh_for: Duration) -> Self {
        Self { fresh_for, ..self }
    }

    /// The bucket it keeps tenants' KEKs in.
    pub fn bucket(&self) -> &Arc<B> {
        &self.bucket
    }

    /// Its master keys, as now.
    pub fn keys(&self) -> Arc<MasterKeys> {
        self.shared.keys.lock().unwrap().clone()
    }

    /// cladefs: has it use `keys` from its next KEK object on (board 8487): its KEKs held stay, as
    /// they are the same whatever key wraps their objects.
    fn set_keys(&self, keys: Arc<MasterKeys>) {
        *self.shared.keys.lock().unwrap() = keys;
    }

    /// cladefs: refuses removing `removed`, of its master keys, its bucket numbered `number`, while
    /// a KEK object in it is under one (its tenant would open no cell here once its KEK was read
    /// again). Caller holds `self.shared.sealing` exclusively so no KEK object is made under a
    /// removed key concurrently across any bucket sharing `self.shared` (hotbed review 117).
    async fn check_unsealed(&self, number: u16, removed: &[MasterKeyId]) -> Result<(), Unreloaded> {
        let name = format!("bucket {number}");
        match sealed_under(&*self.bucket, removed, &name).await? {
            None => Ok(()),
            Some((tenant, id)) => Err(Unreloaded::InUse(format!(
                "{name}: tenant {tenant}'s KEK object is under master key {id}: rotate them under \
                 another first (cladefs-gc --rotate-master-key), then remove it"
            ))),
        }
    }

    /// Acquires the shared sealing fence and checks that no KEK object in `self.bucket` is sealed
    /// under `removed`.
    #[cfg(test)]
    async fn unsealed(
        &self,
        number: u16,
        removed: &[MasterKeyId],
    ) -> Result<tokio::sync::OwnedRwLockWriteGuard<()>, Unreloaded> {
        let label = format!("bucket {number}'s KEK objects being made");
        let fence = within(label, self.shared.sealing.clone().write_owned()).await?;
        self.check_unsealed(number, removed).await?;
        Ok(fence)
    }

    /// Shreds `tenant` ([`destroy`]), and forgets its KEK here.
    pub async fn destroy(&self, tenant: u64) -> Result<Destroyed, bucket::Refused> {
        let destroyed = destroy(&*self.bucket, tenant).await;
        self.forget(tenant);
        destroyed
    }

    /// `tenant`'s KEK: as held, if fresh; else read from its object, or, if it has none and
    /// `make`, made. Refused if the object is a tombstone, or missing and not to be made: the
    /// tenant is destroyed.
    async fn kek(&self, tenant: u64, make: bool) -> Result<Arc<TenantKek>, Error> {
        let (read, generation) = {
            let held = self.held.lock().unwrap();
            if let Some(kept) = held.tenants.get(&tenant)
                && kept.read.elapsed() < self.fresh_for
            {
                return Ok(kept.kek.clone());
            }
            (Instant::now(), held.generation)
        };
        let kek = self.read(tenant, make).await;
        let mut held = self.held.lock().unwrap();
        if held.generation != generation {
            return Err(Error::Io {
                tenant,
                source: std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "the bucket KMS cache was invalidated during a KEK read",
                ),
            });
        }
        match &kek {
            Ok(kek) => held.tenants.insert(tenant, Held { kek: kek.clone(), read }),
            // Unread: none held, so that the next asks again; the error is the caller's.
            Err(_) => held.tenants.remove(&tenant),
        };
        kek
    }

    /// `tenant`'s KEK, read from its object; or, none there and `make`, made, create-only. A create
    /// finding another host's object there takes that.
    async fn read(&self, tenant: u64, make: bool) -> Result<Arc<TenantKek>, Error> {
        let key = layout::kek(tenant);
        let refused = |refused| refused_kms(tenant, refused);
        if let Some(object) = bucket::get(&*self.bucket, &key).await.map_err(refused)? {
            return self.opened(tenant, &object.bytes);
        }
        // A key wrapped under a KEK names one that was there: deleted since, by someone other than
        // `destroy`.
        if !make {
            return Err(Error::Destroyed { tenant });
        }
        let kek = TenantKek::random();
        // Held until its create is answered: no reload removes its key meanwhile.
        let _sealing = self.shared.sealing.read().await;
        let keys = self.keys();
        let newest = keys.newest();
        let sealed = KekObject::seal(newest, tenant, &kek);
        match bucket::create(&*self.bucket, &key, sealed).await.map_err(refused)? {
            Created::Created(_) => {
                log::info!(
                    "cladefs: tenant {tenant}: its KEK made, {key}, under master key {}",
                    newest.id()
                );
                Ok(Arc::new(kek))
            }
            Created::Exists(object) => self.opened(tenant, &object.bytes),
        }
    }

    /// The KEK of `bytes`, `tenant`'s KEK object, unwrapped by one of its master keys.
    fn opened(&self, tenant: u64, bytes: &[u8]) -> Result<Arc<TenantKek>, Error> {
        KekObject::decode(tenant, bytes)?.open(&self.keys()).map(Arc::new)
    }
}

impl<B: Bucket> Kms for BucketKms<B> {
    async fn wrap(&self, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        let identity = self.identity(tenant, false).await?;
        let kek = self.kek(tenant, true).await?;
        Ok(formats::wrap(tenant, identity, &kek, key))
    }

    async fn unwrap(&self, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        let unauthentic = |source| Error::Unauthentic { tenant, source };
        let (of, kept_in, id) = formats::kek_of(wrapped).map_err(unauthentic)?;
        if of != tenant {
            return Err(unauthentic(Unauthentic("a wrapped key, wrapped for another tenant")));
        }
        // Another bucket's key says nothing of the tenant's KEK here: never read as destroyed. A
        // key bound to no bucket (0) was wrapped before its bucket had an identity.
        let mut identity = self.identity(tenant, false).await?;
        if kept_in != 0 && kept_in != identity && identity == 0 {
            // Given one since, perhaps.
            identity = self.identity(tenant, true).await?;
        }
        if kept_in != 0 && kept_in != identity {
            return Err(Error::Elsewhere { tenant, bucket: kept_in });
        }
        let identity = kept_in;
        let mut kek = self.kek(tenant, false).await?;
        if kek.id != id {
            // Held from before its object changed, perhaps: read it again.
            self.forget(tenant);
            kek = self.kek(tenant, false).await?;
        }
        formats::unwrap(tenant, identity, &kek, wrapped).map_err(unauthentic)
    }

    fn forget(&self, tenant: u64) {
        let mut held = self.held.lock().unwrap();
        held.generation += 1;
        held.tenants.remove(&tenant);
    }

    fn prewrap(&self) -> bool {
        false
    }
}

/// cladefs: what the KMS answers when the bucket refuses `tenant`'s KEK object's request for good
/// (board 5861): the KMS failed, as it does when out of reach, saying why, never a revocation.
fn refused_kms(tenant: u64, refused: bucket::Refused) -> Error {
    let kind = match refused.refusal {
        bucket::Refusal::Denied => std::io::ErrorKind::PermissionDenied,
        bucket::Refusal::Request => std::io::ErrorKind::InvalidInput,
        bucket::Refusal::Unreadable => std::io::ErrorKind::InvalidData,
    };
    Error::Io { tenant, source: std::io::Error::new(kind, refused) }
}

/// What [`destroy`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destroyed {
    /// It put the tombstone there: the tenant's KEK is destroyed, or, had it none, never made.
    Now,
    /// One was there already.
    Already,
}

/// Shreds `tenant` in `bucket` (doc-16, Shredding): its KEK object replaced by a tombstone, by a
/// conditional update, or created one if it has none, so that it is never made a KEK. Needs no
/// master key. Every host reading it from then on refuses the tenant as destroyed: those holding
/// its KEK within [`FRESH_FOR`], or at once once told (`ForgetTenantKeys`). Only as strong as the
/// bucket keeps no older version of the object: with versioning on, a lifecycle rule must expire
/// non-current versions of `tenants/<tenant>/kek`. cladefs: refused by the bucket, [`bucket::Refused`]
/// (board 5861).
pub async fn destroy(bucket: &impl Bucket, tenant: u64) -> Result<Destroyed, bucket::Refused> {
    let key = layout::kek(tenant);
    loop {
        let Some(object) = bucket::get(bucket, &key).await? else {
            match bucket::create(bucket, &key, KekObject::tombstone(tenant)).await? {
                Created::Created(_) => return Ok(Destroyed::Now),
                Created::Exists(_) => continue,
            }
        };
        // A KEK object, or bytes that are none: a tombstone in their place all the same.
        if let Ok(KekObject::Tombstone { .. }) = KekObject::decode(tenant, &object.bytes) {
            return Ok(Destroyed::Already);
        }
        let tombstone = KekObject::tombstone(tenant);
        match bucket::update(bucket, &key, tombstone, &object.meta.version).await? {
            Updated::Updated(_) => return Ok(Destroyed::Now),
            Updated::Changed => {}
        }
    }
}

/// cladefs: shreds `tenant` across `home` and every other bucket recorded in its registry (and the
/// root bucket's registry), needing **no master key**: replaces `tenants/<tenant>/kek` with a
/// tombstone in every bucket under a bucket KMS, records any bucket under a named external KMS in
/// [`Shredded::named`] without opening it, and records any unreachable/failing bucket in
/// [`Shredded::failed`]. Once at least one bucket is tombstoned, publishes
/// [`crate::events::Publish::TenantShredded`] to the root bucket's `events` object so every extent
/// daemon halts open cells and `cladefs-gc` reaps the tenant's cells without host fan-out.
pub async fn shred_buckets(
    home: &RoutedBucket,
    vars: &Vars,
    tenant: u64,
    wait: Duration,
) -> Shredded {
    shred_buckets_inner(home, None, vars, tenant, wait).await
}

/// Shared keyless multi-bucket shredding core used by both [`shred_buckets`] and
/// [`ByBucket::shred`]. When `own_override` is `Some`, it overrides the home bucket's KMS kind
/// (used by [`ByBucket::shred`] so its constructed `HostKms` matches tests without env vars).
async fn shred_buckets_inner(
    home: &RoutedBucket,
    own_override: Option<Option<&str>>,
    vars: &Vars,
    tenant: u64,
    wait: Duration,
) -> Shredded {
    let mut shredded = Shredded::default();
    let own = match identity::get(&*home.own()).await {
        Ok(id) => id.map(|id| id.number),
        Err(why) => {
            shredded.failed.push((NAMED, format!("the host's bucket's identity: {why}")));
            None
        }
    };
    let mut registries = Vec::new();
    match identity::registry_once(&*home.own()).await {
        Ok(reg) => registries.push(reg),
        Err(err) => shredded.failed.push((identity::ROOT, format!("the registry: {err}"))),
    }
    if own.is_some_and(|n| n != identity::ROOT) {
        match home.numbered(identity::ROOT).await {
            Ok(root) => match identity::registry_once(&*root).await {
                Ok(reg) => registries.push(reg),
                Err(err) => shredded.failed.push((identity::ROOT, format!("the registry: {err}"))),
            },
            Err(err) => shredded.failed.push((identity::ROOT, err.to_string())),
        }
    }

    let entry_of = |number: u16| -> Option<identity::Entry> {
        registries.iter().rev().flatten().find_map(|reg| reg.bucket(number).cloned())
    };

    // Determine whether the home bucket is under a named external KMS or a bucket KMS.
    let own_num = own.unwrap_or(NAMED);
    let own_named: Option<String> = match own_override {
        Some(named) => named.map(str::to_owned),
        None => own
            .and_then(&entry_of)
            .and_then(|e| e.details.kms)
            .or_else(|| vars.get(KMS_VAR).map(str::to_owned)),
    };
    match own_named {
        Some(url) => shredded.named.push((own_num, url)),
        None => shredded.destroying(own_num, destroy(&*home.own(), tenant).await),
    }

    let mut numbers = BTreeSet::new();
    for reg in registries.iter().flatten() {
        numbers.extend(reg.buckets.iter().map(|e| e.number));
    }
    let others = numbers.into_iter().filter(|&number| Some(number) != own);
    let shredding = others.map(|number| {
        let entry = entry_of(number);
        async move {
            let bucket_vars = vars.numbered(number);
            let recorded_kms = entry
                .as_ref()
                .and_then(|e| e.details.kms.as_deref())
                .or_else(|| bucket_vars.get(KMS_VAR));
            if let Some(url) = recorded_kms {
                return (number, Ok(Shred::Named(url.to_owned())));
            }
            let work = async {
                let member =
                    home.numbered(number).await.map_err(|err| format!("the bucket: {err}"))?;
                Ok(Shred::Destroyed(destroy(&*member, tenant).await))
            };
            let within = tokio::time::timeout(wait, work).await;
            let out_of_reach = || format!("no answer within {wait:?}");
            (number, within.unwrap_or_else(|_| Err(out_of_reach())))
        }
    });
    for (number, shred) in futures::future::join_all(shredding).await {
        match shred {
            Err(why) => shredded.failed.push((number, why)),
            Ok(Shred::Destroyed(destroyed)) => shredded.destroying(number, destroyed),
            Ok(Shred::Named(url)) => shredded.named.push((number, url)),
        }
    }

    // Publish TenantShredded to the root bucket's `events` object once at least one bucket was
    // tombstoned so every extent daemon halts open cells and `cladefs-gc` reaps the tenant.
    if !shredded.destroyed.is_empty() {
        let publish = async {
            let auth = crate::events::authority(home)
                .await
                .map_err(|err| format!("events authority: {err}"))?;
            crate::events::publish(&*auth, crate::events::Publish::TenantShredded(tenant))
                .await
                .map_err(|err| format!("publishing events: {err}"))?;
            Ok::<(), String>(())
        };
        match tokio::time::timeout(wait, publish).await {
            Ok(Ok(())) => {}
            Ok(Err(why)) => shredded.failed.push((identity::ROOT, why)),
            // Timeout Elapsed error: recorded in `shredded.failed` so the caller knows to retry.
            Err(_elapsed) => {
                shredded.failed.push((identity::ROOT, format!("events: no answer within {wait:?}")))
            }
        }
    }

    shredded
}

/// What [`rotate`] did, tenant by tenant.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rotated {
    /// KEK objects it rewrapped under the newest master key.
    pub rewrapped: usize,
    /// KEK objects under it already.
    pub newest: usize,
    /// Tombstones: tenants shredded.
    pub destroyed: usize,
    /// KEK objects it could not open, each refusal naming its tenant: under a master key none of
    /// those given, or not KEK objects. Left as they are.
    pub refused: Vec<String>,
}

/// Rewraps every tenant's KEK object in `bucket` under the newest of `keys` (doc-16, The bucket
/// KMS): each one under an older key, one of `keys`, by a conditional update, read again and done
/// again if another changed it meanwhile. The KEKs stay as they are. Once done, refusing none,
/// hosts need the newest key alone: the others may be dropped from `CLADEFS_MASTER_KEY`, once the
/// registry of buckets records it too ([`rotate_recorded`], which hosts of other buckets go by,
/// board 8483). Tenants
/// are listed a page at a time; done again, it rewraps nothing more. cladefs: refused by the bucket,
/// [`bucket::Refused`] (board 5861), what it rewrapped before rewrapped.
pub async fn rotate<B: Bucket>(bucket: &B, keys: &MasterKeys) -> Result<Rotated, bucket::Refused> {
    let mut rotated = Rotated::default();
    let mut tenants = std::pin::pin!(bucket::prefixes(bucket, layout::TENANTS.to_owned()));
    while let Some(prefix) = tenants.next().await {
        let prefix = prefix?;
        let Some(tenant) = layout::tenant_of_prefix(&prefix) else { continue };
        let key = layout::kek(tenant);
        loop {
            let Some(object) = bucket::get(bucket, &key).await? else { break };
            let resealed = KekObject::decode(tenant, &object.bytes)
                .and_then(|kek| Ok((kek.resealed(keys)?, kek)));
            match resealed {
                Err(error) => rotated.refused.push(error.to_string()),
                Ok((None, KekObject::Tombstone { .. })) => rotated.destroyed += 1,
                Ok((None, KekObject::Kek { .. })) => rotated.newest += 1,
                Ok((Some(bytes), _)) => {
                    match bucket::update(bucket, &key, bytes, &object.meta.version).await? {
                        Updated::Updated(_) => rotated.rewrapped += 1,
                        Updated::Changed => continue,
                    }
                }
            }
            break;
        }
    }
    Ok(rotated)
}

/// cladefs: what [`rotate_recorded`] made of the registry of buckets (board 8483).
#[derive(Debug, PartialEq, Eq)]
pub enum Recorded {
    /// The rotation refused a KEK object: nothing recorded, the older master keys still needed.
    Refused,
    /// The bucket is in no registry, never formatted, or its entry records no master key: none to
    /// record.
    Unrecorded,
    /// Its entry records the newest master key, in the root's registry and in every bucket's copy
    /// but those that failed, by number, and why: recording again, a rotation run again, finishes
    /// them.
    Recorded { failed: Vec<(u16, String)> },
}

/// cladefs: [`rotate`] of `home`'s own bucket under `keys`; then, refusing none, the newest of
/// `keys` recorded as the master key of its tenants' KEKs in the registry of buckets (board 8483):
/// its entry in the root's registry, by a conditional update ([`identity::record_master_key`]),
/// then every bucket's copy ([`identity::spread`]), the root's first, each reached by `home`'s
/// routes, as its own hosts reach it. Hosts for which the bucket is another than their own choose
/// its bucket KMS by that entry, refusing it unless they hold the key it names ([`ByBucket`]),
/// and read it from their own bucket's copy first: left naming a key dropped from their
/// `CLADEFSD_BUCKET_<N>_CLADEFS_MASTER_KEY` since, they would refuse every cell of the bucket. An
/// entry recording no master key records none still (hotbed question 460). Refused by the bucket, or
/// the registry not to be read or written, says why.
pub async fn rotate_recorded<B: Bucket>(
    home: &RoutedBucket<B>,
    keys: &MasterKeys,
) -> Result<(Rotated, Recorded), String> {
    rotate_recorded_within(home, keys, SPREAD_WITHIN).await
}

/// cladefs: how long [`rotate_recorded`] waits for each bucket's copy of the registry to be
/// written before it says it failed, the bucket out of reach, as [`ByBucket::shred`] waits: the
/// bucket's requests are asked again until it answers, and one out of reach would hold the rest
/// (hotbed review 47). Recording again finishes it.
pub const SPREAD_WITHIN: Duration = Duration::from_secs(30);

/// [`rotate_recorded`], waiting `within` for each copy.
async fn rotate_recorded_within<B: Bucket>(
    home: &RoutedBucket<B>,
    keys: &MasterKeys,
    within: Duration,
) -> Result<(Rotated, Recorded), String> {
    let own = home.own();
    let rotated = rotate(&*own, keys).await.map_err(|err| err.to_string())?;
    if !rotated.refused.is_empty() {
        return Ok((rotated, Recorded::Refused));
    }
    let registry = |err: crate::Error| format!("the registry of buckets: {err}");
    let Some(ours) = identity::get(&*own).await.map_err(registry)? else {
        return Ok((rotated, Recorded::Unrecorded));
    };
    let root = match ours.number {
        identity::ROOT => own.clone(),
        _ => home.numbered(identity::ROOT).await.map_err(registry)?,
    };
    let newest = keys.newest().id().to_string();
    let recorded = identity::record_master_key(&*root, ours.number, &newest).await;
    let Some(recorded) = recorded.map_err(registry)? else {
        return Ok((rotated, Recorded::Unrecorded));
    };
    // Every copy at once, each within `within`.
    let (recorded, own, root) = (&recorded, &own, &root);
    let spreads = recorded.buckets.iter().map(|entry| async move {
        let spread = async {
            let bucket = match entry.number {
                number if number == ours.number => own.clone(),
                identity::ROOT => root.clone(),
                number => home.numbered(number).await?,
            };
            identity::spread(recorded, &*bucket).await
        };
        let spread = match tokio::time::timeout(within, spread).await {
            Ok(spread) => spread.map_err(|err| err.to_string()),
            Err(_) => Err(format!("no answer within {within:?}")),
        };
        (entry.number, spread)
    });
    let spread = futures::future::join_all(spreads).await;
    let failed = spread.into_iter().filter_map(|(number, spread)| Some((number, spread.err()?)));
    Ok((rotated, Recorded::Recorded { failed: failed.collect() }))
}

/// A host's KMS, as [`from_env`] chose it.
pub enum HostKms<B> {
    /// The one `CLADEFS_KMS` names, by its URL.
    Named(String, kms::Shared),
    /// The bucket KMS, over the host's bucket.
    Bucket(Arc<BucketKms<B>>),
}

impl<B: Bucket> HostKms<B> {
    /// It, as a host holds any KMS.
    pub fn shared(&self) -> kms::Shared {
        match self {
            Self::Named(_, kms) => kms.clone(),
            Self::Bucket(kms) => kms.clone(),
        }
    }

    /// The bucket KMS, if it is that.
    pub fn bucket(&self) -> Option<&Arc<BucketKms<B>>> {
        match self {
            Self::Named(..) => None,
            Self::Bucket(kms) => Some(kms),
        }
    }
}

/// Why a host has no KMS ([`from_env`]): neither variable gives one. Every cell is encrypted
/// (TASK-411.28), so a process that opens cells refuses to start so, saying this.
pub fn no_kms() -> String {
    format!(
        "no KMS: {KMS_VAR} names none, and neither {MASTER_KEY_VAR} nor {MASTER_KEY_FILE_VAR} \
         gives a master key; every cell is encrypted (doc-16): set {KMS_VAR} to a KMS's URL, or \
         {MASTER_KEY_VAR} to a master key (openssl rand -hex 32) to keep tenants' KEKs in the \
         bucket"
    )
}

/// The KMS a host's environment chooses, the one place every process that opens cells asks
/// (cladefsd, cladefsctl, the cladefs CLI's mounts through `cladefs::bucket::kms`;
/// `cladefs-gc-fill`): the KMS [`KMS_VAR`] names, if it names one; else, given master keys
/// (`CLADEFS_MASTER_KEY` or `CLADEFS_MASTER_KEY_FILE`), the bucket KMS over `bucket()`, the host's
/// bucket; else refused ([`no_kms`]), as every cell is encrypted (TASK-411.28). A KMS named, or
/// master keys given, that can't be had is refused, saying why, never what a key holds.
pub fn from_env<B: Bucket>(
    bucket: impl FnOnce() -> Result<Arc<B>, String>,
) -> Result<HostKms<B>, String> {
    from_vars(&Vars::from_env()?, bucket)
}

/// cladefs: the same, of `vars`, the host's environment as it read it once ([`Vars::from_env`],
/// board 8593), which its [`ByBucket`] keeps.
pub fn from_vars<B: Bucket>(
    vars: &Vars,
    bucket: impl FnOnce() -> Result<Arc<B>, String>,
) -> Result<HostKms<B>, String> {
    match vars.get(KMS_VAR) {
        Some(url) => {
            let kms = named(vars, url, Origin::Host, KMS_VAR, None)?;
            Ok(HostKms::Named(url.to_owned(), kms))
        }
        None => over_bucket(vars, bucket),
    }
}

/// The KMS named by `url`, with the credentials `vars` give, sharing `keys=FILE` cells in `files`.
fn named(
    vars: &Vars,
    url: &str,
    origin: Origin,
    source: &str,
    files: Option<&kms::OverrideFiles>,
) -> Result<kms::Shared, String> {
    let given =
        [MASTER_KEY_VAR, MASTER_KEY_FILE_VAR].into_iter().find(|name| vars.get(name).is_some());
    if let Some(name) = given {
        log::warn!("cladefs: {name} is set, and unused: {source} names the KMS");
    }
    kms::from_url_with_files(url, vars, origin, files).map_err(|why| format!("{source}: {why}"))
}

/// cladefs: the bucket KMS over `bucket()`, given master keys, as [`from_vars`] chooses it when no
/// KMS is named; else refused ([`no_kms`]).
fn over_bucket<B: Bucket>(
    vars: &Vars,
    bucket: impl FnOnce() -> Result<Arc<B>, String>,
) -> Result<HostKms<B>, String> {
    let keys = MasterKeys::from_vars(vars)?.ok_or_else(no_kms)?;
    let bucket = bucket().map_err(|why| {
        format!("{MASTER_KEY_VAR}: the bucket KMS keeps tenants' KEKs in the host's bucket: {why}")
    })?;
    Ok(HostKms::Bucket(Arc::new(BucketKms::new(bucket, keys))))
}

/// cladefs: the KMSes of the buckets a host reaches (TASK-633.6; backlog doc-23, KMS per bucket),
/// each the one its own hosts use:
///
/// - the host's own bucket's, and the cells tests name (bucket number 0): the host's, as its
///   environment chose it ([`from_env`]);
/// - another bucket's: the KMS its entry in the registry of buckets names (`kms`, as `cladefsctl
///   add-bucket --kms` records it); else the one its variables name (`CLADEFS_KMS`, as
///   `CLADEFSD_BUCKET_<N>_CLADEFS_KMS` overrides it: [`Vars::numbered`]); else,
///   given master keys there (`CLADEFS_MASTER_KEY`, or `CLADEFSD_BUCKET_<N>_CLADEFS_MASTER_KEY`),
///   the bucket KMS over that bucket itself, its tenants' KEKs kept in it, refused unless the
///   master key its entry records (`master_key_id`), if any, is among them; else none, refusing
///   every call for its cells, never as a revocation.
///
/// Each reads its credentials from the variables of its bucket's own over the host's, a group at a
/// time ([`Vars::numbered`], board 8485). A KMS is named by its URL and whose credentials it calls
/// with, one for every bucket naming it with the host's, so that its refusal of a tenant refuses
/// the tenant in all of them and no other ([`keys::CachingKms`]), one of its own for a bucket with
/// credentials of its own; a bucket KMS by its bucket. A KMS URL its registry entry records reaches
/// what the host allows alone ([`Trust`], board 8525).
pub struct ByBucket {
    home: Arc<RoutedBucket>,
    own: HostKms<ObjectBucket>,
    /// The host's environment, as it read it once: the variables its KMSes read alone, zeroed when
    /// dropped (board 8593).
    vars: Arc<Vars>,
    /// The home bucket's number, cached once it has an identity (board 6400).
    own_number: Mutex<Option<u16>>,
    chosen: Mutex<BTreeMap<u16, Arc<OnceCell<Chosen>>>>,
    named: Mutex<HashMap<KmsKey, Arc<OnceCell<kms::Shared>>>>,
    /// Per-host shared `keys=FILE` override cells (board 10466): read once on first use and
    /// changed only by [`ByBucket::reload`].
    override_files: kms::OverrideFiles,
    /// Per-host shared master-key file cells (board 10466, hotbed review 117): read once on first
    /// use and changed only by [`ByBucket::reload`]. Weak so failed constructions leave no stale
    /// entry.
    master_key_files: Mutex<HashMap<PathBuf, Weak<SharedKeys>>>,
    /// How long [`ByBucket::shred`] waits for each other bucket: [`SHRED_WITHIN`], but in tests.
    shred_within: std::time::Duration,
    /// Held by a reload ([`ByBucket::reload`]): one at a time, so that what one checked is what it
    /// applies.
    reloading: tokio::sync::Mutex<()>,
}

impl ByBucket {
    /// The KMSes of the buckets `home`, the host's routed bucket, reaches, its own `own`, the
    /// others' chosen from the registry and `vars`, the host's environment ([`Vars::from_env`]).
    pub fn new(home: Arc<RoutedBucket>, own: HostKms<ObjectBucket>, vars: Arc<Vars>) -> Self {
        let override_files = kms::OverrideFiles::default();
        let mut master_key_files = HashMap::new();
        let named = match &own {
            HostKms::Named(url, kms) => {
                if let Some(names) = kms.names_dyn() {
                    override_files.register(names);
                }
                HashMap::from([(
                    KmsKey(url.as_str().into()),
                    Arc::new(OnceCell::new_with(Some(kms.clone()))),
                )])
            }
            HostKms::Bucket(kms) => {
                if let Some(path) = vars.get(MASTER_KEY_FILE_VAR) {
                    master_key_files
                        .insert(PathBuf::from(path), Arc::downgrade(&kms.shared_keys()));
                }
                HashMap::new()
            }
        };
        Self {
            home,
            own,
            vars,
            own_number: Mutex::default(),
            chosen: Mutex::default(),
            named: Mutex::new(named),
            override_files,
            master_key_files: Mutex::new(master_key_files),
            shred_within: SHRED_WITHIN,
            reloading: tokio::sync::Mutex::new(()),
        }
    }

    /// Shreds `tenant` in every bucket it reaches (doc-16, Shredding; TASK-633.6): its KEK object
    /// replaced by a tombstone in each bucket under a bucket KMS ([`BucketKms::destroy`]), the
    /// host's own first, then each other bucket the registry records, its copy in the host's
    /// bucket and the root bucket's, the authority. A bucket under a KMS named by its URL keeps
    /// the tenant's KEK there, out of reach: destroyed there, out of band (doc-16). Says which.
    /// cladefs: the other buckets at once, each within [`SHRED_WITHIN`], the registries read once:
    /// a bucket out of reach, opened before or not, is said failed, and the others shredded
    /// all the same (board 5528).
    pub async fn shred(&self, tenant: u64) -> Shredded {
        let own_override = match &self.own {
            HostKms::Bucket(_) => Some(None),
            HostKms::Named(url, _) => Some(Some(url.as_str())),
        };
        let shredded =
            shred_buckets_inner(&self.home, own_override, &self.vars, tenant, self.shred_within)
                .await;
        if let HostKms::Bucket(kms) = &self.own {
            kms.forget(tenant);
        }
        let cached: Vec<Arc<BucketKms<ObjectBucket>>> = self
            .chosen
            .lock()
            .unwrap()
            .values()
            .filter_map(|chosen| chosen.get())
            .filter_map(|(_, kms)| kms.clone())
            .collect();
        for kms in cached {
            kms.forget(tenant);
        }
        shredded
    }

    /// The home bucket's number, cached once it has an identity (board 6400): `None` is never
    /// cached, because an unformatted bucket acquires an identity on first cell creation.
    async fn own_number(&self) -> Result<Option<u16>, String> {
        if let Some(own) = *self.own_number.lock().unwrap() {
            return Ok(Some(own));
        }
        let own = identity::get(&*self.home).await.map_err(|err| err.to_string())?;
        let own = own.map(|identity| identity.number);
        if let Some(number) = own {
            *self.own_number.lock().unwrap() = Some(number);
        }
        Ok(own)
    }

    /// cladefs: reloads the host's KMS configuration that reloads live (board 8487; doc-16, What
    /// reloads): the `keys` file of every KMS it holds that is named by a URL, and the master keys
    /// of every bucket KMS it holds, `CLADEFS_MASTER_KEY_FILE`'s (`CLADEFSD_BUCKET_<N>_`'s for
    /// bucket N's), each read again as at the start. Applied whole or not at all: refused, changing
    /// nothing, if a file is not to be read or holds what is not one ([`Unreloaded::Malformed`],
    /// naming it and its line); if it would lose what is in use ([`Unreloaded::InUse`]): a
    /// tenant's KEK renamed (an override added, changed or removed) while the tenant has cells in
    /// a bucket under that KMS, which would no longer open (board 2062), or a master key removed
    /// while the registry of buckets records it as its bucket's or a KEK object is under it; or if
    /// a bucket to check is out of reach ([`Unreloaded::Unreached`]). Its URLs, credentials and
    /// choice of KMS a bucket stay as they are: those are the process's for its life.
    ///
    /// What a host holds of what it renamed (its pools of new cell KEKs, wrapped under the old
    /// names) is its caller's to drop ([`crate::keys::CachingKms::renamed`]). The tenants' cells
    /// it checks are those of the buckets it reaches under each KMS so far: the host's own and
    /// those it asked for; every host reloads, and checks its own.
    pub async fn reload(&self) -> Result<Reloaded, Unreloaded> {
        let _one = self.reloading.lock().await;
        let mut staged = Vec::new();
        let mut override_texts = HashMap::new();
        let named: Vec<(KmsKey, kms::Shared)> = self
            .named
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(key, kms)| Some((key.clone(), kms.get()?.clone())))
            .collect();
        for (key, kms) in &named {
            let Some(names) = kms.names_dyn() else { continue };
            let reread = names
                .reread_in(&mut override_texts)
                .map_err(|why| Unreloaded::Malformed(format!("{key}: {why}")))?;
            staged.extend(reread.map(|reread| (key.clone(), reread)));
        }
        let mut pending = Vec::new();
        let mut master_file_keys: HashMap<PathBuf, Arc<MasterKeys>> = HashMap::new();
        for (number, kms, own) in self.bucket_kmses_held().await? {
            let source = format!("bucket {number}'s bucket KMS");
            let numbered;
            let vars = match own {
                true => &*self.vars,
                false => {
                    numbered = self.vars.numbered(number);
                    &numbered
                }
            };
            let given = match (vars.get(MASTER_KEY_VAR), vars.get(MASTER_KEY_FILE_VAR)) {
                (None, None) => {
                    return Err(Unreloaded::Malformed(format!(
                        "{source}: no master key given any more"
                    )));
                }
                (Some(_), Some(_)) => {
                    return Err(Unreloaded::Malformed(format!(
                        "{source}: both {MASTER_KEY_VAR} and {MASTER_KEY_FILE_VAR} are set: give \
                         the master keys one way"
                    )));
                }
                (Some(text), None) => Arc::new(
                    MasterKeys::parse(text, MASTER_KEY_VAR)
                        .map_err(|why| Unreloaded::Malformed(format!("{source}: {why}")))?,
                ),
                (None, Some(path)) => {
                    let path_buf = PathBuf::from(path);
                    match master_file_keys.get(&path_buf) {
                        Some(cached) => cached.clone(),
                        None => {
                            let loaded = Arc::new(MasterKeys::from_file(Path::new(path)).map_err(
                                |why| Unreloaded::Malformed(format!("{source}: {why}")),
                            )?);
                            master_file_keys.insert(path_buf, loaded.clone());
                            loaded
                        }
                    }
                }
            };
            let held = kms.keys();
            if given.ids() == held.ids() {
                continue;
            }
            let (given_ids, held_ids) = (given.ids(), held.ids());
            let added: Vec<_> =
                given_ids.iter().filter(|id| !held_ids.contains(id)).copied().collect();
            let removed: Vec<_> =
                held_ids.iter().filter(|id| !given_ids.contains(id)).copied().collect();
            pending.push((kms, given, MasterKeysReloaded { bucket: number, added, removed }));
        }
        // Acquire the shared sealing write fence once per unique SharedKeys cell that removes keys,
        // before checking any bucket (hotbed review 117). This blocks both existing and lazily
        // opened buckets sharing that file from sealing under a removed key until reload finishes.
        let mut fences: Vec<(Arc<SharedKeys>, tokio::sync::OwnedRwLockWriteGuard<()>)> = Vec::new();
        for (kms, _, changed) in &pending {
            if changed.removed.is_empty() {
                continue;
            }
            let shared = kms.shared_keys();
            if fences.iter().any(|(locked, _)| Arc::ptr_eq(locked, &shared)) {
                continue;
            }
            let label = format!("bucket {}'s KEK objects being made", changed.bucket);
            let guard = within(label, shared.sealing.clone().write_owned()).await?;
            fences.push((shared, guard));
        }
        // Re-scan held buckets AFTER acquiring the shared sealing fences so any bucket lazily
        // opened against one of those shared master-key files before the fence was acquired is
        // checked for unrecorded/sealed KEKs too (hotbed review 117).
        if !fences.is_empty() {
            for (number, kms, _) in self.bucket_kmses_held().await? {
                if pending.iter().any(|(_, _, changed)| changed.bucket == number) {
                    continue;
                }
                let shared = kms.shared_keys();
                let Some((_, given, sibling)) =
                    pending.iter().find(|(other, _, _)| Arc::ptr_eq(&other.shared_keys(), &shared))
                else {
                    continue;
                };
                pending.push((
                    kms,
                    given.clone(),
                    MasterKeysReloaded {
                        bucket: number,
                        added: sibling.added.clone(),
                        removed: sibling.removed.clone(),
                    },
                ));
            }
        }
        for (kms, _, changed) in &pending {
            if !changed.removed.is_empty() {
                self.unrecorded(changed.bucket, &changed.removed).await?;
                kms.check_unsealed(changed.bucket, &changed.removed).await?;
            }
        }
        for (key, staged) in &staged {
            for &tenant in staged.renamed() {
                self.cell_less(key, tenant).await?;
            }
        }
        let mut reloaded = Reloaded::default();
        for (_, staged) in &staged {
            reloaded.renamed.extend_from_slice(staged.renamed());
        }
        // One reload at a time (`reloading`), and only a reload swaps the overrides; instances
        // sharing one `keys=FILE` cell swap it once (board 10466).
        kms::Staged::apply_all(staged.into_iter().map(|(_, s)| s))
            .expect("reloads are one at a time");
        reloaded.renamed.sort_unstable();
        reloaded.renamed.dedup();
        // Swap every changed master-key cell while every fence is still held.
        for (kms, given, changed) in pending {
            kms.set_keys(given);
            log::warn!(
                "cladefs: bucket {}'s bucket KMS: master keys reloaded, now {}",
                changed.bucket,
                kms.keys().named_ids()
            );
            reloaded.master_keys.push(changed);
        }
        drop(fences);
        if !reloaded.renamed.is_empty() {
            log::warn!(
                "cladefs: the KMS's key overrides reloaded, renaming tenants {:?}",
                reloaded.renamed
            );
        }
        Ok(reloaded)
    }

    /// Every bucket KMS it holds, by its bucket's number (the host's own's, or [`NAMED`] if it has
    /// none yet), and whether it is the host's own, its master keys the host's variables' rather
    /// than its bucket's ([`Vars::numbered`]).
    async fn bucket_kmses_held(
        &self,
    ) -> Result<Vec<(u16, Arc<BucketKms<ObjectBucket>>, bool)>, Unreloaded> {
        let mut all = Vec::new();
        if let HostKms::Bucket(kms) = &self.own {
            let identity = "the host's bucket's identity".to_owned();
            let own = within(identity.clone(), self.own_number())
                .await?
                .map_err(|why| Unreloaded::Unreached(format!("{identity}: {why}")))?;
            all.push((own.unwrap_or(NAMED), kms.clone(), true));
        }
        all.extend(self.chosen.lock().unwrap().iter().filter_map(|(&number, chosen)| {
            let (_, kms) = chosen.get()?;
            Some((number, kms.as_ref()?.clone(), false))
        }));
        Ok(all)
    }

    /// Refuses removing `removed`, master keys of bucket `number`'s bucket KMS, while the registry
    /// of buckets records one as the bucket's (board 8483: hosts of other buckets choose it by
    /// that, and would refuse its cells).
    async fn unrecorded(&self, number: u16, removed: &[MasterKeyId]) -> Result<(), Unreloaded> {
        if number == NAMED {
            return Ok(());
        }
        let entry = match within(format!("bucket {number}"), self.entry(number)).await? {
            Ok(entry) => Some(entry),
            // Unrecorded: none to keep.
            Err(crate::Error::NoBucket(_)) => None,
            Err(err) => {
                let why = format!("bucket {number}: its entry in the registry: {err}");
                return Err(Unreloaded::Unreached(why));
            }
        };
        let recorded = entry.and_then(|entry| entry.details.master_key_id);
        match recorded.filter(|id| removed.iter().any(|key| key.to_string() == *id)) {
            None => Ok(()),
            Some(id) => Err(Unreloaded::InUse(format!(
                "bucket {number}: the registry of buckets records master key {id} as its tenants' \
                 KEKs': rotate them under another first (cladefs-gc --rotate-master-key), then \
                 remove it"
            ))),
        }
    }

    /// Refuses renaming `tenant`'s KEK in the KMS named `key` if the tenant has a cell in a bucket
    /// it reached under that KMS: the host's own, or another it asked for.
    async fn cell_less(&self, key: &KmsKey, tenant: u64) -> Result<(), Unreloaded> {
        let mut buckets = Vec::new();
        if matches!(&self.own, HostKms::Named(url, _) if *url == *key.0) {
            buckets.push(("the host's bucket".to_owned(), Ok(self.home.own())));
        }
        let others: Vec<u16> = (self.chosen.lock().unwrap().iter())
            .filter_map(|(&number, chosen)| {
                let ((of, _), _) = chosen.get()?;
                (of == key).then_some(number)
            })
            .collect();
        for number in others {
            let name = format!("bucket {number}");
            buckets.push((name.clone(), within(name, self.home.numbered(number)).await?));
        }
        for (bucket_name, bucket) in buckets {
            let unreached = |why: String| Unreloaded::Unreached(format!("{bucket_name}: {why}"));
            let bucket = bucket.map_err(|err| unreached(err.to_string()))?;
            let first =
                async { std::pin::pin!(crate::cell::of_tenant(&*bucket, tenant)).next().await };
            match within(bucket_name.clone(), first).await? {
                None => {}
                Some(Err(refused)) => return Err(unreached(refused.to_string())),
                Some(Ok(cell)) => {
                    return Err(Unreloaded::InUse(format!(
                        "{key}: tenant {tenant}'s KEK would be renamed, and it has cells in \
                         {bucket_name} ({cell} among them), whose KEKs are wrapped under its \
                         name now and would open no more (board 2062): keep its name"
                    )));
                }
            }
        }
        Ok(())
    }

    /// The host's own KMS, and its name.
    fn own(&self) -> (KmsKey, kms::Shared) {
        match &self.own {
            HostKms::Named(url, kms) => (KmsKey(url.as_str().into()), kms.clone()),
            HostKms::Bucket(kms) => (KmsKey("the host's bucket KMS".into()), kms.clone()),
        }
    }

    /// What the registry records of the bucket numbered `number`: its copy in the home bucket's
    /// root, or, for a number that copy lacks, the root bucket's registry, the authority, the root
    /// reached by the home bucket's routes, as its requests of the root's cells are. `NoBucket` for
    /// a number neither records. Each read asked once (board 5528).
    async fn entry(&self, number: u16) -> Result<identity::Entry, crate::Error> {
        let copy = identity::registry_once(&*self.home).await?;
        if let Some(entry) = copy.as_ref().and_then(|copy| copy.bucket(number)) {
            return Ok(entry.clone());
        }
        let ours = identity::get_once(&*self.home).await?.ok_or(crate::Error::NoBucket(number))?;
        let root = copy.as_ref().and_then(|copy| copy.bucket(identity::ROOT));
        if ours.number == identity::ROOT || root.is_none() {
            return Err(crate::Error::NoBucket(number));
        }
        let root = self.home.numbered(identity::ROOT).await?;
        let registry = identity::registry_once(&*root).await?;
        let entry = registry.as_ref().and_then(|registry| registry.bucket(number));
        entry.cloned().ok_or(crate::Error::NoBucket(number))
    }

    /// Master keys for a bucket's `vars`, reusing the shared per-host cell (keys and sealing
    /// fence) when `vars` names a master-key file another live bucket KMS on this host already
    /// loaded (board 10466, hotbed review 117).
    fn master_keys_for(&self, vars: &Vars) -> Result<Arc<SharedKeys>, String> {
        match (vars.get(MASTER_KEY_VAR), vars.get(MASTER_KEY_FILE_VAR)) {
            (None, None) => Err(no_kms()),
            (Some(_), Some(_)) => Err(format!(
                "both {MASTER_KEY_VAR} and {MASTER_KEY_FILE_VAR} are set: give the master keys \
                 one way"
            )),
            (Some(text), None) => {
                let keys = MasterKeys::parse(text, MASTER_KEY_VAR)?;
                Ok(SharedKeys::new(keys))
            }
            (None, Some(path)) => {
                let mut files = self.master_key_files.lock().unwrap();
                let key_path = Path::new(path);
                if let Some(shared) = files.get(key_path).and_then(Weak::upgrade) {
                    return Ok(shared);
                }
                let loaded = MasterKeys::from_file(key_path)?;
                let shared = SharedKeys::new(loaded);
                files.insert(key_path.to_path_buf(), Arc::downgrade(&shared));
                Ok(shared)
            }
        }
    }

    /// The KMS of the bucket numbered `number`, another than the host's, chosen (see the type).
    /// Its bucket KMS too, if it is that.
    async fn other(&self, number: u16) -> Result<Chosen, Unfound> {
        let entry = self.entry(number).await.map_err(|err| {
            let why = format!("its entry in the registry: {err}");
            // A read of the registry failing, asked once: the bucket out of reach, which says
            // nothing of its KMS (board 14562).
            match identity::unreached(&err) {
                true => Unfound::Unreached(why),
                false => Unfound::Refused(why),
            }
        })?;
        let vars = self.vars.numbered(number);
        let recorded = entry.details.kms.as_deref();
        if let Some(url) = recorded.or_else(|| vars.get(KMS_VAR)) {
            let trust = recorded.map(|_| Trust::of(&vars)).transpose()?;
            return Ok((self.named(number, &vars, url, trust.as_ref()).await?, None));
        }
        // The bucket is opened only when it keeps its tenants' KEKs (board 5528).
        let shared_keys = self.master_keys_for(&vars)?;
        let keys = shared_keys.keys.lock().unwrap().clone();
        if let Some(id) = &entry.details.master_key_id
            && !keys.ids().iter().any(|held| held.to_string() == *id)
        {
            let refused = format!(
                "its tenants' KEKs are kept under master key {id}, which this host has \
                 not (it has {}): CLADEFSD_BUCKET_{number}_{MASTER_KEY_VAR}",
                keys.named_ids()
            );
            return Err(Unfound::Refused(refused));
        }
        let bucket = self.home.numbered(number).await.map_err(|err| {
            let why = format!(
                "{MASTER_KEY_VAR}: the bucket KMS keeps tenants' KEKs in the host's bucket: \
                 the bucket: {err}"
            );
            // Its identity's read failing, asked once, as its registry entry's.
            match identity::unreached(&err) {
                true => Unfound::Unreached(why),
                false => Unfound::Refused(why),
            }
        })?;
        let kms = Arc::new(BucketKms::with_shared_keys(bucket, shared_keys));
        kms.identify(entry.id);
        let key = KmsKey(format!("the bucket KMS of bucket {number}").into());
        Ok(((key, kms.clone()), Some(kms)))
    }

    /// The instance for a URL and credentials, constructed once.
    async fn named(
        &self,
        number: u16,
        vars: &Vars,
        url: &str,
        trust: Option<&Trust>,
    ) -> Result<(KmsKey, kms::Shared), String> {
        // A bucket's own credentials give it a distinct name (board 8485). The prefix
        // cannot collide with a KMS URL (hotbed review 58).
        let key = if self.vars.own_credentials(number, url) {
            KmsKey(format!("bucket {number}'s credentials: {url}").into())
        } else {
            KmsKey(url.into())
        };
        let recorded = "the registry of buckets";
        if let Some(trust) = trust {
            // This bucket's trust can differ from that of the first caller, including the
            // host's own configuration. Authorize the recorded URL before sharing it.
            kms::check_recorded_url(url, trust).map_err(|why| format!("{recorded}: {why}"))?;
        }
        let (origin, source) = match trust {
            Some(trust) => (Origin::Recorded(trust), recorded),
            None => (Origin::Host, KMS_VAR),
        };
        let slot = self.named.lock().unwrap().entry(key.clone()).or_default().clone();
        let kms = slot
            .get_or_try_init(|| async {
                named(vars, url, origin, source, Some(&self.override_files))
            })
            .await?;
        Ok((key, kms.clone()))
    }

    /// Coalesces cold opens of a bucket. Failed or cancelled lookups can be tried again.
    async fn chosen(&self, number: u16) -> Result<Chosen, Unfound> {
        let chosen = self.chosen.lock().unwrap().entry(number).or_default().clone();
        chosen.get_or_try_init(|| self.other(number)).await.cloned()
    }
}

/// cladefs: how long [`ByBucket::shred`] waits for each other bucket's shredding before it says
/// it failed, the bucket out of reach: shredding again finishes it (board 5528).
pub const SHRED_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

/// cladefs: how long [`ByBucket::reload`] waits for each bucket it checks before it refuses, the
/// bucket out of reach: reloading again does it.
pub const RELOAD_WITHIN: Duration = Duration::from_secs(30);

/// cladefs: what `future` gives, within [`RELOAD_WITHIN`]; or, `what` not answered by then,
/// [`Unreloaded::Unreached`] (hotbed review 79): a bucket's requests are asked again until it
/// answers.
async fn within<T>(what: String, future: impl Future<Output = T>) -> Result<T, Unreloaded> {
    tokio::time::timeout(RELOAD_WITHIN, future)
        .await
        .map_err(|_| Unreloaded::Unreached(format!("{what}: no answer within {RELOAD_WITHIN:?}")))
}

/// cladefs: what [`ByBucket::reload`] changed (board 8487).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reloaded {
    /// The tenants whose KEK's name changed, in order, in any KMS: an override added, changed or
    /// removed.
    pub renamed: Vec<u64>,
    /// The bucket KMSes whose master keys changed.
    pub master_keys: Vec<MasterKeysReloaded>,
}

/// cladefs: a bucket KMS's master keys reloaded ([`ByBucket::reload`]).
#[derive(Debug, PartialEq, Eq)]
pub struct MasterKeysReloaded {
    /// Its bucket's number: [`NAMED`] for the host's own, numbered none yet.
    pub bucket: u16,
    /// The keys added, the newest first.
    pub added: Vec<MasterKeyId>,
    /// The keys removed.
    pub removed: Vec<MasterKeyId>,
}

/// cladefs: why [`ByBucket::reload`] refused, changing nothing.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unreloaded {
    /// A file not to be read, or holding what is not one: naming it, and its line.
    #[error("{0}")]
    Malformed(String),
    /// What it would change is in use: a tenant's KEK renamed while it has cells, a master key
    /// removed while it is held.
    #[error("{0}")]
    InUse(String),
    /// A bucket to check is out of reach.
    #[error("{0}")]
    Unreached(String),
    /// The host has no KMSes by bucket to reload: no bucket of its own to check against.
    #[error("{0}")]
    Unsupported(String),
}

/// cladefs: the first tenant whose KEK object in `bucket`, `name`d so, is under one of `keys`, and
/// that key, if any: a listing of every tenant's, as [`rotate`] lists them, each request answered
/// within [`RELOAD_WITHIN`], however many tenants (hotbed review 78).
async fn sealed_under<B: Bucket>(
    bucket: &B,
    keys: &[MasterKeyId],
    name: &str,
) -> Result<Option<(u64, MasterKeyId)>, Unreloaded> {
    let refused = |refused: bucket::Refused| Unreloaded::Unreached(format!("{name}: {refused}"));
    let mut tenants = std::pin::pin!(bucket::prefixes(bucket, layout::TENANTS.to_owned()));
    while let Some(prefix) = within(name.to_owned(), tenants.next()).await? {
        let Some(tenant) = layout::tenant_of_prefix(&prefix.map_err(refused)?) else { continue };
        let got = within(name.to_owned(), bucket::get(bucket, &layout::kek(tenant))).await?;
        let Some(object) = got.map_err(refused)? else { continue };
        // An object that is no KEK object is opened by no key, removed or kept: none to keep.
        if let Ok(KekObject::Kek { master, .. }) = KekObject::decode(tenant, &object.bytes)
            && keys.contains(&master)
        {
            return Ok(Some((tenant, master)));
        }
    }
    Ok(None)
}

/// What shredding a tenant in a bucket did ([`ByBucket::shredding`]).
enum Shred {
    /// Its bucket KMS's destroy of the tenant's KEK object: done, or refused by the bucket.
    Destroyed(Result<Destroyed, bucket::Refused>),
    /// Under the KMS named by this URL, which keeps the tenant's KEK: to destroy there.
    Named(String),
}

/// What [`ByBucket::other`] chose: the KMS and its name, and the bucket KMS it is, if it is one.
type Chosen = ((KmsKey, kms::Shared), Option<Arc<BucketKms<ObjectBucket>>>);

/// What [`ByBucket::shred`] did, bucket by bucket, each by its number (0: the host's own bucket,
/// numbered none).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Shredded {
    /// The buckets under a bucket KMS, and what destroying the tenant's KEK object there did.
    pub destroyed: Vec<(u16, Destroyed)>,
    /// The buckets under a KMS named by its URL, which keeps the tenant's KEK: to destroy there.
    pub named: Vec<(u16, String)>,
    /// The buckets it could not shred in, and why: shredding again finishes them.
    pub failed: Vec<(u16, String)>,
}

impl Shredded {
    /// Notes what destroying the tenant's KEK object in bucket `number` did: destroyed, or
    /// refused by the bucket (board 5861), shredding again to finish it.
    pub fn destroying(&mut self, number: u16, destroyed: Result<Destroyed, bucket::Refused>) {
        match destroyed {
            Ok(destroyed) => self.destroyed.push((number, destroyed)),
            Err(refused) => self.failed.push((number, refused.to_string())),
        }
    }
}

impl Kmses for ByBucket {
    fn of(&self, bucket: u16) -> BoxFuture<'_, Result<(KmsKey, kms::Shared), Unfound>> {
        Box::pin(async move {
            // An unformatted home bucket has no registry and takes bucket 1 (ROOT) once formatted,
            // so bucket-1 cells (e.g. a standalone image `.bucket` opened before the daemon creates
            // its first cell in an empty `CLADEFSD_BUCKET`) use the host's own KMS (board 6400).
            if bucket == NAMED || self.own_number().await?.unwrap_or(identity::ROOT) == bucket {
                return Ok(self.own());
            }
            self.chosen(bucket).await.map(|(chosen, _)| chosen)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_bucket::Registered;
    use crate::testing::{Direct, SimBucket};
    use extent_keys::kms::trust::LOCAL_VAR;
    use futures::executor::block_on;

    /// The host's variables `pairs` name, as [`ByBucket`] keeps them.
    fn vars_of(pairs: &[(String, String)]) -> Arc<Vars> {
        Vars::new(pairs.iter().cloned()).shared()
    }

    /// Master keys, these, the newest first, copied.
    fn keys_of(keys: &[&Secret]) -> MasterKeys {
        MasterKeys::new(keys.iter().map(|key| Secret::from_bytes(key.expose())).collect()).unwrap()
    }

    /// A bucket whose requests take effect at once.
    fn bucket() -> Arc<Direct> {
        Arc::new(Direct(SimBucket::new()))
    }

    /// A host's bucket KMS over `bucket`, given `keys`.
    fn host<B: Bucket>(bucket: &Arc<B>, keys: &[&Secret]) -> BucketKms<B> {
        BucketKms::new(bucket.clone(), keys_of(keys))
    }

    /// board 8487 (hotbed reviews 78, 79): a reload removing a master key waits for the KEK objects
    /// being made, and checks them: a new tenant's KEK object sealed under the key as the reload
    /// begins, its create not yet answered, refuses the removal once it lands, never missed by a
    /// listing made before it.
    #[test]
    fn removing_master_keys_waits_for_kek_objects_being_made_under_them() {
        use crate::testing::{Fault, SimBucket};
        use std::task::{Context, Poll};
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _entered = runtime.enter();
        let (newest, older) = (Secret::random(), Secret::random());
        let bucket = SimBucket::new();
        let kms = host(&bucket, &[&newest, &older]);
        let removed = [kms.keys().newest().id()];
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let key = Secret::random();
        let mut wrapping = std::pin::pin!(kms.wrap(5, &key));
        // Its requests answered until its KEK object's create waits.
        loop {
            assert!(wrapping.as_mut().poll(&mut context).is_pending());
            if bucket.position("put ").is_some() {
                break;
            }
            bucket.complete(0, Fault::None);
        }
        let mut checking = std::pin::pin!(kms.unsealed(1, &removed));
        // Every request but the create answered, a listing among them if one was made.
        loop {
            assert!(checking.as_mut().poll(&mut context).is_pending(), "checked before the create");
            let others = (0..bucket.waiting()).find(|&at| !bucket.describe(at).starts_with("put "));
            let Some(at) = others else { break };
            bucket.complete(at, Fault::None);
        }
        bucket.complete(bucket.position("put ").unwrap(), Fault::None);
        assert!(matches!(wrapping.as_mut().poll(&mut context), Poll::Ready(Ok(_))));
        let (checked, _) = bucket.served(checking);
        let why = format!(
            "bucket 1: tenant 5's KEK object is under master key {}: rotate them under another \
             first (cladefs-gc --rotate-master-key), then remove it",
            removed[0]
        );
        assert_eq!(checked.map(drop), Err(Unreloaded::InUse(why)));
    }

    fn wrap(kms: &impl Kms, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        block_on(kms.wrap(tenant, key))
    }

    fn unwrap(kms: &impl Kms, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        block_on(kms.unwrap(tenant, wrapped))
    }

    fn refusal<T>(result: Result<T, Error>) -> String {
        result.map(drop).unwrap_err().to_string()
    }

    /// The bytes of `tenant`'s KEK object, if any.
    fn object(bucket: &impl Bucket, tenant: u64) -> Option<Vec<u8>> {
        block_on(bucket::get(bucket, &layout::kek(tenant))).unwrap().map(|object| object.bytes)
    }

    fn keys(bucket: &impl Bucket) -> Vec<String> {
        block_on(bucket.list("")).unwrap().into_iter().map(|meta| meta.key).collect()
    }

    /// AC #1, #5: a tenant's KEK is made on its first wrap, its object created where none was;
    /// keys it wraps unwrap on this host and every other given the master key. A host asks the
    /// bucket nothing more for the tenant while it holds its KEK fresh.
    #[test]
    fn keks_are_made_on_a_tenants_first_wrap_and_unwrap_on_every_host() {
        let master = Secret::random();
        let sim = SimBucket::new();
        let first = host(&sim, &[&master]);
        let key = Secret::random();
        let (wrapped, asked) = sim.served(first.wrap(7, &key));
        let wrapped = wrapped.unwrap();
        let kek = layout::kek(7);
        // Its bucket's identity read once, which what it wraps names (TASK-633.6).
        let identity = layout::IDENTITY;
        assert_eq!(asked, [format!("get {identity}"), format!("get {kek}"), format!("put {kek}")]);
        assert_eq!(wrapped.0.len(), formats::WRAPPED_LEN);
        let made = object(&Direct(sim.clone()), 7).unwrap();
        assert_eq!(made.len(), formats::KEK_OBJECT_LEN);
        assert_eq!(&made[22..30], &keys_of(&[&master]).newest().id().0);
        let (unwrapped, asked) = sim.served(first.unwrap(7, &wrapped));
        assert_eq!((unwrapped.unwrap().expose(), asked), (key.expose(), vec![]));
        let (again, asked) = sim.served(first.wrap(7, &key));
        assert_eq!((again.unwrap(), asked), (wrapped.clone(), vec![]));

        // Another host reads the object once, and unwraps.
        let second = host(&sim, &[&master]);
        let (unwrapped, asked) = sim.served(second.unwrap(7, &wrapped));
        assert_eq!(
            (unwrapped.unwrap().expose(), asked),
            (key.expose(), vec![format!("get {identity}"), format!("get {kek}")])
        );
        assert_eq!(object(&Direct(sim.clone()), 7), Some(made));
        // Each tenant its own KEK: a key wrapped for one is refused to another.
        let theirs = sim.served(second.wrap(8, &key)).0.unwrap();
        assert_eq!(
            refusal(sim.served(first.unwrap(7, &theirs)).0),
            "tenant 7: not authentic: a wrapped key, wrapped for another tenant"
        );
        assert_eq!(keys(&Direct(sim.clone())), [layout::kek(7), layout::kek(8)]);
    }

    /// AC #1, #5: two hosts racing to make a tenant's KEK, each finding none, each creating one at
    /// once: the create that lands second finds the other's object, and takes its KEK, dropping its
    /// own. One KEK object; each unwraps what the other wrapped.
    #[test]
    fn hosts_racing_to_make_a_kek_agree_on_the_one_that_landed() {
        let master = Secret::random();
        let sim = SimBucket::new();
        let (first, second) = (host(&sim, &[&master]), host(&sim, &[&master]));
        let (first_key, second_key) = (Secret::random(), Secret::random());
        let racing = futures::future::join(first.wrap(7, &first_key), second.wrap(7, &second_key));
        let ((first_wrapped, second_wrapped), rounds) = sim.served_in_rounds(racing);
        let kek = layout::kek(7);
        let identity = layout::IDENTITY;
        assert_eq!(
            rounds,
            [
                vec![format!("get {identity}"), format!("get {identity}")],
                vec![format!("get {kek}"), format!("get {kek}")],
                vec![format!("put {kek}"), format!("put {kek}")],
                // The second create, finding an object there, reads it: another's.
                vec![format!("get {kek}")],
            ]
        );
        let (first_wrapped, second_wrapped) = (first_wrapped.unwrap(), second_wrapped.unwrap());
        assert_eq!(keys(&Direct(sim.clone())), [kek]);
        assert_eq!(formats::kek_of(&first_wrapped), formats::kek_of(&second_wrapped));
        let third = host(&Arc::new(Direct(sim.clone())), &[&master]);
        for kms in [&first as &dyn kms::Dyn, &second, &third] {
            let unwrapped = block_on(kms.unwrap_dyn(7, &first_wrapped)).unwrap();
            assert_eq!(unwrapped.expose(), first_key.expose());
            let unwrapped = block_on(kms.unwrap_dyn(7, &second_wrapped)).unwrap();
            assert_eq!(unwrapped.expose(), second_key.expose());
        }
    }

    #[test]
    fn a_kek_read_before_invalidation_cannot_return_or_be_held_afterward() {
        use crate::testing::Fault;
        use std::task::{Context, Poll};

        for shred in [false, true] {
            let sim = SimBucket::new();
            let master = Secret::random();
            let key = Secret::random();
            let writer = host(&sim, &[&master]);
            let wrapped = sim.served(writer.wrap(7, &key)).0.unwrap();
            let reader = host(&sim, &[&master]);
            reader.identify(0);

            let mut reading = std::pin::pin!(reader.unwrap(7, &wrapped));
            let mut context = Context::from_waker(futures::task::noop_waker_ref());
            assert!(reading.as_mut().poll(&mut context).is_pending());
            let get = format!("get {}", layout::kek(7));
            assert_eq!(sim.describe(0), get);
            sim.complete(0, Fault::None);
            // The answer already contains the old KEK, but the reader has yet to consume it.
            if shred {
                assert_eq!(sim.served(reader.destroy(7)).0.unwrap(), Destroyed::Now);
            } else {
                reader.forget(7);
            }
            assert!(matches!(
                reading.as_mut().poll(&mut context),
                Poll::Ready(Err(Error::Io { tenant: 7, source }))
                    if source.kind() == std::io::ErrorKind::Interrupted
            ));
            let (next, asked) = sim.served(reader.unwrap(7, &wrapped));
            assert_eq!(asked, [get]);
            if shred {
                assert!(matches!(next, Err(Error::Destroyed { tenant: 7 })));
            } else {
                assert_eq!(next.unwrap().expose(), key.expose());
            }
        }
    }

    /// AC #2, #5: a host given a new master key before an old one wraps new KEK objects under the
    /// new, and unwraps those of the old; its wraps under a KEK made before stay under it, its
    /// object untouched. Until the rotation, a host given the new key alone can't open the old
    /// objects, refused naming the key; after it, it opens every one, and a host given the old
    /// alone none. A rotation again rewraps nothing; tombstones it counts, and leaves.
    #[test]
    fn rotations_rewrap_kek_objects_under_the_newest_master_key() {
        let (old, new) = (Secret::random(), Secret::random());
        let (old_id, new_id) = (keys_of(&[&old]).newest().id(), keys_of(&[&new]).newest().id());
        let bucket = bucket();
        let before = host(&bucket, &[&old]);
        let key = Secret::random();
        let wrapped = wrap(&before, 7, &key).unwrap();
        let shredded = wrap(&before, 9, &key).unwrap();
        block_on(destroy(&*bucket, 9)).unwrap();
        let made = object(&*bucket, 7).unwrap();

        let both = host(&bucket, &[&new, &old]);
        assert_eq!(unwrap(&both, 7, &wrapped).unwrap().expose(), key.expose());
        assert_eq!(wrap(&both, 7, &key).unwrap(), wrapped);
        assert_eq!(object(&*bucket, 7), Some(made.clone()));
        let other = wrap(&both, 8, &key).unwrap();
        assert_eq!(&object(&*bucket, 8).unwrap()[22..30], &new_id.0);
        let refused = format!(
            "tenant 7: its KEK is wrapped by master key {old_id}, which this host has not (it has \
             {new_id}): CLADEFS_MASTER_KEY"
        );
        assert_eq!(refusal(unwrap(&host(&bucket, &[&new]), 7, &wrapped)), refused);

        let rotated = block_on(rotate(&*bucket, &keys_of(&[&new, &old]))).unwrap();
        let expected = Rotated { rewrapped: 1, newest: 1, destroyed: 1, refused: vec![] };
        assert_eq!(rotated, expected);
        assert_eq!(&object(&*bucket, 7).unwrap()[22..30], &new_id.0);
        assert_eq!(&object(&*bucket, 7).unwrap()[14..22], &made[14..22], "the KEK's ID");
        let after = host(&bucket, &[&new]);
        assert_eq!(unwrap(&after, 7, &wrapped).unwrap().expose(), key.expose());
        assert_eq!(unwrap(&after, 8, &other).unwrap().expose(), key.expose());
        assert_eq!(refusal(unwrap(&after, 9, &shredded)), "tenant 9: its KEK is destroyed");
        let refused = format!(
            "tenant 7: its KEK is wrapped by master key {new_id}, which this host has not (it has \
             {old_id}): CLADEFS_MASTER_KEY"
        );
        assert_eq!(refusal(unwrap(&host(&bucket, &[&old]), 7, &wrapped)), refused);
        let again = block_on(rotate(&*bucket, &keys_of(&[&new, &old]))).unwrap();
        assert_eq!(again, Rotated { rewrapped: 0, newest: 2, destroyed: 1, refused: vec![] });

        // A rotation given keys none of which opens an object leaves it, naming it.
        let lost = block_on(rotate(&*bucket, &keys_of(&[&Secret::random()]))).unwrap();
        assert_eq!((lost.rewrapped, lost.newest, lost.destroyed, lost.refused.len()), (0, 0, 1, 2));
        assert!(
            lost.refused[0].starts_with("tenant 7: its KEK is wrapped by master key "),
            "{lost:?}"
        );
        assert_eq!(&object(&*bucket, 7).unwrap()[22..30], &new_id.0);
    }

    /// AC #3, #5: a tenant shredded has its KEK object replaced by a tombstone: no key wrapped
    /// under its KEK unwraps, its wraps are refused, and none makes it a KEK again. A host holding
    /// its KEK, told nothing, refuses it once it reads the object again ([`FRESH_FOR`]) or is
    /// told (`Kms::forget`); the host shredding forgets at once. Shredding again finds it so; a
    /// tenant never given a KEK, shredded, is never made one.
    #[test]
    fn shredded_tenants_are_destroyed_and_never_made_a_kek_again() {
        let master = Secret::random();
        let bucket = bucket();
        let (shredder, untold, fresh) = (
            host(&bucket, &[&master]),
            host(&bucket, &[&master]),
            host(&bucket, &[&master]).fresh_for(Duration::ZERO),
        );
        let key = Secret::random();
        let wrapped = wrap(&shredder, 7, &key).unwrap();
        for kms in [&untold, &fresh] {
            assert_eq!(unwrap(kms, 7, &wrapped).unwrap().expose(), key.expose());
        }

        assert_eq!(block_on(shredder.destroy(7)), Ok(Destroyed::Now));
        assert_eq!(object(&*bucket, 7), Some(b"fxkt\x01\x02\0\0\0\0\0\0\0\x07".to_vec()));
        for kms in [&shredder, &fresh] {
            assert_eq!(refusal(unwrap(kms, 7, &wrapped)), "tenant 7: its KEK is destroyed");
            assert_eq!(refusal(wrap(kms, 7, &key)), "tenant 7: its KEK is destroyed");
        }
        // Told nothing yet, and its KEK fresh, a host goes on until told.
        assert_eq!(unwrap(&untold, 7, &wrapped).unwrap().expose(), key.expose());
        untold.forget(7);
        assert_eq!(refusal(unwrap(&untold, 7, &wrapped)), "tenant 7: its KEK is destroyed");
        assert_eq!(
            refusal(wrap(&host(&bucket, &[&master]), 7, &key)),
            "tenant 7: its KEK is destroyed"
        );
        assert_eq!(object(&*bucket, 7), Some(KekObject::tombstone(7)));
        assert_eq!(block_on(destroy(&*bucket, 7)).unwrap(), Destroyed::Already);

        assert_eq!(block_on(destroy(&*bucket, 9)).unwrap(), Destroyed::Now);
        assert_eq!(refusal(wrap(&shredder, 9, &key)), "tenant 9: its KEK is destroyed");
        assert_eq!(keys(&*bucket), [layout::kek(7), layout::kek(9)]);
        assert_eq!(object(&*bucket, 9), Some(KekObject::tombstone(9)));
    }

    /// AC #3 (TASK-411.27), as TASK-411.28 left it: a KEK object deleted outright, not shredded,
    /// loses what its KEK wrapped: keys under it are refused, as destroyed until a wrap makes the
    /// tenant a new KEK, then as another KEK's. Its new cells are encrypted under the new one: no
    /// cell is ever in the clear, so no marker refuses the tenant (TASK-411.19's went).
    #[test]
    fn kek_objects_deleted_outright_lose_what_they_wrapped() {
        let master = Secret::random();
        let bucket = bucket();
        let key = Secret::random();
        let wrapped = wrap(&host(&bucket, &[&master]), 8, &key).unwrap();
        block_on(bucket.delete(&layout::kek(8))).unwrap();
        let kms = host(&bucket, &[&master]);
        assert_eq!(refusal(unwrap(&kms, 8, &wrapped)), "tenant 8: its KEK is destroyed");
        assert_eq!(object(&*bucket, 8), None);
        let anew = wrap(&kms, 8, &key).unwrap();
        assert_eq!(
            refusal(unwrap(&kms, 8, &wrapped)),
            "tenant 8: not authentic: a wrapped key, wrapped by another KEK of the tenant's"
        );
        assert_eq!(unwrap(&kms, 8, &anew).unwrap().expose(), key.expose());
    }

    /// AC #4: a host given another master key than the one a tenant's KEK object is under is
    /// refused naming that key's ID and its own, wrapping and unwrapping, and makes no KEK anew:
    /// the object stays as it was.
    #[test]
    fn wrong_master_keys_are_refused_naming_their_ids() {
        let (right, wrong) = (Secret::random(), Secret::random());
        let bucket = bucket();
        let key = Secret::random();
        let wrapped = wrap(&host(&bucket, &[&right]), 7, &key).unwrap();
        let made = object(&*bucket, 7);
        let kms = host(&bucket, &[&wrong]);
        let (right_id, wrong_id) =
            (keys_of(&[&right]).newest().id(), keys_of(&[&wrong]).newest().id());
        let refused = format!(
            "tenant 7: its KEK is wrapped by master key {right_id}, which this host has not (it \
             has {wrong_id}): CLADEFS_MASTER_KEY"
        );
        assert_eq!(refusal(wrap(&kms, 7, &key)), refused);
        assert_eq!(refusal(unwrap(&kms, 7, &wrapped)), refused);
        assert!(matches!(
            unwrap(&kms, 7, &wrapped),
            Err(Error::UnknownMasterKey { tenant: 7, .. })
        ));
        assert_eq!(object(&*bucket, 7), made);
    }

    /// What a test's process logged, every line ([`Logged`]).
    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// A logger keeping every line: of this test's process alone, as each nextest test is one.
    struct Logged;

    impl log::Log for Logged {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            LOGGED.lock().unwrap().push(format!("{} {}", record.target(), record.args()));
        }

        fn flush(&self) {}
    }

    /// Whether `bytes` hold `key`, raw or in hexadecimal, lower or upper case.
    fn holds_key(bytes: &[u8], key: &Secret) -> bool {
        let hex: String = key.expose().iter().map(|byte| format!("{byte:02x}")).collect();
        [key.expose().to_vec(), hex.clone().into_bytes(), hex.to_uppercase().into_bytes()]
            .iter()
            .any(|needle| bytes.windows(needle.len()).any(|window| window == needle))
    }

    /// AC #4: the master keys are written nowhere: no object of the bucket, no refusal, no line
    /// logged holds one, raw or in hexadecimal, through making, racing, rotating, refusing and
    /// shredding; and neither does any object hold a tenant's KEK.
    #[test]
    fn master_keys_are_in_no_object_refusal_or_log_line() {
        log::set_logger(&Logged).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
        let (old, new, wrong) = (Secret::random(), Secret::random(), Secret::random());
        let bucket = bucket();
        let key = Secret::random();
        let mut refusals = Vec::new();
        let first = host(&bucket, &[&old]);
        let wrapped: Vec<_> = (1..=3).map(|tenant| wrap(&first, tenant, &key).unwrap()).collect();
        let both = host(&bucket, &[&new, &old]);
        wrap(&both, 4, &key).unwrap();
        refusals.push(refusal(unwrap(&host(&bucket, &[&wrong]), 1, &wrapped[0])));
        refusals.push(refusal(wrap(&host(&bucket, &[&new]), 2, &key)));
        let rotated = block_on(rotate(&*bucket, &keys_of(&[&new, &old]))).unwrap();
        assert_eq!(rotated.rewrapped, 3);
        refusals.extend(block_on(rotate(&*bucket, &keys_of(&[&wrong]))).unwrap().refused);
        block_on(both.destroy(3)).unwrap();
        refusals.push(refusal(unwrap(&both, 3, &wrapped[2])));
        refusals.push(format!("{:?}", keys_of(&[&new, &old])));
        assert_eq!(refusals.len(), 8, "{refusals:?}");
        log::info!("the test's own line, so that some are kept");

        let objects: Vec<Vec<u8>> = keys(&*bucket)
            .iter()
            .map(|key| block_on(bucket::get(&*bucket, key)).unwrap().unwrap().bytes)
            .collect();
        assert_eq!(objects.len(), 4);
        let logged = LOGGED.lock().unwrap().clone();
        assert!(logged.len() >= 4, "{logged:?}");
        for master in [&old, &new, &wrong] {
            for bytes in &objects {
                assert!(!holds_key(bytes, master), "an object holds a master key");
            }
            for text in refusals.iter().chain(&logged) {
                assert!(!holds_key(text.as_bytes(), master), "{text}");
            }
        }
        for (tenant, bytes) in (1..=2).zip(&objects) {
            let kek = KekObject::decode(tenant, bytes).unwrap().open(&both.keys()).unwrap();
            let wrapped_again = formats::wrap(tenant, 0, &kek, &key);
            assert_eq!(wrapped_again, wrapped[tenant as usize - 1]);
        }
    }

    /// TASK-633.6: each bucket's cells are under that bucket's KMS, as its entry in the registry
    /// and its variables choose it: the host's own bucket's under the host's bucket KMS, a second
    /// bucket's under the bucket KMS over that bucket, by its own master key, a third's under the
    /// KMS its entry names, none of its KEKs in it. A tenant destroyed in the second is refused
    /// there alone; a master key the registry records that the host has not refuses that bucket's
    /// cells, as no revocation; a key one bucket's KMS wrapped is refused by another's as another
    /// bucket's, not as destroyed. Shredding the tenant destroys its KEK in both buckets under a
    /// bucket KMS, naming the third's KMS, which keeps its own.
    #[test]
    fn buckets_cells_are_under_their_own_buckets_kms() {
        use crate::identity::Details;
        use crate::keys::CachingKms;
        use crate::layout::CellId;
        // A runtime's, as shredding waits for each bucket for so long (board 5528).
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let url = |name: &str| {
                std::fs::create_dir(dir.path().join(name)).unwrap();
                format!("file://{}", dir.path().join(name).display())
            };
            let urls = [url("home"), url("other"), url("third")];
            let [home, other, third] =
                urls.clone().map(|url| ObjectBucket::from_url(&url).unwrap());
            let local = dir.path().join("kms");
            extent_keys::kms::LocalKms::open(&local).unwrap().create(7).unwrap();
            let local = format!("local://{}", local.display());
            let (ours, theirs) = ("11".repeat(32), "22".repeat(32));
            let parsed = |hex: &str| MasterKeys::parse(hex, "a test's").unwrap();
            let id = |hex: &str| parsed(hex).newest().id().to_string();
            identity::format(&home, &Details::default()).await.unwrap();
            let recorded = |url: &str, kms: Option<&str>, key: Option<String>| Details {
                url: Some(url.to_owned()),
                kms: kms.map(str::to_owned),
                master_key_id: key,
                ..Details::default()
            };
            let added = recorded(&urls[1], None, Some(id(&theirs)));
            identity::add(&home, &Details::default(), &other, added).await.unwrap();
            let added = recorded(&urls[2], Some(&local), None);
            identity::add(&home, &Details::default(), &third, added).await.unwrap();
            let by_bucket = |key: &str| {
                let vars = vec![
                    ("CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY".to_owned(), key.to_owned()),
                    (LOCAL_VAR.to_owned(), "1".to_owned()),
                ];
                let home = ObjectBucket::from_url(&urls[0]).unwrap();
                let home = Arc::new(home.routed(Arc::new(Registered::new(vars.clone()))));
                let own = HostKms::Bucket(Arc::new(BucketKms::new(home.own(), parsed(&ours))));
                Arc::new(ByBucket::new(home, own, vars_of(&vars)))
            };
            let host = |key: &str| CachingKms::over(by_bucket(key), None);
            let cells = [1u64, 2, 3].map(|number| CellId { tenant: 7, id: number << 48 | 1 });
            let kms = host(&theirs);
            for cell in cells {
                kms.new_kek(cell).await.unwrap();
            }
            async fn kek(bucket: &ObjectBucket) -> Option<Vec<u8>> {
                bucket::get(bucket, &layout::kek(7)).await.unwrap().map(|object| object.bytes)
            }
            let opens = |bytes: &[u8], hex: &str| {
                KekObject::decode(7, bytes).unwrap().open(&parsed(hex)).is_ok()
            };
            let (home_kek, other_kek) = (kek(&home).await.unwrap(), kek(&other).await.unwrap());
            assert_eq!((opens(&home_kek, &ours), opens(&home_kek, &theirs)), (true, false));
            assert_eq!((opens(&other_kek, &theirs), opens(&other_kek, &ours)), (true, false));
            assert_eq!(kek(&third).await, None, "a KEK object in a bucket under a KMS it names");
            // A key bucket 2's KMS wrapped, asked of bucket 1's: refused as another bucket's, not
            // as destroyed, though bucket 1 has no KEK object of tenant 8's.
            let kmses = by_bucket(&theirs);
            let (_, second) = kmses.of(2).await.unwrap();
            let wrapped = second.wrap_dyn(8, &Secret::random()).await.unwrap();
            let (_, first) = kmses.of(1).await.unwrap();
            let refused = first.unwrap_dyn(8, &wrapped).await.map(drop).unwrap_err();
            let other_id = identity::get(&other).await.unwrap().unwrap().id;
            assert_eq!(
                refused.to_string(),
                format!(
                    "tenant 8: a key wrapped in the bucket KMS of another bucket ({other_id:016x})"
                )
            );
            assert!(second.unwrap_dyn(8, &wrapped).await.is_ok());

            assert_eq!(destroy(&other, 7).await.unwrap(), Destroyed::Now);
            let fresh = host(&theirs);
            let mut made = Vec::new();
            for cell in cells {
                made.push(fresh.new_kek(cell).await.map(drop));
            }
            let destroyed = crate::Error::Kms("tenant 7: its KEK is destroyed".into());
            assert_eq!(made, [Ok(()), Err(destroyed.clone()), Ok(())]);
            assert_eq!(cells.map(|cell| fresh.refuses(cell)), [None, Some(destroyed), None]);

            let wrong = host(&ours);
            let refused = crate::Error::Kms(format!(
                "tenant 7: the KMS of bucket 2: its tenants' KEKs are kept under master key {}, \
                 which this host has not (it has {}): CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY",
                id(&theirs),
                id(&ours)
            ));
            assert_eq!(wrong.new_kek(cells[1]).await.map(drop), Err(refused));
            assert_eq!(wrong.refuses(cells[1]), None);
            assert_eq!(wrong.new_kek(cells[0]).await.map(drop), Ok(()));

            // Shredding reaches every bucket under a bucket KMS, and names those under another.
            let shredded = by_bucket(&theirs).shred(7).await;
            let expected = Shredded {
                destroyed: vec![(1, Destroyed::Now), (2, Destroyed::Already)],
                named: vec![(3, local.clone())],
                failed: vec![],
            };
            assert_eq!(shredded, expected);
            for bucket in [&home, &other] {
                let object = KekObject::decode(7, &kek(bucket).await.unwrap()).unwrap();
                assert!(matches!(object, KekObject::Tombstone { .. }), "a KEK left");
            }
        });
    }
    /// board 8483: a rotation of a bucket's master key, run as `cladefs-gc --rotate-master-key` runs
    /// it on that bucket, records the newest key in its entry of the registry, the root's and
    /// every copy: a host of another bucket given the new key alone for it, restarted, opens its
    /// cells, which it refused when the entry named the key dropped. A bucket recorded with no
    /// master key records none; a rotation refusing a KEK object records nothing. No test combined
    /// a rotation with a host of another bucket after the old key was gone.
    #[test]
    fn rotations_record_the_newest_master_key_for_hosts_of_other_buckets() {
        use crate::identity::Details;
        use crate::keys::CachingKms;
        use crate::layout::CellId;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let url = |name: &str| {
                std::fs::create_dir(dir.path().join(name)).unwrap();
                format!("file://{}", dir.path().join(name).display())
            };
            let urls = [url("home"), url("other"), url("plain")];
            let [home, other, plain] =
                urls.clone().map(|url| ObjectBucket::from_url(&url).unwrap());
            let (ours, old, new) = ("11".repeat(32), "22".repeat(32), "33".repeat(32));
            let parsed = |hex: &str| MasterKeys::parse(hex, "a test's").unwrap();
            let id = |hex: &str| parsed(hex).newest().id().to_string();
            let at = |url: &str, key: Option<String>| Details {
                url: Some(url.to_owned()),
                master_key_id: key,
                ..Details::default()
            };
            identity::format(&home, &at(&urls[0], None)).await.unwrap();
            identity::add(&home, &Details::default(), &other, at(&urls[1], Some(id(&old))))
                .await
                .unwrap();
            let (_, registry) =
                identity::add(&home, &Details::default(), &plain, at(&urls[2], None))
                    .await
                    .unwrap();
            for bucket in [&home, &other, &plain] {
                identity::spread(&registry, bucket).await.unwrap();
            }
            // A host of bucket 1, given bucket 2's master keys as `keys`, as it restarts.
            let host = |keys: &str| {
                let vars =
                    vec![("CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY".to_owned(), keys.to_owned())];
                let home = ObjectBucket::from_url(&urls[0]).unwrap();
                let home = Arc::new(home.routed(Arc::new(Registered::new(vars.clone()))));
                let own = HostKms::Bucket(Arc::new(BucketKms::new(home.own(), parsed(&ours))));
                Arc::new(ByBucket::new(home, own, vars_of(&vars)))
            };
            let cell = CellId { tenant: 7, id: 2 << 48 | 1 };
            let (secret, wrapped) = CachingKms::over(host(&old), None).new_kek(cell).await.unwrap();

            // Rotated where bucket 2 is the home bucket, as cladefs-gc runs.
            let gc = |url: &str| {
                ObjectBucket::from_url(url).unwrap().routed(Arc::new(Registered::new(Vec::new())))
            };
            let both = parsed(&format!("{new},{old}"));
            let (rotated, recorded) = rotate_recorded(&gc(&urls[1]), &both).await.unwrap();
            assert_eq!((rotated.rewrapped, rotated.refused.len()), (1, 0));
            assert_eq!(recorded, Recorded::Recorded { failed: vec![] });
            for bucket in [&home, &other, &plain] {
                let copy = identity::registry(bucket).await.unwrap().unwrap();
                let entry = copy.bucket(2).unwrap();
                assert_eq!(entry.details.master_key_id, Some(id(&new)));
                assert_eq!(copy.version, registry.version + 1);
            }
            let (_, second) = host(&new).of(2).await.unwrap();
            assert_eq!(second.unwrap_dyn(7, &wrapped).await.unwrap().expose(), secret.expose());

            // A bucket recorded with no master key records none.
            let (_, recorded) = rotate_recorded(&gc(&urls[2]), &both).await.unwrap();
            assert_eq!(recorded, Recorded::Unrecorded);
            let copy = identity::registry(&home).await.unwrap().unwrap();
            assert_eq!(copy.bucket(3).unwrap().details.master_key_id, None);
            // A rotation refusing a KEK object records nothing.
            let wrong = parsed(&"44".repeat(32));
            let (rotated, recorded) = rotate_recorded(&gc(&urls[1]), &wrong).await.unwrap();
            assert_eq!((rotated.refused.len(), recorded), (1, Recorded::Refused));
            let copy = identity::registry(&home).await.unwrap().unwrap();
            assert_eq!(copy.bucket(2).unwrap().details.master_key_id, Some(id(&new)));

            // hotbed review 47: a bucket out of reach once opened, whose requests are asked
            // again until it answers, is said failed within the wait; the other copies record
            // the rotation all the same.
            let newer = "55".repeat(32);
            let rotating = gc(&urls[1]);
            rotating.numbered(3).await.unwrap();
            let plain_dir = dir.path().join("plain");
            std::fs::rename(&plain_dir, dir.path().join("gone")).unwrap();
            std::fs::write(&plain_dir, b"").unwrap();
            let keys = parsed(&format!("{newer},{new}"));
            let within = Duration::from_secs(1);
            let (rotated, recorded) =
                rotate_recorded_within(&rotating, &keys, within).await.unwrap();
            assert_eq!(rotated.rewrapped, 1);
            let failed = vec![(3, "no answer within 1s".to_owned())];
            assert_eq!(recorded, Recorded::Recorded { failed });
            for bucket in [&home, &other] {
                let copy = identity::registry(bucket).await.unwrap().unwrap();
                assert_eq!(copy.bucket(2).unwrap().details.master_key_id, Some(id(&newer)));
            }
        });
    }

    /// board 8485 AC #3: buckets naming OpenBaos of their own each call theirs with their own
    /// token, never the host's: bucket 2 under a dev server whose token is A, bucket 3 under another
    /// whose token is B, the host's variables naming a token C neither knows; the cells of both
    /// make and unwrap their KEKs. AC #2: bucket 4, naming bucket 2's URL with the host's token, has
    /// a KMS of its own, which OpenBao refuses as C, bucket 2's untouched. Before, every bucket's
    /// KMS read the process's environment, which named no token at all.
    #[test]
    fn buckets_call_their_own_openbao_with_their_own_tokens() {
        use crate::identity::Details;
        use crate::keys::CachingKms;
        use crate::layout::CellId;
        use extent_keys::kms::openbao::TOKEN_ENV;
        use extent_keys::kms::trust::HOSTS_VAR;
        use extent_keys::testing::openbao::DevServer;
        let (Some(a), Some(b)) = (DevServer::start(), DevServer::start()) else { return };
        let token = |server: &DevServer| {
            server.transit("transit");
            server.bao(&["write", "-f", "transit/keys/cladefs-tenant-7"]);
            server.policy("cladefs", &DevServer::cladefs_policy("transit", "cladefs-tenant-*"));
            server.token("cladefs")
        };
        let (token_a, token_b) = (token(&a), token(&b));
        let query = "key=cladefs-tenant-{tenant}";
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let url = |name: &str| {
                std::fs::create_dir(dir.path().join(name)).unwrap();
                format!("file://{}", dir.path().join(name).display())
            };
            let urls = ["home", "two", "three", "four"].map(url);
            let buckets = urls.clone().map(|url| ObjectBucket::from_url(&url).unwrap());
            identity::format(&buckets[0], &Details::default()).await.unwrap();
            let kmses = [a.url("transit", query), b.url("transit", query), a.url("transit", query)];
            for (at, kms) in (1..).zip(kmses) {
                let details =
                    Details { url: Some(urls[at].clone()), kms: Some(kms), ..Details::default() };
                identity::add(&buckets[0], &Details::default(), &buckets[at], details)
                    .await
                    .unwrap();
            }
            let ours = "11".repeat(32);
            let vars: Vec<(String, String)> = [
                (MASTER_KEY_VAR.to_owned(), ours.clone()),
                (TOKEN_ENV.to_owned(), "s.the-hosts-own".to_owned()),
                (format!("CLADEFSD_BUCKET_2_{TOKEN_ENV}"), token_a.clone()),
                (format!("CLADEFSD_BUCKET_3_{TOKEN_ENV}"), token_b.clone()),
                (HOSTS_VAR.to_owned(), format!("127.0.0.1:{} 127.0.0.1:{}", a.port(), b.port())),
            ]
            .into();
            let home = ObjectBucket::from_url(&urls[0]).unwrap();
            let home = Arc::new(home.routed(Arc::new(Registered::new(vars.clone()))));
            let keys = MasterKeys::parse(&ours, "a test's").unwrap();
            let own = HostKms::Bucket(Arc::new(BucketKms::new(home.own(), keys)));
            let by_bucket = Arc::new(ByBucket::new(home, own, vars_of(&vars)));
            let kms = CachingKms::over(by_bucket.clone(), None);
            for number in [2u16, 3] {
                let cell = CellId { tenant: 7, id: u64::from(number) << 48 | 1 };
                let (secret, wrapped) = kms.new_kek(cell).await.unwrap();
                let (_, theirs) = by_bucket.of(number).await.unwrap();
                assert_eq!(theirs.unwrap_dyn(7, &wrapped).await.unwrap().expose(), secret.expose());
            }
            let ((two, _), (four, hosts)) =
                (by_bucket.of(2).await.unwrap(), by_bucket.of(4).await.unwrap());
            assert_eq!(four.0.as_ref(), a.url("transit", query));
            assert_eq!(two.0.as_ref(), format!("bucket 2's credentials: {}", four.0));
            // No URL a KMS takes is named so: one beginning as a bucket's credentials do is none.
            let theirs = format!("bucket 2's credentials: {}", a.url("transit", query));
            let refused = kms::from_url_with(&theirs, &by_bucket.vars, Origin::Host).err();
            assert!(refused.is_some_and(|why| why.contains("names no KMS")), "{theirs}");
            let refused = hosts.wrap_dyn(7, &Secret::random()).await.map(drop).unwrap_err();
            assert!(refused.to_string().contains("OpenBao refuses cladefs's token"), "{refused}");
        });
    }

    #[test]
    fn sequential_and_concurrent_bucket_opens_reuse_a_named_kms() {
        use crate::identity::Details;
        use extent_keys::kms::openbao::TOKEN_FILE_ENV;

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let urls = ["home", "two", "three"].map(|name| {
                let path = dir.path().join(name);
                std::fs::create_dir(&path).unwrap();
                format!("file://{}", path.display())
            });
            let [home, two, three] =
                urls.each_ref().map(|url| ObjectBucket::from_url(url).unwrap());
            let token = dir.path().join("token");
            let url = "openbao://127.0.0.1:8200/transit?key=k{tenant}";
            let details = Details::default();
            identity::format(&home, &details).await.unwrap();
            for (number, bucket) in [(1, two), (2, three)] {
                let member = Details {
                    url: Some(urls[number].clone()),
                    kms: Some(url.to_owned()),
                    ..Details::default()
                };
                identity::add(&home, &details, &bucket, member).await.unwrap();
            }
            let vars = vars_of(&[
                (TOKEN_FILE_ENV.to_owned(), token.display().to_string()),
                (kms::trust::HOSTS_VAR.to_owned(), "127.0.0.1:8200".to_owned()),
            ]);
            let home = Arc::new(home.routed(Arc::new(Registered::new(Vec::new()))));
            let router = || {
                let own = HostKms::Bucket(Arc::new(BucketKms::new(
                    home.own(),
                    keys_of(&[&Secret::random()]),
                )));
                ByBucket::new(home.clone(), own, vars.clone())
            };

            std::fs::write(&token, "s.test").unwrap();
            let sequential = router();
            let (key, first) = sequential.of(2).await.unwrap();
            // A second construction would try to read the token file again.
            std::fs::remove_file(&token).unwrap();
            let (other_key, second) = sequential.of(3).await.unwrap();
            assert_eq!(key, other_key);
            assert!(Arc::ptr_eq(&first, &second));

            std::fs::write(&token, "s.test").unwrap();
            let concurrent = router();
            let (two, three, again) =
                futures::join!(concurrent.of(2), concurrent.of(3), concurrent.of(2));
            let (key, first) = two.unwrap();
            for other in [three, again] {
                let (other_key, other) = other.unwrap();
                assert_eq!(key, other_key);
                assert!(Arc::ptr_eq(&first, &other));
            }
        });
    }

    /// board 8525: a KMS URL a bucket's entry in the registry records reaches what the host allows
    /// alone: `local://`, the host allowing no fake, refuses the bucket's cells, its directory never
    /// made; OpenBao in the clear, at a host none names, too, before the host's token goes there.
    #[test]
    fn registries_naming_kmses_the_host_allows_not_are_refused() {
        use crate::identity::Details;
        use extent_keys::kms::openbao::TOKEN_ENV;
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let url = |name: &str| {
                std::fs::create_dir(dir.path().join(name)).unwrap();
                format!("file://{}", dir.path().join(name).display())
            };
            let urls = ["home", "two", "three"].map(url);
            let buckets = urls.clone().map(|url| ObjectBucket::from_url(&url).unwrap());
            let local = dir.path().join("kms");
            let local_url = format!("local://{}", local.display());
            identity::format(&buckets[0], &Details::default()).await.unwrap();
            let kmses =
                [local_url.clone(), "openbao://10.9.9.9:8200/transit?key=k{tenant}".to_owned()];
            for (at, kms) in (1..).zip(kmses) {
                let details =
                    Details { url: Some(urls[at].clone()), kms: Some(kms), ..Details::default() };
                identity::add(&buckets[0], &Details::default(), &buckets[at], details)
                    .await
                    .unwrap();
            }
            let vars = vec![(TOKEN_ENV.to_owned(), "s.the-hosts-own".to_owned())];
            let home = ObjectBucket::from_url(&urls[0]).unwrap();
            let home = Arc::new(home.routed(Arc::new(Registered::new(vars.clone()))));
            let own = HostKms::Bucket(Arc::new(BucketKms::new(
                home.own(),
                MasterKeys::parse(&"11".repeat(32), "a test's").unwrap(),
            )));
            let by_bucket = ByBucket::new(home, own, vars_of(&vars));
            let refused = by_bucket.of(2).await.map(drop).unwrap_err();
            let Unfound::Refused(refused) = refused else { panic!("{refused:?}: no refusal") };
            assert!(refused.starts_with("the registry of buckets: \"local://"), "{refused}");
            assert!(refused.ends_with("only if CLADEFSD_KMS_LOCAL is 1"), "{refused}");
            assert!(!local.exists(), "local:// made its directory");
            let own = kms::from_url_with(&local_url, &by_bucket.vars, Origin::Host).unwrap();
            let by_bucket_with_own = ByBucket::new(
                by_bucket.home.clone(),
                HostKms::Named(local_url, own),
                by_bucket.vars.clone(),
            );
            let with_own = by_bucket_with_own.of(2).await.map(drop).unwrap_err();
            assert_eq!(with_own, Unfound::Refused(refused));
            let refused = by_bucket.of(3).await.map(drop).unwrap_err();
            let Unfound::Refused(refused) = refused else { panic!("{refused:?}: no refusal") };
            assert!(
                refused.ends_with(
                    "10.9.9.9:8200: a KMS URL the registry of buckets records reaches only the \
                     hosts CLADEFSD_KMS_HOSTS names, or the host's own CLADEFS_KMS's"
                ),
                "{refused}"
            );
        });
    }

    /// board 6400: an unformatted home bucket serves bucket 1 (ROOT) with its own KMS while
    /// refusing other bucket numbers, and does not cache `None` so formatting it later (e.g. as
    /// bucket 2 via `identity::add`) takes effect on subsequent lookups.
    #[test]
    fn unformatted_home_serves_root_and_learns_number_once_formatted() {
        use crate::identity::Details;
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mk = |name: &str| {
                std::fs::create_dir(dir.path().join(name)).unwrap();
                format!("file://{}", dir.path().join(name).display())
            };
            let (root_url, home_url) = (mk("root"), mk("home"));
            let root_kms_dir = dir.path().join("root-kms");
            let home_kms_dir = dir.path().join("home-kms");
            extent_keys::kms::LocalKms::open(&root_kms_dir).unwrap().create(1).unwrap();
            extent_keys::kms::LocalKms::open(&home_kms_dir).unwrap().create(1).unwrap();
            let root_kms_url = format!("local://{}", root_kms_dir.display());
            let home_kms_url = format!("local://{}", home_kms_dir.display());
            let home_kms: kms::Shared =
                Arc::new(extent_keys::kms::LocalKms::open(&home_kms_dir).unwrap());
            let home_routed = Arc::new(
                ObjectBucket::from_url(&home_url)
                    .unwrap()
                    .routed(Arc::new(Registered::new(Vec::new()))),
            );
            let kmses = ByBucket::new(
                home_routed,
                HostKms::Named(home_kms_url.clone(), home_kms),
                vars_of(&[(LOCAL_VAR.to_owned(), "1".to_owned())]),
            );
            // Before `home` is formatted: bucket 1 resolves to `own`, bucket 2 is refused.
            let (key, _) = kmses.of(identity::ROOT).await.unwrap();
            assert_eq!(key.0.as_ref(), home_kms_url.as_str());
            assert!(kmses.of(2).await.is_err());

            // Now format `root` (bucket 1) and add `home` as bucket 2; `own_number` must NOT have
            // cached `None`, so bucket 2 now resolves to `own` and bucket 1 to `root`'s KMS.
            let root_bucket = ObjectBucket::from_url(&root_url).unwrap();
            let home_bucket = ObjectBucket::from_url(&home_url).unwrap();
            let root_details = Details {
                url: Some(root_url.clone()),
                kms: Some(root_kms_url.clone()),
                ..Details::default()
            };
            let home_details = Details {
                url: Some(home_url.clone()),
                kms: Some(home_kms_url.clone()),
                ..Details::default()
            };
            identity::format(&root_bucket, &root_details).await.unwrap();
            let (_, registry) =
                identity::add(&root_bucket, &root_details, &home_bucket, home_details)
                    .await
                    .unwrap();
            identity::spread(&registry, &home_bucket).await.unwrap();

            let (key2, _) = kmses.of(2).await.unwrap();
            assert_eq!(key2.0.as_ref(), home_kms_url.as_str());
            let (key1, _) = kmses.of(identity::ROOT).await.unwrap();
            assert_eq!(key1.0.as_ref(), root_kms_url.as_str());
        });
    }

    /// board 5528: shredding a tenant goes on past buckets out of reach, saying each failed, and
    /// shreds those after them: of four buckets, the host's (1), one under its bucket KMS out of
    /// reach since that KMS was opened (2), one under its bucket KMS (3), one under a KMS its entry
    /// names, out of reach and never opened (4). 2 failed within the wait; 1 and 3 destroyed; 4
    /// named, its bucket never asked (asked, it would have failed).
    #[test]
    fn shreds_go_on_past_buckets_out_of_reach() {
        use crate::identity::Details;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = |number: u16| dir.path().join(format!("bucket-{number}"));
            let url = |number| format!("file://{}", path(number).display());
            let buckets = [1, 2, 3, 4].map(|number| {
                std::fs::create_dir(path(number)).unwrap();
                ObjectBucket::from_url(&url(number)).unwrap()
            });
            let local = dir.path().join("kms");
            extent_keys::kms::LocalKms::open(&local).unwrap().create(7).unwrap();
            let local = format!("local://{}", local.display());
            let root = &buckets[0];
            identity::format(root, &Details::default()).await.unwrap();
            for (number, bucket) in (2..).zip(&buckets[1..]) {
                let kms = (number == 4).then(|| local.clone());
                let details = Details { url: Some(url(number)), kms, ..Details::default() };
                identity::add(root, &Details::default(), bucket, details).await.unwrap();
            }
            let master = "11".repeat(32);
            let vars = vec![
                (MASTER_KEY_VAR.to_owned(), master.clone()),
                (LOCAL_VAR.to_owned(), "1".to_owned()),
            ];
            let home = ObjectBucket::from_url(&url(1)).unwrap();
            let home = Arc::new(home.routed(Arc::new(Registered::new(vars.clone()))));
            let keys = MasterKeys::parse(&master, "a test's").unwrap();
            let own = HostKms::Bucket(Arc::new(BucketKms::new(home.own().clone(), keys)));
            let mut by_bucket = ByBucket::new(home, own, vars_of(&vars));
            by_bucket.shred_within = std::time::Duration::from_secs(1);
            by_bucket.of(2).await.unwrap();
            // 2 and 4 out of reach: their directories files.
            for number in [2, 4] {
                std::fs::rename(path(number), dir.path().join(format!("gone-{number}"))).unwrap();
                std::fs::write(path(number), b"").unwrap();
            }
            let expected = Shredded {
                destroyed: vec![(1, Destroyed::Now), (3, Destroyed::Now)],
                named: vec![(4, local.clone())],
                failed: vec![(2, "no answer within 1s".to_owned())],
            };
            assert_eq!(by_bucket.shred(7).await, expected);
        });
    }

    /// The KMS a host's variables choose: `CLADEFS_KMS`'s if it names one, a master key or not;
    /// else the bucket KMS over the host's bucket, given a master key; else none, refused naming
    /// both variables (TASK-411.28). A master key with no bucket, or two ways of giving it, is
    /// refused naming why; so is a URL of no KMS.
    #[test]
    fn hosts_choose_cladefs_kms_then_the_bucket_kms_and_refuse_none() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = bucket();
        let chosen = |pairs: &[(&str, &str)], bucket: Option<&Arc<Direct>>| {
            let vars =
                Vars::new(pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())));
            from_vars(&vars, || {
                bucket.cloned().ok_or_else(|| "CLADEFSD_BUCKET names none".to_owned())
            })
        };
        let kind = |chosen: &Result<HostKms<Direct>, String>| match chosen {
            Ok(HostKms::Named(..)) => "named".to_owned(),
            Ok(HostKms::Bucket(_)) => "bucket".to_owned(),
            Err(why) => why.clone(),
        };
        assert_eq!(
            kind(&chosen(&[], Some(&bucket))),
            "no KMS: CLADEFS_KMS names none, and neither CLADEFS_MASTER_KEY nor CLADEFS_MASTER_KEY_FILE \
             gives a master key; every cell is encrypted (doc-16): set CLADEFS_KMS to a KMS's URL, or \
             CLADEFS_MASTER_KEY to a master key (openssl rand -hex 32) to keep tenants' KEKs in the \
             bucket"
        );
        assert_eq!(kind(&chosen(&[], Some(&bucket))), no_kms());
        let empty = chosen(&[(KMS_VAR, "")], Some(&bucket));
        assert_eq!(kind(&empty), no_kms(), "an empty CLADEFS_KMS names none");
        let hex: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
        let host = chosen(&[(MASTER_KEY_VAR, &hex)], Some(&bucket)).unwrap();
        let kms = host.bucket().unwrap();
        assert!(Arc::ptr_eq(kms.bucket(), &bucket));
        assert_eq!(kms.keys().named_ids(), "8b1917a7f2bf1306");
        let key = Secret::random();
        let wrapped = block_on(host.shared().wrap_dyn(7, &key)).unwrap();
        assert_eq!(unwrap(&**kms, 7, &wrapped).unwrap().expose(), key.expose());
        assert_eq!(keys(&*bucket), [layout::kek(7)]);
        assert_eq!(
            kind(&chosen(&[(MASTER_KEY_VAR, &hex)], None)),
            "CLADEFS_MASTER_KEY: the bucket KMS keeps tenants' KEKs in the host's bucket: \
             CLADEFSD_BUCKET names none"
        );
        let both = [(MASTER_KEY_VAR, hex.as_str()), (MASTER_KEY_FILE_VAR, "/nonexistent")];
        assert_eq!(
            kind(&chosen(&both, Some(&bucket))),
            "both CLADEFS_MASTER_KEY and CLADEFS_MASTER_KEY_FILE are set: give the master keys one way"
        );
        let local = format!("local://{}", dir.path().display());
        let named = [(KMS_VAR, local.as_str()), (MASTER_KEY_VAR, hex.as_str())];
        assert_eq!(kind(&chosen(&named, None)), "named");
        assert_eq!(
            kind(&chosen(&[(KMS_VAR, "vault://key")], None)),
            "CLADEFS_KMS: \"vault://key\" names no KMS: local:///DIR, openbao://, aws-kms://, \
             gcp-kms:// or azure-kv:// (doc-16)"
        );
    }

    /// TASK-578 (board 2552): a `BucketKms::kek` read that raced `forget` / `destroy` must not
    /// cache the pre-forget KEK bytes back into `BucketKms` once the in-flight GET returns.
    #[test]
    fn forget_during_inflight_kek_read_does_not_resurrect_destroyed_kek() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::Notify;

        struct GatedBucket {
            inner: Direct,
            armed: AtomicBool,
            entered: Notify,
            release: Notify,
        }

        impl crate::bucket::Bucket for GatedBucket {
            async fn get(&self, key: &str) -> Result<crate::bucket::Object, crate::bucket::Error> {
                let result = self.inner.get(key).await;
                if key == layout::kek(7) && self.armed.swap(false, Ordering::AcqRel) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                result
            }

            async fn get_range(
                &self,
                key: &str,
                range: crate::bucket::ByteRange,
            ) -> Result<Vec<u8>, crate::bucket::Error> {
                self.inner.get_range(key, range).await
            }

            async fn head(&self, key: &str) -> Result<crate::bucket::Meta, crate::bucket::Error> {
                self.inner.head(key).await
            }

            async fn list(
                &self,
                prefix: &str,
            ) -> Result<Vec<crate::bucket::Meta>, crate::bucket::Error> {
                self.inner.list(prefix).await
            }

            async fn list_delimited(
                &self,
                prefix: &str,
                page: crate::bucket::Page<'_>,
            ) -> Result<crate::bucket::Listing, crate::bucket::Error> {
                self.inner.list_delimited(prefix, page).await
            }

            async fn put(
                &self,
                key: &str,
                bytes: bytes::Bytes,
                mode: crate::bucket::PutMode,
            ) -> Result<crate::bucket::Version, crate::bucket::Error> {
                self.inner.put(key, bytes, mode).await
            }

            async fn delete(&self, key: &str) -> Result<(), crate::bucket::Error> {
                self.inner.delete(key).await
            }

            fn now(&self) -> u64 {
                self.inner.now()
            }
        }

        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let gated = Arc::new(GatedBucket {
                inner: Direct(crate::testing::SimBucket::new()),
                armed: AtomicBool::new(false),
                entered: Notify::new(),
                release: Notify::new(),
            });
            let master = MasterKeys::new(vec![Secret::random()]).unwrap();
            let kms = Arc::new(BucketKms::new(gated.clone(), master));
            let plain = Secret::random();
            let wrapped = kms.wrap(7, &plain).await.unwrap();
            // Clear the cache so the next unwrap fetches `layout::kek(7)` from the bucket.
            kms.forget(7);
            gated.armed.store(true, Ordering::Release);

            let kms_reader = kms.clone();
            let wrapped_reader = wrapped.clone();
            let inflight = tokio::spawn(async move { kms_reader.unwrap(7, &wrapped_reader).await });
            gated.entered.notified().await;

            // While `inflight` holds the pre-destroy bytes, destroy & forget tenant 7.
            assert_eq!(kms.destroy(7).await, Ok(Destroyed::Now));
            gated.release.notify_one();

            let outcome = inflight.await.unwrap();
            assert!(
                outcome.is_err(),
                "in-flight unwrap racing destroy/forget must not succeed or resurrect the KEK"
            );
            assert!(
                kms.unwrap(7, &wrapped).await.is_err(),
                "a subsequent unwrap must still see tenant 7 destroyed"
            );
        });
    }

    /// board 10466: every KMS instance on a host naming the same `keys=FILE` or master-key file
    /// shares one overrides / master-keys cell, read on first use and changed only by `reload()`.
    /// Reaching a new bucket with its own credentials after the file was edited (and before reload,
    /// or after a refused reload) still sees the old names/keys until a valid reload applies.
    #[test]
    fn lazy_bucket_kms_shares_keys_and_master_key_files_until_reloaded() {
        use crate::identity::Details;
        use crate::layout::CellId;
        use extent_keys::kms::openbao::TOKEN_ENV;

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mk = |name: &str| {
                let path = dir.path().join(name);
                std::fs::create_dir(&path).unwrap();
                format!("file://{}", path.display())
            };

            // Part A: `keys=FILE` shared across buckets with distinct credentials.
            let urls = [mk("a-home"), mk("a-two")];
            let [home, two] = urls.each_ref().map(|u| ObjectBucket::from_url(u).unwrap());
            identity::format(&home, &Details::default()).await.unwrap();
            identity::add(
                &home,
                &Details::default(),
                &two,
                Details { url: Some(urls[1].clone()), ..Details::default() },
            )
            .await
            .unwrap();

            let keys_file = dir.path().join("keys.txt");
            std::fs::write(&keys_file, "7 key-old\n").unwrap();
            let kms_url = format!(
                "openbao://127.0.0.1:8200/transit?key=t-{{tenant}}&keys={}",
                keys_file.display()
            );
            let vars = vec![
                (KMS_VAR.to_owned(), kms_url.clone()),
                (TOKEN_ENV.to_owned(), "s.host-token".to_owned()),
                (format!("CLADEFSD_BUCKET_2_{TOKEN_ENV}"), "s.bucket2-token".to_owned()),
            ];
            let home_routed = Arc::new(home.routed(Arc::new(Registered::new(Vec::new()))));
            let vars_shared = vars_of(&vars);
            let own = from_vars(&vars_shared, || Ok(home_routed.own())).unwrap();
            let by_bucket = ByBucket::new(home_routed.clone(), own, vars_shared);

            // Create a cell head for tenant 7 in bucket 1 so renaming tenant 7 is refused.
            let cell = CellId { tenant: 7, id: 1u64 << 48 | 1 };
            home_routed
                .own()
                .put(
                    &layout::head(cell),
                    bytes::Bytes::from_static(b"head"),
                    crate::bucket::PutMode::Create,
                )
                .await
                .unwrap();

            // Edit keys=FILE to rename tenant 7: reload is refused with InUse.
            std::fs::write(&keys_file, "7 key-new\n").unwrap();
            assert!(matches!(by_bucket.reload().await, Err(Unreloaded::InUse(_))));

            // Lazily reaching bucket 2 (its own credentials -> distinct KMS instance) MUST still
            // see "key-old", never reading the unreloaded edit from disk.
            let (_, b1_kms) = by_bucket.of(1).await.unwrap();
            let (_, b2_kms) = by_bucket.of(2).await.unwrap();
            assert!(!Arc::ptr_eq(&b1_kms, &b2_kms), "bucket 2 has its own KMS instance");
            assert_eq!(b1_kms.names_dyn().unwrap().of(7), "key-old");
            assert_eq!(b2_kms.names_dyn().unwrap().of(7), "key-old");

            // A valid edit (keeping tenant 7, adding tenant 8) applies on reload to BOTH buckets.
            std::fs::write(&keys_file, "7 key-old\n8 key-eight\n").unwrap();
            let reloaded = by_bucket.reload().await.unwrap();
            assert_eq!(reloaded.renamed, vec![8]);
            assert_eq!(b1_kms.names_dyn().unwrap().of(8), "key-eight");
            assert_eq!(b2_kms.names_dyn().unwrap().of(8), "key-eight");

            // Part B: `CLADEFS_MASTER_KEY_FILE` (and per-bucket shared master key files).
            let urls = [mk("b-home"), mk("b-two"), mk("b-three"), mk("b-four")];
            let [b_home, b_two, b_three, b_four] =
                urls.each_ref().map(|u| ObjectBucket::from_url(u).unwrap());
            identity::format(&b_home, &Details::default()).await.unwrap();
            for (idx, bucket) in [(1, &b_two), (2, &b_three), (3, &b_four)] {
                identity::add(
                    &b_home,
                    &Details::default(),
                    bucket,
                    Details { url: Some(urls[idx].clone()), ..Details::default() },
                )
                .await
                .unwrap();
            }

            let k1 = "11".repeat(32);
            let k2 = "22".repeat(32);
            let k3 = "33".repeat(32);
            let k4 = "44".repeat(32);
            let id1 = MasterKeys::parse(&k1, "t").unwrap().newest().id();
            let id2 = MasterKeys::parse(&k2, "t").unwrap().newest().id();
            let id3 = MasterKeys::parse(&k3, "t").unwrap().newest().id();
            let id4 = MasterKeys::parse(&k4, "t").unwrap().newest().id();

            let host_master = dir.path().join("host-master.key");
            let pair_master = dir.path().join("pair-master.key");
            std::fs::write(&host_master, format!("{k1}\n")).unwrap();
            std::fs::write(&pair_master, format!("{k3}\n")).unwrap();

            let vars = vec![
                (MASTER_KEY_FILE_VAR.to_owned(), host_master.display().to_string()),
                (
                    format!("CLADEFSD_BUCKET_3_{MASTER_KEY_FILE_VAR}"),
                    pair_master.display().to_string(),
                ),
                (
                    format!("CLADEFSD_BUCKET_4_{MASTER_KEY_FILE_VAR}"),
                    pair_master.display().to_string(),
                ),
            ];
            let b_home_routed = Arc::new(b_home.routed(Arc::new(Registered::new(Vec::new()))));
            let vars_shared = vars_of(&vars);
            let own = from_vars(&vars_shared, || Ok(b_home_routed.own())).unwrap();
            let by_bucket = ByBucket::new(b_home_routed, own, vars_shared);

            // Edit host_master without calling reload(); lazily reaching bucket 2 MUST still wrap
            // under id1, matching bucket 1.
            std::fs::write(&host_master, format!("{k2},{k1}\n")).unwrap();
            let (_, m2) = by_bucket.of(2).await.unwrap();
            m2.wrap_dyn(7, &Secret::random()).await.unwrap();
            let b2_kek = object(&b_two, 7).unwrap();
            assert_eq!(&b2_kek[22..30], &id1.0, "lazy bucket 2 read unreloaded master key file");

            // Open bucket 3 with pair_master (k3), then edit pair_master without reload; lazy
            // bucket 4 sharing pair_master MUST still see id3.
            let (_, m3) = by_bucket.of(3).await.unwrap();
            m3.wrap_dyn(7, &Secret::random()).await.unwrap();
            std::fs::write(&pair_master, format!("{k4},{k3}\n")).unwrap();
            let (_, m4) = by_bucket.of(4).await.unwrap();
            m4.wrap_dyn(7, &Secret::random()).await.unwrap();
            let b4_kek = object(&b_four, 7).unwrap();
            assert_eq!(&b4_kek[22..30], &id3.0, "lazy bucket 4 read unreloaded master key file");

            // After reload(), new KEKs in bucket 2 wrap under id2 and in bucket 4 under id4.
            let reloaded = by_bucket.reload().await.unwrap();
            assert_eq!(reloaded.master_keys.len(), 4);
            m2.wrap_dyn(8, &Secret::random()).await.unwrap();
            assert_eq!(&object(&b_two, 8).unwrap()[22..30], &id2.0);
            m4.wrap_dyn(8, &Secret::random()).await.unwrap();
            assert_eq!(&object(&b_four, 8).unwrap()[22..30], &id4.0);

            // Part C (hotbed review 117): a lazily-opened bucket sharing the host's master-key
            // file shares its sealing fence too, so removing an unused newest key cannot race with
            // the lazy bucket sealing a KEK under that removed key.
            let urls = [mk("c-home"), mk("c-two")];
            let [c_home, c_two] = urls.each_ref().map(|u| ObjectBucket::from_url(u).unwrap());
            identity::format(&c_home, &Details::default()).await.unwrap();
            identity::add(
                &c_home,
                &Details::default(),
                &c_two,
                Details { url: Some(urls[1].clone()), ..Details::default() },
            )
            .await
            .unwrap();
            let c_master = dir.path().join("c-master.key");
            std::fs::write(&c_master, format!("{k2},{k1}\n")).unwrap();
            let c_vars =
                vars_of(&[(MASTER_KEY_FILE_VAR.to_owned(), c_master.display().to_string())]);
            let c_home_routed = Arc::new(c_home.routed(Arc::new(Registered::new(Vec::new()))));
            let c_own = from_vars(&c_vars, || Ok(c_home_routed.own())).unwrap();
            let c_shared = c_own.bucket().unwrap().shared_keys();
            let c_by_bucket = Arc::new(ByBucket::new(c_home_routed, c_own, c_vars));

            // Simulate a reload removing unused newest key k2 having already acquired the shared
            // sealing write fence: a lazily-opened bucket 2 sharing c-master MUST share the same
            // sealing RwLock, so its wrap waits until the fence drops and then seals under k1.
            let fence = c_shared.sealing.clone().write_owned().await;
            let (_, c_m2) = c_by_bucket.of(2).await.unwrap();
            let secret = Secret::random();
            let secret_bytes = *secret.expose();
            let wrap_task = tokio::spawn({
                let c_m2 = c_m2.clone();
                async move { c_m2.wrap_dyn(9, &secret).await }
            });
            // Yield a few times: wrap_task MUST stay blocked behind the shared sealing fence.
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            assert!(
                !wrap_task.is_finished(),
                "lazy bucket 2 escaped shared sealing fence during master-key reload"
            );
            // Swap shared keys to k1-only (as reload does before dropping the fence) and release.
            *c_shared.keys.lock().unwrap() = Arc::new(MasterKeys::parse(&k1, "t").unwrap());
            drop(fence);
            let wrapped = wrap_task.await.unwrap().unwrap();
            assert_eq!(&object(&c_two, 9).unwrap()[22..30], &id1.0);
            c_m2.forget_dyn(9);
            assert_eq!(c_m2.unwrap_dyn(9, &wrapped).await.unwrap().expose(), &secret_bytes);
        });
    }
}
