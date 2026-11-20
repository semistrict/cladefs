// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a host's store writes to its disk for what a guest writes, by kind, against what its design
//! says it writes (TASK-330.2): a guest's large file, written faster than it commits, its small
//! files, and its large extents, each workload's bytes of each kind bounded by the rules the store writes by (doc-7, On
//! the host; TASK-310), not by what it wrote when the test was written. A daemon that writes a
//! guest's bytes twice, framing in a copy what it frames in place (TASK-310's sandbox host: 2.3
//! times what its guests wrote), or writing back what it wrote back already, writes past them.
//!
//! The store is a host's, as the extent daemon opens it (`crate::host_cell`: its segments of
//! 32 MiB, its extents of 64 MiB at most, large from 4 MiB), on the host's disk, its bucket in
//! memory, uploading nothing until the test is done: what the process wrote to its disk, as its
//! kernel counts it (`/proc/self/io`, Linux's), is the store's alone.

use super::tests::{idle, kept_in};
use crate::test_data::noise;
use crate::{ExtentId, OpenCell, VolatileSlice};
use bucket_cell::object_bucket::ObjectBucket;
use local_cell::Written;
use std::sync::Arc;

/// The bytes of each write a guest's VM process sends: Fxfs's writeback's, of 256 KiB.
const WRITE: usize = 256 << 10;

/// The extents of a guest's large file: Fxfs's, of a MiB each, under the store's large ones
/// (`Options::large_extent`), so that they go to its open segment first.
const LARGE_FILES_EXTENT: usize = 1 << 20;

/// What the store wrote between `before` and `after`, by kind.
fn delta(before: Written, after: Written) -> Written {
    Written {
        raw: after.raw - before.raw,
        copied: after.copied - before.copied,
        framed: after.framed - before.framed,
        own: after.own - before.own,
        log: after.log - before.log,
        writes: after.writes - before.writes,
    }
}

/// Writes `data` to `extent` of `store` as a guest's VM process does, [`WRITE`] bytes a request.
fn write(store: &OpenCell, extent: ExtentId, data: &[u8]) {
    for (at, piece) in data.chunks(WRITE).enumerate() {
        store.write(extent, (at * WRITE) as u64, &[VolatileSlice::from(piece)]).unwrap();
    }
}

/// What this process has had its kernel write to its disk, in bytes, each page as it was dirtied,
/// those of files gone before their writing back among them (`/proc/self/io`'s `write_bytes`).
#[cfg(target_os = "linux")]
fn io_written() -> u64 {
    let io = std::fs::read_to_string("/proc/self/io").unwrap();
    let line = io.lines().find_map(|line| line.strip_prefix("write_bytes: ")).unwrap();
    line.trim().parse().unwrap()
}

/// The CPU time this process has spent, user and system: what the test says the store spent, a
/// figure, not a budget, as a test's build is no measurement's and no design bounds it.
fn cpu() -> std::time::Duration {
    // SAFETY: getrusage writes the struct it is given, which is the caller's.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let time = |time: libc::timeval| {
        std::time::Duration::new(time.tv_sec as u64, time.tv_usec as u32 * 1000)
    };
    time(usage.ru_utime) + time(usage.ru_stime)
}

/// The most bytes a framing of `len` bytes of data writes beyond them: each part's entry in the
/// table, its start aligned, and the trailer (`extent_frame`); its footer, the commits' contents,
/// is the segment's log's, which the store writes too, at most.
fn framing_overhead(len: u64) -> u64 {
    let parts = len.div_ceil(extent_frame::PART) + 1;
    parts * (extent_frame::ENTRY + extent_frame::ALIGN) + extent_frame::TRAILER
}

/// What a host's store writes for a guest's large file and its small files, by kind, is what its
/// design says: each byte the guest writes once, raw; of a large file written faster than the
/// store commits, every extent sealed where it was written, in the segments it filled, which its
/// commit spans, each framed in place, its tail alone written (TASK-92.41, TASK-310); each small
/// file, written in one run of the open segment, never copied, framed once, no larger than it was
/// written; each large extent, in a raw file of its own, framed there in place, its tail alone
/// written, as none of its parts compresses (TASK-310). And
/// (Linux) what its process had the kernel write to its disk is what the store says it wrote, each
/// write a page more than its bytes at most: nothing written twice uncounted.
#[test]
fn a_hosts_store_writes_what_its_design_says() {
    // cladefs: at the shell's limit of open files, not the daemon's: a burst's extents spilled to
    // raw files of their own held two each till its commit, past 1,024 (TASK-332).
    let dir = tempfile::tempdir().unwrap();
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "cells"));
    let store = kept_in(dir.path().join("store"), (4 << 30, None), &bucket, idle()).unwrap();
    let options = crate::host_cell::options();
    let cell = store.cell();
    let mut sequence = 0;
    let mut next = || {
        sequence += 1;
        ExtentId { cell: cell.id, sequence }
    };
    #[cfg(target_os = "linux")]
    let (start, io_before) = (local_cell::written(), io_written());

    // The large file: 768 MiB that don't compress, in two bursts between commits, its fsyncs, each
    // past what the open segment takes (`Options::segment_limit`, 96 MiB), as LangSmith's sandboxes
    // wrote (TASK-310).
    const BURSTS: u64 = 2;
    const BURST: usize = 384 << 20;
    let extents_per_burst = BURST / LARGE_FILES_EXTENT;
    // Its extents' data made first, so that what the process spends meanwhile is the store's: as
    // many as a part, which is compressed alone, needs to find none like it.
    let data: Vec<Vec<u8>> = (0..16).map(|seed| noise(seed, LARGE_FILES_EXTENT)).collect();
    let (before, cpu_before) = (local_cell::written(), cpu());
    for _ in 0..BURSTS {
        for at in 0..extents_per_burst {
            write(&store, next(), &data[at % data.len()]);
        }
        local_cell::upload::wait_on(store.commit_local(None)).unwrap();
    }
    // The last commit's copies framed too, as the open segment closes for its upload.
    local_cell::upload::wait_on(store.frame_open_segment()).unwrap();
    let (large, large_cpu) = (delta(before, local_cell::written()), cpu() - cpu_before);

    // The small files: 2,000 of 8 KiB, which compress, four commits of 500.
    const SMALL_FILES: usize = 2000;
    const SMALL_FILE: usize = 8 << 10;
    let before = local_cell::written();
    for commit in 0..4 {
        for file in 0..SMALL_FILES / 4 {
            let seed = (commit * SMALL_FILES + file) as u8;
            let data: Vec<u8> = (0..SMALL_FILE).map(|i| seed ^ (i % 251) as u8).collect();
            write(&store, next(), &data);
        }
        local_cell::upload::wait_on(store.commit_local(None)).unwrap();
    }
    // Their segment framed, as it closes for its upload.
    local_cell::upload::wait_on(store.frame_open_segment()).unwrap();
    let small = delta(before, local_cell::written());

    // The large extents: 4 of 8 MiB, which don't compress, each reaching past
    // `Options::large_extent` with its first write, its last piece first, as a guest's sparse
    // file's does (TASK-142): in a raw file of its own from then on, sealed at their commit.
    const LARGE_EXTENTS: usize = 4;
    let large_extent = 2 * options.large_extent as usize;
    let before = local_cell::written();
    let mut tails = 0;
    for seed in 0..LARGE_EXTENTS {
        let (extent, data) = (next(), noise(16 + seed as u64, large_extent));
        let (first, last) = data.split_at(large_extent - WRITE);
        store.write(extent, first.len() as u64, &[VolatileSlice::from(last)]).unwrap();
        write(&store, extent, first);
        tails += framing_overhead(large_extent as u64)
            + bucket_cell::layout::encode(&extent).len() as u64;
    }
    local_cell::upload::wait_on(store.commit_local(None)).unwrap();
    let own = delta(before, local_cell::written());

    let guests = (BURSTS as usize * BURST) as u64;
    eprintln!("large file, {} MiB: {large:?}, {large_cpu:?} of CPU", guests >> 20);
    eprintln!("small files, {} MiB: {small:?}", (SMALL_FILES * SMALL_FILE) >> 20);
    eprintln!("large extents, {} MiB: {own:?}", (LARGE_EXTENTS * large_extent) >> 20);
    // Each byte the guest wrote, written once, raw, to the open segment or a raw file of its
    // extent's own.
    assert_eq!(large.raw, guests);
    assert_eq!(small.raw, (SMALL_FILES * SMALL_FILE) as u64);
    // The large file: each extent one run where it was written, in the segments each burst filled,
    // its commit spanning them; nothing copied, nor in a file of its own, each segment framed in
    // place, its tail alone written, as it doesn't compress: one for each segment's worth, and one
    // for each commit's last, and the open one's (TASK-92.41).
    let limit = options.segment_limit();
    let rewritten = large.copied + large.own + large.framed;
    let segments = guests / options.segment_size + 2 * BURSTS + 1;
    let bound = segments * framing_overhead(limit);
    assert!(
        rewritten <= bound,
        "the large file's {} MiB rewritten: {} MiB at most, by design",
        rewritten >> 20,
        bound >> 20
    );
    // The small files: each in one run of the open segment, sealed where it is, never copied nor
    // in a file of its own; framed once, compressed, no larger than raw but for the framing's own.
    assert_eq!((small.copied, small.own), (0, 0));
    let bound = small.raw + framing_overhead(small.raw) + small.log;
    assert!(small.framed <= bound, "small files framed in {} bytes: {bound} at most", small.framed);
    // The large extents: each byte written once, raw, to its own file, never copied; framed in
    // place, as none of its parts compresses, its tail alone written: its parts' table, the
    // trailer and its footer (TASK-310).
    assert_eq!((own.raw, own.copied), ((LARGE_EXTENTS * large_extent) as u64, 0));
    assert!(
        own.own <= tails,
        "large extents' own files written {} bytes: {tails} at most",
        own.own
    );

    // What the process wrote to its disk is what the store says it wrote: each write's bytes, and
    // a page more at most, the last it dirtied in part.
    #[cfg(target_os = "linux")]
    {
        let (all, io) = (delta(start, local_cell::written()), io_written() - io_before);
        let counted = [all.raw, all.copied, all.framed, all.own, all.log].iter().sum::<u64>();
        let bound = counted + all.writes * crate::pages::page_size();
        assert!(
            io <= bound,
            "the kernel wrote {} MiB for the store, which says {} MiB in {} writes: {} MiB at most",
            io >> 20,
            counted >> 20,
            all.writes,
            bound >> 20
        );
    }
    drop(store);
}
