# vhost-user extensions for a backend-directed device: a sketch

Superseded as a proposal by doc-32 (`docs/design/doc-32 - vhost-user-lazy-shared-memory-regions.md`),
which folds in review 176; kept for its reasoning and history.

Research for board 14378 (2026-11-27), after review 174 found the plan in
`docs/research/vhost-user-extent-device.md` could not reach its goal: the extent device's DAX window
does not fit vhost-user's `SHMEM_MAP`, and the backend had no safe way to reach guest RAM. This
proposed two small, generic protocol features that would close both gaps. Review 175 confirmed the
second's kernel mechanism but the user dropped it (below); the first stands, with review 175's fixes. Neither is implemented or measured. **Derived** marks what follows from the sources without
having been run.

The trust model throughout is cladefs's (AGENTS.md, "Trust boundaries"): the frontend, a jailed VM
process, is untrusted, and one backend, the daemon, serves every tenant. A frontend may misbehave
in any way; the backend must never block, crash or spend unbounded memory for it. Upstream
vhost-user assumes the opposite (the backend is a helper of a trusted VMM), so each feature says
what it costs a trusted backend too.

## Feature 1: `SHMEM_LAZY`, a backend-directed lazy shared memory region

### Why `SHMEM_MAP` falls short

`VHOST_USER_BACKEND_SHMEM_MAP` (vhost-user spec, `VHOST_USER_PROTOCOL_F_SHMEM`) has the frontend
`mmap` one (fd, offset, length) into a VIRTIO shared memory region at once, read-only or read-write;
"mapping over an already existing map is not allowed", and `SHMEM_UNMAP` names a whole mapping. The
extent device's window (doc-4, backlog doc-13) needs what the userfaultfd second stage in
`crates/cladefs_core/src/dax/uffd.rs` does today, in the VM process:

1. **Lazy install.** A `MAP` names pages; the guest's first touch installs them, so a change of a
   page never touched flushes nothing (TASK-276.6), and the VM process keeps its installed runs within
   a share of `vm.max_map_count`, trimming the oldest (board 8493).
2. **Copy on write.** A guest store makes a private copy (`PrivateMemory`, a per-window memfd laid out
   as the window), which a later `MAP` or `UNMAP` over it ends. A load must not: x86 KVM's async page
   fault worker faults every page for writing (`async_pf_execute`), so a plain `MAP_PRIVATE` mapping
   would copy on a guest's loads (`docs/research/kvm-async-pf-and-shared-guest-memory.md`).
3. **Replacing.** Maps over mapped pages, and unmaps of part of a map.
4. **Restricting.** Withholding a page or making it read-only while a change is in flight, and KVM's
   `EFAULT` retries (`AccessFailures`).

The stage's module docs already say "nothing here is particular to a VMM". The proposal moves that
mechanism into the frontend, generic, driven by a table the backend writes.

### The feature

A device that negotiates `VHOST_USER_PROTOCOL_F_SHMEM_LAZY` (a bit to be allocated through the spec;
bits through 23 are taken) may make any of its shared memory regions lazy, and, with a frontend that
agrees, a guest RAM region too:

- **Sources.** `VHOST_USER_BACKEND_SHMEM_LAZY_SOURCES` (backend to frontend, with fds) names the files
  pages may come from, by index. The frontend maps each `MAP_PRIVATE` from a read-only descriptor and
  write-protects what it installs, as today's stage does (a shared read-only mapping can't be
  registered with userfaultfd: `EPERM` without `VM_MAYWRITE`, review 175 finding 10). The frontend
  holds each source whole, so a source must hold only what the device's guest may always read: for
  cladefs, its tenant's arena and the public one, as a VM process maps today.
- **The table.** One more fd with that message: memory the backend writes and the frontend maps
  read-only, an entry per page (source index, source offset, flags `MAPPED`, `ZEROS`,
  `COPY_ON_WRITE`), each a pair of 64-bit words, the second written last with release and read first
  with acquire, so that a frontend never installs a torn entry.
- **A MAP is a table write.** The backend writes entries and sends nothing; the guest's first touch
  finds them. Ordered before the request's completion, as the used ring's write follows.
- **Faults.** The frontend serves a fault from the table, under the lock it takes to apply
  invalidations, so an install and an invalidation never interleave: installs the run, read-only; on a
  store by the guest's own threads to a `COPY_ON_WRITE` page, a private copy of its own (its memory, its
  budget); a worker's write treated as a load, as today (KVM's async page faults). A fault on an entry
  without `MAPPED` it asks of the backend (`SHMEM_LAZY_FAULT`), without blocking on the answer: faults
  are served concurrently, each answered by the backend writing the entry and acking the fault's id.
- **Invalidate.** To change a mapped entry or reuse a source page, the backend writes the new entry
  first, then sends `SHMEM_LAZY_INVALIDATE` (offset, length, keep-private, an id); the frontend
  uninstalls the range, under its lock, ending its private copies there unless told to keep them; the
  backend reuses the old page once the frontend's acks pass that id. Acks are cumulative and never
  waited on by a thread of the backend's that serves anything else; a frontend that stops acking is
  found by a watchdog and let go, its pages held meanwhile, as today's stage operations are
  (`VmProcessStage`, review 175 finding 3). vhost-user's messages carry no ids, so these carry their own.
- **The optional half** (a sub-feature): `PREFAULT` (install ahead of the guest, doc-4's
  `MAP_PREFAULT`), `MAKE_PRIVATE` (doc-4's `PRIVATE`; and, over guest RAM, the backend's way to write a
  page that isn't private), `PRIVATE_COUNT` (a version is refused while any is, TASK-203).
- **One stage per frontend process**: its regions, the DAX window and guest RAM alike, share one
  mapping budget (`vm.max_map_count`, board 8493) and one set of vCPU hooks (`EFAULT` retries).

### Who else wants it

virtio-fs's DAX window stalled in QEMU on these limits (the `SHMEM_MAP` series came out of that
work); virtio-gpu blob resources map host memory into a guest region; a pmem-like device backed by a
cache shared between guests would want copy-on-write. **Derived**: none of these needs anything
cladefs-specific from the feature.

### What it costs a frontend

The stage is some thousands of lines (`dax/uffd.rs`, `dax/hvf.rs` on macOS), x86-specific async-PF
handling, `EFAULT` retries in the vCPU loop, and a fault thread. A frontend implements it once for all
devices; Firecracker's would carry what `virtio_extent`'s window does today, without anything named
cladefs. That is the upstreaming bet.

## Dropped: `MEM_REMOTE`

v2 had the backend reach guest RAM with `process_vm_readv`/`writev` through the frontend's address
space. Review 175 confirmed the kernel path (a remote write faults through the frontend's userfaultfd
handler, `pin_user_pages_remote` with `FOLL_UNLOCKABLE`, since 4.10) but found its peer identity wrong (a
socketpair's credentials are its maker's). The user dropped it: ptrace-class calls are unusual, and an
upstream reviewer would balk at a backend that may ptrace its VMM. Guest RAM goes through the
private-copy memfd and the daemon's own pages instead (`vhost-user-extent-device.md`, plan v3).

## The eventfds

vhost-user's kick and call fds come from the frontend (`SET_VRING_KICK`, `SET_VRING_CALL`). An
eventfd's two ends are one open file whose flags either process sets, and a frontend may send any fd
at all. So the backend checks each is an eventfd (`fstat` and the anon inode's name), reads kicks with
`preadv2(RWF_NOWAIT)` at offset -1 (`eventfd_read` honours `IOCB_NOWAIT`; an eventfd has no
`FMODE_PREAD`; Linux 5.8, review 175 finding 11), and writes calls only on the
connection's own thread (`eventfd_write` has no `write_iter`, so `RWF_NOWAIT` is refused, review 174
finding 3; a write blocks only at a full counter, which only the frontend can bring about, and only
its own thread waits). KVM's irqfd and ioeventfd need real eventfds for the VMM to leave the path, so
they stay eventfds.

## macOS

`SHMEM_LAZY` doesn't reach macOS as it is: no eventfd or memfd, and
Hypervisor.framework has no irqfd or ioeventfd, so libmachine relays every kick and interrupt anyway
(finding 8). libmachine on macOS keeps its in-process daemon (`libcladefs`) and `virtio_extent`; the
vhost-user path is Linux's.

## Questions for review

1. Are the table's publication, the write-then-invalidate order and installing under the invalidation
   lock enough for the backend to reuse a source page, with faults served concurrently?
2. Can one frontend stage serve guest RAM and the DAX window alike (budget, vCPU hooks, async-PF
   writes), and what does the memory cell's guest RAM need of `SHMEM_LAZY` beyond the window's?
3. Is the daemon writing guest RAM only after `MAKE_PRIVATE` sound and cheap enough: what pages does a
   request write (used ring, READ targets, the response), how often after a snapshot, and can the
   frontend make a vring's pages private at `SET_VRING_ADDR`?
4. What in this is still not generic enough for upstream Firecracker or the vhost-user spec?
