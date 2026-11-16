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
//! - **Which KMS a host uses** ([`from_env`]): `CLADEFS_KMS`'s if it names one; else, given a master
//!   key, this one, over the host's bucket; else none, which every process opening stores refuses
//!   to start with, naming both (TASK-411.28: every store is encrypted).

use crate::bucket::{self, Bucket, Created, Updated};
use crate::layout;
use extent_keys::kms::bucket::{self as formats, KekObject, TenantKek};
use extent_keys::kms::master::{MASTER_KEY_FILE_VAR, MASTER_KEY_VAR, MasterKeys};
use extent_keys::kms::{self, Error, Kms};
use extent_keys::{Secret, Unauthentic, Wrapped};
use futures::StreamExt as _;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The variable naming a host's KMS by its URL (`extent_keys::kms::from_url`), which [`from_env`]
/// takes over a master key.
pub const KMS_VAR: &str = "CLADEFS_KMS";

/// How long a host uses a tenant's KEK as it last read it before reading its object again: so
/// that a tenant shredded elsewhere is refused here within it, told or not, and the bucket is
/// asked once a tenant that long at most however many cells' KEKs the host wraps and unwraps.
pub const FRESH_FOR: Duration = Duration::from_secs(60);

/// A KMS over a bucket: tenants' KEKs kept in it, each wrapped by a master key (see the module).
pub struct BucketKms<B> {
    bucket: Arc<B>,
    keys: MasterKeys,
    /// Each tenant's KEK as last read, and when.
    held: Mutex<HashMap<u64, Held>>,
    fresh_for: Duration,
}

/// A tenant's KEK as a host read it.
struct Held {
    kek: Arc<TenantKek>,
    read: Instant,
}

impl<B: Bucket> BucketKms<B> {
    /// The KMS keeping tenants' KEKs in `bucket`, under `keys`, the newest first.
    pub fn new(bucket: Arc<B>, keys: MasterKeys) -> Self {
        Self { bucket, keys, held: Mutex::default(), fresh_for: FRESH_FOR }
    }

    /// The same, reading a tenant's KEK object again once it is `fresh_for` old: tests'.
    pub fn fresh_for(self, fresh_for: Duration) -> Self {
        Self { fresh_for, ..self }
    }

    /// The bucket it keeps tenants' KEKs in.
    pub fn bucket(&self) -> &Arc<B> {
        &self.bucket
    }

    /// Its master keys.
    pub fn keys(&self) -> &MasterKeys {
        &self.keys
    }

    /// Shreds `tenant` ([`destroy`]), and forgets its KEK here.
    pub async fn destroy(&self, tenant: u64) -> Destroyed {
        let destroyed = destroy(&*self.bucket, tenant).await;
        self.forget(tenant);
        destroyed
    }

    /// `tenant`'s KEK: as held, if fresh; else read from its object, or, if it has none and
    /// `make`, made. Refused if the object is a tombstone, or missing and not to be made: the
    /// tenant is destroyed.
    async fn kek(&self, tenant: u64, make: bool) -> Result<Arc<TenantKek>, Error> {
        let held = self
            .held
            .lock()
            .unwrap()
            .get(&tenant)
            .and_then(|held| (held.read.elapsed() < self.fresh_for).then(|| held.kek.clone()));
        if let Some(kek) = held {
            return Ok(kek);
        }
        let read = Instant::now();
        let kek = self.read(tenant, make).await;
        let mut held = self.held.lock().unwrap();
        match &kek {
            Ok(kek) => held.insert(tenant, Held { kek: kek.clone(), read }),
            Err(_) => held.remove(&tenant),
        };
        kek
    }

    /// `tenant`'s KEK, read from its object; or, none there and `make`, made, create-only. A create
    /// finding another host's object there takes that.
    async fn read(&self, tenant: u64, make: bool) -> Result<Arc<TenantKek>, Error> {
        let key = layout::kek(tenant);
        if let Some(object) = bucket::get(&*self.bucket, &key).await {
            return self.opened(tenant, &object.bytes);
        }
        // A key wrapped under a KEK names one that was there: deleted since, by someone other than
        // `destroy`.
        if !make {
            return Err(Error::Destroyed { tenant });
        }
        let kek = TenantKek::random();
        let newest = self.keys.newest();
        match bucket::create(&*self.bucket, &key, KekObject::seal(newest, tenant, &kek)).await {
            Created::Created => {
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
        KekObject::decode(tenant, bytes)?.open(&self.keys).map(Arc::new)
    }
}

impl<B: Bucket> Kms for BucketKms<B> {
    async fn wrap(&self, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        let kek = self.kek(tenant, true).await?;
        Ok(formats::wrap(tenant, &kek, key))
    }

    async fn unwrap(&self, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        let unauthentic = |source| Error::Unauthentic { tenant, source };
        let (of, id) = formats::kek_of(wrapped).map_err(unauthentic)?;
        if of != tenant {
            return Err(unauthentic(Unauthentic("a wrapped key, wrapped for another tenant")));
        }
        let mut kek = self.kek(tenant, false).await?;
        if kek.id != id {
            // Held from before its object changed, perhaps: read it again.
            self.forget(tenant);
            kek = self.kek(tenant, false).await?;
        }
        formats::unwrap(tenant, &kek, wrapped).map_err(unauthentic)
    }

    fn forget(&self, tenant: u64) {
        self.held.lock().unwrap().remove(&tenant);
    }
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
/// non-current versions of `tenants/<tenant>/kek`.
pub async fn destroy(bucket: &impl Bucket, tenant: u64) -> Destroyed {
    let key = layout::kek(tenant);
    loop {
        let Some(object) = bucket::get(bucket, &key).await else {
            match bucket::create(bucket, &key, KekObject::tombstone(tenant)).await {
                Created::Created => return Destroyed::Now,
                Created::Exists(_) => continue,
            }
        };
        // A KEK object, or bytes that are none: a tombstone in their place all the same.
        if let Ok(KekObject::Tombstone { .. }) = KekObject::decode(tenant, &object.bytes) {
            return Destroyed::Already;
        }
        let tombstone = KekObject::tombstone(tenant);
        match bucket::update(bucket, &key, tombstone, &object.meta.version).await {
            Updated::Updated => return Destroyed::Now,
            Updated::Changed => {}
        }
    }
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
/// hosts need the newest key alone: the others may be dropped from `CLADEFS_MASTER_KEY`. Tenants
/// are listed a page at a time; done again, it rewraps nothing more.
pub async fn rotate<B: Bucket>(bucket: &B, keys: &MasterKeys) -> Rotated {
    let mut rotated = Rotated::default();
    let mut tenants = std::pin::pin!(bucket::prefixes(bucket, layout::TENANTS.to_owned()));
    while let Some(prefix) = tenants.next().await {
        let Some(tenant) = layout::tenant_of_prefix(&prefix) else { continue };
        let key = layout::kek(tenant);
        loop {
            let Some(object) = bucket::get(bucket, &key).await else { break };
            let resealed = KekObject::decode(tenant, &object.bytes)
                .and_then(|kek| Ok((kek.resealed(keys)?, kek)));
            match resealed {
                Err(error) => rotated.refused.push(error.to_string()),
                Ok((None, KekObject::Tombstone { .. })) => rotated.destroyed += 1,
                Ok((None, KekObject::Kek { .. })) => rotated.newest += 1,
                Ok((Some(bytes), _)) => {
                    match bucket::update(bucket, &key, bytes, &object.meta.version).await {
                        Updated::Updated => rotated.rewrapped += 1,
                        Updated::Changed => continue,
                    }
                }
            }
            break;
        }
    }
    rotated
}

/// A host's KMS, as [`from_env`] chose it.
pub enum HostKms<B> {
    /// The one `CLADEFS_KMS` names.
    Named(kms::Shared),
    /// The bucket KMS, over the host's bucket.
    Bucket(Arc<BucketKms<B>>),
}

impl<B: Bucket> HostKms<B> {
    /// It, as a host holds any KMS.
    pub fn shared(&self) -> kms::Shared {
        match self {
            Self::Named(kms) => kms.clone(),
            Self::Bucket(kms) => kms.clone(),
        }
    }

    /// The bucket KMS, if it is that.
    pub fn bucket(&self) -> Option<&Arc<BucketKms<B>>> {
        match self {
            Self::Named(_) => None,
            Self::Bucket(kms) => Some(kms),
        }
    }
}

/// Why a host has no KMS ([`from_env`]): neither variable gives one. Every store is encrypted
/// (TASK-411.28), so a process that opens stores refuses to start so, saying this.
pub fn no_kms() -> String {
    format!(
        "no KMS: {KMS_VAR} names none, and neither {MASTER_KEY_VAR} nor {MASTER_KEY_FILE_VAR} \
         gives a master key; every store is encrypted (doc-16): set {KMS_VAR} to a KMS's URL, or \
         {MASTER_KEY_VAR} to a master key (openssl rand -hex 32) to keep tenants' KEKs in the \
         bucket"
    )
}

/// The KMS a host's environment chooses, the one place every process that opens stores asks
/// (extentd, cladefsctl, the cladefs CLI's mounts through `cladefs::bucket::kms`; `cladefs-gc`): the KMS
/// [`KMS_VAR`] names, if it names one; else, given master keys (`CLADEFS_MASTER_KEY` or
/// `CLADEFS_MASTER_KEY_FILE`), the bucket KMS over `bucket()`, the host's bucket; else refused
/// ([`no_kms`]), as every store is encrypted (TASK-411.28). A KMS named, or master keys given, that
/// can't be had is refused, saying why, never what a key holds.
pub fn from_env<B: Bucket>(
    bucket: impl FnOnce() -> Result<Arc<B>, String>,
) -> Result<HostKms<B>, String> {
    let named = std::env::var(KMS_VAR).ok().filter(|url| !url.is_empty());
    let given = [MASTER_KEY_VAR, MASTER_KEY_FILE_VAR]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        .collect::<Vec<_>>();
    if let Some(url) = named {
        if let Some(name) = given.first() {
            log::warn!("cladefs: {name} is set, and unused: {KMS_VAR} names the host's KMS");
        }
        let kms = kms::from_url(&url).map_err(|why| format!("{KMS_VAR}: {why}"))?;
        return Ok(HostKms::Named(kms));
    }
    let keys = MasterKeys::from_env()?.ok_or_else(no_kms)?;
    let bucket = bucket().map_err(|why| {
        format!("{MASTER_KEY_VAR}: the bucket KMS keeps tenants' KEKs in the host's bucket: {why}")
    })?;
    Ok(HostKms::Bucket(Arc::new(BucketKms::new(bucket, keys))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;

    const KEK_OBJECT_LEN: usize = formats::KEK_OBJECT_LEN;

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
        block_on(bucket::get(bucket, &layout::kek(tenant))).map(|object| object.bytes)
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
        assert_eq!(asked, [format!("get {kek}"), format!("put {kek}")]);
        assert_eq!(wrapped.0.len(), formats::WRAPPED_LEN);
        let made = object(&Direct(sim.clone()), 7).unwrap();
        assert_eq!(made.len(), KEK_OBJECT_LEN);
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
            (key.expose(), vec![format!("get {kek}")])
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

    /// An unwrap makes no KEK: a key naming a tenant's KEK in a bucket that has none is of a KEK
    /// gone, refused as destroyed, nothing made.
    #[test]
    fn unwraps_of_tenants_with_no_kek_object_are_destroyed_and_make_none() {
        let master = Secret::random();
        let (ours, elsewhere) = (bucket(), bucket());
        let wrapped = wrap(&host(&elsewhere, &[&master]), 9, &Secret::random()).unwrap();
        let kms = host(&ours, &[&master]);
        assert_eq!(refusal(unwrap(&kms, 9, &wrapped)), "tenant 9: its KEK is destroyed");
        assert_eq!(keys(&*ours), Vec::<String>::new());
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
        assert_eq!(
            rounds,
            [
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

    /// The same race, many hosts on threads at once over a bucket in memory: one KEK object, every
    /// wrap unwrapped by every host.
    #[test]
    fn many_hosts_racing_on_threads_make_one_kek() {
        let master = Secret::random();
        let bucket = Arc::new(crate::object_bucket::ObjectBucket::new(
            Arc::new(object_store::memory::InMemory::new()),
            "",
        ));
        let hosts: Vec<_> = (0..8).map(|_| Arc::new(host(&bucket, &[&master]))).collect();
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let wraps: Vec<_> = (hosts.iter().cloned())
            .map(|kms| {
                runtime.spawn(async move {
                    let key = Secret::random();
                    let wrapped = kms.wrap(7, &key).await.unwrap();
                    (Secret::from_bytes(key.expose()), wrapped)
                })
            })
            .collect();
        let wrapped: Vec<_> =
            wraps.into_iter().map(|wrap| runtime.block_on(wrap).unwrap()).collect();
        assert_eq!(runtime.block_on(bucket.list("")).unwrap().len(), 1);
        for kms in &hosts {
            for (key, wrapped) in &wrapped {
                let unwrapped = runtime.block_on(kms.unwrap(7, wrapped)).unwrap();
                assert_eq!(unwrapped.expose(), key.expose());
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
        block_on(destroy(&*bucket, 9));
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

        let rotated = block_on(rotate(&*bucket, &keys_of(&[&new, &old])));
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
        let again = block_on(rotate(&*bucket, &keys_of(&[&new, &old])));
        assert_eq!(again, Rotated { rewrapped: 0, newest: 2, destroyed: 1, refused: vec![] });

        // A rotation given keys none of which opens an object leaves it, naming it.
        let lost = block_on(rotate(&*bucket, &keys_of(&[&Secret::random()])));
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

        assert_eq!(block_on(shredder.destroy(7)), Destroyed::Now);
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
        assert_eq!(block_on(destroy(&*bucket, 7)), Destroyed::Already);

        assert_eq!(block_on(destroy(&*bucket, 9)), Destroyed::Now);
        assert_eq!(refusal(wrap(&shredder, 9, &key)), "tenant 9: its KEK is destroyed");
        assert_eq!(keys(&*bucket), [layout::kek(7), layout::kek(9)]);
        assert_eq!(object(&*bucket, 9), Some(KekObject::tombstone(9)));
    }

    /// AC #3 (TASK-411.27), as TASK-411.28 left it: a KEK object deleted outright, not shredded,
    /// loses what its KEK wrapped: keys under it are refused, as destroyed until a wrap makes the
    /// tenant a new KEK, then as another KEK's. Its new cells are encrypted under the new one: no cell
    /// is ever in the clear, so no marker refuses the tenant (TASK-411.19's went).
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
        let rotated = block_on(rotate(&*bucket, &keys_of(&[&new, &old])));
        assert_eq!(rotated.rewrapped, 3);
        refusals.extend(block_on(rotate(&*bucket, &keys_of(&[&wrong]))).refused);
        block_on(both.destroy(3));
        refusals.push(refusal(unwrap(&both, 3, &wrapped[2])));
        refusals.push(format!("{:?}", keys_of(&[&new, &old])));
        assert_eq!(refusals.len(), 8, "{refusals:?}");
        log::info!("the test's own line, so that some are kept");

        let objects: Vec<Vec<u8>> = keys(&*bucket)
            .iter()
            .map(|key| block_on(bucket::get(&*bucket, key)).unwrap().bytes)
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
            let kek = KekObject::decode(tenant, bytes).unwrap().open(both.keys()).unwrap();
            let wrapped_again = formats::wrap(tenant, &kek, &key);
            assert_eq!(wrapped_again, wrapped[tenant as usize - 1]);
        }
    }

    /// The KMS a host's environment chooses: `CLADEFS_KMS`'s if it names one, a master key or not;
    /// else the bucket KMS over the host's bucket, given a master key; else none, refused naming
    /// both variables (TASK-411.28). A master key with no bucket, or two ways of giving it, is
    /// refused naming why. Each nextest test is a process of its own: this one's variables are its
    /// own.
    #[test]
    fn hosts_choose_cladefs_kms_then_the_bucket_kms_and_refuse_none() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: this test's process is its own (nextest), no other thread reading it.
        let set = |name: &str, value: Option<&str>| unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        };
        for name in [KMS_VAR, MASTER_KEY_VAR, MASTER_KEY_FILE_VAR] {
            set(name, None);
        }
        let bucket = bucket();
        let chosen = |bucket: Option<&Arc<Direct>>| {
            from_env(|| bucket.cloned().ok_or_else(|| "CLADEFSD_BUCKET names none".to_owned()))
        };
        let kind = |chosen: &Result<HostKms<Direct>, String>| match chosen {
            Ok(HostKms::Named(_)) => "named".to_owned(),
            Ok(HostKms::Bucket(_)) => "bucket".to_owned(),
            Err(why) => why.clone(),
        };
        assert_eq!(
            kind(&chosen(Some(&bucket))),
            "no KMS: CLADEFS_KMS names none, and neither CLADEFS_MASTER_KEY nor CLADEFS_MASTER_KEY_FILE \
             gives a master key; every store is encrypted (doc-16): set CLADEFS_KMS to a KMS's URL, or \
             CLADEFS_MASTER_KEY to a master key (openssl rand -hex 32) to keep tenants' KEKs in the \
             bucket"
        );
        assert_eq!(kind(&chosen(Some(&bucket))), no_kms());
        set(KMS_VAR, Some(""));
        assert_eq!(kind(&chosen(Some(&bucket))), no_kms(), "an empty CLADEFS_KMS names none");
        set(KMS_VAR, None);
        let hex: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
        set(MASTER_KEY_VAR, Some(&hex));
        let host = chosen(Some(&bucket)).unwrap();
        let kms = host.bucket().unwrap();
        assert!(Arc::ptr_eq(kms.bucket(), &bucket));
        assert_eq!(kms.keys().named_ids(), "8b1917a7f2bf1306");
        let key = Secret::random();
        let wrapped = block_on(host.shared().wrap_dyn(7, &key)).unwrap();
        assert_eq!(unwrap(&**kms, 7, &wrapped).unwrap().expose(), key.expose());
        assert_eq!(keys(&*bucket), [layout::kek(7)]);
        assert_eq!(
            kind(&chosen(None)),
            "CLADEFS_MASTER_KEY: the bucket KMS keeps tenants' KEKs in the host's bucket: \
             CLADEFSD_BUCKET names none"
        );
        set(MASTER_KEY_FILE_VAR, Some("/nonexistent"));
        assert_eq!(
            kind(&chosen(Some(&bucket))),
            "both CLADEFS_MASTER_KEY and CLADEFS_MASTER_KEY_FILE are set: give the master keys one way"
        );
        set(MASTER_KEY_FILE_VAR, None);
        set(KMS_VAR, Some(&format!("local://{}", dir.path().display())));
        assert_eq!(kind(&chosen(None)), "named");
        set(KMS_VAR, Some("vault://key"));
        assert_eq!(
            kind(&chosen(None)),
            "CLADEFS_KMS: \"vault://key\" names no KMS: local:///DIR, openbao://, aws-kms:// or \
             gcp-kms:// (doc-16)"
        );
    }
}
