------------------------------ MODULE Buckets ------------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Forks across buckets and their mirrors (backlog doc-23, TASK-633.2),    *)
(* cut down from specs/bucket's forks, references and collection: one      *)
(* parent cell, P, at home in bucket A, its versions forked into children  *)
(* at home in bucket B. Every step reads or writes one bucket's objects,   *)
(* each bucket linearizable alone; a bucket down refuses every request,    *)
(* and a step that needs it waits.                                         *)
(*                                                                         *)
(* P's head, in A, names its versions; its writer drops them. A version's  *)
(* state is its parts, objects at keys under P's prefix in A, and, once    *)
(* copied, at the same keys in B (doc-23, Mirrors).                        *)
(*                                                                         *)
(* A fork of a version into a child in B: the child's record in B, being   *)
(* made; where its reference goes read in B, B if the version's mirror is  *)
(* marked there, else A; its reference written there; the head naming the  *)
(* version read in A, failing the fork if dropped; having referred in A,   *)
(* the marker read again, and if it is there now, a reference in B, then   *)
(* its own in A deleted; then the child made. Referring in B at once when  *)
(* the marker is there saves a fork after a mirror those two writes; the   *)
(* read again alone keeps every invariant. A child made reads its version  *)
(* from B once the mirror's marker is there, from A before (the resolver). *)
(* Forks read P's head in A: a fork into B waits for A, even once mirrored *)
(* (a fork going by the marker alone races B's collector deleting the      *)
(* copy). Children are deleted.                                            *)
(*                                                                         *)
(* A version's mirror into B starts at its first fork into B, in the       *)
(* background (the user, 2026-11-18): its pin, a reference of its own in   *)
(* B; A's head read, the mirror given up if its version is dropped; the    *)
(* parts copied, A to B, the same bytes at the same keys; the marker,      *)
(* create-only, the switch; the children referring in A listed, and their  *)
(* references written in B; theirs in A retired, marked; its pin dropped.  *)
(* A mirror's host may die at any step: it starts again from its pin, what *)
(* it wrote kept, past its copy if its marker is there. One whose parts    *)
(* are gone from A before it copied them is *)
(* given up: its pin dropped, no marker.                                   *)
(*                                                                         *)
(* The collector runs passes, each over one cell in one bucket: P in A     *)
(* (its head read, then the references under P in A listed, then the      *)
(* parts of versions neither names deleted); P's copy in B (A's head read, *)
(* then the references under P in B listed, then, of each version neither  *)
(* names, the marker deleted, then its parts); a child (its record read;   *)
(* deleted, its references retired, marked, in each bucket holding one,    *)
(* each once that bucket is up).                                           *)
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
(* - mirrorListsAfterMark (NoMirrorListsAfterMark): a mirror lists the     *)
(*   children referring in A after its marker: listed before, it misses a  *)
(*   fork that referred in A and read the head before the marker.          *)
(* - mirrorPins (NoMirrorPins): a mirror holds its pin from before it      *)
(*   copies until it is done.                                              *)
(* - mirrorChecks (NoMirrorChecks): a mirror reads A's head after its pin, *)
(*   before it copies, and gives up if its version is dropped: a pass of B *)
(*   that read the version dropped, then listed B's references, before the *)
(*   pin, deletes the copy under the marker (TLC, 2026-11-18).             *)
(* - mirrorResumesMarked (NoMirrorResumesMarked): a mirror started again   *)
(*   that finds its marker in B, once pinned, goes on to its listing: one  *)
(*   reading the head instead gives up if its version was dropped since,   *)
(*   its children left referring in A (TLC, 2026-11-18).                   *)
(* - markerAfterCopy (NoMarkerAfterCopy): a mirror writes its marker only  *)
(*   once it copied every part.                                            *)
(* - copiesKeptWhileNamed (NoCopiesKeptWhileNamed): B's collector keeps a  *)
(*   copy whose version A's head names.                                    *)
(* - markerGoesFirst (NoMarkerGoesFirst): B's collector deletes a copy's   *)
(*   marker before its parts.                                              *)
(* - copyRelists (NoCopyRelists): B's collector lists B's references after *)
(*   it reads A's head.                                                    *)
(*                                                                         *)
(* TLC found these needed no rule (2026-11-18): a fork referring in B at   *)
(* once when marked (the read again covers it), and a mirror listing the   *)
(* children before its copy too (its listing after the marker covers it).  *)
(*                                                                         *)
(* Not modelled: the root bucket and its registry (TASK-633.9's tests);    *)
(* P's own collection beyond its versions; compaction and moves (a copy    *)
(* takes moves objects too, as more parts); the children's own data.       *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Children,       \* the children's cells, at home in B
    Versions,       \* P's versions
    Parts,          \* the parts of each version's state
    MaxPasses,      \* collector passes
    MaxDowns,       \* buckets going down (each comes back)
    MaxRestarts,    \* mirrors' hosts dying
    Can,            \* the kinds of step the model takes
    None,           \* a model value: nothing
    Rules           \* the rules in force (all, but in a mutation)

AllRules == {"forksReferFirst", "forksRecheck", "forksLeaveA", "mirrorListsAfterMark",
             "mirrorPins", "mirrorChecks", "mirrorResumesMarked", "markerAfterCopy",
             "copiesKeptWhileNamed", "markerGoesFirst", "copyRelists"}

R(rule) == rule \in Rules

Buckets == {"A", "B"}

\* The mirror's pin of version v: a reference of its own, in B.
Pin(v) == <<v, "mirror">>

VARIABLES
    vers,       \* the versions P's head, in A, names
    objs,       \* objs[b]: the parts in bucket b, each <<version, part>>
    rec,        \* rec[c]: the child's record, in B: None, or [v, kind, deleted]
    refs,       \* refs[b]: the references under P in bucket b, each <<version, child>> or a pin
    marked,     \* marked[b]: the references retired in bucket b, marked
    marker,     \* the versions whose mirrors are marked in B
    fork,       \* fork[c]: the fork making c: None, or [pc, v, at]
    mir,        \* mir[v]: v's mirror into B: [pc, kids, copied]
    gc,         \* gc[i]: pass i
    down,       \* the buckets down
    passes, downs, restarts

vars == <<vers, objs, rec, refs, marked, marker, fork, mir, gc, down, passes, downs, restarts>>

Up(b) == b \notin down

PartsOf(v) == {<<v, k>> : k \in Parts}

\* Where a child reads its version v from: B once its mirror is marked there.
Src(v) == IF v \in marker THEN "B" ELSE "A"

Live(c) == rec[c] # None /\ rec[c].kind = "fork" /\ ~rec[c].deleted

\* The versions references in Rs name.
Named(Rs) == {r[1] : r \in Rs}

\* The children referring in A to version v.
ReferringInA(v) == {c \in Children : <<v, c>> \in refs["A"]}

GcIdle == [pc |-> "idle", kind |-> None, c |-> None, vs |-> {}, rs |-> {}]

MirIdle == [pc |-> "idle", kids |-> {}, copied |-> {}]

-----------------------------------------------------------------------------
(* P's writer. *)

DropVersion(v) ==
    /\ "drop" \in Can /\ v \in vers /\ Up("A")
    /\ vers' = vers \ {v}
    /\ UNCHANGED <<objs, rec, refs, marked, marker, fork, mir, gc, down, passes, downs, restarts>>

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
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, gc, down, passes, downs, restarts>>

\* Its record, in B, being made.
ForkRecord(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "record" /\ Up("B")
    /\ rec' = [rec EXCEPT ![c] = [v |-> q.v, kind |-> "forking", deleted |-> FALSE]]
    /\ fork' = [fork EXCEPT ![c].pc = IF R("forksReferFirst") THEN "where" ELSE "check"]
    /\ UNCHANGED <<vers, objs, refs, marked, marker, mir, gc, down, passes, downs, restarts>>

\* Where its reference goes, read in B: there if its version's mirror is marked, else in A.
ForkWhere(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "where" /\ Up("B")
    /\ fork' = [fork EXCEPT ![c].pc = "refer",
                            ![c].at = {IF q.v \in marker THEN "B" ELSE "A"}]
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, mir, gc, down, passes, downs, restarts>>

\* Its reference written there: a request after the marker's read.
ForkRefer(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "refer" /\ \A b \in q.at : Up(b)
    /\ refs' = [b \in Buckets |-> IF b \in q.at THEN refs[b] \cup {<<q.v, c>>} ELSE refs[b]]
    /\ fork' = [fork EXCEPT ![c].pc = IF R("forksReferFirst") THEN "check" ELSE "make"]
    /\ UNCHANGED <<vers, objs, rec, marked, marker, mir, gc, down, passes, downs, restarts>>

\* The head naming its version read, in A: dropped, the fork fails, its child deleted. Having
\* referred in A, the marker read again (forksRecheck): there now, a reference in B.
ForkCheck(c) ==
    LET q == fork[c]
        again == R("forksRecheck") /\ q.at = {"A"} /\ q.v \in marker
    IN /\ q # None /\ q.pc = "check" /\ Up("A") /\ (again => Up("B"))
       /\ IF q.v \notin vers THEN
             /\ rec' = [rec EXCEPT ![c].deleted = TRUE]
             /\ fork' = [fork EXCEPT ![c] = None]
             /\ UNCHANGED refs
          ELSE
             /\ refs' = IF again THEN [refs EXCEPT !["B"] = @ \cup {<<q.v, c>>}] ELSE refs
             /\ fork' = [fork EXCEPT ![c].pc = CASE ~R("forksReferFirst") -> "where"
                                                  [] again /\ R("forksLeaveA") -> "leave"
                                                  [] OTHER -> "make"]
             /\ UNCHANGED rec
       /\ UNCHANGED <<vers, objs, marked, marker, mir, gc, down, passes, downs, restarts>>

\* Its reference in A deleted, once it refers in B (forksLeaveA).
ForkLeave(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "leave" /\ Up("A")
    /\ refs' = [refs EXCEPT !["A"] = @ \ {<<q.v, c>>}]
    /\ fork' = [fork EXCEPT ![c].pc = "make"]
    /\ UNCHANGED <<vers, objs, rec, marked, marker, mir, gc, down, passes, downs, restarts>>

\* The child made, in B.
ForkMake(c) ==
    LET q == fork[c] IN
    /\ q # None /\ q.pc = "make" /\ Up("B")
    /\ rec' = [rec EXCEPT ![c].kind = "fork"]
    /\ fork' = [fork EXCEPT ![c] = None]
    /\ UNCHANGED <<vers, objs, refs, marked, marker, mir, gc, down, passes, downs, restarts>>

DeleteChild(c) ==
    /\ "delete" \in Can /\ Live(c) /\ Up("B")
    /\ rec' = [rec EXCEPT ![c].deleted = TRUE]
    /\ UNCHANGED <<vers, objs, refs, marked, marker, fork, mir, gc, down, passes, downs, restarts>>

-----------------------------------------------------------------------------
(* Mirrors into B. *)

\* Its pin, in B (mirrorPins); then, its marker found in B, started again after it, on to its
\* listing (mirrorResumesMarked). Without mirrorListsAfterMark, the children referring in A listed
\* now, in A, and referred in B with it.
MirPin(v) ==
    LET early == IF R("mirrorListsAfterMark") THEN {} ELSE ReferringInA(v)
        pin == IF R("mirrorPins") THEN {Pin(v)} ELSE {}
    IN /\ mir[v].pc = "pin" /\ Up("B") /\ (~R("mirrorListsAfterMark") => Up("A"))
       /\ refs' = [refs EXCEPT !["B"] = @ \cup pin \cup {<<v, c>> : c \in early}]
       /\ mir' = [mir EXCEPT ![v] =
                    [@ EXCEPT !.pc = CASE v \in marker /\ R("mirrorResumesMarked") ->
                                            IF R("mirrorListsAfterMark") THEN "list" ELSE "retire"
                                       [] R("mirrorChecks") -> "check"
                                       [] OTHER -> "copy",
                              !.kids = early]]
       /\ UNCHANGED <<vers, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts>>

\* A's head read after the pin (mirrorChecks): the version dropped, the mirror given up, its pin
\* dropped next.
MirCheck(v) ==
    /\ mir[v].pc = "check" /\ Up("A")
    /\ mir' = [mir EXCEPT ![v].pc = IF v \in vers THEN "copy" ELSE "unpin"]
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts>>

\* A part copied, A to B: the same bytes at the same key.
MirCopy(v, k) ==
    /\ mir[v].pc = "copy" /\ k \notin mir[v].copied /\ Up("A") /\ Up("B")
    /\ <<v, k>> \in objs["A"]
    /\ objs' = [objs EXCEPT !["B"] = @ \cup {<<v, k>>}]
    /\ mir' = [mir EXCEPT ![v].copied = @ \cup {k}]
    /\ UNCHANGED <<vers, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts>>

\* A part to copy gone from A: the mirror given up, its pin dropped next.
MirGiveUp(v) ==
    /\ mir[v].pc = "copy" /\ Up("A")
    /\ \E k \in Parts \ mir[v].copied : <<v, k>> \notin objs["A"]
    /\ mir' = [mir EXCEPT ![v].pc = "unpin"]
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, gc, down, passes, downs, restarts>>

\* The marker, create-only: the switch, once every part is copied (markerAfterCopy).
MirMark(v) ==
    /\ mir[v].pc = "copy" /\ Up("B")
    /\ R("markerAfterCopy") => mir[v].copied = Parts
    /\ marker' = marker \cup {v}
    /\ mir' = [mir EXCEPT ![v].pc = IF R("mirrorListsAfterMark") THEN "list" ELSE "retire"]
    /\ UNCHANGED <<vers, objs, rec, refs, marked, fork, gc, down, passes, downs, restarts>>

\* The children referring in A listed, after the marker (mirrorListsAfterMark), and referred in B.
MirList(v) ==
    LET kids == ReferringInA(v) IN
    /\ mir[v].pc = "list" /\ Up("A") /\ Up("B")
    /\ refs' = [refs EXCEPT !["B"] = @ \cup {<<v, c>> : c \in kids}]
    /\ mir' = [mir EXCEPT ![v] = [@ EXCEPT !.pc = "retire", !.kids = kids]]
    /\ UNCHANGED <<vers, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts>>

\* Their references in A retired, marked.
MirRetire(v) ==
    LET Rs == {<<v, c>> : c \in mir[v].kids} IN
    /\ mir[v].pc = "retire" /\ Up("A")
    /\ refs' = [refs EXCEPT !["A"] = @ \ Rs]
    /\ marked' = [marked EXCEPT !["A"] = @ \cup (Rs \cap refs["A"])]
    /\ mir' = [mir EXCEPT ![v].pc = "unpin"]
    /\ UNCHANGED <<vers, objs, rec, marker, fork, gc, down, passes, downs, restarts>>

\* Its pin dropped: done.
MirUnpin(v) ==
    /\ mir[v].pc = "unpin" /\ Up("B")
    /\ refs' = [refs EXCEPT !["B"] = @ \ {Pin(v)}]
    /\ mir' = [mir EXCEPT ![v].pc = "done"]
    /\ UNCHANGED <<vers, objs, rec, marked, marker, fork, gc, down, passes, downs, restarts>>

\* Its host dies: it starts again from its pin, what it wrote kept.
MirRestart(v) ==
    /\ "crash" \in Can /\ restarts < MaxRestarts
    /\ mir[v].pc \notin {"idle", "pin", "done"}
    /\ mir' = [mir EXCEPT ![v] = [MirIdle EXCEPT !.pc = "pin"]]
    /\ restarts' = restarts + 1
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, gc, down, passes, downs>>

-----------------------------------------------------------------------------
(* The collector. *)

\* A pass begun: over P in A, its head read; over P's copy in B, A's head read (copyRelists), or,
\* without the rule, B's references listed first; over a child, its record read.
GcStart(kind, c) ==
    LET i == passes + 1
        needs == CASE kind = "P" -> "A"
                   [] kind = "copy" -> IF R("copyRelists") THEN "A" ELSE "B"
                   [] kind = "child" -> "B"
    IN /\ "collect" \in Can /\ passes < MaxPasses /\ Up(needs)
       /\ (kind = "child") = (c # None)
       /\ gc' = [gc EXCEPT ![i] =
                   CASE kind = "P" -> [GcIdle EXCEPT !.pc = "list", !.kind = kind, !.vs = vers]
                     [] kind = "copy" ->
                          IF R("copyRelists")
                          THEN [GcIdle EXCEPT !.pc = "list", !.kind = kind, !.vs = vers]
                          ELSE [GcIdle EXCEPT !.pc = "list", !.kind = kind, !.rs = refs["B"]]
                     [] kind = "child" -> [GcIdle EXCEPT !.pc = "retire", !.kind = kind, !.c = c,
                                                        !.vs = IF rec[c] # None /\ rec[c].deleted
                                                               THEN {"deleted"} ELSE {}]]
       /\ passes' = i
       /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, mir, down, downs, restarts>>

\* The second read: of P in A, the references under P in A; of P's copy, B's references after A's
\* head (copyRelists), or A's head after them.
GcList(i) ==
    LET g == gc[i]
        b == IF g.kind = "P" \/ (g.kind = "copy" /\ ~R("copyRelists")) THEN "A" ELSE "B"
    IN /\ g.pc = "list" /\ Up(b)
       /\ gc' = [gc EXCEPT ![i] =
                   CASE g.kind = "P" -> [g EXCEPT !.pc = "delete", !.rs = refs["A"]]
                     [] R("copyRelists") -> [g EXCEPT !.pc = "unmark", !.rs = refs["B"]]
                     [] OTHER -> [g EXCEPT !.pc = "unmark", !.vs = vers]]
       /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, mir, down, passes, downs, restarts>>

\* The versions to go, of P in A or of its copy in B: neither the head (copiesKeptWhileNamed, for
\* a copy) nor a reference names.
Going(g) ==
    LET named == IF g.kind = "copy" /\ ~R("copiesKeptWhileNamed") THEN {} ELSE g.vs
    IN (Versions \ named) \ Named(g.rs)

\* Of P's copy in B: the markers of the versions going deleted first (markerGoesFirst); without
\* the rule, their parts first.
GcUnmark(i) ==
    LET g == gc[i] IN
    /\ g.pc = "unmark" /\ Up("B")
    /\ IF R("markerGoesFirst")
       THEN marker' = marker \ Going(g) /\ UNCHANGED objs
       ELSE objs' = [objs EXCEPT !["B"] = {p \in @ : p[1] \notin Going(g)}] /\ UNCHANGED marker
    /\ gc' = [gc EXCEPT ![i].pc = "delete"]
    /\ UNCHANGED <<vers, rec, refs, marked, fork, mir, down, passes, downs, restarts>>

\* The parts of the versions going deleted: of P in A; of its copy in B, after their markers (or,
\* without markerGoesFirst, the markers after the parts).
GcDelete(i) ==
    LET g == gc[i]
        b == IF g.kind = "P" THEN "A" ELSE "B"
    IN /\ g.pc = "delete" /\ Up(b)
       /\ objs' = [objs EXCEPT ![b] = {p \in @ : p[1] \notin Going(g)}]
       /\ marker' = IF g.kind = "copy" /\ ~R("markerGoesFirst") THEN marker \ Going(g) ELSE marker
       /\ gc' = [gc EXCEPT ![i] = GcIdle]
       /\ UNCHANGED <<vers, rec, refs, marked, fork, mir, down, passes, downs, restarts>>

\* A deleted child's references retired, marked, in bucket b, if it holds one there.
GcRetire(i, b) ==
    LET g == gc[i]
        Rs == {r \in refs[b] : r[2] = g.c}
    IN /\ g.pc = "retire" /\ Up(b) /\ g.vs = {"deleted"} /\ Rs # {}
       /\ refs' = [refs EXCEPT ![b] = @ \ Rs]
       /\ marked' = [marked EXCEPT ![b] = @ \cup Rs]
       /\ UNCHANGED <<vers, objs, rec, marker, fork, mir, gc, down, passes, downs, restarts>>

\* The child's pass ended, once it holds no reference, or was not deleted.
GcRetired(i) ==
    LET g == gc[i] IN
    /\ g.pc = "retire"
    /\ g.vs = {"deleted"} => \A b \in Buckets : ~\E r \in refs[b] : r[2] = g.c
    /\ gc' = [gc EXCEPT ![i] = GcIdle]
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, mir, down, passes, downs, restarts>>

-----------------------------------------------------------------------------
(* Buckets going down, and coming back. *)

GoDown(b) ==
    /\ "down" \in Can /\ downs < MaxDowns /\ Up(b)
    /\ down' = down \cup {b}
    /\ downs' = downs + 1
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, mir, gc, passes, restarts>>

ComeBack(b) ==
    /\ b \in down
    /\ down' = down \ {b}
    /\ UNCHANGED <<vers, objs, rec, refs, marked, marker, fork, mir, gc, passes, downs, restarts>>

-----------------------------------------------------------------------------

Init ==
    /\ vers = Versions
    /\ objs = [b \in Buckets |-> IF b = "A" THEN Versions \X Parts ELSE {}]
    /\ rec = [c \in Children |-> None]
    /\ refs = [b \in Buckets |-> {}]
    /\ marked = [b \in Buckets |-> {}]
    /\ marker = {}
    /\ fork = [c \in Children |-> None]
    /\ mir = [v \in Versions |-> MirIdle]
    /\ gc = [i \in 1..MaxPasses |-> GcIdle]
    /\ down = {}
    /\ passes = 0 /\ downs = 0 /\ restarts = 0

ForkStep(c) ==
    ForkRecord(c) \/ ForkWhere(c) \/ ForkRefer(c) \/ ForkCheck(c) \/ ForkLeave(c) \/ ForkMake(c)

MirStep(v) ==
    \/ MirPin(v) \/ MirCheck(v) \/ MirGiveUp(v) \/ MirMark(v) \/ MirList(v) \/ MirRetire(v)
    \/ MirUnpin(v) \/ \E k \in Parts : MirCopy(v, k)

GcStep(i) == GcList(i) \/ GcUnmark(i) \/ GcDelete(i) \/ GcRetired(i) \/ \E b \in Buckets : GcRetire(i, b)

Next ==
    \/ \E v \in Versions : DropVersion(v) \/ MirStep(v) \/ MirRestart(v)
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
    /\ vers \subseteq Versions
    /\ \A b \in Buckets : objs[b] \subseteq Versions \X Parts
    /\ marker \subseteq Versions
    /\ down \subseteq Buckets

\* Every child made reads its version whole, from where the resolver sends it; and every version
\* P's head names is whole in A.
NothingNeededGone ==
    /\ \A c \in Children : Live(c) => PartsOf(rec[c].v) \subseteq objs[Src(rec[c].v)]
    /\ \A v \in vers : PartsOf(v) \subseteq objs["A"]

\* A marked mirror is whole in B: the switch sends readers to a copy holding every part.
MarkersWhole == \A v \in marker : PartsOf(v) \subseteq objs["B"]

\* Every reference is a mirror's pin, or names a child whose record names its version: by it the
\* child's passes retire it.
ReferencesNamed ==
    \A b \in Buckets : \A r \in refs[b] :
        r[2] = "mirror" \/ (rec[r[2]] # None /\ rec[r[2]].v = r[1])

\* Every child made holds a reference, in A or B.
ChildrenReferred ==
    \A c \in Children : Live(c) => \E b \in Buckets : <<rec[c].v, c>> \in refs[b]

\* Once a version's mirror is done, every child of it made refers to it in B, and not in A: none
\* depends on A (forksRecheck, forksLeaveA, mirrorListsAfterMark).
MirroredChildrenOnB ==
    \A c \in Children :
        (Live(c) /\ rec[c].v \in marker /\ mir[rec[c].v].pc = "done") =>
            /\ <<rec[c].v, c>> \in refs["B"]
            /\ <<rec[c].v, c>> \notin refs["A"]

\* Reached, as models of specs/buckets/mutations show they are violated (Reaches*): a child made, a
\* mirror done with a child of its version made, and a copy collected, its marker gone.
NoChildMade == \A c \in Children : ~Live(c)
NoMirrorDoneWithChild ==
    ~\E c \in Children : Live(c) /\ rec[c].v \in marker /\ mir[rec[c].v].pc = "done"
NoCopyCollected ==
    ~\E v \in Versions : /\ mir[v].pc = "done" /\ mir[v].copied = Parts /\ v \notin marker
                        /\ PartsOf(v) \cap objs["B"] = {}

\* Liveness, every bucket coming back: every mirror begun ends, and every deleted child's
\* references go, once its passes run.
MirrorsEnd == \A v \in Versions : mir[v].pc # "idle" ~> mir[v].pc = "done"
=============================================================================
