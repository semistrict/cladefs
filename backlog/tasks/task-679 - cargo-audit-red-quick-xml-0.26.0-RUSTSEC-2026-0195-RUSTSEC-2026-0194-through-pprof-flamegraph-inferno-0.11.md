---
id: TASK-679
title: >-
  cargo audit red: quick-xml 0.26.0 (RUSTSEC-2026-0195, RUSTSEC-2026-0194)
  through pprof flamegraph (inferno 0.11)
status: To Do
assignee: []
created_date: '2026-11-20 00:56'
labels: []
dependencies: []
references:
  - RUSTSEC-2026-0195
  - RUSTSEC-2026-0194
priority: low
ordinal: 1061000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
cargo audit fails with two vulnerabilities in quick-xml 0.26.0, pulled in by pprof 0.15's `flamegraph` feature (inferno 0.11.21), added for /z/pprof (TASK-672.3). RUSTSEC-2026-0195 (7.5 high): unbounded namespace-declaration allocation in `NsReader`; RUSTSEC-2026-0194: quadratic duplicate-attribute check. Both are on the XML-reading side; inferno only writes SVG with `quick_xml::Writer`, so likely unreachable from cladefs, but the audit stays red and hides new findings. Fix needs quick-xml >= 0.41 (a newer inferno/pprof, or dropping the flamegraph feature). Found while checking bincode's RUSTSEC-2025-0141 (unmaintained, informational).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `cargo audit` reports no vulnerabilities (warnings for unmaintained crates allowed)
- [ ] #2 /z/pprof still serves CPU profiles in each format it serves today, its tests green
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
