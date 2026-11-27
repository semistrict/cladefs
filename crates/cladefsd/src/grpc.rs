// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's gRPC calls, served and made, each seen alike by one layer around its
//! service or its channel, so that every method of the .protos is, the ones added later too, with
//! nothing in a handler:
//!
//! - **Counted** (board 8565): each method's calls, those that failed, and how long they took, to
//!   their answer's end, served ([`Served`]: the control API, its Mount service, the peer service)
//!   and made ([`Made`]: the peer service's clients), for the daemon's metrics
//!   (`cladefsd_grpc_server_*`, `cladefsd_grpc_client_*`, `super::metrics`). A call fails as its
//!   status says, a status other than OK, in its headers or its trailers; or as its answer's stream
//!   ends with none, its caller gone or its stream cut. Methods are labelled by the .protos'
//!   names alone ([`SERVICES`], which build.rs lists), never by what a caller asks for: a method
//!   none has is counted as `unknown`, so that no caller makes series of its own.
//! - **Traced** (board 8569): a call served is a span, `PACKAGE.SERVICE/METHOD`, a child of the
//!   trace its caller's W3C trace context names (`traceparent`, `tracestate`), so that a VM
//!   manager's trace goes on into the daemon; what serving it does, its own spans, are the call's
//!   children. A call made is a span too, its context sent to the peer it calls, as the peer
//!   then takes it. Nothing while spans are off (`super::otel`), at the cost of a load.

use crate::metrics::Timed;
use bucket_cell::trace::Span;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::SpanKind;
use std::sync::LazyLock;
use std::time::Instant;

/// cladefs: what a gRPC service names itself by: tonic's trait, or, under madsim, its tonic's
/// (doc-24, TASK-685.13).
#[cfg(not(madsim))]
use tonic::server::NamedService;
#[cfg(madsim)]
use tonic::transport::NamedService;

pub use cladefs_proto::services::{PEER, SERVICES, Service};

/// The upper bounds of the calls' duration histograms' buckets, in seconds: from a peer's part
/// served from its memory (a millisecond) to a sync waiting for the bucket, or a hand-over (a
/// minute).
const BUCKETS: [f64; 10] = [1e-3, 2.5e-3, 10e-3, 25e-3, 100e-3, 250e-3, 1.0, 2.5, 10.0, 60.0];

/// The label of the calls of a method a service has not.
pub(super) const UNKNOWN: &str = "unknown";

/// The calls of a service's methods: served, and, of the service the daemon calls, made, each
/// method's, in its order, then those of methods it has not ([`UNKNOWN`]).
pub(super) struct Calls {
    service: Service,
    served: Box<[Timed]>,
    made: Option<Box<[Timed]>>,
}

/// The services the daemon calls, whose calls made it counts: its peers'.
const MADE: [&str; 1] = [PEER.name];

/// Each service's calls, in [`SERVICES`]' order.
// The process's counts, every simulated host's together, as a process's metrics are.
static CALLS: LazyLock<[Calls; SERVICES.len()]> = LazyLock::new(|| SERVICES.map(Calls::new));

impl Calls {
    fn new(service: Service) -> Self {
        let each = || (0..=service.methods.len()).map(|_| Timed::new(&BUCKETS)).collect();
        let made = MADE.contains(&service.name).then(each);
        Self { service, served: each(), made }
    }

    /// The calls of the service named `name`, one of the daemon's .protos'.
    fn of(name: &str) -> &'static Calls {
        let calls = CALLS.iter().find(|calls| calls.service.name == name);
        calls.unwrap_or_else(|| panic!("{name}: no service of the daemon's .protos (build.rs)"))
    }

    /// The method of the service whose path is `path`, by its place among them, past them all if
    /// it has none.
    fn method(&self, path: &str) -> usize {
        let methods = self.service.methods;
        methods.iter().position(|known| *known == path).unwrap_or(methods.len())
    }

    /// The name of the span of a call of method `method`: its path but the first `/`, as
    /// OpenTelemetry names gRPC's spans, or the service's own of a method it has not.
    fn span_name(&self, method: usize) -> &'static str {
        match self.service.methods.get(method) {
            Some(path) => path.strip_prefix('/').unwrap_or(path),
            None => self.service.name,
        }
    }

    /// A call of the method whose path is `path` begins, `side` of it, its context `parent` if
    /// its caller sent one: counted in `timed`'s once it ends.
    fn begin(
        &'static self,
        path: &str,
        side: Side,
        parent: Option<&opentelemetry::Context>,
    ) -> Call {
        let method = self.method(path);
        let timed = match side {
            Side::Served => &self.served[method],
            // Made of a service the daemon calls: its peers', the one Made is of.
            Side::Made => &self.made.as_ref().expect("a service the daemon calls")[method],
        };
        let kind = match side {
            Side::Served => SpanKind::Server,
            Side::Made => SpanKind::Client,
        };
        let span = Span::start_of(self.span_name(method), kind, parent);
        if span.is_on() {
            span.set("rpc.system", "grpc");
            span.set("rpc.service", self.service.name);
            span.set(
                "rpc.method",
                self.service
                    .methods
                    .get(method)
                    .map_or(UNKNOWN, |path| path.rsplit('/').next().unwrap_or(path)),
            );
        }
        Call { counted: Some((timed, Instant::now())), span }
    }

    /// The service's name.
    pub(super) fn service(&self) -> &'static str {
        self.service.name
    }

    /// Each method's calls, by its name, [`UNKNOWN`] last: served, and made, if the daemon makes
    /// them.
    pub(super) fn methods(&self) -> impl Iterator<Item = (&'static str, &Timed, Option<&Timed>)> {
        let names = self.service.methods.iter().map(|path| path.rsplit('/').next().unwrap_or(path));
        let made = self.made.iter().flat_map(|made| made.iter().map(Some));
        let made = made.chain(std::iter::repeat(None));
        names
            .chain([UNKNOWN])
            .zip(self.served.iter())
            .zip(made)
            .map(|((name, served), made)| (name, served, made))
    }
}

/// Every service's calls, in [`SERVICES`]' order.
pub(super) fn calls() -> impl Iterator<Item = &'static Calls> {
    CALLS.iter()
}

/// Which side of a call the daemon is.
#[derive(Clone, Copy)]
enum Side {
    Served,
    // Under madsim, whose tonic's clients take its channel alone, none made ([`Made`]).
    #[cfg_attr(madsim, allow(dead_code))]
    Made,
}

/// A call under way: counted as it ends ([`Call::end`]), or, dropped before it ends, as failed, its
/// caller gone or its answer's stream cut; and its span, ended with it.
struct Call {
    counted: Option<(&'static Timed, Instant)>,
    span: Span,
}

impl Call {
    /// It ended, as the gRPC status `code` says, OK if 0; with none, its answer's stream cut.
    fn end(&mut self, code: Option<i32>) {
        let Some((timed, began)) = self.counted.take() else { return };
        let failed = code != Some(0);
        timed.record(began.elapsed(), failed);
        if let Some(code) = code {
            self.span.set("rpc.grpc.status_code", i64::from(code));
        }
        if failed {
            self.span.fail(match code {
                Some(code) => format!("gRPC status {}", tonic::Code::from_i32(code)),
                None => "ended with no status".to_owned(),
            });
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.end(None);
    }
}

/// The gRPC status `headers` hold, headers' or trailers', if they hold one.
#[cfg(not(madsim))]
fn status_of(headers: &tonic::codegen::http::HeaderMap) -> Option<i32> {
    // Not a number: none said, as gRPC's clients take it.
    headers.get("grpc-status")?.to_str().ok()?.parse().ok()
}

#[cfg(not(madsim))]
impl Call {
    /// `response`, the answer to it, which ends it: at once if its headers hold its status (a
    /// trailers-only answer, as every refusal is), else once its body ends, as its trailers say.
    fn answered(
        mut self,
        response: tonic::codegen::http::Response<tonic::body::Body>,
    ) -> tonic::codegen::http::Response<tonic::body::Body> {
        if let Some(code) = status_of(response.headers()) {
            self.end(Some(code));
            return response;
        }
        response.map(|body| tonic::body::Body::new(Watched { body, call: self }))
    }
}

#[cfg(not(madsim))]
pin_project_lite::pin_project! {
    /// cladefs: the body of the answer to `call`, which it ends as it ends: as its trailers' status
    /// says, as failed if it fails, or with none if it ends without them, or is dropped first.
    struct Watched<B> {
        #[pin]
        body: B,
        call: Call,
    }
}

#[cfg(not(madsim))]
impl<B: http_body::Body> http_body::Body for Watched<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use std::task::Poll;
        let this = self.project();
        let polled = this.body.poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(code) = frame.trailers_ref().and_then(status_of) {
                    this.call.end(Some(code));
                }
            }
            // Its error passed on to whoever reads the body, as it is: the call ended with no status.
            Poll::Ready(Some(Err(_)) | None) => this.call.end(None),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// The trace context a caller sent in `headers`, if spans are on and it sent one: W3C's, as the
/// propagator `super::otel` sets reads it.
fn extracted(headers: &dyn Extractor) -> Option<opentelemetry::Context> {
    if !bucket_cell::trace::enabled() {
        return None;
    }
    let context =
        opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(headers));
    use opentelemetry::trace::TraceContextExt as _;
    context.span().span_context().is_remote().then_some(context)
}

/// The headers of a call, as trace context is read from them ([`extracted`]) and written to them.
#[cfg(not(madsim))]
struct Headers<'a>(&'a mut tonic::codegen::http::HeaderMap);

#[cfg(not(madsim))]
impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        // Not text: no context, as a propagator takes what it can't parse.
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|key| key.as_str()).collect()
    }
}

#[cfg(not(madsim))]
impl opentelemetry::propagation::Injector for Headers<'_> {
    fn set(&mut self, key: &str, value: String) {
        use tonic::codegen::http::{HeaderName, HeaderValue};
        // What W3C's propagator writes is ASCII: a value that isn't, it would never write, is not
        // sent rather than send the call none.
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(key), HeaderValue::try_from(value)) {
            self.0.insert(name, value);
        }
    }
}

/// cladefs: the metadata of a call under madsim's tonic, as trace context is read from it.
#[cfg(madsim)]
struct Metadata<'a>(&'a tonic::metadata::MetadataMap);

#[cfg(madsim)]
impl Extractor for Metadata<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        // Not text: no context, as a propagator takes what it can't parse.
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        (self.0.keys())
            .filter_map(|key| match key {
                tonic::metadata::KeyRef::Ascii(key) => Some(key.as_str()),
                tonic::metadata::KeyRef::Binary(_) => None,
            })
            .collect()
    }
}

/// cladefs: the service `S` of the daemon's, its calls counted, timed and traced as the module
/// says, a child of the trace its caller sent: the control API's, its Mount service's, the peer
/// service's.
#[derive(Clone)]
pub(super) struct Served<S> {
    service: S,
    calls: &'static Calls,
}

impl<S: NamedService> Served<S> {
    /// `service`, one of the daemon's .protos' ([`SERVICES`]), served so.
    pub(super) fn new(service: S) -> Self {
        Self { service, calls: Calls::of(S::NAME) }
    }
}

impl<S: NamedService> NamedService for Served<S> {
    const NAME: &'static str = S::NAME;
}

#[cfg(not(madsim))]
impl<S, B> tower::Service<tonic::codegen::http::Request<B>> for Served<S>
where
    S: tower::Service<
            tonic::codegen::http::Request<B>,
            Response = tonic::codegen::http::Response<tonic::body::Body>,
        >,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures::future::BoxFuture<'static, Result<S::Response, S::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, mut request: tonic::codegen::http::Request<B>) -> Self::Future {
        let parent = extracted(&Headers(request.headers_mut()));
        let call = self.calls.begin(request.uri().path(), Side::Served, parent.as_ref());
        // Its span current as the service takes the call, and as each poll of its answer runs:
        // the parent of what serving it does, on whatever task it is done (`control::Drained`).
        let answered = {
            let _entered = call.span.enter();
            self.service.call(request)
        };
        let answered = call.span.parent().around(answered);
        Box::pin(async move {
            let mut call = call;
            match answered.await {
                Ok(response) => Ok(call.answered(response)),
                Err(error) => {
                    call.end(None);
                    Err(error)
                }
            }
        })
    }
}

/// cladefs: under madsim's tonic, a call is its path and its request, and its answer the response
/// or its status, as the simulated network carries them whole: ended as its answer comes.
#[cfg(madsim)]
impl<S, M> tower::Service<(tonic::codegen::http::uri::PathAndQuery, tonic::Request<M>)>
    for Served<S>
where
    S: tower::Service<
            (tonic::codegen::http::uri::PathAndQuery, tonic::Request<M>),
            Error = tonic::Status,
        >,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = tonic::Status;
    type Future = futures::future::BoxFuture<'static, Result<S::Response, tonic::Status>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(
        &mut self,
        (path, request): (tonic::codegen::http::uri::PathAndQuery, tonic::Request<M>),
    ) -> Self::Future {
        let parent = extracted(&Metadata(request.metadata()));
        let mut call = self.calls.begin(path.path(), Side::Served, parent.as_ref());
        let answered = {
            let _entered = call.span.enter();
            self.service.call((path, request))
        };
        let answered = call.span.parent().around(answered);
        Box::pin(async move {
            let answered = answered.await;
            call.end(Some(match &answered {
                Ok(_) => 0,
                Err(status) => status.code() as i32,
            }));
            answered
        })
    }
}

/// cladefs: a channel to a service of `S`'s kind, its calls counted, timed and traced as the module
/// says, each sending its span's context to the service: the peer service's clients'. None under
/// madsim, whose tonic's clients take its channel alone.
#[cfg(not(madsim))]
#[derive(Clone)]
pub(crate) struct Made<C> {
    channel: C,
    calls: &'static Calls,
}

#[cfg(not(madsim))]
impl<C> Made<C> {
    /// `channel`, to a service of `service`'s kind, one the daemon calls ([`MADE`]).
    pub(crate) fn new(channel: C, service: Service) -> Self {
        Self { channel, calls: Calls::of(service.name) }
    }
}

#[cfg(not(madsim))]
impl<C, B> tower::Service<tonic::codegen::http::Request<B>> for Made<C>
where
    C: tower::Service<
            tonic::codegen::http::Request<B>,
            Response = tonic::codegen::http::Response<tonic::body::Body>,
        >,
    C::Future: Send + 'static,
{
    type Response = C::Response;
    type Error = C::Error;
    type Future = futures::future::BoxFuture<'static, Result<C::Response, C::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.channel.poll_ready(cx)
    }

    fn call(&mut self, mut request: tonic::codegen::http::Request<B>) -> Self::Future {
        let call = self.calls.begin(request.uri().path(), Side::Made, None);
        if let Some(context) = call.span.context() {
            opentelemetry::global::get_text_map_propagator(|propagator| {
                propagator.inject_context(context, &mut Headers(request.headers_mut()));
            });
        }
        let answered = self.channel.call(request);
        Box::pin(async move {
            let mut call = call;
            match answered.await {
                Ok(response) => Ok(call.answered(response)),
                Err(error) => {
                    call.end(None);
                    Err(error)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A method is counted by the .protos' name of it alone: a path none has, whatever a caller
    /// makes up, is [`UNKNOWN`]'s, so that no caller makes series of its own; its span is named
    /// as OpenTelemetry names gRPC's, or by its service's name for an unknown one.
    #[test]
    fn methods_are_the_protos_or_unknown() {
        let peer = Calls::of(PEER.name);
        let get_part = peer.method("/cladefs.peer.v1.Peer/GetPart");
        assert_eq!(PEER.methods[get_part], "/cladefs.peer.v1.Peer/GetPart");
        assert_eq!(peer.span_name(get_part), "cladefs.peer.v1.Peer/GetPart");
        for made_up in
            ["", "/", "/cladefs.peer.v1.Peer/", "/cladefs.peer.v1.Peer/GetPar", "GetPart"]
        {
            assert_eq!(peer.method(made_up), PEER.methods.len(), "{made_up:?}");
        }
        assert_eq!(peer.span_name(PEER.methods.len()), "cladefs.peer.v1.Peer");
        let named: Vec<&str> = peer.methods().map(|(name, ..)| name).collect();
        assert_eq!(named.first(), Some(&"GetPart"));
        assert_eq!(named.last(), Some(&UNKNOWN));
        assert_eq!(named.len(), PEER.methods.len() + 1);
    }
}
