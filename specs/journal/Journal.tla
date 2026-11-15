------------------------------ MODULE Journal ------------------------------
(***************************************************************************)
(* Copyright 2026 The fxdax Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Fxfs's journal, its objects' flushes, the device's flushes and the     *)
(* super-block (crates/fxfs/src/object_store/journal.rs, write_super_block;*)
(* object_manager.rs, journal_file_offsets and apply_mutation; TASK-149.11)*)
(* : that a super-block claims only what its journal flushed. The bug of  *)
(* 2026-10-31 broke it: the offsets read after the device's flush, an     *)
(* object's flush ending meanwhile, its EndFlush past the super-block's   *)
(* checkpoint, the super-block said the object's mutations before it were *)
(* in its layers, which the journal, cut there, didn't say (TASK-149.5,   *)
(* dead-ends/theory-commits-racing-a-super-block-lose-the-journal.md).    *)
(*                                                                         *)
(* The journal is a sequence of transactions, the i-th at offset i, each  *)
(* committed whole, at one offset, as write_and_apply_mutations writes    *)
(* them (TASK-444.1: one record a mutation before, which the code's       *)
(* traces, a transaction of several objects at one offset, weren't): a    *)
(* commit's mutations of some objects; an object's flush's BeginFlush,    *)
(* whose layer holds the object's mutations before it, with mutations of  *)
(* the store that will hold the layer; or its EndFlush, naming the        *)
(* BeginFlush, which commits the layer, with the mutations of that store. *)
(* Replay, finding an EndFlush, applies the object's mutations after its  *)
(* BeginFlush's transaction only (Journal::replay's `offset + 1`). What is durable is a prefix, as  *)
(* far as a device flush took it. Each object keeps the offsets of its    *)
(* earliest mutations in no layer (journal_checkpoints): `old`, of those  *)
(* its flush writes, and `cur`, since, None for none, which mutations,    *)
(* BeginFlush and EndFlush change as apply_mutation does (Checkpoints:    *)
(* Current, Old, Both). A super-block names a checkpoint, the offset its  *)
(* journal goes on from (super_block_journal_file_offset), and each       *)
(* object's earliest offset, from which replay applies its mutations, or  *)
(* the checkpoint if it names none. Replay after a cut applies an         *)
(* object's EndFlush within what is durable, its layer then the object's, *)
(* and each mutation from the object's offset on.                         *)
(*                                                                         *)
(* write_super_block pads the journal to a block, the checkpoint; flushes *)
(* the device as far, at least; and saves the super-block. Commits and    *)
(* objects' flushes go on meanwhile. The power may be cut at any point,   *)
(* and the filesystem mount again (Recover): replay keeps what is durable *)
(* and rebuilds each object's offsets from the transactions it applies,   *)
(* as Journal::replay does, from which the journal goes on.               *)
(*                                                                         *)
(* Its rules, each a member of Rules; specs/journal/mutations removes one *)
(* each, and must find the violation it prevents:                         *)
(*                                                                         *)
(* - offsetsAtPad: the objects' offsets are read with the checkpoint,     *)
(*   under the journal's writer lock, before the flush (NoLostMutation).  *)
(* - flushBeforeSave: the super-block is saved once the journal is        *)
(*   durable to its checkpoint (SuperBlockFlushed).                       *)
(* - replayFromOffsets: replay applies each object's mutations from its   *)
(*   offset, not from the checkpoint: the offsets it rebuilds, which the  *)
(*   next super-block names, start no later than its mutations in no     *)
(*   layer (NoLostMutation, after a second cut).                          *)
(* - beginFlushApart: a BeginFlush's transaction holds no mutation of the *)
(*   object it begins flushing (allocator.rs's `start` says so): one      *)
(*   applied after the BeginFlush is in no layer, and replay, which skips *)
(*   that transaction, loses it (NoLostMutation).                         *)
(***************************************************************************)
EXTENDS Naturals, Sequences, TLC

CONSTANTS
    Objects,    \* the objects (stores, the allocator) whose mutations the journal holds
    MaxLen,     \* the journal's transactions at most
    Rules       \* the rules in force

AllRules == {"offsetsAtPad", "flushBeforeSave", "replayFromOffsets", "beginFlushApart"}

ASSUME Rules \subseteq AllRules
ASSUME MaxLen \in Nat

\* No offset: transactions' offsets start at 1.
None == 0
\* No object, of a transaction that begins or ends no flush.
NoObj == "-"
ASSUME NoObj \notin Objects

VARIABLES
    journal,    \* the transactions written, in order
    durable,    \* how many of them a device flush made durable
    old,        \* old[o]: the earliest offset of the mutations o's flush is writing, or None
    cur,        \* cur[o]: the earliest offset of o's mutations since (or since its last flush)
    began,      \* began[o]: the offset of the BeginFlush of o's flush under way, or None
    sb,         \* the super-block on the device: [ckpt, off]
    pc,         \* write_super_block's step: "idle", "padded" or "flushed"
    padCkpt,    \* its checkpoint
    padOff      \* the objects' offsets it read

vars == <<journal, durable, old, cur, began, sb, pc, padCkpt, padOff>>

Max(a, b) == IF a > b THEN a ELSE b

\* A transaction: the objects it mutates, the object whose flush it begins or ends, and, of an
\* EndFlush, the offset of the flush's BeginFlush.
Txn(m, b, e, upTo) == [muts |-> m, begin |-> b, end |-> e, upTo |-> upTo]

\* The next transaction's offset.
At == Len(journal) + 1
Room == Len(journal) < MaxLen

\* journal_file_offsets: each object's earliest offset, the old one first.
Offsets == [o \in Objects |-> IF old[o] # None THEN old[o] ELSE cur[o]]

\* The objects `S` mutated at `at`: the first of an object's since its flush began, or since its
\* last, is its current offset (Checkpoints::Current, or Both beside Old).
Mutated(S, at) == [o \in Objects |-> IF o \in S /\ cur[o] = None THEN at ELSE cur[o]]

NoSuperBlock == [ckpt |-> 1, off |-> [o \in Objects |-> None]]

TypeOK ==
    /\ durable \in 0..Len(journal)
    /\ old \in [Objects -> 0..MaxLen]
    /\ cur \in [Objects -> 0..MaxLen]
    /\ began \in [Objects -> 0..MaxLen]
    /\ pc \in {"idle", "padded", "flushed"}

Init ==
    /\ journal = <<>>
    /\ durable = 0
    /\ old = [o \in Objects |-> None]
    /\ cur = [o \in Objects |-> None]
    /\ began = [o \in Objects |-> None]
    /\ sb = NoSuperBlock
    /\ pc = "idle"
    /\ padCkpt = 1
    /\ padOff = [o \in Objects |-> None]

\* A commit's mutations of the objects `S`.
Commit(S) ==
    /\ Room
    /\ S # {}
    /\ journal' = Append(journal, Txn(S, NoObj, NoObj, None))
    /\ cur' = Mutated(S, At)
    /\ UNCHANGED <<durable, old, began, sb, pc, padCkpt, padOff>>

\* o's flush begins (BeginFlush), none under way, with the mutations `S` of the store that will
\* hold its layer: the layer takes o's mutations so far, whose earliest offset is now old. After a
\* cut, o may have an old offset of a flush the cut ended, which a BeginFlush keeps
\* (Checkpoints::Old). A mutation of o's own in the transaction, without beginFlushApart, applies
\* after the BeginFlush, as the code may apply them in either order.
BeginFlush(o, S) ==
    /\ Room
    /\ began[o] = None
    /\ Offsets[o] # None
    /\ "beginFlushApart" \in Rules => o \notin S
    /\ journal' = Append(journal, Txn(S, o, NoObj, None))
    /\ old' = [old EXCEPT ![o] = Offsets[o]]
    /\ cur' = [p \in Objects |-> IF p = o THEN IF o \in S THEN At ELSE None ELSE Mutated(S, At)[p]]
    /\ began' = [began EXCEPT ![o] = At]
    /\ UNCHANGED <<durable, sb, pc, padCkpt, padOff>>

\* o's flush ends (EndFlush), its layer written, with the mutations `S` of the store holding it
\* (and of the allocator, for its own): the old offset goes.
EndFlush(o, S) ==
    /\ Room
    /\ began[o] # None
    /\ journal' = Append(journal, Txn(S, NoObj, o, began[o]))
    /\ old' = [old EXCEPT ![o] = None]
    /\ cur' = Mutated(S, At)
    /\ began' = [began EXCEPT ![o] = None]
    /\ UNCHANGED <<durable, sb, pc, padCkpt, padOff>>

\* A device flush, as a sync's or the journal's own, makes more of what was written durable.
DeviceFlush ==
    /\ \E d \in (durable + 1)..Len(journal): durable' = d
    /\ UNCHANGED <<journal, old, cur, began, sb, pc, padCkpt, padOff>>

\* write_super_block pads the journal, under the writer lock: the checkpoint is where the next
\* transaction goes, and, offsetsAtPad, the objects' offsets as of it.
Pad ==
    /\ pc = "idle"
    /\ pc' = "padded"
    /\ padCkpt' = At
    /\ padOff' = IF "offsetsAtPad" \in Rules THEN Offsets ELSE padOff
    /\ UNCHANGED <<journal, durable, old, cur, began, sb>>

\* It flushes the device, as far as the checkpoint at least, transactions written since maybe too;
\* read now, not offsetsAtPad, the offsets are of objects whose flushes may have ended since.
Flush ==
    /\ pc = "padded"
    /\ \E d \in Max(durable, padCkpt - 1)..Len(journal): durable' = d
    /\ padOff' = IF "offsetsAtPad" \in Rules THEN padOff ELSE Offsets
    /\ pc' = "flushed"
    /\ UNCHANGED <<journal, old, cur, began, sb, padCkpt>>

\* It saves the super-block, once flushed, if flushBeforeSave: the barrier committing the root that
\* names it is the step, which a cut finds done or not.
Save ==
    /\ \/ pc = "flushed"
       \/ pc = "padded" /\ "flushBeforeSave" \notin Rules
    /\ sb' = [ckpt |-> padCkpt, off |-> padOff]
    /\ pc' = "idle"
    /\ UNCHANGED <<journal, durable, old, cur, began, padCkpt, padOff>>

\* Where the super-block s says replay applies o's mutations from.
ReplayFromOf(s, o) == IF s.off[o] = None THEN s.ckpt ELSE s.off[o]

\* The last transaction of the first n ending o's flush, or 0.
RECURSIVE LastEnd(_, _)
LastEnd(o, n) == IF n = 0 THEN 0 ELSE IF journal[n].end = o THEN n ELSE LastEnd(o, n - 1)

\* Where replay from super-block s reads the journal from: the earliest offset it names.
ReaderStart(s) ==
    LET named == {s.off[o] : o \in {p \in Objects : s.off[p] # None}} \cup {s.ckpt}
    IN CHOOSE f \in named : \A g \in named : f <= g

\* Where replay from super-block s, of the first d transactions, applies o's mutations from: past
\* the BeginFlush of the last flush of o it finds ended, if it read the BeginFlush, as that flush's
\* layer holds o's mutations before (Journal::replay's `offset + 1`), or else from where s says.
ReplayFromAt(s, o, d) ==
    LET e == LastEnd(o, d)
        skip == IF e # 0 /\ journal[e].upTo >= ReaderStart(s) THEN journal[e].upTo + 1 ELSE 0
        from == IF "replayFromOffsets" \in Rules THEN ReplayFromOf(s, o) ELSE s.ckpt
    IN Max(from, skip)

\* o's offsets <<old, cur>> once transaction t at offset i applies to them, as apply_mutation
\* has it: a BeginFlush makes the earliest old, if there is one; a mutation makes a first current
\* offset; an EndFlush drops the old. A transaction that both mutates o and ends its flush leaves
\* the same in either order; one that begins it, which beginFlushApart rules out, applies the
\* mutation after.
Applied(o, t, i, st) ==
    LET earliest == IF st[1] # None THEN st[1] ELSE st[2]
        begun == IF t.begin = o /\ earliest # None THEN <<earliest, None>> ELSE st
        mutated == IF o \in t.muts /\ begun[2] = None THEN <<begun[1], i>> ELSE begun
    IN IF t.end = o THEN <<None, mutated[2]>> ELSE mutated

\* o's offsets once replay applies the transactions from i to d, from `st`.
RECURSIVE FoldFrom(_, _, _, _)
FoldFrom(o, i, d, st) ==
    IF i > d THEN st ELSE FoldFrom(o, i + 1, d, Applied(o, journal[i], i, st))

\* Each object's offsets as a replay from super-block s, of the journal's first d transactions,
\* rebuilds them: from the object's offset on, replayFromOffsets, or else from the checkpoint.
Replayed(s, d) == [o \in Objects |-> FoldFrom(o, ReplayFromAt(s, o, d), d, <<None, None>>)]

\* The power is cut, and the filesystem mounts again: replay keeps the journal as far as it finds
\* it durable, the first d transactions, rebuilds the objects' offsets from what it applies, and
\* the journal goes on after it. No flush is under way, nor a super-block's write.
RecoverTo(d) ==
        LET r == Replayed(sb, d) IN
        /\ journal' = SubSeq(journal, 1, d)
        /\ durable' = d
        /\ old' = [o \in Objects |-> r[o][1]]
        /\ cur' = [o \in Objects |-> r[o][2]]
        /\ began' = [o \in Objects |-> None]
        /\ pc' = "idle"
        /\ padCkpt' = 1
        /\ padOff' = [o \in Objects |-> None]
        /\ UNCHANGED sb

\* As far as any of what was written since the last flush.
Recover == \E d \in durable..Len(journal): RecoverTo(d)

Next ==
    \/ \E S \in SUBSET Objects: Commit(S)
    \/ \E o \in Objects, S \in SUBSET Objects: BeginFlush(o, S) \/ EndFlush(o, S)
    \/ DeviceFlush
    \/ Pad
    \/ Flush
    \/ Save
    \/ Recover

Spec == Init /\ [][Next]_vars

\* The objects are alike: a model may check one of each permutation of them.
ObjectsAlike == Permutations(Objects)

\* After a cut of all but the first d transactions, o's layer holds its mutations before the
\* BeginFlush that the last of them ending o's flush names (none before 1). Flushes of an object
\* follow each other, so the last is the latest.
LayerBelow(o, d) == LET e == LastEnd(o, d) IN IF e = 0 THEN 1 ELSE journal[e].upTo

\* A cut that leaves the first d transactions and super-block s loses no mutation: each is in its
\* object's layer, or replayed.
LostNothing(s, d) ==
    \A o \in Objects :
        LET below == LayerBelow(o, d)
            from == ReplayFromAt(s, o, d)
        IN \A i \in 1..d : o \in journal[i].muts => i < below \/ i >= from

\* A cut now loses no durable mutation.
NoLostMutation == LostNothing(sb, durable)

\* The super-block's journal is durable as far as its checkpoint.
SuperBlockFlushed == sb.ckpt - 1 <= durable

=============================================================================
