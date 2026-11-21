# The simulations' branch coverage (board 2842, 2026-11-21)

Which branches of the protocol's code no seed of the simulations takes (TASK-685.10). Measured with
`scripts/sim-coverage.sh` (cargo-llvm-cov over rustc's `-C instrument-coverage`, nightly's branch
coverage), the simulations alone, none of the unit tests:

- **local_cell::sim**: its 29 profiles (`test_holds!` and the like), not its rule searches, on the
  Mac (`scripts/sim-coverage.sh local`, 2.5 minutes);
- **the full-stack simulation**: `crates/cladefs/tests/madsim.rs`'s 20 tests, searches aside, over
  seeds 1 to 20, each a process (`scripts/sim-coverage.sh madsim`, asahimbp, under fxjob);
- **both**: a branch side taken if either took it (`scripts/coverage-branches.py` given both
  exports), what no simulation reaches.

Main at 04bd743da with board 2830's fault profiles. The nightly runs both from now on
(`sim-coverage` in `.github/workflows/nightly.yml`), each night's 50 full-stack seeds, the reports
its artifact.

A side is a condition's outcome: "never true" is a condition always false, "never false" one always
true, "neither" one never reached. Counts are branch sides, the files' own unit-test modules aside.

| File | Sides | Not taken: local_cell::sim | Not taken: full stack | Not taken: both |
|---|---:|---:|---:|---:|
| local_cell/src/upload.rs | 270 | 96 | 78 | 50 |
| local_cell/src/replicate.rs | 108 | 35 | 42 | 24 |
| local_cell/src/cell.rs | 350 | 170 | 190 | 136 |
| bucket_cell/src/writer.rs | 62 | 18 | 26 | 17 |
| bucket_cell/src/head.rs | 60 | 17 | 28 | 15 |
| cladefs/src/cladefsd/cells.rs | 152 | | 109 | 109 |
| cladefs/src/cladefsd/server.rs | 276 | | 188 | 188 |
| cladefs/src/cladefsd/migration.rs | 100 | | 28 | 28 |
| cladefs/src/cladefsd/peers.rs | 50 | | 17 | 17 |
| cladefs/src/host_cell.rs | 50 | | 32 | 32 |
| cladefs/src/open_cell.rs | 188 | | 119 | 119 |

**The two simulations reach different halves.** local_cell::sim drives the uploader's steps
itself, never its task: `upload`'s loop, the open and close handshake (`start`, `close`, `stop`),
and the waits for replication (`waiting_replicated`) are the full stack's alone, which took
upload.rs's untaken sides from 96 to 50. The full stack, in turn, writes whole 4 KiB extents, one
guest at a time, and runs no takeover racing a checkpoint, which local_cell::sim's aimed steps do.

## What no simulation reaches, and what would

By class, for upload.rs, replicate.rs and cell.rs (the functions named; `branches.md` of each run
lists every side with its line).

**The framing's helper threads and write-behind** (cell.rs `see`, `see_here`, `to_see`, `look`,
`at_once`, `wait_framed`, `wait_idle`, `finish`'s write-behind and write-back, `frame_queued`'s
`wrote`): both simulations' cell directories are serial (`Dir::concurrent()` false), whose framings
run inline or as tasks. Unreachable by design in a simulation; Shuttle's tests
(`crates/local_cell/tests/shuttle`) and production run them.

**A guest's requests out of the ordinary** (cell.rs `check_io`'s misaligned and oversized I/O,
`try_begin_write`'s extent of another cell, `u64::MAX` and the gate held, `check_opens`' spent
extents; `seal`, `seal_own` and `place_of`'s extents of several pieces, large or in place;
`parts_locked`; `paint`'s partial ranges; `commit_version` with extents open; `free` of an extent
not there): the simulated guest writes whole 4 KiB blocks at offset 0 of new extents and reads
them whole. Reached by a guest action of partial, overlapping, large and refused writes and reads
(board 6037); the hostile-guest tests (`crates/cladefs/src/cladefsd/tests/hostile.rs`) reach the
refusals.

**Replication degraded or reconfigured** (upload.rs `waiting_replicated`'s degraded and replacing
waits, `replicate` and `replace` past `SEEDING`, `keep`'s configurations of f 0, `configured` and
`forced_after` with no replication; replicate.rs `next`, `degraded`, `serve` and `turn` with a
member refused, retired, fenced or its log dropped mid-send, `acknowledged` past a drop): no profile
keeps a member gone past `SEEDING` (60 s) nor shrinks a configuration. Reached by a fault profile of
members lost for good and replaced (board 6039); local_cell::sim's `replicated` and `retakes`
profiles reach the protocol's half.

**Bounds no seed gets near** (upload.rs `ask` and `within_lag` for `Awaited::Lag`, `upload`'s
checkpoint every `checkpoint_every` segments, `acknowledge_pending`'s bucket full): the lag bound
(1 GiB), the checkpoint interval and a bucket refusing for space are production sizes and faults
the simulations never reach in five rounds of six blocks. Reached by doc-24's adoption 3, knobs
small under BUGGIFY (reply 5760 on board 2830), and a bucket fault answering full (board
6041).

**The host's disk failing at that step** (cell.rs `file`, `remove`, `remove_files`, `leave`,
`made`, `recover`, `remove_spare`, `create_raw`, `spool`, `seal`'s removal, `remove_raw`;
replicate.rs `restore`'s sync and `stop`'s spool removal; upload.rs `took`): `host_fs`'s faults
fail a call before it changes anything with EIO or ENOSPC, at 0.2% of calls; a missing file
(`NotFound`) after a crash, or an error at one of these calls in particular, is rare in 20 seeds.
Left to local_cell's crash tests (`crates/local_cell/tests/crash`) and the disk-faults
profiles' nightly soak.

**Invariants, argued unreachable** (a `let ... else` that never takes its `else`, a condition
never false): upload.rs `waiting_replicated`'s replicator (asked of replicated cells alone),
`close`/`stop`'s stopped handle (taken once), `upload`'s `while !stop` (a stopped uploader leaves
its loop by `break`), `start`'s `wish.f >= 1` (replicated cells), `drop_before`, `keep`,
`eager`, `version`, `first_read`, `read_longest`; replicate.rs `own_frames` and `trimmed`'s keys,
`drive`'s link, `installed` and `abandon`'s pending; cell.rs `holds_commits`, `make_described`'s
block size, `count_open` and `drop`'s footprint (every simulated directory has a disk),
`acknowledged`, `see_ahead`, `run_framer`, `wait_framed_through`, `seal`'s entry. Each holds by
how its caller calls it; a side reached would be a bug, which a `debug_assert!` there would say
first.

**Rules' other branch** (cell.rs `close`'s `close_waits_for_framings`, `finish_write`'s
`drop_writes_to_freed_extents`, `close_between_commits`' `close_waits_for_writers`; upload.rs
`replicate`'s `installs_own`): the branch a rule off takes, which the searches reach
(local_cell's Shuttle tests, local_cell::sim's and the full stack's `*_needs_*`).
