# doc-4 over vsock against the device's queues (board 14663), 2026-11-28

One build, `c83c18c20` (branch `vsock-doc4`, Firecracker `fad389a01`), its two sides told apart by
`CLADEFS_EXTENT_STREAM=1` alone: **queues**, the guest's data requests on the extent device's
queues, which Firecracker forwards to cladefsd (step 0's path); **stream**, the same requests over
vsock to cladefsd, which serves them itself. `scripts/bench-ab-host.sh`, default cases and groups
(no pgbench), 3 rounds of 2 iterations, interleaved, `hotbed run --bench` on ubu1 (an 8-CPU VM).
Took 4778 s. Whether others wrote to the disk beside the rounds is unchecked (ab.md's header).

What moved, past drift (`stream vs queues`: >1 is the stream slower):

| Case | create-fsync | fsync-append | seq-write |
|---|---|---|---|
| fxfs-guest | 2,939 → 2,042 ops/s, 1.44× | 2,662 → 1,907 IOPS, 1.40× | 13.5 → 31.9 MiB/s, 0.42× |
| fxfs-guest-dax | 2,827 → 2,088, 1.35× | 2,867 → 2,163, 1.33× | 12.3 → 27.3, 0.45× |
| fxfs-guest-dax-unsafe | 2,688 → 2,081, 1.29× | 2,964 → 2,190, 1.35× | 16.0 → 27.9, 0.57× |

And fxfs-guest's mmap-rand-write, 9,312 → 10,900 IOPS (0.85×). Everything else within drift.

An fsync waits on one round trip, which the stream made longer: the guest's request woke its
driver's sender thread, its reply the receiver thread, and cladefsd's reply its writer thread, each
a wakeup the fsync waited on, where the queues' caller polls for its completion. Next, the submitter
sends its own request without waiting (`8c73c885a`) and the serving thread writes its own reply
(`e0d1d52a8`); measured again, with pgbench.

Absolute numbers are low on both sides (seq-write 85 MiB/s in step 0's unisolated baseline): the
comparison holds, the sides interleaved on one host, but not the numbers against other runs.
