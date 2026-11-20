---
id: TASK-674
title: >-
  License header missing: guest/tests/audit/kernel-security-probes-8.patch
  (9f4c381d8, security review); test (clippy scripts) red on main
status: Done
assignee:
  - '@claude'
created_date: '2026-11-19 22:27'
updated_date: '2026-11-20 00:01'
labels:
  - ci
  - bug
dependencies: []
priority: medium
ordinal: 1056000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
scripts/tests test_license_headers.LicenseHeadersTest.test_the_tree_carries_its_headers fails on main since 57321d675 (run 44597568780; still on 28a38a315, run 44615661631): guest/tests/audit/kernel-security-probes-8.patch has 'no copyright of The cladefs Authors' and 'no BSD-style license' (AssertionError lists that file, maybe others truncated at [96 chars]). Added by 9f4c381d8 'security: review kernel and sandbox boundaries'. Either give the patch its header (as other patches in the tree carry it) or exempt it in scripts/license-headers.py as the probe's upstream-format patch, whichever that script's rules say.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 scripts/license-headers.py passes on the tree
- [x] #2 CI's test (clippy scripts) passes on main
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Header added before the diff (git apply ignores it): copyright, BSD notice, a line naming the fixture; git apply --check passes on HEAD; scripts/license-headers.py passes.

CI run 44629552079 (e7fbf3abb): its job passes. Workspace: macOS 2754/2754; Linux (ubu1, fxjob, with TASK-644/675's changes too) 3096/3097, the one failing mappings_asked_for_past_the_credit_let_their_vm_process_go, TASK-546's, failing on main before. DoD #1 left unchecked for that. scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
kernel-security-probes-8.patch carries the tree's license header before its diff, which git apply ignores. Verified: scripts/license-headers.py, git apply --check, and main's test (clippy scripts) CI job (run 44629552079).
<!-- SECTION:FINAL_SUMMARY:END -->
