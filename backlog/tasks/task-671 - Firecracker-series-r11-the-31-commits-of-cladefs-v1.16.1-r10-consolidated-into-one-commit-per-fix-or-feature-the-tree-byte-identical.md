---
id: TASK-671
title: >-
  Firecracker series r11: the 31 commits of cladefs-v1.16.1-r10 consolidated
  into one commit per fix or feature, the tree byte-identical
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-19 20:42'
updated_date: '2026-11-19 21:07'
labels:
  - firecracker
dependencies: []
priority: medium
ordinal: 1054000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
vendor/firecracker's series had grown to 31 commits on v1.16.1, many of them fixes and renames on earlier ones (the extent device alone was 13 commits, eight of which no longer build against today's crates: TASK-422). The user asked for a handful of commits, one per fix or feature we could upstream, with its fixes folded in, ordered least controversial and smallest first, with each commit's dependencies on the others noted, and anything no longer needed in a commit of its own to consider dropping; the rewrite's tip must be byte-identical to r10's.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Branch cladefs-v1.16.1-r11 of the fork has the series as consolidated commits on v1.16.1, its tip's tree identical to r10's (99b3b968a)
- [x] #2 Each commit's message says which commits of the series it depends on and which r10 commits it replaces
- [x] #3 scripts/firecracker-series-check.sh passes on r11, every commit both ways
- [x] #4 The user has reviewed r11; vendor/firecracker pins it, with its line in vendor/firecracker.rewrites
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch cladefs-v1.16.1-r11 (local, in vendor/firecracker's module repo; not pushed) at a382029c2: 16 commits, tree 57a3de3d9 = r10's 99b3b968a tree. Built by cherry-picking the general fixes in a new order (the PCI reset fix moved ahead of the shared memory regions, attach_dummy with it), the cladefs commits after; the memory cell split out of the extent commits (e9a226282, c4a163704 mixed both); the extent device commit is r10's tip less bac96b047's diff, and bac96b047 (the old 'extent' key refused) last, as a removal candidate. Textual dependencies computed by applying each commit onto v1.16.1 with the fewest earlier ones. Per-commit series check running on ubu1 (~/src/fxdax-r11check, log ~/src/fxdax-r11check-series.log). Resolves TASK-422's AC#1 once pinned: the extent commits that no longer built are folded into one.

Branch now at 043f49e18 (tree 57a3de3d9, identical to r10's). The first check found the EFAULT-retry commit calls devices::virtio::extent::dax_window_at, so it moved after the extent device (memory cell and extent trees rebuilt without it). scripts/firecracker-series-check.sh on ubu1: all 16 commits pass, both ways where the feature exists. ubu1 scratch removed. Waiting on the user's review, then a push of the branch, the pin and the rewrites line.

Restructured on the user's direction (upstreaming isn't drip by drip): r11 is 4 commits, one per upstream submission: 35d9ba276 = deps backport, the six general fixes (apply to v1.16.1 alone), virtio-pci shared memory regions + device IDs past 63, the cladefs feature (everything else). Each tree is one the ubu1 series check passed. The user chose to drop bac96b047 (old 'extent' key refused): tip = r10 less exactly that diff. Pushed to semistrict/firecracker; pinned on main with its vendor/firecracker.rewrites line (firecracker-forward.sh passes locally); scripts/test.sh, OPEN_QUESTIONS.md, docs/upgrading-a-host.md, docs/code.md updated. Closing waits on CI's firecracker-series and test jobs for the pin.
<!-- SECTION:NOTES:END -->
