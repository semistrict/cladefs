---
id: TASK-597
title: A volume whose base layers fail to open is committed anyway and never opens
status: To Do
assignee: []
created_date: '2026-11-18 15:31'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 988000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-store review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs/src/object_store/volume/base.rs`: `rg -n -F 'transaction.commit().await?;' crates/fxfs/src/object_store/volume/base.rs`

`new_volume_over_base` checks only the manifest's shape before it commits (`check_manifest`: own layers empty, base layers non-empty, unencrypted) and its cells (`check_base_cells`). It then commits the store info and the volume directory entry. The doc-10 checks of each base layer (order, alignment, bounds in `BaseLayer::check`, inside `BaseLayerObject::new`) happen only afterwards, in `ObjectStore::open`. So do the layer's own open (`LayerData::open`, which reads the header, trailer, bloom filter and seek table through the attached base) and the base-layer version check. If any of these fail, for example a misaligned extent, a layer the device can't read yet or any more, or a read error, the error goes back to FXFS_IOC_IMPORT, but the volume is already durable. Re-importing the name fails with AlreadyExists. fxfs-automount mounts an existing name 'with no ask', so it retries the broken volume on every lookup. Every eager open of all volumes also fails: `on_replay_complete` with `lazy_volumes` off, which is how followers mount (`lazy_volumes(!follows)`), and fsck. So one failed import makes every follower mount of that filesystem fail.

**Trigger.** 1) The sandbox manager answers `attach NAME` with a manifest whose generation store has since been deleted (doc-10: a stopped store's generation 'then reads ENOENT'), or the read through to the fork fails, or the manifest has a misaligned base extent. 2) FXFS_IOC_IMPORT runs `new_volume_over_base`: `check_manifest` and `check_base_cells` pass, line 102 commits, and `ObjectStore::open` fails (`BaseLayer::check` → Inconsistent, or `LayerData::open` → read error). 3) The ioctl returns an error, but the volume 'NAME' is in the volume directory. 4) A follower of this device mounts: `on_replay_complete` → `open_store(NAME)` fails → the mount fails ('Failed to open store'). The writer's automount retries the same broken volume and cannot re-import.

**Why the tests stay green.** `test_base_manifests_refused` ('Manifests that can't be honoured leave no volume') only covers failures before the commit: deserialize errors, an empty manifest, the manifest version, own or held cells. `test_base_layers_of_older_versions_refused` hits a failure after the commit but only asserts the error kind. It never looks the name up or remounts eagerly. `layers_that_make_no_sense_are_refused` calls `BaseLayerObject::new` directly, not through `new_volume_over_base`.

**Backlog.** Related to TASK-136.3 (Done; volumes over bases) and TASK-136.6 (Done; automount refusals), whose promise that manifests that can't be honoured leave no volume is tested only for failures before the commit.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When new_volume_over_base fails for any reason (a misaligned base extent, a layer the device cannot read, a layer of another version), the volume directory has no entry of that name, and importing the same name again can succeed.
- [ ] #2 After such a failure and a close, a non-lazy FxFilesystem::open (the way followers mount) mounts, and fsck passes.
- [ ] #3 Tests in crates/fxfs/src/object_store/volume/base.rs import (a) a manifest with an unaligned extent_offset and (b) a valid manifest on a device with no attach(). test_base_layers_of_older_versions_refused is extended the same way. Each test asserts the name lookup is empty and the eager remount succeeds, and each goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
