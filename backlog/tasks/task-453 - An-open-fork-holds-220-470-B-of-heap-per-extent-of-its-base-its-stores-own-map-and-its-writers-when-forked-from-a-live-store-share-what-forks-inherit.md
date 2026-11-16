---
id: TASK-453
title: >-
  An open fork holds 220-470 B of heap per extent of its base (its store's own
  map, and its writer's when forked from a live store): share what forks inherit
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:15'
updated_date: '2026-11-16 04:04'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.2
  - TASK-242
priority: high
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.2 (docs/measurements/macos-fork-extent-memory-2026-11-15/note.md; cladefs remote::tests::bench_fork_extent_memory, heap counted exactly). Every open, idle fork holds a private HashMap of every extent its branch inherited, each entry with an owned key String (extent_store Recovering::open): 238 B an extent at 32,768 extents (130 B table, 107 B key), 221 B at 300,000. A fork of a live store (a checkpoint of its own per fork) also holds its writer's map unshared, 230 B an extent more: 468 B (451 B at 300,000); forks of a point share that one. Measured: 100 forks of a 32,768-extent base hold 0.78 GB (point) or 1.41 GB (live); of a 300,000-extent base 6.6 GB or 12.4 GB. Extrapolated to 400 forks: 3.1 / 6.1 GB at 32,768, 26 / 54 GB at 300,000. None of it is counted: the memory governor counts extent-cache pages and held parts only, and the anon budget (extentd/process.rs AnonBudget) has no part for it while adding 288 MiB per open store for framing, so it would not flag it either; the daemon is OOM-killed with every VM. extent_remote's Decoded cache also keeps 16 decoded states whatever their size (16 x 69 MB at 300,000 extents). TASK-242 names the same copy for open time (measured here: 6-9 ms a fork at 32,768 extents, 40-80 ms at 300,000, sim profile).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 bench_fork_extent_memory: an idle fork of a base of 32,768 and of 300,000 extents holds heap that does not grow with the base's extents (per fork, of a point and of a live store), the numbers recorded beside TASK-443.2's
- [x] #2 What the daemon still holds per extent, once per base, is counted by something the governor or the anon budget sees, or bounded
- [ ] #3 extentd's serves_hundreds_of_forks_of_a_base runs at 32,768 extents on Linux and bounds resident memory per fork
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Refactor (no behaviour change): extent_store's Extents becomes a type of its own (layout.rs) with the operations the store needs (get, write entry, insert, remove, follow, own_mut, iterators); every user goes through it.
2. Layered: Extents = the bucket's layer, the recovered persistent map (extent_remote::Extents, imbl OrdMap) shared with the writer's state and every other open store of the lineage, never mutated; over it the store's own overlay (HashMap of what it wrote, sealed, moved or acknowledged) and the inherited entries it hides (freed or overlaid). Lookups: overlay, then the base unless hidden. Recovering::open places the bucket's state as the base without copying.
3. extent_remote: Recovered carries the decoded state it started from (its layer, an Arc<State>); the writer keeps it and the store pins it (Opening.layer). Decoded keeps, besides its 16 kept (TASK-450's FIFO, unchanged), every decoded state still alive by Weak, so a layer open stores use is found while they hold it. A writer that creates a checkpoint keeps its state decoded under the checkpoint's key, so forks of a live store share the parent writer's persistent map instead of each decoding its own checkpoint.
4. Location's key an Arc<str>, interned as a checkpoint decodes (one per object), and shared by the extents a segment's acknowledgement places.
5. Accounting: Decoded::held() counts the live layers' extents once each (distinct maps by ptr_eq); extent_store counts overlay entries process-wide; cladefs AnonBudget gains an extents part (layers + overlays, by per-entry bounds measured by the benchmark), the most so far.
6. Tests: cladefs remote.rs unit test: N forks of one base share one layer (ptr_eq of their bases, Decoded::held() one layer of E extents), each reads the base's data, its writes and frees (of inherited extents too) its own; extent_store unit tests of the layered map; bench_fork_extent_memory asserts a per-fork bound independent of E and that the accounting covers what the heap holds.
7. Before/after bench at 1,024, 32,768, 300,000 x 10, 100 forks, point and live; suites: extent_store, extent_remote, cladefs (non-sim), extent_store sim, shuttle, clippy; docs doc-7 Memory and doc-14 Forks; notes.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15, built on macOS. Design: extent_store's map of a branch's extents is one type (extents.rs, Extents) in two layers: the bucket's, the recovered state's imbl OrdMap shared with the writer's state and every store that started from the same decoded state, never mutated; and the store's own (HashMap of what it writes, seals, acknowledges, follows) with the set of bucket entries it hides (freed or overlaid). get/remove/follow/written/own_mut/iter/lens/own; every user goes through it (store.rs, state.rs, segments.rs); Life holds Cow<Extent>. Recovering::open places the bucket's map as the bucket's layer, no copy. extent_remote: Recovered.layer (the decoded Arc<State> it started from: its checkpoint's, or the fork point's it inherited); Writer keeps it (layer()), Opening.layer, the store pins it. Decoded: besides its FIFO of 16 (TASK-450's, unchanged), every state it decoded still held by someone, by Weak, so a layer open stores use is found however many decodes came since; a state decoded anew is rebased onto the live state with the most extents of the same branches (per-branch counts kept per state): one ordered walk, alike's map with fresh's differences applied (None past half changed), so the checkpoints a live writer writes per fork, and a resumed fork's own checkpoint, share the map. Writer seeding of Decoded was considered and left out: rebasing covers it and needs no holder at the right time.
Accounting: Decoded::held() (each map once by ptr_eq; a state made of another still held counts the entries it changed, else all), at recovery::ENTRY_BYTES (512); extent_store::own_entries() process-wide (own layers, hidden, and bucket layers sharing no decoded state), at OWN_ENTRY_BYTES (1024: 612 measured per block written+uploaded, the key String counted twice, TASK-472). extentd AnonBudget gains extents (the most so far) = both, summed over this process's buckets (host_store::decoded_held). Not counted: the COW delta of footers a recovery applies after a checkpoint (bounded by checkpoint cadence), and per-fork overlay entries for inherited extents the collector moves (the fork's writer follows them).
TASK-450 left: FIFO KEEP=16 eviction order untouched; only the Weak lookup was added. Unbounded counting of states nobody holds stays theirs.
Tests: extent_store extents::tests::stores_layer_their_own_over_the_buckets; cladefs remote::tests::forks_open_at_once_share_what_they_inherited (4 forks live and of a point: one shared map by ptr_eq, Decoded::held == 1 state of 64 entries, reads the base's data; a fork's free of an inherited extent and its own write stay its own, others and a fork opened after read the base's and still share). Hand mutants (restored): rebased() always None -> red (forks of the live store: their maps); Extents::over copying the map -> red. bench_fork_extent_memory now asserts each fork past the first <= 64 KiB (IDLE_FORK_BYTES), Decoded of the bucket == 1 state of E entries, own entries unchanged by idle forks, a full unshared map <= E x ENTRY_BYTES, a fork's writes <= own entries x OWN_ENTRY_BYTES; CLADEFS_BENCH_PARENTS=live,point picks halves (TASK-471's flaky close check otherwise kills the point half).
Filed: TASK-471 (bench close/reopen check off by 256-2048 B on main), TASK-472 (intern Location keys).

2026-11-15, verified on macOS (commit b712efcad, branch t453). AC #1 checked: bench_fork_extent_memory, every (E in 1,024 / 32,768 / 300,000) x (10, 100 forks) x (live, point) run alone: marginal heap a fork 31.5-36.1 KB at every E (before: 0.27-0.51 MB at 1,024, 7.8-15.3 MB at 32,768, 66-135 MB at 300,000); totals at 300,000 x 100: 3.41 MB live / 3.19 MB point (before 12.42 / 6.62 GB); docs/measurements/macos-fork-extent-memory-shared-2026-11-15/note.md beside TASK-443.2's. Its new assertions passed in all 12 runs. Its close/reopen check (TASK-471) failed in the 6 point-alone runs (+213 to +2,048 B) and 1 of 6 live; with both halves in one process 3 of 3 passed: cold-process first allocations; noted on TASK-471.
AC #2 NOT checked: Decoded::held and extent_store::own_entries are exercised and their bounds asserted on macOS (bench, unit test), but extentd/process.rs, which adds them to AnonBudget, is Linux-only and was neither compiled nor run here. AC #3 (Linux, serves_hundreds_of_forks_of_a_base) not run: no ubu1 in this task.
Suites (macOS): cargo nextest run -p extent_store -p extent_remote -p cladefs -E 'not (package(extent_store) & test(/^sim::/))': 923 run, 909 passed and 14 failed in one run; the 14 (cladefs branches/cladefsctl/import and extent_remote gc tests) ran cladefsctl and cladefs-gc binaries another agent had built at bucket format 12 into the shared target dir ('of format 11, which this reader, of format 12, doesn't read'); rerun with this tree's binaries, those test binaries: 37 run, 37 passed. An earlier run, before the final bench-only edits, passed 923 of 923. sim part (test.sh): 52 passed; shuttle part: all passed (37, 10, 6, 33, 10, 18, 2, 1, 1); clippy part: clean. scripts/upstream-changes.py --check: pass. DoD #1 open: only these three packages on macOS, not the workspace, and nothing on Linux.

2026-11-15, review fixes (commit 164f4eed2, branch t453-fix; macOS).
Finding 1 (verified as reported): Store::extent -> State::life -> Extents::get built Cow::Owned(remote(location)) for every bucket entry, its key String cloned, under the store lock, on every read of an inherited extent (read_cached, DAX map, Holders::another_holds, public_reach). Fix: Extents lends an Entry (Own(&Extent) | Bucket(&Location)) with len/own/into_cow/to_extent; Life holds Entry; Store::extent, place/places, why_sealed, seal, begin_write borrow; entry_for, the audit and segments' touched extents take whole entries. Also found on the read path: CachedExtent::read boxed a bring_in future for every page even when present (1 allocation per cache hit, pre-existing); now copies a present page directly. Test remote::tests::reads_of_inherited_extents_allocate_nothing (heap::allocations_while, per-thread count; base of 4,160 extents, past extent_store's AUDITED_EXTENTS, since the audit feature unifies in when the three packages build together and recounts on every lock): 0 allocations for 64 store asks and 64 cache hits; red: 64/64 with the owned entry back, 64/64 with bring_in's box back. bench_inherited_cached_reads (sim, 32,768-extent base, 655,360 hits, interleaved runs): before 679/792/650/562 ns, 2 allocations a read; after 455/456/313/311 ns, 0 (one more after-run at 705 ns came while the disk was full).
Finding 2 (verified, reproduced): bench_fork_extent_memory now has a fork free every 32nd inherited extent, commit, checkpoint, upload, close, then reopen from its checkpoint beside an idle fork. Before the fix (sim, 10 forks): frees held 75,598 B vs 32,768 counted at 1,024 (2,362 B/free); 2,127,182 vs 1,048,576 at 32,768 (2,077 B/free); reopened map 26,804 vs 16,384 at 1,024 and 1,912,597 vs 524,288 at 32,768 (bytes past a 64 KiB idle bound). Fix: charge by what imbl copies. recovery::Changes { entries, runs }: rebased counts the runs of changes no unchanged entry separates; Held gains runs; RUN_BYTES = 4,096 a run past ENTRY_BYTES an entry, capped at a full copy (Changes::within). extent_store Own { entries, runs } (own_held replaces own_entries): runs of hidden bucket entries in the bucket's order, kept incrementally in hide() (+1, -1 per hidden neighbour; hidden never shrinks), own entries one more run, capped at the bucket layer copied whole; extentd extents_held = own_held().bytes() + decoded_held().bytes(). Chosen over a structural change (store's own layer taking the writer's acknowledged frees) as proportionate: the copy is bounded by a full copy anyway and the runs model is exact enough (2.2-2.5x over for scattered changes, ~2x for blocks). After: frees counted 163,840 / 5,242,880 / 48,000,000 at 1,024 / 32,768 / 300,000 vs held 75,598 / 2,127,182 / 19,174,879; reopened counted 147,456 / 4,718,592 / 43,200,000 vs 26,804 / 1,912,597 / 17,787,175. All bench assertions pass at 1,024 and 32,768 (live and point) and 300,000 (live; point not run). recovery::tests::changes_bound_what_maps_made_of_others_copy (own counting allocator, 1,024/32,768/300,000 entries, one change in 3..16,384, a block, growth): 689-3,298 B a scattered change, 230-254 B in a block, dense cases capped at 512/entry vs 230 held; red with runs uncounted. extents::tests::stores_count_the_runs_their_writers_change; red with hide counting nothing.
TASK-471's close/reopen check (asserted last, after all the above) failed by +256 B in 4 of 5 runs (1,024 live+point, 32,768 point, 300,000 live); passed in one (live 1,024+32,768).
Suites (macOS): -p extent_store -p extent_remote -p cladefs (not extent_store sim): 926 run, 926 passed (bins rebuilt from this tree first); sim part 50/50 (the earlier note's 52: sim sources unchanged since, not explained); shuttle part 37, 10, 6, 33, 10, 18 all passed; clippy part clean; cargo fmt clean for these crates. extentd/process.rs (Linux-only) not compiled: x86_64-unknown-linux-gnu check fails on aws-lc-sys's C build (no cross toolchain).
AC #2 left unchecked: the counts are now honest bounds (asserted on macOS), but the AnonBudget plumbing that adds them is Linux-only and still uncompiled and unrun.
doc-7 budget paragraph and docs/measurements/macos-fork-extent-memory-shared-2026-11-15/note.md updated.

2026-11-15: Linux, ubu1 under fxjob, TASK-453 before its review fixes (t453-int on main 21f042594-era): scripts/test.sh clippy and workspace, 2819 passed, 83 skipped; process.rs's AnonBudget code compiled and ran there. The review fixes (164f4eed2) need their own Linux run.

2026-11-15: Linux, ubu1 under fxjob, TASK-453 with its review fixes on main (t453-int2 996e1bac6): scripts/test.sh clippy and workspace, 2824 passed, 84 skipped; process.rs's AnonBudget extents part compiled and ran. Pushed to main.

AC #2 checked: shared maps counted once and stores' own entries and hidden runs counted (Decoded::held, extent_store::own_held), bounded above what imbl copies by changes_bound_what_maps_made_of_others_copy and the bench's frees/reopen checks on macOS; their sum is the anon budget's extents part (process.rs), which compiled and ran in the Linux workspace run (2824 passed). AC #3 (serves_hundreds_of_forks_of_a_base at 32,768 extents on Linux) still open.
<!-- SECTION:NOTES:END -->
