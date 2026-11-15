------------------------------ MODULE DaxCow ------------------------------
(***************************************************************************)
(* Copyright 2026 The fxdax Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Copy-on-write DAX (backlog doc-13, TASK-164): the guest Fxfs's window  *)
(* maps sealed extents, or zeros, read-only; a window page the guest      *)
(* dirties, the host makes the VM's own, a private copy of it at the same *)
(* address (the device's DAX feature), before any store reaches it.       *)
(* Fxfs allocates the blocks at writeback, as the page cache's path       *)
(* does, and commits after their bytes: guest/linux/fs/fxfs/dax.c, DAX    *)
(* as it is since writable DAX went (dead-ends/writable-dax.md).          *)
(*                                                                         *)
(* A file of a few pages (Pages), tasks storing to them (Storers), tasks  *)
(* doing the rest (Others), and the host:                                  *)
(*                                                                         *)
(* - a storer: a fault, whose iomap_begin (IOMAP_WRITE) has the host make *)
(*   the page private (PRIVATE), puts it in the file's dirty set, counts  *)
(*   it private, and then maps it writable (its PTE, before its           *)
(*   iomap_end); the fault holds the page's DAX entry past its iomap_end  *)
(*   until it returns, after which the store, through the PTE, if it is   *)
(*   writable still, or a fault again. Or a write(2), whose copy into the *)
(*   window is between its iomap_begin and iomap_end, holding the file's  *)
(*   semaphore shared, and which may have pages made private ahead of it, *)
(*   clean (dax.c's private_before and write-ahead). Before either, a page*)
(*   not counted waits, holding nothing, for the budget to have room for  *)
(*   it (private_max).                                                     *)
(* - a task doing the rest: writeback (->writepages, the budget's), fsync *)
(*   (writebacks, then the seal that makes them durable), rebinding (each *)
(*   of a set of private pages MAPped back to its extent's page, freeing  *)
(*   it), and punching a page (truncate). Each holds the file's writeback *)
(*   lock (dax.c's wb_lock), which keeps them apart, before the semaphore.*)
(*   Writeback swaps the dirty set out for an empty one (the writeback    *)
(*   set) with the semaphore held exclusively, lets it go, write-protects *)
(*   the set's PTEs, a page at a time (by the page tables), copies the    *)
(*   pages from the window, WRITEs them to a new extent, commits the      *)
(*   transaction naming it, and keeps the pages until the seal after that *)
(*   commit, which seals the extent; a seal between the WRITE and the     *)
(*   commit seals it before the commit names it. The extent that held a   *)
(*   page's block before dies at that seal (Fxfs frees it after the       *)
(*   flush), and a window page mapping it reads zeros from then on.       *)
(* - the host: copies on write, at a PRIVATE or at a store to a page not  *)
(*   private, refusing either past private_max, which stops the VM; seals *)
(*   (a COMMIT: the journal's, or any fsync's) at any time; refuses a MAP *)
(*   of an open extent (EPERM); and evicts a page of a sealed extent from *)
(*   the window, as its cache does, at any time. The VM process installs *)
(*   what the window names as the guest first touches it (TASK-276.6): a  *)
(*   MAP names pages, and a load's fault installs the page (`inst`), a    *)
(*   copy of what the window maps; a store's first touch, a PRIVATE,      *)
(*   copies what the window names into a private page, nothing installed.*)
(*   And the kernel, which write-protects a writable PTE as it cleans the *)
(*   page's DAX entry (dax_writeback_one, writepages), but one a fault    *)
(*   holds.                                                                *)
(* - a loader: the guest's loads of the file's pages, each through what   *)
(*   the VM process installed, or a fault that installs it first.         *)
(*                                                                         *)
(* Data are versions: each store writes the next, a punch writes zeros as *)
(* the next, and a page's copy holds every store before it. A store is to *)
(* part of a page: one the host copies a freed block's zeros for leaves   *)
(* the rest of the page lost (Torn). `durable` is what a crash leaves:    *)
(* Fxfs's file as of the last seal.                                        *)
(*                                                                         *)
(* The rules, each a member of Rules; specs/dax-cow/mutations removes one *)
(* each, and must find the violation it prevents:                          *)
(*                                                                         *)
(* - privateAtIomap: the iomap that dirties a page has the host make it   *)
(*   private first (PRIVATE), so that dirty implies private (the invariant*)
(*   DirtyImpliesPrivate), and no store ever has the host copy a window   *)
(*   page: the store of a fault comes after its iomap_end, maybe after a  *)
(*   writeback took the page, moved its block, and the seal after freed   *)
(*   the old one, whose zeros the host would copy (guest/sim's seed 2802; *)
(*   TASK-164.9, dax.c's fxfs_dax_cow_dirty).                              *)
(* - protectByPageTables: writeback write-protects every PTE of its pages, *)
(*   by the page tables; dax_writeback_mapping_range, by the DAX entries, *)
(*   passes by an entry a fault holds past its iomap_end, whose PTE stays *)
(*   writable, and a store through it after the writeback's copy is in no *)
(*   set (guest/sim's seed 22).                                            *)
(* - swapFirst: writeback swaps the dirty set before it write-protects, so*)
(*   that a store after the write-protect faults into the new dirty set.  *)
(* - swapExclusive: the swap holds the semaphore exclusively, so that no  *)
(*   iomap between its iomap_begin, which dirtied its page, and its copy  *)
(*   or PTE straddles it: a write(2) copying after the swap and the       *)
(*   writeback's copy loses its bytes. Doc-13 has the swap under the      *)
(*   semaphore shared, which this model finds (NoSwapExclusive.cfg): a     *)
(*   page put in the dirty set at iomap_end, after the copy, as the page  *)
(*   cache's write_end dirties a folio, would do as well.                  *)
(* - rebindExclusive: a rebind holds the semaphore exclusively, so that no*)
(*   iomap dirties a page between its look and its MAPs.                   *)
(* - rebindAfterSeal: a rebind takes no page whose extent the seal after  *)
(*   its commit hasn't sealed.                                              *)
(* - keepUntilSealed: writeback keeps its pages in the set waiting for    *)
(*   that seal (`sealing`), so that a rebind finding a page in no set     *)
(*   finds its extent sealed. Dax.c keeps these two otherwise: a rebind   *)
(*   maps only what Fxfs says is sealed, and leaves a page private on the *)
(*   host's EPERM (fxfs_dax_cow_map), so guest/sim has no mutant of them; *)
(*   specs/dax-cow/trace checks that what it rebinds is what they allow.   *)
(* - countPrivate: iomap_begin counts its page private, which the budget  *)
(*   waits on, so that the guest never asks the host for a private page   *)
(*   past private_max.                                                     *)
(* - bytesFirst: writeback commits after its bytes are written (Fxfs's     *)
(*   fxfs_write_many), so that a crash shows old or new data, never what   *)
(*   a new extent held unwritten.                                           *)
(* - installAsNamed: a load's fault installs the page as the window names *)
(*   it then, its look and its install one step, under the lock the      *)
(*   window's changes take (dax::uffd's reinstall): installed as the fault *)
(*   found it, a change between, a punch's zeros, is undone for good.      *)
(* - changesTakeAway: a change of what the window maps at a page takes    *)
(*   away what the VM process installed there, for the next touch to      *)
(*   install anew (dax::uffd's take_away): a page left installed reads    *)
(*   what the window mapped before.                                         *)
(*                                                                         *)
(* Steps split as dax.c takes them (TASK-444.1, whose trace validation    *)
(* follows guest/sim's runs of dax.c through this spec,                   *)
(* specs/dax-cow/trace): the writeback lock where the semaphore downgraded *)
(* to the commit stood for it; a fault's PTE made writable after its      *)
(* iomap dirtied the page, a writeback's write-protect between finding it *)
(* not yet writable; the write-protect a page at a time, of the writeback *)
(* set alone; an fsync's writebacks, one or more, before its seal; a      *)
(* rebind's look at once at a set, then a MAP a page; an extent minted    *)
(* before a seal, sealed by it, though committed after. The kernel's DAX *)
(* entries are modelled only as a fault holds its page's (EntryHeld),     *)
(* which nothing of the protocol waits for but the kernel's cleaning of   *)
(* PTEs: a fault trylocks the semaphore, holding its entry, as dax.c's do *)
(* since TASK-79.12. Their dirty tags aren't: no rule reads them (dax.c   *)
(* cleans them for the kernel's sake alone, and a range's release, which  *)
(* needs them clean, is the kernel's, outside this model). Nor is a range *)
(* UNMAPped and MAPped again, which reclaim, dax.c's punches, truncates   *)
(* and clones do: the punch here is a page's MAP_ZEROS.                    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Pages,          \* the file's pages
    PrivateMax,     \* the device's private_max
    MaxStores,      \* the stores, faults' and write(2)s'
    MaxPunches,     \* the punches
    MaxLoads,       \* the loader's loads
    Storers,        \* the tasks storing
    Others,         \* the tasks doing the rest
    Rules           \* the protocol's rules in force

AllRules == {"privateAtIomap", "protectByPageTables", "swapFirst", "swapExclusive",
             "rebindExclusive", "rebindAfterSeal", "keepUntilSealed", "countPrivate",
             "bytesFirst", "installAsNamed", "changesTakeAway"}

ASSUME Rules \subseteq AllRules
ASSUME PrivateMax \in Nat \ {0}
ASSUME Storers \cap Others = {} /\ "none" \notin Storers \cup Others

\* What a new extent holds where nothing was written to it.
Garbage == -1
\* What a freed block reads as, through the window: zeros.
Zeroed == -2
\* A page the host copied a freed block's zeros for, then a store changed a part of.
Torn == -3

VARIABLES
    \* The host.
    win,        \* win[p]: the window's page: [kind: "extent", "zeros", "private", "doomed"
                \* (an extent's block Fxfs replaced, freed at the next seal) or "freed", data]
    inst,       \* inst[p]: what the VM process installed at p, a copy of a window page's data,
                \* or None: named alone, its next touch a fault
    refused,    \* whether the host refused a store or a PRIVATE, which stops the VM
    \* Fxfs.
    file,       \* file[p]: the file's committed block: [state: "hole", "open" or "sealed", data]
    durable,    \* durable[p]: what a crash shows: file[p]'s data as of the last seal
    \* What happened.
    latest,     \* latest[p]: the version of the last store or punch of p that completed
    next,       \* the next version
    errors,     \* what the host refused that it shouldn't have been asked
    \* The guest.
    pte,        \* pte[p]: the user's mapping of p: "none", "ro" or "rw"
    dirty,      \* the file's dirty set
    wb,         \* the writeback set, from the swap to the commit
    sealing,    \* the writeback set, from the commit to the seal (keepUntilSealed)
    counted,    \* the pages the guest counts private
    semX,       \* who holds the file's semaphore exclusively, or "none"
    semR,       \* who holds it shared
    wbLock,     \* who holds the file's writeback lock, or "none"
    \* The storers: each one's step, page, way (a fault's or a write(2)'s); the stores so far.
    spc, sPage, sWay, stores,
    \* The others: each one's step, operation, pages left to write-protect or rebind, and an
    \* fsync's floor; the writeback's copy of its pages, and its new extent, from its WRITE to
    \* its commit ("open", "sealed" or "none"); the punches so far.
    wpc, wOp, wLeft, floor, buf, minted, punches,
    \* The loader: its page, what its fault found there, and its loads so far.
    lpc, lPage, lSeen, loads

hostVars == <<win, inst, refused>>
fxfsVars == <<file, durable>>
ghostVars == <<latest, next, errors>>
guestVars == <<pte, dirty, wb, sealing, counted, semX, semR, wbLock>>
sVars == <<spc, sPage, sWay, stores>>
wVars == <<wpc, wOp, wLeft, floor, buf, minted, punches>>
lVars == <<lpc, lPage, lSeen, loads>>
vars == <<hostVars, fxfsVars, ghostVars, guestVars, sVars, wVars, lVars>>

\* What the VM process installed at no page: it is named alone.
None == -4

\* What a change of the window's pages, `changed`, leaves the VM process installed: nothing there
\* (changesTakeAway), for the next touch to install as the window names the page then.
TakenAway(changed) ==
    IF "changesTakeAway" \in Rules THEN [p \in Pages |-> IF p \in changed THEN None ELSE inst[p]]
    ELSE inst

\* The pages the host holds private.
HostPrivate == Cardinality({p \in Pages : win[p].kind = "private"})

\* Whether the host takes a store to p: a private page's, or a copy within private_max.
CanStore(p) == win[p].kind = "private" \/ HostPrivate < PrivateMax

\* A store of the next version to part of p, through the window: the host copies on write, what
\* the window maps there (a freed block's zeros, the rest of the page lost), or refuses.
Store(p) ==
    IF CanStore(p)
    THEN /\ win' = [win EXCEPT ![p] = [kind |-> "private",
                                       data |-> IF win[p].kind = "freed" THEN Torn ELSE next]]
         /\ inst' = [inst EXCEPT ![p] = None]
         /\ latest' = [latest EXCEPT ![p] = next]
         /\ next' = next + 1
         /\ UNCHANGED refused
    ELSE /\ refused' = TRUE
         /\ UNCHANGED <<win, inst, latest, next>>

\* PRIVATE of p: the host copies it now, what the window maps there, within private_max, in place
\* of what the VM process installed, if anything (MadePrivateInst).
MadePrivate(p) ==
    IF win[p].kind = "private" THEN win[p]
    ELSE [kind |-> "private", data |-> win[p].data]

MadePrivateInst(p) == [inst EXCEPT ![p] = None]

\* Whether a storer's fault holds p's DAX entry: from before its iomap_begin until it returns.
EntryHeld(p) ==
    \E t \in Storers : sWay[t] = "fault" /\ sPage[t] = p /\
                       spc[t] \in {"lock", "begin", "map", "unlock", "entry"}

\* p's PTE write-protected by writeback: read-only if writable, by the page tables; or, by the DAX
\* entries, as dax_writeback_mapping_range does, unless a fault holds its entry.
WriteProtectedAt(p) ==
    IF pte[p] = "rw" /\ ("protectByPageTables" \in Rules \/ ~EntryHeld(p)) THEN "ro" ELSE pte[p]

\* The semaphore let go of by t, however held.
Released(t) ==
    /\ semX' = IF semX = t THEN "none" ELSE semX
    /\ semR' = semR \ {t}

\* What a seal changes: the open extents sealed, the extent a writeback minted, the commits so far
\* durable, the set waiting for it let go of, and the extents the commits replaced freed.
SealChanges ==
    \/ \E p \in Pages : file[p].state = "open" \/ win[p].kind = "doomed"
    \/ minted = "open"
    \/ sealing # {}
    \/ durable # [p \in Pages |-> file[p].data]

\* A seal: the open extents sealed, the commits so far durable, the writeback set let go of, and
\* the extents the commits replaced freed: a window page mapping one reads zeros.
SealEffect ==
    /\ file' = [p \in Pages |->
                   IF file[p].state = "open" THEN [file[p] EXCEPT !.state = "sealed"]
                   ELSE file[p]]
    /\ durable' = [p \in Pages |-> file[p].data]
    /\ sealing' = {}
    /\ minted' = IF minted = "open" THEN "sealed" ELSE minted
    /\ win' = [p \in Pages |-> IF win[p].kind = "doomed" THEN [kind |-> "freed", data |-> Zeroed]
                               ELSE win[p]]
    /\ inst' = TakenAway({p \in Pages : win[p].kind = "doomed"})

-----------------------------------------------------------------------------
(* A storer, t. *)

\* A store to page p: through its PTE if it is writable, or a fault; or a write(2).
SStartAt(t, p, way) ==
    /\ spc[t] = "idle" /\ stores < MaxStores
    /\ sPage' = [sPage EXCEPT ![t] = p]
    /\ sWay' = [sWay EXCEPT ![t] = way]
    /\ spc' = [spc EXCEPT ![t] = IF way = "fault" /\ pte[p] = "rw" THEN "store" ELSE "throttle"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, stores, wVars>>

SStart(t) == \E p \in Pages, way \in {"fault", "write"} : SStartAt(t, p, way)

\* The budget: a page not counted waits, holding nothing, until there is room for it.
SThrottle(t) ==
    /\ spc[t] = "throttle"
    /\ sPage[t] \in counted \/ Cardinality(counted) < PrivateMax
    /\ spc' = [spc EXCEPT ![t] = "lock"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sPage, sWay, stores, wVars>>

SLock(t) ==
    /\ spc[t] = "lock" /\ semX = "none"
    /\ semR' = semR \cup {t}
    /\ spc' = [spc EXCEPT ![t] = "begin"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, semX, wbLock,
                   sPage, sWay, stores, wVars>>

\* fxfs_dax_iomap_begin, IOMAP_WRITE (fxfs_dax_cow_dirty): the page made private on the host
\* (privateAtIomap), unless the host refuses, then dirty, counted private (countPrivate); a fault
\* maps it writable next (SMap), a write(2) copies.
SBegin(t) ==
    /\ spc[t] = "begin"
    /\ LET p == sPage[t] IN
       IF "privateAtIomap" \in Rules /\ ~CanStore(p)
       THEN /\ refused' = TRUE
            /\ spc' = [spc EXCEPT ![t] = "stopped"]
            /\ UNCHANGED <<win, inst, dirty, counted>>
       ELSE /\ win' = IF "privateAtIomap" \in Rules THEN [win EXCEPT ![p] = MadePrivate(p)]
                      ELSE win
            /\ inst' = IF "privateAtIomap" \in Rules THEN MadePrivateInst(p) ELSE inst
            /\ dirty' = dirty \cup {p}
            /\ counted' = IF "countPrivate" \in Rules THEN counted \cup {p} ELSE counted
            /\ spc' = [spc EXCEPT ![t] = IF sWay[t] = "fault" THEN "map" ELSE "copy"]
            /\ UNCHANGED refused
    /\ UNCHANGED <<fxfsVars, ghostVars, pte, wb, sealing, semX, semR, wbLock, sPage, sWay, stores,
                   wVars>>

\* A fault's PTE made writable, before its iomap_end: a writeback's write-protect may come
\* between its iomap_begin and this, which it then finds not yet writable.
SMap(t) ==
    /\ spc[t] = "map"
    /\ pte' = [pte EXCEPT ![sPage[t]] = "rw"]
    /\ spc' = [spc EXCEPT ![t] = "unlock"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, dirty, wb, sealing, counted, semX, semR, wbLock,
                   sPage, sWay, stores, wVars>>

\* A page p made private ahead of a write(2), clean: before its lock, holding the semaphore
\* shared (dax.c's private_before), or after its iomap dirtied its own (write-ahead). Of a page
\* of an extent, within the budget, as its PRIVATE and fxfs_dax_take_private have it.
SAheadAt(t, p) ==
    /\ spc[t] \in {"throttle", "copy"} /\ sWay[t] = "write" /\ semX = "none"
    /\ win[p].kind \in {"extent", "doomed"}
    /\ CanStore(p)
    /\ p \notin counted
    /\ "countPrivate" \in Rules => Cardinality(counted) < PrivateMax
    /\ win' = [win EXCEPT ![p] = MadePrivate(p)]
    /\ inst' = MadePrivateInst(p)
    /\ counted' = IF "countPrivate" \in Rules THEN counted \cup {p} ELSE counted
    /\ UNCHANGED <<refused, fxfsVars, ghostVars, pte, dirty, wb, sealing, semX, semR, wbLock,
                   sVars, wVars>>

SAhead(t) == \E p \in Pages : SAheadAt(t, p)

\* A write(2)'s copy into the window, between its iomap_begin and iomap_end.
SCopy(t) ==
    /\ spc[t] = "copy"
    /\ Store(sPage[t])
    /\ stores' = stores + 1
    /\ spc' = [spc EXCEPT ![t] = IF CanStore(sPage[t]) THEN "unlock" ELSE "stopped"]
    /\ UNCHANGED <<fxfsVars, errors, guestVars, sPage, sWay, wVars>>

\* fxfs_dax_iomap_end; a fault holds its page's DAX entry until it returns (SReturn).
SUnlock(t) ==
    /\ spc[t] = "unlock"
    /\ semR' = semR \ {t}
    /\ IF sWay[t] = "fault"
       THEN /\ spc' = [spc EXCEPT ![t] = "entry"]
            /\ UNCHANGED <<sPage, sWay>>
       ELSE /\ spc' = [spc EXCEPT ![t] = "idle"]
            /\ sPage' = [sPage EXCEPT ![t] = 0]
            /\ sWay' = [sWay EXCEPT ![t] = "none"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, semX, wbLock,
                   stores, wVars>>

\* The fault returns, its page's DAX entry let go of, and the store is retried.
SReturn(t) ==
    /\ spc[t] = "entry"
    /\ spc' = [spc EXCEPT ![t] = "store"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sPage, sWay, stores, wVars>>

\* The store through the PTE, writable still; or, write-protected or unmapped since, a fault again.
SStore(t) ==
    /\ spc[t] = "store"
    /\ IF pte[sPage[t]] = "rw"
       THEN /\ Store(sPage[t])
            /\ stores' = stores + 1
            /\ spc' = [spc EXCEPT ![t] = IF CanStore(sPage[t]) THEN "idle" ELSE "stopped"]
            /\ sPage' = [sPage EXCEPT ![t] = 0]
            /\ sWay' = [sWay EXCEPT ![t] = "none"]
       ELSE /\ spc' = [spc EXCEPT ![t] = "throttle"]
            /\ UNCHANGED <<win, inst, refused, latest, next, stores, sPage, sWay>>
    /\ UNCHANGED <<fxfsVars, errors, guestVars, wVars>>

\* A fault that finds its PTE writable, another storer's fault having made it so meanwhile: it
\* does nothing (mm's do_wp_page isn't reached), and the store is retried.
SResolved(t) ==
    /\ spc[t] = "throttle" /\ sWay[t] = "fault" /\ pte[sPage[t]] = "rw"
    /\ spc' = [spc EXCEPT ![t] = "store"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sPage, sWay, stores, wVars>>

StorerNext(t) ==
    SStart(t) \/ SThrottle(t) \/ SLock(t) \/ SBegin(t) \/ SMap(t) \/ SAhead(t) \/ SCopy(t)
    \/ SUnlock(t) \/ SReturn(t) \/ SStore(t) \/ SResolved(t)

-----------------------------------------------------------------------------
(* A task doing the rest, t: writeback, fsync, rebinding, punching. *)

WStartOp(t, op) ==
    /\ wpc[t] = "idle"
    /\ op = "punch" => punches < MaxPunches
    /\ op = "rebind" => HostPrivate > 0
    /\ wOp' = [wOp EXCEPT ![t] = op]
    \* An fsync keeps every store that completed before it began.
    /\ floor' = [floor EXCEPT ![t] = IF op = "fsync" THEN latest ELSE floor[t]]
    /\ wpc' = [wpc EXCEPT ![t] = CASE op \in {"writeback", "fsync"} -> "wbLock"
                                    [] op = "rebind" -> "rbWbLock"
                                    [] op = "punch" -> "ptWbLock"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sVars, wLeft, buf, minted, punches>>

WStart(t) == \E op \in {"writeback", "fsync", "rebind", "punch"} : WStartOp(t, op)

\* The writeback lock, a writeback's, or an fsync's next: with nothing dirty, it is let go of.
WWbLock(t) ==
    /\ wpc[t] \in {"wbLock", "fsNext"} /\ wbLock = "none"
    /\ wbLock' = t
    /\ wpc' = [wpc EXCEPT ![t] = IF dirty = {} THEN "unlock"
                                ELSE IF "swapFirst" \in Rules THEN "swapLock" ELSE "wpFirst"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, semX, semR,
                   sVars, wOp, wLeft, floor, buf, minted, punches>>

\* Without swapFirst: the dirty set's PTEs write-protected before the swap.
WWpFirst(t) ==
    /\ wpc[t] = "wpFirst"
    /\ pte' = [p \in Pages |-> IF p \in dirty THEN WriteProtectedAt(p) ELSE pte[p]]
    /\ wpc' = [wpc EXCEPT ![t] = "swapLock"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, dirty, wb, sealing, counted, semX, semR, wbLock,
                   sVars, wOp, wLeft, floor, buf, minted, punches>>

WSwapLock(t) ==
    /\ wpc[t] = "swapLock" /\ semX = "none"
    /\ IF "swapExclusive" \in Rules
       THEN /\ semR = {}
            /\ semX' = t
            /\ UNCHANGED semR
       ELSE /\ semR' = semR \cup {t}
            /\ UNCHANGED semX
    /\ wpc' = [wpc EXCEPT ![t] = "swap"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, wbLock, sVars,
                   wOp, wLeft, floor, buf, minted, punches>>

\* The dirty set swapped out for an empty one, and the semaphore let go of: the writeback lock
\* keeps the set from rebinds and punches until its commit.
WSwap(t) ==
    /\ wpc[t] = "swap"
    /\ wb' = dirty
    /\ dirty' = {}
    /\ Released(t)
    /\ wLeft' = [wLeft EXCEPT ![t] = IF "swapFirst" \in Rules THEN dirty ELSE {}]
    /\ wpc' = [wpc EXCEPT ![t] = IF "swapFirst" \in Rules /\ dirty # {} THEN "wp" ELSE "copy"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, sealing, counted, wbLock, sVars, wOp, floor,
                   buf, minted, punches>>

\* A page of the writeback set write-protected (fxfs_dax_protect, wp_shared_mapping_range).
WWpAt(t, p) ==
    /\ wpc[t] = "wp" /\ p \in wLeft[t]
    /\ pte' = [pte EXCEPT ![p] = WriteProtectedAt(p)]
    /\ wLeft' = [wLeft EXCEPT ![t] = wLeft[t] \ {p}]
    /\ wpc' = [wpc EXCEPT ![t] = IF wLeft[t] = {p} THEN "copy" ELSE "wp"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, dirty, wb, sealing, counted, semX, semR, wbLock,
                   sVars, wOp, floor, buf, minted, punches>>

WWp(t) == \E p \in Pages : WWpAt(t, p)

\* fxfs_write_many's copy of the pages, from the window into its transfer buffer.
WCopy(t) ==
    /\ wpc[t] = "copy"
    /\ buf' = [p \in Pages |-> IF p \in wb THEN win[p].data ELSE buf[p]]
    /\ wpc' = [wpc EXCEPT ![t] = IF "bytesFirst" \in Rules THEN "write" ELSE "commit"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sVars, wOp, wLeft, floor, minted,
                   punches>>

\* Its WRITE of them to a new extent, open, which a seal before the commit seals: before the
\* commit (bytesFirst), or after it.
WWrite(t) ==
    /\ wpc[t] = "write"
    /\ IF "bytesFirst" \in Rules
       THEN /\ wpc' = [wpc EXCEPT ![t] = "commit"]
            /\ minted' = "open"
            /\ UNCHANGED <<file, wb>>
       ELSE /\ file' = [p \in Pages |->
                           IF p \in wb THEN [file[p] EXCEPT !.data = buf[p]] ELSE file[p]]
            /\ wb' = {}
            /\ wpc' = [wpc EXCEPT ![t] = "unlock"]
            /\ UNCHANGED minted
    /\ UNCHANGED <<hostVars, durable, ghostVars, pte, dirty, sealing, counted, semX, semR, wbLock,
                   sVars, wOp, wLeft, floor, buf, punches>>

\* The transaction naming the new extent, open unless a seal came since its WRITE; its pages kept
\* until it is sealed. The blocks it replaces die at the next seal: a window page mapping one, not
\* private, is doomed.
WCommit(t) ==
    /\ wpc[t] = "commit"
    /\ LET sealed == "bytesFirst" \in Rules /\ minted = "sealed" IN
       /\ file' = [p \in Pages |->
                      IF p \in wb
                      THEN [state |-> IF sealed THEN "sealed" ELSE "open",
                            data |-> IF "bytesFirst" \in Rules THEN buf[p] ELSE Garbage]
                      ELSE file[p]]
       /\ sealing' = IF "keepUntilSealed" \in Rules /\ ~sealed THEN sealing \cup wb ELSE sealing
    /\ win' = [p \in Pages |->
                  IF p \in wb /\ win[p].kind = "extent" THEN [win[p] EXCEPT !.kind = "doomed"]
                  ELSE win[p]]
    /\ wb' = IF "bytesFirst" \in Rules THEN {} ELSE wb
    /\ minted' = "none"
    /\ wpc' = [wpc EXCEPT ![t] = IF "bytesFirst" \in Rules THEN "unlock" ELSE "write"]
    /\ UNCHANGED <<inst, refused, durable, ghostVars, pte, dirty, counted, semX, semR, wbLock,
                   sVars, wOp, wLeft, floor, buf, punches>>

\* The writeback lock let go of: an fsync's writeback may be followed by another, or its seal.
WUnlock(t) ==
    /\ wpc[t] = "unlock"
    /\ wbLock' = "none"
    /\ wpc' = [wpc EXCEPT ![t] = IF wOp[t] = "fsync" THEN "fsNext" ELSE "idle"]
    /\ buf' = [p \in Pages |-> 0]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, semX, semR,
                   sVars, wOp, wLeft, floor, minted, punches>>

\* An fsync's seal (fxfs_fsync), after its writebacks.
WFsSeal(t) ==
    /\ wpc[t] = "fsNext"
    /\ SealEffect
    /\ wpc' = [wpc EXCEPT ![t] = "fsDone"]
    /\ UNCHANGED <<refused, ghostVars, pte, dirty, wb, counted, semX, semR, wbLock, sVars, wOp,
                   wLeft, floor, buf, punches>>

\* The fsync returns.
WFsDone(t) ==
    /\ wpc[t] = "fsDone"
    /\ wpc' = [wpc EXCEPT ![t] = "idle"]
    /\ floor' = [floor EXCEPT ![t] = [p \in Pages |-> 0]]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sVars, wOp, wLeft, buf, minted,
                   punches>>

WRbWbLock(t) ==
    /\ wpc[t] = "rbWbLock" /\ wbLock = "none"
    /\ wbLock' = t
    /\ wpc' = [wpc EXCEPT ![t] = "rbLock"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, semX, semR,
                   sVars, wOp, wLeft, floor, buf, minted, punches>>

WRbLock(t) ==
    /\ wpc[t] = "rbLock" /\ semX = "none"
    /\ IF "rebindExclusive" \in Rules
       THEN /\ semR = {}
            /\ semX' = t
            /\ UNCHANGED semR
       ELSE /\ semR' = semR \cup {t}
            /\ UNCHANGED semX
    /\ wpc' = [wpc EXCEPT ![t] = "rbLook"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, wbLock, sVars,
                   wOp, wLeft, floor, buf, minted, punches>>

\* What a rebind may take: private pages in neither set (rebindAfterSeal: nor waiting for its
\* seal).
RebindCandidates ==
    {p \in Pages : /\ win[p].kind = "private"
                   /\ p \notin dirty \cup wb
                   /\ "rebindAfterSeal" \in Rules => p \notin sealing}

\* Its look, at a set of those, as many as it likes: dax.c's, at a range's every one.
WRbLookAt(t, S) ==
    /\ wpc[t] = "rbLook"
    /\ S \subseteq RebindCandidates
    /\ wLeft' = [wLeft EXCEPT ![t] = S]
    /\ wpc' = [wpc EXCEPT ![t] = "rbMap"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sVars, wOp, floor, buf, minted,
                   punches>>

WRbLook(t) == \E S \in SUBSET Pages : WRbLookAt(t, S)

\* The MAP of a page's block over its private page, which the host frees: zeros for a hole.
WRbMapAt(t, p) ==
    /\ wpc[t] = "rbMap" /\ p \in wLeft[t]
    /\ errors' = IF file[p].state = "open" THEN errors \cup {"a MAP of an open extent"}
                 ELSE errors
    /\ win' = [win EXCEPT ![p] =
                  [kind |-> IF file[p].state = "hole" THEN "zeros" ELSE "extent",
                   data |-> file[p].data]]
    /\ inst' = TakenAway({p})
    /\ counted' = counted \ {p}
    /\ wLeft' = [wLeft EXCEPT ![t] = wLeft[t] \ {p}]
    /\ UNCHANGED <<refused, fxfsVars, latest, next, pte, dirty, wb, sealing, semX, semR, wbLock,
                   sVars, wOp, wpc, floor, buf, minted, punches>>

WRbMap(t) == \E p \in Pages : WRbMapAt(t, p)

\* Its MAPs done, the semaphore let go of.
WRbUnlock(t) ==
    /\ wpc[t] = "rbMap" /\ wLeft[t] = {}
    /\ Released(t)
    /\ wpc' = [wpc EXCEPT ![t] = "unlock"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, wbLock, sVars,
                   wOp, wLeft, floor, buf, minted, punches>>

WPtWbLock(t) ==
    /\ wpc[t] = "ptWbLock" /\ wbLock = "none"
    /\ wbLock' = t
    /\ wpc' = [wpc EXCEPT ![t] = "ptLock"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, semX, semR,
                   sVars, wOp, wLeft, floor, buf, minted, punches>>

WPtLock(t) ==
    /\ wpc[t] = "ptLock" /\ semX = "none" /\ semR = {}
    /\ semX' = t
    /\ wpc' = [wpc EXCEPT ![t] = "punch"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, pte, dirty, wb, sealing, counted, semR, wbLock,
                   sVars, wOp, wLeft, floor, buf, minted, punches>>

\* A punch of a page: out of both sets, unmapped, MAP_ZEROS over it, a hole in Fxfs.
WPunch(t) ==
    /\ wpc[t] = "punch"
    /\ \E p \in Pages :
         /\ dirty' = dirty \ {p}
         /\ sealing' = sealing \ {p}
         /\ counted' = counted \ {p}
         /\ pte' = [pte EXCEPT ![p] = "none"]
         /\ win' = [win EXCEPT ![p] = [kind |-> "zeros", data |-> next]]
         /\ inst' = TakenAway({p})
         /\ file' = [file EXCEPT ![p] = [state |-> "hole", data |-> next]]
         /\ latest' = [latest EXCEPT ![p] = next]
    /\ next' = next + 1
    /\ punches' = punches + 1
    /\ semX' = "none"
    /\ wpc' = [wpc EXCEPT ![t] = "unlock"]
    /\ UNCHANGED <<refused, durable, errors, wb, semR, wbLock, sVars, wOp, wLeft, floor, buf,
                   minted>>

OtherNext(t) ==
    \/ WStart(t) \/ WWbLock(t) \/ WWpFirst(t) \/ WSwapLock(t) \/ WSwap(t) \/ WWp(t) \/ WCopy(t)
    \/ WWrite(t) \/ WCommit(t) \/ WUnlock(t) \/ WFsSeal(t) \/ WFsDone(t) \/ WRbWbLock(t)
    \/ WRbLock(t) \/ WRbLook(t) \/ WRbMap(t) \/ WRbUnlock(t) \/ WPtWbLock(t) \/ WPtLock(t)
    \/ WPunch(t)

-----------------------------------------------------------------------------
(* The host's seals: the journal's, or another file's fsync's, whenever one changes anything. *)

Seal ==
    /\ SealChanges
    /\ SealEffect
    /\ UNCHANGED <<refused, ghostVars, pte, dirty, wb, counted, semX, semR, wbLock, sVars, wpc,
                   wOp, wLeft, floor, buf, punches>>

\* The kernel cleaning a page's DAX entry (dax_writeback_one, as writepages and a range's release
\* clean them), which write-protects its PTE: not one a fault holds, which it passes by.
WCleanAt(p) ==
    /\ pte[p] = "rw" /\ ~EntryHeld(p)
    /\ pte' = [pte EXCEPT ![p] = "ro"]
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, dirty, wb, sealing, counted, semX, semR, wbLock,
                   sVars, wVars>>

WClean == \E p \in Pages : WCleanAt(p)

\* The host's cache evicting a sealed extent's page, or zeros', which the window maps: the window
\* unmaps it, and maps it again as a fault brings it in, the same bytes; what the VM process
\* installed goes (changesTakeAway). A private page stays.
Evict ==
    /\ \E p \in Pages :
         /\ win[p].kind \in {"extent", "zeros", "doomed"}
         /\ inst[p] # None
         /\ inst' = TakenAway({p})
    /\ UNCHANGED <<win, refused, fxfsVars, ghostVars, guestVars, sVars, wVars, lVars>>

-----------------------------------------------------------------------------
(* The loader: the guest's loads, through what the VM process installed, or a fault. *)

LStart ==
    /\ lpc = "idle" /\ loads < MaxLoads
    /\ \E p \in Pages : lPage' = p
    /\ lpc' = "load"
    /\ UNCHANGED <<hostVars, fxfsVars, ghostVars, guestVars, sVars, wVars, lSeen, loads>>

\* A load of the page: of the private page, the guest's own, or what the VM process installed,
\* which is to be what the window maps there; or, named alone, a fault, which installs the page as
\* the window names it (installAsNamed), or looks at it, to install what it found after, and the
\* load is retried.
LLoad ==
    /\ lpc = "load"
    /\ IF win[lPage].kind = "private" \/ inst[lPage] # None
       THEN /\ errors' = IF win[lPage].kind # "private" /\ inst[lPage] # win[lPage].data
                         THEN errors \cup {"a stale load"} ELSE errors
            /\ loads' = loads + 1
            /\ lpc' = "idle"
            /\ UNCHANGED <<inst, lSeen>>
       ELSE IF "installAsNamed" \in Rules
            THEN /\ inst' = [inst EXCEPT ![lPage] = win[lPage].data]
                 /\ UNCHANGED <<errors, loads, lpc, lSeen>>
            ELSE /\ lSeen' = win[lPage].data
                 /\ lpc' = "install"
                 /\ UNCHANGED <<inst, errors, loads>>
    /\ UNCHANGED <<win, refused, fxfsVars, latest, next, guestVars, sVars, wVars, lPage>>

\* Without installAsNamed: the install of what the fault found, whatever the window maps by now.
LInstall ==
    /\ lpc = "install"
    /\ inst' = [inst EXCEPT ![lPage] = lSeen]
    /\ lpc' = "load"
    /\ UNCHANGED <<win, refused, fxfsVars, ghostVars, guestVars, sVars, wVars, lPage, lSeen,
                   loads>>

LoaderNext == LStart \/ LLoad \/ LInstall

-----------------------------------------------------------------------------

Init ==
    \* Each page a sealed extent's, as a file read in has, or a hole: the window maps them so.
    /\ \E holes \in SUBSET Pages :
         /\ file = [p \in Pages |->
                       [state |-> IF p \in holes THEN "hole" ELSE "sealed", data |-> 0]]
         /\ win = [p \in Pages |->
                      [kind |-> IF p \in holes THEN "zeros" ELSE "extent", data |-> 0]]
    \* Named, as the guest's MAPs left them, none touched yet.
    /\ inst = [p \in Pages |-> None]
    /\ refused = FALSE
    /\ durable = [p \in Pages |-> 0]
    /\ latest = [p \in Pages |-> 0]
    /\ next = 1
    /\ errors = {}
    /\ pte = [p \in Pages |-> "none"]
    /\ dirty = {} /\ wb = {} /\ sealing = {} /\ counted = {}
    /\ semX = "none" /\ semR = {} /\ wbLock = "none"
    /\ spc = [t \in Storers |-> "idle"]
    /\ sPage = [t \in Storers |-> 0]
    /\ sWay = [t \in Storers |-> "none"]
    /\ stores = 0
    /\ wpc = [t \in Others |-> "idle"]
    /\ wOp = [t \in Others |-> "none"]
    /\ wLeft = [t \in Others |-> {}]
    /\ floor = [t \in Others |-> [p \in Pages |-> 0]]
    /\ buf = [p \in Pages |-> 0]
    /\ minted = "none"
    /\ punches = 0
    /\ lpc = "idle" /\ lPage = 0 /\ lSeen = 0 /\ loads = 0

Next ==
    \/ /\ \/ \E t \in Storers : StorerNext(t)
          \/ \E t \in Others : OtherNext(t)
          \/ Seal
          \/ WClean
       /\ UNCHANGED lVars
    \/ Evict
    \/ LoaderNext

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(* What must hold. *)

\* An fsync that returned has every store that completed before it began in what a crash shows: the
\* extent its commit names, or a later one.
FsyncKeepsStores ==
    \A t \in Others : wpc[t] = "fsDone" => \A p \in Pages : durable[p] >= floor[t][p]

\* The window shows every page's last store: no rebind, nor anything else, discarded one.
NoLostStore == \A p \in Pages : win[p].data = latest[p]

\* A page in neither set is in Fxfs as the window shows it: writeback has nothing of it to write.
CleanInFxfs == \A p \in Pages : p \notin dirty \cup wb => file[p].data = win[p].data

\* Nothing maps an open extent: the host refuses such a MAP (EPERM).
NoOpenMap == "a MAP of an open extent" \notin errors

\* A crash shows each page old or new, as a commit left it, never a new extent's unwritten block.
CrashOldOrNew == \A p \in Pages : durable[p] # Garbage

\* The host never refuses a store, nor a PRIVATE: the guest keeps within private_max.
NoRefusal == ~refused

\* A dirty page, or one being written back, is private on the host: a store to it reaches no
\* extent's block, which the writeback moves, and the seal after it frees (privateAtIomap).
DirtyImpliesPrivate == \A p \in dirty \cup wb : win[p].kind = "private"

\* At most private_max private pages: the host's to keep, by refusing.
PrivateWithinMax == HostPrivate <= PrivateMax

\* A load reads what the window maps at the page as it is made, or the guest's private page: never
\* what the VM process installed of what the window mapped before (installAsNamed,
\* changesTakeAway).
NoStaleLoad == "a stale load" \notin errors
=============================================================================
