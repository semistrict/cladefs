// The result of a function returning int, the guest's error convention, called as a statement and
// dropped: as fxfs_dax_write_back_file dropped fxfs_dax_write_pages's, so that a writeback whose
// Fxfs commit failed said nothing and fsync returned as if its pages were in Fxfs (TASK-164.4,
// doc-13). A call meant to drop its result says so with a cast to void.
// Mutants: task-164.4-commit-failure-dropped

@defined@
identifier f;
@@
int f(...) { ... }

@declared@
identifier f;
parameter list ps;
@@
int f(ps);

@@
identifier defined.f;
expression list es;
@@
* f(es);

@@
identifier declared.f;
expression list es;
@@
* f(es);
