// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A VM process's private pages, as its pagemap says (`PAGEMAP_SCAN`,
//! `include/uapi/linux/fs.h`): those in memory or swapped, of no file's, nor the zero page. What
//! a guest stored since its memory was mapped, and pages a copy installed: what a persist keeps.

use std::fs::File;
use std::ops::Range;
use std::os::fd::AsRawFd;

const PAGE_IS_FILE: u64 = 1 << 2;
const PAGE_IS_PRESENT: u64 = 1 << 3;
const PAGE_IS_SWAPPED: u64 = 1 << 4;
const PAGE_IS_PFNZERO: u64 = 1 << 5;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PageRegion {
    start: u64,
    end: u64,
    categories: u64,
}

#[repr(C)]
#[derive(Default)]
struct ScanArg {
    size: u64,
    flags: u64,
    start: u64,
    end: u64,
    walk_end: u64,
    vec: u64,
    vec_len: u64,
    max_pages: u64,
    category_inverted: u64,
    category_mask: u64,
    category_anyof_mask: u64,
    return_mask: u64,
}

/// `_IOWR('f', 16, struct pm_scan_arg)`.
const PAGEMAP_SCAN: u64 = (3 << 30) | ((size_of::<ScanArg>() as u64) << 16) | (0x66 << 8) | 16;

/// How many regions a scan returns at a time.
const REGIONS: usize = 512;

/// The private pages from `start` to `end` of the process whose pagemap is `pagemap`, as runs of
/// its addresses, in order, apart.
pub(super) fn private_runs(
    pagemap: &File,
    start: u64,
    end: u64,
) -> std::io::Result<Vec<Range<u64>>> {
    let mut runs: Vec<Range<u64>> = Vec::new();
    let mut regions = vec![PageRegion::default(); REGIONS];
    let mut at = start;
    while at < end {
        let mut arg = ScanArg {
            size: size_of::<ScanArg>() as u64,
            start: at,
            end,
            vec: regions.as_mut_ptr() as u64,
            vec_len: REGIONS as u64,
            // Of no file's, not the zero page: both clear.
            category_inverted: PAGE_IS_FILE | PAGE_IS_PFNZERO,
            category_mask: PAGE_IS_FILE | PAGE_IS_PFNZERO,
            category_anyof_mask: PAGE_IS_PRESENT | PAGE_IS_SWAPPED,
            return_mask: PAGE_IS_PRESENT | PAGE_IS_SWAPPED,
            ..ScanArg::default()
        };
        // SAFETY: PAGEMAP_SCAN, with its argument and the regions it fills, which outlive it.
        let found = unsafe { libc::ioctl(pagemap.as_raw_fd(), PAGEMAP_SCAN as _, &mut arg) };
        if found < 0 {
            return Err(std::io::Error::last_os_error());
        }
        for region in &regions[..found as usize] {
            match runs.last_mut() {
                Some(last) if last.end == region.start => last.end = region.end,
                _ => runs.push(region.start..region.end),
            }
        }
        if arg.walk_end <= at {
            return Err(std::io::Error::other(format!("a pagemap scan stuck at {at:#x}")));
        }
        at = arg.walk_end;
    }
    Ok(runs)
}
