---
id: TASK-549
title: Each VM restore closes the version store the last restore kept open
status: To Do
assignee: []
created_date: '2026-11-18 15:24'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 940000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), daemon-resources review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/cladefsd/cells.rs`: `rg -n -F 'let held = hold(&dir).await?;' crates/cladefs/src/cladefsd/cells.rs`

`fork_version` (and `at_version`) first call `opened_version(cell, version, capacity)`. That function takes the version directory with `hold(&dir)` and not `hold_to_fork(&dir)`. `hold()` is `hold_to_fork` followed by `close_kept(hold.take_kept()).await`. So every restore takes the version's kept store out of the table and closes it. While still holding the directory's hold, it waits for `closed`, which resolves only after every fork still using that store has finished: each one holds an `Arc<HeldCell>` inside `fork_kept`'s blocking closure. Only after that does it return, even though when the directory exists it returns at once (`if dir.exists() && !unmade { return Ok(dir) }`) and never needed the store closed. `fork_kept` then finds nothing kept, so `kept_or_open` reopens the version store from scratch (`take_store`, about a second or more against a bucket a region away, per the doc on `LINGER`) and keeps it, only for the next restore's `opened_version` to close it again. The 60 s `LINGER` therefore never serves a later restore. Restores that overlap queue behind each other's forks, which is the behaviour TASK-92.5 fixed and documented: "the k-th waiting for all those before it, a second or more each". Restores waiting for the directory's hold while another restore waits in `close_kept` are bounded by `CLOSING_WAIT` (10 s) and fail with `EBUSY` once the in-flight forks take longer than that. TASK-508's commit (9a65b3838) introduced `opened_version` with `hold()`. Its message says the version is "kept open for a zygote's restores as before" and notes "tests not run". Before that change, restores forked the copy without holding it.

**Trigger.** 1. A zygote's VM snapshot gives version V of cell C. 2. VM restore R1 sends `OPEN`/`ForkOf{C,V}`, which goes through `Connection::open_held`, `cells::fork_version` and `opened_version`, which holds `dir`. Nothing is kept yet, so it returns. Then `fork_kept` and `kept_or_open` run `take_store` (opening the version store) and `keep` it, and R1 forks. 3. Restore R2 arrives a moment later, while R1's fork is in flight or anywhere within the next 60 s. R2's `opened_version` calls `hold(&dir)`, which runs `take_kept()` and then `close_kept`: it waits for R1's fork to finish, then closes the store. 4. R2's `kept_or_open` finds nothing kept and reopens the version store, paying another open and close of about a second with a remote bucket. Every subsequent restore repeats this. Restores queued on `dir`'s hold behind one stuck in step 3 for more than 10 s fail with `EBUSY`, and their VMs fail to restore.

**Why the tests stay green.** `a_zygotes_restores_open_its_copy_once` starts all 24 restores at the same instant. They tend to pass `opened_version` before the first restore keeps the store, so they then share it through `kept_or_open`'s first branch. The test asserts only that every restore succeeded and that the version's cell head was not taken. It never counts how many times the version store was opened and never staggers restores. No test restores the same version twice in sequence and checks that the store stayed kept.

**Backlog.** Regresses TASK-92.5 (Done, High), which promised that restores of a store no connection holds open it once and keep it for the forks after. 9a65b3838 (TASK-508) added opened_version, whose hold() closes the kept store. TASK-443.6 (To Do, EBUSY while the first open holds the key) is a different defect.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A second VM restore of version V within LINGER of the first forks from the store the first restore kept. The version store is opened once, not once per restore.
- [ ] #2 Restores of one version spaced 200 ms apart, against a bucket with CLADEFS_BUCKET_LATENCY_MS=50, open the version store exactly once, and none fails with EBUSY.
- [ ] #3 A cladefsd test (crates/cladefs/src/cladefsd/tests.rs) restores V, waits for it to finish, restores V again within LINGER, and asserts the same kept store and one opening. A staggered variant of a_zygotes_restores_open_its_copy_once asserts the same. Both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
