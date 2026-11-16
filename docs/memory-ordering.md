# Memory orderings of the host crates' atomics

For whoever changes an atomic in `crates/cladefs`, `physmem`, `extent_store`, `extent_remote`,
`follower`, `virtio_extent` or the extent device in `vendor/firecracker`: what each atomic that is
not `SeqCst` publishes, which access pairs with which, why that is enough, and which loom model
shows it. A change to one of these orderings, or a new atomic that orders anything but itself,
belongs here, with a model if it carries an argument.

Shuttle, which runs this repo's concurrency tests, treats every atomic as `SeqCst`, so no Shuttle
test can see an `Acquire`, `Release` or `Relaxed` bug. loom can: its model lets a load read any
store that happens-before doesn't hide from it. A model's verdict counts, not an audit's
(TASK-444.2).

## How it is checked

- **Models.** The protocols whose orderings carry an argument are small types the production code
  uses, built on a `sync` module that is std's, or loom's with the crate's `loom` feature:
  `cladefs::sync` (atomics and a `OnceLock` with std's orderings), `virtio_extent::sync`,
  `physmem::sync::atomic`. The models are `loom` test modules beside them, run by
  `scripts/test.sh`'s shuttle part:

  ```
  cargo nextest run -p cladefs --features loom --lib loom::
  cargo nextest run -p virtio_extent --features loom --lib loom::
  cargo nextest run -p physmem --features loom --lib loom::
  ```

  Each runs in well under a second. None needs a preemption bound.
- **Ordering mutants.** `crates/*/loom-mutants/*.patch` each weakens one ordering (`Release` or
  `Acquire` to `Relaxed`) or puts a bug a model found back. `scripts/loom-mutants.sh` applies each,
  runs the model its header names and expects it to fail. A mutant marked `# Fate: equivalent`
  must pass instead: its site is stronger than needed. `scripts/test.sh`'s scripts part runs
  `--check`. The whole run takes about 20 s a mutant.
- **Classes**, after AtomVChecker: (1) an ordering too weak to publish the state related to it;
  (2) an `AtomicPtr` whose pointee isn't synchronised; (3) an ordering stronger than needed. The
  host crates have no `AtomicPtr`, and no `std::sync::atomic::fence` (`crate::halt`, which
  halts a store after a panic, is no memory fence).

The count, as of 2026-11-15: 326 non-`SeqCst` accesses in the production code of these crates,
about 90 more in their tests.

## Protocols with an ordering argument (modelled)

| Atomic | Publishes | Pairing and argument | Model | Mutants |
|---|---|---|---|---|
| `cladefs::hearing::Occurrence::state`: extentd client's daemon-lost news (`Client::lost`) | its listener (`OnceLock`), to `happen` | `listen`: set the listener, then `fetch_or(LISTENING, Release)`. `happen`: `fetch_or(HAPPENED, Acquire)`. Two RMWs of one atomic: the later one sees the earlier one's bit and calls the listener, exactly once. If `happen` sees `LISTENING`, its Acquire reads from `listen`'s Release, so the listener's write happens before `get`. `happened()` is `Relaxed`: its readers come after a request failure, which `happen` precedes on the receiving thread. | `hearing::loom::occurrences_are_told_once` | `occurrence-happens-relaxed`, `occurrence-listens-relaxed`: killed. `occurrence-told-by-a-store-and-a-load` (TASK-444.2.1 put back): killed |
| `cladefs::hearing::Latest::latest`: a follower's writer's latest commit (`Client::committed`) | its listener, to `rise` | `listen`: set the listener, then `fetch_max(0, Release)`, an RMW whose result it returns. `rise`: `fetch_max(value, Acquire)`. If the rise's RMW comes first, `listen` returns at least its value. If it comes after, it reads from `listen`'s RMW (or a later RMW in its release sequence), so it sees the listener. `latest()` is `Relaxed`: only the value. | `hearing::loom::listeners_hear_of_every_rise` | `latest-rises-relaxed`, `latest-listens-relaxed`: killed. `latest-listening-loads` (TASK-444.2.2 put back): killed |
| `virtio_extent::notices::Resizing::{capacity, asked}` | in `answered`, the ask cleared, to a driver reading the capacity it took | `answered` (under `changing`): `asked.store(0)`, then `capacity.store(c, Release)`. `Resizing::known`, the driver's read (`Device::read_config`): `capacity.load(Acquire)`, then `asked`. A read of the taken capacity is ordered after the ask's clearing, so the guest never reads the capacity it took together with the ask it answered (TASK-79.41). Nothing orders `changed`'s pair of stores against a read in this way: a read may take the new capacity with the old ask. The configuration interrupt that `changed` raises afterwards has the driver read both again. | `notices::loom::capacities_taken_are_read_with_their_asks_answered` | `answers-release-no-capacity`, `capacities-read-relaxed`: killed. `answers-clear-asks-relaxed`, `asks-read-relaxed`: equivalent (class 3) |
| `physmem::frames::Frame::staged` | that a second stage mapped the page, to its free | `stage` (by the page's holder, no lock) and `take_staged` (`quarantine::sift`, as the page is freed) are both `Relaxed`. The page's handover orders the mark before the free: a window's lock, an `Arc`'s last drop or a move through a channel. `free` sees the mark by coherence. | `frames::loom::staged_marks_reach_the_free_of_their_page` | none: already `Relaxed`, with no fence to delete |

The other `Frame` fields (`state`, `order`, `prev`, `next`) change only under the PMM's lock,
which orders them. They are atomics only so the array can be shared.

## Orderings stronger than needed (class 3)

Each of these publishes nothing beyond its own value, or is already ordered by a lock or a system
call. Weakening one to `Relaxed` is an equivalent mutant by the argument given. A loom model would
have to model the socket, eventfd or tokio `Notify` that carries the order; TASK-444.2.3 holds that
work. They stay as they are: weakening them would trade a model-free argument for nothing
measurable.

| Site | What orders it instead |
|---|---|
| `cladefs::extentd::client::Client::closing` (Release store in `close`, Acquire load as the receiving loop ends) | `close` stores, then shuts the socket down. The loop ends on the receive that the shutdown ends, a kernel ordering. A daemon going at the same instant is a race no ordering decides. |
| `DaemonExtentStore::copying` (Release in `create_dax_window_in_host_memory`, Acquire in `copies_on_write`) | only the bool |
| `cladefs::extentd::server::Connection::closing` (Release once the connection ends, Acquire in `going`) | only the bool. `going` is a hint the writer path polls. |
| `server::Peer::let_go` (swap AcqRel, loads Acquire) | swapped once, to shut the socket down once; read by `gone` after the connection ended. Only the bool. |
| `server::Counted` connections (fetch_add/sub AcqRel) | a limit counter: only itself |
| `extentd::control::CONTROL_SERVING` (Release, Acquire) | only the bool |
| `extents::ExtentStore::flush_turns` (fetch_add Release, load Acquire) | a count of turns that a test reads as a number |
| `executor::Jobs::running` (fetch_add/sub AcqRel, load Acquire) | `Notify`'s lock orders the wakeup. The executor thread then drops the runtime, whose tasks tokio's own synchronization frees. The jobs' effects reach no one through the count. |
| `dax::uffd::Lift::asked` (fetch_add AcqRel, load Acquire) | the eventfd write that follows the ask, and the read that precedes the load, carry happens-before through the kernel, so the load sees the ask by coherence |
| `extent_store::durability::Helpers` (`update` AcqRel, fetch_sub AcqRel) | a semaphore count of helper threads: only itself |
| `extent_remote::trace::ENABLED` (Release store, Relaxed load) | the Release pairs with no Acquire. `TRACER` is a `OnceLock` with its own ordering, and a span that sees `enabled()` before `TRACER` is set makes none. |
| `physmem::physmap::PhysMemory::chunks` (Release store in `assign_chunk`/`release_chunk`, Acquire loads) | chunks are assigned and released under the PMM's lock, which also puts their pages on the free lists. Any holder of a page took it under that lock. `is_free` (a test's query) reads the value alone. |
| `physmem::pmm::relief::Reliever::running` (Release in `Gone::drop`, Acquire in the allocator) | only the bool. The `room` pulse after the store is a lock (`Pulse`) and wakes waiters. |
| `virtio_extent::notices::{Following::committed, StatsAsks::asked, Notices::frozen}`, `Resizing::opened`/`changed` stores | only their values. The configuration interrupt raised after a change carries the order to the guest (an eventfd, through KVM). |

## Relaxed flags and hints ordered by something else (class-1 candidates, argued safe)

| Site | Why `Relaxed` is enough |
|---|---|
| `cladefs::extentd::memory::InFlight::{held, budget}`, `TENANT_IN_FLIGHT`, `GOVERNED` | `held` is changed only by RMWs, so the budget check is atomic on one location. Waiters use event-listener 5.4.2, whose `notify` always takes the list's lock and so serializes with a listener's insertion. A taker that registers and then checks again (`take`) misses no `sub`. |
| `cladefs::extentd::disk::Disk::{used, limit, full, nudged}`, `TOTAL_LIMIT`, `USED` | quotas updated by RMWs on one location each. `nudged` is swapped to true, then signalled under `NUDGED`'s lock, and swapped back by the recounter before it counts: no nudge lost. |
| `cladefs::cache::CachedExtent::{present, used}`, `CACHE_CLOCK`, `MOST_CACHED`, `TRIM_AT` | eviction hints: `present` changes under the pages' lock and is read as a hint before that lock is taken. The clock orders uses only roughly. |
| `cladefs::dax::uffd::Busy::{doing, since, serving}` | a watchdog's diagnostic. The two loads may disagree with each other. |
| `cladefs::profile::Profile::written` | a once-flag (swap). The profile's file is the real guard (`path.exists()`). |
| `extent_remote::marks::Watch::{any, marked, marking, marking_due}`, `Held::read` | `any` and `marking` are hints in front of the `cells` lock, which guards the marks. A stale hint only delays a warning. `marking_due` is a CAS so that one caller per refresh rereads. |
| `extent_remote::object_bucket::{ObjectBucket::honoured, Failing::on}` | `honoured` caches a probe that is idempotent. `on` is a hint in front of the `run` lock. |
| `extent_store::dir::Dir::bypassing` | the last open's answer, a diagnostic |
| `extent_store::sync::REFUSING` (Shuttle builds only) | a test's switch, set before the threads it affects |
| `vendor/firecracker` extent `window::KvmPrefault::failed` | a log-once flag (swap) |
| `cladefs::extentd::server::Peer::mappings` | set once, as a window is made, before the replies that it bounds |

## Counters, statistics and IDs

These publish nothing. Each is an RMW or a load of a value that is read as a number.

- cladefs: `stats::COUNTS`; `heap::{HELD, PEAK, LIMIT}` (tests' global allocator); `extents::OPEN_MAPS_REFUSED`;
  `bucket::peers::Counters`; `dax::Gauges` (window state's gauges); `dax::uffd::{Taken, REFUSED, CHANGED}`;
  `extentd::client`'s `Client::next`, `StageCosts` and `Transfer::used`; `extentd::metrics::Requests`;
  `extentd::stats`'s `ASKS`, `IDS` and each VM's `costs`, `private` and `installed`; `extentd::peers::{SERVED,
  SERVED_MISSING}`; `extentd::server::REFUSED`; `extentd::stores::{STATS_WRITTEN,
  UPLOADS_WHEN_ASKED}`; `extentd::wire::AsyncSender::queued`.
- physmem: `pmm::{free_count, alloc_failed_no_mem}`; `physmap::create_shared_memory`'s `NEXT`.
- extent_store: `segments`' timing statics; `written::WRITES`; `framed::{OPENED, UNPACKS}`;
  `framing::SEEN_AHEAD`; `timings`.
- extent_remote: `store::Counts` (requests, bytes, failures); `client::Progress::last`;
  `checkpoint`, `fetch` and `sweep` counters; `collector`'s; `bin/cladefs-gc-fill`'s.
- virtio_extent: `requests::Served`.

## Test-only atomics

The atomics in test modules (`#[cfg(test)]`), in Shuttle builds (whose atomics are `SeqCst`
whatever they ask) and in `tests/` are switches a test sets before the threads it affects start,
or counters a test reads after joining them. Examples: `cladefs::bucket`'s `Gated` store (`shut`,
`full`, `held`, `went`, `writes`), `extentd::tests`, `store_table`'s and `store_requests`' Shuttle
tests, `follower`'s test listener, and `extent_remote::{branch, collector, fetch}` tests. The join
or channel that ends the test orders the read after the writes.

## What the audit found

- TASK-444.2.1: extentd's daemon-lost handoff was a store-buffering pattern. The lost flag was
  stored and then `on_lost` loaded; `on_lost` was set and then the flag loaded. loom told the
  listener 0 times. Under `SeqCst` alone it could also be told twice, from both threads, which
  meant two concurrent `process::exit` calls in Firecracker's listener. Fixed by `Occurrence`.
- TASK-444.2.2: a follower's device read the latest commit with a load after setting its
  listener, the same pattern against the rise. loom showed a commit heard by neither. Fixed by
  `Latest::listen`, which returns the value through an RMW (`DaemonExtentStore::on_committed`
  now returns it).
