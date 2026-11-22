---------------------------- MODULE PartSlabs ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* The extent cache in part slabs, its quarantine scoped to the VM         *)
(* processes a slab was staged to, and copy on write in the VM process     *)
(* (docs/design doc-26, Wave 2 of the 2026-11-21 simplification): what     *)
(* the daemon and the VM processes do with a cached part's memory, from    *)
(* its page-in to its free, at the level of the stage operations.          *)
(*                                                                         *)
(* A cached part is one slab of its arena's memory object, a slab a part   *)
(* (doc-26: a whole slab, or a run of a small slab's pages, the same here: *)
(* one unit owned whole). The daemon's table names, for each part, the     *)
(* slab it is in, owned by the part's slot while present, by an eviction   *)
(* token while an eviction unmaps it from the windows, then by the         *)
(* quarantine, until every VM process it was staged to has acknowledged    *)
(* what took it away, or is dead; then it is free, and may be punched      *)
(* (zeros) or filled again. Two tenants' VM processes, va and vb, each     *)
(* with its tenant's arena, A or B, and the default arena D, public bases' *)
(* parts, which both map.                                                  *)
(*                                                                         *)
(* - the daemon: a guest's MAP or UNMAP over a window page (the window     *)
(*   names a part, zeros, or nothing there, and sends the VM process an    *)
(*   unmap, which ends a private page there); a fault on a page the VM     *)
(*   process has nothing installed at, which stages the part's slab there  *)
(*   (a map); a page-in, which fills a free slab; evictions, on several    *)
(*   threads at once (relief, the governor's shrink, the control API's     *)
(*   Shrink, a KEK's forgetting), each taking its part's slab with a token *)
(*   of its own, sending each window page staged it an evict (a host's     *)
(*   change, which leaves a private page there), then handing the slab to  *)
(*   the quarantine, its waits each VM process's last operation sent;      *)
(* - each VM process: makes what it is sent in order, acknowledging each,  *)
(*   and copies on write itself: a guest's write to a page staged a slab   *)
(*   copies the slab's bytes into a page of its own memory, or zeros, and  *)
(*   counts it against private_max, telling the daemon nothing; it may     *)
(*   freeze (stop making what it is sent), thaw, or be killed by the       *)
(*   daemon's watchdog, which frees whatever it held.                      *)
(*                                                                         *)
(* The rules, each a member of Rules; specs/part-slabs/mutations removes   *)
(* one each, and must find the violation it prevents:                      *)
(*                                                                         *)
(* - ownTokens (NoOwnTokens): an eviction frees the slab its token took    *)
(*   alone. Without it, it frees every slab being evicted, another's too,  *)
(*   whose evicts that other has yet to send (TASK-560, board 2516): a VM  *)
(*   process that acknowledged all it was sent keeps a freed slab mapped   *)
(*   (NoStaleInstall).                                                     *)
(* - waitsForStaged (NoWaitsForStaged): the quarantine waits for every VM  *)
(*   process the slab was staged to since it was filled. Without it, for   *)
(*   those of the slab's arena's tenant alone: a public part's slab, in    *)
(*   the default arena, staged to both tenants' VM processes, waits for    *)
(*   none (NoStaleInstall).                                                *)
(* - scopedWaits (NoScopedWaits): ... and for none other. Without it, for  *)
(*   every VM process on the host, as cladefsd's PEERS has it: tenant B's  *)
(*   slabs wait on tenant A's frozen VM (ScopedWaits).                     *)
(* - killBeforeLetGo (NoKillBeforeLetGo): a VM process's waits are let go  *)
(*   of only once it is dead, killed by the watchdog. Without it, let go of *)
(*   as it stops answering: it thaws with a freed slab mapped               *)
(*   (NoStaleInstall).                                                     *)
(* - copyUnderLock (NoCopyUnderLock): the VM process's copy on write reads *)
(*   the slab staged at the page under the window's lock, which the        *)
(*   operations it is sent take: so no evict comes between its look and    *)
(*   its copy, which the daemon frees the slab after. Without it, the copy *)
(*   reads what the slab holds by then (NoStaleCopy).                      *)
(* - evictionKeepsPrivate (NoEvictionKeepsPrivate): an evict, a host's     *)
(*   change, leaves a private page at its window page; only the guest's    *)
(*   MAP or UNMAP ends one. The daemon no longer knows which pages are     *)
(*   private: it sends evicts for every page it staged the slab at.        *)
(*   Without it, an eviction drops the guest's write (NoLostWrite).        *)
(* - registerOnce (NoRegisterOnce): a part registers a window once,        *)
(*   however often the guest MAPs it over its own range. Without it, every *)
(*   such MAP adds an entry (TASK-602, board 2600) (WindowsOnce).          *)
(* - watchdog (NoWatchdog, a property): a VM process frozen is killed in   *)
(*   time. Without it, what it was staged waits for good                   *)
(*   (QuarantineDrains, PartSlabsLive's).                                  *)
(*                                                                         *)
(* Steps are messages, or merged: a fault is the VM process's FAULT and    *)
(* the daemon's answer at once (it maps what the window names when it      *)
(* answers, its map after any unmap sent before, in order); a page-in      *)
(* fills a slab in one step; a VM process's copy reads the slab's bytes    *)
(* through its arena's memory object, a step (two without copyUnderLock).  *)
(* Not modelled: the guest's own protocol of private pages (dirty sets,    *)
(* writeback, rebinding), specs/dax-cow's; pages installed lazily, as      *)
(* named (dax::uffd's install-as-named and take-away, specs/dax-cow's      *)
(* rules installAsNamed, changesTakeAway); a part shorter than a slab,     *)
(* whose run of a small slab is owned whole as a slab is; relief's choice  *)
(* of what to evict.                                                       *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Evictors,       \* the threads that evict, at once
    Offsets,        \* each window's pages
    PrivateMax,     \* each window's private_max
    MaxMaps,        \* the guests' MAPs and UNMAPs, all VM processes'
    MaxWrites,      \* the guests' writes
    MaxEvictions,   \* evictions begun
    MaxFreezes,     \* VM processes frozen
    Can,            \* the kinds of step the model takes
    Rules           \* the protocol's rules in force (all, but in a mutation)

AllRules == {"ownTokens", "waitsForStaged", "scopedWaits", "killBeforeLetGo", "copyUnderLock",
             "evictionKeepsPrivate", "registerOnce", "watchdog"}
AllCan == {"zeros", "freeze", "punch"}

ASSUME Rules \subseteq AllRules
ASSUME Can \subseteq AllCan
ASSUME PrivateMax \in Nat \ {0}

R(rule) == rule \in Rules

\* The host: tenant a's VM process va, tenant b's vb; their arenas, and the default one, D, which
\* holds public bases' parts and which every VM process maps. A part, of an arena, has one slab of
\* it here: the slab it is filled into whenever it is paged in (the allocator's choice of a slab is
\* no rule's).
VMs == {"va", "vb"}
Arenas == {"A", "B", "D"}
OwnArena(v) == IF v = "va" THEN "A" ELSE "B"
Holds(v) == {OwnArena(v), "D"}
Parts == {"pa", "pb", "pd"}
PartArena(p) == CASE p = "pa" -> "A" [] p = "pb" -> "B" [] p = "pd" -> "D"
Slabs == {"sA", "sB", "sD"}
SlabArena(s) == CASE s = "sA" -> "A" [] s = "sB" -> "B" [] s = "sD" -> "D"
SlabOf(p) == CHOOSE s \in Slabs : SlabArena(s) = PartArena(p)
\* The parts VM process v's window may name: of the arenas it holds (open_cell's Arenas::of).
Mappable(v) == {p \in Parts : PartArena(p) \in Holds(v)}

\* What nothing is, and what a punched slab, or a page of zeros, holds.
None == "none"
Zero == "zero"

VARIABLES
    \* The daemon's cache.
    owner,      \* owner[s]: [k: "free"], [k: "part", p] (present), [k: "token", e] (an
                \* eviction's), or [k: "quarantined"]
    held,       \* held[s]: the part whose bytes the slab holds, or Zero (punched)
    slot,       \* slot[p]: [st: "absent"], [st: "present", s], or [st: "evicting", s, e]
    staged,     \* staged[s]: the VM processes it was staged to since it was filled
    regs,       \* regs[p][v]: how many times p's window list names v's window (TASK-602)
    quar,       \* the quarantine: [s, waits], waits a set of <<VM process, operation>>
    ev,         \* ev[e]: eviction thread e: [pc: "idle"], or [pc: "unmap" or "free", s, p, todo]
    \* The daemon's windows, and what it sends their VM processes.
    win,        \* win[v][o]: what v's window names at page o: a part, Zero, or None
    pte,        \* pte[v][o]: the slab the daemon staged at o, Zero (the zeros page), or None
    q,          \* q[v]: the operations sent, not yet made: [op: "map", "unmap" or "evict", o, s, p, id]
    sent,       \* sent[v]: the last operation's number
    \* The VM processes.
    inst,       \* inst[v][o]: [k: "none"], [k: "slab", s, p] (staged, as the part p), [k: "zeros"],
                \* or [k: "priv", base, named, w]: a private page, of base's bytes, copied from a
                \* page named as `named`, and the guest's write w
    acked,      \* acked[v]: the last operation v made
    wrote,      \* wrote[v][o]: the guest's last write there, 0 if none since a MAP or UNMAP of its
    vst,        \* vst[v]: "running", "frozen" or "dead"
    cp,         \* cp[v]: a copy looked at but not made (without copyUnderLock): [k: "look", o, s,
                \* named], or [k: "none"]
    \* Bounds.
    maps, writes, evictions, freezes

cacheVars == <<owner, held, slot, staged, regs, quar, ev>>
winVars == <<win, pte, q, sent>>
vmVars == <<inst, acked, wrote, vst, cp>>
countVars == <<maps, writes, evictions, freezes>>
vars == <<cacheVars, winVars, vmVars, countVars>>

Nothing == [k |-> "none"]
Free == [k |-> "free"]
Quarantined == [k |-> "quarantined"]
Absent == [st |-> "absent"]
Idle == [pc |-> "idle"]

\* VM process v's private pages.
Private(v) == {o \in Offsets : inst[v][o].k = "priv"}

\* Sends VM process v the operation op (its number given here).
Send(v, op) ==
    /\ q' = [q EXCEPT ![v] = Append(@, [op EXCEPT !.id = sent[v] + 1])]
    /\ sent' = [sent EXCEPT ![v] = @ + 1]

Op(kind, o, s, p) == [op |-> kind, o |-> o, s |-> s, p |-> p, id |-> 0]

\* Whom the quarantine of slab s waits for: the VM processes it was staged to (waitsForStaged,
\* scopedWaits); without scopedWaits, every one, as cladefsd's PEERS; without waitsForStaged,
\* those of its arena's tenant.
Scope(s) ==
    CASE ~R("scopedWaits") -> VMs
      [] ~R("waitsForStaged") -> {v \in VMs : OwnArena(v) = SlabArena(s)}
      [] OTHER -> staged[s]

-----------------------------------------------------------------------------
(* The daemon. *)

\* A guest's MAP of x over page o of v's window, or its UNMAP (x None): the window names x there,
\* and the VM process is sent an unmap, which ends what is there, a private page too. The window
\* list of the part named before lets go of the window if no other page names it; the part named
\* now registers it, if no other page did, once (registerOnce), or anew each time (Window::place
\* forgets first, then finds none: TASK-602).
MapReq(v, o, x) ==
    LET old == win[v][o]
        others(p) == \E o2 \in Offsets \ {o} : win[v][o2] = p
        regs1 == IF old \in Parts /\ old # x /\ ~others(old)
                 THEN [regs EXCEPT ![old][v] = 0] ELSE regs
        regs2 == IF x \in Parts /\ ~others(x)
                 THEN [regs1 EXCEPT ![x][v] = IF R("registerOnce") THEN 1 ELSE @ + 1]
                 ELSE regs1
    IN /\ vst[v] = "running" /\ maps < MaxMaps
       /\ win' = [win EXCEPT ![v][o] = x]
       /\ pte' = [pte EXCEPT ![v][o] = None]
       /\ Send(v, Op("unmap", o, None, None))
       /\ regs' = regs2
       /\ maps' = maps + 1
       /\ UNCHANGED <<owner, held, slot, staged, quar, ev, vmVars, writes, evictions, freezes>>

\* A fault on page o, where v has nothing: the part the window names there, in memory, staged: its
\* slab mapped there; or zeros, the shared zeros page's, which is never evicted, no slab here.
Fault(v, o) ==
    LET p == win[v][o] IN
    /\ vst[v] = "running" /\ inst[v][o].k = "none" /\ pte[v][o] = None /\ cp[v] = Nothing
    /\ IF p = Zero THEN
          /\ Send(v, Op("map", o, None, Zero))
          /\ pte' = [pte EXCEPT ![v][o] = Zero]
          /\ UNCHANGED staged
       ELSE
          /\ p \in Parts /\ slot[p].st = "present"
          /\ Send(v, Op("map", o, slot[p].s, p))
          /\ pte' = [pte EXCEPT ![v][o] = slot[p].s]
          /\ staged' = [staged EXCEPT ![slot[p].s] = @ \cup {v}]
    /\ UNCHANGED <<owner, held, slot, regs, quar, ev, win, vmVars, countVars>>

\* A page-in of p, which a window names: its slab, free, filled, its slot's.
Fill(p) ==
    LET s == SlabOf(p) IN
    /\ slot[p] = Absent /\ owner[s] = Free
    /\ \E v \in VMs, o \in Offsets : win[v][o] = p
    /\ owner' = [owner EXCEPT ![s] = [k |-> "part", p |-> p]]
    /\ held' = [held EXCEPT ![s] = p]
    /\ slot' = [slot EXCEPT ![p] = [st |-> "present", s |-> s]]
    /\ staged' = [staged EXCEPT ![s] = {}]
    /\ UNCHANGED <<regs, quar, ev, winVars, vmVars, countVars>>

\* Eviction thread e takes p's slab with a token of its own: every window page staged it to be
\* sent an evict.
EvStart(e, p) ==
    LET s == slot[p].s IN
    /\ ev[e] = Idle /\ evictions < MaxEvictions
    /\ slot[p].st = "present"
    /\ slot' = [slot EXCEPT ![p] = [st |-> "evicting", s |-> s, e |-> e]]
    /\ owner' = [owner EXCEPT ![s] = [k |-> "token", e |-> e]]
    /\ ev' = [ev EXCEPT ![e] = [pc |-> "unmap", s |-> s, p |-> p,
                                todo |-> {vo \in VMs \X Offsets : pte[vo[1]][vo[2]] = s}]]
    /\ evictions' = evictions + 1
    /\ UNCHANGED <<held, staged, regs, quar, winVars, vmVars, maps, writes, freezes>>

\* It sends a window page staged the slab still an evict (a host's change); then, none left, it
\* frees.
EvUnmap(e) ==
    /\ ev[e].pc = "unmap"
    /\ IF ev[e].todo = {} THEN
          /\ ev' = [ev EXCEPT ![e].pc = "free"]
          /\ UNCHANGED <<pte, q, sent>>
       ELSE \E vo \in ev[e].todo :
          /\ ev' = [ev EXCEPT ![e].todo = @ \ {vo}]
          /\ IF pte[vo[1]][vo[2]] = ev[e].s /\ vst[vo[1]] # "dead" THEN
                /\ Send(vo[1], Op("evict", vo[2], ev[e].s, ev[e].p))
                /\ pte' = [pte EXCEPT ![vo[1]][vo[2]] = None]
             ELSE UNCHANGED <<pte, q, sent>>
    /\ UNCHANGED <<owner, held, slot, staged, regs, quar, win, vmVars, countVars>>

\* It frees the slab its token took (ownTokens), or, without the rule, every slab a token took
\* (cache.rs's reaping of every Evicting slot): each quarantined, waiting for the last operation
\* sent so far to each VM process of its scope alive, its part absent.
EvFree(e) ==
    LET freed == IF R("ownTokens") THEN {ev[e].s}
                 ELSE {s \in Slabs : owner[s].k = "token"}
        waits(s) == {<<v, sent[v]>> : v \in {v \in Scope(s) : vst[v] # "dead"}}
    IN /\ ev[e].pc = "free"
       /\ owner' = [s \in Slabs |-> IF s \in freed THEN Quarantined ELSE owner[s]]
       /\ quar' = quar \cup {[s |-> s, waits |-> waits(s)] : s \in freed}
       /\ slot' = [p \in Parts |-> IF slot[p].st # "absent" /\ slot[p].s \in freed THEN Absent
                                  ELSE slot[p]]
       /\ ev' = [ev EXCEPT ![e] = Idle]
       /\ UNCHANGED <<held, staged, regs, winVars, vmVars, countVars>>

\* A quarantined slab every VM process it waits for acknowledged, or died, gives up: free, and
\* punched or not.
Release(qe) ==
    /\ \A w \in qe.waits : acked[w[1]] >= w[2] \/ vst[w[1]] = "dead"
    /\ quar' = quar \ {qe}
    /\ owner' = [owner EXCEPT ![qe.s] = Free]
    /\ \E punch \in IF "punch" \in Can THEN BOOLEAN ELSE {FALSE} :
          held' = IF punch THEN [held EXCEPT ![qe.s] = Zero] ELSE held
    /\ staged' = [staged EXCEPT ![qe.s] = {}]
    /\ UNCHANGED <<slot, regs, ev, winVars, vmVars, countVars>>

\* The watchdog kills a VM process that stopped answering (SIGKILL, then its pidfd readable): its
\* window gone, nothing of it maps anything; its waits are let go of then.
Kill(v) ==
    /\ R("watchdog") /\ vst[v] = "frozen"
    /\ vst' = [vst EXCEPT ![v] = "dead"]
    /\ inst' = [inst EXCEPT ![v] = [o \in Offsets |-> Nothing]]
    /\ wrote' = [wrote EXCEPT ![v] = [o \in Offsets |-> 0]]
    /\ cp' = [cp EXCEPT ![v] = Nothing]
    /\ q' = [q EXCEPT ![v] = <<>>]
    /\ win' = [win EXCEPT ![v] = [o \in Offsets |-> None]]
    /\ pte' = [pte EXCEPT ![v] = [o \in Offsets |-> None]]
    /\ regs' = [p \in Parts |-> [regs[p] EXCEPT ![v] = 0]]
    /\ UNCHANGED <<owner, held, slot, staged, quar, ev, sent, acked, countVars>>

\* Without killBeforeLetGo: the daemon lets go of a VM process that stopped answering, its socket
\* shut, what it was sent lost, every wait for it let go of, though it lives.
LetGo(v) ==
    /\ ~R("killBeforeLetGo") /\ vst[v] = "frozen"
    /\ acked' = [acked EXCEPT ![v] = sent[v]]
    /\ q' = [q EXCEPT ![v] = <<>>]
    /\ UNCHANGED <<cacheVars, win, pte, sent, inst, wrote, vst, cp, countVars>>

-----------------------------------------------------------------------------
(* The VM processes. *)

\* Makes the next operation it was sent, and acknowledges it: a map stages the slab, as the part,
\* or zeros;
\* an unmap (a guest's MAP or UNMAP) ends what is there, a private page and its writes too; an
\* evict takes the slab away, but a private page stays (evictionKeepsPrivate).
Receive(v) ==
    LET m == Head(q[v])
        o == m.o
    IN /\ vst[v] = "running" /\ q[v] # <<>>
       /\ CASE m.op = "map" ->
                 /\ inst' = [inst EXCEPT ![v][o] = IF m.p = Zero THEN [k |-> "zeros"]
                                                  ELSE [k |-> "slab", s |-> m.s, p |-> m.p]]
                 /\ UNCHANGED wrote
            [] m.op = "unmap" ->
                 /\ inst' = [inst EXCEPT ![v][o] = Nothing]
                 /\ wrote' = [wrote EXCEPT ![v][o] = 0]
            [] m.op = "evict" ->
                 /\ inst' = IF inst[v][o].k = "priv" /\ R("evictionKeepsPrivate") THEN inst
                            ELSE [inst EXCEPT ![v][o] = Nothing]
                 /\ UNCHANGED wrote
       /\ acked' = [acked EXCEPT ![v] = m.id]
       /\ q' = [q EXCEPT ![v] = Tail(@)]
       /\ UNCHANGED <<cacheVars, win, pte, sent, vst, cp, countVars>>

\* A guest's write to page o, the VM process copying on write, within private_max: to its private
\* page; or, staged a slab, the slab's bytes copied, as the part it was staged as, under the
\* window's lock (copyUnderLock), else looked at first; or zeros.
Write(v, o) ==
    LET i == inst[v][o] IN
    /\ vst[v] = "running" /\ writes < MaxWrites /\ cp[v] = Nothing
    /\ i.k \in {"slab", "zeros", "priv"}
    /\ i.k = "priv" \/ Cardinality(Private(v)) < PrivateMax
    /\ writes' = writes + 1
    /\ IF i.k = "slab" /\ ~R("copyUnderLock") THEN
          /\ cp' = [cp EXCEPT ![v] = [k |-> "look", o |-> o, s |-> i.s, named |-> i.p]]
          /\ UNCHANGED <<inst, wrote>>
       ELSE
          /\ inst' = [inst EXCEPT ![v][o] =
                         CASE i.k = "priv" -> [i EXCEPT !.w = writes + 1]
                           [] i.k = "slab" -> [k |-> "priv", base |-> held[i.s], named |-> i.p,
                                               w |-> writes + 1]
                           [] i.k = "zeros" -> [k |-> "priv", base |-> Zero, named |-> Zero,
                                                w |-> writes + 1]]
          /\ wrote' = [wrote EXCEPT ![v][o] = writes + 1]
          /\ UNCHANGED cp
    /\ UNCHANGED <<cacheVars, winVars, acked, vst, maps, evictions, freezes>>

\* Without copyUnderLock: the copy of what the write looked at, the slab's bytes as they are now.
Copy(v) ==
    LET c == cp[v] IN
    /\ vst[v] = "running" /\ c.k = "look"
    /\ inst' = [inst EXCEPT ![v][c.o] = [k |-> "priv", base |-> held[c.s], named |-> c.named,
                                         w |-> writes]]
    /\ wrote' = [wrote EXCEPT ![v][c.o] = writes]
    /\ cp' = [cp EXCEPT ![v] = Nothing]
    /\ UNCHANGED <<cacheVars, winVars, acked, vst, countVars>>

Freeze(v) ==
    /\ "freeze" \in Can /\ freezes < MaxFreezes /\ vst[v] = "running"
    /\ vst' = [vst EXCEPT ![v] = "frozen"]
    /\ freezes' = freezes + 1
    /\ UNCHANGED <<cacheVars, winVars, inst, acked, wrote, cp, maps, writes, evictions>>

Thaw(v) ==
    /\ vst[v] = "frozen"
    /\ vst' = [vst EXCEPT ![v] = "running"]
    /\ UNCHANGED <<cacheVars, winVars, inst, acked, wrote, cp, countVars>>

-----------------------------------------------------------------------------

Init ==
    /\ owner = [s \in Slabs |-> Free]
    /\ held = [s \in Slabs |-> Zero]
    /\ slot = [p \in Parts |-> Absent]
    /\ staged = [s \in Slabs |-> {}]
    /\ regs = [p \in Parts |-> [v \in VMs |-> 0]]
    /\ quar = {}
    /\ ev = [e \in Evictors |-> Idle]
    /\ win = [v \in VMs |-> [o \in Offsets |-> None]]
    /\ pte = [v \in VMs |-> [o \in Offsets |-> None]]
    /\ q = [v \in VMs |-> <<>>]
    /\ sent = [v \in VMs |-> 0]
    /\ inst = [v \in VMs |-> [o \in Offsets |-> Nothing]]
    /\ acked = [v \in VMs |-> 0]
    /\ wrote = [v \in VMs |-> [o \in Offsets |-> 0]]
    /\ vst = [v \in VMs |-> "running"]
    /\ cp = [v \in VMs |-> Nothing]
    /\ maps = 0 /\ writes = 0 /\ evictions = 0 /\ freezes = 0

DaemonNext ==
    \/ \E v \in VMs, o \in Offsets :
          \/ \E x \in Mappable(v) \cup {None} \cup (IF "zeros" \in Can THEN {Zero} ELSE {}) :
                MapReq(v, o, x)
          \/ Fault(v, o)
    \/ \E p \in Parts : Fill(p)
    \/ \E e \in Evictors : \/ \E p \in Parts : EvStart(e, p)
                           \/ EvUnmap(e) \/ EvFree(e)
    \/ \E qe \in quar : Release(qe)
    \/ \E v \in VMs : Kill(v) \/ LetGo(v)

VMNext ==
    \E v \in VMs :
        \/ Receive(v) \/ Copy(v) \/ Freeze(v) \/ Thaw(v)
        \/ \E o \in Offsets : Write(v, o)

Next == DaemonNext \/ VMNext

Spec == Init /\ [][Next]_vars

\* With the VM processes making what they are sent, the quarantine releasing, the eviction
\* threads going on, and the watchdog killing a VM process frozen.
Fairness ==
    /\ \A v \in VMs : WF_vars(Receive(v)) /\ WF_vars(Kill(v))
    /\ \A e \in Evictors : WF_vars(EvUnmap(e)) /\ WF_vars(EvFree(e))
    /\ WF_vars(\E qe \in quar : Release(qe))

LiveSpec == Spec /\ Fairness

-----------------------------------------------------------------------------
(* What must hold. *)

\* No VM process, frozen or not, maps a slab that no longer holds the part it was staged as: freed
\* and punched, or filled with another part. Its guest would read it through KVM, with no fault.
NoStaleInstall ==
    \A v \in VMs, o \in Offsets :
        vst[v] # "dead" /\ inst[v][o].k = "slab" => held[inst[v][o].s] = inst[v][o].p

\* A private page holds the bytes of the page the window named as the write began.
NoStaleCopy ==
    \A v \in VMs, o \in Offsets :
        inst[v][o].k = "priv" => inst[v][o].base = inst[v][o].named

\* No guest's write is lost but to its own MAP or UNMAP over it, or its VM process's death.
NoLostWrite ==
    \A v \in VMs, o \in Offsets :
        wrote[v][o] # 0 => inst[v][o].k = "priv" /\ inst[v][o].w = wrote[v][o]

\* A VM process maps slabs of the arenas it holds alone: its tenant's, and the default.
ArenasHeld ==
    \A v \in VMs, o \in Offsets :
        inst[v][o].k = "slab" => SlabArena(inst[v][o].s) \in Holds(v)

\* A quarantined slab waits for VM processes it was staged to alone: tenant B's memory never waits
\* on tenant A's VM processes, nor a public part's on those that never mapped it.
ScopedWaits == \A qe \in quar : \A w \in qe.waits : w[1] \in staged[qe.s]

\* A window is on a part's list once.
WindowsOnce == \A p \in Parts, v \in VMs : regs[p][v] <= 1

\* At most private_max private pages a window.
PrivateWithinMax == \A v \in VMs : Cardinality(Private(v)) <= PrivateMax

\* Each slab has one owner, which knows it: free; a part present's slot; an eviction's token,
\* that eviction at work on it; or the quarantine, once. None leaked, none owned twice.
OneOwner ==
    /\ \A s \in Slabs :
          \/ owner[s] = Free /\ ~\E qe \in quar : qe.s = s
          \/ owner[s] = Quarantined /\ Cardinality({qe \in quar : qe.s = s}) = 1
          \/ owner[s].k = "part" /\ slot[owner[s].p] = [st |-> "present", s |-> s]
          \/ /\ owner[s].k = "token" /\ ev[owner[s].e].pc # "idle" /\ ev[owner[s].e].s = s
             /\ slot[ev[owner[s].e].p] = [st |-> "evicting", s |-> s, e |-> owner[s].e]
    /\ \A p \in Parts : slot[p].st # "absent" => owner[slot[p].s].k \in {"part", "token"}

\* Every slab quarantined is freed in time, whatever VM process freezes (PartSlabsLive).
QuarantineDrains == \A s \in Slabs : owner[s] = Quarantined ~> owner[s] = Free
=============================================================================
