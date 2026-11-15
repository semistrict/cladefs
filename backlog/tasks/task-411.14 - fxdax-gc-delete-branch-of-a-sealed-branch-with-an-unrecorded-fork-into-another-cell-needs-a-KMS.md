---
id: TASK-411.14
title: >-
  fxdax-gc --delete-branch of a sealed branch with an unrecorded fork into
  another cell needs a KMS
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 00:17'
updated_date: '2026-11-15 03:24'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: low
ordinal: 810000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.4 seals the parent's checkpoint at a fork point under the parent's DEK, as a child holding the parent's keys refuses one in the clear. A deletion that records the forks of its branch writes that checkpoint for a fork into another cell whose child has no record yet (its writer died between the fork's acknowledgement and recording it): branch::delete_under takes the host's KMS for it, and without one fails (Error::Kms) rather than write a checkpoint the child refuses. fxdax-gc reads no FXDAX_KMS (doc-16: 'fxdax-gc needs none'), so its --delete-branch of such a branch fails until it has one. fxdax's own deletions (host_store::delete) pass the host's KMS.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 fxdax-gc --delete-branch deletes a sealed branch whose fork into another cell has no record yet, the checkpoint at the fork point sealed under the branch's DEK, given FXDAX_KMS
- [x] #2 Without a KMS it fails naming the tenant and what it needs, leaving the branch as a deletion stopped partway
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. fxdax-gc --delete-branch reads FXDAX_KMS (the kms() its sweeps read) and calls branch::delete_under with a keys::Kms over it (delete_branch fn); a bad FXDAX_KMS stops it before deleting.
2. With no KMS, an Error::Kms from the deletion becomes 'deleting branch B: tenant T: cell C is encrypted, and this host has no KMS: fxdax-gc reads it from FXDAX_KMS, which names none; the deletion stopped partway: run it again with FXDAX_KMS to finish it'. delete_under ends the lease first and marks the record deleted only after recording the forks, so the branch is left as a deletion stopped partway; other errors name the branch.
3. Test (extent_remote tests/gc.rs, the binary, file bucket): sealed cell with LocalKms, branch 1 with DEK, a sealed segment, then a fork segment into cell 1/2 uploaded but never acknowledged (child unrecorded). Without FXDAX_KMS: exact stderr, exit 1, record not deleted, child unrecorded, no checkpoint. With FXDAX_KMS=local://DIR: success, record deleted, child recorded at Origin{1, at}, one checkpoint, Authenticated under KeyId{1,1}.
4. Docs: fxdax-gc module docs, doc-16 (TASK-411.4 bullet, keys table), doc-7 (--delete-branch).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t3/cache-tenant-keys-gc. fxdax-gc already read FXDAX_KMS for sweeps since TASK-411.6 (doc-16's 'fxdax-gc needs none' predated it); --delete-branch went through branch::delete (no KMS). Now delete_branch(bucket, branch, kms()?) -> delete_under(.., Some(keys::Kms::new(kms))). The KMS is asked only when a checkpoint at a fork point is to be written (keys::Parents::Kms), so clear branches and sealed ones with recorded forks ask nothing, as before. Error mapping only when no KMS was given: an Error::Kms then can only be the missing KMS; refusals of a given KMS (revoked, destroyed) pass through as 'deleting branch B: tenant T: its KEK is revoked'.
Test: extent_remote tests/gc.rs deletes_sealed_branches_recording_their_forks_into_other_cells_under_the_kms passes; the old code fails its second run (branch::delete has no KMS). The run showed 'LEAK' on the untouched collects_a_bucket_once (nextest leak detection of the child process's handles; passes).

Validation (macOS, branch t3/cache-tenant-keys-gc at fb253eb4a): cargo nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p fxdax -E 'not (package(extent_store) & test(/^sim::/))': 960/960 passed (extent_keys 27, extent_frame 33, extent_remote 323, extent_store 223, fxdax 354; 1 leaky: faults::tests::faults_say_their_words, untouched). sim::one SIM_SEEDS=10 over collectors, everything, forks, takeovers, crashes, wals, forked_wals, restores, fresh_cells, failed: all pass. clippy --all-targets (extent_remote, extent_store, fxdax): no warning in touched files. fmt clean on touched crates. scripts/upstream-changes.py --check passes. No dependency change. Linux not run (no ubu1 per brief): DoD #1 open.

Verified 2026-11-15 at 594ff82e9 (origin/main merged): macOS scripts/test.sh workspace 2466/2466, shuttle, scripts (mutants 0 broken), go; Linux ubu1 under fxjob (target on /mnt/fxdax-scratch), every part: clippy, workspace 2752/2752, sim 63/63, shuttle, scripts, go, guest, firecracker. Only fxdax's own crates changed (extent_keys, extent_frame, extent_remote, extent_store, sqlite_extents, fxdax, fxdax-examples): no upstream deviation; scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
fxdax-gc --delete-branch now deletes through branch::delete_under with a keys::Kms over FXDAX_KMS (the KMS its sweeps already read), asked only when a checkpoint at a fork point is to be written: a sealed branch whose fork into another cell has no record yet gets that checkpoint authenticated under its DEK and its child recorded. Without a KMS it fails ('deleting branch B: tenant T: cell C is encrypted, and this host has no KMS: fxdax-gc reads it from FXDAX_KMS, which names none; the deletion stopped partway: run it again with FXDAX_KMS to finish it'), lease ended, record not yet deleted, child unrecorded; running it again with the KMS finishes. doc-16 and doc-7 updated. Verified by tests/gc.rs deletes_sealed_branches_recording_their_forks_into_other_cells_under_the_kms (the binary over a file bucket, both runs); 960 macOS tests and a 10-seed sim sample over 10 profiles pass; Linux not run.
<!-- SECTION:FINAL_SUMMARY:END -->
