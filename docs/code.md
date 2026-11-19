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

- Fxfs;
- its libraries (`storage_device`, `fscrypt`, `fsverity-merkle` and others);
- the Rust runtime crates (`fuchsia-async`, `fuchsia-sync` and others);
- the SDK's `zx` crates, for their types: cladefs defines none of Zircon's
  syscalls, and neither Fxfs in the guest nor Fxfs on the host calls any.

Each crate's `Cargo.toml` cites the Fuchsia directory it is copied from. Most
files are as upstream wrote them. Those that aren't say how they deviate in
their leading comment:

- The runtime crates are built for the host only, without Fuchsia's runtime
  and handles.
- Code that needs Zircon, such as VMO-backed buffers, builds only when
  `target_os = "fuchsia"`: cladefs's buffers are plain memory. What can't build
  at all, such as FIDL protocols over channels or tests that need Fuchsia's
  component framework, is removed from those files.
- `storage_device` has no `FileBackedDevice`: cladefs has its own image device
  (see `crates/cladefs`).
- Fxfs can snapshot a volume (`RootVolume::snapshot_volume`), which upstream
  can't. The snapshot shares the source's layer files, by reference count,
  and its data extents. The allocator records a store's allocations under a
  data owner that a snapshot changes; the source's owner at the snapshot is
  frozen, its extents never deallocated one by one while any store shares
  them, and freed once none does. Blocks written in place (preallocated
  files) are written to new blocks where a snapshot shares them. fsck checks
  shared extents, and the on-disk format is version 63, the only one a mount
  reads: no format is migrated.

## `crates/physmem`: physical memory

The extent cache's pages, and the DAX windows', are host shared
memory: physical memory in arenas, each its own shared memory object (a memfd
on Linux, POSIX shared memory on macOS), which a process given an arena's
object maps the arena's pages from (TASK-43). An arena takes memory 2 MiB at a
time, and gives a chunk back once its pages are all free; arenas may be capped,
each and together (TASK-50.3); an arena starved at a cap asks for relief, which
evicts cached extents' pages; and a page a VM process's window may still map,
freed, waits in quarantine until the VM process has unmapped it (TASK-79.9).
It was the Zircon kernel port's PMM, cladefs's own code there, lifted out with
its tests when the port was deleted (TASK-96.1).

## `crates/fxfs-posix`: Fxfs's files and directories

One POSIX layer over Fxfs's object stores, which the guest kernel's VFS calls
(`guest/rust/cladefs-linux`) and the host's filesystem calls as a library
(`crates/cladefs`), generic over what differs between them (`Platform`): the
guest's, Fxfs in the kernel, the host's, images being built.

## `crates/`: stand-ins for Fuchsia-only pieces

Some crates only exist inside Fuchsia's build, or only work on Fuchsia. Each
of these stands in with the items the code copied from Fuchsia uses:

| Crate | Stands in for | How |
|---|---|---|
| `fidl` | FIDL runtime | No channels or wire format: the types the generated bindings use. |
| `fidl-fuchsia-io`, `-fs`, `-fxfs`, `-diagnostics-types` | generated FIDL bindings | Written to the shape of the generated types, for the types Fxfs uses. |
| `fuchsia-inspect`, `diagnostics-log` | diagnostics | No-ops, and stderr logging. |
| `bssl-crypto`, `mundane` | BoringSSL bindings | SHA-256/512 on the `sha2` crate. |
| `fxfs-unicode-gen` | build-time generated tables | Tables generated once from UCD 12.1.0 by Fxfs's own generator (`scripts/generate-unicode-tables.sh`). |

## `crates/cladefs`: cladefs's own code

`cladefs` holds the extent daemon and the host's side of Fxfs. Its crate docs
(`cargo doc -p cladefs --open`) are the guide.

- `extents.rs` holds `OpenCell`, an extent store as a VM's device serves
  it: its I/O, and `MAP` and `UNMAP` into its DAX window.
- `cache.rs` holds the cache of sealed extents' pages, shared by every store
  in the process, each extent a table of its pages, read in a part at a time
  and evicted as memory runs out.
- `cladefsd/` holds the daemon (`bin/cladefsd.rs`) and its client,
  `CellClient`, which a VMM's device is; see `architecture.md`, The extent daemon.
- `dax.rs` holds DAX windows, their mappings and the pages they have mapped,
  and the `SecondStage` trait that follows a window's pages: the daemon's connection to a VM process implements it;
  `dax/uffd.rs` is the VM process's own second stage, in the host memory that
  KVM follows (`architecture.md`, DAX on Linux).
- `filesystem.rs`, `volume.rs`, `image.rs` and `node.rs` hold the host's view
  of an Fxfs image: `Filesystem` and `Image`, which format, fill and check
  images, such as the stores a guest's Fxfs mounts, through `crates/fxfs-posix`
  as the guest's kernel does. A `Filesystem` is a volume whose nodes are `Node`
  handles, each holding its node, with the checks Linux's VFS makes for the
  guest; `volume.rs` keeps the nodes held, and runs the volume's one unmount.
  An `Image`'s volumes share its journal, space and executor.
- `import.rs` makes filesystems of container images (TASK-39.5): an OCI image
  layout's layers, or a `docker export` tarball, applied as the OCI image spec
  applies changesets (whiteouts, opaque directories), files keeping their
  owners, permissions, times and extended attributes. `cladefsctl import` makes
  a base store of one, for VMs to boot forks of.
- `fuse/` mounts a `Filesystem` on the host with FUSE (TASK-222): a server of
  the protocol, a request's bytes in and its reply's out, on every host, and on
  Linux a mount through `/dev/fuse`, by mount(2) as root or `fusermount3`
  otherwise. `cladefs-mount STORE MOUNTPOINT` mounts a store nothing else has
  open, read-write, until it is unmounted or sent SIGINT or SIGTERM; with
  `--new CAPACITY`, a new store it makes. cladefsd, with `CLADEFSD_MOUNT`
  naming a MOUNTPOINT and `CLADEFSD_MOUNT_STORES` a DIR, is one mount for any
  number of stores (TASK-302, TASK-475.2): its Mount service, on the daemon's
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
  bucket its store keeps it in, the image's own file bucket, `<image>.bucket`
  beside its directory, or on a host of cladefs's system the host's
  (`CLADEFSD_BUCKET`); the fork's directory is a store of its own, which
  reads what it inherited from the bucket (TASK-165.7). `Storage::delete`
  deletes forks and images alike: the cell in its bucket, whose collector
  reclaims what no other cell inherited, and the directory; forks live on.
  An image's own bucket's collector runs in the processes that have its images
  open (`collect.rs`), the only thing that deletes what they free (TASK-165.8).
  - `image.rs` is the device on images, in place of Fuchsia's
    `FileBackedDevice`: an extent store in a directory, `crates/local_cell`'s.
    Extents' I/O bypasses the host's page cache (`O_DIRECT`, `F_NOCACHE`); on
    a host filesystem that can't, the image, like the part cache, fails to open
    rather than run in the host's page cache (TASK-433.6).
  - `io.rs` runs extents' I/O off Fxfs's executor threads: on an io_uring on
    Linux, and a small thread pool elsewhere.
- `host_cell.rs` makes, opens, forks and deletes both host stacks' extent
  stores in the host's directories, images (`Storage`) and guests' stores
  (`OpenCell`), with one block size, one set of options and one way of
  minting cells. Neither stack collects its stores: the collector alone
  deletes (TASK-165.8).
- `pages.rs` configures physical memory (`crates/physmem`): its size, which
  extents' pages come from, and tenants' arenas of it.
- `executor.rs` holds the fuchsia-async executor Fxfs runs on.

`crates/local_cell` is cladefs's too: backlog doc-4's extent store on local
disk, of segments, large extents' own files, and an index of checkpoints and
per-commit deltas, which recovers to a prefix of its commits after a crash. A
store's directory holds one cell; forks, versions and deletion are the
bucket's (`crates/bucket_cell`, TASK-165.7). Nothing collects in a store:
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
| `extent_files` | Named files kept straight on an extent device, with no filesystem between: file sets. |
| `sqlite_extents` | SQLite databases kept so, each transaction one commit of the device. |
| `fxfs-automount` | A guest's automounter of shared volumes (backlog doc-10), and `git-remote-fxfs`. |
| `follower`, `follower_sim`, `follower_history` | Live read-only followers' concurrency protocol, its deterministic simulation and its history checker (backlog doc-8). |
| `fxfs-sim-diff` | `guest/sim`'s model of Fxfs run beside Fxfs on the same operations, each file compared after each. |
| `cladefs-examples` | cladefs's examples, benches and tools, in a package of their own so that a release build measures cladefs as its daemon has it. |

## `vendor/firecracker`: the VMM

The VMM is a submodule: branch `cladefs-v1.16.1-r11` of
[semistrict/firecracker](https://github.com/semistrict/firecracker),
Firecracker's v1.16.1 release, the sandboxes' pin, plus cladefs's changes as one
linear series, each release of it a branch `cladefs-v1.16.1-rN` (TASK-269; r2,
TASK-273, the one with the extent device over virtio-PCI alone; r8, TASK-437, r7
renamed, the releases before it named `fxdax-v1.16.1-rN`; r10, TASK-503, r8 with
its API saying cladefs; r11, TASK-671, r10's 31 commits as 4). A commit is an
upstream submission: upstream's dependency bumps, backported; the general
fixes, which apply to v1.16.1 alone; the PCI transport's shared memory regions
and device IDs past 63; and the `cladefs` feature, everything else, so that a
build without it is upstream's. Every
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
| The extent-store device (`devices/virtio/extent`), behind the `cladefs` feature: `PUT /cladefs/{id}` before boot, or the configuration file's `cladefs` list, names its tag, store, capacity, DAX window and the extent daemon's socket (`cladefsd_socket`). Its store is served by the daemon (`cladefs::CellClient`). It attaches over virtio-PCI alone (`--enable-pci`), its DAX window a BAR of its own. | The guest's own Fxfs keeps its filesystem in it (backlog doc-4). No cladefs code serves a store in the VM process: the daemon owns the stores and their cache, which the host's VMs share. |
| Virtio shared memory regions on the PCI transport, a BAR with `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` capabilities (a general fix), and `KvmMemSlot`, a KVM memory slot any device may map host memory with. | The device's DAX window is a shared memory region, the daemon's memory mapped in; KVM follows it. |
| The window maps the pages the daemon maps for the guest ahead of its accesses (`KVM_PRE_FAULT_MEMORY`). | Each of KVM's own faults on a page costs an exit, slow under nested virtualization (TASK-25.3). |
| A vCPU retries a `KVM_RUN` failing with `EFAULT` while a window is remapped. | The daemon's changes to a window's host memory make the guest's access fail meanwhile; see `architecture.md`, DAX on Linux (TASK-25.4). |
| `WRITE_COMMIT`, INFO's `sequence_floor`, and the store's capacity in the device's configuration, changed while the guest runs. | doc-4's requests (TASK-36, TASK-37). |
| Snapshots of a VM with the extent device: `PUT /snapshot/create` forks the store to `<snapshot>.extent-<id>` and saves the window's mappings; `PUT /snapshot/load`, with `cladefs_overrides` naming each restored VM's own store, forks it from that version and maps the window again. The guest's filesystem must be frozen (fsfreeze) as the snapshot is taken, and thawed after a restore, when it learns its store's cell (doc-4, Snapshots). | A zygote's children, suspend-to-RAM and moving VMs, each restored VM with a store of its own (TASK-39.1). |

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
  are a series of commits on the release (`guest/linux/patches`, each
  with the files of `guest/linux` it adds), which the script makes in the
  kernel's tree and builds. Its `out/<arch>` has what builds the same kernel
  elsewhere: `patches/` (`git am` them on the release), `libcladefs_linux.a`
  (copy it to `fs/cladefs/`) and `cladefs.config` (cladefs's options, over
  Firecracker's configuration).
- `scripts/probe-guest-std.sh` builds Fxfs's third-party dependencies
  (`guest/std-probe`) for the kernel's targets, against a sysroot whose `std`
  is `guest/rust/std`, and lists those that do not build.

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
- Extents written in place (Fxfs's overwrite modes, which nothing in the guest
  makes) can't be shared: a reflink of them fails with `EOPNOTSUPP`.
- `MAP_SYNC` mmaps fail with `EOPNOTSUPP`: the DAX window is the host's
  memory, not persistent memory, and what is stored through it is in a private
  page until writeback writes it to the store, and ordered once an fsync
  commits it, never at the fault.
- chattr's other flags (`FS_IOC_FSSETXATTR`'s project IDs and extent sizes,
  synchronous, compressed, no-COW) fail with `EOPNOTSUPP`.
- No change counter (`i_version`): the change attribute, an NFS server's or
  IMA's, is the ctime, which is fine-grained (`FS_MGTIME`) once read.
- Fxfs's in-memory layers of its trees, bounded by its journal's size, are not
  shrunk under the guest's memory pressure; its cached records and its page
  cache are.
- A follower's files not on DAX are cached in 4 KiB folios, not large ones,
  as its batches drop ranges of them: more overhead per byte in large
  sequential I/O. A writer's are in large folios (TASK-26.19.11).

## Updating from upstream

`scripts/upstream-changes.py REVISION` lists the files whose cited upstream
sources changed between the recorded revision and `REVISION` in a Fuchsia
checkout (`$FUCHSIA`, by default `~/src/fuchsia`); `--diff` prints the
changes. Bring them over, then record `REVISION` in `Cargo.toml`.
`--check` checks that every cited source exists at the recorded revision.

Formatting: run `rustfmt` on the files you change. `cargo fmt` would also
reformat the files copied from Fuchsia, which keep upstream's formatting.
