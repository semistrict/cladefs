---
id: TASK-558
title: 'mprotect makes a written-back DAX PTE writable with no fault, skipping PRIVATE'
status: To Do
assignee: []
created_date: '2026-11-18 15:25'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 949000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-kernel-runtime review; verified against the code by independent reviewers. Severity: high.

**Where:** `guest/linux/fs/cladefs/dax.c`: `rg -n -F 'wp_shared_mapping_range(mapping, first + page, end - page);' guest/linux/fs/cladefs/dax.c`

Writeback write-protects a writeback set's PTEs with wp_shared_mapping_range (cladefs_dax_protect). Its wp_pte only clears the write bit and leaves the PTE dirty. In the base tree, pte_wrprotect keeps pte_dirty() true on x86 (saved-dirty) and on arm64 (PTE_DIRTY). The budget's writeback, seal and rebind (cladefs_dax_relieve) never clean those dirty bits: neither the rebind's MAP nor anything else touches the PTE. The same is true of the writebacks from SEEK_DATA, FIEMAP, write_inode, resize and drop. Only writepages' cladefs_dax_clean_entries runs pfn_mkclean_range, and that comes at the next flusher pass, up to ~30 s later. During that window a shared mapping holds read-only PTEs that are dirty. mprotect_fixup sets MM_CP_TRY_CHANGE_WRITABLE because cladefs has page_mkwrite and pfn_mkwrite (writenotify), and the uffd unprotect path in mm/userfaultfd.c sets it too. change_pte_range then calls can_change_shared_pte_writable, which returns pte_dirty(), and makes the PTE writable without calling cladefs_dax_mkwrite. Patch 0013 only stops a batch from spreading one PTE's dirty bit to its neighbours. It keeps the per-PTE rule, which wrongly assumes 'dirty means the filesystem was told'. This breaks doc-13 invariant 3 (no writable PTE outside the dirty and writeback sets) and DaxCow's 'private before any store reaches it'.

**Trigger.** 1) A process mmaps a DAX file (read in from sealed extents) MAP_SHARED with PROT_READ|PROT_WRITE and stores to page P. The write fault sends PRIVATE, adds P to the dirty set, and leaves the PTE writable and dirty. 2) Private pages pass 7/8 of private_max, or cladefs.dax_rebind_now is set. The worker's cladefs_dax_relieve calls cladefs_dax_write_back, whose cladefs_dax_protect leaves the PTE read-only but still dirty; it then writes, seals with cladefs_fsync and rebinds P with a MAP of its sealed extent page. No clean_entries runs. 3) The process calls mprotect(P, PROT_READ), then mprotect(P, PROT_READ|PROT_WRITE). change_pte_range sees pte_dirty and makes the PTE writable with no fault. 4) The process stores to P again. If the store lands between step 2's protect and its rebind, it goes into the private page outside every set: the copy may already be taken, and the rebind's MAP then replaces the page, so the store is lost silently and loads see the old bytes. If it lands after the rebind, it hits a shared window page that no PRIVATE claimed. The host either copies it on write without the guest counting it, or refuses it and the VM stops, as in TASK-149.42's r932. In the copy case the store is in no dirty set and is never written back, so it is lost at the next UNMAP, eviction or remount. The host's private count also drifts from the guest's: PRIVATE then fails with EIO past private_max, and cladefs_dax_frozen cannot rebind the stray page, so the host refuses VM snapshots with EBUSY.

**Why the tests stay green.** test_cow_mprotect_after_a_split_pmd only mprotects a page that was read and never stored to (its PTE is clean), and checks a different page. test_cow_store_persists rebinds after msync/fsync, whose writepages runs clean_entries first and leaves the PTEs clean. No check stores, rebinds through the budget path, toggles mprotect and stores again. guest/sim/kernel/mm.c models faults, inserts, unmaps and wp_shared_mapping_range but has no change_protection, mremap, fork or userfaultfd. Its check_no_writable_mapping only looks at pte_write at MAP time, so a dirty read-only PTE passes. specs/dax-cow/DaxCow.tla changes PTE writability only through faults, writeback and unmaps; it has no PTE dirty bit and no 'made writable without a fault' action.

**Backlog.** Related: TASK-149.42 (Done, patch 0013) stopped a batch from spreading one PTE's dirty bit but kept the per-PTE dirty rule. TASK-149.16 (Done) is the fork and VM_MIXEDMAP fix. Neither covers mprotect after cladefs's own write-protect.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A process stores to page P of a shared read-write mapping of a DAX file, and the budget's writeback, seal and rebind run (cladefs_dax_relieve or cladefs.dax_rebind_now). If it then calls mprotect(PROT_READ), mprotect(PROT_READ|PROT_WRITE) and stores to P again, the store faults and sends PRIVATE (dax_private_pages rises by exactly one), and the file holds that store after eviction. A userfaultfd write-protect then unprotect behaves the same.
- [ ] #2 A store that lands between the writeback's write-protect and its rebind (at the CLADEFS_DAX_STEP_WRITEBACK pause point), after such an mprotect toggle, is neither lost nor put outside the dirty and writeback sets.
- [ ] #3 A guest check, test_cow_mprotect_after_budget_rebind, with uffd and pause-point variants, goes red with the fix reverted. guest/sim mm.c gains a change_protection op that has the dirty-PTE rule. DaxCow.tla gains a PTE dirty bit and a make-writable-if-dirty action, with a spec mutation that TLC catches.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
