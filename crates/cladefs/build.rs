// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Generates the extent daemon's gRPC API (`proto/cladefs/cladefsd/v1/cladefsd.proto`), what
//! daemons sharing a bucket serve each other (`proto/cladefs/peer/v1/peer.proto`, TASK-406.2), and
//! the API of FUSE mounts of many cells (`proto/cladefs/mount/v1/mount.proto`), with protox, which
//! compiles protobuf in Rust: no protoc needed; for tonic, and for madsim's tonic. cladefs: and
//! stamps the build with the commit it is of (`CLADEFS_BUILD_COMMIT`, [`crate::BUILD_COMMIT`]), as
//! scripts/build-commit.sh says, which every build of the tree reports (TASK-111.7).

use std::path::PathBuf;

// What the build stamps, and checks of it (board 8599).
#[path = "build/stamp.rs"]
mod stamp;

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
    stamp::stamp_commit()
}
