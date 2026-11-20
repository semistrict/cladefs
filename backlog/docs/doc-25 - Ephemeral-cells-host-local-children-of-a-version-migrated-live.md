---
id: doc-25
title: 'Ephemeral cells: host-local children of a version, migrated live'
type: specification
created_date: '2026-11-20 16:20'
updated_date: '2026-11-20 16:20'
---
# Ephemeral cells: host-local children of a version, migrated live

Design, 2026-11-20. The user's decisions are marked *(decided)*.

## Why

The sandboxes' local mode today is overlayfs inside the guest. Its lower layer is a JuiceFS
snapshot held on JuiceFS, and its upper layer is a virtio block device on the host's local NVMe.
The upper layer never leaves the host, so nothing in it costs an upload, a framing or a bucket round
trip, and losing the host loses it. cladefs has no such cell: every cell's durable copy is in the
bucket (CONTEXT.md, Cell), and its sync, syncfs and unmount wait for an upload (doc-12).

## Decisions

1. *(decided)* **An ephemeral cell** is a cell forked from a version of a cell in a bucket, kept on
   one host's disk alone, and never uploaded. Its parent's version is its lower layer, read as every
   fork reads its origin: the extent cache, shared by DAX across the host's VMs, then the part cache,
   then the bucket. Its own extents are its upper layer.
2. *(decided)* **No bucket of its own.** The store keeps an ephemeral cell's segments raw in its
   directory, with no uploader and no framing. The other design, a `file://` bucket per host disk
   holding ephemeral cells as cells homed there (doc-23's cross-bucket fork), was rejected once
   promotion was ruled out (below). It would have framed, encrypted and checkpointed data that never
   leaves the host. It would also have spent a bucket number per host, and hosts come and go.
3. *(decided)* **Never promoted.** No ephemeral cell becomes a bucket cell, and none is forked into
   one.
4. *(decided)* **No versions.** An ephemeral cell is a leaf. It has no `MakeVersion`, no forks of it,
   and no VM snapshot that includes it.
5. *(decided)* **The parent's version is kept by a reference.** The reference is in the parent's
   bucket and names the holding host's incarnation. A daemon retires its own stale references when
   it starts. **Decommissioning** a host, the VM manager's call for a host gone for good, retires every
   reference naming its incarnation. This uses no lease and assumes nothing about clocks.
6. *(decided)* **Survives what keeps the host's disk:** a daemon's restart, its crash, its upgrade, and
   the host's reboot. Losing the disk loses the cell.
7. *(decided)* **Live migration, by pre-copy.** A VM whose disk is an ephemeral cell moves to another
   host with its cell. Guest RAM is the VM manager's to move, not cladefs's.

## The cell

- **Its ID** comes from the parent's bucket's allocator, as every new cell's does, so that its
  extent IDs, extent cache keys and part cache keys stay unique. Nothing in the bucket names it but
  its reference. Its directory's description says it is ephemeral.
- **Its keys:** a cell KEK of its own, wrapped by its tenant's KEK, kept in its directory, since it
  has no head. Shredding the tenant makes it unreadable, as it does any cell. It inherits its
  parent's keys for its origin's extents, as forks do.
- **Its levels:** Ordered and Local only. `FSYNC_LEVEL_REPLICATED` and `replicas` are refused
  (`ENOTSUP`). Its sync, syncfs and unmount wait for Local, or for nothing on a device with
  `cache_type` Unsafe (TASK-363).
- **Its writes:** commits append to the open segment as they do today. A closed segment stays raw in
  the cell's directory for good. It joins no write-ahead log, takes no lag bound, and is never
  framed.
- **Its frees:** a free punches a hole (`FALLOC_FL_PUNCH_HOLE`) over the freed extent in its raw
  segment, once the commit recording the free is Local. A segment with nothing left in it is
  unlinked. Raw segments are written in whole 4 KiB XTS sectors (`extent_keys::xts::SECTOR`), so
  every extent's bytes are whole blocks and no compaction is needed. A free of an inherited extent
  is recorded and frees nothing, as a fork's is today.
- **Its footprint** counts the blocks its files have allocated (`st_blocks`), not their lengths, so
  that a punched hole gives its space back to the tenant's allotment (doc-22). Writes are admitted
  against the tenant's disk limit, as every store's are. Since nothing is ever uploaded, its
  reserve holds no copies.
- **Its recovery:** from its latest **local checkpoint**, the bucket's checkpoint format written to
  the cell's directory, then the logs of the segments after it. A drain writes a checkpoint of every
  ephemeral cell. Checkpoints are also written every N segments (to be measured), which bounds
  replay after a crash. A segment's log is removed once a checkpoint covers it; its raw data stays.

## Forking one

`ForkCell` with `ephemeral` set, from a version (`version`, `tenant`, `cell`):

1. The child's ID is taken from the parent's bucket's allocator.
2. **The reference is written** in the parent's bucket, naming the version and the host's
   incarnation, before anything reads the version. This is doc-7's `forksReferFirst`, with a key of
   its own kind, so that the collector knows no head will come for it.
3. **The parent's head is read**, and must name the version. If it does not, the reference is
   deleted and the fork fails (`NOT_FOUND`).
4. The checkpoint at the version is read, or written once if missing, as for any fork. The child's
   directory is made from it.

A fork on the version's host, before the version is uploaded, waits for the upload, as a fork on
another host does today. Forks of a base's version are the expected case, and are uploaded long
before.

**The host's incarnation:** a random number made with the daemon's state directory and kept in it,
like doc-21's replica directory's (`local_cell::replica`). It is new when the disk is lost. The
two incarnations become one if the code allows it; otherwise the replica directory's is renamed.

**Retiring references:** a daemon starting lists the references naming its incarnation in each
bucket it uses, and retires those whose cell it does not hold, for example a fork that crashed
before step 4 or a cell deleted before a crash. `DecommissionHost(incarnation)` retires every
reference naming the incarnation. The collector keeps the version while any reference stands, as
it does today.

## Live migration

Host S migrates an ephemeral cell to host T. Both are peers (doc-21's `SetPeers`), and the
VM manager drives both (`MigrateCell` on S, naming T).

1. **T's reference first.** S has T write a reference naming T's incarnation, before anything is
   sent. While both stand, either host's death keeps the version for the other.
2. **Bulk copy.** S sends its closed raw segments to T, sparse, keeping holes, through the peer
   transport.
3. **Rounds.** A replicator (`local_cell::replicate`) streams S's log to T as a one-member
   configuration of f = 0 that never tells anything replicated. A round sends the segments closed
   since the last one, the open segment's log, and the frees, which T applies as punched holes.
   Sealed extents never change, so a round sends only what was written since the last one.
4. **Hand over.** The VM manager freezes and pauses the guest. S makes one commit and sends the
   rest of the log. S writes a local checkpoint as of that commit and sends it, then **gives the
   cell up**: it closes the cell's store and refuses its guest's requests from then on. Only then does S tell T the commit it ended at. T checks
   it holds the log through that commit, recovers the cell from the checkpoint, and opens it as its
   writer. The guest resumes on T, its extent device connected to T's daemon.
5. **After.** S deletes its directory, then its reference. A crash of S at any point leaves T's
   reference, and S's own reference is retired when S starts again or is decommissioned.

**One writer:** S gives the cell up before T can open, and T opens only for the commit S named. A hand-over that
fails before S gives the cell up leaves S the writer. T deletes what it received, and its reference, so that S
can try again. A hand-over that fails after S gives it up leaves T the writer if T holds the log through
the named commit. Otherwise the cell is lost, as it would be if S had lost its disk.

**Downtime** is the freeze, one commit, the last of the log, and one checkpoint, with no bucket
request. The parent's pages on T come cold from T's part cache or the bucket. Pre-copy may warm T's
extent cache with the pages S's window maps (later).

## The spec

`specs/ephemeral` (TLA+, every model under ten minutes): the parent's version, its dropping, the
collector's passes, ephemeral forks crashing at each step, daemon restarts retiring references,
decommissioning, and migration with S or T dying at each step.

- **Invariants:** every live ephemeral cell's version is whole in its bucket (`NothingNeededGone`); at
  most one host is a cell's writer (`OneWriter`); a writer on T holds every commit S told its guest
  was Local (`HandOverLosesNothing`).
- **Rules, each with a mutation TLC finds:** `forksReferFirst`, `targetRefersFirst`,
  `sourceGivesUpFirst`, `sourceRetiresLast`, `retireOnlyUnheld`.

## Order of work

1. The spec.
2. The store: an ephemeral store mode in `local_cell` (raw segments kept, frees punched, footprint by
   allocated blocks, local checkpoints, recovery from them).
3. Forks: `ForkCell`'s `ephemeral`, the host's incarnation, the references and their retiring,
   `DecommissionHost`; levels refused.
4. Live migration: `MigrateCell`, the bulk copy, the rounds, the hand-over; a guest case moving a
   running VM's disk between two daemons, with its downtime measured on the test host.
5. The full-stack simulation (doc-24): ephemeral forks, restarts and migrations under faults.
6. CONTEXT.md, docs/architecture.md, the proto's docs.
