---
id: TASK-411.10
title: >-
  Plaintext tenants on a host with a KMS cost one KMS wrap call per store
  opening: the KMS's no-such-tenant answer is not cached
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 23:22'
updated_date: '2026-11-15 14:15'
labels: []
dependencies:
  - TASK-411.3
parent_task_id: TASK-411
priority: low
type: bug
ordinal: 808000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.3: a cell chooses encryption iff a KMS is configured and the tenant has a KEK. For a tenant with none, each opening asks the KMS again (a wrap call that fails NoSuchTenant), as that answer is not cached. A real KMS makes that a round trip on every opening of every plaintext tenant's store.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A host opening several stores of a tenant with no KEK makes at most one KMS call for it within a bounded time: a test counts calls (testing::Counted)
- [x] #2 A tenant given a KEK later gets encrypted new cells within that bound, documented in doc-16
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. keys::Kms keeps the tenants the KMS said have no KEK (NoSuchTenant on a wrap: new_kek's, or a pool refill's), with when it said so, believed for keys::KEKLESS_FOR (60 s): new_kek answers None (the cell in the clear) without a call meanwhile; any answer of the KMS for the tenant forgets it (Kms::answered). Clock: tokio::time::Instant, so tests advance a paused clock.
2. Test (extent_remote keys::tests): Counted, several new_cell_kek for a kekless tenant -> 1 wrap; KEK created in the KMS -> still None within the bound, no call; clock advanced past it -> Some (encrypted), and the next ask goes on pooled/wrap as before.
3. Test (extent_store upload::encrypted): a host opening three stores of a kekless tenant, new cells and reopenings -> (1, 0) calls.
4. Docs: keys.rs module/Kms docs; doc-16 'The KMS and how a cell chooses' gains the bound and why; concurrent first openings may each ask (no shared probe: a wrap's KEK is the cell's own).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t3/cache-tenant-keys-gc (from 979ecd28f). The brief called this TASK-411.11; its text and ACs are this task's (411.11, pruning old cell KEK generations, untouched).
keys::Kms.kekless: tenant -> tokio::time::Instant of the NoSuchTenant heard (new_kek's inline wrap, or a pool refill's fill). Kms::new_kek: pooled KEK first; then kekless within KEKLESS_FOR (60 s) -> Ok(None), no call; else wrap. Kms::answered (any Ok of the KMS for the tenant) forgets it. tokio's Instant so tests drive a paused clock; outside a runtime it is std's.
Bound 60 s: the cost of a plaintext tenant drops from a round trip per store opening to one per host per minute (openings are what call: every opening of a store whose record has no DEK asks new_cell_kek in case it makes the cell); the price is that a tenant given a KEK gets sealed new cells within 60 s, cells made in the clear meanwhile staying clear for good (a cell's sealing is chosen at making). A minute is short beside onboarding a tenant's BYOK and long beside a zygote's burst of restores. Not shared between concurrent first askers: a wrap's KEK belongs to the cell it makes, so a shared probe would need a second wrap anyway; a burst asks once per concurrent opening, then nothing for 60 s.
Tests: extent_remote keys::tests::tenants_without_a_kek_are_asked_for_one_once_a_while (paused current-thread runtime: 3 asks -> 1 wrap; KEK created, advance 59 s -> None, no call; +1 s -> Some, generation 1, 2 wraps; next take wraps as before); extent_store upload::encrypted::cells_of_tenants_with_no_kek_are_in_the_clear extended (host opens a second volume of the cell, a volume of a new cell, the first again: calls stay (1, 0); another host asks once: (2, 0)). Both pass.

Validation (macOS, branch t3/cache-tenant-keys-gc at fb253eb4a): cargo nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p fxdax -E 'not (package(extent_store) & test(/^sim::/))': 960/960 passed (extent_keys 27, extent_frame 33, extent_remote 323, extent_store 223, fxdax 354; 1 leaky: faults::tests::faults_say_their_words, untouched). sim::one SIM_SEEDS=10 over collectors, everything, forks, takeovers, crashes, wals, forked_wals, restores, fresh_cells, failed: all pass. clippy --all-targets (extent_remote, extent_store, fxdax): no warning in touched files. fmt clean on touched crates. scripts/upstream-changes.py --check passes. No dependency change. Linux not run (no ubu1 per brief): DoD #1 open.

Verified 2026-11-15 at 594ff82e9 (origin/main merged): macOS scripts/test.sh workspace 2466/2466, shuttle, scripts (mutants 0 broken), go; Linux ubu1 under fxjob (target on /mnt/fxdax-scratch), every part: clippy, workspace 2752/2752, sim 63/63, shuttle, scripts, go, guest, firecracker. Only fxdax's own crates changed (extent_keys, extent_frame, extent_remote, extent_store, sqlite_extents, fxdax, fxdax-examples): no upstream deviation; scripts/upstream-changes.py --check passes.

TASK-411.28 (2026-11-15) removed what this made: keys::KEKLESS_FOR and the kekless cache. It spared a plaintext tenant's openings a wrap each; no tenant is plaintext now. A NoSuchTenant is a refusal like Revoked and Destroyed (keys::revokes): the tenant's pool dropped, the tenant noted refused (Kms::revoked), and the KMS asked again at the tenant's next call. tenants_without_a_kek_are_asked_for_one_once_a_while and cells_of_tenants_with_no_kek_are_in_the_clear went with it.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
keys::Kms believes a NoSuchTenant for KEKLESS_FOR (60 s) from the wrap that heard it (new_kek's or a pool refill's): new_kek answers None (cell in the clear) with no KMS call meanwhile; any answer of the KMS for the tenant forgets it. A plaintext tenant's store openings cost one wrap per host per minute instead of one per opening; a tenant given a KEK gets sealed new cells within 60 s (cells made clear before stay clear). Documented in doc-16. Verified by keys::tests::tenants_without_a_kek_are_asked_for_one_once_a_while (paused clock: 3 asks 1 wrap; KEK given, 59 s still none and no call, 60 s sealed) and extent_store cells_of_tenants_with_no_kek_are_in_the_clear (four openings on a host, 1 wrap; another host 1 more); 960 macOS tests and a 10-seed sim sample over 10 profiles pass; Linux not run.
<!-- SECTION:FINAL_SUMMARY:END -->
