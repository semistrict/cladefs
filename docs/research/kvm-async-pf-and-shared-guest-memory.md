# KVM async page faults and guest memory shared between restored VMs

Research for TASK-456.3 (2026-11-16). The question: cladefs restores Firecracker VMs whose guest RAM
is a userfaultfd-registered anonymous mapping, installs pages read-only from a daemon's shared memfd
pages under UFFD write-protect, and must give a guest store a private copy while a load keeps the
shared page. On ubu1 (Ubuntu `7.0.0-43-generic`, x86-64) nearly every first touch arrives from a
kworker (`events`) as a write fault, so loads become private copies.

Sources are the upstream kernel at tags (fetched from git.kernel.org), kernel documentation, patch
series, Firecracker's and E2B's repositories at named commits, and the papers. lore.kernel.org
refused automated fetches (HTTP 403 bot check), so patch threads were read through mirrors of the
same messages (patchew.org, ratatoskr.run) or through copies of the patches in Firecracker's repo;
each such place says so. Claims marked **unverified** could not be traced to a primary source.
Claims marked **derived** follow from the cited code but were not run or measured.

Ubuntu's `7.0.0-43-generic` may carry Ubuntu patches; everything below is upstream `v7.0`
(**unverified** that Ubuntu's KVM/mm code matches it). The latest release is `v7.3`; the latest tag
`v7.4-rc3`. Where `v7.3` or `v6.1` differ from `v7.0`, it says so.


## Summary

1. **The async PF worker always faults for writing.** `async_pf_execute` calls
   `get_user_pages_remote(mm, addr, 1, FOLL_WRITE, NULL, &locked)` whatever the vCPU's access; so it
   has since async PF was merged (v2.6.38) and still does in v7.3. x86 KVM takes the async path when
   GUP with `FOLL_NOWAIT` returns "needs I/O" (a userfaultfd-registered page does) and
   `kvm_can_do_async_pf` is true. That consults the guest's `MSR_KVM_ASYNC_PF_EN` **only** when the
   VM has `KVM_X86_DISABLE_EXITS_HLT`; otherwise a guest without PV async PF still gets async PF
   ("APF halt": the vCPU is put in an artificial halt while the worker runs). That is why clearing
   the MSR changed nothing. There is no per-VM switch; the only userspace lever on v7.0 is
   `KVM_CAP_X86_DISABLE_EXITS` with HLT **plus** a guest that cannot take PV async PF.
2. **`run->memory_fault` is never filled for an ordinary (non-guest_memfd, shared) memslot on
   x86**, in v7.0, v7.3 or v7.4-rc3. Only private/guest_memfd paths annotate. The exit carries gpa,
   size and the `PRIVATE` flag; read/write/exec are not reported ("RWX flags are not (yet) defined
   or communicated to userspace"). Exit and cap came in v6.8.
3. **KVM userfault (`KVM_MEM_USERFAULT`, `KVM_CAP_USERFAULT`) is not merged** (absent from v7.4-rc3,
   kvm/next and kvm-x86/next). Latest series found: v3, 2025-06-18 (a 2026-06-18 posting is
   referenced but **unverified**). It exits with `KVM_EXIT_MEMORY_FAULT` +
   `KVM_MEMORY_EXIT_FLAG_USERFAULT` and gpa, no read/write flag; the bitmap check sits before GUP,
   so such faults never go async ("not compatible with async page faults").
4. **A load of a page whose host PTE is present (or a uffd-wp marker over a resident shmem page)
   resolves synchronously, without async PF.** KVM tries fast GUP for writing first (fails on a
   read-only PTE), then slow GUP without `FOLL_WRITE`, which succeeds without `VM_FAULT_RETRY`; the
   SPTE is made read-only. A **missing** page under userfaultfd makes the `FOLL_NOWAIT` GUP return
   `VM_FAULT_RETRY` without queueing a message, KVM treats it as needing I/O and goes async; the
   worker's write then reaches userfaultfd from the kworker. A **store** to a present uffd-wp page
   takes the same route: `FOLL_WRITE|FOLL_NOWAIT` → `handle_userfault(VM_UFFD_WP)` returns
   `VM_FAULT_RETRY` at once → async PF → the worker's `UFFD_PAGEFAULT_FLAG_WP|WRITE` fault, which
   here really is a store. So pre-installing pages removes async PF (and the worker's write) for
   loads of them.
5. **Sharing elsewhere.** Firecracker's File backend maps the memory file `MAP_PRIVATE`: loads of
   pages in the page cache share it; but a first touch of a page **not** in the page cache goes
   async and the worker's `FOLL_WRITE` CoWs it into a private anonymous copy even for a load
   (**derived**). E2B installs every page with `UFFDIO_COPY` (a private copy per sandbox). REAP
   installs from a buffer, disallows sharing on purpose. FaaSnap maps non-zero regions `MAP_PRIVATE`
   from the memory file and prefetches into the page cache. AWS's publications describe no
   cross-VM page sharing (Firecracker's own docs advise disabling KSM). guest_memfd `mmap` (v6.18)
   is `MAP_SHARED` only and bound to one VM: no sharing, no CoW.
6. **Options on 7.0**, cheapest first: (a) map the daemon's memfd runs **`MAP_PRIVATE`** instead of
   `MAP_SHARED`+uffd-wp, so the kernel does CoW on stores and loads of resident pages never fault to
   userspace or go async; (b) install pages **before** the guest touches them (whole node at once),
   so only first touches of unmapped regions are ambiguous, and resolve those by failing the
   worker and letting the vCPU's synchronous page-ready retry decide; (c) turn async PF off with
   `KVM_X86_DISABLE_EXITS_HLT` + clearing `MSR_KVM_ASYNC_PF_EN` (all faults then come from the vCPU
   thread with the true access; idle vCPUs no longer exit on HLT); (d) read gpa+error code from the
   `kvm:kvm_page_fault` tracepoint (privileged). Newer kernels would add per-GFN no-write memory
   attributes with exits carrying gpa + WRITE (Bonzini/Saenz Julienne, unmerged), KVM userfault
   (unmerged), and userfaultfd for guest_memfd (unmerged).

## 1. Async PF: what the worker does, when x86 KVM uses it, and how to turn it off

### 1.1 `async_pf_execute` faults for writing, always

- v7.0 [`virt/kvm/async_pf.c`: `async_pf_execute`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/async_pf.c?h=v7.0):
  ```c
  if (mmget_not_zero(mm)) {
      mmap_read_lock(mm);
      get_user_pages_remote(mm, addr, 1, FOLL_WRITE, NULL, &locked);
  ```
  `FOLL_WRITE` is a constant; the work item (`struct kvm_async_pf`) carries only `addr`,
  `cr2_or_gpa` and the arch part, not the access. The vCPU's error code is kept in
  `work->arch.error_code` ([`arch/x86/kvm/mmu/mmu.c`: `kvm_arch_setup_async_pf`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/mmu.c?h=v7.0))
  but used only for the later retry (1.4).
- Its result is ignored: "Notify and kick the vCPU even if faulting in the page failed, e.g. so that
  the vCPU can retry the fault synchronously." (same function, v7.0).
- The work runs on the system workqueue: `kvm_setup_async_pf` calls `schedule_work(&work->work)`
  ([`virt/kvm/async_pf.c`: `kvm_setup_async_pf`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/async_pf.c?h=v7.0)), hence kworker
  `events` in the trace.
- History: the original v2.6.38 code was `get_user_pages(current, mm, addr, 1, 1, 0, &page, NULL)`
  (write=1) ([`virt/kvm/async_pf.c` @v2.6.38](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/async_pf.c?h=v2.6.38)). v6.1 has
  `get_user_pages_remote(mm, addr, 1, FOLL_WRITE, NULL, NULL, &locked)` with `apf->mm`
  ([@v6.1](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/async_pf.c?h=v6.1)); the Feb 2024 commits by Sean Christopherson ("Get
  reference to VM's address space in the async #PF worker", "Put mm immediately after async #PF
  worker completes remote gup()") changed the mm handling only. v7.3's file is byte-identical to
  v7.0's ([@v7.3](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/async_pf.c?h=v7.3)).

### 1.2 When x86 KVM goes async

- v7.0 [`arch/x86/kvm/mmu/mmu.c`: `__kvm_mmu_faultin_pfn`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/mmu.c?h=v7.0):
  ```c
  unsigned int foll = fault->write ? FOLL_WRITE : 0;
  if (fault->is_private || kvm_memslot_is_gmem_only(fault->slot))
      return kvm_mmu_faultin_pfn_gmem(vcpu, fault);
  foll |= FOLL_NOWAIT;
  fault->pfn = __kvm_faultin_pfn(...);
  if (fault->pfn != KVM_PFN_ERR_NEEDS_IO)
      return RET_PF_CONTINUE;
  if (!fault->prefetch && kvm_can_do_async_pf(vcpu)) {
      if (kvm_find_async_pf_gfn(vcpu, fault->gfn)) {
          kvm_make_request(KVM_REQ_APF_HALT, vcpu);
          return RET_PF_RETRY;
      } else if (kvm_arch_setup_async_pf(vcpu, fault)) {
          return RET_PF_RETRY;
      }
  }
  foll |= FOLL_INTERRUPTIBLE;
  foll &= ~FOLL_NOWAIT;
  fault->pfn = __kvm_faultin_pfn(...);   /* synchronous, on the vCPU thread */
  ```
  So: first a non-blocking GUP with the vCPU's real access; only if that reports "needs I/O" does
  KVM go async, and only if not a prefetch and `kvm_can_do_async_pf`. Else it faults synchronously
  on the vCPU thread with the vCPU's access. guest_memfd-only memslots never go through GUP or
  async PF.
- `KVM_PFN_ERR_NEEDS_IO` is set in [`virt/kvm/kvm_main.c`: `hva_to_pfn`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/kvm_main.c?h=v7.0)
  when GUP returned no page, the VMA exists, is not `VM_IO|VM_PFNMAP`, the call had `FOLL_NOWAIT`,
  and `vma_is_valid(vma, write)` (VMA has `VM_READ`, and `VM_WRITE` for a write). Otherwise
  `KVM_PFN_ERR_FAULT`.
- `kvm_arch_setup_async_pf` saves token, gfn, `error_code`, `direct_map`, cr3 and calls
  `kvm_setup_async_pf`, which refuses past `ASYNC_PF_PER_VCPU` queued items or if the
  `GFP_NOWAIT` allocation fails (then KVM faults synchronously)
  ([`virt/kvm/async_pf.c`: `kvm_setup_async_pf`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/async_pf.c?h=v7.0)).
- v7.0 [`arch/x86/kvm/x86.c`: `kvm_can_do_async_pf`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/x86.c?h=v7.0):
  ```c
  if (unlikely(!lapic_in_kernel(vcpu) ||
               kvm_event_needs_reinjection(vcpu) ||
               kvm_is_exception_pending(vcpu)))
      return false;
  if (kvm_hlt_in_guest(vcpu->kvm) && !kvm_can_deliver_async_pf(vcpu))
      return false;
  return kvm_arch_interrupt_allowed(vcpu);
  ```
  Identical in v6.1; in v7.3 the last line is `kvm_is_interrupt_allowed(vcpu)` (rename)
  ([@v6.1](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/x86.c?h=v6.1), [@v7.3](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/x86.c?h=v7.3)).
- `kvm_hlt_in_guest(kvm)` is `kvm->arch.disabled_exits & KVM_X86_DISABLE_EXITS_HLT`
  ([`arch/x86/kvm/x86.h`: `kvm_hlt_in_guest`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/x86.h?h=v7.0)).
- `kvm_can_deliver_async_pf` (same file, v7.0) is true only if the guest enabled PV async PF
  (`kvm_pv_async_pf_enabled`: `KVM_ASYNC_PF_ENABLED | KVM_ASYNC_PF_DELIVERY_AS_INT` both set in
  `MSR_KVM_ASYNC_PF_EN`), the vCPU is at CPL>0 unless the guest set `KVM_ASYNC_PF_SEND_ALWAYS`,
  state is not protected, paging is on (or L1 opted into #PF vmexits when nested).
- Where the HLT condition came from: commit
  [`1dfdb45ec510` "KVM: x86: clean up conditions for asynchronous page fault handling"](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id=1dfdb45ec510ba27e366878f97484e9c9e728902)
  (Paolo Bonzini, 2019-06, in v5.3): "Even when asynchronous page fault is disabled, KVM does not
  want to pause the host if a guest triggers a page fault; instead it will put it into an
  artificial HLT state ... it is used even if kvm_halt_in_guest() returns true; in this case, the
  guest probably should not pay the additional latency cost of the artificial halt, and thus we
  should handle the page fault in a completely synchronous way."

### 1.3 The guest MSR, HLT-in-guest and "APF halt"

- `kvm_arch_async_page_not_present` ([`arch/x86/kvm/x86.c` @v7.0](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/x86.c?h=v7.0)):
  if `kvm_can_deliver_async_pf` it injects a PV "page not present" #PF; else
  `kvm_make_request(KVM_REQ_APF_HALT, vcpu)` with the comment "putting the guest in an artificial
  halt state can be beneficial nevertheless ... When the instruction that triggered a page fault is
  retried, hopefully the page will be ready in the host."
- `vcpu_enter_guest` on `KVM_REQ_APF_HALT`: "Page is swapped out. Do synthetic halt",
  `vcpu->arch.apf.halted = true` (same file). `kvm_arch_async_page_present` clears it and makes
  the vCPU runnable when the worker is done.
- So the guest's `MSR_KVM_ASYNC_PF_EN` decides only **how** the vCPU waits (PV event, or APF halt),
  not **whether** the worker runs, unless HLT exits are disabled. Clearing the MSR leaves async PF
  on, delivered as APF halt: consistent with what ubu1 showed.
- Also: a Linux guest enables async PF only with `KVM_FEATURE_ASYNC_PF_INT` and without the
  `no-kvmapf` boot parameter, and does not set `KVM_ASYNC_PF_SEND_ALWAYS`, so faults taken while
  the guest is in kernel mode are APF halts even when enabled
  ([`arch/x86/kernel/kvm.c`: `kvm_guest_cpu_init`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kernel/kvm.c?h=v7.0)).
  MSR layout: [Documentation/virt/kvm/x86/msr.rst, `MSR_KVM_ASYNC_PF_EN`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/Documentation/virt/kvm/x86/msr.rst?h=v7.0).
- Writing the MSR with the enable bits clear runs `kvm_clear_async_pf_completion_queue` and
  `kvm_async_pf_hash_reset` ([`kvm_pv_enable_async_pf` @v7.0](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/x86.c?h=v7.0)).

### 1.4 The retry after the worker: synchronous, on the vCPU thread, with the true access

- When the vCPU next enters, `KVM_REQ_APF_READY` → `kvm_check_async_pf_completion` →
  `kvm_arch_async_page_ready` ([`virt/kvm/async_pf.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/async_pf.c?h=v7.0),
  [`arch/x86/kvm/mmu/mmu.c`: `kvm_arch_async_page_ready`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/mmu.c?h=v7.0)),
  which calls `kvm_mmu_do_page_fault(vcpu, work->cr2_or_gpa, work->arch.error_code, true, NULL, NULL)`.
  The fourth argument is `prefetch`
  ([`arch/x86/kvm/mmu/mmu_internal.h`: `kvm_mmu_do_page_fault`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/mmu_internal.h?h=v7.0)),
  and `.write = err & PFERR_WRITE_MASK` comes from the saved error code.
- With `prefetch` set, `__kvm_mmu_faultin_pfn` skips the async cell: after the `FOLL_NOWAIT`
  attempt it faults synchronously with the vCPU's own access, on the vCPU thread (**derived** from
  1.2). A userfaultfd message from this retry therefore carries the vCPU thread's tid
  (`UFFD_FEATURE_THREAD_ID`) and `UFFD_PAGEFAULT_FLAG_WRITE` only for a real store
  ([`fs/userfaultfd.c`: `userfault_msg`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/userfaultfd.c?h=v7.0)). Its return value is ignored
  except for stats; if it fails, the guest re-executes the instruction and faults again normally.
- Consequence (**derived**): a worker fault that fails is harmless; the vCPU's retry tells the truth.

### 1.5 Turning async PF off for one VM

- No module parameter or capability controls it: `CONFIG_KVM_ASYNC_PF` is selected unconditionally
  by `KVM_X86` ([`arch/x86/kvm/Kconfig`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/Kconfig?h=v7.0)); no `module_param` in
  x86.c/mmu.c mentions async PF (searched v7.0 and v7.3).
- The one lever: `KVM_ENABLE_CAP(KVM_CAP_X86_DISABLE_EXITS, KVM_X86_DISABLE_EXITS_HLT)` before any
  vCPU is created, and a guest that cannot take PV async PF (MSR cleared on restore, or
  `KVM_FEATURE_ASYNC_PF_INT` hidden from CPUID). Then `kvm_can_do_async_pf` returns false and every
  fault is synchronous on the vCPU thread (1.2).
  - api.rst [7.13 KVM_CAP_X86_DISABLE_EXITS](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/Documentation/virt/kvm/api.rst?h=v7.0): "suggested
    when vCPUs are associated to dedicated physical CPUs", "Do not enable KVM_FEATURE_PV_UNHALT if
    you disable HLT exits", fails "if any vCPUs have already been created".
  - HLT is allowed only when the `mitigate_smt_rsb` module parameter is off; on CPUs with
    `X86_BUG_SMT_RSB` and SMT possible KVM warns "KVM_CAP_X86_DISABLE_EXITS should only be used with
    SMT disabled or trusted guests" (`kvm_get_allowed_disable_exits`, `kvm_vm_ioctl_enable_cap`,
    [x86.c @v7.0](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/x86.c?h=v7.0)).
  - Firecracker does not use this capability (no `DISABLE_EXITS` in `src/vmm` at
    [main `cc88013`](https://github.com/firecracker-microvm/firecracker/tree/cc88013eadcf432da1b37d47d445b7e8bcc3046e/src/vmm/src)).
- Not controllable from userspace in practice: in-kernel LAPIC (Firecracker uses one), guest
  interrupt state, pending events.
- guest_memfd-only memslots skip GUP and async PF entirely (1.2), but see 5.6.

## 2. `KVM_EXIT_MEMORY_FAULT` / `KVM_CAP_MEMORY_FAULT_INFO` on x86

- Filled by `kvm_prepare_memory_fault_exit`
  ([`include/linux/kvm_host.h` @v7.0](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/linux/kvm_host.h?h=v7.0)), unchanged in v7.3:
  ```c
  vcpu->run->exit_reason = KVM_EXIT_MEMORY_FAULT;
  vcpu->run->memory_fault.gpa = gpa;
  vcpu->run->memory_fault.size = size;
  /* RWX flags are not (yet) defined or communicated to userspace. */
  vcpu->run->memory_fault.flags = 0;
  if (is_private)
      vcpu->run->memory_fault.flags |= KVM_MEMORY_EXIT_FLAG_PRIVATE;
  ```
  It takes `is_write`/`is_exec` and drops them. The only flag in the uapi of v7.0, v7.3, v7.4-rc3
  and kvm/next is `KVM_MEMORY_EXIT_FLAG_PRIVATE`.
- x86 call sites in v7.0 (`kvm_mmu_prepare_memory_fault_exit`,
  [mmu.c](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/mmu.c?h=v7.0), [mmu_internal.h](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/mmu_internal.h?h=v7.0)):
  `kvm_handle_noslot_fault` (private access, no slot); `kvm_mmu_faultin_pfn_gmem` (slot without
  guest_memfd, or `kvm_gmem_get_pfn` failed); `kvm_mmu_faultin_pfn` (private/shared attribute
  mismatch); `kvm_mmu_do_page_fault` (emulation requested on private memory). v7.3 has the same
  five sites.
- Not annotated: `kvm_handle_error_pfn`, the path for an ordinary memslot whose GUP failed
  (`KVM_PFN_ERR_FAULT`), returns a bare `-EFAULT`. So the EFAULTs cladefs sees (a store to a page
  made read-only, a VMA lacking `VM_WRITE`) come with no gpa, as observed.
- api.rst [8.44 KVM_CAP_MEMORY_FAULT_INFO](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/Documentation/virt/kvm/api.rst?h=v7.0) "Architectures:
  x86"; "KVM_RUN will fill kvm_run.memory_fault if KVM cannot resolve a guest page fault VM-Exit,
  e.g. if there is a valid memslot but no backing VMA" (aspirational wording; the code above is
  narrower); valid only with errno EFAULT/EHWPOISON *and* `exit_reason == KVM_EXIT_MEMORY_FAULT`.
- Versions: `KVM_CAP_MEMORY_FAULT_INFO` (232) first in v6.8 uapi (absent in v6.7). The exit came
  with
  [`16f95f3b95ca` "KVM: Add KVM_EXIT_MEMORY_FAULT exit to report faults to userspace"](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id=16f95f3b95caded251a0440051e44a2fbe9e5f55)
  (Chao Peng, guest_memfd series), whose message says it "will initially use
  KVM_EXIT_MEMORY_FAULT to report implicit conversions between private and shared memory". 2024-05
  "KVM: MMU: Disable fast path if KVM_EXIT_MEMORY_FAULT is needed" (Michael Roth) is the other
  merged change found.
- Not merged: Anish Moorthy's annotation of x86/arm64 stage-2 EFAULTs and RWX flags
  ("KVM_CAP_EXIT_ON_MISSING", v7 Feb 2024; "Set up KVM_EXIT_MEMORY_FAULTs when arm64/x86 stage-2
  fault handlers fail", v2 Aug 2024). Titles/dates from search results only; lore threads not
  readable here: **unverified**. The RWX patch ("KVM: Define and communicate KVM_EXIT_MEMORY_FAULT
  RWX flags to userspace", `KVM_MEMORY_EXIT_FLAG_READ/WRITE/EXEC` bits 0-2) is patch 02 of the
  memory protection attributes RFC (6.2).

## 3. KVM userfault (`KVM_MEM_USERFAULT`)

- **Not merged.** No `KVM_MEM_USERFAULT`/`KVM_CAP_USERFAULT` in uapi at v7.3, v7.4-rc3,
  [kvm.git `next`](https://git.kernel.org/pub/scm/virt/kvm/kvm.git/tree/include/uapi/linux/kvm.h?h=next)
  or [kvm-x86 `next`/`mmu`](https://github.com/kvm-x86/linux/blob/next/include/uapi/linux/kvm.h)
  (checked 2026-11-16).
- Series: James Houghton, "[PATCH v3 00/15] KVM: Introduce KVM Userfault", 2025-06-18,
  Message-ID `20250618042424.330664-1-jthoughton@google.com`, based on kvm-x86/next (read via the
  [patchew mirror](https://patchew.org/linux/20250618042424.330664-1-jthoughton@google.com/);
  [lore](https://lore.kernel.org/kvm/20250618042424.330664-1-jthoughton@google.com/)). v1
  2024-12, v2 2025-01-09. A RISC-V RFC of 2026-07 links a Houghton posting
  `20260618205753.2600064-1-jthoughton@google.com` as "KVM_MEM_USERFAULT generic series": version
  and content **unverified** (lore blocked, patchew 404). Its cover letter of 2026-07 says the
  RISC-V port waits "once the generic userfault series lands".
- API (from the v3 cover letter and the documentation patch as carried in Firecracker's
  [`feature/secret-hiding` cell, `15-kvm-mem-userfault/0059`](https://github.com/firecracker-microvm/firecracker/tree/aa10ee7828b357cbb0cbf7ee38a5f19ca5093e24/resources/hiding_ci/kernels/6.18-secret-hiding/linux_patches/15-kvm-mem-userfault)):
  - memslot flag `KVM_MEM_USERFAULT`; `userfault_bitmap` (a `__u64` taken from `pad2` of
    `struct kvm_userspace_memory_region2`): "each bit in the array linearly corresponds to a single
    gfn ... If the bit for a page is set, any vCPU access to that page will exit to userspace with
    KVM_MEMORY_EXIT_FLAG_USERFAULT."
  - exit: `KVM_EXIT_MEMORY_FAULT` with `KVM_MEMORY_EXIT_FLAG_USERFAULT` (bit 4), gpa and size. No
    read/write: the patch passes `fault->write` to `kvm_prepare_memory_fault_exit`, which (2)
    drops it.
  - "Setting bits in userfault_bitmap has no effect on pages that have already been mapped by KVM
    until KVM_MEM_USERFAULT is disabled and re-enabled again"; enabling zaps the slot's SPTEs; only
    4K mappings while enabled.
  - "KVM Userfault does not attempt to catch KVM's own accesses to guest memory. That is left up to
    userfaultfd." (cover letter)
- Async PF: cover letter: "KVM Userfault is not compatible with async page faults." In the x86
  patch (`0050-KVM-x86-mmu-Add-support-for-KVM_MEM_USERFAULT.patch` in the same directory) the
  bitmap check is the first thing in `__kvm_mmu_faultin_pfn`, before GUP:
  ```c
  userfault = kvm_gfn_userfault(vcpu->kvm, fault->slot, fault->gfn);
  if (userfault) {
      kvm_mmu_prepare_userfault_exit(vcpu, fault);
      return -EFAULT;
  }
  ```
  so a fault on a marked page exits on the vCPU thread and never starts a worker.
- Related, Amazon (Jack Thomson): "userfaultfd async page faults" on top of KVM userfault
  (`KVM_ASYNC_PF` vCPU ioctl with ACCEPT/READY ops, `KVM_MEMORY_EXIT_FLAG_APF`), patches dated
  2026-01 to 2026-04 in the same cell, [`60-async-pf/`](https://github.com/firecracker-microvm/firecracker/tree/aa10ee7828b357cbb0cbf7ee38a5f19ca5093e24/resources/hiding_ci/kernels/6.18-secret-hiding/linux_patches/60-async-pf).
  Upstream posting status **unverified**. Firecracker's README there: "The patches are not yet
  merged upstream."

## 4. Loads and stores on pages installed read-only under uffd-wp

All v7.0; same logic in v6.1 (`hva_to_pfn_slow(addr, async, ...)` with `FOLL_NOWAIT` and
`*async = true` in place of `KVM_PFN_ERR_NEEDS_IO`, which came in v6.13).

### 4.1 How KVM looks up the pfn

[`virt/kvm/kvm_main.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/kvm_main.c?h=v7.0):

- `hva_to_pfn_fast`: tries `get_user_page_fast_only(hva, FOLL_WRITE, ...)` when the caller wants
  write **or** passed `map_writable` (x86 always passes `&fault->map_writable`). Read-only fast GUP
  is never tried: "If the caller only wants to read the page, KVM must go down the full, slow path
  in order to avoid racing an operation that breaks Copy-on-Write".
- Fast GUP with `FOLL_WRITE` needs `_PAGE_RW` (`pte_access_permitted`,
  [`arch/x86/include/asm/pgtable.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/include/asm/pgtable.h?h=v7.0)); `pte_mkuffd_wp` is
  `pte_wrprotect(pte_set_flags(pte, _PAGE_UFFD_WP))` (same file). So it fails on a uffd-wp or
  read-only PTE.
- `hva_to_pfn_slow`: `get_user_pages_unlocked(hva, 1, &page, FOLL_HWPOISON | FOLL_HONOR_NUMA_FAULT | flags)`
  with the vCPU's `FOLL_WRITE` (or not) and `FOLL_NOWAIT`; then "map read fault as writable if
  possible" by another write fast-GUP, which fails on a read-only PTE, so `writable = false`.
- `writable = false` → the SPTE lacks `shadow_host_writable_mask` and write access
  ([`arch/x86/kvm/mmu/spte.c`: `make_spte`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/spte.c?h=v7.0)); `fast_page_fault`
  makes an SPTE writable only if `is_mmu_writable_spte(spte)`
  ([mmu.c](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/mmu/mmu.c?h=v7.0)), so a later guest store takes the slow path again
  through GUP with `FOLL_WRITE`.

### 4.2 Guest load

- **Host PTE present (read-only, uffd-wp):** slow GUP without `FOLL_WRITE` finds the page without
  faulting; pfn returned, SPTE read-only. No `VM_FAULT_RETRY`, no async PF, no userfaultfd message
  (**derived**).
- **uffd-wp marker over a shmem page (installed but never touched):** read GUP faults:
  `handle_pte_marker` → `pte_marker_handle_uffd_wp` → `do_pte_missing`
  ([`mm/memory.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/memory.c?h=v7.0)) → `shmem_fault`, which returns a resident page without
  retry ([`mm/shmem.c`: `shmem_fault`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/shmem.c?h=v7.0)); `set_pte_range` keeps the uffd-wp bit
  ("`if (unlikely(vmf_orig_pte_uffd_wp(vmf))) entry = pte_mkuffd_wp(entry);`"). Markers exist
  because `change_pte_range` writes `PTE_MARKER_UFFD_WP` into none PTEs of file-backed uffd-wp
  VMAs ([`mm/mprotect.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/mprotect.c?h=v7.0),
  [`userfaultfd_wp_use_markers`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/linux/userfaultfd_k.h?h=v7.0): "File-based uffd-wp
  always need markers"). No async PF if the memfd page is resident (**derived**; swapped-out shmem
  and a concurrent hole punch can retry).
- **Missing page in an anonymous VMA registered for MISSING:** `do_anonymous_page` →
  `handle_userfault(vmf, VM_UFFD_MISSING)` (even for reads: the uffd check precedes the zero page)
  ([`mm/memory.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/memory.c?h=v7.0)). `FOLL_NOWAIT` became
  `FAULT_FLAG_ALLOW_RETRY | FAULT_FLAG_RETRY_NOWAIT` (`faultin_page`,
  [`mm/gup.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/gup.c?h=v7.0)), and `handle_userfault` returns at once, **before** queueing a
  message: "Handle nowait, not much to do other than tell it to retry and wait.
  `ret = VM_FAULT_RETRY; if (vmf->flags & FAULT_FLAG_RETRY_NOWAIT) goto out;`"
  ([`fs/userfaultfd.c`: `handle_userfault`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/userfaultfd.c?h=v7.0)). GUP turns that into 0
  pages (`-EBUSY` → `ret = 0`, `__get_user_pages`), `hva_to_pfn` into `KVM_PFN_ERR_NEEDS_IO`, and
  KVM goes async (1.2). The worker's `FOLL_WRITE` GUP then blocks in `handle_userfault` and
  userspace sees `UFFD_PAGEFAULT_FLAG_WRITE` from the kworker's tid. That is the ubu1 trace. If
  userspace resolves it by installing the page write-protected, the worker's GUP retries
  (`VM_FAULT_RETRY` after wake), finds a uffd-wp PTE, and faults again: `do_wp_page` →
  `handle_userfault(VM_UFFD_WP)`: a second, WP+WRITE message from the kworker for what was a load.

### 4.3 Guest store

- **Present uffd-wp PTE:** `FOLL_WRITE|FOLL_NOWAIT` slow GUP: `follow_page` refuses the read-only
  PTE, `faultin_page` with `FAULT_FLAG_WRITE|RETRY_NOWAIT` → `do_wp_page` →
  `userfaultfd_pte_wp` → `handle_userfault(vmf, VM_UFFD_WP)` ([`mm/memory.c`: `do_wp_page`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/memory.c?h=v7.0))
  → immediate `VM_FAULT_RETRY`, no message → `KVM_PFN_ERR_NEEDS_IO` → async PF (if allowed) → the
  worker's `FOLL_WRITE` fault arrives as WP+WRITE from the kworker. When the page was present and
  write-protected **at the vCPU's attempt**, that worker fault is a store. When async PF isn't
  possible, the same fault comes synchronously from the vCPU thread.
- **VMA without `VM_WRITE` (cladefs's "restricted" pages):** `check_vma_flags` returns `-EFAULT`
  for `FOLL_WRITE` before any fault handler or userfaultfd ([`mm/gup.c`: `check_vma_flags`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/gup.c?h=v7.0));
  `hva_to_pfn` sees `vma_is_valid(vma, true)` false → `KVM_PFN_ERR_FAULT` → `kvm_handle_error_pfn`
  → `-EFAULT` from `KVM_RUN`, no gpa (2). No async PF. The worker's `FOLL_WRITE` on such a VMA
  likewise fails at once without a userfaultfd message (**derived**).
- **`KVM_MEM_READONLY` memslot:** a write gets `KVM_HVA_ERR_RO_BAD` → `KVM_PFN_ERR_RO_FAULT` →
  `RET_PF_EMULATE` (`kvm_follow_pfn`, `kvm_handle_error_pfn`); api.rst: "writes to this memory will
  be posted to userspace as KVM_EXIT_MMIO exits" (with gpa and data). It needs the instruction
  emulator for every first store and one memslot per run (x86 `KVM_USER_MEM_SLOTS` = 32767 − 3,
  [`include/linux/kvm_host.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/linux/kvm_host.h?h=v7.0),
  [`arch/x86/include/asm/kvm_host.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/include/asm/kvm_host.h?h=v7.0)). Not practical
  for page-granular CoW (**derived**).

### 4.4 Answers

- Pre-installing pages read-only (PTE present, or uffd-wp marker over a resident memfd page) avoids
  async PF and the worker's write for **loads** of those pages.
- A store to such a page goes async; the worker's WP+WRITE fault is genuinely a store, except in
  the race where the page was missing at the vCPU's `FOLL_NOWAIT` attempt and got installed before
  the worker ran (then the worker goes straight to WP, and it may be a load). Nothing tells
  userspace which (the NOWAIT attempt leaves no message).
- A worker fault on a page that was missing is ambiguous (load or store).
- The vCPU's own retry after the worker (1.4) is synchronous and truthful; a failed worker costs
  only a wake-up.

## 5. How others share (or don't) memory across restores of one snapshot

### 5.1 Firecracker File backend

- [`src/vmm/src/vstate/memory.rs`: `snapshot_file` @v1.18.0](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/src/vmm/src/vstate/memory.rs)
  calls `create(..., libc::MAP_PRIVATE, Some(file), ...)`.
- [docs/snapshotting/snapshot-support.md @v1.18.0](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/snapshotting/snapshot-support.md):
  "The design allows sharing of memory pages and read only disks between multiple microVMs ...
  Firecracker creates a MAP_PRIVATE mapping of the memory file ... Any subsequent memory writes go
  to a copy-on-write anonymous memory mapping." The memory file "backs the guest OS memory for read
  access through the page cache".
- Async PF interaction (**derived**, not measured): a load of a page **in** the page cache resolves
  in the vCPU's `FOLL_NOWAIT` read GUP (`filemap_fault` finds an uptodate folio) and maps the page
  cache page read-only: shared. A load of a page **not** in the page cache: `filemap_fault` starts
  readahead, `lock_folio_maybe_drop_mmap` can't lock the folio under I/O and returns retry for
  `FAULT_FLAG_RETRY_NOWAIT` ([`mm/filemap.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/mm/filemap.c?h=v7.0)) → `KVM_PFN_ERR_NEEDS_IO` →
  async PF → the worker's `FOLL_WRITE` on a `MAP_PRIVATE` file VMA takes the CoW path
  (`set_pte_range`: "copy-on-write page ... `if (write && !(vma->vm_flags & VM_SHARED))`",
  `folio_add_new_anon_rmap`): a private anonymous copy for a load. The page cache page remains for
  other VMs. So with a cold page cache, much of what each VM touches first becomes private; with a
  warm page cache, loads share.
- Upstream UFFD backend: the handler "issues `UFFDIO_COPY` to load the previously mmaped file
  contents" ([docs/snapshotting/handling-page-faults-on-snapshot-resume.md @v1.18.0](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/snapshotting/handling-page-faults-on-snapshot-resume.md)):
  a private copy per VM.
- Secret hiding (guest_memfd, KVM userfault) is on cell `feature/secret-hiding` with patched
  6.18 kernels; snapshot-support.md: diff snapshots are in developer preview "while we are diving
  deep into how the feature can be combined with guest_memfd support".
- [docs/prod-host-setup.md](https://github.com/firecracker-microvm/firecracker/blob/cc88013eadcf432da1b37d47d445b7e8bcc3046e/docs/prod-host-setup.md):
  "Users should disable Kernel Samepage Merging to mitigate side channel issues that rely on page
  deduplication".

### 5.2 AWS Lambda / SnapStart

- [Lambda SnapStart docs](https://docs.aws.amazon.com/lambda/latest/dg/snapstart.html): Lambda
  "takes a Firecracker MicroVM snapshot of the memory and disk state ..., encrypts the snapshot, and
  intelligently caches it", "resumes new execution environments from the cached snapshot", "uses a
  single snapshot as the initial state for multiple execution environments". Nothing on how restored
  memory is backed or whether pages are shared on a host: **unverified**.
- ["Restoring Uniqueness in MicroVM Snapshots" (Brooker et al., AWS, arXiv 2102.12892)](https://arxiv.org/abs/2102.12892)
  is about RNG/uniqueness (vmgenid, `MADV_WIPEONFORK`), not memory backing.
- REAP (below) cites the Firecracker NSDI'20 paper and prod-host-setup for "disallows memory sharing
  for security reasons (as is the case in practice [5])" and "In a production serverless
  deployment, memory sharing is considered insecure and is generally disallowed [5, 59]".

### 5.3 E2B

At [`e2b-dev/infra` `d889992a`](https://github.com/e2b-dev/infra/tree/d889992a9600804247050328972e58ce5d0268df/packages/orchestrator/pkg/sandbox/uffd/userfaultfd)
(local `/Users/ramon/src/e2b-infra`):

- `faultPage` in `userfaultfd.go` reads the page from its source (`source.ReadAt(ctx, b, offset)`)
  into a buffer and installs it with `u.fd.copy(...)`, which is `UFFDIO_COPY` (`fd.go`:
  `func (f Fd) copy(...)`); holes are zero pages via `UFFDIO_COPY` of a zero buffer or
  `UFFDIO_ZEROPAGE`. Read faults are installed with `UFFDIO_COPY_MODE_WP` (`if accessType ==
  block.Read { mode = UFFDIO_COPY_MODE_WP }`) to track later writes.
- Access type is `UFFD_PAGEFAULT_FLAG_WRITE` as reported (`if pf.flags&UFFD_PAGEFAULT_FLAG_WRITE == 0
  { accessType = block.Read } else { accessType = block.Write }`), so an async PF worker's write
  counts as a write (**derived**).
- The only `MAP_SHARED` in the orchestrator is `block/memfd.go` mapping a memfd `PROT_READ` for
  reading pre-images (`CoWWindow` exports pause-time content); no `UFFDIO_CONTINUE`/MINOR mode, no
  mapping of shared pages into guest memory. Each sandbox holds a private copy of every page it
  touches; the orchestrator's cache holds the source once.

### 5.4 REAP (ASPLOS'21)

[Ustiugov, Petrov, Kogias, Bugnion, Grot, arXiv 2101.09355](https://arxiv.org/abs/2101.09355):

- Prefetch phase: the monitor reads the working-set file "from disk into a buffer in the monitor's
  memory with a single read system call. Then, the monitor eagerly and proactively installs the
  pages into the guest memory through a sequence of ioctl calls" (userfaultfd; the paper doesn't
  name `UFFDIO_COPY`, but installing from a buffer is a copy).
- The WS file is read with `O_DIRECT`, bypassing the page cache.
- "speeds up function cold start time by 3.7×, on average, without introducing memory overheads or
  memory sharing across function instances"; evaluation: "We disallow memory sharing among all
  function instances".

### 5.5 FaaSnap (EuroSys'22)

[Ao, Porter, Voelker, "FaaSnap: FaaS Made Fast Using Snapshot-based VMs"](https://www.sysnet.ucsd.edu/~voelker/pubs/faasnap-eurosys22.pdf):

- Per-region mapping: "FaaSnap only maps the pages that are non-zero to the guest memory file. Zero
  pages are instead mmap'd to anonymous host memory."
- Concurrent paging: a loader thread reads pages so "the page will be served from the page cache,
  resulting in a faster minor page fault"; working set recorded with `mincore`.
- Code ([`ucsdsysnet/faasnap-firecracker` `84206e97`, `src/vmm/src/memory_snapshot.rs`](https://github.com/ucsdsysnet/faasnap-firecracker/blob/84206e97b8de0f340bcc820069e423aaf4648e69/src/vmm/src/memory_snapshot.rs)):
  `libc::MAP_FIXED | libc::MAP_NORESERVE | libc::MAP_PRIVATE` of the memory file over an anonymous
  base. Page cache shared, CoW on write, so the async PF effect of 5.1 applies (**derived**; the
  paper doesn't discuss async PF).

### 5.6 guest_memfd with mmap

- `GUEST_MEMFD_FLAG_MMAP` and `GUEST_MEMFD_FLAG_INIT_SHARED` first appear in uapi in **v6.18**
  (absent in v6.17).
- `kvm_gmem_mmap` requires `VM_SHARED | VM_MAYSHARE`, else `-EINVAL`
  ([`virt/kvm/guest_memfd.c` @v7.0](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/virt/kvm/guest_memfd.c?h=v7.0); same at v7.3 and v7.4-rc3):
  no `MAP_PRIVATE`, no CoW.
- api.rst [KVM_CREATE_GUEST_MEMFD](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/Documentation/virt/kvm/api.rst?h=v7.0): files "are bound to
  their owning virtual machine", "For a given guest_memfd file, there can be at most one mapping per
  page, i.e. binding multiple memory regions to a single guest_memfd range is not allowed". With
  `GUEST_MEMFD_FLAG_MMAP`, "the fault will always be consumed from guest_memfd". So no sharing of a
  page between VMs.
- KVM faults on gmem-only slots call `kvm_gmem_get_pfn` directly (no GUP, no async PF) and annotate
  failures with gpa (2).
- userfaultfd for guest_memfd: Mike Rapoport, "[PATCH v2 00/15] mm, kvm: allow uffd support in
  guest_memfd", 2026-03-06 (MISSING via `UFFDIO_COPY`, MINOR via `UFFDIO_CONTINUE`; "Only support
  userfaultfd for guest_memfd with INIT_SHARED flag"; WP not mentioned), superseded by a v4 posted
  2026-04-02 (read via the [list mirror](https://ratatoskr.run/kvm/2026/03/342569/t); RFC
  Message-ID `20260127192936.1250096-1-rppt@kernel.org`). The mm side `struct vm_uffd_ops` is in
  v7.4-rc3 ([`include/linux/userfaultfd_k.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/linux/userfaultfd_k.h?h=v7.4-rc3)); no uffd
  support in `virt/kvm/guest_memfd.c` at v7.4-rc3. Copies either way.

## 6. Options for cladefs on kernel 7.0, and what newer kernels add

Signals available on 7.0 (from 1 and 4):
(A) a userfaultfd fault from a vCPU thread is truthful (synchronous path, or the page-ready retry);
(B) a kworker fault on a page that was missing is ambiguous;
(C) a kworker WP fault on a page present and write-protected at the vCPU's attempt is a store;
(D) a failed worker is harmless and is followed by the vCPU's truthful retry;
(E) `KVM_RUN` EFAULT on an ordinary memslot has no gpa.

### 6.1 Map the daemon's memfd runs `MAP_PRIVATE` (kernel CoW), no uffd-wp

What changes: install runs of the daemon's memfd pages with `MAP_PRIVATE` (`PROT_READ|PROT_WRITE`)
in place of `MAP_SHARED` + `UFFDIO_WRITEPROTECT`.

- Load of a mapped page: GUP read finds the page cache page (resident memfd page: no retry), SPTE
  read-only, shared. No async PF, no userfaultfd (**derived**, 4.2 and 5.1).
- Store: GUP `FOLL_WRITE|FOLL_NOWAIT` → `do_wp_page` → CoW (allocation, no I/O, no retry) →
  private anonymous page; no async PF, no userspace round trip (**derived**). The VMM's own writes
  CoW the same way, so no tid bookkeeping for them.
- Private pages are PTE-level anonymous CoW pages inside the file VMA: no mapping per private page,
  so they don't count against `vm.max_map_count` (the runs still do, TASK-12).
- Costs and open versions: private pages are anonymous memory of the VM process (charged to its
  cgroup, consistent with the 2026-11-15 decision that memory windows reserve nothing in extentd),
  outside `PrivateMemory`, so counting and snapshotting them needs `/proc/pid/pagemap` or
  `PAGEMAP_SCAN`, or KVM dirty logging; the daemon must not punch or evict memfd pages under live
  mappings (truncation unmaps page cache pages from private VMAs; the CoW'd ones stay); a read of a
  memfd hole through a `MAP_PRIVATE` mapping allocates a page in the memfd (`shmem_fault` uses
  `SGP_CACHE`), so holes must stay anonymous mappings (they are, as zero runs); balloon
  `MADV_DONTNEED` on a private file mapping reverts to the file's page, not zeros (Firecracker's
  memory.rs comment "regions keep `Some(file_offset)` + `MAP_PRIVATE`, routing balloon discards
  through ..." is about this). First touches in regions not yet mapped still come through the
  worker (B): map node runs eagerly, or treat as in 6.2.
- Whether a write-sealed or read-only memfd fd allows a writable `MAP_PRIVATE` mapping on 7.0:
  **unverified**.

### 6.2 Keep uffd-wp, but install before first touch and fail ambiguous worker faults

- Install every page of a node (the 170-page tree node cladefs maps on first fault) when the node
  is mapped, not on each page's first touch: loads of those pages then never go async (4.2), and
  later worker WP faults on them are stores (C).
- For the remaining ambiguous worker faults (B, and WP faults on pages installed between a vCPU's
  attempt and its worker): make the worker fail (page `PROT_READ` so `check_vma_flags` refuses
  `FOLL_WRITE` before userfaultfd), wake it, and let the vCPU's page-ready retry decide (D): a load
  succeeds read-only; a store fails `KVM_RUN` with EFAULT (E), and the VMM lifts only the pending
  restrictions, after which the store's retry goes async again and its worker WP fault is a store.
- Costs: one VMA split and merge per restricted page (`mprotect`, mmap_lock for writing, TLB
  flush), one worker round trip plus APF halt per ambiguous first touch, and the EFAULT round trip
  for stores to restricted pages. Racy windows must be bounded by bookkeeping (**derived**, not
  designed or measured here).

### 6.3 Turn async PF off: `KVM_X86_DISABLE_EXITS_HLT` + guest PV async PF off

- Enable `KVM_CAP_X86_DISABLE_EXITS` with `KVM_X86_DISABLE_EXITS_HLT` before creating vCPUs, and
  restore `MSR_KVM_ASYNC_PF_EN` with the enable bits clear (or hide `KVM_FEATURE_ASYNC_PF_INT`;
  don't advertise `KVM_FEATURE_PV_UNHALT`). Then `kvm_can_do_async_pf` is false and every fault on
  guest memory comes from the vCPU thread with the true access (1.2, 1.5): loads stay shared, stores
  are WP faults, nothing ambiguous.
- Costs: a guest `HLT` no longer exits, so an idle vCPU thread stays in guest mode and the host sees
  it as busy (api.rst: suggested for dedicated physical CPUs), against "hundreds of forks per
  host"; a vCPU blocked on userfaultfd blocks the whole vCPU (no other guest task runs meanwhile);
  allowed only with `mitigate_smt_rsb` off, with a warning on SMT-RSB-affected CPUs. If the guest
  ever re-enables the MSR (CPU hotplug, kexec), async PF returns for CPL3 faults.

### 6.4 Learn gpa and access from the `kvm:kvm_page_fault` tracepoint

- `handle_ept_violation` calls `trace_kvm_page_fault(vcpu, gpa, exit_qualification)`
  ([`arch/x86/kvm/vmx/vmx.c` @v7.0](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/vmx/vmx.c?h=v7.0)); SVM and the shadow MMU trace
  too (svm.c, mmu.c). The event holds `vcpu_id`, `guest_rip`, `fault_address`, `error_code`
  ([`arch/x86/kvm/trace.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/kvm/trace.h?h=v7.0)). A BPF program keyed by vCPU thread
  could give the gpa and write bit of the fault behind an unannotated EFAULT (E) or an async PF.
- Costs: needs `CAP_BPF`/`CAP_PERFMON` (or root) on the host, a probe on every EPT violation of
  every VM, and correlation logic. Not tried (**derived**).

### 6.5 What newer kernels would add (none merged as of v7.4-rc3)

- **Memory protection attributes** (Paolo Bonzini, "mostly the work of Nicolas Saenz Julienne"),
  "[RFC PATCH 00/24] KVM: x86: Introduce memory protection attributes", 2026-07-16
  (`20260716181456.402786-1-pbonzini@redhat.com`); v2/v3 2026-09-18
  (`20260918135030.171564-1-pbonzini@redhat.com`), v4 2026-11-02 per the
  [list mirror](https://ratatoskr.run/kvm/2026/07/17271346/t) (v4 content **unverified**).
  `KVM_SET_MEMORY_ATTRIBUTES` gains `KVM_MEMORY_ATTRIBUTE_NR/NW/NX`; violating accesses exit with
  `KVM_EXIT_MEMORY_FAULT` and `KVM_MEMORY_EXIT_FLAG_READ/WRITE/EXEC`; patch 04: checking attributes
  first "lets a fault that violates the attributes exit to userspace without the cost of gup and/or
  an async #PF"; its selftests use ordinary (non-guest_memfd) VMs; NW is refused together with
  PRIVATE. For cladefs: NW on every non-private gfn makes every guest store exit on the vCPU thread
  with gpa and WRITE, before GUP; any userfaultfd fault on an NW gfn is then a load. Not in v7.4-rc3
  uapi.
- **KVM userfault** (3): exits with gpa (no access type) before GUP for marked pages, so a page is
  installed before the vCPU's GUP ever sees it; then loads never go async and worker WP faults are
  stores (closes the race of 4.4). Plus Amazon's userfault async PF on top (3).
- **Stage-2 EFAULT annotation with RWX** (2): would give gpa and access for (E).
- **userfaultfd for guest_memfd** (5.6): post-copy for guest_memfd, still copies, no cross-VM
  sharing.

## Measurements on ubu1 (2026-11-16)

Every fault on a restored VM's memory window logged (thread, access, write-protect flag, whether the
page was installed), cell t456-measure, fxfs-guest-snapshot-memory-volume-dax: a 512 MiB VM
snapshotted, its memory file imported into a memory volume, two VMs restored from its version one after
the other, each running the case's guest workload (thaw, reads, writes of a few MiB, remount).

| Faults on the memory window | VM a | VM b (restored after a) |
|---|---:|---:|
| kworker write, page not installed (resolved as a load) | 7,608 | 8,124 |
| kworker write, page installed write-protected (made private) | 10,222 | 10,242 |
| vCPU write, page installed write-protected | 382 | 399 |
| vCPU read, page not installed | 187 | 138 |
| vCPU write, page not installed | 72 | 64 |
| Firecracker's own threads | 12 | 12 |

So the guests' first touches reach the window almost only through the async PF worker, and its
write-protected retry makes each page private: about 10,000 private pages (some 40 MiB) a VM, VM b's
as many as VM a's though its pages were already in the daemon's memory, which grew 2 → 86 MiB with
VM a and 4 MiB more with VM b. The daemon holds each page once; each VM holds its own copy of nearly
every page it touched. Pages installed before the guest's first touch (option (b)) or mapped so
that loads never reach userfaultfd (option (a)) would take the kworker out of loads.

A guest writing all its free memory (the same case with a write-all step): 31,859 faults, 14,599
kworker writes of pages not installed and 16,441 kworker write-protected writes; and one page
restricted for a worker's write was never lifted while the vCPU's store failed for a second: exit
159 (AccessRefused).

### After installing a node's pages ahead (option (b))

The same case, the first restore's memory window traced again, with each tree node's 170 pages read
into the daemon, mapped and installed as the node's first page faults, and every write to the window
a store:

| Faults on VM a's memory window | before | after |
|---|---:|---:|
| kworker write, page not installed | 7,608 | 125 |
| kworker write, page installed write-protected (a store) | 10,222 | 10,394 |
| vCPU write, page installed write-protected | 382 | 457 |
| vCPU read or write, page not installed | 259 | 75 |

First touches of pages not installed fell from 7,608 to 125: the guest's loads find their pages
installed and take no fault here. What stays is its stores: the window mapped 33,331 pages at most,
11,004 of them private, about a third, the case's files read into the guest's page cache among them;
the rest, some 22,000 pages, are the daemon's, shared.
