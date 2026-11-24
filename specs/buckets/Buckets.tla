------------------------------ MODULE Buckets ------------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Forks across buckets and their mirrors (backlog doc-23, TASK-633.2,     *)
(* TASK-633.8), cut down from specs/bucket's forks, references and         *)
(* collection: one parent cell, P, at home in bucket A, its versions       *)
(* forked into children at home in bucket B. Every step reads or writes    *)
(* one bucket's objects, each bucket linearizable alone; a bucket down     *)
(* refuses every request, and a step that needs it waits.                  *)
(*                                                                         *)
(* P's head, in A, names its versions. Its writer cuts new ones, each      *)
(* named once, and drops them; P is deleted, its head with it, and cuts    *)
(* none after. A version's state is its parts: objects, each written once  *)
(* under P's prefix in A, and shared across versions: a version cut names  *)
(* the writer's state, the last version's parts kept or new ones written.  *)
(* Once copied, a part is at the same key in B (doc-23, Mirrors).          *)
(*                                                                         *)
(* A fork of a version into a child in B: the child's record in B, being   *)
(* made; where its reference goes read in B, B if the version's mirror is  *)
(* marked there, else A; its reference written there; the head naming the  *)
(* version read in A, failing the fork if dropped; having referred in A,   *)
(* the marker read again in B, and if it is there now, a reference         *)
(* written in B; then the head read again in A, and if it still names the  *)
(* version, its own reference in A deleted; then the child made. A child   *)
(* made reads its version from B once the mirror's marker is there, from   *)
(* A before (the resolver). Forks read P's head in A: a fork into B waits  *)
(* for A, even once mirrored. Children are deleted.                        *)
(*                                                                         *)
(* A version's mirror into B starts at its first fork into B, in the       *)
(* background (the user, 2026-11-18): its pin, a reference of its own in   *)
(* B; its marker read in B, and if not there, its hold, a reference of its *)
(* own in A (<P>refs/<at>/mirror.<B>), create-only; A's head read, the     *)
(* mirror given up if its version is dropped or P deleted; the parts       *)
(* copied, A to B, the same bytes at the same keys, a part already in B    *)
(* (an earlier version's copy) taken as copied; the marker, create-only,   *)
(* the switch; the children referring in A listed, and their references    *)
(* written in B; A's head read again, and if it still names the version,   *)
(* theirs in A retired, marked; its pin dropped, its hold left for B's     *)
(* collector. A mirror's host may die at any step: it starts again from    *)
(* its pin, what it wrote kept, past its copy if its marker is there. One  *)
(* whose parts are gone from A and B before it copied them is given up:    *)
(* its hold deleted, its pin dropped, no marker. A mirror done may be      *)
(* begun again, as a new mirror, by a host that read no marker before the  *)
(* first marked: it finds the marker, and goes on to the listing.          *)
(*                                                                         *)
(* The collector runs passes, each over one cell in one bucket: P in A     *)
(* (its head read, what it names and the writer's state with it, then the  *)
(* references under P in A listed, then the parts that neither keeps       *)
(* deleted, of those there at the head's read); P's copy in B (A's head    *)
(* read, then B listed, its references, markers and parts; while P lives,  *)
(* or a reference in B stands, the whole copy kept; else the listed        *)
(* markers' versions' holds in A deleted, then the markers, then the       *)
(* listed parts); a child (its record read; deleted, its references        *)
(* retired, marked, in each bucket holding one, each once that bucket is   *)
(* up).                                                                    *)
(*                                                                         *)
(* Each rule is a member of Rules; specs/buckets/mutations removes one     *)
(* each, and must find the violation it prevents:                          *)
(*                                                                         *)
(* - forksReferFirst (NoForksReferFirst): a fork writes its reference      *)
(*   before it reads the head naming its version, as in one bucket.        *)
(* - forksRecheck (NoForksRecheck): a fork that referred in A reads the    *)
(*   marker again after, and refers in B if it is there.                   *)
(* - forksLeaveA (NoForksLeaveA): a fork that so referred in B deletes its *)
(*   reference in A after: no mirror retires it, and it would keep the     *)
(*   version in A as long as its child lives.                              *)
(* - forksRereadBeforeLeaving (NoForksRereadBeforeLeaving): a fork reads   *)
(*   A's head again after its reference in B, and deletes its own in A     *)
(*   only if the head still names its version: a pass of B that read P     *)
(*   deleted and listed B before that reference deletes the copy, its      *)
(*   child then reading A with no reference there (the review, TLC).       *)
(* - mirrorListsAfterMark (NoMirrorListsAfterMark): a mirror lists the     *)
(*   children referring in A after its marker: listed before, it misses a  *)
(*   fork that referred in A and read the head before the marker.          *)
(* - mirrorRereadsBeforeRetiring (NoMirrorRereadsBeforeRetiring): a        *)
(*   mirror reads A's head again after it refers its children in B, and    *)
(*   retires their references in A only if the head still names its        *)
(*   version, as forks do (forksRereadBeforeLeaving). Without it, a mirror *)
(*   begun again after a pass of B read P deleted and listed B finds its   *)
(*   marker, refers a forking child in B and retires its reference in A;   *)
(*   the pass deletes the marker, and the child, made, reads A, where      *)
(*   nothing keeps its version (TLC, 2026-11-19, NothingNeededGone).       *)
(* - mirrorPins (NoMirrorPins): a mirror holds its pin from before it      *)
(*   copies until it is done.                                              *)
(* - mirrorChecks (NoMirrorChecks): a mirror reads A's head after its pin, *)
(*   before it copies, and gives up if its version is dropped: a pass of B *)
(*   that read P deleted, then listed B, parts of an earlier version's     *)
(*   copy, before the pin, deletes the parts the mirror took as copied,    *)
(*   under the marker (TLC, 2026-11-18, 2026-11-19).                       *)
(* - mirrorResumesMarked (NoMirrorResumesMarked): a mirror started again   *)
(*   that finds its marker in B, once pinned, goes on to its listing: one  *)
(*   reading the head instead gives up if its version was dropped since,   *)
(*   its children left referring in A (TLC, 2026-11-18).                   *)
(* - mirrorHolds (NoMirrorHolds): a mirror writes its hold in A after its  *)
(*   pin, before it reads the head, and deletes it if it gives up; B's     *)
(*   collector deletes the holds of the versions whose markers it deletes, *)
(*   before them. A version's state may name objects of P's own origin     *)
(*   cell, which A keeps while P's cell is not removed, and A removes P    *)
(*   once no reference in A stands: the hold keeps P, for the copy's       *)
(*   readers in B, while the copy stands. That is outside the model (P's   *)
(*   cell's removal, its origin's objects); MarkedHeld says what it is     *)
(*   for, a marked copy's hold standing in A (board 8441).                 *)
(* - mirrorHoldsUnmarked (NoMirrorHoldsUnmarked): a mirror reads its       *)
(*   marker before its hold, and writes the hold only if the marker is not *)
(*   there. Writing it first, a mirror begun again after the first marked, *)
(*   P deleted, rewrites the hold after a pass of B that listed before its *)
(*   pin deleted the holds, then finds the marker, which that pass deletes *)
(*   next: the hold is left in A for good, and P's cell with it (TLC,      *)
(*   2026-11-19, HoldsMarked). One found marked needs none: the first      *)
(*   marking mirror's stands until B's collector deletes the marker.       *)
(* - markerAfterCopy (NoMarkerAfterCopy): a mirror writes its marker only  *)
(*   once it copied every part.                                            *)
(* - copiesKeptWhileAlive (NoCopiesKeptWhileAlive): B's collector keeps    *)
(*   the whole copy while P's head stands, or a reference in B: keeping    *)
(*   only the versions the head names, a pass that read the head before a  *)
(*   version cut, and listed parts an earlier version's copy left, deletes *)
(*   them under a mirror of the new version that took them as copied (the  *)
(*   review, TLC 2026-11-19).                                              *)
(* - markerGoesFirst (NoMarkerGoesFirst): B's collector deletes a copy's   *)
(*   marker before its parts.                                              *)
(* - copyRelists (NoCopyRelists): B's collector lists B after it reads A's *)
(*   head.                                                                 *)
(* - releasesOwn (NoReleasesOwn): a mirror deletes its hold and its pin,  *)
(*   letting go, only if each is still of its own write (mirror.rs's       *)
(*   Bucket::delete_if, board 9229): the next mirror's write of either,    *)
(*   its pin taken over, is of another version. Nothing bounds how long a  *)
(*   deletion waits in flight: its host stopped as it lets go, the next    *)
(*   mirror takes its pin over and writes the hold again, and a deletion   *)
(*   it sent lands after (LateLand, in BucketsLate and NoReleasesOwn, whose *)
(*   steps include "late"); unconditional, the next mirror marks           *)
(*   with no hold in A (TLC, 2026-11-22, MarkedHeld; board 8441). A        *)
(*   mirror gives up, deleting its hold, as it fails too (MirFail), its    *)
(*   version still named: the next mirror does not. A lease alone, its     *)
(*   deletions sent within it (board 8441), assumed each landed within the *)
(*   lease's other half, which nothing bounds.                             *)
(* - copyChecksDead (NoCopyChecksDead): B's collector retires a child's    *)
(*   reference in B when the child's head is deleted and holds no          *)
(*   references of its own (collector.rs's collect_copy, board 10446): a   *)
(*   dead child's own pass retires its reference in A and B once, marking  *)
(*   it in B, and skips the unchanged dead cell thereafter. When P is      *)
(*   deleted, a pass over P's copy deletes those marks in B so P's prefix  *)
(*   there goes empty; a late write of the child's reference in B (from a  *)
(*   fork paused before its create or a mirror paused between listing A    *)
(*   and writing B) then lands with no mark in B, and without this rule    *)
(*   every later pass over the copy sees the child's unremoved head and    *)
(*   keeps the reference (and the copy) in B for good (NoDeadRefKept).     *)
(*                                                                         *)
(* TLC found these needed no rule (2026-11-18): a fork referring in B at   *)
(* once when marked (the read again covers it), and a mirror listing the   *)
(* children before its copy too (its listing after the marker covers it).  *)
(*                                                                         *)
(* Not modelled: the root bucket and its registry (TASK-633.9's tests);    *)
(* P's own collection beyond its versions and its writer's state (which    *)
(* specs/bucket checks: a pass of A here deletes only parts there at its   *)
(* head's read that neither its versions nor the writer's state named);    *)
(* P's cell's removal and its origin's objects (mirrorHolds); writers'     *)
(* relocations (doc-27: a version's state is immutable, a copy takes the   *)
(* segments it reads, wherever its writer relocated since); the time a     *)
(* lease lasts (a mirror stopped past its lease is one restarted, the      *)
(* next taking its pin over, releasesOwn); the children's own data.        *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Children,       \* the children's cells, at home in B
    Versions,       \* P's versions' names, 1..N, cut in order: 1 first
    Objects,        \* the objects P's versions' parts may be
    FirstParts,     \* version 1's parts
    MaxPasses,      \* collector passes
    MaxDowns,       \* buckets going down (each comes back)
    MaxRestarts,    \* mirrors' hosts dying, or mirrors begun again once done
    Can,            \* the kinds of step the model takes
    None,           \* a model value: nothing
    Rules           \* the rules in force (all, but in a mutation)

AllRules == {"forksReferFirst", "forksRecheck", "forksLeaveA", "forksRereadBeforeLeaving",
             "mirrorListsAfterMark", "mirrorRereadsBeforeRetiring", "mirrorPins", "mirrorChecks",
             "mirrorResumesMarked", "mirrorHolds", "mirrorHoldsUnmarked", "markerAfterCopy",
             "copiesKeptWhileAlive", "markerGoesFirst", "copyRelists", "releasesOwn",
             "copyChecksDead"}

R(rule) == rule \in Rules

Buckets == {"A", "B"}

\* The mirror's pin of version v: a reference of its own, in B.
Pin(v) == <<v, "mirror">>

\* The mirror's hold of version v: a reference of its own, in A.
Hold(v) == <<v, "hold">>

VARIABLES
    vers,       \* the versions P's head, in A, names
    cut,        \* the versions P's writer cut, named or dropped since
    parts,      \* parts[v]: the objects v's state names, {} until v is cut
    ended,      \* P deleted: its head, and every version with it
    objs,       \* objs[b]: the objects in bucket b
    rec,        \* rec[c]: the child's record, in B: None, or [v, kind, deleted]
    refs,       \* refs[b]: the references under P in bucket b: <<version, child>>, a pin, a hold
    marked,     \* marked[b]: the references retired in bucket b, marked
    marker,     \* the versions whose mirrors are marked in B
    fork,       \* fork[c]: the fork making c: None, or [pc, v, at]
    mir,        \* mir[v]: v's mirror into B: [pc, kids, copied, run], and the runs whose writes its
                \* pin and hold are (pinBy, holdBy)
    gc,         \* gc[i]: pass i
    down,       \* the buckets down
    passes, downs, restarts,
    late        \* deletions a stopped mirror sent, landing later: <<bucket, ref, its run>>

vars == <<vers, cut, parts, ended, objs, rec, refs, marked, marker, fork, mir, gc, down, passes,
          downs, restarts, late>>

Up(b) == b \notin down

\* The objects the versions in S name.
PartsOf(S) == UNION {parts[v] : v \in S}

\* The writer's state: the last version cut's parts, until P is deleted.
Last == CHOOSE v \in cut : \A w \in cut : w <= v
Writing == IF ended THEN {} ELSE parts[Last]

\* Objects no version cut names: never written.
Fresh == Objects \ PartsOf(cut)

\* Where a child reads its version v from: B once its mirror is marked there.
Src(v) == IF v \in marker THEN "B" ELSE "A"

Live(c) == rec[c] # None /\ rec[c].kind = "fork" /\ ~rec[c].deleted

\* The versions references in Rs name.
Named(Rs) == {r[1] : r \in Rs}

\* The children referring in A to version v.
ReferringInA(v) == {c \in Children : <<v, c>> \in refs["A"]}

GcIdle == [pc |-> "idle", kind |-> None, c |-> None, alive |-> FALSE, vs |-> {}, rs |-> {},
           ms |-> {}, os |-> {}, mk |-> {}, missed |-> FALSE]

MirIdle == [pc |-> "idle", kids |-> {}, copied |-> {}, run |-> 0, pinBy |-> 0, holdBy |-> 0]

\* Whether the model lets a stopped mirror's deletions land late ("late"): it tells the runs of v's
\* mirror apart, and whose writes its pin and hold are, only then, its other models none the
\* larger for them (BucketsLate checks it, and NoReleasesOwn).
Late == "late" \in Can

\* Whether the model tracks a dead child's one-shot pass (passed), splits a mirror's list and
\* write in B (MirList/MirRefer), lets a forking child be deleted, and deletes B's retired marks
\* with the copy ("dead"): its other models are none the larger for them (BucketsDead checks it,
\* and NoCopyChecksDead; board 10446).
Dead == "dead" \in Can

\* v's mirror begun again, as its run `run`: what its pin and hold are of kept.
MirAnew(v, run) ==
    [mir[v] EXCEPT !.pc = "pin", !.kids = {}, !.copied = {}, !.run = IF Late THEN run ELSE 0]

\* P's head and writer.
UNCH == <<vers, cut, parts, ended>>

-----------------------------------------------------------------------------
(* P's writer. *)

\* A version cut, the next not named before: its parts, the writer's state kept in part, objects
\* written for it in A.
CutVersion(S) ==
    LET v == CHOOSE w \in Versions \ cut : \A u \in Versions \ cut : w <= u IN
    /\ "cut" \in Can /\ ~ended /\ Up("A") /\ cut # Versions
    /\ S # {} /\ S \subseteq Writing \cup Fresh
    /\ vers' = vers \cup {v}
    /\ cut' = cut \cup {v}
    /\ parts' = [parts EXCEPT ![v] = S]
    /\ objs' = [objs EXCEPT !["A"] = @ \cup S]
    /\ UNCHANGED <<ended, rec, refs, marked, marker, fork, mir, gc, down, passes, downs, restarts,
                   late>>

DropVersion(v) ==
    /\ "drop" \in Can /\ v \in vers /\ Up("A")
    /\ vers' = vers \ {v}
    /\ UNCHANGED <<cut, parts, ended, objs, rec, refs, marked, marker, fork, mir, gc, down, passes,
                   downs, restarts, late>>

\* P deleted: its head, naming no version, and no writer.
EndP ==
    /\ "end" \in Can /\ ~ended /\ Up("A")
    /\ ended' = TRUE
    /\ vers' = {}
    /\ UNCHANGED <<cut, parts, objs, rec, refs, marked, marker, fork, mir, gc, down, passes, downs,
                   restarts, late>>

-----------------------------------------------------------------------------
(* Forks into B. *)

\* A fork of v to c, begun: v named by P's head, read in A. The first fork of v into B starts its
\* mirror.
StartFork(c, v) ==
    /\ "fork" \in Can /\ rec[c] = None /\ fork[c] = None
    /\ Up("A") /\ v \in vers
    /\ fork' = [fork EXCEPT ![c] = [pc |-> "record", v |-> v, at |-> {}]]
    /\ mir' = IF "mirror" \in Can /\ mir[v].pc = "idle"
              THEN [mir EXCEPT ![v].pc = "pin"] ELSE mir
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, gc, down, passes, downs, restarts, late>>

\* Its record, in B, being made.
ForkRecord(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "record" /\ Up("B")
    /\ rec' = [rec EXCEPT ![c] = [v |-> q.v, kind |-> "forking", deleted |-> FALSE,
                                  passed |-> FALSE]]
    /\ fork' = [fork EXCEPT ![c].pc = IF R("forksReferFirst") THEN "where" ELSE "check"]
    /\ UNCHANGED <<UNCH, objs, refs, marked, marker, mir, gc, down, passes, downs, restarts, late>>

\* Where its reference goes, read in B: there if its version's mirror is marked, else in A.
ForkWhere(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "where" /\ Up("B")
    /\ fork' = [fork EXCEPT ![c].pc = "refer",
                            ![c].at = {IF q.v \in marker THEN "B" ELSE "A"}]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, mir, gc, down, passes, downs, restarts,
                   late>>

\* Its reference written there: a request after the marker's read.
ForkRefer(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "refer" /\ \A b \in q.at : Up(b)
    /\ refs' = [b \in Buckets |-> IF b \in q.at THEN refs[b] \cup {<<q.v, c>>} ELSE refs[b]]
    /\ fork' = [fork EXCEPT ![c].pc = IF R("forksReferFirst") THEN "check" ELSE "make"]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, mir, gc, down, passes, downs, restarts, late>>

\* The head naming its version read, in A: dropped, the fork fails, its child deleted. Having
\* referred in A, on to the marker's read again (forksRecheck).
ForkCheck(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "check" /\ Up("A")
    /\ IF q.v \notin vers THEN
          /\ rec' = [rec EXCEPT ![c].deleted = TRUE]
          /\ fork' = [fork EXCEPT ![c] = None]
       ELSE
          /\ fork' = [fork EXCEPT ![c].pc = CASE ~R("forksReferFirst") -> "where"
                                               [] R("forksRecheck") /\ q.at = {"A"} -> "recheck"
                                               [] OTHER -> "make"]
          /\ UNCHANGED rec
    /\ UNCHANGED <<UNCH, objs, refs, marked, marker, mir, gc, down, passes, downs, restarts, late>>

\* The marker read again, in B: there now, on to a reference in B.
ForkRecheck(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "recheck" /\ Up("B")
    /\ fork' = [fork EXCEPT ![c].pc = IF q.v \in marker THEN "referB" ELSE "make"]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, mir, gc, down, passes, downs, restarts,
                   late>>

\* Its reference written in B; then, to leave A (forksLeaveA).
ForkReferB(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "referB" /\ Up("B")
    /\ refs' = [refs EXCEPT !["B"] = @ \cup {<<q.v, c>>}]
    /\ fork' = [fork EXCEPT ![c].pc = IF R("forksLeaveA") THEN "leave" ELSE "make"]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, mir, gc, down, passes, downs, restarts, late>>

\* A's head read again, in A (forksRereadBeforeLeaving): naming its version still, P alive, its
\* reference in A deleted; else it stays.
ForkLeave(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "leave" /\ Up("A")
    /\ refs' = IF ~R("forksRereadBeforeLeaving") \/ q.v \in vers
               THEN [refs EXCEPT !["A"] = @ \ {<<q.v, c>>}] ELSE refs
    /\ fork' = [fork EXCEPT ![c].pc = "make"]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, mir, gc, down, passes, downs, restarts, late>>

\* The child made, in B (refused if deleted meanwhile, lifecycle.rs).
ForkMake(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "make" /\ Up("B")
    /\ rec' = IF rec[c].deleted THEN rec ELSE [rec EXCEPT ![c].kind = "fork"]
    /\ fork' = [fork EXCEPT ![c] = None]
    /\ UNCHANGED <<UNCH, objs, refs, marked, marker, mir, gc, down, passes, downs, restarts, late>>

DeleteChild(c) ==
    /\ "delete" \in Can /\ Up("B")
    /\ IF Dead THEN rec[c] # None /\ ~rec[c].deleted ELSE Live(c)
    /\ rec' = [rec EXCEPT ![c].deleted = TRUE]
    /\ UNCHANGED <<UNCH, objs, refs, marked, marker, fork, mir, gc, down, passes, downs, restarts,
                   late>>

-----------------------------------------------------------------------------
(* Mirrors into B. *)

\* Where a mirror giving up goes: its hold deleted (mirrorHolds), then its pin dropped.
GiveUp == IF R("mirrorHolds") THEN "unhold" ELSE "unpin"

\* Where a mirror goes once it knows whether its marker is in B: found, started again after it, on
\* to its listing (mirrorResumesMarked); else to A's head (mirrorChecks), or its copy.
Resume(v) ==
    CASE v \in marker /\ R("mirrorResumesMarked") ->
            IF R("mirrorListsAfterMark") THEN "list" ELSE "retire"
      [] R("mirrorChecks") -> "check"
      [] OTHER -> "copy"

\* Its pin, in B (mirrorPins). Then its hold in A (mirrorHolds), and its marker read in B: the
\* marker read first, the hold written only if it is not there (mirrorHoldsUnmarked), as one found
\* marked has a hold standing, the first marking mirror's, until B's collector deletes it with the
\* marker; without the rule, the hold written first, then the marker read (MirRead). Without
\* mirrorListsAfterMark, the children referring in A listed now, in A, and referred in B with it.
MirPin(v) ==
    LET early == IF R("mirrorListsAfterMark") THEN {} ELSE ReferringInA(v)
        pin == IF R("mirrorPins") THEN {Pin(v)} ELSE {}
    IN /\ mir[v].pc = "pin" /\ Up("B") /\ (~R("mirrorListsAfterMark") => Up("A"))
       /\ refs' = [refs EXCEPT !["B"] = @ \cup pin \cup {<<v, c>> : c \in early}]
       /\ mir' = [mir EXCEPT ![v] =
                    [@ EXCEPT !.pc = CASE ~R("mirrorHolds") -> Resume(v)
                                       [] ~R("mirrorHoldsUnmarked") -> "hold"
                                       [] v \in marker /\ R("mirrorResumesMarked") -> Resume(v)
                                       [] OTHER -> "hold",
                              !.kids = early,
                              !.pinBy = IF R("mirrorPins") /\ Late THEN mir[v].run ELSE @]]
       /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts,
                      late>>

\* Its hold written in A, before it reads the head (mirrorHolds): of its own bytes, over any there,
\* a write of its own run's.
MirHold(v) ==
    /\ mir[v].pc = "hold" /\ Up("A")
    /\ refs' = [refs EXCEPT !["A"] = @ \cup {Hold(v)}]
    /\ mir' = [mir EXCEPT ![v] =
                [@ EXCEPT !.pc = IF R("mirrorHoldsUnmarked") THEN Resume(v) ELSE "read",
                          !.holdBy = IF Late THEN mir[v].run ELSE @]]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts, late>>

\* Without mirrorHoldsUnmarked, its marker read in B after its hold.
MirRead(v) ==
    /\ mir[v].pc = "read" /\ Up("B")
    /\ mir' = [mir EXCEPT ![v].pc = Resume(v)]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts,
                   late>>

\* A's head read after the pin (mirrorChecks): the version dropped, or P deleted, the mirror given
\* up.
MirCheck(v) ==
    /\ mir[v].pc = "check" /\ Up("A")
    /\ mir' = [mir EXCEPT ![v].pc = IF v \in vers THEN "copy" ELSE GiveUp]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts,
                   late>>

\* A part copied, A to B, the same bytes at the same key: or, there already in B, taken as copied.
MirCopy(v, o) ==
    /\ mir[v].pc = "copy" /\ o \in parts[v] \ mir[v].copied /\ Up("B")
    /\ IF o \in objs["B"] THEN UNCHANGED objs
       ELSE /\ Up("A") /\ o \in objs["A"]
            /\ objs' = [objs EXCEPT !["B"] = @ \cup {o}]
    /\ mir' = [mir EXCEPT ![v].copied = @ \cup {o}]
    /\ UNCHANGED <<UNCH, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts, late>>

\* A part to copy gone from B and A: the mirror given up.
MirGiveUp(v) ==
    /\ mir[v].pc = "copy" /\ Up("A") /\ Up("B")
    /\ \E o \in parts[v] \ mir[v].copied : o \notin objs["B"] /\ o \notin objs["A"]
    /\ mir' = [mir EXCEPT ![v].pc = GiveUp]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts,
                   late>>

\* The marker, create-only: the switch, once every part is copied (markerAfterCopy).
MirMark(v) ==
    /\ mir[v].pc = "copy" /\ Up("B")
    /\ R("markerAfterCopy") => mir[v].copied = parts[v]
    /\ marker' = marker \cup {v}
    /\ mir' = [mir EXCEPT ![v].pc = IF R("mirrorListsAfterMark") THEN "list" ELSE "retire"]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, fork, gc, down, passes, downs, restarts, late>>

\* The children referring in A listed, after the marker (mirrorListsAfterMark); under "dead",
\* split from writing their references in B (MirRefer) so a dead child's pass may interleave.
MirList(v) ==
    LET kids == ReferringInA(v) IN
    /\ mir[v].pc = "list" /\ Up("A") /\ (~Dead => Up("B"))
    /\ refs' = IF Dead THEN refs ELSE [refs EXCEPT !["B"] = @ \cup {<<v, c>> : c \in kids}]
    /\ mir' = [mir EXCEPT ![v] =
                 [@ EXCEPT !.pc = IF ~Dead THEN "retire"
                                  ELSE IF kids = {} THEN "unpin" ELSE "refer",
                           !.kids = kids]]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts, late>>

\* The listed children's references written in B ("dead").
MirRefer(v) ==
    /\ Dead /\ mir[v].pc = "refer" /\ Up("B")
    /\ refs' = [refs EXCEPT !["B"] = @ \cup {<<v, c>> : c \in mir[v].kids}]
    /\ mir' = [mir EXCEPT ![v].pc = "retire"]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts, late>>

\* A's head read again (mirrorRereadsBeforeRetiring): naming its version still, P alive, their
\* references in A retired, marked; else they stay.
MirRetire(v) ==
    LET Rs == IF ~R("mirrorRereadsBeforeRetiring") \/ v \in vers
              THEN {<<v, c>> : c \in mir[v].kids} ELSE {}
    IN /\ mir[v].pc = "retire" /\ Up("A")
       /\ refs' = [refs EXCEPT !["A"] = @ \ Rs]
       /\ marked' = [marked EXCEPT !["A"] = @ \cup (Rs \cap refs["A"])]
       /\ mir' = [mir EXCEPT ![v].pc = "unpin"]
       /\ UNCHANGED <<UNCH, objs, rec, marker, fork, gc, down, passes, downs, restarts, late>>

\* Given up, its hold deleted in A (mirrorHolds).
MirUnhold(v) ==
    /\ mir[v].pc = "unhold" /\ Up("A")
    /\ refs' = [refs EXCEPT !["A"] = @ \ {Hold(v)}]
    /\ mir' = [mir EXCEPT ![v].pc = "unpin"]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts, late>>

\* Its pin dropped: done.
MirUnpin(v) ==
    /\ mir[v].pc = "unpin" /\ Up("B")
    /\ refs' = [refs EXCEPT !["B"] = @ \ {Pin(v)}]
    /\ mir' = [mir EXCEPT ![v].pc = "done"]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts, late>>

\* A mirror begun again once done ("again"): by a host that asked for it, having read no marker
\* before the first mirror marked (or none since B's collector deleted it), or by B's collector
\* resuming a pin gone stale (not modelled), on a host of its own.
MirAgain(v) ==
    /\ "again" \in Can /\ restarts < MaxRestarts /\ mir[v].pc = "done"
    /\ mir' = [mir EXCEPT ![v] = MirAnew(v, restarts + 1)]
    /\ restarts' = restarts + 1
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, gc, down, passes, downs, late>>

\* Failing, as a request it can't go on past does (an error), it gives up: its hold deleted, its pin
\* dropped (mirror.rs's let_go), as it would begun again, its version named still.
MirFail(v) ==
    /\ "fail" \in Can /\ mir[v].pc \in {"hold", "check", "copy"}
    /\ mir' = [mir EXCEPT ![v].pc = GiveUp]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts,
                   late>>

\* The requests a stopped mirror may have in flight: writes of references to B (run = None), and
\* deletions of its hold and its pin, each with the run that sent it.
Releasing(v, pc) ==
    LET run == mir[v].run IN
    CASE pc = "refer" -> {<<"B", <<v, c>>, None>> : c \in mir[v].kids}
      [] pc = "unhold" -> {<<"A", Hold(v), run>>, <<"B", Pin(v), run>>}
      [] pc = "unpin" -> {<<"B", Pin(v), run>>}
      [] OTHER -> {}

\* Its host dies, or stops past its lease: it starts again from its pin, as a run of its own, what
\* it wrote kept, the next mirror taking its pin over once stale. Stopped as it writes references
\* or lets go, its requests in flight land later (LateLand), after the next mirror or a pass over
\* the copy ran; in a model without "late", each landed before it stopped, or was never sent.
MirRestart(v) ==
    /\ "crash" \in Can /\ restarts < MaxRestarts
    /\ mir[v].pc \notin {"idle", "pin", "done"}
    /\ mir' = [mir EXCEPT ![v] = MirAnew(v, restarts + 1)]
    /\ restarts' = restarts + 1
    /\ late' = IF Late THEN late \cup Releasing(v, mir[v].pc) ELSE late
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, gc, down, passes, downs>>

\* The run whose write the reference r, a pin or a hold, is.
WrittenBy(r) == IF r[2] = "mirror" THEN mir[r[1]].pinBy ELSE mir[r[1]].holdBy

\* A request a stopped mirror sent, landing: a late create (l[3] = None) creates the reference in
\* its bucket; a late deletion, conditional on its run's write (releasesOwn), deletes another run's
\* write of the reference not.
LateLand(l) ==
    LET b == l[1] IN
    /\ l \in late /\ Up(b)
    /\ IF l[3] = None
       THEN refs' = [refs EXCEPT ![b] = @ \cup {l[2]}]
       ELSE LET own == ~R("releasesOwn") \/ WrittenBy(l[2]) = l[3] IN
            refs' = IF own THEN [refs EXCEPT ![b] = @ \ {l[2]}] ELSE refs
    /\ late' = late \ {l}
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, mir, gc, down, passes, downs, restarts>>

-----------------------------------------------------------------------------
(* The collector. *)

\* Whether the model takes passes of this kind: "collect" every kind, else "collectP", "collectCopy"
\* or "collectChild" that kind alone.
Collects(kind) ==
    \/ "collect" \in Can
    \/ CASE kind = "P" -> "collectP" \in Can
          [] kind = "copy" -> "collectCopy" \in Can
          [] kind = "child" -> "collectChild" \in Can

\* Whether a reference r in B is gone as B's collector checks it: marked retired in B, its child
\* gone or of another origin, or (copyChecksDead) its child deleted and holding no references.
IsGone(r, mk) ==
    /\ r[2] \in Children
    /\ \/ r \in mk
       \/ rec[r[2]] = None
       \/ rec[r[2]].v # r[1]
       \/ (R("copyChecksDead") /\ rec[r[2]].deleted)

\* Of a pass over P's copy, the markers and the parts to go: none while P lives or a reference in B
\* stands, a pin or a child's (copiesKeptWhileAlive), else all it listed; without the rule, those
\* of the versions neither the head nor a reference names.
CopyKept(g) == IF R("copiesKeptWhileAlive") THEN g.alive \/ g.rs # {} ELSE FALSE
GoingMarkers(g) ==
    IF R("copiesKeptWhileAlive") THEN IF CopyKept(g) THEN {} ELSE g.ms
    ELSE g.ms \ (g.vs \cup Named(g.rs))
GoingParts(g) ==
    IF R("copiesKeptWhileAlive") THEN IF CopyKept(g) THEN {} ELSE g.os
    ELSE g.os \ PartsOf(g.vs \cup Named(g.rs))

\* A pass over the copy, its reads done: on to its deletions, if any, its holds first
\* (mirrorHolds); else ended (preserving g.missed so NoDeadRefKept sees a missed dead child).
CopyNext(g) ==
    IF GoingMarkers(g) = {} /\ GoingParts(g) = {} /\ (CopyKept(g) \/ g.mk = {})
    THEN [GcIdle EXCEPT !.missed = g.missed]
    ELSE [g EXCEPT !.pc = CASE GoingMarkers(g) # {} /\ R("mirrorHolds") -> "unhold"
                            [] GoingMarkers(g) # {} \/ GoingParts(g) # {} -> "unmark"
                            [] OTHER -> "delete"]

\* A pass begun: over P in A, its head read, and with it what neither its versions nor the writer's
\* state name of the parts there; over P's copy in B, A's head read (copyRelists), or, without the
\* rule, B listed first; over a child, its record read: deleted (and under "dead", not yet passed).
GcStart(kind, c) ==
    LET i == passes + 1
        needs == CASE kind = "P" -> "A"
                   [] kind = "copy" -> IF R("copyRelists") THEN "A" ELSE "B"
                   [] kind = "child" -> "B"
        gone == IF Dead THEN {r \in refs["B"] : IsGone(r, marked["B"])} ELSE {}
        standing == refs["B"] \ gone
        missed == Dead /\ \E r \in standing :
                    r[2] \in Children /\ rec[r[2]] # None /\ rec[r[2]].passed
    IN /\ Collects(kind) /\ passes < MaxPasses /\ Up(needs)
       /\ (kind = "child") = (c # None)
       /\ kind = "child" =>
            /\ rec[c] # None /\ rec[c].deleted
            /\ Dead => /\ ~rec[c].passed
                       /\ ~\E j \in 1..MaxPasses : gc[j].kind = "child" /\ gc[j].c = c
       /\ gc' = [gc EXCEPT ![i] =
                   CASE kind = "P" ->
                          [GcIdle EXCEPT !.pc = "list", !.kind = kind,
                                         !.os = objs["A"] \ (PartsOf(vers) \cup Writing)]
                     [] kind = "copy" ->
                          IF R("copyRelists")
                          THEN [GcIdle EXCEPT !.pc = "list", !.kind = kind, !.alive = ~ended,
                                              !.vs = vers]
                          ELSE [GcIdle EXCEPT !.pc = "list", !.kind = kind, !.rs = standing,
                                              !.ms = marker, !.os = objs["B"],
                                              !.mk = IF Dead THEN marked["B"] \cup gone ELSE {},
                                              !.missed = missed]
                     [] kind = "child" ->
                          IF Dead
                          THEN [GcIdle EXCEPT !.pc = "retireA", !.kind = kind, !.c = c]
                          ELSE [GcIdle EXCEPT !.pc = "retire", !.kind = kind, !.c = c,
                                              !.alive = ~(rec[c] # None /\ rec[c].deleted)]]
       /\ IF Dead /\ kind = "copy" /\ ~R("copyRelists")
          THEN /\ refs' = [refs EXCEPT !["B"] = @ \ gone]
               /\ marked' = [marked EXCEPT !["B"] = @ \cup gone]
          ELSE UNCHANGED <<refs, marked>>
       /\ passes' = i
       /\ UNCHANGED <<UNCH, objs, rec, marker, fork, mir, down, downs, restarts, late>>

\* The second read: of P in A, the references under P in A; of P's copy, B listed after A's head
\* (copyRelists), retiring gone references in B under "dead", or A's head after it.
GcList(i) ==
    LET g == gc[i]
        b == IF g.kind = "P" \/ (g.kind = "copy" /\ ~R("copyRelists")) THEN "A" ELSE "B"
        gone == IF Dead THEN {r \in refs["B"] : IsGone(r, marked["B"])} ELSE {}
        standing == refs["B"] \ gone
        missed == Dead /\ \E r \in standing :
                    r[2] \in Children /\ rec[r[2]] # None /\ rec[r[2]].passed
    IN /\ g.pc = "list" /\ Up(b)
       /\ gc' = [gc EXCEPT ![i] =
                   CASE g.kind = "P" -> [g EXCEPT !.pc = "delete", !.rs = refs["A"]]
                     [] R("copyRelists") ->
                          CopyNext([g EXCEPT !.rs = standing, !.ms = marker, !.os = objs["B"],
                                             !.mk = IF Dead THEN marked["B"] \cup gone ELSE {},
                                             !.missed = missed])
                     [] OTHER -> CopyNext([g EXCEPT !.alive = ~ended, !.vs = vers])]
       /\ IF Dead /\ g.kind = "copy" /\ R("copyRelists")
          THEN /\ refs' = [refs EXCEPT !["B"] = @ \ gone]
               /\ marked' = [marked EXCEPT !["B"] = @ \cup gone]
          ELSE UNCHANGED <<refs, marked>>
       /\ UNCHANGED <<UNCH, objs, rec, marker, fork, mir, down, passes, downs, restarts, late>>

\* Of P's copy in B, the holds in A of the versions whose markers go deleted (mirrorHolds).
GcUnhold(i) ==
    LET g == gc[i] IN
    /\ g.pc = "unhold" /\ Up("A")
    /\ refs' = [refs EXCEPT !["A"] = @ \ {Hold(v) : v \in GoingMarkers(g)}]
    /\ gc' = [gc EXCEPT ![i].pc = "unmark"]
    /\ UNCHANGED <<UNCH, objs, rec, marked, marker, fork, mir, down, passes, downs, restarts, late>>

\* Of P's copy in B: the markers going deleted first (markerGoesFirst); without the rule, the parts
\* first.
GcUnmark(i) ==
    LET g == gc[i] IN
    /\ g.pc = "unmark" /\ Up("B")
    /\ IF R("markerGoesFirst")
       THEN marker' = marker \ GoingMarkers(g) /\ UNCHANGED objs
       ELSE objs' = [objs EXCEPT !["B"] = @ \ GoingParts(g)] /\ UNCHANGED marker
    /\ gc' = [gc EXCEPT ![i].pc = "delete"]
    /\ UNCHANGED <<UNCH, rec, refs, marked, fork, mir, down, passes, downs, restarts, late>>

\* The parts going deleted: of P in A, those neither its head nor a reference named; of its copy in
\* B, after the markers (or, without markerGoesFirst, the markers after the parts), and under
\* "dead" the marks of references retired in B once the copy goes whole (collector.rs:494).
GcDelete(i) ==
    LET g == gc[i] IN
    /\ g.pc = "delete" /\ Up(IF g.kind = "P" THEN "A" ELSE "B")
    /\ IF g.kind = "P"
       THEN /\ objs' = [objs EXCEPT !["A"] = @ \ (g.os \ PartsOf(Named(g.rs)))]
            /\ UNCHANGED <<marker, marked>>
       ELSE /\ IF R("markerGoesFirst")
               THEN objs' = [objs EXCEPT !["B"] = @ \ GoingParts(g)] /\ UNCHANGED marker
               ELSE marker' = marker \ GoingMarkers(g) /\ UNCHANGED objs
            /\ marked' = IF Dead /\ ~CopyKept(g)
                         THEN [marked EXCEPT !["B"] = @ \ g.mk] ELSE marked
    /\ gc' = [gc EXCEPT ![i] = [GcIdle EXCEPT !.missed = g.missed]]
    /\ UNCHANGED <<UNCH, rec, refs, fork, mir, down, passes, downs, restarts, late>>

\* Without "dead": a deleted child's references retired, marked, in bucket b, if it holds one.
GcRetire(i, b) ==
    LET g == gc[i]
        Rs == {r \in refs[b] : r[2] = g.c}
    IN /\ ~Dead /\ g.pc = "retire" /\ Up(b) /\ ~g.alive /\ Rs # {}
       /\ refs' = [refs EXCEPT ![b] = @ \ Rs]
       /\ marked' = [marked EXCEPT ![b] = @ \cup Rs]
       /\ UNCHANGED <<UNCH, objs, rec, marker, fork, mir, gc, down, passes, downs, restarts, late>>

GcRetired(i) ==
    LET g == gc[i] IN
    /\ ~Dead /\ g.pc = "retire"
    /\ ~g.alive => \A b \in Buckets : ~\E r \in refs[b] : r[2] = g.c
    /\ gc' = [gc EXCEPT ![i] = GcIdle]
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, mir, down, passes, downs,
                   restarts, late>>

\* Under "dead": a deleted child's reference retired, marked, in A first, then in B (whether
\* present in B or not, as Here::retire does); after B, its dead cell's digest is recorded so later
\* passes skip it.
GcRetireA(i) ==
    LET g == gc[i]
        r == <<rec[g.c].v, g.c>>
    IN /\ Dead /\ g.pc = "retireA" /\ Up("A")
       /\ refs' = [refs EXCEPT !["A"] = @ \ {r}]
       /\ marked' = [marked EXCEPT !["A"] = @ \cup {r}]
       /\ gc' = [gc EXCEPT ![i].pc = "retireB"]
       /\ UNCHANGED <<UNCH, objs, rec, marker, fork, mir, down, passes, downs, restarts, late>>

GcRetireB(i) ==
    LET g == gc[i]
        r == <<rec[g.c].v, g.c>>
    IN /\ Dead /\ g.pc = "retireB" /\ Up("B")
       /\ refs' = [refs EXCEPT !["B"] = @ \ {r}]
       /\ marked' = [marked EXCEPT !["B"] = @ \cup {r}]
       /\ rec' = [rec EXCEPT ![g.c].passed = TRUE]
       /\ gc' = [gc EXCEPT ![i] = [GcIdle EXCEPT !.missed = g.missed]]
       /\ UNCHANGED <<UNCH, objs, marker, fork, mir, down, passes, downs, restarts, late>>

-----------------------------------------------------------------------------
(* Buckets going down, and coming back. *)

GoDown(b) ==
    /\ "down" \in Can /\ downs < MaxDowns /\ Up(b)
    /\ down' = down \cup {b}
    /\ downs' = downs + 1
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, mir, gc, passes, restarts, late>>

ComeBack(b) ==
    /\ b \in down
    /\ down' = down \ {b}
    /\ UNCHANGED <<UNCH, objs, rec, refs, marked, marker, fork, mir, gc, passes, downs, restarts,
                   late>>

-----------------------------------------------------------------------------

Init ==
    /\ vers = {1}
    /\ cut = {1}
    /\ parts = [v \in Versions |-> IF v = 1 THEN FirstParts ELSE {}]
    /\ ended = FALSE
    /\ objs = [b \in Buckets |-> IF b = "A" THEN FirstParts ELSE {}]
    /\ rec = [c \in Children |-> None]
    /\ refs = [b \in Buckets |-> {}]
    /\ marked = [b \in Buckets |-> {}]
    /\ marker = {}
    /\ fork = [c \in Children |-> None]
    /\ mir = [v \in Versions |-> MirIdle]
    /\ gc = [i \in 1..MaxPasses |-> GcIdle]
    /\ down = {}
    /\ passes = 0 /\ downs = 0 /\ restarts = 0
    /\ late = {}

ForkStep(c) ==
    \/ ForkRecord(c) \/ ForkWhere(c) \/ ForkRefer(c) \/ ForkCheck(c) \/ ForkRecheck(c)
    \/ ForkReferB(c) \/ ForkLeave(c) \/ ForkMake(c)

MirStep(v) ==
    \/ MirPin(v) \/ MirHold(v) \/ MirRead(v) \/ MirCheck(v) \/ MirGiveUp(v) \/ MirMark(v)
    \/ MirList(v) \/ MirRefer(v) \/ MirRetire(v) \/ MirUnhold(v) \/ MirUnpin(v)
    \/ \E o \in Objects : MirCopy(v, o)

GcStep(i) ==
    \/ GcList(i) \/ GcUnhold(i) \/ GcUnmark(i) \/ GcDelete(i)
    \/ GcRetired(i) \/ \E b \in Buckets : GcRetire(i, b)
    \/ GcRetireA(i) \/ GcRetireB(i)

Next ==
    \/ \E S \in SUBSET Objects : CutVersion(S)
    \/ EndP
    \/ \E v \in Versions : DropVersion(v) \/ MirStep(v) \/ MirRestart(v)
                           \/ MirAgain(v) \/ MirFail(v)
    \/ \E l \in late : LateLand(l)
    \/ \E c \in Children : ForkStep(c) \/ DeleteChild(c) \/ GcStart("child", c)
                           \/ \E v \in Versions : StartFork(c, v)
    \/ \E kind \in {"P", "copy"} : GcStart(kind, None)
    \/ \E i \in 1..MaxPasses : GcStep(i)
    \/ \E b \in Buckets : GoDown(b) \/ ComeBack(b)

Spec == Init /\ [][Next]_vars

\* With fairness, for liveness: every fork, mirror and pass under way goes on whenever it can;
\* buckets come back.
Fair ==
    /\ \A c \in Children : WF_vars(ForkStep(c))
    /\ \A v \in Versions : WF_vars(MirStep(v))
    /\ \A i \in 1..MaxPasses : WF_vars(GcStep(i))
    /\ \A b \in Buckets : WF_vars(ComeBack(b))

FairSpec == Spec /\ Fair

ChildPerms == Permutations(Children)

-----------------------------------------------------------------------------
(* What must hold. *)

TypeOK ==
    /\ vers \subseteq cut /\ cut \subseteq Versions
    /\ \A v \in Versions : parts[v] \subseteq Objects /\ (parts[v] = {}) = (v \notin cut)
    /\ ended => vers = {}
    /\ \A b \in Buckets : objs[b] \subseteq Objects
    /\ marker \subseteq cut
    /\ down \subseteq Buckets
    /\ \A l \in late : l[1] \in Buckets

\* Every child made reads its version whole, from where the resolver sends it; every version P's
\* head names, and the writer's state, are whole in A.
NothingNeededGone ==
    /\ \A c \in Children : Live(c) => parts[rec[c].v] \subseteq objs[Src(rec[c].v)]
    /\ PartsOf(vers) \cup Writing \subseteq objs["A"]

\* A marked mirror is whole in B: the switch sends readers to a copy holding every part.
MarkersWhole == \A v \in marker : parts[v] \subseteq objs["B"]

\* Every reference is a mirror's pin or hold, or names a child whose record names its version: by
\* it the child's passes retire it.
ReferencesNamed ==
    \A b \in Buckets : \A r \in refs[b] :
        r[2] \in {"mirror", "hold"} \/ (rec[r[2]] # None /\ rec[r[2]].v = r[1])

\* Every child made holds a reference, in A or B.
ChildrenReferred ==
    \A c \in Children : Live(c) => \E b \in Buckets : <<rec[c].v, c>> \in refs[b]

\* Once a version's mirror is done, every child of it made refers to it in B, and, while P's head
\* names the version, not in A: none depends on A (forksRecheck, forksLeaveA,
\* mirrorListsAfterMark). A fork whose read again found the version dropped leaves its reference in
\* A (forksRereadBeforeLeaving), which keeps the version there as long as its child lives.
MirroredChildrenOnB ==
    \A c \in Children :
        (Live(c) /\ rec[c].v \in marker /\ mir[rec[c].v].pc = "done") =>
            /\ <<rec[c].v, c>> \in refs["B"]
            /\ rec[c].v \in vers => <<rec[c].v, c>> \notin refs["A"]

\* A mirror's hold stands only while its marker does, or its mirror is under way: B's collector,
\* which deletes the holds of the markers it deletes, leaves none behind it, which would keep P's
\* cell in A for good (mirrorHolds, mirrorHoldsUnmarked).
HoldsMarked ==
    \A v \in Versions : Hold(v) \in refs["A"] => v \in marker \/ mir[v].pc \notin {"idle", "done"}

\* A marked copy's hold stands in A (mirrorHolds): from before its marker until B's collector
\* deletes both, the hold first, the pass deleting it then about to delete the marker. It keeps P's cell,
\* and what the version inherits from P's origin, in A while readers in B read the copy.
MarkedHeld ==
    \A v \in marker :
        \/ Hold(v) \in refs["A"]
        \/ \E i \in 1..MaxPasses : gc[i].pc = "unmark" /\ v \in GoingMarkers(gc[i])

\* A pass over P's copy in B never keeps as standing a reference of a dead child whose own pass
\* already ran (passed): no future pass of that child will retire it, so keeping it standing leaves
\* the reference (and the copy) in B for good (copyChecksDead, board 10446).
NoDeadRefKept == \A i \in 1..MaxPasses : ~gc[i].missed

\* Reached, as models of specs/buckets/mutations show they are violated (Reaches*): a child made, a
\* mirror done with a child of its version made, a copy collected, its marker gone, and two
\* versions sharing a part both marked, the second mirror taking it as copied.
NoChildMade == \A c \in Children : ~Live(c)
NoMirrorDoneWithChild ==
    ~\E c \in Children : Live(c) /\ rec[c].v \in marker /\ mir[rec[c].v].pc = "done"
NoCopyCollected ==
    ~\E v \in Versions : /\ mir[v].pc = "done" /\ mir[v].copied = parts[v] /\ v \notin marker
                        /\ parts[v] # {} /\ parts[v] \cap objs["B"] = {}
NoSharedMirrors ==
    ~\E v, w \in marker : v # w /\ parts[v] \cap parts[w] # {}

\* Liveness, every bucket coming back: every mirror begun ends.
MirrorsEnd == \A v \in Versions : mir[v].pc # "idle" ~> mir[v].pc = "done"
=============================================================================
