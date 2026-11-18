---
id: TASK-615
title: A cancelled GetStats leaves an ask on every VM that has not answered
status: To Do
assignee: []
created_date: '2026-11-18 15:33'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1006000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-resources review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/cladefsd/stats.rs`: `rg -n -F 'let vm = match tokio::time::timeout_at(deadline, answer).await {' crates/cladefs/src/cladefsd/stats.rs`

`stats::take` pushes an `Asked` onto each VM's `Report.asked` in `Report::ask`. It removes it with `report.forget(ask)` only after the timeout in this loop expires or the answer channel errors. `Control::get_stats` awaits `take(wait)` directly inside the tonic handler. When the client's gRPC deadline expires, or it disconnects or cancels (Ctrl-C on `cladefsctl stats`, a scraper deadline shorter than the 2 s default wait), tonic/hyper drop the handler future. The `forget` calls never run, so each VM that has not answered keeps one `Asked` plus a oneshot sender per cancelled call. Such asks leave only when that VM answers a later ask (`answered` drains every ask up to it). A VM whose guest is paused never answers, which is exactly TASK-198's case. Its report then grows by one entry per cancelled GetStats for as long as it lives.

**Trigger.** 1. A VM is paused, so its device still takes stats asks but its guest never answers. 2. A monitoring client polls GetStats with a 1 s gRPC deadline while the daemon waits `WAIT` (2 s). 3. Each call is cancelled after 1 s and the handler future is dropped inside `timeout_at`. The paused VM's `Report.asked` keeps that call's `Asked`, about one entry per poll, without bound, until the VM answers or disconnects.

**Why the tests stay green.** `asks_given_up_on_are_forgotten` exercises only the timeout path: `take_stats` with a 10 ms wait, run to completion. No test drops a `take` future before its deadline.

**Backlog.** Related. TASK-198 (Done) fixed the same leak for a take that times out, never for one cancelled before its deadline.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When a GetStats handler is dropped before its wait ends (a client deadline shorter than the wait, a disconnect or a cancel), every ask it made is forgotten: with a VM process whose guest never answers, three takes aborted after 10 ms leave asks_waiting() == [0].
- [ ] #2 A cladefsd stats test connects a silent VM process (on_stats_asked(|_| true)), aborts stats::take(5 s) three times after 10 ms, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
