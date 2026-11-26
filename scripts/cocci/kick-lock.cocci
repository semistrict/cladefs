// The budget's worker (struct cladefs_dax's `work`) queued on a path of a function that does not
// hold its kick_lock, under which cladefs_dax_stop says that the worker stops: a kick that read
// `stopping` unlocked queued the round after the stop's cancel_work_sync, and the worker ran on a
// filesystem unmounting or frozen (board 10373). cladefs_dax_kick is the one place that queues it.
// Mutants: board-10373-kick-unlocked

@exists@
identifier fn;
struct cladefs_dax *D;
expression wq;
@@
fn(...) {
... when != mutex_lock(&D->kick_lock)
* queue_work(wq, &D->work)
...
}
