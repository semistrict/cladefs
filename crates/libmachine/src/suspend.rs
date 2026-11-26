// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Suspending a machine while its guest waits: the whole machine (its
//! vCPUs, its devices, the network stack with every guest connection's TCP
//! state, and its RAM) goes to a file of the machine's, and the hypervisor
//! VM, the vCPU, device and network threads and the guest's RAM are freed.
//! What cannot go in a file stays in this process: the host sockets of the
//! guest's connections, the vsock's host end (which agent clients hold),
//! the network's policy and log, the console, and a watcher thread. When
//! something comes for the guest, the machine is restored from the file
//! onto those, and the guest carries on, on the same connections. Nothing
//! the guest can see changes, apart from the time that passes.
//!
//! - **Only an idle guest:** every vCPU halted (waiting for an interrupt)
//!   with no interrupt pending in its local APIC, every device settled, and
//!   the network stack with nothing to do by itself (no DNS lookup or
//!   decider question in flight, no frame waiting either way). A guest
//!   that is halted everywhere has nothing to do until an interrupt comes,
//!   so stopping it there loses nothing. A guest stopped anywhere else
//!   might be in the middle of taking a frame off its network queue, with
//!   its device's interrupts turned off while it polls, and would never be
//!   woken for the frame it had not yet read.
//! - **Woken by what comes for it:** data or a close on a host socket the
//!   network stack would pass on, room on one it waits to write to, a host
//!   connection made, or a packet the vsock's host end has for the guest
//!   (an agent call). The [`Watcher`] waits for those and rings the
//!   [`Doorbell`]; once the machine is restored (to be snapshotted, say)
//!   and until it runs, its devices ring it when they put something in one
//!   of the guest's queues. The guest's own timers do not wake it: they
//!   fire when it runs again, late. Nor do the network stack's: they
//!   resume with the time they had left.
//! - **Its clocks run on:** the restore moves kvmclock and the TSC on by
//!   the time that passed, as for any snapshot, so the guest's wall clock
//!   stays right and its monotonic clock counts the time it waited, as if
//!   it had waited awake (a read timeout still expires), and tells each
//!   vCPU's kvmclock it was paused (KVM_KVMCLOCK_CTRL), so that the
//!   guest's watchdogs do not take the gap for a lockup. Its VM generation
//!   ID stays: it is the same guest, not a clone.
//! - **The file:** one per machine, made unnamed in a directory the caller
//!   chooses at the first suspension (with a snapshot's pages, for a
//!   machine restored from one), going with the machine. A 4 KiB header
//!   says what it is and what it lacks (the host sockets and the vsock's
//!   host end: it is not a template, and `Snapshot::open` refuses it); the
//!   guest's RAM follows from 2 MiB, laid out as in a snapshot, holes for
//!   pages of zeroes; the machine's state (what a snapshot holds, the
//!   network stack's whole state included) follows the RAM. Each later
//!   suspension writes into it only the pages the guest changed since the
//!   last restore (from /proc/self/pagemap: the restored RAM is a private
//!   mapping of the file, so the pages it wrote are the process's own),
//!   and the state anew. Nothing is flushed: pages go to the disk in the
//!   background, and the kernel reclaims them from the page cache as it
//!   needs the memory; a guest woken soon finds them there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// What ended a suspended machine's [`wait_woken`](crate::Machine::wait_woken).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woken {
    /// A device has something for the guest: run it.
    Device,
    /// A [`StopHandle`](crate::StopHandle) asked the machine to stop; the
    /// request is used up, as a run's end would use it.
    Stop,
    /// The wait's timeout passed first; the guest is still suspended.
    TimedOut,
}

/// Rings when a device has something for a suspended guest, and wakes the
/// thread waiting on it: also when the machine's stop handle stops it.
#[derive(Debug, Default)]
pub(crate) struct Doorbell {
    /// Whether a suspension listens. Checked before the lock, so that a
    /// running guest's devices never take it.
    armed: AtomicBool,
    bell: Mutex<Bell>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct Bell {
    /// A device rang since the suspension armed the bell.
    rung: bool,
}

impl Doorbell {
    fn lock(&self) -> MutexGuard<'_, Bell> {
        self.bell.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Listens from now on: rings before this are forgotten.
    pub(crate) fn arm(&self) {
        let mut bell = self.lock();
        bell.rung = false;
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Stops listening: the machine runs again.
    pub(crate) fn disarm(&self) {
        let _bell = self.lock();
        self.armed.store(false, Ordering::SeqCst);
    }

    pub(crate) fn armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    /// Whether a device rang since the bell was armed.
    pub(crate) fn rung(&self) -> bool {
        self.lock().rung
    }

    /// A device put something in a guest queue.
    pub(crate) fn ring(&self) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        let mut bell = self.lock();
        if self.armed.load(Ordering::SeqCst) {
            bell.rung = true;
            self.changed.notify_all();
        }
    }

    /// The machine's stop handle asked it to stop: a waiter looks again.
    pub(crate) fn stop_requested(&self) {
        let _bell = self.lock();
        self.changed.notify_all();
    }

    /// Waits until a device rings, `take_stop` takes a stop request
    /// (checked under the bell's lock, which [`stop_requested`] takes after
    /// the request is made, so that none is missed), or `timeout` passes.
    ///
    /// [`stop_requested`]: Self::stop_requested
    pub(crate) fn wait(&self, take_stop: impl Fn() -> bool, timeout: Option<Duration>) -> Woken {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut bell = self.lock();
        loop {
            if bell.rung {
                return Woken::Device;
            }
            if take_stop() {
                return Woken::Stop;
            }
            bell = match deadline {
                None => self.changed.wait(bell).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                        return Woken::TimedOut;
                    };
                    self.changed.wait_timeout(bell, left).unwrap_or_else(|e| e.into_inner()).0
                }
            };
        }
    }
}

/// The first bytes of a suspended machine's file: not a snapshot's.
pub(crate) const FILE_MAGIC: [u8; 8] = *b"lmsusp\0\0";
/// What a suspended machine's file lacks, in its header: the host sockets
/// of the guest's network connections.
pub(crate) const LACKS_NETWORK: u32 = 1;
/// ... and the vsock's host end.
pub(crate) const LACKS_VSOCK: u32 = 2;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use ram::{RamFile, read_state, write_state, write_to_file};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use watch::Watcher;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod ram {
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    use std::path::Path;
    use std::sync::Arc;

    use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

    #[cfg(test)]
    use super::{LACKS_NETWORK, LACKS_VSOCK};
    use crate::error::{Error, Result};
    use crate::memory::MemoryLayout;
    use crate::snapshot::ram_offsets;

    const PAGE: usize = 4096;
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    const RAMFS_MAGIC: i64 = 0x8584_58f6;
    /// /proc/self/pagemap: the page is present, swapped, or a file's page
    /// (or shared anonymous memory) (Documentation/admin-guide/mm/pagemap.rst).
    const PM_PRESENT: u64 = 1 << 63;
    const PM_SWAPPED: u64 = 1 << 62;
    const PM_FILE: u64 = 1 << 61;
    /// Pagemap entries read at a time: 512 KiB of them, for 256 MiB of RAM.
    const PAGEMAP_CHUNK: usize = 1 << 16;

    /// Where the guest's RAM starts in a suspended machine's file: past the
    /// header, aligned as in a snapshot.
    const RAM_BASE: u64 = 2 << 20;
    /// The header: magic, version, what the file lacks, the state's
    /// offset, length and SHA-256.
    const HEADER_LEN: usize = 64;
    /// The version of the file's layout this libmachine writes and reads.
    const FILE_VERSION: u32 = 1;

    /// The file a machine's RAM is a private mapping of, if it is one: each
    /// RAM region, in the layout's order, from its offset.
    #[derive(Debug, Clone)]
    pub(crate) struct RamFile {
        pub(crate) file: Arc<File>,
        pub(crate) offsets: Vec<u64>,
        /// A suspension's file, this machine's alone, which the next
        /// suspension updates in place. Otherwise a snapshot's, shared with
        /// every machine restored from it, which no write may reach.
        pub(crate) ours: bool,
    }

    /// Writes the RAM regions of `layout` in `mem` to this machine's own
    /// file (see the module's documentation): the one `backing` names if it
    /// is ours, updated with the pages the guest changed since it was
    /// mapped; else a new one in `dir`, holding the snapshot's pages if the
    /// RAM came from one, which `backing` then names. Nothing may write
    /// guest memory meanwhile: every vCPU stopped, the device thread held
    /// off. RAM itself is left as it is: the machine is dropped after, and
    /// a restore maps the file.
    ///
    /// The pages written are the process's own only: those a private
    /// mapping of the file has not copied are the file's already, and stay
    /// so (writing the file under a private mapping changes only what it
    /// has not copied).
    pub(crate) fn write_to_file(
        mem: &GuestMemoryMmap,
        layout: &MemoryLayout,
        dir: &Path,
        backing: &mut Option<RamFile>,
    ) -> Result<()> {
        let io = |source| Error::Io { path: dir.to_owned(), source };
        let regions = ram_offsets(layout, RAM_BASE);
        let hosts = regions
            .iter()
            .map(|(r, _)| {
                mem.get_host_address(GuestAddress(r.start))
                    .map_err(|e| Error::MemoryAllocation(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let (target, fresh) = match backing {
            Some(b) if b.ours => (b.clone(), false),
            _ => {
                let file = unnamed_file(dir).map_err(io)?;
                if on_memory_fs(&file).map_err(io)? {
                    return Err(Error::InvalidConfig(format!(
                        "{} is on tmpfs or ramfs: a suspended guest's RAM there would stay in \
                         memory",
                        dir.display()
                    )));
                }
                let end = regions.last().map_or(RAM_BASE, |(r, offset)| offset + r.size);
                file.set_len(end).map_err(io)?;
                let target = RamFile {
                    file: Arc::new(file),
                    offsets: regions.iter().map(|&(_, offset)| offset).collect(),
                    ours: true,
                };
                // A snapshot's pages the guest has not changed are the
                // snapshot's: copied, holes and all.
                if let Some(snapshot) = backing.as_ref() {
                    for ((r, _), (&from, &to)) in
                        regions.iter().zip(snapshot.offsets.iter().zip(&target.offsets))
                    {
                        copy_data(&snapshot.file, from, &target.file, to, r.size).map_err(io)?;
                    }
                }
                (target, backing.is_none())
            }
        };
        for (&host, (&(r, _), &offset)) in hosts.iter().zip(regions.iter().zip(&target.offsets)) {
            // SAFETY: the region is guest RAM, mapped while `mem` lives,
            // and nothing writes it meanwhile (above).
            let ram = unsafe { std::slice::from_raw_parts(host, r.size as usize) };
            write_changed(&target.file, ram, offset, fresh).map_err(io)?;
        }
        *backing = Some(target);
        Ok(())
    }

    /// Writes the machine's `state` into its own file (`ram`, which
    /// [`write_to_file`] made ours) after the RAM, and the header that says
    /// where it is and what the file `lacks` (`LACKS_*`), then starts the
    /// file's writeback in the background.
    pub(crate) fn write_state(
        ram: &RamFile,
        layout: &MemoryLayout,
        state: &[u8],
        lacks: u32,
    ) -> Result<()> {
        debug_assert!(ram.ours, "never a snapshot's file");
        let io = |source| Error::Io { path: "the suspended machine's file".into(), source };
        let ram_end = layout
            .of_kind(crate::memory::RegionKind::Ram)
            .zip(&ram.offsets)
            .map(|(r, offset)| offset + r.size)
            .max()
            .unwrap_or(RAM_BASE);
        let at = ram_end.next_multiple_of(PAGE as u64);
        ram.file.write_all_at(state, at).map_err(io)?;
        ram.file.set_len(at + state.len() as u64).map_err(io)?;
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&super::FILE_MAGIC);
        header.extend_from_slice(&FILE_VERSION.to_le_bytes());
        header.extend_from_slice(&lacks.to_le_bytes());
        header.extend_from_slice(&at.to_le_bytes());
        header.extend_from_slice(&(state.len() as u64).to_le_bytes());
        header.extend_from_slice(&<sha2::Sha256 as sha2::Digest>::digest(state));
        debug_assert_eq!(header.len(), HEADER_LEN);
        ram.file.write_all_at(&header, 0).map_err(io)?;
        // On to the disk, in the background: then the pages are clean, for
        // the kernel to reclaim as it needs the memory.
        // SAFETY: a plain call on an open file.
        unsafe { libc::sync_file_range(ram.file.as_raw_fd(), 0, 0, libc::SYNC_FILE_RANGE_WRITE) };
        Ok(())
    }

    /// The machine's state [`write_state`] wrote into `file`, checked.
    pub(crate) fn read_state(file: &File) -> Result<Vec<u8>> {
        let damaged = |why: &str| Error::Snapshot(format!("the suspended machine's file {why}"));
        let mut header = [0u8; HEADER_LEN];
        file.read_exact_at(&mut header, 0).map_err(|_| damaged("has no header"))?;
        if header[..8] != super::FILE_MAGIC {
            return Err(damaged("does not start as one does"));
        }
        let u32_at = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
        if u32_at(8) != FILE_VERSION {
            return Err(damaged(&format!(
                "is of version {}, this libmachine's {FILE_VERSION}",
                u32_at(8)
            )));
        }
        let (at, len) = (u64_at(16), u64_at(24));
        let len = usize::try_from(len).map_err(|_| damaged("has a damaged header"))?;
        let mut state = vec![0u8; len];
        file.read_exact_at(&mut state, at).map_err(|_| damaged("ends inside its state"))?;
        if <sha2::Sha256 as sha2::Digest>::digest(&state)[..] != header[32..64] {
            return Err(damaged("holds a state whose checksum does not match"));
        }
        Ok(state)
    }

    /// Writes into `file` at `offset` the pages of `ram` that are this
    /// process's own (anonymous: written by the guest since `ram` was
    /// mapped, or never a file's), as /proc/self/pagemap tells; the others
    /// are the file's already, or were never touched. Pages of zeroes stay
    /// holes in a `fresh` file, and are written as zeroes over what an
    /// older file holds.
    fn write_changed(file: &File, ram: &[u8], offset: u64, fresh: bool) -> std::io::Result<()> {
        let pagemap = File::open("/proc/self/pagemap")?;
        let pages = ram.len().div_ceil(PAGE);
        let first = ram.as_ptr() as usize / PAGE;
        let mut entries = vec![0u64; PAGEMAP_CHUNK.min(pages)];
        let mut bytes = vec![0u8; entries.len() * 8];
        // A run of pages to write, [start, i).
        let mut run: Option<usize> = None;
        let flush = |start: usize, end: usize| -> std::io::Result<()> {
            let data = &ram[start * PAGE..(end * PAGE).min(ram.len())];
            file.write_all_at(data, offset + (start * PAGE) as u64)
        };
        for chunk in (0..pages).step_by(PAGEMAP_CHUNK) {
            let n = PAGEMAP_CHUNK.min(pages - chunk);
            pagemap.read_exact_at(&mut bytes[..n * 8], ((first + chunk) * 8) as u64)?;
            for (e, b) in entries.iter_mut().zip(bytes.as_chunks::<8>().0).take(n) {
                *e = u64::from_ne_bytes(*b);
            }
            for (j, &e) in entries[..n].iter().enumerate() {
                let i = chunk + j;
                let own = e & (PM_PRESENT | PM_SWAPPED) != 0 && e & PM_FILE == 0;
                let write =
                    own && !(fresh && zero(&ram[i * PAGE..((i + 1) * PAGE).min(ram.len())]));
                match (write, run) {
                    (true, None) => run = Some(i),
                    (false, Some(start)) => {
                        flush(start, i)?;
                        run = None;
                    }
                    _ => {}
                }
            }
        }
        if let Some(start) = run {
            flush(start, pages)?;
        }
        Ok(())
    }

    fn zero(page: &[u8]) -> bool {
        // SAFETY: any bit pattern is a valid u64; align_to only
        // reinterprets the aligned middle.
        let (head, words, tail) = unsafe { page.align_to::<u64>() };
        head.iter().all(|&b| b == 0)
            && words.iter().all(|&w| w == 0)
            && tail.iter().all(|&b| b == 0)
    }

    /// Copies what `from` holds in `[from_offset, from_offset + len)` to
    /// `to` at `to_offset`: its data only, its holes left holes.
    fn copy_data(
        from: &File,
        from_offset: u64,
        to: &File,
        to_offset: u64,
        len: u64,
    ) -> std::io::Result<()> {
        let end = from_offset + len;
        let mut at = from_offset;
        while at < end {
            let Some(data) = seek(from, at, libc::SEEK_DATA)?.filter(|&d| d < end) else {
                break;
            };
            let hole = seek(from, data, libc::SEEK_HOLE)?.unwrap_or(end).min(end);
            copy_range(from, data, to, to_offset + (data - from_offset), hole - data)?;
            at = hole;
        }
        Ok(())
    }

    /// lseek with SEEK_DATA or SEEK_HOLE from `at`; None past the data.
    fn seek(file: &File, at: u64, whence: i32) -> std::io::Result<Option<u64>> {
        let at = libc::off_t::try_from(at).map_err(std::io::Error::other)?;
        // SAFETY: a plain call on an open file.
        let r = unsafe { libc::lseek(file.as_raw_fd(), at, whence) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::ENXIO) => Ok(None),
                _ => Err(e),
            };
        }
        Ok(Some(r as u64))
    }

    /// Copies `len` bytes in the kernel (copy_file_range: the filesystem
    /// may share the blocks), else through a buffer.
    fn copy_range(
        from: &File,
        mut src: u64,
        to: &File,
        mut dst: u64,
        mut len: u64,
    ) -> std::io::Result<()> {
        while len > 0 {
            let (mut s, mut d) = (src as libc::loff_t, dst as libc::loff_t);
            // SAFETY: both files are open; the offsets are the kernel's to
            // move.
            let n = unsafe {
                libc::copy_file_range(
                    from.as_raw_fd(),
                    &mut s,
                    to.as_raw_fd(),
                    &mut d,
                    len as usize,
                    0,
                )
            };
            if n <= 0 {
                if n == 0 {
                    break;
                }
                let e = std::io::Error::last_os_error();
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EXDEV | libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL)
                ) {
                    return copy_buffered(from, src, to, dst, len);
                }
                return Err(e);
            }
            src += n as u64;
            dst += n as u64;
            len -= n as u64;
        }
        Ok(())
    }

    fn copy_buffered(
        from: &File,
        mut src: u64,
        to: &File,
        mut dst: u64,
        mut len: u64,
    ) -> std::io::Result<()> {
        let mut buf = vec![0u8; 1 << 20];
        while len > 0 {
            let n = buf.len().min(len as usize);
            from.read_exact_at(&mut buf[..n], src)?;
            to.write_all_at(&buf[..n], dst)?;
            (src, dst, len) = (src + n as u64, dst + n as u64, len - n as u64);
        }
        Ok(())
    }

    /// A new file in `dir`, already unlinked: only its mappings keep it.
    fn unnamed_file(dir: &Path) -> std::io::Result<File> {
        let mut nonce = [0u8; 8];
        crate::devices::virtio::rng::getrandom(&mut nonce);
        let path = dir.join(format!(".libmachine-suspended.{:016x}", u64::from_le_bytes(nonce)));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        std::fs::remove_file(&path)?;
        Ok(file)
    }

    fn on_memory_fs(file: &File) -> std::io::Result<bool> {
        // SAFETY: an all-zero statfs is valid, and fstatfs fills it.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: as above, on an open file.
        if unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast)]
        let magic = fs.f_type as i64;
        Ok(magic == TMPFS_MAGIC || magic == RAMFS_MAGIC)
    }

    /// Replaces the mapping at `host` with a private mapping of `file`
    /// from `offset`, in one step (what a restore's mapping of the file
    /// gives, for tests): the file is mapped elsewhere, then moved
    /// over the old mapping (mremap unmaps what it replaces), so that the
    /// address is never left unmapped.
    ///
    /// # Safety
    /// `host..host + size` must be one whole mapping that nothing uses
    /// while it is replaced, and the file must hold the same bytes.
    #[cfg(test)]
    unsafe fn remap(host: *mut u8, size: usize, file: &File, offset: u64) -> std::io::Result<()> {
        let offset = libc::off_t::try_from(offset).map_err(std::io::Error::other)?;
        // SAFETY: a new mapping, at an address the kernel picks.
        let new = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                file.as_raw_fd(),
                offset,
            )
        };
        if new == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: moves the mapping just made over `host`, which the caller
        // vouches for.
        let moved = unsafe {
            libc::mremap(
                new,
                size,
                size,
                libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED,
                host.cast::<libc::c_void>(),
            )
        };
        if moved == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            // SAFETY: the mapping made above, still where it was.
            unsafe { libc::munmap(new, size) };
            return Err(e);
        }
        debug_assert_eq!(moved.cast::<u8>(), host);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::memory::{MemoryRegion, RegionKind};

        const MIB: usize = 1 << 20;

        fn rss_pages() -> usize {
            let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
            statm.split_whitespace().nth(1).unwrap().parse().unwrap()
        }

        /// target/ is on a disk (/tmp may be tmpfs).
        fn disk() -> std::path::PathBuf {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
        }

        fn one_region(size: usize) -> (GuestMemoryMmap, MemoryLayout, &'static mut [u8]) {
            let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), size)]).unwrap();
            let layout =
                MemoryLayout { regions: vec![MemoryRegion::new(0, size as u64, RegionKind::Ram)] };
            let host = mem.get_host_address(GuestAddress(0)).unwrap();
            // SAFETY: the region is mapped while `mem` lives (the test
            // keeps it past every use); only this test uses it.
            let ram = unsafe { std::slice::from_raw_parts_mut(host, size) };
            (mem, layout, ram)
        }

        /// [`write_to_file`], then the RAM mapped from the file, as a
        /// restore maps it.
        fn move_to_file(
            mem: &GuestMemoryMmap,
            layout: &MemoryLayout,
            dir: &Path,
            backing: &mut Option<RamFile>,
        ) -> Result<()> {
            write_to_file(mem, layout, dir, backing)?;
            let b = backing.as_ref().unwrap();
            for ((r, _), &offset) in ram_offsets(layout, 0).iter().zip(&b.offsets) {
                let host = mem.get_host_address(GuestAddress(r.start)).unwrap();
                // SAFETY: the region is the whole of its mapping, unused
                // meanwhile, and the file holds its bytes.
                unsafe { remap(host, r.size as usize, &b.file, offset) }.unwrap();
            }
            Ok(())
        }

        fn page_value(i: usize, round: usize) -> (u8, u8) {
            ((i % 251) as u8 + 1, ((i + round) % 241) as u8)
        }

        /// The bytes stay, the pages leave the process; later suspensions
        /// write into the same file only what changed (a page zeroed
        /// included), and writes after a move stay private to the mapping.
        #[test]
        fn ram_moves_to_a_file_and_out_of_memory() {
            let (mem, layout, ram) = one_region(64 * MIB);
            // Every page of the first 48 MiB written, the rest left zero.
            for (i, page) in ram[..48 * MIB].chunks_mut(PAGE).enumerate() {
                (page[0], page[PAGE - 1]) = page_value(i, 0);
            }
            let before = rss_pages();
            let mut backing = None;
            move_to_file(&mem, &layout, &disk(), &mut backing).unwrap();
            let after = rss_pages();
            assert!(
                before - after >= 40 * MIB / PAGE,
                "{before} pages resident before, {after} after"
            );
            let file = backing.as_ref().unwrap().file.clone();
            assert!(backing.as_ref().unwrap().ours);
            for (i, page) in ram[..48 * MIB].chunks(PAGE).enumerate() {
                assert_eq!((page[0], page[PAGE - 1]), page_value(i, 0));
            }
            assert!(ram[48 * MIB..].iter().all(|&b| b == 0));
            // Holes for the zeroes, data for the rest.
            let used = file.metadata().unwrap();
            assert!(std::os::unix::fs::MetadataExt::blocks(&used) * 512 < 56 * MIB as u64);

            // Change a few pages, zero one, write one in the zero part.
            for i in [0, 7, 1000] {
                ram[i * PAGE + PAGE - 1] = page_value(i, 1).1;
            }
            ram[5 * PAGE] = 0;
            ram[5 * PAGE + PAGE - 1] = 0;
            ram[60 * MIB] = 9;
            move_to_file(&mem, &layout, &disk(), &mut backing).unwrap();
            assert!(Arc::ptr_eq(&backing.as_ref().unwrap().file, &file), "the same file");
            for (i, page) in ram[..48 * MIB].chunks(PAGE).enumerate() {
                let want = match i {
                    0 | 7 | 1000 => page_value(i, 1),
                    5 => (0, 0),
                    _ => page_value(i, 0),
                };
                assert_eq!((page[0], page[PAGE - 1]), want, "page {i}");
            }
            assert_eq!(ram[60 * MIB], 9);
            // What the file holds: read it, not the mapping.
            let mut buf = [0u8; 1];
            file.read_exact_at(&mut buf, RAM_BASE + (5 * PAGE) as u64).unwrap();
            assert_eq!(buf, [0]);
            file.read_exact_at(&mut buf, RAM_BASE + (60 * MIB) as u64).unwrap();
            assert_eq!(buf, [9]);
            drop(mem);
        }

        /// RAM that is a snapshot's private mapping moves to a new file
        /// with the snapshot's data and the guest's changes, and the
        /// snapshot's file is never written.
        #[test]
        fn ram_from_a_snapshot_moves_to_a_file_of_its_own() {
            let size = 8 * MIB;
            let (_template_mem, _, template) = one_region(size);
            for (i, page) in template.chunks_mut(PAGE).enumerate().filter(|(i, _)| i % 3 == 0) {
                (page[0], page[PAGE - 1]) = page_value(i, 0);
            }
            // The snapshot: the template's RAM, from 2 MiB into a file.
            let snap = Arc::new(unnamed_file(&disk()).unwrap());
            snap.set_len(2 * MIB as u64 + size as u64).unwrap();
            for (i, page) in template.chunks(PAGE).enumerate().filter(|(i, _)| i % 3 == 0) {
                snap.write_all_at(page, (2 * MIB + i * PAGE) as u64).unwrap();
            }
            let (mem, layout, ram) = one_region(size);
            // SAFETY: the region is the whole of the mapping, unused meanwhile.
            unsafe { remap(ram.as_mut_ptr(), size, &snap, 2 * MIB as u64) }.unwrap();
            ram[PAGE + 1] = 0x55;
            let mut backing =
                Some(RamFile { file: snap.clone(), offsets: vec![2 * MIB as u64], ours: false });
            move_to_file(&mem, &layout, &disk(), &mut backing).unwrap();
            let moved = backing.unwrap();
            assert!(moved.ours && !Arc::ptr_eq(&moved.file, &snap));
            for (i, page) in ram.chunks(PAGE).enumerate() {
                let want = if i % 3 == 0 { page_value(i, 0) } else { (0, 0) };
                assert_eq!((page[0], page[PAGE - 1]), want, "page {i}");
            }
            assert_eq!(ram[PAGE + 1], 0x55);
            let mut buf = [0u8; 1];
            snap.read_exact_at(&mut buf, (2 * MIB + PAGE + 1) as u64).unwrap();
            assert_eq!(buf, [0], "the snapshot's file is unchanged");
            drop(mem);
        }

        /// The state goes after the RAM, with a header that says where;
        /// a later, smaller state replaces it whole (the file shrinks); a
        /// damaged one, or a file that is not one, is refused.
        #[test]
        fn the_state_goes_after_the_ram() {
            let (mem, layout, ram) = one_region(4 * MIB);
            ram[0] = 1;
            let mut backing = None;
            write_to_file(&mem, &layout, &disk(), &mut backing).unwrap();
            let b = backing.unwrap();
            let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
            write_state(&b, &layout, &big, LACKS_NETWORK | LACKS_VSOCK).unwrap();
            assert_eq!(read_state(&b.file).unwrap(), big);
            let len = b.file.metadata().unwrap().len();
            assert_eq!(len, RAM_BASE + 4 * MIB as u64 + big.len() as u64);
            write_state(&b, &layout, b"small", LACKS_VSOCK).unwrap();
            assert_eq!(read_state(&b.file).unwrap(), b"small");
            assert_eq!(b.file.metadata().unwrap().len(), RAM_BASE + 4 * MIB as u64 + 5);
            // The RAM is where it was.
            let mut buf = [0u8; 1];
            b.file.read_exact_at(&mut buf, RAM_BASE).unwrap();
            assert_eq!(buf, [1]);
            // A flipped byte in the state, then in the header's magic.
            b.file.write_all_at(b"S", RAM_BASE + 4 * MIB as u64).unwrap();
            let err = read_state(&b.file).unwrap_err();
            assert!(err.to_string().contains("checksum"), "{err}");
            b.file.write_all_at(b"x", 0).unwrap();
            let err = read_state(&b.file).unwrap_err();
            assert!(err.to_string().contains("does not start"), "{err}");
            drop(mem);
        }

        /// A suspended machine's file is not a template: opening it as a
        /// snapshot fails, saying what it lacks.
        #[test]
        fn a_suspended_machines_file_is_no_snapshot() {
            let (mem, layout, _) = one_region(MIB);
            let mut backing = None;
            write_to_file(&mem, &layout, &disk(), &mut backing).unwrap();
            let b = backing.unwrap();
            write_state(&b, &layout, b"state", LACKS_NETWORK | LACKS_VSOCK).unwrap();
            // The file is unnamed; its copy under a name is the same bytes.
            let path = disk().join(format!("suspended-{}.file", std::process::id()));
            let mut all = vec![0u8; b.file.metadata().unwrap().len() as usize];
            b.file.read_exact_at(&mut all, 0).unwrap();
            std::fs::write(&path, &all).unwrap();
            let err = crate::Snapshot::open(&path).unwrap_err();
            std::fs::remove_file(&path).unwrap();
            let msg = err.to_string();
            assert!(
                msg.contains("suspended machine")
                    && msg.contains("host sockets")
                    && msg.contains("vsock"),
                "{msg}"
            );
            drop(mem);
        }

        #[test]
        fn memory_filesystems_are_refused() {
            let shm = Path::new("/dev/shm");
            if !shm.is_dir() {
                return;
            }
            let (mem, layout, _) = one_region(MIB);
            let err = move_to_file(&mem, &layout, shm, &mut None).unwrap_err();
            assert!(matches!(&err, Error::InvalidConfig(m) if m.contains("tmpfs")), "{err}");
            drop(mem);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod watch {
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
    use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

    use super::Doorbell;
    use crate::Vsock;
    use crate::error::{Error, Result};
    use crate::net::stack::WakeOn;

    const STOP: u64 = 0;
    const VSOCK: u64 = 1;
    /// Held stream `i` has the token `STREAMS + i`.
    const STREAMS: u64 = 2;

    fn io_err(op: &'static str) -> impl FnOnce(std::io::Error) -> Error {
        move |source| Error::Hypervisor { op, source }
    }

    /// The thread that waits, while a machine is suspended and nothing of
    /// it runs, for what comes for its guest, and rings its doorbell once
    /// it does (see the module's documentation): data, a close or room on
    /// a host socket of the guest's connections, as the network stack said
    /// to watch each for, or a packet the vsock's host end owes the guest.
    /// It only watches: it has the sockets' file descriptors, never the
    /// sockets, and reads nothing from them. Dropping it stops it.
    pub(crate) struct Watcher {
        stop: EventFd,
        handle: Option<JoinHandle<()>>,
    }

    impl Watcher {
        /// Starts watching `streams` (file descriptors that stay open while
        /// the watcher lives) and `vsock`'s host end, detached from any
        /// device: its wakeups come here from now on.
        pub(crate) fn spawn(
            doorbell: Arc<Doorbell>,
            vsock: Option<Vsock>,
            streams: &[(RawFd, WakeOn)],
        ) -> Result<Self> {
            let epoll = Epoll::new().map_err(io_err("create the watcher's epoll"))?;
            let stop =
                EventFd::new(EFD_NONBLOCK).map_err(io_err("create the watcher's stop eventfd"))?;
            epoll
                .ctl(ControlOperation::Add, stop.as_raw_fd(), EpollEvent::new(EventSet::IN, STOP))
                .map_err(io_err("watch the watcher's stop eventfd"))?;
            for (i, &(fd, wake_on)) in streams.iter().enumerate() {
                let mut events = EventSet::empty();
                if wake_on.data {
                    events |= EventSet::IN | EventSet::READ_HANG_UP;
                }
                if wake_on.room {
                    events |= EventSet::OUT;
                }
                // Level-triggered: what is there already counts. Errors and
                // hang-ups are reported whatever the events asked.
                epoll
                    .ctl(ControlOperation::Add, fd, EpollEvent::new(events, STREAMS + i as u64))
                    .map_err(io_err("watch a held host socket"))?;
            }
            let vsock = match vsock {
                Some(vsock) => {
                    let wake = EventFd::new(EFD_NONBLOCK)
                        .map_err(io_err("create the watcher's vsock eventfd"))?;
                    epoll
                        .ctl(
                            ControlOperation::Add,
                            wake.as_raw_fd(),
                            EpollEvent::new(EventSet::IN, VSOCK),
                        )
                        .map_err(io_err("watch the vsock's host end"))?;
                    let theirs = wake
                        .try_clone()
                        .map_err(io_err("duplicate the watcher's vsock eventfd"))?;
                    vsock.watch(theirs);
                    Some((vsock, wake))
                }
                None => None,
            };
            let handle = std::thread::Builder::new()
                .name("libmachine-watch".into())
                .stack_size(64 << 10)
                .spawn(move || watch(&epoll, vsock.as_ref(), &doorbell))
                .map_err(io_err("start the watcher"))?;
            Ok(Self { stop, handle: Some(handle) })
        }
    }

    fn watch(epoll: &Epoll, vsock: Option<&(Vsock, EventFd)>, doorbell: &Doorbell) {
        let mut events = vec![EpollEvent::default(); 16];
        loop {
            let n = match epoll.wait(-1, &mut events) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // Nothing can be watched: let the guest run rather than
                // sleep for good.
                Err(_) => {
                    doorbell.ring();
                    return;
                }
            };
            for event in &events[..n] {
                match event.data() {
                    STOP => return,
                    VSOCK => {
                        let Some((vsock, wake)) = vsock else { continue };
                        let _ = wake.read();
                        // Host activity the guest has nothing to take from
                        // (a read that owes no credit update) is no reason.
                        if vsock.owes_guest() {
                            doorbell.ring();
                            return;
                        }
                    }
                    _ => {
                        doorbell.ring();
                        return;
                    }
                }
            }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            let _ = self.stop.write(1);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::io::Write;
        use std::net::{Ipv4Addr, TcpListener, TcpStream};
        use std::time::Duration;

        use super::*;
        use crate::suspend::Woken;

        fn pair() -> (TcpStream, TcpStream) {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (b, _) = listener.accept().unwrap();
            (a, b)
        }

        fn armed() -> Arc<Doorbell> {
            let bell = Arc::new(Doorbell::default());
            bell.arm();
            bell
        }

        const DATA: WakeOn = WakeOn { data: true, room: false };
        const ROOM: WakeOn = WakeOn { data: false, room: true };

        /// Data on a socket watched for data rings; room on a socket not
        /// watched for room does not (a connected socket always has room),
        /// nor does data on one watched for nothing; and the watcher never
        /// reads what it rang for.
        #[test]
        fn data_rings_and_is_left_unread() {
            let (mut far, near) = pair();
            let (mut far2, near2) = pair();
            let bell = armed();
            let _w = Watcher::spawn(
                bell.clone(),
                None,
                &[(near.as_raw_fd(), DATA), (near2.as_raw_fd(), WakeOn::default())],
            )
            .unwrap();
            far2.write_all(b"ignored").unwrap();
            assert_eq!(bell.wait(|| false, Some(Duration::from_millis(100))), Woken::TimedOut);
            far.write_all(b"hello").unwrap();
            assert_eq!(bell.wait(|| false, Some(Duration::from_secs(10))), Woken::Device);
            let mut buf = [0u8; 16];
            near.set_nonblocking(true).unwrap();
            assert_eq!(std::io::Read::read(&mut &near, &mut buf).unwrap(), 5);
        }

        /// Data already there, a close, and room on a socket watched for
        /// room each ring at once.
        #[test]
        fn what_is_there_already_rings() {
            for case in 0..3 {
                let (mut far, near) = pair();
                let wake_on = match case {
                    0 => {
                        far.write_all(b"x").unwrap();
                        std::thread::sleep(Duration::from_millis(20));
                        DATA
                    }
                    1 => {
                        drop(far);
                        DATA
                    }
                    _ => ROOM,
                };
                let bell = armed();
                let _w =
                    Watcher::spawn(bell.clone(), None, &[(near.as_raw_fd(), wake_on)]).unwrap();
                assert_eq!(
                    bell.wait(|| false, Some(Duration::from_secs(10))),
                    Woken::Device,
                    "case {case}"
                );
            }
        }

        /// A detached vsock host end rings only when it owes the guest a
        /// packet; dropping the watcher ends its thread.
        #[test]
        fn the_vsock_rings_when_it_owes_the_guest() {
            use crate::devices::virtio::vsock::{GUEST_CID, VsockDevice};
            let (device, vsock) = VsockDevice::new(GUEST_CID).unwrap();
            // The guest's driver had started the device.
            vsock.shared().lock().activate();
            vsock.detach();
            let bell = armed();
            let w = Watcher::spawn(bell.clone(), Some(vsock.clone()), &[]).unwrap();
            assert_eq!(bell.wait(|| false, Some(Duration::from_millis(50))), Woken::TimedOut);
            // A connect to the guest owes it a request.
            let v = vsock.clone();
            let connect = std::thread::spawn(move || v.connect(1024, Duration::from_millis(300)));
            assert_eq!(bell.wait(|| false, Some(Duration::from_secs(10))), Woken::Device);
            drop(w);
            let _ = connect.join();
            drop(device);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn rings_count_only_while_armed() {
        let bell = Doorbell::default();
        let never = || false;
        bell.ring();
        bell.arm();
        assert_eq!(bell.wait(never, Some(Duration::from_millis(10))), Woken::TimedOut);
        bell.ring();
        assert_eq!(bell.wait(never, Some(Duration::ZERO)), Woken::Device);
        bell.arm();
        assert_eq!(bell.wait(never, Some(Duration::ZERO)), Woken::TimedOut, "forgotten");
        bell.disarm();
        bell.ring();
        bell.arm();
        assert_eq!(bell.wait(never, Some(Duration::ZERO)), Woken::TimedOut);
    }

    #[test]
    fn a_ring_or_a_stop_from_another_thread_ends_the_wait() {
        let bell = Arc::new(Doorbell::default());
        bell.arm();
        let b = bell.clone();
        let ringer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            b.ring();
        });
        assert_eq!(bell.wait(|| false, Some(Duration::from_secs(10))), Woken::Device);
        ringer.join().unwrap();

        bell.arm();
        let stop = Arc::new(AtomicBool::new(false));
        let (b, s) = (bell.clone(), stop.clone());
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s.store(true, Ordering::SeqCst);
            b.stop_requested();
        });
        let take = || stop.swap(false, Ordering::SeqCst);
        assert_eq!(bell.wait(take, Some(Duration::from_secs(10))), Woken::Stop);
        stopper.join().unwrap();
        assert!(!stop.load(Ordering::SeqCst), "the request was used up");
    }
}
