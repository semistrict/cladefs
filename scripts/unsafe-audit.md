You are auditing the unsafe Rust of cladefs (a filesystem and extent daemon) for soundness. Report
only; change nothing. The sandbox is read-only: read with shell commands, and write no file, a
temporary one included. Your labels are checked against an independent reviewer's.

For EVERY `unsafe` block, `unsafe impl` and `unsafe fn` call site in the files listed at the end,
read the code around it (the function, its callers, the type's other methods) and decide whether its
`// SAFETY:` comment (or `# Safety` doc, for an `unsafe fn`) is true:

1. Write the precondition the operation needs, exactly: for a pointer, what it must point to, for
   how long, and its alignment and size; for `from_raw_parts`, initialization and length; for
   `Send`/`Sync`, what another thread may do to the same memory; for a syscall or FFI call, the
   contract of the function called.
2. Label the comment:
   - `Correct`: the comment states the precondition, and the code makes it true on every path.
   - `Missing`: the code makes the precondition true, but the comment doesn't say why (absent,
     vague such as "as the caller says", or only part of it).
   - `Incorrect`: the precondition can be false: name the path, the caller, or the interleaving.
3. Check concurrency for each: can another thread touch the same memory meanwhile (a sibling
   method on a shared `&self`, a `Send`/`Sync` impl handing it over, a signal handler, another
   process mapping it)? Can a sibling method of the type break the invariant the block relies on
   (for example, a safe method that frees, truncates, unmaps or reallocates what the block points
   to)? A comment true for one thread but false under concurrency is `Incorrect`.

Be exact and skeptical: "Correct" needs the code to prove it, not the comment to assert it. Don't
report style. Read the code before deciding; don't guess.

Output: one line per unsafe site, tab-separated, nothing else on the line:

    FILE:LINE<TAB>LABEL<TAB>PRECONDITION<TAB>WHY

where LINE is the line of the `unsafe` keyword. After all lines, a section `## Incorrect` repeating
each Incorrect site with the full path or interleaving that breaks it, and how a test (a unit test,
a Miri run, a Shuttle or loom schedule) would show it.

The files:
