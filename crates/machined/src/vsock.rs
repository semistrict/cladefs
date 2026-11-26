// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine's vsock, reached from the host through a Unix socket that speaks Firecracker's
//! protocol (its `docs/vsock.md`), so that a VM manager's client of its own agent in the guest
//! (hotbed's guestd dialer) works unchanged (hotbed question 568):
//!
//! - **Host to guest:** a caller connects to the socket and sends `CONNECT <port>\n`; machined
//!   connects to the guest's port and answers `OK <host port>\n`, then the two streams are one.
//!   A connect the guest refuses, or a request that means nothing, closes the connection.
//! - **Guest to host:** the guest's connections to a host port of the machine's
//!   `vsock_listen_ports` are passed to the Unix socket `<socket>_<port>`, if one listens there,
//!   and refused otherwise.
//!
//! The socket is mode 0600 in machined's state directory, mode 0700: machined's user's alone, as
//! its API's socket is, and its callers are checked by their peer credentials the same way.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use libmachine::{Vsock, VsockStream};

use crate::listen;

/// How long a caller has to send its `CONNECT` line.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest `CONNECT` line taken: "CONNECT 4294967295\n" is 19 bytes.
const MAX_LINE: usize = 32;
/// How long a connect to the guest waits for it to answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a guest port's listener looks whether its machine stopped.
const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// The splicing threads only copy: their stacks need little.
const THREAD_STACK: usize = 128 << 10;

/// A machine's vsock socket, served until [`stop`](Self::stop)ped or dropped.
pub(crate) struct Server {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
    accepting: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Serves `vsock` at `path`, and passes the guest's connections to each of `ports` on.
    pub(crate) fn start(path: PathBuf, vsock: &Vsock, ports: &[u32]) -> io::Result<Self> {
        let stopped = Arc::new(AtomicBool::new(false));
        // Listeners first: a port taken fails the start before anything is served.
        let listeners = ports
            .iter()
            .map(|&port| vsock.listen(port).map(|l| (port, l)))
            .collect::<io::Result<Vec<_>>>()?;
        let listener = listen::bind_owner_only(&path)?;
        for (port, listener) in listeners {
            let target = PathBuf::from(format!("{}_{port}", path.display()));
            let stopped = stopped.clone();
            spawn("machined-vsock-listen", move || {
                pass_guest_connections(&listener, &target, &stopped);
            })?;
        }
        let vsock = vsock.clone();
        let accepting = tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        log::warn!("machined: a vsock socket's accept failed: {e}");
                        // As the API's listener does: no spin while descriptors are short.
                        tokio::time::sleep(listen::ACCEPT_PAUSE).await;
                        continue;
                    }
                };
                if let Err(e) = listen::check_peer(&stream) {
                    log::warn!("machined: a vsock socket's caller refused: {e}");
                    continue;
                }
                let vsock = vsock.clone();
                let started = stream.into_std().and_then(|stream| {
                    stream.set_nonblocking(false)?;
                    spawn("machined-vsock", move || {
                        if let Err(e) = connect_host_to_guest(stream, &vsock) {
                            log::debug!("machined: a vsock connection to the guest: {e}");
                        }
                    })
                });
                if let Err(e) = started {
                    log::warn!("machined: a vsock connection to the guest: {e}");
                }
            }
        });
        Ok(Self { path, stopped, accepting })
    }

    /// Serves it no more: the socket goes, and the guest ports' listeners within
    /// [`ACCEPT_POLL`]. Connections made go on until their machine is freed.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.accepting.abort();
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                log::warn!("machined: removing {}: {e}", self.path.display());
            }
            _ => {}
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new().name(name.into()).stack_size(THREAD_STACK).spawn(f).map(drop)
}

/// Takes a caller's `CONNECT <port>` line, connects to the guest's port, answers, and splices.
fn connect_host_to_guest(mut stream: UnixStream, vsock: &Vsock) -> io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let port = parse_connect(&read_line(&mut stream)?)?;
    stream.set_read_timeout(None)?;
    let guest = vsock.connect(port, CONNECT_TIMEOUT)?;
    stream.write_all(format!("OK {}\n", guest.local_port()).as_bytes())?;
    splice(stream, guest)
}

/// The first line a caller sends, without its newline: at most [`MAX_LINE`] bytes, read a byte at
/// a time so that nothing past it is taken from the stream.
fn read_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no CONNECT line"));
        }
        if byte == *b"\n" {
            return Ok(line);
        }
        if line.len() == MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "CONNECT line too long"));
        }
        line.extend_from_slice(&byte);
    }
}

/// The port of a `CONNECT <port>` line: a decimal u32, nothing else.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn parse_connect(line: &[u8]) -> io::Result<u32> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "not a CONNECT <port> line");
    let port = line.strip_prefix(b"CONNECT ").ok_or_else(invalid)?;
    if port.is_empty() || !port.iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    std::str::from_utf8(port).map_err(|_| invalid())?.parse().map_err(|_| invalid())
}

/// Passes the guest's connections to `listener`'s port to `target`, until `stopped`.
fn pass_guest_connections(
    listener: &libmachine::VsockListener,
    target: &Path,
    stopped: &AtomicBool,
) {
    while !stopped.load(Ordering::SeqCst) {
        let guest = match listener.accept(Some(ACCEPT_POLL)) {
            Ok(guest) => guest,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            // The guest stopped, or its machine is gone: nothing more comes.
            Err(e) => {
                log::debug!("machined: vsock port {}: {e}", listener.port());
                return;
            }
        };
        let target = target.to_owned();
        let spliced = spawn("machined-vsock", move || {
            // Refused (dropped) if nothing listens there.
            match UnixStream::connect(&target) {
                Ok(host) => {
                    if let Err(e) = splice(host, guest) {
                        log::debug!("machined: a guest's vsock connection: {e}");
                    }
                }
                Err(e) => log::debug!("machined: {}: {e}", target.display()),
            }
        });
        if let Err(e) = spliced {
            log::warn!("machined: a guest's vsock connection: {e}");
        }
    }
}

/// Copies each way between `host` and `guest` until both have ended, each end's shutdown passed
/// on to the other.
fn splice(host: UnixStream, guest: VsockStream) -> io::Result<()> {
    let guest = Arc::new(guest);
    let (host_reader, guest_writer) = (host.try_clone()?, guest.clone());
    let to_guest = std::thread::Builder::new()
        .name("machined-vsock-up".into())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            let copied = io::copy(&mut &host_reader, &mut &*guest_writer);
            // The other way may go on: pass the end on, as a half-close.
            let _ = guest_writer.shutdown(Shutdown::Write);
            copied
        })?;
    let copied = io::copy(&mut &*guest, &mut &host);
    // Passed on as above; the host's end may be gone already.
    let _ = host.shutdown(Shutdown::Write);
    let up = to_guest.join().unwrap_or_else(|_| Err(io::Error::other("a splice thread panicked")));
    copied.and(up).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port is a decimal u32 and nothing more: the edges and what is past them.
    #[test]
    fn connect_lines() {
        assert_eq!(parse_connect(b"CONNECT 0").unwrap(), 0);
        assert_eq!(parse_connect(b"CONNECT 1").unwrap(), 1);
        assert_eq!(parse_connect(b"CONNECT 4294967295").unwrap(), u32::MAX);
        for bad in [
            &b"CONNECT 4294967296"[..],
            b"CONNECT ",
            b"CONNECT",
            b"CONNECT -1",
            b"CONNECT +1",
            b"CONNECT 1 ",
            b"connect 1",
            b"CONNECT 99999999999999999999999",
            b"",
        ] {
            assert_eq!(
                parse_connect(bad).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
