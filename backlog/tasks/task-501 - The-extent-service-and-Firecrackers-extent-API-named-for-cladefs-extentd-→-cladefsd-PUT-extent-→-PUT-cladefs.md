---
id: TASK-501
title: >-
  The extent service and Firecracker's extent API named for cladefs: extentd →
  cladefsd, PUT /extent → PUT /cladefs
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16 22:23'
updated_date: '2026-11-16 22:57'
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
- [ ] #1 No live file says extentd, Extentd or EXTENTD, but records (docs/measurements, docs/reviews, dead-ends/, DONE.md, the backlog's closed tasks, vendor/firecracker.rewrites), the earlier rename scripts and renamed.rs
- [ ] #2 Firecracker's API says cladefs: PUT /cladefs/{id}, the configuration file's `cladefs`, LoadSnapshot's `cladefs_overrides`, `cladefsd_socket`, the swagger; a configuration still saying `extent` is refused, naming `cladefs`
- [ ] #3 The Firecracker series is rewritten, released as cladefs-v1.16.1-r9 on semistrict/firecracker, the submodule pinned to it and listed in vendor/firecracker.rewrites
- [ ] #4 A/B tooling compares a side from before the rename: its extentd-stats.txt and extentd-* metrics read as the new names
- [ ] #5 A script renames a branch begun before (repo, --tree, --filter, --backlog modes), tested as rename-to-cladefsd.sh is
- [ ] #6 CONTEXT.md, the integrator guide and upgrading-a-host say the new names
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
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
<!-- SECTION:NOTES:END -->
