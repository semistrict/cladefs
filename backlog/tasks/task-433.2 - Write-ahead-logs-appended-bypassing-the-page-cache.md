---
id: TASK-433.2
title: Write-ahead logs appended bypassing the page cache
status: Done
assignee: []
created_date: '2026-11-15 10:27'
updated_date: '2026-11-15 10:48'
labels: []
dependencies: []
parent_task_id: TASK-433
ordinal: 825000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Logs are appended a record at a time and synced keeping their pages, so an append need not read its last partial page back (TASK-301). Keep that page in memory and write whole blocks with O_DIRECT.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Logs and the store's small files bypass the page cache (HostFile::small over extent_remote::direct, a log's last block held in memory). Verified: macOS gate 2489/2489; ubu1 ext4: crates' tests passing, the open segment's log holds no cached pages.
<!-- SECTION:FINAL_SUMMARY:END -->
