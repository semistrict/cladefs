---
id: TASK-444.6
title: >-
  Variant hunts and static rules from every fixed bug: Coccinelle for C, dylint
  for Rust
status: To Do
assignee: []
created_date: '2026-11-15 12:33'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
ordinal: 855000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Big Sleep first real bug came from variant analysis of recent fixes; KNighter (SOSP 2025) turned kernel fix commits into static checkers that found 92 new bugs. fxdax puts past bugs back as mutants (guest/mutants, guest/sim/mutants.sh) but has one Coccinelle rule (scripts/cocci/region-lock.cocci). A static rule applies a lesson at every call site for good, where a seed covers one path.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each Done bug task in the guest C and host Rust has a recorded pattern and a variant search at HEAD
- [ ] #2 Each rule flags the pre-fix tree and is silent on the fixed one
- [ ] #3 Hits elsewhere are each fixed, filed, or argued safe
- [ ] #4 Rules run in CI (Coccinelle context mode, per the Homebrew constraint; dylint for Rust)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
