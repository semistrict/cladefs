# FUSE_WRITEBACK_CACHE on fxdax's FUSE server

**Tried:** 2026-10-10, TASK-25.9 step B, measured on GCE (gce-fs-bench run T2151Z).
**Idea:** Only the guest changes an fxdax filesystem, so it can cache all it can, writes included.
**Result:** Unlinks 3,530 -> 2,560/s, creates without DAX 2,193 -> 1,973, sequential writes without
DAX 58 -> 43 MiB/s. Random writes without DAX 4,560 -> 5,469 and the build 2.5% quicker.
**Why it's dead:** It cost more than it saved. With it, closing a written file writes it back and
sends its times (fs/fuse/file.c, fuse_flush). The FUSE server itself has since gone (fda9d7d2).
**Reopen if:** A FUSE path comes back. Guest Fxfs doesn't use one.

A FUSE path came back on 2026-11-07: the host's mounts of stores (`cladefs::fuse`, `cladefs-mount`,
TASK-222), a server descended from the one measured here, over `/dev/fuse` rather than virtio-fs.
It initially kept writeback cache disabled, unmeasured on `/dev/fuse`.

**Reopened and landed (2026-11-25):** Measured on `/dev/fuse` (Linux 7.0 on `vm1`, 3 rounds,
memory-backed Fxfs cell, alongside coalescing `mtime`/`ctime` into `fxfs.write`'s single
transaction and re-enabling `FUSE_HANDLE_KILLPRIV_V2` to set `SB_NOSEC` and stop Linux from
sending `GETXATTR("security.capability")` before every `write(2)` syscall):

| Workload | Baseline (2-txn, no-kp, no-wb) | 1-txn + killpriv_v2 | 1-txn + wb (no-kp) | 1-txn + killpriv_v2 + wb |
|---|---:|---:|---:|---:|
| `write-4k-16m` (`ld.bfd` `_IO_fwrite`) | 26.9 MiB/s (8,200 RPCs) | 55.7 MiB/s (4,105 RPCs) | 127.1 MiB/s (4,128 RPCs) | **651.8 MiB/s (35 RPCs)** |
| `write-2k-16m` | 14.0 MiB/s (16,391 RPCs) | 28.7 MiB/s (8,199 RPCs) | 71.9 MiB/s (8,222 RPCs) | **817.2 MiB/s (33 RPCs)** |
| `seq-write-1m` (64 MiB) | 615.3 MiB/s (134 RPCs) | 898.1 MiB/s (72 RPCs) | 658.2 MiB/s (158 RPCs) | **856.1 MiB/s (105 RPCs)** |
| `rand-write-4k` (4,000 ops) | 5,925 ops/s (8,004 RPCs) | 14,255 ops/s (4,004 RPCs) | 25,568 ops/s (5,907 RPCs) | **57,875 ops/s (1,900 RPCs)** |
| `create-empty` (2,000 ops) | 3,844 ops/s (6,000 RPCs) | 6,580 ops/s (6,000 RPCs) | 8,002 ops/s (6,000 RPCs) | **7,619 ops/s (6,000 RPCs)** |
| `unlink-empty` (2,000 ops) | 5,659 ops/s (5,999 RPCs) | 6,928 ops/s (6,000 RPCs) | 7,358 ops/s (7,999 RPCs) | **7,071 ops/s (7,999 RPCs)** |
| `create-4k` (2,000 ops) | 2,369 ops/s (10,000 RPCs) | 4,333 ops/s (10,000 RPCs) | 3,750 ops/s (12,000 RPCs) | **3,501 ops/s (12,000 RPCs)** |
| `unlink-4k` (2,000 ops) | 5,335 ops/s (5,999 RPCs) | 9,646 ops/s (5,999 RPCs) | 6,480 ops/s (7,999 RPCs) | **6,115 ops/s (7,999 RPCs)** |
| `fsync-4k` (200 ops) | 4,307 ops/s (605 RPCs) | 8,189 ops/s (406 RPCs) | 10,032 ops/s (649 RPCs) | **5,806 ops/s (508 RPCs)** |
| `par4-write-4k` (4x8 MiB) | 43.7 MiB/s (16,411 RPCs) | 115.0 MiB/s (8,223 RPCs) | 204.2 MiB/s (8,296 RPCs) | **917.9 MiB/s (100 RPCs)** |

On `/dev/fuse`, the old GCE virtio-fs regressions did not reproduce relative to baseline: every
measured workload improved (small buffered writes 21–58x, random 4K writes 9.8x, 1 MiB sequential
writes 1.39x, creates 1.48–1.98x, unlinks 1.15–1.25x). Both `FUSE_WRITEBACK_CACHE` (with
`FATTR_CTIME` and `WRITE_CACHE` handling) and `FUSE_HANDLE_KILLPRIV_V2` (with `security.capability`
removal and `FALLOCATE` privilege clearing) are now enabled on `cladefs::fuse::Server`.
