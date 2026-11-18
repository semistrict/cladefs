---
id: TASK-542
title: Hosts reading each other's parts deadlock their GET budgets for 5 s at a time
status: To Do
assignee: []
created_date: '2026-11-18 15:23'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 933000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-read review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/bucket_cell/src/fetch.rs`: `rg -n -F 'let fetched = self.fetch(key, part, &stored, owner).await;' crates/bucket_cell/src/fetch.rs`

In `Fetching::part` the flight first awaits its place in the host's budget (`let slot = given.await;`). It then calls `self.fetch(..)`, which asks the owner (`owner.part(key, part).await`) before going to the bucket, and the slot (1 MiB of byte budget and one of 64 requests) stays held through the whole peer round trip. The owner serves through `cladefs::bucket::Serving::part` -> `Fetcher::stored_part` -> `flight(.., Priority::Demand, None)` -> `budget.enter`. That is the same global `BUDGET` (64 requests, 64 MiB) its own stores' reads use (`serving_peers` passes `BUDGET.clone()`), and the request queues FIFO behind every demand flight already waiting on that host. This makes a wait-for cycle across hosts: A's admitted slots wait on B, and B's GetPart requests to A queue behind A's local demand flights, which wait on B. Bucket-bound flights finish in tens of ms, and FIFO admission hands their slots to the next local flight. Flights waiting on a peer free nothing, so they pile up until every slot on both hosts is waiting on the other. Only tonic's per-request DEADLINE (5 s, cladefsd/peers.rs) breaks the cycle. After it fires, `Health::down` stops asking that peer for DOWN (30 s), and the server-side `stored_part` tasks, spawned on the bucket runtime and never aborted, keep their queued places and later GET parts nobody waits for. The budget bounds GETs. A peer request is not a bucket GET, yet it holds a GET slot while depending on another host's GET slots.

**Trigger.** 1. Two daemons have CLADEFSD_PEERS naming each other (TASK-406.2) and cold part caches. 2. Forks of one base boot on both hosts at once, so each host queues more than about 130 demand part flights. These come from page-ins via StoredExtent::read -> HostFetcher::read -> Fetcher::read -> data -> part, plus cladefs cache.rs PARTS_AHEAD, for objects whose parts the rendezvous hash splits between the hosts. 3. On A, 64 flights are admitted: about half go to the bucket and finish, and their slots go to the next queued local flights. Within a few round trips all 64 slots on A hold flights blocked in `owner.part()` to B, and B ends up the same way. 4. B's GetPart for A's parts reaches A's `stored_part` -> `flight` -> `budget.enter(Demand)` and waits behind A's remaining local flights. No slot on A ever frees, and the same holds on B. 5. Every cold read on both hosts stalls up to 5 s. Then each host marks the other down for 30 s and all reads go to the bucket. The cycle comes back once peers are asked again.

**Why the tests stay green.** The fetch.rs peer tests (`parts_another_host_owns_come_from_it`, `parts_their_owner_gives_wrong_come_from_the_bucket`) give each host a separate `limits(8)` budget and read 2 parts. Their Owner fake serves through a fetcher that asks no peer and is never saturated. No test has two hosts that each own parts the other reads, each with one budget shared between local reads and serving. TASK-406.2's notes say the path where a peer answers past its 5 s deadline is not tested directly, and tests/peers.rs reads a handful of parts from one host.

**Backlog.** Related: TASK-406.2 (Done) chose to ask peers under the GET grant but never named this cycle and left the past-deadline path untested. TASK-518 (To Do) is a different slot leak, on a cancelled cold read. TASK-443.7 (To Do) names the shared 64-GET budget but not peers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Two daemons whose CLADEFSD_PEERS name each other, each with one GET budget shared by its own stores' reads and its serving of the peer, and each queueing demand reads of parts the other serves, finish every cold read without waiting for the 5 s peer deadline. No read is left pending while no bucket request is outstanding.
- [ ] #2 When a peer request is given up at its deadline, the serving host's queued fetch for it leaves its place in that host's budget. It does not later GET a part nobody waits for.
- [ ] #3 A regression test in crates/bucket_cell/src/fetch.rs builds two hosts, each with one Budget (Limits{requests: 2, ..}) shared by its local and serving fetchers, and a Peers fake that routes every part to the other host's stored_part with no timeout. It queues 4 demand reads on each host of parts the other serves and completes bucket requests with complete_all. It asserts every read finishes after run_until_stalled, and it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
