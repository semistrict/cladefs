/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * What the kernel gives the filesystem's Rust (guest/rust/linux-rt and guest/rust/std): memory,
 * logging, panics, a way for a thread to wait for a future, work items, timers, the clocks and
 * randomness. Rust declares the same functions in linux-rt's ffi module and std's sys module.
 */
#ifndef _FS_CLADEFS_RT_H
#define _FS_CLADEFS_RT_H

#include <linux/types.h>

/*
 * Memory for Rust's allocator, which may sleep: without failing (__GFP_NOFAIL), as Rust's code
 * mostly can't, and some writes through what it got unchecked (TASK-60.1); NULL only if `align` is
 * too large.
 */
void *cladefs_rt_alloc(size_t size, size_t align);
void cladefs_rt_free(void *ptr);

/* Logs `len` bytes of `msg` at a printk level (0 to 7, KERN_EMERG to KERN_DEBUG). */
void cladefs_rt_log(int level, const char *msg, size_t len);
/* Panics the kernel, for a Rust panic. */
void __noreturn cladefs_rt_panic(const char *msg, size_t len);

/*
 * A parker lets a thread wait until it is woken: an cladefs_rt_unpark lets cladefs_rt_park return, one
 * at a time, or several at once, as std's thread parking does. The thread spins briefly before it
 * sleeps. Unparking does not sleep, so it may be called from any context. Made without failing.
 */
struct cladefs_rt_parker;
struct cladefs_rt_parker *cladefs_rt_parker_new(void);
void cladefs_rt_parker_free(struct cladefs_rt_parker *parker);
void cladefs_rt_park(struct cladefs_rt_parker *parker);
void cladefs_rt_unpark(struct cladefs_rt_parker *parker);

/* Sets up, and tears down, what the runtime needs: before the filesystem runs, and after. */
int cladefs_rt_init(void);
void cladefs_rt_exit(void);

/*
 * A task's work item, which calls `run(arg)` on a kernel worker each time it is queued, with the
 * `arg` it was queued with: made where the kernel may sleep, without failing, so that queueing it,
 * from any context, allocates nothing and can't fail, as losing a poll would leave its task
 * waiting for ever (TASK-60.1). It is queued once at a time, not again until its run has begun.
 * Urgent work, which tasks wait for, runs at a high priority. It may be freed from any context
 * once it is queued no more, from its own run too.
 */
struct cladefs_rt_work;
struct cladefs_rt_work *cladefs_rt_work_new(void (*run)(void *arg), bool urgent);
void cladefs_rt_work_queue(struct cladefs_rt_work *work, void *arg);
void cladefs_rt_work_free(struct cladefs_rt_work *work);

/*
 * A one-shot timer, which calls `fire(arg)` from a softirq once started and its time has passed.
 * Freeing a timer cancels it, waiting for `fire` if it is running. Made without failing.
 */
struct cladefs_rt_timer;
struct cladefs_rt_timer *cladefs_rt_timer_new(void (*fire)(void *arg), void *arg);
void cladefs_rt_timer_start(struct cladefs_rt_timer *timer, u64 ns);
void cladefs_rt_timer_free(struct cladefs_rt_timer *timer);

/*
 * Lets other tasks run on this CPU, if the scheduler wants it to, as the kernel preempts no task:
 * for Rust's waits that spin.
 */
void cladefs_rt_yield(void);

/*
 * Futexes, for Rust's locks: sleeps while `*futex` holds `expected`, until cladefs_rt_futex_wake wakes
 * it or `timeout_ns` passes (U64_MAX: never). Returns false if the timeout passed.
 */
bool cladefs_rt_futex_wait(const u32 *futex, u32 expected, u64 timeout_ns);
/* Wakes one of the tasks waiting on `futex`, whose value the caller changed. */
void cladefs_rt_futex_wake(const u32 *futex);
/* Wakes all of them. */
void cladefs_rt_futex_wake_all(const u32 *futex);

/* The monotonic clock, in nanoseconds. */
u64 cladefs_rt_monotonic_ns(void);
/* The real-time clock, in nanoseconds since the Unix epoch. */
u64 cladefs_rt_realtime_ns(void);

/* Fills `buf` with `len` random bytes, from the kernel's generator. */
void cladefs_rt_random_bytes(void *buf, size_t len);

/*
 * The current task's slot for Rust's thread-local values (its `cladefs_tls`), or NULL outside a
 * task's context, such as in an interrupt.
 */
void **cladefs_rt_tls(void);
/* Drops the thread-local values in `tls`, as their task exits: Rust's. */
void cladefs_rust_tls_free(void *tls);

#endif
