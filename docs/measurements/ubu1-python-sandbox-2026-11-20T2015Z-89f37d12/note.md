Traces of 12 sandboxes' reads, notebook and query alternating, for TASK-702.1's comparison of read predictors.

Host: ubu1.

Traces: e2e/fxfs-guest-python-sandbox/fxfs-guest-python-sandbox/traces (7c006e3b9). They record
READs and faults alone: a warm host's VMs fault on none of the pages their MAPs install, so boots
4 and 6-12 recorded 0-11 faults and boot 5 part of its reads. MAPs are recorded since 7714fae6c;
the run after this one has complete traces of warm boots. Of the cold boots: the two notebook
boots read the same 1,999 extents (Jaccard 1.0); the query's 816 are all but 2 among them.
