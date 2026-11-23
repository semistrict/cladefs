// cladefs_dax_last, the filesystem mounted last, which the tests' knobs (cladefs.dax_stray,
// dax_rebind_now, dax_rebind_every) act on, read or written on a path of a function that does not
// hold cladefs_dax_last_lock: a knob written as the filesystem unmounts, its dax read with no lock,
// used it after cladefs_dax_destroy freed it (board 8555). READ_ONCE is no exception here: the
// lock is what keeps the filesystem from going while a knob holds it, not the read's tearing.
// Mutants: board-8555-knobs-unlocked

@exists@
identifier fn;
@@
fn(...) {
... when != mutex_lock(&cladefs_dax_last_lock)
* cladefs_dax_last
...
}
