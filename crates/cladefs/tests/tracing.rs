// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-258: the daemon's binary exports its spans over OTLP, to the collector
//! `OTEL_EXPORTER_OTLP_ENDPOINT` names, with `CLADEFSD_TRACES=otlp`; and sends nothing without, nor
//! with `CLADEFSD_TRACES=off` (TASK-707), which makes no spans at all. The collector is one of the
//! test's own, which takes OTLP over HTTP as a real one does (`docs/tracing.md` runs Jaeger's), and
//! keeps what it is sent.
//!
//! cladefs: board 8569: over HTTPS too, the exporter's TLS its own; within the drain's bound however
//! its collector fails to answer; and a control call's W3C trace context the parent of the daemon's
//! span of it.

// cladefs: the daemon's, on Linux and macOS (board 13065).
#![cfg(not(madsim))]

use cladefs::{CellClient, ExtentId, OpenCell, VolatileSlice};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A collector of OTLP over HTTP, on a thread: its endpoint, and the bodies of the exports it
/// takes, each acknowledged, as they come.
fn collector() -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
    collector_over(None)
}

/// cladefs: [`collector`], over HTTPS if `tls` is given, the TLS of a server so configured, at
/// `https://localhost:PORT` (board 8569).
fn collector_over(
    tls: Option<Arc<rustls::ServerConfig>>,
) -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let endpoint = match tls {
        Some(_) => format!("https://localhost:{port}"),
        None => format!("http://127.0.0.1:{port}"),
    };
    let (sent, exports) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            let served = match &tls {
                None => serve_http(stream, &sent),
                Some(tls) => {
                    let connection = rustls::ServerConnection::new(tls.clone()).unwrap();
                    serve_http(rustls::StreamOwned::new(connection, stream), &sent)
                }
            };
            if served.is_err() {
                return;
            }
        }
    });
    (endpoint, exports)
}

/// cladefs: the TLS of a collector of the tests' own CA (`tests/otlp-tls`), for `localhost` and
/// `127.0.0.1`.
#[cfg(target_os = "linux")]
fn collector_tls() -> Arc<rustls::ServerConfig> {
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/otlp-tls");
    let certs = CertificateDer::pem_file_iter(dir.join("collector.crt")).unwrap();
    let certs = certs.collect::<Result<Vec<_>, _>>().unwrap();
    let key = PrivateKeyDer::from_pem_file(dir.join("collector.key")).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    Arc::new(config)
}

/// cladefs: serves OTLP over HTTP/1.1 on `stream`, a connection, its exports sent to `sent`, each
/// acknowledged, until the client closes it: `Err` once the test has gone.
fn serve_http(stream: impl Read + Write, sent: &mpsc::Sender<(String, Vec<u8>)>) -> Result<(), ()> {
    let mut stream = BufReader::new(stream);
    loop {
        let mut line = String::new();
        // Closed, or cut, as a client closes TLS without saying so: the connection's end.
        if !matches!(stream.read_line(&mut line), Ok(1..)) {
            return Ok(());
        }
        let target = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
        let mut len = 0;
        loop {
            let mut header = String::new();
            stream.read_line(&mut header).unwrap();
            if header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                len = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        stream.read_exact(&mut body).unwrap();
        // Kept before it is acknowledged, which the daemon may exit on.
        if sent.send((target, body)).is_err() {
            return Err(());
        }
        let ok = "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\n\
                  content-length: 0\r\n\r\n";
        stream.get_mut().write_all(ok.as_bytes()).unwrap();
        stream.get_mut().flush().unwrap();
    }
}

/// cladefs: a collector that takes connections and never answers, nor reads them, as one hung or
/// behind a black hole: its endpoint (board 8569).
fn unanswering() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        // Held open, unread, for as long as the test runs.
        let held: Vec<TcpStream> = listener.incoming().map_while(Result::ok).collect();
        drop(held);
    });
    endpoint
}

/// A daemon serving at `socket`, its cells in a bucket in its memory, as `traces` says
/// of its spans, OpenTelemetry's variables naming `endpoint` the collector, every trace kept;
/// cladefs: its environment `env` besides.
fn daemon(socket: &Path, traces: Option<&str>, endpoint: &str, env: &[(&str, &str)]) -> KillOnDrop {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command
        .arg(socket)
        .env("CLADEFSD_ROOTS", socket.parent().unwrap())
        .env("CLADEFSD_BUCKET", "memory:///cells")
        .env("CLADEFSD_TOKEN", "1")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
        .env("OTEL_TRACES_SAMPLER", "always_on")
        .env_remove("OTEL_SERVICE_NAME")
        .envs(env.iter().copied());
    match traces {
        Some(traces) => command.env("CLADEFSD_TRACES", traces),
        None => command.env_remove("CLADEFSD_TRACES"),
    };
    let daemon = KillOnDrop(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

/// Has `daemon` write, commit and close a cell in `dir`, then drains it with SIGTERM, which
/// exports the spans it has not yet: what it was sent, in order, once it exited.
fn serve_and_drain(dir: &Path, traces: Option<&str>) -> Vec<(String, Vec<u8>)> {
    let (endpoint, exports) = collector();
    let mut daemon = daemon(&dir.join("cladefsd.sock"), traces, &endpoint, &[]);
    write_and_commit(dir);
    drained(&mut daemon);
    exports.try_iter().collect()
}

/// cladefs: has the daemon serving in `dir` write, commit and close a new cell, as a VM would.
fn write_and_commit(dir: &Path) {
    let client = CellClient::connect_new(dir.join("cladefsd.sock"), 0, 1 << 28).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    client.write(extent, 0, &[VolatileSlice::from(&[7; BLOCK][..])]).unwrap();
    client.commit(Some(b"traced")).unwrap();
}

/// cladefs: drains `daemon` with SIGTERM, and waits for it to exit, drained: how long it took.
fn drained(daemon: &mut KillOnDrop) -> Duration {
    let began = Instant::now();
    // SAFETY: Signals the daemon this test started.
    assert_eq!(unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) }, 0);
    let status = daemon.0.wait().unwrap();
    let took = began.elapsed();
    assert!(status.success(), "{status}");
    took
}

fn holds(body: &[u8], text: &str) -> bool {
    body.windows(text.len()).any(|window| window == text.as_bytes())
}

#[test]
fn the_daemon_exports_its_spans_over_otlp() {
    let dir = tempfile::tempdir().unwrap();
    let exports = serve_and_drain(dir.path(), Some("otlp"));
    let targets: BTreeSet<&str> = exports.iter().map(|(target, _)| target.as_str()).collect();
    assert_eq!(targets, BTreeSet::from(["/v1/traces"]));
    let body: Vec<u8> = exports.into_iter().flat_map(|(_, body)| body).collect();
    let missing: Vec<&str> = [
        "cladefsd",
        "cladefsd.open",
        "cladefsd.write",
        "cladefsd.commit",
        "store.create",
        "store.commit",
        "commit.seal",
        "commit.write",
        "store.flush",
        "commit.upload_wait",
        "upload.segment",
        "bucket.create",
        "cladefs.request.kind",
        "cladefs.cell",
        "cladefs.bytes",
    ]
    .into_iter()
    .filter(|name| !holds(&body, name))
    .collect();
    assert_eq!(missing, Vec::<&str>::new(), "not exported");
}

#[test]
fn the_daemon_exports_nothing_unless_told_to() {
    let dir = tempfile::tempdir().unwrap();
    let exports = serve_and_drain(dir.path(), None);
    assert_eq!(exports.len(), 0);
}

/// TASK-707: `off` makes no spans, so sends none; the daemon serves and drains as it does with
/// them.
#[test]
fn the_daemon_exports_nothing_with_traces_off() {
    let dir = tempfile::tempdir().unwrap();
    let exports = serve_and_drain(dir.path(), Some("off"));
    assert_eq!(exports.len(), 0);
}

/// cladefs: board 8569: an `https://` collector takes the daemon's spans, its certificate of a CA
/// `SSL_CERT_FILE` names: the exporter's TLS is the feature this crate names, which before came
/// only from another crate's turning reqwest's on.
// cladefs: macOS's verifier refuses the tests' certificate (board 13592).
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_exports_its_spans_over_https() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, exports) = collector_over(Some(collector_tls()));
    let ca = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/otlp-tls/ca.crt");
    let ca = ca.to_str().unwrap();
    let env = [("SSL_CERT_FILE", ca)];
    let mut daemon = daemon(&dir.path().join("cladefsd.sock"), Some("otlp"), &endpoint, &env);
    write_and_commit(dir.path());
    drained(&mut daemon);
    let exports: Vec<_> = exports.try_iter().collect();
    let targets: BTreeSet<&str> = exports.iter().map(|(target, _)| target.as_str()).collect();
    assert_eq!(targets, BTreeSet::from(["/v1/traces"]));
    let body: Vec<u8> = exports.into_iter().flat_map(|(_, body)| body).collect();
    assert!(holds(&body, "cladefsd.commit"), "no commit's span exported");
}

/// cladefs: board 8569: a daemon whose collector never answers exits within its drain's bound
/// (`CLADEFSD_DRAIN`), drained: its last spans' export gets what the drain leaves of it, where
/// before the SDK's own timeouts held it 5 s and more past it, past an orchestrator's patience.
#[test]
fn a_daemon_whose_collector_never_answers_exits_within_its_drain() {
    let dir = tempfile::tempdir().unwrap();
    let bound = Duration::from_secs(2);
    let env = [("CLADEFSD_DRAIN", "2")];
    let socket = dir.path().join("cladefsd.sock");
    let mut daemon = daemon(&socket, Some("otlp"), &unanswering(), &env);
    write_and_commit(dir.path());
    let took = drained(&mut daemon);
    assert!(
        took < bound + Duration::from_secs(1),
        "exited {took:?} after SIGTERM, its bound {bound:?}"
    );
}

/// cladefs: board 8569: a control call's W3C trace context, as a VM manager sends it, is the parent
/// of the daemon's span of the call, `cladefs.cladefsd.v1.Cladefsd/GetCounters`, exported on the
/// caller's trace: before, every trace the daemon made began at its door.
#[test]
fn a_control_calls_trace_context_is_the_parent_of_its_span() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, exports) = collector();
    let control = dir.path().join("control.sock");
    let env = [("CLADEFSD_GRPC", control.to_str().unwrap())];
    let mut daemon = daemon(&dir.path().join("cladefsd.sock"), Some("otlp"), &endpoint, &env);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&control).is_err() {
        assert!(Instant::now() < deadline, "the control API never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    let trace: [u8; 16] = *b"cladefs-trace-id";
    let parent: [u8; 8] = *b"vmmspan!";
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let traceparent = format!("00-{}-{}-01", hex(&trace), hex(&parent));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let mut api = cladefs::cladefsd::control::connect(&control).await.unwrap();
        let mut request =
            tonic::Request::new(cladefs::cladefsd::control::proto::GetCountersRequest {});
        request.metadata_mut().insert("traceparent", traceparent.parse().unwrap());
        api.get_counters(request).await.unwrap();
    });
    drained(&mut daemon);
    let body: Vec<u8> = exports.try_iter().flat_map(|(_, body)| body).collect();
    let holds_bytes = |bytes: &[u8]| body.windows(bytes.len()).any(|window| window == bytes);
    assert!(holds(&body, "cladefs.cladefsd.v1.Cladefsd/GetCounters"), "no span of the call");
    assert!(holds_bytes(&trace), "not on the caller's trace");
    assert!(holds_bytes(&parent), "not a child of the caller's span");
}
