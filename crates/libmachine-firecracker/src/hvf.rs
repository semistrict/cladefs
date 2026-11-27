// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The VMM itself (see the crate's docs).

use std::io::{BufRead, BufReader, Read as _, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use libmachine::vsock_unix::{GuestPorts, Server};
use libmachine::{
    CellId, Disk, Error, ExitReason, ExtentCache, ExtentCell, ExtentDaemon, ExtentStore, LinuxBoot,
    Machine, MachineConfig,
};
use serde_json::Value;

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

fn run(args: &Args, log: &Log) -> Result<i32, String> {
    // A socket that answers, as the tests wait for it to; every request refused but for now.
    let _api = match &args.api {
        Some(path) => Some(serve_api(path, log.clone())?),
        None => None,
    };
    let Some(config_path) = &args.config else {
        return Err("no --config-file: loading a snapshot through the API isn't supported".into());
    };
    let text = std::fs::read(config_path)
        .map_err(|e| format!("reading {}: {e}", config_path.display()))?;
    let config: Value =
        serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", config_path.display()))?;
    let plan = plan(&config)?;
    let mut machine =
        Machine::boot_linux(plan.config, &plan.boot).map_err(|e| format!("booting: {e}"))?;
    // A file, as the tests give it: what the serial port says, as it says it.
    machine.set_console(std::io::stdout());
    let _vsock = match &plan.vsock {
        Some(path) => {
            let vsock = machine.vsock().ok_or("the machine has no vsock")?;
            let _ = std::fs::remove_file(path);
            let listener =
                UnixListener::bind(path).map_err(|e| format!("binding {}: {e}", path.display()))?;
            Some(
                Server::start(listener, path.clone(), &vsock, GuestPorts::Any)
                    .map_err(|e| format!("serving the vsock at {}: {e}", path.display()))?,
            )
        }
        None => None,
    };
    match machine.run() {
        Ok(ExitReason::Reset | ExitReason::Poweroff) => Ok(0),
        Err(Error::ExtentStore(why)) => {
            log.say(&format!("the extent store device stopped the machine: {why}"));
            Ok(CLADEFSD_LOST)
        }
        Ok(other) => Err(format!("the machine stopped: {other:?}")),
        Err(e) => Err(format!("the machine failed: {e}")),
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
        let path = string(device, "path_on_host")?;
        let cell = match path.parse::<CellId>() {
            Ok(cell) if cell.id == 0 => ExtentCell::New { tenant: cell.tenant },
            Ok(cell) => ExtentCell::Existing(cell),
            Err(_) => ExtentCell::Directory(path.into()),
        };
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
    let config = MachineConfig {
        mem_size: usize::try_from(mem_mib << 20).map_err(|e| e.to_string())?,
        vcpus: u32::try_from(vcpus).map_err(|e| e.to_string())?,
        extent_daemon: daemon.map(|socket| Arc::new(ExtentDaemon::at_socket(socket))),
        extent_stores: stores,
        balloon: config.get("balloon").is_some(),
        disks,
        ..Default::default()
    };
    Ok(Plan { config, boot: LinuxBoot { kernel: kernel.into(), initrd, cmdline }, vsock })
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

/// Answers Firecracker's API at `path`, on a thread of its own, refusing every request (400, its
/// fault message saying why): the tests wait for the socket to answer before they drive it.
fn serve_api(path: &Path, log: Log) -> Result<(), String> {
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    std::thread::Builder::new()
        .name("api".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Err(e) = refuse(stream, &log) {
                    log.say(&format!("the API: {e}"));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reads one request and refuses it.
fn refuse(stream: UnixStream, log: &Log) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
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
    std::io::copy(&mut (&mut reader).take(len as u64), &mut std::io::sink())?;
    let request = request.trim_end();
    log.say(&format!("refused API request {request:?}"));
    let body = serde_json::json!({
        "fault_message": format!(
            "{request}: not supported by libmachine-firecracker (snapshots, pause and migration \
             need libmachine's snapshots on Hypervisor.framework)"
        ),
    })
    .to_string();
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
