// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Refuses release builds without frame pointers and compresses debug sections in Linux release
//! binaries (`cladefsd`, `cladefsctl`, `cladefs-mount`).

#[path = "build/stamp.rs"]
#[allow(dead_code)]
mod stamp;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    stamp::require_frame_pointers()?;
    if std::env::var("PROFILE")? == "release" && std::env::var("CARGO_CFG_TARGET_OS")? == "linux" {
        println!("cargo:rustc-link-arg-bins=-Wl,--compress-debug-sections=zlib");
    }
    Ok(())
}
