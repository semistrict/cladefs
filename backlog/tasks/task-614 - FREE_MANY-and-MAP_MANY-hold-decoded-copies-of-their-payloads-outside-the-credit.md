---
id: TASK-614
title: >-
  FREE_MANY and MAP_MANY hold decoded copies of their payloads outside the
  credit
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:33'
updated_date: '2026-11-19 16:27'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1005000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-protocol review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/cladefsd/codec.rs`: `rg -n -F "let rest: Cow<'a, [ExtentId]> = Cow::Owned(extent_ids(payload)?.collect());" crates/cladefs/src/cladefsd/codec.rs`

`Request::decode` turns a FREE_MANY payload into a Vec<ExtentId> (16 B per 16 B of payload) and a MAP_MANY payload into a Vec<WindowMapping> (48 B per 48-byte mapping) via `mappings(payload)` at codec.rs:472. Both are held while `serve_in_turn` keeps the message's payload alive. Each such request therefore holds twice the stream data that `within_bounds`, `Peer::credit` and the tenant's InFlight count. With MAX_STREAM_BYTES (256 MiB) of 64 MiB payloads, four at once per connection, that is about 256 MiB more than the 272 MiB credit, and up to twice the tenant's in-flight budget across its connections. Every other request decodes by borrowing its payload, as WRITE's `Data::Inline` does.

**Trigger.** A VM process pipelines FREE_MANY (or MAP_MANY) requests with 64 MiB payloads, four in flight at a time, on many connections of one tenant. The daemon holds 2× the bytes counted in flight.

**Why the tests stay green.** `what_requests_hold_is_within_the_credit` bursts roots, reads and writes only. The hostile proptest's FREE_MANY payloads hold at most 8 extents, and there are no large MAP_MANY payloads.

**Backlog.** Related: TASK-149.17 (Done) fixed this class for READ, WRITE and ROOT only. TASK-150.3 and TASK-149.35 (Done) cap FREE_MANY's length and the tenant's in-flight bytes. TASK-513 (To Do) is a flaky write burst in the same test.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When a VM process pipelines FREE_MANY or MAP_MANY requests with 64 MiB payloads, four in flight per connection, the daemon holds at most credit(0) + MAX_PAYLOAD per connection and at most the tenant's in-flight budget across its connections. OPEN and FORK requests with paths up to 64 MiB hold no uncounted copy either.
- [ ] #2 what_requests_hold_is_within_the_credit in hostile.rs gains FREE_MANY and MAP_MANY bursts of 64 MiB payloads, asserts held <= credit(0) + MAX_PAYLOAD, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
stream_bytes counts what decoding holds besides the payload: a FREE_MANY twice its payload, a MAP_MANY MAP_MANY_HELD (4) times, decoded mappings and the window's pieces; paths past PATH_MAX refused (ENAMETOOLONG) before they are copied. The client counts the same, as it shares stream_bytes. Tests: codec many_requests_count_what_they_decode and map_manys_count_what_they_hold (a mutant dropping the counting fails the first). Left: AC1/AC2's heap bursts of 64 MiB FREE_MANY and MAP_MANY in what_requests_hold_is_within_the_credit. macOS: clippy, workspace 2718/2718, sim 41/41; Linux (ubu1, 9cca798f0): workspace 3081/3081, scripts 528 tests; hostile proptest 5 rounds passing; hand mutants (target/b3-linux-mutants.py on Linux, target/b2-mutants.py on macOS) each caught by its test. Wire version 12.
<!-- SECTION:NOTES:END -->
