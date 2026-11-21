TASK-702.3 (board 2902): a cold host's first boot of a version with read evidence, fetched ahead, against warm ones; with cold peers (every host restarted) and warm peers (one restarted).

Host: ubu1. `CLADEFS_PYSANDBOX_COLD=1 scripts/python-sandbox-host.sh` at 3c5ae9b37 (the code as it
landed, after review); notebook workload, 2 rounds, then the two cold cases.

Answered (ms, from just before the fork):

| sandbox | host | answered | against warm (6,202 ms, round 2's mean) |
|---|---|---|---|
| a1 | warm (imported the base), no evidence yet | 7,809 | 1.26x |
| b1 | cold, evidence from a1, peers mixed | 7,990 | 1.29x |
| c1 | cold, evidence from a1 and b1, peers warm | 8,471 | 1.37x |
| a2, b2, c2 | warm | 6,263, 6,183, 6,161 | 1.00x |
| b-cold_peers | every host restarted: cold host, cold peers | 8,549 | 1.38x |
| c-warm_peers | host-c alone restarted: cold host, warm peers | 8,171 | 1.32x |

Before prefetch (ubu1-python-sandbox-2026-11-19T1842Z-7786f0b5): cold b1 13,422 ms (2.2x), c1
10,246 ms (1.7x), warm ~6,020 ms. A cold host's first boot is within 1.5x of a warm one, with cold
and with warm peers (TASK-702.3 #5). An earlier run before the review's fixes
(prefetch fetching only each unit's first part) gave 1.37x and 1.29x.

Each boot read 2,022 units (MiBs of the version's extents) in its first 10 s; every boot with
evidence read exactly the units predicted (the daemons' stats: predicted-read 2,022 a boot,
predicted-unread 0, read-unpredicted 0). host-a's two boots had no prediction (read-unpredicted
4,044): the first found no evidence, and the host kept that empty prediction for the interval
(board 5337). Each cold host's prefetch fetched ~308 parts itself (prefetches: flights it started);
the rest of the parts it asked for were already in flight, the guest's reads and read-ahead
asking for them first, or kept.
