---
id: TASK-608
title: The device turns each guest MAP_MANY into one daemon MAP per piece
status: To Do
assignee: []
created_date: '2026-11-18 15:32'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 999000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), virtio-host review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/virtio_extent/src/requests.rs`: `rg -n -F 'store.map(args.id.into(), offset, len, window_offset).await?;' crates/virtio_extent/src/requests.rs`

The guest batches up to 86 maps into one MAP_MANY (CLADEFS_DEVICE_MAP_MANY in device.c), for example a chunk's pieces or the runs a rebind maps back (TASK-91.4, TASK-164.10). The device undoes that batching. Each piece becomes its own daemon request: its own message and reply, its own pass through the connection's 8 WORKERS, its own dax lock and second-stage change, and its own stage operations for the VM process's single receive thread to apply. The daemon has had Request::MapMany since TASK-92.14. Through OpenCell::map_many and Window::map_many it maps all pieces under one lock in one change, merges adjacent runs, and handles zeros and private pages (Private::Free) exactly as a guest MAP does. Only the blocking restore path uses it. TASK-92.14 measured 1,280 mappings: 60-68 ms as separate MAPs against 14-15 ms as one MAP_MANY, and 2,560 against 2,048 stage operations. The fan-out also blocks other work: a MAP_MANY of 86 pieces fills the connection's 8 workers and queues 78 more MAPs ahead of that VM's other requests (window faults from other vCPUs, READs, COMMITs). FREE_MANY already goes to the daemon as one request (TASK-92.13); MAP_MANY never got the same.

**Trigger.** A guest DAX workload maps a chunk's pieces or rebinds scattered private pages, such as the forked pgbench in TASK-164.10 (about 30k MAPs a minute). Each guest MAP_MANY of k maps arrives at handle(), which sends k daemon MAP requests (plus k prefaults). The daemon admits 8 at a time per connection, queues the rest FIFO, and does k separate lock-and-change rounds, each with its own reply and its own stage operations on the VM process's receive thread. One daemon MAP_MANY would be one request and one change.

**Why the tests stay green.** No test counts daemon requests per guest MAP_MANY. FakeBackend records maps, not requests. tests/daemon.rs has frees_many_in_one_request, which asserts one daemon request for FREE_MANY, but nothing like it for maps. Guest cases measure total time, not requests per MAP.

**Backlog.** No related task covers it: TASK-92.14 (Done) added the daemon's MapMany for restore only; TASK-91.4 and TASK-164.10 (Done) batch maps in the guest; TASK-92.13 (Done) did the same for FREE_MANY.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 One guest MAP_MANY of N maps, zeros and private pages included, reaches the daemon as exactly one request: served("map") moves by 0, served("map_many") by 1, and every page reads its extent's bytes or zeros through the window.
- [ ] #2 A failing piece fails the guest request as doc-4 says, the first that fails failing it whichever others mapped.
- [ ] #3 A regression test maps_many_in_one_request in virtio_extent's tests/daemon.rs, beside frees_many_in_one_request, asserts those counts and contents and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
