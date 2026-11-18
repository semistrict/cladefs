---
id: TASK-589
title: A follower open after an unlink's batch holds nothing; a long purge zeroes it
status: To Do
assignee: []
created_date: '2026-11-18 15:30'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 980000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), followers review; verified against the code by independent reviewers. Severity: medium.

**Where:** `guest/rust/cladefs-linux/src/fs.rs`: `rg -n -F 'if let Err(error) = self.attr(ino).await {' guest/rust/cladefs-linux/src/fs.rs`

GuestFxfs::hold says that if a batch deleted a file after the open looked it up, the hold fails with NotFound (lib.rs cladefs_hold: "-ENOENT if a batch deleted it"). Its only check is `self.attr(ino)`. Fxfs keeps the object record of a file in its graveyard with refs=1 (`adjust_refs` does not commit refs=0, and fxfs-posix `in_graveyard` says "Its record counts one link meanwhile"), so `attr_of` returns Ok for a file that was unlinked but not yet purged. The hold goes through as `Held { opens: 1, kept: None }`. The GraveyardEntry(Some) that would have started keeping it was applied before the hold existed. fxfs follower.rs `keep_deleted` then fires only on the final Object tombstone. A purge that takes more than one transaction runs `trim_or_tombstone`/`trim_some` with TrimResult::Incomplete (more extents than TRANSACTION_ROOM holds), or with Done(Some(next attribute)) when the file has a non-DATA attribute such as a large xattr. In both cases the earlier transactions carry only deleted-extent merges and allocated_size changes for the object, with no graveyard or tombstone mutation. They apply to the held object, their ranges are dropped from the kernel's caches, and the previous commit is let go of. By the time the final tombstone keeps the object, its records have no extents left to pin. The open file then reads holes, silently returning zeros, which breaks doc-8 decision 6 ("stays readable until the follower closes it"). A related symptom: `attr_applied` settles a graveyard object as nlink=1, not gone. Its inode therefore stays in the icache, and `cladefs_vfs_fh_to_dentry`'s ilookup hit returns it with no graveyard check (unlike `attr_by_handle`). open_by_handle_at then reaches the same hold with no race at all.

**Trigger.** Step 1. The writer VM opens /f, a file of several hundred separately written extents (or any size with a large xattr), and unlinks it while it is still open. /f goes into the writer's graveyard. Step 2. A follower reader runs open("/f"). The path walk's lookup (or a dcache hit) resolves the dentry and releases the fill barrier. The batch carrying the unlink then runs: GraveyardEntry Some, no follower hold yet, so nothing is kept, and the inode is settled with refs=1. Only then does `cladefs_vfs_file_open` take the barrier and call `cladefs_hold`. `attr(ino)` succeeds and the open returns 0. Alternative step 2: a follower process that saved a handle with name_to_handle_at calls open_by_handle_at after the unlink's batch. The inode is still cached, so fh_to_dentry skips `attr_by_handle`'s graveyard check. Step 3. The writer closes /f, and `tombstone_object` purges it over several transactions. Step 4. The follower's batches apply the trim transactions to the held object and let go of the commits, so the extents leave the view. Step 5. The reader's read(2) or DAX loads of /f return zeros for every trimmed range. There is no error.

**Why the tests stay green.** follower_sim's holders take the hold inside the same fill as the lookup (TASK-79.47), so a hold always exists before the unlink's batch. graveyard.rs's cases, including `held_files_outlive_purges_over_several_transactions`, hold before the unlink. The guest torture test holds files before deleting them. No test opens, or opens by handle, a file whose unlink a batch has already applied.

**Backlog.** Regresses TASK-42.10 (Done), which promised that an open of a file a batch deleted since its lookup fails ENOENT instead of holding nothing. Its fix covers only tombstoned objects, not files moved to the Fxfs graveyard. TASK-42.4 (Done) designed the follower's graveyard.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 On a follower, opening a file whose unlink a batch has already applied (it sits in the writer's Fxfs graveyard) fails with ENOENT, as it does for a tombstoned file. This holds for open after lookup and for open_by_handle_at with the inode still cached.
- [ ] #2 No reader on a follower ever reads zeros for a range the writer's purge trimmed over several transactions (a file of many extents, or one with a large xattr).
- [ ] #3 A test in crates/follower_sim/src/graveyard.rs unlinks a file of MANY blocks through the Fxfs graveyard and lets the follower follow. Only then does it call hold. It purges with tombstone_object, follows twice, and asserts the hold was refused with NotFound. With the fix reverted, the hold succeeds and the file reads fill 0.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
