---
id: TASK-516
title: A failed delete can become durable
status: To Do
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
- [ ] #1 After an injected extent-tree read failure during delete, the file and its contents remain accessible immediately and after another file commits and the file set reopens. All device extents are accounted for.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
