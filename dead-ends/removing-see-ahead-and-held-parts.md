# Removing `see_ahead` and `HeldParts` now that `PageCache` buffers raw segments

**Tried:** 2026-11-22, board 6029, `ubu1` (`hotbed run --node ubu1 --bench`, job `bktns3opis`, base commit `e82cb2ec2`).
**Idea:** `see_ahead` + `HeldParts` (`TASK-400`, `TASK-421`) was originally added when raw segments fell out of the host OS page cache before segment close, forcing synchronous disk reads at close time. Now that `PageCache` (`TASK-433`) keeps recently written raw segment pages in governed RAM and Wave 1 unified `Unpacked`/`Recent` under the governor, removing `see_ahead` + `HeldParts` (~500 lines and the ungoverned `AHEAD` worker pool) might be neutral or faster.
**Result:** Interleaved A/B on `ubu1` (`--bench`, 4 rounds, round 1 cold dropped, medians over rounds 2–4) comparing A (current `see_ahead` + `HeldParts`) vs B (`CLADEFS_NO_SEE_AHEAD=1`, disabling `Ahead` in `Segment::new`):

| Case (`cell-bench`, 32 MiB segments, encrypted) | Metric | A (`see_ahead` + `HeldParts`) | B (disabled) | B / A |
|---|---|---:|---:|---:|
| `framing 16` (`text`, 4 stores, 0 hogs) | avg framing ms | 27.7 | 74.1 | **2.68×** |
| | slowest framing ms | 44.4 (0 > 50 ms) | 106.2 (16/16 > 50 ms) | **2.39×** |
| `framing 16` (`noise`, 4 stores, 0 hogs) | avg framing ms | 0.2 | 31.1 | **155.5×** |
| | slowest framing ms | 0.9 | 39.2 | **43.6×** |
| `framing 16` (`half`, 4 stores, 0 hogs) | avg framing ms | 10.9 | 55.0 | **5.05×** |
| | slowest framing ms | 32.6 (0 > 50 ms) | 90.5 (8/16 > 50 ms) | **2.78×** |
| `framing 16` (`text`, 4 stores, 8 hogs) | avg framing ms | 138.5 | 179.8 | **1.30×** |
| | slowest framing ms | 246.7 | 287.7 | **1.17×** |
| `burst 256` (3 segments) | framed ms | 33.5 | 267.5 | **7.99×** |

**Why it's dead:** `PageCache` eliminates the disk read, not the CPU work. `see_ahead` pipelines XTS decryption, CRC32C, HMAC-SHA256 part tagging, compressibility sampling, and compression behind writes as each 1 MiB part finishes. Without it, all 32 parts of a closing segment must be XTS-decrypted, hashed/tagged, sampled, and compressed synchronously at close time — increasing close-path framing latency by 2.7× on compressible data, 5.0× on mixed data, 155× on incompressible/in-place data (0.2 ms → 31.1 ms), and 8.0× on `burst` (33.5 ms → 267.5 ms).
**Reopen if:** Segment framing moves off the commit/upload latency path completely (e.g., uploading raw XTS sectors or streaming parts incrementally during writes rather than framing whole 32 MiB segments at close), or hardware/algorithmic changes make framing 32 MiB synchronously take < 5 ms.
