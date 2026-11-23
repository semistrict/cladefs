// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A fake of an HTTP service, in this process, for the tests of the KMS backends over HTTP (Google
//! Cloud KMS, Azure Key Vault and its authority): each request answered as a test's handler says,
//! on a runtime of the server's own, which outlives every runtime its clients' calls are made on,
//! dropped or not (board 8477).

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A request, as the fake read it.
pub struct Request {
    pub method: String,
    /// Its path and query, as sent.
    pub path: String,
    /// Its headers, a line each, `name: value`, the names in lowercase, the values as sent.
    pub headers: String,
    pub body: Vec<u8>,
}

impl Request {
    /// The value of its header `name` (lowercase), if it has one.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))
    }

    /// Its path, without its query.
    pub fn route(&self) -> &str {
        self.path.split_once('?').map_or(&self.path[..], |(route, _)| route)
    }
}

/// What the fake answers a request with: its status, extra headers and JSON body, after `delay`.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub delay: Duration,
}

impl Response {
    /// `body`, with `status`, at once.
    pub fn of(status: u16, body: impl Into<String>) -> Self {
        Self { status, headers: Vec::new(), body: body.into(), delay: Duration::ZERO }
    }

    /// The same, with the header `name: value`.
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The same, answered after `delay`.
    pub fn after(self, delay: Duration) -> Self {
        Self { delay, ..self }
    }
}

/// A test's handler: the base URL the fake serves at, and a request, to the answer.
type Handler = dyn Fn(&str, &Request) -> Response + Send + Sync;

/// The fake, serving on a port of this host's until dropped.
pub struct Server {
    base: String,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Server {
    /// The fake, each request answered as `handler` says.
    pub fn start(handler: impl Fn(&str, &Request) -> Response + Send + Sync + 'static) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fake-http")
            .enable_all()
            .build()
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler: Arc<Handler> = Arc::new(handler);
        let served = base.clone();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(connection(stream, handler.clone(), served.clone()));
            }
        });
        Self { base, runtime: Some(runtime) }
    }

    /// Its base URL, `http://127.0.0.1:PORT`.
    pub fn base(&self) -> &str {
        &self.base
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Serves the requests of one connection, kept alive, until its client closes it.
async fn connection(mut stream: tokio::net::TcpStream, handler: Arc<Handler>, base: String) {
    let mut buffer = Vec::new();
    loop {
        let end = loop {
            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break at;
            }
            let mut chunk = [0; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let original = String::from_utf8_lossy(&buffer[..end]).into_owned();
        let mut words = original.split_whitespace();
        let (method, path) = (words.next().unwrap().to_owned(), words.next().unwrap().to_owned());
        // Header names in lowercase, values as sent: a bearer's token is case-sensitive.
        let headers: String = original
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(": "))
            .map(|(name, value)| format!("{}: {value}\n", name.to_lowercase()))
            .collect();
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .map_or(0, |n| n.trim().parse().unwrap());
        while buffer.len() < end + 4 + length {
            let mut chunk = [0; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let body = buffer[end + 4..end + 4 + length].to_vec();
        buffer.drain(..end + 4 + length);
        let request = Request { method, path, headers, body };
        let response = handler(&base, &request);
        tokio::time::sleep(response.delay).await;
        let extra: String =
            response.headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
        let written = format!(
            "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}\r\n{}",
            response.status,
            response.body.len(),
            response.body
        );
        if stream.write_all(written.as_bytes()).await.is_err() {
            return;
        }
    }
}
