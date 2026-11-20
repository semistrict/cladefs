---
id: TASK-685.13
title: >-
  cladefsd builds under --cfg madsim on Linux: its gRPC services (control API,
  health, mount, peers) on madsim's tonic
status: To Do
assignee: []
created_date: '2026-11-20 15:20'
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
