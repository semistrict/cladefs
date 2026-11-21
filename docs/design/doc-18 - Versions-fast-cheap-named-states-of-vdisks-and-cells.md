---
id: doc-18
title: 'Versions: fast, cheap named states of vdisks and cells'
type: specification
created_date: '2026-11-16 13:05'
updated_date: '2026-11-18 02:23'
---
# Versions: fast, cheap named states of vdisks and cells

Design, 2026-11-16 (TASK-476). `specs/bucket` checked it before coding (The model, below).
extent_bucket's simulation checks the code as it is written. *Implemented 2026-11-16 in
extent_bucket and extent_store* (the bucket's protocol, the store's cuts, copies and restores,
the simulation's `sim::versions_*` tests); cladefs's interfaces and the whole-cell cut next. The user's decisions are marked *(decided)*: those of 2026-11-15
in TASK-476's description, and that of 2026-11-16 on where a snapshot is recorded. doc-7 says where
this fits in the bucket's protocol (Versions).

## Why

A VM's snapshot today is a fork of its vdisk into a point, each point a branch of its own in a new
cell (doc-7, Points). Taking one costs the following, all with the VM paused (TASK-452,
docs/measurements/macos-bucket-lifecycle-2026-11-15; code paths in `extents.rs`'s `point`,
`host_store::fork_in`, `upload.rs`'s `Remote::forked`):

- **The upload backlog, whole.** `Remote::forked` asks the uploader to upload everything up to the
  fork's segment and waits for its acknowledgement, in order. That is up to the lag bound, 1 GiB
  of WAL or 4,096 commits, with the VM paused all the while.
- **20 requests in 18 round trips** after that: 0.57 s at 30 ms a request, 1.8 s at 100 ms. Among
  them, the checkpoint at the fork point, since the point is in another cell
  (`recovery::checkpoint_at`). The writer then writes the same checkpoint again, the bucket refuses
  it, and the writer reads it back to compare. That is 1.7 MB up and 0.9 MB down at 262,144
  extents, for a fork that wrote 584 bytes.
- **Deleting one** costs 8 requests in 8 round trips. Its parent's state keeps naming the fork
  until a checkpoint of the parent drops it, at most once every 64 segments, reading 32 children's
  records at a time. The point's record is never removed, and every pass of its cell reads it.

The user wants snapshots of vdisks and cells that are extremely fast and cheap, taken often and
pruned to a rolling window *(decided, 2026-11-15)*.

## Decisions

1. *(decided)* **Local-first.** A snapshot returns once its cut is local, synced to the host's disk. The
   version is **local** until the bucket acknowledges it, and **uploaded** after that. A host
   that crashes keeping its disk keeps its local versions, as it keeps its commits. A host that
   loses its disk loses them, as it loses commits not yet uploaded.
2. *(decided)* **A vdisk's snapshots are in its own cell**, as versions of it: no new cell, no
   checkpoint across cells, no reference at snapshot time. The first restore into another cell
   writes the checkpoint at the version, once for every restore after it.
3. *(decided, 2026-11-16)* **The vdisk's head names its versions.** No record is made per
   version. The writer swaps the head at every acknowledgement anyway (doc-7, The head), so a
   version costs nothing more in the bucket than the segment its cut closes. Under the other way
   offered, a record per version, records stay for good (rule `recordsKept`: a slow writer would
   make a deleted one again), and every pass over the cell reads each record ever made. A version
   belongs to its vdisk: deleting the vdisk is refused while it has versions, unless the
   deletion is asked to delete them too.
4. *(decided)* **Whole-cell cuts in one process**, cladefsd (TASK-475), which holds every vdisk of
   the cells it serves. The cut pauses each vdisk's commits, in order of the vdisks' names, cuts
   a version of each, and lets them go. Once every member is acknowledged, one object names the
   cut: the cell's object, updated by its holder.
5. *(decided)* **Frequent, few kept.** Taking and deleting a version must be cheap, and the
   collector must reclaim a deleted version's data at its next pass over the cell.

## A version

A version is a name for a vdisk's state at a moment: a branch's state as of a segment its head
names, its versions' **at**, kept for as long as the head names the version.

- **Its ID** is random, 64 bits, unique to its making, as owners are (doc-7, Forks). A version's
  `at` is known only once it is acknowledged: a WAL put after the branch's end by `Wal::rebase`
  renumbers the segments it restamps. The ID names it from the start. A version is named
  `TENANT/CELL/BRANCH/ID`, with the ID in base 32 (`base32_id`, TASK-504).
- **The cut** is a commit of the vdisk's that closes its segment, as a fork's does (doc-7,
  Forks). The commit seals nothing: the store must be quiescent, with no extent written since its
  last commit (`ExtentStore::is_quiescent`), as for a fork today. Its footer names the version
  (`Contents::versions`, `[(id, cut)]`, `cut` the ID of the cell's cut it is part of, if any), and
  so does its record in the segment's log. So the version survives the host's crash with its
  commit.
- **The head** (`layout::Head::versions`) names each acknowledged version: its ID, `at`, its cut,
  and whether it is shared (doc-7, Shared points). The acknowledgement whose swap names a
  segment names the versions in that segment's footer, in the same swap. A takeover's swap names
  what the head named, its versions among them. A deletion's swap to the final epoch names none.
- **At most 256 versions** a vdisk (`MAX_VERSIONS`). The head stays small: 256 entries of 25
  bytes are 6.4 KB, which every acknowledgement writes. A snapshot past the bound fails
  (`EMLINK`) unless it is asked to drop the oldest (below).

FORMAT 14: a footer's `versions`, and a head's. Nothing reads format 13 (no buckets kept,
2026-11-14).

## Taking a version

A vdisk's snapshot (VM process `SNAPSHOT`; control API `Snapshot`) runs on its host, the store
quiescent and its VM paused by its manager (`virtio_extent`'s `Device::take_snapshot` checks the
guest's filesystem is frozen, as it does now):

1. **The cut**, `Store::commit_version(id, cut)`, under the store's commit lock: the commit that
   closes the segment and names the version, then the flush that makes it local.
2. **Done.** The call returns the version's ID, local. The VM resumes. Its pause covered the
   freeze, the cut's commit and one local flush, and no request to the bucket (TASK-476 AC #2).

The uploader uploads the segment in its turn and acknowledges it, and the swap names the
version: **two requests**, the segment's create and the head's swap, neither growing with the
vdisk (AC #3). An ordinary commit costs the same two. A cut with no write since the last commit
closes a segment with only a footer: about 300 bytes.

**Dropping old versions as new ones come.** A snapshot may name a window, `keep_last N`: the
acknowledgement that names the new version drops the oldest past N, in the same swap. A rolling
window costs no request of its own.

**Waiting for it uploaded.** `WaitVersion` waits until the acknowledgement names it
(`Remote::uploaded` of the version's commit). `ListVersions` reports each version of a store, local or uploaded. A
host taking over from a host that lost its disk sees neither: what that host had local is lost.

## Deleting a version

**By its writer:** a swap of the head that names the version no more (`Writer::drop_versions`), one
request, conditional as every swap is (rule `headSwap`). A host holding the vdisk does it at
once, between acknowledgements. A host holding no writer of the vdisk takes it first, with the
cell's token as any taking does: a read of the head and a swap, then the drop, without recovering
the branch. It writes nothing else. A local version is dropped from the store's log: its
acknowledgement will not name it.

**Deleting a vdisk** with versions is refused (`ENOTEMPTY`) unless asked to delete its versions
too. Its deletion's final head then names none, and every restore being made of one of them fails
at its check (below).

**What frees its data:** the cell's next pass, which recovers the vdisk from its head and keeps no
state for a version the head no longer names (Collection). A fork made of it holds what it needs
by its reference, as now.

## Restoring a version

A restore of version V of branch S, at `at`, into C, a new branch of cell C' (control API
`RestoreVersion`; the VM process restoring its device): the steps of a fork of a point (doc-7,
Points), with the point's record replaced by S's head.

1. **C's record**, created as being made (`Kind::Forking { origin: (S, at), version: V }`). A
   record there refuses the restore, as for a point's fork.
2. **C's reference** under S's cell, `refs/S/at/<C'>.<C>`. At once with it, C's cell held for the
   host, as now (doc-7, What a host knows).
3. **S's head, read**: it must name V at `at`. If it names V no more, or is S's deletion's, the
   restore fails, and C's record is written deleted. If C' is of another tenant than S's cell, V
   must be shared (`EACCES` otherwise).
4. **The checkpoint at the version**, if C' is another cell than S's. One GET of the key `checkpoints/S/<epoch>.<at>`. If none is there, S's state at `at` is
   recovered (the newest checkpoint up to `at`, then the segments after it to `at`) and created
   there (`recovery::checkpoint_at`, as a fork to another cell does today). Only the first restore
   into another cell writes it. C's recovery, anywhere, reads its parent across cells by this key
   alone (doc-7, For the code): the collector lists no other cell.
5. **C's record**, updated from the version its create left: made. Only then does the restore
   succeed.

A host dying at any step leaves C's record being made, which opening C resumes (steps 2 to 5,
`branch::resume_point_fork`), or which the collector gives up on once S's head names V no more
(rule `collectorAbandons`).

A restore of a local version waits for it to be uploaded. Restores on another host find nothing
until the version is uploaded. The VM manager waits (`WaitVersion`) before it sends a restore
elsewhere.

**What it costs:** 5 requests in 4 round trips, the steps of a point's fork, with S's head in
place of the point's record. Add, for the first restore into another cell, the checkpoint's GET,
the recovery's reads (a LIST of S's checkpoints, the newest's GET, the footers up to `at`, 32 at
once) and its create. C's first open costs what a point's fork's does: 3 requests in 2 round
trips.

**The checkpoint and S's writer.** S's writer may later write its own checkpoint at the same key,
if the version's segment is its last acknowledged one. Its create finds the restore's there and
names that one in the head (`Writer::write_checkpoint`: "it stands, at its key, written once").
The restore's names every extent where it was sealed, no further along its moves than the writer
names it, which is what the collector relies on (doc-7, TASK-175).

## Collection

A pass over a cell recovers each live branch from its head (doc-7, A pass over a cell). For each
version that head names, it keeps the branch's **state at the version** whole, as for a fork point
(rule `versionsKept`, the model's `forkPointsKept` applied to versions): the extents it names, the
objects and moves holding them, and what recovering it reads (the newest checkpoint up to `at`, and
the segments from it to `at`). The pass's listing of the cell gives the checkpoints, and the head
gives the segments' keys: no request beyond those a pass makes now, but the footers it reads.

- **Cost.** A branch's versions are states of one history, recovered in one walk
  (`recovery::recover_ats`, as `collector::keep_points` recovers a branch's fork points): those
  sharing their newest checkpoint up to them from it at once, each footer read once a pass however
  many versions. The walk keeps the first state whole and, of each after it, the extents the
  footers since named, where it names them: the places a pass keeps of them all, in what changed
  of them, not in a copy of every extent a version. Plus one GET a version, its checkpoint at its
  key, which a restore into another cell writes. Measured by `versions-pass-bench`
  (cladefs-examples), a dry run over a vdisk of 4,096 extents, checkpointed every 64 segments,
  each version after a commit of 16 extents: at 256 versions (513 segments), 8,232 ranged GETs of
  footers, 547 GETs and 84.7 ms when each version was recovered alone; 515, 262 and 9.9 ms in one
  walk. Pinned: `collector::tests::passes_read_a_vdisks_footers_once_for_all_its_versions`.
- **Prompt reclaim.** A version dropped from the head is kept by no pass after the swap. Its data
  goes in that pass, unless a fork made of it holds it by its reference, or a later version of the
  vdisk still names it.
- **Races**, as the model checks them:
  - A restore that read V named wrote its reference before V's drop. A pass whose head shows V
    dropped read it after the drop, and lists the references after the states (rule
    `referencesRelisted`). It finds the reference.
  - A pass that read a head before V was named keeps the branch's state as of that head. V's
    state holds only that state and what segments after it sealed. The pass keeps those segments
    if they are of the head's epoch, at or past its end (rule `collectorEpochs`). It never lists
    those written after its listing (rule `candidatesFirst`).
- **Checkpoints kept.** Each version keeps the newest checkpoint up to it: at most 257 checkpoints
  of a branch, and in practice a few, since versions taken often share one.

## Whole-cell cuts

> **Superseded** (2026-11-17) by doc-20, "A cell has one kind": a cell holds one thing, so a version is always the whole cell's, one local commit; a VM snapshot pauses the VM and makes a version of each of its cells, with nothing to coordinate.

The daemon holds every vdisk of a cell it serves (TASK-475): VMs' and the FUSE mount's alike
(TASK-475.3, `Entry::Mounted`).

- **The daemon's vdisks of a cell** (`cladefsd::stores::cell(tenant, cell)`, TASK-476 AC #5). It
  lists every store of the cell the daemon holds, VM-served or mounted, each with a handle
  (`Vdisk`): its name and kind in the cell, its store, and `pause()`. That returns a guard under
  which the store takes no write and makes no commit: they wait. Requests in progress end first.
  The guard does not wait for a commit; it reports whether the store is quiescent.
- **Quiescence.** A vdisk of a VM is quiescent only if its VM manager paused the VM and its guest's
  filesystem is frozen, as for a vdisk's snapshot. A cut finding it otherwise refuses (`EBUSY`).
  A mounted vdisk is made quiescent by the daemon: its Fxfs synced, then paused. Nothing writes
  to it during the cut.
- **The cut** (control API `SnapshotCell`): the guards taken in order of the vdisks' names, so that
  two cuts of one cell never wait on each other in a ring. Then each vdisk's version, all with
  the cut's ID. Then their flushes, at once. Then the guards go. No commit of any vdisk comes
  between the first vdisk's version and the last's: the cut holds one moment of all of them
  (AC #4, a test committing to two vdisks concurrently).
- **The cell's object names the cut** once every member is uploaded (`cell::Object::cuts`): its ID,
  and each vdisk's name, branch and version. The holder's conditional update writes it, one
  request (rule-free: the cell's object is the holder's alone, doc-7 Cells). Until then the cut is
  local, as its versions are. A holder that dies before writing it leaves versions of a cut no
  object names: restorable one by one, and dropped by the vdisks' windows in time.
- **Dropping a cut:** the cell's object updated without it, then each member version dropped. A
  `keep_last` window on `SnapshotCell` drops whole cuts.
- **Restoring a cut** into a new cell, each vdisk a restore of its member version, the new cell's
  object naming each child by its vdisk's name and kind: TASK-364.1, which builds on this.

specs/bucket does not model cells' objects (doc-7, What a host knows). A cut's members are
versions, which it does model. Whether the cell's object names a cut changes nothing that the
collector or a restore of a version relies on. The cut's single moment is the daemon's work, done
in one process, which the test of AC #4 shows.

## The model (specs/bucket)

Changes to Bucket.tla, in one place each:

- **The head** gains `vers`, a set of `[id, at]`. `HeadOf`, a takeover's swap (`Open`) and
  `Started` carry it, and `Ended` empties it.
- **A commit of kind `"version"`** (in `Can`) cuts a version: a segment whose footer names
  `version |-> id`. `Ack` adds the versions of the segments it names to the head. A new writer step,
  `DropVersion`, swaps the head without one, under the condition `Taking` (or stops).
- **A restore of a version** (`StartRestoreVersion`, `"restoreVersion"` in `Can`): `q.p` is
  `<<S, id>>`. The `"check"` step reads `head[S]`, not a point's record. The checkpoint at the
  version is the model's own: `StateAt` lists S's checkpoints up to `at`, and a late one at `at` is
  what a writer's may be. It needs no step (doc-7, For the code).
- **The collector** keeps `ForkPoint(b, v.at)` for each `v` its recovery of `b` finds in the head
  (`RecoverAll`), and gives up on a fork being made whose version S's head names no more.
- **Invariants:** `NothingNeededGone` and `HistoriesOwn` apply to each version a live branch's head
  names: its state recovers, every extent it names reads back, and its history is its branch's
  history up to `at`. New: `VersionsStay`, a version acknowledged stays named until its writer
  drops it or its branch is deleted.
- **Rules**, each with a mutation in `specs/bucket/mutations` that must find the violation:
  - `versionsKept` (`NoVersionsKept`): without it, a pass keeps only the branch's current state, and
    deletes what a version needs (`NothingNeededGone`).
  - `versionsCarried` (`NoVersionsCarried`): without it, a takeover's swap names no versions
    (`VersionsStay`).
  - `versionsAcked` (`NoVersionsAcked`): without it, a head names a version as its commit is made,
    before its segment lands. A restore then forks from a state the head doesn't name
    (`NothingNeededGone`, for the fork).
  - The restore's ordering reuses `forksRecordFirst`, `forksReferFirst`, `forksCheckPoints`,
    `forksPending` and `collectorAbandons`. Mutations of three of them for versions must find
    their violations too: `NoForksCheckVersions` (the version dropped, a pass deletes what the
    fork needs before its reference lands), `NoForksReferFirstVersions` (`NothingNeededGone`)
    and `NoCollectorAbandonsVersions` (`NoErrors`: a pass fails on a fork being made of a version
    dropped).
- **Models**, checked 2026-11-16 on the Mac (`scripts/check-tla.sh`):
  - `BucketVersions`: versions cut, acknowledged, checkpointed past, dropped and restored into
    another cell, the branch deleted, two passes. 1,988,678 states, 56 s.
  - `BucketVersionsTakeovers`: two hosts; versions cut, their segments restamped by a WAL put after
    the branch's end or lost with a disk, taken over, dropped and collected. 1,620,852 states, 34 s.
  - `BucketCellsVersions`: two forks of a version into a cell each, one resumed, the version
    dropped and its branch deleted meanwhile, two passes overlapping. 499,990 states, 18 s.
  - Each of the six mutations finds its violation in 3 s or less.

## The simulation (extent_bucket)

Each rule above is a field of `extent_bucket::Rules`, with its doc line naming its TLA+ mutation.
The simulation's tests (extent_store's, profile `sim`) cover versions cut, acknowledged across
takeovers, dropped, restored into another cell, and collected, under faults. Each rule has a test
that fails without it, and a hand mutant in `crates/extent_bucket/mutants` that the simulation
kills (`mutants.sh`).

## For the code, beyond the model

- **A restore into another cell, being made, before its checkpoint.** C's recovery reads S across
  cells by the checkpoint at the version's key alone, which the restore writes after C's reference
  (step 4). A pass over C's cell in between finds it lost. C holds nothing yet, nothing relies on
  it, and its reference keeps what it would inherit in S's cell: the pass takes it as lost nothing
  and decides again next time (`collector::lost_is_ok`). The simulation found it (seed 25 of
  `sim::versions_cut_dropped_and_restored`). A point's fork never meets it: the point's recording
  writes the checkpoint before any fork.
- **Dropping a local version** puts its ID in the writer's set of dropped versions, which no
  acknowledgement names, and the store forgets it. A host crashing before the acknowledgement
  forgets the drop: the next writer, putting the log after the branch's end, names the version
  again. A drop asked again then drops it.
- **A version's copy** is a store, read-only, its description naming the version
  (`StoreFile::version`, store format 13): opened as a point's copy is, its state recovered at the
  version (`recovery::recover_version`), its forks restores of the version
  (`Remote::forked`, `branch::fork_version_with`), sealing the checkpoint under its keys, its
  branch's.
- **Formats.** Bucket format 14 (`layout::FORMAT`): heads name versions, footers the versions cut
  at their ends, and a fork being made says whether it forks a point or a version
  (`record::Forked`). Store format 12: logs' records hold footers' contents, versions among them.

## Interfaces

- **VM process protocol** (doc-4): `SNAPSHOT { id, keep_last }` on a quiescent store, answering once
  the cut is local; `VERSIONS` (the store's, local or uploaded). `virtio_extent`'s
  `Device::take_snapshot` makes a version, and the device's saved state names it (branch, version)
  in place of a point's path and branch. A restore forks from the version.
- **Control API** (`cladefsd.proto`): `Snapshot`, `ListVersions`, `WaitVersion`, `DeleteVersion`,
  `ShareVersion`, `RestoreVersion`; `SnapshotCell`, `ListCuts`, `DeleteCut`. `DeleteStore` gains
  `with_versions`. `cladefsctl` and the Go client follow.
- **Points stay** for what outlives its vdisk: a base image's point, shared across tenants
  (`ForkStore`'s `point`, doc-7 Points). Whether versions should replace them is a follow-up.

## Order of work

1. specs/bucket: versions, rules, mutations, models; TLC passes on each model, and each mutation
   finds its violation. doc-7's Versions section, from this.
2. extent_frame and extent_bucket: `Contents::versions`, `Head::versions`, FORMAT 14; the writer's
   acknowledgement, takeover and deletion; `drop_versions`; restores of versions; the collector;
   the rules, their simulation tests and mutants.
3. extent_store: `Store::commit_version`, versions in the log, rebase, local versions.
4. cladefs: the VM process's `SNAPSHOT`, the device's snapshot, the control API, `cladefsctl`, Go.
   `bucket_lifecycle_by_phase` measures a snapshot and its restore; a pinned test of their
   requests (AC #3). The pause measured on ubu1 (AC #2).
5. cladefsd: the daemon's vdisks of a cell and their pause (AC #5); `SnapshotCell` and the
   cell's cuts; the test of AC #4.

## Not in this design

- **An instant restore of a local version on its host.** Before the version is acknowledged, a
  restore would need the fork to start from the source's write-ahead log, which a local lineage
  between stores would have to provide. TASK-165.7 removed local lineage. Here a restore of a
  local version waits for it to be uploaded. That takes the upload backlog, up to the lag bound.
  The VM no longer waits for it, but a restore asked right after the snapshot does.
- **Versions replacing points.** One mechanism would serve bases too, but a base's versions would
  then go with its vdisk.
- **A restore into the version's own cell**, a vdisk rolled back in place. Every restore makes a
  new cell today.
