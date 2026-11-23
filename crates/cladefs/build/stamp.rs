// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What crates/cladefs/build.rs stamps a build with, and checks of it, apart from its protos, so
//! that scripts/tests/test_build_stamp.py builds a crate of its own with them, twice around an
//! edit, as cargo runs them (board 8599).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Has the crate's `CLADEFS_BUILD_COMMIT` be the commit scripts/build-commit.sh says the tree is
/// of, and the build stamp it again as that changes: as the checkout moves to another commit; as
/// any file of the workspace's crates, or its manifest or lock, changes, which may make it dirty or
/// clean again, whichever crate it is of (most of the daemon is local_cell's and bucket_cell's,
/// board 8599); or as the source a host run's worktree carries says another
/// (`source-revision.txt`). `ROOT` is the workspace's, two directories above the crate's.
pub fn stamp_commit() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("../..");
    let script = root.join("scripts/build-commit.sh");
    let output = Command::new("bash").arg(&script).arg(&root).output()?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{}: {}: {said}", script.display(), output.status).into());
    }
    let commit = String::from_utf8(output.stdout)?;
    println!("cargo:rustc-env=CLADEFS_BUILD_COMMIT={}", commit.trim());
    println!("cargo:rerun-if-env-changed=CLADEFS_BUILD_COMMIT");
    println!("cargo:rerun-if-changed={}", script.display());
    // Only where it is: a path missing would have the script run at every build.
    let shipped = root.join("source-revision.txt");
    if shipped.exists() {
        println!("cargo:rerun-if-changed={}", shipped.display());
    }
    // Every crate's files, one directory, which cargo walks at each build, comparing their times
    // with the stamp's: some 650 files, a stat each. A crate this one doesn't depend on reruns it
    // too, its tree dirty as well, and its binaries, which link this crate, report the stamp.
    for path in ["crates", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    for path in git_paths(&root) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    Ok(())
}

/// The files of `root`'s checkout, if it is one, that change as it moves to another commit: its
/// HEAD, the branch it names, and its index, which a commit writes.
fn git_paths(root: &Path) -> Vec<PathBuf> {
    let git = |args: &[&str]| -> Option<String> {
        // No git, or no checkout (a host run's copy of the tree): no paths, the stamp then
        // source-revision.txt's, which is watched above.
        let output = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        output.status.success().then(|| text.trim().to_owned())
    };
    let mut names = vec!["HEAD".to_owned(), "index".to_owned()];
    names.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    (names.iter())
        .filter_map(|name| git(&["rev-parse", "--path-format=absolute", "--git-path", name]))
        .map(PathBuf::from)
        .collect()
}
