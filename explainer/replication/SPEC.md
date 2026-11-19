# SPEC: Replicated commits (cuddly cut)

GOAL:    the viewer leaves knowing that a replicated cell's fsync returns once two of three hosts hold the
         commit on their disks, the writer's own counting as one: one round trip on the hosts' network, no
         bucket request; the upload goes on behind; lose the writer's host and another host takes the cell
         and gets the commit back from the members; the bucket's head still says who writes; without a
         majority the cell waits for the bucket again.
OUTPUT:  HyperFrames, 1920 x 1080, 30 fps, 70 s (2,100 frames).
MUSIC:   the overview's bed, `uke-loud.wav` at 0.75, 120 BPM, phrase of 40 beats (20 s): three passes, then a
         last pass from 12 s in (on a bar) so its own last chord lands at 68.0 s; 2 s of ring-out to 70 s.
SEAMS:   0, 6.5, 19, 25.5, 37, 53, 60.5, 70 s; the stage carries across every seam but S2-S3 (host A lost).
LOOK:    the overview's, unchanged: ground #EFEAF8 with 3% felt noise; ink #2E2440, 4-6 px outlines, round
         joins; accent #E2177E only for the one commit (the pink bead) and its copies, and the last commit of
         S7, the same idea; critter colours peach, rose, lilac, mint, butter, sky; secondary text #5E5672.
         Nunito 900 80 px lines at (120, 84), 800-900 28-30 px labels and tags, 28 px floor; JetBrains Mono
         for the head. One stage throughout: four hosts as dashed outlines 400 x 510 at x 120, 540, 960,
         1380, y 250, each with its disk (a lavender strip, beads on it are commits of cell 9's log); the
         bucket pail at (960, 900), its name and head to its left (uploads from D pass under host C and
         reach it from the right). S1 is close on host A (1.3x); S2 pulls back to the whole stage.
CHARACTERS: cell 9 is the overview's peach critter (seed 909); the sandbox is the overview's window with a
         face (smiles when its fsync or sync returns, frowns while it waits). Commits are beads, as in the
         overview's S8. Hosts and members have no faces: only cells are critters.
MOTION:  the overview's: pops back.out(2.2-2.8) from the feet; copies fly on quadratic arcs, power1.inOut,
         0.8-1.0 s; acks fly back as small butter dots, 0.5-0.6 s; uploads drift straight to the pail; a
         lost host's contents shrink on back.in(2) and its outline fades to 0.3; titles pop word by word
         (back.out(2.2), 60 ms) and then hold still.
SHOTS:   (sources in brackets)
         S1 0-6.5   host A, a sandbox and cell 9; one commit (pink bead) leaves the sandbox; "fsync returns" at
                    once, then the commit reaches host A's disk. "A sandbox calls fsync", note "by default, fsync
                    returns once the daemon has the commit", "Will its commit outlive host A?". [layout.rs
                    `enum Fsync`, Ordered the default; cladefsd.proto FsyncLevel; doc-12: fsync, fdatasync, msync
                    and O_SYNC wait for ordered; CONTEXT Ordered]
         S2 6.5-19  "A sync waits for the bucket": a sync on the same commit, a copy drifts to the pail while a
                    counter rolls to ~130 ms ("one sync on GCS, measured"), then "sync returns". "An fsync
                    doesn't: lost with the host": a second commit's fsync returns at once, host A is lost before
                    its upload lands, the uploaded commit is kept. [doc-12: sync, syncfs, unmount and freeze ask
                    COMMIT_UPLOADED; cladefsd.proto: sync waits for uploaded in a cell not replicated; doc-21 Why
                    and TASK-537: the bucket's round trips on every sync, ~130 ms on GCS (TASK-165.11's measured
                    guest fdatasync waiting for the upload, r740); doc-21 "the fsyncs that don't pay them lose up
                    to the whole upload lag with the host"; doc-12 row "Host lost, disk with it, with a bucket:
                    the unuploaded lag goes"]
         S3 19-25.5 hosts A, B, C; "Replicated: on 2 of 3 hosts' disks"; note "an option per cell: cell 9's fsync
                    waits for Replicated"; roles writer, peer, peer; "The writer's own copy counts as one", note
                    "members: the writer and 2 peers, f = 1"; the head "epoch 7 · A B C". [doc-21 decisions "A
                    per-cell option", "The option is per cell, set by CreateCell and PlaceCell (f, fsync)";
                    cladefsd.proto `replicas`, `FsyncLevel fsync`; CONTEXT Replicated, Configuration; doc-21
                    Terms; layout.rs Configuration: "Its members, its writer first"]
         S4 25.5-37 the commit: appended to host A's log and, as it is appended, sent to B and C on arcs; host A's
                    own sync ("on disk: 1 of 3") overlaps their round trips; each answers with an ack; 2 of 3 at
                    B's ack → the fsync returns; C, slower, answers 1.5 s later → 3 of 3. "One round trip on the
                    hosts' network"; "No request to the bucket on the way" ("bucket requests: 0"); "The upload
                    goes on behind": a copy starts to drift to the pail; note "B and C keep it until the head
                    names it uploaded". [doc-21 Commit steps 2-4; cell.rs `send()`: "sends a flush's records, as
                    it appends them, before its syncs ... their round trips overlap its own sync"; Costs: "one
                    round trip ... and no bucket request"; replicate.rs "the writer's own copy the f+1-th";
                    replicate.rs `acknowledged()`: "The head names segment `segment` and those before it: their
                    logs go, here and on the members"]
         S5 37-53   "Host A is lost, its disk with it": its contents shrink, the drifting upload is gone. "Host D
                    takes the cell": the head goes to epoch 8 (the take; note "takes the head at epoch 8, fences
                    B and C"), fences pop on B and C ("fenced at epoch 8"); the uploaded commits come up from the
                    bucket and the pink commit from B onto D's disk (note "then reads the longest log B and C
                    hold"); "Nothing Replicated is lost". Install, then activation: D sends that tail to B and C
                    again (new logs, slot 2), their acks come back, only then the head swaps to "epoch 8 · D B C"
                    and the sandbox and writer tag pop on D; then B and C's old configuration's copies go (note
                    "the tail to B and C, then the head, then D serves"). "The bucket's head says who writes":
                    the upload from D lands, the head acknowledges it, and B and C drop their copies (note "once
                    the head names it uploaded, B and C drop theirs"). [doc-21 Take steps 1-6, Fence and the
                    "f+1 answers from members other than the old writer"; upload.rs `read_longest`; upload.rs
                    `replicate()`: the seed sent and waited for replicated, then `writer.configure`, then
                    `drop_before` of the old configuration; doc-21 mutations ActivateBeforeInstall,
                    DropBeforeAck; doc-12 replicated row "Writer's host lost, its disk with it: kept", tested
                    by cladefs's replicated_commits_survive_their_writer_and_its_disk; CONTEXT Head, Take,
                    Epoch]
         S6 53-60.5 "No majority? It waits for the bucket": B and C lost, a "degraded" tag on cell 9, a commit
                    goes to D's disk and straight to the pail ("uploaded at once"), then the fsync returns.
                    "Replicated again once members answer": B and C come back. [CONTEXT Degraded; doc-21
                    Degraded; doc-12 row "Both members lost, writer up: degraded, its commits wait for Uploaded,
                    uploaded at once ... comes back as members answer"]
         S7 60.5-70 "Will a commit outlive its host?" on host D: one more commit, sent to B and C as it is appended,
                    fsync returns at 2 of 3; "Yes, once two of three hosts hold it"; note "a cell set to Replicated, f = 1: lose any
                    one host"; cell 9 hops and lands on the last chord (68 s). [doc-12 replicated table, f = 1]
ILLUSTRATIVE: the epochs 7 and 8; which host takes (any host may, doc-21 Take); the number of beads; the
         flights' durations (a real round trip is milliseconds, the counter's 130 ms is stretched to 2.4 s).
LEFT OUT: incarnations, the joint rule, replacements after 30 s, the f = 0 drop by name, Local as an
         fsync level, Replicated latency in ms (doc-21 estimates "a few ms over Local";
         nothing measured).
BANNED:  the overview's list: an end card or wordmark; the word "VM" on screen (say "sandbox"); dark void,
         glow, gradients, glass, everything centred, bullet lists, icons in circles, legends. Plus this film's:
         "durable", "replica" alone, "replica set", "quorum-committed", "snapshot", "lease" on screen (CONTEXT
         _Avoid_ words); a Replicated latency in ms; members drawn as critters (only cells are critters).
QA:      every scene start, middle and seam; S4 at 27.9 s (fsync returns at 2 of 3, C's ack not yet
         sent); S5 at 47.0 s (B and C hold the new tail, head about to swap) and 48.0 s (head "epoch 8 · D B C",
         sandbox on D, old copies gone); S7 at 68.0 s (hop lands).
