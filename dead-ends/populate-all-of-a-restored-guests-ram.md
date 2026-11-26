# Prefault all of a restored guest's RAM (MAP_POPULATE on the snapshot mapping)

**Tried:** 2026-11-14, board 13177, branch batch-50-45-52 (an environment-gated
`MAP_POPULATE` on the snapshot's RAM mapping in `backend/kvm.rs guest_ram`,
never committed).
**Idea:** a restored guest is slow at first (it takes the vsock transport
reset ~2-3 ms after its vCPU starts, against ~0.2 ms for the same guest once
warm) because each first touch of a page costs an EPT fault plus a host fault
on the copy-on-write file mapping. Mapping every page up front should take
those faults out of the first agent answer.
**Result:** with the RAM populated, the window from the restore's end to the
guest having taken the reset shrank from ~2.2-3.7 ms to ~1.0-1.4 ms (host
faults in that window: ~190 down to 1), but the restore itself went from
~2.5 ms to 120-600 ms for 256 MiB (65,536 faults), and it reads every hole
of the sparse file into the page cache.
**Why it's dead:** the guest touches a few MiB of its RAM before its first
command (the snapshot file's page cache holds 4-7 MiB after 100 clones), so
populating all of it does ~50x the work it saves; the cost grows with RAM
size, the saving does not.
**Reopen if:** only the pages a clone will touch are prefaulted (a working
set recorded from a first clone, then `KVM_PRE_FAULT_MEMORY` or
`MADV_POPULATE_READ` over those ranges), or the snapshot's RAM is mapped with
huge pages (e.g. a THP-backed memfd: this host has shmem THP off), so that
faulting it in costs a few hundred faults rather than tens of thousands.
