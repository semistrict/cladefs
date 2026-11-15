// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The fxdax Authors.
/*
 * specs/dax-cow's world, played by dax.c, the seed's interleaving: one file of a few pages, two
 * storers storing to them through one mapping and by write(2), loading and reading them too, a
 * task fsyncing it and asking the budget's worker to rebind, and the worker, which few private
 * pages keep busy writing back and rebinding (TASK-444.1). With -o, its steps are traced as the
 * spec's actions (kernel/spec_trace.c), for specs/dax-cow/trace/TraceDaxCow.tla to follow.
 *
 *   traced SEED [OPERATIONS] [-o TRACE] [-v]
 *
 * As DaxCow's Init has it, the traced part begins with each page a sealed extent's, mapped in the
 * window, none private nor dirty, no PTE: written a page at a time from the last (so that the
 * file is no one piece of an extent, which a read would map as a slice), synced, and rebound. The
 * file model (kernel/shadow.c) checks every read, load and fsync, and the end its bytes in Fxfs.
 */

#include <sim/sim.h>
#include <sim/spec_trace.h>

#define WINDOW_LEN (64UL << 20)
#define STORERS 2
#define MOST_PAGES 4

#define check(cond, ...) do { if (!(cond)) sim_panic(__VA_ARGS__); } while (0)

static struct sim_mount *mount;
static struct file *file;
static unsigned long addr, pages, operations = 60;
static int storers_left;
static unsigned long stores;
static bool verbose;

static u64 between(u64 low, u64 high)
{
	return low + sim_random() % (high - low + 1);
}

/* What the budget's worker serves as it writes back, seals and rebinds all it can, waited for. */
static void rebind_now(void)
{
	u64 asked;

	check(!sim_param_set("dax_rebind_now", "1"), "rebinding now");
	asked = sim_dax_stat(mount, "dax_rebinds_asked");
	sim_await(sim_dax_stat(mount, "dax_rebinds_done") >= asked, "no rebind came");
}

static void setup_task(void *arg)
{
	char page[PAGE_SIZE];
	long at;

	for (at = pages - 1; at >= 0; at--) {
		memset(page, 'a' + at, sizeof(page));
		check(sim_write(file, at << PAGE_SHIFT, page, PAGE_SIZE) == PAGE_SIZE, "writing page %ld",
		      at);
		check(!sim_fsync(file), "syncing page %ld", at);
	}
	rebind_now();
	check(!sim_dax_stat(mount, "dax_private_pages"), "pages private after the rebind: %llu",
	      sim_dax_stat(mount, "dax_private_pages"));
}

/* A store of a few bytes within one page, through the mapping or by write(2). */
static void store(int id, unsigned long page, bool fault)
{
	char bytes[64];
	size_t len = between(1, sizeof(bytes)), i;
	loff_t at = (page << PAGE_SHIFT) + between(0, PAGE_SIZE - len);
	ssize_t done;

	for (i = 0; i < len; i++)
		bytes[i] = sim_random();
	if (verbose)
		sim_log("traced: storer %d stores %zu bytes at %lld by %s\n", id, len, at,
			fault ? "its mapping" : "write(2)");
	stores++;
	sim_spec_store_begins(page, fault);
	if (fault)
		done = sim_user_store(addr + at, bytes, len) ?: (ssize_t)len;
	else
		done = sim_write(file, at, bytes, len);
	check(done == len, "a store of storer %d at %lld: %zd", id, at, done);
}

static void storer_task(void *arg)
{
	int id = (uintptr_t)arg;
	unsigned long i, page;
	char bytes[64];

	for (i = 0; i < operations; i++) {
		page = between(0, pages - 1);
		switch (between(0, 5)) {
		case 0 ... 1:
			store(id, page, true);
			break;
		case 2 ... 3:
			store(id, page, false);
			break;
		case 4:
			check(!sim_user_load(addr + (page << PAGE_SHIFT), bytes, sizeof(bytes)),
			      "a load of page %lu", page);
			break;
		default:
			check(sim_read(file, page << PAGE_SHIFT, bytes, sizeof(bytes)) == sizeof(bytes),
			      "a read of page %lu", page);
			break;
		}
	}
	storers_left--;
}

/* Fsyncs, and asks the budget's worker to rebind, as the storers store. */
static void fsyncer_task(void *arg)
{
	while (READ_ONCE(storers_left)) {
		if (between(0, 3)) {
			check(!sim_fsync(file), "an fsync");
		} else {
			check(!sim_param_set("dax_rebind_now", "1"), "rebinding now");
		}
		msleep(between(0, 3));
	}
}

int main(int argc, char **argv)
{
	unsigned long long seed;
	const char *path = NULL;
	static const char *const names[STORERS] = { "S0", "S1" };
	unsigned long private_max;
	int i;

	if (argc < 2 || sscanf(argv[1], "%llu", &seed) != 1)
		sim_panic("usage: traced SEED [OPERATIONS] [-o TRACE] [-v]");
	if (argc > 2 && argv[2][0] != '-')
		sscanf(argv[2], "%lu", &operations);
	for (i = 2; i < argc; i++) {
		if (!strcmp(argv[i], "-o") && i + 1 < argc)
			path = argv[++i];
		else if (!strcmp(argv[i], "-v"))
			verbose = true;
	}
	sim_init(seed);
	sim_device_init(WINDOW_LEN);
	pages = between(2, MOST_PAGES);
	/* Few, so that the storers wait for the budget, and the worker writes back and rebinds. */
	private_max = between(2, 2 * MOST_PAGES);
	sim_device_private_max(private_max);
	mount = sim_mount_new();
	file = sim_file_new(mount);
	sim_spawn("setup", setup_task, NULL);
	sim_run();
	addr = sim_mmap(file, 0, pages << PAGE_SHIFT);
	if (path)
		sim_spec_trace_begin(file, path, seed, pages, private_max);
	storers_left = STORERS;
	for (i = 0; i < STORERS; i++) {
		sim_spec_actor(sim_spawn(names[i], storer_task, (void *)(uintptr_t)i), names[i]);
	}
	sim_spec_actor(sim_spawn("F", fsyncer_task, NULL), "F");
	sim_run();
	sim_spec_trace_end(stores);
	check(!sim_fsync(file), "the last fsync");
	sim_munmap(addr);
	sim_end(SIM_END_READ_FILES);
	sim_log("traced: seed %llu passed: %lu pages, private_max %lu, %lu stores, trace %llx\n", seed,
		pages, private_max, stores, sim_trace_hash());
	return 0;
}
