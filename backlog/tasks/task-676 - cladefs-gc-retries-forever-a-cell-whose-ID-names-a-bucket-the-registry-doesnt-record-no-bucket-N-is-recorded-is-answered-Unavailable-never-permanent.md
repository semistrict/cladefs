---
id: TASK-676
title: >-
  cladefs-gc retries forever a cell whose ID names a bucket the registry doesn't
  record: 'no bucket N is recorded' is answered Unavailable, never permanent
status: To Do
assignee: []
created_date: '2026-11-19 22:46'
labels:
  - bug
  - gc
dependencies: []
priority: medium
ordinal: 1058000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found under TASK-645: scripts/test-rustfs.sh's cladefs-gc over s3://cladefs/test ran 11+ minutes, logging 'bucket: reaching bucket 6377, for tenants/0000000000001/cells/1htck3582v2z8/head: no bucket 6377 is recorded' over and over, never finishing. The cell was bucket_cell's object_bucket::tests::rustfs's, named by a timestamp whose high bits read as bucket 6377 (fixed in TASK-645 to name its cells under identity::NAMED). But the collector's behaviour stands: ObjectBucket::member (crates/bucket_cell/src/object_bucket.rs) maps every failure of numbered(number), the registry's NotRecorded included, to Error::Unavailable, which callers retry as transient. A bucket the registry doesn't record will not appear by retrying; a cell ID naming one is either corrupt or another registry's. Decide what the collector (and other callers) should do with it: skip the cell, saying so, or fail the pass.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test: a cell whose ID names an unrecorded bucket, in the bucket collected; cladefs-gc finishes, saying what it skipped (or fails, naming it), never retrying it forever
- [ ] #2 Whether NotRecorded is permanent for every caller of member, not only the collector, decided and recorded here
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
