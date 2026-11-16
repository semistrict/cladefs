---
id: TASK-472
title: >-
  A Location's key is a String of its own in every map entry (~107 B an extent),
  though every extent of a segment shares it: intern it
status: To Do
assignee: []
created_date: '2026-11-16 00:47'
labels:
  - perf
dependencies: []
references:
  - TASK-453
priority: medium
ordinal: 878000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-453. extent_frame::segment::Location { key: String } is cloned into every entry of the maps of extents: the decoded states a base's forks now share (extent_remote::recovery::Decoded, once per base), each store's own layer (extent_store::extents, its acknowledged extents Remote(Location) one String each, into_bucket's key.to_owned()), and the writer's map. bench_fork_extent_memory measured 612 B of heap per block a fork writes, commits and uploads (store's entry and writer's, ~107 B of key in each), and a base's map unshared 229 B an extent of which 107 B is keys. The key names a segment object shared by every extent sealed in it: an Arc<str> interned per object as a checkpoint decodes (extent_remote::checkpoint::located) and as a segment is acknowledged would hold it once. A broad change: Location is built in ~50 places across extent_frame, extent_remote, extent_store and cladefs (codec/wire).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 bench_fork_extent_memory: the bytes a written, uploaded block holds and a base's unshared map's per extent drop by the key's length, extent_store::OWN_ENTRY_BYTES and extent_remote::recovery::ENTRY_BYTES lowered to match
- [ ] #2 Checkpoint and footer formats unchanged on the wire (serde of Arc<str> as str)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
