## UNCHECKED: whether others wrote beside these rounds is not known; their timings are kept

- queues-r1: /work/cladefs/target/host-runs/run-abstream2-20261128T165531Z-3b1b9832/results/queues-r1: on cladefs (rootfs), no block device found
- queues-r2: /work/cladefs/target/host-runs/run-abstream2-20261128T165531Z-3b1b9832/results/queues-r2: on cladefs (rootfs), no block device found
- queues-r3: /work/cladefs/target/host-runs/run-abstream2-20261128T165531Z-3b1b9832/results/queues-r3: on cladefs (rootfs), no block device found
- stream-r1: /work/cladefs/target/host-runs/run-abstream2-20261128T165531Z-3b1b9832/results/stream-r1: on cladefs (rootfs), no block device found
- stream-r2: /work/cladefs/target/host-runs/run-abstream2-20261128T165531Z-3b1b9832/results/stream-r2: on cladefs (rootfs), no block device found
- stream-r3: /work/cladefs/target/host-runs/run-abstream2-20261128T165531Z-3b1b9832/results/stream-r3: on cladefs (rootfs), no block device found

| Case | Workload | Unit | queues | stream | stream vs queues | ext4-guest-kernel stream vs queues | Verdict |
|---|---|---|---|---|---|---|---|
| ext4-dax-pmem | build-total | s | 8.028 (4.085–16.78) | 8.715 (3.77–11.37) | 1.09× | 1.18× | within drift |
| ext4-dax-pmem | chmod | ops/s | 4.105e+05 (3.169e+05–4.358e+05) | 3.925e+05 (3.157e+05–4.31e+05) | 1.05× | 1.01× | within drift |
| ext4-dax-pmem | create | ops/s | 1.481e+04 (1.157e+04–1.833e+04) | 1.395e+04 (8,771–1.865e+04) | 1.06× | 0.91× | within drift |
| ext4-dax-pmem | create-fsync | ops/s | 100.7 (75.72–728.7) | 103.2 (71.91–957.5) | 0.98× | 1.00× | within drift |
| ext4-dax-pmem | create-parallel | ops/s | 9.018e+04 (1.911e+04–1.226e+05) | 8.193e+04 (4.807e+04–1.194e+05) | 1.10× | 0.87× | within drift |
| ext4-dax-pmem | create-shared-dir | ops/s | 8.17e+04 (6.086e+04–8.417e+04) | 7.66e+04 (6.111e+04–8.411e+04) | 1.07× | 1.01× | within drift |
| ext4-dax-pmem | find-cold | ops/s | 2.021e+05 (1.242e+05–2.342e+05) | 1.748e+05 (1.14e+05–2.348e+05) | 1.16× | 0.84× | within drift |
| ext4-dax-pmem | fsync-append | IOPS | 86.06 (77.91–189.2) | 89.45 (77.56–1,123) | 0.96× | 1.10× | within drift |
| ext4-dax-pmem | large-dir-create | ops/s | 3.156e+04 (1.42e+04–4.103e+04) | 3.781e+04 (2.14e+04–5.793e+04) | 0.83× | 0.68× | within drift |
| ext4-dax-pmem | large-dir-stat-cold | ops/s | 1.565e+05 (8.61e+04–2.063e+05) | 2.028e+05 (4.488e+04–2.071e+05) | 0.77× | 2.28× | within drift |
| ext4-dax-pmem | large-dir-unlink | ops/s | 1.259e+05 (1.179e+05–1.3e+05) | 1.188e+05 (9.728e+04–1.326e+05) | 1.06× | 1.12× | within drift |
| ext4-dax-pmem | link | ops/s | 9.737e+04 (9.231e+04–1.039e+05) | 9.801e+04 (9.282e+04–1.007e+05) | 0.99× | 0.98× | within drift |
| ext4-dax-pmem | lookup-missing | ops/s | 2.251e+05 (1.236e+05–3.432e+05) | 2.289e+05 (1.47e+05–3.309e+05) | 0.98× | 0.99× | within drift |
| ext4-dax-pmem | make | s | 7.992 (4.063–16.75) | 8.685 (3.737–11.16) | 1.09× | 1.18× | within drift |
| ext4-dax-pmem | mkdir-tree | ops/s | 1.817e+04 (8,057–2.056e+04) | 1.691e+04 (1.044e+04–2.019e+04) | 1.07× | 1.05× | within drift |
| ext4-dax-pmem | mmap-rand-read | IOPS | 3.219e+04 (2.582e+04–1.135e+05) | 3.244e+04 (3.177e+04–1.531e+06) | 0.99× | 1.01× | within drift |
| ext4-dax-pmem | mmap-rand-write | IOPS | 2,187 (1,901–2,535) | 2,110 (1,878–1.027e+04) | 1.04× | 1.02× | within drift |
| ext4-dax-pmem | mmap-seq-read | MiB/s | 6,650 (4,571–7,314) | 7,117 (6,400–7,758) | 0.93× | 1.00× | within drift |
| ext4-dax-pmem | rand-read-cold | IOPS | 4.508e+04 (4.3e+04–4.668e+04) | 4.655e+04 (4.3e+04–4.695e+04) | 0.97× | 1.18× | within drift |
| ext4-dax-pmem | rand-read-warm | IOPS | 8.755e+05 (1.367e+05–8.859e+05) | 8.843e+05 (8.758e+05–8.871e+05) | 0.99× | 0.99× | within drift |
| ext4-dax-pmem | rand-write | IOPS | 2,612 (1,520–4,041) | 3,503 (2,884–1.013e+04) | 0.75× | 1.06× | within drift |
| ext4-dax-pmem | read-small-cold | ops/s | 2.3e+04 (2.141e+04–2.435e+04) | 2.338e+04 (2.242e+04–2.445e+04) | 0.98× | 1.11× | within drift |
| ext4-dax-pmem | readdir | ops/s | 3.58e+06 (3.236e+06–3.756e+06) | 3.484e+06 (3.25e+06–3.626e+06) | 1.03× | 1.05× | within drift |
| ext4-dax-pmem | rename | ops/s | 7.488e+04 (6.655e+04–8.138e+04) | 7.563e+04 (4.852e+04–8.349e+04) | 0.99× | 0.97× | within drift |
| ext4-dax-pmem | rename-across | ops/s | 6.64e+04 (5.794e+04–6.966e+04) | 6.613e+04 (5.719e+04–6.924e+04) | 1.00× | 0.98× | within drift |
| ext4-dax-pmem | rm-tree | ops/s | 6.811e+04 (5.476e+04–6.843e+04) | 6.712e+04 (5.65e+04–6.746e+04) | 1.01× | 0.97× | within drift |
| ext4-dax-pmem | seq-read-cold | MiB/s | 146.2 (135.2–154.1) | 150.7 (140.7–154) | 0.97× | 0.58× | within drift |
| ext4-dax-pmem | seq-read-warm | MiB/s | 3.521e+04 (3.205e+04–3.584e+04) | 3.495e+04 (2.301e+04–3.566e+04) | 1.01× | 0.89× | within drift |
| ext4-dax-pmem | seq-write | MiB/s | 30.41 (22.18–36.09) | 30.04 (21.55–49.31) | 1.01× | 0.94× | within drift |
| ext4-dax-pmem | setxattr | ops/s | 3.606e+05 (2.984e+05–3.634e+05) | 3.644e+05 (2.961e+05–3.701e+05) | 0.99× | 1.00× | within drift |
| ext4-dax-pmem | sqlite-mmap-scan | MiB/s | 2,908 (2,624–3,200) | 2,968 (2,666–3,180) | 0.98× | 0.90× | within drift |
| ext4-dax-pmem | sqlite-mmap-scan-warm | MiB/s | 4,817 (4,483–4,941) | 4,898 (4,787–4,930) | 0.98× | 1.03× | within drift |
| ext4-dax-pmem | sqlite-pread-scan | MiB/s | 117.4 (105.2–120.7) | 118.5 (79.21–121.5) | 0.99× | 0.83× | within drift |
| ext4-dax-pmem | sqlite-pread-scan-warm | MiB/s | 3,346 (3,186–3,434) | 3,254 (768.7–3,376) | 1.03× | 0.96× | within drift |
| ext4-dax-pmem | stat-cold | ops/s | 1.193e+05 (4,257–2.236e+05) | 1.135e+05 (8.409e+04–2.281e+05) | 1.05× | 0.75× | within drift |
| ext4-dax-pmem | stat-parallel | ops/s | 7.66e+05 (1.674e+04–9.553e+05) | 9.024e+05 (1.96e+05–1.01e+06) | 0.85× | 1.06× | within drift |
| ext4-dax-pmem | stat-warm | ops/s | 6.966e+05 (4.399e+05–9.361e+05) | 8.823e+05 (8.411e+05–9.519e+05) | 0.79× | 0.83× | within drift |
| ext4-dax-pmem | unlink | ops/s | 2.356e+05 (2.058e+05–2.433e+05) | 2.187e+05 (2.032e+05–2.43e+05) | 1.08× | 0.96× | within drift |
| ext4-dax-pmem | unlink-parallel | ops/s | 2.419e+05 (1.976e+05–2.511e+05) | 2.463e+05 (2.226e+05–2.565e+05) | 0.98× | 1.05× | within drift |
| ext4-dax-pmem | unlink-shared-dir | ops/s | 2.209e+05 (2.039e+05–2.298e+05) | 2.299e+05 (2.065e+05–2.325e+05) | 0.96× | 1.01× | within drift |
| ext4-dax-pmem | untar | s | 0.029 (0.022–0.187) | 0.03 (0.025–0.216) | 1.03× | 0.85× | within drift |
| fxfs-guest | build-total | s | 3.508 (3.111–8.578) | 3.141 (3.06–3.282) | 0.90× | 1.18× | within drift |
| fxfs-guest | chmod | ops/s | 3.166e+05 (2.15e+05–3.226e+05) | 3.204e+05 (2.107e+05–3.361e+05) | 0.99× | 1.01× | within drift |
| fxfs-guest | create | ops/s | 5.037e+04 (2.775e+04–6.459e+04) | 6.14e+04 (3.545e+04–6.673e+04) | 0.82× | 0.91× | within drift |
| fxfs-guest | create-fsync | ops/s | 2,939 (2,550–3,027) | 2,042 (1,898–2,196) | 1.44× | 1.00× | real |
| fxfs-guest | create-parallel | ops/s | 8.907e+04 (3.786e+04–1.147e+05) | 1.001e+05 (7.311e+04–1.187e+05) | 0.89× | 0.87× | within drift |
| fxfs-guest | create-shared-dir | ops/s | 7.264e+04 (6.087e+04–8.449e+04) | 7.81e+04 (6.022e+04–8.77e+04) | 0.93× | 1.01× | within drift |
| fxfs-guest | find-cold | ops/s | 1.268e+05 (1.108e+05–1.457e+05) | 1.241e+05 (1.141e+05–1.416e+05) | 1.02× | 0.84× | within drift |
| fxfs-guest | fsync-append | IOPS | 2,662 (2,349–2,856) | 1,907 (1,473–2,048) | 1.40× | 1.10× | real |
| fxfs-guest | large-dir-create | ops/s | 6.828e+04 (5.255e+04–7.33e+04) | 7.045e+04 (1.776e+04–7.492e+04) | 0.97× | 0.68× | within drift |
| fxfs-guest | large-dir-stat-cold | ops/s | 1.55e+05 (1.525e+05–1.589e+05) | 1.533e+05 (1.338e+05–1.572e+05) | 1.01× | 2.28× | within drift |
| fxfs-guest | large-dir-unlink | ops/s | 1.376e+05 (9.111e+04–1.894e+05) | 1.763e+05 (3.006e+04–1.936e+05) | 0.78× | 1.12× | within drift |
| fxfs-guest | link | ops/s | 7.826e+04 (5.32e+04–8.184e+04) | 8.219e+04 (5.726e+04–8.452e+04) | 0.95× | 0.98× | within drift |
| fxfs-guest | lookup-missing | ops/s | 3.101e+05 (2.009e+05–3.81e+05) | 3.797e+05 (1.714e+05–3.952e+05) | 0.82× | 0.99× | within drift |
| fxfs-guest | make | s | 3.424 (3.098–8.556) | 3.129 (3.046–3.267) | 0.91× | 1.18× | within drift |
| fxfs-guest | mkdir-tree | ops/s | 6.331e+04 (4.966e+04–6.403e+04) | 6.393e+04 (6.026e+04–6.507e+04) | 0.99× | 1.05× | within drift |
| fxfs-guest | mmap-rand-read | IOPS | 2.139e+06 (2.085e+06–2.212e+06) | 2.059e+06 (1.947e+06–2.204e+06) | 1.04× | 1.01× | within drift |
| fxfs-guest | mmap-rand-write | IOPS | 9,312 (9,062–9,570) | 1.09e+04 (1.048e+04–1.138e+04) | 0.85× | 1.02× | real |
| fxfs-guest | mmap-seq-read | MiB/s | 1.829e+04 (1.506e+04–1.829e+04) | 1.829e+04 (1.707e+04–1.829e+04) | 1.00× | 1.00× | within drift |
| fxfs-guest | rand-read-cold | IOPS | 3,564 (2,661–4,648) | 2,794 (2,205–3,307) | 1.28× | 1.18× | within drift |
| fxfs-guest | rand-read-warm | IOPS | 1.124e+06 (1.111e+06–1.13e+06) | 1.126e+06 (1.101e+06–1.133e+06) | 1.00× | 0.99× | within drift |
| fxfs-guest | rand-write | IOPS | 3.125e+04 (2.69e+04–4.763e+04) | 4.241e+04 (3.698e+04–4.589e+04) | 0.74× | 1.06× | within drift |
| fxfs-guest | read-small-cold | ops/s | 8,712 (1,866–9,293) | 4,964 (3,910–5,592) | 1.75× | 1.11× | within drift |
| fxfs-guest | readdir | ops/s | 1.745e+06 (4.743e+04–1.914e+06) | 1.731e+06 (1.009e+06–1.889e+06) | 1.01× | 1.05× | within drift |
| fxfs-guest | rename | ops/s | 6.414e+04 (5.457e+04–6.682e+04) | 6.51e+04 (5.808e+04–6.881e+04) | 0.99× | 0.97× | within drift |
| fxfs-guest | rename-across | ops/s | 5.881e+04 (5.555e+04–6.174e+04) | 6.028e+04 (5.214e+04–6.158e+04) | 0.98× | 0.98× | within drift |
| fxfs-guest | rm-tree | ops/s | 6.07e+04 (5.465e+04–6.928e+04) | 6.708e+04 (5.82e+04–6.905e+04) | 0.90× | 0.97× | within drift |
| fxfs-guest | seq-read-cold | MiB/s | 13.03 (8.54–21.79) | 18.3 (10.52–18.92) | 0.71× | 0.58× | within drift |
| fxfs-guest | seq-read-warm | MiB/s | 2.352e+04 (1.954e+04–2.7e+04) | 2.568e+04 (2.237e+04–2.717e+04) | 0.92× | 0.89× | within drift |
| fxfs-guest | seq-write | MiB/s | 13.51 (11.77–15.59) | 31.86 (20.19–42.24) | 0.42× | 0.94× | real |
| fxfs-guest | setxattr | ops/s | 1.487e+05 (1.272e+05–1.528e+05) | 1.457e+05 (1.427e+05–1.604e+05) | 1.02× | 1.00× | within drift |
| fxfs-guest | sqlite-mmap-scan | MiB/s | 406.1 (18.41–803.9) | 448 (52.21–533.3) | 0.91× | 0.90× | within drift |
| fxfs-guest | sqlite-mmap-scan-warm | MiB/s | 4,151 (1,703–4,538) | 4,199 (2,385–4,584) | 0.99× | 1.03× | within drift |
| fxfs-guest | sqlite-pread-scan | MiB/s | 27.9 (11.25–30.95) | 25.42 (9.82–29.25) | 1.10× | 0.83× | within drift |
| fxfs-guest | sqlite-pread-scan-warm | MiB/s | 1,432 (857.4–2,621) | 1,071 (995.2–1,909) | 1.34× | 0.96× | within drift |
| fxfs-guest | stat-cold | ops/s | 9.666e+04 (8.358e+04–1.63e+05) | 1.542e+05 (8.14e+04–1.733e+05) | 0.63× | 0.75× | within drift |
| fxfs-guest | stat-parallel | ops/s | 4.107e+05 (1.496e+05–5.444e+05) | 5.805e+05 (4.072e+05–7.256e+05) | 0.71× | 1.06× | within drift |
| fxfs-guest | stat-warm | ops/s | 7.867e+05 (5.685e+05–8.767e+05) | 8.483e+05 (6.15e+05–8.563e+05) | 0.93× | 0.83× | within drift |
| fxfs-guest | unlink | ops/s | 1.407e+05 (5.671e+04–1.938e+05) | 1.148e+05 (2.788e+04–2.166e+05) | 1.23× | 0.96× | within drift |
| fxfs-guest | unlink-parallel | ops/s | 1.557e+05 (6.716e+04–1.854e+05) | 1.73e+05 (1.698e+05–1.789e+05) | 0.90× | 1.05× | within drift |
| fxfs-guest | unlink-shared-dir | ops/s | 1.557e+05 (1.272e+05–1.682e+05) | 1.647e+05 (1.446e+05–1.739e+05) | 0.95× | 1.01× | within drift |
| fxfs-guest | untar | s | 0.018 (0.013–0.149) | 0.014 (0.012–0.016) | 0.78× | 0.85× | within drift |
| fxfs-guest-dax | build-total | s | 5.303 (3.094–8.74) | 4.844 (3.427–10.72) | 0.91× | 1.18× | within drift |
| fxfs-guest-dax | chmod | ops/s | 2.438e+05 (1.344e+05–3.314e+05) | 2.328e+05 (1.339e+05–3.28e+05) | 1.05× | 1.01× | within drift |
| fxfs-guest-dax | create | ops/s | 5.477e+04 (1.883e+04–6.024e+04) | 3.127e+04 (8,090–6.082e+04) | 1.75× | 0.91× | within drift |
| fxfs-guest-dax | create-fsync | ops/s | 2,827 (2,630–2,973) | 2,088 (2,012–2,355) | 1.35× | 1.00× | real |
| fxfs-guest-dax | create-parallel | ops/s | 9.192e+04 (2.341e+04–1.328e+05) | 7.255e+04 (2.745e+04–1.158e+05) | 1.27× | 0.87× | within drift |
| fxfs-guest-dax | create-shared-dir | ops/s | 6.991e+04 (5.97e+04–8.497e+04) | 6.449e+04 (5.076e+04–8.299e+04) | 1.08× | 1.01× | within drift |
| fxfs-guest-dax | find-cold | ops/s | 1.213e+05 (8.832e+04–1.331e+05) | 1.22e+05 (9.714e+04–1.419e+05) | 0.99× | 0.84× | within drift |
| fxfs-guest-dax | fsync-append | IOPS | 2,867 (2,547–3,080) | 2,163 (1,928–2,346) | 1.33× | 1.10× | real |
| fxfs-guest-dax | large-dir-create | ops/s | 2.802e+04 (1.547e+04–5.089e+04) | 5.036e+04 (2.324e+04–5.84e+04) | 0.56× | 0.68× | within drift |
| fxfs-guest-dax | large-dir-stat-cold | ops/s | 1.251e+05 (9.248e+04–1.501e+05) | 1.118e+05 (3.934e+04–1.45e+05) | 1.12× | 2.28× | within drift |
| fxfs-guest-dax | large-dir-unlink | ops/s | 1.514e+05 (1.227e+05–1.811e+05) | 1.854e+05 (1.185e+05–1.959e+05) | 0.82× | 1.12× | within drift |
| fxfs-guest-dax | link | ops/s | 7.364e+04 (6.386e+04–7.503e+04) | 8.023e+04 (6.555e+04–8.329e+04) | 0.92× | 0.98× | within drift |
| fxfs-guest-dax | lookup-missing | ops/s | 2.265e+05 (1.586e+05–3.535e+05) | 2.923e+05 (1.288e+05–3.522e+05) | 0.77× | 0.99× | within drift |
| fxfs-guest-dax | make | s | 5.272 (3.083–8.713) | 4.827 (3.415–10.69) | 0.92× | 1.18× | within drift |
| fxfs-guest-dax | mkdir-tree | ops/s | 5.922e+04 (2.65e+04–6.576e+04) | 6.17e+04 (5.174e+04–6.518e+04) | 0.96× | 1.05× | within drift |
| fxfs-guest-dax | mmap-rand-read | IOPS | 1.98e+06 (1.923e+06–2.061e+06) | 1.998e+06 (1.952e+06–2.055e+06) | 0.99× | 1.01× | within drift |
| fxfs-guest-dax | mmap-rand-write | IOPS | 3.629e+04 (3.169e+04–3.741e+04) | 4.093e+04 (3.625e+04–4.513e+04) | 0.89× | 1.02× | within drift |
| fxfs-guest-dax | mmap-seq-read | MiB/s | 2.56e+04 (1.347e+04–2.56e+04) | 2.444e+04 (1.113e+04–2.56e+04) | 1.05× | 1.00× | within drift |
| fxfs-guest-dax | rand-read-cold | IOPS | 6,387 (5,258–6,644) | 6,349 (4,452–6,762) | 1.01× | 1.18× | within drift |
| fxfs-guest-dax | rand-read-warm | IOPS | 8.335e+05 (8.201e+05–8.507e+05) | 8.228e+05 (8.114e+05–8.285e+05) | 1.01× | 0.99× | within drift |
| fxfs-guest-dax | rand-write | IOPS | 3,169 (2,981–4,136) | 4,211 (2,274–5,375) | 0.75× | 1.06× | within drift |
| fxfs-guest-dax | read-small-cold | ops/s | 1.939e+04 (1.852e+04–1.973e+04) | 1.935e+04 (7,002–1.984e+04) | 1.00× | 1.11× | within drift |
| fxfs-guest-dax | readdir | ops/s | 2.202e+06 (1.67e+06–2.858e+06) | 1.905e+06 (1.592e+06–2.943e+06) | 1.16× | 1.05× | within drift |
| fxfs-guest-dax | rename | ops/s | 5.803e+04 (5.067e+04–6.355e+04) | 6.08e+04 (5.683e+04–6.693e+04) | 0.95× | 0.97× | within drift |
| fxfs-guest-dax | rename-across | ops/s | 5.307e+04 (4.987e+04–5.89e+04) | 5.506e+04 (4.609e+04–6.153e+04) | 0.96× | 0.98× | within drift |
| fxfs-guest-dax | rm-tree | ops/s | 6.033e+04 (5.945e+04–6.154e+04) | 6.238e+04 (5.778e+04–6.77e+04) | 0.97× | 0.97× | within drift |
| fxfs-guest-dax | seq-read-cold | MiB/s | 133.2 (125.6–141.8) | 131.5 (117.2–136.8) | 1.01× | 0.58× | within drift |
| fxfs-guest-dax | seq-read-warm | MiB/s | 3.077e+04 (2.795e+04–3.171e+04) | 3.026e+04 (2.94e+04–3.08e+04) | 1.02× | 0.89× | within drift |
| fxfs-guest-dax | seq-write | MiB/s | 12.31 (11.76–14.52) | 27.31 (24.58–36.69) | 0.45× | 0.94× | real |
| fxfs-guest-dax | setxattr | ops/s | 1.26e+05 (1.003e+05–1.278e+05) | 1.352e+05 (1.255e+05–1.42e+05) | 0.93× | 1.00× | within drift |
| fxfs-guest-dax | sqlite-mmap-scan | MiB/s | 4,347 (2,493–4,452) | 4,398 (2,672–4,484) | 0.99× | 0.90× | within drift |
| fxfs-guest-dax | sqlite-mmap-scan-warm | MiB/s | 4,932 (4,667–4,979) | 4,953 (4,812–4,981) | 1.00× | 1.03× | within drift |
| fxfs-guest-dax | sqlite-pread-scan | MiB/s | 67.45 (18.98–70.85) | 60.82 (23.13–81.38) | 1.11× | 0.83× | within drift |
| fxfs-guest-dax | sqlite-pread-scan-warm | MiB/s | 3,235 (1,247–3,281) | 3,128 (2,747–3,282) | 1.03× | 0.96× | within drift |
| fxfs-guest-dax | stat-cold | ops/s | 1.417e+05 (8.023e+04–1.558e+05) | 1.241e+05 (5.069e+04–1.603e+05) | 1.14× | 0.75× | within drift |
| fxfs-guest-dax | stat-parallel | ops/s | 3.592e+05 (2.646e+05–7.012e+05) | 4.136e+05 (2.53e+05–5.615e+05) | 0.87× | 1.06× | within drift |
| fxfs-guest-dax | stat-warm | ops/s | 8.049e+05 (4.762e+05–8.5e+05) | 8.087e+05 (6.807e+05–8.731e+05) | 1.00× | 0.83× | within drift |
| fxfs-guest-dax | unlink | ops/s | 7.116e+04 (2.987e+04–2.464e+05) | 1.527e+05 (4.069e+04–2.868e+05) | 0.47× | 0.96× | within drift |
| fxfs-guest-dax | unlink-parallel | ops/s | 1.783e+05 (1.362e+05–1.863e+05) | 1.799e+05 (1.379e+05–1.863e+05) | 0.99× | 1.05× | within drift |
| fxfs-guest-dax | unlink-shared-dir | ops/s | 1.517e+05 (1.187e+05–1.725e+05) | 1.556e+05 (1.263e+05–1.663e+05) | 0.97× | 1.01× | within drift |
| fxfs-guest-dax | untar | s | 0.018 (0.012–0.046) | 0.017 (0.012–0.024) | 0.94× | 0.85× | within drift |
| fxfs-guest-dax-unsafe | build-total | s | 7.494 (3.101–12.99) | 4.149 (3.098–14.35) | 0.55× | 1.18× | within drift |
| fxfs-guest-dax-unsafe | chmod | ops/s | 3.257e+05 (6.272e+04–3.304e+05) | 3.123e+05 (1.304e+05–3.294e+05) | 1.04× | 1.01× | within drift |
| fxfs-guest-dax-unsafe | create | ops/s | 3.023e+04 (2,473–4.116e+04) | 4.818e+04 (3.009e+04–6.28e+04) | 0.63× | 0.91× | within drift |
| fxfs-guest-dax-unsafe | create-fsync | ops/s | 2,688 (2,522–2,831) | 2,081 (1,951–2,178) | 1.29× | 1.00× | real |
| fxfs-guest-dax-unsafe | create-parallel | ops/s | 8.16e+04 (4.85e+04–1.008e+05) | 7.716e+04 (6.538e+04–1.155e+05) | 1.06× | 0.87× | within drift |
| fxfs-guest-dax-unsafe | create-shared-dir | ops/s | 6.798e+04 (5.158e+04–7.92e+04) | 6.905e+04 (4.95e+04–8.527e+04) | 0.98× | 1.01× | within drift |
| fxfs-guest-dax-unsafe | find-cold | ops/s | 9.39e+04 (8.189e+04–1.308e+05) | 1.253e+05 (7.072e+04–1.325e+05) | 0.75× | 0.84× | within drift |
| fxfs-guest-dax-unsafe | fsync-append | IOPS | 2,964 (2,622–3,122) | 2,190 (1,857–2,281) | 1.35× | 1.10× | real |
| fxfs-guest-dax-unsafe | large-dir-create | ops/s | 1.771e+04 (1.609e+04–4.432e+04) | 3.592e+04 (1.847e+04–5.42e+04) | 0.49× | 0.68× | within drift |
| fxfs-guest-dax-unsafe | large-dir-stat-cold | ops/s | 1.236e+05 (6.339e+04–1.49e+05) | 1.193e+05 (1.062e+05–1.459e+05) | 1.04× | 2.28× | within drift |
| fxfs-guest-dax-unsafe | large-dir-unlink | ops/s | 1.807e+05 (4.851e+04–1.875e+05) | 1.766e+05 (1.517e+05–1.962e+05) | 1.02× | 1.12× | within drift |
| fxfs-guest-dax-unsafe | link | ops/s | 6.96e+04 (4.496e+04–8.328e+04) | 7.667e+04 (6.884e+04–8.161e+04) | 0.91× | 0.98× | within drift |
| fxfs-guest-dax-unsafe | lookup-missing | ops/s | 1.629e+05 (1.247e+05–3.779e+05) | 2.27e+05 (2.179e+05–3.633e+05) | 0.72× | 0.99× | within drift |
| fxfs-guest-dax-unsafe | make | s | 7.476 (3.088–12.97) | 4.129 (3.087–14.29) | 0.55× | 1.18× | within drift |
| fxfs-guest-dax-unsafe | mkdir-tree | ops/s | 4.012e+04 (1.962e+04–6.257e+04) | 6.135e+04 (3.414e+04–6.537e+04) | 0.65× | 1.05× | within drift |
| fxfs-guest-dax-unsafe | mmap-rand-read | IOPS | 1.982e+06 (1.821e+06–2.079e+06) | 1.766e+06 (9.269e+05–2.049e+06) | 1.12× | 1.01× | within drift |
| fxfs-guest-dax-unsafe | mmap-rand-write | IOPS | 3.49e+04 (3.31e+04–3.665e+04) | 4.022e+04 (3.357e+04–4.602e+04) | 0.87× | 1.02× | within drift |
| fxfs-guest-dax-unsafe | mmap-seq-read | MiB/s | 1.656e+04 (6,564–2.56e+04) | 2.265e+04 (4,197–2.56e+04) | 0.73× | 1.00× | within drift |
| fxfs-guest-dax-unsafe | rand-read-cold | IOPS | 1.334e+04 (1.049e+04–1.388e+04) | 1.269e+04 (2,140–1.467e+04) | 1.05× | 1.18× | within drift |
| fxfs-guest-dax-unsafe | rand-read-warm | IOPS | 8.268e+05 (8.014e+05–8.394e+05) | 8.222e+05 (8.13e+05–8.371e+05) | 1.01× | 0.99× | within drift |
| fxfs-guest-dax-unsafe | rand-write | IOPS | 3,649 (3,303–4,330) | 3,121 (1,319–3,983) | 1.17× | 1.06× | within drift |
| fxfs-guest-dax-unsafe | read-small-cold | ops/s | 1.906e+04 (1.773e+04–1.929e+04) | 1.871e+04 (1.804e+04–1.943e+04) | 1.02× | 1.11× | within drift |
| fxfs-guest-dax-unsafe | readdir | ops/s | 1.745e+06 (1.007e+06–2.243e+06) | 1.698e+06 (1.205e+06–2.694e+06) | 1.03× | 1.05× | within drift |
| fxfs-guest-dax-unsafe | rename | ops/s | 5.613e+04 (4.474e+04–6.439e+04) | 6.309e+04 (5.42e+04–6.569e+04) | 0.89× | 0.97× | within drift |
| fxfs-guest-dax-unsafe | rename-across | ops/s | 5.291e+04 (2.576e+04–6.018e+04) | 5.87e+04 (4.758e+04–6.13e+04) | 0.90× | 0.98× | within drift |
| fxfs-guest-dax-unsafe | rm-tree | ops/s | 5.658e+04 (5.028e+04–6.451e+04) | 6.037e+04 (5.921e+04–6.604e+04) | 0.94× | 0.97× | within drift |
| fxfs-guest-dax-unsafe | seq-read-cold | MiB/s | 69.42 (57.45–127.9) | 87.94 (63.38–137.5) | 0.79× | 0.58× | within drift |
| fxfs-guest-dax-unsafe | seq-read-warm | MiB/s | 3.043e+04 (2.961e+04–3.093e+04) | 3.077e+04 (2.781e+04–3.126e+04) | 0.99× | 0.89× | within drift |
| fxfs-guest-dax-unsafe | seq-write | MiB/s | 15.99 (12.48–18.92) | 27.87 (22.76–36.4) | 0.57× | 0.94× | real |
| fxfs-guest-dax-unsafe | setxattr | ops/s | 1.387e+05 (6.238e+04–1.467e+05) | 1.331e+05 (1.199e+05–1.455e+05) | 1.04× | 1.00× | within drift |
| fxfs-guest-dax-unsafe | sqlite-mmap-scan | MiB/s | 4,238 (2,999–4,478) | 4,338 (3,788–4,526) | 0.98× | 0.90× | within drift |
| fxfs-guest-dax-unsafe | sqlite-mmap-scan-warm | MiB/s | 4,857 (4,724–4,986) | 4,977 (4,818–5,058) | 0.98× | 1.03× | within drift |
| fxfs-guest-dax-unsafe | sqlite-pread-scan | MiB/s | 61.87 (51.81–80.51) | 71.13 (60.16–85.52) | 0.87× | 0.83× | within drift |
| fxfs-guest-dax-unsafe | sqlite-pread-scan-warm | MiB/s | 3,210 (2,742–3,316) | 3,175 (2,814–3,302) | 1.01× | 0.96× | within drift |
| fxfs-guest-dax-unsafe | stat-cold | ops/s | 8.154e+04 (6.469e+04–1.388e+05) | 1.138e+05 (1.026e+05–1.451e+05) | 0.72× | 0.75× | within drift |
| fxfs-guest-dax-unsafe | stat-parallel | ops/s | 4.228e+05 (1.235e+05–6.654e+05) | 4.075e+05 (2.629e+05–6.649e+05) | 1.04× | 1.06× | within drift |
| fxfs-guest-dax-unsafe | stat-warm | ops/s | 7.615e+05 (4.103e+05–8.753e+05) | 8.204e+05 (6.212e+05–8.555e+05) | 0.93× | 0.83× | within drift |
| fxfs-guest-dax-unsafe | unlink | ops/s | 9.793e+04 (4.216e+04–2.377e+05) | 1.681e+05 (5.625e+04–2.669e+05) | 0.58× | 0.96× | within drift |
| fxfs-guest-dax-unsafe | unlink-parallel | ops/s | 1.479e+05 (7.027e+04–1.787e+05) | 1.737e+05 (1.366e+05–1.911e+05) | 0.85× | 1.05× | within drift |
| fxfs-guest-dax-unsafe | unlink-shared-dir | ops/s | 1.584e+05 (1.18e+05–1.692e+05) | 1.543e+05 (1.07e+05–1.656e+05) | 1.03× | 1.01× | within drift |
| fxfs-guest-dax-unsafe | untar | s | 0.02 (0.013–0.046) | 0.0195 (0.011–0.086) | 0.97× | 0.85× | within drift |
