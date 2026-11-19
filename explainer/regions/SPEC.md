# SPEC: An ocean away (many regions: cells across buckets)

GOAL:    the viewer leaves knowing that cladefs gives each cloud region its own bucket, numbered from the
         root bucket, and that every cell's ID carries its bucket's number, so every request goes to the
         right bucket, opened with the host's own credentials; parts that must cross a region do so about once
         for it, and peers never cross. Forks across buckets and mirrors are shown as in progress.
OUTPUT:  HyperFrames, 1920 x 1080, 30 fps, 72 s (2160 frames).
MUSIC:   the overview's ukulele and pizzicato bed (`uke-loud.wav`), 120 BPM (15 frames a beat), phrase of 40
         beats (20 s), at 0.75: three passes cut at 20 s, then a last pass from 10 s into the file (on a bar)
         at 60 s, so its own last chord lands at 70.0 s, on the cell's landing; it rings to 72 s.
SEAMS:   0, 9, 22, 32, 44, 55, 64, 72 s.
LOOK:    the overview's, unchanged: ground #EFEAF8 with 3% felt noise; ink #2E2440, outlines 4-6 px, round
         joins, corners 14-48 px; critter colours peach #FFB088, rose #FF9EC0, lilac #B9A4FF, mint #8FDDC2,
         butter #FFD86E, sky #8CC8FF; secondary text #5E5672. Nunito 900 80 px lines, 800-900 28-40 px
         labels, 28 px floor; JetBrains Mono for IDs, URLs, commands and variables.
         Accent #E2177E only for bucket numbers: the pails' number badges, an ID's top 16 bits, the
         registry's numbers, and a read's route to the bucket its number names. Butter tags mark what is
         in progress.
WORLD:   one frame that rebuilds: two cloud regions as darker felt patches (#E3DAF5, corners 48 px),
         us-east-1 at x 100-740 and europe-west1 at x 1180-1820, y 250-850, each named in mono at its
         top left; between them an ocean, a #D8ECFF patch (x 760-1160, y 300-800) with five rows of
         sky-blue waves. Every scene redraws it in the same place, so cuts change only what is in it.
         Pails at (420, 740) and (1630, 740); the sandbox at (1220, 330); the cell at (1330, 690).
CHARACTERS: the overview's: cells are fuzzy critters (seeded fur boiling at 4 redraws a second, breath,
         blinks, blush, feet); the sandbox is the overview's window with a face; buckets are its lilac
         pail, here wearing an accent number badge; hosts are its dashed grey boxes, each with a white
         part-cache shelf. The sandbox's disk is the peach cell; the child across buckets is mint.
MOTION:  pops back.out(2.2-2.8) from the feet; hops squash, stretch, land on elastic.out(1, 0.4); titles
         pop word by word (back.out(2.2), 60 ms) and then hold still; requests are butter pills that
         travel on power1.inOut arcs; counters step with each trip, never glide.
SHOTS:   (source after each)
  S1 0-9    A sandbox in europe-west1 restores its disk from the one bucket, in us-east-1. A pill crosses
            the ocean and back 11 times; "N round trips across" steps 0 to 11; the sandbox frowns and
            fidgets. "One bucket, an ocean away" / "Each trip crosses the ocean"; note "a cold restore:
            11, one after another". [11 = cold copy-open 4 + cold fork 6 + cold fork-open 1 round trips
            one after another, asserted in `restores_ask_the_bucket_what_the_protocol_needs`
            (crates/cladefs/src/bucket/lifecycle.rs); one bucket for several cloud regions pays a round
            trip across per serial request: doc-23 "Why not the other shapes".]
  S2 9-22   `$ cladefsctl format-bucket s3://ROOT --region us-east-1 -> 1`: badge 1, "root bucket
            s3://ROOT". `$ cladefsctl add-bucket s3://ROOT gs://OTHER --region europe-west1 -> 2`: a pail
            pops in europe-west1 with badge 2. The registry card (1 s3 us-east-1, 2 gcs europe-west1) pops
            at the root and a copy hops over the ocean to bucket 2. "A bucket in every cloud region" /
            "Numbered in turn, from the root" / "Every bucket keeps a copy of the registry"; note "number, cloud,
            region, URL, KMS; no credentials". [docs/operating-a-host.md "Buckets and their numbers" (its
            example commands verbatim; each prints the number); TASK-633.9 (done); CONTEXT.md Root bucket,
            Registry, Formatting, Adding; provider names `s3`, `gcs` from bucket_cell identity::Details.]
  S3 22-32  The sandbox's disk, the peach cell, hangs its ID `0002 000005 000009`: "bucket 2" (accent),
            "block 5", "n 9"; an accent dotted line runs from 0002 to badge 2; note "bucket << 48 |
            block << 24 | n: no two buckets give one ID". Then the restore's 11 trips go next door to
            bucket 2; "0 round trips across", "N round trips, all in europe-west1"; the sandbox smiles.
            "Every cell's ID carries its bucket" / "Its home is bucket 2, next door". [objectid.rs module
            doc; CONTEXT.md Bucket number, Cell; doc-23 "Bucket numbers and cell IDs". Block 5 and n 9 are
            illustrative; the shape (4 + 6 + 6 hex digits = 16/24/24 bits) is the code's. The restore asks
            its own bucket when the version is in it: the same 11 requests, in-region.]
  S4 32-44  Another cell, mint, in europe-west1, whose parent (lilac, v1 flag, ID tag `0001 000003
            000004`) lives in bucket 1; the fork thread over the ocean carries "forks across buckets: in
            progress". A read pill `0001…` leaves the child, stops at bucket 2's "registry copy" (row 1
            lights; note "bucket 1, found in this host's bucket's registry copy"), a key and the tag
            `CLADEFSD_BUCKET_1_AWS_ACCESS_KEY_ID=…` pop, the pill crosses on an accent route to pail 1,
            "identity checked". "Another cell's parent is in bucket 1" / "The number finds the bucket" /
            "With this host's own credentials". [TASK-633.3 (done): own copy, else the root's;
            `CLADEFSD_BUCKET_<N>_<NAME>` (`numbered_options`, object_bucket.rs) strips the prefix, and `cloud()` hands
            `aws_access_key_id` to object_store's S3 builder, so the access key shown is a real credential
            variable; the registry copy read is the asking host's bucket's (object_bucket.rs `entry`); ESTALE on a
            mismatched identity; `hosts_find_buckets_by_number` (crates/cladefs/src/host_cell.rs).
            TASK-633.4 (done): reads of inherited extents routed to the parent's bucket,
            `children_in_one_bucket_read_their_parents_in_another` (bucket_cell object_bucket.rs).
            TASK-633.7 in progress: the fork across buckets itself, so tagged.]
  S5 44-55  Hosts A, B, C of europe-west1, each with a part-cache shelf. One part crosses the ocean to host
            B; "fetched across the ocean: 1"; note "its owner fetches it into its part cache". Hosts A
            and C ask B and get it; the count stays 1. Host D of us-east-1 sends "home bucket 1" to host A:
            it bounces back, "refused"; note "a peer refuses another home bucket's request". "A part
            crosses once, not once per host" / "Peers share it inside the cloud region" / "Peers never
            cross a cloud region". (doc-23: about once per region; the part cache evicts.) [TASK-633.4 AC 5 and 6 (done); doc-23 "Routing by bucket"; tests
            `a_peer_of_another_region_is_never_asked_again`, `peers_serve_their_region_alone`
            (crates/cladefs/src/bucket/peers/tests.rs).]
  S6 55-64  Stamped "in progress" for the whole shot. The mint child reads from bucket 1 (dashed thread,
            "reads: bucket 1"); four of the version's objects hop over the ocean into bucket 2 (note
            "copied in the background, byte for byte, at the same keys"); the marker flag lands on pail 2
            and the thread swings to it ("reads: bucket 2"); a second child (sky) pops and reads the same
            copy (note "one copy for every child of the version there", from 5.5 s into the shot). "In progress: the version copied
            over" / "Then its reads stay at home". [TASK-633.8 (in progress, nothing on main); doc-23
            "Mirrors": automatic, same keys, the marker the switch, one mirror per version and bucket.]
  S7 64-72  S1 again: the frowning sandbox, pail 1, "Remember the ocean?", one trip across, "11 round trips
            across". Bucket 2 pops beside, the trips go next door, the count steps 11 to 0, the sandbox
            smiles; the peach cell pops and hops onto pail 2, landing on the last chord (70.0 s). "Each
            cloud region, its own bucket".
BANNED:  the overview's: an end card or wordmark; the word "VM" on screen (say "sandbox"); calling
         cladefs "a page cache"; any franchise's creatures; dark void, glow, gradients, glass, everything
         centred, bullet lists, icons in circles, legends. And for this film: "snapshot" (say version),
         "region" alone where the DAX region could be meant in a title (say cloud region at least once
         per scene that names one), "remote" for a bucket, "cache" alone (say part cache), a map of the
         real world, claiming mirrors or forks across buckets as shipped, any seconds or prices on
         screen (doc-23's are estimates).
QA:      every scene start, middle and seam; the trip counter at 7 s reads 11 and at 68 s reads 0; every
         "in progress" tag readable while its subject is on screen; phone width.
