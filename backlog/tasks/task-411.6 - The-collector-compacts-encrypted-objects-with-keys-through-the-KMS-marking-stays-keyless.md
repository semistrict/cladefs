---
id: TASK-411.6
title: >-
  The collector compacts encrypted objects with keys through the KMS; marking
  stays keyless
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 23:45'
labels: []
dependencies:
  - TASK-411.3
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 803000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
collector::compact_object copies extent ranges out of decompressed data, so it must decrypt and re-encrypt under the source branch's DEK with a new salt. The user chose giving the collector KMS access over part-granular keyless compaction (doc-16). Marking reads only metadata, which stays in the clear.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pass compacts an encrypted cell and readers read the moved extents
- [ ] #2 A pass that only marks, or finds nothing to compact, makes no KMS call: a test counts calls
- [ ] #3 A cell whose tenant KEK is revoked is marked and has unreferenced objects deleted, but is not compacted, and the pass reports it
- [ ] #4 A sealed moves object copied to another key of its branch is refused (TASK-411.3 wrote the check, untested until moves are sealed)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Collector gets an optional KMS (kms::Shared): Collector::kms(..), Sweep.kms; fxdax-gc reads FXDAX_KMS via extent_keys::kms::from_url (a bad URL fails at start; none: sealed cells are left uncompacted, reported).
2. Lengths keeps each candidate's seal from the trailer it already reads (keyless). Pass::compact partitions the compactable into clear and sealed; only if a sealed one is there does it read the cell object and unwrap, through a keys::Kms made for that pass alone (no unwrap kept across passes: revocation stops the next pass), each KEK generation the needed records' DEKs name once: one KMS unwrap per cell pass. DEKs from the pass's records (keys::compacting ring, additive in keys.rs).
3. compact_object reads the object under the ring's view of its branch (keys::of_key: clear objects of sealed branches refused), and writes its moves sealed under the source object's own DEK generation (Seal::Sealed key of its trailer) with a fresh salt; clear in, clear out.
   Generation: the source's own, not the record's newest: every reader that may follow the moves read the source under that key, while a store opened before a newer generation was added holds a ring without it (rings fixed at opening) and would fail NoKey on moved extents.
4. KMS refusal (revoked, destroyed, failing) or no KMS: the pass compacts the cell's clear objects only, still marks/deletes, reports Report::locked (passes) and Report::uncompacted (objects), warns with the reason, and records no digest (KMS state is outside the cell's listing) so the next pass goes in full. fxdax-gc's pass line and sweep summary print them.
5. Tests (extent_remote collector): sealed cell compacted, readers holding the branch's ring read moved extents (fetcher), 1 unwrap per pass for N objects; KEEP pass, no-compactable pass, dry run: 0 KMS calls; revoked tenant: unreferenced objects deleted, nothing compacted, report says so; plaintext twin; sealed moves copied to another moves key of the branch refused (Corrupt names it). fxdax-gc parse/line tests.
6. Docs: doc-16 (what TASK-411.6 made; generation choice), doc-7 (compaction of sealed cells, locked passes, no digest). Follow-up task: compaction re-sealing under the newest generation once rings learn generations added after opening.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t3/sealed-cell-compaction (from 7bd73c7d9).
Keys: Collector::kms(Option<kms::Shared>), Sweep.kms; fxdax-gc reads FXDAX_KMS (extent_keys::kms::from_url; bad URL fails at start; dry runs read none). Per pass a fresh keys::Kms over it, so nothing unwrapped outlives the pass (a revoked tenant stops the next pass). Pass::compacting: if any compactable object is sealed (seal from the trailer Lengths read anyway; Lengths renamed Trailers, keeps the Frame), reads the cell object and keys::compacting (new, additive) unwraps each KEK generation the needed records name once, then the DEKs locally: 1 KMS unwrap per pass. Nothing sealed: keys::compacting(records, None), no KMS, still flags branches with DEKs sealed so clear objects there are refused (downgrade).
compact_object(.., ring): data_under(of_key view), moves sealed under the source trailer's KeyId (same branch+generation), fresh salt (framed draws one). Source generation, not newest: rings are fixed at opening; a store opened before add_generation holds no newer generation (test moves_are_sealed_under_the_generation_their_objects_were). Filed TASK-411.12 for moving old data to new generations.
Locked: Error::Kms from the unwrap (revoked, destroyed, Io, no KMS) -> warn (cell, why), Report.locked += 1, Report.uncompacted += sealed, compact clear ones only, still mark/delete; listing digest dropped (KMS outside the cell), so the next pass goes in full. Other errors (corrupt record, missing KEK generation) fail the pass as corruption does.
fxdax-gc: pass/sweep lines append ", left N sealed uncompacted, without their cell's KEK" (or "in L cells, without their KEKs"); metrics fxdax_gc_sweep_objects{action="uncompacted"}, fxdax_gc_sweep_locked_cells. extent_store sim/world.rs and fxdax storage/collect.rs take the two new Report fields (clear cells: 0).
Hand mutant: dropping fetch::Tail::of's Moves::of check fails sealed_moves_copied_to_another_key_of_their_branch_are_refused.
<!-- SECTION:NOTES:END -->
