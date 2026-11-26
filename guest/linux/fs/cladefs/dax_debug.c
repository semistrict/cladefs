// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The cladefs Authors.
/*
 * cladefs: DAX fault injection, delay injection, pause points, trace ring-buffer, test knobs, and
 * diagnostics/stats reporting, split out of dax.c's page-fault and writeback hot paths.
 */

#include <linux/bitmap.h>
#include <linux/dax.h>
#include <linux/delay.h>
#include <linux/genalloc.h>
#include <linux/hashtable.h>
#include <linux/ktime.h>
#include <linux/mm.h>
#include <linux/moduleparam.h>
#include <linux/prandom.h>
#include <linux/rcupdate.h>
#include <linux/seq_file.h>
#include <linux/vmalloc.h>

#include "dax_internal.h"

/* cladefs: whether a fault injection counting down in `left` injects once more, counting it. */
static bool cladefs_dax_inject(unsigned int *left)
{
	unsigned int now = READ_ONCE(*left);

	while (now) {
		unsigned int was = cmpxchg(left, now, now - 1);

		if (was == now)
			return true;
		now = was;
	}
	return false;
}

/* cladefs: fault injection: inodes fail to attach to their files, which leaves them in the page cache. */
static bool cladefs_dax_fail_attach;
module_param_named(dax_fail_attach, cladefs_dax_fail_attach, bool, 0644);
MODULE_PARM_DESC(dax_fail_attach, "Fail inodes' attaching to their files, which leaves them in the page cache (fault injection)");

/*
 * cladefs: fault injection: of the next this many maps of a range's pages in more than one piece, the
 * range maps the first piece alone, then fails (cladefs_dax_map_all).
 */
static unsigned int cladefs_dax_fail_maps;
module_param_named(dax_fail_maps, cladefs_dax_fail_maps, uint, 0644);
MODULE_PARM_DESC(dax_fail_maps, "Fail ranges' maps of more than one piece after the first, the next this many (fault injection)");

/*
 * cladefs: fault injection of a chunk's map failing part way (TASK-119.13): a MAP_MANY that maps its
 * first member, then fails, as a device's may; and the MAP, one by one, of the second piece on
 * after it.
 */
static unsigned int cladefs_dax_fail_map_many;
module_param_named(dax_fail_map_many, cladefs_dax_fail_map_many, uint, 0644);
MODULE_PARM_DESC(dax_fail_map_many, "Fail the next this many MAP_MANYs after their first member (fault injection)");

static unsigned int cladefs_dax_fail_fallbacks;
module_param_named(dax_fail_fallbacks, cladefs_dax_fail_fallbacks, uint, 0644);
MODULE_PARM_DESC(dax_fail_fallbacks, "Fail the next this many MAPs, after a failed MAP_MANY, of pieces after its first (fault injection)");

/*
 * cladefs: fault injection: simulate an open extent (-EPERM) during the next this many rebinds
 * (cladefs_dax_map_all), leaving private pages counted in `*open` (board 12113).
 */
static unsigned int cladefs_dax_fail_rebind_open;
module_param_named(dax_fail_rebind_open, cladefs_dax_fail_rebind_open, uint, 0644);
MODULE_PARM_DESC(dax_fail_rebind_open, "Treat the next this many rebind MAPs as open extents (-EPERM) (fault injection)");

bool cladefs_dax_should_fail_attach(void)
{
	return READ_ONCE(cladefs_dax_fail_attach);
}

bool cladefs_dax_should_fail_maps(void)
{
	return cladefs_dax_inject(&cladefs_dax_fail_maps);
}

bool cladefs_dax_should_fail_map_many(void)
{
	return cladefs_dax_inject(&cladefs_dax_fail_map_many);
}

bool cladefs_dax_should_fail_fallbacks(void)
{
	return cladefs_dax_inject(&cladefs_dax_fail_fallbacks);
}

bool cladefs_dax_should_fail_rebind_open(void)
{
	return cladefs_dax_inject(&cladefs_dax_fail_rebind_open);
}

/*
 * cladefs: delay injection (TASK-44.11): at the steps of the DAX protocol where a check and its use
 * race what other tasks change, a quarter of the time, a sleep of up to cladefs.dax_delay_us
 * microseconds, from a generator seeded with cladefs.dax_delay_seed, which a run names: a window of
 * microseconds two tasks rarely meet in, as TASK-99's, widened so that they do. Off (0) unless set.
 */
static unsigned int cladefs_dax_delay_us;
module_param_named(dax_delay_us, cladefs_dax_delay_us, uint, 0644);
MODULE_PARM_DESC(dax_delay_us, "Sleep up to this many microseconds at the DAX protocol's racing steps (delay injection)");
static unsigned long cladefs_dax_delay_seed;
module_param_named(dax_delay_seed, cladefs_dax_delay_seed, ulong, 0444);
MODULE_PARM_DESC(dax_delay_seed, "The seed of cladefs.dax_delay_us's delays");

static unsigned long cladefs_dax_pause_steps;
module_param_named(dax_pause, cladefs_dax_pause_steps, ulong, 0644);
MODULE_PARM_DESC(dax_pause, "Steps (bits) of the DAX protocol tasks wait at, until cleared (pause points)");
static unsigned long cladefs_dax_paused_steps;
module_param_named(dax_paused, cladefs_dax_paused_steps, ulong, 0444);
MODULE_PARM_DESC(dax_paused, "Steps (bits) a task waits at (pause points)");
/* cladefs: the file whose tasks wait at the steps of a file, any file's if 0 (pause points). */
static unsigned long cladefs_dax_pause_ino;
module_param_named(dax_pause_ino, cladefs_dax_pause_ino, ulong, 0644);
MODULE_PARM_DESC(dax_pause_ino, "The inode whose tasks wait at cladefs.dax_pause's steps of a file, or 0 for any (pause points)");

/* cladefs: waits at `step`, of file `ino`, while cladefs.dax_pause asks, if cladefs.dax_pause_ino names it. */
void cladefs_dax_pause_file(enum cladefs_dax_step step, u64 ino)
{
	unsigned long only = READ_ONCE(cladefs_dax_pause_ino);

	if (likely(!(READ_ONCE(cladefs_dax_pause_steps) & BIT(step))) || (only && only != ino))
		return;
	set_bit(step, &cladefs_dax_paused_steps);
	while (READ_ONCE(cladefs_dax_pause_steps) & BIT(step))
		msleep(1);
	clear_bit(step, &cladefs_dax_paused_steps);
}

static DEFINE_SPINLOCK(cladefs_dax_delay_lock);
static struct rnd_state cladefs_dax_delay_state;
static bool cladefs_dax_delay_seeded;

/* cladefs: a delay injected here, if cladefs.dax_delay_us asks for them. Where the caller may sleep. */
void cladefs_dax_delay(void)
{
	unsigned int most = READ_ONCE(cladefs_dax_delay_us);
	u32 chance;

	if (!most)
		return;
	spin_lock(&cladefs_dax_delay_lock);
	if (!cladefs_dax_delay_seeded) {
		prandom_seed_state(&cladefs_dax_delay_state, cladefs_dax_delay_seed);
		cladefs_dax_delay_seeded = true;
	}
	chance = prandom_u32_state(&cladefs_dax_delay_state);
	spin_unlock(&cladefs_dax_delay_lock);
	if (chance % 4)
		return;
	fsleep(chance / 4 % most + 1);
}

/*
 * cladefs: diagnostic (TASK-121): with cladefs.dax_trace set as the filesystem mounts, the latest of
 * what happened to files' pages, which writing `ino page` to cladefs.dax_trace_dump logs for that
 * page: where the window maps it, and the iomaps of it.
 */
static const char *const cladefs_dax_trace_kinds[] = {
	"mapped", "write iomap", "fault iomap", "read iomap",
};

struct cladefs_dax_trace_event {
	u64 time, seals, ino, page, window, extent, extent_offset;
	u32 kind, flags;
};

#define CLADEFS_DAX_TRACE_EVENTS (1 << 18)

static bool cladefs_dax_tracing;
module_param_named(dax_trace, cladefs_dax_tracing, bool, 0444);
MODULE_PARM_DESC(dax_trace, "Trace what happens to DAX pages (diagnostic), as the filesystem mounts");
/* Allocated as the first filesystem traced mounts, and kept: never freed under a tracer. */
static struct cladefs_dax_trace_event *cladefs_dax_trace_events;
static atomic64_t cladefs_dax_trace_next;
/* The window's first page frame, which `pfn N` written to cladefs.dax_trace_dump is counted from. */
static unsigned long cladefs_dax_trace_base;

/*
 * Traces an event of `dax`'s filesystem, its seal count its own device's: a tracer has its mount's
 * dax, which outlives it. Before, the device of the filesystem mounted last, kept in a static,
 * which another filesystem's tracer read as that one unmounted, its device freed (board 8555).
 */
void cladefs_dax_trace(struct cladefs_dax *dax, u32 kind, u64 ino, u64 page, u64 window,
		       u64 extent, u64 extent_offset, u32 flags)
{
	struct cladefs_dax_trace_event *events = READ_ONCE(cladefs_dax_trace_events);
	u64 at;

	if (likely(!events))
		return;
	at = atomic64_inc_return(&cladefs_dax_trace_next) - 1;
	events[at % CLADEFS_DAX_TRACE_EVENTS] = (struct cladefs_dax_trace_event) {
		.time = ktime_get_ns(), .seals = cladefs_device_seals(dax->dev), .ino = ino,
		.page = page, .window = window, .extent = extent, .extent_offset = extent_offset,
		.kind = kind, .flags = flags,
	};
}

/* cladefs: traces what `map`, of `range`'s pages, mapped: an extent's pages, or zeros. */
void cladefs_dax_trace_map(struct cladefs_dax *dax, const struct cladefs_dax_range *range,
			   const struct cladefs_device_map *map)
{
	unsigned long page = (map->window_offset - range->window_offset) >> PAGE_SHIFT;
	unsigned long end = page + (map->len >> PAGE_SHIFT), i;
	u64 first = cladefs_dax_chunk_start(range) >> PAGE_SHIFT;

	for (i = page; i < end; i++)
		cladefs_dax_trace(dax, CLADEFS_DAX_TRACE_MAPPED, range->file->ino, first + i,
				  cladefs_dax_page_offset(range, i), map->zeros ? 0 : map->sequence,
				  map->zeros ? 0 : map->offset + ((u64)(i - page) << PAGE_SHIFT),
				  map->zeros);
}

/* Logs the traced events of `ino`'s page `page`, oldest first: `ino page` written. */
static int cladefs_dax_trace_dump(const char *val, const struct kernel_param *kp)
{
	struct cladefs_dax_trace_event *events = READ_ONCE(cladefs_dax_trace_events), *e;
	u64 ino = 0, page = 0, pfn, window = 0, next, at, first;
	unsigned int shown = 0;
	bool by_window;

	if (!events)
		return -EINVAL;
	/* `pfn N`: the events of the window's page at page frame N, whichever page they were of. */
	by_window = sscanf(val, "pfn %llx", &pfn) == 1;
	if (by_window) {
		if (!cladefs_dax_trace_base || pfn < cladefs_dax_trace_base)
			return -EINVAL;
		window = (pfn - cladefs_dax_trace_base) << PAGE_SHIFT;
	} else if (sscanf(val, "%llu %llu", &ino, &page) != 2) {
		return -EINVAL;
	}
	next = atomic64_read(&cladefs_dax_trace_next);
	first = next > CLADEFS_DAX_TRACE_EVENTS ? next - CLADEFS_DAX_TRACE_EVENTS : 0;
	if (by_window)
		pr_err("cladefs: the trace of window page %llx, of %llu events:\n", window, next - first);
	else
		pr_err("cladefs: the trace of ino %llu page %llu, of %llu events:\n", ino, page,
		       next - first);
	for (at = first; at < next; at++) {
		e = &events[at % CLADEFS_DAX_TRACE_EVENTS];
		if (by_window ? e->window != window : e->ino != ino || e->page != page)
			continue;
		pr_err("cladefs:   %llu ns seals %llu: ino %llu page %llu: %s window %llx extent %llu at %llx flags %x\n",
		       e->time, e->seals, e->ino, e->page, cladefs_dax_trace_kinds[e->kind], e->window,
		       e->extent, e->extent_offset, e->flags);
		if (++shown == 200)
			break;
	}
	return 0;
}

static const struct kernel_param_ops cladefs_dax_trace_dump_ops = { .set = cladefs_dax_trace_dump };
module_param_cb(dax_trace_dump, &cladefs_dax_trace_dump_ops, NULL, 0200);
MODULE_PARM_DESC(dax_trace_dump, "Log the traced events of `ino page`, or `pfn N` (hex), the window's page at page frame N (diagnostic)");

/*
 * The guest's memory of the window's page at `window_offset`. cladefs: with dax_read_lock held, as
 * dax_direct_access needs, for as long as the caller uses the memory; a debug kernel's lockdep
 * found faults taking it without (TASK-44.8).
 */
static void *cladefs_dax_window_page(struct cladefs_dax *dax, u64 window_offset)
{
	void *kaddr;

	if (dax_direct_access(dax->dax_dev, window_offset >> PAGE_SHIFT, 1, DAX_ACCESS, &kaddr,
			      NULL) < 1)
		return NULL;
	return kaddr;
}

/*
 * cladefs: the filesystem mounted last, for the tests' knobs (cladefs.dax_stray, dax_rebind_now,
 * dax_rebind_every), under cladefs_dax_last_lock: published once its dax is whole, at the end of
 * cladefs_dax_create, and taken back first thing in cladefs_dax_destroy, which so waits for a knob
 * using it. Before, a static read with READ_ONCE: a knob written as the filesystem unmounted used
 * its dax freed, and one written as it mounted used a dax not yet whole, or freed as the mount
 * failed (board 8555).
 */
static DEFINE_MUTEX(cladefs_dax_last_lock);
static struct cladefs_dax *cladefs_dax_last;

/*
 * The filesystem mounted last, held until cladefs_dax_last_put, or NULL with none. The knob's work
 * races its unmount: a delay may come (cladefs_dax_delay), which a test's unmounts meet.
 */
static struct cladefs_dax *cladefs_dax_last_get(void)
{
	mutex_lock(&cladefs_dax_last_lock);
	if (!cladefs_dax_last) {
		mutex_unlock(&cladefs_dax_last_lock);
		return NULL;
	}
	cladefs_dax_delay();
	return cladefs_dax_last;
}

static void cladefs_dax_last_put(void)
{
	mutex_unlock(&cladefs_dax_last_lock);
}

/*
 * cladefs: `dax`, whole, is the filesystem mounted last, for the tests' knobs (cladefs_dax_last),
 * and the trace's, if asked for (TASK-121): its window's first page frame, and the events, made
 * once, under the lock as two mounts may make them at once.
 */
void cladefs_dax_publish(struct cladefs_dax *dax)
{
	mutex_lock(&cladefs_dax_last_lock);
	cladefs_dax_last = dax;
	if (cladefs_dax_tracing) {
		void *kaddr;
		int id = dax_read_lock();

		if (dax_direct_access(dax->dax_dev, 0, 1, DAX_ACCESS, &kaddr,
				      &cladefs_dax_trace_base) < 1)
			cladefs_dax_trace_base = 0;
		dax_read_unlock(id);
		if (!cladefs_dax_trace_events)
			WRITE_ONCE(cladefs_dax_trace_events,
				   vzalloc(sizeof(*cladefs_dax_trace_events) * CLADEFS_DAX_TRACE_EVENTS));
	}
	mutex_unlock(&cladefs_dax_last_lock);
}

/* cladefs: `dax` is no longer the knobs', once the knob using it, if one does, returns. */
void cladefs_dax_unpublish(struct cladefs_dax *dax)
{
	mutex_lock(&cladefs_dax_last_lock);
	if (cladefs_dax_last == dax)
		cladefs_dax_last = NULL;
	mutex_unlock(&cladefs_dax_last_lock);
}

/*
 * cladefs: what a guest that keeps to no protocol does, for tests (TASK-77): `load N` or `store N`,
 * written to cladefs.dax_stray, reads or stores a word at byte N of the window, whatever maps it.
 */
static int cladefs_dax_stray_set(const char *value, const struct kernel_param *kp)
{
	struct cladefs_dax *dax;
	char op[6];
	u64 offset;
	u64 *at;
	int id, err = -EINVAL;

	if (sscanf(value, "%5s %llu", op, &offset) != 2)
		return -EINVAL;
	dax = cladefs_dax_last_get();
	if (!dax)
		return -EINVAL;
	if (offset >= dax->window_len)
		goto put;
	id = dax_read_lock();
	at = cladefs_dax_window_page(dax, offset);
	if (at) {
		at = (void *)at + offset_in_page(offset);
		if (!strcmp(op, "load")) {
			pr_info("cladefs: a stray load at %llu of the window: %llx\n", offset, READ_ONCE(*at));
			err = 0;
		} else if (!strcmp(op, "store")) {
			WRITE_ONCE(*at, 0x5a5a5a5a5a5a5a5aULL);
			err = 0;
		}
	}
	dax_read_unlock(id);
put:
	cladefs_dax_last_put();
	return err;
}

static const struct kernel_param_ops cladefs_dax_stray_ops = { .set = cladefs_dax_stray_set };
module_param_cb(dax_stray, &cladefs_dax_stray_ops, NULL, 0200);
MODULE_PARM_DESC(dax_stray, "`load N` or `store N`: access byte N of the window, whatever maps it");

/*
 * cladefs: for tests, as freezing does (cladefs_dax_frozen): writing to cladefs.dax_rebind_now asks the
 * budget's worker of the filesystem mounted last to write back, seal and rebind every private page
 * it can, and returns, the round queued. A check waits for it by the filesystem's stats: once
 * dax_rebinds_done reaches dax_rebinds_asked as read after the write, a round begun after the ask
 * has ended. Waited for here, the setter held kernel_param_lock (param_attr_store) through a
 * rebind a pause point held, and every later read of a parameter waited for it (GCE).
 */
static int cladefs_dax_rebind_now_set(const char *val, const struct kernel_param *kp)
{
	struct cladefs_dax *dax = cladefs_dax_last_get();

	if (!dax)
		return -EINVAL;
	atomic64_inc(&dax->rebinds_asked);
	cladefs_dax_kick(dax, true);
	cladefs_dax_last_put();
	return 0;
}

static const struct kernel_param_ops cladefs_dax_rebind_now_ops = { .set = cladefs_dax_rebind_now_set };
module_param_cb(dax_rebind_now, &cladefs_dax_rebind_now_ops, NULL, 0200);
MODULE_PARM_DESC(dax_rebind_now, "Have the worker write back, seal and rebind every private page, on the filesystem mounted last, not waited for: dax_rebinds_done reaches dax_rebinds_asked (tests)");

/*
 * cladefs: for tests (doc-13's stage 2): every this many milliseconds, the budget's worker writes
 * back, seals and rebinds every private page it can, under whatever stores go on; 0 never. Setting
 * it starts it, on the filesystem mounted last.
 */
static unsigned int cladefs_dax_rebind_every;

unsigned int cladefs_dax_rebind_every_ms(void)
{
	return READ_ONCE(cladefs_dax_rebind_every);
}

/* cladefs: cladefs.dax_rebind_every, set, starts the worker's rounds on the filesystem mounted last. */
static int cladefs_dax_rebind_every_set(const char *val, const struct kernel_param *kp)
{
	struct cladefs_dax *dax;
	int err = param_set_uint(val, kp);

	/* Set with no filesystem mounted, it starts the rounds of the next to mount. */
	if (err || !READ_ONCE(cladefs_dax_rebind_every))
		return err;
	dax = cladefs_dax_last_get();
	if (dax) {
		cladefs_dax_kick(dax, false);
		cladefs_dax_last_put();
	}
	return 0;
}

static const struct kernel_param_ops cladefs_dax_rebind_every_ops = {
	.set = cladefs_dax_rebind_every_set,
	.get = param_get_uint,
};
module_param_cb(dax_rebind_every, &cladefs_dax_rebind_every_ops, &cladefs_dax_rebind_every, 0644);
MODULE_PARM_DESC(dax_rebind_every, "Write back, seal and rebind every private page every this many ms (tests; 0: never)");

/*
 * cladefs: fault injection (TASK-164.2): the next this many of Fxfs's write_many transactions fail
 * once their blocks are written, before they commit, as a crash between the two would leave them
 * (cladefs_set_fail_commits).
 */
static unsigned long cladefs_dax_fail_commits;

static int cladefs_dax_fail_commits_set(const char *val, const struct kernel_param *kp)
{
	int err = param_set_ulong(val, kp);

	if (!err)
		cladefs_set_fail_commits(cladefs_dax_fail_commits);
	return err;
}

static const struct kernel_param_ops cladefs_dax_fail_commits_ops = {
	.set = cladefs_dax_fail_commits_set,
	.get = param_get_ulong,
};
module_param_cb(dax_fail_commits, &cladefs_dax_fail_commits_ops, &cladefs_dax_fail_commits, 0644);
MODULE_PARM_DESC(dax_fail_commits, "Fail the next this many write_many transactions after their bytes, before they commit (fault injection)");

static const char *const cladefs_dax_whys[CLADEFS_DAX_WHYS] = {
	"reach", "range",
};

/*
 * cladefs: where write_ns went (TASK-146.2), each part apart, so that they add up: the copy, which is
 * dax_iomap_rw less its iomaps', and what none of them holds, which a new step in the write would
 * show as.
 */
static void cladefs_dax_show_write_times(struct seq_file *m, struct cladefs_dax *dax)
{
	s64 begin = atomic64_read(&dax->write_begin_ns), end = atomic64_read(&dax->write_end_ns);
	s64 rw = atomic64_read(&dax->write_rw_ns), setup = atomic64_read(&dax->write_setup_ns);
	s64 fault_in = atomic64_read(&dax->write_fault_in_ns);
	s64 retry = atomic64_read(&dax->write_retry_ns), tail = atomic64_read(&dax->write_tail_ns);
	s64 lock = atomic64_read(&dax->write_lock_ns);
	s64 private = atomic64_read(&dax->write_private_ns);

	seq_printf(m, "dax_writes %lld\ndax_writes_ns %lld\ndax_write_private_ns %lld\n",
		   atomic64_read(&dax->writes), atomic64_read(&dax->write_ns), private);
	seq_printf(m, "dax_write_lock_ns %lld\n", lock);
	seq_printf(m, "dax_write_setup_ns %lld\ndax_write_fault_in_ns %lld\n", setup, fault_in);
	/* Read apart, as writes go on: a difference may come out below zero by a little. */
	seq_printf(m, "dax_write_begin_ns %lld\ndax_write_copy_ns %lld\ndax_write_end_ns %lld\n",
		   begin, max_t(s64, 0, rw - begin - end), end);
	seq_printf(m, "dax_write_retry_ns %lld\ndax_write_tail_ns %lld\n", retry, tail);
	seq_printf(m, "dax_write_other_ns %lld\n",
		   max_t(s64, 0, atomic64_read(&dax->write_ns) - private - lock - setup - fault_in -
			 rw - retry - tail));
}

/*
 * cladefs: the longest run of `pool`'s free space, in bytes: of a window free in pieces, the most a
 * range or a region can take (TASK-91.10). Read as it changes.
 */
static size_t cladefs_dax_largest_free(struct gen_pool *pool)
{
	unsigned long nbits, start, end, largest = 0;
	struct gen_pool_chunk *chunk;

	rcu_read_lock();
	list_for_each_entry_rcu(chunk, &pool->chunks, next_chunk) {
		nbits = (chunk->end_addr - chunk->start_addr + 1) >> pool->min_alloc_order;
		for (start = find_first_zero_bit(chunk->bits, nbits); start < nbits;
		     start = find_next_zero_bit(chunk->bits, nbits, end)) {
			end = find_next_bit(chunk->bits, nbits, start);
			largest = max(largest, end - start);
		}
	}
	rcu_read_unlock();
	return (size_t)largest << pool->min_alloc_order;
}

/* cladefs: the window's regions' space, the references held to them, and the slices that hold them. */
static void cladefs_dax_show_regions(struct cladefs_dax *dax, struct seq_file *m)
{
	unsigned long pages = 0, refs = 0, slices = 0;
	struct cladefs_dax_region *region;
	struct cladefs_dax_volume *volume;
	struct cladefs_dax_range *range;
	struct cladefs_dax_file *file;
	unsigned long ino, chunk;
	unsigned int bucket;

	spin_lock(&dax->regions_lock);
	hash_for_each(dax->regions, bucket, region, node) {
		pages += region->pages;
		refs += region->refs;
	}
	spin_unlock(&dax->regions_lock);
	mutex_lock(&dax->files_lock);
	cladefs_dax_for_each_file(dax, volume, ino, file) {
		/*
		 * Ranges freed meanwhile are freed after a grace period (cladefs_dax_free_range), but not
		 * their regions: only the ranges are read.
		 */
		rcu_read_lock();
		xa_for_each(&file->ranges, chunk, range)
			slices += !!READ_ONCE(range->region);
		rcu_read_unlock();
	}
	mutex_unlock(&dax->files_lock);
	seq_printf(m, "window_regions_bytes %lu\nwindow_regions_refs %lu\nwindow_slices %lu\n",
		   pages << PAGE_SHIFT, refs, slices);
}

void cladefs_dax_show_stats(struct cladefs_dax *dax, struct seq_file *m)
{
	bool apart = dax->region_pool != dax->pool;
	int why;

	seq_printf(m, "window_free_bytes %zu\n", gen_pool_avail(dax->pool) +
		   (apart ? gen_pool_avail(dax->region_pool) : 0));
	seq_printf(m, "window_largest_free_bytes %zu\n", cladefs_dax_largest_free(dax->pool));
	if (apart)
		seq_printf(m, "window_regions_free_bytes %zu\nwindow_regions_largest_free_bytes %zu\n",
			   gen_pool_avail(dax->region_pool),
			   cladefs_dax_largest_free(dax->region_pool));
	seq_printf(m, "window_full %ld\nwindow_regions_full %ld\nwindow_reclaims %ld\n",
		   atomic_long_read(&dax->window_full), atomic_long_read(&dax->regions_full),
		   atomic_long_read(&dax->reclaims));
	seq_printf(m, "window_regions %ld\n", atomic_long_read(&dax->region_count));
	seq_printf(m, "window_unaligned %ld\n", atomic_long_read(&dax->unaligned));
	cladefs_dax_show_regions(dax, m);
	seq_printf(m, "window_lost_bytes %lld\n", atomic64_read(&dax->lost_bytes));
	seq_printf(m, "dax_huge_maps %lld\n", atomic64_read(&dax->huge_maps));
	seq_printf(m, "dax_extent_alloc_bytes %lld\n", atomic64_read(&dax->extent_alloc_bytes));
	/* cladefs: the private pages, writeback and rebinding (doc-13, TASK-164.2). */
	seq_printf(m, "dax_private_max %ld\n", dax->private_max);
	seq_printf(m, "dax_private_pages %ld\ndax_dirty_pages %ld\n",
		   atomic_long_read(&dax->private), atomic_long_read(&dax->dirty));
	seq_printf(m, "dax_writebacks %lld\ndax_written_pages %lld\n",
		   atomic64_read(&dax->writebacks), atomic64_read(&dax->written));
	seq_printf(m, "dax_rebound_pages %lld\ndax_throttled %lld\n",
		   atomic64_read(&dax->rebound), atomic64_read(&dax->throttled));
	seq_printf(m, "dax_rebinds_asked %lld\ndax_rebinds_done %lld\n",
		   atomic64_read(&dax->rebinds_asked), atomic64_read(&dax->rebinds_done));
	seq_printf(m, "dax_seals %lld\ndax_privates %lld\ndax_open_maps %lld\n",
		   atomic64_read(&dax->seals_made), atomic64_read(&dax->privates),
		   atomic64_read(&dax->open_maps));
	seq_printf(m, "dax_private_ahead_pages %lld\ndax_private_before_pages %lld\n",
		   atomic64_read(&dax->private_ahead), atomic64_read(&dax->private_before));
	seq_printf(m, "dax_unsealed_inodes %ld\n", atomic_long_read(&dax->unsealed));
	cladefs_dax_show_write_times(m, dax);
	cladefs_hist_show(m, "dax_write", &dax->write_hist);
	cladefs_hist_show(m, "dax_faults", &dax->fault_hist);
	for (why = 0; why < CLADEFS_DAX_WHYS; why++)
		seq_printf(m, "window_maps_%s %lld\nwindow_map_bytes_%s %lld\n", cladefs_dax_whys[why],
			   atomic64_read(&dax->maps[why]), cladefs_dax_whys[why],
			   atomic64_read(&dax->map_bytes[why]));
}
