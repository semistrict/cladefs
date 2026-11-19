---
id: doc-23
title: 'Cells across buckets: a bucket per cloud region, forks that span them'
type: specification
created_date: '2026-11-18 17:01'
updated_date: '2026-11-19 09:34'
---
# Cells across buckets: a bucket per cloud region, forks that span them

Design, 2026-11-18. The user chose the direction that day. This doc gathers what the code assumes
today, quoting it by text as of 8368dfe5. It then says what changes, what the TLA+ spec must check,
and the order of the work. Two research passes found that nothing in the backlog, the docs or
OPEN_QUESTIONS.md had planned for more than one cloud region before this.

"Region" in CONTEXT.md is the guest's DAX term. Here **cloud region** is the geographic one.

## The user's decisions (2026-11-18)

| Question | Decision |
|---|---|
| What multi-region means | **A bucket per cloud region.** Each cell has one **home bucket**, which holds its head, its segments and its versions. Hosts in a cloud region use its bucket. |
| Clouds | **All three:** S3, GCS and Azure Blob Storage, each bucket on any of them. |
| Forks across buckets | **Required.** A child in bucket B is forked from a version of a parent in bucket A, and keeps its origin and lineage. A one-way flattened copy into B, a new cell with new IDs, was rejected. |
| Copies | **Allowed as a mirror.** The parent version's objects are copied byte for byte into B at the same keys, in the background, so that B's children stop depending on A. This is not a flattened copy: no ID, key or record changes. **Automatic** (the user, 2026-11-18): the first fork of a version into another bucket starts its mirror there; children read A lazily until the mirror's marker lands, then B. |
| Cell IDs | Unique across every bucket: a **16-bit bucket number** in a cell ID's high bits. This is a format bump. |
| Bucket numbers | **Sequential**, from the **root bucket**, number 1. Adding a bucket gives it the next number by a conditional write (CAS) of the root's record of every bucket, which serializes additions. Each number is kept in a create-only identity object in its bucket. (Superseded the same day: random numbers, an explicit one as an option, and a registry in each host's configuration.) |
| Record of buckets | **In the root of every bucket**: each bucket's number, provider, cloud region, URL, endpoint, KMS and master key's ID; never a key or a credential. The root's copy is the authority; every other bucket holds a copy no older than the one before it. |
| Credentials | Every host and collector may **write to every bucket**: a fork and a pass write across buckets themselves. No proxy through A's hosts. |
| Caching remote objects | **Cached like any other** (the user, 2026-11-18): parts read from another cloud region's bucket go into the host's part cache and are served by its own region's peers. **Peers never cross a cloud region, under any circumstances:** the peer network is partitioned by region, a host's region being its home bucket. |
| Not chosen | Hosts of several cloud regions sharing one bucket, as the general shape. Surviving the loss of a bucket's cloud region for the cells it is home to (DR). doc-21's replicas spread across cloud regions. |

## Why not the other shapes

- **Many cloud regions over one bucket** is correct today. A head's conditional write is decided at the bucket, wherever its caller is. But it pays the inter-region round trip on every serial request, and egress on every part. A cold restore is 13 serial round trips: `("cold copy-open", (5, 4)), ("cold fork", (6, 6)), ("cold fork-open", (3, 3))` in `crates/cladefs/src/bucket/lifecycle.rs`. That is about 1–1.5 s across the Atlantic before a VM boots.
- **DR of a cell's home bucket** cannot be had on S3 or Azure while the head is the only authority (doc-21: "Authority | The bucket"). Their geo-replication is asynchronous (S3 CRR and MRAP, Azure GRS). After a failover, two cloud regions could each take one cell. Only GCS dual-region and multi-region buckets keep conditional writes consistent across cloud regions (https://cloud.google.com/storage/docs/consistency), and their data still replicates behind: RPO 15 min with turbo replication, 12 h without. A cell whose home is a GCS dual-region bucket gets that much, with nothing in cladefs changed.
- **doc-21's members in other cloud regions** would keep the unuploaded tail, but not availability: a take needs the head first. Every Replicated commit would pay a WAN round trip, plus 2f times its bytes in egress.

## What the code assumes today

### A fork is in its parent's bucket

- `make_child` (`crates/cladefs/src/host_cell.rs`) makes the child naming the parent's bucket: `let url = parent.bucket().map(str::to_owned)`.
- `Uploader::forked` (`crates/local_cell/src/upload.rs`) calls `lifecycle::fork_version_keyed(&*self.bucket, child.cell, at.version, at.origin, given, keys)`. That one call writes the child's head, the reference `<P>refs/<at>/<TENANT>.<CELL>` and the checkpoint at the version, all in A.
- `lifecycle.rs`'s module doc says the opposite ("its child's head alone, which writes nothing of the version's cell's"). It is stale.

### Names carry no bucket

- `pub struct Origin { pub parent: CellId, pub at: u64 }` (`bucket_cell/src/layout.rs`)
- `pub struct Location { pub key: String, pub offset: u64, pub len: u64 }` (`extent_frame/src/segment.rs`)
- `pub struct ExtentId { pub cell: u64, pub sequence: u64 }` (`storage_device/src/extent.rs`). Fxfs keeps this on disk too, in its extent records.
- Reference keys (`layout::reference(parent, at, child)`), `KeyId { cell, generation }`, and the HKDF bindings of subkeys.
- Cell IDs: "unique in their bucket" (`bucket_cell/src/objectid.rs`). An ID is `block << 24 | n`, the block taken at random by creating `objectid/<block>`, with `ID_BLOCK = 1 << 24`, giving 2^40 blocks.
- The extent cache is keyed `HashMap<(ArenaId, ExtentId), Cached>` (`cladefs/src/cache.rs`), and a hit is not checked again. Two buckets that gave one ID would serve each other's pages silently. The part cache is keyed by object key alone.

### One bucket, one peer set, one KMS a process

- New cells go to `CLADEFSD_BUCKET`. `host_cell.rs` keeps `static BUCKETS: Mutex<BTreeMap<String, Arc<ObjectBucket>>>`, so a store whose description names another URL opens that bucket. But a URL is not an identity: `s3://b` and `https://s3.<region>.amazonaws.com/b` are one bucket.
- `ObjectBucket::from_url` is `reach(url, std::env::vars(), &Client::DEFAULT)`, so every bucket of a process shares one environment: one `AWS_REGION` (object_store falls back to `us-east-1`) and one Azure account.
- The fetcher is built over one bucket, `fetch::Fetcher::new(bucket, BUDGET.clone(), MEMORY.clone(), cache)`, with one GET budget a host: `Budget::new(Limits { requests: 64, bytes: 64 << 20 })` (`cladefs/src/bucket.rs`).
- Peers serve only the host's bucket. The client side has `PEERS.get().filter(|(theirs, _)| Arc::ptr_eq(theirs, &bucket))`. The server refuses others with `if bucket != self.bucket {` (`cladefsd/peers.rs`).
- KMS: `static POOLED: std::sync::OnceLock<Arc<keys::CachingKms>>`. `aws-kms://REGION` is bound to one cloud region. The bucket KMS keeps `tenants/<t>/kek` in the host's bucket. No Azure Key Vault backend exists.

### A take walks every ancestor's head

`keys::ring` (`bucket_cell/src/keys.rs`) runs `head::get(bucket, parent)` for every ancestor, even when the child's head holds every key it inherited (`given`). Its callers are the uploader's start (`keys::ring(&**bucket, &cell_keys, cell, &held.head, true)`) and `writer.rs`'s `open_under`. A writer's checkpoint also follows the moves of inherited objects, with `maps.read(&*self.bucket, keys)`. So a child cannot be taken, or checkpoint, while any ancestor's bucket is out of reach, even with a checkpoint of its own.

### Keys already cross cells

A fork rewraps the parent's DEKs, as of the fork, under the child's cell KEK: `Inheriting::given` → `wrap_inherited(kek, child, of, id.generation, dek)` (doc-16, Forks and shared versions). After its fork, a child needs only its own tenant's KMS. Forks into other tenants' cells work this way today. Across buckets, the same holds with the forking host using two KMSes.

## The design

### Bucket numbers and cell IDs

- **A cell ID is `bucket << 48 | block << 24 | n`.** That is a 16-bit bucket number above a 24-bit block, with 2^48 IDs per bucket. `id >> 48` names a cell's home bucket. `Origin`, `Location`, `ExtentId`, `KeyId`, the HKDF bindings, both caches and the proto keep their shapes, and every name becomes unique across buckets.
- **Bucket number 0** is never given: IDs under it are the ones tests name (`objectid::named`; TASK-455's block 0 was part of it). Nor is 65535, so `u64::MAX` still names no cell.
- **The allocator** takes blocks within its bucket's number: `objectid/<bucket << 24 | block>`.
- **Format bump:** bucket format 19 → 20, or the next number free when this lands. Nothing migrates (README, "Formats change without migration").

### A bucket's identity and the record of every bucket

- Every bucket holds a create-only object at its root, `identity`: its format, its **bucket number** and a random ID (`bucket_cell::identity`). `layout::kind` knows it (`Kind::Identity`), and the collector keeps it.
- The **root bucket**, number 1, holds the **registry** at `buckets`: its version, and each bucket's number, identity ID and details (provider, cloud region, URL, endpoint, KMS, master key's ID). It is the authority. Every other bucket's root holds a copy (`Kind::Buckets`), which is never replaced by an older one.
- **Formatting** (`cladefsctl format-bucket URL [DETAILS]`) makes a bucket never formatted a root of its own: identity number 1, and a registry recording it alone. An allocator formats its bucket so before its first block, so every bucket that holds a cell has a number. A formatting that stopped short of the registry is finished by the next.
- **Adding** (`cladefsctl add-bucket ROOT URL [DETAILS]`) takes a bucket never formatted:
  1. Read the root's registry, and write it back with the next number added, conditional on its version. A write that loses retries with the number after. Additions are serialized here; nothing else ever writes the root's registry.
  2. Create the new bucket's identity, of that number.
  3. Copy the registry to every bucket's root, conditional on each copy's version, never replacing a newer one.
  - An addition that stopped after step 1 is finished by another of the same URL, which takes the recorded number, not the next. Adding a bucket again keeps its number.
  - Refused: a bucket that already has a number (`Error::Formatted`, `EEXIST`), a root that is not number 1 (`Error::NotRoot`, `EINVAL`), and no number left (`Error::NoNumbers`, `ENOSPC`).
- The root bucket's cloud region being down blocks only additions. Hosts read the copy in their own bucket's root, and the root's only for a number their copy lacks.
- These are TASK-633.9, done before the rest, so that IDs given from then on stay valid once buckets meet.
- Credentials and other per-host options for each bucket stay in each host's configuration, by bucket number (TASK-633.3). `CLADEFSD_BUCKET` names the host's home bucket.

### Routing by bucket

One **resolver** maps a cell ID to its bucket: `id >> 48` → the registry → an `ObjectBucket` with that bucket's own options (`reach(url, options, …)`, not the process environment). It replaces the URL-keyed `BUCKETS`.

- **Fetchers** route each key by `layout::cell_of(key)`, each bucket with its own GET budget.
- **Recovery's checkpoints** and a writer's moves read each inherited object's moves from its own bucket.
- **Peers, partitioned by cloud region** (the user, 2026-11-18). A host's region is its home bucket. Every peer request names the asking host's home bucket number, and a peer refuses any whose number is not its own. A host's peer list holds hosts of its region alone, and it asks no other, whatever its list says. Within a region, a peer serves a part of **any** bucket: the region's owner of a part (`owner_of`) fetches it on a miss, from its own bucket or another region's, with its own credentials, and keeps it in its part cache. So a part of a remote bucket crosses regions about once per region, not once per host, and never between peers. The part cache keys stay as they are, since keys are now unique across buckets, and a remote object's parts are cached exactly as a local one's.
- The **part cache's per-bucket bytes**, and every GET, carry the bucket's number in the stats and `/metrics`, so that cross-bucket egress shows.

### Taking a cell asks only its own bucket

- **`keys::ring` stops at a head that is `given`.** It checks the inherited keys' completeness against the head's own records, and reads no ancestor's head. A head with no keys of its own, made before its fork finished, still walks. Its fork is then resumed, which needs the parent's bucket, as today.
- **A checkpoint follows inherited objects' moves in their own bucket,** or in the child's bucket once a mirror holds them (below).

After this, a child whose parent's bucket is out of reach is taken, commits, uploads and checkpoints. Only its cold reads of inherited extents wait, and the mirror removes those.

### KMS routing

- A cell's keys are wrapped by its tenant's KEK in **its home bucket's KMS**. The KMS is configured per bucket in the registry. The cache of KMSes is keyed by (tenant, KMS).
- **One KMS refusing revokes the tenant there alone,** not on every bucket. An unwrap that crosses buckets under the bucket KMS is refused, never reported as `Destroyed`.
- **Shredding a tenant** destroys its KEK in every KMS that holds one. A cross-bucket child is shredded with its own tenant: its keys are its own, rewrapped at the fork.
- **Azure Key Vault** joins OpenBao, AWS KMS and Google Cloud KMS as a backend.

### A fork across buckets

**A child lives in the forking host's bucket** (decided 2026-11-18, TASK-633.7): a host forks a version by opening it through its own bucket's router, which sends the parent's keys to the parent's bucket, and gives the child an ID of its own bucket. So `ForkCell` names no bucket: a VM manager forks into a region by asking a host of that region, which is where the VM runs anyway. `versions_forked_on_a_host_of_another_bucket_make_children_there` checks it.

The forking host, in B's cloud region, forks version `at` of P (home A) into a new cell C (home B):

1. Allocate C's ID from B's allocator.
2. Wrap P's DEKs as of the fork under C's cell KEK from KMS B (`Inheriting`). This needs Decrypt on P's KEK in KMS A, as forks into other tenants' cells do today.
3. In **B**: C's head, being made, its `Origin { parent: P, at }` naming A by P's ID, and its inherited keys.
4. **Where its reference goes**, read in B: B if the version's mirror is marked there, else A. Then the reference `<P>refs/<at>/<C>` written there, and the checkpoint at the version in A, ordered as `fork_version_keyed` orders them today.
5. **P's head read in A**, naming the version, or the fork fails (forks write their reference first, `forksReferFirst`, as in one bucket).
6. **Having referred in A**, the marker read again in B: if it is there now, the reference written in B, then its own in A deleted (`forksRecheck`, `forksLeaveA`). Referring in B at once when marked only saves a fork after a mirror those two writes.
7. C's head made.

**A fork reads P's head in A, so a fork into B waits for A, even once the version is mirrored.** A fork going by the marker alone races B's collector deleting the copy: the collector would have to drop the marker, then list references, and a fork that read the marker before the drop leaves a child reading A for a version A no longer keeps (worked through for TASK-633.2). Mirrors make children's reads independent of A, not new forks.

Every step is one object's conditional write, and each survives a crash where it is, as today. No transaction across buckets is needed: each bucket is linearizable for its own objects (S3, GCS and Azure all are, Azure's primary alone; a geo-redundant secondary is never read). Writes to two objects in two buckets are ordered in real time by the forking host, which is all the order the protocol uses.

**Collection across buckets:**
- B's pass over C, once C is gone or its version no longer needs P's state, writes the `retired` mark and deletes the reference **in A**, as it does in one bucket today. doc-7 keeps the reference under the parent's cell (TASK-395.1), so the parent's own pass sees its listing change.
- A pass that cannot reach a bucket it needs **deletes nothing** of what depends on that bucket, and records no listing digest (TASK-395.6), so that a skipped pass does not hide the change when the bucket returns.
- A's collector needs nothing new: it keeps P's state at `at` while a reference stands, wherever its child lives.

### Mirrors

A **mirror** is a copy of a version's state in another bucket, at the same keys: the checkpoint at `at`, every object it names, and their moves objects, byte for byte. Copies are authentic where they land (`extent_frame/src/lib.rs`: "A whole object copied to another key is authentic there"), and keys are written once (TASK-407), so a copy and its source cannot differ.

- **The order** (checked by `specs/buckets`, TASK-633.2):
  1. In B, the mirror's **pin**: a reference of its own under P's prefix, which keeps the copy from B's collector while it is made.
  2. **P's head read in A**: the version dropped, the mirror is given up (its pin dropped, no marker). Without this read, a pass of B that read the version dropped and listed B's references before the pin deletes the copy under the marker (TLC found it, `mirrorChecks`).
  3. **Copy** every object into B. Server-side where the clouds allow it: S3 `CopyObject` across AWS cloud regions, GCS `objects.rewrite`, Azure Put Blob From URL (which pulls from any URL, S3's presigned ones included). Into S3 from another cloud, a host must GET and PUT. An object gone from A before it is copied gives the mirror up.
  4. In B, the create-only **mirror marker** under P's prefix, naming `at` and the checkpoint's epoch, which `state_at` finds today from P's head, in A: only once every object is copied (`markerAfterCopy`).
  5. **List the children referring in A** to the version, after the marker (`mirrorListsAfterMark`: listed before, a fork that referred in A and read the head before the marker is missed), and write their references in B.
  6. In A, the `retired` mark of each such child's reference, then its deletion.
  7. Drop the pin.
  - A mirror's host may die at any step: the mirror starts again from its pin, what it wrote kept (writes are create-only or idempotent). One that finds its marker already in B goes on to step 5 (`mirrorResumesMarked`): reading the head instead, it would give up if the version was dropped since, its children left referring in A (TLC found it).
- **Reads:** the resolver serves P's keys at `at` from B once the marker is there, and from A before it.
- **One mirror serves every child in B of that version,** and later forks of it into B refer to it in B alone.
- **B's collector** keeps a copy while P's head names its version (read first, in A: `copiesKeptWhileNamed`, `copyRelists`) or a reference in B to it stands; otherwise it deletes the marker, then the objects (`markerGoesFirst`). A mirror has no head, no writer and no KEK of P's, so no compactor ever touches it.
- **What hydrates, and when:** every version forked into another bucket, at once (the user, 2026-11-18). The first such fork starts the mirror in the background; later forks of the version into that bucket find it under way or done. The mirror's create-only marker is the switch: before it, reads go to A; once it exists, every host reads B.
- **As built (TASK-633.8):** `bucket_cell::mirror` runs the order above; `ObjectBucket` serves a marked version's keys from B first, and falls back to A for any it lacks; `Collector::collect_copy` is B's pass over a copy; forks refer through `Bucket::mirrors` (`mirror::Here`). A process runs one mirror of a version at a time. Choices the spec leaves open:
  - **One mirror at a time per version and bucket, by its pin.** The pin's bytes are its mirror's own (a random run ID). A mirror that finds another's pin standing does nothing: the mirror is under way. Two mirrors sharing one pin would break `mirrorPins`: the first to finish drops the pin from under the second.
  - **A pin goes stale** after `PIN_STALE` (10 minutes) unrefreshed, by B's clock. Its mirror refreshes it every minute while it copies, and right before its marker, with a conditional write: it gives up, unmarked, if the pin is no longer its own. A stale pin keeps nothing from B's collector, and the next mirror takes it over by a conditional write. This is a lease's assumption: a host stopped for longer than `PIN_STALE` between its refresh and its marker, while the version is dropped and its copy collected meanwhile, would mark a copy that is not whole.
  - **Resuming:** B's collector, finding a stale pin, begins that mirror again in its pass. The next fork of the version into B does too.
  - **Simplifications:** copies are GET and PUT (server-side copy is not done yet). A mirror copies only the objects under P's own prefix; objects of other cells that the version inherited are read where they are. B's collector keeps a cell's whole copy while any of its versions is needed. A mirror's writes go through B's own store, whose requests are counted apart from `stats_by_bucket`; fetch budgets charge a read that B serves from a copy to A, the cell's bucket.
- **Prior art:** Fly volumes fork across regions and hydrate behind, usable meanwhile (https://fly.io/docs/volumes/volume-states/). EBS loads a volume lazily from its snapshot. Cloudflare R2's Sippy migrates lazily.

### What the parent's bucket being out of reach costs

| | Before its mirror | After its mirror |
|---|---|---|
| Taking, committing, uploading a child that has a checkpoint of its own | works (a take asks only its own bucket, TASK-633.5) | works |
| A child's checkpoint | waits for A: it follows the moves A's collector made of the objects it inherited, under the parent's prefix, in A | works once its mirror routes the parent's version's keys to B (TASK-633.8) |
| Opening a child with no checkpoint of its own | waits for A: it recovers through its origin's state | works once mirrored |
| Reads of inherited extents cached in memory or the part cache | work | work |
| Cold reads of inherited extents | wait for A | work |
| New forks of P's versions | wait for A | wait for A: a fork reads P's head |
| B's passes over references in A | delete nothing, retry | none left in A |

## Costs

- **A cold inherited read across buckets:** a tail GET of 64 KiB, then the part, so 2 round trips, about 140 ms between us-east-1 and eu-west-1. A tail larger than 64 KiB takes a third. The region's peers bring each part across about once per region, not once per host; no peer of another region is ever asked.
- **Egress per GB:**

  | Route | Price |
  |---|---|
  | S3 between AWS cloud regions | $0.01–0.02 |
  | GCS within North America | $0.02 |
  | GCS between continents | $0.05–0.08 |
  | Azure within a continent | $0.02 |
  | Azure between continents | $0.05 |
  | Across clouds (internet egress) | $0.09–0.12 |

  Sources: each provider's pricing pages, 2026-11.
- **A mirror** costs the version's objects' bytes once per bucket, dead extents included. For a 20 GiB base that is about $0.40 within AWS's US and about $1.80 across clouds, against $0.04–0.18 per host for each 2 GiB read lazily.
- **Bucket requests:** a fork across buckets makes the same requests as one within a bucket, some of them in A. A mirror adds one COPY or GET+PUT per object, plus the marker and the references.

## What the spec checks

`specs/buckets/Buckets.tla` (TASK-633.2), cut down from `specs/bucket`'s forks, references and collection, as `specs/replication` was: one parent cell P at home in A, children forked into B, P's writer dropping versions, children deleted, the automatic mirror (a host dying at any step of it), the collectors of P in A, of P's copy in B, and of each child, and buckets going down. Each step reads or writes one bucket.

- **Invariants:** every child made reads its version whole from where the resolver sends it, and every version P's head names is whole in A (`NothingNeededGone`); a marker never names an incomplete copy (`MarkersWhole`); every reference is a pin or names its child's version (`ReferencesNamed`); every child made holds a reference (`ChildrenReferred`); once a mirror is done, every child of its version refers in B and not in A (`MirroredChildrenOnB`).
- **Forks against the mirror at every point:** before it starts, while it copies, between its last copy and its marker, after the marker and after it is done; racing each other, the collectors and the version's drop.
- **Rules, each with a mutation TLC shows breaking an invariant:** `forksReferFirst`, `forksRecheck`, `forksLeaveA`, `mirrorListsAfterMark`, `mirrorPins`, `mirrorChecks`, `mirrorResumesMarked`, `markerAfterCopy`, `copiesKeptWhileNamed`, `markerGoesFirst`, `copyRelists`. Three models (`Reaches*`) show the spec reaches a child made, a mirror done with a child, and a copy collected.
- **TLC found** the mirror's head read after its pin was needed, and a mirror started again after its marker must not read the head again, and that two steps of the first design were not: a fork referring in B at once (the read again covers it) and a mirror listing children before its copy (the listing after the marker covers it).
- **Buckets down:** a step a bucket down refuses changes nothing, so every behaviour with a bucket down is one without it, its steps later; the safety models leave them out, and `BucketsLive` checks with them, under fairness, that every mirror begun ends.
- **Not modelled:** the root bucket's registry (TASK-633.9's tests), compaction and moves (a copy takes moves objects too, as more objects), the children's own data, and P's collection beyond its versions (`specs/bucket`'s).

## Order of the work

Nothing here waits for a measurement (TASK-633.1, archived 2026-11-18): a mirror starts with the first fork of its version into a bucket, and a cold read across buckets costs what Costs says.

1. **The spec** (above).
2. **Bucket numbers in cell IDs**, the identity object and the record of buckets in every root (TASK-633.9, done first), then hosts finding buckets by number (TASK-633.3).
3. **Routing:** options per bucket, the resolver, fetchers, budgets and peers per bucket, and per-bucket stats.
4. **A take that asks only its own bucket,** and a checkpoint's moves read in their own bucket.
5. **KMS per bucket,** and the Azure Key Vault backend.
6. **Forks across buckets** (`ForkCell` names the child's bucket), and collection across buckets.
7. **Mirrors:** a copy for each pair of clouds, the marker, the switch, and their collection.

## Where this meets other work

- **doc-21 (TASK-537):** no failure-domain rule (the user, 2026-11-19): a commit may be replicated to any other host in the cell's home cloud region, the hosts sharing its bucket. A member elsewhere would keep the tail but give no availability. Members, like peers, are kept per bucket.
- **TASK-406.2 and TASK-406.3:** the peer network and the warming of a version, which become per bucket. Warming is the model for asking for a mirror.
- **doc-16:** KMS backends, and shredding, now per bucket.
- **doc-7:** object IDs, references (TASK-395.1) and the skip pass (TASK-395.6).
- **CONTEXT.md** gains **home bucket**, **bucket number**, **registry** and **mirror** when the code does.
