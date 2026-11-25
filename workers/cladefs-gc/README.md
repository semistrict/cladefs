# cladefs-gc Cloudflare Worker (WASM)

Runs `bucket_cell::collector::Collector` compiled to `wasm32-unknown-unknown` (`crates/gc_wasm`)
inside Cloudflare Workers against a native `R2Bucket` binding, fanning out one cell per Cloudflare
Queue message (`cladefs-gc-cells`) on a 6-hour cron trigger (`0 */6 * * *`).

## Architecture

- **Keyless, pure-Rust WASM** (`crates/gc_wasm` -> `bucket_cell` with `default-features = false,
  features = ["keyless-gc"]`): no C dependencies (`aws-lc`, `zstd-sys`, `crc-fast`), no tokio, no
  disk I/O, and zero encryption keys. Uses pure-Rust `ruzstd` and Castagnoli CRC32C.
- **Streaming low-memory checkpoint recovery** (`Collector::stream_checkpoints(true)`): reads only
  a checkpoint's `TableFooter` (~50 KiB) when no tail segments shadowed checkpointed extents, and
  streams 1 MiB parts with a <=64 KiB carry buffer when tail frees exist — staying well under
  Cloudflare Workers' 128 MiB isolate RAM limit even on 50+ MiB checkpoints.
- **Per-cell queue fan-out**: `scheduled` lists all cells and reads `events` for shredded tenants,
  then enqueues one message per cell to `cladefs-gc-cells` (`maxBatchSize: 1, maxConcurrency: 16`).
  Each cell runs in its own Worker invocation with full CPU/memory headroom and coordinates via
  `<C>gc/lease` exactly like native `cladefs-gc`.

## HTTP Endpoints

- `GET /healthz` — unauthenticated health probe
- All other endpoints require `Authorization: Bearer <GC_TOKEN>`:
  - `POST /sweep` — enqueue a full sweep to `cladefs-gc-cells`
  - `POST /cell/:tenant/:cell` — synchronously collect one cell
  - `GET /cells` — list all cells in the bucket
  - `GET /dry-run` — dry-run all cells sequentially
  - `GET /dry-run/:tenant/:cell` — dry-run one cell
  - `GET /usage` — read recorded usage from all cells' collector leases
  - `GET /usage/:tenant/:cell` — read recorded usage for one cell
  - `GET /live/:tenant/:cell` — check whether a cell's head is live

## Build, Test, and Deploy

```bash
# 1. Build WASM via hotbed (from repo root or workers/cladefs-gc)
./scripts/build-wasm.sh

# 2. Typecheck, test against Miniflare R2, and bundle
pnpm install
pnpm run typecheck
pnpm test -- --run
pnpm run build

# 3. Deploy to Cloudflare
cf deploy
```
