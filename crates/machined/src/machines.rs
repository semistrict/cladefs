// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined's machines, by name: each one's spec, its state, its console, and, while it runs, its
//! [`Vm`] (in a helper process of its own) and vsock socket. A machine's lifecycle calls (start, stop, delete, snapshot, suspend,
//! resume) take turns on it; reads (get, list, watch, console, exec) never wait for them.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use libmachine::agent::Command;
use tokio::sync::{Semaphore, watch};
use tonic::Status;

use crate::checks;
use crate::console::Console;
use crate::guest::{self, Guest};
use crate::proto::{self, MachineState};
use crate::vmm::{Ended, Source, Vm};

/// The most machines machined holds, running or not.
const MAX_MACHINES: usize = 4096;
/// The longest path a Unix socket may have, its NUL past it: macOS's (Linux allows 107).
const MAX_SOCKET_PATH: usize = 103;
/// How long a call waits for libmachine's agent in a guest to answer.
pub(crate) const AGENT_READY: Duration = Duration::from_secs(30);
/// How long StopMachine's sync may take unless its request says.
const SYNC_TIMEOUT: Duration = Duration::from_secs(180);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a machine does now, and how it last ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Current {
    state: MachineState,
    last_exit: Option<proto::Exit>,
}

impl Current {
    fn new(state: MachineState, last_exit: Option<proto::Exit>) -> Self {
        Self { state, last_exit }
    }

    pub(crate) fn to_proto(&self) -> proto::MachineStatus {
        proto::MachineStatus { state: self.state.into(), last_exit: self.last_exit.clone() }
    }

    pub(crate) fn state(&self) -> MachineState {
        self.state
    }

    pub(crate) fn last_exit(&self) -> Option<&proto::Exit> {
        self.last_exit.as_ref()
    }
}

fn exit(reason: proto::ExitReason, detail: impl Into<String>) -> proto::Exit {
    proto::Exit { reason: reason.into(), detail: detail.into() }
}

fn exit_of(ended: Ended) -> proto::Exit {
    use proto::ExitReason as Why;
    match ended {
        Ended::Stopped => exit(Why::Stopped, ""),
        Ended::Poweroff => exit(Why::Poweroff, ""),
        Ended::Reset => exit(Why::Reset, ""),
        Ended::Shutdown => exit(Why::Shutdown, ""),
        Ended::Failed(why) => exit(Why::Failed, why),
    }
}

/// One machine.
pub(crate) struct Entry {
    name: String,
    spec: proto::MachineSpec,
    /// Held by each lifecycle call, so that they take turns.
    lifecycle: tokio::sync::Mutex<()>,
    running: Mutex<Option<Arc<Running>>>,
    status: watch::Sender<Current>,
    console: Arc<Console>,
    /// Set once it is deleted: streams on it end, and lifecycle calls waiting find it gone.
    deleted: watch::Sender<bool>,
}

/// A machine while it runs.
struct Running {
    vm: Vm,
    /// Its places for calls to its guest under way ([`guest::MAX_CALLS`]).
    calls: Arc<Semaphore>,
}

impl Entry {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn status(&self) -> watch::Receiver<Current> {
        self.status.subscribe()
    }

    pub(crate) fn deleted(&self) -> watch::Receiver<bool> {
        self.deleted.subscribe()
    }

    pub(crate) fn console(&self) -> &Arc<Console> {
        &self.console
    }

    fn running(&self) -> Option<Arc<Running>> {
        lock(&self.running).clone()
    }

    /// The running machine, or FAILED_PRECONDITION.
    fn require_running(&self) -> Result<Arc<Running>, Status> {
        self.running().ok_or_else(|| {
            Status::failed_precondition(format!("machine {:?} is not running", self.name))
        })
    }

    /// Its guest, to call, while it runs.
    pub(crate) fn guest(&self) -> Result<Guest, Status> {
        let running = self.require_running()?;
        Ok(Guest::new(running.vm.agent(), running.calls.clone()))
    }

    pub(crate) fn to_proto(&self) -> proto::Machine {
        proto::Machine {
            name: self.name.clone(),
            spec: Some(self.spec.clone()),
            status: Some(self.status.borrow().to_proto()),
            vsock_socket: self
                .running()
                .map(|r| r.vm.vsock_socket().to_string_lossy().into_owned())
                .unwrap_or_default(),
            pid: self.running().map_or(0, |r| r.vm.pid()),
        }
    }

    /// Takes the lifecycle turn, refusing a machine deleted while it waited.
    async fn turn(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, Status> {
        let turn = self.lifecycle.lock().await;
        if *self.deleted.borrow() {
            return Err(Status::not_found(format!("machine {:?} was deleted", self.name)));
        }
        Ok(turn)
    }

    /// `running` ended as `ended`: if it is still this machine's run, it is freed, STOPPED.
    fn finish(&self, running: &Arc<Running>, ended: Ended) {
        let mut current = lock(&self.running);
        if !current.as_ref().is_some_and(|r| Arc::ptr_eq(r, running)) {
            return;
        }
        current.take();
        self.status.send_replace(Current::new(MachineState::Stopped, Some(exit_of(ended))));
    }

    /// Changes its status with `change` (which says whether it changed it), only while `running`
    /// is this machine's run: a run that ended meanwhile keeps the STOPPED its end set.
    fn while_current(&self, running: &Arc<Running>, change: impl FnOnce(&mut Current) -> bool) {
        let current = lock(&self.running);
        if current.as_ref().is_some_and(|r| Arc::ptr_eq(r, running)) {
            self.status.send_if_modified(change);
        }
    }

    /// While `running` is this machine's run and not stopping, its state follows whether its
    /// guest is suspended.
    fn follow_suspension(&self, running: &Arc<Running>, suspended: bool) {
        self.while_current(running, |status| {
            let state = if suspended { MachineState::Suspended } else { MachineState::Running };
            let follows = matches!(status.state, MachineState::Running | MachineState::Suspended);
            let changed = follows && status.state != state;
            if changed {
                status.state = state;
            }
            changed
        });
    }

    /// `running` is STOPPING, if it still runs.
    fn stopping(&self, running: &Arc<Running>) {
        self.while_current(running, |status| {
            status.state = MachineState::Stopping;
            true
        });
    }

    /// Stops `running` and waits until it is freed.
    async fn stop_running(&self, running: &Arc<Running>) {
        self.stopping(running);
        running.vm.stop();
        let ended = running.vm.wait().await;
        self.finish(running, ended);
    }
}

/// A snapshot machined holds: its file, and what its machine was.
#[cfg(target_os = "linux")]
struct Taken {
    path: PathBuf,
    spec: proto::MachineSpec,
}

/// Every machine.
pub(crate) struct Machines {
    images: PathBuf,
    /// The helper binary each machine's VM runs in (`crate::helper`).
    helper: PathBuf,
    /// cladefs's daemon, this process's, which machines' roots are cells of.
    cladefs: libcladefs::Daemon,
    /// Where the daemon serves helpers, as VM processes (its VM-process wire).
    cladefs_socket: PathBuf,
    vsock_dir: PathBuf,
    #[cfg(target_os = "linux")]
    snapshot_dir: PathBuf,
    #[cfg(target_os = "linux")]
    suspend_dir: PathBuf,
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    #[cfg(target_os = "linux")]
    snapshots: Mutex<BTreeMap<String, Arc<Taken>>>,
    /// The next vsock socket's number.
    next_socket: AtomicU32,
}

/// Makes `dir`, and empties it of what an earlier machined left: its own directory under the
/// state directory, nothing else.
fn fresh(dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::create_dir(dir)
}

impl Machines {
    /// machined's machines, none yet, booting kernels from `images`, keeping what it makes in
    /// `state` (made mode 0700, what an earlier machined left in it removed).
    pub(crate) fn new(
        images: &Path,
        helper: &Path,
        state: &Path,
        cladefs: libcladefs::Daemon,
    ) -> io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        if !images.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("the images directory {} is not a directory", images.display()),
            ));
        }
        if !helper.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("the helper binary {} is not there", helper.display()),
            ));
        }
        match std::fs::DirBuilder::new().mode(0o700).create(state) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        // SAFETY: A plain system call, which cannot fail.
        let ours = unsafe { libc::geteuid() };
        if std::fs::metadata(state)?.uid() != ours {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the state directory {} is not machined's user's", state.display()),
            ));
        }
        std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700))?;
        let vsock_dir = state.join("vsock");
        // The longest socket path machined makes or connects to: "<vsock_dir>/<n>_<port>".
        let longest = vsock_dir.join(format!("{}_{}", u32::MAX, u32::MAX));
        if longest.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the state directory {} is too long for its vsock sockets' paths ({} bytes \
                     past the {MAX_SOCKET_PATH} a Unix socket's may have)",
                    state.display(),
                    longest.as_os_str().len() - MAX_SOCKET_PATH
                ),
            ));
        }
        fresh(&vsock_dir)?;
        // Beside the vsock sockets, in a directory only machined's user may enter, made afresh.
        let cladefs_socket = vsock_dir.join("cladefs");
        cladefs.serve_vm_processes(&cladefs_socket).map_err(|e| {
            io::Error::other(format!("serving helpers at {}: {e:#}", cladefs_socket.display()))
        })?;
        #[cfg(target_os = "linux")]
        let (snapshot_dir, suspend_dir) = (state.join("snapshots"), state.join("suspensions"));
        #[cfg(target_os = "linux")]
        {
            fresh(&snapshot_dir)?;
            fresh(&suspend_dir)?;
        }
        Ok(Self {
            images: images.to_owned(),
            helper: helper.to_owned(),
            cladefs,
            cladefs_socket,
            vsock_dir,
            #[cfg(target_os = "linux")]
            snapshot_dir,
            #[cfg(target_os = "linux")]
            suspend_dir,
            entries: Mutex::default(),
            #[cfg(target_os = "linux")]
            snapshots: Mutex::default(),
            next_socket: AtomicU32::new(1),
        })
    }

    /// A new cell forked from the version `from`, of `capacity` bytes, on a disk of the
    /// daemon's: a machine's root, its own.
    pub(crate) async fn fork_cell(
        &self,
        from: libcladefs::CellVersion,
        capacity: u64,
    ) -> Result<proto::Cell, Status> {
        let daemon = self.cladefs.clone();
        // A cell's client's calls block its thread; the client goes as it is made, closing it.
        let forked = tokio::task::spawn_blocking(move || {
            let client = daemon.fork(from.tenant, from, capacity)?;
            Ok::<_, libcladefs::Error>(proto::Cell { tenant: client.tenant(), cell: client.cell() })
        })
        .await
        .map_err(|e| Status::internal(format!("forking a cell: {e}")))?;
        forked.map_err(|e| Status::unavailable(format!("forking {from:?}: {e:#}")))
    }

    /// The machine named `name`.
    pub(crate) fn get(&self, name: &str) -> Result<Arc<Entry>, Status> {
        lock(&self.entries)
            .get(name)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("no machine {name:?}")))
    }

    /// Every machine, by name.
    pub(crate) fn list(&self) -> Vec<Arc<Entry>> {
        lock(&self.entries).values().cloned().collect()
    }

    /// Adds a machine of `spec`, made into `plan`, CREATED.
    fn insert(&self, name: &str, spec: proto::MachineSpec) -> Result<Arc<Entry>, Status> {
        let mut entries = lock(&self.entries);
        if entries.contains_key(name) {
            return Err(Status::already_exists(format!("machine {name:?} exists")));
        }
        if entries.len() >= MAX_MACHINES {
            return Err(Status::resource_exhausted(format!(
                "machined holds {MAX_MACHINES} machines, its most"
            )));
        }
        let entry = Arc::new(Entry {
            name: name.to_owned(),
            spec,
            lifecycle: tokio::sync::Mutex::new(()),
            running: Mutex::new(None),
            status: watch::channel(Current::new(MachineState::Created, None)).0,
            console: Console::new(),
            deleted: watch::channel(false).0,
        });
        entries.insert(name.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Checks that `spec` makes a machine, as [`create`](Self::create) would.
    pub(crate) fn check(&self, spec: &proto::MachineSpec) -> Result<(), Status> {
        checks::plan(&self.images, spec).map(drop)
    }

    /// Makes a machine, CREATED.
    pub(crate) async fn create(
        &self,
        name: String,
        spec: proto::MachineSpec,
    ) -> Result<Arc<Entry>, Status> {
        checks::name("machine", &name)?;
        let images = self.images.clone();
        // Checked here, once, and planned again by its helper as it boots.
        let spec = tokio::task::spawn_blocking(move || {
            checks::plan(&images, &spec)?;
            Ok::<_, Status>(spec)
        })
        .await
        .map_err(|e| Status::internal(format!("checking a spec failed: {e}")))??;
        self.insert(&name, spec)
    }

    /// Boots a CREATED or STOPPED machine.
    pub(crate) async fn start(&self, entry: &Arc<Entry>) -> Result<(), Status> {
        let _turn = entry.turn().await?;
        let state = entry.status.borrow().state;
        if !matches!(state, MachineState::Created | MachineState::Stopped) {
            return Err(Status::failed_precondition(format!(
                "machine {:?} is {}, not CREATED or STOPPED",
                entry.name,
                state.as_str_name()
            )));
        }
        self.boot(entry, Source::Boot).await
    }

    /// Starts `entry` from `source` in a helper of its own, its turn held: RUNNING, or STOPPED
    /// with a BOOT_FAILED exit.
    async fn boot(&self, entry: &Arc<Entry>, source: Source) -> Result<(), Status> {
        entry.status.send_replace(Current::new(MachineState::Starting, None));
        let socket =
            self.vsock_dir.join(self.next_socket.fetch_add(1, Ordering::Relaxed).to_string());
        let console = entry.console.writer();
        let started = Vm::start(
            &self.helper,
            entry.spec.clone(),
            &self.images,
            &self.cladefs_socket,
            source,
            socket,
            console,
        )
        .await;
        let vm = match started {
            Ok(vm) => vm,
            Err(status) => {
                entry.status.send_replace(Current::new(
                    MachineState::Stopped,
                    Some(exit(proto::ExitReason::BootFailed, status.message())),
                ));
                return Err(status);
            }
        };
        let running = Arc::new(Running { vm, calls: Arc::new(Semaphore::new(guest::MAX_CALLS)) });
        *lock(&entry.running) = Some(running.clone());
        entry.status.send_replace(Current::new(MachineState::Running, None));
        let entry = entry.clone();
        tokio::spawn(async move {
            let mut suspended = running.vm.suspended();
            loop {
                tokio::select! {
                    ended = running.vm.wait() => {
                        entry.finish(&running, ended);
                        return;
                    }
                    changed = suspended.changed() => {
                        // Its sender lives as long as the machine: gone, the wait above ends.
                        if changed.is_ok() {
                            let now = *suspended.borrow_and_update();
                            entry.follow_suspension(&running, now);
                        } else {
                            let ended = running.vm.wait().await;
                            entry.finish(&running, ended);
                            return;
                        }
                    }
                }
            }
        });
        Ok(())
    }

    /// Stops a running or suspended machine, syncing its guest's filesystems first if asked.
    pub(crate) async fn stop(
        &self,
        entry: &Arc<Entry>,
        sync: bool,
        timeout: Option<Duration>,
    ) -> Result<(), Status> {
        let _turn = entry.turn().await?;
        let Some(running) = entry.running() else {
            return Ok(());
        };
        if sync {
            let before = entry.status.borrow().state;
            entry.stopping(&running);
            let guest = Guest::new(running.vm.agent(), running.calls.clone());
            if let Err(status) = sync_filesystems(&guest, timeout.unwrap_or(SYNC_TIMEOUT)).await {
                entry.while_current(&running, |s| {
                    let back = s.state == MachineState::Stopping;
                    if back {
                        s.state = before;
                    }
                    back
                });
                return Err(status);
            }
        }
        entry.stop_running(&running).await;
        Ok(())
    }

    /// Deletes a machine: one not running, or with `force`, any, stopped first.
    pub(crate) async fn delete(&self, entry: &Arc<Entry>, force: bool) -> Result<(), Status> {
        let _turn = entry.turn().await?;
        if let Some(running) = entry.running() {
            if !force {
                return Err(Status::failed_precondition(format!(
                    "machine {:?} runs: stop it first, or delete it with force",
                    entry.name
                )));
            }
            entry.stop_running(&running).await;
        }
        lock(&self.entries).remove(&entry.name);
        entry.deleted.send_replace(true);
        Ok(())
    }

    /// As machined goes: stops every machine, waits until each is freed, and removes them all,
    /// which ends the streams on them.
    pub(crate) async fn close(&self) {
        let entries = std::mem::take(&mut *lock(&self.entries));
        // Each in its turn, so that a start under way ends first, and its machine is stopped too.
        let closing = entries.values().map(|entry| async move {
            let _turn = entry.lifecycle.lock().await;
            if let Some(running) = entry.running() {
                entry.stop_running(&running).await;
            }
            entry.deleted.send_replace(true);
        });
        futures::future::join_all(closing).await;
    }

    /// Snapshots a running machine as `snapshot`, replacing one of that name.
    #[cfg(target_os = "linux")]
    pub(crate) async fn snapshot(
        &self,
        entry: &Arc<Entry>,
        snapshot: String,
    ) -> Result<(), Status> {
        checks::name("snapshot", &snapshot)?;
        let _turn = entry.turn().await?;
        let running = entry.require_running()?;
        let path = self.snapshot_dir.join(format!("{snapshot}.snap"));
        running.vm.snapshot(path.clone()).await?;
        let taken = Taken { path, spec: entry.spec.clone() };
        lock(&self.snapshots).insert(snapshot, Arc::new(taken));
        Ok(())
    }

    /// Makes a machine from a snapshot, and starts it.
    #[cfg(target_os = "linux")]
    pub(crate) async fn restore(
        &self,
        snapshot: &str,
        name: String,
        labels: std::collections::HashMap<String, String>,
        vsock_listen_ports: Vec<u32>,
    ) -> Result<Arc<Entry>, Status> {
        checks::name("snapshot", snapshot)?;
        checks::name("machine", &name)?;
        checks::labels(&labels)?;
        checks::vsock_ports(&vsock_listen_ports)?;
        let taken = lock(&self.snapshots)
            .get(snapshot)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("no snapshot {snapshot:?}")))?;
        let spec = proto::MachineSpec { labels, vsock_listen_ports, ..taken.spec.clone() };
        let entry = self.insert(&name, spec)?;
        let booted = async {
            let _turn = entry.turn().await?;
            self.boot(&entry, Source::Snapshot(taken.path.clone())).await
        };
        if let Err(status) = booted.await {
            // Made whole or not at all.
            lock(&self.entries).remove(&name);
            entry.deleted.send_replace(true);
            return Err(status);
        }
        Ok(entry)
    }

    /// Deletes a snapshot.
    #[cfg(target_os = "linux")]
    pub(crate) fn delete_snapshot(&self, snapshot: &str) -> Result<(), Status> {
        checks::name("snapshot", snapshot)?;
        lock(&self.snapshots)
            .remove(snapshot)
            .ok_or_else(|| Status::not_found(format!("no snapshot {snapshot:?}")))?;
        // Machines restored from it map it privately: they keep what they mapped.
        let path = self.snapshot_dir.join(format!("{snapshot}.snap"));
        std::fs::remove_file(&path)
            .map_err(|e| Status::internal(format!("removing {}: {e}", path.display())))
    }

    /// A running machine's balloon: its target set to `target` bytes, if given, and how it stands.
    pub(crate) async fn balloon(
        &self,
        entry: &Arc<Entry>,
        target: Option<u64>,
    ) -> Result<proto::BalloonStatus, Status> {
        if !entry.spec.balloon {
            return Err(Status::failed_precondition(format!(
                "machine {:?} has no balloon: its spec asks for none",
                entry.name
            )));
        }
        entry.require_running()?.vm.balloon(target).await
    }

    /// Suspends a running machine.
    #[cfg(target_os = "linux")]
    pub(crate) async fn suspend(&self, entry: &Arc<Entry>) -> Result<(), Status> {
        let _turn = entry.turn().await?;
        let running = entry.require_running()?;
        running.vm.suspend(self.suspend_dir.clone()).await
    }

    /// Wakes a suspended machine.
    #[cfg(target_os = "linux")]
    pub(crate) async fn resume(&self, entry: &Arc<Entry>) -> Result<(), Status> {
        let _turn = entry.turn().await?;
        let running = entry.require_running()?;
        running.vm.resume().await
    }
}

/// Hypervisor.framework's machines can't be snapshotted or suspended yet.
#[cfg(not(target_os = "linux"))]
fn unsupported(what: &str) -> Status {
    Status::unimplemented(format!(
        "{what}: machines on Hypervisor.framework can't be snapshotted or suspended yet (board \
         13399)"
    ))
}

#[cfg(not(target_os = "linux"))]
impl Machines {
    pub(crate) async fn snapshot(&self, _: &Arc<Entry>, _: String) -> Result<(), Status> {
        Err(unsupported("SnapshotMachine"))
    }

    pub(crate) async fn restore(
        &self,
        _: &str,
        _: String,
        _: std::collections::HashMap<String, String>,
        _: Vec<u32>,
    ) -> Result<Arc<Entry>, Status> {
        Err(unsupported("RestoreMachine"))
    }

    pub(crate) fn delete_snapshot(&self, _: &str) -> Result<(), Status> {
        Err(unsupported("DeleteSnapshot"))
    }

    pub(crate) async fn suspend(&self, _: &Arc<Entry>) -> Result<(), Status> {
        Err(unsupported("SuspendMachine"))
    }

    pub(crate) async fn resume(&self, _: &Arc<Entry>) -> Result<(), Status> {
        Err(unsupported("ResumeMachine"))
    }
}

/// Runs `sync` in the guest through its agent, within `timeout`.
async fn sync_filesystems(guest: &Guest, timeout: Duration) -> Result<(), Status> {
    // Each wait of the agent bounded too, so that its thread ends with the call.
    let synced = tokio::time::timeout(
        timeout,
        guest.call_waiting(timeout, move |agent| {
            agent.wait_ready(AGENT_READY.min(timeout))?;
            agent.exec(&Command::new("sync"))
        }),
    )
    .await
    .map_err(|_| {
        Status::deadline_exceeded(format!("the guest's sync took more than {timeout:?}"))
    })??;
    if synced.status.success() {
        Ok(())
    } else {
        Err(Status::failed_precondition(format!(
            "the guest's sync failed ({:?}): {}",
            synced.status,
            String::from_utf8_lossy(&synced.stderr)
        )))
    }
}
