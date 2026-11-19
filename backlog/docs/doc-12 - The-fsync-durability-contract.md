---
id: doc-12
title: The fsync durability contract
type: other
created_date: 2026-10-31 22:17
updated_date: '2026-11-19T13:55:00Z'
---
# What an acknowledged fsync survives (TASK-66)

doc-2's requirement 2 is the rule: a crash may lose a suffix of recent history, and the filesystem
recovers to a prefix of it. fsync keeps its ordering, and need not wait for a commit to be local. This says
what that means, failure by failure. The *Tested by* column names what shows it, or says nothing
does: a claim no test shows is a design statement, not a result.

*Revised 2026-11-04 (doc-13, TASK-164):* with DAX as without it, writeback is the write path. A DAX
file's written pages are private pages in host memory, as the page cache's are folios in guest RAM,
and reach the store only as writeback `WRITE`s them, then commits. The window plays no part in
recovery.

## The levels a commit reaches

| Level | What it means | When |
|---|---|---|
| Ordered | `COMMIT` returned: cladefsd has it, in order: its data in the store's files, its commit record in cladefsd's memory (`Pending`, `Store::commit_locked`) | as `fsync` returns |
| Local | the data synced, then the record written and synced (`Store::make_durable`) | within 50 ms at the median (32 ms on main); the crash bound, 1 s, at most (below) |
| Replicated | of a replicated cell (doc-21): on the disks of f+1 of its configuration's 2f+1 members, the writer's own Local copy one, each synced; or Uploaded | a member's round trip and sync, overlapping the writer's own (TASK-537.3.3) |
| Uploaded | in the bucket, a contiguous prefix, acknowledged by the head's swap while the writer holds it | the lag (below) |

*Revised 2026-11-08 (TASK-221, the user's decision: the window is what main achieves):* "within
50 ms" is the **median**: cladefsd makes the commits ordered local every 50 ms, a flush behind the
host's disk at worst. Measured on main (08d444ef7, GCE n2-standard-16, jailed, 3 rounds each of
fxfs-guest-pgbench and -pgbench-dax, ~365,000 commits a round, cladefsd's `durable_lag_*` stats): the
median 32 ms or less, p99 64 ms or less, 85% within 50 ms, and the longest under 1,024 ms (about
0.7% of commits over 64 ms). What a daemon or host crash may lose is bounded by that longest lag:
**the crash bound, 1 s**. The daemon-killed test holds cladefsd to it (`CRASH_LOSS_BOUND_MS`), and the
pgbench cases to the median (`durable_lag_within_50ms`). Shortening the tail is performance work
(TASK-221's notes: commits whose sealing copies whole extents into the open segment).

A guest's **fsync, fdatasync, msync and O_SYNC** wait for *ordered*. **sync, syncfs, unmount and
freeze** ask for `COMMIT_UPLOADED`, and wait for *local*, and with a bucket, for *uploaded*.
So an acknowledged fsync is not yet local: it is ordered, and survives what leaves cladefsd alive.

*Added 2026-11-12 (TASK-384):* *uploaded* needs *local* of the commits, not of the
segment's framed file. A segment's upload waits for its commits' records synced in its log (the
flush), and for its framed file written, not synced: the framed file's sync, and its directory's,
go on at once with the upload, and the store hears of the acknowledgement only once they are done,
its files leaving then as before. A crash meanwhile keeps every commit: the segment's raw file and
log, which hold them, stay until its framed file is synced; once the bucket acknowledged it,
recovery takes its commits as the bucket's (`wal::recover`: a log's commits the start acknowledged
are the bucket's), and, the store not told yet, the bucket's state, which the store opens on, holds
them. And the next segment's raw file and log are made ahead of it, under their names, their
entries synced off the commits: a raw file and log holding no record, past the last segment,
hold nothing, and an opening removes them (`ending`, TASK-334). An uploaded commit, closing a segment
and opening the next for its upload, waited for those syncs one after another, ext4 journal
commits, some 8 ms over the bucket's round trip on ubu1; 2 ms after
(`durable_commits_through_the_daemon`, crash test `segments_upload_before_their_framed_files_are_durable`).

*Noted 2026-11-15 (TASK-397):* none of the figures above ran with the store on ubu1's home, which
is ZFS. The median, the p99 and the crash bound are GCE's (TASK-221), and TASK-384's 8 ms and 2 ms
are of a store on ubu1's ext4 scratch disk (/mnt/fxdax-scratch) with the bucket on tmpfs. What did
run on ZFS is every measurement scripts/bench-rounds.sh made before 2026-11-15: its results'
directory, in the home, held each run's stores and its file bucket. Their local commits' sync rates
(TASK-371's 3.2/s, 6.1/s, 10.8/s and 15.9/s; docs/measurements' ubu1-loop-baseline-2026-11-11, its
-governed run and ubu1-unsafe-2026-11-11) are ZFS's syncs' as much as cladefs's, 91 ms a sync there
against 32.5 ms with the store on ext4 and the bucket in memory, and say nothing of this
contract's *local*. bench-rounds.sh now keeps a run's stores on ext4 and its bucket on
tmpfs, and refuses another filesystem (scripts/test-e2e-job.sh).

The lag is bounded by commits and bytes (`upload::Options`: 4096 commits, 1 GiB, by default) and in
time by the segment's age. Past the bound, commits wait: the lag is what a lost host can cost, and
it stays within the bound.

## What each failure keeps

| Failure | fsync acknowledged | sync acknowledged | Tested by |
|---|---|---|---|
| Guest crash or panic, VM reset | kept | kept | fxfs-guest-crash, -crash-dax; guest/sim's `cow_crash_between_bytes_and_commit` (a commit held after its bytes) |
| VMM killed (SIGKILL) | kept | kept | fxfs-guest-crash-dax-killed |
| cladefsd killed | what was ordered within the crash bound (1 s) may go, its records unwritten; a prefix stays, a file whose fsync returned in that time found short or empty at worst. The VM exits 158 | kept | fxfs-guest-crash-dax-daemon-killed (1 of 296 files told of went, GCE r342), fxfs-guest-daemon-lost |
| Host reboot or kernel crash, disk kept | what was ordered within the crash bound may go; a prefix stays | kept | **nothing: no test loses a host's page cache (TASK-67)** |
| Host lost, disk with it, with a bucket | the unuploaded lag goes; a prefix stays | kept | the simulation (doc-7, `crashes_anywhere`); **no guest test (TASK-67)** |
| Host lost, without a bucket | everything goes | everything goes | not a recovery case: a store with no bucket, or whose bucket is a file bucket beside it (an image's, `<image>.bucket`), lives on one disk; cladefsd refuses to start without a bucket (`CLADEFSD_REMOTE`, TASK-45) |
| The bucket failing for good: out of reach, full, refusing its credentials | kept: waits at the lag bound, then fails nothing (below) | kept: waits | extent_bucket's `writes_failing_say_why_until_they_land` (a full disk's `ENOSPC`, logged); cladefs's `commits_wait_at_the_lag_bound_while_the_bucket_is_full`, `commits_wait_at_the_lag_bound` (TASK-314); for minutes, slow, full, then out of service, then answering, VM processes writing all the while: cladefs's `cladefsd::tests::soak` (5 min in CI, 30 each night), `fxfs-guest-bucket-soak` with guests (TASK-330.3) |

Every row keeps ordering: what survives is a prefix of the commits, never a later commit without an
earlier one. Filesystems recover by replaying their journal to that prefix. Open-but-unnamed files
are freed as the store is next mounted.

### A replicated cell's (doc-21, TASK-537.3.6)

*Added 2026-11-19 (TASK-537.3.6):* a cell made or taken with `replicas` f >= 1 (CreateCell) waits
for *replicated* where the table above says *uploaded*: its sync family's `COMMIT_UPLOADED`
returns once f+1 copies hold the commit, or the bucket does, whichever first. Its fsync waits for
the level its `fsync` asks (`FsyncLevel`: Ordered, Local, or Replicated). Every row keeps ordering,
as above. With f = 1 (three members):

| Failure | fsync (Replicated) acknowledged | sync acknowledged | Tested by |
|---|---|---|---|
| Guest crash, VMM killed | kept | kept | the rows above |
| cladefsd killed, or the host rebooted, disk kept | kept: the reopen is a take, which recovers from the members and its own write-ahead log, whichever goes further | kept | local_cell's `members_go_past_own_logs_by_writer_then_commit` (the choice alone); **no test kills a replicated cell's daemon and reopens it on the same disk** |
| Writer's host lost, its disk with it | kept: a take on another host fences the two members and recovers from them | kept | cladefs's `replicated_commits_survive_their_writer_and_its_disk` (three daemons, the writer's killed and its directory removed, the bucket out of reach meanwhile); local_cell's `takes_recover_what_a_lost_writers_members_hold` |
| Writer's host lost, the other two left | kept, as above; the taker has no configuration of its own to be had, so it serves degraded (f = 0), the head naming the configuration before until the acknowledgement naming what it recovered names its own (doc-21), and is replicated again as a third host answers | kept | cladefs's `replicated_commits_survive_their_writer_and_its_disk` |
| Writer's host and a member lost | Replicated commits not yet uploaded may go: more than f failures. A take is refused until f+1 of the members other than the old writer answer | as fsync | local_cell's `takes_without_enough_members_to_fence_are_refused` |
| A member lost, writer up | kept, Replicated on the other member meanwhile; the writer replaces it after 30 s (`CLADEFSD_REPLACE_AFTER`), the new one given the tail, and its commits are Replicated again | kept | cladefs's `lost_members_are_replaced_then_the_bucket_releases_commits` (four daemons, the bucket out of reach after the replacement); local_cell's `lost_members_are_replaced_and_commits_stay_replicated` |
| Both members lost, writer up | kept: degraded, its commits wait for Uploaded, uploaded at once; with no replacement to be had it drops to f = 0 once the bucket holds what it told, and comes back as members answer | kept | cladefs's `lost_members_are_replaced_then_the_bucket_releases_commits`; local_cell's `degraded_cells_drop_to_f_zero_and_come_back` |
| The bucket failing | kept, Replicated as before, waiting at the lag bound | kept | cladefs's `replicated_commits_survive_their_writer_and_its_disk` (the commits told durable while the bucket is out of reach, its directory moved aside, the head checked unchanged after) |
| The bucket failing and both members lost | waits, as the bucket failing does above | waits | cladefs's `lost_members_are_replaced_then_the_bucket_releases_commits` (both members killed, the bucket out of reach: the commit's reply waits, then comes once the bucket is back) |

StatCell says `degraded`, `members_up` and `active_replicas`; `/metrics` says
`cladefsd_replication_degraded{cell}` and `cladefsd_replication_members_up{cell}`, and the daemon
logs a warning as a cell is degraded and as it is no more.

## What a guest sees meanwhile

- **A store through a mapping, or a `write(2)`,** is in the page, not in a commit: a private page of
  the VM's on DAX, a folio of the guest's page cache without. It is covered by the next `msync` or
  `fsync` of the file, and may reach the store before that only by writeback (`sync`, the flusher
  at ~30 s, the guest keeping its private pages under `private_max`), each page as writeback copied
  it. Every crash, of the guest, the VMM or cladefsd, loses what wasn't written back: private pages
  go with the device.
- **A `write(2)` cut short** leaves the bytes it copied, as on ext4: nothing it writes moves.
- **The device's store failing** fails the store: a commit that failed part way leaves it refusing
  what follows (`State::failing`), and the guest's writes and fsyncs return errors.
- **The bucket failing**, for a while or for good, fails nothing (TASK-314; doc-7, decision 3: as
  a hard NFS mount hangs). Every request it fails is retried, from 50 ms apart, doubling, to 5 s,
  until it is answered. Meanwhile the guest's `sync`, `syncfs`, unmount and freeze wait at once,
  as they wait for *uploaded*; its fsyncs go on, ordered and made local, until the lag
  reaches its bound (4,096 commits or 1 GiB of write-ahead log), then wait too; its writes go on,
  and so do its reads of what the host has, but a page-in of what only the bucket holds waits. Its
  kernel says so as a hang does (`fxfs: Write I/O request blocked for N s`, hung tasks), and
  nothing returns an error. Once the bucket answers, within 5 s of its next retry, all of it goes
  on, and nothing was lost: what waited was local all along. *Decided 2026-11-10
  (TASK-314), the most conservative of the options:* failing commits after a bound instead would
  give the guest errors for data that is safe on the host's disk, and a guest filesystem that
  errors on fsync shuts down, or retries what Linux has already dropped from its page cache: a
  failure that frees the bucket's disk, or renews its credentials, cures, made one that loses
  data. It is loud instead: cladefsd logs each kind of request failing as the run of failures
  starts, a warning naming the bucket's own error (a file bucket's `No space left on device`,
  S3's `AccessDenied`), then every 30 s with how many failed, and as it answers again
  (`extent_bucket::object_bucket`, `FAILING_EVERY`); counts them by kind (the stats line's `failed-NAME`,
  GetCounters' `ObjectStorageStats.failures`, the metric `cladefsd_object_storage_failures_total`),
  beside how long each store's oldest commit has waited for its upload
  (`cladefsd_upload_oldest_pending_seconds`), what an operator alerts on; and warns as a
  commit first waits at the bound. *Added 2026-11-10 (TASK-317):* meanwhile cladefsd's memory does
  not grow with what waits, which is on the host's disk: what it holds of it is what it uploads at
  once, its upload memory (`CLADEFSD_UPLOAD_MEMORY`, 512 MiB), and it gives back what it
  frees (doc-7, Uploads, Memory). Before, a backlog held its uploads twice and the allocator kept
  what it freed, 4-6 GB of anonymous memory, and the host's OOM killer took the daemon, every VM
  it served with it.
  *Added 2026-11-10 (TASK-330.3):* once the bucket answers, every commit made before is uploaded
  within that retry's 5 s, the upload interval's second, and the time the guests took to write what
  waited, as the bucket, answering, takes it back at least as fast as it took it before it failed;
  and the daemon's anonymous memory stays within its budget meanwhile (doc-7, Memory), every
  sample of it.

## How a host tells them apart

A VM whose daemon goes exits with status 158 (`CladefsdLost`), within the bound its test holds it
to. The README lists the rest ("How a VM process exits").

## Not promised

- That an fsync survives cladefsd or the host going within the crash bound (1 s) of returning. An application that
  needs that must `syncfs`, which costs a round trip to the disk, and to the bucket with one.
- Anything of what the lag holds, across the loss of a host.
