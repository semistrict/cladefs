---
id: TASK-504
title: 'IDs in base 32: bucket keys, store paths and displayed IDs leave hexadecimal'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-17 01:49'
labels: []
dependencies: []
ordinal: 892000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every u64 naming something (tenant, cell, branch, extent sequence number, segment number, epoch, version, pass) is written as 16 hexadecimal digits in bucket keys and store paths, and unpadded hex where operators read and type it (TENANT/CELL/ID, cladefsctl, cladefs-gc, metrics labels, KMS key names, control socket names). The user asked for base 32: shorter keys and names. No deployment exists, so the formats change outright with no compatibility (AGENTS.md: no migrations).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 One codec, used everywhere: fixed width 13 digits in keys and file names, sorting as the numbers do; shortest form where displayed; both refuse non-canonical text
- [ ] #2 No ID is written or parsed in hexadecimal in bucket keys, store paths, CLI input and output, metrics labels, KMS key names, sockets and logs
- [ ] #3 Docs and comments say base 32 where they said hexadecimal of IDs
- [ ] #4 Workspace tests pass on macOS and the Linux suite on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
