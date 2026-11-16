// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: reads and writes of the host's files bypassing its page cache (TASK-433): `O_DIRECT` on
//! Linux, `F_NOCACHE` on macOS, where the file system allows. cladefs keeps what it caches in memory
//! of its own, budgeted (its extent cache, the fetchers' parts, held parts); the host's page cache
//! is a second memory, unbudgeted, holding the same bytes again, which the files it writes and
//! reads keep out of.
//!
//! Direct I/O on Linux moves whole blocks, from memory aligned to them: what is read here is read
//! by the blocks around it, into a buffer aligned so, and what is written that doesn't start or
//! end at a block's edge is merged into the blocks it touches, read first, unless the caller holds
//! the block that ends what it wrote before ([`Tail`]), as a log appended to does. A file that
//! can't bypass the cache (a file system without `O_DIRECT`, such as eCryptfs) is not read or
//! written at all: [`bypass`] fails, and so does the opening of what it is for, a store's file or
//! the part cache, rather than run in the host's page cache, a second memory no budget counts
//! (TASK-433.6). Its setup is to be fixed: a file system that bypasses it.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

/// The block that direct I/O moves, and aligns memory to: the largest a host's disks have.
pub const BLOCK: u64 = 4096;

/// Makes I/O on `file`, at `path`, bypass the host's page cache, or fails, naming `path`, if its
/// file system refuses (`InvalidInput`): nothing is read or written through the host's cache. The
/// `direct::bypass` failpoint refuses as such a file system does, for tests.
pub fn bypass(file: &File, path: &Path) -> io::Result<()> {
    let refused = |err: io::Error| {
        let why = format!(
            "{}: its file system can't bypass the host's page cache (O_DIRECT, F_NOCACHE): {err}",
            path.display()
        );
        io::Error::new(io::ErrorKind::InvalidInput, why)
    };
    fail::fail_point!("direct::bypass", |_| Err(refused(io::Error::from_raw_os_error(
        libc::EINVAL
    ))));
    let fd = file.as_raw_fd();
    #[cfg(target_os = "linux")]
    // SAFETY: Reads and sets the status flags of an open file descriptor.
    let set = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_DIRECT) == 0
    };
    #[cfg(target_os = "macos")]
    // SAFETY: Sets an option of an open file descriptor.
    let set = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) == 0 };
    match set {
        true => Ok(()),
        false => Err(refused(io::Error::last_os_error())),
    }
}

/// Memory aligned to [`BLOCK`], zeroed as it is allocated, a whole number of blocks long.
struct Aligned {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: It owns its memory, which nothing else points to.
unsafe impl Send for Aligned {}

impl Aligned {
    /// `blocks` blocks, zeroed.
    fn zeroed(blocks: u64) -> Self {
        let len = (blocks.max(1) * BLOCK) as usize;
        let layout = Layout::from_size_align(len, BLOCK as usize).expect("a block-aligned layout");
        // SAFETY: The layout is of a non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, len }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, BLOCK as usize).expect("its layout");
        // SAFETY: Allocated with this layout by `zeroed`.
        unsafe { dealloc(self.ptr, layout) };
    }
}

/// The most a thread keeps of a [`Buffer`] it let go, for its next: a write-back's run, the page
/// cache's `CHUNK`. More is allocated for one that needs it, and freed with it, so that what a
/// thread keeps, outside every budget, stays this small.
const SPARE_MOST: usize = 1 << 20;

thread_local! {
    /// The memory of the last [`Buffer`] the thread let go, if no more than [`SPARE_MOST`].
    static SPARE: std::cell::Cell<Option<Aligned>> = const { std::cell::Cell::new(None) };
}

/// cladefs: `len` bytes aligned to [`BLOCK`], what direct I/O reads into and writes from as they
/// are: the thread's spare memory if it is large enough, which held what the last held, or memory
/// allocated zeroed. Each write-back, read and write here used to allocate a zeroed buffer of its
/// own: on Linux a MiB of fresh pages each, faulted in, zeroed and freed again, a quarter of the
/// time of a store's writes (TASK-433.8).
pub struct Buffer {
    aligned: Option<Aligned>,
    len: usize,
}

impl Buffer {
    /// `len` bytes, aligned to [`BLOCK`]: what they hold is unspecified, zeros or what the
    /// thread's last buffer held.
    pub fn new(len: usize) -> Self {
        let blocks = (len as u64).div_ceil(BLOCK).max(1);
        let aligned = match SPARE.take() {
            Some(spare) if spare.len as u64 >= blocks * BLOCK => spare,
            spare => {
                SPARE.set(spare);
                Aligned::zeroed(blocks)
            }
        };
        Self { aligned: Some(aligned), len }
    }

    /// Its bytes.
    pub fn bytes(&self) -> &[u8] {
        let aligned = self.aligned.as_ref().expect("held until dropped");
        // SAFETY: It owns `aligned.len` bytes at `ptr`, initialized, `len` or more.
        unsafe { std::slice::from_raw_parts(aligned.ptr, self.len) }
    }

    /// Its bytes, to fill.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        let aligned = self.aligned.as_ref().expect("held until dropped");
        // SAFETY: It owns `aligned.len` bytes at `ptr`, initialized, `len` or more, and is
        // borrowed mutably.
        unsafe { std::slice::from_raw_parts_mut(aligned.ptr, self.len) }
    }
}

/// Its memory is the thread's spare, if it is no more than [`SPARE_MOST`] and larger than the
/// spare there.
impl Drop for Buffer {
    fn drop(&mut self) {
        let Some(aligned) = self.aligned.take() else { return };
        if aligned.len > SPARE_MOST {
            return;
        }
        let kept = match SPARE.take() {
            Some(kept) if kept.len >= aligned.len => kept,
            _ => aligned,
        };
        SPARE.set(Some(kept));
    }
}

/// Whether `bytes` at `offset` are whole blocks, from memory aligned to them: what direct I/O
/// moves as it is.
fn aligned(bytes: &[u8], offset: u64) -> bool {
    offset.is_multiple_of(BLOCK)
        && (bytes.len() as u64).is_multiple_of(BLOCK)
        && (bytes.as_ptr() as usize).is_multiple_of(BLOCK as usize)
}

/// The blocks around `range`: from the block it starts in to the end of the block it ends in.
fn blocks(range: &Range<u64>) -> Range<u64> {
    let start = range.start / BLOCK * BLOCK;
    start..range.end.div_ceil(BLOCK) * BLOCK
}

/// Reads all of `buf` at `offset` in `file`, zeros past its end.
fn read_fully(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match file.read_at(buf, offset) {
            Ok(0) => {
                buf.fill(0);
                return Ok(());
            }
            Ok(read) => {
                buf = &mut buf[read..];
                offset += read as u64;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Reads `buf.len()` bytes at `offset` in `file`, which bypasses the page cache ([`bypass`]): by
/// the blocks around them, into memory aligned to them. Bytes past the file's end read as zeros.
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let range = offset..offset + buf.len() as u64;
    if buf.is_empty() || aligned(buf, offset) {
        return read_fully(file, buf, offset);
    }
    let around = blocks(&range);
    let mut bounce = Buffer::new((around.end - around.start) as usize);
    read_fully(file, bounce.bytes_mut(), around.start)?;
    let from = (offset - around.start) as usize;
    buf.copy_from_slice(&bounce.bytes()[from..from + buf.len()]);
    Ok(())
}

/// The block a writer holds that ends what it wrote to a file before, which its next write merges
/// into rather than read from the file: a log's last block, partly filled, appended to again and
/// again, which would be read back from the disk for each append otherwise.
#[derive(Default)]
pub struct Tail {
    held: Option<(u64, Box<[u8]>)>,
}

impl Tail {
    /// The block at `at`, if held.
    fn block(&self, at: u64) -> Option<&[u8]> {
        self.held.as_ref().filter(|(held, _)| *held == at).map(|(_, block)| &**block)
    }
}

/// Writes `bytes` at `offset` in `file`, which bypasses the page cache ([`bypass`]): whole blocks,
/// from memory aligned to them, what `bytes` doesn't cover of the first and last block merged in,
/// read from the file unless `tail` holds the last it wrote. The file's size is as a plain write's
/// would leave it: the blocks written past the end of what was written are cut off. `tail` holds,
/// after, the block that ends what this wrote, if it ends partway through one. Whole blocks from
/// memory aligned to them ([`Buffer`]) are written as they are: no copy, nothing merged.
pub fn write_at(file: &File, tail: &mut Tail, bytes: &[u8], offset: u64) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if aligned(bytes, offset) {
        file.write_all_at(bytes, offset)?;
        tail.held = None;
        return Ok(());
    }
    let range = offset..offset + bytes.len() as u64;
    let around = blocks(&range);
    let len = file.metadata()?.len();
    let mut buf = Buffer::new((around.end - around.start) as usize);
    // The edges' blocks, what this doesn't write of them: as held, or as on the disk; none past
    // the file's end, zeros there. One block alone is read once.
    let (first, last) = (around.start, around.end - BLOCK);
    if range.start > first {
        merge_from(file, tail, len, &mut buf, around.start, first)?;
    }
    if range.end < last + BLOCK && !(last == first && range.start > first) {
        merge_from(file, tail, len, &mut buf, around.start, last)?;
    }
    let from = (offset - around.start) as usize;
    buf.bytes_mut()[from..from + bytes.len()].copy_from_slice(bytes);
    file.write_all_at(buf.bytes(), around.start)?;
    // What a plain write would leave: the file as long as what it held, or what this wrote.
    let end = len.max(range.end);
    if around.end > end {
        file.set_len(end)?;
    }
    let last = around.end - BLOCK;
    tail.held = (range.end % BLOCK != 0).then(|| {
        let at = (last - around.start) as usize;
        (last, buf.bytes()[at..at + BLOCK as usize].into())
    });
    Ok(())
}

/// Fills the block at `at` of `buf`, which holds the blocks from `start` on, with what the file of
/// `len` bytes holds there: as `tail` holds it, or read; zeros past the file's end, as `buf` holds
/// what it held before ([`Buffer::new`]).
fn merge_from(
    file: &File,
    tail: &Tail,
    len: u64,
    buf: &mut Buffer,
    start: u64,
    at: u64,
) -> io::Result<()> {
    let place = (at - start) as usize..(at - start + BLOCK) as usize;
    if at >= len {
        buf.bytes_mut()[place].fill(0);
        return Ok(());
    }
    match tail.block(at) {
        Some(held) => buf.bytes_mut()[place].copy_from_slice(held),
        None => read_fully(file, &mut buf.bytes_mut()[place], at)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file in `dir`, bypassing the page cache.
    fn opened(dir: &Path) -> File {
        let path = dir.join("file");
        let options = std::fs::OpenOptions::new().read(true).write(true).create_new(true).clone();
        let file = options.open(&path).unwrap();
        bypass(&file, &path).unwrap();
        file
    }

    /// Writes at any offset and of any length, appended or over what was written, read back at
    /// any offset and length, are as plain I/O's would be, the file as long as what was written;
    /// appends merged into the block held read nothing back.
    #[test]
    fn unaligned_writes_and_reads_are_as_plain_ones() {
        let dir = tempfile::tempdir().unwrap();
        let file = opened(dir.path());
        let mut tail = Tail::default();
        let mut model = Vec::new();
        let mut write = |bytes: &[u8], offset: u64, model: &mut Vec<u8>| {
            write_at(&file, &mut tail, bytes, offset).unwrap();
            let end = offset as usize + bytes.len();
            if model.len() < end {
                model.resize(end, 0);
            }
            model[offset as usize..end].copy_from_slice(bytes);
        };
        let pattern = |seed: u8, len: usize| -> Vec<u8> {
            (0..len).map(|at| seed.wrapping_add((at % 251) as u8)).collect()
        };
        // Appends, a record at a time, across blocks.
        let mut at = 0;
        for (seed, len) in [(1, 100), (2, 3000), (3, 5000), (4, 1), (5, 8192), (6, 77)] {
            write(&pattern(seed, len), at, &mut model);
            at += len as u64;
        }
        // Over what was written, unaligned, within a block and across them.
        write(&pattern(7, 10), 50, &mut model);
        write(&pattern(8, 6000), 2000, &mut model);
        // Past the end, leaving a gap.
        write(&pattern(9, 300), 20_000, &mut model);
        assert_eq!(file.metadata().unwrap().len(), model.len() as u64);
        for (offset, len) in [(0, model.len()), (1, 4095), (4000, 200), (8191, 2), (19_999, 302)] {
            let mut read = vec![0; len];
            read_at(&file, &mut read, offset as u64).unwrap();
            let mut want = vec![0; len];
            let end = (offset + len).min(model.len());
            want[..end - offset].copy_from_slice(&model[offset..end]);
            assert_eq!(read, want, "{len} bytes at {offset}");
        }
    }

    /// TASK-433.8: a write past the file's end from a buffer the thread reused, which holds what
    /// the last held, leaves zeros in the gap, not those bytes; whole blocks from a buffer are
    /// written as they are, and read back so.
    #[test]
    fn reused_buffers_leave_none_of_what_they_held() {
        let dir = tempfile::tempdir().unwrap();
        let file = opened(dir.path());
        let mut tail = Tail::default();
        let mut stale = Buffer::new(4 * BLOCK as usize);
        stale.bytes_mut().fill(0xaa);
        drop(stale);
        write_at(&file, &mut tail, &[1; 10], BLOCK + 5).unwrap();
        let mut want = vec![0; BLOCK as usize + 15];
        want[BLOCK as usize + 5..].fill(1);
        let mut read = vec![0xbb; want.len()];
        read_at(&file, &mut read, 0).unwrap();
        assert_eq!(read, want);
        let mut whole = Buffer::new(2 * BLOCK as usize);
        whole.bytes_mut().fill(7);
        write_at(&file, &mut tail, whole.bytes(), 2 * BLOCK).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 4 * BLOCK);
        let mut read = vec![0; 2 * BLOCK as usize];
        read_at(&file, &mut read, 2 * BLOCK).unwrap();
        assert_eq!(read, vec![7; 2 * BLOCK as usize]);
    }

    /// A file system that refuses to bypass the page cache, as the failpoint makes every one, fails
    /// the bypass, naming the file, rather than leave the file to be read and written through the
    /// host's cache (TASK-433.6).
    #[test]
    fn a_refused_bypass_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let file = File::create(&path).unwrap();
        let scenario = fail::FailScenario::setup();
        fail::cfg("direct::bypass", "return").unwrap();
        let err = bypass(&file, &path).unwrap_err();
        scenario.teardown();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        let want = format!(
            "{}: its file system can't bypass the host's page cache (O_DIRECT, F_NOCACHE): \
             {}",
            path.display(),
            io::Error::from_raw_os_error(libc::EINVAL)
        );
        assert_eq!(err.to_string(), want);
    }
}
