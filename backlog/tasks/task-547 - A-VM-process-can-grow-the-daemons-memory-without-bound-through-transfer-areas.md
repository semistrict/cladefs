---
id: TASK-547
title: A VM process can grow the daemon's memory without bound through transfer areas
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:24'
updated_date: '2026-11-19 16:27'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 938000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-protocol review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/cladefsd/server.rs`: `rg -n -F "fn map(fd: std::os::fd::BorrowedFd<'_>, len: u64) -> Result<Self, Errno> {" crates/cladefs/src/cladefsd/server.rs`

`TransferArea::map` accepts any shmem fd that is sealed against shrinking, up to MAX_TRANSFER_AREA (64 MiB), and maps it MAP_SHARED into the daemon. READ_TRANSFER then has the daemon write into it (`store.read` into `area.slice_mut`). For a never-touched memfd, each first write faults a new shmem page in the daemon's mm. mm/shmem.c charges it to the faulting mm's memcg (`mem_cgroup_charge(folio, vma->vm_mm, …)`), so the page counts against the daemon's cgroup and the host, not the jail's memory limit. When the connection ends, `TransferArea::drop` only munmaps. The pages stay alive and charged for as long as the VM process keeps the memfd open or mapped. Nothing counts these pages: not the governor, not the tenant's arena limit, not InFlight, not process.rs's budget. Doing this once per connection with a fresh memfd makes the memory unbounded.

**Trigger.** A compromised VM process repeats in a loop: connect to its tenant socket, OPEN its own store (or OPEN_READ_ONLY it as a follower, so up to 1024 run at once), create a fresh 64 MiB memfd with F_SEAL_SHRINK and never touch it, send TRANSFER with it, READ_TRANSFER 64 MiB of an extent it wrote into the whole area, close the connection, keep the memfd mapped and close its fd (one mapping each, up to about 65k). Each round leaves 64 MiB of shmem charged to the daemon's cgroup that only the VM process can free. The governor sees its cgroup room and MemAvailable shrink, so it evicts every tenant's extent cache. Then the daemon is OOM-killed and every VM goes with it.

**Why the tests stay green.** The hostile proptest bounds the daemon's heap (`crate::heap::Bounded`), which does not see shmem pages. `vm_processes_shrinking_their_memory_crash_nothing` covers shrinking arenas, not memory charged through transfer areas. No test looks at the daemon's cgroup memory or RssShmem after connections that gave transfer areas have closed.

**Backlog.** New. TASK-149.17 (Done) noted transfer areas are not counted but fixed only heap copies and arena seals; TASK-150.3 (Done) treated TRANSFER as capped by its 64 MiB size; TASK-26.18.6 introduced transfer areas. None covers pages charged to the daemon that outlive the connection.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Under the trigger, 32 rounds of: a connection gives a fresh, never-touched 64 MiB memfd sealed with F_SEAL_SHRINK, READ_TRANSFERs into all of it, then disconnects while the VM process keeps the memfd. The daemon's cgroup memory.current (or Shmem in /proc/meminfo) grows by less than one area (64 MiB) in total.
- [x] #2 Memory charged to the daemon through transfer areas is bounded per tenant and counted by the governor or the tenant's in-flight budget; an area whose connection has ended charges the daemon nothing, however many connections come and go.
- [ ] #3 A Linux test in the daemon's tests runs that 32-round loop and checks the bound; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The daemon seals a transfer area against future writes once it maps it (F_SEAL_FUTURE_WRITE, GROW, SEAL; the client no longer seals it against more), so no page of it is punched out, and touches only pages resident (mincore, TransferArea::range): those the VM process wrote, charged to it. The client writes a slot's pages as it first takes it (an area written whole up front held 2 MiB a connection, past serves_hundreds_of_forks_of_a_base's bound). Arenas are sealed against more seals (physmem), which a VM process giving one back as its area had the daemon add, its chunks unpunchable then: the hostile proptest found it. Tests: transfer_areas_are_their_vm_processes_memory (unwritten bytes refused, written read, punching refused EPERM; mutants without the check or the seal fail it). Left: AC1/AC3's 32-round measurement of the daemon's cgroup memory. macOS: clippy, workspace 2718/2718, sim 41/41; Linux (ubu1, 9cca798f0): workspace 3081/3081, scripts 528 tests; hostile proptest 5 rounds passing; hand mutants (target/b3-linux-mutants.py on Linux, target/b2-mutants.py on macOS) each caught by its test. Wire version 12.
<!-- SECTION:NOTES:END -->
