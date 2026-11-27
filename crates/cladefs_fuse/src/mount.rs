// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A FUSE mount on Linux: the server, serving the kernel's requests through `/dev/fuse`.
//!
//! The mount is made as libfuse makes it: mount(2) of a `fuse` filesystem whose options name an
//! open `/dev/fuse`, which a process may do with `CAP_SYS_ADMIN`; refused that (`EPERM`), through
//! `fusermount3`, setuid root, which mounts it and hands the device back over a socket
//! (`_FUSE_COMMFD`, `SCM_RIGHTS`). Either way the kernel checks permissions itself
//! (`default_permissions`), as for any filesystem.
//!
//! Threads of the mount's own each read a request from the device, have the server handle it, and
//! write its reply, until the filesystem is unmounted, which ends the connection: reads then fail
//! with `ENODEV`. A handler that panics is answered `EIO`, its thread going on (board 8539). A
//! panic on a thread itself, outside a handler, ends it alone, as does any other error reading the
//! device, and a panic in a request's task outside its handler ends that task alone, its tenant's
//! waiting requests never answered; unless the mount is a service of the daemon's
//! ([`MountOptions::supervised`]): there each aborts the daemon, naming the mount, as with every
//! thread gone the mount would hang, and its callers' calls with it (board 12443, board 12891). A
//! request may wait long on its cell, as one waiting on the bucket does, holding its thread: of a
//! mount of many cells, a tenant's requests take at most all threads but two at once
//! ([`Admission`]), the rest waiting for one of its own to end, so that the other tenants' cells,
//! and the mount's own directories, are served whatever one tenant's wait on.
//!
//! A mount point a dead server left mounted, its connection aborted (`ENOTCONN`), as a daemon
//! killed with its mount leaves it, is detached first, if it is a cladefs's (board 8537).

use crate::server::{MAX_WRITE, Notifier, Server};
use anyhow::{Context as _, bail};
use cladefs_core::error::Error;
use cladefs_fs::filesystem::Filesystem;
use cladefs_fs::node::Owner;
use cladefs_fs::storage::Storage;
use event_listener::{Event, Listener as _};
use futures::FutureExt as _;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// The setuid program that mounts FUSE filesystems for users without `CAP_SYS_ADMIN`: libfuse 3's.
const FUSERMOUNT: &str = "fusermount3";

/// `_IOR(229, 0, uint32_t)` from `<linux/fuse.h>`: binds a fresh `/dev/fuse` fd to an existing
/// FUSE connection so each reader thread has its own kernel queue without lock contention
/// (board 12092).
const FUSE_DEV_IOC_CLONE: libc::c_ulong = 0x8004_e500;

/// The room a request is read into: the largest, a write's, its headers, and more. The kernel
/// refuses reads into less than its headers and `max_write`.
const REQUEST_ROOM: usize = MAX_WRITE as usize + 4096;

/// How a filesystem is mounted with FUSE.
#[derive(Clone, Debug)]
pub struct MountOptions {
    allow_other: bool,
    threads: NonZeroUsize,
    writeback_cache: bool,
    queued_bytes: Option<Arc<AtomicU64>>,
    supervised: Option<&'static str>,
}

impl MountOptions {
    /// The default options: the mounting user's alone, served by 8 workers.
    pub fn new() -> Self {
        Self {
            allow_other: false,
            threads: NonZeroUsize::new(8).unwrap(),
            writeback_cache: true,
            queued_bytes: None,
            supervised: None,
        }
    }

    /// Recommended worker count for host mounts (`cladefsd`'s): scales with host
    /// CPUs, clamped to `8..=32` (board 12092).
    pub fn default_threads() -> NonZeroUsize {
        let cpus = std::thread::available_parallelism().map_or(8, NonZeroUsize::get);
        NonZeroUsize::new(cpus.clamp(8, 32)).unwrap()
    }

    /// Whether users other than the mounting one may use the filesystem, as its permissions say
    /// (FUSE's `allow_other`). A user other than root needs `user_allow_other` in
    /// `/etc/fuse.conf` for it.
    pub fn allow_other(&mut self, allow_other: bool) -> &mut Self {
        self.allow_other = allow_other;
        self
    }

    /// How many reader threads and async FUSE runtime workers serve the kernel's requests.
    pub fn threads(&mut self, threads: NonZeroUsize) -> &mut Self {
        self.threads = threads;
        self
    }

    /// Whether `INIT` advertises `FUSE_WRITEBACK_CACHE` (default `true`). Disabling it forces
    /// synchronous writethrough `FUSE_WRITE` requests.
    pub fn writeback_cache(&mut self, enabled: bool) -> &mut Self {
        self.writeback_cache = enabled;
        self
    }

    /// Supplies a shared counter updated with bytes currently queued in per-tenant admission so
    /// `cladefsd`'s memory governor counts them (board 9850).
    pub fn queued_bytes_counter(&mut self, counter: Arc<AtomicU64>) -> &mut Self {
        self.queued_bytes = Some(counter);
        self
    }

    /// Has each reader thread, and each request's task, run as the daemon's service `service`:
    /// a panic on one outside a handler (those run on the runtime, answered `EIO`, and those it
    /// runs inline, said: a cell's, which halts it), or an error reading the device but the
    /// mount's end, aborts the daemon, naming `service`, rather than ending the thread or task
    /// alone, which with every thread gone would hang the mount and its callers, and with a
    /// tenant's task gone its waiting requests (board 12443, hotbed question 561, board 12891).
    pub fn supervised(&mut self, service: &'static str) -> &mut Self {
        self.supervised = Some(service);
        self
    }

    /// Mounts `fs` at `mountpoint`, a directory, and serves it until it is unmounted. The kernel
    /// sends no request until the mount is made, and holds those made meanwhile until the mount
    /// answers: the filesystem may be used from the moment this returns.
    ///
    /// The filesystem is its cell's only writer, as the cell directory is its process's
    /// alone, which the kernel's caches count on: they are kept as long as it likes.
    pub fn mount(&self, fs: Filesystem, mountpoint: impl AsRef<Path>) -> Result<Mount, Error> {
        let served = self.serve(mountpoint.as_ref(), |device| {
            Server::new(fs, notifier(device)).with_writeback_cache(self.writeback_cache)
        })?;
        Ok(Mount { served })
    }

    /// Mounts a directory of cell directories at `mountpoint`, none attached yet (TASK-302): cells
    /// are attached in it and detached by [`CellsMount::server`]'s [`Server::attach`] and
    /// [`Server::detach`], one mount however many. It is the mounting user's, as its cells' roots
    /// say who else may use them. Served until it is unmounted, as [`MountOptions::mount`]'s.
    pub fn mount_cells(&self, mountpoint: impl AsRef<Path>) -> Result<CellsMount, Error> {
        // SAFETY: Plain calls returning the process's IDs.
        let owner = unsafe { Owner { uid: libc::getuid(), gid: libc::getgid() } };
        let served = self.serve(mountpoint.as_ref(), |device| {
            Server::cells(owner, notifier(device)).with_writeback_cache(self.writeback_cache)
        })?;
        Ok(CellsMount { served })
    }

    /// Mounts a FUSE filesystem at `mountpoint`, and serves it with the server `server` makes,
    /// given the device it is served through: one cloned `/dev/fuse` reader per thread dispatching
    /// requests asynchronously onto a dedicated Tokio runtime (board 9850, board 12092).
    fn serve(
        &self,
        mountpoint: &Path,
        server: impl FnOnce(Arc<File>) -> Server,
    ) -> Result<Served, Error> {
        // Build the Tokio runtime BEFORE mounting `/dev/fuse` so runtime creation failures (e.g.
        // EMFILE) never leak a disconnected mountpoint (hotbed review 131).
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(self.threads.get())
            .thread_name("fuse worker")
            .enable_all()
            .build()
            .map_err(|error| {
                Error::new(anyhow::Error::new(error).context("starting the FUSE runtime"))
            })?;
        let mountpoint = mountpoint.to_owned();
        let (device, how) = mount(&mountpoint, self.allow_other).map_err(Error::new)?;
        let unmounter = Unmounter { mountpoint, how };
        let device = Arc::new(device);
        let server = Arc::new(server(device.clone()));
        let in_flight = Arc::new(InFlight::default());
        let queued_bytes = self.queued_bytes.clone().unwrap_or_default();
        let admission = Arc::new(Admission::of(self.threads, Arc::clone(&queued_bytes)));
        let plain_budget = Arc::new(PlainBudget::new(64 * MAX_WRITE as usize));
        let mut served = Served {
            unmounter,
            _device: device.clone(),
            server: Some(server.clone()),
            workers: Vec::with_capacity(self.threads.get()),
            in_flight: in_flight.clone(),
            queued_bytes,
            runtime: Some(runtime),
        };
        let handle = served.runtime.as_ref().unwrap().handle().clone();
        for index in 0..self.threads.get() {
            let cloned = match clone_device(&device) {
                Ok(fd) => Arc::new(fd),
                Err(error) => {
                    drop(served);
                    return Err(Error::new(anyhow::Error::new(error).context("cloning /dev/fuse")));
                }
            };
            let (server, admission, plain_budget, handle, in_flight) = (
                server.clone(),
                admission.clone(),
                plain_budget.clone(),
                handle.clone(),
                in_flight.clone(),
            );
            let supervised = self.supervised;
            let spawned =
                std::thread::Builder::new().name(format!("fuse {index}")).spawn(move || {
                    serve(cloned, server, admission, plain_budget, handle, in_flight, supervised);
                });
            match spawned {
                Ok(worker) => served.workers.push(worker),
                Err(error) => {
                    // The workers started end once it is unmounted.
                    drop(served);
                    return Err(Error::new(anyhow::Error::new(error).context("starting a thread")));
                }
            }
        }
        Ok(served)
    }
}

impl Default for MountOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A filesystem mounted with FUSE, served until it is unmounted: by [`unmount`](Self::unmount),
/// an [`Unmounter`], or anyone (`umount`, `fusermount3 -u`). [`join`](Self::join) then closes it.
///
/// Dropped before it is joined, it unmounts it lazily, waits until the kernel is done with it,
/// and closes it.
#[derive(Debug)]
pub struct Mount {
    served: Served,
}

impl Mount {
    /// Where the filesystem is mounted.
    pub fn mountpoint(&self) -> &Path {
        &self.served.unmounter.mountpoint
    }

    /// What unmounts the filesystem, from another thread, such as a signal's.
    pub fn unmounter(&self) -> Unmounter {
        self.served.unmounter.clone()
    }

    /// Unmounts the filesystem (see [`Unmounter::unmount`]).
    pub fn unmount(&self, lazily: bool) -> Result<(), Error> {
        self.served.unmounter.unmount(lazily)
    }

    /// Waits until the filesystem is unmounted and the kernel is done with it, then closes it,
    /// writing back what is dirty, and gives its storage back, as [`Filesystem::close`] does.
    pub fn join(self) -> Result<Storage, Error> {
        let server = self.served.join();
        futures::executor::block_on(server.into_filesystem().close())
    }
}

/// A directory of cells mounted with FUSE ([`MountOptions::mount_cells`]), served until it is
/// unmounted, as a [`Mount`] is; [`join`](Self::join) then unmounts the cells attached.
///
/// Dropped before it is joined, it unmounts it lazily, waits until the kernel is done with it, and
/// unmounts the cells.
#[derive(Debug)]
pub struct CellsMount {
    served: Served,
}

impl CellsMount {
    /// Where it is mounted.
    pub fn mountpoint(&self) -> &Path {
        &self.served.unmounter.mountpoint
    }

    /// The server, which attaches cells, detaches them and lists them.
    pub fn server(&self) -> &Arc<Server> {
        self.served.server.as_ref().expect("served until joined")
    }

    /// What unmounts it, from another thread, such as a signal's.
    pub fn unmounter(&self) -> Unmounter {
        self.served.unmounter.clone()
    }

    /// Unmounts it (see [`Unmounter::unmount`]).
    pub fn unmount(&self, lazily: bool) -> Result<(), Error> {
        self.served.unmounter.unmount(lazily)
    }

    /// Bytes currently retained in memory by in-flight and queued FUSE requests for this mount
    /// (reported to `cladefsd`'s memory governor; board 9850).
    pub fn queued_bytes(&self) -> u64 {
        self.served.queued_bytes.load(Ordering::Relaxed)
    }

    /// Waits until it is unmounted and the kernel is done with it, then unmounts the cells still
    /// attached, writing back what is dirty, and closes their cells: once those sharing the
    /// server, which attach no more, have let it go. Fails with the first unmount's error.
    pub fn join(self) -> Result<(), Error> {
        let server = self.served.join_shared();
        futures::executor::block_on(server.unmount())
    }
}

/// What sends the kernel a server's notifications through `device`: written whole, as replies
/// are, by whatever thread sends them.
fn notifier(device: Arc<File>) -> Notifier {
    Box::new(move |message: &[u8]| match (&*device).write(message)? {
        written if written == message.len() => Ok(()),
        written => Err(io::Error::other(format!(
            "{written} bytes of a {}-byte notification",
            message.len()
        ))),
    })
}

/// Tracks async FUSE request tasks in flight so unmount waits for every task to finish before
/// dropping the server or runtime.
#[derive(Debug, Default)]
struct InFlight {
    count: AtomicUsize,
    idle: Event,
}

impl InFlight {
    fn enter(self: &Arc<Self>) -> InFlightGuard {
        // Relaxed, as `Arc::clone`'s increment is: the reader thread entering is joined before
        // `wait_idle` loads (`Served::join_workers`), so its increment is seen there; the guard's
        // AcqRel decrement is what orders a task's work before the wait ends.
        self.count.fetch_add(1, Ordering::Relaxed);
        InFlightGuard(Arc::clone(self))
    }

    fn wait_idle(&self) {
        loop {
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            let listener = self.idle.listen();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            listener.wait();
        }
    }
}

struct InFlightGuard(Arc<InFlight>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify(usize::MAX);
        }
    }
}

/// A FUSE filesystem served by reader threads and an async worker runtime until it is unmounted.
#[derive(Debug)]
struct Served {
    unmounter: Unmounter,
    /// Master `/dev/fuse` fd kept open for the lifetime of the mount (notifications and clones).
    _device: Arc<File>,
    /// The server, which the workers share until they end: taken by a join.
    server: Option<Arc<Server>>,
    workers: Vec<JoinHandle<()>>,
    in_flight: Arc<InFlight>,
    queued_bytes: Arc<AtomicU64>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Served {
    /// Waits until the filesystem is unmounted and the kernel is done with it, and returns the
    /// server, which the workers shared alone.
    fn join(self) -> Server {
        Arc::into_inner(self.join_shared()).expect("the workers shared the server alone")
    }

    /// Waits until the filesystem is unmounted and the kernel is done with it, and returns the
    /// server, which others may share: or goes on with a worker's panic, once all ended.
    fn join_shared(mut self) -> Arc<Server> {
        if let Some(panic) = self.join_workers() {
            std::panic::resume_unwind(panic);
        }
        self.server.take().expect("joined once")
    }

    /// Waits for all reader threads and in-flight async tasks to finish, then shuts down the FUSE
    /// runtime cleanly without panicking if called from inside another Tokio runtime's blocking
    /// pool.
    fn join_workers(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        let mut panicked = None;
        for worker in self.workers.drain(..) {
            if let Err(panic) = worker.join() {
                panicked.get_or_insert(panic);
            }
        }
        self.in_flight.wait_idle();
        if let Some(rt) = self.runtime.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                // Dropping a Tokio runtime inside a `spawn_blocking` thread panics; offload drop.
                let _ = std::thread::spawn(move || drop(rt)).join();
            } else {
                drop(rt);
            }
        }
        panicked
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let Some(server) = self.server.take() else { return };
        if let Err(error) = self.unmounter.unmount(true) {
            log::error!("fuse: unmounting {}: {error:#}", self.unmounter.mountpoint.display());
        }
        // Said, never unwound: a drop as a panic unwinds that panicked again would abort the
        // process (board 8539).
        if let Some(panic) = self.join_workers() {
            log::error!(
                "fuse: a worker of {} panicked: {}",
                self.unmounter.mountpoint.display(),
                said(&*panic)
            );
        }
        // The kernel's DESTROY unmounted what was served, unless it never came.
        if let Err(error) = futures::executor::block_on(server.unmount()) {
            log::error!("fuse: closing {}: {error:#}", self.unmounter.mountpoint.display());
        }
    }
}

/// What unmounts a [`Mount`]'s filesystem.
#[derive(Clone, Debug)]
pub struct Unmounter {
    mountpoint: PathBuf,
    how: How,
}

/// How the filesystem was mounted, so how it is unmounted.
#[derive(Clone, Copy, Debug)]
enum How {
    /// By mount(2): umount2(2) unmounts it.
    Syscall,
    /// By `fusermount3`: `fusermount3 -u` unmounts it.
    Fusermount,
}

impl Unmounter {
    /// Unmounts the filesystem. Unless `lazily`, it fails with `EBUSY` while it is in use, as a
    /// file open in it, and stays mounted; `lazily`, it is detached from its mount point at once,
    /// and unmounted once it is no longer in use (`MNT_DETACH`).
    pub fn unmount(&self, lazily: bool) -> Result<(), Error> {
        match self.how {
            How::Syscall => umount(&self.mountpoint, lazily),
            How::Fusermount => fusermount_unmount(&self.mountpoint, lazily),
        }
        .with_context(|| format!("unmounting {}", self.mountpoint.display()))
        .map_err(Error::new)
    }
}

/// A FUSE request buffer whose byte size is charged to `counter` for as long as it is held in
/// memory (both while waiting in [`Admission`] and while being served asynchronously), so
/// `cladefsd`'s memory governor counts every buffered request and dropping it on completion,
/// panic, or unmount always releases the charge (board 9850).
#[derive(Debug)]
struct TrackedRequest {
    bytes: Vec<u8>,
    counter: Arc<AtomicU64>,
}

impl TrackedRequest {
    fn new(bytes: Vec<u8>, counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Self { bytes, counter }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl std::ops::Deref for TrackedRequest {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for TrackedRequest {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes.len() as u64, Ordering::Relaxed);
    }
}

/// Outcome of per-tenant admission for a newly read FUSE request (board 9850, review 131).
#[derive(Debug)]
enum Decision<D> {
    /// Serve immediately on a new async task.
    Serve(D, TrackedRequest),
    /// Buffered in the tenant's FIFO waiting queue (within `most_waiting`).
    Queued,
    /// The tenant already holds `most` active tasks and `most_waiting` queued bytes: reject
    /// immediately with `EAGAIN` without retaining the request buffer so a stalled tenant can
    /// neither starve other tenants nor exhaust daemon memory.
    Rejected,
}

/// Bounds in-flight request bytes on a single-filesystem mount (`!server.is_many()`) via reader
/// thread backpressure: there are no other tenants to starve on a single-cell mount, so waiting
/// once `limit` (64 MiB) is in flight keeps excess requests in the kernel with zero errors and
/// bounded memory (board 9850, review 131).
#[derive(Debug)]
struct PlainBudget {
    limit: usize,
    used: Mutex<usize>,
    changed: Condvar,
}

impl PlainBudget {
    fn new(limit: usize) -> Self {
        Self { limit, used: Mutex::new(0), changed: Condvar::new() }
    }

    fn acquire(self: &Arc<Self>, len: usize) -> PlainPermit {
        let mut used = self.used.lock().unwrap();
        while *used > 0 && used.saturating_add(len) > self.limit {
            used = self.changed.wait(used).unwrap();
        }
        *used += len;
        PlainPermit { budget: Arc::clone(self), len }
    }
}

struct PlainPermit {
    budget: Arc<PlainBudget>,
    len: usize,
}

impl Drop for PlainPermit {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap();
        *used -= self.len;
        self.budget.changed.notify_all();
    }
}

/// Bounds a tenant's concurrent FUSE tasks and kept request memory (board 8539, board 9850,
/// review 131): requests run asynchronously on Tokio rather than blocking `/dev/fuse` reader
/// threads, so a stalled tenant never holds OS reader threads or Tokio workers. Each tenant runs
/// at most `most` concurrent tasks and buffers at most `most_waiting` (64 MiB) waiting; any
/// further request while stalled is rejected immediately with `EAGAIN` without allocating a
/// buffer. All retained request bytes (active and queued) are charged to `queued_bytes` via
/// [`TrackedRequest`] for `cladefsd`'s memory governor.
#[derive(Debug)]
struct Admission<D = Arc<File>> {
    /// The most concurrent tasks a tenant runs.
    most: usize,
    /// The most bytes a tenant's waiting requests hold: 64 [`MAX_WRITE`]s.
    most_waiting: usize,
    /// Live total of bytes held by buffered requests across all tenants (counted by the governor).
    queued_bytes: Arc<AtomicU64>,
    tenants: Mutex<HashMap<Arc<str>, Admitted<D>>>,
}

/// A tenant's in-flight tasks and queued requests.
#[derive(Debug)]
struct Admitted<D> {
    /// How many async tasks are currently serving this tenant's requests.
    serving: usize,
    /// Its requests read, waiting for one of its active tasks to finish.
    waiting: VecDeque<(D, TrackedRequest)>,
    /// The bytes they hold.
    waiting_bytes: usize,
}

impl<D> Default for Admitted<D> {
    fn default() -> Self {
        Self { serving: 0, waiting: VecDeque::new(), waiting_bytes: 0 }
    }
}

impl<D> Admission<D> {
    /// The bound of a mount configured with `threads` workers.
    fn of(threads: NonZeroUsize, queued_bytes: Arc<AtomicU64>) -> Self {
        let most = threads.get().saturating_sub(2).max(1);
        Self {
            most,
            most_waiting: 64 * MAX_WRITE as usize,
            queued_bytes,
            tenants: Mutex::default(),
        }
    }

    fn track(&self, bytes: Vec<u8>) -> TrackedRequest {
        TrackedRequest::new(bytes, Arc::clone(&self.queued_bytes))
    }

    /// Admits `(device, request)` for `tenant`. Only copies `request` onto the heap if it is
    /// accepted (`Serve` or `Queued`); returns `Decision::Rejected` without allocating when
    /// `tenant` already has `most` active tasks and `most_waiting` queued bytes.
    fn admit(&self, tenant: &Arc<str>, device: D, request: &[u8]) -> Decision<D> {
        let mut tenants = self.tenants.lock().unwrap();
        let admitted = tenants.entry(tenant.clone()).or_default();
        if admitted.serving < self.most {
            admitted.serving += 1;
            let tracked = self.track(request.to_vec());
            return Decision::Serve(device, tracked);
        }
        if admitted.waiting_bytes.saturating_add(request.len()) > self.most_waiting {
            return Decision::Rejected;
        }
        let tracked = self.track(request.to_vec());
        admitted.waiting_bytes += tracked.len();
        admitted.waiting.push_back((device, tracked));
        Decision::Queued
    }

    /// One of `tenant`'s active tasks finished a request: returns the next queued `(device, req)`
    /// for it to serve, or releases its permit if none remain.
    fn next(&self, tenant: &Arc<str>) -> Option<(D, TrackedRequest)> {
        let mut tenants = self.tenants.lock().unwrap();
        let admitted = tenants.get_mut(tenant).expect("a tenant in service");
        if let Some((device, next)) = admitted.waiting.pop_front() {
            admitted.waiting_bytes -= next.len();
            return Some((device, next));
        }
        admitted.serving -= 1;
        if admitted.serving == 0 {
            tenants.remove(tenant);
        }
        None
    }
}

/// Clones `master` (`/dev/fuse`) via `FUSE_DEV_IOC_CLONE` so each reader thread gets its own
/// kernel `fuse_dev` queue without lock contention (board 12092).
fn clone_device(master: &File) -> io::Result<File> {
    let cloned = File::options().read(true).write(true).open("/dev/fuse")?;
    let master_fd = master.as_raw_fd() as u32;
    // SAFETY: `FUSE_DEV_IOC_CLONE` reads a `uint32_t` master `/dev/fuse` fd from `&master_fd` and
    // binds `cloned` to that FUSE connection; both fds stay open across the ioctl.
    if unsafe { libc::ioctl(cloned.as_raw_fd(), FUSE_DEV_IOC_CLONE as _, &master_fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cloned)
}

/// Reads the kernel's requests from its cloned `device` until the filesystem is unmounted,
/// handling fire-and-forget opcodes inline and dispatching all other requests asynchronously onto
/// `runtime` (board 9850, board 12092): as the daemon's service `supervised`, if named, except
/// while it handles a request inline (board 12443), and so the tasks it spawns ([`spawn_task`]).
fn serve(
    device: Arc<File>,
    server: Arc<Server>,
    admission: Arc<Admission<Arc<File>>>,
    plain_budget: Arc<PlainBudget>,
    runtime: tokio::runtime::Handle,
    in_flight: Arc<InFlight>,
    supervised: Option<&'static str>,
) {
    let mut supervising = supervised.map(|name| {
        let holding = local_cell::shared::Holding::work(name);
        fail::fail_point!(&format!("cladefsd::service::{name}"));
        holding
    });
    let mut request = vec![0; REQUEST_ROOM];
    let is_many = server.is_many();
    loop {
        let len = match read_request(&device, &mut request) {
            Ok(len) => len,
            Err(error) => match error.raw_os_error() {
                // Unmounted: the kernel is done with the filesystem.
                Some(libc::ENODEV) => return,
                // Its connection aborted (fusectl's `abort`): the mount ended, its callers
                // answered `ENOTCONN` by the kernel, which every reader reads from now on.
                Some(libc::ECONNABORTED) => {
                    return log::error!("fuse: the mount's connection was aborted: {error}");
                }
                // A request interrupted before it was read, or a signal.
                Some(libc::ENOENT | libc::EINTR | libc::EAGAIN) => continue,
                // A reader's end, as a panic is, supervised: aborting the daemon, naming the
                // mount. Ended alone, said only in a log, it left the mount to hang once every
                // reader had (board 12891, hotbed question 577).
                _ => panic!("fuse: reading a request: {error}"),
            },
        };
        let slice = &request[..len];
        // A handler, as `answer_async`'s are, of a cell's nodes (a FORGET's letting one go): its
        // panic is the cell's, which halts it, said here and the thread going on, unsupervised
        // meanwhile so that it doesn't abort the daemon (board 8539, board 12443). Unwind safe
        // as `answer_async` says.
        drop(supervising.take());
        let inline = std::panic::catch_unwind(AssertUnwindSafe(|| server.try_handle_inline(slice)));
        supervising = supervised.map(local_cell::shared::Holding::work);
        match inline {
            Ok(None) => {}
            Ok(Some(Ok(()))) => continue,
            Ok(Some(Err(error))) => {
                log::error!("fuse: handling inline request: {error:#}");
                continue;
            }
            Err(panic) => {
                log::error!("fuse: handling a request inline panicked: {}", said(&*panic));
                continue;
            }
        }
        match server.tenant_of(slice) {
            None => {
                // Single-cell mounts bound in-flight bytes via reader backpressure; multi-tenant
                // root ops are tiny in-memory metadata requests that never wait on a cell.
                let permit = (!is_many).then(|| plain_budget.acquire(slice.len()));
                let owned = admission.track(slice.to_vec());
                let guard = in_flight.enter();
                let (dev, srv) = (device.clone(), server.clone());
                spawn_task(&runtime, supervised, async move {
                    let (_guard, _permit) = (guard, permit);
                    answer_async(&dev, &srv, &owned).await;
                });
            }
            Some(tenant) => match admission.admit(&tenant, device.clone(), slice) {
                Decision::Serve(dev, req) => {
                    let guard = in_flight.enter();
                    let (srv, adm) = (server.clone(), admission.clone());
                    spawn_task(&runtime, supervised, async move {
                        let _guard = guard;
                        let mut current = Some((dev, req));
                        while let Some((cur_dev, cur_req)) = current {
                            tenant_failpoint_async(&tenant).await;
                            answer_async(&cur_dev, &srv, &cur_req).await;
                            drop(cur_req);
                            current = adm.next(&tenant);
                        }
                    });
                }
                Decision::Queued => {}
                Decision::Rejected => {
                    if let Some(reply) = Server::rejected(slice)
                        && let Err(error) = (&*device).write(&reply)
                        && error.raw_os_error() != Some(libc::ENOENT)
                    {
                        log::error!("fuse: writing admission rejection reply: {error}");
                    }
                }
            },
        }
    }
}

/// Reads a request from `device` into `request`: or, in a test, the error the failpoint
/// `fuse::read` returns, `EIO`, as an unexpected error of the device's (board 12891).
fn read_request(device: &File, request: &mut [u8]) -> io::Result<usize> {
    fail::fail_point!("fuse::read", |_| Err(io::Error::from_raw_os_error(libc::EIO)));
    (&*device).read(request)
}

/// Spawns `task`, a request's, on `runtime`. A panic in it outside its handler (`answer_async`
/// catches those, answered `EIO`): in [`Admission::next`], the admission's lock poisoned, the
/// reply's writing, is one of the mount's, which leaves its tenant's waiting requests unanswered
/// for good, their callers hung, and its tenant's tasks one fewer: as the daemon's service
/// `supervised`, it aborts the daemon, naming it. Tokio swallowed it before, its handle dropped
/// (board 12891, hotbed question 576). Caught around the task, rather than its holding of the
/// service counted ([`local_cell::shared::Awaited`]), which would lock the process's table of them
/// twice a request.
fn spawn_task(
    runtime: &tokio::runtime::Handle,
    supervised: Option<&'static str>,
    task: impl Future<Output = ()> + Send + 'static,
) {
    runtime.spawn(async move {
        // Unwind safe: nothing of the task's is used after its panic, the daemon aborting, or,
        // unsupervised, the task ending, as it did.
        if let Err(panic) = AssertUnwindSafe(task).catch_unwind().await {
            let Some(service) = supervised else { std::panic::resume_unwind(panic) };
            // On the thread polling the task: the process's panic hook aborts the daemon, naming
            // the service (`local_cell::shared::abort_on_panics`), as for a reader's panic.
            let _holding = local_cell::shared::Holding::work(service);
            panic!("fuse: a request's task panicked outside its handler: {}", said(&*panic));
        }
    });
}

/// A test's failpoint, `fuse::tenant::TENANT`, before each of `tenant`'s requests is handled: to
/// hold them, as a bucket that never answers holds a cell's, or to panic, as a bug outside the
/// handler does (board 12891). Runs on Tokio's blocking pool so a test callback blocking on a
/// `Condvar` never stalls Tokio's async worker threads (board 9850).
#[cfg(feature = "failpoints")]
async fn tenant_failpoint_async(tenant: &str) {
    let name = format!("fuse::tenant::{tenant}");
    let held = tokio::task::spawn_blocking(move || {
        fail::fail_point!(&name);
    });
    match held.await {
        Ok(()) => {}
        // The failpoint's panic: the task's, as one outside the handler.
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // Cancelled: the runtime shutting down, the mount's ending, which waits for no request.
        Err(error) => log::warn!("fuse: tenant {tenant}'s failpoint: {error}"),
    }
}

#[cfg(not(feature = "failpoints"))]
async fn tenant_failpoint_async(_: &str) {}

/// Has `server` handle `request` asynchronously and writes its reply to the same cloned `device`
/// fd that read it: `EIO` if its handling panicked (board 8539).
async fn answer_async(device: &File, server: &Server, request: &[u8]) {
    // Unwind safe as far as the server needs: its tables are behind locks a panic poisons, and a
    // panic through a cell's locks halts the cell (`cladefs_core::halt`).
    let handled = AssertUnwindSafe(server.handle(request)).catch_unwind().await;
    let reply = match handled {
        Ok(Ok(Some(reply))) => reply,
        Ok(Ok(None)) => return,
        Ok(Err(error)) => return log::error!("fuse: handling a request: {error:#}"),
        Err(panic) => {
            log::error!("fuse: handling a request panicked: {}", said(&*panic));
            match Server::panicked(request) {
                Some(reply) => reply,
                None => return,
            }
        }
    };
    // A reply is written whole, in one write, or not at all. If the kernel never accepted it (e.g.
    // ENOENT because the request was interrupted before the reply arrived), undo any node lookups
    // the reply recorded so a clean `Server::detach` does not wait forever for a FORGET the kernel
    // will never send.
    match (&*device).write(&reply) {
        Ok(written) if written == reply.len() => {}
        Ok(written) => {
            log::error!("fuse: {written} bytes of a {}-byte reply", reply.len());
            server.undelivered(request, &reply);
        }
        // The request was interrupted, and its reply is not wanted.
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            server.undelivered(request, &reply);
        }
        Err(error) => {
            log::error!("fuse: writing a reply: {error}");
            server.undelivered(request, &reply);
        }
    }
}

/// What a panic said, as its payload has it.
fn said(panic: &(dyn std::any::Any + Send)) -> &str {
    (panic.downcast_ref::<&str>().copied())
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic of no message")
}

/// Mounts a FUSE filesystem at `mountpoint`, and returns the device it is served through.
fn mount(mountpoint: &Path, allow_other: bool) -> anyhow::Result<(File, How)> {
    detach_dead(mountpoint)?;
    // Opened close-on-exec, as std opens files.
    let device = File::options().read(true).write(true).open("/dev/fuse").context("/dev/fuse")?;
    // SAFETY: Plain calls returning the process's IDs.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut options = format!(
        "fd={},rootmode=40000,user_id={uid},group_id={gid},default_permissions",
        device.as_raw_fd()
    );
    if allow_other {
        options.push_str(",allow_other");
    }
    let target = CString::new(mountpoint.as_os_str().as_bytes()).context("the mount point")?;
    let options = CString::new(options).expect("no NUL in the options");
    // SAFETY: mount(2), with NUL-terminated strings that outlive the call.
    let mounted = unsafe {
        libc::mount(
            c"cladefs".as_ptr(),
            target.as_ptr(),
            c"fuse.cladefs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    };
    if mounted == 0 {
        return Ok((device, How::Syscall));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EPERM) {
        return Err(anyhow::Error::new(error).context(format!("mounting {}", mountpoint.display())));
    }
    drop(device);
    let device = fusermount(mountpoint, allow_other)
        .with_context(|| format!("mounting {} through {FUSERMOUNT}", mountpoint.display()))?;
    Ok((device, How::Fusermount))
}

/// Detaches the mount at `mountpoint` if it is a dead cladefs's, as a server killed with its
/// mount leaves it (board 8537): its connection aborted, every access there, the lookup mount(2)
/// does of its target among them, fails with `ENOTCONN`, and nothing mounts there until it goes.
/// Only a cladefs's, as this process's mountinfo says (`fuse.cladefs`), is detached, lazily, as it
/// was mounted (mount(2)'s, or `fusermount3`'s, which refuses `umount2` to a user without
/// `CAP_SYS_ADMIN`): any other file system not connected there is refused, named. Anything else
/// wrong with the mount point, mounting it says.
fn detach_dead(mountpoint: &Path) -> anyhow::Result<()> {
    if !is_not_connected(mountpoint) {
        return Ok(());
    }
    // Its directory's path as the kernel says it: the mount point itself answers nothing.
    let (Some(parent), Some(name)) = (mountpoint.parent(), mountpoint.file_name()) else {
        bail!("{} is not connected", mountpoint.display());
    };
    let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
    let at = parent.canonicalize().with_context(|| format!("resolving {}", parent.display()))?;
    let at = at.join(name);
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").context("mountinfo")?;
    let fstype = mounted_at(&mountinfo, &at);
    if fstype != Some("fuse.cladefs") {
        let what = fstype.map_or("nothing mountinfo lists".to_owned(), |fstype| fstype.to_owned());
        bail!("{} is not connected, and holds {what}, no cladefs mount", mountpoint.display());
    }
    let detached = match umount(&at, true) {
        Err(error)
            if error.downcast_ref::<io::Error>().and_then(io::Error::raw_os_error)
                == Some(libc::EPERM) =>
        {
            fusermount_unmount(&at, true)
        }
        detached => detached,
    };
    detached.with_context(|| format!("detaching the dead cladefs mount at {}", at.display()))?;
    log::warn!("fuse: detached the dead cladefs mount at {}, its server gone", at.display());
    Ok(())
}

/// Whether `mountpoint` is a disconnected FUSE mount (`ENOTCONN`). Checks both `stat(2)` and
/// `statfs(2)` because Linux VFS can serve `stat(2)` from a cached root inode's `attr_valid`
/// window even after the FUSE server dies, whereas `statfs(2)` is uncached in FUSE and always
/// hits the dead connection (board 11554).
fn is_not_connected(mountpoint: &Path) -> bool {
    match std::fs::metadata(mountpoint) {
        Err(error) => return error.raw_os_error() == Some(libc::ENOTCONN),
        Ok(_) => {}
    }
    let Ok(path) = CString::new(mountpoint.as_os_str().as_bytes()) else { return false };
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `statfs(2)` takes a NUL-terminated path and writes into a caller-owned `statfs`.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } == 0 {
        return false;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN)
}

/// The type of the file system `mountinfo`, a `/proc/PID/mountinfo`'s, says is mounted at `at` on
/// top (the last line of `at`), if any.
fn mounted_at<'a>(mountinfo: &'a str, at: &Path) -> Option<&'a str> {
    let at = at.as_os_str().as_bytes();
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let point = fields.nth(4)?;
            // Past the optional fields, `-` ends.
            let fstype = fields.skip_while(|field| *field != "-").nth(1)?;
            (unescaped(point) == at).then_some(fstype)
        })
        .next_back()
}

/// `field`, a mountinfo path, its space, tab, newline and backslash unescaped from octal (`\040`).
fn unescaped(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let octal = bytes.get(at + 1..at + 4).filter(|digits| {
            byte == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        match octal {
            Some(digits) => {
                let value =
                    digits.iter().fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                out.push(value as u8);
                at += 4;
            }
            None => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}

/// Has `fusermount3` mount a FUSE filesystem at `mountpoint`, and returns the device it hands
/// back, which it opened.
fn fusermount(mountpoint: &Path, allow_other: bool) -> anyhow::Result<File> {
    // Both ends close-on-exec, as std makes sockets: the child's is kept open across its exec
    // alone, below.
    let (ours, theirs) = UnixStream::pair().context("a socket for it")?;
    let theirs_fd = theirs.as_raw_fd();
    let mut options = String::from("default_permissions,fsname=cladefs,subtype=cladefs");
    if allow_other {
        options.push_str(",allow_other");
    }
    let mut command = Command::new(FUSERMOUNT);
    command
        .arg("-o")
        .arg(options)
        .arg("--")
        .arg(mountpoint)
        .env("_FUSE_COMMFD", theirs_fd.to_string());
    // SAFETY: fcntl(2) is async-signal-safe, and the descriptor is the socket's, open in the child
    // as in this process.
    unsafe {
        command.pre_exec(move || match libc::fcntl(theirs_fd, libc::F_SETFD, 0) {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let output = command.output().with_context(|| format!("running {FUSERMOUNT}"))?;
    drop(theirs);
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        bail!("{FUSERMOUNT} failed, {}: {}", output.status, said.trim());
    }
    // It sent the device before it exited.
    receive_fd(&ours).context("the device it opened")
}

/// The file whose descriptor the peer of `socket` sent, as `fusermount3` sends it: one byte, with
/// the descriptor in an `SCM_RIGHTS` message. Received close-on-exec.
fn receive_fd(socket: &UnixStream) -> io::Result<File> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: byte.len() };
    // Room for a control message of a descriptor, aligned as its header needs.
    let mut control = [0u64; 8];
    // SAFETY: A message header, filled below.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: recvmsg(2) into buffers the header points to, which outlive the call.
    let received =
        unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: Walks the control messages recvmsg filled in, within `control`.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !cmsg.is_null() {
        // SAFETY: A control message header recvmsg wrote.
        let header = unsafe { &*cmsg };
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            // SAFETY: An SCM_RIGHTS message's data is the descriptors sent, now this process's.
            let fd = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>()) };
            // SAFETY: The descriptor is this process's, and nothing else owns it.
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        // SAFETY: As above.
        cmsg = unsafe { libc::CMSG_NXTHDR(&message, cmsg) };
    }
    Err(io::Error::other("no descriptor was sent"))
}

/// Unmounts what is mounted at `mountpoint`, which mount(2) mounted.
fn umount(mountpoint: &Path, lazily: bool) -> anyhow::Result<()> {
    let target = CString::new(mountpoint.as_os_str().as_bytes()).context("the mount point")?;
    let flags = if lazily { libc::MNT_DETACH } else { 0 };
    // SAFETY: umount2(2), with a NUL-terminated string that outlives the call.
    match unsafe { libc::umount2(target.as_ptr(), flags) } {
        0 => Ok(()),
        _ => {
            let error = io::Error::last_os_error();
            let busy = error.raw_os_error() == Some(libc::EBUSY);
            let error = anyhow::Error::new(error);
            Err(if busy { error.context(holders(mountpoint)) } else { error })
        }
    }
}

/// What holds the filesystem mounted at `mountpoint` busy, as far as this process may read
/// /proc: each process with a file under it open, or a directory under it its current or root
/// one, by ID and command (TASK-705). Others' processes this one may not read are not among them,
/// nor a call under way that holds a path in it, as a desktop's trash monitor's looking into each
/// new mount (`gvfsd-trash`) does: a strict unmount coming then fails, busy, which a lazy one
/// doesn't.
fn holders(mountpoint: &Path) -> String {
    let mut holders = Vec::new();
    for process in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = process.file_name();
        if !pid.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let at = process.path();
        let command = std::fs::read_to_string(at.join("comm")).unwrap_or_default();
        let links = ["cwd", "root"].map(|link| (link.to_owned(), at.join(link)));
        let fds = std::fs::read_dir(at.join("fd")).into_iter().flatten().flatten();
        let fds = fds.map(|fd| (format!("fd {}", fd.file_name().display()), fd.path()));
        for (what, link) in links.into_iter().chain(fds) {
            if std::fs::read_link(&link).is_ok_and(|target| target.starts_with(mountpoint)) {
                holders.push(format!("{} ({}) {what}", pid.display(), command.trim()));
            }
        }
    }
    match holders.is_empty() {
        true => "held by no process this one may see, maybe by a call under way".to_owned(),
        false => format!("held by {}", holders.join(", ")),
    }
}

/// Has `fusermount3` unmount what it mounted at `mountpoint`.
fn fusermount_unmount(mountpoint: &Path, lazily: bool) -> anyhow::Result<()> {
    let mut command = Command::new(FUSERMOUNT);
    command.arg("-u");
    if lazily {
        command.arg("-z");
    }
    let output = command
        .arg("--")
        .arg(mountpoint)
        .output()
        .with_context(|| format!("running {FUSERMOUNT}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let held = match said.contains("Device or resource busy") {
            true => format!(" ({})", holders(mountpoint)),
            false => String::new(),
        };
        bail!("{FUSERMOUNT} -u failed, {}: {}{held}", output.status, said.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tenant's requests past the most in service wait, in order, until what they hold would
    /// pass `most_waiting`: then excess requests are rejected without allocating so a stalled
    /// tenant's kept memory stays hard-bounded even under thousands of concurrent writes
    /// (hotbed review 57, board 9850, review 131).
    #[test]
    fn a_tenants_waiting_requests_are_bounded_in_bytes() {
        let queued = Arc::new(AtomicU64::new(0));
        let admission = Admission::<()>::of(NonZeroUsize::new(4).unwrap(), queued.clone());
        let admission = Admission::<()> {
            most: 2,
            most_waiting: 10,
            queued_bytes: queued.clone(),
            tenants: admission.tenants,
        };
        let (slow, fast): (Arc<str>, Arc<str>) = (Arc::from("slow"), Arc::from("fast"));
        let r1 = match admission.admit(&slow, (), b"1") {
            Decision::Serve((), r) => r,
            other => panic!("expected Serve, got {other:?}"),
        };
        let r2 = match admission.admit(&slow, (), b"2") {
            Decision::Serve((), r) => r,
            other => panic!("expected Serve, got {other:?}"),
        };
        assert!(matches!(admission.admit(&slow, (), b"three"), Decision::Queued));
        assert!(matches!(admission.admit(&slow, (), b"four"), Decision::Queued));
        // 2 in-flight bytes ("1", "2") + 9 queued bytes ("three", "four").
        assert_eq!(queued.load(Ordering::Relaxed), 11);
        // Five bytes more would hold eleven in waiting: rejected with zero extra bytes retained.
        assert!(matches!(admission.admit(&slow, (), b"fifth"), Decision::Rejected));
        assert_eq!(queued.load(Ordering::Relaxed), 11);
        let rf = match admission.admit(&fast, (), b"f") {
            Decision::Serve((), r) => r,
            other => panic!("expected Serve for fast, got {other:?}"),
        };
        assert_eq!(queued.load(Ordering::Relaxed), 12);
        drop((r1, r2, rf));
        assert_eq!(queued.load(Ordering::Relaxed), 9);
        let next = admission.next(&slow).unwrap();
        assert_eq!(&*next.1, b"three");
        drop(next);
        assert_eq!(queued.load(Ordering::Relaxed), 4);
        assert!(matches!(admission.admit(&slow, (), b"six"), Decision::Queued));
        assert_eq!(queued.load(Ordering::Relaxed), 7);
        let next = admission.next(&slow).unwrap();
        assert_eq!(&*next.1, b"four");
        drop(next);
        let next = admission.next(&slow).unwrap();
        assert_eq!(&*next.1, b"six");
        drop(next);
        assert_eq!(queued.load(Ordering::Relaxed), 0);
        for _ in 0..2 {
            assert!(admission.next(&slow).is_none());
        }
        assert!(admission.next(&fast).is_none());
        assert!(admission.tenants.lock().unwrap().is_empty(), "tenants left in service");

        // Regression for review 131: 1,000 concurrent MAX_WRITE requests for a stalled tenant
        // stay hard-bounded at (most + 64) * MAX_WRITE.
        let full_queued = Arc::new(AtomicU64::new(0));
        let full = Admission::<()>::of(NonZeroUsize::new(8).unwrap(), full_queued.clone());
        let big = vec![0u8; MAX_WRITE as usize];
        let mut active = Vec::new();
        let (mut queued_count, mut rejected_count) = (0usize, 0usize);
        for _ in 0..1000 {
            match full.admit(&slow, (), &big) {
                Decision::Serve((), r) => active.push(r),
                Decision::Queued => queued_count += 1,
                Decision::Rejected => rejected_count += 1,
            }
        }
        assert_eq!(active.len(), 6);
        assert_eq!(queued_count, 64);
        assert_eq!(rejected_count, 930);
        assert_eq!(full_queued.load(Ordering::Relaxed), (6 + 64) * MAX_WRITE as u64);
        drop(active);
        while let Some(((), r)) = full.next(&slow) {
            drop(r);
        }
        for _ in 0..5 {
            assert!(full.next(&slow).is_none());
        }
        assert_eq!(full_queued.load(Ordering::Relaxed), 0);
    }

    /// Plain single-cell mounts apply reader backpressure once in-flight request bytes reach the
    /// budget, waking waiters as soon as an in-flight request completes (review 131).
    #[test]
    fn plain_mount_budget_bounds_in_flight_bytes() {
        let budget = Arc::new(PlainBudget::new(10));
        let p1 = budget.acquire(6);
        let p2 = budget.acquire(4);
        let (tx, rx) = std::sync::mpsc::channel();
        let b2 = Arc::clone(&budget);
        let waiter = std::thread::spawn(move || {
            let _p3 = b2.acquire(5);
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(50)).is_err());
        drop(p1);
        rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        drop(p2);
        waiter.join().unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 0);
    }

    /// A mount point is found as mountinfo escapes it, its space, tab, newline and backslash in
    /// octal, the last mount at it the one on top; past the optional fields, whatever their count.
    #[test]
    fn mountinfo_names_what_is_mounted_on_top() {
        let mountinfo = "\
            22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw\n\
            90 22 0:50 / /tmp/a\\040b rw - ext4 /dev/sda rw\n\
            91 90 0:51 / /tmp/a\\040b rw,nosuid shared:7 master:3 - fuse.cladefs cladefs rw\n\
            92 22 0:52 / /tmp/c\\134d\\011e rw - fuse.other other rw\n\
            93 22 0:53 / /tmp/bad\\9 rw - tmpfs tmpfs rw\n";
        assert_eq!(mounted_at(mountinfo, Path::new("/tmp/a b")), Some("fuse.cladefs"));
        assert_eq!(mounted_at(mountinfo, Path::new("/tmp/c\\d\te")), Some("fuse.other"));
        assert_eq!(mounted_at(mountinfo, Path::new("/tmp/bad\\9")), Some("tmpfs"));
        assert_eq!(mounted_at(mountinfo, Path::new("/tmp/a")), None);
        assert_eq!(mounted_at("truncated line", Path::new("/tmp/a b")), None);
    }
}
