---
id: decision-3
title: cladefs assumes powersafe overwrite
date: '2026-11-21 17:00'
status: accepted
---
## Context

Board 5890: at seed 423 of the madsim profile `ephemeral_cells_survive_disk_faults`, a block the
guest was told was committed was lost. A cell's segment log is written through the page cache,
which writes back whole pages. Appending a record therefore writes again the 512-byte sector that
holds the end of the record synced before it, with the same bytes. The append's sync failed, the
host crashed before another, and the simulated disk turned that sector into garbage. Recovery then
found the previous record, told durable, broken.

The crash models (`host_fs`'s simulated disks and `local_cell::MemDir::crash_corrupting`, from
TASK-680) let any block written since its file's last sync become garbage, including bytes the
write left as they were. A drive that behaves so is what SQLite calls one without "powersafe
overwrite": writing part of a sector may damage the sector's other bytes on power loss.
SQLite assumes powersafe overwrite of Linux's disks by default
(`SQLITE_IOCAP_POWERSAFE_OVERWRITE`, since 3.7.10). Without it, SQLite pads each synced batch of
its WAL to a sector boundary, so that no later write shares a sector with synced bytes.

## Decision

cladefs assumes powersafe overwrite of the disks it runs on: a power loss during a write may
leave the bytes the write changes old, new, torn or garbage, but never damages a byte the write
leaves as it was, in the same sector or not.

The crash models hold to it: a block cut short as garbage is garbage only in the bytes its write
changes (`host_fs::sim::garbled`, and `MemDir`'s). Tests:
`host_fs::madsim::a_crash_damages_no_byte_a_write_leaves_as_it_was` and
`local_cell` `mem::tests::corrupting_crashes_damage_no_byte_written_as_it_was`.

## Consequences

- Logs are appended through the page cache as before: no padding of each flush to a sector
  boundary, and no change to how logs are written.
- On a drive that breaks powersafe overwrite (no power-loss protection, sectors torn into garbage),
  a power loss during a log append can lose the commit synced just before it. Supporting such
  drives would mean padding every synced batch of log records to a sector boundary and writing
  those sectors alone, as SQLite does without the assumption.
- A crash model that garbles unchanged bytes again would find this class of loss at once; such
  a change revisits this decision.
