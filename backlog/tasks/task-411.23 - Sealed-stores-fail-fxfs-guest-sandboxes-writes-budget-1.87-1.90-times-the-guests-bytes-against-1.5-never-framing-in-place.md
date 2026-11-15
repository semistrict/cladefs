---
id: TASK-411.23
title: >-
  Sealed stores fail fxfs-guest-sandboxes' writes budget: 1.87-1.90 times the
  guests' bytes against 1.5, never framing in place
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 05:57'
updated_date: '2026-11-15 09:55'
labels:
  - performance
  - encryption
dependencies: []
references:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-411.15 (ubu1, 2026-11-15, prebuilt artifacts of 04d5b312, scripts/sandboxes.sh short): with the run's stores sealed (test-e2e.sh's FXDAX_KMS), fxfs-guest-sandboxes fails its check 'writes: the stores wrote 1.90 times what the guests did, past 1.5' (and 1.87 on a second run), and on the first run also 'framings: host-a framed 1 segments slower than 1000 ms, the slowest 1093.9 ms; host-b 2, the slowest 1080.6 ms'. The same artifacts with the stores in the clear (a KMS holding no KEK) pass: 1.16 times. Cause: doc-16 (What TASK-411.5 made, Framing) has sealed stores never frame in place (segments_framed_in_place=0 against 55-58 in the clear), so every segment is written twice, its XTS raw file then its GCM-sealed framed copy: written_framed_bytes 3.7-3.8 GB per host sealed against 0.32-0.36 GB clear, for about 5 GB raw. TASK-421 (framings read their raw file back) is the read side of the same choice. scripts/qualify.sh runs this case (long mode), so a qualification of an encrypted build fails its sandboxes part until this is settled. Either the budget is a clear store's and sealed stores get their own, said why, or sealed segments' framing stops rewriting what was written (e.g. sealing parts as they fill, TASK-421's packing ahead, so the framed copy is written once and the raw file's pages drop). Main (no encryption) passes; the e2e suite (--tag e2e) passes sealed, its daemons' budgets (scripts/budgets.py) holding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 fxfs-guest-sandboxes passes sealed on ubu1 (scripts/sandboxes.sh short, then long under scripts/qualify.sh), its writes and framings checks holding, or its budgets for sealed stores changed with the measured reason
- [ ] #2 Write amplification of sealed and clear stores measured side by side on the same build and recorded in doc-16
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_frame FORMAT 2 gains a seal kind 3, Seal::Sectors { key, salt } (Keyed::Sectors): an object framed in place from a sealed raw file, every part stored raw as the raw file's AES-256-XTS sectors, tweak = the sector's place in the object (= in the raw file), under Xts::derive(dek, salt, "fxdax raw file") (the raw file's own key, the trailer naming its DEK and salt); each part's tag (the entry's 16 bytes) HMAC-SHA256 truncated to 128 bits under dek.derive(salt, ["fxdax raw part mac"]) of (index, encoding, lengths) || stored ciphertext (encrypt-then-MAC, no nonce); footer/table MACed as any keyed object. Readers holding the DEK check CRC, then tag, then decrypt; keyless readers check CRCs. Parts of a Sectors object must be raw and whole sectors (refused as corrupt otherwise). One object never mixes XTS-raw and GCM parts: an in-place object is all raw, as in the clear.
2. extent_keys: Secret::tag/verify_tag (truncated HMAC over several slices, constant-time compare).
3. extent_frame API: ObjectKey::sectors (shared by the raw file's Sealer, so one derivation), encrypt/decrypt_sectors, raw_part (CRC+tag of stored bytes); Packer::in_place(key) + push_raw_parts(RawPart); PackedPart::seal (a part packed ahead in the clear, sealed at framing); stores_raw (every part incompressible).
4. extent_store: Sealer holds the Sectors ObjectKey; RawFile::clear() goes for RawFile::sectors(); frame_segment frames in place sealed exactly as in the clear (look ahead or read: CRC+tag over the ciphertext read through the page cache, the look over its decryption); tail written in the clear after the data's sectors; large extents' seal_own: check every part stores raw (sealed: CRC+tag over ciphertext), then in place, else write_own in a copy.
5. TASK-421 (settled here): sealed segments get looks ahead (ciphertext for CRC/tag, plaintext for the look) and hold parts packed ahead in memory in the clear (memory holds plaintext anyway, doc-16), sealed under the framed object's key drawn at framing (PackedPart::seal), so a framing in a copy reads back only what was not held. No plaintext on disk.
6. Write-once argument for XTS sectors (segments append, reservations never overlap, raw files made anew, retries rewrite the same bytes; large extents' raw files may be rewritten before sealing, the standard XTS disk exposure on the host only, the bucket getting one version per salt) and the composition, in doc-16.
7. Tests: extent_frame (sectors objects read under their key, refused changed byte/tag/index/salt/no key, keyless check, restamp); extent_store (sealed incompressible segment framed in place, no copy, readable, no plaintext; compressible sealed in a copy; mixed; held parts sealed at framing read nothing back; large extents in place sealed; crash/recovery of in-place sealed framing; written bytes sealed ~ clear). store-bench sealed vs clear writes.
8. Verify: Mac nextest of the six crates, shuttle, sim sample, mutants --check, firecracker-cargo --check, upstream-changes --check, fmt, clippy; ubu1 scripts/sandboxes.sh short sealed (write ratio, framing times vs 1.5x / 1 s).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The user's decision (2026-11-15): frame in place sealed. Incompressible ('raw') parts of a sealed object keep the raw file's XTS sectors as they are, authenticated by an HMAC tag (encrypt-then-MAC), so in-place framing works sealed exactly as in the clear; compressible parts are compressed and GCM-sealed into a copy as now. A frame format change (no migration needed).
<!-- SECTION:NOTES:END -->
