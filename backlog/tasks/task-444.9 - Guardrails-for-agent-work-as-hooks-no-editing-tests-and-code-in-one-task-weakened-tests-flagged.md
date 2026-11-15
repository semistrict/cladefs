---
id: TASK-444.9
title: >-
  Guardrails for agent work as hooks: no editing tests and code in one task,
  weakened tests flagged
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 12:34'
updated_date: '2026-11-15 14:02'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
ordinal: 858000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ImpossibleBench (ICLR 2026): frontier models cheat on contradictory tests, stronger ones more; Anthropic guidance: enforce rules with hooks, not instruction lines. AGENTS.md forbids tests written around bugs but nothing enforces it. Dan Luu: reviewer agents that edit code made results worse; keep them report-only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A hook denies implementer agents edits to tests, specs and mutation configs unless the task is a test-writing one
- [x] #2 A check flags diffs that remove assertions, add #[ignore], loosen bounds or narrow generators, run before commit
- [x] #3 A read-only, report-only review step verifies each finding before reporting, with a second vendor on load-bearing changes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/test-weakening.py: flag commits that weaken checks unless the message has a Weakens-tests: line; commit-msg hook and a CI job. 2. scripts/agent-role-guard.py: a PreToolUse hook holding a session whose .agent-role says implement or test to its side. 3. .claude/agents/verifying-reviewer.md and scripts/review-change.sh (Codex, same instructions). 4. AGENTS.md: when to use each. 5. Prove each: unit tests, the checker over main history, a real claude -p session refused by the hook, both reviewers on a real commit.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Weakening check: 9 rules, patterns scoped by language (a first version flagged Rust patterns inside Python strings, its own commit). Rewrites that restate checks are not flagged: assertions count net per commit, moved lines are moves, a changed expected value is not a loosened bound, spec configs flag only invariants/properties removed and constants lowered, counts only in code/scripts/workflows. Over main 1,500 commits: 74 flagged (4.9%), e.g. TASK-407 dropping fencing invariants and mutants, TASK-307 ignoring 19 root tests, TASK-344 removing the GCE drivers tests; each would have needed a one-line reason. First version flagged 16%; tightened as above.

Role guard: AGENTS.md asks agents to write tests with their change, so the guard is opt-in per session (.agent-role, gitignored), for split work (a reproducer by one agent, the fix by another; best-of-N). End to end: claude -p (Haiku) in a scratch worktree with .agent-role=implement, asked to delete an assertion from a test file: the Edit was refused by the hook; asked to use sed -i and rm .agent-role: both refused. It is a guard against slips, not an adversary (shell commands judged by the paths they name).

Reviews of 0ef6fa1b4 (TASK-433.2/433.4), both report-only: the verifying-reviewer agent (claude -p --agent) found per-record O_DIRECT writes and ftruncates per log flush (filed TASK-433.7, measure first); Codex via review-change.sh found a silent fallback to buffered I/O where O_DIRECT is refused, against AGENTS.md fail-hard rule (filed TASK-433.6, confirmed in direct.rs/dir.rs/cache.rs). Disjoint findings, as doc-17 predicted for a second vendor. codex review takes no custom prompt with --commit, so the script uses codex exec --sandbox read-only.

Hooks via core.hooksPath are not installed in this checkout or worktrees (git config core.hooksPath empty), so CI is the enforcement; the commit-msg hook is for clones that opt in.

DoD: no Rust or upstream code changed (cargo nextest not rerun for that reason); scripts suite 347+ tests OK on macOS after merging main; upstream-changes.py --check passes; license-headers.py passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Agent guardrails: scripts/test-weakening.py flags commits that weaken checks (assertions, ignored/removed tests, loosened bounds, lowered counts, narrowed generators, known failures, invariants, mutants) unless the message says why in Weakens-tests:, run by a commit-msg hook and a CI job on every push and PR (4.9% of 1,500 past commits flagged, all real weakenings or redesigns). scripts/agent-role-guard.py, a PreToolUse hook in .claude/settings.json, holds a session with .agent-role implement or test to its side; verified refusing Edit, sed -i and rm .agent-role in a real claude -p session. .claude/agents/verifying-reviewer.md and scripts/review-change.sh (Codex, read-only, same instructions) give load-bearing changes two report-only reviews; on 0ef6fa1b4 they found two different issues, filed as TASK-433.6 and TASK-433.7. AGENTS.md says when to use each. Tests: test_test_weakening (17), test_agent_role_guard (10), test_review_change (3).
<!-- SECTION:FINAL_SUMMARY:END -->
