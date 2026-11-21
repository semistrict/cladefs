// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stores in directories of the host's, as cladefs's images and cladefsd's stores are: a directory
//! holds one store, of one cell (TASK-165.7). Forks, versions and deletions are the bucket's,
//! which the store's description names: the store makes nothing of them but the commit that names
//! a version ([`LocalCell::commit_version`]).

use crate::cell::{LocalCell, UNMADE};
use crate::dir::{Dir, File as _, HostDir};
use crate::layout::{CellFile, DESCRIPTION, Description};
use anyhow::{Context as _, Error};
use bucket_cell::cell::Kind;
use extent_frame::segment::CellId;
use std::path::Path;

/// Makes a new store at `path`, which must be a directory holding no store, as `description`
/// describes. cladefs: its description names `bucket`, the URL of the bucket its cell is kept in,
/// as every host's store's does (TASK-165.6); it is unmade until its maker says it is whole
/// ([`LocalCell::made`], TASK-196). Its description is made last: until it is durable, the directory
/// is no store, whatever else is in it. Opening it takes its cell in the bucket
/// ([`LocalCell::recover`], `crate::upload`), and its cell, of `kind`, whose cell it is
/// (TASK-364, doc-20).
pub fn create_host(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
) -> Result<(), Error> {
    LocalCell::make(&HostDir::new(path)?, description, Some(bucket), Some(kind))
}

/// cladefs: makes a new store at `path` as [`create_host`] does, a store at the version `version` of
/// its cell: the cell's state at the version, which nothing writes, and which forks fork
/// (doc-18, doc-20). Opening it reads the version from the bucket, once the cell's head names it.
pub fn create_host_at_version(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    version: u64,
) -> Result<(), Error> {
    LocalCell::make_copy(&HostDir::new(path)?, description, Some(bucket), Some(kind), Some(version))
}

/// cladefs: the store at `path` is whole, as its maker made it ([`LocalCell::made`], TASK-196): the file
/// saying it is not goes, durably, its maker having let it go.
pub fn made_host(path: &Path) -> Result<(), Error> {
    let dir = HostDir::new(path)?;
    let made = match dir.remove(UNMADE) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    };
    made.and_then(|()| dir.sync_dir("")).with_context(|| format!("making {} whole", path.display()))
}

/// cladefs: whether `path` is a directory a store's making left unmade, its maker stopping part
/// way: no store, which deleting finishes (TASK-79.84, TASK-196). Its description, made last of
/// its directory, is not there; or it says it was never made whole ([`LocalCell::made`]).
pub fn unmade_host(path: &Path) -> bool {
    host_fs::is_dir(path)
        && (!host_fs::exists(path.join(DESCRIPTION)) || host_fs::exists(path.join(UNMADE)))
}

/// cladefs: places the store at `path` (TASK-39.7): records, durably, in its description, the
/// fencing token its host takes its cell with from then on, the boot ID of the placement it
/// is, which a later placement's exceeds; its writer takes its cell's head with the epoch of
/// that taking of the cell. A placement takes the cell only with a token as high as the last
/// one's: a placement cut off takes no cell back from the one after it.
/// A store never placed takes it with its host's (TASK-165.9: the token is the description's).
/// cladefs: `force`d, this placement's takings of a replicated cell go on with the members of the
/// configuration its head names that answer, too few for a take (TASK-681): the commits told
/// replicated that only the others held are lost. A placement after, unforced, is not.
pub fn place_host(path: &Path, token: u64, force: bool) -> Result<(), Error> {
    let placed = HostDir::new(path).map_err(Error::new).and_then(|dir| place(&dir, token, force));
    placed.with_context(|| format!("placing {}", path.display()))
}

/// cladefs: places the store in `dir` at the placement whose fencing token is `token`, its takings
/// forced if `force`, as [`place_host`] places a host's store: its description rewritten, durably.
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

/// cladefs: the fencing token of the placement of the store at `path`, if it was placed
/// ([`place_host`]).
pub fn host_token(path: &Path) -> Result<Option<u64>, Error> {
    Ok(store_file(path)?.token)
}

/// Deletes the store at `path`, which no store has open: `before` goes once that is found so, with
/// the store's cell, before its directory goes, to delete what keeps the cell elsewhere, as a
/// bucket does; then the directory, whole. A deletion that stopped partway is finished by deleting
/// again, which calls `before` again unless the directory's removal had begun. A directory with no
/// description is no store, but what a store's making left, or its removal: it goes, `before`
/// called for nothing. It fails while another process has the store open, or this one.
pub async fn delete_host(
    path: &Path,
    before: impl AsyncFnOnce(CellId) -> Result<(), Error>,
) -> Result<(), Error> {
    // cladefs: its disk work off the runtime's threads, `before` awaited between (TASK-685.3).
    use crate::upload::blocking;
    if !host_fs::exists(path.join(DESCRIPTION)) {
        return blocking(|| remove_store_dir(path));
    }
    let (cell, dir) = blocking(|| -> Result<_, Error> {
        let cell = host_cell(path)?;
        let dir = HostDir::new(path)?;
        dir.take().context("taking the store's directory")?;
        Ok((cell, dir))
    })?;
    // cladefs: once no store of the directory is closing in the process, holding no thread, as an
    // opening awaits it before its claim, which then waits for none (board 2946).
    crate::closed(path).await.context("waiting for the store's close")?;
    let removed = match blocking(|| dir.claim().context("taking the store's directory")) {
        Err(err) => Err(err),
        Ok(()) => {
            let removed = match before(cell).await {
                Ok(()) => blocking(|| -> Result<(), Error> {
                    failpoint!("delete::cell");
                    remove_store_dir(path)
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

/// The name a store's description goes by as its directory goes ([`remove_store_dir`]): its
/// [`DESCRIPTION`]'s, `.deleted` after it.
pub(crate) const DELETED: &str = "store.deleted";

/// Removes the directory of the store at `store`, unless it went already: whole, or so that
/// removing it again finishes it. Its description goes first, renamed to [`DELETED`],
/// durably, so that the directory is no store from then on, but one going, which a deletion resumed
/// finds so; then the rest, then that, then the directory. Then the directory it was in is synced,
/// so that it is gone durably (TASK-79.77). cladefs: the one way a store's directory goes, whether
/// it held a store or what a making left of one, so that what its files took of the host's disk
/// goes with them (TASK-539.3); the store opened nowhere.
pub fn remove_store_dir(store: &Path) -> Result<(), Error> {
    crate::footprint::removing(store, || remove_store(store))
}

/// Removes the directory of the store at `store`, as [`remove_store_dir`] says.
fn remove_store(store: &Path) -> Result<(), Error> {
    let name = DELETED;
    let deleted = store.join(name);
    let removing = || -> std::io::Result<()> {
        match removals::rename(&store.join(DESCRIPTION), &deleted) {
            Ok(()) => removals::sync_dir(store)?,
            // Going already, or no store: a deletion that stopped, or a making that did.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let entries = match host_fs::read_dir(store) {
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
        remove(removals::remove_dir(store))?;
        // Gone durably, the directory it was in synced.
        removals::sync_dir(parent(store))
    };
    removing().with_context(|| format!("removing {}", store.display()))
}

/// The directory `path` is in.
fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The changes a deletion makes to the host's file system as it removes a store's directory
/// ([`remove_store_dir`]): std's, each a step, which a test records, and stops after a number of, as a
/// crash would stop them (TASK-79.77).
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

/// The cell of the store at `path`, as its description names it.
pub fn host_cell(path: &Path) -> Result<CellId, Error> {
    Ok(store_file(path)?.cell)
}

/// cladefs: the URL of the bucket the store at `path` keeps its cell in, as its description names
/// it: none for a store of memory's, which names none (TASK-165.6).
pub fn host_bucket(path: &Path) -> Result<Option<String>, Error> {
    Ok(store_file(path)?.bucket)
}

/// cladefs: the version of its cell the store at `path` is at, if it is one.
pub fn host_version(path: &Path) -> Result<Option<u64>, Error> {
    Ok(store_file(path)?.version)
}

/// cladefs: whether the store at `path` is an ephemeral cell's (doc-25, TASK-698.3), as its
/// description says.
pub fn host_ephemeral(path: &Path) -> Result<bool, Error> {
    Ok(store_file(path)?.ephemeral)
}

/// cladefs: the local checkpoint of the ephemeral cell's store at `path`, if it is one (doc-25, TASK-698.3):
/// what deleting it reads its reference of.
pub fn host_local_checkpoint(path: &Path) -> Result<Option<crate::LocalCheckpoint>, Error> {
    if !store_file(path)?.ephemeral {
        return Ok(None);
    }
    crate::LocalCheckpoint::read(&HostDir::new(path)?).map(Some)
}

/// cladefs: makes a new ephemeral cell's store at `path`, which must be a directory holding no
/// store (doc-25, TASK-698.3), as `description` describes, naming `bucket`, its parent's, its
/// cell of `kind`, its local checkpoint `local-checkpoint`: unmade until its maker says it is whole ([`made_host`]).
pub fn create_host_ephemeral(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    checkpoint: &crate::LocalCheckpoint,
) -> Result<(), Error> {
    LocalCell::make_ephemeral(
        &HostDir::new(path)?,
        description,
        Some(bucket),
        Some(kind),
        checkpoint,
    )
}

/// The description of the store whose directory is `store`.
fn store_file(store: &Path) -> Result<CellFile, Error> {
    let text = host_fs::read_to_string(store.join(DESCRIPTION))
        .with_context(|| format!("reading {}'s description", store.display()))?;
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

    /// The bucket the tests' stores name, which nothing here reaches.
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

    /// The store at `path`, opened where the bucket holds nothing of its cell.
    fn open(path: &Path) -> Result<LocalCell<HostDir>, Error> {
        LocalCell::open(HostDir::new(path)?, Options::default())
    }

    /// A store at `store` in `temp`, holding an extent: its path.
    async fn stored(temp: &Path) -> PathBuf {
        let path = temp.join("store");
        host_fs::create_dir(&path).unwrap();
        let description = Description {
            block_size: BLOCK as u32,
            capacity: 1 << 20,
            cell: bucket_cell::layout::test_cell(3),
        };
        create_host(&path, description, BUCKET, bucket_cell::cell::Kind::Fxfs).unwrap();
        let store = LocalCell::recover_unmade(HostDir::new(&path).unwrap()).unwrap();
        let store = store
            .open(Options::default(), crate::Opening::new(bucket_cell::layout::test_cell(3)))
            .unwrap();
        store.made().unwrap();
        store.write(ExtentId::new(3, 0), 0, &[7; BLOCK]).await.unwrap();
        store.commit(None).await.unwrap();
        path
    }

    /// A deletion that stopped partway, its description renamed and part of the rest gone, is
    /// finished by deleting again, which calls nothing to delete the cell elsewhere; and so is
    /// a directory a store's making left before its description, holding files of no store's.
    #[tokio::test(flavor = "multi_thread")]
    async fn stopped_deletions_finish() {
        let temp = tempfile::tempdir().unwrap();
        let path = stored(temp.path()).await;
        host_fs::rename(path.join(DESCRIPTION), path.join("store.deleted")).unwrap();
        host_fs::remove_dir_all(path.join(crate::layout::LOGS)).unwrap();
        delete_host(&path, async |_| Err(anyhow!("called"))).await.unwrap();
        assert!(!host_fs::exists(&path));

        let unmade = temp.path().join("unmade");
        host_fs::create_dir_all(unmade.join("segments")).unwrap();
        host_fs::write(unmade.join("stray"), "").unwrap();
        delete_host(&unmade, async |_| Err(anyhow!("called"))).await.unwrap();
        assert!(!host_fs::exists(&unmade));
    }

    /// A deletion calls `before` with the store's cell, while the store is open nowhere, before
    /// its directory goes: one `before` stopped leaves the store, which deleting again finishes.
    /// One of a store open in this process fails, calling nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn deletions_delete_their_cell_elsewhere_first() {
        let temp = tempfile::tempdir().unwrap();
        let path = stored(temp.path()).await;
        let opened = open(&path).unwrap();
        let refused = delete_host(&path, async |_| Err(anyhow!("called"))).await;
        assert!(refused.is_err() && host_fs::exists(&path.join(DESCRIPTION)), "{refused:?}");
        drop(opened);
        let stopped = delete_host(&path, async |cell| Err(anyhow!("stopped at {cell}"))).await;
        assert_eq!(stopped.map_err(|err| err.to_string()), Err("stopped at 1/3".to_owned()));
        assert!(open(&path).is_ok(), "a store stopped opens no more");
        let called = std::cell::Cell::new(None);
        delete_host(&path, async |cell| Ok(called.set(Some(cell)))).await.unwrap();
        assert_eq!(
            (called.get(), host_fs::exists(&path)),
            (Some(bucket_cell::layout::test_cell(3)), false)
        );
    }

    /// TASK-79.77: a deletion stopped at any step is finished by deleting again, which leaves
    /// nothing of the store: one stopped before the cell went elsewhere, and one stopped before
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
            let left: Vec<_> = host_fs::read_dir(temp).unwrap().collect();
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

    /// TASK-79.77: a deletion renames the store's description first, durably, so that the
    /// directory is no store from then on, and removes the directory durably, the directory it
    /// was in synced, last.
    #[tokio::test(flavor = "multi_thread")]
    async fn deletions_remove_the_stores_directory_durably() {
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
