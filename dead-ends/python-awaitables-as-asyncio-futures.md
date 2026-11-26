# Returning pyo3-async-runtimes' asyncio Futures straight from Python methods

**Tried:** 2026-11-14, board 13101, branch `task-11-sandbox` (before its first commit).
**Idea:** `pyo3_async_runtimes::tokio::future_into_py` returns an `asyncio.Future` that
completes when the Rust future does; returning it from `AsyncSandbox`'s methods is the
library's documented pattern, with no Python code of libmachine's own.
**Result:** the first check failed in two ways. `asyncio.create_task(sb.exec(...))`
raised `TypeError: a coroutine was expected, got <Future pending ...>` (task groups refuse
Futures too), and the sandboxes those failures left behind made the process segfault in 1
of 3 runs at exit. A Future still pending when the interpreter finalized completed from a
tokio thread (its sandbox, collected at exit, failed the call) and called into the
finalized interpreter: `PyErr_Display` from
`future_into_py_with_locals<..., AsyncProcess::wait>` while the main thread ran C exit
handlers (core dump from apport, `target/check.log` of that run).
**Why it's dead:** a bare Future is not what Python's structured concurrency takes, and
nothing stops it from completing after finalization. Every awaitable now goes through
one helper (`awaitable()` in `crates/libmachine-py/src/lib.rs`). It wraps the Future in a
coroutine (`async def _await(f): return await f`) and holds every completion back once
the `atexit` hook has run. That guard later proved leaky, and the library was replaced by a
bridge of libmachine's own (board 13191, dead-ends/pyo3-async-runtimes-guarded-by-an-exit-flag.md).
**Reopen if:** pyo3-async-runtimes returns coroutines itself and refuses to complete
after finalization, or pyo3's native `async fn` methods (the `experimental-async`
feature) become stable and can drive tokio futures.
