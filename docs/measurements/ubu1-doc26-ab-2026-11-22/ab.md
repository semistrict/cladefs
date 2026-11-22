## CONTAMINATED: others wrote beside these rounds; their timings are left out, their counts and sizes kept

- base-r3: others wrote 21.2 MB/s to its disk over the round, bound 10 (rpool/hotbed/ws/nfhvbft4 15.3, zd0 4.0, rpool/hotbed/ws/aihhiixv 0.8); others wrote 65.1 MB/s to its disk in the minute 240 s in, bound 50
- base-r4: others wrote 42.2 MB/s to its disk over the round, bound 10 (rpool/hotbed/bases/cladefs/g22 20.4, rpool/hotbed/ws/nfhvbft4 16.0, zd0 2.5); others wrote 68.8 MB/s to its disk in the minute 60 s in, bound 50
- base-r5: others wrote 26.9 MB/s to its disk over the round, bound 10 (rpool/hotbed/ws/nfhvbft4 11.3, rpool/hotbed/bases/cladefs/g22 8.0, zd0 3.4)
- head-r2: others wrote 10.2 MB/s to its disk over the round, bound 10 (zd0 6.5, rpool/hotbed/ws/2alsvm2p 1.7, rpool/hotbed/ws/lx5ysq7g 1.6)
- head-r3: others wrote 47.7 MB/s to its disk over the round, bound 10 (rpool/hotbed/bases/cladefs/g22 23.3, rpool/hotbed/ws/nfhvbft4 19.7, rpool/hotbed/ws/aihhiixv 1.9); others wrote 73.3 MB/s to its disk in the minute 60 s in, bound 50
- head-r4: others wrote 52.6 MB/s to its disk over the round, bound 10 (rpool/hotbed/bases/cladefs/g22 30.0, rpool/hotbed/ws/nfhvbft4 17.3, zd0 2.3); others wrote 88.6 MB/s to its disk in the minute 180 s in, bound 50
- head-r5: others wrote 15.6 MB/s to its disk over the round, bound 10 (rpool/hotbed/ws/nfhvbft4 8.6, zd0 3.1, rpool/hotbed/ws/jums355h 0.6)

| Case | Workload | Unit | base | head | head vs base | ext4-guest-kernel head vs base | Verdict |
|---|---|---|---|---|---|---|---|
| fxfs-guest-dax | chmod | ops/s | 2.769e+05 (2.73e+05–2.862e+05) [contaminated rounds left out: 3] | 2.823e+05 (2.701e+05–2.945e+05) [contaminated rounds left out: 4] | 0.98× | 0.99× | within drift |
| fxfs-guest-dax | create | ops/s | 5.599e+04 (5.495e+04–5.694e+04) [contaminated rounds left out: 3] | 5.747e+04 (5.739e+04–5.755e+04) [contaminated rounds left out: 4] | 0.97× | 0.98× | real |
| fxfs-guest-dax | create-fsync | ops/s | 4,550 (3,671–4,708) [contaminated rounds left out: 3] | 4,358 (3,953–4,762) [contaminated rounds left out: 4] | 1.04× | 0.81× | within drift |
| fxfs-guest-dax | freeze-loop | ops/s | 23.3 (9.293–38.08) [contaminated rounds left out: 3] | 42.24 (41.69–42.8) [contaminated rounds left out: 4] | 0.55× | 0.90× | real |
| fxfs-guest-dax | fsync-append | IOPS | 5,225 (4,719–5,721) [contaminated rounds left out: 3] | 4,340 (4,276–4,404) [contaminated rounds left out: 4] | 1.20× | 0.61× | within drift |
| fxfs-guest-dax | link | ops/s | 7.454e+04 (7.191e+04–7.56e+04) [contaminated rounds left out: 3] | 7.629e+04 (7.622e+04–7.636e+04) [contaminated rounds left out: 4] | 0.98× | 0.98× | real |
| fxfs-guest-dax | lookup-missing | ops/s | 3.182e+05 (2.948e+05–3.234e+05) [contaminated rounds left out: 3] | 3.408e+05 (3.4e+05–3.416e+05) [contaminated rounds left out: 4] | 0.93× | 0.97× | real |
| fxfs-guest-dax | mmap-rand-read | IOPS | 2.277e+06 (2.26e+06–2.371e+06) [contaminated rounds left out: 3] | 2.252e+06 (2.211e+06–2.292e+06) [contaminated rounds left out: 4] | 1.01× | 0.96× | within drift |
| fxfs-guest-dax | mmap-rand-write | IOPS | 4.554e+04 (2.82e+04–6.4e+04) [contaminated rounds left out: 3] | 3.449e+04 (2.257e+04–4.641e+04) [contaminated rounds left out: 4] | 1.32× | 1.30× | within drift |
| fxfs-guest-dax | mmap-seq-read | MiB/s | 2.327e+04 (2.327e+04–2.56e+04) [contaminated rounds left out: 3] | 2.444e+04 (2.327e+04–2.56e+04) [contaminated rounds left out: 4] | 0.95× | 0.91× | within drift |
| fxfs-guest-dax | rand-read-cold | IOPS | 1.183e+04 (1.031e+04–1.549e+04) [contaminated rounds left out: 3] | 1.304e+04 (1.254e+04–1.354e+04) [contaminated rounds left out: 4] | 0.91× | 0.90× | within drift |
| fxfs-guest-dax | rand-read-warm | IOPS | 7.113e+05 (6.99e+05–7.132e+05) [contaminated rounds left out: 3] | 7.394e+05 (7.305e+05–7.483e+05) [contaminated rounds left out: 4] | 0.96× | 0.97× | real |
| fxfs-guest-dax | rand-write | IOPS | 1.52e+04 (1.377e+04–1.602e+04) [contaminated rounds left out: 3] | 1.706e+04 (1.533e+04–1.879e+04) [contaminated rounds left out: 4] | 0.89× | 0.44× | within drift |
| fxfs-guest-dax | read-small-cold | ops/s | 4.077e+04 (3.302e+04–4.4e+04) [contaminated rounds left out: 3] | 4.062e+04 (3.996e+04–4.129e+04) [contaminated rounds left out: 4] | 1.00× | 0.93× | within drift |
| fxfs-guest-dax | readdir | ops/s | 3.355e+06 (2.992e+06–3.56e+06) [contaminated rounds left out: 3] | 2.52e+06 (1.815e+06–3.224e+06) [contaminated rounds left out: 4] | 1.33× | 0.98× | within drift |
| fxfs-guest-dax | rename | ops/s | 6.047e+04 (5.739e+04–6.15e+04) [contaminated rounds left out: 3] | 6.137e+04 (6.071e+04–6.202e+04) [contaminated rounds left out: 4] | 0.99× | 1.00× | within drift |
| fxfs-guest-dax | rename-across | ops/s | 5.272e+04 (5.22e+04–5.444e+04) [contaminated rounds left out: 3] | 5.125e+04 (4.755e+04–5.495e+04) [contaminated rounds left out: 4] | 1.03× | 0.98× | within drift |
| fxfs-guest-dax | seq-read-cold | MiB/s | 1,575 (1,533–1,590) [contaminated rounds left out: 3] | 1,562 (1,515–1,610) [contaminated rounds left out: 4] | 1.01× | 0.75× | within drift |
| fxfs-guest-dax | seq-read-warm | MiB/s | 2.84e+04 (2.69e+04–2.884e+04) [contaminated rounds left out: 3] | 2.995e+04 (2.938e+04–3.052e+04) [contaminated rounds left out: 4] | 0.95× | 0.95× | real |
| fxfs-guest-dax | seq-write | MiB/s | 207.4 (117.7–325.3) [contaminated rounds left out: 3] | 215.5 (193.6–237.3) [contaminated rounds left out: 4] | 0.96× | 1.36× | within drift |
| fxfs-guest-dax | setxattr | ops/s | 1.462e+05 (1.39e+05–1.469e+05) [contaminated rounds left out: 3] | 1.471e+05 (1.445e+05–1.497e+05) [contaminated rounds left out: 4] | 0.99× | 0.99× | within drift |
| fxfs-guest-dax | sqlite-mmap-scan | MiB/s | 4,539 (4,165–4,792) [contaminated rounds left out: 3] | 4,600 (4,569–4,630) [contaminated rounds left out: 4] | 0.99× | 0.98× | within drift |
| fxfs-guest-dax | sqlite-mmap-scan-warm | MiB/s | 4,870 (4,406–4,983) [contaminated rounds left out: 3] | 4,928 (4,785–5,070) [contaminated rounds left out: 4] | 0.99× | 0.98× | within drift |
| fxfs-guest-dax | sqlite-pread-scan | MiB/s | 264.1 (215.2–292.9) [contaminated rounds left out: 3] | 225.1 (222.6–227.7) [contaminated rounds left out: 4] | 1.17× | 0.91× | within drift |
| fxfs-guest-dax | sqlite-pread-scan-warm | MiB/s | 2,739 (2,710–2,805) [contaminated rounds left out: 3] | 2,786 (2,756–2,816) [contaminated rounds left out: 4] | 0.98× | 0.98× | within drift |
| fxfs-guest-dax | stat-cold | ops/s | 1.429e+05 (1.317e+05–1.458e+05) [contaminated rounds left out: 3] | 1.359e+05 (1.286e+05–1.432e+05) [contaminated rounds left out: 4] | 1.05× | 0.94× | within drift |
| fxfs-guest-dax | stat-warm | ops/s | 7.204e+05 (6.288e+05–7.926e+05) [contaminated rounds left out: 3] | 7.589e+05 (7.31e+05–7.867e+05) [contaminated rounds left out: 4] | 0.95× | 0.99× | within drift |
| fxfs-guest-dax | sync-loop | ops/s | 73.11 (17.25–122.8) [contaminated rounds left out: 3] | 131.3 (129.7–133) [contaminated rounds left out: 4] | 0.56× | 0.85× | real |
| fxfs-guest-dax | syncfs-loop | ops/s | 73.31 (28.97–130.1) [contaminated rounds left out: 3] | 137.6 (136.8–138.5) [contaminated rounds left out: 4] | 0.53× | 0.90× | real |
| fxfs-guest-dax | unlink | ops/s | 2.731e+05 (1.489e+05–2.8e+05) [contaminated rounds left out: 3] | 3.067e+05 (2.992e+05–3.143e+05) [contaminated rounds left out: 4] | 0.89× | 0.98× | real |
| fxfs-guest-dax-fork-cold | base-copy-usr | s | 1.238 (1.195–1.281) [contaminated rounds left out: 3] | 1.381 (1.381–1.381) [contaminated rounds left out: 4] | 1.12× | - | no control |
| fxfs-guest-dax-fork-cold | base-pgbench-init | s | 4.853 (4.435–5.271) [contaminated rounds left out: 3] | 4.224 (4.224–4.224) [contaminated rounds left out: 4] | 0.87× | - | no control |
| fxfs-guest-dax-fork-cold | fork-mount | s | 0.1875 (0.178–0.312) [contaminated rounds left out: 3] | 0.247 (0.216–0.278) [contaminated rounds left out: 4] | 1.32× | - | no control |
| fxfs-guest-dax-fork-cold | fork-pg-start | s | 0.1265 (0.125–0.127) [contaminated rounds left out: 3] | 0.127 (0.127–0.127) [contaminated rounds left out: 4] | 1.00× | - | no control |
| fxfs-guest-dax-fork-cold | fork-pgbench-select | tps | 4.057e+04 (3.947e+04–4.165e+04) [contaminated rounds left out: 3] | 3.878e+04 (3.857e+04–3.899e+04) [contaminated rounds left out: 4] | 1.05× | - | no control |
| fxfs-guest-dax-fork-cold | fork-read-usr | s | 1.567 (1.426–2.083) [contaminated rounds left out: 3] | 2.094 (1.941–2.247) [contaminated rounds left out: 4] | 1.34× | - | no control |
| fxfs-guest-dax-fork-cold | fork-reread-usr | s | 0.0755 (0.074–0.077) [contaminated rounds left out: 3] | 0.076 (0.075–0.077) [contaminated rounds left out: 4] | 1.01× | - | no control |
| fxfs-guest-dax-fork-warm | base-copy-usr | s | 1.198 (1.147–1.249) [contaminated rounds left out: 3] | 1.338 (1.338–1.338) [contaminated rounds left out: 4] | 1.12× | - | no control |
| fxfs-guest-dax-fork-warm | base-pgbench-init | s | 4.709 (4.616–4.802) [contaminated rounds left out: 3] | 7.259 (7.259–7.259) [contaminated rounds left out: 4] | 1.54× | - | no control |
| fxfs-guest-dax-fork-warm | fork-mount | s | 0.127 (0.055–0.236) [contaminated rounds left out: 3] | 0.092 (0.042–0.142) [contaminated rounds left out: 4] | 0.72× | - | no control |
| fxfs-guest-dax-fork-warm | fork-pg-start | s | 0.1265 (0.126–0.128) [contaminated rounds left out: 3] | 0.127 (0.125–0.129) [contaminated rounds left out: 4] | 1.00× | - | no control |
| fxfs-guest-dax-fork-warm | fork-pgbench-select | tps | 4.125e+04 (3.788e+04–4.32e+04) [contaminated rounds left out: 3] | 4.072e+04 (4.016e+04–4.129e+04) [contaminated rounds left out: 4] | 1.01× | - | no control |
| fxfs-guest-dax-fork-warm | fork-read-usr | s | 1.341 (0.404–2.534) [contaminated rounds left out: 3] | 1.327 (0.416–2.237) [contaminated rounds left out: 4] | 0.99× | - | no control |
| fxfs-guest-dax-fork-warm | fork-reread-usr | s | 0.075 (0.074–0.101) [contaminated rounds left out: 3] | 0.075 (0.074–0.076) [contaminated rounds left out: 4] | 1.00× | - | no control |
