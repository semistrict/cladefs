// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the HTTP client of buckets in the cloud (TASK-205), which [`crate::object_bucket`]
//! gives `object_store`'s S3, GCS and Azure backends. It is `object_store`'s own, reqwest, built
//! from the same options as `object_store` builds it, but for how a request times out.
//!
//! `object_store`'s gives a request 30 s, its body included. A host's uploads, up to 512 MiB of
//! single PUTs at once, share its upstream: below 18 MB/s they all ran out of time together, part
//! sent, and each went again from its first byte, for ever. Here a request runs as long as its
//! bytes move, and times out once they stall: none of its body taken by the connection, nor any
//! of its response read, for [`Client::stall`]. A body is handed to the connection in slices of
//! [`SLICE`] bytes, so that taking each is progress.
//!
//! What the connection's socket buffers it takes, though the link has not carried it yet: its
//! last bytes may wait there, moving nothing, for as long as the link takes to carry the buffer.
//! Over a link that limits the bandwidth the buffer grows with the congestion window, a couple of
//! round trips' and the link's queue's worth: seconds at worst, well within the stall time. A
//! peer that reads slowly lets it grow to the kernel's bound, 4 MiB on Linux, which over
//! loopback, whose segments are 64 KiB, it does at once: a request's share of the bandwidth must
//! carry that within the stall time (the tests' S3 server's reads, `object_bucket::tests`).

use bytes::Bytes;
use futures::future::Either;
use hyper::body::{Body, Frame, SizeHint};
use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpRequestBody,
    HttpResponse, HttpResponseBody,
};
use object_store::{ClientConfigKey, ClientOptions};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::time::{Instant, Sleep};

/// The slices a request's body goes to its connection in.
pub const SLICE: usize = 16 << 10;

/// The client of buckets: how long a connection may take to open, and a request to move no bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Client {
    pub connect: Duration,
    pub stall: Duration,
}

impl Client {
    /// `object_store`'s connect timeout; S3 closes a connection that moved nothing for 20 s.
    pub const DEFAULT: Self =
        Self { connect: Duration::from_secs(5), stall: Duration::from_secs(30) };

    /// The options it builds backends with, which the environment's may add to: its connect
    /// timeout, and no timeout of a whole request.
    pub(crate) fn options(&self) -> ClientOptions {
        ClientOptions::new().with_connect_timeout(self.connect).with_timeout_disabled()
    }
}

/// cladefs: what connects a bucket's backend: [`Client`]'s, the date of each response to a request
/// of the bucket's own heard by its clock (`Date`, [`crate::clock::Served`], board 8467): one that
/// names `bucket` ([`names`]). Its credentials' requests go through it too, to services of clocks
/// of their own (an instance's metadata, STS, OAuth), whose dates it hears not.
#[derive(Debug)]
pub(crate) struct Connector {
    pub client: Client,
    pub clock: Arc<crate::clock::Served>,
    /// The bucket's name, or its container's: what its own requests name.
    pub bucket: String,
}

impl HttpConnector for Connector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let client = reqwest_client(options)?;
        let (stall, clock) = (self.client.stall, self.clock.clone());
        let bucket = self.bucket.clone();
        Ok(HttpClient::new(Watched { client, stall, clock, bucket }))
    }
}

/// Whether a request to `url` is of `bucket`'s own, as its backend's requests are: naming it as its
/// host's first label (virtual-hosted, `bucket.s3.region.amazonaws.com`), or one of its path's first
/// two segments (path-style S3's and GCS's first, Azure's container, the emulator's second, after
/// its account), and at no link-local address, an instance's metadata service's (whose paths begin
/// `/latest/`, a bucket's name as well as any). Its credentials' requests, to services of their
/// own, name it nowhere.
pub(crate) fn names(bucket: &str, url: &reqwest::Url) -> bool {
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_link_local(),
        // EC2's metadata service's, fd00:ec2::254, and link-local addresses.
        Some(url::Host::Ipv6(ip)) => {
            let segments = ip.segments();
            segments[..2] == [0xfd00, 0xec2] || segments[0] & 0xffc0 == 0xfe80
        }
        _ => false,
    };
    let labelled = url.host_str().and_then(|host| host.split('.').next()) == Some(bucket);
    let pathed = url.path_segments().is_some_and(|path| path.take(2).any(|at| at == bucket));
    !local && (labelled || pathed)
}

/// cladefs: the write-offset header of an append PUT through a [`Connector`]'s client (doc-29):
/// S3 Express One Zone's `x-amz-write-offset-bytes`, the object's path in its bucket, and whether
/// a request carried it.
#[derive(Debug)]
pub(crate) struct AppendHeader {
    pub header: (reqwest::header::HeaderName, reqwest::header::HeaderValue),
    pub path: String,
    sent: std::sync::atomic::AtomicBool,
}

tokio::task_local! {
    /// The write-offset header of the append [`appending`] runs: what its PUT requests carry
    /// (doc-29).
    static APPEND: AppendHeader;
}

/// cladefs: runs `put`, an append PUT through a [`Connector`]'s client of the object at `path`,
/// its PUT request carrying `header` (`x-amz-write-offset-bytes`, doc-29); and whether a request
/// carried it.
pub(crate) async fn appending<T>(
    header: (reqwest::header::HeaderName, reqwest::header::HeaderValue),
    path: String,
    put: impl Future<Output = T>,
) -> (T, bool) {
    let append = AppendHeader { header, path, sent: std::sync::atomic::AtomicBool::new(false) };
    APPEND
        .scope(append, async {
            let written = put.await;
            (written, APPEND.with(|append| append.sent.load(Ordering::Relaxed)))
        })
        .await
}

/// Decodes `%XX` escapes in a URL path segment without external dependencies.
fn percent_decode_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Puts the write-offset header of the append under way in this task, if any, on `request`, if it
/// is a PUT of the append's path (doc-29).
fn append_header(request: &mut reqwest::Request) {
    if request.method() != reqwest::Method::PUT {
        return;
    }
    let path = percent_decode_path(request.url().path());
    // No append active in this task: nothing to add.
    let _ = APPEND.try_with(|append| {
        if path.strip_suffix(append.path.as_str()).is_some_and(|rest| rest.ends_with('/')) {
            let (name, value) = append.header.clone();
            request.headers_mut().insert(name, value);
            append.sent.store(true, Ordering::Relaxed);
        }
    });
}
/// The user agent of a client whose options name none.
const USER_AGENT: &str = concat!("cladefs/", env!("CARGO_PKG_VERSION"));

/// Parses a human-readable duration (`"30s"`, `"0.5s"`, `"1m 30s"`, `"500ms"`, `"100us"`, `"2h"`,
/// etc.), matching `humantime::parse_duration` (including fractional components and unit aliases)
/// for `object_store`'s `ClientConfigKey` timeout options.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn parse_duration(input: &str) -> Result<Duration, String> {
    let bytes = input.as_bytes();
    let mut i: usize = 0;
    let mut total = Duration::ZERO;
    let mut any = false;
    while i < bytes.len() {
        while let Some(&b) = bytes.get(i) {
            if b.is_ascii_whitespace() {
                i = i.checked_add(1).ok_or_else(|| "duration too long".to_owned())?;
            } else {
                break;
            }
        }
        if i >= bytes.len() {
            break;
        }
        let num_start = i;
        while let Some(&b) = bytes.get(i) {
            if b.is_ascii_digit() {
                i = i.checked_add(1).ok_or_else(|| "duration too long".to_owned())?;
            } else {
                break;
            }
        }
        if i == num_start {
            return Err(format!("expected number in duration {input:?}"));
        }
        let whole: u64 = input
            .get(num_start..i)
            .ok_or_else(|| format!("invalid duration {input:?}"))?
            .parse()
            .map_err(|_| format!("duration number overflow in {input:?}"))?;
        let mut frac_num: u128 = 0;
        let mut frac_scale: u128 = 1;
        if bytes.get(i) == Some(&b'.') {
            i = i.checked_add(1).ok_or_else(|| "duration too long".to_owned())?;
            let frac_start = i;
            let mut kept: u32 = 0;
            while let Some(&b) = bytes.get(i) {
                if b.is_ascii_digit() {
                    if kept < 9 {
                        let digit = u128::from(b.saturating_sub(b'0'));
                        frac_num = frac_num
                            .checked_mul(10)
                            .and_then(|v| v.checked_add(digit))
                            .ok_or_else(|| "duration overflow".to_owned())?;
                        frac_scale = frac_scale
                            .checked_mul(10)
                            .ok_or_else(|| "duration overflow".to_owned())?;
                        kept = kept.checked_add(1).ok_or_else(|| "duration overflow".to_owned())?;
                    }
                    i = i.checked_add(1).ok_or_else(|| "duration too long".to_owned())?;
                } else {
                    break;
                }
            }
            if i == frac_start {
                return Err(format!("expected fractional digits in duration {input:?}"));
            }
        }
        while let Some(&b) = bytes.get(i) {
            if b.is_ascii_whitespace() {
                i = i.checked_add(1).ok_or_else(|| "duration too long".to_owned())?;
            } else {
                break;
            }
        }
        let unit_start = i;
        while let Some(&b) = bytes.get(i) {
            if !b.is_ascii_whitespace() && !b.is_ascii_digit() {
                i = i.checked_add(1).ok_or_else(|| "duration too long".to_owned())?;
            } else {
                break;
            }
        }
        let unit = input
            .get(unit_start..i)
            .ok_or_else(|| format!("invalid duration unit in {input:?}"))?;
        let unit_nanos: u128 = match unit {
            "nanos" | "nsec" | "ns" | "nanosec" | "nanosecs" | "nanosecond" | "nanoseconds" => 1,
            "usec" | "us" | "µs" | "μs" | "microsec" | "microsecs" | "microsecond"
            | "microseconds" => 1_000,
            "millis" | "msec" | "ms" | "millisec" | "millisecs" | "millisecond"
            | "milliseconds" => 1_000_000,
            "sec" | "secs" | "s" | "second" | "seconds" => 1_000_000_000,
            "min" | "mins" | "m" | "minute" | "minutes" => 60_000_000_000,
            "hr" | "hrs" | "h" | "hour" | "hours" => 3_600_000_000_000,
            "day" | "days" | "d" => 86_400_000_000_000,
            "wk" | "wks" | "week" | "weeks" | "w" => 604_800_000_000_000,
            // 30.44 days = 2_630_016 s, matching humantime.
            "month" | "months" | "M" => 2_630_016_000_000_000,
            // 365.25 days = 31_557_600 s, matching humantime.
            "yr" | "yrs" | "year" | "years" | "y" => 31_557_600_000_000_000,
            _ => return Err(format!("unknown duration unit {unit:?} in {input:?}")),
        };
        let whole_nanos = u128::from(whole)
            .checked_mul(unit_nanos)
            .ok_or_else(|| format!("duration overflow in {input:?}"))?;
        let frac_nanos = frac_num
            .checked_mul(unit_nanos)
            .and_then(|v| v.checked_div(frac_scale))
            .ok_or_else(|| format!("duration overflow in {input:?}"))?;
        let part_nanos = whole_nanos
            .checked_add(frac_nanos)
            .ok_or_else(|| format!("duration overflow in {input:?}"))?;
        let secs = u64::try_from(
            part_nanos.checked_div(1_000_000_000).ok_or_else(|| "duration overflow".to_owned())?,
        )
        .map_err(|_| format!("duration overflow in {input:?}"))?;
        let nanos = u32::try_from(
            part_nanos.checked_rem(1_000_000_000).ok_or_else(|| "duration overflow".to_owned())?,
        )
        .map_err(|_| format!("duration overflow in {input:?}"))?;
        total = total
            .checked_add(Duration::new(secs, nanos))
            .ok_or_else(|| format!("duration overflow in {input:?}"))?;
        any = true;
    }
    if !any {
        return Err("empty duration".to_owned());
    }
    Ok(total)
}

/// A reqwest client built from `options` as `object_store` builds its own (0.14.2,
/// `ClientOptions::client`, which is its crate's alone), from what the options say of each key.
/// Root certificates, which only `object_store`'s API adds, cladefs adds none of.
pub(crate) fn reqwest_client(options: &ClientOptions) -> object_store::Result<reqwest::Client> {
    use ClientConfigKey as Key;
    let value = |key: Key| options.get_config_value(&key);
    let invalid = |key: Key, error: &dyn std::fmt::Display| object_store::Error::Generic {
        store: "cladefs",
        source: format!("{}: {error}", key.as_ref()).into(),
    };
    let duration = |key: Key| {
        (value(key).map(|value| parse_duration(&value)).transpose())
            .map_err(|error| invalid(key, &error))
    };
    let flag = |key: Key| match value(key).map(|value| value.to_ascii_lowercase()).as_deref() {
        None | Some("0" | "false" | "off" | "no" | "n") => Ok(false),
        Some("1" | "true" | "on" | "yes" | "y") => Ok(true),
        Some(other) => Err(invalid(key, &format!("{other} is no boolean"))),
    };
    let tls = |error: reqwest::Error| invalid(Key::ProxyCaCertificate, &error);

    let mut builder = reqwest::Client::builder()
        .user_agent(value(Key::UserAgent).unwrap_or_else(|| USER_AGENT.to_owned()))
        // Compression would change Content-Length, which says an object's size.
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .https_only(!flag(Key::AllowHttp)?);
    if let Some(headers) = options.get_default_headers() {
        builder = builder.default_headers(headers.clone());
    }
    if let Some(url) = value(Key::ProxyUrl) {
        let mut proxy =
            reqwest::Proxy::all(&url).map_err(|error| invalid(Key::ProxyUrl, &error))?;
        if let Some(pem) = value(Key::ProxyCaCertificate) {
            let certificate = reqwest::Certificate::from_pem(pem.as_bytes()).map_err(tls)?;
            builder = builder.tls_certs_merge([certificate]);
        }
        if let Some(excludes) = value(Key::ProxyExcludes) {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&excludes));
        }
        builder = builder.proxy(proxy);
    }
    if flag(Key::NoSystemCertificates)? {
        builder = builder.tls_certs_only([]);
    }
    if let Some(timeout) = duration(Key::Timeout)? {
        builder = builder.timeout(timeout);
    }
    if let Some(timeout) = duration(Key::ConnectTimeout)? {
        builder = builder.connect_timeout(timeout);
    }
    if let Some(timeout) = duration(Key::ReadTimeout)? {
        builder = builder.read_timeout(timeout);
    }
    if let Some(timeout) = duration(Key::PoolIdleTimeout)? {
        builder = builder.pool_idle_timeout(timeout);
    }
    if let Some(max) = value(Key::PoolMaxIdlePerHost) {
        let max = max.parse().map_err(|error| invalid(Key::PoolMaxIdlePerHost, &error))?;
        builder = builder.pool_max_idle_per_host(max);
    }
    if let Some(interval) = duration(Key::Http2KeepAliveInterval)? {
        builder = builder.http2_keep_alive_interval(interval);
    }
    if let Some(timeout) = duration(Key::Http2KeepAliveTimeout)? {
        builder = builder.http2_keep_alive_timeout(timeout);
    }
    if flag(Key::Http2KeepAliveWhileIdle)? {
        builder = builder.http2_keep_alive_while_idle(true);
    }
    if let Some(size) = value(Key::Http2MaxFrameSize) {
        let size = size.parse().map_err(|error| invalid(Key::Http2MaxFrameSize, &error))?;
        builder = builder.http2_max_frame_size(Some(size));
    }
    if flag(Key::Http1Only)? {
        builder = builder.http1_only();
    }
    if flag(Key::Http2Only)? {
        builder = builder.http2_prior_knowledge();
    }
    if flag(Key::AllowInvalidCertificates)? {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if flag(Key::RandomizeAddresses)? {
        builder = builder.dns_resolver(Arc::new(Shuffled));
    }
    builder
        .build()
        .map_err(|error| object_store::Error::Generic { store: "cladefs", source: Box::new(error) })
}

/// Resolves names to their addresses in a random order, as `object_store` does unless told not
/// to: a host's connections spread over S3's many addresses.
struct Shuffled;

impl reqwest::dns::Resolve for Shuffled {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            use rand::seq::SliceRandom as _;
            let mut addresses: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            addresses.shuffle(&mut rand::rng());
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// A request that moved no bytes for so long.
#[derive(Debug, thiserror::Error)]
#[error("no bytes moved for {0:?}")]
pub struct Stalled(pub Duration);

fn stalled(stall: Duration) -> HttpError {
    HttpError::new(HttpErrorKind::Timeout, Stalled(stall))
}

/// reqwest's client, whose requests time out once they stall, and the dates of whose responses to
/// `bucket`'s own requests `clock` hears.
#[derive(Debug)]
struct Watched {
    client: reqwest::Client,
    stall: Duration,
    clock: Arc<crate::clock::Served>,
    bucket: String,
}

#[async_trait::async_trait]
impl object_store::client::HttpService for Watched {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let (parts, body) = request.into_parts();
        let url = reqwest::Url::parse(&parts.uri.to_string())
            .map_err(|error| HttpError::new(HttpErrorKind::Unknown, error))?;
        let dated = names(&self.bucket, &url);
        let mut sent = reqwest::Request::new(parts.method, url);
        *sent.headers_mut() = parts.headers;
        // cladefs: attach write-offset header for unsigned local/test S3 append endpoints (doc-29).
        append_header(&mut sent);
        let progress = Arc::new(Progress::new());
        let body = Sending { body, slice: Bytes::new(), progress: progress.clone() };
        *sent.body_mut() = Some(reqwest::Body::wrap(body));
        let response = std::pin::pin!(self.client.execute(sent));
        let stalls = std::pin::pin!(progress.stalls(self.stall));
        let response = match futures::future::select(response, stalls).await {
            Either::Left((response, _)) => response.map_err(http_error)?,
            Either::Right(((), _)) => return Err(stalled(self.stall)),
        };
        // As it arrived, headers read: a failure's response is dated as well as any.
        if let Some(date) = response.headers().get(reqwest::header::DATE).filter(|_| dated) {
            self.clock.heard(date.as_bytes(), std::time::Instant::now());
        }
        let (parts, body) = hyper::Response::<reqwest::Body>::from(response).into_parts();
        let body = Receiving { body, stall: self.stall, waiting: None };
        Ok(HttpResponse::from_parts(parts, HttpResponseBody::new(body)))
    }
}

/// What kind of failure a reqwest error is, which says whether `object_store` tries the request
/// again: one whose connection closed under it, as a pooled one the server had closed, goes again
/// whatever it is; one that timed out or was cut short, only if it is idempotent.
fn http_error(error: reqwest::Error) -> HttpError {
    let kind = if error.is_timeout() {
        HttpErrorKind::Timeout
    } else if error.is_connect() {
        HttpErrorKind::Connect
    } else if error.is_decode() {
        HttpErrorKind::Decode
    } else {
        let mut source = std::error::Error::source(&error);
        let mut kind = HttpErrorKind::Unknown;
        while let (HttpErrorKind::Unknown, Some(cause)) = (kind, source) {
            if let Some(hyper) = cause.downcast_ref::<hyper::Error>() {
                if hyper.is_closed()
                    || hyper.is_incomplete_message()
                    || hyper.is_body_write_aborted()
                {
                    kind = HttpErrorKind::Request;
                } else if hyper.is_timeout() {
                    kind = HttpErrorKind::Timeout;
                }
            }
            if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                use std::io::ErrorKind as Io;
                match io.kind() {
                    Io::TimedOut => kind = HttpErrorKind::Timeout,
                    Io::ConnectionAborted
                    | Io::ConnectionReset
                    | Io::BrokenPipe
                    | Io::UnexpectedEof => kind = HttpErrorKind::Interrupted,
                    _ => {}
                }
            }
            source = cause.source();
        }
        kind
    };
    // Its URL, which may hold a presigned request's credentials, `object_store` says if it may.
    HttpError::new(kind, error.without_url())
}

/// When a request last moved bytes.
struct Progress {
    start: Instant,
    /// Since `start`, in nanoseconds.
    last: AtomicU64,
}

impl Progress {
    fn new() -> Self {
        Self { start: Instant::now(), last: AtomicU64::new(0) }
    }

    fn made(&self) {
        self.last.store(self.start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// Returns once no bytes moved for `stall`.
    async fn stalls(&self, stall: Duration) {
        loop {
            let last = Duration::from_nanos(self.last.load(Ordering::Relaxed));
            let idle = self.start.elapsed().saturating_sub(last);
            if idle >= stall {
                return;
            }
            tokio::time::sleep(stall - idle).await;
        }
    }
}

/// A request's body, handed to its connection a slice at a time, each slice taken progress.
struct Sending {
    body: HttpRequestBody,
    /// What is left of the body's frame being sliced.
    slice: Bytes,
    progress: Arc<Progress>,
}

impl Body for Sending {
    type Data = Bytes;
    type Error = HttpError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, HttpError>>> {
        let this = &mut *self;
        while this.slice.is_empty() {
            match ready!(Pin::new(&mut this.body).poll_frame(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => this.slice = data,
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                end => return Poll::Ready(end),
            }
        }
        this.progress.made();
        let len = this.slice.len().min(SLICE);
        Poll::Ready(Some(Ok(Frame::data(this.slice.split_to(len)))))
    }

    fn is_end_stream(&self) -> bool {
        self.slice.is_empty() && self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let (body, slice) = (self.body.size_hint(), self.slice.len() as u64);
        let mut hint = SizeHint::new();
        hint.set_lower(body.lower() + slice);
        if let Some(upper) = body.upper() {
            hint.set_upper(upper + slice);
        }
        hint
    }
}

/// A response's body, which fails once none of it came for the stall time while it was waited
/// for: a reader slow to read it stalls nothing.
struct Receiving {
    body: reqwest::Body,
    stall: Duration,
    /// Since the read waiting began, if one is waiting.
    waiting: Option<Pin<Box<Sleep>>>,
}

impl Body for Receiving {
    type Data = Bytes;
    type Error = HttpError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, HttpError>>> {
        let this = &mut *self;
        if let Poll::Ready(frame) = Pin::new(&mut this.body).poll_frame(cx) {
            this.waiting = None;
            return Poll::Ready(frame.map(|frame| frame.map_err(http_error)));
        }
        let stall = this.stall;
        let waiting = this.waiting.get_or_insert_with(|| Box::pin(tokio::time::sleep(stall)));
        ready!(waiting.as_mut().poll(cx));
        this.waiting = None;
        Poll::Ready(Some(Err(stalled(stall))))
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
    }

    /// A body goes in slices of at most [`SLICE`] bytes, its frames' bytes in order, its size
    /// known throughout; each slice taken is progress.
    #[test]
    fn bodies_go_in_slices() {
        runtime().block_on(async {
            let payload: object_store::PutPayload =
                [Bytes::from(vec![1; SLICE * 2 + 5]), Bytes::new(), Bytes::from(vec![2; 3])]
                    .into_iter()
                    .collect();
            let progress = Arc::new(Progress::new());
            let mut body =
                Sending { body: payload.into(), slice: Bytes::new(), progress: progress.clone() };
            assert_eq!(body.size_hint().exact(), Some(SLICE as u64 * 2 + 8));
            tokio::time::advance(Duration::from_secs(7)).await;
            let mut lens = vec![];
            let mut bytes = vec![];
            while let Some(frame) = body.frame().await {
                let data = frame.unwrap().into_data().unwrap();
                lens.push(data.len());
                bytes.extend_from_slice(&data);
                let left = body.size_hint().exact().unwrap();
                assert_eq!(left as usize, SLICE * 2 + 8 - bytes.len());
            }
            assert_eq!(lens, [SLICE, SLICE, 5, 3]);
            let mut expected = vec![1; SLICE * 2 + 5];
            expected.extend([2; 3]);
            assert_eq!(bytes, expected);
            assert!(body.is_end_stream());
            assert_eq!(progress.last.load(Ordering::Relaxed), 7_000_000_000);
        });
    }

    /// A request stalls once no bytes moved for the stall time since they last did.
    #[test]
    fn requests_stall_once_idle_for_the_stall_time() {
        runtime().block_on(async {
            let progress = Progress::new();
            tokio::time::advance(Duration::from_secs(20)).await;
            progress.made();
            let start = Instant::now();
            progress.stalls(Duration::from_secs(30)).await;
            assert_eq!(start.elapsed(), Duration::from_secs(30));
        });
    }

    /// A body that never comes.
    struct Never;

    impl Body for Never {
        type Data = Bytes;
        type Error = std::io::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
            Poll::Pending
        }
    }

    /// A response none of which comes fails as timed out, once its reader waited the stall time.
    #[test]
    fn responses_stall_once_nothing_came_for_the_stall_time() {
        runtime().block_on(async {
            let stall = Duration::from_secs(30);
            let mut body = Receiving { body: reqwest::Body::wrap(Never), stall, waiting: None };
            let start = Instant::now();
            let error = body.frame().await.unwrap().unwrap_err();
            assert_eq!(start.elapsed(), stall);
            assert_eq!(error.kind(), HttpErrorKind::Timeout);
            assert_eq!(
                std::error::Error::source(&error).unwrap().to_string(),
                "no bytes moved for 30s"
            );
        });
    }

    #[test]
    fn parse_duration_handles_units_composites_and_errors() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("0.5s").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1.5m").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("0.25h").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("500millis").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("100nanos").unwrap(), Duration::from_nanos(100));
        assert_eq!(parse_duration("2wk").unwrap(), Duration::from_secs(1_209_600));
        assert_eq!(parse_duration("1yr").unwrap(), Duration::from_secs(31_557_600));
        assert_eq!(parse_duration("1m 30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("2h5m100µs").unwrap(), Duration::new(7500, 100_000));
        assert_eq!(parse_duration(" 1 day 2 hours ").unwrap(), Duration::from_secs(93_600));
        let options = ClientOptions::new().with_config(ClientConfigKey::ConnectTimeout, "0.5s");
        reqwest_client(&options).unwrap();
        assert!(parse_duration("").is_err());
        assert!(parse_duration("   ").is_err());
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("0.s").is_err());
        assert!(parse_duration(".5s").is_err());
        assert!(parse_duration("0.5.5s").is_err());
        assert!(parse_duration("30lightyears").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("18446744073709551615y").is_err());
    }
}
