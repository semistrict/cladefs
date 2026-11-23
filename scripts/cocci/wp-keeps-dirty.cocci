// A user mapping of the window write-protected and left dirty (wp_shared_mapping_range): the
// kernel makes a shared mapping's dirty PTE writable again with no fault
// (can_change_shared_pte_writable, from mprotect or a userfaultfd's unprotect), taking the
// filesystem to have been told of its store, which no longer holds of a page writeback wrote back:
// the next store lands in no dirty set (TASK-558). Write-protect and clean in one step
// (mapping_wrprotect_range, pfn_mkclean_range).
// Mutants: task-558-protect-keeps-dirty

@@
expression M, F, N;
@@
* wp_shared_mapping_range(M, F, N)
