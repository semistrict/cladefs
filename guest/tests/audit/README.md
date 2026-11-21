# Guest filesystem audit checks

Run these checks from a revision on origin. The test-host driver applies the fixture in its disposable worktree, builds the guest kernel, and runs the checks on the test host. The patch carries both the probes and the guest tests registered against them.

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

## Kernel sandbox security review (2026-11-19)

`kernel-security-probes-8.patch` is the final review fixture: real guest-kernel malformed extent
requests, per-thread seccomp inspection after a Cell restore, host-kernel escape attempts under
Firecracker's real trapping filters, the repeated-MAP and greatest-sequence regressions, the VMM
large-WRITE heap probe, near-4-GiB malformed-request work, and pre-authorization cross-tenant
restore probes. Apply it to the named base revision in a disposable test-host worktree; these are
deliberately red on affected code and are not a suite to land unchanged.

```sh
# Compromised-VM protocol and tenant-isolation battery (no fixture; clean result expected).
CLADEFS_TEST_HOST_RUN=sec1 CLADEFS_TEST_PARTS=workspace CLADEFS_GUEST_TESTS=0 \
CLADEFS_HOSTILE_CASES=256 \
CLADEFS_TEST_ARGS='a_compromised_vm_process_harms_no_tenant_beside_it' \
scripts/test-host.sh target/security-review/hostile-256

# Real virtio, Cell restore, seccomp, VMM heap and extent regressions.
CLADEFS_TEST_HOST_RUN=sec2 CLADEFS_TEST_HOST_COMMIT=HEAD \
CLADEFS_TEST_HOST_PATCH="$PWD/guest/tests/audit/kernel-security-probes-8.patch" \
CLADEFS_TEST_PARTS=workspace \
CLADEFS_TEST_ARGS='kernel_escape_attempts_are_stopped_by_firecrackers_seccomp' \
CLADEFS_E2E_TEST='extent-guest extent-guest-attack fxfs-guest-snapshot-memory-cell-dax' \
scripts/test-host.sh target/security-review/kernel-probes

# Malformed-request work amplification and pre-authorization restore side effects.
CLADEFS_TEST_HOST_RUN=sec3 CLADEFS_TEST_HOST_COMMIT=HEAD \
CLADEFS_TEST_HOST_PATCH="$PWD/guest/tests/audit/kernel-security-probes-8.patch" \
CLADEFS_TEST_PARTS=workspace CLADEFS_GUEST_TESTS=0 \
CLADEFS_TEST_ARGS='an_unshared_restore_has_no_effect_before_its_refusal' \
scripts/test-host.sh target/security-review/protocol-probes
```

For the other workspace probes, substitute one of these test names for `CLADEFS_TEST_ARGS` in the
seccomp or authorization command: `large_writes_waiting_for_replies_do_not_gather_unbounded_payloads`,
`remapping_one_extent_registers_its_window_once`,
`the_greatest_extent_sequence_is_refused_before_a_write`, or
`a_malformed_request_does_not_zero_gigabytes`. A simple substring selects the intended test; the
compound `-E` expressions used during development selected the whole non-sim suite.

The standalone release sequence probe is `sequence_probe.rs`. Build and run it with:

```sh
cargo build --release -p local_cell --features unkept
STORAGE=$(echo target/release/deps/libstorage_device-*.rlib)
rustc --edition 2024 -L dependency=target/release/deps \
  --extern local_cell=target/release/liblocal_cell.rlib \
  --extern storage_device="$STORAGE" guest/tests/audit/sequence_probe.rs \
  -o target/security-review/sequence_probe
target/security-review/sequence_probe
```

Its affected output is `first write: Ok(())`, `next_sequence: 0`, `rewrite after free: Ok(())`,
`read back: a5`.
