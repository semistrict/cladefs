# Prefetching a fork's predicted reads into the part cache alone, not into memory

**Tried:** 2026-11-21, TASK-702.3 (board 2902), ee69c94e3; undone by board 6225.
**Idea:** A base on local disk measured as fast as one in memory (0.20 s), and the part cache
charges no tenant's arena: fetching the predicted parts onto the host's disk ahead of the guest
should be enough, the guest's page-ins reading them from there.
**Result:** On GCE (16 vCPU, nested KVM, DAX, Node + Codex, `codex --version`), a fork whose
version was in the part cache but not in daemon memory took 1,103-1,160 ms to its first command,
against 212-223 ms with TASK-38's replay into the extent cache, and 209-216 ms in memory.
**Why it's dead:** The 0.20 s was with the host's page cache warm. The part cache is read
bypassing it (one memory budget, TASK-433), so each guest page-in of a part waits on a disk read
and an unpack, one part at a time as the guest asks. The prefetch skipped every part the part
cache held, so nothing put them in memory ahead of the guest.
**Reopen if:** Reading a part from the part cache costs what a page already in memory does.
