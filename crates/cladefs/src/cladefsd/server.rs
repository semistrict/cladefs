// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's side of a connection: a VM process's cell and window, served from this process's
//! cells, extent cache and kernel.

use super::Daemon;
use super::cells::{self, cell_dir_key, open_cell_at};
use super::codec::{
    self, Data, Fault, Forked, Info, Made, Opened, ReadInto, Request, Stage, StageOp, Told,
    stream_bytes,
};
use super::reads::Kind;
use super::stats;
use super::wire::*;
use crate::cell_requests::{self, Guest, Resizes, Shrinking};
use crate::cell_table::Writer;
use crate::{Commit, MappingContents, VolatileSlice, VolatileSliceMut, WindowMapping};
use crate::{DaxWindow, Errno, OpenCell, SecondStage};
use bucket_cell::trace;
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
    let _serving = super::Serving::begin(daemon);
    let serving = Arc::clone(daemon);
    serve_as(daemon, listener, move || default_tenant_on(&serving));
}

/// Serves the VM processes that connect to `listener` on the process's default daemon
/// ([`serve_on`]).
#[cfg(not(madsim))]
pub fn serve(listener: UnixListener) {
    serve_on(Daemon::process(), listener);
}

/// Serves the VM processes of `tenant` that connect to `listener` on `daemon`, as [`serve_on`]
/// does (TASK-43): until its grant is revoked, which shuts the listener down.
pub(super) fn serve_tenant_on(daemon: &Arc<Daemon>, listener: UnixListener, tenant: Arc<Tenant>) {
    serve_as(daemon, listener, move || Ok(tenant.clone()));
}

/// Serves the VM processes of `tenant` that connect to `listener` on the process's default daemon
/// ([`serve_tenant_on`]).
#[cfg(not(madsim))]
pub fn serve_tenant(listener: UnixListener, tenant: Arc<Tenant>) {
    serve_tenant_on(&tenant.daemon.clone(), listener, tenant);
}

/// cladefs: how many mappings the host lets a process make, which the reply to an `OPEN` tells the
/// VM process ([`Opened`]), as it can't read it in a jail without /proc (TASK-172): Linux's
/// default, said, if this process can't either.
fn max_map_count() -> Option<u64> {
    #[cfg(test)]
    if let Some(&given) = MAX_MAP_COUNT_GIVEN.get() {
        return Some(given);
    }
    crate::pages::max_map_count().unwrap_or_else(|err| {
        let default = crate::pages::DEFAULT_MAX_MAP_COUNT;
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
/// `listener`, until it listens no more, shut down (`EINVAL`): one whose tenant it fails to give,
/// logged, it closes. An accept that fails otherwise, as with as many files open as the
/// daemon may, is tried again after [`ACCEPT_AGAIN`], and its connection served then: given up on,
/// it would leave every VM process that connects to it unserved, a tenant's socket bound still
/// (TASK-79.35).
fn serve_as(
    daemon: &Arc<Daemon>,
    listener: UnixListener,
    tenant: impl Fn() -> Result<Arc<Tenant>, Errno>,
) {
    // cladefs: until the daemon drains, which shuts it down (TASK-141.4); dropped before it.
    let _accepting = Accepting::new(daemon, &listener);
    let mut failing = false;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                failing = false;
                if let Ok(tenant) = tenant() {
                    drop(start_connection(stream.into(), tenant));
                }
            }
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => return,
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
/// listener is: shut down at once if the daemon drains, as it may have as it came.
struct Accepting {
    daemon: Arc<Daemon>,
    fd: std::os::fd::RawFd,
}

impl Accepting {
    fn new(daemon: &Arc<Daemon>, listener: &UnixListener) -> Self {
        use std::os::fd::AsRawFd;
        let mut accepting = daemon.accepting.lock().unwrap();
        if super::drain::draining(daemon) {
            super::shut_down(listener.as_fd());
        }
        let fd = listener.as_raw_fd();
        accepting.push(fd);
        Self { daemon: Arc::clone(daemon), fd }
    }
}

impl Drop for Accepting {
    fn drop(&mut self) {
        self.daemon.accepting.lock().unwrap().retain(|fd| *fd != self.fd);
    }
}

/// cladefs: has every VM socket served on `daemon` take no more connections, as the daemon drains
/// (TASK-141.4): their listeners shut down, their accepts failing (`EINVAL`), and connecting
/// refused.
pub(super) fn stop_accepting(daemon: &Daemon) {
    for &fd in daemon.accepting.lock().unwrap().iter() {
        // SAFETY: Its listener lives while it is there: its `Accepting` takes it out first.
        super::shut_down(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) });
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
    disk: Option<Arc<super::disk::Disk>>,
}

/// cladefs: what a tenant's VM processes may do (TASK-160.10), which its connections hold them to.
struct Policy {
    /// Where they name cells.
    cell_dirs: CellDirs,
}

/// cladefs: where a tenant's VM processes name cells ([`Policy`]).
enum CellDirs {
    /// In its directory alone, canonical (TASK-43.4).
    Dir(PathBuf),
    /// In the daemon's roots, if it has any ([`roots`]); anywhere, if it has none: the default
    /// tenant's.
    Roots(Option<Arc<[PathBuf]>>),
}

impl Policy {
    /// A granted tenant's, whose cells are in `dir`, canonical.
    fn granted(dir: PathBuf) -> Self {
        Self { cell_dirs: CellDirs::Dir(dir) }
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
    arena: Arc<crate::pages::Arena>,
    /// Its arena's memory, reopened read-only: its VM processes copy their guests' writes into
    /// memory of their own (doc-26), and write no page of it.
    memory: std::os::fd::OwnedFd,
    /// The default arena's memory, reopened read-only.
    public: std::os::fd::OwnedFd,
    /// cladefs: how many connections the tenant's VM processes have ([`MAX_CONNECTIONS`]).
    connections: Arc<std::sync::atomic::AtomicUsize>,
    /// cladefs: what its connections have the daemon hold of its VM processes' requests together,
    /// within the bytes in flight they share (TASK-149.35).
    in_flight: Arc<super::memory::InFlight>,
}

impl Memory {
    /// An arena of its own, and the default one, each read-only.
    fn new(daemon: &Daemon) -> std::io::Result<Self> {
        use std::os::fd::AsFd;
        let public = reopen_read_only(crate::pages::physical_memory()?.as_fd())?;
        let arena = crate::pages::Arena::new()?;
        let memory =
            reopen_read_only(crate::pages::arena_memory(arena.id()).expect("a new arena").as_fd())?;
        let connections = Default::default();
        // cladefs: within every tenant's together (board 8465).
        let in_flight = Arc::new(super::memory::InFlight::of_a_tenant(daemon));
        Ok(Self { arena, memory, public, connections, in_flight })
    }
}

/// cladefs: the most connections a tenant's VM processes, the default tenant's too, have at once
/// (TASK-79.16): a few times the VMs a host runs. Those past it are closed as they come, as each
/// holds the daemon's memory, as much as the protocol bounds, files and tasks. VMs restored from
/// a zygote, two connections each, their root's and their memory's, took 1024 at 512 VMs, ubu1's
/// 16 GiB holding 497 with 3.9 GiB to spare (board 7084).
pub(super) const MAX_CONNECTIONS: usize = 4096;

/// cladefs: how many of the daemon's open files a connection takes, as [`most_connections`]
/// counts them (board 8465): its socket, the socket again for its sender, its VM process's pidfd,
/// and one for what its cell opens beside the cells of others.
const FILES_PER_CONNECTION: usize = 4;

/// cladefs: how many connections each tenant alive keeps, whatever the others have (board 8465): a
/// tenant with fewer is refused none while the daemon has room for any, however many the others
/// have taken; those past it take what all of them leave. A VM restored from a zygote has two
/// (hotbed question 483).
const RESERVED_CONNECTIONS: usize = 64;

/// cladefs: the most connections the daemon serves at once, every tenant's together (board 8465):
/// what its open files leave, [`FILES_PER_CONNECTION`] each, a quarter of its limit kept for the
/// rest (its cells' files, its part cache, its control API). Past it, an accept that failed for
/// files held every tenant's new VM processes, retried every [`ACCEPT_AGAIN`], whichever tenant had
/// taken them.
fn most_connections() -> usize {
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
    (files - files / 4) / FILES_PER_CONNECTION
}

/// cladefs: the connections the daemon serves at once, of the most ([`most_connections`]), and how
/// many each tenant keeps ([`RESERVED_CONNECTIONS`]): as a test gives them, if it does.
fn connection_limits() -> (usize, usize) {
    #[cfg(test)]
    if let Some(&given) = CONNECTION_LIMITS_GIVEN.get() {
        return given;
    }
    (most_connections(), RESERVED_CONNECTIONS)
}

/// cladefs: the limits of connections in a test, in place of the daemon's ([`connection_limits`]):
/// the process's, a test's, which runs in a process of its own.
#[cfg(test)]
pub(super) static CONNECTION_LIMITS_GIVEN: OnceLock<(usize, usize)> = OnceLock::new();

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
        if own >= MAX_CONNECTIONS {
            return Err(format!("its tenant has {MAX_CONNECTIONS}"));
        }
        // Of the others' shares, what their VM processes leave, which this one takes none of.
        let kept = super::memory::reserved_connections_left_on(&tenant.daemon, reserved);
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
    let id = crate::host_cell::DEFAULT_TENANT;
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
    pub(super) fn new_on(
        daemon: &Arc<Daemon>,
        id: u64,
        dir: impl AsRef<Path>,
    ) -> std::io::Result<Arc<Self>> {
        let dir = dir.as_ref();
        let (dir, disk) = local_cell::upload::blocking(|| -> std::io::Result<_> {
            let dir = host_fs::canonicalize(&dir)?;
            let disk = Some(super::disk::Disk::new_on(daemon, id, &dir)?);
            Ok((dir, disk))
        })?;
        let policy = Policy::granted(dir);
        let tenant = Arc::new(Self {
            daemon: Arc::clone(daemon),
            id,
            policy,
            memory: Memory::new(daemon)?,
            disk,
        });
        super::memory::register_on(daemon, &tenant);
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
        self.dir().map(|_| self.id)
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

    /// Its cells' directory, if it is a granted tenant's: the default tenant has none
    /// ([`CellDirs::Roots`]).
    pub fn dir(&self) -> Option<&Path> {
        match &self.policy.cell_dirs {
            CellDirs::Dir(dir) => Some(dir),
            CellDirs::Roots(_) => None,
        }
    }

    /// Its arena of physical memory.
    pub fn arena(&self) -> crate::pages::ArenaId {
        self.memory.arena.id()
    }

    /// cladefs: what its connections have the daemon hold of its VM processes' requests, within the
    /// bytes in flight they share (TASK-149.35).
    pub(super) fn in_flight(&self) -> &Arc<super::memory::InFlight> {
        &self.memory.in_flight
    }

    /// cladefs: how many connections its VM processes have ([`Counted`], board 8465).
    pub(super) fn connections(&self) -> usize {
        self.memory.connections.load(Ordering::Acquire)
    }

    /// cladefs: the host's disk its directory takes, a granted tenant's (TASK-94): the default
    /// tenant has none.
    pub fn disk(&self) -> Option<&Arc<super::disk::Disk>> {
        self.disk.as_ref()
    }

    /// Whether its VM processes may name the cell at `path`, whichever way `path` names it:
    /// `EACCES` unless it is where they name cells ([`CellDirs`]).
    fn holds(&self, path: &Path) -> Result<(), Errno> {
        let held = match &self.policy.cell_dirs {
            CellDirs::Dir(dir) => in_dir(path, dir),
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

/// cladefs: `fd` again, on macOS, where the daemon builds under madsim alone (board 2912): an
/// arena's object there is a file unlinked as it is made (`slabs`), and so can't be opened anew,
/// read-only or not. Writable, then; but no process maps it: a simulated guest is in the
/// simulation's process, and maps nothing it is handed.
#[cfg(not(target_os = "linux"))]
fn reopen_read_only(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::os::fd::OwnedFd> {
    fd.try_clone_to_owned()
}

/// Serves the VM process at the other end of `stream`, of `tenant`, until it closes the
/// connection, or exits: then its cell and window go. Returns then: for tests, which serve
/// connections one by one.
#[cfg(test)]
pub(crate) fn serve_connection(stream: std::os::unix::net::UnixStream, tenant: Arc<Tenant>) {
    let _ = start_connection(stream.into(), tenant).recv();
}

/// cladefs: a connection of a simulated guest's to the daemon, in memory, served as a VM process's
/// is, of `tenant`'s: the guest's end (doc-24, TASK-685.6). Under madsim, which has no VM socket.
#[cfg(madsim)]
pub(crate) fn connect_in_memory(tenant: Arc<Tenant>) -> super::wire::Stream {
    let (daemons, guests) = super::wire::Stream::pair();
    drop(start_connection(daemons, tenant));
    guests
}

/// Starts serving the VM process at the other end of `stream`, as [`serve_connection`] does, on
/// the daemon's runtime: no thread of its own. What it returns says when it is done.
/// cladefs: one of a tenant past [`MAX_CONNECTIONS`], or past the daemon's most, it closes at once
/// ([`Counted::new`]).
/// cladefs: one coming as the daemon drains it closes at once (TASK-141.4).
fn start_connection(stream: super::wire::Stream, tenant: Arc<Tenant>) -> mpsc::Receiver<()> {
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
        run_connection(stream, tenant).await;
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
async fn run_connection(stream: super::wire::Stream, tenant: Arc<Tenant>) {
    let daemon = Arc::clone(&tenant.daemon);
    let sender = match stream.try_clone().and_then(|clone| {
        clone.set_nonblocking(true)?;
        // cladefs: what it queues held in its tenant's bytes in flight too (TASK-149.35).
        AsyncSender::counting(clone, tenant.in_flight().clone())
    }) {
        Ok(sender) => Arc::new(sender),
        Err(err) => {
            log::error!("cladefsd: cloning a connection: {err}");
            return;
        }
    };
    // cladefs: one the daemon could not let go of, once it stopped answering, it refuses
    // (TASK-79.9).
    let peer = match Peer::connected(&daemon, &stream, sender, tenant.id) {
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
                    connection.end_receiving();
                    went.await
                }
                () = super::drain::started(&daemon) => {
                    peer.sender.shutdown();
                    connection.end_receiving();
                    went.await
                }
            };
            // Dropped unsent if receiving failed. cladefs: a VM process past the protocol's
            // bounds the daemon lets go of (TASK-79.16).
            if ended == Ok(Ended::Broke) {
                peer.let_go(format_args!("kept more in flight than the protocol allows"));
            }
        }
        Err(err) => log::error!("cladefsd: receiving from a VM process: {err}"),
    }
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
    if super::drain::draining(&daemon)
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
                Ok(Some((message, _)))
                    if [ACK, STAGE_COUNTS, STAGE_PRIVATE_COUNTS].contains(&message.header.kind) =>
                {
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
                        Ok(Made::Counts(counts)) => connection.report.stage_counted(counts),
                        Ok(Made::PrivateCounts(counts)) => {
                            connection.report.private_counted(counts)
                        }
                        Err(errno) => log::warn!(
                            "cladefsd: a VM process's malformed {}, dropped ({errno:?}): {:?}",
                            match message.header.kind {
                                ACK => "ACK",
                                STAGE_COUNTS => "STAGE_COUNTS",
                                _ => "STAGE_PRIVATE_COUNTS",
                            },
                            message.header,
                        ),
                    }
                }
                Ok(Some(admitted)) => {
                    let Some((message, charge)) = connection.admit(admitted) else { continue };
                    connection.daemon().spawn(receive(connection.clone(), receiver, gone));
                    // The task does nothing more: its receiving goes on in another. Inline under
                    // madsim, which has one thread (`upload::blocking`, doc-24 step 11).
                    local_cell::upload::blocking(move || connection.serve_in_turn(message, charge));
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
        let failed = match super::run::off_the_runtime(move || std::fs::write(&file, text)).await {
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
    waiting: std::collections::VecDeque<(Message, Charge)>,
    running: usize,
    deferred: usize,
    stream_bytes: u64,
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
        guest: Option<&crate::request::RequestId>,
        charge: Option<Charge>,
    ) -> Self {
        let started = std::time::Instant::now();
        let cell = connection.cell().map(|open_cell| open_cell.cell());
        let span = super::otel::request_span(&request, cell, guest);
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
            self.span.set(super::otel::ERRNO, errno.raw() as i64);
            self.span.fail(errno);
        }
        // Counted before its reply, which its VM process may then count on.
        super::metrics::served(header.kind, self.started.elapsed(), reply.status != 0);
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
        Request::Map(mapping) => (Some(mapping.len), Some(1)),
        Request::MapMany(mappings) => {
            // Each within a window, as decoding has them: summed so that no count of them wraps
            // (board 2776).
            let bytes = mappings.iter().fold(0u64, |sum, mapping| sum.saturating_add(mapping.len));
            (Some(bytes), Some(mappings.len() as u64))
        }
        Request::Unmap { len, .. } => (Some(*len), None),
        Request::Free(_) => (None, Some(1)),
        Request::FreeMany { rest, .. } => (None, Some(1 + rest.len() as u64)),
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
    reads: OnceLock<super::reads::Trace>,
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
        if let Some(trace) = super::reads::Trace::opened(self.0.daemon(), open_cell.cell().id) {
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
            Phase::Writing(hold) => Some(cells::Closed {
                table: self.tenant.daemon.table.clone(),
                key: hold.key().to_owned(),
            }),
            Phase::Unopened | Phase::Opening => None,
        };
        drop(self.tenant.daemon.spawn(async move {
            if let Some(open_cell) = open_cell {
                if let Some(closed) = closed {
                    open_cell.keep_until_closed(Box::new(closed));
                }
                crate::open_cell::release(open_cell).await;
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
        MAP | MAP_MANY => "cladefsd::map",
        UNMAP => "cladefsd::unmap",
        COMMIT => "cladefsd::commit",
        FREE | FREE_MANY => "cladefsd::free",
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
    fn arena(&self) -> &Arc<crate::pages::Arena> {
        &self.tenant.memory.arena
    }

    /// cladefs: the arena's ID ([`Connection::arena`]).
    pub(super) fn arena_id(&self) -> crate::pages::ArenaId {
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
        if [ACK, STAGE_COUNTS, STAGE_PRIVATE_COUNTS].contains(&header.kind) {
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
            let in_flight = requests.in_flight();
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
    fn end_receiving(&self) {
        self.ended.store(true, Ordering::Release);
        self.ending.notify(usize::MAX);
    }

    /// `message`, a request, with what the daemon holds for it counted, to serve now, unless
    /// [`WORKERS`] of the connection's run, when it waits for one of them to be done, which serves
    /// it then ([`Connection::serve_in_turn`]).
    fn admit(&self, admitted: (Message, Charge)) -> Option<(Message, Charge)> {
        let mut requests = self.requests.with(Mutex::lock);
        if requests.running == WORKERS {
            requests.waiting.push_back(admitted);
            return None;
        }
        requests.running += 1;
        Some(admitted)
    }

    /// Serves `message`, admitted ([`Connection::admit`]); then the next waiting, if any, on the
    /// [`Pool`], in turn with every other connection's: whether serving it returned or panicked,
    /// which would otherwise leave it counted running for good (TASK-79.1). cladefs: a fault, and a
    /// writer's opening, are futures, deferred as they wait ([`serve_future`]): for the pager
    /// (TASK-92.20), and for a connection going to let the cell go, or a restore's version
    /// (TASK-92.5). A request its kind gives no meaning ([`Request::decode`]), or naming a cell
    /// its tenant's VM processes don't ([`Connection::authorize`]), is refused, whatever it is, in
    /// one place (TASK-160.9).
    fn serve_in_turn(self: Arc<Self>, message: Message, charge: Charge) {
        // cladefs: served with the guest request it is a call for current, if any, as its futures
        // are polled too, wherever: what its log records and its span name (`crate::request`,
        // TASK-260).
        let guest = match codec::guest_request(&message) {
            Ok((Some(guest), _)) => {
                Some(crate::request::RequestId { guest, open_cell: self.named.get().cloned() })
            }
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
        serve_future(turn, crate::request::within(guest, parent.around(serving)));
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
            // cladefs: boxed, as an opening's future holds all a cell's opening awaits, some 22 KB,
            // which every request's future, commits' and writes' too, would hold and be copied
            // with, onto the heap in `serve_future`: some 15% of the daemon's CPU under pgbench
            // (TASK-707).
            Ok(Request::Open { path, opening }) => {
                Box::pin(self.clone().serve_open(path, opening, answer)).await
            }
            Ok(request) => self.serve(&request, answer).await,
        }
    }

    /// Serves the next request waiting, if any, on the [`Pool`], the last having been served; or,
    /// with none, counts one fewer running.
    fn next_turn(self: Arc<Self>) {
        loop {
            let mut requests = self.requests.with(Mutex::lock);
            if let Some((next, charge)) = requests.waiting.pop_front() {
                drop(requests);
                let header = next.header;
                let connection = self.clone();
                // Its charge, refused, dropped with what would have served it.
                if self
                    .daemon()
                    .pool()
                    .run(Box::new(move || connection.serve_in_turn(next, charge)))
                {
                    return;
                }
                let answer = Answer::new(&self, header, None, None);
                let reply = answer.reply_header();
                answer.send(Err(Errno::EAGAIN), reply, Vec::new(), Vec::new());
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

    /// Whether it may name the cell at `path`: `EACCES` unless the cell is where its tenant's
    /// VM processes name cells (TASK-43.4).
    fn authorize(&self, path: &Path) -> Result<(), Errno> {
        self.tenant.holds(path)
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
            Request::Free(extent) => open_cell.free(*extent),
            // cladefs: and those after it, in order (TASK-92.13).
            Request::FreeMany { first, rest } => {
                let mut extents = std::iter::once(first).chain(rest.iter());
                extents.try_for_each(|extent| open_cell.free(*extent))
            }
            Request::Map(mapping @ WindowMapping { window_offset, len, contents }) => {
                let began = std::time::Instant::now();
                match *contents {
                    MappingContents::Zeros => open_cell.map_zeros_at(*window_offset, *len)?,
                    MappingContents::Extent { extent, offset } => {
                        open_cell.map(extent, offset, *len, *window_offset).await?
                    }
                }
                self.traced_maps(began, open_cell, std::slice::from_ref(mapping));
                Ok(())
            }
            // cladefs: in one change of the window (TASK-92.14).
            Request::MapMany(mappings) => {
                let began = std::time::Instant::now();
                open_cell.map_many(mappings).await?;
                self.traced_maps(began, open_cell, mappings);
                Ok(())
            }
            Request::Mappings => {
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
                // A VM snapshot's: a version of the cell, made here (doc-20), refused while
                // the window holds private pages, whose bytes no extent holds (TASK-203). Each
                // refusal says why here, as the VMM's error can't (TASK-92.14).
                // Boxed, as a cell's opening is (`serve_message`, TASK-707).
                let version = Box::pin(open_cell.make_version(None)).await.map_err(|err| {
                    if err.errno() == Some(Errno::EBUSY) {
                        log::warn!("cladefsd: a version refused: {err:#}");
                    }
                    cells::errno_of(format_args!("making a version"), err)
                })?;
                self.versions_made.with(Mutex::lock).insert(version);
                let cell = open_cell.cell();
                reply.set_body(&Forked { version, tenant: cell.tenant }.to_body());
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
            Request::Build(commit) => self.report.said_build(commit),
            Request::Stats { ask, guest } => self.report.answered(*ask, *guest),
            // Served apart, as futures ([`Connection::serve_in_turn`]).
            Request::Open { .. } | Request::Fault(_) => Err(Errno::EINVAL),
        }
    }

    /// cladefs: resolves the guest's `fault` ([`FAULT`]), and replies: a future, which holds no
    /// thread while it waits for the pager (TASK-92.20), served so ([`serve_future`]).
    async fn fault(self: Arc<Self>, fault: Fault, answer: Answer) {
        let Fault { offset, page, .. } = fault;
        let began = std::time::Instant::now();
        let faulted = async {
            let window = self.window.with(Mutex::lock).clone().ok_or(Errno::EBADF)?;
            // cladefs: in its turn, as what it pages in holds memory (TASK-149.34).
            let _turn = self.page_ins.acquire().await.map_err(|_| Errno::EIO)?;
            window.fault(offset, crate::Access::Read).await?;
            Ok(window)
        };
        let faulted = match faulted.await {
            Ok(window) => {
                self.traced_fault(&window, began, offset);
                Ok(())
            }
            Err(errno) => Err(errno),
        };
        if !page {
            crate::stats::add(crate::stats::Counter::FaultStores, 1);
        }
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
                    open_cell.refused(offset - offset % crate::pages::page_size(), what)
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
    /// ([`Connection::open`]), and replies with its cell and physical memory: a future, which
    /// holds no thread while it waits (TASK-92.5), served so ([`serve_future`]).
    async fn serve_open(self: Arc<Self>, path: PathBuf, how: codec::Opening, answer: Answer) {
        let mut reply = answer.reply_header();
        let opened = self.open(&path, how).await;
        let fds = match &opened {
            Ok(cell) => {
                let (cell, arena, max_map_count) = (*cell, self.arena_id(), max_map_count());
                reply.set_body(&Opened { cell, arena, max_map_count }.to_body());
                Vec::from(self.memories())
            }
            // Not opened: no memories to send; the failure goes in the answer.
            Err(_) => Vec::new(),
        };
        answer.send(opened.map(drop), reply, Vec::new(), fds);
    }

    /// cladefs: records in its trace of its guest's reads, if it keeps one (TASK-702.1), a read of
    /// `kind` the guest began at `began`, of `len` bytes at `offset` in `extent`, of `open_cell`.
    fn traced(
        &self,
        kind: Kind,
        began: std::time::Instant,
        open_cell: &OpenCell,
        extent: crate::ExtentId,
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
        self.traced(Kind::Fault, began, open_cell, extent, at, crate::pages::page_size());
    }

    /// Opens the cell at `path`, authorized, as `how` says: creating it if it doesn't exist, or
    /// forking it from the version `how` names if it doesn't exist ([`cells::fork_version`]);
    /// unless another connection has it open, or others have it open (`EBUSY`); and returns its
    /// cell. Its waits, for a connection whose VM
    /// process is gone to let the cell go, and for a restore's version, hold no thread
    /// (TASK-92.5); its I/O blocks. A version of another tenant's than the connection's forks only
    /// if it is shared (`EACCES`, TASK-395.7).
    async fn open(&self, path: &Path, how: codec::Opening) -> Result<u64, Errno> {
        let opening = self.opening()?;
        let hold =
            cells::hold_to_write(self.daemon(), &cell_dir_key(path), self.this.clone()).await?;
        // cladefs: nor one of another tenant's cells (TASK-364).
        self.tenant.reaches(path)?;
        // cladefs: a cell it makes, within its disk and its tenant's limits, as its making admits
        // it (doc-22).
        // Closed before its hold lets its path go, if it fails here.
        let open_cell = self.open_held(path, &hold, &how).await?;
        // A VM process opens no cell to write that its writer may not write
        // ([`crate::CellAccess::may_write`]): a public base, only forks of it (TASK-43), what it
        // wrote being every tenant's, its pages the default arena's, which every VM process holds
        // (TASK-79.17); cladefs: nor a version, which nothing writes (doc-7, Versions).
        open_cell.access().may_write()?;
        let flushed = open_cell.clone();
        let flushing_daemon = Arc::clone(self.daemon());
        self.start_behind(move |stopped| flush_behind(flushing_daemon, flushed, stopped));
        let cell = open_cell.cell().id;
        // cladefs: should its VM process go, and a writer wait for its cell to close, the cell's
        // uploader stops at once rather than upload what it holds first (TASK-216).
        if let Some(cut_short) = open_cell.cut_short() {
            hold.when_wanted(cut_short);
        }
        opening.opened(path, open_cell, hold);
        Ok(cell)
    }

    /// Opens the cell at `path`, which `hold` holds, as [`Connection::open`] does: the cell
    /// open there from now on.
    async fn open_held(
        &self,
        path: &Path,
        hold: &cells::Hold,
        how: &codec::Opening,
    ) -> Result<Arc<OpenCell>, Errno> {
        let daemon = self.daemon();
        let arena = self.arena();
        // cladefs: a restore's cell there already, which the snapshot was taken of or an earlier
        // restore forked, holds the version's state, or is refused, as it is (TASK-194).
        let mut there = None;
        if let codec::Opening::ForkOf { tenant, cell, version, capacity } = *how
            && local_cell::upload::blocking(|| {
                host_fs::exists(&path) && !crate::host_cell::unmade(path)
            })
        {
            let open_cell = Arc::new(open_cell_at(daemon, path, None, Some(arena)).await?);
            cells::at_version(
                daemon,
                (crate::CellId { tenant, id: cell }, version),
                capacity,
                path,
                &open_cell,
            )
            .await?;
            there = Some(open_cell);
        }
        let open_cell = match (there, how) {
            (Some(open_cell), _) => open_cell,
            // cladefs: or where a cell's making was left unmade, its host crashing, which is no
            // cell: removed first, as deleting it would (TASK-79.84).
            (None, &codec::Opening::ForkOf { tenant, cell, version, capacity }) => {
                local_cell::upload::blocking(|| cells::removed_if_unmade(path))?;
                let forked = (crate::CellId { tenant, id: cell }, version);
                // cladefs: a restore's fork, made and opened syncing nothing: durable by its first
                // flush or its close, before any commit of its VM is (board 7420, the user's
                // call). A host crash before leaves it unmade, as if the restore never was.
                // cladefs: a crash of the daemon's too, its VM processes going with it.
                let deferring = local_cell::defer_durability(path);
                cells::fork_version(daemon, forked, capacity, (path, self.tenant.granted()))
                    .await?;
                let open_cell = open_cell_at(daemon, path, None, Some(arena)).await?;
                deferring.opened();
                Arc::new(open_cell)
            }
            (None, codec::Opening::Create(capacity, kind)) => {
                let new = cells::New { capacity: *capacity, kind: *kind, tenant: self.tenant.id };
                Arc::new(open_cell_at(daemon, path, Some(new), Some(arena)).await?)
            }
            (None, _) => Arc::new(open_cell_at(daemon, path, None, Some(arena)).await?),
        };
        hold.open(&open_cell)?;
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
}

impl Resizes for Connection {
    /// Sets its cell's capacity to `capacity` bytes (the control API's `ResizeCell`): grows it,
    /// and tells the guest; or asks the guest to shrink, and shrinks it if the guest does, holding
    /// no thread while it waits for the guest's answer (TASK-43.6). The cell's I/O runs on a
    /// blocking thread.
    async fn resize(&self, capacity: u64) -> Result<(), Errno> {
        let open_cell = cells::Live(self.cell().ok_or(Errno::EBADF)?.clone());
        cell_requests::resize_held(&open_cell, self, &self.shrinking, capacity, SHRINK_WAIT).await
    }
}

impl Guest for Connection {
    /// Tells the VM process the cell's capacity is `capacity` bytes, or, if `ask`, asks its guest
    /// to shrink to it ([`CAPACITY`]).
    fn tell(&self, capacity: u64, ask: bool) -> Result<(), Errno> {
        let told = Told::Capacity { capacity, ask }.encode();
        self.peer.sender.send(told, Vec::new(), &[]).map_err(|err| {
            log::warn!("cladefsd: telling a VM process its capacity: {err}");
            Errno::EIO
        })
    }
}

/// cladefs: how many `lseek(SEEK_HOLE)` checks [`TransferArea::written`] has run (board 7411).
#[cfg(test)]
pub(super) static TRANSFER_SEEK_HOLES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// A connection's transfer area ([`TRANSFER`]), shared memory its VM process made, mapped here:
/// which `READ`s write and `WRITE`s read their data in, as the VM process says, which may change
/// it meanwhile: it is accessed as volatile memory, its ranges checked against its length.
struct TransferArea {
    addr: std::ptr::NonNull<u8>,
    len: usize,
    num_pages: usize,
    /// cladefs: its memory opened anew, read-only, for [`TransferArea::written`] to seek in with
    /// an offset of its own, not the VM process's descriptor's.
    memory: std::os::fd::OwnedFd,
    /// cladefs: bitmap of pages already verified non-hole by `SEEK_HOLE` (board 7411). Because
    /// [`TransferArea::map`] seals the memfd with `F_SEAL_FUTURE_WRITE | F_SEAL_GROW | F_SEAL_SEAL`
    /// before returning, once a page is written by the VM process it can never become a hole again;
    /// steady-state transfer-area I/O checks this bitmap with zero syscalls.
    verified: Box<[std::sync::atomic::AtomicU64]>,
}

// SAFETY: Shared memory mapped for the area's life, accessed as volatile memory alone.
unsafe impl Send for TransferArea {}
// SAFETY: as Send's: shared, it is only read and written volatile, through VolatileSlice(Mut),
// as memory another process writes too.
unsafe impl Sync for TransferArea {}

impl TransferArea {
    /// The `len` bytes of the shared memory `fd`, mapped: `EINVAL` unless it has that many, is a
    /// whole number of host pages, and is sealed against shrinking, which would fault the daemon's
    /// accesses past its new end.
    /// cladefs: sealed here, once mapped, against future writes, which takes the VM process's
    /// punching holes in it away, the VM process's own mappings writing on; and accessed only
    /// where its VM process wrote it first ([`TransferArea::range`]): the kernel charges a page of
    /// shared memory to the process that faults it in (`mm/shmem.c`), and one the daemon faulted
    /// would be charged to the daemon, for as long as the VM process kept the memory, the
    /// connection gone (TASK-547).
    fn map(fd: std::os::fd::BorrowedFd<'_>, len: u64) -> Result<Self, Errno> {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::AtomicU64;
        let page = crate::pages::page_size();
        if len == 0 || len > MAX_TRANSFER_AREA || !len.is_multiple_of(page) {
            return Err(Errno::EINVAL);
        }
        if !seals::against_shrinking(fd) {
            return Err(Errno::EINVAL);
        }
        // SAFETY: A stat buffer the call fills.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: As above, of a descriptor the message carried.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 || (stat.st_size as u64) < len {
            return Err(Errno::EINVAL);
        }
        let num_pages = usize::try_from(len / page).map_err(|_| Errno::EINVAL)?;
        let words = num_pages.div_ceil(64);
        let len = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        let memory = reopen_read_only(fd).map_err(|err| {
            log::warn!("cladefsd: a transfer area not to be opened anew: {err}");
            Errno::EINVAL
        })?;
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
        let verified = (0..words).map(|_| AtomicU64::new(0)).collect();
        let area = Self {
            addr: std::ptr::NonNull::new(addr.cast()).unwrap(),
            len,
            num_pages,
            memory,
            verified,
        };
        if let Err(err) = seals::against_writes(fd) {
            log::warn!("cladefsd: a transfer area not to be sealed against writes: {err}");
            return Err(Errno::EINVAL);
        }
        Ok(area)
    }

    /// Bitmask for bits `[lo, hi)` within one 64-bit word (`0 <= lo < hi <= 64`).
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn word_mask(lo: u32, hi: u32) -> u64 {
        let count = hi.saturating_sub(lo);
        if count >= 64 {
            u64::MAX
        } else {
            1u64.wrapping_shl(count).wrapping_sub(1).wrapping_shl(lo)
        }
    }

    /// First page in `from..to` whose bit is not yet set in [`Self::verified`].
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn first_unverified(&self, from: usize, to: usize) -> Option<usize> {
        use std::sync::atomic::Ordering;
        if from >= to {
            return None;
        }
        let first_word = from / 64;
        let last_word = to.checked_sub(1)? / 64;
        for word in first_word..=last_word {
            let lo = if word == first_word { (from % 64) as u32 } else { 0 };
            let hi = if word == last_word {
                let rem = (to % 64) as u32;
                if rem == 0 { 64 } else { rem }
            } else {
                64
            };
            let mask = Self::word_mask(lo, hi);
            let bits = self.verified.get(word).map_or(0, |slot| slot.load(Ordering::Relaxed));
            let missing = mask & !bits;
            if missing != 0 {
                return word.checked_mul(64)?.checked_add(missing.trailing_zeros() as usize);
            }
        }
        None
    }

    /// Marks every page in `from..to` as verified non-hole in [`Self::verified`].
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn mark_verified(&self, from: usize, to: usize) {
        use std::sync::atomic::Ordering;
        let to = to.min(self.num_pages);
        let Some(last_page) = to.checked_sub(1) else {
            return;
        };
        if from >= to {
            return;
        }
        let first_word = from / 64;
        let last_word = last_page / 64;
        for word in first_word..=last_word {
            let lo = if word == first_word { (from % 64) as u32 } else { 0 };
            let hi = if word == last_word {
                let rem = (to % 64) as u32;
                if rem == 0 { 64 } else { rem }
            } else {
                64
            };
            let mask = Self::word_mask(lo, hi);
            if let Some(slot) = self.verified.get(word) {
                slot.fetch_or(mask, Ordering::Relaxed);
            }
        }
    }

    /// cladefs: whether its VM process wrote each page of the `len` bytes at `offset`, within it:
    /// no hole in its memory there, sealed as the area is against its punching one again. Its
    /// memory's, not its mapping's: a page written and then swapped out is written still, but in
    /// no memory, which `mincore` asked before, and refused the reads into it of a host short of
    /// memory, its guests' reads failing with EIO (board 6881). The daemon faulting such a page
    /// in charges it to whom it was charged when it went to swap, the VM process.
    ///
    /// Verified non-hole pages are cached in [`Self::verified`] (board 7411) so steady-state
    /// transfer-area I/O makes no `lseek(SEEK_HOLE)` syscalls.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn written(&self, offset: usize, len: usize) -> Result<bool, Errno> {
        use std::os::fd::AsRawFd;
        let page = usize::try_from(crate::pages::page_size()).map_err(|_| Errno::EINVAL)?;
        if page == 0 {
            return Err(Errno::EINVAL);
        }
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if end > self.len {
            return Err(Errno::EINVAL);
        }
        let start_page = offset.checked_div(page).ok_or(Errno::EINVAL)?;
        let end_page = end.div_ceil(page);
        let Some(miss) = self.first_unverified(start_page, end_page) else {
            return Ok(true);
        };
        #[cfg(test)]
        TRANSFER_SEEK_HOLES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let seek_at = miss.checked_mul(page).ok_or(Errno::EINVAL)?;
        let seek_at = libc::off_t::try_from(seek_at).map_err(|_| Errno::EINVAL)?;
        // SAFETY: Seeks in a descriptor of its own.
        let hole = unsafe { libc::lseek(self.memory.as_raw_fd(), seek_at, libc::SEEK_HOLE) };
        if hole < 0 {
            log::warn!("cladefsd: a transfer area's holes: {}", std::io::Error::last_os_error());
            return Err(Errno::EINVAL);
        }
        let hole_pages = usize::try_from(hole)
            .map_err(|_| Errno::EINVAL)?
            .checked_div(page)
            .ok_or(Errno::EINVAL)?;
        let verified_end = hole_pages.min(self.num_pages);
        if verified_end > miss {
            self.mark_verified(miss, verified_end);
        }
        Ok(hole_pages >= end_page)
    }

    /// The `len` bytes at `offset`, checked: `EINVAL` past its end; cladefs: and where its VM
    /// process didn't write it first, which the daemon would fault in itself, charged to it
    /// (TASK-547).
    fn range(&self, offset: u64, len: u64) -> Result<*mut u8, Errno> {
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if end > self.len as u64 {
            return Err(Errno::EINVAL);
        }
        if len > 0 && !self.written(offset as usize, len as usize)? {
            log::warn!("cladefsd: a transfer area's bytes its VM process never wrote");
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

/// cladefs: a transfer area's seals, Linux's (memfd): macOS, where the simulation runs too, has
/// none, so a transfer area is refused there, as no simulated guest makes one (board 2912).
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

    #[cfg(not(target_os = "linux"))]
    pub(super) fn against_shrinking(_: BorrowedFd<'_>) -> bool {
        false
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn against_writes(_: BorrowedFd<'_>) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
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
        pages * crate::pages::page_size()
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
    id: crate::slabs::VmProcessId,
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
        stream: &super::wire::Stream,
        sender: Arc<AsyncSender>,
        tenant: u64,
    ) -> std::io::Result<Arc<Self>> {
        daemon.watching_vm_peers.call_once(|| {
            // cladefs: physical memory's quarantine is the process's, given once, whichever
            // simulated host is first (TASK-685.14); the watchdog is each daemon's.
            // Process-wide registration for slabs::quarantine_with.
            static QUARANTINED: std::sync::Once = std::sync::Once::new();
            QUARANTINED.call_once(|| crate::slabs::quarantine_with(quarantine));
            daemon.spawn(watch(Arc::downgrade(daemon)));
        });
        let process = peer_process(stream)?;
        if let Some(process) = &process {
            // Signal 0, which checks the right to signal it, and sends nothing.
            process.signal(0).map_err(|err| {
                std::io::Error::new(err.kind(), format!("the daemon may not kill it: {err}"))
            })?;
        }
        let peer = Arc::new(Self {
            daemon: Arc::clone(daemon),
            id: crate::slabs::VmProcessId::connected(),
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

    /// Its window is of `size` bytes, whose mappings, a page each at most, a reply to `MAPPINGS`
    /// holds (TASK-79.43).
    fn set_window(&self, size: u64) {
        assert!(size <= MAX_WINDOW, "a window past the most the daemon gives credit for");
        let mappings = size / crate::pages::page_size() * MAPPING_BYTES;
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

/// cladefs: a VM process, other than this one, as a pidfd. Under madsim, none: a simulated
/// guest is in the simulation's process ([`peer_process`]), so the type has no values, and a
/// simulated host, macOS's too, has no pidfds (doc-24, board 2912).
#[cfg(not(madsim))]
enum VmProcess {
    Pidfd(std::os::fd::OwnedFd),
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
    #[cfg(not(madsim))]
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

    /// Returns once it has exited, holding no thread.
    #[cfg(not(madsim))]
    async fn exited(&self) -> std::io::Result<()> {
        use std::os::fd::AsFd;
        let pidfd = match self {
            Self::Pidfd(pidfd) => pidfd,
            #[cfg(test)]
            Self::Unkillable => return std::future::pending().await,
        };
        // A pidfd is readable once its process has exited.
        let exited =
            tokio::io::unix::AsyncFd::with_interest(pidfd.as_fd(), tokio::io::Interest::READABLE);
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
}

/// cladefs: the process at the other end of `stream`, as a pidfd, unless it is this one: the
/// socket's (`SO_PEERPIDFD`, Linux 6.5), which names the process that connected whatever became of
/// its pid. A kernel without it the daemon refuses at its start ([`require_peer_pidfds`], board
/// 8533): opening the pid `SO_PEERCRED` keeps (`pidfd_open`) raced a VM process that exits leaving
/// its socket to another, as a child, its pid then another process's, which the daemon would kill
/// in its place. Without it here, as in a test serving VM processes without booting a daemon, the
/// connection is refused.
#[cfg(madsim)]
fn peer_process(_: &super::wire::Stream) -> std::io::Result<Option<VmProcess>> {
    // cladefs: a simulated guest's, in this process, as a test's is (doc-24).
    Ok(None)
}

#[cfg(not(madsim))]
fn peer_process(stream: &super::wire::Stream) -> std::io::Result<Option<VmProcess>> {
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

/// cladefs: the pidfd of the process at the other end of the Unix socket `socket`
/// (`SO_PEERPIDFD`): `ENOPROTOOPT` said as needing Linux 6.5 (board 8533).
#[cfg(not(madsim))]
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
/// whose guests are in the simulation's process, nothing to ask.
pub(super) fn require_peer_pidfds() -> std::io::Result<()> {
    #[cfg(not(madsim))]
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
    use super::{Errno, Fault, refusal};

    /// cladefs: a probe of a page mapped nothing at, the VM process's own after a write's fault,
    /// refuses the guest nothing, and isn't told of; the guest's own access to it is. Red with
    /// probes told of as the guest's accesses.
    #[test]
    fn probes_refuse_the_guest_nothing() {
        let probe = Fault { offset: 0x1000, page: true, probe: true };
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
fn quarantine(freed: crate::slabs::Quarantined) {
    let waiting = PARKING.with_borrow(Clone::clone);
    if let Some(waiting) = &waiting {
        waiting.parked.fetch_add(1, Ordering::SeqCst);
    }
    let staged = freed.staged().to_vec();
    let counted = AtomicBool::new(waiting.is_some());
    let freed = Arc::new(Parked { freed: Some(freed), waiting, counted });
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
    freed: Option<crate::slabs::Quarantined>,
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
        self.freed.as_ref().map_or(0, crate::slabs::Quarantined::pages)
    }

    /// cladefs: what it waits on counts it no more, if it did.
    fn uncount(&self) {
        if let Some(waiting) = &self.waiting
            && self.counted.swap(false, Ordering::SeqCst)
        {
            waiting.parked.fetch_sub(1, Ordering::SeqCst);
            waiting.freed.notify(usize::MAX);
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
struct Waiting {
    parked: std::sync::atomic::AtomicUsize,
    freed: event_listener::Event,
}

thread_local! {
    /// cladefs: while this thread frees pages for a shrink ([`Parking::counting`]), what counts
    /// them.
    static PARKING: std::cell::RefCell<Option<Arc<Waiting>>> = const { std::cell::RefCell::new(None) };
}

/// cladefs: has the pages a shrink frees, quarantined ([`quarantine`]), counted until they are
/// free, which [`Parking::wait`] waits for (TASK-79.9): as a shrink of the daemon's memory, which
/// evicts until it holds no more than it is asked to, finds what it holds once what it evicted is
/// free, rather than at once, the pages a window mapped waiting for its VM process.
pub(super) struct Parking(Arc<Waiting>);

impl Parking {
    /// Counts nothing yet: what is freed in [`Parking::counting`] alone.
    pub(super) fn new() -> Self {
        Self(Arc::default())
    }

    /// cladefs: `free`'s output, counting the pages it frees, on this thread, as it runs (board
    /// 2946): the thread is what [`quarantine`] knows them by, and a task awaiting
    /// [`Parking::wait`] may go on on another, so what frees runs whole, synchronously, on the
    /// thread that counts it, and the waiting is the task's.
    pub(super) fn counting<T>(&self, free: impl FnOnce() -> T) -> T {
        /// What this thread counted for before, given back as `free` returns or unwinds.
        struct Restore(Option<Arc<Waiting>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                PARKING.set(self.0.take());
            }
        }
        let _restore = Restore(PARKING.replace(Some(self.0.clone())));
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
            let stalled = (peers.iter())
                .map(|peer| (peer.clone(), peer.sender.stalled(now)))
                .filter(|(_, stalled)| *stalled >= STALL)
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
        for (peer, stalled) in stalled {
            let queued = peer.sender.queued();
            peer.let_go(format_args!(
                "read nothing of the {queued} bytes queued for it for {stalled:?}, of {STALL:?} \
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
    vm: crate::slabs::VmProcessId,
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
            None => self.send_now(&[op]),
        }
    }

    /// Sends `ops` at once, in order, each with the next ID ([`Acks::sent`]), given as they are
    /// queued, so that IDs reach the VM process in order: the VM process acknowledges those that
    /// take access to pages away ([`StageOp::taking`]), each for every ID before it.
    fn send_now(&self, ops: &[StageOp]) {
        let stages = || ops.iter().map(|op| Stage { id: self.acks.sent(op), op: *op }.encode());
        // Failing, the VM process is gone, and its mappings with it.
        let _ = self.sender.send_all(|| stages().collect());
    }
}

impl SecondStage for VmProcessStage {
    fn vm_process(&self) -> Option<crate::slabs::VmProcessId> {
        Some(self.vm)
    }

    fn map(&self, offset: u64, phys: crate::pages::PhysLocation, len: u64) {
        self.send(StageOp::Map { offset, phys, len });
        crate::stats::add(crate::stats::Counter::StageMaps, 1);
        crate::stats::add(crate::stats::Counter::StageMapBytes, len);
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
            self.send_now(&ops);
        }
    }

    fn unmap(&self, offset: u64, len: u64, private: crate::dax::Private) {
        let keep_private = private == crate::dax::Private::Keep;
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
