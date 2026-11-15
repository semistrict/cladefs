---
id: TASK-433.5
title: 'Store crash rounds run over the store''s page cache: a CachedDir over any Dir'
status: To Do
assignee: []
created_date: '2026-11-15 12:14'
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
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
