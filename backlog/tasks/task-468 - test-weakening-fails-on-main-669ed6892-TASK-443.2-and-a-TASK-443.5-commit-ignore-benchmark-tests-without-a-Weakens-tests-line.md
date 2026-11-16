---
id: TASK-468
title: >-
  test-weakening fails on main: 669ed6892 (TASK-443.2) and a TASK-443.5 commit
  #[ignore] benchmark tests without a Weakens-tests: line
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 20:58'
updated_date: '2026-11-15 22:12'
labels:
  - bug
  - ci
dependencies: []
priority: high
ordinal: 875000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43758893191 (a252cb53f): job 'Commits that weaken checks say why' fails: 669ed6892 adds #[ignore = "a benchmark (TASK-443.2): --run-ignored only, in the sim profile"] at crates/cladefs/src/remote.rs:1083, and another commit #[ignore = "a benchmark (TASK-443.5): --run-ignored only, alone"] at crates/cladefs/src/remote/lifecycle.rs:540, neither message saying why in a 'Weakens-tests:' line (AGENTS.md). History on main can't be rewritten: find how the check accepts a reason after the fact (an allowlist or a later commit's line) and record that these are benchmarks, not tests around a bug; or move the benchmarks where the check doesn't count them.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. The check runs on each push's range (BEFORE..AFTER): no after-the-fact reason exists, nor is one needed for main's next push, whose range lacks these commits. The flag is a false positive: a test the commit adds, ignored (benchmark) or should_panic, weakens nothing checked before.
2. scripts/test-weakening.py: test-ignored skips an ignoring line on a test the commit adds (its function, of a name new to its file and to the commit's removed lines, added in the same run with its test attribute); moved ignore lines aren't flagged, as the docstring says of all rules. Benchmarks stay where they are (they use the crates' private test helpers).
3. scripts/tests/test_test_weakening.py: added tests not flagged; an existing one ignored beside an added one, renamed, or moved with another reason flagged; moved as it is not.
4. Show the job's script passes for main's range after the push.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause: test-ignored flagged every added #[ignore]/#[should_panic]/skip line, even on a test the same commit adds; the check runs on each push's range, so main's push bringing 669ed6892 and c563138a6 failed. No after-the-fact reason mechanism exists, and none is needed for history: later pushes' ranges lack these commits. But every new benchmark or panic test would fail again (880a20f59, 85b163d62, f8dbd42c2, 7abc39092, 78b94e218 were flagged the same way). Moving benchmarks out (an examples/bench target) wasn't the fix: they use the crates' private test helpers, and should_panic tests are real tests that can't move.
Fix (5f84230ea): an ignoring line on a test the commit adds isn't flagged: the function it applies to (after an attribute or decorator, else the one it is in), of a name its file didn't define before nor any removed line of the commit, in one added run with the ignoring line and its test attribute. Ignore lines moved as they are aren't flagged either, as the docstring already said of every rule.
Evidence: scripts/tests/test_test_weakening.py, four new tests (added ignored, should_panic, Python skipTest: not flagged; existing test ignored beside an added one, renamed and ignored, moved with another reason: flagged; ignored test moved as it is: not), failed before the fix and pass after (21/21). Over main's last 400 commits the new script drops exactly the flags of those seven commits; 815409fb8 (the rename, test names renamed) is still flagged and is history. scripts/test-weakening.py a252cb53f..origin/main and over this push's commits: exit 0. Mac scripts part passes.

Commit hash: the fix is 69cf424d9 (5f84230ea above was its pre-amend hash, never pushed). Pushed to main as b66fc7394..464e6e201; CI run 43762655434 queued at the time of writing: DoD #1 (workspace on macOS and Linux) left to it, as these commits change no workspace code.

2026-11-15, coordinator: main's CI run 43765433746 on cd8d9a286 green on every job (test (workspace), macos (workspace), guest, clippy scripts, test-weakening, sandboxes among them).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
test-weakening.py no longer flags an ignore/should_panic/skip on a test the commit adds, nor an ignore line moved as it is (69cf424d9); existing tests ignored (also beside an added test, renamed, or moved with another reason) stay flagged. Four new script tests; over main's last 400 commits only the seven new-test commits' flags drop. History needs no reason: later pushes' ranges lack 669ed6892/c563138a6. DoD #1 awaits main's CI run 43762655434.
<!-- SECTION:FINAL_SUMMARY:END -->
