---
id: TASK-664
title: >-
  An unshared cross-tenant restore does source I/O and host mutation before
  EACCES
status: To Do
assignee: []
created_date: '2026-11-19 17:28'
labels:
  - security
dependencies: []
priority: medium
type: bug
ordinal: 1048000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `crates/cladefs/src/cladefsd/cells.rs:780-849`; `crates/bucket_cell/src/lifecycle.rs:329-341`.

`opened_version` can sync the victim's live writers, fetch its head, create a version directory and recover/keep a source store before `finish_fork` applies the shared-version authorization rule. A request that ends EACCES can thus cause cross-tenant I/O, a metadata/timing oracle and host disk side effects, contrary to the integrator guide's "making nothing". It does not itself return plaintext and blind version guessing is constrained by random version IDs.

**Probe, confirmed on ubu1:** `grants::an_unshared_restore_has_no_effect_before_its_refusal` kept the victim writer live with an uncommitted extent and attempted OPEN_FORK from tenant 2. The request returned EACCES but still created and recovered a version store under `cladefs-versions-<pid>/1.<cell>.<version>`. No victim commit-number change was observed in this setup, so the dynamic evidence is unauthorized recovery/host mutation, not a claimed sync. **Structure:** authorize the immutable version record before source recovery or host mutation, without reintroducing key material into the refused path (TASK-544).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Authorize before source I/O or mutation and test a live victim writer and shared-version control
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 cargo nextest run --workspace passes on macOS and Linux (ubu1)
<!-- DOD:END -->
