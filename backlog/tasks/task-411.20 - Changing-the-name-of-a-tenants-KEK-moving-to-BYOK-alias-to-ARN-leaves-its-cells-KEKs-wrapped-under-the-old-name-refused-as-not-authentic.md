---
id: TASK-411.20
title: >-
  Changing the name of a tenant's KEK (moving to BYOK, alias to ARN) leaves its
  cells' KEKs wrapped under the old name, refused as not authentic
status: To Do
assignee: []
created_date: '2026-11-15 03:58'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: low
type: task
ordinal: 818000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-411.17. A real backend's wrapped key names the key that wrapped it (AWS KMS: header 'fxka', the key's name as KeyNames gave it), and an unwrap refuses, as Unauthentic and asking the KMS nothing, a wrapped key naming another key than the tenant's current one (TASK-411.17 AC #3). So a tenant whose KeyNames name changes, e.g. a template alias replaced by an override naming its own key ARN in its account (BYOK adoption), or the same key named by ARN instead of alias, can no longer open its existing cells: each cell object's KEK generations stay wrapped under the old name. There is no way to rewrap a cell's KEK generations under a tenant's new key (keys::rotate adds a cell KEK generation and rewraps DEKs, but older generations stay wrapped under the old tenant KEK, TASK-411.11).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An operator can move a tenant to a new key name: every cell of the tenant gets its KEK generations rewrapped under the new key while the old one still unwraps, by a conditional update per cell, and stores open afterwards under the new name alone; a test over LocalKms or the AWS emulator shows it
- [ ] #2 doc-16 says how, and what happens to wraps under the old key name until then
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
