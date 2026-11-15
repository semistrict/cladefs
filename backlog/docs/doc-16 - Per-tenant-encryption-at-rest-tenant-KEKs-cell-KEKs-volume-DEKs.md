---
id: doc-16
title: 'Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs'
type: specification
created_date: '2026-11-14 18:32'
updated_date: '2026-11-15 04:05'
---
# Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs

Design, 2026-11-14. The user's decisions of that day are marked *(decided)*. TASK-411.1 (keys, KMS), TASK-411.2 (frame FORMAT 2), TASK-411.3 (cell KEKs, DEKs, rings, below: **What TASK-411.3 made**), TASK-411.4 (forks into other cells, pools; below: **What TASK-411.4 made**), TASK-411.5 (open segments and large extents' raw files encrypted, logs authenticated, below: **What TASK-411.5 made**), TASK-411.6 (the collector's compaction, below: **What TASK-411.6 made**), TASK-411.7 (shredding through the control API, below: **What TASK-411.7 made**), TASK-411.9 (recovery frames nothing before keys), TASK-411.10 (a tenant's missing KEK believed for a minute), TASK-411.13 (locked cells skipped while their KEK stays refused) and TASK-411.14 (`fxdax-gc --delete-branch` under `FXDAX_KMS`) are implemented.

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
object); rotating a DEK starts a generation, which seals what writers write from then on. *(found,
TASK-411.6)* Old data does not move to it: checkpoints rewrite metadata alone, and compaction
seals each object's moves under the generation the object was sealed under, as a store opened
before the generation was added holds none of it. Moving old data on needs readers that learn
newer generations (TASK-411.12).

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
  derived from the DEK and a salt of the file's own, kept in its log's header (TASK-411.5, below). *(found
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
| `fxdax-gc --delete-branch` | the branch's cell's KEK, through the KMS, only to record a fork into another cell whose child has no record yet | its checkpoint at the fork point is authenticated under the branch's DEK, which the child requires (TASK-411.4, TASK-411.14) |
| `fxdax-gc`, compacting | the cell's KEK, through the KMS, for that pass | *(decided)* compaction copies extents out of decompressed data (`collector::compact_object`), so it decrypts and re-encrypts, under the source branch's DEK, the generation the object was sealed under, with a new salt (TASK-411.6). Revoking a tenant's key stops its compaction, which is right; its garbage still goes. A collector that never decrypts (moving whole stored parts) was the alternative; it reclaims less, as one live extent keeps its 1 MiB part |
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

- **A tenant:** destroy its KEK in the KMS, then tell every host (`ForgetTenantKeys`, TASK-411.7).
  Absolute: every cell KEK of it, in every version of every cell object, unwraps no more.
- **A cell:** remove its wrapped KEK from its object (`ShredCell`, conditional as every cell
  update, TASK-411.7). Only as strong as the bucket keeps no older version of `<C>cell`: with
  versioning on, a lifecycle rule must expire non-current versions (S3
  `NoncurrentVersionExpiration`; GCS versioning off or a lifecycle delete of non-current
  versions), or the old wrapped KEK stays unwrappable while the tenant's KEK lives. With
  versioning off, nothing more is needed. The README says it for operators, doc-7 for the bucket.
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
  `fxdax::remote::runtime()`: extentd, the fxdax CLI's FUSE mounts, fxdaxctl; and `fxdax-gc`, to
  compact sealed cells (TASK-411.6; marking keyless). A cell is made sealed iff
  the host has a KMS and the cell's tenant has a KEK in it; `NoSuchTenant` means the clear, any
  other refusal (revoked, destroyed, unreachable, a bad `FXDAX_KMS`) fails the making, so that no
  cell a tenant wants sealed is made in the clear. New cell KEKs come from a per-tenant pool of keys
  wrapped ahead (a wrap when it is empty; a KEK that made no cell goes back to it).
  *(TASK-411.10)* A host believes a `NoSuchTenant` for `keys::KEKLESS_FOR`, 60 s, from the wrap
  that heard it (`keys::Kms::new_kek`, or a pool's refill), asking the KMS nothing for that tenant
  meanwhile: every opening of a store whose record has no DEK asks for a KEK in case it makes the
  cell, so a host opening a plaintext tenant's stores made one wrap call per opening; now one a
  minute at most (openings asking at once, before the first answer, ask each: a wrapped KEK is one
  cell's alone). The cost: a tenant given a KEK has new cells made sealed within 60 s, not at once;
  cells made in the clear before stay so (a cell's sealing is chosen as it is made). Give a tenant
  its KEK a minute before making the cells it wants sealed. Any answer of the KMS for the tenant
  forgets it had none. `extent_remote` `keys::tests::tenants_without_a_kek_are_asked_for_one_once_a_while`
  (paused clock) and `extent_store` `upload::encrypted::cells_of_tenants_with_no_kek_are_in_the_clear`
  count the calls.
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
  (`extent_frame::check`), sealed or not. *(TASK-411.9)* WAL recovery runs before the store's keys
  are known and frames nothing: a segment it closes again keeps its raw file and log, its footer
  folded and stamped provisionally as before (`Recovered::closing`), and is framed as the store
  opens, holding its ring, first of all (`Recovered::close`, from `Recovering::open`): sealed if the
  ring seals, in the clear otherwise, the raw file and log removed once the framed file and its
  entry are durable. A crash before leaves them, which the next recovery closes again alike;
  `drop_wal` removes them durably. (TASK-411.3 framed it in the clear and resealed it as the store
  opened, its data on the host's disk in the clear meanwhile, and for good if the host died
  between; `wal::reseal` and `Dir::replace` went.)

## What TASK-411.6 made

- **Marking stays keyless.** A pass reads records, checkpoints (authenticated, their parts in the
  clear), segments' footers, moves' maps and trailers holding no key, as before; each candidate's
  trailer, which it reads anyway for its data's length, says whether it is sealed.
- **Compacting under the cell's keys.** A pass whose compactable objects include a sealed one
  reads the cell's object and unwraps its KEK through the collector's KMS
  (`collector::Collector::kms`, `sweep::Sweep::kms`), by a `keys::Kms` made for that pass alone:
  one unwrap per KEK generation its branches' DEKs name, one in practice, however many objects it
  compacts, and nothing kept past the pass, so a tenant's KEK revoked stops the next. The DEKs come
  from the pass's records of those branches (`keys::compacting`). A pass compacting only objects in
  the clear, or nothing (no candidate below its fraction, `--compact-below 0`), and every dry run
  ask the KMS nothing: `collector`'s tests count the calls with `extent_keys::testing::Counted`.
- **Moves sealed as their objects were.** `compact_object` reads the object under the ring's view of
  its branch (an object in the clear of a branch with DEKs is refused, a downgrade) and seals its
  moves under the very key its trailer names, branch and generation, with a salt of their own;
  their footer, `Moves::of` under its MAC, names the object they are the moves of, so moves copied
  whole to another moves key of the branch are refused, by readers holding the keys or none
  (TASK-411.3's check, tested now). *Why the source's generation, not the record's newest:* every
  reader that may follow the moves read the object under that key; a store's ring is built as it
  opens, and one opened before a generation was added holds none of it (TASK-411.12). Moves of
  objects in the clear are in the clear, as before.
- **KEKs not to be had.** The KMS refusing the cell's KEK (the tenant's KEK revoked or destroyed,
  the KMS failing), or the collector having no KMS: the pass still marks, compacts what is in the
  clear, and deletes what no live branch needs, leaving the sealed uncompacted; it counts them in
  its report (`Report::locked`, the passes, `Report::uncompacted`, the objects), warns naming the
  cell and the KMS's refusal. *(TASK-411.13)* It records its listing's digest and how many it left
  (`passed::Passed::uncompacted`); a pass after it, the cell unchanged, skips the cell (its lease
  and one listing) while the tenant's KEK is still not to be had, which a sweep asks the KMS once a
  tenant for all its cells (`collector::Heard`, a wrap of a key of no use; none without a KMS), and
  goes over it in full, compacting, once it is, or once the cell changes. (TASK-411.6 recorded no
  digest: every pass over a revoked tenant's idle cells went in full.)
  `fxdax-gc` prints them on the pass's line and the sweep's (`left N sealed uncompacted, without
  their cell's KEK`) and in `--metrics` (`fxdax_gc_sweep_objects{action="uncompacted"}`,
  `fxdax_gc_sweep_locked_cells`).
- **`fxdax-gc` reads `FXDAX_KMS`** as every fxdax process does (`extent_keys::kms::from_url`); one it
  names that can't be had stops it before it collects (a CLI fails at once rather than leave every
  sealed cell uncompacted); `--dry-run` reads none.


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
  with none, it fails rather than write one the child refuses; a failed fork's claim, the host's
  KMS (`branch::delete_fork_under`). Asked for only when one is to be written. *(TASK-411.14)*
  `fxdax-gc --delete-branch` deletes under `FXDAX_KMS`'s KMS, as its passes compact: a sealed
  branch with an unrecorded fork into another cell (its writer died between the fork's
  acknowledgement and recording it) is deleted given one, the checkpoint at the fork point
  authenticated under the branch's DEK. Without one the deletion fails naming the tenant and
  `FXDAX_KMS`, its lease ended and its record not yet deleted: a deletion stopped partway, which
  running it again with the KMS finishes (`extent_remote` `tests/gc.rs`
  `deletes_sealed_branches_recording_their_forks_into_other_cells_under_the_kms`).
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

## What TASK-411.5 made

- **The cipher.** `extent_keys::xts::Xts`: AES-256-XTS (IEEE 1619-2007, no ciphertext stealing),
  Fuchsia's `storage_xts` over RustCrypto's `aes` (key schedules zeroed on drop), as aws-lc-rs has
  none. Checked against IEEE 1619's vectors 10 and 11 (as aws-lc's `xts_test.cc` carries them) and
  against aws-lc's own `EVP_aes_256_xts` on random keys and 4 KiB sectors (a dev-dependency on
  `aws-lc-sys`, at the version aws-lc-rs builds).
- **Raw files.** `extent_store::RawFile`: a sealed store's (its ring seals, `Ring::sealing`) open
  segments' and large extents' raw files are encrypted a sector at a time (`SECTOR`, 4096), the
  sector's place in the file its tweak (the data unit number), under
  `Xts::derive(dek, salt, "fxdax raw file")`: two HKDF subkeys (`[info, "data"]`, `[info, "tweak"]`)
  of the branch's newest DEK and a salt drawn for the file. Writes are whole sectors (TASK-411.8;
  a sealed store refuses writes to large extents that are not, `Unsectored`, as it always refused
  them to segments), encrypted through a buffer; reads decrypt the sectors they are in. A large
  extent's raw file keeps its salt in memory alone: an opening removes those left. A clear store's
  raw files are as they were, byte for byte.
- **Logs.** A sealed store's segment's log is headed `FXLOG\0\0\x03` (`wal::SEALED_LOG_MAGIC`):
  after the clear header's fields (record salt, branch, number), the DEK's branch (u64) and
  generation (u32) and the raw file's 32-byte salt, then an HMAC-SHA256 of the header so far under
  `dek.derive(salt, ["fxdax raw file log mac"])`; each record's payload (bincode of `LogRecord`) is
  followed by an HMAC of its offset in the log (u64) and the payload, inside the salted hash's
  frame. The runs a record holds or checksums (`Runs::Held`, `Runs::Checked`) are the raw file's
  stored bytes, encrypted: recovery, holding no key, works on stored bytes as before (salted
  hashes, CRCs). A clear store's logs are as they were (`FXLOG\0\0\x02`).
- **Recovery.** The segments recovery closes again are framed at `Recovering::open`
  (`Recovered::close`, TASK-411.9): the log's header MAC and every record's MAC checked under the
  DEK and salt it names (a log changed, or under a DEK the ring lacks, refuses the opening, naming
  the segment), the raw data decrypted, then sealed into FORMAT 2 parts.
- **Framing.** Closing a segment reads its raw file through `RawFile` (decrypting) and seals it, as
  TASK-411.3 did. **Sealed stores never frame in place** (segments nor large extents): a sealed
  object's bytes are AES-GCM parts and tags, not the XTS sectors the raw file holds, so framing in
  place would write every byte again anyway, and a crash part way would leave a file neither raw
  nor framed. `RawFile::clear` hands the inner file to in-place framing only in the clear.
  Nor does a look ahead (TASK-400, merged from main) see a sealed segment's parts as it fills:
  its bytes in the raw file are XTS ciphertext, and a framing in a copy reads every part anyway,
  so `Segment::new` gives a sealed open segment no `Ahead`, nothing is queued for the framer
  (`Job::See`), and its framing reads all its parts through `RawFile`, decrypting
  (`sealed_segments_parts_are_not_seen_ahead`). A clear store's looks ahead are main's: through
  `RawFile`, the clear file's bytes as they are, their CRCs and looks those its in-place framing
  writes. TASK-401's writing back holds for sealed framed files as for clear copies: the framing
  writes its copy to the page cache, `make_durable` writes it back a MiB at a time before it
  syncs it, and the uploader reads it through the cache (`read_object`).
  Nor are a sealed segment's parts packed ahead and held in memory (TASK-400's held parts,
  c2daf7630, merged from main): holding rides on the look ahead, which a sealed segment has none
  of, so nothing of it is queued, packed or held, and its framing reads every part back through
  `RawFile`, decrypting, then packs and seals it, as before. Chosen over packing and sealing ahead
  because that would need the framed object's key (`Packer::new(sealing)` draws its salt and
  derives its `ObjectKey` at framing) drawn when the segment opens, kept with it across the
  framer's queue, the threads packing ahead, eviction and a framing that fails and runs again,
  each part's nonce (its index) under it never reused for other bytes; and the parts held would be
  ciphertext the governor evicts and a framing re-reads anyway. Not holding adds no key's
  lifetime, no plaintext anywhere but the framing's own buffers, and keeps the bytes framed what
  they were. The cost: a sealed store's framings in a copy read their raw file back as clear
  stores' did before TASK-400, from disk once the host let its pages go (TASK-421).
  `Store`'s look ahead asserts it is of a clear raw file; `Packer::push_stored` asserts a part is
  sealed as its object is, so a clear held part could never reach a sealed object. Tests:
  `sealed_segments_parts_are_neither_seen_nor_held_ahead`,
  `sealed_stores_hold_no_parts_packed_ahead` (a sealed store holds nothing, reads all its parts
  back, and leaves no plaintext in any file; its clear twin holds its parts and reads its last
  alone).
- **No writer of raw files but the store.** `Write::file()` went: `Write::write` encrypts, and
  `Write::stored` gives a caller doing its own I/O (fxdax's image device) the file, the offset and
  the bytes encrypted, which it writes again from the last whole sector written after a short
  write. `Part::Data` carries the `RawFile`: such a reader reads `RawFile::read_range` and
  decrypts it (`RawFile::decrypt`).
- **Cost, macOS (Apple M5 Pro, 15 CPUs, `store-bench`, release, 2026-11-14).** Writes into open segments, 1 MiB extents:
  1.8-1.9 GB/s sealed against 2.8-3.6 GB/s in the clear; reads of them while open 1.6-1.7 against
  2.3-2.7 GB/s; 4 KiB extents 195 against 209 MiB/s. Framing 32 MiB segments, four stores at once:
  text 66-76 ms each in the clear, 70-72 ms sealed; noise 39-41 ms in the clear (in place), 83-85 ms
  sealed (in a copy). Linux to be measured.

## What TASK-411.7 made

- **A cell shredded** (`extent_remote::cell::shred`): one conditional update of `<C>cell`, by
  whoever calls it, not its holder, takes every KEK generation out and sets `shredded`
  (`cell::Object::shredded`, layout FORMAT 10 extended, unreleased). Takings and holdings of it are
  refused (`cell::shredded`: `Error::Kms`, "tenant T: cell T/C is shredded, its KEK gone", so
  `EACCES` to callers and the collector's locked path, marking and deleting on, its sealed objects
  left uncompacted, not a corrupt pass), and so is `add_kek`. `CellKeys` of a shredded cell are
  sealed and refuse their KEKs naming the cell: no branch of it is written in the clear, no DEK is
  given. Its holder, if any, goes on changing its volumes until told: every change keeps the mark.
- **A host told** (`fxdax::remote::forget(Scope, why)`, `keys::Scope::{Tenant, Cell}`), by the
  control API (`ShredCell`, which shreds then forgets, idempotent, to be called on every host;
  `ForgetTenantKeys`, once the operator destroyed or revoked the tenant's KEK in the KMS) or
  fxdaxctl (`shred-cell`, `forget-tenant-keys`):
  - fences every store of the scope open in the process (`fence::fence_stores`, fences listed by
    branch): each request fails with `EACCES` from then on, as its reopening does; extentd refuses
    its VM process's requests so;
  - takes the scope's stores out of the extent cache's holders (`cache::forget`): an extent no
    other open store holds goes, its pages evicted, unmapped from every window, and no page-in
    reads it again;
  - drops what its fetchers kept of the scope's objects (`fetch::Memory::forget_kept`: tails opened
    under keys, parts unpacked in the clear) and the rings of the scope's stores
    (`HostRing::forget`, by `Ring`'s owner, the branch whose store opened it);
  - has its KMS forget the scope (`keys::Kms::forget`): a tenant's unwrapped cell KEKs, pool and
    forks inheriting its keys (`Kms::revoked`, now wired), until the KMS answers for it again; a
    cell's unwrapped KEKs, refused from then on on this host even under a cell object read before.
- **What stays.** In memory: a fenced store's own ring and its open segment's XTS key, until
  whoever holds it closes it (a VM process whose guest met `EACCES`, a point kept for forks); the
  DEKs other stores hold, which they inherited (a fork of the shredded tenant into another's keeps
  its keys: shredding a parent leaves its children what they inherited, and their pages); pages of
  extents other open stores hold too (a shared point's clone, a public base, by design one copy for
  all). A host not told keeps reading its open stores under what it unwrapped, but opens none afresh:
  a fresh opening reads the cell object (shredded) or asks the KMS (destroyed). On disk: everything
  of the scope's is as its keys sealed it, open segments (XTS), logs (MACed, metadata), the part
  cache (GCM), and a shredded cell's objects stay in the bucket, ciphertext, until its branches are
  deleted. A race: a store opening on a host as it is told may finish opening; telling it again
  fences it.
- **Tests.** `extent_store` `upload::encrypted::shredded_tenants_leave_none_of_their_volumes_readable_on_any_host`
  (two hosts, open segment, part cache on disk, a cross-tenant fork reading on) and
  `shredded_cells_leave_their_volumes_unreadable_and_the_tenants_others_readable` (the collector
  locked, the old cell object put back reading again: why versions must expire); `extent_remote`
  `keys::tests::shredded_cells_refuse_their_keys_and_hosts_told_forget_them`; `fxdax`
  `remote::tests::stores_of_shredded_cells_and_tenants_are_fenced_and_leave_the_cache`; Linux:
  `tests/control.rs` `sealed_stores_served_and_shredded_through_the_control_api`.

## Real KMSes: AWS KMS

*(TASK-411.17)* `FXDAX_KMS=aws-kms://REGION?key=TEMPLATE[&keys=FILE][&endpoint=URL]`
(`AwsKms`, `extent_keys/src/kms/aws.rs`), the query percent-encoded as any URL's; any other query name is
refused, so that a misspelt `endpoint` does not send calls to AWS itself.

- **Naming.** Each tenant's KEK is a symmetric KMS key (`SYMMETRIC_DEFAULT`, `ENCRYPT_DECRYPT`)
  named as `kms::KeyNames` says: the template `key` with `{tenant}` in hexadecimal, say
  `alias/fxdax-tenant-{tenant}`, an alias per tenant the operator makes (`aws kms create-key`,
  `aws kms create-alias`); and the overrides file `keys`, lines `TENANT_HEX NAME`, for tenants that
  bring their own key (BYOK). A name is any form `Encrypt` and `Decrypt` take: a key ID, a key
  ARN, an alias name or an alias ARN; a key in another account needs its key ARN (or alias ARN).
- **Endpoint.** AWS's own for the region, unless `endpoint` names another: a VPC interface
  endpoint, or an emulator. Without `endpoint`, the SDK's own `AWS_ENDPOINT_URL_KMS` and
  `AWS_ENDPOINT_URL` apply, as for any AWS SDK.
- **Credentials.** The standard AWS chain, as `aws-config` (AWS's own SDK, Apache-2.0) finds them:
  the environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`), the shared
  profile (`AWS_PROFILE`, `~/.aws/config` and `credentials`, SSO, `credential_process`), web identity
  (EKS's IRSA), the container's (ECS) and the instance's role (IMDSv2), refreshed as they expire.
  Nothing of them, nor any key, is logged: an I/O error says the call, the key's name and AWS's
  error code and message, or the SDK's error and its causes' Display, never its Debug.
- **Wrap.** KMS `Encrypt` of the key's 32 bytes under the tenant's key, with the encryption context
  `{"fxdax-tenant": TENANT_HEX}`: the ciphertext decrypts for that tenant alone even where two
  tenants share a key, and a key policy can let fxdax use a key for one tenant only. The wrapped
  key is a header, `fxka`, format 1, the key name's length (2 bytes, big-endian) and the name, then
  KMS's ciphertext blob. `LocalKms`'s begin `fxkw`.
- **Unwrap.** The header is checked first, asking the KMS nothing: another backend's (`fxkw`, or
  any not `fxka`), another format, or a key name other than the tenant's (another tenant's key, or
  the tenant's under a name since changed, TASK-411.20) is `Unauthentic`. Then KMS `Decrypt` with
  `KeyId` the key's name, so that a ciphertext of another key, even with the tenant's context
  (whose key's owner could make one), is refused, and with the same context; a plaintext not of
  32 bytes is `Unauthentic`. The blob names the key version that encrypted it, so a rotated key
  (automatic rotation, or `RotateKeyOnDemand`) unwraps what every version of it wrapped, and
  fxdax does nothing on a rotation.
- **Refusals**, each naming the tenant (`kms::Error`):

  | AWS KMS's error | `kms::Error` |
  |---|---|
  | `NotFoundException` | `NoSuchTenant` |
  | `AccessDeniedException` (the grant or policy taken away), `DisabledException` | `Revoked` |
  | `KMSInvalidStateException`, the key pending deletion | `Destroyed` |
  | `KMSInvalidStateException`, any other state (pending import: imported key material deleted; unavailable: a custom key store disconnected; creating, updating) | `Revoked` |
  | `InvalidCiphertextException`, `IncorrectKeyException` | `Unauthentic` |
  | anything else: throttling, `KeyUnavailableException`, `KMSInternalException`, `DependencyTimeoutException`, `InvalidKeyUsageException`, 5xx, the network, a timeout, no credentials | `Io` |

  AWS says which invalid state only in its message ("`<key ARN>` is pending deletion."), which the
  mapping reads. A tenant shreds itself by scheduling its key's deletion: `Destroyed` at once, for
  the waiting period's 7 to 30 days; cancelling it leaves the key disabled (`Revoked`) until
  enabled. Once the period is over AWS has forgotten the key and answers `NotFoundException`, as
  for a key never made, so the tenant reads as having no KEK: its unwraps still fail, but a host
  would make its new cells in the clear (TASK-411.19); until that is fixed, forget the tenant's keys
  on every host (`ForgetTenantKeys`) and stop serving it before the waiting period ends. A tenant
  with imported key material that deletes it reads as `Revoked` (it may import the same material
  again), never `Destroyed`.
- **IAM.** fxdax's role needs `kms:Encrypt` and `kms:Decrypt` on each tenant's key, no more (no
  `kms:DescribeKey`, no `kms:GenerateDataKey`); a condition on the encryption context ties each key
  to its tenant:

  ```json
  {
    "Effect": "Allow",
    "Action": ["kms:Encrypt", "kms:Decrypt"],
    "Resource": "arn:aws:kms:REGION:ACCOUNT:key/KEY-ID",
    "Condition": {"StringEquals": {"kms:EncryptionContext:fxdax-tenant": "2a"}}
  }
  ```

  Cross-account BYOK: the tenant's key policy allows fxdax's role (its ARN as principal) the same
  two actions with the same condition, fxdax's role's own IAM policy allows them on the key's ARN
  (cross-account use needs both), and the overrides file names the key by its ARN. The tenant
  revokes by disabling the key or removing that statement (`Revoked`), and shreds by scheduling the
  key's deletion (`Destroyed`).
- **Calls.** On a runtime of the backend's own (one thread, `fxdax-aws-kms`), its HTTPS client
  (hyper 1 over rustls 0.23, the build's, not the SDK's legacy default) made there when first
  needed: its futures run on any executor, and its connections never outlive a caller's runtime.
  The SDK retries what AWS says is retryable (throttling, 5xx, a lost connection), three attempts
  backing off; an attempt times out after 10 s, a call after 30 s, so a KMS that answers not fails a
  store's opening as `Io` rather than holding it.
- **Memory.** The key fxdax hands `Encrypt`, and `Decrypt`'s answer, are zeroed where fxdax holds
  them; the SDK's own copies (the request's JSON body, the response's buffer, its `Blob`) are not,
  which it offers no way to do. Its models mark both fields sensitive, so its `Debug` redacts
  them.
- **Tests.** Against local-kms (github.com/nsmithuk/local-kms, MIT), AWS KMS's emulator, pinned to
  its commit 2a5e92d09ab6 (2026-02-28), which `scripts/test-local-kms.sh` installs with
  `go install` into `target/local-kms` and runs the tests named `local_kms` with (ignored in the
  workspace's run, as the RustFS and Azurite ones), its dummy credentials in the environment for
  the standard chain to find; each test starts an emulator of its own on a free port
  (`extent_keys::testing::LocalAwsKms`, the `testing` feature). CI's object-stores job runs it:
  no AWS account, no secret. `extent_keys` `kms::aws::tests`: wrap and unwrap through
  `kms::from_url`, keys named by key ID, key ARN, alias name and alias ARN, overrides, a key shared
  by two tenants refusing each other's (context), a ciphertext of another key under the tenant's
  context refused (`KeyId`), tampering, a rotated key unwrapping its old version's wraps, disabled
  (`Revoked`) and enabled again, pending deletion (`Destroyed`), no key (`NoSuchTenant`), an
  endpoint answering nothing (`Io`, after retries); and, needing no emulator, URLs, the header,
  another backend's and another tenant's keys refused unasked, and the table on the SDK's errors.
  `extent_store` `upload::encrypted::local_kms_sealed_stores_open_through_aws_kms`: a sealed
  store made, written and opened by another host through `aws-kms://...&endpoint=...`, refused
  revoked and destroyed.
- **Where the emulator differs from AWS** (behaviour verified against it, not against AWS, as
  no AWS account was used): it has no IAM, so `AccessDeniedException` is never answered but for a
  ciphertext naming no key it has, and is tested on the SDK's error alone, as throttling is; it
  rotates no key on demand, nor before a year, so the rotation test seeds two emulators with the
  same key ID, the second with a second backing key, as its seeding has rotations; it answers a
  ciphertext of another key with `InvalidCiphertextException` where AWS answers
  `IncorrectKeyException` (both `Unauthentic`); it deletes no key, pending deletion for ever, so
  `NotFoundException` after deletion is untested; its messages are its own, the pending deletion
  one as AWS's.

## Real KMSes: OpenBao

TASK-411.16: OpenBao's Transit engine (openbao.org, MPL-2.0; Vault is BSL, excluded) as a KMS,
`extent_keys::kms::openbao::OpenBao`. Self-hosted, so the one real KMS tested anywhere: its tests
start `bao server -dev` themselves.

- **Configuring.** `FXDAX_KMS=openbao://HOST[:PORT]/MOUNT?key=TEMPLATE[&keys=FILE][&namespace=NS]`
  (port 8200 by default; `MOUNT` the Transit engine's path, which may hold `/`; `NS` its namespace,
  sent as `X-Vault-Namespace`), or `openbaos://` over HTTPS, which may add `&ca=FILE`, a PEM file of
  the only CAs trusted (else the system's). `openbao://` sends the token in the clear: loopback or a
  trusted network only. Each tenant's key is named by `kms::KeyNames`: `TEMPLATE` with `{tenant}` in
  hexadecimal, `FILE`'s overrides for tenants bringing their own; names of letters, digits, `-`, `_`
  and `.`. The token is `FXDAX_KMS_OPENBAO_TOKEN`'s, or that of the file `FXDAX_KMS_OPENBAO_TOKEN_FILE`
  names (exactly one set), read at start and again whenever OpenBao answers 403, the call made once
  more if it changed: OpenBao Agent's file sink renewing it works. Credentials in the URL are refused.
- **The token's policy** needs, for each tenant key `NAME`: `update` on `MOUNT/encrypt/NAME` and
  `MOUNT/decrypt/NAME`, `read` on `MOUNT/keys/NAME`, and `auth/token/lookup-self` (the `default`
  policy has it). Not `create` on `MOUNT/encrypt/NAME`: Transit's encrypt makes a missing key when
  the token may create it (its upsert), so such a token gives every tenant wrapping a key, every cell
  sealed. Without `read`, a tenant with no key is refused as revoked: its cells are made neither
  sealed nor in the clear. The keys: `aes256-gcm96` (Transit's default) or `chacha20-poly1305`, for
  associated data; `derived` keys work too.
- **Wrapped keys.** A header, `fxkm`, format 1, the backend's name (`openbao`, its length a byte
  before it), the tenant (u64, big-endian), the key's name (its length two bytes, big-endian, before
  it); then Transit's ciphertext, `vault:vN:...`. An unwrap refuses, as `Unauthentic`, OpenBao not
  asked, a header of another magic (a `LocalKms` wrap's `fxkw`), format, backend, tenant, or a key
  name other than the tenant's now: a tenant's key renamed (overrides changed) unwraps none of what
  the old name wrapped. The header is the encryption's `associated_data` (and `context`, for
  derived keys): Transit's AEAD binds it, so two tenants sharing one key (overrides may name one key
  twice) unwrap only their own wraps (a mutant sending no header was caught). A wrap is the 32-byte
  key, base64, under the key's newest version; any version Transit keeps unwraps.
- **What OpenBao says, as `kms::Error`** (OpenBao 2.7.2, each checked against `bao server -dev`):

  | OpenBao | wrap | unwrap |
  |---|---|---|
  | 200 | wrapped | the key |
  | 403, then `GET MOUNT/keys/NAME` 404 with no errors (key missing) | `NoSuchTenant` | `Destroyed` (the header says it wrapped) |
  | 400 `encryption key not found` (a deleted key's decrypt) | | `Destroyed` |
  | 403, the key readable; or 403 everywhere, `lookup-self` 200 | `Revoked` | `Revoked` |
  | 400 `refusing to use soft-deleted key` (soft-delete, restorable) | `Revoked` | `Revoked` |
  | 400 `disallowed by policy (too old)`, version ≥ `min_available_version` (`min_decryption_version` raised, reversible) | | `Revoked` |
  | the same, version < `min_available_version` (trimmed, gone) | | `Destroyed` |
  | 400 `message authentication failed`, `invalid ciphertext` | | `Unauthentic` |
  | 400 `version is too new` | | `Unauthentic` |
  | 403 and `lookup-self` 403 (fxdax's token invalid, expired, revoked) | `Io` | `Io` |
  | 404 route not found (no such mount, a token with rights to it), other 4xx, 412/429/5xx (sealed: 503) after 3 tries, network | `Io` | `Io` |

  Why the probing: OpenBao answers 403 both for a token without rights to a key and for an encrypt
  of a missing key with a token that may not create one; the key's metadata says which, and
  `lookup-self` tells a tenant's revocation from fxdax's own token failing, so that an expired host
  token is not read as every tenant revoked (which would drop their keys from every host's memory).
  Nothing is `NoSuchTenant` but a key OpenBao says it has not, as that one makes cells in the clear.
  A mount the token has no rights to, there or not, reads as revoked (OpenBao checks the token
  first). A deleted key is as missing as one never made: unwraps say `Destroyed`, but wraps say
  `NoSuchTenant`, so a tenant shredded by deleting its key has new cells made in the clear unless
  its key is made again or the hosts refuse it (`ForgetTenantKeys`); soft-deleting it first, or
  trimming its versions (which leaves the key, wrapping anew), keeps them refused.
- **Calls.** reqwest (rustls over aws-lc-rs; the system's roots by rustls-platform-verifier), on a
  Tokio runtime of its own (one thread), so its futures run under any executor; timeouts 5 s to
  connect, 15 s a request; transport errors, 412, 429 and 5xx retried, 3 tries, 100 then 400 ms
  apart; dropping a call's future aborts it. No redirects followed. Nothing is logged; no error says
  the token (tested); the key's base64 in the request body and OpenBao's replies are zeroed when
  dropped, as far as reqwest lets them (its own buffers are not). New dependencies of `extent_keys`,
  each already in the build through `object_store`/`extent_remote`: reqwest (MIT OR Apache-2.0),
  tokio (MIT), serde_json (MIT OR Apache-2.0), base64 (MIT OR Apache-2.0), bytes (MIT), url (MIT OR
  Apache-2.0), zeroize's `serde` feature; tempfile (MIT OR Apache-2.0) for the `testing` feature.
- **Tests.** `extent_keys` `kms::openbao::tests` (18: URLs, tokens, headers, local refusals with no
  server, wrap and unwrap, rotation, revocation by policy, soft delete and `min_decryption_version`,
  destruction by trim and delete, tenants without keys, shared keys, a revoked token and a token file
  renewed, a sealed server, a missing mount, namespaces, TLS with and without `ca`, `kms::from_url`
  through the environment, a `derived` key) and `extent_store` `upload::encrypted::sealed_stores_open_through_openbao`
  (a sealed cell made, taken with one unwrap, refused when the policy drops its key, opened again,
  a tenant without a key in the clear). Each starts `bao server -dev` on a free port
  (`extent_keys::testing::openbao::DevServer`, `-dev-no-store-token`), and is skipped, saying so,
  without `bao` (`FXDAX_BAO`, else `PATH`), unless `FXDAX_REQUIRE_OPENBAO` is set: then it fails.
  `scripts/test-openbao.sh` fetches OpenBao's release (2.7.2, by digest, `scripts/lib/openbao.sh`)
  and runs them so required; CI's `object-stores` job could run it (proposed, not added).
