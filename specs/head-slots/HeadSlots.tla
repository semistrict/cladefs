---------------------------- MODULE HeadSlots ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* A cell's head kept in N slots (TASK-690): GCS takes about one write a   *)
(* second of an object, and a cell's head is swapped at each               *)
(* acknowledgement, so the head is N objects, version k of it in slot      *)
(* k mod N, each slot written once every N swaps. specs/bucket models the  *)
(* head as one object, read whole and swapped with a condition; this spec  *)
(* shows the slots are that object:                                        *)
(*                                                                         *)
(* - a write of version k+1 is conditional on its slot holding what the    *)
(*   writer last saw there (GCS's ifGenerationMatch, S3's If-Match), so    *)
(*   that only a writer holding the newest version, k, writes k+1: two     *)
(*   writers of k+1 write one slot under one condition, and one fails; a   *)
(*   writer behind, its k not the newest, fails, as a writer taken over    *)
(*   fails its next swap of the one head (FencedWrites);                   *)
(* - a read of the head reads every slot, one GET each, in any order,      *)
(*   while writers write, and takes the newest version it found: a version *)
(*   that was the newest at some moment of the read (ReadsLinearizable),   *)
(*   as one GET of the one head reads it. The newest version as the read   *)
(*   began is in its slot until N more are written, so the read finds it,  *)
(*   or one newer; and each version between that one and the newest as the *)
(*   read ends was the newest at some moment between.                      *)
(*                                                                         *)
(* A writer learns the slots by such a read, or by its own writes; a write *)
(* failing, it reads them again. Each rule is a member of Rules;           *)
(* specs/head-slots/mutations removes one each, and must find the          *)
(* violation it prevents:                                                  *)
(*                                                                         *)
(* - slotCondition (NoSlotCondition): a write is conditional on its slot.  *)
(*   Without it, a writer behind writes over the newer version in its slot *)
(*   (FencedWrites).                                                       *)
(* - newestOfSlots (NoNewestOfSlots): a read takes the newest version of   *)
(*   every slot. Without it, it takes the first slot's it reads, an older  *)
(*   version than the newest as it began (ReadsLinearizable).              *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Writers,        \* those who write the head: a cell's writers, takers and deleters
    Readers,        \* those who only read it: the collector, a restore checking its version
    N,              \* the slots
    MaxVersion,     \* the versions written, after version 0
    Rules           \* the rules in force (all, but in a mutation)

AllRules == {"slotCondition", "newestOfSlots"}
ASSUME Rules \subseteq AllRules
ASSUME N >= 1

R(rule) == rule \in Rules
Slots == 0..(N - 1)
None == [v |-> -1, g |-> 0]

VARIABLES
    slot,       \* slot[i]: the object in slot i, [v, g]: the version it holds and its generation,
                \* each write's own; None if never written
    gen,        \* the next generation
    wr,         \* wr[w]: writer w: [pc, seen, got, k]: idle, reading the slots, or holding version
                \* k; seen[i] the object it last saw in slot i, got the slots its read has read
    rd,         \* rd[r]: reader r: [pc, seen, best, from, to]: what it read, the newest version it
                \* found, and the newest written as its read began and ended
    writes      \* what happened: every version written, [v, w, stale]: by whom, and whether the
                \* writer's version before it, k, was not the newest then

vars == <<slot, gen, wr, rd, writes>>

Max(S) == CHOOSE x \in S : \A y \in S : x >= y

\* The newest version written.
Newest == Max({x.v : x \in writes})

\* The newest version among the objects seen.
NewestSeen(seen) == Max({seen[i].v : i \in Slots})

Init ==
    \* Version 0, the head the cell's maker created, in slot 0.
    /\ slot = [i \in Slots |-> IF i = 0 THEN [v |-> 0, g |-> 1] ELSE None]
    /\ gen = 2
    /\ wr = [w \in Writers |-> [pc |-> "idle", seen |-> [i \in Slots |-> None], got |-> {},
                                k |-> -1]]
    /\ rd = [r \in Readers |-> [pc |-> "idle", seen |-> {}, best |-> -1, from |-> -1, to |-> -1]]
    /\ writes = {[v |-> 0, w |-> "maker", stale |-> FALSE]}

\* Writer w begins to read the slots: a taking, or after a write failed.
WriterRead(w) ==
    /\ wr[w].pc = "idle"
    /\ wr' = [wr EXCEPT ![w] = [@ EXCEPT !.pc = "reading", !.got = {}]]
    /\ UNCHANGED <<slot, gen, rd, writes>>

\* One of its GETs, of slot i, any it has not read; the last, and it holds the newest it found.
WriterGet(w, i) ==
    LET seen == [wr[w].seen EXCEPT ![i] = slot[i]]
        got == wr[w].got \cup {i}
        done == got = Slots
    IN /\ wr[w].pc = "reading" /\ i \notin wr[w].got
       /\ wr' = [wr EXCEPT ![w] = [@ EXCEPT !.seen = seen, !.got = got,
                                             !.pc = IF done THEN "holding" ELSE "reading",
                                             !.k = IF done THEN NewestSeen(seen) ELSE -1]]
       /\ UNCHANGED <<slot, gen, rd, writes>>

\* Writer w, holding version k, writes k+1 in its slot, conditional on what it last saw there
\* (slotCondition): written, it holds k+1; failing, it reads the slots again.
Write(w) ==
    LET k == wr[w].k
        i == (k + 1) % N
        new == [v |-> k + 1, g |-> gen]
    IN /\ wr[w].pc = "holding" /\ k < MaxVersion
       /\ IF R("slotCondition") => slot[i] = wr[w].seen[i] THEN
             /\ slot' = [slot EXCEPT ![i] = new]
             /\ gen' = gen + 1
             /\ wr' = [wr EXCEPT ![w] = [@ EXCEPT !.seen[i] = new, !.k = k + 1]]
             /\ writes' = writes \cup {[v |-> k + 1, w |-> w, stale |-> k # Newest]}
          ELSE
             /\ wr' = [wr EXCEPT ![w].pc = "idle"]
             /\ UNCHANGED <<slot, gen, writes>>
       /\ UNCHANGED rd

\* Writer w stops: crashed, or done.
WriterStop(w) ==
    /\ wr[w].pc # "idle"
    /\ wr' = [wr EXCEPT ![w].pc = "idle"]
    /\ UNCHANGED <<slot, gen, rd, writes>>

\* Reader r begins to read the head, the newest version written as it begins noted.
ReaderRead(r) ==
    /\ rd[r].pc \in {"idle", "done"}
    /\ rd' = [rd EXCEPT ![r] = [pc |-> "reading", seen |-> {}, best |-> -1, from |-> Newest,
                                to |-> -1]]
    /\ UNCHANGED <<slot, gen, wr, writes>>

\* One of its GETs, of slot i, any it has not read: the newest version found so far kept. The
\* last, every slot read (newestOfSlots), and it has read the head, the newest written as it ends
\* noted; without the rule, the first that holds a version.
ReaderGet(r, i) ==
    LET best == IF slot[i].v > rd[r].best THEN slot[i].v ELSE rd[r].best
        seen == rd[r].seen \cup {i}
        done == IF R("newestOfSlots") THEN seen = Slots ELSE best >= 0
    IN /\ rd[r].pc = "reading" /\ i \notin rd[r].seen
       /\ rd' = [rd EXCEPT ![r] = [@ EXCEPT !.seen = seen, !.best = best,
                                             !.pc = IF done THEN "done" ELSE "reading",
                                             !.to = IF done THEN Newest ELSE -1]]
       /\ UNCHANGED <<slot, gen, wr, writes>>

Next ==
    \/ \E w \in Writers : WriterRead(w) \/ Write(w) \/ WriterStop(w)
    \/ \E w \in Writers, i \in Slots : WriterGet(w, i)
    \/ \E r \in Readers : ReaderRead(r)
    \/ \E r \in Readers, i \in Slots : ReaderGet(r, i)

Spec == Init /\ [][Next]_vars

WriterPerms == Permutations(Writers)

-----------------------------------------------------------------------------
(* What must hold. *)

\* Every version written was written by a writer holding the newest version before it: none
\* behind wrote over a newer one, as no writer taken over swaps the one head. So each version is
\* written once, each on the one before it.
FencedWrites == \A x \in writes : ~x.stale

\* Each slot holds the newest version written of those it holds, k mod N its own.
SlotsNewest ==
    \A i \in Slots : slot[i] # None =>
        slot[i].v = Max({x.v : x \in {x \in writes : x.v % N = i}})

\* A read of the head finds a version that was the newest at some moment of the read: as new as
\* the newest as it began, and no newer than the newest as it ended.
ReadsLinearizable ==
    \A r \in Readers : rd[r].pc = "done" => rd[r].from <= rd[r].best /\ rd[r].best <= rd[r].to
=============================================================================
