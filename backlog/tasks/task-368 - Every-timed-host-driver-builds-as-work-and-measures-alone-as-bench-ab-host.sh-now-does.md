---
id: TASK-368
title: >-
  Every timed host driver builds as work and measures alone, as bench-ab-host.sh
  now does
status: Done
assignee:
  - '@claude'
created_date: '2026-11-11 22:22'
updated_date: '2026-11-16 00:43'
labels:
  - scripts
  - ubu1
  - perf
dependencies: []
priority: medium
ordinal: 752000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-330.4 split bench-ab-host.sh into a build job (fxjob, the work slice) and a rounds job (fxjob --bench), two jobs of one run (scripts/lib/host.sh: host_bench=0 host_job). The other timed drivers still compile inside their measurement: bench-host.sh (bench-linux-host.sh builds through scripts/bench.sh's test-e2e.sh), perf-gate-host.sh's test-host.sh runs, fork-time-host.sh, gets-host.sh, soak-host.sh. Each such build holds fxjob --bench's cores 4-7 and up to 16 GiB away from every other measurement for as long as it compiles (an hour on 4 cores for two cladefs trees). Give each a build phase (test-e2e.sh CLADEFS_E2E_BUILD_ONLY into the run's work directory) run as work, then its measurement against the prebuilt artifacts.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 No host driver's fxjob --bench job runs cargo or a kernel build
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/lib/measurement.sh: measuring (this process's cgroup, /proc/self/cgroup, in fxjob --bench's fxdax-bench.slice; survives sudo) and measurement_refuses WHAT (exit 2, saying why). The guard, called where a build would start: test-e2e.sh without CLADEFS_E2E_PREBUILT (and, with it, its artifacts complete), scripts/test.sh, host_kernel_build, linux-host-setup.sh's host_cargo, gc-sweep.sh's cargo.
2. host.sh: host_job runs a measurement (host_bench=1) as two jobs of its run: CLADEFS_HOST_PHASE=build as work (no --bench, CLADEFS_HOST_NICE, 12G, RESULTS/remote-build.log), then CLADEFS_HOST_PHASE=measure under fxjob --bench; other runs one job, no phase.
3. linux-host-setup.sh: host_phase (all, build, measure) checked by host_setup; host_artifacts DIR NAME [KERNEL] (test-e2e.sh's CLADEFS_E2E_BUILD_ONLY, the kernel as DIR/vmlinux) and host_prebuilt DIR; host_build_nice gone (the build job's niceness is host_command's).
4. Host scripts by phase: bench-linux-host.sh, test-linux-host.sh (perf-gate-host.sh, soak-host.sh with CLADEFS_HOST_BENCH=1; workspace tests in the build phase), fork-time-linux-host.sh (builds and its image in the build phase), gets-linux-host.sh, gc-sweep-linux-host.sh via gc-sweep.sh --prepare/--prepared, bench-ab-linux-host.sh (CLADEFS_AB_PHASE and CLADEFS_AB_BUILD_NICE replaced by the shared phase; bench-ab-host.sh one host_job).
5. bench-rounds.sh: TASK-446, CARGO_BUILD_JOBS (default 4) and CARGO_TARGET_DIR through sudo; made runnable under fakes (CLADEFS_HOST_PROC, a cgroup root) so its jobs are tested.
6. Tests (scripts/tests): fake fxjob places its job in a work or bench cgroup (CLADEFS_HOST_PROC); fake host scripts build unless measuring, through the real guard; each driver's jobs are build-as-work then measure-in-bench; the guard refusing (test-e2e.sh, incomplete prebuilt, test.sh, host_kernel, host_cargo, gc-sweep.sh); bench-rounds' build jobs and environment.
7. Verify: scripts suite in full on macOS, shellcheck, test.sh's scripts part; on ubu1 bench-rounds.sh one round of bench-fs-fxfs-guest-dax (sync, 1 iteration) under the split, fxjob history lines.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 (claude): built, commit 3127f204d on branch t368. Each driver's fxjob --bench job, checked: bench-host.sh (bench-linux-host.sh's test-e2e.sh built inside it, and host_kernel), perf-gate-host.sh and soak-host.sh (CLADEFS_HOST_BENCH=1) through test-linux-host.sh (test-e2e.sh's build, host_kernel), fork-time-linux-host.sh (cargo per tree, and its 10 GiB image), gets-linux-host.sh (cargo), gc-sweep.sh (cargo, and its buckets' fill) built inside the bench; bench-ab-host.sh and bench-rounds.sh built as work already; qualify.sh runs only under bench-ab's rounds, prebuilt (RustFS's binary fetched, not built); migrate-host.sh and soak-host.sh by default are no measurement. Now host_job splits every measurement (CLADEFS_HOST_PHASE build as work, measure under --bench); preparation (fork-time's image, gc-sweep's fill) is in the build job too. Guard: scripts/lib/measurement.sh by the process's cgroup (fxdax-bench.slice), so it holds for drivers that call fxjob --bench directly (bench-rounds.sh, ab-guest-cases.sh) and through sudo. Test-only knobs: CLADEFS_SELF_CGROUP (the guard), CLADEFS_HOST_PROC/CLADEFS_HOST_SYS (bench-rounds.sh). Behaviour changes: a measurement run of test-linux-host.sh no longer runs the harness's own tests (test.sh guest/firecracker), and its workspace tests, if any, run in the build job (a failure there measures nothing); bench-linux-host.sh needs no nextest. TASK-446 fixed here (bench-rounds.sh passes CARGO_BUILD_JOBS, default 4, and CARGO_TARGET_DIR through sudo). macOS: scripts suite 442 OK; scripts/test.sh's scripts part exits 1 only on crates/cladefs/mutants/written-back-twice.patch not applying (TASK-461, someone else's, open); shellcheck -x clean on what changed (only pre-existing info notes). Hand mutants each red: host_job as one job (10 failures), guard never refusing (6), bench-rounds without cargo_env (2), test-e2e.sh without the completeness check (2).

2026-11-15 (claude): rebased onto main d39f504fa (commit d82edf385, branch t368); the first ubu1 try built the base's guest tests, which failed to compile (test_cases' HostDir::new(from, false), fixed on main since). macOS after the rebase: scripts suite 446 OK. ubu1 (no cached guest kernel had these sources' key, cbe44d4f; one built as work, fxjob t368-kernel, 4 jobs, nice 19): scripts/bench-rounds.sh ~/src/cladefs-t368-results 1 bench-fs-fxfs-guest-dax, CLADEFS_BENCH_GROUPS=sync CLADEFS_BENCH_ITERATIONS=1 CLADEFS_REMOTE_LATENCY_MS=25, CARGO_TARGET_DIR in the home, its drop_caches pointed at a file of its own (CLADEFS_HOST_PROC, a /proc of symlinks: the host's caches left alone, the one difference). fxjob's history: rounds-build bench=false mem 8G cpus 4, queued 994 s, wall 529.6 s, cpu 1,425 s, peak 5.45 GB, status 0; rounds-bench-fs-fxfs-guest-dax-1 bench=true mem 16G cpus 8, queued 1,071 s, wall 24.6 s, cpu 4.5 s, peak 1.44 GB, status 0: the build held no measurement cores, and the measurement compiled nothing (its 4.5 s of CPU the guest's run; test-e2e.sh's guard let its complete artifacts through). The round ok, contamination clean (others 2.3 MB/s). ubu1 left as found: worktree, target, kernel, results removed. scripts/upstream-changes.py --check passes.

2026-11-15: Linux, ubu1 under fxjob (CARGO_BUILD_JOBS=4, --test-threads 4), main 21f042594: scripts/test.sh parts workspace and go, 2817 tests passed, 83 skipped; go/extentd/v1 ok (clippy passed in the run before, on 618403778). macOS: the crates this change touched and the scripts suite (see notes above). DoD #1 checked on that evidence.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
host.sh's host_job runs every timed driver as a build job (work) then a measurement job (fxjob --bench) on prebuilt artifacts; scripts/lib/measurement.sh refuses cargo, kernel builds and unprebuilt test-e2e.sh inside a bench job. Verified: scripts suite with fake fxjob cgroups (each driver build-work then measure-bench; 4 hand mutants red); ubu1 bench-rounds.sh run (build 530 s as work, bench job 4.5 s CPU); Linux workspace 2817 passed.
<!-- SECTION:FINAL_SUMMARY:END -->
