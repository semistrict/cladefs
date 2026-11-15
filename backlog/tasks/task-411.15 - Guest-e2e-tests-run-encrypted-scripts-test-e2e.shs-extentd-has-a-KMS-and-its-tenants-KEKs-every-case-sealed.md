---
id: TASK-411.15
title: >-
  Guest e2e tests run encrypted: scripts/test-e2e.sh's extentd has a KMS and its
  tenants KEKs, every case sealed
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 03:35'
updated_date: '2026-11-15 05:58'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
type: task
ordinal: 813000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411's AC #3: the VMM and guest end-to-end paths (scripts/test-e2e.sh, guest/tests) have no encrypted variant. The user chose (2026-11-14) that e2e runs encrypted only: no clear e2e runs; plaintext stays covered by the host-side suites. Today test-e2e.sh starts extentd with no FXDAX_KMS, so every guest's store is a plaintext cell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 scripts/test-e2e.sh starts extentd (and every process of the cases that opens stores: fxdaxctl, fxdax-gc, FUSE mounts) with FXDAX_KMS naming a LocalKms holding a KEK for every tenant the cases use, tenant 0 included or the cases moved to granted tenants
- [x] #2 A check in the harness fails a run whose stores are not sealed: every cell the run made has a wrapped KEK and every branch record a DEK, and no object of the bucket nor file of the stores' directories holds a known plaintext a case wrote
- [x] #3 The e2e suite passes encrypted on ubu1 (test-e2e.sh, --tag e2e), and the nightly/qualification scripts that run it (scripts/qualify.sh, bench scripts) run encrypted too
- [x] #4 Guest checks' time budgets hold encrypted, or the cases that overrun are named with their measured times
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/test-e2e.sh: a LocalKms beside the run's sockets (/tmp/fxdax-e2e-sockets.*/kms, gone with them, not in the results), KEKs for tenants 0-15 made by `runner kms`; FXDAX_KMS and FXDAX_E2E_SEALED exported, so the daemon, the cases' own daemons, the runner's in-process stores, fxdaxctl, fxdax-gc and FUSE inherit them; a caller's FXDAX_KMS kept. A daemon (no FXDAX_KMS string in it) or runner (no `kms`) built before encryption runs in the clear, unchecked, saying so; results/encryption.txt says which.
2. Tenant 0 gets a KEK (doc-16 lets it; the default tenant's socket, fxdaxctl create and the cases' in-process stores are tenant 0's; fxdax's tests seal it already); granted IDs to 15 cover fxfs-guest-migrate-tenant-dax's 1-4.
3. The check, keyless (test_cases::sealed, extent_store::on_disk): cells have KEKs (or shredded), records of branches that wrote objects have DEKs (not points/deleted), framed objects not Seal::Clear, store dirs' framed files and logs not clear, and no run of test_cases::canary in any object or file read. The runner checks each passed case's directory (FXDAX_E2E_SEALED); `runner sealed` checks the run's bucket whole and part cache at its end.
4. Known plaintext: fxfs-guest's written() and fxfs-guest-checks' pattern() are the canary.
5. Tests: extent_store on_disk in sealed_stores_raw_files_and_logs_hold_no_plaintext (sealed and clear twin); test_cases canary and sealed unit tests.
6. ubu1: single cases, negative control (KMS with no KEK fails the check), S3 (RustFS), sandboxes short, full --tag e2e once; times against limits.
7. doc-16 section; qualify/sandboxes/bench/nightly go through test-e2e.sh unchanged.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 ubu1, 04d5b312, prebuilt artifacts, kernel release-c87bcefc (cached; its sources differ from this tree's only in the workspace Cargo.toml's dependencies): fxfs-guest OK 1 s of 15 s, fxfs-guest-checks-dax OK 62 s of 1628 s, extent-guest OK 0 s of 15 s, each 'sealed: 1 stores, 1 cells, 1 records, N objects ... 0 in the clear'; run end: run bucket and part cache 0 in the clear. Negative control (FXDAX_KMS naming a KMS holding no KEK, tenant 0's stores in the clear): fxfs-guest FAILS the check: 21 in the clear (cell with no KEK, segments/checkpoints/moves framed in the clear, clear logs, the canary in a segment at offset 0), and the run-end check fails on the part cache's copy of that segment. Harness unit tests (canary, sealed) pass on ubu1.

Full run, ubu1 2026-11-15, 04d5b312, scripts/test-e2e.sh --tag e2e built (not prebuilt; its scripts/test.sh guest+firecracker parts passed: 130, 119, 9, 30, 31 tests), 05:15-05:42 UTC: 16/16 cases pass, every one 'sealed: ... 0 in the clear'; run end: run bucket (8 cells, 8 records, 104 objects) and part cache 0 in the clear; daemons' budgets (scripts/budgets.py) ok. Times (s of limit): fxfs-guest 1.6/15, -crash 1.9/15, -pgbench 379/1200, -pgbench-dax 376/1200, -checks 47/1428, -checks-dax 73/1628, -checks-dax-delay 82/1628, -checks-dax-small 77/1618, -checks-overrun 62/1440, -dax 1.5/15, -crash-dax 3.3/15, -crash-dax-killed 2.7/15, -crash-stray-dax 4.3/15, -docker-dax 19/300, extent-guest 0.5/15, extent-guest-other-version 0.4/15. Every guest check held its budget (the suites fail on one that doesn't; -overrun passes on its own check failing, as designed).
S3 (scripts/test-e2e-rustfs.sh, RustFS): fxfs-guest and fxfs-guest-checks-dax pass sealed, the check listing and reading s3://fxdax/e2e.
scripts/sandboxes.sh short (what qualify.sh runs, long): runs sealed, the run-end check over the S3 bucket finds 0 in the clear, but the case fails its own budgets: writes 1.90 and 1.87 times the guests' bytes against 1.5, and once framings over 1000 ms (1093.9 ms). Same artifacts with a KMS holding no KEK (clear): 1.16 times, passes. Cause: sealed stores never frame in place (segments_framed_in_place 0 against 55-58; written_framed 3.7-3.8 GB a host against 0.32-0.36 GB). Filed TASK-411.23 (parent TASK-411). The sandboxes case drops its stores before it ends: its own per-case check found 0 stores; the run-end check over the whole bucket caught the clear run (512 findings), so that path is covered there.
Mac: cargo clippy -p extent_store --all-targets (no new warnings), extent_store sealed_stores_raw_files_and_logs_hold_no_plaintext passes, python3 -m unittest discover -s scripts/tests 317 OK, scripts/test.sh scripts part OK, scripts/upstream-changes.py --check OK. Not run: cargo nextest --workspace (DoD #1).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Guest e2e runs sealed only. test-e2e.sh makes a run-local LocalKms (KEKs for tenants 0-15, tenant 0 included: it is the default tenant's and the in-process stores', doc-16 allows it) and exports FXDAX_KMS and FXDAX_E2E_SEALED to every process of the run; builds predating encryption run clear, unchecked, said in encryption.txt. A keyless check (test_cases::sealed over the new extent_store::on_disk) fails a passed case whose stores aren't sealed (cells without KEKs, written branches' records without DEKs, clear frames or logs, or the canary the cases now write found in any file or object), and runner sealed checks the run's bucket and part cache at its end. Verified on ubu1: --tag e2e 16/16 pass sealed with 0 findings, guest checks within budget (times in notes); a negative control with no KEKs fails with 21 findings; S3 via RustFS passes. qualify/sandboxes/bench/nightly run encrypted through test-e2e.sh, but sandboxes fails its writes budget sealed (1.87-1.90x vs 1.5; clear 1.16x): TASK-411.23. DoD #1 (workspace nextest) not run.
<!-- SECTION:FINAL_SUMMARY:END -->
