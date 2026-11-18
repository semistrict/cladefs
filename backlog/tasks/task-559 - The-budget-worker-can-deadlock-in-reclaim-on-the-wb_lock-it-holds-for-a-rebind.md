---
id: TASK-559
title: The budget worker can deadlock in reclaim on the wb_lock it holds for a rebind
status: To Do
assignee: []
created_date: '2026-11-18 15:26'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 950000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-kernel-runtime review; verified against the code by independent reviewers. Severity: high.

**Where:** `guest/linux/drivers/virtio_extent/driver.c`: `rg -n -F 'err = sg_alloc_table(sgt, 1, GFP_KERNEL);' guest/linux/drivers/virtio_extent/driver.c`

cladefs avoids fs reclaim under its locks everywhere else. device.c allocates its requests and MAP_MANY entries with GFP_NOFS, and dax.c wraps its kvmallocs and Rust calls in cladefs_nofs. But virtio_extent_submit maps a request's data with vext_map_data, whose sg_alloc_table uses GFP_KERNEL. The budget worker (cladefs_dax_work, on dax->wq) has no memalloc_nofs scope. cladefs_dax_rebind_ranges takes file->wb_lock and down_write(&file->sem), and cladefs_dax_rebind_range then sends MAP_MANY (cladefs_dax_map_all -> cladefs_device_map_many -> virtio_extent_call), whose entries are request data. So a __GFP_FS allocation runs with both locks held. The worker holds only a cladefs_dax_file reference, not the inode, so the file's inode can sit unreferenced and clean on the superblock LRU. Direct reclaim's super_cache_scan -> prune_icache_sb can evict it, and cladefs_vfs_evict_inode -> cladefs_dax_evict -> cladefs_dax_write_back takes mutex_lock(&file->wb_lock) before checking for dirty pages (dax.c:2365). That is a self-deadlock. cladefs_dax_frozen's relieve rebinds the same way during freeze.

**Trigger.** A file is written with write(2) and closed, so its mapping has no DAX entries. The flusher's writepages and write_inode write back its dirty pages, but they stay private (unsealed or not yet rebound), and the clean, unreferenced inode goes onto the LRU. Private pages then pass 7/8 of private_max (or a freeze happens), and the worker's cladefs_dax_relieve -> cladefs_dax_rebind(file) -> cladefs_dax_rebind_ranges takes the file's wb_lock and sem. It seals if extents are open, then sends MAP_MANY. Under guest memory pressure, the kmalloc in sg_alloc_table enters direct reclaim. shrink_slab -> super_cache_scan (GFP_KERNEL has __GFP_FS) evicts that inode, and cladefs_dax_evict -> cladefs_dax_write_back -> mutex_lock(&file->wb_lock) waits forever on a lock its own task holds. Afterwards the budget never relieves again: writers throttle until -ENOMEM and faults SIGBUS. fsync, sync and unmount touching the file hang in D state.

**Why the tests stay green.** guest/sim replaces virtio_extent_call/submit entirely (kernel/device.c), so the driver's sg_alloc_table never runs there, and the sim has no reclaim. The guest checks do not combine memory pressure strong enough to shrink the inode cache with a budget rebind of a file whose inode is unreferenced. Lockdep's fs_reclaim tracking on the debug kernel needs both edges: wb_lock -> fs_reclaim happens on every such MAP_MANY, but fs_reclaim -> wb_lock needs kswapd or direct reclaim to evict a linked DAX inode, which the short debug suites apparently never do. TASK-292's fix covered only the TreeCache shard lock.

**Backlog.** New. TASK-292 (Done) fixed a reclaim deadlock on the TreeCache shard lock, not wb_lock; TASK-444.6 (Done) added static rules but none for NOFS on the device's submit path.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No allocation made while the budget worker holds a file's wb_lock or sem (a rebind's MAP_MANY, vext_map_data's sg_alloc_table included, and the freeze path's relieve) can enter fs reclaim and evict that file's inode.
- [ ] #2 A debug-kernel (PROVE_LOCKING) guest case writes files with write(2), closes them, waits for the flusher to clean their inodes, sets cladefs.dax_rebind_every and runs a memory hog that drives kswapd and direct reclaim into the inode shrinker: it reports no fs_reclaim / &file->wb_lock circular dependency and no hung task, and goes red with the fix reverted.
- [ ] #3 A static rule in the TASK-444.6 style flags any allocation reachable from virtio_extent_submit that is not GFP_NOFS or inside a memalloc_nofs scope, and is shown to flag today's sg_alloc_table call.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
