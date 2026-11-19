---
id: TASK-585
title: A VM restored from a memory cell runs three threads with no seccomp filter
status: To Do
assignee: []
created_date: '2026-11-18 15:29'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 976000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), firecracker review; verified against the code by independent reviewers. Severity: medium.

**Where:** `vendor/firecracker/src/vmm/src/persist.rs`: `rg -n -F 'crate::cladefs_memory::guest_memory(' vendor/firecracker/src/vmm/src/persist.rs`

restore_from_snapshot builds the `Cell` guest memory before it calls builder::build_microvm_from_snapshot. That function's first step is confine_extent_threads, the only place that calls cladefs::thread::set_spawner. So while cladefs_memory::guest_memory runs, the spawner is unset, and cladefs::thread::spawn falls back to std::thread::Builder from the VMM main thread. That thread has no filter yet: apply_filter runs at the end of the build, and it calls seccomp(SECCOMP_SET_MODE_FILTER, 0, ..) with no TSYNC, so the filter covers only the calling thread. Three threads are affected: "cladefsd client" (Client::start, client.rs:899), "dax faults" (UffdSecondStage::serve) and "cladefs-memory-nodes". The last one is spawned with std::thread::Builder directly in memory_cell.rs:248, so it would skip the spawner even if one were set. All three stay unfiltered for the VM's whole life. They are the threads that serve userfaultfd faults at guest-chosen addresses and parse the daemon's replies. TASK-50.1 put exactly these threads under the VMM filter for the extent device (see the comment on confine_extent_threads). They also miss the spawner's install_signal_stack, so an overflow on them dies with no report (TASK-137).

**Trigger.** PUT /snapshot/load with mem_backend.backend_type "Cell" and memory_cell set -> restore_from_snapshot -> cladefs_memory::guest_memory -> MemoryCell::open_forked -> DaemonConnection::connect -> Client::start -> cladefs::thread::spawn with SPAWNER unset -> std spawn from the unfiltered main thread; memory_cell.window -> create_memory_window_in_host_memory -> stage.serve -> "dax faults"; MemoryCell::window -> "cladefs-memory-nodes". After the restore, /proc/<pid>/task/<tid>/status shows `Seccomp: 0` for these three threads and 2 for the vCPU, API and VMM threads. A guest drives fault handling on the unfiltered "dax faults" thread by touching any guest page.

**Why the tests stay green.** TASK-149.28's filter test and the jailed guest runs (TASK-69) only fail when a filter refuses a syscall. A thread with no filter refuses nothing, so fxfs-guest-snapshot-memory-vdisk-dax passes. No test checks that every thread of the VM process is filtered.

**Backlog.** Related to TASK-50.1 (Done), which made sure no extent device thread starts before the filter but predates the memory-cell path, and to TASK-456.3 (In Progress), which owns this restore code.

2026-11-19 security review: dynamically confirmed on ubu1 with the release guest kernel and Cell restore. `/proc/<pid>/task/*/status` showed `Seccomp: 0` for exactly `cladefsd client`, `cladefs-memory-` and `dax faults`, with the VM's other threads filtered. See `target/security-review/kernel-probes/e2e/fxfs-guest-snapshot-memory-cell-dax/fxfs-guest-snapshot-memory-cell-dax/log.txt`.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After PUT /snapshot/load with a Cell memory backend, every thread of the VM process shows `Seccomp: 2` in /proc/<pid>/task/<tid>/status. This includes 'cladefsd client', 'dax faults' and 'cladefs-memory-nodes'.
- [ ] #2 No thread of the VM process, on the restore path or the boot path, starts before the production filter is installed.
- [ ] #3 A Linux test restores from a Cell backend under the production filters, reads /proc/self/task/*/status, and asserts `Seccomp: 2` on every thread. With the fix reverted it goes red and names the three threads.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
