---
id: doc-22
title: 'Disk governor: the host''s disk as a managed resource'
type: specification
created_date: '2026-11-18 14:14'
updated_date: '2026-11-18 14:18'
---
# Disk governor: the host's disk as a managed resource

Status: design, 2026-11-18. It replaces disk.rs's directory walks (TASK-94) and the part cache's fixed floor. It folds in TASK-538, TASK-525, TASK-520, TASK-355, TASK-521 and TASK-443.8. Modelled on the memory governor (TASK-358, `cladefsd/governor.rs`, `cladefsd/memory.rs`).

## Requirements (user, 2026-11-18)
1. The common case is a whole machine dedicated to cladefs. The daemon takes all of every filesystem it uses except a headroom.
2. A fixed upper limit is also supported (`CLADEFSD_DISK_BYTES`).
3. Caches get a dynamic limit from free space. At about 95% used, or under X bytes free, they start evicting: being a good neighbour.
4. Bytes are counted at the source, as the stores write them, not by walking directories.

## Decisions (user, 2026-11-18)
- **Default headroom:** 5% of the filesystem, at least 4 GiB, at most half of it. `CLADEFSD_DISK_HEADROOM` takes bytes or `N%`.
- **Under pressure guests get `ENOSPC` on writes**, after the cache has yielded and the uploaders have been urged. Commits are not stalled. Revisit this with T14's measurements.
- **The part cache yields to zero before any guest write is refused.** "Derived first", as memory does. There is no protected minimum.
- **Reserve:** tenant and total limits are hard bounds, so the reserve counts against them. The reserve applies only to unframed data and to writes in flight, never to framed or uploaded bytes. Its factor (`RESERVE_FACTOR`) is the worst case that T12 proves in the sim world, and admission uses no factor until T12 has fixed it. 3 is only the placeholder from the argument in §4. Measured on ubu1's qualify runs, each guest byte costs 0.15 copied + 0.04 framed + 0.01 log, about 0.2 extra on average (docs/measurements/ubu1-qualify-ab-2026-11-12).
- **Taken as recommended:**
  - v1's `GetTenantDiskResponse.bytes` and the `SetDiskLimit` docs are reworded in place, with the wire unchanged: "its stores' files, each file's length rounded up to 4 KiB, once however many names it has, counted as written".
  - doc-21's replica logs are charged explicitly to the cell's tenant by ID, and always against the HostDisk.
  - Files in a tenant directory outside any store are not counted against the tenant. They show in `cladefsd_host_disk_other_bytes` and stay within the headroom.
- **Still open:** whether the ignored benchmark `measure_disk_recounts_with_resident_pages` (TASK-525) is deleted once recounts are gone. Ask before deleting it.

## 0. How the design was reached

The spine is **accounting-first** (design 2): a `Counted` Dir layer, admission inside the store, and one per-filesystem object. It takes these grafts from the others:
- From mirror-memory: the `Dir::give` hand-off, `settle_for_write` charging, the `other_bytes` budget check, and the memory-governor shape and names.
- From good-neighbour: the hysteresis band, admission that leans on at most one band of not-yet-evicted cache, `N%` headroom, and the hardening list.

Contradictions between the designs, and how each was resolved. All verified in the code.

1. **`settle_for_write` has a caller.** Design 2 says it has none and deletes it. That is wrong: `crates/cladefs/src/storage/image.rs` calls `stored.file.settle_for_write(stored.offset..stored.offset + bytes.len() as u64)?;` and then writes through its own fd with `self.io.run(stored.file.clone() as IoFile, op)`. It stays, and `Counted` charges `range.end` on it.
2. **"room" is a reserved word.** CONTEXT.md Ambiguities says: "**room**: a segment's free space alone." Designs 2 and 3 use "room" for host free space. This design says **spare** instead.
3. **`Space` is taken.** Design 3's type name collides with `crates/local_cell/src/state.rs` `pub(crate) struct Space`, and the glossary rule is "two types of one name are one concept". This design uses **HostDisk**.
4. **Attribution stays path-based.** Design 1 charges by the cell's tenant. That changes the meaning of `SetDiskLimit`, whose proto says "what is in its directory, its stores' files". The path rule is kept: a store's files are the granted tenant's whose directory holds it. Every store also charges its HostDisk, so stores outside granted directories are still governed. These are the default tenant's, control-API, FUSE-mount and version stores, which today are counted nowhere.
5. **MemDisk refuses rename-over.** `crates/local_cell/src/mem.rs` has `DirChange::Rename(_, to, _) if entries.contains_key(to)`. Design 2 would relax that check. Instead, the rewrites of small records get their own `Dir::replace` operation, and the store's own no-overwrite renames keep their check.
6. **`CLADEFSD_PART_CACHE_BYTES` is kept** as a cap inside the governed limit. Design 1 deletes it. The sandbox team uses it (TASK-406.1).
7. **Admission moves inside the store**, at `begin_write_locked` and `make_copy`. Designs 1 and 3 keep it at server.rs. Inside the store it covers every writer with one check: VM socket, FUSE, image device (`write_file` takes its `Write` from `begin_write`), control-API makings and forks.
8. **A daemon without a part cache is still governed**, per filesystem. Design 1 leaves it ungoverned.
9. **Fixed mode is a ceiling on top of free-space governing.** All three designs agree on this. It differs from `MEMORY_BYTES` on purpose: running a disk out of space fails stores (`Standing::Failed`), so free space is always watched.

## 1. Model and words

| Word | What it is | Where |
|---|---|---|
| **HostDisk** | One host filesystem the daemon keeps stores or the part cache on. Keyed by `st_dev`, with an open fd sampled by `fstatvfs`. Counters (atomics): `stores`, `unframed`, `pledged`, `cache`, `cache_writing`. Published by the governor: `allowance`, `headroom`, `pressure`. | new `crates/bucket_cell/src/host_disk.rs`. bucket_cell is the lowest crate both `local_cell` and the part cache depend on, beside `direct.rs`. |
| **Footprint** | One store's bytes on its HostDisk: the sum over its files' distinct inodes of `round_up(len, 4096)`. Also tracks **unframed**: its files under `open-segments/` and `open-extents/`, which are raw data whose framing or own-extent sealing is still to come. | new `crates/local_cell/src/footprint.rs` |
| **Allotment** | A granted tenant directory's footprints summed, with that directory's `limit`, `share`, `pledged` and `full` flag, which cladefsd sets. There is one **total allotment** over all allotments. | `local_cell::footprint` |
| **Reserve** | `RESERVE_FACTOR × unframed` (factor proven by T12; 3 is the placeholder): what a store's own unrefusable work may still add for data already written. | derived |
| **Pledge** | What an admitted write holds until it lands: `(1 + RESERVE_FACTOR) × bytes`. | `footprint::Pledge` guard |
| **Allowance** | What the daemon may keep on a HostDisk. | governor |
| **Spare** | `allowance − stores − reserve − pledged`. What the part cache and new guest writes share. | governor and admission |
| **Headroom (H)** and **band** (`H/4`) | What is left free between looks, and the hysteresis step. | governor |

Glossary checks:
- Not "volume" (Fxfs's), "account" (avoided for tenant) or "room" (a segment's).
- `Found` is taken (`bucket_cell/src/cache.rs` `struct Found`), so the listing type is `DirFile`.

## 2. Counting at the source: `Counted<D>` (`crates/local_cell/src/counted.rs`)

### 2.1 Placement: outermost, above the store page cache
- `pub type HostDir = Counted<CachedDir<DiskDir>>;` and `pub type MemDir = Counted<CachedDir<MemDisk>>;`. Today they are `pub type HostDir = crate::CachedDir<DiskDir>;` (dir.rs) and `pub type MemDir = CachedDir<MemDisk>;` (mem.rs). Tests therefore run the production layer.
- `HostFile` becomes `CountedFile<CachedFile<DiskFile>>`. It forwards `AsRawFd`, `settle` and `settle_for_write`, so `image.rs`'s `IoFile` cast keeps working.
- A write is counted when the store page cache takes it, not at write-back. This removes three bugs:
  - **TASK-538:** nothing ever needs writing back to count.
  - **TASK-520:** a failed write-back leaves the file's length, and so its charge, where it was. `pages.rs` already keeps the pages dirty: `if wrote.is_err() { continue; }`.
  - **TASK-355:** there is no `st_blocks`.
- A file removed before write-back is uncharged at removal, whether or not its pages ever reached the disk. `cached.rs` says "A file removed drops its pages, its dirty ones unwritten."

### 2.2 Per file
`CountedFile { file, len: Arc<FileLen>, footprint }`. `FileLen` is one `AtomicU64`: the counted length rounded to 4096, plus an `unlinked` bit.

**`write_at`:**
1. Call the inner write.
2. On `Ok`: `fetch_update` the length to `max(round_up(off+len))` unless unlinked. If it grew, call `footprint.grow(delta)`. That adds to the footprint, its allotment and the total allotment (if tallied), and its HostDisk, plus `unframed` when the file is under `open-*`.
3. On `Err`: raise the length to the inner `size()`. A partial O_DIRECT write or `bucket_cell::direct::write_at`'s `file.set_len(end)?` may have grown the file, so this stays conservative.

**`settle_for_write(range)`:** charge `round_up(range.end)` before the caller's own-fd write. This covers the image device.

**Reads, `size`, `sync*`, `write_back`, `write_behind`:** pass straight through.

### 2.3 Names
`Footprint.names: Mutex<HashMap<String, Arc<FileLen>>>`, with a name count per `FileLen`. The inner operation runs first, and counts change only on `Ok`.

| Op | Effect |
|---|---|
| `create` | new `FileLen` |
| `open` of an unknown name | sized from `size()` and charged; `debug_assert!`, since it means a bypass |
| `remove` | names −1; at 0 set `unlinked` and uncharge |
| `rename` | moves the entry. Crossing `open-extents/` → `extents/` (sealed in place) moves bytes from unframed to plain. |
| `link` | shares the `Arc`, so in-place framing's `dir.link(raw_path, path)` counts once |
| `create_dir` | one block |
| `give` (§2.5) | on `Ok(true)` acts as `remove` |
| `replace` (§2.6) | swaps the record |

### 2.4 Ledger and seeding
The ledger is process-wide in `footprint.rs`, keyed by the store directory's `DirId` (`DiskDir::id()`, the `(dev, ino)` that `TAKEN`/`CLAIMED` in dir.rs already use).
- `HostDir::new(path)` attaches to the entry, so two HostDirs of one store share one footprint.
- **Seeding and reconciliation:**
  - The first opening while no other `Counted` of the store is alive lists the directory once with the new `Dir::files() -> io::Result<Vec<DirFile { path, len, inode }>>`. DiskDir uses `symlink_metadata` and `st_ino`; MemDisk uses its inode table; CachedDir forwards.
  - Names are grouped by inode, and the delta from the retained total is charged.
  - When the last opening drops, the footprint keeps only its totals (bytes, unframed) and drops its names table. Closed stores stay charged, and edits made by other processes (cladefsctl) are corrected at the next opening.
  - `MemDir::crash(seed)` reseeds, so every crash test exercises seeding.
- **Removal:** one function, `local_cell::remove_store_dir(path)`, removes a store's directory and forgets its ledger entry. It replaces:
  - `host::remove_store` (in `delete_host`);
  - `cladefsd/cells.rs` `removed_if_unmade`'s `std::fs::remove_dir_all(path)`;
  - the three `let _ = std::fs::remove_dir_all(...)` calls in `crates/cladefs/src/host_cell.rs` (failed make, failed fork, `fork_through`'s version store).

  A removal that stops partway keeps the old count, which is conservative.

### 2.5 Hand-off to the part cache: `Dir::give` replaces `Dir::host_path`
Today `leave()` (cell.rs) does `match (cache, key, dir.host_path(&path)) { (Some(cache), Some(key), Some(from)) => cache.adopt(&key, &from)...`, which renames outside Dir.

New method:
```
fn give(&self, path: &str, to: &mut dyn FnMut(&Path) -> io::Result<bool>) -> io::Result<bool>
```
- **DiskDir:** calls `to(self.path.join(path))`.
- **MemDisk:** `Ok(false)`, as `host_path` returns `None` today.
- **CachedDir:** settles and cancels the file's pages (moved from its `host_path`), then forwards.
- **Counted:** uncharges on `Ok(true)`.

`leave()` becomes `dir.give(&path, &mut |from| cache.adopt(&key, from))` and removes the file when it was not given.

The other use is the device check in `Recovering::open`: `if let (Some(cache), Some(here)) = (&cache, dir.host_path(""))` … `ensure!(device(&here)? == device(cache.dir())?, ...)`. It becomes `Arc::ptr_eq(dir.disk(), cache.disk())`. `host_path` is deleted.

### 2.6 Bypasses closed
These std::fs writes into store directories now go through Dir:
- `local_cell/src/host.rs` `place_host` (`std::fs::File::create(&new)` … `std::fs::rename(&new, path.join(DESCRIPTION))`)
- `made_host` (`std::fs::remove_file(path.join(UNMADE))`)
- `cladefs/src/cell_dir.rs` `write` (`std::fs::write(&new, text)` / `std::fs::rename(&new, dir.join(name))`) and `mark` (`std::fs::File::create(dir.join(name))`)
- `profile.rs`'s `std::fs::create_dir(dir)`

How:
- A new **required** method `Dir::replace(path, bytes)` writes, syncs and atomically replaces, then syncs the directory.
  - DiskDir: `.new`, `sync_all`, `rename`, dir sync.
  - MemDisk: a new `DirChange::Replace` that the crash model drops or keeps like any change, keeping the old or the new file.
- `Dir::rename`'s no-overwrite contract and MemDisk's check stay as they are.
- A guard test greps non-test sources of `local_cell`, `cladefs` and `bucket_cell` for `std::fs::(write|File::create|rename|remove_file|remove_dir_all|hard_link|OpenOptions)`. It allows only `dir.rs`, `cache.rs`, `direct.rs`, `host_disk.rs`, `remove_store_dir`, and the daemon's sockets and stats file.

### 2.7 Granule
The granule is a fixed 4096 (`extent_frame::ALIGN`). That makes counts deterministic and equal across MemDisk, ext4, XFS and tmpfs. On ZFS with compression the count is higher than blocks used, which is conservative. The cache's tails are counted the same way: `len.next_multiple_of(ALIGN)`, matching its test `evicted_parts_free_their_blocks`.

## 3. Attribution: allotments by granted directory (v1 meaning kept)

- `Tenant::new` → `Disk::new` calls `footprint::allotment(&dir)`. That returns the directory's `Arc<Allotment>`, made once and kept for the daemon's life.
- Making an allotment:
  1. attaches every footprint in the ledger whose canonical path is `in_dir` (the rule in `cladefsd/server.rs` `pub(super) fn in_dir`, moved to local_cell);
  2. **seeds** the stores under the directory that the ledger lacks. A store directory is one holding `store`, `store.deleted` or `unmade`; seeding does not descend into it.

  This one listing per grant replaces `Disk::new`'s synchronous `disk.recount()`, without its `write_back_all` and without any later walks.
- A footprint made later finds its allotment by the longest `in_dir` match under the ledger lock.
- A tenant revoked and granted again with another directory has two allotments, each limited, exactly as the proto says today: "A tenant revoked and granted again while its earlier VMs run has the limit for each of its directories."
- Stores in no granted directory charge only their HostDisk. These are the default tenant's, control-API stores elsewhere, `$TMPDIR` version stores (`host_cell::version_dir` → `std::env::temp_dir().join(format!("cladefs-versions-{}", ...))`) and FUSE mount stores. The default tenant stays without limits, as `grants::check_tenant` keeps it.
- The part cache stays no tenant's (CONTEXT.md: "Part cache: A host's disk copies of the bucket's objects' parts, no tenant's.").

## 4. Admission: inside the store, one place for every writer

```
Footprint::admit(bytes) -> Result<Pledge, DiskFull>
```

It is called in `LocalCell::begin_write_locked`, right after `state.check_room(grows, overwrites)?` with `bytes = len`, and in `LocalCell::make_copy` before the directory is made, with `STORE_MADE` (64 KiB). The `Write` holds the pledge until `finish` or `Drop`. Everything is lock-free atomics, so it is fine under the state lock.

Let `p = (1 + RESERVE_FACTOR) · bytes`. The write is admitted iff all of these hold:
1. **HostDisk:** `stores + RESERVE_FACTOR·unframed + pledged + p + max(0, cache − band) ≤ allowance`.
   - At most one band of cache bytes not yet evicted is relied on (graft from good-neighbour).
   - If `cache > spare − p`, the governor is woken so the cache yields now.
2. **Allotment limit** (if tallied): `bytes + RESERVE_FACTOR·unframed + pledged + p ≤ limit`. The reserve counts here (decided): limits are hard bounds.
3. **Total allotment limit:** the same check over every allotment.
4. **Share under pressure** (if tallied, and the HostDisk is under pressure): the left side of (2) ≤ `share`.

On refusal:
- `local_cell::DiskFull { allotment, disk }`;
- `full` is set and logged once per episode, as `Disk::refuse` does now;
- the governor is woken;
- `crates/cladefs/src/open_cell.rs` `errno` maps `DiskFull` to `ENOSPC`, beside the existing `error.is::<NoSpace>() || error.is::<TooSmall>()`. FUSE gets the same errno.

**Never refused, only counted:** commit, framing, flush, log appends, the acknowledgement's start file, adoption and recovery. This is doc-7 decision 3 and TASK-94 ("Commits go on"). The reserve plus the headroom is their room.

**Why 3 as the placeholder for `RESERVE_FACTOR`.** Before its raw file goes, a byte written raw is written at most:
- once more by a commit's copy (`written::Kind::Copied`);
- once into a log record (`Runs::Held`, only when a flush seals ≤ `held_most` = `256 << 10`);
- once framed (`Kind::Framed`/`Own`, compressed ≤ raw).

This is argued, not proven. Task T12 is the proof obligation: it measures the bound in the sim world and fixes the constant from what it finds.

## 5. Governor loop

Policy lives in `governor.rs` as pure functions; the mechanism is in `disk.rs`. The thread is `"disk governor"`. It is started by `disk::start(DiskConfig)` from `bin/cladefsd.rs` always, and from `tests/common/daemon.rs` with a 64 MiB headroom, because production runs it (memory: tests run production layers). Start fails if a variable is malformed or the first `fstatvfs` fails, like `governor::start`.

A pass runs every `governor::INTERVAL` (100 ms), or at once when woken: by an admission refusal, or by `HostDisk::grow` seeing `cache > spare`. For each live HostDisk `d`:
1. Sample `fstatvfs` on its fd: `size = f_blocks·f_frsize`, `free = f_bavail·f_frsize`, `used = (f_blocks − f_bfree)·f_frsize`.
2. `in_flight = local_cell::pages::shared().usage().dirty + d.cache_writing`. These are bytes counted but not on disk yet.
3. `H = disk_headroom(size)`: `CLADEFSD_DISK_HEADROOM` as bytes or `N%`, by default `max(size/20, 4 GiB)` clamped to `size/2`. `band = H/4`.
4. `A = sat(d.stores + d.cache + free − in_flight − H)`. If `CLADEFSD_DISK_BYTES = N` is set: `A = min(A, sat(N − Σ_{e≠d}(e.stores + e.cache)))`. This is the memory governor's `held + available − headroom` shape.
5. `reserve = 3·d.unframed`; `spare = sat(A − d.stores − reserve − d.pledged)`.
6. **Derived first** (only on the cache's HostDisk): `limit = min(CLADEFSD_PART_CACHE_BYTES or ∞, spare)`; `cache.set_limit(limit)`. When over, it evicts least recently read down to `sat(limit − band)`, on this thread, at once.
7. **Pressure** enters when `spare < H` and leaves when `spare ≥ H + band`.
   - On entering, `d.urge()` wakes every uploader registered on `d`: `progress.wanted = true; wake.notify_one()`, which the loop folds in through `urgent |= std::mem::take(&mut progress.wanted)`. Each closes its open segment now instead of waiting `filling_wait` (30 s), and uploads. Uploaders register through `dir.disk()` in `Uploader::start`.
   - While under pressure: `shares(sat(A − untallied stores − their reserve), weights)` over the allotments whose directory is on `d`, reusing `governor::shares` unchanged, and each allotment's `share` is published. Out of pressure, `share = u64::MAX`.
8. Publish `allowance`, `headroom`, `pressure` and `other = sat(used − d.stores − d.cache)`. On a dedicated filesystem, `other` stays at filesystem metadata; growth there means an uncounted writer.

**Why sampling suffices.** A write the daemon makes raises `held` and, once written back, lowers `free` by the same amount, so `A` is unchanged. The 100 ms sample only has to catch neighbours. Removals lower `held` at once and raise `free` only when the filesystem reports it (ZFS txg), so `A` dips, which is conservative. Files unlinked while open behave the same way.

**Failures.** If `fstatvfs` fails, the last allowance is kept (it is still exact for the daemon's own writes), the error is logged once per episode and `cladefsd_host_disk_sample_errors_total` counts it. Cache eviction I/O errors are logged as today; their bytes have already left the index, so `A` falls, which is conservative.

**Without the thread** (local_cell tests, tools such as cladefsctl): `allowance = u64::MAX` (ungoverned), as memory's `GOVERNED` starts at `u64::MAX`. Tests drive `disk::govern_once()`.

## 6. Modes
- **Dedicated (the default, nothing set):** every filesystem the daemon uses is cladefs's except H. Stores come first; the part cache fills what they leave.
- **Fixed:** `CLADEFSD_DISK_BYTES=N` caps everything the daemon keeps (stores plus part cache, all HostDisks) at N, on top of the free-space watch (the `min`). Per-tenant and total `SetDiskLimit` limits stack on both modes.
- `CLADEFSD_PART_CACHE_BYTES` caps the cache alone, inside the governed limit.
- `CACHE_FLOOR` is deleted; the headroom replaces it, with the same 4 GiB minimum.

## 7. Part cache (`crates/bucket_cell/src/cache.rs`)
- Fields `capacity: u64` and `floor: u64` become `limit: AtomicU64`, plus `disk: Arc<HostDisk>` from `HostDisk::of(dir)`.
- `Cache::open(dir, limit)` replaces `open(dir, capacity, floor)`.
- `Files::free` (`statvfs`, `stat.f_bavail as u64 * stat.f_frsize as u64`) moves to `host_disk.rs`.
- `make_room((capacity, floor), free, len, needed)` becomes `make_room(limit, len)` in **two passes**: choose victims until `bytes + len ≤ limit`, and apply them only if it fits.
  - Today `let evicting = self.evict(evicted); if !fits { return evicting; }` applies evictions for a fill that is then refused. Fixing this resolves TASK-443.8.
  - `bytes` already counts fills in flight (`kept.bytes += len` at admission) against one limit under the index lock, which resolves TASK-521.
- New `set_limit(bytes) -> io::Result<u64>` stores the limit and evicts down to the target. It keeps `make_room`'s rules: a tail goes with its last part, and busy entries are skipped.
- Every change to `Index.bytes` moves `disk.cache` too. `Busy::Writing` admissions also move `disk.cache_writing` until `settle`.
- **Adoption** moves bytes from the store's footprint to the cache, so held is unchanged.
  - It evicts least recently read data to fit the limit (the adopted object is the newest) and refuses only when `len > limit`. A refused adoption removes the file, as `leave()` does today.
  - The cache's own comment already says the blocks exist: `// Its file's blocks are allocated already.`

## 8. Backpressure on data that cannot be evicted
The ladder per HostDisk:
1. The part cache shrinks (derived data).
2. Pressure: uploaders are urged, and allotments over their weighted share are refused.
3. A full spare: every guest write and store making on that disk gets `ENOSPC`. Commits, framing, flush, acknowledgement and recovery go on inside the reserve and the headroom.

The lag bound is unchanged. `upload.rs` keeps `max_lag_bytes: 1 << 30` per store, gating `Commit::Ordered` through `wait_lag`. Commits do not stall under pressure (decided); guests get `ENOSPC`.

## 9. Tenant limits and weights; control API; environment
- `disk::set_limit` keeps its signature and its persistence by ID. It writes `allotment.limit` on every allotment of the ID; the total limit writes the total allotment's. The default tenant is `EINVAL`, as today.
- New `disk::set_weight(tenant, weight)`: `DEFAULT_WEIGHT = 1`, the default tenant refused, weights persisted for later grants. This mirrors `memory::set_weight`.
- **Proto** (`cladefs.cladefsd.v1`, which only grows):
  - New `rpc SetDiskWeight(SetDiskWeightRequest{uint64 tenant; uint64 weight})`; the Go client is regenerated.
  - `SetDiskLimit` and `GetTenantDisk` keep their wire. `GetTenantDisk` reads counters without `cells::blocking`. The doc of `GetTenantDiskResponse.bytes` ("as `du -sx` counts them") is reworded in place (decided).
- **Environment**, all parsed at start and fatal if bad, like `cache_config()`:
  - `CLADEFSD_DISK_HEADROOM` (bytes or `N%`)
  - `CLADEFSD_DISK_BYTES` (positive bytes)
  - `CLADEFSD_PART_CACHE` and `CLADEFSD_PART_CACHE_BYTES`, unchanged.

## 10. Metrics and stats
- **Kept:**
  - `cladefsd_disk_bytes` (total allotment) and `cladefsd_disk_limit_bytes`;
  - `cladefsd_tenant_disk_bytes{tenant}` and `_limit_bytes`;
  - `cladefsd_part_cache_bytes`;
  - `cladefsd_part_cache_capacity_bytes`, which now reports the governed limit.
- **New, per HostDisk `{device="maj:min"}`:** `cladefsd_host_disk_size_bytes`, `_free_bytes`, `_headroom_bytes`, `_allowance_bytes`, `_stores_bytes`, `_unframed_bytes`, `_pledged_bytes`, `_spare_bytes`, `_other_bytes`, `_pressure` (0/1), `_refused_total`, `_sample_errors_total`.
- **New per tenant:** `cladefsd_tenant_disk_weight{tenant}` and `cladefsd_tenant_disk_share_bytes{tenant}` (only while under pressure).
- **Stats file:** a `disk:` line per device beside `cache:`.

## 11. Failure handling

| Failure | Behaviour |
|---|---|
| Inner write fails | Count raised to `size()` |
| Inner remove or rename fails | Nothing changes |
| Seeding fails | `HostDir::new` fails, so the opening fails; at grant, the grant fails with `EIO` naming the store (a tenant left uncounted would be unlimited) |
| Store deleted by another process | Over-counted until its next opening or a restart (conservative) |
| Neighbour writes faster than one band per 100 ms | Can still reach real ENOSPC; the hardening tasks (T13) keep stores alive through transient write-back and acknowledgement failures |

The pre-existing store-fatal paths are filed and fixed separately (T13; TASK-517 already covers `throttle`):
- `pages.rs` `if let Some(failed) = shared.lock().failed.take() { return Err(failed); }` fails a flush after space returns.
- `cell.rs` `acknowledged`: `if let Err(err) = start.write(&*self.dir, after)` → `fail`.
- `PageCache::throttle`'s `while self.dirty.load(Ordering::Relaxed) > self.hard()` has no error exit.

## 12. What is deleted
- **`crates/cladefs/src/cladefsd/disk.rs`:**
  - `walk`, `Disk::recount`, `recounter`, `RECOUNT_EVERY`, `RECOUNT_GAP`, `NUDGED`/`nudge`/`nudged`, `counting`, `writing`, `used`, `dir`;
  - the `Vec<Weak<Disk>>` registry, which becomes a map by ID over allotments;
  - `USED`, which becomes the total allotment;
  - the `local_cell::pages::shared().write_back_all()` call and its warning;
  - the blocking `count`.
  - The module doc's "Not directory quotas" argument stays; the `du -sx` text goes.
- **`server.rs`:** `Tenant::admit_to_disk`, its three `let _admitted = self.tenant.admit_to_disk(...)` sites, and `disk::Admitted`.
- **`open_cell.rs`:** `OpenCell::disk_bytes` and local_cell's `Usage::disk_bytes`. The `ENOSPC` log prints the footprint instead.
- **`cache.rs`:** `floor`, `capacity`, `Files::free`, and `make_room`'s `free`/`needed`.
- **`cells.rs`:** `CACHE_FLOOR`.
- **`local_cell`:** `Dir::host_path` on the trait, DiskDir and CachedDir.

## 13. Migration of disk.rs
There is no on-disk format change (AGENTS.md: no migration). `disk.rs` becomes policy only:
- `Disk { tenant, allotment: Arc<Allotment> }`;
- `set_limit`, `set_weight`, `total`, `total_limit`, `count` (non-blocking), `tenants`;
- `start`, `govern_once`.

The mechanism moves to `local_cell::{counted, footprint}` and `bucket_cell::host_disk`. `Tenant.disk` stays `Option`: the default tenant has no allotment, and its stores are admitted against their HostDisk inside the store.

## 14. Tests that prove it
All tests live in the repo. Linux ones run on ubu1 through `fxjob`, one job at a time.
1. **Counted oracle (proptest over MemDir, and over HostDir on a tempdir):**
   - Random create, write, overwrite, link, rename, remove, give (accepted or refused), replace, `settle_for_write`, MemDisk `fail_after`, and `crash(seed)`.
   - After every step, `footprint == Σ round_up(len,4096)` over the distinct inodes of `files()`, plus directories; unframed equals the same sum over `open-*`.
   - A failed inner write raises the count to `size()`; a write after the last unlink charges nothing.
2. **Sim world:** the same oracle after every step of `follower_sim`/local_cell scenarios, read from MemDisk after `write_back_all`. The write-back is the oracle's only, never production's.
3. **Ledger:**
   - reopen gives an equal count;
   - two HostDirs of one store share one count;
   - an edit made while closed is reconciled at reopen;
   - `remove_store_dir` and a failed make return the allotment to its earlier count;
   - grant seeding counts a closed store once and skips an open one.
4. **Part cache:**
   - `set_limit` evicts least recently read down to the target;
   - a fill that cannot fit evicts nothing (TASK-443.8);
   - TASK-521's `free_space_admission_reserves_space_for_unwritten_fills` restated against the limit, and passing;
   - adoption over the limit evicts older objects first;
   - `crashes_at_each_change_leave_nothing_read_back_wrong` passes.
5. **Admission with `MemDisk::with_capacity(n)`:** `write_at`, `create` and `create_dir` past n fail with `ENOSPC`, and `space()` reports `n − Σ`. Guests write until refused; no store reaches `Standing::Failed`, and every `ENOSPC` the MemDisk returned was on an admitted path.
6. **Hostile tests:**
   - `a_tenant_filling_its_disk_leaves_others_committing` and the proptest assert `counted ≤ limit` at every sample, with no `+1 MiB`.
   - `crates/cladefs/mutants/tenants-disks-unlimited.patch` is retargeted to `Footprint::admit` and still killed.
7. **TASK-520:** `failed_writeback_preserves_disk_admission` (seccomp `EIO` on `pwrite64`) passes against an allotted store with no recount.
8. **TASK-538:** counting and admitting one tenant leaves another's dirty pages dirty, and a grep test finds no non-test caller of `write_back_all` in cladefsd.
9. **TASK-355:** `tests/disk.rs` on ubu1's ZFS `TMPDIR`, and the hostile seed 53 at `CLADEFS_HOSTILE_CASES=1920`, end within the limit.
10. **Governor unit tests (pure):** `disk_headroom` (bytes, `5%`, the default, malformed values), `disk_allowance`, `spare`, the pressure hysteresis, and shares 3:1.
11. **`govern_once` on a MemDisk HostDisk:**
    - growth past spare wakes the governor and the cache shrinks in one pass;
    - entering pressure sets `wanted` on a registered uploader;
    - weights 3:1 refuse the heavier tenant while the lighter one writes, and both commit.
12. **`tests/disk.rs`:** `tenant_disk == Σ rounded lengths` (and `du -sxB1` on tmpfs). The 16 × 1 MiB-then-`ENOSPC` arithmetic is restated for the pledge. The "count drops within 60 s" step passes on counters.
13. **End to end, root only** (like the guest loop-mount tests of TASK-307: `#[ignore]` unprivileged, run as root by fxjob):
    - The daemon runs with its part cache and stores on a 1–2 GiB RAM-backed loop ext4, with `CLADEFSD_DISK_HEADROOM=256 MiB`, a neighbour `fallocate`, and a gated bucket.
    - Asserts:
      - a 10 ms `statvfs` sampler never sees `f_bavail < band`;
      - the cache empties before the first refusal;
      - guests see only `ENOSPC` on WRITE;
      - no store fails;
      - uploads drain and writes resume once the bucket is ungated;
      - the cache evicts within two looks of the neighbour;
      - `other_bytes < 64 MiB`.
    - Under 5 minutes.
14. **Cost:** cell-bench `durable` and `burst` with `fxjob --bench`, within noise of main.
15. **Last:** `cargo nextest run --workspace` on macOS and on ubu1 under fxjob.

## Tasks
TASK-539 is the parent task. The design's T numbers map to these tasks:

| T | Task |
|---|---|
| T1 | TASK-539.1 HostDisk per filesystem |
| T2 | TASK-539.2 Dir::give |
| T3 | TASK-539.3 every change through Dir |
| T4 | TASK-539.4 Counted layer and ledger |
| T5 | TASK-539.5 allotments, no walks |
| T6 | TASK-539.6 part cache under a governed limit |
| T12 | TASK-539.7 prove the reserve |
| T7 | TASK-539.8 admission inside the store |
| T8 | TASK-539.9 governor loop and modes |
| T9 | TASK-539.10 SetDiskWeight |
| T10 | TASK-539.11 metrics |
| T13 | TASK-539.12 hardening |
| T14 | TASK-539.13 end to end on ubu1 |
| T11 | TASK-539.14 docs |
