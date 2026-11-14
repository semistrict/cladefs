---
id: TASK-411.9
title: >-
  Recovery frames a segment it closes again in the clear before the store's keys
  are known, resealing it once the store opens: plaintext on the host's disk
  meanwhile
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 23:22'
updated_date: '2026-11-14 23:52'
labels: []
dependencies:
  - TASK-411.3
parent_task_id: TASK-411
priority: high
type: bug
ordinal: 807000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.3: WAL recovery runs before the store's ring of keys is built, so a segment it closes again is framed in the clear and resealed (Dir::replace) once the store opens. Until then, and for good if the host dies between, a sealed branch's data sits on local disk in the clear, which shredding (doc-16 goal 2) must not leave. Found by TASK-411.3's implementer.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 No framed file of a sealed branch is ever written in the clear: recovery has the store's keys before it frames, or frames nothing until it has them
- [ ] #2 A test crashes a sealed store with a segment to close again and finds no plaintext of its data in any file of the store's directory at any point of its reopening
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. wal::recover frames nothing: a segment it closes again becomes a Closing (its footer, computed and stamped provisionally as close_again does now, and the log records it kept), its raw file and log kept until it is framed; torn framed files, and the segments after the last kept, removed as now. Recovered lists the closings beside the segments; Recovering's answers (follows, holds_commit, base) are unchanged, as they read footers alone.
2. Recovering::open frames the closings first, holding Opening.keys: re-reads each one's log and raw file, checks the records are those recovery kept, frames it sealed under keys.sealing() if the ring seals the branch, else in the clear (bytes as today), syncs it, syncs the directory of framed files, then removes its raw file and log (as settled did). A crash before leaves the raw file and log, which the next recovery closes again the same way (idempotent: the state recovery reached before close_again). drop_wal removes the closings' raw files and logs, durably, before it writes the base.
3. wal::reseal goes, and Dir::replace with it if nothing else uses it.
4. Tests: a sealed store crashed with its open segment holding durable commits, reopened with the file system failing after each operation in turn (MemDir::fail_after) until it opens: no file of its directory holds plaintext of the data at any point (open segments' raw files and logs excepted until TASK-411.5 encrypts them, then included); the reopened store reads the data and uploads a sealed segment. Clear twin: the framed file closed again is byte-identical to what close_again made before. Existing wal, crash, store, shuttle and simulation tests pass.
5. doc-16 and doc-14 say recovery frames nothing before keys.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Chose 'frames nothing until it has the keys' over 'keys before recovery': getting keys first would make every caller of Store::recover (extentd's stores.rs, host.rs, the sim, tests) read the record and cell before recovering, and with TASK-411.5 recovery reads only stored bytes anyway, so all key-holding work (framing; later MAC checks and decryption) sits at Recovering::open, where Opening.keys arrive.
wal::recover keeps its decisions and removals as they were; a segment it closes again gets its footer (closed_again: folded, stamped provisionally, as close_again did) and an entry in Recovered::closing (its kept records); its raw file and log stay. Recovered::close (first thing in Recovering::open) re-reads log and raw data, checks the records are still those kept, frames sealed under keys.sealing() or clear (bytes as before), syncs each, syncs segments/, then removes raw files and logs. Deferring is idempotent: the directory is as close_again's crash point left it, which recovery already handles (later segments settled meanwhile keep their footers; the next recovery finds the closing torn again). drop_wal removes closings' raw files and logs durably (else a later recovery would close commits fxdaxctl take dropped again). wal::reseal and Dir::replace (its only user) removed. The opening reads a closing's raw file twice (recovery, then framing): raw_files_are_read_through_the_page_cache expects 15 blocks, not 14; kept rather than holding a segment's data in memory across the uploader's bucket round trips.
Tests: wal::tests::segments_closed_again_are_framed_under_the_keys_alone (no framed file after recover, raw/log kept, recovered again same; close with a sealing ring frames sealed, raw/log gone); upload::encrypted::segments_closed_again_are_never_in_the_clear (sealed store crashed with a durable commit in its open segment; reopened with MemDir::fail_after(n) for every n until it opens; MemDir::files() searched after each: no plaintext; reads back; bucket segments sealed). Verified it fails if close frames in the clear. Until TASK-411.5 it skips open-segments/, logs/, open-extents/.
Runs (macOS): extent_store all but sim 215/215 (crash tests included); shuttle 37/37; sim::one SIM_SEEDS=10 everything, crashes, wals, forked_wals pass; fxdax sealed/reopen/crash/recover filter 11/11. clippy: no warnings in changed lines.
<!-- SECTION:NOTES:END -->
