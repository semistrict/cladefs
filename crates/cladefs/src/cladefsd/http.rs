// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's HTTP server, on the address `CLADEFSD_METRICS` names: its metrics, `GET /metrics`,
//! for whoever scrapes them (TASK-47, `super::metrics`); cladefs: and its z-pages, under `/z/`, for
//! whoever looks into it (TASK-672, `super::zpages`). A server of its own, small, as what it
//! answers is a few GETs, each on a thread of its own, within deadlines that no client holds past.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// How long a client has to send its request, and then to take the response: one deadline for
/// each half of the exchange, as a client sending a byte at a time, or taking nothing, would hold
/// its turn for good against a timeout of each read or write (TASK-79.27). cladefs: two, as what is
/// asked for may take long to make, as a CPU profile of the seconds asked for (TASK-672.3).
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

/// Serves the metrics and z-pages to whoever connects to `listener` and asks for them over HTTP,
/// until it listens no more (`EINVAL`): each client on a thread of its own, [`AT_ONCE`] at most,
/// within [`EXCHANGE`]. An accept that fails otherwise, as with as many files open as the daemon
/// may, is tried again after a while (`server::ACCEPT_AGAIN`), as the daemon's VM sockets' are:
/// given up on, the daemon's metrics would be served no more, the daemon serving on (TASK-79.35).
pub fn serve_http(listener: TcpListener) -> std::io::Result<()> {
    let serving = std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
    let mut failing = false;
    loop {
        {
            let (count, done) = &*serving;
            let mut count = count.lock().unwrap();
            while *count == AT_ONCE {
                count = done.wait(count).unwrap();
            }
            *count += 1;
        }
        let accepted = listener.accept();
        if accepted.is_err() {
            *serving.0.lock().unwrap() -= 1;
        }
        let stream = match accepted {
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
        let served = serving.clone();
        let spawned = std::thread::Builder::new().name("cladefsd http".into()).spawn(move || {
            if let Err(err) = answer(stream) {
                log::warn!("cladefsd: serving HTTP: {err}");
            }
            let (count, done) = &*served;
            *count.lock().unwrap() -= 1;
            done.notify_one();
        });
        if let Err(err) = spawned {
            log::warn!("cladefsd: serving HTTP: {err}");
            *serving.0.lock().unwrap() -= 1;
        }
    }
}

/// What `method` asks of `target`, a path and its query: the metrics, at `/metrics`; the z-pages,
/// under `/z/`.
fn route(method: &str, target: &str) -> Response {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match (method, path) {
        ("GET", "/metrics") => Response::ok(METRICS, super::metrics::render()),
        ("GET", path) if let Some(page) = path.strip_prefix("/z") => {
            super::zpages::serve(page, query)
        }
        _ => Response::not_found(),
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

/// Answers the HTTP request `stream` carries ([`route`]), its head read within [`EXCHANGE`] and
/// what it asked for sent within another; `431`, for a request whose line and headers are more
/// than [`HEAD_MAX`] bytes.
fn answer(stream: TcpStream) -> std::io::Result<()> {
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
            Response::plain("431 Request Header Fields Too Large", "request too large")
        }
        (Some(method), Some(target)) => route(method, target),
        _ => Response::plain("400 Bad Request", "no request line"),
    };
    let Response { status, content_type, body } = response;
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
