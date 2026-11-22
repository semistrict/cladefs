---
id: doc-27
title: 'Writers compact, a keyless collector, and one head object'
type: specification
created_date: '2026-11-21 21:10'
updated_date: '2026-11-21 21:10'
---
# Writers compact, a keyless collector, and one head object

Design, 2026-11-21: Wave 2, track 2B, of the 2026-11-21 first-principles simplification (board
6127). Nothing here is built yet. Decisions taken without the user are in `OPEN_QUESTIONS.md`
(# Decisions, "doc-27"). The spec is `specs/bucket` (Bucket.tla, updated with this doc). The
implementation tasks are listed under Order of work.

Code is cited by file and exact text (`rg -F` finds it), as of main at 967e93b8a.

## Why

**Compaction is the collector's, and costs the most of anything it does.** The collector
(`crates/bucket_cell/src/collector.rs`, 5,987 lines; `bin/cladefs-gc.rs`) compacts an object whose
live extents take less than half its data (`compact_below`, 0.5):
- It reads the object's trailer by a suffix GET (`Trailers`,
  `let range = ByteRange::Suffix(layout::TRAILER);`) for the data's length.
- It decrypts the whole object, copies the live extents into a **moves** object
  (`<C>moves/<rest>/<pass>`, `const MOVES: &str = "moves/";`), and re-encrypts them under the cell's
  DEK.
- Moves objects are compacted in turn, up to 32 deep
  (`pub(crate) const MOVES_DEPTH: usize = 32;`).

Everything else then has to follow the moves:
- **Readers chase them.** `Fetcher::followed` loops `for _ in 0..MOVES {`, listing each object's
  moves and walking them newest first, and fails with `"moved too often"`.
- **Writers follow them.** `Writer::following` and the `Mover` callback
  (`pub type Mover = `) take moves into the host's state through `cell.moved` → `state.rs` →
  `extents.rs`'s `follow` → `kept.rs` → `Data::follow`.
- **Checkpoints follow them** (`moves_taken_first`).
- **Mirrors copy them** (`mirror.rs`).
- **The collector keeps every moves on the way from an extent's seal** (`ways_from_seals`), since a
  fenced writer's late checkpoint may name an extent further along its moves than the writer
  taking over does (TASK-175, `dead-ends/fences-naming-their-checkpoints.md`).
- **Moved objects wait out a grace period** (`Deferred`, `grace`, 60 s) for readers still at the
  old location.

Compaction is also the only reason the collector holds keys. Marking reads checkpoints and
footers, which are "authenticated, not encrypted" (`checkpoint.rs`), and the collector checks
their CRCs with `Ring::keyless`. Compacting decrypts data, so `cladefs-gc` loads a KMS client for
every tenant of every bucket (`Mode::Delete | Mode::MarkOnly => Some(kms(bucket)?),`). It probes
each tenant's KMS (`Heard`, `kms.wrap_dyn(tenant, &Secret::random()).await.is_ok()`) and records
what it couldn't compact (`Passed::uncompacted`). One process with access to every tenant's keys
is the system's largest blast radius, and it buys only compaction.

Yet the writer already has everything compaction needs:
- It holds the head and the cell DEK.
- It knows every live extent's location in memory (`Inner.state: State`, `State.extents`:
  `Location { key, offset, len }`), so it needs no trailer GETs.
- It writes checkpoints.

When the writer relocates an extent, the relocation is just a commit, which the head names like
any other. Nothing has to follow it, and no grace period is needed: an old location stays readable
as long as some state the collector keeps reads it.

**The head is N objects for GCS alone.** GCS takes about one write a second per object. TASK-690
(board 2848) already spaces a cell's head swaps to its store's interval: one swap names everything
uploaded since the last (`self.bucket.rewritable(&next).await;`). TASK-691 (board 2850) then kept
the head in `head_slots` objects (default 10, `pub const DEFAULT_HEAD_SLOTS: u64 = 10;`), version
k in slot k mod N (`let slot = next.swaps % slots.n;`), for ten swaps a second on GCS. That costs:
- N PUTs to create a cell and N GETs to open one;
- marks to fail a half-made head (`Head.failed`), deleted a day later
  (`pub const MARKS_KEPT: u64 = 24 * 60 * 60 * 1000;`), and the collector failing unmade heads
  after an hour (`pub const UNMADE_FOR: u64 = 60 * 60 * 1000;`, `Collector::unmade`);
- a spec of its own (specs/head-slots) and four rules (`slots_at_birth`, `whole_or_final`,
  `marks_fail`, `marks_kept`).

All of that buys one thing: an *Uploaded*-level `sync` loop on a GCS cell running faster than
once a second. An fsync waits only for *Ordered* (doc-12). sync, syncfs, unmount and freeze ask
for Uploaded. A replicated cell's sync waits for *Replicated*, its peers, with no swap.

## Decisions

1. **Writers compact.** A cell's writer relocates the live extents of its own cell's sparse
   segments into its open segment, in a commit of its own (a *relocation*). The head names it as
   it names any segment, and every protocol rule applies unchanged. The collector never writes an
   object.
2. **When:** at each checkpoint round (every `CHECKPOINT_EVERY = 64` segments), within a budget:
   - at most as many live bytes copied per round as the cell uploaded new bytes in it, so write
     amplification is at most 1x;
   - at most `UPLOAD_BYTES / 4` in flight;
   - nothing while the host disk has no spare (the disk governor).

   Not on a version's path, which stays one local commit (doc-18). Not on close, which keeps
   drain short.
3. **What:** segments of the cell's own prefix whose live bytes are under half their data, and
   that no kept state reads. The kept states are the versions the head names and the forks'
   origins under `<C>refs/`. The writer reads each kept point's state once per session, as a
   fork's recovery does, and keeps the set of keys it names. Relocating a pinned segment would
   only copy bytes the pin keeps anyway.
4. **The collector becomes keyless mark and sweep.** It keeps what the head's state, each version's
   state and each referenced origin's state read. It deletes the rest of the cell's prefix that no
   head may still name (the rules it has now, without moves). It reads checkpoints and footers
   keyless, as it does now, and loads no KMS client for sweeps. `--delete-cell` and
   `--rotate-master-key` keep their KMS, which they are for.
5. **One head object, swapped in place, coalesced.** The head is `<C>head`, a single object, as
   specs/bucket always modelled it. Swaps are conditional on the version the writer last saw
   (S3 `If-Match`, GCS `ifGenerationMatch`). They are spaced as TASK-690 spaces them (GCS: one a
   second per cell), each naming everything that landed since (group commit: every `Uploaded`
   waiter it covers is released by the one swap) and the checkpoint landed since, if any.
   - `head_slots` goes from the bucket's settings (`identity::Settings`), along with marks,
     `MARKS_KEPT`, `UNMADE_FOR`, `Collector::unmade`, `Found::{Unmade, Failed}` and
     `head::fail`. A head is one create, so it is never half made.
   - The four slot rules and their sim toggles go.
   - specs/head-slots stays until the code it models is gone. Deleting it then is the user's
     call (`OPEN_QUESTIONS.md`).
6. **Format.** The bucket `layout::FORMAT` goes one past whatever Wave 1 leaves it at (24 today;
   board 6017 bumps it). `Kind::Moves` and `<C>head/<slot>` are refused as `Error::Format`, and
   checkpoints gain each named object's data length (below). Nothing migrates, and no bucket
   exists to migrate.
7. **No create-only head per version.** `<C>head/<k>`, a new key per version created with
   If-None-Match, would also dodge GCS's rate limit, one write per key. It is rejected: when the
   collector removes a dead cell whole, no tombstone is left. A writer slow to swap (paused,
   partitioned) would then create `head/<k+1>` where nothing is, and bring the removed cell back.
   A swap in place is conditional on the object being there at the version seen, so it fails
   where the cell was removed. Spec rule `swapsInPlace`; mutation NoSwapsInPlace breaks
   `RemovedStayGone`. specs/head-slots's `NoSlotsAtBirth` found the same of slots created as their
   turns came.

## Writers compact

### The relocation commit

A relocation is a commit the open cell makes on its own, as a version's commit is (doc-18). The
guest asks for nothing and sees nothing: no root, no frees, no sequence numbers minted. It holds
each relocated extent's bytes again in the open segment's data. Its contents (`Contents`, in
`crates/extent_frame/src/segment.rs`) name them in `extents` at their new offsets, exactly as a
seal names an extent, and as an inherited extent a cell uploads is named today ("an inherited
extent a cell uploads goes in its data"). Recovery's `apply` puts the extent's location to the
new segment, the same path as a seal. An extent's ID still names one set of bytes, so caches keyed
by extent ID (the extent cache, the part cache's parts as read through a location) stay correct,
and the bytes are identical.

The writer reads the bytes through its fetcher (part cache, then peers, then the bucket), with
the cell's DEK, and writes them into the open segment through the open cell, as a commit of
sealed data. From then on it is an ordinary commit:
- the WAL holds it; framing, upload and acknowledgement are as for any segment;
- a replicated cell replicates it (doc-21), so members store it;
- a crash before *Local* loses it, which costs nothing (the next round redoes it), and one after
  keeps it.

No new kind of object, key or protocol step exists.

**Followers** (doc-8) apply the writer's commits. A relocation changes no extent's bytes and no
root, so a follower's view takes it as a commit that changed nothing it reads. **Forks and
versions** read immutable states that relocation never touches. **Ephemeral cells** (doc-25) are
never uploaded and keep their segments raw on the host, so they relocate nothing.

### Choosing what to relocate

The writer keeps, beside `State.extents`, each named object's **live bytes** (the sum of its
extents' lengths, updated as commits free, seal and relocate) and its **data length**. Lengths
come from:
- a checkpoint's new `lengths` (each object its state names, with its data's length);
- the trailers that recovery's tail reads of the segments after the checkpoint already fetch;
- for its own uploads, what it framed.

So computing density needs no request at all; the collector's `Trailers` probing goes.

At each checkpoint round, the candidates are the segments of the cell's own prefix such that:
- `live * 2 < len` (`COMPACT_BELOW = 0.5`, today's `compact_below`);
- the segment is not *pinned*. The pinned set is the keys named by the state at each kept point:
  the versions in the head, and the origins of the references listed under `<C>refs/` (one LIST a
  round). Each point's state is read once a session, by its checkpoint and footers, as a fork's
  recovery reads it, and cached, since points are immutable.

Candidates are taken most dead bytes first, until the round's budget is spent:
- copied bytes ≤ bytes uploaded since the last round (write amplification ≤ 1);
- ≤ `UPLOAD_BYTES / 4` in flight, so 128 MiB of the 512 MiB upload memory;
- the fetches' memory is the fetcher's, under the governor (`HeldParts`, TASK-433);
- the open segment's bytes are the cell's footprint. The disk governor admits a relocation only
  while the host disk has spare, never against the reserve: a relocation is optional, and a disk
  short of spare skips it (`dead-ends/enospc-on-dead-space.md`: never refuse a commit that frees
  space).

**Inherited extents** (a fork's, in its parent's segments) are never candidates. They live under
the parent's prefix, where the parent's reference keeps them whatever the child does.

### What happens to the old segment

Nothing, at once. It is still named, read or kept by:
- recovery from the head's checkpoint, until a checkpoint covering the relocation is in the head;
- any kept state that names it (versions, origins), until they go.

Once neither holds, the next collector pass deletes it, as it deletes any segment no state reads
(spec: ReachesRelocatedFreed reaches exactly this). A reader holding the old location from before
the acknowledgement (the writer's own fetch in flight) may find the object gone. It reads the
extent's location again from the writer's state, or the head again, as doc-7's "a reader that
finds something its head named gone reads the head again" already says.

### Cells nobody writes, and versions already made

A cell with no writer (closed, or dead but kept for its children's origins) makes no new garbage:
only commits free extents. Its sparse segments stay until a writer opens it. Since the writer
compacts every round while it writes, a cell closed cleanly has few segments under half live, so
at most about 2x space amplification remains. Versions and origins are immutable states: their
segments stay whole while they are kept, and are relocated or deleted once the head names the
version no more and no reference holds the origin.

The collector compacted kept states' segments too, by moves, when they were named but not read
by a recovery. Writers do not: rewriting a version's state would mean writing checkpoints at its
point, which keys written once forbid, and pointing its children elsewhere, which would be a moves
protocol again.

The cost of this is space for long-kept versions of churning cells. The mitigations: the writer
keeps the head's state dense, so a version made at any moment pins mostly live bytes; and the VM
manager drops old versions (`keep_last`). If the space shows up in the soak's numbers, a
`cladefsctl compact` (an operator's take, compaction, then close) is the follow-up, and board
tasks are filed from measurements, not guessed now.

*Rejected: packing before a version is made* ("pre-version packing"). It would copy live bytes,
fetching them from the bucket when they are not local, on a VM snapshot's path, which must stay
one local commit (doc-18: "Making one is local"). It would also only defer the waste: what the
version names is pinned either way. *Rejected: compacting on close.* SIGTERM's drain must finish
promptly, and the next writer compacts anyway.

## A keyless collector

What goes from `collector.rs`, `passed.rs`, `sweep.rs` and `bin/cladefs-gc.rs`:
- `Pass::compactable`, `Pass::compact`, `Pass::compacting`, `compact_object`,
  `COMPACTIONS_AT_ONCE`;
- `Trailers`, `Remembered.trailers`, `TRAILERS_AT_ONCE`;
- moves' walking in `Pass::new` (`behind`, `hops`, `MovesMaps::read_listed`), and `behind_at`,
  `keep_places`;
- `Fate::GoesAt`, `Fate::at`, `Deferred`, `Passed.deferred`, `Decided { compact_below, grace }`,
  `Options.compact_below` and `Options.grace`, along with the binary's `--compact-below` and
  `--grace` (refused as unknown, as formats change outright);
- `Collector.kms`, `Collector.heard`, `Heard`, `kms()`/`kmses()`, `Passed.uncompacted`,
  `Report.compacted`/`compacted_bytes`/`locked`/`uncompacted`, and `sweep::Options.kms`;
- the KMS load for `Mode::Delete | Mode::MarkOnly`;
- `Kind::Moves` and the collector's `collect_copy` arm for it.

What is left: list, read the record and head, recover the live cell (keyless, CRCs), keep each
version's and each referenced origin's state, retire references, remove dead cells whole, and
delete. That is `GcFinish` in the spec, which now neither reads nor writes data.

What goes elsewhere:
- **`crates/bucket_cell/src/moves.rs`**, all 147 lines.
- **In `layout.rs`:** `moves`, `moves_of`, `moves_under`, `moves_depth`, `moved_from`,
  `moves_prefix`, `further`, `struct Moves`.
- **In `fetch.rs`:** `followed`, `moved`, the `MOVES` import, `Kind::Moves` in tail decoding, and
  their tests.
- **In `writer.rs`:** `Mover`, `mover`, `following`, `take_moves`, and moves in
  `write_checkpoint`.
- **In `local_cell`:** `cell.rs`'s `moved`, and `follow` in `state.rs`, `extents.rs`, `kept.rs`
  and `layout.rs`'s `Data`.
- **`mirror.rs`'s move copying:** a mirror copies the objects a state names, nothing along moves.
- **Rules** `moves_by_pass`, `ways_from_seals`, `moves_taken_first` and `hosts_follow_moves`, and
  their sim toggles (`lc/sim/mod.rs`).

**Integrity without keys.** The collector checks CRCs, not MACs, as it does now: whoever can write
the bucket can delete from it anyway, so a forged checkpoint gains an attacker nothing they lack.
Readers that decrypt still check MACs under the DEK.

**Dead ends kept.** `collector-in-flight-declarations.md`: unchanged. Keys still declare
everything the collector needs (an object's epoch and segment), and no writer declares anything
in flight. `fences-naming-their-checkpoints.md`: the TASK-175 class is gone, not patched. A fenced
writer's late checkpoint names only extents at segments, which every recovery reads where they
are, so there are no moves whose ways it could be ahead on. The collector keeps exactly what the
states it trusts read: validity stays monotone, as that dead end requires. Spec model
BucketRelocationsForks holds on TASK-175's ground: a version forked, then the extent relocated
and checkpointed past.

## One head object, coalesced

### The swap

`head::write` swaps `<C>head` in place, conditional on the version last seen. These go: `Slots`,
`slot = next.swaps % slots.n`, `head::read`'s N GETs, `birth`'s N creates, `fail`, `found`'s
`Unmade`/`Failed`, and `Error::Unmade`. A create is one If-None-Match PUT: the record's, deciding
whose the cell is, as `lifecycle::create` does now in slot 0.

Coalescing is TASK-690's, kept:
- `Rewrites` spaces writes of one key to its store's interval (GCS: 1 s);
- the uploader keeps one head work at a time (`HeadWork`);
- each acknowledgement waits for `rewritable`, then names every segment uploaded meanwhile, every
  version they make, and every drop since (`Naming`).

Every `Uploaded` waiter at or before the last segment it names is released by that one swap
(`progress.uploaded_through(epoch)`), which is group commit.

What changes: the checkpoint's own swap (`HeadWork::Checkpointed`) folds into the next
acknowledgement. A checkpoint landed since the last swap is named by the next one. That was
TASK-690's open end ("a checkpoint's own head swap costs a commit a slot every 64 segments"), held
back as "the collector vs. an unnamed checkpoint". The collector already keeps a checkpoint newer
than the head's that its epoch may still name (`collectorEpochs`, `ckptAhead`), so naming it later
is safe. A checkpoint with nothing to acknowledge after it (an idle cell's, or a close's) is named
by a swap of its own, as now. Spec: `Ack` names a landed checkpoint; `CheckpointSwap` remains for
the idle case.

### What it costs on GCS

- A non-replicated GCS cell's Uploaded-level `sync` loop gets about one sync a second, where ten
  head slots allowed about ten. A sync wants every commit before it in the bucket, so concurrent
  syncs on one cell share a swap.
- A VM snapshot's freeze, which waits for Uploaded, waits up to a second more if the head swapped
  in the last second.
- A fork on another host waits for its version's upload (doc-18) the same way.

fsync (Ordered), Local and Replicated levels are unaffected. A GCS cell that needs fast durable
syncs is a replicated cell (doc-21), whose syncs wait for f peers' disks, not the bucket: the
recommended setup, recorded in `OPEN_QUESTIONS.md`.

Every bucket gains back the rest: creation is 1 PUT and opening is 1 GET (9 and 9 fewer at the
default), with no marks, no day-old garbage and no hour-old unmade heads. Wave 1's board 6020
already takes non-GCS buckets to one slot. This finishes the job for GCS, and the setting goes.

### Why removal and fencing stay airtight

- **Taking:** a take is a conditional swap from epoch E to E+1 of the one object, unchanged. Two
  takers race on one condition and one fails. `HeadsCurrent`, `NoAckedLost` and `DeletedUntaken`
  are unchanged in the spec.
- **Fencing a writer taken over:** its next swap's condition fails (`headSwap`, NoHeadSwap).
  Unchanged.
- **Dead cells removed whole** (TASK-455, `RemovedStayGone`): the collector deletes the head with
  the cell. A writer of the removed cell then swaps a head that isn't there: the condition fails,
  as the object is gone (If-Match on nothing, or a generation match on nothing). So the cell
  stays gone with no tombstone. A per-version create-only key would succeed instead (Decision 7;
  NoSwapsInPlace). The maker's create is If-None-Match on an allocator's ID that no other maker
  is given, as today. A create request outliving its cell's whole life (made, deleted, removed)
  was not modelled before either (specs/head-slots says so of slot 0). With one slot, the only
  create is that deciding one, so nothing is new.
- **`fences_kept`:** doc-7 names a rule `fencesKept`, a mutation `NoFencesKept` and a model
  `BucketFences`, but none of them exists in specs/bucket or the code (board 6198). Single-slot
  heads change nothing there.
- `dead-ends/reference-tombstones-in-place.md` stands: references are still retired by marks
  outside `<C>refs/`. The head never becomes a tombstone either: a deletion's head is final,
  kept, and removed only with its dead cell.

## The spec

`specs/bucket/Bucket.tla`, updated with this doc:
- **Gone:** the collector's moves (variables `moves`, `mflight`; `Ways`, `Hops`, `KP`, `Followed`,
  `Moved`, `Further`, `TakeMoves`, `CheckpointTaken`, `Behind`, `GcLand`), its compaction in
  `GcFinish`, the rules `movesByPass`, `waysFromSeals` and `movesTakenFirst`, and their mutations
  (NoMovesByPass, NoMovesTakenFirst, NoWaysFromSeals). `Readable` is now: the object at the
  location names the extent.
- **New step:** a writer's relocation (`Can`'s `"relocate"`). `Ops` offers a relocation of an
  extent of its own cell's segments, acknowledged and still live in the guest's view, made as a
  commit naming the extent in its new segment (`relocated` records where it came from).
- **New rule:** `swapsInPlace`. Without it, a swap where the head was removed creates it
  (NoSwapsInPlace: `RemovedStayGone`).
- **Changed:** `Ack` names a landed checkpoint (the folded checkpoint swap).
- **New checks:**
  - mutations/ReachesRelocatedFreed: the spec reaches a relocated-from segment deleted by a pass,
    so compaction frees space and the model is not vacuous;
  - BucketRelocationsForks: a version forked, then relocation and checkpoints, with every
    invariant holding;
  - BucketCompaction and BucketSpansCollect now relocate rather than compact.
- **Header:** the head is one object, swapped in place and coalesced. The spec is ahead of the
  code until Order of work's steps 2 and 4 land.

specs/head-slots: retired once step 4 lands (the user's OK to delete a test suite,
`OPEN_QUESTIONS.md`); until then it still models the code. specs/buckets mentions moves only as
not modelled, which needs one line changed.

`make check-tla`: every model and mutation green, the three removed mutations' rules being gone
from the protocol (the commit's `Weakens-tests:` line says so).

## Composition with Wave 1

- **Board 6017** (epoch in `Origin`/`Version`, `known.rs` deleted, FORMAT bumped) touches
  `recovery.rs`, `collector.rs`, `layout.rs` and `fetch.rs`, the same files as steps 2 and 3. Steps
  2 and 3 go after it, and step 2's FORMAT bump goes on top of it.
- **Board 6020** (`head_slots = 1` default off GCS; `resume_version_fork` and `Head::resumed`
  gone) edits `head.rs`, `identity.rs` and specs/bucket's resumes. Step 4 goes after it and
  removes what it left for GCS. Its spec edits (resumes out) and this doc's (moves out) touch
  the same lines of Bucket.tla and its configs' `Rules`. Whichever lands second merges them: the
  union of the removals.
- **Board 6023** (`RoutedBucket<B>`) moves `Rewrites`' spacing per routed member. Step 4 keeps
  it where 6023 puts it.

No step of this doc lands before 6017, 6020 and 6023.

## Order of work

Each step is a board task, unclaimed, and lands on main with the workspace suite, the bucket
simulations and `make check-tla` passing.

1. **Writers relocate** (`local_cell` uploader and cell, `bucket_cell` writer and checkpoint).
   Covers per-object live bytes and lengths in `State`, `Checkpoint::lengths`, pinned keys from
   kept points, the relocation commit, the budgets, and the disk governor's admission. Tests: the
   simulation's collector keeps everything a recovery needs with relocations on (its
   `NothingNeededGone` oracle); a sim property that a cell freeing and writing at random stays
   under 2x space; and a bucket's bytes over a run of `bench-fs-fxfs-guest-dax` stay within the
   bound. The collector still compacts too until step 2: both are safe together, as moves of a
   relocated-from object are only ever named by states that read it.
2. **Moves go** (`collector.rs`, `moves.rs`, `fetch.rs`, `writer.rs`, `mirror.rs`, `local_cell`'s
   follow, `layout.rs`, rules, FORMAT bumped). Deleting their tests needs the user's OK
   (`OPEN_QUESTIONS.md`). Needs 1, and board 6017.
3. **The collector is keyless** (`collector.rs`, `sweep.rs`, `bin/cladefs-gc.rs`). Covers the KMS
   gone from sweeps, `Heard`, `uncompacted` and `--compact-below`/`--grace`. Test: a sweep of a
   bucket of encrypted cells with no KMS configured collects everything it did before. Needs 2.
4. **One head object** (`head.rs`, `identity.rs`, `lifecycle.rs`, `collector.rs`, `layout.rs`, the
   checkpoint swap folded, FORMAT, specs/head-slots retired with the user's OK). Tests: the
   GCS-like `rewrite-every-ms=1000` fault test (TASK-690's
   `heads_are_swapped_as_fast_as_gcs_takes_them`) with one slot; a create of 1 PUT, an open of 1
   GET. Needs boards 6020 and 6023.
5. **Measure** on the test host: the bucket soak (`soak-host.sh`), `gc-sweep-host.sh`, and the
   nightly's fork cases. Expected: cladefs-gc's requests per sweep down (no trailers, no
   compaction GETs or PUTs), writers' uploaded bytes up by at most the relocations, and bucket
   bytes steady. A regression is filed and fixed.

Steps 1 and 4 are independent and may go in parallel once their Wave 1 dependencies land.
