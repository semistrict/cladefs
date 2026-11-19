---
id: TASK-633.8
title: >-
  Mirrors: a version forked into another bucket is copied there in the
  background, its children switched once the copy is whole
status: In Progress
assignee: []
created_date: '2026-11-18 17:04'
updated_date: '2026-11-19 14:16'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.7
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: medium
type: feature
ordinal: 939000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Until its version is in its own bucket, a child across buckets reads its parent's bucket cold, pays egress, and needs that bucket up for those reads. The user decided (2026-11-18): the first fork of a version into another bucket starts copying it there in the background, byte for byte at the same keys (not a flattened copy), one mirror per version and bucket, shared by its children there; children read the parent's bucket lazily until the mirror's create-only marker lands, then their own (doc-23, Mirrors).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The first fork of a version into another bucket starts its mirror there: every object of its checkpoint and their moves copied (server-side where the clouds allow, GET and PUT otherwise), then the marker; later forks find it under way or done
- [ ] #2 Children's references move to the mirror's bucket in the order the spec checks; reads of the version come from the mirror once its marker exists
- [ ] #3 A mirror interrupted by a crash or a bucket down resumes; the mirror goes when no reference to it stands
- [ ] #4 Tests: a child reads cold and forks with its parent's bucket failing after its mirror; a crash at each step of a mirror leaves nothing needed gone
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Order (each its own verified commit):
1. Layout: the mirror marker (create-only, under P's prefix, naming at and the checkpoint epoch) and the mirror's pin (a reference of its own under P's prefix); Kinds parsed, kept by no pass but the foreign-cell rule.
2. B's collector over a foreign cell's prefix (P numbers another bucket): never a dead cell; keeps a copy while P's head in A names its version or a reference in B stands, else deletes the marker first, then the objects (copiesKeptWhileNamed, copyRelists, markerGoesFirst). Before anything is ever written under a foreign prefix.
3. Reads: a key of a foreign cell read from the own bucket first when it holds a marker of that cell (keys written once: a copy equals its source), else the cell's bucket.
4. The mirror job: pin, head read in A, copy (GET+PUT create-only; server-side later), marker, list A refs after the marker and refer in B, retire+delete in A, drop pin; resumes from its pin, after the marker goes on to the listing.
5. Forks: reference in B when marked, else A; read again after referring in A (forksRecheck, forksLeaveA); the first fork into B starts the mirror in the background.
6. Tests per AC #4, and the simulation profile with mirrors (TASK-633.10).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Steps 1-4 (dcd176354, e7bdb0714, 6792b32ee, a17a5a0d4): layout (FORMAT 22: pin, marker), B's collector over a copy (collect_copy), reads served from a marked copy first, the mirror job.

Step 5: forks refer through Bucket::mirrors (mirror::Here): in B when the marker is there, else in A; having referred in A, the marker read again after P's head, and if there, the reference written in B and retired in A (forksRecheck, forksLeaveA). Uploader::forked starts ObjectBucket::mirror_wanted on the bucket runtime; a process runs one mirror of a version at a time.

Pins (beyond the spec, which models one mirror): the pin's bytes are its mirror's own; another's standing, a mirror does nothing (UnderWay). A pin unrefreshed for PIN_STALE (10 min) is stale: the mirror refreshes it each minute and right before its marker by conditional write, giving up if it is no longer its own; a stale pin keeps nothing, the next mirror takes it over; B's collector begins a stale-pinned mirror again. Lease assumption documented in doc-23 (As built).

Simplifications (doc-23): GET+PUT copies only; only P's own prefix copied; a cell's whole copy kept while any version needs it.

Tests: forks_refer_where_their_versions_are_mirrored, mirrors_hold_their_pins_alone (bucket_cell), versions_forked_on_a_host_of_another_bucket_make_children_there (cladefs, the fork starting the mirror). Hand mutants, each failing a test: no recheck, no retirement in A, never referring in B, shared pins, refresh unchecked, stale pins keeping, never stale, the fork starting no mirror.

Step 6 (aa0e3e279): a fresh host learns a mirrored version's epoch from its marker (mirror::Here::learn) before reading the parent's head; tests children_of_a_mirrored_version_live_with_its_bucket_out_of_reach and mirrors_dying_at_each_step_leave_nothing_needed_gone (AC #4).

Review round 1 (4 dimensions + verifiers, 2026-11-19). Confirmed and fixed:
- B's collector kept a copy only for a named and marked version; versions are added after its head read (outside the spec), so a mirror could skip objects a pass then deleted. Now the copy is kept while the parent's cell lives, a pin stands, or a reference here stands (copiesKeptWhileAlive).
- Children's references moved to B left nothing in A keeping what the version inherits from other cells: the parent removed, its origin reference retired. Now the mirror writes a hold in A, <P>refs/<at>/mirror.<B>, after its pin and before its head read; B's collector deletes it as the copy goes (mirrorHolds). Test holds_keep_what_mirrored_versions_inherit_from_other_cells (fails without the hold: the child's read of the inherited extent).
- A fork that rechecked the marker wrote its B reference after its head read: it now reads the head again before retiring its A reference (forksRereadBeforeLeaving).
- copy() required the extent's original object; one compacted away before the version was cut is gone. Now it copies along the moves, the earlier objects if there. Test versions_cut_after_compactions_are_mirrored_with_their_moves.
- Retired marks in B now deleted with the copy; pin released only after a conditional refresh; errors let the hold and pin go; per-version turn map cleaned on drop; a fork's mirror takes its turn as it is asked for (the cladefs test no longer races it); a child's store opening starts its version's mirror (mirror_for).
- Tests: misleading collector test rewritten; exact counts; duplicated setup now Two; sweeps_pass_over_copies_as_copies covers collect_with_own and Sweep::run_with_own; deletion order asserted (hold, marker, objects, marks).
Not fixed, documented: a fork retried by the same host can't tell it resumes (identical head bytes), so a reference it left in A stays until the child's deletion; the hold keeps as much. lists_here_too returns B's moves alone when A's listing fails (B holds the version whole).
Spec update (holds, version additions, split fork check) delegated; TLC on the Mac.

Review round 2 (3 dimensions + verifiers). Confirmed and fixed:
- B's collector retired a deleted child's B reference while the child's cell still held its own children's references (A keeps it by references_held): now a deleted child's reference stands in B too, and the child's own pass, its cell dead, retires it in B as well as in A (mirror::Here::retire; Counted forwards Bucket::mirrors). Test references_here_hold_while_their_childrens_cells_hold_references.
- A mirror giving up deleted the shared hold though its pin was taken over: it now writes its pin again first, and deletes the hold only if the pin is still its own.
- Holds without markers (a mirror that copied and never marked) leaked: B's collector deletes the holds of every version it finds in B (markers, pins, checkpoints, references, marks); a resumed marked mirror that moved nothing and finds the marker gone deletes its hold.
- Mirrors of versions no longer named come back as Mirrored::Unnamed, cached, so store openings don't retry them.
- Tests: opening_a_child_mirrors_its_version (mirror_for), mirrors_of_a_version_take_turns, mirrors_failing_let_their_hold_and_pin_go, release's guard in mirrors_hold_their_pins_alone, a marker-less hold in copies_of_other_buckets_versions_are_kept_while_needed, the mark-only sweep touching nothing in A.
Left as is: mirror_for reads the child's head on each store opening on a routed bucket (one GET); the open wiring itself has no test of its own.

Spec updated (delegated, TLC on the Mac, 24 models in 18 min): versions cut sharing objects, P deleted, the fork's check split into single-bucket steps, holds, a mirror begun again once done. Rules added: copiesKeptWhileAlive (replaces copiesKeptWhileNamed), forksRereadBeforeLeaving, mirrorHolds (justified outside the model), and two TLC found in the code with a mirror begun again:
- mirrorRereadsBeforeRetiring: refer_here retired the children's references in A without reading the head again; after B's pass read P deleted and listed B, a mirror begun again moved a forking child to B and the pass deleted the copy (NothingNeededGone). Fixed: the head read again, references retired in A only if it names the version.
- mirrorHoldsUnmarked: the hold written before the marker read, a mirror begun again rewrote it after B's pass deleted the holds (HoldsMarked, a leak keeping P's cell for good). Fixed: the hold written only once the marker is read absent.
Tests: the dying test's per-step references (a mirror resumed after P's deletion leaves the A reference), a marked re-run writing no hold; hand mutants of both fail them. Each spec mutation finds its violation; Reaches* reach; liveness holds.
<!-- SECTION:NOTES:END -->
