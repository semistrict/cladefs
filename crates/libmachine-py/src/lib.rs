// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Python bindings for libmachine, exposed as `libmachine._libmachine` and
//! re-exported from the `libmachine` package.
//!
//! libmachine's platforms alone: x86-64 Linux, for now (board 13063).
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use pyo3::create_exception;
use pyo3::exceptions::{
    PyConnectionResetError, PyException, PyOSError, PyTimeoutError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use std::collections::HashMap;
use std::ffi::OsString;
use std::time::Duration;

create_exception!(
    _libmachine,
    MachineError,
    PyException,
    "Raised when the hypervisor or VMM fails."
);

create_exception!(
    _libmachine,
    GuestBusyError,
    MachineError,
    "Raised when a guest that is not idle is asked to suspend."
);

/// OSError(errno, strerror, filename), whose subclass Python picks from
/// the errno (e.g. FileNotFoundError).
fn os_error(source: &std::io::Error, filename: impl Into<String>) -> PyErr {
    let filename = filename.into();
    match source.raw_os_error() {
        Some(errno) => {
            let message = source.to_string();
            let strerror = message
                .strip_suffix(&format!(" (os error {errno})"))
                .unwrap_or(&message)
                .to_owned();
            PyOSError::new_err((errno, strerror, filename))
        }
        None => PyOSError::new_err(format!("{filename}: {source}")),
    }
}

fn to_py_err(err: libmachine::Error) -> PyErr {
    match err {
        libmachine::Error::Guest { what, source } => os_error(&source, what),
        libmachine::Error::ConnectionReset(_) | libmachine::Error::GuestGone(_) => {
            PyConnectionResetError::new_err(err.to_string())
        }
        libmachine::Error::TimedOut(_) => PyTimeoutError::new_err(err.to_string()),
        libmachine::Error::Busy(_) => GuestBusyError::new_err(err.to_string()),
        libmachine::Error::InvalidConfig(_)
        | libmachine::Error::GuestMemory(_)
        | libmachine::Error::Boot(_) => PyValueError::new_err(err.to_string()),
        // OSError(errno, strerror, filename): Python picks the subclass from
        // the errno, e.g. FileNotFoundError.
        libmachine::Error::Io { path, source } => match source.raw_os_error() {
            Some(errno) => {
                let message = source.to_string();
                let strerror = message
                    .strip_suffix(&format!(" (os error {errno})"))
                    .unwrap_or(&message)
                    .to_owned();
                PyOSError::new_err((errno, strerror, path))
            }
            None => PyOSError::new_err(format!("{}: {source}", path.display())),
        },
        other => MachineError::new_err(other.to_string()),
    }
}

/// cladefs's extent daemon, which this process's machines share: made once, given to each
/// machine with extent store devices (`Machine.boot_linux(extent_daemon=...)`).
/// `ExtentDaemon(socket)` is the daemon listening at `socket` (a host's cladefsd, or a test's);
/// `ExtentDaemon.local(dir)` and `ExtentDaemon.from_config(path)` boot one in this process, its
/// one for its life.
#[pyclass(module = "libmachine", frozen)]
struct ExtentDaemon(std::sync::Arc<libmachine::ExtentDaemon>);

#[pymethods]
impl ExtentDaemon {
    #[new]
    fn new(socket: std::path::PathBuf) -> Self {
        Self(std::sync::Arc::new(libmachine::ExtentDaemon::at_socket(socket)))
    }

    /// Boots a daemon in this process keeping everything in `dir`: one local disk, and a bucket
    /// of local files in it. `memory` is its pages' and caches' budget in bytes (the host's
    /// governs it if None); its keys are under the KMS at `kms`, or the master key in the file
    /// `master_key_file`.
    #[staticmethod]
    #[pyo3(signature = (dir, *, memory = None, kms = None, master_key_file = None))]
    fn local(
        dir: std::path::PathBuf,
        memory: Option<u64>,
        kms: Option<String>,
        master_key_file: Option<std::path::PathBuf>,
    ) -> PyResult<Self> {
        let mut config = libmachine::ExtentDaemonConfig::local(dir);
        if let Some(memory) = memory {
            config = config.memory(memory);
        }
        if let Some(kms) = kms {
            config = config.kms(kms);
        }
        if let Some(key) = master_key_file {
            config = config.master_key_file(key);
        }
        Self::start(config)
    }

    /// Boots a daemon in this process serving as the cladefsd configuration file at `path` says.
    #[staticmethod]
    fn from_config(path: std::path::PathBuf) -> PyResult<Self> {
        let config = libmachine::ExtentDaemonConfig::from_file(path)
            .map_err(|err| PyValueError::new_err(format!("{err:#}")))?;
        Self::start(config)
    }
}

impl ExtentDaemon {
    fn start(config: libmachine::ExtentDaemonConfig) -> PyResult<Self> {
        let daemon = libmachine::ExtentDaemon::start(config).map_err(to_py_err)?;
        Ok(Self(std::sync::Arc::new(daemon)))
    }
}

/// An extent store device: the cell in the directory `cell` (created with `capacity` bytes if it
/// isn't there), or, `cell` None, a new cell of `tenant`'s on the daemon's disks, which the guest
/// mounts its Fxfs from by `tag` (`mount -t cladefs TAG DIR`). `dax_window_size`
/// (a multiple of 2 MiB) gives it a DAX window, `dax_private_max_bytes` bounds the window's
/// private pages, and `cache` ("writeback", the default, or "unsafe") says whether the guest's
/// syncs wait for the bucket. `id` names it in errors, its tag by default.
#[pyclass(module = "libmachine", frozen)]
struct ExtentStore(libmachine::ExtentStore);

#[pymethods]
impl ExtentStore {
    #[new]
    #[pyo3(signature = (
        tag,
        cell,
        capacity,
        *,
        dax_window_size = None,
        dax_private_max_bytes = None,
        cache = "writeback",
        id = None,
        tenant = None,
    ))]
    #[allow(clippy::too_many_arguments, reason = "Python's keyword arguments")]
    fn new(
        tag: String,
        cell: Option<std::path::PathBuf>,
        capacity: u64,
        dax_window_size: Option<u64>,
        dax_private_max_bytes: Option<u64>,
        cache: &str,
        id: Option<String>,
        tenant: Option<u64>,
    ) -> PyResult<Self> {
        let cell = match (cell, tenant) {
            (Some(dir), None) => libmachine::ExtentCell::Directory(dir),
            (None, Some(tenant)) => libmachine::ExtentCell::New { tenant },
            _ => {
                return Err(PyValueError::new_err(
                    "an extent store is on a cell directory or a new cell of a tenant's: \
                     cell or tenant, one of them",
                ));
            }
        };
        let cache = match cache {
            "writeback" => libmachine::ExtentCache::Writeback,
            "unsafe" => libmachine::ExtentCache::Unsafe,
            other => {
                return Err(PyValueError::new_err(format!(
                    "cache is \"writeback\" or \"unsafe\", not {other:?}"
                )));
            }
        };
        let mut store = libmachine::ExtentStore::new(tag, cell, capacity);
        if let Some(id) = id {
            store.id = id;
        }
        store.dax_window_size = dax_window_size;
        store.dax_private_max_bytes = dax_private_max_bytes;
        store.cache = cache;
        Ok(Self(store))
    }

    #[getter]
    fn tag(&self) -> &str {
        &self.0.tag
    }
}

/// Why `Machine.run()` returned.
///
/// `kind` is one of: "poweroff", "reset", "stopped", "timed_out", "halt",
/// "shutdown", "unhandled_io", "unhandled_mmio", "fail_entry",
/// "internal_error", "other"; for how an AsyncSandbox ended also "closed"
/// (closed or garbage-collected) and "failed" (running it failed, the
/// error in `detail`). Fields that do not apply to a kind are None.
#[pyclass(module = "libmachine", frozen, get_all)]
#[derive(Clone)]
struct Exit {
    kind: &'static str,
    port: Option<u16>,
    addr: Option<u64>,
    size: Option<u8>,
    write: Option<bool>,
    reason: Option<u64>,
    detail: Option<String>,
}

impl Exit {
    fn simple(kind: &'static str) -> Self {
        Self { kind, port: None, addr: None, size: None, write: None, reason: None, detail: None }
    }
}

impl From<libmachine::ExitReason> for Exit {
    fn from(r: libmachine::ExitReason) -> Self {
        use libmachine::ExitReason as R;
        match r {
            R::Poweroff => Exit::simple("poweroff"),
            R::Reset => Exit::simple("reset"),
            R::Stopped => Exit::simple("stopped"),
            R::TimedOut => Exit::simple("timed_out"),
            R::Halt => Exit::simple("halt"),
            R::Shutdown => Exit::simple("shutdown"),
            R::UnhandledIo { port, size, write } => Exit {
                port: Some(port),
                size: Some(size),
                write: Some(write),
                ..Exit::simple("unhandled_io")
            },
            R::UnhandledMmio { addr, size, write } => Exit {
                addr: Some(addr),
                size: Some(size),
                write: Some(write),
                ..Exit::simple("unhandled_mmio")
            },
            R::FailEntry { reason } => Exit { reason: Some(reason), ..Exit::simple("fail_entry") },
            R::InternalError => Exit::simple("internal_error"),
            R::Other(detail) => Exit { detail: Some(detail), ..Exit::simple("other") },
            other => Exit { detail: Some(format!("{other:?}")), ..Exit::simple("other") },
        }
    }
}

#[pymethods]
impl Exit {
    fn __repr__(&self) -> String {
        let mut parts = vec![format!("kind={:?}", self.kind)];
        if let Some(p) = self.port {
            parts.push(format!("port={p:#x}"));
        }
        if let Some(a) = self.addr {
            parts.push(format!("addr={a:#x}"));
        }
        if let Some(s) = self.size {
            parts.push(format!("size={s}"));
        }
        if let Some(w) = self.write {
            parts.push(format!("write={}", if w { "True" } else { "False" }));
        }
        if let Some(r) = self.reason {
            parts.push(format!("reason={r:#x}"));
        }
        if let Some(d) = &self.detail {
            parts.push(format!("detail={d:?}"));
        }
        format!("Exit({})", parts.join(", "))
    }
}

/// A vCPU count from Python: any int out of range is a ValueError, like the
/// counts libmachine refuses (rather than an OverflowError for some).
fn vcpu_count(vcpus: i64) -> PyResult<u32> {
    u32::try_from(vcpus).map_err(|_| {
        PyValueError::new_err(format!(
            "vcpus must be from 1 to {}, got {vcpus}",
            libmachine::MachineConfig::MAX_VCPUS
        ))
    })
}

/// How often `run()` checks for Python signals while the guest runs.
const SIGNAL_CHECK_INTERVAL: Duration = Duration::from_millis(50);

/// Stops a Machine's `run()` from any thread; see `Machine.stop_handle()`.
/// A stop requested while the machine is not running is kept: the next
/// `run()` returns "stopped" at once.
#[pyclass(module = "libmachine", frozen)]
struct StopHandle(libmachine::StopHandle);

#[pymethods]
impl StopHandle {
    /// Ask the machine to stop; returns without waiting for `run()`.
    fn stop(&self) {
        self.0.stop();
    }
}

/// x86_64 general-purpose registers of a vCPU.
#[pyclass(module = "libmachine", frozen, get_all)]
struct Registers {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rsp: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
}

impl From<libmachine::Registers> for Registers {
    fn from(r: libmachine::Registers) -> Self {
        Self {
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rsp: r.rsp,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rip: r.rip,
            rflags: r.rflags,
        }
    }
}

#[pymethods]
impl Registers {
    fn __repr__(&self) -> String {
        format!(
            "Registers(rax={:#x}, rbx={:#x}, rcx={:#x}, rdx={:#x}, rip={:#x}, rflags={:#x})",
            self.rax, self.rbx, self.rcx, self.rdx, self.rip, self.rflags
        )
    }
}

/// Seconds as a Duration; None stays None.
fn seconds(what: &str, t: Option<f64>) -> PyResult<Option<Duration>> {
    match t {
        None => Ok(None),
        Some(t) if t.is_finite() && t > 0.0 => Ok(Some(Duration::from_secs_f64(t))),
        Some(t) => Err(PyValueError::new_err(format!(
            "{what} must be a finite number of seconds > 0, got {t}"
        ))),
    }
}

/// How a command in the guest ended, and its output.
#[pyclass(module = "libmachine", frozen)]
struct ExecResult {
    status: libmachine::agent::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[pymethods]
impl ExecResult {
    /// Its exit status, or None if a signal killed it.
    #[getter]
    fn exit_code(&self) -> Option<i32> {
        self.status.code
    }

    /// The signal that killed it, or None.
    #[getter]
    fn signal(&self) -> Option<i32> {
        self.status.signal
    }

    #[getter]
    fn stdout<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.stdout)
    }

    #[getter]
    fn stderr<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.stderr)
    }

    fn __repr__(&self) -> String {
        format!(
            "ExecResult(exit_code={:?}, signal={:?}, stdout={} bytes, stderr={} bytes)",
            self.status.code,
            self.status.signal,
            self.stdout.len(),
            self.stderr.len()
        )
    }
}

impl From<libmachine::agent::Output> for ExecResult {
    fn from(o: libmachine::agent::Output) -> Self {
        Self { status: o.status, stdout: o.stdout, stderr: o.stderr }
    }
}

/// The command for `argv`: the program, its arguments; `env` (if given)
/// is its whole environment, else PATH alone.
fn command(
    argv: Vec<OsString>,
    env: Option<HashMap<OsString, OsString>>,
    cwd: Option<OsString>,
) -> PyResult<libmachine::agent::Command> {
    let mut argv = argv.into_iter();
    let program = argv.next().ok_or_else(|| PyValueError::new_err("argv is empty"))?;
    let mut cmd = libmachine::agent::Command::new(program);
    cmd.args(argv);
    if let Some(env) = env {
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    Ok(cmd)
}

/// libmachine's guest agent, reached over the machine's vsock; see
/// `Machine.agent()`. Its methods release the GIL while they wait for the
/// guest, which only answers while the machine runs (in another thread).
///
/// Errors: OSError (its errno the guest's) when the operation failed in
/// the guest, ConnectionResetError when the guest is gone (it powered off,
/// reset or triple-faulted) or a command could not be taken up again after
/// a reset (the agent restarted or forgot it), TimeoutError past `timeout`,
/// and MachineError when the agent cannot be reached. A command whose
/// connection is reset is taken up again on a new one: it runs once.
#[pyclass(module = "libmachine", frozen)]
struct Agent(libmachine::agent::Agent);

impl Agent {
    fn with_timeout(&self, timeout: Option<f64>) -> PyResult<libmachine::agent::Agent> {
        Ok(self.0.clone().timeout(seconds("timeout", timeout)?))
    }
}

#[pymethods]
impl Agent {
    /// Check that the agent answers.
    #[pyo3(signature = (*, timeout = None))]
    fn ping(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<()> {
        let agent = self.with_timeout(timeout)?;
        py.detach(|| agent.ping()).map_err(to_py_err)
    }

    /// Wait until the agent answers (the guest is booting), at most
    /// `timeout` seconds.
    #[pyo3(signature = (timeout = 30.0))]
    fn wait_ready(&self, py: Python<'_>, timeout: f64) -> PyResult<()> {
        let timeout = seconds("timeout", Some(timeout))?.unwrap();
        py.detach(|| self.0.wait_ready(timeout)).map_err(to_py_err)
    }

    /// Run `argv` in the guest to its end. `env` is its whole environment
    /// (default: PATH alone); `stdin` bytes are fed to it (default:
    /// /dev/null). `timeout` bounds each wait for the guest, not the
    /// command.
    #[pyo3(signature = (argv, *, env = None, cwd = None, stdin = None, timeout = None))]
    fn exec(
        &self,
        py: Python<'_>,
        argv: Vec<OsString>,
        env: Option<HashMap<OsString, OsString>>,
        cwd: Option<OsString>,
        stdin: Option<&[u8]>,
        timeout: Option<f64>,
    ) -> PyResult<ExecResult> {
        let mut cmd = command(argv, env, cwd)?;
        if let Some(data) = stdin {
            cmd.stdin(data);
        }
        let agent = self.with_timeout(timeout)?;
        py.detach(|| agent.exec(&cmd)).map(ExecResult::from).map_err(to_py_err)
    }

    /// Start `argv` in the guest, its stdin a pipe fed through the Process.
    #[pyo3(signature = (argv, *, env = None, cwd = None, timeout = None))]
    fn spawn(
        &self,
        py: Python<'_>,
        argv: Vec<OsString>,
        env: Option<HashMap<OsString, OsString>>,
        cwd: Option<OsString>,
        timeout: Option<f64>,
    ) -> PyResult<Process> {
        let cmd = command(argv, env, cwd)?;
        let agent = self.with_timeout(timeout)?;
        py.detach(|| agent.spawn(&cmd)).map(Process).map_err(to_py_err)
    }

    /// The contents of the guest's file `path`.
    #[pyo3(signature = (path, *, timeout = None))]
    fn read_file<'py>(
        &self,
        py: Python<'py>,
        path: OsString,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let agent = self.with_timeout(timeout)?;
        let data = py.detach(|| agent.read_file(&path)).map_err(to_py_err)?;
        Ok(PyBytes::new(py, &data))
    }

    /// Make the guest's file `path` hold `data`, with permissions `mode`
    /// (whole, or not at all).
    #[pyo3(signature = (path, data, mode = 0o644, *, timeout = None))]
    fn write_file(
        &self,
        py: Python<'_>,
        path: OsString,
        data: &[u8],
        mode: u32,
        timeout: Option<f64>,
    ) -> PyResult<()> {
        let agent = self.with_timeout(timeout)?;
        py.detach(|| agent.write_file(&path, data, mode)).map_err(to_py_err)
    }
}

/// A command running in the guest; see `Agent.spawn()`. Usable from
/// several threads at once (one feeding stdin, one reading). Once it is
/// garbage, it releases the command: the agent kills it if it still runs.
#[pyclass(module = "libmachine", frozen)]
struct Process(libmachine::agent::Process);

#[pymethods]
impl Process {
    /// Its process ID in the guest (also its process group's).
    #[getter]
    fn pid(&self) -> u32 {
        self.0.pid()
    }

    /// Feed `data` to its stdin.
    fn write_stdin(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        py.detach(|| self.0.write_stdin(data)).map_err(to_py_err)
    }

    /// Close its stdin.
    fn close_stdin(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.0.close_stdin()).map_err(to_py_err)
    }

    /// Send `signal` to its process group.
    fn signal(&self, py: Python<'_>, signal: i32) -> PyResult<()> {
        py.detach(|| self.0.signal(signal)).map_err(to_py_err)
    }

    /// Kill its process group.
    fn kill(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.0.kill()).map_err(to_py_err)
    }

    /// What it does next: ("stdout", bytes), ("stderr", bytes), or
    /// ("exit", (exit_code, signal)) once, after all its output; then None.
    fn read<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, pyo3::types::PyTuple>>> {
        use libmachine::agent::Event;
        let event = py.detach(|| self.0.next_event()).map_err(to_py_err)?;
        let Some(event) = event else {
            return Ok(None);
        };
        let tuple = match event {
            Event::Stdout(d) => ("stdout", PyBytes::new(py, &d)).into_pyobject(py)?,
            Event::Stderr(d) => ("stderr", PyBytes::new(py, &d)).into_pyobject(py)?,
            Event::Exit(status) => ("exit", (status.code, status.signal)).into_pyobject(py)?,
        };
        Ok(Some(tuple))
    }

    /// Wait for it to end: its exit and the output not read yet.
    fn wait(&self, py: Python<'_>) -> PyResult<ExecResult> {
        py.detach(|| self.0.wait()).map(ExecResult::from).map_err(to_py_err)
    }

    fn __repr__(&self) -> String {
        format!("Process(pid={})", self.0.pid())
    }
}

/// The host's end of a machine's virtio-vsock; see `Machine.vsock()`.
#[pyclass(module = "libmachine", frozen)]
struct Vsock(libmachine::Vsock);

#[pymethods]
impl Vsock {
    /// The guest's CID.
    #[getter]
    fn guest_cid(&self) -> u64 {
        self.0.guest_cid()
    }

    /// Reset every connection, as a restore from a snapshot does: for
    /// testing how code handles it.
    fn reset_connections(&self) {
        self.0.reset_connections();
    }
}

/// Which destinations the guest's connections may reach: rules in order,
/// the first that matches deciding, anything no rule matches denied. An
/// "ask" rule hands the connection to the network's decider
/// (`Network.set_decider`), and denies it if there is none.
///
/// A host is "*" (every globally routable address), an IP address, a CIDR
/// network ("10.0.0.0/8"), or a DNS name ("pypi.org", or "*.pypi.org" for
/// every name under it) that the guest resolved through the VMM's DNS.
/// "*" and names never match the host's loopback, private or link-local
/// addresses: those need an explicit IP or CIDR rule, unless the name is a
/// host entry of the network's (`NetworkConfig(hosts=...)`). Ports are None
/// (every port), a port, a (first, last) range, or a string ("443",
/// "8000-8080", "*").
#[pyclass(module = "libmachine")]
#[derive(Clone, Default)]
struct EgressPolicy {
    inner: libmachine::net::EgressPolicy,
}

fn host_arg(host: &str) -> PyResult<libmachine::net::HostPattern> {
    host.parse().map_err(to_py_err)
}

fn ports_arg(ports: Option<&Bound<'_, PyAny>>) -> PyResult<libmachine::net::Ports> {
    use libmachine::net::Ports;
    let Some(ports) = ports else {
        return Ok(Ports::ANY);
    };
    if let Ok(port) = ports.extract::<u16>() {
        return Ok(Ports::from(port));
    }
    if let Ok((first, last)) = ports.extract::<(u16, u16)>() {
        return Ports::range(first, last).map_err(to_py_err);
    }
    if let Ok(s) = ports.extract::<String>() {
        return s.parse().map_err(to_py_err);
    }
    Err(PyValueError::new_err(format!(
        "ports must be None, a port from 0 to 65535, a (first, last) tuple or a string, got {ports}"
    )))
}

#[pymethods]
impl EgressPolicy {
    /// A policy that allows nothing; add rules with `allow()` and `deny()`.
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Every globally routable address on every port (still not the
    /// host's loopback or private networks).
    #[staticmethod]
    fn allow_all() -> Self {
        Self { inner: libmachine::net::EgressPolicy::allow_all() }
    }

    /// Append a rule allowing `host` on `ports`; returns the policy.
    #[pyo3(signature = (host, ports = None))]
    fn allow<'py>(
        slf: PyRefMut<'py, Self>,
        host: &str,
        ports: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        Self::push(slf, libmachine::net::Action::Allow, host, ports)
    }

    /// Append a rule denying `host` on `ports`; returns the policy.
    #[pyo3(signature = (host, ports = None))]
    fn deny<'py>(
        slf: PyRefMut<'py, Self>,
        host: &str,
        ports: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        Self::push(slf, libmachine::net::Action::Deny, host, ports)
    }

    /// Append a rule asking the network's decider about connections to
    /// `host` on `ports`; returns the policy.
    #[pyo3(signature = (host, ports = None))]
    fn ask<'py>(
        slf: PyRefMut<'py, Self>,
        host: &str,
        ports: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        Self::push(slf, libmachine::net::Action::Ask, host, ports)
    }

    /// The rules, in order: ("allow", "deny" or "ask", host, (first port,
    /// last port)).
    #[getter]
    fn rules(&self) -> Vec<(String, String, (u16, u16))> {
        self.inner
            .rules()
            .iter()
            .map(|r| (r.action.to_string(), r.host.to_string(), (r.ports.first(), r.ports.last())))
            .collect()
    }

    /// What the policy does with a connection to `addr` (an IP address) on
    /// `port`: "allow", "deny" or "ask", the guest having resolved `names`
    /// to it through the VMM's DNS, and host entries giving it
    /// `host_names`.
    #[pyo3(signature = (addr, port, names = Vec::new(), host_names = Vec::new()))]
    fn check(
        &self,
        addr: &str,
        port: u16,
        names: Vec<String>,
        host_names: Vec<String>,
    ) -> PyResult<String> {
        let addr: std::net::IpAddr =
            addr.parse().map_err(|e| PyValueError::new_err(format!("{addr:?}: {e}")))?;
        let dest =
            libmachine::net::Destination { addr, port, names: &names, host_names: &host_names };
        Ok(self.inner.check(&dest).action.to_string())
    }

    /// Whether `check()` says "allow".
    #[pyo3(signature = (addr, port, names = Vec::new(), host_names = Vec::new()))]
    fn allows(
        &self,
        addr: &str,
        port: u16,
        names: Vec<String>,
        host_names: Vec<String>,
    ) -> PyResult<bool> {
        Ok(self.check(addr, port, names, host_names)? == "allow")
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    fn __repr__(&self) -> String {
        format!("EgressPolicy({})", self.inner)
    }
}

impl EgressPolicy {
    fn push<'py>(
        mut slf: PyRefMut<'py, Self>,
        action: libmachine::net::Action,
        host: &str,
        ports: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let rule =
            libmachine::net::Rule { action, host: host_arg(host)?, ports: ports_arg(ports)? };
        slf.inner.push(rule);
        Ok(slf)
    }
}

/// `{name: address or [addresses]}` as host entries.
fn hosts_arg(hosts: Option<&Bound<'_, PyDict>>) -> PyResult<libmachine::net::Hosts> {
    let mut out = libmachine::net::Hosts::new();
    let Some(hosts) = hosts else {
        return Ok(out);
    };
    for (name, addrs) in hosts.iter() {
        let name: String = name.extract()?;
        let addrs: Vec<String> = match addrs.extract::<String>() {
            Ok(one) => vec![one],
            Err(_) => addrs.extract().map_err(|_| {
                PyTypeError::new_err(format!(
                    "host entry {name:?}: an IPv4 address or a list of them, got {addrs}"
                ))
            })?,
        };
        for addr in addrs {
            let ip: std::net::Ipv4Addr = addr.parse().map_err(|e| {
                PyValueError::new_err(format!("host entry {name:?}: {addr:?}: {e}"))
            })?;
            out.insert(&name, ip).map_err(to_py_err)?;
        }
    }
    Ok(out)
}

fn hosts_dict(hosts: &libmachine::net::Hosts) -> HashMap<String, Vec<String>> {
    hosts.iter().map(|(n, a)| (n.to_owned(), a.iter().map(|a| a.to_string()).collect())).collect()
}

fn verdict_arg(verdict: &str) -> PyResult<libmachine::net::Verdict> {
    match verdict {
        "allow" => Ok(libmachine::net::Verdict::Allow),
        "deny" => Ok(libmachine::net::Verdict::Deny),
        _ => Err(PyValueError::new_err(format!(
            "a verdict is \"allow\" or \"deny\", got {verdict:?}"
        ))),
    }
}

fn verdict_str(verdict: libmachine::net::Verdict) -> &'static str {
    match verdict {
        libmachine::net::Verdict::Allow => "allow",
        libmachine::net::Verdict::Deny => "deny",
    }
}

/// A Python callable as a decider. It runs on libmachine's decider thread,
/// taking the GIL; never on the network thread, and not at all once Python
/// is exiting. True or "allow" allows, False or "deny" denies; an exception
/// (reported as unraisable) or any other answer gets the default.
fn py_decider(
    callback: Py<PyAny>,
    timeout: f64,
    default: &str,
) -> PyResult<libmachine::net::Decider> {
    let timeout = seconds("decider_timeout", Some(timeout))?.unwrap_or_default();
    let default = verdict_arg(default)?;
    let decide = move |req: &libmachine::net::ConnectionRequest| {
        with_python(|py| {
            let request = ConnectionRequest {
                guest_port: req.guest_port,
                addr: req.addr.to_string(),
                port: req.port,
                names: req.names.clone(),
                rule: req.rule,
            };
            match callback.call1(py, (request,)) {
                Ok(answer) => {
                    let answer = answer.bind(py);
                    if let Ok(b) = answer.extract::<bool>() {
                        Some(if b {
                            libmachine::net::Verdict::Allow
                        } else {
                            libmachine::net::Verdict::Deny
                        })
                    } else {
                        answer.extract::<String>().ok().and_then(|s| verdict_arg(&s).ok())
                    }
                }
                Err(e) => {
                    e.write_unraisable(py, Some(callback.bind(py)));
                    None
                }
            }
        })
        .flatten()
    };
    Ok(libmachine::net::Decider::new(decide).with_timeout(timeout).with_default(default))
}

/// A machine's network, all of it: `NetworkConfig(policy, hosts=..., ...)`
/// where `boot_linux` and `AsyncSandbox.create` take `network=` (which
/// also takes a bare EgressPolicy).
///
/// `hosts` maps names to an IPv4 address or a list of them, which the
/// VMM's DNS answers itself (for names the policy could allow); 127.0.0.1
/// is the host's loopback, answered as the guest's gateway. A name rule
/// matches an entry's addresses whatever they are. `dns_ttl` is the TTL of
/// the DNS answers, and how long a learnt name keeps matching name rules
/// (whole seconds, 1 to 86400). `log_capacity` is how many connection
/// records `Network.take_log()` keeps (0: none). `decider` is a callable
/// deciding the connections "ask" rules hand it (see
/// `Network.set_decider`).
#[pyclass(module = "libmachine", frozen)]
struct NetworkConfig {
    inner: libmachine::net::NetworkConfig,
}

#[pymethods]
impl NetworkConfig {
    #[new]
    #[pyo3(signature = (
        policy = None,
        *,
        hosts = None,
        dns_ttl = 60,
        log_capacity = 1024,
        decider = None,
        decider_timeout = 10.0,
        decider_default = "deny",
    ))]
    fn new(
        policy: Option<PyRef<'_, EgressPolicy>>,
        hosts: Option<&Bound<'_, PyDict>>,
        dns_ttl: u32,
        log_capacity: usize,
        decider: Option<Py<PyAny>>,
        decider_timeout: f64,
        decider_default: &str,
    ) -> PyResult<Self> {
        let mut inner = libmachine::net::NetworkConfig::new(
            policy.map(|p| p.inner.clone()).unwrap_or_default(),
        )
        .with_hosts(hosts_arg(hosts)?)
        .with_dns_ttl(Duration::from_secs(dns_ttl.into()))
        .with_log_capacity(log_capacity);
        if let Some(decider) = decider {
            inner = inner.with_decider(py_decider(decider, decider_timeout, decider_default)?);
        }
        // Checked again when a machine is made; here, so that a bad value
        // fails where it is written.
        inner.validate().map_err(to_py_err)?;
        Ok(Self { inner })
    }

    #[getter]
    fn policy(&self) -> EgressPolicy {
        EgressPolicy { inner: self.inner.policy.clone() }
    }

    #[getter]
    fn hosts(&self) -> HashMap<String, Vec<String>> {
        hosts_dict(&self.inner.hosts)
    }

    #[getter]
    fn dns_ttl(&self) -> u64 {
        self.inner.dns_ttl.as_secs()
    }

    #[getter]
    fn log_capacity(&self) -> usize {
        self.inner.log_capacity
    }

    fn __repr__(&self) -> String {
        format!(
            "NetworkConfig({}, hosts={:?}, dns_ttl={}, log_capacity={}, decider={})",
            self.inner.policy,
            hosts_dict(&self.inner.hosts),
            self.inner.dns_ttl.as_secs(),
            self.inner.log_capacity,
            if self.inner.decider.is_some() { "set" } else { "None" }
        )
    }
}

/// `network=`: an EgressPolicy or a NetworkConfig.
fn network_arg(network: &Bound<'_, PyAny>) -> PyResult<libmachine::net::NetworkConfig> {
    if let Ok(policy) = network.cast::<EgressPolicy>() {
        return Ok(libmachine::net::NetworkConfig::new(policy.borrow().inner.clone()));
    }
    if let Ok(config) = network.cast::<NetworkConfig>() {
        return Ok(config.get().inner.clone());
    }
    Err(PyTypeError::new_err(format!(
        "network must be an EgressPolicy, a NetworkConfig or None, got {network}"
    )))
}

/// A connection the decider is asked about: the guest's port, the host
/// address and port the VMM would connect to (127.0.0.1 for the guest's
/// gateway), the names the address goes by for the guest (host entries,
/// then names it resolved), and the index of the ask rule that matched.
#[pyclass(module = "libmachine", frozen, get_all)]
struct ConnectionRequest {
    guest_port: u16,
    addr: String,
    port: u16,
    names: Vec<String>,
    rule: usize,
}

#[pymethods]
impl ConnectionRequest {
    fn __repr__(&self) -> String {
        format!(
            "ConnectionRequest(addr={:?}, port={}, names={:?}, guest_port={}, rule={})",
            self.addr, self.port, self.names, self.guest_port, self.rule
        )
    }
}

/// One guest connection, allowed or denied, logged when it ended.
/// `verdict` is "allow" or "deny"; `decided_by` says what decided it ("rule
/// 0 (allow 127.0.0.1:8080)", "no rule", "callback", "callback timeout",
/// "callback failed", "no callback", "connection limit"), and `rule` is the
/// deciding rule's index if a rule did. `connected` is whether the host
/// connection was made. `started` is a Unix time, `duration` seconds.
#[pyclass(module = "libmachine", frozen, get_all)]
struct ConnectionRecord {
    guest_port: u16,
    addr: String,
    port: u16,
    names: Vec<String>,
    verdict: &'static str,
    decided_by: String,
    rule: Option<usize>,
    connected: bool,
    bytes_to_host: u64,
    bytes_to_guest: u64,
    started: f64,
    duration: f64,
}

impl From<libmachine::net::ConnectionRecord> for ConnectionRecord {
    fn from(r: libmachine::net::ConnectionRecord) -> Self {
        let rule = match &r.decided_by {
            libmachine::net::DecidedBy::Rule { index, .. } => Some(*index),
            _ => None,
        };
        Self {
            guest_port: r.guest_port,
            addr: r.addr.to_string(),
            port: r.port,
            names: r.names,
            verdict: verdict_str(r.verdict),
            decided_by: r.decided_by.to_string(),
            rule,
            connected: r.connected,
            bytes_to_host: r.bytes_to_host,
            bytes_to_guest: r.bytes_to_guest,
            started: r
                .started
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64()),
            duration: r.duration.as_secs_f64(),
        }
    }
}

#[pymethods]
impl ConnectionRecord {
    fn __repr__(&self) -> String {
        format!(
            "ConnectionRecord({} {}:{} names={:?} by {}, connected={}, {} bytes to host, {} to guest, {:.3} s)",
            self.verdict,
            self.addr,
            self.port,
            self.names,
            self.decided_by,
            if self.connected { "True" } else { "False" },
            self.bytes_to_host,
            self.bytes_to_guest,
            self.duration
        )
    }
}

/// A machine's network (`Machine.network()`): its policy, host entries and
/// decider can change from any thread, including while another runs the
/// machine; its log and counters say what the guest connected to.
#[pyclass(module = "libmachine", frozen)]
struct Network(libmachine::net::Network);

#[pymethods]
impl Network {
    /// The policy in force (a copy).
    #[getter]
    fn policy(&self) -> EgressPolicy {
        EgressPolicy { inner: self.0.policy() }
    }

    /// Replace the policy for connections and DNS queries from now on;
    /// open connections stay open.
    fn set_policy(&self, policy: &EgressPolicy) {
        self.0.set_policy(policy.inner.clone());
    }

    /// The host entries in force: {name: [addresses]}.
    #[getter]
    fn hosts(&self) -> HashMap<String, Vec<String>> {
        hosts_dict(&self.0.hosts())
    }

    /// Replace the host entries ({name: address or [addresses]}) for DNS
    /// queries and connections from now on.
    fn set_hosts(&self, hosts: &Bound<'_, PyDict>) -> PyResult<()> {
        self.0.set_hosts(hosts_arg(Some(hosts))?);
        Ok(())
    }

    /// Set the callable deciding the connections "ask" rules hand it, or
    /// remove it with None (they are then denied). It is called with a
    /// ConnectionRequest on libmachine's decider thread, holding the GIL,
    /// one connection at a time, and answers True/"allow" or
    /// False/"deny". The guest's connection waits for it at most `timeout`
    /// seconds; then, or if it raises or answers anything else, `default`
    /// applies. Since it runs on a thread of its own, it may block, e.g. on
    /// `asyncio.run_coroutine_threadsafe(coro, loop).result()`.
    #[pyo3(signature = (callback, *, timeout = 10.0, default = "deny"))]
    fn set_decider(
        &self,
        callback: Option<Py<PyAny>>,
        timeout: f64,
        default: &str,
    ) -> PyResult<()> {
        let decider = callback.map(|c| py_decider(c, timeout, default)).transpose()?;
        self.0.set_decider(decider);
        Ok(())
    }

    /// Take the connection records logged since the last call, oldest
    /// first (the log keeps the newest `log_capacity`).
    fn take_log(&self) -> Vec<ConnectionRecord> {
        self.0.take_log().into_iter().map(Into::into).collect()
    }

    #[getter]
    fn log_capacity(&self) -> usize {
        self.0.log_capacity()
    }

    /// The TTL of the DNS server's answers, in seconds.
    #[getter]
    fn dns_ttl(&self) -> u64 {
        self.0.dns_ttl().as_secs()
    }

    /// The counters since the machine started: connections_allowed,
    /// connections_denied, connections_failed, connections_open,
    /// bytes_to_host, bytes_to_guest, dns_queries, dns_refused, decisions,
    /// decisions_timed_out, decisions_failed, log_dropped.
    fn stats(&self) -> HashMap<&'static str, u64> {
        let s = self.0.stats();
        HashMap::from([
            ("connections_allowed", s.connections_allowed),
            ("connections_denied", s.connections_denied),
            ("connections_failed", s.connections_failed),
            ("connections_open", s.connections_open),
            ("bytes_to_host", s.bytes_to_host),
            ("bytes_to_guest", s.bytes_to_guest),
            ("dns_queries", s.dns_queries),
            ("dns_refused", s.dns_refused),
            ("decisions", s.decisions),
            ("decisions_timed_out", s.decisions_timed_out),
            ("decisions_failed", s.decisions_failed),
            ("log_dropped", s.log_dropped),
        ])
    }
}

/// A virtual machine. `Machine()` makes a bare machine: one vCPU starting
/// in 16-bit real mode, RAM, and a UART; `Machine.boot_linux()` makes a PC
/// with `vcpus` vCPUs, each on a host thread of its own.
///
/// `run()` releases the GIL, so several machines can run in parallel from
/// Python threads. A single machine must not be used from two threads at once.
#[pyclass(module = "libmachine")]
struct Machine {
    inner: libmachine::Machine,
}

#[pymethods]
impl Machine {
    #[new]
    #[pyo3(signature = (
        *,
        mem_size = libmachine::MachineConfig::default().mem_size,
        cpu_model = "host",
        vcpus = 1,
    ))]
    fn new(py: Python<'_>, mem_size: usize, cpu_model: &str, vcpus: i64) -> PyResult<Self> {
        let cpu_model: libmachine::CpuModel = cpu_model.parse().map_err(PyValueError::new_err)?;
        let config = libmachine::MachineConfig {
            mem_size,
            cpu_model,
            vcpus: vcpu_count(vcpus)?,
            ..Default::default()
        };
        let inner = py.detach(|| libmachine::Machine::new(config)).map_err(to_py_err)?;
        Ok(Self { inner })
    }

    /// Create a PC machine and load a Linux kernel into it, ready to `run()`.
    ///
    /// `kernel` is an ELF vmlinux with a PVH entry point or a bzImage;
    /// `cmdline` is appended to "console=ttyS0 panic=-1". `vcpus` is how
    /// many vCPUs the guest gets, from 1 to 254. `network`, an
    /// EgressPolicy or a NetworkConfig, gives the guest a network card
    /// whose connections the policy filters (None: no network). `extent_stores`, ExtentStores,
    /// are cladefs's extent store devices, their cells opened on `extent_daemon`, an
    /// ExtentDaemon. Run it until the guest powers off ("poweroff") or reboots ("reset").
    #[staticmethod]
    #[pyo3(signature = (
        kernel,
        initrd = None,
        cmdline = "",
        *,
        mem_size = 256 << 20,
        cpu_model = "host",
        vcpus = 1,
        network = None,
        extent_daemon = None,
        extent_stores = Vec::new(),
    ))]
    #[allow(clippy::too_many_arguments, reason = "Python keyword arguments")]
    fn boot_linux(
        py: Python<'_>,
        kernel: std::path::PathBuf,
        initrd: Option<std::path::PathBuf>,
        cmdline: &str,
        mem_size: usize,
        cpu_model: &str,
        vcpus: i64,
        network: Option<&Bound<'_, PyAny>>,
        extent_daemon: Option<PyRef<'_, ExtentDaemon>>,
        extent_stores: Vec<PyRef<'_, ExtentStore>>,
    ) -> PyResult<Self> {
        let cpu_model: libmachine::CpuModel = cpu_model.parse().map_err(PyValueError::new_err)?;
        let config = libmachine::MachineConfig {
            mem_size,
            cpu_model,
            vcpus: vcpu_count(vcpus)?,
            network: network.map(network_arg).transpose()?,
            extent_daemon: extent_daemon.map(|daemon| daemon.0.clone()),
            extent_stores: extent_stores.iter().map(|store| store.0.clone()).collect(),
            ..Default::default()
        };
        let boot = libmachine::LinuxBoot { kernel, initrd, cmdline: cmdline.to_owned() };
        let inner =
            py.detach(|| libmachine::Machine::boot_linux(config, &boot)).map_err(to_py_err)?;
        Ok(Self { inner })
    }

    /// The CPU model the guest sees: "host" or "baseline".
    #[getter]
    fn cpu_model(&self) -> String {
        self.inner.cpu_model().to_string()
    }

    /// Stable hex identifier of the CPU the guest sees (SHA-256 of its CPUID
    /// table and feature MSRs).
    #[getter]
    fn cpu_fingerprint(&self) -> &str {
        self.inner.cpu_fingerprint()
    }

    /// The (eax, ebx, ecx, edx) the guest's CPUID returns for `function` and
    /// subleaf `index`, as configured; None if the leaf is not in the table.
    #[pyo3(signature = (function, index = 0))]
    fn cpuid(&self, function: u32, index: u32) -> Option<(u32, u32, u32, u32)> {
        self.inner.cpuid(function, index).map(|e| (e.eax, e.ebx, e.ecx, e.edx))
    }

    /// How many vCPUs the machine has.
    #[getter]
    fn vcpus(&self) -> u32 {
        self.inner.vcpus()
    }

    /// Guest RAM size in bytes.
    #[getter]
    fn mem_size(&self) -> usize {
        self.inner.mem_size()
    }

    /// Copy `data` into guest RAM at guest physical address `addr`.
    fn load(&mut self, addr: u64, data: &[u8]) -> PyResult<()> {
        self.inner.load(addr, data).map_err(to_py_err)
    }

    /// Read `length` bytes of guest RAM starting at `addr`.
    fn read_memory<'py>(
        &self,
        py: Python<'py>,
        addr: u64,
        length: usize,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let data = self.inner.read_memory(addr, length).map_err(to_py_err)?;
        Ok(PyBytes::new(py, &data))
    }

    /// Point vCPU 0 at `addr` (real mode, must be below 0x10000).
    fn set_entry(&mut self, addr: u64) -> PyResult<()> {
        self.inner.set_entry(addr).map_err(to_py_err)
    }

    /// Run every vCPU until the guest stops ("poweroff", "reset", ...), a
    /// StopHandle stops it ("stopped"), or `timeout` seconds pass
    /// ("timed_out"). Whichever vCPU ends the run, all have stopped when it
    /// returns. The machine keeps its state either way and can be run again.
    ///
    /// The GIL is released while the guest runs. Every 50 ms the waiting
    /// thread checks for Python signals without disturbing the vCPUs; Ctrl-C
    /// (any pending signal handler that raises) stops the guest within
    /// about 50 ms and raises here, and the machine can be run again.
    #[pyo3(signature = (timeout = None))]
    fn run(&mut self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Exit> {
        let timeout = match timeout {
            None => None,
            Some(t) if t.is_finite() && t >= 0.0 => Some(Duration::from_secs_f64(t)),
            Some(t) => {
                return Err(PyValueError::new_err(format!(
                    "timeout must be a finite number of seconds >= 0, got {t}"
                )));
            }
        };
        let mut run = self.inner.start_run(timeout);
        loop {
            run = match py.detach(move || run.wait_for(SIGNAL_CHECK_INTERVAL)) {
                libmachine::Waited::Ended(outcome) => {
                    return outcome.map(Exit::from).map_err(to_py_err);
                }
                libmachine::Waited::Running(run) => run,
            };
            if let Err(e) = py.check_signals() {
                // Every vCPU stops; the machine keeps its state.
                let _ = py.detach(move || run.stop());
                return Err(e);
            }
        }
    }

    /// The guest agent on the machine's vsock (PC machines only). Get it
    /// before calling `run()` in another thread, then use it from this one.
    fn agent(&self) -> PyResult<Agent> {
        self.inner
            .agent()
            .map(Agent)
            .ok_or_else(|| PyValueError::new_err("a bare machine has no vsock"))
    }

    /// The host's end of the machine's vsock (PC machines only); like
    /// `agent()`, get it before `run()`.
    fn vsock(&self) -> PyResult<Vsock> {
        self.inner
            .vsock()
            .map(Vsock)
            .ok_or_else(|| PyValueError::new_err("a bare machine has no vsock"))
    }

    /// The machine's network, if `boot_linux(network=...)` gave it one;
    /// like `agent()`, get it before `run()`.
    fn network(&self) -> PyResult<Network> {
        self.inner
            .network()
            .map(Network)
            .ok_or_else(|| PyValueError::new_err("the machine has no network"))
    }

    /// A handle that stops `run()` from another thread.
    fn stop_handle(&self) -> StopHandle {
        StopHandle(self.inner.stop_handle())
    }

    /// How many vCPU exits of each kind the machine has taken since it was
    /// created, over all its vCPUs: a dict from kind ("serial", "rtc", "acpi",
    /// "unclaimed_io", "pci_config", "pci_bar", "unclaimed_mmio", "halt",
    /// "interrupted", "startup", "hypercall", "wait", "system_register",
    /// "access_retried", "other") to count.
    fn exit_counts<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let counts = PyDict::new(py);
        for (kind, n) in self.inner.exit_counts().kinds() {
            counts.set_item(kind, n)?;
        }
        Ok(counts)
    }

    /// vCPU 0's current registers.
    fn registers(&self) -> PyResult<Registers> {
        self.inner.registers().map(Registers::from).map_err(to_py_err)
    }

    /// Bytes the guest has written to the serial port so far.
    fn serial_output<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.serial_output())
    }

    /// Return the captured serial output and clear the buffer.
    fn take_serial_output<'py>(&mut self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.take_serial_output())
    }

    fn __repr__(&self) -> String {
        format!("Machine(mem_size={}, vcpus={})", self.inner.mem_size(), self.inner.vcpus())
    }
}

/// The gate every entry into Python from a thread of libmachine's own goes
/// through (a tokio worker completing an awaitable, the decider thread).
/// Entering the interpreter once it has begun to finalize crashes the
/// process, or panics in pyo3; Python exits while such threads still run.
/// So the `atexit` hook closes the gate, then waits, without the GIL, for
/// whoever is inside to leave: once it returns, none of our threads enters
/// Python again. Pending awaitables then never complete; nothing can await
/// them any more.
struct PythonGate {
    state: std::sync::Mutex<GateState>,
    left: std::sync::Condvar,
}

struct GateState {
    closed: bool,
    inside: usize,
}

static GATE: PythonGate = PythonGate {
    state: std::sync::Mutex::new(GateState { closed: false, inside: 0 }),
    left: std::sync::Condvar::new(),
};

/// How long the `atexit` hook waits for threads inside Python to leave: a
/// decider is user code and may block. One that outstays it is already
/// inside the interpreter, which treats it as it treats any thread at exit.
const GATE_CLOSE_WAIT: Duration = Duration::from_secs(1);

impl PythonGate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Runs `f` with the GIL from a thread of libmachine's own, unless Python
/// is exiting: then `f` never runs (None). The one way such a thread enters
/// Python.
fn with_python<R>(f: impl FnOnce(Python<'_>) -> R) -> Option<R> {
    struct Pass;
    impl Drop for Pass {
        fn drop(&mut self) {
            let mut state = GATE.state();
            state.inside -= 1;
            if state.inside == 0 {
                GATE.left.notify_all();
            }
        }
    }
    let _pass = {
        let mut state = GATE.state();
        if state.closed {
            return None;
        }
        state.inside += 1;
        Pass
    };
    Some(Python::attach(f))
}

/// What `atexit` calls (and tests, in a process of their own): closes the
/// gate into Python.
#[pyfunction]
fn _python_exiting(py: Python<'_>) {
    py.detach(|| {
        let mut state = GATE.state();
        state.closed = true;
        let _ = GATE.left.wait_timeout_while(state, GATE_CLOSE_WAIT, |s| s.inside > 0);
    });
}

/// For tests: an awaitable that completes after `delay` seconds with `delay`,
/// or fails with a ValueError if `fail`. It needs no guest, so a test can
/// leave thousands of calls completing, failing and cancelled as Python
/// exits.
#[pyfunction]
#[pyo3(signature = (delay, fail = false))]
fn _after(py: Python<'_>, delay: f64, fail: bool) -> PyResult<Bound<'_, PyAny>> {
    let wait = seconds("delay", Some(delay))?.unwrap_or_default();
    awaitable(py, async move {
        tokio::time::sleep(wait).await;
        if fail { Err(PyValueError::new_err("failed as asked")) } else { Ok(delay) }
    })
}

/// The bindings' Python helpers.
static HELPERS: pyo3::sync::PyOnceLock<Py<PyModule>> = pyo3::sync::PyOnceLock::new();

fn helpers(py: Python<'_>) -> PyResult<&Bound<'_, PyModule>> {
    HELPERS
        .get_or_try_init(py, || -> PyResult<Py<PyModule>> {
            Ok(PyModule::from_code(
                py,
                c"import asyncio
get_running_loop = asyncio.get_running_loop

async def await_(f):
    return await f

async def read(ready, take):
    await ready
    return take()

def complete(f, ok, value):
    if not f.done():
        (f.set_result if ok else f.set_exception)(value)
",
                c"<libmachine>",
                c"_libmachine_helpers",
            )?
            .unbind())
        })
        .map(|m| m.bind(py))
}

/// The tokio runtime every awaitable runs on.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("libmachine-async")
            .enable_time()
            .build()
            .expect("start libmachine's tokio runtime")
    })
}

/// The second step of `AsyncProcess.read()`: takes the event the first step
/// found (another read may have taken it first: then wait again).
#[pyclass(module = "libmachine", frozen)]
struct TakeEvent(std::sync::Arc<libmachine::sandbox::SandboxProcess>);

#[pymethods]
impl TakeEvent {
    fn __call__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match self.0.take_event() {
            Some(event) => EventOut(event.map_err(to_py_err)?).into_pyobject(py),
            None => {
                // Taken by a concurrent read: this read waits for the next.
                let p = self.0.clone();
                awaitable(py, async move { p.next_event().await.map(EventOut).map_err(to_py_err) })
            }
        }
    }
}

/// Every awaitable the bindings return, made here: `fut` runs on tokio at
/// once, and the result is a coroutine awaiting an asyncio Future of the
/// running loop (a coroutine, so that `asyncio.create_task` and task groups
/// take it, as they take no Future). Cancelling it aborts the task, which
/// drops `fut`.
///
/// No tokio thread enters Python but to complete the Future, through the
/// gate (`with_python`): cancellation is a done callback on the loop's own
/// thread, aborting the task, with no call back into Python. (board 13191: the
/// pyo3-async-runtimes bridge this replaces entered Python from tokio
/// threads to check for cancellation, crashing the process at exit.)
fn awaitable<'py, F, T>(py: Python<'py>, fut: F) -> PyResult<Bound<'py, PyAny>>
where
    F: std::future::Future<Output = PyResult<T>> + Send + 'static,
    T: for<'a> IntoPyObject<'a> + Send + 'static,
{
    let helpers = helpers(py)?;
    let event_loop = helpers.getattr("get_running_loop")?.call0()?;
    let future = event_loop.call_method0("create_future")?;
    let call = runtime().spawn(fut);
    future.call_method1("add_done_callback", (AbortCall(call.abort_handle()),))?;
    let (event_loop, completed) = (event_loop.unbind(), future.clone().unbind());
    runtime().spawn(async move {
        let result = match call.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => {
                Err(pyo3::panic::PanicException::new_err(panic_message(e.into_panic())))
            }
            Err(_) => return, // cancelled from Python
        };
        // Off the async workers: taking the GIL may wait.
        drop(tokio::task::spawn_blocking(move || complete(&event_loop, &completed, result)));
    });
    helpers.getattr("await_")?.call1((future,))
}

/// Completes `future` with `result`, from a tokio thread, on its loop's
/// thread. Unless Python is exiting, or the loop closed (asyncio.run
/// returned): then nothing can await it any more.
fn complete<T>(event_loop: &Py<PyAny>, future: &Py<PyAny>, result: PyResult<T>)
where
    T: for<'a> IntoPyObject<'a>,
{
    with_python(|py| -> PyResult<()> {
        let (ok, value) = match result.and_then(|v| pyo3::IntoPyObjectExt::into_py_any(v, py)) {
            Ok(value) => (true, value),
            Err(e) => (false, e.into_value(py).into_any()),
        };
        let complete = helpers(py)?.getattr("complete")?;
        event_loop.call_method1(py, "call_soon_threadsafe", (complete, future, ok, value))?;
        Ok(())
    });
}

/// A done callback of an awaitable's Future: when Python cancels it, the
/// tokio task stops (a no-op once the task has finished).
#[pyclass(frozen)]
struct AbortCall(tokio::task::AbortHandle);

#[pymethods]
impl AbortCall {
    fn __call__(&self, _future: &Bound<'_, PyAny>) {
        self.0.abort();
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(s) => *s,
        Err(panic) => panic.downcast_ref::<&str>().map_or("unknown panic", |s| s).to_owned(),
    }
}

/// Bytes for an awaitable's result (a Vec<u8> would become a list).
struct Bytes(Vec<u8>);

impl<'py> IntoPyObject<'py> for Bytes {
    type Target = PyBytes;
    type Output = Bound<'py, PyBytes>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(PyBytes::new(py, &self.0))
    }
}

/// A process's next event for an awaitable's result: what `Process.read()`
/// returns.
struct EventOut(Option<libmachine::agent::Event>);

impl<'py> IntoPyObject<'py> for EventOut {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        use libmachine::agent::Event;
        Ok(match self.0 {
            None => py.None().into_bound(py),
            Some(Event::Stdout(d)) => {
                ("stdout", PyBytes::new(py, &d)).into_pyobject(py)?.into_any()
            }
            Some(Event::Stderr(d)) => {
                ("stderr", PyBytes::new(py, &d)).into_pyobject(py)?.into_any()
            }
            Some(Event::Exit(status)) => {
                ("exit", (status.code, status.signal)).into_pyobject(py)?.into_any()
            }
        })
    }
}

impl From<libmachine::sandbox::Ended> for Exit {
    fn from(ended: libmachine::sandbox::Ended) -> Self {
        use libmachine::sandbox::Ended;
        match ended {
            Ended::Closed => Exit::simple("closed"),
            Ended::Guest(reason) => Exit::from(reason),
            Ended::Failed(detail) => Exit { detail: Some(detail), ..Exit::simple("failed") },
            other => Exit { detail: Some(format!("{other:?}")), ..Exit::simple("other") },
        }
    }
}

/// A Linux guest running libmachine's agent, used from asyncio: create it
/// with `await AsyncSandbox.create(...)`, then await its methods from any
/// number of tasks at once. Many can run in one process; their waits run on
/// libmachine's own threads, never blocking the event loop.
///
/// `async with` closes it at the end of the block; so does garbage
/// collection, without waiting. Closing it (or the guest stopping by
/// itself) fails every call waiting on the guest with ConnectionResetError.
/// Cancelling a task awaiting `exec()` kills its command. Errors are as for
/// `Agent`.
///
/// Its methods return coroutines whose work starts at the call, not at the
/// first await (a coroutine never awaited still runs its call). Once Python
/// is exiting (its atexit hooks have run), pending calls never complete.
#[pyclass(module = "libmachine", frozen)]
struct AsyncSandbox(std::sync::Arc<libmachine::sandbox::Sandbox>);

#[pymethods]
impl AsyncSandbox {
    /// Boot `kernel` (with `initrd` and `cmdline`, as for
    /// `Machine.boot_linux()`) and wait until the guest's agent answers,
    /// at most `ready_timeout` seconds. A guest that stops while booting
    /// fails at once (ConnectionResetError, quoting its console).
    /// `timeout` bounds each wait for the guest in later calls (None: as
    /// long as the guest runs). Awaitable.
    #[staticmethod]
    #[pyo3(signature = (
        kernel,
        initrd = None,
        cmdline = "",
        *,
        mem_size = 256 << 20,
        cpu_model = "host",
        vcpus = 1,
        network = None,
        ready_timeout = 30.0,
        timeout = None,
    ))]
    #[allow(clippy::too_many_arguments, reason = "Python keyword arguments")]
    fn create<'py>(
        py: Python<'py>,
        kernel: std::path::PathBuf,
        initrd: Option<std::path::PathBuf>,
        cmdline: &str,
        mem_size: usize,
        cpu_model: &str,
        vcpus: i64,
        network: Option<&Bound<'_, PyAny>>,
        ready_timeout: f64,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cpu_model: libmachine::CpuModel = cpu_model.parse().map_err(PyValueError::new_err)?;
        let config = libmachine::MachineConfig {
            mem_size,
            cpu_model,
            vcpus: vcpu_count(vcpus)?,
            network: network.map(network_arg).transpose()?,
            ..Default::default()
        };
        let boot = libmachine::LinuxBoot { kernel, initrd, cmdline: cmdline.to_owned() };
        let builder = libmachine::sandbox::Sandbox::builder(boot)
            .config(config)
            .ready_timeout(seconds("ready_timeout", Some(ready_timeout))?.unwrap())
            .agent_timeout(seconds("timeout", timeout)?);
        awaitable(py, async move {
            let sandbox = builder.create().await.map_err(to_py_err)?;
            Ok(AsyncSandbox(std::sync::Arc::new(sandbox)))
        })
    }

    /// Restore a sandbox from `snapshot` (from `AsyncSandbox.snapshot()`,
    /// or `Snapshot.take()` of a machine running the agent) and wait until
    /// its agent answers, having reseeded the guest's RNG: milliseconds,
    /// where `create()` boots. Its machine is the snapshot's; each restored
    /// sandbox is its own guest (RAM copy-on-write from the file, RNG
    /// reseeded, clock moved on). `ready_timeout` and `timeout` are as for
    /// `create()`. Awaitable.
    #[staticmethod]
    #[pyo3(signature = (snapshot, *, ready_timeout = 30.0, timeout = None))]
    fn restore<'py>(
        py: Python<'py>,
        snapshot: PyRef<'_, Snapshot>,
        ready_timeout: f64,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let builder = libmachine::sandbox::Sandbox::from_snapshot(snapshot.inner.clone())
            .ready_timeout(seconds("ready_timeout", Some(ready_timeout))?.unwrap())
            .agent_timeout(seconds("timeout", timeout)?);
        awaitable(py, async move {
            let sandbox = builder.create().await.map_err(to_py_err)?;
            Ok(AsyncSandbox(std::sync::Arc::new(sandbox)))
        })
    }

    /// Snapshot the sandbox's machine to a file at `path` (replacing any
    /// there) while it keeps running, for `AsyncSandbox.restore()`: the way
    /// to make a template. The guest is paused while the file is written.
    /// Snapshot an idle guest: a command running now runs on, unreachable,
    /// in every clone. A sandbox with a network is refused (MachineError).
    /// Awaitable: a Snapshot.
    fn snapshot<'py>(
        &self,
        py: Python<'py>,
        path: std::path::PathBuf,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move {
            let inner = sandbox.snapshot(&path).await.map_err(to_py_err)?;
            Ok(Snapshot { inner: std::sync::Arc::new(inner) })
        })
    }

    /// Suspend the guest while it waits for something from outside (a slow
    /// server's answer): its whole machine (vCPUs, devices, every
    /// connection's TCP state, RAM) goes to a file in `dir` (on a disk, not
    /// tmpfs) and is freed, and its connections stay open, their host ends
    /// held. It wakes by itself, where it was, when data or a close comes
    /// on any of its connections, at any call on this sandbox, or at
    /// `resume()`. Its clocks run on meanwhile. Raises GuestBusyError if
    /// the guest is not idle within 2 s. Awaitable: None, once suspended.
    fn suspend<'py>(
        &self,
        py: Python<'py>,
        dir: std::path::PathBuf,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move { sandbox.suspend(&dir).await.map_err(to_py_err) })
    }

    /// Wake a suspended guest. Awaitable: None, once it runs again.
    fn resume<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move { sandbox.resume().await.map_err(to_py_err) })
    }

    /// Whether the guest is suspended now.
    #[getter]
    fn suspended(&self) -> bool {
        self.0.suspended()
    }

    /// Run `argv` to its end, as `Agent.exec()`. Awaitable: an ExecResult.
    #[pyo3(signature = (argv, *, env = None, cwd = None, stdin = None))]
    fn exec<'py>(
        &self,
        py: Python<'py>,
        argv: Vec<OsString>,
        env: Option<HashMap<OsString, OsString>>,
        cwd: Option<OsString>,
        stdin: Option<&[u8]>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut cmd = command(argv, env, cwd)?;
        if let Some(data) = stdin {
            cmd.stdin(data);
        }
        let sandbox = self.0.clone();
        awaitable(
            py,
            async move { sandbox.exec(&cmd).await.map(ExecResult::from).map_err(to_py_err) },
        )
    }

    /// Start `argv`, its stdin a pipe fed through the AsyncProcess.
    /// Awaitable: an AsyncProcess.
    #[pyo3(signature = (argv, *, env = None, cwd = None))]
    fn spawn<'py>(
        &self,
        py: Python<'py>,
        argv: Vec<OsString>,
        env: Option<HashMap<OsString, OsString>>,
        cwd: Option<OsString>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cmd = command(argv, env, cwd)?;
        let sandbox = self.0.clone();
        awaitable(py, async move {
            let process = sandbox.spawn(&cmd).await.map_err(to_py_err)?;
            Ok(AsyncProcess(std::sync::Arc::new(process)))
        })
    }

    /// The contents of the guest's file `path`. Awaitable: bytes.
    fn read_file<'py>(&self, py: Python<'py>, path: OsString) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move { sandbox.read_file(&path).await.map(Bytes).map_err(to_py_err) })
    }

    /// Make the guest's file `path` hold `data`, with permissions `mode`
    /// (whole, or not at all). Awaitable.
    #[pyo3(signature = (path, data, mode = 0o644))]
    fn write_file<'py>(
        &self,
        py: Python<'py>,
        path: OsString,
        data: &[u8],
        mode: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        let data = data.to_vec();
        awaitable(py, async move { sandbox.write_file(&path, data, mode).await.map_err(to_py_err) })
    }

    /// Copy the host's file `src` to the guest's `dst`, with its
    /// permissions. Awaitable.
    fn copy_to_guest<'py>(
        &self,
        py: Python<'py>,
        src: std::path::PathBuf,
        dst: OsString,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move { sandbox.copy_to_guest(&src, &dst).await.map_err(to_py_err) })
    }

    /// Copy the guest's file `src` to the host's `dst`. Awaitable.
    fn copy_from_guest<'py>(
        &self,
        py: Python<'py>,
        src: OsString,
        dst: std::path::PathBuf,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move { sandbox.copy_from_guest(&src, &dst).await.map_err(to_py_err) })
    }

    /// Wait until the guest stops by itself or the sandbox is closed.
    /// Awaitable: an Exit ("closed", "poweroff", "reset", ...).
    fn wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move { Ok(Exit::from(sandbox.wait().await)) })
    }

    /// Stop the guest if it runs and wait until its machine is freed.
    /// Awaitable: an Exit, "closed" unless the guest had stopped by itself.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move { Ok(Exit::from(sandbox.close().await)) })
    }

    /// How the sandbox ended (an Exit), or None while it runs.
    #[getter]
    fn ended(&self) -> Option<Exit> {
        self.0.ended().map(Exit::from)
    }

    /// The end of what the guest wrote to its console (at most 64 KiB).
    fn console_output<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.0.console_output())
    }

    /// The blocking agent of the guest, as `Machine.agent()`.
    fn agent(&self) -> Agent {
        Agent(self.0.agent().clone())
    }

    /// The guest's network, if `create(network=...)` gave it one.
    fn network(&self) -> PyResult<Network> {
        self.0
            .network()
            .cloned()
            .map(Network)
            .ok_or_else(|| PyValueError::new_err("the sandbox has no network"))
    }

    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        awaitable(py, async move { Ok(slf) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Py<PyAny>,
        _exc: Py<PyAny>,
        _traceback: Py<PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sandbox = self.0.clone();
        awaitable(py, async move {
            sandbox.close().await;
            Ok(false)
        })
    }

    fn __repr__(&self) -> String {
        match self.0.ended() {
            None => "AsyncSandbox(running)".into(),
            Some(ended) => format!("AsyncSandbox(ended={ended:?})"),
        }
    }
}

/// A command running in an AsyncSandbox's guest; see `AsyncSandbox.spawn()`.
/// Its methods are awaitable and may be awaited from several tasks at once
/// (one feeding stdin, one reading). Cancelling a `read()` loses no output.
/// Once it is garbage, the command is killed if it still runs.
#[pyclass(module = "libmachine", frozen)]
struct AsyncProcess(std::sync::Arc<libmachine::sandbox::SandboxProcess>);

#[pymethods]
impl AsyncProcess {
    /// Its process ID in the guest (also its process group's).
    #[getter]
    fn pid(&self) -> u32 {
        self.0.pid()
    }

    /// Feed `data` to its stdin. Awaitable.
    fn write_stdin<'py>(&self, py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, PyAny>> {
        let p = self.0.clone();
        let data = data.to_vec();
        awaitable(py, async move { p.write_stdin(data).await.map_err(to_py_err) })
    }

    /// Close its stdin. Awaitable.
    fn close_stdin<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let p = self.0.clone();
        awaitable(py, async move { p.close_stdin().await.map_err(to_py_err) })
    }

    /// Send `signal` to its process group. Awaitable.
    fn signal<'py>(&self, py: Python<'py>, signal: i32) -> PyResult<Bound<'py, PyAny>> {
        let p = self.0.clone();
        awaitable(py, async move { p.signal(signal).await.map_err(to_py_err) })
    }

    /// Kill its process group. Awaitable.
    fn kill<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let p = self.0.clone();
        awaitable(py, async move { p.kill().await.map_err(to_py_err) })
    }

    /// What it does next, as `Process.read()`. Awaitable.
    fn read<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Two steps: wait until an event is ready, then take it with no
        // await in between. A cancelled read can then lose nothing: the
        // task waiting for it may still receive the event after Python
        // cancelled, and leaves it for the next read.
        let p = self.0.clone();
        let ready = awaitable(py, async move {
            p.event_ready().await;
            Ok(())
        })?;
        let take = Py::new(py, TakeEvent(self.0.clone()))?;
        helpers(py)?.getattr("read")?.call1((ready, take))
    }

    /// Wait for it to end: its exit and the output not read yet.
    /// Awaitable: an ExecResult.
    fn wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let p = self.0.clone();
        awaitable(py, async move { p.wait().await.map(ExecResult::from).map_err(to_py_err) })
    }

    fn __repr__(&self) -> String {
        format!("AsyncProcess(pid={})", self.0.pid())
    }
}

/// A snapshot file: a stopped PC machine's whole state, from which any
/// number of machines can be restored.
///
/// `Snapshot.take(machine, path)` writes one (between runs: not while
/// another thread is in `machine.run()`); `Snapshot(path)` opens one and
/// checks it; `restore()` makes a new Machine from it, ready to `run()`.
/// Each restored machine's RAM is a copy-on-write mapping of the file, so
/// what it writes stays its own; its clock is moved on to the present, its
/// RNG reseeded and its vsock connections from before reset.
#[pyclass(module = "libmachine", frozen)]
struct Snapshot {
    /// Shared with the sandboxes being restored from it.
    inner: std::sync::Arc<libmachine::Snapshot>,
}

#[pymethods]
impl Snapshot {
    /// Open the snapshot at `path` and check it. Raises MachineError for a
    /// file of another format version or a damaged one (snapshots are never
    /// converted), OSError if it cannot be read.
    #[new]
    fn open(py: Python<'_>, path: std::path::PathBuf) -> PyResult<Self> {
        let inner = py.detach(|| libmachine::Snapshot::open(&path)).map_err(to_py_err)?;
        Ok(Self { inner: std::sync::Arc::new(inner) })
    }

    /// Snapshot `machine` (a PC machine without a network, between runs)
    /// to a file at `path`, replacing any there, and open it.
    #[staticmethod]
    fn take(
        py: Python<'_>,
        mut machine: PyRefMut<'_, Machine>,
        path: std::path::PathBuf,
    ) -> PyResult<Self> {
        let inner = &mut machine.inner;
        let snapshot = py
            .detach(|| {
                inner.snapshot(&path)?;
                libmachine::Snapshot::open(&path)
            })
            .map_err(to_py_err)?;
        Ok(Self { inner: std::sync::Arc::new(snapshot) })
    }

    /// A new machine from the snapshot. By default it is the snapshot's
    /// machine; `mem_size`, `cpu_model` and `vcpus` may be given, but a
    /// machine unlike the snapshot's (another vCPU count, RAM size, or a
    /// CPU model whose fingerprint differs on this host) is refused with
    /// MachineError.
    #[pyo3(signature = (*, mem_size = None, cpu_model = None, vcpus = None))]
    fn restore(
        &self,
        py: Python<'_>,
        mem_size: Option<usize>,
        cpu_model: Option<&str>,
        vcpus: Option<i64>,
    ) -> PyResult<Machine> {
        let mut config = self.inner.config();
        if let Some(mem_size) = mem_size {
            config.mem_size = mem_size;
        }
        if let Some(model) = cpu_model {
            config.cpu_model = model.parse().map_err(PyValueError::new_err)?;
        }
        if let Some(vcpus) = vcpus {
            config.vcpus = vcpu_count(vcpus)?;
        }
        let inner =
            py.detach(|| libmachine::Machine::restore(&self.inner, config)).map_err(to_py_err)?;
        Ok(Machine { inner })
    }

    /// The file's path.
    #[getter]
    fn path(&self) -> std::path::PathBuf {
        self.inner.path().to_owned()
    }

    /// The snapshot's machine: RAM size in bytes.
    #[getter]
    fn mem_size(&self) -> usize {
        self.inner.config().mem_size
    }

    /// The snapshot's machine: how many vCPUs.
    #[getter]
    fn vcpus(&self) -> u32 {
        self.inner.config().vcpus
    }

    /// The snapshot's machine: its CPU model, "host" or "baseline".
    #[getter]
    fn cpu_model(&self) -> String {
        self.inner.config().cpu_model.to_string()
    }

    /// The fingerprint of the CPU the snapshot's guest saw.
    #[getter]
    fn cpu_fingerprint(&self) -> &str {
        self.inner.cpu_fingerprint()
    }

    fn __repr__(&self) -> String {
        format!(
            "Snapshot({:?}, mem_size={}, vcpus={})",
            self.inner.path(),
            self.inner.config().mem_size,
            self.inner.config().vcpus
        )
    }
}

#[pymodule]
fn _libmachine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Machine>()?;
    m.add_class::<ExtentDaemon>()?;
    m.add_class::<ExtentStore>()?;
    m.add_class::<Exit>()?;
    m.add_class::<Registers>()?;
    m.add_class::<StopHandle>()?;
    m.add_class::<Agent>()?;
    m.add_class::<Process>()?;
    m.add_class::<ExecResult>()?;
    m.add_class::<Vsock>()?;
    m.add_class::<EgressPolicy>()?;
    m.add_class::<Network>()?;
    m.add_class::<NetworkConfig>()?;
    m.add_class::<ConnectionRequest>()?;
    m.add_class::<ConnectionRecord>()?;
    m.add_class::<AsyncSandbox>()?;
    m.add_class::<AsyncProcess>()?;
    m.add_function(wrap_pyfunction!(_python_exiting, m)?)?;
    m.add_function(wrap_pyfunction!(_after, m)?)?;
    m.py().import("atexit")?.call_method1("register", (m.getattr("_python_exiting")?,))?;
    m.add_class::<Snapshot>()?;
    m.add("SNAPSHOT_FORMAT_VERSION", libmachine::SNAPSHOT_FORMAT_VERSION)?;
    m.add("GUEST_ADDR", libmachine::net::GUEST_ADDR.to_string())?;
    m.add("GATEWAY_ADDR", libmachine::net::GATEWAY_ADDR.to_string())?;
    m.add("MachineError", m.py().get_type::<MachineError>())?;
    m.add("GuestBusyError", m.py().get_type::<GuestBusyError>())?;
    Ok(())
}
