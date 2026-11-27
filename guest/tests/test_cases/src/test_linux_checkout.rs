// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a checkout the size of Linux's costs (cladefs's TASK-136.1): on the guest's Fxfs, with a
//! DAX window and without, against ext4 on a virtio-blk disk, and on virtio-pmem with DAX, which
//! TASK-227 takes Fxfs's DAX to match.
//!
//! A first VM checks Linux out: it clones it, without a checkout, from the bare repo the root
//! filesystem holds, then checks it out, and syncs. Then, its caches dropped, it runs `git status`
//! cold and warm, and once `sync` has written what that left, such as directories' access times;
//! and reads the tree whole, every file of the checkout but `.git`'s, cold and warm. A second VM,
//! booted on the same disk or cell, does the same, a guest that has seen nothing of it. On Fxfs,
//! the first VM checks Linux out into a volume, and the host measures the bytes of the volume's
//! layers and its records in the allocator's tree, and what each cell and bucket takes on its disk
//! after each VM.
//!
//! Every VM is measured as its guest asks ([`crate::measure`]): the host's memory, the VM process's
//! and the extent daemon's, which the case starts for itself, and the guest's, on the log's
//! `memory:` lines. The guest's timings are lines of standard output, `measure ROLE NAME VALUE
//! UNIT`, which the log keeps; a memory stage whose name ends in `-pinned` is measured after
//! dropping the guest's caches. In no suite, for what it takes: each case runs by name
//! (`CLADEFS_E2E_TEST=linux-checkout-fxfs-dax scripts/test-host.sh`), and
//! scripts/linux-checkout-report.py makes tables of what they said. CLADEFS_LINUX_CHECKOUT_WINDOW
//! sets the DAX window's bytes (as large as the data, [`crate::common::dax_window`]), and
//! CLADEFS_LINUX_CHECKOUT_CLADEFSD_MEMORY the daemon's (by default, a page cache of the host's free
//! memory, TASK-358): each larger than the tree, its warm reread finds it all still there, as
//! ext4's finds the host's page cache; a window or memory smaller than the tree, 1 GiB, finds
//! nothing of it the second time (GCE r870 and r881: 25.6 s against 600 ms, TASK-227). ext4's disk,
//! and its pmem device, is an 8 GiB image file, the host's page cache holding what it has of it: a
//! virtio-pmem device maps the file into the guest whole.

use macros::{guest, host};

/// The filesystem a case checks Linux out on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checkout {
    /// ext4 on a virtio-blk disk, on a raw image file the host's page cache caches.
    Ext4,
    /// ext4 with DAX (`dax=always`) on writable virtio-pmem, on a raw image file the VM maps:
    /// files read in place from the host's page cache, TASK-227's mark for Fxfs's DAX.
    Ext4Pmem,
    /// The guest's Fxfs on an extent-store device, with files in the page cache.
    Fxfs,
    /// The guest's Fxfs on an extent-store device, mapping files from its DAX window.
    FxfsDax,
}

impl Checkout {
    /// Whether it is ext4's, on an image file: one VM checks it out, another opens it.
    #[cfg_attr(feature = "host", allow(dead_code))]
    fn ext4(self) -> bool {
        matches!(self, Checkout::Ext4 | Checkout::Ext4Pmem)
    }
}

pub struct TestLinuxCheckout {
    #[cfg_attr(feature = "guest", allow(dead_code))]
    pub on: Checkout,
}

/// The variable naming the ext4 device in the guest.
#[cfg_attr(feature = "host", allow(dead_code))]
const DEVICE: &str = "CLADEFS_LINUX_CHECKOUT_DEVICE";

#[host]
mod host {
    use super::*;
    use crate::checkout_vms::host::{Vms, format};
    use crate::common::{Window, cladefs_guest_should_run, dax_window, setup_rootfs, spawn_daemon};
    use crate::measure::{HostMemory, metrics_address};
    use crate::vm::{Vm, without_kernel_lines};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::Context;
    use cladefs::{Options, Storage};
    use futures::executor::block_on;
    use std::ffi::{CString, OsStr};
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;

    /// Linux, its tree alone, which the first VM clones, and Git, strace, for the first `git
    /// status`'s check, and e2fsprogs, for ext4's. The clone's objects are in one pack, as a repo
    /// cloned whole has; its history isn't what a checkout's costs depend on.
    const CONTAINERFILE: &str = "FROM fedora:43
RUN dnf install -y git strace e2fsprogs && dnf clean all
RUN git clone --bare --depth 1 --branch v6.18 https://github.com/torvalds/linux /linux.git
";
    const CPUS: u8 = 4;
    const MEMORY_MIB: u32 = 4096;
    const CAPACITY: u64 = 8 << 30;

    /// The bytes the variable `name` says, if set, as a count.
    fn bytes(name: &str) -> anyhow::Result<Option<u64>> {
        let Some(value) = std::env::var(name).ok().filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(value.parse().with_context(|| format!("{name} is a count of bytes"))?))
    }

    /// Says what the volume of the first VM's cell at `path` takes of its metadata.
    fn measure_metadata(path: &Path) -> anyhow::Result<()> {
        block_on(async {
            let fs = Options::new().volume("repo").mount(Storage::open(path).await?).await?;
            let metadata = fs.metadata().await?;
            println!("measure host volume-layers {} bytes", metadata.layer_bytes);
            println!(
                "measure host volume-allocator-records {} records",
                metadata.allocator_records
            );
            fs.close().await?;
            Ok(())
        })
    }

    /// The bytes of the host's disk that what `path` names takes, all it holds told.
    fn disk_bytes(path: &Path) -> std::io::Result<u64> {
        let metadata = std::fs::symlink_metadata(path)?;
        let mut bytes = metadata.blocks() * 512;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                bytes += disk_bytes(&entry?.path())?;
            }
        }
        Ok(bytes)
    }

    /// Says what each of the cells, buckets and images in `dir` takes of the host's disk, after
    /// the VM of `role` ran.
    fn measure_disk(dir: &Path, role: &str) -> anyhow::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let bytes = disk_bytes(&entry.path())?;
            println!("measure host {role}-disk-{} {bytes} bytes", name.to_string_lossy());
        }
        Ok(())
    }

    impl TestLinuxCheckout {
        fn run(&self, test_setup: &TestSetup) -> anyhow::Result<()> {
            let dir = &test_setup.tmp_dir;
            let root = setup_rootfs(test_setup)?;
            // A daemon of the case's own, whose memory the host measures, its socket's path short,
            // as Unix sockets' must be; its bucket a directory, whose bytes the host measures.
            let sockets = tempfile::Builder::new().prefix("cladefs-linux").tempdir()?;
            let cladefsd = sockets.path().join("cladefsd.sock");
            let metrics = metrics_address()?;
            let bucket = dir.join("bucket");
            std::fs::create_dir(&bucket)?;
            let (metrics_at, bucket_at) =
                (metrics.to_string(), format!("file://{}/", bucket.canonicalize()?.display()));
            let env = [
                ("CLADEFSD_METRICS", OsStr::new(&metrics_at)),
                ("CLADEFSD_BUCKET", OsStr::new(&bucket_at)),
            ];
            let memory = bytes("CLADEFS_LINUX_CHECKOUT_CLADEFSD_MEMORY")?;
            let daemon = spawn_daemon(test_setup, &cladefsd, "cladefsd", &env, memory)?;
            let memory = HostMemory::new(daemon.id(), metrics);
            memory.say(0, "before", None, "")?;
            let window = match self.on {
                Checkout::FxfsDax => match bytes("CLADEFS_LINUX_CHECKOUT_WINDOW")? {
                    Some(window) => window,
                    None => dax_window(Window::Large)?,
                },
                Checkout::Ext4 | Checkout::Ext4Pmem | Checkout::Fxfs => 0,
            };
            let mut vms = Vms::new(test_setup, root, cladefsd, CAPACITY, window);
            (vms.cpus, vms.memory_mib, vms.memory) = (CPUS, MEMORY_MIB, Some(memory));
            match self.on {
                Checkout::Ext4 | Checkout::Ext4Pmem => ext4(&vms, dir, self.on),
                Checkout::Fxfs | Checkout::FxfsDax => fxfs(&vms, dir),
            }
        }
    }

    /// The checkout on ext4, `on` a disk or a pmem device the two VMs boot with.
    fn ext4(vms: &Vms, dir: &Path, on: Checkout) -> anyhow::Result<()> {
        let image = dir.join("ext4.img");
        std::fs::File::create(&image)?.set_len(CAPACITY)?;
        let disk = |vm: &mut Vm| -> anyhow::Result<()> {
            let device = match on {
                Checkout::Ext4 => vm.disk("repo", &image)?,
                Checkout::Ext4Pmem => vm.pmem("repo", &image)?,
                Checkout::Fxfs | Checkout::FxfsDax => anyhow::bail!("{on:?} is no ext4"),
            };
            vm.env(&CString::new(format!("{DEVICE}={device}"))?);
            Ok(())
        };
        vms.boot("checkout", disk)?.wait()?;
        measure_disk(dir, "checkout")?;
        vms.boot("reopened", disk)?.wait()?;
        measure_disk(dir, "reopened")
    }

    /// The checkout on Fxfs: in the first VM's cell, which a second VM opens.
    fn fxfs(vms: &Vms, dir: &Path) -> anyhow::Result<()> {
        let disk_root = crate::common::disk_root_for(dir, "cladefsd");
        let cell = format(&vms.cladefsd, &disk_root, CAPACITY)?;
        vms.run("checkout", cell)?.wait()?;
        measure_metadata(&crate::common::cell_disk_path(&disk_root, cell))?;
        measure_disk(dir, "checkout")?;
        vms.run("reopened", cell)?.wait()
    }

    impl Test for TestLinuxCheckout {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn rootfs_image(&self) -> Option<crate::rootfs::Image> {
            Some(crate::rootfs::Image::new(CONTAINERFILE))
        }

        /// The clone alone took 8 to 44 minutes in Lima on a busy host.
        fn timeout_secs(&self) -> u64 {
            7200
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            if let Err(err) = self.run(&test_setup) {
                eprintln!("{err:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, _test_setup: TestSetup) -> TestOutcome {
            let output = without_kernel_lines(&String::from_utf8(stdout).unwrap());
            let said: Vec<_> = output.lines().filter(|line| line.starts_with("OK ")).collect();
            let expected = ["OK checkout", "OK reopened"];
            if said == expected {
                TestOutcome::Pass
            } else {
                TestOutcome::Fail(format!("expected {expected:?}, got {said:?}"))
            }
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use crate::checkout_vms::guest::{
        HOME, MNT, REPO, drop_caches, git, give, mount, mount_repo, settle_index, sync,
        tracked_files, unmount,
    };
    use std::fs;
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// Linux's bare clone, which the root filesystem's image holds.
    const UPSTREAM: &str = "/linux.git";

    /// Says what the guest of a role measures.
    struct Measure {
        role: String,
    }

    impl Measure {
        fn say(&self, name: &str, value: impl std::fmt::Display, unit: &str) {
            println!("measure {} {name} {value} {unit}", self.role);
        }

        fn duration(&self, name: &str, duration: Duration) {
            self.say(name, duration.as_secs_f64() * 1e3, "ms");
        }

        /// Runs `f`, saying how long it took as `name`.
        fn time<T>(&self, name: &str, f: impl FnOnce() -> T) -> T {
            let start = Instant::now();
            let output = f();
            self.duration(name, start.elapsed());
            output
        }

        /// Has the host measure its memory and the guest's as of `stage` ([`crate::measure`]).
        fn memory(&self, stage: &str) {
            crate::measure::measure(&format!("{}-{stage}", self.role));
        }

        /// The same, once the guest's caches are dropped: what it keeps.
        fn pinned(&self, stage: &str) {
            drop_caches();
            self.memory(&format!("{stage}-pinned"));
        }
    }

    /// Runs `program` with `args`, as root, which must succeed.
    fn run(program: &str, args: &[&str]) {
        let status = Command::new(program).args(args).status();
        let status = status.unwrap_or_else(|error| panic!("{program}: {error}"));
        assert!(status.success(), "{program} {args:?}: {status}");
    }

    /// Adds what the directory `path` holds to `files`, `directories` and `bytes`.
    fn walk(path: &Path, files: &mut u64, directories: &mut u64, bytes: &mut u64) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            if metadata.is_dir() {
                *directories += 1;
                walk(&entry.path(), files, directories, bytes);
            } else {
                *files += 1;
                *bytes += metadata.size();
            }
        }
    }

    /// Says what the tree at `path` holds, as `name`.
    fn measure_tree(measure: &Measure, name: &str, path: &str) {
        let (mut files, mut directories, mut bytes) = (0, 0, 0);
        walk(Path::new(path), &mut files, &mut directories, &mut bytes);
        measure.say(&format!("{name}-files"), files, "files");
        measure.say(&format!("{name}-directories"), directories, "directories");
        measure.say(&format!("{name}-bytes"), bytes, "bytes");
    }

    /// Reads every regular file under `path` whole, but `.git`'s, through `buffer`, and returns how
    /// many bytes they held.
    fn read_tree(path: &Path, buffer: &mut [u8]) -> u64 {
        let mut bytes = 0;
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() && entry.file_name() != ".git" {
                bytes += read_tree(&entry.path(), buffer);
            } else if kind.is_file() {
                let mut file = fs::File::open(entry.path()).unwrap();
                loop {
                    let read = file.read(buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes += read as u64;
                }
            }
        }
        bytes
    }

    /// Reads the checkout at `path` whole, its caches dropped, then again.
    fn reads(measure: &Measure, path: &str) {
        let mut buffer = vec![0; 1 << 20];
        drop_caches();
        let bytes = measure.time("read-cold", || read_tree(Path::new(path), &mut buffer));
        measure.say("read-bytes", bytes, "bytes");
        assert_eq!(measure.time("read-warm", || read_tree(Path::new(path), &mut buffer)), bytes);
    }

    /// The `git status`es of the repo at `path`, which find it clean: the first, its caches
    /// dropped, the next, the first once `sync` has written what they left, such as directories'
    /// access times, and the one after.
    fn statuses(measure: &Measure, path: &str) {
        let status = ["status", "--porcelain"];
        // What a program costs to load isn't what this measures.
        git(path, &["--version"]);
        drop_caches();
        assert_eq!(measure.time("status-cold", || git(path, &status)), "");
        measure.memory("status");
        assert_eq!(measure.time("status-warm", || git(path, &status)), "");
        sync();
        assert_eq!(measure.time("status-synced", || git(path, &status)), "");
        assert_eq!(measure.time("status-warm-again", || git(path, &status)), "");
    }

    /// Checks Linux out at `REPO`, and measures it.
    fn checkout(measure: &Measure, on: Checkout) {
        if !on.ext4() {
            mount_repo();
        }
        measure.pinned("booted");
        git("/", &["--version"]);
        let clone =
            ["-c", "safe.directory=*", "clone", "--quiet", "--no-local", "--no-checkout", UPSTREAM];
        measure.time("clone", || git("/", &[&clone[..], &[REPO]].concat()));
        git(REPO, &["config", "gc.auto", "0"]);
        measure.time("checkout", || git(REPO, &["reset", "--hard", "--quiet", "HEAD"]));
        measure.time("sync", sync);
        measure.say("tracked-files", tracked_files(REPO).len(), "files");
        measure_tree(measure, "volume", REPO);
        measure_tree(measure, "git", &format!("{REPO}/.git"));
        settle_index();
        sync();
        measure.memory("checked-out");
        measure.pinned("checked-out");

        statuses(measure, REPO);
        reads(measure, REPO);
        measure.memory("read");
        measure.pinned("read");
        unmount(REPO);
    }

    /// Opens the checkout at `REPO` in a VM that has seen nothing of it, and measures it.
    fn reopened(measure: &Measure, on: Checkout) {
        measure.pinned("booted");
        if !on.ext4() {
            // Given to the user as the first VM checked it out: nothing to write.
            measure.time("mount", || mount(Some("repo"), REPO));
        }
        measure.memory("mounted");
        statuses(measure, REPO);
        reads(measure, REPO);
        measure.memory("read");
        measure.pinned("read");
        unmount(REPO);
    }

    impl Test for TestLinuxCheckout {
        fn in_guest(self: Box<Self>) {
            let role = std::env::var("CLADEFS_LINUX_CHECKOUT_ROLE").unwrap();
            let measure = Measure { role: role.clone() };
            fs::create_dir_all(MNT).unwrap();
            if self.on.ext4() {
                let device = std::env::var(DEVICE).unwrap();
                let fresh = role == "checkout";
                if fresh {
                    measure.time("mkfs", || run("mkfs.ext4", &["-q", "-F", &device]));
                }
                fs::create_dir_all(REPO).unwrap();
                // On pmem, ext4 maps files from the device, or refuses to mount.
                let options = if self.on == Checkout::Ext4Pmem { "dax=always" } else { "defaults" };
                let mount = ["-t", "ext4", "-o", options, &device, REPO];
                measure.time("mount", || run("mount", &mount));
                if self.on == Checkout::Ext4Pmem {
                    let mounts = fs::read_to_string("/proc/mounts").unwrap();
                    let line = mounts.lines().find(|line| line.contains(REPO)).unwrap();
                    assert!(line.contains("dax=always"), "ext4 mounted without DAX: {line}");
                }
                if fresh {
                    // Git clones into an empty directory alone.
                    fs::remove_dir(format!("{REPO}/lost+found")).unwrap();
                    give(REPO);
                }
            } else {
                measure.time("default-mount", || mount(None, MNT));
            }
            give(HOME);
            match (self.on, role.as_str()) {
                (on, "checkout") => checkout(&measure, on),
                (on, "reopened") => reopened(&measure, on),
                (on, role) => panic!("no role {role} on {on:?}"),
            }
            if !self.on.ext4() {
                unmount(MNT);
            }
            println!("OK {role}");
        }
    }
}
