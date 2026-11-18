---
id: TASK-568
title: >-
  A fork's checkpoint reads the parent's state without keys, then authenticates
  it
status: To Do
assignee: []
created_date: '2026-11-18 15:27'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 959000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-write review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/recovery.rs`: `rg -n -F 'let checkpoints = Checkpoints::new(bucket, 0);' crates/bucket_cell/src/recovery.rs`

`recovery::checkpoint_at` writes the checkpoint at a version's segment. Every restore of a version into another cell runs it (`lifecycle::finish_fork`, which calls it whenever `cell != child`). It already holds the parent's ring (`keys.ring(bucket, cell)` a few lines up), but it builds `Checkpoints::new(bucket, 0)` without `.holding(&keys)`, so `state_at` reads with the empty ring:
- In `walk`, `newest_to` picks a checkpoint by key name alone, and `checkpoints.read` decodes it with `KEYLESS.of(cell.id)`, whose `encrypted()` is false. That checks CRCs only, no MAC.
- `segment_footer` skips `tail.open` because `keys.is_empty()`. Footers are checked by CRC and by `placed` (cell, segment, epoch), never by MAC.

The state built that way is then encoded with `crate::checkpoint::encode(&state, keys.encrypting())`, which authenticates it under the parent's real DEK at key `(at, epoch_of(at))`. The child's recovery reads exactly that key with a ring holding the parent's DEKs (`state_at` by key, `decode_under`). The MAC checks out, so the child accepts the forged extent map.

doc-16 says only that a bucket writer may tamper with what the *keyless collector* reads, which is "no new power". Key-holding readers must refuse forged metadata, and `objects_authenticated_under_keys_unheld_are_refused_by_every_ring_holding_keys` tests that. This path launders CRC-only data into authenticated data.

The keyless decode also lands in the host-wide `Decoded` cache (`decoded.keep`). `Checkpoints::read_key` returns a cached state from `decoded.get(&key)` before any MAC check, so later keyed recoveries on this host that list and pick the forged key also accept it.

For a shared version restored into another tenant's cell, the child inherits every DEK generation of the parent cell (`keys::inherit`; `add_generation` is test-only, so the DEK never changes). A forged map can therefore point the child's extents at the parent's later, unshared segments, and the child's guest reads them in plaintext.

**Trigger.** 1. Tenant A's cell C cuts version V at segment S and shares it. A version-copy store of V is opened on host H. Its keyed `recover_version` succeeds.
2. Someone with write access to the bucket but no keys does either of these:
   - creates `checkpoints/<E>.<S'>` (S' ≤ S, sorting newest) holding a CRC-valid checkpoint framed under a made-up key, whose map points extent X at a later segment of C;
   - or overwrites a segment in (newest checkpoint, S] with a CRC-valid footer.
3. Tenant B's VM manager forks V into B's cell (`Uploader::forked` → `fork_version_keyed` → `finish_fork` → `checkpoint_at`). The same happens on a resumed fork (`resume_version_fork` with `Parents::Kms`).
4. `checkpoint_at` reads the forged object with no keys, builds the state, and creates `checkpoints/<epoch_of(S)>.<S>` authenticated under C's DEK.
5. B's child store opens. `state_at` reads that key by key, the MAC verifies, and B's guest reads extent X from C's post-version data, decrypted with the inherited DEK.

**Why the tests stay green.** No test tampers with a parent's checkpoint or segment before a restore into another cell. The tampering test only exercises `checkpoint::decode_under` directly. The copy store's keyed `recover_version` happens to refuse tampering that was already there when it opened, so happy-path tests never reach `checkpoint_at` with forged input. `forks_checkpoint_their_own_states` only covers another cell's checkpoint placed at the child's own key.

**Backlog.** No related task covers it: TASK-411.4 (Done) encrypted checkpoint_at's output under the parent's DEK but left its keyless read; TASK-411.28 and TASK-411.3 (Done) set the refusal rules this path gets around.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Writing the checkpoint at a version's segment for a fork into another cell reads the parent's checkpoints and segment footers with the parent's ring, and refuses (NoKey or Corrupt) one framed under a key the ring lacks, writing no object at checkpoint(cell, S, epoch); the child never opens on a forged extent map.
- [ ] #2 A state decoded without keys never lands in the host's Decoded cache where a keyed read could take it without a MAC check.
- [ ] #3 A regression test in bucket_cell's recovery.rs cuts a version at segment S, plants a checkpoint at S-1 encrypted under Secret::random() with a made-up KeyId mapping an extent elsewhere, and asserts checkpoint_at(bucket, cell, S, Parents::Held(&writer_ring)) fails and writes nothing; a second case overwrites a segment with a CRC-valid footer framed under another key; both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
