---
id: TASK-574
title: 'A mark-only pass marks a dead cell''s head, and the next reports it RESURRECTED'
status: To Do
assignee: []
created_date: '2026-11-18 15:28'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 965000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), collector review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/collector.rs`: `rg -n -F 'doomed.extend(removal.head.iter().chain(&removal.lease).copied());' crates/bucket_cell/src/collector.rs`

Pass::doomed, which mark_held uses for its marks, appends the removal's head and lease whenever the cell is dead and removable (a deleted Root or Clone with nothing else left). So the first mark-only pass over such a cell writes `<C>gc/marks/<e1>` naming `<C>head` and `<C>gc/lease`. This contradicts the doc's rule that the collector's own `<C>gc/` is never a candidate. Nothing is deleted, so the cell stays. On the next mark-only pass the listing includes the marks object, and Pass::removal returns None for any Kind::Marks object (`_ => return None`). doomed now lacks the head, which is listed at the same ETag, so resurrected() logs 'RESURRECTED <C>head ... a bug in the collector, which deleting would have lost it to' and cladefs_gc_sweep_resurrected_objects counts it. In the meantime Watch::reached, which checks only the key and not the version, warns 'a reader reached what the collector's newest mark-only pass found no live cell needs' on each GET of that lease or head. That includes the collector's own passed::found and records() reads, `--usage` and `--cells`. With sandboxes deleted at 10^4 to 10^6 a day, a mark-only watch floods the very warnings operators use to decide whether to trust the collector.

**Trigger.** 1. A sandbox cell of an allocator-given ID is made and deleted (head FINAL, Made::Root or Made::Clone). 2. `cladefs-gc URL --mark-only --every 600`. Sweep 1: mark_held calls pass.doomed(); removal() returns Some (dead, nothing deferred, only head and lease left), so the marks name head and lease. 3. Sweep 2: the lease GET, then the listing (which refreshes the Watch), then records()' head GET, which warns. The listing now holds gc/marks/e1, so removal() returns None, doomed lacks the head, and resurrected() finds the head at the same version: WARN 'RESURRECTED', with resurrected = 1 in the line and the metric. 4. The same happens once for every such dead cell in the bucket.

**Why the tests stay green.** Every mark-only test uses live cells (mark_only_passes_compact_and_mark_what_passes_delete, readers_and_mark_only_passes_warn_of_what_a_wrong_pass_marked, gc.rs dry_runs_and_mark_only_passes_delete_nothing) or dead cells that are not removable: an abandoned Forking clone (the test around line 4980) or a child deleted from a writable fork (dry_runs_and_mark_only_passes_retire_nothing). None runs two mark-only passes over a removable dead cell, and none checks that the collector's own reads raise no warnings.

**Backlog.** Related: TASK-455 (In Progress) designed dead-cell removal with no marks of its own but did not consider a first mark-only pass. TASK-264 (Done) built mark-only before removal existed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A mark-only pass over a removable dead cell (a deleted Root or Clone with only its head and collector lease left) marks neither its head nor its collector lease.
- [ ] #2 A second mark-only pass over that cell reports nothing RESURRECTED: the log line and cladefs_gc_sweep_resurrected_objects stay at 0. Meanwhile neither the collector's own reads nor any other reader of the head or collector lease logs the 'a reader reached what the collector's newest mark-only pass found no live cell needs' warning.
- [ ] #3 A collector test makes a cell with an Allocator ID, writes a segment, checkpoints, deletes the cell with lifecycle::delete, and runs mark_cell twice with marks::captured(). It asserts the criteria above and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
