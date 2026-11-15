---
id: TASK-444.4
title: >-
  unsafe audit of the host crates: two independent agents, confirmed by tests or
  Miri
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
ordinal: 853000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The host crates hold ~557 unsafe sites (2026-11-15) and Miri runs on none. The first Rust-in-Linux CVE (CVE-2025-68260, Binder) was a SAFETY claim false under concurrency. SafeDep asks whether a sibling method can break an unsafe precondition; std-unsafe-doc has one model write preconditions and an independent one label each claim Correct, Missing or Incorrect.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every unsafe block in the host crates has written preconditions and a SAFETY argument covering other threads touching the same memory
- [ ] #2 A second, independent agent labelled each claim; disagreements resolved in the notes
- [ ] #3 Each finding is confirmed by a failing test, Shuttle schedule or Miri run before it is filed
- [ ] #4 Miri runs in CI on the tests that can run under it
- [ ] #5 clippy undocumented_unsafe_blocks is denied in the host crates
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
