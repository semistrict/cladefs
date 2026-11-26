# Keeping pyo3-async-runtimes safe at exit with an exit flag checked in our futures

**Tried:** 2026-11-14, board 13101 (the guard), board 13173, board 13191 (where it was given up).
**Idea:** pyo3-async-runtimes completes asyncio Futures from tokio threads, which crashes once
the interpreter finalizes. An `atexit` hook setting a flag, checked by each future before it
returns its result (holding it forever once set), should keep every completion out of a
finalizing interpreter without replacing the library.
**Result:** a segfault at exit in about 1 pytest run in 20 (`test_no_call_completes_once_python_exits`),
and in 10 of 20 runs of a script cancelling 64 sandbox calls just before `asyncio.run` returned.
In the core dump, a tokio blocking thread was in `pyo3_async_runtimes::generic::cancelled` ->
`PyObject_GetAttr`. Two holes:
- When Python cancels, the library drops our future, guard and all, and then enters Python from
  a tokio thread itself, to ask the Future whether it was cancelled.
- A flag checked before `Python::attach` is not atomic with it. A thread can check, lose the
  CPU, and attach after `atexit`. A guest-free stress test (`tests/test_awaitables.py`) also
  showed pyo3's "The Python interpreter is not initialized" panics.

It was the third bug from that bridge, after board 13101 and board 13173 (a cancelled read did not stop
its task).
**Why it's dead:** the library enters Python from foreign threads on paths a caller cannot wrap,
and no flag check closes the window between checking and attaching. Replaced by libmachine's own
bridge in `crates/libmachine-py/src/lib.rs`:
- `awaitable()` makes an asyncio Future and runs the future as a tokio task.
- Cancellation is a done callback that aborts the task, so nothing calls back into Python.
- Completion goes through `with_python()`, a gate the `atexit` hook closes. The hook then waits,
  without the GIL, for every thread inside to leave.

**Reopen if:** pyo3 gains native async methods that can drive tokio futures (the
`experimental-async` feature made stable, with a story for interpreter finalization). That would
be worth comparing against our own bridge.
