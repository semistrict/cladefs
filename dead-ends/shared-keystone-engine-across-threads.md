# Assembling test guests on one Keystone engine from several threads

**Tried:** 2026-11-14, the Python tests' move from raw bytes to Keystone (commit 0134187).
**Idea:** one module-level `Ks` instance in `tests/guest.py`, used by every test, including the
test that boots eight machines from eight Python threads.
**Result:** the threaded test crashed the interpreter once and, on another run, one machine ran a
corrupted program and printed nothing.
**Why it's dead:** Keystone is not thread-safe, and ctypes releases the GIL while it runs, so
threads assemble on the same engine at once. `guest.py` now serializes calls with a lock (20 of 20
threaded runs clean after).
**Reopen if:** Keystone becomes thread-safe, or the tests move to an assembler without shared
state (e.g. llvm-mc in a subprocess).
