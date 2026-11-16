# Operating a host

What a host's extent daemon, `cladefsd`, takes of the host and how to bound it: memory, disk, the
bucket and the placement it must be given, encryption, and what a VM process's exit status says.
The VM manager's side, the control API call by call, is `integrator-guide.md`; upgrades are
`upgrading-a-host.md`; how the daemon works is `architecture.md`.

## The daemon's memory

DAX moves the guests' file pages out of their RAM into
the daemon's (TASK-50.3). Its physical memory is a page cache, governed
(TASK-358): as much of the host's memory as is free, less a headroom
(`CLADEFSD_HEADROOM` bytes, by default a sixteenth of the host's memory, 1 GiB
at least), given back as the host's free memory falls. `MEMORY_BYTES`, its
second argument, fixes it at that instead, ungoverned, as a test's daemon may
have it. Arenas take it in 2 MiB chunks as they need them. The control API limits, at runtime, the memory
of each tenant and of every tenant together (`SetMemoryLimit`): past a
tenant's limit, the daemon evicts the clean pages of its extents read longest
ago; past the total, or out of `MEMORY_BYTES`, any tenant's. With none left to
evict, what would take more fails with `ENOMEM`, which the guest sees as an I/O
error, and the daemon goes on. `ShrinkMemory` evicts toward a target at once,
as the host's memory runs low. `/metrics` shows the memory held, every
tenant's together and each tenant's, by its ID in hex (`tenant="a"`). Under cgroup
v2 the arenas' memfds are shmem, charged to the cgroup of the process that
first touches each page, and uncharged when the chunk goes back (a hole
punched): that is the daemon, which faults each chunk in as an arena takes it
and writes pages before mapping them to a VM. So the daemon's cgroup is charged
for every VM's cached file pages, and the VMs' cgroups are not: give the
daemon a cgroup of its own whose `memory.max` holds `MEMORY_BYTES` and its heap
(the stores' metadata, the connections' buffers, the uploads in flight), and limit tenants through the control API, as cgroups cannot see them apart.
A bucket behind costs the heap no more than its upload memory, `CLADEFSD_UPLOAD_MEMORY`
(512 MiB by default): what waits is on the host's disk (backlog doc-7, Uploads, Memory); on ubu1
a docker build's backlog held the daemon's anonymous memory to 718 MB, against 5.9 GB before (TASK-317). A
memory.max under that is met by the cgroup's OOM killer, not by eviction:
lower `MEMORY_BYTES` or the total limit instead.

## The host's disk

A tenant takes what is in its directory, the one its VM
processes name stores in: its stores' open segments and their logs, their
write-ahead logs' segments not yet acknowledged in the bucket, their own
extents, each file's blocks once, as `du -sx` counts them (TASK-94). One rule
for what is shared: a file is the tenant's whose directory holds it, and one in
no tenant's directory is no tenant's. A fork's directory holds what the fork
wrote, its base's data being in the bucket, read through the host's part cache
(`CLADEFSD_PART_CACHE`), which is no tenant's: copies of objects any tenant
reads, bounded by its own capacity and floor, evicting to keep within them. The
daemon walks a tenant's directory as it is granted, every 10 s, soon after a
write of it is refused, and as `GetTenantDisk` asks, and counts each write it
admits between walks. The control API limits, at runtime, the disk of each
tenant and of every tenant together (`SetDiskLimit`, none by default): past a
tenant's limit its VMs' writes, and the stores they make, fail with `ENOSPC`,
which the guest sees as a full disk; the daemon goes on, and other tenants'
writes do. Its commits go on, and what leaves its disk, uploaded or freed, or a
store deleted, lets it write again. `/metrics` shows each tenant's disk, by its
ID, as memory. A filesystem's directory quota would not do it: it
refuses whichever write of the directory comes past it, a store framing a closed
segment, which fails the store, or a flush appending a commit's record, rather
than the guest's next write, which the daemon refuses before the store takes
any of it; and it is the host's setup, of a filesystem that has them, which the
daemon can't make as it grants tenants. Set a tenant's limit above what its
stores need at once: each holds about the lag bound (1 GiB) not yet uploaded,
and its open segment.

## Multi-node only

A branch's durable copy is in a bucket, and hosts hold
copies of it, whose placements fence each other with their tokens: a placement
cut off neither takes a branch back nor deletes it from under a later one
(TASK-39.7, TASK-45). The daemon runs as a host of that system, with the bucket
(`CLADEFSD_BUCKET`) and its placement's token from the control plane
(`CLADEFSD_TOKEN`, not 0), and refuses to start without them: there is no
single-node mode. Tests and development use an in-memory bucket
(`memory:///NAME`), or one in a directory of the host's (`file:///DIR`), which
outlives the daemon and is shared with cladefsctl and cladefs-gc. What a bucket has
acknowledged leaves the store's directory (TASK-165.8): an in-memory bucket is
its process's, and what left for it is gone once that process exits, so a store
used by more than one process, or across a daemon's restarts, needs a file or
cloud bucket. cladefsctl, without the daemon, changes existing stores'
branches on the same terms, and makes new stores (`take`, `import`) anywhere.

## Encryption and shredding

Backlog doc-16 is the design. With `CLADEFS_KMS` naming the
host's KMS (`openbao://`, `aws-kms://` or `gcp-kms://`; `local:///DIR`, a fake
for tests and development, its KEKs files in a directory), the cells of a tenant whose KEK it holds
are encrypted: the bucket, the stores' directories and the part cache hold their
data encrypted. To shred a **tenant**, destroy its KEK in the KMS, then tell
every host (`cladefsctl forget-tenant-keys TENANT`, the control API's
`ForgetTenantKeys`): absolute, as every cell KEK of the tenant, in every
version of every object, was wrapped by it. To shred a **cell**, run
`cladefsctl shred-cell TENANT/CELL` (`ShredCell`) against every host's daemon:
it takes the cell's wrapped KEKs out of its object `<C>cell` and has each host
forget them. That is only as strong as the bucket keeps no older version of
that object, so a bucket with versioning on needs a lifecycle rule expiring
non-current versions: on S3, `NoncurrentVersionExpiration` (with
`NoncurrentDays` as short as the deployment's recovery needs allow); on GCS,
object versioning off, or a lifecycle rule deleting non-current versions
(`isLive: false`, or `daysSinceNoncurrentTime`). Until that rule has run, the
old version's wrapped KEK unwraps while the tenant's KEK lives. A bucket with
versioning off needs nothing. A host told either halts the stores of the
tenant or cell it has open (`EACCES`) and drops their keys and cached pages;
a halted store's own keys stay in memory until its VM process closes it.

## Access to userfaultfd

On Linux, DAX windows take KVM's faults through userfaultfd, which needs
access to `/dev/userfaultfd` (or `vm.unprivileged_userfaultfd=1`). To grant it
to the `kvm` group:

```sh
echo 'KERNEL=="userfaultfd", GROUP="kvm", MODE="0660"' |
    sudo tee /etc/udev/rules.d/99-userfaultfd.rules
sudo udevadm control --reload && sudo udevadm trigger --name-match=userfaultfd
```

## How a VM process exits

What a host can tell from a VM process's status (TASK-70):

| Status | Name | Whose it is |
|---|---|---|
| 0 | `Ok` | The guest shut down or reset. |
| 1 | `GenericError` | A VMM failure not named below. |
| 2 | `UnexpectedError` | A VMM bug: not possible if its logic is sound. |
| 148 | `BadSyscall` | A VMM bug, or a compromised VMM: the seccomp filter stopped a system call. |
| 149-151, 154-156 | `SIGBUS`, `SIGSEGV`, `SIGXFSZ`, `SIGXCPU`, `SIGPIPE`, `SIGHUP` | The VMM's handler caught the signal: a bug, or the host's limits, by the signal. |
| 152, 153 | `BadConfiguration`, `ArgParsing` | How it was started. |
| 157 | `SIGILL` | A VMM bug, or a host of another CPU. |
| 158 | `ExtentdLost` | Storage: its extent daemon went, and the VM can't go on. What it was told stays as doc-12 says. |
| 159 | `AccessRefused` | The guest: an access stayed refused, of a part of its DAX window nothing maps, or a store past its private pages' bound (`private_max`, backlog doc-13). That VM alone stops (TASK-77). |
| Killed by SIGABRT | A panic | A VMM bug: Firecracker panics abort, and an unresolved DAX window fault does. |
