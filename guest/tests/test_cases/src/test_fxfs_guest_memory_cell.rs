// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Direct `/snapshot/create` into a memory cell (TASK-456.4) and KVM dirty-page tracking on
//! cell-backed guest memory (`MemBackendType::Cell`, board 11225):
//!
//! 1. A freshly booted 1 GiB VM, its memory a new memory cell's window as every VM's is (board
//!    14415), writes a 4 MiB marker buffer (`buf_a`), freezes Fxfs, and calls
//!    `PUT /snapshot/create` with `memory_cell: {}` (no local memory file), as a restored VM's
//!    snapshot does: its private pages written into its memory cell. Checks that `StatCell`
//!    stores far less than 1 GiB (the pages the guest wrote, untouched ones holes), and that the
//!    snapshot's version is of the memory cell the VM booted with.
//! 2. Restores `child` from that version with `MemBackendType::Cell` and `track_dirty_pages: true`.
//!    - Round 1: `child` allocates & writes a 4 MiB buffer (`buf_r1`) and mutates 16 shared pages of
//!      `buf_a`. Host pauses `child` and takes a `Diff` snapshot (`KVM_GET_DIRTY_LOG` +
//!      `AtomicBitmap`), verifying all 1024 `buf_r1` pages and all 16 mutated `buf_a` pages appear
//!      in the diff while untouched shared pages are not dirtied.
//!    - Round 2: `child` resumes, re-writes 32 already-private pages of `buf_r1`, mutates 16
//!      previously untouched shared pages of `buf_a`, freezes Fxfs, and signals host. Host pauses
//!      `child` and takes a second `Diff` snapshot, verifying both re-written private pages and
//!      newly privatized shared pages appear while Round-1-only pages are not re-dirtied.
//! 3. While `child` is still paused, host calls `PUT /snapshot/create` with `memory_cell: {}`
//!    (writing only `child`'s private pages straight into its forked memory cell, reclaiming them,
//!    and making version `v2`, with no local memory file). Checks `own` bytes stored << 1 GiB,
//!    then restores `grandchild` from `v2` and verifies every inherited and mutated page across
//!    both buffers and Fxfs.

use macros::{guest, host};

pub struct TestFxfsGuestMemoryCell;

#[cfg_attr(feature = "guest", allow(dead_code))]
const CAPACITY: u64 = 128 << 20;
#[cfg_attr(feature = "guest", allow(dead_code))]
const RAM_MIB: u32 = 1024;
const PORT: u32 = 5053;
const PAGE: usize = 4096;
const BUF_PAGES: usize = 1024; // 4 MiB

const MAGIC_BOOT: u64 = 0x434c_4144_4500_0001;
const MAGIC_R1: u64 = 0x434c_4144_4500_0002;
const MAGIC_R1_MUT: u64 = 0x434c_4144_4500_0003;
const MAGIC_R2_REWRITE: u64 = 0x434c_4144_4500_0004;
const MAGIC_R2_NEW: u64 = 0x434c_4144_4500_0005;

#[cfg_attr(feature = "host", allow(dead_code))]
fn fill_page(page: &mut [u8], magic: u64, index: usize) {
    assert_eq!(page.len(), PAGE);
    page[0..8].copy_from_slice(&magic.to_le_bytes());
    page[8..16].copy_from_slice(&(index as u64).to_le_bytes());
    let seed = (magic as u8).wrapping_add((index & 0xff) as u8);
    for (i, byte) in page[16..].iter_mut().enumerate() {
        *byte = seed ^ (i % 251) as u8;
    }
}

fn check_page(page: &[u8], magic: u64, index: usize) -> bool {
    if page.len() != PAGE {
        return false;
    }
    if u64::from_le_bytes(page[0..8].try_into().unwrap()) != magic
        || u64::from_le_bytes(page[8..16].try_into().unwrap()) != index as u64
    {
        return false;
    }
    let seed = (magic as u8).wrapping_add((index & 0xff) as u8);
    page[16..].iter().enumerate().all(|(i, byte)| *byte == (seed ^ (i % 251) as u8))
}

#[host]
mod host {
    use super::*;
    use crate::common::{cladefs_guest_should_run, setup_rootfs};
    use crate::vm::{Firecracker, Vm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::{Context, ensure};
    use cladefs::cladefsd::control::proto::{
        ListCellsRequest, ShrinkMemoryRequest, StatCellRequest, SyncCellRequest,
    };
    use nix::libc;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};

    #[derive(Debug, serde::Deserialize)]
    struct MemoryCellSnapshot {
        tenant: u64,
        cell: u64,
        version: u64,
    }

    fn uds(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cladefs-mcell-{}-{name}.vsock", std::process::id()))
    }

    fn listen(uds: &Path) -> anyhow::Result<UnixListener> {
        let path = format!("{}_{PORT}", uds.display());
        // Stale socket from a previous attempt, if any.
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).with_context(|| format!("binding {path}"))
    }

    /// Scans allocated (non-hole) 4 KiB pages of a sparse Firecracker diff memory file using
    /// `SEEK_DATA`/`SEEK_HOLE`, returning total non-hole bytes and the page indices found for each
    /// of our marker magics.
    fn scan_diff_file(path: &Path) -> anyhow::Result<(u64, BTreeMap<u64, BTreeSet<usize>>)> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as libc::off_t;
        let fd = file.as_raw_fd();
        let mut dirty_bytes: u64 = 0;
        let mut found: BTreeMap<u64, BTreeSet<usize>> = BTreeMap::new();
        let mut page = [0u8; PAGE];
        let mut pos: libc::off_t = 0;

        while pos < len {
            // SAFETY: plain lseek(SEEK_DATA) on an open regular file descriptor.
            let data = unsafe { libc::lseek(fd, pos, libc::SEEK_DATA) };
            if data < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENXIO) {
                    break;
                }
                return Err(err.into());
            }
            // SAFETY: plain lseek(SEEK_HOLE) on an open regular file descriptor.
            let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
            let end = if hole < 0 { len } else { hole.min(len) };
            let start = (data as u64 / PAGE as u64) * PAGE as u64;
            let stop = (end as u64).min(len as u64);
            dirty_bytes += stop.saturating_sub(start);
            file.seek(SeekFrom::Start(start))?;
            let mut off = start;
            while off + PAGE as u64 <= stop {
                file.read_exact(&mut page)?;
                let magic = u64::from_le_bytes(page[0..8].try_into().unwrap());
                if matches!(
                    magic,
                    MAGIC_BOOT | MAGIC_R1 | MAGIC_R1_MUT | MAGIC_R2_REWRITE | MAGIC_R2_NEW
                ) {
                    let index = u64::from_le_bytes(page[8..16].try_into().unwrap()) as usize;
                    if index < BUF_PAGES && check_page(&page, magic, index) {
                        found.entry(magic).or_default().insert(index);
                    }
                }
                off += PAGE as u64;
            }
            pos = end;
        }
        Ok((dirty_bytes, found))
    }

    impl TestFxfsGuestMemoryCell {
        fn run(&self, test_setup: &TestSetup) -> anyhow::Result<()> {
            let root = setup_rootfs(test_setup)?;
            std::fs::create_dir_all(root.join("mnt/fxfs"))?;
            let dir = &test_setup.tmp_dir;
            let cladefsd: PathBuf =
                std::env::var_os("CLADEFSD").context("CLADEFSD not set")?.into();
            let control = crate::common::control(&cladefsd)?;

            // 1. A freshly booted 1 GiB VM snapshots straight into its memory cell (no memory
            // file), as a restored one does.
            let uds_boot = uds("boot");
            let listen_boot = listen(&uds_boot)?;
            let mut vm = Vm::new(2, RAM_MIB)?;
            vm.guest_kernel()?;
            vm.new_cell_served(
                "extents",
                CAPACITY,
                crate::common::dax_window(crate::common::Window::Large)?,
                &cladefsd,
            );
            vm.read_only_root();
            vm.vsock(&uds_boot);
            let mut boot_fc =
                vm.spawn_api(&root, &test_setup.test_case, "boot", &Vm::api_socket("boot"))?;
            let mut boot_stream =
                boot_fc.accept_within(&listen_boot, Some(std::time::Duration::from_secs(120)))?;
            let mut line = String::new();
            BufReader::new(&mut boot_stream).read_line(&mut line)?;
            ensure!(line.trim() == "frozen-boot", "boot guest said {line:?}");

            // The cells as the VM runs: its memory cell, which its snapshot makes a version of,
            // among them.
            let cells_booted: BTreeSet<(u64, u64)> = control
                .call(async |client| client.list_cells(ListCellsRequest {}).await)?
                .stores
                .into_iter()
                .map(|c| (c.tenant, c.cell))
                .collect();
            boot_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            let snap1 = dir.join("snap1.state");
            let v1: MemoryCellSnapshot = boot_fc.request_json(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": snap1,
                    "memory_cell": {},
                }),
            )?;
            ensure!(v1.cell != 0 && v1.version != 0, "unexpected v1: {v1:?}");
            ensure!(
                cells_booted.contains(&(v1.tenant, v1.cell)),
                "v1 {v1:?} is of no cell the booted VM had: {cells_booted:?}"
            );
            boot_fc.kill()?;

            let boot_stat = control.call(async |client| {
                client.stat_cell(StatCellRequest { tenant: v1.tenant, cell: v1.cell }).await
            })?;
            eprintln!("booted memory cell: used={} bytes (RAM={} MiB)", boot_stat.used, RAM_MIB);
            ensure!(
                boot_stat.used < 256 << 20,
                "the booted VM's memory cell stored {} bytes of a {RAM_MIB} MiB VM",
                boot_stat.used
            );
            control.call(async |client| {
                client.sync_cell(SyncCellRequest { tenant: v1.tenant, cell: v1.cell }).await
            })?;
            control.call(async |client| {
                client.shrink_memory(ShrinkMemoryRequest { tenant: None, target_bytes: 0 }).await
            })?;

            // 2. Restore `child` with MemBackendType::Cell + track_dirty_pages: true.
            let cells_before_child: BTreeSet<(u64, u64)> = control
                .call(async |client| client.list_cells(ListCellsRequest {}).await)?
                .stores
                .into_iter()
                .map(|c| (c.tenant, c.cell))
                .collect();
            let uds_child = uds("child");
            let listen_child = listen(&uds_child)?;
            let mut child_fc =
                Firecracker::spawn(dir, "child", &Vm::api_socket("child"), None, &[])?;
            child_fc.request(
                "PUT",
                "/snapshot/load",
                &serde_json::json!({
                    "snapshot_path": snap1,
                    "mem_backend": {
                        "backend_type": "Cell",
                        "backend_path": "0/0",
                    },
                    "memory_cell": {
                        "tenant": v1.tenant,
                        "cell": v1.cell,
                        "version": v1.version,
                        "cladefsd_socket": cladefsd,
                    },
                    "track_dirty_pages": true,
                    "resume_vm": true,
                    "vsock_override": { "uds_path": uds_child },
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": "0/0",
                    }],
                }),
            )?;

            // Tell guest its role ("child"), then wait for Round 1 writes.
            let mut stream =
                child_fc.accept_within(&listen_child, Some(std::time::Duration::from_secs(120)))?;
            writeln!(stream, "child")?;
            let mut reader = BufReader::new(stream.try_clone()?);
            line.clear();
            reader.read_line(&mut line)?;
            ensure!(line.trim() == "round1", "child guest said {line:?}");

            // Pause and take Diff snapshot 1 (exercises KVM_GET_DIRTY_LOG + AtomicBitmap).
            child_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            let diff1 = dir.join("diff1.mem");
            child_fc.request(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Diff",
                    "snapshot_path": dir.join("diff1.state"),
                    "mem_file_path": diff1,
                }),
            )?;
            let (alloc1, found1) = scan_diff_file(&diff1)?;
            let r1_pages = found1.get(&MAGIC_R1).map_or(0, BTreeSet::len);
            let r1_mut = found1.get(&MAGIC_R1_MUT).map_or(0, BTreeSet::len);
            eprintln!(
                "diff1: allocated={alloc1} bytes, r1={r1_pages}/{BUF_PAGES}, r1_mut={r1_mut}/16"
            );
            ensure!(
                alloc1 < 128 << 20,
                "diff1 allocated {alloc1} bytes of a {RAM_MIB} MiB VM (untouched shared pages dirtied)"
            );
            ensure!(r1_pages == BUF_PAGES, "diff1 missed buf_r1 pages: {r1_pages}/{BUF_PAGES}");
            ensure!(r1_mut == 16, "diff1 missed mutated shared buf_a pages: {r1_mut}/16");

            // Resume `child` for Round 2 writes. Creating `diff1` reset vsock connections, so
            // accept a fresh connection from `child` before sending `continue1`.
            drop(reader);
            drop(stream);
            child_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Resumed" }))?;
            let mut stream =
                child_fc.accept_within(&listen_child, Some(std::time::Duration::from_secs(120)))?;
            writeln!(stream, "continue1")?;
            let mut reader = BufReader::new(stream.try_clone()?);
            line.clear();
            reader.read_line(&mut line)?;
            ensure!(line.trim() == "round2-frozen", "child guest said {line:?}");

            // Pause and take Diff snapshot 2.
            child_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            let diff2 = dir.join("diff2.mem");
            child_fc.request(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Diff",
                    "snapshot_path": dir.join("diff2.state"),
                    "mem_file_path": diff2,
                }),
            )?;
            let (alloc2, found2) = scan_diff_file(&diff2)?;
            let r2_rewrite = found2.get(&MAGIC_R2_REWRITE).map_or(0, BTreeSet::len);
            let r2_new = found2.get(&MAGIC_R2_NEW).map_or(0, BTreeSet::len);
            let r2_stale_r1 = found2.get(&MAGIC_R1).map_or(0, BTreeSet::len);
            let r2_stale_mut = found2.get(&MAGIC_R1_MUT).map_or(0, BTreeSet::len);
            eprintln!(
                "diff2: allocated={alloc2} bytes, rewrite={r2_rewrite}/32, new={r2_new}/16, \
                 stale_r1={r2_stale_r1}, stale_mut={r2_stale_mut}"
            );
            ensure!(alloc2 < 64 << 20, "diff2 allocated {alloc2} bytes");
            ensure!(r2_rewrite == 32, "diff2 missed re-written private pages: {r2_rewrite}/32");
            ensure!(r2_new == 16, "diff2 missed newly privatized pages: {r2_new}/16");
            ensure!(
                r2_stale_r1 == 0 && r2_stale_mut == 0,
                "diff2 re-dirtied untouched Round-1 pages: r1={r2_stale_r1}, mut={r2_stale_mut}"
            );

            // 3. Snapshot `child` straight into its forked memory cell (`memory_cell: {}`).
            let snap2 = dir.join("snap2.state");
            let v2: MemoryCellSnapshot = child_fc.request_json(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": snap2,
                    "memory_cell": {},
                }),
            )?;
            ensure!(v2.cell != v1.cell && v2.version != 0, "unexpected v2: {v2:?}");
            drop(stream);
            child_fc.kill()?;

            let child_stat = control.call(async |client| {
                client.stat_cell(StatCellRequest { tenant: v2.tenant, cell: v2.cell }).await
            })?;
            eprintln!(
                "child forked memory cell: own={} bytes, inherited={} bytes",
                child_stat.own, child_stat.inherited
            );
            ensure!(
                child_stat.own < 128 << 20,
                "forked memory cell wrote {} own bytes (expected only private pages)",
                child_stat.own
            );
            control.call(async |client| {
                client.sync_cell(SyncCellRequest { tenant: v2.tenant, cell: v2.cell }).await
            })?;
            // Sync the child's forked Fxfs cell too.
            let cells_after_child =
                control.call(async |client| client.list_cells(ListCellsRequest {}).await)?.stores;
            for c in cells_after_child {
                if !cells_before_child.contains(&(c.tenant, c.cell))
                    && (c.tenant, c.cell) != (v2.tenant, v2.cell)
                {
                    control.call(async |client| {
                        client.sync_cell(SyncCellRequest { tenant: c.tenant, cell: c.cell }).await
                    })?;
                }
            }

            // 4. Restore `grandchild` from `v2` and verify every page across both buffers & Fxfs.
            let uds_gc = uds("gc");
            let listen_gc = listen(&uds_gc)?;
            let gc_fc =
                Firecracker::spawn(dir, "grandchild", &Vm::api_socket("grandchild"), None, &[])?;
            std::thread::spawn(move || -> std::io::Result<()> {
                for stream in listen_gc.incoming() {
                    writeln!(stream?, "grandchild")?;
                }
                Ok(())
            });
            gc_fc.request(
                "PUT",
                "/snapshot/load",
                &serde_json::json!({
                    "snapshot_path": snap2,
                    "mem_backend": {
                        "backend_type": "Cell",
                        "backend_path": "0/0",
                    },
                    "memory_cell": {
                        "tenant": v2.tenant,
                        "cell": v2.cell,
                        "version": v2.version,
                        "cladefsd_socket": cladefsd,
                    },
                    "resume_vm": true,
                    "vsock_override": { "uds_path": uds_gc },
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": "0/0",
                    }],
                }),
            )?;
            let (status, console) = gc_fc.wait()?;
            crate::vm::split_console(std::fs::File::open(&console)?)?;
            ensure!(status.success(), "grandchild exited {status}");
            Ok(())
        }
    }

    impl Test for TestFxfsGuestMemoryCell {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn timeout_secs(&self) -> u64 {
            240
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            if let Err(err) = self.run(&test_setup) {
                eprintln!("{err:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            if !output.contains("OK grandchild") {
                return TestOutcome::Fail(format!("missing OK grandchild\n{output}"));
            }
            TestOutcome::Pass
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use crate::guest_fxfs::{
        FIFREEZE, FITHAW, MNT, alloc_anon_buf, freeze_mnt as freeze, mount_extents as mount,
        role_from_host as role_on, try_vsock_connect,
    };
    use std::fs;
    use std::io::{BufRead, BufReader, Write};

    fn try_connect() -> std::io::Result<fs::File> {
        try_vsock_connect(PORT)
    }

    fn role_from_host() -> (String, fs::File) {
        role_on(PORT)
    }

    fn alloc_buf() -> &'static mut [u8] {
        alloc_anon_buf(BUF_PAGES * PAGE)
    }

    impl Test for TestFxfsGuestMemoryCell {
        fn in_guest(self: Box<Self>) {
            mount();
            fs::write(format!("{MNT}/stage"), b"boot").unwrap();

            // Allocate and fill `buf_a` before the first snapshot.
            let buf_a = alloc_buf();
            for i in 0..BUF_PAGES {
                fill_page(&mut buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_BOOT, i);
            }

            freeze(FIFREEZE);
            {
                let mut host = try_connect().expect("connecting to host");
                writeln!(host, "frozen-boot").unwrap();
                let mut wait = String::new();
                // Wait for snapshot & restore reset (EOF / ECONNRESET).
                let _ = BufReader::new(host).read_line(&mut wait);
            }

            // Resumed in `child`.
            let (role, mut host) = role_from_host();
            assert_eq!(role, "child");
            freeze(FITHAW);
            assert_eq!(fs::read(format!("{MNT}/stage")).unwrap(), b"boot");
            for i in 0..BUF_PAGES {
                assert!(
                    check_page(&buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_BOOT, i),
                    "buf_a page {i} corrupted after restore"
                );
            }

            // Round 1: allocate fresh buffer `buf_r1` + mutate 16 shared pages of `buf_a`.
            let buf_r1 = alloc_buf();
            for i in 0..BUF_PAGES {
                fill_page(&mut buf_r1[i * PAGE..(i + 1) * PAGE], MAGIC_R1, i);
            }
            for i in 0..16 {
                fill_page(&mut buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_R1_MUT, i);
            }
            freeze(FIFREEZE);
            {
                writeln!(host, "round1").unwrap();
                let mut wait = String::new();
                // Wait for diff1 snapshot's vsock transport reset.
                let _ = BufReader::new(host).read_line(&mut wait);
            }
            let (cmd, mut host) = role_from_host();
            assert_eq!(cmd, "continue1");
            freeze(FITHAW);

            // Round 2: re-write 32 already-private pages of `buf_r1`, mutate 16 untouched shared
            // pages of `buf_a` (100..116), write Fxfs stage, freeze, and signal host.
            for i in 0..32 {
                fill_page(&mut buf_r1[i * PAGE..(i + 1) * PAGE], MAGIC_R2_REWRITE, i);
            }
            for i in 100..116 {
                fill_page(&mut buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_R2_NEW, i);
            }
            fs::write(format!("{MNT}/stage"), b"child-round2").unwrap();
            freeze(FIFREEZE);
            {
                writeln!(host, "round2-frozen").unwrap();
                let mut wait = String::new();
                // Wait for snapshot 2 & grandchild restore reset.
                let _ = BufReader::new(host).read_line(&mut wait);
            }

            // Resumed in `grandchild`.
            let (gc_role, _gc_host) = role_from_host();
            assert_eq!(gc_role, "grandchild");
            freeze(FITHAW);
            assert_eq!(fs::read(format!("{MNT}/stage")).unwrap(), b"child-round2");

            // Verify every single page of both buffers.
            for i in 0..BUF_PAGES {
                let expected_a = if i < 16 {
                    MAGIC_R1_MUT
                } else if (100..116).contains(&i) {
                    MAGIC_R2_NEW
                } else {
                    MAGIC_BOOT
                };
                assert!(
                    check_page(&buf_a[i * PAGE..(i + 1) * PAGE], expected_a, i),
                    "grandchild buf_a page {i} mismatch (expected magic {expected_a:#x})"
                );
                let expected_r1 = if i < 32 { MAGIC_R2_REWRITE } else { MAGIC_R1 };
                assert!(
                    check_page(&buf_r1[i * PAGE..(i + 1) * PAGE], expected_r1, i),
                    "grandchild buf_r1 page {i} mismatch (expected magic {expected_r1:#x})"
                );
            }
            println!("OK grandchild");
        }
    }
}
