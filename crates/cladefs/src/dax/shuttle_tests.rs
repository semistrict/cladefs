// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! DAX windows under Shuttle (awslabs/shuttle), on Shuttle's locks: faults, the source that
//! brings their pages in, and the cell that maps and frees them, on threads of their own, in
//! every interleaving Shuttle's scheduler chooses (TASK-44.3).
//!
//! The race of a guest's writes and loads with a free is run against a [`Guest`]: a guest's view
//! of the window through a second stage, on Linux the userfaultfd second stage over a model of its
//! host memory (`uffd::shuttle_tests`), which copies on write itself (doc-26).

use super::tests::TestSource;
use super::*;
use crate::pages::PhysLocation;
use crate::shuttle_testing::{BATCHES, config, runs};
use futures::future::LocalBoxFuture;
use shuttle::scheduler::{RandomScheduler, Scheduler};
use shuttle::{Runner, future, thread};
use std::os::fd::RawFd;

/// The schedules each test runs, before `SHUTTLE_SOAK`.
pub(super) const RUNS: usize = 100;

/// Faults on two pages, each on a thread of its own, wait for the source, which supplies each page
/// asked for, its bytes naming it; each completes with its page mapped.
fn faults_wait_for_the_pager() {
    let ps = page_size();
    let pager = TestSource::paged(2);
    let (dax, window) = Dax::new(2 * ps, None).unwrap();
    dax.window.map(0, &pager.source(), 0, 2 * ps);
    let faults: Vec<_> = (0..2)
        .map(|page| {
            let window = window.clone();
            thread::spawn(move || future::block_on(window.fault(page * ps, Access::Read)))
        })
        .collect();
    for _ in 0..2 {
        let page = pager.request();
        pager.supply(page, page as u8 + 1);
    }
    for fault in faults {
        assert_eq!(fault.join().unwrap(), Ok(()));
    }
    // SAFETY: Both pages are mapped, and nothing else accesses them.
    let bytes = unsafe { [*window.host_base(), *window.host_base().add(ps as usize)] };
    assert_eq!(bytes, [1, 2]);
}

#[test]
fn faults_wait_for_the_pager_randomly() {
    Runner::new(RandomScheduler::new(runs(RUNS)), config()).run(faults_wait_for_the_pager);
}

/// The test source's waits are Shuttle's: a wait for a request no fault makes is a deadlock
/// Shuttle reports, rather than a test that never ends.
#[test]
#[should_panic(expected = "deadlock")]
fn source_waits_are_shuttles() {
    Runner::new(RandomScheduler::new(1), config()).run(|| {
        let pager = TestSource::paged(1);
        pager.request();
    });
}

/// How a vCPU's access to a page of the window goes.
pub(super) enum Outcome<'a> {
    /// It was made: the page's first byte after it.
    Made(u8),
    /// It faulted: the future resolves the fault, after which the vCPU retries the access.
    Faulted(LocalBoxFuture<'a, Result<()>>),
    /// It failed, as KVM's run of a vCPU fails with `EFAULT`, and the vCPU retries it, as cladefs's
    /// Firecracker does: the userfaultfd second stage's, on Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Failed,
}

/// A guest's view of a window through its second stage: how its vCPUs' accesses go, each made at
/// once, against the stage as it is, as the hardware makes it.
pub(super) trait Guest: Send + Sync + 'static {
    /// Where a page's memory is, which tells a page's copies apart.
    type Where: Copy + PartialEq + std::fmt::Debug + Send;

    /// A vCPU's access of `kind` to the page at `offset`, storing `write` if it is a write.
    fn access(&self, offset: u64, kind: Access, write: Option<u8>) -> Outcome<'_>;

    /// The pages a write would change without faulting: cladefs: private pages alone (backlog
    /// doc-13).
    fn writable(&self) -> Vec<u64>;

    /// Where the memory of each page mapped is.
    fn mapped(&self) -> Vec<(u64, Self::Where)>;

    /// Where the page of physical memory at `phys` is, as a page mapped of it is.
    fn place(&self, phys: PhysLocation) -> Self::Where;

    /// cladefs: how many pages its stage made private (backlog doc-13, doc-26).
    fn private_pages(&self) -> u64;
}

/// The first byte of the page at `offset` of `fd`'s memory, read, or written with `write` first,
/// as the guest would through its mapping.
pub(super) fn byte(fd: RawFd, offset: u64, write: Option<u8>) -> u8 {
    let len = page_size() as usize;
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    // SAFETY: A new mapping of a page of the memory, which outlives it.
    let page =
        unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, offset as _) };
    assert_ne!(page, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
    let page = page.cast::<u8>();
    // SAFETY: `page` is the mapping's first byte, which only the guest's accesses change, each at
    // once.
    let value = unsafe {
        if let Some(value) = write {
            page.write_volatile(value);
        }
        page.read_volatile()
    };
    // SAFETY: The mapping made above.
    assert_eq!(unsafe { libc::munmap(page.cast(), len) }, 0);
    value
}

/// What a sealed extent's pages hold, and what a vCPU writes.
pub(super) const SEALED: u8 = 0xb;
pub(super) const STORED: u8 = 0xc;

/// What a page of the window holds, in the order it changes: a sealed extent's, then, freed,
/// zeros; or, stored to, a private page holding the write, which a free leaves.
fn rank(value: u8) -> u8 {
    match value {
        SEALED => 0,
        0 => 1,
        STORED => 2,
        other => panic!("the guest read {other:#x}, which nothing mapped held"),
    }
}

/// What a race test's schedules are to reach, each in some run.
#[derive(Clone, Copy, Debug)]
pub(super) enum Event {
    ReadSealed,
    ReadZeros,
    ReadStored,
    /// A fault resolved.
    Faulted,
    /// An access failed, and was retried.
    Failed,
    /// A write refused, past the window's private pages.
    WriteRefused,
    /// cladefs: a write's private copy that a `MAP` after it ended.
    MapEndedCopy,
    /// cladefs: a write's private copy of what a `MAP` before it mapped.
    CopiedAfterMap,
}

/// How often each event happened, over a test's runs (each test is a process of its own).
static REACHED: [std::sync::atomic::AtomicUsize; 8] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 8];

pub(super) fn reached(event: Event) {
    REACHED[event as usize].fetch_add(1, Ordering::Relaxed);
}

/// Runs `test` on the schedules of `scheduler`, then checks they reached each of `events`:
/// schedules that miss the races a test is for fail it. They are random: a batch may miss one,
/// which isn't that it can't be reached (TASK-55), so batches of new schedules are run, from
/// `scheduler`, until they have reached each, up to [`BATCHES`].
pub(super) fn reaching<S: Scheduler + 'static>(
    scheduler: impl Fn() -> S,
    test: fn(),
    events: &[Event],
) {
    let counts = || -> Vec<_> {
        (events.iter())
            .map(|&event| (event, REACHED[event as usize].load(Ordering::Relaxed)))
            .collect()
    };
    for _ in 0..BATCHES {
        Runner::new(scheduler(), config()).run(test);
        if counts().iter().all(|(_, count)| *count > 0) {
            return;
        }
    }
    panic!("unreached in {BATCHES} batches: {:?}", counts());
}

/// The attempts an access makes before it is taken never to complete, as a vCPU faulting on a
/// page for ever: a few races' worth.
pub(super) const ATTEMPTS: usize = 100;

/// A vCPU's access of `kind` to the page at `offset` through `guest`, storing [`STORED`] if it is
/// a write, retried as each fault is resolved, and as it fails: the page's first byte after it; or
/// none, for a write the stage refused, past its private pages, which one of the guest's that
/// faults says (`EACCES`), and one of a guest's own memory by failing.
fn access<G: Guest>(guest: &G, offset: u64, kind: Access) -> Option<u8> {
    let write = (kind == Access::Write).then_some(STORED);
    // Once a page is private, the stage holds as many as it may: a write to another is refused.
    let refused = || kind == Access::Write && guest.private_pages() > 0;
    for _ in 0..ATTEMPTS {
        match guest.access(offset, kind, write) {
            Outcome::Made(read) => return Some(read),
            // A fault resolved may leave the page as it was here: the window maps it already, and
            // the stage, on another thread, is still making the change (the extent daemon's
            // changes made at once, `uffd::shuttle_tests::Remapping`). The vCPU faults again until
            // it is made, as the host's scheduler runs that thread meanwhile: so it yields, else
            // Shuttle's PCT schedules retry it at once, for ever, the change's thread, of lower
            // priority, never running (CI run 41876989384).
            Outcome::Faulted(fault) => match future::block_on(fault) {
                Ok(()) => {
                    reached(Event::Faulted);
                    thread::yield_now();
                }
                Err(Errno::EACCES) if refused() => return None,
                Err(err) => panic!("a {kind:?} fault at {offset:#x}: {err:?}"),
            },
            Outcome::Failed if refused() => return None,
            Outcome::Failed => {
                reached(Event::Failed);
                thread::yield_now();
            }
        }
    }
    panic!("a {kind:?} at {offset:#x} never completed");
}

/// cladefs: two vCPUs write to and load two pages of a sealed extent's mapping, in a window that
/// copies on write and holds one private page at most (`dax`, of two pages), while the cell frees
/// the extent, mapping zeros in its place (backlog doc-13). Each writes to a page of its own as
/// each round begins, and loads the other's: one write is made, in a private page, which the free
/// leaves, the guest's; the other is refused, past the window's private pages, and its vCPU loads
/// its page instead, each round, as zeros are mapped over the page it refused. Each access `guest`
/// refuses faults, and is retried once the fault is resolved, or fails, and is retried. Each vCPU
/// goes on until it reads both pages freed or stored to, as a guest polling them would. Checked: a
/// vCPU never reads a page older than one it read; once the free returns, no page of the extent
/// is mapped; one write is made, its private page the one page writable; every access completes.
pub(super) fn writes_race_frees<G: Guest>(mut dax: Dax, guest: Arc<G>) {
    let ps = page_size();
    let sealed = TestSource::filled(2, SEALED);
    dax.map(0, &sealed.source(), 0, 2 * ps);
    let mut extent = Vec::new();
    sealed.each_present(0..2, &mut |present| extent.push(guest.place(present.at)));

    let vcpus: Vec<_> = [[Access::Write, Access::Read], [Access::Read, Access::Write]]
        .into_iter()
        .map(|kinds| {
            let guest = guest.clone();
            thread::spawn(move || {
                let mut seen = [rank(SEALED); 2];
                while seen.iter().any(|seen| *seen < rank(0)) {
                    for (page, kind) in kinds.into_iter().enumerate() {
                        let offset = page as u64 * ps;
                        let read = access(&*guest, offset, kind).unwrap_or_else(|| {
                            reached(Event::WriteRefused);
                            let read = access(&*guest, offset, Access::Read);
                            read.expect("a load refused")
                        });
                        assert!(rank(read) >= seen[page], "read {read:#x} after a newer page");
                        seen[page] = rank(read);
                        let events = [Event::ReadSealed, Event::ReadZeros, Event::ReadStored];
                        reached(events[seen[page] as usize]);
                    }
                    thread::yield_now();
                }
            })
        })
        .collect();

    let freeing = guest.clone();
    let freer = thread::spawn(move || {
        assert_eq!(dax.map_zeros_over(&sealed), [0..2 * ps]);
        let still: Vec<_> =
            freeing.mapped().into_iter().filter(|(_, at)| extent.contains(at)).collect();
        assert!(still.is_empty(), "the freed extent is mapped after its free: {still:#x?}");
        dax
    });
    for vcpu in vcpus {
        vcpu.join().unwrap();
    }
    let _dax = freer.join().unwrap();
    assert_eq!(guest.private_pages(), 1, "writes made");
    let writable = guest.writable();
    assert_eq!(writable.len(), 1, "writable but the private page: {writable:#x?}");
}
