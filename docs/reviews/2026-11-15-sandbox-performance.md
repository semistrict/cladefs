# Where agent sandboxes will hit performance problems, 2026-11-15

Seven read-only reviews of main (80138a886), each of one area, against one workload: LangSmith's
agent sandboxes. Nothing was built or run: **every size below is an estimate from the code, not a
measurement.** Its purpose is to rank what the nightly (TASK-441) measures first. "Checked" means
the cited code was read again after the review; the rest is one reviewer's reading.

## The workload assumed

Hundreds of VMs a host, forks of a few popular bases or restores of snapshots, several tenants.
High churn: fork or restore, boot, bursts of commands between long idle waits, snapshot, restore
elsewhere, delete; most live minutes, some weeks. Inside: git, pip/npm/apt (tens of thousands of
small files), builds and tests, docker build, sometimes a database. Up to 8 vCPUs a VM,
overcommitted; one local SSD shared by every VM, the WAL and the part cache; a bucket tens of ms
away, sometimes slow. What the customer feels: time to ready, p99 of small commands, install and
build throughput, idle sandboxes per host, host and bucket cost per sandbox, one tenant slowing
another.

## Ranked: what to measure first

### A. A whole host stops, or stops admitting VMs

1. **Request intake stops for every VM once ~512 requests block** (checked; three reviewers
   independently; the stall itself rests on tokio's behaviour at its blocking-thread cap).
   `crates/cladefs/src/extentd/server.rs`: the `IO` runtime has `.worker_threads(2)` and no
   `max_blocking_threads`; every request is served in
   `tokio::task::block_in_place(move || connection.serve_in_turn(message))`, 8 a connection
   (`WORKERS`). A `READ` of data not in memory sleeps in
   `futures::executor::block_on(self.clone().bring_in(page))` (`crates/cladefs/src/cache.rs`); durable
   and lagging commits wait likewise. ~65 VMs with 8 cold reads each, or a slow bucket with a few
   hundred VMs, and no connection's requests or faults are received. TASK-92.20 fixed this for
   `FAULT` alone. *Measure:* 80 non-DAX forks cold-reading through a throttled bucket and one VM
   reading local data: its latency, and the daemon's thread count.
2. **Daemon heap grows as base extents × forks, outside the governor** (checked).
   `crates/extent_store/src/store.rs`, `Recovering::open`: every open collects the branch's whole
   map, inherited extents too, into a `HashMap<ExtentId, Extent>`, each with an owned key `String`
   (`extent_frame::segment::Location`). ~150-200 B an extent a fork: 2 GB at 32k extents and 400
   forks, 20 GB at 300k. *Measure:* `serves_hundreds_of_forks_of_a_base` with `EXTENTS` at 32,768
   and 300,000 (it runs at 64). TASK-92.55's slice 4 names the copy for open time, not memory.
3. **VMs a host are capped by private pages reserved, not used** (checked).
   `PRIVATE_MAX_DEFAULT` is 1 GiB (`crates/virtio_extent/src/config.rs`), pinned whole at `WINDOW`
   by `CopyOnWrite::reserve`, against 7/8 of the allowance (`Arenas::pin`): ~105 VMs on 128 GiB, 7
   for a tenant limited to 8 GiB. And an idle guest keeps up to 7/8 of it in clean private pages
   (`fxfs_dax_work` relieves only near the bound). *Measure:* idle VMs started until `WINDOW` fails;
   `cladefs_extentd_vm_window_private_pages` after a write burst and five idle minutes. The perf
   gate's density phase runs 3 VMs.
4. **Every sandbox ever made costs every sweep 5 requests, for ever** (checked: `Kind::Head` is
   `Fate::Stays`, `records_kept` is true). `crates/extent_remote/src/collector.rs`: a dead cell
   keeps its cell object, record, head, lease, and a `retired/` mark under its parent; each sweep
   takes its lease, lists it, releases it. At 10^5 sandboxes a day, 30 days in: 1.5×10^7 requests a
   sweep, hours long, so garbage lingers as long. A popular base's pass also lists every reference
   and every mark ever (`passed::list`), a million after 100 days. *Measure:* `scripts/gc-sweep.sh`
   over N cells all deleted, third sweep's requests against N.
5. **Giving memory back is serial and evicts far more than asked.** `extentd/memory.rs` `shrink`
   waits for every VM's unmap between extents; each page is quarantined alone under the `PEERS`
   mutex; any overshoot evicts every tenant above its share down to it; memory returns only in
   whole 2 MiB chunks. A second cause of TASK-366. *Measure:* `cache.relief` span's evicted bytes
   against the drop in `physmem::arenas_bytes()`, by connection count.

### B. Time to ready

6. **A restore is ~13 serial bucket round trips warm, ~25 cold, and ~10 fsyncs in a row.**
   `branch::fork_point`/`finish_point_fork`, `upload.rs` `child_free`/`Shared::start`,
   `recover_layers`, `cell::hold`, `Writer::take`: the child's record is read three times and its
   head twice, for a branch made milliseconds before. `store_dir::write`, `Store::make`,
   `Segments::open`: each a journal commit on the disk every VM shares. ~0.4-0.75 s of bucket at
   30 ms, seconds on a busy disk. *Measure:* a point-fork phase in the ignored
   `remote::tests::bucket_round_trips_by_phase`; fsyncs counted around one fork beside 8 writing
   VMs.
7. **Restores of a point still being opened fail `EBUSY` after 10 s** (`stores.rs` `hold_to_fork`,
   `CLOSING_WAIT`): 200 restores of one snapshot on a cold host with a slow bucket. *Measure:* 20
   `ForkFrom` at once with the bucket slowed past 10 s.
8. **Every boot scans the whole allocator tree and replays what journal the base was closed with**
   (`allocator.rs` `load_extents`, `Query::FullScan` into a `HashMap`). Grows with the image.
   *Measure:* mount time and guest memory against 100k and 1M files.
9. **One 4 KiB cold touch is 2-17 one-MiB GETs** (`fetch.rs` `read_ahead`, `AHEAD` 16, under
   `cache.rs` `PARTS_AHEAD` 2), all in the host's 64-GET budget with demand reads; only 64 MiB of
   fetched parts stay in memory for the whole host (`RECENT`), so small-file reads re-read and
   re-unpack 1 MiB each; a demand read waits for the part's write to the shared SSD
   (`Fetching::part`). *Measure:* GETs and fetched bytes for one fork's boot on a cold host,
   read-ahead on and off; `bench_reads_while_parts_are_kept` beside a disk writer.
10. **Under 4 GiB free on the SSD the part cache empties itself and keeps nothing** (checked:
    `Kept::make_room` walks `by_use` to its end). That is when the bucket is slow and WALs back up.
    *Measure:* `cladefs_extentd_part_cache_bytes` as the disk fills.
11. **A restored VM's window comes back one userfaultfd fault at a time on one thread**
    (`uffd.rs` `map` installs nothing since TASK-276.6; the guest has its PTEs, so no `MAP_AHEAD`),
    five syscalls a present run, a bucket page-in otherwise. *Measure:* `dax-stage-syscalls.bt` and
    `Faults` between resume and first command, 1, 8 and 32 restores.

### C. Small commands' p99, installs and builds in the guest

12. **`stat` of a file just written writes it back in the foreground, and every regular-file stat
    takes the journal's writer mutex** (checked). `super.c` `fxfs_vfs_getattr`:
    `filemap_write_and_wait` when `inode_get_bytes(inode) < i_size_read(inode)`;
    `Journal::file_offset` locks `writer_mutex`. git checkout, make. *Measure:* create, write
    1 KiB, fstat, close against the same without the fstat, by `device_writes`.
13. **At most 4 transactions in flight filesystem-wide, a slot held across device writes**
    (checked: `MAX_IN_FLIGHT_TRANSACTIONS`). Likely what TASK-288 sees (21% in `fxfs_rt_park`
    under create; fsync flat at 4-5k for any job count). *Measure:* A/B at 64 on `scale-create-j8`
    and `create-fsync`.
14. **Every transaction blocks when the journal reaches 8 MiB until a compaction ends** (checked:
    `JOURNAL_MAX`; the non-Fuchsia `CompactionYielder::yield_now` is empty). Grows with the
    filesystem's age. *Measure:* `journal_space_waits`/`_wait_ns` over `untar` on an image of 1M
    files, a disk loader beside it.
15. **No fault-around on DAX: each 4 KiB of every exec'd binary is a fault** (checked:
    `fxfs_dax_vm_ops` has no `.map_pages`), each taking the inode's locks and the range LRU's
    global lock. *Measure:* faults an exec of `/bin/true` and `cc1`, DAX against no DAX.
16. **Once the window fills, each new chunk reclaims a hot one** (`fxfs_dax_reclaim`: the LRU is
    refreshed by iomaps, so a mapped library with live PTEs ages; one rmap walk per entry; faults
    wait in 10 ms steps). Slows run over run. *Measure:* `window_full`, `window_reclaims`,
    `window_unmaps_hist` after reading twice a 512 MiB window.
17. **Any fsync stalls cold DAX mappings, waiting with the file's semaphore held for writing**
    (`fxfs_dax_sealed_extents`). apt or sqlite beside exec. *Measure:* `seal_waits`, and
    `window_map_requests_hist` with an fsync loop on one vCPU.
18. **Layer-file chunks, once read, stay in guest memory** (checked: no caller of `clear_cache`
    or `purge` but tests; the shrinker frees TreeCache items alone). *Measure:* `MemAvailable`
    after `find` over 1M files, then `drop_caches`.
19. Also: each small file's writeback is two transactions and an awaited device write on the one
    flusher; create and rename apply every queued unlink first (`After::All`); 98 KiB of free
    space reserved per dirty file, then full syncs; a 20 µs spin before every wait
    (`fxfs_rt_park`); one receiving thread a VM doing every window change and pre-fault inline.

### D. Neighbours and cost

20. **The host's upload budget is first come, first served** (checked: `Budget::admit` looks at
    the queue's head alone; every upload is `Priority::Demand`): a small VM's sync, snapshot or
    close waits behind every byte its neighbours queued. *Measure:* one VM writing 4 GiB, another
    looping `sync` on 4 KiB, the bucket rate-limited.
21. **A sandbox that lived minutes uploads all it wrote, and a checkpoint of its whole state,
    before it can be deleted** (checked: `store_table.rs` cuts a drain short for a
    `Claim::Writer` alone; `upload.rs` checkpoints on every clean stop). Delete returns `EBUSY`
    meanwhile, and itself downloads the checkpoint to list forks (`branch::delete_under`).
    *Measure:* write 500 MiB, kill the VM, delete: time, and bytes uploaded after the kill.
22. **A snapshot holds the paused VM until its backlog is in the bucket** (`take_snapshot` ->
    `fork_quiescent` -> `wait_uploaded`), then re-downloads its own state and writes its
    checkpoint twice (`recovery::checkpoint_at`, then `writer.checkpoint()`). *Measure:*
    `upload_wait` span for a snapshot after N GiB written.
23. **Each store that committed in the last second closes a segment: 2 PUTs and ~6 fsyncs**
    (`upload.rs` `gather`); idle writer stores wake ~21 times a second (`FLUSH_EVERY` 50 ms, the
    1 s `gather`). *Measure:* requests and fsyncs a store-second, and daemon CPU, at 0, 300 and
    1,000 idle stores.
24. **A commit reads back and compresses every extent of 4 MiB or more under the store's locks**
    (`store.rs` `seal_own`); framing and packing take every host CPU a store, unbounded across
    stores. *Measure:* `commit.seal` span and READ p99 during a 2 GiB `dd` and fsync.
25. Also: a checkpoint lists once per compacted object, serially, the uploader waiting
    (`MovesMaps::read`); every full collector pass re-reads every live segment's trailer; garbage
    goes one lineage generation a sweep; GETs, LISTs and deletes retry without jitter or a shared
    limit; writes over 256 KiB go through the socket with two zeroed buffers; control calls share
    32 threads and retry for ever, so a slow bucket stalls `Grant`; `StatStore` of a closed store
    opens it whole; metrics series per connection and branch.

## Verified fixed from the 2026-10-21 review

Unlink queue drained by lookups; a transaction per extending write; mount replay a block a round
trip; the 200 ms stats walk; restore's one MAP per mapping; struct pages for the whole window at
probe; the sealer under `files_lock`; the PMM's lock across mmap. Partial: relief still clones
every cached extent under the cache mutex (TASK-92.33); a DAX write iomap still takes its file's
semaphore twice.

## Not settled by reading

Extents in a real base image (sets 2, 8, 9); how often an idle guest commits (sets 23); whether
async page faults and PV spinlocks are on in production guests; whether bases are published
compacted; tokio's exact behaviour at the blocking cap (sets 1).
