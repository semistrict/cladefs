---
id: doc-28
title: 'Frozen formats: every later build reads every frozen version'
type: specification
created_date: '2026-11-23 12:00'
updated_date: '2026-11-23 12:00'
---
# Frozen formats: every later build reads every frozen version

Design, 2026-11-23 (the user: "as this goes into production, we really need to finalize these
formats ... so that we can no longer make backwards incompatible changes"). It ends AGENTS.md's
"No migrations, ever" (2026-11-05) for every format it freezes, and replaces doc-11's rules 1, 2
and 4 for them. The work is board 10033's subtasks. Decisions taken without the user are hotbed
questions 494–498 (project cladefs).

## What changes

Until now a reader refused every version but its own, and a format changed outright: cells and
buckets of the old format were made again, never read. That was right while nothing held data
anyone kept. In production a bucket holds tenants' only copy of their disks, an ephemeral host
the only copy of its cells, a replica directory acked commits no bucket has yet. None of it can
be made again.

So, once a format is **frozen**:

1. **Every later build reads every version of it from its frozen one on**, `oldest..=current`,
   and refuses any other by name (newer ones too: a rollback past a change is still refused).
2. **A change is a new version, never an edit of an old one.** The old version's decoder, and the
   types it decodes into, stay as they are for good; the new reader converts what it reads into
   the current types (Fxfs's `versioned_type!` model: `From<V27> for V28`). Or the change goes
   through an extension point the format already has (an unknown key kind the collector keeps, a
   protobuf field), which needs no version.
3. **Nothing frozen is rewritten to convert it.** Old versions are read as they are, for as long
   as they exist. A writer writes the current version only.
4. **What proves it: a golden corpus.** Each frozen format keeps, in the repo, objects of every
   version it reads, written by the build that froze or changed it, and what each decodes to.
   Those files are append-only (`scripts/format-freeze.py`, in CI and the commit-msg hook): a
   change that stops reading one fails its test, and deleting or editing one fails the push.

What isn't frozen keeps the old rule: refused unless it is this build's.

## Which formats are frozen

| Class | Formats | Frozen? |
|---|---|---|
| Only copy, in the bucket | cell heads, collector leases, the bucket's identity and registry, references, retired marks, mirrors, pins, holds, the ephemeral index and tombstones (keys), checkpoints, segment and moves footers, marks, objectid blocks, read evidence; extent frames; KMS objects and wrapped keys of every backend; the key grammar | Yes |
| Only copy, on a host's disk | cell directories (`store`, segment logs, `wal`, local checkpoints, ephemeral keys, migration markers, attached bases and ancestry); replica directories; ephemeral host directories; block devices' roots and nodes | Yes |
| Only copy, inside a cell | Fxfs (`LATEST_VERSION`), block devices | Yes |
| Long-lived files | Firecracker snapshots' extent device state (zygotes outlive builds) | Yes |
| Wire across builds | the peer service (a rolling upgrade's hosts), the control and mount APIs (a VM manager upgraded apart), the virtio device (a guest restored onto a newer VMM) | Grows only, checked (`buf breaking` in CI, `go/compat.sh`; versioned state) |
| Wire within a build | VM process and daemon (`wire::VERSION`): installed together, and a daemon's restart ends its VMs | No: exact, as now |
| Caches and diagnostics | the part cache, read traces, stats | No: refused and rebuilt |

## One version per family, read by range

A family is the formats one writer writes together: the bucket's objects (`bucket_cell::layout`),
a cell directory's files (`local_cell::layout`), a replica directory, an ephemeral host directory,
Fxfs, a block device, extent frames, each KMS backend's. Each family has one version number, as
Fxfs has `LATEST_VERSION`: every object it writes says the version it was written in, and a type
that didn't change between two versions decodes the same in both (`27.. => HeadV27`). A bump for
a checkpoint's change doesn't darken a bucket's identity: the identity of 27 is still read.

Each family declares itself once, in the `formats` crate's type:

```rust
pub const FORMAT: formats::Format = formats::Format::new("bucket layout", 28, 28);
```

`oldest` is fixed when the family freezes and never rises; `current` only rises. `Format::check`
is every reader's one version check, its refusal naming the family, the version found and the
range read. `scripts/format-freeze.py` refuses a push that raises `oldest` or lowers `current` of
a frozen family.

## The golden corpus

`formats/<family>/<version>/` at the repo's root, for each version of `oldest..=current`:

- `<case>.bin`: an object as that version's writer wrote it, made once by the build that froze or
  changed the family (`CLADEFS_FORMATS_WRITE=1` on its corpus test writes cases missing, never
  rewrites one);
- `<case>.txt`: what it decodes into, as that version's type (`{:#?}` of `HeadV27`), which
  never changes, as `HeadV27` never does;
- `fingerprint`, for families bincode serializes: `fprint::TypeFingerprint` of the version's
  types, so that editing a frozen type, or the current one without a new version, fails, where no
  case exercises what changed (an enum's variant). A JSON family's cases cover each variant
  instead, as JSON names what it holds.

Each family's corpus test (`formats::golden::check`) asserts that every version of
`oldest..=current` has a case, that each case decodes, by its version's decoder, into its
`.txt`, and that the family's current reader reads it; and that the current version's cases are
exactly what this build's writers write, byte for byte, so that a writer changing what it writes
is a new version. A conversion (`From<V2> for V3`) has a test of its own over the old version's
cases (`formats::golden::cases`): the corpus proves an old version is read, not what it is read
as. The script refuses a push that changes or
deletes any file under `formats/`, or a frozen family's `oldest`; the only way past it is a
`Breaks-format:` line in the commit's message naming the hotbed question in which the user
agreed to that break.

## Last changes before the freeze

A frozen format can grow only through what it can say. So each family first takes one last
incompatible change, where it lacks the means to grow (board 10033's subtasks):

- **Every object says its version.** Checkpoints, segment and moves footers, marks, objectid
  blocks, the `wal` start, local checkpoints, ephemeral keys, migration markers and the cell
  directory's records say none today, relying on the head's or the directory's. A block device's
  nodes and owners' lists said none, relying on its root's, which a commit leaves them under when
  it rewrites the root alone (board 10052).
- **Extent frames name their version as a number**, not one ASCII digit, which reads frames of 10
  on as corrupt; the table's MAC covers it (board 10040).
- **Unknown fields are refused, not ignored**: the `store` file's unknown keys, a migration
  marker's unknown fields. Bincode reads of frozen types refuse trailing bytes.
- **The peer service says each family's version**, not one opaque string compared whole, so that
  a member reads a writer's logs whenever it reads their versions.
- **A snapshot's extent device state says its version before bitcode reads it.**
- **The segment log's fallback to older records goes** (board 10062): a second, unversioned format.
- **Numbers in keys and file names are base 32**, as every other: the ephemeral index's bucket,
  holds', replica directories' and the ephemeral host's names.

Each family freezes when its corpus lands, with `oldest` its version then.

## Upgrading, once frozen

A host's upgrade is still its drain (doc-11, `docs/upgrading-a-host.md`), but a newer build reads
what the older wrote: its cell directories, replica directory and ephemeral cells stay, and are
read in place. A rollback is refused past any version change, by rule 1, as before.

## Undoing it

Unfreezing a family is deleting its corpus and its range with a `Breaks-format:` line; the
user's decision alone.
