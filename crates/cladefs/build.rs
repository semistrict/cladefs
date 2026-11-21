// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Generates the extent daemon's gRPC API (`proto/cladefs/cladefsd/v1/cladefsd.proto`), what
//! daemons sharing a bucket serve each other (`proto/cladefs/peer/v1/peer.proto`, TASK-406.2), and
//! the API of FUSE mounts of many cells (`proto/cladefs/mount/v1/mount.proto`), with protox, which
//! compiles protobuf in Rust: no protoc needed; for tonic, and for madsim's tonic. cladefs: and
//! stamps the build with the commit it is of (`CLADEFS_BUILD_COMMIT`, [`crate::BUILD_COMMIT`]), as
//! scripts/build-commit.sh says, which every build of the tree reports (TASK-111.7).

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/cladefs/cladefsd/v1/cladefsd.proto",
        "proto/cladefs/peer/v1/peer.proto",
        "proto/cladefs/mount/v1/mount.proto",
    ];
    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }
    // Compiled by protox, then generated twice from its descriptors by madsim's tonic-build: as
    // tonic has it, and, in `$OUT_DIR/sim`, as madsim's simulated tonic has it, which the build
    // under `--cfg madsim` includes (doc-24, TASK-685.12).
    let descriptors = protox::Compiler::new(["proto"])?
        .include_imports(true)
        .include_source_info(true)
        .open_files(protos)?
        .encode_file_descriptor_set();
    let set = PathBuf::from(std::env::var("OUT_DIR")?).join("protos.bin");
    std::fs::write(&set, descriptors)?;
    madsim_tonic_build::configure()
        .file_descriptor_set_path(&set)
        .skip_protoc_run()
        .compile_protos(&protos, &["proto"])?;
    stamp_commit()
}

/// Has the crate's `CLADEFS_BUILD_COMMIT` be the commit scripts/build-commit.sh says the tree is
/// of, and the build stamp it again as that changes: as the checkout moves to another commit, or
/// the crate's own sources change, which may make it dirty; or the source a host run's worktree
/// carries says another (`source-revision.txt`).
fn stamp_commit() -> Result<(), Box<dyn std::error::Error>> {
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
    println!("cargo:rerun-if-changed=src");
    for path in git_paths(&root) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    Ok(())
}

/// The files of `root`'s checkout, if it is one, that change as it moves to another commit: its
/// HEAD, the branch it names, and its index, which a commit writes.
fn git_paths(root: &Path) -> Vec<PathBuf> {
    let git = |args: &[&str]| -> Option<String> {
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
