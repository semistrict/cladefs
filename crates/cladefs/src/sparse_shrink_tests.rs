// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use block_device::{BlockDevice, Options};
use storage_device::{DeviceHolder, fake_device::FakeDevice};

/// TASK-532: the scratch memory needed to discard an empty subtree depends on its height.
/// Both block devices here have no allocated extents; 1 MiB allows for an additional tree level.
#[test]
fn shrinking_an_all_hole_device_skips_absent_blocks() {
    let shrink = |size| {
        let device = DeviceHolder::new(FakeDevice::new(1024, 4096));
        let blocks = BlockDevice::open(device, Options::default()).unwrap();
        blocks.resize(size).unwrap();
        blocks.flush().unwrap();
        assert!(blocks.device().extents().is_empty());
        blocks.resize(0).unwrap();
        let peak = crate::heap::peak_while(|| blocks.flush().unwrap());
        let recovered =
            BlockDevice::open(blocks.device().snapshot().unwrap(), Options::default()).unwrap();
        assert_eq!(recovered.size().unwrap(), 0);
        peak
    };
    let small = shrink(16 << 20);
    let large = shrink(1 << 30);
    assert!(large <= small + (1 << 20), "peak scratch bytes: 16 MiB={small}, 1 GiB={large}");
}
