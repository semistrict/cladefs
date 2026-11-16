---
id: TASK-411.23
title: >-
  Sealed stores fail fxfs-guest-sandboxes' writes budget: 1.87-1.90 times the
  guests' bytes against 1.5, never framing in place
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 05:57'
updated_date: '2026-11-15 14:26'
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
Found by TASK-411.15 (ubu1, 2026-11-15, prebuilt artifacts of 04d5b312, scripts/sandboxes.sh short): with the run's stores sealed (test-e2e.sh's CLADEFS_KMS), fxfs-guest-sandboxes fails its check 'writes: the stores wrote 1.90 times what the guests did, past 1.5' (and 1.87 on a second run), and on the first run also 'framings: host-a framed 1 segments slower than 1000 ms, the slowest 1093.9 ms; host-b 2, the slowest 1080.6 ms'. The same artifacts with the stores in the clear (a KMS holding no KEK) pass: 1.16 times. Cause: doc-16 (What TASK-411.5 made, Framing) has sealed stores never frame in place (segments_framed_in_place=0 against 55-58 in the clear), so every segment is written twice, its XTS raw file then its GCM-sealed framed copy: written_framed_bytes 3.7-3.8 GB per host sealed against 0.32-0.36 GB clear, for about 5 GB raw. TASK-421 (framings read their raw file back) is the read side of the same choice. scripts/qualify.sh runs this case (long mode), so a qualification of an encrypted build fails its sandboxes part until this is settled. Either the budget is a clear store's and sealed stores get their own, said why, or sealed segments' framing stops rewriting what was written (e.g. sealing parts as they fill, TASK-421's packing ahead, so the framed copy is written once and the raw file's pages drop). Main (no encryption) passes; the e2e suite (--tag e2e) passes sealed, its daemons' budgets (scripts/budgets.py) holding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 fxfs-guest-sandboxes passes sealed on ubu1 (scripts/sandboxes.sh short, then long under scripts/qualify.sh), its writes and framings checks holding, or its budgets for sealed stores changed with the measured reason
- [x] #2 Write amplification of sealed and clear stores measured side by side on the same build and recorded in doc-16
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_frame FORMAT 2 gains a seal kind 3, Seal::Sectors { key, salt } (Keyed::Sectors): an object framed in place from a sealed raw file, every part stored raw as the raw file's AES-256-XTS sectors, tweak = the sector's place in the object (= in the raw file), under Xts::derive(dek, salt, "cladefs raw file") (the raw file's own key, the trailer naming its DEK and salt); each part's tag (the entry's 16 bytes) HMAC-SHA256 truncated to 128 bits under dek.derive(salt, ["cladefs raw part mac"]) of (index, encoding, lengths) || stored ciphertext (encrypt-then-MAC, no nonce); footer/table MACed as any keyed object. Readers holding the DEK check CRC, then tag, then decrypt; keyless readers check CRCs. Parts of a Sectors object must be raw and whole sectors (refused as corrupt otherwise). One object never mixes XTS-raw and GCM parts: an in-place object is all raw, as in the clear.
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

Implemented (02302b951, branch t3/sealed-in-place): extent_frame FORMAT 2 seal kind 3 Seal::Sectors (raw parts = the raw file's XTS sectors under its own key, trailer naming DEK + raw file salt; per-part tag = HMAC-SHA256/128 under dek.derive(salt, 'fxdax raw part tag') of aad||ciphertext; table entries unchanged). Raw files' Sealer holds ObjectKey::sectors (one derivation). frame_segment frames in place sealed exactly as in the clear; seal_own (large extents) likewise, single pass (a sealed extent whose raw prefix precedes a compressible part is framed anew whole by write_own). TASK-421 settled: sealed segments look ahead (CRC+tag of ciphertext, look at plaintext) and hold parts packed in the clear in memory, sealed at framing (PackedPart::sealed, byte-identical to packing under the key). Mac: six crates' suite 1044/1045 then the one failure (page-cache read count: seal_own read compressible large extents twice) fixed by the single-pass rewrite; extent_frame+extent_store re-run 277/277. Write amplification (extent_store upload::encrypted::sealed_stores_frame_in_place_writing_what_clear_stores_write): sealed raw/copied/framed/own bytes == clear, byte for byte. mutants: framed-in-copies and large-extents-framed-in-copies regenerated (22 checked, 0 broken). Filed TASK-434 (pre-existing: sampled in-place framing on a Dir without link writes a copy without data).

Verification. Mac (Apple M5 Pro): cargo nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p sqlite_extents -p fxdax (not extent_store sim::) 1045/1045 (extent_keys 70, extent_frame 37, extent_remote 318, extent_store 240, sqlite_extents 23, fxdax 357); shuttle (-p extent_store --features shuttle --test shuttle) 37/37; sim::one 10 seeds x 23 profiles pass; mutants.sh --check 22/0 broken; firecracker-cargo.sh --check, upstream-changes.py --check pass; clippy: no new warnings in touched code; fmt clean for touched crates (extent_remote/fxdax have pre-existing unformatted files, untouched). store-bench framing (release, 16 x 32 MiB, 4 stores): sealed == clear written: noise 1.00x (16/16 in place), half 1.25x, text 1.50x. ubu1 02302b95 scripts/sandboxes.sh short: SEALED case OK, ratio 1.21 (host-a 1.22, host-b 1.21; budget 1.5; was 1.90/1.87), in place 66/63 (was 0), slowest framings 111.0/129.2 ms (budget 1000; was 1093.9), over_budget 0; CLEAR twin same build (KMS without KEK) case OK 1.17 (1.17/1.18), in place 58/59, slowest 122.9/231.6 ms. sandboxes.sh exits 1 sealed because the run-end bucket check finds 3 tenant-0 cells of the run's first branches (not the case daemons') with no KEK: filed TASK-411.26. Long mode / qualify.sh not run (AC #1 open). DoD #1 Linux workspace nextest not run.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Sealed stores frame in place (the user's decision): extent_frame FORMAT 2 seal kind 3 (Seal::Sectors), parts = the sealed raw file's AES-256-XTS sectors as they are under the raw file's own key (trailer: DEK id + raw file salt), each tagged with HMAC-SHA256/128 of aad||ciphertext (encrypt-then-MAC, no nonce, so retagging is safe); footer/table MACed as before. Segments and large extents frame in place sealed exactly when clear ones do; no object mixes XTS and GCM parts. TASK-421 settled: sealed segments look ahead (CRC+tag of ciphertext, look at plaintext) and hold parts packed in the clear in memory, sealed under the framed object's key at framing. Write-once argument for XTS sectors and the composition in doc-16 (What TASK-411.23 made). Verified: Mac 1045/1045 + shuttle 37/37 + sim sample; sealed bytes written == clear byte for byte (extent_store test, store-bench); ubu1 sandboxes short sealed 1.21x (was 1.90x, budget 1.5), framings <= 129 ms, 66/63 in place; clear twin 1.17x. Open: AC #1 needs sandboxes.sh to exit 0 (TASK-411.26: three clear tenant-0 cells at the run's end) and the long mode under qualify.sh.
<!-- SECTION:FINAL_SUMMARY:END -->
