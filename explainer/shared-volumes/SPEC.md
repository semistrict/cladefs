# SPEC: One keeps it, the rest attach (shared volumes, cuddly cut)

GOAL:    the viewer leaves knowing how a shared volume works: one sandbox keeps a Linux checkout
         current with plain Git; the host forks its cell into a read-only base (nothing copied); any
         running sandbox attaches that base by looking in /repos/linux, and writes into a layer of
         its own, the base untouched; the base is kept once, its file data one copy in each host's
         extent cache; the publisher's next pull and publish make the next base, whose objects
         `git fetch` reaches, the volume staying over the first (in progress).
OUTPUT:  HyperFrames, 1920 x 1080, 30 fps, 72 s (2,160 frames).
MUSIC:   the overview's bed, `uke-loud.wav` at 0.75, 120 BPM, 40-beat (20 s) phrase: three passes
         (0, 20, 40 s), then a last pass from 10 s in (on a bar) at 60 s, so its own last chord lands
         at 70.0 s; the film ends at 72 s. The overview's effects, each on a visible action.
SEAMS:   0, 8, 15, 25, 35, 45, 53, 63, 72 s.
LOOK:    the overview's, unchanged: ground #EFEAF8 with 3% felt noise; ink #2E2440 for words and
         outlines, 4-6 px, corners 14-40 px; accent #E2177E only for the base (its flag, its layer's
         border, its threads, its pages); critter colours peach #FFB088, rose #FF9EC0, lilac
         #B9A4FF, mint #8FDDC2, butter #FFD86E, sky #8CC8FF; secondary text #5E5672. Nunito 900
         80 px lines (one at a time, top-left at 120, 84), 800-900 28-40 px labels, 28 px floor;
         JetBrains Mono 500 for commands and paths.
OBJECTS: sandboxes are the overview's windows with faces (Cuddly.vm). A volume is the overview's
         s11 folder card (butter tab, file rows). A cell is a fuzzy critter; the base is a dashed
         ghost critter (a fork: still its parent's data) wearing a magenta flag "base 1". A volume's
         layers are a stack of bars: the base's at the bottom, magenta border, "read-only"; the
         sandbox's own above it in the sandbox's colour. The manifest is a small paper card. The
         sandbox manager is a plum-outlined box on the host with a clipboard face. Commands are
         white mono chips.
MOTION:  as the overview: pops back.out(2.2-2.8) from the feet; hops squash, stretch, land on
         elastic.out(1, 0.4); titles pop word by word (back.out(2.2), 60 ms) and then hold still;
         threads draw on power2.out in 0.4-0.5 s; exits fade 0.25-0.35 s on power2.in.
WORDS:   CONTEXT.md's: sandbox (never "VM" on screen), cell, fork, base, volume, layer, host,
         bucket, extent cache's pages. doc-10 calls each published fork a "generation", but
         CONTEXT.md keeps "generation" for keys alone (MakePublic's flag is `attach_only`), so the
         screen says "base 1", "base 2". Never "snapshot" for a cell's state.

SHOTS (sources per shot):
 S1 0-8     Three sandboxes in a row, each typing `git clone linux`; under each a disk bar fills to
            "1.83 GB". Lines "Every sandbox clones the same repo" / "and keeps pulling it".
            Pops on the sandboxes, a boing as the third disk bulges.
            Source: doc-10 intro and TASK-136 ("many users clone the same repos over and over and
            keep them up to date"); Linux v6.18 checkout, 91,166 files, 1.83 GB (doc-10 Proof).
 S2 8-15    The first sandbox grows to the hero: its volume "linux" beside it, rows Makefile,
            README, kernel/, fs/, mm/. `git pull` pops; two rows flash mint. Lines "One sandbox
            keeps it current" / "with plain git". Note: "Fxfs and the daemon know nothing of Git" (cladefs does ship
            git-remote-fxfs, S7, so not "cladefs").
            Source: doc-10 Decisions ("Who keeps a shared volume current: a sandbox, running
            ordinary commands"), intro ("Nothing in Fxfs, the store or the daemon knows of Git").
 S3 15-25   Publishing. Chips light in turn beside the sandbox: sync, freeze, flush to one layer,
            manifest; a manifest card (Linux's is 600 bytes) floats down to the host. On the host the
            publisher's cell (butter critter) is forked: a dashed ghost hops out, a magenta flag
            "base 1" pops on it, the manifest card lands beside it; then "thaw" lights and the
            publisher smiles, free to go on. Lines "It publishes a base" / "The host forks its cell" /
            "Then it thaws and goes on". Notes "a fork of its cell: nothing copied" and "base 1:
            read-only, as of now". Chimes on the flag.
            Source: doc-10 Publishing steps 1-7 (sync, FIFREEZE, CLADEFS_IOC_FLUSH "puts all it
            holds in one layer", FIFREEZE others, CLADEFS_IOC_MANIFEST, the host forks the store
            and MakePublic `attach_only`, FITHAW); guest/linux/fs/cladefs/vfs.h; the test's
            publish_volume (shared_volumes.rs); "a 600-byte manifest" (doc-10 Proof); a fork copies
            nothing (CONTEXT.md Fork, O(1)); "the sandbox moves on".
 S4 25-35   Attaching. A second sandbox (sky), already running, types `ls /repos/linux`. A dotted
            ask leaves it, "attach linux", to the sandbox manager on the host; the manager calls
            `AttachBase`; a magenta thread draws from base 1 to the sandbox; the manifest card
            flies back; "import" and "mount" chips pop; the folder "linux" opens in the sandbox,
            butter tab, rows filled. Counter "first open: about 60 ms". Lines "Another sandbox just
            looks in /repos/linux" / "It attaches while it runs". Notes "only the sandbox manager
            attaches", "the guest can only ask"; under the manager a dashed tag "not built yet: the
            tests stand in" (doc-10: "Not built: the sandbox manager's part, which the tests stand
            in for"; `AttachBase` itself is built, TASK-136.5).
            Source: doc-10 Automount (lookup under /repos mounts the volume; `attach NAME` answered
            `manifest SIZE`), Attaching (AttachBase, "The guest cannot ask"), Decisions (attach
            authorization: the sandbox manager alone, through the host control API); proto
            `rpc AttachBase` comment; crates/fxfs-automount/src/linux.rs (`/repos`);
            CLADEFS_IOC_IMPORT; first open 21-25 + 32-36 + 1.5-1.8 ms, "about 60 ms" (doc-10 Proof).
 S5 35-45   A write. The sky sandbox's volume opens into its layers: bottom bar "base 1's layer,
            read-only" (magenta border), above it "its own layer" (sky). `vim Makefile`: a small
            block drops into its own layer, the base bar does not move. `rm README`: a tombstone
            drops into its own layer and README fades from the file list; the base bar still has it.
            Lines "Its writes go in a layer of its own" / "The base never changes". Note "only the
            blocks it writes: no copy of the file".
            Source: doc-10 The model ("bottom LSM layers are a base's, read-only ... tombstones hide
            base records, and writes make new extents, per block. So there are no whiteouts, no
            merged directories and no file-level copy-up"); TASK-136 AC #1 (writes private, base
            unchanged, done).
 S6 45-53   Many. Six sandboxes in a row over base 1, magenta threads to each; each gains a thin
            layer of its own colour. On the host, a bar "the host's extent cache" with one magenta
            block "base 1's file data" and six slivers of their own. Lines "Six sandboxes, one base"
            / "Kept once, one copy per host". Notes "each added checkout: 1.7 MiB of disk" (above
            the row), "one copy, however many sandboxes attach" and "each guest still copies the
            metadata it reads" (doc-10 Open questions: only file data shares through DAX; layer
            blocks are copied into each guest, 130-185 MiB a sandbox for a Linux-sized status).
            Source: doc-10 Attaching ("sandboxes on one generation share one pin, one ExtentStore
            ... The extent cache is keyed by extent ID, so pages stay one copy"); TASK-234 (the
            extent cache keeps one copy, done); doc-10 Proof ("Disk per added checkout 1.7 MiB",
            measured on three sandboxes). "Kept once": a fork copies nothing (CONTEXT.md Fork), so
            the base's extents stay the publisher's, once in the bucket.
 S7 53-63   The next base. Line "Pull, publish again: base 2": the publisher pulls, the host
            forks base 2 (a second ghost, flag "base 2"). Note "their volumes stay over base 1".
            Line "git fetch reaches base 2's objects", a dashed "in progress" tag under it. One
            sandbox types `git fetch`, its chip turning into the ask "newest <url>" (upcall
            `newest URL`, answered `named NAME`, crates/fxfs-automount/src/upcall.rs); base 2's
            thread draws to it beside base 1's; notes "its alternates: base 2's objects", "it
            fetches only what base 2 lacks".
            Source: doc-10 Moving to a newer generation (git-remote-fxfs, `newest URL`, alternates,
            "the fetch transfers only what is newer than the generation", "A mounted worktree's base
            doesn't change"); TASK-136.7 is In Progress (AC unchecked), so the screen says so.
 S8 63-72   Close, back to S1's three sandboxes: their three 1.83 GB disk bars squeeze into one
            magenta "base" bar, three thin layers left; frowns to smiles. Lines "Three clones became
            one base" then, held to the end, "And a layer each". Each disk's size sits inside its bar
            (1.83 GB, then "from 1.7 MiB": the writable layers grow with pulls, doc-10); the bar reads "base 1: the 1.83 GB checkout, once". Base 1's critter hops in and lands
            on the last chord (70.0 s).
            Source: as S1, S3, S6 (1.83 GB checkout; 1.7 MiB per added checkout).
BANNED:  the overview's: an end card or wordmark; the word "VM" on screen (say "sandbox"); libc as
         the example; calling cladefs "a page cache"; Star Trek or other franchises' creatures; dark
         void, glow, gradients (logos excepted), glass, everything centred, bullet lists, icons in
         circles, legends. This film's: "generation", "snapshot" or "copy-on-write" on screen;
         numbered step labels ("01 / 02"); showing `git fetch` onto a new base as shipped; a
         hundred repos per sandbox (TASK-136 AC #2 open); any claim of attachments across hosts.
QA:      every scene start, middle and seam; 8 s (S1 to S2 seam), 20.5 s (base 1 lands), 31 s
         (sandbox attached), 40 s (tombstone), 50 s (one copy per host), 59 s (in progress tag
         readable), 70 s (last landing).
