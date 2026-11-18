// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what cladefs keeps of its own in a store's directory, beside the store's files
//! (TASK-160.13): what is attached to it ([`attached`]), what it was forked from ([`ancestry`]),
//! the cells marked public ([`public_cells`]), and the boot profiles of bases
//! (`crate::profile`). Each record is written one way, durably ([`write`], [`mark`]): whole or not
//! at all, the file and its directory synced. And each is read by one rule ([`read`]): missing, it
//! is none; unreadable, or not what it says, the store's opening fails with `EIO`, logged, rather
//! than going on as if there were none, as a public base opening private would (TASK-160.10).

use crate::error::{Errno, Result};
use base32_id::{Padded, Short};
use local_cell::{Dir as _, File as _};
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// The record of what is attached to a store: a line of each's cells, in base 32,
/// comma-separated, and path.
const ATTACHED: &str = "attached-bases";

/// The record naming the stores a store was forked from, nearest first: a line of each's cell,
/// in base 32, and directory, as of the fork. A fork's directory is its own, its cell alone
/// in it (TASK-165.7): what it inherited of what cladefs keeps beside a store ([`inherit`]).
const ANCESTRY: &str = "ancestry";

/// The prefix of the empty file marking a cell public, its cell in base 32 after it, padded.
const PUBLIC: &str = "public-";

/// Writes `text` as the record `name` in `dir`, durably: whole or not at all, by a file written
/// and synced beside it, renamed over it, the directory synced after.
/// cladefs: through the store's directory, as every change to it is (`Dir::replace`, TASK-539.3).
pub(crate) fn write(dir: &Path, name: &str, text: &str) -> io::Result<()> {
    let dir = local_cell::HostDir::new(dir)?;
    dir.replace(name, text.as_bytes())?;
    dir.sync_dir("")
}

/// Makes the empty records `names` in `dir`, durably: each synced, then the directory; one there
/// already is left as it is. Through the store's directory, as every change to it is.
fn mark(dir: &Path, names: impl IntoIterator<Item = String>) -> io::Result<()> {
    let dir = local_cell::HostDir::new(dir)?;
    for name in names {
        match dir.create(&name, false) {
            Ok(file) => file.sync()?,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    dir.sync_dir("")
}

/// The record `name` in `dir`: none if it isn't there; `EIO`, logged, saying what it is (`what`),
/// if it can't be read.
fn read(dir: &Path, name: &str, what: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(dir.join(name)) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            log::error!("cladefs: reading {what} of {}: {err}", dir.display());
            Err(Errno::EIO)
        }
    }
}

/// The lines of the record `name` in `dir`, each as `parse` reads it: `EIO`, logged, for one it
/// can't.
fn lines<T>(
    dir: &Path,
    name: &str,
    what: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>> {
    let Some(text) = read(dir, name, what)? else { return Ok(Vec::new()) };
    text.lines()
        .map(|line| {
            parse(line).ok_or_else(|| {
                log::error!("cladefs: {} says {line:?} of {what}", dir.display());
                Errno::EIO
            })
        })
        .collect()
}

/// `EIO`, logged, for `err`, doing `what` to the store in `dir`.
fn failed(dir: &Path, what: &str, err: io::Error) -> Errno {
    log::error!("cladefs: {what} of {}: {err}", dir.display());
    Errno::EIO
}

/// What is recorded as attached to the store in `dir`: each's cells and path.
pub(crate) fn attached(dir: &Path) -> Result<Vec<(Vec<u64>, PathBuf)>> {
    lines(dir, ATTACHED, "what is attached", |line| {
        let (cells, base) = line.split_once(' ')?;
        let cells = cells.split(',').map(base32_id::parse);
        Some((cells.collect::<Option<_>>()?, base.into()))
    })
}

/// Records `base`, of `cells`, as attached to the store in `dir`, durably, unless it is.
pub(crate) fn attach(dir: &Path, base: &Path, cells: &[u64]) -> Result<()> {
    let mut attached = attached(dir)?;
    if attached.iter().any(|(_, recorded)| recorded == base) {
        return Ok(());
    }
    attached.push((cells.to_vec(), base.to_owned()));
    write_attached(dir, &attached)
}

/// Replaces what the store in `dir` records as attached with `attached`, durably.
fn write_attached(dir: &Path, attached: &[(Vec<u64>, PathBuf)]) -> Result<()> {
    let mut text = String::new();
    for (cells, base) in attached {
        let cells: Vec<String> = cells.iter().map(|&cell| Short(cell).to_string()).collect();
        text += &format!("{} {}\n", cells.join(","), base.display());
    }
    write(dir, ATTACHED, &text).map_err(|err| failed(dir, "recording what is attached", err))
}

/// The stores the store in `dir` was forked from, nearest first: none, if it is no fork.
pub(crate) fn ancestry(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    lines(dir, ANCESTRY, "what it was forked from", |line| {
        let (cell, from) = line.split_once(' ')?;
        Some((base32_id::parse(cell)?, from.into()))
    })
}

/// The cells of the store in `dir` marked public; `EIO`, logged, if its directory can't be
/// listed, as with as many files open as the process may: read as none, a public base would open
/// private, its writer's (TASK-160.10).
pub(crate) fn public_cells(dir: &Path) -> Result<BTreeSet<u64>> {
    let listed = || -> io::Result<_> {
        fail::fail_point!("extents::public_cells", |_| {
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        });
        let mut public = BTreeSet::new();
        for entry in std::fs::read_dir(dir)? {
            let name = entry?.file_name();
            let marked = name.to_str().and_then(|name| name.strip_prefix(PUBLIC));
            if let Some(cell) = marked.and_then(base32_id::parse_padded) {
                public.insert(cell);
            }
        }
        Ok(public)
    };
    listed().map_err(|err| failed(dir, "listing the public cells", err))
}

/// Marks `cells` public in the store in `dir`, durably.
pub(crate) fn mark_public(dir: &Path, cells: &[u64]) -> Result<()> {
    let markers = cells.iter().map(|&cell| format!("{PUBLIC}{}", Padded(cell)));
    mark(dir, markers).map_err(|err| failed(dir, "marking cells public", err))
}

/// What a fork of the store in `from`, of `cell`, forked from `forked_from` itself, inherits of
/// what cladefs keeps beside it, into the directory `to`, its child's, before the child's store is
/// made there: what is attached to it, which stays attached to the fork, a restored VM's too; the
/// cells marked public, its own and those it inherited from, whose pages the fork's are of the
/// default arena for; and what it was forked from, it nearest (TASK-165.7).
pub(crate) fn inherit(
    from: &Path,
    cell: u64,
    forked_from: &[(u64, PathBuf)],
    to: &Path,
) -> Result<()> {
    inherit_attached(from, to)?;
    let from = from.canonicalize().map_err(|err| failed(from, "finding the store", err))?;
    let mut text = format!("{} {}\n", Short(cell), from.display());
    for &(cell, ref dir) in forked_from {
        text += &format!("{} {}\n", Short(cell), dir.display());
    }
    write(to, ANCESTRY, &text).map_err(|err| failed(to, "recording what it was forked from", err))
}

/// What the store in `to` inherits of the store in `from` that its guest reads: what is attached
/// to it, and the cells marked public, its own and those it inherited from ([`inherit`]).
pub(crate) fn inherit_attached(from: &Path, to: &Path) -> Result<()> {
    let attached = attached(from)?;
    if !attached.is_empty() {
        write_attached(to, &attached)?;
    }
    let public: Vec<u64> = public_cells(from)?.into_iter().collect();
    if !public.is_empty() {
        mark_public(to, &public)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each record reads back as written; one missing is none; one not what it says, or that
    /// can't be read, is `EIO`; and a write leaves no file of its own beside the record.
    #[test]
    fn records_read_back_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        assert_eq!(attached(dir), Ok(Vec::new()));
        assert_eq!(ancestry(dir), Ok(Vec::new()));
        attach(dir, Path::new("/stores/base"), &[0x1a, 0x2b]).unwrap();
        attach(dir, Path::new("/stores/base"), &[0x1a, 0x2b]).unwrap();
        attach(dir, Path::new("/stores/other"), &[0x3c]).unwrap();
        let expected =
            vec![(vec![0x1a, 0x2b], "/stores/base".into()), (vec![0x3c], "/stores/other".into())];
        assert_eq!(attached(dir), Ok(expected));
        mark_public(dir, &[7, 9]).unwrap();
        assert_eq!(public_cells(dir), Ok(BTreeSet::from([7, 9])));
        let names: BTreeSet<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let expected = ["attached-bases", "public-0000000000007", "public-0000000000009"];
        assert_eq!(names, expected.map(str::to_owned).into());

        std::fs::write(dir.join(ANCESTRY), "1F /stores/base\n").unwrap();
        assert_eq!(ancestry(dir), Err(Errno::EIO));
        std::fs::create_dir(dir.join("unreadable")).unwrap();
        assert_eq!(read(dir, "unreadable", "a directory"), Err(Errno::EIO));
    }

    /// A fork inherits what is attached, what is public and what its store was forked from, its
    /// store nearest.
    #[test]
    fn forks_inherit_their_stores_records() {
        let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        attach(from.path(), Path::new("/stores/base"), &[0x1a]).unwrap();
        mark_public(from.path(), &[5]).unwrap();
        let older = vec![(3, PathBuf::from("/stores/grandparent"))];
        inherit(from.path(), 4, &older, to.path()).unwrap();
        assert_eq!(attached(to.path()), attached(from.path()));
        assert_eq!(public_cells(to.path()), Ok(BTreeSet::from([5])));
        let parent = from.path().canonicalize().unwrap();
        assert_eq!(ancestry(to.path()), Ok(vec![(4, parent), (3, "/stores/grandparent".into())]));
    }
}
