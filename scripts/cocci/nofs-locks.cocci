// A DAX file's writeback lock or semaphore taken or let go of bare, not through vfs.h's
// cladefs_nofs_*, which check that it is held in a NOFS allocation scope: reclaim from the
// filesystem evicts inodes no one holds, and an eviction takes both (cladefs_dax_evict), so that
// an allocation under them that could reclaim from the filesystem, of dax.c's, Rust's or the
// driver's, could wait on them itself. The budget's rebind did, holding a file whose inode it
// didn't hold, in the driver's sg_alloc_table (TASK-559): every caller wrapping its own calls in a
// NOFS scope left that one out.
// Mutants: task-559-rebind-locks-bare

@@
struct cladefs_dax_file *F;
@@
(
* down_read(&F->sem)
|
* down_read_trylock(&F->sem)
|
* down_write(&F->sem)
|
* down_write_trylock(&F->sem)
|
* up_read(&F->sem)
|
* up_write(&F->sem)
|
* downgrade_write(&F->sem)
|
* mutex_lock(&F->wb_lock)
|
* mutex_trylock(&F->wb_lock)
|
* mutex_unlock(&F->wb_lock)
)
