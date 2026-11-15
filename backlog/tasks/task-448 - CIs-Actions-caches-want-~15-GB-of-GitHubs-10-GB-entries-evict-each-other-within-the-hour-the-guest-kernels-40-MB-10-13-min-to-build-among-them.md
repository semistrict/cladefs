---
id: TASK-448
title: >-
  CI's Actions caches want ~15 GB of GitHub's 10 GB: entries evict each other
  within the hour, the guest kernel's (40 MB, 10-13 min to build) among them
status: To Do
assignee: []
created_date: '2026-11-15 12:55'
updated_date: '2026-11-15 13:35'
labels:
  - ci
dependencies: []
references:
  - .github/workflows/ci.yml
  - .github/actions/guest-kernel/action.yml
priority: medium
type: bug
ordinal: 830000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-330.1 (2026-11-15). The sandboxes runs on main saved guest-kernel-62f7ba18... at 11:38, and by 12:55 it was gone. The repo's caches were at 14.6 GB at 11:28 and 10.5 GB (8 entries) at 12:55. The largest: cargo-Linux-workspace 4.5 GB, cargo-Linux-object-stores 3.2 GB (only to build scripts/test-rustfs.sh's and test-azurite.sh's tests), macOS workspace 1.4 GB, the two shuttle caches 0.9-1.0 GB each, firecracker 0.7 GB, go 0.56+0.39 GB, clippy 0.58, pjdfstest-fuse 0.55, soak 0.6, sim 0.28. Their sum is past the limit, so GitHub evicts the least recently used each time another saves, and jobs keep building their dependencies whole again. A job that loses the guest kernel spends 10-13 more minutes building it: the sandboxes job on every push to main, nightly's guest-build. Also object-stores keys on Cargo.lock alone while the Linux test parts key on all three lock files, so their caches are saved apart.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The repo's caches fit in 10 GB at steady state (gh api repos/semistrict/cladefs/actions/cache/usage, after a day of main pushes)
- [ ] #2 The guest kernel is still cached, or can be had without a rebuild, a day after the last push that built it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
