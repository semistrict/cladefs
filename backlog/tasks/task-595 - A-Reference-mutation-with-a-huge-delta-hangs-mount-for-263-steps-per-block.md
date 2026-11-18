---
id: TASK-595
title: A Reference mutation with a huge delta hangs mount for 2^63 steps per block
status: To Do
assignee: []
created_date: '2026-11-18 15:30'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 986000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-journal review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs/src/object_store/journal/checksum_list.rs`: `rg -n -F 'for _ in 0..delta.unsigned_abs() {' crates/fxfs/src/object_store/journal/checksum_list.rs`

Replay's validation pass calls `update_checksum_list` for each mutation. `validate_mutation` accepts any `AllocatorMutation::Reference` whose range is block-aligned and whose `delta != 0`. `mark_referenced` then applies the reference count one step at a time: `for _ in 0..delta.unsigned_abs()` per overlapping block, with each step calling `Block::reference` or `Block::release`. `delta` is an i64 read from the image, so `i64::MIN` (or any huge value) makes the loop run ~2^63 times for every block with a checksum entry in the range. The mount never finishes, and the guest kernel thread spins with soft lockups. The data checksums this needs are what the guest writes by default (barriers off, so every data write journals DataChecksums), so one resealed Reference record for a range written after the last DidFlushDevice is enough. The effect is O(1) to compute: `shared = shared.saturating_add(n - 1)` after the first un-death, and the reverse for releases. Alternatively, validate_mutation could refuse a |delta| no transaction makes.

**Trigger.** 1. A guest disk (barriers off, the guest default) whose journal, after its last DidFlushDevice, holds DataChecksums for range R of extent E. Any write after the last flush does.
2. The same or a later committed transaction holds AllocatorMutation::Reference { device_range: R, owner_object_id: <store>, delta: i64::MIN }, resealed. A shared version crafted this way hangs every tenant VM that forks it.
3. Mount -> Journal::replay -> the validation loop: checksum_list.push(R) puts it in the list. validate_mutation(Reference) is true because delta != 0. update_checksum_list -> mark_referenced -> for_each_block over R's blocks -> 2^63 iterations each. Replay never returns.

**Why the tests stay green.** The mark_referenced tests use delta ±1. corrupt_tests' images hold no Reference records near DataChecksums, so a flip cannot make one. fuzz_journal would need a Reference whose ExtentId and range overlap an arbitrary DataChecksums record's, with a huge delta, and corrupt_tests' watchdog only catches what its images can produce.

**Backlog.** New. Breaks TASK-149.9's (Done) rule that a corrupt image fails with an error, not a hang; that task fixed a bloom-filter spin of the same kind. TASK-26.19.6 (Done) added the Reference mutation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 mark_referenced with a delta of i64::MIN or i64::MAX over a range with checksum entries returns at once and leaves the same state as a delta of that sign and size, in a checksum_list.rs test run under a short timeout.
- [ ] #2 Mounting a journal that holds DataChecksums for a range R and a resealed Reference{R, delta: i64::MIN} mounts or fails with Inconsistent within seconds; it never hangs.
- [ ] #3 Both tests go red (time out) with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
