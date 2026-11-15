---
id: TASK-149.46
title: >-
  scripts/tests/test_review_change.py fails outside a git checkout: 3 tests exit
  128 on ubu1's run tree
status: To Do
assignee: []
created_date: '2026-11-15 15:22'
updated_date: '2026-11-15 15:24'
labels: []
dependencies: []
parent_task_id: TASK-149
priority: medium
type: bug
ordinal: 870000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen by TASK-444.2's ubu1 run t4442c (2026-11-15, revision 35128bd67, main's 9da162ff0 plus TASK-444.2): scripts/test-host.sh's tree on the host is no git repository, and test_review_change.py (TASK-444.9, e86fc21a1) runs review-change.sh against the repository: test_a_base_names_what_head_brings, test_codex_reviews_a_commit_read_only_with_the_agents_instructions, test_no_change_named_fails each got exit 128 ('fatal: not a git repository ... Stopping at filesystem boundary'). They pass on a Mac checkout and in CI (a clone). So the scripts part fails every ubu1 qualification run.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The three tests pass in scripts/test-host.sh's tree on ubu1 (each makes the repository it needs, as other script tests make their temporary trees), or skip there saying why with check-exclusions' rules
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fix in 31c9ad944: review-change.sh took its root from git rev-parse --show-toplevel; it now takes it from its own path. Verified on the Mac: the three tests on a copy of scripts/ and .claude/ outside any repository fail with the old script (3 failures) and pass with the new; they pass in the checkout too. AC #1 waits on the next ubu1 run of the scripts part. Blind spot: scripts tests run only in git checkouts (Mac, CI clones); ubu1 run trees are the one place that is not. Looked around: the other scripts using rev-parse --show-toplevel (rename-to-cladefs.sh, build-commit.sh) passed on ubu1 in t4442c.
<!-- SECTION:NOTES:END -->
