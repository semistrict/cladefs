// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the linux-checkout cases have in common ([`crate::test_linux_checkout`]): VMs that run one
//! after the other on a cell, and what their guests do with a repo kept in a volume.

use macros::{guest, host};

/// The volume the first VM checks Linux out into.
const VOLUME: &str = "repo";

#[host]
pub mod host {
    use super::*;
    use crate::TestSetup;
    use crate::measure::{HostMemory, Requests, Served};
    use crate::vm::{Firecracker, Vm};
    use cladefs::{Filesystem, Storage};
    use futures::executor::block_on;
    use std::cell::Cell;
    use std::ffi::CString;
    use std::path::{Path, PathBuf};

    /// Creates and formats a cell on `cladefsd` (backed by `disk_root`) with its default volume and
    /// the volume the first VM checks Linux out into.
    pub fn format(
        cladefsd: &Path,
        disk_root: &Path,
        capacity: u64,
    ) -> anyhow::Result<cladefs::CellId> {
        let cell = crate::common::create_cell(cladefsd, capacity)?;
        let path = crate::common::cell_disk_path(disk_root, cell);
        block_on(async {
            let fs = Filesystem::format(Storage::open(&path).await?).await?;
            fs.create_volume(VOLUME).await?;
            fs.close().await?;
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(cell)
    }

    /// A test's VMs, which run one after the other, with `cpus` and `memory_mib` each.
    pub struct Vms<'a> {
        pub test_setup: &'a TestSetup,
        pub root: PathBuf,
        pub cpus: u8,
        pub memory_mib: u32,
        /// Their cells' capacity, and their DAX windows' size, none if 0.
        pub capacity: u64,
        pub window: u64,
        /// The socket of the extent daemon that serves their cells.
        pub cladefsd: PathBuf,
        /// What says the host's memory as their guests ask ([`crate::measure`]), if they do.
        pub memory: Option<HostMemory>,
        /// How many have booted.
        booted: Cell<usize>,
    }

    /// A VM of [`Vms`], running.
    pub struct Running {
        firecracker: Firecracker,
        /// The thread answering its guest's requests to be measured, if the VMs are; or their
        /// vsock's sockets, kept until it ends, if not.
        served: Result<Served, Requests>,
    }

    impl<'a> Vms<'a> {
        /// VMs of 2 vCPUs and 1 GiB each, on cells of `capacity` that the daemon at `cladefsd`
        /// serves, with DAX windows of `window`, not measured.
        pub fn new(
            test_setup: &'a TestSetup,
            root: PathBuf,
            cladefsd: PathBuf,
            capacity: u64,
            window: u64,
        ) -> Self {
            let booted = Cell::new(0);
            let (cpus, memory_mib, memory) = (2, 1024, None);
            Self { test_setup, root, cpus, memory_mib, capacity, window, cladefsd, memory, booted }
        }

        /// A VM of the guest's `role`, on `cell`.
        pub fn run(&self, role: &str, cell: cladefs::CellId) -> anyhow::Result<Running> {
            self.boot(role, |vm| {
                vm.cell_id_served("extents", cell, self.capacity, self.window, &self.cladefsd);
                Ok(())
            })
        }

        /// A VM of the guest's `role`, with what `devices` gives it, as [`Vms::run`].
        pub fn boot(
            &self,
            role: &str,
            devices: impl FnOnce(&mut Vm) -> anyhow::Result<()>,
        ) -> anyhow::Result<Running> {
            let mut vm = Vm::new(self.cpus, self.memory_mib)?;
            vm.guest_kernel()?;
            devices(&mut vm)?;
            vm.env(&CString::new(format!("CLADEFS_LINUX_CHECKOUT_ROLE={role}"))?);
            // One vsock, its Unix socket's path short, as it must be, for each VM's Firecracker:
            // for measuring on its port.
            let requests = Requests::new(&format!("checkout-{role}"))?;
            vm.vsock(requests.uds());
            let api = Vm::api_socket(role);
            let firecracker = vm.spawn_api(&self.root, &self.test_setup.test_case, role, &api)?;
            let number = self.booted.get() + 1;
            self.booted.set(number);
            let pid = firecracker.pid();
            let served = match self.memory {
                Some(memory) => Ok(requests.serve(memory, number, pid)),
                None => Err(requests),
            };
            Ok(Running { firecracker, served })
        }
    }

    impl Running {
        /// Waits for its VM to end, and passes on what its guest said.
        pub fn wait(self) -> anyhow::Result<()> {
            let (status, console) = self.firecracker.wait()?;
            crate::vm::split_console(std::fs::File::open(&console)?)?;
            if let Ok(served) = self.served {
                served.stop()?;
            }
            anyhow::ensure!(status.success(), "a VM exited {status}");
            Ok(())
        }
    }
}

#[guest]
pub mod guest {
    use super::*;

    use nix::libc;
    use std::collections::HashSet;
    use std::ffi::CString;
    use std::fs;
    use std::io::Error;
    use std::os::unix::fs::chown;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::Duration;

    /// The default volume, on whose directories the others mount.
    pub const MNT: &str = "/mnt/fxfs";
    /// Who runs Git.
    pub const USER: u32 = 1000;
    /// `USER`'s home, in the default volume.
    pub const HOME: &str = "/mnt/fxfs/home";
    /// Where the first VM mounts the volume.
    pub const REPO: &str = "/mnt/fxfs/repo";

    pub fn try_mount(volume: Option<&str>, target: &str) -> std::io::Result<()> {
        let source = CString::new("extents").unwrap();
        let (target, fstype) = (CString::new(target).unwrap(), CString::new("cladefs").unwrap());
        let options = volume.map(|volume| CString::new(format!("volume={volume}")).unwrap());
        let data = options.as_ref().map_or(std::ptr::null(), |options| options.as_ptr());
        // SAFETY: The strings outlive the call.
        let ret = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, data.cast())
        };
        if ret == 0 { Ok(()) } else { Err(Error::last_os_error()) }
    }

    pub fn mount(volume: Option<&str>, target: &str) {
        try_mount(volume, target).unwrap_or_else(|error| panic!("mount {target}: {error}"));
    }

    pub fn unmount(target: &str) {
        let path = CString::new(target).unwrap();
        // SAFETY: The string outlives the call.
        let ret = unsafe { libc::umount(path.as_ptr()) };
        assert_eq!(ret, 0, "umount {target}: {}", Error::last_os_error());
    }

    /// Runs `git args` as `USER` in `dir`, and returns what it printed, which it must succeed at.
    pub fn git(dir: &str, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .uid(USER)
            .gid(USER)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", HOME)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // A status leaves the index as it is: what one finds, the next finds too.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "checkout")
            .env("GIT_AUTHOR_EMAIL", "checkout@cladefs")
            .env("GIT_COMMITTER_NAME", "checkout")
            .env("GIT_COMMITTER_EMAIL", "checkout@cladefs")
            .output()
            .unwrap_or_else(|error| panic!("git: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} in {dir}: {:?}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Has `dir`, and what is made in it, belong to `USER`.
    pub fn give(dir: &str) {
        fs::create_dir_all(dir).unwrap();
        chown(dir, Some(USER), Some(USER)).unwrap();
    }

    /// Mounts the volume to check Linux out into at `REPO`, for `USER`.
    pub fn mount_repo() {
        give(REPO);
        mount(Some(VOLUME), REPO);
        give(REPO);
    }

    /// What the first VM does for a first `git status` elsewhere to trust the index: Git doesn't,
    /// for a file changed in the second the index was written or after, so the index is written
    /// again once that second is over.
    pub fn settle_index() {
        std::thread::sleep(Duration::from_millis(1100));
        git(REPO, &["update-index", "--refresh"]);
        git(REPO, &["update-index", "--force-write-index"]);
    }

    pub fn sync() {
        // SAFETY: It takes nothing.
        unsafe { libc::sync() };
    }

    /// The files the repo at `path` tracks.
    pub fn tracked_files(path: &str) -> HashSet<String> {
        git(path, &["ls-files", "-z"]).split_terminator('\0').map(str::to_owned).collect()
    }

    pub fn drop_caches() {
        fs::write("/proc/sys/vm/drop_caches", b"3").unwrap();
    }
}
