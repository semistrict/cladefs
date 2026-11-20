---------------------------- MODULE Replication ----------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Replicated commits (doc-21, TASK-537.2): a cell's commits on a majority *)
(* of hosts' disks before the bucket, and their recovery by a take on any  *)
(* host. Quorum-acked primary-backup: each configuration, its writer and   *)
(* 2f peers, holds a write-once log, which only its writer appends to; the *)
(* bucket's head names the active one and changes only by conditional      *)
(* writes (CAS), fenced by the writer's epoch. No member votes.            *)
(*                                                                         *)
(* What is modelled, per doc-21:                                           *)
(*                                                                         *)
(* - The head: the epoch, the active configuration, and the uploaded       *)
(*   prefix of the cell's history (`up`). A take raises the epoch; an      *)
(*   upload's acknowledgement, an activation and a replacement are CASes   *)
(*   on it, refused to a writer whose epoch it no longer has.              *)
(* - A configuration: a key <<epoch, n>> (its writer's epoch, and how many *)
(*   replacements that writer made before it), its f, its writer, and its  *)
(*   members as <<host, incarnation>> pairs. `sent[k]` is what its writer  *)
(*   sent on its streams: its log. Sent stays after the writer dies, as    *)
(*   messages in flight do, and members take it in order (a TCP stream),   *)
(*   when they will: delays, loss (never taken) and a zombie writer are    *)
(*   all behaviours of it.                                                 *)
(* - A member's log of each configuration (`mlog`), how much of it it      *)
(*   dropped (`drop`), and its promise: the greatest epoch that fenced it. *)
(*   Its writer's own copy, on its own host, is a member too, but takes    *)
(*   no notice of promises (it is the writer's local WAL).                 *)
(* - A host's incarnation (`inc`), new each time its disk is lost: a       *)
(*   member is a host in an incarnation, and a host whose disk was lost,   *)
(*   its promises with it, is no longer that member (BookKeeper's          *)
(*   cookies).                                                             *)
(* - Writers: a take (fence, recover, install, activate), commits,          *)
(*   Replicated acknowledgements, uploads in two steps (land, then the     *)
(*   head's swap), replacements of members (or a drop to f = 0) by a CAS   *)
(*   that lands, then is learned. A writer never learns it was fenced but  *)
(*   by a refused CAS, so a fenced writer goes on, a zombie, as long as    *)
(*   the model lets it.                                                    *)
(* - Crashes: a daemon's (its sessions gone, its disk kept), and a disk's  *)
(*   (the host's logs and promises gone, a new incarnation).               *)
(*                                                                         *)
(* `told` is the truth: the commits a guest was told are Replicated or     *)
(* Uploaded. Every one must be in the history of the writer the head's     *)
(* epoch names, once it serves (ReplicatedSurvives).                       *)
(*                                                                         *)
(* The protocol's rules, each a member of Rules; specs/replication/        *)
(* mutations removes one each, and must find the violation it prevents:    *)
(*                                                                         *)
(* - fence: a take raises the promise of the members it reads, so that the *)
(*   old writer's appends are refused after (NoFence).                      *)
(* - fenceOthers: a take reads f+1 members other than the old writer, whose *)
(*   own copy no fence stops, unless the taker is that host                 *)
(*   (FenceCountsWriter).                                                   *)
(* - longest: a take recovers from the longest log it read (ReadOneMember).*)
(* - installFirst: a configuration is activated (or replaces another) only *)
(*   once f+1 of its members hold what its writer has told                 *)
(*   (ActivateBeforeInstall).                                              *)
(* - installOwn: what f+1 members of a take's configuration must hold     *)
(*   before it activates includes what the taker recovered from its own    *)
(*   copy, the old writer's, beyond the members it read                    *)
(*   (InstallMembersOnly).                                                 *)
(* - activate: a taker serves only once the head names its configuration   *)
(*   (NoActivation).                                                       *)
(* - joint: while a replacement's swap is in flight, a commit is           *)
(*   Replicated only on a quorum of both configurations (NoJointRule).     *)
(* - dropAcked: a member drops only what the head acknowledged             *)
(*   (DropBeforeAck).                                                      *)
(* - promise: a member refuses appends of an epoch below its promise       *)
(*   (NoPromise).                                                          *)
(* - incarnation: a member is a host in one incarnation (NoIncarnation).   *)
(* - zeroAfterUpload: a writer drops to f = 0 only once the bucket holds   *)
(*   what it told Replicated (ZeroFWithoutUpload).                         *)
(* - zeroNoQuorum: an f = 0 configuration holds no quorum, so nothing is   *)
(*   told Replicated while a drop to it is in flight (NoZeroQuorum).       *)
(* - abandonUnissued: a writer gives a replacement up (`abandon`) only     *)
(*   before it sends its CAS (AbandonIssued, TASK-537.3.5).                *)
(* - aloneSwitch: a take that could give what it recovered no members of   *)
(*   its own serves alone (f = 0) while the head names the configuration   *)
(*   before; the acknowledgement that first covers what it recovered names *)
(*   its own configuration in the same CAS, and none before it acknowledges*)
(*   past it (AloneNoSwitch, TASK-537.3.6). So does a take that recovered  *)
(*   from its own copy more than its members can be given (TASK-678), its  *)
(*   own configuration of F members taking its commits meanwhile.          *)
(* - deferQuiet: a take serving so tells nothing Replicated until the head *)
(*   names its configuration (DeferTellsReplicated, TASK-678).             *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Hosts,          \* the hosts sharing the bucket
    NoHost,         \* no host
    F,              \* the f of each configuration a take makes
    MaxCommits,     \* commits at most, over all writers
    MaxTakes,       \* takes at most: the epochs
    MaxReplaces,    \* replacements at most, per writer
    MaxLosses,      \* disks lost at most
    MaxCrashes,     \* daemons crashed at most
    Can,            \* the kinds of step the model takes, of AllCan
    Rules           \* the protocol's rules in force (all, but in a mutation)

AllRules == {"fence", "fenceOthers", "longest", "installFirst", "activate", "joint",
             "dropAcked", "promise", "incarnation", "zeroAfterUpload", "zeroNoQuorum",
             "abandonUnissued", "aloneSwitch", "installOwn", "deferQuiet"}

ASSUME Rules \subseteq AllRules
ASSUME F \in Nat /\ Cardinality(Hosts) >= 2 * F + 1
ASSUME NoHost \notin Hosts
\* The kinds of step a model may leave out, so that each model checks the rules its steps reach
\* in minutes: uploads (land, acknowledge), members' drops of what the head acknowledged, sweeps
\* of configurations the head no longer names, and replacements dropping to f = 0 (with "zero",
\* every replacement is such a drop: replacements keeping f are checked by the models without it). Takes, fences,
\* commits, deliveries, Replicated acknowledgements, replacements, crashes and losses are always
\* in (bounded by their Max constants).
\* And replacements given up, their members not taking the tail in time ("abandon", TASK-537.3.5);
\* and takes serving alone, no members to be had for their own configuration ("alone",
\* TASK-537.3.6); and takes serving so with members, deferring their activation to the
\* acknowledgement that covers what they recovered ("defer", TASK-678).
AllCan == {"upload", "drop", "sweep", "zero", "abandon", "alone", "defer"}
ASSUME Can \subseteq AllCan

R(rule) == rule \in Rules

Ids == 1..MaxCommits
Epochs == 1..MaxTakes
Keys == (0..MaxTakes) \X (0..MaxReplaces + 1)

Max(a, b) == IF a > b THEN a ELSE b
Min2(a, b) == IF a < b THEN a ELSE b
Range(s) == {s[i] : i \in DOMAIN s}
IsPrefix(a, b) == Len(a) <= Len(b) /\ a = SubSeq(b, 1, Len(a))
\* A configuration key's order: by epoch, then by replacement.
Later(k, j) == k[1] > j[1] \/ (k[1] = j[1] /\ k[2] > j[2])

NoCfg == [key |-> <<0, 0>>, f |-> 0, w |-> NoHost, members |-> {}]
NoReply == [log |-> <<>>, drop |-> 0]
NoSes == [st |-> "none", ep |-> 0, old |-> NoCfg, fenced |-> {},
          replies |-> [h \in Hosts |-> NoReply], cfg |-> NoCfg, pend |-> NoCfg, issued |-> FALSE,
          cas |-> NoCfg, landed |-> 0, replaces |-> 0, alone |-> FALSE, need |-> 0, inst |-> 0]

VARIABLES
    head,       \* the bucket's head: [epoch, cfg, up]
    sent,       \* sent[k]: the log configuration k's writer sent its members
    cfgs,       \* cfgs[k]: configuration k, as its writer made it
    mlog,       \* mlog[h][k]: host h's log of configuration k
    drop,       \* drop[h][k]: how much of it h dropped
    promise,    \* promise[h]: the greatest epoch that fenced h
    inc,        \* inc[h]: h's incarnation
    ses,        \* ses[h]: the writer session on h, or NoSes
    told,       \* the commits told Replicated or Uploaded
    next,       \* commits made
    losses,     \* disks lost
    crashes     \* daemons crashed

vars == <<head, sent, cfgs, mlog, drop, promise, inc, ses, told, next, losses, crashes>>

Member(h, c) == [h |-> h, inc |-> inc[h]] \in c.members
          \/ (~R("incarnation") /\ \E m \in c.members : m.h = h)
HostsOf(c) == {m.h : m \in c.members}

\* How many of c's members hold its log's first k commits.
Holding(c, k) == Cardinality({h \in HostsOf(c) : Member(h, c) /\ Len(mlog[h][c.key]) >= k})
\* An f = 0 configuration holds no quorum: a taker that finds it reads the bucket alone, so nothing
\* is Replicated in it, nor, by the joint rule, while a writer's drop to it is in flight (found by
\* TLC: a commit told Replicated on the old configuration and the writer's own copy, the drop's
\* swap landed, a taker recovering the bucket alone).
Quorum(c, k) == (c.f >= 1 \/ ~R("zeroNoQuorum")) /\ Holding(c, k) >= c.f + 1

\* The longest prefix of a log every commit of which was told.
ToldLen(log) == LET ks == {k \in 0..Len(log) : Range(SubSeq(log, 1, k)) \subseteq told}
                IN CHOOSE k \in ks : \A j \in ks : j <= k

TypeOK ==
    /\ head.epoch \in 0..MaxTakes
    /\ \A h \in Hosts : ses[h].st \in {"none", "fencing", "installing", "serving"}
    /\ told \subseteq Ids

Init ==
    /\ head = [epoch |-> 0, cfg |-> NoCfg, up |-> <<>>]
    /\ sent = [k \in Keys |-> <<>>]
    /\ cfgs = [k \in Keys |-> NoCfg]
    /\ mlog = [h \in Hosts |-> [k \in Keys |-> <<>>]]
    /\ drop = [h \in Hosts |-> [k \in Keys |-> 0]]
    /\ promise = [h \in Hosts |-> 0]
    /\ inc = [h \in Hosts |-> 0]
    /\ ses = [h \in Hosts |-> NoSes]
    /\ told = {}
    /\ next = 0
    /\ losses = 0
    /\ crashes = 0

(***************************************************************************)
(* Members                                                                 *)
(***************************************************************************)

\* Member h takes the next commit of configuration k's stream: if it is a member, and k's epoch is
\* not below its promise (its writer's own copy minds no promise).
Deliver(h, k) ==
    LET c == cfgs[k] IN
    /\ c # NoCfg
    /\ Member(h, c)
    /\ Len(mlog[h][k]) < Len(sent[k])
    /\ h = c.w \/ ~R("promise") \/ promise[h] <= k[1]
    /\ mlog' = [mlog EXCEPT ![h][k] = Append(@, sent[k][Len(@) + 1])]
    /\ UNCHANGED <<head, sent, cfgs, drop, promise, inc, ses, told, next, losses, crashes>>

\* Member h drops what of configuration k the head acknowledged (dropAcked), or what its writer
\* landed (without the rule); or the whole of a configuration the head no longer names.
Drop(h, k) ==
    /\ \/ /\ "drop" \in Can
          /\ LET limit == IF R("dropAcked") THEN Len(head.up)
                          ELSE LET w == cfgs[k].w IN
                               IF w # NoHost /\ ses[w].cfg.key = k THEN ses[w].landed ELSE 0
               IN /\ drop[h][k] < Min2(limit, Len(mlog[h][k]))
                  /\ drop' = [drop EXCEPT ![h][k] = Min2(limit, Len(mlog[h][k]))]
          /\ UNCHANGED mlog
       \/ /\ "sweep" \in Can
          /\ Later(head.cfg.key, k)
          /\ mlog[h][k] # <<>>
          /\ mlog' = [mlog EXCEPT ![h][k] = <<>>]
          /\ drop' = [drop EXCEPT ![h][k] = 0]
    /\ UNCHANGED <<head, sent, cfgs, promise, inc, ses, told, next, losses, crashes>>


(***************************************************************************)
(* A take                                                                  *)
(***************************************************************************)

\* h fences member m of configuration c at epoch e (without the rule fence, reads it only).
FenceOf(m, c, e) ==
    [promise EXCEPT ![m] = IF R("fence") THEN Max(@, e) ELSE @]

\* The VM manager takes the cell on h: the head's epoch raised by a CAS. A taker that was a member
\* of the active configuration fences and reads its own copy at once.
Take(h) ==
    LET e == head.epoch + 1
        c == head.cfg
        self == Member(h, c) /\ promise[h] < e
    IN
    /\ ses[h].st = "none"
    /\ e <= MaxTakes
    /\ head' = [head EXCEPT !.epoch = e]
    /\ promise' = IF self THEN FenceOf(h, c, e) ELSE promise
    /\ ses' = [ses EXCEPT ![h] = [NoSes EXCEPT !.st = "fencing", !.ep = e, !.old = c,
                  !.fenced = IF self THEN {h} ELSE {},
                  !.replies = [m \in Hosts |-> IF self /\ m = h
                                   THEN [log |-> mlog[h][c.key], drop |-> drop[h][c.key]]
                                   ELSE NoReply]]]
    /\ UNCHANGED <<sent, cfgs, mlog, drop, inc, told, next, losses, crashes>>

\* h's take fences member m of the configuration it found, and reads its log: if m has promised no
\* greater epoch.
Fence(h, m) ==
    LET s == ses[h]
        c == s.old
    IN
    /\ s.st = "fencing"
    /\ m \notin s.fenced
    /\ Member(m, c)
    /\ promise[m] <= s.ep
    /\ promise' = FenceOf(m, c, s.ep)
    /\ ses' = [ses EXCEPT ![h].fenced = @ \cup {m},
                          ![h].replies[m] = [log |-> mlog[m][c.key], drop |-> drop[m][c.key]]]
    /\ UNCHANGED <<head, sent, cfgs, mlog, drop, inc, told, next, losses, crashes>>

\* The members whose reads count: f+1 of them must be other than the old writer, whose own copy no
\* fence stops, unless h is the old writer's host (fenceOthers).
Counted(h) ==
    LET s == ses[h] IN
    IF R("fenceOthers") THEN {m \in s.fenced : m # s.old.w \/ m = h} ELSE s.fenced

\* What h's take recovers: the head's uploaded prefix, extended by the longest log it read (or by
\* any one it read, without the rule longest) that dropped nothing the head has not.
Recovered(h, r) ==
    LET up == head.up
        log == ses[h].replies[r].log
    IN IF ses[h].replies[r].drop <= Len(up) /\ Len(log) > Len(up) THEN log ELSE up

\* What f+1 members of h's configuration must hold before it activates: all it recovered
\* (installOwn); without the rule, only what the members it read held, its own copy aside if it is
\* the old writer's (a reopen on the writer's host recovering its own log).
InstallLen(h, rec) ==
    LET s == ses[h]
        rs == {r \in Counted(h) : r # h \/ h # s.old.w}
        lens == {Len(Recovered(h, r)) : r \in rs}
    IN IF R("installOwn") \/ s.old = NoCfg \/ s.old.f = 0 THEN Len(rec)
       ELSE IF rs = {} THEN Len(head.up)
       ELSE CHOOSE n \in lens : \A m \in lens : m <= n

Readable(h) ==
    LET s == ses[h] IN
    {r \in Counted(h) : s.replies[r].drop <= Len(head.up)}

\* h's take, fenced enough, recovers and installs its configuration: h and 2f others.
Recover(h) ==
    LET s == ses[h]
        c == s.old
        k == <<s.ep, 0>>
    IN
    /\ s.st = "fencing"
    /\ c = NoCfg \/ c.f = 0 \/ Cardinality(Counted(h)) >= c.f + 1
    /\ \E rec \in IF c = NoCfg \/ c.f = 0 THEN {head.up}
                  ELSE IF R("longest")
                       THEN {Recovered(h, r) : r \in {r \in Counted(h) :
                               \A q \in Counted(h) : Len(Recovered(h, q)) <= Len(Recovered(h, r))}}
                       ELSE {Recovered(h, r) : r \in Counted(h)}:
       \E others \in SUBSET (Hosts \ {h}) :
          /\ Cardinality(others) = 2 * F
          /\ LET new == [key |-> k, f |-> F, w |-> h,
                         members |-> {[h |-> m, inc |-> inc[m]] : m \in others \cup {h}}]
             IN /\ cfgs' = [cfgs EXCEPT ![k] = new]
                /\ sent' = [sent EXCEPT ![k] = rec]
                \* What the take read is spent: kept, it would only tell states apart.
                /\ ses' = [ses EXCEPT ![h].st = "installing", ![h].cfg = new, ![h].old = NoCfg,
                                      ![h].fenced = {}, ![h].replies = NoSes.replies,
                                      ![h].inst = InstallLen(h, rec)]
    /\ UNCHANGED <<head, mlog, drop, promise, inc, told, next, losses, crashes>>

\* h's take, fenced enough, recovers as Recover does, but has no members for a configuration of
\* its own (TASK-537.3.6): it serves alone, at f = 0, the head naming the configuration before,
\* until an acknowledgement covers what it recovered (`need`), which names its own (aloneSwitch).
\* Or (TASK-678, "defer") it has members, h and 2F others, but defers its configuration's
\* activation so, as a take does whose own copy, the old writer's, held more than its members can
\* be given: its members take its commits from now on, and it tells none Replicated meanwhile
\* (deferQuiet). The model lets any take defer, whatever it recovered.
RecoverAlone(h) ==
    LET s == ses[h]
        c == s.old
        k == <<s.ep, 0>>
    IN
    /\ s.st = "fencing"
    /\ c # NoCfg /\ c.f >= 1 /\ Cardinality(Counted(h)) >= c.f + 1
    /\ \E rec \in {Recovered(h, r) : r \in {r \in Counted(h) :
                   \A q \in Counted(h) : Len(Recovered(h, q)) <= Len(Recovered(h, r))}},
          f \in (IF "alone" \in Can THEN {0} ELSE {}) \cup (IF "defer" \in Can THEN {F} ELSE {}),
          others \in SUBSET (Hosts \ {h}) :
          /\ Cardinality(others) = 2 * f
          /\ LET new == [key |-> k, f |-> f, w |-> h,
                         members |-> {[h |-> m, inc |-> inc[m]] : m \in others \cup {h}}]
             IN /\ cfgs' = [cfgs EXCEPT ![k] = new]
                /\ sent' = [sent EXCEPT ![k] = rec]
                /\ ses' = [ses EXCEPT ![h].st = "serving", ![h].cfg = new, ![h].old = NoCfg,
                                      ![h].fenced = {}, ![h].replies = NoSes.replies,
                                      ![h].alone = TRUE, ![h].need = Len(rec)]
    /\ UNCHANGED <<head, mlog, drop, promise, inc, told, next, losses, crashes>>

\* h activates its configuration by a CAS of the head, once f+1 of its members hold what it
\* recovered (installFirst; all of it, by installOwn), and serves. Without the rule activate, it may serve without.
Activate(h) ==
    LET s == ses[h] IN
    /\ s.st = "installing"
    /\ ~R("installFirst") \/ Quorum(s.cfg, s.inst)
    /\ \/ /\ head.epoch = s.ep
          /\ head' = [head EXCEPT !.cfg = s.cfg]
       \/ /\ ~R("activate")
          /\ UNCHANGED head
    /\ ses' = [ses EXCEPT ![h].st = "serving"]
    /\ UNCHANGED <<sent, cfgs, mlog, drop, promise, inc, told, next, losses, crashes>>

(***************************************************************************)
(* A serving writer                                                        *)
(***************************************************************************)

Log(h) == sent[ses[h].cfg.key]

\* A commit, sent on the streams of h's configuration and of the one replacing it.
Commit(h) ==
    LET s == ses[h]
        id == next + 1
    IN
    /\ s.st = "serving"
    /\ next < MaxCommits
    /\ next' = id
    /\ sent' = [k \in Keys |-> IF k = s.cfg.key \/ (s.pend # NoCfg /\ k = s.pend.key)
                               THEN Append(sent[k], id) ELSE sent[k]]
    /\ UNCHANGED <<head, cfgs, mlog, drop, promise, inc, ses, told, losses, crashes>>

\* h tells the guest the first k commits of its log are Replicated: on a quorum of its
\* configuration, and of the one replacing it while that one's swap is in flight (joint).
AckReplicated(h) ==
    LET s == ses[h] IN
    /\ s.st = "serving"
    /\ s.cfg.f >= 1
    /\ ~s.alone \/ ~R("deferQuiet")
    /\ \E k \in 1..Len(Log(h)) :
          /\ ~(Range(SubSeq(Log(h), 1, k)) \subseteq told)
          /\ Quorum(s.cfg, k)
          /\ s.pend = NoCfg \/ ~R("joint") \/ Quorum(s.pend, k)
          /\ told' = told \cup Range(SubSeq(Log(h), 1, k))
    /\ UNCHANGED <<head, sent, cfgs, mlog, drop, promise, inc, ses, next, losses, crashes>>

\* h's uploader lands its log in the bucket, not yet named by the head. It lands the whole log as
\* it is: a prefix landed later is the whole log landed earlier, so no behaviour is lost.
Land(h) ==
    LET s == ses[h] IN
    /\ "upload" \in Can
    /\ s.st = "serving"
    /\ s.landed < Len(Log(h))
    /\ ses' = [ses EXCEPT ![h].landed = Len(Log(h))]
    /\ UNCHANGED <<head, sent, cfgs, mlog, drop, promise, inc, told, next, losses, crashes>>

\* The head's swap acknowledging what h landed, a CAS on its epoch; the guest told it is Uploaded.
Acknowledge(h) ==
    LET s == ses[h] IN
    /\ "upload" \in Can
    /\ s.st = "serving"
    /\ head.epoch = s.ep
    /\ s.landed > Len(head.up)
    \* Alone, the acknowledgement covering what it recovered names its own configuration too
    \* (aloneSwitch); without the rule, a replacement's swap later.
    /\ LET switch == s.alone /\ s.landed >= s.need /\ R("aloneSwitch")
       IN /\ head' = [head EXCEPT !.up = SubSeq(Log(h), 1, s.landed),
                                  !.cfg = IF switch THEN s.cfg ELSE @]
          /\ ses' = IF switch THEN [ses EXCEPT ![h].alone = FALSE] ELSE ses
    /\ told' = told \cup Range(SubSeq(Log(h), 1, s.landed))
    /\ UNCHANGED <<sent, cfgs, mlog, drop, promise, inc, next, losses, crashes>>

\* h replaces members of its configuration, or drops to f = 0 (itself alone): the new one's
\* stream starts with h's whole log, and takes its commits from now on.
Replace(h) ==
    LET s == ses[h]
        k == <<s.ep, s.replaces + 1>>
    IN
    /\ s.st = "serving"
    /\ s.pend = NoCfg
    /\ ~s.alone \/ ~R("aloneSwitch")
    /\ s.replaces < MaxReplaces
    /\ \E others \in SUBSET (Hosts \ {h}), f \in IF "zero" \in Can THEN {0} ELSE {F} :
          /\ Cardinality(others) = 2 * f
          /\ LET new == [key |-> k, f |-> f, w |-> h,
                         members |-> {[h |-> m, inc |-> inc[m]] : m \in others \cup {h}}]
             IN /\ cfgs' = [cfgs EXCEPT ![k] = new]
                /\ sent' = [sent EXCEPT ![k] = Log(h)]
                /\ ses' = [ses EXCEPT ![h].pend = new, ![h].replaces = @ + 1]
    /\ UNCHANGED <<head, mlog, drop, promise, inc, told, next, losses, crashes>>

\* h sends the replacement's CAS: once f+1 of its members hold all h told (installFirst), or, for
\* f = 0, once the bucket does (zeroAfterUpload).
Issue(h) ==
    LET s == ses[h]
        need == ToldLen(Log(h))
    IN
    /\ s.st = "serving"
    /\ s.pend # NoCfg
    /\ ~s.issued
    /\ IF s.pend.f = 0
       THEN ~R("zeroAfterUpload") \/ Len(head.up) >= need
       ELSE ~R("installFirst") \/ Quorum(s.pend, need)
    /\ ses' = [ses EXCEPT ![h].issued = TRUE, ![h].cas = s.pend]
    /\ UNCHANGED <<head, sent, cfgs, mlog, drop, promise, inc, told, next, losses, crashes>>

\* The bucket applies h's CAS, if the head is still h's at its epoch and configuration: the CAS
\* sent, `cas`, whether or not h gave its replacement up since.
Lands(h) ==
    LET s == ses[h] IN
    /\ s.st = "serving"
    /\ s.cas # NoCfg
    /\ head.epoch = s.ep
    /\ head.cfg = s.cfg \/ s.alone
    /\ head' = [head EXCEPT !.cfg = s.cas]
    /\ UNCHANGED <<sent, cfgs, mlog, drop, promise, inc, ses, told, next, losses, crashes>>

\* h learns its CAS landed: the replacement is its configuration from now on.
Learn(h) ==
    LET s == ses[h] IN
    /\ s.st = "serving"
    /\ s.issued
    /\ head.cfg = s.pend
    /\ ses' = [ses EXCEPT ![h].cfg = s.pend, ![h].pend = NoCfg, ![h].issued = FALSE,
                          ![h].cas = NoCfg, ![h].alone = FALSE]
    /\ UNCHANGED <<head, sent, cfgs, mlog, drop, promise, inc, told, next, losses, crashes>>

\* h gives its replacement up, its CAS not sent (abandonUnissued), as its members took not the tail
\* in time (TASK-537.3.5): its configuration alone counts again. Without the rule, a CAS in flight
\* may land after.
Abandon(h) ==
    LET s == ses[h] IN
    /\ "abandon" \in Can
    /\ s.st = "serving"
    /\ s.pend # NoCfg
    /\ ~s.issued \/ ~R("abandonUnissued")
    /\ ses' = [ses EXCEPT ![h].pend = NoCfg, ![h].issued = FALSE]
    /\ UNCHANGED <<head, sent, cfgs, mlog, drop, promise, inc, told, next, losses, crashes>>

(***************************************************************************)
(* Failures                                                                *)
(***************************************************************************)

\* h's daemon crashes: its session goes, its disk stays.
CrashDaemon(h) ==
    /\ crashes < MaxCrashes
    /\ ses[h] # NoSes
    /\ crashes' = crashes + 1
    /\ ses' = [ses EXCEPT ![h] = NoSes]
    /\ UNCHANGED <<head, sent, cfgs, mlog, drop, promise, inc, told, next, losses>>

\* h's disk is lost: its logs and promises with it, its session too, and it comes back a new
\* incarnation.
LoseDisk(h) ==
    /\ losses < MaxLosses
    /\ losses' = losses + 1
    /\ ses' = [ses EXCEPT ![h] = NoSes]
    /\ mlog' = [mlog EXCEPT ![h] = [k \in Keys |-> <<>>]]
    /\ drop' = [drop EXCEPT ![h] = [k \in Keys |-> 0]]
    /\ promise' = [promise EXCEPT ![h] = 0]
    /\ inc' = [inc EXCEPT ![h] = @ + 1]
    /\ UNCHANGED <<head, sent, cfgs, told, next, crashes>>

Next ==
    \E h \in Hosts :
        \/ Take(h) \/ Recover(h) \/ RecoverAlone(h) \/ Activate(h)
        \/ \E m \in Hosts : Fence(h, m)
        \/ Commit(h) \/ AckReplicated(h) \/ Land(h) \/ Acknowledge(h)
        \/ Replace(h) \/ Issue(h) \/ Lands(h) \/ Learn(h) \/ Abandon(h)
        \/ \E k \in Keys : Deliver(h, k) \/ Drop(h, k)
        \/ CrashDaemon(h) \/ LoseDisk(h)

Spec == Init /\ [][Next]_vars

\* A constraint for the mutation AloneNoSwitch alone, which needs three takes and three commits:
\* a writer's stream delivered to its own copy only, so that its peers hold nothing a take reads,
\* the old writer's own copy, on its host, the only log longer than what a take recovered.
OwnCopiesOnly == \A h \in Hosts, k \in Keys : mlog[h][k] # <<>> => cfgs[k].w = h

Symmetry == Permutations(Hosts)

(***************************************************************************)
(* Properties                                                              *)
(***************************************************************************)

\* The writer the head's epoch names, once it serves, holds every commit a guest was told.
ReplicatedSurvives ==
    \A h \in Hosts :
        (ses[h].st = "serving" /\ ses[h].ep = head.epoch) => told \subseteq Range(Log(h))

\* What the bucket holds is a prefix of that writer's history.
HistoryIsPrefix ==
    \A h \in Hosts :
        (ses[h].st = "serving" /\ ses[h].ep = head.epoch) => IsPrefix(head.up, Log(h))

\* Every member's log of a configuration is a prefix of its writer's.
MembersArePrefixes ==
    \A h \in Hosts, k \in Keys : IsPrefix(mlog[h][k], sent[k])

=============================================================================
