---
id: doc-24
title: 'Full-stack deterministic simulation: N daemons, one seed'
type: specification
created_date: '2026-11-20 03:31'
updated_date: '2026-11-21 02:17'
---
# Full-stack deterministic simulation

FoundationDB runs its whole cluster, every role, in one process under one seed: the network, the
disks and time are simulated, faults are injected, and a seed replays exactly. cladefs has that
for its storage layer alone (`local_cell::sim`, TASK-88, TASK-680): the open cells, their uploaders,
the bucket protocol and, since TASK-680, replicated cells and their members, but not cladefsd. The
daemon's own logic (its control API, its cell table and placements, its peers over gRPC, its page
cache, its pools and governors, the restarts that reopen its cells) runs only in hand-written
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
| The host's disk (`HostDir`, `std::fs`) | `host_fs`: a disk in memory per daemon, crashed at its restart, unsynced changes kept, lost, garbage or torn (TASK-685.4) |
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

As built, a node's values are leaked, not dropped: a killed node's tasks may leave references to
them anywhere. What a value holds outside the process's memory, a static made
`NodeLocal::releasing` lets go of once its simulation ends, as the simulation's thread exits:
physmem's memory, its mapping and its arenas' shared memory objects (board 2890), which a process
running simulation after simulation (madsim's `MADSIM_TEST_NUM`) otherwise kept, about six
descriptors a simulation, until it ran out. The leaked values' memory stays: a megabyte or so a
simulation, which is why scripts/test.sh still runs each seed in a process of its own.

## The guest and the checker

A simulated guest per open cell: writes extents, commits at each level (Ordered, Local, and its
cell's fsync level: Replicated or Uploaded), frees, cuts versions, forks; as `local_cell::sim`'s
guests do, but through the daemon's request handling, so that its pools, its per-connection limits
and its cell table run. The checker is `local_cell::sim::World`'s, lifted to the daemon: what each
guest was told per level against the bucket and against the cell of the writer that serves after
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

## The disk as built (TASK-685.4, 2026-11-20)

Not `Dir` threaded through every caller, which would have made the cell directory roots, replica roots, the
ephemeral journal, version directories and the part cache take a root `Dir` each, and given `Dir`
`exists`, `remove_dir`, identities and holes: a seam beneath, path-based, `host_fs`.

- **`host_fs` is `std::fs`** built normally, re-exported, with what the daemon does on a file's
  descriptor beside it: `punch_hole`, `data_ranges` (`SEEK_DATA`/`SEEK_HOLE`), `sync_with`
  (macOS's barrier and full syncs), `Flock`, `FileExt`/`MetadataExt`/`OpenOptionsExt`,
  `create_dir_all_durably`, and what `Path` asks the file system (`exists`, `is_dir`). Production
  pays nothing.
- **Under madsim, each simulated host's disk is in memory**, of the same calls: a tree of inodes;
  files' bytes durable as of their last sync, directories' entries of files as of theirs, and
  directories once made (`local_cell::MemDir`'s model, which the cells' own crash tests are
  written to, and ext4's with the next fsync); what changed since kept in order. A host started
  again finds its disk crashed at its first call: what was durable, each change since kept or not
  as the seed says, in order, each block of each write whole, not at all, garbage, or torn, its
  garbage in the bytes the write changes alone: powersafe overwrite, which cladefs assumes of its
  disks (decision-3, board 5890). A host's locks of files are its disk's, gone with the crash.
- **Who goes through it**: local_cell and bucket_cell wholly (`DiskDir`, `DiskFile`, the part
  cache's files, `HostDisk::of`, which samples a terabyte under madsim), and cladefsd's serving
  files: `host_cell`, `open_cell`, `cell_dir`, `profile`, `ephemeral_host`, `cladefsd::{cells,
  server, peers, boot, migration}`, and an image's device, which an import's storage and FUSE's
  read and write through. A simulated host's file has no descriptor: the device's I/O, by its
  files' descriptors on a real host (io_uring, `storage::io`), runs each operation in place on the
  simulated disk instead, as it is submitted. And Fxfs, which an import fills the image's
  filesystem with: its executor (`cladefs::executor`) is the simulated host's node, and
  fuchsia-async's tasks and timers are madsim's; its executors, real tokio runtimes, stay outside
  (board 5687).
- **What stays on the real file system**, outside the simulation: FUSE, the VM sockets and a VM
  process's client, procfs, read traces and stats, the image an import reads, a `file://` bucket,
  the tools.
- **The lint** `off-host-fs` (scripts/rust-rules.py) refuses `std::fs`, its traits, and
  `Path::exists`/`canonicalize` in those crates outside `host_fs`, unless the line says why it is
  outside the simulation or its file is.
- **What it found**: the seeds ran several times faster (no real syncs); the ephemeral profile's
  new schedules found TASK-713, TASK-714 and TASK-715 (live migration's lost messages); strict
  POSIX hardening for directories the daemon makes (TASK-708).

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
  statics; its page memory (memfds, userfaultfd) is Linux's. So a daemon core that boots from a
  configuration value, as an async task, comes first.
- Its dependencies are MIT, Apache-2.0, or both (madsim, madsim-macros, madsim-tokio,
  madsim-tonic: Apache-2.0).
- **Decision: go on with madsim.** The madsim build's target is `target/madsim`
  (`CARGO_TARGET_DIR`), as `--cfg madsim` rebuilds everything.

## What the thread inventory found (TASK-685.3, 2026-11-20)

cladefsd starts its threads at 41 sites outside tests. They fall into four groups:

- **12 become tasks**: the control, peers, VM-receive (`server::io`), pager and bucket runtimes;
  the request pool and the flushers; the HTTP server; the sweep and stats loops.
- **5 already run inline** on a serial `Dir` (`Dir::concurrent()`); only `framing::on_threads`
  is not gated yet.
- **4 have step functions**, which the simulation drives itself:
  - the memory governor (it needs a step taking the host's memory);
  - the disk governor (`govern_once`);
  - the page cache's flusher (`flush_pass`);
  - the replicators (`Replicator::idle`/`drive`).
- **16 are outside** the simulation: the VM socket, FUSE, userfaultfd, signals and Fxfs, plus 3
  sites in other binaries. (Fxfs's executor has since moved in, for imports: board 5687.)

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
path becomes async from the pool down to the cell's durability and upload waits. That is a
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
- the cell's open and close handshake with its uploader task (`start.recv()`, and
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
6. The cell's open and close with its uploader: `upload::open_async`, `Uploader::stop`, and
   `OpenCell`'s and `host_cell`'s open, create, take and close in async forms.
7. The control handlers on the async forms: `blocking()` keeps only disk work.
8. VM OPEN, bases, VERSION, DROP_VERSION and FORK as futures.
9. An explicit async close for every cell the daemon has open, and under madsim an assert in
   `Drop` that it was closed. The ordering rules of TASK-54, TASK-79.34 and TASK-129 rest on
   today's close in `Drop`.
10. Replication async: `Members` and `MemberLink` returning futures, members served by tasks,
    `local_cell::sim`'s synchronous `drive()` kept.
11. The dispatch seam under madsim: the pool, the flushers, `block_in_place`, `spawn_blocking`
    and `cells::blocking` as tasks, or inline.
12. The peers server, sweep, drain and control as tasks.

The hardest are 9 (a cell's close can't be awaited in `Drop`, and its last reference goes
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
5. **Disk through `host_fs`** everywhere cladefsd writes (its cell directories, replica directories, the part
   cache, its state files): see "The disk as built" below.
6. **The in-process bucket** with faults, and the control API on TCP under madsim.
7. **The simulated guest and the checker**, one profile (attrition alone), seed replay checked.
8. **Fault profiles**, then a rule search per rule.
9. **CI**: a few hundred seeds per profile on each push, a soak at night, failing seeds kept as
   regression seeds, as `local_cell::sim`'s are.

## On macOS, and its known failures in CI (board 2912, 2026-11-21)

- **The daemon builds under madsim on macOS**: its dependencies are Linux's and, under madsim,
  every OS's (`cfg(any(target_os = "linux", madsim))`), its tests/madsim.rs no longer Linux's
  alone. What stays Linux's is outside the simulation: a VM process's client (memfds,
  userfaultfd) and its half of the wire, the FUSE mount (`mount::start` refuses under madsim),
  pidfds (`VmProcess`, uninhabited under madsim) and a transfer area's seals (refused off Linux).
  A shared memory object macOS unlinked as it made it can't be reopened read-only: the default
  arena's is handed on as it is, to simulated guests that map nothing. The macOS CI job runs the
  madsim part, 20 seeds; seeds differ between the OSes (seed 71 fails one way on Linux, seed 6
  another on macOS).
- **Known failures**: scripts/madsim-seeds.py runs every seed, then fails the part on any failure
  but those crates/cladefs/tests/madsim-known lists, each an open bug's, by its test and the start
  of its panic's message, said as known with its board task. Another test's failure, another
  message, a hang (nextest's profile madsim ends a test at five minutes) or a crash fails it. A
  line goes with its bug's fix.

## Fault profiles and rule searches as built (board 2830, 2026-11-21)

`crates/cladefs/tests/madsim.rs`'s attrition is one workload (a writer guest's rounds, each on the
next of three hosts, takes at later placements, kills, zombie writers, the bucket failing and out
of reach) over a `Profile` of fault families, each a test of its own, and all of them drawn at
random per seed (`Profile::swarm`, FoundationDB's `addFailureInjection`):

- **Network**: a host cut off both ways (`clog_node`), one way (`clog_node_in`/`_out`), or the
  links between nodes cut one after another and mended in another order (FoundationDB's
  swizzle); packets lost and delayed throughout, which TCP's streams ride out stalling.
- **Replication**: a replicated cell's takes, a member killed with the writer's host and down
  still as the next host takes the cell, forced (doc-21).
- **Operations**: versions made and each forked at the end; writers freeing blocks told uploaded,
  each freed reading no more; bases made, a version of each forked on another host, the base
  deleted; the collector passing meanwhile and twice at the end, its grace gone between, from a
  node of its own under the hosts' KMS; the hosts told other peers (SetPeers); a host drained, as
  SIGTERM has it, before its kill.
- **Pressure**: a host's memory and disk limited a while (SetMemoryLimit, ShrinkMemory,
  SetDiskLimit), writes refused past them. The host's disk governor stays out of reach while a
  simulated disk is a fixed terabyte (board 5899).
- **Disks**: calls failing (board 5654), as before. BUGGIFY stays off in the full stack: turned on,
  its sites switched by the madsim seed, it lost an ephemeral cell's Local commits (board 6034),
  which it waits for.

The checker: every block told uploaded and not freed reads as written on the last host, none freed
does, each version's fork holds what was told uploaded before it, and each base's fork reads the
block it inherited.

**Rule searches** (`needs!`): a rule of `bucket_cell::Rules` off, seeds of its profile until one
loses what a guest was told, each seed a process of its own (as scripts/test.sh runs them: a
simulation's node-local values stay in its process, board 2890). Each rule was tried over 2,000
seeds of the profile that reaches it. Five find theirs: `head_swap` (attrition),
`collector_epochs` and `candidates_first` (frees and the collector), `references` and
`references_relisted` (bases). `slots_at_birth` found its on Linux (seed 92 of the bases profile)
but in none of 2,000 seeds on macOS, whose schedules differ; a search one platform misses would
fail `scripts/test.sh` there, so it has none. The rest found none: `local_cell::sim`'s searches find each, their
steps aimed at the protocol's races over 20,000 seeds, where a full-stack seed reaches them
seldom; aiming the full stack's faults at those races is adoption 5 below. `local_cell::Rules`
(the open cell's concurrency) found none either: its framings', flushes' and frees' rules over
2,000 seeds of their profiles, and its writes' rules guard a writer racing its own commits, which one simulated guest, its requests one at a time, never is; Shuttle's
searches find them (`crates/local_cell/tests/shuttle`).

**What it found**: cladefsd's drain counters were process statics, so each simulated host waited
for every host's connections and calls, and simulations sharing a process woke each other's tasks
(board 5806); with BUGGIFY on, an ephemeral cell's Local commits lost (board 6034); and, on main as
it was, two live-migration endings of the ephemeral disk-faults profile (board 6032).

## Cells across buckets as built (board 14521, 2026-11-27)

doc-23's cells across buckets ran in no simulated daemon: the worlds above share one bucket, given
to each daemon unrouted, and `local_cell::sim`'s `mirrors` profile routes through its own
`SimBucket::at`. `crates/cladefs/tests/madsim/regions.rs` runs them on the daemon's own path.

- **The clouds are the simulation's, and nothing above them** (`bucket_cell::clouds`,
  `cfg(madsim)`): a simulation says which store a `memory:///NAME` URL names, and every opening of
  that URL in it reaches that store. A daemon names its home bucket by its URL
  (`CLADEFSD_BUCKET`), opens it as production does (`host_cell::bucket_on`: `Registered`,
  `RoutedBucket`), and finds the other cloud regions' buckets in the registry of buckets, which
  the case's operator wrote with `identity::format`, `add` and `spread_to` before the hosts
  booted. No daemon is handed a bucket, a resolver or a KMS.
- **A world**: three cloud regions, two hosts each; each region's bucket under its own
  `faults::Seeded`, journalled under the faults (every write and deletion that landed, by key);
  each region its own `local://` KMS, recorded in its registry entry, which another region's host
  reaches as far as it trusts it (`CLADEFSD_KMS_LOCAL`, `CLADEFSD_KMS_HOSTS`); each region its
  collector, sweeping as `cladefs-gc` does (`Sweep::run_limited` with the bucket's own). A node's
  whole environment is its own (`node_local::env::vars_os`), so that each host's KMS and trust are
  its own.
- **Faults aimed at the protocol's state** (adoption 5): a round stops the first forking host to
  come to a step of its fork or of its mirror, by the step's failpoint (`version::recorded` to
  `version::made`, `mirror::pinned` to `mirror::referred`): the failpoint's callback pauses the
  host's node where it stands, and the case then kills it, or resumes it once its pin stood stale.
  A mirror stalled, its cell and children are deleted under it the likelier, so that its copy is
  collected before it goes on. Besides: both hosts of a region forking a version at once, a third
  fork later, a version dropped or its cell deleted as its mirror goes, a second version sharing
  the first's objects, a region's whole bucket out of reach.
- **The checker**: every child reads what it inherits and its own told uploaded; every version
  named still, a child of it living in another region, ends marked there, its children referring
  there and no more in its cell's bucket; `bucket_cell::testing`'s `uncollected` and
  `uncollected_copies`, the other simulator's end checks, over the buckets as the clouds have them
  (no copy short under a marker, no marker without its hold, no pin left); its cell's bucket out
  of reach, each child of a marked version taken by a host that never had it, reading blocks no
  host read before, and committing; every cell deleted and a week gone, no copy, hold, pin or
  marker left, nor anything else no cell needs. And by the journals, read together in the order
  things landed, at every moment and not at a check's alone: a marker lands marked with its hold
  standing, its checkpoint copied and no object of its copy deleted since its run claimed it;
  and no object of a copy is deleted while a marker of its cell stands marked.
- **A reach check** (`mirrors_across_regions_needs_every_outcome_reached`, run once with the
  rules' searches): each outcome the case checks (a read before its version's marker, one from
  the child's own bucket with the cell's out of reach, runs raced, a host crashed at a
  step, one stalled past its lease, a slot taken over, a mirror given up, a copy collected, two
  versions sharing objects marked, the teardown) is reached by a seed of its set, or it fails.
- **Peers** (`peers_serve_parts_within_their_cloud_region_alone`): every host told every host;
  each region's hosts' parts given are those its own hosts served, by each daemon's own metrics
  (its served counts were process statics, every simulated host's together).
- **KMS trust** (`recorded_kms_urls_reach_what_the_host_trusts_alone`): a recorded `local://` URL
  forks on the host that allows it (`CLADEFSD_KMS_LOCAL`) and not on the one that does not, each
  host's own environment's say. What a recorded URL may reach besides (`CLADEFSD_KMS_HOSTS`, the
  GCP projects, the AWS accounts) no simulated daemon tells from its absence: the simulation has
  no KMS's server, and a KMS at a host not allowed and one allowed and not there both refuse.

**What it found**: a conditional-write probe stopped short left its object at `probe/<nonce>` for
good, nothing reclaiming it, which `cladefs-gc` now deletes after a sweep (board 14544); one failed read of a host's
registry of buckets answered a fork of another bucket's version `EACCES`, a KMS's refusal, where
it is the bucket out of reach (board 14562); a ForkCell whose host dies after its child's record
leaves a fork being made for as long as its version is named, the VM manager's retry forking
another cell (board 14559, open); and `scripts/rust-rules.py` failing on main (board 14534).

**Mutants** (2026-11-27): each rule of `specs/buckets/mutations` put back as a bug by hand, 40
seeds each. The case catches `mirrorHolds`, `markerAfterCopy`, `markerGoesFirst` (every seed),
`mirrorListsAfterMark` (11), `forksReferFirst` (7), `mirrorResumesMarked` (6), `releasesOwn` (3),
`forksRecheck` (3), `marksOwn` (1) and `forksLeaveA` (1); and, of what the spec does not model,
reads sent to the cell's bucket for a marked version (every seed), peers serving across regions,
every recorded KMS trusted, and probes' objects unreaped. It catches none of `mirrorChecks`,
`mirrorHoldsUnmarked`, `mirrorRereadsBeforeRetiring`, `mirrorPins`, `copyRelists`,
`copiesKeptWhileAlive`, `copyChecksDead` and `forksRereadBeforeLeaving`: each needs a pass of the
copy's bucket's collector stopped between its read of the cell's head, its listing and its
deletions while a fork or a mirror goes, and the case stops hosts at their steps, not collectors
(board 14576). `local_cell::sim`'s aimed steps and TLC find those.

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

**What FoundationDB's history says found its bugs (2026-11-20).** Read from its 31,966 commits
since 2017-05, as 12,001 PR-sized units. 2,560 of them are bug fixes, and 1,377 of those PRs'
descriptions were read from GitHub. Only about 13% of the fixes say how their bug was found, so
every share below is a floor.

- **Simulation runs found the most.** 161 fixes were checked by hand as found by a simulation
  seed; between 160 and 550 by estimate. 10% of the fixes in patch releases cite one.
- **Sanitizers on simulation seeds came second**: valgrind, ASAN, UBSAN, TSAN and LSAN, with 139
  fixes, at least 49 of them on simulation seeds. Their ensembles run the same seeds as the
  simulation's.
- **Everything else found little**: static analysis about 12, the binding tester about 9,
  production about 6 (a floor, as Apple's and Snowflake's internal trackers aren't read), unit tests
  about 4, review 2 or 3, fuzzers none.

Within the simulation:

- **How the 161 failures showed:**
  - crashes and memory errors, about 55;
  - asserts, about 49;
  - hangs and timeouts, about 30 (19%);
  - a workload's own invariant check, about 26;
  - only an error event, about 17;
  - a determinism mismatch, about 8.
- **Faults named:**
  - restart and upgrade tests, 11;
  - kills, 8;
  - clogging and partitions, 7 to 10;
  - a buggified knob, 5 or 6;
  - Rollback, 4;
  - disk-fault injection, none.
- **BUGGIFY**: runs with it on found about 1.9 times as many bugs as runs with it off (a small
  sample: 0.9 to 3.9 times).
- **Probe-coverage reports**: no fix cites one. Probes appear in fixes, to show the situation fixed
  is reached.
- **Scale**: fixes cite ensembles of 100,000 to 500,000 runs, and rates like 4 in 177,766.

**So the adoptions, ranked again by evidence:**

1. A mix of faults per seed (done: `swarm`).
2. A heal phase with a bound in time, as hangs are a fifth of the finds. The invariant checks need
   tests of their own: one broken check hid many failures (FoundationDB PR #9635).
3. Every error not injected fails a seed.
4. `buggify!` (done).
5. Faults aimed at the protocol's state.
6. Knobs.
7. Timers jittered and I/O short: the bugs the simulation missed were of unusual timing (#4900,
   #2481).
8. A determinism digest.
9. `probe!`.
10. Reseeding from a step.

**What doc-24 lacked, all with evidence:**

- **Sanitizers on the simulation's seeds**: Miri, or ASAN, over `local_cell::sim` and the full
  stack, for the unsafe code and the atomics.
- **Workloads checked against a model of their own**: our checker against the world's model is one.
  Each feature wants its own.
- **Restarts from the disk alone**: write, stop everything, start again from the disks, check.
- **Ensemble scale**: about 10^5 seeds, as fresh ones each run, in a nightly soak, a rare bug's
  rate measured over as many.

**What doesn't transfer:**

- Upgrade tests across binaries: no existing buckets.
- Stackful coroutines for blocking code (`CoroThreadPool`): this is the alternative to the async
  rewrite, worth keeping in mind only for code the rewrite can't reach.
- Real threads marked `noUnseed`: madsim panics on threads.
- Joshua-scale thresholds.
- Knobs randomized in the production binary: compiled out instead.
- Whether `MemDir::crash_corrupting` covers what `AsyncFileNonDurable` models (writes reordered
  unless they overlap, sectors torn): answered below, and closed by board 5748.

## A second pass over FoundationDB (2026-11-21)

Read at origin/main c06497f39c, 104 commits past the first pass, beyond the simulation: the storage
engine and file layers, object storage and backup, recovery and fencing, overload control, and
testing and observability. Each claim filed was checked in our code first.

**The open question above, answered.** `MemDir::crash_corrupting`, and `host_fs::sim`'s crash,
which copies it, model less than `AsyncFileNonDurable`:
- they tear a chunk only at its midpoint, its left half written; never the right half, a random
  offset, or valid bytes beside garbage;
- they chunk at 512 B from a write's offset, not at the file's sector boundaries;
- a directory made is durable at once, so a `sync_dir` missing is never caught;
- they inject no read errors, stalls, or bit flips in what was synced (`AsyncFileChaos`,
  `SimpleFile`'s `io_error` and `io_timeout`).
Board 5748.

**What both crash models do since board 5748** (`host_fs::crash`, shared by `local_cell::MemDisk`
and `host_fs::sim`): a write lands in 512-byte sectors at the file's offsets; a crash keeps each
sector or not, and, corrupting, also garbage or torn at a random offset, its left or right part
written and the rest as it was or garbage, garbage only in the bytes the write changes (powersafe
overwrite, decision-3). Writes since a sync land or not each on its own, so they reorder, as
`AsyncFileNonDurable`'s do. A directory made is durable at the next sync of anything on its
disk, which commits a journaling file system's journal as ext4 and XFS do, and lost with all
under it at a crash before; `AsyncFileNonDurable`'s stricter rule, durable only once its parent is
synced, cost a fork 2.5 times its time in the forks-failing test for no file system cladefs runs
on, so the models take the journal's. A cell's directories of each kind are made before its own
in them, as a real file system needs (`local_cell::layout::make_dir`). A simulated disk fails reads of a file's data with `EIO` at the odds the disk-fault profiles set
(`host_fs::faults::Odds::reads`), which found an open cell dropped unclosed as its attached bases
failed to read, and a journal note removed as torn when only unread. Not modelled, and why: a
stall, as a simulated disk's calls are synchronous, and madsim's time advances only at an await;
bit flips in synced data, which a cell with one copy can only refuse, never repair, so they need a
profile that expects that refusal (board 6550).

**For the simulation** (posted on board 2830): `buggify!` has four sites in the whole tree, so
every overload bound runs at its production size in the simulation, where FoundationDB shrinks
them together (`smallStorageTarget`, `smallTlogTarget`); a simulated host's disk is a fixed 1 TiB,
so disk pressure never happens; seeds don't fail on errors not injected, and no full-stack
profile checks it settles clean after its heal (adoptions 3 and 6 above).

**Bugs found, filed:**
- live migration: a target crash between its rename and `accepted/` (5728); a stale hand-over
  note deleting a later store at its path (5730); handlers racing, migration IDs unordered
  (5732); abandoned migrations never reaped while a target runs (5734). Each is a handoff with
  more than one durable step and no single commit point, or a step a late request can redo: the
  class of board 2934. FoundationDB rolls a half-done handoff forward on reading it, and orders
  every supersedable request by generation.
- the bucket: auth and permission errors retried forever (5736); a lost create answered by
  downloading the whole object (5738); faults turning paginated listing off (5740).
- the store: raw and kept segments read with no integrity check, an ephemeral cell's for its
  whole life (5742); recovery trusting files it never synced (5744), which `DiskQueue::recover`
  syncs first; a stale tail block after a failed write (5746).

**Worth doing, filed:** one host status with a health tier, active probes and latency past 1 s
(5750); a bucket audit, scrub and layout-independent digest (5752); pacing writes before ENOSPC,
fair shares across tenants, an age bound on upload lag (5754); replicated takes not stalling on
dead members, nor a fenced writer lingering (5756, to verify); one retry loop with jitter and
Retry-After, PUT checksums, every wire attempt counted (5758).

**What doesn't transfer:** coordinators and their generation register (a cell's head is its
authority), chains of old log generations (the bucket holds all but the bounded tail), Redwood's
B-tree and remapping (extents are written once), a cluster-wide Ratekeeper with clients that
throttle themselves (guests are untrusted; pacing is the daemon's), killing the process at its
memory limit (it would take every VM on the host), format migrations. FoundationDB has no TLA+
specs; ours are worth keeping, their atomic steps split where the code's are not (board 5732).

## Compared with FoundationDB's again, as built (board 2844, 2026-11-21)

The full stack runs its fault profiles (board 2830) and both simulations their branch coverage
(board 2842). Of the ten adoptions above, as ranked first:

1. **`buggify!`, two levels**: made (`bucket_cell::buggify`, ef663c5ee), on in local_cell::sim's
   runs (four seeds in five), but at four sites in the whole tree, not the thresholds, retries and
   governors planned (reply 5760 on board 2830). Off in the full stack: turned on, its first 200
   seeds lost an ephemeral cell's Local commits (board 6034), the first finding of its own.
2. **A mixed profile per seed**: made, local_cell::sim's `swarm` (f3ce86ee1) and the full stack's
   `Profile::swarm` (board 2830). Found in the full stack: harness gaps alone so far (a partition's
   links left cut at the heal), as the named profiles found first what the families reach.
3. **Knobs**: not made. The protocol's bounds run at production sizes in both simulations (lag
   1 GiB, checkpoints, page cache, in-flight bytes), so their branches stay unreached (the
   coverage's "bounds no seed gets near"). Next after board 6034, with BUGGIFY.
4. **`probe!`**: not made; llvm-cov's branch coverage of the simulations stands in for it
   (scripts/sim-coverage.sh, nightly). What it found: the two simulations reach different halves of
   the protocol (local_cell::sim never runs the uploader's task; the full stack never writes
   anything but whole blocks), each gap now a task (boards 6037, 6039, 6041). A probe names a
   situation, which a branch doesn't: still worth making for the races the searches miss.
5. **Faults aimed at the protocol's state**: barely made (a forced take with a member held down).
   Its absence is why 26 of bucket_cell's 31 rules have no full-stack search: over 2,000 seeds
   each, the profiles' blind faults hit 25 of their races never, and `slots_at_birth`'s on Linux
   alone, where local_cell::sim's aimed steps hit each.
6. **A heal phase**: made in part. Every profile heals (the bucket, the disks and the network
   whole, limits lifted, the collector's last passes past their grace), then checks what guests
   were told; but with no bound in time to reach Uploaded and replicas converged, nor a leak check
   (reply 5760 on board 2830).
7. **Every error not injected fails a seed**: not made.
8. **A determinism digest**: not made as a digest; MADSIM_TEST_CHECK_DETERMINISM run by hand over
   1,200 seeds found what reached the simulation from outside (a real temporary directory's name,
   board 5471; two simulations' threads deadlocking on node_local's locks, board 5473).
9. **Timers jittered, I/O short**: the network's latency and loss made (board 2830); timers and
   short reads and writes not.
10. **Reseeding from a step**: not made.

**What the full-stack simulation found** (TASK-685 to now): live migration's lost messages,
markers and notes (TASK-713 to 715, boards 2934, 5721, 5890's commit told durable lost, 6032), a
CreateCell whose last sync failed (5668), a bucket probe taking a bucket out of reach (TASK-699),
the daemon's process statics shared by simulated hosts (5806, and physmem's memory, TASK-685.14),
BUGGIFY's lost Local commits (6034), and leaks of simulations into each other (5473, 2890). Most
by the ephemeral profiles' crashes and the disk faults; none yet by a rule search alone.

**What FoundationDB does still that we don't**, by its history's evidence: faults aimed at the
protocol's state and knobs shrunk under BUGGIFY (the two that would reach the 26 rules and the
bounds); every unexpected error failing a seed, and a bounded heal with a leak check (a fifth of
its finds were hangs); sanitizers over the simulation's seeds (Miri or ASAN over local_cell::sim);
restarts from the disks alone; a model of its own per workload; ensembles of 10^5 fresh seeds a
night, where our soak runs 2,000 a profile; and code probes, situations rather than branches.

## Costs and risks

- A second build of the workspace (`--cfg madsim`), in CI and on ubu1.
- madsim is maintained by one company; its tokio shim is at 0.2.30 (2024-10) against our pinned
  tokio 1.53.1. The spike checks the shim builds against it; a fork, if needed, is Apache-2.0.
- Every new thread, static or `std::fs` call in cladefsd must go through the seams, or the
  simulation build fails (threads panic, statics are shared): a lint (TASK-444.6's script) keeps
  them out.
- The simulation does not cover the kernel paths (DAX, FUSE, userfaultfd), which keep their own
  tools.
