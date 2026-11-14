---
id: doc-16
title: 'Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs'
type: specification
created_date: '2026-11-14 18:32'
updated_date: '2026-11-14 18:33'
---
# Per-tenant encryption at rest: tenant KEKs, cell KEKs, volume DEKs

Design, 2026-11-14. Nothing implemented. The user's decisions of that day are marked *(decided)*.

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
  first, as now, then sealed with AES-256-GCM under the object's subkey; the nonce is the part's
  index, the associated data the object's key, the part's index, encoding and length. The table's
  entry holds the tag. Compression keeps working; ciphertext is the same length plus 16 bytes.
- **Footers, tables, trailers, checkpoints, moves' maps, records, cell objects, leases:** in the
  clear, as now, plus a MAC under a key derived from the DEK, which key-holding readers check. The
  collector reads them without keys, checking CRCs, as now. They show extent IDs, sizes, the fork
  tree and live bytes; Fxfs's own metadata (names, inodes, directories) is data inside extents, so
  encrypted. A bucket writer could tamper with what the collector reads; it can delete objects
  anyway, so that is no new power.
- **The trailer** gains the salt, the branch and DEK generation, and a flag: FORMAT 2 of
  `extent_frame` (doc-11 rule 1: a FORMAT 1 reader names FORMAT 2, not corrupt).
- **Local disk:** closed segments in the WAL and the part cache hold the stored bytes, so they are
  encrypted with nothing more. *(decided)* **The open segment is encrypted too**, under the volume
  DEK, so that shredding covers hosts' disks without counting on their disk encryption. Its pieces
  are read back by offset until the segment is framed, so it needs a length-preserving, random-access
  cipher: AES-256-XTS per 4 KiB sector (the workspace has `storage_xts`), tweak the sector, key
  derived from the DEK and the segment's salt (`wal::log_salt` exists already). Writes that are not
  whole sectors need a read-modify-write, or a check that none come; the implementer finds which.
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

The cost: a fork into a new cell makes a cell KEK and wraps it with the KMS, one call on the fork's
path, which doc-7 keeps O(1) in the bucket. Measured before deciding whether it needs anything else.

## Shredding

- **A tenant:** destroy its KEK in the KMS. Absolute: every cell KEK of it, in every version of
  every cell object, unwraps no more.
- **A cell:** remove its wrapped KEK from its object (a control API call, conditional as every cell
  update). Only as strong as the bucket keeps no older version of `<C>cell`: with versioning on, a
  lifecycle rule must expire non-current versions, or the old wrapped KEK stays unwrappable while
  the tenant's KEK lives.
- **A volume:** deleting it, as now; its record stays (records stay, doc-7) and could have its
  wrapped DEKs cleared once the collector has removed the branch's objects.

## Open questions

1. Unencrypted stores: does FORMAT 2 keep a plaintext mode (tenant 0, tests, existing buckets), and
   how do FORMAT 1 buckets move over (doc-11)?
2. The KMS call on a cross-cell fork's path: acceptable as measured, or a per-host cache of
   unwrapped cell KEKs with a TTL bounding revocation?
3. Sectors: does any write reach the open segment that is not whole 4 KiB sectors?
4. Which AEAD crate: `aws-lc-rs` is already in the lock file (Apache-2.0 and ISC); RustCrypto's
   `aes-gcm` (MIT or Apache-2.0) is the other. Licenses checked before adding either.
5. Real KMS backends. *(decided)* A trait and a local fake first; providers later.
