---
id: TASK-433.5
title: 'Store crash rounds run over the store''s page cache: a CachedDir over any Dir'
status: Done
assignee: []
created_date: '2026-11-15 12:14'
updated_date: '2026-11-15 20:32'
labels:
  - memory
  - test
dependencies: []
parent_task_id: TASK-433
priority: high
ordinal: 828000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-433's page cache (extent_store::pages) is wired into HostDir alone: its registry of files by name (create, open, remove, rename, link, host_path) lives in dir.rs, so the store's crash rounds (tests/crash, over MemDir) and Shuttle tests never run through it; only pages.rs's unit tests do (writes unsynced lost in a crash, flusher, cancel, concurrent writes and syncs). Move the registry into a generic pages::CachedDir<D: Dir>, HostDir built on it, and run the crash rounds and the simulation over CachedDir<MemDir>, so that a crash keeps what was synced and nothing the cache held dirty, as the host's page cache had it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Widened with the user (2026-11-15: 'all e2e tests should use this new cache'): no store runs around the cache. extent_store::CachedDir<D> wraps any Dir (HostDir = CachedDir<DiskDir>, MemDir = CachedDir<MemDisk> with a small flusherless cache); DiskDir refuses file systems that can't bypass (no fallback); direct flag removed. Bugs found by running crash rounds/Shuttle through it: a write-back unwound by a panic left pages in flight forever (InFlight guard); Shuttle's scoped threads wake the scope thread on a Condvar (TASK-465), avoided by per-file write-back on a mutex (I_SYNC); MemDisk reads past EOF panicked. Blind spot: the cache lived in one Dir impl, so fakes composed beside it, not under it; class closed by the wrapper every Dir is. Tests changed: commits_failing_part_way_fail_the_store (seal case fails at flush), writes_are_written_back_as_they_go (cache peak dirty). Verified: macOS clippy, workspace 2497/2497, Shuttle 37/37, extent_store 291/291 incl. sims; ubu1 workspace 2803/2803. Pushed 744df100e.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Every store's directory holds its data files in the page cache (CachedDir over any Dir); tests, crash rounds and Shuttle run through it; macOS and ubu1 green; pushed 744df100e.
<!-- SECTION:FINAL_SUMMARY:END -->
