// A call to Fxfs that changes a directory's entries on a path that drops no listing of a directory
// (cladefs_vfs_dir_changes): readdir then goes on serving the entries it listed before, as it did
// after a create, an unlink, a rename or an exchange whose drop had gone (TASK-230).
// Mutants: task-230-create-keeps-listing task-230-unlink-keeps-listing task-230-rename-keeps-listing
// Mutants: task-230-exchange-keeps-listing

@exists@
identifier fn;
identifier change = {cladefs_create, cladefs_symlink, cladefs_link, cladefs_unlink, cladefs_unlink_later,
                     cladefs_rename, cladefs_exchange};
expression list es;
@@
fn(...) {
... when != cladefs_vfs_dir_changes(...)
* change(es)
... when != cladefs_vfs_dir_changes(...)
}
