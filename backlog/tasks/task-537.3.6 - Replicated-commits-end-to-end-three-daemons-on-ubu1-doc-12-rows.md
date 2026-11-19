---
id: TASK-537.3.6
title: 'Replicated commits end to end: three daemons on ubu1, doc-12 rows'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-19T05:36:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.5
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 936000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 6. The guarantee must be shown on real daemons, including the liveness the spec leaves out.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Three daemons: the writers daemon and disk killed mid-workload, every Replicated commit present after a take elsewhere
- [x] #2 A member down: commits stay Replicated after replacement; both down: released by Uploaded
- [x] #3 doc-12 gains the Replicated level and its failure rows with their tests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Built (c844d4cb9, 4d2bd50bd, a296e3ee2, 1423f3594, a658215fe, d74d76b31, on main): crates/cladefs/tests/replicated.rs runs cladefsd processes with replica directories, sharing a file bucket, the bucket out of reach by moving its directory aside (root-proof; each outage checked against the head's segments).
- AC 1: three daemons; a thread commits (COMMIT_UPLOADED, Replicated) with the bucket out of reach; the writer's daemon killed mid-workload and its directory removed; a take on another host holds every commit acknowledged before the kill.
- AC 2: four daemons; a member killed, the writer replaces it with the spare (head checked), commits Replicated again with the bucket out of reach; both killed, a commit stays pending while the bucket is out of reach and returns once it is back; drop to f = 0.
- AC 3: doc-12 gains the Replicated level and a table of failure rows for replicated cells, each with its tests; two rows say plainly they have none (a replicated cell's daemon killed and reopened on the same disk; the bucket failing with both members lost).
Found by the tests: a take with one host of three lost could make no configuration and failed. It now serves alone at f = 0, the head naming the old configuration until the acknowledgement naming the last recovered commit names its own in the same swap (Writer::configure_past). Spec: RecoverAlone, rule aloneSwitch, models ReplicationAlone (11.8M states, 238 s) and ReplicationAloneRetake (508K, 10 s; constraint OwnCopiesOnly), mutation AloneNoSwitch (caught in 4 s). All 23 non-deep replication models and mutations re-checked, each under 6 min. trimmed went back to commits (local segment numbers are not the head's).
Reviews: Codex (StatCell's configuration after the switch; tests of the switch riding its acknowledgement and of commits pending with both members down), the verifying reviewer (forged member records could make trimmed drop honest logs before any MAC check, now refused; read-only directories don't stop root, ubu1's runs). All fixed with tests.
Verified under fxjob on ubu1 (temp files on /home while the scratch disk was full): local_cell 360 tests, bucket_cell writer/head/wal 50, cladefs replication/peers/control 27, the daemon tests 3x and as uid 0 in a user namespace; clippy with test.sh's flags clean.
Not done: a guest-level run (pgbench in VMs, scripts/test-host.sh) needs sudo, which this session lacks; the liveness of an idle cell noticing a lost member before its next commit.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Replicated commits are shown end to end on real cladefsd processes: the writer's daemon and disk lost mid-workload, every commit told durable recovered elsewhere; members replaced; both members lost, commits wait for the bucket and the cell drops to f = 0. doc-12 has the Replicated level and failure rows with tests. The tests exposed a take that failed with one host of three lost; it now serves alone, switching the head's configuration atomically with the acknowledgement that covers what it recovered, checked by new TLA+ models and a mutation. Reviews found and fixed a forged-log trimming hole and a root-ineffective test outage.
<!-- SECTION:FINAL_SUMMARY:END -->
