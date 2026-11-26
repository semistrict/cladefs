// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What machined's end-to-end tests share: whether this host runs VMs, and the guest they boot.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Whether this host runs VMs (else a note): /dev/kvm, accessible, on Linux;
/// Hypervisor.framework (`kern.hv_support`) on macOS, which GitHub's macOS runners, themselves
/// VMs, lack (board 13847).
pub fn hypervisor() -> bool {
    if cfg!(target_os = "linux") {
        if std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_err() {
            eprintln!("skipping: /dev/kvm is not accessible");
            return false;
        }
        return true;
    }
    let support = std::process::Command::new("sysctl").args(["-n", "kern.hv_support"]).output();
    if !support.is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1") {
        eprintln!("skipping: this host has no Hypervisor.framework (kern.hv_support)");
        return false;
    }
    true
}

/// The guest's kernel and initramfs, if this host can boot them (else a note).
pub fn guest() -> Option<(PathBuf, PathBuf)> {
    if !hypervisor() {
        return None;
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (default_dir, default_kernel) = if cfg!(target_os = "linux") {
        (workspace.join("target/guest"), None)
    } else {
        let dir = workspace.join("target/arm64-guest");
        let kernel = dir.join("Image");
        (dir, Some(kernel))
    };
    let kernel = std::env::var_os("CLADEFS_GUEST_KERNEL").map(PathBuf::from).or(default_kernel);
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or(default_dir, PathBuf::from);
    let initramfs = dir.join("initramfs.cpio");
    match kernel {
        Some(kernel) if kernel.is_file() && initramfs.is_file() => Some((kernel, initramfs)),
        kernel => {
            eprintln!(
                "skipping: no guest kernel ({kernel:?}; CLADEFS_GUEST_KERNEL) or initramfs ({}): \
                 scripts/guest-kernel.sh and crates/libmachine/scripts/build-test-guest.sh, or \
                 on macOS crates/libmachine/scripts/arm64-guest.sh",
                initramfs.display()
            );
            None
        }
    }
}

/// The cladefs daemon a test's machined embeds: machined's default, of a small memory budget.
pub fn cladefs(state: &Path) -> machined::CladefsConfig {
    machined::Config::default_cladefs(state).unwrap().memory(512 << 20)
}

/// The cladefsd client of the machined serving at `socket`.
pub async fn cladefsd(
    socket: &Path,
) -> cladefsd::control::proto::cladefsd_client::CladefsdClient<tonic::transport::Channel> {
    let channel = machined::client::channel_unix(socket).await.unwrap();
    cladefsd::control::proto::cladefsd_client::CladefsdClient::new(channel)
}

/// A root for machines: a cell of the machined at `socket`'s daemon holding the test initramfs's
/// files (its init, libmachine's agent, busybox), imported as an image of one layer (a tar of
/// them, written to `scratch`).
pub async fn import_root(socket: &Path, initramfs: &Path, scratch: &Path) -> machined::proto::Cell {
    use cladefsd::control::proto::ImportImageRequest;
    let tar = scratch.join("root.tar");
    cpio_to_tar(&std::fs::read(initramfs).unwrap(), &tar);
    let request = ImportImageRequest {
        capacity: 1 << 30,
        image: tar.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let imported = cladefsd(socket).await.import_image(request).await.unwrap().into_inner();
    machined::proto::Cell { tenant: imported.tenant, cell: imported.cell }
}

/// A new cell forked from `base`: a machine's root of its own.
pub async fn fork(socket: &Path, base: &machined::proto::Cell) -> machined::proto::Cell {
    use cladefsd::control::proto::ForkCellRequest;
    let request = ForkCellRequest { tenant: base.tenant, cell: base.cell, ..Default::default() };
    let forked = cladefsd(socket).await.fork_cell(request).await.unwrap().into_inner();
    machined::proto::Cell { tenant: forked.tenant, cell: forked.cell }
}

/// Rewrites a cpio archive of the "newc" format (the kernel's initramfs) as a tar archive at
/// `tar`: its directories, files, symbolic links and device nodes, with their modes and owners.
pub fn cpio_to_tar(cpio: &[u8], tar: &Path) {
    let field = |header: &[u8], i: usize| {
        let hex = std::str::from_utf8(&header[6 + 8 * i..14 + 8 * i]).unwrap();
        u64::from_str_radix(hex, 16).unwrap()
    };
    let align = |n: usize| n.div_ceil(4) * 4;
    let mut out = tar::Builder::new(std::fs::File::create(tar).unwrap());
    let mut at = 0;
    loop {
        let header = &cpio[at..at + 110];
        assert_eq!(&header[..6], b"070701", "a newc cpio header at {at}");
        let (mode, uid, gid, mtime) =
            (field(header, 1), field(header, 2), field(header, 3), field(header, 5));
        let size = usize::try_from(field(header, 6)).unwrap();
        let (rmajor, rminor) = (field(header, 9), field(header, 10));
        let name_len = usize::try_from(field(header, 11)).unwrap();
        let name =
            std::str::from_utf8(&cpio[at + 110..at + 110 + name_len - 1]).unwrap().to_owned();
        let data_at = align(at + 110 + name_len);
        let data = &cpio[data_at..data_at + size];
        at = align(data_at + size);
        if name == "TRAILER!!!" {
            break;
        }
        let path = name.trim_start_matches("./").trim_start_matches('/');
        if path.is_empty() || path == "." {
            continue;
        }
        let mut entry = tar::Header::new_gnu();
        entry.set_mode(u32::try_from(mode & 0o7777).unwrap());
        entry.set_uid(uid);
        entry.set_gid(gid);
        entry.set_mtime(mtime);
        entry.set_size(0);
        match mode & 0o170000 {
            0o040000 => {
                entry.set_entry_type(tar::EntryType::Directory);
                out.append_data(&mut entry, path, std::io::empty()).unwrap();
            }
            0o100000 => {
                entry.set_entry_type(tar::EntryType::Regular);
                entry.set_size(size as u64);
                out.append_data(&mut entry, path, data).unwrap();
            }
            0o120000 => {
                entry.set_entry_type(tar::EntryType::Symlink);
                // gen_init_cpio keeps a link's target with its NUL.
                let target = std::str::from_utf8(data).unwrap().trim_end_matches('\0');
                out.append_link(&mut entry, path, target).unwrap();
            }
            0o020000 => {
                entry.set_entry_type(tar::EntryType::Char);
                entry.set_device_major(u32::try_from(rmajor).unwrap()).unwrap();
                entry.set_device_minor(u32::try_from(rminor).unwrap()).unwrap();
                out.append_data(&mut entry, path, std::io::empty()).unwrap();
            }
            other => panic!("{path}: a cpio entry of type {other:o}"),
        }
    }
    out.finish().unwrap();
}

/// A machined binary of this build, run as a process of its own: one cladefs daemon a process,
/// so a test that needs machined twice, or a daemon's cells before its machined, runs one so.
pub struct Process {
    child: std::process::Child,
    pub socket: PathBuf,
}

impl Process {
    /// Starts `machined --images IMAGES --state STATE --socket <state's>/machined.sock ARGS` and
    /// waits until it serves.
    pub async fn start(images: &Path, state: &Path, args: &[&str]) -> Self {
        let socket = state.with_file_name("machined-process.sock");
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_machined"))
            .arg("--images")
            .arg(images)
            .arg("--state")
            .arg(state)
            .arg("--socket")
            .arg(&socket)
            .args(args)
            .spawn()
            .unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while machined::client::channel_unix(&socket).await.is_err() {
            assert!(
                std::time::Instant::now() < until,
                "machined did not serve at {}",
                socket.display()
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Self { child, socket }
    }

    /// Stops it as SIGTERM does, and waits for it: its cells drained, what it made kept.
    pub fn stop(mut self) {
        // SAFETY: a plain system call, on the test's own child.
        let pid = libc::pid_t::try_from(self.child.id()).unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "machined stopped with {status}");
    }
}

impl Process {
    /// Kills it, as a crash would, and waits for it: what its daemon committed is on its disk.
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Process {
    /// A test that failed with it running leaves nothing running.
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|ended| ended.is_none()) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A version of `cell`, made now.
pub async fn make_version(socket: &Path, cell: &machined::proto::Cell) -> u64 {
    use cladefsd::control::proto::MakeVersionRequest;
    let request = MakeVersionRequest { tenant: cell.tenant, cell: cell.cell, ..Default::default() };
    cladefsd(socket).await.make_version(request).await.unwrap().into_inner().version
}
