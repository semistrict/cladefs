board 5519: warm cladefs sandboxes against the same VM on an ext4 image on virtio-blk, no cladefs.

Host: ubu1. `scripts/python-sandbox-host.sh` at 15ba7bd2e; notebook workload, 2 rounds, each round's
three forks then the baseline: the same VM (2 vCPUs, 2 GiB), image and notebook, booted on an ext4
image of the image's root directory on a virtio-blk disk, the image just made, so in the host's
page cache, as a warm host's base is in its memory. The baseline's clock starts once its image is
made.

Milliseconds from just before the fork (cladefs) or the VM's start (ext4); boot is the guest's own:

| sandbox | fork | VM started | boot | marimo serving | answered |
|---|---:|---:|---:|---:|---:|
| ext4-r1 | - | 11 | 191 | 2,289 | 5,232 |
| ext4-r2 | - | 11 | 206 | 2,321 | 5,245 |
| a2 (cladefs, warm) | 346 | 357 | 238 | 2,900 | 6,075 |
| b2 (cladefs, warm) | 340 | 351 | 230 | 2,857 | 6,018 |
| c2 (cladefs, warm) | 342 | 353 | 234 | 2,866 | 6,035 |

A warm cladefs sandbox answers in 6.04 s, the baseline in 5.24 s: cladefs adds 0.8 s (15%), 0.34 s
of it the fork, and 0.44 s more from the VM's start to the answers (5.69 s against 5.24 s), the
guest's page-ins through its DAX window against ext4's reads from the host's page cache. Python is
the rest, on either disk: marimo's server serving at 2.3 s from the VM's start, the notebook's
answers 2.9 s later, compiling 1,640 modules' bytecode (the base holds none) in both.

The first round's cold sandboxes: a1 7,250 ms, b1 7,904 ms, c1 7,896 ms (TASK-702.3's prefetch).
The warm forks took 340-346 ms here, against 460-467 ms at 3c5ae9b37 (board 5517).
