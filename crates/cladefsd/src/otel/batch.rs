// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the processor of the spans the daemon exports over OTLP ([`Batch`]): in batches, as
//! OpenTelemetry's `BatchSpanProcessor` exports them, configured by its variables, but counting
//! the spans it exports, those it drops, and those whose export fails ([`Counts`]), which the
//! daemon's metrics say (`cladefsd_otel_spans_*_total`, board 8565). The SDK's drops the spans past
//! its queue saying so once, to a log of its own that reaches no one, and counts nothing an
//! operator can read: a collector too slow, or gone, lost traces unseen.
//!
//! - A span ended waits in a queue of `OTEL_BSP_MAX_QUEUE_SIZE` spans at most (2048): past it, it
//!   is dropped, and counted.
//! - A thread of its own exports what waits, `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` spans at most a
//!   batch (512), once that many wait, every `OTEL_BSP_SCHEDULE_DELAY` milliseconds (5000), and
//!   all of it when flushed or shut down; each batch exported or failed, as the exporter answers
//!   within its own timeout (`OTEL_EXPORTER_OTLP_TIMEOUT`).
//! - A flush, and a shutdown, wait for that thread so long at most, the time given
//!   ([`SpanProcessor::shutdown_with_timeout`]): past it, they return, whatever the collector, so
//!   that the daemon's exit is bounded by its drain's (`super::shutdown`, board 8569).

use opentelemetry::Context;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{Span, SpanData, SpanExporter, SpanProcessor};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a flush waits at most, as the SDK's does.
const FLUSH_WITHIN: Duration = Duration::from_secs(5);

/// How a [`Batch`] batches: OpenTelemetry's variables' (`OTEL_BSP_*`), or their defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// The most spans waiting; past it, dropped.
    pub queue: usize,
    /// The most spans exported at once, and how many waiting have them exported at once.
    pub batch: usize,
    /// How long spans wait at most before they are exported.
    pub delay: Duration,
}

impl Config {
    /// As `OTEL_BSP_MAX_QUEUE_SIZE`, `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` and `OTEL_BSP_SCHEDULE_DELAY`
    /// say, each its default if unset; a batch no larger than the queue, as the SDK has it. An
    /// error for one that is no number, or 0.
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str, default: u64| match std::env::var(name) {
            Ok(value) if !value.is_empty() => {
                // Not a number: refused below.
                let number = value.parse::<u64>().ok().filter(|number| *number > 0);
                number.ok_or_else(|| format!("{name} must be a number past 0, not {value:?}"))
            }
            _ => Ok(default),
        };
        let queue = usize::try_from(var("OTEL_BSP_MAX_QUEUE_SIZE", 2048)?).unwrap_or(usize::MAX);
        let batch = usize::try_from(var("OTEL_BSP_MAX_EXPORT_BATCH_SIZE", 512)?);
        let delay = Duration::from_millis(var("OTEL_BSP_SCHEDULE_DELAY", 5000)?);
        Ok(Self { queue, batch: batch.unwrap_or(usize::MAX).min(queue), delay })
    }
}

/// What a [`Batch`] did with the spans it was given: exported, dropped, its queue full or it shut
/// down, and failed, their export refused or unanswered.
#[derive(Debug, Default)]
pub struct Counts {
    exported: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
}

impl Counts {
    /// None yet.
    pub const fn new() -> Self {
        Self { exported: AtomicU64::new(0), dropped: AtomicU64::new(0), failed: AtomicU64::new(0) }
    }

    /// The spans exported, dropped and failed, so far.
    pub fn read(&self) -> (u64, u64, u64) {
        let load = |count: &AtomicU64| count.load(Ordering::Relaxed);
        (load(&self.exported), load(&self.dropped), load(&self.failed))
    }

    fn add(count: &AtomicU64, spans: usize) {
        count.fetch_add(spans as u64, Ordering::Relaxed);
    }
}

/// The processor of spans exported in batches, as the module says.
#[derive(Debug)]
pub struct Batch {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    config: Config,
    counts: &'static Counts,
    state: Mutex<State>,
    /// Wakes the exporting thread: spans enough waiting, a flush or a shutdown asked.
    work: Condvar,
    /// Wakes those waiting for it: a flush done, its shutdown.
    done: Condvar,
}

#[derive(Debug, Default)]
struct State {
    queue: VecDeque<SpanData>,
    /// The flushes asked, and those done, in order: a flush is done once `flushed` reaches its.
    asked: u64,
    flushed: u64,
    /// Whether a shutdown was asked, and whether its thread then stopped.
    shutdown: bool,
    stopped: bool,
    /// The resource the exporter is to say its spans are of, until its thread hands it over.
    resource: Option<Resource>,
}

impl Batch {
    /// Spans exported by `exporter`, on a thread of its own, as `config` says, counted in
    /// `counts`.
    pub fn new<E: SpanExporter + 'static>(
        exporter: E,
        config: Config,
        counts: &'static Counts,
    ) -> std::io::Result<Self> {
        let state = Mutex::new(State::default());
        let (work, done) = (Condvar::new(), Condvar::new());
        let shared = Arc::new(Shared { config, counts, state, work, done });
        let working = shared.clone();
        // cladefs: outside the simulation, as the collector the exporter reaches over HTTP is.
        std::thread::Builder::new()
            .name("cladefsd otlp".to_owned())
            .spawn(move || working.export(exporter))?;
        Ok(Self { shared })
    }

    /// Asks its thread for a flush, or for its shutdown if `shutdown`, and waits for it `within`
    /// at most: `Timeout` past it, the thread exporting still, as to a collector that never
    /// answers.
    fn ask(&self, shutdown: bool, within: Duration) -> OTelSdkResult {
        let deadline = Instant::now() + within;
        let mut state = self.shared.state.lock().unwrap();
        if state.stopped {
            return Err(OTelSdkError::AlreadyShutdown);
        }
        let flush = match shutdown {
            true => {
                state.shutdown = true;
                None
            }
            false => {
                state.asked += 1;
                Some(state.asked)
            }
        };
        self.shared.work.notify_one();
        loop {
            let done = state.stopped || flush.is_some_and(|flush| state.flushed >= flush);
            if done {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(OTelSdkError::Timeout(within));
            }
            state = self.shared.done.wait_timeout(state, left).unwrap().0;
        }
    }
}

impl Shared {
    /// Exports what waits with `exporter`, as the module says, until shut down: the thread's.
    fn export<E: SpanExporter>(&self, mut exporter: E) {
        let mut last = Instant::now();
        loop {
            let (mut spans, flush, shutdown, resource) = {
                let mut state = self.state.lock().unwrap();
                loop {
                    let due = last + self.config.delay;
                    let asked = state.shutdown || state.asked > state.flushed;
                    let now = Instant::now();
                    if asked || state.queue.len() >= self.config.batch || now >= due {
                        break;
                    }
                    state = self.work.wait_timeout(state, due - now).unwrap().0;
                }
                let all = state.shutdown || state.asked > state.flushed;
                let take = match all {
                    true => state.queue.len(),
                    false => state.queue.len().min(self.config.batch),
                };
                let spans: Vec<SpanData> = state.queue.drain(..take).collect();
                (spans, state.asked, state.shutdown, state.resource.take())
            };
            if let Some(resource) = resource {
                exporter.set_resource(&resource);
            }
            while !spans.is_empty() {
                let rest = spans.split_off(spans.len().min(self.config.batch));
                self.exported(&exporter, spans);
                spans = rest;
            }
            last = Instant::now();
            if shutdown && let Err(err) = exporter.shutdown() {
                log::warn!("cladefsd: shutting the OTLP exporter down: {err}");
            }
            let mut state = self.state.lock().unwrap();
            state.flushed = flush;
            state.stopped = shutdown;
            self.done.notify_all();
            if shutdown {
                return;
            }
        }
    }

    /// Exports `spans`, a batch, with `exporter`, counting them exported or failed.
    fn exported<E: SpanExporter>(&self, exporter: &E, spans: Vec<SpanData>) {
        let count = spans.len();
        // On this thread of its own, whose exporter's client blocks, as the SDK's processor does:
        // outside the simulation, as the collector is.
        match futures::executor::block_on(exporter.export(spans)) {
            Ok(()) => Counts::add(&self.counts.exported, count),
            Err(err) => {
                Counts::add(&self.counts.failed, count);
                log::warn!("cladefsd: exporting {count} spans over OTLP: {err}");
            }
        }
    }
}

impl SpanProcessor for Batch {
    fn on_start(&self, _span: &mut Span, _parent: &Context) {}

    fn on_end(&self, span: SpanData) {
        let mut state = self.shared.state.lock().unwrap();
        if state.shutdown || state.queue.len() >= self.shared.config.queue {
            drop(state);
            Counts::add(&self.shared.counts.dropped, 1);
            return;
        }
        state.queue.push_back(span);
        if state.queue.len() >= self.shared.config.batch {
            self.shared.work.notify_one();
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.ask(false, FLUSH_WITHIN)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.ask(true, timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.shared.state.lock().unwrap().resource = Some(resource.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{Tracer as _, TracerProvider as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    /// A span, ended, as a processor is handed it.
    fn span() -> SpanData {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
        drop(provider.tracer("test").start("a span"));
        exporter.get_finished_spans().unwrap().remove(0)
    }

    /// An exporter whose exports wait until `open` lets them through, each answering `answer`.
    #[derive(Debug)]
    struct Gate {
        open: Arc<(Mutex<bool>, Condvar)>,
        answer: fn() -> OTelSdkResult,
    }

    impl Gate {
        fn new(open: bool, answer: fn() -> OTelSdkResult) -> (Self, Arc<(Mutex<bool>, Condvar)>) {
            let gate = Arc::new((Mutex::new(open), Condvar::new()));
            (Self { open: gate.clone(), answer }, gate)
        }
    }

    impl SpanExporter for Gate {
        fn export(
            &self,
            _batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            let (open, opened) = &*self.open;
            drop(opened.wait_while(open.lock().unwrap(), |open| !*open).unwrap());
            std::future::ready((self.answer)())
        }
    }

    /// Counts of a test's own, which no other test's batch adds to.
    fn counts() -> &'static Counts {
        Box::leak(Box::new(Counts::new()))
    }

    /// A batch that never exports on its own: its batch and delay never reached.
    const QUEUE_OF_TWO: Config = Config { queue: 2, batch: 10, delay: Duration::from_secs(3600) };

    /// Spans past the queue are dropped, and counted, as are those ended once it shut down; those
    /// queued are exported as it shuts down, and counted so. The SDK's processor dropped them
    /// unsaid (board 8565).
    #[test]
    fn spans_past_the_queue_are_dropped_and_counted() {
        let counts = counts();
        let batch = Batch::new(Gate::new(true, || Ok(())).0, QUEUE_OF_TWO, counts).unwrap();
        for _ in 0..5 {
            batch.on_end(span());
        }
        assert_eq!(counts.read(), (0, 3, 0), "two queued, three dropped");
        batch.shutdown_with_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(counts.read(), (2, 3, 0));
        batch.on_end(span());
        assert_eq!(counts.read(), (2, 4, 0), "dropped, ended after its shutdown");
        let flushed = batch.force_flush();
        assert!(matches!(flushed, Err(OTelSdkError::AlreadyShutdown)), "{flushed:?}");
    }

    /// Spans whose export fails are counted failed, not exported.
    #[test]
    fn spans_whose_export_fails_are_counted_failed() {
        let counts = counts();
        let refused = || Err(OTelSdkError::InternalFailure("the collector refused".into()));
        let batch = Batch::new(Gate::new(true, refused).0, QUEUE_OF_TWO, counts).unwrap();
        batch.on_end(span());
        batch.on_end(span());
        batch.force_flush().unwrap();
        assert_eq!(counts.read(), (0, 0, 2));
    }

    /// board 8569: a shutdown waits for its exports the time it is given at most, an export to a
    /// collector that never answers holding its thread: it returns past it, `Timeout`, so that
    /// the daemon exits within its drain's bound, where the SDK's exporter's own timeouts held it.
    #[test]
    fn shutdowns_wait_no_longer_than_given_for_a_collector_that_never_answers() {
        let counts = counts();
        let (gate, open) = Gate::new(false, || Ok(()));
        let batch = Batch::new(gate, QUEUE_OF_TWO, counts).unwrap();
        batch.on_end(span());
        let began = Instant::now();
        let within = Duration::from_millis(300);
        let shut = batch.shutdown_with_timeout(within);
        assert!(matches!(shut, Err(OTelSdkError::Timeout(given)) if given == within), "{shut:?}");
        let took = began.elapsed();
        assert!(took >= within && took < within + Duration::from_secs(2), "{took:?}");
        // Let through, it exports what it held, and stops.
        *open.0.lock().unwrap() = true;
        open.1.notify_all();
        let deadline = Instant::now() + Duration::from_secs(30);
        while counts.read() != (1, 0, 0) {
            assert!(Instant::now() < deadline, "{:?}", counts.read());
            std::thread::yield_now();
        }
    }
}
