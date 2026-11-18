---
id: TASK-570
title: 'A cell''s versions are never capped, so its head grows without bound'
status: To Do
assignee: []
created_date: '2026-11-18 15:27'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 961000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-write, cross-host-take-restore review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/layout.rs`: `rg -n -F 'self.versions.push(Version { id, at, shared: false });' crates/bucket_cell/src/layout.rs`
**Where:** `crates/bucket_cell/src/layout.rs`: `rg -n -F 'self.versions.push(Version { id, at, shared: false });' crates/bucket_cell/src/layout.rs`

layout::MAX_VERSIONS (256) says a version past it is refused unless keep_last drops the oldest, and doc-18 says making one past the bound fails with EMLINK. Nothing enforces it: rg finds only the constant and a bench comment, and EMLINK appears nowhere in crates/. Head::naming (crates/bucket_cell/src/layout.rs) appends every version with no bound, after a linear self.version(id) check for each, and without, sharing and version are O(N) too. Every path can pass keep_last None and makes as many versions as asked. One is a VM process's VERSION request: Device::take_snapshot -> CellClient::make_version (Request::Version) -> store.make_version(None) -> OpenCell::make_version -> commit_version -> LocalCell::commit_version, which checks only quiescence before close_for_version and counts nothing. The others are the control API's MakeVersion with keep_last 0 and cladefsctl's make_version(&dir, None). The VM snapshot path can never set keep_last, so unbounded growth is its default. Making a version also skips the uploader's lag wait (commit_version -> commit_locked). That is doc-18's design, a version being local, but it means only the local fsync slows a VERSION loop. Each version adds 17 to 25 bytes to the head. Every head swap re-encodes and PUTs the whole head: O(N) bytes per acknowledgement and O(N^2) in total. Swaps come from each Writer::acknowledge (about one per uploaded segment), each checkpoint and each share. Every collector pass adds an origin for each named version and recovers its state (recover_ats / recovery::states_at), whose comments assume at most 256. Every take, fork, restore and head::get downloads and decodes the larger head. The design kept heads at a few KB because every acknowledgement writes the whole head. Triggers: a VM manager that takes a VM snapshot every minute with keep_last 0 and never drops one reaches 100k versions in about 70 days. That is a head of 1.7 to 2.5 MB, which a busy cell PUTs about once a second, so uploads stall behind head swaps and the lag bound throttles commits. A compromised VM process looping VERSION requests without writing (at_rest holds, so each makes a version) gets there faster. Failed VM snapshots that leak fork versions (TASK-514) add more. Tests stay green because versions-pass-bench stops at 256 (it treats MAX_VERSIONS as a cap), heads_name_their_versions tests only keep_last windows, and no test makes more than 256 versions or expects EMLINK. The refusal belongs where a version is made: OpenCell::make_version should count the head's versions plus local ones. Refusing at naming would break versions already reported local.

**Backlog.** Related: TASK-476 and TASK-508.2 (In Progress) build doc-18's versions, but neither lists the cap in its ACs. TASK-514 (To Do) is one more way versions pile up and does not name the missing bound.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Making a version of a cell that already has 256 versions, with no keep_last, fails with EMLINK on every path: a VM process's VERSION request, the control API's MakeVersion with keep_last 0, and cladefsctl. The head still names 256 versions.
- [ ] #2 However many versions are asked for, a cell's encoded head, and so each acknowledgement's head PUT, stays at most the size of 256 version records plus the rest of the head.
- [ ] #3 Regression tests go red with the fix reverted: a bucket_cell writer test on SimBucket that makes 257 versions with keep_last None, and a cladefsd test where the 257th VERSION request on a connection is answered EMLINK.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
