---
id: TASK-613
title: A peer's part is kept on its CRC alone; one failing its tag is never refetched
status: To Do
assignee: []
created_date: '2026-11-18 15:33'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1004000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cross-host-take-restore review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/bucket_cell/src/fetch.rs`: `rg -n -F 'if bytes.len() as u64 == stored.len && stored.check(&bytes).is_ok() {' crates/bucket_cell/src/fetch.rs`

Fetching::fetch accepts a peer's bytes once their length and CRC32 match the asker's tail. Fetching::part then stores them in the part cache (self.keep) and in `recent` before anything unpacks them. The AEAD/GMAC tag is checked only later, in Fetched::unpacked, and a failure there returns Error::Corrupt with no eviction and no fallback to the bucket. CRC32 is not authentication. A peer that serves bytes with a forged CRC (a compromised host, or anyone answering at a peer's address on the unauthenticated plain-TCP GetPart service) therefore poisons the asker: every later read of that part on the host fails Corrupt (EIO to guests) until the LRU evicts it. If the asker later becomes the part's owner, it serves the poisoned bytes on to other hosts. This contradicts bucket_cell::peer's stated property that 'a part that fails it ... is fetched from the bucket: a peer is trusted with no more than the bucket's latency'. Keyed readers never use forged plaintext; the effect is a persistent read failure.

**Trigger.** Host X serves GetPart for (key, part) with bytes of the right length whose CRC32 is forged to match the table entry. Asker A: Fetching::fetch accepts them, keep() writes them to A's part cache, and the unpack's tag check fails, so A's read returns Corrupt. Every later read of that extent on A finds the cached part (Cache::part re-checks only the CRC), fails again, and never asks the bucket.

**Why the tests stay green.** Peer tests (cladefsd/peers/tests, bucket/peers/tests) inject parts that fail their CRC, which is handled by falling back to the bucket. None gives a CRC-valid part with a wrong tag.

**Backlog.** Related to TASK-406.2 (Done), which promises only the fallback to the bucket on a CRC failure, and TASK-407.4 (Done), which made CRC failures Corrupt. Neither covers a part whose CRC passes and whose tag fails.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A part from a peer whose CRC32 matches but whose tag fails is not kept in the part cache. The read succeeds by fetching the part from the bucket.
- [ ] #2 Later reads of that part on the host, and peers asking this host for it, get the bucket's bytes, not the peer's.
- [ ] #3 A fetch test in crates/bucket_cell/src/fetch.rs uses a fake Peer that returns the part with one byte flipped and four bytes adjusted to keep the CRC32. It asserts that the read succeeds from the bucket and that the part cache does not hold the peer's bytes. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
