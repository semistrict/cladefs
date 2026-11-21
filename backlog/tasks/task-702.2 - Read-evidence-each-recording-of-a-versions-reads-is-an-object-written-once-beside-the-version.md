---
id: TASK-702.2
title: >-
  Read evidence: each recording of a version's reads is an object written once
  beside the version
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 19:46'
updated_date: '2026-11-21 06:03'
labels:
  - sandbox
  - cache
dependencies:
  - TASK-702.1
parent_task_id: TASK-702
priority: high
ordinal: 1104000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A prediction of a version's reads must reach every host and outlive daemon restarts (TASK-38's profiles live in a per-pid temp directory, TASK-670), but forks that record are other cells, often on other hosts, and only a cell's writer changes it. Evidence objects created once, at keys of their own, need no writer and no in-place merge, as segments need none (TASK-407). Recency and expiry make the decay; nothing is rewritten. A shared version's evidence grown from other tenants' forks would tell every tenant what one tenant read.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A fork of a version, recording, writes its evidence as a new object beside the version by a conditional create, in a format of its own (refused if unknown), encrypted under the version's cell's keys
- [x] #2 A fork writes and reads only its own tenant's evidence of a version, a public base's and a shared version's alike (decided with the user 2026-11-20): another tenant's fork reads none of it (test)
- [x] #3 The collector deletes a version's evidence with the version, and all but each tenant's newest K = 8 of it, by the bucket's time; evidence written changes no listing digest, so a base's cell stays skipped between them (test)
- [x] #4 A host reads a version's evidence for a tenant, and writes it, at most once an interval, not on every fork it opens (test)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 scripts/upstream-changes.py --check passes
- [x] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell::evidence: keys <parent>reads/<at>/<tenant>/<random> (layout::Kind::Evidence, owner the parent, FORMAT 24); object framed under the version's cell's newest DEK the fork's ring holds (Ring::encrypting_as), footer {evidence format 1, its key} so a copy elsewhere is refused; data the units (extent, MiB) with each one's first read in ms. write (conditional create), read (list the tenant's prefix, newest K = 8 by the bucket's time, get, open, decode; skip what fails), Prediction (union, earliest first read first: TASK-702.1's model).
2. Collector: evidence leaves the listing a pass decides by and its digest (as gc/ does); every pass that deletes trims each (at, tenant) to its newest K, the skipping look too (evidence needs no lease: nothing relies on it); a full pass deletes the evidence of versions its head names no more, all of a dead cell's, so removal leaves the prefix empty; mark-only and dry runs delete none. collect_copy deletes any with the copy. Spec: specs/bucket models no evidence, whose loss costs speed alone: argued in notes.
3. local_cell: a fork's store says its version's origin and its ring (Uploader from its writer's head and keys; open_ephemeral's too).
4. cladefs: OpenCell, opened for a guest (profiled), a fork of a version: a host-wide evidence table per (origin, tenant) reads the prediction once an interval (10 min) and lets one fork record once an interval; a recording takes the READs, MAPs and faults the daemon's connection serves (server.rs's traced hooks, beside the read trace) of inherited extents in the first 10 s, written at the window's end or the store's close, off the runtime. Prediction held by the store for TASK-702.3.
5. Tests: evidence round trip, unknown format refused, copied key refused, wrong keys refused; collector trims to K per tenant, deletes with a dropped version and a dead cell, skipped cell still trimmed, digest unchanged by evidence; daemon: forks of a shared version by two tenants, each reads its own tenant's evidence alone; a second fork within the interval reads and writes nothing. CONTEXT.md (Read evidence, Prediction), doc-7 layout.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From TASK-702.1 (2026-11-20): a recording must take the VM's MAPs (and READs), not its faults alone: a warm host's VM faults on none of what its MAPs install (crates/cladefs/src/cladefsd/reads.rs records all three). Units are MiBs of extents (an Fxfs extent is a MiB at most). Keep the last K = 8 recordings; older ones are what the collector removes beyond the count.

Verified 2026-11-21: Mac, the changed crates 1163/1163 (before the rebase) and the rebased tree (below); ubu1 workspace 3174/3175, the one failure TASK-710's pprof flake on a loaded host; evidence tests passed there (a_restored_vms_reads_are_its_versions_evidence, forks_of_a_version_read_and_record_their_tenants_evidence). upstream-changes.py --check passes. Rebased onto main's host_fs change (open_ephemeral's signature merged).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Read evidence: each fork of a version opened for a guest records what its guest reads of the version in its first 10 s (READs, MAPs, faults, in MiB units with their first-read times) as an object created once at <version cell>reads/<at>/<tenant>/<id>, encrypted under the version's cell's newest DEK, format 1 refused otherwise. Per tenant: a fork reads and writes its own tenant's evidence alone. The prediction is the union of the tenant's newest 8 recordings within 7 days, earliest first read first. The collector trims to each tenant's newest 8 on every pass (skipped cells too, evidence out of the listing digest) and deletes a dropped version's and a dead cell's. A host reads a version's prediction for a tenant once per 10 min and records one fork per 10 min. Bucket FORMAT 24. Verified by unit tests (round trip and refusals, per-tenant reads, trimming, digest), daemon tests (two tenants' forks of one version; a restored VM's MAP and READ recorded), and the Mac and ubu1 runs.
<!-- SECTION:FINAL_SUMMARY:END -->
