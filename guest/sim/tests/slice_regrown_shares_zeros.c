// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The cladefs Authors.
/*
 * board 2508 (TASK-556): a slice of a shared region shrunk to a page-aligned size and grown again
 * must never MAP zeros over its region's shared window pages when read.
 *
 * Two files share a sealed 4-page extent through a reflink, so both read it as slices of one
 * region (one MAP). File `a` is shrunk to 2 pages (page-aligned, so `resize_end` clears pages 2..4
 * in `a`'s range state without unmapping the region's window pages) and grown back to 4 pages.
 * Reading `a`'s regrown pages 2..4 (by read(2) and by a read fault) must return zeros without
 * mapping over the region's window: `b` still reads its own bytes through read(2), through a read
 * fault, and through a fresh slice opened after dropping its ranges.
 */

#include <sim/sim.h>

/* What it expects, void once a sweep's call has failed (kernel/sweep.c). */
#define check sim_expect

#define PAGES 4

/* Drops `file`'s ranges, whatever they map. */
static void drop_ranges(struct file *file)
{
	struct inode *inode = file_inode(file);
	int err;

	filemap_invalidate_lock(inode->i_mapping);
	err = cladefs_vfs_inode(inode)->data->punch_drop(inode, 0, MAX_LFS_FILESIZE);
	filemap_invalidate_unlock(inode->i_mapping);
	check(!err, "dropping ino %lu's ranges: %d", inode->i_ino, err);
}

static void check_bytes(const char *buf, size_t len, char expected, const char *what)
{
	size_t i;

	for (i = 0; i < len; i++) {
		if (buf[i] != expected) {
			check(false, "%s: byte %zu is %#x, expected %#x", what, i,
			      (unsigned char)buf[i], (unsigned char)expected);
			return;
		}
	}
}

static void regrown_slice_keeps_sibling_intact(struct sim_mount *mount, bool via_write)
{
	static char data[PAGES * PAGE_SIZE], got[PAGES * PAGE_SIZE];
	struct file *a, *b, *c;
	unsigned long addr_a, addr_b;
	u64 slices;
	ssize_t done;
	int err;

	a = sim_file_new(mount);
	memset(data, 'a', sizeof(data));
	done = sim_write(a, 0, data, sizeof(data));
	check(done == sizeof(data), "writing file a: %zd", done);
	check(!cladefs_dax_write_back(file_inode(a)), "writing back file a");
	check(!sim_fsync(a), "fsync file a");

	b = sim_file_new(mount);
	err = sim_clone(a, 0, b, 0, sizeof(data));
	check(!err, "cloning a to b: %d", err);

	c = sim_file_new(mount);
	err = sim_clone(a, 0, c, 0, sizeof(data));
	check(!err, "cloning a to c: %d", err);

	/* Reopen as read-only DAX files with no prior ranges so reads create shared slices. */
	a = sim_file_read_in(mount, a);
	b = sim_file_read_in(mount, b);
	c = sim_file_read_in(mount, c);
	drop_ranges(a);
	drop_ranges(b);
	drop_ranges(c);

	slices = sim_dax_stat(mount, "window_slices");
	done = sim_read(a, 0, got, sizeof(got));
	check(done == sizeof(got), "initial read of a: %zd", done);
	check_bytes(got, sizeof(got), 'a', "initial read of a");

	done = sim_read(b, 0, got, sizeof(got));
	check(done == sizeof(got), "initial read of b: %zd", done);
	check_bytes(got, sizeof(got), 'a', "initial read of b");
	check(sim_dax_stat(mount, "window_slices") - slices == 2,
	      "expected 2 slices for a and b, got %llu",
	      sim_dax_stat(mount, "window_slices") - slices);

	addr_a = sim_mmap(a, 0, sizeof(data));
	addr_b = sim_mmap(b, 0, sizeof(data));
	err = sim_user_load(addr_b + 2 * PAGE_SIZE, got, PAGE_SIZE);
	check(!err, "initial fault load of b page 2: %d", err);
	check_bytes(got, PAGE_SIZE, 'a', "initial fault load of b page 2");

	/* Shrink a to a page-aligned size (2 pages), leaving pages 2..4 unmapped in a's slice. */
	err = sim_truncate(a, 2 * PAGE_SIZE);
	check(!err, "shrinking a to 2 pages: %d", err);

	if (via_write) {
		/* Grow a back by writing page 3, leaving page 2 as a regrown hole. */
		static char tail[PAGE_SIZE];

		memset(tail, 'z', sizeof(tail));
		done = sim_write(a, 3 * PAGE_SIZE, tail, sizeof(tail));
		check(done == sizeof(tail), "writing a page 3: %zd", done);
	} else {
		/* Grow a back via truncate, leaving pages 2..4 as regrown holes. */
		err = sim_truncate(a, sizeof(data));
		check(!err, "growing a back to 4 pages: %d", err);
	}

	/* Reading a's regrown page 2 (read(2) and read fault) must return zeros. */
	done = sim_read(a, 2 * PAGE_SIZE, got, PAGE_SIZE);
	check(done == PAGE_SIZE, "read(2) of a's regrown page 2: %zd", done);
	check_bytes(got, PAGE_SIZE, 0, "read(2) of a's regrown page 2");

	err = sim_user_load(addr_a + 2 * PAGE_SIZE, got, PAGE_SIZE);
	check(!err, "fault load of a's regrown page 2: %d", err);
	check_bytes(got, PAGE_SIZE, 0, "fault load of a's regrown page 2");

	/* Sibling b must still read 'a' through read(2) and through a read fault. */
	done = sim_read(b, 0, got, sizeof(got));
	check(done == sizeof(got), "read(2) of b after a regrew: %zd", done);
	check_bytes(got, sizeof(got), 'a', "read(2) of b after a regrew");

	err = sim_user_load(addr_b + 2 * PAGE_SIZE, got, PAGE_SIZE);
	check(!err, "fault load of b page 2 after a regrew: %d", err);
	check_bytes(got, PAGE_SIZE, 'a', "fault load of b page 2 after a regrew");

	err = sim_user_load(addr_b + 3 * PAGE_SIZE, got, PAGE_SIZE);
	check(!err, "fault load of b page 3 after a regrew: %d", err);
	check_bytes(got, PAGE_SIZE, 'a', "fault load of b page 3 after a regrew");

	/* A brand-new slice of the same region (file c) must also still read 'a'. */
	done = sim_read(c, 0, got, sizeof(got));
	check(done == sizeof(got), "read(2) of fresh slice c: %zd", done);
	check_bytes(got, sizeof(got), 'a', "read(2) of fresh slice c");

	sim_file_evict(c);
	sim_file_evict(b);
	sim_file_evict(a);
}

static void scenario(void *arg)
{
	struct sim_mount *mount = sim_mount_new();

	regrown_slice_keeps_sibling_intact(mount, false);
	regrown_slice_keeps_sibling_intact(mount, true);
	sim_mount_free(mount);
}

int main(void)
{
	sim_init(1);
	sim_device_init(32UL << 20);
	sim_spawn("scenario", scenario, NULL);
	sim_run();
	sim_end(SIM_END_READ_FILES);
	sim_log("slice_regrown_shares_zeros: passed\n");
	return 0;
}
