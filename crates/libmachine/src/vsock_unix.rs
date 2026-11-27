// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine's vsock reached from the host through Unix sockets that speak Firecracker's protocol
//! (its `docs/vsock.md`), so that a client of a guest's agent written for Firecracker works
//! unchanged:
//!
//! - **Host to guest:** a caller connects to the socket and sends `CONNECT <port>\n`; the guest's
//!   port is connected to, `OK <host port>\n` answered, and the two streams are one. A connect the
//!   guest refuses, or a request that means nothing, closes the connection
//!   ([`connect_host_to_guest`]).
//! - **Guest to host:** the guest's connections to host port P are passed to the Unix socket
//!   `<socket>_<P>`, if one listens there, and dropped otherwise ([`pass_guest_connections`]): to
//!   the ports a listener names, or to every port ([`Vsock::listen_any`]).
//!
//! [`Server`] serves both on threads of its own; machined serves the first on its runtime, checking
//! its callers, and uses the pieces here.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::{Vsock, VsockListener, VsockStream};

/// How long a caller has to send its `CONNECT` line.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest `CONNECT` line taken: "CONNECT 4294967295\n" is 19 bytes.
const MAX_LINE: usize = 32;
/// How long a connect to the guest waits for it to answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a guest port's listener looks whether it is to stop.
const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// The splicing threads only copy: their stacks need little.
const THREAD_STACK: usize = 128 << 10;

/// The guest's connections passed on to the host: to which of its ports.
#[derive(Debug, Clone)]
pub enum GuestPorts {
    /// These alone; the guest's connections to the rest are refused.
    Listed(Vec<u32>),
    /// Every port, as Firecracker's are.
    Any,
}

/// A machine's vsock served at a Unix socket, on threads of its own, until dropped.
pub struct Server {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
}

impl Server {
    /// Serves `vsock` at `listener`, bound at `path`: callers' connections to the guest, and the
    /// guest's to `ports` passed to `<path>_<port>`.
    pub fn start(
        listener: UnixListener,
        path: PathBuf,
        vsock: &Vsock,
        ports: GuestPorts,
    ) -> io::Result<Self> {
        let stopped = Arc::new(AtomicBool::new(false));
        for listener in listen(vsock, &ports)? {
            let (base, stopped) = (path.clone(), stopped.clone());
            spawn("vsock-listen", move || pass_guest_connections(&listener, &base, &stopped))?;
        }
        let vsock = vsock.clone();
        let accepting = stopped.clone();
        spawn("vsock-accept", move || {
            for stream in listener.incoming() {
                if accepting.load(Ordering::SeqCst) {
                    return;
                }
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => {
                        log_debug(&format!("a vsock socket's accept failed: {e}"));
                        // No spin while descriptors are short.
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };
                let vsock = vsock.clone();
                let started = spawn("vsock-conn", move || {
                    if let Err(e) = connect_host_to_guest(stream, &vsock) {
                        log_debug(&format!("a vsock connection to the guest: {e}"));
                    }
                });
                if let Err(e) = started {
                    log_debug(&format!("a vsock connection to the guest: {e}"));
                }
            }
        })?;
        Ok(Self { path, stopped })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `vsock`'s listeners for the guest's connections to `ports`: taken first, so that a port taken
/// fails before anything is served.
pub fn listen(vsock: &Vsock, ports: &GuestPorts) -> io::Result<Vec<VsockListener>> {
    match ports {
        GuestPorts::Listed(ports) => ports.iter().map(|&port| vsock.listen(port)).collect(),
        GuestPorts::Any => Ok(vec![vsock.listen_any()?]),
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new().name(name.into()).stack_size(THREAD_STACK).spawn(f).map(drop)
}

/// What goes wrong in a connection of the guest's or a caller's: theirs, said at debug alone.
fn log_debug(what: &str) {
    if std::env::var_os("LIBMACHINE_VSOCK_DEBUG").is_some() {
        eprintln!("libmachine: {what}");
    }
}

/// Takes a caller's `CONNECT <port>` line, connects to the guest's port, answers, and splices.
pub fn connect_host_to_guest(mut stream: UnixStream, vsock: &Vsock) -> io::Result<()> {
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

/// Passes the guest's connections that `listener` takes to `<base>_<port>`, the port each was to,
/// until `stopped`, or its machine stops.
pub fn pass_guest_connections(listener: &VsockListener, base: &Path, stopped: &AtomicBool) {
    while !stopped.load(Ordering::SeqCst) {
        let guest = match listener.accept(Some(ACCEPT_POLL)) {
            Ok(guest) => guest,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            // The guest stopped, or its machine is gone: nothing more comes.
            Err(e) => {
                log_debug(&format!("vsock port {}: {e}", listener.port()));
                return;
            }
        };
        let target = PathBuf::from(format!("{}_{}", base.display(), guest.local_port()));
        let spliced = spawn("vsock-guest", move || {
            // Dropped, which resets the guest's, if nothing listens there.
            match UnixStream::connect(&target) {
                Ok(host) => {
                    if let Err(e) = splice(host, guest) {
                        log_debug(&format!("a guest's vsock connection: {e}"));
                    }
                }
                Err(e) => log_debug(&format!("{}: {e}", target.display())),
            }
        });
        if let Err(e) = spliced {
            log_debug(&format!("a guest's vsock connection: {e}"));
        }
    }
}

/// Copies each way between `host` and `guest` until both have ended, each end's shutdown passed
/// on to the other.
pub fn splice(host: UnixStream, guest: VsockStream) -> io::Result<()> {
    let guest = Arc::new(guest);
    let (host_reader, guest_writer) = (host.try_clone()?, guest.clone());
    let to_guest = std::thread::Builder::new()
        .name("vsock-up".into())
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
