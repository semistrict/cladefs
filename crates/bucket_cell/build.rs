// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Has a release build's binaries (cladefs-gc) link as cladefs's do (board 12766).

#[path = "../cladefs/build/release_link.rs"]
mod release_link;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../cladefs/build/release_link.rs");
    release_link::compress_debug_sections()
}
