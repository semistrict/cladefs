# A rust-rules.py rule flagging sorts whose comparison calls a method

**Tried:** 2026-11-16, TASK-493 (not committed).
**Idea:** The page cache's flusher died on `files.sort_by_key(|file| file.used())`, keys read live
from an atomic and a mutex other threads change. A rule in scripts/rust-rules.py matching
`.sort(_unstable)_by(_key)(|..| ... x())` would find the next such sort as it is written.
**Result:** Over crates/ it matched one other sort, `entries.sort_by_key(|entry| entry.ino())` in
fxfs-posix's model tests: a pure accessor of a Copy value, no shared state.
**Why it's dead:** Whether a key reads state other threads change is a matter of the method's body
and the item's type, which a syntactic rule can't see; it flags pure accessors as often as live
reads. The structural fix holds instead: the page cache orders its files only through
`PageCache::files_by`, which takes each key once.
**Reopen if:** a lint with types (dylint) comes to the crates, which could flag a key closure that
reaches an atomic load or a lock.
