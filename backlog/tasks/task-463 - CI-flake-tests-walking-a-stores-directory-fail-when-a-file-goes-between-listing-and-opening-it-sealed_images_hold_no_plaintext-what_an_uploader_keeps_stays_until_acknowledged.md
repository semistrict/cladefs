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
updated_date: '2026-11-15 19:03'
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
- [ ] #1 Both tests no longer fail when a listed file is removed before it is opened
- [ ] #2 Each passes 200 runs in a row on ubu1 (nextest --retries 0)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 19:10 UTC, coordinator (TASK-336/400 lane): taking it; a delegate fixes it.
<!-- SECTION:NOTES:END -->
