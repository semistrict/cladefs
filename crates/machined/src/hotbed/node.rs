// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The node: joined once, then its heartbeats and its tunnel, until machined stops.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::Hotbed;
use super::controller::{Controller, Identity};
use super::error;
use super::host;
use super::proto;
use super::tunnel::{self, Tunnel};
use super::workspaces::Workspaces;
use crate::machines::Machines;

/// The file in machined's state directory the node keeps what it joined as in.
const JOINED: &str = "hotbed-node.json";
/// The longest backoff between failed heartbeats (hotbed's worker's is 16 s).
const MAX_RETRY_WAIT: Duration = Duration::from_secs(16);

/// What the node joined as: kept, so that it joins once.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Joined {
    controller: String,
    node_id: String,
    cluster_id: String,
    worker_token: String,
}

/// The node running: [`Node::shutdown`] stops it.
pub(crate) struct Node {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Node {
    /// Stops its heartbeats and its tunnel, and waits for them.
    pub(crate) async fn shutdown(self) {
        self.stop.send_replace(true);
        for task in self.tasks {
            if let Err(e) = task.await {
                log::warn!("machined: the hotbed node's task failed: {e}");
            }
        }
    }
}

fn invalid(why: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("hotbed: {why}"))
}

/// The controller's base URL: http or https, with no trailing '/', query or fragment (hotbed's
/// `api.BaseURL`).
fn base_url(controller: &str) -> io::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(controller)
        .map_err(|e| invalid(format!("controller URL {controller:?}: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid(format!(
            "controller URL {controller:?}: a full http(s) URL, as https://hotbed-controller.example.workers.dev"
        )));
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// What the node joined `base` as before, if it did, as `node_id`.
fn joined(state: &Path, base: &reqwest::Url, node_id: &str) -> io::Result<Option<Joined>> {
    let path = state.join(JOINED);
    let bytes = match std::fs::read(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        read => read?,
    };
    let joined: Joined =
        serde_json::from_slice(&bytes).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    if joined.controller != base.as_str() || joined.node_id != node_id {
        return Err(invalid(format!(
            "{} says machined joined {} as {}, not {base} as {node_id}: remove it to join anew",
            path.display(),
            joined.controller,
            joined.node_id
        )));
    }
    Ok(Some(joined))
}

/// Keeps `joined` in `state`, machined's user's alone: written whole, then renamed into place.
fn keep(state: &Path, joined: &Joined) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let (path, part) = (state.join(JOINED), state.join(format!("{JOINED}.part")));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&part)?;
    file.write_all(&serde_json::to_vec_pretty(joined).map_err(io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&part, &path)
}

/// The node joined, not yet running: [`Joining::run`] starts it.
pub(crate) struct Joining {
    controller: Controller,
    who: Identity,
    cluster_id: String,
    workspaces: Arc<Workspaces>,
    tunnel: Tunnel,
    report_interval: Duration,
}

/// Joins hotbed as `config` says, or takes up what it joined as before: the node, to run, serving
/// its workspaces on `machines`.
pub(crate) async fn join(
    config: &Hotbed,
    machines: Arc<Machines>,
    state: &Path,
) -> io::Result<Joining> {
    let base = base_url(&config.controller)?;
    if config.node.is_empty() || config.node_id.is_empty() {
        return Err(invalid("a node has a name and an ID"));
    }
    machines
        .check(&config.workspace)
        .map_err(|s| invalid(format!("the workspace machine: {}", s.message())))?;
    let controller = Controller::new(base.clone()).map_err(io::Error::other)?;
    let joined = match joined(state, &base, &config.node_id)? {
        Some(joined) => joined,
        None => {
            let token = read_token(&config.token_file)?;
            let who = Identity {
                token: token.clone(),
                node_id: config.node_id.clone(),
                host: config.node.clone(),
            };
            let answer = controller
                .join(&who)
                .await
                .map_err(|e| io::Error::other(format!("hotbed: joining {base}: {e}")))?;
            // A worker token joins again as itself, and is answered no other.
            let worker_token =
                if answer.worker_token.is_empty() { token } else { answer.worker_token };
            let joined = Joined {
                controller: base.to_string(),
                node_id: config.node_id.clone(),
                cluster_id: answer.cluster_id,
                worker_token,
            };
            keep(state, &joined)?;
            joined
        }
    };
    log::info!(
        "machined: a node of hotbed {base} (cluster {}) as {} ({})",
        joined.cluster_id,
        config.node,
        config.node_id
    );
    let who = Identity {
        token: joined.worker_token,
        node_id: config.node_id.clone(),
        host: config.node.clone(),
    };
    let workspaces =
        Arc::new(Workspaces::new(machines, config.node_id.clone(), config.workspace.clone()));
    let tunnel = Tunnel {
        url: tunnel::url(&base).map_err(io::Error::other)?,
        identity: who.clone(),
        ping_interval: config.ping_interval,
        read_timeout: config.read_timeout,
    };
    Ok(Joining {
        controller,
        who,
        cluster_id: joined.cluster_id,
        workspaces,
        tunnel,
        report_interval: config.report_interval,
    })
}

impl Joining {
    /// Starts the node: its heartbeats and its tunnel.
    pub(crate) fn run(self) -> Node {
        let router = Arc::new(self.workspaces.router());
        let (stop, stopped) = watch::channel(false);
        let beats = heartbeats(
            self.controller,
            self.who,
            self.cluster_id,
            self.workspaces,
            self.report_interval,
            stop.clone(),
        );
        let tasks =
            vec![tokio::spawn(beats), tokio::spawn(tunnel::run(self.tunnel, router, stopped))];
        Node { stop, tasks }
    }
}

/// The token in `file`, trimmed.
fn read_token(file: &PathBuf) -> io::Result<String> {
    let token = std::fs::read_to_string(file).map_err(|e| {
        io::Error::new(e.kind(), format!("hotbed: the token file {}: {e}", file.display()))
    })?;
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(invalid(format!("the token file {} holds no token", file.display())));
    }
    Ok(token.to_owned())
}

/// Heartbeats every `interval` until `stop`, releasing what the controller says to: hotbed's
/// worker's `keep`. A controller that says the node is not joined, or of another cluster, stops the
/// node, as no retry can fix it.
async fn heartbeats(
    controller: Controller,
    who: Identity,
    cluster_id: String,
    workspaces: Arc<Workspaces>,
    interval: Duration,
    stop: watch::Sender<bool>,
) {
    let resources = host::resources();
    let mut stopped = stop.subscribe();
    let mut failures = 0u32;
    loop {
        let beat = proto::HeartbeatRequest {
            cluster_id: cluster_id.clone(),
            resources: Some(resources.clone()),
            reserved_memory_bytes: workspaces.reserved_memory(),
            ..proto::HeartbeatRequest::default()
        };
        let wait = match controller.heartbeat(&who, &beat).await {
            Ok(answer) => {
                failures = 0;
                for id in answer.release {
                    let workspaces = workspaces.clone();
                    tokio::spawn(async move {
                        match workspaces.release(&id).await {
                            Err(e) if e.code != error::NOT_FOUND => {
                                log::warn!("machined: releasing hotbed workspace {id}: {e}");
                            }
                            _ => {}
                        }
                    });
                }
                interval
            }
            Err(e) if e.code == error::NOT_JOINED || e.code == error::CLUSTER_MISMATCH => {
                log::error!("machined: hotbed's controller refuses the node, which stops: {e}");
                stop.send_replace(true);
                return;
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                if failures == 1 || failures.is_multiple_of(10) {
                    log::warn!(
                        "machined: hotbed heartbeat failed ({failures} in a row); retrying: {e}"
                    );
                }
                Duration::from_secs(1 << (failures - 1).min(4)).min(MAX_RETRY_WAIT)
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stopped.wait_for(|stop| *stop) => return,
        }
    }
}
