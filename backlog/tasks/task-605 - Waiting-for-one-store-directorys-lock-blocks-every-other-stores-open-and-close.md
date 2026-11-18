---
id: TASK-605
title: >-
  Waiting for one store directory's lock blocks every other store's open and
  close
status: To Do
assignee: []
created_date: '2026-11-18 15:32'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 996000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), local-io review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/local_cell/src/dir.rs`: `rg -n -F 'take_from_others(&self.handle)?;' crates/local_cell/src/dir.rs`

`DiskDir::take` locks the global `TAKEN` map, then calls `take_from_others`. That function polls `flock(LOCK_EX|LOCK_NB)` with 20 ms sleeps for up to `TAKE_WAIT` (10 s) without dropping the guard. `TAKEN` is shared by every store directory in the process, and `let_go` needs it as well. So while one store waits on a directory that another process holds, the daemon cannot open, make or delete any other store, because `recover_unmade`, `make_copy` and `delete_host` all call `take`. It also cannot finish closing any store: `TakenDir::drop` and `DirHold`, at the end of `Uploader::drop`, call `let_go`, which blocks on `TAKEN` on whatever thread drops the last hold.

**Trigger.** 1) A local `cladefsctl` holds store X's directory flock. TASK-43.8 names this case, and a daemon restart overlapping an old process that is still draining does the same. 2) A daemon request opens or deletes X: `recover_unmade` or `delete_host` calls `HostDir::take`, which takes `TAKEN` (dir.rs:320) and polls in `take_from_others` for up to 10 s. 3) During that time, a VM opening store Y blocks in `Y.take()` on `TAKEN`. A VM closing store Z blocks in `Z.let_go()` (dir.rs:338) when its `Uploader`/`LocalCell` drops, and the thread doing that drop is held for up to 10 s. 4) Every take and let_go in the process stalls for the full wait, even though only X is contended.

**Why the tests stay green.** `directories_are_taken_one_at_a_time` (crates/local_cell/tests/host.rs) contends a single directory and checks only that the wait happens. No test takes or releases a second directory while the first is waiting.

**Backlog.** Related to TASK-43.8 (To Do), which takes the daemon's 10 s wait off a blocking thread but keeps other callers waiting and never names the process-wide mutex. TASK-43.6 (Done) left the wait in place.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 While one store directory waits for a lock another process holds, taking and letting go of a different store directory each return within 100 ms.
- [ ] #2 Two takings of the same directory within the process still never hold it at once.
- [ ] #3 A test in crates/local_cell/tests/host.rs flocks directory X from a separate fd and starts HostDir::new(X).take() on a thread. On the main thread it then takes and lets go of directory Y and asserts both finish within about 100 ms while X is still waiting. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
