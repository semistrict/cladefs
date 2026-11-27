// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Refuses release builds without frame pointers and compresses debug sections in Linux release
//! binaries (`cladefsd`, `cladefsctl`), as bucket_cell's `cladefs-gc` does too
//! ([`release_link`], board 10269, board 12766).

#[path = "build/stamp.rs"]
#[allow(dead_code)]
mod stamp;

#[path = "build/release_link.rs"]
mod release_link;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    stamp::require_frame_pointers()?;
    release_link::compress_debug_sections()
}
