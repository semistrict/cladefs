---
id: TASK-619
title: Every OPENDIR on the host mount reads and copies the whole directory
status: To Do
assignee: []
created_date: '2026-11-18 15:34'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1010000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fuse-sim review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/fuse/server.rs`: `rg -n -F 'let mut entries = dir.entries().await?;' crates/cladefs/src/fuse/server.rs`

open_dir calls Node::entries(), which runs fxfs.readdir from index 0 to the end inside one volume.run closure. It pushes a DirEntry with an owned name for every entry, and the server keeps that Vec in the handle's OpenDir until RELEASEDIR. The cost is proportional to the directory's size on every opendir, even when the caller reads one entry (ls | head, fts opening a directory to descend). Each open handle holds its own copy. In cladefsd, which serves this mount of tenants' cells (cladefsd/mount.rs), this memory is not counted by the governor, against AGENTS.md's one-budget rule. The listing also runs as one task on the filesystem's Fxfs executor, which has one thread by default (DEFAULT_EXECUTOR_THREADS). Other operations on that cell, other FUSE requests included, queue behind it for the whole walk.

**Trigger.** A tenant's Fxfs cell holds a directory of 5,000,000 files (a cache or a mail spool), attached to the daemon's mount at TENANT/CELL. A host process runs `ls TENANT/CELL/dir | head -1` or `find TENANT/CELL`: OPENDIR, then open_dir reads all 5M names into one Vec of about 5M x (24 bytes + name + allocation) = hundreds of MB, held until RELEASEDIR. The Fxfs executor walks the whole tree before the reply. Eight concurrent openers (the mount's 8 threads) hold eight copies, with no relief and no governor accounting.

**Why the tests stay green.** The FUSE tests (fuse.rs, fuzz_tests.rs, cells_tests.rs, fuse_differential.rs) use directories of a handful of names on 8-16 MiB stores. None measures what OPENDIR reads or allocates, and no budget test covers the mount.

**Backlog.** New. TASK-5 (Done) chose to take a directory's entries at opendir for correctness and TASK-17 (Done) serves FUSE in the daemon; neither records the cost on large directories.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 OPENDIR then one 4096-byte READDIR of a 100,000-entry directory reads no more entries from Fxfs than one reply's worth plus a bounded read-ahead.
- [ ] #2 Memory held per open directory handle does not grow with the directory's size, and each entry is still listed once however the directory changes.
- [ ] #3 A FUSE server test counting Node::entries or Fxfs readdir emits reproduces the 100,000-entry case and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
