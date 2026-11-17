# Testing and measurement

```sh
scripts/test.sh          # macOS or Linux
scripts/test-linux.sh    # Linux, in a Lima VM
scripts/test-host.sh     # x86-64 Linux with KVM, on the test host over ssh (ubu1)
```

`scripts/lima-setup.sh` creates the Lima VM the Linux scripts use on macOS, and
installs what they need in it; run again, it installs only what is missing.

The guest tests (`guest/tests`) boot cladefs's guest kernel in cladefs's
Firecracker, on Linux with KVM, and mount its Fxfs from an extent-store device,
whose store cladefs's extent daemon serves (`scripts/test-e2e.sh` starts one):
`extent-guest` (the driver), `fxfs-guest`, `fxfs-guest-dax`, `fxfs-guest-checks`
and `fxfs-guest-checks-dax` (file workloads, including `mmap`, after which the
host remounts the store, fscks it, and checks what the guest wrote), the crash
tests, resizing a mounted store, and pjdfstest. The harness (runner, guest agent
and test cases) came from libkrun's tests, Apache-2.0 (`guest/tests/LICENSE.libkrun`).

```sh
scripts/test-e2e.sh        # Linux (KVM): CLADEFS_GUEST_KERNEL names the kernel
scripts/test-e2e-linux.sh  # Linux, in a Lima VM
```

Each case is in suites, which the scripts run it by: `scripts/test-e2e.sh --tag kernel`, or cases
by name, `--tag e2e` if none (the runner's `list --help` names the suites). A run's outcomes are
data, a repeat's in `CASE/repeat-K.json` and the run's in `summary.json` (`scripts/guest_results.py`).

`scripts/test-e2e-rustfs.sh` runs them with their branches in an S3 bucket on the host (RustFS, a
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
sudo scripts/pjdfstest-fuse.py target/debug/cladefs-mount   # Linux
```

What the extent daemon costs is a test's assertion too, its budgets the design's (TASK-330): its
anonymous memory, sampled as its stats are, against the sum of what its design bounds
(`cladefs::cladefsd::process`: its upload memory, a framing for each store written, its fetches,
its tenants' bytes in flight, its own), which its stats' `process:` line and its metrics say; and what it
writes, by kind, against what it says it wrote. `scripts/test-e2e.sh` fails a run whose daemons
passed either (`scripts/budgets.py`, which says too what each wrote for each guest byte, by kind);
a host's store held to what its design writes for a guest's large file and its small files, each
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
LangSmith's controlled load, 16 VMs eight at a time, each doing more: it runs on ubu1 alone, by
hand, in the qualification before each candidate (below), as no CI runner has its memory (TASK-330.5).
Each run keeps what the host and the run spent every 30 s (`resources.log`):

```sh
scripts/sandboxes.sh [short|long]   # Linux (KVM); CLADEFS_SANDBOX_DISK, ext4, holds it all
```

## Before a candidate goes to LangSmith

LangSmith runs cladefs at a commit they take, their pin (96c7808b8 as of 2026-11-11, TASK-310), and
bump it now and then. Before a candidate is offered, it is qualified against their pin on ubu1,
alone there (`fxjob --bench`), the two interleaved, A B then B A (TASK-330.4):

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

`fxjob --bench` gives a measurement cores, cache, memory bandwidth and memory of its own; not the
disk, ubu1's one NVMe, which others write to all the while. So every round of a measurement runs
under `scripts/contamination.py` (the A/B's rounds, `scripts/bench-host.sh`'s run,
`scripts/bench-rounds.sh`'s), which records in the round's results (`contamination.json`) what
others read from and wrote to the round's disk as it ran, the disk's own count less the round's
cgroup's, and under ubu1's scratch zvol the pool's other datasets' writes; and how long the
round's tasks stalled on CPU, memory and I/O. A round others wrote more than 10 MB/s beside,
or 50 MB/s in any one minute, is contaminated (`CLADEFS_CONTAMINATION_WRITE_MB_S`,
`CLADEFS_CONTAMINATION_MINUTE_WRITE_MB_S`): `scripts/bench-ab.py` leaves its timings and rates out
of the comparison, saying so above the table and in each cell, and keeps its counts and sizes
(bytes written for each guest byte, peak memory), which a load doesn't move. A metric with no
clean round on a side is NO RESULT, and fails the comparison. Where others' I/O can't be told
from the round's own (no cgroup v2 cgroup of its own, as in a CI runner's container or on macOS;
a ZFS dataset as the round's disk), the round is unchecked, said as such, never clean
(TASK-375). A round run by hand is recorded the same way, and `scripts/contamination-check.sh`
checks the record on a host with fxjob, a round alone and one beside a loader of its own:

```sh
scripts/contamination.py run RESULTS -- scripts/qualify.sh RESULTS
scripts/contamination-check.sh /mnt/fxdax-scratch/contamination-check   # ubu1, four minutes
```

`scripts/test-guest-kernel.sh` builds the guest kernel with Fxfs in it and runs
the guest tests on it, in Lima on macOS.

`scripts/test-host.sh` runs them too, on the test host, a Linux machine of one's own, over ssh
(`CLADEFS_TEST_HOST`, ubu1 by default): in a worktree there of a commit origin has,
under `fxjob`, copying the results back and removing the worktree
(`scripts/lib/host.sh`).

The protocols whose bugs are rare interleavings have TLA+ specs, which TLC
model-checks exhaustively over small bounds (`specs/`): live read-only
followers (`specs/follower`, backlog doc-8), the guest Fxfs's copy-on-write
DAX (`specs/dax-cow`, backlog doc-13, `guest/linux/fs/cladefs/dax.c`),
branches in the bucket with the host's write-ahead log (`specs/bucket`,
backlog doc-7 and doc-14), and Fxfs's super-block claiming only what its
journal flushed (`specs/journal`). Each rule
of a protocol has a mutation, the spec with that rule removed, which TLC must
find the violation of: `specs/dax-cow`'s include a writeback leaving a fault's
PTE writable, which guest/sim found (`guest/sim`).

```sh
make check-tla             # about two hours, each model in minutes, on Java; fetches TLC once
```

## The nightly measurement on ubu1

Every night at 02:00 a cron entry on ubu1 runs `scripts/nightly-host.sh` from the nightly's own
clone (`~/src/cladefs-nightly`), main's own copy of it (TASK-441). If main's code didn't change
since the last night that measured (its newest commit that changed more than `docs/measurements` and
`backlog`), it does nothing. Otherwise it measures main's head against the commit that night
measured, as an A/B (`scripts/bench-ab-host.sh`, on ubu1 itself, `CLADEFS_TEST_HOST=local`): the
head built as work, the base's artifacts kept from the night it was the head (`CLADEFS_AB_KEEP`),
then three rounds of both sides interleaved alone in `fxjob --bench`, each of the fs bench's cases
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
each case or count benchmark that failed only on the head, becomes a backlog task labelled
`nightly` with both commits, every round's numbers and the command that replays it; a later night
that finds it again adds a note to the open task. A quiet night files nothing. Each night's results
are committed to main from the clone, the push retried on a main that moved; a push that fails
stays in the clone and is said in `~/.local/state/cladefs/nightly/push-failed.txt`, and the next
night pushes it first.

- Results: `docs/measurements/ubu1-nightly-<time>-<head>/nightly.md` for each night, and
  `docs/measurements/nightly-history.md` for all of them, newest first.
- A night that **could not run** (a side failed to build, no round ran, or the scratch disk was
  short of room) or that has **no result** (every round was left out: the disk was busy, or the
  host drifted) measured nothing: its row says why, and its `nightly.md` lists each round and why
  it was left out. The next night measures the same commits again, or main's newer head against
  the same base. Three such nights in a row file a task (`[nightly:no-result]`).
- The scratch disk, `/mnt/fxdax-scratch`, is every job's (TASK-489). A night first removes its own
  earlier runs' directories (`run-nightly-*` whose job isn't running) and keeps the artifacts of
  tonight's two sides alone; then checks the disk has room before it builds and before it
  measures: `CLADEFS_NIGHTLY_ROOM_BUILD` (100G) and `CLADEFS_NIGHTLY_ROOM_MEASURE` (20G), or a
  fifth more than the most the last 7 nights took, if more (`scripts/nightly.py`'s `ROOM` says
  why). Short of it, the night could not run, and its `nightly.md` names the disk, the free space
  found and the need.
  A single metric's NO RESULT means one of its rounds and that round's rerun were both left out.
- The log: `~/.local/state/cladefs/nightly/nightly.log` on ubu1, a line a step; cron's output in
  `cron.log` beside it.

A night by hand, on ubu1 (it skips if a night measured main's code already):

```sh
~/src/cladefs-nightly/scripts/nightly-host.sh
```

Installing it is ubu1's setup, a `~/docs/system-setup.d` step that runs, as the user,
`scripts/nightly-install.sh` (the clone, its git identity, the cron entry; it checks first that
fxjob, flock, cargo-nextest, Backlog.md's `backlog` and a fetch without a prompt are there):

```sh
# ~/docs/system-setup.d/151-cladefs_nightly.sh
sudo -u ramon -H bash -lc '~/src/fxdax/scripts/nightly-install.sh'
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
scripts/bench-host.sh      # x86-64 Linux with KVM, on the test host (ubu1)
scripts/bench.sh           # this host, Linux
scripts/bench-linux.sh     # Linux, in a Lima VM
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
