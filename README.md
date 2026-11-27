# cladefs

cladefs is storage for fleets of virtual machines that start from the same few images: sandboxes
for agents, CI jobs, development environments. Object storage holds every disk durably; each host
keeps a local write-ahead log, part cache and extent cache in front of it, as an operating system's
page cache sits over a disk. What forked VMs have in common is kept once: once in object storage,
once on each host's disk, and once in each host's memory, where every VM that reads a file maps the
same pages.

A clade is an ancestor and all its descendants. Here it is a base image and every fork grown from
it, at any depth.

It has three parts:

- **In the guest**, Fuchsia's [Fxfs](https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0136_fxfs)
  filesystem, ported into the VM's own Linux kernel (`guest/`). Programs in the VM see an ordinary
  Linux filesystem.
- **On the host**, one extent daemon, `cladefsd`, which keeps every VM's data, caches it in memory
  of its own, and maps that memory straight into the VMs (DAX). A virtio extent store device in
  Firecracker connects each guest to it.
- **In object storage**, one or more buckets (S3, Google Cloud Storage or Azure Blob Storage, one
  per cloud region), which hold the durable copy of every cell. A host's local disk is its
  write-ahead log and its part cache.

## Why

A host that runs hundreds of sandboxes from one image wants three things of their storage, and the
usual choices give up at least one.

- **A block device with ext4 in the guest** is fast for metadata, but each guest caches the files
  it reads in its own RAM: a hundred VMs hold a hundred copies of the same libraries. A block
  address means nothing outside its VM, so the host can share none of it.
- **virtio-fs with DAX** shares the host's page cache, but the filesystem is on the host, so every
  `stat`, `create` or `readdir` the guest has not cached is a round trip out of the VM. cladefs
  began this way, and measured creates, cold stats, readdirs and unlinks at 8 to 55 times ext4's
  cost under nested KVM ([doc-2](<docs/design/doc-2 - Guest-owned-metadata-Fxfs-in-the-guest-kernel-over-a-virtio-extent-store.md>)).

cladefs splits the filesystem where each half is cheap:

- **Metadata lives in the guest.** The guest's Fxfs keeps its directories, inodes and journal
  itself, so a `stat` or a `create` is work in guest memory, as with ext4.
- **Data lives on the host, as extents.** An extent is an immutable piece of data, written once
  and named by an ID, not by an address. The guest reads, writes, maps, frees and commits extents
  through its extent store device, in place of a block device. The cell has no fixed size and no
  free space to manage: it holds what was written.
- **Immutable extents can be shared.** The daemon maps a sealed extent's pages read-only into
  every guest that asks for it, so file data is one copy per host, not one per VM, and is in no
  guest's RAM. A fork names its parent's extents and copies nothing. A page a guest writes becomes
  its own private copy until writeback seals a new extent and maps the page back to it.

**Why Fxfs in the guest?** Porting Fuchsia's Fxfs into Linux rather than modifying ext4 or XFS is
deliberate: Fxfs is written in Rust, log-structured and copy-on-write, and sits over an abstract
storage trait rather than assuming a mutable fixed-size block device. That lets it map directly
onto immutable extents without rewriting a C filesystem's block allocator.

## Explainer films

Short animated films, no voice (`explainer/`):

- [How cladefs works](https://storage.googleapis.com/echophase-cladefs-explainers/cladefs-overview.mp4), 132 s
- [Encryption at rest and shredding](https://storage.googleapis.com/echophase-cladefs-explainers/cladefs-encryption.mp4), 62 s
- [Many regions: cells across buckets](https://storage.googleapis.com/echophase-cladefs-explainers/cladefs-regions.mp4), 72 s
- [Shared volumes](https://storage.googleapis.com/echophase-cladefs-explainers/cladefs-shared-volumes.mp4), 72 s
- [Container images as bases](https://storage.googleapis.com/echophase-cladefs-explainers/cladefs-images.mp4), 62 s

## Core model

Four abstractions carry the whole design ([CONTEXT.md](CONTEXT.md) defines every term):

- **Extent:** an immutable piece of data, written while open and never modified once a barrier or
  `COMMIT` seals it. Sealed extents are packed into compressed, encrypted segments for the bucket
  and mapped read-only into guests via DAX.
- **Cell:** a tenant's independent unit of storage, concurrency, placement, fencing and encryption:
  one writer at a time, its own log, head, key and versions. A cell has one kind—an Fxfs
  filesystem, a SQLite database (`crates/sqlite_cladefs`) or a VM's memory image. Any host can
  take any cell whole.
- **Version:** a named read-only state of a cell in its past, made by a single local commit on the
  cell's writer and uploaded in the background.
- **Fork:** a new cell created in O(1) from a parent's version, inheriting its extents without
  copying them. Any number of hosts can fork one version at once, including across buckets in
  different cloud regions. An **ephemeral cell** is a leaf fork whose own writes stay on one host's
  local disk, never uploaded, and move to another host by live hand-over ([doc-25](<docs/design/doc-25 - Ephemeral-cells-host-local-children-of-a-version-migrated-live.md>)).

## How it works

```
guest (Linux + Fxfs)     guest/: Fxfs in the kernel; DAX reads and writes
  │  virtio extent store device: READ, WRITE, MAP, UNMAP, FREE, COMMIT
Firecracker              vendor/firecracker: the device, a client of the daemon
  │  Unix socket; shared memfd arenas mirrored into KVM via userfaultfd
cladefsd                 crates/cladefs: extent cache in part slabs, DAX windows, peer network
  │  O_DIRECT / F_NOCACHE (one memory budget, one disk governor)
local disk               crates/local_cell: open segment, write-ahead log, part cache
  │  async framing & upload (or optional Fast WAL to appendable zonal object storage)
bucket(s)                crates/bucket_cell: durable cells across S3 / GCS / Azure, cladefs-gc
```

- **A read (DAX, no host kernel module).** The guest asks the extent store device to `MAP` part of
  a sealed extent into its DAX window, a range of guest physical address space. `cladefsd` serves
  the pages from its extent cache (in shared `memfd` arenas)—or fetches missing 1 MiB parts from
  the local part cache, a peer daemon in the same cloud region, or the bucket—and mirrors them
  read-only into the VM process using `userfaultfd` and `KVM_PRE_FAULT_MEMORY` on stock Linux
  6.1+. Two VMs mapping one extent map the same physical host pages.
- **A write (copy-on-write DAX).** A guest write to a mapped read-only page faults through
  `userfaultfd` and copies it to a VM-private page at the same address (bounded per VM by
  `private_max`, charged to the VM process rather than the daemon). Writeback sends dirty pages as
  `WRITE`s to new open extents, `COMMIT` seals them and records the filesystem's new root, and the
  guest rebinds the pages back to the sealed extents so they can be shared again ([doc-13](<docs/design/doc-13 - Copy-on-write-DAX.md>)).
- **A commit (durability levels).** Commits progress through four levels ([doc-12](<docs/design/doc-12 - The-fsync-durability-contract.md>)):
  1. **Ordered:** `cladefsd` has the commit in order in memory (default for `fsync`).
  2. **Local:** synced to the host's disk (within a second, 50 ms at the median).
  3. **Fast WAL** *(optional, per cell)*: appended via an offset-checked write to an appendable
     zonal object store (Amazon S3 Express One Zone today)—surviving host loss with zero local
     disk `fsync`s ([doc-29](<docs/design/doc-29 - Fast-WAL-commits-appendable-object-storage-without-local-state.md>)).
  4. **Uploaded:** sealed extents framed into compressed, encrypted segments, uploaded to the
     bucket, and acknowledged by a conditional write of the cell's head (`sync` waits for
     Uploaded, or Fast WAL on a Fast WAL cell).

  A crash loses at most a suffix of recent unacknowledged history and recovers to a commit, never
  to a torn state.
- **Versions, forks, buckets and collection.** Making a version closes the open segment locally
  with no bucket round trip; uploading and recording it in the head happen asynchronously. When a
  cell in one cloud region forks a version from another bucket, it reads the parent's bucket lazily
  while a background mirror copies the parent version's objects byte for byte into the child's
  bucket ([doc-23](<docs/design/doc-23 - Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md>)).
  The collector (`cladefs-gc`) is the only component that deletes from a bucket, removing extents
  once no cell, version or ephemeral-cell reference holds them.

See [docs/architecture.md](docs/architecture.md) for the full mechanics.

## What it does

### Storage, branching and snapshots

- **Three cell kinds:** Fxfs filesystems, SQLite databases (`crates/sqlite_cladefs`), and VM memory
  images, the last two each a block device kept on the cell's extents (`crates/block_device`).
- **Host-local versions and O(1) forks:** make a version of a cell locally and fork it in constant
  time on any number of hosts.
- **VM snapshots:** pause a VM, make a version of its disk cell and memory-image cell locally with
  no bucket round trip, and restore that VM snapshot on any host ([docs/integrator-guide.md](docs/integrator-guide.md)).
- **Ephemeral cells with live hand-over:** host-local forks over a bucket version for disposable
  sandbox writes that never touch the bucket, with pre-copy live migration to another host
  ([doc-25](<docs/design/doc-25 - Ephemeral-cells-host-local-children-of-a-version-migrated-live.md>)).
- **Container images as bases:** `cladefsctl import` turns an OCI image layout or `docker export`
  tarball into a base cell for VMs to boot forks of.

### Durability, caching and multi-region

- **Object storage as the home of the data:** hosts are disposable; another host takes a cell from
  its bucket, and fencing tokens keep a partitioned host from writing it again.
- **Multi-region buckets:** one bucket per cloud region across S3, GCS and Azure Blob Storage,
  coordinated by a shared bucket registry, with cross-bucket forks and automatic mirroring
  ([doc-23](<docs/design/doc-23 - Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md>)).
- **Fast WAL commits:** optional appendable zonal object storage (S3 Express One Zone) so commits
  survive host loss before cold-bucket upload completes, with stateless compute hosts
  ([doc-29](<docs/design/doc-29 - Fast-WAL-commits-appendable-object-storage-without-local-state.md>)).
- **One memory budget and one disk governor:** every data file and cache bypasses the host OS page
  cache (`O_DIRECT` on Linux, `F_NOCACHE` on macOS). `cladefsd` manages its extent cache, cells'
  page cache, uploads and fetches under one memory governor, and bounds local cells and the part
  cache under one disk governor ([doc-22](<docs/design/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md>)).
- **Peer part caches:** daemons in the same cloud region serve cached 1 MiB bucket parts to each
  other over the peer network before going to object storage.

### Multi-tenancy and security

- **Per-tenant isolation:** each tenant gets its own Unix socket, its own shared-memory (memfd)
  arena (plus read-only access to the default arena for public bases), memory and disk limits, and
  keys. A VM process reaches only its own tenant's data.
- **Encryption at rest, always:** envelope encryption with a tenant KEK in OpenBao, AWS KMS or
  Google Cloud KMS—or in the bucket under a master key (`CLADEFS_MASTER_KEY`). A daemon given
  neither refuses to start. Destroying a tenant's or cell's key cryptographically shreds its data
  everywhere ([doc-16](<docs/design/doc-16 - Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-vdisk-DEKs.md>)).

### Linux semantics and host tooling

- **Linux file semantics:** `mmap`, `O_DIRECT`, reflinks, `fallocate`, ACLs, extended attributes,
  idmapped mounts, freezing and online resize. [docs/code.md](docs/code.md) lists what the guest's
  Fxfs does not do.
- **Host access and control API:** `cladefsd`'s Mount service exposes cells on the host via
  FUSE; `cladefs` is a Rust library for formatting, populating and checking images;
  a gRPC control API on a Unix socket—with a Go client (`go/`) and the `cladefsctl` CLI—drives the
  daemon for a VM manager.

## Verification

Storage protocols fail on rare interleavings, crashes and degraded networks, so cladefs is tested
at several layers ([docs/testing.md](docs/testing.md)):

- **Formal specifications:** TLA+ models (`specs/`) cover the bucket protocol,
  multi-bucket mirroring, fast WAL, ephemeral hand-over, DAX copy-on-write and the journal—each
  paired with spec mutations that TLC must reject (`make check-tla`).
- **Deterministic simulation:** single-threaded seeded simulations test both the storage engine
  (`local_cell::sim`) and multi-daemon clusters under `madsim` ([doc-24](<docs/design/doc-24 - Full-stack-deterministic-simulation-N-daemons-one-seed.md>)), injecting crashes, torn disks, network partitions and failing buckets.
- **Concurrency and memory ordering:** `shuttle` explores thread schedules and `loom` verifies
  weak-ordering atomic protocols ([docs/memory-ordering.md](docs/memory-ordering.md)).
- **Mutation and differential testing:** hand mutants (`crates/*/mutants`, `guest/mutants`) put
  past bugs back to prove tests catch them; FUSE mounts run differential tests against `tmpfs` and
  `pjdfstest`.
- **KVM guest suites and qualification:** end-to-end tests boot Firecracker VMs with the guest
  kernel, and release candidates are qualified against LangSmith's production pin in interleaved
  A/B runs under strict memory, disk-write and CPU budgets.

## Status

LangSmith's sandboxes are its first user: their team runs cladefs at a commit they pin, and each
candidate for the next pin is qualified against the current one under their load
([docs/testing.md](docs/testing.md)).

- **Hosts:** Linux 6.1 or later with KVM, on x86-64 and aarch64.
- **Guests:** Amazon Linux's 6.18 kernel with cladefs's patches and Fxfs built in
  (`scripts/build-guest-kernel.sh`).
- **VMM:** Firecracker 1.18.0 with cladefs's series of changes (`vendor/firecracker`, a
  submodule). `crates/virtio_extent` is the device without its transport, for another VMM to host.
- **macOS:** the workspace builds and its tests run there. VMs and FUSE mounts need Linux.
- **Formats change without migration:** on-disk, bucket and wire formats change when that is the
  better design, and new code refuses old data instead of converting it.
- **No single-node mode:** the daemon needs a bucket and a fencing token to start. For development
  the bucket may be a directory (`file:///DIR`).
- **No live followers:** read-only mounts tailing a disk another VM writes were built, frozen and
  removed (2026-11-22, board 8237; [doc-8](<docs/design/doc-8 - Live-read-only-followers-of-a-mounted-Fxfs.md>)).
- **No shared volumes:** attaching a read-only base to a running VM's cell, as an Fxfs volume over a
  base, was removed (2026-11-27, board 14527; [doc-10](<docs/design/doc-10 - Shared-volumes-Fxfs-volumes-over-read-only-bases.md>)):
  cells, versions and forks serve.

## Try it

Build and test, on macOS or Linux:

```sh
git submodule update --init
cargo build
scripts/test.sh
```

Make a base from a container image's files and inspect it offline (no VM and no daemon needed):

```sh
export CLADEFSD_BUCKET=file:///tmp/cladefs/bucket CLADEFSD_TOKEN=1
export CLADEFS_MASTER_KEY=$(openssl rand -hex 32)   # cells are always encrypted
docker export "$(docker create alpine)" > /tmp/alpine.tar
# The host's one daemon owns its cells, which cladefsctl reaches through its control socket.
export CLADEFSD_GRPC=/tmp/cladefs/control.sock CLADEFSD_ROOTS=/tmp/cladefs/disk0
# 1 GiB of memory given, so no memory governor (and its host setup) is needed.
mkdir -p /tmp/cladefs/disk0 && cargo run --bin cladefsd -- /tmp/cladefs/vms.sock 1073741824 &
base=$(cargo run --bin cladefsctl -- import 1073741824 /tmp/alpine.tar)
cargo run --bin cladefsctl -- stat "$base"
fork=$(cargo run --bin cladefsctl -- fork "$base")
cargo run --bin cladefsctl -- stat "$fork"
```

`stat` on the fork shows its bytes as `inherited`, holding the base's extents without copying them.

Boot guests, on Linux with KVM. This builds the guest kernel and Firecracker, starts a daemon and
runs the guest tests, each of which boots VMs that mount Fxfs from an extent store device:

```sh
scripts/test-guest-kernel.sh   # this host, Linux with KVM; scripts/test-host.sh runs it on a test host
```

[docs/testing.md](docs/testing.md) has the other suites, and [docs/operating-a-host.md](docs/operating-a-host.md)
the access to `/dev/userfaultfd` that DAX needs. To run cladefs under a VM manager of your own,
read [docs/integrator-guide.md](docs/integrator-guide.md).

## What is where

| Path | What |
|---|---|
| `crates/cladefs` | The extent daemon (`cladefsd`), `cladefsctl`, and the host's library. |
| `crates/local_cell`, `crates/bucket_cell` | An extent store on local disk, and cells in the bucket (including Fast WAL and `cladefs-gc`). |
| `crates/virtio_extent` | The extent store device, transport-independent, for a VMM to host. |
| `crates/sqlite_cladefs`, `crates/block_device` | SQLite over a block device kept on extents, and the block device itself (one a SQLite or VM memory-image cell). |
| `crates/fxfs`, `crates/fxfs-posix` | Fxfs, copied from Fuchsia, and the POSIX layer the guest and the host both call. |
| `crates/` (the rest) | Fuchsia's libraries, copied, and stand-ins for what only Fuchsia's build has ([docs/code.md](docs/code.md)). |
| `guest/` | Fxfs in the guest's kernel: its C glue, its Rust, the kernel patches and the guest tests. |
| `vendor/firecracker` | cladefs's Firecracker, a submodule. |
| `go/` | Go clients of the daemon's APIs. |
| `specs/` | TLA+ specs of the protocols whose bugs are rare interleavings. |
| `guide/` | An interactive guide to the code: `cd guide && pnpm install && pnpm dev`. |
| `explainer/` | The explainer films' sources (`explainer/README.md`); links above. |
| `docs/design/`, `docs/decisions/` | Design docs and decisions. Tasks, what remains to do included, are on the team board ([AGENTS.md](AGENTS.md), Tasks). |
| `dead-ends/` | Approaches tried and given up on, each with why. |

## Documentation

- [CONTEXT.md](CONTEXT.md): the glossary. One word per concept.
- [docs/architecture.md](docs/architecture.md): requests, the DAX window, the extent daemon, ephemeral cells and whom it trusts.
- [docs/operating-a-host.md](docs/operating-a-host.md): the daemon's memory and disk, buckets, Fast WAL, encryption and shredding, VM processes' exit statuses.
- [docs/integrator-guide.md](docs/integrator-guide.md): for whoever writes the VM manager, call by call.
- [docs/upgrading-a-host.md](docs/upgrading-a-host.md): draining and upgrading a host.
- [docs/code.md](docs/code.md): where the code comes from, crate by crate; the Firecracker series; the guest's kernel; updating from Fuchsia.
- [docs/testing.md](docs/testing.md): the test suites, the qualification of a candidate, the nightly measurement, benchmarks.
- [docs/memory-ordering.md](docs/memory-ordering.md): atomic protocols and `loom` models.
- `docs/design/`: the design docs—start with [doc-2](<docs/design/doc-2 - Guest-owned-metadata-Fxfs-in-the-guest-kernel-over-a-virtio-extent-store.md>) (why Fxfs is in the guest), [doc-4](<docs/design/doc-4 - Virtio-extent-store-device-specification.md>) (the device), [doc-13](<docs/design/doc-13 - Copy-on-write-DAX.md>) (copy-on-write DAX), [doc-7](<docs/design/doc-7 - Object-storage-how-the-host-keeps-branches-in-S3-and-GCS.md>) and [doc-14](<docs/design/doc-14 - Local-disk-as-the-buckets-write-ahead-log-and-cache-one-storage-engine.md>) (the bucket and local disk), [doc-12](<docs/design/doc-12 - The-fsync-durability-contract.md>) (what an `fsync` survives), [doc-16](<docs/design/doc-16 - Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-vdisk-DEKs.md>) (encryption), [doc-23](<docs/design/doc-23 - Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md>) (multi-region buckets), [doc-25](<docs/design/doc-25 - Ephemeral-cells-host-local-children-of-a-version-migrated-live.md>) (ephemeral cells) and [doc-29](<docs/design/doc-29 - Fast-WAL-commits-appendable-object-storage-without-local-state.md>) (Fast WAL).
- [AGENTS.md](AGENTS.md): how work on cladefs is done.

cladefs was fxdax (Fxfs plus DAX, the first prototype's name) until TASK-437. Records of what ran
before, such as `docs/measurements` and tasks closed before it, keep the old name.

## Licenses

- **The workspace:** BSD-3-Clause, as is Fuchsia (`LICENSE`, `PATENTS`,
  `AUTHORS`). What was copied or adapted from Fuchsia keeps The Fuchsia
  Authors' copyright; cladefs's own code, and its changes, are The cladefs
  Authors'.
- **The parts of Rust's std imported into the guest's
  (`scripts/import-guest-std.py`):** MIT OR Apache-2.0, as Rust is (the
  licenses and Rust's `COPYRIGHT` in `guest/rust/std`).
- **`guest/linux`:** GPL-2.0 or BSD-3-Clause, built into the guest's Linux
  kernel (GPL-2.0), which `scripts/build-guest-kernel.sh` downloads; its
  patches to the kernel are GPL-2.0, as the kernel is.
- **Firecracker (`vendor/firecracker`):** Apache-2.0.
- **The guest tests' harness (`guest/tests`):** libkrun's test framework,
  Apache-2.0 (`guest/tests/LICENSE.libkrun`), its files keeping libkrun's
  license; the test cases cladefs wrote are its own, BSD-3-Clause.
- **Amazon Linux's kernel:** whose sources `scripts/build-guest-kernel.sh`
  builds the guest kernel from, GPL-2.0, with Firecracker's configuration of
  them (`guest/linux/configs`, from Firecracker's main branch: Apache-2.0).

Each source file's header says which applies to it: `make
check-license-headers` checks that each has the header its origin needs
(`scripts/license-headers.py`), and `make fix-license-headers` adds those
missing.
