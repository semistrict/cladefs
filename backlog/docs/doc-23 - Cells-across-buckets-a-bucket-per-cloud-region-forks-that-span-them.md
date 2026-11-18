---
id: doc-23
title: 'Cells across buckets: a bucket per cloud region, forks that span them'
type: specification
created_date: '2026-11-18 17:01'
updated_date: '2026-11-18 20:23'
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
| Copies | **Allowed as a mirror.** The parent version's objects are copied byte for byte into B at the same keys, in the background, so that B's children stop depending on A. This is not a flattened copy: no ID, key or record changes. |
| Cell IDs | Unique across every bucket: a **16-bit bucket number** in a cell ID's high bits. This is a format bump. |
| Bucket numbers | **Sequential**, from the **root bucket**, number 1. Adding a bucket gives it the next number by a conditional write (CAS) of the root's record of every bucket, which serializes additions. Each number is kept in a create-only identity object in its bucket. (Superseded the same day: random numbers, an explicit one as an option, and a registry in each host's configuration.) |
| Record of buckets | **In the root of every bucket**: each bucket's number, provider, cloud region, URL, endpoint, KMS and master key's ID; never a key or a credential. The root's copy is the authority; every other bucket holds a copy no older than the one before it. |
| Credentials | Every host and collector may **write to every bucket**: a fork and a pass write across buckets themselves. No proxy through A's hosts. |
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
- **Peers** are kept per bucket number. A peer serves a part of any bucket in its registry, and the part cache keys stay as they are, since keys are now unique.
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

The forking host, in B's cloud region, forks version `at` of P (home A) into a new cell C (home B):

1. Allocate C's ID from B's allocator.
2. Wrap P's DEKs as of the fork under C's cell KEK from KMS B (`Inheriting`). This needs Decrypt on P's KEK in KMS A, as forks into other tenants' cells do today.
3. In **A**: the reference `<P>refs/<at>/<C>` and the checkpoint at the version, ordered as `fork_version_keyed` orders them today (`NoForksReferFirst`, `NoForksRecordFirst`).
4. In **B**: C's head, its `Origin { parent: P, at }` naming A by P's ID, and its inherited keys.

Every step is one object's conditional write, and each survives a crash where it is, as today. No transaction across buckets is needed: each bucket is linearizable for its own objects (S3, GCS and Azure all are, Azure's primary alone; a geo-redundant secondary is never read). Writes to two objects in two buckets are ordered in real time by the forking host, which is all the order the protocol uses.

**Collection across buckets:**
- B's pass over C, once C is gone or its version no longer needs P's state, writes the `retired` mark and deletes the reference **in A**, as it does in one bucket today. doc-7 keeps the reference under the parent's cell (TASK-395.1), so the parent's own pass sees its listing change.
- A pass that cannot reach a bucket it needs **deletes nothing** of what depends on that bucket, and records no listing digest (TASK-395.6), so that a skipped pass does not hide the change when the bucket returns.
- A's collector needs nothing new: it keeps P's state at `at` while a reference stands, wherever its child lives.

### Mirrors

A **mirror** is a copy of a version's state in another bucket, at the same keys: the checkpoint at `at`, every object it names, and their moves objects, byte for byte. Copies are authentic where they land (`extent_frame/src/lib.rs`: "A whole object copied to another key is authentic there"), and keys are written once (TASK-407), so a copy and its source cannot differ.

- **The order:**
  1. Copy every object into B. Server-side where the clouds allow it: S3 `CopyObject` across AWS cloud regions, GCS `objects.rewrite`, Azure Put Blob From URL (which pulls from any URL, S3's presigned ones included). Into S3 from another cloud, a host must GET and PUT.
  2. In B, the reference `<P>refs/<at>/<C>` for each child in B that the mirror serves.
  3. In B, a create-only **mirror marker** under P's prefix, naming `at` and the checkpoint's epoch, which `state_at` finds today from P's head, in A.
  4. In A, the `retired` mark of each such child's reference.
  5. In A, the deletion of those references.
- **Reads:** the resolver serves P's keys at `at` from B once the marker is there, and from A before it.
- **One mirror serves every child in B of that version,** and later forks of it in B are made against the mirror: their references are B's alone.
- **B's collector** keeps a mirror while a reference to it stands, and removes it whole when none does. A mirror has no head, no writer and no KEK of P's, so no compactor ever touches it.
- **What hydrates, and when:** an explicit control call, as warming a version is (TASK-406.3). Policy, such as hydrating at once across clouds and lazily within one, is the VM manager's, until measurements say otherwise.
- **Prior art:** Fly volumes fork across regions and hydrate behind, usable meanwhile (https://fly.io/docs/volumes/volume-states/). EBS loads a volume lazily from its snapshot. Cloudflare R2's Sippy migrates lazily.

### What the parent's bucket being out of reach costs

| | Before its mirror | After its mirror |
|---|---|---|
| Taking, committing, uploading, checkpointing a child | works (once a take asks only its own bucket) | works |
| Reads of inherited extents cached in memory or the part cache | work | work |
| Cold reads of inherited extents | wait for A | work |
| New forks of P's versions | wait for A | made against the mirror, in B |
| B's passes over references in A | delete nothing, retry | none left in A |

## Costs

- **A cold inherited read across buckets:** a tail GET of 64 KiB, then the part, so 2 round trips, about 140 ms between us-east-1 and eu-west-1. A tail larger than 64 KiB takes a third. Peers keyed by bucket bring each part across about once per cloud region, not once per host.
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

## What the spec must check

`specs/bucket/Bucket.tla`, with its configs `BucketRefsForks`, `BucketResumes` and `BucketInherit` and their mutations:

- **Cells are `<<bucket, id>>`.** References, checkpoints and objects live in buckets.
- **`Down ⊆ Buckets`,** with steps that take a bucket down and bring it back. A step that needs a bucket in `Down` cannot run.
- **`GcFinish` split** into its retirement and its removal, which may then be in different buckets.
- **Mirrors:** a copy step, the marker, and readers that read A until the marker exists.
- **Invariants:** `NothingNeededGone`, `ReferencesNamed`, `ReferencesStand` and `NoIdTwice` hold unchanged, across buckets.
- **New rules, each with a mutation TLC shows breaking one:**

  | Rule | Mutation breaks |
  |---|---|
  | retire before remove, across buckets | `ReferencesNamed` |
  | a mirror's reference in B before the retirement in A (`NoMirrorReferFirst`) | `NothingNeededGone` |
  | a pass short of a bucket deletes nothing (`NoPassesWhileDown`) | `NothingNeededGone` |
  | a take asks only its own bucket once given, and holds every key it needs (`NoGivenComplete`) | a key's completeness |

- **Liveness, under weak fairness and every bucket back up:** every reference that should go is eventually retired, and every mirror asked for is eventually complete.

Each model runs in under 10 minutes, split by the steps it takes, as Replication.tla's are.

## Order of the work

1. **Measure.** How much of a base a child reads (bytes read over the version's object bytes), on real container-image bases. A version's dead-byte share, which a mirror copies. A cold inherited read under injected latency (`CLADEFS_BUCKET_LATENCY_MS`, about 70 ms) on ubu1. The real latency of each cloud pair needs real buckets, which the no-GCP rule forbids without the user's word.
2. **The spec** (above).
3. **Bucket numbers in cell IDs**, the identity object and the record of buckets in every root (TASK-633.9, done first), then hosts finding buckets by number (TASK-633.3).
4. **Routing:** options per bucket, the resolver, fetchers, budgets and peers per bucket, and per-bucket stats.
5. **A take that asks only its own bucket,** and a checkpoint's moves read in their own bucket.
6. **KMS per bucket,** and the Azure Key Vault backend.
7. **Forks across buckets** (`ForkCell` names the child's bucket), and collection across buckets.
8. **Mirrors:** a copy for each pair of clouds, the marker, the switch, and their collection.

## Where this meets other work

- **doc-21 (TASK-537):** its failure domain is still undecided between zone and cloud region. With a bucket per cloud region, a cell's members belong in its home cloud region, since a member elsewhere keeps the tail but gives no availability. That favours a zone, and should be settled before TASK-537.3. Members, like peers, are kept per bucket.
- **TASK-406.2 and TASK-406.3:** the peer network and the warming of a version, which become per bucket. Warming is the model for asking for a mirror.
- **doc-16:** KMS backends, and shredding, now per bucket.
- **doc-7:** object IDs, references (TASK-395.1) and the skip pass (TASK-395.6).
- **CONTEXT.md** gains **home bucket**, **bucket number**, **registry** and **mirror** when the code does.
