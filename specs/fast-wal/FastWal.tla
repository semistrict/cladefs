---------------------------- MODULE FastWal ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Fast-WAL commits (doc-29): low-latency (~2-3 ms) host-loss-proof        *)
(* commits using an appendable object store (S3 Express One Zone via       *)
(* PutObject x-amz-write-offset-bytes, or GCS Rapid Storage via            *)
(* BidiWriteObject) with ZERO load-bearing local SSD state on compute      *)
(* hosts.                                                                  *)
(*                                                                         *)
(* What is modelled, per doc-29:                                           *)
(*                                                                         *)
(* - The cold regional bucket holds the cell's head (`head`: epoch,        *)
(*   acknowledged commit prefix `up`, active fast-WAL epoch `wal`),        *)
(*   cold segments, and large extents (`coldExt`: extents >= threshold     *)
(*   uploaded directly to the cold bucket so they never traverse or bloat  *)
(*   the fast zonal bucket).                                               *)
(* - The fast zonal bucket holds appendable WAL objects `walObj[ep][seg]`  *)
(*   (`0..MaxSegs`), each bounded to at most `MaxParts` appends (S3        *)
(*   Express's 10,000-part cap modelled in miniature). Every append carries*)
(*   its expected offset (`off`), rejected unless `off = Len(obj)`.        *)
(* - Adaptive group-commit batching: a serving writer commits 1..K commits *)
(*   into its in-memory log and appends any contiguous batch `from..to` in *)
(*   a single fast-WAL append request (`inFlight`).                        *)
(* - Large-write optimization: a large commit's extent is uploaded directly*)
(*   to `coldExt` BEFORE the fast-WAL batch referencing it is sent; the    *)
(*   collector (`GcCold`) protects unacknowledged large extents of both    *)
(*   `head.epoch` and `head.wal`.                                          *)
(* - Takeover: bumps `head.epoch` while leaving `head.wal` pointing at the *)
(*   old WAL epoch; fences the old WAL (walking past `rolled` segments and *)
(*   appending `fence` at the open/empty segment); recovers all commits    *)
(*   past `head.up` (copying any unacked large extents to the new epoch);  *)
(*   and atomically flushes them into `head.up` while switching `head.wal` *)
(*   to the new epoch in one CAS (`Activate`).                             *)
(* - Failures: compute hosts can crash at any point (`CrashHost`), losing  *)
(*   all in-memory and local disk state; in-flight network requests from   *)
(*   zombie or crashed writers remain in `inFlight` and may arrive late.   *)
(*                                                                         *)
(* Protocol rules (`AllRules`), each tested by a mutation under            *)
(* specs/fast-wal/mutations/ that removes it and expects TLC to catch it:  *)
(*                                                                         *)
(* - fenceBeforeRecover: taker fences old WAL before recovering (NoFence). *)
(* - offsetChecked: fast bucket checks expected offset on every append     *)
(*   (NoOffsetCheck).                                                      *)
(* - rollBeforeNext: writer waits for `rolled` to land on `seg` before     *)
(*   opening `seg + 1` (NoRollBeforeNext).                                 *)
(* - fenceFollowsRoll: taker seeing `rolled` on `seg` continues fencing at *)
(*   `seg + 1` (NoFenceFollowsRoll).                                       *)
(* - rollBeforeLimit: writer stops appending batches to `seg` while at     *)
(*   least one part slot remains for `rolled` or `fence`                   *)
(*   (NoRollBeforeLimit).                                                  *)
(* - keepOldWalInHead: `Take` keeps `head.wal` pointing at the old WAL     *)
(*   epoch until `Activate` (ClearWalOnTake).                              *)
(* - activateAfterFlush: `Activate` flushes recovered commits into         *)
(*   `head.up` in the same CAS that switches `head.wal`                    *)
(*   (ActivateBeforeFlush).                                                *)
(* - largeBeforeWal: a large commit's extent must land in `coldExt` before *)
(*   any fast-WAL batch naming it is sent (WalBeforeLarge).                *)
(* - protectWalEpoch: `GcCold` keeps unacknowledged `coldExt` of           *)
(*   `head.wal` as well as `head.epoch` (GcDeletesWalExtents).             *)
(* - deleteWalAfterAck: writer deletes a rolled WAL object only after      *)
(*   `head.up` acknowledges all commits in it (DeleteWalBeforeAck).        *)
(* - sweepOnlySuperseded: fast-WAL sweeper reclaims `walObj[ep]` only when *)
(*   `ep < head.epoch /\ ep # head.wal` (SweepByHeadEpoch).                *)
(* - tombstoneOnSweep: sweeping a superseded WAL object overwrites it with *)
(*   a tiny Fence tombstone (size between 0 and HEADER_LEN) rather than    *)
(*   deleting it to empty, so a delayed zombie `offset = 0` append cannot  *)
(*   resurrect the object without its fence (SweepDeletesFence; found by   *)
(*   TLC on the very first run of FastWalUploads!).                        *)
(* - rolledTombstoneOnDelete: reclaiming a rolled, head-acked WAL segment  *)
(*   overwrites it with a tiny Rolled tombstone rather than Fence or empty,*)
(*   so a taker still walks forward to `seg + 1` while blocking delayed    *)
(*   `offset = 0` retries (DeleteClearsRolled).                            *)
(* - walContiguous: fast-WAL appends never skip a commit that is not       *)
(*   already in `head.up` (WalSkipsUnacked).                               *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Hosts,          \* compute hosts (stateless / cache-only)
    MaxCommits,     \* commits at most, across all writers
    MaxTakes,       \* takes at most: epochs 1..MaxTakes
    MaxSegs,        \* WAL segment numbers 0..MaxSegs per epoch
    MaxParts,       \* max appends per WAL object (models S3 Express 10,000 cap)
    MaxCrashes,     \* compute host crashes at most
    Can,            \* optional action kinds enabled in this model
    Rules           \* protocol rules enabled (AllRules except in mutations)

AllRules == {
    "fenceBeforeRecover",
    "offsetChecked",
    "rollBeforeNext",
    "fenceFollowsRoll",
    "rollBeforeLimit",
    "keepOldWalInHead",
    "activateAfterFlush",
    "largeBeforeWal",
    "protectWalEpoch",
    "deleteWalAfterAck",
    "sweepOnlySuperseded",
    "tombstoneOnSweep",
    "rolledTombstoneOnDelete",
    "walContiguous"
}

AllCan == {"upload", "large", "roll", "delete", "sweep", "gc", "skip"}

ASSUME Rules \subseteq AllRules
ASSUME Can \subseteq AllCan
ASSUME MaxParts >= 2
ASSUME MaxTakes >= 1
ASSUME MaxSegs >= 0

R(rule) == rule \in Rules

Ids == 1..MaxCommits
Epochs == 1..MaxTakes
Segs == 0..MaxSegs

Range(s) == {s[i] : i \in DOMAIN s}
IsPrefix(a, b) == Len(a) <= Len(b) /\ a = SubSeq(b, 1, Len(a))
EndsWith(seq, k) == Len(seq) > 0 /\ seq[Len(seq)].kind = k
IsTombstone(seq) == Len(seq) = 1 /\ seq[1].kind \in {"fence_tombstone", "rolled_tombstone"}
IsRolled(seq) == EndsWith(seq, "rolled") \/ EndsWith(seq, "rolled_tombstone")
IsFenced(seq) == EndsWith(seq, "fence") \/ EndsWith(seq, "fence_tombstone")

RECURSIVE Flatten(_, _)
Flatten(seq, i) ==
    IF i > Len(seq) THEN <<>>
    ELSE IF seq[i].kind = "batch" THEN seq[i].commits \o Flatten(seq, i + 1)
    ELSE IF seq[i].kind \in {"fence", "fence_tombstone"} THEN <<>>
    ELSE Flatten(seq, i + 1)

SegCommits(seq) == Flatten(seq, 1)

RECURSIVE DropIn(_, _)
DropIn(seq, S) ==
    IF seq = <<>> THEN <<>>
    ELSE IF Head(seq) \in S THEN DropIn(Tail(seq), S)
    ELSE seq

NoReq == [ep |-> 0, seg |-> 0, off |-> 0, kind |-> "none", commits |-> <<>>, toIdx |-> 0]

NoSes == [
    st         |-> "none",
    ep         |-> 0,
    oldWal     |-> 0,
    curSeg     |-> 0,
    fencedSeg  |-> 0,
    log        |-> <<>>,
    walSent    |-> 0,
    walAcked   |-> 0,
    walOff     |-> 0,
    pendReq    |-> NoReq,
    coldLanded |-> 0
]

VARIABLES
    head,       \* [epoch, up, wal] in the cold regional bucket
    headExt,    \* set of [ep, c] large extents named by head.up
    coldExt,    \* set of [ep, c] large extents present in the cold regional bucket
    walObj,     \* [ep \in Epochs |-> [seg \in Segs |-> Seq(Entry)]] in the fast zonal bucket
    inFlight,   \* set of in-flight append/roll requests to the fast zonal bucket
    doneReqs,   \* set of requests accepted by the fast zonal bucket (for writer ack)
    ses,        \* [h \in Hosts |-> session state] (volatile memory only; lost on crash)
    told,       \* commits acknowledged to the guest (FastWal or Uploaded)
    large,      \* commits that carry a large extent
    pred,       \* [id \in Ids |-> preceding commit ID when created, or 0]
    next,       \* commit ID counter
    crashes     \* compute host crashes so far

vars == <<head, headExt, coldExt, walObj, inFlight, doneReqs, ses, told, large, pred, next, crashes>>

RECURSIVE AllSegCommits(_, _)
AllSegCommits(ep, s) ==
    IF s = 0 THEN SegCommits(walObj[ep][0])
    ELSE AllSegCommits(ep, s - 1) \o SegCommits(walObj[ep][s])

HighestExisting(ep) ==
    LET nonEmpty == {s \in Segs : walObj[ep][s] # <<>>}
    IN IF nonEmpty = {} THEN 0
       ELSE CHOOSE s \in nonEmpty : \A t \in nonEmpty : t <= s

TypeOK ==
    /\ head.epoch \in 0..MaxTakes
    /\ head.wal \in 0..MaxTakes
    /\ \A h \in Hosts : ses[h].st \in {"none", "fencing", "recovering", "activating", "serving"}
    /\ told \subseteq Ids
    /\ large \subseteq Ids

Init ==
    /\ head = [epoch |-> 0, up |-> <<>>, wal |-> 0]
    /\ headExt = {}
    /\ coldExt = {}
    /\ walObj = [ep \in Epochs |-> [s \in Segs |-> <<>>]]
    /\ inFlight = {}
    /\ doneReqs = {}
    /\ ses = [h \in Hosts |-> NoSes]
    /\ told = {}
    /\ large = {}
    /\ pred = [id \in Ids |-> 0]
    /\ next = 0
    /\ crashes = 0

(***************************************************************************)
(* Takeover: Take -> FenceStep* -> RecoverAndFlush -> Activate             *)
(***************************************************************************)

\* A compute host takes the cell's head in the cold regional bucket, bumping head.epoch.
\* By keepOldWalInHead, head.wal stays pointing at the old WAL epoch until Activate.
Take(h) ==
    LET e == head.epoch + 1
        old == head.wal
    IN
    /\ ses[h].st = "none"
    /\ e <= MaxTakes
    /\ head' = [head EXCEPT !.epoch = e,
                            !.wal = IF R("keepOldWalInHead") THEN old ELSE e]
    /\ ses' = [ses EXCEPT ![h] =
        IF old = 0
        THEN [NoSes EXCEPT !.st = "recovering", !.ep = e, !.oldWal = 0]
        ELSE [NoSes EXCEPT !.st = "fencing", !.ep = e, !.oldWal = old,
                           !.curSeg = HighestExisting(old)]]
    /\ UNCHANGED <<headExt, coldExt, walObj, inFlight, doneReqs, told, large, pred, next, crashes>>

\* Taker h fences the active fast-WAL object `curSeg` of `oldWal`.
\* - If `curSeg` ends in `rolled`, walk forward to `curSeg + 1` (fenceFollowsRoll).
\* - If `curSeg` already ends in `fence` (from a prior crashed taker), fencing is done.
\* - Otherwise append `fence` at the current end (fenceBeforeRecover), requiring room (< MaxParts).
FenceStep(h) ==
    LET s == ses[h]
        old == s.oldWal
        k == s.curSeg
        obj == walObj[old][k]
    IN
    /\ s.st = "fencing"
    /\ IF IsRolled(obj) THEN
           IF R("fenceFollowsRoll") /\ k < MaxSegs THEN
               /\ ses' = [ses EXCEPT ![h].curSeg = k + 1]
               /\ UNCHANGED walObj
           ELSE
               /\ ~R("fenceFollowsRoll")
               /\ ses' = [ses EXCEPT ![h].st = "recovering", ![h].fencedSeg = k]
               /\ UNCHANGED walObj
       ELSE IF IsFenced(obj) THEN
           /\ ses' = [ses EXCEPT ![h].st = "recovering", ![h].fencedSeg = k]
           /\ UNCHANGED walObj
       ELSE IF R("fenceBeforeRecover") THEN
           /\ Len(obj) < MaxParts
           /\ walObj' = [walObj EXCEPT ![old][k] = Append(@, [kind |-> "fence", commits |-> <<>>])]
           /\ ses' = [ses EXCEPT ![h].st = "recovering", ![h].fencedSeg = k]
       ELSE
           \* Mutation NoFence: proceed to recovery without fencing the WAL object.
           /\ ses' = [ses EXCEPT ![h].st = "recovering", ![h].fencedSeg = k]
           /\ UNCHANGED walObj
    /\ UNCHANGED <<head, headExt, coldExt, inFlight, doneReqs, told, large, pred, next, crashes>>

\* Taker h reads the fenced fast WAL, replays commits past head.up, and copies any
\* unacknowledged large extents from oldWal's cold keys into its own epoch's cold keys.
RecoverAndFlush(h) ==
    LET s == ses[h]
        extra == IF s.oldWal = 0 THEN <<>>
                 ELSE DropIn(AllSegCommits(s.oldWal, s.fencedSeg), Range(head.up))
        rec == head.up \o extra
        newLarge == Range(extra) \cap large
        copied == {[ep |-> s.ep, c |-> id] : id \in newLarge}
    IN
    /\ s.st = "recovering"
    \* Every large extent referenced by an unacknowledged recovered commit must exist in coldExt.
    /\ \A id \in newLarge : [ep |-> s.oldWal, c |-> id] \in coldExt
    /\ coldExt' = coldExt \cup copied
    /\ ses' = [ses EXCEPT ![h].st = "activating",
                          ![h].log = rec,
                          ![h].coldLanded = Len(rec)]
    /\ UNCHANGED <<head, headExt, walObj, inFlight, doneReqs, told, large, pred, next, crashes>>

\* Taker h activates epoch `s.ep` with a single conditional swap on head:
\* by activateAfterFlush, it flushes the recovered commits into head.up in the same CAS
\* that switches head.wal to s.ep.
Activate(h) ==
    LET s == ses[h]
        newLarge == (Range(s.log) \ Range(head.up)) \cap large
        addedExt == {[ep |-> s.ep, c |-> id] : id \in newLarge}
    IN
    /\ s.st = "activating"
    /\ head.epoch = s.ep
    /\ IF R("activateAfterFlush") THEN
           /\ head' = [head EXCEPT !.up = s.log, !.wal = s.ep]
           /\ headExt' = headExt \cup addedExt
       ELSE
           \* Mutation ActivateBeforeFlush: switch head.wal without acknowledging recovered tail.
           /\ head' = [head EXCEPT !.wal = s.ep]
           /\ UNCHANGED headExt
    /\ ses' = [ses EXCEPT ![h].st = "serving",
                          ![h].oldWal = 0,
                          ![h].curSeg = 0,
                          ![h].walOff = 0,
                          ![h].walSent = Len(s.log),
                          ![h].walAcked = Len(s.log)]
    /\ UNCHANGED <<coldExt, walObj, inFlight, doneReqs, told, large, pred, next, crashes>>

(***************************************************************************)
(* Serving writer: commits, large extents, adaptive batching, rolling      *)
(***************************************************************************)

\* Guest issues a commit (small or large); ordered in writer's volatile memory only.
Commit(h) ==
    LET s == ses[h]
        id == next + 1
        prev == IF s.log = <<>> THEN 0 ELSE s.log[Len(s.log)]
    IN
    /\ s.st = "serving"
    /\ next < MaxCommits
    /\ \E isLarge \in (IF "large" \in Can THEN {FALSE, TRUE} ELSE {FALSE}) :
        /\ next' = id
        /\ large' = IF isLarge THEN large \cup {id} ELSE large
        /\ pred' = [pred EXCEPT ![id] = prev]
        /\ ses' = [ses EXCEPT ![h].log = Append(@, id)]
    /\ UNCHANGED <<head, headExt, coldExt, walObj, inFlight, doneReqs, told, crashes>>

\* Large-write optimization: writer uploads a large commit's extent straight to the cold
\* regional bucket before referencing it in the fast WAL.
PutLargeExtent(h, id) ==
    LET s == ses[h] IN
    /\ "large" \in Can
    /\ s.st = "serving"
    /\ id \in Range(s.log) \cap large
    /\ [ep |-> s.ep, c |-> id] \notin coldExt
    /\ coldExt' = coldExt \cup {[ep |-> s.ep, c |-> id]}
    /\ UNCHANGED <<head, headExt, walObj, inFlight, doneReqs, ses, told, large, pred, next, crashes>>

\* Adaptive group-commit batching: writer sends a batch of 1..K pending commits in one
\* offset-checked append to its active fast-WAL object `curSeg`.
\* One append in flight per writer at a time (sequential WriteOffsetBytes / stream order).
SendBatch(h) ==
    LET s == ses[h]
        maxBatchOff == IF R("rollBeforeLimit") THEN MaxParts - 1 ELSE MaxParts
    IN
    /\ s.st = "serving"
    /\ s.pendReq = NoReq
    /\ s.walSent < Len(s.log)
    /\ s.walOff < maxBatchOff
    /\ \E startIdx \in (s.walSent + 1)..Len(s.log), toIdx \in (s.walSent + 1)..Len(s.log) :
        /\ startIdx <= toIdx
        \* By walContiguous, writer never skips a commit unless it is already in head.up.
        /\ (startIdx = s.walSent + 1)
           \/ (Range(SubSeq(s.log, s.walSent + 1, startIdx - 1)) \subseteq Range(head.up))
           \/ ("skip" \in Can /\ ~R("walContiguous"))
        /\ LET batch == SubSeq(s.log, startIdx, toIdx)
               req == [ep |-> s.ep, seg |-> s.curSeg, off |-> s.walOff,
                       kind |-> "batch", commits |-> batch, toIdx |-> toIdx]
           IN
           \* By largeBeforeWal, every large extent named by the batch must be in coldExt first.
           /\ ~R("largeBeforeWal")
              \/ \A id \in Range(batch) \cap large : [ep |-> s.ep, c |-> id] \in coldExt
           /\ inFlight' = inFlight \cup {req}
           /\ ses' = [ses EXCEPT ![h].pendReq = req, ![h].walSent = toIdx]
    /\ UNCHANGED <<head, headExt, coldExt, walObj, doneReqs, told, large, pred, next, crashes>>

\* Writer closes its current WAL object with a `rolled` marker to advance to `curSeg + 1`.
SendRoll(h) ==
    LET s == ses[h] IN
    /\ "roll" \in Can
    /\ s.st = "serving"
    /\ s.pendReq = NoReq
    /\ s.curSeg < MaxSegs
    /\ s.walOff > 0
    /\ s.walOff < MaxParts
    /\ IF R("rollBeforeNext") THEN
           LET req == [ep |-> s.ep, seg |-> s.curSeg, off |-> s.walOff,
                       kind |-> "rolled", commits |-> <<>>, toIdx |-> s.walAcked]
           IN /\ inFlight' = inFlight \cup {req}
              /\ ses' = [ses EXCEPT ![h].pendReq = req]
       ELSE
           \* Mutation NoRollBeforeNext: jump straight to curSeg + 1 without landing `rolled`.
           /\ ses' = [ses EXCEPT ![h].curSeg = s.curSeg + 1, ![h].walOff = 0]
           /\ UNCHANGED inFlight
    /\ UNCHANGED <<head, headExt, coldExt, walObj, doneReqs, told, large, pred, next, crashes>>

\* Fast zonal bucket processes an in-flight append/roll request (including delayed ones
\* from zombie or crashed writers). Rejects if offset mismatches (offsetChecked), object is
\* a tombstone (size between 0 and HEADER_LEN, matching no valid writer offset), or full.
DeliverFastWal(req) ==
    LET obj == walObj[req.ep][req.seg] IN
    /\ req \in inFlight
    /\ Len(obj) < MaxParts
    /\ ~R("offsetChecked") \/ (~IsTombstone(obj) /\ req.off = Len(obj))
    /\ walObj' = [walObj EXCEPT ![req.ep][req.seg] =
                    Append(@, [kind |-> req.kind, commits |-> req.commits])]
    /\ inFlight' = inFlight \ {req}
    /\ doneReqs' = doneReqs \cup {req}
    /\ UNCHANGED <<head, headExt, coldExt, ses, told, large, pred, next, crashes>>

\* Fast zonal bucket rejects a stale in-flight request (offset mismatch, tombstone, or full).
RejectFastWal(req) ==
    LET obj == walObj[req.ep][req.seg] IN
    /\ req \in inFlight
    /\ (R("offsetChecked") /\ (IsTombstone(obj) \/ req.off # Len(obj))) \/ Len(obj) >= MaxParts
    /\ inFlight' = inFlight \ {req}
    /\ UNCHANGED <<head, headExt, coldExt, walObj, doneReqs, ses, told, large, pred, next, crashes>>

\* Writer h learns that its pending fast-WAL append/roll landed, advancing offset and
\* acknowledging batch commits to the guest (`told`).
LearnFastWal(h) ==
    LET s == ses[h]
        req == s.pendReq
    IN
    /\ s.st = "serving"
    /\ req # NoReq
    /\ req \in doneReqs
    /\ IF req.kind = "batch" THEN
           /\ ses' = [ses EXCEPT ![h].pendReq = NoReq,
                                 ![h].walOff = s.walOff + 1,
                                 ![h].walAcked = req.toIdx]
           /\ told' = told \cup Range(req.commits)
       ELSE
           \* `rolled` landed: now safe to open `curSeg + 1` at offset 0.
           /\ ses' = [ses EXCEPT ![h].pendReq = NoReq,
                                 ![h].curSeg = s.curSeg + 1,
                                 ![h].walOff = 0]
           /\ UNCHANGED told
    /\ UNCHANGED <<head, headExt, coldExt, walObj, inFlight, doneReqs, large, pred, next, crashes>>

(***************************************************************************)
(* Background cold uploader, WAL deletion, sweeping, and cold GC           *)
(***************************************************************************)

\* Background uploader lands segments (up to `toCold`) in the cold regional bucket.
\* Any large extents in those commits must already be in coldExt.
ColdLand(h) ==
    LET s == ses[h] IN
    /\ "upload" \in Can
    /\ s.st = "serving"
    /\ s.coldLanded < Len(s.log)
    /\ \E toCold \in (s.coldLanded + 1)..Len(s.log) :
        /\ \A id \in Range(SubSeq(s.log, s.coldLanded + 1, toCold)) \cap large :
             [ep |-> s.ep, c |-> id] \in coldExt
        /\ ses' = [ses EXCEPT ![h].coldLanded = toCold]
    /\ UNCHANGED <<head, headExt, coldExt, walObj, inFlight, doneReqs, told, large, pred, next, crashes>>

\* Cold head CAS acknowledging what h landed; releases any waiter via Uploaded too.
ColdAck(h) ==
    LET s == ses[h]
        newLarge == (Range(SubSeq(s.log, 1, s.coldLanded)) \ Range(head.up)) \cap large
        addedExt == {[ep |-> s.ep, c |-> id] : id \in newLarge}
    IN
    /\ "upload" \in Can
    /\ s.st = "serving"
    /\ head.epoch = s.ep
    /\ s.coldLanded > Len(head.up)
    /\ head' = [head EXCEPT !.up = SubSeq(s.log, 1, s.coldLanded)]
    /\ headExt' = headExt \cup addedExt
    /\ told' = told \cup Range(SubSeq(s.log, 1, s.coldLanded))
    /\ UNCHANGED <<coldExt, walObj, inFlight, doneReqs, ses, large, pred, next, crashes>>

\* Serving writer reclaims an already-rolled fast-WAL object once its commits are
\* acknowledged in head.up (deleteWalAfterAck). By rolledTombstoneOnDelete, it overwrites
\* the object with a tiny `rolled_tombstone` (not `fence_tombstone` and not empty) so a
\* taker still walks past it to `seg + 1` while blocking any delayed `offset = 0` retry.
DeleteWal(h, seg) ==
    LET s == ses[h]
        obj == walObj[s.ep][seg]
        covered == IF R("deleteWalAfterAck") THEN Range(head.up)
                   ELSE Range(SubSeq(s.log, 1, s.coldLanded))
        reclaimed == IF R("rolledTombstoneOnDelete")
                     THEN <<[kind |-> "rolled_tombstone", commits |-> <<>>]>>
                     ELSE <<[kind |-> "fence_tombstone", commits |-> <<>>]>>
    IN
    /\ "delete" \in Can
    /\ s.st = "serving"
    /\ seg < s.curSeg
    /\ obj # <<>>
    /\ ~IsTombstone(obj)
    /\ EndsWith(obj, "rolled")
    /\ Range(SegCommits(obj)) \subseteq covered
    /\ walObj' = [walObj EXCEPT ![s.ep][seg] = reclaimed]
    /\ UNCHANGED <<head, headExt, coldExt, inFlight, doneReqs, ses, told, large, pred, next, crashes>>

\* Fast-WAL sweeper reclaims WAL objects of superseded epochs (sweepOnlySuperseded:
\* ep < head.epoch AND ep # head.wal). By tombstoneOnSweep, it overwrites the object with a
\* tiny `fence_tombstone` rather than deleting it to empty, preventing a delayed zombie
\* `offset = 0` append from resurrecting the object without its fence.
SweepWal(ep, seg) ==
    /\ "sweep" \in Can
    /\ walObj[ep][seg] # <<>>
    /\ ~IsTombstone(walObj[ep][seg])
    /\ ep < head.epoch
    /\ ~R("sweepOnlySuperseded") \/ ep # head.wal
    /\ walObj' = [walObj EXCEPT ![ep][seg] =
                    IF R("tombstoneOnSweep")
                    THEN <<[kind |-> "fence_tombstone", commits |-> <<>>]>>
                    ELSE <<>>]
    /\ UNCHANGED <<head, headExt, coldExt, inFlight, doneReqs, ses, told, large, pred, next, crashes>>

\* Cold-bucket collector (`cladefs-gc`): deletes unacknowledged large extents of superseded
\* epochs. By protectWalEpoch, it keeps extents of `head.wal` as well as `head.epoch`.
GcCold(obj) ==
    /\ "gc" \in Can
    /\ obj \in coldExt
    /\ obj \notin headExt
    /\ obj.ep < head.epoch
    /\ ~R("protectWalEpoch") \/ obj.ep # head.wal
    /\ coldExt' = coldExt \ {obj}
    /\ UNCHANGED <<head, headExt, walObj, inFlight, doneReqs, ses, told, large, pred, next, crashes>>

(***************************************************************************)
(* Compute host crash (stateless: loses all local session state)           *)
(***************************************************************************)

CrashHost(h) ==
    /\ crashes < MaxCrashes
    /\ ses[h] # NoSes
    /\ crashes' = crashes + 1
    /\ ses' = [ses EXCEPT ![h] = NoSes]
    /\ UNCHANGED <<head, headExt, coldExt, walObj, inFlight, doneReqs, told, large, pred, next>>

Next ==
    \/ \E h \in Hosts :
        \/ Take(h) \/ FenceStep(h) \/ RecoverAndFlush(h) \/ Activate(h)
        \/ Commit(h) \/ SendBatch(h) \/ SendRoll(h) \/ LearnFastWal(h)
        \/ ColdLand(h) \/ ColdAck(h) \/ CrashHost(h)
        \/ \E id \in Ids : PutLargeExtent(h, id)
        \/ \E seg \in Segs : DeleteWal(h, seg)
    \/ \E req \in inFlight : DeliverFastWal(req) \/ RejectFastWal(req)
    \/ \E ep \in Epochs, seg \in Segs : SweepWal(ep, seg)
    \/ \E obj \in coldExt : GcCold(obj)

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Hosts)

(***************************************************************************)
(* Invariants                                                              *)
(***************************************************************************)

\* Every commit acknowledged to the guest (FastWal or Uploaded) is in the history of the
\* serving writer named by head.epoch.
DurableSurvives ==
    \A h \in Hosts :
        (ses[h].st = "serving" /\ ses[h].ep = head.epoch) => told \subseteq Range(ses[h].log)

\* What the cold bucket's head holds is always a prefix of the serving writer's history.
HistoryIsPrefix ==
    \A h \in Hosts :
        (ses[h].st = "serving" /\ ses[h].ep = head.epoch) => IsPrefix(head.up, ses[h].log)

\* No commit appears without its predecessor in the serving writer's history (catches skip gaps).
NoGaps ==
    \A h \in Hosts :
        (ses[h].st = "serving" /\ ses[h].ep = head.epoch) =>
            \A i \in 1..Len(ses[h].log) :
                IF i = 1 THEN pred[ses[h].log[1]] = 0
                ELSE pred[ses[h].log[i]] = ses[h].log[i - 1]

\* Every large commit acknowledged to the guest has its large extent present in the cold
\* bucket under either an acknowledged head key, the active fast-WAL epoch, or head.epoch.
LargeExtentsPresent ==
    \A id \in told \cap large :
        \/ \E obj \in headExt : obj.c = id /\ obj \in coldExt
        \/ (head.wal > 0 /\ [ep |-> head.wal, c |-> id] \in coldExt)
        \/ (head.epoch > 0 /\ [ep |-> head.epoch, c |-> id] \in coldExt)

\* No open (non-rolled, non-fenced) WAL object ever fills to MaxParts, so a taker's Fence
\* append is never blocked by S3 Express's 10,000-part limit.
FenceHasRoom ==
    \A ep \in Epochs, seg \in Segs :
        LET obj == walObj[ep][seg] IN
        Len(obj) = MaxParts => (IsRolled(obj) \/ IsFenced(obj))

=============================================================================
