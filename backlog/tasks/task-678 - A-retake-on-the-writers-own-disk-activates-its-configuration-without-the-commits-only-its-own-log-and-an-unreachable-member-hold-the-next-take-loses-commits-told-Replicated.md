---
id: TASK-678
title: >-
  A retake on the writer's own disk activates its configuration without the
  commits only its own log and an unreachable member hold: the next take loses
  commits told Replicated
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 00:42'
updated_date: '2026-11-20 01:04'
labels:
  - durability
  - peers
dependencies: []
references:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
  - specs/replication/Replication.tla
  - crates/local_cell/src/upload.rs
  - crates/local_cell/src/replicate.rs
priority: high
type: bug
ordinal: 1060000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 checking specs/replication against the code. doc-21 Take step 5 and the spec's Activate require f+1 members of the new configuration to hold everything the take recovered. The code (local_cell upload.rs, Shared::start then Shared::replicate) installs only the members' seed: the replicator starts after the store's last commit (Replicator::base), so commits recovered from the taker's own write-ahead log beyond what the fenced members held are sent to no member of c+1, and the head is swapped to c+1 at once (at once with no wait at all when the seed is empty). Reachable: the old writer's daemon restarts on its own disk, f members besides it fenced (fence() needs f+1-1 when it holds its own log), the member holding the told commits out of reach. doc-21's 'As built (Replicator::base)' note covers only commits after the take, not those told before it. Replacements share the root cause: Replicator::replacing sends only what is spooled. Reproduced both ways: specs/replication/mutations/InstallMembersOnly.cfg (the spec with the new rule installOwn removed, modelling the code) violates ReplicatedSurvives in 3 s, 17 states deep; crates/local_cell/tests/replicated.rs retakes_on_the_writers_disk_install_what_its_own_log_holds fails today (5 hosts: a writes with c out of reach, b holds the commits; a restarts with b out of reach, activates {a,c,d}; a lost, e takes from c, d and the bucket and lacks them).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 retakes_on_the_writers_disk_install_what_its_own_log_holds passes
- [ ] #2 A take activates its configuration only once f of its members hold every commit it recovered, its own write-ahead log's included
- [ ] #3 A replacement after such a take installs those commits too, or waits for the bucket to hold them
- [ ] #4 doc-21's Take steps 5-6 'As built' note corrected; specs/replication's installOwn rule and InstallMembersOnly mutation kept
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
