// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: traces of what VMs read of their cells (TASK-702.1), the evidence a prediction of a
//! version's reads is to be chosen from. With `CLADEFSD_READ_TRACES` naming a directory, each VM
//! process's connection records, from its cell's opening and for [`WINDOW`], every `READ` its guest
//! makes, every `MAP` of an extent into its DAX window, and every fault of the window the daemon
//! resolves: when, since the opening, what of which extent, and how long the guest waited for it. A
//! `MAP` is the guest's mapping of a file's range it touches; a fault, a VM's first touch of a page
//! mapped that is not in memory. A warm host installs a `MAP`'s pages in memory already, and its VM
//! faults on none of them, so the `MAP`s say what a VM read on a warm host, and the faults which of
//! the pages it touched on a cold one; a page-in would say neither what a warm host's VM read nor
//! which reads were read-ahead's.
//!
//! A trace is written once, as its connection goes (`server::finish_connection`, off the runtime
//! through the daemon's seam), to `<cell>-<opened>.reads` in the directory, the cell in base 32
//! (`base32_id`) and the opening in milliseconds since the Unix epoch: the first line [`HEADER`],
//! the second `cell CELL opened MS`, then a line a read, `AT_US KIND EXTENT_CELL EXTENT_SEQUENCE
//! EXTENT_LEN OFFSET LEN WAITED_US`: times in decimal microseconds, the extent's ID in base 32, its
//! length (0 where the cell can't say, as for an attached base's) and the range read in
//! hexadecimal, and the kind `read` (a `READ`), `map` (a `MAP`) or `fault` (a fault, a read's, or
//! a write's its VM process copies the page for, doc-26). `scripts/read_prediction.py` reads them.

use super::Daemon;
use crate::ExtentId;
use base32_id::Short;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// How long after its cell's opening a VM's reads are recorded.
pub(super) const WINDOW: Duration = Duration::from_secs(120);

/// The most reads a trace holds.
const MOST: usize = 1 << 20;

/// The first line of a trace.
pub(super) const HEADER: &str = "cladefs read trace 1";

/// cladefs: where the environment says traces go, `CLADEFSD_READ_TRACES`, if it names a directory
/// (empty, it names none): kept for `daemon`'s connections, or why it can't be.
pub(super) fn configure_on(daemon: &Daemon) -> Result<(), String> {
    let dir = node_local::env::var_os("CLADEFSD_READ_TRACES").filter(|dir| !dir.is_empty());
    if let Some(dir) = &dir
        && !Path::new(dir).is_dir()
    {
        return Err(format!("CLADEFSD_READ_TRACES names no directory: {dir:?}"));
    }
    // Set once per daemon: a second call keeps the first configuration.
    let _ = daemon.read_traces.set(dir.map(PathBuf::from));
    Ok(())
}

/// cladefs: [`configure_on`] for the process's default daemon (tests).
#[cfg(not(madsim))]
#[cfg(test)]
pub(super) fn configure() -> Result<(), String> {
    configure_on(Daemon::process())
}

/// What a guest's read was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// A `READ`.
    Read,
    /// A read's fault on its window.
    Fault,
    /// A `MAP` of an extent's bytes into its window: the guest's mapping of a file's range it
    /// touches, whose pages a warm host installs without a fault.
    Map,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Fault => "fault",
            Kind::Map => "map",
        }
    }
}

/// A read a trace records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Read {
    /// When it was made, since the cell's opening.
    pub at: Duration,
    pub kind: Kind,
    pub extent: ExtentId,
    /// The extent's length, 0 if the cell can't say.
    pub extent_len: u64,
    pub offset: u64,
    pub len: u64,
    /// How long the guest waited for it.
    pub waited: Duration,
}

/// A VM's reads of its cell, from its cell's opening, written once as its connection goes.
pub(super) struct Trace {
    file: PathBuf,
    cell: u64,
    opened_ms: u128,
    opened: Instant,
    reads: Mutex<Vec<Read>>,
}

impl Trace {
    /// A trace of the reads of a VM whose cell of `cell` opened now, if `daemon` writes traces.
    pub(super) fn opened(daemon: &Daemon, cell: u64) -> Option<Self> {
        let dir = daemon.read_traces.get()?.as_ref()?;
        Some(Self::in_dir(dir, cell))
    }

    /// A trace of the reads of a VM whose cell of `cell` opened now, written in `dir`.
    fn in_dir(dir: &Path, cell: u64) -> Self {
        let opened_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let file = dir.join(format!("{}-{opened_ms}.reads", base32_id::Padded(cell)));
        Self { file, cell, opened_ms, opened: Instant::now(), reads: Mutex::default() }
    }

    /// When the cell opened.
    #[cfg(test)]
    fn opened_at(&self) -> Instant {
        self.opened
    }

    /// The file it is written to.
    pub(super) fn file(&self) -> &Path {
        &self.file
    }

    /// Records a read the guest began at `began`, of `len` bytes at `offset` in `extent`, of
    /// `extent_len` bytes, done now: within [`WINDOW`] of the opening and [`MOST`] reads.
    pub(super) fn record(
        &self,
        kind: Kind,
        began: Instant,
        extent: ExtentId,
        extent_len: u64,
        offset: u64,
        len: u64,
    ) {
        let at = began.saturating_duration_since(self.opened);
        if at > WINDOW {
            return;
        }
        let waited = began.elapsed();
        let mut reads = self.reads.lock().unwrap();
        if reads.len() < MOST {
            reads.push(Read { at, kind, extent, extent_len, offset, len, waited });
        }
    }

    /// The trace's text.
    pub(super) fn text(&self) -> String {
        let reads = self.reads.lock().unwrap();
        let mut text = format!("{HEADER}\ncell {} opened {}\n", Short(self.cell), self.opened_ms);
        for read in reads.iter() {
            let Read { at, kind, extent, extent_len, offset, len, waited } = read;
            let _ = writeln!(
                text,
                "{} {} {} {} {extent_len:x} {offset:x} {len:x} {}",
                at.as_micros(),
                kind.name(),
                Short(extent.cell),
                Short(extent.sequence),
                waited.as_micros(),
            );
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trace's text: its header, its cell and opening, and a line a read, in the order recorded,
    /// IDs in base 32, ranges in hexadecimal, times in microseconds; reads past the window left
    /// out.
    #[test]
    fn traces_say_each_read() {
        let dir = tempfile::tempdir().unwrap();
        let trace = Trace::in_dir(dir.path(), 42);
        assert!(trace.file.starts_with(dir.path()));
        let name = trace.file.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with("000000000001a-") && name.ends_with(".reads"), "{name}");
        let extent = ExtentId { cell: 7, sequence: 33 };
        let opened = trace.opened_at();
        trace.record(
            Kind::Fault,
            opened + Duration::from_millis(5),
            extent,
            0x30_0000,
            0x1000,
            0x1000,
        );
        trace.record(Kind::Read, opened + Duration::from_millis(9), extent, 0x30_0000, 0, 0x200);
        trace.record(Kind::Map, opened + WINDOW + Duration::from_secs(1), extent, 1, 0, 1);
        let text = trace.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], HEADER);
        assert_eq!(lines[1], format!("cell 1a opened {}", trace.opened_ms));
        let fields = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let (fault, read) = (fields(lines[2]), fields(lines[3]));
        assert_eq!(lines.len(), 4, "{text}");
        assert_eq!(&fault[..7], ["5000", "fault", "7", "11", "300000", "1000", "1000"]);
        assert_eq!(&read[..7], ["9000", "read", "7", "11", "300000", "0", "200"]);
    }
}
