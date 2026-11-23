// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell made whose durability its maker defers (board 7420): a restore's forks, its
//! root's and its memory's, which its VM runs on at once, durable only by the cell's first flush
//! or its close, whichever comes first. A restore waited on its forks' syncs, two dozen one after
//! another, each a journal commit on a loaded host's disk: 18 s to 70 s on ubu1 at 500 VMs.
//!
//! While its durability is deferred, nothing of the cell's directory is synced: its making's,
//! its opening's, whatever else asks. Its [`UNMADE`](crate::cell::UNMADE) file stays, so that a
//! crash, of the host or of the process, leaves a directory every reader deletes whole (TASK-196),
//! its restore made anew by the next; in this process, once its maker says it is whole
//! ([`made_host`](crate::made_host)), it is a cell like any other. Its settling, by the cell's
//! first flush, before anything of the flush is synced, and by its close, makes all of it durable
//! at once: every file and directory under it, and its parent's entry for it; then its `UNMADE`
//! file goes, durably, and its syncs are its own again. No commit is acknowledged before its cell
//! is durable: the guest sees no difference. What renames, links or replaces a file in it waits
//! for a settling in flight, and a settling for those in flight before it lists what is there:
//! a name it listed is the name it syncs. A settling that fails fails the cell, as any flush's failure does: nothing of it is
//! taken as durable after, its syncs its own again.
//!
//! A deferral is the directory's, by its canonical path, however its users spell it, as the cell
//! table's keys are.

use crate::sync::{Condvar, Mutex};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The durability of a cell directory its maker defers. See the module's docs.
pub(crate) struct Deferral {
    /// Its directory's canonical path ([`key`]).
    path: PathBuf,
    /// What its settling listed, a test's to see.
    #[cfg(test)]
    listed: Mutex<Vec<PathBuf>>,
    /// Whether its settling waits for renames in flight, a test's to see.
    #[cfg(test)]
    awaits_renames: std::sync::atomic::AtomicBool,
    state: Mutex<Stage>,
    /// What its settling's waits and renames' wait for: a settling's end, or a rename's.
    settled: Condvar,
}

/// A [`Deferral`]'s state, and how many renames of files in its directory are in flight
/// ([`Deferral::renaming`]), which a settling waits for before it lists what is there.
struct Stage {
    state: State,
    renaming: usize,
}

/// Where a [`Deferral`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Being made: unmade, here too.
    Making,
    /// Made, as its maker said: whole here, unmade on the disk.
    Made,
    /// Being made durable: what syncs, renames or settles meanwhile waits for it.
    Settling,
    /// Durable: its syncs its own.
    Settled,
    /// Its settling failed: unmade on the disk still, its syncs its own, its cell failed.
    Failed,
}

node_local::node_local! {
    /// The cell directories whose durability is deferred, by their canonical paths ([`key`]).
    static DEFERRED: std::sync::Mutex<HashMap<PathBuf, Arc<Deferral>>> =
        std::sync::Mutex::new(HashMap::new());
}

/// The key of the cell directory at `path`, which may not be there yet: its parent's canonical
/// path and its name, as the parent is there; as it is spelled, if the parent isn't.
fn key(path: &Path) -> PathBuf {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    let canonical = host_fs::canonicalize(parent.unwrap_or(Path::new(".")));
    match (canonical, path.file_name()) {
        (Ok(parent), Some(name)) => parent.join(name),
        _ => path.to_owned(),
    }
}

/// Defers the durability of the cell to be made at `path` (the module's docs), until its
/// settling: the cell made there, and opened, by its maker, who says so of what is returned
/// ([`Deferring::opened`]). If it is not made whole, the deferral ends as what is returned goes,
/// its directory's syncs its own again, as its maker's failure removes it; if it is made, and not
/// opened, it settles then, as a fork's making would have made it durable.
pub fn defer_durability(path: &Path) -> Deferring {
    let path = key(path);
    let deferral = Arc::new(Deferral {
        path: path.clone(),
        #[cfg(test)]
        listed: Mutex::new(Vec::new()),
        #[cfg(test)]
        awaits_renames: std::sync::atomic::AtomicBool::new(false),
        state: Mutex::new(Stage { state: State::Making, renaming: 0 }),
        settled: Condvar::new(),
    });
    DEFERRED.get().lock().unwrap().insert(path, deferral.clone());
    Deferring { deferral, opened: false }
}

/// A cell's deferred durability, as its maker holds it ([`defer_durability`]).
pub struct Deferring {
    deferral: Arc<Deferral>,
    /// Whether the cell was opened, which settles it from then on.
    opened: bool,
}

impl Deferring {
    /// The cell is open: its first flush, or its close, settles it.
    pub fn opened(mut self) {
        self.opened = true;
    }
}

impl Drop for Deferring {
    fn drop(&mut self) {
        let mut stage = self.deferral.state.lock().unwrap();
        match stage.state {
            State::Making => {
                stage.state = State::Settled;
                drop(stage);
                self.deferral.forget();
            }
            State::Made if !self.opened => {
                drop(stage);
                if let Err(err) = crate::upload::blocking(|| self.deferral.settle()) {
                    log::error!(
                        "extent store: making {} durable, its opening failed: {err}",
                        self.deferral.path.display()
                    );
                }
            }
            _ => {}
        }
    }
}

/// The deferral of the cell directory at `path`, if its durability is deferred.
pub(crate) fn of(path: &Path) -> Option<Arc<Deferral>> {
    DEFERRED.get().lock().unwrap().get(&key(path)).cloned()
}

/// Ends the deferral of the cell directory at `path`, if its durability is deferred: removed.
pub(crate) fn forget(path: &Path) {
    DEFERRED.get().lock().unwrap().remove(&key(path));
}

/// Whether the cell directory at `path` is whole in this process, unmade on the disk as its
/// durability is deferred.
pub(crate) fn made_here(path: &Path) -> bool {
    of(path).is_some_and(|deferral| deferral.made())
}

impl Deferral {
    /// Its stage, once no settling is in flight.
    fn settled_state(&self) -> crate::sync::MutexGuard<'_, Stage> {
        let mut stage = self.state.lock().unwrap();
        while stage.state == State::Settling {
            stage = self.settled.wait(stage).unwrap();
        }
        stage
    }

    /// Whether a sync of its directory's is to be skipped now: while it is made, or being made;
    /// a sync while it settles waits for its settling.
    pub fn skips(&self) -> bool {
        matches!(self.settled_state().state, State::Making | State::Made)
    }

    /// A rename, link or replacement of a file in its directory, in flight until what is returned
    /// goes: begun once no settling is in flight, and waited for by the next before it lists what
    /// is there (the module's docs).
    pub fn renaming(&self) -> Renaming<'_> {
        self.settled_state().renaming += 1;
        Renaming(self)
    }

    /// Whether it is whole here: made, as its maker said.
    pub fn made(&self) -> bool {
        matches!(self.state.lock().unwrap().state, State::Made | State::Settling)
    }

    /// Its maker says it is whole: returns whether that is deferred too, its `UNMADE` file to go
    /// as it settles.
    pub fn mark_made(&self) -> bool {
        let mut stage = self.state.lock().unwrap();
        match stage.state {
            State::Making => {
                stage.state = State::Made;
                true
            }
            _ => false,
        }
    }

    /// Makes it durable, if it is made and not yet ([`Deferral`]'s docs): every file and
    /// directory under its directory, and its parent's entry for it, at once; then its `UNMADE`
    /// file goes, durably. Waits for a settling in flight, and fails as it failed; failing, it
    /// fails for good, its syncs its own again, its cell failed by the flush that asked.
    pub fn settle(&self) -> io::Result<()> {
        {
            let mut stage = self.settled_state();
            match stage.state {
                State::Making | State::Settled => return Ok(()),
                State::Failed => return Err(io::Error::other("its deferred durability failed")),
                State::Settling => unreachable!("waited for"),
                State::Made => stage.state = State::Settling,
            }
            // The renames in flight done first: what it lists is there under the names it syncs.
            while stage.renaming > 0 {
                #[cfg(test)]
                self.awaits_renames.store(true, std::sync::atomic::Ordering::Relaxed);
                stage = self.settled.wait(stage).unwrap();
            }
        }
        fail::fail_point!("deferral::settle", |_| {
            self.ended(State::Failed);
            Err(io::Error::other("failpoint deferral::settle"))
        });
        let settled = self.make_durable();
        // Its error, if it failed, is the settling's answer, below.
        self.ended(if settled.is_ok() { State::Settled } else { State::Failed });
        settled
    }

    /// Its settling ended in `state`: what waited for it goes on, and it is forgotten.
    fn ended(&self, state: State) {
        self.state.lock().unwrap().state = state;
        self.settled.notify_all();
        self.forget();
    }

    /// Ends its deferral: forgotten, its directory's next users syncing as their own.
    fn forget(&self) {
        let mut deferred = DEFERRED.get().lock().unwrap();
        if deferred.get(&self.path).is_some_and(|alive| std::ptr::eq(&**alive, self)) {
            deferred.remove(&self.path);
        }
    }

    fn make_durable(&self) -> io::Result<()> {
        let mut found = Vec::new();
        under(&self.path, &mut found)?;
        #[cfg(test)]
        self.listed.lock().unwrap().extend(found.iter().map(|(path, _)| path.clone()));
        let parent = self.path.parent().filter(|parent| !parent.as_os_str().is_empty());
        found.push((parent.unwrap_or(Path::new(".")).to_owned(), true));
        let synced =
            crate::durability::at_once(&found, crate::durability::AT_ONCE, |(path, dir)| {
                sync(path, *dir)
                    .map_err(|err| anyhow::Error::new(err).context(path.display().to_string()))
            });
        synced.map_err(io::Error::other)?;
        match host_fs::remove_file(self.path.join(crate::cell::UNMADE)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        sync(&self.path, true)
    }
}

/// A rename of a file in a deferred directory, in flight ([`Deferral::renaming`]).
pub(crate) struct Renaming<'a>(&'a Deferral);

impl Drop for Renaming<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().renaming -= 1;
        self.0.settled.notify_all();
    }
}

/// Adds `dir` and what is under it to `found`, each with whether it is a directory: what is
/// there, not what went meanwhile, as what renames waits for the settling.
fn under(dir: &Path, found: &mut Vec<(PathBuf, bool)>) -> io::Result<()> {
    found.push((dir.to_owned(), true));
    let entries = match host_fs::read_dir(dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for entry in entries {
        let entry = match entry {
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            entry => entry?,
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => under(&entry.path(), found)?,
            Ok(_) => found.push((entry.path(), false)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Makes the file or directory at `path` durable, as the cell's own syncs do (`DiskDir`): one
/// removed meanwhile needs nothing.
fn sync(path: &Path, dir: bool) -> io::Result<()> {
    let file = match host_fs::File::open(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        file => file?,
    };
    #[cfg(target_os = "macos")]
    {
        let _ = dir;
        host_fs::sync_with(&file, &[libc::F_BARRIERFSYNC, libc::F_FULLFSYNC])
    }
    #[cfg(not(target_os = "macos"))]
    match dir {
        true => file.sync_all(),
        false => file.sync_data(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::{Dir as _, DiskDir, File as _};

    /// A file renamed through its directory while a settling begins is synced by its new name: the
    /// settling waits for the rename in flight before it lists what is there, as a framer's rename
    /// of a segment framed, its sync skipped, once lost its data (board 7420's reviews).
    #[test]
    fn settlings_list_what_renames_in_flight_left() {
        use std::sync::{Arc as StdArc, Barrier};
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("cell");
        let deferring = defer_durability(&path);
        host_fs::create_dir(&path).unwrap();
        let dir = DiskDir::new(&path).unwrap();
        let file = dir.create("framing", false).unwrap();
        file.write_at(&[7; 4096], 0).unwrap();
        file.sync().unwrap();
        crate::made_host(&path).unwrap();
        deferring.opened();
        let deferral = of(&path).unwrap();
        // The rename stops past its start, as it renames, until let go.
        let (arrived, released) = (StdArc::new(Barrier::new(2)), StdArc::new(Barrier::new(2)));
        let (at, go) = (arrived.clone(), released.clone());
        fail::cfg_callback("dir::renaming", move || {
            at.wait();
            go.wait();
        })
        .unwrap();
        std::thread::scope(|scope| {
            let renamed = scope.spawn(|| dir.rename("framing", "segment"));
            arrived.wait();
            let settling = scope.spawn(|| deferral.settle());
            // Once the settling waits for the rename, or, were it not to, has listed what is
            // there: renamed then, its listing missed the file.
            let awaits = || deferral.awaits_renames.load(std::sync::atomic::Ordering::Relaxed);
            while !awaits() && deferral.listed.lock().unwrap().is_empty() {
                std::thread::yield_now();
            }
            released.wait();
            renamed.join().unwrap().unwrap();
            settling.join().unwrap().unwrap();
        });
        fail::remove("dir::renaming");
        let listed = deferral.listed.lock().unwrap();
        assert!(listed.contains(&key(&path).join("segment")), "{listed:?}");
        assert!(!path.join(crate::cell::UNMADE).exists());
    }
}
