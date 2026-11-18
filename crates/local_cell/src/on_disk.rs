// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what of a store's directory is in the clear, as a reader holding no key tells
//! (TASK-411.15): for tests that an encrypted store leaves none of its files so, the guest tests'
//! harness's among them, which knows nothing of a store's directory but this.
//!
//! A store's framed files, its closed segments' and its large extents', say they are encrypted in
//! their trailers (`extent_frame::Encryption`), and its segments' logs are headed [`LOG_MAGIC`]: a framed
//! file whose trailer's encryption is of kind 0 (`extent_frame::in_the_clear`), and a log headed
//! [`CLEAR_LOG_MAGIC`], are in the clear, as stores wrote them before every store was encrypted
//! (TASK-411.28), which no reader reads now. Raw files (open segments, open large extents) say nothing:
//! encrypted, they are as random bytes (TASK-411.5), which only a search for what was written in
//! them tells from the clear.

use crate::dir::{Dir, File as _, not_found, read_file};
use crate::layout::{CellFile, DESCRIPTION, EXTENTS, LOGS, SEGMENTS, SPARE, parse_number};
use crate::wal::LOG_MAGIC;
use extent_frame::segment::CellId;
use extent_frame::{Frame, TRAILER};
use std::io;

/// What a segment's log in the clear was headed (TASK-411.5), before every store was encrypted
/// (TASK-411.28): no segment's log now.
pub const CLEAR_LOG_MAGIC: &[u8; 8] = b"FXLOG\0\0\x02";

/// What [`on_disk`] found of a store's directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnDisk {
    /// Its cell, as its description names it.
    pub cell: CellId,
    /// The bucket its cell is kept in, as its description names it, if it does.
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
        CellFile::parse(&description).map_err(|err| io::Error::other(format!("{err:#}")))?;
    let mut found = OnDisk { cell: store.cell, bucket: store.bucket, read: 0, clear: Vec::new() };
    for (kind, what) in [(SEGMENTS, "a framed segment"), (EXTENTS, "a framed extent")] {
        for path in files(dir, kind)? {
            let Some(clear) = in_the_clear(dir, &path)? else { continue };
            found.read += 1;
            if clear {
                found.clear.push(ClearFile { path, what });
            }
        }
    }
    for path in files(dir, LOGS)? {
        let mut magic = [0; LOG_MAGIC.len()];
        match dir.open(&path, false).and_then(|file| file.read_at(&mut magic, 0)) {
            Ok(()) => {}
            Err(err) if not_found(&err) || err.kind() == io::ErrorKind::UnexpectedEof => continue,
            Err(err) => return Err(err),
        }
        found.read += 1;
        if &magic == CLEAR_LOG_MAGIC {
            found.clear.push(ClearFile { path, what: "a segment's log" });
        }
    }
    Ok(Some(found))
}

/// The paths of the files of each cell's directory of `kind` in `dir` (`<kind>/<cell>/<file>`)
/// named by numbers, as segments', extents' and logs' are, or spare logs (`spare-<number>`): not a
/// framed file being made durable (`framing-`), whole only once renamed.
fn files(dir: &impl Dir, kind: &str) -> io::Result<Vec<String>> {
    let numbered = |name: &str| parse_number(name.strip_prefix(SPARE).unwrap_or(name)).is_some();
    let mut files = Vec::new();
    for cell in listed(dir, kind)? {
        if parse_number(&cell).is_none() {
            continue;
        }
        let cell = format!("{kind}/{cell}");
        let names = listed(dir, &cell)?;
        files.extend(
            names.into_iter().filter(|name| numbered(name)).map(|name| format!("{cell}/{name}")),
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

/// Whether the trailer of the framed file at `path` in `dir` says it is in the clear, none if it is
/// gone; one that doesn't parse otherwise is an error, naming it.
fn in_the_clear(dir: &impl Dir, path: &str) -> io::Result<Option<bool>> {
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
    if extent_frame::in_the_clear(&trailer) {
        return Ok(Some(true));
    }
    Frame::of_trailer(&trailer)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{path}: {err}")))?;
    Ok(Some(false))
}
