---
id: TASK-607
title: A SQLite file set's window faults hang forever once its daemon is lost
status: To Do
assignee: []
created_date: '2026-11-18 15:32'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 998000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), unsafe-atomics review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/sqlite_extents/src/daemon.rs`: `rg -n -F '.and_then(|daemon| daemon.open_sqlite(store, capacity));' crates/sqlite_extents/src/daemon.rs`

Reads of a file set's window (`extent_files::pages` copying from the region through raw pointers, and SQLite dereferencing `x_fetch` pointers) are plain memory accesses into a DAX window that the in-process uffd second stage serves. When the daemon connection is lost, `Client` marks `lost` and fails every request. `UffdSecondStage::resolve_through`, finding the fault unresolved and `window.lost()`, awaits `std::future::pending()` forever (dax/uffd.rs). The design behind that wait is that "the VM process is stopping": a VMM registers `on_lost` (virtio_extent's `connect`) and exits. `sqlite_extents::daemon::open` registers no `on_lost`, and nothing else stops or wakes the process. So any read that needs the daemon after it went never returns. That covers a page named but never brought into the daemon's cache (STAGE_MAPPED with no phys), a page the daemon evicted, and a chunk mapped after a commit. The faulting thread blocks in handle_userfault indefinitely. Device requests on the same path return SQLITE_IOERR as intended. When the fault came from `FileSet::read`, the thread holds the FileSet `inner` mutex, so every other connection's next VFS call on that file set blocks too. This breaks the AGENTS.md rule to fail hard: the process neither errors nor exits.

**Trigger.** 1) An app opens a SQLite cell through sqlite_extents::daemon::open (window present) and registers the VFS. 2) The extent daemon restarts: SIGTERM drain or a crash. The client's receive loop ends with closing=false and calls lost.happen(). 3) The app runs a query reading a page whose chunk is mapped in the region but whose extent page the daemon had not installed for this window (cold at MAP time, or evicted since). FileSet::read -> Pages::read copies from the region, or SQLite reads a fetched pointer. 4) The uffd fault reaches DaemonFaults, the daemon request fails, window.lost() is true, and resolve_through awaits pending() forever. 5) The thread never wakes. It holds the FileSet lock, so every other connection on the file set blocks at its next x_read/x_write/x_sync. The app hangs until it is SIGKILLed, where reads outside the window return SQLITE_IOERR.

**Why the tests stay green.** sqlite_extents/tests/daemon.rs never stops the daemon under an open file set. Its fencing test (`a_database_is_fenced_with_its_cell`) only exercises request errors. The daemon-loss tests (TASK-231, TASK-232) are VM-process tests, where on_lost stops the VM, so the pending() wait is the expected outcome there.

**Backlog.** Related. TASK-50.2 AC #3 and TASK-231 (Done) cover DAX faults after daemon loss only for VM processes, where on_lost stops the VM; TASK-300 (Done) added SQLite's in-process window and never handled daemon loss.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After the daemon connection is lost (a drain or a crash), a read through a sqlite_extents file set's window of a page the daemon had not installed (cold at MAP time or evicted) returns an error to SQLite or ends the process within a few seconds; it never blocks for ever.
- [ ] #2 Other connections on the same file set do not block behind that read's FileSet lock: their next x_read, x_write or x_sync returns an error.
- [ ] #3 A sqlite_extents/tests/daemon.rs test serves a daemon it can shut down, opens a file set with a window and committed pages not installed, stops the daemon and reads those pages on a thread with a deadline; it goes red (hangs) with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
