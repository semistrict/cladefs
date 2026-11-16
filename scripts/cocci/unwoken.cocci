// A field a wait's condition reads, changed by a function that wakes none of those waiting on that
// wait's queue: as a range freed counted in `released`, and a round of the budget's worker in
// `rounds`, without waking their waiters, which waited out their timeouts instead, only slower,
// which nothing saw (TASK-149.18). The change and its wake belong in one function, as
// fxfs_dax_released keeps them.
// Mutants: task-149.18-release-unwoken task-149.18-round-unwoken

@waited@
type T;
T D;
identifier Q, F;
@@
(
wait_event(D->Q, <+... D->F ...+>)
|
wait_event_idle(D->Q, <+... D->F ...+>)
|
wait_event_killable(D->Q, <+... D->F ...+>)
|
wait_event_timeout(D->Q, <+... D->F ...+>, ...)
|
wait_event_killable_timeout(D->Q, <+... D->F ...+>, ...)
|
wait_event_interruptible(D->Q, <+... D->F ...+>)
|
wait_event_interruptible_timeout(D->Q, <+... D->F ...+>, ...)
)

@changed exists@
identifier waited.F, fn;
type waited.T;
T D;
expression E;
position p;
@@
fn(...) {
<+...
(
atomic_inc(&D->F@p)
|
atomic_long_inc(&D->F@p)
|
atomic64_inc(&D->F@p)
|
atomic_dec(&D->F@p)
|
atomic_long_dec(&D->F@p)
|
atomic64_dec(&D->F@p)
|
atomic_add(E, &D->F@p)
|
atomic_long_add(E, &D->F@p)
|
atomic64_add(E, &D->F@p)
|
atomic_sub(E, &D->F@p)
|
atomic_long_sub(E, &D->F@p)
|
atomic64_sub(E, &D->F@p)
|
WRITE_ONCE(D->F@p, E)
|
D->F@p = E
)
...+>
}

@woken exists@
identifier waited.Q, changed.fn;
type waited.T;
T D;
@@
fn(...) {
...
(
wake_up(&D->Q);
|
wake_up_all(&D->Q);
|
wake_up_interruptible(&D->Q);
|
wake_up_interruptible_all(&D->Q);
)
...
}

@depends on !woken@
identifier waited.F, changed.fn;
expression D;
position changed.p;
@@
fn(...) {
<+...
* D->F@p
...+>
}
