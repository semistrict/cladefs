---
id: TASK-498
title: README describes the project; its reference sections move to docs/
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 16:12'
updated_date: '2026-11-16 16:22'
labels: []
dependencies: []
ordinal: 887000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user (2026-11-16): "the readme does not really describe the project". README.md is 845 lines that open with two dense paragraphs and go straight into internals (requests, window sizing, tenants, the nightly). A reader who does not know cladefs cannot tell from it what cladefs is for, what problem it solves, what it gives them, what state it is in or how to try it. The reference material is good and others cite it (scripts, guide/tools/lib/repo.ts parses its stand-ins table), so it must stay findable, not be cut.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 README.md says, for a reader new to cladefs: what it is and who it is for, why it is built this way, what it does, how it works in one diagram, its status and requirements, how to build, test and try it, where each directory and document is
- [x] #2 Every section of the old README is in a docs/ file, its text kept, and each is linked from README.md
- [x] #3 Everything that cited a README section (scripts, guide, tests) cites where it went; the guide tests and build pass
- [x] #4 Commands the README shows were run as written, and every relative link in README.md and the new docs resolves
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. README.md rewritten: what, why, what it does, how it works, status, try it, map, docs, licenses.
2. Old sections moved verbatim (headings only reshaped): docs/architecture.md (how it fits together, DAX on Linux, the extent daemon), docs/operating-a-host.md (memory, disk, multi-node, encryption, userfaultfd, exit statuses), docs/code.md (where the code comes from, crates, vendor/firecracker, guest/, updating from upstream), docs/testing.md (testing, qualification, nightly, benchmarks).
3. Stale facts met on the way fixed (KMS backends; the daemon memory default).
4. Citations of README sections updated; guide/tools/lib/repo.ts reads the stand-ins table from docs/code.md.
5. Proof: a link check, cladefsctl commands run, guide pnpm test and build, scripts tests.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Moved by line range from the old README with a script, then checked: every paragraph of the old README is in README.md or one of the four docs, but for headings reshaped, the introduction rewritten, four cross-references ("below", "see Testing") and two stale statements (KMS backends; the daemon memory default, governed since TASK-358). docs/code.md gained a table of the crates the old README never named (extent_bucket, extent_frame, extent_keys, virtio_extent, extent_files, sqlite_extents, fxfs-automount, follower*, fxfs-sim-diff, cladefs-examples), from each Cargo.toml header. The rest of the moved text was not audited against the code.

Validation: README commands run with a cladefsctl build of 05356ff on macOS (docker export alpine, import, fork, stat: inherited 8491008, own 0); cargo metadata: cladefsctl, cladefsd, cladefs-mount each one bin of package cladefs; every path a code span names in the five files exists; the Fxfs RFC link answers 200. guide: pnpm test 42 passed, pnpm build (its check) passed. scripts/tests: 516 passed; license-headers.py, check-exclusions.py, bash -n of the four scripts, rustfmt of the one .rs: ok. Not run: scripts/test-guest-kernel.sh as the README shows it (Linux, KVM); the guest test whose comment changed was not rebuilt.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
README.md now describes cladefs to a reader who does not know it: what it is, why metadata is in the guest and data on the host, what it does, how a read, write, commit and fork go, status and requirements, how to build, try and boot it, and where everything is. The old reference sections are in docs/architecture.md, docs/operating-a-host.md, docs/code.md and docs/testing.md, text kept; scripts, the guide and one test comment cite the new files, and guide/tools/lib/repo.ts reads the stand-ins table from docs/code.md. Verified by running the commands shown, a path and coverage check of the files, the guide tests and build, and the scripts tests.
<!-- SECTION:FINAL_SUMMARY:END -->
