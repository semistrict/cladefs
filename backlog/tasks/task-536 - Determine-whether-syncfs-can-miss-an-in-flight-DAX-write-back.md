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
- [ ] #1 Schedule the two-file omission, trace the prior VFS write-back stage, and establish with a guest test whether syncfs can return before the older write-back commits. Record the outcome even if VFS ordering prevents the durability failure.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
