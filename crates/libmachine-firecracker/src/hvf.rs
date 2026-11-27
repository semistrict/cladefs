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
    Machine, MachineConfig, Snapshot, SnapshotOptions, StopHandle,
};
use serde_json::{Value, json};

/// Firecracker's exit code once its extent daemon goes (`FcExitCode::CladefsdLost`).
const CLADEFSD_LOST: i32 = 158;

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
        Err(e) => {
            log.say(&e);
            1
        }
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
}

/// A full snapshot: its state at `snapshot`, the guest's memory at `memory`, as Firecracker's
/// memory file holds it (libmachine's RAM file of its own).
struct Create {
    snapshot: PathBuf,
    memory: PathBuf,
    /// cladefs's `for_migration`: the extent store devices' cells handed over whole.
    for_migration: bool,
}

/// A snapshot to load, as `PUT /snapshot/load` says.
struct Load {
    snapshot: PathBuf,
    memory: PathBuf,
    resume: bool,
    /// Where the vsock is served: the snapshot's, unless overridden.
    vsock: Option<PathBuf>,
    /// cladefs's `cladefs_overrides`: the cell each extent store device opens.
    overrides: Vec<Override>,
}

/// An extent store device's cell in the loaded machine, and its daemon's socket.
struct Override {
    id: String,
    path_on_host: String,
    cladefsd_socket: Option<String>,
}

/// A request of the API's, and where its answer goes: an error a 400's fault message.
struct Request {
    action: Action,
    reply: mpsc::Sender<Result<(), String>>,
}

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

/// The VMM's microVM: its machine, and what serves it.
struct Vmm {
    machine: Machine,
    kept: Kept,
    _vsock: Option<Server>,
    paused: bool,
}

fn run(args: &Args, log: &Log) -> Result<i32, String> {
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
    fn boot(path: &Path) -> Result<Self, String> {
        let text = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let config: Value =
            serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let plan = plan(&config)?;
        let machine =
            Machine::boot_linux(plan.config, &plan.boot).map_err(|e| format!("booting: {e}"))?;
        let kept = Kept { vsock: plan.vsock, cladefsd_socket: plan.cladefsd_socket };
        Self::serving(machine, kept, false)
    }

    /// The microVM around `machine`: its console on standard output (a file, as the tests give
    /// it), its vsock served, paused if `paused`.
    fn serving(mut machine: Machine, kept: Kept, paused: bool) -> Result<Self, String> {
        machine.set_console(std::io::stdout());
        let vsock = kept.vsock.as_deref().map(|path| serve_vsock(&machine, path)).transpose()?;
        Ok(Self { machine, kept, _vsock: vsock, paused })
    }

    /// Waits for the API's `PUT /snapshot/load`, refusing every other request meanwhile, and
    /// loads the snapshot it names. A load that fails is refused, and the next awaited.
    fn await_load(rx: &mpsc::Receiver<Request>) -> Result<Self, String> {
        loop {
            let request = rx.recv().map_err(|_| "the API went before a snapshot was loaded")?;
            let Action::Load(load) = request.action else {
                let _ = request.reply.send(Err("no microVM yet: load a snapshot first".into()));
                continue;
            };
            match Self::load(load) {
                Ok(vmm) => {
                    // Its requester's answer: the API thread waits for it.
                    let _ = request.reply.send(Ok(()));
                    return Ok(vmm);
                }
                Err(e) => {
                    let _ = request.reply.send(Err(e));
                }
            }
        }
    }

    /// The microVM `load`'s snapshot holds, its extent store devices on the cells and daemon it
    /// says, its vsock at its own path or the snapshot's.
    fn load(load: Load) -> Result<Self, String> {
        let snapshot = Snapshot::open_split(&load.snapshot, &load.memory)
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
                        ));
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
        let machine = Machine::restore(&snapshot, config)
            .map_err(|e| format!("loading the snapshot: {e}"))?;
        Self::serving(machine, kept, !load.resume)
    }

    /// Runs the microVM, serving the API's requests between its runs, until it ends: 0 once
    /// its guest resets or powers off, 158 once its extent daemon goes.
    fn serve(
        mut self,
        rx: &mpsc::Receiver<Request>,
        interrupt: &Interrupt,
        log: &Log,
    ) -> Result<i32, String> {
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
                    Ok(other) => return Err(format!("the machine stopped: {other:?}")),
                    Err(e) => return Err(format!("the machine failed: {e}")),
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

    fn handle(&mut self, action: Action) -> Result<(), String> {
        match action {
            Action::Vm { paused } => {
                self.paused = paused;
                Ok(())
            }
            Action::Create(create) => {
                if !self.paused {
                    return Err("the microVM must be paused to snapshot it".into());
                }
                let kept = serde_json::to_vec(&self.kept).map_err(|e| e.to_string())?;
                let options = SnapshotOptions {
                    ram: Some(&create.memory),
                    for_migration: create.for_migration,
                    caller_data: &kept,
                };
                self.machine
                    .snapshot_with(&create.snapshot, &options)
                    .map_err(|e| format!("creating the snapshot: {e}"))
            }
            Action::Load(_) => {
                Err("a microVM runs already: load a snapshot in a new process".into())
            }
            Action::Balloon { amount_mib } => {
                let balloon = self.machine.balloon().ok_or("the microVM has no balloon")?;
                balloon.set_target(amount_mib << 20);
                Ok(())
            }
        }
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
    if !array(config, "pmem")?.is_empty() {
        return Err("pmem devices aren't supported on Hypervisor.framework yet".into());
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
        Ok(()) => write!(stream, "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"),
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
            if body.get("memory_cell").is_some() {
                return Err(MEMORY_CELLS.into());
            }
            match body["snapshot_type"].as_str().unwrap_or("Full") {
                "Full" => {}
                "Diff" => return Err(format!("Diff snapshots aren't supported: {MEMORY_CELLS}")),
                other => return Err(format!("no snapshot type {other:?}")),
            }
            Ok(Action::Create(Create {
                snapshot: string(&body, "snapshot_path")?.into(),
                memory: string(&body, "mem_file_path")?.into(),
                for_migration: body["for_migration"].as_bool().unwrap_or(false),
            }))
        }
        ("PUT", "/snapshot/load") => {
            if body.get("memory_cell").is_some() {
                return Err(MEMORY_CELLS.into());
            }
            let memory = match (&body["mem_backend"], &body["mem_file_path"]) {
                (Value::Null, Value::String(path)) => path.clone(),
                (backend, _) => match backend["backend_type"].as_str() {
                    Some("File") => string(backend, "backend_path")?,
                    Some("Cell") => return Err(MEMORY_CELLS.into()),
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
                memory: memory.into(),
                resume: body["resume_vm"].as_bool().unwrap_or(false),
                vsock: body["vsock_override"]["uds_path"].as_str().map(PathBuf::from),
                overrides,
            }))
        }
        ("PATCH", "/balloon") => Ok(Action::Balloon { amount_mib: number(&body, "amount_mib")? }),
        _ => Err(format!("{method} {path} isn't supported by libmachine-firecracker")),
    }
}

/// Why what needs memory cells is refused: macOS machines keep anonymous RAM until a macOS design
/// of them is chosen (board 13994, the user's choice of 2026-11-26).
const MEMORY_CELLS: &str =
    "memory cells aren't supported on macOS yet (board 13994): snapshot to a memory file";
