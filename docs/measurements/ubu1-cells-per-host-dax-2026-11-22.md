# How many DAX cells ubu1 holds, and what each costs (boards 5583, 5649, 5662), 2026-11-22

cells-per-host-dax (guest/tests/test_cases/src/test_cells_per_host.rs): idle sandboxes, 1 vCPU and
256 MiB each, booted on forks of one small Alpine base through a 1 GiB DAX window, each probing
`echo`, `cat` of a base file and a 4 KiB fsynced write every second, added in steps until the
host holds no more. On ubu1 (Ryzen 9 8945HS), each run a `hotbed run --bench` measurement (cores
4-7 and their siblings, isolated) of a 16 GiB cgroup (its `memory.max` set to 16G, its
`memory.low` 16G), every Firecracker in its jailer's jail, a release guest kernel. The drivers
still call fxjob (board 6058), so each ran by hand: scripts/lib/test-linux-host.sh's `build`
phase as hotbed work, then its `measure` phase as the measurement, its results on the scratch
disk's ext4 (in the workspace's ZFS a fork fails, board 6589). 16 GiB, not the driver's 20: ubu1
had no more to guarantee beside others' work. The case logs are in
[ubu1-cells-per-host-dax-2026-11-22/](ubu1-cells-per-host-dax-2026-11-22).

| run | tree | VMs held | limit | guest RAM touched (vmm_anon) | boot p99 |
| --- | --- | --- | --- | --- | --- |
| r5649a | c2ad3dad8: a full host's fixes (board 5662), the breakdown, no cut | **133** | memory | 95.3 MiB | 490 ms |
| r5649b | e3855c931: every cut of board 5649 | **181** | memory | 72.2 MiB | 400 ms |
| r5649c | r5649b's build, `CLADEFS_GUEST_BALLOON=0` | 151, then failed | (a VM's ready budget, below) | 72.2 MiB | 490 ms |
| r5649d | 9557bf9f5: r5649b and the ready budget per VM | **181** | memory | 72.2 MiB | 410 ms |
| r5649e | r5649d's build, `CLADEFS_GUEST_BALLOON=0` | **180** | memory | 72.0 MiB | 400 ms |

Every probe's p99 was 0 ms (rounded) in every window of every run, fsync's at most 31 ms; the
budgets (100 ms, 250 ms for fsync) were never near. No VM exited or went silent, the daemon
logged no ENOMEM, no page-in waiting for memory, and let no VM process go.

## A full host keeps its DAX VMs (board 5662)

Before board 5662 a DAX ramp on ubu1 died at 159 VMs, its guests' faults failing ENOMEM as the
governor squeezed the daemon's memory to one chunk under the VMs' working set. In r5649a the
governor held it at its floor, 256 MiB (40 MiB held at 133 VMs), said the host full under it and
no longer full past it and a quarter more (once in r5649a, d and e; 9 times in r5649b, whose last
steps added one VM each across the edge), and the ramp stopped at its memory limit.

## Where a VM's memory goes, and what each cut took (board 5649)

At each run's last step, MiB, the mean of the VMs running:

| | r5649a | r5649d | change |
| --- | --- | --- | --- |
| guest RAM its kernel never had (`guest_reserved_mib`) | 31.6 | 30.3 | -1.3 |
| guest memory not available (`guest_used_mib`) | 47.6 | 42.8 | -4.8 |
| of it, slab / stacks / page tables | 9.6 / 4.0 / 0.6 | 9.1 / 3.5 / 0.6 | |
| of it, the window's struct pages (`guest_memmap_mib`) | 4.0 | 0.2 | -3.8 |
| the VM process's anonymous memory, the guest RAM touched (`vmm_anon_mib`) | 95.3 | 72.2 | -23.1 |
| the VM process's Pss (`vmm_pss_mib`) | 98.9 | 74.9 | -24.0 |
| KVM's page tables for it | 0.3 | 0.2 | |
| the daemon's own memory, over the VMs | 2.1 | 2.1 | |

- **The window's struct pages made 2 MiB at a time** (f3d51c71f): 4.0 to 0.2 MiB, all the
  guest's memmap the window had needed. Boots faster too (490 to 400-410 ms p99): fewer struct
  pages made.
- **The guest kernel's config trimmed** (b3befb7b5): 1.3 MiB of the RAM its kernel keeps.
- **The transfer heap zeroed as buffers reach it** (7e1a311a3): the rest of the RAM touched,
  about 18 MiB. In r5649a the host held 16.1 MiB of guest RAM beyond what the guest's kernel had
  or used (95.3 against 31.6 + 47.6): the heap's 16 MiB, zeroed at mount. In r5649d, none (72.2
  against 30.3 + 42.8): the heap's pages no buffer reached are allocated to the guest's kernel,
  still counted used, but never touched.
- **The balloon's free page reporting** (379bd9dda): nothing here, r5649d against r5649e, 181
  VMs against 180 and the same memory to 0.2 MiB. An idle guest frees almost nothing once
  booted, so there is little to report. What it gives back is a guest's own freed pages after a
  workload that takes memory and frees it, which this case doesn't run. It stays on, the user's
  call.
- Not tried in this batch: 16 KiB kernel stacks (Fxfs's mount needs 34, TASK-26.9), and every
  cut past the guest's (private pages, the VM process's bookkeeping, the daemon's work per VM).

The VMs held is the number to compare. The step lines' `per_vm_mib`, the room lost since the
first VM over the VMs running, moves with what the daemon gives back of its cache and what the
cgroup's page cache holds: 64, 73 and 87 MiB at about 150 VMs in three runs of one tree.

## The ready budget (r5649c)

r5649c failed at 151 VMs: `cell-151's guest: the guest didn't connect within 0ns`. Spawning a
step's 16 jailed VMs took 114 s there, growing with the VMs held (the jail's chown of every file
of the jail, board 5588), and every VM's 120 s to be ready ran from its step's first spawn. Each
VM's runs from its own since 9557bf9f5; r5649d's steps spawned for up to 119 s and passed.
