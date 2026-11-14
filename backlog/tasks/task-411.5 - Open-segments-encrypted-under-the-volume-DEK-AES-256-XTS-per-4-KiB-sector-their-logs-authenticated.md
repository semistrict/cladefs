---
id: TASK-411.5
title: >-
  Open segments encrypted under the volume DEK (AES-256-XTS per 4 KiB sector);
  their logs authenticated
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 23:33'
labels: []
dependencies:
  - TASK-411.8
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 802000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The open segment holds raw guest writes on local disk until it is framed, read back by offset meanwhile. The user chose to encrypt it rather than rely on host disk encryption, so that shredding covers hosts' disks too (doc-16). Every write reaching it is whole 4 KiB sectors since TASK-411.8, so XTS needs no read-modify-write. TASK-411.8 found three more places plaintext reaches local disk or bypasses the store: Write::file() hands callers the open segment's raw file directly (fxdax storage/image.rs write_file, which may retry after a short write at an unaligned offset; the shuttle tests); frame_in_place writes a closing segment's tail into its raw file; and large extents' own raw files (open-extents/) hold guest data in the clear until sealed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An open segment file holds no plaintext of written data: a test writes known content and searches the file
- [ ] #2 Reads of sealed extents from the open segment and its framing on close give the written data
- [ ] #3 Recovery after a crash with an open segment gives the same state as before
- [ ] #4 Large extents' raw files (open-extents/) hold no plaintext of written data either
- [ ] #5 No caller writes an open segment's raw file directly: Write::file() is gone or encrypts, and image.rs's write_file retries only whole sectors
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_keys::Xts: AES-256-XTS (IEEE 1619-2007, no ciphertext stealing) over storage_xts (BSD-3-Clause) and the aes crate (MIT/Apache-2.0), both already in the workspace; data units any multiple of 16 bytes, the tweak the unit's number; key schedules zeroized. KATs: IEEE 1619 vectors 10 and 11 (512-byte units), as aws-lc's xts_test.cc carries them, and a 4 KiB sector checked against OpenSSL 4.0.3's enc -aes-256-xts.
2. extent_store::raw: RawFile<F>, a File: clear, its bytes as written (today's); or sealed by a Sealer (KeyId, a random salt, AES-256-XTS under dek.derive(salt, ...) per 4 KiB sector, tweak the sector; a MAC key for its log): writes of whole sectors encrypted through a buffer, reads decrypted (partial sectors read whole). Open segments' and large extents' raw files are RawFiles, sealed iff the store's ring seals (keys.sealing()); large extents' salts live in memory only (their files never outlive the process: opening removes them). Sealed stores refuse writes to large extents that are not whole sectors too.
3. Sealed logs: header FXLOG v3 (salt, segment, KeyId, data salt, HMAC of the header); each record's payload followed by an HMAC of its offset and payload. Runs a record holds or checks are the raw file's stored bytes, so recovery stays keyless (salted hashes, CRCs); the closings TASK-411.9 defers to Recovering::open check the header's and records' MACs and decrypt before sealing into FORMAT 2. Clear logs unchanged.
4. No caller writes a raw file directly: Write::file() goes; Write::write(buf, at) encrypts; Write::stored(data) hands a caller doing its own I/O (fxdax image.rs) the file, offset and encrypted bytes, which image.rs writes, retrying from the last whole sector after a short write. Part::Data carries the RawFile: read_range and decrypt for image.rs's reads. Segments, framing and LogRecord::of read through RawFile (decrypting) or its stored bytes.
5. frame_in_place: sealed stores never frame in place, segments nor large extents (as TASK-411.3 already decided for segments): a sealed object's bytes are AES-GCM parts, not the XTS sectors in the raw file, so in place would rewrite every byte anyway, and a crash part way would leave a file neither raw nor framed. RawFile::clear() hands the inner file only when clear, which in-place framing takes.
6. Tests: known data searched in a sealed store's open segment raw file, log and large extent raw file (absent) and found in the clear twin; reads from the open segment and after framing; crash and reopen gives the same state (and TASK-411.9's no-plaintext crash test extended to raw files and logs); tampered header/record refused naming the segment; misaligned writes to sealed large extents refused; clear stores' raw files and logs byte-identical to the written data/old format. Shuttle tests and image.rs tests use the new Write API.
7. Measure on the Mac: store-bench writes (MiB/s into an open segment) and framing, sealed vs clear (FXDAX_BENCH_SEALED); numbers in the notes.
8. doc-16 and doc-14 describe the formats.
<!-- SECTION:PLAN:END -->
