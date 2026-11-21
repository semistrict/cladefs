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
use crate::keys::{KmsKey, Kmses};
use crate::layout;
use crate::object_bucket::{ObjectBucket, Registered, numbered_options};
use extent_keys::kms::bucket::{self as formats, KekObject, TenantKek};
use extent_keys::kms::master::{MASTER_KEY_FILE_VAR, MASTER_KEY_VAR, MasterKeys};
use extent_keys::kms::{self, Error, Kms};
use extent_keys::{Secret, Unauthentic, Wrapped};
use futures::StreamExt as _;
use futures::future::BoxFuture;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
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
    /// The identity of its bucket (TASK-633.6), which the keys it wraps name, once read: 0 for a
    /// bucket of none, its keys bound to no bucket.
    identity: Mutex<Option<u64>>,
}

/// A tenant's KEK as a host read it.
struct Held {
    kek: Arc<TenantKek>,
    read: Instant,
}

impl<B: Bucket> BucketKms<B> {
    /// The KMS keeping tenants' KEKs in `bucket`, under `keys`, the newest first.
    pub fn new(bucket: Arc<B>, keys: MasterKeys) -> Self {
        Self {
            bucket,
            keys,
            held: Mutex::default(),
            fresh_for: FRESH_FOR,
            identity: Mutex::default(),
        }
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
        KekObject::decode(tenant, bytes)?.open(&self.keys).map(Arc::new)
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
                Created::Created(_) => return Destroyed::Now,
                Created::Exists(_) => continue,
            }
        };
        // A KEK object, or bytes that are none: a tombstone in their place all the same.
        if let Ok(KekObject::Tombstone { .. }) = KekObject::decode(tenant, &object.bytes) {
            return Destroyed::Already;
        }
        let tombstone = KekObject::tombstone(tenant);
        match bucket::update(bucket, &key, tombstone, &object.meta.version).await {
            Updated::Updated(_) => return Destroyed::Now,
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
                        Updated::Updated(_) => rotated.rewrapped += 1,
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
/// (cladefsd, cladefsctl, the cladefs CLI's mounts through `cladefs::bucket::kms`; `cladefs-gc`):
/// the KMS [`KMS_VAR`] names, if it names one; else, given master keys (`CLADEFS_MASTER_KEY` or
/// `CLADEFS_MASTER_KEY_FILE`), the bucket KMS over `bucket()`, the host's bucket; else refused
/// ([`no_kms`]), as every cell is encrypted (TASK-411.28). A KMS named, or master keys given, that
/// can't be had is refused, saying why, never what a key holds.
pub fn from_env<B: Bucket>(
    bucket: impl FnOnce() -> Result<Arc<B>, String>,
) -> Result<HostKms<B>, String> {
    choose(&|name| std::env::var_os(name), None, bucket)
}

/// cladefs: the KMS the variables `var` gives by name choose, as [`from_env`] does, unless
/// `recorded`, a registry's, names one (TASK-633.6): that one then.
fn choose<B: Bucket>(
    var: &dyn Fn(&str) -> Option<OsString>,
    recorded: Option<&str>,
    bucket: impl FnOnce() -> Result<Arc<B>, String>,
) -> Result<HostKms<B>, String> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    let given = [MASTER_KEY_VAR, MASTER_KEY_FILE_VAR]
        .into_iter()
        .filter(|name| set(name).is_some())
        .collect::<Vec<_>>();
    let named = match recorded {
        Some(url) => Some((url.to_owned(), "the registry of buckets".to_owned())),
        None => set(KMS_VAR).map(|url| (url.to_string_lossy().into_owned(), KMS_VAR.to_owned())),
    };
    if let Some((url, source)) = named {
        if let Some(name) = given.first() {
            log::warn!("cladefs: {name} is set, and unused: {source} names the KMS");
        }
        let kms = kms::from_url(&url).map_err(|why| format!("{source}: {why}"))?;
        return Ok(HostKms::Named(url, kms));
    }
    let keys = MasterKeys::from_vars(var)?.ok_or_else(no_kms)?;
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
///   `CLADEFSD_BUCKET_<N>_CLADEFS_KMS` overrides it: `object_bucket::numbered_options`); else,
///   given master keys there (`CLADEFS_MASTER_KEY`, or `CLADEFSD_BUCKET_<N>_CLADEFS_MASTER_KEY`),
///   the bucket KMS over that bucket itself, its tenants' KEKs kept in it, refused unless the
///   master key its entry records (`master_key_id`), if any, is among them; else none, refusing
///   every call for its cells, never as a revocation.
///
/// A KMS is named by its URL, one for every bucket naming it, so that its refusal of a tenant
/// refuses the tenant in all of them and no other ([`keys::CachingKms`]); a bucket KMS by its
/// bucket.
pub struct ByBucket {
    home: Arc<ObjectBucket>,
    own: HostKms<ObjectBucket>,
    registered: Registered,
    vars: Vec<(String, String)>,
    /// The home bucket's number, once read: none if it has no identity.
    own_number: Mutex<Option<Option<u16>>>,
    chosen: Mutex<HashMap<u16, (KmsKey, kms::Shared)>>,
    named: Mutex<HashMap<String, kms::Shared>>,
    buckets: Mutex<BTreeMap<u16, Arc<BucketKms<ObjectBucket>>>>,
}

impl ByBucket {
    /// The KMSes of the buckets `home`, the host's routed bucket, reaches, its own `own`, the
    /// others' chosen from the registry and `vars`, the host's environment.
    pub fn new(
        home: Arc<ObjectBucket>,
        own: HostKms<ObjectBucket>,
        vars: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let vars: Vec<(String, String)> = vars.into_iter().collect();
        let named = match &own {
            HostKms::Named(url, kms) => HashMap::from([(url.clone(), kms.clone())]),
            HostKms::Bucket(_) => HashMap::new(),
        };
        Self {
            home,
            own,
            registered: Registered::new(vars.clone()),
            vars,
            own_number: Mutex::default(),
            chosen: Mutex::default(),
            named: Mutex::new(named),
            buckets: Mutex::default(),
        }
    }

    /// The bucket KMSes chosen so far, by their buckets' numbers: the host's own bucket's under its
    /// number, or 0 if it has none.
    pub fn bucket_kmses(&self) -> Vec<(u16, Arc<BucketKms<ObjectBucket>>)> {
        let mut all: Vec<_> =
            self.buckets.lock().unwrap().iter().map(|(n, kms)| (*n, kms.clone())).collect();
        if let HostKms::Bucket(kms) = &self.own {
            let own = self.own_number.lock().unwrap().flatten().unwrap_or(NAMED);
            all.push((own, kms.clone()));
        }
        all
    }

    /// Shreds `tenant` in every bucket it reaches (doc-16, Shredding; TASK-633.6): its KEK object
    /// replaced by a tombstone in each bucket under a bucket KMS ([`BucketKms::destroy`]), the
    /// host's own first, then each other bucket the registry records, its copy in the host's
    /// bucket and the root bucket's, the authority. A bucket under a KMS named by its URL keeps
    /// the tenant's KEK there, out of reach: destroyed there, out of band (doc-16). Says which.
    pub async fn shred(&self, tenant: u64) -> Shredded {
        let mut shredded = Shredded::default();
        let own = match self.own_number().await {
            Ok(own) => own,
            Err(why) => {
                shredded.failed.push((NAMED, format!("the host's bucket's identity: {why}")));
                None
            }
        };
        match &self.own {
            HostKms::Bucket(kms) => {
                shredded.destroyed.push((own.unwrap_or(NAMED), kms.destroy(tenant).await))
            }
            HostKms::Named(url, _) => shredded.named.push((own.unwrap_or(NAMED), url.clone())),
        }
        let mut numbers = BTreeSet::new();
        let mut registries = vec![identity::registry(&*self.home).await];
        if own.is_some_and(|own| own != identity::ROOT) {
            match self.home.numbered(identity::ROOT).await {
                Ok(root) => registries.push(identity::registry(&*root).await),
                Err(err) => shredded.failed.push((identity::ROOT, err.to_string())),
            }
        }
        for registry in registries {
            match registry {
                Ok(registry) => numbers.extend(
                    (registry.iter())
                        .flat_map(|registry| registry.buckets.iter().map(|e| e.number)),
                ),
                Err(err) => shredded.failed.push((identity::ROOT, format!("the registry: {err}"))),
            }
        }
        for number in numbers.into_iter().filter(|&number| Some(number) != own) {
            match self.of(number).await {
                Err(why) => shredded.failed.push((number, why)),
                Ok((key, _)) => {
                    let kms = self.buckets.lock().unwrap().get(&number).cloned();
                    match kms {
                        Some(kms) => shredded.destroyed.push((number, kms.destroy(tenant).await)),
                        None => shredded.named.push((number, key.0.to_string())),
                    }
                }
            }
        }
        shredded
    }

    /// The home bucket's number, read once: none if it has no identity.
    async fn own_number(&self) -> Result<Option<u16>, String> {
        if let Some(own) = *self.own_number.lock().unwrap() {
            return Ok(own);
        }
        let own = identity::get(&*self.home).await.map_err(|err| err.to_string())?;
        let own = own.map(|identity| identity.number);
        *self.own_number.lock().unwrap() = Some(own);
        Ok(own)
    }

    /// The host's own KMS, and its name.
    fn own(&self) -> (KmsKey, kms::Shared) {
        match &self.own {
            HostKms::Named(url, kms) => (KmsKey(url.as_str().into()), kms.clone()),
            HostKms::Bucket(kms) => (KmsKey("the host's bucket KMS".into()), kms.clone()),
        }
    }

    /// The KMS of the bucket numbered `number`, another than the host's, chosen (see the type).
    /// Its bucket KMS too, if it is that.
    async fn other(&self, number: u16) -> Result<Chosen, String> {
        let entry = self.registered.entry(&self.home, number).await;
        let entry = entry.map_err(|err| format!("its entry in the registry: {err}"))?;
        let vars = kms_vars(&self.vars, number);
        let var = |name: &str| {
            vars.iter().rev().find(|(of, _)| of == name).map(|(_, value)| OsString::from(value))
        };
        let member = self.home.numbered(number).await.map_err(|err| format!("the bucket: {err}"));
        match choose(&var, entry.details.kms.as_deref(), || member)? {
            HostKms::Named(url, kms) => {
                let kms = self.named.lock().unwrap().entry(url.clone()).or_insert(kms).clone();
                Ok(((KmsKey(url.into()), kms), None))
            }
            HostKms::Bucket(kms) => {
                if let Some(id) = &entry.details.master_key_id
                    && !kms.keys().ids().iter().any(|held| held.to_string() == *id)
                {
                    return Err(format!(
                        "its tenants' KEKs are kept under master key {id}, which this host has \
                         not (it has {}): CLADEFSD_BUCKET_{number}_{MASTER_KEY_VAR}",
                        kms.keys().named_ids()
                    ));
                }
                kms.identify(entry.id);
                let key = KmsKey(format!("the bucket KMS of bucket {number}").into());
                Ok(((key, kms.clone()), Some(kms)))
            }
        }
    }
}

/// What [`ByBucket::other`] chose: the KMS and its name, and the bucket KMS it is, if it is one.
type Chosen = ((KmsKey, kms::Shared), Option<Arc<BucketKms<ObjectBucket>>>);

/// cladefs: the variables bucket `number`'s KMS is chosen by, of `vars`: its own
/// (`CLADEFSD_BUCKET_<N>_...`) over the host's, as for its options
/// (`object_bucket::numbered_options`); the KMS's ([`KMS_VAR`], [`MASTER_KEY_VAR`],
/// [`MASTER_KEY_FILE_VAR`]) as one, so that a bucket given any of them as its own takes none of the
/// host's: its own master key and the host's master key file are not both given, nor is the
/// host's KMS chosen over its own master key.
fn kms_vars(vars: &[(String, String)], number: u16) -> Vec<(String, String)> {
    let prefix = format!("CLADEFSD_BUCKET_{number}_");
    let kms = [KMS_VAR, MASTER_KEY_VAR, MASTER_KEY_FILE_VAR];
    let own = (vars.iter())
        .any(|(name, _)| name.strip_prefix(&prefix).is_some_and(|name| kms.contains(&name)));
    let shared = vars.iter().filter(|(name, _)| !(own && kms.contains(&name.as_str())));
    numbered_options(shared.cloned(), number)
}

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

impl Kmses for ByBucket {
    fn of(&self, bucket: u16) -> BoxFuture<'_, Result<(KmsKey, kms::Shared), String>> {
        Box::pin(async move {
            if bucket == NAMED || self.own_number().await? == Some(bucket) {
                return Ok(self.own());
            }
            if let Some(chosen) = self.chosen.lock().unwrap().get(&bucket) {
                return Ok(chosen.clone());
            }
            let (chosen, bucket_kms) = self.other(bucket).await?;
            // Chosen at once by another call, perhaps: that one, the one every caller holds.
            let mut all = self.chosen.lock().unwrap();
            if let Some(chosen) = all.get(&bucket) {
                return Ok(chosen.clone());
            }
            if let Some(bucket_kms) = bucket_kms {
                self.buckets.lock().unwrap().insert(bucket, bucket_kms);
            }
            Ok(all.entry(bucket).or_insert(chosen).clone())
        })
    }
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
        // Its bucket's identity read once, which what it wraps names (TASK-633.6).
        let identity = layout::IDENTITY;
        assert_eq!(asked, [format!("get {identity}"), format!("get {kek}"), format!("put {kek}")]);
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
            let wrapped_again = formats::wrap(tenant, 0, &kek, &key);
            assert_eq!(wrapped_again, wrapped[tenant as usize - 1]);
        }
    }

    /// TASK-633.6 (review): a bucket given a KMS variable of its own takes none of the host's: its
    /// master key over the host's master key file, not both; its master key over the host's KMS.
    #[test]
    fn a_buckets_own_kms_variables_override_the_hosts_as_one() {
        let vars = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
        };
        let host = vars(&[
            (MASTER_KEY_FILE_VAR, "/etc/cladefs/master"),
            (KMS_VAR, "local:///kms"),
            ("AWS_REGION", "us-east-1"),
            ("CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY", "22"),
        ]);
        let mut two = kms_vars(&host, 2);
        two.sort();
        assert_eq!(two, vars(&[("AWS_REGION", "us-east-1"), (MASTER_KEY_VAR, "22")]));
        let mut three = kms_vars(&host, 3);
        three.sort();
        let expected = vars(&[
            ("AWS_REGION", "us-east-1"),
            ("CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY", "22"),
            (KMS_VAR, "local:///kms"),
            (MASTER_KEY_FILE_VAR, "/etc/cladefs/master"),
        ]);
        assert_eq!(three, expected);
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
        futures::executor::block_on(async {
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
            identity::format(&home, &Details::default(), crate::identity::Asked::default())
                .await
                .unwrap();
            let recorded = |url: &str, kms: Option<&str>, key: Option<String>| Details {
                url: Some(url.to_owned()),
                kms: kms.map(str::to_owned),
                master_key_id: key,
                ..Details::default()
            };
            let added = recorded(&urls[1], None, Some(id(&theirs)));
            identity::add(
                &home,
                &Details::default(),
                &other,
                added,
                crate::identity::Asked::default(),
            )
            .await
            .unwrap();
            let added = recorded(&urls[2], Some(&local), None);
            identity::add(
                &home,
                &Details::default(),
                &third,
                added,
                crate::identity::Asked::default(),
            )
            .await
            .unwrap();
            let by_bucket = |key: &str| {
                let vars =
                    vec![("CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY".to_owned(), key.to_owned())];
                let home = ObjectBucket::from_url(&urls[0]).unwrap();
                let home = Arc::new(home.routed(Arc::new(Registered::new(vars.clone()))));
                let own = HostKms::Bucket(Arc::new(BucketKms::new(home.clone(), parsed(&ours))));
                Arc::new(ByBucket::new(home, own, vars))
            };
            let host = |key: &str| CachingKms::over(by_bucket(key), None);
            let cells = [1u64, 2, 3].map(|number| CellId { tenant: 7, id: number << 48 | 1 });
            let kms = host(&theirs);
            for cell in cells {
                kms.new_kek(cell).await.unwrap();
            }
            async fn kek(bucket: &ObjectBucket) -> Option<Vec<u8>> {
                bucket::get(bucket, &layout::kek(7)).await.map(|object| object.bytes)
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

            assert_eq!(destroy(&other, 7).await, Destroyed::Now);
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
            Ok(HostKms::Named(..)) => "named".to_owned(),
            Ok(HostKms::Bucket(_)) => "bucket".to_owned(),
            Err(why) => why.clone(),
        };
        assert_eq!(
            kind(&chosen(Some(&bucket))),
            "no KMS: CLADEFS_KMS names none, and neither CLADEFS_MASTER_KEY nor CLADEFS_MASTER_KEY_FILE \
             gives a master key; every cell is encrypted (doc-16): set CLADEFS_KMS to a KMS's URL, or \
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
            "CLADEFS_KMS: \"vault://key\" names no KMS: local:///DIR, openbao://, aws-kms://, \
             gcp-kms:// or azure-kv:// (doc-16)"
        );
    }
}
