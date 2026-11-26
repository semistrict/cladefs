// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What machined is started with: machined's flags (src/main.rs), or a [`Config`] made by its
//! tests.

use std::net::SocketAddr;
use std::path::PathBuf;

/// How machined runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The Unix socket to serve the API on, if any: made mode 0600, machined's user's alone; it
    /// fails to start if something is there already.
    pub socket: Option<PathBuf>,
    /// TCP to serve the API on, with mutual TLS, if any.
    pub tcp: Option<Tcp>,
    /// The directory machines' kernels and initramfses are named in (`Boot`'s paths): nothing
    /// outside it boots.
    pub images: PathBuf,
    /// machined's own directory: its machines' vsock sockets, snapshots and suspensions. Made
    /// mode 0700 if it isn't there; what machined left in it before is removed as it starts.
    /// On disk, not tmpfs (suspensions refuse tmpfs), and short: its vsock sockets' paths, past
    /// it, must fit a Unix socket's (about 100 bytes).
    pub state: PathBuf,
}

/// TCP with mutual TLS.
#[derive(Debug, Clone)]
pub struct Tcp {
    /// Where to listen: port 0 for one the system picks ([`crate::Server::tcp_address`]).
    pub address: SocketAddr,
    /// machined's certificate (PEM), with its chain.
    pub cert: PathBuf,
    /// Its private key (PEM).
    pub key: PathBuf,
    /// The CA (PEM) whose certificates callers must present.
    pub client_ca: PathBuf,
}
