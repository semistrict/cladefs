// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined as a node of hotbed, end to end against a fake controller that speaks the
//! controller's side as hotbed's Cloudflare Durable Object does (`cf-controller/src/tunnel.ts`,
//! `services/cluster.ts`), as hotbed's own tunnel test drives its worker
//! (`internal/worker/tunnel_test.go`): ClusterService's Join and Heartbeat over Connect, and the
//! `/ws/worker` WebSocket with its JSON frames, down which it calls WorkspaceService.
//!
//! Without a guest: the join, its token kept and used, heartbeats, the tunnel's frames and its
//! errors, its reconnection when the controller goes silent. With one (the guest of
//! tests/machined.rs, or a "skipping:" note): workspaces as machines, commands in them, their logs
//! followed, canceled and stopped, and a heartbeat's release.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]

mod common;

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::{BodyExt as _, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::HeaderMap;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use machined::hotbed::{Hotbed, proto as pb};
use prost::Message as _;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

const JOIN_TOKEN: &str = "hbj_test.join";
const WORKER_TOKEN: &str = "hbw_test.minted";
const CLUSTER: &str = "c-test";
const NODE: &str = "machined1";
const NODE_ID: &str = "machined:machined1";
/// How long a test waits for what the node is to do.
const WAIT: Duration = Duration::from_secs(60);
/// Each workspace's machine's memory.
const WORKSPACE_MEMORY: u64 = 512 << 20;

type Ws = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

/// What the fake controller's handlers share with the test.
struct Shared {
    joins: mpsc::UnboundedSender<HeaderMap>,
    beats: mpsc::UnboundedSender<(HeaderMap, pb::HeartbeatRequest)>,
    sockets: mpsc::UnboundedSender<(HeaderMap, Ws)>,
    /// What the next heartbeat's answer says to release.
    release: Mutex<Vec<String>>,
    /// Joins refused, as for a token the controller doesn't know.
    refuse: AtomicBool,
}

/// A controller of the test's, on a port of localhost.
struct Fake {
    url: String,
    shared: Arc<Shared>,
    joins: mpsc::UnboundedReceiver<HeaderMap>,
    beats: mpsc::UnboundedReceiver<(HeaderMap, pb::HeartbeatRequest)>,
    sockets: mpsc::UnboundedReceiver<(HeaderMap, Ws)>,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

fn answer(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

/// Connect's JSON error, hotbed's code in its ErrorInfo, as the controller refuses a token.
fn refused() -> Response<Full<Bytes>> {
    let info = pb::ErrorInfo {
        code: "NOT_AUTHORIZED".into(),
        suggestion: "mint a join token".into(),
        retryable: false,
    };
    let value = base64::engine::general_purpose::STANDARD_NO_PAD.encode(info.encode_to_vec());
    let body = json!({
        "code": "unauthenticated",
        "message": "bad token",
        "details": [{"type": "hotbed.v1.ErrorInfo", "value": value}],
    });
    answer(StatusCode::UNAUTHORIZED, "application/json", body.to_string().into_bytes())
}

async fn handle(shared: Arc<Shared>, request: Request<Incoming>) -> Response<Full<Bytes>> {
    let headers = request.headers().clone();
    let auth = header(&headers, "authorization").to_owned();
    match request.uri().path() {
        "/hotbed.v1.ClusterService/Join" => {
            shared.joins.send(headers.clone()).unwrap();
            if shared.refuse.load(Ordering::SeqCst) || auth != format!("Bearer {JOIN_TOKEN}") {
                return refused();
            }
            let node = pb::Node {
                id: header(&headers, "x-hotbed-node-id").into(),
                hostname: header(&headers, "x-hotbed-host").into(),
                ..pb::Node::default()
            };
            let joined = pb::JoinResponse {
                cluster_id: CLUSTER.into(),
                node: Some(node),
                worker_token: WORKER_TOKEN.into(),
            };
            answer(StatusCode::OK, "application/proto", joined.encode_to_vec())
        }
        "/hotbed.v1.ClusterService/Heartbeat" => {
            let body = request.into_body().collect().await.unwrap().to_bytes();
            if auth != format!("Bearer {WORKER_TOKEN}") {
                return refused();
            }
            let beat = pb::HeartbeatRequest::decode(body).unwrap();
            shared.beats.send((headers, beat)).unwrap();
            let release = std::mem::take(&mut *shared.release.lock().unwrap());
            let answered = pb::HeartbeatResponse { release, ..pb::HeartbeatResponse::default() };
            answer(StatusCode::OK, "application/proto", answered.encode_to_vec())
        }
        "/ws/worker" => {
            if auth != format!("Bearer {WORKER_TOKEN}") {
                return answer(StatusCode::FORBIDDEN, "text/plain", b"bad auth".to_vec());
            }
            let key = header(&headers, "sec-websocket-key").as_bytes().to_vec();
            let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(&key);
            tokio::spawn(async move {
                let upgraded = hyper::upgrade::on(request).await.unwrap();
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                // The test may be done with it.
                let _ = shared.sockets.send((headers, ws));
            });
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header("Upgrade", "websocket")
                .header("Connection", "Upgrade")
                .header("Sec-WebSocket-Accept", accept)
                .body(Full::new(Bytes::new()))
                .unwrap()
        }
        _ => answer(StatusCode::NOT_FOUND, "text/plain", b"404 page not found".to_vec()),
    }
}

impl Fake {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (joins, joins_rx) = mpsc::unbounded_channel();
        let (beats, beats_rx) = mpsc::unbounded_channel();
        let (sockets, sockets_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            joins,
            beats,
            sockets,
            release: Mutex::default(),
            refuse: AtomicBool::new(false),
        });
        let serving = shared.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let shared = serving.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        let shared = shared.clone();
                        async move { Ok::<_, Infallible>(handle(shared, request).await) }
                    });
                    // A connection the node drops ends here.
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Self { url, shared, joins: joins_rx, beats: beats_rx, sockets: sockets_rx }
    }

    async fn join(&mut self) -> HeaderMap {
        tokio::time::timeout(WAIT, self.joins.recv()).await.expect("no join").unwrap()
    }

    async fn beat(&mut self) -> (HeaderMap, pb::HeartbeatRequest) {
        tokio::time::timeout(WAIT, self.beats.recv()).await.expect("no heartbeat").unwrap()
    }

    async fn tunnel(&mut self) -> (HeaderMap, Tunnel) {
        let (headers, ws) =
            tokio::time::timeout(WAIT, self.sockets.recv()).await.expect("no tunnel").unwrap();
        (headers, Tunnel { ws, next: 0 })
    }
}

/// The controller's end of a tunnel.
struct Tunnel {
    ws: Ws,
    next: u32,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode<T: prost::Message + Default>(frame: &Value) -> T {
    let body = base64::engine::general_purpose::STANDARD
        .decode(frame["body"].as_str().unwrap_or_default())
        .unwrap();
    T::decode(body.as_slice()).unwrap()
}

impl Tunnel {
    async fn send(&mut self, frame: Value) {
        self.ws.send(Message::text(frame.to_string())).await.unwrap();
    }

    /// The node's next frame, past its pings, each answered as Cloudflare does.
    async fn up(&mut self) -> Value {
        tokio::time::timeout(WAIT, async {
            loop {
                let frame = self.ws.next().await.expect("the tunnel ended").unwrap();
                let text = frame.into_text().unwrap();
                if text.as_str() == "ping" {
                    self.ws.send(Message::text("pong")).await.unwrap();
                    continue;
                }
                return serde_json::from_str(text.as_str()).unwrap();
            }
        })
        .await
        .expect("no frame from the node")
    }

    /// Calls `method` with `request`: its ID.
    async fn call(&mut self, method: &str, request: &impl prost::Message, stream: bool) -> String {
        self.next += 1;
        let id = format!("call-{}", self.next);
        let procedure = format!("/hotbed.v1.WorkspaceService/{method}");
        let frame = json!({
            "type": "rpc_req", "id": id, "procedure": procedure,
            "body": b64(&request.encode_to_vec()), "stream": stream,
        });
        self.send(frame).await;
        id
    }

    /// The answer of unary `method`, or its error frame.
    async fn unary<T: prost::Message + Default>(
        &mut self,
        method: &str,
        request: &impl prost::Message,
    ) -> Result<T, Value> {
        let id = self.call(method, request, false).await;
        let up = self.up().await;
        assert_eq!(up["id"], id.as_str(), "{up}");
        match up["type"].as_str() {
            Some("rpc_msg") => Ok(decode(&up)),
            Some("rpc_err") => Err(up),
            _ => panic!("a unary call's answer: {up}"),
        }
    }

    /// The answers of streaming `method`, and the frame that ended them (rpc_end or rpc_err).
    async fn stream<T: prost::Message + Default>(
        &mut self,
        method: &str,
        request: &impl prost::Message,
    ) -> (Vec<T>, Value) {
        let id = self.call(method, request, true).await;
        let mut answers = Vec::new();
        loop {
            let up = self.up().await;
            assert_eq!(up["id"], id.as_str(), "{up}");
            match up["type"].as_str() {
                Some("rpc_msg") => answers.push(decode(&up)),
                Some("rpc_end" | "rpc_err") => return (answers, up),
                _ => panic!("a stream's frame: {up}"),
            }
        }
    }
}

/// An rpc_err frame's code and message.
fn refusal(frame: &Value) -> (&str, &str) {
    assert_eq!(frame["type"], "rpc_err", "{frame}");
    (frame["code"].as_str().unwrap(), frame["message"].as_str().unwrap())
}

/// A machined of the test's, a node of `controller`, its images `images`.
struct Daemon {
    server: Option<machined::Server>,
    dir: PathBuf,
}

fn config(dir: &Path, controller: &str, tune: impl FnOnce(&mut Hotbed)) -> machined::Config {
    let workspace = machined::proto::MachineSpec {
        vcpus: 2,
        memory_bytes: WORKSPACE_MEMORY,
        boot: Some(machined::proto::Boot {
            kernel: "kernel".into(),
            initramfs: "initramfs.cpio".into(),
            command_line: "libmachine_action=agent".into(),
        }),
        ..Default::default()
    };
    let mut hotbed = Hotbed::new(controller, dir.join("token"), NODE, workspace);
    tune(&mut hotbed);
    machined::Config {
        socket: Some(dir.join("machined.sock")),
        tcp: None,
        images: dir.join("images"),
        helper: env!("CARGO_BIN_EXE_machined-vm").into(),
        state: dir.join("state"),
        hotbed: Some(hotbed),
    }
}

/// A directory for a machined: its images `images` (else stand-ins, for a node that boots
/// nothing), and its join token.
fn prepare(images: Option<(&Path, &Path)>) -> tempfile::TempDir {
    // Short, as vsock sockets' paths must be.
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let image_dir = dir.path().join("images");
    std::fs::create_dir(&image_dir).unwrap();
    match images {
        Some((kernel, initramfs)) => {
            std::fs::copy(kernel, image_dir.join("kernel")).unwrap();
            std::fs::copy(initramfs, image_dir.join("initramfs.cpio")).unwrap();
        }
        None => {
            std::fs::write(image_dir.join("kernel"), b"not booted").unwrap();
            std::fs::write(image_dir.join("initramfs.cpio"), b"not booted").unwrap();
        }
    }
    std::fs::write(dir.path().join("token"), format!("{JOIN_TOKEN}\n")).unwrap();
    dir
}

impl Daemon {
    async fn start(dir: &Path, controller: &str, tune: impl FnOnce(&mut Hotbed)) -> Self {
        let server = machined::start(config(dir, controller, tune)).await.unwrap();
        Self { server: Some(server), dir: dir.to_owned() }
    }

    async fn shutdown(mut self) {
        self.server.take().unwrap().shutdown().await.unwrap();
        assert!(!self.dir.join("machined.sock").exists());
    }
}

/// The node joins with its join token, keeps the worker token it is given, heartbeats and opens
/// its tunnel with it, serves the controller's calls down it as hotbed's worker does, and, started
/// again, takes up what it joined as, joining no more.
#[tokio::test]
async fn joins_beats_and_serves_its_tunnel() {
    let mut fake = Fake::start().await;
    let dir = prepare(None);
    let daemon = Daemon::start(dir.path(), &fake.url, |_| {}).await;

    let join = fake.join().await;
    assert_eq!(header(&join, "authorization"), format!("Bearer {JOIN_TOKEN}"));
    assert_eq!(header(&join, "x-hotbed-node-id"), NODE_ID);
    assert_eq!(header(&join, "x-hotbed-host"), NODE);
    assert_eq!(header(&join, "content-type"), "application/proto");
    assert_eq!(header(&join, "connect-protocol-version"), "1");
    let kept = dir.path().join("state/hotbed-node.json");
    let joined: Value = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
    assert_eq!(
        joined,
        json!({
            "controller": format!("{}/", fake.url),
            "node_id": NODE_ID,
            "cluster_id": CLUSTER,
            "worker_token": WORKER_TOKEN,
        })
    );
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777, 0o600);

    let (headers, beat) = fake.beat().await;
    assert_eq!(header(&headers, "authorization"), format!("Bearer {WORKER_TOKEN}"));
    assert_eq!(header(&headers, "x-hotbed-node-id"), NODE_ID);
    assert_eq!(beat.cluster_id, CLUSTER);
    let resources = beat.resources.unwrap();
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" };
    assert_eq!(resources.arch, arch);
    let cpus = std::thread::available_parallelism().unwrap().get();
    assert_eq!(resources.cpus, i32::try_from(cpus).unwrap());
    assert_eq!(resources.version, concat!("machined ", env!("CARGO_PKG_VERSION")));
    assert_eq!(resources.runtimes, Vec::<String>::new());
    assert_eq!(beat.reserved_memory_bytes, 0);

    let (headers, mut tunnel) = fake.tunnel().await;
    assert_eq!(header(&headers, "authorization"), format!("Bearer {WORKER_TOKEN}"));
    assert_eq!(header(&headers, "x-hotbed-node-id"), NODE_ID);
    assert_eq!(header(&headers, "x-hotbed-host"), NODE);

    // The controller's keepalive, which Cloudflare answers for it, is no call.
    tunnel.ws.send(Message::text("ping")).await.unwrap();

    // A unary call: its answer, and Go's omitempty JSON (no code, message or retryable; and no
    // body, as an empty list's protobuf is no bytes).
    let id = tunnel.call("ListWorkspaces", &pb::ListWorkspacesRequest::default(), false).await;
    let up = tunnel.up().await;
    let keys: Vec<_> = up.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "type"]);
    assert_eq!((up["type"].as_str(), up["id"].as_str()), (Some("rpc_msg"), Some(id.as_str())));
    assert_eq!(decode::<pb::ListWorkspacesResponse>(&up), pb::ListWorkspacesResponse::default());

    // Errors, with hotbed's codes and worker's messages.
    let missing = pb::GetWorkspaceRequest { id: "missing".into() };
    let up = tunnel.unary::<pb::Workspace>("GetWorkspace", &missing).await.unwrap_err();
    assert_eq!(refusal(&up), ("NOT_FOUND", "no workspace missing on this node"));
    let keys: Vec<_> = up.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["code", "id", "message", "type"]);
    let bad = pb::GetWorkspaceRequest { id: "../x".into() };
    let up = tunnel.unary::<pb::Workspace>("GetWorkspace", &bad).await.unwrap_err();
    assert_eq!(
        refusal(&up),
        (
            "INVALID_ARGS",
            "invalid ID \"../x\": must start with an ASCII letter or digit and contain only \
             letters, digits, '-' or '_'"
        )
    );
    let up = tunnel.unary::<pb::FileContent>("GetFile", &pb::GetFileRequest::default()).await;
    let up = up.unwrap_err();
    assert_eq!(
        refusal(&up),
        ("PLATFORM_NOT_SUPPORTED", "hotbed.v1.WorkspaceService.GetFile is not implemented")
    );
    let (none, up) = tunnel.stream::<pb::SyncResponse>("Sync", &pb::SyncRequest::default()).await;
    assert!(none.is_empty());
    assert_eq!(
        refusal(&up),
        ("PLATFORM_NOT_SUPPORTED", "hotbed.v1.WorkspaceService.Sync is not implemented")
    );
    let up = tunnel.unary::<pb::Workspace>("NoSuchCall", &missing).await.unwrap_err();
    assert_eq!(refusal(&up), ("INTERNAL", "404 page not found"));
    tunnel
        .send(json!({"type": "rpc_req", "id": "other", "procedure": "/hotbed.v1.ClusterService/Join", "body": ""}))
        .await;
    let up = tunnel.up().await;
    assert_eq!(
        (up["id"].as_str(), refusal(&up)),
        (Some("other"), ("INTERNAL", "404 page not found"))
    );
    // A unary call asked for as a stream, and a stream as a unary call: Connect's 415.
    let (none, up) = tunnel.stream::<pb::Workspace>("GetWorkspace", &missing).await;
    assert!(none.is_empty());
    assert_eq!(refusal(&up), ("INTERNAL", "415 Unsupported Media Type"));
    let follow = pb::FollowLogRequest { id: "nope".into(), ..Default::default() };
    let up = tunnel.unary::<pb::LogChunk>("FollowLog", &follow).await.unwrap_err();
    assert_eq!(refusal(&up), ("INTERNAL", "415 Unsupported Media Type"));
    // A body not base64, and one not the request's protobuf.
    let procedure = "/hotbed.v1.WorkspaceService/GetWorkspace";
    tunnel
        .send(json!({"type": "rpc_req", "id": "b64", "procedure": procedure, "body": "!!"}))
        .await;
    let up = tunnel.up().await;
    assert_eq!(up["id"], "b64");
    assert_eq!(refusal(&up).0, "INVALID_ARGS");
    assert!(refusal(&up).1.starts_with("invalid base64 request body: "), "{up}");
    tunnel
        .send(json!({"type": "rpc_req", "id": "pb", "procedure": procedure, "body": b64(&[0xff])}))
        .await;
    let up = tunnel.up().await;
    assert_eq!(up["id"], "pb");
    assert_eq!(refusal(&up).0, "INVALID_ARGS");
    assert!(refusal(&up).1.starts_with("unmarshal into hotbed.v1.GetWorkspaceRequest: "), "{up}");
    // A malformed frame, a frame of another type, and a cancel of no call: nothing.
    tunnel.ws.send(Message::text("{not json")).await.unwrap();
    tunnel.send(json!({"type": "rpc_later", "id": "x"})).await;
    tunnel.send(json!({"type": "rpc_cancel", "id": "x"})).await;
    // Streams: an error before any answer.
    let (none, up) = tunnel.stream::<pb::LogChunk>("FollowLog", &follow).await;
    assert!(none.is_empty());
    assert_eq!(refusal(&up), ("NOT_FOUND", "no command nope on this node"));
    let watch = pb::WatchWorkspaceRequest { id: "missing".into() };
    let (none, up) = tunnel.stream::<pb::Workspace>("WatchWorkspace", &watch).await;
    assert!(none.is_empty());
    assert_eq!(refusal(&up), ("NOT_FOUND", "no workspace missing on this node"));
    // Commands, by session alone, as hotbed's worker lists them.
    let listed: pb::ListCommandsResponse =
        tunnel.unary("ListCommands", &pb::ListCommandsRequest::default()).await.unwrap();
    assert_eq!(listed, pb::ListCommandsResponse::default());
    let history = pb::ListCommandsRequest { ended: true, ..Default::default() };
    let up = tunnel.unary::<pb::ListCommandsResponse>("ListCommands", &history).await.unwrap_err();
    assert_eq!(
        refusal(&up),
        (
            "INVALID_ARGS",
            "a worker lists its commands by session alone: ask the controller for history, \
             projects, matches or limits"
        )
    );
    daemon.shutdown().await;

    // Started again, with no token to join with: it joined, and takes that up.
    std::fs::remove_file(dir.path().join("token")).unwrap();
    let daemon = Daemon::start(dir.path(), &fake.url, |_| {}).await;
    let (headers, beat) = fake.beat().await;
    assert_eq!(header(&headers, "authorization"), format!("Bearer {WORKER_TOKEN}"));
    assert_eq!(beat.cluster_id, CLUSTER);
    let (_, mut tunnel) = fake.tunnel().await;
    let listed: pb::ListWorkspacesResponse =
        tunnel.unary("ListWorkspaces", &pb::ListWorkspacesRequest::default()).await.unwrap();
    assert_eq!(listed, pb::ListWorkspacesResponse::default());
    assert!(fake.joins.try_recv().is_err(), "a node joined joins no more");
    daemon.shutdown().await;
}

/// A join the controller refuses fails machined's start, saying why in hotbed's words, and leaves
/// nothing behind: no socket, nothing kept of a join.
#[tokio::test]
async fn a_refused_join_fails_the_start() {
    let mut fake = Fake::start().await;
    fake.shared.refuse.store(true, Ordering::SeqCst);
    let dir = prepare(None);
    let Err(e) = machined::start(config(dir.path(), &fake.url, |_| {})).await else {
        panic!("a refused join started machined");
    };
    assert_eq!(
        e.to_string(),
        format!("hotbed: joining {}/: NOT_AUTHORIZED: bad token (mint a join token)", fake.url)
    );
    fake.join().await;
    assert!(!dir.path().join("machined.sock").exists());
    assert!(!dir.path().join("state/hotbed-node.json").exists());
}

/// A controller that goes silent, its socket half-open, is left after the read timeout, and the
/// node opens another tunnel; one the controller closes, too.
#[tokio::test]
async fn a_silent_controller_is_left_for_another_tunnel() {
    let mut fake = Fake::start().await;
    let dir = prepare(None);
    let tune = |hotbed: &mut Hotbed| {
        hotbed.ping_interval = Duration::from_millis(40);
        hotbed.read_timeout = Duration::from_millis(300);
    };
    let daemon = Daemon::start(dir.path(), &fake.url, tune).await;
    let (_, mut silent) = fake.tunnel().await;
    // It pings, and is never answered.
    let ping = tokio::time::timeout(WAIT, silent.ws.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(ping, Message::text("ping"));
    let started = std::time::Instant::now();
    let (_, mut second) = fake.tunnel().await;
    // The read timeout, then the first backoff (1 s).
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let listed: pb::ListWorkspacesResponse =
        second.unary("ListWorkspaces", &pb::ListWorkspacesRequest::default()).await.unwrap();
    assert_eq!(listed, pb::ListWorkspacesResponse::default());
    second.ws.close(None).await.unwrap();
    let (_, mut third) = fake.tunnel().await;
    let listed: pb::ListWorkspacesResponse =
        third.unary("ListWorkspaces", &pb::ListWorkspacesRequest::default()).await.unwrap();
    assert_eq!(listed, pb::ListWorkspacesResponse::default());
    drop(silent);
    daemon.shutdown().await;
}

/// Workspaces are machines and commands execs in them, down the tunnel: made, run in, their
/// output followed to its end, a follow canceled, a command stopped, its log tailed, watched, and
/// released as a heartbeat says.
#[tokio::test]
async fn workspaces_are_machines() {
    let Some((kernel, initramfs)) = common::guest() else { return };
    let mut fake = Fake::start().await;
    let dir = prepare(Some((&kernel, &initramfs)));
    let daemon = Daemon::start(dir.path(), &fake.url, |hotbed| {
        hotbed.report_interval = Duration::from_millis(200);
    })
    .await;
    let (_, mut tunnel) = fake.tunnel().await;

    let create = pb::CreateWorkspaceRequest {
        id: "ws-1".into(),
        session_id: "s-1".into(),
        agent: "claude".into(),
        ..Default::default()
    };
    let ws: pb::Workspace = tunnel.unary("CreateWorkspace", &create).await.unwrap();
    assert_eq!(
        (ws.id.as_str(), ws.node_id.as_str(), ws.state(), ws.session_id.as_str()),
        ("ws-1", NODE_ID, pb::WorkspaceState::Active, "s-1")
    );
    assert_eq!((ws.agent.as_str(), ws.runtime.as_str()), ("claude", "machined"));
    // Again: the same workspace.
    let again: pb::Workspace = tunnel.unary("CreateWorkspace", &create).await.unwrap();
    assert_eq!(again, ws);
    let listed: pb::ListWorkspacesResponse =
        tunnel.unary("ListWorkspaces", &pb::ListWorkspacesRequest::default()).await.unwrap();
    assert_eq!(listed.workspaces, std::slice::from_ref(&ws));
    // Heartbeats say what its machine holds.
    loop {
        let (_, beat) = fake.beat().await;
        if beat.reserved_memory_bytes == i64::try_from(WORKSPACE_MEMORY).unwrap() {
            break;
        }
    }
    // A second workspace runs beside it, a machine of its own in a helper of its own (board
    // 13460: on Hypervisor.framework too).
    let second = pb::CreateWorkspaceRequest { id: "ws-2".into(), ..Default::default() };
    let ws2: pb::Workspace = tunnel.unary("CreateWorkspace", &second).await.unwrap();
    assert_eq!(ws2.state(), pb::WorkspaceState::Active);
    let echo = pb::RunCommandRequest {
        workspace_id: "ws-2".into(),
        id: "c-0".into(),
        args: vec!["echo".into(), "two".into()],
        ..Default::default()
    };
    tunnel.unary::<pb::Command>("RunCommand", &echo).await.unwrap();
    let follow = pb::FollowLogRequest { id: "c-0".into(), follow: true, ..Default::default() };
    let (chunks, end) = tunnel.stream::<pb::LogChunk>("FollowLog", &follow).await;
    assert_eq!(end["type"], "rpc_end", "{end}");
    let output: Vec<u8> = chunks.iter().flat_map(|c| c.data.clone()).collect();
    assert_eq!(output, b"two\n");
    let release = pb::ReleaseWorkspaceRequest { id: "ws-2".into() };
    tunnel.unary::<pb::ReleaseWorkspaceResponse>("ReleaseWorkspace", &release).await.unwrap();

    // A command: its output followed to its end, which says how it ended.
    let run = |id: &str, script: &str| pb::RunCommandRequest {
        workspace_id: "ws-1".into(),
        id: id.into(),
        args: vec!["sh".into(), "-c".into(), script.into()],
        env: vec!["GREETING=hello".into()],
        kind: "test".into(),
        ..Default::default()
    };
    let started: pb::Command =
        tunnel.unary("RunCommand", &run("c-1", "echo $GREETING; echo two; exit 3")).await.unwrap();
    assert_eq!(
        (started.id.as_str(), started.workspace_id.as_str(), started.session_id.as_str()),
        ("c-1", "ws-1", "s-1")
    );
    assert_eq!(started.argv, "sh -c 'echo $GREETING; echo two; exit 3'");
    let follow = pb::FollowLogRequest { id: "c-1".into(), follow: true, ..Default::default() };
    let (chunks, end) = tunnel.stream::<pb::LogChunk>("FollowLog", &follow).await;
    assert_eq!(end["type"], "rpc_end", "{end}");
    let output: Vec<u8> = chunks.iter().flat_map(|c| c.data.clone()).collect();
    assert_eq!(String::from_utf8(output).unwrap(), "hello\ntwo\n");
    let last = chunks.last().unwrap();
    assert_eq!((last.next_offset, last.state()), (10, pb::CommandState::Failed));
    let ended: pb::Command =
        tunnel.unary("GetCommand", &pb::GetCommandRequest { id: "c-1".into() }).await.unwrap();
    assert_eq!((ended.state(), ended.exit_code), (pb::CommandState::Failed, Some(3)));
    // From an offset, not following: what is there from it, and no end chunk.
    let rest =
        pb::FollowLogRequest { id: "c-1".into(), offset: 6, follow: false, ..Default::default() };
    let (chunks, end) = tunnel.stream::<pb::LogChunk>("FollowLog", &rest).await;
    assert_eq!(end["type"], "rpc_end");
    assert_eq!(
        chunks,
        [pb::LogChunk { data: b"two\n".to_vec(), next_offset: 10, ..Default::default() }]
    );
    let tail = pb::GetLogLinesRequest { id: "c-1".into(), tail: 1 };
    let log: pb::CommandLog = tunnel.unary("GetLogLines", &tail).await.unwrap();
    assert_eq!(
        log,
        pb::CommandLog { command_id: "c-1".into(), lines: vec!["two".into()], cut: false }
    );

    // A command that runs on: a follow of it canceled, then the command stopped.
    let long: pb::Command =
        tunnel.unary("RunCommand", &run("c-2", "echo up; sleep 600")).await.unwrap();
    assert_eq!(long.state(), pb::CommandState::Running);
    let follow = pb::FollowLogRequest { id: "c-2".into(), follow: true, ..Default::default() };
    let id = tunnel.call("FollowLog", &follow, true).await;
    let up = tunnel.up().await;
    assert_eq!((up["type"].as_str(), up["id"].as_str()), (Some("rpc_msg"), Some(id.as_str())));
    assert_eq!(decode::<pb::LogChunk>(&up).data, b"up\n");
    tunnel.send(json!({"type": "rpc_cancel", "id": id})).await;
    let up = tunnel.up().await;
    assert_eq!(up["id"], id.as_str());
    assert_eq!(refusal(&up), ("INTERNAL", "context canceled"));
    let stop =
        pb::StopCommandRequest { id: "c-2".into(), comment: "enough".into(), ..Default::default() };
    let stopped: pb::Command = tunnel.unary("StopCommand", &stop).await.unwrap();
    assert_eq!((stopped.state(), stopped.reason.as_str()), (pb::CommandState::Cancelled, "enough"));
    let session = pb::ListCommandsRequest { session_id: "s-1".into(), ..Default::default() };
    let listed: pb::ListCommandsResponse = tunnel.unary("ListCommands", &session).await.unwrap();
    let ids: Vec<_> = listed.commands.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c-1", "c-2"]);
    let other = pb::ListCommandsRequest { session_id: "s-2".into(), ..Default::default() };
    let listed: pb::ListCommandsResponse = tunnel.unary("ListCommands", &other).await.unwrap();
    assert_eq!(listed.commands, []);

    // Watched, then released as a heartbeat's answer says: the watch sees it go.
    let watch = pb::WatchWorkspaceRequest { id: "ws-1".into() };
    let watching = tunnel.call("WatchWorkspace", &watch, true).await;
    let up = tunnel.up().await;
    assert_eq!(up["id"], watching.as_str());
    assert_eq!(decode::<pb::Workspace>(&up).state(), pb::WorkspaceState::Active);
    fake.shared.release.lock().unwrap().push("ws-1".into());
    let up = tunnel.up().await;
    assert_eq!(up["id"], watching.as_str());
    assert_eq!(decode::<pb::Workspace>(&up).state(), pb::WorkspaceState::Released);
    let up = tunnel.up().await;
    assert_eq!(
        (up["type"].as_str(), up["id"].as_str()),
        (Some("rpc_end"), Some(watching.as_str()))
    );
    let gone = pb::GetWorkspaceRequest { id: "ws-1".into() };
    let up = tunnel.unary::<pb::Workspace>("GetWorkspace", &gone).await.unwrap_err();
    assert_eq!(refusal(&up), ("NOT_FOUND", "no workspace ws-1 on this node"));
    let up = tunnel.unary::<pb::Command>("RunCommand", &run("c-3", "true")).await.unwrap_err();
    assert_eq!(refusal(&up), ("NOT_FOUND", "no workspace ws-1 on this node"));
    daemon.shutdown().await;
}
