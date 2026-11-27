// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Repeated park/suspend and resume cycles with memory cells, active bidirectional AF_VSOCK
//! streams, and active concurrent guest filesystem workloads across `FIFREEZE`/`FITHAW` (boards
//! 12110 and 12113):
//!
//! 1. Cold-booted DAX VM runs concurrent `mmap(MAP_SHARED, PROT_WRITE)`, `write(2)`, and
//!    `unlink(2)` workloads alongside:
//!    - host->guest `guestd` RPC control stream and a high-throughput host->guest log stream
//!      actively pumping bytes right up to `PATCH /vm { state: "Paused" }`, connected via
//!      fragmented `CONNECT <port>\n` writes; and
//!    - a persistent guest->host `git-auth` stream exchanging heartbeats and reconnecting on reset.
//! 2. Guest runs `FIFREEZE` under active load (verifying 0 private & 0 dirty DAX pages), host
//!    pauses VM while streams are active, issues repeated `PUT /snapshot/create` calls while
//!    paused (two `Diff` + one `Full` into a memory cell), verifies host UDS streams see EOF
//!    immediately after snapshot creation before killing the VM.
//! 3. Cycle 2 restores from memory cell `v1` with `vsock_override`, immediately reconnects both
//!    host->guest (fragmented `CONNECT`) and guest->host streams with zero hangs, thaws, verifies
//!    state, runs another active workload + `FIFREEZE`, takes two snapshots while paused (into
//!    forked memory cell `v2`), verifies immediate EOF, and kills the VM.
//! 4. Cycle 3 restores from memory cell `v2` reusing the same UDS path after unlinking the stale
//!    socket (no `vsock_override`), immediately reconnects both directions, thaws, verifies all
//!    generations across a remount, and exits cleanly.

use macros::{guest, host};

pub struct TestFxfsGuestParkResume;

#[cfg_attr(feature = "guest", allow(dead_code))]
const CAPACITY: u64 = 128 << 20;
#[cfg_attr(feature = "guest", allow(dead_code))]
const RAM_MIB: u32 = 512;

/// Host -> guest `guestd` RPC & log-stream port.
const GUESTD_PORT: u32 = 6050;
/// Guest -> host `git-auth` persistent stream port.
const GIT_AUTH_PORT: u32 = 6051;

#[host]
mod host {
    use super::*;
    use crate::common::{cladefs_guest_should_run, setup_rootfs};
    use crate::vm::{Firecracker, Vm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::{Context, ensure};
    use cladefs::cladefsd::control::proto::{ListCellsRequest, SyncCellRequest};
    use std::collections::BTreeSet;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    #[derive(Debug, serde::Deserialize)]
    struct MemoryCellSnapshot {
        tenant: u64,
        cell: u64,
        version: u64,
    }

    fn uds(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cladefs-park-{}-{name}.vsock", std::process::id()))
    }

    /// Connects host -> guest on `uds` to `port` using fragmented `CONNECT <port>\n` writes
    /// across three syscalls (exercising non-blocking `LocalStream` buffering, board 12110),
    /// retrying until the guest is listening and has acked any `TRANSPORT_RESET`.
    fn connect_guest_fragmented(
        uds: &Path,
        port: u32,
        timeout: Duration,
    ) -> anyhow::Result<UnixStream> {
        let deadline = Instant::now() + timeout;
        let mut last_err = None;
        while Instant::now() < deadline {
            match UnixStream::connect(uds) {
                Ok(mut stream) => {
                    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                    // Fragment the CONNECT command across three writes with tiny sleeps to
                    // force separate epoll wakeups in Firecracker's vsock muxer.
                    let port_str = port.to_string();
                    let write_res = stream
                        .write_all(b"CONNECT ")
                        .and_then(|()| {
                            std::thread::sleep(Duration::from_millis(2));
                            stream.write_all(port_str.as_bytes())
                        })
                        .and_then(|()| {
                            std::thread::sleep(Duration::from_millis(2));
                            stream.write_all(b"\n")
                        });
                    if let Err(err) = write_res {
                        last_err = Some(err.to_string());
                        std::thread::sleep(Duration::from_millis(25));
                        continue;
                    }
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone()?);
                    match reader.read_line(&mut line) {
                        Ok(n) if n > 0 && line.starts_with("OK ") => return Ok(stream),
                        Ok(n) => {
                            last_err = Some(format!("unexpected handshake ({n}B): {line:?}"));
                        }
                        Err(err) => {
                            last_err = Some(format!("handshake read: {err}"));
                        }
                    }
                }
                Err(err) => {
                    last_err = Some(format!("connect {}: {err}", uds.display()));
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        anyhow::bail!(
            "timed out connecting to guest port {port} via {} ({last_err:?})",
            uds.display()
        )
    }

    /// Sends one RPC line on `stream` and reads one response line.
    fn rpc(stream: &mut UnixStream, cmd: &str) -> anyhow::Result<String> {
        stream
            .write_all(format!("{cmd}\n").as_bytes())
            .with_context(|| format!("sending {cmd:?}"))?;
        let mut line = String::new();
        let n = BufReader::new(stream.try_clone()?)
            .read_line(&mut line)
            .with_context(|| format!("reading reply to {cmd:?}"))?;
        ensure!(n > 0, "unexpected EOF waiting for response to {cmd:?}");
        Ok(line.trim_end().to_owned())
    }

    /// Returns true only when `res` proves the peer closed/reset the connection (EOF or a
    /// connection-termination error), explicitly rejecting `WouldBlock`/`TimedOut` on live sockets.
    fn is_peer_closed(res: &std::io::Result<usize>) -> bool {
        matches!(res, Ok(0))
            || matches!(
                res,
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                    )
            )
    }

    /// Starts a background host->guest high-throughput stream to `GUESTD_PORT` simulating log or
    /// port-forward traffic right up to VM pause. Returns the stop flag, join handle, and a
    /// flag set once the host side observes EOF/reset (never on timeout/backpressure).
    fn start_host_stream(
        uds: &Path,
    ) -> anyhow::Result<(Arc<AtomicBool>, Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
        let mut stream = connect_guest_fragmented(uds, GUESTD_PORT, Duration::from_secs(30))?;
        let reply = rpc(&mut stream, "STREAM")?;
        ensure!(reply == "STREAM-OK", "STREAM handshake got {reply:?}");
        stream.set_write_timeout(Some(Duration::from_millis(200)))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;

        let stop = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = stop.clone();
            let closed = closed.clone();
            std::thread::spawn(move || {
                let chunk = vec![0x42u8; 4096];
                while !stop.load(Ordering::Relaxed) {
                    match stream.write(&chunk) {
                        Ok(0) => {
                            closed.store(true, Ordering::Release);
                            return;
                        }
                        Ok(_) => {}
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) =>
                        {
                            closed.store(true, Ordering::Release);
                            return;
                        }
                        Err(_) => return,
                    }
                }
                // After pause+snapshot, reading from the stream must promptly hit EOF/reset,
                // NOT a read timeout on a still-open socket.
                let mut buf = [0u8; 16];
                if is_peer_closed(&stream.read(&mut buf)) {
                    closed.store(true, Ordering::Release);
                }
            })
        };
        Ok((stop, closed, handle))
    }

    /// Starts a host-side listener for guest->host `GIT_AUTH_PORT` connections on `{uds}_6051`.
    /// Replies `OK <seq>\n` to every `AUTH <seq>\n` and increments `handled`.
    struct GitAuthServer {
        stop: Arc<AtomicBool>,
        handled: Arc<AtomicU64>,
        path: String,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl GitAuthServer {
        fn start(uds: &Path) -> anyhow::Result<Self> {
            let path = format!("{}_{GIT_AUTH_PORT}", uds.display());
            // Remove stale socket if reusing path.
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).with_context(|| format!("binding {path}"))?;
            listener.set_nonblocking(true)?;
            let stop = Arc::new(AtomicBool::new(false));
            let handled = Arc::new(AtomicU64::new(0));
            let handle = {
                let stop = stop.clone();
                let handled = handled.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let stop = stop.clone();
                                let handled = handled.clone();
                                std::thread::spawn(move || {
                                    let _ =
                                        stream.set_read_timeout(Some(Duration::from_millis(250)));
                                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                                    let mut writer = match stream.try_clone() {
                                        Ok(w) => w,
                                        Err(_) => return,
                                    };
                                    let mut reader = BufReader::new(stream);
                                    let mut line = String::new();
                                    while !stop.load(Ordering::Relaxed) {
                                        line.clear();
                                        match reader.read_line(&mut line) {
                                            Ok(0) => break,
                                            Ok(_) => {
                                                if let Some(seq) = line.trim().strip_prefix("AUTH ")
                                                {
                                                    if writeln!(writer, "OK {seq}").is_err() {
                                                        break;
                                                    }
                                                    handled.fetch_add(1, Ordering::Release);
                                                }
                                            }
                                            Err(err)
                                                if matches!(
                                                    err.kind(),
                                                    std::io::ErrorKind::WouldBlock
                                                        | std::io::ErrorKind::TimedOut
                                                ) =>
                                            {
                                                continue;
                                            }
                                            Err(_) => break,
                                        }
                                    }
                                });
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Ok(Self { stop, handled, path, handle: Some(handle) })
        }

        fn wait_for_requests(&self, min_count: u64, timeout: Duration) -> anyhow::Result<()> {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if self.handled.load(Ordering::Acquire) >= min_count {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            anyhow::bail!(
                "timed out waiting for {min_count} guest->host git-auth requests on {} (got {})",
                self.path,
                self.handled.load(Ordering::Acquire)
            )
        }
    }

    impl Drop for GitAuthServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Verifies that a host-side vsock UDS stream sees EOF/reset promptly (proving Firecracker's
    /// `reset_connections()` closed the host FD immediately on snapshot, even before VM kill).
    /// Explicitly rejects `WouldBlock`/`TimedOut` from a still-open stalled socket.
    fn assert_stream_closed_promptly(mut stream: UnixStream, label: &str) -> anyhow::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut buf = [0u8; 16];
        let res = stream.read(&mut buf);
        if is_peer_closed(&res) {
            return Ok(());
        }
        anyhow::bail!("{label}: expected peer EOF/reset after snapshot, got {res:?}")
    }

    impl TestFxfsGuestParkResume {
        fn run(&self, test_setup: &TestSetup) -> anyhow::Result<()> {
            let root = setup_rootfs(test_setup)?;
            std::fs::create_dir_all(root.join("mnt/fxfs"))?;
            let dir = &test_setup.tmp_dir;
            let cladefsd: PathBuf =
                std::env::var_os("CLADEFSD").context("CLADEFSD not set")?.into();
            let control = crate::common::control(&cladefsd)?;

            // --- Cycle 1: Cold boot -> active bidirectional vsock + FS workload -> FIFREEZE ->
            //     repeated snapshots while paused (into memory cell v1) -> kill ---
            let uds_boot = uds("boot");
            let _ = std::fs::remove_file(&uds_boot);
            let auth_boot = GitAuthServer::start(&uds_boot)?;

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
            let boot_fc =
                vm.spawn_api(&root, &test_setup.test_case, "boot", &Vm::api_socket("boot"))?;

            let mut rpc_boot =
                connect_guest_fragmented(&uds_boot, GUESTD_PORT, Duration::from_secs(90))?;
            ensure!(rpc(&mut rpc_boot, "PING boot")? == "PONG boot");
            let (stream_stop, stream_closed, stream_handle) = start_host_stream(&uds_boot)?;
            auth_boot.wait_for_requests(2, Duration::from_secs(30))?;

            // Let concurrent mmap + write + unlink + vsock traffic run briefly, then freeze.
            std::thread::sleep(Duration::from_millis(100));
            ensure!(rpc(&mut rpc_boot, "FREEZE")? == "FROZEN");

            // Pause while active host->guest stream and guest->host git-auth stream are open.
            boot_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;

            // Take 3 snapshots while paused: 2 Full file snapshots + 1 Full memory_cell,
            // verifying repeated prepare_save() while paused does not exhaust EVQ.
            for idx in 0..2 {
                boot_fc.request(
                    "PUT",
                    "/snapshot/create",
                    &serde_json::json!({
                        "snapshot_type": "Full",
                        "snapshot_path": dir.join(format!("boot-full-{idx}.state")),
                        "mem_file_path": dir.join(format!("boot-full-{idx}.mem")),
                    }),
                )?;
            }
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

            // Host UDS connections must already be closed by Firecracker's reset_connections()
            // BEFORE killing the VM process (board 12110).
            assert_stream_closed_promptly(rpc_boot, "boot rpc")?;
            stream_stop.store(true, Ordering::Relaxed);
            stream_handle.join().expect("boot stream thread");
            ensure!(
                stream_closed.load(Ordering::Acquire),
                "boot host->guest stream did not close on snapshot"
            );

            drop(auth_boot);
            boot_fc.kill()?;
            let _ = std::fs::remove_file(&uds_boot);

            control.call(async |client| {
                client.sync_cell(SyncCellRequest { tenant: v1.tenant, cell: v1.cell }).await
            })?;

            // --- Cycle 2: Restore #1 from memory cell v1 with vsock_override -> reconnect ->
            //     FITHAW -> active workload + streams -> FIFREEZE -> 2 snapshots (v2) -> kill ---
            let cells_before_r1: BTreeSet<(u64, u64)> = control
                .call(async |client| client.list_cells(ListCellsRequest {}).await)?
                .stores
                .into_iter()
                .map(|c| (c.tenant, c.cell))
                .collect();
            let uds_r1 = uds("r1");
            let _ = std::fs::remove_file(&uds_r1);
            let auth_r1 = GitAuthServer::start(&uds_r1)?;

            let r1_fc = Firecracker::spawn(dir, "r1", &Vm::api_socket("r1"), None, &[])?;
            r1_fc.request(
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
                    "vsock_override": { "uds_path": uds_r1 },
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": "0/0",
                    }],
                }),
            )?;

            let mut rpc_r1 =
                connect_guest_fragmented(&uds_r1, GUESTD_PORT, Duration::from_secs(30))?;
            ensure!(rpc(&mut rpc_r1, "PING r1")? == "PONG r1");
            let (r1_stream_stop, r1_stream_closed, r1_stream_handle) = start_host_stream(&uds_r1)?;
            auth_r1.wait_for_requests(2, Duration::from_secs(30))?;

            ensure!(rpc(&mut rpc_r1, "THAW 1")? == "THAWED 1");
            std::thread::sleep(Duration::from_millis(200));
            ensure!(rpc(&mut rpc_r1, "FREEZE")? == "FROZEN");

            r1_fc.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
            r1_fc.request(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Diff",
                    "snapshot_path": dir.join("r1-diff.state"),
                    "mem_file_path": dir.join("r1-diff.mem"),
                }),
            )?;
            let snap2 = dir.join("snap2.state");
            let v2: MemoryCellSnapshot = r1_fc.request_json(
                "PUT",
                "/snapshot/create",
                &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": snap2,
                    "memory_cell": {},
                }),
            )?;
            ensure!(v2.cell != v1.cell && v2.version != 0, "unexpected v2: {v2:?}");

            assert_stream_closed_promptly(rpc_r1, "r1 rpc")?;
            r1_stream_stop.store(true, Ordering::Relaxed);
            r1_stream_handle.join().expect("r1 stream thread");
            ensure!(
                r1_stream_closed.load(Ordering::Acquire),
                "r1 host->guest stream did not close on snapshot"
            );

            drop(auth_r1);
            r1_fc.kill()?;
            // Unlink the stale UDS socket so Cycle 3 can reuse the exact same saved uds_path
            // without vsock_override.
            let _ = std::fs::remove_file(&uds_r1);

            control.call(async |client| {
                client.sync_cell(SyncCellRequest { tenant: v2.tenant, cell: v2.cell }).await
            })?;
            let cells_after_r1 =
                control.call(async |client| client.list_cells(ListCellsRequest {}).await)?.stores;
            for c in cells_after_r1 {
                if !cells_before_r1.contains(&(c.tenant, c.cell))
                    && (c.tenant, c.cell) != (v2.tenant, v2.cell)
                {
                    control.call(async |client| {
                        client.sync_cell(SyncCellRequest { tenant: c.tenant, cell: c.cell }).await
                    })?;
                }
            }

            // --- Cycle 3: Restore #2 from memory cell v2 reusing uds_r1 (no vsock_override) ->
            //     immediate reconnect both ways -> FITHAW -> verify -> DONE ---
            let auth_r2 = GitAuthServer::start(&uds_r1)?;
            let r2_fc = Firecracker::spawn(dir, "r2", &Vm::api_socket("r2"), None, &[])?;
            r2_fc.request(
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
                    "cladefs_overrides": [{
                        "id": "extents",
                        "path_on_host": "0/0",
                    }],
                }),
            )?;

            let mut rpc_r2 =
                connect_guest_fragmented(&uds_r1, GUESTD_PORT, Duration::from_secs(30))?;
            ensure!(rpc(&mut rpc_r2, "PING r2")? == "PONG r2");
            auth_r2.wait_for_requests(2, Duration::from_secs(30))?;

            ensure!(rpc(&mut rpc_r2, "THAW 2")? == "THAWED 2");
            std::thread::sleep(Duration::from_millis(150));
            ensure!(rpc(&mut rpc_r2, "DONE")? == "BYE");
            drop(rpc_r2);

            let (status, console) = r2_fc.wait()?;
            drop(auth_r2);
            let _ = std::fs::remove_file(&uds_r1);
            crate::vm::split_console(std::fs::File::open(&console)?)?;
            ensure!(status.success(), "r2 exited {status}");
            Ok(())
        }
    }

    impl Test for TestFxfsGuestParkResume {
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
            if !output.contains("OK park-resume") {
                return TestOutcome::Fail(format!("missing OK park-resume\n{output}"));
            }
            TestOutcome::Pass
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use nix::libc;
    use nix::sys::socket::{
        AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept, bind, connect, listen,
        socket,
    };
    use std::ffi::CString;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    const MNT: &str = "/mnt/fxfs";
    const FIFREEZE: libc::c_ulong = 0xc004_5877;
    const FITHAW: libc::c_ulong = 0xc004_5878;
    const MMAP_PAGES: usize = 64;
    const PAGE: usize = 4096;
    const MMAP_LEN: usize = MMAP_PAGES * PAGE;

    fn mount() {
        let (source, target, fstype) = (
            CString::new("extents").unwrap(),
            CString::new(MNT).unwrap(),
            CString::new("cladefs").unwrap(),
        );
        // SAFETY: valid C strings for mount(2).
        let ret = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
        };
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    fn unmount() {
        let target = CString::new(MNT).unwrap();
        // SAFETY: valid C string for umount(2).
        assert_eq!(unsafe { libc::umount(target.as_ptr()) }, 0, "umount");
    }

    fn freeze(request: libc::c_ulong) {
        let dir = fs::File::open(MNT).unwrap();
        // SAFETY: FIFREEZE / FITHAW ioctl on mounted directory fd.
        let ret = unsafe { libc::ioctl(dir.as_raw_fd(), request as _, 0) };
        assert_eq!(ret, 0, "ioctl {request:#x}: {}", std::io::Error::last_os_error());
    }

    fn stat(name: &str) -> Option<u64> {
        let text = fs::read_to_string("/proc/fs/cladefs/extents").unwrap();
        text.lines().find_map(|line| {
            let (k, v) = line.split_once(' ')?;
            (k == name).then(|| v.parse().ok()).flatten()
        })
    }

    fn vsock_connect(port: u32) -> std::io::Result<fs::File> {
        let fd = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)?;
        connect(fd.as_raw_fd(), &VsockAddr::new(2, port))?;
        // SAFETY: `fd` is an open socket owned by the returned File.
        Ok(unsafe { fs::File::from_raw_fd(fd.into_raw_fd()) })
    }

    fn vsock_listen(port: u32) -> std::os::fd::OwnedFd {
        let fd = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::SOCK_NONBLOCK, None)
            .expect("vsock listen socket");
        bind(fd.as_raw_fd(), &VsockAddr::new(libc::VMADDR_CID_ANY, port)).expect("vsock bind");
        listen(&fd, Backlog::new(16).unwrap()).expect("vsock listen");
        fd
    }

    impl Test for TestFxfsGuestParkResume {
        fn in_guest(self: Box<Self>) {
            mount();

            // Create a sealed file and remount so it is opened as S_DAX.
            let mmap_path = format!("{MNT}/mmap.bin");
            fs::write(&mmap_path, vec![0xaa; MMAP_LEN]).unwrap();
            fs::write(format!("{MNT}/gen-0"), b"generation 0").unwrap();
            unmount();
            mount();

            // Background writer 1: active mmap(MAP_SHARED, PROT_WRITE) stores across DAX pages in
            // a forked child process (separate `mm_struct`, mirroring real workloads beside
            // `hotbed-guestd`, so `sb_start_pagefault()` holding `mm->mmap_lock` during `FIFREEZE`
            // cannot deadlock `guestd`'s own allocations before `FITHAW`).
            #[repr(C)]
            struct MmapCtrl {
                stop: AtomicBool,
                round: AtomicU64,
            }
            // SAFETY: valid 1-page anonymous shared mapping for parent/child coordination.
            let ctrl_ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    PAGE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(
                ctrl_ptr,
                libc::MAP_FAILED,
                "mmap ctrl: {}",
                std::io::Error::last_os_error()
            );
            let ctrl = ctrl_ptr.cast::<MmapCtrl>();
            // SAFETY: `ctrl_ptr` is zero-initialized and page-aligned.
            unsafe {
                (*ctrl).stop.store(false, Ordering::Release);
                (*ctrl).round.store(0, Ordering::Release);
            }
            let mmap_cstr = CString::new(mmap_path.as_str()).unwrap();
            // SAFETY: single-threaded before spawning worker threads; child uses raw syscalls + `_exit`.
            let mmap_pid = unsafe { libc::fork() };
            assert!(mmap_pid >= 0, "fork: {}", std::io::Error::last_os_error());
            if mmap_pid == 0 {
                // SAFETY: raw syscalls in freshly forked child; exits with `_exit`.
                unsafe {
                    let fd = libc::open(mmap_cstr.as_ptr(), libc::O_RDWR);
                    if fd < 0 {
                        libc::_exit(2);
                    }
                    let ptr = libc::mmap(
                        std::ptr::null_mut(),
                        MMAP_LEN,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd,
                        0,
                    );
                    if ptr == libc::MAP_FAILED {
                        libc::_exit(3);
                    }
                    let mut seq = 1u64;
                    while !(*ctrl).stop.load(Ordering::Acquire) {
                        for p in 0..MMAP_PAGES {
                            let dst = (ptr as usize + p * PAGE) as *mut u64;
                            std::ptr::write_volatile(dst, seq);
                        }
                        (*ctrl).round.store(seq, Ordering::Release);
                        seq += 1;
                    }
                    libc::munmap(ptr, MMAP_LEN);
                    libc::close(fd);
                    libc::_exit(0);
                }
            }

            let stop = Arc::new(AtomicBool::new(false));

            // Background writer 2: continuous write(2) + unlink(2) exercising delayed unlinks.
            let fs_worker = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut i = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        let p = format!("{MNT}/churn-{}", i % 16);
                        fs::write(&p, format!("churn payload {i}")).unwrap();
                        let _ = fs::remove_file(&p);
                        i += 1;
                    }
                })
            };

            // Background guest->host persistent `git-auth` stream with automatic reconnect on reset.
            let auth_worker = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut seq = 1u64;
                    while !stop.load(Ordering::Relaxed) {
                        let Ok(mut sock) = vsock_connect(GIT_AUTH_PORT) else {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        };
                        let mut reader = BufReader::new(sock.try_clone().unwrap());
                        while !stop.load(Ordering::Relaxed) {
                            if writeln!(sock, "AUTH {seq}").is_err() {
                                break;
                            }
                            let mut line = String::new();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    assert_eq!(
                                        line.trim(),
                                        format!("OK {seq}"),
                                        "unexpected git-auth reply"
                                    );
                                    seq += 1;
                                }
                            }
                            std::thread::sleep(Duration::from_millis(15));
                        }
                    }
                })
            };

            // Host -> guest `guestd` RPC + stream listener: dispatch every accepted connection on
            // its own thread so long-lived RPC and STREAM connections run concurrently.
            let listener = vsock_listen(GUESTD_PORT);
            let done = Arc::new(AtomicBool::new(false));
            let freeze_lock = Arc::new(std::sync::Mutex::new(()));
            let mut conn_threads = Vec::new();

            while !done.load(Ordering::Acquire) {
                let client_fd = match accept(listener.as_raw_fd()) {
                    Ok(fd) => fd,
                    Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(err) => panic!("vsock accept: {err}"),
                };
                let stop = stop.clone();
                let done = done.clone();
                let freeze_lock = freeze_lock.clone();
                let handle = std::thread::spawn(move || {
                    // SAFETY: `client_fd` is a fresh accepted socket fd.
                    let mut client = unsafe { fs::File::from_raw_fd(client_fd) };
                    let mut reader = BufReader::new(client.try_clone().unwrap());
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let cmd = line.trim();
                                if let Some(tag) = cmd.strip_prefix("PING ") {
                                    if writeln!(client, "PONG {tag}").is_err() {
                                        break;
                                    }
                                } else if cmd == "STREAM" {
                                    if writeln!(client, "STREAM-OK").is_err() {
                                        break;
                                    }
                                    let mut buf = [0u8; 4096];
                                    loop {
                                        match reader.read(&mut buf) {
                                            Ok(0) | Err(_) => break,
                                            Ok(_) => {}
                                        }
                                    }
                                    break;
                                } else if cmd == "FREEZE" {
                                    let _guard = freeze_lock.lock().unwrap();
                                    freeze(FIFREEZE);
                                    let private = stat("dax_private_pages").unwrap_or(0);
                                    let dirty = stat("dax_dirty_pages").unwrap_or(0);
                                    assert_eq!(
                                        (private, dirty),
                                        (0, 0),
                                        "frozen with private={private}, dirty={dirty} pages"
                                    );
                                    if writeln!(client, "FROZEN").is_err() {
                                        break;
                                    }
                                } else if let Some(gen_str) = cmd.strip_prefix("THAW ") {
                                    let _guard = freeze_lock.lock().unwrap();
                                    let gen_num: u64 = gen_str.parse().unwrap();
                                    freeze(FITHAW);
                                    for g in 0..gen_num {
                                        assert_eq!(
                                            fs::read(format!("{MNT}/gen-{g}")).unwrap(),
                                            format!("generation {g}").as_bytes(),
                                            "gen-{g} corrupted after restore"
                                        );
                                    }
                                    fs::write(
                                        format!("{MNT}/gen-{gen_num}"),
                                        format!("generation {gen_num}"),
                                    )
                                    .unwrap();
                                    if writeln!(client, "THAWED {gen_num}").is_err() {
                                        break;
                                    }
                                } else if cmd == "DONE" {
                                    stop.store(true, Ordering::Relaxed);
                                    done.store(true, Ordering::Release);
                                    let _ = writeln!(client, "BYE");
                                    break;
                                } else {
                                    panic!("unknown guestd command: {cmd:?}");
                                }
                            }
                        }
                    }
                });
                conn_threads.push(handle);
            }

            // Stop the forked mmap child and wait for it to exit cleanly before reading `mmap.bin`.
            // SAFETY: `ctrl` points to our shared `MmapCtrl` page and `mmap_pid` is our child.
            let final_round = unsafe {
                (*ctrl).stop.store(true, Ordering::Release);
                let mut status = 0;
                let waited = libc::waitpid(mmap_pid, &mut status, 0);
                assert_eq!(waited, mmap_pid, "waitpid: {}", std::io::Error::last_os_error());
                assert!(
                    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                    "mmap child exited abnormally: status={status:#x}"
                );
                let round = (*ctrl).round.load(Ordering::Acquire);
                assert_eq!(libc::munmap(ctrl_ptr, PAGE), 0, "munmap ctrl");
                round
            };
            fs_worker.join().unwrap();
            auth_worker.join().unwrap();
            for handle in conn_threads {
                handle.join().unwrap();
            }

            // Record the final mmap contents before unmounting, then remount and verify disk.
            assert!(final_round > 0, "mmap worker never completed a round");
            let expected_mmap = fs::read(&mmap_path).unwrap();
            assert_eq!(expected_mmap.len(), MMAP_LEN, "mmap.bin length mismatch");

            unmount();
            mount();
            for g in 0..=2u64 {
                assert_eq!(
                    fs::read(format!("{MNT}/gen-{g}")).unwrap(),
                    format!("generation {g}").as_bytes(),
                    "gen-{g} corrupted after remount"
                );
            }
            assert_eq!(
                fs::read(&mmap_path).unwrap(),
                expected_mmap,
                "mmap.bin mismatch after remount"
            );
            println!("OK park-resume");
        }
    }
}
