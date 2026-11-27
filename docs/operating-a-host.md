# Operating a host

What a host's extent daemon, `cladefsd`, takes of the host and how to bound it: memory, disk, the
bucket and the placement it must be given, encryption, and what a VM process's exit status says.
The VM manager's side, the control API call by call, is `integrator-guide.md`; upgrades are
`upgrading-a-host.md`; how the daemon works is `architecture.md`.

## Configuration (TOML)

`cladefsd`, `cladefs-gc`, and `cladefsctl` read a single TOML configuration file
via `--config PATH` / `-c PATH` or `CLADEFS_CONFIG=PATH`. See
[`docs/cladefs.example.toml`](cladefs.example.toml) for every section and option.

```sh
cladefsd --config /etc/cladefs/cladefs.toml
cladefsctl --config /etc/cladefs/cladefs.toml disks
cladefs-gc --config /etc/cladefs/cladefs.toml
```

In a TOML config, `[[disks]]` declares the host's managed disk pool:

```toml
[[disks]]
id = "nvme0"
path = "/mnt/nvme0/cladefs"
cap_bytes = "1TiB"           # optional hard byte cap on this disk
headroom = "5%"              # bytes or percentage kept free (overrides [disk].headroom)
cache_cap_bytes = "512GiB"   # optional cap on this disk's part-cache shard
dirty_budget_bytes = "256MiB"# optional per-disk cell page-cache dirty budget
placement = "read_write"     # read_write | drain | read_only
```

`cladefsd` owns the on-disk layout under each disk root (`<path>/cells/<TENANT>/<CELL>`,
`<path>/part-cache/`, `<path>/replicas/`) and chooses which disk holds each new cell based on
placement mode, health state, and remaining spare capacity. Callers never pass host filesystem paths
to control RPCs (`CreateCell`, `ForkCell`, `ImportImage`, `Grant`, `MigrateCell`, etc.) — every cell
is addressed by `(tenant, cell)` alone.

Legacy `CLADEFSD_*` environment variables remain supported for compatibility and override TOML
settings only when no `--config` file is passed.

## The daemon's memory

DAX moves the guests' file pages out of their RAM into
the daemon's (TASK-50.3). Its physical memory is a page cache, governed
(TASK-358): as much of the host's memory as is free, less a headroom
(`CLADEFSD_HEADROOM` bytes, by default a sixteenth of the host's or cgroup's
limit, 1 GiB at least and half the limit at most), never below a shared working-set
floor (`CLADEFSD_MEMORY_FLOOR` bytes, by default an eighth of the limit: e.g.
~3.5 GiB on a 28 GiB host, 8 GiB on a 64 GiB host). On a soft `MemAvailable` dip
without kernel memory pressure, the governor sheds derived caches, spare slabs,
and idle (unmapped) extents and stops growing without evicting actively mapped
DAX extents; it shrinks mapped extents gradually only under own-cgroup limits,
real Linux PSI memory pressure, or emergency host shortage (`< headroom / 4`),
and backs off above emergency if governor-caused refault thrashing is detected.

On Linux, governed mode unifies `cladefsd`'s shared `memfd` extent cache and
connected VMs' anonymous RAM under one MGLRU + swap aging clock:
- **Fail-hard startup prerequisites:** governed `cladefsd` refuses to start unless
  Linux 6.5+ `cachestat(2)` works, MGLRU is enabled (`/sys/kernel/mm/lru_gen/enabled != 0`),
  `SwapTotal >= floor`, `/sys/fs/cgroup` is writable, and the daemon holds `CAP_DAC_OVERRIDE`
  (or `CLADEFSD_VM_CGROUP_DELEGATED=1` / `[memory].vm_cgroup_delegated = true` when VM cgroups
  are delegated to `cladefsd`'s user). Run `cladefsd --setup-host` as root (`ExecStartPre=+`)
  to enable MGLRU, set `lru_gen/min_ttl_ms=1000`, and provision zstd zram swap automatically.
- **Cold-slab harvesting:** a budgeted background worker polls `cachestat(2)` outside extent
  locks and harvests individual parts MGLRU has completely paged out to swap on two consecutive
  scans (never during thrashing).
- **Proactive VM reclaim (TMO):** when raw host `MemAvailable < headroom` and host swap has room
  above reserve (`max(SwapTotal / 8, 256 MiB)`), the governor discovers connected VM cgroups
  race-free from `SO_PEERPIDFD` and writes deficit-budgeted `memory.reclaim` (`swappiness=max`)
  only to VMs with observable low PSI (`some avg10 < 1%`, `full avg10 == 0`), holding its own
  cache growth for 5 s after progress.
- **Cgroup protection:** place `cladefsd.service` in a top-level `Slice=cladefs.slice` with
  `MemoryLow=` set to at least the floor on both the slice and the service (Linux cgroup v2 caps
  effective `memory.low` by every ancestor).

`MEMORY_BYTES`, its second argument (or `[memory].bytes` in TOML), fixes it at that instead,
ungoverned, as a test's daemon may have it. Arenas take memory in 1 MiB part slabs (doc-26) as
they need them. The control API limits, at runtime, the memory
of each tenant and of every tenant together (`SetMemoryLimit`): past a
tenant's limit, the daemon evicts the clean pages of its extents read longest
ago; past the total, or out of `MEMORY_BYTES`, any tenant's. With none left to
evict, what would take more fails with `ENOMEM`, which the guest sees as an I/O
error, and the daemon goes on. `ShrinkMemory` evicts toward a target at once,
as the host's memory runs low. `/metrics` shows the memory held, every
tenant's together and each tenant's, by its ID in base 32 (`tenant="a"`). Under cgroup
v2 the arenas' memfds are shmem, charged to the cgroup of the process that
first touches each page, and uncharged when the chunk goes back (a hole
punched): that is the daemon, which faults each chunk in as an arena takes it
and writes pages before mapping them to a VM. So the daemon's cgroup is charged
for every VM's cached file pages, and the VMs' cgroups are not: give the
daemon a cgroup of its own whose `memory.max` holds `MEMORY_BYTES` and its heap
(the open cells' metadata, the connections' buffers, the uploads in flight), and limit tenants through the control API, as cgroups cannot see them apart.
A bucket behind costs the heap no more than its upload memory, `CLADEFSD_UPLOAD_MEMORY`
(512 MiB by default): what waits is on the host's disk (backlog doc-7, Uploads, Memory); on the test host
a docker build's backlog held the daemon's anonymous memory to 718 MB, against 5.9 GB before (TASK-317). A
memory.max under that is met by the cgroup's OOM killer, not by eviction:
lower `MEMORY_BYTES` or the total limit instead.

## The host's disk

Governed as the memory is (backlog doc-22): every filesystem the daemon keeps
cell directories or its part cache on is the daemon's but for a headroom
(`CLADEFSD_DISK_HEADROOM`, bytes or `N%`; by default a twentieth of the
filesystem, 4 GiB at least, half of it at most), and, if `CLADEFSD_DISK_BYTES`
is set, no more than that on all of them together. A disk governor looks at
each filesystem every 100 ms (`fstatvfs`), and at once when a write is refused:
what the daemon may keep there, its allowance, is what it holds and what is
free, less the headroom and what it holds not yet on the disk. A machine given
to cladefs alone needs nothing set.

What is counted is lengths: each file of a cell directory's, rounded up to 4 KiB, once
however many names it has, and its directories, counted as the cell writes
them, as its page cache takes a write, not when the write reaches the disk; and
what the part cache keeps. Nothing walks a directory to count it but once, as a
tenant is granted, and as a cell is opened by this process after another had it
(TASK-539.5). `GetTenantDisk` and `/metrics` read those counts.

The cells come first. The part cache (`CLADEFSD_PART_CACHE`, one or more colon-separated
directories or glob patterns such as `/mnt/nvme*/cladefsd/part-cache` across JBOD disks), copies
of the bucket's objects that any tenant's cells read, which is no tenant's, keeps what the cells
leave of each disk's allowance, `CLADEFSD_PART_CACHE_BYTES` at most across all shards if set, and
gives it back at once as they need it, least recently read first. Put one part-cache shard on
every JBOD disk that holds cell directories so acknowledged WAL segments adopt in place via
zero-copy `rename(2)` without `EXDEV`; objects fetched from the bucket distribute across healthy
shards by weighted rendezvous hashing proportional to each disk's size, and per-shard health
(`cladefsd_part_cache_shard_state`) isolates failing drives without stalling healthy ones. Their
cells near the allowance, within the headroom of it, the disk is under
pressure: their uploaders are urged, closing and uploading their open segments
at once, and each tenant's writes are refused past its share, by its weight
(`SetDiskWeight`, 1 each by default), of what the allowance leaves the tenants.
Past the allowance, writes are refused with `ENOSPC`, which the guest sees as a
full disk; the daemon and every cell go on.

### Multi-disk JBOD hosts and per-disk governance

Declare each independent NVMe filesystem as a `[[disks]]` entry in the TOML config (no RAID0
required). Each disk gets:

- **Automatic placement & layout:** `<path>/cells/<TENANT>/<CELL>`, `<path>/part-cache/`, and
  `<path>/replicas/` (unless overridden in `[cache]` / `[peers]`).
- **Per-disk hard caps & headroom:** `cap_bytes` caps total cladefs footprint + part cache on that
  disk; `headroom` overrides the global `[disk].headroom` per disk; when `[disk].max_bytes`
  (`CLADEFSD_DISK_BYTES`) is set without per-disk caps, the global cap is apportioned across disks
  proportionally to each disk's effective capacity (`apportion_cap`).
- **Per-disk part-cache cap & weighting:** `cache_cap_bytes` caps the part-cache shard on that disk;
  shards are weighted by effective capacity (`min(fs_size, cap_bytes)`), and `drain` / `read_only`
  disks stop accepting new part-cache fills while continuing to serve hits.
- **Per-disk dirty-page budget & flusher:** `dirty_budget_bytes` caps dirty `PageCache` pages on
  that disk's dedicated `DiskFlusher` thread.
- **Placement modes & runtime health:** `placement` is `read_write` (new cells + writes + fills),
  `drain` (no new cells or fills; existing cells still writable), or `read_only` (writes refused
  with `ENOSPC`; reads continue). Runtime I/O health (`online` / `suspect` / `offline` / `returning`)
  tracks consecutive I/O failures and isolates failing drives automatically.
- **Runtime control & metrics:** inspect disks with `cladefsctl disks` (`ListDisks`) and adjust
  caps, headroom, dirty budget, or placement at runtime with `cladefsctl set-disk ID ...`
  (`SetDiskGovernance`). `/metrics` exposes `cladefsd_host_disk_cap_bytes`,
  `cladefsd_host_disk_cache_cap_bytes`, `cladefsd_host_disk_state`, and
  `cladefsd_host_disk_placement` alongside the existing per-disk footprint/allowance gauges.

Without a TOML config, legacy `CLADEFSD_ROOTS` / `CLADEFSD_PART_CACHE` / `CLADEFSD_REPLICAS` /
`CLADEFSD_EPHEMERAL_CELLS` / `CLADEFSD_MOUNT_STORES` accept colon-separated directories across
independent filesystems. `CLADEFSD_EPHEMERAL` stays a single small metadata journal directory (tens
of KiB).

Each write is admitted by its cell before it takes any of the disk: what it
writes, and what the cell's own work may still add for it, its commit copying
it, its log holding it, its framing copying it, which nothing refuses; three
times what is written and not framed, and what open extents hold, holes and all,
and 64 KiB an open cell (measured, `local_cell/tests/reserve.rs`). So data
written and not committed costs seven times its size against a limit until its
commit, four times until it is framed, and its size until it uploads, when it
leaves the cell directory. Guests that commit as they write, as filesystems
do, see the difference only near a limit. Commits go on, whatever is full:
what was written commits, uploads, and leaves the disk, as do frees; a commit's
own records, some 115 bytes each, are not admitted (TASK-539.15), and the
headroom absorbs them.

The control API limits, at runtime, the disk of each tenant and of every tenant
together (`SetDiskLimit`, none by default), which admission holds as hard
bounds: what a tenant's cells take, reserve and have admitted. A tenant takes
what its cells' files do, the cells in its directory, the one its VM processes
name cells in; a cell in no granted directory is no tenant's, counted on its
disk alone. A fork's directory holds what the fork wrote, its base's data being
in the bucket. Set a tenant's limit above what its cells need at once: each
holds about the lag bound (1 GiB) not yet uploaded, and its open segment, each
with its reserve.

`/metrics` shows, for each disk by its device (`MAJOR:MINOR`), its size, free
bytes, headroom, allowance, what its cells take, of that unframed, their
reserves and writes admitted, the cache, the spare the cells leave of the
allowance, what the filesystem holds that the daemon counts as none of its own
(`cladefsd_host_disk_other_bytes`: on a dedicated disk, its metadata; more, an
uncounted writer), and whether it is under pressure; and each tenant's bytes,
reserve, limit, weight and, under pressure, share, by its ID. The stats file
says the disks on a `disks:` line.

A filesystem's directory quota would not do it: it refuses whichever write of
the directory comes past it, a cell framing a closed segment, which fails the
cell, or a flush appending a commit's record, rather than the guest's next
write, which the daemon refuses before the cell takes any of it; and it is the
host's setup, of a filesystem that has them, which the daemon can't make as it
grants tenants.

## Multi-node only

A cell's durable copy is in a bucket, and hosts hold
copies of it, whose placements fence each other with their tokens: a placement
cut off neither takes a cell back nor deletes it from under a later one
(TASK-39.7, TASK-45). The daemon runs as a host of that system, with the bucket
(`CLADEFSD_BUCKET`) and its placement's token from the control plane
(`CLADEFSD_TOKEN`, not 0), and refuses to start without them: there is no
single-node mode. Tests and development use an in-memory bucket
(`memory:///NAME`), or one in a directory of the host's (`file:///DIR`), which
outlives the daemon and is shared with cladefsctl and cladefs-gc. What a bucket has
acknowledged leaves the cell directory (TASK-165.8): an in-memory bucket is
its process's, and what left for it is gone once that process exits, so a cell
used by more than one process, or across a daemon's restarts, needs a file or
cloud bucket. cladefsctl reaches cells through the host's daemon alone, which owns them, nothing
else opening them (board 14305).

`StatCell`'s `placement_token` is the fencing token of this host's placement of a
cell, whether a VM process has it open or not (board 13879): the token
`PlaceCell` or `CreateCell` placed it at, or, never placed, `CLADEFSD_TOKEN`. A
host's copy is the cell's latest placement while no other host holds one of a
greater token. `taken_token` says the same, but only while the cell uploads.

### Replicated cells' replica directories

A host keeps the logs it holds of other hosts' replicated cells, and its promises
to their takes, in its replica directory (`CLADEFSD_REPLICAS`, doc-21). **Never
restore it from a snapshot or a backup, and never copy it to another host**
(TASK-682). A restored directory keeps its incarnation but goes back to older
promises, so a writer a take fenced can have commits told Replicated again that
no later writer holds. A host whose disk was restored, or whose image was cloned,
deletes its replica directory before its daemon starts: it comes back as a new
member, whose configurations replace it as they replace a lost disk.

A cell more of whose members are lost for good than its f can't be taken: take it
forced (TASK-681), on a host holding its cell by placing that forced
(`cladefsctl place --force DIR TOKEN`, `PlaceCell`'s `force`), on a new host by
taking it forced (`cladefsctl take --force DIR CAPACITY CELL TOKEN`,
`CreateCell`'s `force`). That placement's takes go on with the members that answer
and the bucket, and lose what only the others held; `StatCell`'s `forced_after`
names the last commit the bucket held, past which commits told Replicated may be lost.

### Fast WAL cells (appendable zonal object storage)

When compute hosts have access to an appendable zonal object store (Amazon S3
Express One Zone today; GCS Rapid Storage via gRPC `BidiWriteObject` is planned
and currently rejected at boot), set `CLADEFSD_FAST_WAL_BUCKET` to its URL
(`s3://...`, doc-29). The daemon probes offset-checked append semantics at boot
and refuses to start if the bucket does not honor them. Optional
`CLADEFSD_FAST_WAL_LARGE_EXTENT_BYTES` overrides the 256 KiB large-write
direct-to-cold-bucket threshold. For cross-AZ takeover recovery, set
`CLADEFSD_FAST_WAL_ALLOWED_BUCKETS` to a comma-separated allowlist of trusted
fast WAL bucket URLs; takeover refuses any bucket URL in a cell head outside
`CLADEFSD_FAST_WAL_BUCKET` and that allowlist. Fast WAL cells
(`CreateCellRequest.fast_wal = true`, `FsyncLevel.FSYNC_LEVEL_FAST_WAL`) perform
zero local disk `fsync`s and keep compute nodes 100% stateless cache-only; losing
any node at any time loses zero acknowledged commits, and takeover from another
host fences and recovers the active WAL automatically. Fast WAL cannot be
combined with peer replication (`replicas >= 1` or an active peer configuration;
disable replication with `replicas = 0` and `SyncCell` first).

### Buckets and their numbers

Every bucket has a **bucket number**, which the high bits of every cell ID it gives carry, so that
no two buckets give one ID and a fork's child may later live in another bucket than its parent
(backlog doc-23, TASK-633.9). Numbers are sequential from the **root bucket**, number 1, whose
record of every bucket is the authority; a copy of it is in every bucket's root.

```sh
cladefsctl format-bucket s3://ROOT --region us-east-1 --kms aws-kms://us-east-1   # prints 1
cladefsctl add-bucket s3://ROOT gs://OTHER --region europe-west1                  # prints 2
cladefsctl buckets gs://OTHER      # the record: its version, then each bucket's number and details
```

`format-bucket` makes a bucket never formatted the root of buckets of its own; a bucket never
formatted is made one by the first host to make a cell in it. `add-bucket` takes a bucket never
formatted: add it before any host uses it. It gives the next number by a conditional write of the
root's record, so additions are serialized there, then copies the record to every bucket's root.
Adding a bucket again keeps its number; a bucket with a number already is refused (`EEXIST`), as is
a root that is not number 1 (`EINVAL`). The record holds each bucket's provider, cloud region,
URL, endpoint, KMS and master key's ID, as given with `--region`, `--endpoint`, `--kms` and
`--master-key-id`: never a key or a credential, which each host keeps. The root's cloud region
being down blocks only additions.

A host finds another bucket by its number (TASK-633.3): in the record in its own bucket's root,
or, for a number that copy lacks, in the root bucket's. It opens the bucket at the URL recorded and
refuses it unless the bucket's identity is the one recorded (`ESTALE`); a number no record holds
is `ENOENT`. It asks each of these once: a bucket out of reach fails the request it was for, as
that bucket failing would, and the request goes on without it (a mirror's copies here, TASK-633.8)
or tries again, rather than waiting for it to come back (board 5528). Each bucket is opened with the host's environment, its own variables first:
`CLADEFSD_BUCKET_<N>_<NAME>` stands for `NAME` for bucket N, so that buckets of other clouds,
regions or accounts each get their own credentials:

```sh
AWS_REGION=us-east-1                                  # the host's own bucket, and the default
CLADEFSD_BUCKET_2_AWS_REGION=eu-west-1                # bucket 2's
CLADEFSD_BUCKET_3_GOOGLE_SERVICE_ACCOUNT=/etc/cladefs/gcs-3.json
```

`add-bucket` opens each bucket so too, the root as bucket 1, the one added as the number it is to
get, so give its variables before adding it. If another addition takes that number first, it is
added under the next and refused (`EAGAIN`): give it that number's variables and add it again,
which finishes it. A bucket that refuses a request for good, credentials it does not take or a key
too long, fails it at once (`EACCES`, `EIO`), naming the key, rather than retrying it.

The cells of another bucket are under that bucket's KMS (TASK-633.6): the one its record names
(`--kms`), else the one its variables name (`CLADEFSD_BUCKET_<N>_CLADEFS_KMS`, or `CLADEFS_KMS`),
else, given a master key (`CLADEFSD_BUCKET_<N>_CLADEFS_MASTER_KEY`, or `CLADEFS_MASTER_KEY`), the
bucket KMS over that bucket, its tenants' KEKs kept in it. A bucket given any of `CLADEFS_KMS`,
`CLADEFS_MASTER_KEY` or `CLADEFS_MASTER_KEY_FILE` as its own takes none of the host's three. A
master key's ID recorded with
`--master-key-id` must be among those given, or the bucket's cells are refused (`EACCES`). A KMS
URL recorded in the registry is untrusted (`Origin::Recorded`): it may name no host files
(`keys=FILE`, `ca=FILE`), `local://` only if `CLADEFSD_KMS_LOCAL=1`, OpenBao or Azure Key Vault
hosts only those in `CLADEFSD_KMS_HOSTS` or the host's own `CLADEFS_KMS` (OpenBao over cleartext
only on loopback), GCP Cloud KMS projects (both key ring and full-resource key template) only those
in `CLADEFSD_KMS_GCP_PROJECTS` or the host's own `CLADEFS_KMS`, and AWS KMS key template ARNs only
accounts in `CLADEFSD_KMS_AWS_ACCOUNTS` or the host's own `CLADEFS_KMS` (non-ARN key IDs and
aliases resolve in the host's own account; `endpoint=` only AWS KMS over HTTPS, loopback, or
`CLADEFSD_KMS_HOSTS`). A KMS
refusing a tenant refuses it in the buckets under that KMS alone. The host's own bucket's cells are
under the KMS its own environment names, as below.

## Encryption and shredding

Design doc-16 is the design. Every cell is encrypted, always:
the bucket, the cell directories and the part cache hold its data encrypted,
under its cell's KEK, which its tenant's KEK wraps. Every process that opens
cells (cladefsd) refuses to start
unless `CLADEFS_KMS` names the host's KMS or `CLADEFS_MASTER_KEY` gives a master key
for the bucket KMS (below); a tenant the KMS has no KEK of gets no cell,
refused as destroyed (`EACCES`). The KMS `CLADEFS_KMS` names is `local:///DIR`, a
fake for tests and development, AWS KMS or Google Cloud KMS (below), or
OpenBao's Transit engine:
`openbao://HOST[:PORT]/MOUNT?key=cladefs-tenant-{tenant}` (`openbaos://` over TLS,
`&ca=FILE` for a private CA; `&namespace=NS`; `&keys=FILE` naming tenants' own
keys), each tenant's KEK the Transit key the template names, `{tenant}` its ID
in base 32. Its token is `CLADEFS_KMS_OPENBAO_TOKEN`'s, or in the file
`CLADEFS_KMS_OPENBAO_TOKEN_FILE` names (read again when refused, as OpenBao Agent
renews it), its policy `update` on `MOUNT/encrypt/*` and `MOUNT/decrypt/*` and
`read` on `MOUNT/keys/*`, and not `create`, which would make a key for every
tenant (doc-16, Real KMSes: OpenBao). To shred a **tenant**, destroy its KEK in
the KMS, then tell every host (`cladefsctl forget-tenant-keys TENANT`, the control
API's `ForgetTenantKeys`): absolute, as every cell KEK of the tenant, in every
version of every object, was wrapped by it. To shred a **cell**, run `cladefsctl
shred-cell TENANT/CELL` (`ShredCell`) against every host's daemon: it takes the
cell's wrapped KEKs out of its object `<C>cell` and has each host forget them.
That is only as strong as the bucket keeps no older version of that object, so a
bucket with versioning on needs a lifecycle rule expiring non-current versions:
on S3, `NoncurrentVersionExpiration` (with `NoncurrentDays` as short as the
deployment's recovery needs allow); on GCS, object versioning off, or a
lifecycle rule deleting non-current versions (`isLive: false`, or
`daysSinceNoncurrentTime`). Until that rule has run, the old version's wrapped
KEK unwraps while the tenant's KEK lives. A bucket with versioning off needs
nothing. A host told either halts the cells of the tenant or cell it has open
(`EACCES`) and drops their keys and cached pages; a halted cell's own keys stay
in memory until its VM process closes it.

**The bucket KMS** (doc-16, The bucket KMS), the default. With no `CLADEFS_KMS`, a
host given a master key, `CLADEFS_MASTER_KEY` (64 hex digits: `openssl rand -hex
32`) or `CLADEFS_MASTER_KEY_FILE` (a file of 32 raw bytes, or of the same text),
keeps each tenant's KEK in its bucket, at `tenants/<tenant>/kek`, wrapped by the
master key and made on the tenant's first use: every tenant's cells are encrypted,
with no setup per tenant. Every process that opens cells (cladefsd, cladefsctl,
mounts, and cladefs-gc to rotate it) is given the same key, and the bucket never holds it; logs and
errors name it only by its ID, a short hash. To rotate it, give every host
`CLADEFS_MASTER_KEY=NEW,OLD` (comma- or line-separated, the newest first: it
wraps, any unwraps), run `cladefs-gc URL --rotate-master-key`, which rewraps every
tenant's KEK under NEW and records NEW in the bucket's entry of the registry of
buckets, every copy of it (run it again until it fails no copy: hosts of other
buckets choose its KMS by that entry), then give the hosts NEW alone, those of
other buckets too (`CLADEFSD_BUCKET_<N>_CLADEFS_MASTER_KEY`). To shred a tenant, run
`cladefsctl shred-tenant TENANT` (`ShredTenant`) against every host's daemon: it
replaces the tenant's KEK object by a tombstone, so that nothing of the tenant's
unwraps anywhere and no KEK is made for it again, and has each host forget its
keys (a host not told refuses it within a minute). It shreds in every bucket at
once, each within 30 s: one out of reach is said failed, the others shredded,
and shredding again finishes it. A bucket under a KMS its record names is never
asked. As for a cell, a bucket with
versioning on needs non-current versions of `tenants/<tenant>/kek` expired, by
the rule above. It is not BYOK: a tenant holding its own key uses a real KMS.

**Reloading the KMS** (doc-16, What reloads). `cladefsctl kms reload` (`ReloadKms`),
against every host's daemon, reads its `&keys=FILE` overrides and its
`CLADEFS_MASTER_KEY_FILE` again, no restart: a tenant onboarded with its own key,
or a master key added for a rotation (or dropped after it), takes effect at once.
It applies all or nothing: a line that is no override fails it naming the line
(`EINVAL`); renaming the key of a tenant with cells, or dropping a master key a
KEK object or the registry still holds, fails it (`EBUSY`). The KMS's URL, its
credentials, `CLADEFS_MASTER_KEY` itself and which KMS each bucket is under change
only by a restart.

**AWS KMS** (doc-16, Real KMSes: AWS KMS). `CLADEFS_KMS=aws-kms://REGION?key=TEMPLATE`,
say `aws-kms://us-east-1?key=alias/cladefs-tenant-{tenant}`: each tenant's KEK is a
symmetric KMS key, named by the template with `{tenant}` in base 32 (a key
ID, key ARN, alias name or alias ARN), and `&keys=FILE` names a file of
overrides, lines `TENANT NAME` (the tenant in base 32), for tenants that bring their own key, in
their own account by its key ARN. `&endpoint=URL` names a VPC endpoint (or an
emulator). Credentials are the standard AWS chain's: the environment, the
shared profile (SSO, `credential_process`), web identity, the ECS task's or
the instance's role. cladefs's role needs `kms:Encrypt` and `kms:Decrypt` on each
tenant's key, and nothing else; every call carries the encryption context
`cladefs-tenant` = the tenant in base 32, so a policy can tie each key to its
tenant with the condition `"StringEquals": {"kms:EncryptionContext:cladefs-tenant": "1a"}`.
A tenant bringing a key in another account allows cladefs's role those two
actions under that condition in its key policy, cladefs's IAM policy allows them
on the key's ARN, and the overrides file names it. A tenant revokes cladefs's use
by disabling its key or removing that statement, and shreds itself by
scheduling its key's deletion (every host refuses its cells from then on);
forget its keys on every host and stop serving it before the waiting period
ends, as AWS then forgets the key (TASK-411.19). Rotating a key needs nothing
of cladefs: what its older versions wrapped still unwraps. The tests run against
local-kms, an emulator (`scripts/test-local-kms.sh`, Go to install it).

**Google Cloud KMS** (backlog doc-16, Real KMSes). With
`CLADEFS_KMS=gcp-kms://projects/P/locations/L/keyRings/R?key=cladefs-tenant-{tenant}`,
each tenant's KEK is the key `cladefs-tenant-<tenant in base 32>` in that key ring
(symmetric, purpose `ENCRYPT_DECRYPT`), a wrap its `:encrypt`, bound to the
tenant as associated data. A tenant bringing its own key, in a project of its
own, is named in a file given as `&keys=FILE`, a line `TENANT
projects/P2/locations/L2/keyRings/R2/cryptoKeys/K` each. Each tenant grants
the hosts' service account `roles/cloudkms.cryptoKeyEncrypterDecrypter` on its
key alone; taking the role away, or disabling the key's versions, revokes it;
destroying them shreds the tenant. Hosts authenticate with Application Default
Credentials, as for `gs://` buckets (`GOOGLE_APPLICATION_CREDENTIALS`, or the
metadata server); `CLADEFS_KMS_GCP_TOKEN`, an access token, is for development.
A tenant that rotates its key keeps its old versions enabled: cells made
before stay wrapped under them (TASK-411.21).

**Azure Key Vault** (TASK-633.6). With
`CLADEFS_KMS=azure-kv://myvault.vault.azure.net?key=cladefs-tenant-{tenant}`,
each tenant's KEK is the key `cladefs-tenant-<tenant in base 32>` in that vault
(RSA, wrapped with `RSA-OAEP-256`; `&alg=A256KW` for a Managed HSM's AES keys), a
wrap its `wrapkey` of the tenant's ID and the key, so that no other tenant's
unwrap gives it. A tenant's own key in its own vault is named in `&keys=FILE`, a
line `TENANT https://THEIR_VAULT.vault.azure.net/keys/NAME` each. Every host named
must be a vault's or a Managed HSM's (`*.vault.azure.net`, `*.managedhsm.azure.net`, and
the China and US Government clouds'), as the hosts' token goes to it; `&endpoint=` takes
`https://VAULT_HOST` or, for an emulator, `http://127.0.0.1:PORT`. Each tenant
grants the hosts' identity `wrapKey` and `unwrapKey` on its key alone (`Key
Vault Crypto Service Encryption User`); taking that away, or disabling the key,
revokes it; deleting it shreds the tenant. Hosts authenticate as Azure's
libraries do: workload identity (`AZURE_FEDERATED_TOKEN_FILE`), a service
principal's secret (`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`),
or the managed identity; `CLADEFS_KMS_AZURE_TOKEN`, an access token, is for
development. Its refusals are mapped by Key Vault's error codes, not yet checked
against the service itself (tests run against a fake of the vault).

## Observing the daemon

`CLADEFSD_METRICS=HOST:PORT` serves `/metrics` (Prometheus) and, unless told otherwise, the
z-pages under `/z/`: CPU profiles (`/z/pprof/profile?seconds=N`,
`/z/pprof/flamegraph?seconds=N`) and what the daemon logs (`/z/log`). Whoever reaches the z-pages
may profile the daemon's CPU and change its log; whoever scrapes need not (board 8559):

| `CLADEFSD_ZPAGES` | `/metrics` | `/z/` |
|---|---|---|
| unset | on `CLADEFSD_METRICS` | on `CLADEFSD_METRICS` too |
| `off` | on `CLADEFSD_METRICS` | nowhere |
| `HOST:PORT` | on `CLADEFSD_METRICS`, alone | there, alone |

Bind either to localhost or to a network only operators reach. A profile whose client hangs up
ends then; one profile at a time (`409` while one is taken).

Metrics label a cell `cell="TENANT/CELL"`, in base 32, as cells name it, in every family
(`cladefsd_vm_*`, `cladefsd_vm_build_info`, `cladefsd_upload_*`, `cladefsd_replication_*`), so
that they join on it. Those families are per cell, and a fork is a cell of its own: their series
come and go with the cells opened, so aggregate them (`sum without (cell)`, recording rules)
rather than keep each cell's. `cladefsd_vm_*` tells a cell's writer from its followers by
`role="writer|follower"`, the followers summed; no family is per connection (board 8563). The
`cladefsd_process_*` gauges are sampled at each scrape, right from the daemon's start; the
object storage and part cache families read 0 until a cell opens them (board 8561).

These families are there from the start too, each 0 until it has any (board 8565; every family
is listed in `crates/cladefs/src/cladefsd/metrics.rs`'s module doc):

| Family | What |
|---|---|
| `cladefsd_grpc_server_calls_total`, `cladefsd_grpc_server_errors_total`, `cladefsd_grpc_server_duration_seconds` (histogram) | gRPC calls served, by `service` (`cladefs.cladefsd.v1.Cladefsd`, the control API; `cladefs.mount.v1.Mount`; `cladefs.peer.v1.Peer`) and `method`, each of the .protos' and `unknown`: their count, those failed (a status other than OK, or their stream cut), and how long each took to its answer's end, a stream's whole life. Refusals while the daemon drains count, `UNAVAILABLE`. |
| `cladefsd_grpc_client_calls_total`, `cladefsd_grpc_client_errors_total`, `cladefsd_grpc_client_duration_seconds` | The calls the daemon made of its peers (parts, replication, migrations), alike. |
| `cladefsd_migrations{state}` | Live migrations under way: from this host `preparing` and `moving`, to it `taking`. |
| `cladefsd_migrations_total{outcome}` | Migrations from this host `prepared`, `aborted` (their prepare failed), `accepted`, `refused`, and `given_up` by their targets. |
| `cladefsd_migration_rounds_total`, `cladefsd_migration_sent_bytes_total`, `cladefsd_migration_taken_bytes_total`, `cladefsd_migrations_taken_given_up_total` | Their rounds, bytes sent from this host and taken by it, and migrations to it given up (aborted, or idle `CLADEFSD_MIGRATION_IDLE`). |
| `cladefsd_mount_cells{state}` | The FUSE mount's cells, `attached` and `detaching`. |
| `cladefsd_mount_requests_total`, `cladefsd_mount_request_errors_total`, `cladefsd_mount_attaches_total`, `cladefsd_mount_detaches_total` | Its requests, those answered with an error (but `ENOENT` and `ENOSYS`, a name not there and an operation it has not, which the kernel asks every filesystem), its attaches and detaches. |
| `cladefsd_page_cache_bytes`, `_dirty_bytes`, `_writeback_bytes`, `_stuck_bytes`, `_limit_bytes`, `_dirty_limit_bytes` | The cells' page cache: held, dirty, being written back, dirty of files whose write-back fails, its size, and the dirty bytes past which writers wait. |
| `cladefsd_page_cache_flush_stalls_total`, `cladefsd_page_cache_flush_stalled_seconds_total`, `cladefsd_page_cache_flusher_died` | Writers held back at that bound and how long, together: the disk not keeping up; 1 once its flusher died. |
| `cladefsd_memory_full`, `cladefsd_memory_pressure`, `cladefsd_memory_thrashing`, `cladefsd_memory_full_entered_total`, `cladefsd_memory_refaults_total`, `cladefsd_memory_refaults_per_second`, `cladefsd_memory_floor_bytes`, `cladefsd_memory_target_bytes`, `cladefsd_memory_psi_some_avg10_bps`, `cladefsd_memory_psi_full_avg10_bps`, `cladefsd_memory_kernel_cold_harvested_bytes_total`, `cladefsd_vm_reclaim_bytes_total`, `cladefsd_vm_reclaim_attempts_total`, `cladefsd_vm_reclaim_errors_total` | 1 while the host's memory is full (the daemon makes no cell, `CreateCell` failing `ENOMEM`), 1 while under memory pressure, 1 while actively refault-thrashing; full-entry and part-refault counters; 1-second refault rate; the protected floor and headroom-adjusted target; memory PSI averages in basis points; cold DAX bytes harvested via `cachestat(2)` after MGLRU swap-out; and proactive VM `memory.reclaim` reclaimed bytes, attempts, and errors. All gauges and counters are also returned by gRPC `GetStats`. |
| `cladefsd_otel_spans_exported_total`, `cladefsd_otel_spans_dropped_total`, `cladefsd_otel_spans_failed_total` | Spans exported over OTLP, dropped (the exporter's queue, `OTEL_BSP_MAX_QUEUE_SIZE`, full), and whose export failed (`docs/tracing.md`). |

No label holds what a caller names: a method none of the .protos has is `unknown`.

The log is on standard error (`CLADEFS_LOG_FORMAT`, `CLADEFS_LOG_LEVEL`): text records begin with
their RFC 3339 time and escape control characters; a record logged within a span carries its
`trace_id` and `span_id`. What it logs changes while the daemon runs, which a restart would crash
its cells for (board 8571):

```sh
curl -X POST 'http://ZPAGES/z/log?level=info,bucket_cell::object_bucket=debug'
curl http://ZPAGES/z/log            # what it logs now
curl -X POST http://ZPAGES/z/log    # back to CLADEFS_LOG_LEVEL's
```

SIGQUIT logs every thread's stack, a record a thread, through eu-stack, killed past 30 s. SIGTERM,
SIGINT and SIGHUP each drain the daemon (`CLADEFSD_DRAIN`) and end it: Ctrl-C in its terminal, or
its terminal or ssh session closing, no longer kills it with every cell crashed (board 8567). A
second signal while it drains changes nothing; SIGKILL ends it at once.

## cladefsctl: the control API from a shell

`cladefsctl`, with `CLADEFSD_GRPC` naming the daemon's control socket, calls its control API
(`integrator-guide.md` has it call by call; `cladefsctl`'s header, `crates/cladefs/src/bin/cladefsctl.rs`,
every command). Besides the cells' commands (`create`, `take`, `fork`, `stat`, ...), these are
for operators and scripts, through the daemon alone, each the call of the same name (as its usage
prints them); each prints
what its call answers, one `name value` a line, or one JSON object with `--json` where it takes
it (board 8585). TENANT is in base 32; `--tenant` names one, its absence every tenant, or, for
`shrink-memory`, the daemon's memory whole.

| Command | Call | What |
|---|---|---|
| `stats [--json] [--wait SECONDS] [--leave-out histograms\|guests\|vms]...` | `GetStats` | What the daemon, its VM processes and their guests counted; `--wait` waits for the VM processes' answers SECONDS at most (2 unless given, 60 at most); `--leave-out` leaves out the guests' histograms, their stats, or the VM processes |
| `cells [--json]` | `ListCells` | The cells VM processes have open, each's `path` and what `StatCell` says of it |
| `counters [--json]` | `GetCounters` | The daemon's counters since it started, its bucket's under `object_storage` |
| `set-peers [ADDRESS...]` | `SetPeers` | The hosts sharing its bucket, each `HOST:PORT`; none without any |
| `migrate TENANT/CELL PEER` | `MigrateCell` | Begins the live migration of the ephemeral cell to the daemon at peer address PEER; prints `migration ID` |
| `hand-over TENANT/CELL` | `HandOverCell` | Hands the ephemeral cell over to its migration's target |
| `wait-hand-over [--migration ID] TENANT/CELL` | `WaitHandOver` | Waits on the target daemon for an incoming ephemeral hand-over |
| `abort-migration TENANT/CELL` | `AbortMigration` | Ends the ephemeral cell's migration, the cell free to migrate again |
| `memory-limit [--tenant TENANT] BYTES\|none` | `SetMemoryLimit` | Limits the memory extents' pages are held in, or lifts the limit |
| `memory-weight TENANT WEIGHT` | `SetMemoryWeight` | Weighs a tenant's memory against the others' |
| `in-flight [--tenant TENANT] BYTES\|default` | `SetTenantInFlight` | The bytes in flight a tenant's connections share, or the default |
| `shrink-memory [--tenant TENANT] TARGET` | `ShrinkMemory` | Evicts pages down to TARGET bytes; prints `bytes`, what is held then |
| `disk-limit [--tenant TENANT] BYTES\|none` | `SetDiskLimit` | Limits the disk, or lifts the limit |
| `disk-weight TENANT WEIGHT` | `SetDiskWeight` | Weighs a tenant's disk against the others' |
| `tenant-disk [--tenant TENANT] [--json]` | `GetTenantDisk` | Prints `bytes`, `limit_bytes` (null without one) and `reserved_bytes` |

**Bounding a call.** Any command may be preceded by `--timeout SECONDS`, or run with
`CLADEFSCTL_TIMEOUT=SECONDS`, which bounds connecting to the daemon and its call as one deadline:
past it the command fails with `ETIMEDOUT`, and the daemon goes on with the call, which it may
still complete, as it completes every call whether or not its caller waits. No command has one
unless given: `sync`, `import` and `fork` take minutes, others as long as their cells or VMs take.
It bounds the control API's calls alone, not `mount`'s, nor what `cladefsctl` does without the
daemon.

**Answers are 64 MiB at most** (`control::MAX_MESSAGE_BYTES`, board 8581). A call whose answer
grows with the host refuses one past it with `RESOURCE_EXHAUSTED` (`EMSGSIZE`), saying its size
and what to ask instead: `GetStats` (`stats`) of a host of many VMs, to leave the guests'
histograms, their stats or the VM processes out (`--leave-out`), the daemon's own stats alone at
the least; `ListCells` (`cells`), to `StatCell` each cell; `ListVersions`, to drop versions;
`ListEphemeralHosts`, to decommission hosts gone for good.

## The daemon's doctor: what it checks of the host as it starts

The daemon checks the host's limits and kernel features as it starts, every one in one place
(`cladefsd::doctor`, board 14217), logging what each found. `cladefsd --doctor [MEMORY_BYTES]`
prints the same without starting the daemon (a memory size given, as one given to a daemon, leaves
the memory governor's prerequisites out), and fails if the daemon would refuse to start:

```
ok       page size: 4096 bytes
ok       open files: 1048576 (room for 4941 cells' files at their most)
ok       SO_PEERPIDFD: supported
warning  vm.max_map_count: 65530, under the 262144 a host of many VMs' windows wants: ...
ok       cachestat: cachestat(2) works
...
```

A **warning** is something the daemon runs with, worse:

- **Open files** (`RLIMIT_NOFILE`) under its own 1,024 and room for 256 cells' files at their most
  (212 each, 55,296): it serves fewer connections at once. The systemd unit's `LimitNOFILE`
  (below, Running under systemd) sets it.
- **`vm.max_map_count`** under 262,144: VM processes' windows map fewer runs of pages at once.
- **Memory pressure stall information** (PSI) missing, in governed mode: the governor goes by
  watermarks alone, and reclaims no VM's memory.

A **requirement** the daemon refuses to start without, every one said at once:

- **Linux 6.5 or later.** The daemon knows the process at the other end of a VM socket by the
  socket's pidfd (`SO_PEERPIDFD`, 6.5), and kills that process when it stops acknowledging the
  daemon's changes to its window. On an older kernel it refuses to start: the pid of the process
  that connected, opened after the fact, may be another process's by then, which it would kill.
  `architecture.md` lists the rest of what it needs of the kernel.
- **Pages of 4 KiB to 1 MiB**, which its 1 MiB slabs are made of.
- **In governed mode** (no memory size given): `cachestat(2)`, MGLRU enabled, swap at least the
  governor's floor, cgroup v2 at a writable `/sys/fs/cgroup`, and `CAP_DAC_OVERRIDE` or VM cgroups
  delegated (`CLADEFSD_VM_CGROUP_DELEGATED=1`). `cladefsd --setup-host` provisions the first three.

What the daemon's configuration names it refuses as it starts too, as only failing once it served
before (board 8595):

- **A disk at least**, `[[disks]]` or `CLADEFSD_ROOTS`: a host's one daemon owns the directories
  its cells are kept in, each locked as it starts, refusing a second daemon, and one named twice
  (board 14305).
- **Every `CLADEFSD_ROOTS` directory there**, and a directory: one missing is refused as it
  starts, rather than every cell under it later.
- **A part cache it can open**: every `CLADEFSD_PART_CACHE` shard there, not locked by another
  daemon, and on a filesystem that bypasses the page cache (`O_DIRECT`), whether or not the daemon
  has peers.

## Access to userfaultfd

On Linux, DAX windows take KVM's faults through userfaultfd, which needs
access to `/dev/userfaultfd` (`vm.unprivileged_userfaultfd=1` does not do: only the device is
used). To grant it to the `kvm` group:

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
| 158 | `CladefsdLost` | Storage: its extent daemon went, and the VM can't go on. What it was told stays as doc-12 says. |
| 159 | `AccessRefused` | The guest: an access stayed refused, of a part of its DAX window nothing maps, or a write past its private pages' bound (`private_max`, backlog doc-13). That VM alone stops (TASK-77). |
| Killed by SIGABRT | A panic | A VMM bug: Firecracker panics abort, and an unresolved DAX window fault does. |

## Building a release daemon

A host runs a static `musl` release build of one commit, of the Rust `rust-toolchain.toml` pins
(board 8603). Linux release builds of `cladefsd`, `cladefsctl`, and `cladefs-gc`
are `musl`-only (non-`musl` Linux release builds fail at compile time) so one static binary per
architecture runs across Ubuntu, Debian, Fedora Asahi (16 KiB pages), etc. with zero glibc
version coupling:

```sh
git submodule update --init
scripts/build-musl.sh                  # native $(uname -m)-unknown-linux-musl (or: make build-musl)
scripts/install-musl.sh --restart      # lockstep-installs cladefsd, cladefsctl, cladefs-gc
# or for a specific target / output directory:
scripts/build-musl.sh --target x86_64-unknown-linux-musl --out dist/x86_64
scripts/build-musl.sh --target aarch64-unknown-linux-musl --out dist/aarch64
```

This builds `cladefsd`, `cladefsctl`, and `cladefs-gc` statically against `musl`
(matching how `scripts/test-e2e.sh` builds Firecracker for `musl`), verifies with `readelf` that
each binary has no `PT_INTERP` dynamic linker or `DT_NEEDED` shared libraries (accepting both
static and static-PIE ELFs) and retains `.debug_line`, and uses `mimalloc` as `#[global_allocator]`
on `target_env = "musl"` (hotbed question 533):

- `musl`'s default `mallocng` allocator contends heavily under multi-threaded allocation;
  `mimalloc` uses lock-free per-thread heaps and detects the OS page size at runtime (so one
  `aarch64-unknown-linux-musl` binary runs on both 4 KiB and 16 KiB kernels).
- On `musl`, `cladefsd::memory::return_large_allocations()` caps `mimalloc`'s arena object size
  just below `MMAP_THRESHOLD` (1 MiB) and lowers `purge_delay` to 10 ms so freed 32 MiB segment
  buffers and 1 MiB parts return to the OS immediately (`RssAnon` stays within `AnonBudget`).
- Native `musl` builds need `musl-tools` (`musl-gcc`); cross-compilation uses
  `<arch>-linux-musl-gcc` (or `CC_<target_underscored>`).
- `-p cladefs` and `-p bucket_cell` alone, never across the workspace: other crates'
  dev-dependencies turn on `testing` and `tests-kms`, which a release build refuses to compile
  (TASK-320, TASK-411.28), and never `--features failpoints`, a test build's (`scripts/test-e2e.sh`
  builds one in the `sim` profile).
- `--locked`: Cargo.lock as committed, never re-resolved.
- No `RUSTFLAGS`: it replaces `.cargo/config.toml`'s `-Cforce-frame-pointers=yes`, which perf and
  `/z/pprof` unwind the daemon's stacks by, and the build refuses a release without them
  (`crates/cladefs/build/stamp.rs`, board 8597). A build that needs flags of its own adds that one.
- The release profile (root `Cargo.toml`) keeps line tables, so that `/z/pprof`
  name each frame's file and line, unwinds panics, as a cell's halt needs, and checks arithmetic
  for overflow, as every test's build does. `cladefsd` and `cladefsctl` link with
  those debug sections zlib-compressed (`crates/cladefs/build.rs`, board 10269), which `/z/pprof`
  reads as it is: `cladefsd` 105 MB, 293 MB uncompressed, 61 MB stripped (aarch64, measured
  2026-11-25).

Telling it is right: `cladefsctl --version`, and `cladefsd_build_info{commit}` once it runs, name
the commit it was built of, with no `-dirty` (the stamp follows every crate's files and the lock,
board 8599); `readelf -S` lists `.debug_line`; `ldd` reports `not a dynamic executable` (or
`statically linked`); and it built at all, which it doesn't without `musl`, without frame pointers,
or with a test feature.

## Running under systemd

The daemon expects a supervisor: a panic under a lock shared beyond one cell aborts it, and its
VMs' cells open anew once it runs again. `docs/examples/cladefsd.service` is a reference unit.
What it sets, and why:

- `Restart=always`, 2 s apart. A restart takes every VM on the host with it: they exited 158
  (`CladefsdLost`) as the daemon went, whether it crashed or exited.
- `RuntimeDirectory=cladefsd` for its sockets (VM, control, tenants'), which systemd removes as the
  daemon stops, a crash included: a daemon started over a dead one's sockets refuses them (board
  5227).
- `LimitNOFILE=1048576`: the daemon raises its soft limit to this hard one and serves at most
  three quarters of it over a connection's cell's files at their most (212 by default: its
  segments', its sealed and open extents' raw files, its open and full segments', its flushes',
  its sockets and its VM process's pidfd; board 9975, board 12555, board 12887), 3,709
  connections at 1,048,576, 195 at the floor; under 55,296 its doctor warns (board 14217).
- `TimeoutStopSec=90` above `CLADEFSD_DRAIN=60`: SIGTERM drains (`docs/upgrading-a-host.md`), and
  systemd's SIGKILL at the stop timeout would cut a longer drain short, its cells not uploaded.
- `MemoryLow` set to at least the daemon's shared DAX cache floor (by default `1/8` of the host's
  or cgroup's memory limit, or `CLADEFSD_MEMORY_FLOOR`) so the kernel protects `cladefsd`'s
  extent cache against global reclaim when sibling VM cgroups allocate anonymous memory (because
  Linux cgroup v2 caps effective `memory.low` by every ancestor's `memory.low`, also set
  `MemoryLow` on `system.slice` or place `cladefsd.service` in a dedicated top-level
  `Slice=cladefs.slice` with `MemoryLow` set); and `MEMORY_BYTES` fixed (64 GiB in the example)
  with `MemoryMax` above it by the daemon's own heap (`cladefsd_process_anon_budget_bytes`), or
  governed without `MEMORY_BYTES` (the governor sizes both headroom and floor to `memory.max` when
  set).
- `OOMScoreAdjust=-900`: the kernel's OOM killer takes VMs before the daemon, whose death takes
  them all.
- `CAP_CHOWN`, `CAP_FOWNER`, `CAP_KILL` and `CAP_DAC_OVERRIDE`, and no other capability: jails run
  as other users than the daemon's, a grant gives each tenant's socket to its jails' user (chown,
  then its mode and link), the daemon serves only a VM process it may kill (`CAP_KILL`), and the
  memory governor writes proactive `memory.reclaim` to low-PSI connected VM cgroups discovered from
  their `SO_PEERPIDFD` (`CAP_DAC_OVERRIDE` with `/sys/fs/cgroup` writable; hosts that delegate VM
  cgroups to `User=cladefsd` may drop `CAP_DAC_OVERRIDE` and set `CLADEFSD_VM_CGROUP_DELEGATED=1`
  instead). Governed mode requires Linux 6.5+ `cachestat(2)`, MGLRU enabled, `SwapTotal >= floor`,
  and writable `/sys/fs/cgroup`, failing hard at startup if any prerequisite is absent;
  `ExecStartPre=+/usr/local/bin/cladefsd --setup-host` provisions MGLRU, `min_ttl_ms=1000`, and
  zstd zram swap automatically before dropping privileges. Enable Firecracker virtio-balloon
  `free_page_reporting` so guests return freed memory to the host.
- The `kvm` group, for `/dev/userfaultfd` by the udev rule above; the master key as a systemd
  credential (`LoadCredential`, `CLADEFS_MASTER_KEY_FILE=%d/master-key`), the bucket and token in
  an environment file only root reads.
- systemd's sandboxing (`NoNewPrivileges`, `ProtectSystem=strict` with its state and runtime
  directories writable, `ProtectHome`, `PrivateTmp`, `RestrictAddressFamilies`, ...), and no
  system call filter or AppArmor profile: one missing a call the daemon makes on a rare path kills
  it, and every VM with it (hotbed question 510). A host whose daemon mounts FUSE
  (`CLADEFSD_MOUNT`) as a user other than root drops `NoNewPrivileges`, which takes away
  fusermount3's setuid.

## Alerts

`docs/examples/cladefsd-alerts.yml` holds Prometheus alerting rules for the daemons, scraped as
the job `cladefsd`. `scripts/check-alerts.sh` checks them with promtool (CI's `ops-examples` job),
and `scripts/tests/test_ops_examples.py` that every metric they name is one the daemon serves.

| Alert | Severity | When |
|---|---|---|
| `CladefsdDown` | critical | Its scrape fails for a minute: the host's VMs have exited 158. |
| `CladefsdRestarted` | warning | `cladefsd_process_cpu_seconds_total` started again: it restarted, its VMs with it. |
| `CladefsdCommitsStalled` | critical | A cell's commits waited at the upload lag bound (`cladefsd_upload_stalls_total`): its guest's syncs stall behind the bucket. |
| `CladefsdUploadsBehind` | warning | A cell's oldest commit not uploaded for a minute, for 5 minutes. |
| `CladefsdBucketFailing` | warning | Requests to the bucket failing (`cladefsd_object_storage_failures_total`) for 10 minutes. |
| `CladefsdMemoryExhausted` | critical | Arenas whose relief found nothing to free (`cladefsd_memory_exhausted_arenas`): reads fail `ENOMEM`. |
| `CladefsdMemoryLeaked` | warning | `cladefsd_memory_leaked_bytes` above 0: a bug. |
| `CladefsdMemoryThrashing` | warning | `cladefsd_memory_thrashing == 1` for 5 minutes: DAX extent cache refault-thrashing under memory pressure. |
| `CladefsdVmReclaimFailing` | warning | `cladefsd_vm_reclaim_errors_total` rising for 10 minutes: proactive VM `memory.reclaim` denied or failing. |
| `CladefsdHeapOverBudget` | warning | The daemon's anonymous memory past its budget (`cladefsd_process_anon_over_budget_total`), which a `MemoryMax` sized by it meets with the OOM killer. |
| `CladefsdVmProcessesWedged` | warning | VM processes it killed that have not gone, for 15 minutes. |
| `CladefsdDiskPressure` | warning | A disk under pressure for 10 minutes: uploads urged, tenants held to their shares. |
| `CladefsdDiskFull` | critical | A disk's free space under its headroom for 5 minutes: another writer (`cladefsd_host_disk_other_bytes`), and guests' writes refused `ENOSPC` soon. |
| `CladefsdReplicationDegraded` | warning | A replicated cell degraded for 5 minutes. |
| `CladefsdUnqualifiedBuild` | warning | `cladefsd_build_info` names a `-dirty` or `unknown` build. |
| `CladefsdBuildsDiffer` | warning | The hosts run more than one build for a day: an upgrade left part way. |

The VM manager alerts on its VMs' exit statuses (above): 158 on many VMs of a host at once is the
daemon's going; 159 is one guest's.
