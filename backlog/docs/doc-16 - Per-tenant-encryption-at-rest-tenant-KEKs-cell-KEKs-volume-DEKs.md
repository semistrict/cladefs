---
id: doc-16
title: 'Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs'
type: specification
created_date: '2026-11-14 18:32'
updated_date: '2026-11-14 23:44'
---
# Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs

Design, 2026-11-14. The user's decisions of that day are marked *(decided)*. TASK-411.1 (keys, KMS), TASK-411.2 (frame FORMAT 2), TASK-411.3 (cell KEKs, DEKs, rings, below: **What TASK-411.3 made**) and TASK-411.6 (the collector's compaction, below: **What TASK-411.6 made**) are implemented.

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
  `fxdax::remote::runtime()`: extentd, the fxdax CLI's FUSE mounts, fxdaxctl; and `fxdax-gc`, to
  compact sealed cells (TASK-411.6; marking keyless). A cell is made sealed iff
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
  the cell's KEK. An ancestor of another cell with DEKs is refused (`Error::Inherited`, `ENOTSUP`):
  forks across cells of sealed branches wait for TASK-411.4's rewrap, and fxdax refuses them
  before anything is made. A branch's first writer gives its record its first DEK by a conditional
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
  (TASK-411.5); forks across cells (TASK-411.4).

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
  cell and the KMS's refusal, and records no digest (what it left went by the KMS, outside the
  cell's listing), so the next pass goes over the cell in full and compacts once the KEK is back.
  `fxdax-gc` prints them on the pass's line and the sweep's (`left N sealed uncompacted, without
  their cell's KEK`) and in `--metrics` (`fxdax_gc_sweep_objects{action="uncompacted"}`,
  `fxdax_gc_sweep_locked_cells`).
- **`fxdax-gc` reads `FXDAX_KMS`** as every fxdax process does (`extent_keys::kms::from_url`); one it
  names that can't be had stops it before it collects (a CLI fails at once rather than leave every
  sealed cell uncompacted); `--dry-run` reads none.
