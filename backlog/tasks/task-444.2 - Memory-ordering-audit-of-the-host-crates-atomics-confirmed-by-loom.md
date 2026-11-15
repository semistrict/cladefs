---
id: TASK-444.2
title: 'Memory-ordering audit of the host crates atomics, confirmed by loom'
status: To Do
assignee: []
created_date: '2026-11-15 12:33'
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
Shuttle models every atomic as SeqCst (its docs; it warns at runtime), so no fxdax test can see an Acquire/Release/Relaxed bug. The host crates (fxdax, physmem, extent_store, extent_remote, follower, virtio_extent) hold ~450 non-SeqCst orderings (2026-11-15). LLMs reason poorly about weak memory (arXiv 2501.14326), so an audit alone is no evidence: loom (MIT) is the disposer. AtomVChecker classes: ordering too weak to publish related state; AtomicPtr data unsynchronised; stronger than needed.
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
