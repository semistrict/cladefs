---
id: TASK-444.2
title: 'Memory-ordering audit of the host crates atomics, confirmed by loom'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:33'
updated_date: '2026-11-15 14:15'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
priority: high
ordinal: 851000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Shuttle models every atomic as SeqCst (its docs; it warns at runtime), so no cladefs test can see an Acquire/Release/Relaxed bug. The host crates (cladefs, physmem, extent_store, extent_remote, follower, virtio_extent) hold ~450 non-SeqCst orderings (2026-11-15). LLMs reason poorly about weak memory (arXiv 2501.14326), so an audit alone is no evidence: loom (MIT) is the disposer. AtomVChecker classes: ordering too weak to publish related state; AtomicPtr data unsynchronised; stronger than needed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each non-SeqCst atomic in the host crates has a recorded pairing and happens-before argument
- [ ] #2 A finding counts only when a loom model fails; each is fixed or filed
- [ ] #3 Ordering mutants (Release or Acquire to Relaxed, a fence deleted) on the audited sites each turn a loom model red, or a task is filed for the gap
- [ ] #4 loom models run in CI
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Inventory every non-SeqCst atomic in fxdax, physmem, extent_store, extent_remote, follower, virtio_extent and vendor/firecracker's extent device (no std::sync::atomic::fence anywhere: every 'fence(' there is the store's fencing): per atomic, what it publishes, pairing, happens-before argument, AtomVChecker class. Recorded in docs/memory-ordering.md; counters a line each.
2. loom (MIT, 0.7.3) behind a 'loom' feature per crate (not cfg(loom): tokio's own cfg(loom) breaks the build), through each crate's sync module, as the shuttle feature is. Protocols whose orderings carry an argument become small types the production code uses, so the models run the code itself.
3. loom models: fxdax client's daemon-lost handoff (lost flag vs on_lost OnceLock: store buffering, suspected double or no call) and committed handoff (fetch_max vs on_committed: suspected missed commit); virtio_extent Resizing's capacity/asked pairing; uffd Lift's ask/serve; physmem frames' staged mark; physmap chunk publication; others where the inventory finds an argument.
4. Ordering mutants as patches (crates/*/loom-mutants/*.patch, Killer: header) with scripts/loom-mutants.sh (runs each, --check in the scripts part). Survivors: a model added, or 'stronger than needed' recorded with the model that shows it.
5. Each loom failure on unmodified code: a task at once, fixed per AGENTS.md (blind spot, class test, structure).
6. loom models in scripts/test.sh's shuttle part (CI's shuttle jobs, Linux and macOS); clippy for the loom builds.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15, batch 1 (c3e355966, local): loom 0.7.3 (MIT; its new transitive crates MIT or MIT/Apache-2.0) behind 'loom' features of fxdax, physmem, virtio_extent. Inventory in docs/memory-ordering.md: 326 non-SeqCst accesses in production code (+~90 in tests); no std fence anywhere; no AtomicPtr. Modelled: hearing::Occurrence and hearing::Latest (fxdax, new owners of extentd client's lost and committed handoffs), virtio_extent Resizing's capacity/ask pairing (Resizing::known), physmem frames' staged mark. Findings, each a loom failure on unmodified logic: TASK-444.2.1 (daemon-lost listener told 0 times; or twice under SC), TASK-444.2.2 (a commit unheard). Both fixed. Mutants: 10 patches, scripts/loom-mutants.sh: 8 killed, 2 equivalent survive as marked (virtio_extent answers-clear-asks-relaxed, asks-read-relaxed). Class-3 sites with no model (ordered by socket/eventfd/Notify/PMM lock): TASK-444.2.3. Suspicion, unreproduced: TASK-444.2.4. Loom models run in scripts/test.sh's shuttle part (CI shuttle jobs, Linux and macOS); their clippy in the clippy part; --check of mutants in the scripts part. Each model runs in under 0.1 s, no preemption bound needed.

2026-11-15: ubu1 run of the workspace's tests (CLADEFS_GUEST_TESTS=0) on 4bd8… branch t3/loom-atomic-audit, queued behind others' jobs: likely over 15 minutes in all.

(The run is of f78d72933's tree, the placeholder above a slip.)
<!-- SECTION:NOTES:END -->
