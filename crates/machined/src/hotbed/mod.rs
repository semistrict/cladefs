// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined as a node of hotbed (board 13896): it joins hotbed's controller as hotbed's worker
//! does, and serves it workspaces on its machines, down the controller's tunnel.
//!
//! - **Joining** (`node`): a join token (`hbj_...`) is traded once, by ClusterService's Join, for
//!   the node's own worker token (`hbw_...`), which machined keeps in its state directory
//!   (`hotbed-node.json`) and uses from then on.
//! - **Heartbeats** every 5 s keep the node online, with its resources; the controller answers
//!   which workspaces to release (`controller`, `host`).
//! - **The tunnel** (`tunnel`), hotbed's `/ws/worker` WebSocket, carries the controller's
//!   WorkspaceService calls (`router`) to the node's workspaces (`workspaces`): a machine each.
//!
//! hotbed's messages are generated from its proto, copied (`hotbed/README.md`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

mod controller;
mod error;
mod host;
mod node;
mod router;
mod tunnel;
mod workspaces;

pub use error::Error;
pub(crate) use node::{Joining, Node, join};

/// hotbed's messages, from its proto.
#[allow(clippy::all, clippy::pedantic, rustdoc::all, missing_docs)]
pub mod proto {
    tonic::include_proto!("hotbed.v1");
}

/// How machined joins hotbed as a node.
#[derive(Debug, Clone)]
pub struct Hotbed {
    /// The controller's base URL: `https://...`, or `http://...` (tests).
    pub controller: String,
    /// A file holding the token to join with: a join token (`hbj_...`), or a node's worker token
    /// (`hbw_...`). Read once, as machined starts, and only if it has joined with no token yet.
    pub token_file: PathBuf,
    /// The node's name in hotbed (`hotbed nodes`): `machined1`.
    pub node: String,
    /// The node's ID in hotbed, which a join binds its worker token to: `machined:<node>` unless
    /// set.
    pub node_id: String,
    /// What each workspace's machine is: its vCPUs, memory and boot (the guest must run
    /// libmachine's agent: `libmachine_action=agent`).
    pub workspace: crate::proto::MachineSpec,
    /// How often the node heartbeats (hotbed's report interval).
    pub report_interval: Duration,
    /// How often the tunnel pings, and how long it waits for a frame before it reconnects.
    pub ping_interval: Duration,
    pub read_timeout: Duration,
}

impl Hotbed {
    /// A node named `node` of the controller at `controller`, joining with the token in
    /// `token_file`, its workspaces machines of `workspace`; hotbed's intervals.
    pub fn new(
        controller: impl Into<String>,
        token_file: impl Into<PathBuf>,
        node: impl Into<String>,
        workspace: crate::proto::MachineSpec,
    ) -> Self {
        let node = node.into();
        Self {
            controller: controller.into(),
            token_file: token_file.into(),
            node_id: format!("machined:{node}"),
            node,
            workspace,
            report_interval: Duration::from_secs(5),
            ping_interval: tunnel::PING_INTERVAL,
            read_timeout: tunnel::READ_TIMEOUT,
        }
    }
}

/// TLS to the controller: the platform's roots, over aws-lc-rs, HTTP/1.1 (a WebSocket's).
fn tls() -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    use rustls_platform_verifier::BuilderVerifierExt as _;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}
