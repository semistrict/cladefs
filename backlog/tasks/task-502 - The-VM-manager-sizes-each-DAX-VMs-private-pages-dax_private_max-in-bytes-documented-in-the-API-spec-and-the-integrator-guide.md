---
id: TASK-502
title: >-
  The VM manager sizes each DAX VM's private pages: dax_private_max in bytes,
  documented in the API spec and the integrator guide
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16T22:17:00Z'
updated_date: '2026-11-17 01:49'
labels:
  - dax
  - api
dependencies:
  - TASK-501
priority: high
ordinal: 891000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A DAX VM's private pages (doc-13: pages it wrote through its window and has not written back yet) are capped by the extent device's dax_private_max, 1 GiB unless set, the same for every VM whatever its vCPUs or memory. The sandbox platform wants the cap to scale with each VM's allocation (its CPUs, or memory), and owns that formula, not cladefs: the only measurement behind the default is forked pgbench needing 1 GiB at 2 vCPUs (TASK-227). The setting exists per VM already, but it is a count of pages (the caller must know the host's page size), the extent device is missing from Firecracker's API spec altogether, and the integrator guide says nothing on sizing it. Since TASK-501 the pages are the VM process's memory, charged to its cgroup, nothing reserved in the daemon.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The extent device takes the cap in bytes (a multiple of the page size, at least a page, at most the window), and a config naming the old page-count field is refused, not read as bytes
- [ ] #2 Snapshots taken before the change restore with the cap they had
- [ ] #3 Firecracker's swagger spec describes PUT /extent/{id} and every field of the extent device, the cap's unit and default included
- [ ] #4 docs/integrator-guide.md says what the cap bounds, what happens at it, its default, and how a VM manager sizes it per VM (with its vCPUs or memory), and that it counts against the VM process's memory
- [ ] #5 Tests cover the byte cap's validation and its conversion to the window's pages
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. virtio_extent Config: dax_private_max (pages) becomes dax_private_max_bytes (bytes); validate > 0, page multiple, <= window (InvalidDaxPrivateMaxBytes); deny_unknown_fields refuses the old name. private_max(len, bytes) returns pages for WINDOW args[1].
2. persist.rs ConfigState keeps its dax_private_max in pages (snapshot format unchanged), converting both ways.
3. vendor/firecracker call sites renamed; swagger gains /extent/{id} and an Extent definition.
4. guest test_cases vm.rs: CLADEFS_DAX_PRIVATE_MAX stays pages for scripts, written as bytes.
5. Docs: config.rs field doc, integrator-guide sizing section, architecture.md, doc-13, OPEN_QUESTIONS entry.
6. Tests: config validation and conversion; persist round trip; firecracker parser test.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented: Config.dax_private_max_bytes (bytes; >0, page multiple, <= window; InvalidDaxPrivateMaxBytes); dax_private_max refused by deny_unknown_fields; private_max(len, bytes) gives WINDOW its pages. persist ConfigState keeps its page count (snapshot format unchanged), converted both ways. Firecracker series commit 93ad15103 on local branch task-502-private-max-bytes (renamed call sites; swagger gains PUT /extent/{id} and Extent, every field); not pushed. Guest tests: CLADEFS_DAX_PRIVATE_MAX stays pages, written as bytes. Docs: integrator guide sizing section, architecture.md, doc-13, OPEN_QUESTIONS.
Verified (ubu1, fxjob): virtio_extent 45/45 (workspace features; -p alone fails EACCES opening stores, a feature-unification artifact, not this change); test.sh firecracker part 18/18 and 31/31; firecracker API parser 2/2; series check e10356028..93ad15103 builds both ways; workspace and loom clippy gates clean; guest test_cases check with host feature clean.

Review cleanup (0e2da7f52): the guest tests' knob is CLADEFS_DAX_PRIVATE_MAX_BYTES, given the device as is; CLADEFS_DAX_PRIVATE_MAX (pages) refused by an assert, as no script set it (plan step 4's 'stays pages for scripts' had no script behind it).
<!-- SECTION:NOTES:END -->
