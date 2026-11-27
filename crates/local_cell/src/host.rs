// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Cells in directories of the host's, as cladefs's images and cladefsd's cells are: a directory
//! holds one cell (TASK-165.7). Forks, versions and deletions are the bucket's,
//! which the cell's description names: the cell makes nothing of them but the commit that names
//! a version ([`LocalCell::commit_version`]).

use crate::cell::{LocalCell, UNMADE};
use crate::dir::{Dir, File as _, HostDir};
use crate::layout::{CellFile, DESCRIPTION, Description};
use anyhow::{Context as _, Error};
use bucket_cell::cell::Kind;
use extent_frame::segment::CellId;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Explicit per-host state for `local_cell` (board 10962): owns the `bucket_cell::Host`, page
/// cache, directory locks/claims, disk footprints, uploader count, bucket Tokio runtime,
/// durability lag histogram, staged commit buffer budget, failed-making unmade set, unpacked
/// part cache, and deferred-durability table.
pub struct Host {
    bucket: Arc<bucket_cell::Host>,
    pages: OnceLock<Arc<crate::pages::PageCache>>,
    pub(crate) dirs: crate::dir::Locks,
    pub(crate) footprints: crate::footprint::State,
    pub(crate) uploaders: tokio::sync::watch::Sender<usize>,
    pub(crate) bucket_runtime: crate::upload::BucketRuntime,
    pub(crate) durability_lags: Mutex<crate::durability::Lags>,
    pub(crate) hot_budget: Arc<crate::hot_budget::HotBudget>,
    pub(crate) left_unmade: Mutex<HashSet<PathBuf>>,
    unpacked: Arc<crate::framed::Unpacked>,
    pub(crate) deferred: Mutex<HashMap<PathBuf, Arc<crate::deferral::Deferral>>>,
}

impl Host {
    /// Creates a fresh, isolated `local_cell` host context with its own `bucket_cell::Host`.
    pub fn new() -> Arc<Self> {
        Self::with_bucket(bucket_cell::Host::new())
    }

    /// Creates a fresh `local_cell` host context backed by `bucket`.
    pub fn with_bucket(bucket: Arc<bucket_cell::Host>) -> Arc<Self> {
        let unpacked = crate::framed::Unpacked::new(Arc::clone(bucket.parts()));
        Arc::new(Self {
            bucket,
            pages: OnceLock::new(),
            dirs: crate::dir::Locks::new(),
            footprints: crate::footprint::State::new(),
            uploaders: tokio::sync::watch::channel(0).0,
            bucket_runtime: crate::upload::BucketRuntime::new(),
            durability_lags: Mutex::new(crate::durability::Lags::new()),
            hot_budget: crate::hot_budget::HotBudget::new(),
            left_unmade: Mutex::new(HashSet::new()),
            unpacked,
            deferred: Mutex::new(HashMap::new()),
        })
    }

    /// The process's default `local_cell` host context outside `madsim`.
    #[cfg(not(madsim))]
    pub fn process() -> &'static Arc<Self> {
        // Outside the simulation: single default host context for standalone CLIs and tests.
        static PROCESS: OnceLock<Arc<Host>> = OnceLock::new();
        PROCESS.get_or_init(|| Self::with_bucket(Arc::clone(bucket_cell::Host::process())))
    }

    /// The underlying `bucket_cell::Host`.
    pub fn bucket(&self) -> &Arc<bucket_cell::Host> {
        &self.bucket
    }

    /// This host's shared cell page cache, created on first use.
    pub fn pages(&self) -> &Arc<crate::pages::PageCache> {
        self.pages.get_or_init(|| crate::pages::PageCache::new(crate::pages::Options::default()))
    }

    /// This host's unpacked-part cache (sharing `bucket().parts()`).
    pub fn unpacked(&self) -> &Arc<crate::framed::Unpacked> {
        &self.unpacked
    }

    /// How many cells' uploaders run on this host.
    pub fn uploaders(&self) -> tokio::sync::watch::Receiver<usize> {
        crate::upload::uploaders_on(self)
    }

    /// How long this host's cells' commits waited to be made durable.
    pub fn durability_lags(&self) -> crate::durability::Lags {
        crate::durability::durability_lags_on(self)
    }

    /// Total bytes held in staged commit buffers on this host.
    pub fn hot_bytes(&self) -> u64 {
        crate::hot_budget::hot_bytes_on(self)
    }

    /// This host's bucket Tokio runtime.
    pub fn bucket_runtime(&self) -> Result<&tokio::runtime::Runtime, crate::upload::Failure> {
        crate::upload::bucket_runtime_on(self)
    }

    /// Runs `future` on this host's bucket runtime.
    pub fn block_on<F: std::future::Future>(
        &self,
        future: F,
    ) -> Result<F::Output, crate::upload::Failure> {
        crate::upload::block_on_host(self, future)
    }

    /// Runs `future` on this host's bucket runtime, panicking if the runtime cannot be made.
    pub fn wait_on<F: std::future::Future>(&self, future: F) -> F::Output {
        crate::upload::wait_on_host(self, future)
    }
}

/// Makes a new cell at `path` on `host`, which must be a directory holding no cell, as
/// `description` describes.
pub fn create_host_on(
    host: &Arc<Host>,
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
) -> Result<(), Error> {
    create_host_with_on(host, path, description, bucket, kind, |_| Ok(()))
}

/// [`create_host_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn create_host(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
) -> Result<(), Error> {
    create_host_on(Host::process(), path, description, bucket, kind)
}

/// cladefs: makes a new cell at `path` on `host` as [`create_host_on`] does, `extra` writing what
/// else its directory holds into its making first, made durable with its description at once
/// ([`Making`](crate::Making), board 7420).
pub fn create_host_with_on(
    host: &Arc<Host>,
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    extra: impl FnOnce(&mut crate::Making<'_, HostDir>) -> Result<(), Error>,
) -> Result<(), Error> {
    LocalCell::make_with(
        &HostDir::new_on(host, path)?,
        description,
        Some(bucket),
        Some(kind),
        None,
        extra,
    )
}

/// [`create_host_with_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn create_host_with(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    extra: impl FnOnce(&mut crate::Making<'_, HostDir>) -> Result<(), Error>,
) -> Result<(), Error> {
    create_host_with_on(Host::process(), path, description, bucket, kind, extra)
}

/// cladefs: makes a new cell at `path` on `host` as [`create_host_on`] does, a cell at the version
/// `version` of its cell.
pub fn create_host_at_version_on(
    host: &Arc<Host>,
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    version: u64,
) -> Result<(), Error> {
    LocalCell::make_copy(
        &HostDir::new_on(host, path)?,
        description,
        Some(bucket),
        Some(kind),
        Some(version),
    )
}

/// [`create_host_at_version_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn create_host_at_version(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    version: u64,
) -> Result<(), Error> {
    create_host_at_version_on(Host::process(), path, description, bucket, kind, version)
}

/// cladefs: the cell at `path` on `host` is whole, as its maker made it ([`LocalCell::made`],
/// TASK-196): the file saying it is not goes, durably, its maker having let it go.
pub fn made_host_on(host: &Arc<Host>, path: &Path) -> Result<(), Error> {
    // cladefs: its durability deferred, its `UNMADE` file goes as it settles (board 7420).
    if crate::deferral::of_on(host, path).is_some_and(|deferral| deferral.mark_made()) {
        return Ok(());
    }
    let dir = HostDir::new_on(host, path)?;
    let made = match dir.remove(UNMADE) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    };
    made.and_then(|()| dir.sync_dir("")).with_context(|| format!("making {} whole", path.display()))
}

/// [`made_host_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn made_host(path: &Path) -> Result<(), Error> {
    made_host_on(Host::process(), path)
}

/// cladefs: the directory at `path`, which a making that failed on `host` left, its removal
/// failing too, is no cell on `host`, whatever it looks like on the disk ([`unmade_host_on`]),
/// until it goes ([`remove_cell_dir_on`]) (board 10506).
pub fn left_unmade_on(host: &Host, path: &Path) {
    host.left_unmade.lock().unwrap().insert(crate::deferral::key(path));
}

/// [`left_unmade_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn left_unmade(path: &Path) {
    left_unmade_on(Host::process(), path);
}

/// cladefs: whether `path` is a directory a cell's making left unmade on `host` (TASK-79.84,
/// TASK-196, board 10506).
pub fn unmade_host_on(host: &Host, path: &Path) -> bool {
    if host.left_unmade.lock().unwrap().contains(&crate::deferral::key(path)) {
        return host_fs::is_dir(path);
    }
    // cladefs: whole here, its durability deferred (board 7420).
    if crate::deferral::made_here_on(host, path) {
        return false;
    }
    host_fs::is_dir(path)
        && (!host_fs::exists(path.join(DESCRIPTION)) || host_fs::exists(path.join(UNMADE)))
}

/// [`unmade_host_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn unmade_host(path: &Path) -> bool {
    unmade_host_on(Host::process(), path)
}

/// cladefs: places the cell at `path` on `host` (TASK-39.7, TASK-681).
pub fn place_host_on(host: &Arc<Host>, path: &Path, token: u64, force: bool) -> Result<(), Error> {
    let placed =
        HostDir::new_on(host, path).map_err(Error::new).and_then(|dir| place(&dir, token, force));
    placed.with_context(|| format!("placing {}", path.display()))
}

/// [`place_host_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn place_host(path: &Path, token: u64, force: bool) -> Result<(), Error> {
    place_host_on(Host::process(), path, token, force)
}

/// cladefs: places the cell in `dir` at the placement whose fencing token is `token`, its takings
/// forced if `force`, as [`place_host`] places a host's cell: its description rewritten, durably.
pub fn place<D: Dir>(dir: &D, token: u64, force: bool) -> Result<(), Error> {
    let file = dir.open(DESCRIPTION, false).context("opening the description")?;
    let mut text = vec![0; file.size()? as usize];
    file.read_at(&mut text, 0).context("reading the description")?;
    let mut described = CellFile::parse(std::str::from_utf8(&text)?)?;
    // A forced taking's report stays its placement's, which placing again at its token is.
    if described.token != Some(token) {
        described.forced_after = None;
    }
    described.token = Some(token);
    described.forced = force.then_some(token);
    dir.replace(DESCRIPTION, described.text().as_bytes())?;
    dir.sync_dir("")?;
    Ok(())
}

/// cladefs: the fencing token of the placement of the cell at `path`, if it was placed
/// ([`place_host`]).
pub fn host_token(path: &Path) -> Result<Option<u64>, Error> {
    Ok(cell_file(path)?.token)
}

/// Deletes the cell at `path`, which no cell has open: `before` goes once that is found so, with
/// the cell, before its directory goes, to delete what keeps the cell elsewhere, as a
/// bucket does; then the directory, whole. A deletion that stopped partway is finished by deleting
/// again, which calls `before` again unless the directory's removal had begun. A directory with no
/// description is no cell, but what a cell's making left, or its removal: it goes, `before`
/// called for nothing. It fails while another process has the cell open, or this one.
pub async fn delete_host_on(
    host: &Arc<Host>,
    path: &Path,
    before: impl AsyncFnOnce(CellId) -> Result<(), Error>,
) -> Result<(), Error> {
    // cladefs: its disk work off the runtime's threads, `before` awaited between (TASK-685.3).
    use crate::upload::blocking;
    if !blocking(|| host_fs::exists(path.join(DESCRIPTION))) {
        return blocking(|| remove_cell_dir_on(host, path));
    }
    let (cell, dir) = blocking(|| -> Result<_, Error> {
        let cell = host_cell(path)?;
        let dir = HostDir::new_on(host, path)?;
        dir.take().context("taking the cell directory")?;
        Ok((cell, dir))
    })?;
    // cladefs: once no cell of the directory is closing on `host`, holding no thread, as an
    // opening awaits it before its claim, which then waits for none (board 2946).
    crate::closed_on(host, path).await.context("waiting for the cell's close")?;
    let removed = match blocking(|| dir.claim().context("taking the cell directory")) {
        Err(err) => Err(err),
        Ok(()) => {
            let removed = match before(cell).await {
                Ok(()) => blocking(|| -> Result<(), Error> {
                    failpoint!("delete::cell");
                    remove_cell_dir_on(host, path)
                }),
                Err(err) => Err(err),
            };
            dir.unclaim();
            removed
        }
    };
    dir.let_go();
    removed
}

/// [`delete_host_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub async fn delete_host(
    path: &Path,
    before: impl AsyncFnOnce(CellId) -> Result<(), Error>,
) -> Result<(), Error> {
    delete_host_on(Host::process(), path, before).await
}

/// The name a cell's description goes by as its directory goes ([`remove_cell_dir_on`]): its
/// [`DESCRIPTION`]'s, `.deleted` after it.
pub(crate) const DELETED: &str = "store.deleted";

/// Removes the directory of the cell at `cell_dir` on `host`, unless it went already: whole, or so
/// that removing it again finishes it (TASK-79.77, TASK-539.3).
pub fn remove_cell_dir_on(host: &Host, cell_dir: &Path) -> Result<(), Error> {
    // cladefs: its durability deferred no more: gone (board 7420).
    crate::deferral::forget_on(host, cell_dir);
    let removed = crate::footprint::removing_on(host, cell_dir, || remove_cell(cell_dir));
    // cladefs: what a failed making left gone, its last step failing or not, what is made there
    // next is what it is.
    if !host_fs::exists(cell_dir) {
        host.left_unmade.lock().unwrap().remove(&crate::deferral::key(cell_dir));
    }
    removed
}

/// [`remove_cell_dir_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn remove_cell_dir(cell_dir: &Path) -> Result<(), Error> {
    remove_cell_dir_on(Host::process(), cell_dir)
}

/// cladefs: whether the directory at `path` is a cell's going ([`remove_cell_dir`]): its
/// description renamed, the rest going, a removal stopped part way among them, which removing it
/// again finishes (board 5730).
pub fn deleting_host(path: &Path) -> bool {
    host_fs::exists(path.join(DELETED))
}

/// Removes the directory of the cell at `cell_dir`, as [`remove_cell_dir`] says.
fn remove_cell(cell_dir: &Path) -> Result<(), Error> {
    let name = DELETED;
    let deleted = cell_dir.join(name);
    let removing = || -> std::io::Result<()> {
        match removals::rename(&cell_dir.join(DESCRIPTION), &deleted) {
            Ok(()) => removals::sync_dir(cell_dir)?,
            // Going already, or no cell: a deletion that stopped, or a making that did.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let entries = match host_fs::read_dir(cell_dir) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            entries => Some(entries?),
        };
        for entry in entries.into_iter().flatten() {
            let entry = entry?;
            if entry.file_name() == *name {
                continue;
            }
            remove(match entry.file_type()?.is_dir() {
                true => removals::remove_dir_all(&entry.path()),
                false => removals::remove_file(&entry.path()),
            })?;
        }
        remove(removals::remove_file(&deleted))?;
        remove(removals::remove_dir(cell_dir))?;
        // Gone durably, the directory it was in synced.
        removals::sync_dir(parent(cell_dir))
    };
    removing().with_context(|| format!("removing {}", cell_dir.display()))
}

/// The directory `path` is in.
fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The changes a deletion makes to the host's file system as it removes a cell directory
/// ([`remove_cell_dir`]): std's, each a step, which a test records, and stops after a number of, as
/// a crash would stop them (TASK-79.77).
mod removals {
    use std::io;
    use std::path::Path;

    /// What a step changes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Change {
        /// A file renamed to the path.
        Renamed,
        /// The file or directory at the path removed.
        Removed,
        /// The directory at the path's entries made durable.
        Synced,
    }

    /// Renames the file at `from` to `to`.
    pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        step(Change::Renamed, to)?;
        host_fs::rename(from, to)
    }

    /// Removes the file at `path`.
    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        step(Change::Removed, path)?;
        host_fs::remove_file(path)
    }

    /// Removes the empty directory at `path`.
    pub(super) fn remove_dir(path: &Path) -> io::Result<()> {
        step(Change::Removed, path)?;
        host_fs::remove_dir(path)
    }

    /// Removes the directory at `path`, and all it holds.
    pub(super) fn remove_dir_all(path: &Path) -> io::Result<()> {
        step(Change::Removed, path)?;
        host_fs::remove_dir_all(path)
    }

    /// Makes the entries of the directory at `path` durable.
    pub(super) fn sync_dir(path: &Path) -> io::Result<()> {
        step(Change::Synced, path)?;
        host_fs::File::open(path)?.sync_all()
    }

    /// Goes on to the step that makes `change` at `path`: at once, but in a test.
    #[cfg(not(test))]
    fn step(_: Change, _: &Path) -> io::Result<()> {
        Ok(())
    }

    #[cfg(test)]
    use super::tests::step;
}

/// What removing something gave, but for its being gone already.
fn remove(removed: std::io::Result<()>) -> std::io::Result<()> {
    match removed {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// The ID of the cell at `path`, as its description names it.
pub fn host_cell(path: &Path) -> Result<CellId, Error> {
    Ok(cell_file(path)?.cell)
}

/// cladefs: the URL of the bucket the cell at `path` keeps its cell in, as its description names
/// it: none for a cell of memory's, which names none (TASK-165.6).
pub fn host_bucket(path: &Path) -> Result<Option<String>, Error> {
    Ok(cell_file(path)?.bucket)
}

/// cladefs: the version of its cell the cell at `path` is at, if it is one.
pub fn host_version(path: &Path) -> Result<Option<u64>, Error> {
    Ok(cell_file(path)?.version)
}

/// cladefs: whether the cell at `path` is an ephemeral cell's (doc-25, TASK-698.3), as its
/// description says.
pub fn host_ephemeral(path: &Path) -> Result<bool, Error> {
    Ok(cell_file(path)?.ephemeral)
}

/// cladefs: the kind of the cell at `path`, if its description names one (doc-20).
pub fn host_kind(path: &Path) -> Result<Option<Kind>, Error> {
    Ok(cell_file(path)?.kind)
}

/// cladefs: the local checkpoint of the ephemeral cell at `path` on `host`, if it is one (doc-25,
/// TASK-698.3): what deleting it reads its reference of.
pub fn host_local_checkpoint_on(
    host: &Arc<Host>,
    path: &Path,
) -> Result<Option<crate::LocalCheckpoint>, Error> {
    if !cell_file(path)?.ephemeral {
        return Ok(None);
    }
    crate::LocalCheckpoint::read(&HostDir::new_on(host, path)?).map(Some)
}

/// [`host_local_checkpoint_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn host_local_checkpoint(path: &Path) -> Result<Option<crate::LocalCheckpoint>, Error> {
    host_local_checkpoint_on(Host::process(), path)
}

/// cladefs: makes a new ephemeral cell at `path` on `host` (doc-25, TASK-698.3).
pub fn create_host_ephemeral_on(
    host: &Arc<Host>,
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    checkpoint: &crate::LocalCheckpoint,
    encrypting: impl bucket_cell::layout::KeyArg,
    extra: impl FnOnce(&mut crate::Making<'_, HostDir>) -> Result<(), Error>,
) -> Result<(), Error> {
    LocalCell::make_ephemeral(
        &HostDir::new_on(host, path)?,
        description,
        Some(bucket),
        Some(kind),
        checkpoint,
        encrypting,
        extra,
    )
}

/// [`create_host_ephemeral_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn create_host_ephemeral(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    checkpoint: &crate::LocalCheckpoint,
    encrypting: (&extent_frame::Secret, extent_frame::KeyId),
    extra: impl FnOnce(&mut crate::Making<'_, HostDir>) -> Result<(), Error>,
) -> Result<(), Error> {
    create_host_ephemeral_on(
        Host::process(),
        path,
        description,
        bucket,
        kind,
        checkpoint,
        encrypting,
        extra,
    )
}

/// The description of the cell whose directory is `cell_dir`.
fn cell_file(cell_dir: &Path) -> Result<CellFile, Error> {
    let text = host_fs::read_to_string(cell_dir.join(DESCRIPTION))
        .with_context(|| format!("reading {}'s description", cell_dir.display()))?;
    CellFile::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostDir, Options};
    use anyhow::anyhow;
    use removals::Change;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use storage_device::ExtentId;

    const BLOCK: usize = 4096;

    /// The bucket the tests' cells name, which nothing here reaches.
    const BUCKET: &str = "memory:///host";

    /// The steps of this thread's deletions' removals ([`removals`]): those taken, and how many
    /// more may be, if limited.
    #[derive(Default)]
    struct Steps {
        taken: Vec<(Change, PathBuf)>,
        left: Option<usize>,
    }

    thread_local! {
        static STEPS: RefCell<Steps> = RefCell::default();
    }

    /// Takes the step that makes `change` at `path`, recording it; or fails, as a crash would stop
    /// it, once the steps allowed are taken ([`stop_after`]).
    pub(super) fn step(change: Change, path: &Path) -> std::io::Result<()> {
        STEPS.with_borrow_mut(|steps| {
            match &mut steps.left {
                Some(0) => return Err(std::io::Error::other("stopped")),
                Some(left) => *left -= 1,
                None => {}
            }
            steps.taken.push((change, path.to_owned()));
            Ok(())
        })
    }

    /// Stops this thread's deletions' removals after `left` more steps, or never if `None`.
    fn stop_after(left: Option<usize>) {
        STEPS.with_borrow_mut(|steps| steps.left = left);
    }

    /// The steps this thread's deletions' removals took since this was last asked.
    fn taken() -> Vec<(Change, PathBuf)> {
        STEPS.with_borrow_mut(|steps| std::mem::take(&mut steps.taken))
    }

    /// The cell at `path`, opened where the bucket holds nothing of its cell.
    fn open(path: &Path) -> Result<LocalCell<HostDir>, Error> {
        host_fs::may_block(|| LocalCell::open(HostDir::new(path)?, Options::default()))
    }

    /// A cell at `cell` in `temp`, holding an extent: its path.
    async fn stored(temp: &Path) -> PathBuf {
        let path = temp.join("cell");
        let cell = host_fs::may_block(|| {
            host_fs::create_dir(&path).unwrap();
            let description = Description {
                block_size: BLOCK as u32,
                capacity: 1 << 20,
                cell: bucket_cell::layout::test_cell(3),
            };
            create_host(&path, description, BUCKET, bucket_cell::cell::Kind::Fxfs).unwrap();
            let cell = LocalCell::recover_unmade(HostDir::new(&path).unwrap()).unwrap();
            let cell = cell
                .open(Options::default(), crate::Opening::new(bucket_cell::layout::test_cell(3)))
                .unwrap();
            cell.made().unwrap();
            cell
        });
        cell.write(ExtentId::new(3, 0), 0, &[7; BLOCK]).await.unwrap();
        cell.commit(None).await.unwrap();
        path
    }

    /// A deletion that stopped partway, its description renamed and part of the rest gone, is
    /// finished by deleting again, which calls nothing to delete the cell elsewhere; and so is
    /// a directory a cell's making left before its description, holding files of no cell's.
    #[tokio::test(flavor = "multi_thread")]
    async fn stopped_deletions_finish() {
        let temp = tempfile::tempdir().unwrap();
        let path = stored(temp.path()).await;
        host_fs::may_block(|| {
            host_fs::rename(path.join(DESCRIPTION), path.join("store.deleted")).unwrap();
            host_fs::remove_dir_all(path.join(crate::layout::LOGS)).unwrap();
        });
        delete_host(&path, async |_| Err(anyhow!("called"))).await.unwrap();
        assert!(!host_fs::may_block(|| host_fs::exists(&path)));

        let unmade = temp.path().join("unmade");
        host_fs::may_block(|| {
            host_fs::create_dir_all(unmade.join("segments")).unwrap();
            host_fs::write(unmade.join("stray"), "").unwrap();
        });
        delete_host(&unmade, async |_| Err(anyhow!("called"))).await.unwrap();
        assert!(!host_fs::may_block(|| host_fs::exists(&unmade)));
    }

    /// A deletion calls `before` with the cell, while the cell is open nowhere, before
    /// its directory goes: one `before` stopped leaves the cell, which deleting again finishes.
    /// One of a cell open in this process fails, calling nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn deletions_delete_their_cell_elsewhere_first() {
        let temp = tempfile::tempdir().unwrap();
        let path = stored(temp.path()).await;
        let opened = open(&path).unwrap();
        let refused = delete_host(&path, async |_| Err(anyhow!("called"))).await;
        assert!(
            refused.is_err() && host_fs::may_block(|| host_fs::exists(&path.join(DESCRIPTION))),
            "{refused:?}"
        );
        drop(opened);
        let stopped = delete_host(&path, async |cell| Err(anyhow!("stopped at {cell}"))).await;
        assert_eq!(stopped.map_err(|err| err.to_string()), Err("stopped at 1/3".to_owned()));
        assert!(open(&path).is_ok(), "a cell stopped opens no more");
        let called = std::cell::Cell::new(None);
        delete_host(&path, async |cell| Ok(called.set(Some(cell)))).await.unwrap();
        assert_eq!(
            (called.get(), host_fs::may_block(|| host_fs::exists(&path))),
            (Some(bucket_cell::layout::test_cell(3)), false)
        );
    }

    /// TASK-79.77: a deletion stopped at any step is finished by deleting again, which leaves
    /// nothing of the cell: one stopped before the cell went elsewhere, and one stopped before
    /// each step of the removals that follow ([`removals`]), as a crash would stop them.
    #[tokio::test(flavor = "multi_thread")]
    async fn deletions_stopped_at_any_step_finish() {
        // Stopped where it was stopped, and not before, by anything else.
        let stopped = |deleted: Result<(), Error>, at: &str| {
            let err = deleted.expect_err(&format!("went on {at}"));
            assert_eq!(err.root_cause().to_string(), "stopped", "stopped {at}: {err:#}");
        };
        let finished = async |temp: &Path, path: &Path, at: &str| {
            delete_host(path, async |_| Ok(()))
                .await
                .unwrap_or_else(|err| panic!("stopped {at}: {err:#}"));
            let left: Vec<_> = host_fs::may_block(|| host_fs::read_dir(temp).unwrap().collect());
            assert_eq!(left.len(), 0, "stopped {at}: {left:?} left");
        };
        let at = "before the cell went";
        let temp = tempfile::tempdir().unwrap();
        let path = stored(temp.path()).await;
        stopped(delete_host(&path, async |_| Err(anyhow!("stopped"))).await, at);
        finished(temp.path(), &path, at).await;
        // The removals' steps, as a deletion stopped at none takes them.
        let temp = tempfile::tempdir().unwrap();
        let path = stored(temp.path()).await;
        taken();
        delete_host(&path, async |_| Ok(())).await.unwrap();
        for (stop, (change, at)) in taken().into_iter().enumerate() {
            let at = format!("before step {stop}, {change:?} {:?}", at.file_name().unwrap());
            let temp = tempfile::tempdir().unwrap();
            let path = stored(temp.path()).await;
            stop_after(Some(stop));
            let deleted = delete_host(&path, async |_| Ok(())).await;
            stop_after(None);
            stopped(deleted, &at);
            finished(temp.path(), &path, &at).await;
        }
    }

    /// TASK-79.77: a deletion renames the cell's description first, durably, so that the
    /// directory is no cell from then on, and removes the directory durably, the directory it
    /// was in synced, last.
    #[tokio::test(flavor = "multi_thread")]
    async fn deletions_remove_the_cell_directory_durably() {
        let temp = tempfile::tempdir().unwrap();
        let path = stored(temp.path()).await;
        let parent = super::parent(&path).to_owned();
        taken();
        delete_host(&path, async |_| Ok(())).await.unwrap();
        let steps = taken();
        let removed = |at: PathBuf| {
            steps.iter().position(|(change, step)| *change == Change::Removed && *step == at)
        };
        assert_eq!(steps[0], (Change::Renamed, path.join("store.deleted")));
        assert_eq!(steps[1], (Change::Synced, path.clone()));
        assert_eq!(
            &steps[steps.len() - 3..],
            [
                (Change::Removed, path.join("store.deleted")),
                (Change::Removed, path.clone()),
                (Change::Synced, parent.clone()),
            ]
        );
        let (segments, description) = (path.join("segments"), path.join("store.deleted"));
        assert!(removed(segments).unwrap() < removed(description).unwrap());
    }
}
