// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-258: the daemon's spans, exported to OpenTelemetry's in-memory exporter, as a collector
//! would get them: a request's and a commit's trees, and their attributes; and none while tracing
//! is off. cladefs: TASK-672: and recorded for `/z/tracez`, exported or not, a read that went to the
//! bucket saying so, with the stack that waited for it.

use super::{BLOCK, connect, host, pattern, read, write};
use crate::ExtentId;
use crate::cladefsd::otel;
use crate::cladefsd::zpages::tracez::{self, Export};
use opentelemetry::Value;
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, Sampler, SdkTracerProvider, SimpleSpanProcessor, SpanData,
};
use std::collections::BTreeSet;

/// A tracer provider keeping every span, at once, in `exporter`.
fn provider(exporter: &InMemorySpanExporter) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .with_sampler(Sampler::AlwaysOn)
        .build()
}

/// Writes 2 blocks, reads them back and commits them, uploaded,, in a store a daemon serves, as a VM
/// process would: returns the store's cell.
fn write_read_commit() -> crate::CellId {
    let dir = tempfile::tempdir().unwrap();
    let (store, served) = connect(&dir.path().join("store"), 1 << 20);
    let cell = super::cell_at(&dir.path().join("store"));
    let extent = ExtentId { cell: cell.id, sequence: 1 };
    // TASK-260: the write for the guest's request 42.
    let guest =
        std::num::NonZeroU64::new(42).map(|guest| crate::request::RequestId { guest, store: None });
    crate::request::within_now(guest, || write(&store, extent, 0, &pattern(1, 2 * BLOCK))).unwrap();
    assert_eq!(read(&store, extent, 0, 2 * BLOCK), Ok(pattern(1, 2 * BLOCK)));
    store.commit(Some(b"root")).unwrap();
    drop(store);
    served.join().unwrap();
    cell
}

/// The span of `spans` named `name`, the one there is.
fn the<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let named: Vec<&SpanData> = spans.iter().filter(|span| span.name == name).collect();
    assert_eq!(named.len(), 1, "spans named {name}: {:?}", names(spans.iter()));
    named[0]
}

/// The names of `spans`, in order.
fn names<'a>(spans: impl Iterator<Item = &'a SpanData>) -> Vec<String> {
    spans.map(|span| span.name.to_string()).collect()
}

/// The names of the children of `parent` in `spans`, each once.
fn children(spans: &[SpanData], parent: &SpanData) -> BTreeSet<String> {
    let id = parent.span_context.span_id();
    let of = spans.iter().filter(|span| span.parent_span_id == id);
    for child in of.clone() {
        assert_eq!(child.span_context.trace_id(), parent.span_context.trace_id(), "{}", child.name);
    }
    names(of).into_iter().collect()
}

/// The attribute `key` of `span`, if it has it.
fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| kv.value.clone())
}

/// A request served is a trace's root, named by its kind, which says the store's cell and what
/// it asked for; a durable commit's span has its phases as children, sealing, writing, the flush
/// and the wait for object storage; and the upload of its segment is a span whose children are
/// the bucket's requests.
#[test]
fn spans_trace_requests_commits_and_uploads() {
    host();
    let exporter = InMemorySpanExporter::default();
    let export =
        Export::new(SimpleSpanProcessor::new(exporter.clone()), Box::new(Sampler::AlwaysOn));
    otel::install(Some(export));
    let cell = write_read_commit();
    let spans = exporter.get_finished_spans().unwrap();
    // cladefs: as `TENANT/CELL`, in base 32 (TASK-364, TASK-504).
    let named = Value::from(cell.to_string());

    let opened = the(&spans, "cladefsd.open");
    assert_eq!(children(&spans, opened), BTreeSet::from(["store.create".to_owned()]));
    let created = the(&spans, "store.create");
    assert_eq!(attribute(created, "cladefs.cell"), Some(named.clone()));

    let written = the(&spans, "cladefsd.write");
    assert_eq!(written.parent_span_id, opentelemetry::trace::SpanId::INVALID);
    assert_eq!(attribute(written, "cladefs.request.kind"), Some(Value::from("write")));
    assert_eq!(attribute(written, "cladefs.cell"), Some(named.clone()));
    assert_eq!(attribute(written, "cladefs.bytes"), Some(Value::I64(2 * BLOCK as i64)));
    assert_eq!(written.status, opentelemetry::trace::Status::Unset);
    assert_eq!(attribute(written, "cladefs.request_id"), Some(Value::from("42")));
    let read = the(&spans, "cladefsd.read");
    assert_eq!(attribute(read, "cladefs.bytes"), Some(Value::I64(2 * BLOCK as i64)));
    assert_eq!(attribute(read, "cladefs.request_id"), None, "a call for no guest request");

    let committed = the(&spans, "cladefsd.commit");
    assert_eq!(attribute(committed, "cladefs.level"), Some(Value::from("uploaded")));
    assert_eq!(children(&spans, committed), BTreeSet::from(["store.commit".to_owned()]));
    let commit = the(&spans, "store.commit");
    assert_eq!(attribute(commit, "cladefs.cell"), Some(named.clone()));
    assert_eq!(
        children(&spans, commit),
        ["commit.seal", "commit.upload_wait", "commit.write", "store.flush"]
            .map(str::to_owned)
            .into()
    );
    let sealed = the(&spans, "commit.seal");
    assert_eq!(attribute(sealed, "cladefs.extents"), Some(Value::I64(1)));

    // The commit's segment, created, the commit's small extents in it; then the cell's
    // checkpoint, which follows its forks' moves, created, then named by a swap of the cell's
    // head (TASK-407).
    let upload = the(&spans, "upload.segment");
    assert_eq!(attribute(upload, "cladefs.cell"), Some(named.clone()));
    assert_eq!(attribute(upload, "cladefs.extents"), Some(Value::I64(0)));
    assert_eq!(children(&spans, upload), BTreeSet::from(["bucket.create".to_owned()]));
    let checkpoint = the(&spans, "upload.checkpoint");
    assert_eq!(attribute(checkpoint, "cladefs.cell"), Some(named));
    assert_eq!(
        children(&spans, checkpoint),
        ["bucket.create", "bucket.list", "bucket.update"].map(str::to_owned).into()
    );
}

/// With tracing off, as the daemon starts unless told otherwise, nothing is exported: not even
/// with a tracer provider set, as another library of the process may set one.
#[test]
fn nothing_is_exported_while_tracing_is_off() {
    host();
    let exporter = InMemorySpanExporter::default();
    let provider = provider(&exporter);
    opentelemetry::global::set_tracer_provider(provider.clone());
    write_read_commit();
    provider.force_flush().unwrap();
    assert!(!bucket_cell::trace::enabled());
    assert_eq!(names(exporter.get_finished_spans().unwrap().iter()), Vec::<String>::new());
}

/// TASK-672.1: spans exported not, as the daemon makes them unless told otherwise, are recorded
/// for `/z/tracez`: each request counted by its kind, and kept, its trace whole.
#[test]
fn spans_are_recorded_unexported() {
    host();
    otel::install(None);
    write_read_commit();
    let recorder = tracez::recorder();
    let counts = recorder.counts();
    let ended = |name: &str| {
        let counted = counts.iter().find(|counts| counts.name == name);
        counted.map_or(0, |counts| counts.ended.iter().sum::<u64>())
    };
    assert_eq!(
        [ended("cladefsd.write"), ended("cladefsd.read"), ended("cladefsd.commit")],
        [1, 1, 1]
    );
    let commits = recorder.samples("cladefsd.commit", None);
    let names: BTreeSet<String> = names(commits[0].trace.spans.iter()).into_iter().collect();
    assert!(names.contains("store.commit"), "{names:?}");
}

/// TASK-672.2: a fork restored on another host reads its version's extent from the bucket: its
/// page-in says it went there, and the read that waited for it carries the stack that did, which
/// `/z/tracez` shows resolved, its samples filtered to the page-ins that went to the bucket.
#[test]
fn reads_from_the_bucket_say_so_with_the_stack_that_waited() {
    use super::{Opening, UnixStream, default_tenant, serve_connection};
    host();
    otel::install(None);
    let dir = tempfile::tempdir().unwrap();
    let (store, served) = connect(&dir.path().join("store"), 1 << 24);
    let extent = ExtentId { cell: store.cell(), sequence: 1 };
    write(&store, extent, 0, &pattern(4, BLOCK)).unwrap();
    store.commit(Some(b"root")).unwrap();
    let version = store.make_version().unwrap();
    drop(store);
    served.join().unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    let served = std::thread::spawn(move || serve_connection(server, default_tenant().unwrap()));
    let fork = Opening::ForkOf {
        tenant: version.tenant,
        cell: version.cell,
        version: version.version,
        capacity: 1 << 24,
    };
    let restored =
        crate::cladefsd::CellClient::over(client, &elsewhere.join("store"), fork).unwrap();
    assert_eq!(read(&restored, extent, 0, BLOCK), Ok(pattern(4, BLOCK)));
    // Every span of its requests ended once its connection is served.
    drop(restored);
    served.join().unwrap();

    let recorder = tracez::recorder();
    let source = |span: &SpanData| attribute(span, bucket_cell::trace::SOURCE);
    let page_ins = recorder.samples("cache.page_in", None);
    let sources: Vec<Option<Value>> = page_ins.iter().map(|sample| source(sample.span())).collect();
    assert_eq!(sources, [Some(Value::from("bucket"))]);
    let trace = &page_ins[0].trace;
    let read = the(&trace.spans, "cladefsd.read");
    let frames = attribute(read, bucket_cell::trace::BACKTRACE).expect("the read's stack");
    assert!(frames.as_str().split(' ').count() > 5, "{frames}");
    assert_eq!(children(&trace.spans, read), BTreeSet::from(["cache.page_in".to_owned()]));

    let page =
        crate::cladefsd::zpages::serve("/tracez", "name=cache.page_in&tag=cladefs.source%3Dbucket");
    let html = String::from_utf8(page.body).unwrap();
    assert!(html.contains("bring_in"), "the stack, resolved: {html}");
    assert!(html.contains("<i>cladefs.source</i>=bucket"), "{html}");
}
