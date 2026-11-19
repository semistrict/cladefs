# Storylines: container images as bases

The one idea: **a container image is imported once, as a base; every sandbox boots a fork of it,
reading the one copy and writing only its own changes.**

The worked example in every storyline is the README's "Try it": alpine, `docker export`ed to
`/tmp/alpine.tar`, then `cladefsctl import`, `fork` and `stat`.

## Recommended: A. One box, a hundred boots

Chosen because a stranger follows one real thing (a tar of alpine) from a command they know
(`docker export`) to a hundred sandboxes, and every step is a real command with its real output.

- **Device and form:** the worked example; an object-to-object chain (the box's files hop into the
  base, the base sprouts a fork, the fork multiplies, the forks become sandboxes).
- **What the viewer sees and feels:** a cardboard box of alpine's files is tipped into a fuzzy
  critter, the base, by one command, with no sandbox anywhere; a fork pops out of it and `stat`
  says `own 0`. A hundred forks boot a hundred sandboxes, all reading one `bin/busybox`, and each
  gets a patch where it writes.
- **First two seconds:** the line "Your sandboxes start from an image" is on screen at frame 0
  over a host shell typing `docker export "$(docker create alpine)" > /tmp/alpine.tar`; the box
  bounces in under it.
- **Beats:** the image → import (files into the base, checked) → fork and `stat` (own 0) → a
  hundred forks → a hundred sandboxes boot, one copy of busybox → each writes its own → "Import
  once, boot a hundred", the base lands on the last chord.
- **Look:** the overview's lilac felt, plum ink, pastel critters; the one new object is a host
  shell card (butter bar, JetBrains Mono) and a butter cardboard box.
- **Sound:** the uke bed, three phrases; pops as files land, a riser into the fork, chimes on the
  one copy, a boing on the last chord.

## B. Layer cake

- **Device and form:** the object is the explanation; one frame that rebuilds.
- **What the viewer sees and feels:** an OCI image as a stacked cake of layers; the importer
  applies them bottom first, a `.wh.motd` whiteout nibbles a file out of the layer below, an
  opaque whiteout empties a drawer; the cake is pressed into one base that forks slice.
- **First two seconds:** a two-layer cake labelled `alpine` and `RUN apk add git` drops onto the
  felt; the title "An image is layers" pops.
- **Beats:** layers → applied in turn → whiteouts → digests checked → one base → forks.
- **Look:** the same felt and critters; layers as felt slabs in critter colours.
- **Sound:** the bed; a squish per layer, a crunch per whiteout.
- Not built: it explains the importer's rules well but leaves the base and its forks, the point
  of the feature, to the last seconds.

## C. A hundred unpacks (the bold one)

- **Device and form:** life before and after; split screen to one.
- **What the viewer sees and feels:** on the left, a hundred sandboxes each unpacking its own copy
  of `alpine.tar`, boxes piling up and the host's memory sweating; on the right, one import and a
  hundred forks; the left collapses into the right.
- **First two seconds:** a hundred boxes rain onto a hundred sandboxes.
- **Beats:** a hundred unpacks → the strain → import once → forks → one copy.
- **Look:** the same, split down the middle.
- **Sound:** the bed; a clatter of boxes, then quiet pops.
- Not built: the "before" would need numbers of its own that no source gives, and it retells the
  overview's opening.
