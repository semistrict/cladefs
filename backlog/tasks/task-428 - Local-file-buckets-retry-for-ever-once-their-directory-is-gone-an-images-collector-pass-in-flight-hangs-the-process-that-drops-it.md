---
id: TASK-428
title: >-
  Local file buckets retry for ever once their directory is gone: an image's
  collector pass in flight hangs the process that drops it
status: To Do
assignee: []
created_date: '2026-11-15 06:08'
updated_date: '2026-11-15 11:37'
labels:
  - bug
dependencies: []
priority: low
ordinal: 819000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found on ubu1 2026-11-15 (cladefs storage::image::tests::flipped_bytes_fail_reads hung 39 min): the test's TempDir dropped before its ImageDevice, deleting the image's file bucket while a background collector pass (storage/collect.rs, 'cladefs collect' thread) was in flight. Local::lock's open of <dir>/.cladefs-lock.NN failed ENOENT, reported as a generic object_store error, which the bucket helpers treat as transient and retry for ever (every 5 s, strace), and Collecting's Drop joins the pass: the process never exits. The test's helper now returns the directory first so the device drops before it (fixed with this task's filing). Left: a bucket whose directory is gone is no transient failure: Local should fail such requests for good (or Collecting's Drop should stop a pass rather than wait on it), so that a process closing an image whose directory was removed exits.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
