// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The VMM itself (see the crate's docs).

use std::io::{BufRead, BufReader, Read as _, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use libmachine::vsock_unix::{GuestPorts, Server};
use libmachine::{
    Disk, Error, ExitReason, ExtentCache, ExtentCell, ExtentDaemon, ExtentStore, LinuxBoot,
    Machine, MachineConfig, Snapshot, SnapshotOptions, SnapshotRam, StopHandle,
};
use serde_json::{Value, json};

use crate::memory::CellMemory;

/// Firecracker's exit code once its extent daemon goes (`FcExitCode::CladefsdLost`).
const CLADEFSD_LOST: i32 = 158;
/// Firecracker's exit code once a guest's access to a DAX window is refused for good
/// (`FcExitCode::AccessRefused`): a page nothing maps there, or a store past `private_max`.
const ACCESS_REFUSED: i32 = 159;

/// What its command line says.
#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    api: Option<PathBuf>,
    log: Option<PathBuf>,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} takes a value"));
            match arg.as_str() {
                "--config-file" => parsed.config = Some(value()?.into()),
                "--api-sock" => parsed.api = Some(value()?.into()),
                "--log-path" => parsed.log = Some(value()?.into()),
                // Every device is PCI here, and macOS has no seccomp; the level, Firecracker's
                // filter of its own log, says nothing of what this logs, which is little.
                "--enable-pci" | "--no-api" => {}
                "--level" | "--seccomp-filter" | "--id" => drop(value()?),
                other => return Err(format!("libmachine-firecracker takes no {other}")),
            }
        }
        Ok(parsed)
    }
}

/// Where what it says goes: standard error, and its log file, if it has one.
#[derive(Clone)]
struct Log(Arc<Mutex<Option<std::fs::File>>>);

impl Log {
    fn open(path: Option<&Path>) -> Self {
        let file = path
            .and_then(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).ok());
        Self(Arc::new(Mutex::new(file)))
    }

    fn say(&self, what: &str) {
        eprintln!("libmachine-firecracker: {what}");
        if let Some(file) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(file, "libmachine-firecracker: {what}");
        }
    }
}

/// The VM a configuration file describes, as libmachine makes it.
struct Plan {
    config: MachineConfig,
    boot: LinuxBoot,
    vsock: Option<PathBuf>,
    cladefsd_socket: Option<String>,
}

pub(crate) fn main() -> i32 {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("libmachine-firecracker: {e}");
            return 1;
        }
    };
    let log = Log::open(args.log.as_deref());
    match run(&args, &log) {
        Ok(code) => code,
        Err(Ending::DaemonLost(why)) => {
            log.say(&why);
            CLADEFSD_LOST
        }
        Err(Ending::Failed(why)) => {
            log.say(&why);
            1
        }
    }
}

/// Why the VMM ends short of its guest, which its log says: its extent daemon gone, which it
/// exits [`CLADEFSD_LOST`] for, as Firecracker does, at boot too, its cell opening as the daemon
/// went (TASK-232); or anything else, 1.
enum Ending {
    DaemonLost(String),
    Failed(String),
}

impl From<String> for Ending {
    fn from(why: String) -> Self {
        Self::Failed(why)
    }
}

impl From<&str> for Ending {
    fn from(why: &str) -> Self {
        Self::Failed(why.to_owned())
    }
}

/// The ending of a machine's call that failed with `e`, doing `what`.
fn ending(what: &str, e: Error) -> Ending {
    match e {
        Error::ExtentDaemonLost(_) => Ending::DaemonLost(format!("{what}: {e}")),
        e => Ending::Failed(format!("{what}: {e}")),
    }
}

/// What the API asks of the VMM, which its main thread does, between the machine's runs.
enum Action {
    /// `PATCH /vm`: paused, or resumed.
    Vm { paused: bool },
    /// `PUT /snapshot/create`.
    Create(Create),
    /// `PUT /snapshot/load`, in a process that has no microVM yet.
    Load(Load),
    /// `PATCH /balloon`: the balloon's target.
    Balloon { amount_mib: u64 },
    /// cladefs's `PATCH /cladefs/extents`: the extent store devices' cells let go of
    /// (`Released`), for a live migration's hand-over, or taken back (`Open`), the migration
    /// aborted.
    Extents { released: bool },
}

/// A snapshot: its state at `snapshot`, the guest's memory where `memory` says.
struct Create {
    snapshot: PathBuf,
    memory: MemoryTo,
    /// cladefs's `for_migration`: the extent store devices' cells handed over whole.
    for_migration: bool,
}

/// Where a snapshot's memory goes.
enum MemoryTo {
    /// All of it, as Firecracker's memory file holds it (libmachine's RAM file of its own).
    File(PathBuf),
    /// What the guest wrote since the last snapshot, a memory cell's window's, at its offsets in
    /// a sparse file of the memory's size, as Firecracker's `Diff` snapshot writes it.
    Diff(PathBuf),
    /// cladefs's `memory_cell` (TASK-456.4): into a memory cell, its window's own if the microVM
    /// was restored from one, else a new one at `path_on_host` of the daemon at
    /// `cladefsd_socket`.
    Cell { path_on_host: Option<PathBuf>, cladefsd_socket: Option<PathBuf>, ephemeral: bool },
}

/// A snapshot to load, as `PUT /snapshot/load` says.
struct Load {
    snapshot: PathBuf,
    memory: MemoryFrom,
    resume: bool,
    /// Where the vsock is served: the snapshot's, unless overridden.
    vsock: Option<PathBuf>,
    /// cladefs's `cladefs_overrides`: the cell each extent store device opens.
    overrides: Vec<Override>,
}

/// Where a loaded snapshot's memory is.
enum MemoryFrom {
    /// A memory file, mapped privately.
    File(PathBuf),
    /// cladefs's `Cell` memory backend (TASK-456.3): a memory cell, at the backend's path, the
    /// guest's RAM its window.
    Cell(crate::memory::LoadCell),
}

/// An extent store device's cell in the loaded machine, and its daemon's socket.
struct Override {
    id: String,
    path_on_host: String,
    cladefsd_socket: Option<String>,
}

/// A request of the API's, and where its answer goes: a body, a 200's, or none, a 204; an error
/// a 400's fault message.
struct Request {
    action: Action,
    reply: mpsc::Sender<Answer>,
}

/// What a request is answered.
type Answer = Result<Option<Value>, String>;

/// What stops the machine's run, for a request to be served: the running machine's.
#[derive(Clone, Default)]
struct Interrupt(Arc<Mutex<Option<StopHandle>>>);

impl Interrupt {
    fn set(&self, stop: StopHandle) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop);
    }

    fn interrupt(&self) {
        if let Some(stop) = &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            stop.stop();
        }
    }
}

/// What libmachine-firecracker keeps in its snapshots besides libmachine's machine (as the
/// snapshot's caller data), which a load uses unless it says otherwise: as Firecracker's vmstate
/// keeps its devices' host sides.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Kept {
    /// The vsock's `uds_path`.
    vsock: Option<PathBuf>,
    /// The extent store devices' daemon's socket.
    cladefsd_socket: Option<String>,
}

/// The VMM's microVM: its machine, and what serves it. The machine goes first, its VM with it,
/// then its memory's cell, if any, whose window lets go of the RAM.
struct Vmm {
    machine: Machine,
    kept: Kept,
    _vsock: Option<Server>,
    paused: bool,
    /// Its memory as a memory cell's window, if it was restored from one.
    memory: Option<CellMemory>,
    /// Whether its extent store devices' cells are let go of: it stays paused until they are
    /// taken back.
    released: bool,
}

fn run(args: &Args, log: &Log) -> Result<i32, Ending> {
    let (requests, rx) = mpsc::channel();
    let interrupt = Interrupt::default();
    if let Some(path) = &args.api {
        serve_api(path, log.clone(), requests, interrupt.clone())?;
    }
    let vmm = match &args.config {
        Some(config) => Vmm::boot(config)?,
        None if args.api.is_some() => Vmm::await_load(&rx)?,
        None => return Err("neither --config-file nor --api-sock: nothing to run".into()),
    };
    vmm.serve(&rx, &interrupt, log)
}

/// Serves the vsock of `machine` at `path` in Firecracker's protocol.
fn serve_vsock(machine: &Machine, path: &Path) -> Result<Server, String> {
    let vsock = machine.vsock().ok_or("the machine has no vsock")?;
    // A socket left by an earlier VMM at the path is no one's now.
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    Server::start(listener, path.to_owned(), &vsock, GuestPorts::Any)
        .map_err(|e| format!("serving the vsock at {}: {e}", path.display()))
}

impl Vmm {
    /// Boots the microVM the configuration file at `path` describes.
    fn boot(path: &Path) -> Result<Self, Ending> {
        let text = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let config: Value =
            serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let plan = plan(&config)?;
        let machine =
            Machine::boot_linux(plan.config, &plan.boot).map_err(|e| ending("booting", e))?;
        let kept = Kept { vsock: plan.vsock, cladefsd_socket: plan.cladefsd_socket };
        Ok(Self::serving(machine, kept, false, None)?)
    }

    /// The microVM around `machine`: its console on standard output (a file, as the tests give
    /// it), its vsock served, paused if `paused`, its memory `memory`'s window if given.
    fn serving(
        mut machine: Machine,
        kept: Kept,
        paused: bool,
        memory: Option<CellMemory>,
    ) -> Result<Self, String> {
        machine.set_console(std::io::stdout());
        let vsock = kept.vsock.as_deref().map(|path| serve_vsock(&machine, path)).transpose()?;
        Ok(Self { machine, kept, _vsock: vsock, paused, memory, released: false })
    }

    /// Waits for the API's `PUT /snapshot/load`, refusing every other request meanwhile, and
    /// loads the snapshot it names. A load that fails is refused, and the next awaited; one that
    /// failed as the extent daemon went ends the VMM, as Firecracker's does.
    fn await_load(rx: &mpsc::Receiver<Request>) -> Result<Self, Ending> {
        loop {
            let request = rx.recv().map_err(|_| "the API went before a snapshot was loaded")?;
            let Action::Load(load) = request.action else {
                let _ = request.reply.send(Err("no microVM yet: load a snapshot first".into()));
                continue;
            };
            match Self::load(load) {
                Ok(vmm) => {
                    // Its requester's answer: the API thread waits for it.
                    let _ = request.reply.send(Ok(None));
                    return Ok(vmm);
                }
                Err(Ending::DaemonLost(why)) => {
                    let _ = request.reply.send(Err(why.clone()));
                    return Err(Ending::DaemonLost(why));
                }
                Err(Ending::Failed(why)) => {
                    let _ = request.reply.send(Err(why));
                }
            }
        }
    }

    /// The microVM `load`'s snapshot holds, its extent store devices on the cells and daemon it
    /// says, its vsock at its own path or the snapshot's.
    fn load(load: Load) -> Result<Self, Ending> {
        let snapshot = match &load.memory {
            MemoryFrom::File(memory) => Snapshot::open_split(&load.snapshot, memory),
            MemoryFrom::Cell(_) => Snapshot::open_without_ram(&load.snapshot),
        }
        .map_err(|e| format!("opening the snapshot: {e}"))?;
        let mut kept: Kept = serde_json::from_slice(snapshot.caller_data()).map_err(|e| {
            format!("{} is no snapshot of libmachine-firecracker's: {e}", load.snapshot.display())
        })?;
        let mut config = snapshot.config();
        for o in &load.overrides {
            let store = (config.extent_stores.iter_mut())
                .find(|store| store.id == o.id)
                .ok_or_else(|| format!("the snapshot has no extent store device {:?}", o.id))?;
            store.cell = ExtentCell::from_path_on_host(&o.path_on_host);
            if let Some(socket) = &o.cladefsd_socket {
                match &kept.cladefsd_socket {
                    Some(set) if set != socket && config.extent_stores.len() > 1 => {
                        return Err(format!(
                            "extent devices on two daemons ({set}, {socket}) aren't supported"
                        )
                        .into());
                    }
                    _ => kept.cladefsd_socket = Some(socket.clone()),
                }
            }
        }
        if !config.extent_stores.is_empty() {
            let socket = kept.cladefsd_socket.clone().ok_or("the snapshot names no daemon")?;
            config.extent_daemon = Some(Arc::new(ExtentDaemon::at_socket(socket)));
        }
        if let Some(vsock) = load.vsock {
            kept.vsock = Some(vsock);
        }
        let (machine, memory) = match load.memory {
            MemoryFrom::File(_) => (Machine::restore(&snapshot, config), None),
            MemoryFrom::Cell(cell) => {
                let memory = CellMemory::open(&cell, config.mem_size)?;
                let machine = Machine::restore_onto(&snapshot, config, memory.ram().clone());
                (machine, Some(memory))
            }
        };
        let machine = machine.map_err(|e| ending("loading the snapshot", e))?;
        if let Some(memory) = &memory {
            memory.take_given_back();
        }
        Ok(Self::serving(machine, kept, !load.resume, memory)?)
    }

    /// Runs the microVM, serving the API's requests between its runs, until it ends: 0 once
    /// its guest resets or powers off, 158 once its extent daemon goes, 159 once a guest's access
    /// to a DAX window is refused for good.
    fn serve(
        mut self,
        rx: &mpsc::Receiver<Request>,
        interrupt: &Interrupt,
        log: &Log,
    ) -> Result<i32, Ending> {
        interrupt.set(self.machine.stop_handle());
        loop {
            if !self.paused {
                // A request ends the run (`Interrupt`), and is served below.
                match self.machine.run() {
                    Ok(ExitReason::Stopped) => {}
                    Ok(ExitReason::Reset | ExitReason::Poweroff) => return Ok(0),
                    Err(Error::ExtentStore(why)) => {
                        log.say(&format!("the extent store device stopped the machine: {why}"));
                        return Ok(CLADEFSD_LOST);
                    }
                    Err(e @ Error::ExtentDaemonLost(_)) => return Err(ending("running", e)),
                    Err(Error::AccessRefused(why)) => {
                        log.say(&format!("the extent store device stopped the machine: {why}"));
                        return Ok(ACCESS_REFUSED);
                    }
                    Ok(other) => return Err(format!("the machine stopped: {other:?}").into()),
                    Err(e) => return Err(format!("the machine failed: {e}").into()),
                }
            }
            let request = match self.paused {
                true => rx.recv().map_err(|_| "the API went with the microVM paused")?,
                false => match rx.try_recv() {
                    Ok(request) => request,
                    // A stop with nothing asked: one left from a request served while paused.
                    Err(_) => continue,
                },
            };
            let outcome = self.handle(request.action);
            // Its requester's answer: the API thread waits for it.
            let _ = request.reply.send(outcome);
        }
    }

    fn handle(&mut self, action: Action) -> Answer {
        match action {
            Action::Vm { paused: false } if self.released => {
                Err("the extent store devices' cells are released: open them first (PATCH \
                 /cladefs/extents)"
                    .into())
            }
            Action::Vm { paused } => {
                self.paused = paused;
                Ok(None)
            }
            Action::Extents { .. } if !self.paused => {
                Err("the microVM must be paused to release or open its cells".into())
            }
            Action::Extents { released: true } => {
                self.machine
                    .release_extent_cells()
                    .map_err(|e| format!("releasing the extent store devices' cells: {e}"))?;
                self.released = true;
                Ok(None)
            }
            Action::Extents { released: false } => {
                self.machine
                    .reopen_extent_cells()
                    .map_err(|e| format!("opening the extent store devices' cells: {e}"))?;
                self.released = false;
                Ok(None)
            }
            Action::Create(create) => {
                if !self.paused {
                    return Err("the microVM must be paused to snapshot it".into());
                }
                self.create(create)
            }
            Action::Load(_) => {
                Err("a microVM runs already: load a snapshot in a new process".into())
            }
            Action::Balloon { amount_mib } => {
                let balloon = self.machine.balloon().ok_or("the microVM has no balloon")?;
                balloon.set_target(amount_mib << 20);
                Ok(None)
            }
        }
    }

    /// Takes the snapshot `create` asks for of the paused microVM: its state, then its memory
    /// where it says. A memory cell's answered as Firecracker answers it, with the cell and its
    /// version.
    fn create(&mut self, create: Create) -> Answer {
        // What the memory asks is checked before anything is written.
        match (&create.memory, &self.memory) {
            (MemoryTo::Diff(_), None) => {
                return Err(format!(
                    "Diff snapshots are of a microVM restored from a memory cell: \
                     {MEMORY_CELL_ONLY}"
                ));
            }
            (MemoryTo::Cell { path_on_host: None, cladefsd_socket: None, .. }, Some(_)) => {}
            (MemoryTo::Cell { .. }, Some(_)) => {
                return Err("a microVM restored from a memory cell snapshots into its own forked \
                            cell: omit path_on_host and cladefsd_socket"
                    .into());
            }
            (MemoryTo::Cell { path_on_host: Some(_), cladefsd_socket: Some(_), .. }, None) => {}
            (MemoryTo::Cell { .. }, None) => {
                return Err("snapshotting a microVM not restored from a memory cell into one \
                            takes both path_on_host and cladefsd_socket"
                    .into());
            }
            (MemoryTo::File(_) | MemoryTo::Diff(_), _) => {}
        }
        let ram = match &create.memory {
            MemoryTo::File(path) => SnapshotRam::File(path),
            MemoryTo::Diff(_) | MemoryTo::Cell { .. } => SnapshotRam::Kept,
        };
        let kept = serde_json::to_vec(&self.kept).map_err(|e| e.to_string())?;
        let options =
            SnapshotOptions { ram, for_migration: create.for_migration, caller_data: &kept };
        let memory = &mut self.memory;
        // The memory as of the state's moment, the devices held meanwhile: none writes it.
        let made = self.machine.snapshot_with_ram(&create.snapshot, &options, |base, size| {
            // SAFETY (each below): The microVM is paused and its devices held; `base` is its RAM,
            // `size` bytes, its memory cell's window if it has one.
            match (&create.memory, memory) {
                // The next diff is of what the guest writes from now.
                (MemoryTo::File(_), Some(memory)) => {
                    unsafe { memory.snapshotted(base) }.map(|()| None)
                }
                (MemoryTo::File(_), None) => Ok(None),
                (MemoryTo::Diff(path), Some(memory)) => {
                    unsafe { memory.write_diff(base, path) }.map(|()| None)
                }
                (MemoryTo::Cell { ephemeral, .. }, Some(memory)) => {
                    unsafe { memory.snapshot(base, *ephemeral) }.map(Some)
                }
                (
                    MemoryTo::Cell {
                        path_on_host: Some(path),
                        cladefsd_socket: Some(socket),
                        ephemeral,
                    },
                    None,
                ) => unsafe {
                    crate::memory::snapshot_into_new(socket, path, base, size, *ephemeral)
                }
                .map(Some),
                (MemoryTo::Diff(_) | MemoryTo::Cell { .. }, _) => unreachable!("checked above"),
            }
        });
        let made = made.map_err(|e| format!("creating the snapshot: {e}"))?;
        // As Firecracker's: the vsock's connections reset, its host ends closed at once, the
        // guest told as it runs again (a transport reset), as a restore of the snapshot is.
        if let Some(vsock) = self.machine.vsock() {
            vsock.reset_connections();
        }
        Ok(made)
    }
}

/// The VM `config`, Firecracker's configuration, describes.
fn plan(config: &Value) -> Result<Plan, String> {
    let object = config.as_object().ok_or("the configuration is not a JSON object")?;
    for key in object.keys() {
        let known = [
            "boot-source",
            "machine-config",
            "drives",
            "cladefs",
            "vsock",
            "balloon",
            "pmem",
            "logger",
            "metrics",
        ];
        if !known.contains(&key.as_str()) {
            return Err(format!("the configuration's {key:?} isn't supported"));
        }
    }
    let mut pmems = Vec::new();
    for device in array(config, "pmem")? {
        if device["root_device"].as_bool().unwrap_or(false) {
            return Err("a pmem root device isn't supported".into());
        }
        let mut pmem = libmachine::Pmem::new(string(device, "path_on_host")?);
        pmem.read_only = device["read_only"].as_bool().unwrap_or(false);
        pmems.push(pmem);
    }
    let machine = &config["machine-config"];
    let vcpus = number(machine, "vcpu_count")?;
    let mem_mib = number(machine, "mem_size_mib")?;
    let source = &config["boot-source"];
    let kernel = string(source, "kernel_image_path")?;
    let mut cmdline = source["boot_args"].as_str().unwrap_or_default().to_owned();
    let initrd = source["initrd_path"].as_str().map(PathBuf::from);

    // The root disk first, as Firecracker attaches it: /dev/vda.
    let mut drives = array(config, "drives")?.to_vec();
    drives.sort_by_key(|drive| !drive["is_root_device"].as_bool().unwrap_or(false));
    let mut disks = Vec::new();
    for drive in &drives {
        let read_only = drive["is_read_only"].as_bool().unwrap_or(false);
        if drive["is_root_device"].as_bool().unwrap_or(false) {
            cmdline = format!("root=/dev/vda {} {cmdline}", if read_only { "ro" } else { "rw" });
        }
        let mut disk = Disk::new(string(drive, "path_on_host")?);
        disk.read_only = read_only;
        disk.unsafe_flushes = drive["cache_type"].as_str() == Some("Unsafe");
        disk.id = drive["drive_id"].as_str().unwrap_or_default().chars().take(20).collect();
        disks.push(disk);
    }

    let mut daemon: Option<String> = None;
    let mut stores = Vec::new();
    for device in array(config, "cladefs")? {
        let socket = string(device, "cladefsd_socket")?;
        match &daemon {
            Some(served) if *served != socket => {
                return Err(format!(
                    "extent devices on two daemons ({served}, {socket}) aren't supported"
                ));
            }
            _ => daemon = Some(socket),
        }
        let cell = ExtentCell::from_path_on_host(&string(device, "path_on_host")?);
        let mut store = ExtentStore::new(string(device, "tag")?, cell, number(device, "capacity")?);
        if let Some(id) = device["id"].as_str() {
            store.id = id.to_owned();
        }
        store.dax_window_size = device["dax_window_size"].as_u64();
        store.dax_private_max_bytes = device["dax_private_max_bytes"].as_u64();
        store.cache = match device["cache_type"].as_str() {
            Some("Unsafe") => ExtentCache::Unsafe,
            _ => ExtentCache::Writeback,
        };
        stores.push(store);
    }

    let vsock = config["vsock"]["uds_path"].as_str().map(PathBuf::from);
    let daemon_socket = daemon.clone();
    let config = MachineConfig {
        mem_size: usize::try_from(mem_mib << 20).map_err(|e| e.to_string())?,
        vcpus: u32::try_from(vcpus).map_err(|e| e.to_string())?,
        extent_daemon: daemon.map(|socket| Arc::new(ExtentDaemon::at_socket(socket))),
        extent_stores: stores,
        balloon: config.get("balloon").is_some(),
        disks,
        pmems,
        ..Default::default()
    };
    let boot = LinuxBoot { kernel: kernel.into(), initrd, cmdline };
    Ok(Plan { config, boot, vsock, cladefsd_socket: daemon_socket })
}

fn array<'a>(config: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match &config[key] {
        Value::Null => Ok(&[]),
        Value::Array(items) => Ok(items),
        _ => Err(format!("the configuration's {key:?} is not an array")),
    }
}

fn number(object: &Value, key: &str) -> Result<u64, String> {
    object[key].as_u64().ok_or_else(|| format!("no number {key:?} in {object}"))
}

fn string(object: &Value, key: &str) -> Result<String, String> {
    object[key].as_str().map(str::to_owned).ok_or_else(|| format!("no string {key:?} in {object}"))
}

/// Answers Firecracker's API at `path`, on a thread of its own: each request read, made an
/// [`Action`] for the main thread, which `interrupt` stops the machine's run for, and answered as
/// Firecracker answers it (204, or 400 with a fault message). The tests open a connection a
/// request.
fn serve_api(
    path: &Path,
    log: Log,
    requests: mpsc::Sender<Request>,
    interrupt: Interrupt,
) -> Result<(), String> {
    // A socket left by an earlier VMM at the path is no one's now.
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    std::thread::Builder::new()
        .name("api".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Err(e) = answer(stream, &requests, &interrupt, &log) {
                    log.say(&format!("the API: {e}"));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reads one request from `stream`, has it served, and answers it.
fn answer(
    stream: UnixStream,
    requests: &mpsc::Sender<Request>,
    interrupt: &Interrupt,
    log: &Log,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    // A connection closed with no request: a caller seeing whether the API is up.
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut len = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            len = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = Vec::new();
    (&mut reader).take(len as u64).read_to_end(&mut body)?;
    let mut words = line.split_whitespace();
    let (method, path) = (words.next().unwrap_or_default(), words.next().unwrap_or_default());
    let outcome = action(method, path, &body).and_then(|action| {
        let (reply, answered) = mpsc::channel();
        requests.send(Request { action, reply }).map_err(|_| "the microVM is gone".to_owned())?;
        interrupt.interrupt();
        answered.recv().map_err(|_| "the microVM went before answering".to_owned())?
    });
    let mut stream = stream;
    match outcome {
        Ok(None) => write!(stream, "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"),
        Ok(Some(body)) => {
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n\
                 {body}",
                body.len()
            )
        }
        Err(why) => {
            log.say(&format!("refused {method} {path}: {why}"));
            let body = json!({ "fault_message": why }).to_string();
            write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\n\r\n{body}",
                body.len()
            )
        }
    }
}

/// What `method` on `path` with `body` asks, as Firecracker's API takes it; what this VMM doesn't
/// do is refused, saying why.
fn action(method: &str, path: &str, body: &[u8]) -> Result<Action, String> {
    let body: Value = match body {
        [] => Value::Null,
        body => serde_json::from_slice(body).map_err(|e| format!("the request's body: {e}"))?,
    };
    match (method, path) {
        ("PATCH", "/vm") => match body["state"].as_str() {
            Some("Paused") => Ok(Action::Vm { paused: true }),
            Some("Resumed") => Ok(Action::Vm { paused: false }),
            other => Err(format!("no VM state {other:?}")),
        },
        ("PUT", "/snapshot/create") => {
            let diff = match body["snapshot_type"].as_str().unwrap_or("Full") {
                "Full" => false,
                "Diff" => true,
                other => return Err(format!("no snapshot type {other:?}")),
            };
            let memory = match (&body["memory_cell"], &body["mem_file_path"]) {
                (Value::Null, Value::String(path)) if diff => MemoryTo::Diff(path.into()),
                (Value::Null, Value::String(path)) => MemoryTo::File(path.into()),
                (Value::Object(_), Value::Null) if diff => {
                    return Err("a Diff snapshot goes to mem_file_path, not a memory cell".into());
                }
                (cell @ Value::Object(_), Value::Null) => {
                    let path = |key| cell[key].as_str().map(PathBuf::from);
                    MemoryTo::Cell {
                        path_on_host: path("path_on_host"),
                        cladefsd_socket: path("cladefsd_socket"),
                        ephemeral: cell["ephemeral"].as_bool().unwrap_or(false),
                    }
                }
                (Value::Null, Value::Null) => {
                    return Err("a snapshot names mem_file_path or memory_cell".into());
                }
                _ => return Err("mem_file_path and memory_cell exclude each other".into()),
            };
            Ok(Action::Create(Create {
                snapshot: string(&body, "snapshot_path")?.into(),
                memory,
                for_migration: body["for_migration"].as_bool().unwrap_or(false),
            }))
        }
        ("PUT", "/snapshot/load") => {
            let memory = match (&body["mem_backend"], &body["mem_file_path"]) {
                (Value::Null, Value::String(path)) => MemoryFrom::File(path.into()),
                (backend, _) => match backend["backend_type"].as_str() {
                    Some("File") => MemoryFrom::File(string(backend, "backend_path")?.into()),
                    Some("Cell") => MemoryFrom::Cell(crate::memory::LoadCell::parse(
                        &string(backend, "backend_path")?,
                        &body["memory_cell"],
                    )?),
                    other => return Err(format!("no memory backend {other:?}")),
                },
            };
            let overrides = array(&body, "cladefs_overrides")?
                .iter()
                .map(|o| {
                    Ok(Override {
                        id: string(o, "id")?,
                        path_on_host: string(o, "path_on_host")?,
                        cladefsd_socket: o["cladefsd_socket"].as_str().map(str::to_owned),
                    })
                })
                .collect::<Result<_, String>>()?;
            Ok(Action::Load(Load {
                snapshot: string(&body, "snapshot_path")?.into(),
                memory,
                resume: body["resume_vm"].as_bool().unwrap_or(false),
                vsock: body["vsock_override"]["uds_path"].as_str().map(PathBuf::from),
                overrides,
            }))
        }
        ("PATCH", "/balloon") => Ok(Action::Balloon { amount_mib: number(&body, "amount_mib")? }),
        ("PATCH", "/cladefs/extents") => match body["state"].as_str() {
            Some("Released") => Ok(Action::Extents { released: true }),
            Some("Open") => Ok(Action::Extents { released: false }),
            other => Err(format!("no extent store devices' state {other:?}")),
        },
        _ => Err(format!("{method} {path} isn't supported by libmachine-firecracker")),
    }
}

/// What only a microVM restored from a memory cell has: its window's private pages, which are
/// what its guest wrote, are what a diff snapshot holds.
const MEMORY_CELL_ONLY: &str = "a cold-booted microVM's memory tracks no writes on macOS";
