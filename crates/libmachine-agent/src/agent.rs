// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The agent proper (Linux only).

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use libmachine_agent::{
    CHUNK, DEFAULT_PORT, EAGAIN, EPROTO, ESRCH, ExecRequest, Exit, Frame, ORPHAN_GRACE,
    RESULT_KEPT, check_preamble, preamble, read_frame, write_frame,
};

use crate::jobs::{Forgotten, Stream};

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// How much stdin the agent holds for a process that is not reading it
/// before it stops reading the connection.
const STDIN_BUFFER: usize = 1 << 20;
/// The most commands the agent keeps at once (running, or finished with a
/// result a host may still attach for).
const MAX_JOBS: usize = 1024;
/// Past this much output kept in all, finished commands no host is
/// attached to are forgotten, oldest first.
const KEPT_TOTAL: usize = 64 << 20;
/// How many forgotten command IDs the agent remembers, refusing to run them
/// again.
const FORGOTTEN: usize = 4096;

pub(crate) fn main() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
    let port = |s: &[u8]| std::str::from_utf8(s).ok().and_then(|s| s.parse::<u32>().ok());
    match args.as_slice() {
        [] => serve(DEFAULT_PORT),
        [b"--port", p] => match port(p) {
            Some(p) => serve(p),
            None => usage(),
        },
        [b"connect", p] => match port(p) {
            Some(p) => connect(p),
            None => usage(),
        },
        _ => usage(),
    }
}

fn usage() -> i32 {
    eprintln!("usage: libmachine-agent [--port PORT] | connect PORT");
    2
}

/// A vsock stream socket.
struct Socket(OwnedFd);

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

fn address(cid: u32, port: u32) -> libc::sockaddr_vm {
    // SAFETY: sockaddr_vm is plain data; all zeroes is valid.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = cid;
    addr.svm_port = port;
    addr
}

const ADDR_LEN: libc::socklen_t = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;

impl Socket {
    fn new() -> io::Result<Self> {
        // SAFETY: plain system call; the fd is owned from here.
        let fd = check(unsafe {
            libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0)
        })?;
        // SAFETY: a new fd nothing else owns.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn listen(port: u32) -> io::Result<Self> {
        let s = Self::new()?;
        let addr = address(libc::VMADDR_CID_ANY, port);
        // SAFETY: `addr` is a valid sockaddr_vm of ADDR_LEN bytes.
        check(unsafe { libc::bind(s.fd(), (&raw const addr).cast(), ADDR_LEN) })?;
        // SAFETY: plain system call on our fd.
        check(unsafe { libc::listen(s.fd(), 128) })?;
        Ok(s)
    }

    fn connect(cid: u32, port: u32) -> io::Result<Self> {
        let s = Self::new()?;
        let addr = address(cid, port);
        // SAFETY: `addr` is a valid sockaddr_vm of ADDR_LEN bytes.
        check(unsafe { libc::connect(s.fd(), (&raw const addr).cast(), ADDR_LEN) })?;
        Ok(s)
    }

    fn accept(&self) -> io::Result<Self> {
        loop {
            // SAFETY: plain system call; null address pointers are allowed.
            let fd = unsafe {
                libc::accept4(
                    self.fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            match check(fd) {
                // SAFETY: a new fd nothing else owns.
                Ok(fd) => return Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) })),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    fn shutdown(&self) {
        // SAFETY: plain system call on our fd.
        unsafe { libc::shutdown(self.fd(), libc::SHUT_RDWR) };
    }

    fn shutdown_write(&self) {
        // SAFETY: plain system call on our fd.
        unsafe { libc::shutdown(self.fd(), libc::SHUT_WR) };
    }
}

impl Read for &Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: the kernel writes at most buf.len() bytes to buf.
            let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

impl Write for &Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            // SAFETY: the kernel reads at most buf.len() bytes from buf.
            let n = unsafe {
                libc::send(self.fd(), buf.as_ptr().cast(), buf.len(), libc::MSG_NOSIGNAL)
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serve(port: u32) -> i32 {
    if std::env::var_os("PATH").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("PATH", DEFAULT_PATH) };
    }
    let listener = match Socket::listen(port) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("libmachine-agent: listen on vsock port {port}: {e}");
            return 1;
        }
    };
    eprintln!("libmachine-agent: listening on vsock port {port}");
    let jobs = Arc::new(Jobs {
        table: Mutex::new(Table { jobs: HashMap::new(), forgotten: Forgotten::new(FORGOTTEN) }),
        janitor: Condvar::new(),
    });
    {
        let jobs = jobs.clone();
        if let Err(e) =
            std::thread::Builder::new().name("janitor".into()).spawn(move || janitor(&jobs))
        {
            eprintln!("libmachine-agent: start a thread: {e}");
            return 1;
        }
    }
    loop {
        match listener.accept() {
            Ok(sock) => {
                let conn = Arc::new(Conn { sock, writing: Mutex::new(()) });
                let jobs = jobs.clone();
                let spawned =
                    std::thread::Builder::new().name("exchange".into()).spawn(move || {
                        let _ = exchange(&conn, &jobs);
                        conn.sock.shutdown();
                    });
                if let Err(e) = spawned {
                    eprintln!("libmachine-agent: start a thread: {e}");
                }
            }
            Err(e) => {
                eprintln!("libmachine-agent: accept: {e}");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

/// The errno of `e`, EIO if it has none.
fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

fn error_frame(what: impl std::fmt::Display, e: &io::Error) -> Frame {
    Frame::Error { errno: errno(e), message: format!("{what}: {e}") }
}

/// This run of the agent: a random number, so that a host can tell an
/// agent that restarted (and forgot its commands) from the one before.
fn instance() -> u64 {
    static INSTANCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *INSTANCE.get_or_init(|| {
        let mut b = [0u8; 8];
        // SAFETY: getrandom writes at most b.len() bytes to b.
        let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
        if n == b.len() as isize {
            u64::from_ne_bytes(b)
        } else {
            // No entropy source: the time and PID still differ between runs.
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            t.as_nanos() as u64 ^ (u64::from(std::process::id()) << 48)
        }
    })
}

/// The restore generation the agent last reseeded the kernel's RNG for
/// (0: none, a machine that booted).
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Mixes the host's `entropy` into the kernel's RNG, crediting it, and has
/// the kernel reseed its CRNG from it now, for restore `generation`: the
/// answer is `Done` only once both are done. A clone of a snapshot otherwise
/// starts with its RNG in the state of the machine it was cloned from.
fn reseed(generation: u64, entropy: &[u8]) -> Frame {
    // RNDADDENTROPY and RNDRESEEDCRNG (linux/random.h); both need
    // CAP_SYS_ADMIN, which the agent has.
    const RNDADDENTROPY: libc::c_ulong = 0x4008_5203;
    const RNDRESEEDCRNG: libc::c_ulong = 0x5207;
    let result = (|| -> io::Result<()> {
        let random = std::fs::OpenOptions::new().write(true).open("/dev/urandom")?;
        // struct rand_pool_info { int entropy_count; int buf_size; __u32 buf[]; }
        let mut info = Vec::with_capacity(8 + entropy.len().next_multiple_of(4));
        info.extend_from_slice(&((entropy.len() * 8) as i32).to_ne_bytes());
        info.extend_from_slice(&(entropy.len() as i32).to_ne_bytes());
        info.extend_from_slice(entropy);
        info.resize(8 + entropy.len().next_multiple_of(4), 0);
        let fd = random.as_raw_fd();
        // SAFETY: `info` is a rand_pool_info whose buffer holds buf_size
        // bytes; the kernel only reads it.
        if unsafe { libc::ioctl(fd, RNDADDENTROPY as _, info.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: an ioctl without an argument.
        if unsafe { libc::ioctl(fd, RNDRESEEDCRNG as _) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            GENERATION.store(generation, Ordering::SeqCst);
            Frame::Done
        }
        Err(e) => error_frame("reseeding the guest's RNG", &e),
    }
}

/// Serves one connection's exchange. An error means the connection broke:
/// there is nobody left to tell.
fn exchange(conn: &Arc<Conn>, jobs: &Arc<Jobs>) -> io::Result<()> {
    let sock = &conn.sock;
    let mut hello = preamble().to_vec();
    hello.extend(
        Frame::Hello { instance: instance(), generation: GENERATION.load(Ordering::SeqCst) }
            .encode(),
    );
    conn.send_bytes(&hello)?;
    let mut theirs = [0; 8];
    (&*sock).read_exact(&mut theirs)?;
    if let Err(e) = check_preamble(&theirs) {
        let refusal = Frame::Error { errno: EPROTO, message: e.to_string() };
        return conn.send(&refusal);
    }
    let reply = match read_frame(sock) {
        Ok(Some(f)) => f,
        Ok(None) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            return conn.send(&error_frame("the request", &e));
        }
        Err(e) => return Err(e),
    };
    match reply {
        Frame::Ping => conn.send(&Frame::Pong),
        Frame::Reseed { generation, entropy } => conn.send(&reseed(generation, &entropy)),
        Frame::ReadFile { path } => read_file(sock, &path),
        Frame::WriteFile { path, mode } => write_file(sock, &path, mode),
        Frame::Exec { id, request } => exec(conn, jobs, id, request),
        Frame::Attach { id, stdout, stderr } => attach(conn, jobs, id, stdout, stderr),
        other => {
            let e = io::Error::from_raw_os_error(EPROTO);
            conn.send(&error_frame(format!("{} is not a request", other.name()), &e))
        }
    }
}

fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

fn read_file(conn: &Socket, path: &[u8]) -> io::Result<()> {
    let shown = String::from_utf8_lossy(path);
    let mut file = match File::open(os(path)).and_then(|f| {
        if f.metadata()?.is_dir() { Err(io::Error::from_raw_os_error(libc::EISDIR)) } else { Ok(f) }
    }) {
        Ok(f) => f,
        Err(e) => return write_frame(conn, &error_frame(&shown, &e)),
    };
    let mut buf = vec![0; CHUNK];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return write_frame(conn, &Frame::Data(Vec::new())),
            Ok(n) => write_frame(conn, &Frame::Data(buf[..n].to_vec()))?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return write_frame(conn, &error_frame(&shown, &e)),
        }
    }
}

/// Removes a temporary file when dropped, unless it was renamed into place.
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_file(conn: &Socket, path: &[u8], mode: u32) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let shown = String::from_utf8_lossy(path).into_owned();
    // The last component as the kernel sees it: Path would drop a
    // trailing "/" or "." and name another file.
    let last = path.rsplit(|&b| b == b'/').next().unwrap_or_default();
    if matches!(last, b"" | b"." | b"..") {
        let e = io::Error::from_raw_os_error(libc::EISDIR);
        return write_frame(conn, &error_frame(&shown, &e));
    }
    let target = PathBuf::from(OsString::from_vec(path.to_vec()));
    let name = os(last);
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(
        ".libmachine-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = target.with_file_name(temp_name);
    let opened = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp);
    let mut file = match opened {
        Ok(f) => f,
        Err(e) => return write_frame(conn, &error_frame(&shown, &e)),
    };
    let mut guard = TempFile(Some(temp.clone()));
    // Take every Data frame, even after a write fails, so that the host
    // reads the error rather than a reset.
    let mut failed: Option<io::Error> = None;
    loop {
        match read_frame(conn)? {
            Some(Frame::Data(d)) if d.is_empty() => break,
            Some(Frame::Data(d)) => {
                if failed.is_none() {
                    failed = file.write_all(&d).err();
                }
            }
            Some(other) => {
                let e = io::Error::from_raw_os_error(EPROTO);
                return write_frame(conn, &error_frame(format!("{other:?} in a file"), &e));
            }
            None => return Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
    let result = match failed {
        Some(e) => Err(e),
        None => file
            .set_permissions(Permissions::from_mode(mode & 0o7777))
            .and_then(|()| std::fs::rename(&temp, &target)),
    };
    match result {
        Ok(()) => {
            guard.0 = None;
            write_frame(conn, &Frame::Done)
        }
        Err(e) => write_frame(conn, &error_frame(&shown, &e)),
    }
}

/// A connection: read by one thread, written whole frames at a time by
/// several.
struct Conn {
    sock: Socket,
    writing: Mutex<()>,
}

impl Conn {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        self.send_bytes(&frame.encode())
    }

    fn send_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let _guard = lock(&self.writing);
        (&self.sock).write_all(bytes)
    }

    /// The peer is gone (the connection was reset or shut down), without
    /// reading from it.
    fn hung_up(&self) -> bool {
        let mut fd = libc::pollfd { fd: self.sock.fd(), events: 0, revents: 0 };
        // SAFETY: one pollfd, no waiting.
        let n = unsafe { libc::poll(&mut fd, 1, 0) };
        n > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR) != 0
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A command, which lives apart from the connections that attach to it.
struct Job {
    id: u64,
    pid: u32,
    state: Mutex<JobState>,
    /// Notified on every change of `state`.
    changed: Condvar,
}

#[derive(Default)]
struct JobState {
    stdout: Stream,
    stderr: Stream,
    /// Set once the process has exited and its output pipes have ended.
    exit: Option<Exit>,
    finished_at: Option<Instant>,
    /// Stdin taken from the host, not yet written to the process.
    stdin: VecDeque<u8>,
    /// Bytes of stdin taken from the host, ever.
    stdin_taken: u64,
    /// The host closed stdin: close the pipe once `stdin` is written.
    stdin_close: bool,
    /// The pipe is closed (or there never was one): stdin is dropped.
    stdin_gone: bool,
    /// Signals taken from the host, ever.
    signals: u64,
    /// The attached connection's number (0: none), and the connection, to
    /// shut it down when another takes over.
    conn: u64,
    attached: Option<Arc<Conn>>,
    /// No connection is attached since then.
    detached_since: Option<Instant>,
    /// The host released it: nobody takes its output again.
    released: bool,
    /// It was killed for having no connection for too long.
    orphan_killed: bool,
}

impl JobState {
    fn kept(&self) -> usize {
        self.stdout.kept() + self.stderr.kept()
    }
}

/// The agent's commands by the host's request IDs.
struct Jobs {
    table: Mutex<Table>,
    /// Wakes the janitor: a command lost its connection or finished.
    janitor: Condvar,
}

struct Table {
    jobs: HashMap<u64, Arc<Job>>,
    forgotten: Forgotten,
}

impl Jobs {
    fn wake_janitor(&self) {
        // Under the table's lock, so that the janitor, which holds it from
        // looking at the jobs until it waits, cannot miss the wakeup.
        let _table = lock(&self.table);
        self.janitor.notify_all();
    }
}

/// Numbers the connections attaching to commands.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

fn kill_group(pgid: u32, signal: i32) {
    // SAFETY: plain system call. The group is the process's own, made for
    // it by process_group(0).
    unsafe { libc::kill(-(pgid as libc::pid_t), signal) };
}

/// Kills orphans whose host has not come back within the grace, and forgets
/// finished commands whose result nobody came for.
fn janitor(jobs: &Jobs) {
    let mut table = lock(&jobs.table);
    loop {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut expired = Vec::new();
        for (&id, job) in &table.jobs {
            let mut st = lock(&job.state);
            let Some(since) = st.detached_since else {
                continue;
            };
            let at = match st.finished_at {
                None if st.orphan_killed => continue,
                None => since + Duration::from_secs(ORPHAN_GRACE),
                Some(finished) => since.max(finished) + Duration::from_secs(RESULT_KEPT),
            };
            if now < at {
                next = Some(next.map_or(at, |n| n.min(at)));
            } else if st.finished_at.is_none() {
                kill_group(job.pid, libc::SIGKILL);
                st.orphan_killed = true;
            } else {
                expired.push(id);
            }
        }
        for id in expired {
            table.jobs.remove(&id);
            table.forgotten.insert(id);
        }
        table = match next {
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                jobs.janitor.wait_timeout(table, left).unwrap_or_else(|e| e.into_inner()).0
            }
            None => jobs.janitor.wait(table).unwrap_or_else(|e| e.into_inner()),
        };
    }
}

/// Makes room in the table for one more command: forgets finished commands
/// no host is attached to, the longest detached first, while there are too
/// many or they keep too much output. False if it is still full.
fn make_room(table: &mut Table) -> bool {
    let mut kept = 0;
    let mut idle: Vec<(Instant, u64)> = Vec::new();
    for (&id, job) in &table.jobs {
        let st = lock(&job.state);
        kept += st.kept();
        if let (Some(since), Some(_)) = (st.detached_since, st.finished_at) {
            idle.push((since, id));
        }
    }
    idle.sort();
    let mut idle = idle.into_iter();
    while table.jobs.len() >= MAX_JOBS || kept > KEPT_TOTAL {
        let Some((_, id)) = idle.next() else { break };
        if let Some(job) = table.jobs.remove(&id) {
            kept -= lock(&job.state).kept();
        }
        table.forgotten.insert(id);
    }
    table.jobs.len() < MAX_JOBS
}

fn exec(conn: &Arc<Conn>, jobs: &Arc<Jobs>, id: u64, request: ExecRequest) -> io::Result<()> {
    let job = {
        let mut table = lock(&jobs.table);
        if let Some(job) = table.jobs.get(&id) {
            // Sent again after a reset: the same command, from its start.
            job.clone()
        } else if table.forgotten.contains(id) {
            drop(table);
            return conn.send(&Frame::Error {
                errno: ESRCH,
                message: format!("command {id:#x} ended and its result was forgotten"),
            });
        } else if !make_room(&mut table) {
            drop(table);
            return conn.send(&Frame::Error {
                errno: EAGAIN,
                message: format!("the agent already has {MAX_JOBS} commands"),
            });
        } else {
            // Started under the table's lock, so that the same request
            // arriving on two connections at once runs once.
            match start(id, request, jobs) {
                Ok(job) => {
                    table.jobs.insert(id, job.clone());
                    job
                }
                Err(frame) => {
                    drop(table);
                    return conn.send(&frame);
                }
            }
        }
    };
    serve_job(conn, jobs, &job, (0, 0), false)
}

fn attach(conn: &Arc<Conn>, jobs: &Arc<Jobs>, id: u64, stdout: u64, stderr: u64) -> io::Result<()> {
    let job = lock(&jobs.table).jobs.get(&id).cloned();
    match job {
        Some(job) => serve_job(conn, jobs, &job, (stdout, stderr), true),
        None => conn.send(&Frame::Error {
            errno: ESRCH,
            message: format!(
                "no command {id:#x}: it ended and was forgotten, or the agent restarted"
            ),
        }),
    }
}

/// Starts a command and the threads that look after it.
fn start(id: u64, request: ExecRequest, jobs: &Arc<Jobs>) -> Result<Arc<Job>, Frame> {
    let program = String::from_utf8_lossy(&request.program).into_owned();
    let mut cmd = Command::new(os(&request.program));
    cmd.args(request.args.iter().map(|a| os(a)))
        .env_clear()
        .envs(request.env.iter().map(|(k, v)| (os(k), os(v))))
        .stdin(if request.stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(cwd) = &request.cwd {
        cmd.current_dir(os(cwd));
    }
    let mut child = cmd.spawn().map_err(|e| error_frame(&program, &e))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let stdin = child.stdin.take();
    let job = Arc::new(Job {
        id,
        pid: child.id(),
        state: Mutex::new(JobState { stdin_gone: stdin.is_none(), ..Default::default() }),
        changed: Condvar::new(),
    });
    let spawned = (|| {
        let thread = |name: &str| std::thread::Builder::new().name(name.into());
        let j = job.clone();
        let out = thread("stdout").spawn(move || pump(&j, stdout, |s| &mut s.stdout))?;
        let j = job.clone();
        let err = thread("stderr").spawn(move || pump(&j, stderr, |s| &mut s.stderr))?;
        if let Some(stdin) = stdin {
            let j = job.clone();
            thread("stdin").spawn(move || feed_stdin(&j, stdin))?;
        }
        let (j, jobs) = (job.clone(), jobs.clone());
        thread("waiter").spawn(move || wait(&j, &jobs, child, [out, err]))?;
        io::Result::Ok(())
    })();
    if let Err(e) = spawned {
        // Whatever started ends with the process.
        kill_group(job.pid, libc::SIGKILL);
        return Err(error_frame("start a thread", &e));
    }
    Ok(job)
}

/// Reads what `pipe` gives into the stream `which` picks, while there is
/// room: no further ahead of what was sent than the stream allows.
fn pump(job: &Job, mut pipe: impl Read, which: fn(&mut JobState) -> &mut Stream) {
    let mut buf = vec![0; CHUNK];
    loop {
        let room = {
            let mut st = lock(&job.state);
            loop {
                let room = if st.released { CHUNK } else { which(&mut st).room() };
                if room > 0 {
                    break room.min(CHUNK);
                }
                st = job.changed.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let n = match pipe.read(&mut buf[..room]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut st = lock(&job.state);
        if !st.released {
            which(&mut st).push(&buf[..n]);
        }
        drop(st);
        job.changed.notify_all();
    }
    which(&mut lock(&job.state)).eof = true;
    job.changed.notify_all();
}

/// Writes the stdin the host gave to the process, closing it when the host
/// did.
fn feed_stdin(job: &Job, mut stdin: ChildStdin) {
    let mut chunk = Vec::with_capacity(CHUNK);
    loop {
        {
            let mut st = lock(&job.state);
            loop {
                if st.released || st.exit.is_some() || (st.stdin.is_empty() && st.stdin_close) {
                    st.stdin_gone = true;
                    st.stdin.clear();
                    drop(st);
                    job.changed.notify_all();
                    return;
                }
                if !st.stdin.is_empty() {
                    let n = st.stdin.len().min(CHUNK);
                    chunk.extend(st.stdin.drain(..n));
                    break;
                }
                st = job.changed.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        }
        // Room in the buffer: the connection may read on.
        job.changed.notify_all();
        if stdin.write_all(&chunk).is_err() {
            // The process closed its stdin.
            let mut st = lock(&job.state);
            st.stdin_gone = true;
            st.stdin.clear();
            drop(st);
            job.changed.notify_all();
            return;
        }
        chunk.clear();
    }
}

/// Waits for the process, then for its output to end: then it has exited.
fn wait(job: &Job, jobs: &Jobs, mut child: Child, pumps: [std::thread::JoinHandle<()>; 2]) {
    let status = child.wait();
    for p in pumps {
        let _ = p.join();
    }
    let exit = match status {
        Ok(st) => match (st.code(), st.signal()) {
            (Some(code), _) => Exit::Code(code),
            (None, Some(signal)) => Exit::Signal(signal),
            (None, None) => Exit::Code(-1),
        },
        Err(_) => Exit::Code(-1),
    };
    let mut st = lock(&job.state);
    st.exit = Some(exit);
    st.finished_at = Some(Instant::now());
    drop(st);
    job.changed.notify_all();
    jobs.wake_janitor();
}

/// Serves a command on a connection that attached to it, sending its
/// output from `offsets` (stdout, stderr) on, until the connection ends,
/// another takes over, or the host releases the command.
fn serve_job(
    conn: &Arc<Conn>,
    jobs: &Jobs,
    job: &Arc<Job>,
    offsets: (u64, u64),
    attach: bool,
) -> io::Result<()> {
    let me = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let (reply, previous) = {
        let mut st = lock(&job.state);
        let checked = if st.released {
            Err(ESRCH)
        } else {
            st.stdout.check(offsets.0).and(st.stderr.check(offsets.1))
        };
        if let Err(errno) = checked {
            drop(st);
            return conn.send(&Frame::Error {
                errno,
                message: format!("command {:#x}: no output kept from offsets {offsets:?}", job.id),
            });
        }
        st.conn = me;
        st.detached_since = None;
        let previous = st.attached.replace(conn.clone());
        let reply = if attach {
            Frame::Attached { stdin: st.stdin_taken, signals: st.signals }
        } else {
            Frame::Started { pid: job.pid }
        };
        (reply, previous)
    };
    // A connection the guest still thinks live (the host's end was reset):
    // this one takes over, and its threads stop.
    if let Some(previous) = previous {
        previous.sock.shutdown();
    }
    job.changed.notify_all();
    if conn.send(&reply).is_err() {
        detach(jobs, job, me);
        return Ok(());
    }
    std::thread::scope(|s| {
        s.spawn(|| send_output(conn, job, me, offsets));
        take_input(conn, jobs, job, me);
        // Wakes the sender if it is writing.
        conn.sock.shutdown();
    });
    Ok(())
}

/// Sends the command's output from `offsets`, then its exit, while `me` is
/// the attached connection.
fn send_output(conn: &Conn, job: &Job, me: u64, (mut out, mut err): (u64, u64)) {
    let mut stdout_turn = true;
    loop {
        let frame = {
            let mut st = lock(&job.state);
            loop {
                if st.conn != me || st.released {
                    return;
                }
                let has_out = st.stdout.end() > out;
                let has_err = st.stderr.end() > err;
                if has_out && (stdout_turn || !has_err) {
                    let d = st.stdout.chunk(out, CHUNK);
                    out += d.len() as u64;
                    break Frame::Stdout(d);
                }
                if has_err {
                    let d = st.stderr.chunk(err, CHUNK);
                    err += d.len() as u64;
                    break Frame::Stderr(d);
                }
                if let Some(exit) = st.exit {
                    break Frame::Exit(exit);
                }
                st = job.changed.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        stdout_turn = !stdout_turn;
        let last = matches!(frame, Frame::Exit(_));
        if conn.send(&frame).is_err() || last {
            return;
        }
        let mut st = lock(&job.state);
        if st.conn == me {
            st.stdout.mark_sent(out);
            st.stderr.mark_sent(err);
        }
        drop(st);
        // Room for the pumps.
        job.changed.notify_all();
    }
}

/// Takes the host's frames for the command: stdin (buffered, so that a
/// process not reading its stdin never keeps the agent from seeing the
/// connection end), signals, and its release.
fn take_input(conn: &Conn, jobs: &Jobs, job: &Job, me: u64) {
    loop {
        {
            let mut st = lock(&job.state);
            loop {
                if st.conn != me {
                    return;
                }
                if st.stdin.len() < STDIN_BUFFER || st.stdin_gone {
                    break;
                }
                st = job
                    .changed
                    .wait_timeout(st, Duration::from_millis(200))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
                if conn.hung_up() {
                    drop(st);
                    return detach(jobs, job, me);
                }
            }
        }
        match read_frame(&conn.sock) {
            Ok(Some(Frame::Stdin(data))) => {
                let mut st = lock(&job.state);
                if st.conn != me {
                    return;
                }
                if data.is_empty() {
                    st.stdin_close = true;
                } else {
                    st.stdin_taken += data.len() as u64;
                    if !st.stdin_gone && !st.stdin_close {
                        st.stdin.extend(data);
                    }
                }
                drop(st);
                job.changed.notify_all();
            }
            Ok(Some(Frame::Signal(signal))) => {
                let mut st = lock(&job.state);
                if st.conn != me {
                    return;
                }
                st.signals += 1;
                if st.exit.is_none() {
                    kill_group(job.pid, signal);
                }
            }
            Ok(Some(Frame::Release)) => return release(jobs, job),
            // Nothing else belongs in an exec: ignored.
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return detach(jobs, job, me),
        }
    }
}

/// Connection `me` ended: if it was the attached one, the command waits
/// for the host to attach again.
fn detach(jobs: &Jobs, job: &Job, me: u64) {
    let mut st = lock(&job.state);
    if st.conn != me {
        return;
    }
    st.conn = 0;
    st.attached = None;
    st.detached_since = Some(Instant::now());
    drop(st);
    job.changed.notify_all();
    jobs.wake_janitor();
}

/// The host is done with the command: it is killed if it still runs, and
/// forgotten.
fn release(jobs: &Jobs, job: &Job) {
    let mut st = lock(&job.state);
    st.released = true;
    if st.exit.is_none() {
        kill_group(job.pid, libc::SIGKILL);
    }
    st.stdout.discard();
    st.stderr.discard();
    st.stdin.clear();
    st.conn = 0;
    st.attached = None;
    drop(st);
    job.changed.notify_all();
    let mut table = lock(&jobs.table);
    if table.jobs.get(&job.id).is_some_and(|j| std::ptr::eq(Arc::as_ptr(j), job)) {
        table.jobs.remove(&job.id);
    }
}

/// `connect PORT`: stdin to the host's port, the answer to stdout.
fn connect(port: u32) -> i32 {
    let conn = match Socket::connect(libc::VMADDR_CID_HOST, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("libmachine-agent: connect to the host's port {port}: {e}");
            return 1;
        }
    };
    let result = std::thread::scope(|s| {
        s.spawn(|| {
            let _ = io::copy(&mut io::stdin().lock(), &mut &conn);
            conn.shutdown_write();
        });
        io::copy(&mut &conn, &mut io::stdout().lock())
    });
    match result {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("libmachine-agent: connection to port {port}: {e}");
            1
        }
    }
}
