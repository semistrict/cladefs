// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined's machines, by name: each one's spec, its state, its console, and, while it runs, its
//! [`Vm`] (in a helper process of its own) and vsock socket. A machine's lifecycle calls (start,
//! delete) take turns on it; reads (get, list, console) never wait for them.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;
use tonic::Status;

use crate::checks;
use crate::console::Console;
use crate::proto::{self, MachineState};
use crate::vmm::{Ended, Vm};

/// The most machines machined holds, running or not.
const MAX_MACHINES: usize = 4096;
/// The longest path a Unix socket may have, its NUL past it: macOS's (Linux allows 107).
const MAX_SOCKET_PATH: usize = 103;

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
}

impl Entry {
    pub(crate) fn deleted(&self) -> watch::Receiver<bool> {
        self.deleted.subscribe()
    }

    pub(crate) fn console(&self) -> &Arc<Console> {
        &self.console
    }

    fn running(&self) -> Option<Arc<Running>> {
        lock(&self.running).clone()
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

/// Every machine.
pub(crate) struct Machines {
    images: PathBuf,
    /// The helper binary each machine's VM runs in (`crate::helper`).
    helper: PathBuf,
    /// Where the daemon serves helpers, as VM processes (its VM-process wire).
    cladefs_socket: PathBuf,
    vsock_dir: PathBuf,
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
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
        Ok(Self {
            images: images.to_owned(),
            helper: helper.to_owned(),
            cladefs_socket,
            vsock_dir,
            entries: Mutex::default(),
            next_socket: AtomicU32::new(1),
        })
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
        self.boot(entry).await
    }

    /// Starts `entry` in a helper of its own, its turn held: RUNNING, or STOPPED with a
    /// BOOT_FAILED exit.
    async fn boot(&self, entry: &Arc<Entry>) -> Result<(), Status> {
        entry.status.send_replace(Current::new(MachineState::Starting, None));
        let socket =
            self.vsock_dir.join(self.next_socket.fetch_add(1, Ordering::Relaxed).to_string());
        let console = entry.console.writer();
        let started = Vm::start(
            &self.helper,
            entry.spec.clone(),
            &self.images,
            &self.cladefs_socket,
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
        let running = Arc::new(Running { vm });
        *lock(&entry.running) = Some(running.clone());
        entry.status.send_replace(Current::new(MachineState::Running, None));
        let entry = entry.clone();
        tokio::spawn(async move {
            let ended = running.vm.wait().await;
            entry.finish(&running, ended);
        });
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
}
