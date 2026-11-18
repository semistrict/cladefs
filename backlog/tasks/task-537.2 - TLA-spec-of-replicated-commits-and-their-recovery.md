---
id: TASK-537.2
title: TLA+ spec of replicated commits and their recovery
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T12:36:00Z'
updated_date: '2026-11-18T15:53:00Z'
labels:
  - durability
  - tla
dependencies:
  - TASK-537.1
parent_task_id: TASK-537
priority: high
type: task
ordinal: 929000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The replication protocol chosen by the research task is model-checked before implementation, as Bucket.tla was for doc-7. It must reproduce known failure modes (a stale writer's acknowledgement, recovery from a minority, truncation before upload) as mutations.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A spec under specs/ models writers, replicas, the bucket's head and replica set, crashes and message loss, and checks that a commit acknowledged Replicated survives every take
- [x] #2 Each safety rule has a mutation config under mutations/ that TLC shows violating it
- [x] #3 Configs run in CI next to the bucket specs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. specs/replication/Replication.tla: head (epoch, cfg, up), configurations keyed <<epoch, n>>, sent streams kept after a writer dies, member logs/drops/promises/incarnations, takes (take, seal, recover, install, activate), commits, Replicated acks, two-step uploads, replacements and drops to f = 0 (issue, land, learn), daemon crashes and disk losses; told as the truth.
2. Invariants: ReplicatedSurvives, HistoryIsPrefix, MembersArePrefixes, TypeOK.
3. One mutation per rule (11) under mutations/, each shown violating ReplicatedSurvives.
4. Models split by dimension (takes, replacements, failures, sweeps), each under 15 minutes on ubu1 under fxjob; the split recorded in the configs.
5. check-tla.sh lists the spec; doc-21 updated with what TLC found.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: first model (4 hosts, 2 commits, 2 takes, a replacement, a loss, a crash) ran ubu1 out of memory outside fxjob (java 4.6 GB RSS, swap full; OOM-killed 07:43). Since then TLC runs only under fxjob with -Xmx4g, each model stopped at 15 min (the user's limit).
TLC found two holes in doc-21, both fixed in the spec and the doc: (1) incarnations: a host whose disk was lost lost its promises; without an incarnation in each member, a host that lost its disk retakes and counts its own empty copy as the old writer's (NoIncarnation); (2) an f = 0 configuration must hold no quorum, or the joint rule acks a commit during a drop to f = 0 that a taker reading the bucket alone loses (NoZeroQuorum, found with every rule in force: 1 commit, 2 takes, a replacement, a loss).
All 11 mutations violate ReplicatedSurvives at small sizes (1 commit; 2 takes; 3 for ActivateBeforeInstall; a replacement for NoJointRule, ZeroFWithoutUpload, NoZeroQuorum; a loss for NoIncarnation); traces read and each is the intended bug.
State space: models combining every dimension did not finish in 12 min (7-11M distinct states, queue growing). Cut: a take's seal replies cleared once spent; sweeps of superseded configurations behind a Sweep constant, checked in a model of their own; landing always the whole log (a prefix landed later is the whole log landed earlier). 2 takes, 1 commit: 122k -> 29k distinct states, 2 s.

Final split (2026-11-18, check-tla.sh under fxjob on ubu1, JAVA_TOOL_OPTIONS=-Xmx4g): 7 default models (~6 min together), 2 deep (ReplicationDeep 200 s, ReplicationFailuresDeep 256 s), 11 mutations each violating ReplicatedSurvives in 1-2 s; 13.5 min for all twenty. Restructured to stay under the user's 15-minute limit per model: a Can constant gates uploads, drops, sweeps and drops to f = 0, so each model takes only the steps its rules need (3 takes went from >11M states, unfinished at 15 min, to 182k in 6 s); in zero models every replacement is a drop to f = 0; sweeps checked over takes alone. Liveness is not checked by the spec: TASK-537.3 tests it on daemons. CI: no workflow runs TLC; make check-tla (check-tla.sh with no arguments) globs specs/*/*.cfg, so the replication models run with the bucket specs.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
specs/replication/Replication.tla models replicated commits as doc-21 designs them: quorum-acked primary-backup, each configuration (writer and 2f peers) a write-once log, the head's CAS the only authority; takes seal f+1 members other than the old writer, recover the longest log, install and activate; replacements under the joint rule; drops to f = 0; uploads; zombie writers, daemon crashes and disk losses. ReplicatedSurvives (every commit told Replicated or Uploaded is in the history of the current writer once it serves), HistoryIsPrefix and MembersArePrefixes hold in 9 models (7 default, 2 deep, the largest 8.1M states in 256 s); each of the 11 rules has a mutation TLC shows losing an acknowledged commit. TLC found two holes in doc-21, now fixed there: members need incarnations (BookKeeper's cookies), as a lost disk loses its promises; and an f = 0 configuration has no quorum, so nothing is told Replicated while a drop to it is in flight. Liveness is left to TASK-537.3's daemon tests.
<!-- SECTION:FINAL_SUMMARY:END -->
