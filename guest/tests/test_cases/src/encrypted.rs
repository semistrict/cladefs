// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: whether a run's cells are encrypted (TASK-411.15, doc-16), as the guest tests run
//! encrypted only: scripts/test-e2e.sh gives every process of the run a KMS holding a KEK for every
//! tenant the cases use (`CLADEFS_KMS`, `CLADEFS_E2E_ENCRYPTED`), and this checks, holding no key,
//! what their cells left:
//!
//! - **The buckets**: every cell has a wrapped KEK (or was shredded); every live cell
//!   that wrote objects has a DEK; every framed object (segments,
//!   own extents, checkpoints, moves) says in its trailer it is encrypted or authenticated, none in
//!   the clear.
//! - **The cells' directories**: no framed file in the clear, no segment's log headed so
//!   (`local_cell::on_disk`).
//! - **Known plaintext**: no file of the directories checked, cells, buckets, part caches and all,
//!   nor object of the buckets, holds a run of [`crate::canary`]'s bytes, which the cases write
//!   into their guests' filesystems.
//!
//! The runner checks each case's directory once it passes (`runner test`, with
//! `CLADEFS_E2E_ENCRYPTED`), and test-e2e.sh the run's own bucket and part cache at its end
//! (`runner encrypted`).

use anyhow::Context as _;
use bucket_cell::Bucket as _;
use bucket_cell::layout::{self, CellId};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// The variable scripts/test-e2e.sh sets when the run's cells are to be encrypted, which the runner
/// checks each case's are.
pub const ENCRYPTED: &str = "CLADEFS_E2E_ENCRYPTED";

/// The tenants a run's KMS holds a KEK for ([`kms`]): the default tenant, 0, whose cells are the
/// daemon's own socket's and those the cases make in processes of their own (cladefs's `Storage`,
/// cladefsctl's `create`), and granted ones, of IDs up to 15 (fxfs-guest-migrate-tenant-dax's 1-4).
pub const TENANTS: std::ops::RangeInclusive<u64> = 0..=15;

/// Whether this process has a KMS to encrypt and open cells under (`CLADEFS_KMS`,
/// `CLADEFS_MASTER_KEY`): what the runner asks before it runs any case, refusing to start with
/// none, naming both, as every process that opens cells does (TASK-411.28;
/// `cladefs::bucket::require_kms`).
pub fn require_kms() -> anyhow::Result<()> {
    cladefs::bucket::require_kms().map_err(anyhow::Error::msg)
}

/// Makes a local KMS in `dir` (`CLADEFS_KMS=local://DIR`, extent_keys' `LocalKms`), holding a KEK
/// for each of [`TENANTS`]: those it holds already are kept.
pub fn kms(dir: &Path) -> anyhow::Result<()> {
    let kms = extent_keys::kms::LocalKms::open(dir).with_context(|| dir.display().to_string())?;
    for tenant in TENANTS {
        match kms.create(tenant) {
            Ok(()) | Err(extent_keys::kms::Error::Exists { .. }) => {}
            Err(err) => return Err(err).with_context(|| format!("tenant {tenant}'s KEK")),
        }
    }
    Ok(())
}

/// What a check found: what it looked at, and what of it is in the clear.
#[derive(Debug, Default)]
pub struct Report {
    pub cell_dirs: usize,
    pub cells: usize,
    pub objects: usize,
    pub files: usize,
    pub bytes: u64,
    /// The buckets its cells named that it can't read: in a daemon's memory (`memory:///`).
    pub unread: BTreeSet<String>,
    /// What it found in the clear, a line each.
    pub clear: Vec<String>,
}

impl Report {
    /// What it looked at, on a line.
    pub fn summary(&self) -> String {
        let Self { cell_dirs, cells, objects, files, bytes, unread, clear } = self;
        let mut said = format!(
            "{cell_dirs} cell directories, {cells} cells, {objects} objects, {files} files \
             ({} MiB) read: {} in the clear",
            bytes >> 20,
            clear.len()
        );
        if !unread.is_empty() {
            let unread: Vec<&str> = unread.iter().map(String::as_str).collect();
            said += &format!("; not read, in a daemon's memory: {}", unread.join(" "));
        }
        said
    }

    /// Ok if nothing was found in the clear; the first things found otherwise.
    pub fn verdict(&self) -> anyhow::Result<()> {
        if self.clear.is_empty() {
            return Ok(());
        }
        const SHOWN: usize = 20;
        let mut said = format!("not encrypted: {}", self.summary());
        for line in self.clear.iter().take(SHOWN) {
            said += &format!("\n  {line}");
        }
        if self.clear.len() > SHOWN {
            said += &format!("\n  ... and {} more", self.clear.len() - SHOWN);
        }
        anyhow::bail!(said)
    }
}

/// Checks a case's directory `dir`: every cell in it, every bucket in it, and the cells of other
/// buckets its cells are of (the run's own, whose other cells are other cases'), and every file
/// under it for known plaintext.
pub fn case(dir: &Path) -> anyhow::Result<Report> {
    run(&[], &[dir.to_owned()])
}

/// Checks the buckets `buckets` (URLs) whole, and every cell and file under `dirs`, and the
/// buckets and cells they name, as [`case`] does: the run's own bucket and part cache.
pub fn run(buckets: &[String], dirs: &[PathBuf]) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let mut scope = Scope::default();
    for url in buckets {
        scope.whole.insert(url.clone());
    }
    let finder = Finder::new();
    for dir in dirs {
        walk(dir, &finder, &mut report, &mut scope)?;
    }
    buckets_of(&scope, &finder, &mut report)?;
    Ok(report)
}

/// What of which buckets a check reads: some whole, of others some cells.
#[derive(Default)]
struct Scope {
    whole: BTreeSet<String>,
    cells: BTreeMap<String, BTreeSet<CellId>>,
}

/// Walks `dir`: every regular file searched for known plaintext; each cell's directory checked
/// (`local_cell::on_disk`), its cell in the bucket it names put in `scope`; each
/// file bucket, a directory holding the local bucket's locks, put in `scope` whole.
fn walk(dir: &Path, finder: &Finder, report: &mut Report, scope: &mut Scope) -> anyhow::Result<()> {
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| dir.display().to_string()),
        };
        let mut bucket = false;
        for entry in entries {
            let entry = entry.with_context(|| dir.display().to_string())?;
            let path = entry.path();
            let kind = entry.file_type().with_context(|| path.display().to_string())?;
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                bucket |= name.starts_with(bucket_cell::local::LOCK);
                if name == "store" {
                    cell_dir(&dir, report, scope)?;
                }
                search_file(&path, finder, report)?;
            }
        }
        if bucket {
            scope.whole.insert(format!("file://{}/", dir.display()));
        }
    }
    Ok(())
}

/// Checks the cell directory `dir`, if it holds a cell (its description, `store`, says so).
fn cell_dir(dir: &Path, report: &mut Report, scope: &mut Scope) -> anyhow::Result<()> {
    let host = local_cell::HostDir::new(dir).with_context(|| dir.display().to_string())?;
    let Some(on_disk) = local_cell::on_disk(&host).with_context(|| dir.display().to_string())?
    else {
        return Ok(());
    };
    report.cell_dirs += 1;
    for file in on_disk.clear {
        report.clear.push(format!("{}/{}: {}, in the clear", dir.display(), file.path, file.what));
    }
    match on_disk.bucket {
        Some(url) if url.starts_with("memory:") => _ = report.unread.insert(url),
        Some(url) => _ = scope.cells.entry(url).or_default().insert(on_disk.cell),
        None => {}
    }
    Ok(())
}

/// Checks the buckets and cells `scope` names.
fn buckets_of(scope: &Scope, finder: &Finder, report: &mut Report) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("a runtime for the buckets")?;
    for url in &scope.whole {
        runtime.block_on(bucket(url, "", finder, report))?;
    }
    for (url, cells) in &scope.cells {
        if scope.whole.contains(url) {
            continue;
        }
        for cell in cells {
            runtime.block_on(bucket(url, &cell.prefix(), finder, report))?;
        }
    }
    Ok(())
}

/// Checks the objects of the bucket at `url` under `prefix`.
async fn bucket(
    url: &str,
    prefix: &str,
    finder: &Finder,
    report: &mut Report,
) -> anyhow::Result<()> {
    let object_bucket =
        bucket_cell::object_bucket::ObjectBucket::from_url(url).map_err(anyhow::Error::msg)?;
    let listed =
        object_bucket.list(prefix).await.with_context(|| format!("listing {url}{prefix}"))?;
    let written: BTreeSet<_> = listed
        .iter()
        .filter_map(|meta| match layout::kind(&meta.key) {
            layout::Kind::Segment { cell, .. }
            | layout::Kind::Extent { cell, .. }
            | layout::Kind::Checkpoint { cell, .. } => Some(cell),
            _ => None,
        })
        .collect();
    for meta in &listed {
        let key = meta.key.as_str();
        let at = || format!("{url} {key}");
        // A refusal fails the check (board 6800).
        let got = bucket_cell::bucket::get(&object_bucket, key).await;
        let Some(object) = got.with_context(|| format!("reading {url}"))? else {
            continue; // Deleted since it was listed.
        };
        let framed = match layout::kind(key) {
            layout::Kind::Segment { .. }
            | layout::Kind::Extent { .. }
            | layout::Kind::Checkpoint { .. }
            | layout::Kind::Moves { .. } => true,
            layout::Kind::Head { cell, slot } => {
                // Each of a head's slots holds a whole head; the cell counts once.
                report.cells += usize::from(slot == 0);
                let head = layout::Head::decode(cell, &object.bytes).with_context(at)?;
                // A mark failing a half-made head holds no keys and no data.
                let keyed = !head.failed;
                if keyed && head.keks.is_empty() && !head.shredded {
                    report.clear.push(format!("{}: cell {cell}: no KEK, in the clear", at()));
                }
                if keyed && head.deks.is_empty() && !head.deleted() && written.contains(&cell) {
                    report
                        .clear
                        .push(format!("{}: cell {cell} wrote objects, its head has no DEK", at()));
                }
                false
            }
            _ => false,
        };
        report.objects += 1;
        report.bytes += object.bytes.len() as u64;
        if framed {
            if extent_frame::in_the_clear(&object.bytes) {
                report.clear.push(format!("{}: framed in the clear", at()));
            } else if let Err(err) = extent_frame::Frame::of(&object.bytes) {
                report.clear.push(format!("{}: not a frame: {err}", at()));
            }
        }
        if let Some(offset) = finder.find(&object.bytes) {
            report.clear.push(format!("{}: holds known plaintext at {offset}", at()));
        }
    }
    Ok(())
}

/// Reads the file at `path` for known plaintext.
fn search_file(path: &Path, finder: &Finder, report: &mut Report) -> anyhow::Result<()> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| path.display().to_string()),
    };
    report.files += 1;
    let mut chunk = vec![0; CHUNK];
    let mut at = 0u64;
    loop {
        let read = read_full(&mut file, &mut chunk).with_context(|| path.display().to_string())?;
        if read == 0 {
            return Ok(());
        }
        report.bytes += read as u64;
        if let Some(offset) = finder.find(&chunk[..read]) {
            let offset = at + offset as u64;
            report.clear.push(format!("{}: holds known plaintext at {offset}", path.display()));
            return Ok(());
        }
        at += read as u64;
    }
}

/// Reads into `buf` until it is full or the file ends: how many bytes.
fn read_full(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(more) => read += more,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(read)
}

/// The bytes a file is read in, a multiple of [`STRIDE`].
const CHUNK: usize = 4 << 20;

/// How far apart the places are that [`Finder`] looks at: any run of the canary's bytes this long
/// and [`WORD`] more is found, wherever it is.
const STRIDE: usize = 512;

/// The bytes [`Finder`] looks up at each place, then as many after them to be sure.
const WORD: usize = 8;

/// Finds runs of [`crate::canary`]'s bytes in other bytes, at any offset: every [`WORD`] bytes of
/// the canary, at each of its offsets, by their value, and the canary's bytes; a place
/// [`STRIDE`] bytes apart looked up, and the [`WORD`] after it compared. A run of
/// `STRIDE + 2 * WORD` bytes or more has one of those places in it. The canary is random: a word
/// found by chance in other bytes is one in 2^44 a place, and two in a row none.
struct Finder {
    words: HashMap<u64, u32>,
    canary: &'static [u8],
}

impl Finder {
    fn new() -> Self {
        let canary = crate::canary::bytes();
        let mut words = HashMap::with_capacity(canary.len());
        for offset in 0..=canary.len() - WORD {
            words.entry(word(&canary[offset..])).or_insert(offset as u32);
        }
        Self { words, canary }
    }

    /// The offset in `bytes` of a run of the canary's, if they hold one.
    fn find(&self, bytes: &[u8]) -> Option<usize> {
        (0..bytes.len().saturating_sub(2 * WORD - 1)).step_by(STRIDE).find(|&at| {
            self.words.get(&word(&bytes[at..])).is_some_and(|&offset| {
                let next = offset as usize + WORD;
                self.canary.get(next..next + WORD) == Some(&bytes[at + WORD..at + 2 * WORD])
            })
        })
    }
}

/// The first [`WORD`] bytes of `bytes`, as a number.
fn word(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..WORD].try_into().expect("a word's bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs of the canary are found at any offset, in any bytes, once long enough; other bytes,
    /// and runs too short, are not.
    #[test]
    fn known_plaintext_is_found_wherever_it_is() {
        let finder = Finder::new();
        let canary = crate::canary::bytes();
        let noise = crate::canary::stream(3 * STRIDE).iter().map(|byte| !byte).collect::<Vec<_>>();
        assert_eq!(finder.find(&noise), None);
        for (at, from, len) in [(0, 0, 2 * WORD), (5, 77, STRIDE + 2 * WORD), (1000, 4096, 4096)] {
            let mut bytes = noise.clone();
            bytes.splice(at..at, canary[from..from + len].iter().copied());
            let found = finder.find(&bytes);
            assert!(
                found.is_some_and(|found| (at..at + len).contains(&found)),
                "{at} {from} {len}"
            );
        }
        // Shorter than a stride and two words, between places: not found.
        let mut bytes = noise.clone();
        bytes.splice(1..1, canary[..STRIDE].iter().copied());
        assert_eq!(finder.find(&bytes), None);
    }

    /// A directory of files: one holding a run of the canary at an odd offset is named, with its
    /// offset; the others, and a cell-less directory, find nothing.
    #[test]
    fn files_holding_known_plaintext_are_named() {
        let dir = tempdir::TempDir::new("encrypted").unwrap();
        let clean: Vec<u8> = crate::canary::stream(10_000).iter().map(|byte| !byte).collect();
        std::fs::write(dir.path().join("clean"), clean).unwrap();
        let mut held = vec![0u8; CHUNK - 3];
        held.extend_from_slice(&crate::canary::bytes()[4096..8192]);
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/held"), &held).unwrap();
        let report = case(dir.path()).unwrap();
        assert_eq!((report.cell_dirs, report.files), (0, 2));
        let held = dir.path().join("sub/held");
        // Found at the first place in it, the second chunk's first byte.
        assert_eq!(report.clear, [format!("{}: holds known plaintext at {CHUNK}", held.display())]);
        assert!(report.verdict().is_err());
    }
}
