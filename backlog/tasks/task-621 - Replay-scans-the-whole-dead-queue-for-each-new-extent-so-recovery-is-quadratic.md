---
id: TASK-621
title: >-
  Replay scans the whole dead queue for each new extent, so recovery is
  quadratic
status: To Do
assignee: []
created_date: '2026-11-18 15:34'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1012000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-alloc review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/fxfs/src/object_store/allocator/extent_table.rs`: `rg -n -F 'if let Some(index) = self.dead.iter().position(|dead| dead.extent == extent) {' crates/fxfs/src/object_store/allocator/extent_table.rs`

`ExtentTable::commit` with `Applied::Replayed` calls `revive`. For any extent not alive, which includes every freshly minted extent the journal allocates, `revive` scans the whole `dead` VecDeque. At replay that queue holds the info's `dying` list, every extent of owners marked for deletion (killed by `load`), extents found empty, and every extent replay has killed so far. Replay therefore costs A_new × D comparisons.

**Trigger.** A crash after a churn workload, or after deleting a volume and before the compaction that filters its owner. On mount, `load` queues the deleted owner's N extents as dead, then replay of an up to 8 MiB journal (JOURNAL_MAX) applies about 10–20k Allocates of new extents, each scanning N + replay deaths. With N = 1M that is about 1–2×10^10 comparisons, tens of seconds of mount CPU. Pure churn gives about 10^8.

**Why the tests stay green.** Replay tests use small journals and few dead extents. No test measures mount time against the size of the dead queue.

**Backlog.** Related to TASK-210 (Done), which added the revive and settle design, and TASK-79.22 (Done), which changed how dying extents are kept. Neither addresses this cost.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Replaying a journal with A newly allocated extents while D extents are queued as dead examines O(A + D) dead-queue entries, not O(A × D). With A = 20k and D = 200k, replay examines under 1 million entries, against about 4×10^9 today.
- [ ] #2 A test in crates/fxfs marks an owner with about 200k extents for deletion, crashes with about 20k new allocations in the journal, remounts, and asserts the bound above by counting dead-queue entries examined. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
