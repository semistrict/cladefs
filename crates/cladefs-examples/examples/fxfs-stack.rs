// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! How much stack Fxfs's operations take when one thread polls them all, as a guest kernel's
//! threads would (TASK-26.1). Linux gives each kernel thread 16 KiB of stack on x86-64 and arm64,
//! some of which the system call and the VFS have used before a filesystem is called.
//!
//! Prints, for each phase of a workload, the most stack it used below the executor's frames, as a
//! Markdown table. Only optimized builds say anything about a kernel's, so it runs only in one:
//!
//! ```sh
//! cargo run --release -p cladefs-examples --example fxfs-stack
//! ```

use fuchsia_async::LocalExecutor;
use fxfs::filesystem::{FxFilesystem, SyncOptions};
use fxfs::fsck::fsck;
use fxfs::object_handle::{ObjectHandle, WriteObjectHandle};
use fxfs::object_store::directory::replace_child;
use fxfs::object_store::transaction::{LockKey, Options, lock_keys};
use fxfs::object_store::volume::root_volume;
use fxfs::object_store::{Directory, NewChildStoreOptions, ObjectStore};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

const KIB: usize = 1024;
const BLOCK_SIZE: u32 = 4096;
/// Directories, and files in each, the workload creates.
const DIRS: usize = 16;
const FILES: usize = 64;
const MAX_FRAMES: usize = 64;

/// Whether `--trace` was passed to record the deepest call chain and frame sizes per phase
/// (TASK-26.9).
static TRACE: AtomicBool = AtomicBool::new(false);

struct TrackingAlloc;

#[global_allocator]
static ALLOC: TrackingAlloc = TrackingAlloc;

// SAFETY: Delegates directly to `System` after optionally sampling the frame pointer chain.
unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACE.load(Ordering::Relaxed) {
            record_deepest_chain();
        }
        // SAFETY: Forwarded to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Forwarded to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if TRACE.load(Ordering::Relaxed) {
            record_deepest_chain();
        }
        // SAFETY: Forwarded to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACE.load(Ordering::Relaxed) {
            record_deepest_chain();
        }
        // SAFETY: Forwarded to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Captured call chain `(return_address, frame_bytes)` at the deepest stack point of a phase.
#[derive(Clone, Copy)]
struct Chain {
    len: usize,
    frames: [(usize, usize); MAX_FRAMES],
}

impl Chain {
    const EMPTY: Self = Self { len: 0, frames: [(0, 0); MAX_FRAMES] };
}

thread_local! {
    /// The lowest and highest addresses of the current thread's stack.
    static STACK_BOUNDS: (usize, usize) = stack_bounds();
    /// Where `paint` was called from, and the range below it that it painted.
    static PAINTED: Cell<(usize, usize, usize)> = const { Cell::new((0, 0, 0)) };
    /// Active phase's `(ceiling_fp, deepest_sp)` while `TRACE` is enabled.
    static TRACE_STATE: Cell<(usize, usize)> = const { Cell::new((0, usize::MAX)) };
    /// Re-entrancy guard while sampling inside `TrackingAlloc`.
    static IN_TRACE: Cell<bool> = const { Cell::new(false) };
    /// Deepest frame-pointer chain captured during the active phase.
    static DEEPEST_CHAIN: RefCell<Chain> = const { RefCell::new(Chain::EMPTY) };
}

#[inline(always)]
fn current_fp() -> usize {
    let fp: usize;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: Reading frame pointer register has no side effects.
    unsafe {
        core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: Reading frame pointer register has no side effects.
    unsafe {
        core::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        fp = 0;
    }
    fp
}

/// Samples the frame-pointer chain without allocating if `sp` is deeper than previously seen in
/// this phase.
fn record_deepest_chain() {
    let _ = IN_TRACE.try_with(|in_trace| {
        if in_trace.replace(true) {
            return;
        }
        let (ceiling, deepest) = TRACE_STATE.get();
        if ceiling != 0 {
            let fp = current_fp();
            if fp != 0 && fp < deepest && fp < ceiling {
                let (low, high) = STACK_BOUNDS.with(|b| *b);
                if fp > low && fp < high {
                    TRACE_STATE.set((ceiling, fp));
                    let mut chain = Chain::EMPTY;
                    let mut cur = fp;
                    while chain.len < MAX_FRAMES && cur > low && cur + 16 <= high && (cur & 7) == 0
                    {
                        // SAFETY: `cur` is 8-byte aligned within this thread's live stack.
                        let (next, lr) = unsafe {
                            let slot = cur as *const usize;
                            (*slot, *slot.add(1))
                        };
                        if next <= cur || next > high {
                            break;
                        }
                        let frame_bytes = next - cur;
                        chain.frames[chain.len] = (lr, frame_bytes);
                        chain.len += 1;
                        if next >= ceiling {
                            break;
                        }
                        cur = next;
                    }
                    DEEPEST_CHAIN.with_borrow_mut(|slot| *slot = chain);
                }
            }
        }
        in_trace.set(false);
    });
}

fn symbolize(addr: usize) -> String {
    // SAFETY: `dladdr` queries loaded symbols for `addr`.
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(addr as *const libc::c_void, &mut info) != 0 && !info.dli_sname.is_null() {
            let raw = CStr::from_ptr(info.dli_sname).to_string_lossy();
            let off = addr.saturating_sub(info.dli_saddr as usize);
            return format!("{raw}+0x{off:x}");
        }
    }
    format!("0x{addr:x}")
}

const PAINT: u8 = 0xa5;
/// What `paint` leaves unpainted below its frame, for the painting's own frames.
const MARGIN: usize = 1024;

/// Paints the current thread's stack below the caller's frame, so that `deepest` can tell how
/// deep it is used from here on.
#[inline(never)]
fn paint() {
    let (low, _) = STACK_BOUNDS.with(|bounds| *bounds);
    let here = std::hint::black_box(&low) as *const usize as usize;
    // From a page above the guard.
    let (start, end) = (low + 4 * KIB, here - MARGIN);
    // SAFETY: The range lies within this thread's stack, below every live frame.
    unsafe { std::ptr::write_bytes(start as *mut u8, PAINT, end - start) };
    PAINTED.set((here, start, end));
    if TRACE.load(Ordering::Relaxed) {
        DEEPEST_CHAIN.with_borrow_mut(|slot| *slot = Chain::EMPTY);
        TRACE_STATE.set((here, usize::MAX));
    }
}

/// How deep the current thread's stack has been used below where `paint` was called: to the
/// deepest byte that changed, or to the end of the margin if none did.
#[inline(never)]
fn deepest() -> (usize, Chain) {
    let chain = if TRACE.load(Ordering::Relaxed) {
        TRACE_STATE.set((0, usize::MAX));
        DEEPEST_CHAIN.with_borrow(|slot| *slot)
    } else {
        Chain::EMPTY
    };
    let (here, start, end) = PAINTED.get();
    // SAFETY: `paint` painted the range, below the frames of its caller, which is this one's.
    let painted = unsafe { std::slice::from_raw_parts(start as *const u8, end - start) };
    let untouched = painted.iter().position(|&byte| byte != PAINT).unwrap_or(painted.len());
    assert!(untouched > 0, "the stack overflowed its painted range");
    (here - (start + untouched), chain)
}

/// Polls `phase` to completion and records how much stack it took as `name`'s.
async fn measure<T>(uses: &mut Uses, name: &'static str, phase: impl Future<Output = T>) -> T {
    paint();
    let output = phase.await;
    let (used, chain) = deepest();
    uses.push((name, used, chain));
    output
}

/// The lowest and highest addresses of the current thread's stack.
#[cfg(target_os = "linux")]
fn stack_bounds() -> (usize, usize) {
    // SAFETY: Reads the current thread's attributes into storage this owns.
    unsafe {
        let mut attr = std::mem::zeroed();
        assert_eq!(libc::pthread_getattr_np(libc::pthread_self(), &mut attr), 0);
        let (mut low, mut size) = (std::ptr::null_mut(), 0);
        assert_eq!(libc::pthread_attr_getstack(&attr, &mut low, &mut size), 0);
        libc::pthread_attr_destroy(&mut attr);
        let low = low as usize;
        (low, low + size)
    }
}

/// The lowest and highest addresses of the current thread's stack.
#[cfg(target_os = "macos")]
fn stack_bounds() -> (usize, usize) {
    // SAFETY: Queries the current thread.
    unsafe {
        let high = libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize;
        let size = libc::pthread_get_stacksize_np(libc::pthread_self());
        (high - size, high)
    }
}

/// Stack used, by phase, with the deepest sampled call chain when `--trace` is passed.
type Uses = Vec<(&'static str, usize, Chain)>;

/// What a guest's filesystem does, phase by phase: formats, makes a volume, creates directories
/// and files, writes, looks up, renames and unlinks, compacts the journal, remounts, replays,
/// checks and reads.
async fn workload() -> Uses {
    let mut uses = Uses::new();
    let device = DeviceHolder::new(FakeDevice::new(16384, BLOCK_SIZE).calm());
    let fs = measure(&mut uses, "format", FxFilesystem::new_empty(device)).await.unwrap();
    populate(&mut uses, &fs).await;
    measure(&mut uses, "compact the journal", fs.journal().force_compact()).await.unwrap();
    measure(&mut uses, "sync", fs.sync(SyncOptions::default())).await.unwrap();
    measure(&mut uses, "close", fs.close()).await.unwrap();

    let device = fs.take_device().await;
    device.reopen(false);
    let fs = measure(&mut uses, "open and replay", FxFilesystem::open(device)).await.unwrap();
    measure(&mut uses, "fsck", fsck(fs.clone())).await.unwrap();
    check(&mut uses, &fs).await;
    fs.close().await.unwrap();
    uses
}

/// Makes a volume, and in it `DIRS` directories of `FILES` files of a block each, half of which
/// are renamed or unlinked.
async fn populate(uses: &mut Uses, fs: &Arc<FxFilesystem>) {
    let volume = root_volume(fs.clone()).await.unwrap();
    let store =
        measure(uses, "new volume", volume.new_volume("vol", NewChildStoreOptions::default()))
            .await
            .unwrap();
    let root = Directory::open(&store, store.root_directory_object_id()).await.unwrap();
    let lock = |id| LockKey::object(store.store_object_id(), id);
    let device = fs.device();
    let mut data = device.allocate_buffer(BLOCK_SIZE as usize).await;
    data.fill(7);
    for d in 0..DIRS {
        let dir = measure(uses, "mkdir", async {
            let mut transaction = store
                .new_transaction(lock_keys![lock(root.object_id())], Options::default())
                .await
                .unwrap();
            let dir = root.create_child_dir(&mut transaction, &format!("dir{d}")).await.unwrap();
            transaction.commit().await.unwrap();
            dir
        })
        .await;
        for f in 0..FILES {
            let file = measure(uses, "create", async {
                let mut transaction = store
                    .new_transaction(lock_keys![lock(dir.object_id())], Options::default())
                    .await
                    .unwrap();
                let file =
                    dir.create_child_file(&mut transaction, &format!("file{f}")).await.unwrap();
                transaction.commit().await.unwrap();
                file
            })
            .await;
            measure(uses, "write", file.write_or_append(Some(0), data.as_ref())).await.unwrap();
        }
        for f in (0..FILES).step_by(2) {
            let name = format!("file{f}");
            let (id, _, _) = measure(uses, "lookup", dir.lookup(&name)).await.unwrap().unwrap();
            let (phase, to) = if f % 4 == 0 { ("unlink", None) } else { ("rename", Some(&dir)) };
            measure(uses, phase, async {
                let mut transaction = store
                    .new_transaction(
                        lock_keys![lock(dir.object_id()), lock(id)],
                        Options::default(),
                    )
                    .await
                    .unwrap();
                match to {
                    None => replace_child(&mut transaction, None, (&dir, &name)).await.unwrap(),
                    Some(to) => {
                        let renamed = format!("renamed{f}");
                        replace_child(&mut transaction, Some((&dir, &name)), (to, &renamed))
                            .await
                            .unwrap()
                    }
                };
                transaction.commit().await.unwrap();
            })
            .await;
        }
    }
}

/// Opens the volume and reads back a renamed file.
async fn check(uses: &mut Uses, fs: &Arc<FxFilesystem>) {
    let volume = root_volume(fs.clone()).await.unwrap();
    let store =
        measure(uses, "open volume", volume.volume("vol", Default::default())).await.unwrap();
    let root = Directory::open(&store, store.root_directory_object_id()).await.unwrap();
    let (id, _, _) = root.lookup("dir3").await.unwrap().unwrap();
    let dir = Directory::open(&store, id).await.unwrap();
    let (id, _, _) = dir.lookup("renamed2").await.unwrap().unwrap();
    let file =
        measure(uses, "open file", ObjectStore::open_object(&store, id, Default::default(), None))
            .await
            .unwrap();
    let mut buf = file.allocate_buffer(BLOCK_SIZE as usize).await;
    assert_eq!(
        measure(uses, "read", file.read_aligned(0, buf.as_mut())).await.unwrap(),
        BLOCK_SIZE as usize
    );
    assert_eq!(buf.to_vec(), vec![7; BLOCK_SIZE as usize]);
}

fn main() {
    assert!(!cfg!(debug_assertions), "measure an optimized build: run with --release");
    let trace = std::env::args().any(|arg| arg == "--trace");
    TRACE.store(trace, Ordering::Relaxed);
    let uses = std::thread::Builder::new()
        .stack_size(1024 * KIB)
        .spawn(|| LocalExecutor::new().run_singlethreaded(Box::pin(workload())))
        .unwrap()
        .join()
        .unwrap();
    TRACE.store(false, Ordering::Relaxed);
    // The most each phase used, in the order they first ran.
    let mut most: Vec<(&str, usize, Chain)> = Vec::new();
    for (name, used, chain) in uses {
        match most.iter_mut().find(|(phase, _, _)| *phase == name) {
            Some((_, prev, prev_chain)) => {
                if used > *prev {
                    *prev = used;
                    *prev_chain = chain;
                }
            }
            None => most.push((name, used, chain)),
        }
    }
    println!("| Phase | Stack (KiB) |\n|---|---:|");
    for (name, used, _) in &most {
        println!("| {name} | {} |", used.div_ceil(KIB));
    }
    if trace {
        for (name, used, chain) in &most {
            if chain.len == 0 {
                continue;
            }
            println!("\n### {name} ({} KiB, {} B)", used.div_ceil(KIB), used);
            let mut sum = 0usize;
            for &(addr, bytes) in &chain.frames[..chain.len] {
                sum += bytes;
                println!("  {bytes:6} B  {}", symbolize(addr));
            }
            println!("  {sum:6} B  total sampled");
        }
    }
}
