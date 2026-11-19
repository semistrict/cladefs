# SPEC: Import once, boot a hundred (cuddly cut)

GOAL:    the viewer leaves knowing that a container image is turned into a base once, by one
         command on the host, and that every sandbox boots a fork of it: a fork copies nothing,
         the sandboxes read one copy of what they share, and each writes only its own changes.
OUTPUT:  HyperFrames, 1920 x 1080, 30 fps, 62 s.
MUSIC:   the overview's bed, `uke-loud.wav` (120 BPM, phrase of 40 beats, 20 s) at 0.75: twice for
         20 s, then once whole (22 s) from 40 s, so its own last chord lands at 60.0 s and rings out.
SEAMS:   0, 10, 22, 32, 40, 52, 62 s, every one on a bar (2 s).
LOOK:    the overview's, unchanged: ground #EFEAF8 with 3% felt noise; ink #2E2440, 4-6 px outlines;
         accent #E2177E only for what is shared: the fork threads, the stat's `inherited` line, the
         one copy in host memory and its links; critter colours peach, rose, lilac, mint, butter,
         sky; secondary text #5E5672. Nunito 900 80 px lines, 800-900 30 px labels, 28 px floor;
         JetBrains Mono 30 px for commands and their output, 28 px in file chips.
OBJECTS: a host shell: a white window, butter title bar "a shell on the host", `$` prompts in
         #5E5672, typed at about 30 characters a second. The image: a peach cardboard box with
         flaps, tape, a sleepy face and a white label `alpine.tar`. Its files: white mono chips.
         The base: a mint critter (r 110, seed 77). Forks: dashed white ghosts that gain a patch where
         they write. Sandboxes: the overview's small windows with faces (20 x 5 grid, as its S2/S10).
         Host memory: the overview's bar, one accent block for the shared copy.
MOTION:  as the overview: pops back.out(2.2-2.8) from the feet; hops squash, stretch and land on
         elastic.out(1, 0.4); titles pop word by word (back.out(2.2), 60 ms) and then hold still.
SHOTS:   (source of every claim in brackets)
         S1 0-10   A shell on the host shows README's exports (`CLADEFSD_BUCKET=file:///tmp/cladefs/bucket
                   CLADEFSD_TOKEN=1`, `CLADEFS_MASTER_KEY=$(openssl rand -hex 32)`) from frame 0, then types
                   `docker export "$(docker create alpine)" > /tmp/alpine.tar`; the box drops; alpine's files
                   peek out as chips: `bin/busybox`, `bin/sh → /bin/busybox`, `etc/motd`, `root/ drwx------`,
                   `tmp/ drwxrwxrwt`. Lines "Your sandboxes start from an image", "Here, alpine: one tar of
                   its files", "Make it a base, once". Note "or an OCI image layout (skopeo copy; docker
                   save, once extracted)".
                   [README "Try it" (the exports: cladefsctl refuses every store command without a KMS,
                   `bucket::require_kms`; `fork` needs the bucket and token, `bucket::Host::from_env`);
                   cladefsctl.rs module doc ("IMAGE is an OCI image layout, as `skopeo copy` makes, or
                   `docker save` (whose tarball is one, once extracted) ... or a tar archive of one layer
                   ... as `docker export` makes"); import.rs `fill` (a directory is a layout, a file one
                   layer). Chips: `tar -tv` of the export of alpine 3.24.4.]
         S2 10-22  The shell types `cladefsctl import /tmp/cladefs/base 1073741824 /tmp/alpine.tar`; the
                   base pops in, empty; the chips hop into it one by one; a "checked" stamp; the next
                   prompt appears (import prints nothing). Lines "cladefsctl import makes the base",
                   "Every file, as the image has it", "No sandbox, no daemon needed". Notes "owners,
                   permissions, times, links and xattrs kept", "then the filesystem is checked", "an
                   Fxfs filesystem, 1 GiB, in a cell of its own".
                   [README "Try it" (the command; "This needs no VM and no daemon"); cladefsctl.rs
                   ("`import` makes a store at DIR, of CAPACITY bytes, holding an Fxfs filesystem ...
                   Files keep their owners, permissions, times and extended attributes"; `create`,
                   `take` and `import` "work so anywhere, as when building base images where no daemon
                   runs"); import.rs `fill` (`storage.check()`), `make_store` (a new cell);
                   Importer (hard links, symlinks).]
         S3 22-32  The shell types `cladefsctl fork /tmp/cladefs/base /tmp/cladefs/sandbox1`, which prints the
                   new cell `0/anp7500000`; a ghost hops out of the base on an accent thread. Then
                   `cladefsctl stat /tmp/cladefs/sandbox1` prints `…` (cell, capacity, used, available),
                   `own 0`, `inherited 8491008`, `own_extents 0`, `inherited_extents 153`, `public false`.
                   Lines "Fork it: nothing is copied", "The fork's bytes: all inherited". Notes "it holds
                   the base's extents and wrote none", "measured: alpine 3.24.4, arm64".
                   [README "Try it" ("`stat` shows the fork's bytes as `inherited`: it holds the base's
                   extents and wrote none"); cladefsctl.rs `Stat` Display (its lines, in that order);
                   open_cell.rs `fork_at_version` (the child inherits the base's extents by reference).
                   Values: run of these commands at f590f040c, 2026-11-19, alpine 3.24.4 arm64. The fork's
                   path is README's `vm1` renamed `sandbox1` (any DST), to keep "VM" off screen.]
         S4 32-40  The base sits low in the middle; a hundred ghosts pop out above it, on a hundred accent
                   threads; the count rolls 0 to 100. Lines "Fork it a hundred times", "Each boots a
                   sandbox". Notes "forks, each copying nothing", "each one: own 0, inherited all".
                   Not "constant time": `cladefsctl fork` of a store commits a version and waits for its
                   upload first (open_cell.rs `fork_at_version`: `commit_version`, `make_uploaded`), and
                   its writer makes the forks one at a time (integrator guide, "Forking a base on any
                   number of hosts at once").
                   [open_cell.rs `fork_at_version`; test_fxfs_guest_sandboxes.rs ("each VM boots on a
                   fork of its host's base, its root Fxfs, as a sandbox does").]
         S5 40-52  Each ghost becomes a sandbox (the overview's grid); host memory below holds one accent
                   block, `bin/busybox`, and a hundred links reach it; the count says 1. Writers' title
                   bars take their colour and small blocks of their own pop into memory. Lines "A
                   hundred sandboxes boot", "What they read is one copy", "What each writes is its
                   own". Notes "copy of bin/busybox in this host's memory", "one copy per host, shared
                   by one tenant's sandboxes", "its writes stay its own: private pages, then its own
                   extents".
                   [README intro ("once in each host's memory, where every VM that reads a file maps
                   the same pages"), "Why" ("file data is one copy per host, not one per VM ... A page
                   a guest writes becomes its own private copy until it is written back to a new
                   extent"); docs/architecture.md (a tenant's stores' pages are of its arena; public
                   bases' are shared by every tenant's forks), CONTEXT.md Public base. The film's
                   sandboxes are one tenant's: `import::store` makes the default tenant's cell.]
         S6 52-62  The empty box, the base, seven forks around it, four with patches; the base hops and
                   lands on the last chord (60.0 s). Lines "One image, one base", "Import once, boot a
                   hundred", held 6.6 s.
BANNED:  the overview's list: an end card or wordmark; the word "VM" on screen (say "sandbox"); calling
         cladefs "a page cache"; any franchise's creatures; dark void, glow, gradients, glass,
         everything centred, bullet lists, icons in circles, legends. And for this film: "snapshot"
         (say "version"); "template" or "golden image" (say "base"); "disk" for a cell; "constant
         time" for `cladefsctl fork` of a store; one copy across tenants unless the base is public
         (`cladefsctl public` not shown).
QA:      0 s (thumbnail: line and the shell's exports); 6.5 s (chips out); 14 s (chips hopping in); 29 s
         (stat output readable); 36 s (a hundred ghosts and threads); 46 s (one copy and its links);
         58 s (closing line); 60 s (the base lands); each seam -0.2/+0.2 s.
