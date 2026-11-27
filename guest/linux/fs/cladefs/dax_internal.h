/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * What dax.c and dax_debug.c share: the window's structs and helpers, and the fault injection,
 * delay injection, pause points, trace ring-buffer, and test knobs living in dax_debug.c.
 */
#ifndef _FS_CLADEFS_DAX_INTERNAL_H
#define _FS_CLADEFS_DAX_INTERNAL_H

#include <linux/atomic.h>
#include <linux/dax.h>
#include <linux/genalloc.h>
#include <linux/hashtable.h>
#include <linux/list.h>
#include <linux/mutex.h>
#include <linux/rcupdate.h>
#include <linux/refcount.h>
#include <linux/rwsem.h>
#include <linux/spinlock.h>
#include <linux/types.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <linux/xarray.h>

#include "device.h"
#include "hist.h"
#include "pages.h"
#include "vfs.h"

/* A range's size: a power of two, the window's and chunks' alignment. */
#define CLADEFS_DAX_SHIFT 21
#define CLADEFS_DAX_SIZE (1ULL << CLADEFS_DAX_SHIFT)
#define CLADEFS_DAX_PAGES (CLADEFS_DAX_SIZE >> PAGE_SHIFT)

/*
 * cladefs: pause points (TASK-119.9): a task reaching a step whose bit cladefs.dax_pause sets waits
 * there until the bit is cleared, the bit set in cladefs.dax_paused meanwhile: a check plays an
 * interleaving out exactly, where cladefs.dax_delay_us's random sleeps only make it likely. The bits
 * of the steps writable DAX had (0 to 3, and 6) are unused, so that the others keep theirs.
 */
enum cladefs_dax_step {
	/* A read of a file, its iomap made, before the copy of what it maps, holding its range. */
	CLADEFS_DAX_STEP_READ_MAPPED = 4,
	/*
	 * A write(2) of a file, its iomap made, its pages dirty, before the copy into what it maps,
	 * holding the file's semaphore shared (cladefs_dax_iomap_write).
	 */
	CLADEFS_DAX_STEP_WRITE_MAPPED = 5,
	/*
	 * A rebind about to MAP a run of a file's clean private pages over, holding the file's
	 * writeback lock and its semaphore for writing (cladefs_dax_rebind).
	 */
	CLADEFS_DAX_STEP_REBIND = 7,
	/*
	 * A writeback, its dirty set swapped out, before it reads the set's pages from the window,
	 * holding their ranges and the file's writeback lock (cladefs_dax_write_back).
	 */
	CLADEFS_DAX_STEP_WRITEBACK = 8,
	/*
	 * A write fault past its iomap_end, its PTE writable and the file's semaphore let go, holding
	 * its page's DAX entry until it returns (cladefs_dax_protect).
	 */
	CLADEFS_DAX_STEP_FAULTED = 9,
	/*
	 * A writeback, its file's dirty ranges counted, its batch allocated for them, before it holds
	 * the semaphore for writing to swap them out, holding the writeback lock alone
	 * (cladefs_dax_write_back).
	 */
	CLADEFS_DAX_STEP_COUNTED = 10,
	/*
	 * A kick of the budget's worker that found it not stopped, before it takes the lock it
	 * queues the worker's round under (cladefs_dax_kick, board 10373): of no file, waiting unless
	 * cladefs.dax_pause_ino names one. Not to be paused in a parameter's setter
	 * (cladefs.dax_rebind_now, dax_rebind_every kick): it holds the module's parameter lock, which
	 * clearing cladefs.dax_pause takes.
	 */
	CLADEFS_DAX_STEP_KICK = 11,
};

/*
 * cladefs: diagnostic (TASK-121): kinds of events in cladefs.dax_trace's ring buffer.
 */
enum cladefs_dax_trace_kind {
	CLADEFS_DAX_TRACE_MAPPED,
	CLADEFS_DAX_TRACE_WRITE,
	CLADEFS_DAX_TRACE_FAULT,
	CLADEFS_DAX_TRACE_READ,
};

/* The window's mapping of a whole sealed extent, from its start, which slices of files share. */
struct cladefs_dax_region {
	/* In the filesystem's regions, by extent. */
	struct hlist_node node;
	u64 cell;
	u64 sequence;
	u64 window_offset;
	/* Its space in the window, in pages: as far as the extent reaches. */
	unsigned long pages;
	/* Its slices. Guarded by the regions lock. */
	unsigned int refs;
	/* Guards `mapped`, and the region's MAPs. */
	struct mutex lock;
	/* How many of its pages are mapped, from its start. */
	unsigned long mapped;
};

/* A mounted volume's share of the window: the files of its inode numbers, which are its own. */
struct cladefs_dax_volume {
	struct cladefs *fs;
	/* The files with ranges, or inodes using the window, by inode number. */
	struct xarray files;
	/* On the filesystem's volumes. Guarded by its files lock. */
	struct list_head list;
	/*
	 * cladefs: the page-cache inodes whose writeback wrote since the last seal began, by inode
	 * number, each the seal count at which a barrier will have sealed it (cladefs_dax_written,
	 * cladefs_dax_seal_target): read in before then, an inode is a page-cache inode again, as the
	 * window would find its extents open, which it maps none of (doc-13, TASK-164.3). The worker
	 * prunes it as seals come.
	 */
	struct xarray unsealed;
};

/* A file's ranges of the window, which outlive its inodes. */
struct cladefs_dax_file {
	/* Its volume, until the volume goes (cladefs_dax_remove_volume). Guarded by the files lock. */
	struct cladefs_dax_volume *volume;
	u64 ino;
	/* The filesystem's (`files`), and others' while they use it. */
	refcount_t refs;
	/* The inode read in for the file, if there is one. Guarded by the filesystem's files lock. */
	struct inode *inode;
	/* Guards `ranges`, and their pages' state. */
	struct rw_semaphore sem;
	/* The window's ranges mapping chunks of the file, by chunk. */
	struct xarray ranges;
	/*
	 * cladefs: a write whose chunk a reader made a slice again, after the write had it dropped, under
	 * the inode's write lock: its chunk gets a range of its own, so that no slice can refuse it
	 * again (xfstests generic/638).
	 */
	bool no_slices;
	/*
	 * cladefs: how far a write past the end grows the file, under the inode's write lock, which
	 * the ranges mapped meanwhile are to reach: a fault on the write's own source, a mapping of
	 * the file, mapped the chunk only as large as the file, again each time the write dropped it
	 * for being too small, losing the write's end (xfstests generic/638, TASK-147).
	 */
	loff_t growing;
	/*
	 * cladefs: held by the file's writeback, from before it swaps the dirty set out until Fxfs has
	 * committed the pages it wrote, which rebinding and truncation wait for: a writeback's pages,
	 * its writeback set, are those it holds it for. Taken before the semaphore (doc-13).
	 */
	struct mutex wb_lock;
	/* Its ranges' pages in its dirty set, and counted private (cladefs_dax_count). */
	atomic_long_t dirty_pages, private_pages;
	/*
	 * cladefs: when its pages were last made dirty or private, in jiffies, which the budget rebinds
	 * the files of, longest ago first (cladefs_dax_relieve, TASK-164.10).
	 */
	unsigned long dirtied;
	/*
	 * cladefs: the last write(2)'s bytes, and how many pages past its end the next, following on
	 * from it, makes private ahead of it (cladefs_dax_iomap_write): under the inode's write lock,
	 * which write(2) holds (TASK-164.10).
	 */
	loff_t write_pos, write_end;
	unsigned int ahead;
};

/* A range of the window. */
struct cladefs_dax_range {
	/* On the filesystem's used list. */
	struct list_head list;
	/* The file whose chunk it maps, and which. */
	struct cladefs_dax_file *file;
	pgoff_t chunk;
	u64 window_offset;
	/* The region it is a slice of, whose pages it maps, if it is one. */
	struct cladefs_dax_region *region;
	/* Its size, in pages: its chunk's, up to the file's end when it was mapped. */
	unsigned long pages;
	/* The iomaps and writebacks using it, which keep it. */
	atomic_t users;
	/*
	 * cladefs: its pages' state, under its file's semaphore (pages.h): which map something, an
	 * extent's page or zeros; which the host holds private, from the PRIVATE of the iomap that
	 * dirtied them (cladefs_dax_dirty) to the MAP or UNMAP over them; and which are in its file's
	 * dirty set, until writeback swaps them out (cladefs_dax_write_back).
	 */
	struct cladefs_dax_pages *state;
	/*
	 * cladefs: whether a MAP of its pages failed (cladefs_dax_map_all), which may have mapped some of
	 * them all the same, as a MAP_MANY's requests before the one that failed did: its state says
	 * nothing of those, which its UNMAP takes nonetheless (cladefs_dax_unmap_range). Under its
	 * file's semaphore held for writing.
	 */
	bool unsure;
	/*
	 * cladefs: once a range is written to, `no_pmd` forbids PMD faults and any existing PMD
	 * entry (`has_pmd`) is invalidated in place under invalidate_lock write and file->sem write
	 * so its 2 MiB compound folio splits back to order-0 folios (board 14407). That ensures no
	 * large folio ever mixes private dirty pages with clean shared pages under mprotect,
	 * mremap, fork, or arm64 contpte.
	 */
	bool no_pmd;
	bool has_pmd;
	/* Freed after a grace period: the stats walk files' ranges without their semaphores. */
	struct rcu_head rcu;
};

/*
 * cladefs: why the window maps: a region as far as a slice needs, or pages in a range's own space
 * (TASK-91.4), both for the access at hand, which the device prefaults (MAP_PREFAULT, TASK-112.1).
 */
enum cladefs_dax_why {
	CLADEFS_DAX_REACH,
	CLADEFS_DAX_RANGE,
	CLADEFS_DAX_WHYS,
};

struct cladefs_dax {
	struct cladefs_filesystem *filesystem;
	struct cladefs_device *dev;
	struct dax_device *dax_dev;
	/* The window's free space for ranges of their own; regions' too, unless they have theirs. */
	struct gen_pool *pool;
	/*
	 * cladefs: the window's free space for regions: its last part, from `regions_at`, apart from the
	 * ranges' (TASK-91.10), or `pool`, in a window too small to split.
	 */
	struct gen_pool *region_pool;
	u64 regions_at;
	/* cladefs: the window's size, which no page's place is past (cladefs_dax_page_offset). */
	u64 window_len;
	/*
	 * cladefs: how often the window had no room for a range, or for a region in their own space,
	 * how many ranges were reclaimed for room, and how many regions there are (TASK-91.7);
	 * how many chunks' ranges took room off a chunk's alignment, which no PMD maps (TASK-149.33).
	 */
	atomic_long_t window_full, regions_full, reclaims, region_count, unaligned;
	/* cladefs: the window's MAPs, and the bytes they mapped, by why (`enum cladefs_dax_why`). */
	atomic64_t maps[CLADEFS_DAX_WHYS], map_bytes[CLADEFS_DAX_WHYS];
	/* cladefs: the window's bytes kept from use as their UNMAP failed (TASK-44.22). */
	atomic64_t lost_bytes;
	/* cladefs: the faults whose iomap mapped a PMD's 2 MiB, its pages in a row (TASK-123). */
	atomic64_t huge_maps;
	/* Requested bytes of extent-result buffers, before allocator rounding. */
	atomic64_t extent_alloc_bytes;
	/*
	 * cladefs: where write(2)'s time goes (TASK-146.2): in all, waiting for the inode's lock, what
	 * comes before its first iomap, faulting in its source, dax_iomap_rw (of which its iomaps'
	 * begin and end, the rest being the copy, a store's faults on the window among it), dropping
	 * a slice to try again, and what comes after.
	 */
	atomic64_t writes, write_ns, write_lock_ns, write_setup_ns, write_fault_in_ns, write_rw_ns;
	/* cladefs: a write's time making its pages private before its lock (TASK-164.11). */
	atomic64_t write_private_ns;
	atomic64_t write_begin_ns, write_end_ns, write_retry_ns, write_tail_ns;
	struct cladefs_hist write_hist;
	/* cladefs: what each fault of a mapping took (TASK-111.6). */
	struct cladefs_hist fault_hist;
	/* The volumes mounted, each with its files. */
	struct list_head volumes;
	/* Guards `volumes`, their files, and files' inodes. */
	struct mutex files_lock;
	/* Guards the used list. */
	spinlock_t lock;
	/* The ranges mapping chunks, least recently used first. */
	struct list_head used;
	/* Woken as ranges are freed, or their last users done, which counts in `released`. */
	wait_queue_head_t wait;
	atomic_long_t released;
	/* The regions mapping extents, by extent. */
	DECLARE_HASHTABLE(regions, 12);
	/* Guards `regions`, and regions' references. */
	spinlock_t regions_lock;
	/* Where the budget's worker runs. */
	struct workqueue_struct *wq;
	/*
	 * cladefs: the most pages the device holds private at once, which the guest keeps under, and the
	 * pages counted private, and in files' dirty sets (cladefs_dax_count).
	 */
	long private_max;
	atomic_long_t private, dirty;
	/* cladefs: the most pages one write's iomap makes private: a slice of the budget. */
	unsigned long iomap_pages;
	/*
	 * cladefs: the budget's worker (cladefs_dax_work): writes back dirty files, and rebinds clean
	 * private pages, as the private pages near private_max, the window runs out of room, or a
	 * test asks; its rounds, which those it throttles wait for (`budget_wait`); whether it is to
	 * write back whatever is dirty (`clean`), as asked since its last round; the asks to rebind
	 * everything it can (cladefs.dax_rebind_now), and those its rounds have served, which a check
	 * waits for; and how many stop it, a volume's inodes evicted or the filesystem frozen, each
	 * until its start (cladefs_dax_stop, board 12453), changed under `kick_lock`, which a kick
	 * reads it and queues a round under (cladefs_dax_kick, board 10373).
	 */
	struct work_struct work;
	wait_queue_head_t budget_wait;
	atomic64_t rounds;
	atomic_t clean;
	atomic64_t rebinds_asked, rebinds_done;
	struct mutex kick_lock;
	unsigned int stops;
	/* cladefs: the page-cache inodes in the volumes' `unsealed`, which the worker prunes. */
	atomic_long_t unsealed;
	/*
	 * cladefs: the writebacks, the pages they wrote, the pages rebound, the iomaps the budget held
	 * up, the seals made for a range or the budget, and the PRIVATE requests of the iomaps that
	 * dirtied pages (TASK-164.2, TASK-164.9).
	 */
	atomic64_t writebacks, written, rebound, throttled, seals_made, privates;
	/*
	 * cladefs: the pages PRIVATEs made private ahead of writes (TASK-164.10), and before writes
	 * took their inodes' locks (TASK-164.11).
	 */
	atomic64_t private_ahead, private_before;
	/*
	 * cladefs: MAPs the device refused as of an open extent (-EPERM), which Fxfs had taken for
	 * sealed: none is meant to be asked for (doc-13's invariant 1), a tripwire the checks read.
	 */
	atomic64_t open_maps;
};

/* Each of `dax`'s files, of each volume, with its files lock held. */
#define cladefs_dax_for_each_file(dax, volume, ino, file)			\
	list_for_each_entry(volume, &(dax)->volumes, list)		\
		xa_for_each(&(volume)->files, ino, file)

static inline loff_t cladefs_dax_chunk_start(const struct cladefs_dax_range *range)
{
	return (loff_t)range->chunk << CLADEFS_DAX_SHIFT;
}

/* Where `range`'s page `page` is in the window. */
static inline u64 cladefs_dax_page_offset(const struct cladefs_dax_range *range, unsigned long page)
{
	return range->window_offset + ((u64)page << PAGE_SHIFT);
}

/* dax.c: kicks the budget's worker (called by dax_debug.c's test knobs). */
void cladefs_dax_kick(struct cladefs_dax *dax, bool clean);

/* dax_debug.c: fault injection, delay injection, pause points, trace ring-buffer, and test knobs. */
bool cladefs_dax_should_fail_attach(void);
bool cladefs_dax_should_fail_maps(void);
bool cladefs_dax_should_fail_map_many(void);
bool cladefs_dax_should_fail_fallbacks(void);
bool cladefs_dax_should_fail_rebind_open(void);
void cladefs_dax_pause_file(enum cladefs_dax_step step, u64 ino);
void cladefs_dax_delay(void);
void cladefs_dax_trace(struct cladefs_dax *dax, u32 kind, u64 ino, u64 page, u64 window,
		       u64 extent, u64 extent_offset, u32 flags);
void cladefs_dax_trace_map(struct cladefs_dax *dax, const struct cladefs_dax_range *range,
			   const struct cladefs_device_map *map);
void cladefs_dax_publish(struct cladefs_dax *dax);
void cladefs_dax_unpublish(struct cladefs_dax *dax);
unsigned int cladefs_dax_rebind_every_ms(void);

#endif
