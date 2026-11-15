---
id: TASK-439
title: >-
  doc-11 says a drain rehearsal runs nightly by scripts/migrate-gce.sh: the
  script is retired and nothing schedules scripts/migrate-host.sh
status: To Do
assignee: []
created_date: '2026-11-15 11:37'
labels:
  - testing
dependencies: []
priority: medium
ordinal: 829000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-11 (Upgrading fxdax in production), under "What makes this true: tests", lists "A drain rehearsal. Nightly, scripts/migrate-gce.sh moves a VM off a host and back" (TASK-141.4). scripts/migrate-gce.sh no longer exists (GCE retired); its successor scripts/migrate-host.sh is named by no workflow (.github/workflows has no migrate, bench or qualify job) and no schedule in scripts/. So the one procedure every production upgrade depends on, moving a VM off a host through the bucket and draining, is rehearsed only when someone runs it by hand, and the doc claims otherwise. TASK-419 (fxfs-guest-migrate-dax fails in a plain test-host.sh run, ENOENT) is the kind of break a rehearsal on a schedule would have shown at once. Found 2026-11-15 surveying what finds production issues ahead of customers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 doc-11 names how and how often the drain rehearsal runs today, and names no retired script
- [ ] #2 The rehearsal runs on a schedule or as a step of scripts/qualify.sh, and a failing one is seen without anyone asking
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
