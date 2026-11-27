# cladefs

Fxfs in a VM's own Linux kernel, on extents its host keeps for it and maps
straight from the host's memory (DAX), one copy shared by every VM of a clade.
This is the project's glossary: the words to use, and the ones to avoid.

## Language

### The clade

**Clade**:
A base and every fork grown from it, at any depth.

**Guest**:
The VM's Linux kernel and what runs on it, its Fxfs included.
_Avoid_: VM (when the kernel is meant), client

**Host**:
The machine that runs the VMs, their VM processes and one extent daemon.
_Avoid_: hypervisor; "host" for a test host (say **test host**)

**VM process**:
The Firecracker process of one VM, which holds its extent device and its DAX window's host memory.
_Avoid_: VMM (when the process is meant), jail

### Extents and the device

**Extent store device**:
The virtio device through which the guest's Fxfs reads, writes, maps, frees and commits extents.
_Avoid_: extent device (fine in passing), block device

**Extent**:
An immutable piece of data, written while open and never again once sealed.
_Avoid_: blob, object (an object is the bucket's)

**Extent ID**:
The name of an extent: the cell that wrote it and the sequence number it gave it.
_Avoid_: key, ref (a ref names blocks of an extent)

**Sequence number**:
The half of an extent ID the guest advances as it mints.
_Avoid_: seq

**Segment number**:
Which segment of a writer's epoch, in its key `<epoch>.<segment>`.
_Avoid_: seq, sequence

**Mint**:
To make a new extent ID locally, with no request.

**Sequence floor**:
The lowest sequence number a writer may mint from: the start of its epoch's range.

**Open extent**:
An extent from its first write until the barrier after that write seals it.

**Seal**:
What a barrier does to every extent open before it: makes it immutable, and mappable.
_Avoid_: seal for encryption (say **encrypted**), close, freeze

**Barrier**:
A request ordering every request completed before it ahead of every one submitted after; `COMMIT` is the usual one.

**Commit**:
A barrier that also records the cell's new root; the unit an open cell recovers to.
_Avoid_: flush, sync (a sync waits for a commit's level)

**Root**:
The one mutable record of a cell: where its metadata starts.

**Free**:
The guest's telling the extent store an extent is no longer referenced.
_Avoid_: delete, trim, discard

**Orphan**:
An extent written whose references the guest's journal lost in a crash.

### DAX

**DAX window**:
A range of guest physical memory into which the host maps sealed extents' pages, read-only.
_Avoid_: window alone where another window could be meant, region (a virtio shared memory region is the device's)

**Map**:
The `MAP` request: names the pages of a sealed extent at a window's address.

**Install**:
The first touch of a named page, which puts it into the VM process's memory.

**Prefault**:
Having KVM map installed pages ahead of the guest's accesses.
_Avoid_: map ahead, pre-fault, pre-faulting

**Private page**:
A window page the VM owns, copied on the guest's write or by `PRIVATE`, at the same address, named by no extent.
_Avoid_: dirty page (dirty is the guest's view), COW page

**private_max**:
The most private pages a window holds at once; a write past it stops the VM.

**Rebind**:
Mapping a private page back to the sealed extent its writeback wrote, sharing it again.
_Avoid_: map back

**Dirty set**:
A file's private pages not yet written back.

**Writeback set**:
The dirty set a writeback swapped out and is writing.

**Second stage**:
The translation of a window outside the daemon that follows its mappings: in the VM process, userfaultfd over the host memory KVM follows.
_Avoid_: stage alone, uffd stage

**Range**:
A unit of the guest's window, mapping one chunk of a file.

**Slice**:
A small file's chunk that is part of one sealed extent's region, shared with other files.

**Region**:
The guest's mapping of a whole sealed extent in its window, of which small files' slices are parts.
_Avoid_: region for virtio's (say **shared memory region**)

**Staging extent**:
An extent the guest's allocator packs small files' blocks into at writeback.

**Page-cache inode**:
A file created in this mount, served from the guest's page cache until it is next read in.

### The extent daemon

**Extent daemon**:
The one process on a host that owns the physical memory, the extent cache and the open cells, and serves them to VM processes.
_Avoid_: server, remote; `cladefsd`, its name on the command line, in its module, protocol and files

**Embedded daemon**:
The extent daemon running inside another program's process, one per process, shared by that process's VMs (`libcladefs`, as machined and libmachine run it); `cladefsd` is a thin binary over it. Its devices connect to it in process, through a socket pair with no listener or path.
_Avoid_: in-memory daemon, local daemon

**Daemon client**:
A VM process's connection to the extent daemon, through which its extent device has an open cell.
_Avoid_: remote, remote store, store

**Tenant**:
An owner of cells, who is granted a VM socket, an arena, memory and disk limits, and keys of its own.
_Avoid_: customer, account, user

**Tenant ID**:
A tenant's one identifier, a 64-bit number, in the bucket and every API alike.
_Avoid_: tenant name

**Default tenant**:
The tenant whose ID is 0: the daemon's own socket's, and public bases'.
_Avoid_: tenant 0, common tenant, no tenant

**Grant**:
Giving a tenant its own VM socket, for the VM manager to put in the tenant's jails.

**Physical memory**:
The daemon's memory, in arenas of shared memory, from which every cached and mapped page comes.
_Avoid_: PMM, page pool

**Arena**:
One shared memory object of physical memory, one per tenant plus the default one.

**Slab**:
The 1 MiB unit of an arena, a part's size, at an offset of its memory object: a part in the extent cache holds a whole one, or a run of a small slab's pages (doc-26, `cladefs::slabs`). A whole slab freed may be kept as a **spare**, populated still, for the next whole part.
_Avoid_: chunk (the 2 MiB unit of physmem, which slabs replaced), frame

**Staged** (a slab, to a VM process):
Named to the VM process's window by a `STAGE_MAP` since the slab was filled: the VM processes its quarantine waits for (doc-26).

**Eviction token**:
What an evicted part's slot holds while one eviction, which alone holds the slab, takes it from the windows (doc-26, `cache::EvictionToken`).

**Relief**:
Eviction of cached pages for an arena starved at its cap.

**Quarantine**:
Where a freed page waits until no VM process may still map it: since doc-26, a slab, until each VM process it was staged to acknowledged what took it away, or died.

**Wedged** (a VM process):
One the daemon killed that has not exited `KILLED_EXITING` after, as in uninterruptible sleep or a frozen cgroup (board 8535): what the quarantine holds for it comes back only once it exits, if ever, so shrinks and relief count it held, not on its way.

**Governor**:
What bounds the daemon's one memory budget, which every cache of its holds under; and, the disk
governor, what bounds each host disk's: the open cells first, the part cache what they leave.

**Host disk**:
A filesystem the daemon keeps cell directories or the part cache on, one however many of its directories are
opened: what its cell directories and part cache hold, counted, and what the disk governor allows them.
_Avoid_: volume, device (but for its `MAJOR:MINOR` label)

**Footprint**:
What a cell directory's files take of its host disk, counted as the open cell writes them: each file's length in
4 KiB granules, once however many names it has.

**Allotment**:
A granted tenant directory's footprints summed, with the tenant's disk limit, weight and share.

**Reserve**:
What an open cell's own work may still add for what was written, never refused: its commits copying it,
its logs holding it, its framings copying it. Admission weighs a write with it.

**Headroom**:
What a governor leaves free of the memory, or of a host disk: room for others between its looks.

**Spare**:
What a host disk's cell directories leave of its allowance, taken, reserved and admitted: what the part cache
keeps of it, and what new writes are admitted into.
_Avoid_: room (a segment's free space alone)

**Extent cache**:
The daemon's pages of sealed extents, in physical memory, shared by every window that maps them.
_Avoid_: cache alone, page cache

**Part cache**:
A host's disk copies of the bucket's objects' parts, no tenant's.
_Avoid_: cache alone, NVMe cache, object cache

**Cell page cache**:
The pages of a cell directory's data files cladefs holds itself, in place of the host's page cache.
_Avoid_: page cache alone (that is the kernel's)

### Cells, cell directories and the bucket

**Bucket**:
The object storage holding every cell's durable copy.
_Avoid_: remote, object store, S3

**Bucket number**:
A bucket's 16-bit number, which the high bits of every cell ID it gives carry, so that no two buckets give one ID (doc-23). Sequential from the root bucket's, 1; neither 0, the cells tests name, nor 65535.
_Avoid_: bucket ID (the identity object's random ID), bucket index

**Root bucket**:
The bucket numbered 1, whose registry is the authority and whose conditional writes serialize additions.
_Avoid_: bucket 0, home bucket (a host's own)

**Registry** (of buckets):
The record of every bucket, its number and details, in the root of every bucket: the root bucket's, and copies of it.

**Formatting** (a bucket):
Making a bucket never formatted the root of buckets of its own, number 1 (`cladefsctl format-bucket`, or its first allocator), writing its identity, registry, and initial format gates.

**Format gate** / **Ratchet**:
The two monotonic write-version numbers (`storage` for the host binary release train, and `fxfs` for guest Linux kernels) stored in a bucket's `formats` object at format time (`formats::Gates`, `formats::Ratchet`, doc-28). Writers never emit a version above the home bucket's gate; operators advance it monotonically (`cladefsctl ratchet`) once all nodes run the new build, keeping partial upgrades and rollbacks safe.

**Adding** (a bucket):
Giving a bucket never formatted the next number, by a conditional write of the root bucket's registry, then copying the registry to every bucket (`cladefsctl add-bucket`).

**Bucket's clock**:
The time by the bucket's backend, which dates its objects, as a host reads it: from the dates of the backend's responses, never the host's own clock (a bucket of files or memory dates by the host's, its own). Never ahead of the backend's, so that a window measured from an object's date closes late, never early; every collector lease and grace period is by it.
_Avoid_: now, wall clock, host time (the host's own)

**Cell**:
A tenant's data of one kind (Fxfs, SQLite, or a VM's memory image), with a root and a capacity, one writer at a time, its head, its key and its versions. A tenant's unit of concurrency, placement, fencing, encryption and collection; a host holds it whole. Its ID is unique across buckets, given by its bucket and carrying its bucket number; no caller names a new one. On a host, a cell is kept in its cell directory: its open segment, its write-ahead log and the extents not yet uploaded, a write-back cache of the bucket's copy.
_Avoid_: volume, disk, store

**Cell directory**:
The directory on a host's disk a cell is kept in, named by its path: its description, its local checkpoint, its segments and their logs, its write-ahead log. Control calls name a cell on a host by it (`path`). An ephemeral cell's is its only copy.
_Avoid_: store, store directory

**Open cell**:
A cell opened from its cell directory in the extent daemon (`OpenCell`), which VMs' daemon clients read and write through; where the difference from the cell or its directory doesn't matter, just the cell.
_Avoid_: store

**Volume**:
One of an Fxfs filesystem's object stores, mounted with `-o volume=NAME`, as upstream Fxfs says.
_Avoid_: volume for a cell

**Block device**:
A cell's contents as one array of blocks, read and written at offsets, discarded, resized and flushed, each flush one commit, kept straight on its extent store with no filesystem between: what a SQLite cell (its one database) and a memory cell (a VM's memory) hold (`block_device::BlockDevice`). No names or files: a cell needing those is an Fxfs cell. Not the extent store device, which is none.
_Avoid_: file set (retired 2026-11-21, board 5790), volume

**Version**:
A read-only state of a cell in its past, kept inside the cell. Only the cell's writer makes or drops one. Making one is local: one commit in the cell's local log that closes its segment, nothing touching the bucket; uploading it and naming it in the head come later, in the background.
_Avoid_: checkpoint (recovery's)

**Shared version**:
A version other tenants' cells may fork.
_Avoid_: public (public is a base's)

**Fork**:
A new cell made from a version of another, sharing its extents, in O(1). Local on the version's host; on another host it needs the version uploaded first.
_Avoid_: clone (git's alone), restore (a VM snapshot's alone)

**Parent** / **Child**:
The cell a fork came from, and the cell it made. Dropping a version that has children keeps its data until they go.

**Ephemeral cell**:
A host-local cell kept on one host's disk alone, never uploaded, with no versions of its own (doc-25): either forked from a parent version in a bucket (its parent's version its lower layer, held by a reference naming its host's incarnation) or standalone (parentless, e.g. a host-local ephemeral memory cell with no bucket reference). It survives what keeps the host's disk, supports host-local in-place suspend/resume, and (when forked) moves to another host only by hand-over.
_Avoid_: local cell (`local_cell` is the extent store's crate, Local a level), scratch, overlay, upper

**Hand over**:
Moving an ephemeral cell's writing from one host to another in a live migration: the source gives the cell up, then names its last commit, from which alone the target opens it.
_Avoid_: take (a bucket cell's, by its head), failover

**Decommission** (a host):
The VM manager's saying a host is gone for good, which retires every reference naming its incarnation, in every bucket, and leaves its tombstone there, which refuses that incarnation's references from then on.
_Avoid_: forget (a shredded tenant's or cell's keys), evict

**Origin**:
Where a child was forked from: its parent and the segment its version closed. The collector keeps an origin's state while children of other cells live on it.

**VM snapshot**:
A VM's saved state: a version of each of the VM's cells (its disk and its memory) plus the record naming them. Host-local: it pauses the VM, makes a version in each cell, and writes the record last; no bucket round trip, no cross-host coordination.

**Base**:
A cell VMs boot forks of, such as an imported container image.
_Avoid_: template, golden image

**Public base**:
A base whose pages every tenant's forks share, in the default arena.

**Read evidence**:
What forks of a version read of it as they opened, each tenant's apart: a **recording** is one fork's, every MiB of the version's extents its guest read in its first 10 s and when it first did, an object written once beside the version (TASK-702.2). The collector deletes it with the version, and past its tenant's newest 8.
_Avoid_: working set, boot profile (TASK-38's per-host file, which it replaces)

**Prediction**:
What a version's next fork will read, as its tenant's newest recordings say: every unit any of them read, the earliest read first. A host fetches it into its part cache as the fork opens, behind the guest's reads, then reads it from there into the extent cache, as the guest's page-ins would (board 6225), and counts how well each boot was predicted (TASK-702.3). One read of no recordings is not kept: the next fork reads it again (board 5337).

**Head**:
A cell's one record that changes: what made it, its keys, who writes it, the commit it ends at, and its versions, one object swapped in place, conditional on the version its writer last saw (doc-27). Taking it is taking the cell.

**Writer**:
The one holder of a cell's head, which alone commits to it and makes and drops its versions.
_Avoid_: owner, leader, primary

**Take**:
To become a cell's writer by swapping its head to the next epoch; that swap is the writer's **taking**.
_Avoid_: lease, acquire

**Epoch**:
The number of a taking: a writer's of a cell, a placement's, a pass's of its collector lease.
_Avoid_: epoch for a commit's number, generation

**Commit number**:
Which commit of a cell, as an open cell's stats name it.
_Avoid_: epoch

**Placement**:
A host's holding of a cell, under a fencing token.
_Avoid_: copy (a version's), holder; placement for where an open extent goes or which arena pages are in

**Halted**:
Of an open cell: stopped for good after a panic went through it (`EIO`) or its tenant or cell was shredded (`EACCES`).
_Avoid_: fenced

**Fencing token**:
The number a placement holds, greater than every earlier placement's, so a host cut off cannot take its cells back.

**Segment**:
Sealed extents packed in commit order, with a footer saying what each commit did; an object in the bucket.

**Open segment**:
The segment an open cell is writing, raw, with its log of commits.

**Write-ahead log**:
A cell's closed segments not yet acknowledged by the bucket, each byte for byte the object it will be.
_Avoid_: log (the log is an open segment's)

**Own extent**:
An extent large enough to be an object of its own rather than part of a segment.
_Avoid_: large extent

**Framing**:
Turning a closed segment into the bucket's object format: parts, footer, table and trailer.
_Avoid_: packing, sealing

**Part**:
A 1 MiB unit of an object, compressed, encrypted and fetched on its own.

**Upload**:
Putting a write-ahead log's segment into the bucket.

**Acknowledged**:
Of a segment: named by the head's swap, so the bucket has it and it may leave the cell directory.
_Avoid_: acknowledged for a VM process's taking in window changes (say **confirmed**)

**Lag bound**:
How far a cell's uploads may fall behind its commits before commits stall.

**Checkpoint**:
A cell's state as of a segment, so recovery reads from it rather than from the beginning. An
ephemeral cell's is a **local checkpoint**, in its directory.
_Avoid_: checkpoint for Fxfs's (say **journal checkpoint**)

**Relocation**:
A writer's commit copying live extents of its own cell's sparse segments into its open segment, which the head names as any: compaction, done by writers alone (doc-27; `local_cell::relocations`).
_Avoid_: moves (the collector's, which relocations replace), cleaning

**Collector**:
The only thing that deletes from the bucket; it copies nothing and holds no key (doc-27). **gc** is its short name, in its binary (`cladefs-gc`), keys and metrics.
_Avoid_: cleaner

**Pass**:
One run of the collector over one cell, under its collector lease.

**Sweep**:
A pass over every cell of a bucket.

**Collector lease**:
What fences one cell's passes from each other.
_Avoid_: lease alone (cells' writers have no lease), gc lease

### Durability

A commit's **levels**, each including the one before:

**Ordered**:
A commit the daemon has, in order, not yet on disk.

**Local**:
A commit synced to the host's disk.
_Avoid_: durable, durable locally

**Fast WAL**:
A commit appended via an offset-checked write to an appendable zonal object store (`tenants/<T>/cells/<C>/wal/<epoch>/<segment>`, Amazon S3 Express One Zone today, GCS Rapid Storage via gRPC `BidiWriteObject` planned, doc-29), or acknowledged in the cold regional bucket. Compute nodes stay stateless cache-only with zero local disk `fsync`s; large extents (`>= 256 KiB` by default) upload directly to the cold regional bucket before referencing them metadata-only in the fast WAL.
_Avoid_: zonal log, express WAL (say Fast WAL)

**Uploaded**:
A commit whose segments the bucket has acknowledged.
_Avoid_: durable

**Incarnation**:
A random number an ephemeral host's directory is made with, new when its disk is lost: ephemeral cells' references name it so a host that lost its disk retires its old references (doc-25). Erlang's node creation; BookKeeper's cookie.
_Avoid_: cookie (Erlang's is a shared secret for authentication), generation

**Crash bound**:
How much of what was ordered a crash may lose: at most a second's worth.

**Drain**:
What a daemon does on SIGTERM: every open cell closed and its commits uploaded before it exits.

**Format**:
The version an on-disk, bucket or wire format names. A **frozen** family's (doc-28) every later build reads from its oldest on, converting on read; one not frozen yet, its reader refuses every other.

**Frozen** (a format family):
One whose golden corpus is under `formats/` (doc-28): every later build reads every version of it from its oldest on, and a change is a new version, never an edit of an old one.

### Encryption

**Tenant KEK**:
A tenant's key in the KMS, which never leaves it and wraps its cells' KEKs.

**Cell KEK**:
A cell's key, wrapped by its tenant's KEK, wrapping its DEK.

**Cell DEK**:
A cell's data key, under whose subkeys its objects are encrypted.

**Encrypted**:
Of a cell or object: held under its tenant's keys.
_Avoid_: sealed (sealing is extents'); its opposite is **unencrypted**, or in the clear

**Shred**:
Making data unreadable by destroying the key that wraps it: a tenant's KEK, or a cell's wrapped KEKs.

**Forget**:
A host's dropping the keys of a shredded tenant or cell, fencing its open cells.

### Testing and measurement

**Test host**:
A Linux machine of one's own that runs cladefs's Linux tests through hotbed, named by `CLADEFS_TEST_HOST`. Its drivers keep the `-host.sh` suffix, short for "on the test host"; its env vars say `TEST_HOST` (`CLADEFS_TEST_HOST*`).
_Avoid_: host alone

**Driver**:
A `scripts/*-host.sh` script that runs a job on the test host and copies its results back.

**Job**:
One hotbed command on the test host: **work** (builds, tests) or a **measurement** (`hotbed run --bench`, alone on its cores).

**Run**:
One driver's job on the test host, in a worktree of its own, numbered like `r1040`.
_Avoid_: run for a case's repeats or a GitHub run, unqualified

**Case**:
One guest test, which boots VMs and checks what they did.
_Avoid_: test alone

**Suite**:
The cases a tag selects.

**LangSmith's pin**:
The commit LangSmith runs.
_Avoid_: pin for a toolchain, Firecracker release or tool version (say **pinned version**)

**Candidate**:
A commit offered to become the next pin.

**Qualification**:
A candidate measured against the pin on the test host: the sandboxes' long mode and the bucket soak, every daemon held to its budgets.

**A/B**:
Two commits measured in interleaved rounds, each a **side**.

**Round**:
Each side of an A/B once, interleaved.

**Repeat**:
One of a case's runs when a soak runs it many times (`repeat-K.json`).
_Avoid_: round

**Control**:
A case with no cladefs in it, whose movement shows the test host drifted.

**Contaminated**:
Of a round: others wrote to its disk past the bound as it ran, so its timings are left out.

**No result**:
A metric, or a night, with no round kept on a side.

**Count**:
A metric a load beside a round cannot move, compared exactly.

**Budget**:
What a daemon's design bounds a cost to (memory, disk written, CPU per MiB), which a case fails past.

**Soak**:
A long case under faults, such as the bucket soak's slow, full and failing bucket.

**Nightly**:
The test host's nightly A/B of main's head against the night before.
_Avoid_: nightly for GitHub's nightly suite (say **nightly suite**), or Rust's toolchain

**Hunt**:
The nightly CI workflow hunting bugs with seeds and faults the push CI doesn't reach.

**Mutant**:
A past bug put back, as a patch, which named tests must fail; a **hand mutant**, as against cargo-mutants' generated ones.

**Spec mutation**:
A TLA+ spec with one rule removed, whose violation TLC must find.

**Dead end**:
An approach tried and given up on, kept with why.

**Wrong theory**:
A dead end that was a diagnosis, not an approach.

## Ambiguities

Words that meant several things. Say the qualified form. These choices bind code
identifiers too (types, protos, env vars, binaries), not prose alone; the names below that the
code does not have yet are said so.

- **seal**: extents' alone; encryption is **encrypted**, in code too.
- **remote**: retired, in code too; say **bucket** or **daemon client** (`CLADEFSD_BUCKET`).
- **store**: retired for a cell on a host, in code too (2026-11-21, board 5622): its directory is the **cell directory** (`cell_dir`), the cell opened from it an **open cell** (`OpenCell`), and else just the **cell**. Left to the **extent store**, the component (`local_cell`, the daemon's storage), and to Fxfs's **object stores**. A CPU's is a **write** (a **store instruction** where a syscall's write could be meant); the object-storage backend's calls are `object_*`, its `ObjectStore` implementations `...Backend`. The cell directory's files (`store`, `store.deleted`), metric names, proto fields and CLI flags keep the word until their own rename.
- **volume**: Fxfs's alone. A cell's array of blocks on an extent device, with no filesystem, is
  its **block device** (`block_device::BlockDevice`).
- **snapshot**: VMs' (Firecracker's) and Fxfs's alone. A cell's named state is a **version**; the daemon's counters are **stats** (no `GetSnapshot`).
- **version**: a cell's past state alone; a format's is its **format**, a tool's a **pinned version**.
- **epoch**: a taking's number alone. A commit's number is a **commit number**, in StatCell's
  `committed`, `LocalCell::last_commit_held` and the trace's `cladefs.commit`.
- **fence**: being cut off by a later taking alone (a writer, a placement, a collector pass). An open cell stopped after a panic or a shred is **halted** (`cladefs::halt`).
- **lease**: only the collector's; writers have none since TASK-407.
- **durable**: names no level; say **local** or **uploaded**, in code too (`Commit::Local`,
  `COMMIT_UPLOADED`, the `local_lag` stats, the trace's `cladefs.level`).
- **cache**: the extent cache, the part cache, the cell page cache, the host's page cache. Name it,
  env vars too (`CLADEFSD_PART_CACHE`, `CLADEFSD_PART_CACHE_BYTES`).
- **region**: the guest's; virtio's is a **shared memory region**.
- **format**: a format's version alone (**Format**); making a bucket a root is **formatting** it, and making a filesystem is Fxfs's mkfs.
- **base**: a cell VMs fork. The WAL's is its **start**; **generation** is keys' alone.
- **super-block**: Fxfs's on-disk record; **superblock** is the kernel's.
- **Fxfs** / **cladefs** in the guest: Fxfs is Fuchsia's filesystem, its crates and its format; the guest
  kernel's module that runs it is `cladefs` (`fs/cladefs`, `mount -t cladefs`, `cladefs.*` parameters,
  the `cladefs` tracepoints); cases keep `fxfs-guest-*`, named for what they test.
- **mutation**: Fxfs's journal mutation; testing's are **mutants** and **spec mutations**.
- **stage**: the second stage alone. A segment's life is its **phase**; a commit's durability, its **level**.
- **pin**: LangSmith's commit is **LangSmith's pin**, never the candidate; a toolchain's or tool's is a **pinned version** (`pinned_version`).
- **round**, **run**, **job**, **repeat**: as defined above, not interchangeably, in code too
  (`CLADEFS_E2E_REPEATS`, `repeat-K.json`).
- **room**: a segment's free space alone. A tenant's is its **in-flight** budget (`SetTenantInFlight`); a peer stream's, its **credit**.
- **type names**: two types of one name are one concept; unrelated ones get names of their own (`RingView`, `TableFooter`, `TABLE_BLOCK`).
- **nightly**: the test host's A/B (**the nightly**); GitHub's is the **nightly suite**; Rust's, **Rust nightly**. Prose only: names stay.
