---
id: TASK-541
title: >-
  A whiteout over a deep kept directory chain overflows the extent daemon's
  stack
status: To Do
assignee: []
created_date: '2026-11-18 15:23'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 932000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-facade review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/import.rs`: `rg -n -F 'Box::pin(self.remove_lower(&join(path, &name))).await?;' crates/cladefs/src/import.rs`

TASK-79.56 fixed a crafted image's deep whiteout overflowing the daemon's stack by making `remove` walk an explicit heap stack. TASK-79.98 then added `remove_lower` for whiteouts and opaque whiteouts over directories the current layer keeps. `remove_lower` recurses with `Box::pin(self.remove_lower(..)).await` for each entry of a kept directory. Boxing puts the future on the heap, but polling still nests one poll frame per level, so stack use grows with the depth of the kept chain. `walk(.., make = true)` marks every existing prefix of an entry's parent as kept (`self.kept.insert(prefix.to_vec(), None)`), so a single entry deep inside a lower layer's tree makes the whole chain kept. Overflowing a tokio blocking thread's 2 MiB stack aborts the whole cladefsd process, so every tenant's stores and VMs on the host lose their daemon and up to the crash bound of ordered commits. Look-around: dropping the Importer at the end also lets go of a deep chain of held directories through `Mounted::let_go` -> `release_child` -> `let_go` recursion in volume.rs, which also grows with depth (it is plain function recursion, so release builds may optimise it).

**Trigger.** 1) The VM manager calls ImportImage (cladefsd cells::import -> blocking -> import::store_keeping) on an OCI layout of two layers. 2) Layer 1 holds `d/d/.../d/f` (D levels, e.g. D=1000, a 2 KB path; PAX path records allow far more). 3) Layer 2 holds the entry `d/d/.../d/new`, then `.wh.d`. 4) Applying `new` walks its parent: every prefix is remembered in `dirs` from layer 1, so every prefix goes into `kept`. 5) `.wh.d` -> remove_lower("d") -> kept -> entries -> remove_lower("d/d") -> ... D nested polls on the spawn_blocking thread (2 MiB). 6) The stack overflows and Rust aborts cladefsd. The same happens with `d/.wh..wh..opq` through `opaque`.

**Why the tests stay green.** deep_images_make_stores_on_a_daemons_thread (tests/import.rs) whites out a deep tree that the top layer does not keep, so it goes through the iterative `remove`. No test whites out a deep directory chain the same layer writes into, which is the only path into `remove_lower`'s recursion.

**Backlog.** Regresses TASK-79.56 (Done), which promised that whiteout removal over deep paths no longer overflows the stack (removal from a heap stack of its own). TASK-79.98 (Done) brought the recursion back in remove_lower.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Importing a two-layer image whose top layer adds `{deep}/new` under a 1000-deep directory chain from the lower layer and then whites out `d`, on a 2 MiB thread, makes the store without aborting the extent daemon: the root holds `d` and `kept`, `{deep}/new` reads `new`, and `{deep}/file` is ENOENT. The same holds for `d/.wh..wh..opq` in place of `.wh.d`.
- [ ] #2 Stack use of whiteout and opaque-whiteout removal does not grow with the depth of the kept chain: the same import at 10,000 levels also finishes on a 2 MiB thread.
- [ ] #3 deep_images_make_stores_on_a_daemons_thread in crates/cladefs/tests/import.rs covers the whiteout and the opaque-whiteout case over a kept chain, and it aborts with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
