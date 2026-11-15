---
id: TASK-437
title: Rename the project fxdax to cladefs
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:25'
updated_date: '2026-11-15 11:41'
labels: []
dependencies: []
priority: high
ordinal: 828000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user renamed the project to cladefs (2026-11-15): a clade is a common ancestor and all its descendants, as a base image and the forks grown from it. "fxdax" named the first prototype (Fxfs plus DAX); the project is now a forkable disk-image store. Live names change everywhere: crates, binaries, env vars, proto packages, Go module, guest config, scripts, docs, the guide, the Firecracker series, the GitHub repo. Historical records keep the old name: docs/measurements, closed backlog tasks, dead-ends/, DONE.md, old fork tags and branches. So do ubu1-provisioned names (the /mnt/fxdax-scratch mount, fxdax.slice and fxdax-bench.slice, the fxdax-bench resctrl group, fxdax-vmm-test-tap.service, /var/lib/fxdax-test), which the user left out of the rename.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No live file or path in the repo names fxdax, except references to historical records and ubu1-provisioned names; scripts/rename-to-cladefs.sh does the rename, so a branch can run it before rebasing
- [ ] #2 Crates, binaries (cladefs-extentd, cladefsctl, cladefs-mount, cladefs-gc), CLADEFS_* env vars, the proto packages (cladefs.extentd.v1, cladefs.mount.v1), the Go module and the frame magic are renamed, the Go code regenerated
- [ ] #3 The Firecracker series is rewritten with the cladefs feature and resources/cladefs, released as cladefs-v1.16.1-r7 on semistrict/firecracker, the submodule pinned to it and listed in vendor/firecracker.rewrites
- [ ] #4 The workspace builds and its tests pass on macOS and on ubu1, and a guest e2e case passes on ubu1 with the renamed Firecracker
- [ ] #5 The backlog project name, docs and decisions, and open tasks name cladefs
- [ ] #6 The GitHub repo is semistrict/cladefs and main CI is green after the push
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/rename-to-cladefs.sh: a perl filter (fxdax/FXDAX/Fxdax to cladefs/CLADEFS/Cladefs, the frame magic fxdaxob to cladefs, both 7 bytes) that keeps protected names: old fork refs (fxdax-v1.16.1*, fxdax-pci-r1), docs/measurements paths and entry names, and ubu1-provisioned names (fxdax-scratch, fxdax.slice, fxdax-bench.slice, resctrl/fxdax-bench, 146-fxdax_cgroups.sh, fxdax-vmm-test-tap, fxdax-host, /var/lib/fxdax-test). It rewrites the live files (everything tracked except backlog/, docs/measurements/, dead-ends/, DONE.md, vendor/firecracker.rewrites, *.pb.go), moves paths, and regenerates the Go code. --tree runs it on any tree (the fork), --filter on stdin (commit messages).
2. Fork: git filter-branch the 23-commit series from vendor/firecracker.base with the --tree and --filter modes, then push branch and tag cladefs-v1.16.1-r8; firecracker-series-check.sh looks for the cladefs feature; pin the submodule; add an OLD REWRITTEN line to vendor/firecracker.rewrites; .gitmodules branch cladefs-v1.16.1-r8.
3. Edit by hand what the script cannot know: the README VMM section, the integrator guide, a "formerly fxdax" note in the README.
4. Backlog: project_name, docs and decisions, open tasks (titles, descriptions) through the CLI.
5. Verify on macOS (cargo check/clippy, nextest, go test, guide tests, generate.sh --check), then on ubu1 under fxjob (workspace nextest, a guest e2e case with the renamed Firecracker).
6. Push to main, rename the GitHub repo, update origin URLs, check main CI.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Round 1: scripts/rename-to-cladefs.sh ran over 926 live files and 152 paths; the fork series (23 commits) rewritten by filter-branch with --tree/--filter, pushed as branch cladefs-v1.16.1-r8 (e10356028). Kept names grew from what broke: fxdax-work.slice and the NNN-fxdax_*.sh setup steps (fxjob, host.sh), and src/fxdax, ubu1's checkout, which host.sh cds to (run t437a failed: cd src/cladefs). macOS run 1 failed one test, cladefs::filesystem write_then_read: its data was b"fxdax", 5 bytes, now 7; the test writes 14 bytes and overwrites 7 in place, as it overwrote 5 of 12.
<!-- SECTION:NOTES:END -->
