# Storylines: shared volumes

Recommendation: **A, One keeps it, the rest attach**, because a stranger follows one Linux checkout
from the sandbox that keeps it to the sandboxes that write over it, and every step is a thing that
happens to that checkout. The brief asked for no stops, so A is built; B and C are kept here.

All three share the series' look (lilac felt, plum ink, critter cells, sandbox windows, Nunito,
the ukulele bed, the overview's effects) and its one example: the Linux repo, 91,166 files, 1.83 GB
(doc-10, Proof).

## A. One keeps it, the rest attach (built)

Device: one object across the story (the Linux checkout) and a character doing the work (the
publisher sandbox). Form: one diagram that grows, a new layout every scene. The viewer sees three
sandboxes each cloning Linux and sweating, then one sandbox keeping the checkout current and
publishing it as a fork, then sandboxes attaching that base while they run, each writing into a
layer of its own, the base's pages kept once; then the next base. It feels like watching one
generous critter feed a crowd.

- First two seconds: three sandboxes side by side, each with `git clone linux` over its head and a
  disk filling with 1.83 GB; the line "Every sandbox clones the same repo" is on screen at frame 0.
- Beats: hook (three clones) → one keeps it current (`git pull`) → publish (sync, freeze, flush,
  manifest, fork, thaw: base 1) → attach (`ls /repos/linux`, AttachBase, import, about 60 ms) →
  a write (`vim Makefile`, `rm README`: its own layer, a tombstone, the base untouched) → kept
  once and cached once → base 2 and `git fetch` (in progress) → close, the base critter lands on
  the last chord.
- Look: the overview's, unchanged; the base is a dashed ghost critter in a magenta flag.
- Sound: the uke bed looped on its 20 s phrase, its last chord at 66 s; pops on chips and
  critters, chimes on the fork and on "cached once", a boing on the last landing.

## B. Inside one write

Device: the object is the explanation (a volume's LSM tree). Form: an endless zoom. We open on a
sandbox typing `vim Makefile` in `/repos/linux`, dive into the volume through the file row,
through the tree's layers to the base's bottom layer, read-only and magenta; the write lands one
layer up, a tombstone hides a file, and the zoom pulls out through the device to the host's one
copy of the base's pages, then to the publisher that made it.

- First two seconds: a terminal row `vim Makefile` in a sandbox window, the cursor in the file.
- Beats: write → zoom into the layers → the base never changes → pull out to many sandboxes →
  one copy of the pages → reveal the publisher → the next base.
- Look: the overview's, with deeper layer cutaways; one zoom carries every cut.
- Sound: the bed under one long riser into the reveal of the publisher.

## C. The hundredth checkout (the bold one)

Device: a number lands. Form: split screen to one. Left, a hundred sandboxes each cloning Linux:
1.83 GB each, counters running. Right, a hundred attaching one base: 1.7 MiB of disk each,
about 60 ms to open. The left side churns and frowns, the right side stays calm; then the split
collapses into the right side and the film shows how: the publisher, the fork, the layer.

- First two seconds: the split already running, two counters climbing at different speeds.
- Beats: two counters → left churns → collapse to the right → how: publish, attach, write →
  the numbers again, the left side gone.
- Look: the overview's; counters in Nunito 900, 120 px.
- Sound: the bed thinned to pizzicato on the left side's churn, full when it collapses.
- Not built: a hundred attachments at once in one sandbox is a design aim (TASK-136 AC #2 still
  open), so the split would claim more than is shipped.
