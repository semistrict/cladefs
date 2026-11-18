---
id: TASK-565
title: One deep entry path makes the importer's memory quadratic in its depth
status: To Do
assignee: []
created_date: '2026-11-18 15:26'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 956000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-facade review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/import.rs`: `rg -n -F 'self.made.insert(prefix.to_vec());' crates/cladefs/src/import.rs`

`walk` makes each missing directory on an entry's path. For every level it stores the whole prefix as a new Vec in `made` (`self.made.insert(prefix.to_vec())`), in `times`, and in `dirs` through `remember_directory` (`self.dirs.entry(path.to_vec())`). An entry path of depth D therefore allocates about 3·D² bytes of keys, plus one held Fxfs node per directory through each `Node` in `dirs`. Entry paths have no length or depth limit: `normalize` only refuses `..`, `valid_name` caps a component at 255 bytes, and tar-rs reads GNU long names and PAX `path` records of any length. A 60 KB path (D≈30,000 components of `a`) needs about 30k Fxfs mkdirs, a few seconds, and about 2.7 GB of key copies in the extent daemon's own heap, which no governor budget covers. D≈50k needs about 7.5 GB, enough to OOM-kill cladefsd and take every tenant's stores down with it. Each level's BTreeMap operations also compare keys up to D·2 bytes long, so the CPU cost is quadratic too. Separately from the stack overflow in remove_lower, this needs no whiteout and no second layer.

**Trigger.** 1) ImportImage of a one-layer tar whose single entry is a PAX header with `path=a/a/a/.../a/f` (30,000 components, about 60 KB) followed by a small regular file. 2) Importer::entry -> self.directory(parent) -> walk(make=true): for i in 1..=D, mkdir, then made.insert, times.insert and dirs.insert, each with a prefix of about 2i bytes. 3) The daemon's resident memory grows by about 3·D² bytes (GBs) within seconds. 4) When the Importer drops, the held-directory chain also unwinds through recursive let_go of depth D in volume.rs.

**Why the tests stay green.** The deepest import test (deep_images_make_stores_on_a_daemons_thread) uses depth 1000, where the keys come to about 3 MB. No test bounds the importer's memory, and none refuses an overlong entry path.

**Backlog.** Related to TASK-92.37 (Done; removal scanned every saved key) and TASK-79.88 (Done; crafted-image faults refused with EINVAL). Neither bounds an entry path's depth or length, or the per-prefix key copies.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An image entry whose path is longer than PATH_MAX (4096 bytes), and so could never be named in one path by the guest, is refused with EINVAL before any directory is made. The store path is left empty.
- [ ] #2 For accepted paths, the importer's memory for path keys grows linearly with depth. Under a counting allocator, an entry 2,000 directories deep allocates no more than about 10 times what an entry 200 deep does (quadratic growth would be 100 times).
- [ ] #3 A test in crates/cladefs/tests/import.rs imports a PAX-path entry 20,000 directories deep and asserts the EINVAL refusal and the empty store path. It also asserts the counting-allocator ratio. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
