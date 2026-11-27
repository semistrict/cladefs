# Testing and measurement

```sh
scripts/test.sh          # macOS or Linux
scripts/test-host.sh     # x86-64 Linux with KVM, in a hotbed workspace on a test host
```

The guest tests (`guest/tests`) boot cladefs's guest kernel in cladefs's
Firecracker, on Linux with KVM, and mount its Fxfs from an extent-store device,
whose cell cladefs's extent daemon serves (`scripts/test-e2e.sh` starts one):
`extent-guest` (the driver), `fxfs-guest`, `fxfs-guest-dax`, `fxfs-guest-checks`
and `fxfs-guest-checks-dax` (file workloads, including `mmap`, after which the
host remounts the cell, fscks it, and checks what the guest wrote), the crash
tests, resizing a mounted cell, and pjdfstest. The harness (runner, guest agent
and test cases) came from libkrun's tests, Apache-2.0 (`guest/tests/LICENSE.libkrun`).

```sh
scripts/test-e2e.sh        # Linux (KVM): CLADEFS_GUEST_KERNEL names the kernel
```

Each case is in suites, which the scripts run it by: `scripts/test-e2e.sh --tag kernel`, or cases
by name, `--tag e2e` if none (the runner's `list --help` names the suites). A run's outcomes are
data, a repeat's in `CASE/repeat-K.json` and the run's in `summary.json` (`scripts/guest_results.py`).

`scripts/test-e2e-rustfs.sh` runs them with their cells in an S3 bucket on the host (RustFS, a
process of its release's binary), as production keeps them in object storage. The nightly suite (`--tag nightly`)
runs so every night on GitHub's runners, whose nested KVM boots the guests
(`.github/workflows/nightly.yml`), Firecracker in its jailer under its seccomp filter: built once,
then a matrix of jobs, each a part of the suite (`runner list --tag nightly --shard K/N`).

FUSE mounts (`crates/cladefs/src/fuse`) have their server fuzzed with random and
malformed requests (`fuse/server/fuzz_tests.rs`), and on Linux random file
system calls on a mount compared with the same on tmpfs, as the user and as
root (`crates/cladefs/tests/fuse_differential.rs`, what may differ in its
`ALLOWED`). CI also runs pjdfstest on mounts, as root:

```sh
sudo scripts/pjdfstest-fuse.py target/$(uname -m)-unknown-linux-musl/debug   # Linux: cladefsd's mount
```

What the extent daemon costs is a test's assertion too, its budgets the design's (TASK-330): its
anonymous memory, sampled as its stats are, against the sum of what its design bounds
(`cladefs::cladefsd::process`: its upload memory, a framing for each cell written, its fetches,
its tenants' bytes in flight, its own), which its stats' `process:` line and its metrics say; and what it
writes, by kind, against what it says it wrote. `scripts/test-e2e.sh` fails a run whose daemons
passed either (`scripts/budgets.py`, which says too what each wrote for each guest byte, by kind);
a host's cell held to what its design writes for a guest's large file and its small files, each
byte raw once and framed once, a commit's copies a segment's worth at most
(`bucket::write_budgets`); and the daemon against a degraded bucket, slow, full, out of service,
then answering again, VM processes writing all the while (`cladefsd::tests::soak`, five minutes in
CI, half an hour each night in Hunt; with guests, `fxfs-guest-bucket-soak`, by name). Each has a
past bug put back that it fails (`crates/cladefs/mutants`).

And cladefs runs as LangSmith's sandboxes' CI does, whose runs found bugs ours didn't (TASK-330.1):
two extent daemons and their S3 bucket (RustFS, a process of its release's binary) on one ext4
disk, many VMs at once on fewer CPUs than they have vCPUs, building and exporting Docker images,
running pgbench, working git repositories and writing large and small files, each guest timing a
sandbox's commands all the while. It fails on a guest kernel's report, a VM let go, a framing past
a second, a command past its budget, or a daemon past its own; LangSmith's pin's double writes,
put back, fail it (`crates/cladefs/mutants/framed-in-copies.patch`). The short mode, eight VMs,
minutes, runs on GitHub's runners' nested KVM on every push to main
(`.github/workflows/sandboxes.yml`, from `ci.yml`), and by hand on a branch. The long mode is
LangSmith's controlled load, 16 VMs eight at a time, each doing more: it runs on the test host alone, by
hand, in the qualification before each candidate (below), as no CI runner has its memory (TASK-330.5).
Each run keeps what the host and the run spent every 30 s (`resources.log`):

```sh
scripts/sandboxes.sh [short|long]   # Linux (KVM); CLADEFS_SANDBOX_DISK, ext4, holds it all
```

`fxfs-guest-python-sandbox` times a data-analysis sandbox from its fork to its first answers
(TASK-665). Its base is Python with a uv-installed project (pandas, pyarrow and marimo) and a year
of New York's taxi trips, none of it compiled to bytecode. It runs on three extent daemons with
part caches that are each other's peers, and one RustFS bucket 25 ms away. Each sandbox starts
marimo and runs a notebook whose answers are checked, cold and then warm on each host, timed phase
by phase, with each host's bucket GETs and peer parts. It runs on the test host as a measurement, and its
first numbers are in `docs/measurements/ubu1-python-sandbox-2026-11-19T1842Z-7786f0b5`:

```sh
scripts/python-sandbox-host.sh   # CLADEFS_PYSANDBOX_ROUNDS, default 2
```

## Before a candidate goes to LangSmith

LangSmith runs cladefs at a commit they take, their pin (96c7808b8 as of 2026-11-11, TASK-310), and
bump it now and then. Before a candidate is offered, it is qualified against their pin on the test host,
alone there (`hotbed run --bench`), the two interleaved, A B then B A (TASK-330.4):

```sh
CLADEFS_AB_SUITE=qualify CLADEFS_AB_ROUNDS=2 CLADEFS_AB_A=pin=96c7808b8 \
    CLADEFS_AB_B=candidate=origin/main scripts/bench-ab-host.sh
```

Each round of each side is the qualification (`scripts/qualify.sh`): the sandboxes' long mode,
then the bucket soak (`fxfs-guest-bucket-soak`, 30 minutes: RustFS's bucket, through a proxy of
the case's own, slow, full, out of service and answering again, guests writing all the while),
every daemon held to its budgets (`scripts/budgets.py`), and each part's daemons to a budget of
CPU time for each MiB their guests wrote (`scripts/qualify-budgets.py`'s `CPU_BUDGETS`, TASK-382:
main's soak daemon once spent 1.55 times the pin's, its uploads all sending their objects again
and again to the failing bucket, and nothing said so). One harness, the tree's that runs it,
drives both sides' builds, their extent daemons, Firecrackers and guest kernels
(`scripts/test-e2e.sh`'s `CLADEFS_E2E_HARNESS`), so that a pin from before the qualification's cases
meets the same cases, bucket and faults as the candidate; and what it measures of each daemon is
its kernel's count (disk written, peak memory, CPU), whatever the daemon says of itself.
`docs/measurements/ubu1-qualify-ab-*/ab.md` says every metric, those that moved (their rounds'
ranges apart), and each side's failed checks. A candidate goes to LangSmith when its side passes
every check and nothing moved the wrong way that the change didn't mean to move.

`hotbed run --bench` gives a measurement cores, cache, memory bandwidth and memory of its own; not the
disk, the test host's one NVMe, which others write to all the while. So every round of a measurement runs
under `scripts/contamination.py` (the A/B's rounds, `scripts/bench-host.sh`'s run,
`scripts/bench-rounds.sh`'s), which records in the round's results (`contamination.json`) what
others read from and wrote to the round's disk as it ran, the disk's own count less the round's
cgroup's, and under the test host's scratch zvol the pool's other datasets' writes; and how long the
round's tasks stalled on CPU, memory and I/O. A round others wrote more than 10 MB/s beside,
or 50 MB/s in any one minute, is contaminated (`CLADEFS_CONTAMINATION_WRITE_MB_S`,
`CLADEFS_CONTAMINATION_MINUTE_WRITE_MB_S`): `scripts/bench-ab.py` leaves its timings and rates out
of the comparison, saying so above the table and in each cell, and keeps its counts and sizes
(bytes written for each guest byte, peak memory), which a load doesn't move. A metric with no
clean round on a side is NO RESULT, and fails the comparison. Where others' I/O can't be told
from the round's own (no cgroup v2 cgroup of its own, as in a CI runner's container or on macOS;
a ZFS dataset as the round's disk), the round is unchecked, said as such, never clean
(TASK-375). A round run by hand is recorded the same way, and `scripts/contamination-check.sh`
checks the record in a hotbed workspace, a round alone and one beside a loader:

```sh
scripts/contamination.py run RESULTS -- scripts/qualify.sh RESULTS
CLADEFS_TEST_HOST=<node> scripts/contamination-check.sh check-1   # four minutes
```

`scripts/test-guest-kernel.sh` builds the guest kernel with Fxfs in it and runs
the guest tests on it, on this host (Linux with KVM).

`scripts/test-host.sh` runs them too, in a hotbed workspace on a test host (`CLADEFS_TEST_HOST`
names the node): this checkout synced there, the run a hotbed command, the results copied back and
the run's directory removed (`scripts/lib/host.sh`).

The protocols whose bugs are rare interleavings have TLA+ specs, which TLC
model-checks exhaustively over small bounds (`specs/`): the guest Fxfs's copy-on-write
DAX (`specs/dax-cow`, backlog doc-13, `guest/linux/fs/cladefs/dax.c`),
cells in the bucket with the host's write-ahead log (`specs/bucket`,
backlog doc-7 and doc-14), and Fxfs's super-block claiming only
what its journal flushed (`specs/journal`). Each rule
of a protocol has a mutation, the spec with that rule removed, which TLC must
find the violation of: `specs/dax-cow`'s include a writeback leaving a fault's
PTE writable, which guest/sim found (`guest/sim`).

```sh
make check-tla             # about two hours, each model in minutes, on Java; fetches TLC once
```

## The nightly measurement on the nightly host

Every night at 02:00 a cron entry on the nightly host (a test host) runs
`scripts/nightly-host.sh` from the nightly's own clone (`~/src/cladefs-nightly`), main's own copy of it (TASK-441). If main's code didn't change
since the last night that measured (its newest commit that changed more than `docs/measurements`), it does nothing. Otherwise it measures main's head against the commit that night
measured, as an A/B (`scripts/bench-ab-host.sh`, on the test host itself, `CLADEFS_TEST_HOST=local`): the
head built as work, the base's artifacts kept from the night it was the head (`CLADEFS_AB_KEEP`),
then three rounds of both sides interleaved alone in `hotbed run --bench`, each of the fs bench's cases
an agent sandbox stands on: the guest's Fxfs with DAX (cold and warm reads, fsynced appends,
SQLite's scans, small files' create, stat, readdir and read, create with fsync, sync, syncfs and
freeze loops), a fork's boot cold and warm, and ext4 on virtio-blk, no cladefs in it, as the
control. Beside them, counts that a load doesn't move: the guest's device requests, the extent
daemon's counters, what each daemon wrote by kind for each guest byte and its peak memory
(`scripts/budgets.py`'s figures), and three benchmarks run with each build (`scripts/counts.sh`: a
sandbox's bucket requests by phase, an idle fork's heap, the collector's requests over dead cells).

`scripts/ab-rounds.py` judges each round apart. A round is left out when others wrote beside it
(`contamination.py`) or when its control moved (the median of its timings' ratios past 5%), and
run once more; a metric moved only if it moved the same way past its bound (10%) in every round
kept, a count if it differs at all in every round (a daemon's peak memory, sampled, past 10%)
(`CLADEFS_AB_BOUND`, `CLADEFS_AB_CONTROL_BOUND`,
`CLADEFS_AB_COUNT_BOUND`, and `CLADEFS_AB_BOUNDS` for single metrics). Each metric that moved, and
each case or count benchmark that failed only on the head, becomes a task on the board labelled
`tag.nightly` with both commits, every round's numbers and the command that replays it; a later night
that finds it again adds a reply to the open task. A quiet night files nothing. Each night's results
are committed to main from the clone, the push retried on a main that moved; a push that fails
stays in the clone and is said in `~/.local/state/cladefs/nightly/push-failed.txt`, and the next
night pushes it first.

- Results: `docs/measurements/<host>-nightly-<time>-<head>/nightly.md` for each night, and
  `docs/measurements/nightly-history.md` for all of them, newest first.
- A night that **could not run** (a side failed to build, or no round ran) or that has **no result** (every round was left out: the disk was busy, or the
  host drifted) measured nothing: its row says why, and its `nightly.md` lists each round and why
  it was left out. The next night measures the same commits again, or main's newer head against
  the same base. Three such nights in a row file a task (`[nightly:no-result]`).
- Each night runs in a hotbed session of its own, on `CLADEFS_TEST_HOST`'s node, which the cron
  entry sets, and builds both sides in its workspace (board 6058).
  A single metric's NO RESULT means one of its rounds and that round's rerun were both left out.
- The log: `~/.local/state/cladefs/nightly/nightly.log` on the test host, a line a step; cron's output in
  `cron.log` beside it.

A night by hand, on the test host (it skips if a night measured main's code already):

```sh
~/src/cladefs-nightly/scripts/nightly-host.sh
```

The nightly host's setup installs it: it runs `scripts/nightly-install.sh` as the user (the clone,
its git identity, the cron entry; it checks first that hotbed, flock, the board's
`vb`, `BOARD_URL` (which the cron entry keeps) and a fetch without a prompt are there):

```sh
BOARD_URL=<the board, as the host reaches it> scripts/nightly-install.sh
```

## Benchmarks

`scripts/bench.sh` compares the guest kernel's Fxfs, on an extent-store device
the extent daemon serves, with a DAX window and without, with ext4 on the same
kernel, over a virtio-blk disk backed by an image file on the same host
filesystem and with DAX on writable virtio-pmem. The workloads are fio's sequential and random reads and
writes (cold and warm), fsync-heavy appends and I/O through `mmap`, creating,
stating, listing and removing 2,000 files (`CLADEFS_BENCH_FILES`), and untarring
and building Lua (`guest/tests/test_cases/src/test_fs_bench.rs`).
`scripts/bench-report.py` turns the measurements into a table of medians,
spreads and ratios to ext4. The guests' root filesystem is an Alpine image
built with podman.

```sh
scripts/bench-host.sh      # x86-64 Linux with KVM, on the test host
scripts/bench.sh           # this host, Linux
```

`scripts/bench-host.sh` keeps its results in `docs/measurements`, with the test host
and the source it measured.

For SQLite, set `CLADEFS_BENCH_GROUPS=sqlite`. This compares cold and warm scans
through pread and mmap, single-update commits and batches of 100 updates in
DELETE/FULL, WAL/FULL and WAL/NORMAL modes, and explicit WAL checkpoints.
`CLADEFS_BENCH_SQLITE_TXNS` sets the commits per workload (default 200).
The group also checks database integrity after reopening, recovery from a
killed writer, and a WAL reader's snapshot during a concurrent writer's commit.
Process recovery does not test power-loss durability. WAL/NORMAL commit rates
exclude the separately reported checkpoint and have different durability
semantics from WAL/FULL.
