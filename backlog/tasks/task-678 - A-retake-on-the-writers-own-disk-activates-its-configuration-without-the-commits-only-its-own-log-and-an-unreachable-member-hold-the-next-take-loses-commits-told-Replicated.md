---
id: TASK-678
title: >-
  A retake on the writer's own disk activates its configuration without the
  commits only its own log and an unreachable member hold: the next take loses
  commits told Replicated
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 00:42'
updated_date: '2026-11-20 04:25'
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
- [x] #1 retakes_on_the_writers_disk_install_what_its_own_log_holds passes
- [x] #2 A replacement after such a take installs those commits too, or waits for the bucket to hold them
- [x] #3 doc-21's Take steps 5-6 'As built' note corrected; specs/replication's installOwn rule and InstallMembersOnly mutation kept
- [x] #4 A take whose own log holds commits past what its members can be given activates its configuration only by the acknowledgement that names them all (Writer::configure_past), the head naming the configuration before until then, nothing told Replicated meanwhile (decided over installing the own log on the members: OPEN_QUESTIONS.md)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC evidence: retakes_on_the_writers_disk_install_what_its_own_log_holds passes; activation deferred (Shared::replicate's configure_past; spec RecoverAlone with f=F, deferQuiet, ReplicationDefer and ReplicationDeferCommits hold, DeferTellsReplicated violates); a replacement waits for the switch (keep's stale; deep ReplicationDeferReplaces holds, the replicated simulation's replacements); doc-21 Take 5-6 corrected; installOwn/InstallMembersOnly kept. The verifying review found a retake could give c+1 an earlier writer's logs: fixed (Recovering::holds_commits), test retakes_give_their_configurations_no_earlier_writers_logs caught by a hand mutant. Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A take whose own write-ahead log held commits past what the members it fenced gave (a retake on the writer's disk, the member holding them out of reach) activated c+1 at once, none of its members holding commits told Replicated: the next take elsewhere lost them. Now such a take defers its configuration's activation to the acknowledgement naming them (head names c until then, nothing told Replicated meanwhile, keeper uploads at once), and gives c+1's members the old configuration's logs only if they are its own history. Rule bucket_cell::Rules::installs_own (the simulation finds the loss without it). Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:FINAL_SUMMARY:END -->
