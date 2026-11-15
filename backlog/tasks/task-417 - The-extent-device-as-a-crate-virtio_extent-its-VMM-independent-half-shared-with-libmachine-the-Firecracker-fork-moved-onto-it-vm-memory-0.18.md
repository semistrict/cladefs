---
id: TASK-417
title: >-
  The extent device as a crate (virtio_extent): its VMM-independent half shared
  with libmachine, the Firecracker fork moved onto it, vm-memory 0.18
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 21:47'
updated_date: '2026-11-15 01:25'
labels:
  - task
dependencies: []
priority: high
ordinal: 808000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
libmachine (a Rust library VMM, KVM in-process, Python bindings) hosts fxdax's extent device (doc-4, type 0x4658) beside fxdax's Firecracker fork, and asked (its doc-2, the boundary) that the device be shared, not copied: one versioned protocol with no migrations must not live in two copies that drift. Today the device lives only in the fork (src/vmm/src/devices/virtio/extent, ~3,750 lines). libmachine's TASK-18 depends on the crate at a commit fxdax sends back. The user decided both VMMs use vm-memory 0.18 (libmachine's) and upgrade it together from then on; upstream Firecracker made that bump with no source change (27e968cb1).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A crate in this repo holds the protocol (wire structs, flags, features, ConfigSpace, VIRTIO_EXTENT_VERSION and its check), request handling (Buffers built from guest-memory segments in descriptor order, Reader/Writer, handle and the serve dispatch, per-type stats), the notices behind a trait the VMM implements, the DAX window logic behind a trait for what only the VMM can do (reserve guest-physical addresses, add and remove a memory slot) plus PrefaultGuestMemory, and the snapshot state as plain serializable values with doc-4's freeze/point/fork protocol; no Firecracker or KVM-binding type in its public API, no Firecracker crate among its dependencies
- [ ] #2 The crate builds and tests on its own, with unit tests of request handling over fake guest memory and a fake store (on macOS too), besides the request tests against a real daemon
- [ ] #3 The Firecracker fork uses the crate, its extent module reduced to the adapter (transport, queues, BAR, memory slot, KVM_PRE_FAULT_MEMORY, threads, snapshot embedding and the version refusal), with no behaviour or protocol change: VIRTIO_EXTENT_VERSION stays 1 and a snapshot's extent state serializes as before
- [ ] #4 The fork is on vm-memory 0.18, as a general fix at its series' base, and every commit of the series builds both ways (scripts/firecracker-series-check.sh)
- [ ] #5 fxdax's guest cases and the Linux workspace suite pass on ubu1 against the new fork, the snapshot and DAX cases among them
- [ ] #6 The crate has a README listing the adapter's obligations; doc-4 says the device is virtio-PCI only (no MMIO since TASK-273)
- [ ] #7 The fxdax commit to pin is sent back for libmachine's TASK-18
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Fork: cherry-pick upstream 27e968cb1 (rust-vmm bump, vm-memory 0.18, no source change) at the series' base, after the general fixes and before 76fae13ea (the feature); rebase the series onto it as fxdax-v1.16.1-r7, with a vendor/firecracker.rewrites line.
2. crates/virtio_extent (Apache-2.0, as the fork's files it comes from): protocol.rs (zerocopy wire structs, no vm-memory), config.rs (Config, the API's serde type, and its validation; CacheType), notices.rs (ConfigInterrupt trait, Resizing/Following/StatsAsks), buffers.rs (Segment, Buffers, Reader, Writer), store.rs (Store trait of what serve calls; impl for RemoteExtentStore on Linux), requests.rs (handle, serve, Served, Requests: spawn_on_replies and the in-flight count), window.rs (WindowVm trait: reserve/release, add_slot/remove_slot; HostMemory; DaxWindow; dax_window_at; PrefaultGuestMemory; private_max), device.rs (Device: store, config space, features, window, activation, reset, snapshot), persist.rs (State, ConfigState, MappingState, Snapshot, check_version). README with the adapter's obligations.
3. Fork tip commit: the extent module the adapter (Extent over virtio_extent::Device; worker = epoll loop, queue pops, Buffers from descriptor chains with dirty marking, used ring and MSI-X; window.rs = WindowVm over KvmVm/ResourceAllocator/KVM slots, KvmPrefault; persist = ExtentState { protocol, virtio_state, device } byte-identical to before). firecracker-cargo.sh appends virtio_extent too; resources/fxdax/README.md says so.
4. Tests move to the crate: protocol/notices/config/persist units, request tests against a real daemon (Linux), plus fake-store request tests (everywhere). Fork keeps the transport-only ones.
5. One compile pass: cargo check/clippy/nextest on macOS for the crate; on ubu1: workspace, firecracker-cargo.sh clippy+tests with the feature, firecracker-series-check.sh, then scripts/test-host.sh guest cases (snapshot, DAX).
6. doc-4: virtio-PCI only. OPEN_QUESTIONS decisions. Land on main, push, send back the commit.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Written: crates/virtio_extent (protocol, config, notices, buffers, store, requests, window, device, persist, README, tests/daemon.rs); fork adapter (extent/{mod,device,worker,window,persist}.rs, vmm_config/extent.rs re-exporting the crate's Config, KvmVm::add_memory_slot/remove_memory_slot replacing KvmMemSlot); fork series rebased onto upstream's vm-memory 0.18 + vhost 0.17 bumps (local branch fxdax-v1.16.1-r7, clean rebase); firecracker-cargo.sh appends virtio_extent; vendor/firecracker.lock relocked; doc-4 says PCI only; guide anchors repointed. macOS: crate clippy clean, 30/30 tests. Linux compile on ubu1 in progress.

ubu1 r t417a (8fea07c16, fork e7c0a413e): workspace 2698/2698 and the other workspace parts pass; default guest suite passes (DAX, crash, docker, pgbench, extent-guest-other-version); firecracker part 19/19 extent + 31/31 KVM; crate 42/42 on Linux. snapshots_serialize_as_before passes: a snapshot taken before restores after. Next: snapshot/migrate/suspend/follow guest cases (t417b), then the series check.

ubu1 t417c: fxfs-guest-snapshot, -snapshot-dax, -snapshot-volumes(-dax), -suspend-dax, -follow-dax, -follow-remount-dax pass; fxfs-guest-migrate-dax fails, as on main (TASK-419). User chose (2026-11-14) the queue seam before landing: the crate owns the workers, pause/resume and reset behind a RequestQueue trait the VMM implements.
<!-- SECTION:NOTES:END -->
