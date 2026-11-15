---
id: TASK-459
title: >-
  rename-to-cladefs.sh --backlog garbled the docs' titles: YAML quoting doubled
  ('''TASK-26.1 ...'''), doc-17's folded title became '>-' and its file 'doc-17
  - untitled.md'; CI's guide job fails on it
status: To Do
assignee: []
created_date: '2026-11-15 13:39'
updated_date: '2026-11-15 13:39'
labels:
  - backlog
  - ci
dependencies: []
references:
  - scripts/rename-to-cladefs.sh
  - guide/tests/evidence.test.ts
priority: high
type: bug
ordinal: 868000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ab1adf06b (TASK-437's 'scripts/rename-to-cladefs.sh --backlog') rewrote the docs' titles from their front matter's raw YAML text, not its value. A quoted title came back quoted again: doc-3 is now title: '''TASK-26.1 spike: porting Fxfs into the guest''''s Linux kernel''', and doc-2, 7, 10, 14, 15 and 16 are quoted the same way (doc-5 and doc-9, whose titles weren't renamed, are as they were). doc-17's folded title (title: >- over two lines) became title: '>-', and its file became 'doc-17 - untitled.md'. Main's CI run 43719878556 (1e74dbf41) fails its guide job on it: guide/tests/evidence.test.ts 'docs carry their titles unquoted' expected 'TASK-26.1 spike: porting Fxfs into th...', got a title starting with a quote. Found by TASK-330.1 checking main's CI after its push. The open tasks' titles that the same run renamed may have been garbled the same way; check them too.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every doc's title is its old title with fxdax renamed (git show ab1adf06b^ for the old ones), unquoted in value, and doc-17's file named from its title
- [ ] #2 guide's pnpm test passes; CI's guide job green on main
- [ ] #3 rename-to-cladefs.sh --backlog takes titles' YAML values (quoted, folded), with a test of each form
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
