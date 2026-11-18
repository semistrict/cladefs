---
id: TASK-583
title: A file set's commit holds three copies of its dirty data at once
status: To Do
assignee: []
created_date: '2026-11-18 15:29'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 974000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), file-sets review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/extent_files/src/pages.rs`: `rg -n -F 'let mut written: BTreeMap<u64, Option<Box<[u8]>>> = self.dirty.clone();' crates/extent_files/src/pages.rs`

Pages::plan deep-clones every dirty chunk (`self.dirty.clone()` clones each Box<[u8]>), then copies them all into `data` while `written` is still alive. Plan::write then builds `whole`, a Vec of every extent's bytes (`self.data[..].to_vec()` plus owners), before writing the first piece. `dirty` survives until apply and `data` until the plan drops. So peak memory during a commit is about 3x its dirty bytes, all of it avoidable, since plan could borrow dirty and write could send pieces straight from `data`.

**Trigger.** MemoryCell::import (the documented way to turn a Firecracker memory file into a memory cell; the guest snapshot case uses it) writes every non-zero page into `dirty` and commits once. A 32 GiB VM with 16 GiB of non-zero pages: plan clones 16 GiB and builds 16 GiB of `data` (48 GiB peak), and Plan::write's `whole` reaches 48 GiB again. The importing process needs about 3x the image's data in RAM. A SQLite transaction that bulk-loads 1 GiB peaks at about 3 GiB in the same way.

**Why the tests stay green.** Commits in tests are KiBs to a few MiB (the memory-cell tests use 256-400 pages, the sqlite tests small tables), and nothing measures allocation during sync.

**Backlog.** New. TASK-532 (To Do) is the same plan function's allocation per removed block on a shrink; TASK-456.2 and TASK-456.3 are memory cells' design. None covers these copies.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 FileSet::sync of a file with 64 MiB dirty allocates at peak under about 1.1x the dirty bytes beyond the dirty data itself (about 2x now), measured with a counting allocator.
- [ ] #2 MemoryCell::import of an image with D bytes of non-zero pages peaks under about 2.1x D in the importing process.
- [ ] #3 A counting-allocator test in extent_files around FileSet::sync holds the bound and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
