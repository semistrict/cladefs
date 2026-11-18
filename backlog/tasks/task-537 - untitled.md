---
id: TASK-537
title: >-
  Replicated commits: durable on a majority of peers' disks without waiting for
  the bucket, and their recovery
status: To Do
assignee: []
created_date: '2026-11-18T12:36:00Z'
updated_date: '2026-11-18T12:36:00Z'
labels:
  - durability
  - bucket
  - peers
dependencies: []
references:
  - TASK-96
  - TASK-406.2
documentation:
  - backlog/docs/doc-12 - The-fsync-durability-contract.md
  - >-
    backlog/docs/doc-7 -
    Object-storage-how-the-host-keeps-branches-in-S3-and-GCS.md
  - >-
    backlog/docs/doc-14 -
    Local-disk-as-the-buckets-write-ahead-log-and-cache-one-storage-engine.md
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
priority: high
type: feature
ordinal: 927000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Today a commit is durable off its host only once it is **Uploaded** (doc-12): sync, syncfs, unmount and freeze wait for the bucket's round trips (~130 ms on GCS, TASK-165.11), and fsync's Ordered/Local commits are lost with the host. This adds a level, **Replicated**: the commit is on a majority of hosts' disks. It does not wait for the object store, so a lost host loses nothing that reached Replicated, and the upload goes on in the background. Recovery then gets that suffix back when the VM manager takes the cell on another host.

Decided with the user (interview, 2026-11-18):
- **Who waits:** a per-cell option, set by the VM manager, says which level fsync waits for (Ordered, Local or Replicated). The sync family waits for Replicated rather than Uploaded in a replicated cell.
- **Copies:** a majority quorum. 2f+1 copies counting the writer, f+1 acknowledgements (the writer's own Local counts as one), f per cell.
- **Authority:** the bucket. The epoch and the replica set change only by conditional writes to it, which extends Bucket.tla's fencing. The data path never touches it. No consensus among the peers.
- **Placement:** daemons pick replicas themselves from a pool of hosts kept in the bucket, by failure-domain label.
- **No quorum:** a cell that loses its majority degrades to waiting for Uploaded. Its waiters wait for the bucket instead, and the cell picks new replicas in the background. Loud, in the stats and metrics.
- **Wire:** the existing peer network (`cladefs.peer.v1`, `CLADEFSD_PEER_ADDRESS`, TASK-406.2): gRPC over TCP between daemons, unauthenticated on the hosts' network. Peers hold only ciphertext under the cell's DEK. mTLS is out of scope for now.
- **Recovery:** started by the VM manager's take of the cell on a new host, as today. The new writer fences in the bucket, seals the old replica set, recovers the longest suffix a majority could have acknowledged, re-replicates it, and only then serves.

Supersedes TASK-96's "Peer log" (one peer named in the lease) for the replication part.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cell can be made replicated with f and an fsync level; its commits reach Replicated with f+1 of 2f+1 copies on disk, and the waiters for that level are woken then without any bucket request on the path
- [ ] #2 Losing the writer's host loses no commit that reached Replicated: a take on another host recovers it (tested with real daemons)
- [ ] #3 Protocol modelled in TLA+ and model-checked, with mutations that break it
- [ ] #4 doc-12 gains the Replicated level, its failure table rows and their tests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
