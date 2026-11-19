---
id: TASK-660
title: >-
  A malicious WRITE of extent sequence u64::MAX overflows next_sequence and
  breaks write-once
status: To Do
assignee: []
created_date: '2026-11-19 17:26'
labels:
  - security
dependencies: []
priority: high
type: bug
ordinal: 1044000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review of e0bfaeaed.

**Where:** `crates/local_cell/src/cell.rs:1603-1618,1923,1064`; `crates/extent_frame/src/spent.rs:34-41`.

`begin_write` bounds I/O but not the sequence. Finishing a write computes `extent.sequence + 1` while holding the shared store-state lock. In checked builds that panics; the production shared-lock panic policy aborts cladefsd, taking every tenant down. In release it wraps `next_sequence` to zero. A freed u64::MAX extent is also omitted by Spent's wrapping range, so it can be written again with different bytes, aliasing old and new contents in cache and shared versions.

**Reproduced:** `LocalCell::write(ExtentId::new(1, u64::MAX), 0, &[0x5a;4096])`. The checked-build probe overflowed at cell.rs:1923, poisoned the shared lock and SIGABRTed. The standalone release probe printed `first write: Ok(())`, `next_sequence: 0`, `rewrite after free: Ok(())`, and `read back: a5` after writing 0x5a first. Thus both failure modes are demonstrated, not inferred.

**Blind spot:** hostile generators use small or random sequences but do not target u64::MAX and assert neither a clean EINVAL nor write-once after free. **Structure:** sequence validity belongs at the input boundary; no post-write arithmetic on an untrusted sequence should be able to overflow. Use checked addition there too and make Spent reject, rather than silently omit, an unrepresentable successor.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Reject u64::MAX before writing, with checked and release regression tests covering commit, free, reopen and rewrite
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 cargo nextest run --workspace passes on macOS and Linux (ubu1)
<!-- DOD:END -->
