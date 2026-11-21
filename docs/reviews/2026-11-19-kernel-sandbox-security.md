# Kernel and sandbox security review, 2026-11-19

**Base:** cladefs `e0bfaeaedc5579633d66c7c6a5a56423e110b0fe`; Firecracker
`99b3b968ac2a42a141ffe7c769598583cd6e5389`.

## Verdict

**Not cleared as a hardened multi-tenant sandbox.** This review did not demonstrate host code
execution, arbitrary host-memory access, or a cross-tenant plaintext read. It did demonstrate a
release-build write-once violation, a checked-build daemon abort, unbounded daemon bookkeeping,
near-4-GiB error-path work, and several containment weaknesses. The 256-case hostile-VM battery
passed, but that does not clear the known-open page-reuse, DAX, and resource-lifecycle issues.

The threat model was a malicious guest kernel, a compromised jailed VM process, and a malicious
client of one tenant's daemon socket. Cross-tenant confidentiality/integrity and host availability
were in scope. A malicious snapshot was considered separately: it presupposes that the VM manager
accepts untrusted snapshot state. No claim is made about host-kernel CVEs, a malicious bucket, or a
malicious VM manager itself.

## New findings

### TASK-660 — high: `u64::MAX` extent sequences break the daemon and write-once

`LocalCell::begin_write` does not bound the sequence, and `finish_write` computes
`extent.sequence + 1` while holding the shared store-state lock
(`crates/local_cell/src/cell.rs:1603-1618,1923`). Recovery repeats the addition at line 1064.
`extent_frame::Spent` also computes a wrapping successor and silently omits the extent.

- **Checked build:** the probe overflowed at `cell.rs:1923`, poisoned the shared lock and aborted.
  Production's shared-lock panic policy makes this a host-wide `cladefsd` abort.
- **Release build:** the standalone optimized probe printed `first write: Ok(())`,
  `next_sequence: 0`, `rewrite after free: Ok(())`, and `read back: a5`. The first contents were
  `0x5a`; after commit, free and rewrite, the extent returned the new `0xa5`. Thus the write-once
  violation is demonstrated, not merely inferred. Old/new aliasing through a shared version is the
  next security boundary to test, not something this probe claims to have shown.

Sequence validity belongs at the input boundary, before any data or state change. Checked arithmetic
and an explicit refusal in `Spent` are also required.

### TASK-585 — high, previously known: Cell restore leaves three threads outside seccomp

The Cell restore path constructs `cladefsd client`, `dax faults`, and `cladefs-memory-nodes` before
installing the VMM's per-thread filter. The last bypasses the project's spawner even after it is
installed (`vendor/firecracker/src/vmm/src/persist.rs`, `builder.rs`;
`crates/cladefs/src/cladefsd/client.rs`, `dax/uffd.rs`, `memory_cell.rs`). This exposes exactly the
host-kernel surface that Firecracker's filter is intended to remove. It is not, alone, proof of host
compromise: chroot and the jail UID still constrain ordinary file and tenant access.

**Dynamically confirmed on the test host:** `/proc/<pid>/task/*/status` showed `Seccomp: 0` for exactly
`cladefsd client`, `cladefs-memory-` and `dax faults`; the VM's other threads were filtered. The
record is `fxfs-guest-snapshot-memory-cell-dax/log.txt` in the kernel-probes results.

### TASK-669 — high: the VMM filter permits a complete io_uring interface

The production VMM filter unconditionally allows `io_uring_setup`, `io_uring_enter` and
`io_uring_register` on x86-64 and aarch64 (`vendor/firecracker/resources/seccomp/*.json`). A real
trapping-filter probe reached `io_uring_setup`; the other attempted high-risk calls were trapped and
the `UFFDIO_API` control succeeded. This is a substantial post-compromise kernel interface and a
seccomp-bypass risk, not merely an invalid-argument oddity. **The probe created a valid two-entry
ring and invoked register and enter on it.** Every other attempted high-risk interface trapped. It
is independent of TASK-585: properly filtered VMM threads have this surface too.

### TASK-661 — high: large WRITE payloads are gathered before backpressure

Each WRITE larger than a transfer slot is gathered into a fresh `Vec` before request-credit
backpressure (`crates/cladefs/src/cladefsd/client.rs:327-349,964-984`). A worker can pop all 256
chains first (`crates/virtio_extent/src/workers.rs:269-286`). At 8 MiB each, that is about 1.94 GiB
of live VMM payload per queue, outside daemon and tenant accounting. A guest can alias one source
buffer, so its own RAM need not scale with the VMM's allocations. Credit must be reserved before
payload gathering, or gathering must use bounded transfer memory.

**Measured on the test host:** 32 unanswered 8 MiB writes gathered **268,454,256 bytes** in the VMM. A full
256-entry queue at the same size extrapolates to about 1.94 GiB.

### TASK-602 — high, previously known: repeated MAP grows daemon bookkeeping without bound

Replacing an extent's only mapping with itself recomputes `first = true` and appends another weak
window (`crates/cladefs/src/dax.rs`, `cache.rs`). The direct regression wrote and sealed one extent,
then issued the same `MAP` 1,000 additional times. **`CachedExtent::window_count()` returned 1,001,
not 1.** Repetition grows daemon heap outside the governor, and eviction repeats unmapping and a
`retain` over the duplicate entries. On a public extent, that is a cross-tenant availability risk.

### TASK-662 — medium: Cell snapshot lengths can wrap the memory reservation

Cell restore uses unchecked region-length summation, offset addition, and raw pointer arithmetic
(`vendor/firecracker/src/vmm/src/cladefs_memory.rs:115-136`). For example, lengths
`usize::MAX - 4095` and 8192 wrap the reservation to 4096 bytes; a subsequent raw region pointer is
no longer inside it. This is a code-confirmed restore-time memory-safety defect when snapshot state
is untrusted, **not a demonstrated guest escape**. Validate the sum and every region before any raw
pointer operation.

### TASK-663 — medium: a malformed request can force almost 4 GiB of writes

Error handling zeroes every writable byte not returned (`crates/virtio_extent/src/requests.rs:81-96`).
The only total bound is `u32::MAX`. An unknown-type request with 255 aliased 16 MiB writable
segments wrote exactly **`0xff000000` bytes** in the probe (88.6 ms on the test host). Aliasing avoids
proportional guest RAM, and repeated requests provide deterministic host work amplification;
distinct descriptors also consume memory bandwidth and populate guest RAM. Descriptor work needs a
protocol-sized bound, not a wire-length-sized one.

### TASK-664 — medium: an unshared restore acts before authorization

`opened_version` can sync a victim's writers, read its head, create a version directory and recover
a source store before `finish_fork` applies the shared-version rule
(`crates/cladefs/src/cladefsd/cells.rs:780-849`; `bucket_cell/src/lifecycle.rs:329-341`). A request
that ends in `EACCES` can therefore cause cross-tenant I/O and host side effects and provide a
metadata/timing oracle. It does not itself return plaintext; random version IDs also impede blind
enumeration. Authorization must precede source recovery and mutation.

**Dynamically confirmed on the test host:** the request returned `EACCES` but still created and recovered the
victim's version store under `cladefs-versions-<pid>/`. No victim commit-number change was observed
in this setup, so the demonstrated side effect is unauthorized recovery/host mutation, not a claimed
writer sync.

### TASK-666 — low: several request types ignore unknown flags

READ, ROOT, INFO, COMMIT, UNMAP, FREEZE, RESIZED and follower requests do not consistently reject
unknown bits as the protocol requires (`crates/virtio_extent/src/requests.rs:110-349`). **The real
virtio probe confirmed all six tested cases—INFO, ROOT, READ, COMMIT, UNMAP and FREEZE—did not
return `EINVAL` (`0x3f`).** The other nine range and cross-cell cases were refused correctly, and
the ordinary self-test passed. This is a validation and future-versioning defect, not a demonstrated
escape. One per-type mask should be checked centrally before any handler side effect.

### TASK-667 — low: MAP_MANY tracing can overflow before validation

Tracing sums mapping lengths with ordinary `sum()` before per-map validation
(`crates/cladefs/src/cladefsd/server.rs:898-905`). Multiple `u64::MAX` entries can panic a checked
daemon with tracing enabled or wrap release accounting. Tracing must use checked/saturating
accounting and never alter request validity.

### TASK-668 — low: out-of-range DAX offsets reach invalid guest memory accesses

`vext_dax_direct_access` can underflow `window->len / PAGE_SIZE - pgoff`, and
`vext_dax_zero_page_range` uses zero or short direct-access results incorrectly
(`guest/linux/drivers/virtio_extent/driver.c:1013-1064`). Normal allocators constrain `pgoff`; no
unprivileged route or host escape was established. This is guest memory-safety hardening: reject
out-of-range and wrapping accesses before subtraction or memory access.

## Known high-risk open issues not refiled

- **TASK-560 (high):** concurrent evictions can collect another eviction's `Evicting` pages before
  every window has been sent its unmap. This is the most important known potential stale-page /
  cross-tenant data exposure. The 256-case generator does not prove it absent.
- **TASK-558 (high):** dirty read-only DAX PTEs can become writable through `mprotect` without a new
  PRIVATE operation, losing writes or desynchronizing guest/host accounting.
- **TASK-631, dynamically confirmed:** the first attack fixture accidentally placed its request on
  the guest kernel stack. Firecracker rejected every chain as non-guest memory and returned used
  length zero; the guest reported all 15 requests as successful (`0x7fff`). Moving the request to
  heap separated this defect from the intended malformed-field probes.
- **TASK-631's neighbor TASK-586, TASK-616 and TASK-617:** worker activation, daemon-loss and
  restore-time correctness gaps remain relevant to the same boundary.
- The prior resource findings (including TASK-546, TASK-547, TASK-548, TASK-580 and TASK-608) were
  reviewed for current mitigations but not all dynamically re-run. Their open acceptance cases still
  matter.

## Attack evidence

| Probe | Result |
|---|---|
| 256 randomized compromised-VM cases on the test host: malformed messages, cross-tenant IDs/paths, forged ACKs, memfd abuse, floods and truncation | **Passed in 164 s.** No observed cross-tenant bytes, daemon crash, or protocol-budget violation in that generator. |
| Direct greatest-sequence checked-build write | **Failed as security expected:** overflow at `cell.rs:1923`, shared-lock poisoning, `SIGABRT`. |
| Standalone release greatest-sequence write/commit/free/rewrite | **Write-once bypass confirmed:** first write and rewrite both `Ok`, `next_sequence = 0`, new `0xa5` read back. |
| Same extent mapped 1,000 extra times | **1,001 window registrations**, confirming unbounded duplicate bookkeeping. |
| Unknown request with 255 aliased 16 MiB writable descriptors | **`0xff000000` bytes written**, 88.6 ms on the test host in the cache-hot case. |
| Real guest-kernel malformed virtio requests | **Six unknown-flag types did not return `EINVAL` (`0x3f`).** Nine range/cross-cell attacks were refused correctly; the normal self-test passed. |
| Real trapping seccomp attempts against ptrace, process_vm_writev, bpf, perf, io_uring, direct userfaultfd, namespaces/mounts, modules, pidfds, exec and KVM_CREATE_VM | **A valid two-entry io_uring was created, and register/enter were invoked.** All other high-risk attempts trapped. UFFDIO_API is the positive control. |
| Per-thread `Seccomp` state after Cell restore | **Confirmed:** `Seccomp: 0` on exactly `cladefsd client`, `cladefs-memory-`, and `dax faults`; other VM threads filtered. |
| 32 unanswered 8 MiB WRITEs with the daemon consuming requests | **268,454,256 bytes gathered in the VMM**, confirming linear pre-backpressure allocation. |
| Unshared restore beside a live victim writer | **`EACCES` was returned, but the victim's version store was created and recovered** under `cladefs-versions-<pid>/`. No commit-number change was observed. |

The exact fixtures and commands are in `guest/tests/audit/README.md`; the fixture is
`kernel-security-probes-8.patch`, and the standalone release probe is `sequence_probe.rs`. The
affected sources remain identified in the tasks rather than being
silently patched in this review.

## Coverage and limits

Three independent source tracks reviewed the guest kernel/virtio boundary, Firecracker's jail and
seccomp implementation, and the daemon/tenant protocol. Dynamic work ran on the x86-64 test host with KVM,
the production musl filters and jailer where the case required them, plus local checked and release
store probes.

This was not exhaustive proof of safety. In particular, it did not cover aarch64, every arbitrary
virtqueue bit pattern, syzkaller/KCSAN exploration, host-kernel vulnerability research, or every
known-open acceptance case. No claim is made that the seccomp syscall battery substitutes for an
exploit in an unfiltered thread. A syscall reaching the kernel is treated as attack surface to
justify, not automatically as a vulnerability.
