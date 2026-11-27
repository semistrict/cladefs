// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A DAX window's second stage in host memory that a hypervisor follows, such as the memory
//! backing a KVM memory slot, whose faults userfaultfd reports.
//!
//! Nothing here is particular to a VMM: any whose hypervisor maps guest memory from host memory
//! and follows the host's mappings of it can hand the memory over.
//!
//! KVM maps a guest's memory from the host memory backing its memory slot, and follows changes to
//! the host's mappings. On arm64 it reports a fault it cannot resolve there only as `EFAULT`, with
//! no address, so the memory must never fault that way. What the window has not mapped, or maps
//! read-only, is instead registered with userfaultfd, which stops the faulting thread (a vCPU, or
//! KVM on its behalf) and reports the address:
//!
//! - An unmapped page is anonymous memory registered for missing faults.
//! - A mapped page maps the window's page again, read-write, from physical memory
//!   (a memfd, which the process that owns the kernel may share with this one), registered for
//!   write-protect faults, and write-protected, as the window maps every page read-only.
//!
//! cladefs: a page the window maps is installed so as the guest first touches it, not as the window
//! maps it (backlog doc-13: a `MAP` names pages, their first touch installs them): until then it
//! is left out, as a page past the budget is (below), which a fault installs as the window names
//! it then, under the lock the window's changes take. So a change of a page never touched takes
//! nothing away, and flushes nothing; a page the guest writes to first, made private, has its
//! private copy put in place of nothing (TASK-276.6). Only what the guest asks for to read next
//! (doc-4's `MAP_PREFAULT`, [`UffdSecondStage::prefault`]) is installed ahead of its accesses.
//!
//! A fault thread passes the faults to the window, which maps the page (and so maps it here), and
//! wakes the faulting thread. It resolves faults concurrently, as futures it polls as they make
//! progress, so that a fault waiting for Fxfs's pager does not hold up the others. A fault the
//! window cannot resolve is fatal: the faulting thread cannot go on.
//!
//! A fault's access is not always the guest's. x86 KVM hands a fault it cannot resolve without
//! waiting, as it cannot one userfaultfd reports, to a worker that faults the page in for writing
//! (`async_pf_execute`), and lets the vCPU retry its own access once the page is in. So a write to
//! a page a window that doesn't copy on write maps, read-only, is resolved as a read, and the page
//! made read-only here, so that the worker's write fails and the vCPU's own access decides.
//!
//! cladefs: a window that copies on write (backlog doc-13) makes a page private for a store
//! instruction, here, a copy of the page the window names there, read from its arena under the
//! lock the window's changes take (doc-26), which a load mustn't: the worker's write, of a page not
//! here, is a load's as much as a store's. So a fault for writing is the guest's if it write-protected the page here,
//! as a load of a page here takes no fault, and if the faulting thread is one of the guest's own
//! (its vCPUs', [`guest_thread_started`]); a worker's write is resolved as a load, the page made
//! read-only here so that the worker's write fails: of a page not here, and of one write-protected
//! here, as the page a vCPU's load found not here may be installed before the worker runs, by
//! another fault, a probe or a map (TASK-191). A guest makes a page private before it writes to it
//! (doc-13), so no write of its comes through a worker. The vCPU's own access decides: a load is
//! made; a write fails, `KVM_RUN` with `EFAULT`, and the VMM, which retries it, says so
//! ([`guest_access_failed`]), which lifts the restrictions such writes made: the write, retried,
//! write-protected here now, faults as one.
//!
//! Nor is a fault always for a page the guest still maps. While the worker faults the page in, the
//! vCPU runs on, taking the guest's interrupts, and the guest may unmap the page meanwhile; once the worker is done, KVM faults the page
//! again itself, before the vCPU next runs (`kvm_arch_async_page_ready`). A fault on a page the
//! window maps nothing at withholds the page, which is made inaccessible here, so that the fault
//! fails with `EFAULT`, which KVM drops. The guest's own access, made only to pages it maps, would
//! fail as KVM's run of the vCPU does, which the VMM gives up on. The window gives the page back
//! when it maps it anew (`SecondStage::window_mapped`): the kernel maps a page here only once a
//! fault brings it in, which a page withheld would never take.
//!
//! A page restricted by a fault, made read-only for a write the window refuses or withheld, is
//! restricted by the fault thread beside the window's own changes to the memory: only between
//! them, and only as the page was when it faulted. Restricted midway through a change, it would
//! leave memory accessible that is not yet registered; restricted once the page has changed, it
//! would refuse the new page's writes, or withhold a page mapped.
//!
//! Changing a page's mapping here replaces its memory, which takes several system calls, and a
//! fault between them finds memory not yet registered. Until it is registered, new memory is
//! inaccessible, or read-only where a write would go untracked, so that such a fault fails with
//! `EFAULT` rather than resolving wrongly: a page of zeros faulted in, or a write not dirtying its
//! page. A VMM must then let the guest retry its access, as cladefs's Firecracker retries a vCPU's
//! `KVM_RUN` that fails with `EFAULT` for a moment, giving up on one that keeps failing
//! ([`AccessFailures`](crate::AccessFailures)).
//!
//! Each run of pages mapped here, consecutive pages of one arena's memory, is a mapping of the
//! process's own, of which Linux allows `vm.max_map_count` (65,530 by default); a window's pages
//! scattered over its arenas would take one each (TASK-12). The process's windows share a budget
//! of runs installed, half of what the process may map beyond what it keeps for itself
//! ([`Mappings`]), each an equal share of it; a page a fault restricts, read-only or withheld,
//! splitting the mapping it is in as a run does, counts as one. Before a change that may take
//! more, the stage makes room in its share: it leaves out the runs installed or faulted on longest
//! ago, memory registered for missing faults again, whose pages it remembers, and installs one
//! again, as it was, when the guest faults on it, without the window; and then, if it must, lifts
//! the restrictions made longest ago, which a fault on the page makes again. The window's mappings
//! are unchanged. cladefs: each window's budget the whole process's once, a VM process with two,
//! its memory's and an extent device's, could take twice what it may map, and restrictions and
//! the runs a `MAP_PREFAULT` installs counted only after the fact, if at all, a guest's accesses
//! could take its VM process past it, which the kernel refuses with `ENOMEM`, and the stage
//! panics for (board 8493).
//!
//! cladefs: the private pages of a window that copies on write (backlog doc-13) are none of those
//! runs. Its memory where nothing is installed is not anonymous but its private memory
//! ([`PrivateMemory`]): an object as large as the window, laid out as the window is, mapped shared
//! at the window's offsets, of which only the private pages are filled, so that the rest faults as
//! anonymous memory would. A page made private is filled there, its source's bytes written to the
//! object, and the memory around it mapped over the run installed at it, if one is: it merges with
//! the memory around it, however many private pages there are, one mapping between two runs
//! installed; a trim never leaves it out, as it leaves out runs alone. A private page that goes is
//! emptied once nothing maps it any more: its memory, written as the object's and never through a
//! mapping, has no page table entry but where the window maps it, which the change takes away with
//! the rest, so that emptying it flushes no TLB (TASK-276.6). Each held its own mapping of an
//! arena's page once, and a VM writing more pages than the budget had them left out and installed
//! again by the thousand, the fault thread trimming in place of serving faults (r544: 110,857
//! private pages against a budget of some 30,000 runs).
//!
//! The userfaultfd must handle faults taken in the kernel, such as KVM's. It is made through
//! `/dev/userfaultfd` alone, which the process needs read and write access to: never by the system
//! call, `userfaultfd(2)`, which the `vm.unprivileged_userfaultfd` sysctl would allow, as the VMM's
//! seccomp filter refuses it, killing the process for it, where a device it cannot open is an error
//! saying so (TASK-308).

use super::{Access, Anew, Faults, Prefault, Private, SecondStage};
use crate::error::{Errno, Error, Result};
use crate::pages::page_size;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll, Wake, Waker};

#[cfg(feature = "shuttle")]
use shuttle::sync::Mutex;
#[cfg(not(feature = "shuttle"))]
use std::sync::Mutex;

// From `include/uapi/linux/userfaultfd.h`.
const UFFD_API: u64 = 0xaa;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 0;
const UFFD_PAGEFAULT_FLAG_WP: u64 = 1 << 1;
const UFFD_FEATURE_THREAD_ID: u64 = 1 << 8;
const UFFD_FEATURE_WP_HUGETLBFS_SHMEM: u64 = 1 << 12;
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;
const UFFDIO_WRITEPROTECT_MODE_WP: u64 = 1 << 0;

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (0xaa << 8) | nr
}
const IOC_NONE: u64 = 0;
const IOC_READ: u64 = 2;
const IOC_READ_WRITE: u64 = 3;
const USERFAULTFD_IOC_NEW: u64 = ioc(IOC_NONE, 0x00, 0);
const UFFDIO_API: u64 = ioc(IOC_READ_WRITE, 0x3f, size_of::<UffdioApi>());
const UFFDIO_REGISTER: u64 = ioc(IOC_READ_WRITE, 0x00, size_of::<UffdioRegister>());
const UFFDIO_WAKE: u64 = ioc(IOC_READ, 0x02, size_of::<UffdioRange>());
const UFFDIO_WRITEPROTECT: u64 = ioc(IOC_READ_WRITE, 0x06, size_of::<UffdioWriteprotect>());

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioWriteprotect {
    range: UffdioRange,
    mode: u64,
}

/// `struct uffd_msg`, for page faults.
#[repr(C)]
struct UffdMsg {
    event: u8,
    reserved1: u8,
    reserved2: u16,
    reserved3: u32,
    flags: u64,
    address: u64,
    ptid: u32,
    reserved4: u32,
}

const _: () = assert!(size_of::<UffdMsg>() == 32);

/// A DAX window's second stage in host memory that a hypervisor follows. See the module docs.
pub struct UffdSecondStage<H: HostMemory = Syscalls> {
    base: usize,
    size: usize,
    /// Logical sub-page alignment in bytes (normally [`super::map_unit()`], 4 KiB). When smaller
    /// than the host page size (e.g. 4 KiB on a 16 KiB arm64 host), private state is tracked per
    /// `unit`-sized sub-page in [`PageChanges::private_mask`].
    unit: usize,
    /// What changes the memory: system calls, but in tests.
    host: H,
    /// Physical memory, which the window's pages are mapped from.
    memories: Memories,
    /// The hypervisor, told of the pages mapped here, if it maps them ahead of the guest.
    prefault: Option<Arc<dyn Prefault>>,
    /// What the window has done to each page here, held while it makes a change: a page a fault
    /// restricts, as a write the window refuses makes it read-only, waits for a change being made,
    /// and is restricted only if it hasn't changed since the fault.
    pages: Mutex<Pages>,
    /// cladefs: the mappings its process's windows share (see the module docs, board 8493).
    mappings: Arc<Mappings>,
    /// The runs of pages, and the pages a fault restricted, it keeps at most: its share of the
    /// mappings, which it is told as windows come and go, under its pages' lock
    /// ([`Mappings::share`]).
    share: AtomicUsize,
    /// cladefs: what tells of its share once it changed, its VM process's daemon, as the daemon's
    /// metrics show it (board 10098): told outside the mappings' lock ([`Share::told`]).
    on_shared: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// cladefs: the changes that took access away from pages it had installed (TASK-91.16).
    taken: Taken,
    /// cladefs: its window's private memory, if the window copies on write (backlog doc-13): a
    /// worker's write of a page not here is resolved as a load then (see the module docs).
    private: OnceLock<PrivateMemory>,
    /// cladefs: the runs of private pages it made, each at once, their pages, and the nanoseconds
    /// they took it (TASK-164.10, [`Self::private_costs`]).
    copies: [AtomicU64; 3],
    /// cladefs: whether every write is the guest's, a VM's memory's window's
    /// ([`Self::writes_are_guests`]).
    process_writes: std::sync::atomic::AtomicBool,
    /// cladefs: when it was made, which its pages' last changes are told from
    /// ([`describe_windows`]).
    start: std::time::Instant,
    /// cladefs: what it is doing to its memory, if anything, and since when
    /// ([`describe_windows`]).
    busy: Busy,
}

/// cladefs: the private memory of a window that copies on write (backlog doc-13; see the module
/// docs): a memory object of the window's size, the window's memory where nothing is installed, at
/// the window's offsets, which holds the private pages alone. Its pages are filled as the object's
/// ([`HostMemory::fill`]), mapped only where the window maps them: filled through a mapping of
/// its own, `UFFDIO_COPY` into it, each page had a page table entry there, which emptying it took
/// a TLB flush of every CPU of the process's to take away, beside the window's (TASK-276.6).
pub struct PrivateMemory {
    fd: OwnedFd,
    /// How many private pages it holds at most, the VM's `private_max` (doc-26): a write past them
    /// is refused, and its VM stops.
    most: u64,
}

/// cladefs: what a second stage is doing to its memory, holding its pages' lock, and since when, in
/// nanoseconds from its start: what a vCPU whose access keeps failing tells of
/// ([`describe_windows`]).
#[derive(Default)]
struct Busy {
    doing: AtomicU8,
    since: AtomicU64,
    /// When its fault thread began resolving the faults and lifting the restrictions it is on
    /// now, one past, or none while it waits for more: the lifts a vCPU's failing access asks for
    /// wait for it (`guest_access_failed`).
    serving: AtomicU64,
}

/// cladefs: what [`Busy`] says a second stage is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Doing {
    Nothing = 0,
    /// A change the window made.
    Change = 1,
    /// Leaving out the runs installed or faulted on longest ago.
    Trim = 2,
    /// Installing a page left out again, for a fault.
    Reinstall = 3,
}

impl Doing {
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Change,
            2 => Self::Trim,
            3 => Self::Reinstall,
            _ => Self::Nothing,
        }
    }
}

impl Busy {
    /// Says the stage does `doing` from `since`, until what it returns is dropped: under its
    /// pages' lock, so that nothing else says otherwise meanwhile.
    fn doing(&self, doing: Doing, since: u64) -> Busied<'_> {
        self.since.store(since, Ordering::Relaxed);
        self.doing.store(doing as u8, Ordering::Relaxed);
        Busied(self)
    }

    /// What the stage is doing, and since when.
    fn now(&self) -> (Doing, u64) {
        let doing = Doing::from_code(self.doing.load(Ordering::Relaxed));
        (doing, self.since.load(Ordering::Relaxed))
    }
}

/// cladefs: a stage's [`Busy`] doing something, until dropped.
struct Busied<'a>(&'a Busy);

impl Drop for Busied<'_> {
    fn drop(&mut self) {
        self.0.doing.store(Doing::Nothing as u8, Ordering::Relaxed);
    }
}

/// cladefs: the threads whose faults are the guest's own accesses, its vCPUs'
/// ([`guest_thread_started`]), by their IDs, as userfaultfd reports them.
// Outside the simulation: userfaultfd serves VMs, which no simulated host runs.
static GUEST_THREADS: std::sync::RwLock<Vec<u32>> = std::sync::RwLock::new(Vec::new());

/// cladefs: the calling thread's accesses to guest memory are the guest's own, as a vCPU's are: a
/// fault of it for writing, on a window that copies on write, is the guest's (backlog doc-13). A
/// VMM calls it on each vCPU's thread as it starts. Any other thread's, as x86 KVM's asynchronous
/// page fault's worker's, may be for a load.
pub fn guest_thread_started() {
    // SAFETY: Plain system call.
    let thread = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    guest_thread(thread);
}

/// cladefs: the calling thread, which [`guest_thread_started`] said is a vCPU's, is no more: it
/// exits. Its ID is the kernel's to reuse, for any thread, a worker's of KVM's whose faults may be
/// loads among them, and a process that runs many VMs one after another, as libmachine's does, would
/// keep every one it ever ran otherwise (board 13067's review).
pub fn guest_thread_ended() {
    // SAFETY: Plain system call.
    let thread = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    let mut threads = GUEST_THREADS.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(at) = threads.iter().position(|&each| each == thread) {
        threads.swap_remove(at);
    }
}

/// cladefs: thread `thread`'s accesses are the guest's own ([`guest_thread_started`]).
pub fn guest_thread(thread: u32) {
    GUEST_THREADS.write().unwrap_or_else(std::sync::PoisonError::into_inner).push(thread);
}

/// cladefs: whether thread `thread`'s accesses are the guest's own.
fn is_guests(thread: u32) -> bool {
    GUEST_THREADS.read().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&thread)
}

/// cladefs: the lifts of the stages serving faults ([`guest_access_failed`]), each its fault
/// thread's while it runs.
// Outside the simulation: userfaultfd serves VMs, which no simulated host runs.
static LIFTS: std::sync::Mutex<Vec<Arc<Lift>>> = std::sync::Mutex::new(Vec::new());

/// cladefs: how long a guest's access failing waits for the fault threads' lifts at most
/// ([`guest_access_failed`]): less than a run of failures' gap (`AccessFailures::IN_A_ROW`), so
/// that a fault thread slow to lift leaves the VMM retrying the access, one run of failures.
const LIFT_PATIENCE: std::time::Duration = std::time::Duration::from_millis(5);

/// cladefs: a fault thread's lifts of what its stage restricted for workers' writes, which a
/// guest's access failing asks for, and waits for ([`guest_access_failed`]).
struct Lift {
    /// What the fault thread polls, signalled at each ask.
    fd: File,
    /// The asks made so far.
    asked: AtomicU64,
    /// The asks the fault thread has lifted for, all those made before its last lift began.
    lifted: std::sync::Mutex<u64>,
    done: std::sync::Condvar,
}

impl Lift {
    fn new() -> Result<Self, Error> {
        let (asked, lifted) = (AtomicU64::new(0), std::sync::Mutex::new(0));
        Ok(Self { fd: event_fd()?, asked, lifted, done: std::sync::Condvar::new() })
    }

    /// Asks the fault thread to lift: the ask's number, which [`Self::wait`] waits for.
    fn ask(&self) -> u64 {
        let ask = self.asked.fetch_add(1, Ordering::AcqRel) + 1;
        // A write fails with `EAGAIN` only if the counter would overflow, when it is signalled.
        let _ = (&self.fd).write(&1u64.to_ne_bytes());
        ask
    }

    /// Waits until the fault thread has lifted for ask `ask`, or `deadline` has passed.
    fn wait(&self, ask: u64, deadline: std::time::Instant) {
        let mut lifted = self.lifted.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while *lifted < ask {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return;
            };
            lifted = (self.done.wait_timeout(lifted, left))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// On the fault thread, signalled: lifts with `lift`, for the asks made before it began.
    fn serve(&self, lift: impl FnOnce()) {
        let mut count = [0u8; 8];
        // A read fails with `EAGAIN` only if nothing signalled it since the last.
        let _ = (&self.fd).read(&mut count);
        let asked = self.asked.load(Ordering::Acquire);
        lift();
        let mut lifted = self.lifted.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *lifted = (*lifted).max(asked);
        self.done.notify_all();
    }
}

/// cladefs: a guest's access failed, as `KVM_RUN` does with `EFAULT`, which the VMM retries: each
/// stage lifts the restrictions it made for workers' writes, on windows that copy on write, so that
/// the access, a write, retried, faults to the window (see the module docs). Called on a vCPU's
/// thread: it signals the fault threads, which lift them, and waits until they have, for
/// [`LIFT_PATIENCE`] at most. It waited for nothing once, and the vCPU, retrying at once, failed
/// again and again until the fault thread had lifted: 140 times a write after a worker's write,
/// 357 while a lift walked every page of the window (TASK-187,
/// `a_write_after_a_workers_write_waits_for_one_lift`).
pub fn guest_access_failed() {
    let lifts: Vec<Arc<Lift>> =
        LIFTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let asks: Vec<u64> = lifts.iter().map(|lift| lift.ask()).collect();
    let deadline = std::time::Instant::now() + LIFT_PATIENCE;
    for (lift, ask) in lifts.iter().zip(asks) {
        lift.wait(ask, deadline);
    }
}

/// cladefs: the changes a second stage made that took access away from pages it had installed, by
/// how: memory installed over theirs, or theirs left out. Each flushes the host's TLBs of the
/// process's threads and the hypervisor's of the guest's vCPUs, interrupting them, which those
/// that took access from no installed page needn't (TASK-91.16).
#[derive(Default)]
struct Taken {
    overwritten: AtomicU64,
    left_out: AtomicU64,
}

/// cladefs: as [`Taken`] counts them: installed over, left out.
pub type TakenCounts = [u64; 2];

/// How many mappings a VM process keeps for itself beside its window's: Firecracker's, its
/// libraries', guest memory's, its threads' stacks'.
const MAPPINGS_RESERVED: usize = 4096;

/// The runs of pages a process's second stages keep installed at most, all of them together, in a
/// process the host lets make `max_map_count` mappings, if it limits them
/// (`pages::max_map_count`): half of those beyond [`MAPPINGS_RESERVED`], counted once, as each
/// run, a mapping of its own, splits the memory around it, a mapping too ([`Mappings`]). cladefs:
/// told the limit, as a VM process in a jail without /proc can't read it, which once had it budget
/// by Linux's default, a seventeenth of the hosts' (TASK-172).
pub fn budget(max_map_count: Option<u64>) -> usize {
    let Some(max) = max_map_count else {
        return usize::MAX / 2;
    };
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    (max.saturating_sub(MAPPINGS_RESERVED) / 2).max(1)
}

/// cladefs: the mappings a process's DAX windows share (board 8493): a budget of runs of pages
/// installed, and of pages a fault restricted, each splitting the mapping it is in as a run does
/// ([`budget`]), of which each window keeps an equal share at most, two at least: the run a fault
/// installs, and the restriction it makes of its page, which a window keeps however small its
/// share, as its fault waits on them. Each window's budget the process's whole once, a VM
/// process's memory's window beside its extent device's could take twice what the process may
/// map. An equal share, not a pool: a guest's accesses through one window can't leave out what
/// another keeps installed (hotbed question 485).
pub struct Mappings {
    /// The runs, and restrictions, its windows keep at most, all of them together.
    total: AtomicUsize,
    /// Its windows, each told its share as one comes or goes.
    windows: std::sync::Mutex<Vec<Weak<dyn Share>>>,
}

/// cladefs: a window's second stage, as the mappings its process's windows share tell it of its
/// share of them ([`Mappings`]).
trait Share: Send + Sync {
    /// Its share is `share` from now on: if it keeps more, it leaves out runs, and lifts
    /// restrictions, down to it.
    fn shared(&self, share: usize);

    /// cladefs: its share changed ([`Share::shared`]), told once the mappings' lock is let go: what
    /// it does may take locks a window's leaving takes after it (board 10098).
    fn told(&self);
}

impl Mappings {
    /// Mappings of `total` runs and restrictions at most, which no window shares yet.
    pub fn new(total: usize) -> Arc<Self> {
        let total = AtomicUsize::new(total);
        Arc::new(Self { total, windows: std::sync::Mutex::new(Vec::new()) })
    }

    /// This process's, which every window of a VM process shares
    /// ([`UffdSecondStage::new`]): of the smallest budget its windows were told of, by the
    /// `max_map_count` their daemons said ([`budget`]).
    fn process(max_map_count: Option<u64>) -> Arc<Self> {
        // Outside the simulation: userfaultfd serves VMs, which no simulated host runs; and the
        // process's, as what it shares is the process's mappings.
        static PROCESS: OnceLock<Arc<Mappings>> = OnceLock::new();
        let mappings = PROCESS.get_or_init(|| Self::new(budget(None))).clone();
        mappings.total.fetch_min(budget(max_map_count), Ordering::Relaxed);
        mappings
    }

    /// Each window's share, of `windows` windows: an equal part of the total, two at least.
    fn share(&self, windows: usize) -> usize {
        (self.total.load(Ordering::Relaxed) / windows.max(1)).max(2)
    }

    /// `window` shares them from now on: each window, it too, told its share anew.
    fn join(&self, window: Weak<dyn Share>) {
        self.reshare(Some(window));
    }

    /// A window went: each left told its share anew.
    fn leave(&self) {
        self.reshare(None);
    }

    /// Tells each window its share, `joining` one of them: holding the windows' lock, so that
    /// two windows coming at once tell each the share of both. The windows told are let go of
    /// only once the lock is, as the last of one would take it again, leaving.
    fn reshare(&self, joining: Option<Weak<dyn Share>>) {
        let live: Vec<Arc<dyn Share>>;
        {
            let mut windows =
                self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            windows.retain(|window| window.strong_count() > 0);
            windows.extend(joining);
            live = windows.iter().filter_map(Weak::upgrade).collect();
            let share = self.share(live.len());
            for window in &live {
                window.shared(share);
            }
        }
        // cladefs: each tells of its share as it is now, which another reshare may have changed
        // since: the last told is the last made (board 10098).
        for window in &live {
            window.told();
        }
        drop(live);
    }
}

/// Logs a fault the window refused, `what` at window offset `offset`, of `thread`: each of the
/// first thousand, a run's few, and every power of two after: a vCPU whose access stays refused
/// for a second stops its VM with only `EFAULT` to say so, from `KVM_RUN` (TASK-99, TASK-101).
fn refused(what: &str, offset: u64, thread: u32) {
    // Outside the simulation: userfaultfd serves VMs, which no simulated host runs.
    static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if count <= 1000 || count.is_power_of_two() {
        // cladefs: whose, by the VMM's own registry of its vCPU threads, as a jailed VMM has no
        // /proc to name a thread by, and a thread gone is named nothing (TASK-167).
        let whose = if is_guests(thread) { "a vCPU" } else { "not a vCPU" };
        log::warn!(
            "cladefs: the DAX window refused {what}, at offset {offset:#x}, by thread {thread} ({}, \
             {whose}) ({count} so far)",
            thread_name(thread)
        );
    }
}

/// cladefs: the name of thread `thread`, as the kernel has it: whether a fault the window refused
/// was a vCPU's, the guest's own access, a device's, the VMM's on its behalf, or a kernel thread's,
/// as KVM's asynchronous page faults are, a vCPU's access faulted in for it as its guest goes on
/// (TASK-44.12): this process's threads, then any. cladefs: "gone" only if /proc is there to say
/// so: a jailed VMM has none, where every thread would read as gone (TASK-172).
fn thread_name(thread: u32) -> String {
    if !std::path::Path::new("/proc/self").exists() {
        return "unnamed: no /proc".to_owned();
    }
    // Unread: the thread gone, as the name says.
    [format!("/proc/self/task/{thread}/comm"), format!("/proc/{thread}/comm")]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map_or_else(|| "gone".to_owned(), |name| name.trim_end().to_owned())
}

/// cladefs: logs a restriction of a page at window offset `offset` to reads, or its lifting,
/// `what`, as its changes stood (`made`, and what the fault saw, `seen`), each of the first
/// thousand and every power of two after: which side was wrong, the window or the guest, when a
/// write stays refused (TASK-102).
fn changed_protection(what: &str, offset: u64, made: u32, seen: Option<u32>) {
    // Outside the simulation: userfaultfd serves VMs, which no simulated host runs.
    static CHANGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = CHANGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if count <= 1000 || count.is_power_of_two() {
        log::warn!("cladefs: DAX window offset {offset:#x} {what} (made {made}, seen {seen:?})");
    }
}

/// What the window has done to a page of a second stage, as the faults on it see it, and what it
/// maps there.
#[derive(Clone, Copy, Default)]
struct PageChanges {
    /// The changes it has made to the page, and its mappings of it anew.
    made: u32,
    /// Whether a fault that found nothing mapped there withheld the page, which the window's
    /// mapping it anew gives back.
    withheld: bool,
    /// cladefs: whether a fault whose write the window refused made the page read-only here, which
    /// its memory replaced does away with (TASK-102).
    restricted: bool,
    /// cladefs: whether it was restricted for a worker's write, on a window that copies on write,
    /// which a guest's access failing lifts ([`guest_access_failed`]): meaningful while
    /// `restricted`.
    for_worker: bool,
    /// cladefs: whether non-private sub-pages of this private host page need refreshing
    /// from the daemon's [`Self::mapped`] before its private memory is mapped back in; while true,
    /// the host VMA at this page is unfilled anonymous memory registered for missing faults.
    stale: bool,
    /// Whether this host page's VMA is currently unfilled `Memory::Anonymous` (either because it
    /// is `stale` or because `trim()` coalesced multiple stale VMAs into one anonymous span).
    anon_vma: bool,
    /// Whether this partially-private host page in a `process_writes` window (`MemoryCell`) is
    /// mapped write-protected in [`PrivateMemory`] so the next guest write faults and promotes
    /// `private_mask` to all mapped sub-pages.
    wp_partial: bool,
    /// Bitmask of logical `unit`-sized sub-pages currently mapped by the daemon in this host page.
    mapped_mask: u16,
    /// cladefs: bitmask of logical `unit`-sized sub-pages that are private (backlog doc-13):
    /// non-zero means this host page is filled in [`PrivateMemory`]. When `private_mask == 0`,
    /// the host page is not private. On 4 KiB hosts (`subpages == 1`), only bit 0 is used.
    private_mask: u16,
    /// cladefs: when the window last changed it, in nanoseconds from the stage's start: what a vCPU
    /// whose access keeps failing tells of ([`describe_windows`]).
    changed: u64,
    /// What the window maps at the page, if anything (for a stale partially-private page, kept
    /// left-out with `installed == 0` so the next fault can refresh its non-private sub-pages).
    mapped: Option<Mapped>,
}

impl PageChanges {
    /// Whether any sub-page of this host page is private in [`PrivateMemory`].
    fn is_private(&self) -> bool {
        self.private_mask != 0
    }

    /// Whether some, but not all, sub-pages of this host page are private.
    fn is_partially_private(&self, all_mask: u16) -> bool {
        self.private_mask != 0 && self.private_mask != all_mask
    }

    /// cladefs: whether a fault restricted it, withheld or read-only, so that it splits the
    /// mapping it is in, as a run does (board 8493).
    fn restricts(&self) -> bool {
        self.withheld || self.restricted
    }

    /// cladefs: when it was installed or faulted on last, if it is installed.
    fn used_installed(&self) -> Option<u64> {
        self.mapped.filter(Mapped::installed).map(|mapped| mapped.used.get())
    }
}

/// A page the window maps here, read-only: its memory, whether it is installed in host memory or
/// left out, and when it was installed or faulted on last. cladefs: `Option<Mapped>`'s `None` is
/// zero bytes, `used`'s 0, which no tick of the clock is, so that a window's pages, zeroed memory
/// to begin with ([`WindowPages`]), each map nothing; `installed` a byte rather than a bool, whose
/// spare values would hold `None` instead (TASK-227).
#[derive(Clone, Copy)]
struct Mapped {
    phys: crate::pages::PhysLocation,
    /// 1 if installed in host memory, 0 if left out ([`Mapped::installed`]).
    installed: u8,
    used: std::num::NonZeroU64,
}

impl Mapped {
    /// Whether it is installed in host memory, rather than left out.
    fn installed(&self) -> bool {
        self.installed != 0
    }
}

/// cladefs: a window's pages' changes, one for each of its pages, in anonymous memory of their own,
/// zeros until written, which is what a page that nothing changed nor maps is
/// ([`PageChanges::default`], [`Mapped`]): the memory a window takes here is the pages the guest
/// reached's, not the window's size's. As a `Vec` made with each page's default, a 64 GiB window's
/// took 48 bytes a page, 768 MiB, as the VM process began (TASK-227).
struct WindowPages {
    pages: NonNull<PageChanges>,
    len: usize,
}

// SAFETY: The memory is the value's alone, as a Vec's is.
unsafe impl Send for WindowPages {}
// SAFETY: as a Box<[PageChanges]>'s: `&self` only reads it (Deref), writes take `&mut self`
// (DerefMut), and PageChanges is plain data.
unsafe impl Sync for WindowPages {}

impl WindowPages {
    /// Changes for `len` pages, each its default, none of their memory touched.
    fn new(len: usize) -> std::io::Result<Self> {
        let bytes = Self::bytes(len)?;
        if bytes == 0 {
            return Ok(Self { pages: NonNull::dangling(), len });
        }
        let (prot, flags) =
            (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS);
        // SAFETY: Anonymous memory of its own, zeros, which is each page's default.
        let memory = unsafe { libc::mmap(std::ptr::null_mut(), bytes, prot, flags, -1, 0) };
        if memory == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let pages = NonNull::new(memory.cast()).expect("mmap returns no null mapping");
        Ok(Self { pages, len })
    }

    fn bytes(len: usize) -> std::io::Result<usize> {
        len.checked_mul(size_of::<PageChanges>())
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOMEM))
    }
}

impl std::ops::Deref for WindowPages {
    type Target = [PageChanges];

    fn deref(&self) -> &[PageChanges] {
        // SAFETY: `len` pages' changes, zeros or written since, each valid as zeros are
        // ([`PageChanges::default`]'s bytes).
        unsafe { std::slice::from_raw_parts(self.pages.as_ptr(), self.len) }
    }
}

impl std::ops::DerefMut for WindowPages {
    fn deref_mut(&mut self) -> &mut [PageChanges] {
        // SAFETY: As `deref`, the memory this value's alone.
        unsafe { std::slice::from_raw_parts_mut(self.pages.as_ptr(), self.len) }
    }
}

impl Drop for WindowPages {
    fn drop(&mut self) {
        let bytes = Self::bytes(self.len).unwrap_or(0);
        if bytes > 0 {
            // SAFETY: The memory `new` mapped, which nothing refers to once this goes.
            unsafe { libc::munmap(self.pages.as_ptr().cast(), bytes) };
        }
    }
}

/// A second stage's pages, and the runs of them installed.
struct Pages {
    pages: WindowPages,
    /// The runs of pages installed: consecutive pages of one arena's memory, each a mapping, by
    /// their first pages. cladefs: kept as the pages change ([`Pages::update`]), so that a trim
    /// looks at the runs alone, where it walked every page of the window, 16.7M of a 64 GiB
    /// window's, holding the lock every fault takes (board 8495).
    runs: BTreeMap<usize, Run>,
    /// cladefs: how many of its pages a fault restricted, withheld or read-only, each splitting
    /// the mapping it is in as a run does ([`PageChanges::restricts`], board 8493).
    restrictions: usize,
    /// Maximal contiguous runs of pages currently mapped as unfilled `Memory::Anonymous`
    /// (`PageChanges::anon_vma`), counted toward `held()` so stale VMAs stay within the VMA budget.
    anon_runs: usize,
    /// Partially-private host pages on `process_writes` (`MemoryCell`) windows mapped
    /// write-protected (`PageChanges::wp_partial`), each splitting adjacent `PrivateMemory` VMAs
    /// and therefore counted toward `held()`.
    wp_partials: usize,
    /// cladefs: the pages a fault restricted, the first restricted first, some unrestricted
    /// since: what a trim lifts, as it must, the oldest first ([`UffdSecondStage::trim`]).
    restricted_order: VecDeque<usize>,
    /// Counts installations and faults, the ones longest ago left out first.
    clock: u64,
    /// cladefs: whether the memory was let go of ([`UffdSecondStage::release`]): nothing changes
    /// it from now on.
    released: bool,
    /// cladefs: the pages restricted for workers' writes, some perhaps unrestricted since, which a
    /// guest's access failing lifts ([`UffdSecondStage::lift_worker_restrictions`]): each of a
    /// vCPU's `KVM_RUN`s failing lifts them, thousands a second while the guest strays, which a
    /// look at every page of the window took the fault thread from serving faults for (TASK-187).
    for_workers: Vec<usize>,
    /// cladefs: how many of its pages are private (backlog doc-13), which [`PrivateMemory`]'s
    /// `most` bounds (doc-26).
    private: u64,
}

/// cladefs: a run of pages installed ([`Pages::runs`]): the page past its last, and when it was
/// installed or faulted on last, the latest of its pages', or later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    end: usize,
    used: u64,
}

impl Pages {
    /// Whether page `at` starts a run installed: it is installed, and its memory doesn't follow
    /// on from its predecessor's, installed.
    fn starts_run(&self, at: usize) -> bool {
        let Some(Mapped { phys, installed: 1, .. }) = self.pages[at].mapped else {
            return false;
        };
        let follows = at > 0
            && matches!(self.pages[at - 1].mapped, Some(Mapped { phys: before, installed: 1, .. })
                if before.arena == phys.arena && before.offset + page_size() == phys.offset);
        !follows
    }

    /// cladefs: whether page `at` is installed, following on from the run its predecessor is in.
    fn continues_run(&self, at: usize) -> bool {
        self.pages[at].mapped.is_some_and(|mapped| mapped.installed()) && !self.starts_run(at)
    }

    /// cladefs: the first page of the run page `at` is in, if it is installed.
    fn run_of(&self, at: usize) -> Option<usize> {
        let (&start, run) = self.runs.range(..=at).next_back()?;
        (run.end > at).then_some(start)
    }

    /// cladefs: the runs installed, the pages restricted, the anonymous stale runs, and the
    /// write-protected partial pages: what its mappings take of its share (board 8493).
    fn held(&self) -> usize {
        self.runs.len() + self.restrictions + self.anon_runs + self.wp_partials
    }

    /// Changes the pages of `range` with `change`, keeping the runs installed, the count of pages
    /// restricted, the count of anonymous stale runs, and the count of write-protected partial
    /// pages as they are after it. cladefs: a page
    /// starts a run, or follows on in one, by itself and its predecessor alone, so only at the
    /// pages of `range` and the one after it can a run start or end anew: the runs there, and the
    /// one before it reaching them, are taken out, and made again from what those pages are now,
    /// as far as the run the page after it was in reaches, if it follows on still. A run taken out
    /// keeps when it was used last, as do runs it splits into, the latest of their pages' perhaps
    /// earlier: those left out first are left out later, never sooner (board 8495).
    fn update(&mut self, range: std::ops::Range<usize>, change: impl FnOnce(&mut [PageChanges])) {
        let (first, end) = (range.start, range.end);
        if first == end {
            return change(&mut self.pages[range]);
        }
        let count = |pages: &[PageChanges]| pages.iter().filter(|page| page.restricts()).count();
        let count_wp = |pages: &[PageChanges]| pages.iter().filter(|page| page.wp_partial).count();
        let restricted = count(&self.pages[range.clone()]);
        let wp_before = count_wp(&self.pages[range.clone()]);
        let last = end.min(self.pages.len() - 1);
        let anon_starts = |pages: &[PageChanges]| -> usize {
            (first..=last)
                .filter(|&i| pages[i].anon_vma && (i == 0 || !pages[i - 1].anon_vma))
                .count()
        };
        let before_anon = anon_starts(&self.pages);
        let mut old: Vec<(usize, Run)> = Vec::new();
        if let Some((&start, &run)) = self.runs.range(..first).next_back()
            && run.end >= first
        {
            old.push((start, run));
        }
        old.extend(self.runs.range(first..=end).map(|(&start, &run)| (start, run)));
        for (start, _) in &old {
            self.runs.remove(start);
        }
        change(&mut self.pages[range.clone()]);
        self.restrictions = self.restrictions - restricted + count(&self.pages[range.clone()]);
        self.wp_partials = self.wp_partials - wp_before + count_wp(&self.pages[range]);
        self.anon_runs = self.anon_runs - before_anon + anon_starts(&self.pages);
        // The run reaching the range from before it, if it still does.
        let mut open =
            old.first().filter(|(start, _)| *start < first).map(|&(start, run)| (start, run.used));
        for at in first..=last {
            let used = self.pages[at].used_installed().unwrap_or(0);
            if let Some((_, latest)) = open.as_mut()
                && self.continues_run(at)
            {
                *latest = (*latest).max(used);
                continue;
            }
            if let Some((start, used)) = open.take() {
                self.runs.insert(start, Run { end: at, used });
            }
            if self.starts_run(at) {
                open = Some((at, used));
            }
        }
        let Some((start, latest)) = open else { return };
        // On past the range's last page, as far as the run that page was in before, unchanged.
        let run = match old.iter().find(|(start, run)| *start <= last && run.end > last) {
            Some(&(_, run)) if last == end => Run { end: run.end, used: run.used.max(latest) },
            _ => Run { end: last + 1, used: latest },
        };
        self.runs.insert(start, run);
    }

    /// The next tick of the clock, never 0.
    fn tick(&mut self) -> std::num::NonZeroU64 {
        self.clock += 1;
        std::num::NonZeroU64::new(self.clock).expect("the clock ticks from 1")
    }

    /// cladefs: whether a page of `range` is installed, which a change taking it away flushes.
    fn any_installed(&self, range: std::ops::Range<usize>) -> bool {
        self.pages[range].iter().any(|page| page.mapped.is_some_and(|mapped| mapped.installed()))
    }

    /// cladefs: what a change of window byte range `start_off..end_off` takes away from its memory
    /// ([`UffdSecondStage::take_away`]): its pages installed, its private pages whose private
    /// sub-pages all end (to be emptied), its partially-private pages that become stale (whose
    /// VMA must become unfilled anonymous memory until refreshed), its already-stale pages to skip
    /// remapping to `PrivateMemory`, and those it made inaccessible or read-only and installed
    /// nothing at, which it gives back.
    fn take_away_bytes(
        &mut self,
        start_off: usize,
        end_off: usize,
        private: Private,
        unit: usize,
        all_mask: u16,
        mark_partial_stale: bool,
    ) -> Taking {
        let ps = page_size() as usize;
        let range = start_off / ps..end_off.div_ceil(ps);
        let installed = self.any_installed(range.clone()) && rules().changes_take_away_installed;
        let protected = (range.clone())
            .filter(|&at| {
                let page = &self.pages[at];
                (page.withheld || page.restricted)
                    && !page.is_private()
                    && !page.mapped.is_some_and(|mapped| mapped.installed())
            })
            .collect();
        let ends = private == Private::End || !rules().host_changes_keep_private;
        let mut private_runs: Vec<std::ops::Range<usize>> = Vec::new();
        let mut newly_staled: Vec<usize> = Vec::new();
        let mut keep_stale: Vec<usize> = Vec::new();
        let mut stale_ended = false;
        let mut ended_private = 0u64;
        self.update(range.clone(), |slice| {
            for (idx, page) in slice.iter_mut().enumerate() {
                let at = range.start + idx;
                let clear_mask = if ends {
                    let page_start = at * ps;
                    let page_end = page_start + ps;
                    let lo = (start_off.max(page_start) - page_start) / unit;
                    let hi = (end_off.min(page_end) - page_start) / unit;
                    (((1u32 << (hi - lo)) - 1) << lo) as u16
                } else {
                    0
                };
                if ends {
                    page.mapped_mask &= !clear_mask;
                }
                if !page.is_private() {
                    if page.anon_vma {
                        stale_ended = true;
                        page.anon_vma = false;
                    }
                    continue;
                }
                if ends {
                    let remain = page.private_mask & !clear_mask;
                    if remain == 0 {
                        stale_ended |= std::mem::take(&mut page.stale);
                        stale_ended |= std::mem::take(&mut page.anon_vma);
                        stale_ended |= std::mem::take(&mut page.wp_partial);
                        page.private_mask = 0;
                        ended_private += 1;
                        match private_runs.last_mut() {
                            Some(run) if run.end == at => run.end = at + 1,
                            _ => private_runs.push(at..at + 1),
                        }
                    } else {
                        page.private_mask = remain;
                        if mark_partial_stale {
                            if !page.anon_vma {
                                newly_staled.push(at);
                            }
                            page.stale = true;
                            page.anon_vma = true;
                            page.wp_partial = false;
                            keep_stale.push(at);
                        } else if page.anon_vma {
                            keep_stale.push(at);
                        }
                    }
                } else if page.is_partially_private(all_mask) {
                    if mark_partial_stale {
                        if !page.anon_vma {
                            newly_staled.push(at);
                        }
                        page.stale = true;
                        page.anon_vma = true;
                        page.wp_partial = false;
                        keep_stale.push(at);
                    } else if page.anon_vma {
                        keep_stale.push(at);
                    }
                } else if page.anon_vma {
                    keep_stale.push(at);
                }
            }
        });
        self.private -= ended_private;
        Taking {
            installed,
            private: private_runs,
            protected,
            newly_staled,
            keep_stale,
            stale_ended,
        }
    }

    /// cladefs: whether the pages of `range` are each installed, in one run of memory from `phys`
    /// on, none restricted by a fault: what a map of the very same pages there leaves in place
    /// (TASK-91.16).
    fn installed_at(
        &self,
        range: std::ops::Range<usize>,
        phys: crate::pages::PhysLocation,
    ) -> bool {
        self.pages[range].iter().enumerate().all(|(at, page)| {
            let offset = phys.offset + at as u64 * page_size();
            !page.restricted
                && page.mapped.is_some_and(|mapped| {
                    mapped.installed()
                        && mapped.phys == (crate::pages::PhysLocation { offset, ..phys })
                })
        })
    }
}

/// cladefs: what a change takes away from the memory of the pages it changes, which their memory
/// must hold no more once it is made ([`Pages::take_away_bytes`]), as the window's mapping of them
/// ends.
struct Taking {
    /// Whether a page of them is installed.
    installed: bool,
    /// The runs of private pages among them whose private sub-pages all ended, to be emptied.
    private: Vec<std::ops::Range<usize>>,
    /// The pages among them installed nothing at, private or not, that a fault made inaccessible,
    /// withheld, or read-only, to be made accessible again.
    protected: Vec<usize>,
    /// Partially-private host pages that just became stale: their VMA must become unfilled
    /// anonymous memory registered for missing faults (preserving [`PrivateMemory`]).
    newly_staled: Vec<usize>,
    /// All host pages in the range that remain stale after the change (newly or previously): must
    /// not be remapped back to `base_memory` (`PrivateMemory`) by `leave_out_emptying`.
    keep_stale: Vec<usize>,
    /// Whether a previously stale private host page ended completely and must be remapped back to
    /// `base_memory` (unfilled `PrivateMemory`) rather than only punched in place.
    stale_ended: bool,
}

/// cladefs: what a guest's write to a page found, on a window that copies on write
/// ([`UffdSecondStage::copy`], doc-26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Copied {
    /// The page is private: made so now, or before.
    Private,
    /// Partially private and stale: non-private sub-pages must be refreshed first.
    Stale,
    /// Past the window's private pages: the write is refused.
    Past,
    /// The window names nothing there yet: to read in first.
    Unnamed,
}

/// Result of attempting to reinstall a page on fault ([`UffdSecondStage::reinstall`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reinstalled {
    /// Installed (or already private and fresh): the access can be retried.
    Done,
    /// Partially private and stale: caller must refresh non-private sub-pages.
    Stale,
    /// Not named by the window (or already installed): fault must go to the window.
    Miss,
}

/// What a second stage maps its host memory with.
pub enum Memory {
    /// Anonymous memory, private, of zeros until written.
    Anonymous,
    /// `fd`'s memory at `offset`, shared with it or private to this process.
    File { fd: std::os::fd::RawFd, offset: u64, shared: bool },
}

/// What a second stage changes its host memory with: system calls ([`Syscalls`]), or, in its
/// Shuttle tests, a model of what they do, against which a guest's accesses are made.
pub trait HostMemory: Send + Sync + 'static {
    /// Maps `len` bytes of `memory` at `addr` with protection `prot`, replacing what was mapped
    /// there, and its registration with userfaultfd (`mmap` with `MAP_FIXED`).
    fn map(&self, addr: usize, len: usize, prot: libc::c_int, memory: Memory);

    /// Changes the protection of the `len` bytes at `addr` (`mprotect`).
    fn protect(&self, addr: usize, len: usize, prot: libc::c_int);

    /// Registers the `len` bytes at `addr` with userfaultfd for faults of `mode`.
    fn register(&self, addr: usize, len: usize, mode: u64);

    /// Write-protects the `len` bytes at `addr` with userfaultfd.
    fn write_protect(&self, addr: usize, len: usize);

    /// Lifts userfaultfd write protection on the `len` bytes at `addr`.
    fn write_unprotect(&self, addr: usize, len: usize);

    /// Wakes the threads faulting on the `len` bytes at `addr`.
    fn wake(&self, addr: usize, len: usize);

    /// cladefs: a new memory object of `len` bytes, none of them filled: a window's private memory
    /// (`memfd_create`).
    fn private_memory(&self, len: usize) -> std::io::Result<OwnedFd>;

    /// cladefs: fills the pages of memory object `fd` at `offset`, none filled yet, with `bytes`,
    /// as the object's (`pwrite`): where they are mapped, accesses find them filled, and nothing
    /// maps them that didn't.
    fn fill(&self, fd: RawFd, offset: u64, bytes: &[u8]);

    /// cladefs: overwrites a non-private sub-page slice of an already-filled host page in memory
    /// object `fd` at `offset` (`pwrite`), used when refreshing stale non-private sub-pages of a
    /// partially-private host page.
    fn write_subpage(&self, fd: RawFd, offset: u64, bytes: &[u8]);

    /// cladefs: empties the `len` bytes of memory object `fd` at `offset`: faults on them, where
    /// they are mapped, find them unfilled again (`fallocate`'s `FALLOC_FL_PUNCH_HOLE`).
    fn punch(&self, fd: RawFd, offset: u64, len: usize);
}

/// The system calls that change a second stage's host memory, with its userfaultfd.
pub struct Syscalls {
    uffd: OwnedFd,
}

impl HostMemory for Syscalls {
    fn map(&self, addr: usize, len: usize, prot: libc::c_int, memory: Memory) {
        let (flags, fd, offset) = match memory {
            Memory::Anonymous => {
                (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0)
            }
            Memory::File { fd, offset, shared } => {
                let share = if shared { libc::MAP_SHARED } else { libc::MAP_PRIVATE };
                (share, fd, offset as libc::off_t)
            }
        };
        // SAFETY: Replaces the second stage's memory at `addr`, which only the guest accesses.
        let mapped =
            unsafe { libc::mmap(addr as *mut _, len, prot, flags | libc::MAP_FIXED, fd, offset) };
        assert_eq!(mapped as usize, addr, "map {addr:#x}: {}", std::io::Error::last_os_error());
    }

    fn protect(&self, addr: usize, len: usize, prot: libc::c_int) {
        // SAFETY: Changes the protection of the second stage's memory.
        let status = unsafe { libc::mprotect(addr as *mut _, len, prot) };
        assert_eq!(status, 0, "protect {addr:#x}: {}", std::io::Error::last_os_error());
    }

    fn register(&self, addr: usize, len: usize, mode: u64) {
        let range = UffdioRange { start: addr as u64, len: len as u64 };
        let mut register = UffdioRegister { range, mode, ioctls: 0 };
        ioctl(&self.uffd, UFFDIO_REGISTER, &mut register)
            .unwrap_or_else(|err| panic!("register {addr:#x} with userfaultfd: {err}"));
    }

    fn write_protect(&self, addr: usize, len: usize) {
        let range = UffdioRange { start: addr as u64, len: len as u64 };
        let mut write_protect = UffdioWriteprotect { range, mode: UFFDIO_WRITEPROTECT_MODE_WP };
        ioctl(&self.uffd, UFFDIO_WRITEPROTECT, &mut write_protect)
            .unwrap_or_else(|err| panic!("write-protect {addr:#x} with userfaultfd: {err}"));
    }

    fn write_unprotect(&self, addr: usize, len: usize) {
        let range = UffdioRange { start: addr as u64, len: len as u64 };
        let mut write_protect = UffdioWriteprotect { range, mode: 0 };
        ioctl(&self.uffd, UFFDIO_WRITEPROTECT, &mut write_protect)
            .unwrap_or_else(|err| panic!("write-unprotect {addr:#x} with userfaultfd: {err}"));
    }

    fn wake(&self, addr: usize, len: usize) {
        let mut range = UffdioRange { start: addr as u64, len: len as u64 };
        ioctl(&self.uffd, UFFDIO_WAKE, &mut range).expect("wake a faulting thread");
    }

    fn private_memory(&self, len: usize) -> std::io::Result<OwnedFd> {
        Self::private_memory_fd(len)
    }

    fn fill(&self, fd: RawFd, offset: u64, bytes: &[u8]) {
        Self::write_at(fd, offset, bytes);
    }

    fn write_subpage(&self, fd: RawFd, offset: u64, bytes: &[u8]) {
        Self::write_at(fd, offset, bytes);
    }

    fn punch(&self, fd: RawFd, offset: u64, len: usize) {
        Self::punch_hole(fd, offset, len);
    }
}

impl Syscalls {
    /// A new userfaultfd's system calls, which handles faults taken in the kernel too.
    fn open() -> Result<Self, Error> {
        let uffd = open_userfaultfd()?;
        let mut api = UffdioApi {
            api: UFFD_API,
            features: UFFD_FEATURE_WP_HUGETLBFS_SHMEM | UFFD_FEATURE_THREAD_ID,
            ioctls: 0,
        };
        ioctl(&uffd, UFFDIO_API, &mut api)
            .map_err(|err| Error::new(anyhow::Error::new(err).context("UFFDIO_API")))?;
        Ok(Self { uffd })
    }

    /// cladefs: a new memory object of `len` bytes, none of them filled
    /// ([`HostMemory::private_memory`]).
    fn private_memory_fd(len: usize) -> std::io::Result<OwnedFd> {
        // SAFETY: Creates a memory object, which is owned below.
        let fd =
            unsafe { libc::memfd_create(c"cladefs-private-pages".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` was just created, and is owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: Plain system call on the descriptor just made: no page is filled.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }

    /// cladefs: writes `bytes` to memory object `fd` at `offset` ([`HostMemory::fill`]).
    fn write_at(fd: RawFd, offset: u64, bytes: &[u8]) {
        let mut done = 0;
        while done < bytes.len() {
            let rest = &bytes[done..];
            let at = (offset + done as u64) as libc::off_t;
            // SAFETY: Writes from `rest`, which is as long as said.
            let written = unsafe { libc::pwrite(fd, rest.as_ptr().cast(), rest.len(), at) };
            if written < 0 {
                let err = std::io::Error::last_os_error();
                assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "fill {at:#x}: {err}");
                continue;
            }
            assert!(written > 0, "fill {at:#x}: nothing written");
            done += written as usize;
        }
    }

    /// cladefs: empties the `len` bytes of memory object `fd` at `offset` ([`HostMemory::punch`]).
    fn punch_hole(fd: RawFd, offset: u64, len: usize) {
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        // SAFETY: Plain system call on a memory object's descriptor.
        let status =
            unsafe { libc::fallocate(fd, mode, offset as libc::off_t, len as libc::off_t) };
        assert_eq!(status, 0, "punch {offset:#x}: {}", std::io::Error::last_os_error());
    }
}

/// The rules of the order in which the second stage changes its memory, each of which its Shuttle
/// tests show to be needed by turning it off (TASK-44.3).
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// A page mapped here is read-only until its writes are tracked.
    pub map_with_window_protection: bool,
    /// A page mapped here is write-protected before it is made writable.
    pub write_protect_before_writable: bool,
    /// Memory unmapped here is inaccessible until it is registered for missing faults.
    pub register_before_accessible: bool,
    /// A write the window refuses makes its page read-only here.
    pub refused_write_read_only: bool,
    /// A refused write's page is made read-only, and a page withheld inaccessible, between the
    /// window's changes to the memory, and only if the page hasn't changed since the fault; a page
    /// withheld is recorded so, to be given back.
    pub restrict_between_changes: bool,
    /// A page withheld is given back when the window maps it anew.
    pub mapping_gives_back_withheld: bool,
    /// cladefs: a worker's write of a page not here, on a window that copies on write, resolved as
    /// a load, makes the page read-only here, so that the worker's write, retried, fails rather
    /// than copying it for a load (backlog doc-13).
    pub worker_writes_restricted: bool,
    /// cladefs: a guest's access failing lifts what workers' writes restricted, so that a write,
    /// retried, faults to the window.
    pub failures_lift_worker_restrictions: bool,
    /// cladefs: a private page is filled before its memory is mapped in place of what was there, so
    /// that the memory is never accessible unfilled before it is registered (backlog doc-13).
    pub private_filled_before_mapped: bool,
    /// cladefs: a private page that goes is emptied, so that the memory around it, mapped there
    /// anew, faults again rather than holding the private page for good.
    pub private_emptied_as_it_goes: bool,
    /// cladefs: a change of pages installed takes their memory away, left out, for their next touch
    /// to install as the window names them then (backlog doc-13: a `MAP` names pages, their first
    /// touch installs them), rather than leaving what the window mapped before (TASK-276.6).
    pub changes_take_away_installed: bool,
    /// cladefs: a page left out is installed as the window names it as it is installed, under the
    /// lock its changes take, not as a fault found it before a change since (TASK-276.6).
    pub install_as_named_now: bool,
    /// cladefs: a guest's write copies the page the window names there under the pages' lock, which
    /// the change taking the page away takes, so that the copy comes before the unmap's
    /// acknowledgement, and the daemon frees the page only after it (doc-26, spec rule
    /// copyUnderLock).
    pub copy_under_lock: bool,
    /// cladefs: the host's own unmaps, an eviction's or a free's (`STAGE_KEEP_PRIVATE`), leave the
    /// private pages there (doc-26, spec rule evictionKeepsPrivate).
    pub host_changes_keep_private: bool,
    /// cladefs: a page the window maps where a private page is leaves it private (doc-26, spec rule
    /// mapsKeepPrivate): after an eviction left it, the window maps the rest of the part over it.
    pub maps_keep_private: bool,
}

impl Rules {
    pub const ALL: Self = Self {
        map_with_window_protection: true,
        write_protect_before_writable: true,
        register_before_accessible: true,
        refused_write_read_only: true,
        restrict_between_changes: true,
        mapping_gives_back_withheld: true,
        worker_writes_restricted: true,
        failures_lift_worker_restrictions: true,
        private_filled_before_mapped: true,
        private_emptied_as_it_goes: true,
        changes_take_away_installed: true,
        install_as_named_now: true,
        copy_under_lock: true,
        host_changes_keep_private: true,
        maps_keep_private: true,
    };
}

#[cfg(not(feature = "shuttle"))]
fn rules() -> Rules {
    Rules::ALL
}

#[cfg(feature = "shuttle")]
static RULES: std::sync::Mutex<Rules> = std::sync::Mutex::new(Rules::ALL);

#[cfg(feature = "shuttle")]
fn rules() -> Rules {
    *RULES.lock().unwrap()
}

/// Physical memory, by arena (TASK-43), which a window's pages are mapped from.
pub enum Memories {
    /// This process's, every arena's, as tests have it.
    #[cfg(test)]
    InProcess,
    /// The arenas' a daemon gave this process: its tenant's, and the default, read-only, or the
    /// default alone.
    Given(Vec<(crate::pages::ArenaId, OwnedFd)>),
}

impl Memories {
    /// The descriptor of `arena`'s memory, which the window maps pages of, and whether it is
    /// read-only, as a tenant's VM process holds the default arena.
    fn fd(&self, arena: crate::pages::ArenaId) -> Option<(std::os::fd::RawFd, bool)> {
        let fd = match self {
            #[cfg(test)]
            Self::InProcess => crate::pages::arena_memory(arena)?.as_raw_fd(),
            Self::Given(memories) => {
                memories.iter().find(|(given, _)| *given == arena)?.1.as_raw_fd()
            }
        };
        // SAFETY: Plain system call on a descriptor the memories hold.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        Some((fd, flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY))
    }
}

/// How many mappings the host lets a test's process make, which a test, with /proc, reads.
#[cfg(test)]
pub fn host_max_map_count() -> Option<u64> {
    crate::pages::max_map_count().expect("the host's vm.max_map_count")
}

impl UffdSecondStage {
    /// Takes over the `size` bytes of host memory at `base`, replacing them with memory whose
    /// faults userfaultfd reports, as nothing is mapped yet: the window's pages are mapped there
    /// from `phys`, physical memory. `prefault`, if any, is told of the pages mapped
    /// there. It keeps as many runs of pages installed as its share of what a process the host
    /// lets make `max_map_count` mappings may keep, with its other windows ([`Mappings`]).
    ///
    /// # Safety
    ///
    /// The memory must be a page-aligned mapping that nothing else in the process uses.
    pub unsafe fn new(
        base: usize,
        size: u64,
        memories: Memories,
        prefault: Option<Arc<dyn Prefault>>,
        max_map_count: Option<u64>,
    ) -> Result<Arc<Self>, Error> {
        let mappings = Mappings::process(max_map_count);
        // SAFETY: As the caller promises.
        let stage = unsafe { Self::with_mappings(base, size, memories, prefault, mappings) }?;
        log::info!(
            "cladefs: a DAX window of {size} bytes keeps at most {} runs of pages installed, its \
             share of its process's windows', the host letting a process make {} mappings",
            stage.budget(),
            max_map_count.map_or_else(|| "any number of".to_owned(), |max| max.to_string())
        );
        Ok(stage)
    }

    /// As [`new`](Self::new), keeping its share of `mappings`.
    ///
    /// # Safety
    ///
    /// As [`new`](Self::new).
    pub unsafe fn with_mappings(
        base: usize,
        size: u64,
        memories: Memories,
        prefault: Option<Arc<dyn Prefault>>,
        mappings: Arc<Mappings>,
    ) -> Result<Arc<Self>, Error> {
        let host = Syscalls::open()?;
        // SAFETY: As the caller promises.
        unsafe { Self::with_host(base, size, host, memories, prefault, mappings) }
    }

    /// As [`new`](Self::new), for a window of this process's kernel, as tests have it.
    ///
    /// # Safety
    ///
    /// As [`new`](Self::new).
    #[cfg(test)]
    pub unsafe fn in_process(
        base: usize,
        size: u64,
        prefault: Option<Arc<dyn Prefault>>,
    ) -> Result<Arc<Self>, Error> {
        // SAFETY: As the caller promises.
        unsafe { Self::new(base, size, Memories::InProcess, prefault, host_max_map_count()) }
    }

    #[cfg(test)]
    pub unsafe fn in_process_with_unit(
        base: usize,
        size: u64,
        unit: usize,
        prefault: Option<Arc<dyn Prefault>>,
    ) -> Result<Arc<Self>, Error> {
        let mappings = Mappings::process(host_max_map_count());
        let host = Syscalls::open()?;
        // SAFETY: As the caller promises.
        unsafe {
            Self::with_host_and_unit(
                base,
                size,
                unit,
                host,
                Memories::InProcess,
                prefault,
                mappings,
            )
        }
    }

    /// cladefs: the runs of its window's private pages, by their offsets in the window, in order:
    /// the private memory's pages filled (`SEEK_DATA`, `SEEK_HOLE`), which only private pages are.
    /// None for a window that copies nothing. A VM's memory's snapshot writes them (TASK-456.3).
    pub fn private_runs(&self) -> Result<Vec<std::ops::Range<u64>>> {
        let Some(private) = self.private.get() else { return Ok(Vec::new()) };
        let fd = private.fd.as_raw_fd();
        let size = self.size as libc::off_t;
        let mut runs = Vec::new();
        let mut at: libc::off_t = 0;
        while at < size {
            // SAFETY: Plain system calls on the private memory's descriptor.
            let data = unsafe { libc::lseek(fd, at, libc::SEEK_DATA) };
            if data < 0 {
                match std::io::Error::last_os_error().raw_os_error() {
                    // No data past `at`.
                    Some(libc::ENXIO) => break,
                    _ => return Err(Errno::EIO),
                }
            }
            // SAFETY: As above.
            let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
            if hole < 0 {
                return Err(Errno::EIO);
            }
            runs.push(data as u64..(hole.min(size)) as u64);
            at = hole;
        }
        Ok(runs)
    }

    /// Serves the faults on the memory with `window`, on a thread of its own, until the returned
    /// thread is dropped.
    pub fn serve(self: &Arc<Self>, window: Arc<dyn Faults>) -> Result<FaultThread, Error> {
        assert_eq!(window.size(), self.size as u64, "the window is not the second stage's size");
        let stop = Arc::new(event_fd()?);
        let wake = Arc::new(EventFdWaker(event_fd()?));
        // cladefs: a guest's access failing, said to every stage (`guest_access_failed`).
        let lift = Arc::new(Lift::new()?);
        LIFTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(lift.clone());
        let thread_stop = stop.clone();
        let (stage, thread_lift) = (self.clone(), lift.clone());
        let thread = crate::thread::spawn("dax faults", move || {
            stage.serve_faults(&*window, &thread_stop, wake, &thread_lift)
        });
        let thread = match thread {
            Ok(thread) => thread,
            Err(err) => {
                forget_lift(&lift);
                return Err(Error::new(err));
            }
        };
        // cladefs: told of by a vCPU whose access keeps failing (`describe_windows`).
        let mut serving = SERVING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        serving.retain(|stage| stage.strong_count() > 0);
        serving.push(Arc::downgrade(self));
        Ok(FaultThread { stop, lift, thread: Some(thread) })
    }

    /// Passes the memory's faults to `window` until `stop` is signalled. Faults are resolved
    /// concurrently: each is a future, which this thread polls once `wake`, its waker, signals that
    /// it can make progress, as when Fxfs's pager supplies the page it waits for. cladefs: and
    /// lifts what workers' writes restricted as `lift` is signalled ([`guest_access_failed`]).
    fn serve_faults(&self, window: &dyn Faults, stop: &File, wake: Arc<EventFdWaker>, lift: &Lift) {
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let mut faults = FuturesUnordered::new();
        let mut fds = [
            libc::pollfd { fd: self.host.uffd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.0.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: lift.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        loop {
            // Make what progress the faults can, and then wait for new faults or for a fault's
            // waker.
            while let Poll::Ready(Some(())) = faults.poll_next_unpin(&mut cx) {}
            self.busy.serving.store(0, Ordering::Relaxed);
            // SAFETY: `fds` is an array of `pollfd`s of its length.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            self.busy.serving.store(self.now() + 1, Ordering::Relaxed);
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "poll userfaultfd: {err}");
                continue;
            }
            if fds[1].revents != 0 {
                return;
            }
            if fds[2].revents != 0 {
                wake.reset();
            }
            if fds[3].revents != 0 {
                lift.serve(|| self.lift_worker_restrictions());
            }
            while let Some(msg) = self.read_event() {
                if msg.event == UFFD_EVENT_PAGEFAULT {
                    faults.push(self.resolve(window, msg.address as usize, msg.flags, msg.ptid));
                }
            }
        }
    }

    /// Reads the next event, if there is one.
    fn read_event(&self) -> Option<UffdMsg> {
        let mut msg = std::mem::MaybeUninit::<UffdMsg>::uninit();
        // SAFETY: Reads one message into `msg`, which is its size.
        let read = unsafe {
            libc::read(self.host.uffd.as_raw_fd(), msg.as_mut_ptr().cast(), size_of::<UffdMsg>())
        };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock, "read userfaultfd: {err}");
            return None;
        }
        assert_eq!(read as usize, size_of::<UffdMsg>(), "short read of a userfaultfd message");
        // SAFETY: The kernel wrote a whole message.
        Some(unsafe { msg.assume_init() })
    }
}

impl<H: HostMemory> UffdSecondStage<H> {
    /// As [`new`](UffdSecondStage::new), changing the memory with `host`.
    ///
    /// # Safety
    ///
    /// As [`new`](UffdSecondStage::new), if `host` changes it.
    pub unsafe fn with_host(
        base: usize,
        size: u64,
        host: H,
        memories: Memories,
        prefault: Option<Arc<dyn Prefault>>,
        mappings: Arc<Mappings>,
    ) -> Result<Arc<Self>, Error> {
        let unit = super::map_unit() as usize;
        // SAFETY: As the caller promises.
        unsafe { Self::with_host_and_unit(base, size, unit, host, memories, prefault, mappings) }
    }

    pub unsafe fn with_host_and_unit(
        base: usize,
        size: u64,
        unit: usize,
        host: H,
        memories: Memories,
        prefault: Option<Arc<dyn Prefault>>,
        mappings: Arc<Mappings>,
    ) -> Result<Arc<Self>, Error> {
        let size = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
        let page_size = page_size() as usize;
        if !base.is_multiple_of(page_size)
            || size == 0
            || !size.is_multiple_of(page_size)
            || unit == 0
            || !page_size.is_multiple_of(unit)
            || (page_size / unit) as u64 > crate::pages::MAX_SUBPAGES
        {
            return Err(Error::msg("DAX window memory must be whole pages"));
        }
        let pages = WindowPages::new(size / page_size).map_err(Error::new)?;
        let for_workers = Vec::new();
        let pages = Mutex::new(Pages {
            pages,
            runs: BTreeMap::new(),
            restrictions: 0,
            anon_runs: 0,
            wp_partials: 0,
            restricted_order: VecDeque::new(),
            clock: 0,
            released: false,
            for_workers,
            private: 0,
        });
        let taken = Taken::default();
        let stage = Arc::new(Self {
            base,
            size,
            unit,
            host,
            memories,
            prefault,
            pages,
            mappings: mappings.clone(),
            // Told its share as it joins, below.
            share: AtomicUsize::new(2),
            on_shared: OnceLock::new(),
            taken,
            private: OnceLock::new(),
            copies: Default::default(),
            process_writes: std::sync::atomic::AtomicBool::new(false),
            start: std::time::Instant::now(),
            busy: Busy::default(),
        });
        stage.leave_out_untouched();
        let shares: Weak<dyn Share> = Arc::downgrade(&stage) as Weak<dyn Share>;
        mappings.join(shares);
        Ok(stage)
    }

    /// Number of logical `unit`-sized sub-pages per host page (1 on 4 KiB hosts, 4 on 16 KiB).
    fn subpages(&self) -> usize {
        page_size() as usize / self.unit
    }

    /// Bitmask with every logical sub-page of a host page set.
    fn all_subpages_mask(&self) -> u16 {
        ((1u32 << self.subpages()) - 1) as u16
    }

    /// Bitmask of the logical sub-pages of host page `at` covered by window byte range
    /// `start_off..end_off` (both multiples of `self.unit`).
    fn subpage_mask(&self, at: usize, start_off: usize, end_off: usize) -> u16 {
        let ps = page_size() as usize;
        let page_start = at * ps;
        let page_end = page_start + ps;
        let lo = (start_off.max(page_start) - page_start) / self.unit;
        let hi = (end_off.min(page_end) - page_start) / self.unit;
        debug_assert!(lo < hi && hi <= self.subpages());
        (((1u32 << (hi - lo)) - 1) << lo) as u16
    }

    /// Replaces the VMA of one host page at `addr` with unfilled anonymous memory registered for
    /// missing faults, without touching [`PrivateMemory`]: used when a partially-private host page
    /// becomes stale so the next access traps into userfaultfd to refresh its non-private
    /// sub-pages.
    fn mark_stale_vma(&self, addr: usize) {
        let page_size = page_size() as usize;
        let writable = libc::PROT_READ | libc::PROT_WRITE;
        let prot = if rules().register_before_accessible { libc::PROT_NONE } else { writable };
        self.host.map(addr, page_size, prot, Memory::Anonymous);
        self.host.register(addr, page_size, UFFDIO_REGISTER_MODE_MISSING);
        self.host.protect(addr, page_size, writable);
    }

    /// Maps `base_memory` (`PrivateMemory`) back at host page `addr`, write-protected when
    /// `want_wp` is true (so a subsequent guest write on a partially-private `MemoryCell` page
    /// traps into userfaultfd and promotes `private_mask`), and wakes any waiting fault thread.
    fn map_private_vma(&self, addr: usize, want_wp: bool) {
        let page_size = page_size() as usize;
        let writable = libc::PROT_READ | libc::PROT_WRITE;
        if want_wp {
            self.host.map(addr, page_size, libc::PROT_READ, self.base_memory(addr));
            self.host.register(
                addr,
                page_size,
                UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_WP,
            );
            if !rules().write_protect_before_writable {
                self.host.protect(addr, page_size, writable);
            }
            self.host.write_protect(addr, page_size);
            if rules().write_protect_before_writable {
                self.host.protect(addr, page_size, writable);
            }
        } else {
            self.host.map(addr, page_size, writable, self.base_memory(addr));
            self.host.register(addr, page_size, UFFDIO_REGISTER_MODE_MISSING);
        }
        self.host.wake(addr, page_size);
    }

    /// cladefs: leaves the whole window out, as a window that maps nothing yet and faults nothing
    /// yet, made or about to copy on write, is: its pages' changes as they begin, each mapping
    /// nothing, none written, where an unmap of the window counted a change of each page, which
    /// took the whole window's changes' memory, 768 MiB of a 64 GiB window's (TASK-227).
    fn leave_out_untouched(&self) {
        debug_assert!(
            self.pages.lock().unwrap().runs.is_empty(),
            "a window left out untouched maps runs"
        );
        self.leave_out(self.base, self.size);
    }

    /// cladefs: its window copies on write (backlog doc-13), from now on, holding `most` private
    /// pages at most, one at least, as many as it has pages at most (doc-26: the VM process bounds
    /// them, not the daemon): told before the window is made, as it faults nothing before then.
    /// Its memory, all of it, is its private memory from now on, none of it filled, which faults
    /// as the anonymous memory it replaces did (see the module docs).
    pub fn copy_on_write(&self, most: u64) -> Result<(), Error> {
        let pages = (self.size as u64) / page_size();
        if most == 0 || most > pages {
            return Err(Error::new(Errno::EINVAL));
        }
        let fd = self.host.private_memory(self.size).map_err(Error::new)?;
        if self.private.set(PrivateMemory { fd, most }).is_err() {
            return Err(Error::msg("the window copies on write already"));
        }
        self.leave_out_untouched();
        Ok(())
    }

    /// The runs of pages it keeps installed at most, with the pages a fault restricted: its share
    /// of its process's windows' ([`Mappings`]). cladefs: which its VM process tells the daemon of
    /// (TASK-111.5), as the window is made, and again as it changes ([`Self::on_shared`]).
    pub fn budget(&self) -> usize {
        self.share.load(Ordering::Relaxed)
    }

    /// cladefs: has `told` called each time it is told its share anew from now on, as another
    /// window of its process comes or goes ([`Mappings`]): outside the mappings' lock, and any of
    /// its own (board 10098). Once.
    pub fn on_shared(&self, told: impl Fn() + Send + Sync + 'static) {
        let set = self.on_shared.set(Box::new(told));
        assert!(set.is_ok(), "a window's share told of twice over");
    }

    /// cladefs: the runs of pages it has installed now, each a mapping of the process's, which
    /// [`budget`](Self::budget) bounds (TASK-111.5).
    pub fn installed(&self) -> u64 {
        self.pages.lock().unwrap().runs.len() as u64
    }

    /// cladefs: how many of its pages are private (backlog doc-13): none unless it copies on write.
    /// A snapshot of its VM is refused while any is (doc-26), and its daemon's metrics show them.
    pub fn private_pages(&self) -> u64 {
        self.pages.lock().unwrap().private
    }

    /// cladefs: the runs of private pages it made so far, each at once, their pages, and the
    /// nanoseconds they took it, which its VM process tells the daemon of (`STAGE_PRIVATE_COUNTS`,
    /// TASK-164.10).
    pub fn private_costs(&self) -> [u64; 3] {
        self.copies.each_ref().map(|count| count.load(Ordering::Relaxed))
    }

    /// cladefs: makes every write the guest's, whoever's: a VM's memory's window (TASK-456.3),
    /// which the VMM's threads write too, restoring devices and emulating them, and whose pages its
    /// memory cell installs ahead of the guest's touches, a tree node of them as its first page
    /// faults (`memory_cell`): a load of a page installed takes no fault here, KVM finding it
    /// present, so a write that faults is the guest's but on that first page, which KVM may hand to
    /// x86's asynchronous page fault worker, whose fault is a write whatever the vCPU's access, a
    /// load's too (docs/research/kvm-async-pf-and-shared-guest-memory.md). That page is made
    /// private, at most one of each node's, rather than restricted for the worker until the vCPU's
    /// own access says what it was: lifted at any vCPU's failure, restrictions raced the worker's
    /// retry, which made nearly every page a guest touched private, and left a write refused until
    /// its VMM gave up (exit 159).
    pub fn writes_are_guests(&self) {
        self.process_writes.store(true, Ordering::Relaxed);
    }

    /// cladefs: whether thread `thread`'s write is the guest's: its own, or any write of a VM's
    /// memory's window's ([`Self::writes_are_guests`]).
    fn guest_writes(&self, thread: u32) -> bool {
        is_guests(thread) || self.process_writes.load(Ordering::Relaxed)
    }

    /// cladefs: whether its window copies on write (backlog doc-13).
    pub fn copying(&self) -> bool {
        self.private.get().is_some()
    }

    /// cladefs: the time from its start, in nanoseconds.
    fn now(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// cladefs: the memory where the window installs nothing, at `addr`: its private memory, if it
    /// copies on write (see the module docs), or anonymous memory.
    fn base_memory(&self, addr: usize) -> Memory {
        match self.private.get() {
            Some(private) => {
                let offset = (addr - self.base) as u64;
                Memory::File { fd: private.fd.as_raw_fd(), offset, shared: true }
            }
            None => Memory::Anonymous,
        }
    }

    /// cladefs: empties the private pages of `runs`, ranges of pages that went: the memory around
    /// them, mapped there anew, faults again, as their private memory is unfilled.
    fn empty_private(&self, runs: &[std::ops::Range<usize>]) {
        let Some(private) = self.private.get() else { return };
        if !rules().private_emptied_as_it_goes {
            return;
        }
        let page_size = page_size() as usize;
        for run in runs {
            let offset = (run.start * page_size) as u64;
            self.host.punch(private.fd.as_raw_fd(), offset, run.len() * page_size);
        }
    }

    /// cladefs: takes away from the memory of the `len` bytes at `addr` what a change of them ends,
    /// `taking` ([`Pages::take_away`]), leaving each page as one left out is: memory where nothing
    /// is installed, accessible, registered for missing faults, unfilled. What maps nothing taken
    /// takes nothing: a page never touched since the window named it, as it installs nothing then
    /// ([`SecondStage::map`]), changes with no system call at all.
    ///
    /// Pages installed are left out with the rest, in one `mmap`, which flushes their TLB entries
    /// once, and the private pages among them emptied after it, while nothing maps them, which
    /// flushes nothing (see the module docs); a run of private pages with nothing installed is
    /// emptied in place, the window's memory there already, one flush. Each emptied before the
    /// `mmap`, as they once were, took a flush of its own, and of the mapping it was filled through
    /// another: a guest's `UNMAP` of a range of private pages scattered among pages installed, as
    /// a fork's database's are, took two TLB flushes a private page (TASK-276.6).
    fn take_away(&self, addr: usize, len: usize, taking: Taking) {
        let Taking { installed, private, protected, newly_staled, keep_stale, stale_ended } =
            taking;
        let page_size = page_size() as usize;
        if keep_stale.is_empty() {
            if installed || private.len() > 1 || stale_ended {
                self.leave_out_emptying(addr, len, &private);
                return;
            }
            self.empty_private(&private);
            for at in protected {
                let writable = libc::PROT_READ | libc::PROT_WRITE;
                self.host.protect(self.base + at * page_size, page_size, writable);
            }
            return;
        }
        // At least one host page in `addr..addr+len` remains stale (partially private): never
        // overwrite its unfilled anonymous VMA with `base_memory` (`PrivateMemory`).
        let range = self.page_range(addr, len);
        let mut at = range.start;
        while at < range.end {
            if keep_stale.binary_search(&at).is_ok() {
                at += 1;
                continue;
            }
            let sub_start = at;
            while at < range.end && keep_stale.binary_search(&at).is_err() {
                at += 1;
            }
            let sub_private: Vec<std::ops::Range<usize>> = private
                .iter()
                .filter_map(|run| {
                    let s = run.start.max(sub_start);
                    let e = run.end.min(at);
                    (s < e).then_some(s..e)
                })
                .collect();
            let sub_addr = self.base + sub_start * page_size;
            let sub_len = (at - sub_start) * page_size;
            if installed || sub_private.len() > 1 || stale_ended {
                self.leave_out_emptying(sub_addr, sub_len, &sub_private);
            } else {
                self.empty_private(&sub_private);
                for &p in &protected {
                    if (sub_start..at).contains(&p) {
                        let writable = libc::PROT_READ | libc::PROT_WRITE;
                        self.host.protect(self.base + p * page_size, page_size, writable);
                    }
                }
            }
        }
        for at in newly_staled {
            self.mark_stale_vma(self.base + at * page_size);
        }
    }

    /// The index of the page at `addr`, and of the pages of the `len` bytes there.
    fn page_range(&self, addr: usize, len: usize) -> std::ops::Range<usize> {
        let page_size = page_size() as usize;
        let first = (addr - self.base) / page_size;
        first..first + len.div_ceil(page_size)
    }

    /// Makes the window's change to the `len` bytes at `addr`: `record`s it in the pages, and
    /// makes it to the memory with `change`, given what `record` returns, holding the pages' lock,
    /// and counts it for each page, which is withheld no more: the change replaces its memory.
    /// cladefs: returns whether it did: not once the memory was let go of ([`Self::release`]).
    /// First makes room in its share for `room` runs more, as many as the change may split
    /// (board 8493): one each of the ranges it takes away, a run reaching past both its ends.
    fn change<T>(
        &self,
        addr: usize,
        len: usize,
        room: usize,
        record: impl FnOnce(&mut Pages) -> T,
        change: impl FnOnce(T),
    ) -> bool {
        let mut pages = self.pages.lock().unwrap();
        if pages.released {
            return false;
        }
        self.trim(&mut pages, room, None);
        let now = self.now();
        let recorded = record(&mut pages);
        for page in &mut pages.pages[self.page_range(addr, len)] {
            page.changed = now;
        }
        if !rules().restrict_between_changes {
            drop(pages);
            change(recorded);
            return true;
        }
        self.made_locked(&mut pages, addr, len, now, || change(recorded));
        true
    }

    /// cladefs: makes `change` to the memory of the `len` bytes at `addr`, holding the pages'
    /// lock, recorded already, counting it for each page, which is withheld no more, as
    /// [`Self::change`] does.
    fn made_locked(
        &self,
        pages: &mut Pages,
        addr: usize,
        len: usize,
        now: u64,
        change: impl FnOnce(),
    ) {
        pages.update(self.page_range(addr, len), |pages| {
            for page in pages {
                page.made = page.made.wrapping_add(1);
                page.withheld = false;
            }
        });
        let _busy = self.busy.doing(Doing::Change, now);
        change();
    }

    /// cladefs: lets go of the memory, as the connection to the daemon whose pages it maps goes,
    /// after which the daemon frees them, for other VMs' extents (TASK-79.8): all of it replaced
    /// with memory nothing accesses, whose faults userfaultfd reports not, the guest's or this
    /// process's; and no change made to it from now on, as the thread receiving from the daemon
    /// may still make those it sent before it went.
    pub fn release(&self) {
        let mut pages = self.pages.lock().unwrap();
        pages.released = true;
        // cladefs: each page mapping nothing, as a window's pages begin: memory of zeros again,
        // where writing each page's changes took the whole window's (TASK-227). Its changes
        // counted from 0 again, a fault that saw others finds them changed.
        let len = pages.pages.len();
        match WindowPages::new(len) {
            Ok(fresh) => {
                (
                    pages.pages,
                    pages.private,
                    pages.restrictions,
                    pages.anon_runs,
                    pages.wp_partials,
                ) = (fresh, 0, 0, 0, 0);
                pages.runs.clear();
                pages.for_workers.clear();
                pages.restricted_order.clear();
            }
            Err(err) => {
                log::warn!("cladefs: a released DAX window's pages, anew: {err}: each changed");
                pages.update(0..len, |pages| {
                    pages.iter_mut().for_each(|page| {
                        (
                            page.mapped,
                            page.restricted,
                            page.private_mask,
                            page.mapped_mask,
                            page.stale,
                            page.anon_vma,
                            page.wp_partial,
                        ) = (None, false, 0, 0, false, false, false)
                    })
                });
                pages.private = 0;
            }
        }
        self.host.map(self.base, self.size, libc::PROT_NONE, Memory::Anonymous);
    }

    /// What a fault finds of the page at `addr`, which a restriction of the page checks: the
    /// changes the window has made to it, as the rules have restrictions check them.
    fn seen(&self, addr: usize) -> Option<u32> {
        let page = self.page_range(addr, 1).start;
        rules().restrict_between_changes.then(|| self.pages.lock().unwrap().pages[page].made)
    }

    /// Restricts the page at `addr` to `prot`, for a fault the window did not resolve, and so
    /// withholds it if it is made inaccessible: unless the window changed it since it had made
    /// `seen` changes to it, whose new memory the access finds when it retries. Without `seen`, at
    /// once. cladefs: in room made for it in its share, as it splits the mapping it is in
    /// (board 8493).
    fn restrict(&self, addr: usize, seen: Option<u32>, prot: libc::c_int) {
        let page_size = page_size() as usize;
        let page = self.page_range(addr, 1).start;
        let withheld = prot == libc::PROT_NONE;
        let offset = (addr - self.base) as u64;
        let mut pages = self.pages.lock().unwrap();
        let made = pages.pages[page].made;
        if pages.released || seen.is_some_and(|seen| made != seen) {
            return;
        }
        self.trim(&mut pages, 1, Some(page));
        self.host.protect(addr, page_size, prot);
        Self::restricted(&mut pages, page, |page| {
            page.withheld = withheld;
            page.restricted = prot == libc::PROT_READ;
            page.for_worker = false;
        });
        let what = if withheld { "withheld" } else { "restricted to reads" };
        changed_protection(what, offset, made, seen);
    }

    /// cladefs: restricts page `at` with `restrict`, which a fault's restriction of it makes: listed
    /// to be lifted, oldest first, if it restricted nothing before ([`Self::trim`]). The list keeps
    /// pages lifted since until it is twice as long as there are restricted, and each but once.
    fn restricted(pages: &mut Pages, at: usize, restrict: impl FnOnce(&mut PageChanges)) {
        let before = pages.pages[at].restricts();
        pages.update(at..at + 1, |page| restrict(&mut page[0]));
        if before || !pages.pages[at].restricts() {
            return;
        }
        pages.restricted_order.push_back(at);
        if pages.restricted_order.len() > 2 * pages.restrictions + 64 {
            let (mut listed, mut order) = (BTreeSet::new(), VecDeque::new());
            for &at in pages.restricted_order.iter().rev() {
                if pages.pages[at].restricts() && listed.insert(at) {
                    order.push_front(at);
                }
            }
            pages.restricted_order = order;
        }
    }

    /// cladefs: restricts the page at `addr` to reads for a worker's write, resolved as a load, on
    /// a window that copies on write (see the module docs), if the window maps it here, read-only,
    /// so that the worker's write fails; until a guest's access fails ([`guest_access_failed`]). A
    /// private page takes the write.
    fn restrict_for_worker(&self, addr: usize) {
        if !rules().worker_writes_restricted {
            return;
        }
        let page_size = page_size() as usize;
        let at = self.page_range(addr, 1).start;
        let mut pages = self.pages.lock().unwrap();
        let installed = pages.pages[at].mapped.is_some_and(|mapped| mapped.installed());
        if pages.released || !installed {
            return;
        }
        // cladefs: in room made for it in its share, its page's run kept (board 8493).
        self.trim(&mut pages, 1, Some(at));
        self.host.protect(addr, page_size, libc::PROT_READ);
        let Pages { pages: all, for_workers, .. } = &mut *pages;
        // Pages unrestricted since stay listed till a lift, of which a VM may take none: dropped
        // once the list is as long as the window, each page listed once while it is restricted.
        if for_workers.len() >= all.len() {
            for_workers.retain(|&at| all[at].restricted && all[at].for_worker);
        }
        if !(all[at].restricted && all[at].for_worker) {
            for_workers.push(at);
        }
        Self::restricted(&mut pages, at, |page| (page.restricted, page.for_worker) = (true, true));
        let (offset, made) = ((addr - self.base) as u64, pages.pages[at].made);
        changed_protection("restricted to reads for a worker's write", offset, made, None);
    }

    /// cladefs: lifts the restrictions made for workers' writes ([`Self::restrict_for_worker`]), as
    /// a guest's access failed ([`guest_access_failed`]): the page is writable here again, and
    /// write-protected still, so that a write to it faults to the window.
    pub fn lift_worker_restrictions(&self) {
        if !rules().failures_lift_worker_restrictions {
            return;
        }
        let page_size = page_size() as usize;
        let mut pages = self.pages.lock().unwrap();
        if pages.released {
            return;
        }
        for at in std::mem::take(&mut pages.for_workers) {
            let page = pages.pages[at];
            if !(page.restricted && page.for_worker) {
                continue;
            }
            pages.update(at..at + 1, |page| {
                (page[0].restricted, page[0].for_worker) = (false, false)
            });
            let writable = libc::PROT_READ | libc::PROT_WRITE;
            self.host.protect(self.base + at * page_size, page_size, writable);
            let offset = (at * page_size) as u64;
            changed_protection("lifted, a guest's access having failed", offset, page.made, None);
        }
    }

    /// cladefs: the changes made so far that took access away from pages installed, by how
    /// ([`Taken`]).
    pub fn taken(&self) -> TakenCounts {
        let taken = &self.taken;
        [&taken.overwritten, &taken.left_out].map(|count| count.load(Ordering::Relaxed))
    }

    /// cladefs: counts a change that took access away from pages installed, `count` by how.
    fn took(count: &AtomicU64, took: bool) {
        if took {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The size of the memory: its window's, as a test's window in it takes it
    /// ([`Dax::in_host_memory`](crate::dax::Dax::in_host_memory)), or a VM process's of the
    /// extent daemon's.
    pub fn size(&self) -> u64 {
        self.size as u64
    }

    /// Resolves the fault at host address `address` described by `flags`, and wakes the faulting
    /// thread.
    pub async fn resolve(&self, window: &dyn Faults, address: usize, flags: u64, thread: u32) {
        let page_size = page_size() as usize;
        let addr = address - address % page_size;
        let offset = (addr - self.base) as u64;
        let access = if flags & (UFFD_PAGEFAULT_FLAG_WRITE | UFFD_PAGEFAULT_FLAG_WP) != 0 {
            Access::Write
        } else {
            Access::Read
        };
        // A write to a page mapped here read-only is the guest's: KVM faults loads of pages mapped
        // here without userfaultfd. A fault on a page not mapped here may be for writing whatever
        // the guest's access (see the module docs), and resolves that page alone.
        let guest_write = flags & UFFD_PAGEFAULT_FLAG_WP != 0;
        // cladefs: a worker's write, on a window that copies on write, which a load may have made,
        // is resolved as a load, and the page restricted to reads here once it is (see the module
        // docs): of a page not here, and of one here, write-protected, which the vCPU's load found
        // not here, and another fault, a probe or a map installed before the worker ran. That one,
        // taken as the guest's, made a private page the guest never counted, and its frozen VM's
        // snapshot was refused (TASK-191, GCE r685, r686, r688).
        let for_worker = access == Access::Write && self.copying() && !self.guest_writes(thread);
        // cladefs: a write of the guest's own, on a window that copies on write, is copied here,
        // from what the window names at the page, under the lock its changes take (doc-26, spec
        // rule copyUnderLock): the page made private in place of nothing if it isn't here, rather
        // than installed first, to be stored to and replaced by its private copy, a TLB flush
        // (backlog doc-13: a write's first touch, TASK-276.6). No probe follows: the guest's
        // writes there take private pages whether their pages are here or not, a fault either way,
        // and the probe, made once the faulting thread is woken, installed the pages after it
        // read-only for a moment (`install`) as that thread stored on to them, which a vCPU's VMM
        // retries and a thread of the host's dies of: SIGSEGV in
        // snapshots_after_private_pages_are_rebound_restore_their_bytes, 3 runs in 500.
        // If a private host page is stale (non-private sub-pages changed by MAP/UNMAP, or
        // coalesced by trim), refresh its non-private sub-pages from the daemon before mapping
        // PrivateMemory back in.
        let promote_on_write = access == Access::Write && self.copying() && !for_worker;
        if self.is_stale(addr) {
            self.refresh_stale(window, addr, promote_on_write).await;
            if for_worker {
                self.restrict_for_worker(addr);
            }
            self.host.wake(addr, page_size);
            return;
        }
        if access == Access::Write && self.copying() && !for_worker {
            self.copy_for(window, addr, thread).await;
            self.host.wake(addr, page_size);
            return;
        }
        // A page the window maps, left out past the budget, or named and never touched, is
        // installed, as the window names it, without the window; any other fault is the window's
        // to resolve.
        let reinstalled = match if guest_write { Reinstalled::Miss } else { self.reinstall(addr) } {
            Reinstalled::Done => true,
            Reinstalled::Stale => {
                self.refresh_stale(window, addr, promote_on_write).await;
                if for_worker {
                    self.restrict_for_worker(addr);
                }
                self.host.wake(addr, page_size);
                return;
            }
            Reinstalled::Miss => false,
        };
        if !reinstalled {
            self.resolve_through(window, addr, access, for_worker, thread).await;
            // cladefs: what the window named for the fault installed now, which the access,
            // retried, finds, rather than faulting again to install it (TASK-276.6).
            if !guest_write && matches!(self.reinstall(addr), Reinstalled::Stale) {
                self.refresh_stale(window, addr, promote_on_write).await;
                if for_worker {
                    self.restrict_for_worker(addr);
                }
                self.host.wake(addr, page_size);
                return;
            }
        }
        // Before the worker retries its write, however its page came to be here. cladefs: one
        // installed again, as it was, write-protected, and left writable, took the retried write
        // as the guest's, which copied the page for a load, uncounted by the guest, past its
        // private pages: a jailed VM process, its budget the default's for want of /proc, left out
        // pages by the thousand, 21,396 copied so, and the guest's writes were refused (GCE r582,
        // TASK-170).
        if for_worker {
            self.restrict_for_worker(addr);
        }
        self.host.wake(addr, page_size);
        // A write to a page not mapped here may start a run a sequential writer writes to: the
        // pages after it are mapped read-only, the next read in if need be, so that writes to them
        // fault as writes, which dirty runs of pages, and loads do not fault at all. Past the file,
        // or the window, there is nothing to map. cladefs: a probe, whose page the guest may not
        // map, which the window refuses then without telling of it as of the guest's access; on a
        // window that copies on write, a worker's write alone, which a run of loads makes.
        let next = offset + page_size as u64;
        // cladefs: the next page, if any, probed ahead on writes as above.
        if !reinstalled && !guest_write && access == Access::Write && next < window.size() {
            match window.probe(next).await {
                // cladefs: installed, as the window names it now (TASK-276.6).
                Ok(()) => {
                    // Best-effort read-ahead probe: a miss or stale page resolves on the next access.
                    let _ = self.reinstall(addr + page_size);
                }
                Err(Errno::ENOENT | Errno::EINVAL) => {}
                Err(err) => log::warn!("mapping DAX window offset {next:#x} ahead: {err}"),
            }
        }
    }

    /// Resolves the fault at `addr`, an `access` of `thread`'s, through `window`, as
    /// [`resolve`](Self::resolve) does all but a page left out, and a guest's write on a window
    /// that copies on write: a worker's write as a load, `for_worker`. A fault the window refuses
    /// restricts the page as the access found it; one it can't resolve aborts the process, as its
    /// faulting thread can't go on.
    async fn resolve_through(
        &self,
        window: &dyn Faults,
        addr: usize,
        access: Access,
        for_worker: bool,
        thread: u32,
    ) {
        let offset = (addr - self.base) as u64;
        // The page as the access found it, which a fault the window does not resolve restricts.
        let seen = self.seen(addr);
        // cladefs: a write to a window that doesn't copy on write, a test's, is refused, as
        // every page is mapped read-only: here, where the window names the page; once it has read
        // it in, where it names nothing yet, as the window is asked only to read (doc-26).
        let resolved = match access == Access::Write && !for_worker && self.named(addr) {
            true => Err(Errno::EACCES),
            false => match window.fault_page(offset).await {
                Ok(()) if access == Access::Write && !for_worker && self.named(addr) => {
                    Err(Errno::EACCES)
                }
                resolved => resolved,
            },
        };
        let resolved = match resolved {
            // Where the window refuses a write, the page is mapped read-only: make it read-only
            // here too, so that the write fails in the kernel rather than faulting here again. A
            // write from the guest then fails as KVM's would.
            Err(Errno::EACCES) if access == Access::Write => {
                refused("a write to a page the window maps read-only", offset, thread);
                if rules().refused_write_read_only {
                    self.restrict(addr, seen, libc::PROT_READ);
                }
                Ok(())
            }
            // Where the window maps nothing, the fault is KVM's, for a page the guest has
            // unmapped (see the module docs): it is withheld, so that the fault fails.
            Err(Errno::ENOENT) => {
                refused("an access to a page the window maps nothing at", offset, thread);
                self.restrict(addr, seen, libc::PROT_NONE);
                Ok(())
            }
            resolved => resolved,
        };
        if let Err(err) = resolved {
            self.unresolved(window, offset, access, err).await;
        }
    }

    /// cladefs: the fault at `offset`, an `access`, which `window` failed with `err`: the process
    /// waits to stop, if what resolves the faults went; else it aborts, as its faulting thread
    /// can't go on.
    async fn unresolved(&self, window: &dyn Faults, offset: u64, access: Access, err: Errno) {
        // cladefs: failed as what resolves the window's faults went, the daemon: the VM process is
        // stopping, CladefsdLost, as its device was told, and its faulting thread waits for that,
        // which aborting would beat, the VM dying of SIGABRT, where a VM whose daemon goes exits
        // 158 (doc-12; fxfs-guest-soak, GCE r830 round 27; TASK-231).
        if window.lost() {
            log::warn!("DAX window fault at offset {offset:#x} unresolved: the extent daemon went");
            std::future::pending::<()>().await;
        }
        let why = format!("unresolved DAX window fault at offset {offset:#x} ({access:?}): {err}");
        log::error!("{why}");
        // cladefs: on standard error too, which a process without a logger, a test's, keeps: its
        // abort said nothing else (TASK-100).
        eprintln!("cladefs: {why}");
        std::process::abort();
    }

    /// cladefs: whether the window names something at the page at `addr`, here.
    fn named(&self, addr: usize) -> bool {
        let at = self.page_range(addr, 1).start;
        self.pages.lock().unwrap().pages[at].mapped.is_some()
    }

    /// cladefs: whether the page at `addr` is a stale partially-private host page whose
    /// non-private sub-pages must be refreshed before mapping [`PrivateMemory`] back in.
    fn is_stale(&self, addr: usize) -> bool {
        let at = self.page_range(addr, 1).start;
        let pages = self.pages.lock().unwrap();
        let page = &pages.pages[at];
        !pages.released && page.is_private() && page.stale
    }

    /// cladefs: refreshes the non-private sub-pages of a stale partially-private host page at
    /// `addr` from what the daemon maps there (bringing it in through `window` if not yet named),
    /// preserving every private sub-page in [`PrivateMemory`], and remaps [`PrivateMemory`] back
    /// into the host VMA.
    async fn refresh_stale(&self, window: &dyn Faults, addr: usize, promote_on_write: bool) {
        let page_size = page_size() as usize;
        let at = self.page_range(addr, 1).start;
        let offset = (addr - self.base) as u64;
        let all_mask = self.all_subpages_mask();
        loop {
            {
                let mut pages = self.pages.lock().unwrap();
                if pages.released || !pages.pages[at].is_private() {
                    return;
                }
                if !pages.pages[at].stale {
                    // Refreshed in place by `STAGE_MAP` while we waited; if its VMA is still
                    // `Memory::Anonymous`, remap `PrivateMemory` now so the access completes.
                    if pages.pages[at].anon_vma {
                        self.restore_coalesced_private(&mut pages, at, promote_on_write);
                    }
                    return;
                }
                if pages.pages[at].private_mask == all_mask {
                    self.apply_stale_refresh(&mut pages, at, None, false);
                    return;
                }
                if let Some(mapped) = pages.pages[at].mapped {
                    let mut bytes = vec![0u8; page_size];
                    self.read_page(mapped.phys, &mut bytes);
                    self.apply_stale_refresh(&mut pages, at, Some(&bytes), promote_on_write);
                    return;
                }
            }
            match window.fault_page(offset).await {
                Ok(()) => {}
                Err(Errno::ENOENT) => {
                    let mut pages = self.pages.lock().unwrap();
                    if pages.released || !pages.pages[at].is_private() {
                        return;
                    }
                    if !pages.pages[at].stale {
                        if pages.pages[at].anon_vma {
                            self.restore_coalesced_private(&mut pages, at, promote_on_write);
                        }
                        return;
                    }
                    if pages.pages[at].mapped.is_none() {
                        self.apply_stale_refresh(&mut pages, at, None, promote_on_write);
                        return;
                    }
                }
                Err(err) => return self.unresolved(window, offset, Access::Read, err).await,
            }
        }
    }

    /// Restores a fresh private host page at `at` whose VMA was temporarily replaced with
    /// `Memory::Anonymous` (either by [`Self::coalesce_anon_vmas`] or after an in-place
    /// `PrivateMemory` refresh): reserves VMA budget first (`trim`), then remaps [`PrivateMemory`]
    /// directly without any daemon round-trip (review 102 findings 349 & 350).
    fn restore_coalesced_private(&self, pages: &mut Pages, at: usize, promote_on_write: bool) {
        self.trim(pages, 1, Some(at));
        let page_size = page_size() as usize;
        let addr = self.base + at * page_size;
        let all_mask = self.all_subpages_mask();
        let mask = pages.pages[at].private_mask;
        let process_writes = self.process_writes.load(Ordering::Relaxed);
        let next_mask = if promote_on_write && process_writes {
            let mapped = pages.pages[at].mapped_mask;
            mask | if mapped != 0 { mapped } else { all_mask }
        } else {
            mask
        };
        let want_wp = process_writes && next_mask != all_mask;
        let now = self.now();
        pages.update(at..at + 1, |page| {
            page[0].anon_vma = false;
            page[0].private_mask = next_mask;
            page[0].wp_partial = want_wp;
            page[0].restricted = false;
            page[0].changed = now;
        });
        self.made_locked(pages, addr, page_size, now, || {
            self.map_private_vma(addr, want_wp);
        });
    }

    /// Overwrites only the non-private sub-pages of host page `at` in [`PrivateMemory`] from
    /// `fresh` (or zeros if `None`, when all non-private sub-pages are unmapped), clears `stale`,
    /// and maps [`PrivateMemory`] back in place of the unfilled anonymous VMA.
    ///
    /// On a `process_writes` window (`MemoryCell`, which has no `PRIVATE` RPC and relies on
    /// write faults), a write fault promotes `private_mask` to all mapped sub-pages while a read
    /// fault maps `PrivateMemory` write-protected (`wp_partial = true`) so any subsequent store
    /// to a reclaimed sub-page traps and promotes `private_mask`.
    fn apply_stale_refresh(
        &self,
        pages: &mut Pages,
        at: usize,
        fresh: Option<&[u8]>,
        promote_on_write: bool,
    ) {
        // Reserve VMA budget before splitting any coalesced anonymous span (review 102 finding 350).
        self.trim(pages, 1, Some(at));
        let page_size = page_size() as usize;
        let addr = self.base + at * page_size;
        let page_offset = (at * page_size) as u64;
        let private = self.private.get().expect("a stale page of a window that doesn't copy");
        let fd = private.fd.as_raw_fd();
        let all_mask = self.all_subpages_mask();
        let mask = pages.pages[at].private_mask;
        let process_writes = self.process_writes.load(Ordering::Relaxed);
        let next_mask = if promote_on_write && process_writes {
            let mapped = pages.pages[at].mapped_mask;
            mask | if mapped != 0 { mapped } else { all_mask }
        } else {
            mask
        };
        let want_wp = process_writes && next_mask != all_mask;
        let zeros = fresh.is_none().then(|| vec![0u8; self.unit]);
        let now = self.now();
        pages.update(at..at + 1, |page| {
            page[0].stale = false;
            page[0].anon_vma = false;
            page[0].private_mask = next_mask;
            page[0].wp_partial = want_wp;
            page[0].mapped = None;
            page[0].restricted = false;
            page[0].changed = now;
        });
        self.made_locked(pages, addr, page_size, now, || {
            for slot in 0..self.subpages() {
                if mask & (1u16 << slot) != 0 {
                    continue;
                }
                let within = slot * self.unit;
                let bytes = match fresh {
                    Some(buf) => &buf[within..within + self.unit],
                    None => zeros.as_deref().unwrap(),
                };
                self.host.write_subpage(fd, page_offset + within as u64, bytes);
            }
            self.map_private_vma(addr, want_wp);
        });
    }

    /// cladefs: the guest's write to the page at `addr`, of `thread`'s, on a window that copies on
    /// write (doc-26): the page copied here ([`Self::copy`]), once `window` has read it in, a read,
    /// where it names nothing yet, and again if it is evicted or unmapped meanwhile; refused past
    /// the window's private pages, as one where the window maps nothing is, which its VM stops for.
    async fn copy_for(&self, window: &dyn Faults, addr: usize, thread: u32) {
        let offset = (addr - self.base) as u64;
        loop {
            let seen = self.seen(addr);
            match self.copy(addr) {
                Copied::Private => return,
                Copied::Stale => {
                    self.refresh_stale(window, addr, true).await;
                    return;
                }
                Copied::Past => {
                    refused("a write past the window's private pages", offset, thread);
                    if rules().refused_write_read_only {
                        self.restrict(addr, seen, libc::PROT_READ);
                    }
                    return;
                }
                Copied::Unnamed => {}
            }
            match window.fault_page(offset).await {
                Ok(()) => {}
                Err(Errno::ENOENT) => {
                    refused("an access to a page the window maps nothing at", offset, thread);
                    self.restrict(addr, seen, libc::PROT_NONE);
                    return;
                }
                Err(err) => return self.unresolved(window, offset, Access::Write, err).await,
            }
        }
    }

    /// cladefs: makes the page at `addr` private for a guest's write (doc-26), unless it is
    /// already: its private memory filled with what the page the window names there holds, read
    /// from its arena, and mapped writable in its place ([`Self::privatize_masked`]), all under
    /// the pages' lock, which the window's changes take: a change taking the page away, an evict
    /// whose acknowledgement the daemon frees it after, comes after the copy (spec rule
    /// copyUnderLock). Not past the window's private pages, nor of a page it names nothing at.
    fn copy(&self, addr: usize) -> Copied {
        let page_size = page_size() as usize;
        let at = self.page_range(addr, 1).start;
        let most = self.private.get().expect("a window that copies on write").most;
        let all_mask = self.all_subpages_mask();
        let mut pages = self.pages.lock().unwrap();
        if pages.released {
            return Copied::Private;
        }
        if pages.pages[at].is_private() {
            if pages.pages[at].stale {
                return Copied::Stale;
            }
            if pages.pages[at].anon_vma {
                self.restore_coalesced_private(&mut pages, at, true);
                return Copied::Private;
            }
            if pages.pages[at].wp_partial {
                let mapped = pages.pages[at].mapped_mask;
                pages.update(at..at + 1, |page| {
                    page[0].private_mask |= if mapped != 0 { mapped } else { all_mask };
                    page[0].wp_partial = false;
                });
                self.host.write_unprotect(addr, page_size);
            }
            return Copied::Private;
        }
        let Some(mapped) = pages.pages[at].mapped else { return Copied::Unnamed };
        if pages.private >= most {
            return Copied::Past;
        }
        let mut bytes = vec![0u8; page_size];
        if !rules().copy_under_lock {
            // Read with the lock let go, as a change may take the page away meanwhile.
            drop(pages);
            self.read_page(mapped.phys, &mut bytes);
            pages = self.pages.lock().unwrap();
            if pages.released {
                return Copied::Private;
            }
            if pages.pages[at].is_private() {
                if pages.pages[at].stale {
                    return Copied::Stale;
                }
                if pages.pages[at].anon_vma {
                    self.restore_coalesced_private(&mut pages, at, true);
                    return Copied::Private;
                }
                if pages.pages[at].wp_partial {
                    let mapped_mask = pages.pages[at].mapped_mask;
                    pages.update(at..at + 1, |page| {
                        page[0].private_mask |=
                            if mapped_mask != 0 { mapped_mask } else { all_mask };
                        page[0].wp_partial = false;
                    });
                    self.host.write_unprotect(addr, page_size);
                }
                return Copied::Private;
            }
            if pages.private >= most {
                return Copied::Past;
            }
        } else {
            self.read_page(mapped.phys, &mut bytes);
        }
        // A write fault carries only the host-page address, so mark every mapped sub-page private
        // (never unmapped sub-pages, which would otherwise leak `private_pages()` on UNMAP).
        let mask = pages.pages[at].mapped_mask;
        if mask == 0 {
            return Copied::Unnamed;
        }
        self.privatize_masked(&mut pages, at, &bytes, &[mask]);
        drop(pages);
        if let Some(prefault) = &self.prefault {
            prefault.prefault((addr - self.base) as u64, page_size as u64);
        }
        Copied::Private
    }

    /// cladefs: reads the page at `phys`, which the window names, into `bytes`: the shared zeros
    /// page's too, a page of the default arena as any other (doc-26).
    fn read_page(&self, phys: crate::pages::PhysLocation, bytes: &mut [u8]) {
        let (fd, _) = self.memories.fd(phys.arena).expect("a window's page is of its arenas");
        read_at(fd, bytes, phys.offset);
    }

    /// cladefs: makes the pages from page `first` on private, one for each page of `bytes`, which
    /// they hold, with the pages' lock held (`pages`): their private memory filled, then mapped in
    /// place of what was there, as the memory around them is, with which it merges (see the module
    /// docs), accessible as soon as it is, as nothing about them faults any more. None of them is
    /// private yet.
    ///
    /// The run at once (TASK-164.10): one fill, one map, registration and wake, where a page each
    /// took them all, a `PRIVATE`'s pages 65 us each nested, the VM process's receiving thread a
    /// fifth of a CPU in the kernel under a forked pgbench (GCE r649). And a run with no page
    /// installed, withheld or restricted maps nothing: the memory there is its private memory,
    /// accessible and registered for missing faults, as a page left out is, which its filling
    /// makes the page's; the map over it unmapped it only to map it again, as the pages a guest
    /// writes and never read, a database's log's, are. Pages the window names are installed only
    /// as the guest first touches them ([`SecondStage::map`]), so a run the guest writes before it
    /// reads it is such a run: no `mmap`, and no TLB flush (TASK-276.6).
    fn privatize_masked(&self, pages: &mut Pages, first: usize, bytes: &[u8], masks: &[u16]) {
        let start = std::time::Instant::now();
        // cladefs: room for the run it may split (board 8493).
        self.trim(pages, 1, None);
        let page_size = page_size() as usize;
        let range = first..first + bytes.len() / page_size;
        debug_assert_eq!(range.len(), masks.len());
        let (addr, len) = (self.base + first * page_size, bytes.len());
        let offset = (addr - self.base) as u64;
        let private = self.private.get().expect("a private page of a window that doesn't copy");
        Self::took(&self.taken.overwritten, pages.any_installed(range.clone()));
        let changes = &pages.pages[range.clone()];
        assert!(
            !changes.iter().any(|page| page.is_private()),
            "DAX window offset {offset:#x}, {len:#x} bytes, made private again"
        );
        // As a fault saw it, before the change gives back what was withheld.
        let in_place = changes.iter().all(|page| {
            !page.withheld
                && !page.restricted
                && !page.anon_vma
                && !page.wp_partial
                && !page.mapped.is_some_and(|mapped| mapped.installed())
        });
        pages.update(range.clone(), |pages| {
            for (page, &mask) in pages.iter_mut().zip(masks) {
                debug_assert_ne!(mask, 0);
                (
                    page.mapped,
                    page.restricted,
                    page.private_mask,
                    page.stale,
                    page.anon_vma,
                    page.wp_partial,
                ) = (None, false, mask, false, false, false);
            }
        });
        pages.private += range.len() as u64;
        let now = self.now();
        for page in &mut pages.pages[range] {
            page.changed = now;
        }
        self.made_locked(pages, addr, len, now, || {
            let (memory, writable) = (self.base_memory(addr), libc::PROT_READ | libc::PROT_WRITE);
            let fd = private.fd.as_raw_fd();
            if rules().private_filled_before_mapped {
                self.host.fill(fd, offset, bytes);
            }
            if !in_place {
                self.host.map(addr, len, writable, memory);
                self.host.register(addr, len, UFFDIO_REGISTER_MODE_MISSING);
            }
            if !rules().private_filled_before_mapped {
                self.host.fill(fd, offset, bytes);
            }
            // What faulted on them meanwhile finds them now.
            self.host.wake(addr, len);
        });
        let [runs, made, ns] = &self.copies;
        runs.fetch_add(1, Ordering::Relaxed);
        made.fetch_add((len / page_size) as u64, Ordering::Relaxed);
        ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// cladefs: makes the `len` bytes at `offset` private now, as the guest's writes to each of
    /// their pages would ([`Self::copy`]), mapped writable, so for the guest ahead of its accesses:
    /// doc-13's `PRIVATE`, a `write(2)`'s pages, served here (doc-26), where the daemon made them
    /// once, a round trip each. `ENOMEM`, nothing made private, if they would take the window past
    /// its private pages or eviction keeps them from being named together; `EINVAL` for a window
    /// that doesn't copy on write, a range not within it, or one with a page `window` maps nothing at.
    ///
    /// Accepts `self.unit`-aligned ranges (4 KiB on both 4 KiB and 16 KiB hosts): on hosts with
    /// larger pages, sets only the requested sub-page bits in [`PageChanges::private_mask`], and
    /// refreshes any stale non-private sub-pages first.
    pub async fn make_private(
        &self,
        window: &dyn Faults,
        offset: u64,
        len: u64,
    ) -> Result<(), Errno> {
        let page_size = page_size();
        let unit = self.unit as u64;
        let most = self.private.get().ok_or(Errno::EINVAL)?.most;
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if len == 0
            || !offset.is_multiple_of(unit)
            || !len.is_multiple_of(unit)
            || end > self.size as u64
        {
            return Err(Errno::EINVAL);
        }
        let h_start = offset - offset % page_size;
        let h_end = end.next_multiple_of(page_size);
        let range = self.page_range(self.base + h_start as usize, (h_end - h_start) as usize);
        let (start_off, end_off) = (offset as usize, end as usize);
        let all_mask = self.all_subpages_mask();
        // A range can span more parts than the cache can hold. Retry transient evictions without
        // letting such a request fault the same pages indefinitely.
        const NAME_ATTEMPTS: usize = 4;
        for _ in 0..NAME_ATTEMPTS {
            for at in range.clone() {
                let req = self.subpage_mask(at, start_off, end_off);
                let needs_fault = {
                    let pages = self.pages.lock().unwrap();
                    if pages.released {
                        false
                    } else {
                        let page = &pages.pages[at];
                        // If sub-page presence is already known and excludes any requested slot,
                        // refuse immediately without faulting or leaking private bits.
                        if (page.mapped_mask != 0 || page.is_private())
                            && (page.mapped_mask & req) != req
                        {
                            return Err(Errno::EINVAL);
                        }
                        (!page.is_private() && page.mapped.is_none())
                            || (page.is_private()
                                && page.stale
                                && page.private_mask != all_mask
                                && page.mapped.is_none())
                    }
                };
                if needs_fault {
                    match window.fault_page(at as u64 * page_size).await {
                        Ok(()) => {}
                        Err(Errno::ENOENT) => return Err(Errno::EINVAL),
                        Err(err) => return Err(err),
                    }
                }
            }
            let mut pages = self.pages.lock().unwrap();
            if pages.released {
                return Ok(());
            }
            for at in range.clone() {
                let req = self.subpage_mask(at, start_off, end_off);
                if (pages.pages[at].mapped_mask & req) != req {
                    return Err(Errno::EINVAL);
                }
            }
            let needed_new =
                pages.pages[range.clone()].iter().filter(|page| !page.is_private()).count() as u64;
            if pages.private + needed_new > most {
                return Err(Errno::ENOMEM);
            }
            let any_unnamed = pages.pages[range.clone()].iter().any(|page| {
                (!page.is_private() && page.mapped.is_none())
                    || (page.is_private()
                        && page.stale
                        && page.private_mask != all_mask
                        && page.mapped.is_none())
            });
            if any_unnamed {
                continue;
            }
            // Keep the lock from the budget check through every run, so a write fault cannot
            // consume pages promised to this request.
            let mut runs: Vec<(usize, Vec<u8>, Vec<u16>)> = Vec::new();
            for at in range.clone() {
                let mask = self.subpage_mask(at, start_off, end_off);
                let page = pages.pages[at];
                if page.is_private() {
                    if page.stale {
                        if page.private_mask == all_mask {
                            self.apply_stale_refresh(&mut pages, at, None, false);
                        } else {
                            let mapped =
                                page.mapped.expect("the stale page is named under the lock");
                            let mut bytes = vec![0u8; page_size as usize];
                            self.read_page(mapped.phys, &mut bytes);
                            self.apply_stale_refresh(&mut pages, at, Some(&bytes), false);
                        }
                        pages.pages[at].private_mask |= mask;
                    } else if page.anon_vma {
                        pages.pages[at].private_mask |= mask;
                        self.restore_coalesced_private(&mut pages, at, false);
                    } else {
                        pages.pages[at].private_mask |= mask;
                    }
                    if pages.pages[at].wp_partial
                        && (!self.process_writes.load(Ordering::Relaxed)
                            || pages.pages[at].private_mask == all_mask)
                    {
                        pages.update(at..at + 1, |page| page[0].wp_partial = false);
                        self.host.write_unprotect(
                            self.base + at * page_size as usize,
                            page_size as usize,
                        );
                    }
                    continue;
                }
                let mapped = page.mapped.expect("the page is named under the lock");
                let mut bytes = vec![0u8; page_size as usize];
                self.read_page(mapped.phys, &mut bytes);
                match runs.last_mut() {
                    Some((first, run, masks)) if *first + masks.len() == at => {
                        run.extend_from_slice(&bytes);
                        masks.push(mask);
                    }
                    _ => runs.push((at, bytes, vec![mask])),
                }
            }
            for (first, bytes, masks) in &runs {
                self.privatize_masked(&mut pages, *first, bytes, masks);
            }
            drop(pages);
            if let Some(prefault) = &self.prefault {
                for (first, bytes, _) in &runs {
                    prefault.prefault(*first as u64 * page_size, bytes.len() as u64);
                }
            }
            return Ok(());
        }
        Err(Errno::ENOMEM)
    }

    /// cladefs: maps the pages of the `len` bytes at `offset` for the guest ahead of its accesses,
    /// as the guest asks for the pages it maps to read (doc-4's `MAP_PREFAULT`): those the window
    /// names and none installed yet are installed now, as their first faults would install them
    /// ([`Self::reinstall`]), in runs of memory following on; then each page installed, or private,
    /// is mapped for the guest (`Prefault`): each one's first access otherwise takes the hypervisor
    /// a fault, which nested virtualization makes dear (TASK-112.1), and a page not installed, a
    /// userfaultfd fault besides (backlog doc-13: a `MAP` names pages, their first touch installs
    /// them, TASK-276.6). Pages the window hasn't named are left to the guest's faults, which this
    /// mustn't wait for: what waits for the daemon's replies may be holding them up. Nothing
    /// outside the second stage is mapped, nor for the guest without a `Prefault`.
    pub fn prefault(&self, offset: u64, len: u64) {
        let page_size = page_size() as usize;
        let end = offset.saturating_add(len).min(self.size as u64);
        if offset >= end {
            return;
        }
        let (first, last) = (offset as usize / page_size, (end as usize).div_ceil(page_size));
        let runs: Vec<std::ops::Range<usize>> = {
            let mut pages = self.pages.lock().unwrap();
            if pages.released {
                return;
            }
            // The runs to install: pages named, left out, not private, that a fault would install
            // as they are.
            let named = |page: &PageChanges| {
                !page.withheld
                    && !page.restricted
                    && !page.is_private()
                    && page.mapped.is_some_and(|mapped| !mapped.installed())
            };
            let follows = |before: &PageChanges, page: &PageChanges| {
                matches!((before.mapped, page.mapped), (Some(before), Some(page))
                    if before.phys.arena == page.phys.arena
                        && before.phys.offset + page_size as u64 == page.phys.offset)
            };
            let mut installs: Vec<std::ops::Range<usize>> = Vec::new();
            for at in (first..last).filter(|&at| named(&pages.pages[at])) {
                match installs.last_mut() {
                    Some(run)
                        if run.end == at && follows(&pages.pages[at - 1], &pages.pages[at]) =>
                    {
                        run.end = at + 1
                    }
                    _ => installs.push(at..at + 1),
                }
            }
            // cladefs: each in room made for it in its share, and no more of them than seven eighths
            // of it, the rest left to the guest's faults: installed all, then trimmed, a call over
            // a guest's pages named scattered, the range and the names its own, took the process
            // past the mappings it may make, and on, installing what it then left out (board
            // 8493).
            let share = self.share.load(Ordering::Relaxed);
            installs.truncate(share - share / 8);
            if !installs.is_empty() {
                let used = pages.tick();
                for run in installs {
                    self.trim(&mut pages, 1, Some(run.start));
                    let now = self.now();
                    let _busy = self.busy.doing(Doing::Reinstall, now);
                    let phys = pages.pages[run.start].mapped.expect("a page named").phys;
                    pages.update(run.clone(), |pages| {
                        for page in pages {
                            page.made = page.made.wrapping_add(1);
                            page.changed = now;
                            page.anon_vma = false;
                            page.wp_partial = false;
                            if let Some(mapped) = &mut page.mapped {
                                (mapped.installed, mapped.used) = (1, used);
                            }
                        }
                    });
                    let addr = self.base + run.start * page_size;
                    self.install(addr, run.len() * page_size, self.memory(phys));
                }
            }
            // cladefs: private pages too, their memory filled here (backlog doc-13), unless stale
            // or currently coalesced into an anonymous VMA.
            let mapped = |page: &PageChanges| {
                !page.withheld
                    && ((page.is_private() && !page.stale && !page.anon_vma)
                        || page.mapped.is_some_and(|mapped| mapped.installed()))
            };
            let mut runs: Vec<std::ops::Range<usize>> = Vec::new();
            for at in (first..last).filter(|&at| mapped(&pages.pages[at])) {
                match runs.last_mut() {
                    Some(run) if run.end == at => run.end = at + 1,
                    _ => runs.push(at..at + 1),
                }
            }
            runs
        };
        let Some(prefault) = &self.prefault else { return };
        for run in runs {
            let (offset, len) = ((run.start * page_size) as u64, (run.len() * page_size) as u64);
            prefault.prefault(offset, len);
        }
    }

    /// The host address of the `len` bytes at `offset`, as a length in host terms.
    fn addr(&self, offset: u64, len: u64) -> (usize, usize) {
        let (offset, len) = (offset as usize, len as usize);
        assert!(offset <= self.size && len <= self.size - offset, "out of the second stage");
        (self.base + offset, len)
    }
    /// Overwrites non-private sub-pages of partially-private host pages directly in
    /// [`PrivateMemory`] from `phys` (when `STAGE_MAP` already has the backing page in hand),
    /// remapping `PrivateMemory` back in place of any stale anonymous VMA without leaving extra
    /// VMAs behind.
    fn apply_in_place_refreshes(
        &self,
        refreshes: &[(usize, crate::pages::PhysLocation, u16, bool, bool)],
    ) {
        let Some(private) = self.private.get() else { return };
        let fd = private.fd.as_raw_fd();
        let ps = page_size() as usize;
        let mut buf = vec![0u8; ps];
        for &(at, phys, mask, remap_vma, want_wp) in refreshes {
            self.read_page(phys, &mut buf);
            let page_offset = (at * ps) as u64;
            for slot in 0..self.subpages() {
                if mask & (1u16 << slot) != 0 {
                    continue;
                }
                let within = slot * self.unit;
                self.host.write_subpage(
                    fd,
                    page_offset + within as u64,
                    &buf[within..within + self.unit],
                );
            }
            if remap_vma {
                self.map_private_vma(self.base + at * ps, want_wp);
            }
        }
    }
}

impl<H: HostMemory> SecondStage for UffdSecondStage<H> {
    /// cladefs: names the pages, installing none (backlog doc-13: a `MAP` names pages, their first
    /// touch installs them, TASK-276.6): what was there is taken away ([`Self::take_away`]),
    /// and each page left out, as the window names it, for its first fault to install
    /// ([`Self::reinstall`]), or a `PRIVATE` to fill in place, or the guest's asking to read it
    /// next to install ahead ([`Self::prefault`]). Installed as it was mapped, every page mapped
    /// took an `mmap` there, and a page the guest then stored to without reading it, a write's,
    /// took another over it, its TLB flushed on every CPU of the process's and the guest's.
    /// cladefs: a fully-private page there stays, named nothing (doc-26, spec rule mapsKeepPrivate);
    /// a partially-private page refreshes its non-private sub-pages in place directly from `phys`,
    /// collapsing any stale anonymous VMA back into `PrivateMemory`.
    fn map(&self, offset: u64, phys: crate::pages::PhysLocation, len: u64) {
        let (addr, len) = self.addr(offset, len);
        let range = self.page_range(addr, len);
        let page_size = page_size();
        let all_mask = self.all_subpages_mask();
        let process_writes = self.process_writes.load(Ordering::Relaxed);
        let keeps = if rules().maps_keep_private { Private::Keep } else { Private::End };
        let name = |pages: &mut Pages| {
            let used = pages.tick();
            let taking = pages.take_away_bytes(
                offset as usize,
                offset as usize + len,
                keeps,
                self.unit,
                all_mask,
                false,
            );
            Self::took(&self.taken.left_out, taking.installed);
            let mut refreshes = Vec::new();
            let mut wp_added = false;
            pages.update(range.clone(), |pages| {
                for (idx, page) in pages.iter_mut().enumerate() {
                    if page.private_mask == all_mask && rules().maps_keep_private {
                        continue;
                    }
                    let at = range.start + idx;
                    let offset = phys.offset + idx as u64 * page_size;
                    let phys = crate::pages::PhysLocation { arena: phys.arena, offset };
                    if page.is_partially_private(all_mask) {
                        let mask = page.private_mask;
                        page.stale = false;
                        page.mapped = None;
                        page.restricted = false;
                        if page.anon_vma {
                            // Keep an existing anonymous VMA intact so `map()` never splits a
                            // coalesced span past the VMA budget; `PrivateMemory` is updated in
                            // place below and remapped lazily on the next touch.
                            page.wp_partial = false;
                            refreshes.push((at, phys, mask, false, false));
                        } else {
                            let want_wp = process_writes && mask != all_mask;
                            let remap_vma = page.wp_partial != want_wp;
                            wp_added |= want_wp && !page.wp_partial;
                            page.wp_partial = want_wp;
                            refreshes.push((at, phys, mask, remap_vma, want_wp));
                        }
                        continue;
                    }
                    // Direct callers (e.g. unit tests) that call `map()` without `window_mapped()`
                    // still mark the full host page present in `mapped_mask`.
                    if page.mapped_mask == 0 {
                        page.mapped_mask = all_mask;
                    }
                    // Left installed as it was, past the rule, what a fault installs next is it.
                    let installed = u8::from(page.mapped.is_some_and(|m| m.installed()));
                    let installed = if taking.installed { 0 } else { installed };
                    page.mapped = Some(Mapped { phys, installed, used });
                    page.restricted = false;
                }
            });
            if wp_added && self.trim_for_stale(pages) {
                for refresh in &mut refreshes {
                    if pages.pages[refresh.0].anon_vma {
                        refresh.3 = false;
                    }
                }
            }
            (taking, refreshes)
        };
        self.change(addr, len, 1, name, |(taking, refreshes)| {
            self.take_away(addr, len, taking);
            self.apply_in_place_refreshes(&refreshes);
        });
    }

    fn unmap(&self, offset: u64, len: u64, private: Private) {
        let ps = page_size();
        let (start_off, end_off) = (offset as usize, (offset + len) as usize);
        let h_start = offset - offset % ps;
        let h_end = (offset + len).next_multiple_of(ps);
        let (h_addr, h_len) = self.addr(h_start, h_end - h_start);
        let range = self.page_range(h_addr, h_len);
        let all_mask = self.all_subpages_mask();
        let forget = |pages: &mut Pages| {
            let mut taking =
                pages.take_away_bytes(start_off, end_off, private, self.unit, all_mask, true);
            Self::took(&self.taken.left_out, taking.installed);
            pages.update(range, |pages| {
                pages.iter_mut().for_each(|page| (page.mapped, page.restricted) = (None, false))
            });
            if !taking.newly_staled.is_empty() && self.trim_for_stale(pages) {
                taking.newly_staled.clear();
            }
            taking
        };
        self.change(h_addr, h_len, 1, forget, |taking| self.take_away(h_addr, h_len, taking));
    }

    /// cladefs: the window's change of a range it mapped, made at once (TASK-91.16): each map of
    /// the very pages installed where it maps them, as a `MAP` of what a range maps already, leaves
    /// them in place, the memory staying; every other page of the range is taken away
    /// ([`Self::take_away`]), the pages a map covers named, installed as they are first touched
    /// ([`SecondStage::map`]), the rest mapping nothing. Made in steps, each page would be left
    /// out, then named again: an unmap of the old memory, flushing the host's TLBs and the
    /// hypervisor's mappings, then the guest's accesses faulting the pages in again.
    fn remap(&self, offset: u64, len: u64, private: Private, anew: &[Anew]) {
        let ps = page_size();
        let all_mask = self.all_subpages_mask();
        let process_writes = self.process_writes.load(Ordering::Relaxed);
        // board 11673: handle both host-page-aligned and sub-page (`self.unit`-aligned) remaps
        // atomically under the same `self.pages` lock so concurrent `make_private` never observes
        // cleared `mapped_mask` bits between unmap and `Anew::Mapped`/`Anew::Map`.
        let (start_off, end_off) = (offset as usize, (offset + len) as usize);
        let h_start = offset - offset % ps;
        let h_end = (offset + len).next_multiple_of(ps);
        let (addr, h_len) = self.addr(h_start, h_end - h_start);
        let range = self.page_range(addr, h_len);
        let page_size = ps as usize;
        let maps: Vec<(usize, usize, crate::pages::PhysLocation)> = (anew.iter())
            .filter_map(|anew| match *anew {
                Anew::Map { offset, phys, len } => {
                    let (addr, len) = self.addr(offset, len);
                    Some((addr, len, phys))
                }
                Anew::Mapped { .. } => None,
            })
            .collect();
        let record = |pages: &mut Pages| {
            let used = pages.tick();
            // The pages a map leaves in place, as they are.
            let mut kept = vec![false; range.len()];
            for &(addr, len, phys) in &maps {
                let mapping = self.page_range(addr, len);
                if pages.installed_at(mapping.clone(), phys) {
                    kept[mapping.start - range.start..mapping.end - range.start].fill(true);
                    pages.update(mapping, |pages| {
                        for page in pages {
                            if let Some(mapped) = &mut page.mapped {
                                mapped.used = used;
                            }
                        }
                    });
                }
            }
            // Pages covered by a subsequent `Anew::Map` in this batch will be refreshed in place
            // if partially private, so `take_away_bytes` need not split their VMA with Anonymous.
            let mut mapped_by_anew = vec![false; range.len()];
            for &(m_addr, m_len, _) in &maps {
                let m = self.page_range(m_addr, m_len);
                mapped_by_anew[m.start - range.start..m.end - range.start].fill(true);
            }
            // Snapshot prior VMA state before `take_away_bytes` marks partially-private pages
            // stale so `Anew::Map` below preserves already-anonymous VMAs without splitting a
            // coalesced span and avoids redundant remaps on already-mapped `PrivateMemory` pages.
            let was_anon: Vec<bool> =
                pages.pages[range.clone()].iter().map(|p| p.anon_vma).collect();
            let was_wp: Vec<bool> =
                pages.pages[range.clone()].iter().map(|p| p.wp_partial).collect();
            // Each run of the rest taken away, and its pages mapping nothing, until named anew.
            let mut taken: Vec<(std::ops::Range<usize>, Taking)> = Vec::new();
            let mut at = range.start;
            while at < range.end {
                if kept[at - range.start] {
                    at += 1;
                    continue;
                }
                let start = at;
                while at < range.end && !kept[at - range.start] {
                    at += 1;
                }
                let run_start = (start * page_size).max(start_off);
                let run_end = (at * page_size).min(end_off);
                let mut taking =
                    pages.take_away_bytes(run_start, run_end, private, self.unit, all_mask, true);
                Self::took(&self.taken.left_out, taking.installed);
                pages.update(start..at, |pages| {
                    pages.iter_mut().for_each(|page| (page.mapped, page.restricted) = (None, false))
                });
                // Remove from `newly_staled` any page that an `Anew::Map` below will immediately
                // refresh in place (avoiding a pointless Anonymous VMA round-trip).
                taking.newly_staled.retain(|&p| !mapped_by_anew[p - range.start]);
                taken.push((start..at, taking));
            }
            // Apply `Anew::Mapped` and `Anew::Map` atomically in wire order under the same lock,
            // preventing any TOCTOU race with concurrent `make_private` calls.
            let gives_back = rules().mapping_gives_back_withheld;
            let mut extra_withheld = Vec::new();
            let mut extra_staled = Vec::new();
            let mut refreshes = Vec::new();
            let mut wp_added = false;
            for entry in anew {
                match *entry {
                    Anew::Mapped { offset: m_off, len: m_len } => {
                        let (m_start_off, m_end_off) = (m_off as usize, (m_off + m_len) as usize);
                        let h_s = m_off - m_off % ps;
                        let h_e = (m_off + m_len).next_multiple_of(ps);
                        let (m_addr, m_hlen) = self.addr(h_s, h_e - h_s);
                        let m_range = self.page_range(m_addr, m_hlen);
                        pages.update(m_range.clone(), |slice| {
                            for (idx, page) in slice.iter_mut().enumerate() {
                                let p = m_range.start + idx;
                                let mask = self.subpage_mask(p, m_start_off, m_end_off);
                                page.mapped_mask |= mask;
                                page.made = page.made.wrapping_add(1);
                                if gives_back && page.withheld {
                                    page.withheld = false;
                                    extra_withheld.push(p);
                                }
                                if page.is_partially_private(all_mask)
                                    && !mapped_by_anew[p - range.start]
                                {
                                    if !page.anon_vma {
                                        extra_staled.push(p);
                                    }
                                    page.stale = true;
                                    page.anon_vma = true;
                                    page.wp_partial = false;
                                }
                            }
                        });
                    }
                    Anew::Map { offset: m_off, phys, len: m_len } => {
                        let (m_addr, m_len) = self.addr(m_off, m_len);
                        let mapping = self.page_range(m_addr, m_len);
                        if kept[mapping.start - range.start] {
                            continue;
                        }
                        pages.update(mapping.clone(), |slice| {
                            for (idx, page) in slice.iter_mut().enumerate() {
                                if page.private_mask == all_mask && rules().maps_keep_private {
                                    continue;
                                }
                                let p = mapping.start + idx;
                                let p_offset = phys.offset + (idx * page_size) as u64;
                                let p_phys =
                                    crate::pages::PhysLocation { offset: p_offset, ..phys };
                                if page.is_partially_private(all_mask) {
                                    let mask = page.private_mask;
                                    page.stale = false;
                                    page.mapped = None;
                                    page.restricted = false;
                                    if was_anon[p - range.start] {
                                        page.anon_vma = true;
                                        page.wp_partial = false;
                                        refreshes.push((p, p_phys, mask, false, false));
                                    } else {
                                        let want_wp = process_writes && mask != all_mask;
                                        let prev_wp = was_wp[p - range.start];
                                        let remap_vma = prev_wp != want_wp;
                                        wp_added |= want_wp && !prev_wp;
                                        page.anon_vma = false;
                                        page.wp_partial = want_wp;
                                        refreshes.push((p, p_phys, mask, remap_vma, want_wp));
                                    }
                                    continue;
                                }
                                if page.mapped_mask == 0 {
                                    page.mapped_mask = all_mask;
                                }
                                page.mapped = Some(Mapped { phys: p_phys, installed: 0, used });
                            }
                        });
                    }
                }
            }
            if (wp_added
                || !extra_staled.is_empty()
                || taken.iter().any(|(_, t)| !t.newly_staled.is_empty()))
                && self.trim_for_stale(pages)
            {
                extra_staled.clear();
                for (_, t) in &mut taken {
                    t.newly_staled.clear();
                }
                for refresh in &mut refreshes {
                    if pages.pages[refresh.0].anon_vma {
                        refresh.3 = false;
                    }
                }
            }
            (taken, extra_withheld, extra_staled, refreshes)
        };
        // The ranges it takes away: between the maps it leaves in place, one more than they.
        let room = maps.len() + 1;
        self.change(
            addr,
            h_len,
            room,
            record,
            |(taken, extra_withheld, extra_staled, refreshes)| {
                for (run, taking) in taken {
                    let addr = self.base + run.start * page_size;
                    self.take_away(addr, run.len() * page_size, taking);
                }
                for at in extra_withheld {
                    let addr = self.base + at * page_size;
                    self.host.protect(addr, page_size, libc::PROT_READ | libc::PROT_WRITE);
                }
                for at in extra_staled {
                    self.mark_stale_vma(self.base + at * page_size);
                }
                self.apply_in_place_refreshes(&refreshes);
            },
        );
    }

    fn window_mapped(&self, offset: u64, len: u64) {
        let ps = page_size();
        let (start_off, end_off) = (offset as usize, (offset + len) as usize);
        let h_start = offset - offset % ps;
        let h_end = (offset + len).next_multiple_of(ps);
        let (addr, h_len) = self.addr(h_start, h_end - h_start);
        let page_size = ps as usize;
        let range = self.page_range(addr, h_len);
        let all_mask = self.all_subpages_mask();
        let mut pages = self.pages.lock().unwrap();
        if pages.released {
            return;
        }
        // Each page counts the mapping as a change, so that a fault that found nothing there
        // before withholds nothing now. A page withheld is unmapped memory, registered for
        // missing faults, made accessible again. A partially-private page that is not yet stale
        // becomes stale so newly mapped non-private sub-pages refresh on next access.
        let gives_back = rules().mapping_gives_back_withheld;
        let withheld: Vec<usize> =
            range.clone().filter(|&at| gives_back && pages.pages[at].withheld).collect();
        let mut staled = Vec::new();
        pages.update(range.clone(), |slice| {
            for (idx, page) in slice.iter_mut().enumerate() {
                let at = range.start + idx;
                page.mapped_mask |= self.subpage_mask(at, start_off, end_off);
                page.made = page.made.wrapping_add(1);
                page.withheld &= !gives_back;
                if page.is_partially_private(all_mask) {
                    if !page.anon_vma {
                        staled.push(at);
                    }
                    page.stale = true;
                    page.anon_vma = true;
                    page.wp_partial = false;
                }
            }
        });
        for at in withheld {
            let addr = self.base + at * page_size;
            self.host.protect(addr, page_size, libc::PROT_READ | libc::PROT_WRITE);
        }
        let coalesced = !staled.is_empty() && self.trim_for_stale(&mut pages);
        if !coalesced {
            for &at in &staled {
                self.mark_stale_vma(self.base + at * page_size);
            }
        }
    }
}

use super::read_at;

impl<H: HostMemory> UffdSecondStage<H> {
    /// The memory of the page at `phys`, which the window maps read-only. Not mapped for the guest
    /// ahead of its accesses (`Prefault`) but those it asks for ([`Self::prefault`]):
    /// mapped for it, the hypervisor's pre-fault took a fifth of the VM process's time changing the
    /// window, and its pages, in the host's page tables, made each later unmap of them flush the
    /// host's and the hypervisor's TLBs on every CPU (TASK-109).
    fn memory(&self, phys: crate::pages::PhysLocation) -> Memory {
        // A page of an arena the window may not map is the daemon's error, which the guest must
        // not get past.
        let (fd, read_only) =
            self.memories.fd(phys.arena).expect("a window's page is of its arenas");
        // Memory this process holds read-only, the default arena's, public bases' pages, is mapped
        // private: userfaultfd tracks writes to a mapping that may be made writable alone, and a
        // write to a private one could only copy the page into this process, which the window
        // never lets it make, as it write-protects every page it maps. Its loads read the very
        // pages.
        Memory::File { fd, offset: phys.offset, shared: !read_only }
    }

    /// Installs `memory` at the `len` bytes at `addr`, which the window maps read-only.
    fn install(&self, addr: usize, len: usize, memory: Memory) {
        let writable = libc::PROT_READ | libc::PROT_WRITE;
        let prot = if rules().map_with_window_protection { libc::PROT_READ } else { writable };
        self.host.map(addr, len, prot, memory);
        // Writes are tracked by write-protecting the page, rather than by its protection, whose
        // faults userfaultfd does not report. The new mapping is read-only until it is
        // write-protected, so that a write meanwhile fails rather than going untracked.
        self.host.register(addr, len, UFFDIO_REGISTER_MODE_WP);
        // Mapped writable already, it needs no protection changed (TASK-26.18.3).
        let make_writable = prot != writable;
        if make_writable && !rules().write_protect_before_writable {
            self.host.protect(addr, len, writable);
        }
        self.host.write_protect(addr, len);
        if make_writable && rules().write_protect_before_writable {
            self.host.protect(addr, len, writable);
        }
    }

    /// Leaves the `len` bytes at `addr` out: memory registered for missing faults. cladefs: its
    /// private memory, of a window that copies on write, unfilled there.
    fn leave_out(&self, addr: usize, len: usize) {
        self.leave_out_emptying(addr, len, &[]);
    }

    /// As [`Self::leave_out`], cladefs: emptying the private pages of `private`, runs of its pages,
    /// once the memory there is mapped anew, inaccessible, and before it is accessible again: with
    /// nothing mapping them then, as their own mapping went with the `mmap`, emptying them flushes
    /// no TLB (see the module docs, TASK-276.6).
    fn leave_out_emptying(&self, addr: usize, len: usize, private: &[std::ops::Range<usize>]) {
        // The new memory is inaccessible until it is registered, so that an access meanwhile fails
        // rather than faulting in a page of zeros.
        let writable = libc::PROT_READ | libc::PROT_WRITE;
        let prot = if rules().register_before_accessible { libc::PROT_NONE } else { writable };
        self.host.map(addr, len, prot, self.base_memory(addr));
        self.empty_private(private);
        self.host.register(addr, len, UFFDIO_REGISTER_MODE_MISSING);
        self.host.protect(addr, len, writable);
    }

    /// Makes room in its share for `room` more runs or restrictions, with its pages' lock held
    /// (`pages`): while what it holds and they are more than its share, leaves out the runs
    /// installed or faulted on longest ago, then lifts the restrictions made longest ago, down to
    /// seven eighths of its share less `room`, so that it is not done at each change. Never page
    /// `keep`'s run, nor its restriction, which a fault waits on: left out, or lifted, the fault's
    /// access, retried, would fault again, for good. cladefs: a restriction lifted, the page
    /// faults to the window once more, which restricts it again (board 8493); a trim looks at the
    /// runs alone, where it walked every page of the window to find them, 16.7M of a 64 GiB
    /// window's, holding the lock every fault takes (board 8495).
    fn trim(&self, pages: &mut Pages, room: usize, keep: Option<usize>) {
        let share = self.share.load(Ordering::Relaxed);
        if pages.released || pages.held().saturating_add(room) <= share {
            return;
        }
        let low = (share - share / 8).saturating_sub(room);
        let now = self.now();
        let _busy = self.busy.doing(Doing::Trim, now);
        let page_size = page_size() as usize;
        let kept = keep.and_then(|keep| pages.run_of(keep));
        let mut runs: Vec<(u64, usize)> = (pages.runs.iter())
            .filter(|&(&start, _)| Some(start) != kept)
            .map(|(&start, run)| (run.used, start))
            .collect();
        runs.sort_unstable();
        for (_, start) in runs {
            if pages.held() <= low {
                break;
            }
            // Leaving out another run leaves this one as it was.
            let end = pages.runs[&start].end;
            pages.update(start..end, |pages| {
                for page in pages {
                    page.made = page.made.wrapping_add(1);
                    page.changed = now;
                    if let Some(mapped) = &mut page.mapped {
                        mapped.installed = 0;
                        page.restricted = false;
                    }
                }
            });
            Self::took(&self.taken.left_out, true);
            self.leave_out(self.base + start * page_size, (end - start) * page_size);
        }
        let mut kept_restricted = false;
        while pages.held() > low {
            let Some(at) = pages.restricted_order.pop_front() else { break };
            if Some(at) == keep {
                kept_restricted = true;
                continue;
            }
            let page = pages.pages[at];
            if !page.restricts() {
                continue;
            }
            pages.update(at..at + 1, |page| {
                (page[0].withheld, page[0].restricted, page[0].for_worker) = (false, false, false)
            });
            let writable = libc::PROT_READ | libc::PROT_WRITE;
            self.host.protect(self.base + at * page_size, page_size, writable);
            let offset = (at * page_size) as u64;
            changed_protection("lifted, its window's share full", offset, page.made, None);
        }
        if kept_restricted && let Some(keep) = keep {
            pages.restricted_order.push_front(keep);
        }
        self.coalesce_anon_vmas(pages, room, keep);
    }

    /// Trims runs and restrictions, then coalesces stale anonymous VMAs if `held() > share`;
    /// returns `true` when `anon_vma`/`wp_partial` spans were just coalesced into an anonymous VMA
    /// (so callers can skip per-page `mark_stale_vma` calls).
    fn trim_for_stale(&self, pages: &mut Pages) -> bool {
        let share = self.share.load(Ordering::Relaxed);
        if pages.released || pages.held() <= share {
            return false;
        }
        // Evict clean runs and restrictions first via `trim`, then check whether coalescing is
        // still needed (or already happened inside `trim`).
        let before = pages.anon_runs + pages.wp_partials;
        self.trim(pages, 0, None);
        pages.anon_runs + pages.wp_partials < before
    }

    /// Coalesces disjoint `anon_vma`/`wp_partial` runs into at most one anonymous VMA per side of
    /// `keep` when `held() + room > share`, bounding kernel VMAs even under thousands of
    /// non-adjacent stale or write-protected partial pages (board 8493 / review 100 finding 344 /
    /// review 102 findings 349 & 350).
    ///
    /// Crucially, `page.stale` is NOT set on fresh private pages swept into the coalesced span:
    /// their [`PrivateMemory`] still holds up-to-date bytes for every sub-page while their daemon
    /// PTE may remain resident, so their next touch remaps [`PrivateMemory`] directly via
    /// [`Self::restore_coalesced_private`] without calling the daemon.
    fn coalesce_anon_vmas(&self, pages: &mut Pages, room: usize, keep: Option<usize>) {
        let share = self.share.load(Ordering::Relaxed);
        if pages.held().saturating_add(room) <= share || (pages.anon_runs + pages.wp_partials) <= 1
        {
            return;
        }
        let page_size = page_size() as usize;
        let first_anon = pages.pages.iter().position(|p| p.anon_vma || p.wp_partial);
        let last_anon = pages.pages.iter().rposition(|p| p.anon_vma || p.wp_partial);
        let (Some(first), Some(last)) = (first_anon, last_anon) else { return };
        let skip = keep.map(|k| match pages.run_of(k) {
            Some(start) => start..pages.runs[&start].end,
            None => k..k + 1,
        });
        let mut spans = Vec::with_capacity(2);
        let total_end = last + 1;
        match skip {
            Some(s) if s.start < total_end && s.end > first => {
                if first < s.start {
                    spans.push(first..s.start);
                }
                if s.end < total_end {
                    spans.push(s.end..total_end);
                }
            }
            _ => spans.push(first..total_end),
        }
        let writable = libc::PROT_READ | libc::PROT_WRITE;
        let prot = if rules().register_before_accessible { libc::PROT_NONE } else { writable };
        for span in spans {
            pages.update(span.clone(), |slice| {
                for page in slice {
                    page.anon_vma = true;
                    page.wp_partial = false;
                    page.restricted = false;
                    page.withheld = false;
                    if let Some(mapped) = &mut page.mapped {
                        mapped.installed = 0;
                    }
                }
            });
            let addr = self.base + span.start * page_size;
            let len = span.len() * page_size;
            self.host.map(addr, len, prot, Memory::Anonymous);
            self.host.register(addr, len, UFFDIO_REGISTER_MODE_MISSING);
            self.host.protect(addr, len, writable);
        }
    }

    /// Installs the page at `addr`, if the window maps it and it is left out, with those left out
    /// around it whose memory follows on from it, as the window names them, and returns whether it
    /// did. A page installed is counted as used. cladefs: left out past the budget, or named and
    /// never installed yet (backlog doc-13: a `MAP` names pages, their first touch installs them,
    /// TASK-276.6), under the lock the window's changes take: a change since the fault found the
    /// page is what is installed. A private page needs nothing: filled, its fault came before it
    /// was, and its access finds it now.
    fn reinstall(&self, addr: usize) -> Reinstalled {
        let page_size = page_size() as usize;
        let at = self.page_range(addr, 1).start;
        let mut pages = self.pages.lock().unwrap();
        if pages.pages[at].is_private() {
            if pages.pages[at].stale {
                return Reinstalled::Stale;
            }
            if pages.pages[at].anon_vma {
                self.restore_coalesced_private(&mut pages, at, false);
            }
            return Reinstalled::Done;
        }
        let used = pages.tick();
        let Some(mapped) = pages.pages[at].mapped else { return Reinstalled::Miss };
        if mapped.installed() {
            pages.update(at..at + 1, |page| {
                if let Some(mapped) = &mut page[0].mapped {
                    mapped.used = used;
                }
            });
            return Reinstalled::Miss;
        }
        // cladefs: in room made for it in its share, as it may install a run more (board 8493).
        self.trim(&mut pages, 1, Some(at));
        let phys = mapped.phys;
        // The pages around it left out with it, of memory following on, as far as a chunk's.
        let left_out_with = |page: &PageChanges, distance: i64| {
            !page.is_private()
                && page.mapped.is_some_and(|other| {
                    !other.installed()
                        && other.phys.arena == phys.arena
                        && other.phys.offset as i64 - phys.offset as i64
                            == distance * page_size as i64
                })
        };
        const MOST: usize = 512;
        let mut start = at;
        while start > 0
            && at - start < MOST
            && left_out_with(&pages.pages[start - 1], start as i64 - 1 - at as i64)
        {
            start -= 1;
        }
        let mut end = at + 1;
        while end < pages.pages.len()
            && end - start < MOST
            && left_out_with(&pages.pages[end], end as i64 - at as i64)
        {
            end += 1;
        }
        let first = crate::pages::PhysLocation {
            arena: phys.arena,
            offset: phys.offset - ((at - start) * page_size) as u64,
        };
        // cladefs: with the rule turned off, installed as the fault found it, its lock let go of
        // meanwhile, whatever a change made of the pages since.
        if !rules().install_as_named_now {
            drop(pages);
            pages = self.pages.lock().unwrap();
        }
        let now = self.now();
        pages.update(start..end, |pages| {
            for page in pages {
                page.made = page.made.wrapping_add(1);
                page.changed = now;
                page.anon_vma = false;
                page.wp_partial = false;
                if let Some(mapped) = &mut page.mapped {
                    mapped.installed = 1;
                    mapped.used = used;
                }
            }
        });
        let len = (end - start) * page_size;
        let _busy = self.busy.doing(Doing::Reinstall, now);
        self.install(self.base + start * page_size, len, self.memory(first));
        Reinstalled::Done
    }

    /// cladefs: what it holds at window offset `offset` and around it, and what it is doing: for a
    /// vCPU whose access keeps failing to tell of ([`describe_windows`]). Waits for its pages'
    /// lock no more than a moment, as what holds it may be what holds the vCPU up.
    fn describe_page(&self, offset: u64) -> String {
        let now = self.now();
        let ago = |at: u64| std::time::Duration::from_nanos(now.saturating_sub(at));
        let (doing, since) = self.busy.now();
        let doing = match doing {
            Doing::Nothing => "nothing".to_owned(),
            doing => format!("{doing:?} for {:?}", ago(since)),
        };
        let serving = match self.busy.serving.load(Ordering::Relaxed) {
            0 => "waiting for faults".to_owned(),
            since => format!("busy for {:?}", ago(since - 1)),
        };
        let mut said = format!(
            "the second stage at {:#x}, {} bytes, is doing {doing}; its fault thread is {serving}",
            self.base, self.size
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        let pages = loop {
            match self.pages.try_lock() {
                Ok(pages) => break Some(pages),
                // Held past the deadline: none, as the caller takes it.
                Err(_) if std::time::Instant::now() >= deadline => break None,
                // Held still: tried again.
                Err(_) => std::hint::spin_loop(),
            }
        };
        let Some(pages) = pages else {
            said.push_str("; its pages' lock stayed held for 100ms");
            return said;
        };
        let count = |what: fn(&PageChanges) -> bool| pages.pages.iter().filter(|p| what(p)).count();
        let installed = |page: &PageChanges| page.mapped.is_some_and(|mapped| mapped.installed());
        let left_out = |page: &PageChanges| page.mapped.is_some_and(|mapped| !mapped.installed());
        said.push_str(&format!(
            "; {} runs installed of a budget of {}, {} pages installed, {} left out, {} private, \
             {} withheld, {} restricted ({} for workers' writes)",
            pages.runs.len(),
            self.budget(),
            count(installed),
            count(left_out),
            count(|page| page.is_private()),
            count(|page| page.withheld),
            count(|page| page.restricted),
            count(|page| page.restricted && page.for_worker),
        ));
        let at = (offset / page_size()) as usize;
        if let Some(page) = pages.pages.get(at) {
            let state = match page.mapped {
                _ if page.is_private() => format!(
                    "private(mask={:#x}{})",
                    page.private_mask,
                    if page.stale { ", stale" } else { "" }
                ),
                Some(mapped) => format!(
                    "{} of {:?} at {:#x}",
                    if mapped.installed() { "installed" } else { "left out" },
                    mapped.phys.arena,
                    mapped.phys.offset
                ),
                None => "nothing mapped".to_owned(),
            };
            said.push_str(&format!(
                "; offset {offset:#x}: {state}{}{}{}, changed {} times, last {:?} ago",
                if page.withheld { ", withheld" } else { "" },
                if page.restricted { ", restricted to reads" } else { "" },
                if page.restricted && page.for_worker { " for a worker's write" } else { "" },
                page.made,
                ago(page.changed),
            ));
        }
        said
    }
}

impl<H: HostMemory> Share for UffdSecondStage<H> {
    fn shared(&self, share: usize) {
        let mut pages = self.pages.lock().unwrap();
        self.share.store(share, Ordering::Relaxed);
        self.trim(&mut pages, 0, None);
    }

    fn told(&self) {
        if let Some(told) = self.on_shared.get() {
            told();
        }
    }
}

impl<H: HostMemory> Drop for UffdSecondStage<H> {
    /// cladefs: its share goes to the windows left (board 8493).
    fn drop(&mut self) {
        self.mappings.leave();
    }
}

/// cladefs: the second stages serving faults ([`UffdSecondStage::serve`]), which a vCPU whose
/// access keeps failing tells of ([`describe_windows`]).
// Outside the simulation: userfaultfd serves VMs, which no simulated host runs.
static SERVING: std::sync::Mutex<Vec<Weak<UffdSecondStage>>> = std::sync::Mutex::new(Vec::new());

/// cladefs: what the DAX windows' second stages serving faults hold and are doing, for a vCPU whose
/// access keeps failing, `KVM_RUN` with `EFAULT`, which says no more, to tell of: of each, what it
/// holds and does, and, of the one whose memory host address `addr` is in, if given, the page
/// there: its window offset, and whether it is installed, left out, withheld, restricted or
/// private, and when it last changed. Waits for no stage more than a moment.
pub fn describe_windows(addr: Option<usize>) -> String {
    let serving: Vec<Arc<UffdSecondStage>> = (SERVING.lock())
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    if serving.is_empty() {
        return "no DAX window's second stage serves faults".to_owned();
    }
    let said: Vec<String> = (serving.iter())
        .map(|stage| {
            let memory = stage.base..stage.base + stage.size;
            let offset = match addr.filter(|addr| memory.contains(addr)) {
                Some(addr) => (addr - stage.base) as u64,
                // None of its pages.
                None => u64::MAX,
            };
            stage.describe_page(offset - offset % page_size())
        })
        .collect();
    said.join("; and ")
}

/// A new event file descriptor, which does not block reads.
fn event_fd() -> Result<File, Error> {
    // SAFETY: Creates an event file descriptor, which is owned below.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(Error::new(std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just created, and is owned here.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Wakes the fault thread by signalling an event file descriptor it polls.
struct EventFdWaker(File);

impl EventFdWaker {
    /// Clears the signal, once the thread has woken.
    fn reset(&self) {
        let mut count = [0u8; 8];
        // A read fails with `EAGAIN` if a racing reset cleared the signal first.
        let _ = (&self.0).read(&mut count);
    }
}

impl Wake for EventFdWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // A write fails with `EAGAIN` only if the counter would overflow, when it is signalled.
        let _ = (&self.0).write(&1u64.to_ne_bytes());
    }
}

/// The thread serving a second stage's faults, which stops when this is dropped.
pub struct FaultThread {
    stop: Arc<File>,
    /// cladefs: what it polls for a guest's access failing ([`guest_access_failed`]).
    lift: Arc<Lift>,
    thread: Option<crate::thread::Thread<()>>,
}

impl Drop for FaultThread {
    fn drop(&mut self) {
        forget_lift(&self.lift);
        (&*self.stop).write_all(&1u64.to_ne_bytes()).expect("stop the fault thread");
        if let Err(panic) = self.thread.take().unwrap().join() {
            std::panic::resume_unwind(panic);
        }
    }
}

/// cladefs: a fault thread polls `lift` no more ([`LIFTS`]).
fn forget_lift(lift: &Arc<Lift>) {
    let mut lifts = LIFTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    lifts.retain(|registered| !Arc::ptr_eq(registered, lift));
}

/// The device userfaultfds are made through, whose permissions grant handling faults taken in the
/// kernel, where the system call needs the `vm.unprivileged_userfaultfd` sysctl or CAP_SYS_PTRACE
/// (and the VMM's seccomp filter refuses it).
const USERFAULTFD_DEVICE: &str = "/dev/userfaultfd";

/// cladefs: whether this process can make the userfaultfds DAX windows take their faults through:
/// opens `/dev/userfaultfd`, as each window's second stage does, failing as it would, saying what
/// the host lacks. For a VMM to call as it makes a device with a window, so that a host without it
/// fails the VM as it is set up, not as its guest activates the device (TASK-308).
pub fn userfaultfd_accessible() -> Result<(), Error> {
    open_device(Path::new(USERFAULTFD_DEVICE)).map(drop)
}

/// Opens the userfaultfd device at `path`, for reading and writing, or says why it can't: which
/// device, the error, and what gives a process access to it.
fn open_device(path: &Path) -> Result<File, Error> {
    File::options().read(true).write(true).open(path).map_err(|err| {
        let (uid, gid) = host_ids();
        Error::new(anyhow::anyhow!(
            "{}: {err}: a DAX window takes its guest's faults, KVM's among them, through it, which \
             this process (uid {uid}, gid {gid}) needs read and write access to: give it the \
             device's group or mode (a udev rule: KERNEL==\"userfaultfd\", GROUP=\"kvm\", \
             MODE=\"0660\"), or an ACL (setfacl -m u:USER:rw), or, in a jail, the device node in \
             it; vm.unprivileged_userfaultfd serves no VMM, as it is for the userfaultfd(2) system \
             call, which the VMM's seccomp filter refuses",
            path.display()
        ))
    })
}

/// This process's effective uid and gid as the host has them, who a device's access is granted
/// to: a VMM in a user namespace, as the guest tests' runner starts it, is root in its own.
fn host_ids() -> (u32, u32) {
    // SAFETY: Neither has preconditions.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let outside = |id, map| {
        let map = std::fs::read_to_string(map).unwrap_or_default();
        host_id(id, &map).unwrap_or(id)
    };
    (outside(uid, "/proc/self/uid_map"), outside(gid, "/proc/self/gid_map"))
}

/// `id` of this user namespace as its parent has it, by the namespace's `map` (uid_map's or
/// gid_map's lines: `INSIDE OUTSIDE COUNT`), if the map holds it.
fn host_id(id: u32, map: &str) -> Option<u32> {
    map.lines().find_map(|line| {
        // A field not a number: not a line of the map's.
        let fields: Vec<u64> =
            line.split_whitespace().map(|field| field.parse().ok()).collect::<Option<_>>()?;
        let [inside, outside, count] = fields[..] else { return None };
        let id = u64::from(id);
        // An ID past a u32: not one the map gives.
        (inside..inside + count).contains(&id).then(|| u32::try_from(outside + id - inside).ok())?
    })
}

/// Opens a userfaultfd that handles faults taken in the kernel as well as in user mode, through
/// `/dev/userfaultfd` alone.
fn open_userfaultfd() -> Result<OwnedFd, Error> {
    let device = open_device(Path::new(USERFAULTFD_DEVICE))?;
    let flags = libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: Creates a userfaultfd, which is owned below.
    let fd = unsafe { libc::ioctl(device.as_raw_fd(), USERFAULTFD_IOC_NEW as _, flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(Error::new(
            anyhow::Error::new(err).context(format!("{USERFAULTFD_DEVICE}: USERFAULTFD_IOC_NEW")),
        ));
    }
    // SAFETY: `fd` was just created, and is owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Issues userfaultfd ioctl `request` with `arg`.
fn ioctl<T>(uffd: &OwnedFd, request: u64, arg: &mut T) -> std::io::Result<()> {
    // SAFETY: `arg` is the structure `request` takes.
    if unsafe { libc::ioctl(uffd.as_raw_fd(), request as _, arg as *mut T) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests;

#[cfg(test)]
mod tests {
    use super::super::Dax;
    use super::super::tests::TestSource;
    use super::*;

    /// TASK-308: a userfaultfd device the process cannot open is an error naming it, the error and
    /// what gives access to it, where the system call the VMM's seccomp filter refuses was made in
    /// its place, killing the VMM ("bad syscall"), which said nothing of the device.
    #[test]
    fn a_device_it_cannot_open_says_which_why_and_what_gives_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("userfaultfd");
        let err = open_device(&path).unwrap_err();
        let (uid, gid) = host_ids();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "{}: No such file or directory (os error 2): a DAX window takes its guest's faults, \
                 KVM's among them, through it, which this process (uid {uid}, gid {gid}) needs read \
                 and write access to: give it the device's group or mode (a udev rule: \
                 KERNEL==\"userfaultfd\", GROUP=\"kvm\", MODE=\"0660\"), or an ACL (setfacl -m \
                 u:USER:rw), or, in a jail, the device node in it; vm.unprivileged_userfaultfd \
                 serves no VMM, as it is for the userfaultfd(2) system call, which the VMM's seccomp \
                 filter refuses",
                path.display()
            )
        );
    }

    /// The ids a VMM in a user namespace says are the host's, as its maps have them: the guest
    /// tests' runner's VMMs are root in theirs, which a device's access is not granted to.
    #[test]
    fn ids_in_a_user_namespace_are_said_as_the_host_has_them() {
        let root_as_ramon = "         0       1000          1\n         1     100000      65536\n";
        assert_eq!(host_id(0, root_as_ramon), Some(1000));
        assert_eq!(host_id(7, root_as_ramon), Some(100006));
        assert_eq!(host_id(65537, root_as_ramon), None);
        assert_eq!(host_id(1000, "         0          0 4294967295\n"), Some(1000));
        assert_eq!(host_id(0, ""), None);
    }

    /// The fault thread resolves faults concurrently: a fault waiting for the pager does not hold
    /// up the next, whose page request reaches the pager first.
    #[test]
    fn faults_are_resolved_concurrently() {
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        let pager = TestSource::paged(2);
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, 2 * ps as u64, None) }.unwrap();
        let (dax, _window) = Dax::in_host_memory(stage).unwrap();
        dax.window.map(0, &pager.source(), 0, 2 * ps as u64);

        // SAFETY: The memory is the window's, whose faults the fault thread serves.
        let load =
            move |offset: usize| move || unsafe { ((base + offset) as *const u8).read_volatile() };
        let first = std::thread::spawn(load(0));
        assert_eq!(pager.request(), 0);
        let second = std::thread::spawn(load(ps));
        assert_eq!(pager.request(), 1);

        pager.supply(1, 2);
        assert_eq!(second.join().unwrap(), 2);
        assert!(!first.is_finished());
        pager.supply(0, 1);
        assert_eq!(first.join().unwrap(), 1);

        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory, 2 * ps) }, 0);
    }

    use futures::future::BoxFuture;

    /// A window whose faults fail with EIO, as its daemon's connection's do once the daemon went,
    /// lost, or not.
    struct Failing {
        lost: bool,
    }

    impl Faults for Failing {
        fn size(&self) -> u64 {
            page_size()
        }

        fn fault_page(&self, _: u64) -> BoxFuture<'_, Result<()>> {
            Box::pin(std::future::ready(Err(Errno::EIO)))
        }

        fn probe(&self, _: u64) -> BoxFuture<'_, Result<()>> {
            Box::pin(std::future::ready(Err(Errno::EIO)))
        }

        fn lost(&self) -> bool {
            self.lost
        }
    }

    /// cladefs TASK-231: a fault its window fails as its daemon goes waits for the VM process to
    /// stop, as it does then, CladefsdLost, rather than abort it: aborted, the VM died of SIGABRT,
    /// a VM of fxfs-guest-soak's (GCE r830 round 27), where a VM whose daemon goes exits 158.
    #[test]
    fn faults_failed_as_their_daemon_goes_wait() {
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        // SAFETY: Only the test uses the memory, which outlives the stage.
        let stage = unsafe { UffdSecondStage::in_process(base, ps as u64, None) }.unwrap();
        let window = Failing { lost: true };
        let fault = stage.resolve_through(&window, base, Access::Read, false, 0);
        assert!(futures::FutureExt::now_or_never(fault).is_none(), "the fault returned");
        drop(stage);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory, ps) }, 0);
    }

    /// cladefs TASK-79.8: a second stage let go of maps nothing from then on, and no change the
    /// window makes reaches its memory, which nothing can access: what the thread receiving from a
    /// daemon gone makes still.
    #[test]
    fn released_stages_change_nothing() {
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        let file = TestSource::filled(2, 7).source();
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, 2 * ps as u64, None) }.unwrap();
        let (dax, _window) = Dax::in_host_memory(stage.clone()).unwrap();
        dax.window.map(0, &file, 0, ps as u64);
        // SAFETY: The memory is the window's, whose faults the fault thread serves.
        assert_eq!(unsafe { (base as *const u8).read_volatile() }, 7);

        stage.release();
        dax.window.map(ps as u64, &file, ps as u64, ps as u64);
        stage.unmap(0, ps as u64, Private::End);
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let window: Vec<&str> = (maps.lines())
            .filter(|line| {
                let (start, end) = line.split_whitespace().next().unwrap().split_once('-').unwrap();
                let start = usize::from_str_radix(start, 16).unwrap();
                let end = usize::from_str_radix(end, 16).unwrap();
                start < base + 2 * ps && end > base
            })
            .map(|line| line.split_whitespace().nth(1).unwrap())
            .collect();
        assert_eq!(window, ["---p"]);

        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory, 2 * ps) }, 0);
    }

    /// cladefs TASK-112.1: the pages the guest asks for ahead of its reads, of what the window
    /// maps, are installed, and are those the hypervisor is told of, the runs of them installed;
    /// not those it maps nothing at, nor past the window. TASK-276.6: the window's maps install
    /// none, nor tell the hypervisor of any: a `MAP` names pages, their first touch installs them.
    #[test]
    fn pages_asked_for_ahead_are_installed_and_prefaulted() {
        struct Told(Mutex<Vec<(u64, u64)>>);
        impl Prefault for Told {
            fn prefault(&self, offset: u64, len: u64) {
                self.0.lock().unwrap().push((offset, len));
            }
        }
        let ps = page_size();
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4 * ps as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let file = TestSource::filled(4, 7).source();
        let told = Arc::new(Told(Mutex::new(Vec::new())));
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage =
            unsafe { UffdSecondStage::in_process(memory as usize, 4 * ps, Some(told.clone())) }
                .unwrap();
        let (dax, _window) = Dax::in_host_memory(stage.clone()).unwrap();
        dax.window.map(0, &file, 0, 2 * ps);
        dax.window.map(3 * ps, &file, 3 * ps, ps);
        assert_eq!(*told.0.lock().unwrap(), []);
        assert_eq!(stage.installed(), 0, "the window's maps installed pages");

        stage.prefault(0, 4 * ps);
        assert_eq!(std::mem::take(&mut *told.0.lock().unwrap()), [(0, 2 * ps), (3 * ps, ps)]);
        assert_eq!(stage.installed(), 2, "the runs asked for ahead aren't installed");
        stage.prefault(2 * ps, 8 * ps);
        assert_eq!(std::mem::take(&mut *told.0.lock().unwrap()), [(3 * ps, ps)]);
        stage.prefault(4 * ps, ps);
        assert_eq!(*told.0.lock().unwrap(), []);
        // SAFETY: The window's memory, installed, which nothing else accesses.
        let read = |page: u64| unsafe {
            ((memory as usize + (page * ps) as usize) as *const u8).read_volatile()
        };
        assert_eq!([read(0), read(1), read(3)], [7; 3]);

        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory, 4 * ps as usize) }, 0);
    }

    /// A window whose pages are scattered over its arena's memory keeps no more runs installed
    /// than its budget, and so few mappings, leaving out those used longest ago, which read as
    /// they were when the guest touches them again (TASK-12).
    #[test]
    fn windows_keep_their_mappings_within_the_budget() {
        const PAGES: usize = 64;
        const BUDGET: usize = 8;
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGES * ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        // The file's pages are scattered: a run each.
        let file = TestSource::scattered(PAGES as u64, |page| page as u8 + 1).source();
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe {
            UffdSecondStage::with_mappings(
                base,
                (PAGES * ps) as u64,
                Memories::InProcess,
                None,
                Mappings::new(BUDGET),
            )
        }
        .unwrap();
        let (dax, _window) = Dax::in_host_memory(stage.clone()).unwrap();
        dax.window.map(0, &file, 0, (PAGES * ps) as u64);

        // The mappings in the window's memory: its runs installed, and what lies between them.
        let mappings = || {
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
            maps.lines()
                .filter(|line| {
                    let range = line.split_whitespace().next().unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let (start, end) = (
                        usize::from_str_radix(start, 16).unwrap(),
                        usize::from_str_radix(end, 16).unwrap(),
                    );
                    start < base + PAGES * ps && end > base
                })
                .count()
        };
        let left_out = || {
            let pages = stage.pages.lock().unwrap();
            pages
                .pages
                .iter()
                .filter(|page| page.mapped.is_some_and(|mapped| !mapped.installed()))
                .count()
        };
        let within = |when: &str| {
            let runs = stage.installed() as usize;
            assert!(runs <= BUDGET, "{when}: {runs} runs installed");
            assert!(mappings() <= 2 * BUDGET + 1, "{when}: {} mappings", mappings());
        };
        within("mapped");
        assert!(left_out() >= PAGES - BUDGET, "{} pages left out", left_out());

        // SAFETY: The memory is the window's, whose faults the fault thread serves.
        let load = |page: usize| unsafe { ((base + page * ps) as *const u8).read_volatile() };
        for round in 0..2 {
            for page in 0..PAGES {
                assert_eq!(load(page), page as u8 + 1, "round {round}, page {page}");
                within(&format!("round {round}, page {page}"));
            }
        }

        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory, PAGES * ps) }, 0);
    }

    /// TASK-91.16: the changes that take access away from pages installed are counted by how,
    /// each a flush of the host's TLBs and the hypervisor's: a private page made over them (below,
    /// `private_pages_made_over_pages_installed_are_counted`), and any other change, which leaves
    /// them out (TASK-276.6: a map names pages, installing none, and takes away what was there). A
    /// map where nothing is installed takes nothing away, as one of pages never touched since they
    /// were mapped does not, nor does a map of the very same pages installed, left in place.
    #[test]
    fn changes_taking_access_from_pages_installed_are_counted() {
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        let file = TestSource::filled(2, 1).source();
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, (2 * ps) as u64, None) }.unwrap();
        let (dax, _window) = Dax::in_host_memory(stage.clone()).unwrap();
        dax.window.map(0, &file, 0, (2 * ps) as u64);
        assert_eq!(stage.taken(), [0, 0], "mapped where nothing was");
        let phys = |page: usize| stage.pages.lock().unwrap().pages[page].mapped.unwrap().phys;
        let (first, second) = (phys(0), phys(1));
        // The first page's own, in place if installed; and `at_second` at the second's.
        let remap = |at_second| {
            stage.remap(
                0,
                (2 * ps) as u64,
                Private::End,
                &[
                    Anew::Map { offset: 0, phys: first, len: ps as u64 },
                    Anew::Map { offset: ps as u64, phys: at_second, len: ps as u64 },
                ],
            )
        };
        remap(second);
        assert_eq!(stage.taken(), [0, 0], "pages never touched taken away");
        // SAFETY: The window's memory, whose faults the fault thread serves.
        let load = |page: usize| unsafe { ((base + page * ps) as *const u8).read_volatile() };
        assert_eq!([load(0), load(1)], [1, 1]);
        remap(first);
        assert_eq!(stage.taken(), [0, 1], "the second page left out, the first in place");
        stage.unmap(0, (2 * ps) as u64, Private::End);
        assert_eq!(stage.taken(), [0, 2], "the first left out, the second never touched");
        stage.unmap(0, (2 * ps) as u64, Private::End);
        assert_eq!(stage.taken(), [0, 2], "nothing installed to leave out");

        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory, 2 * ps) }, 0);
    }

    /// TASK-12: a window of more scattered pages than a process may make mappings
    /// (`vm.max_map_count`), each a run of its own, read through whole, twice: within the budget a
    /// VM process keeps there, every page reads as it should, where a mapping each would fail.
    /// The count is Linux's default, or the host's if lower: at the host's own, Ubuntu's 1,048,576,
    /// its pages took ~8 GiB of shared memory, past a test's 8 GiB limit (TASK-342).
    #[test]
    fn windows_larger_than_the_map_count_read_whole() {
        const LINUX_DEFAULT: u64 = 65_530;
        let max = host_max_map_count().map_or(LINUX_DEFAULT, |host| host.min(LINUX_DEFAULT));
        let budget = budget(Some(max));
        let pages = max as usize + MAPPINGS_RESERVED;
        assert!(pages > budget, "{pages} pages, a budget of {budget} runs");
        let ps = page_size() as usize;
        // Room for the file and the pages between its pages.
        let memory = (4 * pages * ps).next_multiple_of(1 << 30) as u64;
        crate::pages::configure(crate::pages::Config::with_memory(memory)).unwrap();
        // SAFETY: Maps anonymous memory, which this test owns.
        let window = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                pages * ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(window, libc::MAP_FAILED);
        let base = window as usize;
        // The file's pages are scattered: a run each.
        let tag = |page: usize| (page % 251) as u8 + 1;
        let file = TestSource::scattered(pages as u64, |page| tag(page as usize)).source();
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe {
            UffdSecondStage::with_mappings(
                base,
                (pages * ps) as u64,
                Memories::InProcess,
                None,
                Mappings::new(budget),
            )
        }
        .unwrap();
        let (dax, _window) = Dax::in_host_memory(stage.clone()).unwrap();
        dax.window.map(0, &file, 0, (pages * ps) as u64);
        // SAFETY: The memory is the window's, whose faults the fault thread serves.
        let load = |page: usize| unsafe { ((base + page * ps) as *const u8).read_volatile() };
        for round in 0..2 {
            for page in 0..pages {
                assert_eq!(load(page), tag(page), "round {round}, page {page}");
            }
            let runs = stage.installed() as usize;
            assert!(runs <= budget, "round {round}: {runs} runs installed");
        }
        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(window, pages * ps) }, 0);
    }

    /// An access to a page the window maps nothing at, as x86 KVM makes for a guest that has
    /// unmapped the page (see the module docs), fails with `EFAULT`, rather than ending the
    /// process. Mapped again, from memory not yet in memory, as a file's pages before Fxfs's pager
    /// supplies them, the page is accessible: it is mapped here only as a fault brings it in. The
    /// accesses are the kernel's, as KVM's are: a pipe's writes from the page.
    #[test]
    fn accesses_to_pages_unmapped_fail() {
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, ps as u64, None) }.unwrap();
        let (dax, _window) = Dax::in_host_memory(stage).unwrap();
        let mut pipe = [0; 2];
        // SAFETY: Creates a pipe, which the test closes.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // SAFETY: Writes from the window's memory, which outlives the call.
        let write = || unsafe { libc::write(pipe[1], memory, ps) };

        assert_eq!(write(), -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EFAULT));

        // Not yet written, the source has no page in memory, and its first is zeros.
        let source = TestSource::anonymous(1).source();
        dax.window.map(0, &source, 0, ps as u64);
        assert_eq!(write(), ps as isize);
        let mut read = vec![1u8; ps];
        // SAFETY: Reads into `read`, which is `ps` bytes.
        assert_eq!(unsafe { libc::read(pipe[0], read.as_mut_ptr().cast(), ps) }, ps as isize);
        assert!(read.iter().all(|byte| *byte == 0), "the page mapped again reads otherwise");

        drop(dax);
        // SAFETY: Closes the pipe and unmaps the memory, which nothing uses any more.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            assert_eq!(libc::munmap(memory, ps), 0);
        }
    }

    /// cladefs TASK-164.1 (backlog doc-13): on a window that copies on write, a write the kernel
    /// makes to a page not here for a thread that isn't the guest's, as KVM's asynchronous page
    /// fault's worker makes for a vCPU's load and write alike, copies nothing: resolved as a load,
    /// the page restricted to reads here, the write fails (`EFAULT`). The guest's own thread's
    /// write fails too then, until a guest's access failing lifts the restriction
    /// (`guest_access_failed`): it faults as a write, and is made, in a private copy, the
    /// extent's page as it was. The kernel's writes are a pipe's reads into the window.
    #[test]
    fn worker_writes_copy_nothing_and_the_guests_writes_copy() {
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, ps as u64, None) }.unwrap();
        let (mut dax, _window) = Dax::in_host_memory_copying(stage.clone(), Some(1)).unwrap();
        // Not in memory as it is mapped, and zeros as it is brought in: not here until a fault.
        let file = TestSource::anonymous(1);
        dax.map(0, &file.source(), 0, ps as u64);
        let mut pipe = [0; 2];
        // SAFETY: Creates a pipe, which the test closes.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // A byte of the pipe's read into the window's page, as the kernel writes it.
        let read_into = move |base: usize| -> std::result::Result<(), i32> {
            // SAFETY: Writes a byte to the pipe, and reads one into the window's memory.
            unsafe {
                assert_eq!(libc::write(pipe[1], [7u8].as_ptr().cast(), 1), 1);
                match libc::read(pipe[0], base as *mut libc::c_void, 1) {
                    1 => Ok(()),
                    _ => Err(std::io::Error::last_os_error().raw_os_error().unwrap()),
                }
            }
        };

        let worker = std::thread::spawn(move || read_into(base)).join().unwrap();
        assert_eq!(worker, Err(libc::EFAULT));
        assert_eq!(stage.private_pages(), 0, "a worker's write copied the page");
        guest_thread_started();
        assert_eq!(read_into(base), Err(libc::EFAULT));
        guest_access_failed();
        // Lifted by the fault thread, as it is signalled.
        while stage.pages.lock().unwrap().pages[0].restricted {
            std::thread::yield_now();
        }
        assert_eq!(read_into(base), Ok(()));
        assert_eq!(stage.private_pages(), 1);
        // SAFETY: The window's page, private now, which nothing else accesses.
        assert_eq!(unsafe { (base as *const u8).read_volatile() }, 7);
        assert_eq!(file.byte(0), 0, "the write reached the extent's page");

        drop(dax);
        // SAFETY: Closes the pipe and unmaps the memory, which nothing uses any more.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            assert_eq!(libc::munmap(memory, ps), 0);
        }
    }

    /// cladefs: on a window that copies on write, a write to a page installed here,
    /// write-protected, is a write only if a guest's thread makes it (TASK-191): any other's, as
    /// KVM's worker's, which writes for loads too and may find the page a vCPU's load missed
    /// installed by the time it runs, is resolved as a load, the page restricted to reads, and
    /// fails, nothing copied; a vCPU's write fails while the restriction stands, and once a failure
    /// lifts it, as the VMM asks, is a write, made in a private copy. Red with any thread's write
    /// taken as the guest's: the worker's made a private page no guest counted. A test of a
    /// worker's write needn't keep its page from here any more, as
    /// `a_write_after_a_workers_write_waits_for_one_lift` did by mapping zeros, which a host's
    /// `vm.max_map_count` past OrbStack's kept installed (GCE r691, TASK-190).
    #[test]
    fn writes_to_pages_here_are_writes_of_the_guests_alone() {
        let ps = page_size() as usize;
        // SAFETY: Maps anonymous memory, which this test owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let base = memory as usize;
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, ps as u64, None) }.unwrap();
        let (mut dax, _window) = Dax::in_host_memory_copying(stage.clone(), Some(1)).unwrap();
        // In memory as it is mapped, installed here as a load first touches it.
        let file = TestSource::filled(1, 9);
        dax.map(0, &file.source(), 0, ps as u64);
        // SAFETY: The window's memory, whose faults the fault thread serves.
        assert_eq!(unsafe { (base as *const u8).read_volatile() }, 9);
        let installed =
            stage.pages.lock().unwrap().pages[0].mapped.map(|mapped| mapped.installed());
        assert_eq!(installed, Some(true));
        let mut pipe = [0; 2];
        // SAFETY: Creates a pipe, which the test closes.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let read_into = move |base: usize| -> std::result::Result<(), i32> {
            // SAFETY: Writes a byte to the pipe, and reads one into the window's memory.
            unsafe {
                assert_eq!(libc::write(pipe[1], [7u8].as_ptr().cast(), 1), 1);
                match libc::read(pipe[0], base as *mut libc::c_void, 1) {
                    1 => Ok(()),
                    _ => Err(std::io::Error::last_os_error().raw_os_error().unwrap()),
                }
            }
        };

        let extent_byte = || file.byte(0);
        // A thread not the guest's, as KVM's worker is.
        let worker = std::thread::spawn(move || read_into(base)).join().unwrap();
        assert_eq!(worker, Err(libc::EFAULT), "a worker's write was made");
        assert_eq!(stage.private_pages(), 0, "a worker's write copied the page");
        assert_eq!(extent_byte(), 9, "the worker's write reached the extent's page");
        // A vCPU's, which fails while the worker's restriction stands, and is a write once lifted.
        let vcpu = std::thread::spawn(move || {
            crate::guest_thread_started();
            let restricted = read_into(base);
            crate::guest_access_failed();
            (restricted, read_into(base))
        });
        assert_eq!(vcpu.join().unwrap(), (Err(libc::EFAULT), Ok(())));
        assert_eq!(stage.private_pages(), 1);
        // SAFETY: The window's page, private now, which nothing else accesses.
        let page = unsafe { std::slice::from_raw_parts(base as *const u8, ps) };
        assert_eq!(page[0], 7);
        assert!(page[1..].iter().all(|&byte| byte == 9), "the private page holds other data");
        assert_eq!(extent_byte(), 9, "the vCPU's write reached the extent's page");

        drop(dax);
        // SAFETY: Closes the pipe and unmaps the memory, which nothing uses any more.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            assert_eq!(libc::munmap(memory, ps), 0);
        }
    }
    /// cladefs TASK-276.6 (backlog doc-13: a `MAP` names pages, their first touch installs them):
    /// pages the window maps are installed only as they are first touched. A guest's write, its
    /// first touch of a page, makes the page private in place of nothing, and so does a `PRIVATE`
    /// of a page never touched: neither takes a page installed away, which the stage counts, each
    /// a TLB flush, and the window's memory stays one mapping, its private memory's. A load's
    /// first touch installs its page; a `PRIVATE` of it then takes it away, counted.
    #[test]
    fn first_touches_of_pages_named_install_nothing_to_write() {
        let ps = page_size() as usize;
        let base = window_memory(3);
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, (3 * ps) as u64, None) }.unwrap();
        let (mut dax, _window) = Dax::in_host_memory_copying(stage.clone(), Some(3)).unwrap();
        let file = TestSource::filled(3, 9).source();
        dax.map(0, &file, 0, (3 * ps) as u64);
        assert_eq!(stage.installed(), 0, "the window's map installed pages");
        let mut pipe = [0; 2];
        // SAFETY: Creates a pipe, which the test closes.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // A vCPU's write of a byte to the page at `addr`: the pipe's read into it, as the kernel
        // writes for the guest, on a guest's thread.
        let write_byte = move |addr: usize| {
            std::thread::spawn(move || {
                crate::guest_thread_started();
                // SAFETY: Writes a byte to the pipe, and reads it into the window's memory.
                unsafe {
                    assert_eq!(libc::write(pipe[1], [7u8].as_ptr().cast(), 1), 1);
                    libc::read(pipe[0], addr as *mut libc::c_void, 1)
                }
            })
            .join()
            .unwrap()
        };
        assert_eq!(write_byte(base), 1, "the write's first touch failed");
        futures::executor::block_on(stage.make_private(dax.window.as_ref(), ps as u64, ps as u64))
            .unwrap();
        assert_eq!(stage.private_pages(), 2);
        assert_eq!(stage.taken(), [0, 0], "a first touch took a page installed away");
        let mappings = mappings_within(base, 2 * ps);
        assert_eq!(mappings.len(), 1, "the private pages' mappings: {mappings:#?}");
        assert!(mappings[0].contains("cladefs-private-pages"), "{mappings:#?}");
        // SAFETY: The window's memory, whose faults the fault thread serves.
        let read = |addr: usize| unsafe { std::slice::from_raw_parts(addr as *const u8, ps) };
        assert_eq!((read(base)[0], read(base)[1], read(base + ps)[0]), (7, 9, 9));
        assert_eq!(read(base + 2 * ps)[0], 9);
        assert_eq!(stage.installed(), 1, "a load's first touch installed nothing");
        futures::executor::block_on(stage.make_private(
            dax.window.as_ref(),
            (2 * ps) as u64,
            ps as u64,
        ))
        .unwrap();
        assert_eq!(stage.taken(), [1, 0], "a PRIVATE over a page installed, uncounted");

        drop(dax);
        // SAFETY: Closes the pipe and unmaps the memory, which nothing uses any more.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            assert_eq!(libc::munmap(base as *mut libc::c_void, 3 * ps), 0);
        }
    }

    /// cladefs doc-26 (spec rules evictionKeepsPrivate, mapsKeepPrivate): a guest's write makes its
    /// page private here, a copy of the extent's page; the eviction of the extent's page, the
    /// host's unmap, leaves it, the write in it; so does the window's map of a page there after,
    /// as a page-in of the part's other pages makes, the window knowing nothing of it; the guest's
    /// own `UNMAP` ends it, its private memory emptied.
    #[test]
    fn evictions_and_maps_keep_private_pages_and_unmaps_end_them() {
        let ps = page_size() as usize;
        let base = window_memory(2);
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, (2 * ps) as u64, None) }.unwrap();
        let (mut dax, _window) = Dax::in_host_memory_copying(stage.clone(), Some(2)).unwrap();
        let file = TestSource::evictable(2, 9);
        dax.map(0, &file.source(), 0, (2 * ps) as u64);
        std::thread::spawn(move || {
            crate::guest_thread_started();
            // SAFETY: The window's memory, whose faults the fault thread serves.
            unsafe { (base as *mut u8).write_volatile(7) };
        })
        .join()
        .unwrap();
        // SAFETY: The window's memory, whose faults the fault thread serves.
        let read = |at: usize| unsafe { ((base + at) as *const u8).read_volatile() };
        assert_eq!((stage.private_pages(), read(0), read(1)), (1, 7, 9), "written");
        assert_eq!(file.byte(0), 9, "the write reached the extent's page");

        file.evict(&dax.window, 0);
        assert_eq!((stage.private_pages(), read(0), read(1)), (1, 7, 9), "evicted");
        stage.map(0, file.location(1), ps as u64);
        assert_eq!((stage.private_pages(), read(0), read(1)), (1, 7, 9), "mapped over");

        dax.unmap(0, ps as u64);
        assert_eq!(stage.private_pages(), 0, "unmapped by the guest");
        assert_eq!(stage.private_runs(), Ok(Vec::new()), "its private memory filled still");

        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(base as *mut libc::c_void, 2 * ps) }, 0);
    }

    /// The lines of /proc/self/maps of the mappings in the `len` bytes at `base`.
    fn mappings_within(base: usize, len: usize) -> Vec<String> {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        (maps.lines())
            .filter(|line| {
                let range = line.split_whitespace().next().unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start = usize::from_str_radix(start, 16).unwrap();
                let end = usize::from_str_radix(end, 16).unwrap();
                start < base + len && end > base
            })
            .map(str::to_owned)
            .collect()
    }

    /// `pages` pages of anonymous memory, for a window's second stage to take over.
    fn window_memory(pages: usize) -> usize {
        let len = pages * page_size() as usize;
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        // SAFETY: Maps anonymous memory, which the test owns.
        let memory = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, -1, 0) };
        assert_ne!(memory, libc::MAP_FAILED);
        memory as usize
    }

    /// cladefs TASK-227: a window as large as the data costs the VM process what the guest reaches
    /// of it, not its size: a 64 GiB window's stage takes a few MiB of this process's memory as it
    /// is made, and a page of it mapped takes a page more. Red with its pages' changes a Vec of
    /// each one's default: 48 bytes a page, 768 MiB as it was made.
    #[test]
    fn a_large_windows_stage_takes_the_memory_it_reaches() {
        const SIZE: usize = 64 << 30;
        let rss = || -> usize {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let line = status.lines().find(|line| line.starts_with("VmRSS:")).unwrap();
            line.split_whitespace().nth(1).unwrap().parse::<usize>().unwrap() << 10
        };
        let (prot, flags) = (
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
        );
        // SAFETY: Anonymous memory of the test's, reserved, which nothing accesses but the stage.
        let memory = unsafe { libc::mmap(std::ptr::null_mut(), SIZE, prot, flags, -1, 0) };
        assert_ne!(memory, libc::MAP_FAILED);
        let before = rss();
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe {
            UffdSecondStage::with_mappings(
                memory as usize,
                SIZE as u64,
                Memories::InProcess,
                None,
                Mappings::new(64),
            )
        }
        .unwrap();
        let made = rss().saturating_sub(before);
        assert!(made < 16 << 20, "a 64 GiB window's stage took {made} bytes as it was made");
        drop(stage);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory, SIZE) }, 0);
    }

    /// cladefs TASK-227: zero bytes are a page that nothing changed nor maps, which a window's
    /// pages are as their memory is made ([`WindowPages`]): `Option<Mapped>`'s `None` is `used`'s
    /// 0, `installed` a byte with no spare values for it.
    #[test]
    fn zero_bytes_are_a_page_nothing_changed() {
        // SAFETY: Zero bytes, valid for each field: none is a reference, and `Option<Mapped>`'s
        // only spare value is `used`'s 0, its `None`.
        let zeros: PageChanges = unsafe { std::mem::zeroed() };
        let nothing = PageChanges::default();
        assert_eq!(
            (
                zeros.made,
                zeros.withheld,
                zeros.restricted,
                zeros.for_worker,
                zeros.stale,
                zeros.private_mask
            ),
            (
                nothing.made,
                nothing.withheld,
                nothing.restricted,
                nothing.for_worker,
                nothing.stale,
                nothing.private_mask
            )
        );
        assert_eq!((zeros.changed, zeros.mapped.is_none()), (nothing.changed, true));
        let pages = WindowPages::new(4).unwrap();
        assert!(pages.iter().all(|page| {
            page.mapped.is_none() && !page.is_private() && !page.stale && page.made == 0
        }));
    }

    /// cladefs TASK-164.1 (backlog doc-13): a window's private pages cost its VM process no mapping
    /// each: made private between pages installed of an extent scattered over its arena, a run of
    /// its own each, and in an order no allocator's runs follow, the last page first, those
    /// adjacent in the window merge into one mapping, with the memory around them, the window's
    /// private memory, laid out as the window is. Each reads what it copied, and holds what is
    /// stored to it. Red with each private page a page of the tenant's arena mapped on its own, as
    /// r544's 110,857 were: 64 mappings.
    #[test]
    fn adjacent_private_pages_take_one_mapping() {
        const PAGES: usize = 64;
        let ps = page_size() as usize;
        let base = window_memory(PAGES);
        let size = (PAGES * ps) as u64;
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe { UffdSecondStage::in_process(base, size, None) }.unwrap();
        let (mut dax, _window) =
            Dax::in_host_memory_copying(stage.clone(), Some(PAGES as u64)).unwrap();
        let tag = |page: usize| (page % 251) as u8 + 1;
        let file = TestSource::scattered(PAGES as u64, |page| tag(page as usize)).source();
        dax.map(0, &file, 0, size);
        for page in 0..PAGES {
            // SAFETY: The window's memory, whose faults the fault thread serves, installed so.
            assert_eq!(unsafe { ((base + page * ps) as *const u8).read_volatile() }, tag(page));
        }
        assert_eq!(stage.installed(), PAGES as u64, "the loads installed no run each");
        for page in (0..PAGES).rev() {
            let (offset, len) = ((page * ps) as u64, ps as u64);
            futures::executor::block_on(stage.make_private(dax.window.as_ref(), offset, len))
                .unwrap();
        }
        assert_eq!(stage.private_pages(), PAGES as u64);
        let mappings = mappings_within(base, PAGES * ps);
        assert_eq!(mappings.len(), 1, "the private pages' mappings: {mappings:#?}");
        assert!(mappings[0].contains("cladefs-private-pages"), "{mappings:#?}");
        for page in 0..PAGES {
            let at = (base + page * ps) as *mut u8;
            // SAFETY: The window's memory, private pages, whose faults the fault thread serves.
            let (copied, stored) = unsafe {
                at.add(1).write_volatile(0xee);
                (at.read_volatile(), at.add(1).read_volatile())
            };
            assert_eq!((copied, stored), (tag(page), 0xee), "page {page}");
        }

        drop(dax);
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(base as *mut libc::c_void, PAGES * ps) }, 0);
    }

    /// cladefs TASK-164.1 (backlog doc-13): a window installing more runs than its budget trims
    /// them, but never its private pages: each stays in place, its private memory's, which the
    /// kernel reaches without a fault (a pipe's write from it), holding what was copied and stored
    /// there. Red with each private page a run, which a trim left out (r544: the private pages
    /// thrice the budget, the fault thread trimming, its lifts of the writes it refused waiting).
    #[test]
    fn trims_leave_private_pages_in_place() {
        const PAGES: usize = 32;
        const BUDGET: usize = 4;
        let ps = page_size() as usize;
        let base = window_memory(PAGES);
        let size = (PAGES * ps) as u64;
        // SAFETY: Only the test uses the memory, which outlives the window.
        let stage = unsafe {
            UffdSecondStage::with_mappings(
                base,
                size,
                Memories::InProcess,
                None,
                Mappings::new(BUDGET),
            )
        }
        .unwrap();
        let (mut dax, _window) =
            Dax::in_host_memory_copying(stage.clone(), Some(PAGES as u64)).unwrap();
        let tag = |page: usize| (page % 251) as u8 + 1;
        let file = TestSource::scattered(PAGES as u64, |page| tag(page as usize)).source();
        dax.map(0, &file, 0, size);
        // Every other page private, stored to.
        for page in (0..PAGES).step_by(2) {
            let (offset, len) = ((page * ps) as u64, ps as u64);
            futures::executor::block_on(stage.make_private(dax.window.as_ref(), offset, len))
                .unwrap();
            // SAFETY: The window's private page, which nothing else accesses.
            unsafe { ((base + page * ps + 1) as *mut u8).write_volatile(0xee) };
        }
        // The others loaded, each a run installed, many times the budget, twice.
        for _ in 0..2 {
            for page in (1..PAGES).step_by(2) {
                // SAFETY: The window's memory, whose faults the fault thread serves.
                let loaded = unsafe { ((base + page * ps) as *const u8).read_volatile() };
                assert_eq!(loaded, tag(page), "page {page}");
                let runs = stage.installed() as usize;
                assert!(runs <= BUDGET, "page {page}: {runs} runs installed");
            }
        }
        let mut pipe = [0; 2];
        // SAFETY: Creates a pipe, which the test closes.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        for page in (0..PAGES).step_by(2) {
            let at = base + page * ps;
            let mapped = mappings_within(at, ps);
            assert!(mapped[0].contains("cladefs-private-pages"), "page {page}: {mapped:#?}");
            let mut read = [0u8; 2];
            // SAFETY: Writes the page's first bytes to the pipe, and reads them back.
            unsafe {
                assert_eq!(libc::write(pipe[1], at as *const libc::c_void, 2), 2, "page {page}");
                assert_eq!(libc::read(pipe[0], read.as_mut_ptr().cast(), 2), 2);
            }
            assert_eq!(read, [tag(page), 0xee], "page {page}");
        }
        assert_eq!(stage.private_pages(), (PAGES / 2) as u64);

        drop(dax);
        // SAFETY: Closes the pipe and unmaps the memory, which nothing uses any more.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            assert_eq!(libc::munmap(base as *mut libc::c_void, PAGES * ps), 0);
        }
    }

    /// A test's host memory: system calls, after each change of which the mappings in its windows'
    /// memory, as /proc/self/maps lists them, are counted, the most of them kept: what the process
    /// takes of the mappings the host lets it make (`vm.max_map_count`), which a test can't lower.
    struct Counted {
        syscalls: Syscalls,
        windows: Arc<CountedWindows>,
    }

    /// The memory of the windows a [`Counted`] host's mappings are counted in, and the most found.
    #[derive(Default)]
    struct CountedWindows {
        memory: std::sync::Mutex<Vec<std::ops::Range<usize>>>,
        most: AtomicUsize,
    }

    impl CountedWindows {
        /// The mappings in the windows' memory now.
        fn now(&self) -> usize {
            let memory = self.memory.lock().unwrap().clone();
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
            (maps.lines())
                .filter(|line| {
                    let range = line.split_whitespace().next().unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let start = usize::from_str_radix(start, 16).unwrap();
                    let end = usize::from_str_radix(end, 16).unwrap();
                    memory.iter().any(|window| start < window.end && end > window.start)
                })
                .count()
        }

        fn count(&self) {
            self.most.fetch_max(self.now(), Ordering::Relaxed);
        }
    }

    impl HostMemory for Counted {
        fn map(&self, addr: usize, len: usize, prot: libc::c_int, memory: Memory) {
            self.syscalls.map(addr, len, prot, memory);
            self.windows.count();
        }
        fn protect(&self, addr: usize, len: usize, prot: libc::c_int) {
            self.syscalls.protect(addr, len, prot);
            self.windows.count();
        }
        fn register(&self, addr: usize, len: usize, mode: u64) {
            self.syscalls.register(addr, len, mode);
            self.windows.count();
        }
        fn write_protect(&self, addr: usize, len: usize) {
            self.syscalls.write_protect(addr, len);
            self.windows.count();
        }
        fn write_unprotect(&self, addr: usize, len: usize) {
            self.syscalls.write_unprotect(addr, len);
            self.windows.count();
        }
        fn wake(&self, addr: usize, len: usize) {
            self.syscalls.wake(addr, len);
        }
        fn private_memory(&self, len: usize) -> std::io::Result<OwnedFd> {
            self.syscalls.private_memory(len)
        }
        fn fill(&self, fd: RawFd, offset: u64, bytes: &[u8]) {
            self.syscalls.fill(fd, offset, bytes);
        }
        fn write_subpage(&self, fd: RawFd, offset: u64, bytes: &[u8]) {
            self.syscalls.write_subpage(fd, offset, bytes);
        }
        fn punch(&self, fd: RawFd, offset: u64, len: usize) {
            self.syscalls.punch(fd, offset, len);
        }
    }

    /// The pages of each window of [`windows_keep_their_process_within_its_mappings`].
    const HOSTILE_PAGES: usize = 40;

    /// What a guest, its VM process's daemon and its faults do to one of the process's windows, of
    /// [`windows_keep_their_process_within_its_mappings`], of [`HOSTILE_PAGES`] pages, named at
    /// first of memory following on, so that a fault installs them all, one run, and named anew,
    /// scattered, a run each when installed, or following on.
    #[derive(Clone, Debug)]
    enum HostileOp {
        /// The guest's `MAP` of `len` pages from the page on: scattered, or following on.
        Name { window: usize, page: usize, len: usize, scattered: bool },
        /// A fault on the page, which installs it if it is named and left out.
        Fault { window: usize, page: usize },
        /// A `MAP_PREFAULT` of `len` pages from the page on.
        Prefault { window: usize, page: usize, len: usize },
        /// A fault the window refused, which withholds the page or restricts it to reads.
        Refused { window: usize, page: usize, withhold: bool },
        /// A worker's write, resolved as a load, which restricts the page to reads if installed.
        Worker { window: usize, page: usize },
        /// The guest's `UNMAP` of `len` pages from the page on.
        Unmap { window: usize, page: usize, len: usize },
        /// The window's change of `len` pages from the page on, each mapped anew, scattered.
        Remap { window: usize, page: usize, len: usize },
    }

    fn hostile_op() -> impl proptest::strategy::Strategy<Value = HostileOp> {
        use proptest::prelude::*;
        const LAST: usize = HOSTILE_PAGES - 1;
        let window = || 0usize..3;
        let page = || {
            prop_oneof![
                Just(0),
                Just(1),
                Just(HOSTILE_PAGES / 2),
                Just(LAST - 1),
                Just(LAST),
                0..=LAST
            ]
        };
        let len = || prop_oneof![Just(1), Just(2), Just(HOSTILE_PAGES), 1..=HOSTILE_PAGES];
        prop_oneof![
            2 => (window(), page(), len(), any::<bool>()).prop_map(|(window, page, len, scattered)| {
                HostileOp::Name { window, page, len, scattered }
            }),
            4 => (window(), page()).prop_map(|(window, page)| HostileOp::Fault { window, page }),
            2 => (window(), page(), len())
                .prop_map(|(window, page, len)| HostileOp::Prefault { window, page, len }),
            3 => (window(), page(), any::<bool>())
                .prop_map(|(window, page, withhold)| HostileOp::Refused { window, page, withhold }),
            2 => (window(), page()).prop_map(|(window, page)| HostileOp::Worker { window, page }),
            1 => (window(), page(), len())
                .prop_map(|(window, page, len)| HostileOp::Unmap { window, page, len }),
            1 => (window(), page(), len())
                .prop_map(|(window, page, len)| HostileOp::Remap { window, page, len }),
        ]
    }

    /// The runs a stage's pages hold, found by walking them all, as its trims once did.
    fn runs_walked(pages: &Pages) -> Vec<(usize, usize)> {
        let installed = |at: usize| pages.pages[at].mapped.is_some_and(|mapped| mapped.installed());
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for at in 0..pages.pages.len() {
            if pages.starts_run(at) {
                runs.push((at, at + 1));
            } else if installed(at) {
                runs.last_mut().expect("a page following on starts no run").1 = at + 1;
            }
        }
        runs
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// cladefs board 8493: a VM process's windows, one to three, share the mappings the host
        /// lets it make (`vm.max_map_count`, drawn at 0, 1, the reserve and its neighbours, the
        /// windows' pages' worth and theirs, `u64::MAX`, and between), whatever a hostile guest's
        /// accesses, maps and `MAP_PREFAULT`s, and its faults' restrictions, do to them: after
        /// every system call, the mappings in their memory are no more than their runs' and
        /// restrictions' shares take, two each and one, within what the host lets the process
        /// make beyond its reserve, counted once, or two runs a window if that is less; each
        /// window holds no more than its share; and its runs are what a walk of its pages finds.
        /// Red with each window budgeting the process's whole, with restrictions uncounted, and
        /// with a prefault trimming once it has installed all its runs; and with a run reaching
        /// past a change taken to end at it.
        #[test]
        fn windows_keep_their_process_within_its_mappings(
            max_map_count in proptest::prop_oneof![
                proptest::strategy::Just(0u64),
                proptest::strategy::Just(1),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64 - 1),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64 + 1),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64 + 4),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64 + 5),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64 + 2 * HOSTILE_PAGES as u64 - 1),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64 + 2 * HOSTILE_PAGES as u64),
                proptest::strategy::Just(MAPPINGS_RESERVED as u64 + 2 * HOSTILE_PAGES as u64 + 1),
                proptest::strategy::Just(u64::MAX),
                MAPPINGS_RESERVED as u64..MAPPINGS_RESERVED as u64 + 8 * HOSTILE_PAGES as u64,
            ],
            windows in 1usize..=3,
            ops in proptest::collection::vec(hostile_op(), 0..160),
        ) {
            let ps = page_size() as usize;
            // Each window's memory, a page apart, which no window registers.
            let stride = HOSTILE_PAGES + 1;
            let memory = window_memory(windows * stride);
            let counted = Arc::new(CountedWindows::default());
            let total = budget(Some(max_map_count));
            let mappings = Mappings::new(total);
            let source = TestSource::scattered(HOSTILE_PAGES as u64, |page| page as u8 + 1);
            let following = crate::slabs::alloc(
                crate::slabs::ArenaId::DEFAULT,
                HOSTILE_PAGES as u64,
                crate::slabs::Wait::No,
            )
            .unwrap();
            let following_at = |page: usize| crate::pages::PhysLocation {
                offset: following.location().offset + (page * ps) as u64,
                ..following.location()
            };
            let stages: Vec<Arc<UffdSecondStage<Counted>>> = (0..windows)
                .map(|window| {
                    let base = memory + window * stride * ps;
                    counted.memory.lock().unwrap().push(base..base + HOSTILE_PAGES * ps);
                    let host = Counted { syscalls: Syscalls::open().unwrap(), windows: counted.clone() };
                    let size = (HOSTILE_PAGES * ps) as u64;
                    // SAFETY: Only the test uses the memory, which outlives the stage.
                    unsafe {
                        UffdSecondStage::with_host(base, size, host, Memories::InProcess, None, mappings.clone())
                    }
                    .unwrap()
                })
                .collect();
            let shares: Vec<usize> = stages.iter().map(|stage| stage.budget()).collect();
            proptest::prop_assert!(
                shares.iter().fold(0, |sum, &share| usize::saturating_add(sum, share))
                    <= total.max(2 * windows),
                "shares {shares:?} of {total}"
            );
            let most = 2 * total.max(2 * windows) + windows;
            // Every page named, of memory following on.
            for stage in &stages {
                stage.map(0, following_at(0), (HOSTILE_PAGES * ps) as u64);
            }
            for op in &ops {
                let window = match *op {
                    HostileOp::Name { window, .. }
                    | HostileOp::Fault { window, .. } | HostileOp::Prefault { window, .. }
                    | HostileOp::Refused { window, .. } | HostileOp::Worker { window, .. }
                    | HostileOp::Unmap { window, .. } | HostileOp::Remap { window, .. } => window,
                };
                let Some(stage) = stages.get(window) else { continue };
                let at = |page: usize| stage.base + page * ps;
                let span = |page: usize, len: usize| ((page * ps) as u64, (len.min(HOSTILE_PAGES - page) * ps) as u64);
                match *op {
                    HostileOp::Name { page, len, scattered: false, .. } => {
                        let (offset, len) = span(page, len);
                        stage.map(offset, following_at(page), len);
                    }
                    HostileOp::Name { page, len, scattered: true, .. } => {
                        for at in page..(page + len).min(HOSTILE_PAGES) {
                            stage.map((at * ps) as u64, source.location(at as u64), ps as u64);
                        }
                    }
                    HostileOp::Fault { page, .. } => {
                        stage.reinstall(at(page));
                    }
                    HostileOp::Prefault { page, len, .. } => {
                        let (offset, len) = span(page, len);
                        stage.prefault(offset, len);
                    }
                    HostileOp::Refused { page, withhold, .. } => {
                        let prot = if withhold { libc::PROT_NONE } else { libc::PROT_READ };
                        stage.restrict(at(page), None, prot);
                    }
                    HostileOp::Worker { page, .. } => stage.restrict_for_worker(at(page)),
                    HostileOp::Unmap { page, len, .. } => {
                        let (offset, len) = span(page, len);
                        stage.unmap(offset, len, Private::End);
                    }
                    HostileOp::Remap { page, len, .. } => {
                        let (offset, len) = span(page, len);
                        let anew: Vec<Anew> = (0..len / ps as u64)
                            .map(|at| {
                                let page = offset / ps as u64 + at;
                                let phys = source.location((page * 7 + 3) % HOSTILE_PAGES as u64);
                                Anew::Map { offset: page * ps as u64, phys, len: ps as u64 }
                            })
                            .collect();
                        stage.remap(offset, len, Private::End, &anew);
                    }
                }
                let found = counted.most.load(Ordering::Relaxed);
                proptest::prop_assert!(found <= most, "{op:?}: {found} mappings, of {most} at most");
                let pages = stage.pages.lock().unwrap();
                let runs: Vec<(usize, usize)> = pages.runs.iter().map(|(&start, run)| (start, run.end)).collect();
                proptest::prop_assert_eq!(&runs, &runs_walked(&pages), "{:?}", op);
                let restricted = pages.pages.iter().filter(|page| page.restricts()).count();
                proptest::prop_assert_eq!(pages.restrictions, restricted, "{:?}", op);
                proptest::prop_assert!(
                    pages.held() <= stage.budget(),
                    "{op:?}: {} runs, {restricted} restricted, of a share of {}",
                    pages.runs.len(),
                    stage.budget()
                );
            }
            drop(stages);
            drop(following);
            // SAFETY: Unmaps the memory, which nothing uses any more.
            proptest::prop_assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, windows * stride * ps) }, 0);
        }
    }

    /// cladefs board 8493: a window's share grows back as another window of its process goes.
    #[test]
    fn windows_gone_give_back_their_share() {
        let ps = page_size() as usize;
        let mappings = Mappings::new(100);
        let stage = |memory: usize| {
            // SAFETY: Only the test uses the memory, which outlives the stage.
            unsafe {
                UffdSecondStage::with_mappings(
                    memory,
                    ps as u64,
                    Memories::InProcess,
                    None,
                    mappings.clone(),
                )
            }
            .unwrap()
        };
        let (first, second) = (window_memory(1), window_memory(1));
        let kept = stage(first);
        assert_eq!(kept.budget(), 100);
        let gone = stage(second);
        assert_eq!((kept.budget(), gone.budget()), (50, 50));
        drop(gone);
        assert_eq!(kept.budget(), 100);
        drop(kept);
        for memory in [first, second] {
            // SAFETY: Unmaps the memory, which nothing uses any more.
            assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, ps) }, 0);
        }
    }

    /// A host memory that changes nothing: what a test of it measures is the stage's own work.
    struct Unchanged;

    impl HostMemory for Unchanged {
        fn map(&self, _: usize, _: usize, _: libc::c_int, _: Memory) {}
        fn protect(&self, _: usize, _: usize, _: libc::c_int) {}
        fn register(&self, _: usize, _: usize, _: u64) {}
        fn write_protect(&self, _: usize, _: usize) {}
        fn write_unprotect(&self, _: usize, _: usize) {}
        fn wake(&self, _: usize, _: usize) {}
        fn private_memory(&self, _: usize) -> std::io::Result<OwnedFd> {
            Syscalls::private_memory_fd(page_size() as usize)
        }
        fn fill(&self, _: RawFd, _: u64, _: &[u8]) {}
        fn write_subpage(&self, _: RawFd, _: u64, _: &[u8]) {}
        fn punch(&self, _: RawFd, _: u64, _: usize) {}
    }

    #[test]
    fn private_budget_failure_leaves_the_whole_request_shared() {
        struct EvictingFaults<'a> {
            stage: &'a UffdSecondStage<Unchanged>,
            phys: crate::pages::PhysLocation,
            take_budget_on_retry: bool,
        }

        impl Faults for EvictingFaults<'_> {
            fn size(&self) -> u64 {
                3 * page_size()
            }

            fn fault_page(&self, offset: u64) -> BoxFuture<'_, Result<()>> {
                let ps = page_size();
                if offset == ps {
                    // The first page was named when PRIVATE checked it, then evicted before
                    // the fault on the second page finished.
                    self.stage.unmap(0, ps, Private::Keep);
                } else if self.take_budget_on_retry {
                    // A write fault outside the request takes one of its two available pages.
                    assert_eq!(offset, 0);
                    assert!(matches!(
                        self.stage.copy(self.stage.base + (2 * ps) as usize),
                        Copied::Private
                    ));
                } else {
                    // The cache can hold only one of the two pages at a time.
                    assert_eq!(offset, 0);
                    self.stage.unmap(ps, ps, Private::Keep);
                }
                self.stage.map(offset, self.phys, ps);
                Box::pin(std::future::ready(Ok(())))
            }

            fn probe(&self, _: u64) -> BoxFuture<'_, Result<()>> {
                panic!("unexpected probe");
            }
        }

        let ps = page_size();
        let file = TestSource::filled(1, 9);
        let phys = file.location(0);
        for take_budget_on_retry in [true, false] {
            // SAFETY: The host changes no memory at this address.
            let stage = unsafe {
                UffdSecondStage::with_host(
                    64 * ps as usize,
                    3 * ps,
                    Unchanged,
                    Memories::InProcess,
                    None,
                    Mappings::new(64),
                )
            }
            .unwrap();
            stage.copy_on_write(2).unwrap();
            stage.map(0, phys, ps);
            stage.map(2 * ps, phys, ps);
            let faults = EvictingFaults { stage: &stage, phys, take_budget_on_retry };
            assert_eq!(
                futures::executor::block_on(stage.make_private(&faults, 0, 2 * ps)),
                Err(Errno::ENOMEM)
            );
            assert_eq!(stage.private_pages(), u64::from(take_budget_on_retry));
            if take_budget_on_retry {
                assert!(stage.named(stage.base));
                assert!(stage.named(stage.base + ps as usize));
            }
        }
    }

    /// cladefs board 8495: a trim of a full 64 GiB window, every page named, budget + 1 runs
    /// installed across it, holds the pages' lock, which every fault takes, for a time that
    /// follows its runs and the pages it leaves out, not the window's 16.7M pages (of 4 KiB): the
    /// longest a fault waits behind it, measured by a thread taking the lock throughout, is a few
    /// milliseconds, unoptimized, leaving out an eighth of the runs, 512 pages each. Red with
    /// a trim walking every page of the window: on ubu1, unoptimized, each took up to 319 ms, and
    /// the thread taking the lock waited up to 7.7 s.
    #[test]
    fn trims_of_a_large_window_follow_its_runs() {
        const SIZE: u64 = 64 << 30;
        const BUDGET: usize = 1024;
        let ps = page_size() as usize;
        let pages = SIZE as usize / ps;
        // An arena of pages, which the window's are of: the stage changes no memory, so no page
        // is mapped.
        let arena = TestSource::filled(1, 0).location(0).arena;
        // SAFETY: The host changes no memory.
        let stage = unsafe {
            UffdSecondStage::with_host(
                64 * ps,
                SIZE,
                Unchanged,
                Memories::InProcess,
                None,
                Mappings::new(BUDGET),
            )
        }
        .unwrap();
        // Every page named, of memory following on: each fault installs a run of 512 pages.
        stage.map(0, crate::pages::PhysLocation { arena, offset: 0 }, SIZE);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let (stage, stop) = (stage.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut longest = std::time::Duration::ZERO;
                while !stop.load(Ordering::Relaxed) {
                    let start = std::time::Instant::now();
                    stage.installed();
                    longest = longest.max(start.elapsed());
                    std::thread::yield_now();
                }
                longest
            })
        };
        // Runs spread over the window, each a fault's, the trims among them timed.
        let (mut trims, mut longest) = (0, std::time::Duration::ZERO);
        let stride = pages / (4 * BUDGET);
        for round in 0..4 * BUDGET {
            let before = stage.installed();
            let start = std::time::Instant::now();
            stage.reinstall(64 * ps + round * stride * ps);
            let took = start.elapsed();
            if stage.installed() < before {
                trims += 1;
                longest = longest.max(took);
            }
            // Faults come apart, as a guest's accesses do: back to back, the lock, which is not
            // fair, went to this thread again and again, and the waiter waited through many.
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        stop.store(true, Ordering::Relaxed);
        let waited = waiter.join().unwrap();
        eprintln!(
            "{trims} trims of a {SIZE}-byte window of {pages} pages, {BUDGET} runs: the longest \
             took {longest:?}; the longest wait for the pages' lock {waited:?}"
        );
        assert!(trims >= 3, "{trims} trims");
        assert!(longest < std::time::Duration::from_millis(50), "a trim took {longest:?}");
        assert!(waited < std::time::Duration::from_millis(50), "a wait of {waited:?}");
    }

    /// Sub-host-page private tracking (e.g. 4 KiB blocks on a 16 KiB arm64 host, exercised on
    /// every host with `unit = page_size() / 4`):
    /// - `make_private` on sub-page 0 preserves sibling sub-pages' bytes and allows writing slot 0.
    /// - Remapping sibling sub-page 1 (with `MAP` / `Private::End`) updates slot 1's bytes on next
    ///   read WITHOUT losing slot 0's private dirty data.
    /// - Unmapping sibling sub-page 2 clears slot 2 without losing slot 0's private dirty data.
    /// - Rebinding sub-page 0 (`MAP` with `Private::End`) once no other sub-page is private drops
    ///   `private_pages()` back to 0 and empties `PrivateMemory` (`private_runs()` empty).
    #[test]
    fn sub_host_page_private_preserves_siblings_and_releases_when_all_end() {
        let ps = page_size() as usize;
        let unit = ps / 4;
        let memory = window_memory(2);
        // SAFETY: Only the test uses `memory`, which outlives the stage and window.
        let stage =
            unsafe { UffdSecondStage::in_process_with_unit(memory, (2 * ps) as u64, unit, None) }
                .unwrap();
        let (mut dax, window) =
            Dax::in_host_memory_copying_with_unit(stage.clone(), unit as u64, Some(2)).unwrap();

        let make_src = |tag: u8| TestSource::filled(1, tag).source();
        let src_a = make_src(0x11);
        let src_b = make_src(0x22);
        let src_c = make_src(0x33);
        let src_d = make_src(0x44);

        // Host page 0: [slot0=A, slot1=B, slot2=C, slot3=zeros].
        dax.map(0, &src_a, 0, unit as u64);
        dax.map(unit as u64, &src_b, 0, unit as u64);
        dax.map((2 * unit) as u64, &src_c, 0, unit as u64);
        dax.map_zeros((3 * unit) as u64, unit as u64);

        let read_slot = |slot: usize| {
            // SAFETY: `memory` is the window's memory, served by `dax`'s fault thread.
            unsafe { ((memory + slot * unit) as *const u8).read_volatile() }
        };
        let write_slot = |slot: usize, val: u8| {
            // SAFETY: `memory` is the window's memory, served by `dax`'s fault thread.
            unsafe { ((memory + slot * unit) as *mut u8).write_volatile(val) };
        };
        assert_eq!([read_slot(0), read_slot(1), read_slot(2), read_slot(3)], [0x11, 0x22, 0x33, 0]);

        // Make only slot 0 private and store 0xaa into slot 0.
        futures::executor::block_on(stage.make_private(&*dax.window, 0, unit as u64)).unwrap();
        assert_eq!(stage.private_pages(), 1);
        write_slot(0, 0xaa);
        assert_eq!([read_slot(0), read_slot(1), read_slot(2), read_slot(3)], [0xaa, 0x22, 0x33, 0]);

        // Remap sibling slot 1 from B (0x22) -> D (0x44): slot 0 MUST keep 0xaa, slot 1 MUST read 0x44.
        dax.map(unit as u64, &src_d, 0, unit as u64);
        assert_eq!(stage.private_pages(), 1);
        assert_eq!([read_slot(0), read_slot(1), read_slot(2), read_slot(3)], [0xaa, 0x44, 0x33, 0]);

        // Make slot 2 private as well and store 0xcc: still 1 host page private.
        futures::executor::block_on(stage.make_private(
            &*dax.window,
            (2 * unit) as u64,
            unit as u64,
        ))
        .unwrap();
        assert_eq!(stage.private_pages(), 1);
        write_slot(2, 0xcc);
        assert_eq!([read_slot(0), read_slot(1), read_slot(2), read_slot(3)], [0xaa, 0x44, 0xcc, 0]);

        // Unmap sibling slot 1: slots 0 and 2 keep their private bytes, slot 1 becomes 0.
        dax.unmap(unit as u64, unit as u64);
        assert_eq!(stage.private_pages(), 1);
        assert_eq!([read_slot(0), read_slot(1), read_slot(2), read_slot(3)], [0xaa, 0x00, 0xcc, 0]);

        // Rebind slot 0 to D (0x44): slot 2 is STILL private (0xcc), slot 0 now reads 0x44.
        dax.map(0, &src_d, 0, unit as u64);
        assert_eq!(stage.private_pages(), 1);
        assert_eq!([read_slot(0), read_slot(1), read_slot(2), read_slot(3)], [0x44, 0x00, 0xcc, 0]);

        // Rebind slot 2 to A (0x11): NO sub-page is private anymore -> host page is punched!
        dax.map((2 * unit) as u64, &src_a, 0, unit as u64);
        assert_eq!(stage.private_pages(), 0);
        assert_eq!(stage.private_runs(), Ok(Vec::new()));
        assert_eq!([read_slot(0), read_slot(1), read_slot(2), read_slot(3)], [0x44, 0x00, 0x11, 0]);

        // Review 100 finding 347: on host page 1, map ONLY slot 0 (`ps..ps+unit`) and leave
        // slots 1..3 unmapped. `make_private` on an unmapped slot must fail EINVAL, and a guest
        // write fault (`copy()`) on slot 0 must mark ONLY mapped slot 0 private so unmapping slot 0
        // drops `private_pages()` back to 0.
        dax.map(ps as u64, &src_a, 0, unit as u64);
        assert_eq!(
            futures::executor::block_on(stage.make_private(
                &*dax.window,
                (ps + unit) as u64,
                unit as u64,
            )),
            Err(Errno::EINVAL),
            "make_private on unmapped sub-page must fail EINVAL"
        );
        crate::guest_thread_started();
        write_slot(4, 0xee);
        assert_eq!(stage.private_pages(), 1);
        assert_eq!(read_slot(4), 0xee);
        assert_eq!(
            futures::executor::block_on(stage.make_private(
                &*dax.window,
                (ps + unit) as u64,
                unit as u64,
            )),
            Err(Errno::EINVAL),
            "make_private on unmapped sibling of private sub-page must fail EINVAL"
        );
        dax.unmap(ps as u64, unit as u64);
        assert_eq!(
            stage.private_pages(),
            0,
            "unmapping only mapped sub-page must free private page"
        );
        assert_eq!(stage.private_runs(), Ok(Vec::new()));

        drop((dax, window, stage));
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, 2 * ps) }, 0);
    }

    /// Review 100 finding 342: on a `writes_are_guests` (`MemoryCell`) window with sub-host-page
    /// granularity (`unit = page_size() / 4`), reclaiming one sub-page, reading it back, writing
    /// new bytes to it, and then reclaiming a sibling sub-page (or evicting the backing extent)
    /// must preserve the new write rather than clobbering it with old image bytes.
    #[test]
    fn sub_host_page_cow_reclaim_write_survives_sibling_reclaim() {
        let ps = page_size() as usize;
        let unit = ps / 4;
        let memory = window_memory(1);
        // SAFETY: Only the test uses `memory`, which outlives the stage and window.
        let stage = unsafe { UffdSecondStage::in_process_with_unit(memory, ps as u64, unit, None) }
            .unwrap();
        stage.writes_are_guests();
        let (mut dax, window) =
            Dax::in_host_memory_copying_with_unit(stage.clone(), unit as u64, Some(1)).unwrap();
        let src = TestSource::filled(1, 0x11).source();
        dax.map(0, &src, 0, ps as u64);

        let read_slot = |slot: usize| {
            // SAFETY: `memory` is the window's live mapping, served by `dax`'s fault thread.
            unsafe { ((memory + slot * unit) as *const u8).read_volatile() }
        };
        let write_slot = |slot: usize, val: u8| {
            // SAFETY: `memory` is the window's live mapping, served by `dax`'s fault thread.
            unsafe { ((memory + slot * unit) as *mut u8).write_volatile(val) };
        };

        // Write slot 0 -> whole host page becomes private (0b1111).
        write_slot(0, 0xdd);
        assert_eq!(stage.private_pages(), 1);
        // Reclaim slot 1 (`Private::End` via re-mapping the image), then read slot 1 back.
        dax.map(unit as u64, &src, unit as u64, unit as u64);
        assert_eq!([read_slot(0), read_slot(1)], [0xdd, 0x11]);
        // Write new data 0x55 into previously reclaimed slot 1 (must trap WP and re-track COW).
        write_slot(1, 0x55);
        assert_eq!(read_slot(1), 0x55);
        // Reclaim sibling slot 2 and verify slot 1 still holds 0x55.
        dax.map((2 * unit) as u64, &src, (2 * unit) as u64, unit as u64);
        assert_eq!([read_slot(0), read_slot(1), read_slot(2)], [0xdd, 0x55, 0x11]);

        // Also test writing directly while stale (without reading first) after reclaiming slot 2.
        write_slot(2, 0x77);
        dax.map((3 * unit) as u64, &src, (3 * unit) as u64, unit as u64);
        assert_eq!(
            [read_slot(0), read_slot(1), read_slot(2), read_slot(3)],
            [0xdd, 0x55, 0x77, 0x11]
        );

        drop((dax, window, stage));
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, ps) }, 0);
    }

    /// Review 100 finding 344: creating many non-adjacent stale partially-private host pages under
    /// a small VMA budget (`Mappings::new(BUDGET)`) must keep the process's actual `/proc/self/maps`
    fn sub_host_page_counted_stage(
        pages: usize,
        budget: usize,
    ) -> (
        usize,
        usize,
        usize,
        Arc<CountedWindows>,
        Arc<UffdSecondStage<Counted>>,
        Dax,
        crate::dax::DaxWindow,
    ) {
        let ps = page_size() as usize;
        let unit = ps / 4;
        let size = (pages * ps) as u64;
        let memory = window_memory(pages);
        let windows = Arc::new(CountedWindows {
            memory: std::sync::Mutex::new(
                std::iter::once(memory..memory + size as usize).collect(),
            ),
            most: AtomicUsize::new(0),
        });
        let syscalls = Syscalls::open().unwrap();
        let counted = Counted { syscalls, windows: windows.clone() };
        // SAFETY: Only the test uses `memory`, which outlives the stage.
        let stage = unsafe {
            UffdSecondStage::with_host_and_unit(
                memory,
                size,
                unit,
                counted,
                Memories::InProcess,
                None,
                Mappings::new(budget),
            )
        }
        .unwrap();
        stage.copy_on_write(pages as u64).unwrap();
        let (dax, window) = Dax::with_stage_and_unit_on(
            crate::Host::process(),
            crate::pages::ArenaId::DEFAULT,
            size,
            unit as u64,
            Some(stage.clone()),
            true,
        )
        .unwrap();
        (ps, unit, memory, windows, stage, dax, window)
    }

    /// VMA count within `2 * BUDGET + 1` by coalescing disjoint anonymous VMAs, without losing
    /// private sub-page data.
    #[test]
    fn sub_host_page_stale_vmas_stay_within_mapping_budget() {
        const PAGES: usize = 32;
        const BUDGET: usize = 4;
        let (ps, unit, memory, windows, stage, mut dax, window) =
            sub_host_page_counted_stage(PAGES, BUDGET);
        let size = (PAGES * ps) as u64;
        // Map every host page with slot 0 = A (present) and slot 1 = B (uncached/anonymous so
        // remapping slot 1 leaves the page stale until faulted).
        let src_a = TestSource::filled(1, 0x11).source();
        let src_b = TestSource::filled(1, 0x22).source();
        let src_cold = TestSource::anonymous(1).source();
        for p in 0..PAGES {
            let base = (p * ps) as u64;
            dax.map(base, &src_a, 0, unit as u64);
            dax.map(base + unit as u64, &src_b, 0, unit as u64);
        }
        // Make slot 0 private on every even host page (16 non-adjacent host pages).
        for p in (0..PAGES).step_by(2) {
            let base = (p * ps) as u64;
            futures::executor::block_on(stage.make_private(&*dax.window, base, unit as u64))
                .unwrap();
        }
        // Replace slot 1 on every even host page with a cold (not-in-memory) source so each
        // non-adjacent host page becomes stale (would create 33 VMAs without coalescing).
        for p in (0..PAGES).step_by(2) {
            let base = (p * ps) as u64;
            dax.map(base + unit as u64, &src_cold, 0, unit as u64);
        }
        let most = 2 * BUDGET + 1;
        let seen = windows.most.load(Ordering::Relaxed);
        assert!(
            seen <= most,
            "stale VMAs exceeded budget: saw {seen} VMAs > {most} (BUDGET={BUDGET})"
        );
        drop((dax, window, stage));
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, size as usize) }, 0);
    }

    /// Review 102 finding 348: a cold `MAP` of slot 0 followed by `UNMAP` of slot 0 (before any
    /// fault installs the host page) must clear slot 0 from `mapped_mask`; a subsequent warm `MAP`
    /// of sibling slot 1 must not let `make_private` succeed on unmapped slot 0 or leak
    /// `private_pages()`.
    #[test]
    fn sub_host_page_cold_unmap_clears_slot_before_make_private() {
        let ps = page_size() as usize;
        let unit = ps / 4;
        let memory = window_memory(1);
        // SAFETY: Only the test uses `memory`, which outlives the stage.
        let stage = unsafe { UffdSecondStage::in_process_with_unit(memory, ps as u64, unit, None) }
            .unwrap();
        stage.copy_on_write(1).unwrap();
        let (mut dax, window) = Dax::with_stage_and_unit_on(
            crate::Host::process(),
            crate::pages::ArenaId::DEFAULT,
            ps as u64,
            unit as u64,
            Some(stage.clone()),
            true,
        )
        .unwrap();
        let cold = TestSource::anonymous(1).source();
        let warm = TestSource::filled(1, 0x42).source();
        // Cold MAP slot 0, then UNMAP slot 0 before touching it.
        dax.map(0, &cold, 0, unit as u64);
        dax.unmap(0, unit as u64);
        // Warm MAP sibling slot 1 in the same host page.
        dax.map(unit as u64, &warm, 0, unit as u64);
        // PRIVATE on unmapped slot 0 must fail with EINVAL and charge zero private pages.
        assert_eq!(
            futures::executor::block_on(stage.make_private(&*dax.window, 0, unit as u64)),
            Err(Errno::EINVAL)
        );
        assert_eq!(stage.private_pages(), 0);
        drop((dax, window, stage));
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, ps) }, 0);
    }

    /// Review 102 finding 349: when `coalesce_anon_vmas` sweeps across a fresh partially-private
    /// middle host page whose daemon PTE is still resident, reading that middle page must not hang
    /// waiting on `fault_page` and must return its original private and shared sub-page bytes.
    #[test]
    fn sub_host_page_coalesced_fresh_partial_private_reads_without_hang() {
        const PAGES: usize = 32;
        const BUDGET: usize = 4;
        const MID: usize = 16;
        let ps = page_size() as usize;
        let unit = ps / 4;
        let size = (PAGES * ps) as u64;
        let memory = window_memory(PAGES);
        let syscalls = Syscalls::open().unwrap();
        // SAFETY: Only the test uses `memory`, which outlives the stage.
        let stage = unsafe {
            UffdSecondStage::with_host_and_unit(
                memory,
                size,
                unit,
                syscalls,
                Memories::InProcess,
                None,
                Mappings::new(BUDGET),
            )
        }
        .unwrap();
        stage.copy_on_write(PAGES as u64).unwrap();
        let (mut dax, window) = Dax::with_stage_and_unit_on(
            crate::Host::process(),
            crate::pages::ArenaId::DEFAULT,
            size,
            unit as u64,
            Some(stage.clone()),
            true,
        )
        .unwrap();
        let src_a = TestSource::filled(1, 0x11).source();
        let src_b = TestSource::filled(1, 0x22).source();
        let src_cold = TestSource::anonymous(1).source();
        for p in 0..PAGES {
            let base = (p * ps) as u64;
            dax.map(base, &src_a, 0, unit as u64);
            dax.map(base + unit as u64, &src_b, 0, unit as u64);
        }
        // Make slot 0 private on the middle page (leaving its daemon PTE resident!) and write a
        // distinctive private byte into slot 0.
        futures::executor::block_on(stage.make_private(
            &*dax.window,
            (MID * ps) as u64,
            unit as u64,
        ))
        .unwrap();
        // SAFETY: Slot 0 of `MID` is mapped writable in `PrivateMemory`.
        unsafe { *((memory + MID * ps) as *mut u8) = 0xaa };

        // Make surrounding even pages (excluding `MID`) partially private and stale to force
        // `coalesce_anon_vmas` to sweep across `MID`.
        for p in (0..PAGES).step_by(2).filter(|&p| p != MID) {
            let base = (p * ps) as u64;
            futures::executor::block_on(stage.make_private(&*dax.window, base, unit as u64))
                .unwrap();
            dax.map(base + unit as u64, &src_cold, 0, unit as u64);
        }
        assert!(
            stage.pages.lock().unwrap().pages[MID].anon_vma,
            "middle page should have been swept into the coalesced anonymous span"
        );
        // Resolve a read fault on `MID` under a bounded timeout (previously hung forever).
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stage.resolve(&*dax.window, memory + MID * ps, 0, 0),
            )
            .await
            .expect("read of coalesced fresh partially-private middle page hung");
        });
        // SAFETY: `resolve` remapped `PrivateMemory` back at `MID`.
        let (got_priv, got_shared) = unsafe {
            (*((memory + MID * ps) as *const u8), *((memory + MID * ps + unit) as *const u8))
        };
        assert_eq!(got_priv, 0xaa, "private slot 0 lost after coalescing");
        assert_eq!(got_shared, 0x22, "shared slot 1 lost after coalescing");
        drop((dax, window, stage));
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, size as usize) }, 0);
    }

    /// Review 102 finding 350: faulting alternate private pages inside a coalesced anonymous span
    /// (with no mapping operations in between) must reserve VMA budget and re-coalesce so actual
    /// `/proc/self/maps` VMAs never exceed `2 * BUDGET + 1`, while returning exact private bytes.
    #[test]
    fn sub_host_page_post_coalesce_faults_stay_within_mapping_budget() {
        const PAGES: usize = 32;
        const BUDGET: usize = 4;
        let (ps, unit, memory, windows, stage, mut dax, window) =
            sub_host_page_counted_stage(PAGES, BUDGET);
        let size = (PAGES * ps) as u64;
        let src = TestSource::filled(1, 0x33).source();
        let src_cold = TestSource::anonymous(1).source();
        for p in 0..PAGES {
            dax.map((p * ps) as u64, &src, 0, ps as u64);
        }
        // Make middle pages 6..26 fully private with distinct bytes.
        for p in 6..26 {
            futures::executor::block_on(stage.make_private(
                &*dax.window,
                (p * ps) as u64,
                ps as u64,
            ))
            .unwrap();
            // SAFETY: Page `p` is mapped writable in `PrivateMemory`.
            unsafe { *((memory + p * ps) as *mut u8) = (0x80 + p) as u8 };
        }
        // Put stale partially-private pages near both ends to force coalescing across 0..32.
        for p in [0, 2, 4, 27, 29, 31] {
            let base = (p * ps) as u64;
            futures::executor::block_on(stage.make_private(&*dax.window, base, unit as u64))
                .unwrap();
            dax.map(base + unit as u64, &src_cold, 0, unit as u64);
        }
        assert!(
            stage.pages.lock().unwrap().pages[10].anon_vma,
            "middle fully-private pages should be coalesced"
        );
        // Fault every alternate fully-private page in the middle with no mapping ops in between.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for p in (6..26).step_by(2) {
            runtime.block_on(stage.resolve(&*dax.window, memory + p * ps, 0, 0));
            // SAFETY: `resolve` just mapped `PrivateMemory` back at `p`.
            let got = unsafe { *((memory + p * ps) as *const u8) };
            assert_eq!(got, (0x80 + p) as u8, "middle private page {p} bytes corrupted");
        }
        let most = 2 * BUDGET + 1;
        let seen = windows.most.load(Ordering::Relaxed);
        assert!(
            seen <= most,
            "post-coalesce faults exceeded VMA budget: saw {seen} VMAs > {most} (BUDGET={BUDGET})"
        );
        drop((dax, window, stage));
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory as *mut libc::c_void, size as usize) }, 0);
    }
}
