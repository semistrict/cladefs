# cladefs

cladefs is storage for fleets of virtual machines that start from the same few images: sandboxes
for agents, CI jobs, development environments. Object storage holds every disk durably; each host
keeps a cache of it on its disk and in its memory, as an operating system's page cache sits over a
disk. Each VM's disk is a cell: a small, independent object with one writer, its own log and its own
versions, forked from a base in constant time and written privately. What the forks have in common
is kept once: once in object storage, once on each host's disk, and once in each host's memory,
where every VM that reads a file maps the same pages.

A clade is an ancestor and all its descendants. Here it is a base image and every fork grown from
it, at any depth.

It has three parts:

- **In the guest**, Fuchsia's [Fxfs](https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0136_fxfs)
  filesystem, ported into the VM's own Linux kernel (`guest/`). Programs in the VM see an ordinary
  Linux filesystem.
- **On the host**, one extent daemon, `cladefsd`, which keeps every VM's data, caches it in memory
  of its own, and maps that memory straight into the VMs (DAX). A virtio device in Firecracker
  connects each guest to it.
- **In object storage**, a bucket (S3, Google Cloud Storage or Azure Blob Storage), which holds the
  durable copy of every disk. A host's local disk is its write-ahead log and its cache.

## Why

A host that runs hundreds of sandboxes from one image wants three things of their storage, and the
usual choices give up at least one.

- **A block device with ext4 in the guest** is fast for metadata, but each guest caches the files
  it reads in its own RAM: a hundred VMs hold a hundred copies of the same libraries. A block
  address means nothing outside its VM, so the host can share none of it.
- **virtio-fs with DAX** shares the host's page cache, but the filesystem is on the host, so every
  `stat`, `create` or `readdir` the guest has not cached is a round trip out of the VM. cladefs
  began this way, and measured creates, cold stats, readdirs and unlinks at 8 to 55 times ext4's
  cost under nested KVM (backlog doc-2).

cladefs splits the filesystem where each half is cheap:

- **Metadata lives in the guest.** The guest's Fxfs keeps its directories, inodes and journal
  itself, so a `stat` or a `create` is work in guest memory, as with ext4.
- **Data lives on the host, as extents.** An extent is an immutable piece of data, written once
  and named by an ID, not by an address. The guest reads, writes, maps, frees and commits extents
  through its extent store device, in place of a block device. The store has no fixed size and no
  free space to manage: it holds what was written.
- **Immutable extents can be shared.** The daemon maps a sealed extent's pages read-only into
  every guest that asks for it, so file data is one copy per host, not one per VM, and is in no
  guest's RAM. A fork is a record that names its parent's extents, so it copies nothing. A page a
  guest writes becomes its own private copy until it is written back to a new extent.

## What it does

- **Many small, independent cells.** Every disk is a cell: one writer at a time, its own log,
  head, key and versions, kept under its own prefix in the bucket. A cell has a kind: an Fxfs
  filesystem, a SQLite database (`crates/sqlite_extents`) or a VM's memory image. Cells share
  nothing but the extents a fork inherits, so any host can take any cell, whole.
- **Versions, local at once.** A version is a named state of a cell: one commit on the host that
  holds it, uploaded in the background. Only the cell's writer makes or drops one.
- **Forks in constant time**, from a version: a new cell that names its parent's version and copies
  nothing. Any number of hosts fork one version at once.
- **One copy of file data per host**, in the daemon's memory, shared by every VM that maps it and
  evicted under one memory budget.
- **Object storage as the home of the data.** A commit is on the host's disk within a second and
  in the bucket soon after; `sync` waits for the bucket. A host is disposable: another takes its
  cells from the bucket, and a fencing token keeps a host that was cut off from writing them again.
- **Container images as bases.** `cladefsctl import` makes a base from an OCI image layout or a
  `docker export` tarball, for VMs to boot forks of.
- **Many tenants on one host.** Each tenant has a socket, a memory arena, memory and disk limits
  and keys of its own. A VM process reaches only its own tenant's data.
- **Encryption at rest, always**, with a key per tenant in a KMS (OpenBao, AWS KMS or Google
  Cloud KMS), or in the bucket under a master key (`CLADEFS_MASTER_KEY`). Destroying a tenant's key
  shreds its data wherever it is. A host given neither refuses to start.
- **Linux's file semantics**: `mmap`, `O_DIRECT`, reflinks, `fallocate`, ACLs, extended attributes,
  idmapped mounts, freezing, online resize. `docs/code.md` lists what the guest's Fxfs does not do.
- **Access from the host.** `cladefs-mount` mounts a disk with FUSE, and the `cladefs` crate is a
  library that formats, fills and checks filesystems.
- **Shared volumes.** A read-only base, such as a Git checkout kept current, attached to a running
  VM under a writable layer of its own (backlog doc-10).
- **A control API** for the service that manages a host's VMs: gRPC on a Unix socket, with a Go
  client (`go/`). `cladefsctl` is the same from a shell.

## How it works

```
guest (Linux + Fxfs)     guest/: Fxfs in the kernel
  │  virtio extent store device: READ, WRITE, MAP, COMMIT...
Firecracker              vendor/firecracker: the device, a client of the daemon
  │  Unix socket; the daemon's memory mapped into the guest's DAX window
cladefsd                 crates/cladefs: the extent daemon, its extent cache
  │
local disk               crates/local_cell: each store's write-ahead log; the part cache
  │
bucket                   crates/bucket_cell: every cell's durable copy
```

- **A read.** The guest asks the device to `MAP` part of a sealed extent into its DAX window, a
  range of guest physical memory. The daemon finds the pages in its extent cache, or reads them in
  from local disk or the bucket, and maps them read-only. Two guests that map one extent map the
  same pages.
- **A write.** The guest's store to a mapped page copies it to a private page at the same address.
  Writeback sends dirty pages as `WRITE`s to new extents, and a `COMMIT` seals them and records
  the filesystem's new root. The guest then maps the pages back to the sealed extents, sharing
  them again.
- **A commit.** `fsync` returns once the daemon has the commit in order. The daemon syncs it to
  local disk within a second (50 ms at the median), then packs sealed extents into segments and
  uploads them. A crash loses a suffix of recent history and recovers to a commit, never to a
  torn state (backlog doc-12).
- **A version and a fork.** A cell is a line of commits with one writer at a time. A version names
  one of them, made by a local commit and uploaded in the background. Forking makes a new cell that
  names a parent's version and copies nothing. The collector deletes an extent from the bucket once
  no cell or version holds it.

`docs/architecture.md` has the detail, and `CONTEXT.md` defines every term.

## Status

LangSmith's sandboxes are its first user: their team runs cladefs at a commit they pin, and each
candidate for the next pin is qualified against the current one under their load
(`docs/testing.md`).

- **Hosts:** Linux 6.1 or later with KVM, on x86-64 and aarch64.
- **Guests:** Amazon Linux's 6.18 kernel with cladefs's patches and Fxfs built in
  (`scripts/build-guest-kernel.sh`).
- **VMM:** Firecracker 1.16.1 with cladefs's series of changes (`vendor/firecracker`, a
  submodule). `crates/virtio_extent` is the device without its transport, for another VMM to host.
- **macOS:** the workspace builds and its tests run there. VMs and FUSE mounts need Linux.
- **Formats change without migration.** On-disk, bucket and wire formats change when that is the
  better design, and new code refuses old data instead of converting it.
- **No single-node mode.** The daemon needs a bucket and a fencing token to start. For development
  the bucket may be a directory (`file:///DIR`).
- **Built but frozen:** live read-only followers of a mounted disk (backlog doc-8).

## Try it

Build and test, on macOS or Linux:

```sh
git submodule update --init
cargo build
scripts/test.sh
```

Make a base from a container image's files and fork it. This needs no VM and no daemon:

```sh
export CLADEFSD_BUCKET=file:///tmp/cladefs/bucket CLADEFSD_TOKEN=1
docker export "$(docker create alpine)" > /tmp/alpine.tar
cargo run --bin cladefsctl -- import /tmp/cladefs/base 1073741824 /tmp/alpine.tar
cargo run --bin cladefsctl -- fork /tmp/cladefs/base /tmp/cladefs/vm1
cargo run --bin cladefsctl -- stat /tmp/cladefs/vm1
```

`stat` shows the fork's bytes as `inherited`: it holds the base's extents and wrote none.

Boot guests, on Linux with KVM. This builds the guest kernel and Firecracker, starts a daemon and
runs the guest tests, each of which boots VMs that mount Fxfs from an extent store device:

```sh
scripts/test-guest-kernel.sh   # on macOS it runs in a Lima VM, which scripts/lima-setup.sh makes
```

`docs/testing.md` has the other suites, and `docs/operating-a-host.md` the access to
`/dev/userfaultfd` that DAX needs.

To run it under a VM manager of your own, read `docs/integrator-guide.md`.

## What is where

| Path | What |
|---|---|
| `crates/cladefs` | The extent daemon (`cladefsd`), `cladefsctl`, `cladefs-mount`, and the host's library. |
| `crates/local_cell`, `crates/bucket_cell` | An extent store on local disk, and cells in the bucket. |
| `crates/physmem` | The daemon's physical memory: arenas of shared memory. |
| `crates/virtio_extent` | The extent store device, for a VMM to host. |
| `crates/fxfs`, `crates/fxfs-posix` | Fxfs, copied from Fuchsia, and the POSIX layer the guest and the host both call. |
| `crates/` (the rest) | Fuchsia's libraries, copied, and stand-ins for what only Fuchsia's build has (`docs/code.md`). |
| `guest/` | Fxfs in the guest's kernel: its C glue, its Rust, the kernel patches and the guest tests. |
| `vendor/firecracker` | cladefs's Firecracker, a submodule. |
| `go/` | Go clients of the daemon's APIs. |
| `specs/` | TLA+ specs of the protocols whose bugs are rare interleavings. |
| `guide/` | An interactive guide to the code: `cd guide && pnpm install && pnpm dev`. |
| `backlog/` | Tasks and design docs, managed with Backlog.md. What remains to do is there. |
| `dead-ends/` | Approaches tried and given up on, each with why. |

## Documentation

- `CONTEXT.md`: the glossary. One word per concept.
- `docs/architecture.md`: requests, the DAX window, the extent daemon and whom it trusts.
- `docs/operating-a-host.md`: the daemon's memory and disk, the bucket, encryption and shredding,
  VM processes' exit statuses.
- `docs/integrator-guide.md`: for whoever writes the VM manager, call by call.
- `docs/upgrading-a-host.md`: draining and upgrading a host.
- `docs/code.md`: where the code comes from, crate by crate; the Firecracker series; the guest's
  kernel; updating from Fuchsia.
- `docs/testing.md`: the test suites, the qualification of a candidate, the nightly measurement,
  benchmarks.
- `backlog/docs`: the designs. Start with doc-2 (why Fxfs is in the guest), doc-4 (the device),
  doc-13 (copy-on-write DAX), doc-7 and doc-14 (the bucket and local disk), doc-12 (what an `fsync`
  survives) and doc-16 (encryption).
- `AGENTS.md`: how work on cladefs is done.

cladefs was fxdax (Fxfs plus DAX, the first prototype's name) until TASK-437. Records of what ran
before, such as `docs/measurements` and closed backlog tasks, keep the old name, and
`scripts/rename-to-cladefs.sh` renames a cell begun before.

## Licenses

- **The workspace:** BSD-3-Clause, as is Fuchsia (`LICENSE`, `PATENTS`,
  `AUTHORS`). What was copied or adapted from Fuchsia keeps The Fuchsia
  Authors' copyright; cladefs's own code, and its changes, are The cladefs
  Authors'.
- **`physmem`:** MIT (its `LICENSE`), as the Zircon kernel port it was lifted
  from was (deleted, TASK-96.1), keeping the copyrights of the sources it
  ports.
- **The parts of Rust's std imported into the guest's
  (`scripts/import-guest-std.py`):** MIT OR Apache-2.0, as Rust is (the
  licenses and Rust's `COPYRIGHT` in `guest/rust/std`).
- **`guest/linux`:** GPL-2.0 or BSD-3-Clause, built into the guest's Linux
  kernel (GPL-2.0), which `scripts/build-guest-kernel.sh` downloads; its
  patches to the kernel are GPL-2.0, as the kernel is.
- **The Unicode tables (`fxfs-unicode-gen`):** generated from the Unicode
  Character Database, under the Unicode license (`LICENSE-UNICODE`).
- **Firecracker (`vendor/firecracker`):** Apache-2.0.
- **The guest tests' harness (`guest/tests`):** libkrun's test framework,
  Apache-2.0 (`guest/tests/LICENSE.libkrun`), its files keeping libkrun's
  license; the test cases cladefs wrote are its own, BSD-3-Clause.
- **Amazon Linux's kernel:** whose sources `scripts/build-guest-kernel.sh`
  builds the guest kernel from, GPL-2.0, with Firecracker's configuration of
  them (`guest/linux/configs`, from Firecracker's main cell: Apache-2.0).

Each source file's header says which applies to it: `make
check-license-headers` checks that each has the header its origin needs
(`scripts/license-headers.py`), and `make fix-license-headers` adds those
missing.
