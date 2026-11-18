---
id: TASK-444.12
title: >-
  parking_lot_core 0.9.12's futex call is undefined behaviour under Miri: a
  reference passed to variadic syscall
status: To Do
assignee: []
created_date: '2026-11-16 04:30'
labels:
  - bug
  - dependencies
  - miri
dependencies: []
parent_task_id: TASK-444
priority: medium
ordinal: 879000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by scripts/miri.sh --probe physmem on ubu1 (TASK-444.4): physmem's tests::arenas::* that park through shuttle_parking_lot::Condvar end in `Undefined Behavior: incorrect c-variadic argument type for syscall(SYS_futex, ...): expected argument #2 to have type *mut u32 but got incompatible type &Atomic<i32>`, in parking_lot_core::thread_parker::imp::ThreadParker::futex_wait. parking_lot_core 0.9.12 (src/thread_parker/linux.rs) passes `&self.futex` to libc::syscall; 0.10.0 passes `*const AtomicI32` (raw_futex_syscall). Users of 0.9: parking_lot 0.12.5 (object_store, shuttle-parking_lot 0.12.5) and crates/fuchsia-sync (ported) directly. Until fixed, every test that parks a thread through parking_lot stops under Miri, which keeps it out of scripts/miri-tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Miri runs a test that parks through parking_lot without this error (0.10, or a fix upstream for 0.9)
- [ ] #2 Each user of parking_lot_core 0.9 moved, or why one can't be, recorded
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
