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
2. For each tenant, call `Grant` with the tenant's ID, the directory of its stores, and its jails'
   uid and gid. It returns the path of a socket.
3. Bind-mount **that socket alone** into the tenant's jails. Never mount the daemon's default VM
   socket into a jail: it can take any branch by its ID.
4. Run jails with Firecracker's jailer, as a uid other than the daemon's. The default VM socket is
   mode `0600`, owned by the daemon, so that a mistake in step 3 still can't connect.
5. Start Firecracker with `--enable-pci`, on x86-64 and aarch64 alike: the extent device attaches
   over virtio-PCI alone, its DAX window a BAR of its own, and a VM with it on MMIO does not start
   (TASK-273).

Grants don't survive a restart of the daemon. After one, call `Grant` again, and bind-mount the new
socket.

To show tenants' cells on the host as files, start `cladefsd` with `CLADEFSD_MOUNT` naming a mount
version and `CLADEFSD_MOUNT_STORES` a directory for their stores: it mounts them with FUSE and serves
the Mount service (`proto/cladefs/mount/v1/mount.proto`, Go `go/mount/v1`) on the same control
socket. One daemon then holds every cell of the host, whether VMs or the mount serve it (TASK-475.2).

## Upgrading the daemon

Upgrade a host's daemon and Firecracker by draining the host: move its VMs off, then SIGTERM the
daemon, which closes every store before it exits. `docs/upgrading-a-host.md` is the runbook, and
says what a format change refuses.

## Restoring a snapshot, on any number of hosts at once

A VM snapshot's version is immutable. Any number of hosts read it, and none writes it: a
restore on one host changes nothing another host sees, and no copy of it goes stale. Each restore
is a fork of the version into a new branch of the VM's own.

A VM process can't name a branch: cladefsd ignores what it says, or any stolen ID would take another
tenant's data. The VM manager puts the snapshot's branch in the tenant's directory instead, and that
is the grant.

Without step 2 below, a restore on a host that has no copy fails with `ENOENT`.

### 1. On the snapshot's host, once it's taken

```
branch = StatStore(version).branch      # keep this with the snapshot
```

The version is in the bucket by the time the snapshot returns.

Use this `branch`. **Never use `point_branch` from the snapshot file: a VM process wrote it.**

### 2. On each host that will restore it, once, before its first `PUT /snapshot/load`

```
CreateStore(path = <tenant's directory>/<version>, capacity, branch)
```

| Result | Meaning | Do |
|---|---|---|
| OK | The store is made. The daemon takes the point at the first restore | Go on |
| `ALREADY_EXISTS` | A store is there, perhaps a VM process's | Use it only if `StatStore(path).branch == branch`. Otherwise stop |

A version needs no `token`, so ignore `PlaceStore` for it. Keep the copy for as many restores as you
like.

Only do this for the tenant whose snapshot it is: its VM processes can restore the version and read it.
They can't write it: opening a point to write fails with `EROFS`.

### 3. Load the snapshot

In `cladefs_overrides`:

| Field | Value |
|---|---|
| `copy_on_host` | The path from step 2 |
| `path_on_host` | The VM's store, in the tenant's directory |
| `cladefsd_socket` | The tenant's socket, as the jail sees it |

Do this as often as you like, on any host, at the same time. Each restore makes a new branch for
the VM.

If a restore fails with `ENOENT` although the copy from step 2 exists, the snapshot was deleted.

Name a `path_on_host` where no store is yet: the restore forks the version there. A store already
there is opened only if it holds exactly what the version does: the store the snapshot was taken
of, if its VM wrote nothing after the snapshot, or the fork an earlier restore of this snapshot
made there, if that VM wrote nothing. Any other store there, such as the snapshotted VM's own
store once it resumed and wrote on, fails the restore with `EEXIST` and is left as it is: its
guest, running from the snapshot's memory, would corrupt it (TASK-194). Without
`cladefs_overrides`, `path_on_host` is the snapshotted VM's own store.

### 4. When this host won't restore that snapshot any more

```
DropStore(version)
```

This ends the tenant's access on this host and frees the copy. Nothing needs it for restores to work
elsewhere, or to work again here: `CreateStore` takes it back.

`DropStore` fails with `FAILED_PRECONDITION` (`EBUSY`) while a VM process has the store open.
`cladefsctl drop DIR` does the same from a shell.

### 5. When the snapshot is finished with, everywhere

```
DeleteStore(version)
```

**This deletes the snapshot on every host.** VMs already restored from it keep running and keep
their data. New restores fail with `ENOENT`, on every host. A restore racing the deletion either
fails or succeeds whole: one that succeeded keeps everything it inherited.

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

A fork of a store that isn't a version, such as a base from `ImportImage`, is a commit on that
store's branch, so only the branch's writer makes it, one host at a time. To fork a base on many
hosts, make a version of it once, then fork the version, as a snapshot's is restored:

```
# On the base's host, once nothing has it open:
branch = ForkStore(from = <base>, to = <base version>, version = true).branch
# On each host that forks it, once:
CreateStore(path = <base version>, capacity, branch)
# Then, as often as you like, on every host at the same time:
ForkStore(from = <base version>, to = <VM's store>)
```

The base stays writable on its host. What it writes after reaches no fork of the version: make a new
point to pass that on. `DeleteStore(<base point>)` ends the point everywhere, and the forks keep
what they inherited. A version of a version fails with `INVALID_ARGUMENT`: fork the version itself.
`cladefsctl version SRC DST` makes one from a shell.

## Limits

**Forks of a store with a writer.** A fork of anything but a version, a VM's store or a base, is a
commit on that store's branch, so its host needs to be the branch's writer. Fork those on one host
at a time, or make a version first (above).

**Memory.** The pages a guest writes through its DAX window are its own, private copies, in its VM
process's memory (backlog doc-13), until the guest writes them back to the store, as a page cache
writes back dirty pages, and the window shares them again. They are charged to the VM process, as
its guest memory is, not to the daemon or the store's tenant: the daemon reserves none of it, so its
memory sizes its cache alone (TASK-501). Bound the VM process's memory, its cgroup, for guest memory
and private pages together. Everything else the window maps is sealed extents' pages, shared and
evictable.

**Sizing a VM's private pages.** The extent device's `dax_private_max_bytes` caps how many bytes of
them a VM holds at once (`PUT /cladefs/{id}`, in Firecracker's swagger spec): a multiple of the page
size, a page at least, the window's size at most; 1 GiB unless set, or the window's size if less.
The guest keeps under it, writing back as it nears it, so a smaller cap means earlier writeback.
The host refuses what would go past it: a `write(2)` fails `EIO`, and a store through a mapping
stops that VM (exit 159). The VM manager sizes it per VM (TASK-502): the cap bounds a burst of
writes between writebacks, which grows with how fast the guest writes (its vCPUs) and is memory the
VM process holds (weigh it against its guest memory). Forked pgbench at 2 vCPUs needed 1 GiB
(TASK-227); measure your workloads. The cap was a page count, `dax_private_max`, before TASK-502: a
device naming that field is refused.

**Extents.** The daemon holds a few hundred bytes of memory for each extent of every store open, its
own and those it inherited: each fork of a base of _n_ extents holds about 220-470 bytes times _n_,
whichever VM has it open (measured on macOS, TASK-443.2; TASK-453 is to share it). StatStore says
how many a store holds, `own_extents` and `inherited_extents` (TASK-454); `cladefsctl stat DIR`
prints them. Read a base's before choosing how many forks of it a host serves. Of a store a VM has
open, StatStore reads the count kept as it changes, at no cost; of one none has, it opens the store
as a VM's would (its state read, its branch's head taken, the map of its extents built: time and,
until it closes it again, memory as its extents), as for every other field.
