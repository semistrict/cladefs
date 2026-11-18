---
id: TASK-580
title: A follower that stops reading makes COMMITTED notices stall the IO runtime
status: To Do
assignee: []
created_date: '2026-11-18 15:28'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 971000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-protocol review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/cladefsd/wire.rs`: `rg -n -F '&& let Some((queued, _)) = (queue.messages.iter_mut().rev())' crates/cladefs/src/cladefsd/wire.rs`

`AsyncSender::push` with `latest` (used only by `send_latest` for COMMITTED) searches `queue.messages` from the back for an unsent message of the same kind, holding the queue lock, so the cost is O(queue length) per send. The queue can hold about 4.4M header-only messages: credit 272 MiB / 64 B, and each VecDeque slot really holds about 128 B, so roughly 570 MiB while `queued` reports 272 MiB. `tell_commits` runs on the IO runtime (`start_behind` → `IO.spawn`), which has 2 worker threads. Every connection's `receive` task and the watchdog also run there. Its loop awaits only when `store.newer(commit, ZERO)` returns None. If every scan takes tens of ms while the writer keeps committing, `newer` always returns Some and the task never yields, so it holds an IO worker indefinitely.

**Trigger.** A compromised tenant uses three connections to its own store. W opens it to write and pipelines small WRITE+COMMIT requests without stopping. F1 and F2 OPEN_READ_ONLY the same path; granted tenants may follow their own non-public stores. Each F stops reading, so its socket buffer fills. After W's next commit, a COMMITTED sits unsent in F's queue. F then pipelines INFO requests (64-byte replies), keeping fewer than 4096 unanswered, until `queued` nears its 272 MiB credit: about 4.4M replies behind the COMMITTED. From then on, every run of W's commits makes each F's tell_commits walk about 4.4M entries (tens of ms) to replace the buried COMMITTED. Commits land during each walk, so the loop never awaits. Two such tasks hold both IO workers. No connection of any tenant has its messages received any more: faults go unanswered, ACKs go unread, and the watchdog task is starved too. Every VM on the host hangs.

**Why the tests stay green.** No test combines a follower that does not read and has a long queue with a writer that keeps committing. `async_sends_queue_what_the_socket_has_no_room_for` and `sends_of_a_batch_arrive_in_the_order_made` check ordering only. The hostile proptest's followers never queue millions of replies, and floods use READs, whose replies are large and few.

**Backlog.** No related task covers it: TASK-79.9 (Done) added send_latest only to stop COMMITTED notices piling up; TASK-443.1 (To Do) investigates the 2-worker IO runtime stalling behind blocked requests, a different mechanism; TASK-79.16 and TASK-149.35 (Done) bound other things.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Replacing a follower's unsent COMMITTED notice costs O(1) whatever its queue holds: a wire.rs unit test timing send_latest with 1M queued messages sees no growth with queue length.
- [ ] #2 With a follower queuing 1M INFO replies behind a COMMITTED while its writer commits in a loop, tell_commits yields to the IO runtime and another tenant's INFO round trip stays under 100 ms throughout.
- [ ] #3 A regression daemon test in cladefsd's tests reproduces that follower and writer and asserts the 100 ms bound; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
