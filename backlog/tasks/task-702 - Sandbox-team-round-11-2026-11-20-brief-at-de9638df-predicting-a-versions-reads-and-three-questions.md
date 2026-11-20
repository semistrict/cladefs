---
id: TASK-702
title: >-
  Sandbox team, round 11 (2026-11-20 brief at de9638df): predicting a version's
  reads, and three questions
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 19:45'
updated_date: '2026-11-20 19:47'
labels:
  - sandbox
  - performance
  - cache
dependencies: []
priority: high
ordinal: 1102000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The sandbox team is moving sandboxes from JuiceFS block rootfs to cladefs. Measured at de9638df (GCE n2-standard-16, RustFS with 25 ms added delay, 2 GiB DAX window, part cache on, `codex --version` in a fresh VM of a Node + Codex image, medians of 3 fresh bases): cold host 3.6 s on cladefs (115 MB in ~300 GETs) against 3.8 s on JuiceFS without a working set; base in memory 0.20 s (JuiceFS 0.58 s); base on local disk only 0.20 s (JuiceFS 1.10 s). On JuiceFS in production, a working set (blocks read in a snapshot's boxes' first ~10 s, loaded as the next box boots) takes a cold host's first command to ~0.5 s. The host cannot build one outside the daemon: it cannot see which parts page-ins and DAX faults fetch.

Their ask was a working set per version: record a fork's reads for N s, publish the set beside the version, grow it (merge if it adds 1 MiB, once per host per version per 30 min, cap 512 MiB), prefetch it at open (peers first, demand ahead), and for shared versions keep the first set or grow it from the owner's forks only. Or two calls, RecordReads(store, duration) and Prefetch(cell, version, parts).

Decided with the user (2026-11-20): this is a model predicting a cell's reads after it opens, which the daemon keeps and serves as a feature, not a host's or a VM's concern and not exposed as extent IDs to integrators. A version's data never changes, but the reads it receives do (workloads drift; one version serves different commands): the model is built from recent evidence, decayed, never assumed fixed; a set fixed forever is one candidate the data may still choose. The cold cost is a chain of ~25 ms round trips (TASK-670 saw ~1.6 GETs in flight), not bytes: recall matters more than precision, order matters little once the predicted parts are fetched at once. TASK-38's boot profiles (crates/cladefs/src/profile.rs) record and replay per host and per daemon pid, and are what this replaces (TASK-670, superseded).

Questions, answered from the code at a771c3032:
1. DAX faults: a page-in reads the whole 1 MiB part around the faulted page, and 2 parts more only while the extent is read in order (`PARTS_AHEAD`, crates/cladefs/src/cache.rs, `parts_to_read`); never the whole extent. The fetcher reads ahead up to 16 parts along in-order reads of an object (`AHEAD`, crates/bucket_cell/src/fetch.rs).
2. Read-ahead only with a part cache: in the code, a fetcher with no cache keeps nothing, not even the 64 MiB of recent parts (`read_ahead`, `recent`), so a part read ahead has nowhere to land: a subtask.
3. Peers slowing a cold cluster's first boot: read-ahead does cover parts a peer owns (`read_ahead` -> `part` -> the owner); while waiting on the owner the asker holds a slot of its own budget, and the owner fetches as a new demand read. Not diagnosed yet: a subtask. Their proposal (fetch from the bucket when the owner lacks the part, and tell it) gives up the deduplication peers exist for; measure first.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
