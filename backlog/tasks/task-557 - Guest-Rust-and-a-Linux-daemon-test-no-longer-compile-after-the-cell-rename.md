---
id: TASK-557
title: Guest Rust and a Linux daemon test no longer compile after the cell rename
status: To Do
assignee: []
created_date: '2026-11-18 15:25'
updated_date: '2026-11-18 17:07'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 948000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-driver, fxfs-alloc review; verified against the code by independent reviewers. Severity: high.

**Where:** `guest/rust/cladefs-linux/src/device.rs`: `rg -n -F 'Device, ExtentId, NoExtent, OpenExtents, OtherBranch, Previous, ReadOptions, SealedExtent,' guest/rust/cladefs-linux/src/device.rs`
**Where:** `guest/rust/cladefs-linux/src/fs.rs`: `rg -n -F 'self.filesystem.allocator().set_branch(branch);' guest/rust/cladefs-linux/src/fs.rs`

Commit f3cf366ce ('TASK-508.3: branch is cell everywhere') renamed several identifiers. In storage_device: `ExtentId { branch, .. }` became `{ cell, .. }`, `OtherBranch` became `OtherCell`, and the Device trait's required `fn branch` became `fn cell`. In fxfs: `Allocator::set_branch` became `set_cell`. The commit changed nothing under guest/rust. guest/rust/cladefs-linux depends on crates/fxfs and crates/storage_device by path, and at HEAD it still uses the old names. It imports `OtherBranch` (device.rs:48, E0432). It reads `extent.branch` (device.rs:946, 1071 and 1126, E0609). It implements `fn branch` and no `fn cell` in `impl Device for ExtentDevice` (device.rs:1090, E0407 and E0046). It calls `allocator().set_branch(branch)` (fs.rs:355, E0599). The hand-written ffi structs' `branch` fields only mirror the C layout and are not part of the break. Separately, merge 845a6c123 brought in crates/cladefs/src/cladefsd/tests.rs:602 `ExtentId { branch: store.branch(), sequence: 1 }` from c97c3490d (TASK-444.15, written on main before the rename). CellClient only has `cell()`, so the Linux test build of crates/cladefs fails too. Trigger: every guest kernel build fails at HEAD (main 6f4dd56a0 and origin/main 24c6081a6). scripts/build-guest-kernel.sh:194-197 runs `cargo build --release -p cladefs-linux`, which test-host.sh, bench-host.sh, guest cases and qualifications all go through, and scripts/check-guest.sh:42 fails the same way. `cargo nextest run --workspace` on the test host fails too, because cladefsd is `#[cfg(target_os = "linux")]`. So no bzImage is built, and no guest case, guest mutant, nightly, qualification of any candidate or Linux daemon test can run. No data is at risk. Why it stayed green: guest/rust is a separate Cargo workspace, built for the kernel's target against a custom sysroot, and macOS's `cargo nextest run --workspace` never builds it. The cladefsd module and its tests compile on Linux only. No test host run or guest kernel build has happened since f3cf366ce. scripts/check-guest.sh already does the cargo check the fix needs, but no CI job or test.sh part runs it. One caveat: CI's sandboxes (short) job runs on every push to main and builds the guest kernel through .github/actions/guest-kernel on a cache miss. Its cache key (scripts/guest-kernel-key.sh) hashes storage_device and fxfs, so main's push CI is probably already red at that job. Nobody has checked the CI run.

**Backlog.** Related to TASK-508.3 (To Do; f3cf366ce landed under its ID, but its notes say not started) and TASK-508.4 (To Do; the remaining renames). Neither names this break. TASK-466 and TASK-96.2 (Done) fixed earlier breaks of the same class in other crates, so this is not a regression of either.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A guest kernel builds from main: scripts/build-guest-kernel.sh and scripts/check-guest.sh compile guest/rust/cladefs-linux with no error.
- [ ] #2 `cargo nextest run --workspace` on the test host builds and runs crates/cladefs's Linux tests, cladefsd's tests.rs included.
- [ ] #3 Push CI, and scripts/test.sh where the toolchain is installed, type-check the guest workspace (scripts/check-guest.sh) and the workspace's Linux test targets on every push. Run on f3cf366ce, the check fails with the OtherBranch and ExtentId::branch errors.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Merged origin/main (6046b0f78): the rename is done in code. a1167a56a and 8111fb784 moved guest/rust/cladefs-linux to set_cell, OtherCell, ExtentId::cell and fn cell, and 24c6081a6 fixed cladefsd tests.rs. rg finds none of the old names. Nothing has been compiled yet: AC #1 and #2 need a guest kernel build and a Linux nextest run on the test host. AC #3, the blind spot that let this through, is still open: .github/workflows/ci.yml still never type-checks guest/rust on push or pull_request.
<!-- SECTION:NOTES:END -->
