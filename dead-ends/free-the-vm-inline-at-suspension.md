# Close the freed KVM VM on the suspending thread

**Tried:** 2026-11-15, board 13213, branch t3/snapshot-network-suspension (the first version of
`Machine::freeze`, never committed).
**Idea:** a suspension frees the machine: drop its live parts and the last handle on the VM fd,
and the VM is gone when `suspend` returns, nothing left behind.
**Result:** release build, 256 MiB test guest: later suspensions took 26-30 ms median (board 13203's,
which kept the VM: 5 ms). Timed by phase: saving and writing 2.6 ms, dropping the vCPU and
device threads and unmapping the RAM 45 us, closing the VM fd 19-27 ms. KVM destroys the VM in
`close` (its memory map's teardown, grace periods), on the calling thread.
**Why it's dead:** that time is the kernel's and nothing waits on it: no caller needs the VM gone
before the suspension returns, only soon after. The fd now closes on a short-lived thread
(`MemorySlots::retire`): 5.7 ms per suspension.
**Reopen if:** KVM makes VM destruction cheap, or threads become scarce enough that one per
suspension for ~25 ms matters (then a single long-lived reaper thread).
