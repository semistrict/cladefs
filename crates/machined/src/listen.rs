// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Where machined serves its API, and who may call it.
//!
//! - **A Unix socket**, as cladefsd's control API is served: mode 0600 from the moment it exists,
//!   whatever the umask, so that only machined's user can connect; and each caller's peer
//!   credentials are checked too, machined's user or root, others' connections closed at once.
//!   Every caller may do everything: the socket is for the host's VM manager.
//! - **TCP, with mutual TLS**: machined's certificate and key, and the CA whose certificates its
//!   callers must present; a caller without one, or with another CA's, is refused in the
//!   handshake. There is no TCP in the clear (hotbed question 570).

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use tokio::net::UnixListener;
use tokio::sync::watch;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use crate::config::{Config, Tcp};
use crate::machines::Machines;
use crate::proto::machined_server::MachinedServer;
use crate::service::Service;

/// How long a server pauses after an accept fails, as when no descriptor is free (EMFILE): rather
/// than try again at once, in a loop holding a thread while the host is short.
pub(crate) const ACCEPT_PAUSE: Duration = Duration::from_millis(50);
/// How long machined's stop waits for cladefs's daemon to drain: its cells' commits uploaded.
const CLADEFS_DRAIN: Duration = Duration::from_secs(60);

/// A listener at `socket`, which only this process's user may connect to: mode 0600 from the
/// moment it is there, whatever the process's umask (cladefsd's `bind_owner_only`): bound in a
/// directory beside it only this user may enter, given its mode there, and only then linked at
/// `socket`, which fails with `EEXIST` if anything is there already.
pub(crate) fn bind_owner_only(socket: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let name = socket.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let mut hidden = std::ffi::OsString::from(".");
    hidden.push(name);
    let private = socket.with_file_name(hidden);
    let bound = private.join("s");
    remove_unfinished(&private, &bound)?;
    std::fs::DirBuilder::new().mode(0o700).create(&private)?;
    let listener = UnixListener::bind(&bound).and_then(|listener| {
        std::fs::set_permissions(&bound, std::fs::Permissions::from_mode(0o600))?;
        std::fs::hard_link(&bound, socket)?;
        Ok(listener)
    });
    if let Err(e) = remove_unfinished(&private, &bound) {
        log::warn!("machined: removing {}: {e}", private.display());
    }
    listener
}

/// Removes the directory `private` that [`bind_owner_only`] binds in, and the socket `bound` in
/// it, whichever of them is there.
fn remove_unfinished(private: &Path, bound: &Path) -> io::Result<()> {
    let gone = |removed: io::Result<()>| match removed {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    };
    gone(std::fs::remove_file(bound))?;
    gone(std::fs::remove_dir(private))
}

/// Refuses a Unix socket's caller that is neither machined's user nor root.
pub(crate) fn check_peer(stream: &tokio::net::UnixStream) -> io::Result<()> {
    let uid = stream.peer_cred()?.uid();
    // SAFETY: A plain system call, which cannot fail.
    let ours = unsafe { libc::geteuid() };
    if uid == ours || uid == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("uid {uid} is neither machined's ({ours}) nor root"),
        ))
    }
}

/// What `incoming` accepts, its failures logged and passed over after a pause ([`ACCEPT_PAUSE`]),
/// so that one never ends the server.
fn pausing<T: Send + 'static>(
    incoming: impl futures::Stream<Item = io::Result<T>> + Send + 'static,
) -> impl futures::Stream<Item = io::Result<T>> + Send + 'static {
    incoming.filter_map(|accepted| async move {
        match accepted {
            Ok(stream) => Some(Ok(stream)),
            Err(e) => {
                log::warn!("machined: accepting a caller: {e}");
                tokio::time::sleep(ACCEPT_PAUSE).await;
                None
            }
        }
    })
}

/// The connections `listener` accepts whose callers [`check_peer`] lets in.
fn checked(
    listener: UnixListener,
) -> impl futures::Stream<Item = io::Result<tokio::net::UnixStream>> + Send + 'static {
    pausing(tokio_stream::wrappers::UnixListenerStream::new(listener)).filter(|accepted| {
        let admitted = match accepted {
            Ok(stream) => check_peer(stream)
                .map_err(|e| log::warn!("machined: a caller refused: {e}"))
                .is_ok(),
            Err(_) => true,
        };
        std::future::ready(admitted)
    })
}

/// The TLS configuration of TCP: machined's identity, and its callers' CA, required.
fn tls(tcp: &Tcp) -> io::Result<ServerTlsConfig> {
    let read = |path: &Path| {
        std::fs::read(path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
    };
    let identity = Identity::from_pem(read(&tcp.cert)?, read(&tcp.key)?);
    Ok(ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(Certificate::from_pem(read(&tcp.client_ca)?))
        .client_auth_optional(false))
}

/// machined serving, from [`start`]: [`shutdown`](Server::shutdown) stops it.
pub struct Server {
    socket: Option<PathBuf>,
    tcp: Option<SocketAddr>,
    machines: Arc<Machines>,
    /// cladefs's daemon, this process's: drained as machined stops.
    cladefs: libcladefs::Daemon,
    hotbed: Option<crate::hotbed::Node>,
    shutdown: watch::Sender<bool>,
    serving: Vec<tokio::task::JoinHandle<Result<(), tonic::transport::Error>>>,
}

impl Server {
    /// The Unix socket it serves on, if any.
    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// The TCP address it serves on, if any: its port the one bound, if the config asked for 0.
    pub fn tcp_address(&self) -> Option<SocketAddr> {
        self.tcp
    }

    /// Stops serving: takes no more callers (its hotbed node leaves its tunnel), stops every
    /// machine and waits until each is freed, ends the streams on them (watches, consoles, execs),
    /// then waits for the calls under way.
    pub async fn shutdown(self) -> io::Result<()> {
        if let Some(node) = self.hotbed {
            node.shutdown().await;
        }
        self.shutdown.send_replace(true);
        // Before the servers' graceful ends, which wait for every stream to end.
        self.machines.close().await;
        log::info!("machined: every machine stopped; waiting for the calls under way");
        let mut failed = None;
        for serving in self.serving {
            match serving.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failed = Some(io::Error::other(e)),
                Err(e) => failed = Some(io::Error::other(e)),
            }
        }
        if let Some(socket) = &self.socket {
            std::fs::remove_file(socket)?;
        }
        // Its cells closed and their commits uploaded, once no machine has one open.
        log::info!("machined: draining cladefs's daemon");
        if let Err(e) = self.cladefs.stop(CLADEFS_DRAIN).await {
            failed = Some(io::Error::other(format!("draining cladefs's daemon: {e:#}")));
        }
        failed.map_or(Ok(()), Err)
    }
}

/// Starts machined as `config` says: its state directory made ready, its listeners bound, and
/// its API served on them, on the current tokio runtime, until [`Server::shutdown`].
pub async fn start(config: Config) -> io::Result<Server> {
    if config.socket.is_none() && config.tcp.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "machined serves on nothing: give it a Unix socket, TCP, or both",
        ));
    }
    let cladefs = libcladefs::Daemon::start(config.cladefs.clone())
        .await
        .map_err(|e| io::Error::other(format!("starting cladefs's daemon: {e:#}")))?;
    let machines =
        Arc::new(Machines::new(&config.images, &config.helper, &config.state, cladefs.clone())?);
    // Joined first, run last: a node that can't join fails the start with nothing bound, and
    // runs only once machined serves.
    let joining = match &config.hotbed {
        Some(hotbed) => Some(crate::hotbed::join(hotbed, machines.clone(), &config.state).await?),
        None => None,
    };
    let service = Service::new(machines.clone());
    // cladefs's daemon's control API (its Cladefsd service, and the health service, which says
    // whether the daemon serves) and machined's own, on each endpoint.
    let routes = cladefs
        .control_service()
        .await
        .map_err(|e| io::Error::other(format!("cladefs's control API: {e:#}")))?
        .add_service(MachinedServer::new(service));
    let (shutdown, _) = watch::channel(false);
    let stopped = |shutdown: &watch::Sender<bool>| {
        let mut stop = shutdown.subscribe();
        async move {
            // The sender lives in the Server: gone, it stops too.
            let _ = stop.wait_for(|stop| *stop).await;
        }
    };
    // TCP's listener and TLS first: a failure there leaves no Unix socket behind.
    let tcp = match &config.tcp {
        Some(tcp) => {
            let builder = tonic::transport::Server::builder()
                .tls_config(tls(tcp)?)
                .map_err(io::Error::other)?;
            Some((builder, tokio::net::TcpListener::bind(tcp.address).await?))
        }
        None => None,
    };
    let bound_tcp = tcp.as_ref().map(|(_, l)| l.local_addr()).transpose()?;
    let mut serving = Vec::new();
    if let Some(socket) = &config.socket {
        let incoming = checked(bind_owner_only(socket)?);
        let server = tonic::transport::Server::builder()
            .add_routes(routes.clone())
            .serve_with_incoming_shutdown(incoming, stopped(&shutdown));
        serving.push(tokio::spawn(server));
    }
    if let Some((mut builder, listener)) = tcp {
        let incoming = pausing(tokio_stream::wrappers::TcpListenerStream::new(listener));
        let server =
            builder.add_routes(routes).serve_with_incoming_shutdown(incoming, stopped(&shutdown));
        serving.push(tokio::spawn(server));
    }
    let hotbed = joining.map(crate::hotbed::Joining::run);
    Ok(Server {
        socket: config.socket,
        tcp: bound_tcp,
        machines,
        cladefs,
        hotbed,
        shutdown,
        serving,
    })
}
