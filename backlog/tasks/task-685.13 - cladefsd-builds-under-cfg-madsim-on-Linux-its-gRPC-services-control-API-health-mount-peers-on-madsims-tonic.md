---
id: TASK-685.13
title: >-
  cladefsd builds under --cfg madsim on Linux: its gRPC services (control API,
  health, mount, peers) on madsim's tonic
status: Done
assignee: []
created_date: '2026-11-20 15:20'
updated_date: '2026-11-20 17:12'
labels: []
dependencies: []
parent_task_id: TASK-685
priority: high
ordinal: 1088000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-20 adding the madsim part (TASK-685.12): on Linux, RUSTFLAGS='--cfg madsim' cargo check -p cladefs fails in cladefsd. madsim-tonic's generated servers take another shape (Service<(PathAndQuery, Request<BoxMessageStream>)>, generics <T, F>, tonic::transport::NamedService), so control.rs's Drained tower wrapper over http::Request, tonic_health's reporter (real tonic), and mount.rs/fuse::control's MountServer<impl Mount> signatures don't build. select!, timeout_at and block_in_place were fixed with it. Until done, scripts/test.sh's madsim part checks cladefs's madsim build on macOS only. Part of doc-24 step 12.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20 (3beb3af0c): cladefsd builds under --cfg madsim on Linux: control API's Drained serves both tonic shapes (Refusal), served at a simulated TCP address under madsim (no health service, its status still reported); FUSE Mount service out of the simulation; peers' server at a simulated address; boot async with no VM socket/FUSE/metrics under madsim. test.sh's madsim part checks cladefs on Linux too. Verified: ubu1 clippy + unit 3031/3031 + VMM + guest harness; Mac clippy + unit 2688/2688 + madsim.
<!-- SECTION:NOTES:END -->
