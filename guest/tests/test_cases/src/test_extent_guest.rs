// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Test the extent-store device and the guest kernel's driver of it (cladefs's
// TASK-26.4 and TASK-26.5).
//
// The host boots the kernel cladefs's scripts/build-guest-kernel.sh builds, which
// CLADEFS_GUEST_KERNEL names, with an extent-store device on a new cell with a
// DAX window. The guest runs the driver's self-test, which issues every request
// of the protocol, many writes and reads at once, and reads an extent mapped
// into the DAX window in place. After the VM exits the host opens the cell and
// checks what the self-test committed.

use macros::{guest, host};

pub struct TestExtentGuest;

/// cladefs TASK-141.3: the driver refuses a device of another protocol version at probe, naming
/// both: here the device is of this build's, and the driver is told it speaks the next
/// (`virtio_extent.version`), as a guest of another build would. No device is bound, and the
/// kernel's log says why. Before, the device's configuration said no version, and a driver of any
/// build served it.
pub struct TestExtentGuestOtherVersion;

/// cladefs board 8557: the driver refuses, at probe, -EINVAL and the field named, a device whose
/// configuration would have it or the filesystem divide by zero, round to nothing or wait for
/// good: here the device is this build's, and the driver is told it said `fault`
/// (`virtio_extent.config_fault`), as a VMM's bug would have it say. No device is bound, the kernel
/// says why, and nothing oopses (an oops fails the case). Before, a block size of 0 divided by zero
/// as Fxfs mounted, and a ring of fewer than 3 entries wrapped the clamp of its segments.
pub struct TestExtentGuestBadConfig {
    /// `FIELD=N`, as `virtio_extent.config_fault` takes it.
    pub fault: &'static str,
    /// What the driver's refusal says after the device's name.
    #[cfg_attr(feature = "host", allow(dead_code))]
    pub refusal: &'static str,
}

/// cladefs board 14352: the driver's self-test refuses, EBUSY, a device a mounted filesystem holds,
/// whose root it would replace. The guest mounts its cell, writes a file and syncs it and its
/// directory, writes the self-test's attribute (refused, the attribute still "not run"), and
/// resets its VM at once, without syncing or unmounting, as a crash does. The host then checks and
/// mounts the cell: the file the guest synced is there, whole. Before, the self-test ran, and the
/// cell's root after the reset was the self-test's: its journal no longer replayed.
pub struct TestExtentGuestSelftestMounted;

/// What the guest of [`TestExtentGuestSelftestMounted`] writes and syncs before the self-test.
const SYNCED: &[u8] = b"synced before the self-test was asked for\n";

/// The version the driver is told it speaks: the device's, this build's, and one more.
const OTHER_VERSION: (u32, u32) = (3, 4);

/// The root the driver's self-test commits.
#[cfg_attr(feature = "guest", allow(dead_code))]
const ROOT: &[u8] = b"cladefs extent store self-test";

#[host]
mod host {
    use super::*;

    use crate::common::{cladefs_guest_should_run, setup_rootfs};
    use crate::vm::Vm;
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};

    impl Test for TestExtentGuest {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let (cell, _) =
                crate::common::create_shared_cell(&test_setup.tmp_dir, "extents", 64 << 20)?;
            let mut vm = Vm::new(2, 512)?;
            vm.guest_kernel()?;
            vm.cell_id("extents", cell, 64 << 20, 64 << 20)?;
            vm.enter(&setup_rootfs(&test_setup)?, &test_setup.test_case)
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }
            // What the self-test committed is in the cell.
            let cell_path = crate::common::shared_cell_path(&test_setup.tmp_dir, "extents")
                .expect("shared cell path");
            let cell = local_cell::upload::wait_on(cladefs::OpenCell::open(cell_path))
                .expect("open the extent store");
            assert_eq!(cell.root().unwrap(), ROOT);
            let info = cell.info().unwrap();
            // 34 extents of two blocks written, sequences 0 to 33, of which two were freed: the
            // last, a write whose commit set the root (WRITE_ROOT), by the self-test, its root put
            // back.
            assert_eq!(info.used, 32 * 2 * 4096);
            // cladefs: a cell opened says one past the highest of its extents it holds, 32, where
            // fxfs looks for orphans below (TASK-165.9's one recovery; before, its footers'
            // stamp, one past the highest written). The freed 33 is minted no more: its writer
            // mints from its floor, past every number an earlier writer minted.
            assert_eq!(info.next_sequence, 33);
            assert!(info.sequence_floor > 33, "the floor, {}, is past 33", info.sequence_floor);
            TestOutcome::Pass
        }
    }

    impl Test for TestExtentGuestSelftestMounted {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let (cell, _) =
                crate::common::create_shared_cell(&test_setup.tmp_dir, "extents", 64 << 20)?;
            let mut vm = Vm::new(2, 512)?;
            vm.guest_kernel()?;
            vm.cell_id("extents", cell, 64 << 20, 0)?;
            vm.enter(&setup_rootfs(&test_setup)?, &test_setup.test_case)
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            use cladefs::{Filesystem, Storage};
            use futures::executor::block_on;
            let output = crate::vm::without_kernel_lines(&String::from_utf8(stdout).unwrap());
            // EBUSY, 16 on every architecture Linux has.
            if output != "\nSELFTEST Err(Some(16)) not run\n" {
                return TestOutcome::Fail(format!("the guest said {output:?}"));
            }
            let shared = crate::common::shared_cell_path(&test_setup.tmp_dir, "extents")
                .expect("shared cell path");
            // A copy of the cell as the reset left it, once the daemon lets it go.
            let path = test_setup.tmp_dir.join("extents-at-reset");
            crate::common::copy_cell_dir(&shared, &path).unwrap();
            let mounted = block_on(async {
                let storage = Storage::open(&path).await;
                let storage = storage.map_err(|err| format!("the cell does not open: {err:?}"))?;
                let storage = storage.check().await;
                let storage =
                    storage.map_err(|err| format!("the cell fails its check: {err:?}"))?;
                let fs = Filesystem::mount(storage).await;
                let fs = fs.map_err(|err| format!("the cell does not mount: {err:?}"))?;
                let found = fs.root().lookup(b"synced").await;
                let (file, attr) =
                    found.map_err(|err| format!("the synced file is not there: {err:?}"))?;
                let mut contents = vec![0; attr.size as usize];
                let mut read = 0;
                while read < contents.len() {
                    let n = file.read(read as u64, &mut contents[read..]).await;
                    let n = n.map_err(|err| format!("the synced file does not read: {err:?}"))?;
                    if n == 0 {
                        return Err(format!("the synced file ends early, at {read}"));
                    }
                    read += n;
                }
                fs.close()
                    .await
                    .map_err(|err| format!("the filesystem does not close: {err:?}"))?;
                Ok(contents)
            });
            match mounted {
                Ok(contents) if contents == SYNCED => TestOutcome::Pass,
                Ok(contents) => TestOutcome::Fail(format!("the synced file holds {contents:?}")),
                Err(why) => TestOutcome::Fail(why),
            }
        }
    }

    impl Test for TestExtentGuestOtherVersion {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut vm = Vm::new(2, 512)?;
            vm.guest_kernel()?;
            vm.new_cell("extents", 64 << 20, 64 << 20)?;
            let param = format!("virtio_extent.version={}", OTHER_VERSION.1);
            vm.module_param(&std::ffi::CString::new(param)?);
            vm.enter(&setup_rootfs(&test_setup)?, &test_setup.test_case)
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }
            TestOutcome::Pass
        }
    }

    impl Test for TestExtentGuestBadConfig {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut vm = Vm::new(2, 512)?;
            vm.guest_kernel()?;
            vm.new_cell("extents", 64 << 20, 64 << 20)?;
            let param = format!("virtio_extent.config_fault={}", self.fault);
            vm.module_param(&std::ffi::CString::new(param)?);
            vm.enter(&setup_rootfs(&test_setup)?, &test_setup.test_case)
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if output != "OK\n" {
                return TestOutcome::Fail(format!("expected exactly \"OK\\n\", got {output:?}"));
            }
            TestOutcome::Pass
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use std::fs;

    const DRIVER: &str = "/sys/bus/virtio/drivers/virtio_extent";

    impl Test for TestExtentGuest {
        fn in_guest(self: Box<Self>) {
            let devices: Vec<_> = fs::read_dir(DRIVER)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.join("selftest").exists())
                .collect();
            assert_eq!(devices.len(), 1, "{devices:?}");
            let selftest = devices[0].join("selftest");
            assert_eq!(fs::read_to_string(&selftest).unwrap(), "not run\n");
            fs::write(&selftest, "1").unwrap();
            assert_eq!(fs::read_to_string(&selftest).unwrap(), "ok\n");
            let _ = ROOT;
            println!("OK");
        }
    }

    impl Test for TestExtentGuestSelftestMounted {
        fn in_guest(self: Box<Self>) {
            use std::ffi::CString;
            use std::io::Write as _;
            const MNT: &str = "/mnt/fxfs";
            let devices: Vec<_> = fs::read_dir(DRIVER)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.join("selftest").exists())
                .collect();
            assert_eq!(devices.len(), 1, "{devices:?}");
            let selftest = devices[0].join("selftest");
            fs::create_dir_all(MNT).unwrap();
            let (source, target) = (CString::new("extents").unwrap(), CString::new(MNT).unwrap());
            let fstype = CString::new("cladefs").unwrap();
            // SAFETY: A plain system call, on strings that outlive it.
            let ret = unsafe {
                nix::libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ptr(),
                    0,
                    std::ptr::null(),
                )
            };
            assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
            let mut file = fs::File::create(format!("{MNT}/synced")).unwrap();
            file.write_all(SYNCED).unwrap();
            file.sync_all().unwrap();
            fs::File::open(MNT).unwrap().sync_all().unwrap();
            // The self-test, asked for with the filesystem mounted.
            let wrote = fs::write(&selftest, "1").map_err(|err| err.raw_os_error());
            let result = fs::read_to_string(&selftest).unwrap();
            crate::guest_init::say_out(&format!("SELFTEST {wrote:?} {}", result.trim()));
            crate::guest_init::crash()
        }
    }

    impl Test for TestExtentGuestOtherVersion {
        fn in_guest(self: Box<Self>) {
            let bound: Vec<_> = fs::read_dir(DRIVER)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| name.starts_with("virtio"))
                .collect();
            assert_eq!(bound, Vec::<String>::new(), "the driver took a device of another version");
            let (device, driver) = OTHER_VERSION;
            let refused = format!(
                "the device is of protocol version {device}, this driver of {driver}: refused, as \
                 a driver of another build is"
            );
            let records = crate::kernel_log::records();
            let said: Vec<_> = records.iter().filter(|record| record.contains(&refused)).collect();
            assert_eq!(said.len(), 1, "the kernel's log: {records:#?}");
            println!("OK");
        }
    }

    impl Test for TestExtentGuestBadConfig {
        fn in_guest(self: Box<Self>) {
            let bound: Vec<_> = fs::read_dir(DRIVER)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| name.starts_with("virtio"))
                .collect();
            assert_eq!(
                bound,
                Vec::<String>::new(),
                "the driver took a device saying {}",
                self.fault
            );
            let records = crate::kernel_log::records();
            for said in [
                format!("{}: refused", self.refusal),
                // The driver core's, with the probe's error: -EINVAL.
                "probe with driver virtio_extent failed with error -22".to_owned(),
            ] {
                let found: Vec<_> =
                    records.iter().filter(|record| record.contains(&said)).collect();
                assert_eq!(found.len(), 1, "{said:?} in the kernel's log: {records:#?}");
            }
            println!("OK");
        }
    }
}
