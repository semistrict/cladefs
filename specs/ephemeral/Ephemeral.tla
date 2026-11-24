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
(* - Migration: every request and answer is a message kept for ever, so   *)
(*   that any of them may arrive late and again (board 5732): prepares,    *)
(*   their answers, hand-overs, theirs, aborts and releases. A target      *)
(*   taking a prepare makes a token new to that take (`tok`), which its    *)
(*   answer carries with its incarnation, and the hand-over names both: a  *)
(*   request names its target's incarnation, which alone answers it, as    *)
(*   one of another knows nothing of it. A target answers from what it     *)
(*   holds and from the migrations it accepted (`accepted`, on its disk),  *)
(*   until their sources release them, having let the cell go (board       *)
(*   5734). A source may lose a migration it prepared before its           *)
(*   hand-over (its prepare's answer lost, or its daemon crashed), or end  *)
(*   it (AbortMigration, board 8577): it aborts it by ID, a message that   *)
(*   may arrive late, or never. Its hand-over asked, the cell given up, it *)
(*   may abort it too, the target's answer then saying where the cell is   *)
(*   (SourceAbortsHanding); a target may give up a migration of its own    *)
(*   accord (TargetAbort: one idle too long, or whose bucket calls time    *)
(*   out). Migration IDs grow, as the cell's epoch, which the code's       *)
(*   prepares carry, does.                                                 *)
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
(*   migration it accepted, even once it no longer holds the cell, until   *)
(*   its source released it (NoRemembersAccepted).                         *)
(* - supersedes: a target taking a migration the source lost gives it up   *)
(*   at the cell's next, newer prepare, so that the cell migrates there    *)
(*   again (NoSupersedes, MigratesAgain); an older one, late, it refuses.  *)
(* - boundToPrepare: a target accepts a hand-over only naming the token of *)
(*   its take of the migration: a late prepare taken again after the       *)
(*   target answered no, a late request of the first take refused         *)
(*   (NoBoundToPrepare).                                                   *)
(* - abortAsksTarget: a source given up that aborts serves again only on  *)
(*   its target's answer, never on its own word: the target may have      *)
(*   accepted (NoAbortAsksTarget, of EphemeralAborts).                    *)
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
    AbortsHanding,  \* whether a source given up may abort (SourceAbortsHanding)
    Rules           \* the rules in force (all, but in a mutation)

AllRules == {"forksReferFirst", "targetRefersFirst", "sourceGivesUpFirst", "sourceRetiresLast",
             "retireOnlyUnheld", "refuseFinal", "openAtNamed", "remembersAccepted", "supersedes",
             "boundToPrepare", "abortAsksTarget"}

ASSUME Rules \subseteq AllRules
ASSUME AbortsHanding \in BOOLEAN
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
    tok,        \* a target's token of its take of the migration it takes, 0 for none
    took,       \* how often each host took each migration: its takes' tokens, a late one's 2
    accepted,   \* the migrations each host accepted, on its disk
    msgs,       \* requests and answers sent, kept for ever
    told,       \* the most commits of each cell a guest was told were Local
    forked,     \* each cell's fork begun
    gphase,     \* the collector's pass: "idle", "read" or "listed"
    gnamed,     \* what its pass read of the head
    grefs,      \* whether its pass listed any reference
    passes, crashes, losses, migrations,
    handovers,  \* hand-overs accepted, for the Reaches models
    retired     \* references a daemon starting retired, for the Reaches models

vars == <<named, data, refs, inc, phase, rlist, st, log, mig, partner, tok, took, accepted,
          msgs, told, forked, gphase, gnamed, grefs, passes, crashes, losses, migrations, handovers,
          retired>>

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
    /\ tok = [h \in Hosts |-> [c \in Cells |-> 0]]
    /\ took = [h \in Hosts |-> [i \in 1..MaxMigrations |-> 0]]
    /\ accepted = [h \in Hosts |-> {}]
    /\ msgs = {}
    /\ told = [c \in Cells |-> 0]
    /\ forked = [c \in Cells |-> FALSE]
    /\ gphase = "idle" /\ gnamed = TRUE /\ grefs = FALSE
    /\ passes = 0 /\ crashes = 0 /\ losses = 0 /\ migrations = 0 /\ handovers = 0
    /\ retired = 0

SetSt(h, c, s) == st' = [st EXCEPT ![h][c] = s]

\* ---- The version and the collector ----

\* What a migration keeps of a cell, which only the migration's actions and the hosts' change.
MigState == <<mig, partner, tok, took>>

Drop == named /\ named' = FALSE /\ UNCHANGED <<data, refs, inc, phase, rlist, st, log, MigState,
    accepted, msgs, told, forked, gphase, gnamed, grefs, passes, crashes, losses, migrations,
    handovers, retired>>

GcUnchanged == UNCHANGED <<named, refs, inc, phase, rlist, st, log, MigState, accepted, msgs, told,
    forked, crashes, losses, migrations, handovers, retired>>

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

ForkUnchanged == UNCHANGED <<named, data, inc, phase, rlist, log, MigState, accepted, msgs, told,
    gphase, gnamed, grefs, passes, crashes, losses, migrations, handovers, retired>>

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
    /\ UNCHANGED <<named, data, refs, inc, phase, rlist, st, MigState, accepted, msgs, forked,
        gphase, gnamed, grefs, passes, crashes, losses, migrations, handovers, retired>>

\* ---- Migration ----

MigUnchanged == UNCHANGED <<named, data, inc, phase, rlist, forked, gphase, gnamed, grefs, passes,
    crashes, losses>>

\* The source asks the target to prepare a migration, its ID new: a message that may arrive late,
\* and again.
SourcePrepares(s, t, c) ==
    /\ s # t /\ phase[s] = "serving"
    /\ st[s][c] = "writer" /\ mig[s][c] = 0
    /\ migrations < MaxMigrations
    /\ LET id == migrations + 1 IN
       /\ migrations' = id
       /\ mig' = [mig EXCEPT ![s][c] = id]
       /\ partner' = [partner EXCEPT ![s][c] = t]
       /\ msgs' = msgs \cup {[kind |-> "prep", cell |-> c, id |-> id, from |-> s, to |-> t]}
    /\ UNCHANGED <<refs, st, log, tok, took, accepted, told, handovers, retired>>
    /\ MigUnchanged

\* Whether a target takes a prepare of migration `id` of cell `c`: holding nothing of it, or taking
\* an older migration of it, which it gives up (supersedes): its writer, preparing this one, pursues
\* that one no more. An older one, late, it refuses, and one it takes already it takes not again.
Takes(t, c, id) ==
    \/ st[t][c] = "none"
    \/ st[t][c] = "receiving" /\ On("supersedes") /\ id > mig[t][c]

Prepared(c, id, k, n) == [kind |-> "prepared", cell |-> c, id |-> id, k |-> k, tok |-> n]

\* The target takes a prepare: it refers (targetRefersFirst), then takes, its token new to this
\* take of the migration, which its answer carries (the code's tokens are random; here a host's
\* first take of a migration is 1, and one late prepare taken again 2). A migration it accepted it
\* takes not again: its source has its answer.
TargetPrepares(m) == LET t == m.to c == m.cell n == took[t][m.id] + 1 IN
    /\ m.kind = "prep" /\ phase[t] = "serving"
    /\ m.id \notin accepted[t] /\ Takes(t, c, m.id) /\ n <= 2
    /\ took' = [took EXCEPT ![t][m.id] = n]
    /\ st' = [st EXCEPT ![t][c] = "receiving"]
    /\ log' = [log EXCEPT ![t][c] = 0]
    /\ mig' = [mig EXCEPT ![t][c] = m.id]
    /\ partner' = [partner EXCEPT ![t][c] = m.from]
    /\ tok' = [tok EXCEPT ![t][c] = n]
    /\ refs' = IF On("targetRefersFirst") THEN refs \cup {Key(t, c)} ELSE refs
    /\ msgs' = msgs \cup {Prepared(c, m.id, inc[t], n)}
    /\ UNCHANGED <<accepted, told, migrations, handovers, retired>> /\ MigUnchanged

\* A round: the target takes the source's log as it is.
Round(t, c) == LET s == partner[t][c] IN
    /\ phase[t] = "serving" /\ st[t][c] = "receiving"
    /\ s # NoHost /\ phase[s] = "serving" /\ st[s][c] \in {"writer", "handing"}
    /\ mig[s][c] = mig[t][c] /\ log[t][c] # log[s][c]
    /\ log' = [log EXCEPT ![t][c] = log[s][c]]
    /\ UNCHANGED <<refs, st, MigState, accepted, msgs, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* What a target that gives up the migration of `c` it takes leaves: nothing of it, its reference
\* retired.
GivesUp(t, c) ==
    /\ SetSt(t, c, "none")
    /\ mig' = [mig EXCEPT ![t][c] = 0]
    /\ partner' = [partner EXCEPT ![t][c] = NoHost]
    /\ tok' = [tok EXCEPT ![t][c] = 0]
    /\ refs' = refs \ {Key(t, c)}

\* The target gives the migration up of its own accord, before any request: idle too long, or its
\* bucket calls timed out.
TargetAbort(t, c) ==
    /\ phase[t] = "serving" /\ st[t][c] = "receiving"
    /\ GivesUp(t, c)
    /\ UNCHANGED <<log, took, accepted, msgs, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* The source loses a migration it prepared, before its hand-over: its prepare's answer lost, or
\* AbortMigration ends it (board 8577). It aborts it by ID, a message that may arrive late, or
\* never, and serves on.
SourceLoses(s, c) ==
    /\ phase[s] = "serving" /\ st[s][c] = "writer" /\ mig[s][c] # 0
    /\ mig' = [mig EXCEPT ![s][c] = 0]
    /\ partner' = [partner EXCEPT ![s][c] = NoHost]
    /\ msgs' = msgs \cup {[kind |-> "abort", cell |-> c, id |-> mig[s][c], to |-> partner[s][c]]}
    /\ UNCHANGED <<refs, st, log, tok, took, accepted, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* The source, its hand-over asked, the cell given up, aborts the migration (AbortMigration, board
\* 8577): it tells the target to give it up, by ID, and stays given up, asking again as before, the
\* target's answer saying where the cell is: refused, served here again; accepted, the target's.
\* Only in the models that set AbortsHanding (EphemeralAborts): it triples the states.
SourceAbortsHanding(s, c) ==
    /\ AbortsHanding
    /\ phase[s] = "serving" /\ st[s][c] = "handing"
    /\ msgs' = msgs \cup {[kind |-> "abort", cell |-> c, id |-> mig[s][c], to |-> partner[s][c]]}
    /\ IF On("abortAsksTarget")
       THEN UNCHANGED <<st, mig, partner>>
       ELSE /\ SetSt(s, c, "writer")
            /\ mig' = [mig EXCEPT ![s][c] = 0]
            /\ partner' = [partner EXCEPT ![s][c] = NoHost]
    /\ UNCHANGED <<refs, log, tok, took, accepted, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* The target hears an abort: taking that migration still, it gives it up, its reference too.
TargetHearsAbort(m) == LET t == m.to c == m.cell IN
    /\ m.kind = "abort" /\ phase[t] = "serving"
    /\ st[t][c] = "receiving" /\ mig[t][c] = m.id
    /\ GivesUp(t, c)
    /\ UNCHANGED <<log, took, accepted, msgs, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* The source, its guest paused and its prepare answered, asks for the hand-over at its last commit,
\* naming the incarnation and token an answer to its prepare gave, the one it heard
\* (sourceGivesUpFirst: its commits stop here). Without sourceRetiresLast, it retires its reference
\* now.
Hand(s, c) ==
    /\ phase[s] = "serving" /\ st[s][c] = "writer" /\ mig[s][c] # 0
    /\ \E a \in msgs :
        /\ a.kind = "prepared" /\ a.cell = c /\ a.id = mig[s][c]
        /\ msgs' = msgs \cup {[kind |-> "req", cell |-> c, id |-> mig[s][c], n |-> log[s][c],
                               to |-> partner[s][c], k |-> a.k, tok |-> a.tok]}
    /\ SetSt(s, c, "handing")
    /\ refs' = IF On("sourceRetiresLast") THEN refs ELSE refs \ {Key(s, c)}
    /\ UNCHANGED <<log, MigState, accepted, told, migrations, handovers, retired>> /\ MigUnchanged

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
          /\ UNCHANGED <<refs, st, log, mig, partner, tok, accepted, handovers>>
       \/ \* Taking it: accepts holding the log through the commit named (openAtNamed), the
          \* request naming its take (boundToPrepare).
          /\ st[t][c] = "receiving" /\ mig[t][c] = m.id
          /\ (On("openAtNamed") => log[t][c] = m.n)
          /\ (On("boundToPrepare") => tok[t][c] = m.tok)
          /\ st' = [st EXCEPT ![t][c] = "writer"]
          /\ mig' = [mig EXCEPT ![t][c] = 0]
          /\ partner' = [partner EXCEPT ![t][c] = NoHost]
          /\ tok' = [tok EXCEPT ![t][c] = 0]
          /\ accepted' = [accepted EXCEPT ![t] = @ \cup {m.id}]
          /\ refs' = IF On("targetRefersFirst") THEN refs ELSE refs \cup {Key(t, c)}
          /\ msgs' = msgs \cup {Answer(c, m.id, TRUE)}
          /\ handovers' = handovers + 1
          /\ UNCHANGED log
       \/ \* Taking it: refuses; refuseFinal drops what it took, its reference too.
          /\ st[t][c] = "receiving" /\ mig[t][c] = m.id
          /\ IF On("refuseFinal")
             THEN GivesUp(t, c)
             ELSE UNCHANGED <<st, mig, partner, tok, refs>>
          /\ msgs' = msgs \cup {Answer(c, m.id, FALSE)}
          /\ UNCHANGED <<log, accepted, handovers>>
       \/ \* Neither accepted nor taking it: no.
          /\ ~Remembers(t, c, m.id)
          /\ ~(st[t][c] = "receiving" /\ mig[t][c] = m.id)
          /\ msgs' = msgs \cup {Answer(c, m.id, FALSE)}
          /\ UNCHANGED <<refs, st, log, mig, partner, tok, accepted, handovers>>
    /\ UNCHANGED <<took, told, migrations, retired>> /\ MigUnchanged

\* The source reads an answer to its hand-over: yes, it gave the cell up, the migration kept until
\* it lets the cell go; no, it serves again.
SourceHears(s, c, a) ==
    /\ phase[s] = "serving" /\ st[s][c] = "handing"
    /\ a.kind = "ans" /\ a.cell = c /\ a.id = mig[s][c]
    /\ SetSt(s, c, IF a.yes THEN "gaveup" ELSE "writer")
    /\ mig' = IF a.yes THEN mig ELSE [mig EXCEPT ![s][c] = 0]
    /\ partner' = IF a.yes THEN partner ELSE [partner EXCEPT ![s][c] = NoHost]
    /\ UNCHANGED <<refs, log, tok, took, accepted, msgs, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* The source, having given the cell up, deletes it, retires its reference, and releases the
\* migration: it asks no more.
SourceGone(s, c) ==
    /\ phase[s] = "serving" /\ st[s][c] = "gaveup"
    /\ SetSt(s, c, "none")
    /\ log' = [log EXCEPT ![s][c] = 0]
    /\ refs' = refs \ {Key(s, c)}
    /\ mig' = [mig EXCEPT ![s][c] = 0]
    /\ partner' = [partner EXCEPT ![s][c] = NoHost]
    /\ msgs' = msgs \cup {[kind |-> "release", cell |-> c, id |-> mig[s][c], to |-> partner[s][c]]}
    /\ UNCHANGED <<tok, took, accepted, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* The target hears a release: it forgets it accepted that migration.
TargetHearsRelease(m) == LET t == m.to IN
    /\ m.kind = "release" /\ phase[t] = "serving" /\ m.id \in accepted[t]
    /\ accepted' = [accepted EXCEPT ![t] = @ \ {m.id}]
    /\ UNCHANGED <<refs, st, log, MigState, msgs, told, migrations, handovers, retired>>
    /\ MigUnchanged

\* ---- Hosts ----

HostUnchanged == UNCHANGED <<named, data, accepted, msgs, told, forked, gphase, gnamed, grefs,
    passes, migrations, took, handovers>>

\* A crash keeps the disk: writers, hand-overs asked and cells given up stay, with their migrations
\* (the marker of a cell handed, and the journal entry of one given up, name it and its target);
\* what was in memory goes (forks under way, migrations being taken or prepared).
Volatile(s) == s \in {"forking", "failing", "early", "receiving"}
Kept(s) == s \in {"handing", "gaveup"}
Crash(h) ==
    /\ phase[h] # "down" /\ crashes < MaxCrashes
    /\ crashes' = crashes + 1
    /\ phase' = [phase EXCEPT ![h] = "down"]
    /\ st' = [st EXCEPT ![h] = [c \in Cells |-> IF Volatile(@[c]) THEN "none" ELSE @[c]]]
    /\ mig' = [mig EXCEPT ![h] = [c \in Cells |-> IF Kept(st[h][c]) THEN @[c] ELSE 0]]
    /\ partner' = [partner EXCEPT ![h] = [c \in Cells |-> IF Kept(st[h][c]) THEN @[c] ELSE NoHost]]
    /\ tok' = [tok EXCEPT ![h] = [c \in Cells |-> 0]]
    /\ log' = [log EXCEPT ![h] = [c \in Cells |-> IF Volatile(st[h][c]) THEN 0 ELSE @[c]]]
    /\ UNCHANGED <<refs, inc, rlist, losses, retired>> /\ HostUnchanged

Start(h) ==
    /\ phase[h] = "down"
    /\ phase' = [phase EXCEPT ![h] = "starting"]
    /\ rlist' = [rlist EXCEPT ![h] = {r \in refs : r[2] = I(h)}]
    /\ UNCHANGED <<refs, inc, st, log, mig, partner, tok, crashes, losses, retired>>
    /\ HostUnchanged

Held(h, r) == st[h][r[1]] \in {"writer", "handing"}

Retire(h) ==
    /\ phase[h] = "starting"
    /\ LET gone == {r \in rlist[h] : ~On("retireOnlyUnheld") \/ ~Held(h, r)} IN
       /\ refs' = refs \ gone
       /\ retired' = retired + Cardinality(gone \cap refs)
    /\ phase' = [phase EXCEPT ![h] = "serving"]
    /\ rlist' = [rlist EXCEPT ![h] = {}]
    /\ UNCHANGED <<inc, st, log, mig, partner, tok, crashes, losses>> /\ HostUnchanged

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
    /\ tok' = [tok EXCEPT ![h] = [c \in Cells |-> 0]]
    /\ accepted' = [accepted EXCEPT ![h] = {}]
    /\ UNCHANGED <<named, data, refs, msgs, told, forked, gphase, gnamed, grefs, passes,
        crashes, migrations, took, handovers, retired>>

\* The VM manager decommissions an incarnation no host has any more.
Decommission(h, k) ==
    /\ k < inc[h] /\ \E x \in refs : x[2] = <<h, k>>
    /\ refs' = {r \in refs : r[2] # <<h, k>>}
    /\ UNCHANGED <<inc, phase, rlist, st, log, mig, partner, tok, crashes, losses, retired>>
    /\ HostUnchanged

Next ==
    \/ Drop \/ GcRead \/ GcList \/ GcDelete
    \/ \E h \in Hosts, c \in Cells :
        \/ ForkBegin(h, c) \/ ForkRead(h, c) \/ ForkLate(h, c) \/ ForkFail(h, c)
        \/ Commit(h, c) \/ Round(h, c) \/ TargetAbort(h, c) \/ Hand(h, c) \/ SourceGone(h, c)
        \/ SourceLoses(h, c) \/ SourceAbortsHanding(h, c)
        \/ \E t \in Hosts : SourcePrepares(h, t, c)
        \/ \E a \in msgs : SourceHears(h, c, a)
    \/ \E m \in msgs :
        TargetPrepares(m) \/ TargetAnswer(m) \/ TargetHearsAbort(m) \/ TargetHearsRelease(m)
    \/ \E h \in Hosts : Crash(h) \/ Start(h) \/ Retire(h) \/ Lose(h)
    \/ \E h \in Hosts, k \in 0..MaxLosses : Decommission(h, k)

Spec == Init /\ [][Next]_vars

HostPerms == Permutations(Hosts)

\* Whether message `a` may still act: an answer to a prepare only while a writer pursues its
\* migration, and one to a hand-over only while a source hands it over, migration IDs never coming
\* back once their sources move on. Every other may act however late.
Live(a) ==
    CASE a.kind = "prepared" -> \E s \in Hosts : st[s][a.cell] = "writer" /\ mig[s][a.cell] = a.id
      [] a.kind = "ans" -> \E s \in Hosts : st[s][a.cell] = "handing" /\ mig[s][a.cell] = a.id
      [] OTHER -> TRUE

\* What tells states apart: every variable, but the messages that can no longer act, from which
\* the states that differ only in them go on alike.
View == <<named, data, refs, inc, phase, rlist, st, log, mig, partner, tok, took, accepted,
          {a \in msgs : Live(a)}, told, forked, gphase, gnamed, grefs, passes, crashes, losses,
          migrations, handovers, retired>>

\* ---- Invariants ----

\* Whether the source of the migration of `c` that `t` takes still pursues it. A prepare arriving
\* late, its source's disk lost and the version collected since, is taken all the same; that take
\* reads nothing, as no hand-over of it ever comes, and the target gives it up (TargetAbort).
Pursued(t, c) == LET s == partner[t][c] IN
    s # NoHost /\ st[s][c] \in {"writer", "handing"} /\ mig[s][c] = mig[t][c]

\* Every cell a host writes, hands over, or takes from a source pursuing it reads its version whole.
NothingNeededGone ==
    \A h \in Hosts, c \in Cells :
        /\ st[h][c] \in {"writer", "handing"} => data
        /\ st[h][c] = "receiving" /\ Pursued(h, c) => data

\* At most one host serves a cell's guest.
OneWriter == \A c \in Cells : Cardinality({h \in Hosts : Serving(h, c)}) <= 1

\* A cell's writer holds every commit its guests were told.
HandOverLosesNothing == \A h \in Hosts, c \in Cells : st[h][c] = "writer" => log[h][c] >= told[c]

\* A writer migrating nowhere may always migrate to another host that holds the cell not: the
\* target takes its next prepare, a migration it lost there never refusing it for good.
MigratesAgain == \A s, t \in Hosts, c \in Cells :
    (/\ s # t /\ st[s][c] = "writer" /\ mig[s][c] = 0 /\ st[t][c] \in {"none", "receiving"}
     /\ migrations < MaxMigrations)
    => Takes(t, c, migrations + 1)

\* Reached, as models of specs/ephemeral/mutations show they are violated (Reaches*).
NoHandOver == handovers = 0
NoHandOverBack == handovers < 2
NoCollected == data
NoRetired == retired = 0
=============================================================================
