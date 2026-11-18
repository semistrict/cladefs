// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use extent_files::{FileSet, Options};
use storage_device::{DeviceHolder, fake_device::FakeDevice};

/// TASK-532: the scratch memory needed to discard an empty subtree depends on its height.
/// Both files here have no allocated extents; 1 MiB allows for an additional tree level.
#[test]
fn shrinking_an_all_hole_file_skips_absent_blocks() {
    let shrink = |size| {
        let device = DeviceHolder::new(FakeDevice::new(1024, 4096));
        let files = FileSet::open(device, Options::default()).unwrap();
        files.open_file("holes", true).unwrap();
        files.truncate("holes", size).unwrap();
        files.sync("holes").unwrap();
        assert!(files.device().extents().is_empty());
        files.truncate("holes", 0).unwrap();
        let peak = crate::heap::peak_while(|| files.sync("holes").unwrap());
        let recovered =
            FileSet::open(files.device().snapshot().unwrap(), Options::default()).unwrap();
        recovered.open_file("holes", false).unwrap();
        assert_eq!(recovered.size("holes").unwrap(), 0);
        peak
    };
    let small = shrink(16 << 20);
    let large = shrink(1 << 30);
    assert!(large <= small + (1 << 20), "peak scratch bytes: 16 MiB={small}, 1 GiB={large}");
}
