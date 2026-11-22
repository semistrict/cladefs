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
//! crash leaves a directory every reader deletes whole (TASK-196), its restore made anew by the
//! next; in this process, once its maker says it is whole ([`made_host`](crate::made_host)), it is
//! a cell like any other. Its settling, by the cell's first flush, before anything of the flush is
//! synced, and by its close, makes all of it durable at once: every file and directory under it,
//! and its parent's entry for it; then its `UNMADE` file goes, durably, and its syncs are its own
//! again. No commit is acknowledged before its cell is durable: the guest sees no difference.

use crate::sync::{Condvar, Mutex};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The durability of a cell directory its maker defers. See the module's docs.
pub(crate) struct Deferral {
    path: PathBuf,
    state: Mutex<State>,
    settled: Condvar,
}

/// Where a [`Deferral`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Being made: unmade, here too.
    Making,
    /// Made, as its maker said: whole here, unmade on the disk.
    Made,
    /// Being made durable: what syncs meanwhile waits for it.
    Settling,
    /// Durable: its syncs its own.
    Settled,
}

node_local::node_local! {
    /// The cell directories whose durability is deferred, by their paths.
    static DEFERRED: std::sync::Mutex<HashMap<PathBuf, Arc<Deferral>>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Defers the durability of the cell to be made at `path` (the module's docs), until its
/// settling: the cell made there, and opened, by its maker. If it is not made whole, the deferral
/// ends as what is returned goes, its directory's syncs its own again, as its maker's failure
/// removes it.
pub fn defer_durability(path: &Path) -> Deferring {
    let deferral = Arc::new(Deferral {
        path: path.to_owned(),
        state: Mutex::new(State::Making),
        settled: Condvar::new(),
    });
    DEFERRED.get().lock().unwrap().insert(path.to_owned(), deferral.clone());
    Deferring(deferral)
}

/// A cell's deferred durability, as its maker holds it ([`defer_durability`]).
pub struct Deferring(Arc<Deferral>);

impl Drop for Deferring {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        if *state == State::Making {
            *state = State::Settled;
            drop(state);
            forget(&self.0.path);
        }
    }
}

/// The deferral of the cell directory at `path`, if its durability is deferred.
pub(crate) fn of(path: &Path) -> Option<Arc<Deferral>> {
    DEFERRED.get().lock().unwrap().get(path).cloned()
}

/// Ends the deferral of the cell directory at `path`, if its durability is deferred: removed.
pub(crate) fn forget(path: &Path) {
    DEFERRED.get().lock().unwrap().remove(path);
}

/// Whether the cell directory at `path` is whole in this process, unmade on the disk as its
/// durability is deferred.
pub(crate) fn made_here(path: &Path) -> bool {
    of(path).is_some_and(|deferral| deferral.made())
}

impl Deferral {
    /// Whether a sync of its directory's is to be skipped now: while it is made, or being made; a
    /// sync while it settles waits for its settling.
    pub fn skips(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        while *state == State::Settling {
            state = self.settled.wait(state).unwrap();
        }
        matches!(*state, State::Making | State::Made)
    }

    /// Whether it is whole here: made, as its maker said.
    pub fn made(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Made | State::Settling)
    }

    /// Its maker says it is whole: returns whether that is deferred too, its `UNMADE` file to go
    /// as it settles.
    pub fn mark_made(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        match *state {
            State::Making => {
                *state = State::Made;
                true
            }
            _ => false,
        }
    }

    /// Makes it durable, if it is made and not yet ([`Deferral`]'s docs): every file and
    /// directory under its directory, and its parent's entry for it, at once; then its `UNMADE`
    /// file goes, durably. Failing, it stays made, to settle again.
    pub fn settle(&self) -> io::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            if *state != State::Made {
                return Ok(());
            }
            *state = State::Settling;
        }
        let settled = self.make_durable();
        let mut state = self.state.lock().unwrap();
        *state = match settled {
            Ok(()) => State::Settled,
            Err(_) => State::Made,
        };
        drop(state);
        self.settled.notify_all();
        if settled.is_ok() {
            forget(&self.path);
        }
        settled
    }

    fn make_durable(&self) -> io::Result<()> {
        let mut found = Vec::new();
        under(&self.path, &mut found)?;
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

/// Adds `dir` and what is under it to `found`, each with whether it is a directory: what is
/// there, not what went meanwhile.
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

/// Makes the file or directory at `path` durable: one gone meanwhile needs nothing.
fn sync(path: &Path, dir: bool) -> io::Result<()> {
    let file = match host_fs::File::open(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        file => file?,
    };
    match dir {
        true => file.sync_all(),
        false => file.sync_data(),
    }
}
