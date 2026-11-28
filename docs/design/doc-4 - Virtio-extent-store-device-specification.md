---
id: doc-4
title: Virtio extent-store device specification
type: specification
created_date: '2026-10-13 22:18'
updated_date: '2026-11-28 18:00'
---
# The virtio extent-store device

Specification, 2026-10-13 (TASK-26.2, revised the same day). This is the only interface between the
guest's Fxfs and the host's extent store (doc-2). The guest's driver is TASK-26.5, and the VMM's
device is crates/virtio_extent, which every VMM hosting it shares: cladefs's Firecracker
(vendor/firecracker, its adapter) and libmachine (TASK-417); first libkrun's (TASK-26.4; libkrun was
removed in TASK-40).

*Revised 2026-11-04 (TASK-164, doc-13):* the DAX window maps sealed extents and zeros alone,
read-only, and copies on write: a page the guest writes is the VM's own, a private copy at the same
address. Writable mappings of open extents (`DAX_WRITE`, `MAP_WRITE`), the host's bound on their
memory (`ENOMEM`) and its asks that guests seal (`SEAL`, `seal_asked`, `SEALED`) went with them
(dead-ends/writable-dax.md).

## Purpose

Efficient access to object storage from within a guest's kernel. Fxfs is the first client: it
stores its whole filesystem through the device, and anything else in the guest could use it the
same way, minting object IDs and keeping its own index of them.

It is a paravirtual device for an object store, in place of a block device, for two reasons:

- **No finite size.** A block device has one: a range of addresses to fill, and free space to manage
  inside it. An object store has none: it holds what is written, and its only limit is a quota.
- **The host's caches, shared.** The host keeps what VMs read in its memory and on its NVMe, and
  shares it between every VM on the host for every extent they have in common: forks of one VM,
  VMs of one base image, or files that happen to be the same extents. A guest maps shared pages
  straight from the host's memory (DAX), and reads miss to the host's NVMe before object storage.
  A block device's addresses mean nothing outside one VM, so it can share none of this.

## The model

The cell holds **extents**: immutable pieces of data, each named by an ID the guest mints. There
are no addresses, no free space and no device size.

- **IDs.** An extent ID is 128 bits: the branch's 64-bit ID, which the host assigns when it creates
  or forks the filesystem, and a 64-bit sequence number that the guest's Fxfs advances. Minting
  needs no request, and IDs never collide across forks or hosts.
- **Write-once.** The guest writes an extent, in pieces if it likes, until the next barrier. The
  barrier seals it, and it never changes again. Writing a sealed extent is an error.
- **Freeing.** The guest frees an extent once nothing in its filesystem refers to it. The host
  deletes it once every branch that has it has freed it.
- **The root.** Each branch has one mutable record: the root, which names where its filesystem's
  metadata starts. A commit can set it.
- **Capacity.** Capacity is a quota the host reports. The guest counts its extents against it.

### Why

doc-4's first version gave each filesystem a sparse 2^48-byte address space. It kept Fxfs as it
was, but it kept a block device's worldview with it:

- offsets, free space, best-fit allocation and trimming, over a space mostly never written;
- a device size standing in for capacity;
- a host that has to reconstruct what to pack, share, fork and delete from ranges of addresses.

Extents are what object storage, a shared page cache, forks and garbage collection all work in, so
the device speaks them directly.

### Decisions

Settled in a design review on 2026-10-13:

| Decision | Choice | Rejected |
|---|---|---|
| What the guest names | Extents, any size: one per extent Fxfs writes | Fixed-size chunks (3FS, JuiceFS); guest-built log segments, which need a cleaner in the guest |
| How extents become object-store objects | **Hybrid:** small extents are packed, in commit order, into segment objects of about 16-64 MiB; extents of 4 MiB and up are objects of their own | One object per extent, or per block of one (JuiceFS), where every small file is at least one PUT; packing everything, which makes cleaning copy large files |
| Mutability | Write-once, sealed by the next barrier | Appendable extents; extents the host versions |
| IDs | Branch and sequence, 128 bits | Host-granted 64-bit ranges; content hashes |
| Freeing | By the guest, of whole extents: dead parts stay stored until the extent dies or Fxfs rewrites the file | Freeing byte ranges; the guest reporting live ranges for the host to trim |
| Fxfs's format | Extents only: cladefs's image files become a local extent store | Fxfs keeping its block-device mode too; a separate Fxfs for the guest |

## Device

- **Device ID:** `0x4658` ("FX"), outside the virtio IDs OASIS assigns. The transport is virtio 1.x
  over PCI alone (since TASK-273): the DAX window is a 64-bit BAR of the device's own, which
  `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` capabilities describe, as MMIO can't.
- **Byte order:** every field is little-endian.

### Version

The configuration's first field, `version`, is the device's protocol version, `VIRTIO_EXTENT_VERSION`,
3 (TASK-141.3; 2 since live followers went, board 8237; 3 since the stream transport, board 14663). A change to any request, flag, feature,
configuration field or what it means makes a new one: cladefs migrates nothing (AGENTS.md), so a
driver and a device of different versions refuse each other rather than serve with what both happen
to know. The driver reads `version` first, as it
probes, and refuses a device of another, naming both (`-EPROTONOSUPPORT`). No version moves it.
A VMM restoring a snapshot refuses one whose device was of another version: its guest's driver goes
on as of that one.

### Features

The features are no negotiation: they say what the device is configured as, with a window or
not. A driver takes every feature the device offers; the device refuses
its activation otherwise (`DEVICE_NEEDS_RESET`, both named in the VMM's log), and the driver fails
its probe on seeing so after `DRIVER_OK`.

| Bit | Name | Meaning |
|---:|---|---|
| 0 | `VIRTIO_EXTENT_F_DAX` | Shared memory region 0 is a DAX window, which maps sealed extents and zeros, read-only, and copies on write (Shared memory): `MAP`, `UNMAP` and `PRIVATE` work, `MAP` takes `MAP_ZEROS`, and the configuration holds `private_max`. |
| 2 | `VIRTIO_EXTENT_F_WRITE_COMMIT` | `WRITE` takes `WRITE_COMMIT`: a commit follows the write. |
| 3 | `VIRTIO_EXTENT_F_RESIZE` | The configuration holds `capacity` and `asked`, which change while the device runs (Resizing); the guest answers a shrink with `RESIZED`. |
| 4 | `VIRTIO_EXTENT_F_FREEZE` | `FREEZE` works (Snapshots). |
| 6 | `VIRTIO_EXTENT_F_FREE_MANY` | `FREE` takes `FREE_MANY`: its data names more extents to free (TASK-41). |
| 7 | `VIRTIO_EXTENT_F_MAP_MANY` | `MAP` takes `MAP_MANY`: its data holds more maps (TASK-91.4). Needs `DAX`. |
| 8 | `VIRTIO_EXTENT_F_MAP_PREFAULT` | `MAP`, and each of its further maps, takes `MAP_PREFAULT`: the guest accesses the pages next (TASK-112.1). Needs `DAX`. |
| 11 | `VIRTIO_EXTENT_F_STATS` | The configuration holds `stats_asked`, which changes while the device runs: the host asks for the guest's stats, which it answers with `STATS` (Stats, TASK-111.4). |
| 12 | `VIRTIO_EXTENT_F_STREAM` | The guest sends its data requests, `READ`, `WRITE`, `FREE`, `COMMIT`, `ROOT` and `INFO`, over a vsock stream to the host's port `stream_port`, not on the queues (Stream transport, board 14663). |
| 29 | `VIRTIO_RING_F_EVENT_IDX` | The ring's notification suppression: the device offers it, so that a guest's requests queued while it takes them, and its completions while the driver takes them, cost no exit each. |
| 32 | `VIRTIO_F_VERSION_1` | Required. |

Bits 1, 9 and 10 were `DAX_WRITE`, `SEAL` and `DAX_COW` (copy on write beside writable mappings,
2026-11-04), which went as copying on write became the only DAX. Bit 5 was `READ_ONLY`, a live
follower's device (doc-8), which went with followers (2026-11-22, board 8237). None is reused.

### Configuration space

```c
struct virtio_extent_config {
	le32 version;             /* VIRTIO_EXTENT_VERSION, 2: first in every version (Version) */
	u8 tag[36];               /* the filesystem's name, NUL-padded: the mount's source */
	le32 num_request_queues;  /* at least 1 */
	le32 block_size;          /* 4096: the alignment of offsets and lengths */
	le32 map_alignment;       /* the host's page size: the alignment of MAP and UNMAP */
	le32 max_transfer;        /* the most bytes one READ or WRITE moves */
	le32 max_segments;        /* the most data descriptors one request carries */
	le32 max_root;            /* the most bytes the root holds: at least 512 */
	le64 branch;              /* the branch's ID: the first half of every extent the guest mints */
	le64 max_extent;          /* the most bytes one extent holds: at least 64 MiB */
	le64 capacity;            /* F_RESIZE: the cell's capacity, as INFO's */
	le64 asked;               /* F_RESIZE: the shrink the guest is asked to take, or 0 */
	le64 private_max;         /* F_DAX: the most private pages the window holds at once, in
	                             pages of map_alignment */
	le64 stats_asked;         /* F_STATS: the host's latest ask for the guest's stats, or 0 */
	le32 stream_port;         /* F_STREAM: the host's vsock port the data requests go to */
	le32 reserved;
};
```

The configuration never changes while the device runs, but for `capacity` and `asked`, with
`F_RESIZE`, and `stats_asked`, with `F_STATS`. The host sets
`private_max` as it opens the device, and reserves that much memory for the VM then, so that a write never waits for memory.

### Resizing

*Added 2026-10-17 (TASK-37).* The host resizes a cell while its guest runs (`cladefsctl
resize`, through the extent daemon). It grows it at once, sets `capacity`, and raises a
configuration change: the guest's filesystem takes the larger size, its device's block count and
its allocator's budget, while mounted. A shrink needs the guest, whose filesystem may have
reserved more than the cell holds: the host sets `asked` and raises a configuration change; the
guest shrinks if its filesystem holds less than `asked`, with room left for its transactions
(16 MiB, in cladefs's Fxfs), and answers with `RESIZED`, whether it took it; the cell shrinks only
then, never below what its extents hold. The host waits 10 s for the answer. A cell no VM has
open grows, and does not shrink.

### Stats

*Added 2026-11-05 (TASK-111.4).* The host reads, at once, what its extent daemon, each VM's
process and each guest counted: its stats (`cladefsctl stats --json`, the control API's `GetStats`). With
`F_STATS`, the device asks the guest for its part: it sets `stats_asked` to a new number and raises
a configuration change; the guest answers with `STATS`, naming the ask, its data its stats as UTF-8
text, `VIRTIO_EXTENT_MAX_STATS` (64 KiB) at most: lines of a name and a value, a number or a
histogram's numbers between commas (`NAME_hist`), and `commit COMMIT`, the commit its filesystem
was built of. cladefs's guest says what its `/proc/fs/cladefs/TAG` says, and `/proc/fs/cladefs/build`. An
answer answers the asks before it too, as the guest may see only the latest. A guest with nothing
mounted answers with no text. The host waits for the answer a while (2 s by default), and reports
the guest as not having answered past it. The device asks only once the driver acked `F_STATS`, and
until it resets; otherwise the VM process answers itself, with no guest's stats.

### Virtqueues

Queues `0` to `num_request_queues - 1` are request queues, interchangeable. A driver may use one per
CPU. Each request is one descriptor chain:

1. the header, which the device reads;
2. the request's arguments, which the device reads;
3. for `WRITE`, and for a `COMMIT` that sets the root, the data, which the device reads;
4. for `READ` and `ROOT`, the buffer for the data, which the device writes;
5. the response, which the device writes.

The driver may have as many requests in flight as the queues hold.

### Stream transport

*Added 2026-11-28 (board 14663).* With `VIRTIO_EXTENT_F_STREAM`, the guest sends its data requests,
`READ`, `WRITE`, `FREE`, `COMMIT`, `ROOT` and `INFO`, over a vsock stream it connects to the host
(CID 2) at `stream_port`, and nothing else on it: the window's requests and those of the device's
state (`MAP`, `UNMAP`, `PRIVATE`, `RESIZED`, `FREEZE`, `STATS`) stay on the queues. cladefs's extent
daemon serves the stream itself, the VMM's stock vsock device copying it between guest memory and
the daemon, so the data requests need nothing of the VMM's own. A request is a chain's bytes in
order, framed:

```c
struct virtio_extent_stream_req {
	le64 tag;       /* the driver's: names the reply, unique among those in flight */
	le32 readable;  /* bytes that follow: header, arguments, data, root */
	le32 writable;  /* bytes the device writes: data, then the response */
};

struct virtio_extent_stream_reply {
	le64 tag;       /* the request's */
	le32 written;   /* bytes that follow: its `writable`, or 0 if it wrote nothing */
	le32 reserved;
};
```

Replies come in any order. A reply of nothing written fails its request with `EIO`. A request past
the protocol's bounds (more to read than any request has, more to write than any returns, or no room
for a response) closes the stream: nothing after it could be read in step. Barriers order the
stream's requests and the queues' alike, by completion: a `MAP` of an extent goes on its queue once
the `COMMIT` that sealed it completed on the stream.

A stream whose connection is lost, as a vsock device resets every connection when its VM is
restored from a snapshot, connects again (tried for a second, as a vsock device that reset refuses
connections until its guest heard of the reset), and the driver sends every request in flight
again, each whole. Every data request may be sent twice: a `COMMIT`, `FREE`, `READ`, `ROOT` or
`INFO` again changes nothing more; a `WRITE` again writes the same bytes to an extent still open, as
the guest sends a barrier only once the writes it orders completed. A `WRITE` whose first send
sealed its extent (`WRITE_COMMIT`), or that another's `WRITE_BARRIER` sent again sealed, would fail
`EPERM` sent again: on a stream, a `WRITE` of the bytes a sealed extent of the guest's own branch
already holds at its offset succeeds, changing nothing, and its commit, if it carries one, is made
(another branch's extent, or a public base's, is refused whatever it holds). A request cut short as a
connection ends is not answered, so that its guest sends it again. One whose connection is lost again before any
reply, or that can't connect, fails every request in flight, and the driver every one after, `EIO`,
as when the host's daemon goes (When the host's daemon goes). A snapshot holds no stream, whose
listener's path is each VM's own: its restore gives it (cladefs's Firecracker: the load's
`cladefs_overrides`, `stream`), and a guest that sent its requests so, restored without one,
refuses its device, whose features lack `F_STREAM`.

### Shared memory

With `VIRTIO_EXTENT_F_DAX`, shared memory region 0 is the DAX window: guest-physical memory whose
contents `MAP` sets. The transport gives its size, as virtio-fs's does.

Every window page is one of three: a sealed extent's page, which every window that maps it shares;
zeros; or a **private page**, the VM's own, which no extent names. A write to either of the first
two makes it private, holding what it held, at the same address, mapped writable: nothing the guest
maps moves, and no guest page table changes for a write. Only a `MAP` or `UNMAP` over a private
page, which the guest sends, ends it. The device holds at most `private_max` private pages: a write
past them is refused, and the VM stops (cladefs's Firecracker: 159, `AccessRefused`). The guest keeps
under the bound, and writes back what it stored: its dirty pages' bytes, read from the window, are
`WRITE`s to new extents, then a commit; once a commit seals those, it rebinds the pages to them,
which frees the private pages. A commit changes nothing in any window.

The host's VMM takes KVM's faults on the window through userfaultfd, so the host's kernel must be
Linux 6.1 or later (TASK-50.4): `/dev/userfaultfd` (6.1), which a jailed VMM opens in its jail
rather than the host enabling `vm.unprivileged_userfaultfd`; write protection of shared memory
(`UFFD_FEATURE_WP_HUGETLBFS_SHMEM`, 5.19); and `MADV_POPULATE_WRITE` (5.14).

## Requests

```c
struct virtio_extent_id {
	le64 branch;
	le64 sequence;
};

struct virtio_extent_req {
	le32 type;
	le32 flags;
	le64 unique;          /* the driver's own, for tracing */
};

struct virtio_extent_resp {
	le32 status;          /* 0, or an errno */
	le32 reserved;
};
```

| Type | Name | Arguments | Data |
|---:|---|---|---|
| 1 | `READ` | `struct virtio_extent_id id; le64 offset; le32 len; le32 reserved;` | device writes `len` bytes |
| 2 | `WRITE` | `struct virtio_extent_id id; le64 offset; le32 len; le32 root_len;` | device reads `len` bytes, then, with `WRITE_ROOT`, `root_len` bytes: the new root |
| 3 | `FREE` | `struct virtio_extent_id id;` | none; with `FREE_MANY`, device reads `struct virtio_extent_id`s |
| 4 | `MAP` | `struct virtio_extent_id id; le64 offset; le64 len; le64 window_offset;` | none; with `MAP_MANY`, device reads `struct virtio_extent_map_entry`s |
| 5 | `UNMAP` | `le64 window_offset; le64 len;` | none |
| 6 | `COMMIT` | `le32 root_len; le32 reserved;` | device reads `root_len` bytes: the new root, if `COMMIT_ROOT` |
| 7 | `ROOT` | none | device writes `max_root` bytes, and `struct virtio_extent_root_len` |
| 8 | `INFO` | none | device writes `struct virtio_extent_info` |
| 9 | `RESIZED` | `le64 capacity;` (the `asked` it answers); flag `RESIZED_TOOK` (bit 0): it took it | none |
| 10 | `FREEZE` | none; flag `FREEZE_FROZEN` (bit 0): frozen, else thawed | none |
| 16 | `PRIVATE` | `le64 window_offset; le64 len;` | none |
| 17 | `STATS` | `le64 asked;` (the `stats_asked` it answers) | device reads the guest's stats, text, the rest of the request's data (Stats) |

Type 15 was writable DAX's `SEALED`, and `MAP`'s flag bit 0 its `MAP_WRITE`: retired, not reused.
Types 11 to 14 were a live follower's `ADVANCE`, `LET_GO`, `PIN` and `UNPIN`, and `struct
virtio_extent_commit_number` theirs (doc-8): gone with followers (board 8237), not reused.

### `WRITE`

Writes `len` bytes at `offset` in extent `id`. Both are multiples of `block_size`, and the end is
within `max_extent`.

- The extent must be the guest's own (its branch is `branch`), and open. The first write creates
  it.
- An extent's length is the end of its furthest write. Blocks within it that were never written
  read as zeros.
- The data is in the cell, and visible to reads and mappings, when the request completes. It is
  local, or uploaded, with the commit after it (see Ordering and durability).
- With `WRITE_BARRIER` (bit 0), the write follows a barrier, as if a `COMMIT` were submitted just
  before it: Fxfs's `PRE_BARRIER` writes.
- With `WRITE_COMMIT` (bit 1, which needs `F_WRITE_COMMIT`), a barrier follows the write, as if a
  `COMMIT` were submitted once it completes, uploaded with `WRITE_UPLOADED` (bit 2), as with
  `COMMIT_UPLOADED`; the request completes once that commit does. It seals the extent written too:
  Fxfs's `POST_BARRIER` writes, the last of a journal flush that ends a sync, one request rather
  than a `WRITE` and a `COMMIT`, each a round trip to the host.
- With `WRITE_ROOT` (bit 3, which needs `WRITE_COMMIT`), that commit also sets the branch's root,
  as `COMMIT_ROOT` does, to the `root_len` bytes after the data (at most `max_root`): a sync's
  journal write records how far the journal is written in the root it sets, with no request more
  (TASK-153.13). A `WRITE_ROOT` without `WRITE_COMMIT` fails with `EINVAL`.

**Sealing.** An extent is open from its first write until the first barrier submitted after a
write to it completes. That barrier seals it. Every
write to an extent must therefore complete before the next barrier is submitted: Fxfs writes an
extent's data, then commits the transaction that refers to it, so this holds.

### `READ`

Reads `len` bytes at `offset` in extent `id`. Both are multiples of `block_size`, and the range lies
within the extent's length. The extent is the guest's, one its branch inherited from the branch
it was forked from (Branches and forks). A `READ` sees
every `WRITE` completed before it was submitted. Writes to the window reach no extent: they are in
private pages until the guest writes them.

Reading an extent the cell doesn't hold, one never written or one freed, fails with `ENOENT`. The
guest's Fxfs reads such an extent as zeros where it may never have been written: preallocated
extents, and blocks of them not yet written (TASK-26.3).

### `FREE`

Frees extent `id`: nothing in the guest's filesystem refers to it any more. It need not be sealed.
Freeing an extent the cell doesn't hold succeeds: after a crash, the guest frees again what it may
have freed before. With `FREE_MANY` (`F_FREE_MANY`), the data names more extents, each freed as
`id` is, in order: the first that fails fails the request, those before it freed, and the guest
frees them all again. A guest frees the extents a flush lets go of in one request, rather than one
each: each request a round trip, which nested virtualization makes dear.

- Afterwards, reading or mapping it fails.
- The host deletes it once the commit after the `FREE` is uploaded and no other branch has it.
- The guest must not free an extent while requests on it are in flight, and never mints the same ID
  twice.

### `MAP`, `UNMAP` and `PRIVATE`

`MAP` maps `len` bytes at `offset` in extent `id` into the window at `window_offset`. All three are
multiples of `map_alignment`, and the range lies within the window.

- The extent is sealed: a `MAP` of an open extent fails with `EPERM`. A guest that takes an
  extent for sealed while the barrier that seals it is in flight maps it again once that barrier
  completes.
- Loads see the extent's contents. What of the mapping lies past the extent reads as zeros, as
  blocks within it never written do.
- The mapping is read-only to the extent: a write to one of its pages makes the page private
  (Shared memory), and the extent never changes.
- With `MAP_ZEROS` (bit 3), the range maps zeros, `id` and `offset` ignored: holes, and pages past
  a file's end, which a write then makes private pages of zeros, with nothing to copy.
- A `MAP` replaces whatever the window held there, freeing its private pages.

With `MAP_MANY` (bit 1, `F_MAP_MANY`), the data holds more maps, each a
`struct virtio_extent_map_entry { struct virtio_extent_map map; le32 flags; le32 reserved; }`: the
arguments of a `MAP` and its flags, `MAP_PREFAULT` and `MAP_ZEROS` alone, each mapped as a `MAP` of its own would be,
in any order. The first that fails fails the request, whichever others mapped, and the guest maps
them all again, one by one, to learn which failed. A guest maps the pieces of a file's chunk it
reads, scattered across extents as rewrites move its blocks, in one request rather than one each:
each request a round trip, which nested virtualization makes dear.

With `MAP_PREFAULT` (bit 2, `F_MAP_PREFAULT`), the guest accesses the pages it maps next, as a read
does: the device prefaults them, mapping them for the guest ahead of its accesses, those its host holds in memory, rather
than the hypervisor one first access at a time, which nested virtualization makes dear
(TASK-112.1). The flag changes nothing the guest sees but the time its first accesses take.

`UNMAP` makes `len` bytes of the window at `window_offset` inaccessible, freeing its private
pages.

`PRIVATE` makes `len` bytes of the window at `window_offset` private now, as writes would, all or
none, and prefaults them, as `MAP_PREFAULT` does: a guest about to
write many pages that aren't private yet, as a `write(2)` of them, makes them so in one round trip,
rather than a fault of the host's each. Pages already private stay as they are. It fails with
`ENOMEM` past `private_max`, and with `EINVAL` where nothing is mapped.

Every VM on a host that maps the same extent maps the same host pages, which is safe because
extents never change once sealed.

### `COMMIT`

A barrier: every request completed before the `COMMIT` was submitted is ordered before every
request submitted after it completes. It seals every extent written before it, and changes nothing
in any window.

- **With `COMMIT_ROOT`** (bit 1), the commit also sets the branch's root to the `root_len` bytes it
  carries (at most `max_root`), ordered with everything before it.
- **Without `COMMIT_UPLOADED`,** the commit completes once ordered: its order is fixed. It waits
  for no level past that: doc-2's requirement 2, under which fsync keeps its ordering but a crash
  may lose a suffix.
- **With `COMMIT_UPLOADED`** (bit 0), it completes once everything before it is uploaded, its
  segments acknowledged by the bucket; local, if the cell has no bucket. Unmount and `sync` use
  it.
- cladefs: the extent daemon's wire has a third level, which no guest's device asks for:
  `COMMIT_LOCAL` (`WRITE_LOCAL` for a write's commit), which completes once everything before it is
  local, synced to the host's disk, not waiting for the bucket: what a database on the daemon
  commits with, as SQLite's own on a local filesystem does (crates/sqlite_cladefs, the user's
  decision of 2026-11-09, TASK-301).

### `ROOT`

Reads the branch's root: the guest's filesystem starts there.

```c
struct virtio_extent_root_len {
	le32 len;          /* bytes of the root, 0 if the branch has none */
	le32 reserved;
};
```

The device writes the root into the first `len` bytes of the `max_root`-byte buffer. A new
filesystem's root is empty: formatting sets it.

### `INFO`

```c
struct virtio_extent_info {
	le64 capacity;     /* bytes the cell will hold for this branch */
	le64 used;         /* bytes of extents it holds for it */
	le64 available;    /* bytes it can still take */
	le64 next_sequence; /* one more than the highest sequence number the branch has written */
	le64 sequence_floor; /* the lowest sequence number the guest may mint from */
	le64 branch;       /* the branch the guest mints extents of */
};
```

The driver gives room for all of it (`EINVAL` otherwise: before TASK-141.3, a driver without
`F_FREEZE` had the fields before `branch` alone). The guest takes its branch from `INFO`, not the
configuration, which the driver read as it bound the device: a VM restored from a snapshot has a
branch of its own (Snapshots).

For `statfs`, and for the guest's own count of what it may write.

`next_sequence` counts every extent the branch wrote, freed or not, and recovers with the cell
after a crash. A crash may leave extents written whose references the guest's journal lost:
orphans. After replay, the guest mints from the largest of its own next sequence number,
`next_sequence` and `sequence_floor`, and frees the extents between its own and `next_sequence`
that nothing refers to.

`sequence_floor` is 0 unless the cell keeps its branch in object storage, where it is its
writer's range's start (doc-7, The head): IDs below it another writer may have minted. No extent
the cell holds need reach it, so the guest looks for orphans below `next_sequence` only: between
its own count and the floor there may be 2^40 numbers.

### Errors

| Status | When |
|---|---|
| `0` | Success. |
| `EPERM` (1) | A write to a sealed extent, or to another branch's; a `MAP` of an open extent; a `WRITE`, `FREE` or `COMMIT` of a public base's cell (TASK-43). |
| `ENOENT` (2) | A `READ` or `MAP` of an extent that does not exist, or was freed. |
| `EIO` (5) | The cell failed to read or write. |
| `EINVAL` (22) | An unknown type or flag, a misaligned or out-of-range offset, length or window offset, or a feature's flag the device does not offer. |
| `ENOMEM` (12) | A `PRIVATE` past `private_max`. |
| `ENOSPC` (28) | The cell cannot hold more for this branch. |
| `EROFS` (30) | The store stopped taking writes for this branch: another host took its head (milestone 4), or the host is shutting it down. Reads still work. |

## Ordering and durability

Requests on all queues are applied in any order, except across barriers. `COMMIT`s, and the
barriers of `WRITE_BARRIER`s and `WRITE_COMMIT`s, divide the requests into **intervals**:

- A request completed before a barrier was submitted belongs to an interval before it.
- A request submitted after a barrier completed belongs to an interval after it.
- A request in flight while a barrier is may fall on either side.
- Writes to the window belong to no interval: they are in private pages, which no crash keeps,
  until the guest `WRITE`s them.

After a crash of the guest, the VMM or the host, the cell holds:

- everything from the intervals before some barrier B, including the last root set before it;
- any subset of the writes and frees of B's interval, each block written whole or not at all;
- nothing after it.

That is how disks behave with barriers, and how Fxfs's journal recovers: checksums find where a torn
journal ends. With `COMMIT_UPLOADED`, B is at least that commit. How far B can lag the latest commit
is the host's lag, bounded by its configuration (doc-2, requirement 11).

## When the host's daemon goes

A host's extent daemon (cladefs's `cladefsd`) serves every VM's device on the host; the VMM
holds nothing of a cell itself. If the daemon goes, by a crash, a kill or running out of memory,
each VM sees it as follows (TASK-50.2):

- **At once:** the device's connection to the daemon closes. Every request waiting fails, as does
  every one made after, with `EIO`; the DAX window's faults are no longer served.
- **The VM stops.** The VMM is told as the connection closes, and stops the VM, with an exit
  status of its own (cladefs's Firecracker: 158, `CladefsdLost`), rather than let the guest go on
  with a disk whose every request fails. Its host's VM manager sees the VMM exit, and why.
- **What the guest committed** is what the daemon made local: everything before some barrier
  (see Ordering and durability), as after any crash. A cell whose daemon went is opened again by
  the next daemon, or another process, as of its last local commit; its branch in the bucket is the
  writer's as of its last upload.
- **Nothing reconnects.** A device does not connect to a daemon started again: its window's
  mappings, its private pages, the extents it had open and its requests in flight are not
  recovered. The VM is
  started again, from its cell or a snapshot, by its manager.
- **Grants** (per-tenant sockets) are not kept across the daemon's restarts: its manager grants
  them again, as the control API's documentation says.

## Branches and forks

Each VM's filesystem is a branch, with its own ID and root.

- **Forking** a branch makes a child with a new ID, a copy of the parent's root, and every extent
  the parent had at the fork. It takes O(1): the host records the parent and the fork point, in
  its bucket (crates/extent_bucket/src/branch.rs); a cell directory on the host holds one branch.
- **After a fork,** each branch mints extents under its own ID. A branch that frees an inherited
  extent drops only its own claim, and the host deletes the extent once every branch that has it has
  freed it.
- **Reads** of inherited extents, and mappings of them, are as of the fork. Extents never change, so
  this needs no copying.
- **No attached forks.** The host's control API once attached a fork of another cell to a branch
  (`AttachBase`, doc-10, removed 2026-11-27, board 14527), whose extents `READ` and `MAP` read
  through to. None does now: an extent no branch of the cell's ancestry holds fails with `ENOENT`.

The guest sees none of this except a different `branch` in its configuration after a fork.

## Snapshots

A VMM snapshots a VM with the device, and restores the snapshot, as many times as it likes, each
restored VM with a cell of its own (cladefs's TASK-39.1): a zygote's children, suspend-to-RAM, a
VM moved to another host. Extent IDs carry their branch, and the restored guest has the branch it
had in its memory, so the snapshot is taken when nothing is open:

- **Freezing.** The guest's filesystem freezes first (fsfreeze): it syncs, sealing what
  is open, holds its background work, and sends no `WRITE`, `FREE` or `COMMIT` until it thaws;
  `READ`, `MAP` of sealed extents and `UNMAP` go on. It tells the device with `FREEZE` and
  `FREEZE_FROZEN`. Freezing writes back every dirty page and rebinds every private page to the
  extent that now holds it, so that the window maps extents and zeros alone. A VMM refuses to
  snapshot a device whose guest isn't frozen, and the host refuses the snapshot's point while the
  window holds a private page, whose bytes no extent holds (`EBUSY`; cladefs's extent store, saying
  how many in the daemon's log, TASK-203): the guest's freeze couldn't rebind it, as when a
  writeback found no memory. cladefs's Firecracker takes the point before it records the window's
  mappings, so a refused snapshot leaves nothing behind.
- **The snapshot** holds the device's state, its window's place in guest memory and what each
  mapping of the window maps (an extent's bytes, or zeros), and a point of the cell: the host
  forks the cell, as of its last commit, which with nothing open is all the guest wrote.
- **A restore** opens a cell of its own, forked from the snapshot's point (or the cell the VM
  had, if nothing else changed it), and maps the window as the snapshot had it, at the same guest
  physical address. Its configuration's `branch` is the new cell's.
- **Thawing,** the guest sends `FREEZE` without the flag, and takes its `branch` from `INFO`, which
  it mints extents of from then on: its journal continues in new extents of it before anything
  else commits.

A guest that never freezes never sees a snapshot taken.

## The host's storage

This section describes how the host stores extents. It is not part of the protocol.

- **Packing.** Sealed extents smaller than 4 MiB are packed, in commit order, into **segments** of
  about 16-64 MiB. Extents of 4 MiB and more are stored on their own, as objects of their own.
- **The table.** A branch's checkpoint maps each extent ID to its segment and offset, or to its own
  object (doc-7, Checkpoints); the segments uploaded after it add to it.
- **Collecting.** The bucket's collector (`cladefs-gc`, crates/extent_bucket/src/collector.rs),
  the only thing that deletes, compacts objects that are mostly freed extents, copying the live
  small extents into new objects. Large extents are deleted whole and never copied. The host
  collects nothing locally (TASK-165.8).
- **On the host's disk,** a cell keeps its open segment, its write-ahead log and its own extents
  (doc-14), and uploads them to the bucket, in the layout doc-7 describes; a segment acknowledged
  there leaves the cell for the host's part cache.
- **Caches.** The extent cache holds sealed extents' pages in memory, which VMs map through their
  DAX windows; the part cache holds parts of the bucket's objects on the host's disk, which misses
  in memory read from before the bucket. Every VM, fork and point on the host that reads the same
  extent shares both.

## What Fxfs becomes

One on-disk format: extents only (TASK-26.3). The same Fxfs runs on the host, where cladefs builds
images and imports container images into cells (crates/cladefs/src/image.rs, import.rs), each a
local extent store, and in the guest.

| Fxfs | Becomes |
|---|---|
| Device offsets in extent records | `(extent ID, offset)` |
| The allocator: a free-space tree, best-fit, reservations against the device's size | An ID minter (the branch and a sequence number stored in its info), reference counts per extent, and reservations against `INFO`'s capacity |
| Trimming | `FREE`, sent when an extent's last reference is gone, the device has been flushed past that, and the journal records the flush (replay must not verify checksums of writes to extents freed) |
| Superblocks at fixed offsets | Written to new extents, which the root names |
| The journal's preallocated, appended extents | New extents for each journal flush |
| Overwrite mode for preallocated files | Copy-on-write |
| `Device::flush` | `COMMIT` |
| `PRE_BARRIER` writes | `WRITE_BARRIER` |
| `POST_BARRIER` writes (cladefs: a sync's journal flush, whose barrier is its flush) | `WRITE_COMMIT` |
| Closing | `COMMIT_UPLOADED` |
| cladefs's snapshots (frozen data owners) | References to the same extents |

Fxfs runs with barriers enabled.
