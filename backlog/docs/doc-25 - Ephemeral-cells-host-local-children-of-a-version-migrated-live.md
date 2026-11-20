---
id: doc-25
title: 'Ephemeral cells: host-local children of a version, migrated live'
type: specification
created_date: '2026-11-20 16:20'
updated_date: '2026-11-20 19:38'
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
  its reference. Its store's description says it is ephemeral (`ephemeral yes`).
- **Its keys:** what a fork's head would get (`keys::Inheriting::given`): a cell KEK of its own,
  wrapped by its tenant's KEK, its first DEK and the DEKs it inherits, rewrapped under its KEK. They
  are kept wrapped in its base, and opening it unwraps them through the host's KMS, as a fork's
  head would be (`upload::open_ephemeral`). Shredding the tenant makes it unreadable, as it does any
  cell.
- **Its levels:** Ordered and Local only. It has no uploader, so its sync, syncfs and unmount wait
  for Local (`OpenCell` without an uploader), or for nothing on a device with `cache_type` Unsafe
  (TASK-363). Versions and replicas need an uploader and are refused.
- **Its writes:** commits append to the open segment as they do today. A closed segment stays raw
  (`Phase::Raw`): read from its raw file, its log beside it, never framed nor looked at ahead, never
  in a write-ahead log an uploader takes. Its extents are all in segments: none is stored on its
  own (`large_extent` is unbounded for it).
- **Its base**, `base`: its state as of the last segment a checkpoint covered, over its parent's
  version: its own extents, where they are in its kept segments, keyed by segment number at epoch
  0 (recovery stamps footers anew, so their chain's numbers can't key runs); the parent's extents it
  freed; its root, sequence numbers, spent set and capacity; the headers of its kept segments' logs,
  which say how their raw files are encrypted; the runs its last checkpoint gave back; and its keys.
  It holds none of the parent's map, which the opener gives (`EphemeralOpening::parent`, recovered
  with `recovery::recover_version` under the child's ring and shared as the version's layer).
- **A checkpoint** covers the closed segments whose commits are local, once eight wait
  (`CHECKPOINT_SEGMENTS`), or all of them as the store closes. In order: their footers applied to
  the base; their raw files synced, since a flush that sealed little made their runs durable in
  their logs alone (`LogRecord`'s held runs); the base written whole; each raw file linked into
  `kept/` and that directory synced; then their old names and logs removed and those directories
  synced; then the write-ahead log's start moved past them. Linking before removing matters because
  a crash may keep each directory's entries apart. The next opening finishes a checkpoint a crash
  cut short (`settle`) before the write-ahead log is recovered, which would otherwise remove a
  segment the start covers, taking it for one the bucket acknowledged.
- **Its frees:** a free of a sealed extent waits, as for an uploader, until a checkpoint covers the
  commit that freed it (`release_kept`), then for views to let go (`trim_kept`). Letting go of a run
  in a kept segment queues it to give back (`Segments::take_punches`), and the flush or checkpoint
  after punches it (`File::punch`: `FALLOC_FL_PUNCH_HOLE` on Linux, `F_PUNCHHOLE` on macOS). Raw
  segments are written in whole 4 KiB XTS sectors, so every run is whole blocks and no compaction
  is needed. A kept segment with nothing left in it is unlinked. The runs a checkpoint gave back are
  also written in the base, and the next opening punches them again: punching is idempotent, and
  a crash may have cut it short.
- **Its footprint** counts a kept segment by the blocks it has allocated (`File::allocated`,
  `st_blocks`), so that a punched hole gives its space back to the tenant's allotment (doc-22).
- **Its recovery:** settle, then recover the write-ahead log as any store does. The runs that the
  raw segments' logs held are written back into their raw files (`Recovered::restore_raw`), the
  base is applied over the parent's version, the recovered footers are applied over that, and the
  runs the last checkpoint gave back are punched again. Recovery reads only the raw segments after
  the base, at most a checkpoint's worth.

## Forking one

`ForkCell` with `ephemeral` set, from a version (`version`, `tenant`, `cell`), on a daemon with
`CLADEFSD_EPHEMERAL`:

1. The child's ID is taken from the parent's bucket's allocator, and its keys from the host's pool
   (`Uploader::inheriting`).
2. **The host notes the child** in its journal, durably: its origin, its store's path and its
   parent's bucket.
3. **The reference is written** in the parent's bucket, its index entry first, before anything reads
   the version (`lifecycle::fork_ephemeral`, `forksReferFirst`). The reference sits among the
   version's children's references, `<P>refs/<at>/ephemeral.<INC>.<T>.<C>`, so the parent's passes
   keep the origin's state for it as for theirs. No pass retires it, because no record of the
   child's is ever in a bucket. The index entry, `ephemeral/<INC>/<PT>.<PC>.<AT>.<T>.<C>` at the
   bucket's root, is what decommissioning lists. It is written before the reference and deleted
   after it.
4. **The parent's head is read**, and must name the version (`ENOENT`), shared if the child is
   another tenant's (`EACCES`). If it doesn't, the reference and index entry are deleted.
5. The child's store is made: its base is the version's state, with its keys.

A fork that fails leaves nothing: its directory is removed, its reference retired, and its note
forgotten. `DeleteCell` of an ephemeral store retires its reference, then forgets its note.

**The host's incarnation:** a random number made with `CLADEFSD_EPHEMERAL`'s directory and kept in
it (`incarnation`), on the disk the ephemeral cells' stores are on. It is new when that disk is
lost. It is separate from doc-21's replica directory's, which a host may not have.

**Retiring references:** a daemon starting goes through its journal before it serves, and retires
the reference of each entry whose store is not there as an ephemeral cell's of its cell: a fork
that crashed after its reference, or a deletion cut short. `DecommissionHost(incarnation)` lists
the incarnation's index entries and retires each reference.

## Live migration

Host S migrates an ephemeral cell to host T, both daemons with `CLADEFSD_EPHEMERAL` and peer
addresses (`CLADEFSD_PEER_ADDRESS`). Since everything an ephemeral store holds past its base is
files, and its kept segments never change once kept but for holes, a migration copies its store's
files rather than streaming its log (`cladefsd::migration`):

1. **Prepare** (`MigrateCell` on S, naming T's peer address and the store's path on T): T notes the
   cell in its journal as being taken by the migration, writes its own reference before anything is
   sent (`targetRefersFirst`), makes a staging directory beside the path, and answers its
   incarnation.
2. **Rounds**, every 500 ms while the guest runs: S sends the kept segments it has not sent yet,
   whole and sparse (`SEEK_DATA`/`SEEK_HOLE`).
3. **Hand-over** (`HandOverCell` on S): refused while anything has the store open (`EBUSY`), so the
   VM manager stops the VM process first. S then writes a `migrating` marker in the store, durably,
   and from then on refuses to open it (`sourceGivesUpFirst`). It sends the rest: every directory
   (the empty ones too, which opening lists), the files the rounds did not send, whole, and the holes
   the kept segments already sent have gained since. Then it asks T to take over, naming every file
   with its length and the CRC32C of each one that is not a kept segment.
4. **T's answer:** T accepts only if its staging directory holds every named file at that length and
   CRC (`openAtNamed`). It then syncs the staging tree, renames it into place, records the migration
   in `accepted/` for good (`remembersAccepted`), and notes the cell as its own. If the check fails,
   T removes the staging directory and retires its reference, and any later ask gets no
   (`refuseFinal`). A migration T never prepared, or one naming another incarnation of T's, also
   gets no.
5. **S's response:** accepted, S removes its store, then retires its own reference
   (`sourceRetiresLast`). Refused, it removes the marker and serves the cell again (`EAGAIN`). Not
   answered (`ENOTCONN`), the marker stays and the store stays unopened; `HandOverCell` asked
   again, after a restart too, asks T again, which answers as it did. T restarted mid-migration
   gives the migration up as it starts (its journal entry is a migration's), retiring its reference.

**One writer:** S gives the cell up before T can open it, and T opens only the store S named in full.

**Downtime** is the guest's stop, plus copying the raw segments since the last checkpoint (at most
eight of 32 MiB) and the logs, plus T's check and rename. No bucket request is made meanwhile, but
T's preparation wrote its reference earlier. T reads the parent's pages cold. While a store is
open, its ring is held among the host's keys (`RingHold`), which T's fetchers read inherited
extents with.

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
