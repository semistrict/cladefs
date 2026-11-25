# JBOD across all local SSD usage in cladefs

Research (2026-11-24). The goal: support JBOD (Just a Bunch of Disks — multiple independent local
SSDs / filesystems without RAID-0) across **every** local SSD consumer in `cladefs`, then deploy
it on `ubu1` so `cladefsd` uses both of its NVMe drives instead of only one.

Primary sources inspected directly:
- **cladefs** (this checkout): `crates/bucket_cell/src/cache.rs`, `crates/bucket_cell/src/host_disk.rs`,
  `crates/local_cell/src/cell.rs`, `crates/local_cell/src/pages.rs`, `crates/local_cell/src/footprint.rs`,
  `crates/local_cell/src/replica.rs`, `crates/cladefs/src/ephemeral_host.rs`,
  `crates/cladefs/src/host_cell.rs`, `crates/cladefs/src/cladefsd/{boot,cells,control,disk,grants,mount,migration,peers,reads,server,storage}.rs`,
  `crates/cladefs/proto/cladefs/cladefsd/v1/cladefsd.proto`, `docs/design/doc-22`.
- **JuiceFS** (`~/src/juicefs` @ `a84508bf`, plus commit `249ea179` / PR #4543):
  `pkg/chunk/disk_cache.go`, `pkg/chunk/disk_cache_state.go`, `pkg/chunk/cached_store.go`,
  `pkg/utils/utils.go`, `docs/en/guide/cache.md`.
- **RustFS** (`~/src/rustfs`): `crates/ecstore/src/disk/health_state.rs`, `io_admission.rs`,
  `local.rs`, `crates/object-data-cache/src/config.rs`.
- **AWS mountpoint-s3** (`~/src/mountpoint-s3`): `mountpoint-s3-fs/src/data_cache/disk_data_cache.rs`.
- **DeepSeek 3FS** (`~/src/3FS`): `src/storage/store/StorageTargets.cc`, `src/storage/chunk_engine/src/`.
- **Meta CacheLib / Navy** (`~/src/CacheLib`): `cachelib/navy/common/Device.cpp`.
- **hotbed** (`~/src/hotbed`): `internal/cladefs/client.go`, `internal/worker/vm_backend.go`.
- **External Rust projects** (official docs / docs.rs): **Foyer** (`foyer-rs/foyer`), **Garage**
  (`Deuxfleurs/garage`), **Pingora** (`cloudflare/pingora`).
- **ubu1** (live inspection, 2026-11-24): `lsblk`, `df`, `zpool status`, `zfs list`, `zfs version`,
  `systemctl cat cladefsd.service`, `/var/lib/hotbed/worker/hotbed.json`, `~/docs/system-setup.d/{143,154,155}-*.sh`.

---

## Summary

1. **No drop-in Rust crate fits `cladefs` directly.** Every high-quality Rust storage/cache project
   either is single-directory (`mountpoint-s3`), in-memory only (`pingora-cache`, `rustfs/object-data-cache`),
   uses a slab/log engine that breaks `cladefs`'s zero-copy `rename(2)` adoption (`foyer`), is AGPLv3
   (`garage`), or is a giant MinIO port (`rustfs/ecstore/local.rs`, 1.3 MB). Meanwhile, `cladefs`
   already owns the hardest primitives natively at higher quality: per-`st_dev` `HostDisk` accounting
   and governing, rendezvous hashing (`bucket::peers::owner_of`), `host_fs` deterministic simulation
   (`madsim`), and budgeted `O_DIRECT` I/O. The right move is a ~400–600 LOC native JBOD layer
   combining the best patterns from JuiceFS, RustFS `health_state.rs`, Garage, and Kafka JBOD.
2. **The load-bearing constraint in `cladefs` is zero-copy adoption (`dir.give` → `cache.adopt`).**
   `local_cell/src/cell.rs:1202-1209` refuses to open a cell unless its filesystem (`st_dev`) matches
   the part cache's filesystem, because acknowledged WAL segments move into the part cache via
   `rename(2)` (no `EXDEV`, no silent copy fallback; TASK-350 / TASK-359). Under JBOD, **every host
   disk holding cell directories must also have a part-cache shard**, adoption must target the shard
   on the cell directory's own `HostDisk`, and part-cache lookups must find keys on whichever shard
   holds them (checked in RAM across `N` shard indices at zero syscall cost).
3. **Every local SSD consumer in `cladefs` needs JBOD, not just the part cache.** There are 7
   distinct local-disk surfaces: (1) `CLADEFSD_PART_CACHE` (+ `Cache::versions()`), (2) cell
   directories (`CLADEFSD_ROOTS`, `Grant`, `CreateCell`/`ForkCell`, and upstream `hotbed`'s
   `cellRoot`), (3) `local_cell::pages` (currently one single flusher thread across all disks),
   (4) `CLADEFSD_REPLICAS` (with a subtle incarnation-safety constraint), (5) `CLADEFSD_EPHEMERAL`
   and `CLADEFSD_EPHEMERAL_CELLS`, (6) `CLADEFSD_MOUNT_STORES`, and (7) the disk governor
   (`cladefsd/disk.rs`, already per-`HostDisk`!).
4. **Zero frozen on-disk format changes are required.** Every JBOD shard uses the exact existing
   frozen formats (replica directory v4, ephemeral host v2, cell directory current); the part cache
   is unfrozen/rebuildable (doc-28).
5. **Why `ubu1` currently uses only one disk:** `cladefsd.service` places all 5 directories
   (`part-cache`, `replicas`, `ephemeral`, `ephemeral-cells`, `cells`) under `/mnt/fxdax-scratch`,
   which is a 768 GiB ext4 zvol on `rpool` (`nvme0n1`, KIOXIA 1.8 TB — also holding `/` and `/home`).
   `nvme1n1` (Samsung SSD 990 PRO 2 TB, ZFS pool `hotbed`, 1.68 TB free) is completely unused by
   `cladefsd` and `hotbed`'s Firecracker VMs.

---

## 1. How JuiceFS supports JBOD (`~/src/juicefs` @ `a84508bf`)

JuiceFS implements multi-disk local SSD caching in `pkg/chunk/disk_cache.go` and
`pkg/chunk/disk_cache_state.go` (introduced in `7d149466`, upgraded to consistent hashing and bad
disk detection in `249ea179` / PR #4543).

### 1.1 Configuration & expansion
- `--cache-dir` accepts multiple paths separated by `:` (or `,` via `utils.SplitDir`,
  `pkg/utils/utils.go:44-50`), plus shell globs expanded by `expandDir` (`disk_cache.go:1096-1121`,
  e.g. `/mnt/nvme*/jfs-cache`).
- `Config.SelfCheck` (`cached_store.go:589-594`) appends the filesystem UUID to each path so
  multiple mounts sharing the same JBOD roots don't collide.
- Paths are sorted (`sort.Strings(dirs)`), and `CacheSize`, `CacheItems`, and `pendingPages` are
  divided **equally** by `len(dirs)` (`disk_cache.go:1161-1172`).

### 1.2 Shared-nothing per-disk `cacheStore` + consistent hashing
- `cacheManager` (`disk_cache.go:1070-1344`) owns one independent `*cacheStore` per directory. Each
  shard has its own mutex, `pending` write channel, key index (`KeyIndex`, LRU or 2-random),
  background flusher goroutine, free-space monitor (`checkFreeSpace`, 1 s), expiration cleaner,
  and startup scanner (`fastwalk`).
- **Persistent disk identity (`.lock` UUID):** On open, each shard opens `<dir>/.lock` and reads
  (or generates and writes) a random UUID (`cache.id`, `disk_cache.go:195-225`). The consistent-hash
  ring (`davies/groupcache/consistenthash` with **100 virtual nodes** and `murmur3.Sum32`,
  `disk_cache.go:1165`) hashes `store.id` rather than path or slice index, so reordering paths or
  removing a failed disk keeps the remaining shards' key assignments stable (only `1/N` keys move).
- **Same-disk writeback staging:** When `--writeback` is on, `cacheStore.stage()` writes to
  `<dir>/rawstaging/<key>` and hard-links (`os.Link`, `disk_cache.go:801`) into `<dir>/raw/<key>` on
  the *same* shard, avoiding cross-filesystem `EXDEV`.
- **Legacy fallback on read:** `cacheManager.load()` (`disk_cache.go:1292-1305`) checks the
  consistent-hash shard first, and on `errNotCached` checks `getStoreLegacy` (`fnv32 % N`) so blocks
  written by older versions aren't immediately cold.

### 1.3 Bad-disk detection and isolation (`disk_cache_state.go`)
- Every disk I/O passes through `cacheStore.checkErr` (`disk_cache.go:253-281`), which records the
  start timestamp in `opTs` (for a 1 s watchdog `checkTimeout` that flags ops exceeding `maxIODur`
  = 30 s as I/O errors) and inspects `syscall.EIO` / `ErrFuncTimeout`.
- State machine (`disk_cache_state.go:48-284`), active when `--writeback` is off (with `--writeback`,
  state stays `dcUnchanged` because dropping a disk holding un-uploaded writes would lose data):
  - `dcNormal` → `dcUnstable` after **3 I/O errors** within a 1-minute reset window.
  - `dcUnstable`: caps concurrent ops to 10 (`maxConcurrencyForUnstable`) and runs an active probe
    goroutine every 500 ms (`doProbe`: write, read, remove a 3-byte file under `probe/`).
    - If `>= 60` ops succeed with `0%` error rate in a 1-minute tick → back to `dcNormal`.
    - If still unstable after **30 minutes** (`maxDurToDown`) → `dcDown`.
  - `dcDown`: `cacheManager.cleanup()` (1 s loop) or `getStore()` removes `store.id` from the
    consistent-hash ring (`removeStore`, `disk_cache.go:1214-1229`). Remaining disks serve the
    cache; misses fall back to object storage.
- **Unmounted-disk / root-volume guard (`checkLockFile`, `disk_cache.go:227-239`):** Every 10 s,
  stats `.lock`. If it vanished (e.g., an NVMe unmounted and writes started hitting the underlying
  root filesystem mountpoint), it checks `inRootVolume(cache.dir)` and raises `freeRatio` to 20% to
  protect `/`.

### 1.4 Where JuiceFS does NOT fit `cladefs`
1. **Equal capacity splitting wastes heterogeneous disks.** JuiceFS's own docs (`docs/en/guide/cache.md:287-289`)
   warn that `/data1` (1 GiB) + `/data2` (2 GiB) with `--cache-size 3GiB` caps `/data2` at 1.5 GiB
   instead of 1.8 GiB. `ubu1` itself is heterogeneous (755 GiB scratch zvol on `nvme0n1` + 1.7 TiB
   free on `nvme1n1`).
2. **JuiceFS only stores stateless cache blocks (plus optional staging).** `cladefs` has stateful,
   durable cell directories, ephemeral cells, replica logs/promises, and cross-subsystem `rename(2)`
   adoption.
3. **JuiceFS relies on the OS page cache** (with optional `posix_fadvise(DONTNEED)` via `dropOSCache`),
   whereas `cladefs` strictly bypasses it (`O_DIRECT` + `local_cell::pages`, `AGENTS.md`).
4. **Silent fallback on startup:** JuiceFS silently falls back to a 100 MiB memory cache when no
   `--cache-dir` exists (`disk_cache.go:1156-1160`), violating `cladefs`'s fail-hard startup rule.

---

## 2. Candidate Rust (and reference) projects to copy or learn from

| Project | Lang / License | JBOD / Multi-disk Mechanism | Code Quality | Copy code or study design? |
|---|---|---|---|---|
| **Garage** (`Deuxfleurs/garage`) | Rust / **AGPLv3** | `data_dir` array with per-disk `capacity`; 1024 hash slices assigned proportionally to capacity (`src/block/manager.rs`); **secondary slice locations** let reads find old blocks after adding/reweighting disks without a rebalance storm. | **Excellent** (small, idiomatic, clean Rust). | **Design only (AGPLv3 copyleft).** Capacity-proportional placement + multi-location read fallback are directly applicable. |
| **Foyer** (`foyer-rs/foyer`) | Rust / Apache-2.0 | `foyer-storage` has modular `Device` / `DeviceBuilder` traits (`FsDeviceBuilder`, `FileDeviceBuilder`, `PartialDeviceBuilder`, `CombinedDeviceBuilder`) + `io_uring`/direct I/O. | **Excellent** (modern Rust, used by RisingWave, Chroma, SlateDB). | **Do NOT drop in as part cache.** Foyer is a region/slab store; it cannot do zero-copy `rename(2)` adoption of framed sparse segment files from `local_cell`. Good reference for clean `Device` trait & per-device stats. |
| **RustFS** (`~/src/rustfs`) | Rust / Apache-2.0 | `crates/ecstore/src/disk/`: MinIO-derived multi-disk erasure set. `health_state.rs` (253 LOC) has a clean 4-state `RuntimeDriveHealthState` (`Online`/`Suspect`/`Offline`/`Returning`) + recovery classification + metrics. `io_admission.rs` gives foreground-priority I/O admission per local disk. `object-data-cache` is **in-memory Moka only** (not disk JBOD). | **Mixed:** `local.rs` is a 1.3 MB monolithic MinIO Go port; `health_state.rs` and `io_admission.rs` are small and clean. | **Borrow `health_state.rs` state machine** (cleaner than JuiceFS's `dcState`) and per-disk foreground/background I/O separation concept. Do not copy `local.rs`. |
| **mountpoint-s3** (`~/src/mountpoint-s3`) | Rust / Apache-2.0 | `DiskDataCache` takes a **single** `cache_directory: PathBuf`. No JBOD. | **Excellent** Rust. | Nothing for JBOD. |
| **Pingora** (`cloudflare/pingora`) | Rust / Apache-2.0 | `pingora-cache` is in-memory (`TinyUfo`) + abstract `Storage` trait; **no built-in disk/JBOD backend** in open source. | **Excellent** Rust. | Nothing for JBOD. |
| **DeepSeek 3FS** (`~/src/3FS`) | C++ + Rust (`chunk_engine`) / MIT | `StorageTargets` (`StorageTargets.cc`) spawns **one independent Rust `chunk_engine` per NVMe mount path**, probes `st_dev` + hardware info + writability at init, zero cross-disk locking. | **High** (systems-grade). | Reinforces strict shared-nothing per-`HostDisk` architecture. |
| **CacheLib / Navy** (`~/src/CacheLib`) | C++ / Apache-2.0 | `FileDevice` (`Device.cpp`) uses software **RAID-0 striping** (`fvec` + `stripeSize`). | High, but **wrong model**. | **Anti-pattern for JBOD:** 1 dead disk kills the whole cache, heterogeneous drives truncate to `N * min`, breaks same-FS adoption. |
| **Kafka JBOD** (KIP-112 / KIP-858) & **ClickHouse** | Java / C++ (Apache-2.0) | Stateful directory placement by least-used / most-free online disk; per-disk offline isolation on `EIO` without crashing the daemon. | Industry-standard patterns. | Best model for **stateful** directories (`cladefs` cell dirs, FUSE mount cells, migrated cells). |

### Verdict on copying an existing Rust project
No existing Rust crate can be dropped into `cladefs` because `cladefs` enforces five coupled
invariants simultaneously:
1. Single memory budget (`O_DIRECT` + `local_cell::pages`, zero OS page cache).
2. Zero-copy `rename(2)` adoption from a cell directory into the part cache (`dir.give` → `cache.adopt`), requiring sparse files with `FALLOC_FL_PUNCH_HOLE`.
3. Per-`st_dev` `HostDisk` disk governor (derived cache yields to 0 before any guest write gets `ENOSPC`).
4. Full-stack deterministic simulation under `madsim` via `host_fs`.
5. Frozen on-disk formats (doc-28).

Because `cladefs` **already has** `HostDisk` per `st_dev` (`crates/bucket_cell/src/host_disk.rs`)
and rendezvous hashing (`crates/cladefs/src/bucket/peers.rs::owner_of`), building a clean native
JBOD layer (~400–600 LOC across `bucket_cell`, `local_cell`, and `cladefs`) is smaller, cleaner,
and better tested under `madsim` than adapting any external library.

---

## 3. Complete audit of local SSD usage in `cladefs`

Every place `cladefsd` touches local SSDs, what currently assumes a single disk, and how JBOD
should work there:

### 3.1 Part cache (`CLADEFSD_PART_CACHE`) & cell-to-cache adoption (CRITICAL COUPLING)
- **Current code:**
  - `crates/cladefs/src/cladefsd/storage.rs:95-151`: parses a single path `CLADEFSD_PART_CACHE` and
    single byte budget `CLADEFSD_PART_CACHE_BYTES`.
  - `crates/bucket_cell/src/cache.rs`: `Cache` holds one `Files`, one `Mutex<Index>`, one
    `Arc<HostDisk>`, one `versions: PathBuf`, one `_lock: Flock`.
  - `crates/local_cell/src/cell.rs:1202-1209`: `Recovering::open` enforces
    `cache.disk().dev() == Some(here)` so `leave()` (`cell.rs:1564-1580`) can adopt acknowledged
    segments via `rename(2)` without `EXDEV`.
  - `crates/cladefs/src/cladefsd/disk.rs:417-425`: `govern_disks` only calls `cache.set_limit` on
    the single `HostDisk` matching `Arc::ptr_eq(cache.disk(), disk)`.
- **JBOD design:**
  - `CLADEFSD_PART_CACHE` accepts colon-separated paths (`std::env::split_paths`, matching
    `CLADEFSD_ROOTS`) and optional globs (like JuiceFS `expandDir`). Refuse duplicate `st_dev`s at
    boot (two cache dirs on one filesystem would double-count and contend).
  - Split `Cache` into `Cache { shards: Vec<Shard>, ... }` where each `Shard` is today's single-disk
    cache (own `Files`, own `Mutex<Index>`, own `Arc<HostDisk>`, own `versions/` dir, own `Flock`,
    own persistent `.disk-id` UUID for stable hashing, own `DiskHealth` state). A 1-dir config runs
    1 shard with zero overhead.
  - **Placement & lookup (solving the `EXDEV` adoption vs. hash routing tension):**
    - **Adoption (`cache.adopt(key, from, from_dev)`):** routes directly to the shard whose
      `disk.dev() == Some(from_dev)` so `rename(2)` is **always** same-filesystem, O(1), zero-copy.
      If another shard already cached `key`, forget it there.
    - **Bucket fetches (`keep_tail` / `keep_part`):** routes to the healthy shard chosen by
      **weighted rendezvous hashing** (`owner_of` weighted by each shard's `HostDisk` capacity or
      allowance, so heterogeneous disks like 755G + 1.7T fill proportionally — fixing JuiceFS's equal-split
      bug!). If the primary shard is full or `Suspect`/`Offline`, fall through to the next-ranked
      healthy shard.
    - **Lookups (`tail`, `part`, `holds`, `forget`):** check the primary shard's in-memory index
      first, then check the remaining `N - 1` shards' in-memory indices (for `N = 2..16` NVMe drives,
      a few RAM hash lookups = nanoseconds, zero syscalls!). This simultaneously handles:
      1. Segments adopted onto the cell's local disk rather than the hash-primary disk.
      2. Warm cache entries surviving across restarts when disks are added or removed (like Garage's
         secondary locations, for free because `Cache::open` walks and indexes every shard at boot).
  - **`LocalCell::open` check (`cell.rs:1202`):** changes from `cache.disk().dev() == Some(here)` to
    `cache.has_disk(here)` — every cell directory must live on a `HostDisk` that has a part-cache
    shard, preserving TASK-359's fail-hard guarantee against `EXDEV`.
  - **`Cache::versions()` (`host_cell.rs:718`):** picks a healthy shard's `versions/` directory
    (e.g. least-used or round-robin), since every shard's disk satisfies `cache.has_disk`.
  - **Governor (`disk.rs:417`):** `govern_disks` already iterates every `HostDisk`; call
    `cache.set_limit_on(disk, limit)` on whichever shard lives on `disk`. When
    `CLADEFSD_PART_CACHE_BYTES` is set, apportion it across shards proportionally to each shard's
    `sample.size` (instead of JuiceFS's `total / N`).
  - **Runtime bad-disk isolation (part cache only — derived data):** 4-state health (`Online` /
    `Suspect` / `Offline` / `Returning`, from RustFS `health_state.rs` + JuiceFS probe/timeout).
    Startup failures still fail hard (preserving `tests/cladefsctl.rs` board 8595). At runtime,
    repeated `EIO` or stalled I/O isolates only the failing shard while surviving shards continue
    serving; expose `cladefsd_part_cache_shard_state{device=...}` and alert on degraded shards.
  - **Unfrozen format:** Part cache is explicitly unfrozen/rebuildable (doc-28).

### 3.2 Cell directories (`CLADEFSD_ROOTS`, `Grant`, `CreateCell`/`ForkCell`, and `hotbed`)
- **Current code:**
  - `CLADEFSD_ROOTS` (`server.rs:1167-1189`) **already** accepts colon-separated roots via
    `std::env::split_paths`.
  - `HostDisk`, `Footprint`, and `govern_disks` **already** govern multiple `HostDisk`s independently.
  - **Gaps:**
    1. **Granted tenants (`grants.rs:47-90`, `server.rs:206-212`, `cladefsd.proto` `GrantRequest`):**
       A granted tenant can only be granted **one** `dir` (`CellDirs::Dir(PathBuf)`); granting
       another directory returns `EEXIST`. `Allotment` (`footprint.rs`) holds a single
       `disk: Arc<HostDisk>`.
       - *Fix:* Add `repeated string dirs` to `GrantRequest` (additive v1 proto change, compatible
         with existing `dir`), change `CellDirs::Dir` to `Dirs(Vec<PathBuf>)`, and give each granted
         tenant one `Allotment` per directory/disk (all sharing the tenant's aggregate limit in
         `disk.rs`, while each disk's `share()` enforces per-disk pressure independently!).
    2. **Who picks which JBOD disk a new cell goes on?**
       `CreateCell`, `ForkCell`, `ImportImage`, `ImportMemory` take an explicit absolute `path` from
       the caller. On `ubu1`, `hotbed` (`~/src/hotbed/internal/cladefs/client.go:36`,
       `/var/lib/hotbed/worker/hotbed.json`) only configures a single `cellRoot` string
       (`/mnt/fxdax-scratch/cladefsd/cells/hotbed`), so 100% of VMs land on disk 0.
       - *Two complementary layers:*
         - **In `cladefsd`:** Add an optional pool/root-selection API (e.g., if a caller passes a
           **relative** cell path or `SelectDisk` / `GetDisks` / `CLADEFSD_ROOTS` auto-placement in
           `CreateCell`/`ForkCell`/`ImportImage`/`ImportMemory` returning the chosen `path` in the
           response, OR a small `PickCellRoot` / disk-spare telemetry in `GetStats`), picking the
           healthy root whose `HostDisk` has the most `spare` space (Kafka KIP-112 / ClickHouse
           `least_used` policy, skipping disks under pressure or offline!).
         - **In `hotbed`:** Support `cellRoots: [...]` (or use `cladefsd`'s disk-aware placement)
           and record each workspace's chosen `HostPath` (which `CellRef.HostPath` in `hotbed`
           *already* tracks!) so forks/VMs spread evenly across all JBOD SSDs on `ubu1`!

### 3.3 Cell page cache flusher (`crates/local_cell/src/pages.rs`)
- **Current code:** `PageCache::new` (`pages.rs:204-207`) spawns **one** `"store flusher"` thread
  for the entire process, writing back dirty pages across all files and all `HostDisk`s sequentially.
- **JBOD problem:** On multi-NVMe JBOD, a single synchronous `pwrite` flusher serializes write-back
  across drives and lets one slow/stalled disk block write-back to all other healthy disks.
- **Fix:** Spawn **one flusher thread per `HostDisk`** (lazily on first dirty file for that
  `HostDisk`, mirroring Linux's per-BDI `bdi_writeback` threads; `madsim` remains single-threaded as
  today). Per-disk dirty counts (`HostDisk::dirty`, board 5995) and per-disk stuck errors (board
  8553) are *already* tracked per `HostDisk`!

### 3.4 Replica directories (`CLADEFSD_REPLICAS`, `local_cell/src/replica.rs`, `cladefsd/peers.rs`)
- **Current code:** Single directory `CLADEFSD_REPLICAS` → single `Replicas<HostDir>`, single
  `incarnation` file, flat `<tenant>.<cell>.*` files. Format v4 is **frozen** (`formats/replica-directory/`).
- **Safety invariant (doc-21, `operating-a-host.md:125`):** A member's `incarnation` certifies that
  none of its synced promises were lost. If a JBOD disk holding promises for some cells dies and is
  replaced blank, the host **must not** keep advertising the old incarnation.
- **JBOD design (ZERO format change, ZERO `peer.proto` change):**
  - `CLADEFSD_REPLICAS` accepts colon-separated directories (`dir0:dir1:...`), one per JBOD disk.
  - Each shard directory is a completely standard frozen-v4 `Replicas<HostDir>` with its own random
    `incarnation` file on disk.
  - The host's advertised member incarnation is derived deterministically from the sorted set of all
    shards' incarnations (e.g., `BLAKE3` / `SipHash` truncated to non-zero `u64` over
    `[inc_0, inc_1, ..., inc_{k-1}]`; for `k == 1`, unchanged `inc_0` for 100% backward compatibility!).
    - If ANY replica disk is wiped/replaced (or a disk is added/removed), its `inc_i` changes →
      the derived host incarnation changes automatically → writers safely treat this host as a new
      member and replace old configurations (doc-21), never violating fence safety!
  - Cell routing: map `CellId` to a shard via rendezvous hash of `CellId` against each shard's
    `inc_i` (or check all shards on open so every existing cell stays findable). All cells' replica
    `Append` / `Fence` / `ReadLog` / `DropLogs` I/O spreads across all JBOD disks, each metered by
    its own `HostDisk` footprint.
  - Runtime disk failure: if a shard hits `EIO`, fail calls for cells on that shard (writers replace
    the member after `CLADEFSD_REPLACE_AFTER = 30s`) rather than silently dropping promises.

### 3.5 Ephemeral cells & ephemeral host (`CLADEFSD_EPHEMERAL`, `CLADEFSD_EPHEMERAL_CELLS`)
- **Current code:**
  - `CLADEFSD_EPHEMERAL` (`ephemeral_host.rs`): single directory holding `incarnation` + small JSON
    entries (`cells/<T>.<C>`, `accepted/<M>`), **frozen format v2** (`formats/ephemeral-host/`).
  - `CLADEFSD_EPHEMERAL_CELLS` (`migration.rs:1954-2002`): single directory where migrated ephemeral
    cells are placed (and fallback for bucket-backed migrations).
- **JBOD design (ZERO format change):**
  - `CLADEFSD_EPHEMERAL_CELLS` accepts colon-separated directories (one per JBOD disk), choosing the
    healthy root with the most `HostDisk` spare space for incoming migrations (`daemon_internal_dirs`
    in `migration.rs:2007` also splits colon-separated `CLADEFSD_PART_CACHE` and `CLADEFSD_REPLICAS`
    so none of the JBOD internal dirs can be targeted by a peer!).
  - `CLADEFSD_EPHEMERAL` holds tiny metadata JSON files (a few KB per ephemeral cell), not bulk
    I/O. Keeping it as one directory on the durable host/OS disk (or per-disk journals if we later
    want single-disk `DecommissionHost` granularity) needs zero format changes. Note one safety
    detail if `CLADEFSD_EPHEMERAL` stays single-dir while ephemeral cells span JBOD disks: on boot,
    `retire_stale()` should not retire entries whose target mountpoint itself failed to mount (verify
    the root directory exists before retiring missing cells beneath it).

### 3.6 FUSE mount stores (`CLADEFSD_MOUNT_STORES`, `cladefsd/mount.rs`, `fuse/control.rs`)
- **Current code:** Single `CLADEFSD_MOUNT_STORES` directory; attached cells live at
  `<dir>/<TENANT>/<CELL>`.
- **JBOD design:**
  - `CLADEFSD_MOUNT_STORES` accepts colon-separated directories (`std::env::split_paths`).
  - On `Attach(cell)`: scan all configured roots for an existing `<root>/<TENANT>/<CELL>` first; if
    none exists, create it under the healthy root with the most `HostDisk` spare space.

### 3.7 Disk governor (`crates/bucket_cell/src/host_disk.rs`, `crates/cladefs/src/cladefsd/disk.rs`)
- **Already JBOD-ready:** `HostDisk` is keyed by `st_dev`, `govern_disks` already loops over every
  `HostDisk`, `cladefsd_host_disk_*` metrics are already labeled `{device="MAJOR:MINOR"}`, and
  `disks:` stats already print every device!
- **Updates needed:**
  1. `disk.rs:417`: govern **every** part-cache shard on its own `HostDisk` (not just one cache).
  2. `disk.rs:407-413` (`CLADEFSD_DISK_BYTES`): when a fixed total cap spans multiple disks,
     apportion headroom/allowance fairly by disk capacity rather than letting disk 0 grab the whole
     `fixed` budget and starve disk 1.
  3. `disk.rs:551-566` (`share()`): support multi-allotment tenants (one allotment per JBOD disk)
     so pressure on disk 0 throttles tenants on disk 0 without throttling disk 1.
  4. `disk.rs:389-399`: wire repeated `fstatvfs` / I/O errors into disk health state so unhealthy
     disks stop receiving new cell placements and part-cache writes.

---

## 4. `ubu1` current state and deployment plan

### 4.1 What `ubu1` has today (verified 2026-11-24)
- **NVMe 0 (`nvme0n1`, KIOXIA-EXCERIA PLUS G3 SSD, 1.8 TB):**
  - `bpool` (2 GiB), swap (8 GiB), `rpool` (1.8 TB ZFS: `/` 8 GiB, `/home` 385 GiB,
    `rpool/fxdax-scratch` 768 GiB thin ext4 zvol at `/mnt/fxdax-scratch` — 229 GiB used, 494 GiB free).
- **NVMe 1 (`nvme1n1`, Samsung SSD 990 PRO 2 TB, 1.8 TB — faster Gen4 NVMe):**
  - `hotbed` ZFS pool (`hotbed/hotbed` at `/var/lib/hotbed/worker/volumes` — 77 GiB used, **1.68 TB free**).
  - OpenZFS `2.4.1-1ubuntu5.1` (`direct=standard`).
- **Running `cladefsd.service` on `ubu1`:**
  ```ini
  Environment=CLADEFSD_PART_CACHE=/mnt/fxdax-scratch/cladefsd/part-cache
  Environment=CLADEFSD_REPLICAS=/mnt/fxdax-scratch/cladefsd/replicas
  Environment=CLADEFSD_EPHEMERAL=/mnt/fxdax-scratch/cladefsd/ephemeral
  Environment=CLADEFSD_EPHEMERAL_CELLS=/mnt/fxdax-scratch/cladefsd/ephemeral-cells
  Environment=CLADEFSD_ROOTS=/mnt/fxdax-scratch/cladefsd/cells
  ```
- **Running `hotbed-worker.service` on `ubu1` (`/var/lib/hotbed/worker/hotbed.json`):**
  - `"vm.cellRoot": "/mnt/fxdax-scratch/cladefsd/cells/hotbed"` (backing live Firecracker VMs).
- **Result:** 100% of `cladefsd`'s local SSD I/O (part cache, cells, replicas, ephemeral cells) hits
  `nvme0n1` (`rpool/fxdax-scratch`, competing with `/` and `/home`), while `nvme1n1` (Samsung 990
  PRO, 1.68 TB free) sits idle for `cladefsd`.

### 4.2 Target `ubu1` JBOD layout (post-implementation)
1. **Provision a second ext4 filesystem on `nvme1n1` (`hotbed` pool)** via a new `~/docs/system-setup.d`
   step (mirroring `143-fxdax_ext4_scratch.sh`):
   - Create a thin, uncompressed zvol `hotbed/cladefs-scratch` (`volblocksize=16K`,
     `compression=off`, `primarycache=metadata` — preventing double-caching in ZFS ARC and matching
     `ext4` `O_DIRECT` + `FALLOC_FL_PUNCH_HOLE` + `SEEK_HOLE` semantics rather than running directly
     on a ZFS dataset). Size e.g. `1T` (thin, out of 1.68 TB free on `nvme1n1`).
   - Format `ext4`, mount at `/mnt/fxdax-scratch-nvme1` (with `nofail,x-systemd.requires=zfs-volume-wait.service`),
     create `/mnt/fxdax-scratch-nvme1/cladefsd/{part-cache,replicas,ephemeral-cells,cells/hotbed}`
     owned by `cladefsd:cladefsd`.
2. **Update `/etc/systemd/system/cladefsd.service`** (during a clean drain window so no active
   hotbed VMs are disrupted):
   - `CLADEFSD_PART_CACHE=/mnt/fxdax-scratch/cladefsd/part-cache:/mnt/fxdax-scratch-nvme1/cladefsd/part-cache`
   - `CLADEFSD_REPLICAS=/mnt/fxdax-scratch/cladefsd/replicas:/mnt/fxdax-scratch-nvme1/cladefsd/replicas`
   - `CLADEFSD_EPHEMERAL_CELLS=/mnt/fxdax-scratch/cladefsd/ephemeral-cells:/mnt/fxdax-scratch-nvme1/cladefsd/ephemeral-cells`
   - `CLADEFSD_ROOTS=/mnt/fxdax-scratch/cladefsd/cells:/mnt/fxdax-scratch-nvme1/cladefsd/cells`
   - `ReadWritePaths=/mnt/fxdax-scratch/cladefsd /mnt/fxdax-scratch-nvme1/cladefsd /run/cladefsd`
3. **Update `hotbed` (`~/src/hotbed`)** to accept multiple `cellRoots` (or use `cladefsd`'s JBOD
   root placement) and configure `/var/lib/hotbed/worker/hotbed.json` with both roots so new VMs'
   cells and writes spread across both `nvme0n1` and `nvme1n1`.
4. **Verify via `cladefsd` metrics (`http://127.0.0.1:9184/metrics`):** confirm two distinct
   `cladefsd_host_disk_*` `{device="maj:min"}` series both serving part-cache hits/fills and cell
   footprints, with zero `EXDEV` adoption warnings.

---

## 5. Recommended implementation sequence (zero frozen-format changes)

1. **`bucket_cell::cache` JBOD (`CLADEFSD_PART_CACHE=dir1:dir2:...`):**
   - Refactor `Cache` into `Vec<Shard>` with persistent `.disk-id` UUID, weighted rendezvous routing,
     same-`st_dev` `adopt(key, from, from_dev)`, multi-shard RAM lookup, `has_disk(dev)`,
     per-disk `set_limit_on(disk, limit)`, and 4-state shard health (`Online`/`Suspect`/`Offline`/`Returning`).
   - Update `local_cell/src/cell.rs:1202` (`cache.has_disk(here)`), `cladefsd/disk.rs:417`,
     `cladefsd/storage.rs`, `host_cell.rs:718`, `cladefsd/metrics.rs`, and `cladefsd/migration.rs:2007`.
2. **Per-`HostDisk` page cache flushers (`crates/local_cell/src/pages.rs`):**
   - Replace the single global `"store flusher"` thread with one flusher per `HostDisk` so write-back
     scales linearly across NVMe drives and one slow disk cannot stall another.
3. **Multi-disk `CLADEFSD_REPLICAS`, `CLADEFSD_EPHEMERAL_CELLS`, `CLADEFSD_MOUNT_STORES`:**
   - Colon-separated path parsing (`std::env::split_paths`), derived composite incarnation for
     `Replicas` (preserving doc-21 safety with zero frozen-format change), spare-weighted placement
     for mounted and migrated cells.
4. **Multi-disk tenant `Grant` + daemon-side / `hotbed` cell placement:**
   - Additive `repeated string dirs` on `GrantRequest` (`cladefsd.proto` v1), per-disk tenant
     allotments in `footprint.rs` / `disk.rs`.
   - Spare-weighted cell-root selection (in `cladefsd` and/or `hotbed`'s `internal/cladefs/client.go`).
5. **Testing (on hotbed Linux cluster + `madsim`):**
   - Multi-disk unit/crash/proptest coverage in `bucket_cell::cache` (heterogeneous capacities,
     cross-disk adoption routing, shard `EIO` failover & recovery, restart persistence).
   - `local_cell` + `cladefsd` integration tests with 2 distinct filesystems (e.g. loop ext4s or
     `madsim` multi-disk) verifying zero `EXDEV`, independent disk governor pressure, and replicated
     incarnation invalidation on single-disk loss.
6. **Deploy on `ubu1`:**
   - Add `hotbed/cladefs-scratch` ext4 zvol on `nvme1n1` (`Samsung 990 PRO 2TB`), roll out updated
     `cladefsd` + `hotbed`, verify both NVMe devices active in `/metrics`.
