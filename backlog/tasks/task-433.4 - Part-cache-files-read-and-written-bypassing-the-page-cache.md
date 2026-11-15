---
id: TASK-433.4
title: Part cache files read and written bypassing the page cache
status: Done
assignee: []
created_date: '2026-11-15 10:27'
updated_date: '2026-11-15 10:48'
labels: []
dependencies: []
parent_task_id: TASK-433
ordinal: 827000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
extent_remote::cache reads and writes its sparse object files with plain buffered I/O, so parts it keeps on disk are also held in the OS page cache: cached twice. Parts are stored 4 KiB-aligned for exactly this; read and write them with O_DIRECT (F_NOCACHE on macOS).
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The part cache reads and writes its files bypassing the page cache (extent_remote::direct). Verified: macOS gate; ubu1 ext4: kept_objects_hold_no_pages_in_the_page_cache, cache tests passing.
<!-- SECTION:FINAL_SUMMARY:END -->
