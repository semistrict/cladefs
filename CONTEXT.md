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
_Avoid_: hypervisor; "host" for ubu1 (say **test host**)

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
A barrier that also records the cell's new root; the unit the store recovers to.
_Avoid_: flush, sync (a sync waits for a commit's level)

**Root**:
The one mutable record of a cell: where its metadata starts.

**Free**:
The guest's telling the store an extent is no longer referenced.
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
A window page the VM owns, copied on the guest's store or by `PRIVATE`, at the same address, named by no extent.
_Avoid_: dirty page (dirty is the guest's view), COW page

**private_max**:
The most private pages a window holds at once; a store past it stops the VM.

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
The one process on a host that owns the physical memory, the extent cache and the stores, and serves them to VM processes.
_Avoid_: server, remote; `cladefsd`, its name on the command line, in its module, protocol and files

**Daemon client**:
A VM process's connection to the extent daemon, through which its extent device has a store.
_Avoid_: remote, remote store

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

**Chunk**:
The 2 MiB unit an arena takes from and gives back to physical memory.

**Relief**:
Eviction of cached pages for an arena starved at its cap.

**Quarantine**:
Where a freed page waits until no VM process may still map it.

**Governor**:
What bounds the daemon's one memory budget, which every cache of its holds under.

**Extent cache**:
The daemon's pages of sealed extents, in physical memory, shared by every window that maps them.
_Avoid_: cache alone, page cache

**Part cache**:
A host's disk copies of the bucket's objects' parts, no tenant's.
_Avoid_: cache alone, NVMe cache, object cache

**Store page cache**:
The pages of a store's data files cladefs holds itself, in place of the host's page cache.
_Avoid_: page cache alone (that is the kernel's)

### Cells, stores and the bucket

**Bucket**:
The object storage holding every cell's durable copy.
_Avoid_: remote, object store, S3

**Cell**:
A tenant's data of one kind (Fxfs, SQLite, or a VM's memory image), with a root and a capacity, one writer at a time, its head, its key and its versions. A tenant's unit of concurrency, placement, fencing, encryption and collection; a host holds it whole. Its ID is unique in its bucket, which gives it; no caller names a new one. On a host, a cell is kept in a directory: its open segment, its write-ahead log and the extents not yet uploaded, a write-back cache of the bucket's copy.
_Avoid_: volume, disk

**Volume**:
One of an Fxfs filesystem's object stores, mounted with `-o volume=NAME`, as upstream Fxfs says.
_Avoid_: volume for a cell

**File set**:
Named files kept straight on an extent device, with no filesystem between, each committed alone: what a SQLite or memory cell holds (`extent_files`).
_Avoid_: volume

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

**Origin**:
Where a child was forked from: its parent and the segment its version closed. The collector keeps an origin's state while children of other cells live on it.

**VM snapshot**:
A VM's saved state: a version of each of the VM's cells (its disk and its memory) plus the record naming them. Host-local: it pauses the VM, makes a version in each cell, and writes the record last; no bucket round trip, no cross-host coordination.

**Base**:
A store VMs boot forks of, such as an imported container image.
_Avoid_: template, golden image

**Public base**:
A base whose pages every tenant's forks share, in the default arena.

**Attached base**:
A fork of another store attached to a cell, for an Fxfs volume over a read-only base.

**Head**:
A cell's one object that changes: what made it, its keys, who writes it, the commit it ends at, and its versions. Taking it is taking the cell.

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
Which commit of a cell, as a follower's view or a store's stats name it.
_Avoid_: epoch

**Placement**:
A host's holding of a cell, under a fencing token.
_Avoid_: copy (a version's), holder; placement for where an open extent goes or which arena pages are in

**Halted**:
Of a store: stopped for good after a panic went through it (`EIO`) or its tenant or cell was shredded (`EACCES`).
_Avoid_: fenced

**Fencing token**:
The number a placement holds, greater than every earlier placement's, so a host cut off cannot take its cells back.

**Segment**:
Sealed extents packed in commit order, with a footer saying what each commit did; an object in the bucket.

**Open segment**:
The segment a store is writing, raw, with its log of commits.

**Write-ahead log**:
A store's closed segments not yet acknowledged by the bucket, each byte for byte the object it will be.
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
Of a segment: named by the head's swap, so the bucket has it and it may leave the store.
_Avoid_: acknowledged for a VM process's taking in window changes (say **confirmed**)

**Lag bound**:
How far a store's uploads may fall behind its commits before commits stall.

**Checkpoint**:
A cell's state as of a segment, so recovery reads from it rather than from the beginning.
_Avoid_: checkpoint for Fxfs's (say **journal checkpoint**)

**Collector**:
The only thing that deletes from the bucket; **gc** is its short name, in its binary (`cladefs-gc`), keys and metrics.
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

**Replicated**:
A commit on the disks of f+1 of its configuration's 2f+1 members, the writer's counting as one (doc-21); between Local and Uploaded, and released by Uploaded too.
_Avoid_: durable, quorum-committed

**Uploaded**:
A commit whose segments the bucket has acknowledged.
_Avoid_: durable

**Configuration**:
A replicated cell's writer and 2f peers at one time, holding a write-once log of the commits the bucket has not acknowledged; the head names the active one.
_Avoid_: ensemble, replica set (in code, say configuration)

**Member**:
A host in an incarnation, holding a configuration's log; the writer is one.
_Avoid_: replica (alone), follower (a follower reads, doc-8)

**Incarnation**:
A random number a daemon's replica directory is made with, new when its disk is lost: a member is a host in one incarnation, so a host that lost its disk, and its promises with it, is no longer that member. Erlang's node creation; BookKeeper's cookie.
_Avoid_: cookie (Erlang's is a shared secret for authentication), generation

**Fence** (a member):
What a take does to f+1 members of the active configuration before it reads their logs: raises each one's promise to the take's epoch, so that it refuses the appends of every older one (BookKeeper's word). A head's swap fences a writer as a fencing token fences a placement.
_Avoid_: seal (a barrier seals extents)

**Promise**:
The greatest epoch that fenced a member, on its disk.

**Crash bound**:
How much of what was ordered a crash may lose: at most a second's worth.

**Drain**:
What a daemon does on SIGTERM: every store closed and its commits uploaded before it exits.

**Format**:
The version an on-disk, bucket or wire format names; a reader refuses every other, and nothing migrates.

### Followers

**Follower**:
A read-only mount of a store another VM on the host writes, applying that writer's commits as they come.
_Avoid_: replica, secondary, reader

**Writer** (of a mount):
The one read-write mount whose commits followers apply.

**Reader**:
A task in a follower's guest reading its files.

**View**:
A store as of one commit, which keeps what later commits free until let go.

**Advance**:
Moving a follower's view to the writer's latest commit.

**Let go**:
Releasing the view a follower advanced from.

**Pin**:
Keeping one extent past a follower's view, for a file it holds open.

**Cut off**:
Ending a follower that fell too far behind; its operations fail with `ESTALE`.

**Batch**:
The writer's commits a follower applies at once, under its fill barrier.

**Fill barrier**:
A follower's lock: readers hold it shared while filling the kernel's caches, a batch exclusively.

**Settle**:
After a batch, the kernel's inodes taking the attributes it changed.

**Follower's graveyard**:
What keeps a held file readable after its writer deleted it.
_Avoid_: graveyard alone (Fxfs has its own)

### Encryption

**Tenant KEK**:
A tenant's key in the KMS, which never leaves it and wraps its cells' KEKs.

**Cell KEK**:
A cell's key, wrapped by its tenant's KEK, wrapping its DEK.

**Cell DEK**:
A cell's data key, under whose subkeys its objects are encrypted.

**Encrypted**:
Of a cell, store or object: held under its tenant's keys.
_Avoid_: sealed (sealing is extents'); its opposite is **unencrypted**, or in the clear

**Shred**:
Making data unreadable by destroying the key that wraps it: a tenant's KEK, or a cell's wrapped KEKs.

**Forget**:
A host's dropping the keys of a shredded tenant or cell, fencing its stores.

### Testing and measurement

**Test host**:
A Linux machine of one's own that runs cladefs's Linux tests over ssh; ubu1 by default. Its drivers keep the `-host.sh` suffix, short for "on the test host"; its env vars say `TEST_HOST` (`CLADEFS_TEST_HOST*`).
_Avoid_: host alone

**Driver**:
A `scripts/*-host.sh` script that runs a job on the test host and copies its results back.

**Job**:
One fxjob cgroup on the test host: **work** (builds, tests) or a **measurement** (`--bench`, alone on its cores).

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
- **store**: the extent store's alone, `Store` types included; the bucket's client and the device's request server get names of their own. Fxfs's are **object stores**; a CPU's is a **store instruction** where unclear.
- **volume**: Fxfs's alone. Named files on an extent device, with no filesystem, are a
  **file set** (`extent_files::FileSet`).
- **snapshot**: VMs' (Firecracker's) and Fxfs's alone. A cell's named state is a **version**; the daemon's counters are **stats** (no `GetSnapshot`).
- **version**: a cell's past state alone; a format's is its **format**, a tool's a **pinned version**.
- **epoch**: a taking's number alone. A commit's number is a **commit number**, in the device protocol,
  StatCell's `committed`, `Store::last_commit`, `View::commit` and the trace's `cladefs.commit`.
- **fence**: being cut off by a later taking alone (a writer, a placement, a collector pass). A store stopped after a panic or a shred is **halted** (`cladefs::halt`).
- **lease**: only the collector's; writers have none since TASK-407.
- **durable**: names no level; say **local** or **uploaded**, in code too (`Commit::Local`,
  `COMMIT_UPLOADED`, the `local_lag` stats, the trace's `cladefs.level`).
- **cache**: the extent cache, the part cache, the store page cache, the host's page cache. Name it,
  env vars too (`CLADEFSD_PART_CACHE`, `CLADEFSD_PART_CACHE_BYTES`).
- **region**: the guest's; virtio's is a **shared memory region**.
- **base**: a store VMs fork. The WAL's is its **start**; **generation** is keys' alone (MakePublic's flag is `attach_only`).
- **super-block**: Fxfs's on-disk record; **superblock** is the kernel's.
- **Fxfs** / **cladefs** in the guest: Fxfs is Fuchsia's filesystem, its crates and its format; the guest
  kernel's module that runs it is `cladefs` (`fs/cladefs`, `mount -t cladefs`, `cladefs.*` parameters,
  the `cladefs` tracepoints); cases keep `fxfs-guest-*`, named for what they test.
- **mutation**: Fxfs's journal mutation; testing's are **mutants** and **spec mutations**.
- **stage**: the second stage alone. A segment's life is its **phase**; a commit's durability, its **level**.
- **pin**: a follower's extent (`PIN`); LangSmith's commit is **LangSmith's pin**, never the candidate; a toolchain's or tool's is a **pinned version** (`pinned_version`).
- **round**, **run**, **job**, **repeat**: as defined above, not interchangeably, in code too
  (`CLADEFS_E2E_REPEATS`, `repeat-K.json`).
- **room**: a segment's free space alone. A tenant's is its **in-flight** budget (`SetTenantInFlight`); a peer stream's, its **credit**.
- **type names**: two types of one name are one concept; unrelated ones get names of their own (`KeptImage`, `RingView`, `TableFooter`, `TABLE_BLOCK`).
- **follower spec**: Follower.tla says the code's words (`LetGo`, `Drop`); invalidate is retired.
- **nightly**: the test host's A/B (**the nightly**); GitHub's is the **nightly suite**; Rust's, **Rust nightly**. Prose only: names stay.
