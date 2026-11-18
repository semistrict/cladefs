---
id: TASK-546
title: Uncounted MAPPINGS replies let one VM process exhaust the daemon's memory
status: To Do
assignee: []
created_date: '2026-11-18 15:24'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 937000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-protocol review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/cladefsd/server.rs`: `rg -n -F 'let held = requests.stream_bytes + bytes + self.peer.sender.queued();' crates/cladefs/src/cladefsd/server.rs`

`stream_bytes` (codec.rs:580) counts reply bytes only for READ, so a MAPPINGS request is admitted with 0 bytes. `Connection::within_bounds` only compares `requests.stream_bytes + bytes + sender.queued()` with `Peer::credit()` at the moment each message is admitted. `Peer::set_window` sizes that credit for exactly one window's mappings reply. Up to MAX_REQUESTS (4096) MAPPINGS can be admitted before the first reply exists. Each one served builds `window_mappings()` (a Vec of every mapping) and a payload of mappings × 48 bytes: 768 MiB at MAX_WINDOW. `Answer::reply` then pushes it into the AsyncSender queue with no limit, and `InFlight::add` counts it into the tenant's in-flight budget whether or not it fits. Credit is checked only when the next message arrives. A VM process that sends nothing more is never let go, because the watchdog only looks at unacknowledged stage operations. So neither the per-connection credit (TASK-79.16/79.43) nor the per-tenant bound (TASK-149.35) limits what the queue holds.

**Trigger.** A compromised VM process of a granted tenant does the following. (1) OPEN its own store, then WINDOW size=MAX_WINDOW (64 GiB) with private_max=1. (2) MAP_MANY payloads of MAP_ZEROS mappings at every other page, about 8M mappings in 6 requests of 64 MiB, acknowledging the stage ops honestly. MAP_ZEROS takes no private pages, so window_mappings does not return EBUSY. (3) Write 4096 MAPPINGS headers in one write, then stop reading and sending. The first 8 run on the workers and each takes a long time to build 8M mappings. Meanwhile the receive loop admits the other 4088 into `requests.waiting` within milliseconds (held is about 0 ≤ credit). Each one served then queues a 384 MiB reply: up to about 1.5 TiB. With an ordinary 2 GiB window it is 12 MiB × 4096 = 48 GiB. The daemon is OOM-killed and every tenant's VMs go with it.

**Why the tests stay green.** `window_mappings_are_answered_beside_the_stream_data_in_flight` (tests.rs) sends a single MAPPINGS. `what_requests_hold_is_within_the_credit` (hostile.rs) bursts roots, reads and writes, not MAPPINGS. In the hostile proptest a MAPPINGS is one request against a 1 MiB window with few mappings, so no case has many large replies queued.

**Backlog.** Related: TASK-79.43, TASK-150.3, TASK-149.35, TASK-79.16 and TASK-149.17 (all Done) bound one MAPPINGS reply, the window size, the tenant's in-flight bytes and admission. Each assumes one MAPPINGS in flight, and none checks queued replies again after admission.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Take a VM process with a window holding about 100k MAP_ZEROS mappings. If it sends 256 MAPPINGS requests at once and never reads the replies, the daemon's heap peak stays within credit(window) plus slack: later MAPPINGS wait at admission, or the connection is let go.
- [ ] #2 When the same burst runs on 64 connections of one tenant, that tenant's in-flight peak stays at or under its budget.
- [ ] #3 A test in the cladefsd hostile.rs suite runs both bursts and asserts both bounds. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
