Keys-only collector recovery (board 14523): what native cladefs-gc gains or pays.

- A: 5639cefba, main's full-map collector (`recover_layers`, `recover_ats`), with the fill's `--vm ... TAIL`.
- B: c9ddcd36b, keys-only (`recover_keys`, `keys_at`).

Host: asahimbp, hotbed `run --bench` (P-cores 7-10), 2026-11-27. scripts/gc-sweep.sh run directly in
the workspace, `--prepare` as work, then `--prepared` under `--bench`: scripts/gc-sweep-host.sh's job
failed there as the workspace has no /dev/kvm (board 14550).

Bucket (fill-b.txt, the same for both sides): one VM cell of 200 segments of 4,000 extents each
(16-112 bytes), 8 versions evenly apart, its checkpoint at segment 148, 50 segments before its last
(`CLADEFS_GC_FILL="--vm 4000 8 50"`): 202 objects, 31.8 MB, the checkpoint 2.65 MB, 3 parts of 1 MiB.
`CLADEFS_GC_TENANTS=1 CLADEFS_GC_CELLS=1 CLADEFS_GC_SEGMENTS=4 CLADEFS_GC_LARGEST=200
CLADEFS_GC_CELL=1/1 CLADEFS_GC_AT_ONCE=1 CLADEFS_GC_SWEEPS=1 CLADEFS_GC_ROUNDS=3`.

A pass over the cell in full (gc-sweep-cell.md), each of 3 rounds alike:

| side | GET | ranged GET | other | requests | seconds | peak MiB |
|---|---|---|---|---|---|---|
| A, full map | 17 | 450 | 6 | 473 | 0.55 | 517-525 |
| B, keys-only | 8 | 502 | 6 | 516 | 0.19-0.20 | 281-317 |

Both delete nothing and leave the same 204 objects (the VM frees nothing). B makes 9 GETs fewer: the
8 versions' checkpoint keys are probed by a ranged read of their tails, not a GET, and the base
checkpoint is read by its tail and 1 MiB parts, not whole; it makes 52 ranged GETs more, those and
the parts, read for the live cell and again for the walk to the versions after the checkpoint (A
decodes it once and shares it). Net +43 requests (+9%) a pass; -40% to -46% peak memory; 2.8x
faster.

B's peak is still O(extents): the five versions below the checkpoint walk from segment 0, and
`KeyReplay` holds a `String` key per sealed extent (~500k): over the Worker's 128 MiB. Board 14553.
