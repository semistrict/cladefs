---
id: TASK-411.25
title: >-
  The real KMS backends share one wrapped-key header (kms/real.rs): AWS's fxka,
  OpenBao's fxkm and GCP's fxkr are three formats of one idea
status: To Do
assignee: []
created_date: '2026-11-15 09:47'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: low
type: task
ordinal: 820000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.16, .17 and .18 were built at once and each made its own header for what it wraps: magic, format, backend, (tenant,) key name, then the KMS's ciphertext. AWS (fxka) and GCP (fxkr) name no tenant, OpenBao (fxkm) does and sends the header as associated data. TASK-411.18 made kms/real.rs for exactly this; the others kept theirs private to avoid conflicts. One format and one parser make the rule (refused locally if another backend, tenant or key wrapped it) one piece of code, and doc-16 one table.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 AWS KMS, OpenBao and Google Cloud KMS wrap under one header format from kms/real.rs, naming backend, tenant and key name; each binds the header (or tenant) to its ciphertext as its KMS allows (encryption context, associated data, AAD)
- [ ] #2 A key wrapped by one backend, tenant or key name is refused as Unauthentic by every other without a call: one test over all three
- [ ] #3 doc-16 describes the one format; the backends' tests (emulator, dev server, real Cloud KMS) pass
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
