---
id: doc-24
title: 'Full-stack deterministic simulation: N daemons, one seed'
type: specification
created_date: '2026-11-20 03:31'
updated_date: '2026-11-20 08:00'
---
# Full-stack deterministic simulation

FoundationDB runs its whole cluster, every role, in one process under one seed: the network, the
disks and time are simulated, faults are injected, and a seed replays exactly. cladefs has that
for its storage layer alone (`local_cell::sim`, TASK-88, TASK-680): the stores, their uploaders,
the bucket protocol and, since TASK-680, replicated cells and their members, but not cladefsd. The
daemon's own logic (its control API, its cell table and placements, its peers over gRPC, its page
cache, its pools and governors, the restarts that reopen its stores) runs only in hand-written
tests and on ubu1's end-to-end runs, which find what they look for and nothing else. TASK-678, a
commit told Replicated lost on a retake, was found by reading the code against the spec; the
replicated simulation then found two more defects in a night (TASK-680's base gap, TASK-683) and
the review behind it three. This doc is how to get the same at the daemon's level.

## Goal

N cladefsd daemons in one process, one thread, under one seed, sharing a simulated bucket and a
simulated network; guests driving them through what a VM process asks; daemons killed, their
disks kept (crashed, unsynced writes kept, lost, torn or garbage) or lost, and restarted; the
network between them partitioned (one way or both), delayed and lossy; the bucket slow, failing
or out of reach; every step checked against what each guest was told (doc-12's rows: Ordered,
Local, Replicated, Uploaded). A seed that fails replays, request for request.

## The boundary

Inside: everything of cladefsd's above the kernel. Outside, simulated: the kernel's parts that a
simulation can't run, replaced at the protocol the daemon already speaks.

| What | In the simulation |
|---|---|
| Control API (tonic over a Unix socket) | tonic over the simulated network (TCP); the socket is a deployment detail |
| VM processes (the VM socket, memfds, `SCM_RIGHTS`, userfaultfd, the DAX window, KVM) | a simulated guest calling the daemon's request handling (`cell_requests`, `wire`'s requests) in process: READ, WRITE, COMMIT at each level, FREE, versions; no window, no stage operations |
| Peers (tonic over TCP) | as is, over the simulated network |
| The host's disk (`HostDir`, `std::fs`) | `MemDir` per daemon, crashed with `MemDir::crash_corrupting` on a kill |
| Object storage (`object_store` over HTTP) | an in-process bucket, shared by the daemons, its requests delayed, failed before or after landing, and out of reach as the seed says |
| Time, randomness | simulated (below) |
| FUSE mounts, the guest kernel | not simulated: TLA+ (`specs/dax-cow`, `specs/journal`), Shuttle, loom and ubu1's guest suites keep them |

## The runtime: madsim

[madsim](https://github.com/madsim-rs/madsim) (Apache-2.0; madsim-tonic-build MIT; RisingWave's
simulation) swaps tokio and tonic, under `RUSTFLAGS="--cfg madsim"`, for simulated ones, by
renaming the packages (`tokio = { package = "madsim-tokio" }`): built normally they are tokio and
tonic. Checked 2026-11-19 against its source (madsim 0.2.34, madsim-tokio 0.2.30, madsim-tonic
0.6.0+0.14):

- **Nodes**: `NodeBuilder` makes one per daemon, with its own IP and name; `Handle::kill` drops a
  node's tasks and closes its sockets, `restart` runs its `init` again. Exactly a host's crash and
  boot.
- **Time and randomness**: it intercepts `clock_gettime`, `gettimeofday`, `getrandom` and
  `getentropy` at the libc level, so `Instant`, `SystemTime`, `HashMap`'s seeds and `rand` follow
  the seed (getrandom on Linux needs its patched crate). `tokio::time` is simulated.
- **Network**: TCP between nodes, `clog_link(src, dst)` one way, `clog_node`, packet loss and
  latency (`Config`); streams stall while clogged rather than lose data, as TCP does.
- **tonic 0.14**, as cladefs uses (madsim-tonic 0.6.0+0.14), over its TCP; connectors, TLS and
  HTTP/2 settings are ignored.
- **Determinism checking**: `MADSIM_TEST_CHECK_DETERMINISM` runs a seed twice and compares every
  random draw and its time.

What it does not do, and how this design meets it:

- **No Unix sockets** (`todo!()`): the control API serves on TCP under `cfg(madsim)`; the VM socket
  is not simulated (the guest calls in process).
- **No real threads**: `std::thread::spawn` panics in a simulation. Every thread cladefsd starts
  (about 100 sites: framing, flushers, the request pool, the pager, replicators, the page cache's
  flusher) goes through one seam, a task under `cfg(madsim)`. `Replicator::idle`/`drive`
  (TASK-680) and `upload::Spawn` are the pattern.
- **No blocking**: `block_on` and current-thread runtimes are unimplemented, `block_in_place`
  absent, `spawn_blocking` runs inline. cladefs's `blocking()` helpers run their closures inline
  under `cfg(madsim)` (a simulated host's disk is memory); the runtimes cladefsd builds (server,
  control, peers, bucket, pager) become madsim's one; the synchronous APIs that `block_on` (the
  `OpenCell` calls cladefsctl uses) stay out of the simulation.
- **Its filesystem has no crash model** (`power_fail` is a TODO): not used. Every file cladefsd
  writes goes through `Dir`, `MemDir` per node, which does (`MemDir::crash`, `crash_corrupting`).
- **One process, so one set of statics**: the next section.

turmoil (MIT, tokio's) simulates the network and, behind a feature, a filesystem with crash
semantics, but neither time nor randomness nor threads as far as its docs say: weaker than madsim
plus `MemDir`. Building our own instead (driving cladefsd's internals with `local_cell::sim`'s
scheduler, tonic behind a transport trait) gives finer control but redoes what madsim does; it is
the fallback if the spike finds madsim can't host cladefsd.

## Process state becomes per node

cladefsd keeps its state in about 140 statics (cladefs 88, local_cell 35, bucket_cell 15: the
bucket, the peers and members, the cell table, the request pool and flushers, the memory and disk
governors, metrics, z-pages' recorder, drain and serving flags). madsim's nodes share one process,
so two simulated daemons would share them.

A `node_local!` static: a plain static built normally; under `cfg(madsim)`, one value per node,
keyed by `madsim::runtime::NodeHandle::current().id()`, made at the node's first access and dropped
when the node is killed. A mechanical conversion of each static (`X.lock()` becomes
`X.with(|x| x.lock())`, or a `Deref` where it reads well). Statics that are per process by nature
(the panic hook, the logger, constant tables) stay. Metrics and z-pages become per node too, so
that a simulated host's `/z/` pages are its own.

## The guest and the checker

A simulated guest per open store: writes extents, commits at each level (Ordered, Local, and its
cell's fsync level: Replicated or Uploaded), frees, cuts versions, forks; as `local_cell::sim`'s
guests do, but through the daemon's request handling, so that its pools, its per-connection limits
and its cell table run. The checker is `local_cell::sim::World`'s, lifted to the daemon: what each
guest was told per level against the bucket and against the store of the writer that serves after
every take; and doc-12's rows: a daemon killed with its disk keeps what was Local; a host lost,
what was Replicated; every host lost, what was Uploaded.

## Faults (FoundationDB's workloads, ours)

- **Attrition**: a daemon killed, its disk kept (crashed, corrupting) or lost; restarted at once or
  later; a whole region's daemons at once.
- **Network**: partitions between daemons (one way, both, shifting as FoundationDB's swizzle
  does), clogs, loss, latency; a daemon cut off from the bucket but not from its peers (a zombie
  writer).
- **The bucket**: requests failing before or after landing, slow, out of reach for a stretch
  (`bucket_cell::faults`).
- **Operations**: takes elsewhere, forced takes, `SetPeers` changes, drains, deletions, forks,
  versions, collector passes, disk and memory pressure (the governors).
- **Rules off**: each protocol rule in `bucket_cell::Rules` and `local_cell::Rules` has a profile
  that finds its violation, as today's simulation has (`test_needs!`). This is FoundationDB's
  negative tests and `SimBugInjector`, real bugs put in to show the checker catches them. It is not
  their BUGGIFY, which we have no equivalent of yet (below).

## What the spike found (TASK-685.1, 2026-11-20)

- **Normal builds are unchanged** with tokio renamed to madsim-tokio and tonic to madsim-tonic
  in the workspace's manifests: madsim-tokio re-exports tokio, madsim-tonic tonic.
- **Under `--cfg madsim`**, crates outside the simulation name tokio itself: Fxfs's executor and its
  FIDL shims (`fuchsia-async`, `fuchsia-emulated-handle`, `fidl-fuchsia-io`: `LocalSet`,
  current-thread runtimes, UDP) and `extent_keys` (its KMS backends' own HTTP runtimes; the
  simulation uses the tests' KMS). The simulated crates needed, to build: `bucket_cell` 4 changes
  (`blocking()` inline, `tokio::select!` as `futures::future::select`, `JoinError::into_panic`
  absent, the test S3 server left out), `local_cell` 2 (`blocking()` inline, the bucket runtime's
  `on_thread_start`). **cladefs's library builds as it is**; its tests need 7 changes
  (current-thread runtimes, the S3 server, runtime metrics).
- **madsim runs the bucket protocol**: `bucket_cell/tests/madsim.rs`, three hosts as nodes taking a
  cell's head at times the seed picks, one killed and started again taking it back: 20,000 seeds
  in 3.7 s on the Mac; `MADSIM_TEST_CHECK_DETERMINISM` holds.
- **What stops cladefsd booting as a node is at run time, not at build time**: its `main` reads
  its configuration from the environment and the process's statics, spawns threads, parks the
  main thread, binds Unix sockets (madsim's are `todo!()`); about 100 thread spawns, about 140
  statics; its page store (memfds, userfaultfd) is Linux's. So a daemon core that boots from a
  configuration value, as an async task, comes first.
- Its dependencies are MIT, Apache-2.0, or both (madsim, madsim-macros, madsim-tokio,
  madsim-tonic: Apache-2.0).
- **Decision: go on with madsim.** The madsim build's target is `target/madsim`
  (`CARGO_TARGET_DIR`), as `--cfg madsim` rebuilds everything.

## What the thread inventory found (TASK-685.3, 2026-11-20)

cladefsd starts its threads at 41 sites outside tests. They fall into four groups:

- **12 become tasks**: the control, peers, VM-receive (`server::IO`), pager and bucket runtimes;
  the request pool and the flushers; the HTTP server; the sweep and stats loops.
- **5 already run inline** on a serial `Dir` (`Dir::concurrent()`); only `framing::on_threads`
  is not gated yet.
- **4 have step functions**, which the simulation drives itself:
  - the memory governor (it needs a step taking the host's memory);
  - the disk governor (`govern_once`);
  - the page cache's flusher (`flush_pass`);
  - the replicators (`Replicator::idle`/`drive`).
- **16 are outside** the simulation: the VM socket, FUSE, userfaultfd, signals and Fxfs, plus 3
  sites in other binaries.

**The hard part is blocking, not threads.** A request job is synchronous. On its pool thread it
waits on:

- condvars, for durability and upload waits;
- a guest's acknowledgements;
- the pager, through `futures::executor::block_on`.

There are also about 105 `block_on` calls outside tests:

- `upload::block_on`, and the bucket runtime's;
- `delete_cell`, and the region lookup in `peers::start`;
- the replicator's gRPC member;
- the control API's `spawn_blocking` work: OpenCell create, open, take and fork, which wait on
  uploads.

madsim runs one thread. A task that blocks on a condvar stops the whole simulation, because the
task that would wake it never runs. And madsim has no `block_on` at all. So turning threads into
tasks is not enough: every wait on these paths has to become an `.await`, which means the request
path becomes async from the pool down to the store's durability and upload waits. That is a
larger change than step 4 assumed. The ways forward:

1. **Make the daemon async down to its waits** (madsim as planned). This is the biggest change.
   The request path is `local_cell`'s synchronous API (durability, uploads), which the in-process
   device and the CLI share.
2. **A scheduler of our own over real threads**, as `local_cell::sim` and Shuttle are. Shuttle
   (Apache-2.0, already used here) schedules threads, mutexes and condvars deterministically, but
   has no network or time. This needs a simulated network, clock and runtime of our own (the
   in-house fallback above).
3. **Simulate the async layers alone**: control, peers, the bucket protocol, replication through
   `Replicator::drive`, and `local_cell::sim` below them, without the request pool. Most of what
   the replicated simulation does already, at the daemon's level, and no more.

**Decision (the user, 2026-11-20): 1, the daemon async down to its waits**, madsim as planned.
Every wait on another task's work on the request, control, peer and replication paths becomes an
`.await` (an async form beside each synchronous one that the in-process device, the CLI and FUSE
storage keep), the request pool's jobs become tasks, and `block_on` leaves the daemon's paths. The
plan for it is TASK-685.3's, sized by an inventory of those waits.

### The async rewrite's plan (TASK-685.3, inventory 2026-11-20)

The inventory found 14 kinds of cross-task wait on the request, control, peer and replication
paths. They group by the primitive they share, so one change moves a group:

- uploader progress waits, all through `upload::Shared::wait`;
- bucket calls through `upload::block_on`, about 12 functions;
- the store's open and close handshake with its uploader task (`start.recv()`, and
  `Uploader::drop`'s `stopped.recv()`), which has no async form today;
- the cell table's blocking holds;
- the read path (the pager's `bring_in`, the fetcher);
- request dispatch (the pool, `block_in_place`, `spawn_blocking`, `cells::blocking`);
- replication (a thread per member, synchronous `Members` and `MemberLink` over `block_on`);
- the peers server, sweep, drain and control threads;
- the memory shrink's parking, which never blocks without DAX windows.

Many async forms already exist beside the sync ones:

- `Uploader::uploaded`;
- `OpenCell::uploaded`;
- `CellTable::hold_async`;
- `cells::close_kept`;
- `bring_in`.

About 29 `block_on` calls are on the daemon's paths. The rest of the ~105 are in the CLI, the
client and tests.

`local_cell`'s own condvars (the write gate, framing, preparing, idle) can't block under a serial
`Dir`, because the work they wait for runs inline, as long as nothing awaits between `begin_write`
and `finish_write`. They get fail-fast asserts, not async forms.

**One code path (the user, 2026-11-20).** A simulation is worth what it shares with production,
so production runs the code the simulation runs. Each wait has one form, async. The daemon awaits
it all the way up, on tokio in production and on madsim in the simulation, with no blocking form
beside it and no `block_on` in the daemon's code. Only callers outside the daemon block on it, at
their outermost entry point: `cladefsctl`'s `main`, FUSE storage's threads, the tests' bodies. Under
madsim, step 1's panics fail any `block_on` or blocking wait left on the daemon's paths. Each step
converts a path end to end, smallest first:

1. Fail fast under `cfg(madsim)`: "would block" panics in each blocking wait above, and
   `framing::on_threads` gated on `Dir::concurrent()`.
2. `Shared::wait_async`, and `Uploader::lag` and `Uploader::replicated` as async forms.
3. The bucket calls as `async fn`s, their sync forms `block_on` of them.
4. `OpenCell::commit_async` at every level, with write barriers. COMMIT and WRITE are served as
   futures through `serve_future`, which polls once inline, so the hot path (TASK-41) keeps
   serving without a thread wake.
5. The read path async: the fetch and `bring_in` awaited, READ and MAP as futures, the boot
   profile's replay and writer as tasks.
6. The store's open and close with its uploader: `upload::open_async`, `Uploader::stop`, and
   `OpenCell`'s and `host_cell`'s open, create, take and close in async forms.
7. The control handlers on the async forms: `blocking()` keeps only disk work.
8. VM OPEN (writer and follower), bases, VERSION, DROP_VERSION and FORK as futures.
9. An explicit async close for every store the daemon owns, and under madsim an assert in
   `Drop` that it was closed. The ordering rules of TASK-54, TASK-79.34 and TASK-129 rest on
   today's close in `Drop`.
10. Replication async: `Members` and `MemberLink` returning futures, members served by tasks,
    `local_cell::sim`'s synchronous `drive()` kept.
11. The dispatch seam under madsim: the pool, the flushers, `block_in_place`, `spawn_blocking`
    and `cells::blocking` as tasks, or inline.
12. The peers server, sweep, drain and control as tasks.

The hardest are 9 (a store's close can't be awaited in `Drop`, and its last reference goes
wherever the daemon drops it), 5 (`OpenCell::read` nests the pager, the fetcher and the bases'
open under a synchronous call that DAX faults share) and 10.

Before the decision, the steps that served any of the three went on:

- per-node statics (done);
- a per-node environment and the daemon core (step 2);
- disk through `Dir`;
- the in-process bucket.

## Steps

1. **Spike** (a day): cladefs builds under `RUSTFLAGS="--cfg madsim"` with the renamed packages,
   two simulated daemons boot, one makes a cell and the other takes it over the in-process bucket.
   It lists what breaks; the steps after are sized by it. If madsim can't host cladefsd, this doc
   is rewritten for the in-house fallback.
2. **A daemon core that boots from a configuration value**, as an async task: cladefsd's `main`
   reads the environment and calls it; the simulation calls it once per node. Its sockets
   (control, peers, metrics) given to it, TCP under madsim. (Added by the spike.) As built
   (TASK-685.9), the configuration value is the node's environment
   (`node_local::env`, which `node_local::env::set_vars` gives a simulated node). The daemon's
   modules read it as they read the process's, so production and the simulation configure a
   daemon with the same code. The core, `cladefsd::boot`, binds its own sockets in the order
   `main` did.
3. **Per-node statics** (`node_local!`), crate by crate.
4. **Threads and runtimes** through one seam (tasks under madsim), `blocking()` inline.
5. **Disk through `Dir`** everywhere cladefsd writes (its stores, replica directories, the part
   cache, z-pages' files, its state files).
6. **The in-process bucket** with faults, and the control API on TCP under madsim.
7. **The simulated guest and the checker**, one profile (attrition alone), seed replay checked.
8. **Fault profiles**, then a rule search per rule.
9. **CI**: a few hundred seeds per profile on each push, a soak at night, failing seeds kept as
   regression seeds, as `local_cell::sim`'s are.

## What FoundationDB does that we don't (2026-11-20)

Read in `~/src/foundationdb` at 7e7611f82 (2026-11-06). The user asked for this to be compared
again once the full-stack simulation runs (TASK-685.11).

FoundationDB's simulation finds bugs through five things together. We have one of them, the heal
and check phase (`World::run`'s quiesce).

- **BUGGIFY at two levels** (`flow/include/flow/Buggify.h`, `buggify(p)`):
  - Each site is switched on once per run, with probability 0.25, at its first evaluation, and the
    run logs which sites are on.
  - An active site then fires with probability `p` each time it is evaluated.
  - So each run has a few sites misbehaving often, rather than every site a little.
  - It is on in 80% of runs.
  - Its sites inject legal but unusual behaviour: partial writes, extra yields, a disk sync slow up
    to 1 s, writes reordered, batches of one, odd size limits, retry delays jittered, and unusual
    paths forced.
- **Knobs randomized per run.** About 356 `if (randomize && BUGGIFY)` sites set a knob to an
  extreme value: a queue's bytes from 15e6 to 5000, a parallelism from 32 to 2.
- **A random mix of fault workloads per run** (`CompoundWorkload::addFailureInjection`), each
  with randomized parameters, besides the test's own workloads, and a random cluster configuration
  per seed. Faults are bounded by what the system claims to survive (`canKillProcesses`
  downgrades a kill that would exceed it). Some faults are aimed using the cluster's own state
  (`Rollback.cpp` clogs exactly the links that force a rollback).
- **A fault window, then heal, then check**: faults stop after 450 simulated seconds, and the
  database must then go quiet and pass its consistency checks within a bound.
- **Code probes** (`CODE_PROBE(cond, comment)`, 762 sites):
  - Each probe is registered statically, so a run knows the probes it never hit (`Covered=0`).
  - The harness counts, across an ensemble of runs, how many runs hit each probe, and reports a
    probe not marked `rare` that too few runs hit.
  - A probe names a situation (a take while an upload is in flight and the bucket is down), not a
    single branch.
- **Also:**
  - Any `SevError` event not from an injected fault fails a run.
  - An unseed digest (one last random draw) is compared on 5% of seeds, run twice.
  - Sim-only asserts (`ASSERT_WE_THINK`).
  - Tests are scheduled by runtime × priority.

**What we adopt, by bug-finding value per effort:**

1. **`buggify!(p)` with FoundationDB's two levels.** A site is on if `hash(seed, file, line) <
   0.25`, so an unrelated change moves no kept seed. It fires from the world's random source, and
   it is on in 80% of seeds. It is a constant `false` outside the simulations, compiled out, not a
   runtime flag. Sites:
   - the uploader's batch and segment thresholds;
   - framing sizes;
   - retry intervals, backoffs and timeouts;
   - the page cache's size and the governors' limits;
   - yields in long loops;
   - early wakes where the caller must retry.
2. **A mixed profile per seed** (`Profile::swarm(seed)`): which fault families are on, and their
   parameters, drawn as `addFailureInjection` draws them. The named profiles stay for the rule
   searches.
3. **Knobs**: the protocol's constants in one value, extreme under `buggify!` in the simulations.
4. **`probe!(cond, "comment")`** beside llvm-cov's branch coverage (TASK-685.10):
   - Probes are registered statically, so the unhit ones are known.
   - Each seed records which probes it hit.
   - CI fails a probe not marked rare that too few seeds hit, the threshold set for our seed counts.
5. **Faults aimed at the protocol's state**:
   - a writer cut off from its members but not from the bucket in mid-commit;
   - a kill between the head swap and its acknowledgement;
   - one member clogged during a retake.
6. **The full stack's heal phase**: faults stop, then the run must reach Uploaded, its replicas
   converged, within a bound, and the leak check after. Kills beyond f hosts are downgraded or
   tagged, so that each of doc-12's rows means what it says.
7. **Every error not injected fails a seed**: a logging layer in the simulations, errors the fault
   layers inject marked so.
8. **A determinism digest per seed**, with `MADSIM_TEST_CHECK_DETERMINISM` (or `replays`) on about
   5% of CI's seeds, both traces kept on a mismatch.
9. **Timers jittered and I/O short**:
   - under `buggify!`, heavy-tailed delays added to the daemon's sleeps (madsim's tasks are picked
     at random, but its timers aren't jittered);
   - `MemDir` and the simulated bucket answering short reads and writes, and slow syncs.
10. **Reseeding from a step** for triage: a failing seed replayed to step k, then reseeded, mapping
    how likely the failure is from each k.

**What doesn't transfer:**

- Upgrade tests across binaries: no existing buckets.
- Stackful coroutines for blocking code (`CoroThreadPool`): this is the alternative to the async
  rewrite, worth keeping in mind only for code the rewrite can't reach.
- Real threads marked `noUnseed`: madsim panics on threads.
- Joshua-scale thresholds.
- Knobs randomized in the production binary: compiled out instead.
- Whether `MemDir::crash_corrupting` covers what `AsyncFileNonDurable` models (writes reordered
  unless they overlap, sectors torn) is not checked yet.

## Costs and risks

- A second build of the workspace (`--cfg madsim`), in CI and on ubu1.
- madsim is maintained by one company; its tokio shim is at 0.2.30 (2024-10) against our pinned
  tokio 1.53.1. The spike checks the shim builds against it; a fork, if needed, is Apache-2.0.
- Every new thread, static or `std::fs` call in cladefsd must go through the seams, or the
  simulation build fails (threads panic, statics are shared): a lint (TASK-444.6's script) keeps
  them out.
- The simulation does not cover the kernel paths (DAX, FUSE, userfaultfd), which keep their own
  tools.
