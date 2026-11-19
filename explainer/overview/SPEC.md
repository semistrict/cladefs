# SPEC: One copy, a hundred sandboxes (cuddly cut)

GOAL:    the viewer leaves knowing how cladefs stores sandbox disks: a sandbox is memory and a disk; the bucket is
         the disk and the host is the cache; unlike virtio-fs the filesystem (Fxfs, from Fuchsia) lives
         in the sandbox; data is write-once extents; every disk is a small independent cell; forks copy
         nothing; so a hundred sandboxes keep one copy.
OUTPUT:  HyperFrames, 1920 x 1080, 30 fps, 132 s.
MUSIC:   catalog ukulele and pizzicato bed, 120 BPM, phrase of 40 beats (20 s), looped six times, then a
         last pass from 10 s in (on a bar) so its own last chord lands at 130.0 s. Limited copy `uke-loud.wav` (-14.1 LUFS) at 0.75.
SEAMS:   0, 9, 17, 32, 45, 52, 59, 72, 78, 90, 101, 110.5, 120, 132 s (s11-s13 play before s10).
LOOK:    cuddly: pastel critters on a lilac felt ground.
         ground #EFEAF8 with 3% felt noise; ink #2E2440 (plum) for words and outlines, 4-6 px, round joins,
         corners 14-40 px; accent #E2177E only for the one shared copy, its links, and the v1 flag;
         critter colours peach #FFB088, rose #FF9EC0, lilac #B9A4FF, mint #8FDDC2, butter #FFD86E,
         sky #8CC8FF; secondary text #5E5672. Nunito 900 80 px lines, 800-900 26-40 px labels, 28 px
         floor; JetBrains Mono for IDs.
CHARACTERS: cells are fuzzy round critters (seeded tufted outline, boiling at 4 redraws a second,
         breathing, seeded blinks, blush, feet); forks start as dashed white ghosts and gain a patch of
         colour where they write. Sandboxes, processes, the filesystem cabinet and extents have small faces.
         Real marks from their sources: the Fuchsia lockup (fuchsia.dev) and Tux.
MOTION:  pops back.out(2.2-2.8) from the feet; hops squash, stretch, land on elastic.out(1, 0.4); titles
         pop word by word (back.out(2.2), 60 ms) and then hold still.
SHOTS:   S1 meet a sandbox: it opens into memory and a disk; a file hops into memory. "Now make a hundred".
         S2 100 sandboxes pop from one; 100 copies flood host memory; they frown, memory sweats.
         S3 your OS's page cache beside cladefs on one host; the bucket is the disk, the host the cache.
         S4 virtio-fs: every stat leaves the sandbox (trip counter); 8-55x ext4 (measured, README/doc-2);
            cladefs: stats answered inside, trips 0, only file data shared from the host.
         S5 the filesystem is Fxfs, born in Fuchsia; it hops into a Linux sandbox.
         S6 an extent (cell 9, seq 1003), written once; plops on the host's disk, floats to the bucket.
         S7 a field of critter cells; cell 9 up close (log, head, key, one writer); a disk, a memory
            image, a database are all cells; cell 9 hops from host A to host B.
         S8 cell 9's log of beads; v1 stamps; a copy floats up to the bucket.
         S9 fork from v1: one ghost, eight, ninety-six; writers gain patches.
         S11 a shared git checkout: one sandbox keeps it current (git fetch, gen 1 to gen 2); six others
             attach it, each with a writable layer of its own (backlog doc-10).
         S12 host A goes to sleep; cell 9 rises from the bucket onto host B under token 8; host A's late
             write bounces off a fence (fencing token, placement).
         S13 a tenant's cells, each with its own key, wrapped by the tenant's key in the KMS; the key
             breaks and every cell goes poof (shred; docs/operating-a-host.md).
         S10 the close returns to S2: 100 frowning sandboxes, 100 copies, "Remember the hundred copies?";
             the copies squeeze into one, the count rolls 100 to 1, frowns flip to smiles; last line "What they
             share is kept once"; cell 9 hops in and lands on the music's last chord (130 s).
BANNED:  an end card or wordmark; the word "VM" on screen (say "sandbox"); libc as the example (use a git repo's files); calling cladefs "a page cache" (it is the whole storage system: the
         page cache is the analogy); Star Trek or any other franchise's creatures (the critters are our
         own); dark void, glow, gradients (logos excepted), glass, everything centred, bullet lists,
         icons in circles, legends.
