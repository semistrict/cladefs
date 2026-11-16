---
id: TASK-459
title: >-
  rename-to-cladefs.sh --backlog garbled the docs' titles: YAML quoting doubled
  ('''TASK-26.1 ...'''), doc-17's folded title became '>-' and its file 'doc-17
  - untitled.md'; CI's guide job fails on it
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 13:39'
updated_date: '2026-11-16 02:39'
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
- [x] #1 Every doc's title is its old title with fxdax renamed (git show ab1adf06b^ for the old ones), unquoted in value, and doc-17's file named from its title
- [x] #2 guide's pnpm test passes; CI's guide job green on main
- [x] #3 rename-to-cladefs.sh --backlog takes titles' YAML values (quoted, folded), with a test of each form
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Check main: the rename's session (TASK-437) pushed 1d8c55acb (titles from doc list --plain) and d8741092a (docs' titles restored, doc-17's file renamed) while this ran; my own CLI repair matched theirs byte for byte but for dates, so dropped.
2. The script reads docs' titles from backlog search --type document --json (versioned fields, as the backlog's guide says for scripts) instead of the plain listing, and sets a title or body only if the rename changes it.
3. scripts/tests/test_rename_to_cladefs.py: the script in a repo of its own against a fake backlog, a quoted unrenamed title (doc-3's case), a folded one (doc-17's), a quoted renamed one, one untouched; fails on the script as ab1adf06b ran it.
4. End to end: the real script and CLI on a copy of the backlog before ab1adf06b; guide build, test, check.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Main had moved by the time this ran: the rename's session pushed 1d8c55acb (the script's titles from doc list --plain, only if renamed) and d8741092a (the nine docs' titles restored, doc-17's file named from its title). My CLI repair (backlog doc update --title from ab1adf06b^'s titles through rename-to-cladefs.sh --filter) matched theirs but for updated_date, so I dropped it. Kept from this task: the script reads the docs from backlog search --type document --json (the backlog's versioned output for scripts) instead of the plain listing, and scripts/tests/test_rename_to_cladefs.py (fake backlog; quoted unrenamed, folded, quoted renamed, untouched), which fails on the script as ab1adf06b ran it. End to end: the script with the real CLI on a git archive of backlog/ at ab1adf06b^ left every doc's title its value renamed (doc-17: 'Spending frontier tokens on quality: what the forefront does, and what fits cladefs', its file named from it). Open tasks' titles: none garbled (task list --json; the task path always used JSON values). guide: pnpm install, build, test (42 of 42), check pass. Scripts' tests 320 of 320 pass on the Mac. scripts/upstream-changes.py --check passes.

CI on main, run 43725423025 (9da162ff0), at 15:17 UTC: test (workspace, clippy scripts, sim, shuttle, guest, firecracker, go), guide, guest-sim, guest-sim-sweep, soak, object-stores, pjdfstest-fuse, sandboxes (short), firecracker-forward, firecracker-series, test-weakening and macos (shuttle) green; macos (workspace) still running, unread (the API's limit spent by every session till 15:51 UTC). Run 43724391112 (1cff819be) was cancelled by the next push, its clippy scripts failing only on TASK-461's two patches. DoD #1 waits on macos (workspace). guide green on both runs (1cff819be, 9da162ff0).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Fixed earlier on main; the workspace passes on macOS and Linux in main's CI (cd8d9a286, run 43765433746, and 6710dd2d2, run 43790612196).
<!-- SECTION:FINAL_SUMMARY:END -->
