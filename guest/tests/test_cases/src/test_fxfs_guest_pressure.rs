// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs board 14663: writes and fsyncs under memory pressure. A guest of little memory holds
//! nearly all of it in anonymous memory, which nothing reclaims without swap, while writers on each
//! CPU write files and fsync them: their dirty pages' writeback is what frees memory, and each of
//! its requests needs memory to be sent. A transport whose requests wait on reclaim that waits on
//! them (doc-4 over vsock: socket buffers allocated as the requests are sent) deadlocks here, the
//! fsyncs never returning; each must return, every file reading back what was written. It runs on
//! whichever transport the device has: the queues, or the stream with CLADEFS_EXTENT_STREAM=1.

use macros::{guest, host};

pub struct TestFxfsGuestPressure;

#[host]
mod host {
    use super::*;
    use crate::common::{cladefs_guest_should_run, setup_rootfs};
    use crate::vm::Vm;
    use crate::{ShouldRun, Test, TestSetup};

    const CAPACITY: u64 = 1 << 30;

    impl Test for TestFxfsGuestPressure {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn timeout_secs(&self) -> u64 {
            180
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut vm = Vm::new(2, 256)?;
            vm.guest_kernel()?;
            // No window: the page cache's dirty pages, written back, are what reclaim frees.
            vm.new_cell("extents", CAPACITY, 0)?;
            vm.enter(&setup_rootfs(&test_setup)?, &test_setup.test_case)
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use nix::libc;
    use std::ffi::CString;
    use std::fs;
    use std::io::{Read, Write};

    const MNT: &str = "/mnt/fxfs";
    /// The memory the guest keeps available, at most, while the writers write: a few files' worth,
    /// so that their pages' writeback has to free some for the next.
    const LEFT_MIB: u64 = 24;
    /// Each writer's files, and each one's size.
    const FILES: usize = 8;
    const FILE_BYTES: usize = 8 << 20;

    fn mount(source: &str, target: &str, fstype: &str) {
        fs::create_dir_all(target).unwrap();
        let (source, fstype) = (CString::new(source).unwrap(), CString::new(fstype).unwrap());
        let target = CString::new(target).unwrap();
        let ret = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
        };
        assert_eq!(ret, 0, "mount {fstype:?}: {}", std::io::Error::last_os_error());
    }

    /// /proc/meminfo's MemAvailable, in MiB.
    fn available_mib() -> u64 {
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap();
        let kib: u64 = (meminfo.lines())
            .find_map(|line| line.strip_prefix("MemAvailable:"))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|kib| kib.trim().parse().ok())
            .unwrap_or_else(|| panic!("no MemAvailable in /proc/meminfo:\n{meminfo}"));
        kib >> 10
    }

    /// Anonymous memory held until dropped, its pages touched, so that it is memory, not address
    /// space: as much as leaves `left` MiB available, a MiB at a time.
    fn hold_all_but(left: u64) -> Vec<Vec<u8>> {
        let mut held = Vec::new();
        while available_mib() > left {
            let mut chunk = vec![0u8; 1 << 20];
            for page in chunk.chunks_mut(4096) {
                page[0] = 1;
            }
            held.push(chunk);
        }
        held
    }

    /// The bytes of `writer`'s file `file`.
    fn pattern(writer: usize, file: usize) -> Vec<u8> {
        let seed = (writer * FILES + file) as u8;
        (0..FILE_BYTES).map(|i| (i % 251) as u8 ^ seed).collect()
    }

    impl Test for TestFxfsGuestPressure {
        fn in_guest(self: Box<Self>) {
            mount("extents", MNT, "cladefs");
            let held = hold_all_but(LEFT_MIB);
            let pressed = available_mib();
            assert!(pressed <= LEFT_MIB, "{pressed} MiB available, holding {} MiB", held.len());
            let cpus = std::thread::available_parallelism().unwrap().get();
            let writers: Vec<_> = (0..cpus)
                .map(|writer| {
                    std::thread::spawn(move || {
                        let mut least = u64::MAX;
                        for file in 0..FILES {
                            let path = format!("{MNT}/w{writer}-f{file}");
                            let mut out = fs::File::create(&path).unwrap();
                            out.write_all(&pattern(writer, file)).unwrap();
                            least = least.min(available_mib());
                            out.sync_all().unwrap();
                        }
                        least
                    })
                })
                .collect();
            let least = writers.into_iter().map(|writer| writer.join().unwrap()).min().unwrap();
            // The writers wrote with little memory left: what this case is about.
            assert!(least <= LEFT_MIB, "the writers had {least} MiB available at the least");
            drop(held);
            for writer in 0..cpus {
                for file in 0..FILES {
                    let path = format!("{MNT}/w{writer}-f{file}");
                    let mut back = Vec::with_capacity(FILE_BYTES);
                    fs::File::open(&path).unwrap().read_to_end(&mut back).unwrap();
                    assert!(back == pattern(writer, file), "{path} read back otherwise");
                }
            }
            println!("OK");
        }
    }
}
