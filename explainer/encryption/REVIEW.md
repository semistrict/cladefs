# Review (encryption at rest and shredding)

## Creative calls
- Storyline A of `STORYLINES.md`, "One key at the top": one file, tenant 42's `main.rs` in cell 9 (the
  overview's peach critter), followed to every place it rests, then the chain of keys above it, then
  the shred. Not built: B, "Where is main.rs?" (a flight; the keys come too late to carry the shred),
  and C, "Russian dolls" (nesting says "wrapped" but hides where the copies rest).
- The overview's look, characters, type and sound, unchanged; this film adds file cards that flip
  between code and hex, and padlocks colour-matched to the key that opens them (DEK butter, cell KEK
  sky, tenant 42's KEK accent). The accent is only on tenant 42's KEK and its padlock.
- No voiceover. Effects on visible actions: pops, a riser into the KEK's break, poofs on the shred,
  chimes on "every lock" and "tenant 7 reads on", a boing on the landing at the last chord (60.0 s).
- Left out as not shipped: Azure Key Vault and a KMS per bucket (TASK-633.6 In Progress). Not
  claimed: that a shred wipes memory at once (TASK-630, To Do: page-ins in flight can leave plaintext
  pages until the VM process drops its window); the film says "at rest" and shows memory as plain.
- Peers appear "if replicated": replicated commits are implemented (TASK-537.3.x Done; TASK-537
  itself open only for doc-12's rows).
- Tenant 42, tenant 7, `main.rs`, `app.py` and the hex are illustrative. The shred command shows the
  documented `TENANT` placeholder, since cladefsctl takes tenant IDs in base 32.

## Fact check (independent, each finding verified again against the code before changing)
- S5 shred commands: the bucket KMS shreds with `cladefsctl shred-tenant TENANT`; a KMS by URL needs
  its KEK destroyed there, then `forget-tenant-keys TENANT`. Both shown, held to the scene's end.
- Overstated reach: forks into other tenants' cells keep what they inherited, and under the bucket
  KMS a versioned bucket keeps old KEK versions until expired. Lines now "Its cells' copies stay
  ciphertext", "its cell KEKs unwrap no more, once old versions expire", "One key gone, its cells
  unreadable", and S6 adds "except forks into other tenants' cells; under the bucket KMS, expire old
  versions".
- S2 cipher note: framed parts are GCM, or (framed in place) the XTS sectors kept and tagged.
- S4: OpenBao, AWS KMS and Google Cloud KMS now sit under one "in progress" tag (TASK-411.16/.17/.18);
  the bucket KMS, done and the default, below them. SPEC.md says what each was tested against.
- S4 terminal: a bare `cladefsd` fails on `CLADEFSD_BUCKET` first; the command now sets the bucket
  and token, so the KMS refusal is what it prints.
- S5 "Every host told forgets them" (hosts forget only once told; a halted store keeps its own keys
  until closed).
- CONTEXT.md words: "bucket", not "bucket (S3)"; S1 "Its disk is kept in cell 9".

## Measured
- `hyperframes check`: ok; 0 errors, 0 warnings (lint, runtime, layout, motion, contrast, craft,
  audio). Four layout infos: copies overlapping labels while they travel in S2 (under 0.7 s each).
- After the fact check: check again 0 errors, 0 warnings; frames of every changed scene looked at
  (5, 17.5, 34, 37.5, 46, 50.5, 53, 61.9 s) and S4's KMS box zoomed.
- Looked at: the review set (every scene start and middle, the five seams, phone width) and about 40
  more frames across every scene, zoomed where something looked off.
- The sound was placed by time, not heard, and not measured: no render was made (the parent renders
  after review). Same bed and gain (0.75) as the overview, so loudness should match its -16 LUFS.

## Fixed in review
- Cards shrank to 17 px type while travelling (text floor): copies now travel at full size.
- Notes under their reading floor: shortened (S2 cipher note, S3 "taking cell 9 asks the KMS once",
  S5 "no cell KEK of tenant 42 unwraps again", S4 "there is no plaintext mode") or held longer (S1).
- S4's KMS names each held 1.2 s; cladefsd's error cut to its first two lines.
- S5: round line caps drew dots where the hidden crosses started; crosses now hidden by opacity.
- S6: a 3 s dead stretch before the landing; tenant 7's critter now hops into view at 58.3 s first.
- S1, S3: layout moved down and enlarged to fill the frame's middle; "cell 9" tag moved under the
  critter so the file's hop clears it.

## Weakest shot
- S5 (40-54 s) carries the most: a KMS with two KEKs, five cells, three places with six cards, and
  now two command notes. Each beat is one move, but on a phone the mini cards' hex reads only as
  texture. With more time: drop the peers' row to two places, or enlarge the cards.

## Kept on purpose
- Snapshots sometimes come back blank for one sampled time (seen at 22.0, 36.8, 40.4, 41.8 s in batch
  runs; each renders correctly when snapshotted again). The overview's own batch snapshot shows the
  same at 63 s, so it is the snapshot harness, not this film; worth one look at the render.
- `data-hold` on S6's last line: the closing hold is deliberate.
