# Review (shared volumes, cuddly cut)

## Creative calls
- One idea: one sandbox keeps a checkout current; the host forks its cell into a read-only base; any running sandbox attaches it and writes into a layer of its own; the base is kept once, its file data one copy in each host's extent cache. Worked example: the Linux checkout of doc-10's Proof (1.83 GB, 91,166 files).
- Storyline A of `STORYLINES.md` (one keeps it, the rest attach), built without stopping, as asked. Not built: B (an endless zoom into one write) and C (a hundred sandboxes split screen; C would show a hundred repos per sandbox, which TASK-136 AC #2 leaves open).
- The overview's look, characters, type, motion, bed and effects, unchanged. A published fork is a dashed ghost critter (a fork: still its parent's data) wearing a magenta flag; the accent is kept for the base, its layer, its threads and its file data in the extent cache.
- The screen says "base 1", "base 2", not doc-10's "generation": CONTEXT.md keeps "generation" for keys alone.
- `git fetch` onto a newer base is marked "in progress" on screen: TASK-136.7 is In Progress (built and its case passed once on GCE, AC unchecked). The sandbox manager's side is the integrator's (doc-10 "Not built"); the film shows it as the one caller of `AttachBase`, which the API defines.
- The publishing steps are words (sync, freeze, flush to one layer, describe it: manifest, thaw) in one chip that changes, not the ioctl names (`FIFREEZE`, `CLADEFS_IOC_FLUSH`, `CLADEFS_IOC_MANIFEST`, `FITHAW`), which SPEC.md cites.

## Measured
- `hyperframes check`: ok; lint, runtime, layout (9 samples), contrast (72 of 72), craft (576 samples), audio: no errors, no warnings. The first pass warned three S3 labels were on screen under their read floor; S3 was retimed and the warnings are gone.
- Looked at: every scene start and middle, every seam (-0.2 to +0.2 s), the phone sheet, and full frames at 42.5 s and 61 s.
- Not rendered and not heard: the effects are placed on their pictures by the files' measured peaks and the overview's offsets; the bed loops on its 20 s phrase and its last chord should land at 70.0 s, where base 1 lands. Measure the render with `hyperframes audio`.

## Weakest shot
- S3 (15-25 s) carries the most: five steps in one chip, a layer merge, a manifest card, a fork and a hop in 10 s. Each step holds 0.6-1.5 s; a viewer reads the words but may not tie "flush to one layer" to the bars merging. With more time: a beat more per step, or drop "sync".
- S2 (8-15 s) leaves the right third of the frame empty.

## Kept on purpose
- Numbers from doc-10's Proof (2026-11-07, TASK-136.1): 1.83 GB and 91,166 files for Linux v6.18; a 600-byte manifest (said only as "manifest" on screen); first open "about 60 ms" (attach 21-25 ms, import 32-36 ms, mount 1.5-1.8 ms); "1.7 MiB of disk" per added checkout.
- "One copy per host" (S6): doc-10 Attaching (pages keyed by extent ID, one copy) and TASK-234 (the extent cache keeps one copy of a published base's pages, done).
- "newest <url>" is the upcall `newest URL`.

## Fact check (independent, after the first cut), fixed
- S2: "cladefs knows nothing of Git" became "Fxfs and the daemon know nothing of Git": cladefs ships git-remote-fxfs.
- S4: the sandbox manager now carries a dashed "not built yet: the tests stand in" tag (doc-10); `AttachBase` is built.
- S6: "Kept once, cached once" became "Kept once, one copy per host"; the bar is "the host's extent cache" holding "base 1's file data"; a note says each guest still copies the metadata it reads (only file data shares through DAX; 130-185 MiB a sandbox for Linux's status, doc-10).
- S7: "The next pull makes base 2" became "Pull, publish again: base 2"; "git fetch moves a sandbox on" became "git fetch reaches base 2's objects", with "their volumes stay over base 1" (a mounted base doesn't change; the first stays pinned); the ask is "newest <url>"; still tagged in progress.
- S8: "1.7 MiB" became "from 1.7 MiB" (writable layers grow with pulls).
- After the fixes: `check` ok with no warnings (an S2 note was moved earlier to clear a read-floor warning); S2, S4, S6, S7, S8 snapshotted and looked at.
