// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what the guest halves of the tests of the guest's Fxfs share (fxfs-guest's,
//! fxfs-guest-checks'): evicting a file's inode, whether a file is DAX, what a mount counts, and
//! files read in from sealed extents, as a base's are, whose writes copy on write in the window
//! (doc-13).

use nix::libc;
use std::ffi::CString;
use std::fs;
use std::os::fd::AsRawFd;

/// Evicts `path`'s inode, for it to be read in again: written back, and its pages dropped, as an
/// inode that has any isn't evicted (dropping every cache would drop the filesystem's metadata's
/// too).
pub fn evict(path: &str) {
    let file = fs::File::open(path).unwrap();
    file.sync_all().unwrap();
    let fd = file.as_raw_fd();
    assert_eq!(unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) }, 0);
    drop(file);
    fs::write("/proc/sys/vm/drop_caches", b"2").unwrap();
}

/// Drops `path`'s pages, written back first, keeping its inode: for a file to be read from the
/// device again through the page cache. On a DAX mount, copying on write, a file created there is
/// the page cache's until its inode is read in again (doc-13, TASK-164.3), which [`evict`] would
/// have it be: so a file made by a check stays the page cache's, as one on a mount without a
/// window does (TASK-149.20.3).
pub fn drop_pages(path: &str) {
    let file = fs::File::open(path).unwrap();
    file.sync_all().unwrap();
    let fd = file.as_raw_fd();
    assert_eq!(unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) }, 0);
}

/// Whether the kernel accesses `path` through the DAX window: STATX_ATTR_DAX.
pub fn is_dax(path: &str) -> bool {
    const STATX_ATTR_DAX: u64 = 0x200000;
    // struct statx: stx_attributes at byte 8, stx_attributes_mask at 56.
    let mut buf = [0u64; 32];
    let c_path = CString::new(path).unwrap();
    let ret = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            c_path.as_ptr(),
            0,
            0x7ff, // STATX_BASIC_STATS
            buf.as_mut_ptr(),
        )
    };
    assert_eq!(ret, 0, "statx: {}", std::io::Error::last_os_error());
    let (attributes, mask) = (buf[1], buf[7]);
    assert_ne!(mask & STATX_ATTR_DAX, 0, "the kernel does not report DAX");
    attributes & STATX_ATTR_DAX != 0
}

/// What the Fxfs mounted from the device tagged `tag` counts, /proc/fs/cladefs/<tag> (TASK-91.7), by
/// name: a histogram (NAME_hist, its buckets' counts comma-separated, TASK-111.6) as how many it
/// counts.
pub fn stats(tag: &str) -> std::collections::BTreeMap<String, u64> {
    let text = fs::read_to_string(format!("/proc/fs/cladefs/{tag}")).unwrap();
    (text.lines())
        .map(|line| {
            let (name, value) = line.split_once(' ').unwrap();
            let counts = value.split(',').map(|count| count.parse::<u64>().unwrap());
            (name.to_owned(), counts.sum())
        })
        .collect()
}

/// Has the file `path`, written here, read in again once its writes are sealed: evicted, which
/// writes it back, and a barrier seals. A file created in the mount is a page-cache inode until
/// then, its writes the page cache's (TASK-164.3); read in again, it is DAX, mapped from sealed
/// extents, its writes the window's, copying on write, as a base's files are.
pub fn read_in_sealed(path: &str) {
    evict(path);
    assert!(is_dax(path), "{path}, read in again once sealed, isn't DAX");
}

/// A file of `len` bytes of `byte`'s pattern, written, and read in again once sealed
/// ([`read_in_sealed`]): a DAX file, as a base's are. Its bytes.
pub fn cow_base_file(path: &str, len: usize, byte: u8) -> Vec<u8> {
    let data: Vec<u8> = (0..len).map(|i| byte ^ (i % 251) as u8).collect();
    fs::write(path, &data).unwrap();
    read_in_sealed(path);
    data
}

/// Standard guest mount point for the virtio-extent `extents` cladefs filesystem.
pub const MNT: &str = "/mnt/fxfs";
/// Linux `FIFREEZE` ioctl number.
pub const FIFREEZE: libc::c_ulong = 0xc004_5877;
/// Linux `FITHAW` ioctl number.
pub const FITHAW: libc::c_ulong = 0xc004_5878;

/// Mounts the virtio-extent `extents` device as `cladefs` at `MNT`.
pub fn mount_extents() {
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

/// Runs `FIFREEZE` or `FITHAW` on `MNT`.
pub fn freeze_mnt(request: libc::c_ulong) {
    let dir = fs::File::open(MNT).unwrap();
    // SAFETY: FIFREEZE / FITHAW ioctl on mounted directory fd.
    let ret = unsafe { libc::ioctl(dir.as_raw_fd(), request as _, 0) };
    assert_eq!(ret, 0, "ioctl {request:#x}: {}", std::io::Error::last_os_error());
}

/// Connects to host CID 2 on `port` over AF_VSOCK and returns the socket as a `File`.
pub fn try_vsock_connect(port: u32) -> std::io::Result<fs::File> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, VsockAddr, connect, socket};
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    let fd = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
        .expect("a vsock socket");
    connect(fd.as_raw_fd(), &VsockAddr::new(2, port))?;
    // SAFETY: `fd` is an open socket owned by the returned File.
    Ok(unsafe { fs::File::from_raw_fd(fd.into_raw_fd()) })
}

/// Reconnects to host vsock `port` until the host sends a role line, returning `(role, conn)`.
pub fn role_from_host(port: u32) -> (String, fs::File) {
    use std::io::{BufRead as _, BufReader};
    let mut conn = None;
    let role = crate::reconnect::exchange(
        || try_vsock_connect(port),
        |host| {
            let mut line = String::new();
            match BufReader::new(host.try_clone()?).read_line(&mut line)? {
                0 => Err(std::io::ErrorKind::UnexpectedEof.into()),
                _ => {
                    conn = Some(host.try_clone()?);
                    Ok(line)
                }
            }
        },
    )
    .expect("reading role from host");
    (role.trim_end().to_owned(), conn.unwrap())
}

/// Allocates a `len`-byte private anonymous mapping owned for the guest's lifetime.
pub fn alloc_anon_buf(len: usize) -> &'static mut [u8] {
    // SAFETY: private anonymous mapping owned for the guest's lifetime.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(ptr, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
    // SAFETY: `ptr` is valid for `len` bytes and never unmapped.
    unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) }
}
