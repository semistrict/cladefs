---
id: TASK-444.6
title: >-
  Variant hunts and static rules from every fixed bug: Coccinelle for C, dylint
  for Rust
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:33'
updated_date: '2026-11-16 03:04'
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
Big Sleep first real bug came from variant analysis of recent fixes; KNighter (SOSP 2025) turned kernel fix commits into static checkers that found 92 new bugs. cladefs puts past bugs back as mutants (guest/mutants, guest/sim/mutants.sh) but has one Coccinelle rule (scripts/cocci/region-lock.cocci). A static rule applies a lesson at every call site for good, where a seed covers one path.
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
C half (890dafe16, af4de78a4; branch land-444-6, gates pending):

Approach: each guest mutant (guest/mutants, 58) is a past bug put back. A rule in scripts/cocci names the mutants of its kind (// Mutants:), and scripts/cocci-guest.py --mutants fails unless it matches each in a copy of the guest's C, and unless every mutant has a rule or a why in scripts/cocci/unruled. That makes AC#1 and AC#2 a check, not a one-off.

Rules (12 mutants): dropped-error (164.4 commit failure), unwoken (149.18 x2), listing-dropped (230 x4), lock-not-held (44.9, 164.4 writeback-unlocked, swap-shared, rebind-shared), guarded-fields (44.19), error-logged-and-cleared (164.9). The other 46 are values, conditions or choices of their algorithm (each why in unruled), the kernel's fs/dax.c, or guest/sim's model.

Variant hunt at HEAD (AC#3): dropped-error found 2 results dropped that can't fail (cast to void). unwoken: 2 writes making a wait's condition false (argued). lock-not-held: 11 at first; fxfs_dax_map_all's comment-only contract now asserted; the rest are locks held by an external caller (VFS, the follower's Rust), a callee returning holding the lock (fxfs_dax_get_range), or for(;;) loops breaking with the lock held, which Coccinelle takes as possibly not run: argued in scripts/cocci/safe, by rule, file, function and line; an entry matching nothing fails the check. guarded-fields: put_region's last-reference read (argued). Writing lock-not-held turned five comment-only lock contracts into lockdep assertions (map_all, rebind_range's wb_lock, release, range_dirty, dirty_ranges, write_pages' wb_lock) and the dirty-set swap into fxfs_dax_range_swap, asserting the semaphore exclusive.

Log-and-continue sites (the kind 164.9 was), surveyed by hand: the budget worker's (rebinding, sealing, writing back: no caller, pages stay dirty and the next round retries), evict's writeback (can't fail upward; given up on as an I/O error's are, by design, TASK-207), resize work, the follower's invalidations and freeze/thaw on volume leave (void callbacks). None filed.

Structure: the class 'a lock contract only in a comment' is now made checkable: a function says its lock with an assertion, and the rule checks every caller.

Coccinelle gotchas (Homebrew spatch 1.3.3): =~ regexes match nothing, use {a, b} sets; <+... ...+> misses nested calls in a rule with no *; a header's prototypes bind only if the .h goes to spatch with the .c files.

Tests: scripts/tests/test_cocci_guest.py (the guest's C passes, every rule finds its mutants, the safe and unruled lists' semantics).

Left: the Rust half (AC#1 for the host crates' 74 mutants and loom's 10; clippy.toml's disallowed-methods is how cladefs already turns bugs into rules, TASK-98; dylint where that can't express one); CI wiring is via the clippy scripts part, which runs scripts/tests with Coccinelle installed (AC#4).

C half landed on main (244b04ef3). CI on land-444-6 green (43760916383, guest-sim full). DAX gate on fb95ffd75: guest/sim's mutants all killed; the dax-gate suites 5 rounds each passed; pgbench-dax-forked passed (window MAP 20 us, write(2) 49 us); the 11 real-kernel mutants all killed (task-79.49 equivalent). The driver stopped at the agent's 2 h limit in the mutants step; the second mutant's run was resumed and the rest run by guest-mutants-host.py on that commit. On the way: TASK-149.47 (patch 0010's mutant no longer killed on main, a scenario written), and the guide's anchor and task-164.2's mutant moved with fxfs_dax_range_swap. Left: the Rust half.
<!-- SECTION:NOTES:END -->
