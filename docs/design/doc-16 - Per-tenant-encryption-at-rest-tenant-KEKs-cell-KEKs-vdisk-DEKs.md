---
id: doc-16
title: 'Per-tenant encryption at rest: tenant KEKs, cell KEKs, vdisk DEKs'
type: specification
created_date: '2026-11-14 18:32'
updated_date: '2026-11-19 02:33'
---
# Per-tenant encryption at rest: tenant KEKs, cell KEKs, vdisk DEKs

Design, 2026-11-14. The user's decisions of that day are marked *(decided)*. TASK-411.1 (keys, KMS), TASK-411.2 (frame FORMAT 2), TASK-411.3 (cell KEKs, DEKs, rings, below: **What TASK-411.3 made**), TASK-411.4 (forks into other cells, pools; below: **What TASK-411.4 made**), TASK-411.5 (open segments and large extents' raw files encrypted, logs authenticated, below: **What TASK-411.5 made**), TASK-411.6 (the collector's compaction, below: **What TASK-411.6 made**), TASK-411.7 (shredding through the control API, below: **What TASK-411.7 made**), TASK-411.9 (recovery frames nothing before keys), TASK-411.10 (a tenant's missing KEK believed for a minute), TASK-411.13 (locked cells skipped while their KEK stays refused), TASK-411.14 (`cladefs-gc --delete-branch` under `CLADEFS_KMS`), TASK-411.15 (the guest e2e tests run sealed, below: **What TASK-411.15 made**), TASK-411.16, .17 and .18 (OpenBao, AWS KMS and Google Cloud KMS, below: **Real KMSes**), TASK-411.19 (a KEK the KMS forgot told from none by the tenant's sealed marker, below: **A forgotten KEK told from none**), TASK-411.23 (sealed stores frame in place, below: **What TASK-411.23 made**), TASK-411.27 (the bucket KMS, tenants' KEKs kept in the bucket under a master key, below: **The bucket KMS**) and TASK-411.28 (no plaintext: every cell sealed, always, below: **One sealed mode**) are implemented. The sections after **One sealed mode** are the history of how each part was made; where they speak of cells, objects or logs in the clear, of `KEKLESS_FOR` or of the sealed marker, **One sealed mode** says what replaced them.

## One sealed mode (TASK-411.28)

*(decided, the user, 2026-11-15)* **No plaintext.** Every cell is sealed, always, with no opt-out;
the bucket KMS is the default whenever `CLADEFS_KMS` names none; and every process that opens cells
refuses to start with neither a KMS nor a master key. What this is now, whatever the sections below
say of the clear:

- **No clear path.** `extent_frame`'s `Seal::Clear` and `Opening::Clear` went: every object is
  `Sealed` (kind 1), `Authenticated` (kind 2: checkpoints, metadata) or `Sectors` (kind 3: framed
  in place under its raw file's XTS key), and a trailer whose seal byte is 0, in the clear, is
  **corrupt** ("a seal in the clear, which no object has"). `extent_frame::in_the_clear` tells one
  from its bytes for keyless checkers (`extent_store::on_disk`, the guest tests' `sealed` check). A
  segment's log is `FXLOG\0\0\x03` alone, its header authenticated; one headed
  `FXLOG\0\0\x02` is a clear cell's, which `on_disk` names (`CLEAR_LOG_MAGIC`). Raw files are
  always XTS-sealed; a cell refuses a write that is not whole sectors whatever its keys. A packer
  is always keyed (`Packer::new((dek, id))`, `Packer::in_place(sectors)`); parts packed ahead are
  compressed unsealed in memory (`PartPacker::compress`) and sealed as their object is.
  `Error::Downgrade` went with the clear: a seal byte zeroed is caught as corruption, by every
  reader, holding keys or none. A reader holding any key refuses an object authenticated under a
  key it lacks, whatever the object's branch (`keys::View::sealed`): whether a branch was sealed
  came from its record, which nothing authenticates, so a bucket's writer stripping an ancestor's
  record of its DEKs could have had a checkpoint of its own, under a key it made up, read as stored
  (found by TASK-411.28's review; `keys` test
  `objects_authenticated_under_keys_unheld_are_refused_by_every_ring_holding_keys`). The keyless
  ring alone, the collector's marking's, reads such objects, checking CRCs.
- **Every cell has a KEK, every writer's record a DEK.** A cell is made only with a KEK its
  tenant's KEK wraps (`keys::new_cell_kek`, now `(kms, tenant) -> (secret, Kek)`;
  `cell::Object::made`, `cell::create` and `cell::hold`'s making take a `Kek`, never none). A
  cell's object without one is refused ("cell X keeps no KEK, which every cell's object does"),
  as is a writer's record without a DEK ("branch X's record keeps no DEK, which its writer seals
  under"). A writer's ring always seals (`Ring::sealing`); `Ring::keyless` is the reader's that
  holds no key (marking, recovery's keyless checks). A point, which no writer takes, seals the
  open segment it never writes under a key of its own (`Ring::for_point`, kept apart from the
  DEKs it inherits). `keys::taking` makes a cell with its KEK where a tool takes a branch whose
  cell has no object (`Writer::open_under`, `cladefs-gc-fill`).
- **'No such key' is destroyed, for every backend.** `kms::Error::NoSuchTenant` stays the
  backends' precise answer (`LocalKms`, OpenBao, AWS, Google Cloud), but `keys::Kms` treats it as
  it treats `Revoked` and `Destroyed` (`keys::revokes`): the tenant's pool and cells' KEKs dropped,
  the tenant refused (`Kms::revoked`) until the KMS answers a call for it again, and the cell
  refused, `Error::Kms` naming it (`tenant T: no KEK in the KMS`, `EACCES` at cladefs). A new cell of
  a tenant the KMS has no KEK of is never made; a fork into one is refused before anything is made.
- **The sealed marker went** (TASK-411.19; `layout::sealed`, `layout::Kind::Sealed`,
  `bucket::put_if_absent`, `bucket::exists`, `keys::Kms`'s markers, the bucket KMS's marker read).
  It told a KEK the KMS forgot from a tenant that never had one, because the second meant the
  clear. With no clear, both refuse the tenant alike, for real KMSes; the bucket KMS shreds by
  tombstone; and a tenant's KEK object deleted outright loses what it wrapped as shredding does,
  the tenant's next wrap making it a new KEK (`bucket_kms` test
  `kek_objects_deleted_outright_lose_what_they_wrapped`), never a cell in the clear. A host's
  cells made after from cell KEKs it pooled, or wrapped under the old KEK it held for
  `FRESH_FOR`, are under the old KEK and lost with it at their next taking, with what was written
  to them (TASK-411.29).
- **The kekless cache went** (TASK-411.10's `KEKLESS_FOR`, `keys::Kms::kekless`): it spared a
  plaintext tenant's openings a wrap each; no tenant is plaintext. A refusal is noted as every
  revocation is (`Kms::revoked`), and a tenant refused asks the KMS again at its next call.
- **The KMS is required, and checked as each process starts.** `bucket_kms::from_env` returns
  `Result<HostKms, String>`: `CLADEFS_KMS`'s KMS; else, given a master key, the bucket KMS over the
  host's bucket; else the refusal `bucket_kms::no_kms()`:

  > no KMS: CLADEFS_KMS names none, and neither CLADEFS_MASTER_KEY nor CLADEFS_MASTER_KEY_FILE gives a
  > master key; every cell is sealed (doc-16): set CLADEFS_KMS to a KMS's URL, or CLADEFS_MASTER_KEY
  > to a master key (openssl rand -hex 32) to keep tenants' KEKs in the bucket

  `cladefs::remote::require_kms` (`the host's KMS: ...`) is asked as cladefsd starts (after its
  host's environment is read), as cladefsctl acts on cells itself (`locally`), and as `cladefs-mount`
  mounts; the guest tests' runner asks it before any case (`test_cases::sealed::require_kms`).
  `cladefs-gc` needs a KMS to collect (`--delete`, `--mark-only`) and for `--delete-branch`,
  refusing without one; its dry runs, `--branches`, `--usage` and `--rotate-master-key` need none.
  `cladefs-gc-fill` chooses its KMS from the environment, else a `LocalKms` of its own.
- **Collectors.** A collector given no KMS (`Collector::kms(None)`) marks and deletes but compacts
  nothing, every cell locked, as for a KEK refused; `cladefs-gc` refuses to collect without one, and
  an image's own collector (`cladefs::storage::collect`) holds the host's KMS.
- **Tests.** Every test is sealed. `extent_keys::testing::MemoryKms` makes each tenant's KEK on
  first use, the unit tests' default: `extent_remote::keys::testing::{kms, host_kms, host}`, one
  per process, which cladefs's unit tests fall back to and `extent_remote::testing`'s readers
  (`ring_of`, `read`) unwrap under. `extent_keys::testing::KnownKms` keeps every tenant's KEK one
  known key, so that a test's process and those it starts agree with nothing shared: cladefs's
  `tests-kms` feature, which cladefs's and sqlite_cladefs' tests turn on and a release build refuses,
  has a host given no KMS seal under it (`cladefs::remote::testing`); a bench seals under it unless
  its environment names a KMS (`cladefs::remote::testing::sealed`). Cells kept in no bucket seal
  under a fixed DEK (`extent_store`'s `unkept` feature: `Store::create`, `Store::open`,
  `Opening::new`, `UNKEPT_DEK`). Scripts that start cladefs's processes without a KMS of their own
  give them the tests' master key (`scripts/lib/test-kms.sh`: gc-sweep.sh,
  lib/fork-time-linux-host.sh, test-rustfs.sh; pjdfstest-fuse.py's own; the Go daemon test).
  Clear twins were removed or made to assert the sealed path; a frame in the clear is a
  corruption test now (`extent_frame` `objects_in_the_clear_are_corrupt`, `extent_store`
  `upload::encrypted::objects_of_sealed_branches_in_the_clear_are_refused`).
- **Not done here.** TASK-460: a take of a sealed branch reads its cell's object twice (once for
  its recovery's KEK, once to hold it), one GET more than a clear take asked.

## Goals

*(decided)* Three, and no more:

1. **BYOK.** A tenant's root key (its KEK) lives in a KMS, possibly the tenant's own; cladefs gets
   wrap and unwrap rights only, which the tenant can revoke.
2. **Crypto-shredding.** Destroying a tenant's KEK leaves its data unreadable everywhere it is:
   the bucket, its object versions and copies, hosts' disks.
3. **A bucket read by someone else learns nothing useful** of a tenant's data.

**Not a goal:** keeping plaintext from a compromised host. The extent cache holds plaintext pages,
as DAX needs: one copy, mapped into every VM's window (README, doc-13). Keeping the host blind needs
encryption in the guest (Fxfs's fscrypt), and Linux turns DAX off for encrypted inodes, which
loses cladefs's reason to be. Guest-side encryption stays possible as a tenant's opt-in without DAX;
it is not this design.

## Keys

```
tenant KEK   in the KMS; never leaves it. Destroying it shreds the tenant.
  cell KEK     random; wrapped by the tenant KEK; kept in the cell's object (<C>cell)
    vdisk DEK    random; wrapped by the cell KEK; kept in the branch's record (<C>branches/<b>)
      object subkey = HKDF-SHA256(DEK, salt || object key); salt random per object, in its trailer
```

*(decided: tenant, cell, vdisk)* Each level is a unit the bucket already has (doc-7, Layout):

- **Vdisk DEK, one per branch.** A branch is the only writer of its objects
  (`segments/<branch>/`, `extents/<branch>/`, `checkpoints/<branch>/`), and recovery reads its
  record and its ancestors' records already.
- **Cell KEK.** The cell is the unit of placement: a host taking a cell unwraps its KEK with one
  KMS call and holds every vdisk's DEK from then on. It also scopes forks' rewrapping (below).
- **Tenant KEK.** The only key the KMS holds; BYOK and shredding act on it.

A DEK has generations: a record keeps a list of `(generation, wrapped DEK)`, a new writer may add one,
and every object's trailer names the branch and generation its subkey came from. Rotating a KEK
rewraps what it wraps (the KMS's own rotation, or a conditional update of each record or cell
object); rotating a DEK starts a generation, which encrypts what writers write from then on. *(found,
TASK-411.6)* Old data does not move to it: checkpoints rewrite metadata alone, and compaction
encrypts each object's moves under the generation the object was encrypted under, as a cell opened
before the generation was added holds none of it. Moving old data on needs readers that learn
newer generations (TASK-411.12).

**Why a subkey per object, from a random salt:** object keys are create-only, and since TASK-407
each names its writer's epoch (`segments/<branch>/<epoch>.<segment>`); before, a fenced writer kept
its own bytes for `segments/<branch>/<seq>` in its WAL after another epoch's landed there (doc-14).
Nonces from the key alone would have repeated across the two, leaking both plaintexts' XOR and
GCM's authentication key. A salt per object makes every subkey new, whatever key it lands at.

## What is encrypted

*(decided: data encrypted, metadata authenticated only)*

- **Data parts:** each part of a framed object (`extent_frame::PART`, 1 MiB) is compressed
  first, as now, then encrypted with AES-256-GCM under the object's subkey
  (`dek.derive(salt, ["cladefs frame parts"])`); the nonce is the part's index, the associated data
  its index, encoding, stored and unpacked lengths. The table's entry holds the tag (every FORMAT 2
  entry has one, zeros in the clear: 33 bytes a part). Compression keeps working; ciphertext is the
  same length. *(TASK-411.2)* Nothing is bound to the object's key: a rebase renumbers and restamps
  closed segments, uploading them under other keys (doc-14), and own extents' keys name their epoch
  and segment. The salt binds instead: a part moved to another object fails under that object's
  subkey. A whole object copied to another key of its branch is caught by its footer where the
  footer names its place (segments: branch, number, epoch), and by TASK-411.3 for the others.
- **Footers, tables, trailers, checkpoints, moves' maps, records, cell objects, heads, collector
  leases:** in the
  clear, as now, plus a MAC under a key derived from the DEK (*TASK-411.3*: a checkpoint is framed
  `Encryption::Authenticated`, kind 2: its parts, the table's entries, in the clear, each tagged with a
  GMAC under the object's parts subkey, its footer and table MACed, so that the collector reads it
  holding no key and a key-holding reader checks every byte), which key-holding readers check (a
  frame's footer and its table, which covers the trailer's `encryption` and the footer's MAC, each have an
  HMAC-SHA256 under `dek.derive(salt, ["cladefs frame mac"])` in the trailer; restamping a footer
  needs the key and keeps the salt). The
  collector reads them without keys, checking CRCs, as now. They show extent IDs, sizes, the fork
  tree and live bytes; Fxfs's own metadata (names, inodes, directories) is data inside extents, so
  encrypted. A bucket writer could tamper with what the collector reads; it can delete objects
  anyway, so that is no new power.
- **The trailer** gains the salt, the branch and DEK generation, and a flag: FORMAT 2 of
  `extent_frame` (doc-11 rule 1: a FORMAT 1 reader names FORMAT 2, not corrupt). An object in the
  clear says so, its MACs and tags zeros; as its `encryption` is not authenticated, a reader of a branch
  with a DEK refuses its objects in the clear (TASK-411.3), or a bucket writer could downgrade one.
- **Local disk:** closed segments in the WAL and the part cache hold the stored bytes, so they are
  encrypted with nothing more. *(decided)* **The open segment is encrypted too**, under the vdisk
  DEK, so that shredding covers hosts' disks without counting on their disk encryption. Its pieces
  are read back by offset until the segment is framed, so it needs a length-preserving, random-access
  cipher: AES-256-XTS per 4 KiB sector (the workspace has `storage_xts`), tweak the sector, key
  derived from the DEK and a salt of the file's own, kept in its log's header (TASK-411.5, below). *(found
  2026-11-14)* Every write reaching it is whole sectors, so XTS needs no read-modify-write: with an
  assert that each length given `Segments::reserve` and `Segments::append` is a multiple of 4096,
  every test of `extent_store`, `sqlite_extents` and `cladefs` on macOS passed (574) but the 62 of
  `extent_store`'s simulation, whose writes are of 512 bytes (`sim/world.rs`, `BLOCK`); the guest's
  writes are aligned to the device's `block_size`, 4096 (doc-4). So the cell refuses a write that
  is not whole sectors, and the simulation writes 4 KiB blocks (TASK-411.8). Large extents' own raw
  files (`open-extents/`) hold guest data until sealed too, and are encrypted the same way
  (TASK-411.5).
  The open segment's log holds `Contents`, metadata, so it gets a MAC, not encryption.
- **Memory:** the extent cache and DAX windows hold plaintext. DEKs and KEKs in memory are
  zeroized on drop and never written out unwrapped.

## Who holds which key

| Process | Keys | Why |
|---|---|---|
| cladefsd, a cell's holder | the cell's KEK; its vdisks' DEKs; inherited DEKs | reads and writes the vdisks |
| A host opening a point or a follower (doc-8) | the DEKs of the branch and its ancestors | reads only |
| `cladefs-gc`, marking | none | reads metadata only |
| `cladefs-gc --delete-branch` | the branch's cell's KEK, through the KMS, only to record a fork into another cell whose child has no record yet | its checkpoint at the fork point is authenticated under the branch's DEK, which the child requires (TASK-411.4, TASK-411.14) |
| `cladefs-gc`, compacting | the cell's KEK, through the KMS, for that pass | *(decided)* compaction copies extents out of decompressed data (`collector::compact_object`), so it decrypts and re-encrypts, under the source branch's DEK, the generation the object was encrypted under, with a new salt (TASK-411.6). Revoking a tenant's key stops its compaction, which is right; its garbage still goes. A collector that never decrypts (moving whole stored parts) was the alternative; it reclaims less, as one live extent keeps its 1 MiB part |
| the control API | whatever a fork across cells needs | rewraps (below) |

## Forks and shared points

A child reads its parents' objects, under the parents' DEKs. *(decided)* **A fork into another cell
rewraps the inherited DEKs under the child's cell KEK** and keeps them in the child's record: its
own DEK plus each inherited `(branch, generation, wrapped DEK)` from outside its cell. So a child
never needs another cell's KEK, nor another tenant's KMS, and **shredding a parent leaves its
children what they inherited**, as doc-7's rule that forks keep what they inherited (Deleting a
point) already says. Shared points (TASK-395.7) work the same way: the trusted control API, which
can unwrap both, rewraps at each fork into another tenant's cell, so the point's owner shredding its
tenant stops new forks of the point and breaks no fork.

A fork into a new cell needs a new cell KEK, which only the KMS can wrap under the tenant's KEK.
*(decided)* **Hosts keep a pool of cell KEKs per tenant, wrapped ahead of time** in batches, and a
fork takes one: no KMS call on the fork's path, which doc-7 keeps O(1) in the bucket. A tenant's pool
is discarded when its KEK is revoked or destroyed (the next refill's wrap fails, or the control API
says so), so a revocation stops new cells at once, not after a TTL; a pooled key never used is
dropped with its pool, never written out unwrapped.

## Shredding

- **A tenant:** destroy its KEK in the KMS, then tell every host (`ForgetTenantKeys`, TASK-411.7);
  under the bucket KMS, `ShredTenant` on every host, which leaves a tombstone in its KEK object's
  place (The bucket KMS). With cells in several buckets (backlog doc-23, TASK-633.6), each bucket's
  cells are under that bucket's KMS (`bucket_kms::ByBucket`): `ShredTenant` leaves a tombstone in
  every bucket under a bucket KMS that the registry of buckets records, and names each bucket
  under a KMS named by URL, whose KEK of the tenant is destroyed there, then `ForgetTenantKeys`;
  a bucket out of reach is named too, and `ShredTenant` again finishes it.
  Absolute: every cell KEK of it, in every version of every cell object, unwraps no more.
- **A cell:** remove its wrapped KEK from its object (`ShredCell`, conditional as every cell
  update, TASK-411.7). Only as strong as the bucket keeps no older version of `<C>cell`: with
  versioning on, a lifecycle rule must expire non-current versions (S3
  `NoncurrentVersionExpiration`; GCS versioning off or a lifecycle delete of non-current
  versions), or the old wrapped KEK stays unwrappable while the tenant's KEK lives. With
  versioning off, nothing more is needed. The README says it for operators, doc-7 for the bucket.
- **A vdisk:** deleting it, as now; its record stays (records stay, doc-7) and could have its
  wrapped DEKs cleared once the collector has removed the branch's objects.

## Decided later the same day

- **Unencrypted cells stay** *(decided; superseded by TASK-411.28: no plaintext, One sealed mode)*: FORMAT 2's trailer says whether an object is encrypted,
  and a cell with no wrapped KEK (the default tenant's, tests that ask) writes in the clear, its records with no
  DEKs. Both paths are tested: every encryption test has a plaintext twin where it applies.
- **No migration** *(decided)*: no bucket in use needs keeping; FORMAT 2 refuses FORMAT 1 by name,
  as `layout::FORMAT` 5 refused 4 (doc-11 rule 1).
- **aws-lc-rs** *(decided)* for AES-256-GCM, AES-256-XTS (if it has XTS; `storage_xts` otherwise)
  and HKDF: already in the lock file, Apache-2.0 and ISC, AES-NI and ARMv8 AES.
- **Real KMS backends later** *(decided)*: a trait and a local fake first. Then (the user's choice,
  2026-11-14) OpenBao Transit, AWS KMS and Google Cloud KMS (below: **Real KMSes: Google Cloud KMS**).

## What TASK-411.3 made

- **Formats.** `layout::FORMAT` 10 (9 refused by name, no migration; 11 since TASK-407). A cell's object gains
  `keks: Vec<Kek { generation, wrapped }>`, its KEK's generations wrapped by the tenant's KEK in
  the KMS, oldest first; none: a cell in the clear. A record gains `deks: Vec<Dek { generation,
  kek, wrapped }>`, each generation of the branch's DEK wrapped by the cell's KEK of generation
  `kek`, with AES-KW under `HKDF(cell KEK, info = "cladefs dek wrap" || tenant || cell || branch ||
  generation)`: a wrapped DEK copied to another record, or relabelled, fails AES-KW. `Record` is no
  longer `Copy`. An own extent's footer is its `ExtentId`, a moves' footer names the key it is the
  moves of (`Moves::of`): a whole object copied to another key of its branch is refused there, its
  footer under the MAC.
- **The KMS and how a cell chooses.** `CLADEFS_KMS=local:///DIR` (a `LocalKms` directory, the only
  backend; `extent_keys::kms::from_url`), read by every process that opens cells through
  `cladefs::bucket::runtime()`: cladefsd, the cladefs CLI's FUSE mounts, cladefsctl; and `cladefs-gc`, to
  compact sealed cells (TASK-411.6; marking keyless). *(TASK-411.28: every cell is made sealed, a
  process with no KMS refusing to start; `NoSuchTenant` refuses the cell as destroyed: One sealed
  mode.)* A cell was made sealed iff
  the host has a KMS and the cell's tenant has a KEK in it; `NoSuchTenant` means the clear (unless
  the tenant's sealed marker says it had one: TASK-411.19, below), any
  other refusal (revoked, destroyed, unreachable, a bad `CLADEFS_KMS`) fails the making, so that no
  cell a tenant wants encrypted is made in the clear. New cell KEKs come from a per-tenant pool of keys
  wrapped ahead (a wrap when it is empty; a KEK that made no cell goes back to it).
  *(TASK-411.10; gone with TASK-411.28, One sealed mode)* A host believes a `NoSuchTenant` for `keys::KEKLESS_FOR`, 60 s, from the wrap
  that heard it (`keys::CachingKms::new_kek`, or a pool's refill), asking the KMS nothing for that tenant
  meanwhile: every opening of a cell whose record has no DEK asks for a KEK in case it makes the
  cell, so a host opening a plaintext tenant's cells made one wrap call per opening; now one a
  minute at most (openings asking at once, before the first answer, ask each: a wrapped KEK is one
  cell's alone). The cost: a tenant given a KEK has new cells made encrypted within 60 s, not at once;
  cells made in the clear before stay so (a cell's encryption is chosen as it is made). Give a tenant
  its KEK a minute before making the cells it wants encrypted. Any answer of the KMS for the tenant
  forgets it had none. `extent_bucket` `keys::tests::tenants_without_a_kek_are_asked_for_one_once_a_while`
  (paused clock) and `extent_store` `upload::encrypted::cells_of_tenants_with_no_kek_are_in_the_clear`
  count the calls.
- **Taking a cell** unwraps its KEK once per taking (`keys::CachingKms`, keyed by cell, generation and
  the taking's epoch and holder; a writer's recovery, which reads before it holds the cell,
  predicts the taking it makes, `CellKeys::to_hold`), however many vdisks open: a test counts one
  unwrap for three. A cell the host made unwraps nothing. Branch DEKs are unwrapped locally.
- **Rings.** Opening a cell builds a `keys::Ring`: its branch's DEK generations (the newest encrypts
  new objects) and its ancestors' along their origins, read from their records and unwrapped under
  the cell's KEK. (An ancestor of another cell with DEKs was refused, `Error::Inherited`, until
  TASK-411.4 rewrapped them: below.) A branch's first writer gives its record its first DEK by a conditional
  update (`keys::with_dek`): records written by a parent's writers for a fork name none, so that
  they stay the same whoever writes them. Points have none. Every writer and reader of the cell
  holds the ring: the writer (fences encrypted, checkpoints authenticated, recovery checked), the
  cell (segments and own extents encrypted, local files read under it), and the host's fetchers,
  which open tails and parts under the union of the rings of the cells the host serves
  (`keys::HostRing`, in `fetch::Memory`), a tail opened once the host holds its key, `NoKey`
  never kept; peers are served stored bytes, needing none.
- **Downgrades.** A reader holding a branch encrypted (`Keys::encrypted`) refuses its objects in the
  clear (`Error::Downgrade`, naming the object) and authenticated ones under keys it lacks.
  *(TASK-411.28: no object is in the clear; one whose seal says so is corrupt, `Error::Downgrade`
  gone.)*
- **Generations and rotation.** `keys::add_generation` appends a DEK generation by a conditional
  update; cells opened after encrypt under it, the older still read. `keys::rotate` adds a cell KEK
  generation (conditional on the holder holding the cell), then rewraps every record's DEKs under
  it, each by a conditional update. Older KEK generations stay in the cell's object: a writer that
  read the cell before the rotation may wrap a new DEK under the generation it read after the
  rotation listed the records, so dropping them needs that fenced first (follow-up).
- **Local disk.** `wal::whole_footer` checks a framed segment whole by the keyless check
  (`extent_frame::check`), encrypted or not. *(TASK-411.9)* WAL recovery runs before the cell's keys
  are known and frames nothing: a segment it closes again keeps its raw file and log, its footer
  folded and stamped provisionally as before (`Recovered::closing`), and is framed as the cell
  opens, holding its ring, first of all (`Recovered::close`, from `Recovering::open`): encrypted if the
  ring encrypts, in the clear otherwise, the raw file and log removed once the framed file and its
  entry are synced. A crash before leaves them, which the next recovery closes again alike;
  `drop_wal` removes them, synced. (TASK-411.3 framed it in the clear and encrypted it again as the cell
  opened, its data on the host's disk in the clear meanwhile, and for good if the host died
  between; `wal::reseal` and `Dir::replace` went.)

## What TASK-411.6 made

- **Marking stays keyless.** A pass reads records, checkpoints (authenticated, their parts in the
  clear), segments' footers, moves' maps and trailers holding no key, as before; each candidate's
  trailer, which it reads anyway for its data's length, says whether it is encrypted.
- **Compacting under the cell's keys.** A pass whose compactable objects include an encrypted one
  reads the cell's object and unwraps its KEK through the collector's KMS
  (`collector::Collector::kms`, `sweep::Sweep::kms`), by a `keys::CachingKms` made for that pass alone:
  one unwrap per KEK generation its branches' DEKs name, one in practice, however many objects it
  compacts, and nothing kept past the pass, so a tenant's KEK revoked stops the next. The DEKs come
  from the pass's records of those branches (`keys::compacting`). A pass compacting only objects in
  the clear, or nothing (no candidate below its fraction, `--compact-below 0`), and every dry run
  ask the KMS nothing: `collector`'s tests count the calls with `extent_keys::testing::Counted`.
- **Moves encrypted as their objects were.** `compact_object` reads the object under the ring's view of
  its branch (an object in the clear of a branch with DEKs is refused, a downgrade) and encrypts its
  moves under the very key its trailer names, branch and generation, with a salt of their own;
  their footer, `Moves::of` under its MAC, names the object they are the moves of, so moves copied
  whole to another moves key of the branch are refused, by readers holding the keys or none
  (TASK-411.3's check, tested now). *Why the source's generation, not the record's newest:* every
  reader that may follow the moves read the object under that key; a cell's ring is built as it
  opens, and one opened before a generation was added holds none of it (TASK-411.12). Moves of
  objects in the clear are in the clear, as before.
- **KEKs not to be had.** The KMS refusing the cell's KEK (the tenant's KEK revoked or destroyed,
  the KMS failing), or the collector having no KMS: the pass still marks, compacts what is in the
  clear, and deletes what no live branch needs, leaving the encrypted uncompacted; it counts them in
  its report (`Report::locked`, the passes, `Report::uncompacted`, the objects), warns naming the
  cell and the KMS's refusal. *(TASK-411.13)* It records its listing's digest and how many it left
  (`passed::Passed::uncompacted`); a pass after it, the cell unchanged, skips the cell (its collector
  lease and one listing) while the tenant's KEK is still not to be had, which a sweep asks the KMS once a
  tenant for all its cells (`collector::Heard`, a wrap of a key of no use; none without a KMS), and
  goes over it in full, compacting, once it is, or once the cell changes. (TASK-411.6 recorded no
  digest: every pass over a revoked tenant's idle cells went in full.)
  `cladefs-gc` prints them on the pass's line and the sweep's (`left N encrypted uncompacted, without
  their cell's KEK`) and in `--metrics` (`cladefs_gc_sweep_objects{action="uncompacted"}`,
  `cladefs_gc_sweep_locked_cells`).
- **`cladefs-gc` reads `CLADEFS_KMS`** as every cladefs process does (`extent_keys::kms::from_url`); one it
  names that can't be had stops it before it collects (a CLI fails at once rather than leave every
  encrypted cell uncompacted); `--dry-run` reads none.


## What TASK-411.4 made

- **Format.** A record gains `inherited: Vec<Inherited { branch, dek: Dek { generation, kek,
  wrapped } }>`: the DEK of every ancestor of another cell whose objects the branch reads, each
  generation, wrapped by the record's own cell's KEK of generation `kek`, with AES-KW under
  `HKDF(cell KEK, info = "cladefs inherited dek wrap" || holder (tenant, cell, id) || ancestor
  (tenant, cell, id) || generation)`: copied to another record, or of another ancestor, or
  relabelled, it fails AES-KW. `layout::FORMAT` 10 is extended, not bumped: it is unreleased (main
  is at 7), as FORMAT 2 of `extent_frame` was for TASK-411.2. They are given with the branch's
  first DEK, in one conditional update (`branch::give_deks`): **a record with DEKs holds all it
  inherits from other cells** (given), its readers needing no other cell's KEK.
- **Who gives them.** A fork's host, as it makes the fork (`extent_store::upload::Uploader`):
  `Uploader::inheriting(child)`, before anything of the fork is made (its commit, a point's fork's
  record), takes the keys the parent's cell holds in memory (its ring: its own DEKs and every
  ancestor's) and a KEK for the child's cell from the host's pool (`keys::inheriting`); then
  `Uploader::forked(commit, child, inheriting)`, once the child's record is made, makes the child's
  cell with that KEK (`cell::make`: create-only, held by no placement, token 0, so that any
  placement takes it; the host's `keys::CachingKms` keeps the KEK for the cell's first taking here, so
  that the child's cell opening here unwraps nothing), and gives the child's record its first DEK
  and the parent's keys rewrapped under the new KEK (`keys::give_inherited`). A failure there fails
  the fork, which cladefs deletes (`Uploader::unfork`). Nothing is given for a fork within a cell (the
  child's writer gives its own DEK as before, and with it what the parent's record inherits), for
  a point, nor for a fork of branches in the clear: plaintext forks make no request more, no KMS
  call. A child its fork's host did not give keys to (its host died between the record and the
  give, or a fork recorded by the parent's next writer or deletion) is given them by its first
  writer (`keys::with_dek`), reading across, below.
- **Points keep no key**, nothing writing them: their copies read what they name by crossing into
  their origins' cells (one unwrap of the origin's cell KEK a taking, cached), and forks of them,
  shared points' included, rewrap from the copy's ring like any other fork: so the
  point's owner shredding its tenant stops new forks of it (no copy opens) and breaks no fork. A
  host holding a copy open, told the tenant's KEK is revoked (`CachingKms::revoked`), refuses forks of it
  before anything is made.
- **Rings** (`keys::ring`): a given record is read with its own cell's KEK alone: its DEKs, its
  inherited ones, and its same-cell ancestors' records; an encrypted ancestor of another cell it holds
  not is refused (`Error::Inherited`). A record without its keys (a point, a fork's child before
  they are given, a branch of a cell in the clear) crosses into each encrypted ancestor's cell, its
  object read and its KEK unwrapped, taking its DEKs and inherited ones; but a writer of a cell in
  the clear inheriting anything encrypted is refused, and so is a fork of an encrypted cell into a cell
  in the clear, before anything is made (`ENOTSUP`): no encrypted data goes into the clear.
- **KMS calls on the fork's path: none**, its cell KEK from the pool, the parent's keys in memory.
  The child's first opening on the forking host: none. On another host: one unwrap of the child's
  cell KEK, as for any cell taken.
- **Pools** (`keys::Pooling`; cladefs: 8 a tenant at least, 16 a refill, on the uploaders' runtime):
  a take leaving a pool low, or an encrypted cell of the tenant opening, refills it in a batch of
  wraps at once, off the fork's path (`CachingKms::refill`, `CachingKms::filled`); an empty pool wraps one on the
  path. `CachingKms::revoked(tenant)`, called on every revoked or destroyed answer of the KMS (a refill's,
  an unwrap's) and by the control API (TASK-411.7's to wire), drops the tenant's pool, a refill
  going adding nothing, and its cached cell KEKs, and marks it refused: no refill, no fork
  inheriting its keys, until a KMS call for it succeeds again. KEKs a pool wrapped before a
  destruction nothing has heard of yet still make cells, unreadable from the first, until a refill
  or another call hears it.
- **Checkpoints at fork points** (`recovery::checkpoint_at`, which `branch::record_fork` writes
  for a child of another cell) are authenticated under the parent's DEK, if it is encrypted, as its
  writers' are: a child holding the parent's keys refuses one in the clear. Its writer's ring authenticates
  them (`branch::record_fork_under`, `keys::Parents::Held`); a deletion recording an encrypted
  branch's forks, the host's KMS (`branch::delete_under`, `extent_store::upload::delete_branch`):
  with none, it fails rather than write one the child refuses; a failed fork's claim, the host's
  KMS (`branch::delete_fork_under`). Asked for only when one is to be written. *(TASK-411.14)*
  `cladefs-gc --delete-branch` deletes under `CLADEFS_KMS`'s KMS, as its passes compact: an encrypted
  branch with an unrecorded fork into another cell (its writer died between the fork's
  acknowledgement and recording it) is deleted given one, the checkpoint at the fork point
  authenticated under the branch's DEK. Without one the deletion fails naming the tenant and
  `CLADEFS_KMS`, its head ended and its record not yet deleted: a deletion stopped partway, which
  running it again with the KMS finishes (`extent_bucket` `tests/gc.rs`
  `deletes_encrypted_branches_recording_their_forks_into_other_cells_under_the_kms`).
- **Rotation** rewraps a record's inherited DEKs with its own; `add_generation` keeps them.
- **Generations a child reads.** A child keeps the generations its parent's lineage had when it
  was given its keys; what it reads of its parent was encrypted under them (segments up to the fork
  point, the checkpoint there, written before the child's record). TASK-411.6's compaction must
  keep an object's generation when it rewrites one a child inherits, or the child loses it.
- **Crashes in the middle of a fork** (doc-7's protocol unchanged: the record create-only before
  the keys, forks of points on many hosts each giving its own child, failed forks deleted): before
  the record, nothing is given and the pooled KEK goes back (`Inheriting` gives it back when
  dropped unused); the record made, the keys not given: the directory left unmade is deleted,
  deleting the child, and any opener of the child gives them itself, crossing once; the cell made
  and the keys not given: a keyed cell of no vdisk stays, as cells' objects do, and the child's
  opener uses its KEK; keys given: as any fork. A give racing a deletion or another give is
  conditional: `Deleted` fails the fork, another's keys stand.
- **specs/bucket** is unchanged: it models records by their kinds, origins and deletion, the
  objects the collector keeps, and who writes what; keys change none of those (a record's keys are
  a conditional update that keeps its kind, origin and deletion, read by no pass), the child's
  cell object is no object a pass keeps or deletes, the checkpoint at the fork point is written
  where and when it was, and a deletion refused for want of a KMS is a deletion stopping partway,
  which the model has.
- **Fork latency** (`cargo run --release -p cladefs-examples --example fork-bench`, on a Mac, a
  memory bucket, 2026-11-14): a fork of a cell of 64 extents into a new cell, median, encrypted 32-34
  ms, in the clear 31-35 ms, with a KMS or none: the same within noise; 3 bucket requests more an
  encrypted fork (the cell's create, the record's read beside it, its update), 47.5 against 44.5. With
  10 ms a request (`CLADEFS_BUCKET_LATENCY_MS`), 225-242 ms against 200-218 ms: the two round trips
  more. No KMS call. Linux numbers: the user's, later.

## What TASK-411.5 made

- **The cipher.** `extent_keys::xts::Xts`: AES-256-XTS (IEEE 1619-2007, no ciphertext stealing),
  Fuchsia's `storage_xts` over RustCrypto's `aes` (key schedules zeroed on drop), as aws-lc-rs has
  none. Checked against IEEE 1619's vectors 10 and 11 (as aws-lc's `xts_test.cc` carries them) and
  against aws-lc's own `EVP_aes_256_xts` on random keys and 4 KiB sectors (a dev-dependency on
  `aws-lc-sys`, at the version aws-lc-rs builds).
- **Raw files.** `extent_store::RawFile`: an encrypted cell's (its ring encrypts, `Ring::encrypting`) open
  segments' and large extents' raw files are encrypted a sector at a time (`SECTOR`, 4096), the
  sector's place in the file its tweak (the data unit number), under
  `Xts::derive(dek, salt, "cladefs raw file")`: two HKDF subkeys (`[info, "data"]`, `[info, "tweak"]`)
  of the branch's newest DEK and a salt drawn for the file. Writes are whole sectors (TASK-411.8;
  an encrypted cell refuses writes to large extents that are not, `Unsectored`, as it always refused
  them to segments), encrypted through a buffer; reads decrypt the sectors they are in. A large
  extent's raw file keeps its salt in memory alone: an opening removes those left. A clear cell's
  raw files are as they were, byte for byte.
- **Logs.** An encrypted cell's segment's log is headed `FXLOG\0\0\x03` (`wal::ENCRYPTED_LOG_MAGIC`):
  after the clear header's fields (record salt, branch, number), the DEK's branch (u64) and
  generation (u32) and the raw file's 32-byte salt, then an HMAC-SHA256 of the header so far under
  `dek.derive(salt, ["cladefs raw file log mac"])`; each record's payload (bincode of `LogRecord`) is
  followed by an HMAC of its offset in the log (u64) and the payload, inside the salted hash's
  frame. The runs a record holds or checksums (`Runs::Held`, `Runs::Checked`) are the raw file's
  stored bytes, encrypted: recovery, holding no key, works on stored bytes as before (salted
  hashes, CRCs). A clear cell's logs are as they were (`FXLOG\0\0\x02`).
- **Recovery.** The segments recovery closes again are framed at `Recovering::open`
  (`Recovered::close`, TASK-411.9): the log's header MAC and every record's MAC checked under the
  DEK and salt it names (a log changed, or under a DEK the ring lacks, refuses the opening, naming
  the segment), the raw data decrypted, then sealed into FORMAT 2 parts.
- **Framing.** *(Superseded by TASK-411.23, below: sealed cells frame in place, look ahead and
  hold parts as clear ones do; kept for the history.)* Closing a segment reads its raw file
  through `RawFile` (decrypting) and seals it, as TASK-411.3 did. **Sealed cells never frame in place** (segments nor large extents): a sealed
  object's bytes are AES-GCM parts and tags, not the XTS sectors the raw file holds, so framing in
  place would write every byte again anyway, and a crash part way would leave a file neither raw
  nor framed. `RawFile::clear` hands the inner file to in-place framing only in the clear.
  Nor does a look ahead (TASK-400, merged from main) see an encrypted segment's parts as it fills:
  its bytes in the raw file are XTS ciphertext, and a framing in a copy reads every part anyway,
  so `Segment::new` gives an encrypted open segment no `Ahead`, nothing is queued for the framer
  (`Job::See`), and its framing reads all its parts through `RawFile`, decrypting
  (`encrypted_segments_parts_are_neither_seen_nor_held_ahead`). A clear cell's looks ahead are main's: through
  `RawFile`, the clear file's bytes as they are, their CRCs and looks those its in-place framing
  writes. TASK-401's writing back holds for encrypted framed files as for clear copies: the framing
  writes its copy to the page cache, `make_durable` writes it back a MiB at a time before it
  syncs it, and the uploader reads it through the cache (`read_object`).
  Nor are an encrypted segment's parts packed ahead and held in memory (TASK-400's held parts,
  c2daf7630, merged from main): holding rides on the look ahead, which an encrypted segment has none
  of, so nothing of it is queued, packed or held, and its framing reads every part back through
  `RawFile`, decrypting, then packs and encrypts it, as before. Chosen over packing and encrypting ahead
  because that would need the framed object's key (`Packer::new(encryption)` draws its salt and
  derives its `ObjectKey` at framing) drawn when the segment opens, kept with it across the
  framer's queue, the threads packing ahead, eviction and a framing that fails and runs again,
  each part's nonce (its index) under it never reused for other bytes; and the parts held would be
  ciphertext the governor evicts and a framing re-reads anyway. Not holding adds no key's
  lifetime, no plaintext anywhere but the framing's own buffers, and keeps the bytes framed what
  they were. The cost: an encrypted cell's framings in a copy read their raw file back as clear
  cells' did before TASK-400, from disk once the host let its pages go (TASK-421).
  `Store`'s look ahead asserts it is of a clear raw file; `Packer::push_stored` asserts a part is
  encrypted as its object is, so a clear held part could never reach an encrypted object. Tests:
  `encrypted_segments_parts_are_neither_seen_nor_held_ahead`,
  `encrypted_stores_hold_no_parts_packed_ahead` (an encrypted cell holds nothing, reads all its parts
  back, and leaves no plaintext in any file; its clear twin holds its parts and reads its last
  alone).
- **No writer of raw files but the cell.** `Write::file()` went: `Write::write` encrypts, and
  `Write::stored` gives a caller doing its own I/O (cladefs's image device) the file, the offset and
  the bytes encrypted, which it writes again from the last whole sector written after a short
  write. `Located::Data` carries the `RawFile`: such a reader reads `RawFile::read_range` and
  decrypts it (`RawFile::decrypt`).
- **Cost, macOS (Apple M5 Pro, 15 CPUs, `cell-bench`, release, 2026-11-14).** Writes into open segments, 1 MiB extents:
  1.8-1.9 GB/s encrypted against 2.8-3.6 GB/s in the clear; reads of them while open 1.6-1.7 against
  2.3-2.7 GB/s; 4 KiB extents 195 against 209 MiB/s. Framing 32 MiB segments, four cells at once:
  text 66-76 ms each in the clear, 70-72 ms encrypted; noise 39-41 ms in the clear (in place), 83-85 ms
  encrypted (in a copy). Linux to be measured.

## What TASK-411.7 made

- **A cell shredded** (`extent_bucket::cell::shred`): one conditional update of `<C>cell`, by
  whoever calls it, not its holder, takes every KEK generation out and sets `shredded`
  (`cell::Object::shredded`, layout FORMAT 10 extended, unreleased; 11 since TASK-407). Takings and holdings of it are
  refused (`cell::shredded`: `Error::Kms`, "tenant T: cell T/C is shredded, its KEK gone", so
  `EACCES` to callers and the collector's locked path, marking and deleting on, its encrypted objects
  left uncompacted, not a corrupt pass), and so is `add_kek`. `CellKeys` of a shredded cell are
  encrypted and refuse their KEKs naming the cell: no branch of it is written in the clear, no DEK is
  given. Its holder, if any, goes on changing its vdisks until told: every change keeps the mark.
- **A host told** (`cladefs::bucket::forget(Scope, why)`, `keys::Scope::{Tenant, Cell}`), by the
  control API (`ShredCell`, which shreds then forgets, idempotent, to be called on every host;
  `ForgetTenantKeys`, once the operator destroyed or revoked the tenant's KEK in the KMS) or
  cladefsctl (`shred-cell`, `forget-tenant-keys`):
  - halts every cell of the scope open in the process (`halt::halt_cells`, halts listed by
    branch): each request fails with `EACCES` from then on, as its reopening does; cladefsd refuses
    its VM process's requests so;
  - takes the scope's cells out of the extent cache's holders (`cache::forget`): an extent no
    other open cell holds goes, its pages evicted, unmapped from every window, and no page-in
    reads it again;
  - drops what its fetchers kept of the scope's objects (`fetch::Memory::forget_kept`: tails opened
    under keys, parts unpacked in the clear) and the rings of the scope's cells
    (`HostRing::forget`, by `Ring`'s owner, the branch whose cell opened it);
  - has its KMS forget the scope (`keys::CachingKms::forget`): a tenant's unwrapped cell KEKs, pool and
    forks inheriting its keys (`CachingKms::revoked`, now wired), until the KMS answers for it again; a
    cell's unwrapped KEKs, refused from then on on this host even under a cell object read before.
- **What stays.** In memory: a halted cell's own ring and its open segment's XTS key, until
  whoever holds it closes it (a VM process whose guest met `EACCES`, a point kept for forks); the
  DEKs other cells hold, which they inherited (a fork of the shredded tenant into another's keeps
  its keys: shredding a parent leaves its children what they inherited, and their pages); pages of
  extents other open cells hold too (a shared point's fork, a public base, by design one copy for
  all). A host not told keeps reading its open cells under what it unwrapped, but opens none afresh:
  a fresh opening reads the cell object (shredded) or asks the KMS (destroyed). On disk: everything
  of the scope's is as its keys encrypted it, open segments (XTS), logs (MACed, metadata), the part
  cache (GCM), and a shredded cell's objects stay in the bucket, ciphertext, until its branches are
  deleted. A race: a cell opening on a host as it is told may finish opening; telling it again
  halts it.
- **Tests.** `extent_store` `upload::encrypted::shredded_tenants_leave_none_of_their_vdisks_readable_on_any_host`
  (two hosts, open segment, part cache on disk, a cross-tenant fork reading on) and
  `shredded_cells_leave_their_vdisks_unreadable_and_the_tenants_others_readable` (the collector
  locked, the old cell object put back reading again: why versions must expire); `extent_bucket`
  `keys::tests::shredded_cells_refuse_their_keys_and_hosts_told_forget_them`; `cladefs`
  `remote::tests::stores_of_shredded_cells_and_tenants_are_fenced_and_leave_the_cache`; Linux:
  `tests/control.rs` `sealed_stores_served_and_shredded_through_the_control_api`.

## A forgotten KEK told from none (TASK-411.19)

*(Removed by TASK-411.28, with the clear it told a forgotten KEK from: every `NoSuchTenant` refuses
the tenant as destroyed now, for every backend, and the marker, its writes and reads went. One
sealed mode says why. Kept as the history.)*

*(decided 2026-11-15: a tenant marker in the bucket)* A real KMS forgets a deleted key: AWS KMS
answers `NotFoundException` once a key's deletion waiting period is over, OpenBao Transit a 404 for
a deleted key, as for a key never made; the backends map both to `NoSuchTenant`, which alone means
the clear. Without more, a shredded tenant's next cells would be made unencrypted on any host still
serving it. The tenant's sealed marker tells the two apart, for every backend, with no host
configuration.

- **The marker.** `tenants/<tenant>/sealed` (`layout::sealed`, doc-7 Layout), its bytes the layout's
  format, its being there all it says: the tenant had a KEK, and a cell of it was made sealed (or
  was about to be). Under the tenant's prefix beside `cells/`, no cell's: no collector pass lists
  it (a pass lists a cell's prefix), `layout::Kind::Sealed` stays if one did, and nothing deletes
  it.
- **Written** by `keys::new_cell_kek`, which every maker of a cell asks for its KEK: a cell's
  opening (`extent_store::upload`), a fork into a new cell (`keys::inheriting`), the control API's
  attaching of a new cell (`cladefs::fuse::control`). A KEK handed out, from the pool or wrapped
  then, the marker is created first, create-only (`bucket::put_if_absent`: one there already is as
  good, a retry finding its own write there too), before the function returns. *Why that order:*
  the maker writes the cell's object, keeping the wrapped KEK, only after it returns; so no sealed
  cell's object is in the bucket without its tenant's marker, whatever crashes. A crash between
  leaves a marker of a tenant with a KEK and no sealed cell, which says nothing false. A host that
  wrote or saw the marker writes it no more (`keys::CachingKms`'s markers): one PUT per host per tenant,
  with its first sealed cell.
- **Read** when the KMS says the tenant has no KEK (`Kms::new_kek` heard `NoSuchTenant`, now or
  within `KEKLESS_FOR`): a HEAD of the marker. There: the tenant is refused as destroyed, naming it
  and the marker (`tenant T: its KEK is destroyed: the KMS has none, and tenants/<T>/sealed says it
  had one`, `Error::Kms`, `EACCES` at cladefs), and noted as a KMS's `Destroyed` is (`Kms::revoked`:
  its pool and cells' KEKs dropped, forks inheriting its keys refused, until the KMS answers a call
  for it again). Seen once, the host believes it for good, as nothing deletes it. Not there: the
  cell is made in the clear, as before, and the absence believed for `KEKLESS_FOR`, as the KMS's
  word is. *Why not a KEK record per host or in the cell objects:* a host new to the tenant knows
  nothing, and a new cell has no object yet; the tenant's prefix is the one place every host
  reads alike.
- **Pools.** A refill hearing `NoSuchTenant` drops the tenant's pool (as `Revoked` and `Destroyed`
  drop it), so that no cell is made with a KEK of a key since forgotten; the next take is refused
  by the marker. KEKs pooled before anything heard of the deletion still make cells, sealed and
  unreadable from the first, as for any destruction (What TASK-411.4 made).
- **Cost.** A plaintext tenant on a KMS host: one wrap and one HEAD per host per `KEKLESS_FOR` at
  most, however many of its cells open. A sealed tenant: one PUT per host, with its first cell;
  nothing on later cells nor on forks. A forgotten tenant: one HEAD per host, once, and a wrap per
  `KEKLESS_FOR`.
- **What it refuses.** A marked tenant whose KEK the KMS says it has none of makes no new cell, nor
  opens a cell that asks for a KEK in case it makes its cell (a new volume; a volume in the clear
  of a cell made before the tenant had its KEK), as a `Destroyed` answer refused them before. A
  tenant given a key again (its alias pointed at a new key) is served again, sealed, at the next
  wrap answered. **An operator who wants a forgotten tenant served in the clear again deletes its
  marker**; nothing else does. The workaround TASK-411.16 and TASK-411.17 documented
  (`ForgetTenantKeys` on every host before the key is gone) is needed no more for this; telling
  hosts still drops at once what they hold of the tenant (What TASK-411.7 made).
- **Not covered:** a host with no KMS makes cells in the clear, reading no marker, as before
  (TASK-411.24). The bucket KMS reads the marker too, where a tenant's KEK object is gone
  (The bucket KMS).
- **Tests.** `extent_remote` `keys::tests::tenants_whose_kek_the_kms_forgot_are_refused_by_their_sealed_marker`
  (the bucket's requests counted, `SimBucket::served`: a PUT with a host's first KEK, none after;
  the KMS forgetting, a HEAD once on a host that never saw the marker, none on one that wrote it;
  a fork into a new cell refused), `pools_of_tenants_whose_kek_the_kms_forgot_go`,
  `tenants_never_sealed_have_their_marker_read_once_a_while` (paused clock: one HEAD and one wrap
  per `KEKLESS_FOR` over ten openings); `extent_store`
  `upload::encrypted::tenants_whose_kek_the_kms_forgot_get_no_cell_in_the_clear` (openings of a new
  cell and of a new volume of its sealed cell refused, a fork into a new cell refused, nothing
  made; a tenant never sealed in the clear, no marker; a collector sweep of every cell leaves the
  marker); `cladefs` `remote::tests::tenants_whose_kek_the_kms_forgot_open_no_store_in_the_clear`
  (`EACCES`, no cell made; a tenant without a marker in the clear). The KMS:
  `extent_keys::testing::Forgetting`, answering `NoSuchTenant` where its inner KMS says
  `Destroyed`, as AWS and OpenBao do once a key is deleted. A hand mutant reading no marker failed
  the first three. The control API's attaching calls the same `new_cell_kek`, untested here (its
  tests need Linux's FUSE).

## The bucket KMS (TASK-411.27)

*(decided 2026-11-15)* When no `CLADEFS_KMS` names a KMS, a host given a **master key** keeps every
tenant's KEK in the bucket itself, wrapped by the master key, made on the tenant's first use: no
setup per tenant, every tenant sealed. TASK-411.28 made it the default (a host with neither
refuses to start) and removed plaintext (One sealed mode).

- **What it is not.** BYOK (Goal 1): the tenant holds no key of its own; one wanting that brings a
  real KMS. Goal 3 holds while the master key stays out of the bucket: a reader of the bucket finds
  every tenant's KEK there, wrapped by a key it lacks. Goal 2, shredding, holds as below.
- **Choosing it** (`extent_bucket::bucket_kms::from_env`, the one function every process that
  opens cells asks: cladefsd, cladefsctl and the cladefs CLI's FUSE mounts through
  `cladefs::bucket::kms`, and `cladefs-gc`). `CLADEFS_KMS`, if it names one, is the host's KMS (a master
  key also given is unused, and warned of); else, given master keys, the bucket KMS over the
  host's bucket: `CLADEFSD_REMOTE`'s for cladefs's processes, the bucket collected for
  `cladefs-gc`; else none. Master keys given that can't be had (malformed, both variables set, an
  unreadable file, no `CLADEFSD_REMOTE`) make the host's KMS refuse every call, naming why, as
  a bad `CLADEFS_KMS` does (cladefs-gc stops): no cell is made in the clear for want of it. A process
  keeping cells in other buckets (the cladefs CLI's own file buckets) keeps their tenants' KEKs in
  the host's bucket all the same: a cell's KEK, wrapped, is in its cell's object wherever that is.
- **Master keys** (`extent_keys::kms::master`). `CLADEFS_MASTER_KEY`: one or more keys, each 64
  hexadecimal digits (32 bytes), separated by commas or white space (one a line), the **newest
  first**: it wraps new KEK objects, any unwraps. Or `CLADEFS_MASTER_KEY_FILE`, naming a file of
  exactly 32 raw bytes (one key), or of text as the variable holds; both set is refused. A key
  given twice, none, or one not of 64 hex digits is refused, saying which by its place, never what
  it holds. Each key is known by its **ID**, the first 8 bytes of SHA-256 over `cladefs master key
  id\0` and the key, in hex (`2bb56b8197c604e4` for the bytes 0 to 31): all that is ever written,
  logged or said of it. What was read of the variable or the file is zeroed once parsed (the
  process's environment keeps its own copy); keys in memory are zeroed when dropped. Make one with
  `openssl rand -hex 32`.
- **The KEK object**, `tenants/<tenant>/kek` (`layout::kek`, doc-7 Layout): `fxkt`, format 1, kind
  1, the tenant (u64, big-endian), the KEK's ID (8 random bytes), the master key's ID, then the KEK
  (32 random bytes) wrapped with AES-KW under `HKDF-SHA256(master key, info = "cladefs bucket kms
  kek" || all of that header)`, 70 bytes: an object copied to another tenant's key, or relabelled
  another master key or KEK, fails AES-KW. A **tombstone**, a shredded tenant's: `fxkt`, format
  1, kind 2, the tenant, 14 bytes. Under the tenant's prefix, no cell's: no pass lists it, and the
  collector keeps it (`Kind::Kek`) if one did (`extent_remote` `testing::uncollected`, the fork and
  restore fault tests' tables).
- **Keys it wraps** (cell KEKs, in cells' objects): `fxkb`, format 2 (TASK-633.6), the tenant, the
  identity of the bucket its KEK object is in (`identity` object's ID, 0 for a bucket of none), the
  KEK's ID, then the key wrapped with AES-KW under `HKDF-SHA256(tenant KEK, info = "cladefs bucket
  kms wrap" || that header)`, 69 bytes, as `LocalKms`'s (`fxkw`) are: the header names whose KEK,
  where, and which, and is bound to the key. A key of another bucket's asked of this one's is
  refused as `Elsewhere`, never as destroyed (a bucket KMS asked by mistake says nothing of the
  tenant's KEK there), so no revocation follows.
- **First use.** A wrap for a tenant whose KEK the host holds not reads its object: there, it
  unwraps it under whichever master key its ID names. None there: it reads the tenant's sealed
  marker (TASK-411.19), and if it is there refuses the tenant as destroyed (its KEK object was
  deleted outright: no KEK made anew under which its cells would be unreadable); else it makes a
  KEK and creates its object, **create-only**: hosts racing each find none and create, and the
  creates but one find an object there, which they read and take, dropping their own KEK unused
  (`bucket::create`). One KEK object, every host's wraps under it. A line is logged naming the
  object and the master key's ID. An unwrap never makes one: a key naming a tenant's KEK whose
  object is not there is of a KEK gone (`Destroyed`).
- **What a host keeps** (`BucketKms`): each tenant's KEK unwrapped, zeroed on drop, read again from
  its object once `FRESH_FOR` (60 s) old, so that the bucket is asked once a tenant a minute at
  most, however many cells' KEKs the host wraps and unwraps, and a tenant shredded elsewhere is
  refused within a minute by a host told nothing. `Kms::forget` (a method every KMS has now, doing
  nothing but here) drops it at once, which `keys::CachingKms::revoked` calls: every `ForgetTenantKeys`,
  `ShredTenant`, and refusal heard.
- **Refusals**, each naming the tenant, so `Error::Kms`, `EACCES`:

  | Found | `kms::Error` |
  |---|---|
  | a tombstone | `Destroyed` |
  | no object, unwrapping; or a tombstone, wrapping | `Destroyed` |
  | an object under a master key not given | `UnknownMasterKey`: `tenant T: its KEK is wrapped by master key ID, which this host has not (it has IDS): CLADEFS_MASTER_KEY` |
  | an object or a wrapped key not authentic (another tenant's, tampered, another KEK of the tenant's) | `Unauthentic` |

  Never `NoSuchTenant`: every tenant asked for gets a KEK, so every cell made is sealed. A wrong
  master key is never read as a tenant without a KEK. *(TASK-411.28: a tenant's KEK object
  deleted outright, no tombstone, is made anew at the next wrap, as the marker no longer tells;
  what the old KEK wrapped is lost, as shredding loses it.)*
- **Rotating the master key.** (1) Give every host `CLADEFS_MASTER_KEY=NEW,OLD` and restart its
  processes: from then on they unwrap under either and make new KEK objects under NEW (a host
  still given OLD alone refuses those tenants, naming NEW's ID: update every host first). (2) Run
  `cladefs-gc URL --rotate-master-key` with the same variable: every tenant's KEK object under OLD is
  rewrapped under NEW by a conditional update (read again and done again if another changed it),
  the KEK and its ID unchanged, so every cell KEK it wrapped stays valid; it says how many it
  rewrapped, how many were under NEW already, how many tenants are shredded, and leaves, saying so
  and failing, any under none of the keys given. Run again, it rewraps nothing. Refusing none, it
  records NEW's ID in the bucket's entry of the registry of buckets (`master_key_id`, if the entry
  records one), the root's registry first, then every bucket's copy, failing, and saying which,
  for a copy out of reach: run it again until none fails (board 8483). Hosts for which the bucket
  is not their own choose its bucket KMS by that entry, refusing it unless they hold the key it
  names (`CLADEFSD_BUCKET_<N>_CLADEFS_MASTER_KEY`), and read it from their own bucket's copy:
  left naming OLD, step (3) would lock them out of the bucket's cells at their next restart. URL
  names the bucket rotated, the root or any other, its other buckets reached as its hosts reach
  them (`CLADEFSD_BUCKET_<N>_*`). (3) Give hosts NEW alone, the bucket's own
  (`CLADEFS_MASTER_KEY`) and those of other buckets (`CLADEFSD_BUCKET_<N>_CLADEFS_MASTER_KEY`).
  With versioning on, OLD still opens the objects' non-current versions until they expire.
- **Shredding a tenant** (`ShredTenant`, `cladefsctl shred-tenant TENANT`, through each host's
  daemon; `cladefs::bucket::shred_tenant`; `bucket_kms::destroy`, which needs no master key): the
  tenant's KEK object is replaced by a tombstone, by a conditional update (or created one, a tenant
  never given a KEK: it is never made one), then the host forgets the tenant's keys as
  `ForgetTenantKeys` does (What TASK-411.7 made). Call it on every host: the first shreds, the
  others find it shredded (`shredded_now: false`) and forget; a host not told refuses the tenant
  once its KEK is a minute old. Every cell KEK of the tenant, in every version of every cell
  object, unwraps no more, and no KEK is made for it again. A daemon whose KMS is `CLADEFS_KMS`'s
  refuses it (`FAILED_PRECONDITION`, `ENOTSUP`): destroy the KEK in that KMS, then
  `ForgetTenantKeys`. **Only as strong as the bucket keeps no older version of
  `tenants/<tenant>/kek`**: an older version holds the KEK, wrapped by a master key the hosts have.
  With versioning on, a lifecycle rule must expire non-current versions, as for a cell shredded
  (Shredding: S3 `NoncurrentVersionExpiration`; GCS versioning off or a lifecycle delete of
  non-current versions). Rotating the master key afterwards, and destroying the old one, also ends
  what such versions hold.
- **Tests.** `extent_keys` `kms::master::tests` (IDs pinned against Python's hashlib, syntax,
  refusals by place, files raw and text, the environment) and `kms::bucket::tests` (objects sealed
  and opened, other master keys named, tampering and moving refused, tombstones, resealing, wrapped
  keys); `extent_remote` `bucket_kms::tests`: made on first use (the requests counted, none more
  while fresh), two hosts racing in rounds of `SimBucket` (a GET, a HEAD and a create each; the
  second create reads the first's) and eight on threads (one object, each unwrapping all),
  rotation (old key until rotated, the new alone after, idempotent, unopenable objects left
  named), shredding (refused at once on the shredder and a host reading afresh, on a host told,
  never made again; a tenant shredded never given one), objects deleted outright with and without
  the marker, wrong master keys named, no master key in any object, refusal or log line (a logger
  of the test's), and the choice of KMS from the environment; `extent_remote` `tests/gc.rs`
  `rotates_master_keys_rewrapping_every_tenants_kek` (the binary); `extent_store`
  `upload::encrypted::sealed_stores_open_through_the_bucket_kms_on_two_hosts` (a sealed cell made
  on one host, taken and read on another, no object holding the master key, the cell's KEK or the
  data, a collector sweep leaving the KEK object and its tombstone, no host opening it shredded);
  `cladefs` `remote::tests::hosts_given_a_master_key_alone_seal_every_store_under_the_bucket_kms`
  (`CLADEFS_MASTER_KEY` and `CLADEFSD_REMOTE` alone: every cell sealed; `shred_tenant` fences
  and refuses, another tenant reading on) and `hosts_of_another_kms_shred_no_tenant`; Linux:
  `tests/control.rs` `tenants_sealed_under_the_bucket_kms_and_shredded_through_the_control_api`.
  Hand mutants (no marker read, a racing create keeping its own KEK, no tombstone written) each
  failed two or three of them.

## Real KMSes: AWS KMS

*(TASK-411.17)* `CLADEFS_KMS=aws-kms://REGION?key=TEMPLATE[&keys=FILE][&endpoint=URL]`
(`AwsKms`, `extent_keys/src/kms/aws.rs`), the query percent-encoded as any URL's; any other query name is
refused, so that a misspelt `endpoint` does not send calls to AWS itself.

- **Naming.** Each tenant's KEK is a symmetric KMS key (`SYMMETRIC_DEFAULT`, `ENCRYPT_DECRYPT`)
  named as `kms::KeyNames` says: the template `key` with `{tenant}` in base 32 (TASK-504), say
  `alias/cladefs-tenant-{tenant}`, an alias per tenant the operator makes (`aws kms create-key`,
  `aws kms create-alias`); and the overrides file `keys`, lines `TENANT NAME` (the tenant in base 32), for tenants that
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
  `{"cladefs-tenant": TENANT_HEX}`: the ciphertext decrypts for that tenant alone even where two
  tenants share a key, and a key policy can let cladefs use a key for one tenant only. The wrapped
  key is a header, `fxka`, format 1, the key name's length (2 bytes, big-endian) and the name, then
  KMS's ciphertext blob. `LocalKms`'s begin `fxkw`.
- **Unwrap.** The header is checked first, asking the KMS nothing: another backend's (`fxkw`, or
  any not `fxka`), another format, or a key name other than the tenant's (another tenant's key, or
  the tenant's under a name since changed, TASK-411.20) is `Unauthentic`. Then KMS `Decrypt` with
  `KeyId` the key's name, so that a ciphertext of another key, even with the tenant's context
  (whose key's owner could make one), is refused, and with the same context; a plaintext not of
  32 bytes is `Unauthentic`. The blob names the key version that encrypted it, so a rotated key
  (automatic rotation, or `RotateKeyOnDemand`) unwraps what every version of it wrapped, and
  cladefs does nothing on a rotation.
- **Refusals**, each naming the tenant (`kms::Error`):

  | AWS KMS's error | `kms::Error` |
  |---|---|
  | `NotFoundException` | `NoSuchTenant` (refused as destroyed, TASK-411.28) |
  | `AccessDeniedException` (the grant or policy taken away), `DisabledException` | `Revoked` |
  | `KMSInvalidStateException`, the key pending deletion | `Destroyed` |
  | `KMSInvalidStateException`, any other state (pending import: imported key material deleted; unavailable: a custom key store disconnected; creating, updating) | `Revoked` |
  | `InvalidCiphertextException`, `IncorrectKeyException` | `Unauthentic` |
  | anything else: throttling, `KeyUnavailableException`, `KMSInternalException`, `DependencyTimeoutException`, `InvalidKeyUsageException`, 5xx, the network, a timeout, no credentials | `Io` |

  AWS says which invalid state only in its message ("`<key ARN>` is pending deletion."), which the
  mapping reads. A tenant shreds itself by scheduling its key's deletion: `Destroyed` at once, for
  the waiting period's 7 to 30 days; cancelling it leaves the key disabled (`Revoked`) until
  enabled. Once the period is over AWS has forgotten the key and answers `NotFoundException`, as
  for a key never made: its unwraps fail, and its new cells are refused as destroyed, as every
  `NoSuchTenant` is (TASK-411.28). A
  tenant
  with imported key material that deletes it reads as `Revoked` (it may import the same material
  again), never `Destroyed`.
- **IAM.** cladefs's role needs `kms:Encrypt` and `kms:Decrypt` on each tenant's key, no more (no
  `kms:DescribeKey`, no `kms:GenerateDataKey`); a condition on the encryption context ties each key
  to its tenant:

  ```json
  {
    "Effect": "Allow",
    "Action": ["kms:Encrypt", "kms:Decrypt"],
    "Resource": "arn:aws:kms:REGION:ACCOUNT:key/KEY-ID",
    "Condition": {"StringEquals": {"kms:EncryptionContext:cladefs-tenant": "2a"}}
  }
  ```

  Cross-account BYOK: the tenant's key policy allows cladefs's role (its ARN as principal) the same
  two actions with the same condition, cladefs's role's own IAM policy allows them on the key's ARN
  (cross-account use needs both), and the overrides file names the key by its ARN. The tenant
  revokes by disabling the key or removing that statement (`Revoked`), and shreds by scheduling the
  key's deletion (`Destroyed`).
- **Calls.** On a runtime of the backend's own (one thread, `cladefs-aws-kms`), its HTTPS client
  (hyper 1 over rustls 0.23, the build's, not the SDK's legacy default) made there when first
  needed: its futures run on any executor, and its connections never outlive a caller's runtime.
  The SDK retries what AWS says is retryable (throttling, 5xx, a lost connection), three attempts
  backing off; an attempt times out after 10 s, a call after 30 s, so a KMS that answers not fails a
  cell's opening as `Io` rather than holding it.
- **Memory.** The key cladefs hands `Encrypt`, and `Decrypt`'s answer, are zeroed where cladefs holds
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
  cell made, written and opened by another host through `aws-kms://...&endpoint=...`, refused
  revoked and destroyed.
- **Where the emulator differs from AWS** (behaviour verified against it, not against AWS, as
  no AWS account was used): it has no IAM, so `AccessDeniedException` is never answered but for a
  ciphertext naming no key it has, and is tested on the SDK's error alone, as throttling is; it
  rotates no key on demand, nor before a year, so the rotation test seeds two emulators with the
  same key ID, the second with a second backing key, as its seeding has rotations; it answers a
  ciphertext of another key with `InvalidCiphertextException` where AWS answers
  `IncorrectKeyException` (both `Unauthentic`); it deletes no key, pending deletion for ever, so
  `NotFoundException` after deletion is untested against it (what it leads to, `NoSuchTenant` for a
  tenant that had a KEK, is tested with `extent_keys::testing::Forgetting`, TASK-411.19); its messages are its own, the pending deletion
  one as AWS's.

## Real KMSes: OpenBao

TASK-411.16: OpenBao's Transit engine (openbao.org, MPL-2.0; Vault is BSL, excluded) as a KMS,
`extent_keys::kms::openbao::OpenBao`. Self-hosted, so the one real KMS tested anywhere: its tests
start `bao server -dev` themselves.

- **Configuring.** `CLADEFS_KMS=openbao://HOST[:PORT]/MOUNT?key=TEMPLATE[&keys=FILE][&namespace=NS]`
  (port 8200 by default; `MOUNT` the Transit engine's path, which may hold `/`; `NS` its namespace,
  sent as `X-Vault-Namespace`), or `openbaos://` over HTTPS, which may add `&ca=FILE`, a PEM file of
  the only CAs trusted (else the system's). `openbao://` sends the token in the clear: loopback or a
  trusted network only. Each tenant's key is named by `kms::KeyNames`: `TEMPLATE` with `{tenant}` in
  base 32, `FILE`'s overrides for tenants bringing their own; names of letters, digits, `-`, `_`
  and `.`. The token is `CLADEFS_KMS_OPENBAO_TOKEN`'s, or that of the file `CLADEFS_KMS_OPENBAO_TOKEN_FILE`
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
  | 400 `disallowed by policy (too old)`, version < `min_decryption_version` but ≥ `min_available_version` (raised, reversible) | | `VersionDisabled`, that version's alone (board 8479) |
  | the same, version < `min_available_version` (trimmed, gone) | | `VersionGone`, that version's alone (board 8479) |
  | 400 `disallowed by policy`, the version decryptable | | `Revoked` |
  | 400 `message authentication failed`, `invalid ciphertext` | | `Unauthentic` |
  | 400 `version is too new` | | `Unauthentic` |
  | 403 and `lookup-self` 403 (cladefs's token invalid, expired, revoked) | `Io` | `Io` |
  | 404 route not found (no such mount, a token with rights to it), other 4xx, 412/429/5xx (sealed: 503) after 3 tries, network | `Io` | `Io` |

  Why the probing: OpenBao answers 403 both for a token without rights to a key and for an encrypt
  of a missing key with a token that may not create one; the key's metadata says which, and
  `lookup-self` tells a tenant's revocation from cladefs's own token failing, so that an expired host
  token is not read as every tenant revoked (which would drop their keys from every host's memory).
  Nothing is `NoSuchTenant` but a key OpenBao says it has not (refused as destroyed since
  TASK-411.28, as it once made cells in the clear for a tenant never sealed).
  A mount the token has no rights to, there or not, reads as revoked (OpenBao checks the token
  first). A deleted key is as missing as one never made: unwraps say `Destroyed`, wraps
  `NoSuchTenant`, which refuses its new cells as destroyed (TASK-411.28): deleting the key shreds
  the tenant, as
  soft-deleting it or trimming its versions refuses it.
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
  through the environment, a `derived` key) and `extent_store` `upload::encrypted::sealed_cells_open_through_openbao`
  (a sealed cell made, taken with one unwrap, refused when the policy drops its key, opened again,
  a tenant without a key refused, TASK-411.28). Each starts `bao server -dev` on a free port
  (`extent_keys::testing::openbao::DevServer`, `-dev-no-store-token`), and is skipped, saying so,
  without `bao` (`CLADEFS_BAO`, else `PATH`), unless `CLADEFS_REQUIRE_OPENBAO` is set: then it fails.
  `scripts/test-openbao.sh` fetches OpenBao's release (2.7.2, by digest, `scripts/lib/openbao.sh`)
  and runs them so required; CI's `object-stores` job could run it (proposed, not added).

## Real KMSes: Google Cloud KMS

TASK-411.18, `extent_keys::kms::gcp` (2026-11-15). A real backend beside `LocalKms`, over Cloud
KMS's REST API (`cloudkms.googleapis.com/v1`).

- **Configuration.** `CLADEFS_KMS=gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE]`.
  A tenant's KEK is a `cryptoKey` named as `kms::KeyNames` says: the template, `{tenant}` the tenant
  in base 32, an id in ring `R` (`cladefs-tenant-{tenant}`); or a line of the overrides file,
  `TENANT NAME`, where `NAME` is an id in `R` or a key's whole resource name,
  `projects/P2/locations/L2/keyRings/R2/cryptoKeys/K`, in a project of the tenant's own (BYOK,
  cross-project). Each name is checked as the URL is read (project, location, ids of 1 to 63
  letters, digits, `_` and `-`; unknown parameters refused), so a bad one fails the host's KMS, not
  a call.
- **Credentials.** Application Default Credentials as `object_store` finds them for cladefs's `gs://`
  buckets (`GoogleCloudStorageBuilder::from_env`, its credential provider, which caches and renews
  tokens): `GOOGLE_APPLICATION_CREDENTIALS` (a service account's key file, or a user's), gcloud's
  well-known `application_default_credentials.json`, else the metadata server (GCE, GKE workload
  identity). Not covered by `object_store`: `external_account` files (workload identity federation
  from outside GCP) and impersonation. For development only, `CLADEFS_KMS_GCP_TOKEN` holds an access
  token used as is (`gcloud auth print-access-token`, an hour's). `GOOGLE_CLOUD_QUOTA_PROJECT`, if
  set, is sent as `x-goog-user-project`. Tokens go to the KMS alone; neither they nor keys are
  logged or in any error.
- **What a tenant does.** Makes a symmetric key (purpose `ENCRYPT_DECRYPT`,
  `GOOGLE_SYMMETRIC_ENCRYPTION`, software or HSM) and grants cladefs's service account
  `roles/cloudkms.cryptoKeyEncrypterDecrypter` on that key alone; for a key in its own project,
  gives the operator the key's resource name for the overrides file. Revoking is taking the role
  away or disabling the key's versions; shredding is destroying them.
- **A wrap** is `:encrypt` of the key's 32 bytes under the tenant's key (its primary version), with
  `additionalAuthenticatedData` `cladefs-tenant:<tenant hex>`, so that what was wrapped for one
  tenant is refused to another even under one key (overrides may share one); and
  `plaintextCrc32c` and `additionalAuthenticatedDataCrc32c`, the answer checked for
  `verifiedPlaintextCrc32c`, `verifiedAdditionalAuthenticatedDataCrc32c`, its `ciphertextCrc32c`,
  and a version `NAME/cryptoKeyVersions/N` of the key asked (CRC32C by `crc-fast`). **An unwrap**
  is `:decrypt` of the ciphertext with the same associated data and `ciphertextCrc32c`, its
  `plaintextCrc32c` checked, under the key's name: the ciphertext names its version, so any
  version the KMS keeps enabled unwraps what it wrapped. A plaintext not 32 bytes is not authentic.
- **Wrapped bytes** (`kms::real`, shared by the real backends, TASK-411.16, .17): `fxkr`, format 1,
  the backend's scheme (a length byte, `gcp-kms`), the key's resource name (two bytes big-endian of
  length, then the name), then the KMS's ciphertext (113 bytes for a 32-byte key, software
  protection). An unwrap refuses bytes of another backend (`LocalKms`'s `fxkw` among them), or
  naming another key than the tenant's, as `Unauthentic`, asking the KMS nothing. A tenant whose
  key's name changes (a moved override) has its old wraps refused so: rewrapping is not built.
- **Refusals, as Cloud KMS answered them** (2026-11-15; the unit tests `kms::gcp::tests` hold the
  bodies, and every pair of a 429 or 5xx and a status):
  - `NOT_FOUND` "CryptoKey NAME not found." (the key missing; a missing key ring answers the same)
    → `NoSuchTenant`: the tenant's new cells are refused as destroyed (TASK-411.28; made in the
    clear before). Any other `NOT_FOUND` ("Project
    'projects/X' not found.", a location Cloud KMS has not) → `Io`, so that a wrong project in an
    override fails closed. A misnamed key ring in the URL reads as every template tenant keyless,
    their new cells refused (in the clear before TASK-411.28; TASK-411.22, moot): check the URL.
  - `PERMISSION_DENIED` (the role taken away, or a project cladefs may not see: Cloud KMS says "or it
    may not exist"), its `ErrorInfo` reason `IAM_PERMISSION_DENIED` or none → `Revoked`. Of any
    other reason (`SERVICE_DISABLED`, `BILLING_DISABLED`, `SECURITY_POLICY_VIOLATED` of VPC Service
    Controls, `USER_PROJECT_DENIED` of the quota project, `ACCESS_TOKEN_SCOPE_INSUFFICIENT`): the
    host's or its project's configuration, not the tenant's key → `Io`, not retried, naming the
    reason, so that a configuration change never reads as every tenant revoking at once (board
    8481; those reasons' bodies are Google's documented shapes, not yet recorded from the service).
  - `FAILED_PRECONDITION` "NAME/cryptoKeyVersions/N is not enabled, current state is: S.", its
    `PreconditionFailure` violation `KEY_S` (seen: `KEY_DISABLED`, `KEY_DESTROY_SCHEDULED`; the
    message read if no violation says). For a wrap, its primary, so the tenant's KEK: `DISABLED` →
    `Revoked`; `DESTROY_SCHEDULED`, `DESTROYED` → `Destroyed`. For an unwrap, the version that
    wrapped the key alone, named, never the tenant's KEK, whose other versions and cells live
    (board 8479): `DISABLED` → `VersionDisabled`; `DESTROY_SCHEDULED`, `DESTROYED` →
    `VersionGone`. (A version reaches `DESTROYED` a day after scheduling at the soonest: not seen
    yet, mapped by the same rule.)
  - `INVALID_ARGUMENT` of a decrypt ("Decryption failed.", wrong associated data; "the ciphertext
    is invalid"; `INDECIPHERABLE_CIPHERTEXT_FOR_CRYPTOKEY`) → `Unauthentic`; but `INVALID_CHECKSUM`
    (a CRC32C failed in transit) → `Io`, retried.
  - `UNAUTHENTICATED` (cladefs's own credentials), `UNAVAILABLE`, `INTERNAL`, `DEADLINE_EXCEEDED`,
    `RESOURCE_EXHAUSTED`, every 5xx and 429 whatever its body, network failures and timeouts (30 s a
    call, 10 s to connect) → `Io`, naming the call and the key; all but `UNAUTHENTICATED` retried,
    with checksums failed in transit, four times, after waits of up to 200 ms, 1 s, 4 s and 10 s,
    each drawn within its upper half, or the `Retry-After` the service sends (up to 30 s), plus
    jitter (`kms::retry`, board 8473): the quota's refusal (429 `RESOURCE_EXHAUSTED`) was not
    retried at all before, a burst of opens failing at its first.
- **Changes of state take effect eventually**: a version disabled, enabled, destroyed or made
  primary took from a few seconds to about two minutes to show in `:encrypt` and `:decrypt`
  (2026-11-15). A tenant revoking is not instant either.
- **Rotation.** A tenant's new primary version wraps new cell KEKs; old cells' KEKs stay wrapped
  under the version that wrapped them, which must stay enabled: destroying an old version shreds
  every cell whose KEK it wrapped (TASK-411.21).
- **Client.** reqwest over rustls (aws-lc-rs, the platform's roots), serde_json, base64,
  crc-fast, tokio's timer, url's query parsing, and `object_store`'s GCP credentials: each in the
  build already (MIT OR Apache-2.0, tokio MIT), no new crate. Google's own `google-cloud-kms-v1`
  and `google-cloud-auth` (Apache-2.0) would add their gax stack for two calls, and a second
  credential story beside the buckets'.
- **Tests.** Against real Cloud KMS, project `echophase-connectome-test`, key ring
  `projects/echophase-connectome-test/locations/us-central1/keyRings/cladefs-test` (or
  `CLADEFS_KMS_GCP_TEST_RING`), keys `cladefs-test-1` to `-5` and `cladefs-probe`, made if missing, a
  day's scheduled destruction (`extent_keys::testing::gcp::Admin`); skipped, saying why, unless
  `CLADEFS_KMS_GCP_TOKEN` holds a token, so CI, which holds no GCP secret, skips them:
  `CLADEFS_KMS_GCP_TOKEN=$(gcloud auth print-access-token) cargo nextest run -p extent_keys -p cladefs -E 'test(/gcp::service|google_cloud_kms/)'`.
  `extent_keys` `kms::gcp::service`: `keys_wrap_unwrap_and_bind_their_tenant` (header, other
  tenants, a tenant sharing the key refused by associated data, changed and truncated ciphertexts,
  `LocalKms`'s bytes, no key, a foreign project), `rotated_keys_unwrap_old_wraps_until_their_version_is_destroyed`,
  `disabled_versions_revoke_until_enabled`, `destroyed_primaries_destroy`; `cladefs`
  `extents::tests::sealed_stores_open_through_google_cloud_kms` (`CLADEFS_KMS=gcp-kms://...`: a
  sealed cell made, its cell KEK unwrapped by the KMS to the KEK its DEK is under, reopened,
  forked). Billing: one enabled software version per key kept (`cladefs-test-1`, `-2`, `-3`, `-5`),
  each run destroying the versions it rotates out; `cladefs-test-4` and `cladefs-probe` keep none.

## What TASK-411.15 made

- **The guest tests run sealed, only** *(decided, 2026-11-14: no clear e2e runs; plaintext stays
  covered by the host's suites)*. `scripts/test-e2e.sh` makes a `LocalKms` beside the run's
  sockets (`/tmp/cladefs-e2e-sockets.*/kms`, gone with them, never in the results, which runs keep
  and fetch), holding a KEK for tenants 0-15 (`runner kms`, `test_cases::sealed::TENANTS`), and
  exports `CLADEFS_KMS` to every process of the run: its daemon, the cases' own daemons, the runner's
  processes that make and open cells themselves (cladefs's `Storage`, `Image`, `import`), cladefsctl,
  cladefs-gc and FUSE mounts. A caller's own `CLADEFS_KMS` is kept, its KEKs the caller's to make. A
  daemon and harness both built before encryption (an A/B's old pin, through `CLADEFS_E2E_PREBUILT`
  or `CLADEFS_E2E_ROOT`) run in the clear, unchecked, saying so; one old and the other not is
  refused, as neither reads the other's cells (TASK-411.28); the runner refuses to start with no
  KMS (`sealed::require_kms`); `encryption.txt` in the results says
  which each run was. qualify.sh, sandboxes.sh, the bench and soak scripts and the nightly all
  run through test-e2e.sh, so sealed.
- **Tenant 0 has a KEK**, not the cases moved to granted tenants: the default tenant's socket,
  `cladefsctl create`, and every cell a case makes in its own process are of tenant 0
  (`host_store::NO_TENANT`); moving them would change most cases and leave the common tenant's
  path, the daemon's own cells and public bases, untested sealed. Tenant 0 is sealed like any
  other (every cell is, since TASK-411.28), and cladefs's own tests seal it. Granted IDs up to 15 cover
  fxfs-guest-migrate-tenant-dax's 1-4.
- **The check, holding no key** (`test_cases::sealed`; `extent_store::on_disk` for a cell's
  directory): every cell of the buckets read has a wrapped KEK or was shredded; every record of a
  branch that wrote objects, but points' and deleted ones', has a DEK; every framed object
  (segments, own extents, checkpoints, moves) is sealed or authenticated by its trailer; no
  cell's framed file is in the clear, nor log headed `FXLOG\0\0\x02`; and no object nor file
  read holds a run of the guest tests' known plaintext (`test_cases::canary`, 1 MiB of
  xorshift64*, incompressible, so that a clear cell keeps it as written in the bucket too),
  which fxfs-guest's file and fxfs-guest-checks' mapped file are made of: any run of 528 bytes or
  more is found, at any offset. With `CLADEFS_E2E_SEALED`, the runner checks each case's directory
  once it passed, every file under it, its cells, the file buckets in it whole, and the cells its
  cells are of in other buckets (the run's own); a case not sealed fails, naming what. At the
  run's end, `runner sealed` checks the run's bucket whole and its part cache. A cell's bucket in
  a daemon's memory (`memory:///`) can't be read and is named as unread.
- **Proved both ways** (ubu1, 2026-11-15): `--tag e2e`'s 16 cases pass sealed, each check
  `0 in the clear`, the run's bucket (8 cells) and part cache too; a run whose KMS held no KEK
  (tenant 0's cells in the clear, before TASK-411.28 refused such a tenant's cells) failed fxfs-guest's check with 21 findings (a cell with no KEK,
  segments, checkpoints and moves framed in the clear, clear logs, the canary in a segment) and
  the run's part cache's copy of that segment.

## What TASK-411.23 made

*(decided, the user, 2026-11-15)* **Sealed cells frame in place.** TASK-411.15 found
fxfs-guest-sandboxes failing sealed: the cells wrote 1.90 and 1.87 times what the guests wrote,
against a budget of 1.5 (1.16 in the clear), and framings took up to 1094 ms against 1000 ms,
because a sealed cell never framed in place (TASK-411.5, above): 0 segments in place against
55-58 in the clear, each segment written twice, its XTS raw file then its GCM copy. Now the
incompressible parts of a sealed object keep the raw file's XTS sectors as they are, each tagged,
and a sealed cell frames in place exactly when a clear one does.

- **Format.** `extent_frame` FORMAT 2 gains a seal kind, 3, `Seal::Sectors { key, salt }`
  (`Keyed::Sectors`), not a new FORMAT: FORMAT 2 is unreleased and no bucket needs keeping (the
  user's decision of 2026-11-14, as for TASK-411.3's extensions). An object of kind 3 is a sealed
  raw file framed in place: every part stored raw where the raw file holds it, its AES-256-XTS
  sectors as they are, under the raw file's own key, `Xts::derive(dek, salt, "cladefs raw file")`,
  the sector's place in the object its tweak, which is its place in the raw file (parts begin at
  multiples of `PART`, a multiple of `SECTOR`). Its trailer names the DEK (branch, generation) and
  the raw file's salt, so a reader derives the same key; there is no other salt. Each part's tag,
  in its table entry's 16 bytes (the entry's layout unchanged), is HMAC-SHA256 truncated to 128 bits
  (`Secret::tag`) under `dek.derive(salt, ["cladefs raw part tag"])`, of the part's associated data as
  a GCM part's (index, encoding, stored and unpacked lengths) and then its stored bytes, the
  ciphertext. Its footer and table are MACed as any keyed object's, under
  `dek.derive(salt, ["cladefs frame mac"])`; `restamp` works under its key as for a sealed object.
  The key is one object, `ObjectKey::sectors`, which the cell's `raw::Sealer` holds for the raw
  file's encryption too: one derivation for both. A table naming a part of kind 3 that is not raw,
  or not whole sectors, is corrupt (`part N, not whole sectors of a raw file`), to readers holding
  no key as well.
- **Readers.** Holding the DEK: the part's CRC, then its tag (constant time), then XTS decryption,
  in `Part::open`, `Part::unpack` and `Part::unpack_into`; `Part::as_is` is false. Every reader
  goes through those unchanged: the host's fetchers and the part cache (stored bytes kept as they
  are), the cell's framed files, `extent_frame::data`, the collector's compaction
  (`layout::data_under`). Holding none: CRCs alone (`extent_frame::check`, WAL recovery's
  `whole_footer`, the collector's marking), parts refused (`Error::NoKey`). The collector's moves of
  a kind-3 object are sealed with AES-GCM under the key its trailer names (`Seal::keyed`), as for
  any sealed object.
- **No object mixes XTS and GCM parts.** As in the clear, a segment is framed in place only when
  compressing it would save too little of the whole (`framing::IN_PLACE_SAVING`, `Look`), every
  part raw then, compressible ones too; otherwise in a copy, every part compressed if that shrinks
  it and sealed with AES-256-GCM. A large extent likewise: in place only if no part compresses.
- **Why HMAC, not GMAC.** A deterministic MAC needs no nonce. A raw file's part may be tagged more
  than once: by a look ahead, again by a framing that reads it (a part the look ahead missed, a
  framing run again), and its last part at another length across a crash, recovery closing the
  segment from its log. GMAC with the part's index as its nonce would then authenticate other bytes
  under a nonce used before, which gives away its authentication key. HMAC-SHA256 with the CPUs'
  SHA extensions (ubu1's Ryzen 9 8945HS has `sha_ni`; Apple's M-series SHA2) runs about 2 GB/s,
  paid once a part by the look ahead. Truncated to 128 bits (RFC 2104, 5; NIST SP 800-107, 5.3.3):
  a forgery succeeds with probability 2^-128 a try.
- **The composition.** Encrypt-then-MAC under independent subkeys of one DEK and salt (HKDF infos
  `cladefs raw file` + `data`/`tweak`, `cladefs raw part tag`, `cladefs frame mac`,
  `cladefs raw file log mac`), the tag checked before anything is decrypted. The tag binds a part's
  bytes, index, encoding and lengths; the salt binds it to its raw file (a part of another raw file
  of the same data and DEK fails, tested); the table's MAC binds each part's place, which is its
  sectors' tweaks, and its CRC: a part read at another place decrypts to other bytes, and a table
  saying so fails its MAC. CRC32C is not a MAC: the tag is what refuses a part changed with its CRC
  made to match (tested).
- **Each sector of a segment's raw file is written once.** XTS is deterministic for a key and
  tweak: a sector written again with other bytes under the same key shows one who sees both
  versions which 16-byte blocks changed. A segment's writes take ranges past its end alone
  (`Segments::reserve`, `Segments::append`: `segment.len += len`), never overlapping (`Ahead::ended`
  asserts each part's bytes are written once); an abandoned write's range is dead space, never
  given out again; a short write retried (`Write::stored`'s callers, TASK-411.5) writes the same
  bytes, the same ciphertext; each segment's raw file is made anew (spares are logs, never raw
  files) with a salt drawn for it (`Sealer::new`), so no two raw files share a key, a fenced
  writer's and its successor's among them; recovery never writes into a raw file (it reads it and
  frames the segment in a copy, `Recovered::close`); a framing in place writes only its tail, in
  the clear, past the data's sectors. A segment's dead space holds what was written there under its
  own key, or nothing: its object carries no other key's ciphertext.
- **Large extents' raw files may be written again.** A guest may write a block of an open large
  extent twice before it is sealed, and its sectors are then rewritten under one key and tweak:
  the exposure of full-disk encryption (dm-crypt's XTS), on the host's disk only, to one who reads
  it at two times, as since TASK-411.5. The bucket and the part cache get the sealed state alone,
  one version a salt: a raw file is framed once, and an extent opened again after its seal writes a
  new raw file with a new salt (`create_raw`). Framing in place adds no exposure; it stops writing
  a second copy.
- **Writers.** `segments::frame_segment` frames in place when the segment is quiet, holds data, its
  raw file is sealed iff the cell seals (a raw file of the other kind, say a recovered one, goes
  to a copy), and, sealed, its length is whole sectors (always, TASK-411.8); the decision is the
  clear one. What is seen of a part, by a look ahead or a framing's read, is `RawPart::of` its
  stored bytes, read through the host's page cache (CRC and tag), then a look at them decrypted
  (`framing::read_seen`); in place, `Packer::in_place(raw.sectors())` takes those parts
  (`push_raw_parts`) and the tail is written past the data, in the clear, and linked, as before.
  `Store::seal_own` (large extents): `in_place_parts` reads each chunk's stored bytes (CRC, tag),
  decrypts it, and compresses it to tell (`extent_frame::stores_raw`); all raw, the raw file is
  the object, its tail appended, renamed; else `write_own` frames it in a copy. (A clear large
  extent whose parts compress only after some that don't was framed by copying its raw prefix; it
  is now framed by `write_own` whole, its prefix read and compressed again: rare, and noise
  compresses fast.)
- **TASK-421, settled here.** A sealed segment is looked at ahead as a clear one is
  (`Segment::new` gives every segment an `Ahead`; `Store::look` lost its assertion): the stored
  bytes' CRC and tag, and a look at their decryption, so that a framing in place reads back only
  the parts the look ahead missed. Parts packed ahead are packed in the clear and held in memory
  (`HeldParts`), which holds plaintext as the extent cache does (Goals: keeping plaintext from the
  host is not one); a framing in a copy seals each under its object's key, drawn as it begins
  (`PackedPart::sealed`, byte for byte what `PartPacker::pack` makes under that key, tested), so no
  object key is drawn before its framing, none outlives it, and a part's nonce, its index, is used
  once under it (a framing run again draws a new salt). Held parts never leave memory; one let go
  of is read back through `RawFile`, decrypting, as before. So TASK-421's three criteria hold
  (`sealed_stores_hold_parts_packed_ahead_and_seal_them_framing`,
  `sealed_raw_files_parts_are_seen_as_stored_and_held_as_data`), its task closed by this one.
- **Tests.** `extent_keys` `tags_are_truncated_macs_of_their_pieces` (pinned against Python's
  hmac); `extent_frame` `objects_of_raw_files_sectors_read_under_their_key_alone` (the raw file's
  sectors byte for byte, no plaintext, whole, by parts, by ranges, restamped, keyless check, no
  key, another DEK), `parts_of_raw_files_sectors_changed_or_moved_fail` (a byte with its CRC made
  to match, a tag, another index, another place, another salt, a table of compressed or short
  parts), `parts_packed_in_the_clear_seal_as_parts_packed_sealed`,
  `objects_framed_in_place_under_aes_gcm_keys_are_refused`; `extent_store` `segments::tests`
  `sealed_segments_of_noise_are_framed_in_place` (seen ahead, the last part alone read, its raw
  file its framed file, its tail alone written, `Seal::Sectors` of the raw file's salt, read with
  the key, not without, no plaintext) and `sealed_segments_parts_are_held_ahead_and_sealed_framing`;
  `framing::tests::sealed_raw_files_parts_are_seen_as_stored_and_held_as_data`;
  `upload::encrypted::sealed_stores_frame_in_place_writing_what_clear_stores_write` (below),
  `segments_framed_in_place_sealed_recover_from_crashes` (the TASK-411.9 pattern: crashed as the
  framing in place is made durable, four crash seeds and an opening failed after each of its
  operations in turn, never a byte in the clear, the segment closed again in a copy, read back;
  crashed once durable, read from the object of its sectors),
  `sealed_stores_hold_parts_packed_ahead_and_seal_them_framing`; the tests that asserted sealed
  segments of noise were `Seal::Sealed` now assert `Seal::Sectors` (`segment_key`).
- **Written, sealed against clear, Mac.**
  `sealed_stores_frame_in_place_writing_what_clear_stores_write`: a segment of 6 MiB of noise and
  512 KiB of text (its framing compresses it all to tell, then frames it in place), a large extent
  of noise (in place) and one of noise then text (in a copy): the sealed cell's raw, copied, framed
  and own bytes equal its clear twin's, byte for byte; only the logs differ (their MACs).
  `cell-bench framing` prints the bytes written and the segments framed in place.
- **fxfs-guest-sandboxes, ubu1** (2026-11-15, 02302b95, `scripts/sandboxes.sh short`, kernel
  `release-c87bcefc`, under fxjob): sealed (test-e2e.sh's KMS, tenants 0-15 with KEKs), the case
  passes: the cells wrote **1.21 times** what the guests did (host-a 1.22, host-b 1.21; budget
  1.5; was 1.90 and 1.87), 66 and 63 segments framed in place (was 0), framings within their
  second, the slowest 111.0 and 129.2 ms (was up to 1093.9 ms), none over budget; framings read
  0.13-0.14 GiB back, the looks ahead 5.4 GiB a host. Its clear twin, the same build with a KMS
  holding no KEK: 1.17 (1.17, 1.18), 58 and 59 in place, the slowest framings 122.9 and 231.6 ms.
  Per guest byte, sealed: raw 1.0, copied 0.14-0.15, framed 0.06-0.07, own 0, log 0.007. The run's
  end check over the bucket found three tenant-0 cells of the run's first three branches, not the
  case's daemons', made with no KEK (TASK-411.26), so `sandboxes.sh` exits 1 though the case passes.
