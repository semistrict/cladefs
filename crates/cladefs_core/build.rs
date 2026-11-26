// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stamps `CLADEFS_BUILD_COMMIT` and verifies frame pointers for release builds.

#[path = "build/stamp.rs"]
mod stamp;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    stamp::require_frame_pointers()?;
    stamp::stamp_commit()
}
