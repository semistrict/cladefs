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
    /// The helper binary each machine's VM runs in, a process of its own (`machined-vm`, built
    /// with machined, beside it: `Config::default_helper`).
    pub helper: PathBuf,
    /// machined's own directory: its machines' vsock sockets. Made mode 0700 if it isn't there;
    /// what machined left in it before is removed as it starts. Short: its vsock sockets' paths,
    /// past it, must fit a Unix socket's (about 100 bytes).
    pub state: PathBuf,
    /// cladefs's extent daemon, which machined embeds (libcladefs, one a process): what its
    /// machines' roots are cells of, and whose control API (cladefsd's Cladefsd service) machined
    /// serves on its endpoint beside its own. [`Config::default_cladefs`] is a developer's.
    pub cladefs: CladefsConfig,
}

/// cladefs's daemon's configuration: cladefsd's TOML (`cladefsd --config`), or built.
pub use libcladefs::Config as CladefsConfig;

impl Config {
    /// The helper binary beside this process's own: `machined-vm`.
    pub fn default_helper() -> std::io::Result<PathBuf> {
        crate::vmm::default_helper()
    }

    /// A daemon of one local disk in `<state>/cladefs`, with a bucket of files there, its keys
    /// under a master key of its own in `<state>/cladefs/master.key`, made (32 random bytes, mode
    /// 0600) if it isn't there: a developer's machine's, whose cells no other host shares.
    pub fn default_cladefs(state: &std::path::Path) -> std::io::Result<CladefsConfig> {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        let dir = state.join("cladefs");
        std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
        let key = dir.join("master.key");
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key) {
            Ok(mut file) => {
                let mut bytes = [0u8; 32];
                std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        Ok(CladefsConfig::local(dir).master_key_file(key))
    }
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
