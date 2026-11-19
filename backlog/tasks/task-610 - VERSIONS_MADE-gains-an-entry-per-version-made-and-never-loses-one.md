---
id: TASK-610
title: VERSIONS_MADE gains an entry per version made and never loses one
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:33'
updated_date: '2026-11-19 07:06'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1001000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cells review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/host_cell.rs`: `rg -n -F 'VERSIONS_MADE.lock().unwrap().insert((cell, version), path.to_owned());' crates/cladefs/src/host_cell.rs`

`OpenCell::make_version` calls `host_cell::made_version`, which inserts `(CellId, version) -> PathBuf` into the process-wide static `VERSIONS_MADE` BTreeMap. Nothing removes entries: not `drop_versions`, not the drops a later version's `keep_last` makes at acknowledgement, not deleting the store or its cell. The only reader is `version_made_of` in cladefsd's `opened_version`, which needs an entry only while the version still exists. The map therefore grows with every VM snapshot the daemon ever made. That memory sits outside the governor's single budget, which the AGENTS.md rules require everything the daemon holds to stay under.

**Trigger.** The VM manager snapshots VMs periodically: control API MakeVersion goes to `cells::make_version`, then `with_store`, then `OpenCell::make_version(keep_last)`, then `host_cell::made_version`. Each call adds one entry of about 150 bytes (a 24-byte key, a PathBuf header and a ~60-byte path, plus B-tree node overhead). `keep_last` drops the old versions in the bucket, but their entries stay. Example: 500 VMs snapshotted every 5 minutes is 144k entries a day, about 20 MB/day, roughly 2 GB over a 90-day uptime. The leak is unbounded and grows with the API call rate, which doc-18's "fast, cheap" versions encourage.

**Why the tests stay green.** Tests make a handful of versions per process, and none checks the map's size or that dropped versions leave it. Memory budget checks in the qualifications don't run snapshot loops long enough for the growth to show.

**Backlog.** No related task.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Once a version is dropped (by DropVersions or by keep_last) or its store or cell is deleted, version_made_of returns None for it. The map never holds more entries than versions the daemon's open heads still name.
- [ ] #2 A unit test in host_cell or open_cell makes many versions with keep_last 1 and drops one with drop_versions. It asserts no dropped version is still in the map and that its length stays at or under the head's versions, and it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
