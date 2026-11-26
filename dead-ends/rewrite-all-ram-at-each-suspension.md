# Writing the whole guest RAM to a new file, flushed and dropped from the page cache, at each suspension

**Tried:** 2026-11-15, board 13203, never committed (the first version of `suspend::move_to_file`).
**Idea:** the simplest way to take a suspended guest's RAM out of memory: scan every page, write
the non-zero ones to a new unnamed file, `fdatasync`, `POSIX_FADV_DONTNEED`, and map the file
where the RAM was. Memory comes back at once, page cache included.
**Result:** in a debug build, a 256 MiB test guest took 140 ms to suspend the first time and
560-600 ms every time after. The cost was the writing, not the fsync (9 ms). From the second
suspension on, the RAM was the first file's mapping, its page cache dropped, so the scan read the
whole RAM back from disk only to write it to the next file. The 20-round test of suspensions right
after calls took 12 s.
**Why it's dead:** the work is proportional to all of RAM, when only what the guest changed since
the last move is new. Now `/proc/self/pagemap` says which pages are the process's own (written
since the mapping, or never a file's), and only those are written: into the machine's own file in
place from the second suspension on, into a new file holding the snapshot's data extents for a
restored clone. Writeback runs in the background, and the kernel reclaims the clean pages when it
needs memory. In a release build: 21 ms for the first suspension, 5 ms median after.
**Reopen if:** pagemap stops reporting PM_FILE to unprivileged processes, or a guest's RAM is
mapped in a way pagemap cannot see (hugetlbfs, guest_memfd).
