// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what a host keeps its cells in object storage with (backlog doc-7;
//! TASK-28.5): its configuration ([`Host`], [`host_token`]), the runtime its open cells' uploaders
//! and fetches run on and their budgets ([`runtime`], [`HostFetcher`]), and the errno of each
//! failure ([`errno`]). The uploader is local_cell's (`local_cell::upload`, TASK-165.9).
//!
//! cladefs: and its KMS ([`kms`], TASK-411.3), which every process of the host's opening open cells
//! chooses alike, the extent daemon, the cladefs CLI's mounts and `cladefsctl`
//! (`bucket_cell::bucket_kms::from_env`): the one `CLADEFS_KMS` names (doc-16: `local:///DIR`,
//! OpenBao, AWS KMS, Google Cloud KMS); else, given a master key (`CLADEFS_MASTER_KEY` or
//! `CLADEFS_MASTER_KEY_FILE`), the bucket KMS, tenants' KEKs kept in the host's bucket
//! (`CLADEFSD_BUCKET`), made on each tenant's first use (TASK-411.27); else none, which every
//! such process refuses to start with ([`require_kms`], TASK-411.28): every cell is encrypted.

use crate::Errno;
use bucket_cell::bucket_kms::{BucketKms, ByBucket, Unreloaded};
use bucket_cell::cache::Cache;
use bucket_cell::fetch::{self, Budget, Limits, Priority};
#[cfg(not(madsim))]
use bucket_cell::identity::{Details, Identity, Registry};
use bucket_cell::keys;
use bucket_cell::layout::Location;
use bucket_cell::object_bucket::{ObjectBucket, RoutedBucket};
use local_cell::upload::{Failure, Runtime, UPLOAD_BYTES};
use std::sync::Arc;
use storage_device::ExtentId;

/// The fencing token of this host's placement, `CLADEFSD_TOKEN`, which the system's control
/// plane gives it, and which the copies of open cells here that were never placed ([`place`]) take
/// their cells with, as open cells restored or taken here are. Not 0, which would fence nothing:
/// a host without one keeps no cell (TASK-45).
pub fn host_token() -> Result<u64, Failure> {
    let missing = || {
        Failure::msg(
            "CLADEFSD_TOKEN names no placement token: this host's placement's, from the \
             system's control plane, which fences earlier placements",
        )
    };
    match node_local::env::var("CLADEFSD_TOKEN") {
        Ok(token) if !token.is_empty() => match token.parse() {
            Ok(0) => Err(missing()),
            Ok(token) => Ok(token),
            Err(err) => Err(Failure::local(err, format!("CLADEFSD_TOKEN {token:?}"))),
        },
        _ => Err(missing()),
    }
}

/// This host, as one of cladefs's multi-node system, the only kind there is (TASK-45): the bucket
/// its cells are kept in, and its placement's fencing token, as the environment names
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
        let bucket = crate::host_cell::host_bucket_url().ok_or_else(|| {
            "CLADEFSD_BUCKET names no bucket: a host of cladefs's multi-node system keeps its \
             cells in one (tests and development: memory:///NAME)"
                .to_owned()
        })?;
        let token = host_token().map_err(|err| err.to_string())?;
        Ok(Self { bucket, token })
    }
}

/// cladefs: `details` of the bucket at `url`, its URL and provider among them, as the registry of
/// buckets records them (`bucket_cell::identity`, TASK-633.9).
#[cfg(not(madsim))]
fn located(url: &str, details: Details) -> Result<Details, crate::Error> {
    let provider = bucket_cell::object_bucket::provider_of(url)
        .map_err(|err| crate::Error::new(anyhow::anyhow!(err)))?;
    Ok(Details { provider: Some(provider.to_owned()), url: Some(url.to_owned()), ..details })
}

/// cladefs: runs `work` on the bucket runtime, its bucket failure an error with its errno.
#[cfg(not(madsim))]
fn on_buckets<T>(
    work: impl std::future::Future<Output = Result<T, bucket_cell::Error>>,
) -> Result<T, crate::Error> {
    Ok(local_cell::upload::block_on(work)?.map_err(Failure::from)?)
}

/// cladefs: formats the bucket at `url` (`cladefsctl format-bucket`; `bucket_cell::identity`,
/// TASK-633.9): unless it was formatted or added, the root of buckets of its own, number 1, its
/// registry recording it, with `details`. Its identity either way.
#[cfg(not(madsim))]
pub fn format_bucket(url: &str, details: Details) -> Result<Identity, crate::Error> {
    let details = located(url, details)?;
    let bucket = crate::host_cell::bucket(url).map_err(crate::Error::new)?;
    on_buckets(bucket_cell::identity::format(&*bucket, &details))
}

/// cladefs: adds the bucket at `url`, with `details`, to the buckets of the root bucket at
/// `root_url` (`cladefsctl add-bucket`, TASK-633.9): the next number, by a conditional write of the
/// root's registry, which then goes to every bucket's root. Its identity: refused if it is a
/// bucket already (`EEXIST`), or the root is another bucket (`EINVAL`), or no number is left
/// (`ENOSPC`), or it is a memory bucket, its process's alone (`EINVAL`). cladefs: each bucket
/// opened with its own number's options, its credentials (`CLADEFSD_BUCKET_<N>_*`), the new one
/// with the number it is to get (board 5550); `EAGAIN` if another addition took that number
/// first, added under the next, which an addition again finishes.
#[cfg(not(madsim))]
pub fn add_bucket(root_url: &str, url: &str, details: Details) -> Result<Identity, crate::Error> {
    let (root_details, details) = (located(root_url, Details::default())?, located(url, details)?);
    // A memory bucket is its process's alone: no other host, nor a later process, reaches it at
    // its URL, as the registry records it (TASK-633.3).
    if details.provider.as_deref() == Some("memory") {
        let refused = anyhow::anyhow!("{url}: a memory bucket is one process's, none to add");
        return Err(crate::Error::new(refused.context(Errno::EINVAL)));
    }
    use bucket_cell::identity::{self, ROOT};
    let open =
        |url: &str, number| crate::host_cell::bucket_of(url, number).map_err(crate::Error::new);
    let root = open(root_url, ROOT)?;
    let found = on_buckets(identity::registry(&*root))?;
    let number = identity::number_for(found.as_ref(), url).map_err(Failure::from)?;
    let new = open(url, number)?;
    let (identity, registry) = on_buckets(identity::add(&*root, &root_details, &*new, details))?;
    if identity.number != number {
        let raced = anyhow::anyhow!(
            "{url}: added as bucket {}, another addition taking {number} first: give it its \
             CLADEFSD_BUCKET_{}_* and add it again, which finishes it",
            identity.number,
            identity.number
        );
        return Err(crate::Error::new(raced.context(Errno::EAGAIN)));
    }
    for entry in &registry.buckets {
        if let Some(url) = &entry.details.url {
            on_buckets(identity::spread_to(&registry, entry, &*open(url, entry.number)?))?;
        }
    }
    Ok(identity)
}

/// cladefs: the registry of buckets in the root of the bucket at `url`, if it has one
/// (`cladefsctl buckets`, TASK-633.9).
#[cfg(not(madsim))]
pub fn bucket_registry(url: &str) -> Result<Option<Registry>, crate::Error> {
    let bucket = crate::host_cell::bucket(url).map_err(crate::Error::new)?;
    on_buckets(bucket_cell::identity::registry(&*bucket))
}

/// cladefs: the storage-version ratchet gates in the root of the bucket at `url`, if any
/// (`cladefsctl formats`, doc-28).
#[cfg(not(madsim))]
pub fn bucket_formats(url: &str) -> Result<Option<formats::Gates>, crate::Error> {
    let bucket = crate::host_cell::bucket(url).map_err(crate::Error::new)?;
    on_buckets(bucket_cell::gates::get(&*bucket))
}

/// cladefs: monotonically advances the storage-version ratchet gates in the bucket at `url`
/// (`cladefsctl ratchet`, doc-28). When `url` is the root bucket of a multi-bucket registry,
/// spreads the updated gates to every member bucket as well.
#[cfg(not(madsim))]
pub fn ratchet_bucket(
    url: &str,
    storage: Option<u32>,
    fxfs: Option<u32>,
) -> Result<(formats::Gates, bool), crate::Error> {
    let bucket = crate::host_cell::bucket(url).map_err(crate::Error::new)?;
    let (gates, changed) = on_buckets(bucket_cell::gates::ratchet(&*bucket, storage, fxfs))?;
    if let Some(registry) = on_buckets(bucket_cell::identity::registry(&*bucket))? {
        for entry in &registry.buckets {
            if let Some(member_url) = &entry.details.url
                && member_url != url
            {
                let member = crate::host_cell::bucket_of(member_url, entry.number)
                    .map_err(crate::Error::new)?;
                on_buckets(bucket_cell::gates::spread(gates, &*member))?;
            }
        }
    }
    Ok((gates, changed))
}

/// cladefs: the bucket numbered `number`, as this host reaches it from its own (`CLADEFSD_BUCKET`):
/// by the registry of buckets in its root, or the root bucket's, opened with its own options and
/// checked to be the one recorded (TASK-633.3). `ENOENT` if no registry records it, `ESTALE` if the
/// bucket at its URL is another.
#[cfg(not(madsim))]
pub async fn bucket_numbered(number: u16) -> Result<Arc<ObjectBucket>, crate::Error> {
    let home = Host::from_env().map_err(|err| crate::Error::new(anyhow::anyhow!(err)))?.bucket;
    Ok(crate::host_cell::bucket_numbered(&home, number).await?)
}

/// The errno of `failure`, as doc-4 and doc-7 have them: a cell this writer lost, to another
/// writer or deleted, or one nothing writes, is one the open cell takes no more commits for
/// (`EROFS`); one a later placement holds, which a placement cut off takes back no more, or one
/// another writer moved on from this host's write-ahead log, which the open cell opens no more
/// until `cladefsctl take` drops it, `ESTALE`; a cell, or a version, gone, `ENOENT`; a fork's
/// child's ID taken, `EEXIST`; a cell of a newer format, `ENOTSUP`. cladefs: a cell
/// gone, `ENOENT`; a cell's name taken, `EEXIST`, or no cell's, `EINVAL` (TASK-364); a fork of
/// another tenant's version not shared, `EACCES`, and sharing what is no version, `EINVAL`
/// (TASK-395.7). None for
/// what the open cell failed to read or write, here or in object storage, which is the caller's
/// `EIO`, logged.
pub fn errno(failure: &Failure) -> Option<crate::Errno> {
    use crate::Errno;
    use bucket_cell::Error;
    match failure {
        Failure::Bucket(Error::Fenced | Error::Deleted) => Some(Errno::EROFS),
        Failure::Bucket(Error::Stale | Error::Diverged) => Some(Errno::ESTALE),
        Failure::Bucket(Error::NoCell) | Failure::VersionDropped => Some(Errno::ENOENT),
        Failure::Bucket(Error::IdTaken(_) | Error::OtherKind(_)) => Some(Errno::EEXIST),
        Failure::Bucket(Error::NotShared(_)) => Some(Errno::EACCES),
        // cladefs: an ephemeral fork of a host decommissioned, its incarnation's references
        // retired, which forks no more until started of a new one (board 8519).
        Failure::Bucket(Error::Decommissioned(_)) => Some(Errno::ESTALE),
        // cladefs: a bucket refusing the host's credentials, for good, or a request of its own, or
        // answering what this build can't read: never retried (boards 5861, 5534).
        Failure::Bucket(Error::Refused(refused)) => Some(match refused.refusal {
            bucket_cell::bucket::Refusal::Denied => Errno::EACCES,
            bucket_cell::bucket::Refusal::Request | bucket_cell::bucket::Refusal::Unreadable => {
                Errno::EIO
            }
        }),
        // cladefs: a tenant's KEK the KMS refused, revoked, destroyed, or unreached: its open cells
        // open no more until it is given back (TASK-411.3).
        Failure::Bucket(Error::Kms(_)) => Some(Errno::EACCES),
        // cladefs: an encrypted cell inherited by one whose record holds not its keys (TASK-411.4).
        Failure::Bucket(Error::Inherited { .. }) => Some(Errno::ENOTSUP),
        // A cell, or a bucket, of a newer format than this host reads, until it is upgraded
        // (doc-11).
        // cladefs: read evidence of another format, which its reader skips (TASK-702.2).
        Failure::Bucket(
            Error::Format { .. }
            | Error::BucketFormat(_)
            | Error::EvidenceFormat(_)
            | Error::FormatGated { .. },
        ) => Some(Errno::ENOTSUP),
        Failure::Bucket(Error::Gates(err)) => Some(match err {
            formats::GateError::UnsupportedHeader(_)
            | formats::GateError::IncompatibleGate { .. }
            | formats::GateError::BeyondBuild { .. } => Errno::ENOTSUP,
            formats::GateError::CannotDowngrade { .. }
            | formats::GateError::Corrupt(_)
            | formats::GateError::TooLarge { .. } => Errno::EINVAL,
        }),
        // cladefs: a bucket to add that is a bucket already, buckets added to one not the root,
        // and no bucket number left (TASK-633.9).
        Failure::Bucket(Error::Formatted(_)) => Some(Errno::EEXIST),
        Failure::Bucket(Error::NotRoot(_)) => Some(Errno::EINVAL),
        Failure::Bucket(Error::NoNumbers) => Some(Errno::ENOSPC),
        // cladefs: a bucket no registry records, or one other than the registry says (TASK-633.3).
        Failure::Bucket(Error::NoBucket(_)) => Some(Errno::ENOENT),
        Failure::Bucket(Error::Mismatched { .. }) => Some(Errno::ESTALE),
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

/// Explicit per-host bucket, KMS, fetch/upload budget, peer, and member state (board 10962),
/// replacing the 8 `node_local!` statics previously in `crate::bucket`.
pub struct HostState {
    pooled: std::sync::OnceLock<Arc<keys::CachingKms>>,
    kms: std::sync::OnceLock<Result<HostKeys, String>>,
    budget: fetch::Budgets,
    memory: Arc<fetch::Memory>,
    uploads: std::sync::OnceLock<Arc<Budget>>,
    upload_memory: std::sync::OnceLock<u64>,
    peers: std::sync::OnceLock<(Arc<RoutedBucket>, Arc<peers::Peers>)>,
}

impl HostState {
    pub fn new(parts: Arc<bucket_cell::fetch::PartMemory>) -> Self {
        Self {
            pooled: std::sync::OnceLock::new(),
            kms: std::sync::OnceLock::new(),
            budget: fetch::Budgets::per_bucket(Limits { requests: 64, bytes: 64 << 20 }),
            memory: fetch::Memory::with_parts(parts),
            uploads: std::sync::OnceLock::new(),
            upload_memory: std::sync::OnceLock::new(),
            peers: std::sync::OnceLock::new(),
        }
    }

    fn uploads(&self) -> &Arc<Budget> {
        self.uploads.get_or_init(|| {
            let bytes = *self.upload_memory.get_or_init(|| UPLOAD_BYTES);
            Budget::new(Limits { requests: 64, bytes })
        })
    }

    pub fn keys(&self) -> &keys::HostRing {
        self.memory.keys()
    }
}

/// The runtime `host`'s open cells' uploaders run on, and the budget of their uploads and keys.
pub fn runtime_on(host: &crate::Host) -> Result<Runtime, Failure> {
    let state = host.bucket_state();
    let keys = keys::Host::new(kms_on(host)?, state.memory.keys().clone());
    Ok(Runtime { handle: bucket_runtime_on(host)?, uploads: state.uploads().clone(), keys })
}

/// The runtime the host's open cells' uploaders run on, and the budget of their uploads; cladefs:
/// and the host's keys, its KMS and those its fetchers share (TASK-411.3). Fails if the runtime
/// can't be made, a later call making it anew ([`bucket_runtime`]).
#[cfg(not(madsim))]
pub fn runtime() -> Result<Runtime, Failure> {
    runtime_on(crate::Host::process())
}

/// The bucket runtime for `host`.
pub fn bucket_runtime_on(host: &crate::Host) -> Result<tokio::runtime::Handle, Failure> {
    Ok(local_cell::upload::bucket_runtime_on(host.local())?.handle().clone())
}

/// The runtime every open cell's uploader, and its fetches, run on, and the calls from no runtime's
/// task (`local_cell::upload::bucket_runtime`, TASK-34, TASK-449's review): one for the process, so
/// that one runtime serves the connections to the host's buckets, whichever opened them. Fails if
/// it can't be made, a later call making it anew.
#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub fn bucket_runtime() -> Result<tokio::runtime::Handle, Failure> {
    bucket_runtime_on(crate::Host::process())
}

/// cladefs: `host`'s KMS, initialized on first use.
pub fn kms_on(host: &crate::Host) -> Result<Arc<keys::CachingKms>, Failure> {
    let state = host.bucket_state();
    if let Some(kms) = state.pooled.get() {
        return Ok(kms.clone());
    }
    let runtime = bucket_runtime_on(host)?;
    let kms = state.pooled.get_or_init(|| match host_keys_on(host) {
        Ok(keys) => host_kms(host, keys.kmses.clone(), runtime),
        Err(why) => host_kms(host, one(Arc::new(Unreached(why.clone()))), runtime),
    });
    Ok(kms.clone())
}

/// cladefs: the host's KMS, as its environment chooses it the first time it is asked for
/// (`bucket_cell::bucket_kms::from_env`: `CLADEFS_KMS`'s, else the bucket KMS over the host's
/// bucket given a master key, TASK-411.27), or [`set_kms`] set it before (TASK-411.3), its pools of
/// new cell KEKs refilled on the bucket runtime. None chosen, or one that can't be had (a bad
/// `CLADEFS_KMS`, a master key unreadable, or given with no host's bucket to keep tenants' KEKs
/// in), it is a KMS refusing every call, naming why: the host then opens no open cell and makes no
/// cell. Every process that opens open cells refuses to start so first ([`require_kms`],
/// TASK-411.28). Fails, keeping nothing, if the runtime its pools refill on can't be made
/// ([`bucket_runtime`], TASK-449's review).
#[cfg(not(madsim))]
pub fn kms() -> Result<Arc<keys::CachingKms>, Failure> {
    kms_on(crate::Host::process())
}

/// cladefs: whether `host` has a KMS ([`kms_on`]).
pub fn require_kms_on(host: &crate::Host) -> Result<(), String> {
    host_keys_on(host).as_ref().map(drop).map_err(|why| format!("the host's KMS: {why}"))
}

/// cladefs: whether the host has a KMS ([`kms`]): the refusal, naming `CLADEFS_KMS` and
/// `CLADEFS_MASTER_KEY`, if its environment chooses none, or why the one it chooses can't be had.
/// What every process that opens open cells asks as it starts, refusing to start with none
/// (TASK-411.28): cladefsd, cladefsctl, the cladefs CLI's mounts.
#[cfg(not(madsim))]
pub fn require_kms() -> Result<(), String> {
    require_kms_on(crate::Host::process())
}

/// cladefs: `host`'s bucket KMS, if [`kms_on`] is that.
pub fn bucket_kms_on(host: &crate::Host) -> Option<Arc<BucketKms<ObjectBucket>>> {
    // None had: the process refused to start (require_kms), but in a tool that needs none.
    host_keys_on(host).as_ref().ok().and_then(|keys| keys.bucket.clone())
}

/// cladefs: the host's bucket KMS, if [`kms`] is that (TASK-411.27): what shreds a tenant here
/// ([`shred_tenant`]).
#[cfg(not(madsim))]
pub fn bucket_kms() -> Option<Arc<BucketKms<ObjectBucket>>> {
    bucket_kms_on(crate::Host::process())
}

/// cladefs: `host`'s KMS ([`kms_on`]), chosen once: or why it has none.
fn host_keys_on(host: &crate::Host) -> &Result<HostKeys, String> {
    host.bucket_state().kms.get_or_init(|| {
        // cladefs: the environment read once, the variables KMSes read alone, which the host's
        // KMSes keep (board 8593).
        let vars = match extent_keys::kms::Vars::from_env() {
            Ok(vars) => vars.shared(),
            Err(why) => {
                log::error!("cladefs: the host's KMS: {why}");
                return Err(why);
            }
        };
        let own = || host_bucket_on(host).map(|bucket| bucket.own().clone());
        match bucket_cell::bucket_kms::from_vars(&vars, own) {
            Ok(chosen) => {
                let bucket = chosen.bucket().cloned();
                // cladefs: each other bucket's cells under that bucket's KMS (TASK-633.6), given the
                // host's bucket to reach them by; the host's alone without one. One named that
                // can't be had refuses the KMS: the host's alone, other buckets' cells would be
                // wrapped under it rather than their own bucket's (board 6908).
                let home = match crate::host_cell::host_bucket_url() {
                    None => None,
                    Some(_) => Some(host_bucket_on(host)?),
                };
                let (kmses, by_bucket): (Arc<dyn keys::Kmses>, _) = match home {
                    Some(home) => {
                        let by_bucket = Arc::new(ByBucket::new(home, chosen, vars));
                        (by_bucket.clone(), Some(by_bucket))
                    }
                    None => (one(chosen.shared()), None),
                };
                Ok(HostKeys { kmses, bucket, by_bucket })
            }
            // cladefs: this crate's own unit tests, given no KMS, encrypt under the tests' KMS, each
            // tenant's KEK made on its first use, which the tests' readers share
            // (`bucket_cell::keys::testing`, TASK-411.28).
            #[cfg(test)]
            Err(why) if why == bucket_cell::bucket_kms::no_kms() => Ok(HostKeys {
                kmses: one(bucket_cell::keys::testing::kms()),
                bucket: None,
                by_bucket: None,
            }),
            // cladefs: its integration tests, and the processes they start, given no KMS, encrypt under
            // the tests' KMS, every tenant's KEK one known key (`testing::encrypted`, TASK-411.28).
            #[cfg(all(feature = "tests-kms", not(test)))]
            Err(why) if why == bucket_cell::bucket_kms::no_kms() => Ok(testing::host_keys()),
            Err(why) => {
                log::error!("cladefs: the host's KMS: {why}");
                Err(why)
            }
        }
    })
}

/// cladefs: `host`'s bucket, which the bucket KMS keeps tenants' KEKs in (`CLADEFSD_BUCKET`).
fn host_bucket_on(host: &crate::Host) -> Result<Arc<RoutedBucket>, String> {
    let url = crate::host_cell::host_bucket_url().ok_or("CLADEFSD_BUCKET names none")?;
    crate::host_cell::bucket_on(host, &url).map_err(|err| format!("{url}: {err:#}"))
}

/// cladefs: the host's KMSes, a bucket's each (TASK-633.6), its own as its environment chose it,
/// and the bucket KMS that is over its own bucket, if it is that.
struct HostKeys {
    kmses: Arc<dyn keys::Kmses>,
    bucket: Option<Arc<BucketKms<ObjectBucket>>>,
    /// Its KMSes, if it has a bucket to reach others by: what shreds a tenant in every bucket.
    by_bucket: Option<Arc<ByBucket>>,
}

/// cladefs: `kms`, for every bucket's cells.
fn one(kms: extent_keys::kms::Shared) -> Arc<dyn keys::Kmses> {
    Arc::new(keys::OneKms(kms))
}

/// cladefs: `host`'s over `kmses`: its pools of new cell KEKs refilled in batches on `runtime`,
/// the uploaders', off the paths of forks, which take from them (TASK-411.4). Installs a
/// revocation callback so any KMS refusal immediately halts open cells on `host`.
fn host_kms(
    host: &crate::Host,
    kmses: Arc<dyn keys::Kmses>,
    runtime: tokio::runtime::Handle,
) -> Arc<keys::CachingKms> {
    let kms = keys::CachingKms::over(kmses, Some(keys::Pooling::on(runtime)));
    let weak = host.weak();
    kms.set_on_revoked(Arc::new(move |tenant, buckets, why| {
        if let Some(host) = weak.upgrade() {
            on_kms_revoked_on(&host, tenant, buckets, why);
        }
    }));
    kms
}

/// cladefs: invoked by [`keys::CachingKms`] whenever a KMS reports that `tenant`'s KEK is
/// revoked, destroyed, or missing: immediately halts open cells of `tenant` on `host` in the
/// affected buckets and evicts their extent-cache pages without waiting for external fan-out,
/// preserving multi-bucket per-KMS isolation (TASK-633.6).
fn on_kms_revoked_on(
    host: &crate::Host,
    tenant: u64,
    buckets: &std::collections::BTreeSet<u16>,
    why: &str,
) {
    let all_buckets = buckets.is_empty() || buckets.contains(&bucket_cell::identity::NAMED);
    let matches = |cell: crate::CellId| {
        cell.tenant == tenant
            && (all_buckets || buckets.contains(&bucket_cell::objectid::bucket_of(cell.id)))
    };
    let halted = format!("as tenant {tenant}'s keys were revoked by its KMS: {why}");
    let stores = crate::halt::halt_cells_on(host, matches, Errno::EACCES, &halted);
    invoke_forget_cells(&matches);
    let (bytes, extents) = host.cache().forget(matches);
    let state = host.bucket_state();
    let (rings, kept) =
        (state.memory.keys().forget_where(matches), state.memory.forget_kept_where(matches));
    if stores > 0 || bytes > 0 || rings > 0 || kept > 0 {
        log::warn!(
            "cladefs: tenant {tenant}'s KMS revoked keys ({why}): halted {stores} stores, \
             evicted {extents} extents ({bytes} bytes), dropped {rings} rings / {kept} kept"
        );
    }
}

/// cladefs: has `host` use `kms` as its KMS, before any open cell opens. Once.
#[cfg(not(madsim))]
pub fn set_kms_on(host: &crate::Host, kms: extent_keys::kms::Shared) -> Result<(), String> {
    host.bucket_state()
        .kms
        .set(Ok(HostKeys { kmses: one(kms), bucket: None, by_bucket: None }))
        .map_err(|_| "the host's KMS is set already".to_owned())
}

/// cladefs: has the host use `kms` as its KMS, before any open cell opens: tests' (TASK-411.3).
/// Once.
#[cfg(not(madsim))]
pub fn set_kms(kms: extent_keys::kms::Shared) -> Result<(), String> {
    set_kms_on(crate::Host::process(), kms)
}

/// cladefs: the KMS tests' and benches' processes encrypt under, their environment naming none
/// (TASK-411.28): every open cell is encrypted, and a process given no KMS opens none.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::HostKeys;
    use std::sync::Arc;

    /// Has this process's open cells encrypt under the tests' KMS, every tenant's KEK one known key
    /// (`extent_keys::testing::KnownKms`), unless its environment names a KMS (`CLADEFS_KMS`) or
    /// gives a master key (`CLADEFS_MASTER_KEY`, `CLADEFS_MASTER_KEY_FILE`): a bench's first step,
    /// which measures a host's work, a real host's KMS adding its calls. cladefs's tests have it
    /// whatever they do, and the processes they start (the `tests-kms` feature). Before any open
    /// cell opens.
    #[cfg(not(madsim))]
    pub fn encrypted() {
        let named = ["CLADEFS_KMS", "CLADEFS_MASTER_KEY", "CLADEFS_MASTER_KEY_FILE"]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
        if !named {
            let set = crate::Host::process().bucket_state().kms.set(Ok(host_keys()));
            assert!(set.is_ok(), "the tests' KMS, set before any cell opens");
        }
    }

    /// The host's keys under the tests' KMS, which every process has alike: a test's, and those it
    /// starts, whatever bucket each keeps its open cells in.
    pub(super) fn host_keys() -> HostKeys {
        let kmses = super::one(Arc::new(extent_keys::testing::KnownKms));
        HostKeys { kmses, bucket: None, by_bucket: None }
    }
}

/// Hook registered by `cladefs_fuse::server` so forgetting/shredding cells also halts and
/// invalidates any FUSE-mounted slots in this process (outside the simulation: FUSE mounts do not
/// run under madsim).
static FORGET_CELLS_HOOK: std::sync::OnceLock<fn(&dyn Fn(crate::CellId) -> bool)> =
    std::sync::OnceLock::new();

/// Registers the FUSE-mount forget callback called when cells or tenant keys are forgotten.
pub fn register_forget_cells(hook: fn(&dyn Fn(crate::CellId) -> bool)) {
    let _ = FORGET_CELLS_HOOK.set(hook);
}

fn invoke_forget_cells(scope: &dyn Fn(crate::CellId) -> bool) {
    if let Some(hook) = FORGET_CELLS_HOOK.get() {
        hook(scope);
    }
}

/// cladefs: what a host forgot of a tenant's or a cell's keys, told to ([`forget`], TASK-411.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// The open cells open of the scope's cells it halted, which serve nothing more (`EACCES`).
    pub stores: usize,
    /// The rings of the scope's open cells its fetchers held.
    pub rings: usize,
    /// The objects' tails and parts of the scope's cells its fetchers kept in memory.
    pub kept: usize,
    /// The extents it took out of the extent cache.
    pub extents: u64,
    /// The bytes of their pages it evicted.
    pub bytes: u64,
}

/// cladefs: has `host` forget what it holds of `scope`'s keys (doc-16, Shredding; TASK-411.7).
pub fn forget_on(host: &crate::Host, scope: keys::Scope, why: &str) -> Forgotten {
    let halted = format!("as {scope}'s keys are forgotten: {why}");
    let stores =
        crate::halt::halt_cells_on(host, |cell| scope.covers(cell), Errno::EACCES, &halted);
    // cladefs: halt any FUSE mount slots serving cells in `scope` and invalidate their kernel
    // inodes and dentries so cached file pages are dropped immediately (TASK-562, board 2520,
    // board 2704).
    invoke_forget_cells(&|cell| scope.covers(cell));
    let (bytes, extents) = host.cache().forget(|cell| scope.covers(cell));
    // Forget keys before bumping `forget_kept`'s generation so no concurrent read can re-open or
    // re-insert plaintext after `forget_kept` runs (board 6026).
    let state = host.bucket_state();
    let rings = state.memory.keys().forget(scope);
    let kept = state.memory.forget_kept(scope);
    // Its KMS, if it made it: one not made yet unwrapped nothing.
    if let Some(kms) = state.pooled.get() {
        kms.forget(scope, why);
    }
    let forgotten = Forgotten { stores, rings, kept, extents, bytes };
    log::warn!("cladefs: {scope}'s keys forgotten, {why}: {forgotten:?}");
    forgotten
}

/// cladefs: has the host forget what it holds of `scope`'s keys (doc-16, Shredding; TASK-411.7).
#[cfg(not(madsim))]
pub fn forget(scope: keys::Scope, why: &str) -> Forgotten {
    forget_on(crate::Host::process(), scope, why)
}

/// cladefs: strips wrapped KEKs from `cell`'s head in `bucket` and publishes `CellShredded` to the
/// root bucket's `events` object so every host forgets its keys without fan-out (board 11711).
async fn shred_cell_in_bucket(
    bucket: &RoutedBucket,
    cell: crate::CellId,
) -> Result<usize, bucket_cell::Error> {
    let removed = bucket_cell::keys::shred(bucket, cell).await?;
    let auth = bucket_cell::events::authority(bucket).await?;
    bucket_cell::events::publish(&*auth, bucket_cell::events::Publish::CellShredded(cell)).await?;
    Ok(removed)
}

/// cladefs: why [`shred_tenant`] refused or failed, distinguishing invalid input (`EINVAL`),
/// non-bucket KMS (`ENOTSUP`), and unreachable/failing buckets (`EIO`, board 5394).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShredError {
    /// The tenant ID is not valid for shredding (e.g., the default tenant 0).
    #[error("{0}")]
    InvalidTenant(String),
    /// The host or one of its buckets uses an external named KMS rather than the bucket KMS.
    #[error("{0}")]
    Unsupported(String),
    /// One or more buckets were unreachable or failed while shredding.
    #[error("{0}")]
    Failed(String),
}

/// cladefs: shreds `cell` in `bucket` on `host`, publishes
/// [`bucket_cell::events::Publish::CellShredded`] to the root bucket's `events` object so other
/// hosts forget its keys without fan-out, and has `host` forget its keys immediately (board 11711).
pub async fn shred_cell_on(
    host: &crate::Host,
    bucket: &RoutedBucket,
    cell: crate::CellId,
) -> Result<(usize, Forgotten), bucket_cell::Error> {
    let removed = shred_cell_in_bucket(bucket, cell).await?;
    let why = bucket_cell::keys::shredded(cell).to_string();
    Ok((removed, forget_on(host, keys::Scope::Cell(cell), &why)))
}

/// cladefs: shreds `cell` in `bucket` ([`bucket_cell::keys::shred`]): its wrapped KEKs taken out of
/// its head, by a conditional update; publishes [`bucket_cell::events::Publish::CellShredded`] to
/// the root bucket's `events` object so other hosts forget its keys without fan-out; then has this
/// host forget its keys ([`forget`]). Shredded already, its head stays as it is, and the host
/// forgets them all the same. Returns how many generations of its KEK it took out, and what the
/// host forgot. Only as strong as `bucket` keeps no older version of the cell's head (doc-16).
#[cfg(not(madsim))]
pub async fn shred_cell(
    bucket: &RoutedBucket,
    cell: crate::CellId,
) -> Result<(usize, Forgotten), bucket_cell::Error> {
    shred_cell_on(crate::Host::process(), bucket, cell).await
}

/// cladefs: evaluates a multi-bucket tenant shred result, shared between [`shred_tenant_on`] and
/// [`shred_tenant_bucket`] (board 11711): checks `unsupported`, `none destroyed`, and partial
/// failures; calls `on_destroyed` once at least one bucket tombstone was confirmed (board 5552).
fn finish_tenant_shred<T>(
    tenant: u64,
    shredded: &bucket_cell::bucket_kms::Shredded,
    unsupported: impl FnOnce() -> String,
    partial_suffix: &str,
    on_destroyed: impl FnOnce() -> T,
) -> Result<(bucket_cell::bucket_kms::Destroyed, T), ShredError> {
    use bucket_cell::bucket_kms::Destroyed;
    if shredded.destroyed.is_empty() && shredded.failed.is_empty() {
        return Err(ShredError::Unsupported(unsupported()));
    }
    // cladefs: the buckets it did not shred in (TASK-633.6): under another KMS, or out of reach.
    let left: Vec<String> = (shredded.named.iter())
        .map(|(number, url)| format!("bucket {number}'s KMS {url} keeps its KEK: destroy it there"))
        .chain(shredded.failed.iter().map(|(number, why)| format!("bucket {number}: {why}")))
        .collect();
    // cladefs: if no bucket tombstone was confirmed, do not halt open cells or poison the KMS
    // cache (board 5552, board 2704).
    if shredded.destroyed.is_empty() {
        return Err(ShredError::Failed(format!(
            "tenant {tenant}: shredded in no bucket: {}",
            left.join("; ")
        )));
    }
    let now = shredded.destroyed.iter().any(|(_, destroyed)| *destroyed == Destroyed::Now);
    let destroyed = if now { Destroyed::Now } else { Destroyed::Already };
    let extra = on_destroyed();
    if !left.is_empty() {
        let done: Vec<String> = shredded.destroyed.iter().map(|(n, _)| n.to_string()).collect();
        let message = format!(
            "tenant {tenant}: shredded in buckets [{}] alone: {}{partial_suffix}",
            done.join(", "),
            left.join("; ")
        );
        return Err(if !shredded.failed.is_empty() {
            ShredError::Failed(message)
        } else {
            ShredError::Unsupported(message)
        });
    }
    Ok((destroyed, extra))
}

/// cladefs: shreds `tenant` under `host`'s bucket KMS of every bucket `host` reaches (doc-16, The
/// bucket KMS; TASK-411.27, TASK-633.6), publishes `TenantShredded` to the root bucket's `events`
/// object, and forgets `tenant`'s keys on `host` once at least one bucket was tombstoned (board
/// 5552). Refuses the default tenant 0 (`EINVAL`), non-bucket KMS (`ENOTSUP`), and unreachable or
/// failing buckets (`EIO`, board 5394).
pub async fn shred_tenant_on(
    host: &crate::Host,
    tenant: u64,
) -> Result<(bucket_cell::bucket_kms::Destroyed, Forgotten), ShredError> {
    use bucket_cell::bucket_kms::Shredded;
    if tenant == bucket_cell::layout::DEFAULT_TENANT {
        return Err(ShredError::InvalidTenant("cannot shred the default tenant (0)".to_owned()));
    }
    // None had: the process refused to start (require_kms); nothing to shred under.
    let keys = host_keys_on(host).as_ref().ok();
    let shredded = match (keys.and_then(|keys| keys.by_bucket.as_ref()), bucket_kms_on(host)) {
        (Some(by_bucket), _) => by_bucket.shred(tenant).await,
        (None, Some(kms)) => {
            let mut shredded = Shredded::default();
            shredded.destroying(0, kms.destroy(tenant).await);
            // Publish TenantShredded to `events` on the single bucket too (board 11711).
            if !shredded.destroyed.is_empty()
                && let Err(err) = bucket_cell::events::publish(
                    &**kms.bucket(),
                    bucket_cell::events::Publish::TenantShredded(tenant),
                )
                .await
            {
                shredded.failed.push((0, format!("publishing events: {err}")));
            }
            shredded
        }
        (None, None) => Shredded::default(),
    };
    finish_tenant_shred(
        tenant,
        &shredded,
        || {
            format!(
                "tenant {tenant}: this host's KMS is not the bucket KMS (CLADEFS_MASTER_KEY, no \
                 CLADEFS_KMS): destroy the tenant's KEK in its KMS, then have every host forget \
                 its keys"
            )
        },
        "; then have every host forget its keys",
        || {
            let why = extent_keys::kms::Error::Destroyed { tenant }.to_string();
            forget_on(host, keys::Scope::Tenant(tenant), &why)
        },
    )
}

/// cladefs: shreds `tenant` under the bucket KMS of every bucket the host reaches (doc-16, The
/// bucket KMS; TASK-411.27, TASK-633.6).
#[cfg(not(madsim))]
pub async fn shred_tenant(
    tenant: u64,
) -> Result<(bucket_cell::bucket_kms::Destroyed, Forgotten), ShredError> {
    shred_tenant_on(crate::Host::process(), tenant).await
}

/// cladefs: standalone, keyless tenant shred directly against the bucket at `url` (and every
/// bucket in its registry), needing neither a running extent daemon nor `CLADEFS_MASTER_KEY`.
/// Tombstones `tenants/<tenant>/kek` in every bucket-KMS bucket and publishes `TenantShredded` to
/// the root bucket's `events` object so all extent daemons halt open cells and `cladefs-gc` reaps
/// the tenant's cells.
#[cfg(not(madsim))]
pub fn shred_tenant_bucket(
    url: &str,
    tenant: u64,
) -> Result<bucket_cell::bucket_kms::Destroyed, crate::Error> {
    use bucket_cell::bucket_kms::{SHRED_WITHIN, shred_buckets};
    if tenant == bucket_cell::layout::DEFAULT_TENANT {
        return Err(crate::Error::new(
            anyhow::anyhow!("cannot shred the default tenant (0)").context(Errno::EINVAL),
        ));
    }
    let vars = extent_keys::kms::Vars::from_env()
        .map_err(|why| crate::Error::new(anyhow::anyhow!("{why}").context(Errno::EINVAL)))?;
    let bucket = crate::host_cell::bucket(url).map_err(crate::Error::new)?;
    let shredded =
        local_cell::upload::block_on(shred_buckets(&bucket, &vars, tenant, SHRED_WITHIN))?;
    let (destroyed, ()) = finish_tenant_shred(
        tenant,
        &shredded,
        || {
            format!(
                "tenant {tenant}: {url} is not under the bucket KMS: destroy its KEK in its KMS \
                 and run `cladefs-gc {url} --delete-tenant {}`",
                base32_id::Short(tenant)
            )
        },
        "",
        || (),
    )
    .map_err(|err| {
        let errno = match &err {
            ShredError::InvalidTenant(_) => Errno::EINVAL,
            ShredError::Unsupported(_) => Errno::ENOTSUP,
            ShredError::Failed(_) => Errno::EIO,
        };
        crate::Error::new(anyhow::anyhow!("{err}").context(errno))
    })?;
    Ok(destroyed)
}

/// cladefs: standalone cell shred directly against the bucket at `url`, needing no running extent
/// daemon: strips wrapped KEKs from `cell`'s head and publishes `CellShredded` to `events`.
#[cfg(not(madsim))]
pub fn shred_cell_bucket(url: &str, cell: crate::CellId) -> Result<usize, crate::Error> {
    let bucket = crate::host_cell::bucket(url).map_err(crate::Error::new)?;
    on_buckets(async move { shred_cell_in_bucket(&bucket, cell).await })
}

/// cladefs: publishes a bucket-wide peer list update for the bucket at `url` to the root bucket's
/// `events` object so every extent daemon on that bucket converges without host fan-out. Returns
/// the new peer generation.
#[cfg(not(madsim))]
pub fn set_peers_bucket(url: &str, addresses: Vec<String>) -> Result<u64, crate::Error> {
    for address in &addresses {
        if !address.contains(':')
            || address
                .rsplit_once(':')
                .is_none_or(|(h, p)| h.is_empty() || p.parse::<u16>().is_err())
        {
            return Err(crate::Error::new(
                anyhow::anyhow!("peer address {address:?} must be HOST:PORT")
                    .context(Errno::EINVAL),
            ));
        }
    }
    let bucket = crate::host_cell::bucket(url).map_err(crate::Error::new)?;
    on_buckets(async move {
        let number = bucket_cell::events::bucket_number(&bucket).await?;
        let auth = bucket_cell::events::authority(&bucket).await?;
        let log = bucket_cell::events::publish(
            &*auth,
            bucket_cell::events::Publish::PeersUpdated { bucket: number, addresses },
        )
        .await?;
        Ok(log.peers_for(number).map_or(0, |p| p.generation))
    })
}

/// cladefs: reloads `host`'s live KMS configuration (board 8487).
pub async fn reload_kms_on(
    host: &crate::Host,
) -> Result<bucket_cell::bucket_kms::Reloaded, Unreloaded> {
    let unsupported = |why: String| Unreloaded::Unsupported(format!("the host's KMS: {why}"));
    let keys = host_keys_on(host).as_ref().map_err(|why| unsupported(why.clone()))?;
    let by_bucket = keys.by_bucket.as_ref().ok_or_else(|| {
        unsupported("reloads with a bucket of the host's own alone (CLADEFSD_BUCKET)".to_owned())
    })?;
    let reloaded = by_bucket.reload().await?;
    // Its KMS, if it made it: one not made yet pooled nothing.
    if let Some(kms) = host.bucket_state().pooled.get() {
        for &tenant in &reloaded.renamed {
            kms.renamed(tenant);
        }
    }
    Ok(reloaded)
}

/// cladefs: reloads what of the host's KMS configuration reloads live (board 8487; doc-16, What
/// reloads), as [`ByBucket::reload`] does, whole or not at all; then has its pools of new cell KEKs
/// of each tenant renamed dropped, wrapped under the old name as they are
/// ([`keys::CachingKms::renamed`]). Refused, changing nothing, by a host with no KMSes by bucket
/// (no `CLADEFSD_BUCKET`, or no KMS at all), which has no bucket to check renames against.
#[cfg(not(madsim))]
pub async fn reload_kms() -> Result<bucket_cell::bucket_kms::Reloaded, Unreloaded> {
    reload_kms_on(crate::Host::process()).await
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

/// cladefs: how many open cells' uploaders run on `host`, as it changes (TASK-141.4).
pub fn uploaders_on(host: &crate::Host) -> tokio::sync::watch::Receiver<usize> {
    local_cell::upload::uploaders_on(host.local())
}

/// cladefs: the budget of the GETs of `host`'s fetchers and the memory they keep in (doc-27).
pub fn fetching_on(host: &crate::Host) -> local_cell::upload::Fetching {
    let state = host.bucket_state();
    local_cell::upload::Fetching { budgets: state.budget.clone(), memory: state.memory.clone() }
}

/// cladefs: `host`'s fetcher memory, whose tails and unified recent/unpacked parts the memory
/// governor counts and evicts (board 6026).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn memory_on(host: &crate::Host) -> &Arc<fetch::Memory> {
    &host.bucket_state().memory
}

/// cladefs: has `host`'s uploads hold at most `bytes` of the objects they upload at once
/// (TASK-317).
pub fn set_upload_memory_on(host: &crate::Host, bytes: u64) -> Result<(), String> {
    if bytes < crate::OpenCell::MAX_EXTENT {
        return Err(format!(
            "an upload memory of {bytes} bytes: less than an extent's {}",
            crate::OpenCell::MAX_EXTENT
        ));
    }
    let state = host.bucket_state();
    match state.upload_memory.set(bytes) {
        Ok(()) => Ok(()),
        // Set already, to the same: as asked.
        Err(_) if state.upload_memory.get() == Some(&bytes) => Ok(()),
        Err(_) => Err("the upload memory is set already, as cells opened".to_owned()),
    }
}

/// cladefs: has the host's uploads hold at most `bytes` of the objects they upload at once
/// (TASK-317): cladefsd's `CLADEFSD_UPLOAD_MEMORY`. However far behind the bucket its open cells
/// fall, what waits for it is on the host's disk, their write-ahead logs, and what of it is in
/// memory is what this lets upload at once. Before any open cell opens; no less than the largest
/// object an open cell uploads, an extent's of 64 MiB, which goes alone.
#[cfg(not(madsim))]
pub fn set_upload_memory(bytes: u64) -> Result<(), String> {
    set_upload_memory_on(crate::Host::process(), bytes)
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

/// cladefs: what `host`'s uploads hold now, the most they held, and their budget (TASK-317).
pub fn upload_memory_on(host: &crate::Host) -> UploadMemory {
    let uploads = host.bucket_state().uploads();
    let (held, max_held) = uploads.held();
    UploadMemory { held, max_held, budget: uploads.limits().bytes }
}

/// cladefs: what the host's uploads hold now, the most they held, and their budget (TASK-317).
#[cfg(not(madsim))]
pub fn upload_memory() -> UploadMemory {
    upload_memory_on(crate::Host::process())
}

/// cladefs: what [`HostFetcher::read_following`] read: fetched bytes, or what the cell holds, found
/// again, the extent relocated into it.
pub enum Followed<T> {
    Fetched(Vec<u8>),
    Here(T),
}

/// cladefs: the most times a fetch looks its extent up again, its object found gone: each one is
/// another relocation acknowledged meanwhile ([`HostFetcher::read_following`]), which a fetch
/// outlives seldom (doc-27).
const RELOCATED_MOST: usize = 8;

/// Reads the extents an open cell holds whose data is in object storage
/// (`local_cell::LocalCell::adopt`), at their locations there, as the open cell names them: a
/// fetcher (`bucket_cell::fetch::Fetcher`) that is the host's, its reads on the bucket runtime,
/// which their uploads use too, under the host's budget of GETs and memory, asking the host's peers
/// first if it has any.
pub struct HostFetcher {
    runtime: tokio::runtime::Handle,
    parts: Arc<fetch::Fetcher<RoutedBucket>>,
}

/// cladefs: polls `future` outside tokio's cooperative budget so awaiting a `bucket_runtime`
/// `JoinHandle` inside `futures::executor::block_on` from a tokio task cannot livelock
/// (board 6525). Under madsim there is no tokio coop budget.
pub async fn unconstrained<F: std::future::Future>(future: F) -> F::Output {
    #[cfg(not(madsim))]
    {
        tokio::task::unconstrained(future).await
    }
    #[cfg(madsim)]
    {
        future.await
    }
}

impl HostFetcher {
    /// What reads extents' data from `bucket` on `host`, keeping the parts it fetches in `cache`,
    /// if given.
    pub fn new_on(
        host: &crate::Host,
        bucket: Arc<RoutedBucket>,
        cache: Option<Arc<Cache>>,
    ) -> Result<Arc<Self>, Failure> {
        let runtime = bucket_runtime_on(host)?;
        let runs = runtime.clone();
        // A read ahead is for the guest request whose read started it, if any (TASK-260).
        let spawn: fetch::Spawn = Arc::new(move |future| {
            drop(runs.spawn(crate::request::within(crate::request::current(), future)))
        });
        let state = host.bucket_state();
        let peers = state.peers.get().filter(|(theirs, _)| Arc::ptr_eq(theirs, &bucket));
        let parts = fetch::Fetcher::new(bucket, state.budget.clone(), state.memory.clone(), cache);
        let parts = match peers {
            Some((_, peers)) => parts.asking(peers.clone()),
            None => parts,
        };
        Ok(Arc::new(Self { runtime, parts: Arc::new(parts.reading_ahead(spawn)) }))
    }

    /// cladefs: the runtime its reads run on, which what fetches ahead for an open cell, and writes
    /// its guest's boot behind it, runs on too (doc-24, step 5; `crate::evidence`).
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
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
        // requests for it log (TASK-260); and in the trace context current, the parent of its
        // bucket requests' spans, gathering where it went (TASK-672.2).
        let parent = bucket_cell::trace::Parent::current();
        let reading = parent.around(async move {
            Ok(parts.read(extent, &location, offset, len, Priority::Demand).await?)
        });
        let read = self.runtime.spawn(crate::request::within(crate::request::current(), reading));
        // cladefs: `unconstrained` prevents tokio's thread-local coop budget from livelocking
        // when this future is polled inside `futures::executor::block_on` from a tokio task
        // (board 6525).
        async move { unconstrained(read).await.map_err(|err| Failure::local(err, "fetching"))? }
    }

    /// cladefs: `len` bytes at `offset` in `extent`, whose data was at `location` in the bucket,
    /// as many as it holds; or, its object found gone ([`bucket_cell::Error::Lost`]), what
    /// `again`, which looks the extent up where its cell holds it now, finds (doc-27, What happens
    /// to the old segment). Moved in the bucket, as `again` fails saying
    /// ([`local_cell::BucketData`]), it is fetched there; in the cell, a relocation not
    /// acknowledged yet, it is what `again` gives ([`Followed::Here`]). Its writer relocated it,
    /// and the bucket acknowledged that, before the collector could delete where it was, which no
    /// state the collector keeps reads any more. Where it was, it is lost, as said: a fetch's
    /// error is a [`Failure`], `again`'s its own.
    pub async fn read_following<T>(
        &self,
        extent: ExtentId,
        mut location: Location,
        (offset, len): (u64, u64),
        mut again: impl FnMut() -> anyhow::Result<T>,
    ) -> anyhow::Result<Followed<T>> {
        let mut lost = String::new();
        for _ in 0..RELOCATED_MOST {
            lost = match self.read(extent, location.clone(), offset, len).await {
                Ok(bytes) => return Ok(Followed::Fetched(bytes)),
                Err(Failure::Bucket(bucket_cell::Error::Lost(lost))) => lost,
                Err(failure) => return Err(failure.into()),
            };
            let error = match again() {
                Ok(here) => return Ok(Followed::Here(here)),
                Err(error) => error,
            };
            match error.downcast_ref::<local_cell::BucketData>() {
                Some(data) if data.location != location => location = data.location.clone(),
                Some(_) => return Err(Failure::Bucket(bucket_cell::Error::Lost(lost)).into()),
                None => return Err(error),
            }
        }
        let moved = format!("{lost}: {extent:?} moved {RELOCATED_MOST} times as it was fetched");
        Err(Failure::Bucket(bucket_cell::Error::Lost(moved)).into())
    }

    /// cladefs: whether it keeps the parts it fetches in the host's cache, which
    /// [`HostFetcher::fetch_ahead`] fetches into (`Fetcher::keeps_parts`, board 6225).
    pub fn keeps_parts(&self) -> bool {
        self.parts.keeps_parts()
    }

    /// cladefs: fetches the parts holding the `len` bytes at `offset` in `extent`, whose data is
    /// at `location` in the bucket, into the host's cache, as read-ahead, behind reads waiting for
    /// theirs (`Fetcher::fetch_ahead`, TASK-702.3): how many it fetched itself. On the runtime its
    /// reads run on, as the caller is.
    pub async fn fetch_ahead(
        &self,
        extent: ExtentId,
        location: &Location,
        offset: u64,
        len: u64,
    ) -> Result<u64, Failure> {
        Ok(self.parts.fetch_ahead(extent, location, offset, len).await?)
    }
}

/// cladefs: has `host`'s fetchers of `bucket` ask `peers` for the parts they own before it. Once.
pub fn ask_peers_on(
    host: &crate::Host,
    bucket: Arc<RoutedBucket>,
    peers: Arc<peers::Peers>,
) -> Result<(), String> {
    host.bucket_state()
        .peers
        .set((bucket, peers))
        .map_err(|_| "the host's peers are set already".to_owned())
}

/// cladefs: has the fetchers of `bucket`, the host's, made from now on ask `peers`, the hosts
/// sharing it, for the parts they own before it (TASK-406.2): before any open cell opens. Once.
#[cfg(not(madsim))]
pub fn ask_peers(bucket: Arc<RoutedBucket>, peers: Arc<peers::Peers>) -> Result<(), String> {
    ask_peers_on(crate::Host::process(), bucket, peers)
}

/// cladefs: the hosts sharing `host`'s bucket, if it asks them for parts ([`ask_peers_on`]).
pub fn peers_on(host: &crate::Host) -> Option<Arc<peers::Peers>> {
    host.bucket_state().peers.get().map(|(_, peers)| peers.clone())
}

/// cladefs: the hosts sharing the host's bucket, if it asks them for parts ([`ask_peers`]).
#[cfg(not(madsim))]
pub fn peers() -> Option<Arc<peers::Peers>> {
    peers_on(crate::Host::process())
}

/// cladefs: what serves the hosts sharing `bucket` on `host` the parts they ask it for.
pub fn serving_peers_on(
    host: &crate::Host,
    bucket: Arc<RoutedBucket>,
    cache: Arc<Cache>,
) -> Result<Serving, Failure> {
    let state = host.bucket_state();
    Ok(Serving {
        runtime: bucket_runtime_on(host)?,
        parts: Arc::new(fetch::Fetcher::new(
            bucket,
            state.budget.clone(),
            state.memory.clone(),
            Some(cache),
        )),
    })
}

/// cladefs: what serves the hosts sharing `bucket`, the host's, the parts they ask it for, as
/// stored (TASK-406.2): a fetcher of it, keeping what it fetches in `cache`, the host's, as its
/// open cells' fetchers do, with them, under the host's budget, and asking no peer.
#[cfg(not(madsim))]
pub fn serving_peers(bucket: Arc<RoutedBucket>, cache: Arc<Cache>) -> Result<Serving, Failure> {
    serving_peers_on(crate::Host::process(), bucket, cache)
}

/// cladefs: what serves the host's peers the parts they ask for ([`serving_peers`]).
#[derive(Clone)]
pub struct Serving {
    runtime: tokio::runtime::Handle,
    parts: Arc<fetch::Fetcher<RoutedBucket>>,
}

impl Serving {
    /// The stored bytes of the part `asked` names, if the host has it: kept, or
    /// fetched from the bucket once, however many ask, on the bucket runtime.
    ///
    /// Aborts the spawned task if the caller drops this future (e.g., peer RPC deadline expired),
    /// so a timed-out peer request releases its queued place in the GET budget rather than
    /// issuing an orphaned bucket GET later (TASK-542, board 2480).
    pub async fn part(&self, asked: peers::Asked) -> Result<Option<Arc<[u8]>>, Failure> {
        struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
        impl<T> Drop for AbortOnDrop<T> {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let parts = self.parts.clone();
        // cladefs: in the trace context current, the peer's call's, the parent of its bucket
        // requests' spans (board 8569, hotbed review 80).
        let mut serving = AbortOnDrop(self.runtime.spawn(
            bucket_cell::trace::Parent::current().around(async move {
                let peers::Asked { key, part } = asked;
                Ok(parts.stored_part(&key, part).await?)
            }),
        ));
        (&mut serving.0).await.map_err(|err| Failure::local(err, "serving a peer"))?
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
    use crate::test_data::noise;
    use crate::{ExtentId, OpenCell, VolatileSlice};
    use bucket_cell::object_bucket::{ObjectBucket, RoutedBucket};
    use bucket_cell::testing::read;
    use bucket_cell::{lifecycle, recover};
    use object_store::memory::InMemory;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

    /// The URL `bucket` is this process's by, made so if it is none's yet: a memory bucket of its
    /// own, by its address, which the open cells the tests make in it name (TASK-165.9).
    pub(super) fn url_of(bucket: &Arc<RoutedBucket>) -> String {
        let url = format!("memory:///bucket-tests-{:p}", Arc::as_ptr(bucket));
        crate::host_cell::use_bucket(&url, bucket.clone());
        url
    }

    /// A new cell directory at `path`, of `capacity` bytes, of `cell`, a new one if none, its cell
    /// kept in `bucket`, its uploader uploading as `keeping` says.
    pub(super) async fn kept_in(
        path: impl AsRef<std::path::Path>,
        (capacity, cell): (u64, Option<crate::CellId>),
        bucket: &Arc<RoutedBucket>,
        keeping: local_cell::upload::Options,
    ) -> Result<OpenCell, crate::Error> {
        let cell = cell.unwrap_or_else(crate::host_cell::new_alone);
        crate::open_cell::Opening::here()?
            .with_keeping(keeping)
            .create_kept(path, capacity, crate::CellOf::Named(cell), &url_of(bucket))
            .await
    }

    /// Has the cell directory at `path` name `bucket` as the bucket its cell is kept in: a host
    /// restarted whose open cells reach the bucket otherwise than before, as the fault tests' do
    /// (TASK-165.9). An open cell whose host died before it described it names nothing.
    pub(super) fn naming(path: &std::path::Path, bucket: &Arc<RoutedBucket>) {
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

    /// The cell directory at `path`, opened, its uploader uploading as `keeping` says.
    pub(super) async fn reopened(
        path: impl AsRef<std::path::Path>,
        keeping: local_cell::upload::Options,
    ) -> Result<OpenCell, crate::Error> {
        crate::open_cell::Opening::here()?.with_keeping(keeping).open(path).await
    }

    /// Uploads only when a commit waits.
    /// How this process's open cells keep their cells ([`crate::host_cell::keeping`]), but
    /// uploading nothing until a commit waits for it, or the hour is up.
    pub(super) fn idle() -> local_cell::upload::Options {
        let keeping = crate::host_cell::keeping().unwrap();
        local_cell::upload::Options {
            interval: Duration::from_secs(3600),
            upload_closed: false,
            ..keeping
        }
    }

    pub(super) fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed ^ (i % 251) as u8).collect()
    }

    async fn write(open_cell: &OpenCell, sequence: u64, data: &[u8]) -> ExtentId {
        let extent = ExtentId { cell: open_cell.cell().id, sequence };
        open_cell.write(extent, 0, &[VolatileSlice::from(data)]).await.unwrap();
        extent
    }

    /// A cell's extents, by sequence number, with their data, and its root.
    pub(super) type Uploaded = (Vec<(u64, Vec<u8>)>, Option<Vec<u8>>);

    /// The cell's extents and root, read back from `bucket`.
    pub(super) fn uploaded(bucket: &Arc<RoutedBucket>, cell: crate::CellId) -> Uploaded {
        futures::executor::block_on(async {
            let state = recover(&**bucket, cell, None).await.unwrap().state;
            let mut extents = Vec::new();
            for (extent, location) in &state.extents {
                extents.push((extent.sequence, read(bucket, *extent, location).await.unwrap()));
            }
            (extents, state.root)
        })
    }

    /// An open cell's extents, its root, and its commits made durable are uploaded; extents written
    /// and freed between two uploads never are.
    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_what_the_open_cell_makes_durable() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "cells").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = open_cell.cell();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit(Some(b"first")).await.unwrap();
        // What it held before is uploaded on attaching.
        assert_eq!(
            uploaded(&bucket, cell),
            (vec![(1, pattern(1, BLOCK))], Some(b"first".to_vec()))
        );

        let large = pattern(3, 4 << 20);
        write(&open_cell, 2, &pattern(2, 2 * BLOCK)).await;
        write(&open_cell, 3, &large).await;
        write(&open_cell, 4, &pattern(4, BLOCK)).await;
        open_cell.commit_ordered(None).await.unwrap();
        open_cell.free(ExtentId { cell: cell.id, sequence: 1 }).unwrap();
        open_cell.free(ExtentId { cell: cell.id, sequence: 4 }).unwrap();
        open_cell.commit(Some(b"second")).await.unwrap();
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

        // Opened again, the open cell's uploader finds the cell as it left it.
        drop(open_cell);
        let open_cell = reopened(dir.path().join("store"), idle()).await.unwrap();
        // Its guest mints from the second writer's range.
        let info = open_cell.info().unwrap();
        assert_eq!(info.sequence_floor, 2 << bucket_cell::EPOCH_SHIFT);
        // What the open cell holds ends far below: the guest looks for what a crash left there
        // only.
        assert!(info.next_sequence < 1 << bucket_cell::EPOCH_SHIFT, "{info:?}");
        let keys = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, "")).unwrap();
        // No segment more: the open cell takes the cell by its head alone.
        assert_eq!(
            keys.iter()
                .filter(|meta| matches!(
                    bucket_cell::layout::kind(&meta.key),
                    bucket_cell::layout::Kind::Segment { .. }
                ))
                .count(),
            segments
        );
        write(&open_cell, 5, &pattern(5, BLOCK)).await;
        open_cell.commit(None).await.unwrap();
        let (extents, root) = uploaded(&bucket, cell);
        assert_eq!(extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(), [2, 3, 5]);
        assert_eq!(root, Some(b"second".to_vec()));
    }

    /// doc-7, The head: an open cell taking a cell over from a writer still uploading, two segments
    /// of it in flight, commits on. The writer's segments land at keys naming its epoch, which the
    /// open cell's never are, and its head swap fails (`Fenced`): what it wrote, no head names, and
    /// the open cell keeps its head. Before (TASK-407), its late segment landed where the open
    /// cell's first went, and the open cell took the cell again.
    #[tokio::test(flavor = "multi_thread")]
    async fn takeovers_of_writers_still_uploading_keep_their_guests() {
        use bucket_cell::{Commit, SegmentBuilder, Writer};
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
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
        // An open cell takes the cell over, opening, its fence after the writer's.
        let path = dir.path().join("store");
        made_for(&path, cell, &bucket);
        let open_cell = reopened(&path, idle()).await.unwrap();
        // Both land, the second first, at keys of the writer's epoch; its head swap fails.
        futures::executor::block_on(async {
            before.upload(segments[1]).await.unwrap();
            before.upload(segments[0]).await.unwrap();
            assert_eq!(before.acknowledge().await, Err(bucket_cell::Error::Fenced));
        });
        // The open cell's commit is the cell's, its head as it was.
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        let epoch = open_cell.uploader().unwrap().status().unwrap().taking.unwrap().epoch;
        let committed = open_cell.commit(Some(b"after")).await;
        let uploader = open_cell.uploader().unwrap();
        assert_eq!((committed, uploader.failed()), (Ok(()), None));
        assert_eq!(uploader.status().unwrap().taking.unwrap().epoch, epoch);
        let after = (vec![(1, pattern(1, BLOCK))], Some(b"after".to_vec()));
        assert_eq!(uploaded(&bucket, cell), after);
    }

    /// The footers of `cell`'s segments in `bucket`, in order.
    fn footers(bucket: &RoutedBucket, cell: crate::CellId) -> Vec<bucket_cell::layout::Footer> {
        use bucket_cell::layout;
        futures::executor::block_on(async {
            let listed = bucket_cell::Bucket::list(bucket, &layout::segments(cell)).await;
            let mut footers = Vec::new();
            for meta in listed.unwrap() {
                let object = bucket_cell::Bucket::get(bucket, &meta.key).await.unwrap();
                footers.push(layout::Segment::footer(&object.bytes).unwrap());
            }
            footers
        })
    }

    /// TASK-165.9: an open cell whose bucket was out of reach holds its commits in its write-ahead
    /// log, segments the commits closed, which upload once it is in reach again, in order, each the
    /// object its file is; what the commits made, and the root, the cell has then. (Before, an open
    /// cell attaching uploaded what its cell lacked in batches.)
    #[tokio::test(flavor = "multi_thread")]
    async fn logs_caught_up_upload_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        let open_cell =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket, idle()).await.unwrap();
        let cell = open_cell.cell();
        gated.shut.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut written = Vec::new();
        for sequence in 1..=3 {
            let data = pattern(sequence as u8, 2 * BLOCK);
            write(&open_cell, sequence, &data).await;
            written.push((sequence, data));
            open_cell.commit_ordered(Some(format!("root {sequence}").as_bytes())).await.unwrap();
        }
        open_cell.flush().await.unwrap();
        // Out of reach: the open cell holds what it committed, the bucket nothing of it.
        let behind = Arc::new(ObjectBucket::new(gated.backend.clone(), "").unrouted());
        assert_eq!(uploaded(&behind, cell), (vec![], None));
        gated.shut.store(false, std::sync::atomic::Ordering::Relaxed);
        open_cell.wait_uploaded().await.unwrap();
        let footers = footers(&bucket, cell);
        let commits: Vec<_> = footers.iter().map(|footer| footer.contents.commits.len()).collect();
        assert_eq!(commits, [1], "the segment of the commits");
        assert_eq!(uploaded(&bucket, cell), (written, Some(b"root 3".to_vec())));
    }

    /// An open cell whose commit, of more bytes than `options` let wait, is in a segment closed for
    /// want of room, as a closing that beats the uploader leaves it: with no commit to come, as one
    /// in the lag's wait holds its guest's seal, and holds back every other. And the commit's
    /// epoch.
    async fn closed_for_room(
        options: local_cell::upload::Options,
    ) -> (tempfile::TempDir, crate::host_cell::Opened, u64) {
        const EXTENT: usize = 1 << 20;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let path = dir.path().join("store");
        let opened = crate::host_cell::create_unmade(
            &path,
            1 << 30,
            crate::host_cell::CellOf::Named(bucket_cell::layout::test_cell(7)),
            bucket_cell::cell::Kind::Fxfs,
            Some(&url_of(&bucket)),
            options,
        )
        .await;
        let opened = opened.unwrap();
        let local = &opened.local;
        local.write(super::ExtentId::new(7, 1), 0, &pattern(1, EXTENT)).await.unwrap();
        local.commit_ordered(None).await.unwrap();
        let epoch = local.last_commit().unwrap();
        // The most a segment holds, its last extent past it, and no commit with them.
        for sequence in 2..98 {
            let extent = super::ExtentId::new(7, sequence);
            local.write(extent, 0, &pattern(sequence as u8, EXTENT)).await.unwrap();
        }
        (dir, opened, epoch)
    }

    /// TASK-150.2: a commit stalled on the bytes waiting (`Options::max_lag_bytes`) ends though
    /// what it waits for is in a segment closed for want of room, which only a commit's flush would
    /// end, as the commit, or its guest, have none to come: an uploader's own flush ends it.
    #[tokio::test(flavor = "multi_thread")]
    async fn commits_stalled_on_the_lag_end_with_their_segment_closed_for_room() {
        let lag = local_cell::upload::Options { max_lag_bytes: BLOCK as u64, ..idle() };
        let (_dir, crate::host_cell::Opened { local, uploader, .. }, epoch) =
            closed_for_room(lag).await;
        let stalled = std::sync::Arc::new(uploader);
        let waiting = stalled.clone();
        let lagged = tokio::time::timeout(Duration::from_secs(30), waiting.lag(epoch)).await;
        assert_eq!(lagged.map_err(drop), Ok(Ok(())));
        assert_eq!(local.wal_bytes().unwrap(), 0);
    }

    /// TASK-150.2: so does a sync of the open cell's commits, or anything else waiting for their
    /// upload.
    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_waited_for_end_with_their_segment_closed_for_room() {
        let (_dir, crate::host_cell::Opened { uploader, .. }, epoch) =
            closed_for_room(idle()).await;
        let uploaded =
            tokio::time::timeout(Duration::from_secs(30), uploader.uploaded(epoch)).await;
        assert_eq!(uploaded.map_err(drop), Ok(Ok(())));
    }

    /// TASK-79.19: a benchmark of an open cell's uploads keeping up with sustained writes, to a
    /// bucket whose PUTs take 350 ms each, as S3's of a segment of 32 MiB may: 4 GiB written, a MiB
    /// an extent, an ordered commit after each 32 MiB, as a guest's writeback makes them, each
    /// waiting while the lag is past its bound, as the open cell's do; how long until it is all
    /// uploaded, at how many MB/s, and how long the commits waited for the lag, in all.
    /// `cargo nextest run -p cladefs --run-ignored only bench_sustained_uploads --no-capture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a benchmark: --run-ignored only, alone"]
    async fn bench_sustained_uploads() {
        use object_store::throttle::{ThrottleConfig, ThrottledStore};
        const WRITTEN: u64 = 4 << 30;
        const EXTENT: usize = 1 << 20;
        let put = Duration::from_millis(350);
        let config = ThrottleConfig { wait_put_per_call: put, ..ThrottleConfig::default() };
        let objects = Arc::new(ThrottledStore::new(InMemory::new(), config));
        let bucket = Arc::new(ObjectBucket::new(objects, "").unrouted());
        let dir = tempfile::tempdir().unwrap();
        let open_cell = kept_in(
            dir.path().join("store"),
            (2 * WRITTEN, None),
            &bucket,
            local_cell::upload::Options::default(),
        )
        .await
        .unwrap();
        let data = noise(1, EXTENT);
        let (started, mut waited) = (std::time::Instant::now(), Duration::ZERO);
        for sequence in 1..=WRITTEN / EXTENT as u64 {
            write(&open_cell, sequence, &data).await;
            if sequence % 32 == 0 {
                let committing = std::time::Instant::now();
                open_cell.commit_ordered(None).await.unwrap();
                waited += committing.elapsed();
            }
        }
        let written = started.elapsed();
        open_cell.wait_uploaded().await.unwrap();
        let took = started.elapsed();
        let stalls = open_cell.uploader().unwrap().stalls();
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
    /// unless set; its first count alone), once from the open cell itself, live, whose writer
    /// writes a checkpoint for each fork, as `cladefsd::tests::serves_hundreds_of_forks_of_a_base`
    /// makes its forks, and once from a version of it, whose forks all start from the version's one
    /// state (`CLADEFS_BENCH_PARENTS`, `live,version` unless set). A line for each count of forks
    /// open, then one of what holds it ([`fork_extent_memory`]).
    /// TASK-453: and it fails unless each fork past the first holds [`IDLE_FORK_BYTES`] at most,
    /// whatever its base holds, and the daemon's count of the open cells' maps
    /// (`cladefsd::process::AnonBudget::extents`) covers what they hold.
    /// In the sim profile, optimized as a release build is, which this crate's tests, with their
    /// failpoints, refuse to be (TASK-320): the bytes are any build's, the times this one's.
    /// `cargo nextest run -p cladefs --cargo-profile sim --run-ignored only
    /// bench_fork_extent_memory --no-capture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a benchmark (TASK-443.2): --run-ignored only, in the sim profile"]
    async fn bench_fork_extent_memory() {
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
                fork_extent_memory(extents, forks, version).await;
            }
        }
    }

    /// TASK-453: the most bytes of the heap an open, idle fork holds past the first of its base's,
    /// whatever its base holds: what it inherited it shares with the others, its open cell's and
    /// its writer's maps the decoded state they started from, and holds of its own what it wrote,
    /// here nothing. Measured 31-34 KB a fork (`bench_fork_extent_memory`), 221-468 bytes for each
    /// of the base's extents more before (TASK-443.2).
    const IDLE_FORK_BYTES: u64 = 64 << 10;

    /// What the open cells' maps of their extents hold now, as the extent daemon counts it
    /// (`cladefsd::process::extents_held_on`, Linux's): what the open cells hold of their own, and the
    /// decoded states they share, each once.
    fn extents_counted() -> (local_cell::Own, bucket_cell::recovery::Held) {
        (local_cell::own_held(), crate::host_cell::decoded_held())
    }

    /// Opens `forks` forks of a base of `extents` extents, forked from a version of it if
    /// `version`, from the open cell itself otherwise, and prints what the heap holds more than
    /// before them with 1, 10, 20, 50 and 100 of them open, in all, for each, for each opened since
    /// the count before (marginal), and that for each extent of the base; and how long each of
    /// those took to open. TASK-453: each past the first holds [`IDLE_FORK_BYTES`] at most, the
    /// decoded state they share one, of the base's extents, counted at
    /// [`bucket_cell::recovery::ENTRY_BYTES`] an entry, which covers what the first held more than
    /// the others, and a map of them built sharing nothing, as the writer's of a base written live
    /// is. Then what closing them all leaves held over the heap before them; opened and closed
    /// again, the heap is to the byte what the first closing left (TASK-471: not always, by 80 to
    /// 2,048 bytes), checked last. Then one fork more writes, commits and uploads a block for every
    /// extent of the base, up to 4,096: what it holds more, what closing it lets go of more than
    /// closing an idle one does, what it holds of its own, counted ([`local_cell::Own::bytes`]),
    /// covers. And one more frees every 32nd extent it inherited, each its writer's map's change
    /// apart, a path of its nodes copied ([`bucket_cell::recovery::RUN_BYTES`]), and checkpoints:
    /// what closing it lets go of more than an idle one does, counted too; and opened again, from
    /// its checkpoint, its map made of the base's, what the heap holds more than an idle fork the
    /// bucket's decoded states count more.
    async fn fork_extent_memory(extents: u64, forks: usize, version: bool) {
        use crate::host_cell::fork_child;
        let held = || crate::heap::held() as u64;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let capacity = 2 * extents * BLOCK as u64 + (1 << 30);
        let base =
            kept_in(dir.path().join("base"), (capacity, None), &bucket, idle()).await.unwrap();
        let data = pattern(1, BLOCK);
        for sequence in 1..=extents {
            write(&base, sequence, &data).await;
            if sequence % 1024 == 0 {
                base.commit(None).await.unwrap();
            }
        }
        base.commit(None).await.unwrap();
        let base_cell = base.cell().id;
        let parent = match version {
            false => base,
            true => {
                let path = dir.path().join("copy");
                base.uploaded_version_at(&path).await.unwrap();
                drop(base);
                reopened(&path, idle()).await.unwrap()
            }
        };
        // And two more, which write and free, open apart from the others.
        let mut paths = Vec::new();
        for n in 0..forks + 2 {
            let path = dir.path().join(format!("fork-{n}"));
            parent.fork_to(&path, fork_child(parent.cell())).await.unwrap();
            paths.push(path);
        }
        let (freeing, writing) = (paths.pop().unwrap(), paths.pop().unwrap());
        drop(parent);
        let open = async |path: &std::path::PathBuf| {
            crate::open_cell::Opening::keeping(idle()).for_host().open(path).await.unwrap()
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

        let mut open_cells = Vec::with_capacity(forks);
        let mut samples = Vec::with_capacity(5);
        every_task_shard_locked();
        let (own_before, decoded_before) = extents_counted();
        let before = held();
        let started = std::time::Instant::now();
        for (n, path) in paths.iter().enumerate() {
            open_cells.push(open(path).await);
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

        // A map of them sharing nothing, as the writer's of a base written live is, which an open
        // cell over it counts as its own.
        let cell = open_cells[0].cell();
        let record = bucket_cell::head::get(&*bucket, cell).await.unwrap().unwrap();
        let state = recover(&*bucket, cell, record.origin()).await.unwrap().state;
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
        open_cells.clear();
        let closed = settled();
        for path in &paths {
            open_cells.push(open(path).await);
        }
        open_cells.clear();
        let again = settled();
        println!(
            "fork memory: extents={extents} parent={name} left-after-close={}",
            closed as i64 - before as i64
        );

        // One more writes, commits and uploads, beside an idle one.
        let writes = extents.min(4096);
        let (idle, opened) = (open(&paths[0]).await, open(&writing).await);
        let writer = &opened;
        let floor = writer.info().unwrap().next_sequence.max(writer.info().unwrap().sequence_floor);
        let own_before = local_cell::own_held();
        for sequence in floor..floor + writes {
            write(writer, sequence, &data).await;
            if (sequence - floor + 1) % 1024 == 0 {
                writer.commit(None).await.unwrap();
            }
        }
        writer.commit(None).await.unwrap();
        writer.wait_uploaded().await.unwrap();
        let own_after = local_cell::own_held();
        let own_written = own_after.entries - own_before.entries;
        assert!(idle.shares_extents_with(writer), "its bucket's layer, written over");
        // What its writes hold, once its uploader is done with them: what closing it lets go of,
        // more than closing an idle fork does. Not what the heap holds more meanwhile, which
        // its uploader's work in flight swings by megabytes.
        let closing = |open_cell: OpenCell| {
            let open = settled();
            drop(open_cell);
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
        let (idle, opened) = (open(&paths[0]).await, open(&freeing).await);
        let (counted_before, _) = extents_counted();
        let frees =
            (1..=extents).step_by(32).map(|sequence| ExtentId { cell: base_cell, sequence });
        let frees: Vec<_> = frees.collect();
        for &extent in &frees {
            opened.free(extent).unwrap();
        }
        opened.commit(None).await.unwrap();
        crate::test_checkpoints::checkpointed(&opened, &bucket).await;
        opened.wait_uploaded().await.unwrap();
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
        let idle = open(&paths[0]).await;
        let decoded = || bucket_cell::Bucket::decoded(&*bucket).unwrap().held();
        let (heap_before, decoded_before) = (settled(), decoded());
        let reopened = open(&freeing).await;
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

    /// TASK-453: the forks of a base open at once hold what they inherited once: their open cells'
    /// maps of it one, the decoded state they started from, which their bucket counts once, whether
    /// forked from the base live, each from a checkpoint of its own the base's writer wrote, or
    /// from a version of it. Each reads the base's data; what one writes and frees, an inherited
    /// extent among them, is its own: the others, and a fork opened after, read the base's, and
    /// share its map still.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_open_at_once_share_what_they_inherited() {
        use crate::host_cell::fork_child;
        const EXTENTS: u64 = 64;
        const FORKS: usize = 4;
        for version in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
            let base =
                kept_in(dir.path().join("base"), (1 << 30, None), &bucket, idle()).await.unwrap();
            for sequence in 1..=EXTENTS {
                write(&base, sequence, &pattern(sequence as u8, BLOCK)).await;
            }
            base.commit(None).await.unwrap();
            let base_cell = base.cell().id;
            let inherited = |sequence| ExtentId { cell: base_cell, sequence };
            let parent = match version {
                false => base,
                true => {
                    let path = dir.path().join("copy");
                    base.uploaded_version_at(&path).await.unwrap();
                    reopened(&path, idle()).await.unwrap()
                }
            };
            let fork = async |n: usize| {
                let path = dir.path().join(format!("fork-{n}"));
                parent.fork_to(&path, fork_child(parent.cell())).await.unwrap();
                path
            };
            let mut paths = Vec::new();
            for n in 0..=FORKS {
                paths.push(fork(n).await);
            }
            let mut forks = Vec::new();
            for path in &paths[..FORKS] {
                forks.push(reopened(path, idle()).await.unwrap());
            }
            let name = if version { "of a version" } else { "of the live cell" };
            for other in &forks[1..] {
                assert!(forks[0].shares_extents_with(other), "forks {name}: their maps");
            }
            let one = bucket_cell::recovery::Held { states: 1, entries: EXTENTS as usize, runs: 0 };
            let decoded = || bucket_cell::Bucket::decoded(&*bucket).unwrap().held();
            assert_eq!(decoded(), one, "forks {name}: the states they share, decoded");
            let read = async |open_cell: &OpenCell, extent: ExtentId| {
                let mut data = vec![0; BLOCK];
                let read = open_cell
                    .read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])])
                    .await;
                read.map(|()| data).map_err(|err| err.to_string())
            };
            for open_cell in &forks {
                for sequence in 1..=EXTENTS {
                    let data = pattern(sequence as u8, BLOCK);
                    assert_eq!(
                        read(open_cell, inherited(sequence)).await,
                        Ok(data),
                        "forks {name}"
                    );
                }
            }

            // The first frees an inherited extent, and writes and commits one of its own.
            let (freed, own) = (inherited(7), {
                let info = forks[0].info().unwrap();
                info.next_sequence.max(info.sequence_floor)
            });
            forks[0].free(freed).unwrap();
            let mine = write(&forks[0], own, &pattern(200, BLOCK)).await;
            forks[0].commit(None).await.unwrap();
            // What a read of an extent an open cell holds not answers (`NoExtent`, as errno).
            let none = || Err("ENOENT (errno 2)".to_owned());
            assert_eq!(read(&forks[0], freed).await, none());
            assert_eq!(read(&forks[0], mine).await, Ok(pattern(200, BLOCK)));
            let opened = reopened(&paths[FORKS], idle()).await.unwrap();
            for open_cell in forks[1..].iter().chain([&opened]) {
                assert_eq!(
                    read(open_cell, freed).await,
                    Ok(pattern(7, BLOCK)),
                    "forks {name}: the freed"
                );
                assert_eq!(read(open_cell, mine).await, none(), "forks {name}: the first's own");
                assert!(forks[0].shares_extents_with(open_cell), "forks {name}: their maps, after");
            }
            assert_eq!(decoded(), one, "forks {name}: the states they share, after");
        }
    }

    /// TASK-453: what a read asks of a fork's open cell of an extent it inherited, its length and
    /// whether it is open, and a read of it from the extent cache, there once read, allocate
    /// nothing: the open cell's map lends its bucket's layer's entry, where it made one of its own
    /// of it, its key copied, for every read, under the open cell's lock. A base of more extents
    /// than the extent store's audit recounts (`AUDITED_EXTENTS`, 4,096), which the builds of its
    /// own tests run at every lock of an open cell's state, as they build it for this crate's tests
    /// too when run with them: no build but a test's audits.
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_of_inherited_extents_allocate_nothing() {
        use crate::heap::allocations_while;
        use crate::host_cell::fork_child;
        const EXTENTS: u64 = 4160;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let capacity = 2 * EXTENTS * BLOCK as u64 + (1 << 30);
        let base =
            kept_in(dir.path().join("base"), (capacity, None), &bucket, idle()).await.unwrap();
        for sequence in 1..=EXTENTS {
            write(&base, sequence, &pattern(sequence as u8, BLOCK)).await;
            if sequence % 1024 == 0 {
                base.commit(None).await.unwrap();
            }
        }
        base.commit(None).await.unwrap();
        let base_cell = base.cell().id;
        let inherited = |sequence| ExtentId { cell: base_cell, sequence };
        let path = dir.path().join("fork");
        base.fork_to(&path, fork_child(base.cell())).await.unwrap();
        let fork = reopened(&path, idle()).await.unwrap();
        let held = Ok(Some((BLOCK as u64, false)));
        let mut data = vec![0; BLOCK];
        // Each read once: into the cache; on the executor the reads counted run on, whose first
        // use on a thread allocates what it wakes it with.
        futures::executor::block_on(async {
            for sequence in 1..=EXTENTS {
                assert_eq!(fork.stored_extent(inherited(sequence)), held);
                let read = fork.read_from_cache(inherited(sequence), 0, &mut data).await;
                assert_eq!(read, Ok(true));
                assert_eq!(data, pattern(sequence as u8, BLOCK), "{sequence}");
            }
        });
        let (asked, ()) = allocations_while(|| {
            for sequence in 1..=EXTENTS {
                assert_eq!(fork.stored_extent(inherited(sequence)), held);
            }
        });
        assert_eq!(asked, 0, "allocations of {EXTENTS} asks of the cell");
        // Warm up `futures::executor::block_on`'s thread-local `CURRENT_THREAD_NOTIFY` Arc on
        // whichever tokio worker thread we are running on before counting cache-read allocations.
        futures::executor::block_on(async {});
        let (read, ()) = allocations_while(|| {
            futures::executor::block_on(async {
                for sequence in 1..=EXTENTS {
                    assert_eq!(
                        fork.read_from_cache(inherited(sequence), 0, &mut data).await,
                        Ok(true)
                    );
                }
            })
        });
        assert_eq!(read, 0, "allocations of {EXTENTS} reads from the cache");
    }

    /// TASK-453: a benchmark of a read's hit in the extent cache of an extent a fork inherited
    /// (`OpenCell::read_cached`): a base of `CLADEFS_BENCH_EXTENTS` extents (32,768 unless set)
    /// of a block each, forked, each of them read once into the cache, then all of them read
    /// `CLADEFS_BENCH_ROUNDS` times (20 unless set), a block each: the time a read, and the
    /// allocations. In the sim profile: `cargo nextest run -p cladefs --cargo-profile sim
    /// --run-ignored only bench_inherited_cached_reads --no-capture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a benchmark (TASK-453): --run-ignored only, in the sim profile"]
    async fn bench_inherited_cached_reads() {
        use crate::host_cell::fork_child;
        let count = |name: &str, default: u64| -> u64 {
            std::env::var(name).map_or(default, |count| count.parse().expect("a count"))
        };
        let (extents, rounds) =
            (count("CLADEFS_BENCH_EXTENTS", 32768), count("CLADEFS_BENCH_ROUNDS", 20));
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let capacity = 2 * extents * BLOCK as u64 + (1 << 30);
        let base =
            kept_in(dir.path().join("base"), (capacity, None), &bucket, idle()).await.unwrap();
        let data = pattern(1, BLOCK);
        for sequence in 1..=extents {
            write(&base, sequence, &data).await;
            if sequence % 1024 == 0 {
                base.commit(None).await.unwrap();
            }
        }
        base.commit(None).await.unwrap();
        let base_cell = base.cell().id;
        let inherited = |sequence| ExtentId { cell: base_cell, sequence };
        let path = dir.path().join("fork");
        base.fork_to(&path, fork_child(base.cell())).await.unwrap();
        let fork = reopened(&path, idle()).await.unwrap();
        let mut read = vec![0; BLOCK];
        for sequence in 1..=extents {
            assert_eq!(fork.read_from_cache(inherited(sequence), 0, &mut read).await, Ok(true));
        }
        assert_eq!(read, data);
        let started = std::time::Instant::now();
        let (allocations, ()) = crate::heap::allocations_while(|| {
            futures::executor::block_on(async {
                for _ in 0..rounds {
                    for sequence in 1..=extents {
                        let hit = fork.read_from_cache(inherited(sequence), 0, &mut read).await;
                        assert_eq!(hit, Ok(true));
                    }
                }
            })
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
    /// shards none had (32 at most on this runtime): nothing of the open cells'. The other such
    /// holder, each of its threads' list of the shared locks it holds, it makes as each starts
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

    /// An open cell's closed segments are the objects of its cell in the bucket, byte for byte, as
    /// are its extents stored on their own (doc-7, decision 1): the uploader puts its files; and
    /// once the bucket has acknowledged them, they leave the open cell whole, into the host's
    /// cache, as those objects, which the open cell's reads read there with the bucket's gone
    /// (TASK-165.8).
    #[tokio::test(flavor = "multi_thread")]
    async fn uploaded_segments_are_the_cells_files() {
        use bucket_cell::layout;
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(memory.clone(), "").unrouted());
        let cache = Arc::new(
            host_fs::may_block(|| {
                bucket_cell::cache::Cache::open(&dir.path().join("cache"), 1 << 30)
            })
            .unwrap(),
        );
        let options = local_cell::upload::Options { cache: Some(cache.clone()), ..idle() };
        let open_cell =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), options)
                .await
                .unwrap();
        let cell = open_cell.cell();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        write(&open_cell, 2, &pattern(2, 4 << 20)).await;
        open_cell.commit(Some(b"one")).await.unwrap();
        write(&open_cell, 3, &pattern(3, 2 * BLOCK)).await;
        open_cell.commit(Some(b"two")).await.unwrap();

        // The open cell's files of `kind`, by their contents.
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
            .filter(|bytes| !layout::Segment::footer(bytes).unwrap().contents.commits.is_empty())
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
            open_cell
                .read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])])
                .await
                .unwrap();
            assert!(read == data, "extent {sequence} reads other data");
        }
    }

    /// cladefs: a byte the host's disk damaged in a file the uploader puts as it is, a sealed
    /// extent's own, fails the upload, and the commit waiting for it, rather than going to object
    /// storage under the CRC it fails (TASK-153.11).
    #[tokio::test(flavor = "multi_thread")]
    async fn files_with_flipped_bytes_are_not_uploaded() {
        use bucket_cell::layout;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = open_cell.cell();
        // Stored on its own, raw, as it doesn't compress.
        let extent = write(&open_cell, 1, &noise(1, 4 << 20)).await;
        open_cell.commit_ordered(None).await.unwrap();
        // On the disk, as the open cell's page cache holds it written (TASK-433).
        host_fs::may_block(|| local_cell::pages::shared().write_back_all().unwrap());
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
        // Damaged behind the open cell's back: what the open cell's page cache holds of it, clean,
        // goes, so that its reads read the disk (TASK-433).
        local_cell::pages::shared().evict_to(0);
        assert_eq!(open_cell.commit(None).await, Err(crate::error::Errno::EIO));
        let listed = |prefix: String| -> Vec<String> {
            let listed = bucket_cell::Bucket::list(&*bucket, &prefix);
            futures::executor::block_on(listed).unwrap().into_iter().map(|meta| meta.key).collect()
        };
        assert_eq!(listed(layout::extents(cell)), Vec::<String>::new());
    }

    /// A commit waits for its upload no longer than it takes: once another writer has the cell,
    /// a durable commit fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn durable_commits_fail_once_another_takes_the_cell() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit(None).await.unwrap();
        let other = bucket_cell::Writer::open(
            bucket.clone(),
            open_cell.cell(),
            2,
            crate::host_cell::token().unwrap(),
        );
        futures::executor::block_on(other).unwrap();
        write(&open_cell, 2, &pattern(2, BLOCK)).await;
        assert_eq!(open_cell.commit(None).await, Err(crate::Errno::EROFS));
    }

    /// TASK-39.7: an open cell's copy takes its cell with its placement's fencing token: a later
    /// placement's, higher, fences it, its commits failing, and it takes the cell back no more,
    /// though its open cell is as current as the cell; a placement later still takes it.
    #[tokio::test(flavor = "multi_thread")]
    async fn later_placements_fence_earlier_ones() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let (first_path, second_path) = (dir.path().join("first"), dir.path().join("second"));
        let first = kept_in(&first_path, (1 << 30, None), &bucket.clone(), idle()).await.unwrap();
        let cell = first.cell();
        host_fs::may_block(|| OpenCell::place(&first_path, 1).unwrap());
        write(&first, 1, &pattern(1, BLOCK)).await;
        first.commit(Some(b"first")).await.unwrap();

        // The cell moves to another host, a later placement, while the first still runs.
        drop(kept_in(&second_path, (1 << 30, Some(cell)), &bucket, idle()).await.unwrap());
        host_fs::may_block(|| OpenCell::place(&second_path, 2).unwrap());
        let second = reopened(&second_path, idle()).await.unwrap();
        assert_eq!(second.root(), Ok(b"first".to_vec()));
        write(&first, 2, &pattern(2, BLOCK)).await;
        assert_eq!(first.commit(None).await, Err(crate::Errno::EROFS));
        drop((first, second));

        // The first, restarted, takes it back no more: its token is the earlier.
        let refused = reopened(&first_path, idle()).await.err().and_then(|err| err.errno());
        assert_eq!(refused, Some(crate::Errno::ESTALE));
        let head = futures::executor::block_on(bucket_cell::head::get(&*bucket, cell));
        assert_eq!(head.unwrap().unwrap().token, 2);

        // Placed later still, it does.
        host_fs::may_block(|| OpenCell::place(&first_path, 3).unwrap());
        let later = reopened(&first_path, idle()).await.unwrap();
        assert_eq!(later.root(), Ok(b"first".to_vec()));
    }

    /// TASK-550, TASK-612, doc-4: an open cell whose uploader failed for good, as a later placement
    /// took its cell, changes nothing more: neither an ordered commit (a guest's fsync) nor a local
    /// one, nor a write, a free, a version or a resize, each `EROFS` as its durable commits are,
    /// rather than taken for what its cell will hold.
    #[tokio::test(flavor = "multi_thread")]
    async fn fenced_open_cells_change_nothing() {
        changes_nothing_once_lost(async |bucket, dir, cell| {
            let second = dir.join("second");
            drop(kept_in(&second, (1 << 30, Some(cell)), bucket, idle()).await.unwrap());
            host_fs::may_block(|| OpenCell::place(&second, 2).unwrap());
            drop(reopened(&second, idle()).await.unwrap());
        })
        .await;
    }

    /// TASK-550: as [`fenced_open_cells_change_nothing`], an open cell whose cell a later placement
    /// deleted.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_cells_of_deleted_cells_change_nothing() {
        changes_nothing_once_lost(async |bucket, dir, cell| {
            let second = dir.join("second");
            drop(kept_in(&second, (1 << 30, Some(cell)), bucket, idle()).await.unwrap());
            host_fs::may_block(|| OpenCell::place(&second, 2).unwrap());
            OpenCell::delete(&second, Some(bucket)).await.unwrap();
        })
        .await;
    }

    /// An open cell, placed, its cell then lost to it as `lose` loses it, in `dir`, changes nothing
    /// more once its uploader finds it lost.
    async fn changes_nothing_once_lost(
        lose: impl AsyncFnOnce(&Arc<RoutedBucket>, &std::path::Path, crate::CellId),
    ) {
        const EROFS: crate::Errno = crate::Errno::EROFS;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let path = dir.path().join("first");
        let open_cell = kept_in(&path, (1 << 30, None), &bucket.clone(), idle()).await.unwrap();
        let cell = open_cell.cell();
        host_fs::may_block(|| OpenCell::place(&path, 1).unwrap());
        let kept = write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit(None).await.unwrap();
        lose(&bucket, dir.path(), cell).await;
        // Ordered before its uploader finds the cell lost, which its next upload does: a loss
        // doc-4 accepts.
        write(&open_cell, 2, &pattern(2, BLOCK)).await;
        open_cell.commit_ordered(None).await.unwrap();
        assert_eq!(open_cell.wait_uploaded().await, Err(EROFS));
        assert!(open_cell.uploader().unwrap().failed().is_some());
        let extent = ExtentId { cell: cell.id, sequence: 3 };
        let data = pattern(3, BLOCK);
        assert_eq!(open_cell.write(extent, 0, &[VolatileSlice::from(&data[..])]).await, Err(EROFS));
        assert_eq!(open_cell.free(kept), Err(EROFS));
        assert_eq!(open_cell.commit_ordered(None).await, Err(EROFS));
        assert_eq!(open_cell.commit_local(None).await, Err(EROFS));
        let made = open_cell.make_version(None).await.err().and_then(|err| err.errno());
        assert_eq!(made, Some(EROFS));
        assert_eq!(open_cell.set_capacity(2 << 30).await, Err(EROFS));
    }

    /// TASK-570: a cell names [`bucket_cell::layout::MAX_VERSIONS`] versions at most (doc-18):
    /// one more, with no window, fails with `EMLINK`, making none, as every acknowledgement after
    /// would write it into the head; one with a window, which drops the oldest, is made.
    #[tokio::test(flavor = "multi_thread")]
    async fn cells_make_so_many_versions_at_most() {
        let most = bucket_cell::layout::MAX_VERSIONS;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit(None).await.unwrap();
        for _ in 0..most {
            open_cell.make_version(None).await.unwrap();
        }
        let refused = open_cell.make_version(None).await.err().and_then(|err| err.errno());
        assert_eq!(refused, Some(crate::Errno::EMLINK));
        assert_eq!(open_cell.versions().await.len(), most);
        open_cell.make_version(Some(1)).await.unwrap();
    }

    /// TASK-610: what this process made versions of it keeps for versions their cells name alone:
    /// those a later version's window dropped, and those dropped, it forgets, rather than one more
    /// for every version it ever made.
    #[tokio::test(flavor = "multi_thread")]
    async fn versions_made_here_are_forgotten_once_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit(None).await.unwrap();
        for _ in 0..20 {
            open_cell.make_version(Some(1)).await.unwrap();
        }
        open_cell.wait_uploaded().await.unwrap();
        let last = open_cell.make_version(Some(1)).await.unwrap();
        let named = open_cell.versions().await.len();
        assert!(named <= 2, "{named} versions named");
        assert_eq!(crate::host_cell::versions_made(), named);
        open_cell.drop_versions(&[last].into()).await.unwrap();
        assert_eq!(crate::host_cell::versions_made(), named - 1);
    }

    /// TASK-45.1: a placement cut off deletes no cell from under a later one: its delete fails
    /// (`ESTALE`), leaving the cell in the bucket, which the later placement goes on writing; the
    /// later placement's delete deletes it.
    #[tokio::test(flavor = "multi_thread")]
    async fn earlier_placements_delete_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let (first_path, second_path) = (dir.path().join("first"), dir.path().join("second"));
        let first = kept_in(&first_path, (1 << 30, None), &bucket.clone(), idle()).await.unwrap();
        let cell = first.cell();
        host_fs::may_block(|| OpenCell::place(&first_path, 1).unwrap());
        write(&first, 1, &pattern(1, BLOCK)).await;
        first.commit(Some(b"first")).await.unwrap();
        drop(first);
        // The cell moves to another host, a later placement.
        drop(kept_in(&second_path, (1 << 30, Some(cell)), &bucket, idle()).await.unwrap());
        host_fs::may_block(|| OpenCell::place(&second_path, 2).unwrap());
        let second = reopened(&second_path, idle()).await.unwrap();

        let refused = OpenCell::delete(&first_path, Some(&bucket)).await;
        assert_eq!(refused.err().and_then(|err| err.errno()), Some(crate::Errno::ESTALE));
        write(&second, 2, &pattern(2, BLOCK)).await;
        second.commit(Some(b"second")).await.unwrap();
        drop(second);
        let state =
            futures::executor::block_on(bucket_cell::recover(&*bucket, cell, None)).unwrap().state;
        assert_eq!(state.root, Some(b"second".to_vec()));

        OpenCell::delete(&second_path, Some(&bucket)).await.unwrap();
        let deleted = futures::executor::block_on(bucket_cell::head::get(&*bucket, cell));
        assert!(deleted.unwrap().unwrap().deleted(), "the cell lives");
    }

    /// TASK-39.7: an open cell's copy never placed takes its cell with its host's token, as an open
    /// cell restored or taken on a host is.
    #[tokio::test(flavor = "multi_thread")]
    async fn unplaced_copies_take_their_hosts_token() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let options = local_cell::upload::Options { token: 9, ..idle() };
        let open_cell =
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), options)
                .await
                .unwrap();
        let head = futures::executor::block_on(bucket_cell::head::get(&*bucket, open_cell.cell()));
        let head = head.unwrap().unwrap();
        assert_eq!((head.token, head.epoch), (9, 1));
    }

    /// TASK-165.9: an open cell whose bucket acknowledged commits of its cell, opened where the
    /// bucket has no such cell, deleted or another bucket, refuses (`ENOENT`), rather than make
    /// the cell anew without them.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_cells_whose_bucket_lacks_their_cell_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let path = dir.path().join("store");
        let open_cell = kept_in(&path, (1 << 30, None), &bucket, idle()).await.unwrap();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit(Some(b"acknowledged")).await.unwrap();
        drop(open_cell);
        let other = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        naming(&path, &other);
        let refused = reopened(&path, idle()).await.err().and_then(|err| err.errno());
        assert_eq!(refused, Some(crate::Errno::ENOENT));
        // Its own bucket named again, it opens as it was.
        naming(&path, &bucket);
        assert_eq!(reopened(&path, idle()).await.unwrap().root(), Ok(b"acknowledged".to_vec()));
    }

    /// TASK-192: an open cell dropped uploads what it committed, its open segment's commits too, as
    /// `local_cell::upload`'s docs say: its uploader closes the open segment, uploads it and
    /// stops, before the open cell closes. Before, the open cell closed first, and its uploader
    /// found the open segment's commits it could no longer close.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_open_cells_upload_their_open_segments_commits() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = open_cell.cell();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit_ordered(Some(b"ordered")).await.unwrap();
        // Not uploaded yet: the uploader waits an hour, unless asked.
        assert_eq!(uploaded(&bucket, cell), (vec![], None));
        drop(open_cell);
        assert_eq!(
            uploaded(&bucket, cell),
            (vec![(1, pattern(1, BLOCK))], Some(b"ordered".to_vec()))
        );
    }

    /// TASK-39.7: waiting for uploads returns once every commit so far is in the cell.
    #[tokio::test(flavor = "multi_thread")]
    async fn waits_for_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit_ordered(Some(b"ordered")).await.unwrap();
        // Not uploaded yet: the uploader waits an hour, unless asked.
        assert_eq!(uploaded(&bucket, open_cell.cell()).1, None);
        open_cell.wait_uploaded().await.unwrap();
        assert_eq!(
            uploaded(&bucket, open_cell.cell()),
            (vec![(1, pattern(1, BLOCK))], Some(b"ordered".to_vec()))
        );
    }

    /// Which writes a [`Gated`] backend holds.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Hold {
        Nothing,
        /// The writes of segments.
        Segments,
        /// The writes of segments of this number or more.
        SegmentsFrom(u64),
        /// The write of the object at this key.
        Key(String),
        /// cladefs: the writes of the cell's head.
        Head(crate::CellId),
        /// The first write of the cell's head, those after it while it waits going on: a
        /// takeover's, its writer's swaps of it meanwhile not (TASK-407).
        FirstOfHead(crate::CellId),
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
                Hold::Key(held) => held == key,
                Hold::Head(cell) | Hold::FirstOfHead(cell) => {
                    matches!(kind(key), Kind::Head { cell: of, .. } if of == *cell)
                }
            }
        }
    }

    /// What goes wrong with a write through a [`Gated`] backend, as it fails: requests that fail,
    /// the host's bucket's helpers send again.
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
        backend: Arc<dyn object_store::ObjectStore>,
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

        /// The objects of `backend`, gated.
        pub(super) fn over(backend: Arc<dyn object_store::ObjectStore>) -> Self {
            Self {
                backend,
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
            self.severed.lock().unwrap().clone().unwrap_or_else(|| self.backend.clone())
        }

        /// Cuts it off from its objects, as a host dead is: its requests reach a copy of them from
        /// now on, which nothing else sees, so that what the host had going runs to its end, doing
        /// nothing a dead host would not, rather than wait for ever.
        pub(super) fn sever(&self) {
            use object_store::{ObjectStore as _, ObjectStoreExt as _};
            let copy = InMemory::new();
            futures::executor::block_on(async {
                use futures::StreamExt as _;
                for meta in self.backend.list(None).collect::<Vec<_>>().await {
                    let location = meta.unwrap().location;
                    let bytes = self.backend.get(&location).await.unwrap().bytes().await.unwrap();
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
                let landed = self.backend.put_opts(&location, payload, options);
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
            let after_first = matches!(&*self.hold.borrow(), Hold::FirstOfHead(_))
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
    async fn waits_at_the_lag_bound(
        options: local_cell::upload::Options,
        failing: fn(&Gated) -> &std::sync::atomic::AtomicBool,
        commit: fn(&OpenCell) -> futures::future::BoxFuture<'_, crate::error::Result<()>>,
    ) {
        use bucket_cell::object_bucket::Request;
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        let open_cell = Arc::new(
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), options)
                .await
                .unwrap(),
        );
        let creates_failed = bucket.stats().failures(Request::Create);
        failing(&gated).store(true, std::sync::atomic::Ordering::Relaxed);
        for sequence in 1..=2 {
            write(&open_cell, sequence, &pattern(sequence as u8, BLOCK)).await;
            commit(&open_cell).await.unwrap();
        }
        let third = {
            let open_cell = open_cell.clone();
            tokio::spawn(async move {
                write(&open_cell, 3, &pattern(3, 2 * BLOCK)).await;
                commit(&open_cell).await
            })
        };
        let uploader = open_cell.uploader().unwrap();
        // Until it stalls, or finishes, which it must not: past the bound, it waits.
        while (uploader.stalls() == 0 || bucket.stats().failures(Request::Create) == creates_failed)
            && !third.is_finished()
        {
            std::thread::yield_now();
        }
        assert!(!third.is_finished(), "a commit past the lag bound went on");
        assert_eq!(uploader.failed(), None, "the uploader stopped");
        failing(&gated).store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(third.await.unwrap(), Ok(()));
        open_cell.commit(None).await.unwrap();
        let (extents, _) = uploaded(&bucket, open_cell.cell());
        assert_eq!(extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(), [1, 2, 3]);
    }

    /// TASK-317: what an open cell's uploads hold in memory while the bucket takes none of them,
    /// however far behind they fall, is their budget, the host's upload memory: segments of
    /// incompressible data, a MiB an extent, each closed by a commit waiting for its upload, twice
    /// as many as the open cell uploads at once, which the bucket holds unanswered. What waits is
    /// in the open cell's write-ahead log, on disk.
    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_a_bucket_takes_none_of_hold_no_more_than_their_budget() {
        // Seven segments' room, of 32 MiB and their tails each: more than a framing holds.
        const BUDGET: u64 = 256 << 20;
        // What an open cell's framing holds besides, one segment at a time: its data, and its
        // framed bytes, twice as they grow (`local_cell::segments`, TASK-325).
        const FRAMING: u64 = 4 * (32 << 20);
        // And the heap's own: the open cell's and the bucket's, and a commit's buffers.
        const SLACK: u64 = 16 << 20;
        super::set_upload_memory(BUDGET).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        let options =
            local_cell::upload::Options { max_lag: u64::MAX, max_lag_bytes: u64::MAX, ..idle() };
        let open_cell = Arc::new(
            kept_in(dir.path().join("store"), (8 << 30, None), &bucket.clone(), options)
                .await
                .unwrap(),
        );
        // The writer's fence is in; the commits' segments are held.
        gated.hold.send_replace(Hold::Segments);
        let segments = 2 * local_cell::upload::IN_FLIGHT;
        let mut waiting = Vec::new();
        let data = noise(1, 1 << 20);
        let heap = crate::heap::peak_while(|| {
            futures::executor::block_on(async {
                let mut sequence = 0;
                for _ in 0..segments {
                    for _ in 0..32 {
                        sequence += 1;
                        write(&open_cell, sequence, &data).await;
                        open_cell.commit_ordered(None).await.unwrap();
                    }
                    let committing = open_cell.clone();
                    waiting.push(tokio::spawn(async move { committing.commit(None).await }));
                }
                // As many as the budget lets go at once: seven at least, or more of segments closed
                // short, as commits waiting close them.
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                let held = || gated.held.load(std::sync::atomic::Ordering::Relaxed);
                while held() < 7 && std::time::Instant::now() < deadline {
                    std::thread::yield_now();
                }
                assert!(held() >= 7, "{} uploads held", held());
            })
        });
        let memory = super::upload_memory();
        let behind = open_cell.uploader().unwrap().status().unwrap().pending_bytes;
        gated.hold.send_replace(Hold::Nothing);
        for commit in waiting {
            assert_eq!(commit.await.unwrap(), Ok(()));
        }
        eprintln!("heap {} MiB, {memory:?}, {} MiB behind", heap >> 20, behind >> 20);
        assert!(behind >= (segments as u64) << 25, "{behind} bytes behind");
        assert_eq!(memory.budget, BUDGET);
        assert!(memory.max_held <= BUDGET, "{memory:?}");
        // cladefs: and the open cells' page cache's, which a backlog holds no more of than its
        // bound (TASK-433): the backlog is on the disk.
        let pages = local_cell::pages::shared().options().most;
        assert!(heap <= BUDGET + FRAMING + SLACK + pages, "{} MiB held", heap >> 20);
    }

    /// Segments upload at once: a commit made while those before it upload goes up with them.
    #[tokio::test(flavor = "multi_thread")]
    async fn segments_upload_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        let open_cell = Arc::new(
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
                .await
                .unwrap(),
        );
        gated.hold.send_replace(Hold::Segments);
        let commits: Vec<_> = (1..=3)
            .map(|sequence| {
                let committing = open_cell.clone();
                let commit = tokio::spawn(async move {
                    write(&committing, sequence, &pattern(sequence as u8, BLOCK)).await;
                    committing.commit(None).await
                });
                gated.wait_held(sequence as usize);
                commit
            })
            .collect();
        gated.hold.send_replace(Hold::Nothing);
        for commit in commits {
            assert_eq!(commit.await.unwrap(), Ok(()));
        }
        let (extents, _) = uploaded(&bucket, open_cell.cell());
        assert_eq!(extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(), [1, 2, 3]);
    }

    /// TASK-92.15: an ordered commit that no one waits for stays in the open cell's open segment
    /// while the uploader takes the segments that commits waiting for their uploads closed: it
    /// closes the open segment for those waiting, or when it is due, not each time an upload is
    /// done. Here four segments, one for each commit waiting, upload, held in the bucket; a commit
    /// is made that no one waits for; then they land, one after another: the open cell closes none
    /// for it, which uploads once one waits for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_done_close_no_segment_for_commits_no_one_waits_for() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        let open_cell = Arc::new(
            kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
                .await
                .unwrap(),
        );
        let cell = open_cell.cell();
        // The commits' segments are 0 to 3.
        gated.hold.send_replace(Hold::Segments);
        let waiting: Vec<_> = (1..=local_cell::upload::IN_FLIGHT as u64)
            .map(|sequence| {
                let committing = open_cell.clone();
                let commit = tokio::spawn(async move {
                    write(&committing, sequence, &pattern(sequence as u8, BLOCK)).await;
                    committing.commit(None).await
                });
                gated.wait_held(sequence as usize);
                commit
            })
            .collect();
        let ordered = local_cell::upload::IN_FLIGHT as u64 + 1;
        write(&open_cell, ordered, &pattern(ordered as u8, BLOCK)).await;
        open_cell.commit_ordered(None).await.unwrap();
        // The first lands alone: its commit is acknowledged; then the rest.
        let mut waiting = waiting.into_iter();
        gated.hold.send_replace(Hold::SegmentsFrom(1));
        assert_eq!(waiting.next().unwrap().await.unwrap(), Ok(()));
        gated.hold.send_replace(Hold::Nothing);
        for commit in waiting {
            assert_eq!(commit.await.unwrap(), Ok(()));
        }
        // The open cell's segments closed, in the bucket: those of the commits waiting.
        let prefix = bucket_cell::layout::segments(cell);
        let closed = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, &prefix));
        let closed = closed.unwrap();
        assert_eq!(closed.len(), local_cell::upload::IN_FLIGHT);
        open_cell.wait_uploaded().await.unwrap();
        let (extents, _) = uploaded(&bucket, cell);
        let sequences: Vec<_> = extents.iter().map(|(sequence, _)| *sequence).collect();
        assert_eq!(sequences, (1..=ordered).collect::<Vec<_>>());
    }

    /// A cell survives the loss of its host. Cut off from object storage, the host's open cell goes
    /// on committing, ordered, until the lag reaches its bound, `max_lag` commits, then waits.
    /// Lost then, its open cell, its uploader and its disk, its commits made durable, which waited
    /// for their uploads, are all in the cell, which another host's empty open cell takes, losing
    /// the lag's commits at most; and writes on. In the objects of `objects` under `prefix`.
    async fn survives_its_hosts_loss(objects: Arc<dyn object_store::ObjectStore>, prefix: &str) {
        use bucket_cell::layout;
        use std::sync::atomic::Ordering::Relaxed;
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::over(objects.clone()));
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
                &Arc::new(ObjectBucket::new(gated.clone(), prefix).unrouted()),
                options,
            )
            .await
            .unwrap(),
        );
        let cell = lost.cell();
        write(&lost, 1, &pattern(1, BLOCK)).await;
        lost.commit(Some(b"durable")).await.unwrap();
        gated.shut.store(true, Relaxed);
        for sequence in 2..=1 + max_lag {
            write(&lost, sequence, &pattern(sequence as u8, BLOCK)).await;
            lost.commit_ordered(Some(b"ordered")).await.unwrap();
        }
        let waiting = {
            let lost = lost.clone();
            tokio::spawn(async move {
                write(&lost, 2 + max_lag, &pattern(9, BLOCK)).await;
                lost.commit_ordered(None).await
            })
        };
        let uploader = lost.uploader().unwrap();
        while uploader.stalls() == 0 {
            std::thread::yield_now();
        }
        assert!(!waiting.is_finished(), "a commit past the lag went on");
        // The uploader goes on, cut off: it closes the open cell's open segment, for the commit
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
            let footers = bytes.filter_map(|bytes| layout::Segment::footer(&bytes).ok());
            footers.flat_map(|footer| footer.contents.commits).collect()
        };
        while !framed().iter().any(|&tag| bucket_cell::layout::epoch_of(tag) == last) {
            // Returned, the commit waiting says the uploader failed, which never framed it.
            assert!(!waiting.is_finished(), "the commit waiting returned, its segment not framed");
            std::thread::yield_now();
        }
        // The host is lost: its open cell and uploader, forgotten, never reach object storage
        // again, and its disk is gone: renamed away at once, as its threads, forgotten, may still
        // make files in it, which a removal racing them finds (TASK-301: a log becoming a spare).
        std::mem::forget(lost);
        drop(waiting);
        std::fs::rename(dir.path().join("lost"), dir.path().join("gone")).unwrap();

        let bucket = Arc::new(ObjectBucket::new(objects, prefix).unrouted());
        let taken =
            kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                .await
                .unwrap();
        assert_eq!(taken.root(), Ok(b"durable".to_vec()));
        let mut data = vec![0; BLOCK];
        let durable = ExtentId { cell: cell.id, sequence: 1 };
        taken.read(durable, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert_eq!(data, pattern(1, BLOCK));
        let info = taken.info().unwrap();
        let sequence = info.next_sequence.max(info.sequence_floor);
        write(&taken, sequence, &pattern(3, BLOCK)).await;
        taken.commit(Some(b"taken")).await.unwrap();
        drop(taken);
        // The lag's commits, and no more, lost. Read back in the test's runtime, whose reactor a
        // real bucket's client needs.
        let (extents, root) = uploaded(&bucket, cell);
        let sequences: Vec<u64> = extents.iter().map(|(sequence, _)| *sequence).collect();
        assert_eq!(sequences, [1, sequence]);
        assert_eq!(root, Some(b"taken".to_vec()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cells_survive_their_hosts_loss() {
        survives_its_hosts_loss(Arc::new(InMemory::new()), "").await;
    }

    /// TASK-79.68: an extent freed while open and written anew before the commit is the cell's
    /// still once an open cell taking the cell recovers it from object storage: its segment's
    /// footer named it among the commit's frees too, which recovery applies after its extents, and
    /// the cell recovered lacked it.
    #[tokio::test(flavor = "multi_thread")]
    async fn extents_freed_and_written_anew_survive_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let lost = kept_in(dir.path().join("lost"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = lost.cell();
        let extent = write(&lost, 1, &pattern(1, BLOCK)).await;
        lost.free(extent).unwrap();
        write(&lost, 1, &pattern(2, BLOCK)).await;
        lost.commit(None).await.unwrap();
        // The host is lost, what it committed in object storage.
        std::mem::forget(lost);

        let taken = kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle())
            .await
            .unwrap();
        let mut data = vec![0; BLOCK];
        taken.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert!(data == pattern(2, BLOCK), "the extent read back is not its second write");
    }

    /// TASK-92.6: an open cell taking a cell its host lost, the cell's segments since its last
    /// checkpoint many, recovers the cell once, its head taken between the start and the rest:
    /// by the tails of its segments, which hold their footers, a window of them at once, not by
    /// the segments whole. The bucket, which the lost host's open cell probed, it probes not again.
    /// TASK-165.11: what a durable commit, an open, a fork and a fork's first open cost on a real
    /// bucket, CLADEFS_PHASES_BUCKET's (`s3://bucket/prefix`, under a prefix of the run's own), by
    /// phase: each one's time, and the bucket's requests it made, by kind, their count and time
    /// (`Stats::request_ns`), the rest local (closing and framing the segment, flushing). A
    /// measurement, not a check: ignored, run on a host that reaches the bucket, as
    /// scripts/test-host.sh runs it with `CLADEFS_TEST_PARTS=workspace` and
    /// `CLADEFS_TEST_ARGS='--run-ignored only --no-capture bucket_round_trips_by_phase'`. Each
    /// operation is a line, `PHASE ...`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a measurement against CLADEFS_PHASES_BUCKET's bucket"]
    async fn bucket_round_trips_by_phase() {
        use bucket_cell::object_bucket::Request;
        use futures::future::LocalBoxFuture;
        let url =
            std::env::var("CLADEFS_PHASES_BUCKET").expect("CLADEFS_PHASES_BUCKET names a bucket");
        let url = format!("{}/phases-{:016x}/", url.trim_end_matches('/'), rand::random::<u64>());
        let bucket = crate::host_cell::bucket(&url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut phases: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
        // Runs `operation`, a `phase`, and says what it took and asked of the bucket.
        let mut measure = async |phase: &'static str, operation: LocalBoxFuture<'_, ()>| {
            let (before, started) = (bucket.stats(), std::time::Instant::now());
            operation.await;
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
        let mut open_cell = None;
        measure(
            "create",
            Box::pin(async {
                let made = crate::open_cell::Opening::here()
                    .unwrap()
                    .with_keeping(idle())
                    .create_kept(&base, 1 << 30, crate::CellOf::Named(cell), &url)
                    .await;
                open_cell = Some(made.unwrap());
            }),
        )
        .await;
        let open_cell = open_cell.unwrap();
        for sequence in 1..=20 {
            write(&open_cell, sequence, &pattern(sequence as u8, BLOCK)).await;
            measure("durable-commit", Box::pin(async { open_cell.commit(None).await.unwrap() }))
                .await;
        }
        drop(open_cell);
        for _ in 0..10 {
            measure("open", Box::pin(async { drop(reopened(&base, idle()).await.unwrap()) })).await;
        }
        let open_cell = reopened(&base, idle()).await.unwrap();
        let forks: Vec<_> = (0..10).map(|fork| dir.path().join(format!("fork-{fork}"))).collect();
        for fork in &forks {
            let child = crate::host_cell::new_alone();
            measure(
                "fork",
                Box::pin(async {
                    open_cell.fork_to(fork, child).await.unwrap();
                }),
            )
            .await;
        }
        drop(open_cell);
        for fork in &forks {
            measure("fork-open", Box::pin(async { drop(reopened(fork, idle()).await.unwrap()) }))
                .await;
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
    #[tokio::test(flavor = "multi_thread")]
    async fn durable_commits_cost_a_create_and_a_head_swap() {
        use bucket_cell::object_bucket::Request;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(dir.path().join("store"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        write(&open_cell, 1, &pattern(1, BLOCK)).await;
        open_cell.commit(None).await.unwrap();
        let before = bucket.stats();
        write(&open_cell, 2, &pattern(2, BLOCK)).await;
        open_cell.commit(None).await.unwrap();
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
                ("append", 0),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn takes_recover_their_cells_once_by_their_segments_tails() {
        use bucket_cell::layout;
        use bucket_cell::object_bucket::Request;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        // Formatted, as a host's bucket is.
        bucket_cell::identity::format(&*bucket.own(), &Default::default()).await.unwrap();
        let lost = kept_in(dir.path().join("lost"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = lost.cell();
        // A segment of a MiB for each commit, uploaded as it is durable.
        for sequence in 1..=4 {
            write(&lost, sequence, &noise(sequence, 1 << 20)).await;
            lost.commit(None).await.unwrap();
        }
        // The host is lost, its uploader stopping where it is, with no checkpoint.
        std::mem::forget(lost);

        // cladefs: its head's slots as the take reads them, before its swap rewrites one
        // (TASK-691).
        let head: u64 = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, ""))
            .unwrap()
            .into_iter()
            .filter(|meta| matches!(layout::kind(&meta.key), layout::Kind::Head { .. }))
            .map(|meta| meta.size)
            .sum();
        let before = bucket.stats();
        let taken =
            kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                .await
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
        // Its head, read once, the recovery starting from it, then
        // swapped to take it (TASK-449, TASK-515). The segments' tails it names, all at once: no
        // listing (TASK-407).
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
                    ("append", 0),
                ],
                tails + head
            )
        );
        let mut data = vec![0; 1 << 20];
        let extent = ExtentId { cell: cell.id, sequence: 4 };
        taken.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert_eq!(data, noise(4, 1 << 20));
    }

    /// As `cells_survive_their_hosts_loss`, in the bucket CLADEFS_S3_URL names: RustFS's
    /// (scripts/test-rustfs.sh), or another S3 bucket's.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs an S3 bucket: scripts/test-rustfs.sh, CI's object-stores job"]
    async fn rustfs_cells_survive_their_hosts_loss() {
        let url = std::env::var("CLADEFS_S3_URL").expect("CLADEFS_S3_URL names the bucket to use");
        let (objects, prefix) = bucket_cell::object_bucket::object_store_at(&url).unwrap();
        // A prefix of the run's own, in a bucket runs share.
        let prefix = format!("{prefix}/{:016x}", rand::random::<u64>());
        survives_its_hosts_loss(objects, prefix.trim_start_matches('/')).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commits_wait_at_the_lag_bound() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options { interval, max_lag: 2, ..Default::default() };
        waits_at_the_lag_bound(
            options,
            |gated| &gated.shut,
            |open_cell| Box::pin(open_cell.commit_ordered(None)),
        )
        .await;
    }

    /// TASK-582: a local commit, synced to the host's disk, waits at the lag bound as an ordered
    /// one does, each level including the one before (doc-12): what a lost host costs, and what
    /// its disk holds, stays within the bound, as SQLite's and memory cells' commits, all local,
    /// go on while the bucket is out of reach.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_commits_wait_at_the_lag_bound() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options { interval, max_lag: 2, ..Default::default() };
        waits_at_the_lag_bound(
            options,
            |gated| &gated.shut,
            |open_cell| Box::pin(open_cell.commit_local(None)),
        )
        .await;
    }

    /// TASK-582: as [`local_commits_wait_at_the_lag_bound`], at a bound in bytes, as
    /// [`commits_wait_at_the_lag_bound_in_bytes`] sets it.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_commits_wait_at_the_lag_bound_in_bytes() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options {
            interval,
            max_lag_bytes: 3 * BLOCK as u64,
            ..Default::default()
        };
        waits_at_the_lag_bound(
            options,
            |gated| &gated.shut,
            |open_cell| Box::pin(open_cell.commit_local(None)),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    /// Three blocks' lag: the first two commits' at most, whether or not the uploader closed the
    /// segment between the second's write and its commit, the segment holding the write, which the
    /// commit copied into the next (TASK-92.41): their lag a block more. A bound of two blocks
    /// left the second waiting then, once in some 350 runs, and once in 60 with the uploader
    /// closing segments sooner, as it no longer waits for their framed files durable (TASK-384).
    async fn commits_wait_at_the_lag_bound_in_bytes() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options {
            interval,
            max_lag_bytes: 3 * BLOCK as u64,
            ..Default::default()
        };
        waits_at_the_lag_bound(
            options,
            |gated| &gated.shut,
            |open_cell| Box::pin(open_cell.commit_ordered(None)),
        )
        .await;
    }

    /// TASK-314: a bucket that keeps failing its writes, its disk full, holds its open cells'
    /// commits at the lag bound, failing none and losing none: as the guest sees it, its fsyncs
    /// wait once the lag reaches its bound (its syncs at once), until the bucket has room (doc-12).
    #[tokio::test(flavor = "multi_thread")]
    async fn commits_wait_at_the_lag_bound_while_the_bucket_is_full() {
        let interval = Duration::from_millis(1);
        let options = local_cell::upload::Options { interval, max_lag: 2, ..Default::default() };
        waits_at_the_lag_bound(
            options,
            |gated| &gated.full,
            |open_cell| Box::pin(open_cell.commit_ordered(None)),
        )
        .await;
    }

    /// A cell opens on a host without it: an empty open cell of it takes what the cell holds, and
    /// writes on; the open cell it came from, opened again, finds the cell moved on, and refuses.
    #[tokio::test(flavor = "multi_thread")]
    async fn opens_cells_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let first = kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = first.cell();
        let large = pattern(2, 4 << 20);
        write(&first, 1, &pattern(1, BLOCK)).await;
        write(&first, 2, &large).await;
        first.commit(Some(b"root")).await.unwrap();
        drop(first);

        let second =
            kept_in(dir.path().join("second"), (1 << 30, Some(cell)), &bucket.clone(), idle())
                .await
                .unwrap();
        assert_eq!(second.root(), Ok(b"root".to_vec()));
        let mut data = vec![0; 4 << 20];
        let extent = ExtentId { cell: cell.id, sequence: 2 };
        second.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert_eq!(data, large);
        let info = second.info().unwrap();
        let sequence = info.next_sequence.max(info.sequence_floor);
        write(&second, sequence, &pattern(3, BLOCK)).await;
        second.commit(None).await.unwrap();
        drop(second);

        // cladefs: the first, opened again, its write-ahead log empty, takes the cell as it is
        // (TASK-165.9): before, an open cell that held the cell refused once it moved on.
        let first = reopened(dir.path().join("first"), idle()).await.unwrap();
        assert_eq!(first.root(), Ok(b"root".to_vec()));
        let mut data = vec![0; BLOCK];
        let extent = ExtentId { cell: cell.id, sequence };
        first.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert_eq!(data, pattern(3, BLOCK));
        drop(first);
        let (extents, root) = uploaded(&bucket, cell);
        assert_eq!(
            extents.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(),
            [1, 2, sequence]
        );
        assert_eq!(root, Some(b"root".to_vec()));
    }

    /// Opens the cell directory at `path`, whose bucket's objects `gated` reaches, the write of its
    /// cell's head that takes it held until `meanwhile` ran.
    async fn open_racing(
        path: std::path::PathBuf,
        gated: &Arc<Gated>,
        cell: crate::CellId,
        meanwhile: impl AsyncFnOnce(),
    ) -> Result<OpenCell, crate::Error> {
        gated.hold.send_replace(Hold::FirstOfHead(cell));
        let opening = tokio::spawn(async move { reopened(path, idle()).await });
        gated.wait_held(1);
        meanwhile().await;
        gated.hold.send_replace(Hold::Nothing);
        opening.await.unwrap()
    }

    /// The directory of an open cell of `cell` at `path`, its cell kept in `bucket`, made whole,
    /// and not opened: what opening takes the cell into, as the bucket holds it.
    fn made_for(path: &std::path::Path, cell: crate::CellId, bucket: &Arc<RoutedBucket>) {
        host_fs::may_block(|| {
            std::fs::create_dir(path).unwrap();
            let description =
                local_cell::Description { block_size: BLOCK as u32, capacity: 1 << 30, cell };
            let kind = bucket_cell::cell::Kind::Fxfs;
            local_cell::create_host(path, description, &url_of(bucket), kind).unwrap();
            local_cell::made_host(path).unwrap();
        });
    }

    /// TASK-79.4: an open cell opening takes the cell as of its taking of the head, not as of what
    /// it found before: the commit another writer made meanwhile, which frees the extent the cell
    /// held before and writes another, is the cell's, and the open cell's.
    #[tokio::test(flavor = "multi_thread")]
    async fn takes_take_what_the_cell_holds_once_taken() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        let first =
            kept_in(dir.path().join("first"), (1 << 30, None), &bucket, idle()).await.unwrap();
        let cell = first.cell();
        let before = write(&first, 1, &pattern(1, BLOCK)).await;
        first.commit(Some(b"before")).await.unwrap();

        made_for(&dir.path().join("second"), cell, &bucket);
        let second = open_racing(dir.path().join("second"), &gated, cell, async || {
            write(&first, 2, &pattern(2, BLOCK)).await;
            first.free(before).unwrap();
            first.commit(Some(b"meanwhile")).await.unwrap();
        })
        .await;
        let second = second.unwrap();
        assert_eq!(second.root(), Ok(b"meanwhile".to_vec()));
        let mut data = vec![0; BLOCK];
        let written = ExtentId { cell: cell.id, sequence: 2 };
        second.read(written, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert_eq!(data, pattern(2, BLOCK));
        assert_eq!(
            uploaded(&bucket, cell),
            (vec![(2, pattern(2, BLOCK))], Some(b"meanwhile".to_vec()))
        );
    }

    /// TASK-79.4, TASK-165.9: an open cell whose write-ahead log holds commits the cell moved past
    /// refuses to open (`ESTALE`), those commits never acknowledged: before its head is taken, its
    /// log diverged as recovery first finds the cell; and once it holds it, another writer having
    /// committed between the two (`wal_follows`), having fenced that writer, whose commits stay the
    /// cell's. `OpenCell::take` drops the log, and opens the open cell as the cell is.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_cells_whose_cell_moved_on_before_their_taking_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let memory: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let gated = Arc::new(Gated::over(memory.clone()));
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        // The first host's, through a gate of its own.
        let dying = Arc::new(Gated::over(memory));
        let lost = dir.path().join("lost");
        let first = kept_in(
            &lost,
            (1 << 30, None),
            &Arc::new(ObjectBucket::new(dying.clone(), "").unrouted()),
            idle(),
        )
        .await;
        let first = first.unwrap();
        let cell = first.cell();
        write(&first, 1, &pattern(1, BLOCK)).await;
        first.commit(Some(b"before")).await.unwrap();
        // A commit its log holds, durable on its disk alone, as its host dies: restarted with its
        // disk as it was, reaching the bucket again; what it had going does nothing more.
        write(&first, 2, &pattern(2, BLOCK)).await;
        first.commit_ordered(Some(b"local")).await.unwrap();
        first.flush().await.unwrap();
        let path = dir.path().join("first");
        restart(&lost, &path);
        dying.sever();
        drop(first);
        naming(&path, &bucket);

        // The cell moves to another host, which takes it, as the bucket has it.
        let taken = kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle()).await;
        let taken = taken.unwrap();
        assert_eq!(taken.root(), Ok(b"before".to_vec()));
        // Its writer's fence moved the cell past no commit: the first's log follows it still.
        let opened = open_racing(path.clone(), &gated, cell, async || {
            let info = taken.info().unwrap();
            let sequence = info.next_sequence.max(info.sequence_floor);
            write(&taken, sequence, &pattern(3, BLOCK)).await;
            taken.commit(Some(b"meanwhile")).await.unwrap();
        })
        .await;
        assert_eq!(opened.err().and_then(|err| err.errno()), Some(crate::Errno::ESTALE));
        // The writer that committed meanwhile was fenced, its commit the cell's.
        let (extents, root) = uploaded(&bucket, cell);
        assert_eq!(extents.len(), 2);
        assert_eq!(root, Some(b"meanwhile".to_vec()));
        drop(taken);
        // Plainly diverged now, before the taking.
        assert_eq!(
            reopened(&path, idle()).await.err().and_then(|err| err.errno()),
            Some(crate::Errno::ESTALE)
        );
        // Taken, its log dropped: the open cell holds the cell as it is, its local commit lost.
        crate::open_cell::Opening::here()
            .unwrap()
            .take(&path, 1 << 30, cell, None)
            .await
            .unwrap()
            .close()
            .await;
        let first = reopened(&path, idle()).await.unwrap();
        assert_eq!(first.root(), Ok(b"meanwhile".to_vec()));
    }

    /// TASK-160.10: an open cell at a version refuses all that would change what it holds, with
    /// `EROFS`, whoever asks: writes, frees, commits, a resize, a writer taking it over. What
    /// changes none of its extents it does: it forks, and is made public.
    #[tokio::test(flavor = "multi_thread")]
    async fn copies_of_versions_refuse_what_would_change_them() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let source = kept_in(dir.path().join("source"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let extent = write(&source, 1, &pattern(1, BLOCK)).await;
        source.commit(Some(b"root")).await.unwrap();
        let path = dir.path().join("copy");
        source.uploaded_version_at(&path).await.unwrap();
        drop(source);

        let version = reopened(&path, idle()).await.unwrap();
        assert_eq!(version.access().role, crate::CellRole::Version);
        let capacity = version.info().unwrap().capacity;
        let own = ExtentId { cell: version.cell().id, sequence: 2 };
        let refused = [
            ("set_capacity", version.set_capacity(2 * capacity).await),
            ("write", version.write(own, 0, &[VolatileSlice::from(&pattern(2, BLOCK)[..])]).await),
            ("free", version.free(extent)),
            ("commit", version.commit(Some(b"changed")).await),
            ("commit_ordered", version.commit_ordered(None).await),
        ];
        for (what, result) in refused {
            assert_eq!(result, Err(crate::Errno::EROFS), "{what}");
        }
        assert_eq!(version.info().unwrap().capacity, capacity);
        assert_eq!(version.root().unwrap(), b"root");
        version
            .fork_to(dir.path().join("fork"), crate::host_cell::fork_child(version.cell()))
            .await
            .unwrap();
        host_fs::may_block(|| version.make_public().unwrap());
    }

    /// TASK-395.7: a version forks into a cell of another tenant's only while shared: refused with
    /// `EACCES` before, leaving nothing at the fork's path; shared, the fork is made; unshared, new
    /// forks are refused again. Forks into its own tenant's cells need no sharing; sharing a
    /// version the head names not is `ENOENT`. The version dropped once its cell freed what it
    /// held, and the collector run, the other tenant's fork, the one left, still reads all it
    /// inherited.
    #[tokio::test(flavor = "multi_thread")]
    async fn versions_fork_into_other_tenants_cells_while_shared() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let of = |tenant| crate::host_cell::named(tenant);
        let source = dir.path().join("source");
        let source = kept_in(&source, (1 << 30, Some(of(1))), &bucket, idle()).await.unwrap();
        let extent = write(&source, 1, &pattern(1, BLOCK)).await;
        source.commit(Some(b"base")).await.unwrap();
        let path = dir.path().join("copy");
        let version = source.uploaded_version_at(&path).await.unwrap();
        source.free(extent).unwrap();
        source.commit(Some(b"later")).await.unwrap();
        // Another fork, gone: a checkpoint past the version's, so that only the version, and its
        // forks, keep what it names.
        // Deleted as this process, no host, deletes them, of the first placement.
        let delete = async |path: &std::path::Path| {
            crate::host_cell::delete(path, Some(&bucket), crate::host_cell::token).await.unwrap()
        };
        let later = dir.path().join("later");
        source.fork_to(&later, crate::host_cell::fork_child(source.cell())).await.unwrap();
        delete(&later).await;
        let copy = reopened(&path, idle()).await.unwrap();
        let fork = async |name: &str, tenant| {
            let to = dir.path().join(name);
            let forked = copy.fork_to(&to, of(tenant)).await.map_err(|err| err.errno());
            (forked, to.exists())
        };

        assert_eq!(fork("refused", 2).await, (Err(Some(crate::Errno::EACCES)), false));
        assert_eq!(fork("ours", 1).await, (Ok(()), true));
        source.share_version(version, true).await.unwrap();
        assert_eq!(fork("theirs", 2).await, (Ok(()), true));
        source.share_version(version, false).await.unwrap();
        assert_eq!(fork("unshared", 2).await, (Err(Some(crate::Errno::EACCES)), false));
        let refused = source.share_version(version + 1, true).await;
        assert_eq!(refused.map_err(|err| err.errno()), Err(Some(crate::Errno::ENOENT)));

        drop(copy);
        delete(&dir.path().join("ours")).await;
        delete(&path).await;
        source.drop_versions(&[version].into()).await.unwrap();
        for holder in 1..=2 {
            let pass = bucket_cell::collector::collect(&*bucket, holder);
            futures::executor::block_on(pass).unwrap();
        }
        // What this process cached of the extents goes: the fork reads them from the bucket.
        crate::cache::evict_least_used(None, |_| false);
        let theirs = reopened(dir.path().join("theirs"), idle()).await.unwrap();
        assert_eq!(theirs.root().unwrap(), b"base");
        let mut data = vec![0; BLOCK];
        theirs.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert!(data == pattern(1, BLOCK), "the fork reads other bytes");
    }

    /// TASK-411.4: `points_fork_into_other_tenants_cells_while_shared`, both tenants' KEKs in the
    /// host's KMS: the version's fork into the other tenant's cell keeps the DEK it inherits
    /// rewrapped under its own cell's KEK. The version dropped, the collector run, and the
    /// version's tenant's KEK destroyed, the host told so: the clone still reads all it inherited,
    /// and the source forks no more.
    #[tokio::test(flavor = "multi_thread")]
    async fn encrypted_versions_fork_into_other_tenants_cells_while_shared() {
        let keys = tempfile::tempdir().unwrap();
        let kms = Arc::new(extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap());
        for tenant in [1, 2] {
            kms.create(tenant).unwrap();
        }
        crate::bucket::set_kms(kms.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let of = |tenant| crate::host_cell::named(tenant);
        let source = dir.path().join("source");
        let source = kept_in(&source, (1 << 30, Some(of(1))), &bucket, idle()).await.unwrap();
        let extent = write(&source, 1, &pattern(1, BLOCK)).await;
        source.commit(Some(b"base")).await.unwrap();
        let path = dir.path().join("copy");
        let version = source.uploaded_version_at(&path).await.unwrap();
        source.free(extent).unwrap();
        source.commit(Some(b"later")).await.unwrap();
        let delete = async |path: &std::path::Path| {
            crate::host_cell::delete(path, Some(&bucket), crate::host_cell::token).await.unwrap()
        };
        let copy = reopened(&path, idle()).await.unwrap();
        let fork = async |name: &str, tenant| {
            let to = dir.path().join(name);
            let forked = copy.fork_to(&to, of(tenant)).await.map_err(|err| err.errno());
            (forked, to.exists())
        };
        assert_eq!(fork("refused", 2).await, (Err(Some(crate::Errno::EACCES)), false));
        assert_eq!(fork("ours", 1).await, (Ok(()), true));
        source.share_version(version, true).await.unwrap();
        assert_eq!(fork("theirs", 2).await, (Ok(()), true));
        let theirs = reopened(dir.path().join("theirs"), idle()).await.unwrap();
        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, theirs.cell()));
        let record = record.unwrap().unwrap();
        let inherited: Vec<_> = record.inherited.iter().map(|had| had.cell).collect();
        assert_eq!((record.deks.len(), inherited), (1, vec![source.cell()]));
        drop(theirs);

        drop(copy);
        delete(&dir.path().join("ours")).await;
        delete(&path).await;
        source.drop_versions(&[version].into()).await.unwrap();
        for holder in 1..=2 {
            let pass = bucket_cell::collector::collect(&*bucket, holder);
            futures::executor::block_on(pass).unwrap();
        }
        kms.destroy(1).unwrap();
        crate::bucket::kms().unwrap().revoked(1, "tenant 1: its KEK is destroyed");
        crate::cache::evict_least_used(None, |_| false);
        let theirs = reopened(dir.path().join("theirs"), idle()).await.unwrap();
        assert_eq!(theirs.root().unwrap(), b"base");
        let mut data = vec![0; BLOCK];
        theirs.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])]).await.unwrap();
        assert!(data == pattern(1, BLOCK), "the fork reads other bytes");
        let refused =
            source.fork_to(dir.path().join("after"), of(1)).await.map_err(|err| err.errno());
        assert_eq!(refused, Err(Some(crate::Errno::EACCES)));
    }

    /// TASK-411.19, as TASK-411.28 left it: a tenant whose KEK the KMS has forgotten (answering
    /// as for a tenant that never had one), and a tenant that never had one, get no open cell
    /// opened here, encrypted or in the clear: `EACCES`, the cell not made.
    #[tokio::test(flavor = "multi_thread")]
    async fn tenants_with_no_kek_in_the_kms_open_no_cell() {
        let keys = tempfile::tempdir().unwrap();
        let local = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
        local.create(1).unwrap();
        local.destroy(1).unwrap();
        crate::bucket::set_kms(Arc::new(extent_keys::testing::Forgetting::new(local))).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let of = |tenant| crate::host_cell::named(tenant);
        for (name, tenant) in [("forgotten", 1), ("never", 2)] {
            let cell = of(tenant);
            let refused =
                kept_in(dir.path().join(name), (1 << 30, Some(cell)), &bucket, idle()).await;
            let refused = refused.map(drop).map_err(|err| err.errno());
            assert_eq!(refused, Err(Some(crate::Errno::EACCES)), "{name}");
            let cell = futures::executor::block_on(bucket_cell::head::get(&*bucket, cell));
            assert_eq!(cell, Ok(None), "{name}");
        }
    }

    /// TASK-411.7: a host told a cell is shredded, or a tenant's KEK destroyed, forgets their keys:
    /// their open cells open here are halted, every request failing with `EACCES`, as their
    /// openings do; their extents leave the extent cache, pages and all; its fetchers forget their
    /// rings. The tenant's other cell's open cell, until its tenant goes, and another tenant's,
    /// read on, their extents cached still. No file of the cell directories holds what they wrote.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_cells_of_shredded_cells_and_tenants_are_halted_and_leave_the_cache() {
        let keys = tempfile::tempdir().unwrap();
        let kms = Arc::new(extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap());
        for tenant in [1, 2] {
            kms.create(tenant).unwrap();
        }
        crate::bucket::set_kms(kms.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let of = |tenant| crate::host_cell::named(tenant);
        let names = ["ours", "sibling", "theirs"];
        let mut open_cells: Vec<(OpenCell, ExtentId, Vec<u8>)> = Vec::new();
        for (at, (name, tenant)) in names.iter().zip([1, 1, 2]).enumerate() {
            let open_cell =
                kept_in(dir.path().join(name), (1 << 30, Some(of(tenant))), &bucket, idle()).await;
            let open_cell = open_cell.unwrap();
            let data = noise(at as u64 + 1, 4 * BLOCK);
            let extent = write(&open_cell, 1, &data).await;
            open_cell.commit(None).await.unwrap();
            open_cell.wait_uploaded().await.unwrap();
            open_cells.push((open_cell, extent, data));
        }
        let read = async |at: usize| {
            let (open_cell, extent, data) = &open_cells[at];
            let mut read = vec![0; data.len()];
            let done = open_cell
                .read(*extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])])
                .await;
            done.map(|()| read == *data)
        };
        let cached = || {
            let extents: std::collections::BTreeSet<ExtentId> =
                crate::cache::keys().into_iter().map(|(_, extent)| extent).collect();
            open_cells.iter().map(|(_, extent, _)| extents.contains(extent)).collect::<Vec<_>>()
        };
        assert_eq!(
            futures::future::join_all((0..3).map(read)).await,
            [Ok(true), Ok(true), Ok(true)]
        );
        assert_eq!(cached(), [true, true, true]);
        let reopened_errno = async |at: usize| {
            reopened(dir.path().join(names[at]), idle()).await.map(drop).map_err(|err| err.errno())
        };

        let cell = open_cells[0].0.cell();
        let shredded = futures::executor::block_on(crate::bucket::shred_cell(&*bucket, cell));
        let (removed, forgotten) = shredded.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            (forgotten.stores, forgotten.rings, forgotten.extents),
            (1, 1, 1),
            "{forgotten:?}"
        );
        assert_eq!(
            futures::future::join_all((0..3).map(read)).await,
            [Err(crate::Errno::EACCES), Ok(true), Ok(true)]
        );
        assert_eq!(cached(), [false, true, true]);
        assert_eq!(open_cells[0].0.commit(None).await, Err(crate::Errno::EACCES));
        // Nor what the control API changes of it: its capacity, its versions (TASK-611).
        let ours = &open_cells[0].0;
        assert_eq!(ours.set_capacity(2 << 30).await, Err(crate::Errno::EACCES));
        let dropped = ours.drop_versions(&[1].into()).await.map_err(|err| err.errno());
        assert_eq!(dropped, Err(Some(crate::Errno::EACCES)));
        let shared = ours.share_version(1, true).await.map_err(|err| err.errno());
        assert_eq!(shared, Err(Some(crate::Errno::EACCES)));

        kms.destroy(1).unwrap();
        let why = "tenant 1: its KEK is destroyed";
        let forgotten = crate::bucket::forget(bucket_cell::keys::Scope::Tenant(1), why);
        assert_eq!(
            (forgotten.stores, forgotten.rings, forgotten.extents),
            (1, 1, 1),
            "{forgotten:?}"
        );
        assert_eq!(
            futures::future::join_all((0..3).map(read)).await,
            [Err(crate::Errno::EACCES), Err(crate::Errno::EACCES), Ok(true)]
        );
        assert_eq!(cached(), [false, false, true]);
        assert_eq!(crate::bucket::kms().unwrap().in_pool(1), 0);

        // Closed by their holders, the open cells of the tenant open no more; the other's does.
        let written: Vec<Vec<u8>> = open_cells.into_iter().map(|(_, _, data)| data).collect();
        assert_eq!(
            futures::future::join_all((0..3).map(reopened_errno)).await,
            [Err(Some(crate::Errno::EACCES)), Err(Some(crate::Errno::EACCES)), Ok(())]
        );

        let pieces: Vec<&[u8]> = written[..2].iter().map(|data| &data[..64]).collect();
        let held = crate::testing::files_holding(dir.path(), &pieces);
        assert_eq!(held, Vec::<String>::new(), "files holding a cell's data");
    }

    /// board 5550: an addition of a bucket opens each bucket with its own number's options: the
    /// environment's own, an endpoint refusing every request (`403`, as credentials no bucket
    /// takes), of no use to any; each bucket's, its number's (`CLADEFSD_BUCKET_<N>_AWS_ENDPOINT`),
    /// the server holding it. Buckets 2 and 3 added, numbered so, each bucket's registry the
    /// root's, of all three.
    #[tokio::test(flavor = "multi_thread")]
    async fn additions_open_each_bucket_with_its_own_numbers_options() {
        use bucket_cell::identity::Details;
        use bucket_cell::s3_server::{BUCKET, S3Server};
        let (serving, refusing) = (S3Server::start(None), S3Server::start(None));
        refusing.answering(Some(403));
        let endpoint = |server: &S3Server| {
            let options = server.options().into_iter();
            options.into_iter().find(|(name, _)| name == "aws_endpoint").unwrap().1
        };
        let mut vars: Vec<(String, String)> = (refusing.options().into_iter())
            .map(|(name, value)| (name.to_ascii_uppercase(), value))
            .collect();
        let numbered =
            |number| (format!("CLADEFSD_BUCKET_{number}_AWS_ENDPOINT"), endpoint(&serving));
        vars.extend((1..=3).map(numbered));
        for (name, value) in vars {
            // SAFETY: nextest runs each test in a process of its own, whose other threads don't
            // read the environment yet.
            unsafe { std::env::set_var(name, value) };
        }
        let url = |name: &str| format!("s3://{BUCKET}/{name}");
        let add = |new: &str| {
            let added = super::add_bucket(&url("one"), &url(new), Details::default());
            added.unwrap().number
        };
        assert_eq!((add("two"), add("three")), (2, 3));
        let registry = |name: &str| {
            let bucket = ObjectBucket::new(serving.objects(), name);
            let read = local_cell::upload::block_on(bucket_cell::identity::registry(&bucket));
            read.unwrap().unwrap().unwrap()
        };
        let root = registry("one");
        let numbers: Vec<u16> = root.buckets.iter().map(|entry| entry.number).collect();
        assert_eq!(numbers, [1, 2, 3]);
        assert_eq!((registry("two"), registry("three")), (root.clone(), root));
    }

    /// board 8365: `add_bucket` verifies every target bucket's identity before spreading the
    /// updated registry, so a registry entry pointing at an unrelated bucket (even an older root
    /// bucket with a lower registry version) is refused and left untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn add_bucket_refuses_to_overwrite_an_unrelated_buckets_registry() {
        use bucket_cell::identity::Details;
        let dir = tempfile::tempdir().unwrap();
        let url_of = |name: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            url::Url::from_directory_path(&path).unwrap().to_string()
        };
        let (root_a, second, third, root_b) =
            (url_of("root_a"), url_of("second"), url_of("third"), url_of("root_b"));

        super::format_bucket(&root_a, Details::default()).unwrap();
        super::format_bucket(&root_b, Details::default()).unwrap();
        super::add_bucket(&root_a, &second, Details::default()).unwrap();

        // Snapshot root_b's registry bytes before swapping it into `second`'s path.
        let root_b_buckets = std::fs::read(dir.path().join("root_b/buckets")).unwrap();
        let root_b_before = super::bucket_registry(&root_b).unwrap().unwrap();
        assert_eq!(root_b_before.version, 1);

        // Swap `second` out and place `root_b` at `second`'s URL so root_a's registry entry #2
        // now points at the unrelated root bucket `root_b`.
        std::fs::rename(dir.path().join("second"), dir.path().join("second_orig")).unwrap();
        std::fs::rename(dir.path().join("root_b"), dir.path().join("second")).unwrap();

        let err = super::add_bucket(&root_a, &third, Details::default()).unwrap_err();
        assert_eq!(err.errno(), Some(crate::Errno::ESTALE), "unexpected error: {err:#}");

        // The unrelated bucket's registry bytes must be completely intact.
        let root_b_after = std::fs::read(dir.path().join("second/buckets")).unwrap();
        assert_eq!(root_b_after, root_b_buckets);
    }

    /// Board 6908: a host whose bucket, `CLADEFSD_BUCKET`, can't be opened has no KMS: its own alone,
    /// other buckets' cells would be wrapped under it rather than their own bucket's.
    #[test]
    fn hosts_whose_bucket_cant_be_opened_have_no_kms() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: this test's process is its own (nextest), no open cell open yet.
        unsafe {
            std::env::remove_var("CLADEFS_MASTER_KEY");
            std::env::set_var("CLADEFS_KMS", format!("local://{}", dir.path().display()));
            std::env::set_var("CLADEFSD_BUCKET", "nowhere://bucket");
        }
        assert_eq!(
            crate::bucket::require_kms(),
            Err(
                "the host's KMS: nowhere://bucket: nowhere://bucket: Generic URL error: Unable to \
                 recognise URL \"nowhere://bucket\""
                    .to_owned()
            )
        );
    }

    /// TASK-411.27: a host given a master key (`CLADEFS_MASTER_KEY`) and no `CLADEFS_KMS` keeps its
    /// tenants' KEKs in its bucket: every tenant's open cell is encrypted, its KEK made on its
    /// first use at `tenants/<t>/kek`. Shredding a tenant here leaves a tombstone there, halts its
    /// open cell (`EACCES`), which opens no more, nor any new open cell of the tenant's, nothing
    /// made; another tenant's reads on. Shredding it again finds it so.
    #[tokio::test(flavor = "multi_thread")]
    async fn hosts_given_a_master_key_alone_encrypt_every_cell_under_the_bucket_kms() {
        use bucket_cell::bucket_kms::Destroyed;
        use bucket_cell::layout;
        let url = "memory:///bucket-kms";
        let master: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
        // SAFETY: this test's process is its own (nextest), no open cell open yet.
        unsafe {
            std::env::remove_var("CLADEFS_KMS");
            std::env::set_var("CLADEFSD_BUCKET", url);
            std::env::set_var("CLADEFS_MASTER_KEY", &master);
        }
        let bucket = crate::host_cell::bucket(url).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let of = |tenant| crate::host_cell::named(tenant);
        let names = ["ours", "theirs"];
        let mut open_cells: Vec<(OpenCell, ExtentId, Vec<u8>)> = Vec::new();
        for (at, (name, tenant)) in names.iter().zip([1, 2]).enumerate() {
            let open_cell =
                kept_in(dir.path().join(name), (1 << 30, Some(of(tenant))), &bucket, idle()).await;
            let open_cell = open_cell.unwrap();
            let data = noise(at as u64 + 1, 4 * BLOCK);
            let extent = write(&open_cell, 1, &data).await;
            open_cell.commit(None).await.unwrap();
            open_cell.wait_uploaded().await.unwrap();
            open_cells.push((open_cell, extent, data));
        }
        assert!(crate::bucket::bucket_kms().is_some());
        for (open_cell, _, _) in &open_cells {
            let record =
                futures::executor::block_on(bucket_cell::head::get(&*bucket, open_cell.cell()));
            assert_eq!(record.unwrap().unwrap().deks.len(), 1);
            let cell =
                futures::executor::block_on(bucket_cell::head::get(&*bucket, open_cell.cell()));
            assert_eq!(cell.unwrap().unwrap().keks.len(), 1);
        }
        let kek = |tenant| {
            let key = layout::kek(tenant);
            let got = futures::executor::block_on(bucket_cell::bucket::get(&*bucket, &key));
            got.unwrap().map(|object| object.bytes)
        };
        assert_eq!(kek(1).map(|bytes| bytes.len()), Some(extent_keys::kms::bucket::KEK_OBJECT_LEN));
        let read = async |at: usize| {
            let (open_cell, extent, data) = &open_cells[at];
            let mut read = vec![0; data.len()];
            let done = open_cell
                .read(*extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])])
                .await;
            done.map(|()| read == *data)
        };

        let shredded = futures::executor::block_on(crate::bucket::shred_tenant(1));
        let (destroyed, forgotten) = shredded.unwrap();
        assert_eq!((destroyed, forgotten.stores, forgotten.rings), (Destroyed::Now, 1, 1));
        assert_eq!(kek(1), Some(b"fxkt\x01\x02\0\0\0\0\0\0\0\x01".to_vec()));
        assert_eq!((read(0).await, read(1).await), (Err(crate::Errno::EACCES), Ok(true)));
        let written: Vec<Vec<u8>> = open_cells.into_iter().map(|(_, _, data)| data).collect();
        let reopened_errno = async |name: &str| {
            reopened(dir.path().join(name), idle()).await.map(drop).map_err(|err| err.errno())
        };
        assert_eq!(reopened_errno("ours").await, Err(Some(crate::Errno::EACCES)));
        assert_eq!(reopened_errno("theirs").await, Ok(()));
        let new = of(1);
        let refused = kept_in(dir.path().join("new"), (1 << 30, Some(new)), &bucket, idle()).await;
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
            crate::bucket::ShredError::Unsupported(
                "tenant 1: this host's KMS is not the bucket KMS (CLADEFS_MASTER_KEY, no CLADEFS_KMS): \
                 destroy the tenant's KEK in its KMS, then have every host forget its keys"
                    .to_owned()
            )
        );
    }

    /// TASK-395.9: an open cell with a writer forks into a cell of another tenant's only if it is
    /// the default tenant's (0). Another tenant's open cell is refused with `EACCES` by every kind
    /// of fork, into any other tenant's cell, the default tenant's too, leaving nothing at the
    /// fork's path nor in the bucket; into its own tenant's it forks. An open cell of the default
    /// tenant's, as public bases are, forks into two tenants' cells, each reading what it holds.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_cells_with_writers_fork_into_other_tenants_cells_from_the_common_tenant_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let of = |tenant| crate::host_cell::named(tenant);
        let listed = || {
            let listed = futures::executor::block_on(bucket_cell::Bucket::list(&*bucket, ""));
            listed.unwrap()
        };
        let open_cell = async |name: &str, tenant| {
            let open_cell =
                kept_in(dir.path().join(name), (1 << 30, Some(of(tenant))), &bucket, idle()).await;
            let open_cell = open_cell.unwrap();
            write(&open_cell, 1, &pattern(tenant as u8, BLOCK)).await;
            open_cell.commit(Some(name.as_bytes())).await.unwrap();
            open_cell.wait_uploaded().await.unwrap();
            open_cell
        };

        let acme = open_cell("acme", 1).await;
        let before = listed();
        let errno = |forked: Result<(), crate::Error>| forked.map_err(|err| err.errno());
        for tenant in [2, crate::host_cell::DEFAULT_TENANT] {
            let to = dir.path().join(format!("refused-{tenant}"));
            let refused = [
                ("fork", errno(acme.fork(&to, tenant).await.map(drop))),
                ("fork_quiescent", errno(acme.fork_quiescent(&to, tenant).await.map(drop))),
                ("fork_to", errno(acme.fork_to(&to, of(tenant)).await)),
            ];
            for (how, refused) in refused {
                assert_eq!(refused, Err(Some(crate::Errno::EACCES)), "{how} to tenant {tenant}");
                assert!(!to.exists(), "{how} to tenant {tenant} made a cell");
            }
        }
        assert_eq!(listed(), before, "a refused fork wrote to the bucket");
        let ours = dir.path().join("ours");
        assert_eq!(acme.fork(&ours, 1).await.unwrap().tenant, 1);
        assert_eq!(reopened(&ours, idle()).await.unwrap().root().unwrap(), b"acme");

        let base = open_cell("base", crate::host_cell::DEFAULT_TENANT).await;
        for tenant in [1, 2] {
            let to = dir.path().join(format!("base-{tenant}"));
            assert_eq!(base.fork(&to, tenant).await.unwrap().tenant, tenant);
            let fork = reopened(&to, idle()).await.unwrap();
            assert_eq!(fork.root().unwrap(), b"base");
            let mut data = vec![0; BLOCK];
            let extent = ExtentId { cell: base.cell().id, sequence: 1 };
            fork.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut data[..])])
                .await
                .unwrap();
            assert!(data == pattern(0, BLOCK), "tenant {tenant}'s fork reads other bytes");
        }
    }

    /// TASK-79.39: a fork's child takes its cell through its marker, which names the fork's
    /// commit, the last the child inherits, though a later commit's segment is acknowledged with
    /// the fork's.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_name_their_commit_though_later_ones_are_acknowledged_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let gated = Arc::new(Gated::new());
        let bucket = Arc::new(ObjectBucket::new(gated.clone(), "").unrouted());
        let parent = Arc::new(
            kept_in(dir.path().join("parent"), (1 << 30, None), &bucket.clone(), idle())
                .await
                .unwrap(),
        );
        let cell = parent.cell();
        write(&parent, 1, &pattern(1, BLOCK)).await;
        parent.commit(Some(b"root")).await.unwrap();
        // The fork's segment, after the commit's, lands after the next.
        let segment = |number| bucket_cell::layout::segment(cell, number, 1);
        let (forking, next) = (segment(1), segment(2));
        gated.hold.send_replace(Hold::Key(forking));
        let child = crate::host_cell::fork_child(cell);
        let fork = {
            let (parent, path) = (parent.clone(), dir.path().join("child"));
            tokio::spawn(async move { parent.fork_to(path, child).await })
        };
        gated.wait_held(1);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let later = {
            let parent = parent.clone();
            tokio::spawn(async move {
                write(&parent, 2, &pattern(2, BLOCK)).await;
                parent.commit(None).await
            })
        };
        let landed =
            || futures::executor::block_on(bucket_cell::Bucket::head(&*bucket, &next)).is_ok();
        while !landed() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(landed(), "the commit after the fork never uploaded");
        gated.hold.send_replace(Hold::Nothing);
        assert_eq!(later.await.unwrap(), Ok(()));
        fork.await.unwrap().unwrap();

        let forked = reopened(dir.path().join("child"), idle()).await.unwrap();
        assert_eq!(forked.root(), Ok(b"root".to_vec()));
    }

    /// TASK-150.6: an extent's ID is its cell's to write once, on any host: one sealed and freed
    /// is spent on a host that takes the cell, whose local index knows nothing of it, whether
    /// the cell's state there comes from a checkpoint or from its segments' footers; and one
    /// freed while open, never sealed, as a journal's tail is, is written still.
    #[tokio::test(flavor = "multi_thread")]
    async fn extents_freed_are_spent_on_the_host_that_takes_their_cell() {
        for checkpointed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
            let first = kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle())
                .await
                .unwrap();
            let cell = first.cell();
            let sealed = write(&first, 1, &pattern(1, BLOCK)).await;
            first.commit(Some(b"root")).await.unwrap();
            first.free(sealed).unwrap();
            let open = write(&first, 2, &pattern(2, BLOCK)).await;
            first.free(open).unwrap();
            first.commit(None).await.unwrap();
            // The open cell closing checkpoints the cell; a host lost doesn't.
            if checkpointed {
                drop(first);
            } else {
                std::mem::forget(first);
            }

            let taken = kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle())
                .await
                .unwrap();
            let data = pattern(3, BLOCK);
            let again = [crate::VolatileSlice::from(&data[..])];
            assert_eq!(
                taken.write(sealed, 0, &again).await,
                Err(crate::Errno::EPERM),
                "{checkpointed}"
            );
            assert_eq!(taken.write(open, 0, &again).await, Ok(()), "{checkpointed}");
        }
    }

    /// TASK-165.3: an extent sealed by one commit and freed by the next, both in one segment, whose
    /// footer frees it and seals it not, is spent on a host that takes the cell, as it is on the
    /// open cell that made it: every reader of a footer spends every free of the cell's own
    /// extents.
    #[tokio::test(flavor = "multi_thread")]
    async fn extents_sealed_and_freed_in_one_segment_are_spent_where_their_cell_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let first = kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = first.cell();
        let sealed = write(&first, 1, &pattern(1, BLOCK)).await;
        first.commit_ordered(Some(b"root")).await.unwrap();
        first.free(sealed).unwrap();
        first.commit(None).await.unwrap();
        let footers = footers(&bucket, cell);
        let last = footers.last().unwrap();
        let freed = storage_device::ExtentId::new(cell.id, sealed.sequence);
        let contents = &last.contents;
        let (commits, extents) = (contents.commits.len(), contents.extents.len());
        assert_eq!((commits, extents, &contents.frees), (1, 0, &vec![freed]));
        let data = pattern(3, BLOCK);
        let again = [crate::VolatileSlice::from(&data[..])];
        assert_eq!(first.write(sealed, 0, &again).await, Err(crate::Errno::EPERM));
        // Its host lost: the cell's state is its footers'.
        std::mem::forget(first);

        let taken = kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle())
            .await
            .unwrap();
        assert_eq!(taken.write(sealed, 0, &again).await, Err(crate::Errno::EPERM));
    }

    /// TASK-141.2: a cell whose record says a format newer than this daemon's is refused, naming
    /// the cell and the format, before anything of it is read or written: no open cell takes it,
    /// its head is as it was, and no segment of it is written.
    #[tokio::test(flavor = "multi_thread")]
    async fn cells_of_newer_formats_are_refused() {
        use bucket_cell::Bucket as _;
        use bucket_cell::bucket::PutMode;
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let first = kept_in(dir.path().join("first"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let cell = first.cell();
        write(&first, 1, &pattern(1, BLOCK)).await;
        first.commit(Some(b"root")).await.unwrap();
        drop(first);
        // A later release's record, whose format says first what follows, which this one can't
        // read.
        let mut newer = (bucket_cell::layout::FORMAT.current + 1).to_le_bytes().to_vec();
        newer.extend_from_slice(b"what a later format holds");
        let (key, overwrite) = (bucket_cell::layout::head(cell), PutMode::Overwrite);
        futures::executor::block_on(bucket.put(&key, newer.into(), overwrite)).unwrap();
        let listed = || {
            let listed = futures::executor::block_on(bucket.list("")).unwrap();
            listed.into_iter().map(|meta| (meta.key, meta.version)).collect::<Vec<_>>()
        };
        let before = listed();

        let taken = kept_in(dir.path().join("taken"), (1 << 30, Some(cell)), &bucket, idle()).await;
        let refused = taken.err().unwrap();
        let format = bucket_cell::layout::FORMAT;
        let named =
            format!("cell {cell}: {}", formats::Refused { format, found: format.current + 1 });
        assert!(format!("{refused:#}").contains(&named), "{refused:#}");
        assert_eq!(refused.errno(), Some(crate::Errno::ENOTSUP));
        assert_eq!(listed(), before, "a cell of a newer format was written to");
    }

    /// Forks `parent`, attached to `bucket`, to `child`, whose handle is at `path`, the parent
    /// holding an extent and a root: returns the child's record.
    async fn forked(
        parent: &OpenCell,
        bucket: &Arc<RoutedBucket>,
        path: &std::path::Path,
        child: crate::CellId,
    ) -> bucket_cell::layout::Head {
        write(parent, 1, &pattern(1, BLOCK)).await;
        parent.commit(Some(b"root")).await.unwrap();
        parent.fork_to(path, child).await.unwrap();
        futures::executor::block_on(bucket_cell::head::get(&**bucket, child)).unwrap().unwrap()
    }

    /// TASK-79.78: a fork to a child it made before, as a fork retried to another path, fails
    /// having made nothing, the bucket having the child's record: no commit of the parent's names
    /// the child again, from another version, which its writers, finding the child forked twice,
    /// would fail on for ever, the parent's cell taken by none again. The parent attaches again.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_to_a_child_forked_before_make_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let parent = kept_in(dir.path().join("parent"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let child = crate::host_cell::fork_child(parent.cell());
        forked(&parent, &bucket, &dir.path().join("child"), child).await;
        let again = dir.path().join("again");
        let forked = parent.fork_to(&again, child).await;
        let errno = forked.as_ref().map(drop).map_err(crate::Error::errno);
        let forked = forked.map_err(|err| err.to_string());
        assert_eq!((forked, again.exists()), (anothers(child), false));
        assert_eq!(errno, Err(Some(crate::Errno::EEXIST)));
        drop(parent);

        reopened(dir.path().join("parent"), idle()).await.unwrap();
    }

    /// The host whose disk is at `from`, crashed, restarted with its disk as the crash left it at
    /// `to`: its open cells copied there, no open cell of them open. What the crashed host's writes
    /// had in flight may land still, as a dead host's PUTs may.
    ///
    /// Copied again if a file it listed went before it copied it, renamed or removed by the
    /// host's open cells meanwhile, as they go on: a crash leaves a renamed file under one name or
    /// the other, where a copy that listed it under the first would have neither. The open cells
    /// rename files on threads of their own, framings made durable and the next segment's log
    /// placed ahead of it (TASK-384), which a copy raced.
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

    /// A fork, waiting in a task of its own: what it gives.
    type Forking = tokio::task::JoinHandle<Result<(), crate::Error>>;

    /// What made the cell of `head`, and whether it lives: as the tests that are not of owners,
    /// keys or takings compare heads.
    fn made(head: bucket_cell::layout::Head) -> (bucket_cell::record::Made, bool) {
        (head.made, !head.deleted())
    }

    /// What [`made`] says of a cell no fork made, its open cell's own, of no parent, live.
    const OWN: (bucket_cell::record::Made, bool) = (bucket_cell::record::Made::Root, true);

    /// The open cell of another cell, `cell`, at `path`, attached to `bucket`, holding an extent
    /// and a root: what a guest may name as its fork's child.
    async fn others(
        path: &std::path::Path,
        bucket: &Arc<RoutedBucket>,
        cell: crate::CellId,
    ) -> OpenCell {
        let others = kept_in(path, (1 << 30, Some(cell)), &bucket.clone(), idle()).await.unwrap();
        write(&others, 1, &pattern(3, BLOCK)).await;
        others.commit(Some(b"other")).await.unwrap();
        others
    }

    /// What an open cell committing, and its commit uploading, gave, and what its cell then holds.
    type Committed = (Result<(), crate::Errno>, Result<(), crate::Errno>, Uploaded);

    /// Whether the cell of `others`, its open cell, is live still: it commits, and uploads its
    /// commits.
    async fn live(others: &OpenCell, bucket: &Arc<RoutedBucket>) -> Committed {
        write(others, 2, &pattern(4, BLOCK)).await;
        let committed = others.commit(Some(b"again")).await;
        (committed, others.wait_uploaded().await, uploaded(bucket, others.cell()))
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
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_to_a_cell_the_bucket_has_fail_with_eexist() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let parent = kept_in(dir.path().join("parent"), (1 << 30, None), &bucket.clone(), idle())
            .await
            .unwrap();
        let other = crate::host_cell::fork_child(parent.cell());
        let others = others(&dir.path().join("other"), &bucket, other).await;
        write(&parent, 1, &pattern(1, BLOCK)).await;
        parent.commit(Some(b"root")).await.unwrap();
        let path = dir.path().join("child");
        let forked = parent.fork_to(&path, other).await.map_err(|err| err.errno());
        assert_eq!((forked, path.exists()), (Err(Some(crate::Errno::EEXIST)), false));

        // The parent commits and uploads on, its cell naming no fork.
        parent.commit(Some(b"later")).await.unwrap();
        assert_eq!(parent.wait_uploaded().await, Ok(()));
        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, other)).unwrap();
        assert_eq!((record.map(made), live(&others, &bucket).await), (Some(OWN), lives()));
    }

    /// A parent attached to the bucket through `gated`, in `dir`, holding an extent and a root,
    /// and its fork to a child whose handle is `path`, waiting in a thread of its own for the swap
    /// of the parent's head that names the fork's segment, which `gated` holds: the parent's
    /// cell and the child.
    async fn forking_held(
        dir: &std::path::Path,
        gated: &Arc<Gated>,
        path: &std::path::Path,
    ) -> (crate::CellId, crate::CellId, Forking) {
        let parent = Arc::new(
            kept_in(
                dir.join("parent"),
                (1 << 30, None),
                &Arc::new(ObjectBucket::new(gated.clone(), "").unrouted()),
                idle(),
            )
            .await
            .unwrap(),
        );
        let cell = parent.cell();
        write(&parent, 1, &pattern(1, BLOCK)).await;
        parent.commit(Some(b"root")).await.unwrap();
        gated.hold.send_replace(Hold::Head(cell));
        let child = crate::host_cell::fork_child(cell);
        let fork = {
            let path = path.to_owned();
            tokio::spawn(async move { parent.fork_to(path, child).await })
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
    /// was being made, once it found the ID free, as an open cell of that cell made elsewhere then,
    /// fails, its parent's writer finding the child's record another's: and its cleanup deletes
    /// the child here alone, leaving the other cell as it was. A fork deletes no cell it did
    /// not make, whatever ID its guest gives. And it harms no parent: its writers go on, as a
    /// writer taking its cell again does, its footer naming a child that is another's.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_failed_leave_cells_that_took_their_childs_id_be() {
        let dir = tempfile::tempdir().unwrap();
        let memory: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let gated = Arc::new(Gated::over(memory.clone()));
        let bucket = Arc::new(ObjectBucket::new(memory, "").unrouted());
        let path = dir.path().join("child");
        let (_, child, fork) = forking_held(dir.path(), &gated, &path).await;
        let others = others(&dir.path().join("other"), &bucket, child).await;
        gated.hold.send_replace(Hold::Nothing);
        let forked = fork.await.unwrap();
        let errno = forked.as_ref().map(drop).map_err(crate::Error::errno);
        let forked = forked.map_err(|err| err.to_string());
        assert_eq!((forked, errno), (anothers(child), Err(Some(crate::Errno::EEXIST))));
        assert!(!path.exists(), "the handle of a fork that failed is left");

        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, child)).unwrap();
        assert_eq!((record.map(made), live(&others, &bucket).await), (Some(OWN), lives()));
        let parent = reopened(dir.path().join("parent"), idle()).await.unwrap();
        parent.commit(Some(b"later")).await.unwrap();
        assert_eq!(parent.wait_uploaded().await, Ok(()));
    }

    /// TASK-601 (board 2598): `shred_cell` evicts cached extents of a cell even after every
    /// `OpenCell` of that cell has already been closed.
    #[tokio::test(flavor = "multi_thread")]
    async fn shred_cell_evicts_cached_extents_of_closed_store() {
        let keys = tempfile::tempdir().unwrap();
        let kms = Arc::new(extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap());
        kms.create(1).unwrap();
        crate::bucket::set_kms(kms).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
        let open_cell = kept_in(
            dir.path().join("closed"),
            (1 << 30, Some(crate::host_cell::named(1))),
            &bucket,
            idle(),
        )
        .await
        .unwrap();
        let cell = open_cell.cell();
        let data = noise(1, 4 * BLOCK);
        let extent = write(&open_cell, 1, &data).await;
        open_cell.commit(None).await.unwrap();
        open_cell.wait_uploaded().await.unwrap();
        let mut buf = vec![0u8; data.len()];
        open_cell
            .read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut buf[..])])
            .await
            .unwrap();
        assert_eq!(buf, data);
        // Close the store completely before shredding.
        open_cell.close().await;
        assert!(
            crate::cache::keys().iter().any(|(_, e)| *e == extent),
            "closed store's extent should still be cached before shred_cell"
        );

        let (removed, forgotten) = crate::bucket::shred_cell(&*bucket, cell).await.unwrap();
        assert_eq!(removed, 1);
        assert!(
            forgotten.extents >= 1,
            "shred_cell must evict closed store's cached extents: {forgotten:?}"
        );
        assert!(
            !crate::cache::keys().iter().any(|(_, e)| *e == extent),
            "closed store's extent remained in cache after shred_cell"
        );
    }

    /// board 5552: `shred_tenant` when no bucket destroyed the tenant's KEK (`destroyed` empty)
    /// must return an error without calling `forget`, leaving live stores serving and the KMS
    /// cache unpoisoned.
    #[tokio::test(flavor = "multi_thread")]
    async fn shred_tenant_with_no_destroyed_bucket_does_not_halt_or_poison_cache() {
        use bucket_cell::s3_server::{BUCKET, S3Server};
        let server = S3Server::start(None);
        let url = format!("s3://{BUCKET}/shred-5552");
        let master: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
        // SAFETY: this test's process is its own (nextest), no open cell open yet.
        unsafe {
            std::env::remove_var("CLADEFS_KMS");
            std::env::set_var("CLADEFSD_BUCKET", &url);
            std::env::set_var("CLADEFS_MASTER_KEY", &master);
            for (name, value) in server.options() {
                std::env::set_var(name.to_ascii_uppercase(), value);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let bucket = crate::host_cell::bucket(&url).unwrap();
        let open_cell = kept_in(
            dir.path().join("cell"),
            (1 << 30, Some(crate::host_cell::named(1))),
            &bucket,
            idle(),
        )
        .await
        .unwrap();
        let data = noise(3, 4 * BLOCK);
        let extent = write(&open_cell, 1, &data).await;
        open_cell.commit(None).await.unwrap();
        open_cell.wait_uploaded().await.unwrap();

        // Make the bucket refuse every request with 403 (`Refusal::Denied`, non-retryable) so
        // `shred_tenant` fails immediately with `destroyed` empty.
        server.answering(Some(403));

        let err = crate::bucket::shred_tenant(1).await.unwrap_err();
        assert!(
            matches!(err, crate::bucket::ShredError::Failed(_))
                && err.to_string().contains("tenant 1"),
            "unexpected error: {err}"
        );

        // Restore bucket: the open cell was NOT halted and the KMS cache was NOT poisoned.
        server.answering(None);
        assert_eq!(crate::bucket::kms().unwrap().refuses(open_cell.cell()), None);
        let mut buf = vec![0u8; data.len()];
        open_cell
            .read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut buf[..])])
            .await
            .unwrap();
        assert_eq!(buf, data);
        open_cell.close().await;
    }

    /// board 11667: when a per-bucket KMS revokes a tenant on a subset of buckets (`!all_buckets`),
    /// `on_kms_revoked_on` must evict cached `HostRing` DEKs and `fetch::Memory` plaintext tails
    /// and unpacked parts for the revoked tenant's cells in the affected bucket(s) while leaving
    /// unaffected buckets' rings and cached plaintext intact.
    #[tokio::test(flavor = "multi_thread")]
    async fn per_bucket_kms_revocation_evicts_rings_and_kept_plaintext_for_affected_bucket_only() {
        use bucket_cell::Bucket as _;
        use bucket_cell::bucket::PutMode;
        use bucket_cell::fetch::{Budget, Fetcher, Limits, Priority};
        use bucket_cell::keys::{Ring, Scope};
        use bucket_cell::layout::{self, CellId, Chain, Contents, KeyArg as _, Location, Segment};
        use bucket_cell::object_bucket::ObjectBucket;
        use bucket_cell::s3_server::S3Server;
        use extent_frame::Secret;
        use std::collections::BTreeSet;
        use storage_device::ExtentId;

        let host = crate::Host::new();
        let state = host.bucket_state();
        let server = S3Server::start(None);
        let bucket = Arc::new(ObjectBucket::new(server.objects(), "rev-11667"));
        let tenant = 7u64;
        let c1 = CellId { tenant, id: (1u64 << 48) | 1 };
        let c2 = CellId { tenant, id: (2u64 << 48) | 2 };
        let r1 = Ring::writing_under(c1, Secret::random());
        let r2 = Ring::writing_under(c2, Secret::random());
        let _h1 = state.memory.keys().hold(r1.clone());
        let _h2 = state.memory.keys().hold(r2.clone());

        // Write and fetch one encrypted segment for each cell through `state.memory` (with a part
        // cache attached so `fetch::Memory` caches both `tails` and `recent` unpacked parts).
        let cache_dir = tempfile::tempdir().unwrap();
        let cache_path = cache_dir.path().to_path_buf();
        let cache = Arc::new(
            local_cell::upload::blocking(move || {
                bucket_cell::cache::Cache::open(&cache_path, 16 << 20)
            })
            .unwrap(),
        );
        let fetcher = Fetcher::new(
            bucket.clone(),
            Budget::new(Limits::UNBOUNDED),
            state.memory.clone(),
            Some(cache),
        );
        for (cell, ring) in [(c1, &r1), (c2, &r2)] {
            let ext = ExtentId::new(cell.id, 1);
            let payload = vec![0x5a; 4096];
            let contents =
                Contents { extents: vec![(ext, 0, payload.len() as u64)], ..Contents::default() };
            let footer = Chain::new(cell, 1, 0, 0).close(contents);
            let encoded =
                Segment { data: payload.clone(), footer }.encode(ring.encrypting().as_key());
            let key = layout::segment(cell, 1, 1);
            bucket.put(&key, encoded.into(), PutMode::Create).await.unwrap();
            let loc = Location { key, offset: 0, len: payload.len() as u64 };
            let read =
                fetcher.read(ext, &loc, 0, payload.len() as u64, Priority::Demand).await.unwrap();
            assert_eq!(read, payload);
        }
        assert!(state.memory.tails_bytes() > 0 && state.memory.recent_bytes() > 0);

        // Revoke tenant 7 ONLY on bucket 1 (`!all_buckets`).
        super::on_kms_revoked_on(&host, tenant, &BTreeSet::from([1]), "revoked in bucket 1");

        assert!(r1.is_revoked(), "bucket 1 ring must be revoked in place");
        assert!(
            !state.memory.keys().ring().cells().contains(&c1.id),
            "bucket 1 cell DEK must be dropped from HostRing union"
        );
        assert_eq!(
            state.memory.forget_kept(Scope::Cell(c1)),
            0,
            "bucket 1 cell tails and unpacked plaintext parts must already be evicted"
        );

        assert!(!r2.is_revoked(), "unaffected bucket 2 ring must remain valid");
        assert!(
            state.memory.keys().ring().cells().contains(&c2.id),
            "unaffected bucket 2 cell DEK must stay in HostRing union"
        );
        assert!(
            state.memory.forget_kept(Scope::Cell(c2)) > 0,
            "unaffected bucket 2 cached tails/parts must remain until explicitly forgotten"
        );
    }
}
