---
id: TASK-665
title: >-
  A data-analysis sandbox timed from fork to answers: uv-installed pandas and
  marimo, uncompiled, on three hosts sharing one bucket
status: Done
assignee:
  - '@claude'
created_date: '2026-11-19 17:28'
updated_date: '2026-11-19 19:14'
labels:
  - testing
  - sandbox
dependencies: []
priority: high
ordinal: 1044000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Our sandbox runs (fxfs-guest-sandboxes) time builds, pgbench and git, not what an agent sandbox does first: start from a base image of a Python project and answer questions about data. The user asked (2026-11-19) for that workload, timed: a base with Python and a project whose dependencies uv installed (pandas, pyarrow, marimo) plus a dataset of a few MB, with no bytecode compiled into it, so every sandbox compiles what it imports into its own fork; a sandbox is a fork of the base's version, cold-booted, which starts marimo and runs a notebook that loads the dataset with pandas and answers questions. The topology is realistic but single-region: three hosts, each its own extent daemon with a part cache, fetching parts from each other (TASK-406.2), one S3 bucket (RustFS) with 25 ms of latency in front of it (faulty_s3), no other buckets or regions.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A guest case builds the base image: Python, a uv project (pyproject.toml and uv.lock in the repo) with pandas, pyarrow and marimo installed by uv sync, and NYC TLC green-taxi trips for 2025 (Parquet, about 14 MB) with the zone lookup, each download pinned by sha256; the guest checks the image holds no .pyc file before the notebook runs
- [x] #2 Three extent daemons, each with a part cache and the other two as peers, share one S3 bucket behind a proxy adding 25 ms a request; the base is imported on the first, versioned and synced, and every sandbox is a fork of that version on its host
- [x] #3 Sandboxes run cold then warm on each host (A, B, C, then again), each timed by phase: fork, VM start, guest boot to its agent, marimo serving HTTP, and the notebook's answers; plus each host's bucket GETs and parts fetched from and served to peers
- [x] #4 The notebook's answers are checked exactly against values computed from the dataset outside the VM
- [x] #5 Run on ubu1 under fxjob --bench; the numbers recorded in docs/measurements
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. guest/tests/test_cases/python-sandbox: pyproject.toml (marimo, pandas, pyarrow pinned), uv.lock (uv 0.13.9, py3.14), analysis.py (marimo notebook; answers to ANSWERS JSON). Answers computed on the Mac with the same lock and files.
2. rootfs::Image gains files of the case's own (ContextFile), hashed into the image tag.
3. Case fxfs-guest-python-sandbox (Manual): python:3.14.10-slim-trixie + uv 0.13.9, both pinned by digest; uv sync --frozen --no-install-project; 13 downloads by sha256; every .pyc deleted, checked on the host before import. Three daemons (2 GiB, part cache, peers of each other), FaultyS3 at 25 ms over RustFS; base imported on host-a, versioned, synced; rounds x hosts: fork_version, VM (2 vCPU, 2 GiB, root DAX 1 GiB), guest brings lo up, marimo run until /health 200, marimo export html for the answers; host watches the console for phase marks; per-sandbox bucket GETs and peer parts from the stats files.
4. scripts/python-sandbox-host.sh + scripts/lib/python-sandbox-linux-host.sh: build phase as work, measure under fxjob --bench, RustFS on the work disk.
5. Verify: harness unit tests on ubu1; the measurement on ubu1; numbers into docs/measurements.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Harness tests (host half) pass on ubu1: test_fxfs_guest_python_sandbox's two and rootfs' three. First measurement run launched (scripts/python-sandbox-host.sh).

Run 1 (ubu1): marimo's export failed in every guest, 'localhost' unresolved (the image's /etc/hosts is a container runtime's to write); the guest now names localhost as it brings lo up. Run 2 (19fde1f7c, fxjob --bench): all six sandboxes answered exactly ANSWERS. Warm about 6.0 s fork to answers (fork 0.3 s, boot 0.24 s, marimo serving 2.9 s, notebook 3.1 s, 1,640 .pyc compiled each); cold host with cold peers 13.4 s (346 bucket GETs, 103 MiB); cold host with warm peers 10.2 s (14 GETs, 271 parts from peers). docs/measurements/ubu1-python-sandbox-2026-11-19T1842Z-7786f0b5. The report's fetched_mib (daemon fetched_bytes) read 0 because page-ins read through the part cache, so it was replaced by bucket_mib with one decimal; harness tests were rerun on ubu1 after that. DoD #1: the change is the guest harness alone (test_cases, scripts), not the workspace. test_cases' host tests on ubu1 pass (5/5 of python_sandbox and rootfs), as do scripts/tests (528) on macOS, upstream-changes --check, license headers and check-exclusions. The full workspace suite was not run because nothing in it changed.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
New guest case fxfs-guest-python-sandbox (Manual) and scripts/python-sandbox-host.sh. A base of Python 3.14 + uv 0.13.9 with a uv-synced project (marimo, pandas, pyarrow; pyproject.toml and uv.lock in guest/tests/test_cases/python-sandbox) and 14 MB of 2025 NYC green-taxi Parquet, all pinned by digest or sha256, with no bytecode (checked). Three daemons with part caches and each other as peers share one RustFS bucket behind a 25 ms proxy. Sandboxes fork the base's version on each host, cold then warm, and boot, start marimo and run a notebook whose answers are checked exactly; each is timed by phase on the host's clock and costed in bucket GETs and MiB and peer parts. rootfs::Image gained files of the case's own. First run on ubu1: warm 6.0 s fork to answers, cold 13.4 s with cold peers and 10.2 s with warm peers (docs/measurements/ubu1-python-sandbox-2026-11-19T1842Z-7786f0b5).
<!-- SECTION:FINAL_SUMMARY:END -->
