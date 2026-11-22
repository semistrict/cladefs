# VMs restored from a zygote on ubu1: 497 held in 16 GiB, at about 12 MiB each (board 7084), 2026-11-22

cells-per-host-dax-zygote (guest/tests/test_cases/src/test_cells_per_host.rs): as cells-per-host-dax
([ubu1-cells-per-host-dax-2026-11-22.md](ubu1-cells-per-host-dax-2026-11-22.md)), idle sandboxes
of 1 vCPU and 256 MiB, a 1 GiB DAX window each, probing `echo`, `cat` and a 4 KiB fsynced write
every second, added 16 at a time; but no VM boots except the zygote. The zygote boots on a fork
of the base, runs its probes once and freezes its root. A version is made of it: its root's, and
its memory imported into a memory cell, of which a version is made too. Each VM of the ramp
restores the zygote: its root a fork of the zygote's root version, its memory a fork of the memory
cell's version, a copy-on-write window the daemon serves, shared with every other VM's but the
pages it writes (TASK-456.3).

Run z5, tree 16daf8e1e: ubu1, a `hotbed run --bench` measurement of a 16 GiB cgroup, jailed
Firecrackers, a release guest kernel, results on the scratch disk's ext4. Its log is
[ubu1-cells-per-host-dax-zygote-2026-11-22/z5-log.txt](ubu1-cells-per-host-dax-zygote-2026-11-22/z5-log.txt).

| | booted (r5649d; r5649f, on main's tree since, held 184) | restored from the zygote (z5) |
| --- | --- | --- |
| VMs held | **181**, its memory full | **497**, then the daemon's connection cap (below), 3.9 GiB of room left |
| the VM process's Pss (`vmm_pss_mib`) | 74.9 | **12.1** |
| of it, its anonymous memory, private (`vmm_anon_mib`) | 72.2 | 2.9 |
| of it, its share of the daemon's pages it maps (`vmm_shmem_mib`) | | 7.8 |
| the daemon's own memory, over the VMs (`daemon_anon_mib`) | 2.1 | 3.8 |
| a VM's start, p99 | 400 ms boot | 1.1 s at 17 VMs, 70 s at 497 (below) |
| probes' p99 | 0 ms, fsync's at most 31 ms | 0 to 4 ms at 497, fsync's at most 51 ms |

The guest is the same in both: it says 30.3 MiB its kernel never had and 43.7 MiB used. Booted,
the host holds all of that for each VM; restored, it holds once what every VM's zygote pages are,
and for each VM only the pages it wrote since. The room fell from 11.9 GiB at one VM to 3.9 to
6 GiB at 497, 12 to 16 MiB a VM (the room moves with what the daemon and the cgroup's page cache
give back), against about 89 MiB a VM booted (16 GiB over 184). At that cost 16 GiB holds 750 or
so.

## What stopped it

- **The daemon's connection cap.** The 513th restore failed: `refusing a VM process's connection:
  its tenant has 1024`. A restored VM holds two connections, its root's and its memory's.
  `server::MAX_CONNECTIONS` is 4096 since b40688c76; a run past 512 (z6) failed at 161 VMs on a
  metrics scrape the daemon answered after more than 10 s (board 7416), its disk shared with
  another job.
- **Restores slow as the disk fills.** A step's 16 restores took 12 s at 17 VMs and 297 s at 497,
  the slowest single restore 70 s: past the 10 s boot budget from 241 VMs on (`past=boot` every
  other step). Memory was not short; the disk was: 497 VMs fsync 4 KiB a second each, the daemon's
  commits were durable 2 to 7 s after they were ordered, framing waited 8 s, and its file bucket is
  on the same disk. Each fork waits behind those (board 7420).
- One restore of 209 in run z4 soft-locked at once, 125 s in softirqs, and its guest panicked
  (board 7261).

## Fixed on the way

- A jailed spawn chowned the test's whole directory, every cell in it, each time: spawns grew from
  1 s to 5.5 s each by 49 VMs. It now gives the jail's user the directory and the images its
  Firecracker writes; the cells keep root as their owner (board 5588).
- `accept_within` failed a guest that had connected once its deadline passed, unaccepted: a step's
  last VMs, spawned last, were failed for the step's spawning.
- fxfs-guest-snapshot-memory-cell-dax restored with memory backend `CellId`, which Firecracker no
  longer takes (`Cell`, board 7151).
- A restored VM's start is timed as its `/snapshot/load` and its guest's reset found to its first
  round done, not when the host got round to reading it.
