// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what cladefs keeps of its own in a cell directory, beside the cell's files
//! (TASK-160.13): what is attached to it ([`attached`]), what it was forked from ([`ancestry`]),
//! and the cells marked public ([`public_cells`]). Each record is written one way, durably
//! ([`write`], [`mark`]): whole or not at all, the file and its directory synced; or, a fork's,
//! into its cell's making, with its description ([`inherit`], board 7420). And each is read
//! by one rule ([`read`]): missing, it is none; unreadable, or not what it says, the cell's opening
//! fails with `EIO`, logged, rather than going on as if there were none, as a public base opening
//! private would (TASK-160.10).
//!
//! cladefs: the records are a family of formats of their own, frozen ([`FORMAT`], doc-28, board
//! 10044): every later build reads every version from 1 on. Each record says its version, its
//! first line [`HEADER`] and the version; the migration marker its `version` field
//! (`cladefsd::migration`); a public mark is a name alone.

use crate::error::{Errno, Result};
use base32_id::{Padded, Short};
use local_cell::{Dir as _, File as _};
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// The record of what is attached to a cell: a line of each's cells, in base 32,
/// comma-separated, and path.
const ATTACHED: &str = "attached-bases";

/// The record naming the cells a cell was forked from, nearest first: a line of each's cell,
/// in base 32, and directory, as of the fork. A fork's directory is its own, its cell alone
/// in it (TASK-165.7): what it inherited of what cladefs keeps beside a cell ([`inherit`]).
const ANCESTRY: &str = "ancestry";

/// The prefix of the empty file marking a cell public, its cell in base 32 after it, padded.
const PUBLIC: &str = "public-";

/// cladefs's records in a cell directory, beside the cell's own (`local_cell::FORMAT`).
pub(crate) const FORMAT: formats::Format = formats::Format::new("cell records", 1, 1);

/// What a record of lines starts with, its version after it.
const HEADER: &str = "cladefs cell records ";

/// The first line of a record of lines this build writes.
fn header() -> String {
    format!("{HEADER}{}\n", FORMAT.current)
}

/// The version `line`, a record's first, says, if this build reads it.
fn version_of(line: &str) -> Option<u32> {
    let version = line.strip_prefix(HEADER)?;
    // Digits alone: `u32::from_str` would take a sign too.
    if version.is_empty() || !version.bytes().all(|digit| digit.is_ascii_digit()) {
        return None;
    }
    FORMAT.check(version.parse().ok()?).ok()
}

/// The lines of `text`, a record of lines, after its header, each as `parse` reads it: none if its
/// header says no version this build reads, or a line doesn't read; the line, or the header, that
/// doesn't, its error.
fn parse_lines<T>(text: &str, parse: impl Fn(&str) -> Option<T>) -> Result<Vec<T>, String> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    version_of(first).ok_or_else(|| format!("{first:?}, of no version of {FORMAT:?}"))?;
    lines.map(|line| parse(line).ok_or_else(|| format!("{line:?}"))).collect()
}

/// A line of what is attached: its cells, in base 32, comma-separated, and its path.
fn attached_line(line: &str) -> Option<(Vec<u64>, PathBuf)> {
    let (cells, base) = line.split_once(' ')?;
    let cells = cells.split(',').map(base32_id::parse);
    Some((cells.collect::<Option<_>>()?, base.into()))
}

/// A line of what a cell was forked from: its cell, in base 32, and its directory.
fn ancestry_line(line: &str) -> Option<(u64, PathBuf)> {
    let (cell, from) = line.split_once(' ')?;
    Some((base32_id::parse(cell)?, from.into()))
}

/// Writes `text` as the record `name` in `dir`, durably: whole or not at all, by a file written
/// and synced beside it, renamed over it, the directory synced after.
/// cladefs: through the cell directory, as every change to it is (`Dir::replace`, TASK-539.3).
pub(crate) fn write(dir: &Path, name: &str, text: &str) -> io::Result<()> {
    let dir = local_cell::HostDir::new(dir)?;
    dir.replace(name, text.as_bytes())?;
    dir.sync_dir("")
}

/// Makes the empty records `names` in `dir`, durably: each synced, then the directory; one there
/// already is left as it is. Through the cell directory, as every change to it is.
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
    match host_fs::read_to_string(dir.join(name)) {
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
    parse_lines(&text, parse).map_err(|said| {
        log::error!("cladefs: {} says {said} of {what}", dir.display());
        Errno::EIO
    })
}

/// `EIO`, logged, for `err`, doing `what` to the cell in `dir`.
fn failed(dir: &Path, what: &str, err: io::Error) -> Errno {
    log::error!("cladefs: {what} of {}: {err}", dir.display());
    Errno::EIO
}

/// What is recorded as attached to the cell in `dir`: each's cells and path.
pub(crate) fn attached(dir: &Path) -> Result<Vec<(Vec<u64>, PathBuf)>> {
    lines(dir, ATTACHED, "what is attached", attached_line)
}

/// Records `base`, of `cells`, as attached to the cell in `dir`, durably, unless it is.
pub(crate) fn attach(dir: &Path, base: &Path, cells: &[u64]) -> Result<()> {
    let mut attached = attached(dir)?;
    if attached.iter().any(|(_, recorded)| recorded == base) {
        return Ok(());
    }
    attached.push((cells.to_vec(), base.to_owned()));
    write_attached(dir, &attached)
}

/// Replaces what the cell in `dir` records as attached with `attached`, durably.
fn write_attached(dir: &Path, attached: &[(Vec<u64>, PathBuf)]) -> Result<()> {
    let text = attached_text(attached);
    write(dir, ATTACHED, &text).map_err(|err| failed(dir, "recording what is attached", err))
}

/// The record of `attached`, as [`attached`] reads it.
fn attached_text(attached: &[(Vec<u64>, PathBuf)]) -> String {
    let mut text = header();
    for (cells, base) in attached {
        let cells: Vec<String> = cells.iter().map(|&cell| Short(cell).to_string()).collect();
        text += &format!("{} {}\n", cells.join(","), base.display());
    }
    text
}

/// The cells the cell in `dir` was forked from, nearest first: none, if it is no fork.
pub(crate) fn ancestry(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    lines(dir, ANCESTRY, "what it was forked from", ancestry_line)
}

/// The cells of the cell in `dir` marked public; `EIO`, logged, if its directory can't be
/// listed, as with as many files open as the process may: read as none, a public base would open
/// private, its writer's (TASK-160.10).
pub(crate) fn public_cells(dir: &Path) -> Result<BTreeSet<u64>> {
    let listed = || -> io::Result<_> {
        fail::fail_point!("extents::public_cells", |_| {
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        });
        let mut public = BTreeSet::new();
        for entry in host_fs::read_dir(dir)? {
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

/// Marks `cells` public in the cell in `dir`, durably.
pub(crate) fn mark_public(dir: &Path, cells: &[u64]) -> Result<()> {
    mark(dir, cells.iter().map(|&cell| public(cell)))
        .map_err(|err| failed(dir, "marking cells public", err))
}

/// The name of the record marking `cell` public.
fn public(cell: u64) -> String {
    format!("{PUBLIC}{}", Padded(cell))
}

/// What a fork of the cell in `from`, of `cell`, forked from `forked_from` itself, inherits of
/// what cladefs keeps beside it, written into the making of its child's cell, `making`, before the
/// child's description, durable with it at once (board 7420): what is attached to it, which stays
/// attached to the fork, a restored VM's too; the cells marked public, its own and those it
/// inherited from, whose pages the fork's are of the default arena for; and what it was forked
/// from, it nearest (TASK-165.7). Written plainly, not each whole or not at all: a crash before
/// the child is made whole leaves its directory for deleting.
pub(crate) fn inherit(
    from: &Path,
    cell: u64,
    forked_from: &[(u64, PathBuf)],
    making: &mut local_cell::Making<'_, local_cell::HostDir>,
) -> Result<()> {
    let to = making.dir().path().to_owned();
    let mut write = |name: &str, text: &str, what: &str| {
        making.write(name, text.as_bytes()).map_err(|err| {
            log::error!("cladefs: {what} of {}: {err:#}", to.display());
            Errno::EIO
        })
    };
    let attached = attached(from)?;
    if !attached.is_empty() {
        write(ATTACHED, &attached_text(&attached), "recording what is attached")?;
    }
    for public_cell in public_cells(from)? {
        write(&public(public_cell), "", "marking cells public")?;
    }
    let from = host_fs::canonicalize(&from)
        .map_err(|err| failed(from, "finding the cell directory", err))?;
    let ancestry: Vec<_> = [(cell, from)].into_iter().chain(forked_from.iter().cloned()).collect();
    write(ANCESTRY, &ancestry_text(&ancestry), "recording what it was forked from")
}

/// The record of `ancestry`, nearest first, as [`ancestry`] reads it.
fn ancestry_text(ancestry: &[(u64, PathBuf)]) -> String {
    let mut text = header();
    for (cell, dir) in ancestry {
        text += &format!("{} {}\n", Short(*cell), dir.display());
    }
    text
}

/// What the cell in `to` inherits of the cell in `from` that its guest reads: what is attached
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
        attach(dir, Path::new("/cells/base"), &[0x1a, 0x2b]).unwrap();
        attach(dir, Path::new("/cells/base"), &[0x1a, 0x2b]).unwrap();
        attach(dir, Path::new("/cells/other"), &[0x3c]).unwrap();
        let expected =
            vec![(vec![0x1a, 0x2b], "/cells/base".into()), (vec![0x3c], "/cells/other".into())];
        assert_eq!(attached(dir), Ok(expected));
        mark_public(dir, &[7, 9]).unwrap();
        assert_eq!(public_cells(dir), Ok(BTreeSet::from([7, 9])));
        let names: BTreeSet<String> = host_fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let expected = ["attached-bases", "public-0000000000007", "public-0000000000009"];
        assert_eq!(names, expected.map(str::to_owned).into());

        host_fs::write(dir.join(ANCESTRY), format!("{}1F /cells/base\n", header())).unwrap();
        assert_eq!(ancestry(dir), Err(Errno::EIO));
        // A record of a version this build doesn't read, or of none, as before records said
        // theirs, is EIO, never read as another's (doc-28).
        for first in ["cladefs cell records 2", "cladefs cell records 0", "1 /cells/base", ""] {
            host_fs::write(dir.join(ANCESTRY), format!("{first}\n1 /cells/base\n")).unwrap();
            assert_eq!(ancestry(dir), Err(Errno::EIO), "{first:?}");
        }
        host_fs::write(dir.join(ANCESTRY), format!("{}1 /cells/base\n", header())).unwrap();
        assert_eq!(ancestry(dir), Ok(vec![(1, "/cells/base".into())]));
        host_fs::create_dir(dir.join("unreadable")).unwrap();
        assert_eq!(read(dir, "unreadable", "a directory"), Err(Errno::EIO));
    }

    /// A fork inherits what is attached, what is public and what its cell was forked from, its
    /// cell nearest.
    #[test]
    fn forks_inherit_their_cells_records() {
        let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        attach(from.path(), Path::new("/cells/base"), &[0x1a]).unwrap();
        mark_public(from.path(), &[5]).unwrap();
        let older = vec![(3, PathBuf::from("/cells/grandparent"))];
        let cell = local_cell::CellId { tenant: 0, id: 6 };
        let description = local_cell::Description { block_size: 4096, capacity: 1 << 20, cell };
        let made = local_cell::create_host_with(
            to.path(),
            description,
            "memory:///cells",
            bucket_cell::cell::Kind::Fxfs,
            |making| Ok(inherit(from.path(), 4, &older, making)?),
        );
        made.unwrap();
        assert_eq!(attached(to.path()), attached(from.path()));
        assert_eq!(public_cells(to.path()), Ok(BTreeSet::from([5])));
        let parent = host_fs::canonicalize(&from.path()).unwrap();
        assert_eq!(ancestry(to.path()), Ok(vec![(4, parent), (3, "/cells/grandparent".into())]));
    }

    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<Records>(env!("CARGO_MANIFEST_DIR"));
    }

    /// cladefs's records in a cell directory: what is attached, what a cell was forked from, and
    /// a migration's marker (`cladefsd::migration`).
    struct Records;

    impl formats::golden::Family for Records {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            let attached = [(vec![0x1a, 0x2b], "/cells/base".into()), (vec![0x3c], "/b".into())];
            let ancestry = [(4, "/cells/parent".into()), (3, "/cells/grandparent".into())];
            vec![
                ("attached-bases", attached_text(&attached).into_bytes()),
                ("ancestry", ancestry_text(&ancestry).into_bytes()),
                ("migrating", crate::cladefsd::migration::golden_marker()),
            ]
        }

        fn written(version: u32, case: &str, bytes: &[u8]) -> std::result::Result<String, String> {
            // Version 1's readers are this build's.
            if version != 1 {
                return Err(format!("no reader of version {version}"));
            }
            Self::read_as(case, bytes)
        }

        fn read(case: &str, bytes: &[u8]) -> std::result::Result<(), String> {
            Self::read_as(case, bytes).map(drop)
        }
    }

    impl Records {
        fn read_as(case: &str, bytes: &[u8]) -> std::result::Result<String, String> {
            let text = || std::str::from_utf8(bytes).map_err(|err| err.to_string());
            match case {
                "attached-bases" => parse_lines(text()?, attached_line).map(|r| format!("{r:#?}")),
                "ancestry" => parse_lines(text()?, ancestry_line).map(|r| format!("{r:#?}")),
                "migrating" => crate::cladefsd::migration::marker_said(bytes),
                _ => Err(format!("no record {case}")),
            }
        }
    }
}
