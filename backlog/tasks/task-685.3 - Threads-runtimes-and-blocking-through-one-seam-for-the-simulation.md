---
id: TASK-685.3
title: 'Threads, runtimes and blocking through one seam for the simulation'
status: In Progress
assignee: []
created_date: '2026-11-20 03:32'
updated_date: '2026-11-20 17:12'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1069000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 3: about 100 thread spawn sites, the runtimes cladefsd builds (server, control, peers, bucket, pager), block_in_place: one seam that is a thread or runtime built normally and a madsim task under cfg(madsim); blocking() inline under madsim.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No std::thread::spawn, runtime build or block_in_place outside the seam in cladefsd's code; a lint refuses new ones
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20, after the user's decision (the daemon async down to its waits): the plan is doc-24's 'The async rewrite's plan', 12 steps from the inventory of cross-task waits; each step leaves the normal build as it is. Step 1 (fail fast under madsim) and step 2 (async uploader waits) first. Found meanwhile: TASK-688 (a store taken on the control runtime's worker).

2026-11-20: steps 1-4, 6, 8 landed on main by PR #2 (5f65a04ee). The rest goes to main directly (the user: no more PRs). Next batch: step 9 (explicit async close of daemon-owned stores) and step 5 (read path), with TASK-685.12 (madsim build in test.sh/CI).

2026-11-20 (4089f5a78): steps 9 and 5 landed on main. Step 9: open_cell::release closes a store when its last holder lets go (connections' teardown task, HeldCell/Here/kept/forks/Live); OpenCell::close releases a follower's writer and attached bases; a store dropped open panics under madsim. Step 5: READ/MAP, cache page-ins, fetch, base opens awaited; READ takes a connection page-in turn (PAGE_INS) as faults do; boot profile replay/writer are tasks. Only blocking seam left: image import (cells::outside_the_simulation). Remaining: step 10 (replication), 11 (dispatch seam: pool, flushers, spawn_blocking, cells::blocking), 12 (peers, sweep, drain, control as tasks; TASK-685.13 gRPC on madsim tonic). Also found: local_cell::dir::closed_in waits on a std Condvar (UNCLAIMED) up to 60 s for a store of the same directory closing in the process, on every open: a blocking wait the simulation would hit.

Next batch plan (2026-11-20): steps 11, 12 and TASK-685.13 go together, as none is testable until cladefsd builds under madsim. (1) One seam for where a daemon service runs: production, a thread of its own driving its own runtime (control: THREADS/BLOCKING_THREADS; peers; sweep; http; governors); madsim, a task of the node's. serve_control/peers::serve become async fns the seam runs (no runtime.block_on inside them). (2) Pool::run and the flushers' pools: tasks under madsim. (3) gRPC on madsim's tonic: Drained over its Service<(PathAndQuery, Request<BoxMessageStream>)> shape, MountServer<T, F> generics, tonic_health (real tonic) reported only where it builds; control over TCP under madsim (doc-24 boundary). (4) boot under madsim: no VM socket, FUSE or metrics; control on a TCP address. Then TASK-685.9: two daemons as madsim nodes.

2026-11-20 (3beb3af0c): steps 10-12 landed. Step 10: MemberLink/Members futures; members served by tasks on the bucket runtime (event, not condvar); sim drive() polls once. Steps 11/12: cladefsd::run seam (service: own thread+runtime / madsim task; every: periodic; off_the_runtime: spawn_blocking / inline), Pool::run a task under madsim; governors, peers, sweep through it; boot async. Remaining blocking under madsim found at runtime by TASK-685.9's boot test (in progress): threads the store and physmem spawn, drain's IO.block_on, stop_serving's block_on, dir::closed_in's condvar.
<!-- SECTION:NOTES:END -->
