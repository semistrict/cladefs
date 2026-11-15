---
id: TASK-463
title: >-
  CI flake: tests walking a store's directory fail when a file goes between
  listing and opening it (sealed_images_hold_no_plaintext,
  what_an_uploader_keeps_stays_until_acknowledged)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 19:02'
updated_date: '2026-11-15 20:12'
labels:
  - test
  - flaky
dependencies: []
priority: medium
ordinal: 871000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43729753505 (97e5befac), test (workspace): 2 of 2797 failed, both NotFound on a file the walk had just listed.

- cladefs storage::image::tests::sealed_images_hold_no_plaintext panicked at crates/cladefs/src/storage/image.rs's in_the_clear walk: std::fs::read(&at).unwrap() -> Os { code: 2, NotFound }.
- extent_store::store what_an_uploader_keeps_stays_until_acknowledged panicked in crates/extent_store/tests/store.rs's data_files: dir.open(...).unwrap() -> NotFound "segments/0000000000000007/framing-0000000000000027 doesn't exist".

The store removes files in the background (cleaning, write-back, framing) while the test lists then opens them. Either the walk must tolerate a file gone since the listing, or the test must quiesce the store first; decide which the test means to assert. Not from the rename (TASK-437): text only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Both tests no longer fail when a listed file is removed before it is opened
- [x] #2 Each passes 200 runs in a row on ubu1 (nextest --retries 0)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Quiesce, not tolerate, where the test holds the store: Store::wait_idle (no framing, nor its durability/rename, nor the next log's preparation, nor look-ahead); data_files waits for it; the image test waits for the uploader (wait_uploaded), the store, and the collector's pass.
2. One shared walker, cladefs::testing: files_holding (strict: a file gone fails) for quiesced/closed dirs; files_holding_beside (re-walks until a pass reads all it listed, keeping every pass's finds) for daemon tests that cannot wait.
3. Sweep: control.rs and sqlite_extents daemon.rs (live daemon) -> beside; fuse.rs, fuse_mount.rs, sqlite image.rs, remote.rs's shred test (closed stores) -> strict helper.
4. Prove: fail point segments::making_durable holds the rename, released in the list->open window: fails unfixed, passes fixed; Mac 300 runs each under load; ubu1 200 runs each, nextest --retries 0.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 19:10 UTC, coordinator (TASK-336/400 lane): taking it; a delegate fixes it.

Cause: segments::make_durable renames framing-N to N on the store's durable thread after a commit/uploader closing; data_files and the image walk listed framing-N, then opened it after the rename (NotFound). Image test also races the uploader (acks remove files), the opener (spare log renamed) and the collector.
Forced window (temporary tests, fail point segments::making_durable held, released between listing and opening): unfixed both fail with the CI errors (store: 'segments/0000000000000007/framing-0000000000000000 doesn't exist'; image: Os NotFound on .../segments/<branch>/framing-0000000000000000); fixed both pass.
Mac natural rate unfixed: image 0/240 under load. Fixed: 300/300 each under load (16 parallel + 6 CPU hogs), 0 failures.

ubu1 (247a2a85, fxjob, own target): nextest --stress-count 200 --retries 0 --test-threads 4, both tests each iteration: 200/200 iterations, 400 passes, 0 failures (456 s, beside other jobs). Swept and touched tests on Linux (control, fuse_mount, fuse, sqlite_extents daemon and image, testing::, shredded/sealed, extent_store's store binary): 175/175 passed; clippy -D warnings clean on Linux and the Mac. Mac workspace (scripts/test.sh workspace part, --retries 0): 2500/2500 passed. scripts/upstream-changes.py --check passes (nothing upstream touched). Other walks swept and left as they are: encrypted.rs's (MemDir snapshots, and a part cache written synchronously), extents.rs's bucket walk and remote.rs's store-file reads (after commits that wait for their uploads), kept.rs bucket_bytes, storage.rs allocated, gc.rs walk (closed or synchronous); segment_data skips framing- names and opens only names never removed in the background.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Cause: the store's durable thread renames framing-N to N (segments::make_durable) after a commit or uploader closing; data_files and the image test's walk listed framing-N and then opened it after the rename (NotFound). Fix: Store::wait_idle (no framing, durability rename, log preparation or look-ahead running); data_files waits for it; the image test quiesces (uploader wait_uploaded, store idle, collector pass) before each walk. New cladefs::testing walker replaces seven copies: files_holding (strict) for still dirs; files_holding_beside (walks again until a pass reads all it listed, keeping every pass's finds) for live-daemon tests (control.rs, sqlite daemon.rs). Verified: a fail point holding the rename, released between listing and opening, fails both tests as in CI without the fix and passes with it; Mac 300/300 each under load; ubu1 200/200 stress iterations with --retries 0.
<!-- SECTION:FINAL_SUMMARY:END -->
