---
id: TASK-411.3
title: >-
  Cell KEKs in cell objects, volume DEKs in branch records; opening a store
  unwraps them
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 23:18'
labels: []
dependencies:
  - TASK-411.1
  - TASK-411.2
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 800000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-16, Keys: the tenant KEK wraps a cell KEK kept in <C>cell, which wraps each volume's DEK kept in its record, with generations. Record is Copy today and gains variable-length fields; layout FORMAT changes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A new cell gets a cell KEK and a new branch a DEK, each stored only wrapped
- [x] #2 Opening a store unwraps with one KMS call per cell taking, not per volume: a test counts calls
- [x] #3 A store whose tenant KEK is revoked fails to open with an error naming the tenant
- [x] #4 Rotating a cell KEK rewraps its volumes' DEKs by conditional updates; adding a DEK generation makes new objects use it while old ones stay readable
- [x] #5 A reader of a branch whose record has a DEK refuses an object of that branch in the clear (a downgrade: someone who can write the bucket zeroing an object's seal, MACs and tags), with an error naming it
- [x] #6 Objects whose footers bind nothing to their key (own extents, the collector's moves) are bound to what names them (extent ID or key) under the MAC, so that a whole object copied to another key of the same branch is refused
- [x] #7 Host-wide caches (fetch tails, recent parts, peers) open parts with a host-wide key set of the stores the host serves, not a store's own
- [x] #8 wal::whole_footer and every other reader that treats an error of data() as not whole reads encrypted segments as whole (keys, or the keyless check), so recovery never re-frames them from their raw files
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_frame: Keys::sealed() (a reader scoped to a branch whose objects are sealed refuses one in the clear: Error::Clear); Seal::Authenticated (kind 2: parts in the clear, each tagged by GMAC, footer and table MACed) for checkpoints, which the collector reads keyless; table::encode authenticates; Parts::of/of_suffix/check_footer/footer/data/restamp return Error.
2. extent_keys: a dyn-compatible KMS handle (kms::Shared) and kms::from_url (local:///DIR, the only backend).
3. extent_remote: layout FORMAT 10. Cell object gains its KEKs (generations, each wrapped by the tenant's KEK; none: a plaintext cell); records gain DEKs (generation, cell-KEK generation, wrapped under a key derived from the cell KEK and bound to tenant, cell, branch and generation); Record no longer Copy. New module keys: Ring (a reader's DEKs and which branches are sealed, scoped per branch by Ring::of; the writing key), HostRing (refcounted union a host's fetchers read with), Kms (the host's KMS, cell KEKs unwrapped once per taking, a pool of wrapped cell KEKs for cells made), keys::open (one KMS unwrap per taking, DEKs unwrapped locally along the lineage; an ancestor of another cell with DEKs refused: Error::Inherited, TASK-411.4), add_generation, rotate (rewrap records by conditional updates, old KEK generations kept). Writer, recovery (Checkpoints), batch, fetch (tails opened lazily under the host ring, downgrade checked by key's branch, NoKey never cached), object, checkpoint wired. Own extents' footers name their ExtentId, moves' footers the key they are the moves of; readers check.
4. extent_store: Opening and Store carry the Ring; frame_segment, seal_own, write_own, local index checkpoints, wal re-framing, framed.rs readers, uploads restamp use it; wal::whole_footer uses the keyless check (extent_frame::check). upload::Runtime gains the host's Kms and HostRing; Shared::start takes the cell with a pooled KEK if it makes it, builds the Ring, gives the branch a DEK if its cell is encrypted, registers the ring host-wide for the store's life.
5. fxdax: FXDAX_KMS (local:///DIR) configures the KMS for extentd, the CLI's FUSE mounts and fxdaxctl via remote::runtime(); a cell is encrypted iff a KMS is configured and its tenant has a KEK there (NoSuchTenant: plaintext; any other KMS error refuses). Forks into other cells of encrypted parents refused before anything is made (ENOTSUP, TASK-411.4). errno for new errors.
6. Tests per AC: keys only wrapped; Counted KMS one unwrap per cell taking across volumes; revoked tenant named; rotation and generations; downgrade refused naming the object; own extent copied to another key refused; host-wide ring in fetch tails/recent parts/peers; whole_footer reads sealed segments whole. Encrypted variants: forks and points within a cell (extent_remote), store reopen/restore of WAL, followers, sqlite volumes, FUSE where feasible.
7. doc-16 updated (KMS config, cell choice, formats); narrow nextest runs, then the full crate set and a sim sample; clippy, fmt.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t3/tenant-encryption-key-hierarchy, commits 2943522bb..2e558ef14.
Formats: layout FORMAT 10 (9 refused by name). cell::Object.keks: Vec<Kek{generation, wrapped by tenant KEK}> (none: clear cell). Record.deks: Vec<Dek{generation, kek, wrapped}> under AES-KW with a key HKDF'd from the cell KEK and the DEK's place (tenant, cell, branch, generation). Record no longer Copy. Own extents' footers are their ExtentId; Moves gains of: the key it moves. extent_frame Seal::Authenticated (kind 2, no FORMAT bump: FORMAT 2 unreleased) for checkpoints: parts clear, GMAC-tagged; readers' Parts::of/footer/data return Error; Keys::sealed and Error::Clear.
KMS: FXDAX_KMS=local:///DIR (extent_keys::kms::from_url), read via fxdax::remote::runtime() by extentd, CLI FUSE mounts, fxdaxctl; a bad URL becomes a KMS refusing every call (no silent clear cells). fxdax-gc needs none. Cell encrypted iff KMS configured and tenant has a KEK (NoSuchTenant: clear; other refusals fail). Pool of wrapped cell KEKs per tenant (wrap when empty; unused given back).
Keys flow: extent_remote::keys (Ring scoped per branch by Ring::of; HostRing union in fetch::Memory; Kms caches unwraps per (cell, generation, taking); CellKeys::to_hold predicts the taking a writer's pre-hold recovery makes). upload::start: record + lease; if record has DEKs, cell read + ring, recovery keyed; cell held with pooled KEK if it may be made; with_dek gives first DEK; ring; Writer::open_keyed/open_recovered_from(ring); Opening.keys; ring held host-wide for the uploader's life. Points: cell read, ring without taking. Fetch tails kept keyless, opened lazily under host ring (NoKey never cached). Local: Segments/OwnFiles/Framed::open keyed; seal_own/write_own/frame_segment sealed; Closed::bytes restamps keyed; whole_footer uses extent_frame::check; close_again frames clear (WAL recovery precedes keys) and Recovering::open reseals via new Dir::replace (atomic in-directory rename over).
Cross-cell: keys::ring refuses an ancestor of another cell with DEKs (Error::Inherited, ENOTSUP); fxdax host_store::sealed_fork_into refuses such forks before anything is made. TASK-411.4 lifts it.
Rotation keeps older KEK generations in the cell object: pruning needs writers adding DEKs under an older generation fenced first (follow-up, not filed as a bug).
Plaintext path unchanged in requests: no KMS -> no cell read, no ancestor reads, Ring::clear.
Linux: cargo check --target x86_64-unknown-linux-gnu (zig cc) of fxdax, sqlite_extents, extent_store, extent_remote with --tests passes (fuse/control.rs makes cells with a pooled KEK).

Validation (macOS): cargo nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p sqlite_extents -p fxdax -E 'not (package(extent_store) & test(/^sim::/))': 952/952 passed. sim::one SIM_SEEDS=10 over everything, forks, takeovers, crashes, wals, forked_wals, restores, fresh_cells, collectors, failed: all pass. clippy --all-targets: no new warnings in new code. Linux cross-check (cargo check --target x86_64-unknown-linux-gnu, zig cc) passes; Linux tests not run here (user runs them). scripts/upstream-changes.py --check passes.
AC evidence: #1, #2 extent_store upload::encrypted::cells_keep_keys_wrapped_and_one_unwrap_takes_a_cell (no KEK/DEK/data bytes in any object; (wraps, unwraps) = (2, 0) making, (0, 1) for three volumes on another placement) and extent_remote keys::tests::cell_keys_unwrap_once_a_taking_and_rings_follow_origins; #3 stores_of_revoked_tenants_open_not ('tenant 1: its KEK is revoked'); #4 generations_seal_new_objects_and_rotations_rewrap; #5 objects_of_sealed_branches_in_the_clear_are_refused (segment and checkpoint re-framed clear) and extent_frame readers_holding_objects_sealed_refuse_them_in_the_clear (seal, MACs, tags zeroed); #6 own_extents_copied_to_another_key_are_refused (moves' Moves::of check implemented, its refusal untested: no sealed moves exist until TASK-411.6); #7 hosts_fetchers_open_parts_under_the_keys_of_the_stores_they_serve; #8 wal::tests::sealed_segments_framed_whole_are_whole_holding_no_key.
Encrypted variants: forks and points within a cell, cross-cell refusal (forks_and_points_within_a_sealed_cell_read_what_they_inherit), fxdax store served/followed/reopened/fork refused (extents::tests::sealed_stores_serve_follow_reopen_and_fork_into_no_other_cell). Not written: sqlite volumes, FUSE mounts, extentd, restores on sealed cells (Linux-only daemon paths; restores fork across cells, refused until TASK-411.4).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Cell objects keep their KEK generations wrapped by the tenant's KEK (FXDAX_KMS=local:///DIR; a cell is sealed iff the host has a KMS and the tenant a KEK there), records their DEK generations wrapped by the cell KEK and bound to their place; layout FORMAT 10. Opening a store takes the cell with one KMS unwrap per taking and builds a ring of its branch's and same-cell ancestors' DEKs, held by its writer, store and the host's fetchers (host-wide union); objects are sealed (checkpoints authenticated, kind 2), clear objects of sealed branches refused, own extents and moves bound to their keys, whole_footer keyless, recovery's clear re-framings resealed. Rotation and DEK generations by conditional updates. Cross-cell forks of sealed branches refused (TASK-411.4). Verified by 952 macOS tests, a 10-seed sim sample over 10 profiles, a Linux cross-check, and new tests per AC.
<!-- SECTION:FINAL_SUMMARY:END -->
