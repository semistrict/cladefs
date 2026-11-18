# Guest filesystem audit checks

Run these checks from a revision on origin. The test-host driver applies the fixture in its disposable worktree, builds the guest kernel, and runs the checks on ubu1. The patch carries both the probes and the guest tests registered against them.

```sh
CLADEFS_TEST_HOST_COMMIT=HEAD \
CLADEFS_TEST_HOST_PATCH="$PWD/guest/tests/audit/guest-probes.patch" \
CLADEFS_WORKSPACE_TESTS=0 \
CLADEFS_E2E_TEST=fxfs-guest-checks-dax \
CLADEFS_CHECKS=failed_flag_update,sparse_lookup_count,sparse,single_dirty_range_visits,busy_file_collection,syncfs_older_writeback \
scripts/test-host.sh /tmp/guest-filesystem-audit
```

- TASK-523 injects EIO at the synchronous attribute write. The check reads the flags and writes the file before and after eviction.
- TASK-526 counts the calling thread's extent lookups on all-hole files of two sizes. The existing sparse check covers mixed data and FIEMAP.
- TASK-527 checks how many ranges writeback tests for dirtiness while writing a single page. Its fixture independently counts the resident ranges and pages the calling thread wrote. It uses operation counts throughout.
- TASK-536 checks the collector and the actual syncfs syscall independently. The collector check dirties a lower-numbered file between the two passes. The syscall check holds the older writeback before its commit and records the kernel stacks if VFS writeback or the final collection waits on it. A return while paused fails the check. A timeout means the test did not establish the ordering. An omission by the collector alone leaves the syscall's durability unresolved.
