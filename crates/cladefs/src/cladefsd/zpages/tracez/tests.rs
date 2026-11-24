// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-672.1: the recorder's counts, samples and bounds, of spans it is handed as a tracer
//! provider would hand them.

use super::*;
use opentelemetry::KeyValue;
use opentelemetry::trace::{SpanContext, SpanKind, TraceFlags, TraceState};
use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};

/// A span named `name` of trace `trace`, numbered `span`, a child of `parent` (0 for none), that
/// started at `start` after the epoch and took `took`.
fn span(name: &'static str, trace: u128, span: u64, parent: u64, took: Duration) -> SpanData {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    SpanData {
        span_context: SpanContext::new(
            TraceId::from(trace),
            SpanId::from(span),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        ),
        parent_span_id: SpanId::from(parent),
        parent_span_is_remote: false,
        span_kind: SpanKind::Internal,
        name: Cow::Borrowed(name),
        start_time: start,
        end_time: start + took,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status: Status::Unset,
        instrumentation_scope: Default::default(),
    }
}

/// `data`, started: as its processor's `on_start` holds it.
fn running(data: &SpanData) -> Running {
    Running {
        name: data.name.clone(),
        trace: data.span_context.trace_id(),
        span: data.span_context.span_id(),
        parent: data.parent_span_id,
        start: data.start_time,
    }
}

/// The names of the spans of `sample`'s trace, in the order they ended.
fn names(sample: &Sample) -> Vec<String> {
    sample.trace.spans.iter().map(|span| span.name.to_string()).collect()
}

#[test]
fn latencies_fall_in_censuss_buckets() {
    let buckets: Vec<usize> = [0, 9_999, 10_000, 999_999, 1_000_000, 5_000_000, 100_000_000_000]
        .into_iter()
        .map(|nanos| bucket_of(Duration::from_nanos(nanos)))
        .collect();
    assert_eq!(buckets, [0, 0, 1, 2, 3, 3, 8]);
}

/// A trace is held while a span of it runs, its spans counted as they end; once none runs, its
/// spans are samples of their names' buckets, the trace whole in each.
#[test]
fn traces_are_kept_once_none_of_them_runs() {
    let recorder = Recorder::default();
    let request = span("cladefsd.fault", 1, 1, 0, Duration::from_millis(6));
    let page_in = span("cache.page_in", 1, 2, 1, Duration::from_millis(5));
    let mut get = span("bucket.get_range", 1, 3, 2, Duration::from_millis(5));
    get.attributes.push(KeyValue::new("cladefs.object_key", "segments/1.0"));
    for data in [&request, &page_in, &get] {
        recorder.started(running(data));
    }
    recorder.ended(get);
    recorder.ended(page_in);
    assert_eq!(recorder.running("cladefsd.fault").len(), 1);
    assert!(recorder.samples("bucket.get_range", None).is_empty(), "kept while its trace runs");
    let counts = recorder.counts();
    let get = counts.iter().find(|counts| counts.name == "bucket.get_range").unwrap();
    assert_eq!((get.ended[3], get.running), (1, 0));
    let fault = counts.iter().find(|counts| counts.name == "cladefsd.fault").unwrap();
    assert_eq!((fault.ended, fault.running), ([0; BOUNDS.len()], 1));

    recorder.ended(request);
    assert!(recorder.running("cladefsd.fault").is_empty());
    let kept = recorder.samples("bucket.get_range", Some(Which::Latency(3)));
    assert_eq!(kept.len(), 1);
    assert_eq!(names(&kept[0]), ["bucket.get_range", "cache.page_in", "cladefsd.fault"]);
    assert_eq!(kept[0].span().attributes[0].value.as_str(), "segments/1.0");
    assert_eq!(recorder.samples("cladefsd.fault", Some(Which::Latency(3))).len(), 1);
    assert!(recorder.samples("bucket.get_range", Some(Which::Latency(4))).is_empty());
}

/// Of each name and bucket the latest [`SAMPLES`] are kept, and of those that failed; every span
/// is counted.
#[test]
fn the_latest_samples_are_kept() {
    let recorder = Recorder::default();
    for trace in 1..=10u128 {
        let mut data = span("bucket.get", trace, 1, 0, Duration::from_micros(50));
        if trace % 2 == 0 {
            data.status = Status::error("no such key");
        }
        recorder.started(running(&data));
        recorder.ended(data);
    }
    let traces = |which| -> Vec<TraceId> {
        let kept = recorder.samples("bucket.get", Some(which));
        kept.iter().map(|sample| sample.span().span_context.trace_id()).collect()
    };
    assert_eq!(traces(Which::Latency(1)), [10u128, 9, 8, 7].map(TraceId::from));
    assert_eq!(traces(Which::Failed), [10u128, 8, 6, 4].map(TraceId::from));
    let counts = &recorder.counts()[0];
    assert_eq!((counts.ended[1], counts.failed), (10, 5));
}

/// A trace holds its last [`TRACE_SPANS`] spans to end, those before counted, so its root, which
/// ends last, is held; and at most [`OPEN_TRACES`] are held at once, the oldest dropped, its spans
/// that end then kept alone.
#[test]
fn what_is_held_is_bounded() {
    let recorder = Recorder::default();
    let root = span("upload.segment", 1, 1, 0, Duration::from_secs(1));
    recorder.started(running(&root));
    for number in 0..TRACE_SPANS as u64 + 3 {
        recorder.ended(span("bucket.create", 1, 100 + number, 1, Duration::from_millis(20)));
    }
    recorder.ended(root);
    let kept = recorder.samples("upload.segment", None);
    assert_eq!(kept.len(), 1);
    assert_eq!((kept[0].trace.spans.len(), kept[0].trace.dropped), (TRACE_SPANS, 4));
    assert_eq!(kept[0].span().name, "upload.segment");
    let first = &kept[0].trace.spans[0];
    assert_eq!(first.span_context.span_id(), SpanId::from(104), "the first 4 to end dropped");

    // Traces whose last byte puts them in one shard: OPEN_TRACES / SHARDS of them fill it.
    let in_shard = |number: u128| number << 8;
    let held = (OPEN_TRACES / SHARDS) as u128;
    for number in 1..=held + 1 {
        recorder.started(running(&span("store.flush", in_shard(number), 1, 0, Duration::ZERO)));
    }
    assert_eq!(recorder.running("store.flush").len(), held as usize);
    let dropped =
        recorder.running("store.flush").iter().all(|span| span.trace != TraceId::from(in_shard(1)));
    assert!(dropped, "the oldest trace was held still");
    recorder.ended(span("store.flush", in_shard(1), 1, 0, Duration::from_millis(1)));
    assert_eq!(recorder.samples("store.flush", None).len(), 1);
}

/// What is exported is decided by trace, as for a root: every span of a trace kept, of one dropped
/// none; the recorder records both.
#[test]
fn exports_are_whole_traces() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SimpleSpanProcessor};
    let exporter = InMemorySpanExporter::default();
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(0.5)));
    let export = Export::new(SimpleSpanProcessor::new(exporter.clone()), Box::new(sampler));
    let processor = Processor { recorder: Arc::default(), export: Some(export) };
    for trace in 1..=200u128 {
        // Random-looking IDs, as the ratio reads their low bits.
        let trace = trace.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835);
        processor.on_end(span("store.commit", trace, 2, 1, Duration::from_millis(1)));
        processor.on_end(span("cladefsd.commit", trace, 1, 0, Duration::from_millis(2)));
    }
    let exported = exporter.get_finished_spans().unwrap();
    let mut by_trace = HashMap::<TraceId, usize>::new();
    for span in &exported {
        *by_trace.entry(span.span_context.trace_id()).or_default() += 1;
    }
    assert!(by_trace.values().all(|spans| *spans == 2), "a trace exported in part");
    assert!((60..140).contains(&by_trace.len()), "{} of 200 traces exported", by_trace.len());
    let counts = processor.recorder.counts();
    assert_eq!(counts.iter().map(|counts| counts.ended[3]).sum::<u64>(), 400);
}
