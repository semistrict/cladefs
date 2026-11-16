---
id: TASK-496
title: >-
  zx's lib tests fail to compile on Linux under cargo check --workspace
  --all-targets (88 errors); CI's clippy --workspace --tests passes
status: To Do
assignee: []
created_date: '2026-11-16 12:07'
labels:
  - build
  - linux
dependencies: []
priority: low
ordinal: 886000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 2026-11-16 (TASK-475 work, at 1270ab9fe's successor): `cargo check --workspace --all-targets` fails with 'could not compile `zx` (lib test) due to 88 previous errors; 5 warnings emitted'. The same commit passes scripts/test.sh's clippy part (`cargo clippy --workspace --tests`, which also builds zx's lib test) on Linux, and `cargo check -p zx --tests` passes on macOS. Likely a feature or target difference that --all-targets brings in (benches/examples unifying features, or a Linux-only cfg in zx's tests). The errors themselves were not captured; rerun under fxjob with the full output kept.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the 88 errors is known and written in the task
- [ ] #2 cargo check --workspace --all-targets passes on Linux, or the task records why that build is not supported and docs say so
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
