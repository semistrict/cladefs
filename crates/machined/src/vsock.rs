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
//! its API's socket is, and its callers are checked by their peer credentials the same way. The
//! protocol itself is libmachine's (`libmachine::vsock_unix`), which this serves on machined's
//! runtime.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use libmachine::Vsock;
use libmachine::vsock_unix::{GuestPorts, connect_host_to_guest, pass_guest_connections};

use crate::listen;

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
        let listeners = libmachine::vsock_unix::listen(vsock, &GuestPorts::Listed(ports.to_vec()))?;
        let listener = listen::bind_owner_only(&path)?;
        for listener in listeners {
            let (base, stopped) = (path.clone(), stopped.clone());
            spawn("machined-vsock-listen", move || {
                pass_guest_connections(&listener, &base, &stopped);
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
