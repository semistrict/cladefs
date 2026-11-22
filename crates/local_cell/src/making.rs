// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's directory as its maker writes it, unmade (board 7420): what goes in it beside
//! its description, made durable with the description at once, not each before the next.
//!
//! A cell's making syncs its [`UNMADE`](crate::cell::UNMADE) file first: from then on, until its
//! maker says it is whole, a crash leaves a directory no one trusts, which is deleted whole
//! (`unmade_host`, TASK-196). What is written meanwhile needs no order among its syncs: each file's
//! data, each directory's entries, the parent's entry for the directory, are durable together once
//! they all are, which the cell's being made whole waits for. One after another, each sync waited
//! for those before it: a fork's making took seven in a row, each a journal commit on a loaded
//! host's disk, a restore of a zygote, two forks, 18 s to 70 s on ubu1's scratch disk.

use crate::dir::{Dir, File as _};
use crate::durability::{AT_ONCE, at_once};
use anyhow::{Context as _, Error};
use std::io;
use std::sync::Arc;

/// cladefs: what a cell's maker makes durable at once in its directory, unmade (the module's
/// docs): the files it wrote there, the directories whose entries it changed, and what else it
/// syncs of its own.
pub struct Making<'d, D: Dir> {
    dir: &'d D,
    durable: Vec<Durable<'d, D::File>>,
}

/// One thing a [`Making`] makes durable.
enum Durable<'d, F> {
    File(String, Arc<F>),
    Dir(String),
    Other(&'static str, Box<dyn Fn() -> io::Result<()> + Send + Sync + 'd>),
}

impl<'d, D: Dir> Making<'d, D> {
    /// The making of the cell directory `dir`, which says it is unmade, durably.
    pub(crate) fn new(dir: &'d D) -> Self {
        Self { dir, durable: Vec::new() }
    }

    /// The directory.
    pub fn dir(&self) -> &'d D {
        self.dir
    }

    /// Writes `bytes` as the new file `name`, made durable with the rest: plainly, as nothing
    /// reads the directory before it is whole.
    pub fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let file = self.dir.create(name, false).with_context(|| format!("making {name}"))?;
        file.write_at(bytes, 0).with_context(|| format!("writing {name}"))?;
        self.file(name, file);
        Ok(())
    }

    /// Makes `file`, written as `name`, durable with the rest.
    pub(crate) fn file(&mut self, name: &str, file: D::File) {
        self.durable.push(Durable::File(name.to_owned(), Arc::new(file)));
    }

    /// Makes the entries of the directory at `path`, in the cell's directory, durable with the
    /// rest.
    pub fn dir_entries(&mut self, path: &str) {
        self.durable.push(Durable::Dir(path.to_owned()));
    }

    /// Makes `what` durable with the rest by `sync`: what the maker keeps outside the directory,
    /// such as the parent's entry for it.
    pub fn also(
        &mut self,
        what: &'static str,
        sync: impl Fn() -> io::Result<()> + Send + Sync + 'd,
    ) {
        self.durable.push(Durable::Other(what, Box::new(sync)));
    }

    /// Makes all of it durable at once, [`AT_ONCE`] syncs at a time (TASK-92.42); one at a time,
    /// if the directory's I/O is its callers' alone ([`Dir::concurrent`], TASK-79.79).
    pub(crate) fn sync(self) -> Result<(), Error> {
        let Self { dir, durable } = self;
        // Its durability deferred, its settling makes it all durable, its parent's entry too.
        if dir.defers() {
            return Ok(());
        }
        let most = if dir.concurrent() { AT_ONCE } else { 1 };
        at_once(&durable, most, |durable| match durable {
            Durable::File(name, file) => file.sync().with_context(|| format!("syncing {name}")),
            Durable::Dir(path) => dir.sync_dir(path).with_context(|| format!("syncing {path:?}")),
            Durable::Other(what, sync) => sync().with_context(|| format!("syncing {what}")),
        })
    }
}
