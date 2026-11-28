# The extent daemon governed against a fixed 1 GiB, perf gate and cells-per-host (board 14529), 2026-11-28

Since board 14529 every run starts its daemon governed, as production does (no `MEMORY_BYTES`:
`crates/cladefsd/src/governor.rs`). This A/B measures what that changes. On the fixed side,
`CLADEFSD_MEMORY=1073741824` fixes every daemon of the run at 1 GiB, which is what hotbed runs
had before board 14529: test-e2e.sh fell back to it when a host lacked MGLRU or swap.

Each side ran `scripts/perf-gate-host.sh` and `scripts/cells-per-host-host.sh` on ubu1 (Ryzen 9
8945HS), in a hotbed workspace VM. Those VMs now have MGLRU, zram swap and a cgroup handed to
the command's user (hotbed issue 14, hotbed 2c58a723). Each measurement was a `hotbed run
--bench`, with a release guest kernel.

**Every Firecracker was unjailed on both sides** (`CLADEFS_FIRECRACKER_JAIL=`). In a workspace,
the jailed runs failed every case with `setpriv` EPERM (board 14621, fixed by fc41dba53 after
these runs). The trees, synced from one worktree: governed perf gate at ed4c7dc6b/fc41dba53
(r14529pg2); fixed perf gate (r14529pf2, r14529pf3) and both cells-per-host runs (r14529cg2,
r14529cf2) at 3c7c778df. That commit only changes which files the drivers copy back. Each run's
results came back from hotbed truncated (hotbed issue 26). They were rebuilt from the fetch
command's stored output (`hotbed output ID --from/--to`), and each archive decoded whole.

## Perf gate (`scripts/pgbench-gate.py`)

pgbench tps, median of runs 2–5 (run 1 warms up), one VM a case:

| case | governed | fixed 1 GiB |
| --- | --- | --- |
| Fxfs, no DAX | 2235 | 2271 |
| Fxfs, DAX | 1931 | 2087 |
| Fxfs, no DAX, forked | 2082 | 2229 |
| Fxfs, DAX, forked | 1748 | 1724 |
| ext4 virtio-blk, unsafe | 4424 | 4204 |
| ext4 virtio-blk, writeback | 2546 | 2678 |
| ext4 DAX pmem | timed out | 211 |
| ext4 loop (both) | failed: needs root | failed: needs root |

The ext4 cases run no extent daemon. One of them timed out on the governed side only, and the
loop cases need root (board 14635).

bigmap: a 4 GiB file in a base, mapped shared, read all over, 5% of it written:

| | governed | fixed 1 GiB |
| --- | --- | --- |
| no DAX: read cold / warm, MiB/s | 31.5 / 48.9 | 2.07 / 2.63 |
| no DAX: pages written/s, store p99 | 11571, 178 µs | 643, 3125 µs |
| DAX: read cold / warm, MiB/s | 76.4 / 10854 | 2.08 / 2.09 |
| DAX: pages written/s, store p99 | 4705, 430 µs | 529, 3226 µs |
| DAX: host memory per byte written | 1.17 | 1.86 |

A fixed 1 GiB can't hold the 4 GiB file, so every pass reads it from the bucket again. Governed,
the daemon holds as much of it as the host's memory allows.

Density: 3 VMs on forks of a base holding the database, all served by one daemon:

| | governed | fixed 1 GiB |
| --- | --- | --- |
| no DAX: first VM, MiB | 3159 | 2585 |
| no DAX: each added VM, MiB | 2229 | 957 |
| no DAX: daemon's memory with 3 VMs, MiB | 4005 | 1015 |
| no DAX: tps of runs 2–5 | 1851 | 1881 |
| DAX: first VM / each added VM, MiB | 2686 / 1797 | did not run (board 14649) |

Governed, a no-DAX VM costs more host memory because the daemon caches into memory the host has
free. That is the design: the governor gives the cache back as the host runs short. With DAX,
the daemon stays near 0.8 GiB.

Gates failed:
- governed (2): `dax` 0.853 (at least 0.95), `bigmap-read` 2.43 (at least 4.0)
- fixed (4): `dax` 0.916, `forked-dax` 0.748 (at least 0.8), `bigmap-read` 1.01, `bigmap-written-memory` 1.86 (at most 1.5)

## Cells per host (`scripts/cells-per-host.py`), 16 GiB job

| case | governed: VMs, limit | MiB/VM | boot p99 | fixed 1 GiB: VMs, limit | MiB/VM | boot p99 |
| --- | --- | --- | --- | --- | --- | --- |
| cells-per-host | 160, memory | 86.4 | 830 ms | 162, memory | 85.8 | 850 ms |
| cells-per-host-dax | 162, memory | 86.2 | 840 ms | 161, memory | 87.2 | 840 ms |
| cells-per-host-dax-zygote | **608**, memory | 22.5 | 216 ms | **417**, tick latency | 26.5 | 947 ms |

Plain idle VMs hold the same either way, since each VM's own memory sets the limit. With the
zygote, the VMs share their memory through the daemon. At 1 GiB the daemon pages that memory in
and out, and the probes' latency stopped the ramp at 417 VMs. Governed, the host ran out of
memory first, at 608 VMs.

Each case "failed" on both sides, but only on a reclaim check that ed4c7dc6b added: "the host's
memory ran out and the governor reclaimed none of its VMs'" (`vm_reclaim_mib` 0, `daemon_swap_mib`
0). That check was wrong, and 6a2b54cad removed it. The ramp stops (`Limit::Memory`) once
MemAvailable plus cache, less headroom and the floor, leaves no room for one more VM. That
happens before MemAvailable falls below the headroom, the only point where the governor
reclaims VM memory. This is why the fixed side failed the check too. The step lines still report
`vm_reclaim_mib`. Whether the governor should treat hotbed's `memory.high` (16 GiB here, against
a reported capacity of 19.4 GB) as a limit is a separate question (board 14659).
