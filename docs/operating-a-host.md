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

The cells come first. The part cache (`CLADEFSD_PART_CACHE`), copies of the
bucket's objects that any tenant's cells read, which is no tenant's, keeps what
the cells leave of the allowance, `CLADEFSD_PART_CACHE_BYTES` at most if set,
and gives it back at once as they need it, least recently read first. Their
cells near the allowance, within the headroom of it, the disk is under
pressure: their uploaders are urged, closing and uploading their open segments
at once, and each tenant's writes are refused past its share, by its weight
(`SetDiskWeight`, 1 each by default), of what the allowance leaves the tenants.
Past the allowance, writes are refused with `ENOSPC`, which the guest sees as a
full disk; the daemon and every cell go on.

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
cloud bucket. cladefsctl, without the daemon, changes existing cells on the same terms, and makes new cell directories (`take`, `import`)
anywhere.

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
refusing a tenant refuses it in the buckets under that KMS alone. The host's own bucket's cells are
under the KMS its own environment names, as below.

## Encryption and shredding

Design doc-16 is the design. Every cell is encrypted, always:
the bucket, the cell directories and the part cache hold its data encrypted,
under its cell's KEK, which its tenant's KEK wraps. Every process that opens
cells (cladefsd, cladefsctl, cladefs-mount, cladefs-gc to collect) refuses to start
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
mounts, cladefs-gc) is given the same key, and the bucket never holds it; logs and
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
| 158 | `CladefsdLost` | Storage: its extent daemon went, and the VM can't go on. What it was told stays as doc-12 says. |
| 159 | `AccessRefused` | The guest: an access stayed refused, of a part of its DAX window nothing maps, or a write past its private pages' bound (`private_max`, backlog doc-13). That VM alone stops (TASK-77). |
| Killed by SIGABRT | A panic | A VMM bug: Firecracker panics abort, and an unresolved DAX window fault does. |
