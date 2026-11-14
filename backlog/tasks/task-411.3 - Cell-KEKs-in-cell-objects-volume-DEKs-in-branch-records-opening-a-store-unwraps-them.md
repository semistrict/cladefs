---
id: TASK-411.3
title: >-
  Cell KEKs in cell objects, volume DEKs in branch records; opening a store
  unwraps them
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 22:15'
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
- [ ] #1 A new cell gets a cell KEK and a new branch a DEK, each stored only wrapped
- [ ] #2 Opening a store unwraps with one KMS call per cell taking, not per volume: a test counts calls
- [ ] #3 A store whose tenant KEK is revoked fails to open with an error naming the tenant
- [ ] #4 Rotating a cell KEK rewraps its volumes' DEKs by conditional updates; adding a DEK generation makes new objects use it while old ones stay readable
- [ ] #5 A reader of a branch whose record has a DEK refuses an object of that branch in the clear (a downgrade: someone who can write the bucket zeroing an object's seal, MACs and tags), with an error naming it
- [ ] #6 Objects whose footers bind nothing to their key (own extents, the collector's moves) are bound to what names them (extent ID or key) under the MAC, so that a whole object copied to another key of the same branch is refused
- [ ] #7 Host-wide caches (fetch tails, recent parts, peers) open parts with a host-wide key set of the stores the host serves, not a store's own
- [ ] #8 wal::whole_footer and every other reader that treats an error of data() as not whole reads encrypted segments as whole (keys, or the keyless check), so recovery never re-frames them from their raw files
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
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
