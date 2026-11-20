---
id: TASK-685.2
title: 'Per-node statics: node_local! for cladefsd''s process state'
status: Done
assignee: []
created_date: '2026-11-20 03:32'
updated_date: '2026-11-20 05:22'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1068000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 2: about 140 statics (cladefs 88, local_cell 35, bucket_cell 15) are shared by madsim's nodes. A node_local! static, plain built normally, one value per madsim node under cfg(madsim), dropped on the node's kill; each static converted, those per process by nature kept.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Every static holding a daemon's state is node_local!; a test under cfg(madsim) shows two nodes' values apart and a killed node's dropped
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed d37de63c5..465e535d2 on main. node_local crate: NodeLocal<T> is a OnceLock normally; under --cfg madsim one value per node, leaked so get() stays &'static T, let go of on the node's kill by node_local::Plugin (madsim reset_node). Converted: bucket_cell 2, local_cell 11, cladefs ~40 daemon statics. Kept process-wide: metrics counters, rules, heap, tracing, FUSE, DAX/uffd, constants, test statics. Left to step 4 (runtimes): server::IO (a plain LazyLock runtime) and cladefs::thread's SPAWNER, which become madsim's runtime / a per-node seam with the threads. Verified: Mac workspace 2813/2813, madsim tests node_local + bucket_cell 2/2 (node values apart, a killed node starts with none; hand mutant without the plugin caught); ubu1 under fxjob clippy, workspace 3124/3125 (the one failure TASK-677's hostile test, passing alone), sim part 47/47; upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
cladefsd's daemon statics are per simulated node under madsim (node_local!), unchanged built normally; verified Mac 2813/2813, ubu1 3124/3125 + known flake passing alone, sim 47/47, madsim tests.
<!-- SECTION:FINAL_SUMMARY:END -->
