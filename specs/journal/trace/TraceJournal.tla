---------------------------- MODULE TraceJournal ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Trace validation of the journal (TASK-444.1): a sequence of the model  *)
(* test (crates/fxfs-posix), its journal traced step by step (fxfs's      *)
(* journal_trace feature; model_tests/journal_trace.rs writes             *)
(* TraceJournalData), is a behaviour of Journal: each step it took is one *)
(* of the spec's actions, and what the code saw after it is what the spec *)
(* has: each object's offsets after a transaction, the super-block it     *)
(* saved, what a remount's replay found and rebuilt. A power cut is a     *)
(* step that changes nothing and checks what the replay of what it left   *)
(* found: a super-block the spec has on the device (or the one being      *)
(* saved, whose save the code traces only once it returns), as much of    *)
(* the journal as is durable at least, the objects' offsets the spec's    *)
(* replay rebuilds, and nothing lost. TLC follows the behaviours the      *)
(* trace allows; the trace is one of the spec's if one reaches its end,   *)
(* which TraceMatched, an invariant that must be violated, says            *)
(* (scripts/check-tla.sh). A step the spec doesn't allow, or a state it   *)
(* doesn't have, ends TLC's search short of the end.                      *)
(***************************************************************************)
EXTENDS Journal, TraceJournalData

VARIABLE i

TraceInit == Init /\ i = 0

\* The <<old, cur>> a set of <<object, old, cur>> gives o: none if it names none.
PairOf(S, o) ==
    IF \E t \in S : t[1] = o THEN LET t == CHOOSE t \in S : t[1] = o IN <<t[2], t[3]>>
    ELSE <<None, None>>

\* The offset a set of <<object, offset>> gives o.
OffsetOf(S, o) == IF \E t \in S : t[1] = o THEN (CHOOSE t \in S : t[1] = o)[2] ELSE None

\* The super-block record e names.
SuperBlockOf(e) == [ckpt |-> e.c, off |-> [o \in Objects |-> OffsetOf(e.off, o)]]

\* The objects' offsets, `o_` and `c_`, are those the code had, `S`.
OffsetsAre(S, o_, c_) == \A o \in Objects : <<o_[o], c_[o]>> = PairOf(S, o)

\* What the replay of a cut here found, e, is what the spec says: a super-block on the device,
\* or the one write_super_block is saving; the journal durable as far as replay found it, at least
\* as far as flushed; the objects' offsets rebuilt as the spec's replay rebuilds them; and nothing
\* lost.
CutFinds(e) ==
    LET s == SuperBlockOf(e) IN
    /\ \/ s = sb
       \/ pc = "flushed" /\ s = [ckpt |-> padCkpt, off |-> padOff]
    /\ e.d \in durable..Len(journal)
    /\ s.ckpt - 1 <= e.d
    /\ LostNothing(s, e.d)
    /\ LET r == Replayed(s, e.d) IN \A o \in Objects : r[o] = PairOf(e.after, o)

\* A new filesystem, formatted: a trace of many paths (model_tests/journal_cover.rs) starts each
\* afresh.
Formatted ==
    /\ journal' = <<>>
    /\ durable' = 0
    /\ old' = [o \in Objects |-> None]
    /\ cur' = [o \in Objects |-> None]
    /\ began' = [o \in Objects |-> None]
    /\ sb' = NoSuperBlock
    /\ pc' = "idle"
    /\ padCkpt' = 1
    /\ padOff' = [o \in Objects |-> None]

\* The step record e says the code took.
Step(e) ==
    CASE e.a = "Commit" ->
            /\ \/ e.b # NoObj /\ e.e = NoObj /\ BeginFlush(e.b, e.m)
               \/ e.b = NoObj /\ e.e # NoObj /\ EndFlush(e.e, e.m)
               \/ e.b = NoObj /\ e.e = NoObj /\ Commit(e.m)
            /\ OffsetsAre(e.after, old', cur')
      [] e.a = "Pad" -> Pad /\ padCkpt' = e.c
      [] e.a = "Flush" -> Flush /\ durable' = e.d
      [] e.a = "DeviceFlush" -> DeviceFlush /\ durable' = e.d
      [] e.a = "Save" -> Save /\ sb' = SuperBlockOf(e)
      [] e.a = "Recover" ->
            /\ sb = SuperBlockOf(e)
            /\ Recover
            /\ durable' = e.d
            /\ OffsetsAre(e.after, old', cur')
      [] e.a = "Cut" -> CutFinds(e) /\ UNCHANGED vars
      [] e.a = "Format" -> Formatted
      [] OTHER -> FALSE

TraceNext == i < Len(Trace) /\ Step(Trace[i + 1]) /\ i' = i + 1

TraceSpec == TraceInit /\ [][TraceNext]_<<vars, i>>

\* Violated once the whole trace is matched: what scripts/check-tla.sh expects.
TraceMatched == i < Len(Trace)
=============================================================================
