// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's side of a connection: a VM process's cell and window, served from this process's
//! cells, extent cache and kernel.

use crate::Daemon;
use crate::cells::{self, cell_dir_key, open_cell_at};
use crate::codec::{
    self, Data, Fault, Forked, Info, Made, Opened, ReadInto, Request, Stage, StageOp, Told,
    stream_bytes,
};
use crate::reads::Kind;
use crate::stats;
use crate::wire::*;
use bucket_cell::trace;
use cladefs_core::cell_table::{Shrinking, Writer};
use cladefs_core::{Commit, MappingContents, VolatileSlice, VolatileSliceMut, WindowMapping};
use cladefs_core::{DaxWindow, Errno, OpenCell, SecondStage};
use local_cell::shared::SharedLock;
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak, mpsc};

/// How many of a connection's requests the daemon serves at once: a VM's device queues', and
/// faults waiting for the pager. Each on the thread that received it ([`receive`]); those waiting
/// for one of them, then, on the threads every connection shares ([`Pool`]).
const WORKERS: usize = 8;

/// cladefs: doc-4's stream transport (board 14663): the guest's data requests over vsock, which the
/// daemon serves itself. Linux alone, and not under madsim, whose one deterministic thread can't
/// host a stream's threads of their own, blocking on its socket, nor has the simulation Unix
/// sockets: there a `GUEST_STREAM` is refused `ENOTSUP`, the interpreter and the cell it serves
/// with run by the simulation's other paths.
#[cfg(all(target_os = "linux", not(madsim)))]
mod stream;

/// cladefs: how many of a connection's faults and `PRIVATE`s the daemon resolves at once
/// (TASK-149.34), each paging in what it waits for: a part of an extent, a MiB, and the two after
/// it if it reads on (`cache::PARTS_AHEAD`), each part's frames of its tenant's memory held until
/// its read, or fetch from the bucket, answers. A fault waiting for the pager holds no worker
/// (TASK-92.20), so as many paged in at once as the VM process kept faults in flight, 4,096 at most
/// (`MAX_REQUESTS`): 256 faults on cold extents held 258 MiB of the tenant's memory, and 123 MiB of
/// the heap as their fetches answered; 66 and 39 now. Those past it wait their turn, holding
/// nothing. A guest faults as many pages at once as it has threads faulting: its vCPUs, and on x86
/// KVM's asynchronous page faults' workers, 64 a vCPU queued at most; resolving 64 at once, each a
/// part from object storage in 30 to 100 ms, pages in hundreds of MiB a second for it.
pub(super) const PAGE_INS: usize = 64;

/// cladefs: how long a commit only ordered (`COMMIT` without `COMMIT_UPLOADED`) waits at most to be
/// made local: the host's lag, which doc-2's requirement 11 bounds.
const FLUSH_EVERY: std::time::Duration = std::time::Duration::from_millis(50);

/// Serves the VM processes that connect to `listener` on `daemon`, for as long as it listens
/// ([`serve_as`]): the default tenant's ([`default_tenant_on`]). Their requests run on threads
/// every connection shares (`Daemon::pool`, TASK-34).
pub(super) fn serve_on(daemon: &Arc<Daemon>, listener: UnixListener) {
    let _serving = crate::Serving::begin(daemon);
    let serving = Arc::clone(daemon);
    let stop = match StopAccepting::once_made() {
        Ok(stop) => stop,
        Err(err) => return log::error!("cladefsd: serving a VM socket: {err}: serving none"),
    };
    serve_as(daemon, listener, stop, move || default_tenant_on(&serving));
}

/// Serves the VM processes that connect to `listener` on the process's default daemon
/// ([`serve_on`]).
#[cfg(not(madsim))]
pub fn serve(listener: UnixListener) {
    serve_on(Daemon::process(), listener);
}

/// The default tenant's socket served as [`serve_on`] serves it, with a stop made beforehand: for
/// the test of accepting on through EMFILE (Linux's).
#[cfg(all(test, not(madsim), target_os = "linux"))]
pub(super) fn serve_with_stop(listener: UnixListener, stop: Arc<StopAccepting>) {
    let daemon = Daemon::process();
    let _serving = crate::Serving::begin(daemon);
    let serving = Arc::clone(daemon);
    serve_as(daemon, listener, stop, move || default_tenant_on(&serving));
}

/// Serves the VM processes of `tenant` that connect to `listener` on `daemon`, as [`serve_on`]
/// does (TASK-43): until its grant is revoked, which shuts the listener down.
pub(super) fn serve_tenant_on(
    daemon: &Arc<Daemon>,
    listener: UnixListener,
    tenant: Arc<Tenant>,
    stop: Arc<StopAccepting>,
) {
    serve_as(daemon, listener, stop, move || Ok(tenant.clone()));
}

/// Serves the VM processes of `tenant` that connect to `listener` on the process's default daemon
/// ([`serve_tenant_on`]).
#[cfg(not(madsim))]
pub fn serve_tenant(listener: UnixListener, tenant: Arc<Tenant>) {
    match StopAccepting::once_made() {
        Ok(stop) => serve_tenant_on(&tenant.daemon.clone(), listener, tenant, stop),
        Err(err) => log::error!("cladefsd: serving a tenant's VM socket: {err}: serving none"),
    }
}

/// cladefs: how many mappings the host lets a process make, which the reply to an `OPEN` tells the
/// VM process ([`Opened`]), as it can't read it in a jail without /proc (TASK-172): Linux's
/// default, said, if this process can't either.
fn max_map_count() -> Option<u64> {
    #[cfg(test)]
    if let Some(&given) = MAX_MAP_COUNT_GIVEN.get() {
        return Some(given);
    }
    cladefs_core::pages::max_map_count().unwrap_or_else(|err| {
        let default = cladefs_core::pages::DEFAULT_MAX_MAP_COUNT;
        log::warn!("cladefsd: the host's vm.max_map_count: {err}: telling VM processes {default}");
        Some(default)
    })
}

/// cladefs: what the daemon tells VM processes of the host's mappings in a test, in place of the
/// host's ([`max_map_count`]).
#[cfg(test)]
pub(super) static MAX_MAP_COUNT_GIVEN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// cladefs: how long an accept that failed waits to be tried again, as files close (TASK-79.35).
pub(super) const ACCEPT_AGAIN: std::time::Duration = std::time::Duration::from_millis(50);

/// Serves the VM processes of the tenant `tenant` gives, as each connects, that connect to
/// `listener`, until it listens no more, shut down (`EINVAL`), or `stop` is said: one whose
/// tenant it fails to give, logged, it closes. An accept that fails otherwise, as with as many
/// files open as the daemon may, is tried again after [`ACCEPT_AGAIN`], and its connection served
/// then: given up on, it would leave every VM process that connects to it unserved, a tenant's
/// socket bound still (TASK-79.35).
fn serve_as(
    daemon: &Arc<Daemon>,
    listener: UnixListener,
    stop: Arc<StopAccepting>,
    tenant: impl Fn() -> Result<Arc<Tenant>, Errno>,
) {
    // cladefs: until the daemon drains, which stops it (TASK-141.4); dropped before it.
    let _accepting = Accepting::new(daemon, &listener, &stop);
    if let Err(err) = listener.set_nonblocking(true) {
        return log::error!("cladefsd: serving a VM socket: {err}: serving none");
    }
    let mut failing = false;
    loop {
        match stop.accepting(&listener) {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => {
                log::error!("cladefsd: waiting for a VM process's connection: {err}: serving none");
                return;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                failing = false;
                if let Ok(tenant) = tenant() {
                    drop(start_connection(stream.into(), tenant, Connector::Socket));
                }
            }
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => return,
            // Another descriptor's accept took it, or none was there: waited for again.
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            // That connection's, gone before it was accepted.
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted => {}
            Err(err) => {
                if !failing {
                    log::error!(
                        "cladefsd: accepting a VM process's connection: {err}: trying again"
                    );
                    failing = true;
                }
                std::thread::sleep(ACCEPT_AGAIN);
            }
        }
    }
}

/// cladefs: a VM socket's listener served, in `daemon.accepting` until dropped, before the
/// listener is: stopped at once if the daemon drains, as it may have as it came.
struct Accepting {
    daemon: Arc<Daemon>,
    fd: std::os::fd::RawFd,
}

impl Accepting {
    fn new(daemon: &Arc<Daemon>, listener: &UnixListener, stop: &Arc<StopAccepting>) -> Self {
        use std::os::fd::AsRawFd;
        let mut accepting = daemon.accepting.lock().unwrap();
        if crate::drain::draining(daemon) {
            stop.stop(listener.as_fd());
        }
        let fd = listener.as_raw_fd();
        accepting.push((fd, Arc::clone(stop)));
        Self { daemon: Arc::clone(daemon), fd }
    }
}

impl Drop for Accepting {
    fn drop(&mut self) {
        self.daemon.accepting.lock().unwrap().retain(|(fd, _)| *fd != self.fd);
    }
}

/// cladefs: has every VM socket served on `daemon` take no more connections, as the daemon drains
/// (TASK-141.4): their accepts stopped ([`StopAccepting`]), and connecting refused.
pub(super) fn stop_accepting(daemon: &Daemon) {
    for (fd, stop) in daemon.accepting.lock().unwrap().iter() {
        // SAFETY: Its listener lives while it is there: its `Accepting` takes it out first.
        stop.stop(unsafe { std::os::fd::BorrowedFd::borrow_raw(*fd) });
    }
}

/// cladefs: what stops a VM socket's accepts ([`serve_as`]), as a tenant's grant is revoked and
/// the daemon drains: its listener shut down, which on Linux fails every accept of it (`EINVAL`)
/// and refuses a connection at once; and a pipe written, which the accepting thread waits on
/// beside the listener, which wakes it on macOS too, whose `shutdown(2)` of a listener does
/// nothing (`ENOTCONN`): the thread returns, its listener closed, and a connection refused then
/// (board 13065).
pub(crate) struct StopAccepting {
    /// The pipe's ends: readable once stopped.
    stopped: std::os::fd::OwnedFd,
    stop: std::os::fd::OwnedFd,
}

impl StopAccepting {
    /// A stop, not said yet, for a listener about to be served: made once descriptors are to be
    /// had, as its accepts are tried again until they are (TASK-79.35), the process having as many
    /// open as it may; an error of another kind returned.
    pub(crate) fn once_made() -> std::io::Result<Arc<Self>> {
        let mut failing = false;
        loop {
            match Self::new() {
                Err(err) if matches!(err.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) => {
                    if !failing {
                        log::error!("cladefsd: serving a VM socket: {err}: trying again");
                        failing = true;
                    }
                    std::thread::sleep(ACCEPT_AGAIN);
                }
                made => return made,
            }
        }
    }

    /// A stop, not said yet.
    pub(crate) fn new() -> std::io::Result<Arc<Self>> {
        use std::os::fd::FromRawFd;
        let mut ends = [0; 2];
        // SAFETY: Makes a pipe, its two descriptors written into `ends`: close-on-exec as they are
        // made on Linux, so that no VM process started meanwhile inherits them.
        #[cfg(target_os = "linux")]
        let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
        // SAFETY: As above; macOS has no pipe2, so close-on-exec set below, a moment after.
        #[cfg(not(target_os = "linux"))]
        let made = unsafe { libc::pipe(ends.as_mut_ptr()) };
        if made != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: Just made, and owned here.
        let (stopped, stop) = unsafe {
            (std::os::fd::OwnedFd::from_raw_fd(ends[0]), std::os::fd::OwnedFd::from_raw_fd(ends[1]))
        };
        #[cfg(not(target_os = "linux"))]
        for end in &ends {
            // SAFETY: Sets a flag of a descriptor just made, owned above.
            if unsafe { libc::fcntl(*end, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Arc::new(Self { stopped, stop }))
    }

    /// Stops the accepts of `listener`, a descriptor of the socket they accept on.
    pub(crate) fn stop(&self, listener: std::os::fd::BorrowedFd<'_>) {
        use std::os::fd::AsRawFd;
        crate::shut_down(listener);
        // SAFETY: Writes a byte to the pipe's end this owns. Full, it was written before: stopped.
        if unsafe { libc::write(self.stop.as_raw_fd(), [1u8].as_ptr().cast(), 1) } < 0 {
            let err = std::io::Error::last_os_error();
            log::warn!("cladefsd: stopping a VM socket's accepts: {err}");
        }
    }

    /// Waits until `listener` has a connection to accept, or is shut down: `true`; or until
    /// stopped: `false`.
    fn accepting(&self, listener: &UnixListener) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut polled = [
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.stopped.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        loop {
            // SAFETY: Polls descriptors the caller and this keep open, until one is ready.
            if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } >= 0 {
                return Ok(polled[1].revents == 0);
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

/// A tenant of the daemon (TASK-43): its VM processes' cells' pages are of an arena of the
/// kernel's physical memory of its own, which they hold, with the default arena, read-only, whose
/// pages are those of the public bases its cells are forks of, and nothing of another tenant's;
/// and its cells are in a directory of its own, which its VM processes name no cell outside of.
/// This is defence in depth: a VM process escaped from its guest reaches its tenant's pages and
/// cells alone. Guest isolation is the security boundary, which nothing relies on tenants for.
/// What else its VM processes may do its [`Policy`] says: a granted tenant's, or the default
/// tenant's, of the daemon's own socket ([`default_tenant_on`]).
///
/// Its arena goes, and the extents cached in it, once it is gone, as its VM processes are, and the
/// cells opened in its arena, and what reads into the extent cache for them (`pages::Arena`).
pub struct Tenant {
    daemon: Arc<Daemon>,
    /// cladefs: its ID in the bucket (TASK-364), whose cells its cells are, which names
    /// it in every call of the control API: a granted tenant's, its grant's; the default tenant's,
    /// [`DEFAULT_TENANT`](crate::host_cell::DEFAULT_TENANT).
    id: u64,
    policy: Policy,
    memory: Memory,
    /// cladefs: the host's disk its directory takes, a granted tenant's (TASK-94).
    disk: Option<Arc<crate::disk::Disk>>,
}

/// cladefs: what a tenant's VM processes may do (TASK-160.10), which its connections hold them to.
struct Policy {
    /// Where they name cells.
    cell_dirs: CellDirs,
}

/// cladefs: where a tenant's VM processes name cells ([`Policy`]).
enum CellDirs {
    /// In its directory (or JBOD directories) alone, canonical, sorted and deduplicated
    /// (TASK-43.4).
    Dirs(Arc<[PathBuf]>),
    /// In the daemon's roots, if it has any ([`roots`]); anywhere, if it has none: the default
    /// tenant's. A booted daemon has some, refusing to boot owning no disk (board 14305): none is
    /// a test's daemon serving without a boot (board 14318).
    Roots(Option<Arc<[PathBuf]>>),
}

impl Policy {
    /// A granted tenant's, whose cells are in `dirs`, canonical, sorted and deduplicated.
    fn granted(dirs: Vec<PathBuf>) -> Self {
        Self { cell_dirs: CellDirs::Dirs(dirs.into()) }
    }

    /// The default tenant's ([`default_tenant_on`]).
    fn default_tenant(daemon: &Daemon) -> Self {
        Self { cell_dirs: CellDirs::Roots(roots(daemon)) }
    }
}

/// The memory a tenant's VM processes hold (TASK-43): its arena of physical memory,
/// which its cells' own pages are of, and the default arena, read-only, whose pages are public
/// bases'. The default tenant's is as any other's ([`default_tenant_on`], TASK-79.17).
struct Memory {
    arena: Arc<cladefs_core::pages::Arena>,
    /// Its arena's memory, reopened read-only: its VM processes copy their guests' writes into
    /// memory of their own (doc-26), and write no page of it.
    memory: std::os::fd::OwnedFd,
    /// The default arena's memory, reopened read-only.
    public: std::os::fd::OwnedFd,
    /// cladefs: how many connections the tenant's VM processes have ([`MAX_CONNECTIONS`]).
    connections: Arc<std::sync::atomic::AtomicUsize>,
    /// cladefs: what its connections have the daemon hold of its VM processes' requests together,
    /// within the bytes in flight they share (TASK-149.35).
    in_flight: Arc<crate::memory::InFlight>,
}

impl Memory {
    /// An arena of its own, and the default one, each read-only.
    fn new(daemon: &Daemon) -> std::io::Result<Self> {
        use std::os::fd::AsFd;
        let public =
            reopen_read_only(cladefs_core::pages::physical_memory_on(&daemon.host)?.as_fd())?;
        let arena = cladefs_core::pages::Arena::new_on(&daemon.host)?;
        let memory = reopen_read_only(
            cladefs_core::pages::arena_memory_on(&daemon.host, arena.id())
                .expect("a new arena")
                .as_fd(),
        )?;
        let connections = Default::default();
        // cladefs: within every tenant's together (board 8465).
        let in_flight = Arc::new(crate::memory::in_flight_of_a_tenant(daemon));
        Ok(Self { arena, memory, public, connections, in_flight })
    }
}

/// cladefs: the most connections a tenant's VM processes, the default tenant's too, have at once
/// (TASK-79.16): a few times the VMs a host runs. Those past it are closed as they come, as each
/// holds the daemon's memory, as much as the protocol bounds, files and tasks. VMs restored from
/// a zygote, two connections each, their root's and their memory's, took 1024 at 512 VMs, ubu1's
/// 16 GiB holding 497 with 3.9 GiB to spare (board 7084).
pub(super) const MAX_CONNECTIONS: usize = 4096;

/// cladefs: how many connections each tenant alive keeps, whatever the others have (board 8465): a
/// tenant with fewer is refused none while the daemon has room for any, however many the others
/// have taken; those past it take what all of them leave. A VM restored from a zygote has two
/// (hotbed question 483).
pub(super) const RESERVED_CONNECTIONS: usize = 64;

/// cladefs: the most connections the daemon serves at once, every tenant's together (board 8465):
/// what its open files leave, each connection's at their most, a quarter of its limit kept for the
/// rest (its own, its part cache's, its control API's and the cells it opens for it, its peers').
/// A connection's files are its cell's, which it opens one of at most, at their most
/// (`super::cell_open_files`, board 9975): its segments' and extents', its open extents' raw files
/// (board 12555), its sockets and its VM process's pidfd. Before, 4 a connection, as if its cell's
/// were the rest's: one tenant's cells took every file the daemon had far below the cap, 18
/// connections of 56 at a limit of 300, and another tenant's VM processes, within their reserved
/// share, failed to connect (`EMFILE`), as their cells would have to open. Past it, an accept that
/// failed for files held every tenant's new VM processes, retried every [`ACCEPT_AGAIN`],
/// whichever tenant had taken them.
pub(crate) fn most_connections() -> usize {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: Reads the process's limit into `limit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        let err = std::io::Error::last_os_error();
        // A call with a valid resource and buffer, which never fails: none served if it does.
        log::error!("cladefsd: reading the limit of open files: {err}: serving no connection");
        return 0;
    }
    // Unlimited, RLIM_INFINITY, as many as a usize counts.
    let files = usize::try_from(limit.rlim_cur).unwrap_or(usize::MAX);
    // A few hundred files: as many as a usize counts.
    let per_connection = usize::try_from(crate::cell_open_files()).unwrap_or(usize::MAX);
    (files - files / 4) / per_connection
}

/// cladefs: the connections the daemon serves at once, of the most ([`most_connections`]), and how
/// many each tenant keeps ([`RESERVED_CONNECTIONS`]): as a test gives them, if it does.
fn connection_limits() -> (usize, usize) {
    #[cfg(test)]
    if let Some(&(most, reserved)) = CONNECTION_LIMITS_GIVEN.get() {
        return (most.unwrap_or_else(most_connections), reserved);
    }
    (most_connections(), RESERVED_CONNECTIONS)
}

/// cladefs: the limits of connections in a test, in place of the daemon's ([`connection_limits`]):
/// the most the daemon serves, or, if none, what the process's limit of open files leaves
/// ([`most_connections`]); and how many each tenant keeps. The process's, a test's, which runs in
/// a process of its own.
#[cfg(test)]
pub(super) static CONNECTION_LIMITS_GIVEN: OnceLock<(Option<usize>, usize)> = OnceLock::new();

/// cladefs: the most connections a tenant's VM processes have at once: [`MAX_CONNECTIONS`], or as
/// a test gives it ([`TENANT_CONNECTIONS_GIVEN`]).
fn tenant_connections() -> usize {
    #[cfg(test)]
    if let Some(&most) = TENANT_CONNECTIONS_GIVEN.get() {
        return most;
    }
    MAX_CONNECTIONS
}

/// cladefs: test override for [`STALL`] so tests that intentionally queue `MAX_STREAM_BYTES`
/// unread on heavily loaded debug CI runners do not trip the 5 s stall watchdog.
#[cfg(test)]
pub(super) static STALL_GIVEN: OnceLock<std::time::Duration> = OnceLock::new();

fn stall_bound() -> std::time::Duration {
    #[cfg(test)]
    if let Some(&stall) = STALL_GIVEN.get() {
        return stall;
    }
    STALL
}

/// cladefs: a tenant's most connections in a test, in place of [`MAX_CONNECTIONS`]: a process
/// holding both ends of 4096, three files each with the daemon's clone, passes the 10,240 files
/// macOS lets a process have by default (`kern.maxfilesperproc`, board 13850).
#[cfg(test)]
pub(super) static TENANT_CONNECTIONS_GIVEN: OnceLock<usize> = OnceLock::new();

/// cladefs: a tenant's connection counted among its connections, and every tenant's on its daemon,
/// until dropped.
struct Counted {
    daemon: Arc<Daemon>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
}

impl Counted {
    /// A connection of `tenant`, counted: none past its [`MAX_CONNECTIONS`], nor past the most the
    /// daemon serves at once ([`connection_limits`]) but for the share each tenant alive keeps of
    /// them ([`RESERVED_CONNECTIONS`]), so that tenants at their own limit, or together at the
    /// daemon's, refuse no other's VM processes (board 8465). Why not, if refused.
    fn new(tenant: &Tenant) -> Result<Self, String> {
        let (most, reserved) = connection_limits();
        let mut total = tenant.daemon.connected.lock().unwrap();
        let connections = &tenant.memory.connections;
        let own = connections.load(Ordering::Acquire);
        let tenants_most = tenant_connections();
        if own >= tenants_most {
            return Err(format!("its tenant has {tenants_most}"));
        }
        // Of the others' shares, what their VM processes leave, which this one takes none of.
        let kept = crate::memory::reserved_connections_left_on(&tenant.daemon, reserved);
        let room = match own < reserved {
            // Its own share's, which the others leave it.
            true => *total < most,
            false => total.saturating_add(kept) < most,
        };
        if !room {
            return Err(format!(
                "the daemon serves {} of the {most} it serves at once, {kept} kept for tenants with \
                 fewer than {reserved}",
                *total
            ));
        }
        *total += 1;
        connections.fetch_add(1, Ordering::AcqRel);
        Ok(Self { daemon: Arc::clone(&tenant.daemon), connections: connections.clone() })
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        let mut total = self.daemon.connected.lock().unwrap();
        *total -= 1;
        self.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

/// cladefs: `daemon`'s default tenant, if a VM process of its made it ([`default_tenant_on`]).
pub(super) fn default_tenant_made(daemon: &Daemon) -> Option<Arc<Tenant>> {
    daemon.default_tenant.get().cloned()
}

/// cladefs: `daemon`'s default tenant, of the daemon's own socket, which its user's VM processes
/// connect to (TASK-155), made as the first does, for as long as the daemon runs: granted by no
/// one, and limited by every tenant's limits alone. Its policy is its own
/// ([`Policy::default_tenant`]); its memory is as a granted tenant's (TASK-79.17): an arena of its
/// own, so that the default arena holds public bases' pages, which every VM process holds
/// read-only, and nothing of a VM process's own. `EIO`, logged, if its memory can't be made.
pub(super) fn default_tenant_on(daemon: &Arc<Daemon>) -> Result<Arc<Tenant>, Errno> {
    if let Some(tenant) = daemon.default_tenant.get() {
        return Ok(tenant.clone());
    }
    let memory = Memory::new(daemon).map_err(|err| {
        log::error!("cladefsd: the default tenant's memory: {err}");
        Errno::EIO
    })?;
    let (policy, disk) = (Policy::default_tenant(daemon), None);
    let id = cladefs_core::host_cell::DEFAULT_TENANT;
    let tenant = Tenant { daemon: Arc::clone(daemon), id, policy, memory, disk };
    Ok(daemon.default_tenant.get_or_init(|| Arc::new(tenant)).clone())
}

#[cfg(all(test, not(madsim)))]
pub(super) fn default_tenant() -> Result<Arc<Tenant>, Errno> {
    default_tenant_on(Daemon::process())
}

impl Tenant {
    /// A new tenant on `daemon`, granted, of the ID `id` in the bucket (TASK-364), whose cells are
    /// in `dir`, with an arena of its own.
    #[cfg(not(madsim))]
    pub(super) fn new_on(
        daemon: &Arc<Daemon>,
        id: u64,
        dir: impl AsRef<Path>,
    ) -> std::io::Result<Arc<Self>> {
        Self::new_many_on(daemon, id, &[dir.as_ref().to_path_buf()])
    }

    /// A new tenant on `daemon`, granted, of the ID `id` in the bucket, whose cells may live in
    /// any of the JBOD directories `dirs`, with an arena of its own and an aggregate disk limit.
    pub(super) fn new_many_on(
        daemon: &Arc<Daemon>,
        id: u64,
        dirs: &[PathBuf],
    ) -> std::io::Result<Arc<Self>> {
        if dirs.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a granted tenant needs at least one directory",
            ));
        }
        let (dirs, disk) = local_cell::upload::blocking(|| -> std::io::Result<_> {
            let mut canon = Vec::with_capacity(dirs.len());
            for dir in dirs {
                canon.push(host_fs::canonicalize(dir)?);
            }
            canon.sort();
            canon.dedup();
            let disk = Some(crate::disk::Disk::new_many_on(daemon, id, &canon)?);
            Ok((canon, disk))
        })?;
        let policy = Policy::granted(dirs);
        let tenant = Arc::new(Self {
            daemon: Arc::clone(daemon),
            id,
            policy,
            memory: Memory::new(daemon)?,
            disk,
        });
        crate::memory::register_on(daemon, &tenant);
        Ok(tenant)
    }

    /// A new tenant on the process's default daemon ([`Self::new_on`]).
    #[cfg(not(madsim))]
    pub fn new(id: u64, dir: impl AsRef<Path>) -> std::io::Result<Arc<Self>> {
        Self::new_on(Daemon::process(), id, dir)
    }

    /// cladefs: its ID in the bucket (TASK-364), which names it.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// cladefs: its ID, if it is granted (TASK-364): the default tenant's VM processes are the
    /// daemon's user's (TASK-155), which reach any tenant's cells, and make them of the default
    /// tenant's.
    fn granted(&self) -> Option<u64> {
        self.dirs().map(|_| self.id)
    }

    /// cladefs: `EACCES` if it is granted and the cell at `path` is another tenant's cell, or
    /// cell (TASK-364), as its description says, read before it is opened, which would take its
    /// cell from its writer: the daemon makes its own cells alone in its directory, but
    /// for the versions of others' its VM manager gives it, which it restores from, as forks of its
    /// own; and a cell put there by other means is no more its than it was.
    fn reaches(&self, path: &Path) -> Result<(), Errno> {
        let Some(id) = self.granted() else { return Ok(()) };
        match local_cell::upload::blocking(|| local_cell::host_cell(path)) {
            Ok(cell) if cell.tenant != id => Err(Errno::EACCES),
            // No cell yet, which it makes of its own cell; or one its opening fails, saying why.
            _ => Ok(()),
        }
    }

    /// Its cells' directories, canonical and sorted, if it is a granted tenant's: the default
    /// tenant has none ([`CellDirs::Roots`]).
    pub fn dirs(&self) -> Option<&[PathBuf]> {
        match &self.policy.cell_dirs {
            CellDirs::Dirs(dirs) => Some(dirs),
            CellDirs::Roots(_) => None,
        }
    }

    /// Its first cells' directory, if it is a granted tenant's: the default tenant has none
    /// ([`CellDirs::Roots`]).
    pub fn dir(&self) -> Option<&Path> {
        self.dirs().and_then(|dirs| dirs.first()).map(PathBuf::as_path)
    }

    /// Its arena of physical memory.
    pub fn arena(&self) -> cladefs_core::pages::ArenaId {
        self.memory.arena.id()
    }

    /// cladefs: the daemon it is a tenant of (the memory backend's, board 14630).
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(crate) fn daemon(&self) -> &Arc<Daemon> {
        &self.daemon
    }

    /// cladefs: the read-only descriptor of arena `arena`'s memory, which a VM process of its may
    /// map: its own arena's, or the default's, which public bases' parts are of; none of another
    /// tenant's (the memory backend's lists, board 14630).
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(crate) fn arena_memory(
        &self,
        arena: cladefs_core::pages::ArenaId,
    ) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        if arena == self.arena() {
            Some(self.memory.memory.as_fd())
        } else if arena == cladefs_core::pages::ArenaId::DEFAULT {
            Some(self.memory.public.as_fd())
        } else {
            None
        }
    }

    /// cladefs: its arena itself, which its cells' own pages are of: an embedded cell's, outside
    /// the simulation as `embedded` is (board 14256).
    #[cfg(not(madsim))]
    pub(super) fn memory_arena(&self) -> &Arc<cladefs_core::pages::Arena> {
        &self.memory.arena
    }

    /// cladefs: what its connections have the daemon hold of its VM processes' requests, within the
    /// bytes in flight they share (TASK-149.35).
    pub(super) fn in_flight(&self) -> &Arc<crate::memory::InFlight> {
        &self.memory.in_flight
    }

    /// cladefs: how many connections its VM processes have ([`Counted`], board 8465).
    pub(super) fn connections(&self) -> usize {
        self.memory.connections.load(Ordering::Acquire)
    }

    /// cladefs: the host's disk its directory takes, a granted tenant's (TASK-94): the default
    /// tenant has none.
    pub fn disk(&self) -> Option<&Arc<crate::disk::Disk>> {
        self.disk.as_ref()
    }

    /// Whether its VM processes may name the cell at `path`, whichever way `path` names it:
    /// `EACCES` unless it is where they name cells ([`CellDirs`]).
    fn holds(&self, path: &Path) -> Result<(), Errno> {
        let held = match &self.policy.cell_dirs {
            CellDirs::Dirs(dirs) => dirs.iter().any(|dir| in_dir(path, dir)),
            CellDirs::Roots(Some(roots)) => {
                let key = cell_dir_key(path);
                roots.iter().any(|root| key_in_dir(&key, root))
            }
            CellDirs::Roots(None) => true,
        };
        if held { Ok(()) } else { Err(Errno::EACCES) }
    }
}

/// `fd`, reopened read-only: a shared memory object no mapping of which can write it.
#[cfg(target_os = "linux")]
fn reopen_read_only(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsRawFd;
    let path = std::ffi::CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
    // SAFETY: Opens a path, taking the new descriptor.
    let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Just opened, and owned here.
    Ok(unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) })
}

/// cladefs: `fd` again, on macOS: an arena's object there is a file unlinked as it is made
/// (`slabs`), and so can't be opened anew, read-only or not. Writable, then: a simulated guest is
/// in the simulation's process, and maps nothing it is handed; but a VM process connecting on
/// macOS could write the pages it is handed (board 13541), which none maps until its window does
/// (board 13069).
#[cfg(not(target_os = "linux"))]
fn reopen_read_only(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::os::fd::OwnedFd> {
    fd.try_clone_to_owned()
}

/// Serves the VM process at the other end of `stream`, of `tenant`, until it closes the
/// connection, or exits: then its cell and window go. Returns then: for tests, which serve
/// connections one by one.
#[cfg(test)]
pub(crate) fn serve_connection(stream: std::os::unix::net::UnixStream, tenant: Arc<Tenant>) {
    let _ = start_connection(stream.into(), tenant, Connector::Socket).recv();
}

/// cladefs: a connection of this process's own to the daemon, served as a VM process's is, of
/// `tenant`'s: its end. Under madsim, in memory, a simulated guest's (doc-24, TASK-685.6), as the
/// simulation has no Unix sockets; outside it, an end of an anonymous socket pair, which no other
/// process reaches (no listener, no path), and through which the daemon's descriptors pass as
/// through a VM process's socket: a device of a process embedding the daemon (libcladefs, board
/// 13471, hotbed question 565).
pub(crate) fn connect_in_memory(tenant: Arc<Tenant>) -> std::io::Result<crate::wire::Stream> {
    #[cfg(madsim)]
    let (daemons, ours) = crate::wire::Stream::pair();
    #[cfg(not(madsim))]
    let (daemons, ours) = std::os::unix::net::UnixStream::pair()?;
    drop(start_connection(daemons, tenant, Connector::Socket));
    Ok(ours)
}

/// cladefs: serves the VM process `child` at the other end of `stream` on `daemon`, of its default
/// tenant, as one connecting to its socket is ([`serve_on`]), on the daemon's runtime: an end of a
/// socket pair an embedding process handed a VM process it started (board 13460), the child it has
/// not reaped yet. The socket says the embedding process's own pid (`SO_PEERCRED`, made by it), so
/// the child is named instead, as the process the daemon watches and may kill.
#[cfg(not(madsim))]
pub(crate) fn serve_stream_on(
    daemon: &Arc<Daemon>,
    stream: std::os::unix::net::UnixStream,
    child: libc::pid_t,
) -> Result<(), Errno> {
    drop(start_connection(stream, default_tenant_on(daemon)?, Connector::Child(child)));
    Ok(())
}

/// cladefs: who the VM process at the other end of a connection is, as the daemon learns it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Connector {
    /// As its socket says (`SO_PEERPIDFD`; macOS's `LOCAL_PEERPID`): one that connected.
    Socket,
    /// The child of this process's of the pid, not reaped yet: one handed an end of a socket pair
    /// (`Daemon::serve_vm_process`).
    #[cfg(not(madsim))]
    Child(libc::pid_t),
}

/// Starts serving the VM process at the other end of `stream`, as [`serve_connection`] does, on
/// the daemon's runtime: no thread of its own. What it returns says when it is done.
/// cladefs: one of a tenant past [`MAX_CONNECTIONS`], or past the daemon's most, it closes at once
/// ([`Counted::new`]).
/// cladefs: one coming as the daemon drains it closes at once (TASK-141.4).
fn start_connection(
    stream: crate::wire::Stream,
    tenant: Arc<Tenant>,
    connector: Connector,
) -> mpsc::Receiver<()> {
    let daemon = Arc::clone(&tenant.daemon);
    let (done, finished) = mpsc::channel();
    let counted = match Counted::new(&tenant) {
        Ok(counted) => counted,
        Err(why) => {
            log::error!(
                "cladefsd: refusing a VM process's connection of tenant {}: {why}",
                tenant.id
            );
            return finished;
        }
    };
    let Some(connected) = daemon.connections_work.begin(&daemon) else {
        log::warn!("cladefsd: refusing a VM process's connection: the daemon drains");
        return finished;
    };
    daemon.spawn(async move {
        run_connection(stream, tenant, connector).await;
        drop((connected, counted));
        // Its waiter gone: nothing to tell.
        let _ = done.send(());
    });
    finished
}

/// How many VM processes are connected to `daemon`.
pub(super) fn connections_on(daemon: &Daemon) -> usize {
    daemon.connections_work.count()
}

/// How many VM processes are connected to the process's default daemon.
#[cfg(all(test, not(madsim)))]
pub(super) fn connections() -> usize {
    connections_on(Daemon::process())
}

/// Receives the VM process's messages, serving its requests ([`receive`]), until it is gone; then,
/// once its requests are done, lets its cell go.
async fn run_connection(stream: crate::wire::Stream, tenant: Arc<Tenant>, connector: Connector) {
    let daemon = Arc::clone(&tenant.daemon);
    let sender = match stream.try_clone().and_then(|clone| {
        clone.set_nonblocking(true)?;
        // cladefs: what it queues held in its tenant's bytes in flight too (TASK-149.35).
        AsyncSender::counting(clone, tenant.in_flight().clone())
    }) {
        Ok(sender) => Arc::new(sender),
        // cladefs: a VM process gone as it connected, which macOS says making its sender (board
        // 13065): nothing to serve.
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted => {
            log::info!("cladefsd: a VM process went as it connected: {err}");
            return;
        }
        Err(err) => {
            log::error!("cladefsd: cloning a connection: {err}");
            return;
        }
    };
    // cladefs: one the daemon could not let go of, once it stopped answering, it refuses
    // (TASK-79.9).
    let peer = match Peer::connected(&daemon, &stream, connector, sender, tenant.id) {
        Ok(peer) => peer,
        Err(err) => {
            log::error!("cladefsd: refusing a VM process's connection: {err}");
            return;
        }
    };
    let (alive, gone_for_good) = tokio::sync::oneshot::channel::<std::convert::Infallible>();
    let connection = Arc::new_cyclic(|this| Connection {
        this: this.clone(),
        tenant,
        peer: peer.clone(),
        open_cell: OnceLock::new(),
        reads: OnceLock::new(),
        phase: SharedLock::new("connection phase", Mutex::new(Phase::Unopened)),
        window: SharedLock::new("connection window", Mutex::new(None)),
        behind: SharedLock::new("connection behind", Mutex::new(None)),
        shrinking: Shrinking::new(),
        transfer: OnceLock::new(),
        #[cfg(all(target_os = "linux", not(madsim)))]
        guest_stream: Mutex::default(),
        #[cfg(all(target_os = "linux", not(madsim)))]
        stream_requests: Arc::default(),
        versions_made: SharedLock::new("connection versions", Mutex::new(Default::default())),
        closing: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        asking_mappings: Arc::new(tokio::sync::Semaphore::new(1)),
        ending: event_listener::Event::new(),
        requests: SharedLock::new("connection requests", Mutex::new(Requests::default())),
        drained: Arc::new(event_listener::Event::new()),
        report: stats::Report::new(&daemon, &peer.sender),
        page_ins: tokio::sync::Semaphore::new(PAGE_INS),
        named: OnceLock::new(),
        alive: Some(alive),
    });
    match AsyncReceiver::new(stream) {
        Ok(receiver) => {
            let (gone, mut went) = tokio::sync::oneshot::channel();
            daemon.spawn(receive(connection.clone(), receiver, gone));
            // cladefs: its process gone, so is it, whoever it left the socket's end to, as a child,
            // which would hold its cell open for good (TASK-79.101). And as the daemon drains,
            // its VM told so, as at the daemon's exit, and its cell closed as a connection going
            // closes it (TASK-141.4).
            let ended = tokio::select! {
                ended = &mut went => ended,
                exited = peer.exited() => {
                    match exited {
                        Ok(()) => peer.sender.shutdown(),
                        Err(err) => log::error!("cladefsd: waiting for a VM process to exit: {err}"),
                    }
                    #[cfg(all(target_os = "linux", not(madsim)))]
                    connection.stop_guest_stream();
                    connection.end_receiving();
                    went.await
                }
                () = crate::drain::started(&daemon) => {
                    peer.sender.shutdown();
                    #[cfg(all(target_os = "linux", not(madsim)))]
                    connection.stop_guest_stream();
                    connection.end_receiving();
                    went.await
                }
            };
            // cladefs: its guest's streams shut down first, so that none of their requests cut short
            // as the connection ends is answered, its guest sending it again once it connects
            // again (board 14663); then its receiving over however it ended, which what waits for
            // room for a request hears.
            #[cfg(all(target_os = "linux", not(madsim)))]
            connection.stop_guest_stream();
            connection.end_receiving();
            // Dropped unsent if receiving failed. cladefs: a VM process past the protocol's
            // bounds the daemon lets go of (TASK-79.16).
            if ended == Ok(Ended::Broke) {
                peer.let_go(format_args!("kept more in flight than the protocol allows"));
            }
        }
        Err(err) => log::error!("cladefsd: receiving from a VM process: {err}"),
    }
    // cladefs: its guest's stream no longer accepted, nor read: the requests it took are served
    // still, and counted in its requests below (board 14663).
    #[cfg(all(target_os = "linux", not(madsim)))]
    connection.stop_guest_stream();
    // The VM process is gone, and its mappings of the window with it: nothing waits for it.
    // cladefs: one the daemon let go, or that still reaches pages, once it has exited (TASK-79.9,
    // TASK-79.100).
    connection.closing.store(true, Ordering::Release);
    peer.gone(connection.reaches_pages()).await;
    peer.acks.close();
    // A resize waiting for its guest's answer, which none will give, stops waiting, and holds the
    // connection no more (TASK-79.34).
    connection.shrinking.stop();
    // Its requests received are served still, as they were before it went.
    loop {
        let drained = connection.drained.listen();
        if connection.requests.with(Mutex::lock).is_done() {
            break;
        }
        drained.await;
    }
    // What it committed is durable before another connection may open its cell.
    let behind = connection.behind.with(Mutex::lock).take();
    if let Some((stop, behind)) = behind {
        drop(stop);
        // Panicked: the panic halted its cell as it went through the cell's locks (crate::halt),
        // whose next opener has it as of its last commit.
        let _ = behind.await;
    }
    // cladefs: and in the bucket, as the daemon drains, the open segment's commits too, which a
    // cell closed leaves for the next to open it to upload (TASK-141.4).
    if crate::drain::draining(&daemon)
        && let (Some(_), Some(open_cell)) = (connection.hold(), connection.cell())
        && let Err(errno) = cells::uploaded(open_cell).await
    {
        log::error!("cladefsd: uploading a cell's last commits as the daemon drains: {errno}");
    }
    finish_connection(connection).await;
    // Its cell goes, with its window and open extents, before another connection may open it:
    // once nothing holds the connection any more, as a request's job, its receiving task, or the
    // control API's work through it may still, whose last to let go closes its cell and then
    // lets its path go (TASK-54, TASK-79.34).
    let _ = gone_for_good.await;
    peer.forget();
}

/// How a connection's receiving ended ([`receive`]).
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// Its VM process closed it, or receiving failed.
    Closed,
    /// cladefs: its VM process kept more in flight than the protocol allows (TASK-79.16).
    Broke,
}

/// Receives `connection`'s messages from `receiver` until its VM process is gone, and says how
/// (`gone`). A request it may serve at once ([`Connection::admit`]) it serves on the thread that
/// received it, out of the runtime's ([`local_cell::upload::blocking`]), while a task of its own
/// receives on: no thread is woken between a request's arrival and its serving, but the next
/// receiver, meanwhile (TASK-41).
fn receive(
    connection: Arc<Connection>,
    mut receiver: AsyncReceiver,
    gone: tokio::sync::oneshot::Sender<Ended>,
) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        let mut ended = Ended::Closed;
        loop {
            match receiver.receive(|header| connection.clone().within_bounds(*header)).await {
                // cladefs: what the stage operations cost, counted before what waits for them goes
                // on (TASK-91.16). What says so malformed, nothing replies to: it is dropped, as
                // what isn't said (TASK-160.9).
                Ok(Some((message, _))) if [ACK, STAGE_COUNTS].contains(&message.header.kind) => {
                    match Made::decode(&message) {
                        Ok(Made::Ack { id, counts }) => {
                            connection.report.stage_counted(counts);
                            // cladefs: of an operation never sent, which would have every one
                            // sent after taken for made, its pages freed while it may map them
                            // and the watchdog never finding it silent: a breach (board 8463).
                            if let Err(sent) = connection.peer.acks.acked(id) {
                                connection.peer.let_go(format_args!(
                                    "acknowledged stage operation {id}, the last sent it {sent}"
                                ));
                                break;
                            }
                        }
                        Ok(Made::Counts { stage, private }) => {
                            connection.report.stage_counted(stage);
                            connection.report.private_counted(private);
                        }
                        Err(errno) => log::warn!(
                            "cladefsd: a VM process's malformed {}, dropped ({errno:?}): {:?}",
                            match message.header.kind {
                                ACK => "ACK",
                                _ => "STAGE_COUNTS",
                            },
                            message.header,
                        ),
                    }
                }
                Ok(Some((message, charge))) => {
                    let Some(work) = connection.admit(Work::Message(message, charge)) else {
                        continue;
                    };
                    connection.daemon().spawn(receive(connection.clone(), receiver, gone));
                    // The task does nothing more: its receiving goes on in another. Inline under
                    // madsim, which has one thread (`upload::blocking`, doc-24 step 11).
                    local_cell::upload::blocking(move || connection.serve_in_turn(work));
                    return;
                }
                Ok(None) => break,
                Err(err) => {
                    log::error!("cladefsd: receiving from a VM process: {err}");
                    if err.kind() == std::io::ErrorKind::QuotaExceeded {
                        ended = Ended::Broke;
                    }
                    break;
                }
            }
        }
        // Let go of first, as its going waits for the last to (TASK-79.34).
        drop(connection);
        // Its waiter gone: nothing to tell.
        let _ = gone.send(ended);
    })
}

/// cladefs: what of what a VM process sends past what it was answered the daemon holds, besides its
/// requests': what it queued for the VM process that is no reply, as stage operations and commits
/// it is told of, which the protocol's bound on stream data allows for (TASK-79.16). And, its
/// window's, a reply to `MAPPINGS` (TASK-79.43), as many mappings as the window may hold, which
/// [`Peer::credit`] allows for besides.
pub(super) const QUEUED_CREDIT: u64 = 16 << 20;

impl Connection {
    /// cladefs: whether its VM process, should it live on, reaches pages still: those its window
    /// has mapped, or those of an operation taking access away it left unacknowledged
    /// (TASK-79.100).
    fn reaches_pages(&self) -> bool {
        // cladefs: a halted cell's window, which a panic may have left half changed, isn't looked
        // at: it may, as far as anyone knows (TASK-153.16).
        if self.cell().is_some_and(|open_cell| open_cell.is_halted()) {
            return true;
        }
        let window = self.window.with(Mutex::lock);
        self.peer.acks.unacknowledged() || window.as_ref().is_some_and(DaxWindow::maps_pages)
    }
}

/// Lets the cell of `connection`, whose VM process is gone and whose requests are done, go: once
/// what it committed is durable, if it writes it.
async fn finish_connection(connection: Arc<Connection>) {
    if let (Some(_), Some(open_cell)) = (connection.hold(), connection.cell()) {
        // cladefs: its disk work off the runtime's threads (doc-24, step 9).
        if let Err(err) = local_cell::upload::blocking(|| open_cell.flush()).await {
            log::error!("cladefsd: making a cell's last commits durable: {err:?}");
        }
    }
    // cladefs: the trace of its guest's reads, if it keeps one, its requests done (TASK-702.1).
    if let Some(trace) = connection.reads.get() {
        let (file, text) = (trace.file().to_owned(), trace.text());
        // cladefs: outside the simulation: a diagnostic, as the stats file is, not the host's disk
        // its cells are kept on.
        let failed = match crate::run::off_the_runtime(move || std::fs::write(&file, text)).await {
            Ok(Ok(())) => None,
            Ok(Err(err)) => Some(err.to_string()),
            Err(panicked) => Some(panicked),
        };
        if let Some(err) = failed {
            log::warn!("cladefsd: writing the read trace {}: {err}", trace.file().display());
        }
    }
    connection.peer.sender.shutdown();
    drop(connection);
}

/// A connection's requests: those waiting, and how many run (TASK-34); cladefs: how many wait for
/// what they are served on, deferred, their turns over (TASK-92.20), and the bytes of stream data
/// of those received whose replies are not queued yet ([`Answer`], TASK-79.16).
#[derive(Default)]
struct Requests {
    waiting: std::collections::VecDeque<Work>,
    running: usize,
    deferred: usize,
    stream_bytes: u64,
    /// cladefs: whether its guest's streams stopped, after which none of their requests is
    /// admitted, so that none reaches the cell once the connection drains (board 14663).
    streams_stopped: bool,
}

impl Requests {
    /// Whether none waits, runs or is deferred.
    fn is_done(&self) -> bool {
        self.running == 0 && self.deferred == 0 && self.waiting.is_empty()
    }

    /// How many were received and not answered.
    fn in_flight(&self) -> usize {
        self.running + self.deferred + self.waiting.len()
    }
}

/// A request a connection serves in its turns: a message its socket received, with what the daemon
/// holds for it; or, cladefs, a guest's request its stream received (board 14663).
enum Work {
    Message(Message, Charge),
    #[cfg(all(target_os = "linux", not(madsim)))]
    Stream(stream::Received),
}

/// cladefs: what the daemon holds for a request, as its connection's room and its tenant's bytes in
/// flight count it ([`Connection::within_bounds`]): counted no more once dropped, however the
/// request goes: answered ([`Answer`]), refused, or never read whole, its connection lost
/// partway through its payload, which made no request to answer (TASK-548).
struct Charge {
    connection: Weak<Connection>,
    tenant: Arc<Tenant>,
    bytes: u64,
    /// cladefs: a `MAPPINGS`'s turn, the connection's one, until its reply is queued (TASK-546).
    _mappings: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Charge {
    /// Of nothing: a request that holds nothing besides its header.
    fn none(connection: &Connection) -> Self {
        Self {
            connection: connection.this.clone(),
            tenant: connection.tenant.clone(),
            bytes: 0,
            _mappings: None,
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        // Its connection's room, while it is there to count it.
        if let Some(connection) = self.connection.upgrade() {
            connection.requests.with(Mutex::lock).stream_bytes -= self.bytes;
        }
        self.tenant.in_flight().sub(self.bytes);
    }
}

/// A connection's turn at serving a request ([`Connection::serve_in_turn`]), which ends, the
/// request served or its serving panicking, with the connection's next ([`Connection::next_turn`]).
/// cladefs: or, its request deferred, its serving waiting for what it is served on, which the
/// connection's next is served beside, ends as it is answered at last (TASK-92.20).
struct Turn {
    connection: Option<Arc<Connection>>,
    deferred: bool,
}

impl Turn {
    fn new(connection: Arc<Connection>) -> Self {
        Self { connection: Some(connection), deferred: false }
    }

    fn connection(&self) -> &Arc<Connection> {
        self.connection.as_ref().expect("a turn holds its connection until it ends")
    }

    /// cladefs: defers its request, which waits for what it is served on (TASK-92.20): the
    /// connection's next request waiting, if any, is served in its place, and it is counted in
    /// flight until the turn ends.
    fn defer(&mut self) {
        let connection = self.connection().clone();
        connection.requests.with(Mutex::lock).deferred += 1;
        connection.next_turn();
        self.deferred = true;
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else { return };
        match self.deferred {
            false => connection.next_turn(),
            true => connection.answered_deferred(),
        }
    }
}

/// A request's answer, which it is owed until sent ([`Answer::send`]): its reply, or its error.
/// cladefs: its bytes of stream data counted in its connection's requests until its reply is
/// queued, and not after, as its VM process's credit counts it then ([`Peer::credit`]), rather than
/// both for a while, which held a VM process keeping to the protocol's bounds past them
/// (TASK-79.16); and, its serving panicking, it answers `EIO` as it is dropped, rather than leave
/// its VM process waiting for good, those bytes counted for good; and halts its cell, which the
/// panic went through: past the cell's locks, which halt it themselves (`crate::halt`,
/// TASK-153.16). cladefs: and the request's span, which ends as it is answered, before its reply is
/// sent (`super::otel`, TASK-258).
struct Answer {
    connection: Arc<Connection>,
    request: Header,
    started: std::time::Instant,
    sent: bool,
    span: trace::Span,
    /// cladefs: what the daemon holds for the request, counted until its reply is queued.
    charge: Option<Charge>,
}

impl Answer {
    /// The answer owed to `request`, which `connection` received, a call for the guest request
    /// `guest`, if any.
    fn new(
        connection: &Arc<Connection>,
        request: Header,
        guest: Option<&cladefs_core::request::RequestId>,
        charge: Option<Charge>,
    ) -> Self {
        let started = std::time::Instant::now();
        let cell = connection.cell().map(|open_cell| open_cell.cell());
        let span = crate::otel::request_span(&request, cell, guest);
        Self { connection: connection.clone(), request, started, sent: false, span, charge }
    }

    /// The header of its reply, before what the reply says.
    fn reply_header(&self) -> Header {
        Header::new(REPLY, self.request.id)
    }

    /// Replies with `reply`, `payload` and `fds`, if `result` says the request succeeded, or its
    /// error.
    fn send(
        mut self,
        result: Result<(), Errno>,
        reply: Header,
        payload: Vec<u8>,
        fds: Vec<std::os::fd::BorrowedFd<'_>>,
    ) {
        self.reply(result, reply, payload, fds);
    }

    fn reply(
        &mut self,
        result: Result<(), Errno>,
        mut reply: Header,
        mut payload: Vec<u8>,
        mut fds: Vec<std::os::fd::BorrowedFd<'_>>,
    ) {
        self.sent = true;
        let header = self.request;
        if let Err(errno) = result {
            // A full cell: which request found it so, and how full: of its extents' bytes, or
            // of the host's disk, which it may fill far from its capacity (TASK-79.105). cladefs:
            // not its tenant's disk at its limit, which says so itself (TASK-94).
            let tenant_full = self.connection.tenant.disk().is_some_and(|disk| disk.is_full());
            if errno == Errno::ENOSPC
                && !tenant_full
                && let Some(open_cell) = self.connection.cell()
            {
                let used = open_cell.info().map(|info| (info.used, info.capacity));
                let disk = open_cell.disk_bytes();
                log::warn!(
                    "cladefsd: request {} (flags {:#x}, body {:#x?}) found the cell full: \
                     used and capacity {used:?}, {disk} bytes of the host's disk",
                    header.kind,
                    header.flags,
                    header.body
                );
            }
            // A request the cell refuses as malformed is the guest's mistake: which it was.
            if errno == Errno::EINVAL {
                log::warn!(
                    "cladefsd: request {} refused with EINVAL: flags {:#x}, body {:#x?}",
                    header.kind,
                    header.flags,
                    header.body
                );
            }
            reply.status = -errno.raw();
            payload.clear();
            fds.clear();
            self.span.set(crate::otel::ERRNO, errno.raw() as i64);
            self.span.fail(errno);
        }
        // Counted before its reply, which its VM process may then count on.
        crate::metrics::served(header.kind, self.started.elapsed(), reply.status != 0);
        // End the span before replying so it is ready to export.
        drop(std::mem::replace(&mut self.span, trace::Span::none()));
        // cladefs: and its tenant's, which its reply's bytes are counted in as they're queued.
        drop(self.charge.take());
        if let Err(err) = self.connection.peer.sender.send(reply, payload, &fds) {
            log::warn!("cladefsd: replying to a VM process: {err}");
        }
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(open_cell) = self.connection.cell()
        {
            let kind = self.request.kind;
            open_cell.halt(|| format!("a panic serving request {kind}"));
        }
        if !self.sent {
            let reply = self.reply_header();
            self.reply(Err(Errno::EIO), reply, Vec::new(), Vec::new());
        }
    }
}

/// cladefs: says what `request` asks for on `span`, its span: the bytes it reads, writes or maps,
/// and the extents it frees or maps (TASK-258).
fn traced(span: &trace::Span, request: &Request<'_>) {
    if !span.is_on() {
        return;
    }
    let (bytes, extents) = match request {
        Request::Read { len, .. } => (Some(*len), None),
        Request::Write { data: Data::Inline(data), .. } => (Some(data.len() as u64), None),
        Request::Write { data: Data::Transfer { len, .. }, .. } => (Some(*len), None),
        Request::Map { mappings, .. } => {
            // Each within a window, as decoding has them: summed so that no count of them wraps
            // (board 2776).
            let bytes = mappings.iter().fold(0u64, |sum, mapping| sum.saturating_add(mapping.len));
            (Some(bytes), Some(mappings.len() as u64))
        }
        Request::Unmap { len, .. } => (Some(*len), None),
        Request::Free { rest, .. } => (None, Some(1 + rest.len() as u64)),
        Request::Commit { commit, .. } => {
            span.set(trace::LEVEL, commit.level());
            (None, None)
        }
        Request::Open { path, .. } => {
            span.set_with(trace::STORE, || path.display().to_string());
            (None, None)
        }
        _ => (None, None),
    };
    if let Some(bytes) = bytes {
        span.set_count(trace::BYTES, bytes);
    }
    if let Some(extents) = extents {
        span.set_count(trace::EXTENTS, extents);
    }
}

/// cladefs: serves a request admitted, in `turn`, as `serving` serves it, a future, which answers
/// it (TASK-92.20): on this thread until it first waits, in its turn; then on the [`Pool`], polled
/// there as it is woken, deferred meanwhile ([`Turn::defer`]). So a request waiting for what it is
/// served on, as a fault for the pager's fetch from object storage, holds neither a thread nor one
/// of the connection's [`WORKERS`], which its next requests are served by. It is polled where
/// blocking is allowed, a pool's thread or one out of the runtime's (`receive`), the runtime's
/// timers and tasks at hand.
fn serve_future(turn: Turn, serving: impl std::future::Future<Output = ()> + Send + 'static) {
    #[cfg(not(madsim))]
    let daemon = Arc::clone(turn.connection().daemon());
    let turn = Arc::new(Mutex::new(Some(turn)));
    let answered = turn.clone();
    let future = async move {
        serving.await;
        let ended = answered.lock().unwrap().take();
        drop(ended);
    };
    // cladefs: under madsim, polled here first, in its turn, as on the pool; then, if it waits, a
    // task of the simulated host's, which polls it again at once: its wakers the simulation's, no
    // pool's to be found from wherever they fire (doc-24).
    #[cfg(madsim)]
    {
        struct MayBlock<F>(std::pin::Pin<Box<F>>);
        impl<F: std::future::Future> std::future::Future for MayBlock<F> {
            type Output = F::Output;
            fn poll(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                host_fs::may_block(|| self.0.as_mut().poll(cx))
            }
        }
        let mut future = MayBlock(Box::pin(future));
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        if std::pin::Pin::new(&mut future).poll(&mut context).is_pending() {
            drop(tokio::spawn(future));
        }
    }
    #[cfg(not(madsim))]
    {
        let poll = move |daemon: &Daemon, runnable: async_task::Runnable| {
            let _io = daemon.runtime().enter();
            // cladefs: runs on `Pool` (dedicated blocking threads, not tokio's workers), entering
            // `daemon.runtime()` only for timers and spawns (board 10824).
            host_fs::may_block(|| runnable.run());
        };
        let scheduled = Arc::clone(&daemon);
        let (runnable, task) = async_task::spawn(future, move |runnable| {
            let running = Arc::clone(&scheduled);
            scheduled.pool().run(Box::new(move || poll(&running, runnable)));
        });
        task.detach();
        poll(&daemon, runnable);
    }
    // Unless answered already.
    if let Some(turn) = turn.lock().unwrap().as_mut() {
        turn.defer();
    }
}

impl Writer for Connection {
    fn going(&self) -> bool {
        self.closing.load(Ordering::Acquire) || self.peer.sender.peer_gone()
    }
}

impl cells::CellWriter for Connection {
    fn cell(&self) -> Option<&Arc<OpenCell>> {
        Connection::cell(self)
    }

    fn resize(
        &self,
        capacity: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Errno>> + Send + '_>> {
        Box::pin(Connection::resize(self, capacity))
    }

    #[cfg(test)]
    fn connection(self: Arc<Self>) -> Option<Arc<Connection>> {
        Some(self)
    }
}

/// The directories `daemon` serves cells in to the default tenant's connections,
/// `CLADEFSD_ROOTS` (separated by colons), if it names any (TASK-43.4), as [`resolve_roots_on`]
/// resolved them, once. cladefs: a daemon refuses to start with one it can't resolve (board 8595);
/// served without one booted, as in a test, it refuses every path then, rather than serve beyond
/// the roots named.
fn roots(daemon: &Daemon) -> Option<Arc<[PathBuf]>> {
    // Unresolved, said as it was resolved: no root, every path refused.
    resolve_roots_on(daemon).unwrap_or_else(|_| Some(Arc::from([])))
}

/// cladefs: the directories `CLADEFSD_ROOTS` names, each without symbolic links, if it names any;
/// or why one is no directory, `daemon`'s boot's refusal to start, as a root mistyped, dropped,
/// refused every VM's path while the daemon served (board 8595). Resolved once, the first time it
/// is asked: at the boot, before the daemon serves.
pub(super) fn resolve_roots_on(daemon: &Daemon) -> Result<Option<Arc<[PathBuf]>>, String> {
    let resolved = daemon.roots.get_or_init(|| {
        let Some(roots) =
            node_local::env::var_os("CLADEFSD_ROOTS").filter(|roots| !roots.is_empty())
        else {
            return Ok(None);
        };
        let root = |root: PathBuf| {
            let resolved = host_fs::canonicalize(&root)
                .and_then(|resolved| Ok((host_fs::metadata(&resolved)?.is_dir(), resolved)));
            match resolved {
                Ok((true, resolved)) => Ok(resolved),
                Ok((false, _)) => {
                    Err(format!("CLADEFSD_ROOTS: {} is no directory", root.display()))
                }
                Err(err) => Err(format!("CLADEFSD_ROOTS: {}: {err}", root.display())),
            }
        };
        let roots: Result<Vec<PathBuf>, String> = std::env::split_paths(&roots).map(root).collect();
        roots.map(|roots| Some(Arc::from(roots))).inspect_err(|err| log::error!("cladefsd: {err}"))
    });
    resolved.clone()
}

/// Whether `path` names a cell in `dir`, a canonical directory, whichever way it names it:
/// through symlinks, or `..`.
pub(super) fn in_dir(path: &Path, dir: &Path) -> bool {
    key_in_dir(&cell_dir_key(path), dir)
}

/// Membership for an already resolved cell key.
pub(super) fn key_in_dir(key: &Path, dir: &Path) -> bool {
    use std::path::Component;
    // A path not resolved, its directory not there, holds no `..` that leads out.
    let plain =
        key.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
    plain && key.starts_with(dir) && key != dir
}

/// How long a shrink waits for the guest's answer.
const SHRINK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What works behind a connection's cell, a task, and what stops it, dropped.
type Behind = (tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>);

/// A connection's cell and window.
pub(super) struct Connection {
    this: Weak<Connection>,
    /// cladefs: the tenant it is of (TASK-43).
    tenant: Arc<Tenant>,
    /// Its VM process, which it sends to: cladefs: as the daemon depends on it (TASK-79.9).
    peer: Arc<Peer>,
    /// Its cell, once open: set once, as its `OPEN` is, before its phase says how it has it;
    /// read by its every request, without a lock.
    open_cell: OnceLock<Arc<OpenCell>>,
    /// Where it is in its life: whether it has its cell, and how (TASK-160.8). Its locks are
    /// its requests', its teardown's and its window's, beyond what halts its cell: a panic
    /// holding one aborts the daemon (`local_cell::shared`, TASK-149.24).
    phase: SharedLock<Mutex<Phase>>,
    window: SharedLock<Mutex<Option<DaxWindow>>>,
    /// What works behind the cell, a task, and what stops it, dropped: making a writer's commits
    /// durable behind them.
    behind: SharedLock<Mutex<Option<Behind>>>,
    /// The shrink the guest was asked to take, and where its answer goes ([`RESIZED`]).
    shrinking: Shrinking,
    /// Its transfer area, once its VM process gives it one ([`TRANSFER`]).
    transfer: OnceLock<TransferArea>,
    /// cladefs: its guest's stream, once its VM process gives its listener ([`GUEST_STREAM`],
    /// board 14663), until stopped as the connection ends.
    #[cfg(all(target_os = "linux", not(madsim)))]
    guest_stream: Mutex<stream::GuestStream>,
    /// cladefs: its guest's streams' requests in flight, from their reading to their replies'
    /// writing, which its VM process's own bound, [`MAX_REQUESTS`], leaves out: each stream bounds
    /// its own (board 14663).
    #[cfg(all(target_os = "linux", not(madsim)))]
    stream_requests: Arc<std::sync::atomic::AtomicUsize>,
    /// cladefs: the versions its VM process made ([`FORK_QUIESCENT`]), which alone it drops
    /// ([`FORK_DROP`], TASK-575).
    versions_made: SharedLock<Mutex<std::collections::BTreeSet<u64>>>,
    /// Whether its VM process is gone, and its cell goes once durable.
    closing: AtomicBool,
    /// cladefs: whether its receiving ended, and what says so ([`Connection::end_receiving`]).
    ended: AtomicBool,
    /// cladefs: its turn at `MAPPINGS`, one at a time ([`Connection::mappings_turn`]).
    asking_mappings: Arc<tokio::sync::Semaphore>,
    ending: event_listener::Event,
    /// Its requests received and not done (TASK-34), and what says when none is left.
    requests: SharedLock<Mutex<Requests>>,
    drained: Arc<event_listener::Event>,
    /// cladefs: what the daemon reports of its VM process (TASK-111.4): its cell, its build, what
    /// it says of its window's second stage, and its answers to asks for its stats.
    report: Arc<stats::Report>,
    /// cladefs: its faults, `PRIVATE`s and `READ`s resolving, [`PAGE_INS`] at most (TASK-149.34).
    page_ins: tokio::sync::Semaphore,
    /// cladefs: its cell's path, once open, which names the guest requests it serves calls for
    /// (`crate::request::RequestId::store`, TASK-260).
    named: OnceLock<Arc<str>>,
    /// cladefs: the trace of its guest's reads, from its cell's opening, if the daemon writes
    /// traces (`CLADEFSD_READ_TRACES`, TASK-702.1): written as it goes ([`finish_connection`]).
    reads: OnceLock<crate::reads::Trace>,
    /// What says, as it drops, that the connection has: dropped after its cell, window and open
    /// extents, and its hold of its cell's path (TASK-79.34), by its teardown ([`Connection`]'s
    /// drop).
    alive: Option<tokio::sync::oneshot::Sender<std::convert::Infallible>>,
}

/// Where a connection is in its life ([`Connection::phase`]): one `OPEN` at a time, and one cell
/// ([`Connection::cell`]), which it writes, holding its path (TASK-150.5, TASK-160.8).
enum Phase {
    /// Its VM process opened no cell yet.
    Unopened,
    /// cladefs: an [`OPEN`] in progress, the only one ([`Opening`]).
    Opening,
    /// It writes its cell, which it holds in the daemon's table (`cells::table`) until it is
    /// dropped, after its cell and window ([`Connection`]'s drop). Shared only with what makes
    /// the cell's last commits durable as the connection goes ([`finish_connection`]), which holds
    /// the connection.
    Writing(Arc<cells::Hold>),
}

/// cladefs: an [`OPEN`] in progress on a connection ([`Connection::opening`]), the only one, until
/// dropped; only it gives the connection its cell, which it has once: its phase goes back to
/// [`Phase::Unopened`] unless it opened one.
struct Opening<'a>(&'a Connection);

impl Opening<'_> {
    /// The connection has `open_cell`, at `path`, from now on, writing it under `hold`
    /// ([`Phase::Writing`]). Checked without the phase's lock, which a panic would poison.
    fn opened(self, path: &Path, open_cell: Arc<OpenCell>, hold: cells::Hold) {
        // cladefs: its guest's reads traced from now, if the daemon traces them (TASK-702.1).
        if let Some(trace) = crate::reads::Trace::opened(self.0.daemon(), open_cell.cell().id) {
            drop(self.0.reads.set(trace));
        }
        let key = cell_dir_key(path);
        drop(self.0.named.set(key.to_string_lossy().into()));
        self.0.report.opened(key, open_cell.cell());
        assert!(self.0.open_cell.set(open_cell).is_ok(), "a connection opened a second cell");
        let opened = Phase::Writing(Arc::new(hold));
        let was = std::mem::replace(&mut *self.0.phase.with(Mutex::lock), opened);
        assert!(matches!(was, Phase::Opening), "a connection opened a cell not opening one");
    }
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        let mut phase = self.0.phase.with(Mutex::lock);
        if matches!(*phase, Phase::Opening) {
            *phase = Phase::Unopened;
        }
    }
}

impl Drop for Connection {
    /// Its cell goes, then its window, as they did before (TASK-79.34), and only then its hold of
    /// its cell's path, which those waiting to open it may then (TASK-54): all it has open is
    /// closed by then. Then what says it is gone. cladefs: in a task of its own, its cell let go
    /// awaited, closed if this was its last (`open_cell::release`): whatever drops the connection
    /// last, a request, its receiving or the control API, closes no cell in its drop, which blocks
    /// (doc-24, step 9). Should the daemon's work through it, a fork's or a sync's, have the cell
    /// still, the cell is closing (`cell_table::Open::Closing`), which the next writer waits for,
    /// told as it closes (`cells::Closed`, board 8237).
    ///
    /// A lock a panic poisoned is taken all the same: what it guards is whole, each changed at
    /// once, and what it holds must go, in this order, whatever panicked.
    fn drop(&mut self) {
        let open_cell = self.open_cell.take();
        let window = self.window.get_mut().get_mut().unwrap_or_else(PoisonError::into_inner).take();
        let phase = self.phase.get_mut().get_mut().unwrap_or_else(PoisonError::into_inner);
        let phase = std::mem::replace(phase, Phase::Unopened);
        let alive = self.alive.take();
        let closed = match &phase {
            Phase::Writing(hold) => Some((self.tenant.daemon.table.clone(), hold.key().to_owned())),
            Phase::Unopened | Phase::Opening => None,
        };
        let closing = open_cell.and_then(|open_cell| {
            if let Some((table, key)) = closed {
                open_cell.on_close(move || table.closed(&key));
            }
            let cell = Arc::into_inner(open_cell)?;
            cell.mark_closing();
            Some(cell)
        });
        drop(self.tenant.daemon.spawn(async move {
            if let Some(cell) = closing {
                cell.close().await;
            }
            drop(window);
            drop(phase);
            drop(alive);
        }));
    }
}

/// cladefs: refuses the [`OPEN`] of `header`, of a VM process of another wire version, its
/// connection opening nothing (TASK-141.3): logged, naming both versions, and replied to with
/// `EPROTONOSUPPORT` and the daemon's version.
fn refuse_version(header: &Header, answer: Answer) {
    log::error!(
        "cladefsd: refusing a VM process of wire version {}: this daemon's is {VERSION}, and a VM \
         process and a daemon of different wire formats don't serve each other",
        codec::version(header)
    );
    let reply = codec::with_version(answer.reply_header());
    answer.send(Err(Errno::EPROTONOSUPPORT), reply, Vec::new(), Vec::new());
}

/// cladefs: the failpoint of requests of `kind`, if they have one (TASK-44.10).
#[cfg(feature = "failpoints")]
pub(super) fn failpoint(kind: u32) -> Option<&'static str> {
    Some(match kind {
        READ => "cladefsd::read",
        WRITE => "cladefsd::write",
        MAP => "cladefsd::map",
        UNMAP => "cladefsd::unmap",
        COMMIT => "cladefsd::commit",
        FREE => "cladefsd::free",
        FORK => "cladefsd::fork",
        _ => return None,
    })
}

impl Connection {
    /// The daemon serving this connection.
    fn daemon(&self) -> &Arc<Daemon> {
        &self.tenant.daemon
    }

    /// Its cell, once it has opened one.
    pub(super) fn cell(&self) -> Option<&Arc<OpenCell>> {
        self.open_cell.get()
    }

    /// Its hold of its cell, if it writes it.
    fn hold(&self) -> Option<Arc<cells::Hold>> {
        match &*self.phase.with(Mutex::lock) {
            Phase::Writing(hold) => Some(hold.clone()),
            Phase::Unopened | Phase::Opening => None,
        }
    }

    /// The arena of physical memory its cell's own pages are of: its tenant's.
    fn arena(&self) -> &Arc<cladefs_core::pages::Arena> {
        &self.tenant.memory.arena
    }

    /// cladefs: the arena's ID ([`Connection::arena`]).
    pub(super) fn arena_id(&self) -> cladefs_core::pages::ArenaId {
        self.arena().id()
    }

    /// Runs `work` behind its cell, a task on the daemon's runtime, until it is told to stop: the
    /// sender of the receiver it is given dropped.
    fn start_behind<F: std::future::Future<Output = ()> + Send + 'static>(
        &self,
        work: impl FnOnce(tokio::sync::oneshot::Receiver<()>) -> F,
    ) {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = self.daemon().spawn(work(stopped));
        *self.behind.with(Mutex::lock) = Some((stop, task));
    }

    /// cladefs: admits a message of `header`, before its payload is read, as the protocol's bounds
    /// allow what its VM process keeps in flight (TASK-79.16): its requests not answered
    /// ([`MAX_REQUESTS`]), and their stream data ([`MAX_STREAM_BYTES`]), with what the connection
    /// has queued for the VM process, within its credit ([`Peer::credit`]). Counted in, a
    /// request's, until its reply is queued ([`Answer`]). Past them, `QuotaExceeded`: the VM
    /// process broke the protocol, and is let go, rather than held without end in the daemon's
    /// memory. cladefs: and its stream data in the bytes in flight its tenant's connections share,
    /// waiting, before its payload is read, until what they hold leaves enough for it (TASK-149.35,
    /// [`super::memory::InFlight`]).
    async fn within_bounds(self: Arc<Self>, header: Header) -> std::io::Result<Charge> {
        // cladefs: no payload past what its kind holds, refused at its header, before any byte of
        // it is held or reserved (board 8459, TASK-581): none for a kind no request is of.
        let most = codec::most_payload(&header);
        if header.len > most {
            let what = format!(
                "a payload of {} bytes on a {}, of {most} at most",
                header.len, header.kind
            );
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, what));
        }
        // cladefs: nor what its stage operations cost, which it says unasked (TASK-91.16).
        if [ACK, STAGE_COUNTS].contains(&header.kind) {
            return Ok(Charge::none(&self));
        }
        // cladefs: one `MAPPINGS` at a time, the next waiting for the last's reply, as the credit
        // holds one reply of mappings (TASK-546): asked for at once, each of a window's most is
        // past the credit with another.
        let mappings = match header.kind {
            MAPPINGS => Some(self.mappings_turn().await?),
            _ => None,
        };
        let bytes = self.held_for(&header);
        {
            let requests = self.requests.with(Mutex::lock);
            let in_flight = requests.in_flight().saturating_sub(self.stream_requests());
            let held = requests.stream_bytes + bytes + self.peer.sender.queued();
            if in_flight >= MAX_REQUESTS || held > self.peer.credit() {
                let what = format!("{} requests, and {held} bytes, in flight", in_flight + 1);
                return Err(std::io::Error::new(std::io::ErrorKind::QuotaExceeded, what));
            }
        }
        // Admitted one at a time, as its messages are received, so nothing came in meanwhile.
        if bytes > 0 {
            self.take_in_flight(bytes).await?;
        }
        self.requests.with(Mutex::lock).stream_bytes += bytes;
        let (connection, tenant) = (self.this.clone(), self.tenant.clone());
        Ok(Charge { connection, tenant, bytes, _mappings: mappings })
    }

    /// cladefs: the connection's turn at a `MAPPINGS`, once the last one's reply is queued; or
    /// fails, the connection ending meanwhile ([`Connection::end_receiving`]).
    async fn mappings_turn(&self) -> std::io::Result<tokio::sync::OwnedSemaphorePermit> {
        let ending = self.ending.listen();
        if !self.ended.load(Ordering::Acquire) {
            tokio::select! {
                turn = self.asking_mappings.clone().acquire_owned() => {
                    return Ok(turn.expect("a connection's MAPPINGS turns are never closed"));
                }
                () = ending => {}
            }
        }
        let what = "the connection ended waiting for its turn at MAPPINGS";
        Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, what))
    }

    /// cladefs: what the daemon holds for a request of `header`: its stream data
    /// ([`stream_bytes`]); and a `MAPPINGS`'s reply, which holds as many mappings as its window
    /// may (TASK-546), each counted as it is admitted, not once it is made, when it would be held
    /// whatever.
    fn held_for(&self, header: &Header) -> u64 {
        match header.kind {
            MAPPINGS => stream_bytes(header) + self.peer.mappings.load(Ordering::Relaxed),
            _ => stream_bytes(header),
        }
    }

    /// cladefs: holds `bytes` of its tenant's bytes in flight once they leave enough room
    /// ([`super::memory::InFlight::take`]); or fails, the connection ending meanwhile, its VM
    /// process gone or the daemon draining, as a receive waiting for room ends with it
    /// (TASK-548).
    async fn take_in_flight(&self, bytes: u64) -> std::io::Result<()> {
        let ending = self.ending.listen();
        if !self.ended.load(Ordering::Acquire) {
            tokio::select! {
                () = self.tenant.in_flight().take(bytes) => return Ok(()),
                () = ending => {}
            }
        }
        let what = "the connection ended waiting for its tenant's bytes in flight";
        Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, what))
    }

    /// cladefs: ends its receiving: one waiting for room in its tenant's bytes in flight fails
    /// ([`Connection::take_in_flight`]), as its VM process went or the daemon drains.
    /// cladefs: its guest's streams' requests in flight, which its VM process's own bound leaves
    /// out (board 14663): none where it has no streams.
    fn stream_requests(&self) -> usize {
        #[cfg(all(target_os = "linux", not(madsim)))]
        return self.stream_requests.load(Ordering::Relaxed);
        #[cfg(not(all(target_os = "linux", not(madsim))))]
        0
    }

    fn end_receiving(&self) {
        self.ended.store(true, Ordering::Release);
        self.ending.notify(usize::MAX);
    }

    /// `message`, a request, with what the daemon holds for it counted, to serve now, unless
    /// [`WORKERS`] of the connection's run, when it waits for one of them to be done, which serves
    /// it then ([`Connection::serve_in_turn`]).
    fn admit(&self, work: Work) -> Option<Work> {
        let mut requests = self.requests.with(Mutex::lock);
        // cladefs: a stream's request read as its streams stopped, answered with nothing written
        // as it drops, outside the lock (board 14663).
        #[cfg(all(target_os = "linux", not(madsim)))]
        if requests.streams_stopped && matches!(work, Work::Stream(_)) {
            drop(requests);
            drop(work);
            return None;
        }
        if requests.running == WORKERS {
            requests.waiting.push_back(work);
            return None;
        }
        requests.running += 1;
        Some(work)
    }

    /// Serves `work`, admitted ([`Connection::admit`]): a message
    /// ([`Connection::serve_message_in_turn`]) or, cladefs, a guest's request its stream received
    /// (board 14663).
    fn serve_in_turn(self: Arc<Self>, work: Work) {
        match work {
            Work::Message(message, charge) => self.serve_message_in_turn(message, charge),
            #[cfg(all(target_os = "linux", not(madsim)))]
            Work::Stream(received) => self.serve_stream_in_turn(received),
        }
    }

    /// Serves `message`, admitted ([`Connection::admit`]); then the next waiting, if any, on the
    /// [`Pool`], in turn with every other connection's: whether serving it returned or panicked,
    /// which would otherwise leave it counted running for good (TASK-79.1). cladefs: a fault, and a
    /// writer's opening, are futures, deferred as they wait ([`serve_future`]): for the pager
    /// (TASK-92.20), and for a connection going to let the cell go, or a restore's version
    /// (TASK-92.5). A request its kind gives no meaning ([`Request::decode`]), or naming a cell
    /// its tenant's VM processes don't ([`Connection::authorize`]), is refused, whatever it is, in
    /// one place (TASK-160.9).
    fn serve_message_in_turn(self: Arc<Self>, message: Message, charge: Charge) {
        // cladefs: served with the guest request it is a call for current, if any, as its futures
        // are polled too, wherever: what its log records and its span name (`crate::request`,
        // TASK-260).
        let guest = match codec::guest_request(&message) {
            Ok((Some(guest), _)) => Some(cladefs_core::request::RequestId {
                guest,
                open_cell: self.named.get().cloned(),
            }),
            // Refused as it is decoded, if it says one it doesn't hold.
            Ok((None, _)) | Err(_) => None,
        };
        let answer = Answer::new(&self, message.header, guest.as_ref(), Some(charge));
        let turn = Turn::new(self.clone());
        // cladefs: the request's span the parent of what serving it does (TASK-258).
        let parent = answer.span.parent();
        // cladefs: every request a future, served as one (TASK-685.3): one that waits for nothing
        // is answered in its first poll, here, in its turn, as it was served before; one that
        // waits, as a commit for its upload or a fault for its pages, is deferred (`serve_future`).
        let serving = self.serve_message(message, answer);
        serve_future(turn, cladefs_core::request::within(guest, parent.around(serving)));
    }

    /// cladefs: serves the request `message` is, as `answer` owes (TASK-685.3): refused if its
    /// cell halted, or it is malformed, or names a cell its tenant doesn't hold.
    async fn serve_message(self: Arc<Self>, message: Message, answer: Answer) {
        let refuse = |answer: Answer, errno| {
            let reply = answer.reply_header();
            answer.send(Err(errno), reply, Vec::new(), Vec::new())
        };
        // cladefs: a cell a panic went through serves nothing more, of whatever the panic left half
        // done (`crate::halt`, TASK-153.16): `EIO`; nor one whose tenant's or cell's keys the
        // daemon forgot, `EACCES` (TASK-411.7).
        if let Some(errno) = self.cell().and_then(|open_cell| open_cell.halted()) {
            return refuse(answer, errno);
        }
        let request = Request::decode(&message).and_then(|request| {
            request.paths().try_for_each(|path| self.authorize(path))?;
            Ok(request)
        });
        if let Ok(request) = &request {
            traced(&answer.span, request);
        }
        match request {
            Err(Errno::EPROTONOSUPPORT) => refuse_version(&message.header, answer),
            Err(errno) => refuse(answer, errno),
            Ok(Request::Fault(fault)) => self.clone().fault(fault, answer).await,
            #[cfg(all(target_os = "linux", not(madsim)))]
            Ok(Request::GuestStream { ordered_only, listener }) => {
                match listener.0.try_clone_to_owned() {
                    Ok(listener) => self.serve_guest_stream(ordered_only, listener, answer),
                    Err(err) => {
                        log::warn!("cladefsd: keeping a guest stream's listener: {err}");
                        refuse(answer, Errno::EMFILE)
                    }
                }
            }
            #[cfg(not(all(target_os = "linux", not(madsim))))]
            Ok(Request::GuestStream { .. }) => refuse(answer, Errno::ENOTSUP),
            // cladefs: boxed, as an opening's future holds all a cell's opening awaits, some 22 KB,
            // which every request's future, commits' and writes' too, would hold and be copied
            // with, onto the heap in `serve_future`: some 15% of the daemon's CPU under pgbench
            // (TASK-707).
            Ok(Request::Open { path, opening, build }) => {
                let build = build.to_owned();
                Box::pin(self.clone().serve_open(path, opening, build, answer)).await
            }
            Ok(request) => self.serve(&request, answer).await,
        }
    }

    /// Serves the next request waiting, if any, on the [`Pool`], the last having been served; or,
    /// with none, counts one fewer running.
    fn next_turn(self: Arc<Self>) {
        loop {
            let mut requests = self.requests.with(Mutex::lock);
            if let Some(next) = requests.waiting.pop_front() {
                drop(requests);
                // What answers it if no thread is had to serve it: a message `EAGAIN`; a stream's
                // request is answered with nothing written as it drops with what would have served
                // it.
                let refused = match &next {
                    Work::Message(message, _) => Some(message.header),
                    #[cfg(all(target_os = "linux", not(madsim)))]
                    Work::Stream(_) => None,
                };
                let connection = self.clone();
                // Its charge, refused, dropped with what would have served it.
                if self.daemon().pool().run(Box::new(move || connection.serve_in_turn(next))) {
                    return;
                }
                if let Some(header) = refused {
                    let answer = Answer::new(&self, header, None, None);
                    let reply = answer.reply_header();
                    answer.send(Err(Errno::EAGAIN), reply, Vec::new(), Vec::new());
                }
                continue;
            }
            requests.running -= 1;
            let drained = requests.is_done().then(|| self.drained.clone());
            drop(requests);
            // Release the connection before waking the waiter (TASK-54).
            drop(self);
            if let Some(drained) = drained {
                drained.notify(usize::MAX);
            }
            return;
        }
    }

    /// cladefs: counts a request deferred ([`Turn::defer`]) answered.
    fn answered_deferred(self: Arc<Self>) {
        let mut requests = self.requests.with(Mutex::lock);
        requests.deferred -= 1;
        let drained = requests.is_done().then(|| self.drained.clone());
        drop(requests);
        // Told once this holds the connection no more, as at the end of a turn.
        drop(self);
        if let Some(drained) = drained {
            drained.notify(usize::MAX);
        }
    }

    /// Whether it may name the cell at `path`: when a managed [`DiskPool`] is active, as
    /// [`Connection::named`] has it; without one (standalone unit tests), by directory containment
    /// (`self.tenant.holds`).
    fn authorize(&self, path: &Path) -> Result<(), Errno> {
        match self.daemon().disk_pool() {
            Some(_) => self.named(path).map(drop),
            None => self.tenant.holds(path),
        }
    }

    /// The cell `path` names with a managed [`DiskPool`] active: a `TENANT/CELL` cell ID (`EINVAL`
    /// for anything else, raw host paths included), `TENANT/0` a new cell of TENANT's. A granted
    /// tenant names cells of its own alone (`EACCES`), and its new ones `0/0` too, as the
    /// integrator guide has it: the cell named then is of the granted tenant (board 13876).
    fn named(&self, path: &Path) -> Result<cladefs_core::CellId, Errno> {
        let cell: cladefs_core::CellId =
            path.to_str().ok_or(Errno::EINVAL)?.parse().map_err(|_| Errno::EINVAL)?;
        match self.tenant.granted() {
            Some(granted) if cell == (cladefs_core::CellId { tenant: 0, id: 0 }) => {
                Ok(cladefs_core::CellId { tenant: granted, id: 0 })
            }
            Some(granted) if cell.tenant != granted => Err(Errno::EACCES),
            _ => Ok(cell),
        }
    }

    /// Serves `request`, and replies, as `answer` owes.
    async fn serve(&self, request: &Request<'_>, answer: Answer) {
        let mut reply = answer.reply_header();
        let mut payload = Vec::new();
        let handled = self.handle(request, &mut reply, &mut payload);
        let result = answer.span.around(handled).await;
        answer.send(result, reply, payload, Vec::new());
    }

    /// Serves `request`: what its reply says in `reply` and `payload`.
    async fn handle(
        &self,
        request: &Request<'_>,
        reply: &mut Header,
        payload: &mut Vec<u8>,
    ) -> Result<(), Errno> {
        let open_cell = self.cell().ok_or(Errno::EBADF)?;
        // cladefs: a request failing, with the errno given (EIO by default), or waiting, as
        // FAILPOINTS says, in a daemon built with the failpoints feature (TASK-44.10):
        // `cladefsd::map=1%return(5)` fails one MAP in a hundred with EIO, `cladefsd::commit=
        // sleep(50)` delays each COMMIT 50 ms.
        #[cfg(feature = "failpoints")]
        if let Some(version) = failpoint(request.kind()) {
            fail::fail_point!(version, |errno: Option<String>| {
                // Not a number: EIO, the failpoint's default.
                let errno = errno.and_then(|errno| errno.parse().ok());
                Err(Errno::from_raw(errno.unwrap_or(Errno::EIO.raw())))
            });
        }
        match request {
            Request::Transfer { len, area } => {
                let area = TransferArea::map(area.0, *len)?;
                self.transfer.set(area).map_err(|_| Errno::EEXIST)
            }
            Request::Read { extent, offset, len, into: ReadInto::Transfer { at } } => {
                let began = std::time::Instant::now();
                let area = self.transfer.get().ok_or(Errno::EINVAL)?;
                // cladefs: in its turn, as a fault's, as what it pages in holds memory: a read
                // served as a future holds no thread to bound it by (TASK-149.34, doc-24 step 5).
                let _turn = self.page_ins.acquire().await.map_err(|_| Errno::EIO)?;
                open_cell.read(*extent, *offset, &mut [area.slice_mut(*at, *len)?]).await?;
                self.traced(Kind::Read, began, open_cell, *extent, *offset, *len);
                Ok(())
            }
            // As much as a reply holds, the codec has it (TASK-79.16).
            Request::Read { extent, offset, len, into: ReadInto::Reply } => {
                let began = std::time::Instant::now();
                payload.resize(*len as usize, 0);
                let _turn = self.page_ins.acquire().await.map_err(|_| Errno::EIO)?;
                let mut into = [VolatileSliceMut::from(&mut payload[..])];
                open_cell.read(*extent, *offset, &mut into).await?;
                self.traced(Kind::Read, began, open_cell, *extent, *offset, *len);
                Ok(())
            }
            Request::Write { extent, offset, data, barriers } => {
                let data = match *data {
                    Data::Inline(data) => VolatileSlice::from(data),
                    Data::Transfer { at, len } => {
                        self.transfer.get().ok_or(Errno::EINVAL)?.slice(at, len)?
                    }
                };
                // cladefs: within its disk and its tenant's limits, as its cell admits it (doc-22).
                open_cell.write_with_barriers(*extent, *offset, &[data], *barriers).await
            }
            // cladefs: one extent or many in order (TASK-92.13, board 11697).
            Request::Free { first, rest } => {
                let mut extents = std::iter::once(first).chain(rest.iter());
                extents.try_for_each(|extent| open_cell.free(*extent))
            }
            // cladefs: one mapping or many in one change of the window, with optional daemon-side
            // cache warming via `MAP_WARM` (TASK-92.14, board 11681, board 11697).
            Request::Map { mappings, warm } => {
                let began = std::time::Instant::now();
                if *warm {
                    open_cell.map_many_warm(mappings).await?;
                } else if let [mapping] = mappings.as_ref() {
                    match mapping.contents {
                        MappingContents::Zeros => {
                            open_cell.map_zeros_at(mapping.window_offset, mapping.len)?
                        }
                        MappingContents::Extent { extent, offset } => {
                            open_cell
                                .map(extent, offset, mapping.len, mapping.window_offset)
                                .await?
                        }
                    }
                } else {
                    open_cell.map_many(mappings).await?;
                }
                self.traced_maps(began, open_cell, mappings);
                Ok(())
            }
            Request::Mappings { quiescent } => {
                if *quiescent && !open_cell.is_quiescent()? {
                    return Err(Errno::EBUSY);
                }
                *payload = codec::mappings_payload(&open_cell.window_mappings()?);
                Ok(())
            }
            Request::Unmap { window_offset, len } => open_cell.unmap(*window_offset, *len),
            Request::Commit { commit: Commit::Uploaded, root } => open_cell.commit(*root).await,
            Request::Commit { commit: Commit::Local, root } => open_cell.commit_local(*root).await,
            Request::Commit { commit: Commit::Ordered, root } => {
                open_cell.commit_ordered(*root).await
            }
            Request::Root => open_cell.root().map(|root| *payload = root),
            Request::Version => {
                // A VM snapshot's: a version of a bucket-backed cell (doc-20), or a local-only
                // quiesced checkpoint of an ephemeral cell (doc-25), refused while the window
                // holds private pages, whose bytes no extent holds (TASK-203). Each refusal says
                // why here, as the VMM's error can't (TASK-92.14).
                // Boxed, as a cell's opening is (`serve_message`, TASK-707).
                let cell = open_cell.cell();
                if open_cell.is_ephemeral() {
                    let committed =
                        Box::pin(open_cell.quiesce_ephemeral()).await.map_err(|err| {
                            if err.errno() == Some(Errno::EBUSY) {
                                log::warn!("cladefsd: an ephemeral snapshot refused: {err:#}");
                            }
                            cells::errno_of(format_args!("quiescing ephemeral cell"), err)
                        })?;
                    reply.set_body(
                        &Forked { version: 0, tenant: cell.tenant, committed, ephemeral: true }
                            .to_body(),
                    );
                    return Ok(());
                }
                let version = Box::pin(open_cell.make_version(None)).await.map_err(|err| {
                    if err.errno() == Some(Errno::EBUSY) {
                        log::warn!("cladefsd: a version refused: {err:#}");
                    }
                    cells::errno_of(format_args!("making a version"), err)
                })?;
                self.versions_made.with(Mutex::lock).insert(version);
                reply.set_body(
                    &Forked { version, tenant: cell.tenant, committed: 0, ephemeral: false }
                        .to_body(),
                );
                Ok(())
            }
            // cladefs: a version it made, which a VM snapshot failing past it names nowhere
            // (TASK-575): no other, which its VM manager's are (`EPERM`).
            Request::DropVersion { version } => {
                if !self.versions_made.with(Mutex::lock).contains(version) {
                    return Err(Errno::EPERM);
                }
                let ids = std::collections::BTreeSet::from([*version]);
                Box::pin(open_cell.drop_versions(&ids)).await.map_err(|err| {
                    cells::errno_of(format_args!("dropping version {version:x}"), err)
                })?;
                self.versions_made.with(Mutex::lock).remove(version);
                Ok(())
            }
            Request::Info => open_cell.info().map(|info| reply.set_body(&Info(info).to_body())),
            // No larger than the daemon makes room for, the codec has it (TASK-150.3).
            Request::Window { size, copying, budget } => {
                let (sender, acks) = (self.peer.sender.clone(), self.peer.acks.clone());
                let stage = Arc::new(VmProcessStage {
                    sender,
                    acks,
                    vm: self.peer.id,
                    change: SharedLock::new("stage change", Mutex::default()),
                });
                // cladefs: a writer's copies on write (backlog doc-13), in its VM process (doc-26).
                if !*copying || self.hold().is_none() {
                    return Err(Errno::EINVAL);
                }
                let window = open_cell.create_copying_dax_window(*size, stage);
                let window = window.map_err(|err| {
                    log::warn!("cladefsd: creating a window: {err:#}");
                    // cladefs: no memory for it, or a cell that takes none, say so.
                    match err.errno() {
                        Some(errno @ (Errno::ENOMEM | Errno::EROFS | Errno::EPERM)) => errno,
                        _ => Errno::EINVAL,
                    }
                })?;
                // cladefs: its mappings' reply, as many as it may have, its VM process's credit
                // holds (TASK-79.43).
                self.peer.set_window(window.size());
                // cladefs: its gauges, and its runs' budget, as its VM process says (TASK-111.5).
                self.report.windowed(window.gauges(), *budget);
                *self.window.with(Mutex::lock) = Some(window);
                Ok(())
            }
            Request::Resized { capacity, took } => {
                self.shrinking.answered(*capacity, *took);
                Ok(())
            }
            Request::Stats { ask, guest } => self.report.answered(*ask, *guest),
            // Served apart, as futures ([`Connection::serve_message`]).
            Request::Open { .. } | Request::Fault(_) | Request::GuestStream { .. } => {
                Err(Errno::EINVAL)
            }
        }
    }

    /// cladefs: resolves the guest's `fault` ([`FAULT`]), and replies: a future, which holds no
    /// thread while it waits for the pager (TASK-92.20), served so ([`serve_future`]).
    async fn fault(self: Arc<Self>, fault: Fault, answer: Answer) {
        let Fault { offset, .. } = fault;
        let began = std::time::Instant::now();
        let faulted = async {
            let window = self.window.with(Mutex::lock).clone().ok_or(Errno::EBADF)?;
            // cladefs: in its turn, as what it pages in holds memory (TASK-149.34).
            let _turn = self.page_ins.acquire().await.map_err(|_| Errno::EIO)?;
            window.fault(offset, cladefs_core::Access::Read).await?;
            Ok(window)
        };
        let faulted = match faulted.await {
            Ok(window) => {
                self.traced_fault(&window, began, offset);
                Ok(())
            }
            Err(errno) => Err(errno),
        };
        // cladefs: an access to a page the window maps nothing at stops its VM if the guest goes on
        // with it for a second, with no more than KVM_RUN's EFAULT to say so: what the window had
        // there, and its latest MAPs and UNMAPs, each refusal of the first thousand, a run's few,
        // and every power of two after (TASK-99, TASK-101, TASK-119.21).
        let refused = refusal(&fault, faulted);
        if let Some(what) = refused {
            // The process's count of refusals, which spaces their warnings: every host's together.
            static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let count = REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if (count <= 1000 || count.is_power_of_two())
                && let Some(open_cell) = self.cell()
            {
                log::warn!(
                    "cladefsd: {what} at DAX window offset {offset:#x} refused ({count} so far): {}",
                    open_cell.refused(offset - offset % cladefs_core::pages::page_size(), what)
                );
            }
        }
        let reply = answer.reply_header();
        answer.send(faulted, reply, Vec::new(), Vec::new());
    }

    /// The VM process's physical memory, which the reply to its `OPEN` hands it, of the arena
    /// it says ([`Opened`]): its tenant's arena, and the default one, each read-only (TASK-43,
    /// doc-26); the default tenant's as any other's (TASK-79.17).
    fn memories(&self) -> [std::os::fd::BorrowedFd<'_>; 2] {
        let memory = &self.tenant.memory;
        [memory.memory.as_fd(), memory.public.as_fd()]
    }

    /// cladefs: the connection's one [`OPEN`] at a time: `EBUSY` if another is in progress, as its
    /// requests are served side by side, or it has its cell (TASK-150.5). What an `OPEN` holds
    /// meanwhile, its cell's path among it, its [`Opening`] gives the connection, or it lets go of.
    fn opening(&self) -> Result<Opening<'_>, Errno> {
        let mut phase = self.phase.with(Mutex::lock);
        match *phase {
            Phase::Unopened => {
                *phase = Phase::Opening;
                Ok(Opening(self))
            }
            Phase::Opening | Phase::Writing(_) => Err(Errno::EBUSY),
        }
    }

    /// cladefs: serves an [`OPEN`] of the cell at `path` to write, as `how` says
    /// ([`Connection::open`]), recording its VM process's `build`, and replies with its cell,
    /// capacity and physical memory: a future, which holds no thread while it waits (TASK-92.5),
    /// served so ([`serve_future`]).
    async fn serve_open(
        self: Arc<Self>,
        path: PathBuf,
        how: codec::Opening,
        build: String,
        answer: Answer,
    ) {
        let mut reply = answer.reply_header();
        let opened = self.open(&path, how, &build).await;
        let (payload, fds) = match &opened {
            Ok((cell, ephemeral, capacity)) => {
                let (arena, max_map_count) = (self.arena_id(), max_map_count());
                let opened = Opened {
                    cell: cell.id,
                    tenant: cell.tenant,
                    arena,
                    max_map_count,
                    ephemeral: *ephemeral,
                    capacity: *capacity,
                };
                reply.set_body(&opened.to_body());
                (opened.capacity_bytes().to_vec(), Vec::from(self.memories()))
            }
            // Not opened: no memories to send; the failure goes in the answer.
            Err(_) => (Vec::new(), Vec::new()),
        };
        answer.send(opened.map(drop), reply, payload, fds);
    }

    /// cladefs: records in its trace of its guest's reads, if it keeps one (TASK-702.1), a read of
    /// `kind` the guest began at `began`, of `len` bytes at `offset` in `extent`, of `open_cell`.
    fn traced(
        &self,
        kind: Kind,
        began: std::time::Instant,
        open_cell: &OpenCell,
        extent: cladefs_core::ExtentId,
        offset: u64,
        len: u64,
    ) {
        // cladefs: its cell's version's read evidence, if it records it (TASK-702.2).
        open_cell.guest_read(extent, offset, len, began);
        if let Some(trace) = self.reads.get() {
            // An extent the cell can't say the length of, an attached base's, is of 0.
            let extent_len =
                open_cell.stored_extent(extent).ok().flatten().map_or(0, |(len, _)| len);
            trace.record(kind, began, extent, extent_len, offset, len);
        }
    }

    /// cladefs: records in its trace, as [`Connection::traced`] does, the guest's `MAP`s of
    /// `mappings`, begun at `began` and made: each of a sealed extent's bytes, as the guest maps a
    /// file's range it touches, whose pages a warm host installs without a fault.
    fn traced_maps(
        &self,
        began: std::time::Instant,
        open_cell: &OpenCell,
        mappings: &[WindowMapping],
    ) {
        if self.reads.get().is_none() && !open_cell.records_reads() {
            return;
        }
        for mapping in mappings {
            if let MappingContents::Extent { extent, offset } = mapping.contents {
                self.traced(Kind::Map, began, open_cell, extent, offset, mapping.len);
            }
        }
    }

    /// cladefs: records in its trace, as [`Connection::traced`] does, the guest's fault at `offset`
    /// in `window`, begun at `began` and resolved: the page of the sealed
    /// extent mapped there, if one is still.
    fn traced_fault(&self, window: &DaxWindow, began: std::time::Instant, offset: u64) {
        let Some(open_cell) = self.cell() else { return };
        if self.reads.get().is_none() && !open_cell.records_reads() {
            return;
        }
        let Some((extent, at)) = window.extent_at(offset) else { return };
        self.traced(Kind::Fault, began, open_cell, extent, at, cladefs_core::pages::page_size());
    }

    /// Opens the cell at `path`, authorized, as `how` says: creating it if it doesn't exist, or
    /// forking it from the version `how` names if it doesn't exist ([`cells::fork_version`]);
    /// unless another connection has it open, or others have it open (`EBUSY`); and returns its
    /// cell. Its waits, for a connection whose VM
    /// process is gone to let the cell go, and for a restore's version, hold no thread
    /// (TASK-92.5); its I/O blocks. A version of another tenant's than the connection's forks only
    /// if it is shared (`EACCES`, TASK-395.7).
    async fn open(
        &self,
        path: &Path,
        how: codec::Opening,
        build: &str,
    ) -> Result<(cladefs_core::CellId, bool, u64), Errno> {
        let opening = self.opening()?;
        let (resolved, effective_how, named_cell) = if let Some(pool) = self.daemon().disk_pool() {
            let req_cell = self.named(path)?;
            if req_cell.id != 0 {
                (cells::managed_cell_dir(pool, req_cell).await?, how, None)
            } else {
                let daemon = self.daemon();
                let disk = pool.pick_disk()?;
                match how {
                    codec::Opening::Existing
                    | codec::Opening::LocalEphemeral { .. }
                    | codec::Opening::ExistingEphemeralMemory { .. } => {
                        return Err(Errno::EINVAL);
                    }
                    codec::Opening::Create(_, _) => {
                        let (final_path, cell) =
                            cells::new_managed_cell(daemon, &disk, req_cell.tenant).await?;
                        (final_path, how, Some(cell))
                    }
                    codec::Opening::CreateEphemeral(capacity, kind) => {
                        let disk_for_stage = Arc::clone(&disk);
                        let staging = cells::blocking(move || {
                            crate::disks::DiskPool::staging_path(&disk_for_stage, req_cell.tenant)
                        })
                        .await?;
                        let made = cells::create(
                            daemon,
                            staging.clone(),
                            capacity,
                            cladefs_core::CellId { tenant: req_cell.tenant, id: 0 },
                            (None, kind, None, None, true),
                        )
                        .await;
                        let made = match made {
                            Ok(made) => made,
                            Err(err) => {
                                let cleanup = staging.clone();
                                let local = Arc::clone(daemon.host.local());
                                // Best-effort cleanup of failed staging dir; primary error wins.
                                let _ = cells::blocking(move || {
                                    if host_fs::exists(&cleanup) {
                                        // Best-effort cleanup of failed staging dir; primary error wins.
                                        let _ = local_cell::remove_cell_dir_on(&local, &cleanup);
                                    }
                                    Ok(())
                                })
                                .await;
                                return Err(err);
                            }
                        };
                        let pool_commit = Arc::clone(pool);
                        let disk_commit = Arc::clone(&disk);
                        let staging_commit = staging.clone();
                        let final_path = match cells::blocking(move || {
                            pool_commit.commit_staged(&disk_commit, made, &staging_commit)
                        })
                        .await
                        {
                            Ok(final_path) => final_path,
                            Err(err) => {
                                let cleanup = staging;
                                let local = Arc::clone(daemon.host.local());
                                // Best-effort cleanup of failed staging dir; primary error wins.
                                let _ = cells::blocking(move || {
                                    if host_fs::exists(&cleanup) {
                                        // Best-effort cleanup of failed staging dir; primary error wins.
                                        let _ = local_cell::remove_cell_dir_on(&local, &cleanup);
                                    }
                                    Ok(())
                                })
                                .await;
                                return Err(err);
                            }
                        };
                        (final_path, codec::Opening::Existing, None)
                    }
                    codec::Opening::ForkOf { tenant, .. } => {
                        // Allocate the child cell ID up front so `open_held` forks and opens it
                        // directly at `<disk>/cells/<TENANT>/<CELL>` under `defer_durability_on`
                        // (board 7420), with no staging directory or eager syncs.
                        // `0/0` on the default tenant's socket: of the version's tenant.
                        let child_tenant = match req_cell.tenant {
                            0 => tenant,
                            named => named,
                        };
                        let child = cells::new_cell(daemon, child_tenant).await?;
                        let disk_prep = Arc::clone(&disk);
                        let final_path = cells::blocking(move || {
                            drop(crate::disks::DiskPool::ensure_tenant_dir(
                                &disk_prep,
                                child.tenant,
                            )?);
                            Ok(crate::disks::DiskPool::cell_path(&disk_prep, child))
                        })
                        .await?;
                        (final_path, how, Some(child))
                    }
                }
            }
        } else {
            (path.to_owned(), how, None)
        };
        let writer: Weak<dyn cells::CellWriter> = self.this.clone();
        let hold = cells::hold_to_write(self.daemon(), &cell_dir_key(&resolved), writer).await?;
        // cladefs: nor one of another tenant's cells (TASK-364).
        self.tenant.reaches(&resolved)?;
        // cladefs: a cell it makes, within its disk and its tenant's limits, as its making admits
        // it (doc-22).
        // Closed before its hold lets its path go, if it fails here.
        let open_cell = self.open_held(&resolved, &hold, &effective_how, named_cell).await?;
        let capacity = match open_cell.info() {
            Ok(info) => info.capacity,
            Err(err) => {
                cladefs_core::open_cell::release(open_cell).await;
                return Err(err);
            }
        };
        if let Some(pool) = self.daemon().disk_pool()
            && let Some(cell) = named_cell
        {
            pool.register_cell(cell, resolved.clone());
        }
        let flushed = open_cell.clone();
        let flushing_daemon = Arc::clone(self.daemon());
        self.start_behind(move |stopped| flush_behind(flushing_daemon, flushed, stopped));
        let cell = open_cell.cell();
        let ephemeral = open_cell.is_ephemeral();
        // cladefs: should its VM process go, and a writer wait for its cell to close, the cell's
        // uploader stops at once rather than upload what it holds first (TASK-216).
        if let Some(cut_short) = open_cell.cut_short() {
            hold.when_wanted(cut_short);
        }
        opening.opened(&resolved, open_cell, hold);
        // Recorded once per connection when OPEN succeeds; `Opening` ensures OPEN succeeds at most once.
        let _ = self.report.said_build(build);
        Ok((cell, ephemeral, capacity))
    }

    /// Opens the cell at `path`, which `hold` holds, as [`Connection::open`] does: the cell
    /// open there from now on.
    async fn open_held(
        &self,
        path: &Path,
        hold: &cells::Hold,
        how: &codec::Opening,
        named_cell: Option<cladefs_core::CellId>,
    ) -> Result<Arc<OpenCell>, Errno> {
        let daemon = self.daemon();
        let arena = self.arena();
        // cladefs: a restore's cell there already, which the snapshot was taken of or an earlier
        // restore forked, holds the version's state, or is refused, as it is (TASK-194).
        let mut there = None;
        if let codec::Opening::ForkOf { tenant, cell, version, capacity } = *how
            && local_cell::upload::blocking(|| {
                host_fs::exists(&path) && !cladefs_core::host_cell::unmade_on(&daemon.host, path)
            })
        {
            let open_cell = Arc::new(open_cell_at(daemon, path, None, Some(arena)).await?);
            if let Err(err) = cells::at_version(
                daemon,
                (cladefs_core::CellId { tenant, id: cell }, version),
                capacity,
                path,
                &open_cell,
            )
            .await
            {
                cladefs_core::open_cell::release(open_cell).await;
                return Err(err);
            }
            there = Some(open_cell);
        }
        let open_cell = match (there, how) {
            (Some(open_cell), _) => open_cell,
            // cladefs: or where a cell's making was left unmade, its host crashing, which is no
            // cell: removed first, as deleting it would (TASK-79.84).
            (None, &codec::Opening::ForkOf { tenant, cell, version, capacity }) => {
                local_cell::upload::blocking(|| cells::removed_if_unmade_on(&daemon.host, path))?;
                let forked = (cladefs_core::CellId { tenant, id: cell }, version);
                // cladefs: a restore's fork, made and opened syncing nothing: durable by its first
                // flush or its close, before any commit of its VM is (board 7420, the user's
                // call). A host crash before leaves it unmade, as if the restore never was.
                // cladefs: a crash of the daemon's too, its VM processes going with it.
                let deferring = local_cell::defer_durability_on(daemon.host.local(), path);
                cells::fork_version(
                    daemon,
                    forked,
                    capacity,
                    (path, self.tenant.granted(), named_cell),
                )
                .await?;
                let open_cell = open_cell_at(daemon, path, None, Some(arena)).await?;
                deferring.opened();
                Arc::new(open_cell)
            }
            (None, codec::Opening::Create(capacity, kind)) => {
                let new = cells::New {
                    capacity: *capacity,
                    kind: *kind,
                    tenant: self.tenant.id,
                    cell: named_cell,
                    ephemeral: false,
                };
                Arc::new(open_cell_at(daemon, path, Some(new), Some(arena)).await?)
            }
            (None, codec::Opening::CreateEphemeral(capacity, kind)) => {
                local_cell::upload::blocking(|| cells::removed_if_unmade_on(&daemon.host, path))?;
                let new = cells::New {
                    capacity: *capacity,
                    kind: *kind,
                    tenant: self.tenant.id,
                    cell: None,
                    ephemeral: true,
                };
                let open_cell = Arc::new(open_cell_at(daemon, path, Some(new), Some(arena)).await?);
                if !open_cell.is_ephemeral() || open_cell.kind() != Some(*kind) {
                    cladefs_core::open_cell::release(open_cell).await;
                    return Err(Errno::EINVAL);
                }
                open_cell
            }
            (None, &codec::Opening::LocalEphemeral { tenant, cell, committed, capacity: _ }) => {
                let exists = local_cell::upload::blocking(|| {
                    host_fs::exists(&path)
                        && !cladefs_core::host_cell::unmade_on(&daemon.host, path)
                });
                if !exists {
                    return Err(Errno::ENOENT);
                }
                let open_cell = Arc::new(open_cell_at(daemon, path, None, Some(arena)).await?);
                let expected = cladefs_core::CellId { tenant, id: cell };
                let checked = (|| -> Result<(), Errno> {
                    if !open_cell.is_ephemeral() {
                        return Err(Errno::EINVAL);
                    }
                    if open_cell.cell() != expected {
                        log::warn!(
                            "cladefsd: refusing local ephemeral restore of {expected} into {}: \
                             holds cell {}",
                            path.display(),
                            open_cell.cell()
                        );
                        return Err(Errno::EEXIST);
                    }
                    let actual = open_cell.committed()?;
                    if actual != committed {
                        log::warn!(
                            "cladefsd: refusing local ephemeral restore of {expected} at commit \
                             {committed} into {}: cell is at commit {actual}",
                            path.display(),
                        );
                        return Err(Errno::EEXIST);
                    }
                    Ok(())
                })();
                if let Err(err) = checked {
                    cladefs_core::open_cell::release(open_cell).await;
                    return Err(err);
                }
                open_cell
            }
            (None, &codec::Opening::ExistingEphemeralMemory { tenant, cell, capacity: _ }) => {
                let exists = local_cell::upload::blocking(|| {
                    host_fs::exists(&path)
                        && !cladefs_core::host_cell::unmade_on(&daemon.host, path)
                });
                if !exists {
                    return Err(Errno::ENOENT);
                }
                let open_cell = Arc::new(open_cell_at(daemon, path, None, Some(arena)).await?);
                let checked = (|| -> Result<(), Errno> {
                    if !open_cell.is_ephemeral()
                        || open_cell.kind() != Some(bucket_cell::cell::Kind::Memory)
                    {
                        return Err(Errno::EINVAL);
                    }
                    if cell != 0 && open_cell.cell() != (cladefs_core::CellId { tenant, id: cell })
                    {
                        return Err(Errno::EEXIST);
                    }
                    Ok(())
                })();
                if let Err(err) = checked {
                    cladefs_core::open_cell::release(open_cell).await;
                    return Err(err);
                }
                open_cell
            }
            (None, codec::Opening::Existing) => {
                Arc::new(open_cell_at(daemon, path, None, Some(arena)).await?)
            }
        };
        // A VM process opens no cell to write that its writer may not write
        // ([`crate::CellAccess::may_write`]): checked before publishing via `hold.open`, and every
        // refused open closes through `release().await` (TASK-43, TASK-79.17, board 5386).
        if let Err(err) = open_cell.access().may_write().and_then(|()| hold.open(&open_cell)) {
            cladefs_core::open_cell::release(open_cell).await;
            return Err(err);
        }
        Ok(open_cell)
    }

    /// Whether a shrink waits for the guest's answer.
    #[cfg(test)]
    pub(super) fn shrink_awaited(&self) -> bool {
        self.shrinking.awaited()
    }

    /// How many of its requests it received and has not answered.
    #[cfg(test)]
    pub(super) fn unanswered(&self) -> usize {
        self.requests.with(Mutex::lock).in_flight()
    }

    /// How many bytes it has queued for its VM process, which its socket has not taken.
    #[cfg(test)]
    pub(super) fn queued(&self) -> u64 {
        self.peer.sender.queued()
    }

    /// Sets its cell's capacity to `capacity` bytes (the control API's `ResizeCell`): grows it,
    /// and tells the guest; or asks the guest to shrink, and shrinks it if the guest does, holding
    /// no thread while it waits for the guest's answer (TASK-43.6). The cell's I/O runs on a
    /// blocking thread.
    pub(super) async fn resize(&self, capacity: u64) -> Result<(), Errno> {
        let open_cell = self.cell().ok_or(Errno::EBADF)?.clone();
        let resized = self
            .shrinking
            .resize(
                capacity,
                SHRINK_WAIT,
                || cells::capacity(&open_cell),
                |cap| open_cell.set_capacity(cap),
                |cap, ask| self.tell_capacity(cap, ask),
            )
            .await;
        cladefs_core::open_cell::release(open_cell).await;
        resized
    }

    /// Tells the VM process the cell's capacity is `capacity` bytes, or, if `ask`, asks its guest
    /// to shrink to it ([`CAPACITY`]).
    fn tell_capacity(&self, capacity: u64, ask: bool) -> Result<(), Errno> {
        let (header, payload) = Told::Capacity { capacity, ask }.encode();
        self.peer.sender.send(header, payload, &[]).map_err(|err| {
            log::warn!("cladefsd: telling a VM process its capacity: {err}");
            Errno::EIO
        })
    }
}

/// A connection's transfer area ([`TRANSFER`]), shared memory its VM process made, mapped here:
/// which `READ`s write and `WRITE`s read their data in, as the VM process says, which may change
/// it meanwhile: it is accessed as volatile memory, its ranges checked against its length.
struct TransferArea {
    addr: std::ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: Shared memory mapped for the area's life, accessed as volatile memory alone.
unsafe impl Send for TransferArea {}
// SAFETY: as Send's: shared, it is only read and written volatile, through VolatileSlice(Mut),
// as memory another process writes too.
unsafe impl Sync for TransferArea {}

impl TransferArea {
    /// The `len` bytes of the shared memory `fd`, mapped: `EINVAL` unless it has that many, is a
    /// whole number of host pages, is sealed against shrinking, and every page in `0..len` is
    /// already written by the VM process.
    /// cladefs: sealed here, once mapped, against future writes (`F_SEAL_FUTURE_WRITE |
    /// F_SEAL_GROW | F_SEAL_SEAL`, which blocks `fallocate(FALLOC_FL_PUNCH_HOLE)` and
    /// `madvise(MADV_REMOVE)` while letting existing mappings write), then checked hole-free once
    /// with `lseek(fd, 0, SEEK_HOLE)` (TASK-547, board 6881, board 11707, hotbed question 575).
    /// Because the kernel charges a page of shared memory to the process that faults it in
    /// (`mm/shmem.c`) and retains swap entries as non-holes in shmem's xarray, a single post-seal
    /// `SEEK_HOLE >= len` guarantees every page in `0..len` is already charged to the VM process
    /// (whether resident or swapped out) and can never become a hole again.
    fn map(fd: std::os::fd::BorrowedFd<'_>, len: u64) -> Result<Self, Errno> {
        use std::os::fd::AsRawFd;
        let page = cladefs_core::pages::page_size();
        if len == 0 || len > MAX_TRANSFER_AREA || !len.is_multiple_of(page) {
            return Err(Errno::EINVAL);
        }
        if !seals::against_shrinking(fd) {
            return Err(Errno::EINVAL);
        }
        // SAFETY: A stat buffer the call fills.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: As above, of a descriptor the message carried.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0
            || stat.st_size < 0
            || (stat.st_size as u64) < len
        {
            return Err(Errno::EINVAL);
        }
        let len = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        // SAFETY: Maps shared memory, as large as it is, sealed against shrinking.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            log::warn!("cladefsd: mapping a transfer area: {}", std::io::Error::last_os_error());
            return Err(Errno::ENOMEM);
        }
        let area = Self { addr: std::ptr::NonNull::new(addr.cast()).unwrap(), len };
        if let Err(err) = seals::against_writes(fd) {
            log::warn!("cladefsd: a transfer area not to be sealed against writes: {err}");
            return Err(Errno::EINVAL);
        }
        let hole = seals::first_hole(fd, 0).map_err(|err| {
            log::warn!("cladefsd: a transfer area's holes: {err}");
            Errno::EINVAL
        })?;
        if hole < 0 || (hole as u64) < (len as u64) {
            log::warn!("cladefsd: a transfer area's bytes its VM process never wrote");
            return Err(Errno::EINVAL);
        }
        Ok(area)
    }

    /// The `len` bytes at `offset`, checked: `EINVAL` past its end. Every page within `0..self.len`
    /// was verified written once at [`TransferArea::map`] after sealing against future writes.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn range(&self, offset: u64, len: u64) -> Result<*mut u8, Errno> {
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if end > self.len as u64 {
            return Err(Errno::EINVAL);
        }
        // SAFETY: Within the mapping.
        Ok(unsafe { self.addr.as_ptr().add(offset as usize) })
    }

    fn slice(&self, offset: u64, len: u64) -> Result<VolatileSlice<'_>, Errno> {
        let addr = self.range(offset, len)?;
        // SAFETY: Mapped memory of the area's, accessed as volatile, as long as it is.
        Ok(unsafe { VolatileSlice::from_raw_parts(addr, len as usize) })
    }

    fn slice_mut(&self, offset: u64, len: u64) -> Result<VolatileSliceMut<'_>, Errno> {
        let addr = self.range(offset, len)?;
        // SAFETY: As above.
        Ok(unsafe { VolatileSliceMut::from_raw_parts(addr, len as usize) })
    }
}

/// cladefs: a transfer area's seals, Linux's (memfd). macOS has none, but a POSIX shared memory
/// object there, which the VM process makes its area of, is never resized once sized: that is its
/// seal against shrinking; and as macOS punches no holes in one, no seal against writes is needed
/// there, nor does [`TransferArea::map`] ask after holes (board 13065, hotbed question 566).
mod seals {
    use std::os::fd::BorrowedFd;

    /// Whether the memory `fd` is sealed against shrinking.
    #[cfg(target_os = "linux")]
    pub(super) fn against_shrinking(fd: BorrowedFd<'_>) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: Reads the seals of a descriptor the caller keeps open.
        let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
        seals >= 0 && seals & libc::F_SEAL_SHRINK != 0
    }

    /// Seals the memory `fd` against future writes, growing and further seals.
    #[cfg(target_os = "linux")]
    pub(super) fn against_writes(fd: BorrowedFd<'_>) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let seals = libc::F_SEAL_FUTURE_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        // SAFETY: Seals a descriptor the caller keeps open.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Whether the memory `fd` is fixed at a size: on macOS, whether it is a POSIX shared memory
    /// object (`fstat` gives it no file type), sized (`st_size`, which the kernel then keeps).
    #[cfg(not(target_os = "linux"))]
    pub(super) fn against_shrinking(fd: BorrowedFd<'_>) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: A stat buffer the call fills.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: Reads the status of a descriptor the caller keeps open.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            return false;
        }
        stat.st_mode & libc::S_IFMT == 0 && stat.st_size > 0
    }

    /// On macOS, nothing to seal: no hole is punched in a POSIX shared memory object.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn against_writes(_: BorrowedFd<'_>) -> std::io::Result<()> {
        Ok(())
    }

    /// The offset of the first hole of the memory `memory` at or after `at`, as `SEEK_HOLE`
    /// says: where the VM process wrote nothing yet.
    #[cfg(target_os = "linux")]
    pub(super) fn first_hole(
        memory: BorrowedFd<'_>,
        at: libc::off_t,
    ) -> std::io::Result<libc::off_t> {
        use std::os::fd::AsRawFd;
        // SAFETY: Seeks in a descriptor the caller keeps, of its own.
        let hole = unsafe { libc::lseek(memory.as_raw_fd(), at, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(hole)
    }

    /// On macOS, the end of `memory`: a POSIX shared memory object has no holes to tell of, its
    /// pages charged to no process that faults them, as Linux's shmem's are (TASK-547).
    #[cfg(not(target_os = "linux"))]
    pub(super) fn first_hole(
        memory: BorrowedFd<'_>,
        _: libc::off_t,
    ) -> std::io::Result<libc::off_t> {
        use std::os::fd::AsRawFd;
        // SAFETY: A stat buffer the call fills.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: Reads the status of a descriptor the caller keeps open.
        if unsafe { libc::fstat(memory.as_raw_fd(), &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(stat.st_size)
    }
}

impl Drop for TransferArea {
    fn drop(&mut self) {
        // SAFETY: Unmaps the area's mapping, which nothing uses any more.
        unsafe { libc::munmap(self.addr.as_ptr().cast(), self.len) };
    }
}

/// cladefs: makes `open_cell`'s commits durable within [`FLUSH_EVERY`] of being ordered, until
/// `stopped` says to stop: on the daemon's runtime, [`FLUSHES`] cells' flushes at once
/// ([`FLUSH_TURNS`]), each holding its thread only for its syncs (board 2944). Looking at every
/// turn, every cell open took a blocking thread at every turn, which hundreds of cells, their turns
/// coming at once, kept as many of, each idle for a while after (TASK-34). And so did their
/// flushes, every cell's whose guest committed, on the runtime's blocking threads (TASK-92.49): no
/// more threads do them than [`FLUSHES`], the work beyond waiting its turn, holding none. What the
/// guest frees leaves the host's disk with the segments acknowledged in the bucket, whose collector
/// takes it back, not collected behind the guest's commits as it was (TASK-79.2, TASK-165.8).
async fn flush_behind(
    daemon: Arc<Daemon>,
    open_cell: Arc<OpenCell>,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    while tokio::time::timeout(FLUSH_EVERY, &mut stopped).await.is_err() {
        if !open_cell.flush_pending() {
            open_cell.flush_turned();
            continue;
        }
        let flushed = async {
            let _turn = daemon.flush_turns.acquire().await.expect("never closed");
            match open_cell.flush_pending() {
                true => open_cell.flush().await,
                false => Ok(()),
            }
        };
        match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(flushed)).await {
            Ok(Ok(())) => open_cell.flush_turned(),
            Ok(Err(err)) => {
                log::error!("cladefsd: making a cell's commits durable: {err:?}");
                break;
            }
            // Its panic went through the cell (board 1212).
            Err(_) => {
                open_cell.halt(|| "a panic making its commits durable".into());
                break;
            }
        }
    }
}

/// cladefs: how many cells' flushes run at once, at most ([`flush_behind`], board 744), each on a
/// thread of the runtime's for its syncs alone (board 2944): as many syncs at once as a host's disk
/// takes, those past them waiting a few milliseconds for their turn, holding no thread; and the
/// 15 a flush of many files syncs them on beside its own, which every flush shares
/// (`local_cell`'s `HELPERS`), threads of the cell's pool, kept (`local_cell::pool`). A
/// blocking thread each, for every cell whose flush came due at its turn, hundreds of cells'
/// guests committing together kept as many as their syncs' waits took, each idle 10 s after,
/// and made no cell durable sooner: on a GCE host's disk, 300 cells' guests committing at once
/// kept 297, their commits durable within 54 ms, the median, as with these; half as many took
/// twice as long (`hundreds_of_writers_flush_on_threads_they_share`).
pub(super) const FLUSHES: usize = 32;

/// The stage operations a VM process has acknowledged: as it makes them in order, all up to the
/// last.
#[derive(Default)]
struct Acks {
    state: Mutex<AckState>,
}

#[derive(Default)]
struct AckState {
    /// The last stage operation sent.
    sent: u64,
    /// cladefs: the last sent that takes access to pages away, which the VM process may reach until
    /// it has made it (TASK-79.9).
    last_taking: u64,
    /// The last acknowledged.
    acked: u64,
    /// The VM process is gone, and nothing it acknowledges is waited for.
    closed: bool,
    /// cladefs: the VM process is wedged: killed, it has not exited ([`Peer::gone`], board 8535).
    wedged: bool,
    /// cladefs: what is kept until stage operations are acknowledged ([`Acks::park`]), by their
    /// IDs: what an acknowledgement lets go of taken off at once, however
    /// much waits for those after it.
    kept: std::collections::BTreeMap<u64, Vec<Arc<Parked>>>,
    /// cladefs: what the watchdog found unacknowledged ([`Acks::unanswered`]).
    watched: Option<Watched>,
}

impl AckState {
    /// The ID of the next stage operation, which takes access to pages away if `taking`.
    fn next(&mut self, taking: bool) -> u64 {
        self.sent += 1;
        if taking {
            self.last_taking = self.sent;
        }
        self.sent
    }
}

/// cladefs: how the watchdog last found a VM process with stage operations taking access away
/// unacknowledged.
struct Watched {
    /// The last acknowledged then, and when the watchdog first found it the last.
    acked: u64,
    answered: std::time::Instant,
    /// The last sent taking access away when the watchdog looked after the one before it was
    /// acknowledged, and when that was ([`Acks::behind`]).
    id: u64,
    since: std::time::Instant,
    /// The last acknowledged as the window its pace is measured over began, and when
    /// ([`Acks::pace`], TASK-249).
    paced: u64,
    paced_at: std::time::Instant,
    /// The operations it acknowledged over the last whole window of [`BEHIND`]: none until one
    /// ended.
    pace: Option<u64>,
}

impl Acks {
    /// The ID of `op`, the next stage operation sent, which the caller queues to send before it
    /// gives another ([`VmProcessStage::send_now`]).
    fn sent(&self, op: &StageOp) -> u64 {
        self.state.lock().unwrap().next(op.taking())
    }

    /// The VM process made stage operation `id`, and those before it; or, `id` past the last sent,
    /// the last sent, as it can't have (board 8463): `acked` never passes `sent`, which the rest
    /// of the quarantine and the watchdog take it for.
    fn acked(&self, id: u64) -> Result<(), u64> {
        let released = {
            let mut state = self.state.lock().unwrap();
            if id > state.sent {
                return Err(state.sent);
            }
            state.acked = state.acked.max(id);
            let after = state.acked.saturating_add(1);
            let kept = state.kept.split_off(&after);
            std::mem::replace(&mut state.kept, kept)
        };
        // What the VM process no longer reaches, dropped without holding the lock.
        drop(released);
        Ok(())
    }

    /// cladefs: keeps `pages`, freed, until the VM process has made the stage operations sent it so
    /// far that take access to pages away, any of which may have been of them (TASK-79.9). Held
    /// for a wedged VM process, if it is one (board 8535).
    fn park(&self, pages: &Arc<Parked>) {
        let mut state = self.state.lock().unwrap();
        if state.acked < state.last_taking && !state.closed {
            if state.wedged {
                pages.wedged();
            }
            let id = state.last_taking;
            state.kept.entry(id).or_default().push(pages.clone());
        }
    }

    /// cladefs: the VM process is wedged (board 8535): what is kept for it, and kept from now on,
    /// is held for it, which comes back only once it exits, if ever ([`Parked::wedged`]). The
    /// bytes kept for it.
    fn wedged(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.wedged = true;
        let pages: u64 = (state.kept.values().flatten())
            .map(|parked| {
                parked.wedged();
                parked.pages()
            })
            .sum();
        pages * cladefs_core::pages::page_size()
    }

    /// cladefs: whether the VM process has left an operation taking access away unacknowledged.
    fn unacknowledged(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.acked < state.last_taking
    }

    /// cladefs: how long, as of `now`, the VM process has acknowledged nothing with an operation
    /// taking access away unacknowledged, as the watchdog looks every so often (TASK-79.9): since
    /// it first found that so, or none. A backlog it works through is no silence: a burst of tens
    /// of thousands takes seconds, where each of them takes its time on a slow host (TASK-148).
    fn unanswered(&self, now: std::time::Instant) -> std::time::Duration {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.acked >= state.last_taking {
            state.watched = None;
            return std::time::Duration::ZERO;
        }
        let (acked, id) = (state.acked, state.last_taking);
        let watched = state.watched.get_or_insert(Watched {
            acked,
            answered: now,
            id,
            since: now,
            paced: acked,
            paced_at: now,
            pace: None,
        });
        if watched.acked != acked {
            watched.acked = acked;
            watched.answered = now;
        }
        if acked >= watched.id {
            watched.id = id;
            watched.since = now;
        }
        now.saturating_duration_since(watched.answered)
    }

    /// cladefs: how long, as of `now`, the operation taking access away that the watchdog is
    /// waiting for has been unacknowledged, whatever was acknowledged meanwhile, as of its last
    /// look ([`Acks::unanswered`]): an acknowledgement every so often holds nothing for ever
    /// (TASK-148).
    fn behind(&self, now: std::time::Instant) -> std::time::Duration {
        let state = self.state.lock().unwrap();
        let since = state.watched.as_ref().map(|watched| watched.since);
        since.map_or(std::time::Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    /// cladefs: how many operations the VM process acknowledged over the last whole window of
    /// [`BEHIND`] since the watchdog found it with operations taking access away unacknowledged
    /// ([`Acks::unanswered`]), as of `now`, a window ending as the first look after it does: none
    /// until one ended, or with nothing unacknowledged (TASK-249). With how many it has yet to
    /// acknowledge to have made the last taking access away.
    fn pace(&self, now: std::time::Instant) -> (u64, Option<u64>) {
        let mut state = self.state.lock().unwrap();
        let acked = state.acked;
        let backlog = state.last_taking.saturating_sub(acked);
        let Some(watched) = state.watched.as_mut() else { return (backlog, None) };
        if now.saturating_duration_since(watched.paced_at) >= BEHIND {
            watched.pace = Some(acked - watched.paced);
            (watched.paced, watched.paced_at) = (acked, now);
        }
        (backlog, watched.pace)
    }

    /// cladefs: what the watchdog finds of the VM process's acknowledgements as it looks, at `now`:
    /// [`Acks::unanswered`], then [`Acks::pace`] and [`Acks::behind`], which go by what that found.
    fn look(&self, now: std::time::Instant) -> Silence {
        let silent = self.unanswered(now);
        let (backlog, pace) = self.pace(now);
        Silence { silent, behind: self.behind(now), backlog, pace }
    }

    fn close(&self) {
        let kept = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            std::mem::take(&mut state.kept)
        };
        drop(kept);
    }
}

/// cladefs: what the watchdog found of a VM process's acknowledgements as it looked
/// ([`Acks::look`]): how long it has acknowledged nothing ([`Acks::unanswered`]), how long the
/// operation it waits for has been unacknowledged ([`Acks::behind`]), and how many operations it
/// has yet to acknowledge and acknowledged over the last window ([`Acks::pace`]). Said as the
/// daemon lets one go, it tells the two limits apart: one that stopped, and one behind that makes
/// no headway (TASK-136.1, TASK-249).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Silence {
    silent: std::time::Duration,
    behind: std::time::Duration,
    backlog: u64,
    pace: Option<u64>,
}

impl Silence {
    /// Whether it is past [`UNRESPONSIVE`] silent, or past [`BEHIND`] behind at a pace that would
    /// not catch up within [`CATCHING_UP`]: the VM process is to be let go. One behind that
    /// acknowledges as fast as a slow host lets it, however long it has been behind, catches up
    /// within that, and is kept (TASK-249): a cap on how long it may be behind let go of VMs
    /// acknowledging thousands of operations a second, under a load that kept them behind for
    /// 100 s.
    fn unresponsive(self) -> bool {
        self.silent >= UNRESPONSIVE || (self.behind >= BEHIND && !self.catching_up())
    }

    /// Whether, at the pace it acknowledged over the last window of [`BEHIND`], it clears its
    /// backlog within [`CATCHING_UP`]: so too with no window ended yet.
    fn catching_up(self) -> bool {
        self.pace.is_none_or(|pace| {
            u128::from(self.backlog) * BEHIND.as_millis()
                <= u128::from(pace) * CATCHING_UP.as_millis()
        })
    }
}

impl std::fmt::Display for Silence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { silent, behind, backlog, pace } = self;
        write!(f, "silent for {silent:?} (of {UNRESPONSIVE:?} allowed), behind for {behind:?}: ")?;
        write!(f, "{backlog} operations unacknowledged, ")?;
        match pace {
            Some(pace) => write!(f, "{pace} acknowledged in the last {BEHIND:?}")?,
            None => write!(f, "no pace measured yet")?,
        }
        write!(f, " (a pace clearing them within {CATCHING_UP:?} allowed)")
    }
}

/// cladefs: how long a VM process with a stage operation that takes access to pages away
/// unacknowledged may acknowledge nothing before the daemon takes it to have stopped answering,
/// and lets it go ([`Peer::let_go`], TASK-79.9): long past what making one takes, and well within
/// what an allocation waits for the pages freed meanwhile, which wait for it, before it fails.
pub(super) const UNRESPONSIVE: std::time::Duration = std::time::Duration::from_secs(5);

/// cladefs: how long a VM process that goes on acknowledging may be behind, over a backlog of stage
/// operations, before the daemon looks at its pace (TASK-148, TASK-249): long past the seconds a
/// burst of tens of thousands takes on a slow host, where each acknowledgement frees what it
/// waited for. Its pace is measured over windows as long.
const BEHIND: std::time::Duration = std::time::Duration::from_secs(60);

/// cladefs: how long a VM process behind for [`BEHIND`] may take to clear its backlog, at the pace
/// it acknowledged over the last [`BEHIND`], before the daemon lets it go (TASK-249): what keeps
/// one acknowledging just often enough not to be silent from holding every page freed for hours, as
/// one acknowledging an operation a second, tens of thousands behind, would. Every page freed
/// meanwhile waits for it, so not much longer. A VM process acknowledging thousands a second
/// catches up within it from any backlog the daemon queues, being let go only once it stalls.
const CATCHING_UP: std::time::Duration = std::time::Duration::from_secs(600);

/// cladefs: how long a VM process whose connection went, and that may still reach pages, has to
/// exit, as one exiting does just after its connection goes, before the daemon lets it go
/// ([`Peer::gone`], TASK-79.100): long past what the rest of an exit takes.
const EXITING: std::time::Duration = std::time::Duration::from_secs(5);

/// cladefs: how often the watchdog looks for VM processes that stopped answering ([`watch`]).
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// cladefs: a connection's VM process, as the daemon depends on it (TASK-79.9): the stage
/// operations it acknowledges, which the pages it may reach wait for, freed ([`quarantine`]); and
/// the process itself, which the daemon lets go once it has acknowledged none for
/// [`UNRESPONSIVE`], or stayed behind for [`BEHIND`] at a pace that would not catch up within
/// [`CATCHING_UP`] ([`watch`]): its connection shut down, it is killed as the connection goes,
/// which waits for it to have exited, its mappings of the window gone, before the pages it may
/// reach go back ([`Peer::gone`]). So too one that closes its connection and lives on, reaching
/// pages still (TASK-79.100). One the daemon could not kill it refuses as it connects
/// ([`Peer::connected`]): let go, it would map pages the daemon gives others; kept, every page
/// freed would wait for it.
pub(super) struct Peer {
    daemon: Arc<Daemon>,
    /// What the runs staged to it know it by (`slabs::PartSlab::stage_to`): the quarantine of each
    /// waits for the VM processes it was staged to alone (doc-26). Never another's.
    id: cladefs_core::slabs::VmProcessId,
    acks: Arc<Acks>,
    sender: Arc<AsyncSender>,
    /// Its process: none if it is this one, as tests' are, whose code the daemon's own is.
    process: Option<VmProcess>,
    /// Whether the daemon let it go ([`Peer::let_go`]).
    let_go: AtomicBool,
    /// The bytes a reply to `MAPPINGS` of its window takes at most, once it has one (TASK-79.43):
    /// a mapping a page of it ([`Peer::credit`]).
    mappings: std::sync::atomic::AtomicU64,
    /// cladefs: its tenant's ID, which what is said of it names.
    tenant: u64,
}

// Process-wide quarantine dispatch across live daemons (`VmProcessId` is unique in the process).
static DAEMONS: Mutex<Vec<Weak<Daemon>>> = Mutex::new(Vec::new());

/// Registers `daemon` for process-wide slab quarantine dispatch (`quarantine`).
pub(super) fn register_daemon(daemon: &Arc<Daemon>) {
    let mut daemons = DAEMONS.lock().unwrap();
    daemons.retain(|entry| entry.strong_count() > 0);
    daemons.push(Arc::downgrade(daemon));
}

impl Drop for Peer {
    /// Gone: the runs staged to it forget it ([`crate::slabs::VmProcessId::gone`]), as it has
    /// exited, its mappings with it, once its connection is forgotten ([`Peer::gone`]).
    fn drop(&mut self) {
        self.id.gone();
    }
}

impl Peer {
    /// The VM process at the other end of `stream`, which `sender` sends to, among those
    /// connected to `daemon`, until [`Peer::forget`]; with the quarantine of the pages freed that
    /// it, or another, may reach, and the watchdog, started first. An error if the daemon may not
    /// kill it, as it would once it stopped answering: its process unknown, or the daemon not
    /// allowed to signal it, as one of another user is unless the daemon has `CAP_KILL`.
    fn connected(
        daemon: &Arc<Daemon>,
        stream: &crate::wire::Stream,
        connector: Connector,
        sender: Arc<AsyncSender>,
        tenant: u64,
    ) -> std::io::Result<Arc<Self>> {
        daemon.watching_vm_peers.call_once(|| {
            // cladefs: physical memory's quarantine is the process's, given once, whichever
            // simulated host is first (TASK-685.14); the watchdog is each daemon's.
            // Process-wide registration for slabs::quarantine_with.
            static QUARANTINED: std::sync::Once = std::sync::Once::new();
            QUARANTINED.call_once(|| cladefs_core::slabs::quarantine_with(quarantine));
            // cladefs: supervised, its panic aborting the daemon (board 9414): VM processes that
            // stopped answering would be let go never again, holding the pages freed for good.
            daemon.spawn(crate::run::supervising(WATCHDOG, watch(Arc::downgrade(daemon))));
        });
        let process = match connector {
            Connector::Socket => peer_process(stream)?,
            #[cfg(not(madsim))]
            Connector::Child(child) => Some(VmProcess::child(child)?),
        };
        if let Some(process) = &process {
            // Signal 0, which checks the right to signal it, and sends nothing.
            process.signal(0).map_err(|err| {
                std::io::Error::new(err.kind(), format!("the daemon may not kill it: {err}"))
            })?;
        }
        let peer = Arc::new(Self {
            daemon: Arc::clone(daemon),
            id: cladefs_core::slabs::VmProcessId::connected(),
            acks: Arc::default(),
            sender,
            process,
            let_go: AtomicBool::new(false),
            mappings: Default::default(),
            tenant,
        });
        daemon.vm_peers.lock().unwrap().push(peer.clone());
        Ok(peer)
    }

    /// What of what it sends past what it was answered the daemon holds at most (TASK-79.16): its
    /// requests' stream data, as the protocol bounds it ([`MAX_STREAM_BYTES`]), and what the
    /// daemon queued for it besides ([`QUEUED_CREDIT`]), its window's mappings among them, as many
    /// as the window may have ([`Peer::set_window`]): its credit.
    fn credit(&self) -> u64 {
        MAX_STREAM_BYTES + QUEUED_CREDIT + self.mappings.load(Ordering::Relaxed)
    }

    /// Its window is of `size` bytes, whose mappings, a logical block each at most, a reply to
    /// `MAPPINGS` holds (TASK-79.43).
    fn set_window(&self, size: u64) {
        assert!(size <= MAX_WINDOW, "a window past the most the daemon gives credit for");
        let mappings = size / cladefs_core::dax::map_unit() * MAPPING_BYTES;
        self.mappings.store(mappings, Ordering::Relaxed);
    }

    /// Its connection is gone: no page waits for it from now on.
    fn forget(self: &Arc<Self>) {
        let forgotten: Vec<_> = self
            .daemon
            .vm_peers
            .lock()
            .unwrap()
            .extract_if(.., |peer| Arc::ptr_eq(peer, self))
            .collect();
        // Dropped without holding the lock, as what it frees may be quarantined.
        drop(forgotten);
    }

    /// Lets it go, `why`: shuts its connection down, whose going kills it ([`Peer::gone`]).
    fn let_go(&self, why: std::fmt::Arguments<'_>) {
        if !self.let_go.swap(true, Ordering::AcqRel) {
            log::error!("cladefsd: letting a VM process go, as it {why}");
            self.sender.shutdown();
        }
    }

    /// How it has been silent, if it has acknowledged no operation for [`UNRESPONSIVE`], with one
    /// taking access away unacknowledged, or left one so for [`BEHIND`] at a pace that would not
    /// catch up within [`CATCHING_UP`], as of `now`, and is to be let go ([`Silence`]).
    fn unresponsive(&self, now: std::time::Instant) -> Option<Silence> {
        Some(self.acks.look(now)).filter(|silence| silence.unresponsive())
    }

    /// Returns once its process has exited, holding no thread, its memory gone, and its mappings
    /// of the window; never, if it is this process.
    async fn exited(&self) -> std::io::Result<()> {
        let Some(process) = &self.process else { return std::future::pending().await };
        process.exited().await
    }

    /// Its connection is gone, the process perhaps not: `reaching` pages, which go back once it
    /// is, it has [`EXITING`] to go, as it would were it exiting, and is let go otherwise. If the
    /// daemon let it go, kills it and returns once it has exited ([`Peer::exited`]). cladefs: one
    /// not exited [`KILLED_EXITING`] after, wedged, is said once, with its tenant and what is held
    /// for it, and counted ([`wedged`]) until it exits; what is held for it, waited for still, is no
    /// room on its way from then on, which shrinks and relief wait for (board 8535).
    async fn gone(&self, reaching: bool) {
        let Some(process) = &self.process else { return };
        if reaching && !self.let_go.load(Ordering::Acquire) {
            if let Ok(exited) = tokio::time::timeout(EXITING, self.exited()).await {
                if let Err(err) = exited {
                    log::error!("cladefsd: waiting for a VM process to exit: {err}");
                }
                return;
            }
            self.let_go(format_args!("closed its connection, reaching pages still"));
        }
        if !self.let_go.load(Ordering::Acquire) {
            return;
        }
        // Which the daemon may, as it checked as the process connected (`Peer::connected`).
        if let Err(err) = process.signal(libc::SIGKILL) {
            log::error!("cladefsd: killing a VM process: {err}");
        }
        let exited = match tokio::time::timeout(KILLED_EXITING, self.exited()).await {
            Ok(exited) => exited,
            // Not exited within the bound, the timeout's error: it is wedged, waited for still.
            Err(_) => {
                let held = self.acks.wedged();
                let _wedged = Wedged::begin(&self.daemon);
                log::error!(
                    "cladefsd: a VM process of tenant {} has not exited {KILLED_EXITING:?} after the \
                     daemon killed it, as one in uninterruptible sleep or a frozen cgroup would not: \
                     {held} bytes of memory are held for it until it does",
                    self.tenant
                );
                self.exited().await
            }
        };
        if let Err(err) = exited {
            log::error!("cladefsd: waiting for a VM process to exit: {err}");
        }
    }
}

/// cladefs: how long a VM process the daemon killed has to exit before it counts as wedged, as in
/// uninterruptible sleep, or a frozen cgroup, where `SIGKILL` waits (board 8535): long past what
/// tearing down a large guest's memory takes (hotbed question 484).
pub(super) const KILLED_EXITING: std::time::Duration = std::time::Duration::from_secs(10);

/// cladefs: a wedged VM process, counted on its daemon until dropped, as it exits at last
/// ([`Peer::gone`]).
struct Wedged {
    daemon: Arc<Daemon>,
}

impl Wedged {
    fn begin(daemon: &Arc<Daemon>) -> Self {
        daemon.wedged.fetch_add(1, Ordering::Relaxed);
        Self { daemon: Arc::clone(daemon) }
    }
}

impl Drop for Wedged {
    fn drop(&mut self) {
        self.daemon.wedged.fetch_sub(1, Ordering::Relaxed);
    }
}

/// cladefs: how many VM processes `daemon` killed have not exited [`KILLED_EXITING`] after
/// (board 8535): what the metrics show, with the memory held for them
/// (`crate::slabs::wedged_bytes`).
pub(super) fn wedged_on(daemon: &Daemon) -> u64 {
    daemon.wedged.load(Ordering::Relaxed)
}

#[cfg(all(test, not(madsim)))]
pub(super) fn wedged() -> u64 {
    wedged_on(Daemon::process())
}

/// cladefs: a VM process, other than this one, as a pidfd; on macOS, which has none, as its pid
/// and a kqueue watching it exit, made as it connected (board 13065, hotbed question 567). Under
/// madsim, none: a simulated guest is in the simulation's process ([`peer_process`]), so the type
/// has no values (doc-24, board 2912).
#[cfg(not(madsim))]
enum VmProcess {
    #[cfg(target_os = "linux")]
    Pidfd(std::os::fd::OwnedFd),
    /// cladefs: its pid, and a kqueue holding an event once it exited (`EVFILT_PROC`,
    /// `NOTE_EXIT`), readable then, as a pidfd is. Signalled only while that holds none: a pid
    /// is another process's only once the one it named exited and its parent reaped it, and the
    /// VM processes an embedder starts are its own children (board 13460).
    #[cfg(target_os = "macos")]
    Pid { pid: libc::pid_t, exits: std::os::fd::OwnedFd },
    /// cladefs: in a test, this process, as one that never exits, however it is killed: as one in
    /// uninterruptible sleep, or a frozen cgroup, which no test can make (board 8535,
    /// [`UNKILLABLE`]).
    #[cfg(test)]
    Unkillable,
}
#[cfg(madsim)]
enum VmProcess {}

/// cladefs: whether a test's VM processes connecting, this process's, are [`VmProcess::Unkillable`]:
/// the process's, a test's, which runs in a process of its own.
#[cfg(test)]
pub(super) static UNKILLABLE: AtomicBool = AtomicBool::new(false);

impl VmProcess {
    /// Sends it `signal`: 0 sends none, but checks that this process may signal it.
    #[cfg(all(target_os = "linux", not(madsim)))]
    fn signal(&self, signal: libc::c_int) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let pidfd = match self {
            Self::Pidfd(pidfd) => pidfd,
            #[cfg(test)]
            Self::Unkillable => return Ok(()),
        };
        // SAFETY: Signals the process the pidfd, which it keeps open, names.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// On macOS, checks that this process may signal it, as signal 0 does on Linux, and sends no
    /// other: a pid names no process surely (no pidfd), its process perhaps exited and reaped and
    /// the pid another's between a check and the kill, which would end that other. Nothing it maps
    /// needs it killed on macOS, whose VM processes map no window yet (board 13069); a VM process
    /// let go there is waited for until it exits (board 13622).
    #[cfg(all(target_os = "macos", not(madsim)))]
    fn signal(&self, signal: libc::c_int) -> std::io::Result<()> {
        use std::os::fd::AsFd;
        let (pid, exits) = match self {
            Self::Pid { pid, exits } => (*pid, exits),
            #[cfg(test)]
            Self::Unkillable => return Ok(()),
        };
        if signal != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "macOS names no process surely to kill (board 13622)",
            ));
        }
        if readable_now(exits.as_fd())? {
            return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
        }
        // SAFETY: Signal 0 sends nothing: it checks the pid's process may be signalled.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// cladefs: the child of this process's of the pid `child`, not reaped yet, so that its pid is
    /// surely its: a pidfd of it (`pidfd_open`); on macOS, a kqueue watching it exit
    /// ([`VmProcess::Pid`]).
    #[cfg(all(target_os = "linux", not(madsim)))]
    fn child(child: libc::pid_t) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: A plain system call making a descriptor of the process, which it returns.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child, 0) };
        if pidfd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pidfd = libc::c_int::try_from(pidfd).map_err(std::io::Error::other)?;
        // SAFETY: A new descriptor, which the call gave this process.
        Ok(Self::Pidfd(unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) }))
    }

    /// cladefs: [`VmProcess::child`] on macOS.
    #[cfg(all(target_os = "macos", not(madsim)))]
    fn child(child: libc::pid_t) -> std::io::Result<Self> {
        Ok(Self::Pid { pid: child, exits: exits_of(child)? })
    }

    /// Returns once it has exited, holding no thread.
    #[cfg(not(madsim))]
    async fn exited(&self) -> std::io::Result<()> {
        use std::os::fd::AsFd;
        let pidfd = match self {
            #[cfg(target_os = "linux")]
            Self::Pidfd(pidfd) => pidfd,
            #[cfg(target_os = "macos")]
            Self::Pid { exits, .. } => exits,
            #[cfg(test)]
            Self::Unkillable => return std::future::pending().await,
        };
        // A pidfd is readable once its process has exited; on macOS, its kqueue, once it holds
        // the exit's event.
        // SAFETY: `pidfd` is an `OwnedFd` owned by `self` and outlives `exited`, so the descriptor
        // stays valid and open until `AsyncFd` drops and deregisters it from the reactor.
        let exited = unsafe {
            tokio::io::unix::AsyncFd::register_with_interest(
                pidfd.as_fd(),
                tokio::io::Interest::READABLE,
            )
        };
        exited?.readable().await.map(drop)
    }

    #[cfg(madsim)]
    fn signal(&self, _: libc::c_int) -> std::io::Result<()> {
        match *self {}
    }

    #[cfg(madsim)]
    async fn exited(&self) -> std::io::Result<()> {
        match *self {}
    }

    /// Race-free PID of this VM process read from `/proc/self/fdinfo/<pidfd>`. Returns `None` once
    /// the process has exited (`Pid: 0` or `-1`) or outside Linux.
    #[cfg(all(target_os = "linux", not(madsim)))]
    fn pid(&self) -> Option<libc::pid_t> {
        use std::os::fd::AsRawFd;
        let pidfd = match self {
            Self::Pidfd(pidfd) => pidfd,
            #[cfg(test)]
            Self::Unkillable => return None,
        };
        // Real Linux procfs fdinfo for a connected VM's pidfd, outside the simulation; unreadable
        // if the process exited or procfs is restricted, so skip it.
        let fdinfo =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd())).ok()?;
        let pid_str = fdinfo.lines().find_map(|line| line.strip_prefix("Pid:"))?.trim();
        // Malformed or negative Pid line (exited pidfd): treat as no live PID.
        let pid = pid_str.parse::<libc::pid_t>().ok()?;
        (pid > 0).then_some(pid)
    }
}

/// Live unified cgroup v2 relative paths of external VM processes currently connected to `daemon`,
/// resolved race-free by holding each peer's `SO_PEERPIDFD` open across `/proc/<pid>/cgroup` and
/// re-checking `fdinfo` `Pid:` afterwards so a recycled PID can never target an unrelated cgroup.
#[cfg(all(target_os = "linux", not(madsim)))]
pub(super) fn connected_vm_cgroups(daemon: &Daemon) -> Vec<String> {
    let peers: Vec<Arc<Peer>> = daemon.vm_peers.lock().unwrap().clone();
    let mut paths = Vec::new();
    for peer in peers {
        let Some(process) = peer.process.as_ref() else { continue };
        let Some(pid_before) = process.pid() else { continue };
        // Real Linux procfs cgroup lookup for a connected VM process, outside the simulation;
        // unreadable if the process exited concurrently, so skip it.
        let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid_before}/cgroup")) else {
            continue;
        };
        // Re-verify the pidfd still points to the same live PID after reading `/proc/<pid>/cgroup`.
        if process.pid() != Some(pid_before) {
            continue;
        }
        if let Some(rel) = crate::governor::cgroup_v2_path_of(&raw) {
            paths.push(rel.to_owned());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// cladefs: the process at the other end of `stream`, as a pidfd, unless it is this one: the
/// socket's (`SO_PEERPIDFD`, Linux 6.5), which names the process that connected whatever became of
/// its pid. A kernel without it the daemon refuses at its start ([`require_peer_pidfds`], board
/// 8533): opening the pid `SO_PEERCRED` keeps (`pidfd_open`) raced a VM process that exits leaving
/// its socket to another, as a child, its pid then another process's, which the daemon would kill
/// in its place. Without it here, as in a test serving VM processes without booting a daemon, the
/// connection is refused.
#[cfg(madsim)]
fn peer_process(_: &crate::wire::Stream) -> std::io::Result<Option<VmProcess>> {
    // cladefs: a simulated guest's, in this process, as a test's is (doc-24).
    Ok(None)
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn peer_process(stream: &crate::wire::Stream) -> std::io::Result<Option<VmProcess>> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: Reads the peer's credentials into `credentials`, of `len` bytes.
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut len,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if credentials.pid == std::process::id() as libc::pid_t {
        // cladefs: or, as a test asks, one that never exits (board 8535).
        #[cfg(test)]
        if UNKILLABLE.load(Ordering::Relaxed) {
            return Ok(Some(VmProcess::Unkillable));
        }
        return Ok(None);
    }
    Ok(Some(VmProcess::Pidfd(peer_pidfd(stream.as_fd())?)))
}

/// cladefs: the process at the other end of `stream`, unless it is this one, as on Linux: on
/// macOS, its pid (`LOCAL_PEERPID`), watched at once for its exit by a kqueue of its own
/// ([`VmProcess::Pid`]). One that exited already (`ESRCH`) is refused, as its pid is no longer
/// surely its.
#[cfg(all(target_os = "macos", not(madsim)))]
fn peer_process(stream: &crate::wire::Stream) -> std::io::Result<Option<VmProcess>> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: Reads the peer's pid into `pid`, of `len` bytes.
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid == std::process::id() as libc::pid_t {
        // cladefs: or, as a test asks, one that never exits (board 8535).
        #[cfg(test)]
        if UNKILLABLE.load(Ordering::Relaxed) {
            return Ok(Some(VmProcess::Unkillable));
        }
        return Ok(None);
    }
    Ok(Some(VmProcess::Pid { pid, exits: exits_of(pid)? }))
}

/// cladefs: a kqueue holding an event once the process `pid` exited (`EVFILT_PROC`, `NOTE_EXIT`),
/// readable then, as a pidfd is: `ESRCH` if it exited already.
#[cfg(all(target_os = "macos", not(madsim)))]
fn exits_of(pid: libc::pid_t) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: Makes a kqueue, owned below.
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Just made, and owned here.
    let exits = unsafe { std::os::fd::OwnedFd::from_raw_fd(queue) };
    // SAFETY: A zeroed kevent is a valid empty one.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = pid as libc::uintptr_t;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    // SAFETY: Registers the process `pid` with the kqueue, waiting for nothing.
    let added = unsafe {
        libc::kevent(exits.as_raw_fd(), &change, 1, std::ptr::null_mut(), 0, std::ptr::null())
    };
    if added != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(exits)
}

/// cladefs: whether `fd` is readable now, without waiting: on macOS, whether a VM process's
/// kqueue holds its exit ([`VmProcess::Pid`]).
#[cfg(all(target_os = "macos", not(madsim)))]
fn readable_now(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: Polls a descriptor the caller keeps open, without waiting.
    match unsafe { libc::poll(&mut poll, 1, 0) } {
        ready if ready < 0 => Err(std::io::Error::last_os_error()),
        ready => Ok(ready > 0),
    }
}

/// cladefs: the pidfd of the process at the other end of the Unix socket `socket`
/// (`SO_PEERPIDFD`): `ENOPROTOOPT` said as needing Linux 6.5 (board 8533).
#[cfg(all(target_os = "linux", not(madsim)))]
fn peer_pidfd(socket: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut pidfd: libc::c_int = -1;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: Reads the peer's pidfd into `pidfd`, of `len` bytes.
    let read = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&raw mut pidfd).cast(),
            &mut len,
        )
    };
    if read != 0 {
        let err = std::io::Error::last_os_error();
        return Err(match err.raw_os_error() {
            Some(libc::ENOPROTOOPT) => std::io::Error::new(
                err.kind(),
                format!(
                    "the kernel has no SO_PEERPIDFD ({err}): the extent daemon needs Linux 6.5 or \
                     later, to know which process to kill of a VM process's socket"
                ),
            ),
            _ => err,
        });
    }
    // SAFETY: A new descriptor, which the call gave this process.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) })
}

/// cladefs: whether the kernel says which process is at the other end of a Unix socket
/// (`SO_PEERPIDFD`, Linux 6.5), as the daemon needs to serve VM processes ([`peer_process`]): asked
/// once, as it starts, of a pair of sockets of its own, so that a daemon on an older kernel refuses
/// to start, naming the version, rather than refuse every VM process (board 8533). Under madsim,
/// whose guests are in the simulation's process, nothing to ask; nor on macOS, which says a peer's
/// pid always (`LOCAL_PEERPID`, [`VmProcess::Pid`]).
pub(super) fn require_peer_pidfds() -> std::io::Result<()> {
    #[cfg(all(target_os = "linux", not(madsim)))]
    {
        let (ours, _theirs) = std::os::unix::net::UnixStream::pair()?;
        drop(peer_pidfd(ours.as_fd())?);
    }
    Ok(())
}

/// cladefs: what of the guest's access a fault the window resolved as `faulted` refused, if it
/// refused any, to tell of: an access to a page mapped nothing at. A probe ([`FAULT_PROBE`]) is
/// the VM process's own, of the page after one a write faulted on, which the guest may map nothing
/// at: its refusal refuses the guest nothing. Told of as the guest's, it filled cladefsd's log
/// with refusals no access of the guest's met (r544). The words are the log's, which
/// `scripts/dax-refusals.py` reads. A write refused, a "store" there, is the VM process's to tell
/// of now, which copies on write itself (doc-26).
fn refusal(fault: &Fault, faulted: Result<(), Errno>) -> Option<&'static str> {
    match faulted {
        _ if fault.probe => None,
        Err(Errno::ENOENT) => Some("an access to nothing mapped"),
        _ => None,
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::{Fault, refusal};
    use cladefs_core::Errno;

    /// cladefs: a probe of a page mapped nothing at, the VM process's own after a write's fault,
    /// refuses the guest nothing, and isn't told of; the guest's own access to it is. Red with
    /// probes told of as the guest's accesses.
    #[test]
    fn probes_refuse_the_guest_nothing() {
        let probe = Fault { offset: 0x1000, probe: true };
        let guests = Fault { probe: false, ..probe };
        let nothing_mapped = Err(Errno::ENOENT);
        assert_eq!(refusal(&probe, nothing_mapped), None);
        assert_eq!(refusal(&guests, nothing_mapped), Some("an access to nothing mapped"));
        assert_eq!(refusal(&guests, Ok(())), None);
    }
}

/// cladefs: quarantines `freed`, a run freed, which the VM processes it was staged to may still
/// reach, as a stage operation taking it away may be unacknowledged (`slabs::quarantine_with`,
/// TASK-79.9): until each of them still connected has made those it was sent so far, or gone
/// (doc-26, spec rules waitsForStaged and scopedWaits). No other VM process holds it back: a
/// tenant's runs are staged to that tenant's VM processes alone, so another tenant's frozen VM
/// never holds them; a public base's, to those that mapped it. One staged that is gone has exited,
/// its mappings with it ([`Peer::gone`]). The run is freed as the last of them lets go: at once, if
/// none waits.
fn quarantine(freed: cladefs_core::slabs::Quarantined) {
    let live: Vec<Arc<Daemon>> = {
        let mut daemons = DAEMONS.lock().unwrap();
        let mut live = Vec::with_capacity(daemons.len());
        daemons.retain(|entry| match entry.upgrade() {
            Some(daemon) => {
                live.push(daemon);
                true
            }
            None => false,
        });
        live
    };
    let tid = std::thread::current().id();
    let waiting = live.iter().find_map(|daemon| {
        daemon
            .parking
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _)| *id == tid)
            .map(|(_, waiting)| Arc::clone(waiting))
    });
    if let Some(waiting) = &waiting {
        waiting.parked.fetch_add(1, Ordering::SeqCst);
    }
    let staged = freed.staged().to_vec();
    let counted = AtomicBool::new(waiting.is_some());
    let freed = Arc::new(Parked { freed: Some(freed), waiting, counted });
    for daemon in &live {
        for peer in daemon.vm_peers.lock().unwrap().iter().filter(|peer| staged.contains(&peer.id))
        {
            peer.acks.park(&freed);
        }
    }
}

/// cladefs: a run quarantined ([`quarantine`]), freed as it is dropped; and, if it was freed by
/// what counts what it frees, what it waits on ([`Parking`]), and whether that counts it still.
struct Parked {
    freed: Option<cladefs_core::slabs::Quarantined>,
    waiting: Option<Arc<Waiting>>,
    counted: AtomicBool,
}

impl Parked {
    /// cladefs: it is held for a wedged VM process (board 8535): no room on its way, for relief
    /// ([`crate::slabs::Quarantined::wedged`]), nor for the shrink that freed it, which waits for
    /// it no more.
    fn wedged(&self) {
        if let Some(freed) = &self.freed {
            freed.wedged();
        }
        self.uncount();
    }

    /// cladefs: its pages.
    fn pages(&self) -> u64 {
        self.freed.as_ref().map_or(0, cladefs_core::slabs::Quarantined::pages)
    }

    /// cladefs: what it waits on counts it no more, if it did.
    fn uncount(&self) {
        if let Some(waiting) = &self.waiting
            && self.counted.swap(false, Ordering::SeqCst)
        {
            let prev = waiting.parked.fetch_sub(1, Ordering::SeqCst);
            waiting.freed.notify(usize::MAX);
            if prev == 1
                && let Some(wake) = &waiting.wake
            {
                wake();
            }
        }
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        drop(self.freed.take());
        self.uncount();
    }
}

/// cladefs: how many batches of the pages a shrink freed are quarantined still ([`Parking`]), and
/// what says one of them is free.
#[derive(Default)]
pub(super) struct Waiting {
    parked: std::sync::atomic::AtomicUsize,
    freed: event_listener::Event,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// cladefs: has the pages a shrink frees, quarantined ([`quarantine`]), counted until they are
/// free, which [`Parking::wait`] (async) or the unified eviction thread (sync) waits for
/// (TASK-79.9, board 11703).
pub(super) struct Parking(Arc<Waiting>);

impl Parking {
    /// Counts nothing yet: what is freed in [`Parking::counting`] alone.
    pub(super) fn new() -> Self {
        Self(Arc::default())
    }

    /// Counts quarantined runs and invokes `wake` once all counted runs are released (board 11703).
    #[cfg(not(madsim))]
    pub(super) fn with_wake(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self(Arc::new(Waiting {
            parked: std::sync::atomic::AtomicUsize::new(0),
            freed: event_listener::Event::new(),
            wake: Some(wake),
        }))
    }

    /// cladefs: `free`'s output, counting the pages it frees on this thread on `daemon` as it runs
    /// (board 2946, board 11703).
    pub(super) fn counting<T>(&self, daemon: &Daemon, free: impl FnOnce() -> T) -> T {
        struct Restore<'a>(&'a Daemon, std::thread::ThreadId);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let mut parking = self.0.parking.lock().unwrap();
                if let Some(pos) = parking.iter().rposition(|(tid, _)| *tid == self.1) {
                    parking.remove(pos);
                }
            }
        }
        let tid = std::thread::current().id();
        daemon.parking.lock().unwrap().push((tid, Arc::clone(&self.0)));
        let _restore = Restore(daemon, tid);
        free()
    }

    /// Whether pages it counted are quarantined still.
    pub(super) fn parked(&self) -> bool {
        self.0.parked.load(Ordering::SeqCst) > 0
    }

    /// Waits until the pages it counted are free: once every VM process that may reach them has
    /// made the stage operations taking them away, or the daemon let it go, which it does once it
    /// has stopped answering ([`Peer::unresponsive`]), and it exited. cladefs: holding no thread
    /// (board 2946): it listens before it looks again, so that a batch freed between the two wakes
    /// it. cladefs: or until those left are held for wedged VM processes, which may never exit
    /// ([`Parked::wedged`], board 8535); its callers bound it besides (`memory::evict_until`).
    pub(super) async fn wait(&self) {
        while self.parked() {
            let freed = self.0.freed.listen();
            if !self.parked() {
                return;
            }
            freed.await;
        }
    }
}

/// The most bytes the daemon has queued for a VM process connected, which its socket has not
/// taken.
#[cfg(all(test, not(madsim)))]
#[cfg(target_os = "linux")]
pub(super) fn most_queued() -> u64 {
    Daemon::process()
        .vm_peers
        .lock()
        .unwrap()
        .iter()
        .map(|peer| peer.sender.queued())
        .max()
        .unwrap_or(0)
}

/// cladefs: lets the VM processes on `daemon` that stopped answering go ([`Peer::unresponsive`]),
/// or reading what the daemon sends them ([`AsyncSender::stalled`], board 8459), looking every
/// [`WATCH_EVERY`], for as long as `daemon` lives.
/// cladefs: what the VM processes' watchdog ([`watch`]) is called, as a panic in it names it
/// (board 9414).
const WATCHDOG: &str = "cladefsd watchdog";

async fn watch(daemon: Weak<Daemon>) {
    let mut every = tokio::time::interval(WATCH_EVERY);
    loop {
        every.tick().await;
        let Some(daemon) = daemon.upgrade() else { break };
        let now = std::time::Instant::now();
        let (unresponsive, stalled): (Vec<_>, Vec<_>) = {
            let peers = daemon.vm_peers.lock().unwrap();
            let unresponsive = (peers.iter())
                .filter_map(|peer| Some((peer.clone(), peer.unresponsive(now)?)))
                .collect();
            // cladefs: and those whose sockets took nothing of what is queued for them for
            // [`STALL`], which their tenant's bytes in flight held for good, whatever else they
            // said (board 8459).
            let stall = stall_bound();
            let stalled = (peers.iter())
                .map(|peer| (peer.clone(), peer.sender.stalled(now)))
                .filter(|(_, stalled)| *stalled >= stall)
                .collect();
            (unresponsive, stalled)
        };
        // Let go without holding the VM processes connected, which every page freed takes.
        for (peer, silence) in unresponsive {
            let queued = peer.sender.queued();
            peer.let_go(format_args!(
                "stopped answering stage operations: {silence}, {queued} bytes queued for it"
            ));
        }
        let stall = stall_bound();
        for (peer, stalled) in stalled {
            let queued = peer.sender.queued();
            peer.let_go(format_args!(
                "read nothing of the {queued} bytes queued for it for {stalled:?}, of {stall:?} \
                 allowed"
            ));
        }
    }
}

/// A window's second stage in a VM process's memory: its changes go to the VM process, which makes
/// them in order before it takes the replies that follow them. cladefs: none is waited for, as the
/// kernel makes them with its locks held, which a VM process that stopped answering would hold for
/// good (TASK-79.9): one that takes access to pages away is acknowledged, and the guest has no
/// access to a page the kernel frees, reuses or shares next, as what was freed waits for it
/// ([`quarantine`]).
struct VmProcessStage {
    sender: Arc<AsyncSender>,
    acks: Arc<Acks>,
    /// Its VM process: what the runs it maps are staged to (doc-26).
    vm: cladefs_core::slabs::VmProcessId,
    /// cladefs: the operations of the change the window is making, if it is, held back to be sent
    /// together as it ends (TASK-91.16). The window makes its changes, and every call here, under
    /// its lock (`SecondStage::begin_change`): nothing else of the window's comes between, nor
    /// does any wait, so that the guest's access to a page waits for no operation held back
    /// (TASK-160.11).
    change: SharedLock<Mutex<Option<Vec<StageOp>>>>,
}

impl VmProcessStage {
    /// Sends the stage operation `op`, or holds it back with the change being made.
    fn send(&self, op: StageOp) {
        let mut change = self.change.with(Mutex::lock);
        match change.as_mut() {
            Some(ops) => ops.push(op),
            None => self.send_now(vec![op]),
        }
    }

    /// Coalesces `ops` (folding unmap+mapped+map runs into atomic `STAGE_REMAP` messages, board
    /// 11673) and sends them at once, in order, each with the next ID ([`Acks::sent`]), given as
    /// they are queued, so that IDs reach the VM process in order: the VM process acknowledges
    /// those that take access to pages away ([`StageOp::taking`]), each for every ID before it.
    fn send_now(&self, ops: Vec<StageOp>) {
        let coalesced = cladefs_wire::codec::coalesce_stage_ops(ops);
        let stages = || {
            coalesced
                .iter()
                .map(|op| Stage { id: self.acks.sent(op), op: op.clone() }.encode())
                .collect()
        };
        // Failing, the VM process is gone, and its mappings with it.
        let _ = self.sender.send_all(stages);
    }
}

impl SecondStage for VmProcessStage {
    fn vm_process(&self) -> Option<cladefs_core::slabs::VmProcessId> {
        Some(self.vm)
    }

    fn map(&self, offset: u64, phys: cladefs_core::pages::PhysLocation, len: u64) {
        self.send(StageOp::Map { offset, phys, len });
        cladefs_core::stats::add(cladefs_core::stats::Counter::StageMaps, 1);
        cladefs_core::stats::add(cladefs_core::stats::Counter::StageMapBytes, len);
    }

    fn begin_change(&self) {
        let before = self.change.with(Mutex::lock).replace(Vec::new());
        assert!(before.is_none(), "a window's change begun in another");
    }

    /// cladefs: as a change's drop ends it: during a panic, which a window's lock held has aborted
    /// the daemon for, unless a test's window, panicking no more (TASK-149.24).
    fn end_change(&self) {
        let mut change = self.change.with(Mutex::lock);
        let Some(ops) = change.take() else {
            assert!(std::thread::panicking(), "a window's change ended, not begun");
            return;
        };
        if !ops.is_empty() {
            self.send_now(ops);
        }
    }

    fn unmap(&self, offset: u64, len: u64, private: cladefs_core::dax::Private) {
        let keep_private = private == cladefs_core::dax::Private::Keep;
        self.send(StageOp::Unmap { offset, len, keep_private });
    }

    fn window_mapped(&self, offset: u64, len: u64) {
        self.send(StageOp::Mapped { offset, len });
    }
}

#[cfg(test)]
mod acks_tests {
    use super::{Acks, BEHIND, Silence};
    use std::sync::Arc;

    /// The ID of the next stage operation sent, which takes access to pages away if `taking`.
    fn next(acks: &Acks, taking: bool) -> u64 {
        acks.state.lock().unwrap().next(taking)
    }

    /// TASK-79.9: pages freed are parked until the VM process has made what it was sent so far
    /// that takes access to pages away, not what it was sent after, nor what gives access; and
    /// not at all with none of that unacknowledged, or once the VM process is gone.
    #[test]
    fn parked_until_what_takes_access_away_is_acknowledged() {
        let acks = Acks::default();
        let pages = Arc::new(super::Parked {
            freed: None,
            waiting: None,
            counted: std::sync::atomic::AtomicBool::new(false),
        });
        let mapped = next(&acks, false);
        acks.park(&pages);
        assert_eq!(Arc::strong_count(&pages), 1, "parked behind a map");
        let unmapped = next(&acks, true);
        acks.park(&pages);
        let after = next(&acks, true);
        assert_eq!(Arc::strong_count(&pages), 2);
        acks.acked(mapped).unwrap();
        assert_eq!(Arc::strong_count(&pages), 2);
        acks.acked(unmapped).unwrap();
        assert_eq!(Arc::strong_count(&pages), 1, "parked behind what came after");
        acks.park(&pages);
        assert_eq!(Arc::strong_count(&pages), 2);
        acks.close();
        assert_eq!(Arc::strong_count(&pages), 1, "parked once the VM process is gone");
        acks.park(&pages);
        assert_eq!(Arc::strong_count(&pages), 1);
        assert!(after > unmapped);
    }

    /// A backlog a VM process works through is no silence, however long it takes (fxfs-guest-checks
    /// lost VMs that acknowledged 10,900 operations a second, 46,000 behind): it counts once the
    /// VM process stops.
    #[test]
    fn a_backlog_worked_through_is_answered() {
        let acks = Acks::default();
        let start = std::time::Instant::now();
        let at = |seconds| start + std::time::Duration::from_secs(seconds);
        let first = next(&acks, true);
        (0..50_000).for_each(|_| {
            next(&acks, true);
        });
        assert_eq!(acks.unanswered(at(0)), std::time::Duration::ZERO);
        for second in 1..=20 {
            acks.acked(first + second * 1000).unwrap();
            assert_eq!(acks.unanswered(at(second)), std::time::Duration::ZERO);
        }
        assert_eq!(acks.unanswered(at(25)), std::time::Duration::from_secs(5));
    }

    /// A backlog counts as behind from when the watchdog found it until it is cleared, however
    /// often the VM process acknowledges meanwhile.
    #[test]
    fn a_backlog_counts_as_behind_until_it_is_cleared() {
        let acks = Acks::default();
        let start = std::time::Instant::now();
        let at = |seconds| start + std::time::Duration::from_secs(seconds);
        let first = next(&acks, true);
        let last = (0..1000).map(|_| next(&acks, true)).last().unwrap();
        assert_eq!(acks.behind(at(0)), std::time::Duration::ZERO);
        acks.unanswered(at(0));
        for second in 1..=60 {
            acks.acked(first + second).unwrap();
            assert_eq!(acks.unanswered(at(second)), std::time::Duration::ZERO);
        }
        assert_eq!(acks.behind(at(60)), BEHIND);
        // Cleared, what comes after counts on its own.
        acks.acked(last).unwrap();
        let next = next(&acks, true);
        assert_eq!(acks.unanswered(at(61)), std::time::Duration::ZERO);
        assert_eq!(acks.behind(at(61)), std::time::Duration::ZERO);
        acks.acked(next).unwrap();
    }

    /// TASK-249: a VM process that acknowledges at the pace a slow host lets it, behind a backlog
    /// it clears slowly, the daemon sending more all the while, is kept however long it is behind:
    /// 300 s, five times [`BEHIND`]. The cap of [`BEHIND`] let go of one making a DAX clone of the
    /// Linux checkout, which went on to finish 100 s behind.
    #[test]
    fn slow_vm_processes_acknowledging_are_kept_however_long_behind() {
        let acks = Acks::default();
        let start = std::time::Instant::now();
        let at = |seconds| start + std::time::Duration::from_secs(seconds);
        let first = next(&acks, true);
        (0..100_000).for_each(|_| {
            next(&acks, true);
        });
        assert!(!acks.look(at(0)).unresponsive());
        // 300 acknowledged a second, 100 more sent: the operation the watchdog waits for, the
        // 100,001st, acknowledged only at 333 s.
        for second in 1..=300 {
            (0..100).for_each(|_| {
                next(&acks, true);
            });
            acks.acked(first + second * 300).unwrap();
            let silence = acks.look(at(second));
            assert!(!silence.unresponsive(), "at {second} s: {silence}");
        }
        assert_eq!(
            acks.look(at(300)),
            Silence {
                silent: std::time::Duration::ZERO,
                behind: std::time::Duration::from_secs(300),
                backlog: 40_000,
                pace: Some(18_000),
            }
        );
    }

    /// TASK-249, TASK-148: a VM process behind that acknowledges just often enough not to be
    /// silent, an operation every 4 s against 10,000 outstanding, hours from catching up, is let
    /// go once behind for [`BEHIND`], its pace measured.
    #[test]
    fn stalled_vm_processes_behind_are_let_go() {
        let acks = Acks::default();
        let start = std::time::Instant::now();
        let at = |seconds| start + std::time::Duration::from_secs(seconds);
        let first = next(&acks, true);
        (0..10_000).for_each(|_| {
            next(&acks, true);
        });
        assert!(!acks.look(at(0)).unresponsive());
        for second in 1..60 {
            acks.acked(first + second / 4).unwrap();
            let silence = acks.look(at(second));
            assert!(!silence.unresponsive(), "at {second} s: {silence}");
        }
        acks.acked(first + 15).unwrap();
        let stalled = acks.look(at(60));
        assert!(stalled.unresponsive());
        assert_eq!(
            stalled,
            Silence {
                silent: std::time::Duration::ZERO,
                behind: BEHIND,
                backlog: 9985,
                pace: Some(16)
            }
        );
    }

    /// What the watchdog says as it lets a VM process go tells the two limits apart: one silent,
    /// and one acknowledging all along that stays behind at a pace that would not catch up
    /// (TASK-136.1: a publisher was let go `silent for 0ns, behind for 60s`, which no log said
    /// before; TASK-249).
    #[test]
    fn a_silence_says_which_limit_it_passed() {
        let acks = Acks::default();
        let start = std::time::Instant::now();
        let at = |seconds| start + std::time::Duration::from_secs(seconds);
        let first = next(&acks, true);
        (0..10_000).for_each(|_| {
            next(&acks, true);
        });
        let none = std::time::Duration::ZERO;
        let found = acks.look(at(0));
        assert_eq!(found, Silence { silent: none, behind: none, backlog: 10_001, pace: None });
        assert_eq!(
            found.to_string(),
            "silent for 0ns (of 5s allowed), behind for 0ns: 10001 operations unacknowledged, no \
             pace measured yet (a pace clearing them within 600s allowed)"
        );
        for second in 1..60 {
            acks.acked(first + second).unwrap();
            assert!(!acks.look(at(second)).unresponsive());
        }
        acks.acked(first + 60).unwrap();
        let behind = acks.look(at(60));
        assert!(behind.unresponsive());
        assert_eq!(
            behind.to_string(),
            "silent for 0ns (of 5s allowed), behind for 60s: 9940 operations unacknowledged, 61 \
             acknowledged in the last 60s (a pace clearing them within 600s allowed)"
        );
        let silent = acks.look(at(66));
        assert!(silent.unresponsive());
        assert_eq!(
            silent.to_string(),
            "silent for 6s (of 5s allowed), behind for 66s: 9940 operations unacknowledged, 61 \
             acknowledged in the last 60s (a pace clearing them within 600s allowed)"
        );
    }

    /// TASK-79.9: what the watchdog finds unacknowledged counts from when it first found it so,
    /// anew once the VM process acknowledges it, and not at all with nothing unacknowledged.
    #[test]
    fn unanswered_from_when_first_found_so() {
        let acks = Acks::default();
        let start = std::time::Instant::now();
        let at = |seconds| start + std::time::Duration::from_secs(seconds);
        assert_eq!(acks.unanswered(at(0)), std::time::Duration::ZERO);
        let first = next(&acks, true);
        assert_eq!(acks.unanswered(at(1)), std::time::Duration::ZERO);
        assert_eq!(acks.unanswered(at(3)), std::time::Duration::from_secs(2));
        let second = next(&acks, true);
        acks.acked(first).unwrap();
        assert_eq!(acks.unanswered(at(4)), std::time::Duration::ZERO);
        assert_eq!(acks.unanswered(at(9)), std::time::Duration::from_secs(5));
        acks.acked(second).unwrap();
        assert_eq!(acks.unanswered(at(10)), std::time::Duration::ZERO);
    }
}

/// TASK-707: what a request's future holds, which every request served is moved into, and onto the
/// heap with (`serve_future`), however little it waits.
#[cfg(test)]
mod request_future_tests {
    use super::*;

    /// The size of the future `f` returns, read from its type: `f` is not called.
    fn returned_size<A, B, C, F: std::future::Future>(_: impl FnOnce(A, B, C) -> F) -> usize {
        std::mem::size_of::<F>()
    }

    /// A request's future is small: as a commit's, not as a cell's opening, which it boxes. At
    /// 23 KB, copied as it was spawned, it cost the daemon some 15% more CPU a transaction under
    /// pgbench than serving requests in place did (TASK-707).
    #[test]
    fn a_requests_future_holds_no_cells_opening() {
        let request = returned_size(Connection::serve_message);
        assert!(request <= 4096, "a request's future is {request} bytes");
    }
}

/// TASK-43.4: which paths name cells in a directory.
#[cfg(test)]
mod in_dir_tests {
    use super::in_dir;

    /// A cell is in a directory by whatever path names it: not outside it, through symlinks or
    /// `..`, nor the directory itself.
    #[test]
    fn paths_name_cells_in_a_directory_however_they_name_them() {
        let dir = tempfile::tempdir().unwrap();
        let (ours, theirs) = (dir.path().join("ours"), dir.path().join("theirs"));
        host_fs::create_dir(&ours).unwrap();
        host_fs::create_dir(&theirs).unwrap();
        host_fs::create_dir(ours.join("store")).unwrap();
        std::os::unix::fs::symlink(&theirs, ours.join("link")).unwrap();
        let ours = host_fs::canonicalize(&ours).unwrap();
        for path in [ours.join("store"), ours.join("new"), ours.join("./new")] {
            assert!(in_dir(&path, &ours), "{}", path.display());
        }
        for path in [
            theirs.join("store"),
            ours.join("../theirs/store"),
            ours.join("link/store"),
            ours.join("none/../../theirs/store"),
            ours.clone(),
            dir.path().join("store"),
        ] {
            assert!(!in_dir(&path, &ours), "{}", path.display());
        }
    }
}
