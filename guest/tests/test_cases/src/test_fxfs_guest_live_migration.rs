// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! End-to-end live migration of a running Firecracker VM with a bucket-backed extent cell and a
//! shared memory-cell base:
//!
//! 1. Cold-booted 1 GiB VM writes `/mnt/fxfs/stage` and a 4 MiB RAM buffer (`buf_a`), freezes
//!    Fxfs, and snapshots straight into memory cell version `v1`.
//! 2. Restores `src` on daemon A with `MemBackendType::Cell` (`v1`, `track_dirty_pages: true`) and
//!    bucket-backed cell `store-src`.
//! 3. **No-freeze migration snapshot + abort-safe cell release/reopen:**
//!    - `src` writes `/mnt/fxfs/before_abort`, syncs (WITHOUT `FIFREEZE`), writes `buf_r1` and
//!      mutates 16 pages of `buf_a`.
//!    - Host pauses `src`, verifies a normal snapshot without `for_migration` is refused
//!      (`NotFrozen`), then takes a `Diff` snapshot with `for_migration: true` into `diff1.mem`
//!      (creating 0 versions on `store-src`).
//!    - Host releases the cell via `PATCH /cladefs/extents` (`{"state": "Released"}`), verifies
//!      `Resume` and `CreateSnapshot` are refused while released, then aborts by reopening via
//!      `PATCH /cladefs/extents` (`{"state": "Open"}`) and resumes `src` on daemon A.
//! 4. **Live migration through the bucket (`SyncCell` -> `CreateCell` + `PlaceCell` ->
//!    `PUT /snapshot/load` -> `DropCell`) + `memory_cell.diff_files`:**
//!    - `src` verifies DAX mappings survived reopen, writes `/mnt/fxfs/after_abort`, mutates 32
//!      pages of `buf_r1`, 16 new pages of `buf_a`, and zeroes 8 pages of `buf_a`, then syncs
//!      (still without `FIFREEZE`).
//!    - Host pauses `src`, takes a second `Diff` snapshot with `for_migration: true` into
//!      `diff2.mem` (still 0 versions), and releases `extents` on paused `src` (without killing
//!      it).
//!    - Source daemon runs `SyncCell` to upload every commit to the bucket.
//!    - Target daemon runs `CreateCell` (for the existing cell in the bucket) + `PlaceCell` with
//!      the new fencing token (`2`), then loads `dst` on daemon B from `v1` +
//!      `diff_files: [diff1, diff2]` and `store_src` (which opens the cell directly and fences the
//!      source), and resumes `dst`.
//!    - Source daemon runs `DropCell` on the fenced cell, kills paused `src`, and verifies `dst`
//!      resumes with all disk files, all RAM pages (shared base, diff1, diff2, zeroed),
//!      `own < 128 MiB` on `mem-dst`, and 0 versions on `store-dst`.

use macros::{guest, host};

pub struct TestFxfsGuestLiveMigration;

#[cfg_attr(feature = "guest", allow(dead_code))]
const CAPACITY: u64 = 128 << 20;
#[cfg_attr(feature = "guest", allow(dead_code))]
const RAM_MIB: u32 = 1024;
const PORT: u32 = 5054;
#[cfg_attr(feature = "host", allow(dead_code))]
const PAGE: usize = 4096;
#[cfg_attr(feature = "host", allow(dead_code))]
const BUF_PAGES: usize = 1024; // 4 MiB

#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_BOOT: u64 = 0x4d49_4752_0000_0001;
#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_R1: u64 = 0x4d49_4752_0000_0002;
#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_R1_MUT: u64 = 0x4d49_4752_0000_0003;
#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_R2_REWRITE: u64 = 0x4d49_4752_0000_0004;
#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_R2_NEW: u64 = 0x4d49_4752_0000_0005;

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

#[cfg_attr(feature = "host", allow(dead_code))]
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
    use crate::common::{cladefs_guest_should_run, control, setup_rootfs, spawn_daemon};
    use crate::vm::{Firecracker, Vm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::{Context, ensure};
    use cladefs::cladefsd::control::proto::{
        CreateCellRequest, DropCellRequest, ListCellsRequest, ListVersionsRequest,
        PlaceCellRequest, ShrinkMemoryRequest, StatCellRequest, SyncCellRequest,
    };
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};

    #[derive(Debug, serde::Deserialize)]
    struct MemoryCellSnapshot {
        tenant: u64,
        cell: u64,
        version: u64,
    }

    fn uds(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cladefs-mig-{}-{name}.vsock", std::process::id()))
    }

    fn listen(uds: &Path) -> anyhow::Result<UnixListener> {
        let path = format!("{}_{PORT}", uds.display());
        // Stale socket from a previous attempt, if any.
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).with_context(|| format!("binding {path}"))
    }

    impl TestFxfsGuestLiveMigration {
        fn run(&self, test_setup: &TestSetup) -> anyhow::Result<()> {
            let root = setup_rootfs(test_setup)?;
            std::fs::create_dir_all(root.join("mnt/fxfs"))?;
            let dir = &test_setup.tmp_dir;
            let bucket = dir.join("shared.bucket");
            std::fs::create_dir_all(&bucket)?;
            let bucket_url = format!("file://{}/", bucket.display());
            let src_roots = dir.join("src-cells");
            let dst_roots = dir.join("dst-cells");
            std::fs::create_dir_all(&src_roots)?;
            std::fs::create_dir_all(&dst_roots)?;

            // Short, as a Unix socket's path is (SUN_LEN, board 14393): the case's directory
            // may not be.
            let sockets = tempfile::Builder::new().prefix("cladefs-mig").tempdir()?;
            let src_sock = sockets.path().join("src.sock");
            let dst_sock = sockets.path().join("dst.sock");
            let _src_daemon = spawn_daemon(
                test_setup,
                &src_sock,
                "src",
                &[
                    ("CLADEFSD_BUCKET", OsStr::new(&bucket_url)),
                    ("CLADEFSD_TOKEN", OsStr::new("1")),
                    ("CLADEFSD_ROOTS", src_roots.as_os_str()),
                    ("CLADEFSD_UPLOAD_INTERVAL", OsStr::new("3600")),
                ],
                Some(1 << 30),
            )?;
            let _dst_daemon = spawn_daemon(
                test_setup,
                &dst_sock,
                "dst",
                &[
                    ("CLADEFSD_BUCKET", OsStr::new(&bucket_url)),
                    ("CLADEFSD_TOKEN", OsStr::new("1")),
                    ("CLADEFSD_ROOTS", dst_roots.as_os_str()),
                ],
                Some(1 << 30),
            )?;
            let src_ctrl = control(&src_sock)?;
            let dst_ctrl = control(&dst_sock)?;

            // 1. Cold-boot `boot` VM on daemon A and snapshot into memory cell version `v1`.
            let uds_boot = uds("boot");
            let listen_boot = listen(&uds_boot)?;
            let mut vm = Vm::new(2, RAM_MIB)?;
            vm.guest_kernel()?;
            vm.new_cell_served(
                "extents",
                CAPACITY,
                crate::common::dax_window(crate::common::Window::Large)?,
                &src_sock,
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

            let boot_cells =
                src_ctrl.call(async |c| c.list_cells(ListCellsRequest {}).await)?.stores;
            ensure!(boot_cells.len() == 1, "expected 1 boot cell, got {}", boot_cells.len());
            let store_boot = (boot_cells[0].tenant, boot_cells[0].cell);

            boot_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            let snap1 = dir.join("snap1.state");
            let v1: MemoryCellSnapshot = boot_fc.request_json(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": snap1,
                    "memory_cell": {
                        "path_on_host": "0/0",
                        "cladefsd_socket": src_sock,
                    },
                }),
            )?;
            ensure!(v1.cell != 0 && v1.version != 0, "unexpected v1: {v1:?}");
            boot_fc.kill()?;

            src_ctrl.call(async |c| {
                c.sync_cell(SyncCellRequest { tenant: v1.tenant, cell: v1.cell }).await
            })?;
            src_ctrl.call(async |c| {
                c.sync_cell(SyncCellRequest { tenant: store_boot.0, cell: store_boot.1 }).await
            })?;
            src_ctrl.call(async |c| {
                c.shrink_memory(ShrinkMemoryRequest { tenant: None, target_bytes: 0 }).await
            })?;

            // 2. Restore `src` on daemon A from `v1` with dirty-page tracking.
            let src_before: BTreeSet<(u64, u64)> = src_ctrl
                .call(async |c| c.list_cells(ListCellsRequest {}).await)?
                .stores
                .into_iter()
                .map(|c| (c.tenant, c.cell))
                .collect();
            let uds_src = uds("src");
            let listen_src = listen(&uds_src)?;
            let mut src_fc = Firecracker::spawn(dir, "src", &Vm::api_socket("src"), None, &[])?;
            src_fc.request(
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
                        "cladefsd_socket": src_sock,
                    },
                    "track_dirty_pages": true,
                    "resume_vm": true,
                    "vsock_override": { "uds_path": uds_src },
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": "0/0",
                        "cladefsd_socket": src_sock,
                    }],
                }),
            )?;

            let store_src = src_ctrl
                .call(async |c| c.list_cells(ListCellsRequest {}).await)?
                .stores
                .into_iter()
                .find(|c| {
                    !src_before.contains(&(c.tenant, c.cell))
                        && c.stat.as_ref().is_some_and(|s| s.capacity == CAPACITY)
                })
                .map(|c| (c.tenant, c.cell))
                .context("finding restored src extent cell")?;

            let mut stream =
                src_fc.accept_within(&listen_src, Some(std::time::Duration::from_secs(120)))?;
            writeln!(stream, "src")?;
            let mut reader = BufReader::new(stream.try_clone()?);
            line.clear();
            reader.read_line(&mut line)?;
            ensure!(line.trim() == "round1-ready", "src guest said {line:?}");

            // 3. Test no-freeze snapshot (`for_migration: true`) + abort-safe release & reopen.
            src_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            // Normal snapshot without `for_migration` must fail because guest did not fsfreeze.
            ensure!(
                src_fc
                    .request(
                        "PUT",
                        "/snapshot/create",
                        &serde_json::json!({
                            "snapshot_type": "Diff",
                            "snapshot_path": dir.join("unfrozen-fail.state"),
                            "mem_file_path": dir.join("unfrozen-fail.mem"),
                        }),
                    )
                    .is_err(),
                "normal snapshot must be refused when guest filesystem is not frozen"
            );

            let diff1 = dir.join("diff1.mem");
            src_fc.request(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Diff",
                    "snapshot_path": dir.join("mig1.state"),
                    "mem_file_path": diff1,
                    "for_migration": true,
                }),
            )?;
            let src_versions = src_ctrl.call(async |c| {
                c.list_versions(ListVersionsRequest { tenant: store_src.0, cell: store_src.1 })
                    .await
            })?;
            ensure!(
                src_versions.versions.is_empty(),
                "for_migration snapshot must not create versions: {:?}",
                src_versions.versions
            );

            // Release cell on paused VM; verify Resume & CreateSnapshot are refused while released.
            src_fc.request(
                "PATCH",
                "/cladefs/extents",
                &serde_json::json!({ "state": "Released" }),
            )?;
            ensure!(
                src_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Resumed" })).is_err(),
                "Resume must be refused while extent cell is released"
            );
            ensure!(
                src_fc
                    .request(
                        "PUT",
                        "/snapshot/create",
                        &serde_json::json!({
                            "snapshot_type": "Diff",
                            "snapshot_path": dir.join("released-fail.state"),
                            "mem_file_path": dir.join("released-fail.mem"),
                            "for_migration": true,
                        }),
                    )
                    .is_err(),
                "CreateSnapshot must be refused while extent cell is released"
            );

            // Abort path: reopen cell on source and resume VM!
            src_fc.request("PATCH", "/cladefs/extents", &serde_json::json!({ "state": "Open" }))?;
            drop(reader);
            drop(stream);
            src_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Resumed" }))?;
            let mut stream =
                src_fc.accept_within(&listen_src, Some(std::time::Duration::from_secs(120)))?;
            writeln!(stream, "continue-after-abort")?;
            let mut reader = BufReader::new(stream.try_clone()?);
            line.clear();
            reader.read_line(&mut line)?;
            ensure!(line.trim() == "round2-ready", "src guest said {line:?}");

            // 4. Live migration through the bucket:
            //    (1) pause + for_migration diff snapshot + release cell on source VM,
            //    (2) SyncCell on source daemon,
            //    (3) CreateCell + PlaceCell(token=2) on target daemon + PUT /snapshot/load + resume,
            //    (4) DropCell on source daemon.
            let uds_dst = uds("dst");
            let listen_dst = listen(&uds_dst)?;
            let dst_fc = Firecracker::spawn(dir, "dst", &Vm::api_socket("dst"), None, &[])?;
            std::thread::spawn(move || -> std::io::Result<()> {
                for stream in listen_dst.incoming() {
                    writeln!(stream?, "dst")?;
                }
                Ok(())
            });

            let pause_start = std::time::Instant::now();
            src_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            let snap_mig2 = dir.join("mig2.state");
            let diff2 = dir.join("diff2.mem");
            src_fc.request(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Diff",
                    "snapshot_path": snap_mig2,
                    "mem_file_path": diff2,
                    "for_migration": true,
                }),
            )?;
            // Release cell while `src_fc` stays alive, then upload every commit to the bucket.
            src_fc.request(
                "PATCH",
                "/cladefs/extents",
                &serde_json::json!({ "state": "Released" }),
            )?;
            let t_snap_release = pause_start.elapsed();

            src_ctrl.call(async |c| {
                c.sync_cell(SyncCellRequest { tenant: store_src.0, cell: store_src.1 }).await
            })?;
            let t_sync = pause_start.elapsed();

            // Target takes the cell from the bucket and places it at fencing token 2.
            dst_ctrl.call(async |c| {
                c.create_cell(CreateCellRequest {
                    capacity: CAPACITY,
                    tenant: store_src.0,
                    cell: store_src.1,
                    ..Default::default()
                })
                .await
            })?;
            dst_ctrl.call(async |c| {
                c.place_cell(PlaceCellRequest { tenant: store_src.0, cell: store_src.1, token: 2 })
                    .await
            })?;
            let t_take_place = pause_start.elapsed();

            // Load on destination daemon B from base memory cell `v1` + `[diff1, diff2]` and
            // taken cell `store_src`.
            let store_dst_ref =
                cladefs::CellId { tenant: store_src.0, id: store_src.1 }.to_string();
            dst_fc.request(
                "PUT",
                "/snapshot/load",
                &serde_json::json!({
                    "snapshot_path": snap_mig2,
                    "mem_backend": {
                        "backend_type": "Cell",
                        "backend_path": "0/0",
                    },
                    "memory_cell": {
                        "tenant": v1.tenant,
                        "cell": v1.cell,
                        "version": v1.version,
                        "cladefsd_socket": dst_sock,
                        "diff_files": [diff1, diff2],
                    },
                    "resume_vm": false,
                    "vsock_override": { "uds_path": uds_dst },
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": store_dst_ref,
                        "cladefsd_socket": dst_sock,
                    }],
                }),
            )?;
            // Capture the newly forked destination memory cell ID while `dst_fc` holds it open
            // (`list_cells` returns open cells only).
            let dst_mem = dst_ctrl
                .call(async |c| c.list_cells(ListCellsRequest {}).await)?
                .stores
                .into_iter()
                .find(|c| (c.tenant, c.cell) != store_src)
                .map(|c| (c.tenant, c.cell))
                .context("finding dst memory cell")?;
            dst_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Resumed" }))?;
            let pause_total = pause_start.elapsed();
            eprintln!(
                "bucket live migration pause: total={:.1}ms (snap+release={:.1}ms, \
                 sync={:.1}ms, create+place={:.1}ms, load+resume={:.1}ms)",
                pause_total.as_secs_f64() * 1e3,
                t_snap_release.as_secs_f64() * 1e3,
                (t_sync - t_snap_release).as_secs_f64() * 1e3,
                (t_take_place - t_sync).as_secs_f64() * 1e3,
                (pause_total - t_take_place).as_secs_f64() * 1e3,
            );

            // Step 4: DropCell on the fenced source (while `src_fc` is still alive/paused with
            // `extents` released), then kill `src_fc`.
            src_ctrl.call(async |c| {
                c.drop_cell(DropCellRequest { tenant: store_src.0, cell: store_src.1 }).await
            })?;
            drop(reader);
            drop(stream);
            src_fc.kill()?;

            let (status, console) = dst_fc.wait()?;
            crate::vm::split_console(std::fs::File::open(&console)?)?;
            ensure!(status.success(), "dst exited {status}");

            let dst_mem_stat = dst_ctrl.call(async |c| {
                c.stat_cell(StatCellRequest { tenant: dst_mem.0, cell: dst_mem.1 }).await
            })?;
            eprintln!(
                "dst memory cell: own={} bytes, inherited={} bytes",
                dst_mem_stat.own, dst_mem_stat.inherited
            );
            ensure!(
                dst_mem_stat.own < 128 << 20,
                "dst memory cell wrote {} own bytes (expected base pages shared)",
                dst_mem_stat.own
            );
            let dst_versions = dst_ctrl.call(async |c| {
                c.list_versions(ListVersionsRequest { tenant: store_src.0, cell: store_src.1 })
                    .await
            })?;
            ensure!(
                dst_versions.versions.is_empty(),
                "handed-over cell should have 0 versions: {:?}",
                dst_versions.versions
            );
            Ok(())
        }
    }

    impl Test for TestFxfsGuestLiveMigration {
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
            if !output.contains("OK migrated") {
                return TestOutcome::Fail(format!("missing OK migrated\n{output}"));
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
    use nix::libc;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;

    fn try_connect() -> std::io::Result<fs::File> {
        try_vsock_connect(PORT)
    }

    fn sync_fs() {
        let dir = fs::File::open(MNT).unwrap();
        // SAFETY: syncfs(2) on an open directory descriptor inside the mounted filesystem.
        let ret = unsafe { libc::syncfs(dir.as_raw_fd()) };
        assert_eq!(ret, 0, "syncfs: {}", std::io::Error::last_os_error());
        // If DAX is active, ask the guest DAX worker to rebind any clean private pages back to
        // their sealed extents (without freezing the filesystem).
        let knob = "/sys/module/cladefs/parameters/dax_rebind_now";
        if std::path::Path::new(knob).exists() {
            fs::write(knob, b"1").unwrap();
            let asked = crate::guest_fxfs::stats("extents")["dax_rebinds_asked"];
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while crate::guest_fxfs::stats("extents")["dax_rebinds_done"] < asked {
                assert!(std::time::Instant::now() < deadline, "rebind {asked} never done");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    fn role_from_host() -> (String, fs::File) {
        role_on(PORT)
    }

    fn alloc_buf() -> &'static mut [u8] {
        alloc_anon_buf(BUF_PAGES * PAGE)
    }

    impl Test for TestFxfsGuestLiveMigration {
        fn in_guest(self: Box<Self>) {
            mount();
            fs::write(format!("{MNT}/stage"), b"boot").unwrap();

            let buf_a = alloc_buf();
            for i in 0..BUF_PAGES {
                fill_page(&mut buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_BOOT, i);
            }

            freeze(FIFREEZE);
            {
                let mut host = try_connect().expect("connecting to host");
                writeln!(host, "frozen-boot").unwrap();
                let mut wait = String::new();
                let _ = BufReader::new(host).read_line(&mut wait);
            }

            // Resumed in `src` on daemon A.
            let (role, mut host) = role_from_host();
            assert_eq!(role, "src");
            freeze(FITHAW);
            assert_eq!(fs::read(format!("{MNT}/stage")).unwrap(), b"boot");

            // Round 1: write file & mutate RAM, syncfs WITHOUT fsfreeze!
            let buf_r1 = alloc_buf();
            for i in 0..BUF_PAGES {
                fill_page(&mut buf_r1[i * PAGE..(i + 1) * PAGE], MAGIC_R1, i);
            }
            for i in 0..16 {
                fill_page(&mut buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_R1_MUT, i);
            }
            fs::write(format!("{MNT}/before_abort"), b"survived-release-reopen").unwrap();
            sync_fs();
            {
                writeln!(host, "round1-ready").unwrap();
                let mut wait = String::new();
                let _ = BufReader::new(host).read_line(&mut wait);
            }

            // Resumed on `src` after abort (release -> open -> resume), still unfrozen!
            let (cmd, mut host) = role_from_host();
            assert_eq!(cmd, "continue-after-abort");
            assert_eq!(
                fs::read(format!("{MNT}/before_abort")).unwrap(),
                b"survived-release-reopen"
            );

            // Round 2: mutate RAM (including zeroing 8 previously non-zero base pages!) + write
            // another file, syncfs WITHOUT fsfreeze!
            for i in 0..32 {
                fill_page(&mut buf_r1[i * PAGE..(i + 1) * PAGE], MAGIC_R2_REWRITE, i);
            }
            for i in 100..116 {
                fill_page(&mut buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_R2_NEW, i);
            }
            for i in 200..208 {
                buf_a[i * PAGE..(i + 1) * PAGE].fill(0);
            }
            fs::write(format!("{MNT}/after_abort"), b"handed-over-to-target").unwrap();
            sync_fs();
            {
                writeln!(host, "round2-ready").unwrap();
                let mut wait = String::new();
                let _ = BufReader::new(host).read_line(&mut wait);
            }

            // Resumed on `dst` (daemon B), never frozen so no FITHAW needed!
            let (dst_role, _dst_host) = role_from_host();
            assert_eq!(dst_role, "dst");
            assert_eq!(fs::read(format!("{MNT}/stage")).unwrap(), b"boot");
            assert_eq!(
                fs::read(format!("{MNT}/before_abort")).unwrap(),
                b"survived-release-reopen"
            );
            assert_eq!(fs::read(format!("{MNT}/after_abort")).unwrap(), b"handed-over-to-target");
            fs::write(format!("{MNT}/on_target"), b"target-writes-work").unwrap();
            assert_eq!(fs::read(format!("{MNT}/on_target")).unwrap(), b"target-writes-work");

            for i in 0..BUF_PAGES {
                if (200..208).contains(&i) {
                    assert!(
                        buf_a[i * PAGE..(i + 1) * PAGE].iter().all(|&b| b == 0),
                        "dst buf_a zeroed page {i} was not zeroed"
                    );
                } else {
                    let expected_a = if i < 16 {
                        MAGIC_R1_MUT
                    } else if (100..116).contains(&i) {
                        MAGIC_R2_NEW
                    } else {
                        MAGIC_BOOT
                    };
                    assert!(
                        check_page(&buf_a[i * PAGE..(i + 1) * PAGE], expected_a, i),
                        "dst buf_a page {i} mismatch (expected magic {expected_a:#x})"
                    );
                }
                let expected_r1 = if i < 32 { MAGIC_R2_REWRITE } else { MAGIC_R1 };
                assert!(
                    check_page(&buf_r1[i * PAGE..(i + 1) * PAGE], expected_r1, i),
                    "dst buf_r1 page {i} mismatch (expected magic {expected_r1:#x})"
                );
            }
            println!("OK migrated");
        }
    }
}
