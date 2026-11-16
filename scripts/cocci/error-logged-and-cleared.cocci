// An error logged and then cleared, so that what failed goes on as if it hadn't: as a failed
// PRIVATE that went on, its pages dirty without being the VM's, warned of and its error set to 0
// (TASK-164.9). A failure is passed on, or handled where it is logged; never a log line and on
// (AGENTS.md: no silent fallbacks).
// Mutants: task-164.9-private-failure-ignored

@@
expression E;
identifier log = {pr_warn, pr_warn_ratelimited, pr_warn_once, pr_err, pr_err_ratelimited,
                  pr_err_once, pr_info, pr_info_ratelimited, pr_notice, WARN, WARN_ONCE};
@@
if (E) {
  ...
  log(...);
  ...
* E = 0;
  ...
}
