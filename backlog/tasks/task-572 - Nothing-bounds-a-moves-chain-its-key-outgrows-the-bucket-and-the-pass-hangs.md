---
id: TASK-572
title: 'Nothing bounds a moves chain: its key outgrows the bucket and the pass hangs'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:27'
updated_date: '2026-11-19 07:06'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 963000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), collector review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/collector.rs`: `rg -n -F 'Kind::Moves { .. } => true,' crates/bucket_cell/src/collector.rs`

Pass::compactable treats every named moves object with no moves of its own as compactable, and compact_object names the new moves `layout::moves(key, pass)`. That key is moves_of(key) plus the pass number, so it embeds the whole chain: 78 + 20*k bytes at depth k for a segment key, more under a URL prefix. Only the live-fraction arithmetic bounds the depth, and nothing enforces moves::MOVES (64), which the code describes as how many times an extent may have moved. At the default 0.5, the bound is about log2(32 MiB / extent size), roughly 14. At 0.9, which local_cell's simulation uses, the bound is about 85. At 1.0, which local_cell's encrypted.rs tests use, one more level is added each time a pass sees a co-located extent die. Above 1.0, which parse_collect accepts unchecked, every pass compacts every fully live moves object again. At depth 48 the key passes the 1024-byte limit of S3, GCS and Azure (sooner on macOS file buckets, whose PATH_MAX is 1024). object_store reports the 400 as Generic, error() maps it to Unavailable, and bucket::create retries forever. The pass never ends, the sweep's buffer_unordered never finishes, `--every` never runs another sweep, and once the lease expires each other process that takes the cell hangs the same way. A reader that starts deep in a chain also fails past 64 hops with Corrupt("moved too often") in fetch.rs, for example a fork whose checkpoint at the origin names an extent early in its chain.

**Trigger.** 1. cladefs-gc URL --compact-below 1.0 --every 300 (or 0.9) over a written cell in which one 4 KiB extent outlives the extents it shares a segment with. 2. Each pass that sees a co-located extent freed compacts the moves object holding it: Pass::compactable, Kind::Moves => true, below() is true, compact_object writes moves of moves at a key 20 bytes longer. 3. After about 48 such passes, layout::moves() returns a key over 1024 bytes. bucket::create gets HTTP 400 from S3 (ENAMETOOLONG on a file bucket), error() maps it to Unavailable, and create loops forever. 4. The pass's future never completes, Sweep::over never returns, and all collection in the process stops. With --compact-below 1.5 this takes about 48 passes regardless of churn, because every live moves object is compacted every pass.

**Why the tests stay green.** No test compacts one chain more than two or three levels deep. The simulation runs at 0.9 but on SimBucket, which has no key-length limit, and over short soaks. fetch.rs's 64-hop limit is never exercised against collector-made chains. parse_collect's tests check only in-range fractions.

**Backlog.** RELATED: TASK-407.1 (Done) put the pass into moves keys, TASK-395.2 (Done) laid out moves of moves and TASK-195 (Done) fixed double compaction; TASK-314 (Done) accepts retrying transient bucket failures forever, not keys that can never succeed. None bounds chain depth or validates compact_below.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A moves chain's depth stays bounded (within moves::MOVES, 64 hops, and under a bucket's 1024-byte key limit): after 60 passes at --compact-below 1.0, each seeing one co-located extent freed, every pass returns, no moves key exceeds 1024 bytes, and the surviving extent reads through fetch from the oldest location any kept state names.
- [ ] #2 The collector refuses a --compact-below outside (0, 1] (such as 1.5) when it parses its options.
- [ ] #3 A regression test in bucket_cell's collector tests runs the 60-pass loop on a bucket wrapper that refuses keys over 1024 bytes, plus a parse test for 1.5, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
