---------------------------- MODULE Ephemeral ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Ephemeral cells (doc-25, TASK-698.1): children of a version of a cell   *)
(* in the bucket, kept on one host's disk alone, never uploaded, handed    *)
(* over live from host to host. The parent's version is kept in the       *)
(* bucket by references, each naming the host's incarnation that holds it. *)
(*                                                                         *)
(* What is modelled:                                                       *)
(*                                                                         *)
(* - The parent's head naming the version (`named`), dropped once; the     *)
(*   version's data (`data`), which a collector pass deletes when it read  *)
(*   the head not naming it and then listed no reference.                  *)
(* - References: <<cell, incarnation>>, the incarnation <<host, k>>.        *)
(* - Hosts: a daemon serving, down, or starting (it lists the references   *)
(*   naming its incarnation, then retires those of cells it does not hold, *)
(*   before it serves). A crash keeps the disk; a loss wipes it, and the   *)
(*   host comes back in a new incarnation. Decommissioning an incarnation  *)
(*   no host has any more retires its references.                          *)
(* - A cell on a host: forking (referred, its head not yet read), failing  *)
(*   (its head read not naming the version, its reference not yet         *)
(*   deleted), writer, handing (its guest paused, its hand-over asked),    *)
(*   receiving (a target taking the source's log), gave up (handed over,   *)
(*   its reference not yet retired). Writers' commits are Local at once,   *)
(*   so a guest is told each (`told`).                                     *)
(* - Hand-over: requests and answers are messages kept for ever, so that   *)
(*   any of them may arrive late and again. A request names its target's   *)
(*   incarnation, which alone answers it. A target answers from what it    *)
(*   holds and from the migrations it accepted (`accepted`, on its disk).  *)
(*                                                                         *)
(* The rules, each a member of Rules; specs/ephemeral/mutations removes    *)
(* one each, and must find the violation it prevents:                      *)
(*                                                                         *)
(* - forksReferFirst: a fork writes its reference before it reads the      *)
(*   parent's head (NoForksReferFirst).                                    *)
(* - targetRefersFirst: a target writes its reference before it takes      *)
(*   anything of the source's (NoTargetRefersFirst).                       *)
(* - sourceGivesUpFirst: a source stops committing before it asks for the  *)
(*   hand-over (NoSourceGivesUpFirst).                                     *)
(* - sourceRetiresLast: a source retires its reference only once the       *)
(*   target accepted (NoSourceRetiresLast).                                *)
(* - retireOnlyUnheld: a daemon starting retires only the references of    *)
(*   cells it does not hold (NoRetireOnlyUnheld).                          *)
(* - refuseFinal: a target that refuses a hand-over drops what it took, so *)
(*   that it can never accept that hand-over after (NoRefuseFinal).        *)
(* - openAtNamed: a target accepts only holding the log through the commit *)
(*   the request names (NoOpenAtNamed).                                    *)
(* - remembersAccepted: a target answers yes to every request of a         *)
(*   migration it accepted, even once it no longer holds the cell          *)
(*   (NoRemembersAccepted).                                                *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Hosts,          \* the hosts
    Cells,          \* the ephemeral cells, each forked at most once
    NoHost,         \* no host
    MaxCommits,     \* the most commits a cell's log holds
    MaxMigrations,  \* migrations at most, over all cells
    MaxCrashes,     \* daemons crashed at most
    MaxLosses,      \* disks lost at most
    MaxPasses,      \* collector passes at most
    Rules           \* the rules in force (all, but in a mutation)

AllRules == {"forksReferFirst", "targetRefersFirst", "sourceGivesUpFirst", "sourceRetiresLast",
             "retireOnlyUnheld", "refuseFinal", "openAtNamed", "remembersAccepted"}

ASSUME Rules \subseteq AllRules
ASSUME NoHost \notin Hosts

States == {"none", "forking", "failing", "early", "writer", "handing", "receiving", "gaveup"}

VARIABLES
    named,      \* the parent's head names the version
    data,       \* the version's data is whole in the bucket
    refs,       \* the references in the bucket: <<cell, <<host, k>>>>
    inc,        \* each host's incarnation number
    phase,      \* each host's daemon: "serving", "down" or "starting"
    rlist,      \* the references a starting daemon listed
    st,         \* each host's state of each cell
    log,        \* the commits each host holds of each cell
    mig,        \* the migration a source asked for or a target takes, 0 for none
    partner,    \* a source's target, or a target's source
    pinc,       \* a source's target's incarnation number, as prepared
    accepted,   \* the migrations each host accepted, on its disk
    msgs,       \* requests and answers sent, kept for ever
    told,       \* the most commits of each cell a guest was told were Local
    forked,     \* each cell's fork begun
    gphase,     \* the collector's pass: "idle", "read" or "listed"
    gnamed,     \* what its pass read of the head
    grefs,      \* whether its pass listed any reference
    passes, crashes, losses, migrations,
    handovers   \* hand-overs accepted, for the Reaches models

vars == <<named, data, refs, inc, phase, rlist, st, log, mig, partner, pinc, accepted, msgs, told,
          forked, gphase, gnamed, grefs, passes, crashes, losses, migrations, handovers>>

On(r) == r \in Rules
I(h) == <<h, inc[h]>>
Key(h, c) == <<c, I(h)>>
Serving(h, c) == st[h][c] = "writer" \/ (st[h][c] = "handing" /\ ~On("sourceGivesUpFirst"))
Max(a, b) == IF a > b THEN a ELSE b

TypeOK ==
    /\ named \in BOOLEAN /\ data \in BOOLEAN
    /\ st \in [Hosts -> [Cells -> States]]
    /\ phase \in [Hosts -> {"serving", "down", "starting"}]
    /\ gphase \in {"idle", "read", "listed"}

Init ==
    /\ named = TRUE /\ data = TRUE /\ refs = {}
    /\ inc = [h \in Hosts |-> 0]
    /\ phase = [h \in Hosts |-> "serving"]
    /\ rlist = [h \in Hosts |-> {}]
    /\ st = [h \in Hosts |-> [c \in Cells |-> "none"]]
    /\ log = [h \in Hosts |-> [c \in Cells |-> 0]]
    /\ mig = [h \in Hosts |-> [c \in Cells |-> 0]]
    /\ partner = [h \in Hosts |-> [c \in Cells |-> NoHost]]
    /\ pinc = [h \in Hosts |-> [c \in Cells |-> 0]]
    /\ accepted = [h \in Hosts |-> {}]
    /\ msgs = {}
    /\ told = [c \in Cells |-> 0]
    /\ forked = [c \in Cells |-> FALSE]
    /\ gphase = "idle" /\ gnamed = TRUE /\ grefs = FALSE
    /\ passes = 0 /\ crashes = 0 /\ losses = 0 /\ migrations = 0 /\ handovers = 0

SetSt(h, c, s) == st' = [st EXCEPT ![h][c] = s]

\* ---- The version and the collector ----

Drop == named /\ named' = FALSE /\ UNCHANGED <<data, refs, inc, phase, rlist, st, log, mig,
    partner, pinc, accepted, msgs, told, forked, gphase, gnamed, grefs, passes, crashes, losses,
    migrations, handovers>>

GcUnchanged == UNCHANGED <<named, refs, inc, phase, rlist, st, log, mig, partner, pinc, accepted,
    msgs, told, forked, crashes, losses, migrations, handovers>>

GcRead ==
    /\ gphase = "idle" /\ passes < MaxPasses /\ data
    /\ gphase' = "read" /\ gnamed' = named /\ passes' = passes + 1
    /\ UNCHANGED <<data, grefs>> /\ GcUnchanged

GcList ==
    /\ gphase = "read"
    /\ gphase' = "listed" /\ grefs' = (refs # {})
    /\ UNCHANGED <<data, gnamed, passes>> /\ GcUnchanged

GcDelete ==
    /\ gphase = "listed"
    /\ data' = (data /\ (gnamed \/ grefs))
    /\ gphase' = "idle"
    /\ UNCHANGED <<gnamed, grefs, passes>> /\ GcUnchanged

\* ---- Forks ----

ForkUnchanged == UNCHANGED <<named, data, inc, phase, rlist, log, mig, partner, pinc, accepted,
    msgs, told, gphase, gnamed, grefs, passes, crashes, losses, migrations, handovers>>

\* forksReferFirst: the reference first, then the head; without it, the head first.
ForkBegin(h, c) ==
    /\ phase[h] = "serving" /\ ~forked[c]
    /\ forked' = [forked EXCEPT ![c] = TRUE]
    /\ IF On("forksReferFirst")
       THEN refs' = refs \cup {Key(h, c)} /\ SetSt(h, c, "forking")
       ELSE named /\ refs' = refs /\ SetSt(h, c, "early")
    /\ ForkUnchanged

ForkRead(h, c) ==
    /\ phase[h] = "serving" /\ st[h][c] = "forking"
    /\ SetSt(h, c, IF named THEN "writer" ELSE "failing")
    /\ UNCHANGED <<refs, forked>> /\ ForkUnchanged

ForkLate(h, c) ==
    /\ phase[h] = "serving" /\ st[h][c] = "early"
    /\ refs' = refs \cup {Key(h, c)} /\ SetSt(h, c, "writer")
    /\ UNCHANGED forked /\ ForkUnchanged

ForkFail(h, c) ==
    /\ phase[h] = "serving" /\ st[h][c] = "failing"
    /\ refs' = refs \ {Key(h, c)} /\ SetSt(h, c, "none")
    /\ UNCHANGED forked /\ ForkUnchanged

\* ---- Commits ----

Commit(h, c) ==
    /\ phase[h] = "serving" /\ Serving(h, c) /\ log[h][c] < MaxCommits
    /\ log' = [log EXCEPT ![h][c] = @ + 1]
    /\ told' = [told EXCEPT ![c] = Max(@, log[h][c] + 1)]
    /\ UNCHANGED <<named, data, refs, inc, phase, rlist, st, mig, partner, pinc, accepted, msgs,
        forked, gphase, gnamed, grefs, passes, crashes, losses, migrations, handovers>>

\* ---- Migration ----

MigUnchanged == UNCHANGED <<named, data, inc, phase, rlist, forked, gphase, gnamed, grefs, passes,
    crashes, losses>>

\* The source asks the target to prepare: the target refers (targetRefersFirst), then takes.
\* A target takes a cell only holding nothing of it.
Prepare(s, t, c) ==
    /\ s # t /\ phase[s] = "serving" /\ phase[t] = "serving"
    /\ st[s][c] = "writer" /\ mig[s][c] = 0 /\ st[t][c] = "none"
    /\ migrations < MaxMigrations
    /\ LET id == migrations + 1 IN
       /\ migrations' = id
       /\ st' = [st EXCEPT ![t][c] = "receiving"]
       /\ log' = [log EXCEPT ![t][c] = 0]
       /\ mig' = [mig EXCEPT ![s][c] = id, ![t][c] = id]
       /\ partner' = [partner EXCEPT ![s][c] = t, ![t][c] = s]
       /\ pinc' = [pinc EXCEPT ![s][c] = inc[t]]
       /\ refs' = IF On("targetRefersFirst") THEN refs \cup {Key(t, c)} ELSE refs
    /\ UNCHANGED <<accepted, msgs, told, handovers>> /\ MigUnchanged

\* A round: the target takes the source's log as it is.
Round(t, c) == LET s == partner[t][c] IN
    /\ phase[t] = "serving" /\ st[t][c] = "receiving"
    /\ s # NoHost /\ phase[s] = "serving" /\ st[s][c] \in {"writer", "handing"}
    /\ mig[s][c] = mig[t][c] /\ log[t][c] # log[s][c]
    /\ log' = [log EXCEPT ![t][c] = log[s][c]]
    /\ UNCHANGED <<refs, st, mig, partner, pinc, accepted, msgs, told, migrations, handovers>>
    /\ MigUnchanged

\* The target gives the migration up of its own accord, before any request.
TargetAbort(t, c) ==
    /\ phase[t] = "serving" /\ st[t][c] = "receiving"
    /\ SetSt(t, c, "none")
    /\ mig' = [mig EXCEPT ![t][c] = 0]
    /\ partner' = [partner EXCEPT ![t][c] = NoHost]
    /\ refs' = refs \ {Key(t, c)}
    /\ UNCHANGED <<log, pinc, accepted, msgs, told, migrations, handovers>> /\ MigUnchanged

\* The source, its guest paused, asks for the hand-over at its last commit (sourceGivesUpFirst:
\* its commits stop here). Without sourceRetiresLast, it retires its reference now.
Hand(s, c) ==
    /\ phase[s] = "serving" /\ st[s][c] = "writer" /\ mig[s][c] # 0
    /\ SetSt(s, c, "handing")
    /\ msgs' = msgs \cup {[kind |-> "req", cell |-> c, id |-> mig[s][c], n |-> log[s][c],
                           to |-> partner[s][c], k |-> pinc[s][c]]}
    /\ refs' = IF On("sourceRetiresLast") THEN refs ELSE refs \ {Key(s, c)}
    /\ UNCHANGED <<log, mig, partner, pinc, accepted, told, migrations, handovers>> /\ MigUnchanged

\* Whether a target knows it accepted a migration: from its disk (remembersAccepted), or, without
\* the rule, only while it is still the cell's writer.
Remembers(t, c, id) ==
    /\ id \in accepted[t]
    /\ (~On("remembersAccepted") => st[t][c] = "writer")

Answer(c, id, yes) == [kind |-> "ans", cell |-> c, id |-> id, yes |-> yes]

\* The target answers a request, as often as it comes.
TargetAnswer(m) == LET t == m.to c == m.cell IN
    /\ m.kind = "req" /\ phase[t] = "serving" /\ inc[t] = m.k
    /\ \/ \* A migration it accepted: yes again.
          /\ Remembers(t, c, m.id)
          /\ msgs' = msgs \cup {Answer(c, m.id, TRUE)}
          /\ UNCHANGED <<refs, st, log, mig, partner, accepted, handovers>>
       \/ \* Taking it: accepts holding the log through the commit named (openAtNamed).
          /\ st[t][c] = "receiving" /\ mig[t][c] = m.id
          /\ (On("openAtNamed") => log[t][c] = m.n)
          /\ st' = [st EXCEPT ![t][c] = "writer"]
          /\ mig' = [mig EXCEPT ![t][c] = 0]
          /\ partner' = [partner EXCEPT ![t][c] = NoHost]
          /\ accepted' = [accepted EXCEPT ![t] = @ \cup {m.id}]
          /\ refs' = IF On("targetRefersFirst") THEN refs ELSE refs \cup {Key(t, c)}
          /\ msgs' = msgs \cup {Answer(c, m.id, TRUE)}
          /\ handovers' = handovers + 1
          /\ UNCHANGED log
       \/ \* Taking it: refuses; refuseFinal drops what it took, its reference too.
          /\ st[t][c] = "receiving" /\ mig[t][c] = m.id
          /\ IF On("refuseFinal")
             THEN /\ st' = [st EXCEPT ![t][c] = "none"]
                  /\ mig' = [mig EXCEPT ![t][c] = 0]
                  /\ partner' = [partner EXCEPT ![t][c] = NoHost]
                  /\ refs' = refs \ {Key(t, c)}
             ELSE UNCHANGED <<st, mig, partner, refs>>
          /\ msgs' = msgs \cup {Answer(c, m.id, FALSE)}
          /\ UNCHANGED <<log, accepted, handovers>>
       \/ \* Neither accepted nor taking it: no.
          /\ ~Remembers(t, c, m.id)
          /\ ~(st[t][c] = "receiving" /\ mig[t][c] = m.id)
          /\ msgs' = msgs \cup {Answer(c, m.id, FALSE)}
          /\ UNCHANGED <<refs, st, log, mig, partner, accepted, handovers>>
    /\ UNCHANGED <<pinc, told, migrations>> /\ MigUnchanged

\* The source reads an answer to its hand-over: yes, it gave the cell up; no, it serves again.
SourceHears(s, c, a) ==
    /\ phase[s] = "serving" /\ st[s][c] = "handing"
    /\ a.kind = "ans" /\ a.cell = c /\ a.id = mig[s][c]
    /\ SetSt(s, c, IF a.yes THEN "gaveup" ELSE "writer")
    /\ mig' = [mig EXCEPT ![s][c] = 0]
    /\ partner' = [partner EXCEPT ![s][c] = NoHost]
    /\ UNCHANGED <<refs, log, pinc, accepted, msgs, told, migrations, handovers>> /\ MigUnchanged

\* The source, having given the cell up, deletes it, then retires its reference.
SourceGone(s, c) ==
    /\ phase[s] = "serving" /\ st[s][c] = "gaveup"
    /\ SetSt(s, c, "none")
    /\ log' = [log EXCEPT ![s][c] = 0]
    /\ refs' = refs \ {Key(s, c)}
    /\ UNCHANGED <<mig, partner, pinc, accepted, msgs, told, migrations, handovers>>
    /\ MigUnchanged

\* ---- Hosts ----

HostUnchanged == UNCHANGED <<named, data, accepted, msgs, told, forked, gphase, gnamed, grefs,
    passes, migrations, handovers>>

\* A crash keeps the disk: writers, hand-overs asked and cells given up stay; what was in memory
\* goes (forks under way, migrations being taken or prepared).
Volatile(s) == s \in {"forking", "failing", "early", "receiving"}
Crash(h) ==
    /\ phase[h] # "down" /\ crashes < MaxCrashes
    /\ crashes' = crashes + 1
    /\ phase' = [phase EXCEPT ![h] = "down"]
    /\ st' = [st EXCEPT ![h] = [c \in Cells |-> IF Volatile(@[c]) THEN "none" ELSE @[c]]]
    /\ mig' = [mig EXCEPT ![h] = [c \in Cells |-> IF st[h][c] = "handing" THEN @[c] ELSE 0]]
    /\ partner' = [partner EXCEPT ![h] =
                     [c \in Cells |-> IF st[h][c] = "handing" THEN @[c] ELSE NoHost]]
    /\ log' = [log EXCEPT ![h] = [c \in Cells |-> IF Volatile(st[h][c]) THEN 0 ELSE @[c]]]
    /\ UNCHANGED <<refs, inc, rlist, pinc, losses>> /\ HostUnchanged

Start(h) ==
    /\ phase[h] = "down"
    /\ phase' = [phase EXCEPT ![h] = "starting"]
    /\ rlist' = [rlist EXCEPT ![h] = {r \in refs : r[2] = I(h)}]
    /\ UNCHANGED <<refs, inc, st, log, mig, partner, pinc, crashes, losses>> /\ HostUnchanged

Held(h, r) == st[h][r[1]] \in {"writer", "handing"}

Retire(h) ==
    /\ phase[h] = "starting"
    /\ refs' = refs \ {r \in rlist[h] : ~On("retireOnlyUnheld") \/ ~Held(h, r)}
    /\ phase' = [phase EXCEPT ![h] = "serving"]
    /\ rlist' = [rlist EXCEPT ![h] = {}]
    /\ UNCHANGED <<inc, st, log, mig, partner, pinc, crashes, losses>> /\ HostUnchanged

\* A disk lost: everything the host held goes, and it comes back in a new incarnation.
Lose(h) ==
    /\ losses < MaxLosses
    /\ losses' = losses + 1
    /\ inc' = [inc EXCEPT ![h] = @ + 1]
    /\ phase' = [phase EXCEPT ![h] = "down"]
    /\ rlist' = [rlist EXCEPT ![h] = {}]
    /\ st' = [st EXCEPT ![h] = [c \in Cells |-> "none"]]
    /\ log' = [log EXCEPT ![h] = [c \in Cells |-> 0]]
    /\ mig' = [mig EXCEPT ![h] = [c \in Cells |-> 0]]
    /\ partner' = [partner EXCEPT ![h] = [c \in Cells |-> NoHost]]
    /\ accepted' = [accepted EXCEPT ![h] = {}]
    /\ UNCHANGED <<named, data, refs, pinc, msgs, told, forked, gphase, gnamed, grefs, passes,
        crashes, migrations, handovers>>

\* The VM manager decommissions an incarnation no host has any more.
Decommission(h, k) ==
    /\ k < inc[h] /\ \E x \in refs : x[2] = <<h, k>>
    /\ refs' = {r \in refs : r[2] # <<h, k>>}
    /\ UNCHANGED <<inc, phase, rlist, st, log, mig, partner, pinc, crashes, losses>>
    /\ HostUnchanged

Next ==
    \/ Drop \/ GcRead \/ GcList \/ GcDelete
    \/ \E h \in Hosts, c \in Cells :
        \/ ForkBegin(h, c) \/ ForkRead(h, c) \/ ForkLate(h, c) \/ ForkFail(h, c)
        \/ Commit(h, c) \/ Round(h, c) \/ TargetAbort(h, c) \/ Hand(h, c) \/ SourceGone(h, c)
        \/ \E t \in Hosts : Prepare(h, t, c)
        \/ \E a \in msgs : SourceHears(h, c, a)
    \/ \E m \in msgs : TargetAnswer(m)
    \/ \E h \in Hosts : Crash(h) \/ Start(h) \/ Retire(h) \/ Lose(h)
    \/ \E h \in Hosts, k \in 0..MaxLosses : Decommission(h, k)

Spec == Init /\ [][Next]_vars

HostPerms == Permutations(Hosts)

\* ---- Invariants ----

\* Every cell a host writes, hands over or takes reads its version whole.
NothingNeededGone ==
    \A h \in Hosts, c \in Cells : st[h][c] \in {"writer", "handing", "receiving"} => data

\* At most one host serves a cell's guest.
OneWriter == \A c \in Cells : Cardinality({h \in Hosts : Serving(h, c)}) <= 1

\* A cell's writer holds every commit its guests were told.
HandOverLosesNothing == \A h \in Hosts, c \in Cells : st[h][c] = "writer" => log[h][c] >= told[c]

\* Reached, as models of specs/ephemeral/mutations show they are violated (Reaches*).
NoHandOver == handovers = 0
NoHandOverBack == handovers < 2
NoCollected == data
=============================================================================
