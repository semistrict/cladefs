---
id: TASK-564
title: >-
  A sparse tar entry with a huge declared size makes an import scan zeros
  forever
status: To Do
assignee: []
created_date: '2026-11-18 15:26'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 955000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-facade review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/import.rs`: `rg -n -F 'let size = entry.size();' crates/cladefs/src/import.rs`

The importer accepts GNUSparse entries and copies them with `write_contents`. That function loops `while offset < size` over `entry.size()`, reading 1 MiB chunks and scanning each byte in `data_runs` to find runs of zeros. For a GNU sparse entry, tar 0.4.46 (`parse_sparse_header`) sets `entry.size` to the header's real size. The holes before each sparse block become `io::repeat(0).take(off - cur)`. Offsets and the real size are 12-byte numeric fields that accept base-256 binary, so up to about 2^63. A last sparse block with a non-empty but zero numbytes field, at offset 2^60, describes a file of 2^60 zero bytes in about 1.5 KB of tar, with no data bytes at all. Nothing writes for runs of zeros, so the store's capacity never stops the loop. Nothing checks the size against the capacity or a maximum file size. The loop never awaits while the data is all zeros. So the import scans 2^60 zeros at a few GB/s, effectively forever. It runs on a spawn_blocking thread inside `futures::executor::block_on`, holding the daemon's hold on the store path, and cancelling the gRPC call cannot stop it. Each crafted import, or a caller's retry of it, permanently costs a core and a blocking thread until cladefsd restarts. Regular entries stay bounded by the bytes actually in the layer (deflate is at most about 1000:1). Sparse entries are the one case with no bound tied to input size.

**Trigger.** 1) A tenant's image layer (`docker export`-style tar or OCI layer) holds one GNU sparse entry ('S') with sparse map [{offset: 2^60 in base-256, numbytes: "00000000000"}], realsize 2^60, header size 0. 2) ImportImage -> make_store -> fill -> Importer::apply_layer -> entry -> dir.create, then write_contents. 3) `entry.size()` is 2^60. The loop reads 1 MiB of repeat(0) per iteration and data_runs finds no data. About 2^40 iterations follow, and the call never returns.

**Why the tests stay green.** No import test or test layer builder makes a sparse entry: rg -i sparse over crates/cladefs/tests/import.rs and crates/cladefs/src/import/testing.rs finds nothing. The existing size tests use regular entries, which are bounded by the archive's bytes.

**Backlog.** No related task covers it: TASK-79.56 and TASK-79.88 (Done) handle crafted images' aborts and errnos, never sparse entries, declared sizes or unbounded time.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Importing a layer with a GNU sparse entry whose declared real size passes the store's capacity or Fxfs's maximum file size (such as 2^50 or 2^60, with a zero-length last block) fails within a few seconds with EINVAL and leaves the store path empty.
- [ ] #2 An import's time for a sparse entry is bounded by the archive's bytes and its declared data blocks, not by its declared size: no hole is scanned byte by byte.
- [ ] #3 A regression test in cladefs's tests/import.rs builds a tar by hand with one GNUSparse header mapping a 0-length block at 1<<50 with realsize 1<<50, and asserts cladefs::import::store returns EINVAL quickly with the store path empty; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
