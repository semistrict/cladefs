// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Cells in the host's directories, as both of cladefs's host stacks keep them: the images
//! [`Storage`](crate::Storage) mounts, and the cells [`OpenCell`](crate::OpenCell) serves a
//! guest. Either makes, opens, forks and deletes its cells here, so that both do it alike. Neither
//! collects them: what their segments hold that no extent is in leaves with the segments, once
//! acknowledged in the bucket, whose collector takes it back (TASK-165.8).
//!
//! Every cell directory keeps its cell in a bucket, which its description names (TASK-165.6): one
//! made on a host of cladefs's system, this host's (`CLADEFSD_BUCKET`); one made elsewhere, as an
//! image of the cladefs CLI's or a test's is, a file bucket of its own, beside its directory, which
//! its forks share ([`create`]). Opening a cell directory opens its bucket ([`Opened`]), the one
//! this process has of that URL ([`bucket`]). A cell directory made by older code, of an older
//! format, is refused (`local_cell::Format`), opened or deleted alike: nothing reads or converts
//! it.
//!
//! A cell directory holds its cell alone. Forks, versions and deletions are the bucket's
//! (TASK-165.7): a fork is a commit of its parent's naming the child, made once its segment is
//! acknowledged, which gives the child its record; the child's directory is made only then, a
//! cell directory of its own taking the child's state from the bucket ([`fork`]).

use crate::error::{Errno, Error};
use crate::faults::failpoint;
use anyhow::Context as _;
use bucket_cell::cache::Cache;
use bucket_cell::cell;
use bucket_cell::layout::{CellId, Child};
use bucket_cell::object_bucket::{ObjectBucket, RoutedBucket};
use local_cell::upload::{self, Failure, Uploader};
use local_cell::{Description, HostDir, LocalCell, Making, Options, Recovering};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The size of the blocks cells' extents are read and written in. Their capacities are multiples
/// of it.
pub const BLOCK_SIZE: u32 = 4096;

/// The default tenant (TASK-364), of the cells made for no tenant of the host's: the cladefs
/// CLI's images, an ungranted caller's cells, public bases, tests'. Its cells with writers alone
/// fork into other tenants' cells ([`may_fork_into`], TASK-395.9).
pub const DEFAULT_TENANT: u64 = bucket_cell::layout::DEFAULT_TENANT;

/// cladefs: whether a fork of `parent` by `uploader`, its uploader, may make a new cell of
/// `tenant`'s (TASK-395.9): `EACCES` if `parent` has a writer and `cell` is another tenant's than
/// its own, unless its own is the default tenant's ([`DEFAULT_TENANT`]), as the daemon's own cells
/// and public bases are. Inheritance across tenants is the default tenant's and shared versions'
/// alone: a fork of a version is checked at the read of the head naming the version its child's
/// making makes instead (`lifecycle::finish_point_fork`, TASK-395.7).
pub fn may_fork_into(
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
/// other cell of the bucket has, nor will (`bucket_cell::objectid`), so that cells made apart,
/// and the forks of one, mint extent IDs of their own (backlog doc-4), which a host's cache of
/// extents keys them by (TASK-364). A cell's ID is its cell's: no caller names a new one.
pub async fn new_cell(bucket: &RoutedBucket, tenant: u64) -> Result<CellId, Failure> {
    let id = bucket.new_objectid().await?;
    Ok(CellId { tenant, id })
}

/// A new cell of `tenant`'s, of the bucket at `url` on `host`, as [`new_cell`] gives it.
#[cfg(all(target_os = "linux", not(madsim)))]
pub async fn new_cell_at_on(host: &crate::Host, url: &str, tenant: u64) -> Result<CellId, Failure> {
    new_cell(&*bucket_on(host, url)?, tenant).await
}

/// A new cell of `tenant`'s, of the bucket at `url`, as [`new_cell`] gives it: for a cell whose
/// object is made before its cell ([`CellOf::Named`]), as the FUSE mount's are. It blocks.
#[cfg(all(target_os = "linux", not(madsim)))]
pub async fn new_cell_at(url: &str, tenant: u64) -> Result<CellId, Failure> {
    new_cell_at_on(crate::Host::process(), url, tenant).await
}

/// A cell of a new cell of the default tenant's, of a random ID no allocator gave: for tests
/// that write a cell's objects themselves, as no cell does, or name a fork's child
/// (`OpenCell::fork_to`).
#[cfg(any(test, feature = "testing"))]
pub fn new_alone() -> CellId {
    named(DEFAULT_TENANT)
}

/// A new cell of `tenant`'s, of a random ID no allocator gives (`bucket_cell::objectid::named`).
#[cfg(any(test, feature = "testing"))]
pub fn named(tenant: u64) -> CellId {
    CellId { tenant, id: bucket_cell::objectid::named() }
}

/// A child of a fork of `parent`, of a random ID no allocator gave, for tests that name it
/// (`OpenCell::fork_to`): of a new cell of its tenant's. Random, not next to its parent's: an
/// allocator gives the IDs of its block in order, the next to another cell.
#[cfg(any(test, feature = "testing"))]
pub fn fork_child(parent: CellId) -> CellId {
    named(parent.tenant)
}

/// The cell a new cell directory is made of (TASK-364).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellOf {
    /// A new cell of the tenant's, its ID its bucket's to give ([`new_cell`]).
    New(u64),
    /// A cell the bucket has (`ENOENT` otherwise), which the cell directory takes as it opens, or
    /// reads, if a version.
    Taken(CellId),
    /// The cell of an ID given, made unless the bucket has it: a new one, its ID the bucket's
    /// already ([`new_cell_at`]), as the FUSE mount's are, or one a test chose, no allocator's, for
    /// tests that write extents of a cell they name, or take it again.
    Named(CellId),
}

/// What the name of the directory beside a cell's that is its own file bucket ends with, where no
/// host's bucket is ([`create`]): beside it, not in it, as its forks keep their cells there,
/// which outlive it.
const OWN_BUCKET: &str = ".bucket";

/// The fencing token a copy of a cell never placed takes its cell with where no placement names
/// one, as for the cladefs CLI's images ([`token`]): the first placement's.
const FIRST_PLACEMENT: u64 = 1;

/// Explicit per-host bucket registry and versions-made map (board 10962), replacing `BUCKETS` and
/// `VERSIONS_MADE` `node_local!` statics.
pub struct State {
    /// The buckets this host keeps cells in, by their URLs: one each, which every cell naming it
    /// shares.
    buckets: Mutex<BTreeMap<String, Arc<RoutedBucket>>>,
    /// Shared filtered bucket-option snapshot reused by every bucket opened on this host
    /// (board 9684).
    registered: std::sync::OnceLock<Arc<bucket_cell::object_bucket::Registered>>,
    /// cladefs: the cells this host made versions of, by the version (cell and ID).
    versions_made: Mutex<BTreeMap<(CellId, u64), PathBuf>>,
}

impl State {
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(BTreeMap::new()),
            registered: std::sync::OnceLock::new(),
            versions_made: Mutex::new(BTreeMap::new()),
        }
    }

    fn registered(&self) -> Arc<bucket_cell::object_bucket::Registered> {
        self.registered
            .get_or_init(|| Arc::new(bucket_cell::object_bucket::Registered::from_env()))
            .clone()
    }
}

/// The bucket at `url` in `state`.
pub fn bucket_in(state: &State, url: &str) -> anyhow::Result<Arc<RoutedBucket>> {
    bucket_opened_in(state, url, |resolve, url| resolve.open_url(url))
}

/// The bucket at `url` on `host` ([`ObjectBucket::from_url`]): `host`'s, made if it has none yet.
pub fn bucket_on(host: &crate::Host, url: &str) -> anyhow::Result<Arc<RoutedBucket>> {
    bucket_in(host.host_cells(), url)
}

/// The bucket at `url` ([`ObjectBucket::from_url`]): this process's, made if it has none yet.
#[cfg(not(madsim))]
pub fn bucket(url: &str) -> anyhow::Result<Arc<RoutedBucket>> {
    bucket_on(crate::Host::process(), url)
}

/// cladefs: the bucket numbered `number` at `url` on `host`.
#[cfg(not(madsim))]
pub fn bucket_of_on(
    host: &crate::Host,
    url: &str,
    number: u16,
) -> anyhow::Result<Arc<RoutedBucket>> {
    bucket_opened_in(host.host_cells(), url, |resolve, url| resolve.open_url_numbered(url, number))
}

/// cladefs: the bucket numbered `number` at `url`, as [`bucket`] has it: opened with that bucket's
/// own options, its credentials (`CLADEFSD_BUCKET_<number>_*`,
/// [`bucket_cell::object_bucket::numbered_options`]), unless this process opened it already
/// (board 5550).
#[cfg(not(madsim))]
pub fn bucket_of(url: &str, number: u16) -> anyhow::Result<Arc<RoutedBucket>> {
    bucket_of_on(crate::Host::process(), url, number)
}

/// `state`'s bucket at `url`, `open` opening it if it has none yet.
fn bucket_opened_in(
    state: &State,
    url: &str,
    open: impl FnOnce(&bucket_cell::object_bucket::Registered, &str) -> Result<ObjectBucket, String>,
) -> anyhow::Result<Arc<RoutedBucket>> {
    let mut buckets = state.buckets.lock().unwrap();
    if let Some(bucket) = buckets.get(url) {
        return Ok(bucket.clone());
    }
    // Its requests of other buckets' cells go there, by the registry of buckets (TASK-633.4).
    // One filtered snapshot shared across every bucket opened on this host (board 9684).
    let resolve = state.registered();
    let bucket = open(&resolve, url).map_err(anyhow::Error::msg)?.routed(resolve);
    let bucket = Arc::new(bucket);
    buckets.insert(url.to_owned(), bucket.clone());
    Ok(bucket)
}

/// cladefs: the bucket numbered `number`, as `host` whose bucket is at `home` reaches it.
#[cfg(not(madsim))]
pub async fn bucket_numbered_on(
    host: &crate::Host,
    home: &str,
    number: u16,
) -> Result<Arc<ObjectBucket>, Failure> {
    let ours = bucket_on(host, home)?;
    let identity = bucket_cell::identity::get(&*ours).await?;
    if identity.is_some_and(|identity| identity.number == number) {
        return Ok(ours.own().clone());
    }
    Ok(ours.numbered(number).await?)
}

/// cladefs: the bucket numbered `number`, as the host whose bucket is at `home` reaches it
/// (TASK-633.3, backlog doc-23): `home` itself, if of that number; else the bucket its requests of
/// that number's cells go to ([`RoutedBucket::numbered`]): the registry in `home`'s root, or the
/// root bucket's, records it; opened with its own options, and refused unless its identity is the
/// one recorded (`Error::Mismatched`). `NoBucket` if no registry records it.
#[cfg(not(madsim))]
pub async fn bucket_numbered(home: &str, number: u16) -> Result<Arc<ObjectBucket>, Failure> {
    bucket_numbered_on(crate::Host::process(), home, number).await
}

/// cladefs: the URLs of the buckets `host` opened, for tests counting them (board 8443).
#[cfg(any(test, feature = "testing"))]
pub fn buckets_opened_on(host: &crate::Host) -> Vec<String> {
    host.host_cells().buckets.lock().unwrap().keys().cloned().collect()
}

/// cladefs: what the decoded states of the cells `host`'s buckets keep hold.
#[cfg(any(target_os = "linux", madsim, test))]
pub fn decoded_held_on(host: &crate::Host) -> bucket_cell::recovery::Held {
    let buckets = host.host_cells().buckets.lock().unwrap();
    (buckets.values())
        .filter_map(|bucket| bucket_cell::Bucket::decoded(&**bucket))
        .map(bucket_cell::recovery::Decoded::held)
        .fold(bucket_cell::recovery::Held::default(), std::ops::Add::add)
}

/// cladefs: estimated bytes of idle decoded states retained strongly across `host`'s buckets
/// (board 11554).
#[cfg(any(target_os = "linux", madsim, test))]
pub fn decoded_kept_bytes_on(host: &crate::Host) -> u64 {
    let buckets = host.host_cells().buckets.lock().unwrap();
    (buckets.values())
        .filter_map(|bucket| bucket_cell::Bucket::decoded(&**bucket))
        .map(bucket_cell::recovery::Decoded::kept_bytes)
        .sum()
}

/// cladefs: evicts strong idle decoded states across `host`'s buckets until at most `target` bytes
/// remain, returning the total bytes evicted (board 11554).
#[cfg(any(target_os = "linux", madsim, test))]
pub fn evict_decoded_on(host: &crate::Host, target: u64) -> u64 {
    let buckets: Vec<Arc<RoutedBucket>> =
        host.host_cells().buckets.lock().unwrap().values().cloned().collect();
    let mut left = target;
    let mut evicted = 0;
    for bucket in buckets {
        let Some(decoded) = bucket_cell::Bucket::decoded(&*bucket) else { continue };
        let kept = decoded.kept_bytes();
        if kept > left {
            evicted += decoded.evict_to(left);
        }
        left = left.saturating_sub(decoded.kept_bytes());
    }
    evicted
}

/// cladefs: what the decoded states of the cells this process's buckets keep, or its cells hold
/// as the maps of the extents they share, hold: each once, however many cells share it
/// (`bucket_cell::recovery::Decoded::held`, TASK-453).
#[cfg(all(test, not(madsim)))]
pub fn decoded_held() -> bucket_cell::recovery::Held {
    decoded_held_on(crate::Host::process())
}

/// Makes `bucket` `host`'s of `url`, before any cell naming it opens.
#[cfg(any(test, feature = "testing", madsim))]
pub fn use_bucket_on(host: &crate::Host, url: &str, bucket: Arc<RoutedBucket>) {
    let mut buckets = host.host_cells().buckets.lock().unwrap();
    let had = buckets.entry(url.to_owned()).or_insert_with(|| bucket.clone());
    assert!(Arc::ptr_eq(had, &bucket), "this host has another bucket at {url}");
}

/// Makes `bucket` this process's of `url`, before any cell naming it opens, as the tests do the
/// buckets they put themselves in front of, and the simulation the bucket its hosts share; unless
/// it is already, the same.
#[cfg(all(test, not(madsim)))]
pub fn use_bucket(url: &str, bucket: Arc<RoutedBucket>) {
    use_bucket_on(crate::Host::process(), url, bucket);
}

/// cladefs: the URL of this host's bucket, if it is a host of cladefs's system: what a test made
/// it ([`use_host_bucket`]), or `CLADEFSD_BUCKET`.
pub fn host_bucket_url() -> Option<String> {
    #[cfg(test)]
    if let Some(url) = HOST_BUCKET.lock().unwrap().clone() {
        return Some(url);
    }
    // Unset, or not UTF-8, which no URL is: none.
    node_local::env::var("CLADEFSD_BUCKET").ok().filter(|url| !url.is_empty())
}

/// cladefs: the host's bucket's URL as a test made it ([`use_host_bucket`]).
#[cfg(test)]
static HOST_BUCKET: Mutex<Option<String>> = Mutex::new(None);

/// cladefs: makes `url` this host's bucket's, before `CLADEFSD_BUCKET`: as a test whose threads
/// may read the environment already makes it, where setting the variable would race their reads
/// (`std::env::set_var`'s safety, TASK-444.16).
#[cfg(test)]
pub fn use_host_bucket(url: String) {
    *HOST_BUCKET.lock().unwrap() = Some(url);
}

/// The URL of the bucket a cell made at `path`, a directory, keeps its cells in: this host's,
/// if it is a host of cladefs's system ([`host_bucket_url`], as [`crate::bucket::Host`] has
/// it); else a file bucket of the cell's own ([`bucket_named`]).
pub fn bucket_for(path: &Path) -> anyhow::Result<String> {
    bucket_named(host_bucket_url(), path)
}

/// The URL of the bucket a cell made at `path`, a directory, keeps its cell in, on the host
/// whose bucket's URL is `host`, if it is one: that; or a file bucket of the cell's own, beside
/// its directory ([`OWN_BUCKET`]), which its forks share, and which outlives it.
fn bucket_named(host: Option<String>, path: &Path) -> anyhow::Result<String> {
    if let Some(url) = host {
        return Ok(url);
    }
    let path = upload::blocking(|| host_fs::canonicalize(&path))?;
    let name = path.file_name().ok_or_else(|| anyhow::anyhow!("{} has no name", path.display()))?;
    let own = path.with_file_name(format!("{}{OWN_BUCKET}", name.to_string_lossy()));
    let url = url::Url::from_directory_path(&own)
        .map_err(|()| anyhow::anyhow!("{} names no file bucket", own.display()))?;
    Ok(url.to_string())
}

/// Whether `local` keeps its cell in a file bucket no host's collector collects: one made beside
/// a cell directory, where no host's bucket was ([`bucket_named`]), its own or that of the
/// cell it was forked from, which its forks share.
pub fn keeps_own_bucket(local: &LocalCell<HostDir>) -> bool {
    let url = local.bucket().unwrap_or_default();
    url.starts_with("file:") && url.trim_end_matches('/').ends_with(OWN_BUCKET)
}

/// The fencing token copies of cells never placed take their cells with where this process
/// opens them as no host's daemon does, as [`crate::Storage`] does: this host's placement's, if it
/// is a host of cladefs's system (`CLADEFSD_TOKEN`, [`crate::bucket::host_token`]); else the
/// first placement's. So the daemon and the cladefs CLI on one host take each other's cells.
pub fn token() -> Result<u64, Failure> {
    match node_local::env::var_os("CLADEFSD_TOKEN").filter(|token| !token.is_empty()) {
        Some(_) => crate::bucket::host_token(),
        None => Ok(FIRST_PLACEMENT),
    }
}

/// How a cell `host` opens where no host's daemon serves it keeps its cell in its bucket.
pub fn keeping_on(host: &crate::Host) -> Result<upload::Options, Failure> {
    let fetching = Some(crate::bucket::fetching_on(host));
    Ok(upload::Options { token: token()?, fetching, ..Default::default() })
}

/// How a cell this process opens where no host's daemon serves it keeps its cell in its bucket:
/// its uploader's options, as a copy never placed takes its cell's head here ([`token`]).
#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub fn keeping() -> Result<upload::Options, Failure> {
    keeping_on(crate::Host::process())
}

/// A cell directory opened ([`open`]), with the bucket its description names, which its cell is
/// kept in, and its uploader, which took the cell's head, or holds its version (TASK-165.9).
pub struct Opened {
    pub local: Arc<LocalCell<HostDir>>,
    pub bucket: Arc<RoutedBucket>,
    pub uploader: Uploader<HostDir>,
}

impl Opened {
    /// The cell, whose maker made it whole (TASK-196): opening it opens it from now on.
    pub fn made(&self) -> anyhow::Result<()> {
        self.local.made()
    }

    /// cladefs: closes it, its uploader stopped (`Uploader::close`), and returns once nothing of it
    /// holds the cell open, which may be opened again at once (TASK-685.3).
    pub async fn close(self) {
        let Opened { local, bucket, uploader } = self;
        drop((local, bucket));
        uploader.close().await;
    }
}

/// The bucket the cell directory at `path` names on `host`, if its description is there.
pub fn bucket_at_on(host: &crate::Host, path: &Path) -> anyhow::Result<Option<Arc<RoutedBucket>>> {
    match local_cell::host_bucket(path) {
        Ok(Some(url)) => Ok(Some(bucket_on(host, &url)?)),
        Ok(None) => Ok(None),
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// The bucket the cell directory at `path` names, if its description is there: as deleting it, or
/// what of it a deletion that stopped left, goes by. A cell of another format is refused
/// (`local_cell::Format`), and a description there but unread, or not whole: taken for none, its
/// cell would be deleted in no bucket, or the daemon's, and left in its own (board 6908).
#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub fn bucket_at(path: &Path) -> anyhow::Result<Option<Arc<RoutedBucket>>> {
    bucket_at_on(crate::Host::process(), path)
}

/// The options cells are opened with: their extents hold at most what the guest's device says
/// one does (doc-4's `max_extent`).
pub fn options() -> Options {
    Options { max_extent: crate::OpenCell::MAX_EXTENT, ..Options::default() }
}

/// Makes a new cell at `path` on `host`, whole, as made.
pub async fn create_on(
    host: &crate::Host,
    path: &Path,
    capacity: u64,
    of: CellOf,
    kind: cell::Kind,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    let opened = create_unmade_on(host, path, capacity, of, kind, None, keeping).await?;
    if let Err(err) = upload::blocking(|| opened.made()) {
        opened.close().await;
        // Its making's last step failing may have left it looking made (`UNMADE` gone, not
        // durably): no store whose making failed is left behind (board 5668).
        upload::blocking(|| discard_on(host, path));
        return Err(err.into());
    }
    Ok(opened)
}

#[cfg(all(test, not(madsim)))]
pub async fn create(
    path: &Path,
    capacity: u64,
    of: CellOf,
    kind: cell::Kind,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    create_on(crate::Host::process(), path, capacity, of, kind, keeping).await
}

/// Makes a new cell at `path` on `host`, unmade until its maker says it is whole.
pub async fn create_unmade_on(
    host: &crate::Host,
    path: &Path,
    capacity: u64,
    of: CellOf,
    kind: cell::Kind,
    bucket: Option<&str>,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    upload::blocking(|| host_fs::create_dir(path))
        .map_err(|err| Failure::local(err, format!("creating {}", path.display())))?;
    let made = async {
        let url = bucket.map_or_else(|| bucket_for(path), |bucket| Ok(bucket.to_owned()))?;
        let bucket = bucket_on(host, &url)?;
        let (cell, initial_head) = match of {
            CellOf::New(tenant) => {
                let cell = new_cell(&bucket, tenant).await?;
                make_keys_on(host, &bucket, cell).await?;
                (cell, None)
            }
            CellOf::Named(cell) => match learn_taken(&bucket, cell).await? {
                Some(read) => (cell, Some(read)),
                None => {
                    make_keys_on(host, &bucket, cell).await?;
                    (cell, None)
                }
            },
            CellOf::Taken(cell) => match learn_taken(&bucket, cell).await? {
                Some(read) => (cell, Some(read)),
                None => return Err(bucket_cell::Error::NoCell.into()),
            },
        };
        let recovering = upload::blocking(|| -> Result<_, Failure> {
            let description = Description { block_size: BLOCK_SIZE, capacity, cell };
            local_cell::create_host_on(host.local(), path, description, &url, kind)?;
            let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
            let synced = host_fs::File::open(parent.unwrap_or(Path::new(".")))
                .and_then(|parent| parent.sync_all());
            synced.map_err(|err| Failure::local(err, "syncing a cell directory's parent"))?;
            Ok(LocalCell::recover_unmade(host_dir_on(host, path)?)?)
        })?;
        opened_with_head_on(host, recovering, keeping, initial_head).await
    };
    // Leave nothing half made behind.
    let made = made.await;
    if made.is_err() {
        upload::blocking(|| discard_on(host, path));
    }
    made
}

#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub async fn create_unmade(
    path: &Path,
    capacity: u64,
    of: CellOf,
    kind: cell::Kind,
    bucket: Option<&str>,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    create_unmade_on(crate::Host::process(), path, capacity, of, kind, bucket, keeping).await
}

/// A new cell made on `host`, a root, its KEK from the host's pool, before its cell first opens.
async fn make_keys_on(
    host: &crate::Host,
    bucket: &RoutedBucket,
    cell: CellId,
) -> Result<(), Failure> {
    let kms = crate::bucket::kms_on(host)?;
    bucket_cell::keys::make(bucket, &kms, cell, rand::random()).await?;
    Ok(())
}

/// Reads `cell`'s head to check whether the bucket holds it, returning the [`head::Read`] so the
/// opening immediately after reuses it instead of issuing a second head GET (board 6017).
async fn learn_taken(
    bucket: &RoutedBucket,
    cell: CellId,
) -> Result<Option<bucket_cell::head::Read>, Failure> {
    let read = bucket_cell::head::read(bucket, cell).await?;
    Ok(read.head()?.is_some().then_some(read))
}

/// Opens the cell directory at `path` on `host` as of its last commit, with the bucket it names.
pub async fn open_on(
    host: &crate::Host,
    path: &Path,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    // cladefs: a close of it on this host waited for first, holding no thread (doc-24).
    closed_on(host, path).await?;
    let recovered = upload::blocking(|| -> Result<_, Failure> {
        Ok(LocalCell::recover(host_dir_on(host, path)?)?)
    });
    match recovered {
        Ok(recovering) => opened_on(host, recovering, keeping).await,
        Err(recover_err) => {
            // cladefs: for an active Fast WAL cell, local disk is cache-only (doc-29, Hotbed
            // review 118): if local WAL recovery fails (e.g. damaged cache files), peek the cell's
            // description and check the cold bucket head. Only if the head confirms an active Fast
            // WAL do we drop the damaged local WAL and recover from the remote buckets; non-Fast-WAL
            // cells still fail hard with the original local error.
            let peeked = upload::blocking(|| {
                let dir = host_dir_on(host, path)?;
                Ok::<_, Failure>(LocalCell::peek_bucket(&dir)?)
            });
            if let Ok((cell, Some(url))) = peeked
                && let Ok(bucket) = bucket_on(host, &url)
                && let Ok(read) = bucket_cell::head::read(&*bucket, cell).await
                && matches!(
                    read.head(),
                    Ok(Some(h)) if h.fast_wal.as_ref().and_then(|fw| fw.active.as_ref()).is_some()
                )
            {
                log::warn!(
                    "cladefs: local cache WAL of active fast-WAL cell {cell} at {} is damaged \
                     ({recover_err}); dropping local cache and recovering from remote buckets",
                    path.display()
                );
                let recovering = upload::blocking(|| -> Result<_, Failure> {
                    Ok(LocalCell::recover_dropping_wal(host_dir_on(host, path)?)?)
                })?;
                return opened_with_head_on(host, recovering, keeping, Some(read)).await;
            }
            Err(recover_err)
        }
    }
}

#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub async fn open(path: &Path, keeping: upload::Options) -> Result<Opened, Failure> {
    open_on(crate::Host::process(), path, keeping).await
}

/// cladefs: returns once no cell of the directory at `path` is closing on `host`.
async fn closed_on(host: &crate::Host, path: &Path) -> Result<(), Failure> {
    let waited = local_cell::closed_on(host.local(), path).await;
    waited.map_err(|err| Failure::local(err, format!("{}", path.display())))
}

/// The host directory of the cell at `path` on `host`.
fn host_dir_on(host: &crate::Host, path: &Path) -> Result<HostDir, Failure> {
    HostDir::new_on(host.local(), path)
        .map_err(|err| Failure::local(err, format!("{}", path.display())))
}

/// Opens the cell directory at `path` on `host`, dropping its WAL if another writer diverged.
pub async fn take_on(
    host: &crate::Host,
    path: &Path,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    match open_on(host, path, keeping.clone()).await {
        Err(Failure::Bucket(bucket_cell::Error::Diverged)) => {
            let recovering = upload::blocking(|| -> Result<_, Failure> {
                let mut recovering = LocalCell::recover(host_dir_on(host, path)?)?;
                log::warn!(
                    "cladefs: dropping {}'s write-ahead log: another writer moved cell {} past it",
                    path.display(),
                    recovering.cell()
                );
                recovering.drop_wal()?;
                Ok(recovering)
            })?;
            opened_on(host, recovering, keeping).await
        }
        opened => opened,
    }
}

/// The cell `recovering` recovered, opened on `host` with its uploader as `keeping` says.
async fn opened_on(
    host: &crate::Host,
    recovering: Recovering<HostDir>,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    opened_with_head_on(host, recovering, keeping, None).await
}

/// Like [`opened_on`], passing an already-fetched [`bucket_cell::head::Read`] to the uploader.
async fn opened_with_head_on(
    host: &crate::Host,
    recovering: Recovering<HostDir>,
    keeping: upload::Options,
    initial_head: Option<bucket_cell::head::Read>,
) -> Result<Opened, Failure> {
    let url = recovering.bucket().map(str::to_owned);
    let path = || recovering_path(&recovering);
    let url = url.ok_or_else(|| Failure::msg(format!("{} names no bucket", path())))?;
    let bucket = bucket_on(host, &url).with_context(|| format!("opening the bucket {url}"))?;
    let runtime = crate::bucket::runtime_on(host)?;
    let holding = (rand::random(), keeping);
    let opening = upload::open_with_head(
        recovering,
        options(),
        bucket.clone(),
        holding,
        &runtime,
        initial_head,
    );
    let (local, uploader) = opening.await?;
    Ok(Opened { local, bucket, uploader })
}

/// The path of the directory `recovering` is of, for what fails of it.
fn recovering_path(recovering: &Recovering<HostDir>) -> String {
    format!("cell {}'s cell directory", recovering.cell())
}

/// Whether `path` is a directory a cell's making, or a fork's, left unmade on `host`.
pub fn unmade_on(host: &crate::Host, path: &Path) -> bool {
    local_cell::unmade_host_on(host.local(), path)
}

#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub fn unmade(path: &Path) -> bool {
    unmade_on(crate::Host::process(), path)
}

/// Removes what a making that failed left at `path` on `host`.
pub fn discard_on(host: &crate::Host, path: &Path) {
    if host_fs::exists(&path)
        && let Err(err) = local_cell::remove_cell_dir_on(host.local(), path)
    {
        log::warn!("cladefs: removing {}, which a making left: {err:#}", path.display());
        local_cell::left_unmade_on(host.local(), path);
    }
}

/// Forks `local`, a cell at a version, to `child` on `host`.
pub async fn fork_on(
    host: &crate::Host,
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
) -> Result<(), Error> {
    fork_with_on(host, local, uploader, path, child, prepare, |child, inheriting| {
        uploader.forked(child, inheriting)
    })
    .await
}

/// Shared bucket-fork and local child directory creation used by both at-version forks
/// ([`fork_on`]) and direct version forks of writable cells ([`fork_through_on`]).
async fn fork_with_on<F>(
    host: &crate::Host,
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
    record: impl FnOnce(Child, bucket_cell::keys::Inheriting) -> F,
) -> Result<(), Error>
where
    F: std::future::Future<Output = Result<(), Failure>>,
{
    may_fork_into(local.description().cell, uploader, child.cell.tenant)?;
    let inheriting = uploader.inheriting(child).await?;
    upload::blocking(|| host_fs::create_dir(path)).map_err(|err| match err.kind() {
        std::io::ErrorKind::AlreadyExists => {
            Error::new(anyhow::Error::new(err).context(Errno::EEXIST))
        }
        _ => Error::new(anyhow::Error::new(err).context(format!("creating {}", path.display()))),
    })?;
    let cell = child.cell;
    let failed = |err| fork_failure(cell, err);
    let made = match record(child, inheriting).await {
        Err(err) => Err(failed(err)),
        Ok(()) => upload::blocking(|| -> anyhow::Result<()> {
            failpoint!("fork::recorded");
            make_child_on(host, local, path, cell, prepare)
                .with_context(|| format!("forking to cell {cell}: making its cell directory"))?;
            failpoint!("fork::made");
            Ok(())
        }),
    };
    let Err(err) = made else { return Ok(()) };
    // Leave nothing half made behind: no cell, and no fork whose cell went.
    upload::blocking(|| discard_on(host, path));
    if let Err(why) = uploader.unfork(child).await {
        log::warn!("cladefs: leaving cell {cell}, whose fork failed: {why}");
    }
    Err(Error::new(err))
}

/// cladefs: the cell directory at `path`, of `cell`, made the version `version` on `host`.
pub fn made_version_on(
    host: &crate::Host,
    cell: CellId,
    version: u64,
    path: &Path,
    named: &BTreeSet<u64>,
) {
    let mut made = host.host_cells().versions_made.lock().unwrap();
    made.retain(|&(of, id), _| of != cell || named.contains(&id));
    made.insert((cell, version), path.to_owned());
}

/// cladefs: the versions `ids` of `cell` were dropped on `host`.
pub fn dropped_versions_on(host: &crate::Host, cell: CellId, ids: &BTreeSet<u64>) {
    host.host_cells()
        .versions_made
        .lock()
        .unwrap()
        .retain(|&(of, id), _| of != cell || !ids.contains(&id));
}

/// cladefs: how many versions it says this process made, which the tests bound.
#[cfg(test)]
pub fn versions_made() -> usize {
    crate::Host::process().host_cells().versions_made.lock().unwrap().len()
}

/// cladefs: where the cell `host` made the version `version` of `cell` of is, if it did.
#[cfg(any(target_os = "linux", madsim))]
pub fn version_made_of_on(host: &crate::Host, cell: CellId, version: u64) -> Option<PathBuf> {
    host.host_cells().versions_made.lock().unwrap().get(&(cell, version)).cloned()
}

/// cladefs: where a cell at the version `version` of `cell` is kept for forks of it, served by
/// `cache`, the host's, if it has one: a directory of its own, among the cache's cells at versions,
/// on its filesystem, as a cell elsewhere than the cache is refused (board 6589,
/// [`Cache::versions`]); without, in the process's temporary directory. Such a cell holds nothing
/// of its own.
pub fn version_dir(cell: CellId, version: u64, cache: Option<&Cache>) -> PathBuf {
    let root = match cache {
        Some(cache) => cache.versions().to_owned(),
        None => std::env::temp_dir().join(format!("cladefs-versions-{}", std::process::id())),
    };
    root.join(format!("{:x}.{:x}.{:x}", cell.tenant, cell.id, version))
}

/// Forks `local`, its cell's version `version` made and uploaded, directly in the bucket to
/// `child` on `host`, without materializing a temporary version directory on disk (board 11701).
pub async fn fork_through_on(
    host: &crate::Host,
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    version: u64,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
) -> Result<(), Error> {
    let forked = fork_with_on(host, local, uploader, path, child, prepare, |child, inheriting| {
        uploader.fork_version(version, child, inheriting)
    })
    .await;
    // The version, the fork's alone, dropped whether or not it forked. The child made is the
    // fork's last step that can fail it: past it, a drop failing leaves the version named, which
    // the cell's versions list for dropping again, and a fork reported failed would leave a live
    // child, its cell whole at `path`, that its caller never learns of (TASK-609).
    let dropped = uploader.drop_versions(&std::collections::BTreeSet::from([version])).await;
    forked?;
    if let Err(failure) = dropped {
        log::error!(
            "cladefs: forked {:?} to {:?}, its version {version:x} not dropped: {failure}",
            local.description().cell,
            child.cell,
        );
    }
    Ok(())
}

#[cfg(test)]
pub async fn fork_through(
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    version: u64,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
) -> Result<(), Error> {
    fork_through_on(crate::Host::process(), local, uploader, version, path, child, prepare).await
}

/// cladefs: the error of a fork to `cell` that failed with `err`: `EEXIST` for an ID another cell
/// has, `ENOENT` for a version deleted, `EACCES` for another tenant's version not shared; and
/// `ESTALE` for an ephemeral fork of a host whose incarnation was decommissioned (board 8519).
fn fork_failure(cell: CellId, err: Failure) -> anyhow::Error {
    match err {
        Failure::Bucket(bucket_cell::Error::IdTaken(_)) => taken(cell),
        Failure::VersionDropped => anyhow::Error::new(Errno::ENOENT)
            .context(format!("forking to cell {cell}: the version was deleted")),
        // cladefs: another tenant's version, not shared (TASK-395.7).
        Failure::Bucket(refused @ bucket_cell::Error::NotShared(_)) => {
            anyhow::Error::new(Errno::EACCES).context(format!("forking to cell {cell}: {refused}"))
        }
        Failure::Bucket(refused @ bucket_cell::Error::Decommissioned(_)) => {
            anyhow::Error::new(Errno::ESTALE).context(format!("forking to cell {cell}: {refused}"))
        }
        err => anyhow::anyhow!("forking to cell {cell}: {err}"),
    }
}

/// cladefs: forks `local`, at a version, to `child`, an ephemeral cell on `host`.
pub async fn fork_ephemeral_on(
    host_ctx: &crate::Host,
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
) -> Result<(), Error> {
    may_fork_into(local.description().cell, uploader, child.cell.tenant)?;
    let origin = uploader.at_version_origin().ok_or(Errno::EINVAL)?;
    let host = crate::ephemeral_host::host_on(host_ctx)?;
    let url = local.bucket().map(str::to_owned).ok_or(Errno::EINVAL)?;
    let kind = local.kind().ok_or(Errno::EINVAL)?;
    let inheriting = uploader.inheriting(child).await?;
    make_dir(path)?;
    let cell = child.cell;
    let entry = crate::ephemeral_host::Entry {
        child: cell,
        origin,
        path: path.to_owned(),
        bucket: url,
        epoch: 0,
        migrating: None,
    };
    let made = match upload::blocking(|| host.note(&entry)) {
        Err(err) => Err(err),
        Ok(()) => match uploader.forked_ephemeral(child, inheriting, host.incarnation()).await {
            Err(err) => Err(fork_failure(cell, err)),
            Ok((checkpoint, ring)) => upload::blocking(|| -> anyhow::Result<()> {
                let capacity = local.description().capacity;
                let description = Description { block_size: BLOCK_SIZE, capacity, cell };
                local_cell::create_host_ephemeral_on(
                    host_ctx.local(),
                    path,
                    description,
                    &entry.bucket,
                    kind,
                    &checkpoint,
                    ring.try_encrypting()?,
                    |making| {
                        prepare(making)?;
                        durable_in_parent(making, path);
                        Ok(())
                    },
                )
                .with_context(|| format!("forking to ephemeral cell {cell}: its cell directory"))?;
                local_cell::made_host_on(host_ctx.local(), path)?;
                Ok(())
            }),
        },
    };
    let Err(err) = made else { return Ok(()) };
    log::warn!("cladefs: forking ephemeral cell {cell} at {}: {err:#}", path.display());
    upload::blocking(|| discard_on(host_ctx, path));
    if let Err(why) = host.retire(&entry).await {
        log::warn!("cladefs: retiring ephemeral cell {cell}'s reference, its fork failed: {why:#}");
    }
    Err(Error::new(err))
}

/// cladefs: makes the directory at `path` for a fork's cell: `EEXIST` if anything is there.
fn make_dir(path: &Path) -> Result<(), Error> {
    upload::blocking(|| host_fs::create_dir(path)).map_err(|err| match err.kind() {
        std::io::ErrorKind::AlreadyExists => {
            Error::new(anyhow::Error::new(err).context(Errno::EEXIST))
        }
        _ => Error::new(anyhow::Error::new(err).context(format!("creating {}", path.display()))),
    })
}

/// cladefs: creates a standalone (parentless) host-local ephemeral cell of `kind` and `capacity`
/// bytes for `tenant` at `path` on `host`, unmade until the caller marks it whole: encrypted under
/// a tenant-wrapped cell KEK via the host's KMS, writing nothing to the bucket and holding no
/// bucket reference or `CLADEFSD_EPHEMERAL` journal entry.
pub async fn create_ephemeral_unmade_on(
    host: &crate::Host,
    path: &Path,
    capacity: u64,
    tenant: u64,
    kind: cell::Kind,
    keeping: &upload::Options,
) -> Result<(Arc<LocalCell<HostDir>>, Arc<RoutedBucket>, bucket_cell::keys::RingHold), Failure> {
    let url = bucket_for(path)?;
    let bucket = bucket_on(host, &url).with_context(|| format!("opening the bucket {url}"))?;
    let cell = new_cell(&bucket, tenant).await?;
    let runtime = crate::bucket::runtime_on(host)?;
    let (checkpoint, ring, ring_hold) =
        upload::create_standalone_ephemeral(&*bucket, &runtime, cell, capacity, rand::random())
            .await?;
    upload::blocking(|| host_fs::create_dir(path))
        .map_err(|err| Failure::local(err, format!("creating {}", path.display())))?;
    let made = upload::blocking(|| -> anyhow::Result<LocalCell<HostDir>> {
        let description = Description { block_size: BLOCK_SIZE, capacity, cell };
        local_cell::create_host_ephemeral_on(
            host.local(),
            path,
            description,
            &url,
            kind,
            &checkpoint,
            ring.try_encrypting()?,
            |making| {
                durable_in_parent(making, path);
                Ok(())
            },
        )
        .with_context(|| {
            format!("creating standalone ephemeral cell {cell}: its cell directory")
        })?;
        let recovering = LocalCell::recover_unmade(host_dir_on(host, path)?)?;
        let opening = local_cell::EphemeralOpening {
            parent: None,
            keys: ring,
            cache: keeping.cache.clone(),
            held: keeping.held.clone(),
        };
        recovering.open_ephemeral(options(), opening)
    });
    match made {
        Ok(local) => Ok((Arc::new(local), bucket, ring_hold)),
        Err(err) => {
            upload::blocking(|| discard_on(host, path));
            Err(Failure::from(err))
        }
    }
}

/// cladefs: creates a standalone (parentless) host-local ephemeral cell at `path` on `host`,
/// marked whole (`made`).
pub async fn create_ephemeral_on(
    host: &crate::Host,
    path: &Path,
    capacity: u64,
    tenant: u64,
    kind: cell::Kind,
    keeping: &upload::Options,
) -> Result<(Arc<LocalCell<HostDir>>, Arc<RoutedBucket>, bucket_cell::keys::RingHold), Failure> {
    let (local, bucket, ring_hold) =
        create_ephemeral_unmade_on(host, path, capacity, tenant, kind, keeping).await?;
    if let Err(err) = upload::blocking(|| local.made()) {
        // Error path: best-effort close before discarding the unmade directory.
        let _ = local.close().await;
        drop((local, bucket, ring_hold));
        upload::blocking(|| discard_on(host, path));
        return Err(Failure::from(err));
    }
    Ok((local, bucket, ring_hold))
}

/// cladefs: opens the ephemeral cell at `path` on `host`.
pub async fn open_ephemeral_on(
    host: &crate::Host,
    path: &Path,
    keeping: &upload::Options,
) -> Result<
    (
        Arc<LocalCell<HostDir>>,
        Arc<RoutedBucket>,
        bucket_cell::keys::RingHold,
        Option<upload::Forked>,
    ),
    Failure,
> {
    closed_on(host, path).await?;
    let recovering = upload::blocking(|| -> Result<_, Failure> {
        Ok(LocalCell::recover(host_dir_on(host, path)?)?)
    })?;
    let url = recovering.bucket().map(str::to_owned);
    let url = url.ok_or_else(|| Failure::msg(format!("{} names no bucket", path.display())))?;
    let bucket = bucket_on(host, &url).with_context(|| format!("opening the bucket {url}"))?;
    let runtime = crate::bucket::runtime_on(host)?;
    let held = (keeping.cache.clone(), keeping.held.clone());
    let opened = upload::open_ephemeral(recovering, options(), bucket.clone(), &runtime, held);
    let (local, keys, forked) = opened.await?;
    Ok((local, bucket, keys, forked))
}

/// cladefs: whether the cell directory at `path` is an ephemeral cell's (doc-25, TASK-698.3).
pub fn is_ephemeral(path: &Path) -> bool {
    local_cell::host_ephemeral(path).unwrap_or(false)
}

/// The error of a fork to `cell`, whose ID another cell has in the bucket.
fn taken(cell: CellId) -> anyhow::Error {
    let refused = format!("forking to cell {cell}: another cell has its ID");
    anyhow::Error::new(Errno::EEXIST).context(refused)
}

/// Makes the cell directory of `cell` on `host`.
fn make_child_on(
    host: &crate::Host,
    parent: &LocalCell<HostDir>,
    path: &Path,
    cell: CellId,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let capacity = parent.description().capacity;
    let named = || anyhow::anyhow!("{} names no bucket", parent.dir().path().display());
    let url = parent.bucket().map(str::to_owned).ok_or_else(named)?;
    let kind = parent
        .kind()
        .ok_or_else(|| anyhow::anyhow!("{} names no cell's kind", parent.dir().path().display()))?;
    let description = Description { block_size: BLOCK_SIZE, capacity, cell };
    local_cell::create_host_with_on(host.local(), path, description, &url, kind, |making| {
        prepare(making)?;
        durable_in_parent(making, path);
        Ok(())
    })?;
    local_cell::made_host_on(host.local(), path)?;
    Ok(())
}

/// cladefs: has `making`, of the cell directory at `path`, make the directory's entry in its parent
/// durable with the rest of it (board 7420): a fork's, which its maker made.
fn durable_in_parent(making: &mut Making<'_, HostDir>, path: &Path) {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.unwrap_or(Path::new(".")).to_owned();
    making.also("the cell directory's parent", move || host_fs::File::open(&parent)?.sync_all());
}

/// Deletes the cell directory at `path` on `host`, which nothing has open.
pub async fn delete_on(
    host_ctx: &crate::Host,
    path: &Path,
    bucket: Option<&RoutedBucket>,
    token: impl Fn() -> Result<u64, Failure>,
) -> Result<(), Error> {
    // cladefs: an ephemeral cell's reference retired, its note forgotten (doc-25, TASK-698.3);
    // standalone (parentless) ephemeral cells hold no bucket reference or journal entry.
    if upload::blocking(|| is_ephemeral(path)) {
        return delete_with_on(host_ctx, path, async |dir, cell| {
            let checkpoint =
                upload::blocking(|| local_cell::host_local_checkpoint_on(host_ctx.local(), dir))?;
            let checkpoint = checkpoint
                .ok_or_else(|| Failure::msg(format!("{} has no checkpoint", dir.display())))?;
            let Some(parent) = checkpoint.parent() else {
                return Ok(());
            };
            let origin = bucket_cell::layout::Origin {
                parent: parent.cell,
                at: parent.at,
                epoch: parent.epoch,
            };
            let host = crate::ephemeral_host::host_on(host_ctx).map_err(|errno| {
                Failure::msg(format!(
                    "deleting ephemeral cell {cell}: no CLADEFSD_EPHEMERAL: {errno}"
                ))
            })?;
            let url = upload::blocking(|| local_cell::host_bucket(dir))?;
            let url =
                url.ok_or_else(|| Failure::msg(format!("{} names no bucket", dir.display())))?;
            let entry = crate::ephemeral_host::Entry {
                child: cell,
                origin,
                path: dir.to_owned(),
                bucket: url,
                epoch: 0,
                migrating: None,
            };
            host.retire(&entry).await.map_err(|err| Failure::msg(format!("{err:#}")))
        })
        .await;
    }
    // As this copy's placement, or this host's, deletes it: not from under a later one (`ESTALE`).
    // A cell at a version deletes nothing in the bucket: the version is its cell's.
    delete_with_on(host_ctx, path, async |dir, cell| match bucket {
        Some(_) if upload::blocking(|| local_cell::host_version(dir))?.is_some() => Ok(()),
        Some(bucket) => {
            let placed = upload::blocking(|| local_cell::host_token(dir))?;
            let token = placed.map_or_else(&token, Ok)?;
            local_cell::upload::delete_cell(bucket, cell, token).await
        }
        None => Ok(()),
    })
    .await
}

#[cfg(all(test, not(madsim)))]
pub async fn delete(
    path: &Path,
    bucket: Option<&RoutedBucket>,
    token: impl Fn() -> Result<u64, Failure>,
) -> Result<(), Error> {
    delete_on(crate::Host::process(), path, bucket, token).await
}

/// Deletes the cell directory at `path` on `host`, `elsewhere` deleting its cell where else it is
/// kept.
pub async fn delete_with_on(
    host: &crate::Host,
    path: &Path,
    elsewhere: impl AsyncFnOnce(&Path, CellId) -> Result<(), Failure>,
) -> Result<(), Error> {
    if !upload::blocking(|| host_fs::exists(&path)) {
        return Err(Errno::ENOENT.into());
    }
    let deleting = async |cell| -> anyhow::Result<()> {
        failpoint!("delete::elsewhere");
        Ok(elsewhere(path, cell).await?)
    };
    local_cell::delete_host_on(host.local(), path, deleting).await.map_err(|err| {
        // Its errno, if deleting the cell elsewhere failed with one: `ESTALE` for a cell a
        // later placement holds.
        let failure = err.chain().find_map(|cause| cause.downcast_ref::<Failure>());
        match failure.and_then(crate::bucket::errno) {
            Some(errno) => Error::new(err.context(errno)),
            None => Error::new(err),
        }
    })?;
    // cladefs: what it made versions of is gone (TASK-610).
    host.host_cells().versions_made.lock().unwrap().retain(|_, of| of != path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// A new cell of a new cell of the default tenant's.
    fn new() -> CellOf {
        CellOf::New(DEFAULT_TENANT)
    }

    /// TASK-196: a cell its maker never made whole is no cell: it refuses to open, and
    /// [`unmade`] says so, which deleting it, or making it anew, goes by; once made, it opens.
    #[tokio::test(flavor = "multi_thread")]
    async fn cells_unmade_open_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let made =
            create_unmade(&path, MIB, new(), cell::Kind::Fxfs, None, keeping().unwrap()).await;
        let cell = made.unwrap().local.description().cell;
        assert!(host_fs::may_block(|| unmade(&path)));
        let refused = open(&path, keeping().unwrap()).await.err().unwrap();
        let Failure::Local(refused) = refused else { panic!("{refused}") };
        assert_eq!(refused.downcast_ref::<local_cell::Unmade>(), Some(&local_cell::Unmade));
        host_fs::may_block(|| {
            local_cell::made_host(&path).unwrap();
            assert!(!unmade(&path));
        });
        assert_eq!(
            open(&path, keeping().unwrap())
                .await
                .map(|opened| opened.local.description().cell)
                .ok(),
            Some(cell)
        );
    }

    /// TASK-633.3: a host finds each bucket by its number: its own; others by the registry in its
    /// bucket's root; one its copy lacks by the root bucket's; and refuses one whose URL reaches
    /// another bucket, and a number no registry records.
    #[tokio::test(flavor = "multi_thread")]
    async fn hosts_find_buckets_by_number() {
        use bucket_cell::identity::{self, Details, Entry};
        let dir = tempfile::tempdir().unwrap();
        let url = |name: &str| {
            host_fs::may_block(|| host_fs::create_dir(dir.path().join(name))).unwrap();
            format!("file://{}", dir.path().join(name).display())
        };
        let (root, home, other, late) = (url("root"), url("home"), url("other"), url("late"));
        let number =
            async |bucket: &ObjectBucket| identity::get(bucket).await.unwrap().unwrap().number;
        crate::bucket::format_bucket(&root, Details::default()).unwrap();
        assert_eq!(crate::bucket::add_bucket(&root, &home, Details::default()).unwrap().number, 2);
        assert_eq!(crate::bucket::add_bucket(&root, &other, Details::default()).unwrap().number, 3);
        assert_eq!(number(&bucket_numbered(&home, 2).await.unwrap()).await, 2);
        assert_eq!(number(&bucket_numbered(&home, 3).await.unwrap()).await, 3);
        assert_eq!(number(&bucket_numbered(&home, 1).await.unwrap()).await, 1);

        // Added, its registry copied to the root and to it alone: home's copy lacks it.
        let (root_bucket, late_bucket) = (bucket(&root).unwrap(), bucket(&late).unwrap());
        let late_details = Details { url: Some(late.clone()), ..Details::default() };
        let (_, registry) =
            identity::add(&*root_bucket, &Details::default(), &*late_bucket, late_details)
                .await
                .unwrap();
        identity::spread(&registry, &*late_bucket).await.unwrap();
        assert_eq!(number(&bucket_numbered(&home, 4).await.unwrap()).await, 4);

        // A copy recording bucket 5 at another's URL: that bucket refused as it.
        let mut wrong = registry.clone();
        wrong.version += 1;
        let at_other = Details { url: Some(other.clone()), ..Details::default() };
        wrong.buckets.push(Entry { number: 5, id: 99, details: at_other });
        identity::spread(&wrong, &*bucket(&home).unwrap()).await.unwrap();
        let refused = bucket_numbered(&home, 5).await.map(drop);
        assert_eq!(
            refused,
            Err(bucket_cell::Error::Mismatched { number: 5, found: Some(3) }.into())
        );
        let refused = bucket_numbered(&home, 9).await.map(drop);
        assert_eq!(refused, Err(bucket_cell::Error::NoBucket(9).into()));
    }

    /// TASK-364: a new cell's ID is its bucket's to give, one of a block this process
    /// took there, the next cell's the next of it; a cell directory of a cell the bucket has not is
    /// refused (`NoCell`), and leaves nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn new_cells_ids_are_their_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (dir.path().join("one"), dir.path().join("two"));
        let url = format!("memory:///new-cells-{}", std::process::id());
        let made = async |path: &Path, of| {
            let made =
                create_unmade(path, MIB, of, cell::Kind::Fxfs, Some(&url), keeping().unwrap())
                    .await;
            made.unwrap().local.description().cell
        };
        let first = made(&one, new()).await;
        let second = made(&two, new()).await;
        assert_eq!(second.id, first.id + 1);
        assert_eq!((first.id, second.id), (first.id, second.id));
        let block = first.id / bucket_cell::objectid::ID_BLOCK;
        let bucket = bucket(&url).unwrap();
        let taken = bucket_cell::layout::objectid_block(block);
        assert!(bucket_cell::bucket::get(&*bucket, &taken).await.unwrap().is_some());

        let absent = CellId { tenant: DEFAULT_TENANT, id: second.id + 1 };
        let path = dir.path().join("absent");
        let refused = create_unmade(
            &path,
            MIB,
            CellOf::Taken(absent),
            cell::Kind::Fxfs,
            Some(&url),
            keeping().unwrap(),
        )
        .await;
        let Err(Failure::Bucket(refused)) = refused.map(drop) else { panic!("made") };
        assert_eq!(refused, bucket_cell::Error::NoCell);
        assert!(
            !host_fs::may_block(|| host_fs::exists(&path)),
            "a cell directory of no cell was left"
        );
    }

    /// TASK-165.6: a cell made on a host of cladefs's system names the host's bucket; one made
    /// elsewhere, a file bucket of its own, beside its directory, where its cell is kept: its
    /// forks' too, which name it. Beside it, not in it, so that it outlives the cell, its forks'
    /// cells living on (TASK-165.7).
    #[tokio::test(flavor = "multi_thread")]
    async fn cells_name_their_hosts_bucket_or_one_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        host_fs::may_block(|| {
            host_fs::create_dir(&path).unwrap();
            let host = Some("gs://bucket/prefix".to_owned());
            assert_eq!(bucket_named(host, &path).unwrap(), "gs://bucket/prefix");
            let own = host_fs::canonicalize(&dir.path()).unwrap().join("store.bucket");
            let url = format!("file://{}/", own.display());
            assert_eq!(bucket_named(None, &path).unwrap(), url);
        });

        // Where no host's bucket is, as in this test's process.
        let (made, fork) = (dir.path().join("made"), dir.path().join("fork"));
        let Opened { local, bucket, uploader } =
            create(&made, MIB, new(), cell::Kind::Fxfs, keeping().unwrap()).await.unwrap();
        let forked = new_cell(&bucket, local.description().cell.tenant).await.unwrap();
        let child = Child { cell: forked, owner: 1 };
        let version = bucket_cell::layout::NewVersion { id: 1, keep_last: None };
        uploader.uploaded(local.commit_version(version, false).await.unwrap()).await.unwrap();
        fork_through(&local, &uploader, version.id, &fork, child, |_| Ok(())).await.unwrap();
        let own = host_fs::may_block(|| {
            let own = host_fs::canonicalize(&dir.path()).unwrap().join("made.bucket");
            let url = format!("file://{}/", own.display());
            for named in [&made, &fork] {
                assert_eq!(local_cell::host_bucket(named).unwrap(), Some(url.clone()));
            }
            assert!(keeps_own_bucket(&local), "a cell directory's own bucket is a host's");
            // The bucket is this process's one of its URL, whose objects are in its directory.
            assert!(Arc::ptr_eq(&bucket, &self::bucket(&url).unwrap()));
            // cladefs: under its cell's prefix (TASK-364).
            let cell = own.join(local.description().cell.prefix());
            assert!(host_fs::exists(&cell.join("head")), "{:?}", host_fs::read_dir(&cell).unwrap());
            own
        });
        // The cell deleted, its fork's cell lives on in it.
        drop(uploader);
        drop(local);
        delete(&made, Some(&bucket), || Ok(1)).await.unwrap();
        assert!(host_fs::may_block(|| !host_fs::exists(&made) && host_fs::exists(&own)));
        let record = futures::executor::block_on(bucket_cell::head::get(&*bucket, forked));
        assert_eq!(record.unwrap().map(|record| record.deleted()), Some(false));
    }

    /// TASK-165.6: a cell made by older code, of an older format, is refused by its format,
    /// opened or deleted: nothing reads it.
    #[tokio::test(flavor = "multi_thread")]
    async fn older_cells_are_refused_by_their_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        drop(create(&path, MIB, new(), cell::Kind::Fxfs, keeping().unwrap()).await.unwrap());
        host_fs::may_block(|| {
            let description = path.join("store");
            let text = host_fs::read_to_string(&description).unwrap();
            let older: String = std::iter::once("cladefs extent store 5")
                .chain(text.lines().skip(1).filter(|line| !line.starts_with("bucket ")))
                .map(|line| format!("{line}\n"))
                .collect();
            host_fs::write(&description, older).unwrap();
        });

        let format = local_cell::Format { found: "cladefs extent store 5".to_owned() };
        let refused = open(&path, keeping().unwrap()).await.err().unwrap();
        let Failure::Local(refused) = refused else { panic!("{refused}") };
        assert_eq!(refused.downcast_ref::<local_cell::Format>(), Some(&format), "{refused:#}");
        let refused = crate::OpenCell::open(&path).await.err().unwrap();
        assert_eq!(refused.errno(), Some(Errno::ENOTSUP), "{refused:#}");
        let refused = crate::OpenCell::delete(&path, None).await.err().unwrap();
        assert_eq!(refused.errno(), Some(Errno::ENOTSUP), "{refused:#}");
        assert!(host_fs::may_block(|| host_fs::exists(&path.join("store"))));
    }

    /// A cell directory whose deletion stopped once its description was gone, or whose making
    /// stopped before it began, empty, is removed by deleting it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_an_emptied_cell_finishes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fork");
        host_fs::may_block(|| {
            host_fs::create_dir(&path).unwrap();
            assert!(unmade(&path));
        });
        delete(&path, None, token).await.unwrap();
        assert!(!host_fs::may_block(|| host_fs::exists(&path)));
    }

    /// TASK-79.20: a cell whose description was never put in place, its host crashing as its
    /// making, or a fork's, wrote what goes before it, is no cell, and is removed by deleting it,
    /// as cladefsd and cladefsctl delete it.
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_cell_never_described_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fork");
        host_fs::may_block(|| {
            host_fs::create_dir_all(path.join("segments")).unwrap();
            host_fs::write(path.join("ancestry"), "cladefs cell records 1\n3 /elsewhere\n")
                .unwrap();
            assert!(unmade(&path));
        });
        delete(&path, None, token).await.unwrap();
        assert!(!host_fs::may_block(|| host_fs::exists(&path)));
    }
}
