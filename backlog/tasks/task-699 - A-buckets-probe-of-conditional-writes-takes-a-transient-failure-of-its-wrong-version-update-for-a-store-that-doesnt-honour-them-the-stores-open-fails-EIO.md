---
id: TASK-699
title: >-
  A bucket's probe of conditional writes takes a transient failure of its
  wrong-version update for a store that doesn't honour them: the store's open
  fails EIO
status: Done
assignee: []
created_date: '2026-11-20 17:46'
updated_date: '2026-11-20 18:14'
labels:
  - bug
dependencies: []
priority: high
ordinal: 1097000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-20 by the madsim boot test with seeded bucket faults (TASK-685.5), seed 3: bucket_cell::object_bucket::probe's update of a version not the object's must fail Precondition; failed Unavailable (the bucket out of reach), the probe answers 'the store does not honour conditional writes: an update of another version gave Err(Unavailable)', and cladefsd's open of the store fails EIO. A transient failure is no answer: the probe should try again (as the bucket's retried requests do) until it gets one, a refusal or a success.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in 247571f06: the probe begins again on Unavailable at any step (a new object, backoff, leftover removed best effort); Error::Unsupported for object_store's NotImplemented/NotSupported, an answer never retried. Test: object_bucket::tests::probes_ride_out_a_bucket_out_of_reach. The madsim boot test with seeded faults: 20/20 seeds.
<!-- SECTION:NOTES:END -->
