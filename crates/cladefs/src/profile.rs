// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: boot profiles of base images (TASK-38). The forks of a base image read much the same
//! of it as they start: what their guests' kernels, Fxfs and services read at boot. A fork's store
//! records the units of the base's extents its page-ins read in its first [`WINDOW`], in the order
//! they were first read, up to [`MOST`] bytes, and writes them, once, as the base's profile, in the
//! store's directory (`profiles/<base cell>`): the first fork's, so that a fork whose start the
//! profile sped up does not replace it with less. A fork opened where the profile is reads its
//! units into the extent cache, behind it, as its guest boots ([`replay`]), so that its first reads
//! find them in memory, as a fork of a base another VM read already does.

use crate::ExtentId;
use base32_id::Short;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long after a fork's store opens its page-ins are recorded.
pub(crate) const WINDOW: Duration = Duration::from_secs(60);

/// The most bytes of units a profile holds.
pub(crate) const MOST: u64 = 1 << 30;

/// The first line of a profile; then a line a unit, `CELL SEQUENCE OFFSET LEN`: its extent's ID
/// in base 32 (`base32_id`), as IDs are written, and the range in hexadecimal. 2: IDs in base 32,
/// which 1 wrote in hexadecimal (TASK-504); 1 is refused.
const HEADER: &str = "cladefs boot profile 2";

/// A unit of an extent a page-in read: its extent, and its offset and length in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Unit {
    pub extent: ExtentId,
    pub offset: u64,
    pub len: u64,
}

/// The path of the profile of the base `cell` of the store whose directory is `dir`.
pub(crate) fn path(dir: &Path, cell: u64) -> PathBuf {
    dir.join("profiles").join(base32_id::Padded(cell).to_string())
}

/// The units of the profile at `path`, in the order they were first read.
pub(crate) fn read(path: &Path) -> io::Result<Vec<Unit>> {
    let text = host_fs::read_to_string(path)?;
    let mut lines = text.lines();
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    if lines.next() != Some(HEADER) {
        return Err(invalid("not a boot profile"));
    }
    let mut units = Vec::new();
    let range = |field: &str| u64::from_str_radix(field, 16).ok();
    let unit = |line: &str| match line.split(' ').collect::<Vec<_>>()[..] {
        [cell, sequence, offset, len] => Some(Unit {
            extent: ExtentId {
                cell: base32_id::parse(cell)?,
                sequence: base32_id::parse(sequence)?,
            },
            offset: range(offset)?,
            len: range(len)?,
        }),
        _ => None,
    };
    for line in lines {
        units.push(unit(line).ok_or_else(|| invalid("a bad line"))?);
    }
    Ok(units)
}

/// Records a fork's page-ins of its base's extents, for its base's profile at `path`.
pub(crate) struct Recorder {
    path: PathBuf,
    /// The fork's own cell, whose extents no other fork holds.
    own: u64,
    until: Instant,
    units: Mutex<Recorded>,
    written: AtomicBool,
}

#[derive(Default)]
struct Recorded {
    units: Vec<Unit>,
    seen: HashSet<(ExtentId, u64)>,
    bytes: u64,
}

impl Recorder {
    /// A recorder of the page-ins of the extents the fork of cell `own` inherited, for its base's
    /// profile at `path`, from now until [`WINDOW`] after.
    pub(crate) fn new(path: PathBuf, own: u64) -> Self {
        Self {
            path,
            own,
            until: Instant::now() + WINDOW,
            units: Mutex::default(),
            written: AtomicBool::new(false),
        }
    }

    /// Records that a page-in read `len` bytes at `offset` in `extent`: of a unit of an inherited
    /// extent, within the window and [`MOST`].
    pub(crate) fn record(&self, extent: ExtentId, offset: u64, len: u64) {
        // The fork's own extents, which no other fork holds, are no part of the base's profile.
        if extent.cell == self.own {
            return;
        }
        if Instant::now() > self.until || self.written.load(Ordering::Relaxed) {
            return;
        }
        let mut recorded = self.units.lock().unwrap();
        if recorded.bytes + len > MOST || !recorded.seen.insert((extent, offset)) {
            return;
        }
        recorded.bytes += len;
        recorded.units.push(Unit { extent, offset, len });
    }

    /// Writes the profile, once, unless one is there already, as what a fork recorded first.
    pub(crate) fn write(&self) -> io::Result<()> {
        if self.written.swap(true, Ordering::Relaxed) || host_fs::exists(&self.path) {
            return Ok(());
        }
        let recorded = self.units.lock().unwrap();
        if recorded.units.is_empty() {
            return Ok(());
        }
        let mut text = format!("{HEADER}\n");
        for unit in &recorded.units {
            let Unit { extent, offset, len } = unit;
            let (cell, sequence) = (Short(extent.cell), Short(extent.sequence));
            text.push_str(&format!("{cell} {sequence} {offset:x} {len:x}\n"));
        }
        // cladefs: in the base's directory, which it makes nothing of if it went: a fork's directory
        // is its own, and outlives its base's (TASK-165.7).
        let dir = self.path.parent().expect("a profile is in a directory");
        match host_fs::create_dir(dir) {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            created => created?,
        }
        // Whole or not at all, and durably, as every record beside a store (TASK-160.13).
        let name = self.path.file_name().and_then(|name| name.to_str());
        crate::cell_dir::write(dir, name.expect("a profile is named by its cell"), &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile reads back as recorded, its IDs in base 32 and its ranges in hexadecimal, under
    /// the base's cell padded; a fork's own extents are left out; a line of another format, as
    /// profile 1's hexadecimal IDs, is refused.
    #[test]
    fn profiles_read_back_as_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let profile = path(dir.path(), 42);
        assert_eq!(profile, dir.path().join("profiles/000000000001a"));
        let recorder = Recorder::new(profile.clone(), 9);
        let inherited = ExtentId { cell: 42, sequence: u64::MAX };
        recorder.record(inherited, 0x3000, 0x1000);
        recorder.record(ExtentId { cell: 9, sequence: 1 }, 0, 0x1000);
        recorder.write().unwrap();
        let text = host_fs::read_to_string(&profile).unwrap();
        assert_eq!(text, "cladefs boot profile 2\n1a fzzzzzzzzzzzz 3000 1000\n");
        let expected = vec![Unit { extent: inherited, offset: 0x3000, len: 0x1000 }];
        assert_eq!(read(&profile).unwrap(), expected);
        for wrong in
            ["cladefs boot profile 1\n2a 1 3000 1000\n", "cladefs boot profile 2\n2A 1 0 1\n"]
        {
            host_fs::write(&profile, wrong).unwrap();
            assert_eq!(read(&profile).unwrap_err().kind(), io::ErrorKind::InvalidData, "{wrong}");
        }
    }
}
