# Architecture

How cladefs's parts work together: what the guest asks of its extent store device, how the DAX
window is kept, and what the extent daemon owns and whom it trusts. `README.md` is the overview;
`CONTEXT.md` defines the words used here; the designs are the design docs (`docs/design`:
doc-2 the split between guest and host, doc-4 the device, doc-13 copy-on-write DAX, doc-7 and
doc-14 the bucket and local disk). Running a daemon is `operating-a-host.md`.

## How it fits together

```
guest (Linux + Fxfs)     guest/: Fxfs in the kernel; DAX reads and writes
  │  virtio extent-store device: READ, WRITE, MAP, COMMIT...
Firecracker (one VM     vendor/firecracker: the extent device, a client of the
  process)               daemon
  │  Unix socket; the daemon's memory (a memfd), mapped into the DAX window
cladefsd            crates/cladefs: cladefsd, the open cells, the extent cache,
  │                      the DAX windows
physical memory          crates/cladefs/src/slabs.rs: arenas of part slabs
  │                      (memfds), their caps, relief and quarantine
extent store             crates/local_cell: segments, large extents' files,
                         an index
```

- **Requests:** the guest's Fxfs reads, writes, frees and commits extents
  through the device, which forwards each request to the daemon.
- **DAX:** `MAP` maps a range of a sealed extent into the VM's DAX window, a
  range of guest physical memory the daemon keeps the page tables of, read-only:
  the extent's pages from the cache, which reads them in from the cell, or
  zeros. The window copies on write (backlog doc-13): a page the guest writes
  becomes the VM's own, a private copy at the same address, at most
  `private_max` of them, until a `MAP` or `UNMAP` over it. The guest writes its
  dirty pages back as it would without DAX, with `WRITE`s to new extents and a
  commit, and once a commit seals those, maps the pages back to them, sharing
  them again. Files a mount creates go through the guest's page cache until
  their inodes are next read in.
- **Sizing (TASK-227):** the window is address space, not memory: size it to
  the data, as virtio-pmem maps a whole image. A page the daemon holds is one
  copy however many windows map it, and a window costs for what its guest
  reaches alone: the guest's struct pages, the VM process's state, its
  mappings and KVM's page tables are the same through 8 GiB and 64 GiB windows
  (GCE r881, r882). A window is 64 GiB at most (`MAX_WINDOW`, which bounds what
  the daemon keeps for it). `private_max` bounds memory, the VM process's: 1 GiB
  unless configured (`dax_private_max_bytes`, the VM manager's to size per VM,
  TASK-502), whatever the window's size. The daemon reserves none of it
  (TASK-501). The daemon's memory (`MEMORY_BYTES`) is what keeps a VM's rereads
  fast, as a host's page cache keeps a pmem image's: a Linux tree's warm
  reread through an 8 GiB window took 13.15 s with a daemon of 1 GiB,
  600 ms with 8 GiB, against ext4's 586 ms from its guest's page cache. The
  guest tests' windows are the host's memory (at most 64 GiB) and their daemons
  governed, as production's (board 14529), but for the cases that say why they
  fix a size (several hosts on one machine), and `CLADEFSD_MEMORY` fixing
  every daemon's for an A/B's fixed side.
- **Hypervisor mirror:** the daemon maps pages at the window's addresses, and
  the VM process maps the same pages of the daemon's physical memory (memfds)
  into the host memory KVM follows for the window, keeping it in step with
  userfaultfd.

## DAX on Linux

KVM maps a guest's memory from the host memory backing its
memory slot, and follows the host's mappings. On arm64, though, a fault it
can't resolve there is reported only as `EFAULT`, with no address. So that
memory must never fault that way. The VM process hands it to its cell's
connection to the daemon (`create_dax_window_in_host_memory`), which keeps it
like this:

- Pages the window hasn't mapped are anonymous memory registered with
  userfaultfd for missing faults.
- Mapped pages are the daemon's pages mapped again, from its physical memory
  (memfds), at their offset there. They stay read-write, and userfaultfd
  write-protects them while the window maps them read-only.
- A fault thread in the VM process passes each event to the daemon, which
  resolves them concurrently, as futures, so a fault waiting for a page to be
  read in holds up no other.
- Changing a page's mapping replaces its memory in several system calls. Until
  userfaultfd tracks the new memory, it is inaccessible, or read-only where a
  write would go untracked, so a fault in between fails with `EFAULT` rather
  than resolving wrongly, and the VMM retries the vCPU.

x86 KVM faults pages in for writing from a worker, whatever the guest's
access. So a write the window refuses is resolved as a read, and the vCPU's own
access decides: a load copies nothing, and a write to a page mapped read-only
copies it once, to the VM's private page.

KVM would map each page for the guest on a fault of its own, which nested
virtualization makes slow. Mapping an extent maps the pages it has in memory, and the window
tells the VMM of the pages it maps (`Prefault`), which the VMM has KVM map
ahead of the guest (`KVM_PRE_FAULT_MEMORY`).

Handling KVM's faults needs `/dev/userfaultfd` access (`operating-a-host.md`, Access to userfaultfd). The host's
kernel must be Linux 6.5 or later: `SO_PEERPIDFD` (6.5, below), `/dev/userfaultfd` (6.1), userfaultfd's
write protection of shared memory (`UFFD_FEATURE_WP_HUGETLBFS_SHMEM`, 5.19) and
`MADV_POPULATE_WRITE` (5.14). In a jail, whose VMM has no privileges left,
the device node must be in the jail, rather than `vm.unprivileged_userfaultfd=1`
set, which every process on the host would get (TASK-50.4). Firecracker's
jailer makes it there when the host has it, owned by the VMM's uid and gid,
mode 0600; a jail made otherwise needs the same (`mknod dev/userfaultfd c 10
MINOR`, its minor as the host's `/dev/userfaultfd` has it).

## The extent daemon

The VMM runs one VM per process, so extents cached in a
VM process would be shared by that VM alone. `cladefsd` (`cladefs::cladefsd`)
is one process on a host that owns the physical memory, the extent
cache and the open cells, and serves them to VM processes over a Unix socket: the
only way the VMM's extent device has its cell, at the socket its configuration
names (`cladefsd_socket`). Physical memory is memfds, which each VM process
receives once: its
window's pages are pages of it, which the VM process maps into its own memory
at the offsets the daemon names, so two VMs mapping one extent map the same
pages. The daemon's changes to a window reach the VM process in order before
the replies after them, unmapping and write-protecting acknowledged before the
daemon frees or shares a page; guests' faults go from the VM process's
userfaultfd to the daemon. One daemon serves every tenant on a host. Guest
isolation is the security boundary: a guest reaches only what its window maps,
so one tenant for everything is secure. As defence in depth against a VM
process escaped from its guest, physical memory is in arenas, each
its own memfd (TASK-43): a tenant's cells' pages are of its arena, and public
bases' pages, which every tenant's forks share, one copy, of the default arena.
A tenant's VM process holds its tenant's arena and the default one, read-only
(mapped private, so the window can track writes to it), and nothing of another
tenant's. An extent's pages are of the arena it is cached in alone. A VM's
private DAX pages, those its guest wrote, are its VM process's own memory,
laid out as its window is, at most `private_max` of them, charged to the VM
process (its cgroup), not to the daemon's memory or the tenant's (TASK-501).
The host's VM manager drives the daemon
through a gRPC control API on a Unix socket of its own (`CLADEFSD_GRPC`;
`crates/cladefs/proto/cladefs/cladefsd/v1/cladefsd.proto`, TASK-43.3; in Go,
`github.com/semistrict/cladefs/go/cladefsd/v1`, TASK-219): it makes,
imports, forks, places, syncs, resizes and deletes cells, and grants tenants
(TASK-43.4). Every call names a tenant by its ID in the bucket (TASK-364), a
64-bit number; no tenant has a name. Grant returns a tenant's own VM socket,
`<ID in base 32>.sock`, for the VM manager to bind-mount into the tenant's jails; a
connection's tenant is the socket it came in on, and it names cell directories in
the tenant's directory alone. Those are its own cells, and its VM processes open no cell of
another tenant's, but fork the versions of others' its VM manager gives it. Revoked, a tenant's socket
goes, and once its VM processes do, its arena and cached pages.
The daemon's own VM socket is the default tenant's, ID 0, which is granted
nothing and has no limits of its own, only every tenant's: its connections name
cell directories in the daemon's roots alone (`CLADEFSD_ROOTS` or `[[disks]]`), which it
owns, and boots owning one at least (board 14305). It is the daemon's user's alone, mode 0600, so that a jailed VM process, of
another user, reaches a tenant's socket and no other, whatever is mounted
(TASK-155). Below a tenant, a VM process's user does not matter: nothing there
is separated.
A tenant's VM process names no cell: what it says is ignored, so that another
tenant's ID gets it nothing. A VM snapshot restored on another host needs its version
there, which the VM manager makes in the tenant's directory first (`CreateCell`
of the version's cell), and may drop once done (`DropCell`, which leaves the
cell in the bucket); the proto says how (TASK-154). A version is immutable: any
number of hosts fork one at once, and none writes it (TASK-158). A base forked
on several hosts is made a version first (`ForkCell`'s `version`).

Beside the control API, on its socket, the daemon serves its Mount service
(`crates/cladefs/proto/cladefs/mount/v1/mount.proto`), which shows cells on the
host through FUSE (`CLADEFSD_MOUNT`). The FUSE server and its mount through
`/dev/fuse` are `crates/cladefs_fuse`, a crate apart from the host's Fxfs
(`crates/cladefs_fs`), which the daemon links only when built with its `mount`
feature: on unless a build says otherwise (board 14536). A daemon built without
it, as a process embedding one builds it (`crates/libcladefs`), or one not on
Linux, mounts no cells: asked to, it refuses to start, and its Mount calls fail
`UNIMPLEMENTED`, each saying why.

The daemon must be able to kill each VM process (it is the daemon's user's, or
the daemon has `CAP_KILL`), and refuses a connection whose process it could
not kill: one that stops acknowledging the daemon's changes to its window,
frozen, holds the pages it may still map until the daemon has killed it
(TASK-79.9). It knows the process by the socket's pidfd (`SO_PEERPIDFD`, Linux
6.5), and refuses to start on a kernel without it (board 8533): the pid of the process that
connected, opened after the fact (`pidfd_open`), may be another process's by then, which the
daemon would kill. Its doctor checks that and every other limit and kernel feature as it starts
(`cladefsd::doctor`, `cladefsd --doctor`, board 14217): it warns of a limit of open files under
55,296 (`RLIMIT_NOFILE`: its own 1,024, and 212 for each of 256 cells). It refuses to start with
a `CLADEFSD_ROOTS` directory it can't resolve, a part cache it can't open (board 8595), or—in
governed memory mode
(no fixed `MEMORY_BYTES`)—without Linux 6.5+ `cachestat(2)`, MGLRU enabled, swap at least the
governor's floor, and writable `/sys/fs/cgroup` with `CAP_DAC_OVERRIDE` (or
`CLADEFSD_VM_CGROUP_DELEGATED=1`); `cladefsd --setup-host` provisions the kernel settings and zram
swap before start.

## Ephemeral cells

An ephemeral cell (doc-25, TASK-698) is a fork of a version kept on one host's disk alone, the
upper layer over its parent's version as a sandbox's overlay is over its image. Its extent store
(`local_cell`'s `cell::ephemeral`) keeps its closed segments raw, never framed nor uploaded, and
covers them in a local checkpoint as eight add up and as it closes: its state over its parent's
version, with its keys. Its kept segments give back the runs of what it frees (`File::punch`), and
its footprint counts their blocks allocated. It has no uploader: its commits are Local at most. A
reference in its parent's bucket, naming the host's incarnation, keeps the version while it lives.
The host's `CLADEFSD_EPHEMERAL` directory holds that incarnation and a journal, by which a daemon
starting retires the references of cells it holds no more, and `DecommissionHost` retires those of
a host gone for good. `MigrateEphemeralCell`, `HandOverEphemeralCell`, `WaitEphemeralHandOver`,
and `AbortEphemeralMigration` move an ephemeral cell live to another host over the daemons' peer
network (`cladefsd::migration`): its kept segments are copied while its guest runs, the rest once
its VM process is gone or its cell is released, the source giving the cell up before the target
takes it over, as `specs/ephemeral` models; `AbortEphemeralMigration` ends a migration before that.
(Bucket-backed cells do not use peer
hand-over; they move between hosts through the bucket via `SyncCell`, `CreateCell` + `PlaceCell`,
`PUT /snapshot/load`, and `DropCell`.) A target takes a migration only of a source of its own
formats
and bucket, the cell going at a new name in its `CLADEFSD_EPHEMERAL_CELLS` directory, and takes it
into place only once it opens as the cell prepared (boards 8443, 8447, 8455).

