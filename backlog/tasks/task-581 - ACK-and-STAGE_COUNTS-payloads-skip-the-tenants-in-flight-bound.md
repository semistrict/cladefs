---
id: TASK-581
title: ACK and STAGE_COUNTS payloads skip the tenant's in-flight bound
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:28'
updated_date: '2026-11-19 07:06'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 972000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-protocol review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/cladefsd/server.rs`: `rg -n -F 'if [ACK, STAGE_COUNTS, STAGE_PRIVATE_COUNTS].contains(&header.kind) {' crates/cladefs/src/cladefsd/server.rs`

`within_bounds` returns Ok at once for ACK, STAGE_COUNTS and STAGE_PRIVATE_COUNTS without counting their bytes. `AsyncReceiver::receive` still allocates `vec![0; header.len]` (up to MAX_PAYLOAD+64) and reads the whole payload, which `Made::decode` then refuses because these kinds carry no payload. A VM process that sends all but the last byte of such a payload keeps up to 64 MiB resident in that connection's receive task forever: reads have no timeout, and the watchdog is quiet without unacknowledged stage ops. None of it is in the tenant's InFlight or process.rs's budget. With MAX_CONNECTIONS (1024) a tenant holds 64 GiB, against the 8 GiB default (or a smaller configured) per-tenant bound that TASK-149.35 added. Ordinary requests are made to wait in `take` before their payload is read.

**Trigger.** A compromised VM process opens 1024 connections to its tenant's socket. On each it writes an ACK header with len = MAX_PAYLOAD + 64, then MAX_PAYLOAD + 63 bytes, and stalls. The daemon holds about 64 GiB, uncounted, and runs into OOM or governor pressure that affects every tenant.

**Why the tests stay green.** The hostile proptest's ACKs (`abusing`) always use `Payload::Bytes(Vec::new())`. Only the `anything` branch can pair ACK with at most 64 bytes, and with 1-3 connections the heap limit (credit(0) per connection + SLACK) absorbs 64 MiB anyway. `a_tenants_in_flight_data_is_bounded` floods with READs only.

**Backlog.** Related to TASK-149.35 (Done), which bounds requests' stream data per tenant but never mentions the payloads of kinds that get no answer, and to TASK-79.16 (Done), whose e6cf07d9b added the skip.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Eight connections of one tenant each send an ACK header with len = MAX_PAYLOAD and all but the last byte of its payload. The extent daemon's heap peak stays within the tenant's in-flight budget plus slack.
- [ ] #2 A header with a nonzero length on ACK, STAGE_COUNTS or STAGE_PRIVATE_COUNTS, kinds that carry no payload, ends its connection before any of the payload is held.
- [ ] #3 The eight-connection test sits beside a_tenants_in_flight_data_is_bounded in crates/cladefs/src/cladefsd/tests.rs, and `abusing` in crates/cladefs/src/cladefsd/tests/hostile.rs generates these kinds with payloads. Both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
