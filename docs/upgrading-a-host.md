# Runbook: upgrading a host's extent daemon by draining it

For whoever operates cladefs hosts: how a host's `cladefsd` and Firecracker are upgraded, and what
a format change refuses on the way. The design behind it is backlog doc-11 (TASK-141); the VM
manager's calls named here are `crates/cladefs/proto/cladefs/cladefsd/v1/cladefsd.proto`'s.

## Why a drain

The daemon holds its VMs' memory, their open cells and their connections, and hands none of it to
another process. When it exits, every VM on the host exits with 158 (`CladefsdLost`). So a daemon
is upgraded by emptying its host first: its VMs move to other hosts or stop, and only then does the
daemon stop.

cladefs migrates nothing (AGENTS.md, "No migrations, ever"). Each format says its version, and a
reader refuses any other. The components of one build run together, and two builds meet only where
their formats are the same. A VM moves only to a host whose build has the same formats as its own.

## What a format change refuses

Formats are freezing (doc-28, board 10033): once a family is frozen (its golden corpus under
`formats/`), a newer build reads every version of it from its oldest on, so its row below no longer
refuses the older build's data: the new daemon reads its cell directories, replica directory and
ephemeral cells in place. A rollback is still refused past any version change. Until its family
freezes, a row holds as written.

Read the release's notes for which of these changed. Each one changed means what its row says:

| Format | Refused when | How it shows |
|---|---|---|
| VM process and daemon wire (`wire::VERSION`) | A VMM and a daemon of different versions meet: the daemon refuses its `OPEN` | The VMM fails to make its extent device: "the extent daemon refused this VM process's wire version N". The daemon's log says "refusing a VM process of wire version N: this daemon's is M". |
| Device protocol (`VIRTIO_EXTENT_VERSION`) | A guest's driver meets a device of another version, or a snapshot of another version is restored | The guest's kernel log says "the device is of protocol version N, this driver of M: refused", and its extent filesystem doesn't mount. The VMM refuses the snapshot's load: "The snapshot's extent device is of protocol version N, this VMM's M". |
| Cell directories (`local_cell` `FORMAT`: the `store` file's first line `cladefs cell directory 18`, each `wal` slot, each segment log's header `FXLG` and version and each of its records, a local checkpoint's rest) and cladefs's records in them (`cell_dir::FORMAT`: `attached-bases` and `ancestry`, first line `cladefs cell records 1`; the `migrating` marker's `version`). **Frozen** (doc-28, board 10044): every later build reads 18 and 1 on | The new daemon opens a cell directory of a version it doesn't read: one before 18 (`cladefs extent store 17` or older), or a newer build's (a rollback) | `ENOTSUP`: "a cell directory saying \"cladefs extent store 17\": this build reads cladefs cell directory 18 to 18". A directory before 18 is made again, from its cell in the bucket if that format didn't change too; a log, start or record of an unread version refuses the cell's recovery, deleting nothing. |
| Cells in the bucket (`bucket_cell::layout::FORMAT`: every object kind says its version — heads, collector leases, identity, registry, references, retired marks, mirrors, objectid blocks, segment footers, own-extent footers, checkpoint tables/states/appendices, read evidence — and bucket numbers in keys are base 32). **Frozen** (doc-28, board 10038): every later build reads version 31 on | A daemon reads a bucket or cell object of a version it doesn't read (30 or older, or a newer build's) | `ENOTSUP`: "cell TENANT/CELL: bucket layout version N: this build reads version 31 alone" or "the bucket: bucket layout version N: ..."; the collector's pass stops, deleting nothing. |
| KMS objects and wrapped keys (`extent_keys::kms`: `bucket::FORMAT` 1, `bucket::WRAPPED_FORMAT` 2, `local::FORMAT` 1, `real::FORMAT` 1, `aws::FORMAT` 1, `openbao::FORMAT` 1). **Frozen** (doc-28, board 10042): every later build reads from each backend's oldest version on | A daemon reads a KEK object or wrapped key of a version its KMS backend doesn't read | Refused as not authentic ("of another format") before calling any external KMS. |
| Extent frames (`extent_frame::FORMAT`, each framed object's trailer, its version before its magic `cladefs3`). **Frozen** (doc-28, board 10040): every later build reads version 3 on | A daemon reads an object of a version it doesn't read: 2 or 1, an older build's, which said it as the magic's last digit, or one newer than its own (a rollback) | The read fails as corrupt, naming it: "an object's trailer: extent frame version 2: this build reads version 3 alone". |
| Block devices (`block_device::format::FORMAT`, a SQLite or memory cell's root, each node's trailer and each extent's owners' list). **Frozen** (doc-28, board 10052): every later build reads version 2 on | A cell's block device was written by a build of version 1, or by a newer one (a rollback) | The cell doesn't open: "block device version 1: this build reads version 2 alone". |
| Fxfs (`EARLIEST_SUPPORTED_VERSION`..=`LATEST_VERSION`, pinned at version 66). **Frozen** (doc-28, board 10050): every later build reads version 66 on | A guest mounts a filesystem or attaches a base manifest of an unsupported version (65 or older, or a newer build's) | The mount or manifest attach fails with `InvalidVersion` (`EINVAL`). |
| VM snapshots (Firecracker's, `virtio_extent::persist::FORMAT`: version-prefixed extent device state with `dax_private_max_bytes`). **Frozen** (doc-28, board 10058): every later build reads version 2 on | A VMM restores a snapshot of another snapshot version, or an extent device state of a version it doesn't read (1 or older, or a newer build's) | The load fails: "snapshot extent state version N: this build reads version 2 alone". |
| Peer service (`peer.proto`, `cladefsd::peers::current_formats`: per-family `Formats` versions for cell directory, cell records, extent frame and bucket layout; `buf breaking` in CI, doc-28, board 10054) | A live migration's target does not read the source's cell directory, cell records, extent frame, or bucket layout version (or receives an older prepare naming no `versions`), refusing before noting, referring to or staging anything (board 8455); or a hand-over's staged cell fails to open | `MigrateCell` fails with `ENOTSUP`; the source logs "preparing migration M at PEER: ... the source's formats are not read here: ..." and serves the cell on. A refused hand-over is `HandOverCell`'s `EAGAIN`, the cell served on the source again. |
| Ephemeral host's files (`ephemeral_host::FORMAT`, the first line of `CLADEFSD_EPHEMERAL`'s `incarnation`, each `cells/` note and each `accepted/` record: `cladefs ephemeral host 2`). **Frozen** (doc-28, board 10048): every later build reads version 2 on | A daemon reads a file of a version it doesn't read: 1, an older build's (board 8461), or one newer than its own (a rollback) | The daemon refuses to start: `Error: reading DIR/incarnation: ephemeral host version 1: this build reads version 2 alone`, or the same of a note. Nothing is deleted: the cells' directories, their notes and their references stay as they were. |

Names change too, refused the same way, never read under both. Since TASK-503 the control API is
`cladefs.cladefsd.v1.Cladefsd` (Go: `go/cladefsd/v1`), not `cladefs.extentd.v1.Extentd`, whose
calls a new daemon answers `UNIMPLEMENTED`; and Firecracker's API says `cladefs`: `PUT
/cladefs/{id}`, a configuration file's `cladefs` list, `cladefs_overrides` and `cladefsd_socket`.
A configuration file still saying `extent` is ignored, as Firecracker ignores keys it doesn't
know, and the VM boots without that disk (TASK-671 dropped the check); `PUT /extent/{id}` is an
unknown path; `extentd_socket` and `extent_overrides` are unknown fields. A VM manager is upgraded
with the host's build. The guest kernel's filesystem module is `cladefs` too: a guest mounts
with `mount -t cladefs` (`fxfs` is an unknown type), sets `cladefs.*` parameters and reads the
`cladefs` tracepoints and `/proc/fs/cladefs`, so guest images and agents move with the guest
kernel. The VM snapshots and cell directories are unchanged.

### Daemons of two builds side by side

In steps 1 to 6 the hosts of a clade run two builds, and their daemons call each other over the
peer service (`peer.proto`, `CLADEFSD_PEER_ADDRESS`), authenticated by mutual TLS with the hosts'
credentials (`CLADEFSD_PEER_TLS`, board 8437): give every host of both builds the same CA's. A
daemon of a build before it calls in the clear, which the new refuses, and is refused by:

- `GetPart` names no format, and needs none: a part is the bucket's bytes, the same to every
  build, and the asker checks it against its own tail of the object, fetching it from the bucket
  on any failure.
- A live migration of an ephemeral cell (`MigrateCell`, `HandOverCell`) checks formats by name,
  as the table says: an ephemeral cell moves only between hosts of the same formats. A target
  takes a migration only to a new name in its `CLADEFSD_EPHEMERAL_CELLS` directory, and only of a
  source in its own bucket (`CLADEFSD_BUCKET`); a host without the variable takes none
  (`ENOTSUP` at the source). So drain a host of ephemeral cells by migrating them to a host of its
  own build. Past a change of any format `cladefsd::peers::formats` names, or of the ephemeral
  host's files, its ephemeral cells can't follow it to the new build: their only copy is on its
  disk, and nothing reads it there. Stop their VMs and delete them (`DeleteCell`) before the drain.

A wire or device protocol change alone leaves cell directories and cells readable: VMs move by
snapshot to a host of the new build only if the device protocol is the same, and stop and boot
again there otherwise. A cell directory or bucket format change means its cell directories are
made again, not upgraded.

## Steps

1. **Cordon the host.** Have the scheduler place no new VMs on it.
2. **Move its VMs off, or stop them.** Move each by snapshot and restore through the bucket, as
   `scripts/migrate-host.sh` does, to a host whose build has the VM's formats (above); the
   integrator guide's "Restoring a snapshot" has the calls. Stop a VM whose tenant accepts that.
   When the host is empty, its daemon's `/metrics` says
   `cladefsd_connections 0`.
3. **Stop the daemon with SIGTERM.** It drains, then exits (`cladefs::cladefsd::drain`, TASK-141.4):
   - its health service reports NOT_SERVING;
   - its VM sockets refuse connections, and its control API refuses calls with `UNAVAILABLE`;
   - any VM still connected is cut off and exits 158, its last commits made local and its cell
     closed;
   - the versions it kept open for forks are closed;
   - every open cell's uploader uploads what its cell made local, and stops.

   It does all of that within `CLADEFSD_DRAIN` seconds (60 by default). If it finishes, it logs
   "drained in ...: every store closed" and exits 0. If not, it logs "not drained within Ns: ...
   left", naming how many connections, control calls and uploaders were left, and exits 1. Then the
   cells' local commits are still on the host's disk, and the next daemon to open each cell
   uploads the rest.
4. **Install the new daemon and Firecracker together.** They are one build. A guest kernel is
   upgraded as its VM next boots, with the Firecracker it boots on. It isn't part of this.
5. **Start the daemon, and check it.** Its health service says SERVING, and `/metrics` says the new
   build: `cladefsd_build_info{commit="..."} 1`. Call `Grant` again for each tenant, because
   grants don't survive a restart.
6. **Uncordon the host.** Moving VMs back is step 2 again, onto this host. Each one restored here
   then runs on the new Firecracker, which `cladefsd_vm_build_info` shows by cell.

## Rolling back

A rollback is the same drain, to the previous build. It works only if no format changed between the
two builds. Past a format change, the old build refuses what the new one wrote, by the rows above:
that isn't supported, by design.
