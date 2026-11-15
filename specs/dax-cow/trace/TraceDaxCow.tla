---------------------------- MODULE TraceDaxCow ----------------------------
(***************************************************************************)
(* Copyright 2026 The fxdax Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Trace validation (TASK-444.1), as specs/follower/trace's: a run of     *)
(* guest/sim's tests/traced, dax.c over the simulator's fake kernel, its  *)
(* steps traced as DaxCow's actions (guest/sim/kernel/spec_trace.c,       *)
(* TraceDaxCowData), is a behavior of DaxCow: each record a step of the   *)
(* spec's, of the task it names, with what the code saw (the page, the    *)
(* pages a swap took or a rebind's look found, the lock a swap or a       *)
(* rebind held). TLC follows the behaviors the trace allows; the trace is *)
(* one of the spec's if one of them reaches its end, which TraceMatched,  *)
(* an invariant that must be violated, says (scripts/check-tla.sh). A run *)
(* that took a step the spec doesn't allow, or saw what the spec doesn't, *)
(* ends TLC's search short of its end; one whose steps lead the spec to   *)
(* break an invariant violates that first.                                *)
(*                                                                         *)
(* Data aren't traced: the spec's versions follow its own steps. A        *)
(* write(2)'s copy is as of its iomap_end, a writeback's as of its WRITE: *)
(* what lands between is a store to a page dirty again, which no step of  *)
(* the spec's tells from the other order.                                 *)
(***************************************************************************)
EXTENDS DaxCow, TraceDaxCowData

VARIABLE i

\* The run's file: every page a sealed extent's, none a hole (tests/traced.c's setup).
TraceInit == Init /\ i = 0 /\ \A p \in Pages : file[p].state = "sealed"

\* The step record e says the run took.
Step(e) ==
    CASE e.a = "Start" -> e.t \in Storers /\ SStartAt(e.t, e.p, e.w)
      [] e.a = "Throttle" -> e.t \in Storers /\ SThrottle(e.t)
      [] e.a = "Lock" -> e.t \in Storers /\ SLock(e.t)
      [] e.a = "Ahead" -> e.t \in Storers /\ SAheadAt(e.t, e.p)
      [] e.a = "Begin" -> e.t \in Storers /\ sPage[e.t] = e.p /\ SBegin(e.t)
      [] e.a = "Map" -> e.t \in Storers /\ sPage[e.t] = e.p /\ SMap(e.t)
      [] e.a = "SCopy" -> e.t \in Storers /\ SCopy(e.t)
      [] e.a = "Unlock" -> e.t \in Storers /\ SUnlock(e.t)
      [] e.a = "Return" -> e.t \in Storers /\ SReturn(e.t)
      [] e.a = "Store" ->
            e.t \in Storers /\ sPage[e.t] = e.p /\ pte[e.p] = "rw" /\ SStore(e.t)
      \* A store finding no writable PTE: the spec's store faulting again, or the first try of
      \* one that began faulting.
      [] e.a = "Fault" ->
            /\ e.t \in Storers /\ sPage[e.t] = e.p
            /\ \/ spc[e.t] = "store" /\ pte[e.p] # "rw" /\ SStore(e.t)
               \/ spc[e.t] = "throttle" /\ UNCHANGED vars
      [] e.a = "Resolved" -> e.t \in Storers /\ sPage[e.t] = e.p /\ SResolved(e.t)
      [] e.a = "FsyncStart" -> e.t \in Others /\ WStartOp(e.t, "fsync")
      [] e.a = "WStart" -> e.t \in Others /\ WStartOp(e.t, e.op)
      [] e.a = "WbLock" -> e.t \in Others /\ WWbLock(e.t)
      [] e.a = "SwapLock" ->
            e.t \in Others /\ WSwapLock(e.t) /\ ((e.m = "write") = (semX' = e.t))
      [] e.a = "Swap" -> e.t \in Others /\ WSwap(e.t) /\ wb' = e.ps
      [] e.a = "Protect" -> e.t \in Others /\ WWpAt(e.t, e.p)
      [] e.a = "WCopy" -> e.t \in Others /\ WCopy(e.t)
      [] e.a = "Write" -> e.t \in Others /\ WWrite(e.t)
      [] e.a = "Commit" -> e.t \in Others /\ WCommit(e.t)
      [] e.a = "WbUnlock" -> e.t \in Others /\ WUnlock(e.t)
      \* A seal: an fsync's, after its writebacks; any other, the host's, if it changes anything.
      [] e.a = "Seal" ->
            IF e.t \in Others /\ wpc[e.t] = "fsNext" THEN WFsSeal(e.t)
            ELSE Seal \/ (~SealChanges /\ UNCHANGED vars)
      [] e.a = "FsyncEnd" -> e.t \in Others /\ WFsDone(e.t)
      [] e.a = "RbWbLock" -> e.t \in Others /\ WRbWbLock(e.t)
      [] e.a = "RbLock" ->
            e.t \in Others /\ WRbLock(e.t) /\ ((e.m = "write") = (semX' = e.t))
      [] e.a = "RbLook" -> e.t \in Others /\ WRbLookAt(e.t, e.ps)
      [] e.a = "RbMap" -> e.t \in Others /\ WRbMapAt(e.t, e.p)
      [] e.a = "RbUnlock" -> e.t \in Others /\ WRbUnlock(e.t)
      [] e.a = "RbWbUnlock" -> e.t \in Others /\ WUnlock(e.t)
      [] e.a = "Clean" -> WCleanAt(e.p)
      \* What the spec has no step for: a range unmapped, cut or filled, a commit failed.
      [] OTHER -> FALSE

TraceNext == i < Len(Trace) /\ Step(Trace[i + 1]) /\ UNCHANGED lVars /\ i' = i + 1

TraceSpec == TraceInit /\ [][TraceNext]_<<vars, i>>

\* Violated once the whole trace is matched: what scripts/check-tla.sh expects.
TraceMatched == i < Len(Trace)
=============================================================================
