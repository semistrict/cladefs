# Guest kernel patch series

Patches applied on top of the pinned Amazon Linux 6.18 kernel release
(`scripts/lib/guest-kernel-release.sh`) when building the cladefs guest kernel.

The apply order is duplicated in two places and checked against this table by
`scripts/tests/test_build_guest_kernel.py`:

- `scripts/build-guest-kernel.sh` (native / hotbed kernel builds)
- `tools/bazel/guest_kernel.bzl` (Bazel kernel target)
- `guest/sim/kernel-tree.sh` additionally applies the `fs/dax.c` patches
  (`0010`, `0011`) to the simulator's sparse kernel tree.

## Origins

- **`project`** — cladefs-specific kernel subsystems (`virtio_extent`, `fs/cladefs`).
- **`cladefs-generic`** — generic Linux kernel fixes written by cladefs; not yet upstream.
- **`upstream`** — backports of commits already in mainline Linux (and/or 6.18 stable), carried only until the pinned Amazon Linux release includes them.

## Series

| Patch | Origin | Upstream / Tracking | Summary |
|---|---|---|---|
| `0003-virtio-extent.patch` | `project` | doc-4 | Wires in `drivers/virtio_extent` and `include/linux/virtio_extent.h` (cladefs extent-store virtio device). |
| `0004-cladefs.patch` | `project` | doc-2 | Wires in `fs/cladefs` (guest Fxfs VFS/DAX glue linking `libcladefs_linux.a`). |
| `0010-dax-read-replaces-no-entry-naming-its-page.patch` | `cladefs-generic` | TASK-152 | Leaves shared DAX entries and other mappings intact on reads of the same page instead of unmapping and replacing. |
| `0011-dax-a-folio-put-for-good-keeps-no-share-count.patch` | `cladefs-generic` | TASK-152; upstream 7.1 `59eb73b98ae0` / `3fc3ebf40b5c` (not in 6.18) | Clears `folio->share = 0` before the order-0 early return in `dax_folio_put` (functionally identical to upstream `dax_folio_reset_order()` without the 7.1 `fsdev` refactor). Drop once base kernel >= 7.1 or backported to 6.18. |
| `0014-fs-rotate-inodes-whose-page-cache-could-not-be-dropped.patch` | `upstream` | mainline `8ee09f597763`, 6.18.64 `c61b2125a21e` (board 13582) | Rotates inodes whose page cache could not be dropped during `inode_lru_isolate` (rebased onto AL 6.18.62). Drop once AL 6.18 >= 6.18.64. |
| `0015-fs-dont-lose-inodes-another-lru-walker-is-isolating.patch` | `upstream` | mainline `8dc4116041e2`, 6.18.64 `dd6dafb38185` (board 13582) | Skips inodes with `I_LRU_ISOLATING` during LRU walks instead of removing them from the LRU. Drop once AL 6.18 >= 6.18.64. |

## Maintaining this series

When adding, removing, or reordering a patch:

1. Update the `commit` list in `scripts/build-guest-kernel.sh`.
2. Update the patch loop in `tools/bazel/guest_kernel.bzl`.
3. Update the **Series** table above (including the upstream commit and drop condition for `upstream` backports).
4. Run `python3 -m unittest discover -s scripts/tests -p test_build_guest_kernel.py` to verify all lists match.
