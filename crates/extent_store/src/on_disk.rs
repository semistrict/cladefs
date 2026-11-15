// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what of a store's directory is in the clear, as a reader holding no key tells
//! (TASK-411.15): for tests that a sealed store leaves none of its files so, the guest tests'
//! harness's among them, which knows nothing of a store's directory but this.
//!
//! A sealed store's framed files, its closed segments' and its large extents', say they are sealed
//! in their trailers (`extent_frame::Seal`), and its segments' logs are headed
//! [`SEALED_LOG_MAGIC`]: a framed file whose trailer says [`Seal::Clear`], and a log headed
//! [`LOG_MAGIC`], are in the clear. Raw files (open segments, open large extents) say nothing:
//! encrypted, they are as random bytes (TASK-411.5), which only a search for what was written in
//! them tells from the clear.

use crate::dir::{Dir, File as _, not_found, read_file};
use crate::layout::{DESCRIPTION, EXTENTS, LOGS, SEGMENTS, SPARE, StoreFile, parse_number};
use crate::wal::{LOG_MAGIC, SEALED_LOG_MAGIC};
use extent_frame::segment::Branch;
use extent_frame::{Frame, Seal, TRAILER};
use std::io;

/// What [`on_disk`] found of a store's directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnDisk {
    /// Its branch, as its description names it.
    pub branch: Branch,
    /// The bucket its branch is kept in, as its description names it, if it does.
    pub bucket: Option<String>,
    /// How many of its framed files and logs it read, each by its trailer or header.
    pub read: usize,
    /// Those in the clear.
    pub clear: Vec<ClearFile>,
}

/// A file of a store's directory in the clear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearFile {
    /// Its path in the store's directory.
    pub path: String,
    /// What it is: a framed segment, a framed extent, or a segment's log.
    pub what: &'static str,
}

/// What of the store in `dir` is in the clear: its framed segments and extents whose trailers say
/// so, and its segments' logs headed so; or none if `dir` holds no store, no description. A file
/// gone as it is read, as a store being served renames and removes its files, is skipped, and so
/// is a log too short to have its header yet; a framed file whose trailer doesn't parse is an
/// error, naming it.
pub fn on_disk(dir: &impl Dir) -> io::Result<Option<OnDisk>> {
    let description = match dir.open(DESCRIPTION, false).and_then(|file| read_file(&file)) {
        Ok(description) => description,
        Err(err) if not_found(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let description = String::from_utf8_lossy(&description);
    let store =
        StoreFile::parse(&description).map_err(|err| io::Error::other(format!("{err:#}")))?;
    let mut found =
        OnDisk { branch: store.branch, bucket: store.bucket, read: 0, clear: Vec::new() };
    for (kind, what) in [(SEGMENTS, "a framed segment"), (EXTENTS, "a framed extent")] {
        for path in files(dir, kind)? {
            let Some(seal) = seal(dir, &path)? else { continue };
            found.read += 1;
            if seal == Seal::Clear {
                found.clear.push(ClearFile { path, what });
            }
        }
    }
    for path in files(dir, LOGS)? {
        let mut magic = [0; SEALED_LOG_MAGIC.len()];
        match dir.open(&path, false).and_then(|file| file.read_at(&mut magic, 0)) {
            Ok(()) => {}
            Err(err) if not_found(&err) || err.kind() == io::ErrorKind::UnexpectedEof => continue,
            Err(err) => return Err(err),
        }
        found.read += 1;
        if &magic == LOG_MAGIC {
            found.clear.push(ClearFile { path, what: "a segment's log" });
        }
    }
    Ok(Some(found))
}

/// The paths of the files of each branch's directory of `kind` in `dir` (`<kind>/<branch>/<file>`)
/// named by numbers, as segments', extents' and logs' are, or spare logs (`spare-<number>`): not a
/// framed file being made durable (`framing-`), whole only once renamed.
fn files(dir: &impl Dir, kind: &str) -> io::Result<Vec<String>> {
    let numbered = |name: &str| parse_number(name.strip_prefix(SPARE).unwrap_or(name)).is_some();
    let mut files = Vec::new();
    for branch in listed(dir, kind)? {
        if parse_number(&branch).is_none() {
            continue;
        }
        let branch = format!("{kind}/{branch}");
        let names = listed(dir, &branch)?;
        files.extend(
            names.into_iter().filter(|name| numbered(name)).map(|name| format!("{branch}/{name}")),
        );
    }
    files.sort();
    Ok(files)
}

/// The names in `dir`'s directory `path`, none if it is gone or was never made.
fn listed(dir: &impl Dir, path: &str) -> io::Result<Vec<String>> {
    match dir.list(path) {
        Err(err) if not_found(&err) => Ok(Vec::new()),
        listed => listed,
    }
}

/// The seal the trailer of the framed file at `path` in `dir` says, none if it is gone.
fn seal(dir: &impl Dir, path: &str) -> io::Result<Option<Seal>> {
    let file = match dir.open(path, false) {
        Ok(file) => file,
        Err(err) if not_found(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let size = file.size()?;
    let at = size.checked_sub(TRAILER).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{path}: {size} bytes, no trailer"))
    })?;
    let mut trailer = vec![0; TRAILER as usize];
    file.read_at(&mut trailer, at)?;
    let frame = Frame::of_trailer(&trailer)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{path}: {err}")))?;
    Ok(Some(frame.seal))
}
