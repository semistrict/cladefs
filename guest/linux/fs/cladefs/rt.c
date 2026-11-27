// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The cladefs Authors.
/*
 * The kernel's side of the filesystem's Rust runtime (see rt.h).
 */

#include <linux/completion.h>
#include <linux/hrtimer.h>
#include <linux/kernel.h>
#include <linux/ktime.h>
#include <linux/log2.h>
#include <linux/mm.h>
#include <linux/printk.h>
#include <linux/random.h>
#include <linux/sched.h>
#include <linux/sched/clock.h>
#include <linux/sched/mm.h>
#include <linux/slab.h>
#include <linux/wait_bit.h>
#include <linux/workqueue.h>

#include "rt.h"

void *cladefs_rt_alloc(size_t size, size_t align)
{
	if (align > PAGE_SIZE)
		return NULL;
	/*
	 * kmalloc aligns sizes that are powers of two to themselves, and vmalloc, which kvmalloc
	 * falls back to, aligns to pages.
	 */
	if (align > ARCH_KMALLOC_MINALIGN)
		size = roundup_pow_of_two(max(size, align));
	return kvmalloc(size, GFP_KERNEL | __GFP_NOFAIL);
}

void cladefs_rt_free(void *ptr)
{
	kvfree(ptr);
}

void cladefs_rt_log(int level, const char *msg, size_t len)
{
	static const char *const levels[] = {
		KERN_EMERG, KERN_ALERT, KERN_CRIT, KERN_ERR,
		KERN_WARNING, KERN_NOTICE, KERN_INFO, KERN_DEBUG,
	};

	printk("%scladefs: %.*s\n", levels[clamp(level, 0, 7)], (int)len, msg);
}

void cladefs_rt_panic(const char *msg, size_t len)
{
	panic("cladefs: %.*s\n", (int)len, msg);
}

/* A parker's states: nothing to take, an unpark to take, or a thread sleeping until there is. */
enum { PARKER_EMPTY, PARKER_NOTIFIED, PARKER_PARKED };

/*
 * How long a parker spins for an unpark before it sleeps: the waits the filesystem's futures
 * park for are mostly as short as a lock held across a commit, and waking a thread sleeping on
 * another CPU takes an interprocessor interrupt, which in a virtual machine exits to the host.
 */
#define PARKER_SPIN_NS (20 * NSEC_PER_USEC)

struct cladefs_rt_parker {
	atomic_t state;
	struct completion unparked;
};

struct cladefs_rt_parker *cladefs_rt_parker_new(void)
{
	struct cladefs_rt_parker *parker = kmalloc(sizeof(*parker), GFP_KERNEL | __GFP_NOFAIL);

	atomic_set(&parker->state, PARKER_EMPTY);
	init_completion(&parker->unparked);
	return parker;
}

void cladefs_rt_parker_free(struct cladefs_rt_parker *parker)
{
	kfree(parker);
}

/*
 * How many times a parker spins between looks at the clock, which a virtual machine's paravirtual
 * clock makes costly to read.
 */
#define PARKER_SPINS_PER_CLOCK 64

void cladefs_rt_park(struct cladefs_rt_parker *parker)
{
	u64 deadline = local_clock() + PARKER_SPIN_NS;
	unsigned int spins = 0;

	/* An unpark already, or one while spinning, unless another task wants the CPU. */
	do {
		if (atomic_read(&parker->state) == PARKER_NOTIFIED &&
		    atomic_cmpxchg(&parker->state, PARKER_NOTIFIED, PARKER_EMPTY) == PARKER_NOTIFIED)
			return;
		cpu_relax();
	} while (!need_resched() &&
		 (++spins % PARKER_SPINS_PER_CLOCK != 0 || local_clock() < deadline));
	if (atomic_cmpxchg(&parker->state, PARKER_EMPTY, PARKER_PARKED) == PARKER_NOTIFIED) {
		/* Taken with a barrier, which a failed exchange is not. */
		atomic_xchg(&parker->state, PARKER_EMPTY);
		return;
	}
	wait_for_completion(&parker->unparked);
	/*
	 * An unpark since the one that woke this thread is taken too: the caller looks again at
	 * what it waits for, after this barrier, which orders it after that unpark.
	 */
	atomic_xchg(&parker->state, PARKER_EMPTY);
}

void cladefs_rt_unpark(struct cladefs_rt_parker *parker)
{
	/* A thread spinning takes the unpark itself: only a sleeping one is woken. */
	if (atomic_xchg(&parker->state, PARKER_NOTIFIED) == PARKER_PARKED)
		complete(&parker->unparked);
}

struct cladefs_rt_work {
	struct work_struct work;
	struct workqueue_struct *wq;
	void (*run)(void *arg);
	void *arg;
};

static void cladefs_rt_work_run(struct work_struct *work)
{
	struct cladefs_rt_work *w = container_of(work, struct cladefs_rt_work, work);
	/* What it runs may free it, its task's last reference dropped: nothing reads it after. */
	void (*run)(void *arg) = w->run;
	void *arg = w->arg;
	/* Rust's allocations must not recurse into filesystems to reclaim memory. */
	unsigned int nofs = memalloc_nofs_save();

	run(arg);
	memalloc_nofs_restore(nofs);
}

/*
 * Where Rust's work runs: with a rescuer, so that work freeing memory runs when memory is short.
 * Urgent work, which the tasks writing wait for, at a high priority, as the work of a filesystem's
 * kernel threads (jbd2) takes the CPU from the tasks that write: unlinks applied in the background
 * otherwise get a third of the CPU four unlinkers spinning on a directory's lock leave. Not the
 * rest: compacting at a high priority took the CPU from the writes.
 */
static struct workqueue_struct *cladefs_rt_wq;
static struct workqueue_struct *cladefs_rt_urgent_wq;

int cladefs_rt_init(void)
{
	cladefs_rt_wq = alloc_workqueue("cladefs", WQ_UNBOUND | WQ_MEM_RECLAIM, 0);
	cladefs_rt_urgent_wq = alloc_workqueue("cladefs_urgent", WQ_UNBOUND | WQ_HIGHPRI | WQ_MEM_RECLAIM,
					    0);
	if (cladefs_rt_wq && cladefs_rt_urgent_wq)
		return 0;
	cladefs_rt_exit();
	return -ENOMEM;
}

void cladefs_rt_exit(void)
{
	if (cladefs_rt_urgent_wq)
		destroy_workqueue(cladefs_rt_urgent_wq);
	if (cladefs_rt_wq)
		destroy_workqueue(cladefs_rt_wq);
}

struct cladefs_rt_work *cladefs_rt_work_new(void (*run)(void *arg), bool urgent)
{
	struct cladefs_rt_work *w = kmalloc(sizeof(*w), GFP_KERNEL | __GFP_NOFAIL);

	INIT_WORK(&w->work, cladefs_rt_work_run);
	w->wq = urgent ? cladefs_rt_urgent_wq : cladefs_rt_wq;
	w->run = run;
	w->arg = NULL;
	return w;
}

void cladefs_rt_work_queue(struct cladefs_rt_work *w, void *arg)
{
	/* Its run, if one has begun, has read its argument already. */
	w->arg = arg;
	WARN_ON_ONCE(!queue_work(w->wq, &w->work));
}

void cladefs_rt_work_free(struct cladefs_rt_work *w)
{
	kfree(w);
}

struct cladefs_rt_timer {
	struct hrtimer timer;
	void (*fire)(void *arg);
	void *arg;
};

static enum hrtimer_restart cladefs_rt_timer_fire(struct hrtimer *timer)
{
	struct cladefs_rt_timer *t = container_of(timer, struct cladefs_rt_timer, timer);

	t->fire(t->arg);
	return HRTIMER_NORESTART;
}

struct cladefs_rt_timer *cladefs_rt_timer_new(void (*fire)(void *arg), void *arg)
{
	struct cladefs_rt_timer *t = kmalloc(sizeof(*t), GFP_KERNEL | __GFP_NOFAIL);

	hrtimer_setup(&t->timer, cladefs_rt_timer_fire, CLOCK_MONOTONIC, HRTIMER_MODE_REL_SOFT);
	t->fire = fire;
	t->arg = arg;
	return t;
}

void cladefs_rt_timer_start(struct cladefs_rt_timer *timer, u64 ns)
{
	hrtimer_start(&timer->timer, ns_to_ktime(ns), HRTIMER_MODE_REL_SOFT);
}

void cladefs_rt_timer_free(struct cladefs_rt_timer *timer)
{
	hrtimer_cancel(&timer->timer);
	kfree(timer);
}

void cladefs_rt_yield(void)
{
	if (in_task())
		cond_resched();
	else
		cpu_relax();
}

/*
 * Waits on `var` as wait_var_event and wait_var_event_timeout do, but exclusively: wake_up_var
 * wakes one such waiter, not all, as a futex's wake does.
 */
#define cladefs_rt_wait_var_exclusive(var, condition)					\
	___wait_var_event(var, condition, TASK_UNINTERRUPTIBLE, 1, 0, schedule())
#define cladefs_rt_wait_var_exclusive_timeout(var, condition, timeout)			\
({											\
	long __ret = timeout;								\
	if (!___wait_cond_timeout(condition))						\
		__ret = ___wait_var_event(var, ___wait_cond_timeout(condition),	\
					  TASK_UNINTERRUPTIBLE, 1, timeout,		\
					  __ret = schedule_timeout(__ret));		\
	__ret;										\
})

bool cladefs_rt_futex_wait(const u32 *futex, u32 expected, u64 timeout_ns)
{
	void *var = (void *)futex;

	might_sleep();
	if (timeout_ns == U64_MAX) {
		if (READ_ONCE(*futex) == expected)
			cladefs_rt_wait_var_exclusive(var, READ_ONCE(*futex) != expected);
		return true;
	}
	return cladefs_rt_wait_var_exclusive_timeout(var, READ_ONCE(*futex) != expected,
						  nsecs_to_jiffies(timeout_ns)) != 0;
}

void cladefs_rt_futex_wake(const u32 *futex)
{
	/* Orders the caller's change to the futex before the check for waiters. */
	smp_mb();
	wake_up_var((void *)futex);
}

void cladefs_rt_futex_wake_all(const u32 *futex)
{
	struct wait_bit_key key = __WAIT_BIT_KEY_INITIALIZER((void *)futex, -1);
	struct wait_queue_head *wq_head = __var_waitqueue((void *)futex);

	/* As cladefs_rt_futex_wake, but for every waiter. */
	smp_mb();
	if (waitqueue_active(wq_head))
		__wake_up(wq_head, TASK_NORMAL, 0, &key);
}

u64 cladefs_rt_monotonic_ns(void)
{
	return ktime_get_ns();
}

u64 cladefs_rt_realtime_ns(void)
{
	struct timespec64 now;

	/*
	 * cladefs: the time the kernel stamps an inode with (current_time()): the coarse clock, or
	 * the multigrain floor if that is later, which a fine-grained ctime given another inode moves
	 * on (FS_MGTIME). A finer clock put a time after a ctime the kernel set later; the coarse
	 * clock alone fell behind a ctime set before: a symlink made after a file had an mtime a tick
	 * before the file's (xfstests generic/423 both times, the second on an arm64 guest,
	 * 2026-11-27).
	 */
	ktime_get_coarse_real_ts64_mg(&now);
	return timespec64_to_ns(&now);
}

void cladefs_rt_random_bytes(void *buf, size_t len)
{
	get_random_bytes(buf, len);
}

u64 cladefs_rt_current_task(void)
{
	if (!in_task())
		return 0;
	return (u64)(unsigned long)current;
}
