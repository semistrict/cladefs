Traces of 12 sandboxes' reads, MAPs recorded, notebook and query alternating, for TASK-702.1's comparison of read predictors.

Host: ubu1.

Traces: e2e/fxfs-guest-python-sandbox/fxfs-guest-python-sandbox/traces, MAPs recorded (39b23c424).
`scripts/read_prediction.py` on them: read-prediction-c32.txt and -c64.txt (30 ms a fetch, 32 or
64 at once).

- Every boot of a workload, cold or warm, read the same units in its first 10 s (Jaccard 1.0 over
  the six of each): the notebook 2,021 units, the query 839, all but 3 of them the notebook's.
- History predicts them all: `first`, `union`, `window` and `recent` recall 1.00, 11-12 MiB fetched
  for nothing a boot (the notebook's units a query boot doesn't read). `next` (the 4 extents after
  each read first) recalls 0.89 and fetches 872 MiB for nothing; it adds nothing to history.
- Order matters once one set serves two workloads: by each unit's earliest read in any boot kept,
  the modelled waiting left is 16.5 s over the 11 boots at 32 fetches at once, 7.4 s at 64, against
  84 s and 34 s by mean read time, and 454 s with no prediction; `first`, the first boot's order,
  80 s and 36 s.
- Replayed as six notebook boots then six query boots (a workload that stops), `first` keeps its
  20 MiB of waste and its waiting for good, `union` its waste; `window` (the last K boots) drops
  both within K boots.

Chosen: the units any of the last K recordings read (K = 8), fetched by each unit's earliest read
in any of them; no score threshold, the window's expiry being the decay.
