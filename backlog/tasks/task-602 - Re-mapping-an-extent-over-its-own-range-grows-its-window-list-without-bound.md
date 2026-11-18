---
id: TASK-602
title: Re-mapping an extent over its own range grows its window list without bound
status: To Do
assignee: []
created_date: '2026-11-18 15:31'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 993000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), host-dax review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/cache.rs`: `rg -n -F 'self.pages.with(Mutex::lock).windows.push(Arc::downgrade(window));' crates/cladefs/src/cache.rs`

Window::place first forgets the old mappings in the range and only then checks whether the window still maps the source (`let first = !state.by_source.contains_key(&source_identity(source));`, dax.rs). A MAP that replaces the window's only mapping of extent E with E again finds by_source without E, so it calls E.mapped(window). CachedExtent::mapped pushes a new Weak<Window> without checking for one already there. Window::unmapped is skipped afterwards because by_source now contains E again, so the old entry stays. Every such MAP adds one entry to E's `windows`. Duplicates are removed only when the window unmaps E entirely; evict_pages' retain removes only dead windows. A guest that MAPs the same extent over the same range again and again grows the daemon's heap without bound, outside the governor's budget. Every eviction of E then calls unmap_pages_of on that window N times, each taking the window lock and scanning PTEs, and each unmapped() runs a retain over N entries. For a public base's extent, shared in the default arena by every tenant's windows, relief for the default arena or total cap pays this cost for all tenants.

**Trigger.** A guest, a hostile one or one re-establishing a range, sends MAP(extent E, offset 0, len L, window_offset W) repeatedly over a range whose only mapping is already E. Each goes OpenCell::map -> map_counted -> Dax::map -> Window::map_as -> map_locked -> place: forget removes E from by_source, first=true, E.mapped(window) pushes another Weak, and unmapped() skips E. After N MAPs, E.windows holds N entries for one window. 10M MAPs is about 80 MB of daemon heap. Each relief or shrink eviction of E then makes about 10M window-lock round trips on the relief thread, which every starved arena's allocations wait for.

**Why the tests stay green.** The window tests use TestSource, whose mapped() and unmapped() do nothing. No test counts CachedExtent.windows. The hostile proptest (cladefsd/tests/hostile.rs) repeats only PRIVATE, UNMAP and FAULT storms, never MAP, and its heap bound has 256 MiB of slack.

**Backlog.** No related task. TASK-160.11 and TASK-92.50 (Done) touch by_source and MAP cost, not this list.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a VM process MAPs the same extent over the same range 1000 times, the extent's window list holds one entry for that DAX window. One eviction of the extent then unmaps it from that window once.
- [ ] #2 A test in open_cell.rs's tests asserts, through a #[cfg(test)] accessor, that the cached extent's window list has length 1 after the repeated MAPs (1001 today). It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
