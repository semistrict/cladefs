# Review (cuddly cut)

## Creative calls
- One worked example from end to end, README's "Try it": alpine `docker export`ed, `cladefsctl import`,
  `fork`, `stat`, then a hundred forks booting sandboxes. Storyline A of `STORYLINES.md`; B (layer cake)
  and C (a hundred unpacks) not built.
- The overview's look and cast unchanged: felt, plum ink, Nunito, critters, sandbox windows, host
  memory bar, word-pop titles, uke bed and its sfx. New objects, this film's own: a host shell (butter
  bar) that types real commands and prints their real output, and a cardboard box for the tar
  (`assets/images.js`: the shell's typing and printing, and the accent threads; drawing them is
  `Cuddly.draw`).
- The commands and output are real: run at f590f040c on 2026-11-19 against alpine 3.24.4 (arm64).
  The shell shows README's exports (bucket, token, master key) from frame 0, so every command on
  screen runs as typed. `stat` printed `own 0`, `inherited 8491008`, `own_extents 0`,
  `inherited_extents 153`, `public false`; its first four lines are shown as `…`. The fork printed
  `0/anp7500000`. Only the fork's path differs: README's `vm1` is `sandbox1`, to keep "VM" off screen.
- Left out: `cladefsctl public` and cross-tenant sharing (S5 says "one tenant's sandboxes"), versions
  and forking on many hosts (integrator guide), OCI layers and whiteouts beyond one note.

## Fact check (coordinator's, each verified in the code before fixing)
- The commands failed as typed without the KMS, bucket and token: the exports are now on screen.
- "docker save" makes a tarball that is a layout once extracted (import.rs reads a file as one
  layer): the note says "docker save, once extracted".
- "One copy per host" holds within a tenant (docs/architecture.md, arenas): now "shared by one
  tenant's sandboxes".
- "Each made in constant time" was wrong for `cladefsctl fork` of a store, which commits a version
  and waits for its upload (`fork_at_version`): now "each copying nothing".
- `stat`'s `public false` is shown; "Each is a sandbox's root disk" (disk is _Avoid_ for a cell) is
  now "Each boots a sandbox".

## Measured
- `hyperframes check`: ok, no errors, no warnings. One run flagged `#s5-private` as on screen 2.16 s
  from a single sample with no opacity at 49.625 s; the next run, with only S1's note changed, was
  clean. Taken as a sampling glitch: the note holds 7.4-11.3 s of its scene.
- Looked at: `snapshot --review` (beats, five seams, phone sheet) and frames at 0, 6.5, 14.2, 18.5,
  28.5, 29, 36, 46, 49.5, 60 s after the fixes.
- Not measured: the render's loudness (no render yet); the sound was placed, not heard.

## Weakest shot
- S5 (40-52 s): the writers' own colour is a 10 px title bar on a 76 px sandbox and a thin block in
  memory; it reads as a sprinkle, not as "each writes its own". With more time: give each writer a
  patch like the forks' in S6, or zoom on one sandbox.

## Kept on purpose
- The shell's text and the notes are 30 px: readable at 1080, not on the 360 px phone sheet (as the
  overview's notes).
- S4 shows a hundred forks of a store on one host, as README's `fork` makes them, one at a time; it
  claims no speed.
