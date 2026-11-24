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
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Makes a new cell at `path`, which must be a directory holding no cell, as `description`
/// describes. cladefs: its description names `bucket`, the URL of the bucket its cell is kept in,
/// as every host's cell's does (TASK-165.6); it is unmade until its maker says it is whole
/// ([`LocalCell::made`], TASK-196). Its description is made last: until it is durable, the
/// directory is no cell, whatever else is in it. Opening it takes its cell in the bucket
/// ([`LocalCell::recover`], `crate::upload`), and its cell, of `kind`, whose cell it is (TASK-364,
/// doc-20).
pub fn create_host(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
) -> Result<(), Error> {
    create_host_with(path, description, bucket, kind, |_| Ok(()))
}

/// cladefs: makes a new cell at `path` as [`create_host`] does, `extra` writing what else its
/// directory holds into its making first, made durable with its description at once
/// ([`Making`](crate::Making), board 7420).
pub fn create_host_with(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    extra: impl FnOnce(&mut crate::Making<'_, HostDir>) -> Result<(), Error>,
) -> Result<(), Error> {
    LocalCell::make_with(&HostDir::new(path)?, description, Some(bucket), Some(kind), None, extra)
}

/// cladefs: makes a new cell at `path` as [`create_host`] does, a cell at the version `version` of
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

/// cladefs: the cell at `path` is whole, as its maker made it ([`LocalCell::made`], TASK-196): the
/// file saying it is not goes, durably, its maker having let it go.
pub fn made_host(path: &Path) -> Result<(), Error> {
    // cladefs: its durability deferred, its `UNMADE` file goes as it settles (board 7420).
    if crate::deferral::of(path).is_some_and(|deferral| deferral.mark_made()) {
        return Ok(());
    }
    let dir = HostDir::new(path)?;
    let made = match dir.remove(UNMADE) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    };
    made.and_then(|()| dir.sync_dir("")).with_context(|| format!("making {} whole", path.display()))
}

node_local::node_local! {
    /// cladefs: the cell directories that makings which failed in this process left, and failed
    /// to remove ([`left_unmade`]), by their canonical paths (`deferral::key`).
    static LEFT: std::sync::Mutex<HashSet<PathBuf>> = std::sync::Mutex::new(HashSet::new());
}

/// cladefs: the directory at `path`, which a making that failed left, its removal failing too,
/// is no cell in this process, whatever it looks like on the disk ([`unmade_host`]), until it
/// goes ([`remove_cell_dir`]): its making's last step, `UNMADE`'s removal, may have left it
/// looking made, undurably, and its maker's caller, told the making failed, makes it again
/// there, which a cell there refuses (board 10506). A crash of the host or the process after
/// leaves what made it look made undurable, as a crash after any making's last step.
pub fn left_unmade(path: &Path) {
    LEFT.get().lock().unwrap().insert(crate::deferral::key(path));
}

/// cladefs: whether `path` is a directory a cell's making left unmade, its maker stopping part
/// way: no cell, which deleting finishes (TASK-79.84, TASK-196). Its description, made last of
/// its directory, is not there; or it says it was never made whole ([`LocalCell::made`]); or a
/// making in this process that failed left it ([`left_unmade`]).
pub fn unmade_host(path: &Path) -> bool {
    if LEFT.get().lock().unwrap().contains(&crate::deferral::key(path)) {
        return host_fs::is_dir(path);
    }
    // cladefs: whole here, its durability deferred (board 7420).
    if crate::deferral::made_here(path) {
        return false;
    }
    host_fs::is_dir(path)
        && (!host_fs::exists(path.join(DESCRIPTION)) || host_fs::exists(path.join(UNMADE)))
}

/// cladefs: places the cell at `path` (TASK-39.7): records, durably, in its description, the
/// fencing token its host takes its cell with from then on, the boot ID of the placement it
/// is, which a later placement's exceeds; its writer takes its cell's head with the epoch of
/// that taking of the cell. A placement takes the cell only with a token as high as the last
/// one's: a placement cut off takes no cell back from the one after it.
/// A cell never placed takes it with its host's (TASK-165.9: the token is the description's).
/// cladefs: `force`d, this placement's takings of a replicated cell go on with the members of the
/// configuration its head names that answer, too few for a take (TASK-681): the commits told
/// replicated that only the others held are lost. A placement after, unforced, is not.
pub fn place_host(path: &Path, token: u64, force: bool) -> Result<(), Error> {
    let placed = HostDir::new(path).map_err(Error::new).and_then(|dir| place(&dir, token, force));
    placed.with_context(|| format!("placing {}", path.display()))
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
pub async fn delete_host(
    path: &Path,
    before: impl AsyncFnOnce(CellId) -> Result<(), Error>,
) -> Result<(), Error> {
    // cladefs: its disk work off the runtime's threads, `before` awaited between (TASK-685.3).
    use crate::upload::blocking;
    if !host_fs::exists(path.join(DESCRIPTION)) {
        return blocking(|| remove_cell_dir(path));
    }
    let (cell, dir) = blocking(|| -> Result<_, Error> {
        let cell = host_cell(path)?;
        let dir = HostDir::new(path)?;
        dir.take().context("taking the cell directory")?;
        Ok((cell, dir))
    })?;
    // cladefs: once no cell of the directory is closing in the process, holding no thread, as an
    // opening awaits it before its claim, which then waits for none (board 2946).
    crate::closed(path).await.context("waiting for the cell's close")?;
    let removed = match blocking(|| dir.claim().context("taking the cell directory")) {
        Err(err) => Err(err),
        Ok(()) => {
            let removed = match before(cell).await {
                Ok(()) => blocking(|| -> Result<(), Error> {
                    failpoint!("delete::cell");
                    remove_cell_dir(path)
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

/// The name a cell's description goes by as its directory goes ([`remove_cell_dir`]): its
/// [`DESCRIPTION`]'s, `.deleted` after it.
pub(crate) const DELETED: &str = "store.deleted";

/// Removes the directory of the cell at `cell_dir`, unless it went already: whole, or so that
/// removing it again finishes it. Its description goes first, renamed to [`DELETED`],
/// durably, so that the directory is no cell from then on, but one going, which a deletion resumed
/// finds so; then the rest, then that, then the directory. Then the directory it was in is synced,
/// so that it is gone durably (TASK-79.77). cladefs: the one way a cell directory goes, whether
/// it held a cell or what a making left of one, so that what its files took of the host's disk
/// goes with them (TASK-539.3); the cell opened nowhere.
pub fn remove_cell_dir(cell_dir: &Path) -> Result<(), Error> {
    // cladefs: its durability deferred no more: gone (board 7420).
    crate::deferral::forget(cell_dir);
    let removed = crate::footprint::removing(cell_dir, || remove_cell(cell_dir));
    // cladefs: what a failed making left gone, its last step failing or not, what is made there
    // next is what it is.
    if !host_fs::exists(cell_dir) {
        LEFT.get().lock().unwrap().remove(&crate::deferral::key(cell_dir));
    }
    removed
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

/// cladefs: the local checkpoint of the ephemeral cell at `path`, if it is one (doc-25,
/// TASK-698.3): what deleting it reads its reference of.
pub fn host_local_checkpoint(path: &Path) -> Result<Option<crate::LocalCheckpoint>, Error> {
    if !cell_file(path)?.ephemeral {
        return Ok(None);
    }
    crate::LocalCheckpoint::read(&HostDir::new(path)?).map(Some)
}

/// cladefs: makes a new ephemeral cell at `path`, which must be a directory holding no cell
/// (doc-25, TASK-698.3), as `description` describes, naming `bucket`, its parent's, its cell of
/// `kind`, its local checkpoint `local-checkpoint`, `extra` writing what else it holds into its
/// making first ([`create_host_with`]): unmade until its maker says it is whole ([`made_host`]).
pub fn create_host_ephemeral(
    path: &Path,
    description: Description,
    bucket: &str,
    kind: Kind,
    checkpoint: &crate::LocalCheckpoint,
    encrypting: (&extent_frame::Secret, extent_frame::KeyId),
    extra: impl FnOnce(&mut crate::Making<'_, HostDir>) -> Result<(), Error>,
) -> Result<(), Error> {
    LocalCell::make_ephemeral(
        &HostDir::new(path)?,
        description,
        Some(bucket),
        Some(kind),
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
        LocalCell::open(HostDir::new(path)?, Options::default())
    }

    /// A cell at `cell` in `temp`, holding an extent: its path.
    async fn stored(temp: &Path) -> PathBuf {
        let path = temp.join("cell");
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

    /// A deletion calls `before` with the cell, while the cell is open nowhere, before
    /// its directory goes: one `before` stopped leaves the cell, which deleting again finishes.
    /// One of a cell open in this process fails, calling nothing.
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
        assert!(open(&path).is_ok(), "a cell stopped opens no more");
        let called = std::cell::Cell::new(None);
        delete_host(&path, async |cell| Ok(called.set(Some(cell)))).await.unwrap();
        assert_eq!(
            (called.get(), host_fs::exists(&path)),
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
