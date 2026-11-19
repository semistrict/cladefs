---
id: TASK-569
title: >-
  A version dropped while its segment is acknowledged is named in the head
  anyway
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:27'
updated_date: '2026-11-19 10:48'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 960000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-write review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/writer.rs`: `rg -n -F 'cuts.into_iter().filter(|(_, cut)| !inner.dropped.contains(&cut.id)).collect();' crates/bucket_cell/src/writer.rs`

`Writer::acknowledge` filters the versions it will name against `inner.dropped` under the `inner` lock. It then releases that lock and awaits `self.held.lock()`, and swaps the head with `naming(named)` using that stale list.

`drop_versions` adds the IDs to `dropped`, then `change_versions` takes `held` and computes `head.without(ids)`. If the head does not name the version yet, `next == held.head` and it returns Ok without swapping.

If the drop runs its whole held section between acknowledge's filtering and acknowledge taking `held`, the acknowledgement names the version afterwards. The caller was told the drop succeeded, but the head names the version: `Writer::versions` lists it as uploaded, forks may restore it, and every collector pass keeps its whole state (rule `versions_kept`) until someone drops it again.

`held` is a `futures::lock::Mutex`. Its unlock wakes the first waiter in slab order, not the oldest, and hands nothing off. So whenever another control call (ShareVersion, another DropVersions) holds `held` across its bucket PUT, the drop can win over an acknowledgement that filtered before it. doc-18 promises that a drop puts the ID in the set "which no acknowledgement names". It accepts a lost drop only after a crash, not this race.

**Trigger.** 1. The VM manager calls MakeVersion on cell C, giving version X (local).
2. The uploader uploads X's segment. `acknowledge()` computes cuts = {X}, releases `inner` and waits on `held`, which a concurrent ShareVersion(Y) holds across its head PUT.
3. A concurrent DropVersions(X) call (`OpenCell::drop_versions` → `Uploader::drop_versions`) runs `forget_versions`, `dropped.insert(X)`, then `change_versions` waits on `held`.
4. Y's swap completes. The mutex wakes the drop's waiter first (lower slab index). The head does not name X, so the drop returns Ok to the caller.
5. `acknowledge` takes `held` and swaps `holding(..).naming([X])`. The head names X for good.

**Why the tests stay green.** `heads_name_versions_as_their_segments_are_acknowledged` drops sequentially, after the acknowledgement. The simulation does spawn drops concurrently (`World::drop_version`), but its oracle (world.rs, the "names version … no more" check) only flags versions that are not dropped and missing from the head. It never flags a dropped version that the head still names, so this interleaving passes.

**Backlog.** Related to TASK-476 (In Progress; the versions design) and TASK-499 (Done; the simulation's handling of an ambiguous fenced drop). Neither names this race, and Bucket.tla models only drops of versions the head already names.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Once DropVersions(X) has returned Ok while X's segment is being acknowledged, the cell's head never names X, whichever of the drop and the acknowledgement takes the head lock first.
- [x] #2 The simulation reports a violation for any version marked dropped that the cell's head still names.
- [x] #3 A unit test in crates/bucket_cell/src/writer.rs, on SimBucket, holds a ShareVersion PUT pending. It parks acknowledge() and drop_versions({X}) on the head lock with the drop woken first, then completes both and asserts the head names no X. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Acknowledge filters dropped versions holding the head lock, under which drops unname. Test: writer.rs acknowledgements_name_no_version_dropped_while_they_wait_for_the_head (holds the head lock itself rather than a ShareVersion PUT; a mutant reading dropped before the lock fails it). The simulation reports a version a live writer dropped, told so, that its head still names (crashed hosts' drops excluded: their late writes may name it, as doc-18 accepts); I have not checked that a seed finds the race with the fix undone. macOS: clippy, workspace 2712/2712, sim 41/41; Linux (ubu1, 808ed60ca): workspace 3044/3046, the fork-steps test updated since and passing alone (43a0c9ae2), windows_are_where_snapshots_had_them failing once, passing 55 runs after (TASK-655); scripts/upstream-changes.py --check. Hand mutants (target/b2-mutants.py), each caught by its test.
<!-- SECTION:NOTES:END -->
