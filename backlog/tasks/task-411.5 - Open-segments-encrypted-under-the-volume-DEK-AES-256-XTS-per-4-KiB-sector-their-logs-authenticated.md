---
id: TASK-411.5
title: >-
  Open segments encrypted under the volume DEK (AES-256-XTS per 4 KiB sector);
  their logs authenticated
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-15 00:34'
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
- [x] #1 An open segment file holds no plaintext of written data: a test writes known content and searches the file
- [x] #2 Reads of sealed extents from the open segment and its framing on close give the written data
- [x] #3 Recovery after a crash with an open segment gives the same state as before
- [x] #4 Large extents' raw files (open-extents/) hold no plaintext of written data either
- [x] #5 No caller writes an open segment's raw file directly: Write::file() is gone or encrypts, and image.rs's write_file retries only whole sectors
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
XTS primitive: extent_keys::xts::Xts, AES-256-XTS (IEEE 1619-2007, no CTS) over storage_xts (Fuchsia, BSD-3-Clause) and aes 0.9 (RustCrypto, MIT OR Apache-2.0, feature zeroize: key schedules zeroed on drop); both already in the workspace. Xts::derive(key, salt, info) = Xts::new(HKDF(key, salt, [info, "data"]), HKDF(key, salt, [info, "tweak"])). Unaligned units go through a stack buffer. KATs: IEEE 1619 vectors 10 and 11 (512-byte units, DUSN 0xff/0xffff) copied from aws-lc's crypto/fipsmodule/modes/xts_test.cc (ISC; from OpenSSL 1.1.1d) by a script reading that file, not by hand; and a cross-check against aws-lc's own EVP_aes_256_xts on 16 rounds of random keys, 3 random 4 KiB sectors at random places up to 2^63 (dev-dependency aws-lc-sys 0.46.0, the version aws-lc-rs already builds, so no new crate in Cargo.lock; licenses ISC/Apache-2.0/MIT/BSD-3-Clause). OpenSSL 4.0.3's enc has no XTS (refuses), so not used.
Formats: raw files (open-segments/, open-extents/) of a sealed store (keys.sealing() is Some) are XTS per 4 KiB sector, tweak = offset/4096, key Xts::derive(newest DEK, random 32-byte salt, "fxdax raw file"); clear stores' unchanged. Open segment logs of sealed stores: magic FXLOG\0\0\x03; after salt/branch/number (u64 LE each): DEK branch u64, generation u32, the 32-byte salt, HMAC-SHA256 of the 108-32 header bytes under dek.derive(salt, ["fxdax raw file log mac"]); each record = salted-hash frame of (bincode(LogRecord) || HMAC(offset_le || bincode)). Runs::Held/Checked hold/CRC the raw file's stored (encrypted) bytes, so recovery stays keyless. Clear logs unchanged (FXLOG\0\0\x02, same record bytes). LOG_HEADER is now the sealed header's length (108), what header readers read; a clear log's header block is 4096 bytes so reading 108 is safe.
Code: extent_store::raw::{RawFile, Sealer, SECTOR}: RawFile<F> implements File (read_at/read_cached decrypt covering sectors, write_at encrypts through an Aligned buffer, refuses partial sectors with InvalidInput); file() = stored bytes (log runs, syncs, write-back, callers' own I/O); clear() = inner file only when clear (in-place framing). Segments phases, Placement::Own, Source::Raw, Part::Data hold RawFile; Log/Preparation/Prepared/Placed carry the Sealer (prepared logs' headers written off-lock with it); SegmentLog::append takes LogRecords and MACs them; LogHeader::of/sealer (DEK lookup by KeyId, header MAC); LogRecord::decode checks record MACs when given a sealer; Recovered::close reads with keys (verifies), decrypts data[..end] (end must be whole sectors), seals.
Decisions: frame_in_place: sealed stores never frame in place (segments nor large extents): the raw file holds XTS sectors, the object must hold GCM parts and tags; in place would rewrite every byte anyway and a crash midway would leave a file neither raw nor framed. Enforced by RawFile::clear() being the only way to the writable inner file for in-place paths (frame_segment, seal_own). Write::file()/offset() removed; Write::write(buf, at) and Write::stored(Aligned) -> Stored{file, offset, bytes} (encrypted) replace them; image.rs encrypts once via stored() and on a short write resumes from the last whole sector written (bails if a retry would not advance a sector), so retries are whole sectors of the same ciphertext; image.rs reads Part::Data by read_range + decrypt. Sealed stores refuse unsectored writes to large extents (Unsectored), clear ones unchanged.
Measurements (macOS, Apple M5 Pro, 15 CPUs, release store-bench, HostDir in /var temp; FXDAX_BENCH_SEALED=1 sealed / 0 clear twin, both through uploaders): writes 512 MiB as 1 MiB extents: clear 2811, 3621, 2886, 2823 MiB/s; sealed 1953, 1772, 1936, 1800 MiB/s. Reads of them while open: clear 2299-2716, sealed 1574-1690 MiB/s. 4 KiB extents (64 MiB): writes clear 209 / sealed 195 MiB/s; reads 86 / 67 MiB/s. Commit after: 136-154 ms both. Framing 32 MiB segments, 4 stores at once (store-bench framing 32 and 64): text data each 75.5/65.9 ms clear vs 71.5/70.4 ms sealed (~450-490 MiB/s per framing either way; read phase dominates on macOS's F_NOCACHE); noise 41.0/38.8 ms clear (in place) vs 83.2/84.9 ms sealed (in a copy: write phase 5-7 ms -> 31-36 ms). XTS costs ~1/3 of write throughput on this Mac (storage_xts does one AES block at a time; an out-of-place, 8-block-parallel XTS could narrow it). Linux numbers are the user's.

Tests: extent_keys xts::tests (IEEE 1619 vectors 10, 11; aws-lc cross-check; unaligned; derivation); extent_store raw::tests::sealed_raw_files_store_ciphertext_and_read_plaintext; upload::encrypted::sealed_stores_raw_files_and_logs_hold_no_plaintext (AC #1, #2, #4: small extent in the open segment, 4 MiB extent in open-extents/, written/committed/framed: no plaintext in any file, reads back; clear twin holds the data verbatim, logs FXLOG v2), writes_stored_by_callers_are_sealed_and_large_extents_take_whole_sectors (AC #5: Write::stored encrypts; sealed 512-byte large-extent write refused Unsectored, clear accepted), sealed_logs_changed_are_refused (record payload changed with its hash redone: 'its record at 4096 is not authentic'; header salt changed: 'its header is not authentic'; generation changed: no such DEK; nothing framed), segments_closed_again_are_never_in_the_clear (AC #3: crash with a durable commit in the open segment, reopened under fail_after(n) for every n; now with no exclusions; reads back); fxdax storage::image::tests::sealed_images_hold_no_plaintext (image device on a sealed cell: write_file via Write::stored, Part::Data reads decrypted, after flush and reopening, no plaintext in the image dir). Mutation checks: raw files unsealed -> segments_closed_again fails on open-segments/ and logs/.
Validation (macOS): cargo nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p sqlite_extents -p fxdax -E 'not (package(extent_store) & test(/^sim::/))': 963/963 passed (extent_keys 27, extent_frame 33, extent_remote 312, extent_store 219, sqlite_extents 22, fxdax 350). Shuttle (extent_store --features shuttle, binary(shuttle)): 37/37. sim::one SIM_SEEDS=10 for everything, forks, takeovers, crashes, wals, forked_wals, restores, fresh_cells, collectors, failed: all pass. clippy --all-targets (and --features shuttle): no warnings on changed lines. cargo fmt --check clean. scripts/firecracker-cargo.sh --lock updated vendor/firecracker.lock (--check passes); scripts/upstream-changes.py --check passes. Linux not run (DoD #1's ubu1 half is the user's).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A sealed store's open segments' and large extents' raw files are encrypted with AES-256-XTS per 4 KiB sector (tweak the sector) under a key derived from the branch's newest DEK and a random salt per file (extent_keys::xts over storage_xts + aes; KATs: IEEE 1619 vectors 10/11 and aws-lc cross-check); open segments' logs of sealed stores carry a MACed header (DEK id, salt) and a MAC per record, their held/checked runs being the stored ciphertext so recovery stays keyless; Recovering::open verifies MACs and decrypts before sealing closings into FORMAT 2. RawFile wraps every raw file; Write::file() is gone (Write::write, Write::stored), image.rs encrypts once and retries whole sectors; sealed stores never frame in place and refuse unsectored large-extent writes. Clear stores unchanged in bytes and behaviour. Verified by 963 macOS tests incl. new sealed/clear-twin, tamper and crash tests, 37 shuttle tests, a 10-seed sim sample over 10 profiles; Mac numbers in the notes (sealed writes ~1.9 vs ~2.9 GB/s clear; framing text ~equal, noise 2x slower sealed as it can't frame in place).
<!-- SECTION:FINAL_SUMMARY:END -->
