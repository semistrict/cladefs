// A field of a DAX region read or written on a path of a function that holds neither the lock
// guarding it nor asserts it held: `mapped`, which the region's own lock guards, as
// cladefs_dax_region_map MAPped and moved it with that lock gone, two readers of slices of one
// extent each saying how far it was mapped (TASK-44.19); `refs`, which the regions lock guards.
// READ_ONCE and WRITE_ONCE mark an access meant to need no lock. scripts/cocci/region-lock.cocci
// looks for `mapped` read under the regions lock instead of its own.
// Mutants: task-44.19-map-to-unlocked

@once@
struct cladefs_dax_region *R;
position q;
@@
(
READ_ONCE(R->mapped@q)
|
WRITE_ONCE(R->mapped@q, ...)
)

@exists@
identifier fn;
struct cladefs_dax_region *R;
position p != once.q;
@@
fn(...) {
... when != mutex_lock(&R->lock)
    when != lockdep_assert_held(&R->lock)
* R->mapped@p
...
}

@exists@
identifier fn;
struct cladefs_dax_region *R;
struct cladefs_dax *D;
@@
fn(...) {
... when != spin_lock(&D->regions_lock)
    when != lockdep_assert_held(&D->regions_lock)
* R->refs
...
}
