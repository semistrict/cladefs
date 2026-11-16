// A call to a function that asserts a lock held (lockdep_assert_held), on a path of the caller's
// that neither takes that lock nor asserts it held: the lock a callee needs, left out. Lockdep finds
// it only on a debug kernel, and only on a path a test runs; as a region looked up without the
// regions lock (TASK-44.9), a writeback's lock taken out (TASK-164.4), or a semaphore taken shared
// where a callee needs it exclusive (lockdep_assert_held_write), as a writeback swapped its dirty
// set out with iomaps dirtying more (TASK-164.4). The lock is known by its field and the type of
// what holds it. A function whose comment says which lock it wants asserts it, so that this checks
// its callers.
// Mutants: task-44.9-regions-find-unlocked task-164.4-writeback-unlocked task-164.4-swap-shared
// Mutants: task-164.4-rebind-shared

@asserts@
identifier f, L;
type T;
T E;
@@
f(...) {
...
(
lockdep_assert_held(&E->L);
|
lockdep_assert_held_read(&E->L);
)
...
}

@exists@
identifier asserts.f, asserts.L, fn != asserts.f;
type asserts.T;
T E;
expression list es;
@@
fn(...) {
... when != mutex_lock(&E->L)
    when != mutex_lock_killable(&E->L)
    when != mutex_lock_interruptible(&E->L)
    when != mutex_trylock(&E->L)
    when != spin_lock(&E->L)
    when != spin_lock_irq(&E->L)
    when != spin_lock_irqsave(&E->L, ...)
    when != spin_lock_bh(&E->L)
    when != spin_trylock(&E->L)
    when != down_read(&E->L)
    when != down_write(&E->L)
    when != down_read_killable(&E->L)
    when != down_write_killable(&E->L)
    when != down_read_trylock(&E->L)
    when != down_write_trylock(&E->L)
    when != filemap_invalidate_lock(E)
    when != filemap_invalidate_lock_shared(E)
    when != lockdep_assert_held(&E->L)
    when != lockdep_assert_held_write(&E->L)
    when != lockdep_assert_held_read(&E->L)
* f(es)
...
}

// Held for writing: not shared.
@asserts_write@
identifier f, L;
type T;
T E;
@@
f(...) {
...
lockdep_assert_held_write(&E->L);
...
}

@exists@
identifier asserts_write.f, asserts_write.L, fn != asserts_write.f;
type asserts_write.T;
T E;
expression list es;
@@
fn(...) {
... when != mutex_lock(&E->L)
    when != mutex_lock_killable(&E->L)
    when != mutex_lock_interruptible(&E->L)
    when != mutex_trylock(&E->L)
    when != spin_lock(&E->L)
    when != spin_lock_irq(&E->L)
    when != spin_lock_irqsave(&E->L, ...)
    when != spin_lock_bh(&E->L)
    when != spin_trylock(&E->L)
    when != down_write(&E->L)
    when != down_write_killable(&E->L)
    when != down_write_trylock(&E->L)
    when != filemap_invalidate_lock(E)
    when != lockdep_assert_held_write(&E->L)
* f(es)
...
}
