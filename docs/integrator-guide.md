# Integrator guide: running tenants' VMs on cladefsd

For whoever writes the VM manager, the trusted service on each host that talks to the cladefsd
daemon and starts Firecracker. `crates/cladefs/proto/cladefs/cladefsd/v1/cladefsd.proto` is the reference
for every call named here.

A VM manager in Go uses the Go package cladefs generates from it, `go/cladefsd/v1` (TASK-219): its
messages and stubs, and `NewClient`, which connects to the control socket and has a failed call's
error carry the daemon's errno (`errors.Is(err, syscall.ENOENT)`). Take it from the commit the
daemon is built from, so that the two agree:

```sh
go get github.com/semistrict/cladefs/go@<cladefs commit>
```

The VM manager is trusted. VM processes are not: treat one as compromised, and everything here
follows from that.

## Per-host setup

1. Start `cladefsd --config /etc/cladefs/cladefs.toml` (see `docs/cladefs.example.toml`) with one
   or more `[[disks]]` entries and `[daemon].control_socket` naming its control socket. Keep the
   control socket away from jails. It can do everything, so it is mode 0600, the daemon's user's,
   whatever the umask (TASK-159): run the VM manager as that user, or as root.
2. For each tenant, call `Grant(tenant, uid, gid)` — no host directories are specified; `cladefsd`
   automatically scopes the tenant across its managed `[[disks]]` pool. It returns the path of a
   socket.
3. Bind-mount **that socket alone** into the tenant's jails. Never mount the daemon's default VM
   socket into a jail: it can take any cell by its ID.
4. Run jails with Firecracker's jailer, as a uid other than the daemon's. The default VM socket is
   mode `0600`, owned by the daemon, so that a mistake in step 3 still can't connect.
5. Start Firecracker with `--enable-pci`, on x86-64 and aarch64 alike: the extent device attaches
   over virtio-PCI alone, its DAX window a BAR of its own, and a VM with it on MMIO does not start
   (TASK-273).

Callers never choose host directories or paths. Every control RPC (`CreateCell`, `ForkCell`,
`ImportImage`, `ImportMemory`, `StatCell`, `SyncCell`, `ResizeCell`, `DeleteCell`, `DropCell`,
`PlaceCell`, `MakePublic`, `MakeVersion`, `ListVersions`, `DropVersions`, `ShareVersion`,
`AttachBase`, `MigrateCell`, `HandOverCell`, `AbortMigration`) addresses cells by `(tenant, cell)`
alone. On the VM socket (`OPEN`), Firecracker passes `"TENANT/CELL"` (in Crockford base 32) to open
an existing cell, or `"TENANT/0"` / `"0/0"` to let the daemon create or fork a new cell on the best
managed disk. On a granted tenant's socket, `"0/0"` is a new cell of that tenant's, and any other
tenant's cell is refused with `EACCES`. Raw host paths are refused with `EINVAL`. `StatCell`'s
`placement_token` says which placement of a cell this host holds, open or not
(`docs/operating-a-host.md`).

Use `ListDisks` and `SetDiskGovernance` (or `cladefsctl disks` / `cladefsctl set-disk`) to inspect
disk health/capacities and adjust per-disk caps, headroom, dirty budgets, or placement modes
(`read_write`, `drain`, `read_only`) at runtime.

Grants don't survive a restart of the daemon. After one, call `Grant` again, and bind-mount the new
socket.

To show tenants' cells on the host as files, start `cladefsd` with `CLADEFSD_MOUNT` naming a mount
version and `CLADEFSD_MOUNT_STORES` a directory for their cell directories: it mounts them with FUSE and serves
the Mount service (`proto/cladefs/mount/v1/mount.proto`, Go `go/mount/v1`) on the same control
socket. One daemon then holds every cell of the host, whether VMs or the mount serve it (TASK-475.2).

## Upgrading the daemon

Upgrade a host's daemon and Firecracker by draining the host: move its VMs off, then SIGTERM the
daemon, which closes every cell before it exits. `docs/upgrading-a-host.md` is the runbook, and
says what a format change refuses.

## Restoring a snapshot, on any number of hosts at once

A VM snapshot makes a version of each of the VM's cells, and its file names each by tenant, cell
and version. A version is immutable: any number of hosts fork it, and none writes it, so a restore
on one host changes nothing another host sees. Each restore is a fork of the version into a new
cell of the restoring VM's tenant's.

A tenant's VM processes restore versions of their own tenant's cells, and versions of other
tenants' cells only while those are shared (`ShareVersion`); any other restore fails with
`PERMISSION_DENIED` (`EACCES`), making nothing.

### 1. Before restoring on another host than the snapshot's

```
SyncCell(<the snapshotted VM's cell>)
```

A version is made locally and uploaded behind. Once `SyncCell` returns, it is in the bucket for
every host.

### 2. Load the snapshot

In `cladefs_overrides`:

| Field | Value |
|---|---|
| `path_on_host` | `"<TENANT>/0"` (or `"0/0"` on a granted tenant's socket) to fork into a new daemon-placed cell, or `"<TENANT>/<CELL>"` to reopen an existing cell |
| `cladefsd_socket` | The tenant's socket, as the jail sees it |

Do this as often as you like, on any host, at the same time. Each restore makes a new cell for the
VM on a managed disk chosen by the daemon; the daemon opens the version once on a host for all the
restores of it there.

Without `cladefs_overrides`, `path_on_host` is the snapshotted VM's own `"TENANT/CELL"` ID, which
resumes that cell in place (failing with `EEXIST` if the cell diverged after the snapshot).

A VM's memory restored from a memory cell names the version in `memory_cell`'s `tenant`, `cell`
and `version`, as the disk's are named in the snapshot file, with `mem_backend.backend_path = "0/0"`
(or `"<TENANT>/0"`).

### Memory cells: snapshotting and restoring without local memory files

A VM's guest memory can live in a **memory cell** (`CELL_KIND_MEMORY`) instead of a local memory
file, so every VM restored from the same snapshot on a host shares unmodified pages straight out of
the extent daemon's cache (copy-on-write on first guest store).

There are two ways to create a snapshot's memory cell version:

1. **Directly from Firecracker (`PUT /snapshot/create`, preferred):** pass `memory_cell` in place of
   `mem_file_path` — no local memory file is written:
   - **Cold-booted (or File/Uffd-restored) VM:** pass
     `"memory_cell": { "path_on_host": "0/0", "cladefsd_socket": "<tenant.sock>" }`. Firecracker
     scans resident pages via `mincore` (skipping untouched anonymous RAM), writes non-zero pages,
     commits, and makes a version.
   - **VM already restored from a memory cell (`MemBackendType::Cell`):** pass `"memory_cell": {}`
     (omit both fields). Firecracker writes only the window's private pages into the VM's forked
     memory cell, commits, reclaims them back to the newly committed extents, and makes a version.
   - On success, `PUT /snapshot/create` returns `200 OK` with JSON
     `{"tenant": <u64>, "cell": <u64>, "version": <u64>}`. Call `SyncCell(tenant, cell)` before
     restoring on another host.
2. **Importing an existing Firecracker memory file (`ImportMemory` / `cladefsctl import-memory`):**
   call `ImportMemory(file, tenant, make_version=true)` over the control API (or
   `cladefsctl import-memory --version FILE`). Zero pages are discarded as holes; call
   `SyncCell(tenant, cell)` before cross-host restore.

To restore with KVM dirty-page tracking (for live migration pre-copy), set `"track_dirty_pages": true`
in `PUT /snapshot/load` alongside `mem_backend.backend_type = "Cell"`: KVM tracks guest stores
(including first-time COW into private pages and subsequent writes to private pages), while
unmodified shared pages remain read-only and clean.

To restore a live-migrated VM on the destination using the same base memory cell version plus
sparse Firecracker diff files (`snapshot_type: "Diff"`), pass `"diff_files": ["<diff0>", "<diff1>", ...]`
(ordered oldest to newest) inside `memory_cell` on `PUT /snapshot/load`: Firecracker forks the base
memory cell version, overlays only allocated ranges from the diff files (later files win; dirty
zero pages become holes shadowing base pages; unallocated holes stay shared from the base version),
commits once, and maps the COW window.

### Host-local suspend/resume of ephemeral VMs (zero bucket uploads)

VMs running on ephemeral Fxfs cells (`ForkCell(..., persistent: false)`, doc-25) can be parked to
local SSD and resumed on the **same host** using standalone ephemeral memory cells without
uploading anything to object storage:

1. **Freeze, pause, and suspend (`PUT /snapshot/create`):**
   - Freeze the guest's Fxfs filesystem and pause the VM (`PATCH /vm` with `{"state": "Paused"}`).
   - **First suspend (cold-booted anonymous RAM):** pass `"ephemeral": true` in `memory_cell`:
     ```json
     {
       "snapshot_type": "Full",
       "snapshot_path": "<dir>/vm.state",
       "memory_cell": {
         "path_on_host": "<dir>/mem-cell",
         "cladefsd_socket": "<tenant.sock>",
         "ephemeral": true
       }
     }
     ```
     Firecracker creates a standalone host-local ephemeral memory cell (`CELL_KIND_MEMORY`, encrypted
     under the tenant's KMS key, zero bucket uploads or references), writes resident non-zero pages,
     commits, and checkpoints it locally. For each ephemeral rootfs extent device, `cladefsd`
     quiesces and checkpoints the ephemeral cell in place instead of creating a bucket version,
     recording `(tenant, cell, committed)` in the snapshot state (`snapshot-extent-state` v3).
     `PUT /snapshot/create` returns `200 OK` with
     `{"tenant": <u64>, "cell": <u64>, "version": 0, "ephemeral": true}`.
   - **Subsequent re-suspends (VM already resumed from an ephemeral memory cell):** pass
     `"memory_cell": {}` (or `{"ephemeral": true}`). Firecracker flushes only dirty private pages
     into the existing ephemeral memory cell, reclaims them back to the cell's extents, and
     checkpoints (punching freed runs in place).
   - Stop the Firecracker process to release 100% of host RAM and swap. Do **not** call `SyncCell`
     (ephemeral cells have no uploader and never upload to the bucket).
2. **Resume on the same host (`PUT /snapshot/load`):**
   - Pass `mem_backend.backend_type = "Cell"`, `mem_backend.backend_path = "<dir>/mem-cell"`, and
     `memory_cell` with `"ephemeral": true` (`version` omitted or `0`; `tenant` and `cell` optional
     to verify identity), plus `cladefs_overrides` pointing at the same ephemeral rootfs cell:
     ```json
     {
       "snapshot_path": "<dir>/vm.state",
       "mem_backend": { "backend_type": "Cell", "backend_path": "<dir>/mem-cell" },
       "memory_cell": {
         "tenant": <u64>,
         "cell": <u64>,
         "cladefsd_socket": "<tenant.sock>",
         "ephemeral": true
       },
       "resume_vm": true,
       "cladefs_overrides": [{ "id": "extents", "path_on_host": "<dir>/rootfs-cell" }]
     }
     ```
   - `cladefsd` reopens the ephemeral rootfs cell in place only if its `(tenant, cell)` and latest
     `committed` epoch still match the snapshot; restoring a stale snapshot after the VM resumed
     and wrote more fails with `EEXIST`. Guest RAM pages fault back lazily via userfaultfd on
     demand and copy-on-write on first guest store.
3. **Discard / cleanup:** once the Firecracker process is stopped, call `DeleteCell` on both the
   ephemeral rootfs cell (which retires its parent version reference) and the ephemeral memory cell
   (standalone — holds no bucket reference and needs no `CLADEFSD_EPHEMERAL`), and remove the
   Firecracker snapshot state file. Standalone ephemeral cells can also be created or imported via
   `CreateCellRequest.ephemeral = true` or `ImportMemoryRequest.ephemeral = true` (with
   `make_version = false`); `StatCellResponse.ephemeral` reports whether a cell is ephemeral.

## Live migration of bucket-backed cells

Bucket-backed cells move between hosts sharing a bucket through the bucket itself (or through the
fast WAL, where each commit already is once acknowledged) using the existing control RPCs
(`SyncCell`, `CreateCell`, `PlaceCell`, `DropCell`) — no peer mTLS hand-over is needed. Peer live
migration (`MigrateCell`, `HandOverCell`, `WaitHandOver`, `AbortMigration`) serves **ephemeral
cells only** and refuses bucket-backed cells with `FAILED_PRECONDITION` (`ENOTSUP`).

1. **Pause, migration snapshot, and release the cell on the source VM:**
   - Quiesce dirty guest data (`syncfs` and rebind clean DAX private pages via
     `/sys/module/cladefs/parameters/dax_rebind_now`, without needing `fsfreeze`) and pause the
     source VM (`PATCH /vm` with `{"state": "Paused"}`).
   - Capture the migration snapshot with `PUT /snapshot/create` and `"for_migration": true`: this
     records each extent device's DAX window mappings without requiring a guest `fsfreeze` and
     without creating a cell version (still verifying that the device has no uncommitted private
     pages or open extents). Guest memory diffs go into `diff_files` overlays, keeping guest memory
     out of the bucket.
   - Release each extent device's cell on the paused source VM via `PATCH /cladefs/{id}` with
     `{"state": "Released"}` (the VM process stays alive, and `Resume` / `CreateSnapshot` are
     refused while any cell is released).
2. **Upload remaining commits on the source daemon:**
   - Call `SyncCell(tenant, cell)` on the source daemon, which returns once every commit is
     uploaded to the bucket (or already durable in the fast WAL).
3. **Take the cell and load the VM on the target daemon:**
   - Call `CreateCell(tenant, cell, capacity)` on the target daemon (which creates the local cell
     directory for the cell kept in the bucket; pass `token` directly if advancing the fencing
     token in one call) and `PlaceCell(tenant, cell, token=<new_fencing_token>)`.
   - Load and resume the VM on the target with `PUT /snapshot/load` (`cladefs_overrides`
     referencing `"TENANT/CELL"`, which opens the cell directly with `version == 0`, taking its
     head at the new fencing token and fencing the source).
4. **Clean up on the source daemon (or abort before step 3):**
   - Call `DropCell(tenant, cell)` on the source daemon to remove its local cell directory (leaving
     the bucket cell intact) and stop the paused source VM process.
   - To **abort** before step 3 (nothing was given up in the bucket), reopen the cell on the
     paused source VM with `PATCH /cladefs/{id}` `{"state": "Open"}` (which restores DAX window
     mappings) and resume it (`PATCH /vm` `{"state": "Resumed"}`).

### 3. When the snapshot is finished with, everywhere

```
DropVersions(<the snapshotted VM's cell>, [version])
```

**This drops the snapshot's version on every host.** VMs already restored from it keep running and
keep their data. New restores fail with `NOT_FOUND` (`ENOENT`), on every host. A restore racing
the drop either fails or succeeds whole: one that succeeded keeps everything it inherited.

### Guests' vsock connections across a restore

A restored VM has none of the snapshotted VM's vsock connections: Firecracker resets them all, and
the guest's kernel takes the resets once the restored VM runs. That includes a connection the guest
was making while the snapshot was taken. Its `connect` may then succeed, and the connection is reset
right after (TASK-277). So in every agent of yours that connects from the guest to the host:

- **Make a new connection for each exchange.** Never keep one open across a point where the VM may
  be snapshotted, and expect it gone after a restore.
- **Treat a reset as "connect again", not as an answer.** Any of these ends the exchange: a
  `connect` that fails with `ECONNRESET`; a write that fails with `EPIPE`, `ENOTCONN`,
  `ECONNRESET` or `ECONNABORTED`; or a read that ends (EOF, 0 bytes) or fails with `ECONNRESET`
  before the whole answer is in. Make the exchange again on a new connection, a few milliseconds
  later, up to about a second's worth of tries.
- **Only resend what the host can safely see twice.** The host may have read the ask before the
  reset. Make the host's side idempotent, or give each ask an ID the host deduplicates on.

`crates/fxfs-automount/src/reconnect.rs` does this for cladefs's own guest agent (`fxfs-automount`,
`git-remote-fxfs`), whose asks to the sandbox manager are idempotent: `AttachBase` of a base that is
already attached does nothing. Copy its rules, or call it from Rust.

This needs cladefs's Firecracker series `fxdax-v1.16.1-r3` or later (from r8 on, `cladefs-v1.16.1-rN`). When it takes a snapshot, it
queues an RST behind any response still in the guest's receive queue. On older series, the guest
can take such a response after the reset: its connection then connects to nothing, and its reads
hang forever.

## Forking a base on any number of hosts at once

A fork of a cell with a writer, such as a base from `ImportImage`, is a commit on it, so only its
writer makes it, one host at a time. To fork a base on many hosts, make a version of it once,
then fork the version, as a snapshot's is restored:

```
# On the base's host, once nothing has it open:
version = MakeVersion(tenant, cell).version
SyncCell(tenant, cell)
# Then, as often as you like, on every host at the same time:
ForkCell(from_tenant=tenant, from_cell=cell, version=version)
```

The base stays writable on its host. What it writes after reaches no fork of the version: make a new
version to pass that on. `DropVersions` drops it everywhere, and the forks keep what they inherited.
`cladefsctl version TENANT/CELL` and `cladefsctl fork-version TENANT/CELL VERSION` do the same from a
shell.

## Limits

**Forks of a cell with a writer.** A fork of anything but a version, a VM's cell or a base, is a
commit on that cell, so its host needs to be its writer. Fork those on one host
at a time, or make a version first (above).

**Memory.** The pages a guest writes through its DAX window are its own, private copies, in its VM
process's memory (backlog doc-13), until the guest writes them back to the cell, as a page cache
writes back dirty pages, and the window shares them again. They are charged to the VM process, as
its guest memory is, not to the daemon or the cell's tenant: the daemon reserves none of it, so its
memory sizes its cache alone (TASK-501). Bound the VM process's memory, its cgroup, for guest memory
and private pages together; leave `memory.swap.max` non-zero (e.g. `max`) so `cladefsd`'s unified
memory governor can proactively swap cold anonymous guest RAM under host pressure, and enable
Firecracker's virtio-balloon `free_page_reporting` so guests return freed pages to the host.
Everything else the window maps is sealed extents' pages, shared and evictable.

**Sizing a VM's private pages.** The extent device's `dax_private_max_bytes` caps how many bytes of
them a VM holds at once (`PUT /cladefs/{id}`, in Firecracker's swagger spec): a multiple of the page
size, a page at least, the window's size at most; 1 GiB unless set, or the window's size if less.
The guest keeps under it, writing back as it nears it, so a smaller cap means earlier writeback.
The host refuses what would go past it: a `write(2)` fails `EIO`, and a write through a mapping
stops that VM (exit 159). The VM manager sizes it per VM (TASK-502): the cap bounds a burst of
writes between writebacks, which grows with how fast the guest writes (its vCPUs) and is memory the
VM process holds (weigh it against its guest memory). Forked pgbench at 2 vCPUs needed 1 GiB
(TASK-227); measure your workloads. The cap was a page count, `dax_private_max`, before TASK-502: a
device naming that field is refused.

**Extents.** The daemon holds a few hundred bytes of memory for each extent of every open cell, its
own and those it inherited: each fork of a base of _n_ extents holds about 220-470 bytes times _n_,
whichever VM has it open (measured on macOS, TASK-443.2; TASK-453 is to share it). StatCell says
how many a cell holds, `own_extents` and `inherited_extents` (TASK-454); `cladefsctl stat TENANT/CELL`
prints them. Read a base's before choosing how many forks of it a host serves. Of a cell a VM has
open, StatCell reads the count kept as it changes, at no cost; of one none has, it opens the cell
as a VM's would (its state read, its cell's head taken, the map of its extents built: time and,
until it closes it again, memory as its extents), as for every other field.
