---
id: TASK-629
title: The simulation ignores uploader failures that only a bug can cause
status: To Do
assignee: []
created_date: '2026-11-18 15:36'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1020000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), local-io review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/local_cell/src/sim/world.rs`: `rg -n -F 'if let Failure::Bucket(Error::Lost(_) | Error::Corrupt(_)) = failure {' crates/local_cell/src/sim/world.rs`

`World::observe` ends a session silently whenever its uploader failed, unless the failure is `Lost` or `Corrupt`. `check_uploaded` and `check` both skip sessions whose uploader failed. The simulation injects no local disk faults: there are no `fail_after` calls, and crashes abort tasks rather than fail them. So a `Failure::Local` or `Failure::Bucket(Error::Read)` from a running uploader can only come from a bug in the store or upload path. Examples are `LocalCell::acknowledged` bailing with "is not the first of the write-ahead log", errors from `gather`'s `flush`/`closed`, a failed restamp in `Closed::bytes` after a takeover, or a CRC failure from `read_object`. In `quiesce`, faults are off and there are no takeovers or deletions, so any uploader failure there is suspect, yet it is not flagged either. In production, such a failure stops the cell's uploads for good. Commits waiting for COMMIT_UPLOADED fail, and once the lag bound is reached every commit fails, until the store is reopened.

**Trigger.** Suppose a regression makes the restamp of a write-ahead-log segment fail after a takeover (`Closed::bytes`, which calls `extent_frame::restamp`), for example for footers with `continued` or versions. 1) In the `wals` profile, a host restarts and `start_session` opens the store, whose log the new writer restamps. 2) At the first upload, `Writer::create` gets a failed `Body::read` and returns `Error::Read`. The upload future fails and `Shared::fail` records `Failure::Bucket(Error::Read)`. 3) `observe()` sees `failed()` and calls `self.end()` without a violation. 4) In `quiesce`, the session is restarted and fails again the same way. `check_uploaded` skips it because `uploader.failed().is_some()`, and the commits it never uploaded were never acked, so `check()` finds nothing. Every `test_holds!` profile stays green.

**Why the tests stay green.** The oracle treats every non-Lost/Corrupt uploader failure as a legitimate end of a session, as if it were a takeover or a deletion. Its liveness check (`check_uploaded`, TASK-149.32) excludes failed uploaders by construction. So a bug that makes the uploader stop, rather than lose data, is invisible across all seeds.

**Backlog.** Related to TASK-149.32 (Done), which added the simulation's liveness checks but leaves failed uploaders out of them by construction.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The simulation in crates/local_cell/src/sim reports a violation when a running uploader fails with Failure::Local or Failure::Bucket(Read, NoKey or Format), and when any live cell's uploader fails while quiescing.
- [ ] #2 With a hand mutant that makes Closed::bytes fail on restamped segments (or makes LocalCell::acknowledged bail on its second call), the logs_put_after_takeovers and everything profiles go red. With the oracle change reverted, they pass again despite the mutant.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
