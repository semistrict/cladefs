---
id: TASK-375
title: >-
  A measurement round says when others wrote to its disk: fxjob --bench isolates
  CPU, cache and memory bandwidth, not the disk
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-12 07:45'
updated_date: '2026-11-15 20:47'
labels:
  - testing
  - ubu1
  - perf
dependencies: []
priority: high
ordinal: 757000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-12 by the coordinator, not by the run: TASK-330.4's qualification A/B ran round 1 wholly beside TASK-369's loader (ordinary work, 1 GiB rewritten with fsync in a loop, ~186 MB/s, on the same /mnt/fxdax-scratch NVMe), and nothing in the round said so; its framings and latencies looked like main's own. Every round of a measurement (scripts/qualify.sh, bench.sh through bench-ab-linux-host.sh, bench-host.sh) should record the device's writes and reads over the round (/sys/block/DEV/stat for the device under the round's disk) against its own cgroup's (io.stat), and the rest, others' I/O, in its results; past a bound (say 10 MB/s averaged, or any minute past 50 MB/s) the round is marked contaminated, and bench-ab.py leaves its timing metrics out of the comparison, saying so, as it fails a round a part didn't run in. resources.log samples the run's io.pressure already; it doesn't tell others' I/O from the run's own.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A round run beside a disk loader is marked contaminated in its results and bench-ab.py's table leaves its timings out, saying so
- [x] #2 A round run alone is not
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Refactor first (own commit): scripts/bench-ab.py reads rounds in one place (each round's results, by case), its two tables built on it with the one slower/overlap; scripts/lib/bench-ab-linux-host.sh's round() builds its command once for both suites.
2. scripts/contamination.py, the one module: `contamination.py run RESULTS [DISK...] -- COMMAND` runs a round's command, sampling every minute, and writes RESULTS/contamination.json; importable by bench-ab.py (the record, which units load skews).
   - Disks found from the path through /proc/self/mountinfo and /sys (a root the tests replace): a partition's whole disk, a loop device and its backing file's disk, dm/md slaves, a zvol and its pool (dataset by /dev/zvol, vdevs by zpool list), a ZFS dataset, tmpfs (no device, said).
   - Others' I/O: each device's /sys/block/DEV/stat less the round's cgroup's io.stat for it (the nearest cgroup up from the round's with I/O accounting, only if nothing else runs in it); under a zvol, the pool's other datasets' writes by ZFS's own per-dataset counts (/proc/spl/kstat/zfs/POOL/objset-*), since the cgroup's writes can't be followed through ZFS to the NVMe, whose totals are recorded unjudged.
   - Pressure: the cgroup's cpu/memory/io PSI over the round, the host's where it has none; recorded, judged only if a bound is set.
   - Verdict: contaminated (why: resource, how much, bound), clean, or unchecked (why: no cgroup, cgroup v1, not Linux, shared cgroup, no device found, a ZFS dataset shared with others); never clean where it could not check. Bounds: 10 MB/s of others' writes over the round, 50 MB/s in any minute, settable (CLADEFS_CONTAMINATION_*).
3. Round drivers call it once each: bench-ab-linux-host.sh round(), bench-linux-host.sh, bench-rounds.sh (one line, inside the job).
4. bench-ab.py: a contaminated round's timings (every unit but counts: MiB, GiB, bytes, x, count) left out, said first and in the rows; a side with no clean round for a metric: NO RESULT, said loudly, exit 1; rounds not checked listed.
5. Tests (scripts/tests): accounting and verdicts with exact values, discovery per kind of path from fixture trees (fake_sysfs.py), the wrapper end to end on a fixture root, bench-ab.py's tables with one side contaminated, both, none clean. Whole suite, check-exclusions, license-headers, shellcheck.
6. Not here (no ubu1): the two acceptance runs; commands, expected output and durations in the notes for the parent.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 (claude): built, not yet run on ubu1 (no access in this task: another agent holds it). Acceptance criteria unchecked, task In Progress until the parent runs the check below.

scripts/contamination.py, the one module: `contamination.py run RESULTS [DISK...] -- COMMAND` runs a round, samples every minute, writes RESULTS/contamination.json (verdict clean / contaminated / unchecked, why, others' read and written bytes and MB/s over the round and in its worst minute, by disk and dataset; each disk's totals and the round's own; cpu/memory/io stall ms of the round's cgroup, the host's where it has none; the cgroup taken for its own; the bounds). Disks from the path by /proc/self/mountinfo and /sys: partition to whole disk, loop to its backing file's disk, dm/md to slaves, zvol to its pool (dataset by /dev/zvol, others' writes by /proc/spl/kstat/zfs/POOL/objset-* nwritten of the pool's other datasets, the pool's NVMe by zpool list recorded unjudged), tmpfs said as no device. The round's own I/O: io.stat of its cgroup or the nearest above with one, if nothing else runs there (under fxjob --bench expected to be fxdax-bench.slice). Unchecked, never clean: no cgroup v2 cgroup, root cgroup, shared cgroup, ZFS dataset or overlay as the round's disk, no record.
Bounds: the task's, 10 MB/s of others' writes over the round, 50 in any minute (FXDAX_CONTAMINATION_WRITE_MB_S, _MINUTE_WRITE_MB_S). The data fits: clean rounds on ubu1 showed 0.5-6.3 MB/s of zd0 writes not the bench's (TASK-330.5, TASK-322), capped work 25-27 MB/s (TASK-374), the loader 186. Reads and pressure recorded, judged only with FXDAX_CONTAMINATION_STALL_PCT.
Callers, one call each: scripts/lib/bench-ab-linux-host.sh round() (fs bench and qualify), scripts/lib/bench-linux-host.sh, scripts/bench-rounds.sh (one line; its header comment left alone for TASK-397's merge, to add after). Refactor first, faf6e6a2b.
scripts/bench-ab.py: a contaminated round's results in any unit but MiB, GiB, bytes, x, count are left out, cells say '[contaminated rounds left out: N]'; no clean round on a side: cell and verdict NO RESULT, '## NO RESULT' first, stderr, exit 1; '## CONTAMINATED' and '## NOT CHECKED' sections list rounds by reason.
Tests: scripts/tests/test_contamination.py (48: discovery per kind of path on fake_sysfs.py trees, cgroups, accounting and verdicts with exact values, a round run under it, drivers), test_bench_ab.py Contaminated (7). 18 hand mutants of the module and bench-ab.py: all red (one survived at first, the deepest mount; its test reordered the mounts and now kills it). Suite: python3 -m unittest discover -s scripts/tests, 371 OK before the last two tests; check-exclusions and license-headers pass.

Left for ubu1 (AC #1, #2), one command, ~4 min once the bench slot is free, from a worktree of this branch:
  scripts/contamination-check.sh /mnt/fxdax-scratch/contamination-check-$(date +%s)
Two fxjob --bench jobs of 90 s each (a dd+fsync workload in the job's TMPDIR standing for a round): alone, then beside a loader inside the same job writing <=128 MB/s to $HOME (ZFS). Expected: 'alone: clean: others wrote N MB/s ...', its cgroup a path; 'beside: CONTAMINATED: others wrote ~100+ MB/s to its disk over the round, bound 10 (POOL/home-dataset ...)'; ab.md's pass row 'NO RESULT [contaminated rounds left out: 1] | - | NO RESULT', written row kept; 'contamination-check: all passed'.
Not shown by that check, by the rule that a loader runs inside the round's job: a loader on the scratch disk in another cgroup (zd0's count less the cgroup's), covered by unit tests only. Unverified on ubu1 until it runs: that fxdax-bench.slice has io.stat and the job's scope's ancestors hold nothing else; that the pool's objset kstats and /dev/zvol links are as assumed. If alone says UNCHECKED, its why names which.

2026-11-15, ubu1, main a252cb53f: scripts/contamination-check.sh /mnt/fxdax-scratch/t375check/r1, all passed. Alone: clean, others wrote 2.6 MB/s over 90 s (2.9 worst minute; bounds 10 and 50); its I/O a cgroup's. Beside the loader (in the same bench job, writing to ZFS home): CONTAMINATED, others 91.8 MB/s over the round (rpool/USERDATA/home 89.6, zd0 1.9, var/lib 0.2), 116.1 MB/s in its first minute; bench-ab.py: pass NO RESULT [contaminated rounds left out: 1], written (MiB) kept, exit 1, saying why. The beside job waited 450 s for the bench slot behind another job. DoD #2: scripts/upstream-changes.py --check passes. DoD #3: no upstream code touched. DoD #1 open: the workspace tests on ubu1 under fxjob have not run on this change.
<!-- SECTION:NOTES:END -->
