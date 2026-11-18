// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Extent stores in the host's directories, as both of cladefs's host stacks keep them: the images
//! [`Storage`](crate::Storage) mounts, and the stores [`OpenCell`](crate::OpenCell) serves a
//! guest. Either makes, opens, forks and deletes its stores here, so that both do it alike. Neither
//! collects them: what their segments hold that no extent is in leaves with the segments, once
//! acknowledged in the bucket, whose collector takes it back (TASK-165.8).
//!
//! Every store keeps its cell in a bucket, which its description names (TASK-165.6): one made on
//! a host of cladefs's system, this host's (`CLADEFSD_BUCKET`); one made elsewhere, as an image
//! of the cladefs CLI's or a test's is, a file bucket of its own, beside its directory, which its
//! forks share ([`create`]). Opening a store opens its bucket ([`Opened`]), the one this process
//! has of that URL ([`bucket`]). A store made by older code, of an older format, is refused
//! (`local_cell::Format`), opened or deleted alike: nothing reads or converts it.
//!
//! A store's directory holds its cell alone. Forks, versions and deletions are the bucket's
//! (TASK-165.7): a fork is a commit of its parent's naming the child, made once its segment is
//! acknowledged, which gives the child its record; the child's directory is made only then, a
//! store of its own taking the child's state from the bucket ([`fork`]).

use crate::error::{Errno, Error};
use crate::faults::failpoint;
use anyhow::Context as _;
use bucket_cell::cell;
use bucket_cell::known::Learned;
use bucket_cell::layout::{CellId, Child};
use bucket_cell::object_bucket::ObjectBucket;
use local_cell::upload::{self, Failure, Uploader};
use local_cell::{Description, HostDir, LocalCell, Options, Recovering};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The size of the blocks stores' extents are read and written in. Their capacities are multiples
/// of it.
pub(crate) const BLOCK_SIZE: u32 = 4096;

/// The default tenant (TASK-364), of the stores made for no tenant of the host's: the cladefs
/// CLI's images, an ungranted caller's stores, public bases, tests'. Its stores with writers alone
/// fork into other tenants' cells ([`may_fork_into`], TASK-395.9).
pub(crate) const DEFAULT_TENANT: u64 = bucket_cell::layout::DEFAULT_TENANT;

/// cladefs: whether a fork of `parent` by `uploader`, its uploader, may make a new cell of `tenant`'s
/// (TASK-395.9): `EACCES` if `parent` has a writer and `cell` is another tenant's than its own,
/// unless its own is the default tenant's ([`DEFAULT_TENANT`]), as the daemon's own stores and
/// public bases are. Inheritance across tenants is the default tenant's and shared versions' alone: a
/// fork of a version is checked at the read of the head naming the version its child's making makes instead
/// (`lifecycle::finish_point_fork`, TASK-395.7).
pub(crate) fn may_fork_into(
    parent: CellId,
    uploader: &Uploader<HostDir>,
    tenant: u64,
) -> Result<(), Error> {
    let own = parent.tenant;
    if uploader.is_at_version() || own == DEFAULT_TENANT || own == tenant {
        return Ok(());
    }
    let refused = format!(
        "forking cell {parent} to a cell of tenant {tenant}: a cell with a writer forks to \
         another tenant's cell only from the default tenant's"
    );
    Err(Error::new(anyhow::Error::new(Errno::EACCES).context(refused)))
}

/// The cell of a new cell of `tenant`'s, of `bucket`: the cell's ID and its cell's one no
/// other cell of the bucket has, nor will (`bucket_cell::objectid`), so that stores made apart,
/// and the forks of one, mint extent IDs of their own (backlog doc-4), which a host's cache of
/// extents keys them by (TASK-364). A cell's ID is its cell's: no caller names a new one.
pub(crate) fn new_cell(bucket: &ObjectBucket, tenant: u64) -> Result<CellId, Failure> {
    let id = upload::block_on(bucket.new_objectid())?;
    Ok(CellId { tenant, id })
}

/// A new cell of `tenant`'s, of the bucket at `url`, as [`new_cell`] gives it: for a cell whose
/// object is made before its store ([`CellOf::Named`]), as the FUSE mount's are. It blocks.
#[cfg(target_os = "linux")]
pub(crate) fn new_cell_at(url: &str, tenant: u64) -> Result<CellId, Failure> {
    new_cell(&*bucket(url)?, tenant)
}

/// A cell of a new cell of the default tenant's, of a random ID no allocator gave: for tests
/// that write a cell's objects themselves, as no store does, or name a fork's child
/// (`OpenCell::fork_to`).
#[cfg(test)]
pub(crate) fn new_alone() -> CellId {
    named(DEFAULT_TENANT)
}

/// A new cell of `tenant`'s, of a random ID no allocator gave.
#[cfg(test)]
pub(crate) fn named(tenant: u64) -> CellId {
    CellId { tenant, id: rand::random::<u64>().max(1) }
}

/// A child of a fork of `parent`, of a random ID no allocator gave, for tests that name it
/// (`OpenCell::fork_to`): of a new cell of its tenant's. Random, not next to its parent's: an
/// allocator gives the IDs of its block in order, the next to another store.
#[cfg(test)]
pub(crate) fn fork_child(parent: CellId) -> CellId {
    named(parent.tenant)
}

/// The cell a store is made of (TASK-364).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellOf {
    /// A new cell of the tenant's, its ID the store's bucket's to give ([`new_cell`]).
    New(u64),
    /// A cell the store's bucket has (`ENOENT` otherwise), which the store takes as it opens, or
    /// reads, if a version.
    Taken(CellId),
    /// The cell of an ID given, made unless the bucket has it: a new one, its ID the bucket's
    /// already ([`new_cell_at`]), as the FUSE mount's are, or one a test chose, no allocator's, for
    /// tests that write extents of a cell they name, or take it again.
    Named(CellId),
}

/// What the name of the directory beside a store's that is its own file bucket ends with, where no
/// host's bucket is ([`create`]): beside it, not in it, as its forks keep their cells there,
/// which outlive it.
const OWN_BUCKET: &str = ".bucket";

/// The fencing token a copy of a store never placed takes its cell with where no placement names
/// one, as for the cladefs CLI's images ([`token`]): the first placement's.
const FIRST_PLACEMENT: u64 = 1;

/// The buckets this process keeps stores' cells in, by their URLs: one each, which every store
/// naming it shares, so that a memory bucket (`memory:///NAME`) is one for the process, as a
/// bucket elsewhere is one for every process.
static BUCKETS: Mutex<BTreeMap<String, Arc<ObjectBucket>>> = Mutex::new(BTreeMap::new());

/// The bucket at `url` ([`ObjectBucket::from_url`]): this process's, made if it has none yet.
pub(crate) fn bucket(url: &str) -> anyhow::Result<Arc<ObjectBucket>> {
    let mut buckets = BUCKETS.lock().unwrap();
    if let Some(bucket) = buckets.get(url) {
        return Ok(bucket.clone());
    }
    let bucket = Arc::new(ObjectBucket::from_url(url).map_err(anyhow::Error::msg)?);
    buckets.insert(url.to_owned(), bucket.clone());
    Ok(bucket)
}

/// cladefs: what the decoded states of the cells this process's buckets keep, or its stores hold
/// as the maps of the extents they share, hold: each once, however many stores share it
/// (`bucket_cell::recovery::Decoded::held`, TASK-453).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn decoded_held() -> bucket_cell::recovery::Held {
    let buckets = BUCKETS.lock().unwrap();
    (buckets.values())
        .filter_map(|bucket| bucket_cell::Bucket::decoded(&**bucket))
        .map(bucket_cell::recovery::Decoded::held)
        .fold(bucket_cell::recovery::Held::default(), std::ops::Add::add)
}

/// Makes `bucket` this process's of `url`, before any store naming it opens, as the tests do the
/// buckets they put themselves in front of; unless it is already, the same.
#[cfg(test)]
pub(crate) fn use_bucket(url: &str, bucket: Arc<ObjectBucket>) {
    let mut buckets = BUCKETS.lock().unwrap();
    let had = buckets.entry(url.to_owned()).or_insert_with(|| bucket.clone());
    assert!(Arc::ptr_eq(had, &bucket), "this process has another bucket at {url}");
}

/// cladefs: the URL of this host's bucket, if it is a host of cladefs's system: what a test made
/// it ([`use_host_bucket`]), or `CLADEFSD_BUCKET`.
pub(crate) fn host_bucket_url() -> Option<String> {
    #[cfg(test)]
    if let Some(url) = HOST_BUCKET.lock().unwrap().clone() {
        return Some(url);
    }
    std::env::var("CLADEFSD_BUCKET").ok().filter(|url| !url.is_empty())
}

/// cladefs: the host's bucket's URL as a test made it ([`use_host_bucket`]).
#[cfg(test)]
static HOST_BUCKET: Mutex<Option<String>> = Mutex::new(None);

/// cladefs: makes `url` this host's bucket's, before `CLADEFSD_BUCKET`: as a test whose threads
/// may read the environment already makes it, where setting the variable would race their reads
/// (`std::env::set_var`'s safety, TASK-444.16).
#[cfg(test)]
pub(crate) fn use_host_bucket(url: String) {
    *HOST_BUCKET.lock().unwrap() = Some(url);
}

/// The URL of the bucket a store made at `path`, a directory, keeps its cells in: this host's,
/// if it is a host of cladefs's system ([`host_bucket_url`], as [`crate::bucket::Host`] has
/// it); else a file bucket of the store's own ([`bucket_named`]).
pub(crate) fn bucket_for(path: &Path) -> anyhow::Result<String> {
    bucket_named(host_bucket_url(), path)
}

/// The URL of the bucket a store made at `path`, a directory, keeps its cell in, on the host
/// whose bucket's URL is `host`, if it is one: that; or a file bucket of the store's own, beside
/// its directory ([`OWN_BUCKET`]), which its forks share, and which outlives it.
fn bucket_named(host: Option<String>, path: &Path) -> anyhow::Result<String> {
    if let Some(url) = host {
        return Ok(url);
    }
    let path = path.canonicalize()?;
    let name = path.file_name().ok_or_else(|| anyhow::anyhow!("{} has no name", path.display()))?;
    let own = path.with_file_name(format!("{}{OWN_BUCKET}", name.to_string_lossy()));
    let url = url::Url::from_directory_path(&own)
        .map_err(|()| anyhow::anyhow!("{} names no file bucket", own.display()))?;
    Ok(url.to_string())
}

/// Whether `store` keeps its cell in a file bucket no host's collector collects: one made beside
/// a store's directory, where no host's bucket was ([`bucket_named`]), its own or that of the
/// store it was forked from, which its forks share.
pub(crate) fn keeps_own_bucket(store: &LocalCell<HostDir>) -> bool {
    let url = store.bucket().unwrap_or_default();
    url.starts_with("file:") && url.trim_end_matches('/').ends_with(OWN_BUCKET)
}

/// The fencing token copies of stores never placed take their cells with where this process
/// opens them as no host's daemon does, as [`crate::Storage`] does: this host's placement's, if it
/// is a host of cladefs's system (`CLADEFSD_TOKEN`, [`crate::bucket::host_token`]); else the
/// first placement's. So the daemon and the cladefs CLI on one host take each other's cells.
pub(crate) fn token() -> Result<u64, Failure> {
    match std::env::var_os("CLADEFSD_TOKEN").filter(|token| !token.is_empty()) {
        Some(_) => crate::bucket::host_token(),
        None => Ok(FIRST_PLACEMENT),
    }
}

/// How a store this process opens where no host's daemon serves it keeps its cell in its bucket:
/// its uploader's options, as a copy never placed takes its cell's head here ([`token`]).
pub(crate) fn keeping() -> Result<upload::Options, Failure> {
    Ok(upload::Options { token: token()?, ..Default::default() })
}

/// A store opened ([`open`]), with the bucket its description names, which its cell is kept in,
/// and its uploader, which took the cell's head, or holds its version (TASK-165.9).
pub(crate) struct Opened {
    pub store: Arc<LocalCell<HostDir>>,
    pub bucket: Arc<ObjectBucket>,
    pub uploader: Uploader<HostDir>,
}

impl Opened {
    /// The store, whose maker made it whole (TASK-196): opening it opens it from now on.
    pub fn made(&self) -> anyhow::Result<()> {
        self.store.made()
    }
}

/// The bucket the store at `path` names, if its description is there: as deleting it, or what of
/// it a deletion that stopped left, goes by. A store of another format is refused
/// (`local_cell::Format`).
pub(crate) fn bucket_at(path: &Path) -> anyhow::Result<Option<Arc<ObjectBucket>>> {
    match local_cell::host_bucket(path) {
        Ok(Some(url)) => Ok(Some(bucket(&url)?)),
        Err(err) if err.is::<local_cell::Format>() => Err(err),
        Ok(None) | Err(_) => Ok(None),
    }
}

/// The options stores are opened with: their extents hold at most what the guest's device says
/// one does (doc-4's `max_extent`).
pub(crate) fn options() -> Options {
    Options { max_extent: crate::OpenCell::MAX_EXTENT, ..Options::default() }
}

/// Makes a new store at `path`, which must not exist, holding at most `capacity` bytes, of the
/// cell `of` says, kept in this host's bucket, or a file bucket of its own ([`bucket_for`]), its
/// cell's, of `kind` (doc-20), and opens it, its uploader as `keeping` says ([`open`]): whole, as
/// made. Its directory entry is as durable as
/// it is. A store that is not made whole is removed; what is at `path` already is left as it is.
pub(crate) fn create(
    path: &Path,
    capacity: u64,
    of: CellOf,
    kind: cell::Kind,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    let opened = create_unmade(path, capacity, of, kind, None, keeping)?;
    opened.made()?;
    Ok(opened)
}

/// Makes a new store at `path` as [`create`] does, kept in the bucket at `bucket`, if given, or
/// else at [`bucket_for`]'s; unmade until its maker says it is whole ([`Opened::made`]), as an
/// import does once filled (TASK-196): opened before, it refuses (`local_cell::Unmade`), and
/// [`unmade`] says it is no store.
pub(crate) fn create_unmade(
    path: &Path,
    capacity: u64,
    of: CellOf,
    kind: cell::Kind,
    bucket: Option<&str>,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    std::fs::create_dir(path)
        .map_err(|err| Failure::local(err, format!("creating {}", path.display())))?;
    let made = || -> Result<Opened, Failure> {
        let url = bucket.map_or_else(|| bucket_for(path), |bucket| Ok(bucket.to_owned()))?;
        let bucket = self::bucket(&url)?;
        // A new cell made here, a root, its KEK from the host's pool, before its store first opens
        // (TASK-515): a tenant the KMS refuses gets nothing made (TASK-411.28); its first taking
        // gives it its DEK and kind.
        let made = |cell: CellId| -> Result<(), Failure> {
            let kms = crate::bucket::kms()?;
            upload::block_on(bucket_cell::keys::make(&*bucket, &kms, cell, rand::random()))??;
            Ok(())
        };
        // Its head, read to see the bucket has it, is what the store's opening, next, would read
        // first: learned for it, a read less (`bucket_cell::known`, TASK-449).
        let taken = |cell: CellId| -> Result<bool, Failure> {
            let read = upload::block_on(bucket_cell::head::read(&*bucket, cell))?;
            let found = read.head(cell)?.is_some();
            if let (Some(known), Some(head)) = (bucket_cell::Bucket::known(&*bucket), read.object())
            {
                known.learn(cell, Learned { head });
            }
            Ok(found)
        };
        let cell = match of {
            CellOf::New(tenant) => {
                let cell = new_cell(&bucket, tenant)?;
                made(cell)?;
                cell
            }
            CellOf::Named(cell) => {
                if !taken(cell)? {
                    made(cell)?;
                }
                cell
            }
            CellOf::Taken(cell) => match taken(cell)? {
                true => cell,
                false => return Err(bucket_cell::Error::NoCell.into()),
            },
        };
        let description = Description { block_size: BLOCK_SIZE, capacity, cell };
        local_cell::create_host(path, description, &url, kind)?;
        let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
        let synced = std::fs::File::open(parent.unwrap_or(Path::new(".")))
            .and_then(|parent| parent.sync_all());
        synced.map_err(|err| Failure::local(err, "syncing a store's parent"))?;
        let recovering = LocalCell::recover_unmade(host_dir(path)?)?;
        opened(recovering, keeping)
    };
    // Leave nothing half made behind.
    made().inspect_err(|_| discard(path))
}

/// Opens the store at `path` as of its last commit, with the bucket it names: its uploader, as
/// `keeping` says, takes its cell's head, with the token its placement names or `keeping`'s,
/// and recovers it, the store's write-ahead log put after its end (`local_cell::upload::open`,
/// TASK-165.9). Fails as that does: a log another writer's commits moved past (`ESTALE`), a cell
/// deleted (`ENOENT`) or a later placement's (`ESTALE`); and a store never made whole (`Unmade`).
pub(crate) fn open(path: &Path, keeping: upload::Options) -> Result<Opened, Failure> {
    let recovering = LocalCell::recover(host_dir(path)?)?;
    opened(recovering, keeping)
}

/// The directory of the store at `path`.
fn host_dir(path: &Path) -> Result<HostDir, Failure> {
    HostDir::new(path).map_err(|err| Failure::local(err, format!("{}", path.display())))
}

/// Opens the store at `path` as [`open`] does, its write-ahead log dropped if another writer's
/// commits moved its cell past it (`bucket_cell::Error::Diverged`): what `cladefsctl take` does
/// of a store that refuses to open so, the commits the log held, never acknowledged, lost (doc-14,
/// Opening a store; doc-7, Recovery on the same host).
pub(crate) fn take(path: &Path, keeping: upload::Options) -> Result<Opened, Failure> {
    match open(path, keeping.clone()) {
        Err(Failure::Bucket(bucket_cell::Error::Diverged)) => {
            let mut recovering = LocalCell::recover(host_dir(path)?)?;
            log::warn!(
                "cladefs: dropping {}'s write-ahead log: another writer moved cell {} past it",
                path.display(),
                recovering.cell()
            );
            recovering.drop_wal()?;
            opened(recovering, keeping)
        }
        opened => opened,
    }
}

/// The store `recovering` recovered, opened with its uploader as `keeping` says.
fn opened(recovering: Recovering<HostDir>, keeping: upload::Options) -> Result<Opened, Failure> {
    let url = recovering.bucket().map(str::to_owned);
    let path = || recovering_path(&recovering);
    let url = url.ok_or_else(|| Failure::msg(format!("{} names no bucket", path())))?;
    let bucket = bucket(&url).with_context(|| format!("opening the bucket {url}"))?;
    let runtime = crate::bucket::runtime()?;
    let holding = (rand::random(), keeping);
    let (store, uploader) = upload::open(recovering, options(), bucket.clone(), holding, &runtime)?;
    Ok(Opened { store, bucket, uploader })
}

/// The path of the directory `recovering` is of, for what fails of it.
fn recovering_path(recovering: &Recovering<HostDir>) -> String {
    format!("cell {}'s store", recovering.cell())
}

/// Whether `path` is a directory a store's making, or a fork's, left unmade, its host crashing:
/// no store, as its description, made last, is not there, or it was never made whole
/// (`local_cell::unmade_host`, TASK-196); deleting it finishes it ([`delete`]).
pub(crate) fn unmade(path: &Path) -> bool {
    local_cell::unmade_host(path)
}

/// Removes what a making that failed left at `path`, if anything, as a store's directory goes
/// (`local_cell::remove_store_dir`): logged if it can't, as the failure the caller returns is
/// the making's.
pub(crate) fn discard(path: &Path) {
    if path.exists()
        && let Err(err) = local_cell::remove_store_dir(path)
    {
        log::warn!("cladefs: removing {}, which a making left: {err:#}", path.display());
    }
}

/// Forks `store`, a store at a version (`uploader.is_at_version`), to `child`, a new cell holding
/// what the version holds, whose store is a new directory at `path`, which [`open`] opens:
/// `EEXIST` if anything is at `path`. The fork is the child's record alone (doc-7, Forks;
/// `Uploader::forked`), inheriting the version's extents rather than copies, which fails with
/// `ENOENT` once the version is dropped, and with `EACCES` if the child is of another tenant's
/// cell than the version, which isn't shared (TASK-395.7). Only then is the child's store made,
/// at `path`, holding nothing of its own: opening it recovers the child's state from the bucket,
/// as any store's opening does (TASK-165.9). `prepare` writes what the caller keeps beside the
/// store into its directory first, before its description, which makes it a store. A fork to
/// another tenant's cell fails with `EACCES` before anything is made, unless the store is the
/// default tenant's ([`may_fork_into`], TASK-395.9).
///
/// The caller names the child: a fork to a child whose ID another cell has in the bucket fails
/// with `EEXIST`, making nothing (TASK-79.82). A fork that fails once its record was made is
/// deleted in the bucket as the fork's alone (`Uploader::unfork`); its host crashing, it stands,
/// or never was, as the bucket has it.
///
/// cladefs: a fork of an encrypted store into another cell, as every fork here is, gives its child
/// the keys it inherits, rewrapped under its cell's KEK, from the pool, no KMS call made
/// (`Remote::inheriting`, `keys::give_inherited`, TASK-411.4): refused before anything is made,
/// `EACCES` if the child's tenant has no KEK in the KMS (TASK-411.28), or the host heard a tenant
/// whose keys it inherits, or the child's, revoked or destroyed.
pub(crate) fn fork(
    store: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> Result<(), Error> {
    may_fork_into(store.description().cell, uploader, child.cell.tenant)?;
    let inheriting = uploader.inheriting(child)?;
    std::fs::create_dir(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::AlreadyExists => {
            Error::new(anyhow::Error::new(err).context(Errno::EEXIST))
        }
        _ => Error::new(anyhow::Error::new(err).context(format!("creating {}", path.display()))),
    })?;
    let cell = child.cell;
    let failed = |err: Failure| match err {
        Failure::Bucket(bucket_cell::Error::IdTaken(_)) => taken(cell),
        Failure::VersionDropped => anyhow::Error::new(Errno::ENOENT)
            .context(format!("forking to cell {cell}: the version was deleted")),
        // cladefs: another tenant's version, not shared (TASK-395.7).
        Failure::Bucket(refused @ bucket_cell::Error::NotShared(_)) => {
            anyhow::Error::new(Errno::EACCES).context(format!("forking to cell {cell}: {refused}"))
        }
        err => anyhow::anyhow!("forking to cell {cell}: {err}"),
    };
    let made = uploader.forked(child, inheriting).map_err(failed).and_then(|()| {
        failpoint!("fork::recorded");
        make_child(store, path, cell, prepare)
            .with_context(|| format!("forking to cell {cell}: making its store"))?;
        failpoint!("fork::made");
        Ok(())
    });
    let Err(err) = made else { return Ok(()) };
    // Leave nothing half made behind: no store, and no fork whose store went.
    discard(path);
    if let Err(why) = uploader.unfork(child) {
        log::warn!("cladefs: leaving cell {cell}, whose fork failed: {why}");
    }
    Err(Error::new(err))
}

/// cladefs: the stores this process made versions of, by the version (cell and ID): what a store at
/// the version made here inherits of what cladefs keeps beside its store, what is attached to it
/// ([`cell_dir::inherit_attached`]), as a fork of the store does. In memory alone: a restore on
/// another host, or after the process went, inherits none.
static VERSIONS_MADE: Mutex<BTreeMap<(CellId, u64), PathBuf>> = Mutex::new(BTreeMap::new());

/// cladefs: the store at `path`, of `cell`, made the version `version` here.
pub(crate) fn made_version(cell: CellId, version: u64, path: &Path) {
    VERSIONS_MADE.lock().unwrap().insert((cell, version), path.to_owned());
}

/// cladefs: where the store this process made the version `version` of `cell` of is, if it did.
#[cfg(target_os = "linux")]
pub(crate) fn version_made_of(cell: CellId, version: u64) -> Option<PathBuf> {
    VERSIONS_MADE.lock().unwrap().get(&(cell, version)).cloned()
}

/// cladefs: where a store at the version `version` of `cell` is kept for forks of it: a directory of
/// its own, in the process's temporary directory, gone with the process, as such a store holds
/// nothing of its own.
pub(crate) fn version_dir(cell: CellId, version: u64) -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| {
        std::env::temp_dir().join(format!("cladefs-versions-{}", std::process::id()))
    });
    root.join(format!("{:x}.{:x}.{:x}", cell.tenant, cell.id, version))
}

/// Forks `store`, its cell's version `version` made and uploaded, to `child` as [`fork`] does:
/// from a store at the version ([`version_dir`]), made, then deleted, and the version dropped, the
/// child keeping what it inherits by its reference. So a store with a writer forks: forks are of
/// versions alone (TASK-508.3).
pub(crate) fn fork_through(
    store: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    version: u64,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> Result<(), Error> {
    let at = version_dir(store.description().cell, version);
    let (kind, url) = (store.kind().ok_or(Errno::EINVAL)?, store.bucket().ok_or(Errno::EINVAL)?);
    let made =
        at.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::create_dir(&at));
    made.map_err(|err| {
        Error::new(anyhow::Error::new(err).context(format!("making {}", at.display())))
    })?;
    let forked = local_cell::create_host_at_version(&at, store.description(), url, kind, version)
        .and_then(|()| local_cell::made_host(&at))
        .map_err(Error::new)
        .and_then(|()| open(&at, uploader.options().clone()).map_err(Error::from))
        .and_then(|at| fork(&at.store, &at.uploader, path, child, prepare));
    discard(&at);
    let dropped = uploader.drop_versions(&std::collections::BTreeSet::from([version]));
    forked?;
    Ok(dropped?)
}

/// The error of a fork to `cell`, whose ID another cell has in the bucket.
fn taken(cell: CellId) -> anyhow::Error {
    let refused = format!("forking to cell {cell}: another cell has its ID");
    anyhow::Error::new(Errno::EEXIST).context(refused)
}

/// Makes the store of `cell`, a fork of `parent`'s cell the bucket has the record of, in the
/// directory at `path`, made empty: `prepare` writes into it first; then the store's directory, of
/// the parent's capacity, naming the parent's bucket, of the parent's kind, its own cell's, whole
/// (TASK-364, doc-20). It holds nothing of its own: opening it recovers the child from the
/// bucket, as any store's opening does (TASK-165.9), and holds its cell, making it.
fn make_child(
    parent: &LocalCell<HostDir>,
    path: &Path,
    cell: CellId,
    prepare: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    prepare(path)?;
    let capacity = parent.description().capacity;
    let named = || anyhow::anyhow!("{} names no bucket", parent.dir().path().display());
    let url = parent.bucket().map(str::to_owned).ok_or_else(named)?;
    let kind = parent
        .kind()
        .ok_or_else(|| anyhow::anyhow!("{} names no cell's kind", parent.dir().path().display()))?;
    let description = Description { block_size: BLOCK_SIZE, capacity, cell };
    local_cell::create_host(path, description, &url, kind)?;
    local_cell::made_host(path)?;
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    std::fs::File::open(parent.unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}

/// Deletes the store at `path`, which nothing has open: with `bucket`, its cell there first, its
/// forks' cells living on, as the placement its copy was placed at, or, never placed, as the one
/// `token` gives; then its directory. A deletion that stopped partway is finished by deleting
/// again (TASK-79.71), and a directory a store's or a fork's making left unmade ([`unmade`]) goes.
///
/// Fails with `ENOENT` if nothing is at `path`, and with `ESTALE` if a later placement of the
/// store holds its cell in `bucket`.
pub(crate) fn delete(
    path: &Path,
    bucket: Option<&ObjectBucket>,
    token: impl Fn() -> Result<u64, Failure>,
) -> Result<(), Error> {
    // As this copy's placement, or this host's, deletes it: not from under a later one (`ESTALE`).
    // A store at a version deletes nothing in the bucket: the version is its cell's.
    delete_with(path, |dir, cell| match bucket {
        Some(_) if local_cell::host_version(dir)?.is_some() => Ok(()),
        Some(bucket) => {
            let placed = local_cell::host_token(dir)?;
            let token = placed.map_or_else(&token, Ok)?;
            local_cell::upload::delete_cell(bucket, cell, token)
        }
        None => Ok(()),
    })
}

/// Deletes the store at `path` as [`delete`] does, `elsewhere` deleting its cell where else it
/// is kept, given the store's directory and the cell: once the store is found open nowhere here,
/// before it goes here (`local_cell::delete_host`), so that a deletion that stopped partway, the
/// host crashing or `elsewhere` failing, leaves the store still, which deleting again finishes,
/// never a cell kept elsewhere alone. What a store's making, or a fork's, left unmade, which is
/// no store, goes, `elsewhere` called for nothing.
pub(crate) fn delete_with(
    path: &Path,
    elsewhere: impl Fn(&Path, CellId) -> Result<(), Failure>,
) -> Result<(), Error> {
    if !path.exists() {
        return Err(Errno::ENOENT.into());
    }
    let deleting = |cell| -> anyhow::Result<()> {
        failpoint!("delete::elsewhere");
        Ok(elsewhere(path, cell)?)
    };
    local_cell::delete_host(path, deleting).map_err(|err| {
        // Its errno, if deleting the cell elsewhere failed with one: `ESTALE` for a cell a
        // later placement holds.
        let failure = err.chain().find_map(|cause| cause.downcast_ref::<Failure>());
        match failure.and_then(crate::bucket::errno) {
            Some(errno) => Error::new(err.context(errno)),
            None => Error::new(err),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// A new cell of a new cell of the default tenant's.
    fn new() -> CellOf {
        CellOf::New(DEFAULT_TENANT)
    }

    /// TASK-196: a store its maker never made whole is no store: it refuses to open, and
    /// [`unmade`] says so, which deleting it, or making it anew, goes by; once made, it opens.
    #[test]
    fn stores_unmade_open_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let made = create_unmade(&path, MIB, new(), cell::Kind::Fxfs, None, keeping().unwrap());
        let cell = made.unwrap().store.description().cell;
        assert!(unmade(&path));
        let refused = open(&path, keeping().unwrap()).err().unwrap();
        let Failure::Local(refused) = refused else { panic!("{refused}") };
        assert_eq!(refused.downcast_ref::<local_cell::Unmade>(), Some(&local_cell::Unmade));
        local_cell::made_host(&path).unwrap();
        assert!(!unmade(&path));
        assert_eq!(
            open(&path, keeping().unwrap()).map(|opened| opened.store.description().cell).ok(),
            Some(cell)
        );
    }

    /// TASK-364: a new store's cell's ID is its bucket's to give, one of a block this process
    /// took there, the next store's the next of it; a store of a cell the bucket has not is
    /// refused (`NoCell`), and leaves nothing.
    #[test]
    fn new_cells_ids_are_their_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (dir.path().join("one"), dir.path().join("two"));
        let url = format!("memory:///new-cells-{}", std::process::id());
        let made = |path: &Path, of| {
            let made =
                create_unmade(path, MIB, of, cell::Kind::Fxfs, Some(&url), keeping().unwrap());
            made.unwrap().store.description().cell
        };
        let first = made(&one, new());
        let second = made(&two, new());
        assert_eq!(second.id, first.id + 1);
        assert_eq!((first.id, second.id), (first.id, second.id));
        let block = first.id / bucket_cell::objectid::ID_BLOCK;
        let bucket = bucket(&url).unwrap();
        let taken = bucket_cell::layout::objectid_block(block);
        assert!(upload::block_on(bucket_cell::bucket::get(&*bucket, &taken)).unwrap().is_some());

        let absent = CellId { tenant: DEFAULT_TENANT, id: second.id + 1 };
        let path = dir.path().join("absent");
        let refused = create_unmade(
            &path,
            MIB,
            CellOf::Taken(absent),
            cell::Kind::Fxfs,
            Some(&url),
            keeping().unwrap(),
        );
        let Err(Failure::Bucket(refused)) = refused.map(drop) else { panic!("made") };
        assert_eq!(refused, bucket_cell::Error::NoCell);
        assert!(!path.exists(), "a store of no cell was left");
    }

    /// TASK-165.6: a store made on a host of cladefs's system names the host's bucket; one made
    /// elsewhere, a file bucket of its own, beside its directory, where its cell is kept: its
    /// forks' too, which name it. Beside it, not in it, so that it outlives the store, its forks'
    /// cells living on (TASK-165.7).
    #[test]
    fn stores_name_their_hosts_bucket_or_one_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        std::fs::create_dir(&path).unwrap();
        let host = Some("gs://bucket/prefix".to_owned());
        assert_eq!(bucket_named(host, &path).unwrap(), "gs://bucket/prefix");
        let own = dir.path().canonicalize().unwrap().join("store.bucket");
        let url = format!("file://{}/", own.display());
        assert_eq!(bucket_named(None, &path).unwrap(), url);

        // Where no host's bucket is, as in this test's process.
        let (made, fork) = (dir.path().join("made"), dir.path().join("fork"));
        let Opened { store, bucket, uploader } =
            create(&made, MIB, new(), cell::Kind::Fxfs, keeping().unwrap()).unwrap();
        let forked = new_cell(&bucket, store.description().cell.tenant).unwrap();
        let child = Child { cell: forked, owner: 1 };
        let version = bucket_cell::layout::NewVersion { id: 1, keep_last: None };
        uploader.wait_uploaded(store.commit_version(version, false).unwrap()).unwrap();
        fork_through(&store, &uploader, version.id, &fork, child, |_| Ok(())).unwrap();
        let own = dir.path().canonicalize().unwrap().join("made.bucket");
        let url = format!("file://{}/", own.display());
        for named in [&made, &fork] {
            assert_eq!(local_cell::host_bucket(named).unwrap(), Some(url.clone()));
        }
        assert!(keeps_own_bucket(&store), "a store's own bucket is a host's");
        // The bucket is this process's one of its URL, whose objects are in its directory.
        assert!(Arc::ptr_eq(&bucket, &self::bucket(&url).unwrap()));
        // cladefs: under its cell's prefix (TASK-364).
        let cell = own.join(store.description().cell.prefix());
        assert!(cell.join("head").exists(), "{:?}", std::fs::read_dir(&cell).unwrap());
        // The store deleted, its fork's cell lives on in it.
        drop(uploader);
        drop(store);
        delete(&made, Some(&bucket), || Ok(1)).unwrap();
        assert!(!made.exists() && own.exists());
        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, forked));
        assert_eq!(record.unwrap().map(|record| record.deleted()), Some(false));
    }

    /// TASK-165.6: a store made by older code, of an older format, is refused by its format,
    /// opened or deleted: nothing reads it.
    #[test]
    fn older_stores_are_refused_by_their_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        drop(create(&path, MIB, new(), cell::Kind::Fxfs, keeping().unwrap()).unwrap());
        let description = path.join("store");
        let text = std::fs::read_to_string(&description).unwrap();
        let older: String = std::iter::once("cladefs extent store 5")
            .chain(text.lines().skip(1).filter(|line| !line.starts_with("bucket ")))
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(&description, older).unwrap();

        let format = local_cell::Format { found: "cladefs extent store 5".to_owned() };
        let refused = open(&path, keeping().unwrap()).err().unwrap();
        let Failure::Local(refused) = refused else { panic!("{refused}") };
        assert_eq!(refused.downcast_ref::<local_cell::Format>(), Some(&format), "{refused:#}");
        let refused = crate::Storage::open(&path).err().unwrap();
        assert_eq!(refused.errno(), Some(Errno::ENOTSUP), "{refused:#}");
        let refused = crate::Storage::delete(&path).err().unwrap();
        assert_eq!(refused.errno(), Some(Errno::ENOTSUP), "{refused:#}");
        assert!(path.join("store").exists());
    }

    /// A store's directory whose deletion stopped once its description was gone, or whose making
    /// stopped before it began, empty, is removed by deleting it again.
    #[test]
    fn deleting_an_emptied_store_finishes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fork");
        std::fs::create_dir(&path).unwrap();
        assert!(unmade(&path));
        delete(&path, None, token).unwrap();
        assert!(!path.exists());
    }

    /// TASK-79.20: a store whose description was never put in place, its host crashing as its
    /// making, or a fork's, wrote what goes before it, is no store, and is removed by deleting it,
    /// as cladefsd and cladefsctl delete it.
    #[test]
    fn deleting_a_store_never_described_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fork");
        std::fs::create_dir_all(path.join("segments")).unwrap();
        std::fs::write(path.join("ancestry"), "3 /elsewhere\n").unwrap();
        assert!(unmade(&path));
        delete(&path, None, token).unwrap();
        assert!(!path.exists());
    }
}
