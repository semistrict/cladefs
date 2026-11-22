------------------------------ MODULE Bucket ------------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Cells in object storage (backlog doc-7, crates/bucket_cell) with the    *)
(* host's write-ahead log of doc-14 (TASK-165.2): at the level of the      *)
(* protocol, what each step reads and writes in the bucket, and in what    *)
(* order. The simulation (bucket_cell's sim) samples interleavings, and    *)
(* found TASK-79.18 and 79.3 only after the fact; TLC exhausts small ones. *)
(*                                                                         *)
(* Keys are written once (TASK-407): every object a writer writes is at a  *)
(* key naming its writer's epoch, and becomes part of its cell only when   *)
(* the cell's head names it. The head, one object per cell, is the         *)
(* writer's taking and its commit: its epoch, the token and holder of its  *)
(* taking, the segments committed (`end`, and each one's epoch: `chain`),  *)
(* the newest checkpoint (`ckpt`) and the versions (`vers`), swapped with  *)
(* a condition (S3 If-Match, GCS generation). A takeover swaps it to the   *)
(* next epoch, and the writer taken over fails at its next swap: the head  *)
(* is the fence. Readers read the head, then what it names, and nothing    *)
(* else; an object the head does not name, of an epoch below its, never    *)
(* will be named, and the collector deletes it. No key is written twice,   *)
(* so readers need no versions of a key (KeysWrittenOnce).                 *)
(*                                                                         *)
(* The head is one object: read whole, swapped in place with a condition  *)
(* on the version its writer last saw (swapsInPlace), at most as often as  *)
(* its bucket's store takes writes of one object (TASK-690: GCS's one a    *)
(* second), each swap naming all that landed since, and a checkpoint       *)
(* landed meanwhile: a group commit, which changes no step's effect here.  *)
(* The bucket kept it in N slots for GCS (TASK-691, specs/head-slots), one *)
(* again since doc-27; the head is never a create-only key per version,    *)
(* which a writer of a cell removed would create anew.                     *)
(*                                                                         *)
(* The bucket holds, per cell: segments, at <<seq, epoch>>, each a footer  *)
(* naming its writer's epoch, one commit (or none), an extent sealed into  *)
(* it or stored on its own, an extent freed, the cell's next sequence      *)
(* number, the version its commit makes, and the run of a commit it goes   *)
(* on with, which a later segment ends; extents stored on their own, at    *)
(* keys naming their writer's epoch and the segment that names them;       *)
(* checkpoints, a state as of a segment, at <<seq, epoch>>; its record;    *)
(* its head; and the references of its versions' forks, and their marks.   *)
(* A segment's PUT is in flight from its upload until it lands, whenever,  *)
(* though its writer has gone.                                             *)
(*                                                                         *)
(* Hosts run writers. Opening a cell swaps its head to the next epoch and  *)
(* starts its recovery (the head's checkpoint, or the origin's state),     *)
(* then reads the segments the head names; then the writer mints from its  *)
(* epoch's range, and puts its WAL after the cell's end (doc-14, Opening a *)
(* cell: wal::rebase): the WAL's segments the head names are               *)
(* acknowledged, and the rest restamped after the head's end, if the       *)
(* cell's last segment is the last of those, or the cell's last commit the *)
(* commit the WAL started from; otherwise the WAL diverged, and is         *)
(* dropped, as `cladefsctl take` drops it. A commit is a segment of the    *)
(* WAL, stamped by the chain; or, spanning segments (TASK-92.41), segments *)
(* going on with it, a run of it in each, then the one it ends in. Readers *)
(* hold such runs until that one; writers acknowledge, and so heads name,  *)
(* only between commits. An upload creates its extent of its own, then     *)
(* puts its segment in flight; an acknowledgement swaps the head to name   *)
(* the WAL's prefix landed. A host crashes keeping its WALs, to their last *)
(* durable commits with "cut" (local recovery keeping commits whole), or   *)
(* losing them. Placements give hosts fencing tokens. Cells are deleted by *)
(* their placements (the head swapped to the final epoch, then the record  *)
(* marked deleted: lifecycle::delete), and forks of versions failing by    *)
(* their restores.                                                         *)
(*                                                                         *)
(* Versions (TASK-476, doc-18): a version is a name for a cell's state as  *)
(* of a segment its head names, read-only, kept while the head names it,   *)
(* in the cell's own prefix and of no record. Its commit, of a quiescent   *)
(* cell, sealing nothing, closes a segment whose footer names it (its ID,  *)
(* the commit's tag here); the acknowledgement naming the segment names it *)
(* in the head; its writer drops it by a swap; a deletion's final head     *)
(* names none. The only fork is a version's (a restore), into a new cell,  *)
(* its ID an allocator's: the child's record first, saying the fork is     *)
(* being made, then its reference, then the head naming the version read,  *)
(* then the record made. Cells' cuts, versions of several cells cut at one *)
(* moment, are a host's work alone (doc-18): not modelled.                 *)
(*                                                                         *)
(* The collector runs passes per cell (TASK-395), each under its cell's    *)
(* collector lease, at once with other cells' and with writers: the cell's *)
(* listing, its record, the cell recovered from its head if it lives, its  *)
(* state at each version the head names kept, its references listed again, *)
(* each one's origin's state kept, then deletion within the cell; the      *)
(* cell's own reference retired once it is deleted and nothing             *)
(* in it lives, and the cell removed whole once dead (TASK-455). A         *)
(* reference is under the cell of the version forked, its parent, and      *)
(* names the parent, the version's segment and the child: it keeps the     *)
(* parent's whole state there while it stands, whatever the child frees of *)
(* it since: the collector is conservative.                                *)
(*                                                                         *)
(* Writers compact (doc-27): a writer relocates live extents of its own    *)
(* cell's acknowledged segments into a new segment of its own, a commit    *)
(* like any other, which the head names as it names any; the collector     *)
(* moves nothing, holds no key, and deletes what no state it keeps reads.  *)
(* Gone with the collector's moves: their keys naming their passes, the    *)
(* collector keeping every moves on the way from an extent's seal, and     *)
(* writers naming what their checkpoints moved before writing them         *)
(* (movesByPass, waysFromSeals, movesTakenFirst; TASK-407.1, TASK-175).    *)
(*                                                                         *)
(* Each rule is a member of Rules; specs/bucket/mutations removes one      *)
(* each, and must find the violation it prevents (each mutation's model    *)
(* says which, and how):                                                   *)
(*                                                                         *)
(* - headSwap (NoHeadSwap): a writer acknowledges what it uploaded only by *)
(*   swapping the head with a condition, its own taking's: without it, a   *)
(*   writer taken over writes the head over its successor's, and the       *)
(*   successor's acknowledged commits are gone (TASK-407).                 *)
(* - swapsInPlace (NoSwapsInPlace): a swap rewrites the one head object,   *)
(*   conditional on the version there, so that a swap of a head removed    *)
(*   with its dead cell fails. Without it, each version of the head is a   *)
(*   create-only key of its own (head/<k>), which a create of the next     *)
(*   finds free once the collector removed the cell whole, no tombstone    *)
(*   left: a writer slow to swap brings the removed cell back              *)
(*   (RemovedStayGone; specs/head-slots's slotsAtBirth found the same of   *)
(*   slots created as their turns came).                                   *)
(* - epochKeys (NoEpochKeys): a segment's and a checkpoint's key names its *)
(*   writer's epoch: without it, two writers' PUTs land at one key, the    *)
(*   later over the earlier, which a head names (KeysWrittenOnce).         *)
(* - collectorEpochs (NoCollectorEpochs): the collector keeps, of a live   *)
(*   cell's objects its head does not name, those of the head's epoch that *)
(*   it may yet name: segments and extents of their own at or past its     *)
(*   end, checkpoints newer than its own; all others are deleted, of an    *)
(*   epoch no swap of whose can succeed.                                   *)
(* - epochSequences (NoEpochSequences): a writer mints from its epoch's    *)
(*   range, having found the head its taking's after recovering.           *)
(* - candidatesFirst (NoCandidatesFirst): the collector lists before it    *)
(*   reads the record and heads. Without it, a pass that read no record    *)
(*   lists the one a restore made since, and removes the cell whole, the   *)
(*   fork's reference left, which no record names (ReferencesNamed).       *)
(* - tokens (NoTokens): a head is taken only with a token as high.         *)
(* - deleteTokens (NoDeleteTokens): a cell is deleted only with a token as *)
(*   high as its head's.                                                   *)
(* - forksCheck (NoForksCheckVersions): a fork of a version reads the head *)
(*   naming the version after it creates its child's record.               *)
(* - forksPending (NoForksPending): that record says the fork is being     *)
(*   made until then.                                                      *)
(* - collectorAbandons (NoCollectorAbandons): the collector gives up on    *)
(*   forks being made of versions dropped.                                 *)
(* - walFollows (NoWalFollows): a WAL is put after the cell only if the    *)
(*   head's last segment is the WAL's last it names, or, naming none of    *)
(*   them, the cell's last commit is the commit the WAL started from.      *)
(* - acksBetweenCommits (NoAcksBetweenCommits): a writer acknowledges      *)
(*   segments only to the last going on with no commit                     *)
(*   (Writer::acknowledge), so that its WAL keeps a commit whole until the *)
(*   bucket has it whole, and a head names no commit begun.                *)
(* - wholeCommitsKept (NoWholeCommitsKept): a host's local recovery keeps  *)
(*   a commit spanning segments only if consecutive segments end it, and   *)
(*   closes the first again without it otherwise (local_cell's             *)
(*   wal::recover): a cell whose log ends inside a commit refuses to       *)
(*   open.                                                                 *)
(* - references (NoReferences): a fork of a version has a reference under  *)
(*   its parent's cell, which that cell's passes keep the parent's state   *)
(*   at the version for: they list nothing of the child's cell, and the    *)
(*   parent deleted, or the version dropped, would delete what the child   *)
(*   inherits.                                                             *)
(* - forksReferFirst (NoForksReferFirst): a fork of a version writes its   *)
(*   reference before it reads the head naming the version: one that read  *)
(*   the version live then wrote it before the version's drop, or its      *)
(*   cell's deletion, which a pass reading the version dropped, or finding *)
(*   its reference retired, read after.                                    *)
(* - referencesHeld (NoReferencesHeld): a reference is retired, by the     *)
(*   pass of its child's cell, only once that cell is dead, its record     *)
(*   deleted and no reference of its own: the forks of its versions        *)
(*   inherit what it did, kept by its reference.                           *)
(* - referencesRelisted (NoReferencesRelisted): a pass lists its cell's    *)
(*   references after the states: listed with the candidates, it misses a  *)
(*   fork's of a version a checkpoint dropped since.                       *)
(* - originsKept (NoOriginsKept): a pass keeps, of the parent's state at a *)
(*   referenced origin, what recovering it reads, not only the objects     *)
(*   holding its extents: a child with no checkpoint of its own recovers   *)
(*   through it.                                                           *)
(* - forksRecordFirst (NoForksRecordFirst): a fork of a version creates    *)
(*   its record, being made, before its reference (and reads the head      *)
(*   naming the version after both, forksReferFirst): a reference always   *)
(*   has a record naming its origin, by which its child's cell's pass      *)
(*   retires it, whatever comes of the fork. Written first, a host dying   *)
(*   between leaves a reference no record names, which no pass retires     *)
(*   (ReferencesNamed).                                                    *)
(* - retirementsMarked (NoRetirementsMarked): a reference retired is       *)
(*   marked, a mark kept for good under its parent's cell beside it,       *)
(*   before it is deleted; a pass deletes the references of its cell it    *)
(*   lists marked. The restore of a fork never made, slow, may write it    *)
(*   after its retirement (TASK-395.14), which its child's cell's passes,  *)
(*   the cell dead and unchanged, or removed, never retire again; its      *)
(*   parent's cell's next pass deletes it, by its mark                     *)
(*   (ReferencesRetiredMarked). A fork's made by its restore alone goes    *)
(*   unmarked: its one writer wrote it before it made the fork, so that    *)
(*   the parent's cell keeps no mark of a fork made and gone (TASK-455).   *)
(*   Without the rule, none is marked.                                     *)
(* - versionsKept (NoVersionsKept): a pass keeps, for each version a live  *)
(*   cell's head names, the cell's state at it whole, and what recovering  *)
(*   it reads, as for an origin (TASK-476, doc-18). Without it, a          *)
(*   checkpoint since covers the version's segments, which the pass        *)
(*   deletes (NothingNeededGone).                                          *)
(* - versionsCarried (NoVersionsCarried): a takeover's swap names the      *)
(*   versions the head named, as it names its segments. Without it, they   *)
(*   are gone though no writer dropped them (VersionsStay).                *)
(* - versionsAcked (NoVersionsAcked): a head names a version only with the *)
(*   segment its commit closed, by the acknowledgement naming that         *)
(*   segment. Without it, a swap names it as its commit is made, before    *)
(*   its segment lands, a state the head does not hold                     *)
(*   (NothingNeededGone).                                                  *)
(*                                                                         *)
(* Gone with writable forks (one log per cell): forks a writer's commit    *)
(* named in its segment's footer, their children recorded after the        *)
(* acknowledgement by the writer, the next or the parent's deletion, their *)
(* references written first and taken back; failed forks' deletions and    *)
(* their claims; IDs guests named; deletions recovering the cell to record *)
(* its forks; the collector finding forks in their parents' states,        *)
(* recovering cells by each origin, listing records again, keeping records *)
(* and deletions' fences for children whose records were yet to come, and  *)
(* stopping a pass fenced on what it found lost, as it read records before *)
(* the states (TASK-180); and the rules of those.                          *)
(*                                                                         *)
(* Gone with the head (TASK-407), with what they guarded: a create where   *)
(* nothing is as a writer's fence, the fence segment a takeover made, gaps *)
(* filled, void and late segments, the collector's pins of where a writer  *)
(* taken over could still create (TASK-388), readers listing checkpoints   *)
(* again, a deletion's fence and its read of the record again, readers     *)
(* going by the footer of the object at an extent's key rather than their  *)
(* state (TASK-79.18), the collector keeping extents of their own by the   *)
(* segments their keys name, the WAL's rules for fences, and readers       *)
(* holding a commit's runs as a rule, as a fence could leave it unended: a *)
(* head names only what its writer acknowledged, whole commits, and a key  *)
(* only one object.                                                        *)
(*                                                                         *)
(* Where this differs from doc-14 and the code, by what TLC found:         *)
(*                                                                         *)
(* - doc-14 put the WAL after the cell by segments: WAL segments whose     *)
(*   footers recovery read were acknowledged. Here by commits, as doc-7's  *)
(*   recovery on the same host was.                                        *)
(* - Ahead of the code (doc-27, Wave 2): writers relocate, the collector   *)
(*   compacts nothing and keeps no moves, an acknowledgement names a       *)
(*   checkpoint landed, and the head is one object everywhere. Until       *)
(*   doc-27's steps land, bucket_cell's collector still compacts into      *)
(*   moves (the spec before this change has their rules), and GCS buckets  *)
(*   keep their head in slots (specs/head-slots).                          *)
(*                                                                         *)
(* Steps are what the code does between two requests, or merged: a         *)
(* writer's taking swaps the head, the one object saying whether its cell *)
(* lives (TASK-515), in one step; it reads the head's checkpoint then, and *)
(* the segments the head names in a step after. The collector's recoveries *)
(* read the bucket at once. A checkpoint's PUT lands in a step of its own, *)
(* while its writer's session lasts, and the head names it in another: a   *)
(* swap of its own, or the acknowledgement after it. A relocation is a     *)
(* commit, made, uploaded and acknowledged as any. Passes overlap if       *)
(* "overlap" is in                                                         *)
(* Can, of any cells, a later one of a cell taking its collector lease;    *)
(* MaxPasses bounds them all. A pass lists its cell; reads its head,       *)
(* giving up on a fork being made, and recovers the cell in a step after;  *)
(* lists its references in the next (referencesRelisted); then deletes.    *)
(* Reads of other cells' objects (a parent's state, a                      *)
(* version's head) are a pass's GETs; it lists only its own cell, and      *)
(* deletes only there, but for the reference it retires, whose key its     *)
(* record gives. Listings are atomic, as before.                           *)
(*                                                                         *)
(* Can bounds what happens: each kind of step needs its name in it, so     *)
(* that a model checks some at a time and a mutation only what it needs.   *)
(* A commit spans three segments at most, two going on with it, each a     *)
(* run of one                                                              *)
(* extent, the one ending it holding nothing else (the code's may hold its *)
(* frees and more). A segment ends a commit and goes on with the next only *)
(* with "spanOn" in Can. Not modelled: a writer's compaction threshold and *)
(* budget (it relocates any extent of its own segments it may, by choice), *)
(* the spacing of a head's swaps, and PUTs failing.                        *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Hosts,          \* the hosts
    Cells,          \* cell IDs
    Root,           \* the cell the bucket starts with
    Segs,           \* segments per cell: 0..Segs-1
    MaxTakes,       \* head takings, all cells'
    MaxCommits,     \* commits
    MaxMints,       \* extent IDs minted
    MaxPasses,      \* passes of the collector
    MaxPlaces,      \* placements
    MaxCrashes,     \* host crashes
    Can,            \* the kinds of step the model takes
    None,           \* a model value: nothing
    Rules           \* the protocol's rules in force (all, but in a mutation)

AllRules == {"headSwap", "swapsInPlace", "epochKeys", "collectorEpochs", "epochSequences",
             "candidatesFirst", "tokens", "deleteTokens", "forksCheck", "forksPending",
             "collectorAbandons", "walFollows", "acksBetweenCommits", "wholeCommitsKept",
             "references", "forksReferFirst", "referencesHeld", "referencesRelisted",
             "originsKept", "forksRecordFirst", "retirementsMarked",
             "versionsKept", "versionsCarried", "versionsAcked"}

AllCan == {"commit", "mint", "seg", "own", "free", "relocate", "restore", "delete", "place",
           "checkpoint", "collect", "crash", "disk", "overlap", "span", "spanOn", "cut",
           "deleteRoot", "landInOrder", "version", "dropVersion"}

ASSUME Rules \subseteq AllRules
ASSUME Can \subseteq AllCan
ASSUME Root \in Cells

Seqs == 0..(Segs - 1)
\* The epochs writers take, and the final one, a deletion's, which no writer takes.
Epochs == 1..MaxTakes
Final == MaxTakes + 1

R(rule) == rule \in Rules

\* The epoch a key names: its writer's (epochKeys); without the rule, none, every writer's at one.
KE(e) == IF R("epochKeys") THEN e ELSE 1

VARIABLES
    \* The bucket.
    seg,        \* seg[b][s][e]: segment s of b by the writer of epoch e, or None
    own,        \* the extents stored on their own: [b, e, s, x], at the key of writer e's segment s
    ckpt,       \* ckpt[b][s][e]: b's checkpoint as of segment s by the writer of epoch e, or None
    head,       \* head[b]: b's one swapped object (TASK-515), or None: what made it (a Record's
                \* fields), and [epoch, token, holder, end, chain, ckpt, vers]: epoch 0 never taken,
                \* Final deleted; the holder its taking's, unique; the segments 0..end-1 committed,
                \* chain[s + 1] the epoch of s; ckpt the newest checkpoint, <<s, e>>, or <<>>; vers
                \* its versions, each [id, at]: the ID its commit's footer names (its tag), and the
                \* segment it is at
    inflight,   \* the segments' PUTs in flight: [h, b, g, s, o]
    refs,       \* the references of versions' forks, [p, at, c]: under p's cell, naming p, the
                \* version's segment at, and the child c
    marked,     \* the marks of the references retired, [p, at, c], under p's cell, kept
                \* (retirementsMarked): a reference marked is garbage, written again or not
    retired,    \* what happened: the references retired (kept with the bucket's variables, as it
                \* changes with them)
    written,    \* what happened: every <<key, object>> written at a segment's or a checkpoint's
                \* key (KeysWrittenOnce)
    gone,       \* what happened: the cells a pass removed whole, whose IDs, an allocator's, no
                \* restore takes again
    \* The hosts.
    ses,        \* ses[h][b]: h's writer of b, or None
    wal,        \* wal[h][b]: h's WAL of b, a sequence of [s, o]: kept by a crash, or lost
    tok,        \* tok[h][b]: the fencing token of h's placement of b
    proc,       \* proc[h]: h's deletion or restore, or None
    \* The collector.
    gc,         \* gc[i]: pass i, of the cell gc[i].cell, whose collector lease epoch is i: a cell's
                \* collector lease is its last pass's
    \* What happened.
    commits,    \* commits[t]: [cell, parent, seals, free, ver, dropped]: its cell, the commit before
                \* it, what it sealed and freed, whether it makes a version, and whether dropped
    nextTag,
    acked,      \* acked[b]: the commits a writer of b acknowledged
    minted,     \* the extent IDs minted
    twice,      \* whether an ID was minted twice
    makers,     \* makers[b]: the makings of b told they made it
    doomed,     \* the makings a deletion of was asked by whoever may
    relocated,  \* what happened: the segments a writer relocated an extent out of
    taken,      \* taken[b]: the highest token that took b's head, -1 if none did
    placed,     \* placed[b]: the last placement's token
    errors,     \* what failed that must not
    \* Bounds.
    gen, takes, passes, places, crashes, mints

bucketVars == <<seg, own, ckpt, head, inflight, refs, marked, retired, written, gone>>
hostVars == <<ses, wal, tok, proc>>
truthVars == <<commits, nextTag, acked, minted, twice, makers, doomed, relocated, taken, placed,
              errors>>
countVars == <<gen, takes, passes, places, crashes, mints>>
vars == <<bucketVars, hostVars, gc, truthVars, countVars>>

-----------------------------------------------------------------------------
(* Values. *)

Max(S) == CHOOSE x \in S : \A y \in S : x >= y
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
\* Pairs <<epoch, n>>, ordered as sequence numbers are: the epoch's range, then n; and keys
\* <<s, e>> of checkpoints, by s, then e.
GE2(a, b) == a[1] > b[1] \/ (a[1] = b[1] /\ a[2] >= b[2])
Max2(a, b) == IF GE2(a, b) THEN a ELSE b

Put(f, x, v) == [y \in DOMAIN f \cup {x} |-> IF y = x THEN v ELSE f[y]]
Drop(f, x) == [y \in DOMAIN f \ {x} |-> f[y]]
Nothing == [x \in {} |-> None]

\* A record: its kind ("root", "forking", "fork": a version's fork being made, or made), its origin
\* <<parent, at>>, the version a fork is of, <<cell, id>> (<<>> if none); and, not in the bucket's
\* bytes, the commit its cell starts from and the making it is of.
Record(kind, origin, o, base) ==
    [kind |-> kind, origin |-> origin, ver |-> <<>>, base |-> base, maker |-> o,
     epoch |-> 0, token |-> 0, holder |-> 0, end |-> 0, chain |-> <<>>, ckpt |-> <<>>, vers |-> {}]

\* Whether the head x is a deleted cell's: of the final epoch.
Deleted(x) == x.epoch = Final

\* The keys of segment s of b by the writer of epoch e, and of b's checkpoint as of s by that
\* writer.
SegKey(b, s, e) == <<"seg", b, s, KE(e)>>
CkptKey(b, s, e) == <<"ckpt", b, s, KE(e)>>

\* The object at the key k of a segment or a checkpoint, or None.
Obj(k) == CASE k[1] = "seg" -> seg[k[2]][k[3]][k[4]]
            [] k[1] = "ckpt" -> ckpt[k[2]][k[3]][k[4]]

\* A state: its extents' locations (the keys holding them), its last commit, its next sequence
\* number, the last segment applied, the runs of the commit the segments applied last went on with
\* (State::begun: [commit, runs], runs the locations of its extents; or None), and, not in the
\* bucket's bytes, the commits applied and whether one was applied twice.
Fresh == [ext |-> Nothing, tip |-> 0, next |-> <<0, 0>>, through |-> -1, begun |-> None,
          hist |-> {}, dup |-> FALSE]

\* A fork's child's state before its segments: what it inherits (State::inheriting: no runs
\* begun).
Inherit(st) == [Fresh EXCEPT !.ext = st.ext, !.tip = st.tip, !.hist = st.hist]

\* Whether segment o, following one that went on with commit t, ends it (Contents::ends): it
\* names it. The code's names a commit of t's writer, t or a later one, the last of those its
\* segment ends; here a segment ends one commit at most, and a commit's segments are its writer's,
\* one after another, so that the one after t's last continuation names t.
Ends(o, t) == o.commit = t
\* Whether it goes on with t, ending no commit (Contents::continues).
Continues(o, t) == o.commit = 0 /\ o.cont # None /\ o.cont.commit = t

\* The locations f, those of g sealed over them.
Over(f, g) == [x \in DOMAIN f \cup DOMAIN g |-> IF x \in DOMAIN g THEN g[x] ELSE f[x]]

\* st, segment s of b, whose object is o, applied. First the runs of the commit the segments
\* before went on with: sealed, if o ends it; else held on (a head names a writer's segments only
\* between its commits, so that o goes on with it). Then o's own; then, if o goes on with a commit,
\* its run held until a segment ends it (State::apply_contents). A state a head or a checkpoint
\* names ends between commits, so that it holds none begun; a WAL's may (wholeCommitsKept). Since
\* the head (TASK-407), no rule: sealed at once, the runs would end the same, the segment ending
\* their commit always named with them (spansHeld's mutation found nothing).
Apply(st, b, s, o) ==
    LET bg == st.begun
        at == SegKey(b, s, o.epoch)
        ended == bg # None /\ Ends(o, bg.commit)
        held == IF ended THEN None ELSE bg
        e0 == IF ended THEN Over(st.ext, bg.runs) ELSE st.ext
        e1 == IF o.ext # None THEN Put(e0, o.ext, at) ELSE e0
        e2 == IF o.own # None THEN Put(e1, o.own, <<"own", b, o.epoch, s>>) ELSE e1
        e3 == IF o.free # None /\ o.free \in DOMAIN e2 THEN Drop(e2, o.free) ELSE e2
        run == IF o.cont = None THEN Nothing ELSE Put(Nothing, o.cont.ext, at)
        begun == IF o.cont = None THEN held
                 ELSE IF held = None THEN [commit |-> o.cont.commit, runs |-> run]
                 ELSE [held EXCEPT !.runs = Over(@, run)]
    IN [st EXCEPT !.ext = e3,
                  !.begun = begun,
                  !.tip = IF o.commit # 0 THEN o.commit ELSE st.tip,
                  !.hist = IF o.commit # 0 THEN st.hist \cup {o.commit} ELSE st.hist,
                  !.dup = st.dup \/ o.commit \in st.hist,
                  !.next = Max2(st.next, o.next),
                  !.through = s]

RECURSIVE ApplyAll(_, _, _)
\* st, the WAL ws applied.
ApplyAll(st, b, ws) ==
    IF ws = <<>> THEN st ELSE ApplyAll(Apply(st, b, ws[1].s, ws[1].o), b, Tail(ws))

CommitsOf(ws) == {ws[i].o.commit : i \in DOMAIN ws} \ {0}

-----------------------------------------------------------------------------
(* Recovery, as one read of the bucket: what invariants and the collector see. *)

RECURSIVE FollowChain(_, _, _, _, _, _)
\* st, b's segments from s to before `to` applied, each at the key of the epoch chain names, every
\* one there, and what that read.
FollowChain(b, st, s, to, chain, t) ==
    IF s >= to THEN [ok |-> TRUE, st |-> st, touched |-> t]
    ELSE LET k == SegKey(b, s, chain[s + 1]) IN
         IF Obj(k) = None THEN [ok |-> FALSE, st |-> st, touched |-> t]
         ELSE FollowChain(b, Apply(st, b, s, Obj(k)), s + 1, to, chain, t \cup {k})

\* The head of b's fields, none if it has no head.
End(b) == IF head[b] = None THEN 0 ELSE head[b].end
Chain(b) == IF head[b] = None THEN <<>> ELSE head[b].chain
HeadCkpt(b) == IF head[b] = None THEN <<>> ELSE head[b].ckpt
HeadEpoch(b) == IF head[b] = None THEN 0 ELSE head[b].epoch
HeadVers(b) == IF head[b] = None THEN {} ELSE head[b].vers

\* Whether the version v, <<cell, id>>, is gone: its cell's head names it no more, dropped by its
\* writer or ended by a deletion, or never did.
VersionGone(v) == ~\E x \in HeadVers(v[1]) : x.id = v[2]

\* b's checkpoints listed as of segment `at` or before: <<s, e>>.
CkptsTo(b, at) == {k \in Seqs \X Epochs : k[1] <= at /\ ckpt[b][k[1]][k[2]] # None}
\* The newest of the checkpoints K.
Newest(K) == CHOOSE k \in K : \A j \in K : GE2(k, j)

RECURSIVE StateAt(_, _)
\* b's state as of its segment at, which its head names: its newest checkpoint to there, listed,
\* or its origin's state, then the segments its head names to there; or a failure, something gone.
StateAt(b, at) ==
    LET cs == CkptsTo(b, at) IN
    IF End(b) <= at THEN [ok |-> FALSE, st |-> Fresh, touched |-> {}]
    ELSE IF cs # {} THEN
       LET c == Newest(cs)
           k == <<"ckpt", b, c[1], c[2]>>
       IN FollowChain(b, Obj(k), c[1] + 1, at + 1, Chain(b), {k})
    ELSE IF head[b] = None THEN [ok |-> FALSE, st |-> Fresh, touched |-> {}]
    ELSE IF head[b].origin = <<>> THEN FollowChain(b, Fresh, 0, at + 1, Chain(b), {<<"head", b>>})
    ELSE LET p == StateAt(head[b].origin[1], head[b].origin[2]) IN
         IF ~p.ok THEN p
         ELSE FollowChain(b, Inherit(p.st), 0, at + 1, Chain(b), p.touched \cup {<<"head", b>>})

Lost == [ok |-> FALSE, st |-> Fresh, end |-> 0, touched |-> {}, ckpt |-> <<>>]

\* b's state, forked from origin, as its head names it: the head's checkpoint, or its origin's
\* state, then the segments the head names after it. Something named gone, a failure.
Recover(b, origin) ==
    LET newest == HeadCkpt(b)
        with(r) == [ok |-> r.ok, st |-> r.st, end |-> End(b), touched |-> r.touched,
                    ckpt |-> newest]
    IN IF newest # <<>> THEN
          LET k == CkptKey(b, newest[1], newest[2]) IN
          IF Obj(k) = None THEN Lost
          ELSE with(FollowChain(b, Obj(k), newest[1] + 1, End(b), Chain(b), {k}))
       ELSE IF origin = <<>> THEN with(FollowChain(b, Fresh, 0, End(b), Chain(b), {}))
       ELSE LET p == StateAt(origin[1], origin[2]) IN
            IF ~p.ok THEN Lost
            ELSE with(FollowChain(b, Inherit(p.st), 0, End(b), Chain(b), p.touched))

\* Whether b's head is the taking of the writer whose session is s: its epoch, and its holder,
\* as its writer last swapped it (an ETag's condition).
Taking(s, b) == head[b] # None /\ head[b].epoch = s.epoch /\ head[b].holder = s.gen

\* Whether segment o's footer names a run of x in its data: of a commit it ends, or of the one it
\* goes on with (Contents::runs).
Runs(o, x) == o.ext = x \/ (o.cont # None /\ o.cont.ext = x)

\* Whether a reader of extent x at loc reads x's bytes: the object at loc's key, there, names it.
Readable(x, loc) ==
    IF loc[1] = "own" THEN [b |-> loc[2], e |-> loc[3], s |-> loc[4], x |-> x] \in own
    ELSE Obj(loc) # None /\ Runs(Obj(loc), x)

-----------------------------------------------------------------------------
(* The truth. *)

RECURSIVE Anc(_)
\* The commits up to t.
Anc(t) == IF t = 0 THEN {} ELSE {t} \cup Anc(commits[t].parent)

RECURSIVE OwnStart(_, _)
\* Where the path of b's own commits to t starts: the commit before the first.
OwnStart(b, t) == IF t # 0 /\ commits[t].cell = b THEN OwnStart(b, commits[t].parent) ELSE t

RECURSIVE Path(_)
\* The commits to t, oldest first.
Path(t) == IF t = 0 THEN <<>> ELSE Append(Path(commits[t].parent), t)

RECURSIVE MadeBy(_, _)
\* The extents the commits of the path p leave.
MadeBy(p, held) ==
    IF p = <<>> THEN held
    ELSE LET c == commits[p[1]]
         IN MadeBy(Tail(p), IF c.free = None THEN held \cup c.seals
                            ELSE (held \cup c.seals) \ {c.free})

Made == UNION {makers[b] : b \in Cells}

\* A writer's guest's state: what it acknowledged, then its WAL.
View(h, b) == ApplyAll(ses[h][b].st, b, wal[h][b])

\* Whether b's head says it lives: a root, a fork made.
LiveRec(b) == head[b] # None /\ ~Deleted(head[b]) /\ head[b].kind \in {"root", "fork"}

\* The reference of the fork of p's version at its segment at to c, under p's cell (TASK-395).
RefOf(p, at, c) == [p |-> p, at |-> at, c |-> c]

\* The references under cell c.
RefsUnder(c) == {r \in refs : r.p = c}

-----------------------------------------------------------------------------
(* Writers. *)

Stopped(h, b) == ses' = [ses EXCEPT ![h][b].pc = "stopped"]

\* The head of b the writer whose session is s writes: its taking, with what it committed. Without
\* swapsInPlace, a create of the next version's key where the head was removed: the head the writer
\* took, with what it committed.
HeadOf(s, b) == [IF head[b] = None THEN s.rec ELSE head[b]
                    EXCEPT !.epoch = s.epoch, !.token = s.token, !.holder = s.gen, !.end = s.end,
                           !.chain = s.chain, !.ckpt = s.hckpt, !.vers = s.vers]

\* Whether the writer whose session is s may swap b's head: it is its taking still (headSwap); or,
\* without swapsInPlace, the head is gone, removed with its cell, and the create of the next
\* version's key finds it free.
Swappable(s, b) == Taking(s, b) \/ (~R("swapsInPlace") /\ head[b] = None)

\* s, the session of a writer of b, its recovery started from b's head H as it swapped it: from
\* the head's checkpoint, or its origin's state; stopped, what that needs gone.
Started(s, b, H) ==
    LET p == StateAt(s.origin[1], s.origin[2])
        newest == H.ckpt
        s1 == [s EXCEPT !.end = H.end,
                        !.chain = H.chain,
                        !.hckpt = newest,
                        !.vers = IF ~R("versionsCarried") THEN {} ELSE H.vers]
    IN IF newest # <<>> THEN
          IF Obj(CkptKey(b, newest[1], newest[2])) = None THEN [s1 EXCEPT !.pc = "stopped"]
          ELSE [s1 EXCEPT !.pc = "read", !.st = Obj(CkptKey(b, newest[1], newest[2])),
                          !.at = newest[1] + 1]
       ELSE IF s.origin = <<>> THEN [s1 EXCEPT !.pc = "read", !.st = Fresh, !.at = 0]
       ELSE IF p.ok THEN [s1 EXCEPT !.pc = "read", !.st = Inherit(p.st), !.at = 0]
       ELSE [s1 EXCEPT !.pc = "stopped"]

\* h takes b's head, as a writer of b, of its placement's token, its taking's holder, in one
\* conditional swap of the one object that says whether b lives (TASK-515): to the next epoch, what
\* it names kept; then its recovery starts. Not a deleted cell's, nor a fork's being made; nor, its
\* token higher, a later placement's (tokens).
Open(h, b) ==
    /\ takes < MaxTakes
    /\ IF ses[h][b] = None THEN TRUE ELSE ses[h][b].pc = "stopped"
    /\ LiveRec(b)
    /\ R("tokens") => head[b].token <= tok[h][b]
    /\ LET e == head[b].epoch + 1
           s == [gen |-> gen + 1, pc |-> "read", epoch |-> e, token |-> tok[h][b],
                 origin |-> head[b].origin, st |-> Fresh, at |-> 0, end |-> 0, chain |-> <<>>,
                 hckpt |-> <<>>, seq |-> 0, next |-> <<0, 0>>, up |-> {}, done |-> {}, open |-> {},
                 ckptThrough |-> -1, ckptPut |-> None, durable |-> None, vers |-> {},
                 rec |-> head[b]]
       IN /\ head' = [head EXCEPT ![b] = [@ EXCEPT !.epoch = e, !.token = tok[h][b],
                                                   !.holder = gen + 1,
                                                   !.vers = IF R("versionsCarried") THEN @ ELSE {}]]
          /\ ses' = [ses EXCEPT ![h][b] = Started(s, b, head[b])]
    /\ taken' = [taken EXCEPT ![b] = IF tok[h][b] > @ THEN tok[h][b] ELSE @]
    /\ gen' = gen + 1
    /\ takes' = takes + 1
    /\ UNCHANGED <<seg, own, ckpt, inflight, refs, marked, retired, written, gone, wal, tok,
                   proc, gc>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, relocated, doomed, placed, errors>>
    /\ UNCHANGED <<passes, places, crashes, mints>>

RECURSIVE Restamp(_, _, _, _)
\* The WAL's segments R restamped on the chain of epoch e from segment q, minting on from next:
\* their data unchanged, new footers.
Restamp(Rs, q, e, next) ==
    IF Rs = <<>> THEN <<>>
    ELSE LET o == [Rs[1].o EXCEPT !.epoch = e, !.next = Max2(@, next)]
         IN <<[s |-> q, o |-> o]>> \o Restamp(Tail(Rs), q + 1, e, o.next)

\* The first commit segment o is of: the one it ends, or the one it goes on with.
SegCommit(o) == IF o.commit # 0 THEN o.commit ELSE o.cont.commit

\* h's writer of b, whose session s recovered the cell to its head's end, opened: the head read
\* again (epochSequences), and the chain, minting from the epoch's range. Then its WAL put after
\* the cell's end (wal::rebase), by commits: the WAL's segments to the one ending the cell's last
\* commit, if it is one of the WAL's, acknowledged; the rest restamped after the head's end, if
\* the cell's last commit is one of the WAL's or the commit the WAL started from (walFollows);
\* otherwise the WAL diverged, and is dropped. By commits, not by the keys the head names: a cell
\* keeps what it restamps in memory alone, its footers on disk naming the keys of an earlier
\* stamping after a crash. The commit the WAL started from, its first segment's commit's parent,
\* the WAL keeps. The cell keeps the restamped footers in memory alone (TASK-165.9): a crash
\* after finds them stamped as before, which the next opening restamps by the same commits, its
\* own chain, and Max2 with a later floor, to what this would have: so the WAL here holds them
\* restamped. The WAL's segments replayed over the state must end every commit they go on with,
\* or the cell refuses to open (cell.rs: "log ends inside a commit"), its local recovery having
\* kept a commit begun.
Opened(h, b, s) ==
    LET floor == IF R("epochSequences") THEN <<s.epoch, 0>> ELSE <<0, 0>>
        next == Max2(s.st.next, floor)
        ws == wal[h][b]
        base == IF ws = <<>> THEN 0 ELSE commits[SegCommit(ws[1].o)].parent
        \* The guest's last commit as of the WAL's first j segments (Wal::heads): the last any of
        \* them makes, or the WAL's base.
        tipAt(j) == LET C == {i \in 1..j : ws[i].o.commit # 0} IN
                    IF C = {} THEN base ELSE ws[Max(C)].o.commit
        at == {j \in 0..Len(ws) : tipAt(j) = s.st.tip}
        follows == at # {}
        k == IF follows THEN Min(at) ELSE 0
        A == SubSeq(ws, 1, k)
        Rs == SubSeq(ws, k + 1, Len(ws))
        keep == Rs # <<>> /\ (follows \/ ~R("walFollows"))
        new == IF keep THEN Restamp(Rs, s.seq, s.epoch, next) ELSE <<>>
    IN IF (R("epochSequences") /\ ~Taking(s, b)) \/ s.seq + Len(new) > Segs
       THEN /\ ses' = [ses EXCEPT ![h][b] = [s EXCEPT !.pc = "stopped"]]
            /\ UNCHANGED <<wal, acked, errors>>
       ELSE IF ApplyAll(s.st, b, new).begun # None
       THEN /\ ses' = [ses EXCEPT ![h][b] = [s EXCEPT !.pc = "stopped"]]
            /\ errors' = errors \cup {"a store's log ends inside a commit"}
            /\ UNCHANGED <<wal, acked>>
       ELSE /\ wal' = [wal EXCEPT ![h][b] = new]
            /\ acked' = [acked EXCEPT ![b] = @ \cup CommitsOf(A)]
            /\ ses' = [ses EXCEPT ![h][b] =
                         [s EXCEPT !.pc = "ready",
                                   !.durable = {new[i].s : i \in DOMAIN new},
                                   !.seq = s.seq + Len(new),
                                   !.next = IF new = <<>> THEN next ELSE new[Len(new)].o.next]]
            \* Diverged, the WAL dropped, though no other writer's commit moved the cell on: its
            \* last is one of the WAL's, or the WAL's base.
            /\ errors' = IF Rs # <<>> /\ ~keep /\ s.st.tip \in CommitsOf(ws) \cup {base}
                         THEN errors \cup {"a WAL diverged from a cell holding its commits"}
                         ELSE errors

\* Recovery's segments, those the head names after its checkpoint, each at its key; something
\* named gone, the writer stops (its head taken since, or the bucket lost it: NothingNeededGone);
\* then the writer opened.
RecoverRead(h, b) ==
    LET s == ses[h][b]
        r == FollowChain(b, s.st, s.at, s.end, s.chain, {})
    IN /\ s # None /\ s.pc = "read"
       /\ IF ~r.ok THEN Stopped(h, b) /\ UNCHANGED <<wal, acked, errors>>
          ELSE Opened(h, b, [s EXCEPT !.st = r.st, !.seq = s.end])
       /\ UNCHANGED <<bucketVars, tok, proc, gc, countVars>>
       /\ UNCHANGED <<commits, nextTag, minted, twice, makers, relocated, doomed, taken, placed>>

Ready(h, b) == ses[h][b] # None /\ ses[h][b].pc = "ready"

\* The guest mints an extent ID, from the chain's next sequence number.
Mint(h, b) ==
    LET s == ses[h][b]
        x == <<b, s.next[1], s.next[2]>>
    IN /\ "mint" \in Can /\ Ready(h, b) /\ mints < MaxMints
       /\ twice' = (twice \/ x \in minted)
       /\ minted' = minted \cup {x}
       /\ ses' = [ses EXCEPT ![h][b].open = @ \cup {x}, ![h][b].next = <<@[1], @[2] + 1>>]
       /\ mints' = mints + 1
       /\ UNCHANGED <<bucketVars, wal, tok, proc, gc>>
       /\ UNCHANGED <<commits, nextTag, acked, makers, relocated, doomed, taken, placed, errors>>
       /\ UNCHANGED <<gen, takes, passes, places, crashes>>

\* What a commit does, besides committing: make a version, the cell quiescent, sealing nothing;
\* seal an open extent into its segment or on its own; free an extent its guest holds; or seal one
\* or two open extents where they were written, in segments filled since the last commit that go
\* on with it, one each, the commit ending in the next (a commit spanning segments: TASK-92.41,
\* segments::Phase::Full). With "spanOn", the first of those segments may be the WAL's last,
\* ending the commit before, not uploaded: it filled after that commit, before this one
\* (Segments::record: "the first alone may hold commits, those made before it filled").
Ops(h, b) ==
    LET s == ses[h][b]
        v == View(h, b)
        op(k, x) == [k |-> k, x |-> x, xs |-> <<>>, on |-> FALSE]
        span(xs, on) == [k |-> "span", x |-> None, xs |-> xs, on |-> on]
        ws == wal[h][b]
        ons == {FALSE} \cup (IF /\ "spanOn" \in Can /\ ws # <<>>
                                 /\ ws[Len(ws)].s \notin s.up /\ ws[Len(ws)].o.cont = None
                              THEN {TRUE} ELSE {})
        open == IF "span" \in Can THEN s.open ELSE {}
        \* What the writer may relocate (doc-27): extents of its own cell's segments acknowledged,
        \* live in its guest's view, where its state acknowledged has them.
        movable == IF "relocate" \notin Can THEN {}
                   ELSE {x \in DOMAIN s.st.ext \cap DOMAIN v.ext :
                           s.st.ext[x][1] = "seg" /\ s.st.ext[x][2] = b /\ v.ext[x] = s.st.ext[x]}
    IN (IF "commit" \in Can THEN {op("plain", None)} ELSE {})
       \* A version's commit (TASK-476): of a quiescent cell, sealing nothing.
       \cup (IF "version" \in Can /\ s.open = {} THEN {op("version", None)} ELSE {})
       \cup {op("seg", x) : x \in IF "seg" \in Can THEN s.open ELSE {}}
       \* A relocation (doc-27): the writer's own commit, copying a live extent's bytes into its
       \* segment, which names it there, as a seal does; its guest's state unchanged.
       \cup {op("relocate", x) : x \in movable}
       \cup {op("own", x) : x \in IF "own" \in Can THEN s.open ELSE {}}
       \cup {op("free", x) : x \in IF "free" \in Can THEN DOMAIN v.ext ELSE {}}
       \cup {span(<<x>>, on) : x \in open, on \in ons}
       \cup {span(<<x, y>>, on) : <<x, y>> \in {p \in open \X open : p[1] # p[2]}, on \in ons}

\* A commit: segments of the WAL, stamped by the chain: those going on with it, if it spans
\* segments (Contents::continued, each naming the commit and its run there), the first perhaps
\* the WAL's last, then the one it ends in. A relocation's commit names the extent it copies, as a
\* seal does, which held it already: the commits' extents are the same.
Commit(h, b) ==
    LET s == ses[h][b]
        t == nextTag
        ws == wal[h][b]
    IN /\ Ready(h, b) /\ t <= MaxCommits
       /\ \E op \in Ops(h, b) :
            LET on == IF op.on THEN 1 ELSE 0
                n == Len(op.xs) - on
                cont(i) == [commit |-> t, ext |-> op.xs[i]]
                part(i) == [s |-> s.seq + i - 1,
                            o |-> [epoch |-> s.epoch, commit |-> 0, ext |-> None, own |-> None,
                                   free |-> None, next |-> s.next, ver |-> None,
                                   cont |-> cont(i + on)]]
                before == IF op.on THEN [ws EXCEPT ![Len(ws)].o.cont = cont(1)] ELSE ws
                o == [epoch |-> s.epoch,
                      commit |-> t,
                      ext |-> IF op.k \in {"seg", "relocate"} THEN op.x ELSE None,
                      own |-> IF op.k = "own" THEN op.x ELSE None,
                      free |-> IF op.k = "free" THEN op.x ELSE None,
                      next |-> s.next,
                      ver |-> IF op.k = "version" THEN t ELSE None,
                      cont |-> None]
                xs == {op.xs[i] : i \in DOMAIN op.xs}
                \* Without versionsAcked, the version named as its commit is made: by the next
                \* swap of the head, whatever it acknowledges.
                early == IF op.k = "version" /\ ~R("versionsAcked")
                         THEN {[id |-> t, at |-> s.seq + n]} ELSE {}
            IN /\ s.seq + n < Segs
               /\ wal' = [wal EXCEPT ![h][b] = before \o [i \in 1..n |-> part(i)]
                                                 \o <<[s |-> s.seq + n, o |-> o]>>]
               /\ ses' = [ses EXCEPT ![h][b] =
                            [s EXCEPT !.seq = @ + n + 1, !.open = @ \ ({op.x} \cup xs),
                                      !.vers = @ \cup early]]
               /\ commits' = [commits EXCEPT ![t] = [cell |-> b, parent |-> View(h, b).tip,
                                                      seals |-> IF op.k \in {"seg", "own", "relocate"}
                                                                THEN {op.x} ELSE xs,
                                                      free |-> o.free,
                                                      ver |-> op.k = "version",
                                                      dropped |-> FALSE]]
               /\ relocated' = IF op.k = "relocate" THEN relocated \cup {s.st.ext[op.x]}
                               ELSE relocated
       /\ nextTag' = t + 1
       /\ UNCHANGED <<bucketVars, tok, proc, gc>>
       /\ UNCHANGED <<acked, minted, twice, makers, doomed, taken, placed, errors, countVars>>

\* The upload of the WAL's next segment: its extent of its own created, then its PUT in flight.
Upload(h, b) ==
    LET s == ses[h][b]
        ws == wal[h][b]
        I == {i \in DOMAIN ws : ws[i].s \notin s.up}
        w == ws[Min(I)]
    IN /\ Ready(h, b) /\ I # {}
       /\ own' = IF w.o.own = None THEN own
                 ELSE own \cup {[b |-> b, e |-> w.o.epoch, s |-> w.s, x |-> w.o.own]}
       /\ inflight' = inflight \cup {[h |-> h, b |-> b, g |-> s.gen, s |-> w.s, o |-> w.o]}
       /\ ses' = [ses EXCEPT ![h][b].up = @ \cup {w.s}]
       /\ UNCHANGED <<seg, ckpt, head, refs, marked, retired, written, gone, wal, tok, proc,
                      gc, truthVars, countVars>>

\* A PUT lands, whenever, though its writer has gone: at its key, its writer's alone, over what
\* is there (nothing, or its own); its writer, if it still is, told. With "landInOrder", a
\* writer's PUTs land in the order it made them, the earliest first: a model of more writers kept
\* to a size TLC exhausts (BucketTakeovers), BucketStrays landing them in any order.
Land(m) ==
    LET s == ses[m.h][m.b]
        mine == s # None /\ s.gen = m.g /\ s.pc # "stopped"
        k == SegKey(m.b, m.s, m.o.epoch)
    IN /\ "landInOrder" \notin Can
          \/ \A n \in inflight : n.h = m.h /\ n.b = m.b /\ n.g = m.g => n.s >= m.s
       /\ inflight' = inflight \ {m}
       /\ seg' = [seg EXCEPT ![m.b][m.s][k[4]] = m.o]
       /\ written' = written \cup {<<k, m.o>>}
       /\ ses' = IF mine THEN [ses EXCEPT ![m.h][m.b].done = @ \cup {m.s}] ELSE ses
       /\ UNCHANGED <<own, ckpt, head, refs, marked, retired, gone, wal, tok, proc, gc,
                      truthVars, countVars>>

\* The acknowledgement of the WAL's prefix landed: the head swapped to name it, with the
\* condition that it is the writer's taking still (headSwap); failing, the writer stops, taken
\* over. Without the rule, written whatever is there. Then its commits told durable. To the last
\* segment going on with no commit (acksBetweenCommits, Writer::acknowledge): a commit spanning
\* segments is acknowledged with the one it ends in, so that the state acknowledged, which heads
\* and checkpoints name, holds no commit begun, and the WAL keeps it whole. The swap names the
\* writer's checkpoint landed since, if any, too (TASK-690's open end: no swap of its own).
Ack(h, b) ==
    LET s == ses[h][b]
        ws == wal[h][b]
        n == Max({k \in 0..Len(ws) : /\ \A i \in 1..k : ws[i].s \in s.done
                                     /\ k = 0 \/ ws[k].o.cont = None \/ ~R("acksBetweenCommits")})
        P == SubSeq(ws, 1, n)
        \* The versions the segments name, named with them (versionsAcked).
        V == IF R("versionsAcked")
             THEN {[id |-> P[i].o.ver, at |-> P[i].s] : i \in {i \in 1..n : P[i].o.ver # None}}
             ELSE {}
        named == s.ckptPut # None /\ s.ckptPut.landed
        s2 == [s EXCEPT !.st = ApplyAll(s.st, b, P),
                        !.end = P[n].s + 1,
                        !.chain = @ \o [i \in 1..n |-> P[i].o.epoch],
                        !.vers = @ \cup V,
                        !.hckpt = IF named THEN <<s.ckptPut.q, s.epoch>> ELSE @,
                        !.ckptPut = IF named THEN None ELSE @]
    IN /\ Ready(h, b) /\ n > 0
       /\ IF R("headSwap") /\ ~Swappable(s, b) THEN
             Stopped(h, b) /\ UNCHANGED <<head, wal, acked>>
          ELSE /\ head' = [head EXCEPT ![b] = HeadOf(s2, b)]
               /\ wal' = [wal EXCEPT ![h][b] = SubSeq(ws, n + 1, Len(ws))]
               /\ ses' = [ses EXCEPT ![h][b] = s2]
               /\ acked' = [acked EXCEPT ![b] = @ \cup CommitsOf(P)]
       /\ UNCHANGED <<seg, own, ckpt, inflight, refs, marked, retired, written, gone, tok,
                      proc, gc, countVars>>
       /\ UNCHANGED <<commits, nextTag, minted, twice, makers, relocated, doomed, taken, placed, errors>>

\* The writer drops a version (Writer::drop_versions): the head swapped to name it no more, with
\* the condition that it is the writer's taking still (headSwap); failing, the writer stops. A
\* window's oldest dropped as an acknowledgement names a new version is this, in that swap.
DropVersion(h, b) ==
    LET s == ses[h][b] IN
    /\ "dropVersion" \in Can /\ Ready(h, b)
    /\ \E v \in s.vers :
         IF R("headSwap") /\ ~Swappable(s, b) THEN Stopped(h, b) /\ UNCHANGED <<head, commits>>
         ELSE LET s2 == [s EXCEPT !.vers = @ \ {v}] IN
              /\ head' = [head EXCEPT ![b] = HeadOf(s2, b)]
              /\ ses' = [ses EXCEPT ![h][b] = s2]
              /\ commits' = [commits EXCEPT ![v.id].dropped = TRUE]
    /\ UNCHANGED <<seg, own, ckpt, inflight, refs, marked, retired, written, gone,
                   wal, tok, proc, gc, countVars>>
    /\ UNCHANGED <<nextTag, acked, minted, twice, makers, relocated, doomed, taken, placed, errors>>

\* A checkpoint as of the last segment acknowledged: its PUT made.
Checkpoint(h, b) ==
    LET s == ses[h][b]
        q == s.st.through
    IN /\ "checkpoint" \in Can /\ Ready(h, b) /\ q >= 0 /\ q > s.ckptThrough /\ s.ckptPut = None
       /\ ses' = [ses EXCEPT ![h][b].ckptThrough = q,
                             ![h][b].ckptPut = [q |-> q, st |-> s.st, landed |-> FALSE]]
       /\ UNCHANGED <<bucketVars, wal, tok, proc, gc, truthVars, countVars>>

\* The checkpoint's PUT lands, at its writer's key, whenever while its writer's session lasts,
\* though it stopped.
CheckpointLand(h, b) ==
    LET s == ses[h][b]
        k == CkptKey(b, s.ckptPut.q, s.epoch)
    IN /\ s # None /\ s.ckptPut # None /\ ~s.ckptPut.landed
       /\ ckpt' = [ckpt EXCEPT ![b][s.ckptPut.q][k[4]] = s.ckptPut.st]
       /\ written' = written \cup {<<k, s.ckptPut.st>>}
       /\ ses' = [ses EXCEPT ![h][b].ckptPut.landed = TRUE]
       /\ UNCHANGED <<seg, own, head, inflight, refs, marked, retired, gone, wal, tok, proc, gc,
                      truthVars, countVars>>

\* The checkpoint landed named in the head by a swap of its own, with the condition that it is the
\* writer's taking still (headSwap); failing, the writer stops. An acknowledgement names it too.
CheckpointSwap(h, b) ==
    LET s == ses[h][b]
        s2 == [s EXCEPT !.hckpt = <<s.ckptPut.q, s.epoch>>, !.ckptPut = None]
    IN /\ Ready(h, b) /\ s.ckptPut # None /\ s.ckptPut.landed
       /\ IF R("headSwap") /\ ~Swappable(s, b) THEN Stopped(h, b) /\ UNCHANGED head
          ELSE /\ head' = [head EXCEPT ![b] = HeadOf(s2, b)]
               /\ ses' = [ses EXCEPT ![h][b] = s2]
       /\ UNCHANGED <<seg, own, ckpt, inflight, refs, marked, retired, written, gone, wal,
                      tok, proc, gc, truthVars, countVars>>

WriterNext ==
    \E h \in Hosts, b \in Cells :
        \/ Open(h, b) \/ RecoverRead(h, b) \/ Mint(h, b) \/ Commit(h, b)
        \/ Upload(h, b) \/ Ack(h, b) \/ Checkpoint(h, b) \/ CheckpointLand(h, b)
        \/ CheckpointSwap(h, b) \/ DropVersion(h, b)

-----------------------------------------------------------------------------
(* Hosts and placements. *)

\* The lengths a crash may cut h's WAL of b to, with "cut": its commits to the last a flush made
\* durable, at least those of every segment uploaded, which waited for theirs (uploads.rs's tag:
\* a segment going on with a commit, for that commit), and those durable before the writer's
\* session (its WAL as it opened; all of it, if it has not). Local recovery (wal::recover) keeps a
\* commit spanning segments only whole, if consecutive segments end it, and closes the first
\* again without it otherwise (wholeCommitsKept): the WAL is cut between commits, as a flush
\* appends a commit's records to its segments' logs one after another, a crash stopping it, or
\* their checks against the raw data, at any of them.
Cuts(h, b) ==
    LET ws == wal[h][b]
        s == ses[h][b]
        up == IF s = None \/ s.durable = None THEN Len(ws)
              ELSE Max({0} \cup {i \in DOMAIN ws : ws[i].s \in s.up \cup s.durable})
        whole(k) == k = 0 \/ ws[k].o.cont = None
        lo == Min({k \in up..Len(ws) : whole(k)} \cup {Len(ws)})
    IN IF "cut" \in Can
       THEN {k \in lo..Len(ws) : whole(k) \/ ~R("wholeCommitsKept")}
       ELSE {Len(ws)}

\* A host crashes: its writers and its other work stop; its WALs stay, or the disk is lost; with
\* "cut", each to its last durable commit.
Crash(h) ==
    /\ "crash" \in Can /\ crashes < MaxCrashes
    /\ \E lose \in IF "disk" \in Can THEN {FALSE, TRUE} ELSE {FALSE},
          cut \in [Cells -> 0..Segs] :
          /\ \A b \in Cells : cut[b] \in Cuts(h, b)
          /\ wal' = [wal EXCEPT ![h] = [b \in Cells |-> IF lose THEN <<>>
                                                        ELSE SubSeq(@[b], 1, cut[b])]]
    /\ ses' = [ses EXCEPT ![h] = [b \in Cells |-> None]]
    /\ proc' = [proc EXCEPT ![h] = None]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<bucketVars, tok, gc, truthVars, gen, takes, passes, places, mints>>

\* b placed on h: a fencing token above every earlier placement's.
Place(h, b) ==
    /\ "place" \in Can /\ places < MaxPlaces /\ head[b] # None
    /\ tok' = [tok EXCEPT ![h][b] = placed[b] + 1]
    /\ placed' = [placed EXCEPT ![b] = @ + 1]
    /\ places' = places + 1
    /\ UNCHANGED <<bucketVars, ses, wal, proc, gc, gen, takes, passes, crashes, mints>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, relocated, doomed, taken, errors>>

-----------------------------------------------------------------------------
(* Deletions and forks of versions: a host's work, one at a time. *)

\* A deletion of b, asked on h: the head ended first. Asked by b's last placement, or one as
\* late, it may delete b's making. With "deleteRoot" alone, of the root alone.
StartDelete(h, b) ==
    /\ proc[h] = None
    /\ head[b] # None /\ ~Deleted(head[b])
    /\ \/ "delete" \in Can
       \/ "deleteRoot" \in Can /\ b = Root
    /\ proc' = [proc EXCEPT ![h] = [op |-> "delete", b |-> b, pc |-> "end", o |-> <<>>,
                                    tk |-> tok[h][b], og |-> <<>>, bs |-> 0, v |-> <<>>]]
    /\ doomed' = IF tok[h][b] >= taken[b] THEN doomed \cup {head[b].maker} ELSE doomed
    /\ UNCHANGED <<bucketVars, ses, wal, tok, gc, countVars>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, relocated, taken, placed, errors>>

\* The reference of the fork of a version, the restore q, to its child: under the cell of the
\* version.
ForkRefs(q) == IF R("references") THEN {RefOf(q.og[1], q.og[2], q.b)} ELSE {}

\* The record of the child of the restore q, being made (forksPending), of its version.
Forking(q) == [Record(IF R("forksPending") THEN "forking" ELSE "fork", q.og, q.o, q.bs)
                   EXCEPT !.ver = q.v]

\* The restore q, to its child, begun on h: first the child's record, saying it is being made
\* (forksPending), then its reference (forksRecordFirst), before it reads the head naming the
\* version (forksReferFirst); without forksRecordFirst, the reference first.
Restoring(h, q) ==
    LET refers == R("forksReferFirst") /\ ForkRefs(q) # {}
        first == refers /\ ~R("forksRecordFirst")
    IN /\ IF first THEN
             /\ refs' = refs \cup ForkRefs(q)
             /\ proc' = [proc EXCEPT ![h] = [q EXCEPT !.pc = "fork"]]
             /\ UNCHANGED head
          ELSE
             /\ head' = [head EXCEPT ![q.b] = Forking(q)]
             /\ proc' = [proc EXCEPT ![h] = [q EXCEPT !.pc = IF refers THEN "refer" ELSE "check"]]
             /\ UNCHANGED refs
       /\ UNCHANGED <<seg, own, ckpt, inflight, marked, retired, written, gone, ses, wal,
                      tok, gc, truthVars, countVars>>

\* A restore of a version of b to c (TASK-476): of b at the version's segment, its base the
\* version's commit, a version b's head names as the restore begins. c's ID is an allocator's:
\* no cell's, nor a removed one's. Its record names the version.
StartRestore(h, c, b) ==
    /\ "restore" \in Can /\ proc[h] = None
    /\ head[c] = None /\ c \notin {Root, b} \cup gone
    /\ \E v \in HeadVers(b) :
         Restoring(h, [op |-> "restore", b |-> c, pc |-> "check", o |-> <<"restore", c, h>>,
                       tk |-> tok[h][c], og |-> <<b, v.at>>, bs |-> v.id, v |-> <<b, v.id>>])

Done(h) == proc' = [proc EXCEPT ![h] = None]
\* A deletion of the cell proc[h] names, after a failed restore.
Deleting(h) == proc' = [proc EXCEPT ![h] = [@ EXCEPT !.op = "delete", !.pc = "end"]]

\* b's head swapped to the final epoch by a deletion on h, what it names kept, but its versions,
\* which go with it (doc-18): made so, if b has none, deleted from the first.
Ended(h, b) ==
    head' = [head EXCEPT ![b] = IF @ = None
                                THEN [Record("fork", <<>>, <<"deletion", h>>, 0)
                                         EXCEPT !.epoch = Final]
                                ELSE [@ EXCEPT !.epoch = Final, !.token = 0, !.holder = 0,
                                               !.vers = {}]]

ProcStep(h) ==
    LET q == proc[h]
        b == q.b
        failed == q.op = "restore" /\ q.pc = "check" /\ R("forksCheck") /\ VersionGone(q.v)
    IN /\ q # None
       /\ CASE q.op = "delete" /\ q.pc = "end" ->
                 \* Its head swapped to the final epoch, in one swap, unless a later placement took
                 \* the cell (deleteTokens). No writer swaps it after: the head is the fence.
                 /\ IF head[b] # None /\ ~Deleted(head[b]) /\ R("deleteTokens")
                       /\ head[b].token > q.tk
                    THEN UNCHANGED head
                    ELSE Ended(h, b)
                 /\ Done(h)
                 /\ UNCHANGED <<refs, makers, relocated>>
            [] q.op = "restore" /\ q.pc = "refer" ->
                 \* Its reference, its head made (forksRecordFirst), before the version's read.
                 /\ refs' = refs \cup ForkRefs(q)
                 /\ proc' = [proc EXCEPT ![h].pc = "check"]
                 /\ UNCHANGED <<head, makers, relocated>>
            [] q.op = "restore" /\ q.pc = "fork" ->
                 \* Without forksRecordFirst: the child's head, its reference made, unless the
                 \* ID was taken since, the reference then left for good.
                 /\ IF head[b] = None THEN /\ head' = [head EXCEPT ![b] = Forking(q)]
                                           /\ proc' = [proc EXCEPT ![h].pc = "check"]
                    ELSE Done(h) /\ UNCHANGED head
                 /\ UNCHANGED <<refs, makers, relocated>>
            [] q.op = "restore" /\ q.pc = "check" ->
                 \* The head naming the version read after the child's record (forksCheck): the
                 \* version named no more, the fork fails, and its host deletes the child.
                 /\ IF failed THEN Deleting(h) ELSE proc' = [proc EXCEPT ![h].pc = "make"]
                 /\ UNCHANGED <<head, refs, makers, relocated>>
            [] q.op = "restore" /\ q.pc = "make" ->
                 \* No longer being made, with a condition: unless deleted, given up on. Without
                 \* forksReferFirst, its reference made now, after the version's read.
                 /\ IF head[b] # None /\ ~Deleted(head[b]) THEN
                       /\ head' = [head EXCEPT ![b].kind = IF @ = "forking" THEN "fork" ELSE @]
                       /\ makers' = [makers EXCEPT ![b] = @ \cup {q.o}]
                       /\ refs' = IF R("forksReferFirst") THEN refs ELSE refs \cup ForkRefs(q)
                    ELSE UNCHANGED <<head, makers, relocated, refs>>
                 /\ Done(h)
       /\ doomed' = IF failed THEN doomed \cup {q.o} ELSE doomed
       /\ UNCHANGED <<seg, own, ckpt, inflight, marked, retired, written, gone, ses, wal,
                      tok, gc, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, relocated, taken, placed, errors>>

HostNext ==
    \/ \E h \in Hosts : Crash(h) \/ ProcStep(h)
    \/ \E h \in Hosts, b \in Cells : Place(h, b) \/ StartDelete(h, b)
    \/ \E h \in Hosts, c, p \in Cells : StartRestore(h, c, p)
    \/ \E m \in inflight : Land(m)

-----------------------------------------------------------------------------
(* The collector. *)

\* A pass: its cell, what it listed, the record it read, the cell recovered, keyed by the cell (if
\* it lives), the references it listed, and the origins' and versions' states it keeps, each
\* [ext, touched]. Idle, a pass keeps its cell: its epoch is that cell's collector lease's.
GcIdle == [pc |-> "idle", cell |-> 0, objs |-> {}, rec |-> None, live |-> Nothing, refs |-> {},
           pts |-> {}, stale |-> {}]
Idle(g) == [GcIdle EXCEPT !.cell = g.cell]

\* Every object's key under cell c's prefix but references, which are no pass's candidates: the
\* child's cell's pass retires them.
Listing(c) ==
    {<<"seg", c, k[1], k[2]>> : k \in {k \in Seqs \X Epochs : seg[c][k[1]][k[2]] # None}}
    \cup {<<"own", o.b, o.e, o.s, o.x>> : o \in {o \in own : o.b = c}}
    \cup {<<"ckpt", c, k[1], k[2]>> : k \in {k \in Seqs \X Epochs : ckpt[c][k[1]][k[2]] # None}}
    \cup (IF head[c] # None THEN {<<"head", c>>} ELSE {})

\* The state of p at its origin at, as a pass keeps it for a reference, or a version: the extents
\* it names, and what recovering it reads (originsKept); without the rule, the extents alone.
Origin(p, at) ==
    LET r == StateAt(p, at) IN
    [ok |-> r.ok, ext |-> r.st.ext, touched |-> IF R("originsKept") THEN r.touched ELSE {}]
Kept(fp) == [ext |-> fp.ext, touched |-> fp.touched]

\* The origins of the references Rs: the states kept, and whether one is lost that a live child of
\* theirs needs. A child not live (deleted, being made, or of no record yet: ReferencesNamed)
\* loses nothing.
RefOrigins(Rs) ==
    LET P == {<<r.p, r.at>> : r \in Rs}
        lost(pt) == /\ ~Origin(pt[1], pt[2]).ok
                    /\ \E r \in Rs : <<r.p, r.at>> = pt /\ LiveRec(r.c)
    IN [pts |-> {Kept(Origin(pt[1], pt[2])) : pt \in {pt \in P : Origin(pt[1], pt[2]).ok}},
        lost |-> \E pt \in P : lost(pt)]

\* Whether a later pass of its cell took the cell's collector lease from pass i.
Fenced(i) == \E j \in (i + 1)..passes : gc[j].cell = gc[i].cell

\* A pass's start over cell c, taking the cell's collector lease, once the others are done (but
\* with "overlap", which lets passes of any cells, c's or others', run at once; a model without it
\* checks the cells' passes one after another, kept to a size TLC exhausts): the listing of c's
\* prefix (candidatesFirst), before its record is read. Without referencesRelisted, its references
\* listed now, and not again. The listing holds the cell's references and marks too: those of the
\* references it lists marked, retired, written again by a slow restore since, are garbage, which
\* it deletes (retirementsMarked).
GcStart(c) ==
    LET i == passes + 1 IN
    /\ "collect" \in Can /\ passes < MaxPasses
    /\ "overlap" \in Can \/ \A j \in DOMAIN gc : gc[j].pc = "idle"
    /\ gc' = [gc EXCEPT ![i] = [GcIdle EXCEPT !.pc = "recover", !.cell = c,
                                               !.objs = IF R("candidatesFirst") THEN Listing(c)
                                                        ELSE {},
                                               !.refs = IF R("referencesRelisted") THEN {}
                                                        ELSE RefsUnder(c),
                                               !.stale = RefsUnder(c) \cap marked]]
    /\ passes' = i
    /\ UNCHANGED <<bucketVars, hostVars, truthVars, gen, takes, places, crashes, mints>>

\* The cell's record read, a fork being made of a version dropped given up on (collectorAbandons),
\* and the cell, if it lives, recovered from its head, its state at each version the head names
\* kept whole (versionsKept), in one read of the bucket: what it finds lost is lost now, whatever
\* later pass took the lease from it (TASK-180's stop of a pass fenced went with the passes reading
\* records before the states). A version of another cell's is read now, a GET of that cell's head
\* (TASK-476).
GcRecover(i) ==
    LET g == gc[i]
        c == g.cell
        r == head[c]
        ab == /\ R("collectorAbandons") /\ r # None /\ ~Deleted(r) /\ r.kind = "forking"
              /\ VersionGone(r.ver)
        r1 == IF ab THEN [r EXCEPT !.epoch = Final] ELSE r
        live == r1 # None /\ ~Deleted(r1)
        got == Recover(c, r1.origin)
        vs == IF R("versionsKept") THEN {Origin(c, v.at) : v \in HeadVers(c)} ELSE {}
    IN /\ g.pc = "recover"
       /\ head' = [head EXCEPT ![c] = r1]
       /\ IF live /\ (~got.ok \/ \E fp \in vs : ~fp.ok) THEN
             /\ errors' = errors \cup {"a pass lost what a live cell needs"}
             /\ gc' = [gc EXCEPT ![i] = Idle(g)]
          ELSE
             \* Each recovery keeps what its head may yet name: of its epoch, the segments from
             \* its end and the checkpoints after its own.
             /\ gc' = [gc EXCEPT ![i] =
                         [g EXCEPT !.rec = r1,
                                   !.live = IF ~live THEN Nothing
                                            ELSE Put(Nothing, c,
                                                     [ext |-> got.st.ext, end |-> got.end,
                                                      epoch |-> HeadEpoch(c), ckpt |-> got.ckpt,
                                                      touched |-> got.touched]),
                                   !.pts = IF live THEN {Kept(fp) : fp \in vs} ELSE {},
                                   !.pc = "relist"]]
             /\ UNCHANGED errors
       /\ UNCHANGED <<seg, own, ckpt, inflight, refs, marked, retired, written, gone,
                      hostVars, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, relocated, doomed, taken, placed>>

\* After the states: the cell's references listed (referencesRelisted), each origin's state kept.
\* A reference written after this listing is of a fork whose parent's state at its origin this
\* pass kept, or never listed: a version's fork writes its reference before it reads the head
\* naming the version, so that one reading it live made its reference before the version's drop,
\* before the pass read the version dropped, or its own reference retired by its cell's pass,
\* after.
GcRelist(i) ==
    LET g == gc[i]
        listed == IF R("referencesRelisted") THEN RefsUnder(g.cell) ELSE g.refs
        kept == RefOrigins(listed)
    IN /\ g.pc = "relist"
       /\ IF kept.lost THEN
             /\ errors' = errors \cup {"a pass lost what a live cell needs"}
             /\ gc' = [gc EXCEPT ![i] = Idle(g)]
          ELSE
             /\ gc' = [gc EXCEPT ![i] = [g EXCEPT !.refs = listed, !.pts = @ \cup kept.pts,
                                                 !.pc = "finish"]]
             /\ UNCHANGED errors
       /\ UNCHANGED <<bucketVars, hostVars, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, relocated, doomed, taken, placed>>

\* Whether the reference of the fork whose record is r is retired unmarked (retirementsMarked): its
\* fork made, by its one restore, having written it before it made it.
Unmarked(r) == r.kind = "fork"

\* Deletion, unless a later pass of the cell took its lease: of what was listed, what the live
\* cell and the origins and versions kept do not read, nor its head may yet name
\* (collectorEpochs). Then the cell's reference, the cell deleted, retired, under its parent's
\* cell, once the cell has no reference of its own (referencesHeld): no cell anywhere inherits
\* from it then; without the rule, once it is deleted. A dead cell, no mark under it, goes whole
\* (TASK-455): its record and head too, the rest of it doomed already. It reads no object's data
\* and writes none (doc-27): it holds no key.
GcFinish(i) ==
    LET g == gc[i]
        c == g.cell
        r == g.rec
        objs == IF R("candidatesFirst") THEN g.objs ELSE Listing(c)
        L == g.live
        touched == UNION {L[k].touched : k \in DOMAIN L} \cup UNION {p.touched : p \in g.pts}
        locs == UNION {{<<x, L[k].ext[x]>> : x \in DOMAIN L[k].ext} : k \in DOMAIN L}
                \cup UNION {{<<x, p.ext[x]>> : x \in DOMAIN p.ext} : p \in g.pts}
        \* The objects the states kept name their extents in: segments, and extents of their own.
        named == {l[2] : l \in {l \in locs : l[2][1] = "seg"}}
                 \cup {<<"own", l[2][2], l[2][3], l[2][4], l[1]>> :
                         l \in {l \in locs : l[2][1] = "own"}}
        \* Whether b's head, as the recovery of b read it, may yet name its segment or extent of
        \* its own at s by the writer of epoch e: of the head's epoch, or a later one, at or past
        \* its end (collectorEpochs). Of an earlier epoch, its writer's swaps all fail.
        ahead(b, s, e) ==
            b \in DOMAIN L /\ R("collectorEpochs") /\ e >= L[b].epoch /\ s >= L[b].end
        \* Likewise its checkpoint as of s by that writer: newer than the head's, by s, then by
        \* epoch (a writer taking over may checkpoint what the head's did, at its own key).
        ckptAhead(b, s, e) ==
            /\ b \in DOMAIN L /\ R("collectorEpochs") /\ e >= L[b].epoch
            /\ L[b].ckpt = <<>> \/ ~GE2(L[b].ckpt, <<s, e>>)
        dead == (r = None \/ Deleted(r)) /\ g.refs = {}
        \* Removed whole: dead, and no mark of a reference under it, which a slow restore's
        \* reference written again goes by.
        removed == dead /\ \A m \in marked : m.p # c
        doomedKey(k) ==
            CASE k[1] = "ckpt" -> k \notin touched /\ ~ckptAhead(k[2], k[3], k[4])
              [] k[1] = "seg" -> ~ahead(k[2], k[3], k[4]) /\ k \notin touched /\ k \notin named
              [] k[1] = "own" -> k \notin named /\ ~ahead(k[2], k[4], k[3])
              \* The head only with the cell: a deleted cell's final head fences its writers.
              [] k[1] = "head" -> removed
        D == IF Fenced(i) THEN {} ELSE {k \in objs : doomedKey(k)}
        \* The cell's reference, of the origin its record names, never listed, marked unless its
        \* fork was made by its restore alone (retirementsMarked), whether it is there or not
        \* (collector's retired).
        retiring == IF \/ Fenced(i) \/ r = None \/ ~Deleted(r) \/ r.origin = <<>>
                       \/ (R("referencesHeld") /\ ~dead)
                    THEN {} ELSE {RefOf(r.origin[1], r.origin[2], c)}
        \* The cell's own references listed marked: retired before.
        stale == IF Fenced(i) THEN {} ELSE g.stale
    IN /\ g.pc = "finish"
       /\ seg' =[b \in Cells |-> [s \in Seqs |-> [e \in Epochs |->
                    IF <<"seg", b, s, e>> \in D THEN None ELSE seg[b][s][e]]]]
       /\ own' = {o \in own : <<"own", o.b, o.e, o.s, o.x>> \notin D}
       /\ ckpt' = [b \in Cells |-> [s \in Seqs |-> [e \in Epochs |->
                     IF <<"ckpt", b, s, e>> \in D THEN None ELSE ckpt[b][s][e]]]]
       /\ head' = [b \in Cells |-> IF <<"head", b>> \in D THEN None ELSE head[b]]
       /\ gone' = gone \cup {b \in Cells : <<"head", b>> \in D}
       /\ refs' = refs \ (retiring \cup stale)
       /\ retired' = retired \cup retiring
       /\ marked' = marked \cup (IF retiring # {} /\ R("retirementsMarked") /\ ~Unmarked(r)
                                 THEN retiring ELSE {})
       /\ gc' = [gc EXCEPT ![i] = Idle(g)]
       /\ UNCHANGED <<inflight, written, hostVars, truthVars, countVars>>

GcNext ==
    \/ \E c \in Cells : GcStart(c)
    \/ \E i \in DOMAIN gc : GcRecover(i) \/ GcRelist(i) \/ GcFinish(i)

-----------------------------------------------------------------------------

Init ==
    /\ seg = [b \in Cells |-> [s \in Seqs |-> [e \in Epochs |-> None]]]
    /\ own = {}
    /\ ckpt = [b \in Cells |-> [s \in Seqs |-> [e \in Epochs |-> None]]]
    /\ head = [b \in Cells |-> IF b = Root THEN Record("root", <<>>, <<"root", Root>>, 0)
                               ELSE None]
    /\ inflight = {}
    /\ refs = {}
    /\ marked = {}
    /\ retired = {}
    /\ written = {}
    /\ gone = {}
    /\ ses = [h \in Hosts |-> [b \in Cells |-> None]]
    /\ wal = [h \in Hosts |-> [b \in Cells |-> <<>>]]
    /\ tok = [h \in Hosts |-> [b \in Cells |-> 0]]
    /\ proc = [h \in Hosts |-> None]
    /\ gc = [i \in 1..MaxPasses |-> GcIdle]
    /\ commits = [t \in 1..MaxCommits |-> [cell |-> 0, parent |-> 0, seals |-> {}, free |-> None,
                                           ver |-> FALSE, dropped |-> FALSE]]
    /\ nextTag = 1
    /\ acked = [b \in Cells |-> {}]
    /\ minted = {}
    /\ twice = FALSE
    /\ makers = [b \in Cells |-> IF b = Root THEN {<<"root", Root>>} ELSE {}]
    /\ doomed = {}
    /\ relocated = {}
    /\ taken = [b \in Cells |-> -1]
    /\ placed = [b \in Cells |-> 0]
    /\ errors = {}
    /\ gen = 0 /\ takes = 0 /\ passes = 0 /\ places = 0 /\ crashes = 0 /\ mints = 0

Next == WriterNext \/ HostNext \/ GcNext

Spec == Init /\ [][Next]_vars

\* Hosts are alike.
HostPerms == Permutations(Hosts)

-----------------------------------------------------------------------------
(* What must hold. *)

\* The cells whose records say they live: roots, forks made.
LiveRecs == {b \in Cells : LiveRec(b)}

\* Nothing a live cell needs is deleted: it recovers from its head, every extent it names reads
\* back as itself, and a head a writer took is there; so does its state at each version its head
\* names (TASK-476); and the writer whose taking the head is reads every extent its guest holds
\* where its state acknowledged says it is.
NothingNeededGone ==
    /\ \A b \in LiveRecs :
          LET r == Recover(b, head[b].origin) IN
          /\ r.ok
          /\ \A x \in DOMAIN r.st.ext : Readable(x, r.st.ext[x])
          /\ taken[b] >= 0 => head[b] # None
    /\ \A b \in LiveRecs : \A v \in HeadVers(b) :
          LET r == StateAt(b, v.at) IN
          /\ r.ok
          /\ \A x \in DOMAIN r.st.ext : Readable(x, r.st.ext[x])
    /\ \A h \in Hosts, b \in LiveRecs :
          (Ready(h, b) /\ Taking(ses[h][b], b)) =>
              \A x \in DOMAIN ses[h][b].st.ext \cap DOMAIN View(h, b).ext :
                  Readable(x, ses[h][b].st.ext[x])

\* Each live cell's history is a path of its own commits from the commit it started from, every
\* one applied once, holding what they made; and its state at each version its head names is that
\* history to the version's commit (TASK-476).
HistoriesOwn ==
    /\ \A b \in LiveRecs :
          LET r == Recover(b, head[b].origin) IN
          r.ok => /\ OwnStart(b, r.st.tip) = head[b].base
                  /\ ~r.st.dup
                  /\ r.st.hist = Anc(r.st.tip)
                  /\ DOMAIN r.st.ext = MadeBy(Path(r.st.tip), {})
    /\ \A b \in LiveRecs : \A v \in HeadVers(b) :
          LET r == StateAt(b, v.at) IN
          r.ok => /\ r.st.tip = v.id
                  /\ ~r.st.dup
                  /\ r.st.hist = Anc(v.id)
                  /\ DOMAIN r.st.ext = MadeBy(Path(v.id), {})

\* A version a writer of its cell acknowledged stays named by the cell's head until a writer drops
\* it, or a deletion ends the head (TASK-476): takeovers carry it, and no other swap names it no
\* more.
VersionsStay ==
    \A b \in Cells :
        head[b] # None /\ head[b].epoch # Final =>
            \A t \in acked[b] :
                commits[t].ver /\ commits[t].cell = b /\ ~commits[t].dropped =>
                    \E v \in head[b].vers : v.id = t

\* No acknowledged commit is lost: each is in its cell's history, and in the guest's of the writer
\* whose taking the head is.
NoAckedLost ==
    /\ \A b \in LiveRecs :
          LET r == Recover(b, head[b].origin) IN r.ok => acked[b] \subseteq Anc(r.st.tip)
    /\ \A h \in Hosts, b \in LiveRecs :
          (Ready(h, b) /\ Taking(ses[h][b], b)) =>
              acked[b] \subseteq Anc(View(h, b).tip)

\* No extent ID is minted twice.
NoIdTwice == ~twice

\* No placement takes a cell back from a later one.
HeadsCurrent == \A b \in Cells : head[b] # None /\ head[b].epoch # Final =>
                                     head[b].token >= taken[b]

\* A making told it made a cell loses it only to a deletion of it asked by who may.
DeletedOnlyByDeletes ==
    \A b \in Cells : head[b] # None /\ Deleted(head[b]) /\ head[b].maker \in Made =>
                         head[b].maker \in doomed

\* One making, not deleted, is told it made each cell.
OneMaker == \A b \in Cells : Cardinality(makers[b] \ doomed) <= 1

\* Not a rule's: a segment a writer relocated an extent out of is never deleted. Its violation is
\* reached (mutations/ReachesRelocatedFreed): writers' compaction frees what it copied out of.
NothingRelocatedFreed == \A k \in relocated : Obj(k) # None

\* Nothing failed that must not: a pass of the collector, a cell opened (its log ending inside a
\* commit), a WAL diverging from a cell whose last commit is its own.
NoErrors == errors = {}

\* A key names one object, ever (TASK-407): every PUT at a segment's or a checkpoint's key
\* writes what every other at it did, if any did, deleted since or not. Readers need no versions of
\* a key: what the host keeps of it, in memory, in its part cache, or asked of its peers, is of
\* the one object.
KeysWrittenOnce == \A p, q \in written : p[1] = q[1] => p[2] = q[2]

\* Every reference is named by its child's record, its origin the reference's origin, which the
\* child's cell's pass retires it by once the child is deleted (TASK-395.10); or is of a fork of a
\* version being made, its record yet to come; or it is marked, retired before, a slow restore's,
\* which its parent's cell's next pass deletes (retirementsMarked). Any other no pass retires, for
\* ever.
ReferencesNamed ==
    \A r \in refs :
        \/ head[r.c] # None /\ head[r.c].origin = <<r.p, r.at>>
        \/ \E h \in Hosts : /\ proc[h] # None /\ proc[h].op = "restore" /\ proc[h].b = r.c
                           /\ proc[h].og = <<r.p, r.at>>
        \/ r \in marked

\* A reference standing after its retirement is marked (TASK-395.14): a slow restore wrote it
\* again, the child deleted or its cell removed since. No record will name it, nor will its
\* child's cell's passes retire it again, skipping the dead cell, whose listing it changes not; its
\* parent's cell's listing it changes, and that cell's next pass deletes it, by its mark.
ReferencesRetiredMarked == \A r \in refs \cap retired : r \in marked

\* A cell a pass removed whole never lives again, nor is being made again (TASK-455): no restore
\* takes its ID. A deletion run again may leave it a record deleted.
RemovedStayGone == \A b \in gone : head[b] = None \/ Deleted(head[b])

\* No writer holds a deleted cell (TASK-455's review): a deletion's final head, its fence, stands
\* from its swap until the record says the cell is deleted, and after, while the record does; no
\* writer takes it, and the one that held it stops at its next swap.
DeletedUntaken ==
    \A h \in Hosts, b \in Cells :
        (Ready(h, b) /\ Taking(ses[h][b], b)) => head[b] # None /\ ~Deleted(head[b])

\* A segment a head names keeps its epoch while the head stands (TASK-449, board 6017): an
\* acknowledgement names more, a takeover goes on from the head's end, a deletion keeps what it
\* names, and nothing renames one; only a removal deletes a head, with its cell. So an origin keeps
\* the epoch of its parent's segment, and recovery reads the checkpoint there by its key, with no
\* read of its parent's head, whenever after.
NamedEpochsStay ==
    [][\A b \in Cells :
          head'[b] # None => \A s \in 0..(End(b) - 1) : s < End(b)' /\ Chain(b)'[s + 1] = Chain(b)[s + 1]
      ]_vars

\* Every fork made has its reference under its parent's cell, naming its origin (TASK-395):
\* written before it is made, retired only once it is deleted.
ReferencesStand ==
    \A b \in LiveRecs :
        head[b].origin # <<>> => RefOf(head[b].origin[1], head[b].origin[2], b) \in refs
=============================================================================
