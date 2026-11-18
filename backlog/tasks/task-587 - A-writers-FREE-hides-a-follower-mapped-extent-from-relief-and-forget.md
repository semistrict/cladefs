---
id: TASK-587
title: A writer's FREE hides a follower-mapped extent from relief and forget
status: To Do
assignee: []
created_date: '2026-11-18 15:29'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 978000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), follower-host-side review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/cache.rs`: `rg -n -F '.filter(|(holder, _)| !Arc::ptr_eq(holder, store))' crates/cladefs/src/cache.rs`

`Holders::another_holds` skips any holder that is `Arc::ptr_eq` to the freeing store. A follower's `OpenCell` is built by `OpenCell::follow` with `store: self.store.clone()`, so it shares its writer's `LocalCell`. When the follower MAPs an extent, `cache::cached` records that same `Arc<LocalCell>` as the holder. When the writer later FREEs the extent, `OpenCell::free` calls `cache::freed(&self.store, ..)`. `another_holds` finds no other holder, so the entry leaves the cache map, and `map_zeros_over` runs only on the writer's own window. The follower's DAX window still holds the `Arc<CachedExtent>` and its present pages, but nothing that walks `cache.extents` can see them any more. That covers relief (`evict_in_order`), the `MOST_CACHED` trim, the metrics gauges and `cache::forget`. Pages that relief could evict before the free (unmapping them from the follower's window) become unevictable. A shred or KEK revocation leaves that plaintext mapped in the follower VM, although `bucket::forget` promises to evict 'every extent no other open store holds ... unmapped from the windows that map them'. A follower MAP of the same extent before its batch also creates a second cache entry and pages the extent in again from the store (`read_kept`), doubling memory and I/O. TASK-79.7 brought in `Holders` for fork siblings, but a follower shares the LocalCell, so that fix does not cover it.

**Trigger.** 1) Writer VM W and follower VM F of cell C on one host. F's guest DAX-faults a chunk of file f: MAP(E) -> OpenCell::map -> mapped_of (view_extent OK) -> self.cached(E) -> cache::cached(&shared LocalCell): new entry (arena,E) whose only holder is that LocalCell; F's window maps it and pages are installed. 2) W's guest rewrites f and Fxfs FREEs E -> OpenCell::free -> store.free -> cache::freed: another_holds filters out the only holder (ptr_eq to W's store, which F shares) -> extents.remove((arena,E)); only W's window maps zeros. 3) F's window still maps the orphaned CachedExtent. If F is slow or stalled (production never cuts it off), or the cell is shredded (halt -> F's ADVANCE fails EACCES, so its batch never UNMAPs), the pages stay until F's connection goes. evict_least_used cannot free them, so a tenant arena at its cap fails page-ins with ENOMEM (NoPages), the writer's included. cache::forget cannot unmap them, so F's guest keeps loading the shredded cell's plaintext through its window.

**Why the tests stay green.** open_cell's followers_map_their_views_commits maps through a follower but the writer never frees; freeing_an_extent_maps_zeros_in_its_place and forks_page_in_what_a_sibling_freed (TASK-79.7) cover the writer's own window and fork siblings, which are different LocalCells. The cache's relief and forget tests have no follower. Follower.tla does not model the extent cache: its MappingsLive covers guest-level mappings against released extents, and the code orphans the entry at FREE time, before the commit and LetGo the spec orders. The pages stay readable through read_kept, so no read fails.

**Backlog.** Related. TASK-79.7 (Done) added Holders for fork siblings, which are separate LocalCells, while a follower shares its writer's; TASK-411.7 (Done) built forget, which promises to unmap every extent no other open store holds.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a writer FREEs and commits an extent its follower still maps, the extent cache keeps the entry (cache::keys() contains (arena, E)) until the follower lets it go.
- [ ] #2 cache::forget for the cell (a shred or a KEK revocation) unmaps the follower window's pages and relief can evict them; a follower MAP of E before its batch uses the one entry rather than paging E in again.
- [ ] #3 An open_cell test commits E, follows, maps and faults page 0 in the follower's window, frees E and commits, then asserts the entry stays and forget unmaps the page; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
