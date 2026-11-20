# Tracing cladefsd with OpenTelemetry

`cladefsd` makes spans of what it does: the requests it serves, the commits they cause and each
commit's phases, uploads to the bucket and each bucket request, store opens and forks,
extent-cache page-ins, and memory-relief passes. It records every span in its own memory and shows
them at `/z/tracez` (see [z-pages](#z-pages)). It exports them over OTLP only when told to.
The code is `crates/bucket_cell/src/trace.rs`, which makes the spans,
`crates/cladefs/src/cladefsd/otel.rs`, which starts the recorder and the exporter (TASK-258), and
`crates/cladefs/src/cladefsd/zpages/`, which serves the pages (TASK-672).

## Turning it on

| Variable | Meaning | Default |
|---|---|---|
| `CLADEFSD_TRACES` | `otlp` exports spans. Any other value except empty makes the daemon refuse to start. Spans are recorded for `/z/tracez` either way. | not exported |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The collector, OTLP over HTTP with protobuf. `/v1/traces` is appended. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` names the full URL instead. | `http://localhost:4318` |
| `OTEL_SERVICE_NAME` | The service the spans belong to. | `cladefsd` |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | OpenTelemetry's standard sampler variables. | see Sampling |

The other `OTEL_EXPORTER_OTLP_*` variables (headers, timeout, compression) work as the
OpenTelemetry SDK documents them. Spans are exported in batches. When SIGTERM drains the daemon,
it exports what it has left before it exits.

### Sampling

Each request, each background flush, each upload and each relief pass starts a trace, and a busy
daemon starts thousands a second. So unless `OTEL_TRACES_SAMPLER` is set, the daemon exports
`parentbased_traceidratio` with ratio `0.01`: one trace in a hundred, whole. The sampler decides
per trace, from the trace ID, as it would for a root span, so every span of an exported trace is
exported. Sampling only limits what is exported: `/z/tracez` records every span.
`OTEL_TRACES_SAMPLER_ARG` alone changes the ratio. To export everything while debugging:

```sh
OTEL_TRACES_SAMPLER=always_on
```

## Pointing it at a collector

Jaeger's all-in-one image (Apache-2.0) takes OTLP on 4318 and serves its UI on 16686:

```sh
docker run -d --name jaeger -p 4318:4318 -p 16686:16686 jaegertracing/all-in-one:latest
CLADEFSD_TRACES=otlp \
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_TRACES_SAMPLER=always_on \
CLADEFSD_BUCKET=... CLADEFSD_TOKEN=... \
cladefsd /run/cladefs/cladefsd.sock
```

Then open `http://localhost:16686` and choose the service `cladefsd`. To route spans through
the OpenTelemetry Collector instead (`otel/opentelemetry-collector`, Apache-2.0), give it an `otlp`
receiver with the `http` protocol and version `OTEL_EXPORTER_OTLP_ENDPOINT` at that receiver.

## Spans

Every attribute is prefixed `cladefs.`. Cells are written in base 32 (`base32_id`), as store paths name them.
Counts are integers. A failed operation has an error status carrying its message. A failed
request also has `cladefs.errno`.

| Span | Made by | Parent | Attributes |
|---|---|---|---|
| `cladefsd.<kind>` (`cladefsd.map`, `cladefsd.write`, `cladefsd.commit`, `cladefsd.read`, `cladefsd.free`, `cladefsd.fault`, `cladefsd.open`, ...) | each request a VM process sends, from its dispatch to its reply | root | `request.kind`, `cell`, `bytes` (read, write, map, unmap, private), `extents` (free, free_many, map, map_many), `durable` (commit), `store` (open, fork), `request_id` (see below), `errno` |
| `store.commit` | every commit of a writer's store | the request, or whatever commits | `cell`, `durable` |
| `commit.seal` | sealing the extents written since the last commit | `store.commit` | `extents`, `bytes` copied |
| `commit.write` | recording the commit in the open segment and queuing its flush | `store.commit` | `epoch` |
| `store.flush` | making ordered commits local: syncs and log appends | `store.commit` if the commit waits for local or uploaded; root for the daemon's 50 ms flushes | `cell`, `commits` |
| `commit.upload_wait` | a commit waiting for the bucket (uploaded: until uploaded; otherwise: within its lag) | `store.commit` | `epoch` |
| `store.open`, `store.create` | opening a store, or making a new one | the request or control call | `store`, `cell` |
| `store.fork` | a fork or version of a store | the request or control call | `cell`, `child`, `version` |
| `cache.page_in` | reading a part of a cached extent into memory | the fault or read that needed it | `cell`, `extent`, `arena`, `bytes`, `source` (see below) |
| `cache.relief` | an eviction pass freeing memory for an arena under pressure | root | `arena`, `bytes` evicted, `extents` evicted |
| `upload.segment` | uploading a segment, its large extents first | root, on the uploader | `cell`, `segment`, `extents`, `bytes` |
| `upload.checkpoint` | writing a cell checkpoint | root, on the uploader | `cell`, `segment`, `extents` |
| `bucket.get`, `bucket.get_range`, `bucket.head`, `bucket.list`, `bucket.list_delimited`, `bucket.create`, `bucket.update`, `bucket.overwrite`, `bucket.delete`, `bucket.delete_batch` | each object-store request (a batch of deletes: one request where the store deletes in batches, as S3's DeleteObjects) | the upload, checkpoint, page-in, open or collector pass that made it | `object_key`, `bytes`, `objects` (listings, batches of deletes), `prefixes` (listings by delimiter) |

Uploads run behind commits, so an upload is a trace of its own rather than a child of the commit
it carries. An uploaded commit's `commit.upload_wait` span shows how long the commit waited for
that upload.

### Request IDs

`cladefs.request_id` is set from `otel::request_id`, the one function that gives a request's ID:
the guest's number of the request the call is for (its virtio extent device's `unique`), which
the VMM's call says (wire `REQUEST`) and the daemon's log records name too (`request_id`, with
the connection's `store`; TASK-260). A call for no guest request, as one the VMM makes of its
own, has none. Work done for the request on other threads, as object storage's reads and the
pager's, reads it from `cladefs::request::current()`.

### Where a page-in's data came from

`cladefs.source` on `cache.page_in` names the farthest place its read went (TASK-672.2):

| Value | Meaning |
|---|---|
| `local` | the host's stores. The fetcher was not used. |
| `memory` | parts and tails the host's fetchers keep in memory |
| `part_cache` | the host's part cache, on its disk |
| `peer` | another host that owns the part |
| `bucket` | a GET or a listing of the bucket: every cache missed |

A read that joins another read's GET while it is in flight also says `bucket`, because it waited
for that GET. When the source is `bucket`, the request that waited for the page-in carries
`cladefs.backtrace`: the stack of the thread that waited, captured as it woke. A page-in runs as a
task of its own, so a stack taken there would show only the runtime. The stack is taken from the
request's own poll instead, which includes every future awaiting the page. It is captured as frame
addresses, in hexadecimal, innermost first, only on bucket misses, and resolved to function names
only when `/z/tracez` shows it. An exported span carries the raw addresses, which mean nothing
outside the process.

## z-pages

With `CLADEFSD_METRICS` naming an address, the daemon serves these pages there, beside
`/metrics`. They are for operators: bind the address to localhost, or to a network only operators
reach.

| Page | Shows |
|---|---|
| `/z/` | the list of pages |
| `/z/tracez` | each span name's counts by latency bucket, failed and running, each linking to its samples |
| `/z/tracez?name=N&bucket=B` | samples of span `N` in latency bucket `B` (0 to 8), whole traces |
| `/z/tracez?name=N&failed` | samples of span `N` that failed |
| `/z/tracez?name=N` | samples of span `N` in every bucket, the slowest first |
| `/z/tracez?name=N&tag=KEY=VALUE` | samples whose trace has a span with that attribute |
| `/z/tracez?name=N&running` | spans `N` running now, the longest running first |
| `/z/pprof/profile?seconds=S` | a CPU profile of `S` seconds (1 to 300, default 30), pprof's protobuf |
| `/z/pprof/flamegraph?seconds=S` | the same, as an SVG flamegraph |

The summary page links to page-ins that went to the bucket:
`/z/tracez?name=cache.page_in&tag=cladefs.source=bucket`.

### tracez

Latencies are wall clock, so a span that waited for the bucket lands in the slow buckets on its
own. The buckets are Census's: >0, >10µs, >100µs, >1ms, >10ms, >100ms, >1s, >10s, >100s. Counts are
since the daemon started.

The recorder holds each trace's spans as they end, until no span of the trace runs. It then keeps
each span as a sample of its name and latency bucket, with the whole trace: the latest 4 of each
name and bucket, and the latest 4 failures. Memory is bounded: 64 spans per trace (more are counted
as dropped), and 4096 traces held at once (the oldest is dropped past that, as when a span never
ends). A span that ends after its trace was kept starts a partial trace of its own.

### CPU profiles

```sh
go tool pprof http://HOST:PORT/z/pprof/profile?seconds=30
curl -o cpu.svg 'http://HOST:PORT/z/pprof/flamegraph?seconds=30'
```

The profiler is pprof-rs (Apache-2.0), with inferno (CDDL-1.0) drawing flamegraphs. It samples
running threads 99 times per CPU second with SIGPROF, walking frame pointers. Every build here keeps
frame pointers (`.cargo/config.toml`). Only one profile runs at a time. A second request gets
`409 Conflict`. SIGPROF interrupts system calls. It is installed with `SA_RESTART`, and the calls
the kernel does not restart fail with `EINTR`, which the daemon's own loops retry (`wire.rs`,
`storage/io.rs`) as Rust's standard library and tokio do. Samples are not tagged with cells or
requests: pprof-rs has no place for tags.

## Cost while off

`spans_off_are_nothing_and_cost_a_load` (`crates/bucket_cell/src/trace.rs`) times a bucket
request's span with tracing off: started, three attributes set, ended. On Linux (aarch64, OrbStack)
it costs under 1 ns in a release build (the test's nanosecond clock reads 0 ns) and 19 ns in a
debug build, the fastest of 5 rounds of a million. With tracing off, no tracer exists and no
attribute is ever computed. The request path adds one more load per request, in
`otel::request_span`.

## Cost while recorded

TODO(TASK-672.1): `a_recorded_span_costs` figures.

## Tests

- `crates/cladefs/src/cladefsd/tests/otel.rs` serves a write, a read and an uploaded commit through
  cladefsd into OpenTelemetry's in-memory exporter. It checks the span tree and its attributes, and
  that nothing is exported while tracing is off.
- `crates/cladefs/tests/tracing.rs` runs the `cladefsd` binary against an OTLP/HTTP collector
  in the test. It checks that the spans arrive at `/v1/traces` and that nothing is sent without
  `CLADEFSD_TRACES`.
- `crates/cladefs/src/cladefsd/tests/otel.rs` also checks that spans are recorded without export,
  and that a fork restored from the bucket reads with `cladefs.source=bucket` and a backtrace that
  `/z/tracez` resolves to `bring_in`.
- `crates/bucket_cell/src/fetch.rs` (`reads_say_how_far_they_went`) checks each source, and that a
  read that joined another's GET says `bucket`.
- `crates/cladefs/src/cladefsd/zpages/` checks the recorder's buckets, samples and bounds, exports of
  whole traces, the pages, and CPU profiles naming the function that ran.
