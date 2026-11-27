---
id: doc-29
title: 'Fast WAL commits: appendable object storage without load-bearing local state'
type: specification
created_date: '2026-11-25 00:00'
updated_date: '2026-11-25 00:00'
---
# Fast WAL commits: appendable object storage without load-bearing local state

Design, 2026-11-25. Peer-replicated commits (doc-21) remain supported for environments without an
appendable fast object store (bare metal, R2, local files).

## Why

In cladefs today, every compute host's local disk is a rebuildable cache except for three things:
the unuploaded tail of an `f = 0` cell (explicitly not promised across host loss, doc-12),
ephemeral cells (doc-25), and `CLADEFSD_REPLICAS` (doc-21).

`CLADEFSD_REPLICAS` breaks the "compute hosts are cattle" property:

- Every VM compute host holds other hosts' only copies of acknowledged commits plus safety-critical
  `promise` and `incarnation` state on local SSD.
- Restoring a host disk snapshot or cloning an image without wiping `CLADEFSD_REPLICAS` silently
  violates fencing safety.
- Losing more than `f` hosts wedges cells until a manual `--force` take that drops acknowledged
  commits.
- Host drains, upgrades, and disk-governor budgets couple across hosts.

Yet workloads that need ~2–3 ms host-loss-proof `fsync` (Postgres, SQLite, durable queues) cannot
pay the ~100–150 ms two-RTT cost of standard regional S3/GCS (`PUT` segment + conditional `<C>head`
swap, throttled to ~1 swap/s per object on GCS).

**Fast WAL** solves this by placing the unuploaded commit tail in a managed **appendable zonal
object store** — **Amazon S3 Express One Zone** (`PutObject` with `x-amz-write-offset-bytes`,
implemented today) or **Google Cloud Storage Rapid Storage** (gRPC `BidiWriteObject` with
`finish_write`, planned and rejected at boot until implemented) — instead of peer compute hosts'
local SSDs. Every compute host stays stateless cache-only on the commit critical path: losing any
compute host or wiping its SSD at any moment loses zero acknowledged commits.

Multi-AZ fast-WAL quorum (2-of-3 zonal buckets) is explicitly deferred; single-AZ fast WAL survives
any compute host, SSD, kernel, or daemon failure with RPO = 0, while the existing background
uploader continues to flush framed segments into the multi-AZ regional bucket (~1 s lag).

## The protocol

### Authority and `<C>head`

The regional cold bucket's `<C>head` remains the single authority. A cell is one of:

- **Standard (`Ordered`):** regional bucket only (`replication = None`, `fast_wal = None`).
- **Peer-replicated (`Replicated`, doc-21):** `f >= 1` peer hosts (`replication = Some(..)`,
  `fast_wal = None`). Kept for clouds/environments without an appendable fast bucket.
- **Fast WAL (`FastWal`, this doc):** appendable fast object store (`fast_wal = Some(..)`,
  `replication = None`). Setting both `replicas >= 1` and `fast_wal` is refused (`EINVAL`).

In `<C>head` (`bucket layout` version 35, see **Frozen format change** below):

```rust
pub enum Fsync {
    Ordered,
    Replicated,
    FastWal,
}

pub struct FastWal {
    /// What fsync waits for: `Ordered` or `FastWal` (sync/syncfs/unmount wait for `FastWal`).
    pub fsync: Fsync,
    /// The active fast-WAL epoch and bucket URL (None until the first writer activates).
    pub active: Option<ActiveWal>,
}

pub struct ActiveWal {
    /// Epoch of the writer that owns `<C>wal/<epoch>/*`.
    pub epoch: u64,
    /// Fast bucket URL holding `<C>wal/<epoch>/*` (so a taker in another AZ knows where to fence).
    pub bucket: String,
}
```

### Appendable WAL objects

A writer of epoch `e` appends batches of self-contained, MACed, DEK-encrypted `LogRecord`s (the
same authenticated record format `local_cell::wal` already writes for doc-21) to sequential
appendable objects in its host's configured fast bucket (`CLADEFSD_FAST_WAL_BUCKET`):

```
<cell_prefix>wal/<epoch>/<seg>     (epoch, seg in fixed-width base 32, Padded)
```

Each WAL object starts with a versioned header naming the cell, epoch, `seg`, DEK `KeyId`, and
salt, followed by framed entries:

- **`Batch`:** one or more commits' `LogRecord`s coalesced by the adaptive group-commit batcher
  (`CFWR`, writer AEAD key, strictly monotonic writer `seq` nonce).
- **`Rolled`:** terminal marker written by epoch `e` before creating `<seg + 1>` (`CFWR`).
- **`Fence`:** terminal marker written by a taker of epoch `e' > e` to permanently seal `<seg>`
  (`CFWF`, domain-separated AEAD key derived per taker epoch `e'` with the byte offset as nonce, so
  a concurrent old writer and taker can never reuse an AES-GCM nonce).

On cross-AZ takeover, a daemon opens the persisted `head.fast_wal.active.bucket` URL only if it
matches the host's explicit allowlist (`CLADEFSD_FAST_WAL_BUCKET` plus optional comma-separated
`CLADEFSD_FAST_WAL_ALLOWED_BUCKETS`), preventing a corrupted or untrusted head from directing
takeover probes to an arbitrary endpoint or local path.

#### Writer invariants

1. **Offset-checked appends (`offsetChecked`):** Every append specifies the writer's expected byte
   offset (`x-amz-write-offset-bytes` on S3 Express; stream offset / generation on GCS Rapid).
   Creating `<seg>` always specifies offset `0` (never bare `PutObject`). On any offset mismatch /
   `Precondition` / `Finalized`, the writer is fenced and halts immediately (never re-`HEAD`s and
   retries past a fence).
2. **Roll before the append limit (`rollBeforeLimit`):** S3 Express caps an appendable object at
   10,000 appends (`MaxParts`). The writer rolls `<seg> → <seg + 1>` well before that limit (default
   5,000 appends, or 32 MiB), guaranteeing at least one append slot remains for `Rolled` or a
   taker's `Fence`.
3. **Roll before next (`rollBeforeNext`):** The writer never creates `<seg + 1>` until its `Rolled`
   append on `<seg>` has been acknowledged.
4. **Contiguous after `<C>head` (`walContiguous`):** If the fast bucket is temporarily unavailable
   and the cell degrades to uploading directly to the regional cold bucket (`FastWal(t) ∨
   Uploaded(t)`), the writer never skips an unacknowledged commit when appending to the fast WAL:
   every batch covers a contiguous range starting right after `max(wal_acked, head_acked)`.
5. **Reclaim only after `<C>head` ack, leaving a 36-byte authenticated `Rolled` tombstone (`deleteWalAfterAck`,
   `rolledTombstoneOnDelete`):** A rolled WAL object `<seg>` is reclaimed by its writer only after
   `<C>head` has acknowledged every commit in `<seg>`. Instead of `DELETE` (which resets the object
   size to 0 and lets a delayed `offset = 0` retry resurrect it) or a `Fence` tombstone (which would
   stop a taker from walking to `<seg + 1>` when `<seg + 1>` is still empty), the writer overwrites
   `<seg>` with a 36-byte authenticated `CFTR` tombstone (`0 < 36 < HEADER_LEN`) carrying
   `max_commit_epoch` (verified `<= last_seen_epoch` on takeover). Because every writer append is
   either at `offset = 0` or `offset >= HEADER_LEN` (80 bytes), a 36-byte tombstone rejects every
   possible writer append forever while freeing 99.9999% of the segment's storage.

### Adaptive group-commit batching

To achieve ~2–3 ms latency for serial `fsync` callers while capping append request rate (S3 Express:
$1.13 per million appends) under concurrent write pressure:

1. **Zero-linger fast path:** When no append is in flight and recent concurrency is low (e.g. a
   single serial client doing `fsync` → wait → `fsync`), a durable commit flushes **immediately**
   with 0 ms added delay.
2. **Natural in-flight coalescing:** Only one append is in flight per cell at a time (required by
   sequential `WriteOffsetBytes`). Any commits arriving while an append is in flight accumulate in
   memory and flush together in a single batch as soon as the in-flight append returns.
3. **Adaptive linger under load:** The batcher tracks an EWMA of append RTT (`ema_rtt`) and recent
   batch sizes (`ema_batch`). When concurrent waiters arrive (`ema_batch >= 2` or sustained QPS
   exceeds the serial threshold), the batcher arms a short adaptive linger timer
   `clamp(ema_rtt * alpha, min_linger, max_linger)` (defaults: `min_linger = 250 µs`,
   `max_linger = 2 ms`, `max_batch_bytes = 512 KiB`, `max_batch_commits = 64`). It flushes early as
   soon as either byte/commit cap is reached, and drops back to 0 ms linger as soon as traffic goes
   serial or idle.
4. **Non-`fsync` commits:** When `Fsync::Ordered`, ordinary `fsync`s do not trigger immediate
   fast-WAL appends; when `Fsync::FastWal`, every `fsync` triggers/awaits the batcher, and any
   earlier `Ordered` commits are swept into the same batch so the WAL stays contiguous.

### Large-write optimization

Appending multi-megabyte extents through S3 Express / GCS Rapid incurs per-GB fast-bucket ingest and
egress fees, head-of-line blocks small 4 KiB `fsync`s, and uploads the same bytes twice (fast bucket
then cold bucket).

For any extent at or above the large-write threshold (`fast_wal_large_extent`, default 256 KiB;
configurable):

1. **Direct-to-cold PUT:** The writer frames and encrypts the extent once into its final cold-bucket
   own-extent key (`layout::extent(cell, epoch, bucket_segment, extent_id)`) and uploads it straight
   to the regional cold bucket ($0/GB upload fee on S3 Standard / GCS Standard).
2. **Order before WAL (`largeBeforeWal`):** The fast-WAL `Batch` record referencing that extent
   carries **metadata only** (the extent's cold key + length + checksum, zero payload bytes) and is
   sent only **after** the cold-bucket PUT has succeeded.
3. **Zero double upload:** When the background uploader later uploads `segments/<epoch>.<seg>` and
   swaps `<C>head`, the large extent is already at its final cold-bucket key.
4. **Collector protection (`protectWalEpoch`):** `cladefs-gc` (`collector::Ended::ahead`) keeps any
   unacknowledged `Kind::Extent` whose `epoch == head.epoch` **or** `Some(epoch) ==
   head.fast_wal.active.epoch` (with `segment >= head.end`). Even if a writer crashes and a taker
   raises `head.epoch` before finishing recovery, `cladefs-gc` will not delete the old epoch's
   unacknowledged large extents until the taker activates the new epoch.

### Takeover, fencing, and recovery (flush-on-takeover)

When host `T` takes a cell whose `<C>head` has `fast_wal.active = Some(ActiveWal { epoch: old_ep,
bucket: old_bucket })`:

1. **Take `<C>head` (`keepOldWalInHead`):** Conditional swap bumps `head.epoch` to `e = old_ep' + 1`
   while **preserving** `head.fast_wal.active` unchanged (still naming `(old_ep, old_bucket)`).
   If `T` crashes before activation, the next taker still sees `old_ep`.
2. **Fence `old_ep` in `old_bucket` (`fenceBeforeRecover`, `fenceFollowsRoll`):**
   - List `<C>wal/<old_ep>/` to find the highest existing `cur` (or `0` if empty). Note: listing is
     only a hint; safety does not depend on list consistency.
   - Loop on `cur`:
     - Read/HEAD `<C>wal/<old_ep>/<cur>`.
     - If missing: write `Fence` at offset `0`. On `InvalidWriteOffset` / `AlreadyExists` (zombie
       created `cur`), re-read `cur` and continue. On success, fencing is done.
     - If present and ends with `Rolled`: advance `cur = cur + 1` and loop.
     - If present and ends with `Fence` (or GCS `finalized`): fencing is done.
     - Otherwise (open): on GCS Rapid, call `BidiWriteObject` takeover + `finish_write = true` (if
       zombie snuck `Rolled` in first, advance `cur + 1`); on S3 Express, append `Fence` at
       `x-amz-write-offset-bytes = len` (on `InvalidWriteOffset`, re-read and retry). Once `Fence`
       lands on a non-`Rolled` object, by `rollBeforeNext` the zombie can never open `cur + 1`.
3. **Recover and copy large extents:**
   - Read `<C>wal/<old_ep>/0..=fenced_seg` up to `Fence`, verify all MACs under the cell's DEK, and
     drop commits already covered by `head.up`.
   - For each unacknowledged large extent referenced by recovered commits, read `extents/<old_ep>.*`
     from the cold bucket (protected by `protectWalEpoch`), verify its MAC, and copy/upload it under
     epoch `e`'s cold extent key.
   - Frame the recovered commits as cold segment(s) under epoch `e` and PUT them to the cold bucket.
4. **Activate (`activateAfterFlush`):** In **one** conditional `<C>head` swap (on `head.epoch ==
   e`), acknowledge the recovered cold segment(s) into `head.end` **and** update
   `head.fast_wal.active = Some(ActiveWal { epoch: e, bucket: my_fast_bucket })`. Only after this
   CAS lands may `T` serve new commits. Notice this also seamlessly migrates a cell across AZs: `T`
   in AZ-b fences/reads AZ-a's fast bucket once during failover, flushes to the regional bucket, and
   switches hot appends to AZ-b's fast bucket.
5. **Cleanup (`sweepOnlySuperseded`, `tombstoneOnSweep`):** Once activation lands, `T` (or any
   background sweeper over the fast bucket) reclaims `<C>wal/<ep>/*` **only** when `ep < head.epoch`
   **and** `head.fast_wal.active.epoch != Some(ep)` (never by `ep < head.epoch` alone). Critically,
   reclamation **overwrites** each superseded WAL object with a 36-byte authenticated `CFTS`
   tombstone (`0 < 36 < HEADER_LEN`; refused if ever seen on an active epoch) rather than calling
   `DELETE` while the cell is live — found by TLC on the first run of `FastWalUploads`: deleting
   `<C>wal/<ep>/0` back to empty allowed a delayed zombie `x-amz-write-offset-bytes: 0` append to
   recreate the object without its fence and acknowledge a commit after takeover! The 36-byte
   tombstones are deleted only when the cell itself is deleted (or expired via a long bucket
   lifecycle rule).

## TLA+ verification (`specs/fast-wal/`)

`specs/fast-wal/FastWal.tla` models the full protocol: cold head + large extents + collector, fast
appendable objects with `MaxParts`, in-flight/delayed zombie requests, adaptive group-commit batch
sizes, segment rolling, takeover fencing/flushing/activation, WAL tombstone reclamation/sweeping,
and stateless compute-host crashes.

All 14 protocol rules have dedicated mutations under `specs/fast-wal/mutations/` verified by
`scripts/check-tla.sh` to violate their target invariant when removed:

| Rule | Mutation | Expected violation |
|---|---|---|
| `fenceBeforeRecover` | `NoFence` | `DurableSurvives` |
| `offsetChecked` | `NoOffsetCheck` | `DurableSurvives` |
| `rollBeforeNext` | `NoRollBeforeNext` | `DurableSurvives` |
| `fenceFollowsRoll` | `NoFenceFollowsRoll` | `DurableSurvives` |
| `rollBeforeLimit` | `NoRollBeforeLimit` | `FenceHasRoom` |
| `keepOldWalInHead` | `ClearWalOnTake` | `DurableSurvives` |
| `activateAfterFlush` | `ActivateBeforeFlush` | `DurableSurvives` |
| `largeBeforeWal` | `WalBeforeLarge` | `LargeExtentsPresent` |
| `protectWalEpoch` | `GcDeletesWalExtents` | `LargeExtentsPresent` |
| `deleteWalAfterAck` | `DeleteWalBeforeAck` | `DurableSurvives` |
| `sweepOnlySuperseded` | `SweepByHeadEpoch` | `DurableSurvives` |
| `tombstoneOnSweep` | `SweepDeletesFence` | `DurableSurvives` |
| `rolledTombstoneOnDelete` | `DeleteClearsRolled` | `DurableSurvives` |
| `walContiguous` | `WalSkipsUnacked` | `NoGaps` |

## Simulation (`crates/local_cell/src/sim`, board 14433)

The fast WAL runs in local_cell's simulated world beside plain and replicated cells, its production
code unchanged: each host's zone has a fast bucket, a region of the world's one `SimBucket`, whose
seed orders every request of the cold and fast buckets alike, fails appends before or after they
land, refuses an object past `FAST_PARTS` appends (the writers roll at `FAST_APPENDS`, two fewer:
room for `Rolled` and a `Fence`), and goes down at times, degrading its writers to uploading alone.
`FastWalConfig` and `FastWalOpener` take any bucket (`bucket_cell::any_bucket::AnyBucket`), and
the writer's lingers and back-offs wait on the uploader's clock. Hosts crash, keeping or losing
their disks, and take their cells back; hosts of either zone take cells over while their writers
append on, fencing and recovering the fast WAL the head names; takeovers may hold the writer taken
over's creates until something deletes what is at their keys (`SimBucket::hold`); collectors run.
After every step, every commit a fast WAL told its guest is durable must be in the history of its
cell's serving writer (`DurableSurvives`).

`fast_wal_commits_survive_takes_crashes_and_lost_hosts`, `fast_wal_takeovers_fence_writers_in_flight`
and `fast_wal_zombies_land_on_tombstones` hold; each rule the code keeps is a `bucket_cell::Rules`
switch, and a `test_needs!` case turns it off and expects a seed to find what it prevents:

| Rule | `Rules` field | Simulated case |
|---|---|---|
| `fenceBeforeRecover` | `wal_fences` | `fast_wal_takes_need_fences` |
| `offsetChecked` | `wal_offset_checked` | `fast_wal_appends_need_offsets` |
| `rollBeforeNext` | `wal_rolls_before_next` | `fast_wal_rolls_need_their_markers` |
| `fenceFollowsRoll` | `wal_fence_follows_roll` | `fast_wal_takes_need_to_follow_rolls` |
| `rollBeforeLimit` | `wal_rolls_before_limit` | `fast_wal_segments_need_room_for_fences` |
| `keepOldWalInHead` | `wal_kept_on_take` | `fast_wal_takes_need_the_old_wal_named` |
| `activateAfterFlush` | `wal_activates_after_flush` | `fast_wal_takes_need_to_flush_first` |
| `largeBeforeWal` | `wal_after_large` | `fast_wal_batches_need_their_large_extents` |
| `protectWalEpoch` | `protect_wal_epoch` | `fast_wal_collectors_need_the_wal_epoch` |
| `deleteWalAfterAck` | `wal_reclaims_after_ack` | `fast_wal_reclaims_need_acknowledgements` |
| `tombstoneOnSweep` | `wal_sweep_tombstones` | `fast_wal::tests::sweeps_need_tombstones`, scripted: its retried create in flight across a whole take and sweep, no seed of 5,000 reached |
| `rolledTombstoneOnDelete` | `wal_rolled_tombstones` | `fast_wal_reclaims_need_tombstones` |
| `walContiguous` | `wal_contiguous` | `fast_wal_degraded_writers_need_contiguous_batches` |

`sweepOnlySuperseded` (`SweepByHeadEpoch`) has no case: the code's one sweeper,
`fast_wal::sweep_superseded`, runs only after its own take activated, naming its own epoch both as
`head.epoch` and as `head.fast_wal.active.epoch`, so no epoch below `head.epoch` is ever the active
one when it sweeps, and dropping the check changes nothing the code does. It stays in the model,
for a background sweeper that sweeps cells it did not take.

A take whose `Fence` the store refuses at a length nobody changed, the segment at its appends'
limit, fails (`refuses a fence at its end`) rather than re-reading and trying again for ever.

## Frozen format change (requires explicit user approval before Rust implementation)

Per doc-28 (`AGENTS.md`, **Frozen formats**), implementing this in Rust requires one additive
version bump to one frozen family:

- **`bucket layout` (`bucket_cell::layout::FORMAT`): `31..=34` → `31..=35`**
  - Keep `v31::Head` and `v32::Head` (decoding v32, v33, and v34 heads via `into_current` setting
    `fast_wal: None`).
  - Add `Fsync::FastWal`, `Head::fast_wal: Option<FastWal>`, and `Kind::Wal` key prefix
    (`<C>wal/<epoch>/<seg>`) in version 35.
  - Append golden corpus cases under `formats/bucket-layout/35/` (never modifying `31/`, `32/`,
    `33/`, or `34/`).
- **No changes needed** to `cell directory` (`local_cell::layout::FORMAT`), `replica directory`,
  `extent frame`, `block device`, `fxfs`, `ephemeral host`, or KMS formats: compute nodes hold no
  new persistent local state for Fast WAL cells.
- **Control API (`cladefsd.proto`, additive only):** add `FSYNC_LEVEL_FAST_WAL = 3` to `FsyncLevel`
  and `optional bool fast_wal` to `CreateCellRequest` (passes `buf breaking` and `go/compat.sh`).
