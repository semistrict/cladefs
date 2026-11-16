------------------------------ MODULE Bucket ------------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Branches in object storage (backlog doc-7, crates/extent_bucket) with   *)
(* the host's write-ahead log of doc-14 (TASK-165.2): at the level of the  *)
(* protocol, what each step reads and writes in the bucket, and in what    *)
(* order. The simulation (extent_bucket's sim) samples interleavings, and  *)
(* found TASK-79.18 and 79.3 only after the fact; TLC exhausts small ones. *)
(*                                                                         *)
(* Keys are written once (TASK-407): every object a writer writes is at a  *)
(* key naming its writer's epoch, and becomes part of its branch only      *)
(* when the branch's head names it. The head, one object per branch, is    *)
(* the writer's taking and its commit: its epoch, the token and holder of  *)
(* its taking, the segments committed (`end`, and each one's epoch:        *)
(* `chain`),                                                               *)
(* and the newest checkpoint (`ckpt`), swapped with a condition (S3        *)
(* If-Match, GCS generation). A takeover swaps it to the next epoch, and   *)
(* the writer taken over fails at its next swap: the head is the fence.    *)
(* Readers read the head, then what it names, and nothing else; an object  *)
(* the head does not name, of an epoch below its, never will be named, and *)
(* the collector deletes it. No key is written twice, so readers need no   *)
(* versions of a key (KeysWrittenOnce).                                    *)
(*                                                                         *)
(* The bucket holds, per branch: segments, at <<seq, epoch>>, each a       *)
(* footer naming its writer's epoch, one commit (or none), an extent       *)
(* sealed into it or stored on its own, an extent freed, the branch's next *)
(* sequence number, a fork, and the run of a commit it goes on with, which *)
(* a later segment ends; extents stored on their own, at keys naming their *)
(* writer's epoch and the segment that names them; checkpoints, a state as *)
(* of a segment, at <<seq, epoch>>; records; heads; and the collector's    *)
(* moves. A segment's PUT is in flight from its upload until it lands,     *)
(* whenever, though its writer has gone.                                   *)
(*                                                                         *)
(* Hosts run writers. Opening a branch swaps its head to the next epoch    *)
(* and starts its recovery (the head's checkpoint, or the origin's state), *)
(* then reads the segments the head names; then the writer mints from its  *)
(* epoch's range, and puts its WAL after the branch's end (doc-14, Opening *)
(* a store: wal::rebase): the WAL's segments the head names are            *)
(* acknowledged, and the rest restamped after the head's end, if the       *)
(* branch's last segment is the last of those, or the branch's last commit *)
(* the commit the WAL started from; otherwise the WAL diverged, and is     *)
(* dropped, as `cladefsctl take` drops it. A commit is a segment of the WAL, *)
(* stamped by the chain; or, spanning segments (TASK-92.41), segments      *)
(* going on with it, a run of it in each, then the one it ends in. Readers *)
(* hold such runs until that one; writers acknowledge, and so heads name,  *)
(* only between commits. An upload creates its extent of its own, then     *)
(* puts its segment in flight; an acknowledgement swaps the head to name   *)
(* the WAL's prefix landed; then the children of its forks get their       *)
(* records. A host crashes keeping its WALs, to their last durable commits *)
(* with "cut" (local recovery keeping commits whole), or losing them.      *)
(* Placements give hosts fencing tokens. Branches are deleted by their     *)
(* placements (the head swapped to the final epoch, then the record, the   *)
(* children recorded), failed forks by their hosts (delete_fork), points   *)
(* forked by restores (the child's record, the point read, the record      *)
(* made); other branches take forks' children's IDs. Every branch is a     *)
(* vdisk of a cell (CellOf), whose prefix holds its objects. The           *)
(* collector runs passes per cell (TASK-395), each under its cell's        *)
(* collector lease, at once with other cells' and with writers: the cell's *)
(* listing and records, its live branches recovered from their heads, the  *)
(* states at their fork points of its branches' forks to other cells kept, *)
(* its records and references listed again, then compaction and deletion   *)
(* within the cell, and the references of its deleted children retired     *)
(* once nothing in it lives, and the cell removed whole if its records are *)
(* all given roots' and points' forks' (TASK-455). A fork to another cell  *)
(* writes a reference under its parent's cell, naming the parent, the fork *)
(* point and the child, which keeps the parent's whole state at the fork   *)
(* point there while it stands, whatever the child frees of it since: the  *)
(* collector is conservative.                                              *)
(*                                                                         *)
(* Each rule is a member of Rules; specs/bucket/mutations removes one      *)
(* each, and must find the violation it prevents (each mutation's model    *)
(* says which, and how):                                                   *)
(*                                                                         *)
(* - headSwap (NoHeadSwap): a writer acknowledges what it uploaded only by *)
(*   swapping the head with a condition, its own taking's: without it, a   *)
(*   writer taken over writes the head over its successor's, and the       *)
(*   successor's acknowledged commits are gone (TASK-407).                 *)
(* - epochKeys (NoEpochKeys): a segment's and a checkpoint's key names its *)
(*   writer's epoch: without it, two writers' PUTs land at one key, the    *)
(*   later over the earlier, which a head names (KeysWrittenOnce).         *)
(* - collectorEpochs (NoCollectorEpochs): the collector keeps, of a live   *)
(*   branch's objects its head does not name, those of the head's epoch    *)
(*   that it may yet name: segments and extents of their own at or past    *)
(*   its end, checkpoints newer than its own; all others are deleted, of   *)
(*   an epoch no swap of whose can succeed.                                *)
(* - movesByPass (NoMovesByPass): the key of moves names the pass that    *)
(*   made them (the cell's collector lease epoch), so that two passes'     *)
(*   compactions of one object never write one key; readers find them by  *)
(*   listing the object's moves. Without it, a pass slow to land its       *)
(*   create, after later passes made the same moves, then deleted them,    *)
(*   makes them again at their key, other bytes (TASK-407.1).              *)
(* - epochSequences (NoEpochSequences): a writer mints from its epoch's    *)
(*   range, having found the head its taking's after recovering.           *)
(* - recordsAfterAck (NoRecordsAfterAck): a fork's child gets its record   *)
(*   once the fork's segment is acknowledged.                              *)
(* - candidatesFirst (NoCandidatesFirst): the collector lists before it    *)
(*   reads records and heads.                                              *)
(* - forksFromStates (NoForksFromStates): the collector finds forks in     *)
(*   their parents' states, not only by their records.                     *)
(* - tokens (NoTokens): a head is taken only with a token as high.         *)
(* - deleteTokens (NoDeleteTokens): a branch is deleted only with a token  *)
(*   as high as its head's.                                                *)
(* - forkClaimsIds (NoForkClaimsIds): a failed fork's deletion takes its   *)
(*   child's ID with a record before it ends the child's head. The code's  *)
(*   rule is of forks made nowhere in the bucket (create_forked), which    *)
(*   doc-14 removes; here every fork has an origin, and the rule is what   *)
(*   it says: without it, the deletion of a child with no record deletes   *)
(*   it directly (TASK-79.82).                                             *)
(* - forkClaimsLanded (NoForkClaimsLanded): that record lives only if the  *)
(*   fork's parent's head names the fork's segment, and says it is a claim.*)
(* - pointsTakeNoWriter (NoPointsTakeNoWriter): no writer takes a point.   *)
(* - forksCheckPoints (NoForksCheckPoints): a fork of a point reads the    *)
(*   point's record after it creates its child's.                          *)
(* - forksPending (NoForksPending): that record says the fork is being     *)
(*   made until then.                                                      *)
(* - collectorAbandons (NoCollectorAbandons): the collector gives up on    *)
(*   forks being made of points deleted.                                   *)
(* - recordsRelisted (NoRecordsRelisted): the collector lists the records  *)
(*   again after reading states with checkpoints of their own. Since       *)
(*   TASK-395.1, a point's fork's reference, listed after the states,      *)
(*   finds it too: its mutation removes references with it.                *)
(* - recordsOwned (NoRecordsOwned): records name their makings' owners.    *)
(* - waysFromSeals (NoWaysFromSeals): the collector keeps every moves on   *)
(*   the way to an extent from where it was sealed, unless the state       *)
(*   naming it is from a checkpoint of the head's epoch (TASK-175).        *)
(* - movesTakenFirst (NoMovesTakenFirst): a writer names what its          *)
(*   checkpoint moves where it went before it writes the checkpoint, only  *)
(*   forward along the way (TASK-175).                                     *)
(* - passesFencedStop (NoPassesFencedStop): a pass that finds what a live  *)
(*   branch needs lost stops, without failing, if a later pass took the    *)
(*   collector's lease (TASK-180).                                         *)
(* - recoveriesByOrigins (NoRecoveriesByOrigins): the collector recovers a *)
(*   branch by each origin it is named by, its record's and its parent's   *)
(*   state's, not once: a child recovered by the state, that fork refused, *)
(*   may have a record of a restore's origin, found on relisting           *)
(*   (TASK-173). Since TASK-395.1, the restore's reference keeps its       *)
(*   origin's state at the fork point too: its mutation removes references *)
(*   with it.                                                              *)
(* - recordsKept (NoRecordsKept): the collector removes a record only with *)
(*   its cell, dead (no live branch, no reference, no mark of its own),    *)
(*   and only if every record of the cell is one no recorder makes again:  *)
(*   a given root's (Given) or a given point's fork's, made, by its        *)
(*   restore alone. A writable fork's child's, a point's, a claim's, a     *)
(*   deletion's of no record and a point's fork's never made stay, for     *)
(*   good: a parent's writer slow to record a fork, or its next writer,    *)
(*   would make the child's again, a deleted branch live again, naming     *)
(*   what the collector reclaimed (TASK-174, TASK-455). Without the rule,  *)
(*   any dead cell's go (RemovedStayGone).                                 *)
(* - deletionsDoneByOthers (NoDeletionsDoneByOthers): a deletion whose     *)
(*   recovery of the branch finds what it needs lost, the record deleted   *)
(*   since, is done (TASK-178; branch::delete, with no field of Rules).    *)
(* - walFollows (NoWalFollows): a WAL is put after the branch only if the  *)
(*   head's last segment is the WAL's last it names, or, naming none of    *)
(*   them, the branch's last commit is the commit the WAL started from.    *)
(* - acksBetweenCommits (NoAcksBetweenCommits): a writer acknowledges      *)
(*   segments only to the last going on with no commit                     *)
(*   (Writer::acknowledge), so that its WAL keeps a commit whole until the *)
(*   bucket has it whole, and a head names no commit begun.                *)
(* - wholeCommitsKept (NoWholeCommitsKept): a host's local recovery keeps  *)
(*   a commit spanning segments only if consecutive segments end it, and   *)
(*   closes the first again without it otherwise (extent_store's           *)
(*   wal::recover): a store whose log ends inside a commit refuses to      *)
(*   open.                                                                 *)
(* - references (NoReferences): a fork of a branch to a child in another   *)
(*   cell, a writable fork or a fork of a point, has a reference under the *)
(*   parent's cell, which that cell's passes keep the parent's state at    *)
(*   the fork point for: its pass lists nothing of the child's cell, and   *)
(*   the parent deleted, would delete what the child inherits.             *)
(* - referencesFirst (NoReferencesFirst): a writable fork's reference is   *)
(*   written before its child's record, by whoever writes the record: its  *)
(*   writer, the next, or the parent's deletion, before that marks the     *)
(*   parent deleted. Without it, a pass reading the parent deleted finds   *)
(*   no reference.                                                         *)
(* - forksReferFirst (NoForksReferFirst): a fork of a point writes its     *)
(*   reference before its record and before it reads the point's record:   *)
(*   one that read the point live then wrote it before the point's         *)
(*   deletion, which a pass reading the point deleted, or finding its      *)
(*   reference retired, read after.                                        *)
(* - referencesHeld (NoReferencesHeld): a reference is retired, by the     *)
(*   pass of its child's cell, only once that cell has no live branch and  *)
(*   no reference of its own: the child's forks inherit what it did, kept  *)
(*   by its reference.                                                     *)
(* - referencesRelisted (NoReferencesRelisted): a pass lists its cell's    *)
(*   references after the states, as it lists the records again: listed    *)
(*   with the candidates, it misses a fork's of a point a checkpoint       *)
(*   dropped since.                                                        *)
(* - forkPointsKept (NoForkPointsKept): a pass keeps, of the parent's      *)
(*   state at a referenced fork point, what recovering it reads, not only  *)
(*   the objects holding its extents: a child with no checkpoint of its    *)
(*   own recovers through it.                                              *)
(* - forksRecordFirst (NoForksRecordFirst): a fork of a point creates its  *)
(*   record, being made, before its reference (and reads the point's after *)
(*   both, forksReferFirst): a reference always has a record naming its    *)
(*   origin, by which its child's cell's pass retires it, whatever comes   *)
(*   of the fork. Written first, a host dying between leaves a reference   *)
(*   no record names, which no pass retires (ReferencesNamed).             *)
(* - referencesTakenBack (NoReferencesTakenBack): whoever finds the record *)
(*   of a writable fork's child naming another origin, or none, deletes    *)
(*   the fork's reference: as it writes the record and finds another's     *)
(*   there, or as a later writer or the parent's deletion records the fork *)
(*   again. Without it, a reference whose child's ID another branch took   *)
(*   between its writer's read of the child's record and its create stays  *)
(*   for ever (ReferencesNamed).                                           *)
(* - retirementsMarked (NoRetirementsMarked): a reference retired, or      *)
(*   taken back, is marked, a mark kept for good under its parent's cell   *)
(*   beside it, before it is deleted; a pass deletes the references of its *)
(*   cell it lists marked. A writer of the reference whose read of the     *)
(*   child's record came before the retirement, none there, writes it      *)
(*   after: one a stale recorder wrote after another's take-back, dying    *)
(*   before its create, which no record names (TASK-395.13); one written   *)
(*   again after its child's cell's pass retired it, which that cell's     *)
(*   passes, skipping it, the cell dead and unchanged, never retire again  *)
(*   (TASK-395.14). Its write changes its parent's cell's listing, whose   *)
(*   next pass deletes it, by its mark (ReferencesRetiredMarked). A writer *)
(*   finding the child's record live and its own writes none: its maker's  *)
(*   came first (referencesFirst), and stands.                             *)
(* - idsGivenOnce (NoIdsGivenOnce): the IDs the bucket's allocators give   *)
(*   (Given, doc-7 Object IDs) no fork names, nor a contest: each goes to  *)
(*   one making, a root or a restore, and to no other ever, its cell       *)
(*   removed or not. Without it, a fork's child, named by a guest, may be  *)
(*   a given point's fork's ID, recorded by the fork's slow writer after   *)
(*   its cell went (RemovedStayGone).                                      *)
(* - forksMadeUnmarked (NoForksMadeUnmarked): a reference is retired       *)
(*   unmarked only if its child is a given point's fork made: its restore, *)
(*   the only writer of it, wrote it before it made the fork, and no       *)
(*   recorder writes it, no fork naming the ID; so a base's cell keeps no  *)
(*   mark of a point's fork made and gone (TASK-455). Any other is marked  *)
(*   first (retirementsMarked). Without the rule, every point's fork's     *)
(*   goes unmarked, one being made among them, whose restore, slow, writes *)
(*   its reference after its retirement (ReferencesRetiredMarked).         *)
(* - fencesKept (NoFencesKept): a head of a branch whose record a pass    *)
(*   did not read goes only if it is not final: a stale taker's, of a      *)
(*   branch removed. A final one is a deletion's fence, of a fork's child  *)
(*   whose record is yet to come, the deletion's own or its parent's slow  *)
(*   writer's: it stays, and keeps its cell from removal. Without it, a    *)
(*   pass over a cell of no record deletes it, the parent's writer         *)
(*   records the child live, and a writer takes it, which the deletion,    *)
(*   going on, marks deleted under it (DeletedUntaken; TASK-455's review). *)
(* - resumesRefer (NoResumesRefer): a fork of a point resumed by another   *)
(*   than the restore that began it, a store opening its child, writes its *)
(*   reference before it makes it, as the restore does. Without it, a      *)
(*   restore dying between its record and its reference leaves a fork made *)
(*   of no reference, whose origin's state at the fork point no pass keeps *)
(*   once the point is gone (ReferencesStand).                             *)
(* - resumesMarked (NoResumesMarked): a fork of a point resumed says so in *)
(*   its record, with a condition, before its reference, and a fork made   *)
(*   so retires its reference marked: the restore that began it, slow, may *)
(*   write it after, the fork made, deleted and its cell removed. A resume *)
(*   finding it made writes none. Without it, retired unmarked, it stands  *)
(*   for good (ReferencesRetiredMarked; TASK-455's review).                *)
(* - versionsKept (NoVersionsKept): a pass keeps, for each version a live  *)
(*   branch's head names, the branch's state at it whole, and what         *)
(*   recovering it reads, as for a fork point (TASK-476, doc-18). Without  *)
(*   it, a checkpoint since covers the version's segments, which the pass  *)
(*   deletes (NothingNeededGone).                                          *)
(* - versionsCarried (NoVersionsCarried): a takeover's swap names the      *)
(*   versions the head named, as it names its segments. Without it, they   *)
(*   are gone though no writer dropped them (VersionsStay).                *)
(* - versionsAcked (NoVersionsAcked): a head names a version only with the *)
(*   segment its cut closed, by the acknowledgement naming that segment.   *)
(*   Without it, a swap names it as its cut is committed, before its       *)
(*   segment lands, a state the head does not hold (NothingNeededGone).    *)
(*                                                                         *)
(* Versions (TASK-476, doc-18): a version is a name for a branch's state  *)
(* as of a segment its head names, kept while the head names it, in the   *)
(* branch's own cell and of no record. Its cut is a commit closing a      *)
(* segment whose footer names it (its ID, the commit's tag here); the     *)
(* acknowledgement naming the segment names it in the head; its writer    *)
(* drops it by a swap; a deletion's final head names none. A restore of   *)
(* one is a point's fork's steps, the branch's head read in place of the  *)
(* point's record. A version's cut is of a quiescent store, and seals     *)
(* nothing. Cells' cuts, versions of several vdisks cut at one moment,    *)
(* are a host's work alone (doc-18): not modelled.                        *)
(*                                                                         *)
(* Gone with the head (TASK-407), with what they guarded: a create where   *)
(* nothing is as a writer's fence, the fence segment a takeover made, gaps *)
(* filled, void and late segments, the collector's pins of where a writer  *)
(* taken over could still create (TASK-388), a lease read before a fork's  *)
(* acknowledgement, readers listing checkpoints again, a deletion's fence  *)
(* and its read of the record again, readers going by the footer of the   *)
(* object at an extent's key rather than their state (TASK-79.18), the     *)
(* collector keeping extents of their own by the segments their keys name, *)
(* the WAL's rules for fences, and readers holding a commit's runs as a    *)
(* rule, as a fence could leave it unended: a head names only what its     *)
(* writer acknowledged, whole commits, and a key only one object.          *)
(*                                                                         *)
(* Where this differs from doc-14 and the code, by what TLC found:         *)
(*                                                                         *)
(* - doc-14 put the WAL after the branch by segments: WAL segments whose   *)
(*   footers recovery read were acknowledged. Here by commits, as doc-7's  *)
(*   recovery on the same host was. The code's segments of forks alone,    *)
(*   making no commit, are none here: a fork is a commit's.                *)
(*                                                                         *)
(* Steps are what the code does between two requests, or merged: a         *)
(* writer's taking reads the record, swaps the head and reads the record   *)
(* again in one step, the first read of it from before the branch's        *)
(* deletion if "staleTake" is in Can; it reads the head's checkpoint then, *)
(* and the segments the head names in a step after. The collector's        *)
(* recoveries read the bucket at once, a deletion's in one step after its  *)
(* swap. Forks' children are recorded in a step after their parents'       *)
(* acknowledgements. A checkpoint's PUT lands after its writer took its    *)
(* moves, in a step of its own, while its writer's session lasts, and its  *)
(* swap into the head in another. Passes overlap if "overlap" is in Can,   *)
(* of any cells, a later one of a cell taking its collector lease;         *)
(* MaxPasses bounds them all. A pass reads its cell's records once each,   *)
(* as listings find them, until one finds none new, then once again each  *)
(* after the states (recordsRelisted), when it lists its references too.   *)
(* Reads of other cells' objects (a parent's state, a point's record) are  *)
(* a pass's GETs; it lists only its own cell, and deletes only there, but  *)
(* for the references it retires, whose keys its deleted children's        *)
(* records give. Listings are atomic, as before.                           *)
(*                                                                         *)
(* Can bounds what happens: each kind of step needs its name in it, so     *)
(* that a model checks some at a time and a mutation only what it needs.   *)
(* Moves go two hops at most: a segment's, then those moves'. A commit     *)
(* spans three segments at most, two going on with it, each a run of one   *)
(* extent, the one ending it holding nothing else (the code's may hold its *)
(* frees and more). A segment ends a commit and goes on with the next only *)
(* with "spanOn" in Can. Not modelled: the collector's grace period        *)
(* (correctness does not depend on it, doc-7) and its compaction threshold *)
(* (it compacts any object it may, by choice), and PUTs failing.           *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Hosts,          \* the hosts
    Branches,       \* branch IDs
    Root,           \* the branch the bucket starts with
    Segs,           \* segments per branch: 0..Segs-1
    MaxTakes,       \* head takings, all branches'
    MaxCommits,     \* commits
    MaxMints,       \* extent IDs minted
    MaxPasses,      \* passes of the collector
    MaxPlaces,      \* placements
    MaxCrashes,     \* host crashes
    Can,            \* the kinds of step the model takes
    None,           \* a model value: nothing
    Rules,          \* the protocol's rules in force (all, but in a mutation)
    CellOf,         \* CellOf[b]: the cell b is a vdisk of, whose prefix holds its objects
    Given           \* the IDs the bucket's allocators give (objectid), each to one making alone:
                    \* the root's, if given, and those of restores that take one (idsGivenOnce)

AllRules == {"headSwap", "epochKeys", "collectorEpochs", "epochSequences", "recordsAfterAck",
             "candidatesFirst", "forksFromStates", "tokens", "deleteTokens", "forkClaimsIds",
             "forkClaimsLanded", "pointsTakeNoWriter", "forksCheckPoints", "forksPending",
             "collectorAbandons", "recordsRelisted", "recordsOwned", "waysFromSeals",
             "movesTakenFirst", "passesFencedStop", "recoveriesByOrigins", "recordsKept",
             "deletionsDoneByOthers", "walFollows", "acksBetweenCommits",
             "wholeCommitsKept", "references", "referencesFirst", "forksReferFirst",
             "referencesHeld", "referencesRelisted", "forkPointsKept", "forksRecordFirst",
             "referencesTakenBack", "retirementsMarked", "movesByPass", "idsGivenOnce",
             "forksMadeUnmarked", "fencesKept", "resumesRefer", "resumesMarked",
             "versionsKept", "versionsCarried", "versionsAcked"}

AllCan == {"commit", "mint", "seg", "own", "free", "fork", "point", "restore", "unfork",
           "contest", "delete", "deletePoints", "place", "checkpoint", "collect", "compact",
           "compactMoves", "crash", "disk", "staleTake", "overlap", "span", "spanOn", "cut",
           "forkOwn", "deleteRoot", "landInOrder", "forkNext", "deleteEarly", "resume",
           "version", "dropVersion", "restoreVersion"}

ASSUME Rules \subseteq AllRules
ASSUME Can \subseteq AllCan
ASSUME Root \in Branches
ASSUME CellOf \in [Branches -> Nat \ {0}]
ASSUME Given \subseteq Branches

\* The maps of branches to cells a model's CellOf may be: one cell, which is the whole-bucket
\* collector before TASK-395; a cell each, as hosts make them (a store, a fork, a point and a
\* restore each the one vdisk of a new cell); and the root sharing its cell with branch 2, the
\* others a cell each, for forks within a cell and across cells in one model.
OneCell == [b \in Branches |-> 1]
CellEach == [b \in Branches |-> b]
Pair == [b \in Branches |-> IF b <= 2 THEN 1 ELSE b]

\* The IDs given, as a model's Given may be: the root's alone, every other ID one a guest names;
\* or none, every ID a guest's, as before TASK-455, which removes no cell: for models of rules
\* removal changes nothing of, kept to the size they had.
RootGiven == {Root}
NoneGiven == {}

Cells == {CellOf[b] : b \in Branches}
CellBranches(c) == {b \in Branches : CellOf[b] = c}
\* Whether a fork of p to c crosses cells: c's cell's collector reads nothing of p's.
Cross(p, c) == CellOf[p] # CellOf[c]

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
    rec,        \* rec[b]: b's record, or None
    head,       \* head[b]: [epoch, token, holder, end, chain, ckpt, vers], or None: the holder its
                \* taking's, unique; the segments 0..end-1 committed, chain[s + 1] the epoch of s;
                \* ckpt the newest checkpoint, <<s, e>>, or <<>>; vers its versions, each [id, at]:
                \* the ID its cut's footer names (its commit's tag), and the segment it is at
    moves,      \* moves[b][s][e][p]: the moves the collector's passes p made of segment <<s, e>> of
                \* b: p = <<i>>, pass i's moves of the segment; <<i, j>>, pass j's of those; or None.
                \* Each [pass, names]: its pass, and the extents it holds
    mflight,    \* the moves' PUTs in flight: [k, o], at the key k, the moves o
    inflight,   \* the segments' PUTs in flight: [h, b, g, s, o]
    refs,       \* the references of forks across cells, [p, at, c]: under p's cell, naming p, the
                \* fork point at, and the child c
    marked,     \* the marks of the references retired or taken back, [p, at, c], under p's cell,
                \* kept (retirementsMarked): a reference marked is garbage, written again or not
    retired,    \* what happened: the references retired or taken back (kept with the bucket's
                \* variables, as it changes with them)
    written,    \* what happened: every <<key, object>> written at a segment's, a checkpoint's or
                \* moves' key (KeysWrittenOnce)
    gone,       \* what happened: the branches whose records a pass removed with their cells
                \* (recordsKept), which their IDs, given once, never name again
    \* The hosts.
    ses,        \* ses[h][b]: h's writer of b, or None
    wal,        \* wal[h][b]: h's WAL of b, a sequence of [s, o]: kept by a crash, or lost
    tok,        \* tok[h][b]: the fencing token of h's placement of b
    proc,       \* proc[h]: h's deletion, failed fork's deletion or restore, or None
    \* The collector.
    gc,         \* gc[i]: pass i, of the cell gc[i].cell, whose collector lease epoch is i: a cell's
                \* collector lease is its last pass's
    \* What happened.
    commits,    \* commits[t]: [branch, parent, seals, free]: its branch, the commit before it, what
                \* it sealed and freed
    nextTag,
    acked,      \* acked[b]: the commits a writer of b acknowledged
    minted,     \* the extent IDs minted
    twice,      \* whether an ID was minted twice
    makers,     \* makers[b]: the makings of b told they made it
    doomed,     \* the makings a deletion of was asked by whoever may
    taken,      \* taken[b]: the highest token that took b's head, -1 if none did
    placed,     \* placed[b]: the last placement's token
    errors,     \* what failed that must not
    \* Bounds.
    gen, takes, passes, places, crashes, mints

bucketVars == <<seg, own, ckpt, rec, head, moves, mflight, inflight, refs, marked, retired, written, gone>>
hostVars == <<ses, wal, tok, proc>>
truthVars == <<commits, nextTag, acked, minted, twice, makers, doomed, taken, placed, errors>>
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

\* Whether a guest may name c, a fork's child's ID or a contest's: one with no record, and none an
\* allocator gives (idsGivenOnce), which goes to its one making alone; without the rule, a given one
\* too, its record removed or not yet made.
Nameable(c) == rec[c] = None /\ (R("idsGivenOnce") => c \notin Given)

\* What a record writes for the owner `o` of its making: nothing, without recordsOwned.
Owner(o) == IF R("recordsOwned") THEN o ELSE <<"anyone">>

\* A record: its kind ("root", "fork", "point", "forking", "claim"), its origin <<parent, at>>,
\* the point a fork being made is of, or the version, <<branch, id>> (<<>> if none), its owner,
\* whether deleted, and, of a fork of a point or a version, whether another than the restore that
\* began it resumed it (resumesMarked); and, not in the bucket's bytes, the commit its branch
\* starts from and the making it is of.
Record(kind, origin, point, o, base) ==
    [kind |-> kind, origin |-> origin, point |-> point, ver |-> <<>>, owner |-> Owner(o),
     deleted |-> FALSE, resumed |-> FALSE, base |-> base, maker |-> o]

\* Whether r is the record of the child of the fork whose owner is o.
ChildOf(r, o) == r.kind # "root" /\ r.owner = Owner(o)

\* How many times the collector moves what was in a segment: into its moves, then those moves'.
Hops == 2
\* The passes that may make moves, one after another from a segment: <<i>>, then <<i, j>>.
Passes == 1..MaxPasses
Ways == {<<i>> : i \in Passes} \cup {<<i, j>> : i, j \in Passes}
\* The pass a moves' key names: the one that made them (movesByPass); without the rule, none,
\* every pass's moves of an object at one key, as before TASK-407.1.
KP(i) == IF R("movesByPass") THEN i ELSE 1

\* The keys of segment s of b by the writer of epoch e, of its moves along the way p from it
\* (p = <<>>, the segment itself), and of b's checkpoint as of s by that writer; a key's way, and
\* its depth.
SegKey(b, s, e) == <<"seg", b, s, KE(e)>>
Key(b, s, e, p) == IF p = <<>> THEN <<"seg", b, s, e>> ELSE <<"moves", b, s, e, p>>
CkptKey(b, s, e) == <<"ckpt", b, s, KE(e)>>
Way(loc) == IF loc[1] = "seg" THEN <<>> ELSE loc[5]
Depth(loc) == Len(Way(loc))

\* The object at the key k of a segment, a checkpoint or moves, or None.
Obj(k) == CASE k[1] = "seg" -> seg[k[2]][k[3]][k[4]]
            [] k[1] = "ckpt" -> ckpt[k[2]][k[3]][k[4]]
            [] k[1] = "moves" -> moves[k[2]][k[3]][k[4]][k[5]]

\* A state: its extents' locations (the keys holding them), its last commit, its next sequence
\* number, its forks, the last segment applied, the runs of the commit the segments applied last
\* went on with (State::begun: [commit, runs], runs the locations of its extents; or None), and,
\* not in the bucket's bytes, the commits applied and whether one was applied twice.
Fresh == [ext |-> Nothing, tip |-> 0, next |-> <<0, 0>>, forks |-> {}, through |-> -1,
          begun |-> None, hist |-> {}, dup |-> FALSE]

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
                  !.forks = IF o.fork = None THEN st.forks
                            ELSE st.forks \cup {[child |-> o.fork.child, at |-> s,
                                                 owner |-> o.fork.owner,
                                                 point |-> o.fork.point]},
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

\* Whether the version v, <<branch, id>>, is gone: its branch's head names it no more, dropped by
\* its writer or ended by a deletion, or never did.
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
    ELSE IF rec[b] = None THEN [ok |-> FALSE, st |-> Fresh, touched |-> {}]
    ELSE IF rec[b].origin = <<>> THEN FollowChain(b, Fresh, 0, at + 1, Chain(b), {<<"rec", b>>})
    ELSE LET p == StateAt(rec[b].origin[1], rec[b].origin[2]) IN
         IF ~p.ok THEN p
         ELSE FollowChain(b, Inherit(p.st), 0, at + 1, Chain(b), p.touched \cup {<<"rec", b>>})

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

\* Whether b's head names its segment at as the one by the writer of epoch e, b holding it, as a
\* fork's recovery from there needs.
Holds(b, at, e) == at < End(b) /\ Chain(b)[at + 1] = e /\ StateAt(b, at).ok

\* Whether no writer has b: no head, or its deletion's.
Unwritten(b) == head[b] = None \/ head[b].epoch = Final

\* Whether b's head is the taking of the writer whose session is s: its epoch, and its holder,
\* as its writer last swapped it (an ETag's condition).
Taking(s, b) == head[b] # None /\ head[b].epoch = s.epoch /\ head[b].holder = s.gen

\* Whether segment o's footer names a run of x in its data: of a commit it ends, or of the one it
\* goes on with (Contents::runs).
Runs(o, x) == o.ext = x \/ (o.cont # None /\ o.cont.ext = x)

\* Whether the object along the way p from segment <<s, e>> of b names x: the segment's footer, or
\* the moves' map.
Names(b, s, e, p, x) ==
    IF p = <<>> THEN seg[b][s][e] # None /\ Runs(seg[b][s][e], x)
    ELSE moves[b][s][e][p] # None /\ x \in moves[b][s][e][p].names

\* The ways one hop on from p: the moves of it any pass may have made.
On(p) == IF Len(p) >= Hops THEN {} ELSE {p \o <<KP(i)>> : i \in Passes}

\* Whether a reader of extent x at loc reads x's bytes: the object at loc's key names it, or, that
\* object gone, moves of it that do, listed (fetch's read), one hop at a time.
Readable(x, loc) ==
    IF loc[1] = "own" THEN [b |-> loc[2], e |-> loc[3], s |-> loc[4], x |-> x] \in own
    ELSE \E p \in {Way(loc)} \cup On(Way(loc)) : Names(loc[2], loc[3], loc[4], p, x)

RECURSIVE Followed(_, _, _, _, _)
\* The way from segment <<s, e>> of b to x, following moves that name it on from p.
Followed(x, b, s, e, p) ==
    LET next == {q \in On(p) : Names(b, s, e, q, x)} IN
    IF next = {} THEN p ELSE Followed(x, b, s, e, CHOOSE q \in next : TRUE)

\* Where a checkpoint records x, at loc, to be: where the collector moved it.
Moved(x, loc) ==
    IF loc[1] = "own" THEN loc
    ELSE Key(loc[2], loc[3], loc[4], Followed(x, loc[2], loc[3], loc[4], Way(loc)))

\* Whether loc2 is further along an extent's way than loc (layout::further): moves of it, or of
\* those moves.
Further(loc, loc2) == /\ loc[1] # "own" /\ loc2[1] = "moves"
                      /\ loc2[2] = loc[2] /\ loc2[3] = loc[3] /\ loc2[4] = loc[4]
                      /\ Depth(loc2) > Depth(loc)
                      /\ SubSeq(Way(loc2), 1, Depth(loc)) = Way(loc)

\* The state st, its extents named where the state ckptSt names them if that is further along their
\* ways (Writer::take_moves).
TakeMoves(st, ckptSt) ==
    [st EXCEPT !.ext = [x \in DOMAIN st.ext |->
                          IF x \in DOMAIN ckptSt.ext /\ Further(st.ext[x], ckptSt.ext[x])
                          THEN ckptSt.ext[x]
                          ELSE st.ext[x]]]

-----------------------------------------------------------------------------
(* The truth. *)

RECURSIVE Anc(_)
\* The commits up to t.
Anc(t) == IF t = 0 THEN {} ELSE {t} \cup Anc(commits[t].parent)

RECURSIVE OwnStart(_, _)
\* Where the path of b's own commits to t starts: the commit before the first.
OwnStart(b, t) == IF t # 0 /\ commits[t].branch = b THEN OwnStart(b, commits[t].parent) ELSE t

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

Made == UNION {makers[b] : b \in Branches}

\* A writer's guest's state: what it acknowledged, then its WAL.
View(h, b) == ApplyAll(ses[h][b].st, b, wal[h][b])

-----------------------------------------------------------------------------
(* Records. *)

\* The record of the child of parent's fork f, the records being rs, given by whoever gives it
\* (record_fork): its writer, the next, or its parent's deletion. Returns the record and whether
\* the fork stands, the record its making's; the fork's commit is what the child starts from.
RecordFork(rs, parent, f) ==
    LET r == rs[f.child]
        new == Record(IF f.point THEN "point" ELSE "fork", <<parent, f.at>>, 0, f.owner,
                      f.owner[2])
    IN IF r = None THEN [rec |-> new, made |-> TRUE]
       ELSE [rec |-> r, made |-> ~r.deleted /\ ChildOf(r, f.owner)]

\* The fork in F made first.
FirstFork(F) == CHOOSE f \in F : \A g \in F : f.at <= g.at

\* Whether b's record says it lives: a root, a fork's child, a point.
LiveRec(b) == rec[b] # None /\ ~rec[b].deleted /\ rec[b].kind \in {"root", "fork", "point"}

\* The reference of the fork of p at its segment at to c, under p's cell (TASK-395).
RefOf(p, at, c) == [p |-> p, at |-> at, c |-> c]

\* Whether parent's fork f needs a reference: its child is in another cell (references).
NeedsRef(parent, f) == R("references") /\ Cross(parent, f.child)

\* The references of the forks F of parent.
RefsOf(parent, F) == {RefOf(parent, f.at, f.child) : f \in {f \in F : NeedsRef(parent, f)}}

\* The references under cell c.
RefsUnder(c) == {r \in refs : CellOf[r.p] = c}

\* Whether the record r is of the child of parent's fork f, live: its writers write its reference
\* (again), having read it so (record_fork reads the child's record first, TASK-395.3).
Refers(r, f) == r = None \/ (ChildOf(r, f.owner) /\ ~r.deleted)

\* The reference of the fork of p at at to c taken back, the record r of c found naming another
\* origin, or none (referencesTakenBack): no record will name it.
TakenBack(p, at, c, r) ==
    IF R("references") /\ R("referencesTakenBack") /\ r # None /\ r.origin # <<p, at>>
    THEN {RefOf(p, at, c)} ELSE {}

\* The marks of the references Rs retired or taken back (retirementsMarked): kept for good under
\* their parents' cells, so that one a stale writer writes again after, which no recorder nor
\* pass of its child's cell would look at again, its parent's cell's next pass deletes.
Marks(Rs) == IF R("retirementsMarked") THEN Rs ELSE {}

\* What writers of the records of parent's forks F to other cells write of their references, and
\* take back, reading the records as they are: without referencesFirst, once recorded.
RefsWritten(parent, F) ==
    {RefOf(parent, f.at, f.child) : f \in {f \in F : NeedsRef(parent, f) /\ Refers(rec[f.child], f)}}
\* The forks of F whose references their writers write, reading their children's records as they
\* are, with referencesFirst: none there. One there of the child, live, its maker's reference
\* came before it, and stands (ReferencesStand): written again, it might land after its
\* retirement, the child deleted since the read (TASK-395.14).
ToRefer(parent, F) == {f \in F : NeedsRef(parent, f) /\ rec[f.child] = None}
\* Those taken back by the records rs: as recorded, each fork's record found or made in turn.
RefsTakenBackIn(rs, parent, F) ==
    UNION {TakenBack(parent, f.at, f.child, rs[f.child]) : f \in {f \in F : NeedsRef(parent, f)}}
RefsTakenBack(parent, F) == RefsTakenBackIn(rec, parent, F)

RECURSIVE RecordAll(_, _, _, _)
\* The records rs, the forks F of parent recorded, one after another; and the makings told.
RecordAll(rs, ms, parent, F) ==
    IF F = {} THEN [rec |-> rs, makers |-> ms]
    ELSE LET f == FirstFork(F)
             r == RecordFork(rs, parent, f)
         IN RecordAll([rs EXCEPT ![f.child] = r.rec],
                      IF r.made THEN [ms EXCEPT ![f.child] = @ \cup {f.owner}] ELSE ms,
                      parent, F \ {f})

-----------------------------------------------------------------------------
(* Writers. *)

Stopped(h, b) == ses' = [ses EXCEPT ![h][b].pc = "stopped"]

\* Whether the writer of b whose session is s, its state st, has forks acknowledged whose children
\* it hasn't recorded, or, in other cells, written the references of.
Unrecorded(b, st, s) ==
    \E f \in st.forks : f.child \notin s.recorded \/ (NeedsRef(b, f) /\ f.child \notin s.reffed)

\* The head the writer whose session is s writes: its taking, with what it committed.
HeadOf(s) == [epoch |-> s.epoch, token |-> s.token, holder |-> s.gen, end |-> s.end,
              chain |-> s.chain, ckpt |-> s.hckpt, vers |-> s.vers]

\* s, the session of a writer of b, its recovery started from b's head H as it swapped it: from
\* the head's checkpoint, or its origin's state; stopped, what that needs gone.
Started(s, b, H) ==
    LET p == StateAt(s.origin[1], s.origin[2])
        newest == IF H = None THEN <<>> ELSE H.ckpt
        s1 == [s EXCEPT !.end = IF H = None THEN 0 ELSE H.end,
                        !.chain = IF H = None THEN <<>> ELSE H.chain,
                        !.hckpt = newest,
                        !.vers = IF H = None \/ ~R("versionsCarried") THEN {} ELSE H.vers]
    IN IF newest # <<>> THEN
          IF Obj(CkptKey(b, newest[1], newest[2])) = None THEN [s1 EXCEPT !.pc = "stopped"]
          ELSE [s1 EXCEPT !.pc = "read", !.st = Obj(CkptKey(b, newest[1], newest[2])),
                          !.at = newest[1] + 1]
       ELSE IF s.origin = <<>> THEN [s1 EXCEPT !.pc = "read", !.st = Fresh, !.at = 0]
       ELSE IF p.ok THEN [s1 EXCEPT !.pc = "read", !.st = Inherit(p.st), !.at = 0]
       ELSE [s1 EXCEPT !.pc = "stopped"]

\* h takes b's head, as a writer of b, of its placement's token, its taking's holder, once it
\* read b's record: swapped to the next epoch, what it names kept. Then, the record read again,
\* its recovery starts, or, deleted since, it stops (Writer::take). With "staleTake", the first
\* read is from before b's deletion, whose head says so: the swap is refused.
Open(h, b) ==
    /\ takes < MaxTakes
    /\ IF ses[h][b] = None THEN TRUE ELSE ses[h][b].pc = "stopped"
    /\ rec[b] # None
    /\ ~rec[b].deleted \/ ("staleTake" \in Can /\ rec[b].maker \in makers[b])
    /\ IF rec[b].kind = "point" THEN ~R("pointsTakeNoWriter") ELSE rec[b].kind \in {"root", "fork"}
    \* Not deleted's; nor, its token higher, a later placement's (tokens).
    /\ IF head[b] = None THEN TRUE
       ELSE head[b].epoch # Final /\ (R("tokens") => head[b].token <= tok[h][b])
    /\ LET e == IF head[b] = None THEN 1 ELSE head[b].epoch + 1
           s == [gen |-> gen + 1, pc |-> "read", epoch |-> e, token |-> tok[h][b],
                 origin |-> rec[b].origin, st |-> Fresh, at |-> 0, end |-> 0, chain |-> <<>>,
                 hckpt |-> <<>>, seq |-> 0, next |-> <<0, 0>>, up |-> {}, done |-> {}, open |-> {},
                 recorded |-> {}, reffed |-> {}, reading |-> {}, ckptThrough |-> -1,
                 ckptPut |-> None, durable |-> None, vers |-> {}]
       IN /\ head' = [head EXCEPT ![b] = [epoch |-> e, token |-> tok[h][b], holder |-> gen + 1,
                                          end |-> End(b), chain |-> Chain(b), ckpt |-> HeadCkpt(b),
                                          vers |-> IF R("versionsCarried") THEN HeadVers(b)
                                                   ELSE {}]]
          /\ ses' = [ses EXCEPT ![h][b] = IF rec[b].deleted THEN [s EXCEPT !.pc = "stopped"]
                                          ELSE Started(s, b, head[b])]
    /\ taken' = [taken EXCEPT ![b] = IF tok[h][b] > @ THEN tok[h][b] ELSE @]
    /\ gen' = gen + 1
    /\ takes' = takes + 1
    /\ UNCHANGED <<seg, own, ckpt, rec, moves, mflight, inflight, refs, marked, retired, written, gone, wal, tok,
                   proc, gc>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, doomed, placed, errors>>
    /\ UNCHANGED <<passes, places, crashes, mints>>

\* With "staleTake", a taking of b whose first read of b's record came before b's deletion, b's
\* cell removed since (recordsKept): its swap finds no head and creates one, its read of the record
\* again finds none, and it stops, leaving the head: garbage, which a pass over the cell deletes,
\* the cell holding no record (Writer::take).
OpenGone(h, b) ==
    /\ "staleTake" \in Can /\ takes < MaxTakes
    /\ b \in gone /\ rec[b] = None /\ head[b] = None
    /\ IF ses[h][b] = None THEN TRUE ELSE ses[h][b].pc = "stopped"
    /\ head' = [head EXCEPT ![b] = [epoch |-> 1, token |-> tok[h][b], holder |-> gen + 1, end |-> 0,
                                    chain |-> <<>>, ckpt |-> <<>>, vers |-> {}]]
    /\ taken' = [taken EXCEPT ![b] = IF tok[h][b] > @ THEN tok[h][b] ELSE @]
    /\ gen' = gen + 1
    /\ takes' = takes + 1
    /\ UNCHANGED <<seg, own, ckpt, rec, moves, mflight, inflight, refs, marked, retired, written, gone,
                   hostVars, gc>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, doomed, placed, errors>>
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

\* h's writer of b, whose session s recovered the branch to its head's end, opened: the head read
\* again (epochSequences), and the chain, minting from the epoch's range. Then its WAL put after
\* the branch's end (wal::rebase), by commits: the WAL's segments to the one ending the branch's
\* last commit, if it is one of the WAL's, acknowledged; the rest restamped after the head's end,
\* if the branch's last commit is one of the WAL's or the commit the WAL started from
\* (walFollows); otherwise the WAL diverged, and is dropped. By commits, not by the keys the head
\* names: a store keeps what it restamps in memory alone, its footers on disk naming the keys of
\* an earlier stamping after a crash. The commit the WAL started
\* from, its first segment's commit's parent, the WAL keeps. The store keeps the restamped
\* footers in memory alone (TASK-165.9): a crash after finds them stamped as before, which the
\* next opening restamps by the same commits, its own chain, and Max2 with a later floor, to what
\* this would have: so the WAL here holds them restamped. The WAL's segments replayed over the
\* state must end every commit they go on with, or the store refuses to open (store.rs: "log
\* ends inside a commit"), its local recovery having kept a commit begun.
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
                         [s EXCEPT !.pc = IF Unrecorded(b, s.st, s) THEN "record" ELSE "ready",
                                   !.durable = {new[i].s : i \in DOMAIN new},
                                   !.seq = s.seq + Len(new),
                                   !.next = IF new = <<>> THEN next ELSE new[Len(new)].o.next]]
            \* Diverged, the WAL dropped, though no other writer's commit moved the branch on: its
            \* last is one of the WAL's, or the WAL's base.
            /\ errors' = IF Rs # <<>> /\ ~keep /\ s.st.tip \in CommitsOf(ws) \cup {base}
                         THEN errors \cup {"a WAL diverged from a branch holding its commits"}
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
       /\ UNCHANGED <<commits, nextTag, minted, twice, makers, doomed, taken, placed>>

\* The children of the forks acknowledged get their records (record_forks), one at a time; those
\* in other cells first their references (referencesFirst), or, without the rule, after. With
\* the rule, the child's record read, then the reference written, as the code's requests are
\* (record_fork): a recorder whose read is stale writes after another's retirement or take-back.
RecordForks(h, b) ==
    LET s == ses[h][b]
        F == {f \in s.st.forks : f.child \notin s.recorded
                                 \/ (NeedsRef(b, f) /\ f.child \notin s.reffed)}
        f == FirstFork(F)
        r == RecordFork(rec, b, f)
        refer == /\ NeedsRef(b, f) /\ f.child \notin s.reffed
                 /\ R("referencesFirst") \/ f.child \in s.recorded
        back == RefsTakenBack(b, {f})
    IN /\ s # None /\ s.pc = "record"
       /\ IF F = {} THEN
             /\ ses' = [ses EXCEPT ![h][b].pc = "ready"]
             /\ UNCHANGED <<rec, makers, refs, marked, retired>>
          ELSE IF refer /\ ~R("referencesFirst") THEN
             \* Without the rule, once recorded: its reference written, as its record reads.
             /\ refs' = (refs \ back) \cup RefsWritten(b, {f})
             /\ retired' = retired \cup back
             /\ marked' = marked \cup Marks(back)
             /\ ses' = [ses EXCEPT ![h][b].reffed = @ \cup {f.child}]
             /\ UNCHANGED <<rec, makers>>
          ELSE IF refer /\ f.child \in s.reading THEN
             \* Its reference written, its record read none.
             /\ refs' = refs \cup RefsOf(b, {f})
             /\ ses' = [ses EXCEPT ![h][b].reffed = @ \cup {f.child},
                                    ![h][b].reading = @ \ {f.child}]
             /\ UNCHANGED <<rec, makers, marked, retired>>
          ELSE IF refer THEN
             \* The child's record read: none, its reference to be written; another's, or
             \* deleted, taken back; its own, live, its reference its maker's (ToRefer).
             /\ ses' = IF ToRefer(b, {f}) # {}
                       THEN [ses EXCEPT ![h][b].reading = @ \cup {f.child}]
                       ELSE [ses EXCEPT ![h][b].reffed = @ \cup {f.child}]
             /\ refs' = refs \ back
             /\ retired' = retired \cup back
             /\ marked' = marked \cup Marks(back)
             /\ UNCHANGED <<rec, makers>>
          ELSE
             \* The record, unless one is there: another's, the reference taken back.
             /\ rec' = [rec EXCEPT ![f.child] = r.rec]
             /\ makers' = IF r.made THEN [makers EXCEPT ![f.child] = @ \cup {f.owner}]
                          ELSE makers
             /\ ses' = [ses EXCEPT ![h][b].recorded = @ \cup {f.child}]
             /\ refs' = refs \ back
             /\ retired' = retired \cup back
             /\ marked' = marked \cup Marks(back)
       /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, written, gone, wal, tok, proc, gc>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, doomed, taken, placed, errors>>
       /\ UNCHANGED countVars

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
       /\ UNCHANGED <<commits, nextTag, acked, makers, doomed, taken, placed, errors>>
       /\ UNCHANGED <<gen, takes, passes, places, crashes>>

\* What a commit does, besides committing: cut a version, the store quiescent, seal an open
\* extent into its segment or on its own,
\* free an extent its guest holds, fork the branch (a point, or not) to a child with no record,
\* with "forkOwn" sealing an open extent on its own too, in the fork's segment (a store's segment
\* holds the commits before the fork it ends, their extents stored on their own among them:
\* TASK-380), or seal one or two open extents where they were written, in segments filled since the last
\* commit that go on with it, one each, the commit ending in the next (a commit spanning
\* segments: TASK-92.41, segments::Phase::Full). With "spanOn", the first of those segments may
\* be the WAL's last, ending the commit before, not uploaded: it filled after that commit, before
\* this one (Segments::record: "the first alone may hold commits, those made before it filled").
Ops(h, b) ==
    LET s == ses[h][b]
        v == View(h, b)
        op(k, x, c, p) == [k |-> k, x |-> x, c |-> c, point |-> p, xs |-> <<>>, on |-> FALSE]
        span(xs, on) == [k |-> "span", x |-> None, c |-> 0, point |-> FALSE, xs |-> xs, on |-> on]
        ws == wal[h][b]
        ons == {FALSE} \cup (IF /\ "spanOn" \in Can /\ ws # <<>>
                                 /\ ws[Len(ws)].s \notin s.up /\ ws[Len(ws)].o.cont = None
                              THEN {TRUE} ELSE {})
        kinds == (IF "fork" \in Can THEN {FALSE} ELSE {})
                 \cup (IF "point" \in Can THEN {TRUE} ELSE {})
        open == IF "span" \in Can THEN s.open ELSE {}
        \* The IDs a fork's child may take: any with no record; with "forkNext", the least, as
        \* an allocator gives them out, for models whose choice of ID would only multiply states.
        free == {c \in Branches \ {b, Root} : Nameable(c)}
    IN (IF "commit" \in Can THEN {op("plain", None, 0, FALSE)} ELSE {})
       \* A version's cut (TASK-476): of a quiescent store, sealing nothing.
       \cup (IF "version" \in Can /\ s.open = {} THEN {op("version", None, 0, FALSE)} ELSE {})
       \cup {op("seg", x, 0, FALSE) : x \in IF "seg" \in Can THEN s.open ELSE {}}
       \cup {op("own", x, 0, FALSE) : x \in IF "own" \in Can THEN s.open ELSE {}}
       \cup {op("free", x, 0, FALSE) : x \in IF "free" \in Can THEN DOMAIN v.ext ELSE {}}
       \cup {op("fork", x, c, p) : x \in {None} \cup (IF "forkOwn" \in Can THEN s.open ELSE {}),
                                   c \in IF "forkNext" \in Can /\ free # {} THEN {Min(free)}
                                         ELSE free,
                                   p \in kinds}
       \cup {span(<<x>>, on) : x \in open, on \in ons}
       \cup {span(<<x, y>>, on) : <<x, y>> \in {p \in open \X open : p[1] # p[2]}, on \in ons}

\* A commit: segments of the WAL, stamped by the chain: those going on with it, if it spans
\* segments (Contents::continued, each naming the commit and its run there), the first perhaps
\* the WAL's last, then the one it ends in.
Commit(h, b) ==
    LET s == ses[h][b]
        t == nextTag
        ws == wal[h][b]
    IN /\ Ready(h, b) /\ t <= MaxCommits
       /\ \E op \in Ops(h, b) :
            LET on == IF op.on THEN 1 ELSE 0
                n == Len(op.xs) - on
                f == [child |-> op.c, owner |-> <<"fork", t>>, point |-> op.point]
                cont(i) == [commit |-> t, ext |-> op.xs[i]]
                part(i) == [s |-> s.seq + i - 1,
                            o |-> [epoch |-> s.epoch, commit |-> 0, ext |-> None, own |-> None,
                                   free |-> None, next |-> s.next, fork |-> None, ver |-> None,
                                   cont |-> cont(i + on)]]
                before == IF op.on THEN [ws EXCEPT ![Len(ws)].o.cont = cont(1)] ELSE ws
                o == [epoch |-> s.epoch,
                      commit |-> t,
                      ext |-> IF op.k = "seg" THEN op.x ELSE None,
                      own |-> IF op.k \in {"own", "fork"} THEN op.x ELSE None,
                      free |-> IF op.k = "free" THEN op.x ELSE None,
                      next |-> s.next,
                      fork |-> IF op.k = "fork" THEN f ELSE None,
                      ver |-> IF op.k = "version" THEN t ELSE None,
                      cont |-> None]
                xs == {op.xs[i] : i \in DOMAIN op.xs}
                \* Without versionsAcked, the version named as its cut is committed: by the next
                \* swap of the head, whatever it acknowledges.
                early == IF op.k = "version" /\ ~R("versionsAcked")
                         THEN {[id |-> t, at |-> s.seq + n]} ELSE {}
            IN /\ s.seq + n < Segs
               /\ wal' = [wal EXCEPT ![h][b] = before \o [i \in 1..n |-> part(i)]
                                                 \o <<[s |-> s.seq + n, o |-> o]>>]
               /\ ses' = [ses EXCEPT ![h][b] =
                            [s EXCEPT !.seq = @ + n + 1, !.open = @ \ ({op.x} \cup xs),
                                      !.vers = @ \cup early]]
               /\ commits' = [commits EXCEPT ![t] = [branch |-> b, parent |-> View(h, b).tip,
                                                      seals |-> IF op.k \in {"seg", "own"}
                                                                   \/ op.x # None
                                                                THEN {op.x} ELSE xs,
                                                      free |-> o.free,
                                                      ver |-> op.k = "version",
                                                      dropped |-> FALSE]]
               \* Without recordsAfterAck, the child's record at once, and its reference.
               /\ rec' = IF op.k = "fork" /\ ~R("recordsAfterAck")
                         THEN [rec EXCEPT ![op.c] = Record(IF op.point THEN "point" ELSE "fork",
                                                           <<b, s.seq>>, 0, f.owner, t)]
                         ELSE rec
               /\ refs' = IF op.k = "fork" /\ ~R("recordsAfterAck")
                          THEN refs \cup RefsOf(b, {[child |-> op.c, at |-> s.seq]})
                          ELSE refs
       /\ nextTag' = t + 1
       /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, marked, retired, written, gone, tok, proc,
                      gc>>
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
       /\ UNCHANGED <<seg, ckpt, rec, head, moves, mflight, refs, marked, retired, written, gone, wal, tok, proc,
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
       /\ UNCHANGED <<own, ckpt, rec, head, moves, mflight, refs, marked, retired, gone, wal, tok, proc, gc,
                      truthVars, countVars>>

\* The acknowledgement of the WAL's prefix landed: the head swapped to name it, with the
\* condition that it is the writer's taking still (headSwap); failing, the writer stops, taken
\* over. Without the rule, written whatever is there. Then its commits told durable, its forks'
\* children to be recorded. To the last segment going on with no commit (acksBetweenCommits,
\* Writer::acknowledge): a commit spanning segments is acknowledged with the one it ends in, so
\* that the state acknowledged, which heads and checkpoints name, holds no commit begun, and the
\* WAL keeps it whole.
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
        s2 == [s EXCEPT !.st = ApplyAll(s.st, b, P),
                        !.end = P[n].s + 1,
                        !.chain = @ \o [i \in 1..n |-> P[i].o.epoch],
                        !.vers = @ \cup V]
    IN /\ Ready(h, b) /\ n > 0
       /\ IF R("headSwap") /\ ~Taking(s, b) THEN
             Stopped(h, b) /\ UNCHANGED <<head, wal, acked>>
          ELSE /\ head' = [head EXCEPT ![b] = HeadOf(s2)]
               /\ wal' = [wal EXCEPT ![h][b] = SubSeq(ws, n + 1, Len(ws))]
               /\ ses' = [ses EXCEPT ![h][b] =
                            [s2 EXCEPT !.pc = IF Unrecorded(b, s2.st, s) THEN "record"
                                              ELSE "ready"]]
               /\ acked' = [acked EXCEPT ![b] = @ \cup CommitsOf(P)]
       /\ UNCHANGED <<seg, own, ckpt, rec, moves, mflight, inflight, refs, marked, retired, written, gone, tok,
                      proc, gc, countVars>>
       /\ UNCHANGED <<commits, nextTag, minted, twice, makers, doomed, taken, placed, errors>>

\* The writer drops a version (Writer::drop_versions): the head swapped to name it no more, with
\* the condition that it is the writer's taking still (headSwap); failing, the writer stops. A
\* window's oldest dropped as an acknowledgement names a new version is this, in that swap.
DropVersion(h, b) ==
    LET s == ses[h][b] IN
    /\ "dropVersion" \in Can /\ Ready(h, b)
    /\ \E v \in s.vers :
         IF R("headSwap") /\ ~Taking(s, b) THEN Stopped(h, b) /\ UNCHANGED <<head, commits>>
         ELSE LET s2 == [s EXCEPT !.vers = @ \ {v}] IN
              /\ head' = [head EXCEPT ![b] = HeadOf(s2)]
              /\ ses' = [ses EXCEPT ![h][b] = s2]
              /\ commits' = [commits EXCEPT ![v.id].dropped = TRUE]
    /\ UNCHANGED <<seg, own, ckpt, rec, moves, mflight, inflight, refs, marked, retired, written, gone,
                   wal, tok, proc, gc, countVars>>
    /\ UNCHANGED <<nextTag, acked, minted, twice, makers, doomed, taken, placed, errors>>

\* A checkpoint as of the last segment acknowledged: the extents the collector moved where they
\* went, the forks whose children were deleted dropped; its PUT then made. The writer names
\* those extents where they went now, before it writes it (movesTakenFirst), only forward along
\* their ways; without the rule, once it is in the head.
Checkpoint(h, b) ==
    LET s == ses[h][b]
        q == s.st.through
        st == [s.st EXCEPT !.ext = [x \in DOMAIN s.st.ext |-> Moved(x, s.st.ext[x])],
                           !.forks = {f \in s.st.forks :
                                         ~(rec[f.child] # None /\ rec[f.child].deleted)}]
        kept == [s.st EXCEPT !.forks = st.forks]
    IN /\ "checkpoint" \in Can /\ Ready(h, b) /\ q >= 0 /\ q > s.ckptThrough /\ s.ckptPut = None
       /\ ses' = [ses EXCEPT ![h][b].st = IF R("movesTakenFirst") THEN TakeMoves(kept, st)
                                         ELSE kept,
                             ![h][b].ckptThrough = q,
                             ![h][b].ckptPut = [q |-> q, st |-> st, landed |-> FALSE,
                                            swapped |-> FALSE]]
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
       /\ UNCHANGED <<seg, own, rec, head, moves, mflight, inflight, refs, marked, retired, gone, wal, tok, proc,
                      gc, truthVars, countVars>>

\* The checkpoint landed swapped into the head, with the condition that it is the writer's taking
\* still (headSwap); failing, the writer stops.
CheckpointSwap(h, b) ==
    LET s == ses[h][b]
        s2 == [s EXCEPT !.hckpt = <<s.ckptPut.q, s.epoch>>,
                        !.ckptPut = IF R("movesTakenFirst") THEN None
                                    ELSE [@ EXCEPT !.swapped = TRUE]]
    IN /\ s # None /\ s.pc \in {"ready", "record"} /\ s.ckptPut # None /\ s.ckptPut.landed
       /\ ~s.ckptPut.swapped
       /\ IF R("headSwap") /\ ~Taking(s, b) THEN Stopped(h, b) /\ UNCHANGED head
          ELSE /\ head' = [head EXCEPT ![b] = HeadOf(s2)]
               /\ ses' = [ses EXCEPT ![h][b] = s2]
       /\ UNCHANGED <<seg, own, ckpt, rec, moves, mflight, inflight, refs, marked, retired, written, gone, wal,
                      tok, proc, gc, truthVars, countVars>>

\* The swap answered, the writer names what the checkpoint moved where it went, only forward
\* along their ways (without movesTakenFirst).
CheckpointTaken(h, b) ==
    LET s == ses[h][b] IN
    /\ s # None /\ s.ckptPut # None /\ s.ckptPut.swapped
    /\ ses' = [ses EXCEPT ![h][b].st = TakeMoves(s.st, s.ckptPut.st), ![h][b].ckptPut = None]
    /\ UNCHANGED <<bucketVars, wal, tok, proc, gc, truthVars, countVars>>

WriterNext ==
    \E h \in Hosts, b \in Branches :
        \/ Open(h, b) \/ OpenGone(h, b) \/ RecoverRead(h, b) \/ RecordForks(h, b) \/ Mint(h, b) \/ Commit(h, b)
        \/ Upload(h, b) \/ Ack(h, b) \/ Checkpoint(h, b) \/ CheckpointLand(h, b)
        \/ CheckpointSwap(h, b) \/ CheckpointTaken(h, b) \/ DropVersion(h, b)

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
          cut \in [Branches -> 0..Segs] :
          /\ \A b \in Branches : cut[b] \in Cuts(h, b)
          /\ wal' = [wal EXCEPT ![h] = [b \in Branches |-> IF lose THEN <<>>
                                                           ELSE SubSeq(@[b], 1, cut[b])]]
    /\ ses' = [ses EXCEPT ![h] = [b \in Branches |-> None]]
    /\ proc' = [proc EXCEPT ![h] = None]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<bucketVars, tok, gc, truthVars, gen, takes, passes, places, mints>>

\* b placed on h: a fencing token above every earlier placement's.
Place(h, b) ==
    /\ "place" \in Can /\ places < MaxPlaces /\ rec[b] # None
    /\ tok' = [tok EXCEPT ![h][b] = placed[b] + 1]
    /\ placed' = [placed EXCEPT ![b] = @ + 1]
    /\ places' = places + 1
    /\ UNCHANGED <<bucketVars, ses, wal, proc, gc, gen, takes, passes, crashes, mints>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, doomed, taken, errors>>

\* Another branch takes the ID c, as a guest may give any (TASK-79.82).
Contest(c) ==
    /\ "contest" \in Can /\ c # Root /\ Nameable(c)
    /\ rec' = [rec EXCEPT ![c] = Record("root", <<>>, 0, <<"root", c>>, 0)]
    /\ makers' = [makers EXCEPT ![c] = @ \cup {<<"root", c>>}]
    /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, refs, marked, retired, written, gone, hostVars,
                   gc, countVars>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, doomed, taken, placed, errors>>

-----------------------------------------------------------------------------
(* Deletions, failed forks' deletions, and forks of points: a host's work, one at a time. *)

\* A deletion of b, asked on h: the head ended first. Asked by b's last placement, or one as
\* late, it may delete b's making. With "deleteRoot" alone, of the root alone.
StartDelete(h, b) ==
    /\ proc[h] = None
    /\ rec[b] # None /\ ~rec[b].deleted
    /\ \/ "delete" \in Can
       \/ "deletePoints" \in Can /\ rec[b].kind = "point"
       \/ "deleteRoot" \in Can /\ b = Root
    /\ proc' = [proc EXCEPT ![h] = [op |-> "delete", b |-> b, pc |-> "end", c |-> 0, p |-> 0,
                                    at |-> 0, ep |-> 0, o |-> <<>>, live |-> FALSE,
                                    tk |-> tok[h][b], fs |-> {}, ws |-> {}, og |-> <<>>, bs |-> 0, v |-> <<>>]]
    /\ doomed' = IF tok[h][b] >= taken[b] THEN doomed \cup {rec[b].maker} ELSE doomed
    /\ UNCHANGED <<bucketVars, ses, wal, tok, gc, countVars>>
    /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, taken, placed, errors>>

\* With "deleteEarly", a deletion of b, a fork's child whose record is yet to come: asked by its
\* placement once the fork was acknowledged, before its parent's writer recorded it, slow (the
\* TASK-79.78 race). branch::delete ends a branch's head first, made final where it has none, then
\* writes its record deleted, unless the fork's came first, which it then recovers and marks
\* deleted. It may delete the fork's making.
StartDeleteEarly(h, b) ==
    LET F == {f \in UNION {ses[w[1]][w[2]].st.forks :
                              w \in {w \in Hosts \X Branches : ses[w[1]][w[2]] # None}} :
                f.child = b}
    IN /\ "deleteEarly" \in Can /\ proc[h] = None /\ rec[b] = None /\ F # {}
       /\ proc' = [proc EXCEPT ![h] = [op |-> "delete", b |-> b, pc |-> "end", c |-> 0, p |-> 0,
                                       at |-> 0, ep |-> 0, o |-> <<>>, live |-> FALSE,
                                       tk |-> tok[h][b], fs |-> {}, ws |-> {}, og |-> <<>>,
                                       bs |-> 0, v |-> <<>>]]
       /\ doomed' = doomed \cup {f.owner : f \in F}
       /\ UNCHANGED <<bucketVars, ses, wal, tok, gc, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, taken, placed, errors>>

\* The deletion of a fork that failed, its writer stopped before acknowledging it
\* (delete_fork): of the child, if its record is the fork's or it has none.
StartUnfork(h, b) ==
    LET s == ses[h][b]
        F == {i \in DOMAIN wal[h][b] : wal[h][b][i].o.fork # None}
    IN /\ "unfork" \in Can /\ proc[h] = None /\ s # None /\ s.pc = "stopped"
       /\ \E i \in F :
            LET w == wal[h][b][i] IN
            /\ proc' = [proc EXCEPT ![h] = [op |-> "unfork", b |-> w.o.fork.child, pc |-> "read",
                                            c |-> w.o.fork.child, p |-> b, at |-> w.s,
                                            ep |-> w.o.epoch, o |-> w.o.fork.owner,
                                            live |-> FALSE, tk |-> tok[h][w.o.fork.child],
                                            fs |-> {}, ws |-> {}, og |-> <<>>, bs |-> 0, v |-> <<>>]]
            /\ doomed' = doomed \cup {w.o.fork.owner}
       /\ UNCHANGED <<bucketVars, ses, wal, tok, gc, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, taken, placed, errors>>

\* The reference of the fork of a point, the restore q, to its child: under the cell of the
\* origin the point names, whatever the child's cell. One in that cell too: the cell's pass reads
\* its records before the states and its references after, and the point's own reference, in
\* another cell, may be retired between, its point deleted, while the child's record came after
\* the records were read (BucketOverlap found it).
PointForkRefs(q) == IF R("references") THEN {RefOf(q.og[1], q.og[2], q.c)} ELSE {}

\* The record of the child of the restore q, being made (forksPending), of its point or version.
Forking(q) == [Record(IF R("forksPending") THEN "forking" ELSE "fork", q.og, q.p, q.o, q.bs)
                   EXCEPT !.ver = q.v]

\* Whether what the restore q forks is gone, as its check reads it: the point's record deleted,
\* or none; the version named by its branch's head no more (TASK-476).
Unforkable(q) == IF q.v # <<>> THEN VersionGone(q.v) ELSE rec[q.p] = None \/ rec[q.p].deleted

\* The restore q, to C, begun on h: first C's record, saying it is being made (forksPending), then
\* its reference (forksRecordFirst), before it reads what it forks (forksReferFirst); without
\* forksRecordFirst, the reference first.
Restoring(h, q) ==
    LET refers == R("forksReferFirst") /\ PointForkRefs(q) # {}
        first == refers /\ ~R("forksRecordFirst")
    IN /\ IF first THEN
             /\ refs' = refs \cup PointForkRefs(q)
             /\ proc' = [proc EXCEPT ![h] = [q EXCEPT !.pc = "fork"]]
             /\ UNCHANGED rec
          ELSE
             /\ rec' = [rec EXCEPT ![q.c] = Forking(q)]
             /\ proc' = [proc EXCEPT ![h] = [q EXCEPT !.pc = IF refers THEN "refer" ELSE "check"]]
             /\ UNCHANGED refs
       /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, marked, retired, written, gone, ses, wal,
                      tok, gc, truthVars, countVars>>

\* A fork of point P to C (fork_point), of the origin P names, from a host's copy of P, stale
\* perhaps. C is an ID an allocator gives (Given), or one a guest names; never one given before,
\* its cell removed (gone).
StartRestore(h, c, p) ==
    /\ "restore" \in Can /\ proc[h] = None
    /\ rec[p] # None /\ rec[p].kind = "point" /\ rec[c] = None /\ c \notin {Root, p} \cup gone
    /\ Restoring(h, [op |-> "restore", b |-> c, pc |-> "check", c |-> c, p |-> p, at |-> 0,
                     ep |-> 0, o |-> <<"restore", c, h>>, live |-> FALSE, tk |-> tok[h][c],
                     fs |-> {}, ws |-> {}, og |-> rec[p].origin, bs |-> rec[p].base, v |-> <<>>])

\* A restore of a version of b to C (TASK-476): of b at the version's segment, its base the
\* version's cut, a version b's head names as the restore begins (one named and dropped since is
\* a restore begun before the drop). Its record names the version, not a point.
StartRestoreVersion(h, c, b) ==
    /\ "restoreVersion" \in Can /\ proc[h] = None
    /\ rec[c] = None /\ c \notin {Root, b} \cup gone
    /\ \E v \in HeadVers(b) :
         Restoring(h, [op |-> "restore", b |-> c, pc |-> "check", c |-> c, p |-> 0, at |-> 0,
                       ep |-> 0, o |-> <<"restore", c, h>>, live |-> FALSE, tk |-> tok[h][c],
                       fs |-> {}, ws |-> {}, og |-> <<b, v.at>>, bs |-> v.id,
                       v |-> <<b, v.id>>])

\* With "resume", the fork of a point to c, being made, resumed by another than the restore that
\* began it: a store opening c, its restore's host dead or slow (upload.rs's record). Its record
\* said resumed first, with a condition, unless made or deleted (resumesMarked); then, as a
\* restore goes on, its reference (resumesRefer), the point's record read, and the fork made.
\* Without resumesRefer, no reference: the fork made as the point is read.
ResumeRestore(h, c) ==
    LET r == rec[c]
        q == [op |-> "restore", b |-> c, pc |-> "check", c |-> c, p |-> r.point, at |-> 0,
              ep |-> 0, o |-> r.maker, live |-> FALSE, tk |-> tok[h][c], fs |-> {}, ws |-> {},
              og |-> r.origin, bs |-> r.base, v |-> r.ver]
        refers == R("resumesRefer") /\ R("forksReferFirst") /\ PointForkRefs(q) # {}
    IN /\ "resume" \in Can /\ proc[h] = None
       /\ r # None /\ r.kind = "forking" /\ ~r.deleted
       /\ rec' = [rec EXCEPT ![c].resumed = TRUE]
       /\ proc' = [proc EXCEPT ![h] = [q EXCEPT !.pc = IF refers THEN "refer" ELSE "check"]]
       /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, refs, marked, retired, written,
                      gone, ses, wal, tok, gc, truthVars, countVars>>

Done(h) == proc' = [proc EXCEPT ![h] = None]
\* A deletion of the branch proc[h] names, after a failed fork's or restore's.
Deleting(h) == proc' = [proc EXCEPT ![h] = [@ EXCEPT !.op = "delete", !.pc = "end"]]

\* b's head swapped to the final epoch, what it names kept, but its versions, which go with it
\* (doc-18): made so, if b has none.
Ended(b) ==
    head' = [head EXCEPT ![b] = IF @ = None
                                THEN [epoch |-> Final, token |-> 0, holder |-> 0, end |-> 0,
                                      chain |-> <<>>, ckpt |-> <<>>, vers |-> {}]
                                ELSE [@ EXCEPT !.epoch = Final, !.token = 0, !.holder = 0,
                                               !.vers = {}]]

ProcStep(h) ==
    LET q == proc[h]
        b == q.b
    IN /\ q # None
       /\ CASE q.op = "delete" /\ q.pc = "end" ->
                 \* end_lease, its head swapped to the final epoch, unless a later placement took
                 \* the branch (deleteTokens). No writer swaps it after: the head is the fence.
                 /\ IF head[b] # None /\ head[b].epoch # Final /\ R("deleteTokens")
                       /\ head[b].token > q.tk
                    THEN Done(h) /\ UNCHANGED head
                    ELSE Ended(b) /\ proc' = [proc EXCEPT ![h].pc = "mark"]
                 /\ UNCHANGED <<seg, own, ckpt, rec, moves, mflight, inflight, makers, errors>>
            [] q.op = "delete" /\ q.pc = "mark" ->
                 \* Its record, deleted: made so if it has none; else, unless it is deleted or
                 \* being made, to recover it.
                 LET r == rec[b] IN
                 /\ IF r = None THEN
                       /\ rec' = [rec EXCEPT ![b] =
                                    [Record("fork", <<>>, 0, <<"deletion", h>>, 0)
                                         EXCEPT !.deleted = TRUE]]
                       /\ Done(h)
                    ELSE IF r.deleted THEN UNCHANGED rec /\ Done(h)
                    ELSE IF r.kind = "forking" THEN
                       /\ rec' = [rec EXCEPT ![b].deleted = TRUE]
                       /\ Done(h)
                    ELSE /\ proc' = [proc EXCEPT ![h].pc = "recover"]
                         /\ UNCHANGED rec
                 /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "delete" /\ q.pc = "recover" /\ rec[b] = None ->
                 \* Its record removed since, with its cell, as a given root's or point's fork's is
                 \* (recordsKept): done, as by another deletion.
                 /\ Done(h)
                 /\ UNCHANGED <<seg, own, ckpt, rec, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "delete" /\ q.pc = "recover" /\ rec[b] # None ->
                 \* Its recovery, from the head it ended, which no writer swaps since: every
                 \* deletion of the branch finds the same forks. Then its forks are recorded,
                 \* those in other cells their references first (referencesFirst). Finding what it
                 \* needs lost: done if the record was deleted since (deletionsDoneByOthers), or a
                 \* claim no writer has.
                 LET got == Recover(b, rec[b].origin) IN
                 /\ IF got.ok THEN
                       /\ proc' = [proc EXCEPT ![h].pc = IF /\ R("referencesFirst")
                                                           /\ RefsOf(b, got.st.forks) # {}
                                                        THEN "refs" ELSE "record",
                                               ![h].fs = got.st.forks]
                       /\ UNCHANGED <<rec, makers, errors>>
                    ELSE IF R("deletionsDoneByOthers") /\ rec[b].deleted THEN
                       Done(h) /\ UNCHANGED <<rec, makers, errors>>
                    ELSE IF rec[b].kind = "claim" /\ Unwritten(b) THEN
                       /\ rec' = [rec EXCEPT ![b].deleted = TRUE]
                       /\ Done(h)
                       /\ UNCHANGED <<makers, errors>>
                    ELSE /\ errors' = errors \cup {"a deletion lost what it needs"}
                         /\ Done(h)
                         /\ UNCHANGED <<rec, makers>>
                 /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight>>
            [] q.op = "delete" /\ q.pc = "refs" ->
                 \* The references of the forks its recovery found to other cells' children,
                 \* before their records (referencesFirst): the children's records read, those
                 \* of other origins or none taken back, and those with none to be written, in a
                 \* step after (record_fork). Without the rule, after its own record is marked
                 \* deleted, in the one step.
                 /\ IF ~R("referencesFirst") THEN Done(h)
                    ELSE IF ToRefer(b, q.fs) # {} THEN
                       proc' = [proc EXCEPT ![h].pc = "write", ![h].ws = ToRefer(b, q.fs)]
                    ELSE proc' = [proc EXCEPT ![h].pc = "record"]
                 /\ UNCHANGED <<seg, own, ckpt, rec, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "delete" /\ q.pc = "write" ->
                 \* Those references written, as their records read none.
                 /\ proc' = [proc EXCEPT ![h].pc = "record", ![h].ws = {}]
                 /\ UNCHANGED <<seg, own, ckpt, rec, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "delete" /\ q.pc = "record" ->
                 \* The children of the forks its recovery found recorded, and its record
                 \* deleted, unless the collector dropped it (without recordsKept).
                 LET all == RecordAll(rec, makers, b, q.fs) IN
                 /\ rec' = IF all.rec[b] = None THEN all.rec
                           ELSE [all.rec EXCEPT ![b].deleted = TRUE]
                 /\ makers' = all.makers
                 /\ IF ~R("referencesFirst") /\ RefsOf(b, q.fs) # {}
                    THEN proc' = [proc EXCEPT ![h].pc = "refs"]
                    ELSE Done(h)
                 /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, errors>>
            [] q.op = "unfork" /\ q.pc = "read" ->
                 /\ IF rec[b] # None THEN
                       IF ~rec[b].deleted /\ ChildOf(rec[b], q.o) THEN Deleting(h) ELSE Done(h)
                    ELSE IF ~R("forkClaimsIds") THEN Deleting(h)
                    ELSE proc' = [proc EXCEPT ![h].pc = "claim"]
                 /\ UNCHANGED <<seg, own, ckpt, rec, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "unfork" /\ q.pc = "claim" ->
                 \* The child's ID taken with a record: live, a claim, only if its parent's head
                 \* names the fork's segment (forkClaimsLanded): its writer's WAL, on that host,
                 \* may yet commit it.
                 LET live == ~R("forkClaimsLanded") \/ Holds(q.p, q.at, q.ep)
                     kind == IF R("forkClaimsLanded") /\ live THEN "claim" ELSE "fork"
                 IN IF rec[b] = None THEN
                       /\ rec' = [rec EXCEPT ![b] = [Record(kind, <<q.p, q.at>>, 0, q.o, q.o[2])
                                                        EXCEPT !.deleted = ~live]]
                       /\ proc' = [proc EXCEPT ![h].pc = "end", ![h].live = live,
                                               ![h].op = "unforkEnd"]
                       /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, makers, errors>>
                    ELSE /\ IF ~rec[b].deleted /\ ChildOf(rec[b], q.o) THEN Deleting(h)
                            ELSE Done(h)
                         /\ UNCHANGED <<seg, own, ckpt, rec, head, moves, mflight, inflight, makers,
                                        errors>>
            [] q.op = "unforkEnd" /\ q.pc = "end" ->
                 /\ IF head[b] # None /\ head[b].epoch # Final /\ R("deleteTokens")
                       /\ head[b].token > q.tk
                    THEN Done(h) /\ UNCHANGED head
                    ELSE Ended(b) /\ proc' = [proc EXCEPT ![h].pc = "mark"]
                 /\ UNCHANGED <<seg, own, ckpt, rec, moves, mflight, inflight, makers, errors>>
            [] q.op = "unforkEnd" /\ q.pc = "mark" ->
                 /\ rec' = IF q.live /\ ~rec[b].deleted THEN [rec EXCEPT ![b].deleted = TRUE]
                           ELSE rec
                 /\ Done(h)
                 /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "restore" /\ q.pc = "refer" ->
                 \* Its reference, its record made (forksRecordFirst), before the point's read.
                 /\ proc' = [proc EXCEPT ![h].pc = "check"]
                 /\ UNCHANGED <<seg, own, ckpt, rec, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "restore" /\ q.pc = "fork" ->
                 \* Without forksRecordFirst: the child's record, its reference made, unless its ID
                 \* was taken since: the reference then taken back (referencesTakenBack), or left
                 \* for good.
                 /\ IF rec[b] = None THEN /\ rec' = [rec EXCEPT ![b] = Forking(q)]
                                          /\ proc' = [proc EXCEPT ![h].pc = "check"]
                    ELSE Done(h) /\ UNCHANGED rec
                 /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "restore" /\ q.pc = "check" ->
                 \* The point's record, or the version's branch's head, read after the child's
                 \* record (forksCheckPoints): the point deleted, or the version named no more,
                 \* the fork fails, and its host deletes the child.
                 /\ IF R("forksCheckPoints") /\ Unforkable(q)
                    THEN Deleting(h)
                    ELSE proc' = [proc EXCEPT ![h].pc = "make"]
                 /\ UNCHANGED <<seg, own, ckpt, rec, head, moves, mflight, inflight, makers, errors>>
            [] q.op = "restore" /\ q.pc = "make" ->
                 \* No longer being made, with a condition: unless deleted, given up on. Without
                 \* forksReferFirst, its reference made now, after the point's read.
                 /\ IF rec[b] # None /\ ~rec[b].deleted THEN
                       /\ rec' = [rec EXCEPT ![b].kind = IF @ = "forking" THEN "fork" ELSE @]
                       /\ makers' = [makers EXCEPT ![b] = @ \cup {q.o}]
                    ELSE UNCHANGED <<rec, makers>>
                 /\ Done(h)
                 /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, errors>>
       /\ doomed' = IF q.op = "restore" /\ q.pc = "check" /\ R("forksCheckPoints")
                       /\ Unforkable(q)
                    THEN doomed \cup {q.o} ELSE doomed
       \* A deletion's references of the forks it found, as their records read (record_fork):
       \* taken back, those records another's (referencesTakenBack), or written, in a step after,
       \* those with none; and taken back as it records the children; a point's fork's. Taken back,
       \* marked (retirementsMarked).
       /\ LET back == CASE q.op = "delete" /\ q.pc = "refs" -> RefsTakenBack(b, q.fs)
                         [] q.op = "delete" /\ q.pc = "record" ->
                             RefsTakenBackIn(RecordAll(rec, makers, b, q.fs).rec, b, q.fs)
                         [] q.op = "restore" /\ q.pc = "fork" /\ rec[b] # None ->
                             TakenBack(q.og[1], q.og[2], q.c, rec[b])
                         [] OTHER -> {}
          IN /\ refs' = (refs \ back)
                       \cup CASE q.op = "delete" /\ q.pc = "refs" /\ ~R("referencesFirst") ->
                                  RefsWritten(b, q.fs)
                               [] q.op = "delete" /\ q.pc = "write" -> RefsOf(b, q.ws)
                               [] q.op = "restore" /\ q.pc = "refer" -> PointForkRefs(q)
                               [] (/\ q.op = "restore" /\ q.pc = "make"
                                   /\ ~R("forksReferFirst") /\ rec[b] # None
                                   /\ ~rec[b].deleted) -> PointForkRefs(q)
                               [] OTHER -> {}
             /\ retired' = retired \cup back
             /\ marked' = marked \cup Marks(back)
       /\ UNCHANGED <<written, gone, ses, wal, tok, gc, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, taken, placed>>

HostNext ==
    \/ \E h \in Hosts : Crash(h) \/ ProcStep(h)
    \/ \E h \in Hosts, b \in Branches : Place(h, b) \/ StartDelete(h, b) \/ StartUnfork(h, b)
                                     \/ StartDeleteEarly(h, b) \/ ResumeRestore(h, b)
    \/ \E h \in Hosts, c, p \in Branches : StartRestore(h, c, p) \/ StartRestoreVersion(h, c, p)
    \/ \E c \in Branches : Contest(c)
    \/ \E m \in inflight : Land(m)

-----------------------------------------------------------------------------
(* The collector. *)

NoRecs == [b \in Branches |-> None]
\* A pass: its cell, what it listed, the records it read, the branches it recovered, whether one
\* was from a checkpoint, the references it listed, and the fork points' states it keeps, each
\* [ext, touched]. Idle, a pass keeps its cell: its epoch is that cell's collector lease's.
GcIdle == [pc |-> "idle", cell |-> 0, objs |-> {}, recs |-> NoRecs, live |-> Nothing,
           fromCkpt |-> FALSE, refs |-> {}, pts |-> {}, stale |-> {}]
Idle(g) == [GcIdle EXCEPT !.cell = g.cell]

\* Every object's key under cell c's prefix but references, which are no pass's candidates: the
\* child's cell's pass retires them.
Listing(c) ==
    LET B == CellBranches(c) IN
    {k \in {<<"seg", b, s, e>> : b \in B, s \in Seqs, e \in Epochs} : seg[k[2]][k[3]][k[4]] # None}
    \cup {<<"own", o.b, o.e, o.s, o.x>> : o \in {o \in own : o.b \in B}}
    \cup {k \in {<<"ckpt", b, s, e>> : b \in B, s \in Seqs, e \in Epochs} :
            ckpt[k[2]][k[3]][k[4]] # None}
    \cup {<<"rec", b>> : b \in {b \in B : rec[b] # None}}
    \cup {<<"head", b>> : b \in {b \in B : head[b] # None}}
    \cup {k \in {<<"moves", b, s, e, p>> : b \in B, s \in Seqs, e \in Epochs, p \in Ways} :
            moves[k[2]][k[3]][k[4]][k[5]] # None}

\* The state of p at its fork point at, as a pass keeps it for a fork to another cell: the extents
\* it names, and what recovering it reads (forkPointsKept); without the rule, the extents alone.
ForkPoint(p, at) ==
    LET r == StateAt(p, at) IN
    [ok |-> r.ok, ext |-> r.st.ext, touched |-> IF R("forkPointsKept") THEN r.touched ELSE {}]
Kept(fp) == [ext |-> fp.ext, touched |-> fp.touched]

\* The fork points of the references Rs: the states kept, and whether one is lost that a live
\* child of theirs needs. A child not live (deleted, being made of a deleted point, a claim, or
\* no record yet, or another's, its reference to be named or taken back: ReferencesNamed) loses
\* nothing.
RefPoints(Rs) ==
    LET P == {<<r.p, r.at>> : r \in Rs}
        lost(pt) == /\ ~ForkPoint(pt[1], pt[2]).ok
                    /\ \E r \in Rs : <<r.p, r.at>> = pt /\ LiveRec(r.c)
    IN [pts |-> {Kept(ForkPoint(pt[1], pt[2])) : pt \in {pt \in P : ForkPoint(pt[1], pt[2]).ok}},
        lost |-> \E pt \in P : lost(pt)]

\* Whether the writer of b may name the extents of its state, recovered from its head's checkpoint
\* newest (<<>> if none), anywhere on their ways from where they were sealed (waysFromSeals): unless
\* the checkpoint is of the head's epoch (collector::behind). A fork's inherited extents, which a
\* fork with no checkpoint holds, always.
Behind(b, newest) == R("waysFromSeals") /\ (newest = <<>> \/ newest[2] # HeadEpoch(b))

\* Whether acc holds a recovery of q, <<b, origin>>: by that origin (recoveriesByOrigins), or,
\* without the rule, by any.
Recovered(acc, q) ==
    IF R("recoveriesByOrigins") THEN q \in DOMAIN acc.live
    ELSE \E k \in DOMAIN acc.live : k[1] = q[1]

RECURSIVE RecoverAll(_, _, _)
\* The live branches recovered into acc, those queued, <<b, origin>>, each once by each origin
\* (recoveriesByOrigins), and the children of their forks with no record read (forksFromStates):
\* a child recovered by its parent's state is recovered again by a record found after naming
\* another origin, a restore's that took the ID (TASK-173). Of the forks to other cells, the
\* parent's state at each fork point is kept instead (forksFromStates too): the child's cell's
\* pass recovers the child. A failure that loses nothing (deleted since, a claim no writer has) is
\* skipped, the claim given up on; any other loses the pass. Each recovery keeps what its head
\* may yet name: of its epoch, the segments from its end and the checkpoints after its own.
RecoverAll(queue, recs, acc) ==
    IF queue = {} \/ acc.lost THEN acc
    ELSE LET q == CHOOSE q \in queue : TRUE
             b == q[1]
             rest == queue \ {q}
             r == Recover(b, q[2])
             found == IF R("forksFromStates") THEN r.st.forks ELSE {}
             kids == {<<f.child, <<b, f.at>>>> :
                        f \in {f \in found : ~Cross(b, f.child) /\ recs[f.child] = None}}
             \* Its fork points to other cells, and its versions the head it recovered from
             \* names (versionsKept), each a state kept whole.
             fps == {ForkPoint(b, f.at) : f \in {f \in found : Cross(b, f.child)}}
                    \cup (IF R("versionsKept") THEN {ForkPoint(b, v.at) : v \in HeadVers(b)}
                          ELSE {})
         IN IF Recovered(acc, q) THEN RecoverAll(rest, recs, acc)
            ELSE IF r.ok /\ \E fp \in fps : ~fp.ok THEN [acc EXCEPT !.lost = TRUE]
            ELSE IF r.ok THEN
               RecoverAll(rest \cup kids, recs,
                          [acc EXCEPT !.live = Put(@, q, [ext |-> r.st.ext, end |-> r.end,
                                                          epoch |-> HeadEpoch(b), ckpt |-> r.ckpt,
                                                          touched |-> r.touched,
                                                          behind |-> Behind(b, r.ckpt)]),
                                      !.fromCkpt = @ \/ r.ckpt # <<>>,
                                      !.pts = @ \cup {Kept(fp) : fp \in fps}])
            ELSE IF rec[b] = None \/ rec[b].deleted THEN RecoverAll(rest, recs, acc)
            ELSE IF rec[b].kind = "claim" /\ Unwritten(b)
                 THEN RecoverAll(rest, recs, [acc EXCEPT !.claims = @ \cup {b}])
            ELSE [acc EXCEPT !.lost = TRUE]

\* Whether a later pass of its cell took the cell's collector lease from pass i.
Fenced(i) == \E j \in (i + 1)..passes : gc[j].cell = gc[i].cell

\* The records of cell c's branches.
CellRecs(c) == [b \in Branches |-> IF CellOf[b] = c THEN rec[b] ELSE None]

\* A pass's start over cell c, taking the cell's collector lease, once the others are done (but
\* with "overlap", which lets passes of any cells, c's or others', run at once; a model without it
\* checks the cells' passes one after another, kept to a size TLC exhausts): the listing of c's
\* prefix (candidatesFirst), then its records. Without referencesRelisted, its references listed
\* now, and not again. The listing holds the cell's references and marks too: those of the
\* references it lists marked, retired or taken back, written again by a stale writer since, are
\* garbage, which it deletes (retirementsMarked).
GcStart(c) ==
    LET i == passes + 1 IN
    /\ "collect" \in Can /\ passes < MaxPasses
    /\ "overlap" \in Can \/ \A j \in DOMAIN gc : gc[j].pc = "idle"
    /\ gc' = [gc EXCEPT ![i] = [GcIdle EXCEPT !.pc = "records", !.cell = c, !.recs = CellRecs(c),
                                               !.objs = IF R("candidatesFirst") THEN Listing(c)
                                                        ELSE {},
                                               !.refs = IF R("referencesRelisted") THEN {}
                                                        ELSE RefsUnder(c),
                                               !.stale = RefsUnder(c) \cap marked]]
    /\ passes' = i
    /\ UNCHANGED <<bucketVars, hostVars, truthVars, gen, takes, places, crashes, mints>>

\* The records of the cell's branches pass i's listing has not read, which a listing finds.
Unread(i) == {b \in CellBranches(gc[i].cell) : gc[i].recs[b] = None /\ rec[b] # None}

\* The records listed again, until a listing finds none new (Collector::records): those new read
\* now, the others as they were read before.
GcRecords(i) ==
    /\ gc[i].pc = "records"
    /\ gc' = [gc EXCEPT ![i].recs = [b \in Branches |-> IF b \in Unread(i) THEN rec[b]
                                                       ELSE @[b]],
                        ![i].pc = IF Unread(i) = {} THEN "recover" ELSE "records"]
    /\ UNCHANGED <<bucketVars, hostVars, truthVars, countVars>>

\* The forks being made of points deleted given up on (collectorAbandons), then the live
\* branches recovered from their heads. Finding what one needs lost, a pass a later one took the
\* lease from stops (passesFencedStop): what it read may be older than what that pass, deleting
\* since, read. A point of another cell's is read now.
GcRecover(i) ==
    LET g == gc[i]
        pointRec(p) == IF CellOf[p] = g.cell THEN g.recs[p] ELSE rec[p]
        pointGone(p) == pointRec(p) = None \/ pointRec(p).deleted
        \* What a fork being made forks gone: its point's record deleted, or its version named by
        \* its branch's head no more, a GET of that head (TASK-476).
        forkedGone(r) == IF r.ver # <<>> THEN VersionGone(r.ver) ELSE pointGone(r.point)
        ab == IF R("collectorAbandons")
              THEN {b \in Branches : g.recs[b] # None /\ ~g.recs[b].deleted
                                     /\ g.recs[b].kind = "forking" /\ forkedGone(g.recs[b])}
              ELSE {}
        rec1 == [b \in Branches |-> IF b \in ab /\ rec[b] # None /\ rec[b].kind = "forking"
                                       THEN [rec[b] EXCEPT !.deleted = TRUE] ELSE rec[b]]
        recs == [b \in Branches |-> IF b \in ab THEN rec1[b] ELSE g.recs[b]]
        queue == {<<b, recs[b].origin>> :
                    b \in {b \in Branches : recs[b] # None /\ ~recs[b].deleted}}
        acc == RecoverAll(queue, recs, [live |-> g.live, fromCkpt |-> FALSE, claims |-> {},
                                        lost |-> FALSE, pts |-> g.pts])
    IN /\ g.pc = "recover"
       /\ IF acc.lost THEN
             /\ errors' = IF R("passesFencedStop") /\ Fenced(i) THEN errors
                          ELSE errors \cup {"a pass lost what a live branch needs"}
             /\ gc' = [gc EXCEPT ![i] = Idle(g)]
             /\ rec' = rec1
          ELSE
             /\ rec' = [b \in Branches |->
                         IF b \in acc.claims THEN [rec1[b] EXCEPT !.deleted = TRUE] ELSE rec1[b]]
             /\ gc' = [gc EXCEPT ![i] = [g EXCEPT !.recs = recs, !.live = acc.live,
                                                 !.pts = acc.pts, !.fromCkpt = acc.fromCkpt,
                                                 !.pc = "relist"]]
             /\ UNCHANGED errors
       /\ UNCHANGED <<seg, own, ckpt, head, moves, mflight, inflight, refs, marked, retired, written, gone,
                      hostVars, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, doomed, taken, placed>>

\* After the states: the cell's records listed again (recordsRelisted), while what it recovered
\* has a checkpoint of its own, new ones recovered too; and its references listed
\* (referencesRelisted), each fork point's state kept. A reference written after this listing
\* is of a fork whose parent's state at its fork point this pass kept, or never listed: a
\* writable fork's child is recorded, with its reference first, before its parent's deletion
\* marks it, which the pass read live, its state naming the fork, or deleted, after; and a
\* point's fork writes its reference before it reads the point's record, so that one reading
\* it live made its reference before the point's deletion, before the pass read the point
\* deleted, or its own reference retired by its cell's pass, after.
GcRelist(i) ==
    LET g == gc[i]
        recs == IF g.fromCkpt /\ R("recordsRelisted") THEN CellRecs(g.cell) ELSE g.recs
        listed == IF R("referencesRelisted") THEN RefsUnder(g.cell) ELSE g.refs
        kept == RefPoints(listed)
    IN /\ g.pc = "relist"
       /\ IF \E b \in Branches : recs[b] # None /\ g.recs[b] = None THEN
             /\ gc' = [gc EXCEPT ![i].recs = recs, ![i].pc = "records"]
             /\ UNCHANGED errors
          ELSE IF kept.lost THEN
             /\ errors' = IF R("passesFencedStop") /\ Fenced(i) THEN errors
                          ELSE errors \cup {"a pass lost what a live branch needs"}
             /\ gc' = [gc EXCEPT ![i] = Idle(g)]
          ELSE
             /\ gc' = [gc EXCEPT ![i] = [g EXCEPT !.recs = recs, !.refs = listed,
                                                 !.pts = @ \cup kept.pts, !.pc = "finish"]]
             /\ UNCHANGED errors
       /\ UNCHANGED <<bucketVars, hostVars, countVars>>
       /\ UNCHANGED <<commits, nextTag, acked, minted, twice, makers, doomed, taken, placed>>

\* Whether a dead cell may be removed with b's record r (recordsKept): deleted, a given root's, or a
\* given point's fork's made, its restore's alone, which no recorder makes again, no fork naming its
\* ID (idsGivenOnce). A writable fork's child's, a point's, a claim's, a deletion's of no record, or
\* a point's fork's never made, its restore perhaps yet to write its reference, stays. Without the
\* rule, any.
Removable(b, r) ==
    /\ r.deleted
    /\ ~R("recordsKept") \/ (b \in Given /\ (r.kind = "root" \/ (r.kind = "fork" /\ r.point # 0)))

\* Whether the reference of the deleted point's fork c, whose record is r, is retired unmarked
\* (forksMadeUnmarked): c given and made, by the restore that began it alone, the reference's one
\* writer, having written it before it made it; not one another resumed (resumesMarked), which the
\* restore that began it, slow, may write after. Without the rule, any point's fork's, made or not;
\* without resumesMarked, a resumed one's too.
Unmarked(c, r) ==
    /\ r.point # 0
    /\ R("forksMadeUnmarked") =>
          (c \in Given /\ r.kind = "fork" /\ (R("resumesMarked") => ~r.resumed))

\* Compaction, then deletion, unless a later pass of the cell took its lease: of what was listed,
\* what the live branches and the fork points kept do not need, nor their heads may yet name
\* (collectorEpochs). A fork point's state is behind (waysFromSeals): the child names what it
\* inherited where it read it. Then the references of the cell's deleted children retired, under
\* their parents' cells, once the cell has no live branch and no reference of its own
\* (referencesHeld): no branch anywhere inherits from them, a fork of one in the cell by its record,
\* in another cell by its reference. Without the rule, once the child is deleted. A dead cell, no
\* mark under it, its records each Removable, goes whole (TASK-455): its records and heads too, the
\* rest of it doomed already; a given point's fork's reference is retired unmarked (Unmarked).
GcFinish(i) ==
    LET g == gc[i]
        objs == IF R("candidatesFirst") THEN g.objs ELSE Listing(g.cell)
        L == g.live
        recs == g.recs
        touched == UNION {L[k].touched : k \in DOMAIN L} \cup UNION {p.touched : p \in g.pts}
        locs == UNION {{<<x, L[k].ext[x]>> : x \in DOMAIN L[k].ext} : k \in DOMAIN L}
                \cup UNION {{<<x, p.ext[x]>> : x \in DOMAIN p.ext} : p \in g.pts}
        \* The key along the way p from the segment of a location of a segment's or moves'.
        along(loc, p) == Key(loc[2], loc[3], loc[4], p)
        chained == {l \in locs : l[2][1] # "own"}
        \* The ways to each extent named, from where a state says it is: the objects listed, and
        \* the moves listed of each, whose maps say it moved there, any pass's.
        reached(l) ==
            LET step(P) == {q \in UNION {On(p) : p \in P} :
                              along(l[2], q) \in objs /\ Names(l[2][2], l[2][3], l[2][4], q, l[1])}
                r0 == {Way(l[2])}
            IN r0 \cup step(r0) \cup step(step(r0))
        namedAt(k) == {l[1] : l \in {l \in chained : \E p \in reached(l) : along(l[2], p) = k}}
        \* And the moves on the ways there from where they were sealed, of the states whose
        \* writers may be behind them (waysFromSeals).
        behind == UNION {{L[k].ext[x] : x \in DOMAIN L[k].ext} :
                            k \in {k \in DOMAIN L : L[k].behind}}
                  \cup (IF R("waysFromSeals")
                        THEN UNION {{p.ext[x] : x \in DOMAIN p.ext} : p \in g.pts} ELSE {})
        hops == UNION {{along(l[2], q) : q \in UNION {On(p) : p \in reached(l)}} : l \in chained}
                \cup UNION {{along(l, SubSeq(Way(l), 1, n)) : n \in 1..Depth(l)} :
                              l \in {l \in behind : l[1] = "moves"}}
        named == UNION {{along(l[2], p) : p \in reached(l)} : l \in chained}
                 \cup {<<"own", l[2][2], l[2][3], l[2][4], l[1]>> :
                         l \in {l \in locs : l[2][1] = "own"}}
        \* Whether b's head, as any recovery of b's read it, may yet name its segment or extent of
        \* its own at s by the writer of epoch e: of the head's epoch, or a later one, at or past
        \* its end (collectorEpochs). Of an earlier epoch, its writer's swaps all fail.
        ahead(b, s, e) ==
            \E k \in DOMAIN L : /\ k[1] = b /\ R("collectorEpochs")
                                /\ e >= L[k].epoch /\ s >= L[k].end
        \* Likewise its checkpoint as of s by that writer: newer than the head's, by s, then by
        \* epoch (a writer taking over may checkpoint what the head's did, at its own key).
        ckptAhead(b, s, e) ==
            \E k \in DOMAIN L : /\ k[1] = b /\ R("collectorEpochs") /\ e >= L[k].epoch
                                /\ L[k].ckpt = <<>> \/ ~GE2(L[k].ckpt, <<s, e>>)
        \* The moves of the object at k listed, any pass's: each holds what was named there as
        \* its pass read it, which no later state names more of.
        movesOf(k) == {Key(k[2], k[3], k[4], q) : q \in On(Way(k))} \cap objs
        \* Segments no recovery reads, which no head may yet name; moves, but the last hop's
        \* (compactMoves): each into moves of this pass's, if none are listed.
        compactable == {k \in named \cap objs :
                          /\ k[1] # "own" /\ Obj(k) # None
                          /\ IF k[1] = "seg"
                             THEN /\ "compact" \in Can /\ k \notin touched
                                  /\ ~ahead(k[2], k[3], k[4])
                             ELSE "compactMoves" \in Can /\ Depth(k) < Hops
                          /\ movesOf(k) = {}}
        dead == /\ \A b \in CellBranches(g.cell) : recs[b] = None \/ recs[b].deleted
                /\ g.refs = {}
        \* Removed whole: dead, no mark of a reference under it, which a stale writer's reference
        \* written again goes by, and every record it read one it may go with. A stale writer's
        \* head, of a branch removed (OpenGone), goes with a cell of no record.
        \* A fence (fencesKept): the head of a branch of no record read, final, a deletion's, whose
        \* record may yet come, the deletion's own or a slow writer's of its parent, and a writer
        \* taking the branch then must find it. Any other head of no record, a stale taker's of a
        \* branch removed (OpenGone), is garbage.
        fence(b) == /\ R("fencesKept") /\ recs[b] = None
                    /\ head[b] # None /\ head[b].epoch = Final
        removed == /\ dead
                   /\ \A r \in marked : CellOf[r.p] # g.cell
                   /\ \A b \in CellBranches(g.cell) : recs[b] = None \/ Removable(b, recs[b])
                   /\ \A b \in CellBranches(g.cell) : <<"head", b>> \in objs => ~fence(b)
        doomedKey(k) ==
            CASE k[1] = "ckpt" -> k \notin touched /\ ~ckptAhead(k[2], k[3], k[4])
              [] k[1] = "seg" -> /\ ~ahead(k[2], k[3], k[4]) /\ k \notin touched
                                 /\ (k \notin named \/ movesOf(k) # {})
              [] k[1] = "own" -> k \notin named /\ ~ahead(k[2], k[4], k[3])
              \* Moves go once no state's way goes through them, and they are moved in turn, or
              \* what they are of is gone.
              [] k[1] = "moves" ->
                   /\ k \notin hops
                   /\ IF k \in named THEN Depth(k) < Hops /\ movesOf(k) # {}
                      ELSE Key(k[2], k[3], k[4], SubSeq(Way(k), 1, Depth(k) - 1)) \notin objs
              \* Records and heads only with the cell (recordsKept): a deleted fork's record stays,
              \* which a slow writer of a parent naming its ID as a fork's child would make again
              \* (TASK-174), and a deleted branch's final head fences its writers. A head of no
              \* record read goes, unless a fence (fencesKept), whatever the cell; without the rule,
              \* with the cell.
              [] k[1] = "rec" -> removed
              [] k[1] = "head" -> IF R("fencesKept") /\ recs[k[2]] = None
                                  THEN head[k[2]] # None /\ ~fence(k[2])
                                  ELSE removed
        D == IF Fenced(i) THEN {} ELSE {k \in objs : doomedKey(k)}
        \* Each deleted child's reference, of an origin in another cell, its key by the child's
        \* record: the origin it names, never listed, marked (retirementsMarked), whether it is
        \* there or not (collector's retired).
        deleted == {c \in CellBranches(g.cell) : /\ recs[c] # None /\ recs[c].deleted
                                                 /\ recs[c].origin # <<>>
                                                 /\ Cross(recs[c].origin[1], c)}
        retiring == IF Fenced(i) \/ (R("referencesHeld") /\ ~dead) THEN {}
                    ELSE {RefOf(recs[c].origin[1], recs[c].origin[2], c) : c \in deleted}
        \* The cell's own references listed marked: retired before, or taken back.
        stale == IF Fenced(i) THEN {} ELSE g.stale
    IN /\ g.pc = "finish"
       \* Its compactions' PUTs in flight, landing whenever (GcLand), though the pass ended.
       /\ \E C \in SUBSET compactable :
             mflight' = mflight \cup {[k |-> Key(k[2], k[3], k[4], Way(k) \o <<KP(i)>>),
                                       o |-> [pass |-> i, names |-> namedAt(k)]] : k \in C}
       /\ moves' = [b \in Branches |-> [s \in Seqs |-> [e \in Epochs |-> [p \in Ways |->
                      IF Key(b, s, e, p) \in D THEN None ELSE moves[b][s][e][p]]]]]
       /\ seg' =[b \in Branches |-> [s \in Seqs |-> [e \in Epochs |->
                    IF <<"seg", b, s, e>> \in D THEN None ELSE seg[b][s][e]]]]
       /\ own' = {o \in own : <<"own", o.b, o.e, o.s, o.x>> \notin D}
       /\ ckpt' = [b \in Branches |-> [s \in Seqs |-> [e \in Epochs |->
                     IF <<"ckpt", b, s, e>> \in D THEN None ELSE ckpt[b][s][e]]]]
       /\ rec' = [b \in Branches |-> IF <<"rec", b>> \in D THEN None ELSE rec[b]]
       /\ head' = [b \in Branches |-> IF <<"head", b>> \in D THEN None ELSE head[b]]
       /\ gone' = gone \cup {b \in Branches : <<"rec", b>> \in D}
       /\ refs' = refs \ (retiring \cup stale)
       /\ retired' = retired \cup retiring
       /\ marked' = marked \cup Marks({r \in retiring : ~Unmarked(r.c, recs[r.c])})
       /\ gc' = [gc EXCEPT ![i] = Idle(g)]
       /\ UNCHANGED <<inflight, written, hostVars, truthVars, countVars>>

\* A compaction's PUT lands, whenever, though its pass ended, or a later one took the cell's
\* collector lease: created, unless moves are at its key (collector::compact_object's create).
\* Its key names its pass (movesByPass), so that none is ever there, nor was.
GcLand(m) ==
    /\ mflight' = mflight \ {m}
    /\ IF Obj(m.k) = None THEN
          /\ moves' = [moves EXCEPT ![m.k[2]][m.k[3]][m.k[4]][m.k[5]] = m.o]
          /\ written' = written \cup {<<m.k, m.o>>}
       ELSE UNCHANGED <<moves, written>>
    /\ UNCHANGED <<seg, own, ckpt, rec, head, inflight, refs, marked, retired, gone, hostVars, gc,
                   truthVars, countVars>>

GcNext ==
    \/ \E c \in Cells : GcStart(c)
    \/ \E i \in DOMAIN gc : GcRecords(i) \/ GcRecover(i) \/ GcRelist(i) \/ GcFinish(i)
    \/ \E m \in mflight : GcLand(m)

-----------------------------------------------------------------------------

Init ==
    /\ seg = [b \in Branches |-> [s \in Seqs |-> [e \in Epochs |-> None]]]
    /\ own = {}
    /\ ckpt = [b \in Branches |-> [s \in Seqs |-> [e \in Epochs |-> None]]]
    /\ rec = [b \in Branches |-> IF b = Root THEN Record("root", <<>>, 0, <<"root", Root>>, 0)
                                 ELSE None]
    /\ head = [b \in Branches |-> None]
    /\ moves = [b \in Branches |-> [s \in Seqs |-> [e \in Epochs |-> [p \in Ways |-> None]]]]
    /\ mflight = {}
    /\ inflight = {}
    /\ refs = {}
    /\ marked = {}
    /\ retired = {}
    /\ written = {}
    /\ gone = {}
    /\ ses = [h \in Hosts |-> [b \in Branches |-> None]]
    /\ wal = [h \in Hosts |-> [b \in Branches |-> <<>>]]
    /\ tok = [h \in Hosts |-> [b \in Branches |-> 0]]
    /\ proc = [h \in Hosts |-> None]
    /\ gc = [i \in 1..MaxPasses |-> GcIdle]
    /\ commits = [t \in 1..MaxCommits |-> [branch |-> 0, parent |-> 0, seals |-> {},
                                             free |-> None, ver |-> FALSE, dropped |-> FALSE]]
    /\ nextTag = 1
    /\ acked = [b \in Branches |-> {}]
    /\ minted = {}
    /\ twice = FALSE
    /\ makers = [b \in Branches |-> IF b = Root THEN {<<"root", Root>>} ELSE {}]
    /\ doomed = {}
    /\ taken = [b \in Branches |-> -1]
    /\ placed = [b \in Branches |-> 0]
    /\ errors = {}
    /\ gen = 0 /\ takes = 0 /\ passes = 0 /\ places = 0 /\ crashes = 0 /\ mints = 0

Next == WriterNext \/ HostNext \/ GcNext

Spec == Init /\ [][Next]_vars

\* Hosts are alike.
HostPerms == Permutations(Hosts)

-----------------------------------------------------------------------------
(* What must hold. *)

\* The branches whose records say they live: roots, forks' children, points.
LiveRecs == {b \in Branches : LiveRec(b)}

\* Nothing a live branch needs is deleted: it recovers from its head, every extent it names reads
\* back as itself, and a head a writer took is there; so does its state at each version its head
\* names (TASK-476); and the writer whose taking the head is reads every extent its guest holds
\* where its state acknowledged says it is.
NothingNeededGone ==
    /\ \A b \in LiveRecs :
          LET r == Recover(b, rec[b].origin) IN
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

\* Each live branch's history is a path of its own commits from the commit it started from,
\* every one applied once, holding what they made; and its state at each version its head names
\* is that history to the version's cut (TASK-476).
HistoriesOwn ==
    /\ \A b \in LiveRecs :
          LET r == Recover(b, rec[b].origin) IN
          r.ok => /\ OwnStart(b, r.st.tip) = rec[b].base
                  /\ ~r.st.dup
                  /\ r.st.hist = Anc(r.st.tip)
                  /\ DOMAIN r.st.ext = MadeBy(Path(r.st.tip), {})
    /\ \A b \in LiveRecs : \A v \in HeadVers(b) :
          LET r == StateAt(b, v.at) IN
          r.ok => /\ r.st.tip = v.id
                  /\ ~r.st.dup
                  /\ r.st.hist = Anc(v.id)
                  /\ DOMAIN r.st.ext = MadeBy(Path(v.id), {})

\* A version a writer of its branch acknowledged stays named by the branch's head until a writer
\* drops it, or a deletion ends the head (TASK-476): takeovers carry it, and no other swap names
\* it no more.
VersionsStay ==
    \A b \in Branches :
        head[b] # None /\ head[b].epoch # Final =>
            \A t \in acked[b] :
                commits[t].ver /\ commits[t].branch = b /\ ~commits[t].dropped =>
                    \E v \in head[b].vers : v.id = t

\* No acknowledged commit is lost: each is in its branch's history, and in the guest's of the
\* writer whose taking the head is.
NoAckedLost ==
    /\ \A b \in LiveRecs :
          LET r == Recover(b, rec[b].origin) IN r.ok => acked[b] \subseteq Anc(r.st.tip)
    /\ \A h \in Hosts, b \in LiveRecs :
          (Ready(h, b) /\ Taking(ses[h][b], b)) =>
              acked[b] \subseteq Anc(View(h, b).tip)

\* No extent ID is minted twice.
NoIdTwice == ~twice

\* Nothing writes a point: no segment, checkpoint or writer's head of one.
PointsUnwritten ==
    \A b \in Branches : rec[b] # None /\ rec[b].kind = "point" =>
        /\ \A s \in Seqs, e \in Epochs : seg[b][s][e] = None /\ ckpt[b][s][e] = None
        /\ head[b] = None \/ head[b].epoch = Final

\* No placement takes a branch back from a later one.
HeadsCurrent == \A b \in Branches : head[b] # None /\ head[b].epoch # Final =>
                                        head[b].token >= taken[b]

\* A making told it made a branch loses it only to a deletion of it asked by who may.
DeletedOnlyByDeletes ==
    \A b \in Branches : rec[b] # None /\ rec[b].deleted /\ rec[b].maker \in Made =>
                            rec[b].maker \in doomed

\* One making, not deleted, is told it made each branch.
OneMaker == \A b \in Branches : Cardinality(makers[b] \ doomed) <= 1

\* Nothing failed that must not: a pass of the collector, a deletion, a store opened (its log
\* ending inside a commit), a WAL diverging from a branch whose last commit is its own.
NoErrors == errors = {}

\* A key names one object, ever (TASK-407): every PUT at a segment's, a checkpoint's or moves' key
\* writes what every other at it did, if any did, deleted since or not. Readers need no versions of
\* a key: what the host keeps of it, in memory, in its part cache, or asked of its peers, is of
\* the one object.
KeysWrittenOnce == \A p, q \in written : p[1] = q[1] => p[2] = q[2]

\* Every reference is named by its child's record, its origin the reference's fork point, which
\* the child's cell's pass retires it by once the child is deleted (TASK-395.10); or is of a fork
\* whose records' writers are yet to name or take it back: a live parent's state names the fork,
\* whose next writer or deletion records it again; a writer or a deletion of it records it now; a
\* fork of a point is being made; or it is marked, retired or taken back before, a stale writer's,
\* which its parent's cell's next pass deletes (retirementsMarked). Any other no pass retires, for
\* ever.
RefPending(r) ==
    LET at(F) == \E f \in F : f.child = r.c /\ f.at = r.at IN
    \/ /\ rec[r.p] # None /\ ~rec[r.p].deleted
       /\ LET st == Recover(r.p, rec[r.p].origin) IN st.ok /\ at(st.st.forks)
    \/ \E h \in Hosts : ses[h][r.p] # None /\ ses[h][r.p].pc = "record" /\ at(ses[h][r.p].st.forks)
    \/ \E h \in Hosts : /\ proc[h] # None /\ proc[h].op = "delete" /\ proc[h].b = r.p
                       /\ proc[h].pc \in {"refs", "write", "record"} /\ at(proc[h].fs)
    \/ \E h \in Hosts : /\ proc[h] # None /\ proc[h].op = "restore" /\ proc[h].c = r.c
                       /\ proc[h].og = <<r.p, r.at>>
    \/ r \in marked
ReferencesNamed ==
    \A r \in refs : (rec[r.c] # None /\ rec[r.c].origin = <<r.p, r.at>>) \/ RefPending(r)

\* A reference standing after its retirement or take-back is marked (TASK-395.13, TASK-395.14):
\* a stale writer wrote it again, its read of the child's record from before, none there, the
\* child deleted or its ID taken since. No record will name it, nor will its child's cell's
\* passes retire it again, skipping the dead cell, whose listing it changes not, nor will a
\* recorder take it back, the fork recorded; its parent's cell's listing it changes, and that
\* cell's next pass deletes it, by its mark.
ReferencesRetiredMarked == \A r \in refs \cap retired : r \in marked

\* A branch whose record a pass removed with its cell never lives again, nor is being made again
\* (TASK-455): no recorder makes its record, nor a fork names its ID. A deletion run again may
\* leave it a record deleted.
RemovedStayGone == \A b \in gone : rec[b] = None \/ rec[b].deleted

\* No writer holds a deleted branch (TASK-455's review): a deletion's final head, its fence, stands
\* from its swap until the record says the branch is deleted, and after, while the record does;
\* no writer takes it, and the one that held it stops at its next swap.
DeletedUntaken ==
    \A h \in Hosts, b \in Branches :
        (ses[h][b] # None /\ ses[h][b].pc \in {"record", "ready"} /\ Taking(ses[h][b], b))
            => rec[b] # None /\ ~rec[b].deleted

\* A segment a head names keeps its epoch while the head stands (TASK-449): an acknowledgement names
\* more, a takeover goes on from the head's end, a deletion keeps what it names, and nothing
\* renames one; only a removal deletes a head, with its cell (recordsKept). So a host keeps the
\* epoch of a fork point's segment once it read it (extent_bucket::known), and reads the
\* checkpoint there by its key, with no read of its origin's head, whenever after.
NamedEpochsStay ==
    [][\A b \in Branches :
          head'[b] # None => \A s \in 0..(End(b) - 1) : s < End(b)' /\ Chain(b)'[s + 1] = Chain(b)[s + 1]
      ]_vars

\* Every live branch forked from another cell, and every point's fork, has its reference under
\* its origin's cell, naming its fork point (TASK-395): written before its record, retired only
\* once it is deleted.
ReferencesStand ==
    \A b \in LiveRecs :
        rec[b].origin # <<>>
        /\ (Cross(rec[b].origin[1], b) \/ rec[b].point # 0 \/ rec[b].ver # <<>>) =>
            RefOf(rec[b].origin[1], rec[b].origin[2], b) \in refs
=============================================================================
