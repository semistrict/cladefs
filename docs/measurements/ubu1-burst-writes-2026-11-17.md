# store-bench burst writes on ubu1, where they go (TASK-433.9, TASK-512), 2026-11-17

`store-bench burst`: 1024 writes of 256 KiB to an encrypted store's open segment, then one
commit. All on ubu1 (Ryzen 9 8945HS, VAES) in `fxjob --bench` jobs, sides interleaved, six
rounds, medians of rounds 2-6, each side in its own worktree and target, checked for a side-only
string. Two disks: ext4 on the scratch zvol, and ext4 over a RAM-backed loop device. A scratch
probe (never on main) timed each step of `Store::write_with`, `RawFile::write_at`,
`Cached::write_at` and `write_runs`, per write.

Sides: p `150b82934` (data through the host's page cache; its stores clear, before TASK-411);
main `073feb370`; xts, main with `store-bench`'s data made before each write is timed and
AES-256-XTS through the backend's parallel blocks (TASK-512); frames, xts with the page cache's
pages in frames of 2 MiB slabs (TASK-433.9).

## Found

- `store-bench burst` timed the making of each write's 256 KiB of data with the write: as long
  as the store's write itself on a Mac. Fixed.
- Since TASK-411 every raw write is encrypted: 334 µs of main's 460 (RAM). Fuchsia's
  `storage_xts` ran every 16-byte block through `encrypt_block`, latency-bound, though `aes`
  chose its VAES-512 backend (perf). Through `encrypt_par_blocks`: 72 µs.
- The copy into the stores' pages, 84 µs, was the faults on each fresh 4 KiB page boxed:
  malloc's heap in transparent huge pages (`GLIBC_TUNABLES=glibc.malloc.hugetlb=1`) made it 20.
  Frames of 2 MiB slabs advised `MADV_HUGEPAGE`: 25.
- Not lock waits: the pages lock 2-5 µs a write, the throttle none.
- What is left is the writer's wait for write-back (24-107 µs a write, varying by run): the
  flusher's 1 MiB direct `pwrite`s, one at a time, take ~950 µs each, wall (`strace -c -w`), 163
  µs of it CPU; off CPU in `blk_io_schedule` under `iomap_dio_rw` (bpftrace). `dd oflag=direct`
  of noise, alone: ~300 µs a MiB on both disks.
- Tried and dropped: begun ranges merged, written 4 MiB at once: RAM 161 -> 210 µs, zvol 216 ->
  176 µs.

## Results (µs per write, medians of rounds 2-6)

| side | RAM write | of it, encryption | copy into pages | waiting on write-back | zvol write |
|---|---|---|---|---|---|
| p (clear, host page cache) | 142 | 0 | | | 85 |
| main | 460 | 334 | 88 | 5 | 468 |
| xts | 243 | 72 | 89 | 46 | 269 |
| xts, again beside frames | 210 | 71 | 83 | 24 | 276 |
| frames | 165 | 63 | 25 | 53 | 208 |

The commit after the burst, durable: main 33.7 ms (RAM), 55.1 (zvol); frames 25.5, 38.0; p
40-162 (RAM, varying), 46 (zvol).
