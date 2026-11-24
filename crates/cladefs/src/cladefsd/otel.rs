// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's spans, exported over OTLP (TASK-258, `docs/tracing.md`): of each request
//! it serves, and of what serving it costs its cell, its commits and their phases, its opens and
//! forks, its page-ins, its uploads and their bucket requests, and its relief of memory
//! (`bucket_cell::trace`, which makes them).
//!
//! Tracing is enabled by `CLADEFSD_TRACES=otlp` ([`from_env`]). The exporter uses
//! `OTEL_EXPORTER_OTLP_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) for the collector,
//! with HTTP and protobuf (`http://localhost:4318` by default), and `OTEL_SERVICE_NAME` for the
//! service (`cladefsd` by default).
//!
//! cladefs: a sample of the traces, whole: a request, a commit or an upload starts a trace, and the
//! daemon makes thousands a second, a recorded span costing it some 5 us under load (pgbench,
//! TASK-707: every one recorded took 7% of the daemon's CPU). Unless `OTEL_TRACES_SAMPLER` says
//! otherwise, one in [`DEFAULT_RATIO`] of the traces they start is made and exported, its
//! spans with it (`parentbased_traceidratio`), `OTEL_TRACES_SAMPLER_ARG`, if given, the ratio:
//! decided by each trace's ID, as a root's sampler decides. A span of a trace not sampled records
//! nothing.
//!
//! cladefs: exported in batches by a processor of the daemon's ([`batch`]), as OpenTelemetry's
//! `OTEL_BSP_*` variables configure it, which counts the spans exported, dropped and failed, for the
//! daemon's metrics (board 8565); over HTTPS if the endpoint says `https://`, rustls's, the
//! certificates of the host's roots, or of `SSL_CERT_FILE`, trusted (board 8569). The trace context
//! of the gRPC calls the daemon serves and makes is W3C's (`traceparent`,
//! `tracestate`, `super::grpc`): a call served goes on its caller's trace.

pub mod batch;

use super::wire::Header;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Config, Sampler, SdkTracerProvider, ShouldSample, SpanProcessor};
use std::sync::OnceLock;
use std::time::Duration;

/// The variable that says what the daemon does with its spans ([`Tracing`]): `otlp`, `off`, or
/// nothing.
pub const OPTION: &str = "CLADEFSD_TRACES";

/// What the daemon does with its spans, as `CLADEFSD_TRACES` says ([`from_env`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracing {
    /// None made (`off` or unset).
    Off,
    /// Sampled traces exported over OTLP (`otlp`).
    Exported,
}

/// The ratio of the traces the daemon makes and exports, unless `OTEL_TRACES_SAMPLER` or
/// `OTEL_TRACES_SAMPLER_ARG` says otherwise: one in a hundred.
pub const DEFAULT_RATIO: f64 = 0.01;

/// The service the daemon's spans are of, unless `OTEL_SERVICE_NAME` says otherwise.
pub const SERVICE: &str = "cladefsd";

/// The attribute of a request's kind, as metrics name it (`metrics::KINDS`).
pub const REQUEST_KIND: &str = "cladefs.request.kind";

/// The attribute of a request's ID ([`request_id`]).
pub const REQUEST_ID: &str = "cladefs.request_id";

/// The attribute of the errno a request failed with.
pub const ERRNO: &str = "cladefs.errno";

/// The tracer provider spans go to, once started ([`install`]): what [`shutdown`] flushes.
// The process's tracer provider, as OpenTelemetry's global one is: every simulated host's spans go to it.
static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Starts the daemon's spans if OTLP is configured, using the environment's sampler and exporter.
pub fn from_env() -> Result<Tracing, String> {
    let option = std::env::var(OPTION).unwrap_or_default();
    match option.as_str() {
        "" | "off" => return Ok(Tracing::Off),
        "otlp" => {}
        other => return Err(format!("{OPTION} must be `otlp`, `off` or unset, not `{other}`")),
    }
    let sampler = sampler()?;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .map_err(|err| format!("making the OTLP exporter: {err}"))?;
    let processor = batch::Batch::new(exporter, batch::Config::from_env()?, &SPANS)
        .map_err(|err| format!("starting the OTLP exporter's thread: {err}"))?;
    install(sampler, processor);
    Ok(Tracing::Exported)
}

/// Installs the process's tracer provider with the given sampler and processor.
pub fn install(sampler: impl Into<Box<dyn ShouldSample>>, processor: impl SpanProcessor + 'static) {
    let provider = SdkTracerProvider::builder()
        .with_sampler(sampler)
        .with_span_processor(processor)
        .with_resource(resource())
        .build();
    if PROVIDER.set(provider.clone()).is_err() {
        log::warn!("cladefsd: tracing was started already");
        return;
    }
    opentelemetry::global::set_tracer_provider(provider);
    // cladefs: the trace context of the gRPC calls served and made, W3C's (board 8569).
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    bucket_cell::trace::enable(true);
}

/// What the daemon's exporter did with the spans it was given ([`spans`]).
// The process's counts, as its tracer provider is the process's: every simulated host's spans go
// to it.
static SPANS: batch::Counts = batch::Counts::new();

/// cladefs: the spans the daemon exported, dropped and failed to export, so far: all 0 unless it
/// exports them (board 8565).
pub fn spans() -> (u64, u64, u64) {
    SPANS.read()
}

/// Stops the daemon's spans, and exports those it made and didn't export yet, as it exits:
/// `within` that at most, what is left of its drain's bound, past which those not exported are
/// lost, as a collector that never answers would have them (board 8569). Before, the SDK's own
/// timeouts held the daemon past its drain, which an orchestrator's patience is set by.
pub fn shutdown(within: Duration) {
    let Some(provider) = PROVIDER.get() else { return };
    bucket_cell::trace::enable(false);
    if within.is_zero() {
        log::warn!("cladefsd: no time left of the drain to export the last spans");
        return;
    }
    if let Err(err) = provider.shutdown_with_timeout(within) {
        log::warn!("cladefsd: exporting the last spans within {within:?}: {err}");
    }
}

/// The daemon's resource: its service, [`SERVICE`] unless `OTEL_SERVICE_NAME` names another, as
/// the resource's builder reads it then.
fn resource() -> Resource {
    let resource = Resource::builder();
    match std::env::var_os("OTEL_SERVICE_NAME").is_some_and(|name| !name.is_empty()) {
        true => resource.build(),
        false => resource.with_service_name(SERVICE).build(),
    }
}

/// The sampler of the traces made and exported: the one `OTEL_TRACES_SAMPLER` names, as
/// OpenTelemetry's configuration reads it, if it names one; else [`default_sampler`]'s.
fn sampler() -> Result<Box<dyn ShouldSample>, String> {
    Ok(match default_sampler()? {
        Some(sampler) => Box::new(sampler),
        None => Config::default().sampler,
    })
}

/// The sampler of the traces made, unless `OTEL_TRACES_SAMPLER` names one: of the traces requests,
/// commits and uploads start, one in `OTEL_TRACES_SAMPLER_ARG` (or [`DEFAULT_RATIO`]), and the
/// spans of each with it.
fn default_sampler() -> Result<Option<Sampler>, String> {
    if std::env::var_os("OTEL_TRACES_SAMPLER").is_some_and(|sampler| !sampler.is_empty()) {
        return Ok(None);
    }
    let ratio = match std::env::var("OTEL_TRACES_SAMPLER_ARG") {
        Ok(ratio) if !ratio.is_empty() => {
            // Not a number: refused below.
            ratio.parse::<f64>().ok().filter(|ratio| (0.0..=1.0).contains(ratio)).ok_or_else(
                || format!("OTEL_TRACES_SAMPLER_ARG must be a ratio, 0 to 1: {ratio}"),
            )?
        }
        _ => DEFAULT_RATIO,
    };
    Ok(Some(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))))
}

/// The span of the request `header` heads, a trace's root, served by a connection to the cell
/// of `cell`, if it has one open, a call for the guest request `guest`, if any: `cladefsd.KIND`
/// (`metrics::span_name`); nothing while spans are off, at the cost of a load.
pub(super) fn request_span(
    header: &Header,
    cell: Option<crate::CellId>,
    guest: Option<&crate::request::RequestId>,
) -> bucket_cell::trace::Span {
    use bucket_cell::trace::{self, Span};
    if !trace::enabled() {
        return Span::none();
    }
    let span = Span::start(super::metrics::span_name(header.kind));
    match super::metrics::kind_name(header.kind) {
        Some(kind) => span.set(REQUEST_KIND, kind),
        // One metrics keep none of, as BUILD: its number.
        None => span.set_with(REQUEST_KIND, || header.kind.to_string()),
    }
    if let Some(id) = request_id(guest) {
        span.set_with(REQUEST_ID, || id.to_string());
    }
    if let Some(cell) = cell {
        span.set_cell(cell);
    }
    span
}

/// The ID of the guest request `guest` a request is a call for, if any, which its span carries as
/// [`REQUEST_ID`]: the guest's number of it, as the daemon's log records name it (`request_id`,
/// TASK-260), the one function that gives a request's ID. What a request's serving does on other
/// threads, as object storage's reads and the pager's, reads it from `crate::request::current`.
pub(super) fn request_id(guest: Option<&crate::request::RequestId>) -> Option<u64> {
    guest.map(|guest| guest.guest.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sets the environment's `variable` to `value`, or removes it.
    fn set(variable: &str, value: Option<&str>) {
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment.
        unsafe {
            match value {
                Some(value) => std::env::set_var(variable, value),
                None => std::env::remove_var(variable),
            }
        }
    }

    /// An unsupported setting is refused. With no setting, tracing stays off.
    #[test]
    fn spans_are_exported_only_if_the_option_says_otlp() {
        set(OPTION, Some("jaeger"));
        assert_eq!(
            from_env(),
            Err("CLADEFSD_TRACES must be `otlp`, `off` or unset, not `jaeger`".to_owned())
        );
        assert!(!bucket_cell::trace::enabled());
        set(OPTION, None);
        assert_eq!(from_env(), Ok(Tracing::Off));
        assert!(!bucket_cell::trace::enabled());
    }

    /// `off` makes no spans: none recorded, none exported, no tracer provider installed
    /// (TASK-707).
    #[test]
    fn off_makes_no_spans() {
        set(OPTION, Some("off"));
        assert_eq!(from_env(), Ok(Tracing::Off));
        assert!(!bucket_cell::trace::enabled());
        assert!(PROVIDER.get().is_none());
    }

    #[test]
    fn only_sampled_traces_record_spans() {
        use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
        set("OTEL_TRACES_SAMPLER", None);
        for (ratio, recorded) in [("0", false), ("1", true)] {
            set("OTEL_TRACES_SAMPLER_ARG", Some(ratio));
            let provider = SdkTracerProvider::builder().with_sampler(sampler().unwrap()).build();
            assert_eq!(provider.tracer("test").start("sampled").is_recording(), recorded);
        }
    }

    #[test]
    fn otlp_starts_tracing() {
        set(OPTION, Some("otlp"));
        set("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://localhost:4318"));
        assert_eq!(from_env(), Ok(Tracing::Exported));
        assert!(bucket_cell::trace::enabled());
        shutdown(Duration::from_secs(5));
        assert!(!bucket_cell::trace::enabled());
    }

    #[test]
    fn traces_are_sampled_by_ratio_unless_opentelemetrys_variables_say_otherwise() {
        let sampler = || format!("{:?}", default_sampler());
        set("OTEL_TRACES_SAMPLER", None);
        set("OTEL_TRACES_SAMPLER_ARG", None);
        assert_eq!(sampler(), "Ok(Some(ParentBased(TraceIdRatioBased(0.01))))");
        set("OTEL_TRACES_SAMPLER_ARG", Some("0.5"));
        assert_eq!(sampler(), "Ok(Some(ParentBased(TraceIdRatioBased(0.5))))");
        set("OTEL_TRACES_SAMPLER_ARG", Some("2"));
        assert_eq!(sampler(), "Err(\"OTEL_TRACES_SAMPLER_ARG must be a ratio, 0 to 1: 2\")");
        set("OTEL_TRACES_SAMPLER", Some("always_on"));
        assert_eq!(sampler(), "Ok(None)");
    }
}
