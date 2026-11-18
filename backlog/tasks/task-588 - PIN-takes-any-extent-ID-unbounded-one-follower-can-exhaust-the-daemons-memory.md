---
id: TASK-588
title: >-
  PIN takes any extent ID, unbounded: one follower can exhaust the daemon's
  memory
status: To Do
assignee: []
created_date: '2026-11-18 15:29'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 979000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), follower-host-side review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/local_cell/src/views.rs`: `rg -n -F 'follower.pins.insert(extent);' crates/local_cell/src/views.rs`

The PIN request decodes any `ExtentId` (codec `PIN | UNPIN`). `Connection::handle` passes it to `OpenCell::pin`, which skips only cells routed to attached bases, and then to `View::pin`. `View::pin` inserts it into the view's `HashSet<ExtentId>` without checking that the view holds the extent (doc-4 says PIN is of an extent 'which the view holds') and without any count or byte bound. Nothing counts these entries against the tenant: `InFlight` counts stream bytes only, and pins live as long as the view. A VM process with a read-only connection to its own tenant's store can therefore send an unbounded stream of PINs with distinct made-up IDs (up to MAX_REQUESTS in flight), and every one succeeds. The daemon's heap grows by roughly 20-40 B per pin until the host's one daemon, which serves every tenant, runs out of memory. Each publish of that cell also rebuilds a HashSet of every pin (`Views::keeping_of`, under the writer's state lock), and `KeptExtents::views_bytes` walks them all. That per-commit cost is the already-reported views.rs finding; the new problem here is the unvalidated, unbounded input. The project bounds what a VM process may make the daemon hold (TASK-79.16, TASK-149.35, TASK-149.17); PIN is outside those bounds.

**Trigger.** A granted tenant's VM process (or the default tenant's) sends OPEN with OPEN_READ_ONLY for one of its cells -> Connection::follow -> cells::followed. It then sends PIN requests in a loop with ExtentId{cell, sequence: 1..N}, none of which needs to exist. Each one goes Request::Pin -> store.pin -> View::pin -> pins.insert and returns success. At round-trip rates, tens of millions of pins a minute grow the daemon's heap by GBs, and the daemon dies of OOM, taking every tenant's stores with it. Before that, every commit of the writer of that cell pays O(pins) under its state lock.

**Why the tests stay green.** Tests pin a few real extents the view holds: followers_read_their_views_commits, followers_follow_their_writer_side_by_side and followers_read_through_to_what_their_writers_have_attached. None pins an extent outside the view, pins many, or checks memory against a compromised VM process. The TASK-149.17 / 79.16 / 149.35 bound tests do not cover PIN.

**Backlog.** No related task covers it: TASK-42.2 (Done) bounds pinned bytes, not the pin count, and made-up IDs count 0 bytes; TASK-149.17, TASK-149.35 and TASK-79.16 (Done) bound other things a VM process can make the daemon hold; TASK-444.8 (To Do) is generic.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 PIN of an extent the follower's view does not hold (such as ExtentId{cell, sequence: 999}) fails with EIO (NotHeld) or ENOENT and adds nothing to the view.
- [ ] #2 A view's pins are bounded: past a fixed count, PIN fails or the connection is let go as past the protocol's bounds, view.pinned().len() stays at that bound, and the daemon's heap stays within the hostile harness's bound.
- [ ] #3 A regression test in cladefsd's tests follows a store, pins an extent its view does not hold expecting an error, then pins past the bound expecting refusal; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
