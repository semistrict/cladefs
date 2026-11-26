// The stops of the budget's worker (struct cladefs_dax's `stops`) set to a value, not counted: one
// flag, a start undid every stop, not its own, and a volume's leave started the worker while another
// volume's eviction was under way, its stop taken (board 12453). cladefs_dax_stop and
// cladefs_dax_start count, each start balancing one stop.
// Mutants: board-12453-stops-one-flag

@@
struct cladefs_dax *D;
constant C;
@@
(
* D->stops = C
|
* WRITE_ONCE(D->stops, C)
)
