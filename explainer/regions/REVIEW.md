# Review (many regions: cells across buckets)

## Creative calls
- Built storyline A, "An ocean away" (STORYLINES.md): one frame that rebuilds, two cloud regions and the
  ocean between them held in place for all seven shots, so every cut changes only what is in the world.
  Not built: B, the registry as a postcard mailed between buckets; C, an endless zoom into a cell ID's 16 bits.
- The worked example is a sandbox in europe-west1 whose disk is one cell, `0002 000005 000009`, at home
  in bucket 2. The hook (S1) and the close (S7) are the same restore: 11 round trips across the ocean,
  then 0.
- A second cell (mint) carries the cross-bucket part: its parent's version is in bucket 1, so its reads
  of inherited parts route to bucket 1 by the parent's number. The sandbox's own cell is never shown
  forked across buckets, so S3's "all 11 trips next door" stays true.
- What has landed (TASK-633.3, .4, .9: numbers, registry, routing, per-bucket credentials, region-bound
  peers and part cache) is shown plainly. What has not is marked on screen: the fork thread in S4 carries
  "forks across buckets: in progress" (TASK-633.7), and S6 (mirrors, TASK-633.8) is titled "In progress:"
  and stamped "in progress" for the whole shot. KMS per bucket (TASK-633.6, 3 of 4 criteria) is left out.
- No voiceover; the overview's titles, critters, pails, bed and sound effects. 72 s: the bed's last chord
  (file 20 s, a last pass from 10 s in at 60 s) lands at 70.0 s on the cell's landing, as the overview does.
- Film-only helpers in `assets/regions.js` (the world, numbered pails, request pills on arcs, stepped
  counters, a key, a sandbox's frown).

## Measured
- `hyperframes check`: ok; 0 errors, 0 warnings, 0 infos (lint, runtime, layout, motion, contrast, craft, audio).
- Looked at: `snapshot --review` (every scene start and middle, every seam, phone width) and frames at
  the trip counters, the registry hop, the credential step, the peer refusal, the marker and the landing.
- Not rendered and not heard: the mix (63 effects on three tracks over the bed at 0.75) is placed by
  time only. Measure loudness on the render.

## Fact check (independent, after the first cut)
No errors; five lines tightened:
- S2 title: "Each root keeps the registry" -> "Every bucket keeps a copy of the registry" (CONTEXT.md Registry).
- S4 note: "found in its own bucket's registry copy" -> "found in this host's bucket's registry copy"
  (the asking host's copy: object_bucket.rs `entry`).
- S4 tag under "With this host's own credentials": `CLADEFSD_BUCKET_1_AWS_REGION=us-east-1` (a region,
  not a credential) -> `CLADEFSD_BUCKET_1_AWS_ACCESS_KEY_ID=…`, which `numbered_options` passes to the S3
  builder as `aws_access_key_id`.
- S5 titles: "A part crosses the ocean once" -> "A part crosses once, not once per host"; "Peers share it
  inside the region" -> "... inside the cloud region" (Region is the DAX term in CONTEXT.md). The note
  drops "once".
- Re-checked: `check` 0 errors, 0 warnings, 0 infos (the S6 note now shows from 5.5 s to hold its read);
  S2, S4, S5 and S6 snapshotted and looked at.

## Weakest shot
- S4 (32-44 s): five things arrive in turn (parent, child, ID tag, registry copy, key and variable, the
  route), and the long `CLADEFSD_BUCKET_1_AWS_ACCESS_KEY_ID=…` tag sits on the bottom row away from the
  pill it powers. With more time: carry the key on the pill itself.
- At phone width the 30 px notes are too small to read, as in the overview; the titles carry each shot.

## Kept on purpose
- 11 round trips for a cold restore: cold copy-open 4 + cold fork 6 + cold fork-open 1, asserted in
  `restores_ask_the_bucket_what_the_protocol_needs` (crates/cladefs/src/bucket/lifecycle.rs).
- "A part crosses once, not once per host": doc-23 says about once per region, and the part cache
  evicts, so the title claims only that hosts do not each fetch it (TASK-633.4 AC 5).
- The cell IDs' block and n digits (5, 9; 3, 4) are illustrative; their 4 + 6 + 6 hex digit shape is the
  code's `bucket << 48 | block << 24 | n`.
- No prices or seconds on screen: doc-23's are estimates.
