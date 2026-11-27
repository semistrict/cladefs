// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: spans of the expensive operations of extent stores, their uploads and their daemon, for
//! OpenTelemetry (TASK-258), shared by the crates that make them: bucket_cell's bucket requests,
//! uploads and checkpoints, local_cell's commits and flushes, and cladefs's daemon
//! (`cladefs::cladefsd`), which enables them from the environment (`cladefs::cladefsd::otel`;
//! `docs/tracing.md`).
//!
//! Off unless enabled, a [`Span`] is nothing: made at the cost of one relaxed load, with no tracer,
//! no exporter and nothing allocated, as every request and commit makes some. On, each is a span of
//! the global tracer provider's tracer, a child of the context current where it starts: of the
//! span a thread runs in ([`Span::run`]), or a future runs around, on whatever thread polls it
//! ([`Span::around`]). It ends as it drops, its attributes and its status set on it as it goes
//! ([`Span::set`], [`Span::fail`]).
//!
//! cladefs: and where a read's data came from (TASK-672.2): the nearest of the host's memory, its
//! part cache, a peer and the bucket that a read went as far as ([`Source`]), each noted where the
//! fetcher goes there ([`note`]) and gathered by what waits for the read ([`sourced`]); and, of a
//! read that went to the bucket, the frames of the thread that waited for it
//! ([`record_backtrace`]), as addresses, resolved only when shown.

use crate::layout::CellId;
use futures::future::Either;
use opentelemetry::context::{FutureExt as _, WithContext};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue, Value, global};
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

/// The attribute of the bytes an operation read, wrote or moved.
pub const BYTES: &str = "cladefs.bytes";
/// The attribute of the cell an operation is of (`doc-7`), in base 32 as paths name it.
pub const CELL: &str = "cladefs.cell";
/// The attribute of an extent's sequence number in its cell ([`CELL`]).
pub const EXTENT: &str = "cladefs.extent";
/// The attribute of the extents an operation took.
pub const EXTENTS: &str = "cladefs.extents";
/// The attribute of the commits an operation took, or the number of the one it made.
pub const COMMITS: &str = "cladefs.commits";
/// The attribute of a commit's number.
pub const COMMIT: &str = "cladefs.commit";
/// The attribute of the level a commit waits for: `ordered`, `local` or `uploaded` (doc-12).
pub const LEVEL: &str = "cladefs.level";
/// The attribute of the cell a fork makes.
pub const CHILD: &str = "cladefs.child";
/// The attribute of a cell's path.
pub const STORE: &str = "cladefs.store";
/// The attribute of the arena of physical memory an operation is of.
pub const ARENA: &str = "cladefs.arena";
/// The attribute of an object's key in the bucket, without the bucket's prefix.
pub const OBJECT_KEY: &str = "cladefs.object_key";
/// The attribute of the objects a bucket's listing found, or a batch of deletes deleted.
pub const OBJECTS: &str = "cladefs.objects";
/// The attribute of the prefixes a bucket's listing by delimiter found (TASK-395.5).
pub const PREFIXES: &str = "cladefs.prefixes";
/// The attribute of a segment's sequence number in its cell.
pub const SEGMENT: &str = "cladefs.segment";
/// cladefs: the attribute of where a read's data came from: a [`Source`]'s name, or `local`, for a
/// read of the host's cells, which went to no fetcher (TASK-672.2).
pub const SOURCE: &str = "cladefs.source";
/// cladefs: the attribute of the frames of the thread that waited for a read from the bucket: their
/// addresses in this process, in hexadecimal, innermost first, separated by spaces
/// ([`record_backtrace`]).
pub const BACKTRACE: &str = "cladefs.backtrace";

/// Whether spans are made, which [`enable`] says.
// The process's tracing, on or off: its spans, every simulated host's, go to one tracer.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The tracer spans are made with, of the global tracer provider as [`enable`] found it.
// The process's tracer, of its global provider: one for every simulated host.
static TRACER: OnceLock<global::BoxedTracer> = OnceLock::new();

/// The name of the tracer, the instrumentation's scope.
const SCOPE: &str = "cladefs";

/// Makes spans from now on, with the global tracer provider, which the caller set
/// (`opentelemetry::global::set_tracer_provider`) before; or, `on` false, no more.
pub fn enable(on: bool) {
    if on {
        TRACER.get_or_init(|| global::tracer(SCOPE));
    }
    ENABLED.store(on, Ordering::Release);
}

/// Whether spans are made ([`enable`]).
#[inline]
pub fn enabled() -> bool {
    // Relaxed: the tracer it says is there is TRACER's, whose get acquires it on its own.
    ENABLED.load(Ordering::Relaxed)
}

/// The IDs of the span current on this thread, if spans are on and one is: its trace's and its
/// own, in hexadecimal, as exporters name them, which a log record says to be found in its trace.
/// Asked off the hot path: it reads the context current.
pub fn current_ids() -> Option<(String, String)> {
    if !enabled() {
        return None;
    }
    let context = Context::current();
    let span = context.span();
    let ids = span.span_context();
    ids.is_valid().then(|| (ids.trace_id().to_string(), ids.span_id().to_string()))
}

/// A span of an operation, ended as it drops; nothing while spans are off ([`enabled`]).
#[must_use = "a span ends as it drops"]
pub struct Span(Option<Context>);

impl Span {
    /// A span named `name`, a child of the context current on this thread (or of a future's
    /// context, [`Span::around`]); nothing while spans are off.
    #[inline]
    pub fn start(name: &'static str) -> Self {
        if !enabled() {
            return Self(None);
        }
        Self::start_in(name, &Context::current())
    }

    /// cladefs: a span named `name`, of `kind`, a child of `parent`, the context a caller in another
    /// process sent with its call (W3C trace context, `cladefs::cladefsd::grpc`), or of the context
    /// current on this thread if none; nothing while spans are off (board 8569).
    pub fn start_of(name: &'static str, kind: SpanKind, parent: Option<&Context>) -> Self {
        if !enabled() {
            return Self(None);
        }
        let Some(tracer) = TRACER.get() else { return Self(None) };
        let current;
        let parent = match parent {
            Some(parent) => parent,
            None => {
                current = Context::current();
                &current
            }
        };
        let span = tracer.span_builder(name).with_kind(kind).start_with_context(tracer, parent);
        Self(Some(parent.with_span(span)))
    }

    /// Nothing, as a span is while spans are off: of an operation that isn't traced.
    pub const fn none() -> Self {
        Self(None)
    }

    /// cladefs: its context, if it is a span: what a call made within it carries to the process
    /// it calls, as its trace's (board 8569).
    pub fn context(&self) -> Option<&Context> {
        self.0.as_ref()
    }

    #[cold]
    fn start_in(name: &'static str, parent: &Context) -> Self {
        let Some(tracer) = TRACER.get() else { return Self(None) };
        let span = tracer.start_with_context(name, parent);
        Self(Some(parent.with_span(span)))
    }

    /// Whether it is a span, not nothing: what saying much of it waits for.
    #[inline]
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// Sets its attribute `key` to `value`.
    pub fn set(&self, key: &'static str, value: impl Into<Value>) {
        if let Some(context) = &self.0 {
            context.span().set_attribute(KeyValue::new(key, value));
        }
    }

    /// Sets its attribute `key` to what `value` makes, made only if it is a span: an attribute
    /// costing something to make, as a path's text.
    pub fn set_with<V: Into<Value>>(&self, key: &'static str, value: impl FnOnce() -> V) {
        if let Some(context) = &self.0 {
            context.span().set_attribute(KeyValue::new(key, value()));
        }
    }

    /// Sets its attribute `key` to `value`, a count: OpenTelemetry's integers are signed.
    pub fn set_count(&self, key: &'static str, value: u64) {
        self.set(key, i64::try_from(value).unwrap_or(i64::MAX));
    }

    /// Sets its attribute [`CELL`] to `cell`, in base 32, as cell paths name cells.
    pub fn set_cell(&self, cell: CellId) {
        self.set_with(CELL, || format!("{cell}"));
    }

    /// Says the operation failed, with `error`, its status an error's.
    pub fn fail(&self, error: impl std::fmt::Display) {
        if let Some(context) = &self.0 {
            context.span().set_status(Status::error(error.to_string()));
        }
    }

    /// Says the operation failed if `result` did, as [`Span::fail`] says.
    pub fn result<T, E: std::fmt::Display>(&self, result: &Result<T, E>) {
        if let Err(error) = result {
            self.fail(format_args!("{error:#}"));
        }
    }

    /// Runs `operation`, of which it is the span, its context current meanwhile ([`Span::enter`]),
    /// saying whether it failed ([`Span::result`]).
    pub fn run<T, E: std::fmt::Display>(
        &self,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let entered = self.enter();
        let result = operation();
        self.result(&result);
        drop(entered);
        result
    }

    /// cladefs: awaits `operation`, of which it is the span, its context current as each poll of it
    /// runs ([`Span::around`]), saying whether it failed ([`Span::result`]): [`Span::run`]'s async
    /// form (TASK-685.3).
    pub async fn run_async<T, E: std::fmt::Display>(
        &self,
        operation: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let result = self.around(operation).await;
        self.result(&result);
        result
    }

    /// Makes it the context current on this thread until the guard returned drops: the parent of
    /// the spans started meanwhile ([`Span::start`]). Nothing while it is nothing.
    pub fn enter(&self) -> Option<opentelemetry::ContextGuard> {
        self.0.as_ref().map(|context| context.clone().attach())
    }

    /// `future`, with it as the context current as each poll of it runs: the parent of the spans
    /// it starts, on whatever thread polls it. `future` itself while it is nothing.
    pub fn around<F: Future>(&self, future: F) -> Either<F, WithContext<F>> {
        self.parent().around(future)
    }

    /// It, as the parent of what another task does for it, apart from it ([`Parent::around`]).
    pub fn parent(&self) -> Parent {
        Parent(self.0.clone())
    }
}

/// A span, as the parent of what another task does for it ([`Span::parent`]), which ends it not.
pub struct Parent(Option<Context>);

impl Parent {
    /// cladefs: the context current on this thread, as the parent of what another task does for it:
    /// the span current here, and where its reads went ([`sourced`]), which a task spawned
    /// elsewhere would lose; nothing while spans are off.
    pub fn current() -> Self {
        Self(enabled().then(Context::current))
    }

    /// `future`, with the span as the context current as each poll of it runs, as
    /// [`Span::around`] says.
    pub fn around<F: Future>(self, future: F) -> Either<F, WithContext<F>> {
        match self.0 {
            Some(context) => Either::Right(future.with_context(context)),
            None => Either::Left(future),
        }
    }

    /// cladefs: runs `work` with the span as the context current on this thread meanwhile: the
    /// parent of the spans it starts, on a thread another task hands it to, as a blocking one
    /// (board 8569).
    pub fn run<T>(self, work: impl FnOnce() -> T) -> T {
        let _attached = self.0.map(Context::attach);
        work()
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(context) = &self.0 {
            context.span().end();
        }
    }
}

/// cladefs: where a fetcher found what a read wanted, nearest first, so that the farthest a read
/// went is the greatest (TASK-672.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Source {
    /// The parts and tails the host's fetchers keep in memory.
    Memory = 1,
    /// The host's part cache, on its disk.
    PartCache = 2,
    /// Another host sharing the bucket, which owns the part (TASK-406.2).
    Peer = 3,
    /// The bucket: a GET, or a listing.
    Bucket = 4,
}

impl Source {
    /// cladefs: every source, nearest first.
    pub const ALL: [Source; 4] = [Source::Memory, Source::PartCache, Source::Peer, Source::Bucket];

    /// Its name, as [`SOURCE`] says it.
    pub fn name(self) -> &'static str {
        match self {
            Source::Memory => "memory",
            Source::PartCache => "part_cache",
            Source::Peer => "peer",
            Source::Bucket => "bucket",
        }
    }

    /// The source numbered `number`, if one is: none, for 0, as nothing noted is.
    fn numbered(number: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|source| *source as u8 == number)
    }
}

/// The farthest [`Source`] noted in a context ([`note`]), by number, 0 for none: what [`sourced`]
/// gathers, in the context of the future it runs.
#[derive(Clone, Default)]
struct Sources(Arc<AtomicU8>);

/// cladefs: notes that what the current context reads went as far as `source`, for what gathers
/// it ([`sourced`]), if anything does; nothing while spans are off, at the cost of a load.
pub fn note(source: Option<Source>) {
    let Some(source) = source.filter(|_| enabled()) else { return };
    if let Some(sources) = Context::current().get::<Sources>() {
        sources.0.fetch_max(source as u8, Ordering::Relaxed);
    }
}

/// cladefs: `future`'s output, and the farthest a read of it went ([`note`]), none if no read noted
/// anything: as a page-in, whose reads of the host's cells go to no fetcher, gathers where its
/// fetcher's reads went; or a flight of the fetcher's, whose readers each note where it went, the
/// first to ask and those that joined it alike. None while spans are off.
pub async fn sourced<F: Future>(future: F) -> (F::Output, Option<Source>) {
    if !enabled() {
        return (future.await, None);
    }
    let sources = Sources::default();
    let output = future.with_context(Context::current_with_value(sources.clone())).await;
    (output, Source::numbered(sources.0.load(Ordering::Relaxed)))
}

/// cladefs: sets [`BACKTRACE`] on the span current on this thread, if spans are on and one is: the
/// frames of where it is called, which are, in a future that waited for a read, those of the poll
/// of every future awaiting it, up to its task's (TASK-672.2). Captured as addresses alone, as
/// unwinding the stack costs microseconds and resolving its symbols far more: called where a read
/// went to the bucket, which took milliseconds.
pub fn record_backtrace() {
    if !enabled() {
        return;
    }
    let context = Context::current();
    let span = context.span();
    if !span.span_context().is_valid() {
        return;
    }
    let mut frames = String::new();
    backtrace::trace(|frame| {
        let separator = if frames.is_empty() { "" } else { " " };
        write!(frames, "{separator}{:x}", frame.ip() as usize).expect("a String takes writes");
        true
    });
    span.set_attribute(KeyValue::new(BACKTRACE, frames));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Off, a span is nothing, with no tracer made, and what is said of it is never made: it costs
    /// what a relaxed load does, as a bucket's request's span (its key, its bytes, its error) does.
    /// Prints what one costs, the fastest of 5 rounds of a million (`docs/tracing.md` has the
    /// figures), held to no bound: a debug build on a loaded CI runner took 63 ns.
    #[test]
    fn spans_off_are_nothing_and_cost_a_load() {
        let request = || {
            let span = Span::start("bucket.get");
            span.set_with(OBJECT_KEY, || -> String {
                unreachable!("an attribute made for nothing")
            });
            span.set_count(BYTES, 4096);
            span.result(&Err::<(), _>("failed"));
            std::hint::black_box(span)
        };
        assert!(request().0.is_none());
        assert!(TRACER.get().is_none());
        const SPANS: u32 = 1_000_000;
        let round = || {
            let started = Instant::now();
            for _ in 0..SPANS {
                drop(request());
            }
            started.elapsed() / SPANS
        };
        let fastest = (0..5).map(|_| round()).min().unwrap();
        println!("a span off costs {fastest:?}");
    }

    /// On, a future's reads gather the farthest source noted, nothing for none; a source noted
    /// inside a nested gathering is the inner one's alone, as a flight's is until its reader notes
    /// it on landing.
    #[test]
    fn sources_gather_the_farthest_noted() {
        enable(true);
        let gathered = |notes: Vec<Source>| {
            futures::executor::block_on(sourced(async move {
                for source in notes {
                    note(Some(source));
                }
            }))
            .1
        };
        assert_eq!(gathered(vec![]), None);
        assert_eq!(gathered(vec![Source::Memory]), Some(Source::Memory));
        assert_eq!(
            gathered(vec![Source::Bucket, Source::Memory, Source::PartCache]),
            Some(Source::Bucket)
        );
        let (inner, outer) = futures::executor::block_on(sourced(async {
            note(Some(Source::Memory));
            sourced(async { note(Some(Source::Peer)) }).await.1
        }));
        assert_eq!((inner, outer), (Some(Source::Peer), Some(Source::Memory)));
        assert_eq!(Source::PartCache.name(), "part_cache");
    }
}
