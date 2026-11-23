// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The golden corpus's test (doc-28): what proves a frozen family is read at every version.
//!
//! `formats/<slug>/<version>/` at the repo's root holds, for each version of the family's range:
//!
//! - `<case>.bin`: an object as that version's writer wrote it;
//! - `<case>.txt`: what it decodes into, as that version's types render it (`{:#?}`), which
//!   never changes, as those types never do;
//! - `fingerprint`, for a family serialized by serde: that version's types' layout
//!   (`fprint::TypeFingerprint`), so that editing a frozen type fails.
//!
//! [`check`] runs it. Cases are written once, by the build whose version they are, when
//! `CLADEFS_FORMATS_WRITE=1` and they are missing; never rewritten. `scripts/format-freeze.py`
//! refuses a commit changing or deleting any of them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::Format;

/// What a family's corpus test needs of it.
pub trait Family {
    const FORMAT: Format;

    /// The objects this build writes, each named, of [`Format::current`]: written by the
    /// family's own writers, deterministically (fixed keys, salts and times), as each current
    /// case must still be what this build writes. A writer that writes otherwise has changed the
    /// format, which is a new version.
    fn write() -> Vec<(&'static str, Vec<u8>)>;

    /// `bytes`, the case `case` of `version`, decoded by that version's decoder into that
    /// version's types, rendered by `{:#?}`: what the case's `.txt` holds. `case` names its
    /// object's kind where the family has several (`cell-receiving`, a `cell` note).
    fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String>;

    /// `bytes`, the case `case`, read by this build's reader, as it reads what it finds: finding
    /// the version in them, and converting to the current types. This proves an old version is
    /// read, not that its conversion keeps what it says: each conversion (`From<V2> for V3`) has
    /// a test of its own over the old version's cases ([`cases`]).
    fn read(case: &str, bytes: &[u8]) -> Result<(), String>;

    /// The layout of `version`'s types, for a family serialized by serde: their
    /// `fprint::TypeFingerprint`s, joined. None for one encoded by hand.
    fn fingerprint(version: u32) -> Option<String> {
        let _ = version;
        None
    }
}

/// Whether missing cases are written: `CLADEFS_FORMATS_WRITE=1`.
const WRITE: &str = "CLADEFS_FORMATS_WRITE";

/// Runs `F`'s corpus, under the workspace's `formats/` (the first ancestor of `manifest_dir`, a
/// crate's `CARGO_MANIFEST_DIR`, holding `Cargo.lock`), panicking with every problem found.
pub fn check<F: Family>(manifest_dir: &str) {
    let write = std::env::var(WRITE).is_ok_and(|write| write == "1");
    let problems = problems::<F>(&root(manifest_dir), write);
    assert!(problems.is_empty(), "{}'s golden corpus:\n{}", F::FORMAT.name, problems.join("\n"));
}

/// The cases of `format`'s `version`, each named, as its corpus holds them: for the test of a
/// conversion from that version, which checks what each converts to.
pub fn cases(format: Format, manifest_dir: &str, version: u32) -> Vec<(String, Vec<u8>)> {
    let at = root(manifest_dir).join(format.slug()).join(version.to_string());
    let cases: Vec<_> = files(&at, "bin")
        .into_iter()
        .map(|case| (stem(&case), std::fs::read(&case).expect("reading a case")))
        .collect();
    assert!(!cases.is_empty(), "{}: no case of version {version}", at.display());
    cases
}

/// The workspace's `formats/`: under the first ancestor of `manifest_dir`, a crate's
/// `CARGO_MANIFEST_DIR`, holding `Cargo.lock`.
fn root(manifest_dir: &str) -> PathBuf {
    Path::new(manifest_dir)
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .expect("a crate is in a workspace")
        .join("formats")
}

/// A case's name: its file's, without `.bin`.
fn stem(case: &Path) -> String {
    case.file_stem().expect("a case's name").to_string_lossy().into_owned()
}

/// What is wrong with `F`'s corpus under `root`, writing what is missing of the current version
/// if `write`.
pub fn problems<F: Family>(root: &Path, write: bool) -> Vec<String> {
    let format = F::FORMAT;
    let dir = root.join(format.slug());
    let mut problems = Vec::new();
    for found in subdirectories(&dir) {
        match found.parse::<u32>() {
            Ok(version) if format.versions().contains(&version) => {}
            _ => problems.push(format!(
                "{}: a directory of no version this build reads ({}..={})",
                dir.join(&found).display(),
                format.oldest,
                format.current
            )),
        }
    }

    // The current version: what this build writes, written if missing.
    let current = Current { dir: dir.join(format.current.to_string()), format, write };
    let written = F::write();
    for case in files(&current.dir, "bin") {
        let name = stem(&case);
        if !written.iter().any(|(each, _)| *each == name) {
            problems.push(format!(
                "{}: a case of version {} this build doesn't write: its writer changed, which is a \
                 new version (doc-28)",
                case.display(),
                format.current
            ));
        }
    }
    for (name, bytes) in written {
        current.keep(&format!("{name}.bin"), &bytes, name, &mut problems);
        match F::written(format.current, name, &bytes) {
            Ok(text) => current.keep(&format!("{name}.txt"), text.as_bytes(), name, &mut problems),
            Err(error) => problems.push(format!("{name}: this build's doesn't decode: {error}")),
        }
    }
    if let Some(fingerprint) = F::fingerprint(format.current) {
        current.keep("fingerprint", fingerprint.as_bytes(), "its types", &mut problems);
    }

    // Every version: each case decodes as it did, and this build reads it.
    for version in format.versions() {
        let at = dir.join(version.to_string());
        let cases = files(&at, "bin");
        if cases.is_empty() {
            problems.push(format!("{}: no case of version {version}", at.display()));
        }
        for case in cases {
            let bytes = std::fs::read(&case).expect("reading a case");
            let name = stem(&case);
            let said = std::fs::read_to_string(case.with_extension("txt"));
            match (F::written(version, &name, &bytes), said) {
                (Ok(decoded), Ok(said)) if decoded == said => {}
                (Ok(decoded), Ok(said)) => problems.push(format!(
                    "{}: decodes otherwise than it did:\n{}",
                    case.display(),
                    difference(&said, &decoded)
                )),
                (Ok(_), Err(_)) => problems.push(format!("{}: no .txt", case.display())),
                (Err(error), _) => problems.push(format!(
                    "{}: version {version}'s decoder refuses it: {error}",
                    case.display()
                )),
            }
            if let Err(error) = F::read(&name, &bytes) {
                problems
                    .push(format!("{}: this build's reader refuses it: {error}", case.display()));
            }
        }
        if let Some(fingerprint) = F::fingerprint(version) {
            match std::fs::read_to_string(at.join("fingerprint")) {
                Ok(held) if held == fingerprint => {}
                Ok(held) => problems.push(format!(
                    "{}: version {version}'s types changed:\n{}",
                    at.join("fingerprint").display(),
                    difference(&held, &fingerprint)
                )),
                Err(_) => problems.push(format!("{}: missing", at.join("fingerprint").display())),
            }
        }
    }
    problems
}

/// The corpus's directory of the current version, whose files must be what this build writes.
struct Current {
    dir: PathBuf,
    format: Format,
    /// Whether a file missing is written.
    write: bool,
}

impl Current {
    /// Checks that its file `file` holds `now`, what this build writes of `what`: writing it if it
    /// is missing and `write`, saying in `problems` if it is otherwise.
    fn keep(&self, file: &str, now: &[u8], what: &str, problems: &mut Vec<String>) {
        let path = self.dir.join(file);
        match std::fs::read(&path) {
            Ok(held) if held == now => {}
            Ok(_) => problems.push(format!(
                "{}: this build writes {what} otherwise than version {} did: a change of a frozen \
                 format is a new version (doc-28)",
                path.display(),
                self.format.current
            )),
            Err(_) if self.write => {
                std::fs::create_dir_all(&self.dir).expect("making the corpus's directory");
                std::fs::write(&path, now).expect("writing a case");
            }
            Err(_) => problems.push(format!(
                "{}: missing; {WRITE}=1 writes version {}'s cases",
                path.display(),
                self.format.current
            )),
        }
    }
}

/// The names of `dir`'s subdirectories, none if it has none.
fn subdirectories(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .map(|entry| entry.expect("listing a corpus"))
        .filter(|entry| entry.file_type().expect("a corpus entry's type").is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `dir`'s files ending `.extension`, in order, none if it has none.
fn files(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.expect("listing a corpus").path())
        .filter(|path| path.extension().is_some_and(|found| found == extension))
        .collect();
    paths.sort();
    paths
}

/// The lines of `was` and `is` that differ, each marked, for a refusal.
fn difference(was: &str, is: &str) -> String {
    let mut said = String::new();
    let (was, is): (Vec<_>, Vec<_>) = (was.lines().collect(), is.lines().collect());
    for at in 0..was.len().max(is.len()) {
        let (old, new) = (was.get(at), is.get(at));
        if old != new {
            if let Some(old) = old {
                writeln!(said, "  - {old}").expect("writing to a string");
            }
            if let Some(new) = new {
                writeln!(said, "  + {new}").expect("writing to a string");
            }
        }
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // A family whose versions 1 and 2 are a byte each, 2 doubling what it says, whose writer
    // writes WRITTEN, and whose reader reads what READS allows.
    static WRITTEN: AtomicU32 = AtomicU32::new(7);
    static READS: AtomicU32 = AtomicU32::new(2);
    static RENAMED: AtomicBool = AtomicBool::new(false);

    struct Bytes;

    impl Family for Bytes {
        const FORMAT: Format = Format::new("test bytes", 1, 2);

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            let name = if RENAMED.load(Ordering::Relaxed) { "eight" } else { "seven" };
            vec![(name, vec![2, WRITTEN.load(Ordering::Relaxed) as u8])]
        }

        fn written(version: u32, _: &str, bytes: &[u8]) -> Result<String, String> {
            match (version, bytes) {
                (1, [1, value]) => Ok(format!("V1 {{ value: {value} }}")),
                (2, [2, value]) => Ok(format!("V2 {{ doubled: {} }}", u32::from(*value) * 2)),
                _ => Err(format!("not a version {version} object: {bytes:?}")),
            }
        }

        fn read(_: &str, bytes: &[u8]) -> Result<(), String> {
            let version = u32::from(*bytes.first().ok_or("empty")?);
            if version <= READS.load(Ordering::Relaxed) {
                Ok(())
            } else {
                Err(format!("version {version}"))
            }
        }

        fn fingerprint(version: u32) -> Option<String> {
            Some(format!("V{version}"))
        }
    }

    /// A workspace, `crate` a crate of it, whose corpus holds version 1's case alone.
    fn corpus() -> tempfile::TempDir {
        let workspace = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(workspace.path().join("Cargo.lock"), "").expect("its lock file");
        let root = workspace.path().join("formats");
        let one = root.join("test-bytes/1");
        std::fs::create_dir_all(&one).expect("version 1's directory");
        std::fs::write(one.join("three.bin"), [1, 3]).expect("a case");
        std::fs::write(one.join("three.txt"), "V1 { value: 3 }").expect("its decoding");
        std::fs::write(one.join("fingerprint"), "V1").expect("its fingerprint");
        workspace
    }

    // One test: the family's statics are shared by every test of this module.
    #[test]
    fn a_corpus_refuses_every_change_but_new_cases() {
        let workspace = corpus();
        let root = workspace.path().join("formats");
        let at = |path: &str| root.join("test-bytes").join(path);

        // Version 2's cases missing: refused, until written.
        let missing = problems::<Bytes>(&root, false);
        assert_eq!(missing.len(), 5, "{missing:#?}");
        assert!(
            missing[0]
                .ends_with("seven.bin: missing; CLADEFS_FORMATS_WRITE=1 writes version 2's cases")
        );
        assert_eq!(problems::<Bytes>(&root, true), Vec::<String>::new());
        assert_eq!(std::fs::read(at("2/seven.bin")).unwrap(), [2, 7]);
        assert_eq!(std::fs::read_to_string(at("2/seven.txt")).unwrap(), "V2 { doubled: 14 }");
        assert_eq!(problems::<Bytes>(&root, false), Vec::<String>::new());

        // The writer writing otherwise, without a new version: refused, and never rewritten.
        WRITTEN.store(8, Ordering::Relaxed);
        let rewritten = problems::<Bytes>(&root, true);
        assert!(
            rewritten[0].contains("this build writes seven otherwise than version 2 did"),
            "{rewritten:#?}"
        );
        assert_eq!(std::fs::read(at("2/seven.bin")).unwrap(), [2, 7]);
        WRITTEN.store(7, Ordering::Relaxed);

        // The reader no longer reading version 2.
        READS.store(1, Ordering::Relaxed);
        let unread = problems::<Bytes>(&root, false);
        assert_eq!(unread.len(), 1, "{unread:#?}");
        assert!(unread[0].ends_with("seven.bin: this build's reader refuses it: version 2"));
        READS.store(2, Ordering::Relaxed);

        // A case renamed in the writer, the old case kept: version 2 changed all the same.
        WRITTEN.store(8, Ordering::Relaxed);
        RENAMED.store(true, Ordering::Relaxed);
        let renamed = problems::<Bytes>(&root, true);
        assert_eq!(renamed.len(), 1, "{renamed:#?}");
        assert!(renamed[0].ends_with(
            "2/seven.bin: a case of version 2 this build doesn't write: its writer changed, which \
             is a new version (doc-28)"
        ));
        WRITTEN.store(7, Ordering::Relaxed);
        RENAMED.store(false, Ordering::Relaxed);
        std::fs::remove_file(at("2/eight.bin")).unwrap();
        std::fs::remove_file(at("2/eight.txt")).unwrap();
        assert_eq!(
            cases(Bytes::FORMAT, workspace.path().join("crate").to_str().unwrap(), 2),
            [("seven".to_owned(), vec![2, 7])]
        );

        // An old case decoding otherwise.
        std::fs::write(at("1/three.txt"), "V1 { value: 4 }").unwrap();
        let decoded = problems::<Bytes>(&root, false);
        assert_eq!(decoded.len(), 1, "{decoded:#?}");
        assert!(
            decoded[0].contains(
                "decodes otherwise than it did:\n  - V1 { value: 4 }\n  + V1 { value: 3 }"
            )
        );
        std::fs::write(at("1/three.txt"), "V1 { value: 3 }").unwrap();

        // An old version's types changed, its cases gone, a version of no range.
        std::fs::write(at("1/fingerprint"), "V1'").unwrap();
        std::fs::remove_file(at("1/three.bin")).unwrap();
        std::fs::create_dir(at("0")).unwrap();
        let gone = problems::<Bytes>(&root, false);
        assert_eq!(gone.len(), 3, "{gone:#?}");
        assert!(
            gone[0].ends_with("test-bytes/0: a directory of no version this build reads (1..=2)")
        );
        assert!(gone[1].ends_with("test-bytes/1: no case of version 1"));
        assert!(gone[2].contains("version 1's types changed:\n  - V1'\n  + V1"));
    }
}
