// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The host's client of the guest agent (`libmachine-agent`, in the guest):
//! running commands with their stdio, and copying files in and out, over
//! the machine's vsock.
//!
//! The protocol is `libmachine_agent`'s, versioned: an agent of another
//! version is refused with an error naming both. Each exchange takes a new
//! connection, and the client follows cladefs's reconnect rules (its
//! integrator guide, "Guests' vsock connections across a restore"):
//!
//! - **A reset means connect again.** A connect the guest refuses or resets,
//!   or a connection that ends before the whole answer is in, is tried
//!   again on a new connection a few milliseconds later, for up to about a
//!   second ([`Agent::retry_for`]).
//! - **Only what is safe to see twice is sent again.** Pings, reading a
//!   file and writing one (a whole file, renamed into place) are. A command
//!   is made so by a request ID the client picks: the agent runs a command
//!   once per ID, and a request sent again attaches to the first. A running
//!   [`Process`] whose connection is reset takes up its command again on a
//!   new one, from the output it has; the stdin written and the signals sent
//!   that the agent had not taken are sent again, so that the command sees
//!   each once.
//! - **What cannot be taken up fails with [`Error::ConnectionReset`]:** the
//!   agent restarted (it then has another instance number) after the
//!   request may have reached it, it forgot the command (its result is kept
//!   [`RESULT_KEPT`](libmachine_agent::RESULT_KEPT) seconds after its
//!   connection went), or output the client had not received is no longer
//!   kept.
//!
//! A guest that stops (it powers off, resets or triple-faults) or whose
//! machine is dropped fails every call, waiting or new, at once with
//! [`Error::GuestGone`]. The agent kills a command whose [`Process`] is
//! dropped (or whose connection stays gone for
//! [`ORPHAN_GRACE`](libmachine_agent::ORPHAN_GRACE) seconds), so a dropped
//! [`Process`] does not leave it running.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use libmachine_agent::{
    CHUNK, ENOBUFS, ESRCH, ExecRequest, Exit, Frame, check_preamble, preamble, read_frame,
    write_frame,
};

use crate::devices::virtio::vsock::{Vsock, VsockStream, is_guest_stopped};
use crate::error::{Error, Result};

pub use libmachine_agent::{DEFAULT_PORT, VERSION as PROTOCOL_VERSION};

/// The `PATH` a [`Command`] has unless it sets its own.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// How long a connect waits for the guest to answer, unless
/// [`Agent::timeout`] says otherwise.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(2);
const MAX_RETRY_DELAY: Duration = Duration::from_millis(50);
/// The longest a command's request is sent again, or its process taken up
/// again, after a reset, whatever [`Agent::retry_for`] says: well within
/// how long the agent remembers a command whose connection went (60 s), so
/// that it never takes a request sent again for a new one.
pub const MAX_RECONNECT: Duration = Duration::from_secs(30);
/// The largest file [`Agent::read_file`] takes from the guest unless
/// [`Agent::max_file_size`] says otherwise: what the guest sends is
/// untrusted, and a guest that sends without end must not take the host's
/// memory with it (board 13488).
pub const DEFAULT_MAX_FILE_SIZE: usize = 64 << 20;
/// How much of what it wrote to a process's stdin the client keeps, to
/// send again what had not reached the agent when a connection was reset:
/// more than libmachine's vsock can have in flight (the host's 256 KiB send
/// queue, the guest's 256 KiB receive buffer and a frame).
const STDIN_KEPT: usize = 1 << 20;
/// How long a reader waits at a time for a link being replaced by another
/// thread (it is woken as soon as it is; this bounds a missed wake-up).
const RELINK_POLL: Duration = Duration::from_millis(10);

/// A connection to the agent, as the client needs it: reads and writes
/// through `&self`, so one thread can read while another writes.
pub(crate) trait Link: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write(&self, buf: &[u8]) -> io::Result<usize>;
    /// How long a read or write may wait (None: forever).
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

/// Makes connections to the agent.
pub(crate) trait Dial: Send + Sync {
    fn dial(&self, port: u32, timeout: Duration) -> io::Result<Box<dyn Link>>;
    /// Which restore of a snapshot the machine is (0: it booted): an agent
    /// whose `Hello` names another generation reseeds first.
    fn generation(&self) -> u64 {
        0
    }
}

impl Link for VsockStream {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        Read::read(&mut &*self, buf)
    }
    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        Write::write(&mut &*self, buf)
    }
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }
}

impl Dial for Vsock {
    fn dial(&self, port: u32, timeout: Duration) -> io::Result<Box<dyn Link>> {
        Ok(Box::new(self.connect(port, timeout)?))
    }
    fn generation(&self) -> u64 {
        Vsock::generation(self)
    }
}

/// How much host entropy a reseed gives the guest: as much as its CRNG's
/// key, twice over.
const RESEED_BYTES: usize = 64;

/// `Read` and `Write` over a link.
struct Io<'a>(&'a dyn Link);

impl Read for Io<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Io<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A request ID for a command: random, so that no two hosts or runs of one
/// (a machine restored from a snapshot in another process) pick the same.
fn request_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // Each RandomState has keys from the OS's random source.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.finish()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A command to run in the guest, built like `std::process::Command`.
///
/// Its environment is exactly what is set here, `PATH` being
/// [`DEFAULT_PATH`] unless set or cleared; a program without a slash is
/// looked up in that `PATH`.
#[derive(Debug, Clone)]
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<OsString>,
    stdin: Option<Vec<u8>>,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            env: vec![("PATH".into(), DEFAULT_PATH.into())],
            cwd: None,
            stdin: None,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(&mut self, args: I) -> &mut Self {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// Sets (or replaces) one environment variable.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref().to_owned();
        self.env.retain(|(k, _)| *k != key);
        self.env.push((key, value.as_ref().to_owned()));
        self
    }

    /// Clears the environment, `PATH` included.
    pub fn env_clear(&mut self) -> &mut Self {
        self.env.clear();
        self
    }

    /// The working directory (the agent's, `/`, by default).
    pub fn current_dir(&mut self, dir: impl AsRef<OsStr>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_owned());
        self
    }

    /// What [`Agent::exec`] feeds the command's stdin (else /dev/null).
    /// [`Agent::spawn`] ignores it: its process's stdin is streamed.
    pub fn stdin(&mut self, data: impl Into<Vec<u8>>) -> &mut Self {
        self.stdin = Some(data.into());
        self
    }

    fn request(&self, stdin: bool) -> ExecRequest {
        let b = |s: &OsStr| s.as_bytes().to_vec();
        ExecRequest {
            program: b(&self.program),
            args: self.args.iter().map(|a| b(a)).collect(),
            env: self.env.iter().map(|(k, v)| (b(k), b(v))).collect(),
            cwd: self.cwd.as_deref().map(b),
            stdin,
        }
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// Its exit status, if it exited.
    pub code: Option<i32>,
    /// The signal that killed it, if one did.
    pub signal: Option<i32>,
}

impl ExitStatus {
    /// It exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl From<Exit> for ExitStatus {
    fn from(exit: Exit) -> Self {
        match exit {
            Exit::Code(c) => Self { code: Some(c), signal: None },
            Exit::Signal(s) => Self { code: None, signal: Some(s) },
        }
    }
}

/// A finished command's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// What a running [`Process`] did next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// It ended, after all its output: the last event.
    Exit(ExitStatus),
}

/// How an attempt at an exchange failed.
enum Failure {
    /// The connection was refused or reset: connect again, the request
    /// being safe to send again.
    Reset(io::Error),
    /// The guest stopped, or its machine is gone: nothing will answer.
    Gone(io::Error),
    Fatal(Error),
}

/// How [`Agent::retrying`] gave up.
enum GaveUp {
    /// Resets until the retry budget ran out: nobody answered.
    NoAnswer(Error),
    /// For good.
    Failed(Error),
}

impl GaveUp {
    fn error(self) -> Error {
        match self {
            GaveUp::NoAnswer(e) | GaveUp::Failed(e) => e,
        }
    }
}

fn is_reset(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    // NotConnected is not among them: the vsock says that of a machine
    // that is gone.
    matches!(
        e.kind(),
        ConnectionRefused | ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
    )
}

/// The guest will not answer again: it stopped, or its machine is gone.
fn is_gone(e: &io::Error) -> bool {
    is_guest_stopped(e) || e.kind() == io::ErrorKind::NotConnected
}

fn failure(what: &str) -> impl Fn(io::Error) -> Failure + '_ {
    move |e| {
        if is_gone(&e) {
            Failure::Gone(e)
        } else if is_reset(&e) {
            Failure::Reset(e)
        } else if e.kind() == io::ErrorKind::TimedOut {
            Failure::Fatal(Error::TimedOut(format!("{what}: {e}")))
        } else {
            Failure::Fatal(Error::Agent(format!("{what}: {e}")))
        }
    }
}

fn gone(what: &str, e: &io::Error) -> Error {
    Error::GuestGone(format!("{what}: {e}"))
}

fn guest_error(what: &str, errno: i32, message: String) -> Failure {
    Failure::Fatal(Error::Guest {
        what: format!("{what}: {message}"),
        source: io::Error::from_raw_os_error(errno),
    })
}

fn unexpected(what: &str, frame: &Frame) -> Failure {
    Failure::Fatal(Error::Agent(format!("{what}: the agent sent {} unexpectedly", frame.name())))
}

/// One connection to the agent, past the preambles and the agent's hello.
struct Session {
    link: Box<dyn Link>,
    /// The agent's instance: another one means it restarted.
    instance: u64,
    /// The restore generation the agent last reseeded for.
    generation: u64,
}

impl Session {
    fn send(&self, what: &str, frame: &Frame) -> std::result::Result<(), Failure> {
        write_frame(Io(&*self.link), frame).map_err(failure(what))
    }

    /// The agent's next frame; the connection ending is a reset (the answer
    /// is not all in).
    fn recv(&self, what: &str) -> std::result::Result<Frame, Failure> {
        recv(&*self.link, what)
    }
}

fn recv(link: &dyn Link, what: &str) -> std::result::Result<Frame, Failure> {
    match read_frame(Io(link)) {
        Ok(Some(f)) => Ok(f),
        Ok(None) => Err(failure(what)(io::ErrorKind::UnexpectedEof.into())),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            Err(Failure::Fatal(Error::Agent(format!("{what}: {e}"))))
        }
        Err(e) => Err(failure(what)(e)),
    }
}

/// The guest agent, reached over a machine's vsock. Cheap to clone; usable
/// from any thread, including while another runs the machine (which it
/// must be, for the guest to answer).
#[derive(Clone)]
pub struct Agent {
    dial: Arc<dyn Dial>,
    port: u32,
    retry_for: Duration,
    timeout: Option<Duration>,
    max_file_size: usize,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("port", &self.port)
            .field("retry_for", &self.retry_for)
            .field("timeout", &self.timeout)
            .field("max_file_size", &self.max_file_size)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// The agent on `vsock`'s guest, at [`DEFAULT_PORT`].
    pub fn new(vsock: Vsock) -> Self {
        Self::with_dial(Arc::new(vsock))
    }

    pub(crate) fn with_dial(dial: Arc<dyn Dial>) -> Self {
        Self {
            dial,
            port: DEFAULT_PORT,
            retry_for: Duration::from_secs(1),
            timeout: None,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }

    /// The largest file [`read_file`](Self::read_file) takes
    /// ([`DEFAULT_MAX_FILE_SIZE`] by default): a larger one fails with
    /// [`Error::Guest`] (`EFBIG`), the client holding no more of it than
    /// this.
    pub fn max_file_size(mut self, bytes: usize) -> Self {
        self.max_file_size = bytes;
        self
    }

    /// The agent listens on `port` instead.
    pub fn port(mut self, port: u32) -> Self {
        self.port = port;
        self
    }

    /// For how long resets are met by connecting again (about a second by
    /// default, as cladefs's rules have it); for a command at most
    /// [`MAX_RECONNECT`].
    pub fn retry_for(mut self, duration: Duration) -> Self {
        self.retry_for = duration;
        self
    }

    /// The longest any one wait for the guest may take (a connect, the next
    /// output of a command, room for its stdin): past it the exchange fails
    /// with [`Error::TimedOut`]. None (the default) waits for as long as it
    /// takes, which is forever if nothing runs the machine; a connect then
    /// still gives up after 10 seconds.
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Connects, exchanges preambles and takes the agent's hello; a reset
    /// in that is never after a request was sent. An agent that has not
    /// reseeded the guest's RNG for this machine's generation (a machine
    /// restored from a snapshot) does so first, on a connection of its own,
    /// so that whatever the session carries runs after the reseed.
    fn open(&self, what: &str) -> std::result::Result<Session, Failure> {
        let generation = self.dial.generation();
        let session = self.connect(what)?;
        if session.generation == generation {
            return Ok(session);
        }
        let mut entropy = vec![0; RESEED_BYTES];
        crate::sys::getrandom(&mut entropy);
        let reseed = Frame::Reseed { generation, entropy };
        session.send(what, &reseed)?;
        match session.recv(what)? {
            Frame::Done => {}
            Frame::Error { errno, message } => {
                return Err(Failure::Fatal(Error::Agent(format!(
                    "{what}: the agent could not reseed the guest's RNG after a restore \
                     (errno {errno}): {message}"
                ))));
            }
            other => return Err(unexpected(what, &other)),
        }
        drop(session);
        let session = self.connect(what)?;
        if session.generation != generation {
            return Err(Failure::Fatal(Error::Agent(format!(
                "{what}: the agent reseeded for generation {generation:#x} but says {:#x}",
                session.generation
            ))));
        }
        Ok(session)
    }

    /// Connects, exchanges preambles and takes the agent's hello.
    fn connect(&self, what: &str) -> std::result::Result<Session, Failure> {
        let connect_timeout = self.timeout.unwrap_or(CONNECT_TIMEOUT);
        let link = self.dial.dial(self.port, connect_timeout).map_err(failure(what))?;
        link.set_timeout(self.timeout).map_err(failure(what))?;
        Io(&*link).write_all(&preamble()).map_err(failure(what))?;
        let mut theirs = [0; 8];
        Io(&*link).read_exact(&mut theirs).map_err(failure(what))?;
        check_preamble(&theirs).map_err(|e| {
            Failure::Fatal(Error::Agent(format!(
                "{what}: the agent on vsock port {}: {e}",
                self.port
            )))
        })?;
        match recv(&*link, what)? {
            Frame::Hello { instance, generation } => Ok(Session { link, instance, generation }),
            other => Err(unexpected(what, &other)),
        }
    }

    /// Runs `attempt` until it succeeds or fails for good, connecting again
    /// a few milliseconds after each reset, for at most `budget`.
    fn retrying<T>(
        &self,
        what: &str,
        budget: Duration,
        mut attempt: impl FnMut() -> std::result::Result<T, Failure>,
    ) -> std::result::Result<T, GaveUp> {
        let deadline = Instant::now() + budget;
        let mut delay = FIRST_RETRY_DELAY;
        loop {
            match attempt() {
                Ok(v) => return Ok(v),
                Err(Failure::Fatal(e)) => return Err(GaveUp::Failed(e)),
                Err(Failure::Gone(e)) => return Err(GaveUp::Failed(gone(what, &e))),
                Err(Failure::Reset(source)) => {
                    if Instant::now() + delay > deadline {
                        return Err(GaveUp::NoAnswer(Error::Agent(format!(
                            "{what}: no answer from the agent on vsock port {} \
                             within {budget:?}: {source}",
                            self.port
                        ))));
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(MAX_RETRY_DELAY);
                }
            }
        }
    }

    /// An exchange safe to make again after any reset, on a new session
    /// each time.
    fn exchange<T>(
        &self,
        what: &str,
        mut attempt: impl FnMut(Session) -> std::result::Result<T, Failure>,
    ) -> Result<T> {
        self.retrying(what, self.retry_for, || attempt(self.open(what)?)).map_err(GaveUp::error)
    }

    fn ping_within(&self, budget: Duration) -> std::result::Result<(), GaveUp> {
        self.retrying("ping", budget, || {
            let s = self.open("ping")?;
            s.send("ping", &Frame::Ping)?;
            match s.recv("ping")? {
                Frame::Pong => Ok(()),
                other => Err(unexpected("ping", &other)),
            }
        })
    }

    /// Checks that the agent answers.
    pub fn ping(&self) -> Result<()> {
        self.ping_within(self.retry_for).map_err(GaveUp::error)
    }

    /// Waits until the agent answers, at most `timeout`: while the guest
    /// boots, nothing listens yet. Any other failure (another protocol
    /// version, a guest that is gone) is returned at once.
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.ping_within(left.min(self.retry_for)) {
                Ok(()) => return Ok(()),
                Err(GaveUp::NoAnswer(_)) if !left.is_zero() => {}
                Err(GaveUp::NoAnswer(e)) => {
                    return Err(Error::TimedOut(format!(
                        "the agent did not answer within {timeout:?}: {e}"
                    )));
                }
                Err(GaveUp::Failed(e)) => return Err(e),
            }
        }
    }

    /// The budget for sending a command's request again, or taking up its
    /// process again.
    fn reconnect_budget(&self) -> Duration {
        self.retry_for.min(MAX_RECONNECT)
    }

    /// Starts `cmd`, its stdin a pipe the caller feeds through the
    /// [`Process`].
    pub fn spawn(&self, cmd: &Command) -> Result<Process> {
        self.start(cmd, true)
    }

    /// Starts `cmd`, its stdin a pipe if `stdin`, else /dev/null.
    pub(crate) fn start(&self, cmd: &Command, stdin: bool) -> Result<Process> {
        let what = format!("exec {}", cmd.program.to_string_lossy());
        let id = request_id();
        let request = Frame::Exec { id, request: cmd.request(stdin) };
        // The agent the request may have reached: sent again only to it.
        let mut reached: Option<u64> = None;
        let (session, pid) = self
            .retrying(&what, self.reconnect_budget(), || {
                let s = self.open(&what)?;
                if reached.is_some_and(|i| i != s.instance) {
                    return Err(Failure::Fatal(Error::ConnectionReset(format!(
                        "{what}: the agent restarted after the request was sent; \
                         the command may have run"
                    ))));
                }
                // Once all of it is out (a part is no request), it may have
                // reached this agent.
                let answer = s.send(&what, &request).and_then(|()| {
                    reached = Some(s.instance);
                    s.recv(&what)
                });
                match answer {
                    Ok(Frame::Started { pid }) => Ok((s, pid)),
                    Ok(Frame::Error { errno, message }) if errno == ESRCH => {
                        Err(Failure::Fatal(Error::ConnectionReset(format!(
                            "{what}: {message}; the command may have run"
                        ))))
                    }
                    Ok(Frame::Error { errno, message }) => Err(guest_error(&what, errno, message)),
                    Ok(other) => Err(unexpected(&what, &other)),
                    // The request went out: the command may run, and the
                    // guest is gone.
                    Err(Failure::Gone(e)) => Err(Failure::Fatal(Error::GuestGone(format!(
                        "{what}: {e}; the command may have run"
                    )))),
                    Err(f) => Err(f),
                }
            })
            .map_err(GaveUp::error)?;
        Ok(Process {
            agent: self.clone(),
            id,
            instance: session.instance,
            pid,
            what,
            reading: Mutex::new(()),
            writing: Mutex::new(Writing::default()),
            current: Mutex::new(Current {
                link: Some(Arc::from(session.link)),
                generation: 0,
                stdout: 0,
                stderr: 0,
                exit: None,
                failed: None,
            }),
            relinked: Condvar::new(),
        })
    }

    /// Runs `cmd` to its end: its output and how it ended. Its stdin is what
    /// [`Command::stdin`] gave, else /dev/null.
    pub fn exec(&self, cmd: &Command) -> Result<Output> {
        let process = self.start(cmd, cmd.stdin.is_some())?;
        std::thread::scope(|s| {
            if let Some(data) = &cmd.stdin {
                let process = &process;
                s.spawn(move || {
                    // The command may end without reading it all.
                    if process.write_stdin(data).is_ok() {
                        let _ = process.close_stdin();
                    }
                });
            }
            process.wait()
        })
    }

    /// The contents of the guest's file `path`: at most
    /// [`max_file_size`](Self::max_file_size) bytes, else it fails with
    /// [`Error::Guest`] (`EFBIG`).
    pub fn read_file(&self, path: impl AsRef<OsStr>) -> Result<Vec<u8>> {
        let path = path.as_ref();
        let what = format!("read {}", path.to_string_lossy());
        let request = Frame::ReadFile { path: path.as_bytes().to_vec() };
        self.exchange(&what, |s| {
            s.send(&what, &request)?;
            let mut data = Vec::new();
            loop {
                match s.recv(&what)? {
                    Frame::Data(d) if d.is_empty() => return Ok(data),
                    // Refused before it is held: the guest's sizes are its own.
                    Frame::Data(d) if d.len() > self.max_file_size - data.len() => {
                        return Err(Failure::Fatal(Error::Guest {
                            what: format!(
                                "{what}: the file is larger than the {} bytes a read takes",
                                self.max_file_size
                            ),
                            source: io::Error::from_raw_os_error(libc::EFBIG),
                        }));
                    }
                    Frame::Data(d) => data.extend_from_slice(&d),
                    Frame::Error { errno, message } => {
                        return Err(guest_error(&what, errno, message));
                    }
                    other => return Err(unexpected(&what, &other)),
                }
            }
        })
    }

    /// Makes the guest's file `path` hold `data`, with permissions `mode`:
    /// whole, or not at all (it is written beside and renamed over `path`).
    pub fn write_file(&self, path: impl AsRef<OsStr>, data: &[u8], mode: u32) -> Result<()> {
        let path = path.as_ref();
        let what = format!("write {}", path.to_string_lossy());
        let request = Frame::WriteFile { path: path.as_bytes().to_vec(), mode };
        self.exchange(&what, |s| {
            let sent = (|| {
                s.send(&what, &request)?;
                for chunk in data.chunks(CHUNK) {
                    s.send(&what, &Frame::Data(chunk.to_vec()))?;
                }
                s.send(&what, &Frame::Data(Vec::new()))
            })();
            // The agent may have refused early and closed: its answer says
            // why, if it is there.
            let answer = s.recv(&what);
            match (answer, sent) {
                (Ok(Frame::Done), Ok(())) => Ok(()),
                (Ok(Frame::Error { errno, message }), _) => Err(guest_error(&what, errno, message)),
                (_, Err(failed)) => Err(failed),
                (Ok(other), Ok(())) => Err(unexpected(&what, &other)),
                (Err(failed), Ok(())) => Err(failed),
            }
        })
    }

    /// Copies the host's file `from` to the guest's `to`, with its
    /// permissions.
    pub fn copy_to_guest(&self, from: impl AsRef<Path>, to: impl AsRef<OsStr>) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let from = from.as_ref();
        let io_err = |source| Error::Io { path: from.to_owned(), source };
        let data = std::fs::read(from).map_err(io_err)?;
        let mode = std::fs::metadata(from).map_err(io_err)?.permissions().mode() & 0o7777;
        self.write_file(to, &data, mode)
    }

    /// Copies the guest's file `from` to the host's `to`.
    pub fn copy_from_guest(&self, from: impl AsRef<OsStr>, to: impl AsRef<Path>) -> Result<()> {
        let data = self.read_file(from)?;
        let to = to.as_ref();
        std::fs::write(to, data).map_err(|source| Error::Io { path: to.to_owned(), source })
    }
}

/// What a [`Process`] wrote to its command, kept to send again what a
/// reset may have lost.
#[derive(Default)]
struct Writing {
    /// The last [`STDIN_KEPT`] bytes of stdin written.
    stdin: VecDeque<u8>,
    /// Bytes of stdin written, ever.
    stdin_written: u64,
    /// Stdin was closed.
    closed: bool,
    /// Every signal sent, in order.
    signals: Vec<i32>,
    /// What a reconnect sends again, while it is being sent: frames written
    /// after wait for it, so that they follow it.
    replaying: Option<Arc<Replay>>,
}

/// The frames a reconnect sends again, sent by a thread of their own while
/// the process's reader goes on reading the new connection. Sending them
/// with the reader parked, as before, could not finish when the command's
/// output was backed up: the command waits for its output to be read
/// before it reads more stdin, and the agent takes stdin only as the command
/// reads it, so a replay larger than the guest's buffers waited for a
/// reader that waited for the replay (found under board 13131's reset storms,
/// made frequent by board 13177's faster reconnects).
struct Replay {
    done: Mutex<bool>,
    finished: Condvar,
}

impl Replay {
    /// Sends `frames` on `link` from a thread of its own (on this one if
    /// the thread cannot start).
    fn start(link: Arc<dyn Link>, frames: Vec<Frame>) -> Option<Arc<Self>> {
        if frames.is_empty() {
            return None;
        }
        let replay = Arc::new(Self { done: Mutex::new(false), finished: Condvar::new() });
        let frames = Arc::new(frames);
        let spawned = {
            let (replay, link, frames) = (replay.clone(), link.clone(), frames.clone());
            std::thread::Builder::new()
                .name("libmachine-replay".into())
                .stack_size(64 << 10)
                .spawn(move || replay.send(&*link, &frames))
        };
        if spawned.is_err() {
            replay.send(&*link, &frames);
        }
        Some(replay)
    }

    fn send(&self, link: &dyn Link, frames: &[Frame]) {
        for frame in frames {
            // A reset ends it: whoever meets the reset next takes the
            // command up again, and sends again what the agent has not
            // taken.
            if write_frame(Io(link), frame).is_err() {
                break;
            }
        }
        *lock(&self.done) = true;
        self.finished.notify_all();
    }

    /// Waits until it is sent (or ended by a reset).
    fn wait(&self) {
        let mut done = lock(&self.done);
        while !*done {
            done = self.finished.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Writing {
    /// Waits until a replay being sent is out, so that what is written next
    /// follows it.
    fn after_replay(&mut self) {
        if let Some(replay) = self.replaying.take() {
            replay.wait();
        }
    }

    fn record_stdin(&mut self, data: &[u8]) {
        self.stdin.extend(data);
        self.stdin_written += data.len() as u64;
        let over = self.stdin.len().saturating_sub(STDIN_KEPT);
        self.stdin.drain(..over);
    }

    /// What to send again on a new connection to an agent that has taken
    /// `stdin` bytes and `signals` signals: None if it lacks stdin no
    /// longer kept, or claims more than was sent.
    fn replay(&self, stdin: u64, signals: u64) -> Option<Vec<Frame>> {
        let kept_from = self.stdin_written - self.stdin.len() as u64;
        if stdin < kept_from || stdin > self.stdin_written || signals > self.signals.len() as u64 {
            return None;
        }
        let missing: Vec<u8> = self.stdin.range((stdin - kept_from) as usize..).copied().collect();
        let mut frames: Vec<Frame> =
            missing.chunks(CHUNK).map(|c| Frame::Stdin(c.to_vec())).collect();
        if self.closed {
            frames.push(Frame::Stdin(Vec::new()));
        }
        frames.extend(self.signals[signals as usize..].iter().map(|&s| Frame::Signal(s)));
        Some(frames)
    }
}

/// The connection a [`Process`] uses now, and what it has read.
struct Current {
    /// None while a reset connection is being replaced (or after that
    /// failed).
    link: Option<Arc<dyn Link>>,
    /// Counts the connections: frames read from an older one, which a new
    /// one sends again, are dropped.
    generation: u64,
    /// Bytes of output taken, by stream: where a new connection resumes.
    stdout: u64,
    stderr: u64,
    exit: Option<ExitStatus>,
    /// Taking up the command again failed: every call fails so.
    failed: Option<Failed>,
}

/// Why a [`Process`] can no longer reach its command.
#[derive(Clone)]
enum Failed {
    Gone(String),
    Reset(String),
}

impl Failed {
    fn of(e: &Error) -> Self {
        match e {
            Error::GuestGone(m) => Failed::Gone(m.clone()),
            Error::ConnectionReset(m) => Failed::Reset(m.clone()),
            other => Failed::Reset(other.to_string()),
        }
    }

    fn error(&self) -> Error {
        match self.clone() {
            Failed::Gone(m) => Error::GuestGone(m),
            Failed::Reset(m) => Error::ConnectionReset(m),
        }
    }
}

/// A command running in the guest. Its methods take `&self`, so one thread
/// can feed its stdin while another takes its output. A reset of its
/// connection is met by taking up the command again on a new one (see the
/// [module docs](self)). Dropping it releases the command: the agent kills
/// it if it still runs.
pub struct Process {
    agent: Agent,
    id: u64,
    instance: u64,
    pid: u32,
    what: String,
    /// Held while reading a frame.
    reading: Mutex<()>,
    /// Held while writing a frame, and while taking up the command again.
    writing: Mutex<Writing>,
    current: Mutex<Current>,
    /// Signalled when `current`'s link goes, is replaced, or replacing it
    /// failed: the reader waits for that, never for the writing lock, which
    /// a writer holds while it waits for a replay that waits for the reader
    /// (board 13193).
    relinked: Condvar,
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process")
            .field("pid", &self.pid)
            .field("what", &self.what)
            .finish_non_exhaustive()
    }
}

/// What a read or write on the current link found.
enum Step<T> {
    Done(T),
    /// A frame of a connection given up on, which the new one sends again.
    Stale,
    /// The link of `generation` was reset: take the command up again.
    Reset(u64, io::Error),
}

impl Process {
    /// Its process ID in the guest (also its process group's).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn lost(&self, e: io::Error) -> Error {
        if is_gone(&e) {
            gone(&self.what, &e)
        } else if e.kind() == io::ErrorKind::TimedOut {
            Error::TimedOut(format!("{}: {e}", self.what))
        } else {
            Error::Agent(format!("{}: {e}", self.what))
        }
    }

    /// The current link, or why there is none. With the writing lock held
    /// there is always one unless taking up the command failed.
    fn link(&self) -> Result<Option<(Arc<dyn Link>, u64)>> {
        let c = lock(&self.current);
        if let Some(failed) = &c.failed {
            return Err(failed.error());
        }
        Ok(c.link.clone().map(|l| (l, c.generation)))
    }

    /// Replaces the link of `generation`, which was reset, with a new
    /// connection that takes up the command where this side is, and sends
    /// again what the agent had not taken. Holds the writing lock.
    fn reconnect(&self, w: &mut Writing, generation: u64, cause: io::Error) -> Result<()> {
        let (stdout, stderr) = {
            let mut c = lock(&self.current);
            if let Some(failed) = &c.failed {
                return Err(failed.error());
            }
            if c.generation != generation {
                // Another thread did it.
                return Ok(());
            }
            if is_gone(&cause) {
                let e = gone(&self.what, &cause);
                c.failed = Some(Failed::of(&e));
                c.link = None;
                return Err(e);
            }
            // From here, what the old link still gives is dropped: the new
            // one sends it again from these offsets.
            c.generation += 1;
            c.link = None;
            (c.stdout, c.stderr)
        };
        self.relinked.notify_all();
        let what = &self.what;
        let attached =
            self.agent.retrying(what, self.agent.reconnect_budget(), || {
                let s = self.agent.open(what)?;
                if s.instance != self.instance {
                    return Err(Failure::Fatal(Error::ConnectionReset(format!(
                        "{what}: the agent restarted, and with it the command's result is lost"
                    ))));
                }
                s.send(what, &Frame::Attach { id: self.id, stdout, stderr })?;
                let replay = match s.recv(what)? {
                Frame::Attached { stdin, signals } => w.replay(stdin, signals).ok_or_else(|| {
                    Failure::Fatal(Error::ConnectionReset(format!(
                        "{what}: the agent took {stdin} bytes of stdin and {signals} signals; \
                         this side wrote {} and sent {}, keeping the last {} bytes",
                        w.stdin_written,
                        w.signals.len(),
                        w.stdin.len()
                    )))
                })?,
                Frame::Error { errno, message } if errno == ESRCH || errno == ENOBUFS => {
                    return Err(Failure::Fatal(Error::ConnectionReset(format!(
                        "{what}: {message}"
                    ))));
                }
                Frame::Error { errno, message } => return Err(guest_error(what, errno, message)),
                other => return Err(unexpected(what, &other)),
            };
                Ok((s.link, replay))
            });
        let mut c = lock(&self.current);
        match attached {
            Ok((link, replay)) => {
                // The new connection first, so that its output is read while
                // the replay goes out (see Replay).
                let link: Arc<dyn Link> = Arc::from(link);
                c.link = Some(link.clone());
                drop(c);
                self.relinked.notify_all();
                w.replaying = Replay::start(link, replay);
                Ok(())
            }
            Err(gave_up) => {
                let e = match gave_up {
                    GaveUp::Failed(e) => e,
                    GaveUp::NoAnswer(e) => Error::ConnectionReset(format!(
                        "{what}: {cause}, and taking it up again failed: {e}"
                    )),
                };
                c.failed = Some(Failed::of(&e));
                drop(c);
                self.relinked.notify_all();
                Err(e)
            }
        }
    }

    /// The reader found the link of `generation` reset: takes the command
    /// up again, unless another thread has or is doing so. That is a writer
    /// whose send the reset failed too: it holds the writing lock while it
    /// waits for its replay to go out, and the replay may wait for this
    /// reader to read the command's output (board 13193).
    fn reset_seen(&self, generation: u64, cause: io::Error) -> Result<()> {
        loop {
            {
                let c = lock(&self.current);
                if let Some(failed) = &c.failed {
                    return Err(failed.error());
                }
                if c.generation != generation {
                    return Ok(());
                }
            }
            match self.writing.try_lock() {
                Ok(mut w) => return self.reconnect(&mut w, generation, cause),
                Err(TryLockError::Poisoned(p)) => {
                    return self.reconnect(&mut p.into_inner(), generation, cause);
                }
                Err(TryLockError::WouldBlock) => {
                    let c = lock(&self.current);
                    if c.generation == generation && c.failed.is_none() {
                        drop(self.relinked.wait_timeout(c, RELINK_POLL));
                    }
                }
            }
        }
    }

    /// Waits a little for the link another thread is replacing (or for
    /// that to fail), without the writing lock it holds.
    fn await_relink(&self) {
        let c = lock(&self.current);
        if c.link.is_none() && c.failed.is_none() {
            drop(self.relinked.wait_timeout(c, RELINK_POLL));
        }
    }

    /// Writes `frame`, which `w` records, taking up the command again if
    /// the connection was reset (which sends what was recorded).
    fn send(&self, w: &mut Writing, frame: &Frame) -> Result<()> {
        w.after_replay();
        let Some((link, generation)) = self.link()? else {
            return Err(Error::ConnectionReset(format!("{}: no connection", self.what)));
        };
        match write_frame(Io(&*link), frame) {
            Ok(()) => Ok(()),
            Err(e) if is_reset(&e) || is_gone(&e) => self.reconnect(w, generation, e),
            Err(e) => Err(self.lost(e)),
        }
    }

    /// Feeds `data` to its stdin; waits while the guest has no room for it.
    pub fn write_stdin(&self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(CHUNK) {
            let mut w = lock(&self.writing);
            w.record_stdin(chunk);
            self.send(&mut w, &Frame::Stdin(chunk.to_vec()))?;
        }
        Ok(())
    }

    /// Closes its stdin (once what was written has reached it).
    pub fn close_stdin(&self) -> Result<()> {
        let mut w = lock(&self.writing);
        w.closed = true;
        self.send(&mut w, &Frame::Stdin(Vec::new()))
    }

    /// Sends `signal` to its process group (once, even across resets).
    pub fn signal(&self, signal: i32) -> Result<()> {
        let mut w = lock(&self.writing);
        w.signals.push(signal);
        self.send(&mut w, &Frame::Signal(signal))
    }

    /// Kills its process group (SIGKILL).
    pub fn kill(&self) -> Result<()> {
        self.signal(9)
    }

    /// Reads one frame of the current connection.
    fn read_one(&self) -> Result<Step<Option<Event>>> {
        let (link, generation) = loop {
            {
                let c = lock(&self.current);
                if c.exit.is_some() {
                    return Ok(Step::Done(None));
                }
            }
            match self.link()? {
                Some(current) => break current,
                // Being replaced: wait for that to end.
                None => self.await_relink(),
            }
        };
        let frame = match read_frame(Io(&*link)) {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(Step::Reset(generation, io::ErrorKind::UnexpectedEof.into())),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Err(Error::Agent(format!("{}: {e}", self.what)));
            }
            Err(e) if is_reset(&e) || is_gone(&e) => return Ok(Step::Reset(generation, e)),
            Err(e) => return Err(self.lost(e)),
        };
        let mut c = lock(&self.current);
        if c.generation != generation {
            return Ok(Step::Stale);
        }
        Ok(Step::Done(Some(match frame {
            Frame::Stdout(d) => {
                c.stdout += d.len() as u64;
                Event::Stdout(d)
            }
            Frame::Stderr(d) => {
                c.stderr += d.len() as u64;
                Event::Stderr(d)
            }
            Frame::Exit(e) => {
                let status = ExitStatus::from(e);
                c.exit = Some(status);
                Event::Exit(status)
            }
            other => {
                return Err(Error::Agent(format!(
                    "{}: the agent sent {} unexpectedly",
                    self.what,
                    other.name()
                )));
            }
        })))
    }

    /// What it does next; `None` once its exit has been returned.
    pub fn next_event(&self) -> Result<Option<Event>> {
        let _reading = lock(&self.reading);
        loop {
            match self.read_one()? {
                Step::Done(event) => return Ok(event),
                Step::Stale => {}
                Step::Reset(generation, cause) => self.reset_seen(generation, cause)?,
            }
        }
    }

    /// Waits for it to end, collecting the output not taken yet.
    pub fn wait(&self) -> Result<Output> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            match self.next_event()? {
                Some(Event::Stdout(d)) => stdout.extend(d),
                Some(Event::Stderr(d)) => stderr.extend(d),
                Some(Event::Exit(status)) => {
                    return Ok(Output { status, stdout, stderr });
                }
                None => {
                    let status = lock(&self.current).exit.expect("None comes after the exit");
                    return Ok(Output { status, stdout, stderr });
                }
            }
        }
    }

    /// Tells the agent the command is done with: it kills it if it runs,
    /// and forgets it. A command still running is taken up again to tell it
    /// if its connection was reset; a finished one the agent forgets in time
    /// anyway.
    fn release(&self) -> Result<()> {
        let mut w = lock(&self.writing);
        w.after_replay();
        let finished = lock(&self.current).exit.is_some();
        let Some((link, generation)) = self.link()? else {
            return Ok(());
        };
        // A short wait for room: the agent reads its connection all along.
        let _ = link.set_timeout(Some(Duration::from_secs(1)));
        match write_frame(Io(&*link), &Frame::Release) {
            Ok(()) => Ok(()),
            Err(e) if is_reset(&e) && !finished => {
                self.reconnect(&mut w, generation, e)?;
                if let Some((link, _)) = self.link()? {
                    let _ = link.set_timeout(Some(Duration::from_secs(1)));
                    write_frame(Io(&*link), &Frame::Release).map_err(|e| self.lost(e))?;
                }
                Ok(())
            }
            Err(e) => Err(self.lost(e)),
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// What the fake agent does with one connection.
    #[derive(Clone)]
    enum Plan {
        /// The connect fails.
        Refuse(fn() -> io::Error),
        /// The connection reads `bytes` (the agent's side), then fails with
        /// `then` if given, else ends; writes fail (a reset) after
        /// `writable` bytes if given.
        Talk { bytes: Vec<u8>, then: Option<fn() -> io::Error>, writable: Option<usize> },
        /// The connection reads `bytes`, then ends; a write past the first
        /// `free` bytes waits until the client has read `read` bytes (at
        /// most 2 s, then fails with a reset): a guest that takes stdin
        /// only once its command's output was read.
        Gated { bytes: Vec<u8>, free: usize, read: usize },
    }

    fn reset() -> io::Error {
        io::ErrorKind::ConnectionReset.into()
    }

    fn refused() -> io::Error {
        io::ErrorKind::ConnectionRefused.into()
    }

    /// The agent says `frames` after its preamble and hello, then the
    /// connection ends.
    fn talk(frames: &[Frame]) -> Plan {
        Plan::Talk { bytes: says(frames), then: None, writable: None }
    }

    /// The same, then the connection is reset.
    fn talk_then_reset(frames: &[Frame]) -> Plan {
        Plan::Talk { bytes: says(frames), then: Some(reset), writable: None }
    }

    const INSTANCE: u64 = 0x1157;

    fn says(frames: &[Frame]) -> Vec<u8> {
        says_as(INSTANCE, frames)
    }

    fn says_as(instance: u64, frames: &[Frame]) -> Vec<u8> {
        says_from(instance, 0, frames)
    }

    /// What an agent that last reseeded for `generation` says.
    fn says_from(instance: u64, generation: u64, frames: &[Frame]) -> Vec<u8> {
        let mut b = preamble().to_vec();
        b.extend(Frame::Hello { instance, generation }.encode());
        for f in frames {
            b.extend(f.encode());
        }
        b
    }

    struct FakeLink {
        bytes: Vec<u8>,
        pos: Mutex<usize>,
        then: Option<fn() -> io::Error>,
        writable: Option<usize>,
        written: Arc<Mutex<Vec<u8>>>,
        /// Writes past `.0` bytes wait until `.1` bytes were read.
        gate: Option<(usize, usize)>,
        was_read: Condvar,
    }

    impl Link for FakeLink {
        fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut pos = self.pos.lock().unwrap();
            if *pos == self.bytes.len() {
                return match self.then {
                    Some(error) => Err(error()),
                    None => Ok(0),
                };
            }
            let n = buf.len().min(self.bytes.len() - *pos);
            buf[..n].copy_from_slice(&self.bytes[*pos..*pos + n]);
            *pos += n;
            self.was_read.notify_all();
            Ok(n)
        }

        fn write(&self, buf: &[u8]) -> io::Result<usize> {
            if let Some((free, read)) = self.gate
                && self.written.lock().unwrap().len() >= free
            {
                let pos = self.pos.lock().unwrap();
                let (pos, _) = self
                    .was_read
                    .wait_timeout_while(pos, Duration::from_secs(2), |p| *p < read)
                    .unwrap();
                if *pos < read {
                    return Err(reset());
                }
            }
            let mut written = self.written.lock().unwrap();
            if self.writable.is_some_and(|w| written.len() + buf.len() > w) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn set_timeout(&self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeDial {
        /// The machine's restore generation.
        generation: u64,
        plans: Mutex<VecDeque<Plan>>,
        dials: AtomicUsize,
        /// What the client wrote on each connection.
        written: Mutex<Vec<Arc<Mutex<Vec<u8>>>>>,
    }

    impl Dial for FakeDial {
        fn dial(&self, port: u32, _: Duration) -> io::Result<Box<dyn Link>> {
            assert_eq!(port, DEFAULT_PORT);
            self.dials.fetch_add(1, Ordering::SeqCst);
            let plan = self.plans.lock().unwrap().pop_front();
            match plan.expect("no more connections planned") {
                Plan::Refuse(error) => Err(error()),
                Plan::Talk { bytes, then, writable } => {
                    let written = Arc::new(Mutex::new(Vec::new()));
                    self.written.lock().unwrap().push(written.clone());
                    Ok(Box::new(FakeLink {
                        bytes,
                        pos: Mutex::new(0),
                        then,
                        writable,
                        written,
                        gate: None,
                        was_read: Condvar::new(),
                    }))
                }
                Plan::Gated { bytes, free, read } => {
                    let written = Arc::new(Mutex::new(Vec::new()));
                    self.written.lock().unwrap().push(written.clone());
                    Ok(Box::new(FakeLink {
                        bytes,
                        pos: Mutex::new(0),
                        then: None,
                        writable: None,
                        written,
                        gate: Some((free, read)),
                        was_read: Condvar::new(),
                    }))
                }
            }
        }

        fn generation(&self) -> u64 {
            self.generation
        }
    }

    impl FakeDial {
        /// The frames the client sent on connection `i`, after its preamble.
        fn sent(&self, i: usize) -> Vec<Frame> {
            let written = self.written.lock().unwrap()[i].lock().unwrap().clone();
            assert_eq!(&written[..8], &preamble(), "the client's preamble");
            let mut r = &written[8..];
            let mut frames = Vec::new();
            while let Some(f) = read_frame(&mut r).unwrap() {
                frames.push(f);
            }
            frames
        }

        fn dials(&self) -> usize {
            self.dials.load(Ordering::SeqCst)
        }
    }

    fn agent(plans: Vec<Plan>) -> (Agent, Arc<FakeDial>) {
        let dial = Arc::new(FakeDial { plans: Mutex::new(plans.into()), ..Default::default() });
        (Agent::with_dial(dial.clone()), dial)
    }

    fn exit(code: i32) -> Frame {
        Frame::Exit(Exit::Code(code))
    }

    fn exec_id(frame: &Frame) -> u64 {
        match frame {
            Frame::Exec { id, .. } => *id,
            other => panic!("not an exec: {other:?}"),
        }
    }

    fn talk_from(generation: u64, frames: &[Frame]) -> Plan {
        Plan::Talk { bytes: says_from(INSTANCE, generation, frames), then: None, writable: None }
    }

    fn restored_agent(generation: u64, plans: Vec<Plan>) -> (Agent, Arc<FakeDial>) {
        let dial = Arc::new(FakeDial {
            generation,
            plans: Mutex::new(plans.into()),
            ..Default::default()
        });
        (Agent::with_dial(dial.clone()), dial)
    }

    /// A machine restored from a snapshot has its agent reseed the guest's
    /// RNG, with fresh host entropy, before any request (here a command):
    /// once per generation, each with entropy of its own.
    #[test]
    fn a_restored_machine_reseeds_before_anything_else() {
        let (a, dial) = restored_agent(
            0x5eed,
            vec![
                talk_from(0, &[Frame::Done]),
                talk_from(0x5eed, &[Frame::Started { pid: 7 }, exit(0)]),
                talk_from(0x5eed, &[Frame::Pong]),
            ],
        );
        let p = a.spawn(&Command::new("true")).unwrap();
        assert_eq!(p.pid(), 7);
        drop(p);
        a.ping().unwrap();
        let reseeds: Vec<Frame> = dial.sent(0);
        let [Frame::Reseed { generation, entropy }] = &reseeds[..] else {
            panic!("expected one reseed first, got {reseeds:?}");
        };
        assert_eq!(*generation, 0x5eed);
        assert_eq!(entropy.len(), RESEED_BYTES);
        assert!(matches!(dial.sent(1)[0], Frame::Exec { .. }));
        assert_eq!(dial.sent(2), vec![Frame::Ping], "no second reseed");

        // Another clone's reseed carries other entropy.
        let (b, other) = restored_agent(
            0x5eed,
            vec![talk_from(0, &[Frame::Done]), talk_from(0x5eed, &[Frame::Pong])],
        );
        b.ping().unwrap();
        assert_ne!(other.sent(0), reseeds, "fresh entropy each time");
    }

    /// A guest that cannot reseed gets nothing run: the request fails for
    /// good, never sent.
    #[test]
    fn an_agent_that_cannot_reseed_gets_no_request() {
        let refusal = Frame::Error { errno: 1, message: "EPERM".into() };
        let (a, dial) = restored_agent(9, vec![talk_from(0, &[refusal])]);
        let err = a.exec(&Command::new("true")).unwrap_err();
        assert!(matches!(&err, Error::Agent(m) if m.contains("reseed")), "{err}");
        assert_eq!(dial.dials(), 1);
        // An agent still behind after its reseed is refused too.
        let (a, _) =
            restored_agent(9, vec![talk_from(0, &[Frame::Done]), talk_from(0, &[Frame::Pong])]);
        assert!(matches!(a.ping(), Err(Error::Agent(_))));
    }

    #[test]
    fn refused_connects_are_tried_again() {
        let (a, dial) =
            agent(vec![Plan::Refuse(refused), Plan::Refuse(reset), talk(&[Frame::Pong])]);
        a.ping().unwrap();
        assert_eq!(dial.dials(), 3);
        assert_eq!(dial.sent(0), vec![Frame::Ping]);
    }

    #[test]
    fn a_read_reset_inside_the_answer_is_read_again() {
        let answer = [
            Frame::Data(b"first ".to_vec()),
            Frame::Data(b"second".to_vec()),
            Frame::Data(Vec::new()),
        ];
        let whole = says(&answer);
        let (a, dial) = agent(vec![
            Plan::Talk {
                bytes: whole[..whole.len() - 3].to_vec(),
                then: Some(reset),
                writable: None,
            },
            // A connection that ends (EOF) inside the agent's hello.
            Plan::Talk { bytes: whole[..12].to_vec(), then: None, writable: None },
            talk(&answer),
        ]);
        assert_eq!(a.read_file("/f").unwrap(), b"first second");
        assert_eq!(dial.dials(), 3);
        assert!(dial.sent(1).is_empty(), "no request before the hello");
    }

    #[test]
    fn writes_are_made_again_whole() {
        let (a, dial) = agent(vec![talk_then_reset(&[]), talk(&[Frame::Done])]);
        let data = vec![5u8; CHUNK + 10];
        a.write_file("/f", &data, 0o640).unwrap();
        for i in 0..2 {
            let sent = dial.sent(i);
            assert_eq!(sent[0], Frame::WriteFile { path: b"/f".to_vec(), mode: 0o640 });
            assert_eq!(sent.len(), 4, "request, two chunks, the end");
            assert_eq!(sent[3], Frame::Data(Vec::new()));
        }
    }

    /// The request went out and the connection was reset: it is sent
    /// again with the same ID, which the agent runs once.
    #[test]
    fn a_command_is_sent_again_with_its_id_after_a_reset() {
        let (a, dial) = agent(vec![
            talk_then_reset(&[]),
            talk_then_reset(&[Frame::Started { pid: 7 }]),
            // Taken up again from where it was.
            talk(&[
                Frame::Attached { stdin: 0, signals: 0 },
                Frame::Stdout(b"out".to_vec()),
                exit(0),
            ]),
        ]);
        let out = a.exec(&Command::new("true")).unwrap();
        assert_eq!(out.stdout, b"out");
        assert!(out.status.success());
        let id = exec_id(&dial.sent(0)[0]);
        assert_eq!(dial.sent(0).len(), 1);
        assert_eq!(exec_id(&dial.sent(1)[0]), id, "the same request");
        assert_eq!(dial.sent(2), [Frame::Attach { id, stdout: 0, stderr: 0 }, Frame::Release]);
        // Every command has its own ID.
        let (b, dial) = agent(vec![talk(&[Frame::Started { pid: 1 }, exit(0)]); 2]);
        b.exec(&Command::new("true")).unwrap();
        b.exec(&Command::new("true")).unwrap();
        assert_ne!(exec_id(&dial.sent(0)[0]), exec_id(&dial.sent(1)[0]));
    }

    #[test]
    fn a_command_is_sent_again_if_reset_before_its_request() {
        // Reset inside the agent's preamble: the client had not sent the
        // request yet.
        let (a, dial) = agent(vec![
            Plan::Talk { bytes: preamble()[..4].to_vec(), then: Some(reset), writable: None },
            talk(&[
                Frame::Started { pid: 7 },
                Frame::Stdout(b"out".to_vec()),
                Frame::Stderr(b"err".to_vec()),
                exit(3),
            ]),
        ]);
        let out = a.exec(Command::new("sh").args(["-c", "x"])).unwrap();
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");
        assert_eq!(out.status, ExitStatus { code: Some(3), signal: None });
        assert!(!out.status.success());
        assert!(dial.sent(0).is_empty(), "nothing but the preamble");
        let sent = dial.sent(1);
        let Frame::Exec { request: req, .. } = &sent[0] else { panic!() };
        assert_eq!(req.program, b"sh");
        assert_eq!(req.args, [b"-c".to_vec(), b"x".to_vec()]);
        assert_eq!(req.env, [(b"PATH".to_vec(), DEFAULT_PATH.as_bytes().to_vec())]);
        assert!(!req.stdin);
        assert_eq!(sent[1..], [Frame::Release], "released once done");
    }

    /// An agent that restarted forgot what it ran: a request that may have
    /// reached the one before is not sent to it, nor is a process taken up.
    #[test]
    fn an_agent_that_restarted_is_not_sent_a_command_again() {
        let (a, dial) = agent(vec![
            talk_then_reset(&[]),
            Plan::Talk {
                bytes: says_as(INSTANCE + 1, &[Frame::Started { pid: 1 }]),
                then: None,
                writable: None,
            },
        ]);
        let err = a.exec(&Command::new("true")).unwrap_err();
        assert!(matches!(&err, Error::ConnectionReset(m) if m.contains("restarted")), "{err}");
        assert!(dial.sent(1).is_empty(), "not sent to the new agent");

        // Before its request went out, any agent will do.
        let (a, _) = agent(vec![
            Plan::Talk { bytes: says_as(1, &[]), then: Some(refused), writable: Some(8) },
            Plan::Talk {
                bytes: says_as(2, &[Frame::Started { pid: 1 }, exit(0)]),
                then: None,
                writable: None,
            },
        ]);
        a.exec(&Command::new("true")).unwrap();

        // A process whose agent restarted.
        let (a, dial) = agent(vec![
            talk_then_reset(&[Frame::Started { pid: 1 }]),
            Plan::Talk { bytes: says_as(INSTANCE + 1, &[]), then: None, writable: None },
        ]);
        let p = a.spawn(&Command::new("cat")).unwrap();
        let err = p.wait().unwrap_err();
        assert!(matches!(&err, Error::ConnectionReset(m) if m.contains("restarted")), "{err}");
        assert!(dial.sent(1).is_empty());
        // Every call fails so from then on, without connecting again.
        assert!(matches!(p.write_stdin(b"x"), Err(Error::ConnectionReset(_))));
        assert!(matches!(p.next_event(), Err(Error::ConnectionReset(_))));
        drop(p);
        assert_eq!(dial.dials(), 2);
    }

    #[test]
    fn a_command_the_agent_forgot_is_not_run_again() {
        let forgot = Frame::Error {
            errno: ESRCH,
            message: "command 0x1 ended and its result was forgotten".into(),
        };
        let (a, _) = agent(vec![talk_then_reset(&[]), talk(std::slice::from_ref(&forgot))]);
        let err = a.exec(&Command::new("true")).unwrap_err();
        assert!(matches!(&err, Error::ConnectionReset(m) if m.contains("forgotten")), "{err}");
        // Attaching again: forgotten, or output no longer kept.
        for answer in [forgot, Frame::Error { errno: ENOBUFS, message: "no output kept".into() }] {
            let (a, _) =
                agent(vec![talk_then_reset(&[Frame::Started { pid: 1 }]), talk(&[answer])]);
            let p = a.spawn(&Command::new("cat")).unwrap();
            assert!(matches!(p.wait(), Err(Error::ConnectionReset(_))));
        }
    }

    /// A reset in the middle of the output: the client takes the command up
    /// again from what it has.
    #[test]
    fn a_process_takes_up_its_command_again_from_its_output_offsets() {
        let (a, dial) = agent(vec![
            talk_then_reset(&[
                Frame::Started { pid: 1 },
                Frame::Stdout(b"ab".to_vec()),
                Frame::Stderr(b"E".to_vec()),
                Frame::Stdout(b"c".to_vec()),
            ]),
            // Refused once (the guest is taking a transport reset).
            Plan::Refuse(refused),
            talk_then_reset(&[
                Frame::Attached { stdin: 0, signals: 0 },
                Frame::Stdout(b"d".to_vec()),
            ]),
            talk(&[
                Frame::Attached { stdin: 0, signals: 0 },
                Frame::Stderr(b"F".to_vec()),
                exit(0),
            ]),
        ]);
        let p = a.spawn(&Command::new("sh")).unwrap();
        let out = p.wait().unwrap();
        assert_eq!((&out.stdout[..], &out.stderr[..]), (&b"abcd"[..], &b"EF"[..]));
        let id = exec_id(&dial.sent(0)[0]);
        let attach = |stdout, stderr| Frame::Attach { id, stdout, stderr };
        assert_eq!(dial.sent(1), [attach(3, 1)]);
        assert_eq!(dial.sent(2), [attach(4, 1)]);
        drop(p);
        assert_eq!(dial.sent(2).last(), Some(&Frame::Release));
    }

    /// Stdin written and signals sent that the agent had not taken when
    /// the connection was reset are sent again, each once.
    #[test]
    fn stdin_and_signals_are_sent_again_from_what_the_agent_took() {
        let started = says(&[Frame::Started { pid: 1 }]);
        // What the client writes on the first connection before stdin: its
        // preamble and the exec.
        let exec =
            8 + Frame::Exec { id: 0, request: Command::new("cat").request(true) }.encode().len();
        let (a, dial) = agent(Vec::new());
        let first = exec + Frame::Stdin(b"ab".to_vec()).encode().len();
        dial.plans.lock().unwrap().extend([
            // Room for the exec and the first write only.
            Plan::Talk { bytes: started, then: None, writable: Some(first) },
            // The agent took "a" of "ab".
            talk(&[Frame::Attached { stdin: 1, signals: 0 }]),
        ]);
        let p = a.spawn(&Command::new("cat")).unwrap();
        p.write_stdin(b"ab").unwrap();
        p.write_stdin(b"cd").unwrap();
        p.signal(15).unwrap();
        p.close_stdin().unwrap();
        let id = exec_id(&dial.sent(0)[0]);
        assert_eq!(dial.sent(0)[1..], [Frame::Stdin(b"ab".to_vec())]);
        assert_eq!(
            dial.sent(1),
            [
                Frame::Attach { id, stdout: 0, stderr: 0 },
                Frame::Stdin(b"bcd".to_vec()),
                Frame::Signal(15),
                Frame::Stdin(Vec::new()),
            ]
        );
    }

    /// A reconnect's replay goes out while the process's output is read:
    /// a guest whose command waits for its output to be read before it
    /// reads more stdin takes the replay only then. Before, the replay was
    /// sent with the reader parked, and could not finish: under board 13131's
    /// reset storms, a process's reconnects failed for their whole budget
    /// (each ended by the next reset), and with no reset to end it the
    /// replay would have waited for good.
    #[test]
    fn a_replay_goes_out_while_the_output_is_read() {
        // The client's preamble and its Attach, then the replay waits for
        // the output to have been read.
        let attach = 8 + Frame::Attach { id: 0, stdout: 0, stderr: 0 }.encode().len();
        let attached =
            says(&[Frame::Attached { stdin: 0, signals: 0 }, Frame::Stdout(b"out".to_vec())]);
        let read = attached.len();
        let mut bytes = attached;
        bytes.extend(exit(0).encode());
        let (a, dial) = agent(vec![
            Plan::Talk {
                bytes: says(&[Frame::Started { pid: 1 }]),
                then: Some(reset),
                writable: Some(usize::MAX),
            },
            Plan::Gated { bytes, free: attach, read },
        ]);
        let p = a.spawn(&Command::new("cat")).unwrap();
        p.write_stdin(b"in").unwrap();
        // The reader meets the reset, takes the command up again and must
        // read the output while "in" is sent again.
        let out = p.wait().unwrap();
        assert_eq!(out.stdout, b"out");
        p.close_stdin().unwrap();
        let id = exec_id(&dial.sent(0)[0]);
        assert_eq!(
            dial.sent(1),
            [
                Frame::Attach { id, stdout: 0, stderr: 0 },
                Frame::Stdin(b"in".to_vec()),
                Frame::Stdin(Vec::new()),
            ]
        );
        assert_eq!(dial.dials(), 2, "one reconnect");
    }

    #[test]
    fn what_is_sent_again() {
        let mut w = Writing::default();
        assert_eq!(w.replay(0, 0), Some(vec![]));
        w.record_stdin(b"hello");
        w.signals.push(2);
        w.signals.push(15);
        assert_eq!(w.replay(2, 1), Some(vec![Frame::Stdin(b"llo".to_vec()), Frame::Signal(15)]));
        w.closed = true;
        assert_eq!(w.replay(5, 2), Some(vec![Frame::Stdin(Vec::new())]));
        assert_eq!(w.replay(6, 0), None, "more stdin than was written");
        assert_eq!(w.replay(0, 3), None, "more signals than were sent");
        // Only the last STDIN_KEPT bytes are kept.
        let mut w = Writing::default();
        for _ in 0..3 {
            w.record_stdin(&vec![1; STDIN_KEPT]);
        }
        let kept_from = 2 * STDIN_KEPT as u64;
        assert_eq!(w.replay(kept_from - 1, 0), None);
        let frames = w.replay(kept_from, 0).unwrap();
        assert_eq!(frames.len(), STDIN_KEPT / CHUNK);
        assert!(frames.iter().all(|f| *f == Frame::Stdin(vec![1; CHUNK])));
    }

    #[test]
    fn dropping_a_process_releases_its_command() {
        let (a, dial) = agent(vec![talk(&[Frame::Started { pid: 1 }])]);
        drop(a.spawn(&Command::new("sleep")).unwrap());
        assert_eq!(dial.sent(0)[1..], [Frame::Release]);
        // Its connection was reset: it is taken up again to release it.
        let (a, dial) = agent(Vec::new());
        let exec =
            8 + Frame::Exec { id: 0, request: Command::new("sleep").request(true) }.encode().len();
        dial.plans.lock().unwrap().extend([
            Plan::Talk {
                bytes: says(&[Frame::Started { pid: 1 }]),
                then: None,
                writable: Some(exec),
            },
            talk(&[Frame::Attached { stdin: 0, signals: 0 }]),
        ]);
        drop(a.spawn(&Command::new("sleep")).unwrap());
        assert!(matches!(dial.sent(1)[..], [Frame::Attach { .. }, Frame::Release]));
    }

    #[test]
    fn the_retry_budget_runs_out() {
        let plans = (0..1000).map(|_| Plan::Refuse(refused)).collect();
        let (a, dial) = agent(plans);
        let a = a.retry_for(Duration::from_millis(30));
        let start = Instant::now();
        let err = a.ping().unwrap_err();
        assert!(matches!(err, Error::Agent(ref m) if m.contains("no answer")), "{err}");
        assert!(start.elapsed() < Duration::from_secs(1));
        let dials = dial.dials();
        assert!((2..20).contains(&dials), "{dials} tries, backing off");
    }

    #[test]
    fn another_protocol_version_is_refused_without_retrying() {
        let mut bytes = preamble().to_vec();
        bytes[4] = 1;
        let (a, dial) = agent(vec![Plan::Talk { bytes, then: None, writable: None }]);
        let err = a.ping().unwrap_err();
        assert!(
            matches!(err, Error::Agent(ref m) if m.contains("version 1") && m.contains("version 3")),
            "{err}"
        );
        assert_eq!(dial.dials(), 1);
    }

    #[test]
    fn a_file_past_the_bound_is_refused_before_it_is_held() {
        let chunk = Frame::Data(vec![7; 8]);
        let (a, _) = agent(vec![talk(&[chunk.clone(), chunk, Frame::Data(Vec::new())])]);
        let a = a.max_file_size(15);
        let err = a.read_file("/big").unwrap_err();
        let Error::Guest { what, source } = &err else { panic!("{err}") };
        assert_eq!(source.raw_os_error(), Some(libc::EFBIG), "{err}");
        assert!(what.contains("/big"), "{what}");

        // At the bound exactly, it is read.
        let chunk = Frame::Data(vec![7; 8]);
        let (a, _) = agent(vec![talk(&[chunk.clone(), chunk, Frame::Data(Vec::new())])]);
        assert_eq!(a.max_file_size(16).read_file("/big").unwrap(), vec![7; 16]);
    }

    #[test]
    fn guest_errors_carry_the_errno() {
        let (a, _) = agent(vec![talk(&[Frame::Error {
            errno: 2,
            message: "/nope: No such file or directory".into(),
        }])]);
        let err = a.read_file("/nope").unwrap_err();
        let Error::Guest { what, source } = &err else { panic!("{err}") };
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
        assert!(what.contains("/nope"), "{what}");
    }

    #[test]
    fn an_early_refusal_of_a_write_is_its_answer() {
        // The agent refuses at once and closes: the client's writes fail,
        // but the refusal is there to read, and is not retried.
        let (a, dial) = agent(vec![Plan::Talk {
            bytes: says(&[Frame::Error { errno: 13, message: "/ro: Permission denied".into() }]),
            then: None,
            writable: Some(100),
        }]);
        let err = a.write_file("/ro", &[0; 1000], 0o644).unwrap_err();
        assert!(
            matches!(&err, Error::Guest { source, .. } if source.kind() == io::ErrorKind::PermissionDenied),
            "{err}"
        );
        assert_eq!(dial.dials(), 1);
    }

    fn stopped() -> io::Error {
        crate::devices::virtio::vsock::guest_stopped()
    }

    fn not_connected() -> io::Error {
        io::ErrorKind::NotConnected.into()
    }

    /// A guest that stopped, or a machine that is gone, fails every call at
    /// once with GuestGone: nothing is tried again.
    #[test]
    fn a_gone_guest_fails_at_once() {
        for gone in [stopped as fn() -> io::Error, not_connected] {
            let (a, dial) = agent(vec![Plan::Refuse(gone)]);
            assert!(matches!(a.ping(), Err(Error::GuestGone(_))));
            assert_eq!(dial.dials(), 1);
            // A request out when it stopped.
            let (a, dial) =
                agent(vec![Plan::Talk { bytes: says(&[]), then: Some(gone), writable: None }]);
            let err = a.exec(&Command::new("true")).unwrap_err();
            assert!(matches!(&err, Error::GuestGone(m) if m.contains("may have run")), "{err}");
            assert_eq!(dial.dials(), 1);
            // A process waiting when it stopped.
            let (a, dial) = agent(vec![Plan::Talk {
                bytes: says(&[Frame::Started { pid: 1 }, Frame::Stdout(b"x".to_vec())]),
                then: Some(gone),
                writable: None,
            }]);
            let p = a.spawn(&Command::new("cat")).unwrap();
            assert_eq!(p.next_event().unwrap(), Some(Event::Stdout(b"x".to_vec())));
            assert!(matches!(p.wait(), Err(Error::GuestGone(_))));
            assert!(matches!(p.wait(), Err(Error::GuestGone(_))), "and again");
            assert!(matches!(p.kill(), Err(Error::GuestGone(_))));
            drop(p);
            assert_eq!(dial.dials(), 1);
        }
    }

    /// wait_ready waits only while nobody answers: any other failure ends
    /// it at once (before, a gone machine or another protocol version made
    /// it spin until its timeout).
    #[test]
    fn wait_ready_returns_other_failures_at_once() {
        let mut v1 = preamble().to_vec();
        v1[4] = 1;
        type Expected = fn(&Error) -> bool;
        let cases: Vec<(Plan, Expected)> = vec![
            (Plan::Refuse(not_connected), |e| matches!(e, Error::GuestGone(_))),
            (Plan::Refuse(stopped), |e| matches!(e, Error::GuestGone(_))),
            (
                Plan::Talk { bytes: v1, then: None, writable: None },
                |e| matches!(e, Error::Agent(m) if m.contains("version")),
            ),
        ];
        for (plan, expected) in cases {
            let (a, dial) = agent(vec![plan]);
            let start = Instant::now();
            let err = a.wait_ready(Duration::from_secs(30)).unwrap_err();
            assert!(expected(&err), "{err}");
            assert_eq!(dial.dials(), 1);
            assert!(start.elapsed() < Duration::from_secs(1));
        }
        // While refused, it waits.
        let mut plans: Vec<Plan> = (0..5).map(|_| Plan::Refuse(refused)).collect();
        plans.push(talk(&[Frame::Pong]));
        let (a, dial) = agent(plans);
        a.wait_ready(Duration::from_secs(30)).unwrap();
        assert_eq!(dial.dials(), 6);
        // And it times out.
        let plans = (0..10_000).map(|_| Plan::Refuse(refused)).collect();
        let (a, _) = agent(plans);
        let a = a.retry_for(Duration::from_millis(10));
        let err = a.wait_ready(Duration::from_millis(50)).unwrap_err();
        assert!(matches!(err, Error::TimedOut(_)), "{err}");
    }

    #[test]
    fn process_events_in_order_then_none() {
        let (a, dial) = agent(vec![talk(&[
            Frame::Started { pid: 42 },
            Frame::Stdout(b"a".to_vec()),
            Frame::Exit(Exit::Signal(9)),
        ])]);
        let p = a.spawn(&Command::new("cat")).unwrap();
        assert_eq!(p.pid(), 42);
        p.write_stdin(b"in").unwrap();
        p.close_stdin().unwrap();
        p.kill().unwrap();
        assert_eq!(p.next_event().unwrap(), Some(Event::Stdout(b"a".to_vec())));
        let killed = ExitStatus { code: None, signal: Some(9) };
        assert_eq!(p.next_event().unwrap(), Some(Event::Exit(killed)));
        assert_eq!(p.next_event().unwrap(), None);
        assert_eq!(p.wait().unwrap().status, killed);
        let sent = dial.sent(0);
        assert!(matches!(&sent[0], Frame::Exec { request, .. } if request.stdin));
        assert_eq!(
            sent[1..],
            [Frame::Stdin(b"in".to_vec()), Frame::Stdin(Vec::new()), Frame::Signal(9)]
        );
    }

    #[test]
    fn command_environment() {
        let mut c = Command::new("env");
        c.env("A", "1").env("PATH", "/bin").env("A", "2");
        let r = c.request(false);
        assert_eq!(r.env, [(b"PATH".to_vec(), b"/bin".to_vec()), (b"A".to_vec(), b"2".to_vec())]);
        c.env_clear().current_dir("/tmp");
        let r = c.request(true);
        assert!(r.env.is_empty());
        assert_eq!(r.cwd.as_deref(), Some(&b"/tmp"[..]));
    }
}
