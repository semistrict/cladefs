---
id: TASK-503
title: >-
  The extent service and Firecracker's extent API named for cladefs: extentd →
  cladefsd, PUT /extent → PUT /cladefs
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 22:23'
updated_date: '2026-11-17 02:32'
labels:
  - naming
  - host
dependencies: []
priority: high
ordinal: 890000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-475.1 renamed the binary cladefsd and its variables CLADEFSD_*, but kept extentd for the extent service: the gRPC service cladefs.extentd.v1.Extentd, its Go package go/extentd/v1, the Rust module cladefs::extentd, Firecracker's extentd_socket, and the files a daemon or a run writes (extentd.sock, extentd.log, extentd-stats.txt). Firecracker's API also names the device extent: PUT /extent/{id}, the configuration file's `extent`, `extent_overrides`. The user asked (2026-11-16) for every name in commands and APIs to say cladefs: extentd becomes cladefsd everywhere (proto cladefs.cladefsd.v1.Cladefsd, go/cladefsd/v1, cladefs::cladefsd, cladefsd_socket, cladefsd.sock, cladefsd-stats.txt, the stats JSON key), and Firecracker's API layer becomes cladefs (PUT /cladefs/{id}, the configuration's `cladefs`, `cladefs_overrides`, CladefsDeviceConfig). Domain words stay: the extent store device, extent_store, ExtentId, virtio_extent, and Firecracker's device module devices/virtio/extent. Records keep the old names, as TASK-437 and TASK-475.1 kept theirs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 No live file says extentd, Extentd or EXTENTD, but records (docs/measurements, docs/reviews, dead-ends/, DONE.md, the backlog's closed tasks, vendor/firecracker.rewrites), the earlier rename scripts and renamed.rs
- [x] #2 Firecracker's API says cladefs: PUT /cladefs/{id}, the configuration file's `cladefs`, LoadSnapshot's `cladefs_overrides`, `cladefsd_socket`, the swagger; a configuration still saying `extent` is refused, naming `cladefs`
- [x] #3 A/B tooling compares a side from before the rename: its extentd-stats.txt and extentd-* metrics read as the new names
- [x] #4 A script renames a branch begun before (repo, --tree, --filter, --backlog modes), tested as rename-to-cladefsd.sh is
- [x] #5 CONTEXT.md, the integrator guide and upgrading-a-host say the new names
- [x] #6 The guest kernel's filesystem module is cladefs: fs/cladefs, mount -t cladefs, CONFIG_CLADEFS_FS, cladefs.* parameters, cladefs tracepoints, /proc/fs/cladefs, guest/rust/cladefs-linux, guest/sim; Fxfs (Fuchsia's crates and format) and the fxfs-guest-* cases keep their names; guest cases pass on a guest kernel built so
- [x] #7 The Firecracker series is rewritten, released as cladefs-v1.16.1-r10 on semistrict/firecracker, the submodule pinned to it and listed in vendor/firecracker.rewrites
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/rename-extentd.sh (repo, --tree, --filter, --backlog): perl renaming extentd/Extentd/EXTENTD to cladefsd/Cladefsd/CLADEFSD everywhere live, and Firecracker's configuration layer extent → cladefs (PUT path, config key, ExtentDeviceConfig, ExtentConfigError, ExtentBuilder, InsertExtentDevice, ExtentOverride/extent_overrides, UnknownExtentDevice, VmResources.extent, VmmConfig.extent_devices, vmm_config::extent and request::extent modules and files, swagger); device internals (devices/virtio/extent, ExtentState, DeviceStates.extent_devices) keep theirs. Paths moved (crates/cladefs/src/extentd, proto, go/extentd, extentd-mapbench). Go regenerated.
2. scripts/tests/test_rename_extentd.py: what --filter renames and keeps.
3. Fork: filter-branch the series from vendor/firecracker.base with --tree and --filter; one commit on top refusing a configuration's `extent`; branch and tag cladefs-v1.16.1-r9 pushed; submodule pinned, .gitmodules, vendor/firecracker.rewrites.
4. A/B readers (ab-rounds.py, ab-guest-summary.py, budgets/guest_results as needed) take an old side's extentd-stats.txt and extentd-* metrics as the new names.
5. Hand edits: CONTEXT.md, integrator guide, upgrading-a-host, OPEN_QUESTIONS.md (decided: config layer cladefs, device layer extent).
6. Verify: macOS scripts/test.sh, go test, guide tests, generate.sh --check, upstream-changes --check; ubu1 under fxjob: workspace nextest, firecracker-series-check, a guest e2e case.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Round 1: scripts/rename-to-cladefs-api.sh (named so it never says the old word itself) renamed 195 live files and 33 paths; idempotent (a second run leaves the tree as it is). The fork series rewritten by filter-branch --tree/--msg-filter: the 14 commits before the extent device unchanged (same trees and messages), the 9 after renamed; stable rustfmt of the pinned toolchain run on renamed Rust files that were formatted before (insert_cladefs_device ran past 100 columns; modules reorder). Then one commit on top: a configuration file's `extent` refused (ResourcesError::CladefsRenamed). Branch cladefs-v1.16.1-r9 pushed (8506cccc1), submodule pinned, rewrites line e10356028 -> 26add4ca1.
Found on the way: thread names "extentd request/control/metrics/signals" became 16 bytes, past Linux's comm (perf, bpftrace saw them cut): now "cladefsd reqs/ctl/metric/signal". bpftrace strncmp lengths of "extentd.bucket"/"extentd-cache" fixed to the new names' lengths. go/cladefsd/v1/client.go needed gofmt (the script runs it). --backlog first turned stale CLADEFS_EXTENTD_* in docs into CLADEFS_CLADEFSD_*: the script applies TASK-475.1's renames first; redone.
Decided without the user (OPEN_QUESTIONS.md): Firecracker's configuration layer says cladefs, the device layer keeps extent; A/B readers take an old side's extentd-* metrics and stats file as the new names (ab-rounds.py named_now, ab-guest-summary.py, ab-guest-cases.sh prune). Qualification's harness driving a pre-rename pin's build breaks on the gRPC and Firecracker names, as TASK-474's proto changes already broke it (no migrations, AGENTS.md).
macOS: clippy, scripts 523 tests, guide 42/42, upstream-changes --check pass.

Correction to the note above: what already breaks the qualification A/B against a pin from before is TASK-437 and TASK-475.1, not TASK-474 (proto field renames keep their numbers): this tree's test-e2e.sh builds a side with `cargo build -p cladefs --bin cladefsd`, which a tree from before them has neither of. TASK-501 adds the gRPC package and Firecracker's keys to that; the pin moving past them is what mends it.

Scope added by the user (2026-11-16): fxfs renamed cladefs inside Linux, the kernel module only (Fuchsia's crates and format, and the fxfs-guest-* cases, keep Fxfs), folded into this task. The script collects the C symbols guest/linux and guest/sim define (GUEST_SYMBOLS) and the module's parameters (GUEST_PARAMS) before renaming, renames the kernel C scope (guest/linux, guest/sim, guest/mutants, crates/fxfs-sim-diff/shim, cocci, bpftrace) whole, and elsewhere only the module's interface. First run missed guest/sim's own fxfs_sim_* symbols and the sim-diff shim's `struct fxfs`, which crates/fxfs-sim-diff failed to build on; fixed in the script and the tree. macOS cargo check of fxfs-sim-diff passes.

Renumbered TASK-501 -> TASK-503 at the merge of main (backlog doctor --fix): another session's TASK-501 (CoW DAX windows reserve no daemon memory) and TASK-502 landed on main meanwhile; this branch's commits before the merge say TASK-501. Main's fork commit for TASK-502 (93ad15103, on r8) taken into the renamed series: cladefs-v1.16.1-r10 (bac96b047) is r8 through it renamed (93ad15103 -> 9bc4a5a8e, its upstream swagger's device named Cladefs/putCladefsDeviceByID too), then the refusal citing TASK-503. r9 (8506cccc1), the first cut, was never pinned on main. Merge conflicts were all in files this rename only renamed: main's side taken, the script run again over it (idempotent after), --backlog again.

Validation at c0a8b6edd (second merge of main): macOS scripts/test.sh whole (2638 workspace, 527 scripts, sim, shuttle, mutants, Go); ubu1 under fxjob at 08a73ab43: Linux test.sh whole (2948 workspace; scripts 526/527, test_nightly the known TASK-500), the Firecracker part with the refusal test; guest cases on a guest kernel built from the renamed module: fxfs-guest-dax, -snapshot-dax, -daemon-lost, -checks-dax, -follow-dax, -bpftrace-dax, -automount at 08a73ab43, and -dax, -snapshot-dax, -checks-dax, -bpftrace-dax at c0a8b6edd. guide 42/42, upstream-changes --check, firecracker-forward (CI). guest/sim: CI's zloop failed 2997/3000 because sim/kernel/lib.c chose allocations to fail by "fs/cladefs/" in the path, which CI's checkout, /home/runner/work/cladefs/cladefs/, says too; now "linux/fs/cladefs/": reproduced on ubu1 under a cladefs/cladefs/ checkout (old fails, new 30/30). CI failures not this task's, on main too: object-stores, test_nightly (TASK-500), kms_having_no_kek_refuses_new_cells_as_destroyed (TASK-505 filed), firecracker-series's commits before TASK-417 (TASK-422). The repo script run again leaves the tree as it is.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Every name in commands and APIs says cladefs: the extent service is cladefsd everywhere (gRPC cladefs.cladefsd.v1.Cladefsd, go/cladefsd/v1, cladefs::cladefsd, cladefsd_socket, cladefsd.sock/.log/-stats.txt, metrics, trace spans, thread names within 15 bytes); Firecracker's configuration of the device is cladefs (PUT /cladefs/{id}, the configuration's cladefs, cladefs_overrides, CladefsDeviceConfig), its old key refused, released as cladefs-v1.16.1-r10 and pinned; the guest kernel's module is cladefs (fs/cladefs, mount -t cladefs, cladefs.* parameters and tracepoints, /proc/fs/cladefs, guest/rust/cladefs-linux, guest/sim), Fxfs keeping its name. scripts/rename-to-cladefs-api.sh does it for branches (repo, --tree, --filter, --backlog; idempotent; tested); A/B readers take a side from before. Verified on macOS and ubu1 (full suites, 7 guest cases on the renamed kernel) and CI; the remaining CI failures are main's too.
<!-- SECTION:FINAL_SUMMARY:END -->
