// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Replays and checks the Fxfs image in a cell directory, as the guest tests' host checks a killed
//! run's cell (`Storage::check`): its journal replayed, fsck of the filesystem and every volume.
//! For a cell a failing run kept (fxfs-guest-script-killed*'s `killed-cell`, board 14491), checked
//! again on the host alone, as often as a change to Fxfs needs.
//!
//!   cargo run -p cladefs --example check_cell -- CELL_DIR

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let cell = std::env::args().nth(1).context("usage: check_cell CELL_DIR")?;
    futures::executor::block_on(async {
        let storage = cladefs::Storage::open(&cell).await?;
        storage.check().await?.close().await;
        anyhow::Ok(())
    })?;
    println!("{cell}: replays and checks");
    Ok(())
}
