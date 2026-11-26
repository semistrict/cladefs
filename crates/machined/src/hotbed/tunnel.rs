// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The node's end of hotbed's controller tunnel, ported from hotbed's worker
//! (`internal/worker/tunnel.go`; the controller's end is `cf-controller/src/tunnel.ts`): one
//! WebSocket the node keeps open to the controller's `/ws/worker`, down which the controller calls
//! WorkspaceService on it, so that a node behind NAT needs no address the controller reaches.
//!
//! Its wire is hotbed's, text frames of JSON:
//!
//! - down, the controller's: `{"type":"rpc_req","id","procedure","body","stream"}`, a call, its
//!   request protobuf in base64, `stream` whether it streams its answers; and
//!   `{"type":"rpc_cancel","id"}`, which ends a call under way.
//! - up, the node's: `{"type":"rpc_msg","id","body"}`, an answer; `{"type":"rpc_end","id"}`, a
//!   stream's end; `{"type":"rpc_err","id","code","message","suggestion","retryable"}`, a call's
//!   failure, its code hotbed's.
//! - `ping` every 25 s, which Cloudflare answers `pong` without waking the controller's Durable
//!   Object; with no frame for 75 s, the socket is taken for half-open and closed.
//!
//! A tunnel that ends is opened again: at once (150 ms) if it had lasted 15 s, as after a deploy
//! of the controller; else after a backoff doubling from 1 s to 30 s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::controller::Identity;
use super::error::{self, Error};
use super::router::{Call, Router};

/// How often the node pings, and how long it waits for any frame, hotbed's.
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(25);
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(75);
/// How long a connection's handshake may take, and a frame's write.
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest frame the node takes: hotbed's read limit.
const MAX_FRAME: usize = 16 << 20;
/// Frames queued for the controller: past that, calls wait to answer.
const QUEUED: usize = 64;

const QUICK_RECONNECT: Duration = Duration::from_millis(150);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A tunnel that lasted this long reconnects at once when it ends.
const SUSTAINED: Duration = Duration::from_secs(15);

/// A frame down the tunnel.
#[derive(Debug, serde::Deserialize)]
struct Down {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    #[serde(default)]
    procedure: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    stream: bool,
}

/// A frame up the tunnel: Go's `omitempty`s, so the same JSON.
#[derive(Debug, Default, serde::Serialize)]
struct Up {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    code: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    suggestion: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    retryable: bool,
}

impl Up {
    fn answer(id: &str, body: &[u8]) -> Self {
        let body = base64::engine::general_purpose::STANDARD.encode(body);
        Self { kind: "rpc_msg", id: id.to_owned(), body, ..Self::default() }
    }

    fn end(id: &str) -> Self {
        Self { kind: "rpc_end", id: id.to_owned(), ..Self::default() }
    }

    fn error(id: &str, e: Error) -> Self {
        Self {
            kind: "rpc_err",
            id: id.to_owned(),
            code: e.code,
            message: e.message,
            suggestion: e.suggestion,
            retryable: e.retryable,
            ..Self::default()
        }
    }
}

/// Where the tunnel goes and how it keeps alive.
pub(crate) struct Tunnel {
    /// `ws://` or `wss://`, ending `/ws/worker`.
    pub(crate) url: String,
    pub(crate) identity: Identity,
    pub(crate) ping_interval: Duration,
    pub(crate) read_timeout: Duration,
}

/// The tunnel's URL for the controller at `base` (http or https): ws or wss, at `/ws/worker`.
pub(crate) fn url(base: &reqwest::Url) -> Result<String, Error> {
    let mut url = base.clone();
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(Error::new(
                error::INVALID_ARGS,
                format!("unsupported controller scheme {other:?}"),
            ));
        }
    };
    url.set_scheme(scheme).map_err(|()| Error::new(error::INVALID_ARGS, "controller URL"))?;
    let path = format!("{}/ws/worker", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url.into())
}

/// Keeps the tunnel open, serving `router`'s calls down it, until `stop` says to stop.
pub(crate) async fn run(tunnel: Tunnel, router: Arc<Router>, mut stop: watch::Receiver<bool>) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let connected = Instant::now();
        let ended = tokio::select! {
            ended = serve_once(&tunnel, &router) => ended,
            _ = stop.wait_for(|stop| *stop) => return,
        };
        let delay = if connected.elapsed() > SUSTAINED {
            backoff = INITIAL_BACKOFF;
            QUICK_RECONNECT
        } else {
            let delay = backoff;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            delay
        };
        log::warn!(
            "machined: hotbed's controller tunnel {} ended ({}); reconnecting in {delay:?}",
            tunnel.url,
            ended.map_or_else(|e| e.to_string(), |()| "closed".into())
        );
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = stop.wait_for(|stop| *stop) => return,
        }
    }
}

/// A connection's byte stream, in the clear or TLS.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Connects to `url`, over TLS for wss with the platform's roots, and makes it a WebSocket with
/// `identity`'s headers.
async fn connect(
    tunnel: &Tunnel,
) -> Result<tokio_tungstenite::WebSocketStream<Box<dyn Io>>, Error> {
    let failed = |e: &dyn std::fmt::Display| {
        Error::new(error::CONTROLLER_UNREACHABLE, format!("dial {}: {e}", tunnel.url)).retryable()
    };
    let url = reqwest::Url::parse(&tunnel.url).map_err(|e| failed(&e))?;
    let host = url.host_str().ok_or_else(|| failed(&"no host"))?.to_owned();
    let tls = url.scheme() == "wss";
    let port = url.port_or_known_default().ok_or_else(|| failed(&"no port"))?;
    let tcp =
        tokio::net::TcpStream::connect((host.as_str(), port)).await.map_err(|e| failed(&e))?;
    tcp.set_nodelay(true).map_err(|e| failed(&e))?;
    let stream: Box<dyn Io> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host).map_err(|e| failed(&e))?;
        let connector = tokio_rustls::TlsConnector::from(super::tls().map_err(|e| failed(&e))?);
        Box::new(connector.connect(name, tcp).await.map_err(|e| failed(&e))?)
    } else {
        Box::new(tcp)
    };
    let mut request = tunnel.url.as_str().into_client_request().map_err(|e| failed(&e))?;
    for (name, value) in tunnel.identity.headers() {
        let value = HeaderValue::from_str(&value).map_err(|e| failed(&e))?;
        request.headers_mut().insert(name, value);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let (socket, _) = tokio_tungstenite::client_async_with_config(request, stream, Some(config))
        .await
        .map_err(|e| failed(&e))?;
    Ok(socket)
}

/// One connection of the tunnel, served until it ends: what ended it, or nothing for a close.
async fn serve_once(tunnel: &Tunnel, router: &Arc<Router>) -> Result<(), Error> {
    let socket = tokio::time::timeout(DIAL_TIMEOUT, connect(tunnel)).await.map_err(|_| {
        Error::new(error::CONTROLLER_UNREACHABLE, format!("dial {}: timed out", tunnel.url))
    })??;
    log::info!("machined: hotbed's controller tunnel {} connected", tunnel.url);
    let (mut sink, mut frames) = socket.split();

    // One writer, so that frames go whole and in order; its end (a write failed or timed out)
    // ends the connection.
    let (up, mut writing) = mpsc::channel::<Message>(QUEUED);
    let mut writer = tokio::spawn(async move {
        while let Some(frame) = writing.recv().await {
            match tokio::time::timeout(WRITE_TIMEOUT, sink.send(frame)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return Error::new(error::CONTROLLER_UNREACHABLE, format!("write: {e}"));
                }
                Err(_) => return Error::new(error::CONTROLLER_UNREACHABLE, "write: timed out"),
            }
        }
        Error::new(error::INTERNAL, "the writer's queue closed")
    });

    let mut calls = JoinSet::new();
    let mut under_way: HashMap<String, AbortHandle> = HashMap::new();
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + tunnel.ping_interval,
        tunnel.ping_interval,
    );
    // When a frame is due by: each frame moves it on, nothing else.
    let mut due = tokio::time::Instant::now() + tunnel.read_timeout;
    let ended = loop {
        let frame = tokio::select! {
            frame = tokio::time::timeout_at(due, frames.next()) => frame,
            _ = ping.tick() => {
                if up.send(Message::text("ping")).await.is_err() {
                    break Err(Error::new(error::INTERNAL, "the writer ended"));
                }
                continue;
            }
            ended = &mut writer => break Err(ended.unwrap_or_else(|e| Error::new(error::INTERNAL, e.to_string()))),
            Some(done) = calls.join_next_with_id(), if !calls.is_empty() => {
                // A call ended: forgotten, unless its ID names a newer call now.
                if let Ok((task, id)) = done
                    && under_way.get(&id).is_some_and(|h: &AbortHandle| h.id() == task)
                {
                    under_way.remove(&id);
                }
                continue;
            }
        };
        if frame.is_ok() {
            due = tokio::time::Instant::now() + tunnel.read_timeout;
        }
        let data = match frame {
            Err(_) => {
                break Err(Error::new(
                    error::CONTROLLER_UNREACHABLE,
                    format!(
                        "tunnel read timeout after {:?} (no pong/frame received)",
                        tunnel.read_timeout
                    ),
                ));
            }
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => {
                break Err(Error::new(error::CONTROLLER_UNREACHABLE, format!("read: {e}")));
            }
            Ok(Some(Ok(Message::Text(text)))) => text.as_bytes().to_vec(),
            Ok(Some(Ok(Message::Binary(data)))) => data.to_vec(),
            Ok(Some(Ok(Message::Close(_)))) => break Ok(()),
            // Control frames: alive, and tungstenite answers pings itself.
            Ok(Some(Ok(_))) => continue,
        };
        if data == b"ping" || data == b"pong" {
            continue;
        }
        let down: Down = match serde_json::from_slice(&data) {
            Ok(down) => down,
            Err(e) => {
                log::warn!("machined: ignoring a malformed frame from hotbed's controller: {e}");
                continue;
            }
        };
        match down.kind.as_str() {
            "rpc_cancel" => {
                if let Some(call) = under_way.remove(&down.id) {
                    call.abort();
                    // What hotbed's worker answers a call its caller canceled; the controller
                    // forgot the call already.
                    let canceled = Error::new(error::INTERNAL, "context canceled");
                    send(&up, Up::error(&down.id, canceled)).await;
                }
            }
            "rpc_req" => {
                let (router, up, id) = (router.clone(), up.clone(), down.id.clone());
                let call = calls.spawn(async move {
                    let id = down.id.clone();
                    dispatch(&router, down, &up).await;
                    id
                });
                // An id the controller uses again names its new call, as in hotbed's worker; the
                // earlier runs on, to its end or the connection's.
                under_way.insert(id, call);
            }
            _ => {}
        }
    };
    writer.abort();
    calls.shutdown().await;
    ended
}

/// Sends `frame` up, unless the connection is ending.
async fn send(up: &mpsc::Sender<Message>, frame: Up) {
    match serde_json::to_string(&frame) {
        // A closed queue: the connection ends, and with it the call.
        Ok(json) => drop(up.send(Message::text(json)).await),
        Err(e) => log::warn!("machined: encoding a tunnel frame: {e}"),
    }
}

/// Serves one call down the tunnel to its end.
async fn dispatch(router: &Router, down: Down, up: &mpsc::Sender<Message>) {
    let id = down.id.as_str();
    let body = match base64::engine::general_purpose::STANDARD.decode(&down.body) {
        Ok(body) => body,
        Err(e) => {
            let e = Error::new(error::INVALID_ARGS, format!("invalid base64 request body: {e}"));
            return send(up, Up::error(id, e)).await;
        }
    };
    match router.call(&down.procedure, down.stream, body) {
        Err(e) => send(up, Up::error(id, e)).await,
        Ok(Call::Unary(answer)) => match answer.await {
            Ok(body) => send(up, Up::answer(id, &body)).await,
            Err(e) => send(up, Up::error(id, e)).await,
        },
        Ok(Call::Stream(answers)) => {
            let mut answers = match answers.await {
                Ok(answers) => answers,
                Err(e) => return send(up, Up::error(id, e)).await,
            };
            while let Some(answer) = answers.next().await {
                match answer {
                    Ok(body) => send(up, Up::answer(id, &body)).await,
                    Err(e) => return send(up, Up::error(id, e)).await,
                }
            }
            send(up, Up::end(id)).await;
        }
    }
}
