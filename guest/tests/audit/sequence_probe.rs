// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Standalone release-profile probe for TASK-660; see guest/tests/audit/README.md.

use local_cell::{CellId, Description, LocalCell, MemDir, Options};
use storage_device::ExtentId;

fn main() {
    let description =
        Description { block_size: 4096, capacity: 1 << 20, cell: CellId { tenant: 1, id: 1 } };
    let store = LocalCell::create(MemDir::new(), description, Options::default()).unwrap();
    let extent = ExtentId::new(1, u64::MAX);
    let first = store.write(extent, 0, &[0x5a; 4096]);
    println!("first write: {first:?}");
    store.commit(None).unwrap();
    println!("next_sequence: {}", store.info().unwrap().next_sequence);
    store.free(extent).unwrap();
    store.commit(None).unwrap();
    let second = store.write(extent, 0, &[0xa5; 4096]);
    println!("rewrite after free: {second:?}");
    let mut bytes = [0; 4096];
    store.read(extent, 0, &mut bytes).unwrap();
    println!("read back: {:x}", bytes[0]);
}
