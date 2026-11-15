---
id: TASK-411.22
title: >-
  A misnamed key ring in FXDAX_KMS=gcp-kms:// reads as every template tenant
  having no KEK: their new cells made in the clear
status: To Do
assignee: []
created_date: '2026-11-15 04:04'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: medium
type: bug
ordinal: 818000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found building the Google Cloud KMS backend (TASK-411.18). Cloud KMS answers a missing key ring exactly as a missing key: NOT_FOUND 'CryptoKey NAME not found.' The backend maps that to kms::Error::NoSuchTenant (AC #2 of TASK-411.18), and a host makes the cells of a tenant with no KEK in the clear (doc-16). A typo in the URL's ring (or the wrong location) so silently turns encryption off for every tenant named by the template. Overrides naming other projects fail closed (Project not found is Io). LocalKms has the like hazard (an empty directory), but a production KMS should fail closed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A host whose gcp-kms:// URL names a key ring that does not exist refuses to make cells (Io or a startup error naming the ring), rather than making them in the clear
- [ ] #2 Tested against real Cloud KMS: a URL with a missing ring fails closed, the right ring still gives NoSuchTenant for a missing key
- [ ] #3 Whatever IAM the check needs on fxdax's own key ring is documented in doc-16
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
