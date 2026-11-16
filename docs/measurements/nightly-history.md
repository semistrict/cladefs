# The nightly's history

Each night on ubu1 (scripts/nightly-host.sh, TASK-441), main's head measured against the commit the night before measured, in interleaved rounds of the same night, with the control case and exact counts (scripts/ab-rounds.py). A night main did not move ran nothing and is not here. A metric that moved is a backlog task, labelled nightly. Written by scripts/nightly.py; do not edit.

**The last night measured nothing**: a task is filed at 3.

| Night (UTC) | Base | Head | Outcome | Moved | Tasks | Results |
|---|---|---|---|---|---|---|
| 2026-11-16 08:33 | b8b99760 | 7f4923f3 | could not run: no round ran (the A/B ended 2; see remote.log and driver.log) | - | - | [ubu1-nightly-2026-11-16T0700Z-7f4923f3](ubu1-nightly-2026-11-16T0700Z-7f4923f3/nightly.md) |
