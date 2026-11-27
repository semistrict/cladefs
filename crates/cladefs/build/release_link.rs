// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! How the release binaries link, shared by the build scripts of the crates that ship them:
//! crates/cladefs/build.rs (cladefsd, cladefsctl) and crates/bucket_cell/build.rs
//! (cladefs-gc), so that the four scripts/build-musl.sh builds link alike (board 12766).

/// cladefs: has a release build's binaries on Linux link with their debug sections compressed
/// (board 10269): the release profile's line tables, which `/z/pprof` and `bucket_cell`'s
/// resolved stacks name each frame's file and line by, took cladefsd from 61 MB to 281 MB, most of
/// it the C dependencies' and std's debug info. zlib (`SHF_COMPRESSED`), which GNU ld and lld both
/// write and the backtrace crate resolving those frames reads; zstd only with its `ruzstd` feature,
/// off here. Tests' and debug builds link as they were, sparing every test's link the compression.
/// cladefsd 293 MB -> 105 MB, its frames still named by file and line (hotbed question 558);
/// cladefs-gc 146.6 MB -> 42.8 MB on x86_64 musl, `addr2line` still naming its file and line
/// (board 12766).
pub fn compress_debug_sections() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("PROFILE")? == "release" && std::env::var("CARGO_CFG_TARGET_OS")? == "linux" {
        println!("cargo:rustc-link-arg-bins=-Wl,--compress-debug-sections=zlib");
    }
    Ok(())
}
