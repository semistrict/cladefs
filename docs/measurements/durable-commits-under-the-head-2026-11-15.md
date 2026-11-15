# Durable commits under the branch's head (TASK-407.5), 2026-11-15

What a guest's durable commit costs once a commit is its segment's create, then a swap of the
branch's head (TASK-407), against the create alone before it (TASK-371).

## How

- ubu1 (x86_64, 16 cores), each run under `fxjob --bench` (cores 4-7 to itself), one at a time.
- `fxdax::extentd::tests::durable_commits_through_the_daemon` (ignored; TASK-371's): rounds of a
  guest's sync loop through a daemon serving on a thread, a 4 KiB write and an ordered commit,
  then another write and a durable commit, each round's time; 40 rounds a run.
- A file bucket on tmpfs (`FXDAX_PHASES_BUCKET=file:///dev/shm/...`), each request held 25 ms
  (`FXDAX_REMOTE_LATENCY_MS=25`); the store on the ext4 scratch disk
  (`TMPDIR=/mnt/fxdax-scratch/...`), as TASK-371 measured.
- Two sides, each a worktree and a target dir of its own, the test binary built before the bench
  job: **head**, main at 480962251 (TASK-407, .1, .2, .4); **before**, main at 5632ac7a9, the
  commit before TASK-407 was merged.
- Runs alternated: head, before, head, before, after one of each first.

## Results

Medians of each run's 40 rounds, in ms (min-max):

| Run | head | before |
|---|---|---|
| first | 58.0 (56.7-63.0) | 66.2 (44.2-1063.1) |
| 1 | 63.6 (62.4-96.8) | 36.3 (35.2-66.1) |
| 2 | 60.0 (56.5-103.5) | 30.5 (29.4-36.5) |

The first run of the side before had one round of 1.06 s, and a spread no other run had; left out,
the sides are **about 60 ms against about 33 ms a round**: about 27 ms more, one bucket round
trip of 25 ms and a little. A round's durable commit was one request before, its segment's create,
its fencing; under the head it is two, one after the other, the create and then the head's
conditional swap naming it. The rest of a round (the writes, the ordered commit, the local disk)
is the same few milliseconds on both sides.

A guest's sync loop, one durable commit a sync since TASK-371, goes from about 30 a second to about
17 at 25 ms of bucket latency: the price of the head, which TASK-407 chose for keys written once
(no fences, pins, void segments or versioned reads).

## Not measured

- A real bucket's latency, which varies more than a file bucket held a fixed time.
- The guest's own sync loop (bench-fs-fxfs-guest-dax), which TASK-371 measured too: it needs a
  guest kernel and VMs; this test drives the daemon as a guest's requests would.
