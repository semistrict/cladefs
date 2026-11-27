# Where the code comes from

What each part of the tree is, and whose: Fuchsia's, a stand-in for something of Fuchsia's, or
cladefs's own. `README.md` has the map of the top-level directories; how the parts work together
is `architecture.md`.

Code falls into three kinds:

- Fuchsia sources, copied;
- stand-ins for what exists only inside Fuchsia's build;
- cladefs's own code.

All of it lives under `crates/`. Code copied, ported or excerpted from Fuchsia
follows the revision in the root `Cargo.toml`'s
`workspace.metadata.fuchsia.commit`.

## Fuchsia, copied

Most crates are Fuchsia's own, with only the files the workspace builds, and
their tests:

- Fxfs, which cladefs owns outright since board 8246: no upstream change is
  brought over to it any more;
- its libraries (`storage_device`, `storage_units` and others);
- the Rust runtime crates (`fuchsia-async`, `fuchsia-sync`): the runtime seam
  Fxfs runs on in the guest's kernel (`guest/rust/linux-rt`), on the host, and
  under Shuttle and madsim.

Each crate's `Cargo.toml` cites the Fuchsia directory it is copied from. Most
files are as upstream wrote them. Those that aren't say how they deviate in
their leading comment:

- The runtime crates have no Fuchsia runtime, handles or Zircon: what only
  Fuchsia builds is removed, as are FIDL protocols, Inspect and tests that need
  Fuchsia's component framework. Tests run on `fuchsia-async`'s own test
  macros.
- Fxfs encrypts nothing (no Fxfs keys, fscrypt or encrypted stores: cladefs
  encrypts below it, in its extents and buckets), has no fsverity or blobs, and
  has no casefold directories, overwrite/preallocation extents, project quotas,
  image-builder mode, or per-volume byte limits.
- `storage_device` has no `FileBackedDevice`: cladefs has its own image device
  (see `crates/cladefs`).
- Fxfs volume snapshots (`RootVolume::snapshot_volume`) were removed (board 11695,
  board 2578) — cell forks replace per-volume snapshots (`doc-10`). Frozen
  version-66 serialized fields stay in place for read compatibility, and non-empty
  snapshot metadata is refused at open.

## `crates/cladefs/src/slabs.rs`: physical memory

The extent cache's parts, and the zeros DAX windows map, are host shared
memory: physical memory in arenas, each its own shared memory object (a memfd
on Linux, an unlinked temporary file on macOS), which a process given an
arena's object maps the arena's pages from (TASK-43). An arena is 1 MiB part
slabs: a whole part takes a whole slab, a shorter one a run of a small slab's
pages, punched out as it is freed but for two whole slabs kept spare (doc-26).
The daemon maps none of it: it reads and writes slabs by offset. Arenas may be
capped, each and together (TASK-50.3); an arena starved at a cap asks for
relief, which evicts cached extents' parts; and a slab a VM process was staged,
freed, waits in quarantine until each VM process it was staged to has made what
took it away (TASK-79.9, doc-26). It replaced `crates/physmem`, the Zircon
kernel port's 4 KiB page allocator, in board 6204.

## `crates/fxfs-posix`: Fxfs's files and directories

One POSIX layer over Fxfs's object stores, which the guest kernel's VFS calls
(`guest/rust/cladefs-linux`) and the host's filesystem calls as a library
(`crates/cladefs`), generic over what differs between them (`Platform`): the
guest's, Fxfs in the kernel, the host's, images being built.

## `crates/`: stand-ins for Fuchsia-only pieces

None remain (the last, `fxfs-unicode-gen`, went with casefold directories in
board 10598).

## `crates/cladefs`: cladefs's own code

`cladefs` holds the extent daemon and the host's side of Fxfs. Its crate docs
(`cargo doc -p cladefs --open`) are the guide.

- `extents.rs` holds `OpenCell`, an extent store as a VM's device serves
  it: its I/O, and `MAP` and `UNMAP` into its DAX window.
- `cache.rs` holds the cache of sealed extents' pages, shared by every open cell
  in the process, each extent a table of its pages, read in a part at a time
  and evicted as memory runs out.
- `cladefsd/` holds the daemon (`bin/cladefsd.rs`) and its client,
  `CellClient`, which a VMM's device is; see `architecture.md`, The extent daemon.
- `dax.rs` holds DAX windows, their mappings and the pages they have mapped,
  and the `SecondStage` trait that follows a window's pages: the daemon's connection to a VM process implements it;
  `dax/uffd.rs` is the VM process's own second stage, in the host memory that
  KVM follows (`architecture.md`, DAX on Linux).
- `filesystem.rs` and `node.rs` hold the host's view of an Fxfs image:
  `Filesystem` formats, opens, fills and checks single-volume (`"default"`)
  images, such as the cells a guest's Fxfs mounts, through `crates/fxfs-posix`
  as the guest's kernel does. Its nodes are `Node` handles, each holding its
  node, with the checks Linux's VFS makes for the guest; `filesystem.rs` keeps
  the nodes held, and runs the filesystem's one unmount.
- `import.rs` makes filesystems of container images (TASK-39.5): an OCI image
  layout's layers, or a `docker export` tarball, applied as the OCI image spec
  applies changesets (whiteouts, opaque directories), files keeping their
  owners, permissions, times and extended attributes. `cladefsctl import` makes
  a base cell of one, for VMs to boot forks of.
- `crates/cladefs_fuse`, a crate of its own (board 14536), mounts a `Filesystem`
  on the host with FUSE (TASK-222): a server of
  the protocol, a request's bytes in and its reply's out, on every host, and on
  Linux a mount through `/dev/fuse`, by mount(2) as root or `fusermount3`
  otherwise. cladefsd, built with its `mount` feature (on unless a build says
  otherwise; without it the daemon links none of the crate and refuses a mount
  asked for, saying so) and with `CLADEFSD_MOUNT`
  naming a MOUNTPOINT and `CLADEFSD_MOUNT_STORES` a DIR, is one mount for any
  number of cells (TASK-302, TASK-475.2): its Mount service, on the daemon's
  control socket (`proto/cladefs/mount/v1/mount.proto`; `cladefsctl mount
  attach`, `create`, `detach`, `list`), takes a tenant's cell of the bucket
  (TASK-364), its head with it, into `DIR/TENANT/CELL`, an Fxfs cell shown
  at `MOUNTPOINT/TENANT/CELL` (doc-20), and
  detaches the cell once the kernel lets go of it, written back; the daemon's
  drain closes the mount. A cell is so held by one process, whether VMs or the
  mount serve it. macOS has no open-source FUSE, so no mounts
  there.
- `memory.rs` holds `VolatileSlice` and `VolatileSliceMut`, memory cladefs only
  copies into and out of, as a guest may change it concurrently.
- `error.rs` holds `Errno`, Linux's errno values on every host, mapped from
  Fxfs's errors, and `Error`, for setting up.
- `storage/` holds `Storage`: an image, or memory. It checks filesystems
  (fsck), and forks images: a fork's cell is a fork of the image's in the
  bucket its cell is kept in, the image's own file bucket, `<image>.bucket`
  beside its directory, or on a host of cladefs's system the host's
  (`CLADEFSD_BUCKET`); the fork's directory is a cell directory of its own, which
  reads what it inherited from the bucket (TASK-165.7). `Storage::delete`
  deletes forks and images alike: the cell in its bucket, whose collector
  reclaims what no other cell inherited, and the directory; forks live on.
  An image's own bucket's collector runs in the processes that have its images
  open (`collect.rs`), the only thing that deletes what they free (TASK-165.8).
  - `device.rs` is the device on images, in place of Fuchsia's
    `FileBackedDevice`: `OpenCellDevice`, an image's `OpenCell` as a device,
    whose reads, writes, frees and commits are the open cell's, as a guest's
    requests of the daemon are, through the cell's page cache
    (`local_cell::pages`); its reads skip the extent cache, as the filesystem
    caches what it reads itself (board 8260). The cell's files bypass the
    host's page cache (`O_DIRECT`, `F_NOCACHE`); on a host filesystem that
    can't, the image, like the part cache, fails to open rather than run in the
    host's page cache (TASK-433.6).
- `host_cell.rs` makes, opens, forks and deletes cell directories in the host's
  directories, images (`Storage`) and guests' cells (`OpenCell`) alike, with
  one block size, one set of options and one way of minting cells. Nothing
  there collects its cells: the collector alone deletes (TASK-165.8).
- `pages.rs` configures physical memory (`slabs.rs`): its size, which
  extents' parts come from, and tenants' arenas of it.
- `executor.rs` holds the fuchsia-async executor Fxfs runs on.

`crates/local_cell` is cladefs's too: backlog doc-4's extent store on local
disk, of segments, large extents' own files, and an index of checkpoints and
per-commit deltas, which recovers to a prefix of its commits after a crash. A
cell directory holds one cell; forks, versions and deletion are the
bucket's (`crates/bucket_cell`, TASK-165.7). Nothing collects in a cell directory:
once a segment is acknowledged in the bucket, it and the extents' files it
names leave the directory, for the host's cache of the bucket's objects, and
the bucket's collector, the only thing that deletes, takes back what no extent
is in (TASK-165.8). Its tests run
it on a file system in memory that crashes at random versions.

## cladefs's other crates

Each crate's `Cargo.toml` says in its first lines what it is. cladefs's own, beside those above:

| Crate | What it is |
|---|---|
| `bucket_cell` | Cells of extent stores kept in object storage (backlog doc-7): writers, readers, forks, versions, checkpoints and the collector, `cladefs-gc`. |
| `extent_frame` | How objects holding extents' data are framed, locally and in object storage. |
| `extent_keys` | The keys a tenant's data is encrypted under at rest, the KMS backends, and the primitives that use them (backlog doc-16). |
| `virtio_extent` | The extent store device but its virtio transport (backlog doc-4), which every VMM hosting it shares; its `README.md` says what a VMM's adapter does. |
| `block_device` | A block device kept straight on an extent device, with no filesystem between: one array of blocks a cell, what SQLite and memory cells hold. |
| `sqlite_cladefs` | A SQLite cell's one database kept on its block device, each transaction one commit of the device. |
| `fxfs-sim-diff` | `guest/sim`'s model of Fxfs run beside Fxfs on the same operations, each file compared after each. |
| `cladefs-examples` | cladefs's examples, benches and tools, in a package of their own so that a release build measures cladefs as its daemon has it. |

## `vendor/firecracker`: the VMM

The VMM is a submodule: branch `cladefs-v1.18.0-r7` of
[semistrict/firecracker](https://github.com/semistrict/firecracker),
Firecracker's v1.18.0 release plus cladefs's changes as one
linear series, each release of it a branch `cladefs-v1.18.0-rN` (rebased from
`cladefs-v1.16.1-r11`, TASK-671; earlier releases on v1.16.1 were
`cladefs-v1.16.1-rN` / `fxdax-v1.16.1-rN`, TASK-269, TASK-273, TASK-437,
TASK-503). Each commit is one upstream submission: five standalone general fixes
at the base (vCPU exit-code ordering, async-signal-safe fatal signal handlers on
alternate stacks, virtio features-page replacement preserving
`device_acked_features()`, vsock snapshot connection resets, and jailer
`/proc/misc` parsing) so downstream forks can share or drop individual fixes
cleanly; the PCI transport's shared memory regions and device IDs past 63; and
the `cladefs` feature, everything else, so that a build without it is
upstream's. Every
cladefs VM, on x86-64 and aarch64 alike, runs its devices on virtio-PCI
(`--enable-pci`): the extent device's DAX window is a BAR of its own, which
upstream's MMIO transport has no way to describe, so a VM with the device on
MMIO does not start (TASK-267, TASK-273). Every commit builds both ways
(`scripts/firecracker-series-check.sh`, CI's firecracker-series job). The series
names no cladefs dependency and holds none of cladefs's in its Cargo.lock, so
that it builds outside this checkout, as those who take it onto forks of their
own build it: cladefs's build supplies them, in a copy of the tree,
`target/firecracker` (`scripts/firecracker-cargo.sh`, which every script that
builds Firecracker goes through): the vmm crate's dependency on
`crates/cladefs`, cladefs's lock (`vendor/firecracker.lock`, which
`scripts/firecracker-cargo.sh --lock` updates), and frame pointers, from this
repo's `.cargo/config.toml`. The series' own `resources/cladefs/README.md` says
the same for its users. Branch `fxdax-v1.16.1`, the series before, a commit a
change, `fxdax`, on Firecracker's main branch, where they began, r1's
`fxdax-v1.16.1-optin` and its PCI stack, `fxdax-pci-r1`, are superseded by it
(`vendor/firecracker.rewrites`). cladefs ran on libkrun before; it left it for
Firecracker, which the sandboxes it serves run (TASK-40).

The workspace builds with it checked out (`git submodule update --init`): on
Linux, cladefs's tests compile its production seccomp filters, upstream's
(`resources/seccomp`) with the rules the `cladefs` feature adds before each
thread's own (`resources/cladefs/seccomp`), with its seccompiler, which links
libseccomp, and run the VM process's side of a DAX window under them, so that a
system call cladefs makes and the filters refuse fails CI rather than a jailed VM
(`crates/cladefs/tests/seccomp.rs`, TASK-149.28).

| Change | Why |
|---|---|
| The extent-store device (`devices/virtio/extent`), behind the `cladefs` feature: `PUT /cladefs/{id}` before boot, or the configuration file's `cladefs` list, names its tag, cell, capacity, DAX window and the extent daemon's socket (`cladefsd_socket`). Its cell is served by the daemon (`cladefs::CellClient`). It attaches over virtio-PCI alone (`--enable-pci`), its DAX window a BAR of its own. | The guest's own Fxfs keeps its filesystem in it (backlog doc-4). No cladefs code serves a cell in the VM process: the daemon owns the open cells and their cache, which the host's VMs share. |
| Virtio shared memory regions on the PCI transport, a BAR with `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` capabilities (a general fix), and `KvmMemSlot`, a KVM memory slot any device may map host memory with. | The device's DAX window is a shared memory region, the daemon's memory mapped in; KVM follows it. |
| The window maps the pages the daemon maps for the guest ahead of its accesses (`KVM_PRE_FAULT_MEMORY`). | Each of KVM's own faults on a page costs an exit, slow under nested virtualization (TASK-25.3). |
| A vCPU retries a `KVM_RUN` failing with `EFAULT` while a window is remapped. | The daemon's changes to a window's host memory make the guest's access fail meanwhile; see `architecture.md`, DAX on Linux (TASK-25.4). |
| `WRITE_COMMIT`, INFO's `sequence_floor`, and the cell's capacity in the device's configuration, changed while the guest runs. | doc-4's requests (TASK-36, TASK-37). |
| Snapshots of a VM with the extent device: `PUT /snapshot/create` forks the cell to `<snapshot>.extent-<id>` and saves the window's mappings; `PUT /snapshot/load`, with `cladefs_overrides` naming each restored VM's own cell, forks it from that version and maps the window again. The guest's filesystem must be frozen (fsfreeze) as the snapshot is taken, and thawed after a restore, when it learns its cell (doc-4, Snapshots). | A zygote's children, suspend-to-RAM and moving VMs, each restored VM with a cell of its own (TASK-39.1). |

## `guest/`: Fxfs in the guest's kernel

The design in backlog doc-2 moves Fxfs into the guest's Linux kernel, over a
virtio extent store. Its first pieces, from TASK-26.1's spike:

- `guest/linux/fs/cladefs`: the kernel's side, in C. `super.c` is the VFS glue
  (superblock, inodes, lookup, readdir, read) over the filesystem's Rust
  (`cladefs.h`), and `rt.c` gives Rust memory (kvmalloc), logging, panics,
  sleeping, work items, timers, the clocks and randomness (`rt.h`). Every call
  into Rust runs in a NOFS allocation scope.
- `guest/rust`: Rust built for the kernel's targets
  (`aarch64-unknown-none-softfloat`, `x86_64-unknown-none`), a workspace of
  its own on a pinned nightly. `scripts/build-guest-sysroot.sh` builds `std`
  with `core` and `alloc` (`-Zbuild-std`) into a sysroot, which the rest of
  the kernel's Rust, Fxfs included, builds against. `linux-rt` is the
  runtime: the global allocator, printk logging, the panic handler, and an
  executor (`block_on` sleeps the calling thread, `spawn` polls on kernel
  workers, `Timer` uses hrtimers).
  `cladefs-linux` is the filesystem, Fxfs for the kernel, and `std` is a std for
  crates written for one (see below).
- `scripts/build-guest-kernel.sh` builds Amazon Linux's 6.18 kernel (the
  release it pins), configured as Firecracker's CI configures the 6.18 guests
  it supports, with fs/cladefs built in, the Rust linked into it as one object of
  what the kernel calls (link-time optimized). The kernel itself has no Rust
  support enabled: Kbuild never sees the Rust. cladefs's changes to the kernel
  are a series of commits on the release (`guest/linux/patches`, indexed with
  each patch's origin in `guest/linux/patches/README.md`, each with the files
  of `guest/linux` it adds), which the script makes in the
  kernel's tree and builds. Its `out/<arch>` has what builds the same kernel
  elsewhere: `patches/` (`git am` them on the release), `libcladefs_linux.a`
  (copy it to `fs/cladefs/`) and `cladefs.config` (cladefs's options, over
  Firecracker's configuration).

**Guest Fxfs: what it does not do** (TASK-26.19). Beside the core inode, file
and superblock operations, it has O_TMPFILE, fallocate (allocate, punch hole,
zero range, keep size), fiemap with SEEK_DATA and SEEK_HOLE, POSIX ACLs,
extended attributes, chattr's immutable, append-only, no-dump and no-atime
flags (kept in its mode's bits above the kernel's 16, which Fxfs's objects have
room for), idmapped mounts, multigrain timestamps, O_DIRECT (through Fxfs, past the page
cache, aligned or not), reflinks (`FICLONE`, `FICLONERANGE`, `cp --reflink`,
and `copy_file_range`, which share extents, counted per range in the
allocator, and copy nothing), freezing, NFS export's file handles, a
shrinker of its cached records, and a device's volumes mounted side by side
(`mount -t cladefs -o volume=NAME tag path`), each with a superblock of its own,
opened as it mounts, sharing the device's journal and DAX window: the filesystem
freezes once all of them are frozen. What programs see of the rest (the guest
suites' `unsupported` check pins deduplication's and `MAP_SYNC`'s):

- Deduplication (`FIDEDUPERANGE`, `duperemove`) reports `EOPNOTSUPP` in each
  destination's status.
- `MAP_SYNC` mmaps fail with `EOPNOTSUPP`: the DAX window is the host's
  memory, not persistent memory, and what is stored through it is in a private
  page until writeback writes it to the cell, and ordered once an fsync
  commits it, never at the fault.
- chattr's other flags (`FS_IOC_FSSETXATTR`'s project IDs and extent sizes,
  synchronous, compressed, no-COW) fail with `EOPNOTSUPP`.
- No change counter (`i_version`): the change attribute, an NFS server's or
  IMA's, is the ctime, which is fine-grained (`FS_MGTIME`) once read.
- Fxfs's in-memory layers of its trees, bounded by its journal's size, are not
  shrunk under the guest's memory pressure; its cached records and its page
  cache are.

## Updating from upstream

`scripts/upstream-changes.py REVISION` lists the files whose cited upstream
sources changed between the recorded revision and `REVISION` in a Fuchsia
checkout (`$FUCHSIA`, by default `~/src/fuchsia`); `--diff` prints the
changes. Bring them over, then record `REVISION` in `Cargo.toml`.
`--check` checks that every cited source exists at the recorded revision.

Formatting: run `rustfmt` on the files you change. `cargo fmt` would also
reformat the files copied from Fuchsia, which keep upstream's formatting.
