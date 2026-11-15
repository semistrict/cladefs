---
id: doc-16
title: 'Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs'
type: specification
created_date: '2026-11-14 18:32'
updated_date: '2026-11-15 00:21'
---
# Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs

Design, 2026-11-14. The user's decisions of that day are marked *(decided)*. TASK-411.1 (keys, KMS), TASK-411.2 (frame FORMAT 2), TASK-411.3 (cell KEKs, DEKs, rings, below: **What TASK-411.3 made**) and TASK-411.4 (forks into other cells, pools; below: **What TASK-411.4 made**) are implemented.

## Goals

*(decided)* Three, and no more:

1. **BYOK.** A tenant's root key (its KEK) lives in a KMS, possibly the tenant's own; fxdax gets
   wrap and unwrap rights only, which the tenant can revoke.
2. **Crypto-shredding.** Destroying a tenant's KEK leaves its data unreadable everywhere it is:
   the bucket, its object versions and copies, hosts' disks.
3. **A bucket read by someone else learns nothing useful** of a tenant's data.

**Not a goal:** keeping plaintext from a compromised host. The extent cache holds plaintext pages,
as DAX needs: one copy, mapped into every VM's window (README, doc-13). Keeping the host blind needs
encryption in the guest (Fxfs's fscrypt), and Linux turns DAX off for encrypted inodes, which
loses fxdax's reason to be. Guest-side encryption stays possible as a tenant's opt-in without DAX;
it is not this design.

## Keys

```
tenant KEK   in the KMS; never leaves it. Destroying it shreds the tenant.
  cell KEK     random; wrapped by the tenant KEK; kept in the cell's object (<C>cell)
    volume DEK   random; wrapped by the cell KEK; kept in the branch's record (<C>branches/<b>)
      object subkey = HKDF-SHA256(DEK, salt || object key); salt random per object, in its trailer
```

*(decided: tenant, cell, volume)* Each level is a unit the bucket already has (doc-7, Layout):

- **Volume DEK, one per branch.** A branch is the only writer of its objects
  (`segments/<branch>/`, `extents/<branch>/`, `checkpoints/<branch>/`), and recovery reads its
  record and its ancestors' records already.
- **Cell KEK.** The cell is the unit of placement: a host taking a cell unwraps its KEK with one
  KMS call and holds every volume's DEK from then on. It also scopes forks' rewrapping (below).
- **Tenant KEK.** The only key the KMS holds; BYOK and shredding act on it.

A DEK has generations: a record keeps a list of `(generation, wrapped DEK)`, a new writer may add one,
and every object's trailer names the branch and generation its subkey came from. Rotating a KEK
rewraps what it wraps (the KMS's own rotation, or a conditional update of each record or cell
object); rotating a DEK starts a generation, and old data moves to the new one as compaction and
checkpoints rewrite it.

**Why a subkey per object, from a random salt:** object keys are create-only, but a fenced writer
keeps its own bytes for `segments/<branch>/<seq>` in its WAL after another epoch's landed there
(doc-14). Nonces from the key alone would repeat across the two, leaking both plaintexts' XOR and
GCM's authentication key. A salt per object makes every subkey new.

## What is encrypted

*(decided: data encrypted, metadata authenticated only)*

- **Data parts:** each part of a framed object (`extent_frame::PART`, 1 MiB) is compressed
  first, as now, then sealed with AES-256-GCM under the object's subkey
  (`dek.derive(salt, ["fxdax frame parts"])`); the nonce is the part's index, the associated data
  its index, encoding, stored and unpacked lengths. The table's entry holds the tag (every FORMAT 2
  entry has one, zeros in the clear: 33 bytes a part). Compression keeps working; ciphertext is the
  same length. *(TASK-411.2)* Nothing is bound to the object's key: a rebase renumbers and restamps
  closed segments, uploading them under other keys (doc-14), and own extents' keys name their epoch
  and segment. The salt binds instead: a part moved to another object fails under that object's
  subkey. A whole object copied to another key of its branch is caught by its footer where the
  footer names its place (segments: branch, number, epoch), and by TASK-411.3 for the others.
- **Footers, tables, trailers, checkpoints, moves' maps, records, cell objects, leases:** in the
  clear, as now, plus a MAC under a key derived from the DEK (*TASK-411.3*: a checkpoint is framed
  `Seal::Authenticated`, kind 2: its parts, the table's entries, in the clear, each tagged with a
  GMAC under the object's parts subkey, its footer and table MACed, so that the collector reads it
  holding no key and a key-holding reader checks every byte), which key-holding readers check (a
  frame's footer and its table, which covers the trailer's seal and the footer's MAC, each have an
  HMAC-SHA256 under `dek.derive(salt, ["fxdax frame mac"])` in the trailer; restamping a footer
  needs the key and keeps the salt). The
  collector reads them without keys, checking CRCs, as now. They show extent IDs, sizes, the fork
  tree and live bytes; Fxfs's own metadata (names, inodes, directories) is data inside extents, so
  encrypted. A bucket writer could tamper with what the collector reads; it can delete objects
  anyway, so that is no new power.
- **The trailer** gains the salt, the branch and DEK generation, and a flag: FORMAT 2 of
  `extent_frame` (doc-11 rule 1: a FORMAT 1 reader names FORMAT 2, not corrupt). An object in the
  clear says so, its MACs and tags zeros; as its seal is not authenticated, a reader of a branch
  with a DEK refuses its objects in the clear (TASK-411.3), or a bucket writer could downgrade one.
- **Local disk:** closed segments in the WAL and the part cache hold the stored bytes, so they are
  encrypted with nothing more. *(decided)* **The open segment is encrypted too**, under the volume
  DEK, so that shredding covers hosts' disks without counting on their disk encryption. Its pieces
  are read back by offset until the segment is framed, so it needs a length-preserving, random-access
  cipher: AES-256-XTS per 4 KiB sector (the workspace has `storage_xts`), tweak the sector, key
  derived from the DEK and the segment's salt (`wal::log_salt` exists already). *(found
  2026-11-14)* Every write reaching it is whole sectors, so XTS needs no read-modify-write: with an
  assert that each length given `Segments::reserve` and `Segments::append` is a multiple of 4096,
  every test of `extent_store`, `sqlite_extents` and `fxdax` on macOS passed (574) but the 62 of
  `extent_store`'s simulation, whose writes are of 512 bytes (`sim/world.rs`, `BLOCK`); the guest's
  writes are aligned to the device's `block_size`, 4096 (doc-4). So the store refuses a write that
  is not whole sectors, and the simulation writes 4 KiB blocks (TASK-411.8). Large extents' own raw
  files (`open-extents/`) hold guest data until sealed too, and are encrypted the same way
  (TASK-411.5).
  The open segment's log holds `Contents`, metadata, so it gets a MAC, not encryption.
- **Memory:** the extent cache and DAX windows hold plaintext. DEKs and KEKs in memory are
  zeroized on drop and never written out unwrapped.

## Who holds which key

| Process | Keys | Why |
|---|---|---|
| extentd, a cell's holder | the cell's KEK; its volumes' DEKs; inherited DEKs | reads and writes the volumes |
| A host opening a point or a follower (doc-8) | the DEKs of the branch and its ancestors | reads only |
| `fxdax-gc`, marking | none | reads metadata only |
| `fxdax-gc`, compacting | the cell's KEK, through the KMS, for that pass | *(decided)* compaction copies extents out of decompressed data (`collector::compact_object`), so it decrypts and re-encrypts, as the source branch's DEK, with a new salt. Revoking a tenant's key stops its compaction, which is right. A collector that never decrypts (moving whole stored parts) was the alternative; it reclaims less, as one live extent keeps its 1 MiB part |
| the control API | whatever a fork across cells needs | rewraps (below) |

## Forks and shared points

A child reads its parents' objects, under the parents' DEKs. *(decided)* **A fork into another cell
rewraps the inherited DEKs under the child's cell KEK** and keeps them in the child's record: its
own DEK plus each inherited `(branch, generation, wrapped DEK)` from outside its cell. So a child
never needs another cell's KEK, nor another tenant's KMS, and **shredding a parent leaves its
children what they inherited**, as doc-7's rule that forks keep what they inherited (Deleting a
point) already says. Shared points (TASK-395.7) work the same way: the trusted control API, which
can unwrap both, rewraps at each fork into another tenant's cell, so the point's owner shredding its
tenant stops new forks of the point and breaks no clone.

A fork into a new cell needs a new cell KEK, which only the KMS can wrap under the tenant's KEK.
*(decided)* **Hosts keep a pool of cell KEKs per tenant, wrapped ahead of time** in batches, and a
fork takes one: no KMS call on the fork's path, which doc-7 keeps O(1) in the bucket. A tenant's pool
is discarded when its KEK is revoked or destroyed (the next refill's wrap fails, or the control API
says so), so a revocation stops new cells at once, not after a TTL; a pooled key never used is
dropped with its pool, never written out unwrapped.

## Shredding

- **A tenant:** destroy its KEK in the KMS. Absolute: every cell KEK of it, in every version of
  every cell object, unwraps no more.
- **A cell:** remove its wrapped KEK from its object (a control API call, conditional as every cell
  update). Only as strong as the bucket keeps no older version of `<C>cell`: with versioning on, a
  lifecycle rule must expire non-current versions, or the old wrapped KEK stays unwrappable while
  the tenant's KEK lives.
- **A volume:** deleting it, as now; its record stays (records stay, doc-7) and could have its
  wrapped DEKs cleared once the collector has removed the branch's objects.

## Decided later the same day

- **Unencrypted stores stay** *(decided)*: FORMAT 2's trailer says whether an object is encrypted,
  and a cell with no wrapped KEK (tenant 0, tests that ask) writes in the clear, its records with no
  DEKs. Both paths are tested: every encryption test has a plaintext twin where it applies.
- **No migration** *(decided)*: no bucket in use needs keeping; FORMAT 2 refuses FORMAT 1 by name,
  as `layout::FORMAT` 5 refused 4 (doc-11 rule 1).
- **aws-lc-rs** *(decided)* for AES-256-GCM, AES-256-XTS (if it has XTS; `storage_xts` otherwise)
  and HKDF: already in the lock file, Apache-2.0 and ISC, AES-NI and ARMv8 AES.
- **Real KMS backends later** *(decided)*: a trait and a local fake first.

## What TASK-411.3 made

- **Formats.** `layout::FORMAT` 10 (9 refused by name, no migration). A cell's object gains
  `keks: Vec<Kek { generation, wrapped }>`, its KEK's generations wrapped by the tenant's KEK in
  the KMS, oldest first; none: a cell in the clear. A record gains `deks: Vec<Dek { generation,
  kek, wrapped }>`, each generation of the branch's DEK wrapped by the cell's KEK of generation
  `kek`, with AES-KW under `HKDF(cell KEK, info = "fxdax dek wrap" || tenant || cell || branch ||
  generation)`: a wrapped DEK copied to another record, or relabelled, fails AES-KW. `Record` is no
  longer `Copy`. An own extent's footer is its `ExtentId`, a moves' footer names the key it is the
  moves of (`Moves::of`): a whole object copied to another key of its branch is refused there, its
  footer under the MAC.
- **The KMS and how a cell chooses.** `FXDAX_KMS=local:///DIR` (a `LocalKms` directory, the only
  backend; `extent_keys::kms::from_url`), read by every process that opens stores through
  `fxdax::remote::runtime()`: extentd, the fxdax CLI's FUSE mounts, fxdaxctl. `fxdax-gc` needs none
  (marking keyless; compaction of sealed objects is refused, TASK-411.6). A cell is made sealed iff
  the host has a KMS and the cell's tenant has a KEK in it; `NoSuchTenant` means the clear, any
  other refusal (revoked, destroyed, unreachable, a bad `FXDAX_KMS`) fails the making, so that no
  cell a tenant wants sealed is made in the clear. New cell KEKs come from a per-tenant pool of keys
  wrapped ahead (a wrap when it is empty; a KEK that made no cell goes back to it).
- **Taking a cell** unwraps its KEK once per taking (`keys::Kms`, keyed by cell, generation and
  the taking's epoch and holder; a writer's recovery, which reads before it holds the cell,
  predicts the taking it makes, `CellKeys::to_hold`), however many volumes open: a test counts one
  unwrap for three. A cell the host made unwraps nothing. Branch DEKs are unwrapped locally.
- **Rings.** Opening a store builds a `keys::Ring`: its branch's DEK generations (the newest seals
  new objects) and its ancestors' along their origins, read from their records and unwrapped under
  the cell's KEK. (An ancestor of another cell with DEKs was refused, `Error::Inherited`, until
  TASK-411.4 rewrapped them: below.) A branch's first writer gives its record its first DEK by a conditional
  update (`keys::with_dek`): records written by a parent's writers for a fork name none, so that
  they stay the same whoever writes them. Points have none. Every writer and reader of the store
  holds the ring: the writer (fences sealed, checkpoints authenticated, recovery checked), the
  store (segments and own extents sealed, local files read under it), and the host's fetchers,
  which open tails and parts under the union of the rings of the stores the host serves
  (`keys::HostRing`, in `fetch::Memory`), a tail opened once the host holds its key, `NoKey`
  never kept; peers are served stored bytes, needing none.
- **Downgrades.** A reader holding a branch sealed (`Keys::sealed`) refuses its objects in the
  clear (`Error::Downgrade`, naming the object) and authenticated ones under keys it lacks.
- **Generations and rotation.** `keys::add_generation` appends a DEK generation by a conditional
  update; stores opened after seal under it, the older still read. `keys::rotate` adds a cell KEK
  generation (conditional on the holder holding the cell), then rewraps every record's DEKs under
  it, each by a conditional update. Older KEK generations stay in the cell's object: a writer that
  read the cell before the rotation may wrap a new DEK under the generation it read after the
  rotation listed the records, so dropping them needs that fenced first (follow-up).
- **Local disk.** `wal::whole_footer` checks a framed segment whole by the keyless check
  (`extent_frame::check`), sealed or not; a segment a recovery closes again is framed in the clear
  (WAL recovery runs before the store's keys are known) and sealed as the store opens
  (`wal::reseal`, by `Dir::replace`, atomic).
- **Not yet:** open segments and large extents' raw files are in the clear until sealed
  (TASK-411.5); collector compaction of sealed objects (TASK-411.6).

## What TASK-411.4 made

- **Format.** A record gains `inherited: Vec<Inherited { branch, dek: Dek { generation, kek,
  wrapped } }>`: the DEK of every ancestor of another cell whose objects the branch reads, each
  generation, wrapped by the record's own cell's KEK of generation `kek`, with AES-KW under
  `HKDF(cell KEK, info = "fxdax inherited dek wrap" || holder (tenant, cell, id) || ancestor
  (tenant, cell, id) || generation)`: copied to another record, or of another ancestor, or
  relabelled, it fails AES-KW. `layout::FORMAT` 10 is extended, not bumped: it is unreleased (main
  is at 7), as FORMAT 2 of `extent_frame` was for TASK-411.2. They are given with the branch's
  first DEK, in one conditional update (`branch::give_deks`): **a record with DEKs holds all it
  inherits from other cells** (given), its readers needing no other cell's KEK.
- **Who gives them.** A fork's host, as it makes the fork (`extent_store::upload::Remote`):
  `Remote::inheriting(child)`, before anything of the fork is made (its commit, a point's clone's
  record), takes the keys the parent's store holds in memory (its ring: its own DEKs and every
  ancestor's) and a KEK for the child's cell from the host's pool (`keys::inheriting`); then
  `Remote::forked(epoch, child, inheriting)`, once the child's record is made, makes the child's
  cell with that KEK (`cell::make`: create-only, held by no placement, token 0, so that any
  placement takes it; the host's `keys::Kms` keeps the KEK for the cell's first taking here, so
  that the child's store opening here unwraps nothing), and gives the child's record its first DEK
  and the parent's keys rewrapped under the new KEK (`keys::give_inherited`). A failure there fails
  the fork, which fxdax deletes (`Remote::unfork`). Nothing is given for a fork within a cell (the
  child's writer gives its own DEK as before, and with it what the parent's record inherits), for
  a point, nor for a fork of branches in the clear: plaintext forks make no request more, no KMS
  call. A child its fork's host did not give keys to (its host died between the record and the
  give, or a fork recorded by the parent's next writer or deletion) is given them by its first
  writer (`keys::with_dek`), reading across, below.
- **Points keep no key**, nothing writing them: their copies read what they name by crossing into
  their origins' cells (one unwrap of the origin's cell KEK a taking, cached), and forks of them,
  restores and shared points' clones, rewrap from the copy's ring like any other fork: so the
  point's owner shredding its tenant stops new forks of it (no copy opens) and breaks no clone. A
  host holding a copy open, told the tenant's KEK is revoked (`Kms::revoked`), refuses forks of it
  before anything is made.
- **Rings** (`keys::ring`): a given record is read with its own cell's KEK alone: its DEKs, its
  inherited ones, and its same-cell ancestors' records; a sealed ancestor of another cell it holds
  not is refused (`Error::Inherited`). A record without its keys (a point, a fork's child before
  they are given, a branch of a cell in the clear) crosses into each sealed ancestor's cell, its
  object read and its KEK unwrapped, taking its DEKs and inherited ones; but a writer of a cell in
  the clear inheriting anything sealed is refused, and so is a fork of a sealed store into a cell
  in the clear, before anything is made (`ENOTSUP`): no sealed data goes into the clear.
- **KMS calls on the fork's path: none**, its cell KEK from the pool, the parent's keys in memory.
  The child's first opening on the forking host: none. On another host: one unwrap of the child's
  cell KEK, as for any cell taken.
- **Pools** (`keys::Pooling`; fxdax: 8 a tenant at least, 16 a refill, on the uploaders' runtime):
  a take leaving a pool low, or a sealed store of the tenant opening, refills it in a batch of
  wraps at once, off the fork's path (`Kms::refill`, `Kms::filled`); an empty pool wraps one on the
  path. `Kms::revoked(tenant)`, called on every revoked or destroyed answer of the KMS (a refill's,
  an unwrap's) and by the control API (TASK-411.7's to wire), drops the tenant's pool, a refill
  going adding nothing, and its cached cell KEKs, and marks it refused: no refill, no fork
  inheriting its keys, until a KMS call for it succeeds again. KEKs a pool wrapped before a
  destruction nothing has heard of yet still make cells, unreadable from the first, until a refill
  or another call hears it.
- **Checkpoints at fork points** (`recovery::checkpoint_at`, which `branch::record_fork` writes
  for a child of another cell) are authenticated under the parent's DEK, if it is sealed, as its
  writers' are: a child holding the parent's keys refuses one in the clear. Its writer's ring seals
  them (`branch::record_fork_under`, `keys::Parents::Held`); a deletion recording a sealed
  branch's forks, the host's KMS (`branch::delete_under`, `extent_store::upload::delete_branch`):
  with none, it fails rather than write one the child refuses (so `fxdax-gc --delete-branch`,
  which has no KMS, deletes no sealed branch with an unrecorded fork into another cell); a failed
  fork's claim, the host's KMS (`branch::delete_fork_under`). Asked for only when one is to be
  written.
- **Rotation** rewraps a record's inherited DEKs with its own; `add_generation` keeps them.
- **Generations a child reads.** A child keeps the generations its parent's lineage had when it
  was given its keys; what it reads of its parent was sealed under them (segments up to the fork
  point, the checkpoint there, written before the child's record). TASK-411.6's compaction must
  keep an object's generation when it rewrites one a child inherits, or the child loses it.
- **Crashes in the middle of a fork** (doc-7's protocol unchanged: the record create-only before
  the keys, forks of points on many hosts each giving its own child, failed forks deleted): before
  the record, nothing is given and the pooled KEK goes back (`Inheriting` gives it back when
  dropped unused); the record made, the keys not given: the directory left unmade is deleted,
  deleting the child, and any opener of the child gives them itself, crossing once; the cell made
  and the keys not given: a keyed cell of no volume stays, as cells' objects do, and the child's
  opener uses its KEK; keys given: as any fork. A give racing a deletion or another give is
  conditional: `Deleted` fails the fork, another's keys stand.
- **specs/bucket** is unchanged: it models records by their kinds, origins and deletion, the
  objects the collector keeps, and who writes what; keys change none of those (a record's keys are
  a conditional update that keeps its kind, origin and deletion, read by no pass), the child's
  cell object is no object a pass keeps or deletes, the checkpoint at the fork point is written
  where and when it was, and a deletion refused for want of a KMS is a deletion stopping partway,
  which the model has.
- **Fork latency** (`cargo run --release -p fxdax-examples --example fork-bench`, on a Mac, a
  memory bucket, 2026-11-14): a fork of a store of 64 extents into a new cell, median, sealed 32-34
  ms, in the clear 31-35 ms, with a KMS or none: the same within noise; 3 bucket requests more a
  sealed fork (the cell's create, the record's read beside it, its update), 47.5 against 44.5. With
  10 ms a request (`FXDAX_REMOTE_LATENCY_MS`), 225-242 ms against 200-218 ms: the two round trips
  more. No KMS call. Linux numbers: the user's, later.
