---
id: TASK-536
title: Determine whether syncfs can miss an in-flight DAX write-back
status: To Do
assignee: []
created_date: '2026-11-18 12:26'
labels: []
dependencies: []
priority: medium
type: spike
ordinal: 926000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 21.

The two passes in `guest/linux/fs/cladefs/dax.c:2550` count and collect busy files under the membership lock. Dirty and private counters can change between passes. A lower-numbered inode becoming busy can take the last array slot, displacing an inode that was busy throughout. The sync path at line 3980 waits for write-back only on the collected files. Check whether this lets syncfs return before an older write-back commits by pausing that write-back, introducing the lower-numbered inode between passes, and checking the durability boundary. The collection omission follows from the code. Linux sync also runs inode write-back before its final filesystem sync. That can already wait for a VFS-dirty inode, and the simulator calling settle directly does not exercise that protection. Whether an omitted inode can still have outstanding writes at the final seal remains unverified.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Schedule the two-file omission, trace the prior VFS write-back stage, and establish with a guest test whether syncfs can return before the older write-back commits. Record the outcome even if VFS ordering prevents the durability failure.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation notes

The baseline ubu1 log under `/tmp/cladefs-all.FmYymB/ubu1/e2e/fxfs-guest-checks-dax` records two distinct observations. In `busy_file_collection`, the test dirtied a lower-numbered inode between counting and selection. The older inode was busy before the pause and absent from the returned references. In `syncfs_older_writeback`, the syncfs thread waited in `writeback_inodes_sb` while the VFS worker waited in `cladefs_dax_write_back_keeping`. Final collection had not started. This interleaving establishes a collector omission and shows no early syncfs return.

Existing durability tests lacked both a forced change of busy membership and an independent observation of the returned references. The instrumented guest checks keep those observations separate from the capacity calculation. The simulator's direct settle call also skips the VFS stage.

Count all file entries while holding files_lock across both walks. That count bounds the number of references the second walk can acquire regardless of changes to either page counter. Membership in the volumes and their file tables is stable under this mutex. The scratch allocation costs one pointer per tracked file, including clean files. It uses the existing no-filesystem-reclaim scope and fails before taking references. Settle propagates ENOMEM and relief can try again on its next round. The collection observes each file when visited and has no single snapshot instant for concurrently dirtied files.

Both settle and relief share the collector. The other use of the same nested file iterator collects statistics in one pass and has no counted output array.
