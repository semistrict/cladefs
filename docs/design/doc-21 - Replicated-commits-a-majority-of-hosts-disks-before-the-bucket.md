---
id: doc-21
title: 'Replicated commits: a majority of hosts'' disks before the bucket'
type: specification
created_date: '2026-11-18T12:27:00Z'
updated_date: '2026-11-20 04:00'
---
# Replicated commits: a majority of hosts' disks before the bucket

> **Removed (2026-11-27, board 14440):** Peer-replicated commits (`CLADEFSD_REPLICAS`, `FsyncLevel::Replicated`, and the `formats/replica-directory` family) were removed from cladefs after a first-principles review: no deployment, script, or guest test enabled peer replication, and Fast WAL ([doc-29](<doc-29 - Fast-WAL-commits-appendable-object-storage-without-local-state.md>)) is the kept durability mechanism for surviving host loss before cold-bucket upload. Stored bucket heads that name peer replication still decode cleanly and are refused at open/take time rather than opened as Standard cells. This document is kept as a historical record of the removed design.

Design, 2026-11-18 (TASK-537.1). The user decided its direction in an interview that day (TASK-537's
description). This doc chooses the protocol, against prior art and against what doc-7's fencing already
gives. It then defines what the TLA+ spec (TASK-537.2) checks and what the implementation (TASK-537.3)
builds. It says what the code does today by `file:line`, as of 845a6c12.

## Why

doc-12 has three levels: **Ordered** (cladefsd has the commit, in memory), **Local** (on the host's
disk) and **Uploaded** (in the bucket, acknowledged by the head's swap). Only Uploaded survives losing
the host. So a guest that needs that survival pays the bucket's round trips on every sync (a PUT, then
the head's conditional swap, ~130 ms on GCS, TASK-165.11). The fsyncs that don't pay them lose up to
the whole upload lag with the host (4,096 commits or 1 GiB).

**Replicated** sits between Local and Uploaded: the commit is on the disks of a majority of 2f+1
hosts, counting the writer. It costs one round trip on the hosts' own network and a peer's disk sync,
both overlapping the writer's own local sync, and no bucket request. The upload goes on behind it as
today, and the bucket stays the only place a cell lives for good. Replicas hold only the unuploaded
tail and drop it as the head acknowledges it.

## The user's decisions (2026-11-18)

| Question | Decision |
|---|---|
| Who waits for Replicated | A per-cell option: fsync waits for Ordered (today's default) or Replicated (deprecated `FSYNC_LEVEL_LOCAL` maps to Ordered with a warning). In a replicated cell, sync, syncfs and unmount wait for Replicated rather than Uploaded. |
| Copies | A majority quorum: 2f+1 members counting the writer, f+1 acknowledgements (the writer's own Local counts as one), f per cell. |
| Authority | The bucket. The epoch and the replica set change only by conditional writes of the head. The commit path never touches the bucket, and the peers run no consensus. |
| Placement | The writer picks its peers itself, from a pool: any other hosts of its cloud region, those sharing the cell's bucket (the user, 2026-11-19: no failure-domain rule; see Decisions: the pool is the existing peer list). |
| No quorum | Degrade to Uploaded: a commit waiting for Replicated is released by Uploaded too, and the writer replaces its peers in the background. |
| Wire | The existing peer network (`cladefs.peer.v1`, TASK-406.2): gRPC over TCP between daemons, authenticated both ways by mutual TLS with the hosts' CA (`CLADEFSD_PEER_TLS`, board 8437); each replication call admitted only if its cell's head in the bucket could have asked it (board 8445). |
| Who recovers | The VM manager's take of the cell on another host, as today. |

## Prior art, and what each would mean here

| Protocol | Who decides membership / epoch | Copies for f failures | Ack rule | Recovery | Fit |
|---|---|---|---|---|---|
| **Raft log replication** (leader, terms) | the replicas, by election | 2f+1 | majority | new leader has the most up-to-date log; conflicting suffixes truncated by term | Elections are what we don't want (the bucket is the authority). Its per-entry terms and truncation exist to reuse one log across leaders, which we avoid by starting a fresh log per configuration. |
| **PacificA / chain replication with a master** | a configuration master | f+1 | all replicas in the configuration | the master removes a failed replica; a new primary is any member of the configuration | Fewest copies, but one slow peer stalls every commit until the master removes it, and every removal is a bucket write on the commit's path. The user chose a majority quorum. |
| **Kafka ISR** | controller (ZooKeeper/KRaft) | f+1 in sync | all of the ISR | the leader is elected from the ISR; the ISR shrinks by a controller write | Same trade as PacificA, with a shrinking set: a bucket write each time a peer is slow. |
| **Apache BookKeeper** ledgers | ZooKeeper metadata: ensemble, ledger state OPEN / IN_RECOVERY / CLOSED | ensemble E, write quorum Qw, ack quorum Qa | Qa of Qw | a reader *fences* the ledger on enough bookies that no ack quorum is left, reads the last entry, re-replicates it, closes the ledger in ZK | Close to what we need: ZK becomes the bucket, a ledger becomes a configuration. Its ensemble changes in the middle of a ledger are recorded in the metadata, as our replica replacement would be. |
| **Delos VirtualLog / loglets** (Meta) | a metastore holds the chain of loglets | per loglet | per loglet | *seal* the active loglet, append a new loglet to the chain in the metastore | The same shape: each configuration is a write-once log, fenced when superseded, chained by the authority. Our chain needs only its last element, as the bucket holds everything before it. |
| **Vertical Paxos II** (Lamport, Malkhi, Zhou) | an external master | 2f+1 per configuration | majority of the configuration | the new configuration gets the state from the old one, and the master *activates* it before the old one is forgotten | The theory behind the two above: why an activation write is needed, and when the old configuration can be dropped. |
| **Amazon Aurora** storage | storage-node epochs, volume membership in its control plane | 6 copies across 3 AZs | 4 of 6 | quorum read of 3 of 6 at recovery, epoch bump fences old writers on the storage nodes | Quorum writes with epoch fencing kept on the replicas themselves, quorum reads only at recovery. That is our steady state and our recovery. |
| **Neon safekeepers** | the safekeepers, by a Paxos-like term election | 3 | majority | the new compute's proposer runs phase 1 on the safekeepers, picks the most advanced WAL, truncates the rest | Postgres WAL → a majority of safekeepers → S3: our pipeline. Its own election is what the bucket replaces for us. |
| **Cloudflare Durable Objects (SQLite storage)** | Cloudflare's placement | 5 followers | 3 of 5 | from the followers; the WAL also goes to object storage in batches | The product TASK-96 takes as its model: a majority of nearby followers on the commit path, and object storage behind it. |

**Chosen: quorum-acked primary-backup, reconfigured only by the bucket's CAS.** This is not Paxos run among the members. No member votes, and there are no ballots per entry and no elections. One writer appends to each configuration, and every change of writer or members is a conditional write of the head. Vertical Paxos is the published form of this pattern (an external master reconfigures; the replicas only accept), and BookKeeper and Delos build it. Bucket CAS cannot do three things, and they are all the protocol adds: fencing the old members on a take (members don't read the bucket on every append, so CAS alone does not stop a zombie writer from gathering acks), reading f+1 members at recovery (which the majority quorum requires), and activating the new configuration with a second CAS before serving (so that a take which crashes halfway leaves the old configuration authoritative). The spec's mutations `NoFence`, `ReadOneMember` and `NoActivation` show each of the three is needed. A cell's replication runs through a sequence of *configurations*. Each is a set of 2f+1 members (the writer and 2f peers) that holds a write-once log of the commits since the bucket's end. The bucket's head names the active configuration. A configuration is never changed: a take, or a replacement of a peer, makes a new one, copies into it what recovery found, and activates it with a conditional write of the head. A superseded configuration is sealed, then dropped.

Within one configuration there is one writer and one sequence, so every member's log is a prefix of the writer's. Recovery picks the longest of the logs it read: no terms per entry, no truncation of conflicting suffixes. That is the simplification over Raft that the bucket's authority pays for.
## The protocol

### Terms

- **Member:** a host holding a copy of a cell's unuploaded log for a configuration. The writer is
  always one.
- **Configuration** `c`: a number, the writer's epoch `e` that made it, and its 2f+1 members. A
  configuration's log is written once: only that writer, at that epoch, appends to it, and nothing
  else ever does. The head names one configuration, the **active** one.
- **Tag:** a commit's identity, `writer_epoch<<40 | commit number` (`extent_frame/src/segment.rs:279`).
  Commit numbers continue across epochs (`open_cell`'s take: the cell's epoch is
  `epoch_of(last_commit)`), so the tags of one cell's history increase.
- **A member's end:** the greatest tag through which it holds every record of the configuration,
  synced.
- **Replicated(t):** at least f+1 members of the active configuration have ends ≥ t. A waiter for
  Replicated is released by `Replicated(t) ∨ Uploaded(t)`. That second disjunct is the whole of
  "degrade to Uploaded": it needs no mode switch.

### The head (bucket format bump)

`Head` (`bucket_cell/src/layout.rs:544`, `FORMAT` 19 → 20) gains:

```rust
replicas: Option<Replicas>,       // None: an unreplicated cell, as today
struct Replicas { config: u64, epoch: u64, f: u8, members: Vec<(String, u64)> /* HOST:PORT and incarnation, the writer's first */ }
```

The head is written by `head::take` (`head.rs:125`) and swapped by `head::swap` (`head.rs:180`),
both conditional, as before. A configuration is activated by such a swap, and so it is fenced by the
writer's epoch, as acknowledgements are.

### What a member holds

A member mirrors the writer's **segment logs**, and nothing else. A commit's local form is already
self-contained (`wal.rs:140`): `LogRecord { contents, runs }`. `Runs::Held(bytes)` carries its runs'
bytes inside the record, as XTS ciphertext under the cell's DEK. The log header names the DEK's
`KeyId` and the file's XTS salt (`wal.rs:293-321`), and every record is MACed. So the writer sends
each record **as Held**, whatever mode it wrote locally (`Checked` records read their runs' ciphertext
back from the raw file through `local_cell::pages`). A member writes them, unopened, to:

```
replicas/<tenant>/<cell>/<config>/logs/<n>      the writer's log n: its header, then records, O_DIRECT
replicas/<tenant>/<cell>/promise                the greatest epoch it promised (synced, small)
```

**Incarnations** (found by TLC, TASK-537.2): a member's promises live on its disk, so a host that lost its disk lost them. A zombie writer could then fill the empty host's log again and gather its quorum, and a host that lost its disk could retake the cell and count its own copy, now empty, as the old writer's. So each daemon's `replicas/` directory has an incarnation, a random 64-bit number made when the directory is created, and kept in it. A configuration names its members as (address, incarnation). A member refuses an `Open`, `Append` or `Fence` that names another incarnation, and a taker counts a member's read only in the incarnation the head names, its own included. This is BookKeeper's cookies. The spec's `NoIncarnation` mutation loses an acknowledged commit without it.

**A replica directory is never restored or copied** (TASK-682; the user's decision,
2026-11-19: a rule, not detected). Restored from a snapshot or a backup, or cloned onto a host
that takes the same address, it keeps its incarnation but goes back to older promises, so a
zombie writer's appends of an epoch a take fenced are taken again, and a commit it tells
Replicated is in no later writer's history. Incarnations cover a disk lost, not one gone back in
time; FoundationDB assumes the same of its disks. A host whose disk was restored deletes its
replica directory before its daemon starts, which makes it a new member (`docs/operating-a-host.md`).

A member therefore holds what the bucket holds and no more: data in ciphertext, and commit metadata
(`Contents`: extent IDs, offsets, the root) in the clear but MACed, as framed footers are in the bucket
(doc-16). It can verify nothing and needs no key. Recovery checks the MACs with the cell's key, so a
host that injects or alters records makes recovery refuse them rather than take them. Peers
authenticate each other (board 8437), and a member admits a call only if the cell's head could have
asked it (board 8445): an append to the head's active configuration or one after it of its epoch, a
fence of the active one at no later epoch than the head's, a drop of the logs of a configuration its
writer appends to, or of configurations before the active one (a log is named by its writer's cell's
segment, which no head names: a drop's last log is its writer's word).
A member holds at most so many configurations of a cell, every byte admitted by the disk governor
(board 8451).

### RPCs (added to `service Peer`)

| RPC | What |
|---|---|
| `Append(stream AppendRequest) → stream AppendResponse` | One stream from a writer to a member for each (cell, configuration). `Open{cell, config, epoch, members, incarnation}` first, then `Log{n, header}`, `Records{n, bytes}`, and `Acknowledged{through_n}` (the bucket's end, below). The member answers `End{tag}` after each sync. It refuses everything if `epoch < promise` (`FAILED_PRECONDITION`, with its promise). |
| `Fence{cell, config, epoch} → {logs: [(n, len)], end}` | Raises the promise to `epoch`, synced, before it answers. Appends of any older epoch are refused afterwards. It returns what it holds of that configuration. |
| `ReadLog{cell, config, n, from} → stream bytes` | A log's bytes, for recovery. |
| `Drop{cell, below_config}` | Removes the logs of configurations older than this one. |

As built (TASK-537.3.5's reviews), `DropLogs` with `through` removes that configuration's logs to
`through` and nothing of any other, and without it removes every configuration's before the one
named. A replacement's members are sent the first as the bucket takes segments while the head still
names the configuration before, whose logs on the same host a take would read; only a writer
whose head names the new configuration sends the second.

The peer channels today have a 5 s per-request timeout (`cladefsd/peers.rs:187`), which would cut a
stream. `Append` gets its own endpoint, with keepalives and no request timeout.

### Commit (steady state)

1. `commit_in` (`local_cell/src/cell.rs:2035`) makes the commit Ordered, as today.
2. The flush (`flush_locked`, `cell.rs:2835`; every 50 ms, `server.rs:62`, or at once for a waiter)
   writes the batch to the local logs and **sends the same records to every peer of the active
   configuration**, at the same time. Each peer appends, syncs and answers its end. There is one
   flush on the path, and the network round trip overlaps the local sync.
3. The writer keeps each member's end. `replicated` = the (f+1)-th greatest of them, its own Local
   end counting as one. When it advances, it notifies the Replicated waiters: a new `Notify` beside
   the upload's `changed` (`upload.rs:1224`). Today no waiter is woken for Local; callers serialize on
   `flushing`.
4. The uploader goes on unchanged. After each head swap that acknowledges segments through `n`
   (`acknowledged`, `upload.rs:1207`), the writer sends `Acknowledged{n}` on the streams, and members
   drop their logs up to and including `n`.
5. The lag bound (`upload::Options`, 4,096 commits and 1 GiB) bounds a member's disk for each cell
   too. It applies to every commit level, Local and Ordered alike (`OpenCell::within_lag`,
   TASK-582), so a member's disk never holds more than the bound.

A member that falls behind, or whose stream breaks, is caught up from the writer's local logs, which
hold everything unacknowledged.

### Take (recovery)

The new writer `W'`, at the VM manager's take, on any host:

1. **Take the head**, epoch `e' = e+1` (`head::take`, as today). The head still names the active
   configuration `c`, members `M`.
2. **Fence.** Send `Fence{cell, c, e'}` to every member of `M`, and wait for **f+1 answers from
   members other than the old writer**. The old writer's own copy counts only when `W'` is that host
   and holds that WAL itself (a reopen on the same host). The arithmetic:
   - The old writer `W` cannot be fenced. It may be alive (a zombie, cut off from the bucket but not
     from its peers), and it counts its own copy. If `W'` fenced only f+1 of all members, `W` plus
     f unfenced peers would still make f+1, and `W` could tell the guest a commit was Replicated that
     `W'` never reads.
   - With f+1 of the 2f peers fenced, at most f−1 peers are unfenced. `W` plus those is f < f+1, so
     no new commit reaches Replicated after the fences.
   - Every commit that did reach Replicated is on f peers besides `W`. f + (f+1) > 2f, so at least
     one fenced peer holds it.
3. **Read.** Take the log with the greatest end among the fenced members' answers (members drop
   acknowledged segments at different times, so the greatest end, not the most bytes). All of them are prefixes of
   `W`'s one sequence in configuration `c`, so the longest contains each of the others: there are no
   conflicts to resolve, no per-entry terms, no truncation. Fetch it (`ReadLog`, from any fenced
   members that hold each log), verify its MACs, and write it as the local logs of `W'`.
   *As built (TASK-684, found writing TASK-680's review test):* of a segment's records, the
   longest copy the cell's keys find authentic, cut to what of it is whole: a longer copy,
   garbled or forged, would otherwise hide an honest one. Only a copy found not authentic is
   passed over; one that can't be read, or reads shorter than its fence said, fails the take,
   as it may be the only copy of commits told Replicated, and so does a log with no authentic
   copy. *Added 2026-11-21 (board 5416):* each copy is read to the end its member's fence said,
   no further, the stream dropped there (a faulty member streaming on grew memory without
   bound), and each piece within the peer deadline (5 s) of the one before (a member whose disk
   stalled answered its keepalives and held the take for ever). A copy that can't be read so is
   passed over if another as long is read; else it fails the take, as above.
4. **Rebase** with the existing code (`Wal::rebase`, `bucket_cell/src/wal.rs:148`): the records
   through the head's `last_commit` are the bucket's already, and the rest is restamped onto
   `W'`'s chain. Members may have dropped segments the head acknowledged, so the replica tail can
   start *after* `last_commit`. `rebase` must accept a log whose first commit follows `last_commit`
   (today it requires the position of `last_commit` itself, or an empty log).
   As built (`replicate::trimmed`, TASK-537.3.5's review): the head acknowledges whole segments,
   so a take keeps the members' logs of the segments after the last whose records name a commit
   through `last_commit`, from the first of them holding a record, and checks that their first
   commit follows `last_commit`. A member's copy of a segment the head named, whole or cut short,
   is the bucket's already: one whose drop never came is no hole. (Not the head's count of
   segments, `end`: a writer's local segments are numbered as its own, the bucket's as the cell's.)
   What members send is read here holding no key, so a segment at or before the last named one
   naming a commit past `last` is refused (no writer's logs do; a forged record may, which would
   otherwise make honest logs after it look named, and drop them before any MAC is checked).
   *As built (TASK-680, found by the simulation):* logs whose first commit does not follow
   `last_commit` are given up, not refused, once the cell's keys find them authentic (a forged
   record past `last_commit` would otherwise make a take give up commits told Replicated:
   TASK-680's review). Members are sent a segment's logs before the next's
   and drop only what the head names, so what is between is their writer's base, the commits it
   opened with, which it gave no member and told none of Replicated before the bucket held them.
   Refused, a writer that took a cell of f = 0 and died before the bucket held its base left the
   cell untakeable for good.
   *As built (TASK-537.3.6),* **no members for `c+1`** (one host of three lost, say): the take
   serves alone, at f = 0, its commits waiting for Uploaded. The head goes on naming `c`, fenced,
   whose members hold what it recovered, until the acknowledgement that first names the last commit
   it recovered names the writer's own configuration (f = 0, number `c+1`) in the same swap
   (`Writer::configure_past`). Nothing of the writer's own is acknowledged before that swap: else a
   later take, reading `c` with the head naming it, could prefer a longer log of `c` (the old
   writer's own, on its host) over the bucket's, and lose a commit told Uploaded (the spec's
   `RecoverAlone`, rule `aloneSwitch`, mutation `AloneNoSwitch`). Its keeper replaces nothing
   before, then brings it to the f its head asks as members answer.
5. **Install** configuration `c+1`, made at `e'`: `W'` and 2f peers picked as below. Stream the
   recovered tail to them, and wait until f of them hold it all (f+1 with `W'`).
6. **Activate:** a conditional swap of the head to `replicas = c+1`. Only then does `W'` serve.
   Then `Drop{cell, c+1}` to `M`, best effort (stale logs are also swept, below).
   *As built (`Replicator::base`, TASK-537.3.3):* the cell's replicator starts after the last
   commit the take opened with, its own write-ahead log's included (a reopen on the same disk may
   hold more than the members did), which no member of `c+1` need hold. So no commit after a take
   is told Replicated before the bucket holds every commit through that one; until then its commits
   wait for Uploaded. A take while the bucket is out of reach serves, but its commits wait for the
   bucket (cladefs's `replicated_commits_survive_their_daemon_killed_and_reopened`).
   *TASK-678's review:* the members' logs are given `c+1`'s members only if they are of the
   the cell's own history, when its own log is what it recovers: a retake after a deferred one may
   find `c`'s members holding an earlier writer's commits past what its own log, a later writer's,
   went on from (none of them told Replicated). Of one writer's sequence, a log holding a commit
   of that writer at or past the members' last place holds it (`Recovering::holds_commits`).
   *Corrected (TASK-678, found checking the spec against the code):* that note missed the commits
   the take recovered from its own write-ahead log past what the members it fenced gave, as on a
   reopen on the old writer's disk with the member holding them out of reach. They may have been
   told Replicated, and no member of `c+1` holds them: activated at once, the head named a
   configuration that loses them to the next take elsewhere. Such a take now activates `c+1` only
   by the acknowledgement that first names them all, in the same swap (`Writer::configure_past`),
   the head naming `c`, fenced, until then, as a take serving alone does. Its members take its
   commits meanwhile, none told Replicated before the swap (`Replicator::told`), and the keeper
   uploads at once. The spec's rules `installOwn` and `deferQuiet` (mutations
   `InstallMembersOnly` and `DeferTellsReplicated`, model `ReplicationDefer`), and
   `bucket_cell::Rules::installs_own`, which the simulation's
   `retakes_need_to_install_their_own_logs` turns off to find the loss.

A crash anywhere in 1–6 leaves the head naming `c`, whose fenced logs nothing changed. The next take
fences `c` again, at a greater epoch, and finds the same commits or more. Configuration `c+1`'s logs
are separate files, even on a host that is a member of both, so a half-done install touches nothing
that recovery reads. This activation, a second head write on each take, is what Vertical Paxos says a
new configuration needs before the old one may be forgotten. Without it, a take that crashed after
fencing would leave the next take reading an incomplete configuration.

A cell whose head names no `replicas` takes as today. So does a take that finds `f = 0`.

### Replacing a member, without a take

The writer `W`, at epoch `e`, replaces a member that failed or lags far behind, or leaves degraded
mode:

1. Pick configuration `c+1` (same `e`): the surviving members and new ones.
2. Install: stream the whole unacknowledged tail to the new members, then every new commit to both
   configurations' members.
3. Once f+1 members of `c+1` hold everything `W` has acknowledged as Replicated, swap the head to
   `c+1` (conditional on `e`, so a writer already fenced fails here and stops).
4. **While that swap is in flight, a commit is Replicated only if it is so in both `c` and
   `c+1`**: a taker may find either in the head. After the swap lands, only `c+1` counts. This is
   Raft's joint consensus, for one write.

`W` needs no fence of `c`, as no one else appends to it, and `W` itself stops counting it once the
swap lands.

As built (TASK-537.3.5, `upload::Shared::keep` and `replace`, `replicate::Replicator::replacing`):

- A member refusing the writer (another incarnation, or full: `RESOURCE_EXHAUSTED` refuses rather
  than retries), or down for 30 s (`CLADEFSD_REPLACE_AFTER`; the user's word, 2026-11-19: 1 s was
  far too short, a peer daemon's restart costing a resend of the tail), is replaced. Every member of `c+1` is picked afresh by the
  rendezvous hash of `(cell, c+1)` over the peers that answer, the refusing aside, and every one of
  them, a survivor of `c` too, is sent the whole spooled tail into `c+1`'s own logs: no member's
  copy is shared between configurations. A replacement is swapped in once f of its members that
  take what they are sent (none down or refusing) hold all that was told replicated, never on the
  bucket alone, as the spec's Issue says; a replacement to f = 0 once the bucket holds it.
- A replacement whose members take not what the guest was told replicated within 60 s is given up
  before its CAS is sent (`Replicator::abandon`), and `c` alone counts again; picked again later,
  backing off from 1 s to 30 s, under a configuration number of its own (the one given up is never
  reused, as the spec's keys are not), its members that refused not picked again at once. Giving up after the CAS is sent would be unsafe (the spec's
  `Abandon` action, its rule `abandonUnissued` and its mutation `AbandonIssued`).
- A take that asks for another f than the configuration it recovers from replicates at the
  greater f first, and the writer then replaces it with one of the f asked for: to f = 0 by the
  rule below, after which the cell replicates no more.

### Degraded

If fewer than f peers of the active configuration answer for 1 s, the cell is **degraded**: it says
so in the stats file and in `/metrics` (`cladefsd_replication_degraded`, `…_members_up`), and logs it
as a warning, as the bucket failing does (doc-12). Its uploader closes and uploads segments at once
rather than every interval (`close_for_upload`), and it starts a replacement. Its waiters are released
by Uploaded meanwhile, by the rule above. If no peer can be had at all, the writer may swap the head
to an `f = 0` configuration (itself alone), after which takers trust the bucket alone. An `f = 0`
configuration has no quorum, so from the moment the writer starts that drop, no commit is told
Replicated: the joint rule needs a quorum in both configurations (found by TLC: with the drop's swap
landed but not yet learned, a commit told Replicated on the old configuration and the writer's own
copy, while a taker that read `f = 0` recovered the bucket alone). That swap waits until
Uploaded has reached everything the writer acknowledged as Replicated, which is the joint rule
again. It is safe because, from then on, everything the writer tells durable is Uploaded.

As built (TASK-537.3.5): degraded is judged at once, not after 1 s (a member is down from its first
failed append), and also covers a configuration of f = 0, a fenced writer, and a broken spool.
While degraded, the writer asks its uploader to close and upload every 250 ms that commits wait,
and a waiter for Replicated asks for its upload at once, as it does while a replacement is under
way. A member down for 30 s (`CLADEFSD_REPLACE_AFTER`) starts a replacement; degraded that long
and no replacement to be had,
the writer drops to f = 0, and replaces that with a configuration of the f its head asks for as
peers answer again. StatCell says `degraded`, `members_up` and `active_replicas` (the active
configuration's f); `/metrics` says `cladefsd_replication_degraded{cell}` and
`cladefsd_replication_members_up{cell}`.

### Choosing members

Members are picked from the peer list the daemon already has (`CLADEFSD_PEERS`, replaced by
`SetPeers`; `bucket/peers.rs`), by rendezvous hashing of (cell, config) over the peers that are up
(`owner_of`'s hash). Any other host of the writer's cloud region may be a member: the hosts sharing
the cell's bucket, peers of another region refusing (doc-23). No failure-domain rule: the user
decided on 2026-11-19 that a commit may be replicated to any other host in the region, so zones are
not tracked and peers carry no domain label. A peer may refuse an `Open` for capacity
(`RESOURCE_EXHAUSTED`); the writer then takes the next by hash.

### Sweeping stale replica logs

A member's logs of a configuration the head no longer names are garbage: superseded by a take or a
replacement whose `Drop` never came, or of a deleted cell. A member sweeps them by reading the head
(one GET per cell it holds) at its start and every 10 minutes for cells idle that long. It deletes a
configuration's logs once the head names a greater configuration, or the cell is gone.

### Closing and versions

- **A version** names a commit in the head, and forks on other hosts read it from the bucket. So
  making one, and a freeze, still wait for Uploaded.
- **Unmount** and the daemon closing a cell wait for Replicated. A cell closing for good (drain,
  TASK-141.4) uploads everything, then sends `Drop`.
- **Forks** see only what is uploaded, as before.
## The spec (TASK-537.2)

`specs/replication/Replication.tla` holds the writers, the members' logs, drops, promises and
incarnations, the head (epoch, active configuration, uploaded prefix), each configuration's stream
(`sent`, kept after its writer dies, as messages in flight are), crashes of a daemon (its sessions
gone, its disk kept) and losses of a disk (its logs and promises gone, a new incarnation), and
uploads in two steps (land, then the head's swap). A writer learns it was fenced only by a refused
CAS, so a fenced writer goes on as a zombie. `told` is the truth: the commits a guest was told are
Replicated or Uploaded. Each of the protocol's rules is a member of `Rules`, and each kind of step a
model may leave out (uploads, drops, sweeps, drops to f = 0) a member of `Can`, so that each model
checks the rules its steps reach in minutes: the first models, every step in each, ran past 15
minutes at 11M states and more.

| Model | What it covers | Distinct states | Time on ubu1 |
|---|---|---|---|
| `Replication` | takes: zombies, fences, recovery, activation; 1 commit, 3 takes | 182,512 | 6 s |
| `ReplicationCommits` | 3 commits, 2 takes: recovery takes the longest prefix | 136,441 | 5 s |
| `ReplicationUploads` | uploads and drops, 2 commits, 2 takes | 2,326,920 | 101 s |
| `ReplicationReplaces` | a replacement, its swap landing and learned, under takes | 3,793,205 | 190 s |
| `ReplicationZero` | a drop to f = 0 with uploads, under takes | 158,133 | 8 s |
| `ReplicationAbandons` | replacements given up before their CAS, and made again, under takes | 6,223,898 | 325 s |
| `ReplicationAlone` | a take serving alone, its acknowledgement naming its own configuration, three takes and a crash, on three hosts | 11,848,705 | 238 s |
| `ReplicationAloneRetake` | `AloneNoSwitch`'s model with every rule: the old writer's host retaking, peers' copies empty (`OwnCopiesOnly`) | 508,209 | 10 s |
| `ReplicationFailures` | a disk lost and a daemon crashed, 2 commits, 2 takes | 480,946 | 16 s |
| `ReplicationSweeps` | sweeps of superseded configurations, 3 takes | 238,260 | 11 s |
| `ReplicationDeep` (deep) | 2 commits, 3 takes | 5,544,379 | 200 s |
| `ReplicationFailuresDeep` (deep) | failures over 3 takes | 8,108,108 | 256 s |
| `ReplicationDeferCommits` | deferred takes telling commits Replicated once the head names their configuration, members' streams starting after what the take recovered (`from`), 2 commits, 2 takes, a crash | 406,820 | 14 s on the Mac |
| `ReplicationDeferReplaces` (deep) | a replacement after a deferred take, the replacement's streams starting where its configuration's do | 21,442,927 | 1,791 s on the Mac, beside other runs |
| `ReplicationDefer` | takes deferring their activation to the acknowledgement that covers what they recovered (TASK-678): the old writer's host retaking after its daemon's crash, uploads, 3 takes | 11,656,113 | 382 s on the Mac |

All on four hosts with f = 1, symmetric in hosts, under fxjob (`check-tla.sh`, 13.5 minutes for all
twenty models and mutations, the deep ones included).

**Safety**

- `ReplicatedSurvives`: every commit told Replicated or Uploaded is in the history of every writer
  that serves after it, at its place. This holds with up to f members lost, the old writer's disk
  among them, and through any number of takes, replacements and crashes in the middle of either.
- `HistoryIsPrefix`: what a writer serves after recovery is a prefix of some earlier writer's commits
  followed by its own. No two writers' commits interleave at the same place.
- `MembersArePrefixes`: in every configuration, every member's log is a prefix of its writer's.
- `DropsOnlyAcknowledged`: a member drops a record only once the head acknowledged its segment.

**Liveness** is not checked yet: every commit eventually Replicated or Uploaded, a degraded cell's
waiters released with the bucket up, and a take completing once f+1 members of the active
configuration other than the old writer are up (TASK-537.3 tests them on daemons).

**Mutations**, each a config under `mutations/` that TLC shows violating `ReplicatedSurvives` in seconds:

| Mutation | Breaks |
|---|---|
| `NoFence`: read without fencing | a zombie's ack lost |
| `FenceCountsWriter`: f+1 fences counting the old writer's member | a zombie's ack lost |
| `ReadOneMember`: recover from one fenced member, not the longest of f+1 | an ack lost |
| `ActivateBeforeInstall`: swap the head before f+1 members of `c+1` hold the tail | an ack lost after a second take |
| `NoActivation`: serve from `c+1` with the head still naming `c` | a crash-retake loses `c+1`'s acks |
| `NoJointRule`: count only `c+1` while its swap is in flight | an ack lost |
| `DropBeforeAck`: drop on the upload's landing, before the head's swap | an ack lost when the swap loses |
| `NoPromise`: members accept appends of a fenced epoch | a zombie's ack lost |
| `ZeroFWithoutUpload`: swap to `f = 0` before Uploaded catches up | an ack lost |
| `NoIncarnation`: a member is a host, whatever disk it has | an ack lost after a disk loss |
| `NoZeroQuorum`: an f = 0 configuration counts as a quorum of one | an ack lost during a drop to f = 0 |
| `AbandonIssued`: give a replacement up after its CAS is sent | an ack lost when the CAS lands after |
| `AloneNoSwitch`: a take serving alone acknowledges its own commits with the head naming `c` | an Uploaded commit lost as the old writer's host retakes, counting its longer log |
| `InstallMembersOnly`: a take activates `c+1` once its members hold what the members it read held, not its own log's commits past them (the code before TASK-678) | an ack lost: the old writer's host retakes, fencing a member that lacks it, and a take elsewhere reads `c+1` |
| `DeferTellsReplicated`: a take deferring its activation tells commits Replicated on `c+1`'s members | an ack lost: the next take reads `c` |

## What each failure keeps (rows for doc-12)

For a cell whose fsync waits for Replicated, with f = 1 (3 members):

| Failure | fsync acknowledged | sync acknowledged |
|---|---|---|
| Guest crash, VMM killed | kept | kept |
| cladefsd killed, or the host rebooted, disk kept | kept: the reopen recovers from the members and its own WAL | kept |
| Writer's host lost | kept: the take recovers from the 2 peers | kept |
| Writer's host and a peer lost | Replicated commits not yet uploaded may go: more than f failures. The take waits for the lost peer, and can't complete without it. The VM manager may then take it forced (TASK-681): on a host holding its cell, `PlaceCell`'s `force` (`cladefsctl place --force`); on a new one, `CreateCell`'s `force` (`cladefsctl take --force`). The take goes on with the members that answer and the bucket, losing that tail, and `StatCell`'s `forced_after` names the last commit it recovered | as fsync |
| A peer lost, writer up | kept; the cell replaces the peer and stays Replicated | kept |
| Both peers lost, writer up | kept: degraded, waits for Uploaded until replaced | kept |
| The bucket failing | kept, Replicated as before; waits at the lag bound | kept |
| Bucket and both peers failing | waits, as today's bucket failing | waits |

## Costs

- **Latency** of a Replicated commit: the slower of the local log sync and (a network round trip +
  the slowest of the f fastest peers' syncs). On one network, with peers' scratch disks like ubu1's
  (~2 ms for an O_DIRECT sync on ext4), that is a few ms over Local, against ~130 ms for a GCS upload.
  The flush on a Replicated fsync runs at once, as `COMMIT_LOCAL`'s does, rather than waiting for the
  50 ms tick.
- **Network:** the cell's write bandwidth times 2f, all of it. `Held` records carry the data, not
  only metadata.
- **Peers' disks:** at most the lag bound per cell (1 GiB), usually much less. It is charged to the
  tenant on the member's host (TASK-94's disk limits), and refused past them.
- **Bucket:** one more head write on each take (the activation) and each replacement. Nothing more on
  the commit path, plus the sweep's GETs.
- **Shared disks** (TASK-322): a member's syncs wait behind other VMs' bulk writes on its host, as
  the writer's do. With majority acks, one such peer doesn't stall the writer, which is why a
  majority beats write-all.

## Alternatives not chosen

- **Raft in each cell, with elections among the members:** the takes would survive the bucket being
  down, but it adds a second authority beside the head, and the user decided the bucket stays the
  authority. Its terms per entry and its truncation of suffixes exist to reuse one log across leaders,
  which a fresh log per configuration avoids.
- **Write-all (PacificA, Kafka's ISR):** f+1 copies rather than 2f+1, but every slow peer stalls the
  commit until a bucket write removes it. The user chose a majority.
- **One log per member with an epoch in every entry**, rather than a log per configuration: less
  disk churn on a take, but recovery would have to reason about entries of several writers in one
  file, which is Raft's log matching, for nothing.
- **Replicating framed segments as they close:** no new record format, but a commit would wait for
  its segment's close (up to 60 s), which is no better than the upload.
- **Peers as extra "buckets"** (uploading the same objects to a peer's file bucket): it reuses the
  uploader, but costs a PUT and a conditional head per peer and keeps the upload's batching on the
  fsync path.

## Decisions

- **The pool is the existing peer list** (`CLADEFSD_PEERS`, `SetPeers`), rather than hosts
  registering in the bucket; no failure-domain label (the user, 2026-11-19: any other host in the
  region). The interview chose "daemons
  pick from a pool registered in the bucket". But every daemon already has this list, kept by the
  VM manager for TASK-406.2, and the authority is the head's member list anyway, not the pool. A
  `members/` prefix would be a second membership with its own renewals, costing bucket writes and
  time stamps. *Undo:* the picking reads the pool from a listing instead; nothing else changes.
- **Members hold the writer's logs with every record Held, per configuration,** so that recovery is
  today's `wal::recover` and `Wal::rebase` over fetched files.
- **Degradation is a disjunction, not a mode:** waiters for Replicated are released by Uploaded too,
  always. "Degraded" only names the state for the stats, the eager uploads and the replacement.
- **No authentication for now** (the user's word, 2026-11-18). Integrity still holds, since recovery
  checks the records' MACs. A hostile host on the network could still deny service: refuse,
  fence, or drop appends, which degrades the cell to Uploaded.
- **The option is per cell**, set by `CreateCell` and `PlaceCell` (`f`, `fsync`), kept in the
  `store` file's `CellFile`. `f` goes into the head too, as part of each configuration.

## Steps (TASK-537.3, after the spec)

1. The head's `replicas` (bucket format 20), the per-cell options through the proto (`CreateCell`
   fields 9 and 10, `PlaceCell`), and the `CellFile` keys.
2. The member's side: the peer RPCs, its logs and promise, written O_DIRECT through
   `local_cell::pages`, then `Fence`, `ReadLog`, `Drop` and the sweep.
3. The writer's side: Held records from each flush to the streams, the members' ends, the
   Replicated waiter, and the commit levels in `OpenCell::commit_and_wait` (`open_cell.rs:1557`).
4. Take: fence, read, rebase of a tail after `last_commit`, install, activate, in `upload::start`
   (`upload.rs:1012`) before `Recovering::open`.
5. Replacement and degraded mode, with metrics.
6. Tests: unit tests of each side with fake links; `local_cell`'s simulation with members
   (TASK-680: profiles `replicated` and `retakes`, the network between hosts breaking, delaying,
   duplicating and dropping calls, disks crashing corrupting what they never synced, members
   driven a turn at a time; it found TASK-680's refusal of a base gap and TASK-683); three daemons on ubu1, killing the
   writer's daemon and its disk mid-pgbench and checking every Replicated commit after the take;
   doc-12's rows.

## Where this meets other work

- **TASK-96** (the extent store as a standalone layer) settled a "peer log" of one peer named in the lease.
  This supersedes it: a majority, and the head rather than a lease.
- **TASK-224** (fsync waiting for Local) becomes the `fsync = Local` option.
- **TASK-322 and TASK-384** (disk sync tails) bound a Replicated commit's latency on the writer's
  side as they bound Local's.
- **TASK-406.2**'s peer network carries it, with a stream endpoint of its own.
