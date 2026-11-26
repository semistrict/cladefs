// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Calls to hotbed's controller, ClusterService's Join and Heartbeat: Connect's unary protocol
//! over HTTPS (a POST of the request's protobuf, `application/proto`, the answer's back; an error
//! as Connect's JSON), with the headers hotbed's worker sends a remote controller
//! (`internal/api/rpc.go`'s `AuthInterceptor`): its bearer token and who it is.

use std::time::Duration;

use prost::Message;

use super::error::{self, Error};
use super::proto;

/// How long a call to the controller may take, hotbed's worker's.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest answer the node reads.
const MAX_ANSWER: usize = 16 << 20;

/// Who the node is to the controller: the headers of each of its calls and of its tunnel.
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    /// `hbj_...` (a join token) to join, `hbw_...` (the node's own) after.
    pub(crate) token: String,
    pub(crate) node_id: String,
    pub(crate) host: String,
}

impl Identity {
    /// Its headers, as hotbed's worker sends them; no `X-Hotbed-Addr`, as no client reaches the
    /// node but through the controller.
    pub(crate) fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![("Authorization", format!("Bearer {}", self.token))];
        if !self.node_id.is_empty() {
            headers.push(("X-Hotbed-Node-Id", self.node_id.clone()));
        }
        if !self.host.is_empty() {
            headers.push(("X-Hotbed-Host", self.host.clone()));
        }
        headers
    }
}

/// A client of the controller at its base URL.
#[derive(Clone)]
pub(crate) struct Controller {
    base: reqwest::Url,
    http: reqwest::Client,
}

impl Controller {
    pub(crate) fn new(base: reqwest::Url) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("machined/", env!("CARGO_PKG_VERSION")))
            .timeout(CALL_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::new(error::INTERNAL, format!("an HTTP client: {e}")))?;
        Ok(Self { base, http })
    }

    pub(crate) async fn join(&self, who: &Identity) -> Result<proto::JoinResponse, Error> {
        self.call(who, "Join", &proto::JoinRequest {}).await
    }

    pub(crate) async fn heartbeat(
        &self,
        who: &Identity,
        beat: &proto::HeartbeatRequest,
    ) -> Result<proto::HeartbeatResponse, Error> {
        self.call(who, "Heartbeat", beat).await
    }

    async fn call<Req: Message, Res: Message + Default>(
        &self,
        who: &Identity,
        method: &str,
        request: &Req,
    ) -> Result<Res, Error> {
        let unreachable = |e: &dyn std::fmt::Display| {
            Error::new(error::CONTROLLER_UNREACHABLE, format!("ClusterService/{method}: {e}"))
                .retryable()
        };
        let path =
            format!("{}/hotbed.v1.ClusterService/{method}", self.base.path().trim_end_matches('/'));
        let mut url = self.base.clone();
        url.set_path(&path);
        let mut call = self
            .http
            .post(url)
            .header("Content-Type", "application/proto")
            .header("Connect-Protocol-Version", "1")
            .body(request.encode_to_vec());
        for (name, value) in who.headers() {
            call = call.header(name, value);
        }
        let mut answer = call.send().await.map_err(|e| unreachable(&e))?;
        let status = answer.status();
        let mut body = Vec::new();
        while let Some(chunk) = answer.chunk().await.map_err(|e| unreachable(&e))? {
            if body.len().saturating_add(chunk.len()) > MAX_ANSWER {
                return Err(Error::new(
                    error::INTERNAL,
                    format!("ClusterService/{method}: an answer past {MAX_ANSWER} bytes"),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(Error::of_connect(&body, &status.to_string()));
        }
        Res::decode(body.as_slice()).map_err(|e| {
            Error::new(error::INTERNAL, format!("ClusterService/{method}'s answer: {e}"))
        })
    }
}
