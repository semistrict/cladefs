---
id: TASK-681
title: >-
  A replicated cell whose f+1 members are lost for good can't be taken: the
  forced take from the bucket alone that doc-21 and doc-12 promise is not built
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 00:59'
updated_date: '2026-11-20 04:25'
labels:
  - durability
  - peers
dependencies: []
references:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
priority: medium
type: bug
ordinal: 1062000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 comparing with FoundationDB's forced recovery (forceRecovery, KillRegion: recovery with known data loss when too few logs survive). doc-21's failure table (row 'Writer's host and a peer lost') says the take waits for the lost peer and 'the VM manager may then take from the bucket alone, by an explicit force, which loses that tail'. No such force exists: Shared::fence fails the take while fewer than f+1 members besides the writer answer, cladefsctl and the control API have no option to skip it, so a cell whose members' hosts are gone for good stays untakeable for ever.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The control API and cladefsctl can take a replicated cell from the bucket alone, by an explicit force, which logs and reports the commits it may lose
- [x] #2 A forced take still fences the members it reaches and recovers what they hold
- [x] #3 A test loses f+1 hosts of a configuration and takes the cell by force; without it, the take is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC evidence: PlaceCell's and CreateCell's force, cladefsctl place/take --force, upload::Options::force; forced_after (StatCell, optional, the bucket's last commit) noted in the store's description with its placement; takes_of_cells_whose_members_are_lost_go_on_by_force (local_cell: refused without force, forced recovers; cladefs e2e on ubu1: refused EIO, forced, forced_after = the bucket's last, writes on). Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A replicated cell whose f+1 members are lost for good can be taken by force: PlaceCell/CreateCell force (cladefsctl place/take --force). The take fences the members that answer and recovers what they hold; StatCell's forced_after names the bucket's last commit, past which commits told Replicated may be lost, kept in the store's description for that placement. Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:FINAL_SUMMARY:END -->
