// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host-local suspend/resume of a Firecracker VM running on an ephemeral Fxfs rootfs cell
//! (`ForkCell(..., ephemeral: true)`, doc-25) with a standalone host-local ephemeral memory cell:
//!
//! 1. Fork an ephemeral Fxfs rootfs cell from a bucket version on a dedicated daemon with
//!    `CLADEFSD_EPHEMERAL`. Record baseline bucket `uploaded_bytes`.
//! 2. Cold-boot Firecracker VM #1 on the ephemeral rootfs cell. Guest mounts Fxfs, writes files
//!    and a 4 MiB RAM marker buffer (`buf_a`), freezes Fxfs, and signals host.
//! 3. Host pauses VM #1, calls `PUT /snapshot/create` with
//!    `memory_cell: { path_on_host, cladefsd_socket, ephemeral: true }`, verifies the response has
//!    `ephemeral: true` and `version: 0`, stops Firecracker #1 (freeing 100% of host RAM), evicts
//!    the daemon's cache, and asserts zero bucket uploads occurred.
//! 4. Host resumes Firecracker VM #2 from snapshot #1 with `MemBackendType::Cell` +
//!    `memory_cell: { tenant, cell, cladefsd_socket, ephemeral: true }` + `cladefs_overrides`
//!    pointing at the same ephemeral rootfs cell. Guest thaws Fxfs, verifies files and `buf_a`,
//!    writes more Fxfs files and mutates/allocates RAM (`buf_a` + `buf_b`), freezes Fxfs, and
//!    signals host.
//! 5. Host pauses VM #2, re-suspends in place with `memory_cell: {}`, stops Firecracker #2, and
//!    asserts zero bucket uploads occurred.
//! 6. Host attempts to restore stale snapshot #1 against the now-advanced ephemeral rootfs cell
//!    and asserts `PUT /snapshot/load` is refused (`EEXIST`).
//! 7. Host resumes Firecracker VM #3 from snapshot #2, verifies all Fxfs writes and RAM pages
//!    across `buf_a` and `buf_b`, deletes both ephemeral cells via `DeleteCell`, and asserts zero
//!    bucket uploads across the entire suspend -> stop -> resume -> write -> re-suspend -> delete
//!    lifecycle.

use macros::{guest, host};

pub struct TestFxfsGuestEphemeralSuspend;

#[cfg_attr(feature = "guest", allow(dead_code))]
const CAPACITY: u64 = 128 << 20;
#[cfg_attr(feature = "guest", allow(dead_code))]
const RAM_MIB: u32 = 512;
const PORT: u32 = 5054;
#[cfg_attr(feature = "host", allow(dead_code))]
const PAGE: usize = 4096;
#[cfg_attr(feature = "host", allow(dead_code))]
const BUF_PAGES: usize = 1024; // 4 MiB

#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_BOOT: u64 = 0x4550_4845_4d00_0001;
#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_R1_MUT: u64 = 0x4550_4845_4d00_0002;
#[cfg_attr(feature = "host", allow(dead_code))]
const MAGIC_R1_NEW: u64 = 0x4550_4845_4d00_0003;

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
    use crate::common::{cladefs_guest_should_run, setup_rootfs, spawn_daemon};
    use crate::vm::{Firecracker, Vm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::{Context, ensure};
    use cladefs::cladefsd::control::proto::{
        CellKind, CreateCellRequest, DeleteCellRequest, ForkCellRequest, GetCountersRequest,
        MakeVersionRequest, ShrinkMemoryRequest, StatCellRequest, SyncCellRequest,
    };
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};

    #[derive(Debug, serde::Deserialize)]
    struct MemoryCellSnapshot {
        tenant: u64,
        cell: u64,
        version: u64,
        #[serde(default)]
        ephemeral: bool,
    }

    fn uds(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cladefs-eph-susp-{}-{name}.vsock", std::process::id()))
    }

    fn listen(uds: &Path) -> anyhow::Result<UnixListener> {
        let path = format!("{}_{PORT}", uds.display());
        // Stale socket from a previous attempt, if any.
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).with_context(|| format!("binding {path}"))
    }

    fn uploaded_bytes(control: &cladefs::testing::Control) -> anyhow::Result<u64> {
        let counters =
            control.call(async |client| client.get_counters(GetCountersRequest {}).await)?;
        Ok(counters
            .object_storage
            .map(|stats| stats.uploaded_bytes.values().copied().sum())
            .unwrap_or(0))
    }

    impl TestFxfsGuestEphemeralSuspend {
        fn run(&self, test_setup: &TestSetup) -> anyhow::Result<()> {
            let root = setup_rootfs(test_setup)?;
            std::fs::create_dir_all(root.join("mnt/fxfs"))?;
            let dir = &test_setup.tmp_dir;

            // Dedicated daemon with its own bucket + CLADEFSD_EPHEMERAL so we can fork
            // ephemerally and assert exact zero bucket uploads across the VM lifecycle.
            let sock_dir =
                std::env::temp_dir().join(format!("cladefs-eph-susp-{}", std::process::id()));
            std::fs::create_dir_all(&sock_dir)?;
            let socket = sock_dir.join("cladefsd.sock");
            let bucket_dir = dir.join("ephemeral.bucket");
            let bucket_url = format!("file://{}/", bucket_dir.display());
            let ephemeral_dir = dir.join("ephemeral-host");
            std::fs::create_dir_all(&ephemeral_dir)?;
            let mut daemon = spawn_daemon(
                test_setup,
                &socket,
                "ephemeral-suspend",
                &[
                    ("CLADEFSD_BUCKET", OsStr::new(&bucket_url)),
                    ("CLADEFSD_EPHEMERAL", ephemeral_dir.as_os_str()),
                ],
                None,
            )?;
            let control = crate::common::control(&socket)?;

            // 1. Create a base Fxfs cell, make a version, sync, and fork ephemerally.
            let base = control.call(async |client| {
                client
                    .create_cell(CreateCellRequest {
                        capacity: CAPACITY,
                        kind: CellKind::Fxfs.into(),
                        ..Default::default()
                    })
                    .await
            })?;
            let base_ver = control.call(async |client| {
                client
                    .make_version(MakeVersionRequest {
                        tenant: base.tenant,
                        cell: base.cell,
                        keep_last: 0,
                    })
                    .await
            })?;
            control.call(async |client| {
                client.sync_cell(SyncCellRequest { tenant: base.tenant, cell: base.cell }).await
            })?;

            let forked = control.call(async |client| {
                client
                    .fork_cell(ForkCellRequest {
                        tenant: base.tenant,
                        cell: base.cell,
                        version: base_ver.version,
                        persistent: Some(false),
                        ..Default::default()
                    })
                    .await
            })?;
            ensure!(forked.cell != 0, "ephemeral fork returned zero cell");
            let eph_root_id = cladefs::CellId { tenant: forked.tenant, id: forked.cell };
            let eph_root_name = eph_root_id.to_string();

            // Record baseline bucket uploads after the ephemeral fork's reference is in place.
            let baseline_uploads = uploaded_bytes(&control)?;

            // 2. Cold-boot Firecracker #1 on the ephemeral rootfs cell.
            let uds_boot = uds("boot");
            let listen_boot = listen(&uds_boot)?;
            let mut vm = Vm::new(2, RAM_MIB)?;
            vm.guest_kernel()?;
            vm.cell_id_served(
                "extents",
                eph_root_id,
                CAPACITY,
                crate::common::dax_window(crate::common::Window::Large)?,
                &socket,
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

            // 3. Suspend VM #1 into an ephemeral memory cell + local ephemeral rootfs checkpoint.
            boot_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            let snap1 = dir.join("snap1.state");
            let s1: MemoryCellSnapshot = boot_fc.request_json(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": snap1,
                    "memory_cell": {
                        "path_on_host": "0/0",
                        "cladefsd_socket": socket,
                        "ephemeral": true,
                    },
                }),
            )?;
            ensure!(
                s1.ephemeral && s1.version == 0 && s1.cell != 0,
                "unexpected ephemeral snapshot 1: {s1:?}"
            );
            let eph_mem_id = cladefs::CellId { tenant: s1.tenant, id: s1.cell };
            let eph_mem_name = eph_mem_id.to_string();
            drop(boot_stream);
            boot_fc.kill()?;

            let root_stat = control.call(async |client| {
                client
                    .stat_cell(StatCellRequest { tenant: eph_root_id.tenant, cell: eph_root_id.id })
                    .await
            })?;
            let mem_stat = control.call(async |client| {
                client
                    .stat_cell(StatCellRequest { tenant: eph_mem_id.tenant, cell: eph_mem_id.id })
                    .await
            })?;
            ensure!(root_stat.ephemeral && !root_stat.uploading, "root_stat={root_stat:?}");
            ensure!(mem_stat.ephemeral && !mem_stat.uploading, "mem_stat={mem_stat:?}");
            ensure!(
                mem_stat.used < 256 << 20,
                "ephemeral memory cell stored {} bytes of {RAM_MIB} MiB VM",
                mem_stat.used
            );
            ensure!(
                uploaded_bytes(&control)? == baseline_uploads,
                "ephemeral suspend uploaded bytes to bucket"
            );

            // Evict daemon cache so resume faults guest RAM and Fxfs extents back from local SSD.
            control.call(async |client| {
                client.shrink_memory(ShrinkMemoryRequest { tenant: None, target_bytes: 0 }).await
            })?;

            // 4. Resume VM #2 in place from snapshot #1, continue writing, then re-suspend.
            let uds_r1 = uds("r1");
            let listen_r1 = listen(&uds_r1)?;
            let mut r1_fc = Firecracker::spawn(dir, "r1", &Vm::api_socket("r1"), None, &[])?;
            r1_fc.request(
                "PUT",
                "/snapshot/load",
                &serde_json::json!({
                    "snapshot_path": snap1,
                    "mem_backend": {
                        "backend_type": "Cell",
                        "backend_path": eph_mem_name,
                    },
                    "memory_cell": {
                        "tenant": s1.tenant,
                        "cell": s1.cell,
                        "cladefsd_socket": socket,
                        "ephemeral": true,
                    },
                    "resume_vm": true,
                    "vsock_override": { "uds_path": uds_r1 },
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": eph_root_name,
                        "cladefsd_socket": socket,
                    }],
                }),
            )?;
            let mut stream =
                r1_fc.accept_within(&listen_r1, Some(std::time::Duration::from_secs(120)))?;
            writeln!(stream, "resumed1")?;
            line.clear();
            BufReader::new(&mut stream).read_line(&mut line)?;
            ensure!(line.trim() == "frozen-resumed1", "resumed1 guest said {line:?}");

            // 5. Re-suspend VM #2 in place (`memory_cell: {}`).
            r1_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            let snap2 = dir.join("snap2.state");
            let s2: MemoryCellSnapshot = r1_fc.request_json(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": snap2,
                    "memory_cell": {},
                }),
            )?;
            ensure!(
                s2.ephemeral && s2.version == 0 && s2.cell == s1.cell && s2.tenant == s1.tenant,
                "unexpected ephemeral snapshot 2: {s2:?} (s1={s1:?})"
            );
            drop(stream);
            r1_fc.kill()?;
            ensure!(
                uploaded_bytes(&control)? == baseline_uploads,
                "ephemeral re-suspend uploaded bytes to bucket"
            );

            // 6. Stale snapshot #1 must be refused because eph_root's committed epoch advanced.
            let stale_fc = Firecracker::spawn(dir, "stale", &Vm::api_socket("stale"), None, &[])?;
            let stale_load = stale_fc.request(
                "PUT",
                "/snapshot/load",
                &serde_json::json!({
                    "snapshot_path": snap1,
                    "mem_backend": {
                        "backend_type": "Cell",
                        "backend_path": eph_mem_name,
                    },
                    "memory_cell": {
                        "tenant": s1.tenant,
                        "cell": s1.cell,
                        "cladefsd_socket": socket,
                        "ephemeral": true,
                    },
                    "resume_vm": true,
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": eph_root_name,
                        "cladefsd_socket": socket,
                    }],
                }),
            );
            stale_fc.kill()?;
            ensure!(
                stale_load.is_err(),
                "stale ephemeral snapshot #1 should have been refused with EEXIST"
            );

            // 7. Resume VM #3 from snapshot #2 and verify all Fxfs files and RAM pages.
            let uds_r2 = uds("r2");
            let listen_r2 = listen(&uds_r2)?;
            let r2_fc = Firecracker::spawn(dir, "r2", &Vm::api_socket("r2"), None, &[])?;
            std::thread::spawn(move || -> std::io::Result<()> {
                for stream in listen_r2.incoming() {
                    writeln!(stream?, "resumed2")?;
                }
                Ok(())
            });
            r2_fc.request(
                "PUT",
                "/snapshot/load",
                &serde_json::json!({
                    "snapshot_path": snap2,
                    "mem_backend": {
                        "backend_type": "Cell",
                        "backend_path": eph_mem_name,
                    },
                    "memory_cell": {
                        "cladefsd_socket": socket,
                        "ephemeral": true,
                    },
                    "resume_vm": true,
                    "vsock_override": { "uds_path": uds_r2 },
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": eph_root_name,
                        "cladefsd_socket": socket,
                    }],
                }),
            )?;
            let (status, console) = r2_fc.wait()?;
            crate::vm::split_console(std::fs::File::open(&console)?)?;
            ensure!(status.success(), "resumed2 exited {status}");

            // 8. Delete both ephemeral cells and confirm zero bucket uploads throughout.
            control.call(async |client| {
                client
                    .delete_cell(DeleteCellRequest {
                        tenant: eph_root_id.tenant,
                        cell: eph_root_id.id,
                    })
                    .await
            })?;
            control.call(async |client| {
                client
                    .delete_cell(DeleteCellRequest {
                        tenant: eph_mem_id.tenant,
                        cell: eph_mem_id.id,
                    })
                    .await
            })?;
            ensure!(
                control
                    .call(async |client| {
                        client
                            .stat_cell(StatCellRequest {
                                tenant: eph_root_id.tenant,
                                cell: eph_root_id.id,
                            })
                            .await
                    })
                    .is_err(),
                "ephemeral rootfs cell still present after delete"
            );
            ensure!(
                control
                    .call(async |client| {
                        client
                            .stat_cell(StatCellRequest {
                                tenant: eph_mem_id.tenant,
                                cell: eph_mem_id.id,
                            })
                            .await
                    })
                    .is_err(),
                "ephemeral memory cell still present after delete"
            );
            ensure!(
                uploaded_bytes(&control)? == baseline_uploads,
                "ephemeral lifecycle uploaded bytes to bucket"
            );

            // Clean up our dedicated daemon and socket directory.
            let _ = daemon.kill();
            let _ = daemon.wait();
            let _ = std::fs::remove_dir_all(&sock_dir);
            Ok(())
        }
    }

    impl Test for TestFxfsGuestEphemeralSuspend {
        fn should_run(&self) -> ShouldRun {
            if std::env::var_os("CLADEFSD_BIN").is_none() {
                return ShouldRun::No("CLADEFSD_BIN does not name the extent daemon");
            }
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
            if !output.contains("OK ephemeral-suspend") {
                return TestOutcome::Fail(format!("missing OK ephemeral-suspend\n{output}"));
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

    impl Test for TestFxfsGuestEphemeralSuspend {
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
                // Wait for suspend & restore vsock reset.
                let _ = BufReader::new(host).read_line(&mut wait);
            }

            // Resumed #1: verify boot state, write more to Fxfs and RAM, re-suspend.
            let (role, mut host) = role_from_host();
            assert_eq!(role, "resumed1");
            freeze(FITHAW);
            assert_eq!(fs::read(format!("{MNT}/stage")).unwrap(), b"boot");
            for i in 0..BUF_PAGES {
                assert!(
                    check_page(&buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_BOOT, i),
                    "buf_a page {i} corrupted after first resume"
                );
            }

            fs::write(format!("{MNT}/stage"), b"resumed1").unwrap();
            fs::write(format!("{MNT}/extra"), b"after-resume").unwrap();
            for i in 0..64 {
                fill_page(&mut buf_a[i * PAGE..(i + 1) * PAGE], MAGIC_R1_MUT, i);
            }
            let buf_b = alloc_buf();
            for i in 0..BUF_PAGES {
                fill_page(&mut buf_b[i * PAGE..(i + 1) * PAGE], MAGIC_R1_NEW, i);
            }

            freeze(FIFREEZE);
            {
                writeln!(host, "frozen-resumed1").unwrap();
                let mut wait = String::new();
                // Wait for re-suspend & second restore vsock reset.
                let _ = BufReader::new(host).read_line(&mut wait);
            }

            // Resumed #2: verify all Fxfs writes and RAM pages across both buffers.
            let (role2, _host2) = role_from_host();
            assert_eq!(role2, "resumed2");
            freeze(FITHAW);
            assert_eq!(fs::read(format!("{MNT}/stage")).unwrap(), b"resumed1");
            assert_eq!(fs::read(format!("{MNT}/extra")).unwrap(), b"after-resume");
            for i in 0..BUF_PAGES {
                let expected_a = if i < 64 { MAGIC_R1_MUT } else { MAGIC_BOOT };
                assert!(
                    check_page(&buf_a[i * PAGE..(i + 1) * PAGE], expected_a, i),
                    "buf_a page {i} mismatch after second resume"
                );
                assert!(
                    check_page(&buf_b[i * PAGE..(i + 1) * PAGE], MAGIC_R1_NEW, i),
                    "buf_b page {i} mismatch after second resume"
                );
            }
            println!("OK ephemeral-suspend");
        }
    }
}
