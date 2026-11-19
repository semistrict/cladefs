---
id: TASK-563
title: >-
  A failed MAP_MANY completes while its other MAPs run; a late one can lose
  writes
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:26'
updated_date: '2026-11-19 06:43'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 954000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), virtio-host review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/virtio_extent/src/requests.rs`: `rg -n -F 'futures::future::try_join_all(maps.iter().map(|(args, flags)| async move {' crates/virtio_extent/src/requests.rs`

The device turns a guest MAP_MANY into one daemon MAP per piece and sends them all at once. It then awaits them with try_join_all, which returns as soon as any piece fails and drops the others. Their daemon requests are already sent and keep running (the daemon serves 8 of a connection's requests at a time, server.rs WORKERS, and queues the rest). So the guest sees the request complete while some of its window changes have not happened yet. The guest assumes the opposite. dax.c's `unsure` comment says a failed MAP_MANY may have mapped some pieces, and the guest then re-maps each piece one by one and treats the window state as settled. A stale MAP that runs late goes through OpenCell::map, then Window::map_as with Private::Free, which replaces whatever is at that window offset by then: a private page made after the retry, or another file's mapping placed after the range was UNMAPped and its window space reused. The daemon already has a request that keeps the guest's contract: map_many maps every piece before the failing one and only then replies.

**Trigger.** 1. A forked VM writes back and rebinds pages. cladefs_dax_rebind_range calls cladefs_dax_map_all with rebind=true and sends one MAP_MANY covering P0 (backed by extent E0, still open) and P1 (backed by E1, sealed). 2. handle() sends MAP(E0→P0) and MAP(E1→P1) to the daemon at once. 3. The daemon answers P0 with EPERM right away (refuse_open_map). MAP(P1) is still in mapped_of or waiting for the dax lock, for example because a pool thread was preempted on a busy host. 4. try_join_all returns the error and the request completes with EPERM. 5. The guest retries piece by piece: P0 gets EPERM again and is skipped; P1 gets a new MAP, which succeeds; cladefs_dax_mapped marks P1 as no longer private; the file's semaphore is released. 6. The guest stores to P1, and a PRIVATE request or copy-on-write fault gives P1 a private page holding the new bytes. 7. The original MAP(E1→P1) now takes the dax lock and maps E1 over P1 with Private::Free. The private page and the store are gone. The guest's dirty set still lists P1, so writeback reads E1's old bytes from the window and writes them back: the store is lost without any error. A second path: a range map fails, the range is marked unsure and UNMAPped, and its window space goes to another file. A stale MAP landing after the new owner's MAP shows the first file's bytes in the second file's page.

**Why the tests stay green.** FakeBackend's async methods never await, so every piece finishes on its first poll and try_join_all never has a sibling still running when one fails. tests/daemon.rs only sends MAP_MANY with bad flags or oversized data. The guest's dax_fail_map_many and dax_fail_fallbacks inject failures inside the guest, never at the device. With the daemon failpoint cladefsd::map, the other pieces finish within microseconds, well before the guest's retry.

**Backlog.** New. TASK-91.18 (archived) kept the concurrent fan-out after a hang the guest driver caused; TASK-119.13 (Done) adds guest-side failpoints only; TASK-92.14 (Done) is the daemon's MAP_MANY for restore. None covers MAPs still running after the request completes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A MAP_MANY that fails completes to the guest only after every daemon MAP it sent for its pieces has finished, as the daemon's own map_many does; no MAP of a completed MAP_MANY changes the window afterwards.
- [ ] #2 Under the trigger (piece 0 fails EPERM at once, piece 1's MAP is held), the guest's piece-by-piece retry and its later store instruction to P1 survive: P1's private page is not replaced by E1, and no range placed in reused window space shows another file's bytes.
- [ ] #3 A requests.rs test with a Backend that fails piece 0 with EPERM at once and holds piece 1 on a oneshot polls handle() by hand: it stays Pending after piece 0 fails and is Ready with EPERM only once piece 1's map is recorded. It goes Ready early with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
