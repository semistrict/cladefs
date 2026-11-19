# Storylines: replicated commits

The one idea: **a replicated cell's fsync returns once two of three hosts hold the commit on disk, so it
outlives its host without waiting for the bucket.** The worked example: one commit of cell 9 (the pink
bead), from the sandbox's fsync to a take on another host and its upload.

## A. One commit's journey (recommended: a stranger follows one bead from fsync to a lost host and back)

- **Device and form:** the worked example on one stage that rebuilds: four hosts in a row over the
  bucket; scenes swap the title and keep the stage, so the pink commit is never lost to a cut.
- **What the viewer sees:** a sandbox on host A calls fsync on one commit. Today it either waits for
  the bucket (~130 ms on GCS) or loses the commit with the host; then the same commit flies to two
  other hosts in one round trip and the fsync returns at two of three. Host A is lost, host D takes
  the cell and gets the commit back from the members; without a majority it waits for the bucket
  again.
- **First two seconds:** host A's dashed outline, a sandbox bounces in, cell 9 pops up beside it,
  "A sandbox calls fsync" pops word by word.
- **Beats:** fsync on host A → wait for the bucket (130 ms counter) or lose it with the host →
  Replicated: 2 of 3 hosts → copies and acks, fsync returns, no bucket request → host A lost, D
  takes: fence, read, head swaps → no majority: wait for the bucket → "Yes, once two of three hosts
  hold it", cell 9 hops on the last chord.
- **Look:** the overview's lilac felt, critter cells, sandbox windows, the pail; the accent only on
  the one commit and its copies.
- **Sound:** the uke bed looped on bars to a last chord at 66 s; pops on beads, chimes on each
  release, the stutter-squeak on a lost host, a boing on the last hop.

## B. The fsync clock

- **Device and form:** a number lands; type carries the argument, cut on the beat.
- **What the viewer sees:** a giant stopwatch over a sandbox's fsync: the bucket's ~130 ms ticks
  by, then the replicated path stops it after one round trip between hosts. The film is the race
  of the two paths, side by side, then a host falls over and only one path still has the commit.
- **First two seconds:** "fsync" in 200 px, a stopwatch hand starting.
- **Beats:** the stopwatch → bucket path → hosts path → the stop → the lost host → the claim.
- **Look:** the series' felt and type, the stopwatch as a butter critter.
- **Sound:** a tick on each beat under the bed, a chime when the hosts' path stops the clock.
- **Why not:** no Replicated latency has been measured (doc-21's "a few ms" is an estimate), so the
  race would show a number the source does not have.

## C. Hold my beads (the bold one)

- **Device and form:** a character does the work; one continuous take.
- **What the viewer sees:** cell 9 is a critter whose log is a necklace of beads; before it naps it
  hands a copy of each new bead to two friends on neighbouring hosts, and only when one friend says
  "got it" does it nod. When it falls asleep for good, a new cell 9 wakes on another host and the
  friends give its beads back.
- **First two seconds:** cell 9 threading a pink bead onto its necklace.
- **Beats:** the necklace → hand-off → "got it" → sleep → wake elsewhere → beads returned.
- **Look:** as A, the members drawn as critters.
- **Why not:** members are hosts, not cells; drawing them as critters breaks the series' rule that a
  critter is a cell, and it hides the bucket's part.
