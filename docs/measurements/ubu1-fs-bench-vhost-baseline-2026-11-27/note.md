# Baseline of today's extent-device path (board 14378, step 0), 2026-11-27

Today's path: guest → Firecracker's extent device (`virtio_extent`) → `CellClient` over the daemon's
socket → cladefsd. Source `67dd2c6d55d7df9e93261a4e43623cd0b83cf097` (main), `scripts/bench-host.sh`,
default cases (`--tag bench`), 3 iterations, on ubu1.

**Unisolated, so indicative only:** run without `hotbed run --bench`
(`CLADEFS_TEST_HOST_BENCH=0`), because `--bench` can't run on amd64 nodes yet (hotbed board 14487).
Other sessions shared ubu1's CPUs. Rerun isolated once 14487 lands, before any later step is held
against these numbers.

The driver couldn't fetch the run's results directory: a hotbed controller outage cancelled its
bookkeeping command ("resume can't find it"). `comparison.md` is the run's own report, taken from
the command's output (hotbed command rinzyuj6). It exited 1 as the report lists workloads more than
2× slower than ext4, not because a case failed.

The extent daemon's serving time per request (its own table in `comparison.md`): read 251 µs,
write 326 µs, map 72 µs, unmap 19 µs, fault 397 µs, commit 31.5 ms (object storage 259 µs of each
of the 20,570 commits). These are what the later steps' daemon-side serving must not exceed.
