// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The checks of the filesystem as a whole: its counts, options, freezing, unbinding, lockups, what
//! it does not do, and running out of space.

use super::*;

/// cladefs: /proc/fs/cladefs/<tag> says what the filesystem waited on and its device's requests
/// took, which the guest counts as idle (TASK-91.7): an fsync's commit and its seal among them.
pub(super) fn test_stats() {
    let before = stats();
    let mut file = fs::File::create(path("stats")).unwrap();
    file.write_all(b"counted").unwrap();
    file.sync_all().unwrap();
    let after = stats();
    let grew = |name: &str| after[name] > before[name];
    assert_eq!(
        ["device_commits", "seals", "seal_waits", "device_writes"].map(grew),
        [true; 4],
        "{before:?} then {after:?}"
    );
    // And their histograms (TASK-111.6).
    assert_eq!(
        ["device_commits_hist", "seals_hist", "seal_waits_hist"].map(grew),
        [true; 3],
        "{before:?} then {after:?}"
    );
    fs::remove_file(path("stats")).unwrap();
}

/// Freezing the filesystem and thawing it (FIFREEZE, FITHAW).
pub(super) fn test_freeze() {
    freeze_at(MNT, FIFREEZE).expect("FIFREEZE");
    // A writer waits while it is frozen, and goes on as it thaws.
    let writer = writer_waits("frozen", b"written once thawed", "frozen");
    freeze_at(MNT, FITHAW).expect("FITHAW");
    writer.join().unwrap();
    assert_eq!(fs::read(path("frozen")).unwrap(), b"written once thawed");
    fs::write(path("thawed"), b"writes go on").unwrap();
    assert_eq!(fs::read(path("thawed")).unwrap(), b"writes go on");
}

/// cladefs: freezing under active concurrent mmap(MAP_SHARED, PROT_WRITE) faults, write(2), and
/// delayed unlinks settles all DAX dirty pages before closing the device gate (leaving 0 private
/// and 0 dirty DAX pages when frozen), and a failed MAP_MANY during post-freeze rebind never
/// deadlocks on cladefs_fsync while frozen (board 12113).
pub(super) fn test_freeze_under_load(dax: bool) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const PAGES: usize = 32;
    const PAGE: usize = 4096;
    const LEN: usize = PAGES * PAGE;

    let mapped_path = path("freeze-load-mmap");
    fs::write(&mapped_path, vec![0x11u8; LEN]).unwrap();
    if dax {
        read_in_sealed(&mapped_path);
    }

    // Shared anonymous page for coordinating the forked mmap writer. The mmap writer runs in a
    // separate process (its own `mm_struct`) because a thread faulting on a frozen mount holds
    // `mm->mmap_lock` in `sb_start_pagefault()`, which would block `mmap`-backed allocations in
    // the freezing process before `FITHAW`.
    // SAFETY: valid anonymous shared 1-page mapping.
    let ctrl = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(ctrl, libc::MAP_FAILED, "mmap ctrl: {}", std::io::Error::last_os_error());
    let child_stop = ctrl as *const AtomicBool;
    // SAFETY: `ctrl` is zero-initialized, valid, and 4096-byte aligned.
    unsafe { (*child_stop).store(false, Ordering::Release) };

    let mapped_cstr = CString::new(mapped_path.as_str()).unwrap();
    // SAFETY: single-threaded at this point; child only uses raw syscalls and `_exit`.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        // SAFETY: raw syscalls in freshly forked child; exit via `_exit`.
        unsafe {
            let fd = libc::open(mapped_cstr.as_ptr(), libc::O_RDWR);
            if fd < 0 {
                libc::_exit(2);
            }
            let map = libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if map == libc::MAP_FAILED {
                libc::_exit(3);
            }
            let mut round = 0u8;
            while !(*child_stop).load(Ordering::Acquire) {
                round = round.wrapping_add(1);
                for p in 0..PAGES {
                    std::ptr::write_volatile((map as usize + p * PAGE) as *mut u8, round);
                }
            }
            libc::munmap(map, LEN);
            libc::close(fd);
            libc::_exit(0);
        }
    }

    struct ChildGuard {
        pid: libc::pid_t,
        ctrl: *mut libc::c_void,
        reaped: bool,
    }
    impl ChildGuard {
        fn stop_and_wait(&mut self) {
            if self.reaped {
                return;
            }
            // SAFETY: `self.ctrl` points to our shared `AtomicBool`.
            unsafe { (*(self.ctrl as *const AtomicBool)).store(true, Ordering::Release) };
            let mut status = 0;
            // SAFETY: waiting on our own child pid.
            let waited = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            self.reaped = true;
            assert_eq!(waited, self.pid, "waitpid: {}", std::io::Error::last_os_error());
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "mmap child exited abnormally: status={status:#x}"
            );
        }
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !self.reaped {
                // SAFETY: clean up child on unwind so it does not hold the mount open.
                unsafe {
                    (*(self.ctrl as *const AtomicBool)).store(true, Ordering::Release);
                    libc::kill(self.pid, libc::SIGKILL);
                    let mut status = 0;
                    libc::waitpid(self.pid, &mut status, 0);
                }
            }
            // SAFETY: unmapping the 1-page anonymous control region.
            unsafe { libc::munmap(self.ctrl, PAGE) };
        }
    }
    let mut mmap_child = ChildGuard { pid, ctrl, reaped: false };

    let stop = Arc::new(AtomicBool::new(false));
    let file_writer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let p = path(&format!("freeze-load-tmp-{}", i % 16));
                fs::write(&p, format!("round {i}")).unwrap();
                let _ = fs::remove_file(&p);
                i += 1;
            }
        })
    };

    // Repeatedly freeze and thaw while active mmap faults, writes, and delayed unlinks race
    // through SB_FREEZE_WRITE -> SB_FREEZE_PAGEFAULT -> SB_FREEZE_FS.
    for round in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        freeze_at(MNT, FIFREEZE).expect("FIFREEZE under load");
        if dax {
            let s = stats();
            let private = s.get("dax_private_pages").copied().unwrap_or(0);
            let dirty = s.get("dax_dirty_pages").copied().unwrap_or(0);
            assert_eq!(
                (private, dirty),
                (0, 0),
                "round {round}: frozen with private={private}, dirty={dirty} pages"
            );
        }
        freeze_at(MNT, FITHAW).expect("FITHAW under load");
    }

    stop.store(true, Ordering::Relaxed);
    file_writer.join().unwrap();
    mmap_child.stop_and_wait();

    // On DAX, if cladefs_dax_frozen's rebind encounters open extents (`open > 0`, simulated via
    // `dax_fail_rebind_open`), cladefs_dax_rebind must NOT call cladefs_fsync while frozen
    // (without the `seal` guard, that deadlocks on hold_commits / the closed device gate).
    if dax {
        let file = fs::OpenOptions::new().read(true).write(true).open(&mapped_path).unwrap();
        // SAFETY: valid MAP_SHARED writable mapping of `LEN` bytes on an open file.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
        let addr = ptr as usize;
        for p in [0usize, 4, 8] {
            // SAFETY: within `LEN`-byte mapping.
            unsafe {
                std::ptr::write_volatile((addr + p * PAGE) as *mut u8, 0x5a);
            }
        }
        let seals_before = stats().get("dax_seals").copied().unwrap_or(0);
        {
            let _open = Param::set("dax_fail_rebind_open", "1");
            freeze_at(MNT, FIFREEZE).expect("FIFREEZE with open-extent rebind");
            assert_injected("dax_fail_rebind_open");
        }
        let after = stats();
        let seals_after = after.get("dax_seals").copied().unwrap_or(0);
        assert_eq!(
            seals_after, seals_before,
            "cladefs_dax_rebind attempted to seal/fsync while frozen ({seals_before} -> {seals_after})"
        );
        freeze_at(MNT, FITHAW).expect("FITHAW after open-extent rebind");
        // SAFETY: unmapping the `LEN`-byte mapping created above.
        assert_eq!(unsafe { libc::munmap(ptr, LEN) }, 0, "munmap");
        drop(file);
    }

    let _ = fs::remove_file(&mapped_path);
    for i in 0..16 {
        let _ = fs::remove_file(path(&format!("freeze-load-tmp-{i}")));
    }
}

/// A writer of `data` to the file `name`, which waits, the filesystem frozen (`what` says how):
/// to be joined once it thaws.
fn writer_waits(name: &str, data: &'static [u8], what: &str) -> std::thread::JoinHandle<()> {
    let (wrote, written) = std::sync::mpsc::channel();
    let name = path(name);
    let writer = std::thread::spawn(move || {
        fs::write(name, data).unwrap();
        // Waited for no longer, once it waited.
        let _ = wrote.send(());
    });
    let waited = written.recv_timeout(std::time::Duration::from_millis(500));
    assert_eq!(waited, Err(std::sync::mpsc::RecvTimeoutError::Timeout), "wrote while {what}");
    writer
}

/// cladefs: a thaw that fails leaves the filesystem frozen, as the VFS leaves the superblock, and
/// the next goes on from the step it stopped at, never taking one twice (TASK-200): failing
/// before it tells the device it thaws (cladefs.fail_thaw_at 1), and before it renews the journal's
/// tail, the device told, and open again (2). Writers wait meanwhile, and write once it thaws,
/// durably. The device is told of the freeze, and of the thaw, once each. Fxfs took a thaw that
/// failed for done: the next let every writer in, to wait for ever at the device it held closed.
pub(super) fn test_failed_thaw_thaws_on_retry() {
    for step in [1, 2] {
        let told = device_requests("freeze");
        freeze_at(MNT, FIFREEZE).expect("FIFREEZE");
        assert_eq!(device_requests("freeze"), told + 1, "frozen, step {step}: the device told");
        {
            let _fail = Param::set("fail_thaw_at", &step.to_string());
            let failed = freeze_at(MNT, FITHAW).expect_err("a thaw made to fail");
            assert_eq!(failed.raw_os_error(), Some(libc::EIO), "step {step}: {failed}");
            assert_injected("fail_thaw_at");
        }
        let thawed_told = told + 1 + u64::from(step > 1);
        assert_eq!(device_requests("freeze"), thawed_told, "step {step}: the device told");
        let name = format!("thawed-on-retry-{step}");
        let writer = writer_waits(&name, b"written once thawed", "a thaw failed");
        freeze_at(MNT, FITHAW).expect("FITHAW after one failed");
        writer.join().unwrap();
        assert_eq!(device_requests("freeze"), told + 2, "step {step}: told of the thaw once");
        fs::File::open(path(&name)).unwrap().sync_all().unwrap();
        assert_eq!(fs::read(path(&name)).unwrap(), b"written once thawed");
        fs::remove_file(path(&name)).unwrap();
    }
}

/// The image mounted writable again as this goes, if a check that mounts it read-only failed: the
/// checks after it write.
struct MountedWritable;

impl Drop for MountedWritable {
    fn drop(&mut self) {
        let mounts = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let read_only = mounts.lines().any(|mount| {
            let fields: Vec<&str> = mount.split_whitespace().collect();
            fields.get(1) == Some(&MNT) && fields.get(3).is_some_and(|o| o.starts_with("ro"))
        });
        if read_only {
            // Not a panic, which in a panic's unwinding would abort before the first said why.
            let _ = freeze_at(MNT, FITHAW);
            let unmounted = unsafe { libc::umount(CString::new(MNT).unwrap().as_ptr()) };
            if unmounted != 0 || mount_with(0).is_err() {
                eprintln!("mounting {MNT} writable again: {}", std::io::Error::last_os_error());
            }
        }
    }
}

/// cladefs: a volume remounted read-only, or mounted so, freezes its filesystem as a writable one
/// does, which tells the device, and thaws it (TASK-202): the VFS freezes a read-only superblock
/// without a call to its filesystem, which waited for it for ever, and its VM was never
/// snapshotted, though fsfreeze returned 0. Remounted writable, it takes writes again; what was
/// written before reads back mounted read-only.
pub(super) fn test_read_only_freezes() {
    let freezes_and_thaws = |what: &str| {
        let told = device_requests("freeze");
        freeze_at(MNT, FIFREEZE).expect("FIFREEZE");
        assert_eq!(device_requests("freeze"), told + 1, "{what}: frozen, the device told");
        freeze_at(MNT, FITHAW).expect("FITHAW");
        assert_eq!(device_requests("freeze"), told + 2, "{what}: thawed, the device told");
    };
    let _writable = MountedWritable;
    let name = path("read-only");
    fs::write(&name, b"written before").unwrap();
    // Its inode dirty, which the remount writes back.
    fs::set_permissions(&name, fs::Permissions::from_mode(0o600)).unwrap();
    mount_with(libc::MS_REMOUNT | libc::MS_RDONLY).expect("remounting read-only");
    freezes_and_thaws("remounted read-only");
    mount_with(libc::MS_REMOUNT).expect("remounting writable");
    fs::write(&name, b"written again").unwrap();
    // The filesystem too, which writes nothing.
    unmount();
    mount_with(libc::MS_RDONLY).expect("mounting read-only");
    freezes_and_thaws("mounted read-only");
    assert_eq!(fs::read(&name).unwrap(), b"written again");
    assert_eq!(fs::metadata(&name).unwrap().permissions().mode() & 0o777, 0o600);
    unmount();
    mount();
    fs::remove_file(&name).unwrap();
}

/// cladefs: unmounting a read-only (or remounted read-only) volume after a failed thaw neither
/// deadlocks in `close_volume` while commits are held (hotbed review 91, finding 328) nor leaves
/// the host device frozen across close and reopen (hotbed review 92, finding 327).
pub(super) fn test_unmount_after_failed_thaw() {
    let _writable = MountedWritable;
    let name = path("failed-thaw-unmount");
    fs::write(&name, b"before failed thaw").unwrap();

    // Case 1: writable filesystem remounted read-only (holds `commits` task while frozen).
    mount_with(libc::MS_REMOUNT | libc::MS_RDONLY).expect("remounting read-only");
    let told = device_requests("freeze");
    freeze_at(MNT, FIFREEZE).expect("FIFREEZE remounted ro");
    assert_eq!(device_requests("freeze"), told + 1, "remounted ro: frozen");
    {
        let _fail = Param::set("fail_thaw_at", "1");
        let failed = freeze_at(MNT, FITHAW).expect_err("FITHAW made to fail");
        assert_eq!(failed.raw_os_error(), Some(libc::EIO), "remounted ro FITHAW: {failed}");
        assert_injected("fail_thaw_at");
    }
    {
        let _fail = Param::set("fail_thaw_at", "1");
        unmount();
        assert_injected("fail_thaw_at");
    }
    // `Filesystem::close` sent best-effort `T_FREEZE(0)` even though `cladefs_thaw` failed.
    assert_eq!(device_requests("freeze"), told + 2, "remounted ro: close unfroze device");
    mount();
    // `ExtentDevice::open` also synchronizes the host device to unfrozen.
    assert_eq!(device_requests("freeze"), told + 3, "remounted ro: reopen unfroze device");
    assert_eq!(fs::read(&name).unwrap(), b"before failed thaw");
    fs::write(&name, b"after remounted ro").unwrap();
    fs::File::open(&name).unwrap().sync_all().unwrap();

    // Case 2: filesystem opened read-only (`commits: None`).
    unmount();
    mount_with(libc::MS_RDONLY).expect("mounting read-only");
    let told = device_requests("freeze");
    freeze_at(MNT, FIFREEZE).expect("FIFREEZE mounted ro");
    assert_eq!(device_requests("freeze"), told + 1, "mounted ro: frozen");
    {
        let _fail = Param::set("fail_thaw_at", "1");
        let failed = freeze_at(MNT, FITHAW).expect_err("FITHAW made to fail");
        assert_eq!(failed.raw_os_error(), Some(libc::EIO), "mounted ro FITHAW: {failed}");
        assert_injected("fail_thaw_at");
    }
    {
        let _fail = Param::set("fail_thaw_at", "1");
        unmount();
        assert_injected("fail_thaw_at");
    }
    assert_eq!(device_requests("freeze"), told + 2, "mounted ro: close unfroze device");
    mount();
    assert_eq!(device_requests("freeze"), told + 3, "mounted ro: reopen unfroze device");
    assert_eq!(fs::read(&name).unwrap(), b"after remounted ro");
    fs::write(&name, b"after mounted ro").unwrap();
    fs::File::open(&name).unwrap().sync_all().unwrap();
    fs::remove_file(&name).unwrap();
}

/// Unbinding the driver from a mounted device waits for it to unmount, the filesystem working
/// meanwhile, rather than freeing what it uses; binding it again gives it back (TASK-79.48).
pub(super) fn test_unbind_waits_for_unmount() {
    use crate::extent_driver;

    let device = extent_driver::device();
    let (unbound, unbinding) = std::sync::mpsc::channel();
    let unbinder = std::thread::spawn({
        let device = device.clone();
        move || {
            extent_driver::unbind(&device);
            unbound.send(()).unwrap();
        }
    });
    let waited = unbinding.recv_timeout(std::time::Duration::from_millis(500));
    assert_eq!(waited, Err(std::sync::mpsc::RecvTimeoutError::Timeout), "unbound while mounted");
    fs::write(path("unbinding"), b"written while unbinding").unwrap();
    fsync_dir(MNT);
    assert_eq!(fs::read(path("unbinding")).unwrap(), b"written while unbinding");
    // Nothing mounts a device going away.
    unmount();
    unbinder.join().unwrap();
    let refused = try_mount().unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(libc::ENOENT), "{refused}");
    extent_driver::bind(&device);
    mount();
    assert_eq!(fs::read(path("unbinding")).unwrap(), b"written while unbinding");
    fs::write(path("bound"), b"written once bound again").unwrap();
    fsync_dir(MNT);
    assert_eq!(fs::read(path("bound")).unwrap(), b"written once bound again");
}

/// /proc/mounts shows the mount as it is: files mapping from the DAX window, if it has one.
pub(super) fn test_show_options(dax: bool) {
    let mounts = fs::read_to_string("/proc/mounts").unwrap();
    let line = mounts
        .lines()
        .find(|line| line.split(' ').nth(1) == Some(MNT))
        .unwrap_or_else(|| panic!("{MNT} not in /proc/mounts: {mounts}"));
    let options: Vec<&str> = line.split(' ').nth(3).unwrap().split(',').collect();
    assert_eq!(options.contains(&"dax"), dax, "{line}");
    assert!(options.contains(&"rw"), "{line}");
}

/// TASK-26.19: what programs see of what Fxfs does not do (docs/code.md, "Guest Fxfs: what it
/// does not do"): FIDEDUPERANGE's status is EOPNOTSUPP, and a MAP_SYNC mapping fails with it.
pub(super) fn test_unsupported() {
    /// `_IOWR(0x94, 54, struct file_dedupe_range)`.
    const FIDEDUPERANGE: libc::c_ulong = 0xc018_9436;
    const MAP_SHARED_VALIDATE: libc::c_int = 0x03;
    const MAP_SYNC: libc::c_int = 0x80000;
    /// `struct file_dedupe_range`, with one `struct file_dedupe_range_info`.
    #[repr(C)]
    struct DedupeRange {
        src_offset: u64,
        src_length: u64,
        dest_count: u16,
        reserved1: u16,
        reserved2: u32,
        dest_fd: i64,
        dest_offset: u64,
        bytes_deduped: u64,
        status: i32,
        reserved: u32,
    }
    fs::write(path("plain"), vec![7; 4096]).unwrap();
    fs::write(path("same"), vec![7; 4096]).unwrap();
    let file = fs::File::open(path("plain")).unwrap();
    let same = fs::OpenOptions::new().read(true).write(true).open(path("same")).unwrap();
    let mut range = DedupeRange {
        src_offset: 0,
        src_length: 4096,
        dest_count: 1,
        reserved1: 0,
        reserved2: 0,
        dest_fd: same.as_raw_fd() as i64,
        dest_offset: 0,
        bytes_deduped: 0,
        status: 0,
        reserved: 0,
    };
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), FIDEDUPERANGE as _, &mut range) };
    assert_eq!(ret, 0, "FIDEDUPERANGE: {}", std::io::Error::last_os_error());
    assert_eq!(range.status, -libc::EOPNOTSUPP, "FIDEDUPERANGE's status");

    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            MAP_SHARED_VALIDATE | MAP_SYNC,
            same.as_raw_fd(),
            0,
        )
    };
    assert_eq!(map, libc::MAP_FAILED);
    let refused = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(refused, Some(libc::EOPNOTSUPP), "MAP_SYNC");
}

/// cladefs: a soft lockup dumps every CPU's stack, as the guest's command line asks (vm::CMDLINE),
/// which a kernel without the setting would ignore but for a line in its log (TASK-133).
pub(super) fn test_lockups_show_every_cpu() {
    let setting = fs::read_to_string("/proc/sys/kernel/softlockup_all_cpu_backtrace").unwrap();
    assert_eq!(setting, "1\n");
}

/// cladefs: the filesystem filled, each way a guest fills it, fails what doesn't fit with
/// ENOSPC, and stays mounted and whole: a panic took the guest's kernel down with it (TASK-60,
/// the sandbox team's pgbench, TASK-58). What fitted is written back, and read back as it was
/// written: in the page cache, a write reserves room as it dirties pages, and fails then if
/// the filesystem has none, where it never did and its writeback, failing, dropped the data
/// (TASK-80). Then what filled it removed, a file is written and read back.
pub(super) fn test_enospc() {
    let dir = path("full");
    fs::create_dir_all(&dir).unwrap();
    let enospc = |result: std::io::Result<()>, what: &str| {
        let err = result.expect_err(what);
        assert_eq!(err.raw_os_error(), Some(libc::ENOSPC), "{what}: {err}");
    };
    // Until a write doesn't fit, as `write` says: Err then; Ok if `write` fitted `limit`
    // times, many more than the filesystem has room for.
    let fill = |limit: usize, write: &mut dyn FnMut(usize) -> std::io::Result<()>| {
        (0..limit).try_for_each(|n| write(n))
    };
    let sync = || unsafe { libc::sync() };
    let block = vec![0x5a; 1 << 20];
    const IMAGE_MIB: usize = (IMAGE_SIZE >> 20) as usize;
    // `path`, opened to read its data as the filesystem has it, its pages written back
    // dropped from the cache: those writeback failed to write are dropped too.
    let stored = |path: &str| {
        let file = fs::File::open(path).unwrap();
        let fd = file.as_raw_fd();
        assert_eq!(unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) }, 0);
        file
    };
    let read_all = |mut file: fs::File| {
        let mut read = Vec::new();
        file.read_to_end(&mut read).unwrap();
        read
    };

    // A file written a MiB at a time, whose writes that fitted fsync, and read back.
    let big = format!("{dir}/big");
    let mut file = fs::File::create(&big).unwrap();
    enospc(fill(IMAGE_MIB * 4, &mut |_| file.write_all(&block)), "a file's writes");
    let fitted = file.metadata().unwrap().len();
    assert!(fitted > (IMAGE_SIZE / 2), "{fitted} bytes fitted");
    file.sync_all().unwrap();
    drop(file);
    let mut read = stored(&big);
    let mut at = 0;
    let mut chunk = vec![0; block.len()];
    loop {
        let n = read.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        assert!(chunk[..n] == block[..n], "the file's data at {at} as written");
        at += n as u64;
    }
    assert_eq!(at, fitted);
    // Open, it would keep its room, unlinked.
    drop(read);
    fs::remove_file(&big).unwrap();
    sync();

    // Files of a block each, as many as fit.
    let mut files = 0;
    enospc(
        fill(IMAGE_MIB * 1024, &mut |n| {
            fs::write(format!("{dir}/f{n}"), &block[..4096])?;
            files = n + 1;
            Ok(())
        }),
        "small files",
    );
    assert!(files > 1000, "{files} small files fitted");
    sync();
    for n in 0..files {
        assert!(read_all(stored(&format!("{dir}/f{n}"))) == block[..4096], "small file {n}");
    }
    // Room for a file of many extents, then truncated while full: as a truncate of
    // PostgreSQL's relations, which panicked (TASK-58).
    for n in 0..files / 2 {
        fs::remove_file(format!("{dir}/f{n}")).unwrap();
    }
    sync();
    let extents = format!("{dir}/extents");
    let file = fs::OpenOptions::new().create(true).write(true).open(&extents).unwrap();
    let mut blocks = 0;
    enospc(
        fill(IMAGE_MIB * 1024, &mut |n| {
            std::os::unix::fs::FileExt::write_all_at(&file, &block[..4096], 2 * n as u64 * 4096)?;
            blocks = n + 1;
            Ok(())
        }),
        "a file of many extents",
    );
    file.sync_all().unwrap();
    let read = stored(&extents);
    let mut data = vec![0; 4096];
    for n in 0..blocks {
        std::os::unix::fs::FileExt::read_exact_at(&read, &mut data, 2 * n as u64 * 4096).unwrap();
        assert!(data == block[..4096], "block {n} of {blocks}");
    }
    drop(read);
    // Full, a directory doesn't fit either. (Nor is fallocate checked: it reserves no space,
    // which an extent store can't, cladefs_data_fallocate.)
    enospc(fill(IMAGE_MIB * 1024, &mut |n| fs::create_dir(format!("{dir}/d{n}"))), "directories");
    file.set_len(0).unwrap();
    drop(file);
    sync();

    // All of it gone, the filesystem is as it was.
    fs::remove_dir_all(&dir).unwrap();
    sync();
    let again = vec![0xa5; 8 << 20];
    fs::write(path("after-full"), &again).unwrap();
    evict(&path("after-full"));
    assert!(fs::read(path("after-full")).unwrap() == again, "read back after full");
    fs::remove_file(path("after-full")).unwrap();
}
