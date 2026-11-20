---------------------------- MODULE HeadSlots ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* A cell's head kept in N slots (TASK-691): GCS takes about one write a   *)
(* second of an object, and a cell's head is swapped at each               *)
(* acknowledgement, so the head is N objects, version k of it in slot      *)
(* k mod N, each slot written once every N swaps. specs/bucket models the  *)
(* head as one object: none until its maker creates it, then read whole    *)
(* and swapped with a condition, until a deletion's swap makes it final,   *)
(* and the collector removes it with its dead cell. This spec shows the    *)
(* slots are that object (OneHead, FencedWrites, DeletedStays):            *)
(*                                                                         *)
(* - a write of version k+1 is conditional on its slot holding what the    *)
(*   writer last saw there (GCS's ifGenerationMatch, S3's If-Match), so    *)
(*   that only a writer holding the newest version, k, writes k+1: two     *)
(*   writers of k+1 write one slot under one condition, and one fails; a   *)
(*   writer behind fails, as a writer taken over fails its next swap of    *)
(*   the one head (slotCondition);                                         *)
(* - a head is made with every slot (slotsAtBirth): its maker creates the  *)
(*   first, version 0, the create deciding whose the cell is, then the     *)
(*   rest, versions 1 to N-1 of the head it made, their PUTs at once. No   *)
(*   other write creates a version in a slot: so a writer that held a cell *)
(*   removed since writes next where a slot was, conditional on what it    *)
(*   saw there, and fails, as its swap of the one head fails where it was; *)
(* - a head found unmade, its first slot version 0 and a slot empty, its   *)
(*   maker making it or dead making it, is failed by whoever needs it      *)
(*   gone (the collector, a failed fork's host): a mark created in each    *)
(*   slot it found empty, where nothing is. A slot the maker filled first  *)
(*   fails the mark, and the failer reads again; one mark landed, the head *)
(*   is failed for good: a read finding a mark finds none (marksFail), and *)
(*   the maker's creates where marks are fail, its making with them. No    *)
(*   one completes a head unmade, nor deletes it with a deletion's head:   *)
(*   deleted while its maker made it, a read of version 0 before the       *)
(*   deletion and of the maker's version 1 after found a whole head that   *)
(*   never was (TLC, on this spec's first draft); finished by another, a   *)
(*   finisher stalled past the head's life, its deletion and removal,      *)
(*   brought copies back beside a reader's stale view (on its second);     *)
(* - the collector deletes a failed head's slots, its marks only once its  *)
(*   maker's creates are done (marksKept): the code's collector, a day     *)
(*   after each mark was made, no request lasting so long;                 *)
(* - a read of the head reads every slot, one GET each, in any order,      *)
(*   while others write, and takes the newest version it found             *)
(*   (newestOfSlots); but only if it found every slot, or a deletion's     *)
(*   head (wholeOrFinal), and no mark: a slot empty and no deletion's head *)
(*   found, the head is being made, or removed, and the read finds none;   *)
(* - the collector removes a dead cell's head, its slots deleted one at a  *)
(*   time, in any order: a read finds the deletion's head, or, a slot      *)
(*   found empty, none.                                                    *)
(*                                                                         *)
(* So a read finds what the one head was at some moment of it: the newest  *)
(* as it began is in its slot until N more are written, so the read finds  *)
(* it or one newer, and each head between was the one head at some moment *)
(* between; a slot found empty, the read began before the head was whole,  *)
(* or overlaps its removal, the head a deletion's then, which a reader     *)
(* takes as none.                                                          *)
(*                                                                         *)
(* Each rule is a member of Rules; specs/head-slots/mutations removes one  *)
(* each, and must find the violation it prevents:                          *)
(*                                                                         *)
(* - slotCondition (NoSlotCondition): a write is conditional on its slot.  *)
(*   Without it, a writer behind writes over the newer version in its slot *)
(*   (FencedWrites).                                                       *)
(* - newestOfSlots (NoNewestOfSlots): a read takes the newest version of   *)
(*   every slot. Without it, the first slot's it reads, older than the     *)
(*   newest as it began (OneHead).                                         *)
(* - slotsAtBirth (NoSlotsAtBirth): a head is made with every slot, and    *)
(*   no write but its maker's creates a version in one. Without it, a head *)
(*   is made in its first slot alone, the others created as their turns    *)
(*   come: a writer that held a removed cell creates its next slot,        *)
(*   bringing back a live head (DeletedStays).                             *)
(* - wholeOrFinal (NoWholeOrFinal): a read finding a slot empty and no     *)
(*   deletion's head finds none. Without it, it takes the newest it found: *)
(*   the head its maker is still making, or an older one than the          *)
(*   deletion's being removed (OneHead).                                   *)
(* - marksFail (NoMarksFail): a read finding a mark finds none. Without    *)
(*   it, a failed head whose maker filled its other slots reads whole, a   *)
(*   head that never was (OneHead).                                        *)
(* - marksKept (NoMarksKept): a failed head's marks are deleted only once  *)
(*   its maker's creates are done. Without it, the maker's late creates    *)
(*   fill the slots the marks held, a whole head, live (FailedStays).      *)
(*                                                                         *)
(* Steps are requests, but the maker's creates of the slots after the      *)
(* first, one step: a PUT of them landing after the cell's deletion and    *)
(* removal would bring a copy of the made head back. A request outliving   *)
(* its cell's removal is not modelled, as specs/bucket models the one      *)
(* head's create as a step. Not modelled either: a deletion run again      *)
(* after the removal, which makes a deleted head, garbage, as with the one *)
(* head (specs/bucket's RemovedStayGone).                                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Writers,        \* those who write the head: a cell's writers, takers, deleters and failers
    Readers,        \* those who only read it: the collector, a restore checking its version
    N,              \* the slots
    MaxVersion,     \* the newest version a writer writes
    Rules           \* the rules in force (all, but in a mutation)

AllRules ==
    {"slotCondition", "newestOfSlots", "slotsAtBirth", "wholeOrFinal", "marksFail", "marksKept"}
ASSUME Rules \subseteq AllRules
ASSUME N >= 2

R(rule) == rule \in Rules
Slots == 0..(N - 1)
\* What an empty slot holds, as a read finds it: nothing, older than any version.
None == [v |-> -1, g |-> 0, fin |-> FALSE, mark |-> FALSE]

VARIABLES
    slot,       \* slot[i]: the object in slot i, [v, g, fin, mark]: the version it holds, its
                \* generation, each write's own, and whether it is a deletion's head, or a mark (of
                \* no version); None if empty
    gen,        \* the next generation
    born,       \* the maker's progress: "none", "first" (its first slot created, the rest to come),
                \* "whole", "failed" (its creates done, a mark among the slots)
    failed,     \* what happened: whether a mark landed in a head never whole
    wr,         \* wr[w]: writer w: [pc, seen, got, k, todo]: idle, reading the slots, failing a head
                \* unmade (the slots it is to mark, todo), or holding version k; seen[i] the object
                \* it last saw in slot i, got the slots its read has read
    rd,         \* rd[r]: reader r: [pc, seen, got, best, from, to]: the objects it read, by slot,
                \* the slots it read, the head it found (-1, none), and the one head's history's
                \* length as it began and ended
    writes,     \* what happened: every version written, [v, w, fin, stale]: by whom, whether
                \* final, and whether it was not newer than the newest then
    hist        \* what happened: the one head the slots are, each change of it appended: [v, fin],
                \* its version and whether a deletion's; v -1 for none (before it is whole, and
                \* once removed)

vars == <<slot, gen, born, failed, wr, rd, writes, hist>>

Max(S) == CHOOSE x \in S : \A y \in S : x >= y

\* The newest version written, -1 before any.
Newest == Max({-1} \cup {x.v : x \in writes})

\* Whether a deletion's head was written: the cell is deleted, for good.
Deleted == \E x \in writes : x.fin

\* The newest of the objects `seen`, by slot; whether they are a whole head, every slot holding
\* one (without slotsAtBirth, every slot up to the newest's, the rest made as their turns come);
\* whether one is a deletion's; whether one is a mark.
NewestOf(seen) == Max({seen[i].v : i \in Slots})
Whole(seen) ==
    \A i \in Slots : (R("slotsAtBirth") \/ i <= NewestOf(seen)) => seen[i] # None
Final(seen) == \E i \in Slots : seen[i].fin
Marked(seen) == \E i \in Slots : seen[i].mark

\* The head a read finds in the objects `seen`: none if one is a mark (marksFail); else the newest
\* of them, if every slot holds one, or one is a deletion's (wholeOrFinal); else none, -1.
Found(seen) ==
    IF R("marksFail") /\ Marked(seen) THEN -1
    ELSE IF Whole(seen) \/ Final(seen) \/ ~R("wholeOrFinal") THEN NewestOf(seen) ELSE -1

\* The one head, changed to version v, a deletion's if fin; none, v -1.
Became(v, fin) ==
    LET h == [v |-> v, fin |-> fin] IN hist' = IF hist[Len(hist)] = h THEN hist ELSE Append(hist, h)

\* A write, of version v, by w, final if fin.
Wrote(v, w, fin) == [v |-> v, w |-> w, fin |-> fin, stale |-> v <= Newest]

\* An object of version v written now, a deletion's if fin.
Obj(v, fin) == [v |-> v, g |-> gen, fin |-> fin, mark |-> FALSE]

Init ==
    /\ slot = [i \in Slots |-> None]
    /\ gen = 1
    /\ born = "none"
    /\ failed = FALSE
    /\ wr = [w \in Writers |-> [pc |-> "idle", seen |-> [i \in Slots |-> None], got |-> {},
                                k |-> -1, todo |-> {}]]
    /\ rd = [r \in Readers |-> [pc |-> "idle", seen |-> [i \in Slots |-> None], got |-> {},
                                best |-> -1, from |-> 1, to |-> 1]]
    /\ writes = {}
    /\ hist = <<[v |-> -1, fin |-> FALSE]>>

\* The maker creates the head's first slot, version 0: the one create deciding whose the cell is.
\* Without slotsAtBirth, the head is whole now: its other slots are created as their turns come.
MakeFirst ==
    /\ born = "none"
    /\ slot' = [slot EXCEPT ![0] = Obj(0, FALSE)]
    /\ gen' = gen + 1
    /\ born' = IF R("slotsAtBirth") THEN "first" ELSE "whole"
    /\ writes' = writes \cup {Wrote(0, "maker", FALSE)}
    /\ IF R("slotsAtBirth") THEN UNCHANGED hist ELSE Became(0, FALSE)
    /\ UNCHANGED <<failed, wr, rd>>

\* Then the rest (slotsAtBirth), versions 1 to N-1 of the head it made, their PUTs at once, each
\* a create where nothing is: failing where a mark is. Whole, the head is the one head now.
MakeRest ==
    LET made == {i \in Slots \ {0} : slot[i] = None}
        new == [i \in Slots |-> IF i \in made THEN [Obj(i, FALSE) EXCEPT !.g = gen + i] ELSE slot[i]]
        whole == \A i \in Slots : new[i] # None /\ ~new[i].mark
    IN /\ born = "first"
       /\ slot' = new
       /\ gen' = gen + N
       /\ born' = IF whole THEN "whole" ELSE "failed"
       /\ writes' = writes \cup {Wrote(i, "maker", FALSE) : i \in made}
       /\ IF whole THEN Became(N - 1, FALSE) ELSE UNCHANGED hist
       /\ UNCHANGED <<failed, wr, rd>>

\* Writer w begins to read the slots: a taking, a deletion, a failer's look, or after a write failed.
WriterRead(w) ==
    /\ wr[w].pc = "idle"
    /\ wr' = [wr EXCEPT ![w] = [@ EXCEPT !.pc = "reading", !.got = {}]]
    /\ UNCHANGED <<slot, gen, born, failed, rd, writes, hist>>

\* One of its GETs, of slot i, any it has not read. The last, and it holds the newest it found, if
\* it found the head whole and unmarked (wholeOrFinal, marksFail); found unmade, its first slot
\* version 0, it fails it, the slots it found empty to mark; having found a deletion's head, a
\* mark, or nothing, or no version 0 in the first slot, it is done.
WriterGet(w, i) ==
    LET seen == [wr[w].seen EXCEPT ![i] = slot[i]]
        got == wr[w].got \cup {i}
    IN /\ wr[w].pc = "reading" /\ i \notin wr[w].got
       /\ wr' = [wr EXCEPT ![w] =
                   [@ EXCEPT !.seen = seen, !.got = got, !.k = NewestOf(seen),
                             !.todo = {j \in Slots : seen[j] = None},
                             !.pc = IF got # Slots THEN "reading"
                                    ELSE IF NewestOf(seen) < 0 \/ Final(seen)
                                            \/ (Marked(seen) /\ R("marksFail"))
                                    THEN "idle"
                                    ELSE IF Whole(seen) \/ ~R("wholeOrFinal") THEN "holding"
                                    ELSE IF seen[0].v = 0 THEN "failing"
                                    ELSE "idle"]]
       /\ UNCHANGED <<slot, gen, born, failed, rd, writes, hist>>

\* Writer w, holding version k, writes k+1, conditional on what it last saw in its slot
\* (slotCondition): a deletion's head, if `fin`. Written, it holds it, or, having deleted the cell,
\* is done; failing, it reads the slots again. A slot it saw empty it creates nothing in
\* (slotsAtBirth).
Write(w, fin) ==
    LET k == wr[w].k
        v == k + 1
        i == v % N
        new == Obj(v, fin)
    IN /\ wr[w].pc = "holding" /\ k < MaxVersion
       /\ R("slotsAtBirth") => wr[w].seen[i] # None
       /\ IF R("slotCondition") => slot[i] = wr[w].seen[i] THEN
             /\ slot' = [slot EXCEPT ![i] = new]
             /\ gen' = gen + 1
             /\ wr' = [wr EXCEPT ![w] = [@ EXCEPT !.seen[i] = new, !.k = v,
                                                   !.pc = IF fin THEN "idle" ELSE "holding"]]
             /\ writes' = writes \cup {Wrote(v, w, fin)}
             \* The one head: this version, once whole, or a deletion's.
             /\ IF born = "whole" \/ fin THEN Became(v, fin) ELSE UNCHANGED hist
          ELSE
             /\ wr' = [wr EXCEPT ![w].pc = "idle"]
             /\ UNCHANGED <<slot, gen, writes, hist>>
       /\ UNCHANGED <<born, failed, rd>>

\* Writer w, failing a head unmade, marks one of the slots it found empty, where nothing is; a slot
\* filled since ends it, the head perhaps whole, and it reads the slots again; the last marked, it
\* is done. A mark landing in a head never whole fails it for good.
Mark(w, i) ==
    LET todo == wr[w].todo \ {i}
    IN /\ wr[w].pc = "failing" /\ i \in wr[w].todo
       /\ IF slot[i] = None THEN
             \* Of no generation of its own: no write is conditional on a mark, as no writer holds
             \* a head failed (and so a failer's marking again after a removal returns to a state
             \* seen, which keeps the model finite).
             /\ slot' = [slot EXCEPT ![i] = [v |-> -1, g |-> 0, fin |-> FALSE, mark |-> TRUE]]
             /\ failed' = (failed \/ born # "whole")
             /\ wr' = [wr EXCEPT ![w].todo = todo,
                                  ![w].pc = IF todo = {} THEN "idle" ELSE "failing"]
          ELSE
             /\ wr' = [wr EXCEPT ![w].pc = "idle"]
             /\ UNCHANGED <<slot, failed>>
       /\ UNCHANGED <<gen, born, rd, writes, hist>>

\* Writer w stops: crashed, or done.
WriterStop(w) ==
    /\ wr[w].pc # "idle"
    /\ wr' = [wr EXCEPT ![w].pc = "idle"]
    /\ UNCHANGED <<slot, gen, born, failed, rd, writes, hist>>

\* The collector removes the dead cell's head with it, a slot at a time, in any order, once the
\* head is a deletion's (a pass reads it so: no swap passes a final head). The last removed, the one
\* head is gone.
Remove(i) ==
    LET left == {j \in Slots : slot[j] # None} \ {i}
    IN /\ Deleted /\ slot[i] # None
       /\ slot' = [slot EXCEPT ![i] = None]
       /\ IF left = {} THEN Became(-1, FALSE) ELSE UNCHANGED hist
       /\ UNCHANGED <<gen, born, failed, wr, rd, writes>>

\* The collector deletes a failed head's slots, a slot at a time: its marks only once its maker's
\* creates are done (marksKept), its others whenever, each the maker's, landed.
RemoveFailed(i) ==
    /\ failed /\ slot[i] # None
    /\ R("marksKept") /\ slot[i].mark => born # "first"
    /\ slot' = [slot EXCEPT ![i] = None]
    /\ UNCHANGED <<gen, born, failed, wr, rd, writes, hist>>

\* Reader r begins to read the head, the one head's history's length noted: once, at any moment of
\* any behaviour, which every read's case is.
ReaderRead(r) ==
    /\ rd[r].pc = "idle"
    /\ rd' = [rd EXCEPT ![r] = [pc |-> "reading", seen |-> [i \in Slots |-> None], got |-> {},
                                best |-> -1, from |-> Len(hist), to |-> Len(hist)]]
    /\ UNCHANGED <<slot, gen, born, failed, wr, writes, hist>>

\* One of its GETs, of slot i, any it has not read. The last, every slot read, and it found what
\* `Found` says, the one head's history's length noted. Without newestOfSlots, its first finding
\* a version ends it, with that version.
ReaderGet(r, i) ==
    LET seen == [rd[r].seen EXCEPT ![i] = slot[i]]
        got == rd[r].got \cup {i}
        first == ~R("newestOfSlots") /\ slot[i].v >= 0
        done == first \/ got = Slots
    IN /\ rd[r].pc = "reading" /\ i \notin rd[r].got
       /\ rd' = [rd EXCEPT ![r] = [@ EXCEPT !.seen = seen, !.got = got,
                                             !.best = IF first THEN slot[i].v ELSE Found(seen),
                                             !.pc = IF done THEN "done" ELSE "reading",
                                             !.to = Len(hist)]]
       /\ UNCHANGED <<slot, gen, born, failed, wr, writes, hist>>

Next ==
    \/ MakeFirst \/ MakeRest
    \/ \E w \in Writers : WriterRead(w) \/ Write(w, FALSE) \/ Write(w, TRUE) \/ WriterStop(w)
    \/ \E w \in Writers, i \in Slots : WriterGet(w, i) \/ Mark(w, i)
    \/ \E i \in Slots : Remove(i) \/ RemoveFailed(i)
    \/ \E r \in Readers : ReaderRead(r)
    \/ \E r \in Readers, i \in Slots : ReaderGet(r, i)

Spec == Init /\ [][Next]_vars

WriterPerms == Permutations(Writers)

-----------------------------------------------------------------------------
(* What must hold. *)

\* Every version written was newer than the newest then: none behind wrote over a newer one, as no
\* writer taken over swaps the one head.
FencedWrites == \A x \in writes : ~x.stale

\* Each slot holds the newest version written of those it holds, v mod N its own, a mark, or
\* nothing.
SlotsNewest ==
    \A i \in Slots : slot[i] # None /\ ~slot[i].mark =>
        slot[i].v = Max({x.v : x \in {x \in writes : x.v % N = i}})

\* A read of the head finds what the one head was at some moment of the read: or none where it was
\* a deletion's, its slots being removed, which every reader takes as it takes none (a cell not
\* to be taken, written, nor forked from).
OneHead ==
    \A r \in Readers : rd[r].pc = "done" =>
        \E j \in rd[r].from..rd[r].to : hist[j].v = rd[r].best \/ (rd[r].best = -1 /\ hist[j].fin)

\* A cell deleted stays deleted: the one head, once a deletion's, is that, or none once removed.
DeletedStays ==
    \A j \in 1..Len(hist) : \A l \in j..Len(hist) : hist[j].fin => hist[l].fin \/ hist[l].v = -1

\* A head failed is none for good: it never was the one head, nor will be.
FailedStays == failed => \A j \in 1..Len(hist) : hist[j].v = -1
=============================================================================
