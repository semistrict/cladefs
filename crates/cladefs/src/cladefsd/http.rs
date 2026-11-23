// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's HTTP server, on the address `CLADEFSD_METRICS` names: its metrics, `GET /metrics`,
//! for whoever scrapes them (TASK-47, `super::metrics`); cladefs: and its z-pages, under `/z/`, for
//! whoever looks into it (TASK-672, `super::zpages`). A server of its own, small, as what it
//! answers is a few requests, each on a thread of its own, within deadlines that no client holds
//! past.
//!
//! cladefs: `CLADEFSD_ZPAGES` says where the z-pages are served (board 8559): unset, with the
//! metrics, on `CLADEFSD_METRICS`' address; `off`, nowhere; an address (`HOST:PORT`), there alone,
//! the metrics' address serving `/metrics` alone. Whoever may reach the z-pages may profile the
//! daemon's CPU and change what it logs, which whoever scrapes its metrics need not
//! (hotbed question 501).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a client has to send its request, and then to take the response: one deadline for
/// each half of the exchange, as a client sending a byte at a time, or taking nothing, would hold
/// its turn for good against a timeout of each read or write (TASK-79.27). cladefs: two, as what is
/// asked for may take long to make, as a CPU profile of the seconds asked for (TASK-672.3), which
/// waits for those seconds alone, and ends once its client hangs up ([`Client::hung_up`], board
/// 8559).
const EXCHANGE: Duration = Duration::from_secs(5);

/// The most bytes of a request's line and headers read: a scraper's are a line and a few headers.
/// More are refused, unread, as a line read whole would grow without end.
const HEAD_MAX: u64 = 8 << 10;

/// How many clients are served at once: those after wait to be accepted, holding nothing, so that
/// clients slow to send or take what they asked for hold up no scrape but past these.
const AT_ONCE: usize = 4;

/// The content type of Prometheus' text exposition format.
const METRICS: &str = "text/plain; version=0.0.4";

/// What a request is answered: its status line's code and reason, the type of what it carries, and
/// that.
pub(super) struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    /// `body`, of `content_type`, found.
    pub fn ok(content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self { status: "200 OK", content_type, body: body.into() }
    }

    /// `status`, saying `why` in plain text.
    pub fn plain(status: &'static str, why: impl Into<String>) -> Self {
        let mut body = why.into();
        body.push('\n');
        Self { status, content_type: "text/plain; charset=utf-8", body: body.into() }
    }

    /// Nothing at the path asked for.
    pub fn not_found() -> Self {
        Self::plain("404 Not Found", "not found")
    }
}

/// cladefs: what a server answers (board 8559): the metrics, the z-pages, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pages {
    /// `/metrics` and `/z/`, on one address: `CLADEFSD_ZPAGES` unset.
    All,
    /// `/metrics` alone: the z-pages served on an address of their own, or nowhere.
    Metrics,
    /// `/z/` alone, on `CLADEFSD_ZPAGES`' address.
    Zpages,
}

/// cladefs: where the z-pages are served, as `CLADEFSD_ZPAGES` says (board 8559).
#[cfg(not(madsim))]
enum Zpages {
    /// With the metrics: unset.
    WithMetrics,
    /// Nowhere: `off`.
    Off,
    /// At an address of their own.
    At(String),
}

/// cladefs: binds and serves the daemon's HTTP servers as its environment says, each on a thread
/// of its own (`CLADEFSD_METRICS`, `CLADEFSD_ZPAGES`): none, if it names no address; an address
/// it can't bind to, an error, the daemon refusing to start.
#[cfg(not(madsim))]
pub(super) fn start() -> anyhow::Result<()> {
    use anyhow::Context;
    let address = |name: &str| -> anyhow::Result<Option<String>> {
        let value = node_local::env::var_os(name).filter(|value| !value.is_empty());
        let value = value.map(|value| value.into_string());
        value.transpose().map_err(|_| anyhow::anyhow!("{name} must be an address"))
    };
    let zpages = match address("CLADEFSD_ZPAGES")?.as_deref() {
        None => Zpages::WithMetrics,
        Some("off") => Zpages::Off,
        Some(address) => Zpages::At(address.to_owned()),
    };
    let serve = |address: &str, pages: Pages| -> anyhow::Result<()> {
        let listener =
            TcpListener::bind(address).with_context(|| format!("serving HTTP at {address}"))?;
        // cladefs: outside the simulation: metrics and z-pages over HTTP (doc-24).
        super::run::thread("cladefsd http", move || {
            if let Err(err) = serve_http(listener, pages) {
                log::error!("cladefsd: serving HTTP: {err}");
            }
        })?;
        Ok(())
    };
    if let Some(metrics) = address("CLADEFSD_METRICS")? {
        let pages = match zpages {
            Zpages::WithMetrics => Pages::All,
            Zpages::Off | Zpages::At(_) => Pages::Metrics,
        };
        serve(&metrics, pages)?;
    }
    match zpages {
        Zpages::At(address) => serve(&address, Pages::Zpages)?,
        Zpages::Off => log::info!("cladefsd: z-pages off (CLADEFSD_ZPAGES=off)"),
        Zpages::WithMetrics => {}
    }
    Ok(())
}

/// Serves `pages` to whoever connects to `listener` and asks for them over HTTP, until it listens
/// no more (`EINVAL`): each client on a thread of its own, [`AT_ONCE`] at most, within
/// [`EXCHANGE`]. An accept that fails otherwise, as with as many files open as the daemon may, is
/// tried again after a while (`server::ACCEPT_AGAIN`), as the daemon's VM sockets' are: given up
/// on, the daemon's metrics would be served no more, the daemon serving on (TASK-79.35).
pub fn serve_http(listener: TcpListener, pages: Pages) -> std::io::Result<()> {
    let slots = Arc::new(Slots::default());
    let mut failing = false;
    loop {
        let slot = slots.take();
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => return Err(err),
            // That client's, gone before it was accepted.
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted => continue,
            Err(err) => {
                if !failing {
                    log::error!("cladefsd: accepting an HTTP client: {err}: trying again");
                    failing = true;
                }
                std::thread::sleep(super::server::ACCEPT_AGAIN);
                continue;
            }
        };
        failing = false;
        // cladefs: outside the simulation: metrics and z-pages over HTTP (doc-24).
        let spawned = std::thread::Builder::new().name("cladefsd http".into()).spawn(move || {
            // Given back as the thread ends, however it does: a page's panic unwinding through it
            // too, which held its slot for good before (board 8559).
            let _slot = slot;
            if let Err(err) = answer(stream, pages) {
                log::warn!("cladefsd: serving HTTP: {err}");
            }
        });
        // Its slot, in what the thread would have run, given back with it.
        if let Err(err) = spawned {
            log::warn!("cladefsd: serving HTTP: {err}");
        }
    }
}

/// cladefs: the clients being served, [`AT_ONCE`] at most, each holding a [`Slot`] (board 8559).
#[derive(Default)]
struct Slots {
    serving: Mutex<usize>,
    done: Condvar,
}

impl Slots {
    /// A slot, once one is free.
    fn take(self: &Arc<Self>) -> Slot {
        // A count, which no panic leaves half changed: poisoned, as it is.
        let mut serving = self.serving.lock().unwrap_or_else(PoisonError::into_inner);
        while *serving == AT_ONCE {
            serving = self.done.wait(serving).unwrap_or_else(PoisonError::into_inner);
        }
        *serving += 1;
        Slot(Arc::clone(self))
    }
}

/// cladefs: a client's turn among [`AT_ONCE`], given back as it drops.
struct Slot(Arc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        // As in `Slots::take`.
        *self.0.serving.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.0.done.notify_one();
    }
}

/// cladefs: the client of an exchange, as a page that takes long to make asks after it, so that
/// what it asked for is made no longer once it is gone (board 8559).
pub(super) struct Client<'a>(Option<&'a TcpStream>);

impl Client<'_> {
    /// A client that never hangs up: a test's, asking a page itself.
    #[cfg(test)]
    pub const NONE: Client<'static> = Client(None);

    /// Whether it hung up: its connection ended, or reset. Asked without waiting: what it sent
    /// past its request, which means nothing here, read and dropped, so that the end behind it is
    /// seen (a byte left there, peeked, hid it for good). A client that shuts down its
    /// connection's writing half, waiting to read, counts as gone, as no scraper, browser or `go
    /// tool pprof` does that.
    pub fn hung_up(&self) -> bool {
        let Some(stream) = self.0 else { return false };
        if let Err(err) = stream.set_nonblocking(true) {
            log::warn!("cladefsd: asking whether an HTTP client hung up: {err}");
            return true;
        }
        let gone = ended(stream);
        if let Err(err) = stream.set_nonblocking(false) {
            log::warn!("cladefsd: asking whether an HTTP client hung up: {err}");
            return true;
        }
        gone
    }
}

/// cladefs: whether `stream`, not blocking, has ended or reset: what it has to read read and
/// dropped, [`HUNG_UP_READ`] at most, as one sending on is there.
fn ended(mut stream: &TcpStream) -> bool {
    let mut dropped = [0; 4096];
    let mut read = 0;
    while read < HUNG_UP_READ {
        match stream.read(&mut dropped) {
            Ok(0) => return true,
            Ok(bytes) => read += bytes,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return err.kind() != std::io::ErrorKind::WouldBlock,
        }
    }
    false
}

/// cladefs: the most bytes [`Client::hung_up`] reads and drops of a client at once.
const HUNG_UP_READ: usize = 64 << 10;

/// What `method` asks of `target`, a path and its query, of `pages`: the metrics, at `/metrics`;
/// the z-pages, under `/z/`. None, if `client` hung up before it was made.
fn route(method: &str, target: &str, pages: Pages, client: &Client<'_>) -> Option<Response> {
    // cladefs: a page's panic, which the server's tests inject (board 8559).
    fail::fail_point!("cladefsd::http::route");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let zpage = path.strip_prefix("/z").filter(|_| pages != Pages::Metrics);
    match (method, path, zpage) {
        ("GET", "/metrics", _) if pages != Pages::Zpages => {
            Some(Response::ok(METRICS, super::metrics::render()))
        }
        (method, _, Some(page)) => super::zpages::serve(method, page, query, client),
        _ => Some(Response::not_found()),
    }
}

/// A client's connection within its exchange's deadline: each read and write waits at most for
/// what is left of it, and fails with `TimedOut` once nothing is.
struct Deadlined<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Deadlined<'_> {
    fn left(&self) -> std::io::Result<Duration> {
        match self.deadline.saturating_duration_since(Instant::now()) {
            Duration::ZERO => Err(std::io::ErrorKind::TimedOut.into()),
            left => Ok(left),
        }
    }
}

impl Read for Deadlined<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.left()?))?;
        (&*self.stream).read(bytes)
    }
}

impl Write for Deadlined<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.left()?))?;
        (&*self.stream).write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Answers the HTTP request `stream` carries of `pages` ([`route`]), its head read within
/// [`EXCHANGE`] and what it asked for sent within another; `431`, for a request whose line and
/// headers are more than [`HEAD_MAX`] bytes; nothing, to a client that hung up before what it asked
/// for was made.
fn answer(stream: TcpStream, pages: Pages) -> std::io::Result<()> {
    let deadline = Instant::now() + EXCHANGE;
    let mut reader = BufReader::new(Deadlined { stream: &stream, deadline }.take(HEAD_MAX));
    let mut request = String::new();
    let mut whole = read_line(&mut reader, &mut request)?;
    // The headers, which say nothing it needs.
    while whole {
        let mut line = String::new();
        whole = read_line(&mut reader, &mut line)?;
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let too_large = !whole && reader.get_ref().limit() == 0;
    let mut parts = request.split_whitespace();
    let response = match (parts.next(), parts.next()) {
        _ if too_large => {
            Some(Response::plain("431 Request Header Fields Too Large", "request too large"))
        }
        (Some(method), Some(target)) => route(method, target, pages, &Client(Some(&stream))),
        _ => Some(Response::plain("400 Bad Request", "no request line")),
    };
    let Some(Response { status, content_type, body }) = response else {
        log::info!("cladefsd: an HTTP client hung up before its answer was made");
        return Ok(());
    };
    let deadline = Instant::now() + EXCHANGE;
    let mut connection = Deadlined { stream: &stream, deadline };
    write!(
        connection,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    connection.write_all(&body)?;
    if too_large {
        // What it sent past its head is read and dropped, until it is done or its time is: the
        // connection closed with bytes unread would be reset, and the response lost with it.
        stream.shutdown(std::net::Shutdown::Write)?;
        std::io::copy(&mut Deadlined { stream: &stream, deadline }, &mut std::io::sink())?;
    }
    Ok(())
}

/// Reads a line into `line` as `read_line` does: whether it is whole, ended by a newline; not, at
/// the end of what `reader` reads.
fn read_line(reader: &mut impl BufRead, line: &mut String) -> std::io::Result<bool> {
    reader.read_line(line)?;
    Ok(line.ends_with('\n'))
}
