------------------------------ MODULE Journal ------------------------------
(***************************************************************************)
(* Copyright 2026 The cladefs Authors. All rights reserved.                  *)
(* Use of this source code is governed by a BSD-style license that can be *)
(* found in the LICENSE file.                                              *)
(*                                                                         *)
(* Fxfs's journal, its objects' flushes, the device's flushes, the single  *)
(* atomic super-block in Root, and direct reclamation of superseded        *)
(* super-block and position-derived journal block extents                  *)
(* (crates/fxfs/src/object_store/journal.rs, super_block.rs; TASK-149.11,  *)
(* boards 6923, 6926).                                                     *)
(*                                                                         *)
(* The journal is a sequence of records, the i-th at offset i: a commit's *)
(* mutation of an object; an object's flush's BeginFlush, whose layer     *)
(* holds the object's mutations before it; and its EndFlush, naming the   *)
(* BeginFlush, which commits the layer. What is durable is a prefix, as   *)
(* far as a device flush took it. Each object keeps the offset of its     *)
(* earliest mutation in no layer (journal_checkpoints: Current, Old while *)
(* it flushes, Both if it has more since): `old` and `cur` here, 0 for    *)
(* none. A super-block names a checkpoint, the offset its journal goes on *)
(* from (super_block_journal_file_offset), and each object's earliest     *)
(* offset, from which replay applies its mutations, or the checkpoint if  *)
(* it names none. Replay after a cut applies an object's EndFlush within  *)
(* what is durable, its layer then the object's, and each mutation from   *)
(* the object's offset on.                                                *)
(*                                                                         *)
(* Super-blocks are single write-once extents swapped atomically in Root  *)
(* (`commit_root`). Old super-block extents (`rootRetired`) and           *)
(* superseded journal block extents (`journalFreedTo .. MinReplayFrom`)   *)
(* are freed directly AFTER `commit_root`, and dropped from Root only     *)
(* AFTER a barrier seals their frees.                                     *)
(*                                                                         *)
(* Rules (each a member of Rules; specs/journal/mutations removes one     *)
(* each and must find the violation it prevents):                         *)
(*                                                                         *)
(* - offsetsAtPad: the objects' offsets are read with the checkpoint,     *)
(*   under the journal's writer lock, before the flush (NoLostMutation).  *)
(* - flushBeforeSave: the super-block is saved once the journal is        *)
(*   durable to its checkpoint (SuperBlockFlushed).                       *)
(* - freeAfterCommit: the old super-block and superseded journal blocks   *)
(*   are freed only after `commit_root` installs the new super-block      *)
(*   (ActiveReadable).                                                    *)
(* - pruneAfterSeal: retired super-blocks and `journal_freed_to` are      *)
(*   dropped from Root only after a barrier seals their frees             *)
(*   (NoLeakedExtent).                                                    *)
(***************************************************************************)
EXTENDS Naturals, Sequences

CONSTANTS
    Objects,    \* the objects (stores, the allocator) whose mutations the journal holds
    MaxLen,     \* the journal's records at most
    MaxSb,      \* the super-block extent IDs at most
    Rules       \* the rules in force

AllRules == {"offsetsAtPad", "flushBeforeSave", "freeAfterCommit", "pruneAfterSeal"}

ASSUME Rules \subseteq AllRules
ASSUME MaxLen \in Nat
ASSUME MaxSb \in Nat /\ MaxSb >= 2

\* No offset: records' offsets start at 1.
None == 0

VARIABLES
    journal,            \* the records written, in order
    durable,            \* how many of them a device flush made durable
    old,                \* old[o]: the earliest offset of the mutations o's flush is writing, or None
    cur,                \* cur[o]: the earliest offset of o's mutations since (or since its last flush)
    began,              \* began[o]: the offset of o's flush's BeginFlush, or None
    sb,                 \* the active super-block on the device: [ckpt, off]
    pc,                 \* write_super_block's step: "idle", "padded" or "flushed"
    padCkpt,            \* its checkpoint
    padOff,             \* the objects' offsets it read
    rootSb,             \* active super-block extent ID named by durable Root
    nextSb,             \* next super-block extent ID to mint
    rootRetired,        \* retired super-block extent IDs still tracked in durable Root
    sbOnDisk,           \* super-block extent IDs currently durable on disk (survive crash)
    sbFreedUnsealed,    \* retired super-block extent IDs freed since last sealing barrier
    journalFreedTo,     \* durable Root.journal_freed_to (offsets < this are sealed freed)
    journalUnsealedTo,  \* offsets < this have been freed since last sealing barrier
    journalOnDisk       \* journal block offsets currently durable on disk

vars == <<journal, durable, old, cur, began, sb, pc, padCkpt, padOff,
          rootSb, nextSb, rootRetired, sbOnDisk, sbFreedUnsealed,
          journalFreedTo, journalUnsealedTo, journalOnDisk>>

Max(a, b) == IF a > b THEN a ELSE b

Mut(o) == [kind |-> "mut", obj |-> o, upTo |-> None]
Begin(o) == [kind |-> "begin", obj |-> o, upTo |-> None]
End(o, at) == [kind |-> "end", obj |-> o, upTo |-> at]

\* The next record's offset.
At == Len(journal) + 1
Room == Len(journal) < MaxLen

\* journal_file_offsets: each object's earliest offset, the old one first.
Offsets == [o \in Objects |-> IF old[o] # None THEN old[o] ELSE cur[o]]

\* Earliest offset replay reads for `s`.
ReplayFromOf(s, o) == IF s.off[o] = None THEN s.ckpt ELSE s.off[o]
MinReplayFrom(s) ==
    LET vals == {ReplayFromOf(s, o) : o \in Objects} \cup {s.ckpt}
    IN CHOOSE m \in vals : \A v \in vals : m <= v

TypeOK ==
    /\ durable \in 0..Len(journal)
    /\ old \in [Objects -> 0..MaxLen]
    /\ cur \in [Objects -> 0..MaxLen]
    /\ began \in [Objects -> 0..MaxLen]
    /\ pc \in {"idle", "padded", "flushed"}
    /\ rootSb \in 1..MaxSb
    /\ nextSb \in 2..(MaxSb + 1)
    /\ rootRetired \subseteq (1..MaxSb)
    /\ sbOnDisk \subseteq (1..MaxSb)
    /\ sbFreedUnsealed \subseteq (1..MaxSb)
    /\ journalFreedTo \in 1..(MaxLen + 1)
    /\ journalUnsealedTo \in 1..(MaxLen + 1)
    /\ journalOnDisk \subseteq (1..MaxLen)

Init ==
    /\ journal = <<>>
    /\ durable = 0
    /\ old = [o \in Objects |-> None]
    /\ cur = [o \in Objects |-> None]
    /\ began = [o \in Objects |-> None]
    /\ sb = [ckpt |-> 1, off |-> [o \in Objects |-> None]]
    /\ pc = "idle"
    /\ padCkpt = 1
    /\ padOff = [o \in Objects |-> None]
    /\ rootSb = 1
    /\ nextSb = 2
    /\ rootRetired = {}
    /\ sbOnDisk = {1}
    /\ sbFreedUnsealed = {}
    /\ journalFreedTo = 1
    /\ journalUnsealedTo = 1
    /\ journalOnDisk = {}

\* A commit's mutation of `o`: its first since o's last flush began is o's current offset.
Commit(o) ==
    /\ Room
    /\ journal' = Append(journal, Mut(o))
    /\ cur' = IF cur[o] = None THEN [cur EXCEPT ![o] = At] ELSE cur
    /\ UNCHANGED <<durable, old, began, sb, pc, padCkpt, padOff,
                   rootSb, nextSb, rootRetired, sbOnDisk, sbFreedUnsealed,
                   journalFreedTo, journalUnsealedTo, journalOnDisk>>

\* o's flush begins (BeginFlush): its layer takes o's mutations so far, whose offset is now old.
BeginFlush(o) ==
    /\ Room
    /\ old[o] = None
    /\ cur[o] # None
    /\ journal' = Append(journal, Begin(o))
    /\ old' = [old EXCEPT ![o] = cur[o]]
    /\ cur' = [cur EXCEPT ![o] = None]
    /\ began' = [began EXCEPT ![o] = At]
    /\ UNCHANGED <<durable, sb, pc, padCkpt, padOff,
                   rootSb, nextSb, rootRetired, sbOnDisk, sbFreedUnsealed,
                   journalFreedTo, journalUnsealedTo, journalOnDisk>>

\* o's flush ends (EndFlush), its layer written: the old offset goes.
EndFlush(o) ==
    /\ Room
    /\ old[o] # None
    /\ journal' = Append(journal, End(o, began[o]))
    /\ old' = [old EXCEPT ![o] = None]
    /\ began' = [began EXCEPT ![o] = None]
    /\ UNCHANGED <<durable, cur, sb, pc, padCkpt, padOff,
                   rootSb, nextSb, rootRetired, sbOnDisk, sbFreedUnsealed,
                   journalFreedTo, journalUnsealedTo, journalOnDisk>>

\* A device flush (sync barrier): makes more journal blocks durable AND seals frees completed
\* before it, pruning sealed retired super-blocks and advancing Root.journal_freed_to.
DeviceFlush ==
    /\ \/ durable < Len(journal)
       \/ sbFreedUnsealed # {}
       \/ journalUnsealedTo > journalFreedTo
    /\ \E d \in durable..Len(journal):
        /\ (d > durable \/ sbFreedUnsealed # {} \/ journalUnsealedTo > journalFreedTo)
        /\ durable' = d
        /\ journalOnDisk' = (journalOnDisk \cup ((durable + 1)..d)) \ (1..(journalUnsealedTo - 1))
    /\ sbOnDisk' = sbOnDisk \ sbFreedUnsealed
    /\ rootRetired' = rootRetired \ sbFreedUnsealed
    /\ sbFreedUnsealed' = {}
    /\ journalFreedTo' = Max(journalFreedTo, journalUnsealedTo)
    /\ UNCHANGED <<journal, old, cur, began, sb, pc, padCkpt, padOff,
                   rootSb, nextSb, journalUnsealedTo>>

\* write_super_block pads the journal, under the writer lock: the checkpoint is where the next
\* record goes, and, offsetsAtPad, the objects' offsets as of it.
Pad ==
    /\ pc = "idle"
    /\ nextSb <= MaxSb
    /\ pc' = "padded"
    /\ padCkpt' = At
    /\ padOff' = IF "offsetsAtPad" \in Rules THEN Offsets ELSE padOff
    /\ UNCHANGED <<journal, durable, old, cur, began, sb,
                   rootSb, nextSb, rootRetired, sbOnDisk, sbFreedUnsealed,
                   journalFreedTo, journalUnsealedTo, journalOnDisk>>

\* It flushes the device, as far as the checkpoint at least, sealing any prior unsealed frees.
Flush ==
    /\ pc = "padded"
    /\ \E d \in Max(durable, padCkpt - 1)..Len(journal):
        /\ durable' = d
        /\ journalOnDisk' = (journalOnDisk \cup ((durable + 1)..d)) \ (1..(journalUnsealedTo - 1))
    /\ padOff' = IF "offsetsAtPad" \in Rules THEN padOff ELSE Offsets
    /\ sbOnDisk' = sbOnDisk \ sbFreedUnsealed
    /\ rootRetired' = rootRetired \ sbFreedUnsealed
    /\ sbFreedUnsealed' = {}
    /\ journalFreedTo' = Max(journalFreedTo, journalUnsealedTo)
    /\ pc' = "flushed"
    /\ UNCHANGED <<journal, old, cur, began, sb, padCkpt,
                   rootSb, nextSb, journalUnsealedTo>>

\* Mutation: if freeAfterCommit is NOT in Rules, old super-block is freed and sealed BEFORE
\* commit_root switches to the new super-block.
FreeBeforeCommit ==
    /\ "freeAfterCommit" \notin Rules
    /\ pc = "flushed"
    /\ rootSb \in sbOnDisk
    /\ sbOnDisk' = sbOnDisk \ {rootSb}
    /\ UNCHANGED <<journal, durable, old, cur, began, sb, pc, padCkpt, padOff,
                   rootSb, nextSb, rootRetired, sbFreedUnsealed,
                   journalFreedTo, journalUnsealedTo, journalOnDisk>>

\* Atomically writes the new super-block extent and commits Root (`commit_root`), moving the old
\* super-block into `rootRetired` and sealing any previously unsealed frees.
Save ==
    /\ \/ pc = "flushed"
       \/ pc = "padded" /\ "flushBeforeSave" \notin Rules
    /\ nextSb <= MaxSb
    /\ sb' = [ckpt |-> padCkpt, off |-> padOff]
    /\ rootSb' = nextSb
    /\ nextSb' = nextSb + 1
    /\ sbOnDisk' = (sbOnDisk \ sbFreedUnsealed) \cup {nextSb}
    /\ rootRetired' = (rootRetired \ sbFreedUnsealed) \cup {rootSb}
    /\ sbFreedUnsealed' = {}
    /\ journalOnDisk' = journalOnDisk \ (1..(journalUnsealedTo - 1))
    /\ journalFreedTo' = Max(journalFreedTo, journalUnsealedTo)
    /\ pc' = "idle"
    /\ UNCHANGED <<journal, durable, old, cur, began, padCkpt, padOff, journalUnsealedTo>>

\* After `commit_root`, frees retired super-blocks and superseded journal blocks (`< MinReplayFrom`).
\* With `pruneAfterSeal`, Root keeps tracking them until a sealing barrier runs; without it, Root
\* drops them immediately, so a crash before a barrier leaks the still-on-disk extents.
FreeRetired ==
    /\ LET newSbFreed == rootRetired \ sbFreedUnsealed
           reclaimTo == MinReplayFrom(sb)
       IN /\ (newSbFreed # {} \/ reclaimTo > journalUnsealedTo)
          /\ sbFreedUnsealed' = sbFreedUnsealed \cup newSbFreed
          /\ journalUnsealedTo' = Max(journalUnsealedTo, reclaimTo)
          /\ IF "pruneAfterSeal" \in Rules
             THEN UNCHANGED <<rootRetired, journalFreedTo>>
             ELSE /\ rootRetired' = {}
                  /\ journalFreedTo' = Max(journalFreedTo, reclaimTo)
    /\ UNCHANGED <<journal, durable, old, cur, began, sb, pc, padCkpt, padOff,
                   rootSb, nextSb, sbOnDisk, journalOnDisk>>

Next ==
    \/ \E o \in Objects: Commit(o) \/ BeginFlush(o) \/ EndFlush(o)
    \/ DeviceFlush
    \/ Pad
    \/ Flush
    \/ FreeBeforeCommit
    \/ Save
    \/ FreeRetired

Spec == Init /\ [][Next]_vars

\* After a cut, replay: o's layer holds its mutations before the BeginFlush the last durable
\* EndFlush of o names (none before 1); replay applies o's mutations from its offset on.
LayerBelow(o) ==
    LET ends == {i \in 1..durable : journal[i].kind = "end" /\ journal[i].obj = o}
        upTos == {journal[i].upTo : i \in ends}
    IN IF ends = {} THEN 1 ELSE CHOOSE u \in upTos : \A v \in upTos : v <= u

ReplayFrom(o) == ReplayFromOf(sb, o)

\* A cut now loses no durable mutation: each is in its object's layer, or replayed.
NoLostMutation ==
    \A o \in Objects : \A i \in 1..durable :
        journal[i] = Mut(o) => i < LayerBelow(o) \/ i >= ReplayFrom(o)

\* The super-block's journal is durable as far as its checkpoint.
SuperBlockFlushed == sb.ckpt - 1 <= durable

\* Every extent replay needs (the active super-block in Root and every durable journal block from
\* the super-block's replay start onwards) is still on disk.
ActiveReadable ==
    /\ rootSb \in sbOnDisk
    /\ \A i \in MinReplayFrom(sb)..durable : i \in journalOnDisk

\* Every super-block or journal extent still durable on disk after a crash is known to Root so
\* recovery can either use it or free it (zero leaked extents).
NoLeakedExtent ==
    /\ sbOnDisk \subseteq ({rootSb} \cup rootRetired)
    /\ \A i \in journalOnDisk : i >= journalFreedTo

=============================================================================
