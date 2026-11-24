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

1. Start `cladefsd` with `CLADEFSD_GRPC` naming its control socket. Keep that socket away
   from jails. It can do everything, so it is mode 0600, the daemon's user's, whatever the umask
   (TASK-159): run the VM manager as that user, or as root.
2. For each tenant, call `Grant` with the tenant's ID, the directory of its cell directories, and its
   jails' uid and gid. It returns the path of a socket.
3. Bind-mount **that socket alone** into the tenant's jails. Never mount the daemon's default VM
   socket into a jail: it can take any cell by its ID.
4. Run jails with Firecracker's jailer, as a uid other than the daemon's. The default VM socket is
   mode `0600`, owned by the daemon, so that a mistake in step 3 still can't connect.
5. Start Firecracker with `--enable-pci`, on x86-64 and aarch64 alike: the extent device attaches
   over virtio-PCI alone, its DAX window a BAR of its own, and a VM with it on MMIO does not start
   (TASK-273).

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
| `path_on_host` | The VM's cell directory, in the tenant's directory |
| `cladefsd_socket` | The tenant's socket, as the jail sees it |

Do this as often as you like, on any host, at the same time. Each restore makes a new cell for the
VM; the daemon opens the version once on a host for all the restores of it there.

Name a `path_on_host` where no cell directory is yet: the restore forks the version there. A cell
directory already there is opened only if it holds exactly what the version does: the cell the
snapshot was taken of, if its VM wrote nothing after the snapshot, or the fork an earlier restore
of this snapshot made there, if that VM wrote nothing. Any other cell there, such as the
snapshotted VM's own cell once it resumed and wrote on, fails the restore with `EEXIST` and is
left as it is: its guest, running from the snapshot's memory, would corrupt it (TASK-194). Without
`cladefs_overrides`, `path_on_host` is the snapshotted VM's own cell directory.

A VM's memory restored from a memory cell names the version in `memory_cell`'s `tenant`, `cell`
and `version`, as the disk's are named in the snapshot file.

### Memory cells: snapshotting and restoring without local memory files

A VM's guest memory can live in a **memory cell** (`CELL_KIND_MEMORY`) instead of a local memory
file, so every VM restored from the same snapshot on a host shares unmodified pages straight out of
the extent daemon's cache (copy-on-write on first guest store).

There are two ways to create a snapshot's memory cell version:

1. **Directly from Firecracker (`PUT /snapshot/create`, preferred):** pass `memory_cell` in place of
   `mem_file_path` — no local memory file is written:
   - **Cold-booted (or File/Uffd-restored) VM:** pass
     `"memory_cell": { "path_on_host": "<dir>", "cladefsd_socket": "<tenant.sock>" }`. Firecracker
     scans resident pages via `mincore` (skipping untouched anonymous RAM), writes non-zero pages,
     commits, and makes a version.
   - **VM already restored from a memory cell (`MemBackendType::Cell`):** pass `"memory_cell": {}`
     (omit both fields). Firecracker writes only the window's private pages into the VM's forked
     memory cell, commits, reclaims them back to the newly committed extents, and makes a version.
   - On success, `PUT /snapshot/create` returns `200 OK` with JSON
     `{"tenant": <u64>, "cell": <u64>, "version": <u64>}`. Call `SyncCell(<mem_cell_dir>)` before
     restoring on another host.
2. **Importing an existing Firecracker memory file (`ImportMemory` / `cladefsctl import-memory`):**
   call `ImportMemory(path, file, tenant, make_version=true)` over the control API (or
   `cladefsctl import-memory --version DIR FILE`). Zero pages are discarded as holes; call
   `SyncCell` before cross-host restore.

To restore with KVM dirty-page tracking (for live migration pre-copy), set `"track_dirty_pages": true`
in `PUT /snapshot/load` alongside `mem_backend.backend_type = "Cell"`: KVM tracks guest stores
(including first-time COW into private pages and subsequent writes to private pages), while
unmodified shared pages remain read-only and clean.

## Live migration of bucket-backed cells (tail-only peer hand-over)

Both ephemeral and bucket-backed cells migrate live between two hosts sharing a bucket over peer
mTLS (`CLADEFSD_PEER_ADDRESS` / `CLADEFSD_PEER_TLS`), using `MigrateCell`, `HandOverCell`, and
`AbortMigration` (or `cladefsctl migrate`, `cladefsctl hand-over`, `cladefsctl abort-migration`).

For a **bucket-backed cell**, only data not yet acknowledged in the bucket is transferred over peer
mTLS, avoiding any bucket upload wait during the VM pause:

1. **Configure target roots:** start the target `cladefsd` with `CLADEFSD_ROOTS` (or
   `CLADEFSD_EPHEMERAL_CELLS`) containing the destination directory; `to` must lie inside one of
   those roots. `CLADEFSD_EPHEMERAL` is not required for bucket-backed migrations.
2. **Pre-copy while the VM runs:** call `MigrateCell(path, peer, to)` on the source daemon. The
   source's uploader keeps uploading finalized segments to the bucket in parallel, while migration
   rounds stream only unacknowledged finalized `segments/` and `extents/` over peer mTLS (skipping
   any that finish uploading before being sent).
3. **Pause and hand over:** pause/stop the source VM so its connection to `cladefsd` closes, then
   call `HandOverCell(path, token=<new_fencing_token>)` on the source daemon:
   - The source cuts its uploader short immediately without draining pending bucket uploads, writes
     the durable `migrating` marker, and sends only the remaining unuploaded tail (open segment,
     WAL, logs, store metadata, and any unsent finalized segments/extents).
   - The target prunes any pre-copied segments already uploaded on the source, places the cell at
     `token` (if provided), renames it into place, and takes its head in the bucket with a single
     CAS (fencing the source) before immediately closing with `cut_short` so no bucket data uploads
     block the pause.
   - Once accepted, the source deletes its local cell directory (leaving the cell intact in the
     bucket) and returns `OK`. Start/resume the VM on the target against `to`; the target's
     uploader uploads the transferred tail to the bucket in the background.
4. **Abort / recovery:** call `AbortMigration(path)` to cancel before hand-over (the source's
   uploader resumes normal drain-on-stop) or to resolve an uncertain hand-over. If the target
   refuses (`EAGAIN`, e.g. a stale fencing `token`), the source removes its `migrating` marker and
   resumes serving locally.

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
version = MakeVersion(<base>).version
SyncCell(<base>)
# Then, as often as you like, on every host at the same time:
ForkCell(to = <VM's cell>, version, tenant, cell)
```

The base stays writable on its host. What it writes after reaches no fork of the version: make a new
version to pass that on. `DropVersions` drops it everywhere, and the forks keep what they inherited.
`cladefsctl version DIR` and `cladefsctl fork-version CELL VERSION DST` do the same from a shell.

## Limits

**Forks of a cell with a writer.** A fork of anything but a version, a VM's cell or a base, is a
commit on that cell, so its host needs to be its writer. Fork those on one host
at a time, or make a version first (above).

**Memory.** The pages a guest writes through its DAX window are its own, private copies, in its VM
process's memory (backlog doc-13), until the guest writes them back to the cell, as a page cache
writes back dirty pages, and the window shares them again. They are charged to the VM process, as
its guest memory is, not to the daemon or the cell's tenant: the daemon reserves none of it, so its
memory sizes its cache alone (TASK-501). Bound the VM process's memory, its cgroup, for guest memory
and private pages together. Everything else the window maps is sealed extents' pages, shared and
evictable.

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
how many a cell holds, `own_extents` and `inherited_extents` (TASK-454); `cladefsctl stat DIR`
prints them. Read a base's before choosing how many forks of it a host serves. Of a cell a VM has
open, StatCell reads the count kept as it changes, at no cost; of one none has, it opens the cell
as a VM's would (its state read, its cell's head taken, the map of its extents built: time and,
until it closes it again, memory as its extents), as for every other field.
