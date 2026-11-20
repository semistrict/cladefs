---
id: doc-24
title: 'Full-stack deterministic simulation: N daemons, one seed'
type: specification
created_date: '2026-11-20 03:31'
updated_date: '2026-11-20 04:01'
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
  that finds its violation, as today's simulation has (`test_needs!`): our BUGGIFY.

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

## Steps

1. **Spike** (a day): cladefs builds under `RUSTFLAGS="--cfg madsim"` with the renamed packages,
   two simulated daemons boot, one makes a cell and the other takes it over the in-process bucket.
   It lists what breaks; the steps after are sized by it. If madsim can't host cladefsd, this doc
   is rewritten for the in-house fallback.
2. **A daemon core that boots from a configuration value**, as an async task: cladefsd's `main`
   reads the environment and calls it; the simulation calls it once per node. Its sockets
   (control, peers, metrics) given to it, TCP under madsim. (Added by the spike.)
3. **Per-node statics** (`node_local!`), crate by crate.
4. **Threads and runtimes** through one seam (tasks under madsim), `blocking()` inline.
5. **Disk through `Dir`** everywhere cladefsd writes (its stores, replica directories, the part
   cache, z-pages' files, its state files).
6. **The in-process bucket** with faults, and the control API on TCP under madsim.
7. **The simulated guest and the checker**, one profile (attrition alone), seed replay checked.
8. **Fault profiles**, then a rule search per rule.
9. **CI**: a few hundred seeds per profile on each push, a soak at night, failing seeds kept as
   regression seeds, as `local_cell::sim`'s are.

## Costs and risks

- A second build of the workspace (`--cfg madsim`), in CI and on ubu1.
- madsim is maintained by one company; its tokio shim is at 0.2.30 (2024-10) against our pinned
  tokio 1.53.1. The spike checks the shim builds against it; a fork, if needed, is Apache-2.0.
- Every new thread, static or `std::fs` call in cladefsd must go through the seams, or the
  simulation build fails (threads panic, statics are shared): a lint (TASK-444.6's script) keeps
  them out.
- The simulation does not cover the kernel paths (DAX, FUSE, userfaultfd), which keep their own
  tools.
