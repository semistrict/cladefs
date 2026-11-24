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
pub(crate) const BLOCK_SIZE: u32 = 4096;

/// The default tenant (TASK-364), of the cells made for no tenant of the host's: the cladefs
/// CLI's images, an ungranted caller's cells, public bases, tests'. Its cells with writers alone
/// fork into other tenants' cells ([`may_fork_into`], TASK-395.9).
pub(crate) const DEFAULT_TENANT: u64 = bucket_cell::layout::DEFAULT_TENANT;

/// cladefs: whether a fork of `parent` by `uploader`, its uploader, may make a new cell of
/// `tenant`'s (TASK-395.9): `EACCES` if `parent` has a writer and `cell` is another tenant's than
/// its own, unless its own is the default tenant's ([`DEFAULT_TENANT`]), as the daemon's own cells
/// and public bases are. Inheritance across tenants is the default tenant's and shared versions'
/// alone: a fork of a version is checked at the read of the head naming the version its child's
/// making makes instead (`lifecycle::finish_point_fork`, TASK-395.7).
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
/// other cell of the bucket has, nor will (`bucket_cell::objectid`), so that cells made apart,
/// and the forks of one, mint extent IDs of their own (backlog doc-4), which a host's cache of
/// extents keys them by (TASK-364). A cell's ID is its cell's: no caller names a new one.
pub(crate) async fn new_cell(bucket: &RoutedBucket, tenant: u64) -> Result<CellId, Failure> {
    let id = bucket.new_objectid().await?;
    Ok(CellId { tenant, id })
}

/// A new cell of `tenant`'s, of the bucket at `url`, as [`new_cell`] gives it: for a cell whose
/// object is made before its cell ([`CellOf::Named`]), as the FUSE mount's are. It blocks.
#[cfg(target_os = "linux")]
pub(crate) async fn new_cell_at(url: &str, tenant: u64) -> Result<CellId, Failure> {
    new_cell(&*bucket(url)?, tenant).await
}

/// A cell of a new cell of the default tenant's, of a random ID no allocator gave: for tests
/// that write a cell's objects themselves, as no cell does, or name a fork's child
/// (`OpenCell::fork_to`).
#[cfg(test)]
pub(crate) fn new_alone() -> CellId {
    named(DEFAULT_TENANT)
}

/// A new cell of `tenant`'s, of a random ID no allocator gives (`bucket_cell::objectid::named`).
#[cfg(test)]
pub(crate) fn named(tenant: u64) -> CellId {
    CellId { tenant, id: bucket_cell::objectid::named() }
}

/// A child of a fork of `parent`, of a random ID no allocator gave, for tests that name it
/// (`OpenCell::fork_to`): of a new cell of its tenant's. Random, not next to its parent's: an
/// allocator gives the IDs of its block in order, the next to another cell.
#[cfg(test)]
pub(crate) fn fork_child(parent: CellId) -> CellId {
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

node_local::node_local! {
    /// The buckets this process keeps cells in, by their URLs: one each, which every cell
    /// naming it shares, so that a memory bucket (`memory:///NAME`) is one for the process, as a
    /// bucket elsewhere is one for every process.
    static BUCKETS: Mutex<BTreeMap<String, Arc<RoutedBucket>>> = Mutex::new(BTreeMap::new());
}

/// The bucket at `url` ([`ObjectBucket::from_url`]): this process's, made if it has none yet.
pub(crate) fn bucket(url: &str) -> anyhow::Result<Arc<RoutedBucket>> {
    bucket_opened(url, ObjectBucket::from_url)
}

/// cladefs: the bucket numbered `number` at `url`, as [`bucket`] has it: opened with that bucket's
/// own options, its credentials (`CLADEFSD_BUCKET_<number>_*`,
/// [`bucket_cell::object_bucket::numbered_options`]), unless this process opened it already
/// (board 5550).
pub(crate) fn bucket_of(url: &str, number: u16) -> anyhow::Result<Arc<RoutedBucket>> {
    bucket_opened(url, |url| ObjectBucket::from_url_numbered(url, number))
}

/// This process's bucket at `url`, `open` opening it if it has none yet.
fn bucket_opened(
    url: &str,
    open: impl FnOnce(&str) -> Result<ObjectBucket, String>,
) -> anyhow::Result<Arc<RoutedBucket>> {
    let mut buckets = BUCKETS.get().lock().unwrap();
    if let Some(bucket) = buckets.get(url) {
        return Ok(bucket.clone());
    }
    // Its requests of other buckets' cells go there, by the registry of buckets (TASK-633.4).
    let resolve = Arc::new(bucket_cell::object_bucket::Registered::from_env());
    let bucket = open(url).map_err(anyhow::Error::msg)?.routed(resolve);
    let bucket = Arc::new(bucket);
    buckets.insert(url.to_owned(), bucket.clone());
    Ok(bucket)
}

/// cladefs: the bucket numbered `number`, as the host whose bucket is at `home` reaches it
/// (TASK-633.3, backlog doc-23): `home` itself, if of that number; else the bucket its requests of
/// that number's cells go to ([`RoutedBucket::numbered`]): the registry in `home`'s root, or the
/// root bucket's, records it; opened with its own options, and refused unless its identity is the
/// one recorded (`Error::Mismatched`). `NoBucket` if no registry records it.
pub(crate) async fn bucket_numbered(home: &str, number: u16) -> Result<Arc<ObjectBucket>, Failure> {
    let ours = bucket(home)?;
    let identity = bucket_cell::identity::get(&*ours).await?;
    if identity.is_some_and(|identity| identity.number == number) {
        return Ok(ours.own().clone());
    }
    Ok(ours.numbered(number).await?)
}

/// cladefs: the URLs of the buckets this process opened, for tests counting them (board 8443).
#[cfg(test)]
pub(crate) fn buckets_opened() -> Vec<String> {
    BUCKETS.get().lock().unwrap().keys().cloned().collect()
}

/// cladefs: what the decoded states of the cells this process's buckets keep, or its cells hold
/// as the maps of the extents they share, hold: each once, however many cells share it
/// (`bucket_cell::recovery::Decoded::held`, TASK-453).
#[cfg(any(target_os = "linux", madsim, test))]
pub(crate) fn decoded_held() -> bucket_cell::recovery::Held {
    let buckets = BUCKETS.get().lock().unwrap();
    (buckets.values())
        .filter_map(|bucket| bucket_cell::Bucket::decoded(&**bucket))
        .map(bucket_cell::recovery::Decoded::held)
        .fold(bucket_cell::recovery::Held::default(), std::ops::Add::add)
}

/// Makes `bucket` this process's of `url`, before any cell naming it opens, as the tests do the
/// buckets they put themselves in front of, and the simulation the bucket its hosts share; unless
/// it is already, the same.
#[cfg(any(test, madsim))]
pub(crate) fn use_bucket(url: &str, bucket: Arc<RoutedBucket>) {
    let mut buckets = BUCKETS.get().lock().unwrap();
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
pub(crate) fn use_host_bucket(url: String) {
    *HOST_BUCKET.lock().unwrap() = Some(url);
}

/// The URL of the bucket a cell made at `path`, a directory, keeps its cells in: this host's,
/// if it is a host of cladefs's system ([`host_bucket_url`], as [`crate::bucket::Host`] has
/// it); else a file bucket of the cell's own ([`bucket_named`]).
pub(crate) fn bucket_for(path: &Path) -> anyhow::Result<String> {
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
pub(crate) fn keeps_own_bucket(local: &LocalCell<HostDir>) -> bool {
    let url = local.bucket().unwrap_or_default();
    url.starts_with("file:") && url.trim_end_matches('/').ends_with(OWN_BUCKET)
}

/// The fencing token copies of cells never placed take their cells with where this process
/// opens them as no host's daemon does, as [`crate::Storage`] does: this host's placement's, if it
/// is a host of cladefs's system (`CLADEFSD_TOKEN`, [`crate::bucket::host_token`]); else the
/// first placement's. So the daemon and the cladefs CLI on one host take each other's cells.
pub(crate) fn token() -> Result<u64, Failure> {
    match node_local::env::var_os("CLADEFSD_TOKEN").filter(|token| !token.is_empty()) {
        Some(_) => crate::bucket::host_token(),
        None => Ok(FIRST_PLACEMENT),
    }
}

/// How a cell this process opens where no host's daemon serves it keeps its cell in its bucket:
/// its uploader's options, as a copy never placed takes its cell's head here ([`token`]).
pub(crate) fn keeping() -> Result<upload::Options, Failure> {
    let fetching = Some(crate::bucket::fetching());
    Ok(upload::Options { token: token()?, fetching, ..Default::default() })
}

/// A cell directory opened ([`open`]), with the bucket its description names, which its cell is
/// kept in, and its uploader, which took the cell's head, or holds its version (TASK-165.9).
pub(crate) struct Opened {
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

/// The bucket the cell directory at `path` names, if its description is there: as deleting it, or
/// what of it a deletion that stopped left, goes by. A cell of another format is refused
/// (`local_cell::Format`), and a description there but unread, or not whole: taken for none, its
/// cell would be deleted in no bucket, or the daemon's, and left in its own (board 6908).
pub(crate) fn bucket_at(path: &Path) -> anyhow::Result<Option<Arc<RoutedBucket>>> {
    match local_cell::host_bucket(path) {
        Ok(Some(url)) => Ok(Some(bucket(&url)?)),
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

/// The options cells are opened with: their extents hold at most what the guest's device says
/// one does (doc-4's `max_extent`).
pub(crate) fn options() -> Options {
    Options { max_extent: crate::OpenCell::MAX_EXTENT, ..Options::default() }
}

/// Makes a new cell at `path`, which must not exist, holding at most `capacity` bytes, of the
/// cell `of` says, kept in this host's bucket, or a file bucket of its own ([`bucket_for`]), its
/// cell's, of `kind` (doc-20), and opens it, its uploader as `keeping` says ([`open`]): whole, as
/// made. Its directory entry is as durable as
/// it is. A cell that is not made whole is removed; what is at `path` already is left as it is.
pub(crate) async fn create(
    path: &Path,
    capacity: u64,
    of: CellOf,
    kind: cell::Kind,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    let opened = create_unmade(path, capacity, of, kind, None, keeping).await?;
    if let Err(err) = upload::blocking(|| opened.made()) {
        opened.close().await;
        // Its making's last step failing may have left it looking made (`UNMADE` gone, not
        // durably): no store whose making failed is left behind (board 5668).
        upload::blocking(|| discard(path));
        return Err(err.into());
    }
    Ok(opened)
}

/// Makes a new cell at `path` as [`create`] does, kept in the bucket at `bucket`, if given, or
/// else at [`bucket_for`]'s; unmade until its maker says it is whole ([`Opened::made`]), as an
/// import does once filled (TASK-196): opened before, it refuses (`local_cell::Unmade`), and
/// [`unmade`] says it is no cell.
pub(crate) async fn create_unmade(
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
        let bucket = self::bucket(&url)?;
        let (cell, initial_head) = match of {
            CellOf::New(tenant) => {
                let cell = new_cell(&bucket, tenant).await?;
                make_keys(&bucket, cell).await?;
                (cell, None)
            }
            CellOf::Named(cell) => match learn_taken(&bucket, cell).await? {
                Some(read) => (cell, Some(read)),
                None => {
                    make_keys(&bucket, cell).await?;
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
            local_cell::create_host(path, description, &url, kind)?;
            let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
            let synced = host_fs::File::open(parent.unwrap_or(Path::new(".")))
                .and_then(|parent| parent.sync_all());
            synced.map_err(|err| Failure::local(err, "syncing a cell directory's parent"))?;
            Ok(LocalCell::recover_unmade(host_dir(path)?)?)
        })?;
        opened_with_head(recovering, keeping, initial_head).await
    };
    // Leave nothing half made behind.
    let made = made.await;
    if made.is_err() {
        upload::blocking(|| discard(path));
    }
    made
}

/// A new cell made here, a root, its KEK from the host's pool, before its cell first opens
/// (TASK-515): a tenant the KMS refuses gets nothing made (TASK-411.28); its first taking gives it
/// its DEK and kind.
async fn make_keys(bucket: &RoutedBucket, cell: CellId) -> Result<(), Failure> {
    let kms = crate::bucket::kms()?;
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

/// Opens the cell directory at `path` as of its last commit, with the bucket it names: its
/// uploader, as `keeping` says, takes its cell's head, with the token its placement names or
/// `keeping`'s, and recovers it, the cell's write-ahead log put after its end
/// (`local_cell::upload::open`, TASK-165.9). Fails as that does: a log another writer's commits
/// moved past (`ESTALE`), a cell deleted (`ENOENT`) or a later placement's (`ESTALE`); and a cell
/// never made whole (`Unmade`).
pub(crate) async fn open(path: &Path, keeping: upload::Options) -> Result<Opened, Failure> {
    // cladefs: a close of it in this process waited for first, holding no thread (doc-24).
    closed(path).await?;
    let recovering =
        upload::blocking(|| -> Result<_, Failure> { Ok(LocalCell::recover(host_dir(path)?)?) })?;
    opened(recovering, keeping).await
}

/// cladefs: returns once no cell of the directory at `path` is closing in this process
/// (`local_cell::closed`), whose opening then finds none: a waiting that gave up, as the opening
/// would have (`WouldBlock`, which the daemon says `EBUSY`).
async fn closed(path: &Path) -> Result<(), Failure> {
    let waited = local_cell::closed(path).await;
    waited.map_err(|err| Failure::local(err, format!("{}", path.display())))
}

/// The host directory of the cell at `path`.
fn host_dir(path: &Path) -> Result<HostDir, Failure> {
    HostDir::new(path).map_err(|err| Failure::local(err, format!("{}", path.display())))
}

/// Opens the cell directory at `path` as [`open`] does, its write-ahead log dropped if another
/// writer's commits moved its cell past it (`bucket_cell::Error::Diverged`): what `cladefsctl take`
/// does of a cell that refuses to open so, the commits the log held, never acknowledged, lost
/// (doc-14, Opening a cell; doc-7, Recovery on the same host).
pub(crate) async fn take(path: &Path, keeping: upload::Options) -> Result<Opened, Failure> {
    match open(path, keeping.clone()).await {
        Err(Failure::Bucket(bucket_cell::Error::Diverged)) => {
            let recovering = upload::blocking(|| -> Result<_, Failure> {
                let mut recovering = LocalCell::recover(host_dir(path)?)?;
                log::warn!(
                    "cladefs: dropping {}'s write-ahead log: another writer moved cell {} past it",
                    path.display(),
                    recovering.cell()
                );
                recovering.drop_wal()?;
                Ok(recovering)
            })?;
            opened(recovering, keeping).await
        }
        opened => opened,
    }
}

/// The cell `recovering` recovered, opened with its uploader as `keeping` says.
async fn opened(
    recovering: Recovering<HostDir>,
    keeping: upload::Options,
) -> Result<Opened, Failure> {
    opened_with_head(recovering, keeping, None).await
}

/// Like [`opened`], passing an already-fetched [`bucket_cell::head::Read`] to the uploader.
async fn opened_with_head(
    recovering: Recovering<HostDir>,
    keeping: upload::Options,
    initial_head: Option<bucket_cell::head::Read>,
) -> Result<Opened, Failure> {
    let url = recovering.bucket().map(str::to_owned);
    let path = || recovering_path(&recovering);
    let url = url.ok_or_else(|| Failure::msg(format!("{} names no bucket", path())))?;
    let bucket = bucket(&url).with_context(|| format!("opening the bucket {url}"))?;
    let runtime = crate::bucket::runtime()?;
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

/// Whether `path` is a directory a cell's making, or a fork's, left unmade, its host crashing:
/// no cell, as its description, made last, is not there, or it was never made whole
/// (`local_cell::unmade_host`, TASK-196); deleting it finishes it ([`delete`]).
pub(crate) fn unmade(path: &Path) -> bool {
    local_cell::unmade_host(path)
}

/// Removes what a making that failed left at `path`, if anything, as a cell directory goes
/// (`local_cell::remove_cell_dir`): logged if it can't, as the failure the caller returns is
/// the making's, and no cell from then on in this process, which a making there again removes
/// first ([`unmade`], `local_cell::left_unmade`, board 10506).
pub(crate) fn discard(path: &Path) {
    if host_fs::exists(&path)
        && let Err(err) = local_cell::remove_cell_dir(path)
    {
        log::warn!("cladefs: removing {}, which a making left: {err:#}", path.display());
        local_cell::left_unmade(path);
    }
}

/// Forks `local`, a cell at a version (`uploader.is_at_version`), to `child`, a new cell holding
/// what the version holds, whose cell is a new directory at `path`, which [`open`] opens:
/// `EEXIST` if anything is at `path`. The fork is the child's record alone (doc-7, Forks;
/// `Uploader::forked`), inheriting the version's extents rather than copies, which fails with
/// `ENOENT` once the version is dropped, and with `EACCES` if the child is of another tenant's
/// cell than the version, which isn't shared (TASK-395.7). Only then is the child's cell made,
/// at `path`, holding nothing of its own: opening it recovers the child's state from the bucket,
/// as any cell's opening does (TASK-165.9). `prepare` writes what the caller keeps beside the
/// cell into its making first, before its description, which makes it a cell, durable with it
/// ([`Making`], board 7420). A fork to
/// another tenant's cell fails with `EACCES` before anything is made, unless the cell is the
/// default tenant's ([`may_fork_into`], TASK-395.9).
///
/// The caller names the child: a fork to a child whose ID another cell has in the bucket fails
/// with `EEXIST`, making nothing (TASK-79.82). A fork that fails once its record was made is
/// deleted in the bucket as the fork's alone (`Uploader::unfork`); its host crashing, it stands,
/// or never was, as the bucket has it.
///
/// cladefs: a fork of an encrypted cell into another cell, as every fork here is, gives its child
/// the keys it inherits, rewrapped under its cell's KEK, from the pool, no KMS call made
/// (`Remote::inheriting`, `keys::give_inherited`, TASK-411.4): refused before anything is made,
/// `EACCES` if the child's tenant has no KEK in the KMS (TASK-411.28), or the host heard a tenant
/// whose keys it inherits, or the child's, revoked or destroyed.
pub(crate) async fn fork(
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
) -> Result<(), Error> {
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
    let made = match uploader.forked(child, inheriting).await {
        Err(err) => Err(failed(err)),
        Ok(()) => upload::blocking(|| -> anyhow::Result<()> {
            failpoint!("fork::recorded");
            make_child(local, path, cell, prepare)
                .with_context(|| format!("forking to cell {cell}: making its cell directory"))?;
            failpoint!("fork::made");
            Ok(())
        }),
    };
    let Err(err) = made else { return Ok(()) };
    // Leave nothing half made behind: no cell, and no fork whose cell went.
    upload::blocking(|| discard(path));
    if let Err(why) = uploader.unfork(child).await {
        log::warn!("cladefs: leaving cell {cell}, whose fork failed: {why}");
    }
    Err(Error::new(err))
}

node_local::node_local! {
    /// cladefs: the cells this process made versions of, by the version (cell and ID): what a cell
    /// at the version made here inherits of what cladefs keeps beside its cell, what is attached to
    /// it ([`cell_dir::inherit_attached`]), as a fork of the cell does. In memory alone: a restore
    /// on another host, or after the process went, inherits none. cladefs: of versions their cells
    /// name, made here or uploaded, alone: one dropped, by a drop or a later version's window, or
    /// whose cell was deleted, goes (TASK-610).
    static VERSIONS_MADE: Mutex<BTreeMap<(CellId, u64), PathBuf>> = Mutex::new(BTreeMap::new());
}

/// cladefs: the cell directory at `path`, of `cell`, made the version `version` here, `cell`'s
/// versions now those of `named`, and it: those of `cell`'s it names no more, dropped since, go.
pub(crate) fn made_version(cell: CellId, version: u64, path: &Path, named: &BTreeSet<u64>) {
    let mut made = VERSIONS_MADE.get().lock().unwrap();
    made.retain(|&(of, id), _| of != cell || named.contains(&id));
    made.insert((cell, version), path.to_owned());
}

/// cladefs: the versions `ids` of `cell` were dropped.
pub(crate) fn dropped_versions(cell: CellId, ids: &BTreeSet<u64>) {
    VERSIONS_MADE.get().lock().unwrap().retain(|&(of, id), _| of != cell || !ids.contains(&id));
}

/// cladefs: how many versions it says this process made, which the tests bound.
#[cfg(test)]
pub(crate) fn versions_made() -> usize {
    VERSIONS_MADE.get().lock().unwrap().len()
}

/// cladefs: where the cell this process made the version `version` of `cell` of is, if it did.
#[cfg(any(target_os = "linux", madsim))]
pub(crate) fn version_made_of(cell: CellId, version: u64) -> Option<PathBuf> {
    VERSIONS_MADE.get().lock().unwrap().get(&(cell, version)).cloned()
}

/// cladefs: where a cell at the version `version` of `cell` is kept for forks of it, served by
/// `cache`, the host's, if it has one: a directory of its own, among the cache's cells at versions,
/// on its filesystem, as a cell elsewhere than the cache is refused (board 6589,
/// [`Cache::versions`]); without, in the process's temporary directory. Such a cell holds nothing
/// of its own.
pub(crate) fn version_dir(cell: CellId, version: u64, cache: Option<&Cache>) -> PathBuf {
    let root = match cache {
        Some(cache) => cache.versions().to_owned(),
        None => std::env::temp_dir().join(format!("cladefs-versions-{}", std::process::id())),
    };
    root.join(format!("{:x}.{:x}.{:x}", cell.tenant, cell.id, version))
}

/// Forks `local`, its cell's version `version` made and uploaded, to `child` as [`fork`] does:
/// from a cell at the version ([`version_dir`]), made, then deleted, and the version dropped, the
/// child keeping what it inherits by its reference. So a cell with a writer forks: forks are of
/// versions alone (TASK-508.3).
pub(crate) async fn fork_through(
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    version: u64,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
) -> Result<(), Error> {
    let at = version_dir(local.description().cell, version, uploader.options().cache.as_deref());
    let (kind, url) = (local.kind().ok_or(Errno::EINVAL)?, local.bucket().ok_or(Errno::EINVAL)?);
    let made = upload::blocking(|| -> Result<(), Error> {
        let made = at.parent().map_or(Ok(()), host_fs::create_dir_all);
        made.and_then(|()| host_fs::create_dir(&at)).map_err(|err| {
            Error::new(anyhow::Error::new(err).context(format!("making {}", at.display())))
        })?;
        local_cell::create_host_at_version(&at, local.description(), url, kind, version)
            .and_then(|()| local_cell::made_host(&at))
            .map_err(Error::new)
    });
    // The cell at the version closed before it goes, as it held its directory.
    let forked = match made {
        Err(err) => Err(err),
        Ok(()) => match open(&at, uploader.options().clone()).await {
            Err(failure) => Err(Error::from(failure)),
            Ok(opened) => {
                let forked = fork(&opened.local, &opened.uploader, path, child, prepare).await;
                opened.close().await;
                forked
            }
        },
    };
    upload::blocking(|| discard(&at));
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

/// cladefs: forks `local`, at a version, its uploader `uploader`, to `child`, an ephemeral cell
/// kept on this host alone, whose cell is made at `path` (doc-25, TASK-698.3): noted in the
/// host's journal first ([`crate::ephemeral_host`]), then its reference written in the version's
/// cell's bucket, whose head must name the version still (`Uploader::forked_ephemeral`), then its
/// cell made, `prepare` writing what the caller keeps beside it first, its local checkpoint its
/// version's state with its keys. A fork that fails leaves nothing: its directory gone, its
/// reference retired, its note forgotten. `ENOTSUP` on a host keeping no ephemeral cells
/// (`CLADEFSD_EPHEMERAL`); `EINVAL` of a cell not at a version: ephemeral cells are forked from
/// versions alone.
pub(crate) async fn fork_ephemeral(
    local: &LocalCell<HostDir>,
    uploader: &Uploader<HostDir>,
    path: &Path,
    child: Child,
    prepare: impl FnOnce(&mut Making<'_, HostDir>) -> anyhow::Result<()> + Send,
) -> Result<(), Error> {
    may_fork_into(local.description().cell, uploader, child.cell.tenant)?;
    let origin = uploader.at_version_origin().ok_or(Errno::EINVAL)?;
    let host = crate::ephemeral_host::host()?;
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
                local_cell::create_host_ephemeral(
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
                local_cell::made_host(path)?;
                Ok(())
            }),
        },
    };
    let Err(err) = made else { return Ok(()) };
    log::warn!("cladefs: forking ephemeral cell {cell} at {}: {err:#}", path.display());
    upload::blocking(|| discard(path));
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

/// cladefs: opens the ephemeral cell at `path` (doc-25, TASK-698.3), with `keeping`'s cache
/// and held parts, its parent's extents read in the bucket its description names: the cell, and
/// that bucket. No uploader: nothing of it goes to a bucket.
pub(crate) async fn open_ephemeral(
    path: &Path,
    keeping: &upload::Options,
) -> Result<
    (Arc<LocalCell<HostDir>>, Arc<RoutedBucket>, bucket_cell::keys::RingHold, upload::Forked),
    Failure,
> {
    closed(path).await?;
    let recovering =
        upload::blocking(|| -> Result<_, Failure> { Ok(LocalCell::recover(host_dir(path)?)?) })?;
    let url = recovering.bucket().map(str::to_owned);
    let url = url.ok_or_else(|| Failure::msg(format!("{} names no bucket", path.display())))?;
    let bucket = bucket(&url).with_context(|| format!("opening the bucket {url}"))?;
    let runtime = crate::bucket::runtime()?;
    let held = (keeping.cache.clone(), keeping.held.clone());
    let opened = upload::open_ephemeral(recovering, options(), bucket.clone(), &runtime, held);
    let (local, keys, forked) = opened.await?;
    Ok((local, bucket, keys, forked))
}

/// cladefs: whether the cell directory at `path` is an ephemeral cell's (doc-25, TASK-698.3).
pub(crate) fn is_ephemeral(path: &Path) -> bool {
    local_cell::host_ephemeral(path).unwrap_or(false)
}

/// The error of a fork to `cell`, whose ID another cell has in the bucket.
fn taken(cell: CellId) -> anyhow::Error {
    let refused = format!("forking to cell {cell}: another cell has its ID");
    anyhow::Error::new(Errno::EEXIST).context(refused)
}

/// Makes the cell directory of `cell`, a fork of `parent`'s cell the bucket has the record of, in
/// the directory at `path`, made empty: the cell directory, of the parent's capacity, naming the
/// parent's bucket, of the parent's kind, its own cell's, `prepare` writing into its making first,
/// whole (TASK-364, doc-20). It holds nothing of its own: opening it recovers the child from the
/// bucket, as any cell's opening does (TASK-165.9), and holds its cell, making it.
fn make_child(
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
    local_cell::create_host_with(path, description, &url, kind, |making| {
        prepare(making)?;
        durable_in_parent(making, path);
        Ok(())
    })?;
    local_cell::made_host(path)?;
    Ok(())
}

/// cladefs: has `making`, of the cell directory at `path`, make the directory's entry in its parent
/// durable with the rest of it (board 7420): a fork's, which its maker made.
fn durable_in_parent(making: &mut Making<'_, HostDir>, path: &Path) {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.unwrap_or(Path::new(".")).to_owned();
    making.also("the cell directory's parent", move || host_fs::File::open(&parent)?.sync_all());
}

/// Deletes the cell directory at `path`, which nothing has open: with `bucket`, its cell there
/// first, its forks' cells living on, as the placement its copy was placed at, or, never placed, as
/// the one `token` gives; then its directory. A deletion that stopped partway is finished by
/// deleting again (TASK-79.71), and a directory a cell's or a fork's making left unmade
/// ([`unmade`]) goes.
///
/// Fails with `ENOENT` if nothing is at `path`, and with `ESTALE` if a later placement of the
/// cell directory holds its cell in `bucket`.
pub(crate) async fn delete(
    path: &Path,
    bucket: Option<&RoutedBucket>,
    token: impl Fn() -> Result<u64, Failure>,
) -> Result<(), Error> {
    // cladefs: an ephemeral cell's reference retired, its note forgotten (doc-25, TASK-698.3).
    if upload::blocking(|| is_ephemeral(path)) {
        return delete_with(path, async |dir, cell| {
            let checkpoint = upload::blocking(|| local_cell::host_local_checkpoint(dir))?;
            let checkpoint = checkpoint
                .ok_or_else(|| Failure::msg(format!("{} has no checkpoint", dir.display())))?;
            let parent = checkpoint.parent();
            let origin = bucket_cell::layout::Origin {
                parent: parent.cell,
                at: parent.at,
                epoch: parent.epoch,
            };
            let host = crate::ephemeral_host::host().map_err(|errno| {
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
    delete_with(path, async |dir, cell| match bucket {
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

/// Deletes the cell directory at `path` as [`delete`] does, `elsewhere` deleting its cell where
/// else it is kept, given the cell directory and the cell: once the cell is found open nowhere
/// here, before it goes here (`local_cell::delete_host`), so that a deletion that stopped partway,
/// the host crashing or `elsewhere` failing, leaves the cell still, which deleting again finishes,
/// never a cell kept elsewhere alone. What a cell's making, or a fork's, left unmade, which is
/// no cell, goes, `elsewhere` called for nothing.
pub(crate) async fn delete_with(
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
    local_cell::delete_host(path, deleting).await.map_err(|err| {
        // Its errno, if deleting the cell elsewhere failed with one: `ESTALE` for a cell a
        // later placement holds.
        let failure = err.chain().find_map(|cause| cause.downcast_ref::<Failure>());
        match failure.and_then(crate::bucket::errno) {
            Some(errno) => Error::new(err.context(errno)),
            None => Error::new(err),
        }
    })?;
    // cladefs: what it made versions of is gone (TASK-610).
    VERSIONS_MADE.get().lock().unwrap().retain(|_, of| of != path);
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
        let refused = crate::Storage::open(&path).await.err().unwrap();
        assert_eq!(refused.errno(), Some(Errno::ENOTSUP), "{refused:#}");
        let refused = crate::Storage::delete(&path).await.err().unwrap();
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
