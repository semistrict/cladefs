# Data-analysis sandboxes, fork to answers (ubu1, 2026-11-19)

TASK-665: fxfs-guest-python-sandbox (`scripts/python-sandbox-host.sh`), one run at 19fde1f7c, the
measurement under `fxjob --bench` (cores 4-7), release guest kernel. The case's log is
`python-sandbox.log`, each daemon's counters at the end are `host-*-stats.txt`, and the host is in
`host.txt`. The log's `fetched_mib` column read 0 throughout: page-ins read their parts through the
part cache. The case reports `bucket_mib` in its place now.

**The setup.** The base is Python 3.14 (python:3.14.10-slim-trixie) with uv 0.13.9 and a project
whose dependencies `uv sync --frozen` installed: marimo 0.25.2, pandas 3.1.1, pyarrow 26.0.1 and 30
more. It also holds New York's green-taxi trips of 2025 (12 Parquet files, 14 MB, 591,375 trips).
The base has no bytecode: the build deletes every `.pyc`, and the host checks before the import.
There are three extent daemons, each with 2 GiB, a part cache, and the other two as peers. They
share one RustFS bucket, with 25 ms added to each request. Host A imported the base (6.7 s), then
versioned and synced it (0.5 s).

Each sandbox is a fork of that version on its host. Its VM has 2 vCPUs and 2 GiB, with its root
through a 1 GiB DAX window. It starts `marimo run` on the notebook and, once `/health` answers, runs
`marimo export html`. The notebook reads the trips with pandas and answers eight questions, which
match what the notebook gave outside any VM in all six sandboxes. Every sandbox compiled 1,640
`.pyc` files into its own fork.

Times are milliseconds on the host's clock from just before the fork, except boot, which is the
guest's own clock from its kernel's start to its agent. Bucket GETs and MiB are given per host as
A/B/C.

| Sandbox | State of its host | Fork | VM started | Boot | marimo serving | Answers | Bucket GETs | Bucket MiB | Parts from peers |
|---|---|---:|---:|---:|---:|---:|---|---|---:|
| a1 | warm (it imported the base) | 333 | 352 | 255 | 3,354 | 7,158 | 7/0/0 | 0/0/0 | 0 |
| b1 | cold, peers cold | 752 | 763 | 995 | 7,190 | **13,422** | 0/176/170 | 0/51/52 | 288 (A 134, C 154) |
| c1 | cold, peers warm | 734 | 1,149 | 459 | 5,389 | **10,246** | 0/0/14 | 0/0/0 | 271 (A 131, B 140) |
| a2 | warm | 305 | 316 | 225 | 2,870 | 6,024 | 6/0/0 | 0/0/0 | 0 |
| b2 | warm | 299 | 311 | 237 | 2,885 | 6,050 | 0/6/0 | 0/0/0 | 0 |
| c2 | warm | 293 | 304 | 238 | 2,845 | **5,984** | 0/0/6 | 0/0/0 | 0 |

What it says:

- **On a warm host, a sandbox answers in 6.0 s from its fork.** The fork takes 0.3 s and the boot
  0.24 s. Python then takes the rest: marimo serves at 2.9 s, and the notebook needs another 3.1 s.
  That time includes compiling 1,640 modules' bytecode, which every sandbox repeats because the
  base holds none. No data came from the bucket: each warm sandbox made 6 GETs of under
  0.1 MiB between the hosts, the fork's own.
- **A cold host whose peers are cold too takes 13.4 s, 7.4 s more.** 103 MiB came from the
  bucket, as compressed parts (the image's tar is 657 MB), at 25 ms a request: 176 GETs by B
  itself, and 170 by C, for the parts C owns and served to B. The fork alone took 0.75 s
  rather than 0.3 s.
- **A cold host whose peers are warm takes 10.2 s.** C took 271 parts from A and B and made 14
  bucket GETs. The 4.2 s over warm is reading parts from peers into a cold memory, with no wait on
  the bucket.

All three daemons ran on one machine and reached each other over loopback, so a peer's part cost no
network time. Only the bucket is at a distance. The bench partition gave the run 4 cores, which the
daemons, RustFS and the VM shared.
