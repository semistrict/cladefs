---
id: TASK-516
title: A failed delete can become durable
status: In Progress
assignee: []
created_date: '2026-11-18 12:23'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 906000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 1.

In `crates/extent_files/src/file_set.rs:518`, delete removes the file from the live map before reading its extent tree. A read error at line 526 returns to the caller with the file already absent. The next successful commit of another file builds the root from that map at line 575, making the deletion durable and omitting the removed file's extents from reclamation. Keep the file in the live map until the replacement root commits. Verify that an injected tree-read failure leaves the file readable both immediately and after committing a different file and reopening.

Isolated arm64 macOS validation. Commit and close a 300-page file on FakeDevice. Fail one extent-tree read while deleting. Inspect a snapshot before and after syncing an unrelated file. Repeat without injection. With the error, the live name disappeared immediately. The first snapshot still contained it. The second snapshot omitted it and held two extents with just one reachable or pending reclamation. The successful-delete control left one extent, accounted for.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 After an injected extent-tree read failure during delete, the file and its contents remain accessible immediately and after another file commits and the file set reopens. All device extents are accounted for.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The storage audit combined a failed tree read with an unrelated commit. The crash suite had checked the old durable root without exercising the next successful commit from the live map. The file_operations regression now checks the name in both views, recovered contents and device extent accounting. Delete prepares a replacement root while the file still owns its directory entry and mapping. The same publication boundary also covers failure writing a dying list. Ordinary file sync already applies its plan after the root commit.

The unchanged regression failed in the macOS audit run and passed against the implementation. Both implementation reviews reported no findings.
The regression also passed on ubu1 in run r516e against 24c6081a6 with the production patch. All 19 extent_files tests passed on both platforms. The macOS crate run passed 412 of 413 (TASK-530 failed), and its workspace run stopped on TASK-521. The ubu1 crate run passed 681 of 684 (TASK-519, TASK-520 and TASK-522 failed, as in the baseline audit). Workspace qualification remains open.
<!-- SECTION:NOTES:END -->
