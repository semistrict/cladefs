---------------------------- MODULE PlacedParts ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                *)
(* Use of this source code is governed by a BSD-style license that can be  *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Parts of the extent cache placed by the lists of cladefsd's memory      *)
(* backend (board 14629, docs/research/guest-ram-mapping-list.md): what    *)
(* the daemon does with a part a VM's list names, from the list made to    *)
(* the page installed, through evictions and refills, and the VM's own     *)
(* copy on write, the kernel's.                                            *)
(*                                                                         *)
(* A list entry names, for a page of the VM's memory, a part and the slab  *)
(* of the arena it maps there (MAP_PRIVATE of the arena's object, at the   *)
(* slab's place, which the entry keeps). The VM's first touch of the page  *)
(* faults to the daemon, which installs it: the slab's page, shared, if    *)
(* the part is present in that slab (UFFDIO_CONTINUE); else a private copy *)
(* of the part's bytes from wherever it is present (UFFDIO_COPY). A store  *)
(* to a shared page copies it in the kernel, a private page of the VM's.   *)
(* Punching a slab (an eviction's, or a free's) drops its pages from every *)
(* mapping, private pages staying.                                         *)
(*                                                                         *)
(* A part placed by a list has a home: the slab it is paged in to while    *)
(* any list places it. An eviction keeps it, punched; a refill fills it    *)
(* again. A part no list places is evicted as any: its slab freed, which   *)
(* another part may be paged in to.                                        *)
(*                                                                         *)
(* The rules, each a member of Rules; specs/placed-parts/mutations removes *)
(* one each, and must find the violation it prevents:                      *)
(*                                                                         *)
(* - keepHome (NoKeepHome): an eviction of a part a list places keeps its  *)
(*   slab as its home, which a refill fills. Without it, it frees the slab *)
(*   as any, a refill goes elsewhere, and the VM's next touch takes a copy *)
(*   rather than share (NoCopies): no defect, but no sharing either.       *)
(* - checkUnderLock (NoCheckUnderLock): the daemon decides how to install  *)
(*   a page and installs it under the part's lock, which an eviction       *)
(*   takes. Without it, an eviction and a refill of another part come      *)
(*   between: it installs the slab's page, another part's (NoStaleInstall).*)
(* - punchBeforePublish (NoPunchBeforePublish): an eviction punches the    *)
(*   slab before it makes it the part's home again, under the part's lock. *)
(*   Without it, a refill fills the home first, and the punch then takes   *)
(*   the part's bytes from under it, present (NoStaleInstall).             *)
(*                                                                         *)
(* Not modelled: the allocator's choice of a slab, but that a free one may *)
(* be any part's; small slabs' runs, owned whole as a slab is; relief's    *)
(* choice of what to evict; VM processes' stages (specs/part-slabs).       *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Parts,          \* the extents' parts
    Slabs,          \* the arena's slabs
    Offsets,        \* the VM's pages
    MaxEvictions,   \* evictions
    MaxWrites,      \* the guest's stores
    MaxRemaps,      \* entries mapped anew (the list made again over a page)
    Rules           \* the rules in force (all, but in a mutation)

AllRules == {"keepHome", "checkUnderLock", "punchBeforePublish"}
ASSUME Rules \subseteq AllRules

R(rule) == rule \in Rules

None == "none"
Zero == "zero"

VARIABLES
    slot,       \* slot[p]: [st: "absent"], or [st: "present", s]
    home,       \* home[p]: the slab it is placed in while its slot holds none, or None
    pins,       \* pins[p]: how many entries place it
    owner,      \* owner[s]: the part whose slab it is (present or home), or None (free)
    held,       \* held[s]: the part whose bytes it holds, or Zero (punched)
    entry,      \* entry[o]: [p, s], the list's entry at page o, or NoEntry
    pte,        \* pte[o]: [k: "none"], [k: "shared", s], or [k: "priv", base, copied, w]
    wrote,      \* wrote[o]: the guest's last store there, 0 if none since its entry was made
    look,       \* look[o]: the install decided, not made (without checkUnderLock): [k: "none"],
                \* or [k: "shared", s]
    punch,      \* punch[p]: a home published, its punch to come (without punchBeforePublish):
                \* the slab, or None
    evictions, writes, remaps

vars == <<slot, home, pins, owner, held, entry, pte, wrote, look, punch, evictions, writes,
          remaps>>

Absent == [st |-> "absent"]
NoEntry == [p |-> None, s |-> None]
NoPte == [k |-> "none"]
NoLook == [k |-> "none"]

Init ==
    /\ slot = [p \in Parts |-> Absent]
    /\ home = [p \in Parts |-> None]
    /\ pins = [p \in Parts |-> 0]
    /\ owner = [s \in Slabs |-> None]
    /\ held = [s \in Slabs |-> Zero]
    /\ entry = [o \in Offsets |-> NoEntry]
    /\ pte = [o \in Offsets |-> NoPte]
    /\ wrote = [o \in Offsets |-> 0]
    /\ look = [o \in Offsets |-> NoLook]
    /\ punch = [p \in Parts |-> None]
    /\ evictions = 0 /\ writes = 0 /\ remaps = 0

\* Where part p is paged in: its slab, present; its home; or a free slab made its home now.
Places(p) ==
    IF slot[p].st = "present" THEN {slot[p].s}
    ELSE IF home[p] # None THEN {home[p]}
    ELSE {s \in Slabs : owner[s] = None}

(* The list made over page o, with nothing there: an entry placing p where it is paged in. *)
Place(o, p) ==
    /\ entry[o] = NoEntry
    /\ \E s \in Places(p) :
          /\ entry' = [entry EXCEPT ![o] = [p |-> p, s |-> s]]
          /\ pins' = [pins EXCEPT ![p] = @ + 1]
          /\ IF slot[p].st = "absent" /\ home[p] = None
             THEN /\ home' = [home EXCEPT ![p] = s]
                  /\ owner' = [owner EXCEPT ![s] = p]
             ELSE UNCHANGED <<home, owner>>
    /\ UNCHANGED <<slot, held, pte, wrote, look, punch, evictions, writes, remaps>>

(* The list made anew over page o: its entry ends (MAP_FIXED dropping what the VM had there, its *)
(* private page too, as the list's maker means); the part placed by no entry gives up its home.  *)
Remap(o) ==
    LET p == entry[o].p IN
    /\ entry[o] # NoEntry /\ remaps < MaxRemaps /\ look[o] = NoLook
    /\ entry' = [entry EXCEPT ![o] = NoEntry]
    /\ pte' = [pte EXCEPT ![o] = NoPte]
    /\ wrote' = [wrote EXCEPT ![o] = 0]
    /\ pins' = [pins EXCEPT ![p] = @ - 1]
    /\ IF pins[p] = 1 /\ home[p] # None /\ punch[p] = None
       THEN /\ owner' = [owner EXCEPT ![home[p]] = None]
            /\ held' = [held EXCEPT ![home[p]] = Zero]
            /\ home' = [home EXCEPT ![p] = None]
       ELSE UNCHANGED <<owner, held, home>>
    /\ remaps' = remaps + 1
    /\ UNCHANGED <<slot, look, punch, evictions, writes>>

(* A page-in of p, which an entry names: into its home, populated, or a free slab.               *)
Fill(p) ==
    /\ slot[p] = Absent /\ \E o \in Offsets : entry[o] # NoEntry /\ entry[o].p = p
    /\ \E s \in IF home[p] # None THEN {home[p]} ELSE {s \in Slabs : owner[s] = None} :
          /\ slot' = [slot EXCEPT ![p] = [st |-> "present", s |-> s]]
          /\ home' = [home EXCEPT ![p] = None]
          /\ owner' = [owner EXCEPT ![s] = p]
          /\ held' = [held EXCEPT ![s] = p]
    /\ UNCHANGED <<pins, entry, pte, wrote, look, punch, evictions, writes, remaps>>

\* Punching slab s: its bytes gone, and its pages from every mapping, private pages staying.
Punched(s) ==
    /\ held' = [held EXCEPT ![s] = Zero]
    /\ pte' = [o \in Offsets |-> IF pte[o].k = "shared" /\ pte[o].s = s THEN NoPte ELSE pte[o]]

(* An eviction of p, present: a part entries place keeps its slab as its home, punched           *)
(* (keepHome), before it is published (punchBeforePublish) or after; any other frees its slab,  *)
(* punched.                                                                                      *)
Evict(p) ==
    LET s == slot[p].s IN
    /\ slot[p].st = "present" /\ evictions < MaxEvictions /\ punch[p] = None
    /\ slot' = [slot EXCEPT ![p] = Absent]
    /\ IF pins[p] > 0 /\ R("keepHome") THEN
          /\ home' = [home EXCEPT ![p] = s]
          /\ UNCHANGED owner
          /\ IF R("punchBeforePublish")
             THEN /\ Punched(s)
                  /\ UNCHANGED punch
             ELSE /\ punch' = [punch EXCEPT ![p] = s]
                  /\ UNCHANGED <<held, pte>>
       ELSE
          /\ owner' = [owner EXCEPT ![s] = None]
          /\ UNCHANGED <<home, punch>>
          /\ Punched(s)
    /\ evictions' = evictions + 1
    /\ UNCHANGED <<pins, entry, wrote, look, writes, remaps>>

(* Without punchBeforePublish: the punch of a home published before it.                          *)
PunchLate(p) ==
    /\ punch[p] # None
    /\ Punched(punch[p])
    /\ punch' = [punch EXCEPT ![p] = None]
    /\ UNCHANGED <<slot, home, pins, owner, entry, wrote, look, evictions, writes, remaps>>

(* The VM's first touch of page o, which faults to the daemon: the slab's page, shared, if the    *)
(* part is present in the entry's slab; else a copy of its bytes from where it is present. Under *)
(* the part's lock, at once (checkUnderLock); or decided, then installed later.                  *)
Fault(o) ==
    LET p == entry[o].p
        s == entry[o].s
    IN /\ entry[o] # NoEntry /\ pte[o] = NoPte /\ look[o] = NoLook /\ slot[p].st = "present"
       /\ IF slot[p].s = s THEN
             IF R("checkUnderLock")
             THEN /\ pte' = [pte EXCEPT ![o] = [k |-> "shared", s |-> s]]
                  /\ UNCHANGED look
             ELSE /\ look' = [look EXCEPT ![o] = [k |-> "shared", s |-> s]]
                  /\ UNCHANGED pte
          ELSE /\ pte' = [pte EXCEPT ![o] = [k |-> "priv", base |-> held[slot[p].s],
                                             copied |-> TRUE, w |-> 0]]
               /\ UNCHANGED look
       /\ UNCHANGED <<slot, home, pins, owner, held, entry, wrote, punch, evictions, writes,
                      remaps>>

(* Without checkUnderLock: the install decided, made.                                            *)
Install(o) ==
    /\ look[o] # NoLook /\ pte[o] = NoPte
    /\ pte' = [pte EXCEPT ![o] = [k |-> "shared", s |-> look[o].s]]
    /\ look' = [look EXCEPT ![o] = NoLook]
    /\ UNCHANGED <<slot, home, pins, owner, held, entry, wrote, punch, evictions, writes, remaps>>

(* The guest's store to page o, installed: a shared page copied in the kernel, private.          *)
Write(o) ==
    /\ pte[o] # NoPte /\ writes < MaxWrites
    /\ pte' = [pte EXCEPT ![o] =
                 IF @.k = "shared"
                 THEN [k |-> "priv", base |-> held[@.s], copied |-> FALSE, w |-> writes + 1]
                 ELSE [@ EXCEPT !.w = writes + 1]]
    /\ wrote' = [wrote EXCEPT ![o] = writes + 1]
    /\ writes' = writes + 1
    /\ UNCHANGED <<slot, home, pins, owner, held, entry, look, punch, evictions, remaps>>

Next ==
    \/ \E o \in Offsets, p \in Parts : Place(o, p)
    \/ \E o \in Offsets : Remap(o) \/ Fault(o) \/ Install(o) \/ Write(o)
    \/ \E p \in Parts : Fill(p) \/ Evict(p) \/ PunchLate(p)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(* What must hold. *)

\* A page the VM shares is the slab's page of the part its entry names: no other part's bytes,
\* nor a hole's.
NoStaleInstall ==
    \A o \in Offsets : pte[o].k = "shared" => held[pte[o].s] = entry[o].p

\* A private page holds the part's bytes, copied or made private by a store.
NoStaleCopy ==
    \A o \in Offsets : pte[o].k = "priv" => pte[o].base = entry[o].p

\* The guest's store stays in its private page until its entry is made anew.
NoLostWrite ==
    \A o \in Offsets : wrote[o] > 0 => pte[o].k = "priv" /\ pte[o].w = wrote[o]

\* A part an entry places is shared, never copied: its slab never changes under its entries.
NoCopies ==
    \A o \in Offsets : ~(pte[o].k = "priv" /\ pte[o].copied)

\* A slab is one part's at most, as present or as home.
OneOwner ==
    \A p \in Parts : \A s \in Slabs :
        (slot[p].st = "present" /\ slot[p].s = s) \/ home[p] = s => owner[s] = p

TypeOK ==
    /\ \A p \in Parts : pins[p] \in Nat
    /\ \A s \in Slabs : held[s] \in Parts \cup {Zero}
=============================================================================
