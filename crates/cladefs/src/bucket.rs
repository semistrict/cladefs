// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what a host keeps its stores' cells in object storage with (backlog doc-7;
//! TASK-28.5): its configuration ([`Host`], [`host_token`]), the runtime its stores' uploaders and
//! fetches run on and their budgets ([`runtime`], [`HostFetcher`]), and the errno of each failure
//! ([`errno`]). The uploader is local_cell's (`local_cell::upload`, TASK-165.9).
//!
//! cladefs: and its KMS ([`kms`], TASK-411.3), which every process of the host's opening stores
//! chooses alike, the extent daemon, the cladefs CLI's mounts and `cladefsctl`
//! (`bucket_cell::bucket_kms::from_env`): the one `CLADEFS_KMS` names (doc-16: `local:///DIR`,
//! OpenBao, AWS KMS, Google Cloud KMS); else, given a master key (`CLADEFS_MASTER_KEY` or
//! `CLADEFS_MASTER_KEY_FILE`), the bucket KMS, tenants' KEKs kept in the host's bucket
//! (`CLADEFSD_BUCKET`), made on each tenant's first use (TASK-411.27); else none, which every
//! such process refuses to start with ([`require_kms`], TASK-411.28): every cell is encrypted.

use crate::Errno;
use bucket_cell::bucket_kms::BucketKms;
use bucket_cell::cache::Cache;
use bucket_cell::fetch::{self, Budget, Limits, Priority};
use bucket_cell::keys;
use bucket_cell::layout::Location;
use bucket_cell::object_bucket::ObjectBucket;
use local_cell::upload::{Failure, Runtime, UPLOAD_BYTES};
use std::sync::Arc;
use storage_device::ExtentId;

/// The fencing token of this host's placement, `CLADEFSD_TOKEN`, which the system's control
/// plane gives it, and which the copies of stores here that were never placed ([`place`]) take
/// their cells with, as stores restored or taken here are. Not 0, which would fence nothing:
/// a host without one keeps no cell (TASK-45).
pub fn host_token() -> Result<u64, Failure> {
    let missing = || {
        Failure::msg(
            "CLADEFSD_TOKEN names no placement token: this host's placement's, from the \
             system's control plane, which fences earlier placements",
        )
    };
    match std::env::var("CLADEFSD_TOKEN") {
        Ok(token) if !token.is_empty() => match token.parse() {
            Ok(0) => Err(missing()),
            Ok(token) => Ok(token),
            Err(err) => Err(Failure::local(err, format!("CLADEFSD_TOKEN {token:?}"))),
        },
        _ => Err(missing()),
    }
}

/// This host, as one of cladefs's multi-node system, the only kind there is (TASK-45): the bucket
/// its stores' cells are kept in, and its placement's fencing token, as the environment names
/// them. Tests and development use an in-memory bucket (`memory:///NAME`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// The bucket's URL, `CLADEFSD_BUCKET`.
    pub bucket: String,
    /// The placement's fencing token, `CLADEFSD_TOKEN` ([`host_token`]).
    pub token: u64,
}

impl Host {
    /// The host the environment names, or why it names none.
    pub fn from_env() -> Result<Self, String> {
        let bucket = std::env::var("CLADEFSD_BUCKET").ok().filter(|url| !url.is_empty());
        let bucket = bucket.ok_or_else(|| {
            "CLADEFSD_BUCKET names no bucket: a host of cladefs's multi-node system keeps its \
             stores' cells in one (tests and development: memory:///NAME)"
                .to_owned()
        })?;
        let token = host_token().map_err(|err| err.to_string())?;
        Ok(Self { bucket, token })
    }
}

/// The errno of `failure`, as doc-4 and doc-7 have them: a cell this writer lost, to another
/// writer or deleted, or one nothing writes, is one the store takes no more commits for (`EROFS`);
/// one a later placement holds, which a placement cut off takes back no more, or one another
/// writer moved on from this host's write-ahead log, which the store opens no more until
/// `cladefsctl take` drops it, `ESTALE`; a cell, or a version, gone, `ENOENT`; a fork's child's ID
/// taken, `EEXIST`; a cell of a newer format, `ENOTSUP`. cladefs: a cell
/// gone, `ENOENT`; a cell's name taken, `EEXIST`, or no cell's, `EINVAL` (TASK-364); a fork of
/// another tenant's version not shared, `EACCES`, and sharing what is no version, `EINVAL`
/// (TASK-395.7). None for
/// what the store failed to read or write, here or in object storage, which is the caller's `EIO`,
/// logged.
pub(crate) fn errno(failure: &Failure) -> Option<crate::Errno> {
    use crate::Errno;
    use bucket_cell::Error;
    match failure {
        Failure::Bucket(Error::Fenced | Error::Deleted) => Some(Errno::EROFS),
        Failure::Bucket(Error::Stale | Error::Diverged) => Some(Errno::ESTALE),
        Failure::Bucket(Error::NoCell) | Failure::VersionDropped => Some(Errno::ENOENT),
        Failure::Bucket(Error::IdTaken(_) | Error::OtherKind(_)) => Some(Errno::EEXIST),
        Failure::Bucket(Error::NotShared(_)) => Some(Errno::EACCES),
        // cladefs: a tenant's KEK the KMS refused, revoked, destroyed, or unreached: its stores
        // open no more until it is given back (TASK-411.3).
        Failure::Bucket(Error::Kms(_)) => Some(Errno::EACCES),
        // cladefs: an encrypted cell inherited by one whose record holds not its keys (TASK-411.4).
        Failure::Bucket(Error::Inherited { .. }) => Some(Errno::ENOTSUP),
        // A cell of a newer format than this host reads, until it is upgraded (doc-11).
        Failure::Bucket(Error::Format { .. }) => Some(Errno::ENOTSUP),
        // The collector's alone (TASK-395.5). cladefs: data encrypted under a key the host doesn't
        // hold is unread, as data it failed to read is (TASK-411.2); and a taking's race for the
        // cell's keys, which it takes again (`bucket_cell::keys::taking`).
        Failure::Bucket(
            Error::Lost(_)
            | Error::Corrupt(_)
            | Error::Read(_)
            | Error::Left(_)
            | Error::NoKey(_)
            | Error::Raced,
        )
        | Failure::Local(_) => None,
    }
}

impl From<crate::faults::Failpoint> for Failure {
    fn from(failpoint: crate::faults::Failpoint) -> Self {
        Self::from(anyhow::Error::new(failpoint))
    }
}

/// The runtime the host's stores' uploaders run on, and the budget of their uploads; cladefs: and the
/// host's keys, its KMS and those its fetchers share (TASK-411.3). Fails if the runtime can't be
/// made, a later call making it anew ([`bucket_runtime`]).
pub fn runtime() -> Result<Runtime, Failure> {
    let keys = keys::Host::new(kms()?, MEMORY.keys().clone());
    Ok(Runtime { handle: bucket_runtime()?, uploads: UPLOADS.clone(), keys })
}

/// The runtime every store's uploader, and its fetches, run on, and the calls from no runtime's
/// task (`local_cell::upload::bucket_runtime`, TASK-34, TASK-449's review): one for the process, so
/// that one runtime serves the connections to the host's buckets, whichever opened them. Fails if
/// it can't be made, a later call making it anew.
pub(crate) fn bucket_runtime() -> Result<tokio::runtime::Handle, Failure> {
    Ok(local_cell::upload::bucket_runtime()?.handle().clone())
}

/// cladefs: the host's KMS, as its environment chooses it the first time it is asked for
/// (`bucket_cell::bucket_kms::from_env`: `CLADEFS_KMS`'s, else the bucket KMS over the host's
/// bucket given a master key, TASK-411.27), or [`set_kms`] set it before (TASK-411.3), its pools of
/// new cell KEKs refilled on the bucket runtime. None chosen, or one that can't be had (a bad
/// `CLADEFS_KMS`, a master key unreadable, or given with no host's bucket to keep tenants' KEKs
/// in), it is a KMS refusing every call, naming why: the host then opens no store and makes no
/// cell. Every process that opens stores refuses to start so first ([`require_kms`],
/// TASK-411.28). Fails, keeping nothing, if the runtime its pools refill on can't be made
/// ([`bucket_runtime`], TASK-449's review).
pub fn kms() -> Result<Arc<keys::CachingKms>, Failure> {
    if let Some(kms) = POOLED.get() {
        return Ok(kms.clone());
    }
    let runtime = bucket_runtime()?;
    let kms = POOLED.get_or_init(|| match host_keys() {
        Ok(keys) => host_kms(keys.kms.clone(), runtime),
        Err(why) => host_kms(Arc::new(Unreached(why.clone())), runtime),
    });
    Ok(kms.clone())
}

/// cladefs: [`kms`], once made.
static POOLED: std::sync::OnceLock<Arc<keys::CachingKms>> = std::sync::OnceLock::new();

/// cladefs: whether the host has a KMS ([`kms`]): the refusal, naming `CLADEFS_KMS` and
/// `CLADEFS_MASTER_KEY`, if its environment chooses none, or why the one it chooses can't be had.
/// What every process that opens stores asks as it starts, refusing to start with none
/// (TASK-411.28): cladefsd, cladefsctl, the cladefs CLI's mounts.
pub fn require_kms() -> Result<(), String> {
    host_keys().as_ref().map(drop).map_err(|why| format!("the host's KMS: {why}"))
}

/// cladefs: the host's bucket KMS, if [`kms`] is that (TASK-411.27): what shreds a tenant here
/// ([`shred_tenant`]).
pub fn bucket_kms() -> Option<Arc<BucketKms<ObjectBucket>>> {
    host_keys().as_ref().ok().and_then(|keys| keys.bucket.clone())
}

/// cladefs: the host's KMS ([`kms`]), chosen once: or why it has none.
fn host_keys() -> &'static Result<HostKeys, String> {
    KMS.get_or_init(|| match bucket_cell::bucket_kms::from_env(host_bucket) {
        Ok(chosen) => Ok(HostKeys { kms: chosen.shared(), bucket: chosen.bucket().cloned() }),
        // cladefs: this crate's own unit tests, given no KMS, encrypt under the tests' KMS, each
        // tenant's KEK made on its first use, which the tests' readers share
        // (`bucket_cell::keys::testing`, TASK-411.28).
        #[cfg(test)]
        Err(why) if why == bucket_cell::bucket_kms::no_kms() => {
            Ok(HostKeys { kms: bucket_cell::keys::testing::kms(), bucket: None })
        }
        // cladefs: its integration tests, and the processes they start, given no KMS, encrypt under
        // the tests' KMS, every tenant's KEK one known key (`testing::encrypted`, TASK-411.28).
        #[cfg(all(feature = "tests-kms", not(test)))]
        Err(why) if why == bucket_cell::bucket_kms::no_kms() => Ok(testing::host_keys()),
        Err(why) => {
            log::error!("cladefs: the host's KMS: {why}");
            Err(why)
        }
    })
}

/// cladefs: the host's bucket, which the bucket KMS keeps tenants' KEKs in (`CLADEFSD_BUCKET`).
fn host_bucket() -> Result<Arc<ObjectBucket>, String> {
    let url = std::env::var("CLADEFSD_BUCKET").ok().filter(|url| !url.is_empty());
    let url = url.ok_or("CLADEFSD_BUCKET names none")?;
    crate::host_cell::bucket(&url).map_err(|err| format!("{url}: {err:#}"))
}

/// cladefs: the host's KMS, as its environment chose it, and the bucket KMS it is over, if it is
/// that.
struct HostKeys {
    kms: extent_keys::kms::Shared,
    bucket: Option<Arc<BucketKms<ObjectBucket>>>,
}

/// cladefs: the host's over `kms`: its pools of new cell KEKs refilled in batches on `runtime`, the
/// uploaders', off the paths of forks, which take from them (TASK-411.4).
fn host_kms(
    kms: extent_keys::kms::Shared,
    runtime: tokio::runtime::Handle,
) -> Arc<keys::CachingKms> {
    keys::CachingKms::pooled(kms, keys::Pooling::on(runtime))
}

/// cladefs: has the host use `kms` as its KMS, before any store opens: tests' (TASK-411.3). Once.
pub fn set_kms(kms: extent_keys::kms::Shared) -> Result<(), String> {
    KMS.set(Ok(HostKeys { kms, bucket: None }))
        .map_err(|_| "the host's KMS is set already".to_owned())
}

/// cladefs: the host's KMS ([`host_keys`]).
static KMS: std::sync::OnceLock<Result<HostKeys, String>> = std::sync::OnceLock::new();

/// cladefs: the KMS tests' and benches' processes encrypt under, their environment naming none
/// (TASK-411.28): every store is encrypted, and a process given no KMS opens none.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::HostKeys;
    use std::sync::Arc;

    /// Has this process's stores encrypt under the tests' KMS, every tenant's KEK one known key
    /// (`extent_keys::testing::KnownKms`), unless its environment names a KMS (`CLADEFS_KMS`) or
    /// gives a master key (`CLADEFS_MASTER_KEY`, `CLADEFS_MASTER_KEY_FILE`): a bench's first step,
    /// which measures a host's work, a real host's KMS adding its calls. cladefs's tests have it
    /// whatever they do, and the processes they start (the `tests-kms` feature). Before any store
    /// opens.
    pub fn encrypted() {
        let named = ["CLADEFS_KMS", "CLADEFS_MASTER_KEY", "CLADEFS_MASTER_KEY_FILE"]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
        if !named {
            let set = super::KMS.set(Ok(host_keys()));
            assert!(set.is_ok(), "the tests' KMS, set before any store opens");
        }
    }

    /// The host's keys under the tests' KMS, which every process has alike: a test's, and those it
    /// starts, whatever bucket each keeps its stores in.
    pub(super) fn host_keys() -> HostKeys {
        HostKeys { kms: Arc::new(extent_keys::testing::KnownKms), bucket: None }
    }
}

/// cladefs: what a host forgot of a tenant's or a cell's keys, told to ([`forget`], TASK-411.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// The stores open of the scope's cells it halted, which serve nothing more (`EACCES`).
    pub stores: usize,
    /// The rings of the scope's stores its fetchers held.
    pub rings: usize,
    /// The objects' tails and parts of the scope's cells its fetchers kept in memory.
    pub kept: usize,
    /// The extents it took out of the extent cache.
    pub extents: u64,
    /// The bytes of their pages it evicted.
    pub bytes: u64,
}

/// cladefs: has the host forget what it holds of `scope`'s keys (doc-16, Shredding; TASK-411.7), as
/// it is told the tenant's KEK is revoked or destroyed in the KMS, or the cell shredded, `why`
/// saying which: what the control API's `ForgetTenantKeys` and `ShredCell` do.
///
/// Every store open of the scope's cells is halted, each request failing with `EACCES`, as its
/// opening would fail; the extent cache lets go of them, and of every extent no other open store
/// holds, its pages evicted, unmapped from the windows that map them; its fetchers forget the
/// stores' rings and drop what they kept of the scope's objects; its KMS forgets the scope's cell
/// KEKs it unwrapped, and a tenant's pool of new ones, refusing them from then on. What stays in
/// memory: a halted store's own ring, and its open segment's key, until whoever holds it closes it
/// (a VM process whose guest met `EACCES`); the DEKs other stores hold, which they inherited; and
/// the pages of extents other stores hold too (a shared version's fork, a public base). On disk,
/// everything of the scope's is as its keys encrypted it: open segments, write-ahead logs, the part
/// cache.
pub fn forget(scope: keys::Scope, why: &str) -> Forgotten {
    let halted = format!("as {scope}'s keys are forgotten: {why}");
    let stores = crate::halt::halt_stores(|cell| scope.covers(cell), Errno::EACCES, &halted);
    let (bytes, extents) = crate::cache::forget(|cell| scope.covers(cell));
    let kept = MEMORY.forget_kept(scope);
    // Its KMS, if it made it: one not made yet unwrapped nothing.
    if let Some(kms) = POOLED.get() {
        kms.forget(scope, why);
    }
    let rings = MEMORY.keys().forget(scope);
    let forgotten = Forgotten { stores, rings, kept, extents, bytes };
    log::warn!("cladefs: {scope}'s keys forgotten, {why}: {forgotten:?}");
    forgotten
}

/// cladefs: shreds `cell` in `bucket` ([`bucket_cell::keys::shred`]): its wrapped KEKs taken out of
/// its head, by a conditional update; then has this host forget its keys ([`forget`]). Shredded
/// already, its head stays as it is, and the host forgets them all the same: what each host told
/// of the shredding does. Returns how many generations of its KEK it took out, and what the host
/// forgot. Only as strong as `bucket` keeps no older version of the cell's head (doc-16).
pub async fn shred_cell(
    bucket: &ObjectBucket,
    cell: crate::CellId,
) -> Result<(usize, Forgotten), bucket_cell::Error> {
    let removed = bucket_cell::keys::shred(bucket, cell).await?;
    let why = bucket_cell::keys::shredded(cell).to_string();
    Ok((removed, forget(keys::Scope::Cell(cell), &why)))
}

/// cladefs: shreds `tenant` under the host's bucket KMS (doc-16, The bucket KMS; TASK-411.27): its
/// KEK object in the host's bucket replaced by a tombstone, so that no key its KEK wrapped unwraps
/// anywhere and none is made it again ([`bucket_cell::bucket_kms::destroy`]); then has this host
/// forget its keys ([`forget`]). Shredded already, it finds it so, and the host forgets them all
/// the same: what each host told does. Refused, changing nothing, if the host's KMS is not the
/// bucket KMS: another KMS's tenant is shredded by destroying its KEK there, then
/// `ForgetTenantKeys`. Only as strong as the bucket keeps no older version of the KEK object.
pub async fn shred_tenant(
    tenant: u64,
) -> Result<(bucket_cell::bucket_kms::Destroyed, Forgotten), String> {
    let Some(kms) = bucket_kms() else {
        return Err(format!(
            "tenant {tenant}: this host's KMS is not the bucket KMS (CLADEFS_MASTER_KEY, no \
             CLADEFS_KMS): destroy the tenant's KEK in its KMS, then have every host forget its keys"
        ));
    };
    let destroyed = kms.destroy(tenant).await;
    let why = extent_keys::kms::Error::Destroyed { tenant }.to_string();
    Ok((destroyed, forget(keys::Scope::Tenant(tenant), &why)))
}

/// cladefs: a KMS the environment chose that can't be had (a bad `CLADEFS_KMS`, master keys
/// unreadable or with no host's bucket, TASK-411.27), or none chosen (TASK-411.28): every call
/// refused, saying why.
struct Unreached(String);

impl extent_keys::kms::Kms for Unreached {
    fn wrap(
        &self,
        tenant: u64,
        _: &extent_keys::Secret,
    ) -> impl std::future::Future<Output = Result<extent_keys::Wrapped, extent_keys::kms::Error>> + Send
    {
        std::future::ready(Err(self.refusal(tenant)))
    }

    fn unwrap(
        &self,
        tenant: u64,
        _: &extent_keys::Wrapped,
    ) -> impl std::future::Future<Output = Result<extent_keys::Secret, extent_keys::kms::Error>> + Send
    {
        std::future::ready(Err(self.refusal(tenant)))
    }
}

impl Unreached {
    fn refusal(&self, tenant: u64) -> extent_keys::kms::Error {
        let source = std::io::Error::other(self.0.clone());
        extent_keys::kms::Error::Io { tenant, source }
    }
}

/// cladefs: how many stores' uploaders run in this process, as it changes: what an extent daemon's
/// drain waits to see none of (TASK-141.4): Linux's alone, as the daemon is.
#[cfg(target_os = "linux")]
pub(crate) fn uploaders() -> tokio::sync::watch::Receiver<usize> {
    local_cell::upload::uploaders()
}

/// The GETs of extents' data a host has in flight: one connection to object storage gives roughly
/// 80-100 MB/s, so a host needs tens (doc-7, Reading); each holds a part, of 1 MiB.
static BUDGET: std::sync::LazyLock<Arc<Budget>> =
    std::sync::LazyLock::new(|| Budget::new(Limits { requests: 64, bytes: 64 << 20 }));

/// What a host's stores' reads keep in memory, objects' tails and the parts fetched last: once for
/// the host, however many of its stores read them, as the forks of one base read its parts, within
/// bounds that hold for the host, not for each store (TASK-92.21, TASK-92.22).
static MEMORY: std::sync::LazyLock<Arc<fetch::Memory>> =
    std::sync::LazyLock::new(fetch::Memory::new);

/// The PUTs of objects a host's stores upload, each holding its object's bytes while it goes:
/// bounded in bytes, so that a host whose stores are far behind, object storage out of reach for a
/// while, holds no more than these at once, whatever their number (TASK-79.19); and in number, as
/// its GETs are. cladefs: in bytes, its upload memory ([`set_upload_memory`], TASK-317).
static UPLOADS: std::sync::LazyLock<Arc<Budget>> = std::sync::LazyLock::new(|| {
    let bytes = *UPLOAD_MEMORY.get_or_init(|| UPLOAD_BYTES);
    Budget::new(Limits { requests: 64, bytes })
});

/// cladefs: the most bytes of objects the host's uploads hold at once ([`UPLOADS`]), as
/// [`set_upload_memory`] set it, or [`UPLOAD_BYTES`].
static UPLOAD_MEMORY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// cladefs: has the host's uploads hold at most `bytes` of the objects they upload at once
/// (TASK-317): cladefsd's `CLADEFSD_UPLOAD_MEMORY`. However far behind the bucket its stores
/// fall, what waits for it is on the host's disk, their write-ahead logs, and what of it is in
/// memory is what this lets upload at once. Before any store opens; no less than the largest
/// object a store uploads, an extent's of 64 MiB, which goes alone.
pub fn set_upload_memory(bytes: u64) -> Result<(), String> {
    if bytes < crate::OpenCell::MAX_EXTENT {
        return Err(format!(
            "an upload memory of {bytes} bytes: less than an extent's {}",
            crate::OpenCell::MAX_EXTENT
        ));
    }
    match UPLOAD_MEMORY.set(bytes) {
        Ok(()) => Ok(()),
        Err(_) if UPLOAD_MEMORY.get() == Some(&bytes) => Ok(()),
        Err(_) => Err("the upload memory is set already, as stores opened".to_owned()),
    }
}

/// cladefs: what the host's uploads hold of the objects they upload, in bytes (TASK-317).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadMemory {
    /// Held now.
    pub held: u64,
    /// The most held at once.
    pub max_held: u64,
    /// The most they may hold at once ([`set_upload_memory`]).
    pub budget: u64,
}

/// cladefs: what the host's uploads hold now, the most they held, and their budget (TASK-317).
pub fn upload_memory() -> UploadMemory {
    let (held, max_held) = UPLOADS.held();
    UploadMemory { held, max_held, budget: UPLOADS.limits().bytes }
}

/// Reads the extents a store holds whose data is in object storage (`local_cell::LocalCell::adopt`),
/// at their locations there, as the store names them: a fetcher (`bucket_cell::fetch::Fetcher`)
/// that is the host's, its reads on the bucket runtime, which their uploads use too, under the
/// host's budget of GETs and memory, asking the host's peers first if it has any.
pub struct HostFetcher {
    runtime: tokio::runtime::Handle,
    parts: Arc<fetch::Fetcher<ObjectBucket>>,
}

impl HostFetcher {
    /// What reads extents' data from `bucket`, keeping the parts it fetches in `cache`, if given,
    /// the host's.
    pub(crate) fn new(
        bucket: Arc<ObjectBucket>,
        cache: Option<Arc<Cache>>,
    ) -> Result<Arc<Self>, Failure> {
        let runtime = bucket_runtime()?;
        let runs = runtime.clone();
        // A read ahead is for the guest request whose read started it, if any (TASK-260).
        let spawn: fetch::Spawn = Arc::new(move |future| {
            drop(runs.spawn(crate::request::within(crate::request::current(), future)))
        });
        let peers = PEERS.get().filter(|(theirs, _)| Arc::ptr_eq(theirs, &bucket));
        let parts = fetch::Fetcher::new(bucket, BUDGET.clone(), MEMORY.clone(), cache);
        let parts = match peers {
            Some((_, peers)) => parts.asking(peers.clone()),
            None => parts,
        };
        Ok(Arc::new(Self { runtime, parts: Arc::new(parts.reading_ahead(spawn)) }))
    }

    /// `len` bytes at `offset` in `extent`, whose data is at `location` in the bucket: as many as
    /// it holds.
    pub fn read(
        &self,
        extent: ExtentId,
        location: Location,
        offset: u64,
        len: u64,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, Failure>> + Send + use<> {
        let parts = self.parts.clone();
        // cladefs: for the guest request current, if any, there too: what the object store's
        // requests for it log (TASK-260).
        let read =
            self.runtime.spawn(crate::request::within(crate::request::current(), async move {
                Ok(parts.read(extent, &location, offset, len, Priority::Demand).await?)
            }));
        async move { read.await.map_err(|err| Failure::local(err, "fetching"))? }
    }
}

/// cladefs: the hosts sharing the host's bucket, the one they share, which its stores' fetchers of it
/// ask for parts before it, if it has any ([`ask_peers`], TASK-406.2).
static PEERS: std::sync::OnceLock<(Arc<ObjectBucket>, Arc<peers::Peers>)> =
    std::sync::OnceLock::new();

/// cladefs: has the fetchers of `bucket`, the host's, made from now on ask `peers`, the hosts sharing
/// it, for the parts they own before it (TASK-406.2): before any store opens. Once.
pub fn ask_peers(bucket: Arc<ObjectBucket>, peers: Arc<peers::Peers>) -> Result<(), String> {
    PEERS.set((bucket, peers)).map_err(|_| "the host's peers are set already".to_owned())
}

/// cladefs: the hosts sharing the host's bucket, if it asks them for parts ([`ask_peers`]).
pub fn peers() -> Option<Arc<peers::Peers>> {
    PEERS.get().map(|(_, peers)| peers.clone())
}

/// cladefs: what serves the hosts sharing `bucket`, the host's, the parts they ask it for, as stored
/// (TASK-406.2): a fetcher of it, keeping what it fetches in `cache`, the host's, as its stores'
/// fetchers do, with them, under the host's budget, and asking no peer.
pub fn serving_peers(bucket: Arc<ObjectBucket>, cache: Arc<Cache>) -> Result<Serving, Failure> {
    Ok(Serving {
        runtime: bucket_runtime()?,
        parts: Arc::new(fetch::Fetcher::new(bucket, BUDGET.clone(), MEMORY.clone(), Some(cache))),
    })
}

/// cladefs: what serves the host's peers the parts they ask for ([`serving_peers`]).
#[derive(Clone)]
pub struct Serving {
    runtime: tokio::runtime::Handle,
    parts: Arc<fetch::Fetcher<ObjectBucket>>,
}

impl Serving {
    /// The stored bytes of the part `asked` names, if the host has it: kept, or
    /// fetched from the bucket once, however many ask, on the bucket runtime.
    pub async fn part(&self, asked: peers::Asked) -> Result<Option<Arc<[u8]>>, Failure> {
        let parts = self.parts.clone();
        let serving = self.runtime.spawn(async move {
            let peers::Asked { key, part } = asked;
            Ok(parts.stored_part(&key, part).await?)
        });
        serving.await.map_err(|err| Failure::local(err, "serving a peer"))?
    }
}

pub mod peers;

#[cfg(test)]
mod fork_faults;

#[cfg(test)]
mod lifecycle;

#[cfg(test)]
mod restore_faults;

#[cfg(test)]
mod write_budgets;

#[cfg(test)]
mod tests {
    use crate::test_checkpoints::checkpointed;
    use crate::test_data::noise;
    use crate::{ExtentId, OpenCell, VolatileSlice};
    use bucket_cell::cache::Cache;
    use bucket_cell::object_bucket::ObjectBucket;
    use bucket_cell::testing::read;
    use bucket_cell::{lifecycle, recover};
    use object_store::memory::InMemory;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

    /// The URL `bucket` is this process's by, made so if it is none's yet: a memory bucket of its
    /// own, by its address, which the stores the tests make in it name (TASK-165.9).
    pub(super) fn url_of(bucket: &Arc<ObjectBucket>) -> String {
        let url = format!("memory:///bucket-tests-{:p}", Arc::as_ptr(bucket));
        crate::host_cell::use_bucket(&url, bucket.clone());
        url
    }

    /// A new store at `path`, of `capacity` bytes, of `cell`, a new one if none, its cell kept
    /// in `bucket`, its uploader uploading as `keeping` says.
    pub(super) fn kept_in(
        path: impl AsRef<std::path::Path>,
        (capacity, cell): (u64, Option<crate::CellId>),
        bucket: &Arc<ObjectBucket>,
        keeping: local_cell::upload::Options,
    ) -> Result<OpenCell, crate::Error> {
        let cell = cell.unwrap_or_else(crate::host_cell::new_alone);
        OpenCell::create_kept_with(path, capacity, cell, &url_of(bucket), keeping)
    }

    /// Has the store at `path` name `bucket` as the bucket its cell is kept in: a host restarted
    /// whose stores reach the bucket otherwise than before, as the fault tests' do (TASK-165.9).
    /// A store whose host died before it described it names nothing.
    pub(super) fn naming(path: &std::path::Path, bucket: &Arc<ObjectBucket>) {
        let description = path.join("store");
        let text = match std::fs::read_to_string(&description) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            text => text.unwrap(),
        };
        let named: String = (text.lines())
            .map(|line| match line.starts_with("bucket ") {
                true => format!("bucket {}\n", url_of(bucket)),
                false => format!("{line}\n"),
            })
            .collect();
        std::fs::write(&description, named).unwrap();
    }

    /// The store at `path`, opened, its uploader uploading as `keeping` says.
    pub(super) fn reopened(
        path: impl AsRef<std::path::Path>,
        keeping: local_cell::upload::Options,
    ) -> Result<OpenCell, crate::Error> {
        OpenCell::open_with(
            path,
            crate::open_cell::Opening { keeping, ..crate::open_cell::Opening::here()? },
        )
    }

    /// Uploads only when a commit waits.
    /// How this process's stores keep their cells ([`crate::host_cell::keeping`]), but
    /// uploading nothing until a commit waits for it, or the hour is up.
    pub(super) fn idle() -> local_cell::upload::Options {
        let keeping = crate::host_cell::keeping().unwrap();
        local_cell::upload::Options { interval: Duration::from_secs(3600), ..keeping }
    }

    pub(super) fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed ^ (i % 251) as u8).collect()
    }

    fn write(store: &OpenCell, sequence: u64, data: &[u8]) -> ExtentId {
        let extent = ExtentId { cell: store.cell().id, sequence };
        store.write(extent, 0, &[VolatileSlice::from(data)]).unwrap();
        extent
    }

    /// A cell's extents, by sequence number, with their data, and its root.
    pub(super) type Uploaded = (Vec<(u64, Vec<u8>)>, Option<Vec<u8>>);

    /// The cell's extents and root, read back from `bucket`.
    pub(super) fn uploaded(bucket: &Arc<ObjectBucket>, cell: crate::CellId) -> Uploaded {
        futures::executor::block_on(async {
            let state = recover(&**bucket, cell, None).await.unwrap().state;
            let mut extents = Vec::new();
            for (extent, location) in &state.extents {
                extents.push((extent.sequence, read(bucket, *extent, location).await.unwrap()));
            }
            (extents, state.root)
        })
    }

    /// A store's extents, its root, and its commits made durable are uploaded; extents written and
    /// freed between two uploads never are.
    #[test]
    fn uploads_what_the_store_makes_durable() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "cells"));
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = store.cell();
        write(&store, 1, &pattern(1, BLOCK));
        store.commit(Some(b"first")).unwrap();
        // What it held before is uploaded on attaching.
        assert_eq!(
            uploaded(&bucket, cell),
            (vec![(1, pattern(1, BLOCK))], Some(b"first".to_vec()))
        );

        let large = pattern(3, 4 << 20);
        write(&store, 2, &pattern(2, 2 * BLOCK));
        write(&store, 3, &large);
        write(&store, 4, &pattern(4, BLOCK));
        store.commit_ordered(None).unwrap();
        store.free(ExtentId { cell: cell.id, sequence: 1 }).unwrap();
        store.free(ExtentId { cell: cell.id, sequence: 4 }).unwrap();
        store.commit(Some(b"second")).unwrap();
        assert_eq!(
            uploaded(&bucket, cell),
            (vec![(2, pattern(2, 2 * BLOCK)), (3, large.clone())], Some(b"second".to_vec()))
        );
        // Extent 4, written and freed between two uploads, never was.
        let keys = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, "")).unwrap();
        assert_eq!(
            keys.iter()
                .filter(|meta| matches!(
                    bucket_cell::layout::kind(&meta.key),
                    bucket_cell::layout::Kind::Extent { .. }
                ))
                .count(),
            1
        );
        let segments = keys
            .iter()
            .filter(|meta| {
                matches!(
                    bucket_cell::layout::kind(&meta.key),
                    bucket_cell::layout::Kind::Segment { .. }
                )
            })
            .count();

        // Opened again, the store's uploader finds the cell as it left it.
        drop(store);
        let store = reopened(dir.path().join("store"), idle()).unwrap();
        // Its guest mints from the second writer's range.
        let info = store.info().unwrap();
        assert_eq!(info.sequence_floor, 2 << bucket_cell::EPOCH_SHIFT);
        // What the store holds ends far below: the guest looks for what a crash left there only.
        assert!(info.next_sequence < 1 << bucket_cell::EPOCH_SHIFT, "{info:?}");
        let keys = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, "")).unwrap();
        // No segment more: the store takes the cell by its head alone.
        assert_eq!(
            keys.iter()
                .filter(|meta| matches!(
                    bucket_cell::layout::kind(&meta.key),
                    bucket_cell::layout::Kind::Segment { .. }
                ))
                .count(),
            segments
        );
        write(&store, 5, &pattern(5, BLOCK));
        store.commit(None).unwrap();
        let (extents, root) = uploaded(&bucket, cell);
        assert_eq!(extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(), [2, 3, 5]);
        assert_eq!(root, Some(b"second".to_vec()));
    }

    /// doc-7, The head: a store taking a cell over from a writer still uploading, two segments
    /// of it in flight, commits on. The writer's segments land at keys naming its epoch, which the
    /// store's never are, and its head swap fails (`Fenced`): what it wrote, no head names, and the
    /// store keeps its head. Before (TASK-407), its late segment landed where the store's first
    /// went, and the store took the cell again.
    #[test]
    fn takeovers_of_writers_still_uploading_keep_their_guests() {
        use bucket_cell::{Commit, SegmentBuilder, Writer};
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let (cell, token) = (crate::host_cell::new_alone(), crate::host_cell::token().unwrap());
        // The writer before, its two segments in flight.
        let before = futures::executor::block_on(async {
            lifecycle::create(&*bucket, cell, 1).await.unwrap();
            Writer::open(bucket.clone(), cell, 10, token).await.unwrap()
        });
        let mut segments = Vec::new();
        for tag in [1, 2] {
            let commit = Commit { tag, ..Commit::default() };
            let objects = SegmentBuilder::from(commit)
                .close(&mut before.chain())
                .frame(before.keys().encrypting());
            segments.push(before.add_segment(objects).unwrap());
        }
        // A store takes the cell over, opening, its fence after the writer's.
        let path = dir.path().join("store");
        made_for(&path, cell, &bucket);
        let store = reopened(&path, idle()).unwrap();
        // Both land, the second first, at keys of the writer's epoch; its head swap fails.
        futures::executor::block_on(async {
            before.upload(segments[1]).await.unwrap();
            before.upload(segments[0]).await.unwrap();
            assert_eq!(before.acknowledge().await, Err(bucket_cell::Error::Fenced));
        });
        // The store's commit is the cell's, its head as it was.
        write(&store, 1, &pattern(1, BLOCK));
        let epoch = store.uploader().unwrap().status().unwrap().taking.unwrap().epoch;
        let committed = store.commit(Some(b"after"));
        let uploader = store.uploader().unwrap();
        assert_eq!((committed, uploader.failed()), (Ok(()), None));
        assert_eq!(uploader.status().unwrap().taking.unwrap().epoch, epoch);
        let after = (vec![(1, pattern(1, BLOCK))], Some(b"after".to_vec()));
        assert_eq!(uploaded(&bucket, cell), after);
    }

    /// TASK-195: a store reads what it holds after the collector compacted it twice, its writer
    /// checkpointing between, with the host's cache and without. It names an extent where the
    /// writer's checkpoints move it, as the writer does (`Writer::following`), from where the
    /// collector keeps the moves: compacted from its segment into moves, and from those into moves
    /// of moves, the segment goes, and the first moves. Before, the store named it in the segment
    /// as long as it held it: gone, and its moves gone, the guest's read failed (`EIO`) on a live
    /// cell, which a recovery read through the moves of moves.
    #[test]
    fn extents_compacted_twice_read_where_the_store_names_them() {
        compacted_twice(false);
        compacted_twice(true);
    }

    /// TASK-217: a store whose host died, its disk as the death left it and the host's cache lost
    /// with it, opens and reads what it holds after the collector compacted its segment and took
    /// it, the writer's checkpoint having followed the moves: opening names its extents as the
    /// cell's newest checkpoint does, in the moves. Before (GCE r757, round 40, a soak's daemon
    /// killed and its cache lost), the store opened from its own index, which named the extent in
    /// the segment the collector had taken, and the checking VM's reads failed EIO (`lost: ... in
    /// segments/<cell>/<segment>`); TASK-165.9 removed the index, and the opening recovers the
    /// cell.
    #[test]
    fn extents_compacted_read_once_their_host_died_and_its_cache_went() {
        const EXTENT: usize = 64 * BLOCK;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let objects: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        // The dying host's way to the objects, and what reaches them after it.
        let gated = Arc::new(Gated::over(objects.clone()));
        let reached = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let bucket = Arc::new(ObjectBucket::new(objects, ""));
        let options = bucket_cell::collector::Options { compact_below: 0.5, grace: 0 };
        let collect = |holder| {
            let pass = bucket_cell::collector::collect(&*bucket, holder, &options);
            futures::executor::block_on(pass).unwrap()
        };
        let cache = Some(Arc::new(Cache::open(&dir.path().join("cache"), 1 << 30, 0).unwrap()));
        let keeping = local_cell::upload::Options { cache, ..idle() };
        // One segment, of extents 1 to 16, which don't compress, freed but 1 to 3.
        let store = kept_in(&path, (16 * EXTENT as u64, None), &reached, keeping).unwrap();
        let cell = store.cell();
        for sequence in 1..=16 {
            write(&store, sequence, &noise(sequence, EXTENT));
        }
        store.commit(None).unwrap();
        for sequence in 4..=16 {
            store.free(ExtentId { cell: cell.id, sequence }).unwrap();
        }
        checkpointed(&store, &bucket);
        // The segment compacted, 1 to 3 into its moves; the writer's checkpoint follows them; the
        // segment goes.
        assert_eq!(collect(1).compacted, 1);
        checkpointed(&store, &bucket);
        collect(2);
        let keys: Vec<String> =
            futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, ""))
                .unwrap()
                .into_iter()
                .map(|meta| meta.key)
                .collect();
        let moves = bucket_cell::layout::moves_under(&bucket_cell::layout::segments(cell)).unwrap();
        let first = bucket_cell::layout::segment(cell, 0, 1);
        assert!(keys.iter().any(|key| key.starts_with(&moves)), "{keys:?}");
        assert!(!keys.contains(&first), "{keys:?}");
        // The host dies: its disk copied as it is, its cache not, its store cut off.
        let restarted = dir.path().join("restarted");
        restart(&path, &restarted);
        gated.sever();
        naming(&restarted, &bucket);
        let opened = reopened(&restarted, idle()).unwrap();
        let mut data = vec![0; EXTENT];
        let read = opened.read(
            ExtentId { cell: cell.id, sequence: 1 },
            0,
            &mut [crate::VolatileSliceMut::from(&mut data[..])],
        );
        assert_eq!(read.map(|()| data == noise(1, EXTENT)), Ok(true));
        drop(store);
    }

    /// TASK-195: so does a fork's store read what it inherited, its parent deleted: it names the
    /// extents where its own writer's checkpoints move them, from the parent's segment, compacted
    /// into moves, the segment gone. TASK-395.4: the fork in a cell of its own, as every fork a host
    /// makes, its parent's cell keeps the parent's state at the origin whole for as long as the
    /// fork lives, by its reference, what the fork frees of it since no matter: the moves, which
    /// hold that state's extents, are compacted no further, as they were when the collector went
    /// by the fork's state (`extents_compacted_twice_read_where_the_store_names_them` shows a
    /// cell's own compacted twice).
    #[test]
    fn forks_read_what_they_inherited_compacted_twice() {
        inherited_compacted_twice();
    }

    /// TASK-339: [`forks_read_what_they_inherited_compacted_twice`] with the fork's checkpoint
    /// landing a second after the commits it covers returned, as on a loaded CI runner: it is what
    /// the collector frees the segment and the first moves by, and the collections waited for
    /// nothing of it (moves/segments/ still listed after collect(4), main's macOS CI); now they
    /// wait for it ([`checkpointed`](crate::test_checkpoints::checkpointed)).
    #[test]
    fn forks_read_what_they_inherited_compacted_twice_their_checkpoint_late() {
        let scenario = fail::FailScenario::setup();
        fail::cfg("upload::checkpointing", "sleep(1000)").unwrap();
        inherited_compacted_twice();
        scenario.teardown();
    }

    fn inherited_compacted_twice() {
        const EXTENT: usize = 64 * BLOCK;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let options = bucket_cell::collector::Options { compact_below: 0.5, grace: 0 };
        let collect = |holder| {
            let pass = bucket_cell::collector::collect(&*bucket, holder, &options);
            futures::executor::block_on(pass).unwrap()
        };
        // The parent's segment, of extents 1 to 16, which don't compress, freed but 1 to 3; its
        // fork, which inherits them there; and the parent deleted.
        let path = dir.path().join("parent");
        let parent = kept_in(&path, (16 * EXTENT as u64, None), &bucket, idle()).unwrap();
        let cell = parent.cell();
        for sequence in 1..=16 {
            write(&parent, sequence, &noise(sequence, EXTENT));
        }
        parent.commit(None).unwrap();
        for sequence in 4..=16 {
            parent.free(ExtentId { cell: cell.id, sequence }).unwrap();
        }
        parent.commit(None).unwrap();
        let child = dir.path().join("child");
        let forked = crate::host_cell::fork_child(cell);
        assert_ne!(forked, cell, "a fork in a cell of its own");
        parent.fork_to(&child, forked).unwrap();
        drop(parent);
        crate::host_cell::delete(&path, Some(&bucket), crate::host_cell::token).unwrap();
        // The fork's store, open all along: the segment compacted, 1 to 3 in its moves.
        let child = reopened(&child, idle()).unwrap();
        assert_eq!(collect(1).compacted, 1);
        // It frees 2 and 3, and its writer checkpoints, naming 1 in the moves.
        child.free(ExtentId { cell: cell.id, sequence: 2 }).unwrap();
        child.free(ExtentId { cell: cell.id, sequence: 3 }).unwrap();
        checkpointed(&child, &bucket);
        // The segment goes, no state naming it there; the moves stay as they are, the origin
        // holding 1 to 3 in them.
        collect(3);
        collect(4);
        let keys: Vec<String> =
            futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, ""))
                .unwrap()
                .into_iter()
                .map(|meta| meta.key)
                .filter(|key| {
                    matches!(
                        bucket_cell::layout::kind(key),
                        bucket_cell::layout::Kind::Moves { .. }
                            | bucket_cell::layout::Kind::Segment { .. }
                    ) && key.starts_with(&cell.prefix())
                })
                .collect();
        let sealed = bucket_cell::layout::segments(cell);
        let once = bucket_cell::layout::moves_under(&sealed).unwrap();
        let twice = bucket_cell::layout::moves_under(&once).unwrap();
        let compacted: Vec<String> = (keys.iter())
            .filter(|key| key.starts_with(&once))
            .filter_map(|key| bucket_cell::layout::moved_from(key))
            .collect();
        assert_eq!(compacted.len(), 1, "{keys:?}");
        assert!(!keys.contains(&compacted[0]), "{keys:?}");
        assert!(!keys.iter().any(|key| key.starts_with(&twice)), "{keys:?}");
        let mut data = vec![0; EXTENT];
        let extent = ExtentId { cell: cell.id, sequence: 1 };
        let read = child.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]);
        assert_eq!(read.map(|()| data == noise(1, EXTENT)), Ok(true));
    }

    /// [`extents_compacted_twice_read_where_the_store_names_them`], with the host's cache if
    /// `cached`.
    fn compacted_twice(cached: bool) {
        const EXTENT: usize = 64 * BLOCK;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let options = bucket_cell::collector::Options { compact_below: 0.5, grace: 0 };
        let collect = |holder| {
            let pass = bucket_cell::collector::collect(&*bucket, holder, &options);
            futures::executor::block_on(pass).unwrap()
        };
        let cache =
            cached.then(|| Arc::new(Cache::open(&dir.path().join("cache"), 1 << 30, 0).unwrap()));
        let keeping = local_cell::upload::Options { cache, ..idle() };
        // One segment, of extents 1 to 16, which don't compress, freed but 1 to 3.
        let store = kept_in(&path, (16 * EXTENT as u64, None), &bucket, keeping).unwrap();
        let store = Arc::new(store);
        let cell = store.cell();
        for sequence in 1..=16 {
            write(&store, sequence, &noise(sequence, EXTENT));
        }
        store.commit(None).unwrap();
        for sequence in 4..=16 {
            store.free(ExtentId { cell: cell.id, sequence }).unwrap();
        }
        // Its writer checkpoints, as every so many segments, each commit here one.
        checkpointed(&store, &bucket);
        // The segment compacted: 1 to 3 in its moves.
        assert_eq!(collect(1).compacted, 1);
        // The store frees 2 and 3, and its writer checkpoints, as every so many segments, naming 1
        // in the moves, which hold a third of what they did.
        store.free(ExtentId { cell: cell.id, sequence: 2 }).unwrap();
        store.free(ExtentId { cell: cell.id, sequence: 3 }).unwrap();
        checkpointed(&store, &bucket);
        // A follower of the store, which reads what the store names.
        let follower = store.follow();
        // The segment goes, no state naming it there; the moves are compacted, then go too.
        collect(3);
        collect(4);
        let keys: Vec<String> =
            futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, ""))
                .unwrap()
                .into_iter()
                .map(|meta| meta.key)
                .filter(|key| {
                    matches!(
                        bucket_cell::layout::kind(key),
                        bucket_cell::layout::Kind::Moves { .. }
                            | bucket_cell::layout::Kind::Segment { .. }
                    )
                })
                .collect();
        let once = bucket_cell::layout::moves_under(&bucket_cell::layout::segments(cell)).unwrap();
        let twice = bucket_cell::layout::moves_under(&once).unwrap();
        assert!(keys.iter().any(|key| key.starts_with(&twice)), "{keys:?}");
        assert!(!keys.iter().any(|key| key.starts_with(&once)), "{keys:?}");
        // The store, open all along, reads it there, and so does its follower.
        let extent = ExtentId { cell: cell.id, sequence: 1 };
        let reads = |store: &OpenCell| {
            let mut data = vec![0; EXTENT];
            let read = store.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]);
            read.map(|()| data == noise(1, EXTENT))
        };
        assert_eq!([reads(&store), reads(&follower)], [Ok(true), Ok(true)], "cached: {cached}");
    }

    /// The footers of `cell`'s segments in `bucket`, in order.
    fn footers(bucket: &ObjectBucket, cell: crate::CellId) -> Vec<bucket_cell::layout::Footer> {
        use bucket_cell::layout;
        futures::executor::block_on(async {
            let listed = bucket_cell::Bucket::list(bucket, &layout::segments(cell)).await;
            let mut footers = Vec::new();
            for meta in listed.unwrap() {
                let object = bucket_cell::Bucket::get(bucket, &meta.key).await.unwrap();
                footers.push(layout::footer(&object.bytes).unwrap());
            }
            footers
        })
    }

    /// TASK-165.9: a store whose bucket was out of reach holds its commits in its write-ahead log,
    /// segments the commits closed, which upload once it is in reach again, in order, each the
    /// object its file is; what the commits made, and the root, the cell has then. (Before, a store attaching uploaded what its cell lacked in batches.)
    #[test]
    fn logs_caught_up_upload_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let store = kept_in(dir.path().join("store"), (1 << 30, None), &bucket, idle()).unwrap();
        let cell = store.cell();
        gated.shut.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut written = Vec::new();
        for sequence in 1..=3 {
            let data = pattern(sequence as u8, 2 * BLOCK);
            write(&store, sequence, &data);
            written.push((sequence, data));
            store.commit_ordered(Some(format!("root {sequence}").as_bytes())).unwrap();
        }
        store.flush().unwrap();
        // Out of reach: the store holds what it committed, the bucket nothing of it.
        let behind = Arc::new(ObjectBucket::new(gated.store.clone(), ""));
        assert_eq!(uploaded(&behind, cell), (vec![], None));
        gated.shut.store(false, std::sync::atomic::Ordering::Relaxed);
        store.wait_uploaded().unwrap();
        let footers = footers(&bucket, cell);
        let commits: Vec<_> = footers.iter().map(|footer| footer.contents.commits.len()).collect();
        assert_eq!(commits, [1], "the segment of the commits");
        assert_eq!(uploaded(&bucket, cell), (written, Some(b"root 3".to_vec())));
    }

    /// A store whose commit, of more bytes than `options` let wait, is in a segment closed for want
    /// of room, as a closing that beats the uploader leaves it: with no commit to come, as one in
    /// the lag's wait holds its guest's seal, and holds back every other. And the commit's epoch.
    fn closed_for_room(
        options: local_cell::upload::Options,
    ) -> (tempfile::TempDir, crate::host_cell::Opened, u64) {
        const EXTENT: usize = 1 << 20;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let path = dir.path().join("store");
        let opened = crate::host_cell::create_unmade(
            &path,
            1 << 30,
            crate::host_cell::CellOf::Named(bucket_cell::layout::test_cell(7)),
            bucket_cell::cell::Kind::Fxfs,
            Some(&url_of(&bucket)),
            options,
        );
        let opened = opened.unwrap();
        let store = &opened.store;
        store.write(super::ExtentId::new(7, 1), 0, &pattern(1, EXTENT)).unwrap();
        store.commit_ordered(None).unwrap();
        let epoch = store.last_commit().unwrap();
        // The most a segment holds, its last extent past it, and no commit with them.
        for sequence in 2..98 {
            let extent = super::ExtentId::new(7, sequence);
            store.write(extent, 0, &pattern(sequence as u8, EXTENT)).unwrap();
        }
        (dir, opened, epoch)
    }

    /// TASK-150.2: a commit stalled on the bytes waiting (`Options::max_lag_bytes`) ends though
    /// what it waits for is in a segment closed for want of room, which only a commit's flush would
    /// end, as the commit, or its guest, have none to come: an uploader's own flush ends it.
    #[test]
    fn commits_stalled_on_the_lag_end_with_their_segment_closed_for_room() {
        let lag = local_cell::upload::Options { max_lag_bytes: BLOCK as u64, ..idle() };
        let (_dir, crate::host_cell::Opened { store, uploader, .. }, epoch) = closed_for_room(lag);
        let stalled = std::sync::Arc::new(uploader);
        let waiting = stalled.clone();
        let (ended, end) = std::sync::mpsc::channel();
        std::thread::spawn(move || ended.send(waiting.wait_lag(epoch)).unwrap());
        assert_eq!(end.recv_timeout(Duration::from_secs(30)), Ok(Ok(())));
        assert_eq!(store.wal_bytes().unwrap(), 0);
    }

    /// TASK-150.2: so does a sync of the store's commits, or anything else waiting for their upload.
    #[test]
    fn uploads_waited_for_end_with_their_segment_closed_for_room() {
        let (_dir, crate::host_cell::Opened { uploader, .. }, epoch) = closed_for_room(idle());
        let (ended, end) = std::sync::mpsc::channel();
        std::thread::spawn(move || ended.send(uploader.wait_uploaded(epoch)).unwrap());
        assert_eq!(end.recv_timeout(Duration::from_secs(30)), Ok(Ok(())));
    }

    /// A store's uploader holds its directory taken until it stops, after the store closed: the
    /// store's closing let it go, and its uploader, which uploads the store's last commits as it
    /// stops, wrote the cell's marker in the directory after, while another process, a crash
    /// test's copy, had taken it, and the store at the version found the marker's new file gone (TASK-129, GCE, the
    /// DAX gate's second run).
    #[test]
    fn uploaders_hold_their_stores_directory_until_they_stop() {
        use std::os::fd::AsRawFd;
        // Whether another process could take the directory at `path` now, as `Dir::take` does.
        let free = |path: &std::path::Path| {
            let dir = std::fs::File::open(path).unwrap();
            // SAFETY: Plain system call on the directory's descriptor, which closing unlocks.
            unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let cell = crate::host_cell::new_alone();
        let opened = crate::host_cell::create(
            &path,
            1 << 30,
            crate::host_cell::CellOf::Named(cell),
            bucket_cell::cell::Kind::Fxfs,
            idle(),
        )
        .unwrap();
        let crate::host_cell::Opened { store, uploader, .. } = opened;
        assert!(!free(&path), "taken while the store is open");
        store.close().unwrap();
        assert!(!free(&path), "taken while its uploader runs");
        drop(uploader);
        assert!(free(&path), "free once the uploader stopped");
    }

    /// TASK-79.19: a benchmark of a store's uploads keeping up with sustained writes, to a bucket
    /// whose PUTs take 350 ms each, as S3's of a segment of 32 MiB may: 4 GiB written, a MiB an
    /// extent, an ordered commit after each 32 MiB, as a guest's writeback makes them, each waiting
    /// while the lag is past its bound, as the store's do; how long until it is all uploaded, at how
    /// many MB/s, and how long the commits waited for the lag, in all.
    /// `cargo nextest run -p cladefs --run-ignored only bench_sustained_uploads --no-capture`.
    #[test]
    #[ignore = "a benchmark: --run-ignored only, alone"]
    fn bench_sustained_uploads() {
        use object_store::throttle::{ThrottleConfig, ThrottledStore};
        const WRITTEN: u64 = 4 << 30;
        const EXTENT: usize = 1 << 20;
        let put = Duration::from_millis(350);
        let config = ThrottleConfig { wait_put_per_call: put, ..ThrottleConfig::default() };
        let objects = Arc::new(ThrottledStore::new(InMemory::new(), config));
        let bucket = Arc::new(ObjectBucket::new(objects, ""));
        let dir = tempfile::tempdir().unwrap();
        let store = kept_in(
            dir.path().join("store"),
            (2 * WRITTEN, None),
            &bucket,
            local_cell::upload::Options::default(),
        )
        .unwrap();
        let data = noise(1, EXTENT);
        let (started, mut waited) = (std::time::Instant::now(), Duration::ZERO);
        for sequence in 1..=WRITTEN / EXTENT as u64 {
            write(&store, sequence, &data);
            if sequence % 32 == 0 {
                let committing = std::time::Instant::now();
                store.commit_ordered(None).unwrap();
                waited += committing.elapsed();
            }
        }
        let written = started.elapsed();
        store.wait_uploaded().unwrap();
        let took = started.elapsed();
        let stalls = store.uploader().unwrap().stalls();
        let rate = WRITTEN as f64 / took.as_secs_f64() / 1e6;
        println!(
            "{} MiB written in {written:.2?}, uploaded in {took:.2?}: {rate:.0} MB/s; commits took \
             {waited:.2?}, {stalls} of them waiting for the lag",
            WRITTEN >> 20
        );
    }

    /// TASK-443.2: a benchmark of what each open, idle fork of a base holds of the process's heap,
    /// by the extents the base holds, and of how long its opening takes: the heap counted exactly
    /// as it is allocated (`crate::heap::held`), not read from the process's resident memory. A
    /// base of each of `CLADEFS_BENCH_EXTENTS` extents (64, 1,024, 32,768 and 300,000 unless set, a
    /// list by commas) of a block each, in a memory bucket, forked `CLADEFS_BENCH_FORKS` times (100
    /// unless set; its first count alone), once from the store itself, live, whose writer writes a
    /// checkpoint for each fork, as `cladefsd::tests::serves_hundreds_of_forks_of_a_base` makes its
    /// forks, and once from a version of it, whose forks all start from the version's one state
    /// (`CLADEFS_BENCH_PARENTS`, `live,version` unless set). A line for each count of forks open,
    /// then one of what holds it ([`fork_extent_memory`]).
    /// TASK-453: and it fails unless each fork past the first holds [`IDLE_FORK_BYTES`] at most,
    /// whatever its base holds, and the daemon's count of the stores' maps
    /// (`cladefsd::process::AnonBudget::extents`) covers what they hold.
    /// In the sim profile, optimized as a release build is, which this crate's tests, with their
    /// failpoints, refuse to be (TASK-320): the bytes are any build's, the times this one's.
    /// `cargo nextest run -p cladefs --cargo-profile sim --run-ignored only
    /// bench_fork_extent_memory --no-capture`.
    #[test]
    #[ignore = "a benchmark (TASK-443.2): --run-ignored only, in the sim profile"]
    fn bench_fork_extent_memory() {
        let listed = |name: &str, default: &str| -> Vec<String> {
            let list = std::env::var(name).unwrap_or_else(|_| default.to_owned());
            list.split(',').map(|item| item.trim().to_owned()).collect()
        };
        let count = |item: &String| -> u64 { item.parse().expect("a count") };
        let forks = count(&listed("CLADEFS_BENCH_FORKS", "100")[0]) as usize;
        let parents = listed("CLADEFS_BENCH_PARENTS", "live,version");
        for extents in listed("CLADEFS_BENCH_EXTENTS", "64,1024,32768,300000").iter().map(count) {
            for parent in &parents {
                let version = match parent.as_str() {
                    "live" => false,
                    "version" => true,
                    other => panic!("CLADEFS_BENCH_PARENTS: {other:?}, neither live nor version"),
                };
                fork_extent_memory(extents, forks, version);
            }
        }
    }

    /// TASK-453: the most bytes of the heap an open, idle fork holds past the first of its base's,
    /// whatever its base holds: what it inherited it shares with the others, its store's and its
    /// writer's maps the decoded state they started from, and holds of its own what it wrote, here
    /// nothing. Measured 31-34 KB a fork (`bench_fork_extent_memory`), 221-468 bytes for each of
    /// the base's extents more before (TASK-443.2).
    const IDLE_FORK_BYTES: u64 = 64 << 10;

    /// What the stores' maps of their extents hold now, as the extent daemon counts it
    /// (`cladefsd::process::extents_held`, Linux's): what the stores hold of their own, and the
    /// decoded states they share, each once.
    fn extents_counted() -> (local_cell::Own, bucket_cell::recovery::Held) {
        (local_cell::own_held(), crate::host_cell::decoded_held())
    }

    /// Opens `forks` forks of a base of `extents` extents, forked from a version of it if `version`,
    /// from the store itself otherwise, and prints what the heap holds more than before them with
    /// 1, 10, 20, 50 and 100 of them open, in all, for each, for each opened since the count before
    /// (marginal), and that for each extent of the base; and how long each of those took to open.
    /// TASK-453: each past the first holds [`IDLE_FORK_BYTES`] at most, the decoded state they
    /// share one, of the base's extents, counted at [`bucket_cell::recovery::ENTRY_BYTES`] an
    /// entry, which covers what the first held more than the others, and a map of them built
    /// sharing nothing, as the writer's of a base written live is. Then what closing them all
    /// leaves held over the heap before them; opened and closed again, the heap is to the byte
    /// what the first closing left (TASK-471: not always, by 80 to 2,048 bytes), checked last.
    /// Then one fork more writes, commits and uploads a block for every extent of the base, up to
    /// 4,096: what it holds more, what closing it lets go of more than closing an idle one does,
    /// what it holds of its own, counted ([`local_cell::Own::bytes`]), covers. And one more frees
    /// every 32nd extent it inherited, each its writer's map's change apart, a path of its nodes
    /// copied ([`bucket_cell::recovery::RUN_BYTES`]), and checkpoints: what closing it lets go of
    /// more than an idle one does, counted too; and opened again, from its checkpoint, its map made
    /// of the base's, what the heap holds more than an idle fork the bucket's decoded states count
    /// more.
    fn fork_extent_memory(extents: u64, forks: usize, version: bool) {
        use crate::host_cell::fork_child;
        let held = || crate::heap::held() as u64;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let capacity = 2 * extents * BLOCK as u64 + (1 << 30);
        let base = kept_in(dir.path().join("base"), (capacity, None), &bucket, idle()).unwrap();
        let data = pattern(1, BLOCK);
        for sequence in 1..=extents {
            write(&base, sequence, &data);
            if sequence % 1024 == 0 {
                base.commit(None).unwrap();
            }
        }
        base.commit(None).unwrap();
        let base_cell = base.cell().id;
        let parent = match version {
            false => base,
            true => {
                let path = dir.path().join("copy");
                base.uploaded_version_at(&path).unwrap();
                drop(base);
                reopened(&path, idle()).unwrap()
            }
        };
        // And two more, which write and free, open apart from the others.
        let mut paths: Vec<_> = (0..forks + 2)
            .map(|n| {
                let path = dir.path().join(format!("fork-{n}"));
                parent.fork_to(&path, fork_child(parent.cell())).unwrap();
                path
            })
            .collect();
        let (freeing, writing) = (paths.pop().unwrap(), paths.pop().unwrap());
        drop(parent);
        let open = |path: &std::path::PathBuf| {
            let opening =
                crate::open_cell::Opening { arena: None, profiled: false, keeping: idle() };
            OpenCell::open_with(path, opening).unwrap()
        };
        let name = if version { "version" } else { "live" };
        // The heap once the page cache has let go of the files closed, and of the pages it holds:
        // its list of files names them by `Weak`, whose allocations stay until the list is next
        // pruned, as its flusher does every quarter of `pages::Options::expire` and
        // `write_back_all` does at once (TASK-433).
        // And once the bucket's decoded states let go of what they knew of those no one holds
        // (`Decoded::held`, TASK-453).
        let settled = || {
            local_cell::pages::shared().write_back_all().unwrap();
            local_cell::pages::shared().evict_to(0);
            bucket_cell::Bucket::decoded(&*bucket).unwrap().held();
            held()
        };

        let mut stores = Vec::with_capacity(forks);
        let mut samples = Vec::with_capacity(5);
        every_task_shard_locked();
        let (own_before, decoded_before) = extents_counted();
        let before = held();
        let started = std::time::Instant::now();
        for (n, path) in paths.iter().enumerate() {
            stores.push(open(path));
            if [1, 10, 20, 50, 100].contains(&(n + 1)) {
                samples.push((n as u64 + 1, held() - before, started.elapsed()));
            }
        }
        let (own, decoded) = extents_counted();
        let (layers, entries) =
            (decoded.states - decoded_before.states, decoded.entries - decoded_before.entries);
        // What this base's bucket holds decoded, whenever it decoded it.
        let shared = bucket_cell::Bucket::decoded(&*bucket).unwrap().held();
        let (mut last, mut last_held, mut last_took) = (0, 0, Duration::ZERO);
        let first = samples[0].1;
        for &(open, held, took) in &samples {
            let marginal = (held - last_held) / (open - last);
            let each = (took - last_took) / (open - last) as u32;
            println!(
                "fork memory: extents={extents} parent={name} forks={open} held={held} \
                 per-fork={} marginal-per-fork={marginal} marginal-per-fork-per-extent={:.1} \
                 open={each:.2?}",
                held / open,
                marginal as f64 / extents as f64,
            );
            (last, last_held, last_took) = (open, held, took);
        }
        let others = (last_held - first) / (last.max(2) - 1);
        println!(
            "fork memory: extents={extents} parent={name} each-past-the-first={others} \
             first={first} counted: decoded-states={layers} decoded-entries={entries} \
             own-entries={} bucket-decoded={shared:?}",
            own.entries - own_before.entries,
        );
        assert!(others <= IDLE_FORK_BYTES, "{others} bytes a fork past the first, at {extents}");
        // The base's extents, once: in one state, or in a checkpoint of the base's read before and
        // the origin's state made of it, holding what changed since.
        assert_eq!(shared.entries, extents as usize, "the decoded states' entries: {shared:?}");
        assert_eq!(own, own_before, "the entries the forks hold of their own, idle");
        let layer = first.saturating_sub(others);
        let counted = shared.bytes();
        assert!(layer <= counted, "the decoded state shared: {layer} bytes, counted {counted}");

        // A map of them sharing nothing, as the writer's of a base written live is, which a store
        // over it counts as its own.
        let cell = stores[0].cell();
        let state = futures::executor::block_on(async {
            let record = bucket_cell::head::get(&*bucket, cell).await.unwrap().unwrap();
            recover(&*bucket, cell, record.origin()).await.unwrap().state
        });
        let keys: usize = state.extents.iter().map(|(_, at)| at.key.len()).sum();
        let weighed = held();
        let mut rebuilt = bucket_cell::State::new(cell).extents;
        for (extent, at) in &state.extents {
            rebuilt.insert(*extent, at.clone());
        }
        let unshared = held() - weighed;
        drop(rebuilt);
        let bound = state.extents.len() as u64 * bucket_cell::recovery::ENTRY_BYTES;
        assert!(unshared <= bound, "a map of the extents unshared: {unshared} bytes, of {bound}");

        // Closed, opened and closed again.
        stores.clear();
        let closed = settled();
        stores.extend(paths.iter().map(open));
        stores.clear();
        let again = settled();
        println!(
            "fork memory: extents={extents} parent={name} left-after-close={}",
            closed as i64 - before as i64
        );

        // One more writes, commits and uploads, beside an idle one.
        let writes = extents.min(4096);
        let (idle, opened) = (open(&paths[0]), open(&writing));
        let writer = &opened;
        let floor = writer.info().unwrap().next_sequence.max(writer.info().unwrap().sequence_floor);
        let own_before = local_cell::own_held();
        for sequence in floor..floor + writes {
            write(writer, sequence, &data);
            if (sequence - floor + 1) % 1024 == 0 {
                writer.commit(None).unwrap();
            }
        }
        writer.commit(None).unwrap();
        writer.wait_uploaded().unwrap();
        let own_after = local_cell::own_held();
        let own_written = own_after.entries - own_before.entries;
        assert!(idle.shares_extents_with(writer), "its bucket's layer, written over");
        // What its writes hold, once its uploader is done with them: what closing it lets go of,
        // more than closing an idle fork does. Not what the heap holds more meanwhile, which
        // its uploader's work in flight swings by megabytes.
        let closing = |store: OpenCell| {
            let open = settled();
            drop(store);
            open - settled()
        };
        let idle = closing(idle);
        let written = closing(opened).saturating_sub(idle);
        let counted = own_after.bytes() - own_before.bytes();
        println!(
            "fork memory: extents={extents} parent={name} a fork's {writes} writes, uploaded: \
             held={written} per-write={} own-entries={own_written} counted={counted}; a map of \
             the base's unshared={unshared} (keys={keys} nodes={})",
            written / writes,
            unshared - keys as u64,
        );
        assert_eq!(own_written, writes, "the entries its writes hold of its own");
        assert!(written <= counted, "its writes: {written} bytes held, counted {counted}");

        // One more frees every 32nd extent it inherited, beside an idle one, and checkpoints: its
        // writer's map, the base's shared, changed in as many places, each a path of its nodes
        // copied, as is the map its checkpoint decodes as made of the base's, opened again.
        let (idle, opened) = (open(&paths[0]), open(&freeing));
        let (counted_before, _) = extents_counted();
        let frees =
            (1..=extents).step_by(32).map(|sequence| ExtentId { cell: base_cell, sequence });
        let frees: Vec<_> = frees.collect();
        for &extent in &frees {
            opened.free(extent).unwrap();
        }
        opened.commit(None).unwrap();
        crate::test_checkpoints::checkpointed(&opened, &bucket);
        opened.wait_uploaded().unwrap();
        let (counted_after, _) = extents_counted();
        let counted = counted_after.bytes() - counted_before.bytes();
        let idle = closing(idle);
        let freed = closing(opened).saturating_sub(idle);
        println!(
            "fork memory: extents={extents} parent={name} a fork's {} frees, scattered, \
             uploaded: held={freed} per-free={} counted={counted}",
            frees.len(),
            freed / frees.len() as u64,
        );
        // Opened again, from its checkpoint, beside an idle fork holding the base's map: what
        // the heap holds more than an idle fork, its map made of the base's, counted.
        let idle = open(&paths[0]);
        let decoded = || bucket_cell::Bucket::decoded(&*bucket).unwrap().held();
        let (heap_before, decoded_before) = (settled(), decoded());
        let reopened = open(&freeing);
        let (heap_after, decoded_after) = (settled(), decoded());
        let grown = (heap_after - heap_before).saturating_sub(IDLE_FORK_BYTES);
        let more = decoded_after.bytes().saturating_sub(decoded_before.bytes());
        println!(
            "fork memory: extents={extents} parent={name} reopened from its checkpoint of {} \
             frees, scattered: held={} past an idle fork's bound={grown} decoded \
             before={decoded_before:?} after={decoded_after:?} counted={more}",
            frees.len(),
            heap_after - heap_before,
        );
        drop((idle, reopened));
        assert!(freed <= counted, "its frees: {freed} bytes held, counted {counted}");
        assert!(grown <= more, "its map, reopened: {grown} bytes held, counted {more}");
        assert_eq!(again, closed, "the heap after the forks were opened and closed again");
    }

    /// TASK-453: the forks of a base open at once hold what they inherited once: their stores' maps
    /// of it one, the decoded state they started from, which their bucket counts once, whether
    /// forked from the base live, each from a checkpoint of its own the base's writer wrote, or
    /// from a version of it. Each reads the base's data; what one writes and frees, an inherited
    /// extent among them, is its own: the others, and a fork opened after, read the base's, and
    /// share its map still.
    #[test]
    fn forks_open_at_once_share_what_they_inherited() {
        use crate::host_cell::fork_child;
        const EXTENTS: u64 = 64;
        const FORKS: usize = 4;
        for version in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            let base = kept_in(dir.path().join("base"), (1 << 30, None), &bucket, idle()).unwrap();
            for sequence in 1..=EXTENTS {
                write(&base, sequence, &pattern(sequence as u8, BLOCK));
            }
            base.commit(None).unwrap();
            let base_cell = base.cell().id;
            let inherited = |sequence| ExtentId { cell: base_cell, sequence };
            let parent = match version {
                false => base,
                true => {
                    let path = dir.path().join("copy");
                    base.uploaded_version_at(&path).unwrap();
                    reopened(&path, idle()).unwrap()
                }
            };
            let fork = |n: usize| {
                let path = dir.path().join(format!("fork-{n}"));
                parent.fork_to(&path, fork_child(parent.cell())).unwrap();
                path
            };
            let paths: Vec<_> = (0..=FORKS).map(fork).collect();
            let forks: Vec<_> = paths[..FORKS].iter().map(|path| reopened(path, idle())).collect();
            let forks: Vec<_> = forks.into_iter().map(Result::unwrap).collect();
            let name = if version { "of a version" } else { "of the live store" };
            for other in &forks[1..] {
                assert!(forks[0].shares_extents_with(other), "forks {name}: their maps");
            }
            let one = bucket_cell::recovery::Held { states: 1, entries: EXTENTS as usize, runs: 0 };
            let decoded = || bucket_cell::Bucket::decoded(&*bucket).unwrap().held();
            assert_eq!(decoded(), one, "forks {name}: the states they share, decoded");
            let read = |store: &OpenCell, extent: ExtentId| {
                let mut data = vec![0; BLOCK];
                let read =
                    store.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]);
                read.map(|()| data).map_err(|err| err.to_string())
            };
            for store in &forks {
                for sequence in 1..=EXTENTS {
                    let data = pattern(sequence as u8, BLOCK);
                    assert_eq!(read(store, inherited(sequence)), Ok(data), "forks {name}");
                }
            }

            // The first frees an inherited extent, and writes and commits one of its own.
            let (freed, own) = (inherited(7), {
                let info = forks[0].info().unwrap();
                info.next_sequence.max(info.sequence_floor)
            });
            forks[0].free(freed).unwrap();
            let mine = write(&forks[0], own, &pattern(200, BLOCK));
            forks[0].commit(None).unwrap();
            // What a read of an extent a store holds not answers (`NoExtent`, as errno).
            let none = || Err("ENOENT (errno 2)".to_owned());
            assert_eq!(read(&forks[0], freed), none());
            assert_eq!(read(&forks[0], mine), Ok(pattern(200, BLOCK)));
            let opened = reopened(&paths[FORKS], idle()).unwrap();
            for store in forks[1..].iter().chain([&opened]) {
                assert_eq!(read(store, freed), Ok(pattern(7, BLOCK)), "forks {name}: the freed");
                assert_eq!(read(store, mine), none(), "forks {name}: the first's own");
                assert!(forks[0].shares_extents_with(store), "forks {name}: their maps, after");
            }
            assert_eq!(decoded(), one, "forks {name}: the states they share, after");
        }
    }

    /// TASK-453: what a read asks of a fork's store of an extent it inherited, its length and
    /// whether it is open, and a read of it from the extent cache, there once read, allocate
    /// nothing: the store's map lends its bucket's layer's entry, where it made one of its own of
    /// it, its key copied, for every read, under the store's lock. A base of more extents than
    /// the extent store's audit recounts (`AUDITED_EXTENTS`, 4,096), which the builds of its own
    /// tests run at every lock of a store's state, as they build it for this crate's tests too
    /// when run with them: no build but a test's audits.
    #[test]
    fn reads_of_inherited_extents_allocate_nothing() {
        use crate::heap::allocations_while;
        use crate::host_cell::fork_child;
        const EXTENTS: u64 = 4160;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let capacity = 2 * EXTENTS * BLOCK as u64 + (1 << 30);
        let base = kept_in(dir.path().join("base"), (capacity, None), &bucket, idle()).unwrap();
        for sequence in 1..=EXTENTS {
            write(&base, sequence, &pattern(sequence as u8, BLOCK));
            if sequence % 1024 == 0 {
                base.commit(None).unwrap();
            }
        }
        base.commit(None).unwrap();
        let base_cell = base.cell().id;
        let inherited = |sequence| ExtentId { cell: base_cell, sequence };
        let path = dir.path().join("fork");
        base.fork_to(&path, fork_child(base.cell())).unwrap();
        let fork = reopened(&path, idle()).unwrap();
        let held = Ok(Some((BLOCK as u64, false)));
        let mut data = vec![0; BLOCK];
        // Each read once: into the cache.
        for sequence in 1..=EXTENTS {
            assert_eq!(fork.stored_extent(inherited(sequence)), held);
            assert_eq!(fork.read_from_cache(inherited(sequence), 0, &mut data), Ok(true));
            assert_eq!(data, pattern(sequence as u8, BLOCK), "{sequence}");
        }
        let (asked, ()) = allocations_while(|| {
            for sequence in 1..=EXTENTS {
                assert_eq!(fork.stored_extent(inherited(sequence)), held);
            }
        });
        assert_eq!(asked, 0, "allocations of {EXTENTS} asks of the store");
        let (read, ()) = allocations_while(|| {
            for sequence in 1..=EXTENTS {
                assert_eq!(fork.read_from_cache(inherited(sequence), 0, &mut data), Ok(true));
            }
        });
        assert_eq!(read, 0, "allocations of {EXTENTS} reads from the cache");
    }

    /// TASK-453: a benchmark of a read's hit in the extent cache of an extent a fork inherited
    /// (`OpenCell::read_cached`): a base of `CLADEFS_BENCH_EXTENTS` extents (32,768 unless set)
    /// of a block each, forked, each of them read once into the cache, then all of them read
    /// `CLADEFS_BENCH_ROUNDS` times (20 unless set), a block each: the time a read, and the
    /// allocations. In the sim profile: `cargo nextest run -p cladefs --cargo-profile sim
    /// --run-ignored only bench_inherited_cached_reads --no-capture`.
    #[test]
    #[ignore = "a benchmark (TASK-453): --run-ignored only, in the sim profile"]
    fn bench_inherited_cached_reads() {
        use crate::host_cell::fork_child;
        let count = |name: &str, default: u64| -> u64 {
            std::env::var(name).map_or(default, |count| count.parse().expect("a count"))
        };
        let (extents, rounds) =
            (count("CLADEFS_BENCH_EXTENTS", 32768), count("CLADEFS_BENCH_ROUNDS", 20));
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let capacity = 2 * extents * BLOCK as u64 + (1 << 30);
        let base = kept_in(dir.path().join("base"), (capacity, None), &bucket, idle()).unwrap();
        let data = pattern(1, BLOCK);
        for sequence in 1..=extents {
            write(&base, sequence, &data);
            if sequence % 1024 == 0 {
                base.commit(None).unwrap();
            }
        }
        base.commit(None).unwrap();
        let base_cell = base.cell().id;
        let inherited = |sequence| ExtentId { cell: base_cell, sequence };
        let path = dir.path().join("fork");
        base.fork_to(&path, fork_child(base.cell())).unwrap();
        let fork = reopened(&path, idle()).unwrap();
        let mut read = vec![0; BLOCK];
        for sequence in 1..=extents {
            assert_eq!(fork.read_from_cache(inherited(sequence), 0, &mut read), Ok(true));
        }
        assert_eq!(read, data);
        let started = std::time::Instant::now();
        let (allocations, ()) = crate::heap::allocations_while(|| {
            for _ in 0..rounds {
                for sequence in 1..=extents {
                    let hit = fork.read_from_cache(inherited(sequence), 0, &mut read);
                    assert_eq!(hit, Ok(true));
                }
            }
        });
        let took = started.elapsed();
        let reads = extents * rounds;
        println!(
            "inherited cached reads: extents={extents} reads={reads} took={took:.2?} \
             per-read={:.0}ns allocations-per-read={:.2}",
            took.as_nanos() as f64 / reads as f64,
            allocations as f64 / reads as f64,
        );
    }

    /// Has the bucket runtime (`local_cell::upload::bucket_runtime`) lock each shard of its list of
    /// tasks once, so that none allocates after: on macOS, a `std::sync::Mutex` allocates its
    /// pthread mutex, 64 bytes, as it is first locked, and the runtime locks the shard of a task's
    /// id, numbered across the process, as it spawns the task and as it ends. Its shards are
    /// tokio's: four for each worker, rounded up to a power of two
    /// (`OwnedTasks::gen_shared_list_size`, tokio 1.53). Opening and closing forks spawns
    /// uploaders, so a heap counted to the byte grew by 64 bytes now and then as they reached
    /// shards none had (32 at most on this runtime): nothing of the stores'. The other such holder,
    /// each of its threads' list of the shared locks it holds, it makes as each starts
    /// (`local_cell::shared::prepare_thread`).
    fn every_task_shard_locked() {
        let runtime = local_cell::upload::bucket_runtime().unwrap();
        let shards = runtime.metrics().num_workers().next_power_of_two() * 4;
        let mut locked = std::collections::HashSet::new();
        while locked.len() < shards {
            let task = runtime.spawn(async {});
            let id: u64 = task.id().to_string().parse().expect("a task's id is its number");
            locked.insert(id % shards as u64);
            futures::executor::block_on(task).unwrap();
        }
    }

    /// A store's closed segments are the objects of its cell in the bucket, byte for byte, as
    /// are its extents stored on their own (doc-7, decision 1): the uploader puts its files; and
    /// once the bucket has acknowledged them, they leave the store whole, into the host's cache,
    /// as those objects, which the store's reads read there with the bucket's gone (TASK-165.8).
    #[test]
    fn uploaded_segments_are_the_stores_files() {
        use bucket_cell::layout::{self, Footer};
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(memory.clone(), ""));
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), 1 << 30, 0).unwrap());
        let options = local_cell::upload::Options { cache: Some(cache.clone()), ..idle() };
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), options).unwrap();
        let cell = store.cell();
        write(&store, 1, &pattern(1, BLOCK));
        write(&store, 2, &pattern(2, 4 << 20));
        store.commit(Some(b"one")).unwrap();
        write(&store, 3, &pattern(3, 2 * BLOCK));
        store.commit(Some(b"two")).unwrap();

        // The store's files of `kind`, by their contents.
        let files = |kind: &str| -> Vec<Vec<u8>> {
            let path =
                dir.path().join("store").join(kind).join(base32_id::Padded(cell.id).to_string());
            let mut files: Vec<_> = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
                .collect();
            files.sort();
            files
        };
        // The bucket's objects under `prefix`.
        let objects = |prefix: String| -> Vec<Vec<u8>> {
            futures::executor::block_on(async {
                let listed = bucket_cell::Bucket::list(&*bucket, &prefix).await.unwrap();
                let mut objects = Vec::new();
                for meta in listed {
                    let object = bucket_cell::Bucket::get(&*bucket, &meta.key).await.unwrap();
                    objects.push(object.bytes);
                }
                objects
            })
        };
        // The cell's segments but its writer's fence, which holds no commit.
        let mut segments: Vec<_> = objects(layout::segments(cell))
            .into_iter()
            .filter(|bytes| !layout::footer::<Footer>(bytes).unwrap().contents.commits.is_empty())
            .collect();
        segments.sort();
        assert_eq!(segments.len(), 2);
        let mut own = objects(layout::extents(cell));
        own.sort();
        assert_eq!(own.len(), 1);
        assert_eq!((files("segments"), files("extents")), (Vec::new(), Vec::new()));
        assert_eq!(
            uploaded(&bucket, cell),
            (
                vec![(1, pattern(1, BLOCK)), (2, pattern(2, 4 << 20)), (3, pattern(3, 2 * BLOCK))],
                Some(b"two".to_vec())
            )
        );
        futures::executor::block_on(async {
            use futures::StreamExt as _;
            use object_store::{ObjectStore as _, ObjectStoreExt as _};
            for prefix in ["segments", "extents"] {
                let prefix = object_store::path::Path::from(prefix);
                for meta in memory.list(Some(&prefix)).collect::<Vec<_>>().await {
                    memory.delete(&meta.unwrap().location).await.unwrap();
                }
            }
        });
        let written =
            [(1, pattern(1, BLOCK)), (2, pattern(2, 4 << 20)), (3, pattern(3, 2 * BLOCK))];
        for (sequence, data) in written {
            let extent = ExtentId { cell: cell.id, sequence };
            let mut read = vec![0; data.len()];
            store.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])]).unwrap();
            assert!(read == data, "extent {sequence} reads other data");
        }
    }

    /// cladefs: a byte the host's disk damaged in a file the uploader puts as it is, a sealed
    /// extent's own, fails the upload, and the commit waiting for it, rather than going to object
    /// storage under the CRC it fails (TASK-153.11).
    #[test]
    fn files_with_flipped_bytes_are_not_uploaded() {
        use bucket_cell::layout;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = store.cell();
        // Stored on its own, raw, as it doesn't compress.
        let extent = write(&store, 1, &noise(1, 4 << 20));
        store.commit_ordered(None).unwrap();
        // On the disk, as the store's page cache holds it written (TASK-433).
        local_cell::pages::shared().write_back_all().unwrap();
        let path = dir.path().join(format!(
            "store/extents/{}/{}",
            base32_id::Padded(cell.id),
            base32_id::Padded(extent.sequence)
        ));
        let mut bytes = std::fs::read(&path).unwrap();
        let stored = extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap().stored[1];
        assert_eq!(stored.encoding, extent_frame::Encoding::Raw);
        bytes[stored.offset as usize + 7] ^= 0x10;
        std::fs::write(&path, bytes).unwrap();
        // Damaged behind the store's back: what the store's page cache holds of it, clean, goes,
        // so that its reads read the disk (TASK-433).
        local_cell::pages::shared().evict_to(0);
        assert_eq!(store.commit(None), Err(crate::error::Errno::EIO));
        let listed = |prefix: String| -> Vec<String> {
            let listed = bucket_cell::Bucket::list(&*bucket, &prefix);
            futures::executor::block_on(listed).unwrap().into_iter().map(|meta| meta.key).collect()
        };
        assert_eq!(listed(layout::extents(cell)), Vec::<String>::new());
    }

    /// A commit waits for its upload no longer than it takes: once another writer has the cell,
    /// a durable commit fails.
    #[test]
    fn durable_commits_fail_once_another_takes_the_cell() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        write(&store, 1, &pattern(1, BLOCK));
        store.commit(None).unwrap();
        let other = bucket_cell::Writer::open(
            bucket.clone(),
            store.cell(),
            2,
            crate::host_cell::token().unwrap(),
        );
        futures::executor::block_on(other).unwrap();
        write(&store, 2, &pattern(2, BLOCK));
        assert_eq!(store.commit(None), Err(crate::Errno::EROFS));
    }

    /// TASK-39.7: a store's copy takes its cell with its placement's fencing token: a later
    /// placement's, higher, fences it, its commits failing, and it takes the cell back no more,
    /// though its store is as current as the cell; a placement later still takes it.
    #[test]
    fn later_placements_fence_earlier_ones() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let (first_path, second_path) = (dir.path().join("first"), dir.path().join("second"));
        let first = kept_in(&first_path, (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = first.cell();
        OpenCell::place(&first_path, 1).unwrap();
        write(&first, 1, &pattern(1, BLOCK));
        first.commit(Some(b"first")).unwrap();

        // The cell moves to another host, a later placement, while the first still runs.
        drop(kept_in(&second_path, (1 << 30, Some(cell)), &bucket, idle()).unwrap());
        OpenCell::place(&second_path, 2).unwrap();
        let second = reopened(&second_path, idle()).unwrap();
        assert_eq!(second.root(), Ok(b"first".to_vec()));
        write(&first, 2, &pattern(2, BLOCK));
        assert_eq!(first.commit(None), Err(crate::Errno::EROFS));
        drop((first, second));

        // The first, restarted, takes it back no more: its token is the earlier.
        let refused = reopened(&first_path, idle()).err().and_then(|err| err.errno());
        assert_eq!(refused, Some(crate::Errno::ESTALE));
        let head = futures::executor::block_on(bucket_cell::head::get(&*bucket, cell));
        assert_eq!(head.unwrap().unwrap().token, 2);

        // Placed later still, it does.
        OpenCell::place(&first_path, 3).unwrap();
        let later = reopened(&first_path, idle()).unwrap();
        assert_eq!(later.root(), Ok(b"first".to_vec()));
    }

    /// TASK-45.1: a placement cut off deletes no cell from under a later one: its delete fails
    /// (`ESTALE`), leaving the cell in the bucket, which the later placement goes on writing; the
    /// later placement's delete deletes it.
    #[test]
    fn earlier_placements_delete_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let (first_path, second_path) = (dir.path().join("first"), dir.path().join("second"));
        let first = kept_in(&first_path, (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = first.cell();
        OpenCell::place(&first_path, 1).unwrap();
        write(&first, 1, &pattern(1, BLOCK));
        first.commit(Some(b"first")).unwrap();
        drop(first);
        // The cell moves to another host, a later placement.
        drop(kept_in(&second_path, (1 << 30, Some(cell)), &bucket, idle()).unwrap());
        OpenCell::place(&second_path, 2).unwrap();
        let second = reopened(&second_path, idle()).unwrap();

        let refused = OpenCell::delete(&first_path, Some(&bucket));
        assert_eq!(refused.err().and_then(|err| err.errno()), Some(crate::Errno::ESTALE));
        write(&second, 2, &pattern(2, BLOCK));
        second.commit(Some(b"second")).unwrap();
        drop(second);
        let state =
            futures::executor::block_on(bucket_cell::recover(&*bucket, cell, None)).unwrap().state;
        assert_eq!(state.root, Some(b"second".to_vec()));

        OpenCell::delete(&second_path, Some(&bucket)).unwrap();
        let deleted = futures::executor::block_on(bucket_cell::head::get(&*bucket, cell));
        assert!(deleted.unwrap().unwrap().deleted(), "the cell lives");
    }

    /// TASK-39.7: a store's copy never placed takes its cell with its host's token, as a store
    /// restored or taken on a host is.
    #[test]
    fn unplaced_copies_take_their_hosts_token() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let options = local_cell::upload::Options { token: 9, ..idle() };
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), options).unwrap();
        let head = futures::executor::block_on(bucket_cell::head::get(&*bucket, store.cell()));
        let head = head.unwrap().unwrap();
        assert_eq!((head.token, head.epoch), (9, 1));
    }

    /// TASK-165.9: a store whose bucket acknowledged commits of its cell, opened where the
    /// bucket has no such cell, deleted or another bucket, refuses (`ENOENT`), rather than make
    /// the cell anew without them.
    #[test]
    fn stores_whose_bucket_lacks_their_cell_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let path = dir.path().join("store");
        let store = kept_in(&path, (1 << 30, None), &bucket, idle()).unwrap();
        write(&store, 1, &pattern(1, BLOCK));
        store.commit(Some(b"acknowledged")).unwrap();
        drop(store);
        let other = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        naming(&path, &other);
        let refused = reopened(&path, idle()).err().and_then(|err| err.errno());
        assert_eq!(refused, Some(crate::Errno::ENOENT));
        // Its own bucket named again, it opens as it was.
        naming(&path, &bucket);
        assert_eq!(reopened(&path, idle()).unwrap().root(), Ok(b"acknowledged".to_vec()));
    }

    /// TASK-192: a store dropped uploads what it committed, its open segment's commits too, as
    /// `local_cell::upload`'s docs say: its uploader closes the open segment, uploads it and
    /// stops, before the store closes. Before, the store closed first, and its uploader found the
    /// open segment's commits it could no longer close.
    #[test]
    fn dropped_stores_upload_their_open_segments_commits() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = store.cell();
        write(&store, 1, &pattern(1, BLOCK));
        store.commit_ordered(Some(b"ordered")).unwrap();
        // Not uploaded yet: the uploader waits an hour, unless asked.
        assert_eq!(uploaded(&bucket, cell), (vec![], None));
        drop(store);
        assert_eq!(
            uploaded(&bucket, cell),
            (vec![(1, pattern(1, BLOCK))], Some(b"ordered".to_vec()))
        );
    }

    /// TASK-39.7: waiting for uploads returns once every commit so far is in the cell.
    #[test]
    fn waits_for_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        write(&store, 1, &pattern(1, BLOCK));
        store.commit_ordered(Some(b"ordered")).unwrap();
        // Not uploaded yet: the uploader waits an hour, unless asked.
        assert_eq!(uploaded(&bucket, store.cell()).1, None);
        store.wait_uploaded().unwrap();
        assert_eq!(
            uploaded(&bucket, store.cell()),
            (vec![(1, pattern(1, BLOCK))], Some(b"ordered".to_vec()))
        );
    }

    /// Which writes a [`Gated`] store holds.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Hold {
        Nothing,
        /// The writes of segments.
        Segments,
        /// The writes of segments of this number or more.
        SegmentsFrom(u64),
        /// The write of the object at this key.
        Key(String),
        /// The first write of the object at this key, those after it while it waits going on: a
        /// takeover's of a cell's head, its writer's swaps of it meanwhile not (TASK-407).
        First(String),
    }

    impl Hold {
        fn holds(&self, key: &str) -> bool {
            use bucket_cell::layout::{Kind, kind};
            match self {
                Hold::Nothing => false,
                Hold::Segments => {
                    matches!(
                        bucket_cell::layout::kind(key),
                        bucket_cell::layout::Kind::Segment { .. }
                    )
                }
                Hold::SegmentsFrom(from) => {
                    matches!(kind(key), Kind::Segment { segment, .. } if segment >= *from)
                }
                Hold::Key(held) | Hold::First(held) => held == key,
            }
        }
    }

    /// What goes wrong with a write through a [`Gated`] store, as it fails: requests that fail, the
    /// host's bucket's helpers send again.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Mishap {
        /// It takes no effect.
        Refused,
        /// It takes effect, its answer lost.
        Unanswered,
        /// The host dies as it goes, and it takes no effect.
        Dies,
        /// It takes effect, and the host dies before its answer comes.
        DiesAnswered,
        /// The host dies as it goes, and it lands later ([`Gated::land_late`]), as a dead host's
        /// PUT may.
        DiesLandingLate,
        /// A pass of the collector, which what the host's dying does stands for here, runs to its
        /// end just before it, which goes as it would have (TASK-150.1).
        Collected,
    }

    /// A [`Mishap`] set to happen to a write, by its count ([`Gated::arm`]), and what a host's
    /// dying does.
    struct Armed {
        write: usize,
        mishap: Mishap,
        die: Arc<dyn Fn() + Send + Sync>,
    }

    impl std::fmt::Debug for Armed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?} at write {}", self.mishap, self.write)
        }
    }

    /// An object store whose requests fail while it is shut, whose writes fail while it is full,
    /// and whose writes wait while they are held, go wrong as it is armed to, or reach a copy of
    /// its objects once it is severed.
    #[derive(Debug)]
    pub(super) struct Gated {
        store: Arc<dyn object_store::ObjectStore>,
        shut: std::sync::atomic::AtomicBool,
        /// Its writes fail as a file bucket's on a full disk do, `ENOSPC` behind a generic error
        /// (TASK-314).
        full: std::sync::atomic::AtomicBool,
        hold: tokio::sync::watch::Sender<Hold>,
        /// The writes waiting.
        held: std::sync::atomic::AtomicUsize,
        /// The objects it reaches, once severed ([`Gated::sever`]).
        severed: Mutex<Option<Arc<dyn object_store::ObjectStore>>>,
        /// The writes that went since it was armed.
        writes: std::sync::atomic::AtomicUsize,
        armed: Mutex<Option<Armed>>,
        /// The writes that land later ([`Mishap::DiesLandingLate`]).
        late: Mutex<Vec<LateWrite>>,
    }

    /// A write that lands later: where, what, and how.
    type LateWrite = (object_store::path::Path, object_store::PutPayload, object_store::PutOptions);

    impl std::fmt::Display for Gated {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "gated")
        }
    }

    impl Gated {
        fn new() -> Self {
            Self::over(Arc::new(InMemory::new()))
        }

        /// The objects of `store`, gated.
        pub(super) fn over(store: Arc<dyn object_store::ObjectStore>) -> Self {
            Self {
                store,
                shut: Default::default(),
                full: Default::default(),
                hold: tokio::sync::watch::channel(Hold::Nothing).0,
                held: Default::default(),
                severed: Default::default(),
                writes: Default::default(),
                armed: Default::default(),
                late: Default::default(),
            }
        }

        /// The objects its requests reach.
        fn reached(&self) -> Arc<dyn object_store::ObjectStore> {
            self.severed.lock().unwrap().clone().unwrap_or_else(|| self.store.clone())
        }

        /// Cuts it off from its objects, as a host dead is: its requests reach a copy of them from
        /// now on, which nothing else sees, so that what the host had going runs to its end, doing
        /// nothing a dead host would not, rather than wait for ever.
        pub(super) fn sever(&self) {
            use object_store::{ObjectStore as _, ObjectStoreExt as _};
            let copy = InMemory::new();
            futures::executor::block_on(async {
                use futures::StreamExt as _;
                for meta in self.store.list(None).collect::<Vec<_>>().await {
                    let location = meta.unwrap().location;
                    let bytes = self.store.get(&location).await.unwrap().bytes().await.unwrap();
                    copy.put(&location, bytes.into()).await.unwrap();
                }
            });
            *self.severed.lock().unwrap() = Some(Arc::new(copy));
        }

        /// Has `mishap` happen to the write numbered `write`, counting from 0 from now on, `die`
        /// being what the host's dying does.
        pub(super) fn arm(&self, write: usize, mishap: Mishap, die: Arc<dyn Fn() + Send + Sync>) {
            self.writes.store(0, std::sync::atomic::Ordering::Relaxed);
            *self.armed.lock().unwrap() = Some(Armed { write, mishap, die });
        }

        /// How many writes went since it was armed.
        pub(super) fn writes(&self) -> usize {
            self.writes.load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Lands the writes a dead host's PUTs left, whatever they find, at the objects it was cut
        /// off from.
        pub(super) fn land_late(&self) {
            for (location, payload, options) in std::mem::take(&mut *self.late.lock().unwrap()) {
                let landed = self.store.put_opts(&location, payload, options);
                if let Err(err) = futures::executor::block_on(landed) {
                    eprintln!("{location} landing late: {err}");
                }
            }
        }

        /// What happens to the write of `payload` to `location`, armed with `armed`.
        async fn mishap(
            &self,
            armed: Armed,
            location: &object_store::path::Path,
            payload: object_store::PutPayload,
            options: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            match armed.mishap {
                Mishap::Collected => {
                    let collecting = std::thread::spawn(move || (armed.die)());
                    collecting.join().unwrap();
                    return self.reached().put_opts(location, payload, options).await;
                }
                Mishap::Refused => {}
                Mishap::Unanswered => {
                    self.reached().put_opts(location, payload, options).await?;
                }
                Mishap::Dies => (armed.die)(),
                Mishap::DiesAnswered => {
                    self.reached().put_opts(location, payload, options).await?;
                    (armed.die)();
                }
                Mishap::DiesLandingLate => {
                    (armed.die)();
                    self.late.lock().unwrap().push((location.clone(), payload, options));
                }
            }
            Err(object_store::Error::Generic { store: "gated", source: "a mishap".into() })
        }

        fn check(&self) -> object_store::Result<()> {
            if self.shut.load(std::sync::atomic::Ordering::Relaxed) {
                Err(object_store::Error::Generic { store: "gated", source: "shut".into() })
            } else {
                Ok(())
            }
        }

        /// Whether it has room for a write: not while full.
        fn room(&self) -> object_store::Result<()> {
            match self.full.load(std::sync::atomic::Ordering::Relaxed) {
                true => Err(object_store::Error::Generic {
                    store: "gated",
                    source: Box::new(std::io::Error::from_raw_os_error(libc::ENOSPC)),
                }),
                false => Ok(()),
            }
        }

        /// Waits until `count` writes wait, as they come in other threads, and fails if they
        /// don't within 30 s.
        fn wait_held(&self, count: usize) {
            Self::wait_for(&self.held, count);
        }

        /// Waits until `counter` counts `count`, as it counts up in other threads, and fails if it
        /// doesn't within 30 s.
        fn wait_for(counter: &std::sync::atomic::AtomicUsize, count: usize) {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let counted = || counter.load(std::sync::atomic::Ordering::Relaxed);
            while counted() < count && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert_eq!(counted(), count);
        }
    }

    #[async_trait::async_trait]
    impl object_store::ObjectStore for Gated {
        async fn put_opts(
            &self,
            location: &object_store::path::Path,
            payload: object_store::PutPayload,
            opts: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.check()?;
            self.room()?;
            let write = self.writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let armed = self.armed.lock().unwrap().take_if(|armed| armed.write == write);
            if let Some(armed) = armed {
                return self.mishap(armed, location, payload, opts).await;
            }
            let key = location.as_ref();
            let after_first = matches!(&*self.hold.borrow(), Hold::First(_))
                && self.held.load(std::sync::atomic::Ordering::Relaxed) > 0;
            if !self.hold.borrow().holds(key) || after_first {
                return self.reached().put_opts(location, payload, opts).await;
            }
            let mut hold = self.hold.subscribe();
            self.held.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = hold.wait_for(|hold| !hold.holds(key)).await;
            self.held.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            self.reached().put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &object_store::path::Path,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.check()?;
            self.room()?;
            self.reached().put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &object_store::path::Path,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            self.check()?;
            self.reached().get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<
                'static,
                object_store::Result<object_store::path::Path>,
            >,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
        {
            self.reached().delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.reached().list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.reached().list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &object_store::path::Path,
            to: &object_store::path::Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.reached().copy_opts(from, to, options).await
        }
    }

    /// While object storage is out of reach, or `failing` otherwise, commits go on until the lag
    /// reaches its bound, as `options` set it, two commits of a block each, then wait, failing
    /// nothing, the bucket's failures counted (TASK-314): the third, of two blocks; they go on once
    /// it answers again, and every commit is uploaded.
    fn waits_at_the_lag_bound(
        options: local_cell::upload::Options,
        failing: fn(&Gated) -> &std::sync::atomic::AtomicBool,
    ) {
        use bucket_cell::object_bucket::Request;
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let store = Arc::new(
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), options).unwrap(),
        );
        let creates_failed = bucket.stats().failures(Request::Create);
        failing(&gated).store(true, std::sync::atomic::Ordering::Relaxed);
        for sequence in 1..=2 {
            write(&store, sequence, &pattern(sequence as u8, BLOCK));
            store.commit_ordered(None).unwrap();
        }
        let third = {
            let store = store.clone();
            std::thread::spawn(move || {
                write(&store, 3, &pattern(3, 2 * BLOCK));
                store.commit_ordered(None)
            })
        };
        let uploader = store.uploader().unwrap();
        while uploader.stalls() == 0 || bucket.stats().failures(Request::Create) == creates_failed {
            std::thread::yield_now();
        }
        assert!(!third.is_finished());
        assert_eq!(uploader.failed(), None, "the uploader stopped");
        failing(&gated).store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(third.join().unwrap(), Ok(()));
        store.commit(None).unwrap();
        let (extents, _) = uploaded(&bucket, store.cell());
        assert_eq!(extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(), [1, 2, 3]);
    }

    /// TASK-317: what a store's uploads hold in memory while the bucket takes none of them, however
    /// far behind they fall, is their budget, the host's upload memory: segments of incompressible
    /// data, a MiB an extent, each closed by a commit waiting for its upload, twice as many as the
    /// store uploads at once, which the bucket holds unanswered. What waits is in the store's
    /// write-ahead log, on disk.
    #[test]
    fn uploads_a_bucket_takes_none_of_hold_no_more_than_their_budget() {
        // Seven segments' room, of 32 MiB and their tails each: more than a framing holds.
        const BUDGET: u64 = 256 << 20;
        // What a store's framing holds besides, one segment at a time: its data, and its framed
        // bytes, twice as they grow (`local_cell::segments`, TASK-325).
        const FRAMING: u64 = 4 * (32 << 20);
        // And the heap's own: the store's and the bucket's, and a commit's buffers.
        const SLACK: u64 = 16 << 20;
        super::set_upload_memory(BUDGET).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let options =
            local_cell::upload::Options { max_lag: u64::MAX, max_lag_bytes: u64::MAX, ..idle() };
        let store = Arc::new(
            kept_in(dir.path().join("store"), (8 << 30, None), &bucket.clone(), options).unwrap(),
        );
        // The writer's fence is in; the commits' segments are held.
        gated.hold.send_replace(Hold::Segments);
        let segments = 2 * local_cell::upload::IN_FLIGHT;
        let mut waiting = Vec::new();
        let data = noise(1, 1 << 20);
        let heap = crate::heap::peak_while(|| {
            let mut sequence = 0;
            for _ in 0..segments {
                for _ in 0..32 {
                    sequence += 1;
                    write(&store, sequence, &data);
                    store.commit_ordered(None).unwrap();
                }
                let committing = store.clone();
                waiting.push(std::thread::spawn(move || committing.commit(None)));
            }
            // As many as the budget lets go at once: seven at least, or more of segments closed
            // short, as commits waiting close them.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let held = || gated.held.load(std::sync::atomic::Ordering::Relaxed);
            while held() < 7 && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(held() >= 7, "{} uploads held", held());
        });
        let memory = super::upload_memory();
        let behind = store.uploader().unwrap().status().unwrap().pending_bytes;
        gated.hold.send_replace(Hold::Nothing);
        for commit in waiting {
            assert_eq!(commit.join().unwrap(), Ok(()));
        }
        eprintln!("heap {} MiB, {memory:?}, {} MiB behind", heap >> 20, behind >> 20);
        assert!(behind >= (segments as u64) << 25, "{behind} bytes behind");
        assert_eq!(memory.budget, BUDGET);
        assert!(memory.max_held <= BUDGET, "{memory:?}");
        // cladefs: and the stores' page cache's, which a backlog holds no more of than its bound
        // (TASK-433): the backlog is on the disk.
        let pages = local_cell::pages::shared().options().most;
        assert!(heap <= BUDGET + FRAMING + SLACK + pages, "{} MiB held", heap >> 20);
    }

    /// Segments upload at once: a commit made while those before it upload goes up with them.
    #[test]
    fn segments_upload_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let store = Arc::new(
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap(),
        );
        gated.hold.send_replace(Hold::Segments);
        let commits: Vec<_> = (1..=3)
            .map(|sequence| {
                let committing = store.clone();
                let commit = std::thread::spawn(move || {
                    write(&committing, sequence, &pattern(sequence as u8, BLOCK));
                    committing.commit(None)
                });
                gated.wait_held(sequence as usize);
                commit
            })
            .collect();
        gated.hold.send_replace(Hold::Nothing);
        for commit in commits {
            assert_eq!(commit.join().unwrap(), Ok(()));
        }
        let (extents, _) = uploaded(&bucket, store.cell());
        assert_eq!(extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(), [1, 2, 3]);
    }

    /// TASK-92.15: an ordered commit that no one waits for stays in the store's open segment while
    /// the uploader takes the segments that commits waiting for their uploads closed: it closes
    /// the open segment for those waiting, or when it is due, not each time an upload is done.
    /// Here four segments, one for each commit waiting, upload, held in the bucket; a commit is
    /// made that no one waits for; then they land, one after another: the store closes none for
    /// it, which uploads once one waits for it.
    #[test]
    fn uploads_done_close_no_segment_for_commits_no_one_waits_for() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let store = Arc::new(
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap(),
        );
        let cell = store.cell();
        // The commits' segments are 0 to 3.
        gated.hold.send_replace(Hold::Segments);
        let waiting: Vec<_> = (1..=local_cell::upload::IN_FLIGHT as u64)
            .map(|sequence| {
                let committing = store.clone();
                let commit = std::thread::spawn(move || {
                    write(&committing, sequence, &pattern(sequence as u8, BLOCK));
                    committing.commit(None)
                });
                gated.wait_held(sequence as usize);
                commit
            })
            .collect();
        let ordered = local_cell::upload::IN_FLIGHT as u64 + 1;
        write(&store, ordered, &pattern(ordered as u8, BLOCK));
        store.commit_ordered(None).unwrap();
        // The first lands alone: its commit is acknowledged; then the rest.
        let mut waiting = waiting.into_iter();
        gated.hold.send_replace(Hold::SegmentsFrom(1));
        assert_eq!(waiting.next().unwrap().join().unwrap(), Ok(()));
        gated.hold.send_replace(Hold::Nothing);
        for commit in waiting {
            assert_eq!(commit.join().unwrap(), Ok(()));
        }
        // The store's segments closed, in the bucket: those of the commits waiting.
        let prefix = bucket_cell::layout::segments(cell);
        let closed = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, &prefix));
        let closed = closed.unwrap();
        assert_eq!(closed.len(), local_cell::upload::IN_FLIGHT);
        store.wait_uploaded().unwrap();
        let (extents, _) = uploaded(&bucket, cell);
        let sequences: Vec<_> = extents.iter().map(|(sequence, _)| *sequence).collect();
        assert_eq!(sequences, (1..=ordered).collect::<Vec<_>>());
    }

    /// A cell survives the loss of its host. Cut off from object storage, the host's store goes
    /// on committing, ordered, until the lag reaches its bound, `max_lag` commits, then waits.
    /// Lost then, its store, its uploader and its disk, its commits made durable, which waited for
    /// their uploads, are all in the cell, which another host's empty store takes, losing the
    /// lag's commits at most; and writes on. In the objects of `store` under `prefix`.
    fn survives_its_hosts_loss(store: Arc<dyn object_store::ObjectStore>, prefix: &str) {
        use bucket_cell::layout;
        use std::sync::atomic::Ordering::Relaxed;
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::over(store.clone()));
        let max_lag = 2;
        let options = local_cell::upload::Options {
            interval: Duration::from_millis(1),
            max_lag,
            ..Default::default()
        };
        let lost = Arc::new(
            kept_in(
                dir.path().join("lost"),
                (1 << 30, None),
                &Arc::new(ObjectBucket::new(gated.clone(), prefix)),
                options,
            )
            .unwrap(),
        );
        let cell = lost.cell();
        write(&lost, 1, &pattern(1, BLOCK));
        lost.commit(Some(b"durable")).unwrap();
        gated.shut.store(true, Relaxed);
        for sequence in 2..=1 + max_lag {
            write(&lost, sequence, &pattern(sequence as u8, BLOCK));
            lost.commit_ordered(Some(b"ordered")).unwrap();
        }
        let waiting = {
            let lost = lost.clone();
            std::thread::spawn(move || {
                write(&lost, 2 + max_lag, &pattern(9, BLOCK));
                lost.commit_ordered(None)
            })
        };
        let uploader = lost.uploader().unwrap();
        while uploader.stalls() == 0 {
            std::thread::yield_now();
        }
        assert!(!waiting.is_finished(), "a commit past the lag went on");
        // The uploader goes on, cut off: it closes the store's open segment, for the commit
        // waiting, writing to the host's disk, and takes the segment framed, the waiting commit
        // last, to upload. Only from then on does nothing write to the disk, the uploads refused,
        // never acknowledged: lost sooner, the disk would go as the uploader writes to it, which
        // runs on, forgotten (TASK-165.9: the write-ahead log's segments, before a marker).
        let last = lost.committed().unwrap();
        let segments =
            dir.path().join("lost").join(format!("segments/{}", base32_id::Padded(cell.id)));
        let framed = || -> Vec<u64> {
            let files = std::fs::read_dir(&segments).into_iter().flatten();
            let bytes = files.filter_map(|file| std::fs::read(file.unwrap().path()).ok());
            let footers = bytes.filter_map(|bytes| layout::footer::<layout::Footer>(&bytes).ok());
            footers.flat_map(|footer| footer.contents.commits).collect()
        };
        while !framed().iter().any(|&tag| bucket_cell::layout::epoch_of(tag) == last) {
            // Returned, the commit waiting says the uploader failed, which never framed it.
            assert!(!waiting.is_finished(), "the commit waiting returned, its segment not framed");
            std::thread::yield_now();
        }
        // The host is lost: its store and uploader, forgotten, never reach object storage again,
        // and its disk is gone: renamed away at once, as its threads, forgotten, may still make
        // files in it, which a removal racing them finds (TASK-301: a log becoming a spare).
        std::mem::forget(lost);
        drop(waiting);
        std::fs::rename(dir.path().join("lost"), dir.path().join("gone")).unwrap();

        let bucket = Arc::new(ObjectBucket::new(store, prefix));
        let taken =
            kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                .unwrap();
        assert_eq!(taken.root(), Ok(b"durable".to_vec()));
        let mut data = vec![0; BLOCK];
        let durable = ExtentId { cell: cell.id, sequence: 1 };
        taken.read(durable, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert_eq!(data, pattern(1, BLOCK));
        let info = taken.info().unwrap();
        let sequence = info.next_sequence.max(info.sequence_floor);
        write(&taken, sequence, &pattern(3, BLOCK));
        taken.commit(Some(b"taken")).unwrap();
        drop(taken);
        // The lag's commits, and no more, lost. Read back with a reactor, which a real bucket's
        // client needs.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (extents, root) = {
            let _entered = runtime.enter();
            uploaded(&bucket, cell)
        };
        let sequences: Vec<u64> = extents.iter().map(|(sequence, _)| *sequence).collect();
        assert_eq!(sequences, [1, sequence]);
        assert_eq!(root, Some(b"taken".to_vec()));
    }

    #[test]
    fn cells_survive_their_hosts_loss() {
        survives_its_hosts_loss(Arc::new(InMemory::new()), "");
    }

    /// TASK-79.68: an extent freed while open and written anew before the commit is the cell's
    /// still once a store taking the cell recovers it from object storage: its segment's footer
    /// named it among the commit's frees too, which recovery applies after its extents, and the
    /// cell recovered lacked it.
    #[test]
    fn extents_freed_and_written_anew_survive_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let lost =
            kept_in(dir.path().join("lost"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = lost.cell();
        let extent = write(&lost, 1, &pattern(1, BLOCK));
        lost.free(extent).unwrap();
        write(&lost, 1, &pattern(2, BLOCK));
        lost.commit(None).unwrap();
        // The host is lost, what it committed in object storage.
        std::mem::forget(lost);

        let taken =
            kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle()).unwrap();
        let mut data = vec![0; BLOCK];
        taken.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert!(data == pattern(2, BLOCK), "the extent read back is not its second write");
    }

    /// TASK-92.6: a store taking a cell its host lost, the cell's segments since its last
    /// checkpoint many, recovers the cell once, its head taken between the start and the rest:
    /// by the tails of its segments, which hold their footers, a window of them at once, not by
    /// the segments whole. The bucket, which the lost host's store probed, it probes not again.
    /// TASK-165.11: what a durable commit, an open, a fork and a fork's first open cost on a real
    /// bucket, CLADEFS_PHASES_BUCKET's (`s3://bucket/prefix`, under a prefix of the run's own), by
    /// phase: each one's time, and the bucket's requests it made, by kind, their count and time
    /// (`Stats::request_ns`), the rest local (closing and framing the segment, flushing). A
    /// measurement, not a check: ignored, run on a host that reaches the bucket, as
    /// scripts/test-host.sh runs it with `CLADEFS_TEST_PARTS=workspace` and
    /// `CLADEFS_TEST_ARGS='--run-ignored only --no-capture bucket_round_trips_by_phase'`. Each
    /// operation is a line, `PHASE ...`.
    #[test]
    #[ignore = "a measurement against CLADEFS_PHASES_BUCKET's bucket"]
    fn bucket_round_trips_by_phase() {
        use bucket_cell::object_bucket::Request;
        let url =
            std::env::var("CLADEFS_PHASES_BUCKET").expect("CLADEFS_PHASES_BUCKET names a bucket");
        let url = format!("{}/phases-{:016x}/", url.trim_end_matches('/'), rand::random::<u64>());
        let bucket = crate::host_cell::bucket(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut phases: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
        // Runs `operation`, a `phase`, and says what it took and asked of the bucket.
        let mut measure = |phase: &'static str, operation: &mut dyn FnMut()| {
            let (before, started) = (bucket.stats(), std::time::Instant::now());
            operation();
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let after = bucket.stats();
            let mut line = format!("PHASE {phase} ms={took:.1}");
            let mut waited = 0.0;
            for request in Request::ALL {
                let count = after.requests(request) - before.requests(request);
                let ns = after.request_ns(request) - before.request_ns(request);
                if count > 0 {
                    line += &format!(" {}={count}/{:.1}ms", request.name(), ns as f64 / 1e6);
                    waited += ns as f64 / 1e6;
                }
            }
            println!("{line} bucket-ms={waited:.1}");
            phases.entry(phase).or_default().push(took);
        };
        let base = dir.path().join("base");
        let cell = crate::host_cell::new_alone();
        let mut store = None;
        measure("create", &mut || {
            let made = OpenCell::create_kept_with(&base, 1 << 30, cell, &url, idle());
            store = Some(made.unwrap());
        });
        let store = store.unwrap();
        for sequence in 1..=20 {
            write(&store, sequence, &pattern(sequence as u8, BLOCK));
            measure("durable-commit", &mut || store.commit(None).unwrap());
        }
        drop(store);
        for _ in 0..10 {
            measure("open", &mut || drop(reopened(&base, idle()).unwrap()));
        }
        let store = reopened(&base, idle()).unwrap();
        let forks: Vec<_> = (0..10).map(|fork| dir.path().join(format!("fork-{fork}"))).collect();
        for fork in &forks {
            let child = crate::host_cell::new_alone();
            measure("fork", &mut || store.fork_to(fork, child).unwrap());
        }
        drop(store);
        for fork in &forks {
            measure("fork-open", &mut || drop(reopened(fork, idle()).unwrap()));
        }
        for (phase, mut took) in phases {
            took.sort_by(f64::total_cmp);
            println!(
                "PHASES {phase} n={} median-ms={:.1} min-ms={:.1} max-ms={:.1}",
                took.len(),
                took[took.len() / 2],
                took[0],
                took[took.len() - 1]
            );
        }
    }

    /// TASK-165.11: a durable commit, as a guest's sync makes one, costs the bucket what doc-7's
    /// The head say it needs: the create of the segment its commit closes, then the swap of the
    /// cell's head that names it, which acknowledges it (TASK-407: before, the create alone, the
    /// fencing; TASK-371: before that, a read of the head after it); nothing else, as no
    /// checkpoint is due and no fork made. On GCS each request is a round trip.
    #[test]
    fn durable_commits_cost_a_create_and_a_head_swap() {
        use bucket_cell::object_bucket::Request;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let store =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        write(&store, 1, &pattern(1, BLOCK));
        store.commit(None).unwrap();
        let before = bucket.stats();
        write(&store, 2, &pattern(2, BLOCK));
        store.commit(None).unwrap();
        let after = bucket.stats();
        let asked: Vec<_> = (Request::ALL.iter())
            .map(|&request| (request.name(), after.requests(request) - before.requests(request)))
            .collect();
        assert_eq!(
            asked,
            [
                ("get", 0),
                ("get-range", 0),
                ("head", 0),
                ("list", 0),
                ("create", 1),
                ("update", 1),
                ("overwrite", 0),
                ("delete", 0),
                ("list-delimited", 0),
                ("delete-batch", 0),
            ]
        );
    }

    #[test]
    fn takes_recover_their_cells_once_by_their_segments_tails() {
        use bucket_cell::layout;
        use bucket_cell::object_bucket::Request;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let lost =
            kept_in(dir.path().join("lost"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = lost.cell();
        // A segment of a MiB for each commit, uploaded as it is durable.
        for sequence in 1..=4 {
            write(&lost, sequence, &noise(sequence, 1 << 20));
            lost.commit(None).unwrap();
        }
        // The host is lost, its uploader stopping where it is, with no checkpoint.
        std::mem::forget(lost);

        let before = bucket.stats();
        let taken =
            kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                .unwrap();
        let after = bucket.stats();
        let asked: Vec<_> = (Request::ALL.iter())
            .map(|&request| (request.name(), after.requests(request) - before.requests(request)))
            .collect();
        let sizes: std::collections::BTreeMap<_, _> =
            futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, ""))
                .unwrap()
                .into_iter()
                .map(|meta| (meta.key, meta.size))
                .collect();
        let tails: u64 = (0..4)
            .map(|segment| sizes[&layout::segment(cell, segment, 1)].min(bucket_cell::object::TAIL))
            .sum();
        let head = sizes[&layout::head(cell)];
        // Its head, read once, the recovery starting from it, then swapped to take it (TASK-449,
        // TASK-515). The segments' tails it names, all at once: no listing (TASK-407).
        assert_eq!(
            (asked, after.downloaded - before.downloaded),
            (
                vec![
                    ("get", 1),
                    ("get-range", 4),
                    ("head", 0),
                    ("list", 0),
                    ("create", 0),
                    ("update", 1),
                    ("overwrite", 0),
                    ("delete", 0),
                    ("list-delimited", 0),
                    ("delete-batch", 0),
                ],
                tails + head
            )
        );
        let mut data = vec![0; 1 << 20];
        let extent = ExtentId { cell: cell.id, sequence: 4 };
        taken.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert_eq!(data, noise(4, 1 << 20));
    }

    /// As `cells_survive_their_hosts_loss`, in the bucket CLADEFS_S3_URL names: RustFS's
    /// (scripts/test-rustfs.sh), or another S3 store's.
    #[test]
    #[ignore = "needs an S3 bucket: scripts/test-rustfs.sh, CI's object-stores job"]
    fn rustfs_cells_survive_their_hosts_loss() {
        let url = std::env::var("CLADEFS_S3_URL").expect("CLADEFS_S3_URL names the bucket to use");
        let (store, prefix) = bucket_cell::object_bucket::object_store_at(&url).unwrap();
        // A prefix of the run's own, in a bucket runs share.
        let prefix = format!("{prefix}/{:016x}", rand::random::<u64>());
        survives_its_hosts_loss(store, prefix.trim_start_matches('/'));
    }

    #[test]
    fn commits_wait_at_the_lag_bound() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options { interval, max_lag: 2, ..Default::default() };
        waits_at_the_lag_bound(options, |gated| &gated.shut);
    }

    #[test]
    /// Three blocks' lag: the first two commits' at most, whether or not the uploader closed the
    /// segment between the second's write and its commit, the segment holding the write, which the
    /// commit copied into the next (TASK-92.41): their lag a block more. A bound of two blocks
    /// left the second waiting then, once in some 350 runs, and once in 60 with the uploader
    /// closing segments sooner, as it no longer waits for their framed files durable (TASK-384).
    fn commits_wait_at_the_lag_bound_in_bytes() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options {
            interval,
            max_lag_bytes: 3 * BLOCK as u64,
            ..Default::default()
        };
        waits_at_the_lag_bound(options, |gated| &gated.shut);
    }

    /// TASK-314: a bucket that keeps failing its writes, its disk full, holds its stores' commits
    /// at the lag bound, failing none and losing none: as the guest sees it, its fsyncs wait once
    /// the lag reaches its bound (its syncs at once), until the bucket has room (doc-12).
    #[test]
    fn commits_wait_at_the_lag_bound_while_the_bucket_is_full() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options { interval, max_lag: 2, ..Default::default() };
        waits_at_the_lag_bound(options, |gated| &gated.full);
    }

    /// A cell opens on a host without it: an empty store of it takes what the cell holds, and
    /// writes on; the store it came from, opened again, finds the cell moved on, and refuses.
    #[test]
    fn opens_cells_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let first =
            kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = first.cell();
        let large = pattern(2, 4 << 20);
        write(&first, 1, &pattern(1, BLOCK));
        write(&first, 2, &large);
        first.commit(Some(b"root")).unwrap();
        drop(first);

        let second =
            kept_in(dir.path().join("second"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                .unwrap();
        assert_eq!(second.root(), Ok(b"root".to_vec()));
        let mut data = vec![0; 4 << 20];
        let extent = ExtentId { cell: cell.id, sequence: 2 };
        second.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert_eq!(data, large);
        let info = second.info().unwrap();
        let sequence = info.next_sequence.max(info.sequence_floor);
        write(&second, sequence, &pattern(3, BLOCK));
        second.commit(None).unwrap();
        drop(second);

        // cladefs: the first, opened again, its write-ahead log empty, takes the cell as it is
        // (TASK-165.9): before, a store that held the cell refused once it moved on.
        let first = reopened(dir.path().join("first"), idle()).unwrap();
        assert_eq!(first.root(), Ok(b"root".to_vec()));
        let mut data = vec![0; BLOCK];
        let extent = ExtentId { cell: cell.id, sequence };
        first.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert_eq!(data, pattern(3, BLOCK));
        drop(first);
        let (extents, root) = uploaded(&bucket, cell);
        assert_eq!(
            extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(),
            [1, 2, sequence]
        );
        assert_eq!(root, Some(b"root".to_vec()));
    }

    /// TASK-260: a read for a guest request of what only object storage holds logs the requests
    /// it makes of the bucket, on the bucket runtime, with the guest request.
    #[cfg(target_os = "linux")]
    #[test]
    fn reads_of_object_storage_are_logged_with_their_guest_request() {
        use crate::request::{RequestId, within_now};
        let lines = crate::cladefsd::logging::capture();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let first =
            kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = first.cell();
        write(&first, 1, &pattern(1, BLOCK));
        first.commit(None).unwrap();
        drop(first);
        let second =
            kept_in(dir.path().join("second"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                .unwrap();
        lines.lock().unwrap().clear();
        let request = RequestId {
            guest: std::num::NonZeroU64::new(42).unwrap(),
            store: Some("/stores/vm1".into()),
        };
        let mut data = vec![0; BLOCK];
        let extent = ExtentId { cell: cell.id, sequence: 1 };
        within_now(Some(request), || {
            second.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap()
        });
        assert_eq!(data, pattern(1, BLOCK));
        // Each request of the bucket the read made: its kind, and the request it names.
        let mut made: Vec<(String, String)> = (lines.lock().unwrap().iter())
            .filter_map(|line| {
                line.strip_prefix("DEBUG bucket_cell::object_bucket: object storage: ")
            })
            .map(|line| {
                let kind = line.split(' ').next().unwrap().to_owned();
                (kind, line.split_once(" request_id=").map_or("none", |(_, id)| id).to_owned())
            })
            .collect();
        made.dedup();
        assert_eq!(made, [("get-range".to_owned(), "42 store=/stores/vm1".to_owned())]);
        drop(second);
    }

    /// Opens the store at `path`, whose bucket's objects `gated` reaches, the write of its cell's
    /// head that takes it held until `meanwhile` ran.
    fn open_racing(
        path: std::path::PathBuf,
        gated: &Arc<Gated>,
        cell: crate::CellId,
        meanwhile: impl FnOnce(),
    ) -> Result<OpenCell, crate::Error> {
        gated.hold.send_replace(Hold::First(bucket_cell::layout::head(cell)));
        let opening = std::thread::spawn(move || reopened(path, idle()));
        gated.wait_held(1);
        meanwhile();
        gated.hold.send_replace(Hold::Nothing);
        opening.join().unwrap()
    }

    /// The directory of a store of `cell` at `path`, its cell kept in `bucket`, made whole, and
    /// not opened: what opening takes the cell into, as the bucket holds it.
    fn made_for(path: &std::path::Path, cell: crate::CellId, bucket: &Arc<ObjectBucket>) {
        std::fs::create_dir(path).unwrap();
        let description =
            local_cell::Description { block_size: BLOCK as u32, capacity: 1 << 30, cell };
        let kind = bucket_cell::cell::Kind::Fxfs;
        local_cell::create_host(path, description, &url_of(bucket), kind).unwrap();
        local_cell::made_host(path).unwrap();
    }

    /// TASK-79.4: a store opening takes the cell as of its taking of the head, not as of what it
    /// found before: the commit another writer made meanwhile, which frees the extent the cell
    /// held before and writes another, is the cell's, and the store's.
    #[test]
    fn takes_take_what_the_cell_holds_once_taken() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let first = kept_in(dir.path().join("first"), (1 << 30, None), &bucket, idle()).unwrap();
        let cell = first.cell();
        let before = write(&first, 1, &pattern(1, BLOCK));
        first.commit(Some(b"before")).unwrap();

        made_for(&dir.path().join("second"), cell, &bucket);
        let second = open_racing(dir.path().join("second"), &gated, cell, || {
            write(&first, 2, &pattern(2, BLOCK));
            first.free(before).unwrap();
            first.commit(Some(b"meanwhile")).unwrap();
        });
        let second = second.unwrap();
        assert_eq!(second.root(), Ok(b"meanwhile".to_vec()));
        let mut data = vec![0; BLOCK];
        let written = ExtentId { cell: cell.id, sequence: 2 };
        second.read(written, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert_eq!(data, pattern(2, BLOCK));
        assert_eq!(
            uploaded(&bucket, cell),
            (vec![(2, pattern(2, BLOCK))], Some(b"meanwhile".to_vec()))
        );
    }

    /// TASK-79.4, TASK-165.9: a store whose write-ahead log holds commits the cell moved past
    /// refuses to open (`ESTALE`), those commits never acknowledged: before its head is taken, its
    /// log diverged as recovery first finds the cell; and once it holds it, another writer having
    /// committed between the two (`wal_follows`), having fenced that writer, whose commits stay the
    /// cell's. `OpenCell::take` drops the log, and opens the store as the cell is.
    #[test]
    fn stores_whose_cell_moved_on_before_their_taking_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let memory: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let gated = Arc::new(Gated::over(memory.clone()));
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        // The first host's, through a gate of its own.
        let dying = Arc::new(Gated::over(memory));
        let lost = dir.path().join("lost");
        let first = kept_in(
            &lost,
            (1 << 30, None),
            &Arc::new(ObjectBucket::new(dying.clone(), "")),
            idle(),
        );
        let first = first.unwrap();
        let cell = first.cell();
        write(&first, 1, &pattern(1, BLOCK));
        first.commit(Some(b"before")).unwrap();
        // A commit its log holds, durable on its disk alone, as its host dies: restarted with its
        // disk as it was, reaching the bucket again; what it had going does nothing more.
        write(&first, 2, &pattern(2, BLOCK));
        first.commit_ordered(Some(b"local")).unwrap();
        first.flush().unwrap();
        let path = dir.path().join("first");
        restart(&lost, &path);
        dying.sever();
        drop(first);
        naming(&path, &bucket);

        // The cell moves to another host, which takes it, as the bucket has it.
        let taken = kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle());
        let taken = taken.unwrap();
        assert_eq!(taken.root(), Ok(b"before".to_vec()));
        // Its writer's fence moved the cell past no commit: the first's log follows it still.
        let opened = open_racing(path.clone(), &gated, cell, || {
            let info = taken.info().unwrap();
            let sequence = info.next_sequence.max(info.sequence_floor);
            write(&taken, sequence, &pattern(3, BLOCK));
            taken.commit(Some(b"meanwhile")).unwrap();
        });
        assert_eq!(opened.err().and_then(|err| err.errno()), Some(crate::Errno::ESTALE));
        // The writer that committed meanwhile was fenced, its commit the cell's.
        let (extents, root) = uploaded(&bucket, cell);
        assert_eq!(extents.len(), 2);
        assert_eq!(root, Some(b"meanwhile".to_vec()));
        drop(taken);
        // Plainly diverged now, before the taking.
        assert_eq!(
            reopened(&path, idle()).err().and_then(|err| err.errno()),
            Some(crate::Errno::ESTALE)
        );
        // Taken, its log dropped: the store holds the cell as it is, its local commit lost.
        OpenCell::take(&path, 1 << 30, cell, None).unwrap();
        let first = reopened(&path, idle()).unwrap();
        assert_eq!(first.root(), Ok(b"meanwhile".to_vec()));
    }

    /// A fork of a store kept in object storage is a fork of its cell there, recorded once the
    /// parent uploaded through it: it inherits the parent's extents rather than upload them again,
    /// and what either commits after is its own.
    /// TASK-160.10: a store at a version refuses all that would change what it holds, with `EROFS`,
    /// whoever asks: writes, frees, commits, a resize, a writer taking it over. What changes none
    /// of its extents it does: it forks, and is made public.
    #[test]
    fn copies_of_versions_refuse_what_would_change_them() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let source =
            kept_in(dir.path().join("source"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let extent = write(&source, 1, &pattern(1, BLOCK));
        source.commit(Some(b"root")).unwrap();
        let path = dir.path().join("copy");
        source.uploaded_version_at(&path).unwrap();
        drop(source);

        let version = reopened(&path, idle()).unwrap();
        assert_eq!(version.access().role, crate::CellRole::Version);
        let capacity = version.info().unwrap().capacity;
        let own = ExtentId { cell: version.cell().id, sequence: 2 };
        let refused = [
            ("set_capacity", version.set_capacity(2 * capacity)),
            ("write", version.write(own, 0, &[VolatileSlice::from(&pattern(2, BLOCK)[..])])),
            ("free", version.free(extent)),
            ("commit", version.commit(Some(b"changed"))),
            ("commit_ordered", version.commit_ordered(None)),
            ("take_over", version.take_over()),
        ];
        for (what, result) in refused {
            assert_eq!(result, Err(crate::Errno::EROFS), "{what}");
        }
        assert_eq!(version.info().unwrap().capacity, capacity);
        assert_eq!(version.root().unwrap(), b"root");
        version
            .fork_to(dir.path().join("fork"), crate::host_cell::fork_child(version.cell()))
            .unwrap();
        version.make_public().unwrap();
    }

    /// TASK-395.7: a version forks into a cell of another tenant's only while shared: refused with
    /// `EACCES` before, leaving nothing at the fork's path; shared, the fork is made; unshared, new
    /// forks are refused again. Forks into its own tenant's cells need no sharing; sharing a
    /// version the head names not is `ENOENT`. The version dropped once its cell freed what it
    /// held, and the collector run, the other tenant's fork, the one left, still reads all it
    /// inherited.
    #[test]
    fn versions_fork_into_other_tenants_cells_while_shared() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let of = |tenant| crate::host_cell::named(tenant);
        let source = dir.path().join("source");
        let source = kept_in(&source, (1 << 30, Some(of(1))), &bucket, idle()).unwrap();
        let extent = write(&source, 1, &pattern(1, BLOCK));
        source.commit(Some(b"base")).unwrap();
        let path = dir.path().join("copy");
        let version = source.uploaded_version_at(&path).unwrap();
        source.free(extent).unwrap();
        source.commit(Some(b"later")).unwrap();
        // Another fork, gone: a checkpoint past the version's, so that only the version, and its
        // forks, keep what it names.
        // Deleted as this process, no host, deletes them, of the first placement.
        let delete = |path: &std::path::Path| {
            crate::host_cell::delete(path, Some(&bucket), crate::host_cell::token).unwrap()
        };
        let later = dir.path().join("later");
        source.fork_to(&later, crate::host_cell::fork_child(source.cell())).unwrap();
        delete(&later);
        let copy = reopened(&path, idle()).unwrap();
        let fork = |name: &str, tenant| {
            let to = dir.path().join(name);
            let forked = copy.fork_to(&to, of(tenant)).map_err(|err| err.errno());
            (forked, to.exists())
        };

        assert_eq!(fork("refused", 2), (Err(Some(crate::Errno::EACCES)), false));
        assert_eq!(fork("ours", 1), (Ok(()), true));
        source.share_version(version, true).unwrap();
        assert_eq!(fork("theirs", 2), (Ok(()), true));
        source.share_version(version, false).unwrap();
        assert_eq!(fork("unshared", 2), (Err(Some(crate::Errno::EACCES)), false));
        let refused = source.share_version(version + 1, true);
        assert_eq!(refused.map_err(|err| err.errno()), Err(Some(crate::Errno::ENOENT)));

        drop(copy);
        delete(&dir.path().join("ours"));
        delete(&path);
        source.drop_versions(&[version].into()).unwrap();
        let options = bucket_cell::collector::Options { compact_below: 0.5, grace: 0 };
        for holder in 1..=2 {
            let pass = bucket_cell::collector::collect(&*bucket, holder, &options);
            futures::executor::block_on(pass).unwrap();
        }
        // What this process cached of the extents goes: the fork reads them from the bucket.
        crate::cache::evict_least_used(None, |_| false);
        let theirs = reopened(dir.path().join("theirs"), idle()).unwrap();
        assert_eq!(theirs.root().unwrap(), b"base");
        let mut data = vec![0; BLOCK];
        theirs.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert!(data == pattern(1, BLOCK), "the fork reads other bytes");
    }

    /// TASK-411.4: `points_fork_into_other_tenants_cells_while_shared`, both tenants' KEKs in the
    /// host's KMS: the version's fork into the other tenant's cell keeps the DEK it inherits
    /// rewrapped under its own cell's KEK. The version dropped, the collector run (compacting
    /// nothing: compaction of encrypted objects is TASK-411.6's), and the version's tenant's KEK
    /// destroyed, the host told so: the clone still reads all it inherited, and the source forks
    /// no more.
    #[test]
    fn encrypted_versions_fork_into_other_tenants_cells_while_shared() {
        let keys = tempfile::tempdir().unwrap();
        let kms = Arc::new(extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap());
        for tenant in [1, 2] {
            kms.create(tenant).unwrap();
        }
        crate::bucket::set_kms(kms.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let of = |tenant| crate::host_cell::named(tenant);
        let source = dir.path().join("source");
        let source = kept_in(&source, (1 << 30, Some(of(1))), &bucket, idle()).unwrap();
        let extent = write(&source, 1, &pattern(1, BLOCK));
        source.commit(Some(b"base")).unwrap();
        let path = dir.path().join("copy");
        let version = source.uploaded_version_at(&path).unwrap();
        source.free(extent).unwrap();
        source.commit(Some(b"later")).unwrap();
        let delete = |path: &std::path::Path| {
            crate::host_cell::delete(path, Some(&bucket), crate::host_cell::token).unwrap()
        };
        let copy = reopened(&path, idle()).unwrap();
        let fork = |name: &str, tenant| {
            let to = dir.path().join(name);
            let forked = copy.fork_to(&to, of(tenant)).map_err(|err| err.errno());
            (forked, to.exists())
        };
        assert_eq!(fork("refused", 2), (Err(Some(crate::Errno::EACCES)), false));
        assert_eq!(fork("ours", 1), (Ok(()), true));
        source.share_version(version, true).unwrap();
        assert_eq!(fork("theirs", 2), (Ok(()), true));
        let theirs = reopened(dir.path().join("theirs"), idle()).unwrap();
        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, theirs.cell()));
        let record = record.unwrap().unwrap();
        let inherited: Vec<_> = record.inherited.iter().map(|had| had.cell).collect();
        assert_eq!((record.deks.len(), inherited), (1, vec![source.cell()]));
        drop(theirs);

        drop(copy);
        delete(&dir.path().join("ours"));
        delete(&path);
        source.drop_versions(&[version].into()).unwrap();
        let options = bucket_cell::collector::Options { compact_below: 0.0, grace: 0 };
        for holder in 1..=2 {
            let pass = bucket_cell::collector::collect(&*bucket, holder, &options);
            futures::executor::block_on(pass).unwrap();
        }
        kms.destroy(1).unwrap();
        crate::bucket::kms().unwrap().revoked(1, "tenant 1: its KEK is destroyed");
        crate::cache::evict_least_used(None, |_| false);
        let theirs = reopened(dir.path().join("theirs"), idle()).unwrap();
        assert_eq!(theirs.root().unwrap(), b"base");
        let mut data = vec![0; BLOCK];
        theirs.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert!(data == pattern(1, BLOCK), "the fork reads other bytes");
        let refused = source.fork_to(dir.path().join("after"), of(1)).map_err(|err| err.errno());
        assert_eq!(refused, Err(Some(crate::Errno::EACCES)));
    }

    /// TASK-411.19, as TASK-411.28 left it: a tenant whose KEK the KMS has forgotten (answering
    /// as for a tenant that never had one), and a tenant that never had one, get no store opened
    /// here, encrypted or in the clear: `EACCES`, the cell not made.
    #[test]
    fn tenants_with_no_kek_in_the_kms_open_no_store() {
        let keys = tempfile::tempdir().unwrap();
        let local = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
        local.create(1).unwrap();
        local.destroy(1).unwrap();
        crate::bucket::set_kms(Arc::new(extent_keys::testing::Forgetting::new(local))).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let of = |tenant| crate::host_cell::named(tenant);
        for (name, tenant) in [("forgotten", 1), ("never", 2)] {
            let cell = of(tenant);
            let refused = kept_in(dir.path().join(name), (1 << 30, Some(cell)), &bucket, idle());
            let refused = refused.map(drop).map_err(|err| err.errno());
            assert_eq!(refused, Err(Some(crate::Errno::EACCES)), "{name}");
            let cell = futures::executor::block_on(bucket_cell::head::get(&*bucket, cell));
            assert_eq!(cell, Ok(None), "{name}");
        }
    }

    /// TASK-411.7: a host told a cell is shredded, or a tenant's KEK destroyed, forgets their keys:
    /// their stores open here are halted, every request failing with `EACCES`, as their openings
    /// do; their extents leave the extent cache, pages and all; its fetchers forget their rings.
    /// The tenant's other cell's store, until its tenant goes, and another tenant's, read on, their
    /// extents cached still. No file of the stores' directories holds what they wrote.
    #[test]
    fn stores_of_shredded_cells_and_tenants_are_halted_and_leave_the_cache() {
        let keys = tempfile::tempdir().unwrap();
        let kms = Arc::new(extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap());
        for tenant in [1, 2] {
            kms.create(tenant).unwrap();
        }
        crate::bucket::set_kms(kms.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let of = |tenant| crate::host_cell::named(tenant);
        let names = ["ours", "sibling", "theirs"];
        let stores: Vec<(OpenCell, ExtentId, Vec<u8>)> = (names.iter().zip([1, 1, 2]))
            .enumerate()
            .map(|(at, (name, tenant))| {
                let store =
                    kept_in(dir.path().join(name), (1 << 30, Some(of(tenant))), &bucket, idle());
                let store = store.unwrap();
                let data = noise(at as u64 + 1, 4 * BLOCK);
                let extent = write(&store, 1, &data);
                store.commit(None).unwrap();
                store.wait_uploaded().unwrap();
                (store, extent, data)
            })
            .collect();
        let read = |at: usize| {
            let (store, extent, data) = &stores[at];
            let mut read = vec![0; data.len()];
            let done = store.read(*extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])]);
            done.map(|()| read == *data)
        };
        let cached = || {
            let extents: std::collections::BTreeSet<ExtentId> =
                crate::cache::keys().into_iter().map(|(_, extent)| extent).collect();
            stores.iter().map(|(_, extent, _)| extents.contains(extent)).collect::<Vec<_>>()
        };
        assert_eq!((0..3).map(read).collect::<Vec<_>>(), [Ok(true), Ok(true), Ok(true)]);
        assert_eq!(cached(), [true, true, true]);
        let reopened_errno = |at: usize| {
            reopened(dir.path().join(names[at]), idle()).map(drop).map_err(|err| err.errno())
        };

        let cell = stores[0].0.cell();
        let shredded = futures::executor::block_on(crate::bucket::shred_cell(&bucket, cell));
        let (removed, forgotten) = shredded.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            (forgotten.stores, forgotten.rings, forgotten.extents),
            (1, 1, 1),
            "{forgotten:?}"
        );
        assert_eq!(
            (0..3).map(read).collect::<Vec<_>>(),
            [Err(crate::Errno::EACCES), Ok(true), Ok(true)]
        );
        assert_eq!(cached(), [false, true, true]);
        assert_eq!(stores[0].0.commit(None), Err(crate::Errno::EACCES));

        kms.destroy(1).unwrap();
        let why = "tenant 1: its KEK is destroyed";
        let forgotten = crate::bucket::forget(bucket_cell::keys::Scope::Tenant(1), why);
        assert_eq!(
            (forgotten.stores, forgotten.rings, forgotten.extents),
            (1, 1, 1),
            "{forgotten:?}"
        );
        assert_eq!(
            (0..3).map(read).collect::<Vec<_>>(),
            [Err(crate::Errno::EACCES), Err(crate::Errno::EACCES), Ok(true)]
        );
        assert_eq!(cached(), [false, false, true]);
        assert_eq!(crate::bucket::kms().unwrap().in_pool(1), 0);

        // Closed by their holders, the stores of the tenant open no more; the other's does.
        let written: Vec<Vec<u8>> = stores.into_iter().map(|(_, _, data)| data).collect();
        assert_eq!(
            (0..3).map(reopened_errno).collect::<Vec<_>>(),
            [Err(Some(crate::Errno::EACCES)), Err(Some(crate::Errno::EACCES)), Ok(())]
        );

        let pieces: Vec<&[u8]> = written[..2].iter().map(|data| &data[..64]).collect();
        let held = crate::testing::files_holding(dir.path(), &pieces);
        assert_eq!(held, Vec::<String>::new(), "files holding a store's data");
    }

    /// TASK-411.27: a host given a master key (`CLADEFS_MASTER_KEY`) and no `CLADEFS_KMS` keeps its
    /// tenants' KEKs in its bucket: every tenant's store is encrypted, its KEK made on its first use
    /// at `tenants/<t>/kek`. Shredding a tenant here leaves a tombstone there, halts its store
    /// (`EACCES`), which opens no more, nor any new store of the tenant's, nothing made; another
    /// tenant's reads on. Shredding it again finds it so.
    #[test]
    fn hosts_given_a_master_key_alone_encrypt_every_store_under_the_bucket_kms() {
        use bucket_cell::bucket_kms::Destroyed;
        use bucket_cell::layout;
        let url = "memory:///bucket-kms";
        let master: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
        // SAFETY: this test's process is its own (nextest), no store open yet.
        unsafe {
            std::env::remove_var("CLADEFS_KMS");
            std::env::set_var("CLADEFSD_BUCKET", url);
            std::env::set_var("CLADEFS_MASTER_KEY", &master);
        }
        let bucket = crate::host_cell::bucket(url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let of = |tenant| crate::host_cell::named(tenant);
        let names = ["ours", "theirs"];
        let stores: Vec<(OpenCell, ExtentId, Vec<u8>)> = (names.iter().zip([1, 2]))
            .enumerate()
            .map(|(at, (name, tenant))| {
                let store =
                    kept_in(dir.path().join(name), (1 << 30, Some(of(tenant))), &bucket, idle());
                let store = store.unwrap();
                let data = noise(at as u64 + 1, 4 * BLOCK);
                let extent = write(&store, 1, &data);
                store.commit(None).unwrap();
                store.wait_uploaded().unwrap();
                (store, extent, data)
            })
            .collect();
        assert!(crate::bucket::bucket_kms().is_some());
        for (store, _, _) in &stores {
            let record =
                futures::executor::block_on(bucket_cell::head::get(&*bucket, store.cell()));
            assert_eq!(record.unwrap().unwrap().deks.len(), 1);
            let cell = futures::executor::block_on(bucket_cell::head::get(&*bucket, store.cell()));
            assert_eq!(cell.unwrap().unwrap().keks.len(), 1);
        }
        let kek = |tenant| {
            let key = layout::kek(tenant);
            let got = futures::executor::block_on(bucket_cell::bucket::get(&*bucket, &key));
            got.map(|object| object.bytes)
        };
        assert_eq!(kek(1).map(|bytes| bytes.len()), Some(extent_keys::kms::bucket::KEK_OBJECT_LEN));
        let read = |at: usize| {
            let (store, extent, data) = &stores[at];
            let mut read = vec![0; data.len()];
            let done = store.read(*extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])]);
            done.map(|()| read == *data)
        };

        let shredded = futures::executor::block_on(crate::bucket::shred_tenant(1));
        let (destroyed, forgotten) = shredded.unwrap();
        assert_eq!((destroyed, forgotten.stores, forgotten.rings), (Destroyed::Now, 1, 1));
        assert_eq!(kek(1), Some(b"fxkt\x01\x02\0\0\0\0\0\0\0\x01".to_vec()));
        assert_eq!((read(0), read(1)), (Err(crate::Errno::EACCES), Ok(true)));
        let written: Vec<Vec<u8>> = stores.into_iter().map(|(_, _, data)| data).collect();
        let reopened_errno = |name: &str| {
            reopened(dir.path().join(name), idle()).map(drop).map_err(|err| err.errno())
        };
        assert_eq!(reopened_errno("ours"), Err(Some(crate::Errno::EACCES)));
        assert_eq!(reopened_errno("theirs"), Ok(()));
        let new = of(1);
        let refused = kept_in(dir.path().join("new"), (1 << 30, Some(new)), &bucket, idle());
        assert_eq!(refused.map(drop).map_err(|err| err.errno()), Err(Some(crate::Errno::EACCES)));
        let cell = futures::executor::block_on(bucket_cell::head::get(&*bucket, new));
        assert_eq!(cell, Ok(None));
        let again = futures::executor::block_on(crate::bucket::shred_tenant(1)).unwrap();
        assert_eq!(again.0, Destroyed::Already);
        assert_eq!(written.len(), 2);
    }

    /// TASK-411.27: a host whose KMS is not the bucket KMS shreds no tenant: its KEK is its KMS's
    /// to destroy. Nothing is written.
    #[test]
    fn hosts_of_another_kms_shred_no_tenant() {
        let keys = tempfile::tempdir().unwrap();
        let kms = Arc::new(extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap());
        crate::bucket::set_kms(kms).unwrap();
        let refused = futures::executor::block_on(crate::bucket::shred_tenant(1)).unwrap_err();
        assert_eq!(
            refused,
            "tenant 1: this host's KMS is not the bucket KMS (CLADEFS_MASTER_KEY, no CLADEFS_KMS): \
             destroy the tenant's KEK in its KMS, then have every host forget its keys"
        );
    }

    /// TASK-395.9: a store with a writer forks into a cell of another tenant's only if it is the
    /// default tenant's (0). Another tenant's store is refused with `EACCES` by every kind of fork,
    /// into any other tenant's cell, the default tenant's too, leaving nothing at the fork's path
    /// nor in the bucket; into its own tenant's it forks. A store of the default tenant's, as
    /// public bases are, forks into two tenants' cells, each reading what it holds.
    #[test]
    fn stores_with_writers_fork_into_other_tenants_cells_from_the_common_tenant_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let of = |tenant| crate::host_cell::named(tenant);
        let listed = || {
            let listed = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, ""));
            listed.unwrap()
        };
        let store = |name: &str, tenant| {
            let store =
                kept_in(dir.path().join(name), (1 << 30, Some(of(tenant))), &bucket, idle());
            let store = store.unwrap();
            write(&store, 1, &pattern(tenant as u8, BLOCK));
            store.commit(Some(name.as_bytes())).unwrap();
            store.wait_uploaded().unwrap();
            store
        };

        let acme = store("acme", 1);
        let before = listed();
        let errno = |forked: Result<(), crate::Error>| forked.map_err(|err| err.errno());
        for tenant in [2, crate::host_cell::DEFAULT_TENANT] {
            let to = dir.path().join(format!("refused-{tenant}"));
            let refused = [
                ("fork", errno(acme.fork(&to, tenant).map(drop))),
                ("fork_quiescent", errno(acme.fork_quiescent(&to, tenant).map(drop))),
                ("fork_to", errno(acme.fork_to(&to, of(tenant)))),
            ];
            for (how, refused) in refused {
                assert_eq!(refused, Err(Some(crate::Errno::EACCES)), "{how} to tenant {tenant}");
                assert!(!to.exists(), "{how} to tenant {tenant} made a store");
            }
        }
        assert_eq!(listed(), before, "a refused fork wrote to the bucket");
        let ours = dir.path().join("ours");
        assert_eq!(acme.fork(&ours, 1).unwrap().tenant, 1);
        assert_eq!(reopened(&ours, idle()).unwrap().root().unwrap(), b"acme");

        let base = store("base", crate::host_cell::DEFAULT_TENANT);
        for tenant in [1, 2] {
            let to = dir.path().join(format!("base-{tenant}"));
            assert_eq!(base.fork(&to, tenant).unwrap().tenant, tenant);
            let fork = reopened(&to, idle()).unwrap();
            assert_eq!(fork.root().unwrap(), b"base");
            let mut data = vec![0; BLOCK];
            let extent = ExtentId { cell: base.cell().id, sequence: 1 };
            fork.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
            assert!(data == pattern(0, BLOCK), "tenant {tenant}'s fork reads other bytes");
        }
    }

    #[test]
    fn forks_inherit_their_parents_extents() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let parent =
            kept_in(dir.path().join("parent"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let large = pattern(2, 4 << 20);
        let small = write(&parent, 1, &pattern(1, BLOCK));
        let own = write(&parent, 2, &large);
        // Committed, not yet durable: the fork makes it so, and uploads it, first.
        parent.commit_ordered(Some(b"root")).unwrap();
        let child = crate::host_cell::fork_child(parent.cell());
        let checkpoints = || {
            let prefix = bucket_cell::layout::checkpoints(parent.cell());
            futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, &prefix)).unwrap()
        };
        assert_eq!(checkpoints().len(), 0);
        parent.fork_to(dir.path().join("child"), child).unwrap();
        // A checkpoint at the fork, which the child recovers from.
        assert_eq!(checkpoints().len(), 1);
        write(&parent, 3, &pattern(3, BLOCK));
        parent.commit(None).unwrap();

        let record =
            futures::executor::block_on(bucket_cell::head::get(&*bucket, child)).unwrap().unwrap();
        let origin = record.origin().unwrap();

        assert_eq!(origin.parent, parent.cell());
        let inherited = |state: &bucket_cell::State| {
            let parents = [
                bucket_cell::layout::segments(parent.cell()),
                bucket_cell::layout::extents(parent.cell()),
            ];
            state
                .extents
                .iter()
                .filter(|(_, location)| {
                    parents.iter().any(|prefix| location.key.starts_with(prefix))
                })
                .map(|(extent, _)| extent.sequence)
                .collect::<Vec<_>>()
        };

        let forked = reopened(dir.path().join("child"), idle()).unwrap();
        let state =
            futures::executor::block_on(recover(&*bucket, child, Some(origin))).unwrap().state;
        assert_eq!(inherited(&state), [small.sequence, own.sequence]);
        assert_eq!(state.root, Some(b"root".to_vec()));
        let mut data = vec![0; 4 << 20];
        forked.read(own, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).unwrap();
        assert_eq!(data, large);
        let info = forked.info().unwrap();
        let sequence = info.next_sequence.max(info.sequence_floor);
        let mine = write(&forked, sequence, &pattern(4, BLOCK));
        forked.commit(None).unwrap();
        drop(forked);
        let state =
            futures::executor::block_on(recover(&*bucket, child, Some(origin))).unwrap().state;
        assert_eq!(inherited(&state), [small.sequence, own.sequence]);
        let held: Vec<_> = state.extents.keys().map(|extent| extent.sequence).collect();
        assert_eq!(held, [small.sequence, own.sequence, mine.sequence]);
        // And one when the parent's uploader stops, the store closing.
        let parent_cell = parent.cell();
        drop(parent);
        let prefix = bucket_cell::layout::checkpoints(parent_cell);
        let listed = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, &prefix));
        assert_eq!(listed.unwrap().len(), 2);
    }

    /// TASK-79.39: a fork's child takes its cell through its marker, which names the fork's
    /// commit, the last the child inherits, though a later commit's segment is acknowledged with
    /// the fork's.
    #[test]
    fn forks_name_their_commit_though_later_ones_are_acknowledged_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), ""));
        let parent = Arc::new(
            kept_in(dir.path().join("parent"), (1 << 30, None), &bucket.clone(), idle()).unwrap(),
        );
        let cell = parent.cell();
        write(&parent, 1, &pattern(1, BLOCK));
        parent.commit(Some(b"root")).unwrap();
        // The fork's segment, after the commit's, lands after the next.
        let segment = |number| bucket_cell::layout::segment(cell, number, 1);
        let (forking, next) = (segment(1), segment(2));
        gated.hold.send_replace(Hold::Key(forking));
        let child = crate::host_cell::fork_child(cell);
        let fork = {
            let (parent, path) = (parent.clone(), dir.path().join("child"));
            std::thread::spawn(move || parent.fork_to(path, child))
        };
        gated.wait_held(1);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let later = {
            let parent = parent.clone();
            std::thread::spawn(move || {
                write(&parent, 2, &pattern(2, BLOCK));
                parent.commit(None)
            })
        };
        let landed =
            || futures::executor::block_on(bucket_cell::Bucket::head(&*bucket, &next)).is_ok();
        while !landed() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(landed(), "the commit after the fork never uploaded");
        gated.hold.send_replace(Hold::Nothing);
        assert_eq!(later.join().unwrap(), Ok(()));
        fork.join().unwrap().unwrap();

        let forked = reopened(dir.path().join("child"), idle()).unwrap();
        assert_eq!(forked.root(), Ok(b"root".to_vec()));
    }

    /// TASK-150.6: an extent's ID is its cell's to write once, on any host: one sealed and freed
    /// is spent on a host that takes the cell, whose local index knows nothing of it, whether
    /// the cell's state there comes from a checkpoint or from its segments' footers; and one
    /// freed while open, never sealed, as a journal's tail is, is written still.
    #[test]
    fn extents_freed_are_spent_on_the_host_that_takes_their_cell() {
        for checkpointed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            let first = kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle())
                .unwrap();
            let cell = first.cell();
            let sealed = write(&first, 1, &pattern(1, BLOCK));
            first.commit(Some(b"root")).unwrap();
            first.free(sealed).unwrap();
            let open = write(&first, 2, &pattern(2, BLOCK));
            first.free(open).unwrap();
            first.commit(None).unwrap();
            // The store closing checkpoints the cell; a host lost doesn't.
            if checkpointed {
                drop(first);
            } else {
                std::mem::forget(first);
            }

            let taken =
                kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle()).unwrap();
            let data = pattern(3, BLOCK);
            let again = [crate::VolatileSlice::from(&data[..])];
            assert_eq!(taken.write(sealed, 0, &again), Err(crate::Errno::EPERM), "{checkpointed}");
            assert_eq!(taken.write(open, 0, &again), Ok(()), "{checkpointed}");
        }
    }

    /// TASK-165.3: an extent sealed by one commit and freed by the next, both in one segment, whose
    /// footer frees it and seals it not, is spent on a host that takes the cell, as it is on the
    /// store that made it: every reader of a footer spends every free of the cell's own extents.
    #[test]
    fn extents_sealed_and_freed_in_one_segment_are_spent_where_their_cell_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let first =
            kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = first.cell();
        let sealed = write(&first, 1, &pattern(1, BLOCK));
        first.commit_ordered(Some(b"root")).unwrap();
        first.free(sealed).unwrap();
        first.commit(None).unwrap();
        let footers = footers(&bucket, cell);
        let last = footers.last().unwrap();
        let freed = storage_device::ExtentId::new(cell.id, sealed.sequence);
        let contents = &last.contents;
        let (commits, extents) = (contents.commits.len(), contents.extents.len());
        assert_eq!((commits, extents, &contents.frees), (1, 0, &vec![freed]));
        let data = pattern(3, BLOCK);
        let again = [crate::VolatileSlice::from(&data[..])];
        assert_eq!(first.write(sealed, 0, &again), Err(crate::Errno::EPERM));
        // Its host lost: the cell's state is its footers'.
        std::mem::forget(first);

        let taken =
            kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle()).unwrap();
        assert_eq!(taken.write(sealed, 0, &again), Err(crate::Errno::EPERM));
    }

    /// TASK-165.3: a cell's capacity is its state, set by the commit after a resize: a host that
    /// takes the cell has it, not the capacity it made its store with, whether the cell's state
    /// there comes from a checkpoint or from its segments' footers; and a store attaching to a
    /// cell that has none, or another, sets its own with its next commit.
    #[test]
    fn resizes_hold_where_their_cell_is_taken() {
        for checkpointed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            let first = kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle())
                .unwrap();
            let cell = first.cell();
            let recovered = || {
                let recovered = recover(&*bucket, cell, None);
                futures::executor::block_on(recovered).unwrap().state.capacity
            };
            assert_eq!(recovered(), None, "{checkpointed}");
            first.commit(None).unwrap();
            assert_eq!(recovered(), Some(1 << 30), "{checkpointed}");
            write(&first, 1, &pattern(1, BLOCK));
            first.set_capacity(3 << 29).unwrap();
            first.commit(None).unwrap();
            assert_eq!(recovered(), Some(3 << 29), "{checkpointed}");
            if checkpointed {
                drop(first);
            } else {
                std::mem::forget(first);
            }

            let taken =
                kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                    .unwrap();
            assert_eq!(taken.info().unwrap().capacity, 3 << 29, "{checkpointed}");
            taken.set_capacity(1 << 31).unwrap();
            taken.commit(None).unwrap();
            assert_eq!(recovered(), Some(1 << 31), "{checkpointed}");
        }
    }

    /// TASK-141.2: a cell whose record says a format newer than this daemon's is refused, naming
    /// the cell and the format, before anything of it is read or written: no store takes it, its
    /// head is as it was, and no segment of it is written.
    #[test]
    fn cells_of_newer_formats_are_refused() {
        use bucket_cell::Bucket as _;
        use bucket_cell::bucket::PutMode;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let first =
            kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = first.cell();
        write(&first, 1, &pattern(1, BLOCK));
        first.commit(Some(b"root")).unwrap();
        drop(first);
        // A later release's record, whose format says first what follows, which this one can't
        // read.
        let mut newer = (bucket_cell::layout::FORMAT + 1).to_le_bytes().to_vec();
        newer.extend_from_slice(b"what a later format holds");
        let (key, overwrite) = (bucket_cell::layout::head(cell), PutMode::Overwrite);
        futures::executor::block_on(bucket.put(&key, newer.into(), overwrite)).unwrap();
        let listed = || {
            let listed = futures::executor::block_on(bucket.list("")).unwrap();
            listed.into_iter().map(|meta| (meta.key, meta.version)).collect::<Vec<_>>()
        };
        let before = listed();

        let taken = kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle());
        let refused = taken.err().unwrap();
        let format = bucket_cell::layout::FORMAT;
        let named = format!(
            "cell {cell} is of format {}, which this reader, of format {format}, doesn't \
             read",
            format + 1
        );
        assert!(format!("{refused:#}").contains(&named), "{refused:#}");
        assert_eq!(refused.errno(), Some(crate::Errno::ENOTSUP));
        assert_eq!(listed(), before, "a cell of a newer format was written to");
    }

    /// Forks `parent`, attached to `bucket`, to `child`, whose handle is at `path`, the parent
    /// holding an extent and a root: returns the child's record.
    fn forked(
        parent: &OpenCell,
        bucket: &Arc<ObjectBucket>,
        path: &std::path::Path,
        child: crate::CellId,
    ) -> bucket_cell::layout::Head {
        write(parent, 1, &pattern(1, BLOCK));
        parent.commit(Some(b"root")).unwrap();
        parent.fork_to(path, child).unwrap();
        futures::executor::block_on(bucket_cell::head::get(&**bucket, child)).unwrap().unwrap()
    }

    /// TASK-79.78: a fork to a child it made before, as a fork retried to another path, fails
    /// having made nothing, the bucket having the child's record: no commit of the parent's names
    /// the child again, from another version, which its writers, finding the child forked twice,
    /// would fail on for ever, the parent's cell taken by none again. The parent attaches again.
    #[test]
    fn forks_to_a_child_forked_before_make_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let parent =
            kept_in(dir.path().join("parent"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let child = crate::host_cell::fork_child(parent.cell());
        forked(&parent, &bucket, &dir.path().join("child"), child);
        let again = dir.path().join("again");
        let forked = parent.fork_to(&again, child);
        let errno = forked.as_ref().map(drop).map_err(crate::Error::errno);
        let forked = forked.map_err(|err| err.to_string());
        assert_eq!((forked, again.exists()), (anothers(child), false));
        assert_eq!(errno, Err(Some(crate::Errno::EEXIST)));
        drop(parent);

        reopened(dir.path().join("parent"), idle()).unwrap();
    }

    /// The host whose disk is at `from`, crashed, restarted with its disk as the crash left it at
    /// `to`: its stores copied there, no store of them open. What the crashed host's writes
    /// had in flight may land still, as a dead host's PUTs may.
    ///
    /// Copied again if a file it listed went before it copied it, renamed or removed by the
    /// host's stores meanwhile, as they go on: a crash leaves a renamed file under one name or the
    /// other, where a copy that listed it under the first would have neither. The stores rename
    /// files on threads of their own, framings made durable and the next segment's log placed
    /// ahead of it (TASK-384), which a copy raced.
    pub(super) fn restart(from: &std::path::Path, to: &std::path::Path) {
        for _ in 0..100 {
            match copy_dir(from, to) {
                Ok(()) => return,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::remove_dir_all(to).unwrap();
                }
                Err(err) => panic!("copying {}: {err}", from.display()),
            }
        }
        panic!("copying {}: its files went as it was copied, again and again", from.display());
    }

    /// Copies the directory `from` to `to`, every file in it as it is.
    fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let (from, to) = (entry.path(), to.join(entry.file_name()));
            match entry.file_type()?.is_dir() {
                true => copy_dir(&from, &to)?,
                false => drop(std::fs::copy(&from, &to)?),
            }
        }
        Ok(())
    }

    /// A fork, waiting in a thread of its own: what it gives.
    type Forking = std::thread::JoinHandle<Result<(), crate::Error>>;

    /// What made the cell of `head`, and whether it lives: as the tests that are not of owners,
    /// keys or takings compare heads.
    fn made(head: bucket_cell::layout::Head) -> (bucket_cell::record::Made, bool) {
        (head.made, !head.deleted())
    }

    /// What [`made`] says of a cell no fork made, its store's own, of no parent, live.
    const OWN: (bucket_cell::record::Made, bool) = (bucket_cell::record::Made::Root, true);

    /// The store of another cell, `cell`, at `path`, attached to `bucket`, holding an extent
    /// and a root: what a guest may name as its fork's child.
    fn others(path: &std::path::Path, bucket: &Arc<ObjectBucket>, cell: crate::CellId) -> OpenCell {
        let others = kept_in(path, (1 << 30, Some(cell)), &bucket.clone(), idle()).unwrap();
        write(&others, 1, &pattern(3, BLOCK));
        others.commit(Some(b"other")).unwrap();
        others
    }

    /// What a store committing, and its commit uploading, gave, and what its cell then holds.
    type Committed = (Result<(), crate::Errno>, Result<(), crate::Errno>, Uploaded);

    /// Whether the cell of `others`, its store, is live still: it commits, and uploads its
    /// commits.
    fn live(others: &OpenCell, bucket: &Arc<ObjectBucket>) -> Committed {
        write(others, 2, &pattern(4, BLOCK));
        let committed = others.commit(Some(b"again"));
        (committed, others.wait_uploaded(), uploaded(bucket, others.cell()))
    }

    /// What [`live`] gives for a cell live still.
    fn lives() -> Committed {
        let extents = vec![(1, pattern(3, BLOCK)), (2, pattern(4, BLOCK))];
        (Ok(()), Ok(()), (extents, Some(b"again".to_vec())))
    }

    /// TASK-79.82: a fork to a child whose ID another cell has in object storage, as a guest may
    /// name, fails with `EEXIST`, having made nothing: no footer of the parent's names the child,
    /// whose record the parent's writers would find another's, failing on it for ever, and the
    /// other cell is left as it was.
    #[test]
    fn forks_to_a_cell_the_bucket_has_fail_with_eexist() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let parent =
            kept_in(dir.path().join("parent"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let other = crate::host_cell::fork_child(parent.cell());
        let others = others(&dir.path().join("other"), &bucket, other);
        write(&parent, 1, &pattern(1, BLOCK));
        parent.commit(Some(b"root")).unwrap();
        let path = dir.path().join("child");
        let forked = parent.fork_to(&path, other).map_err(|err| err.errno());
        assert_eq!((forked, path.exists()), (Err(Some(crate::Errno::EEXIST)), false));

        // The parent commits and uploads on, its cell naming no fork.
        parent.commit(Some(b"later")).unwrap();
        assert_eq!(parent.wait_uploaded(), Ok(()));
        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, other)).unwrap();
        assert_eq!((record.map(made), live(&others, &bucket)), (Some(OWN), lives()));
    }

    /// A parent attached to the bucket through `gated`, in `dir`, holding an extent and a root,
    /// and its fork to a child whose handle is `path`, waiting in a thread of its own for the swap
    /// of the parent's head that names the fork's segment, which `gated` holds: the parent's
    /// cell and the child.
    fn forking_held(
        dir: &std::path::Path,
        gated: &Arc<Gated>,
        path: &std::path::Path,
    ) -> (crate::CellId, crate::CellId, Forking) {
        let parent = Arc::new(
            kept_in(
                dir.join("parent"),
                (1 << 30, None),
                &Arc::new(ObjectBucket::new(gated.clone(), "")),
                idle(),
            )
            .unwrap(),
        );
        let cell = parent.cell();
        write(&parent, 1, &pattern(1, BLOCK));
        parent.commit(Some(b"root")).unwrap();
        gated.hold.send_replace(Hold::Key(bucket_cell::layout::head(cell)));
        let child = crate::host_cell::fork_child(cell);
        let fork = {
            let path = path.to_owned();
            std::thread::spawn(move || parent.fork_to(path, child))
        };
        gated.wait_held(1);
        (cell, child, fork)
    }

    /// Why a fork to `child` fails whose parent's writer, giving the child its record, found it
    /// another cell's.
    fn anothers(child: crate::CellId) -> Result<(), String> {
        Err(format!("forking to cell {child}: another cell has its ID"))
    }

    /// TASK-79.82: a fork whose child's ID another cell took in object storage while the fork
    /// was being made, once it found the ID free, as a store of that cell made elsewhere then,
    /// fails, its parent's writer finding the child's record another's: and its cleanup deletes
    /// the child here alone, leaving the other cell as it was. A fork deletes no cell it did
    /// not make, whatever ID its guest gives. And it harms no parent: its writers go on, as a
    /// writer taking its cell again does, its footer naming a child that is another's.
    #[test]
    fn forks_failed_leave_cells_that_took_their_childs_id_be() {
        let dir = tempfile::tempdir().unwrap();
        let memory: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let gated = Arc::new(Gated::over(memory.clone()));
        let bucket = Arc::new(ObjectBucket::new(memory, ""));
        let path = dir.path().join("child");
        let (_, child, fork) = forking_held(dir.path(), &gated, &path);
        let others = others(&dir.path().join("other"), &bucket, child);
        gated.hold.send_replace(Hold::Nothing);
        let forked = fork.join().unwrap();
        let errno = forked.as_ref().map(drop).map_err(crate::Error::errno);
        let forked = forked.map_err(|err| err.to_string());
        assert_eq!((forked, errno), (anothers(child), Err(Some(crate::Errno::EEXIST))));
        assert!(!path.exists(), "the handle of a fork that failed is left");

        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, child)).unwrap();
        assert_eq!((record.map(made), live(&others, &bucket)), (Some(OWN), lives()));
        let parent = reopened(dir.path().join("parent"), idle()).unwrap();
        parent.commit(Some(b"later")).unwrap();
        assert_eq!(parent.wait_uploaded(), Ok(()));
    }

    /// A store of a cell taken elsewhere keeps what it reads from the bucket in the host's cache,
    /// and reads it there next.
    #[test]
    fn reads_keep_what_they_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(memory.clone(), ""));
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), 1 << 30, 0).unwrap());
        let options = local_cell::upload::Options { cache: Some(cache.clone()), ..idle() };
        let first =
            kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle()).unwrap();
        let cell = first.cell();
        let data = pattern(2, 2 << 20);
        let extent = write(&first, 1, &data);
        first.commit(None).unwrap();
        drop(first);

        let second =
            kept_in(dir.path().join("second"), (1 << 30, Some(cell)), &bucket.clone(), options)
                .unwrap();
        let read = || {
            let mut read = vec![0; 2 << 20];
            second.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])]).unwrap();
            read
        };
        assert_eq!(read(), data);
        // Its parts, as they are stored: compressed.
        assert_ne!(cache.bytes(), 0);
        assert!(cache.bytes() < (2 << 20) / 100, "{} bytes kept", cache.bytes());

        // With the cell's segments gone from the bucket, its data is read from the cache.
        futures::executor::block_on(async {
            use futures::StreamExt as _;
            use object_store::{ObjectStore as _, ObjectStoreExt as _};
            let prefix = object_store::path::Path::from("segments");
            for meta in memory.list(Some(&prefix)).collect::<Vec<_>>().await {
                memory.delete(&meta.unwrap().location).await.unwrap();
            }
        });
        assert_eq!(read(), data);
    }
}
