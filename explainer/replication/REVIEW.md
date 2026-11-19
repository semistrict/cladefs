# Review (cuddly cut)

## Creative calls
- Built storyline A of `STORYLINES.md` (one commit's journey) without stopping to ask, as briefed; B (the fsync
  clock) was left out because no Replicated latency is measured, C (hold my beads) because it would draw
  members as critters, and only cells are critters in this series.
- One stage for the whole film: four hosts over the bucket at fixed places, so every seam but S2-S3 keeps the
  picture still and only the title changes. S1 is close on host A; S2 pulls back as the bucket comes in.
- The worked example is one commit of cell 9 (the pink bead, the accent): fsync, its copies, host A lost
  before its upload lands, recovered on host D from B, then uploaded and dropped by the members.
- The overview's look, characters, type, word-pops, bed and effects, unchanged; no voice. Film-only helpers
  in `assets/replication.js` (the stage, beads, arcs, the fence).

## Measured
- `hyperframes check`: ok, 0 errors, 0 warnings (lint, runtime, layout, motion, contrast, craft, audio),
  after the fact-check fixes too.
- After the fact-check fixes, looked at 2.4, 4.5, 8, 10.5, 13.5, 15.8, 20.5, 24, 26.6, 27.6, 28.3, 34,
  46.2, 47, 48, 49.4, 51, 61.9, 63.6, 65, 67.5, 68.2 s.
- Looked at: the review set (each scene start and middle, every seam, phone width) and frames at 2.2, 5,
  7.1, 9.2, 15.5, 17, 27, 28.2, 30, 33.9, 38.4, 42.2, 44.4, 46.6, 49.2, 50.4, 56.2, 62.4, 64, 68 s.
- Not rendered, so the mix is neither measured nor heard: the bed and effects are placed as the overview
  places them (bed at 0.75, last chord at 68.0 s, the boing at 67.96 s for the last hop).

## Fixed on review
- Small notes held under their reading time in S3, S5, S6 (`line_unread`): fewer, shorter notes; S5 grew
  from 14 to 16 s, and the film from 68 to 70 s (the bed's last pass now starts 12 s in).
- "2 of 3" was on screen 0.3 s before "3 of 3": the third member now answers 1.5 s later, which also shows
  the fsync not waiting for it.
- A dead stretch in S4's upload: a note that B and C keep their copies until the bucket has it.
- Uploads from host D crossed host C's disk while C was lost, reading as C receiving them: they pass under
  host C now, and the bucket's name and head moved to the pail's left so nothing flies over words.

## Fixed after the fact check
- Wrong premise in S1-S2: fsync never waits for the bucket (layout.rs `enum Fsync`, Ordered by default;
  doc-12). S1's fsync now returns at once ("by default, fsync returns once the daemon has the commit");
  S2's ~130 ms wait is a `sync` ("A sync waits for the bucket"), and the loss is an fsync's at its default
  level ("An fsync doesn't: lost with the host").
- S4 and S7: the copies to B and C leave as the commit is appended, before the writer's own sync, which
  ("on disk: 1 of 3") now comes after they set off (cell.rs `send()`).
- Members drop on the head's acknowledgement, not on the upload's landing: S4's note "B and C keep it until
  the head names it uploaded", S5's "once the head names it uploaded, B and C drop theirs".
- S5's take in the code's order: D sends the recovered tail to B and C (new copies beside the old), waits
  for their acks, then the head names D B C, then D serves, then the old configuration's copies go
  (upload.rs `replicate()`, `drop_before`).
- Replication is a per-cell option: S3's note "an option per cell: cell 9's fsync waits for Replicated";
  S7's note "a cell set to Replicated, f = 1: lose any one host".
- s7's `#s7-n1` hold warning did not recur on re-check (0 warnings), so it was the sampler's blank frames.

## Weakest shot
- S5 (37-53 s): the take carries the most (head at epoch 8, fences, the read from the bucket and the
  members, the install to B and C, the head naming D's members, the old copies dropped, the upload and
  the acknowledged drop) on 30 px notes that do not read at phone width; B and C each holding two copies
  for a moment (old and new configuration) is said by no word. With more time: split the read and the activation into a scene of their own.

## Accuracy
- Shipped, not in progress: TASK-537.3 and its six subtasks are Done; TASK-537 stays To Do only for one
  doc-12 row (a replicated cell's daemon killed and reopened on the same disk), which the film does not show.
- ~130 ms: shown as a sync's wait for the bucket on GCS. Its measurement is TASK-165.11's (r740): a guest
  fdatasync that, at the time, waited for the upload; doc-21's Why and TASK-537 give it as the cost of a
  sync. That task is still In Progress, but the number is a measurement. The counter stretches it
  over 2.4 s.
- Illustrative: epochs 7 and 8, the bead counts, which host takes, and every flight's pace. Recovery on D is
  drawn as the bucket's commits flying up, then the members' tail; really the cell is opened from the
  bucket and only the tail is read from the members (doc-21 Take, steps 1-4).

## Kept on purpose
- Labels and notes at 28-30 px, as in the overview: they read at 1080 but not at 360 px; the titles carry
  the story at phone width.
