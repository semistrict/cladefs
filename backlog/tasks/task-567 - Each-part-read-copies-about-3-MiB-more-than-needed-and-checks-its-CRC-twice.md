---
id: TASK-567
title: Each part read copies about 3 MiB more than needed and checks its CRC twice
status: To Do
assignee: []
created_date: '2026-11-18 15:27'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 958000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-read review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/fetch.rs`: `rg -n -F 'Self { part, bytes: bytes.into(), unpacked: OnceLock::new() }' crates/bucket_cell/src/fetch.rs`

A part fetched from the bucket is copied out of object_store's Bytes by `get_range` (object_bucket.rs `Ok(bytes.to_vec())`). Writing it to the part cache bounces it through an aligned `direct::Buffer` copy, because the Vec is not block-aligned. A part read from the part cache costs a zero-filled `vec![0; len]` (`Files::read`), a direct read into a bounce buffer, and a copy into that Vec. Either way, `Fetched::new` turns the Vec into `Arc<[u8]>`, which allocates and copies the whole stored part again. `Fetched::unpacked` -> `Part::unpack` then zero-fills a new Vec and re-runs the CRC that `Fetching::fetch` or `Cache::part` already checked (extent_frame `let checked = self.stored.check(stored)?;`). It copies the stored bytes in (raw) or `to_vec`s them (zstd) before decrypting, and `unpacked.into()` (fetch.rs `Ok(unpacked) => unpacked.into(),`) copies the unpacked MiB into another Arc. For a 1 MiB encrypted part that is roughly 3 MiB of avoidable memcpy, 1 to 2 MiB of zeroing and a second CRC pass beside the one needed decryption. It happens on every RECENT miss: every cold page-in, and every read-ahead part a read later unpacks. Using Bytes or Arc<[u8]> built in place, or reading into an aligned buffer the Fetched keeps, would remove these.

**Trigger.** Any guest read past the extent cache: cladefs cache.rs StoredExtent::read -> HostFetcher::read -> Fetcher::read -> data -> Fetcher::part (RECENT miss) -> Fetching::part -> kept() or fetch() -> Fetched::new (copy) -> Fetched::unpacked -> Part::unpack (zero-fill, CRC again, copy, decrypt) -> into Arc (copy) -> data() copies the range out.

**Why the tests stay green.** The tests check bytes and request counts, not copies. `bench_reads_of_parts_in_memory_by_a_hosts_fetchers` measures RECENT hits only, `bench_reads_of_the_cache_on_a_runtime` is held to no bound, and no CPU-per-MiB budget covers cold reads.

**Backlog.** Related. TASK-92.19 (Done) fixed the same copies in extent_store's framed.rs, not bucket_cell::fetch; TASK-92.21 (Done) made each part unpack once but kept these copies and the second CRC; TASK-443.7 (To Do) is re-reads after RECENT eviction, not copies.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Reading N parts through a fetcher, from a warm part cache and from the bucket, allocates at most about 2 MiB per MiB delivered (the stored part plus the unpacked part), measured with a counting global allocator.
- [ ] #2 crc32 runs once per part per RECENT miss, counted behind a test feature in Stored::check.
- [ ] #3 A bucket_cell fetch test or bench holds both bounds and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
