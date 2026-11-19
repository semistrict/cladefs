// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's spans, kept in its own memory as they run and end, whatever OTLP does
//! (TASK-672.1), for `/z/tracez` ([`page`]) to show, as Census's tracez did: of each span name,
//! how many ended in each of Census's latency buckets ([`BOUNDS`]) and with errors, those running
//! now, and samples of each bucket, whole traces, with their attributes. Wall clock, not CPU: what
//! waits for the bucket takes milliseconds, so a page-in that missed every cache fills the slow
//! buckets on its own, its trace saying where it went (`cladefs.source`) and the stack that waited
//! for it (`cladefs.backtrace`, TASK-672.2).
//!
//! One [`SpanProcessor`] sees every span, the tracer provider sampling all of them: the
//! [`Recorder`], and, if spans are exported over OTLP, the exporter behind it ([`Export`]), handed
//! the traces its sampler keeps. A trace's spans are held as they end, until none of it runs, and
//! then kept as samples, the latest [`SAMPLES`] of each span name in each bucket, sharing the trace
//! they are of: bounded spans of a trace ([`TRACE_SPANS`]), traces held at once ([`OPEN_TRACES`]),
//! so bounded memory however many spans the daemon makes.

mod page;

pub(super) use page::page;

use opentelemetry::Context;
use opentelemetry::trace::{SamplingDecision, SpanId, Status, TraceId};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{ShouldSample, Span, SpanData, SpanProcessor};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

/// The lower bounds of Census's latency buckets: a span is counted in the last whose bound its
/// latency reaches.
pub const BOUNDS: [Duration; 9] = [
    Duration::ZERO,
    Duration::from_micros(10),
    Duration::from_micros(100),
    Duration::from_millis(1),
    Duration::from_millis(10),
    Duration::from_millis(100),
    Duration::from_secs(1),
    Duration::from_secs(10),
    Duration::from_secs(100),
];

/// How many samples are kept of each span name in each latency bucket, and of those that failed:
/// the latest.
pub const SAMPLES: usize = 4;

/// The most spans held of one trace, those that end after dropped and counted: an upload's
/// requests, many, are a trace.
pub const TRACE_SPANS: usize = 64;

/// The most traces held at once, until none of each runs: those that started first dropped past
/// it, as a span never ended would hold its trace for good.
pub const OPEN_TRACES: usize = 4096;

/// How many locks the traces held are spread over, by their IDs, as the daemon's threads end
/// spans at once.
const SHARDS: usize = 16;

/// The latency bucket of a span that took `latency` ([`BOUNDS`]).
pub fn bucket_of(latency: Duration) -> usize {
    BOUNDS.iter().rposition(|bound| latency >= *bound).expect("every latency reaches zero")
}

/// The spans the daemon made, as they ran and ended. See the module docs.
pub struct Recorder {
    /// Each span name's counts and samples, by name.
    names: RwLock<BTreeMap<Cow<'static, str>, Arc<Named>>>,
    /// The traces some span of which runs, by their IDs' last byte.
    shards: [Mutex<Shard>; SHARDS],
}

/// What a span name's spans did.
#[derive(Default)]
struct Named {
    /// How many ended in each latency bucket.
    ended: [AtomicU64; BOUNDS.len()],
    /// How many ended failed, their status an error's.
    failed: AtomicU64,
    samples: Mutex<Samples>,
}

/// A span name's samples, the latest first.
#[derive(Default)]
struct Samples {
    latency: [VecDeque<Sample>; BOUNDS.len()],
    failed: VecDeque<Sample>,
}

/// A span kept: of its trace, the span at `span`.
#[derive(Clone)]
pub struct Sample {
    pub trace: Arc<Trace>,
    pub span: usize,
}

impl Sample {
    /// Its span.
    pub fn span(&self) -> &SpanData {
        &self.trace.spans[self.span]
    }
}

/// The spans of a trace held, in the order they ended, and how many more ended, past
/// [`TRACE_SPANS`].
pub struct Trace {
    pub spans: Vec<SpanData>,
    pub dropped: usize,
}

/// The traces held, some span of each running.
#[derive(Default)]
struct Shard {
    open: HashMap<TraceId, Open>,
    /// How many traces it opened, the number of the next.
    opened: u64,
}

/// A trace held: its spans running and ended.
#[derive(Default)]
struct Open {
    /// Which trace the shard opened it as, the oldest first dropped.
    number: u64,
    running: HashMap<SpanId, Running>,
    ended: Vec<SpanData>,
    dropped: usize,
}

/// A span running.
#[derive(Clone, Debug)]
pub struct Running {
    pub name: Cow<'static, str>,
    pub trace: TraceId,
    pub span: SpanId,
    pub parent: SpanId,
    pub start: SystemTime,
}

/// Which samples of a span name: of a latency bucket, or those that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Latency(usize),
    Failed,
}

/// A span name's counts: how many ended in each latency bucket, how many failed, and how many run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counts {
    pub name: String,
    pub ended: [u64; BOUNDS.len()],
    pub failed: u64,
    pub running: u64,
}

impl Default for Recorder {
    fn default() -> Self {
        Self { names: RwLock::default(), shards: std::array::from_fn(|_| Mutex::default()) }
    }
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

impl Recorder {
    /// The shard holding the trace `trace`: by its ID's last byte, random.
    fn shard(&self, trace: TraceId) -> &Mutex<Shard> {
        &self.shards[usize::from(trace.to_bytes()[15]) % SHARDS]
    }

    /// The counts and samples of spans named `name`, made if none ended yet.
    fn named(&self, name: &Cow<'static, str>) -> Arc<Named> {
        if let Some(named) = self.names.read().unwrap().get(name) {
            return named.clone();
        }
        self.names.write().unwrap().entry(name.clone()).or_default().clone()
    }

    /// Holds `running`, which started, as running, in its trace.
    pub fn started(&self, running: Running) {
        let mut shard = self.shard(running.trace).lock().unwrap();
        let shard = &mut *shard;
        let open = match shard.open.get_mut(&running.trace) {
            Some(open) => open,
            None => {
                if shard.open.len() >= OPEN_TRACES / SHARDS {
                    let oldest = shard.open.iter().min_by_key(|(_, open)| open.number);
                    let oldest = *oldest.expect("a shard holding traces").0;
                    shard.open.remove(&oldest);
                }
                shard.opened += 1;
                let open = Open { number: shard.opened, ..Open::default() };
                shard.open.entry(running.trace).or_insert(open)
            }
        };
        open.running.insert(running.span, running);
    }

    /// Counts `span`, which ended, and holds it in its trace: which, if none of it runs now, is
    /// let go of, its spans kept as samples.
    pub fn ended(&self, span: SpanData) {
        let latency = span.end_time.duration_since(span.start_time).unwrap_or_default();
        let named = self.named(&span.name);
        named.ended[bucket_of(latency)].fetch_add(1, Ordering::Relaxed);
        if matches!(span.status, Status::Error { .. }) {
            named.failed.fetch_add(1, Ordering::Relaxed);
        }
        let trace = span.span_context.trace_id();
        let closed = {
            let mut shard = self.shard(trace).lock().unwrap();
            // One its start was not held of, as its trace was dropped: held alone.
            let open = shard.open.entry(trace).or_default();
            open.running.remove(&span.span_context.span_id());
            match open.ended.len() < TRACE_SPANS {
                true => open.ended.push(span),
                false => open.dropped += 1,
            }
            match open.running.is_empty() {
                true => shard.open.remove(&trace),
                false => None,
            }
        };
        if let Some(closed) = closed {
            self.keep(closed);
        }
    }

    /// Keeps the spans of `trace`, none of which runs, as samples, each the latest of its name and
    /// its latency bucket, and of those that failed if it did.
    fn keep(&self, trace: Open) {
        let trace = Arc::new(Trace { spans: trace.ended, dropped: trace.dropped });
        for (index, span) in trace.spans.iter().enumerate() {
            let sample = Sample { trace: trace.clone(), span: index };
            let latency = span.end_time.duration_since(span.start_time).unwrap_or_default();
            let named = self.named(&span.name);
            let mut samples = named.samples.lock().unwrap();
            let mut push = |kept: &mut VecDeque<Sample>| {
                kept.push_front(sample.clone());
                kept.truncate(SAMPLES);
            };
            push(&mut samples.latency[bucket_of(latency)]);
            if matches!(span.status, Status::Error { .. }) {
                push(&mut samples.failed);
            }
        }
    }

    /// Each span name's counts, by name.
    pub fn counts(&self) -> Vec<Counts> {
        let mut running = HashMap::<String, u64>::new();
        for shard in &self.shards {
            for open in shard.lock().unwrap().open.values() {
                for span in open.running.values() {
                    *running.entry(span.name.to_string()).or_default() += 1;
                }
            }
        }
        let names = self.names.read().unwrap();
        let mut counts: Vec<Counts> = (names.iter())
            .map(|(name, named)| Counts {
                name: name.to_string(),
                ended: std::array::from_fn(|bucket| named.ended[bucket].load(Ordering::Relaxed)),
                failed: named.failed.load(Ordering::Relaxed),
                running: running.remove(name.as_ref()).unwrap_or(0),
            })
            .collect();
        // Those running whose names none ended of yet.
        counts.extend(running.into_iter().map(|(name, running)| Counts {
            name,
            ended: [0; BOUNDS.len()],
            failed: 0,
            running,
        }));
        counts.sort_by(|a, b| a.name.cmp(&b.name));
        counts
    }

    /// The samples of spans named `name`, the latest first: of `which`, or of every latency bucket,
    /// the slowest first.
    pub fn samples(&self, name: &str, which: Option<Which>) -> Vec<Sample> {
        let Some(named) = self.names.read().unwrap().get(name).cloned() else { return Vec::new() };
        let samples = named.samples.lock().unwrap();
        match which {
            Some(Which::Latency(bucket)) => samples
                .latency
                .get(bucket)
                .map_or(Vec::new(), |kept| kept.iter().cloned().collect()),
            Some(Which::Failed) => samples.failed.iter().cloned().collect(),
            None => samples.latency.iter().rev().flatten().cloned().collect(),
        }
    }

    /// The spans named `name` running, the longest running first.
    pub fn running(&self, name: &str) -> Vec<Running> {
        let mut running: Vec<Running> = (self.shards.iter())
            .flat_map(|shard| {
                let shard = shard.lock().unwrap();
                let of = shard.open.values().flat_map(|open| open.running.values());
                of.filter(|span| span.name == name).cloned().collect::<Vec<_>>()
            })
            .collect();
        running.sort_by_key(|span| span.start);
        running
    }
}

/// The recorder of the process's spans, which `/z/tracez` shows.
pub fn recorder() -> &'static Arc<Recorder> {
    static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();
    RECORDER.get_or_init(Arc::default)
}

/// Spans exported: the traces `sampler` keeps, by their IDs alone, as a root span's sampler
/// decides for its trace, handed whole to `processor`, a batch exporter's over OTLP.
#[derive(Debug)]
pub struct Export {
    sampler: Box<dyn ShouldSample>,
    processor: Box<dyn SpanProcessor>,
}

impl Export {
    pub fn new(processor: impl SpanProcessor + 'static, sampler: Box<dyn ShouldSample>) -> Self {
        Self { sampler, processor: Box::new(processor) }
    }

    /// Whether `span` is of a trace exported: decided as for a root, by its trace's ID, so that
    /// each trace is exported whole or not at all, as a parent-based sampler keeps it.
    fn keeps(&self, span: &SpanData) -> bool {
        let trace = span.span_context.trace_id();
        let decision = (self.sampler)
            .should_sample(None, trace, &span.name, &span.span_kind, &[], &[])
            .decision;
        decision == SamplingDecision::RecordAndSample
    }
}

/// The one processor of the daemon's spans: the [`Recorder`]'s, and the [`Export`]'s behind it, if
/// spans are exported. One, as the tracer provider copies each span for each processor it has.
#[derive(Debug)]
pub struct Processor {
    recorder: Arc<Recorder>,
    export: Option<Export>,
}

impl Processor {
    /// Spans recorded by [`recorder`], and exported by `export`, if given.
    pub fn new(export: Option<Export>) -> Self {
        Self { recorder: recorder().clone(), export }
    }
}

impl SpanProcessor for Processor {
    fn on_start(&self, span: &mut Span, _parent: &Context) {
        // A copy of a span just started: its attributes, events and links none yet, nothing
        // allocated.
        let Some(data) = span.exported_data() else { return };
        self.recorder.started(Running {
            name: data.name,
            trace: data.span_context.trace_id(),
            span: data.span_context.span_id(),
            parent: data.parent_span_id,
            start: data.start_time,
        });
    }

    fn on_end(&self, span: SpanData) {
        if let Some(export) = &self.export
            && export.keeps(&span)
        {
            export.processor.on_end(span.clone());
        }
        self.recorder.ended(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.export.as_ref().map_or(Ok(()), |export| export.processor.force_flush())
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        (self.export.as_ref())
            .map_or(Ok(()), |export| export.processor.shutdown_with_timeout(timeout))
    }

    fn set_resource(&mut self, resource: &Resource) {
        if let Some(export) = &mut self.export {
            export.processor.set_resource(resource);
        }
    }
}

#[cfg(test)]
mod tests;
