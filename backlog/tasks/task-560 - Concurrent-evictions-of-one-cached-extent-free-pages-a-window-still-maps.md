---
id: TASK-560
title: Concurrent evictions of one cached extent free pages a window still maps
status: To Do
assignee: []
created_date: '2026-11-18 15:26'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 951000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), host-dax review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/cache.rs`: `rg -n -F 'Slot::Evicting(frame) => Some(frame),' crates/cladefs/src/cache.rs`

CachedExtent::evict_pages works in three steps. Section 1 marks the Present slots in its range Evicting and records their frame numbers (`frames`). It then calls unmap_pages_of on each window, matching only those frames. Section 2 relocks the table and takes every Evicting slot of the whole extent, not just the frames this call marked, and drops them. Several threads evict the same extent with no lock between them: the relief thread (`relieve_with` evictor), the governor (`govern` -> `shrink`), the control API's Shrink (`cells::blocking`) and `cache::forget`. So when two calls overlap, the one that reaches section 2 first frees frames the other has marked but has not yet unmapped from every window. The freed frames are staged, so they go to quarantine, but `quarantine` parks a page only behind taking ops a peer has already been sent. The other evictor's Unmap for those frames has not been sent yet. If the VM process has acked everything it was sent, the page is freed at once and handed to a waiting allocation, while the VM process still has it installed.

**Trigger.** 1) Under memory pressure, relief (arena starved) and the governor's shrink both pick the same least-used mapped extent E, mapped by windows W1..Wn. 2) Relief R: E.evict() section 1 marks pages Evicting (frames_R), snapshots the windows and starts unmap_pages_of on W1, W2, ... 3) A guest on window Wk touches an evicted page. Window::resolve -> bring_in -> read_part makes page p Present with frame F, and install() maps F in Wk (Map op; the VM process installs F). 4) Governor G: E.evict() section 1 marks F Evicting (frames_G={F}, non-empty, so no early return) and starts its own unmap loop, behind R on the same window order. 5) R finishes its loop. Its section 2 collects every Evicting slot, F included, and drops it. Page::drop -> sift -> quarantine -> Acks::park parks nothing for Wk's peer: acked == last_taking, because G's Unmap of F is not sent yet. F is freed and taken by the allocation waiting on relief, which fills it with another extent's bytes. 6) Until G reaches Wk and sends its Unmap, Wk's guest reads another extent's data at E's page through KVM, with no fault. An application may act on it or write it back. The reverse order, with G preempted after its section 1, frees frames_R the same way.

**Why the tests stay green.** No test runs two evictions of one CachedExtent at once. cache.rs has no test module and no Shuttle model. The dax Shuttle tests use TestSource, which has no Evicting state and no two-phase eviction. physmem's quarantine tests check parking against acks, not that every window's unmap of a page was sent before the page is dropped. The relief and shrink tests in cladefsd/memory.rs and open_cell.rs evict from one thread.

**Backlog.** No related task covers it: TASK-79.9 (quarantine), TASK-50.3 (relief and shrink) and TASK-161 (relief while quarantined), all Done, cover the area, but none makes concurrent evictions of one extent safe.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When evictions of one cached extent overlap (relief, the governor's shrink, the control API's Shrink, cache::forget), no page is freed or quarantined before every window that mapped it has been sent its Unmap: each eviction frees only the pages it marked, so a VM process never keeps a page installed that was handed to another extent.
- [ ] #2 A regression test in cladefs's cache.rs (Shuttle, or a failpoint after the marking section) maps a two-page cached extent in two windows with recorded second stages; eviction A pauses after marking, B pages in page 1, maps it in window 2, evicts it and finishes, then A resumes; a test quarantine asserts no second stage still maps a freed page; the reverse order is checked too; the test goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
