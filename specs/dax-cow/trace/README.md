# specs/dax-cow/trace: dax.c's runs as behaviors of DaxCow

TASK-444.1. TLC and `../mutations` check the design. This checks that dax.c behaves as the
design says (trace validation, as `specs/follower/trace` does it for followers, and as CCF and
MongoDB do it). `guest/sim/tests/traced` runs dax.c unmodified over guest/sim's fake kernel: one
file of 4 to 12 pages, two storers storing through one mapping and by write(2), a task fsyncing,
and the budget's worker writing back and rebinding, with the seed's interleaving.
`guest/sim/kernel/spec_trace.c` writes each step it takes as one of DaxCow's actions, with what
it saw: the page, the pages a swap took, the pages a rebind's look found it would rebind, the
lock a swap or a rebind held. `TraceDaxCow.tla` follows the trace through the spec. The run is a
behavior of DaxCow if TLC reaches the trace's end, which violates `TraceMatched`. A step the spec
doesn't allow stops TLC short of the end, and the record it stopped at is the divergence. A step
that leads the spec to break one of its invariants violates that invariant instead.

The steps are taken from the fake kernel and the shim, so dax.c is left as it is:

- the file's semaphore and writeback lock, by name, in `kernel/locks.c`;
- `kernel/mm.c`: a PTE made writable, a write-protect, a cleaning, and a store landing or
  faulting;
- the device's COMMIT and WRITE, and `mount.c`'s fsync;
- the fault and rebind tracepoints, through `shim/linux/tracepoint.h`'s weak `sim_tp_*`;
- pages.c's transitions, Fxfs's writeback and its lookups of extents, linker-wrapped (the
  Makefile's `WRAPPED`).

Where the code does in one step what the spec does in several, the tracer writes each of the
spec's steps there, in order. For example, an iomap's dirtying is written as the budget, the lock
and the dirtying. Where the spec made atomic what the code does in steps, the spec was split
first (DaxCow's header).

## The runs

`runs/seed-N/TraceDaxCowData.tla` holds the traces of `tests/traced`'s seeds. Each must be
matched to its end. `runs/mutant-NAME-seed-N/` holds the traces of a past bug of
`guest/mutants` put back, which the check must reject. Each trace names the digest of the
sources it is of (`guest/sim/trace-sources.sh`). `scripts/check-tla.sh` (with every model, or
alone as `dax-cow/traces`) fails a trace whose digest isn't the tree's, as it no longer says
anything of dax.c as it is. When those sources change, make the traces again on Linux
(guest/sim needs Linux; ubu1):

```
guest/sim/kernel-tree.sh $KERNEL
KERNEL=... BUILD=... guest/sim/trace.sh -s 12 -o 100 -d OUT [-m MUTANT]...
```

Then copy `OUT/seed-*` in `runs/` in place of what is there, along with the mutants' runs
`check-tla.sh` lists, and check them with `scripts/check-tla.sh dax-cow/traces` on any machine
with Java. A trace that is no longer matched is a divergence: decide whether the code or the
spec is wrong, and file a task with the seed.

## What it can't see

Data aren't traced: the spec's versions follow its own steps. A write(2)'s copy is placed at its
iomap_end, and a writeback's copy at its WRITE. A store landing in between is to a page that is
dirty again, and no step of the spec tells the two orders apart. The host's side (installs,
evictions, the loader) isn't in guest/sim. Neither are punches as dax.c does them (a range
dropped and mapped again, which DaxCow doesn't model, TASK-444.13). The run has one file of one
chunk: its pages are its range's.
