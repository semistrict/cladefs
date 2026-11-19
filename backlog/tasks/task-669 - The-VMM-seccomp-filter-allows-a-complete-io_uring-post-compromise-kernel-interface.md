---
id: TASK-669
title: >-
  The VMM seccomp filter allows a complete io_uring post-compromise kernel
  interface
status: To Do
assignee: []
created_date: '2026-11-19 18:47'
labels:
  - security
dependencies: []
priority: high
type: bug
ordinal: 1052000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `vendor/firecracker/resources/seccomp/{x86_64,aarch64}-unknown-linux-musl.json:49-63`.

The production VMM filter unconditionally allows `io_uring_setup`, `io_uring_enter` and `io_uring_register` on both architectures. A real trapping-filter probe reached all three syscalls and then created a valid two-entry ring, calling register and enter on it. Every other attempted high-risk interface—ptrace, process_vm_writev, bpf, perf_event_open, direct userfaultfd, namespaces/mounts, modules, pidfds, exec and KVM_CREATE_VM—trapped. UFFDIO_API succeeded as the positive control.

This is a usable post-compromise kernel interface and seccomp-bypass risk, not an invalid-argument oddity, and it is independent of TASK-585's unfiltered Cell-restore threads. No host code execution is claimed. **Blind spot:** existing seccomp tests check required DAX syscalls but never attempt forbidden kernel interfaces. **Structure:** keep general-purpose post-compromise interfaces out of the long-lived VMM threads; confine necessary drive io_uring work to its own narrowly justified thread/lifetime and document the remaining kernel attack surface.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Prevent arbitrary io_uring creation/use after compromise while preserving required drive I/O, or document and independently review why it is safe
- [ ] #2 Regression probes cover valid ring creation, register and enter under the production filter on x86-64 and aarch64
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 Firecracker and guest security tests pass on ubu1
<!-- DOD:END -->
