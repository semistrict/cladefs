# Tracing cladefsd with OpenTelemetry

`cladefsd` makes spans of what it does: the requests it serves, the commits they cause and each
commit's phases, uploads to the bucket and each bucket request, cell opens and forks,
extent-cache page-ins, and memory-relief passes. With `CLADEFSD_TRACES=otlp`, it exports a
sample of its traces, one in a hundred by default, to an OpenTelemetry collector. The span
instrumentation lives in `crates/bucket_cell/src/trace.rs`, and the exporter configuration is in
`crates/cladefs/src/cladefsd/otel.rs` (TASK-258).

## Turning it on

| Variable | Meaning | Default |
|---|---|---|
| `CLADEFSD_TRACES` | `otlp` enables span export. An empty value or `off` disables tracing. Other values fail daemon startup. | off |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The collector, OTLP over HTTP with protobuf. `/v1/traces` is appended. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` names the full URL instead. | `http://localhost:4318` |
| `OTEL_SERVICE_NAME` | The service the spans belong to. | `cladefsd` |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | OpenTelemetry's standard sampler variables. | see Sampling |

The other `OTEL_EXPORTER_OTLP_*` variables (headers, timeout, compression) work as the
OpenTelemetry SDK documents them. Spans are exported in batches, by a processor of the daemon's
(`otel/batch.rs`) that `OTEL_BSP_MAX_QUEUE_SIZE` (2048), `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` (512)
and `OTEL_BSP_SCHEDULE_DELAY` (5000 ms) configure as they do the SDK's; unlike the SDK's, it
counts the spans it exports, drops (its queue full) and fails to export
(`cladefsd_otel_spans_exported_total`, `_dropped_total`, `_failed_total`; board 8565), which the
SDK's dropped unsaid. When SIGTERM drains the daemon, it exports what it has left before it
exits, within what is left of `CLADEFSD_DRAIN` once the cells are drained: a collector that never
answers holds the exit no longer than that bound (board 8569), and the spans it did not take are
lost.

An `https://` endpoint is reached over TLS, rustls's (the `reqwest-rustls` feature
`crates/cladefs/Cargo.toml` names, board 8569), the collector's certificate checked against the
host's roots, or the PEM file `SSL_CERT_FILE` names (a private CA's).

### Trace context of gRPC calls

The gRPC calls the daemon serves, of the control API, its Mount service and the peer service,
and those it makes of its peers, carry W3C trace context (`traceparent`, `tracestate`; board
8569), when `CLADEFSD_TRACES=otlp`. A call served is a span named as OpenTelemetry
names gRPC's, `PACKAGE.SERVICE/METHOD` (`cladefs.cladefsd.v1.Cladefsd/CreateCell`), of kind
server, a child of the span its caller's `traceparent` names, so that a VM manager's trace goes
on into the daemon: the spans of what the call does (`store.create`, `bucket.*`) are its
children. Its attributes are `rpc.system` (`grpc`), `rpc.service`, `rpc.method` and
`rpc.grpc.status_code`; a status other than OK is an error. A call the daemon makes of a peer is a
span of kind client, a child of whatever span made it, whose context it sends: the peer's span of
the call is its child, on the same trace. A caller's sampled `traceparent` is exported independently of the root sampling ratio. A caller's
unsampled trace is skipped. The layer is `crates/cladefs/src/cladefsd/grpc.rs`, around each service and the
peers' channels, so that every method is traced, those added later too.

### Sampling

Each request, each background flush, each upload and each relief pass starts a trace, and a busy
daemon starts thousands a second. A recorded span costs about 5 µs under load: recording every one
took 7% of the daemon's CPU under pgbench (see [Cost while recorded](#cost-while-recorded)). So
unless `OTEL_TRACES_SAMPLER` is set, the daemon makes `parentbased_traceidratio` with ratio `0.01`:
one trace in a hundred, whole. The sampler decides per trace, from the trace ID, as it would for a
root span, so every span of a sampled trace is exported together. `OTEL_TRACES_SAMPLER_ARG`
alone changes the ratio (TASK-707). To record and export everything while debugging:

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

Every attribute is prefixed `cladefs.`. Cells are written in base 32 (`base32_id`), as cell directories' paths name them.
Counts are integers. A failed operation has an error status carrying its message. A failed
request also has `cladefs.errno`.

| Span | Made by | Parent | Attributes |
|---|---|---|---|
| `cladefsd.<kind>` (`cladefsd.map`, `cladefsd.write`, `cladefsd.commit`, `cladefsd.read`, `cladefsd.free`, `cladefsd.fault`, `cladefsd.open`, ...) | each request a VM process sends, from its dispatch to its reply | root | `request.kind`, `cell`, `bytes` (read, write, map, unmap, private), `extents` (free, free_many, map, map_many), `durable` (commit), `store` (open, fork), `request_id` (see below), `errno` |
| `store.commit` | every commit of a writer's cell | the request, or whatever commits | `cell`, `durable` |
| `commit.seal` | sealing the extents written since the last commit | `store.commit` | `extents`, `bytes` copied |
| `commit.write` | recording the commit in the open segment and queuing its flush | `store.commit` | `epoch` |
| `store.flush` | making ordered commits local: syncs and log appends | `store.commit` if the commit waits for local or uploaded; root for the daemon's 50 ms flushes | `cell`, `commits` |
| `commit.upload_wait` | a commit waiting for the bucket (uploaded: until uploaded; otherwise: within its lag) | `store.commit` | `epoch` |
| `store.open`, `store.create` | opening a cell, or making a new one | the request or control call | `store`, `cell` |
| `store.fork` | a fork or version of a cell | the request or control call | `cell`, `child`, `version` |
| `cache.page_in` | reading a part of a cached extent into memory | the fault or read that needed it | `cell`, `extent`, `arena`, `bytes`, `source` (see below) |
| `cache.relief` | an eviction pass freeing memory for an arena under pressure | root | `arena`, `bytes` evicted, `extents` evicted |
| `upload.segment` | uploading a segment, its large extents first | root, on the uploader | `cell`, `segment`, `extents`, `bytes` |
| `upload.checkpoint` | writing a cell checkpoint | root, on the uploader | `cell`, `segment`, `extents` |
| `bucket.get`, `bucket.get_range`, `bucket.head`, `bucket.list`, `bucket.list_delimited`, `bucket.create`, `bucket.update`, `bucket.overwrite`, `bucket.delete`, `bucket.delete_batch` | each object-storage request (a batch of deletes: one request where the object storage deletes in batches, as S3's DeleteObjects) | the upload, checkpoint, page-in, open or collector pass that made it | `object_key`, `bytes`, `objects` (listings, batches of deletes), `prefixes` (listings by delimiter) |

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
| `local` | the host's cells. The fetcher was not used. |
| `memory` | parts and tails the host's fetchers keep in memory |
| `part_cache` | the host's part cache, on its disk |
| `peer` | another host that owns the part |
| `bucket` | a GET or a listing of the bucket: every cache missed |

A read that joins another read's GET while it is in flight also says `bucket`, because it waited
for that GET. When the source is `bucket`, the request that waited for the page-in carries
`cladefs.backtrace`: the stack of the thread that waited, captured as it woke. A page-in runs as a
task of its own, so a stack taken there would show only the runtime. The stack is taken from the
request's own poll instead, which includes every future awaiting the page. It is captured as frame
addresses, in hexadecimal, innermost first, only on bucket misses. The exported span carries
these raw addresses.

## z-pages

With `CLADEFSD_METRICS` naming an address, the daemon serves these pages there, beside
`/metrics`. They are for operators: bind the address to localhost, or to a network only operators
reach.

| Page | Shows |
|---|---|
| `/z/` | the list of pages |
| `/z/pprof/profile?seconds=S` | a CPU profile of `S` seconds (1 to 300, default 30), pprof's protobuf |
| `/z/pprof/flamegraph?seconds=S` | the same, as an SVG flamegraph |
| `/z/log` | current log levels. POST updates the levels or restores their startup values |

### CPU profiles

```sh
go tool pprof http://HOST:PORT/z/pprof/profile?seconds=30
curl -o cpu.svg 'http://HOST:PORT/z/pprof/flamegraph?seconds=30'
```

The profiler is pprof-rs (Apache-2.0), with inferno (CDDL-1.0) drawing flamegraphs. It samples
running threads 99 times per CPU second with SIGPROF, walking frame pointers. Every build here keeps
frame pointers (`.cargo/config.toml`). Only one profile runs at a time. A second request gets
`409 Conflict`. SIGPROF interrupts system calls. It is installed with `SA_RESTART`, and the calls
the kernel does not restart fail with `EINTR`, which the daemon's own loops retry (`wire.rs`) as
Rust's standard library and tokio do. Samples are not tagged with cells or
requests: pprof-rs has no place for tags.

## Cost while off

`spans_off_are_nothing_and_cost_a_load` (`crates/bucket_cell/src/trace.rs`) times a bucket
request's span with tracing off: started, three attributes set, ended. On Linux (aarch64, OrbStack)
it costs under 1 ns in a release build (the test's nanosecond clock reads 0 ns) and 19 ns in a
debug build, the fastest of 5 rounds of a million. With tracing off, no tracer exists and no
attribute is ever computed. The request path adds one more load per request, in
`otel::request_span`.

## Cost while recorded

A sampled trace's request, commit, page-in and bucket request each pay for a real span.
`span-bench` (`crates/cladefs-examples/examples/span-bench.rs`) times a bucket request's span
with tracing off and with every span sampled and its export discarded. The earlier in-memory
recorder had the following costs on the x86-64 test host in a release build on 2026-11-19:

| | per span |
|---|---|
| off | 2 ns |
| recorded | 497 ns |

That is a span alone, warm, on one thread. Under load a recorded span costs ten times as much:
fxfs-guest-pgbench-dax on the test host (TASK-707, 2026-11-21, cells on a RAM-backed disk), every
span recorded, took 0.399 ms of the daemon's CPU per commit, against 0.371 ms with
`CLADEFSD_TRACES=off`: 7% of its CPU, some 5 µs a span at its 8 500 spans a second. Hence the
sampling: a span of a trace not sampled records nothing, and the daemon's CPU is about as with spans
off.

```sh
cargo run --release -p cladefs-examples --example span-bench
```

## Tests

- `crates/cladefs/src/cladefsd/tests/otel.rs` serves a write, a read and an uploaded commit through
  cladefsd into OpenTelemetry's in-memory exporter. It checks the span tree and its attributes, and
  that nothing is exported while tracing is off.
- `crates/cladefs/tests/tracing.rs` runs the `cladefsd` binary against an OTLP/HTTP collector
  in the test. It checks that the spans arrive at `/v1/traces` and that nothing is sent without
  `CLADEFSD_TRACES`; that they arrive at a collector over HTTPS, its certificate of a CA of the
  test's own (`tests/otlp-tls`); that a daemon whose collector never answers exits within its
  drain's bound; and that a control call's `traceparent` is the parent of the daemon's span of it.
- `crates/cladefs/src/cladefsd/otel/batch.rs` checks the processor's counts of spans dropped,
  failed and exported, and that its shutdown waits no longer than it is given.
- `crates/cladefs/src/cladefsd/tests/otel.rs` also checks that a peer call made within a span is
  served as its child, on its trace, and counted in the gRPC metrics on both sides.
- `crates/cladefs/tests/page_in_tracing.rs` checks the exported trace of a restored VM's
  bucket page-in and the request that waited for it.
- `crates/bucket_cell/src/fetch.rs` (`reads_say_how_far_they_went`) checks each source, and that a
  read that joined another's GET says `bucket`.
- `crates/cladefs/src/cladefsd/zpages/` checks CPU profiles naming the function that ran.
