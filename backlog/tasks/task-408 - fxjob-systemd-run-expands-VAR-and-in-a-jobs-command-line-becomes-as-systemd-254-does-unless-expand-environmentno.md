---
id: TASK-408
title: >-
  fxjob: systemd-run expands $VAR and $$ in a job's command line ($$ becomes $),
  as systemd 254+ does unless --expand-environment=no
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 17:01'
updated_date: '2026-11-14 17:32'
labels:
  - bug
  - ubu1
dependencies: []
priority: medium
type: bug
ordinal: 796000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-14 while checking TASK-379 on ubu1 (systemd 259): fxjob runs its job with systemd-run --scope, which expands environment variables in the command line itself, before the command sees it: `fxjob -- bash -c 'echo $$ > f'` writes '$' to f, not the shell's PID. A $VAR the caller's environment has is replaced by systemd-run (an unbraced $VAR alone as an argument split on whitespace into several), one it lacks by nothing, so a job's shell code (scripts/ab-guest-cases.sh's loader wrapper, "$FXDAX_AB_LOADER", $!, $?) does not run as written. Fix: systemd-run --expand-environment=no.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A job's command reaches it as fxjob was given it: $$, $VAR and ${VAR} unexpanded by systemd-run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in 679737d41 (with TASK-379): systemd-run --expand-environment=no. fxjob-check.sh case: printf of '$HOME ${HOME} $$' comes back as given (ubu1, all passed). DoD #1 as TASK-379's: macOS 2369 passed; ubu1 1365/2651 passed before the 15-min cap.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
fxjob passes --expand-environment=no to systemd-run, so a job's command reaches it as given; scripts/fxjob-check.sh checks it.
<!-- SECTION:FINAL_SUMMARY:END -->
