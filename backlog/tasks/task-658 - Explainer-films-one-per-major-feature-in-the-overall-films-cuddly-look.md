---
id: TASK-658
title: 'Explainer films, one per major feature, in the overall film''s cuddly look'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-19 15:28'
updated_date: '2026-11-19 15:29'
labels:
  - explainer
dependencies: []
type: docs
ordinal: 1038000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user (2026-11-19) wants an explainer video for each major feature, like the 132 s overall film in `explainer/` (HyperFrames, fuzzy critters on lilac felt, ukulele bed, no voice). They picked: encryption and shredding, replicated commits (durable without the bucket), multi-region (cells across buckets), shared volumes, container images as bases; 45-75 s each. The films share one critter library, so the series lives in one tree with one shared library rather than five copies.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `explainer/` holds one HyperFrames project per film, the overall film among them, all drawing on one shared critter library
- [ ] #2 Each film passes `hyperframes check` with no errors, and has SPEC.md and REVIEW.md
- [ ] #3 Every claim on screen traces to the code or a design doc, and nothing still in progress is shown as shipped
- [ ] #4 Each film is rendered to MP4 (renders stay out of git)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Restructure explainer/: the overall film moves to explainer/overview/; the critter library, logos and (untracked) audio move to explainer/shared/, which every film links as its assets. Check the overall film still passes check and its snapshots match.
2. One delegate per film (TASK-658.1-.5) writes STORYLINES.md and SPEC.md from the docs, builds the scenes in the cuddly look, passes check, reviews snapshots, writes REVIEW.md. Delegates add helpers to their own film script, never to the shared library.
3. Review each film myself against its sources; promote helpers two films share into the shared library; render each; publish the renders.
<!-- SECTION:PLAN:END -->
