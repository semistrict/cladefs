// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what tests share of looking at the host's disk beside cells (the `testing` feature,
//! TASK-463): the files under a directory that hold what a test wrote in the clear, as tests of
//! encrypted cells look for (TASK-411.5), here and in sqlite_cladefs'.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

/// The paths of the files under `dir`, at every depth, that hold any of `pieces`, all of one
/// length. What keeps files there must be still meanwhile: its cells closed, or their work
/// behind their callers waited for (`Cell::wait_idle`, and their uploaders' `wait_uploaded`). A
/// file listed, then gone before it is read, fails, as a cell working on would.
pub fn files_holding(dir: &Path, pieces: &[&[u8]]) -> Vec<String> {
    let mut held = BTreeSet::new();
    pass(dir, pieces, &mut held, Gone::Fails, &mut |_| {});
    held.into_iter().collect()
}

/// [`files_holding`], beside what works on in `dir` that the test cannot wait for, as a daemon's
/// cells do, behind its API: a file or directory listed, then gone before it is read, ends the
/// pass short, and another walks `dir` again, until one reads everything it lists. Each pass's
/// finds are kept, so a file renamed between passes is found under either name: none is lost
/// for going, which a pass that skipped it and returned would lose.
pub fn files_holding_beside(dir: &Path, pieces: &[&[u8]]) -> Vec<String> {
    beside(dir, pieces, &mut |_| {})
}

/// [`files_holding_beside`], `listed` called with each directory as a pass lists it.
fn beside(dir: &Path, pieces: &[&[u8]], listed: &mut dyn FnMut(&Path)) -> Vec<String> {
    let mut held = BTreeSet::new();
    while !pass(dir, pieces, &mut held, Gone::EndsShort, listed) {}
    held.into_iter().collect()
}

/// What a pass does with a file or directory gone between its listing and its reading.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gone {
    Fails,
    EndsShort,
}

/// Walks `dir` once, adding to `held` the paths of the files holding any of `pieces`, calling
/// `listed` with each directory once it listed it: returns whether it read every file and
/// directory it listed, those gone meanwhile handled as `gone` says. Any other error fails.
fn pass(
    dir: &Path,
    pieces: &[&[u8]],
    held: &mut BTreeSet<String>,
    gone: Gone,
    listed: &mut dyn FnMut(&Path),
) -> bool {
    let len = pieces.first().expect("pieces to look for").len();
    assert!(pieces.iter().all(|piece| piece.len() == len), "pieces of lengths unlike");
    let mut whole = true;
    let mut went = |at: &Path, error: io::Error| {
        let tolerated = gone == Gone::EndsShort && error.kind() == io::ErrorKind::NotFound;
        assert!(tolerated, "{}: {error}", at.display());
        whole = false;
    };
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(at) = dirs.pop() {
        let entries = std::fs::read_dir(&at).and_then(Iterator::collect::<io::Result<Vec<_>>>);
        let entries = match entries {
            Ok(entries) => entries,
            Err(error) => {
                went(&at, error);
                continue;
            }
        };
        listed(&at);
        for path in entries.into_iter().map(|entry| entry.path()) {
            // As a symbolic link's target is, if it is one.
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => dirs.push(path),
                Ok(_) => match std::fs::read(&path) {
                    Ok(bytes) if bytes.windows(len).any(|window| pieces.contains(&window)) => {
                        held.insert(path.display().to_string());
                    }
                    Ok(_) => {}
                    Err(error) => went(&path, error),
                },
                Err(error) => went(&path, error),
            }
        }
    }
    whole
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIECES: [&[u8]; 2] = [b"secret", b"SECRET"];

    /// Files at every depth holding a piece are found, by their paths, and those holding none, or
    /// only part of one, are not.
    #[test]
    fn finds_the_files_holding_pieces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/held"), b"xxsecretyy").unwrap();
        std::fs::write(dir.path().join("a/other"), b"xxSECRETyy").unwrap();
        std::fs::write(dir.path().join("part"), b"secre").unwrap();
        std::fs::write(dir.path().join("too"), b"secret").unwrap();
        let at = |name: &str| dir.path().join(name).display().to_string();
        let expected = vec![at("a/b/held"), at("a/other"), at("too")];
        assert_eq!(files_holding(dir.path(), &PIECES), expected);
        assert_eq!(files_holding_beside(dir.path(), &PIECES), expected);
    }

    /// A file renamed between its listing and its reading, as a cell's framing renames its
    /// segment's framed file its own (TASK-463), fails a walk of what is to be still.
    #[test]
    #[should_panic(expected = "framing-1: No such file or directory")]
    fn files_renamed_as_listed_fail_a_still_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("framing-1"), b"secret").unwrap();
        let mut held = BTreeSet::new();
        let mut rename = |_: &Path| {
            std::fs::rename(dir.path().join("framing-1"), dir.path().join("1")).unwrap();
        };
        pass(dir.path(), &PIECES, &mut held, Gone::Fails, &mut rename);
    }

    /// Beside what works on, a file renamed between its listing and its reading is found under
    /// its new name, by the walk the pass it went from ended short for; and a directory gone so
    /// leaves nothing to find.
    #[test]
    fn files_renamed_as_listed_are_found_beside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("going")).unwrap();
        std::fs::write(dir.path().join("going/plain"), b"plain").unwrap();
        std::fs::write(dir.path().join("framing-1"), b"xsecretx").unwrap();
        let (mut passes, root) = (0, dir.path().to_path_buf());
        let mut listed = |at: &Path| {
            if at != root {
                return;
            }
            passes += 1;
            if passes == 1 {
                std::fs::rename(root.join("framing-1"), root.join("1")).unwrap();
                std::fs::remove_dir_all(root.join("going")).unwrap();
            }
        };
        let held = beside(dir.path(), &PIECES, &mut listed);
        assert_eq!((held, passes), (vec![dir.path().join("1").display().to_string()], 2));
    }
}
