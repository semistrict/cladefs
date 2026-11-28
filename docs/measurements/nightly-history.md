# The nightly's history

Each night on the nightly host (scripts/nightly-host.sh, TASK-441), main's head measured against the commit the night before measured, in interleaved rounds of the same night, with the control case and exact counts (scripts/ab-rounds.py). A night main did not move ran nothing and is not here. A metric that moved is a task on the board, labelled tag.nightly. Written by scripts/nightly.py; do not edit.

**The last 10 nights measured nothing** (a task is filed).

| Night (UTC) | Base | Head | Outcome | Moved | Tasks | Results |
|---|---|---|---|---|---|---|
| 2026-11-28 07:00 | b8b99760 | 67c46f32 | could not run: no round ran (the A/B ended 2; see remote.log and driver.log) | - | board 11029 (noted) | [ubu1-nightly-2026-11-28T070002Z-67c46f32](ubu1-nightly-2026-11-28T070002Z-67c46f32/nightly.md) |
| 2026-11-27 07:03 | b8b99760 | 6b31d45e | could not run: no round ran (the A/B ended 1; see remote.log and driver.log) | - | board 11029 (noted) | [ubu1-nightly-2026-11-27T070003Z-6b31d45e](ubu1-nightly-2026-11-27T070003Z-6b31d45e/nightly.md) |
| 2026-11-26 07:03 | b8b99760 | 1d943602 | could not run: no round ran (the A/B ended 1; see remote.log and driver.log) | - | board 11029 (noted) | [ubu1-nightly-2026-11-26T070002Z-1d943602](ubu1-nightly-2026-11-26T070002Z-1d943602/nightly.md) |
| 2026-11-25 07:03 | b8b99760 | 3fe634d5 | could not run: no round ran (the A/B ended 1; see remote.log and driver.log) | - | board 11029 (noted) | [ubu1-nightly-2026-11-25T070003Z-3fe634d5](ubu1-nightly-2026-11-25T070003Z-3fe634d5/nightly.md) |
| 2026-11-24 07:00 | b8b99760 | 6cce1108 | could not run: no round ran (the A/B ended 2; see remote.log and driver.log) | - | board 11029 (filed) | [ubu1-nightly-2026-11-24T070003Z-6cce1108](ubu1-nightly-2026-11-24T070003Z-6cce1108/nightly.md) |
| 2026-11-22 07:00 | b8b99760 | 35932073 | could not run: no round ran (the A/B ended 1; see remote.log and driver.log) | - | - | [ubu1-nightly-2026-11-22T070003Z-35932073](ubu1-nightly-2026-11-22T070003Z-35932073/nightly.md) |
| 2026-11-20 07:03 | b8b99760 | deae90d9 | could not run: head failed to build (see build-b.log and guest-kernel-b.log) | - | - | [ubu1-nightly-2026-11-20T070002Z-deae90d9](ubu1-nightly-2026-11-20T070002Z-deae90d9/nightly.md) |
| 2026-11-18 07:00 | b8b99760 | 92929e6b | could not run: the scratch disk /mnt/fxdax-scratch had 60.2 GiB free before building, the night needs 120.0 GiB (see room.txt; CLADEFS_NIGHTLY_ROOM_BUILD) | - | - | [ubu1-nightly-2026-11-18T070002Z-92929e6b](ubu1-nightly-2026-11-18T070002Z-92929e6b/nightly.md) |
| 2026-11-17 07:00 | b8b99760 | c281092a | could not run: the scratch disk /mnt/fxdax-scratch had 111.6 GiB free before building, the night needs 120.0 GiB (see room.txt; CLADEFS_NIGHTLY_ROOM_BUILD) | - | - | [ubu1-nightly-2026-11-17T070003Z-c281092a](ubu1-nightly-2026-11-17T070003Z-c281092a/nightly.md) |
| 2026-11-16 08:33 | b8b99760 | 7f4923f3 | could not run: no round ran (the A/B ended 2; see remote.log and driver.log) | - | - | [ubu1-nightly-2026-11-16T0700Z-7f4923f3](ubu1-nightly-2026-11-16T0700Z-7f4923f3/nightly.md) |
