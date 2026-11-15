// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The fxdax Authors.
/*
 * A run as specs/dax-cow's actions (TASK-444.1): trace validation, as CCF's and MongoDB's, of
 * dax.c against DaxCow. One record a step of the spec's, in the order the run took them, each of
 * what the code did at a point where it does what the step does, with what it saw:
 *
 * - a storer (`S...`): its store beginning (the test says, sim_spec_store_begins), its fault
 *   finding no writable PTE, the iomap that dirties its page (pages.c's dirty transition, with the
 *   file's semaphore held shared: the spec's budget, lock and dirtying, which the code does in
 *   that order, taking the semaphore first, with nothing of the spec's between), pages made private
 *   ahead of a write, its PTE made writable, the iomap's end (the semaphore let go of; a write(2)'s
 *   copy, which lands between, as of then), its fault's return, and its store through the PTE;
 * - a task doing the rest (`F...`), and dax.c's budget worker (`B`): an fsync's beginning and end
 *   (mount.c's), and a writeback's or a rebind's every step: the writeback lock, the swap (pages.c's
 *   swap transition, with the lock it holds and the pages it took), each page write-protected,
 *   Fxfs's copy, write and commit (fxfs_write_many, the copy as of the write), a rebind's look at
 *   Fxfs's extents (what it will rebind of them), each page it rebinds, and the locks let go of;
 * - anyone: a seal (the device's COMMIT), and a PTE the kernel cleaned as it cleans DAX entries.
 *
 * What the spec has no step for (a page unmapped, cut or filled, a writeback's pages dirtied
 * again) is a record of its own, which no step matches: a divergence, as TLC finds it.
 *
 * Kept in memory, and written as a module (TraceDaxCowData) once the run's traced part ends: the
 * trace, the file's pages, the device's private_max, the actors, and the run's seed and sources
 * (SIM_TRACE_SOURCES), which scripts/check-tla.sh compares with the tree's. One file, of one
 * chunk, one range: its pages' places in the range are its pages, numbered from 1 in the spec.
 */

#include <sim/sim.h>
#include <sim/host.h>
#include <sim/mm.h>
#include <sim/spec_trace.h>

#include "../../linux/fs/fxfs/fxfs.h"
#include "../../linux/fs/fxfs/pages.h"

#define MAX_ACTORS 8

struct actor {
	struct task_struct *task;
	const char *name;
	/*
	 * A storer's: the store it is in, whether its iomap has dirtied its page, and whether its
	 * fault is a store's.
	 */
	unsigned long page;
	bool fault, begun, fault_write;
	/*
	 * A storer's store faulted, its iomap not yet come: a store landing then is one whose fault
	 * found the PTE writable, another's fault having made it so.
	 */
	bool faulted;
	/* A task doing the rest: in an fsync, or in a rebind, whose look it has made this hold. */
	bool fsync, rebind, looked;
	/* What its last look at Fxfs's extents, a rebind's, found it would rebind. */
	char look[256];
};

static struct {
	bool on;
	const char *path;
	struct address_space *mapping;
	u64 ino;
	unsigned long pages, private_max;
	/* The file's range's pages' state: the one range of its one chunk. */
	struct fxfs_dax_pages *state;
	const void *sem, *wb_lock;
	struct actor actors[MAX_ACTORS];
	int nr_actors;
	/* The budget worker's runs, one at a time, as one actor. */
	struct actor worker;
	/* The task in Fxfs's writeback of the traced file, whose WRITE the device takes. */
	struct task_struct *writer;
	u64 seed;
	char *text;
	size_t len, cap;
	u64 records;
} trace;

/* The pages' states the run has, which tracing begins with one of. */
#define MAX_STATES 16
static struct fxfs_dax_pages *states[MAX_STATES];

static void append(const char *format, ...)
{
	char line[512];
	va_list args;
	int len;

	va_start(args, format);
	len = vsnprintf(line, sizeof(line), format, args);
	va_end(args);
	if (len >= (int)sizeof(line))
		sim_panic("spec trace: a record too long");
	if (trace.len + len + 1 > trace.cap) {
		trace.cap = max(2 * trace.cap, (size_t)1 << 16);
		trace.text = host_realloc(trace.text, trace.cap);
	}
	memcpy(trace.text + trace.len, line, len + 1);
	trace.len += len;
}

/* The actor of the task running, or NULL if it is none of the run's. */
static struct actor *actor(void)
{
	int i;

	for (i = 0; i < trace.nr_actors; i++) {
		if (trace.actors[i].task == current)
			return &trace.actors[i];
	}
	if (current->comm && !strcmp(current->comm, "fxfs_dax"))
		return &trace.worker;
	return NULL;
}

static bool storer(const struct actor *a)
{
	return a && a->name[0] == 'S';
}

/* A record, of the actor running, or `env` if it is none of the run's: `fields` after its name. */
static void record(const char *action, const char *format, ...)
{
	struct actor *a = actor();
	char fields[300] = "";
	va_list args;

	if (format) {
		va_start(args, format);
		vsnprintf(fields, sizeof(fields), format, args);
		va_end(args);
	}
	append("%s[a |-> \"%s\", t |-> \"%s\"%s%s]\n", trace.records ? ", " : "  ", action,
	       a ? a->name : "env", *fields ? ", " : "", fields);
	trace.records++;
}

bool sim_spec_tracing(void)
{
	return trace.on;
}

void sim_spec_trace_begin(struct file *file, const char *path, u64 seed, unsigned long pages,
			  unsigned long private_max)
{
	int i, live = 0;

	for (i = 0; i < MAX_STATES; i++) {
		if (states[i]) {
			trace.state = states[i];
			live++;
		}
	}
	if (live != 1)
		sim_panic("spec trace: the run has %d ranges' states, not the one of its file", live);
	trace.on = true;
	trace.path = path;
	trace.mapping = file->f_mapping;
	trace.ino = file_inode(file)->i_ino;
	trace.pages = pages;
	trace.private_max = private_max;
	trace.seed = seed;
	trace.worker.name = "B";
}

void sim_spec_actor(struct task_struct *task, const char *name)
{
	if (trace.nr_actors == MAX_ACTORS)
		sim_panic("spec trace: too many actors");
	trace.actors[trace.nr_actors].task = task;
	trace.actors[trace.nr_actors++].name = name;
}

/* A set of the spec's pages, `{1, 3}`, of the pages from `first` whose bits `set` has. */
static void pages_of(char *out, size_t size, unsigned long first, unsigned long n,
		     bool (*has)(unsigned long page, const void *arg), const void *arg)
{
	size_t len = snprintf(out, size, "{");
	unsigned long page;
	bool any = false;

	for (page = first; page < first + n; page++) {
		if (!has(page, arg))
			continue;
		len += snprintf(out + len, size - len, "%s%lu", any ? ", " : "", page + 1);
		any = true;
	}
	snprintf(out + len, size - len, "}");
}

static void write_module(unsigned long stores)
{
	const char *sources = host_getenv("SIM_TRACE_SOURCES");
	char head[1024], names[256] = "";
	size_t len;
	int i;

	for (i = 0, len = 0; i < trace.nr_actors; i++) {
		if (trace.actors[i].name[0] == 'S')
			len += snprintf(names + len, sizeof(names) - len, "%s\"%s\"", len ? ", " : "",
					trace.actors[i].name);
	}
	snprintf(head, sizeof(head),
		 "---- MODULE TraceDaxCowData ----\n"
		 "\\* A run of guest/sim/tests/traced.c as specs/dax-cow's actions (guest/sim/kernel/spec_trace.c).\n"
		 "\\* sources: %s\n"
		 "\\* seed: %llu\n"
		 "EXTENDS Naturals, Sequences\n"
		 "TracePages == 1..%lu\n"
		 "TracePrivateMax == %lu\n"
		 "TraceMaxStores == %lu\n"
		 "TraceStorers == {%s}\n",
		 sources ? sources : "unknown", trace.seed, trace.pages, trace.private_max,
		 stores + 1, names);
	for (i = 0, len = 0, names[0] = 0; i < trace.nr_actors; i++) {
		if (trace.actors[i].name[0] != 'S')
			len += snprintf(names + len, sizeof(names) - len, "%s\"%s\"", len ? ", " : "",
					trace.actors[i].name);
	}
	host_write_file(trace.path, head, strlen(head), false);
	snprintf(head, sizeof(head), "TraceOthers == {%s%s\"B\"}\nTrace == <<\n", names,
		 *names ? ", " : "");
	host_write_file(trace.path, head, strlen(head), true);
	host_write_file(trace.path, trace.text ?: "", trace.len, true);
	snprintf(head, sizeof(head), ">>\n====\n");
	host_write_file(trace.path, head, strlen(head), true);
}

void sim_spec_trace_end(unsigned long stores)
{
	if (!trace.on)
		return;
	trace.on = false;
	write_module(stores);
	sim_log("spec trace: %llu records in %s\n", trace.records, trace.path);
	host_free(trace.text);
	trace.text = NULL;
}

void sim_spec_trace_panicked(void)
{
	static bool writing;

	if (!trace.on || writing)
		return;
	writing = true;
	trace.on = false;
	append("\\* the run panicked here\n");
	/* As many stores as a run of its operations may start: the trace's own are fewer. */
	write_module(trace.records);
}

void sim_spec_store_begins(unsigned long page, bool fault)
{
	struct actor *a = actor();

	if (!trace.on)
		return;
	if (!storer(a))
		sim_panic("spec trace: a store by %s, no storer", current->comm);
	a->page = page;
	a->fault = fault;
	record("Start", "p |-> %lu, w |-> \"%s\"", page + 1, fault ? "fault" : "write");
}

/* Whether `lock` is the traced file's `field`, learning its address the first time. */
static bool is_lock(const void *lock, const char *name, const char *field, const void **known)
{
	if (strcmp(name, field))
		return false;
	if (!*known)
		*known = lock;
	if (*known != lock)
		sim_panic("spec trace: a second file's %s: the run traces one file", field);
	return true;
}

void sim_spec_locked(const void *lock, const char *name, bool write)
{
	struct actor *a;

	if (!trace.on || !name)
		return;
	a = actor();
	if (is_lock(lock, name, "&file->wb_lock", &trace.wb_lock)) {
		if (a && a->rebind) {
			record("WStart", "op |-> \"rebind\"");
			record("RbWbLock", NULL);
			a->looked = false;
			a->look[0] = 0;
		} else {
			/* An fsync's every writeback, or one of its own: the budget worker's. */
			if (!a || !a->fsync)
				record("WStart", "op |-> \"writeback\"");
			record("WbLock", NULL);
		}
	} else if (is_lock(lock, name, "&file->sem", &trace.sem) && a && a->rebind) {
		record("RbLock", "m |-> \"%s\"", write ? "write" : "read");
	}
}

/* A rebind's look at Fxfs's extents, as of its first MAP or its end: what it rebinds. */
static void looked(struct actor *a)
{
	if (a->looked)
		return;
	record("RbLook", "ps |-> %s", a->look[0] ? a->look : "{}");
	a->looked = true;
}

void sim_spec_unlocked(const void *lock, const char *name)
{
	struct actor *a;

	if (!trace.on || !name)
		return;
	a = actor();
	if (is_lock(lock, name, "&file->wb_lock", &trace.wb_lock)) {
		record(a && a->rebind ? "RbWbUnlock" : "WbUnlock", NULL);
	} else if (is_lock(lock, name, "&file->sem", &trace.sem) && a) {
		if (storer(a) && a->begun) {
			if (!a->fault)
				record("SCopy", NULL);
			record("Unlock", NULL);
			a->begun = false;
		} else if (a->rebind) {
			looked(a);
			record("RbUnlock", NULL);
		}
	}
}

/* Whether `vma` maps the traced file. */
static bool traced(struct vm_area_struct *vma)
{
	return trace.on && vma->vm_file && vma->vm_file->f_mapping == trace.mapping;
}

void sim_spec_pte_writable(struct vm_area_struct *vma, unsigned long addr)
{
	if (traced(vma))
		record("Map", "p |-> %lu", linear_page_index(vma, addr) + 1);
}

void sim_spec_protected(struct address_space *mapping, unsigned long index)
{
	if (trace.on && mapping == trace.mapping)
		record("Protect", "p |-> %lu", index + 1);
}

void sim_spec_cleaned(struct vm_area_struct *vma, unsigned long index)
{
	if (traced(vma))
		record("Clean", "p |-> %lu", index + 1);
}

void sim_spec_user_store(struct vm_area_struct *vma, unsigned long addr, bool landed)
{
	struct actor *a = actor();
	unsigned long page;

	if (!traced(vma) || !storer(a))
		return;
	page = linear_page_index(vma, addr) + 1;
	if (landed && a->faulted)
		record("Resolved", "p |-> %lu", page);
	a->faulted = !landed;
	record(landed ? "Store" : "Fault", "p |-> %lu", page);
}

void sim_spec_fsync(struct file *file, bool end)
{
	struct actor *a = actor();

	if (!trace.on || file->f_mapping != trace.mapping)
		return;
	if (!a || storer(a))
		sim_panic("spec trace: an fsync by %s, which does none", current->comm);
	a->fsync = !end;
	record(end ? "FsyncEnd" : "FsyncStart", NULL);
}

void sim_spec_seal(void)
{
	if (trace.on)
		record("Seal", NULL);
}

/* The tracepoints the spec's steps are at (linux/tracepoint.h calls these, if defined). */
void sim_tp_fxfs_dax_fault_start(u64 ino, u64 pgoff, unsigned int order, bool write)
{
	struct actor *a = actor();

	if (trace.on && ino == trace.ino && storer(a))
		a->fault_write = write;
}

void sim_tp_fxfs_dax_fault_end(u64 ino, u64 pgoff, unsigned int ret)
{
	struct actor *a = actor();

	if (trace.on && ino == trace.ino && storer(a) && a->fault_write) {
		record("Return", NULL);
		a->fault_write = false;
	}
}

void sim_tp_fxfs_dax_rebind_start(u64 ino)
{
	struct actor *a = actor();

	if (trace.on && ino == trace.ino && a)
		a->rebind = true;
}

void sim_tp_fxfs_dax_rebind_end(u64 ino, long ret)
{
	struct actor *a = actor();

	if (trace.on && ino == trace.ino && a)
		a->rebind = false;
}

/*
 * pages.c's transitions, as dax.c calls them (--wrap): the traced range's are steps, or what the
 * spec has none for; another range's, while tracing, a divergence of the run from the one range
 * the trace assumes.
 */
struct fxfs_dax_pages *__real_fxfs_dax_pages_new(unsigned long nr);
void __real_fxfs_dax_pages_free(struct fxfs_dax_pages *pages);
void __real_fxfs_dax_pages_fill(struct fxfs_dax_pages *pages, unsigned long first, unsigned long n,
				bool zeros);
long __real_fxfs_dax_pages_rebind(struct fxfs_dax_pages *pages, unsigned long first,
				  unsigned long n, bool zeros);
struct fxfs_dax_change __real_fxfs_dax_pages_dirty(struct fxfs_dax_pages *pages,
						   unsigned long first, unsigned long n);
long __real_fxfs_dax_pages_private_ahead(struct fxfs_dax_pages *pages, unsigned long first,
					 unsigned long n);
unsigned long __real_fxfs_dax_pages_swap(struct fxfs_dax_pages *pages, unsigned long *set);
unsigned long __real_fxfs_dax_pages_redirty(struct fxfs_dax_pages *pages,
					    const unsigned long *set);
struct fxfs_dax_change __real_fxfs_dax_pages_cut(struct fxfs_dax_pages *pages, unsigned long first,
						 unsigned long n);
struct fxfs_dax_change __real_fxfs_dax_pages_unmap(struct fxfs_dax_pages *pages,
						   unsigned long first, unsigned long n);
int __real_fxfs_write_many(struct fxfs *fs, u64 ino, const struct fxfs_write_part *parts,
			   size_t count, u64 dirty);
ssize_t __real_fxfs_dax_extents(struct fxfs *fs, u64 ino, u64 offset, u64 len,
				struct fxfs_dax_extent *out, size_t max);

/* Whether `pages` is the traced range's, tracing: another's is a record no step matches. */
static bool traced_state(const struct fxfs_dax_pages *pages, const char *what)
{
	if (!trace.on)
		return false;
	if (pages == trace.state)
		return true;
	record("Unmodeled", "what |-> \"%s of another range\"", what);
	return false;
}

struct fxfs_dax_pages *__wrap_fxfs_dax_pages_new(unsigned long nr)
{
	struct fxfs_dax_pages *pages = __real_fxfs_dax_pages_new(nr);
	int i;

	for (i = 0; pages && i < MAX_STATES; i++) {
		if (!states[i]) {
			states[i] = pages;
			break;
		}
	}
	if (pages && i == MAX_STATES)
		sim_panic("spec trace: more than %d ranges", MAX_STATES);
	if (pages && trace.on)
		record("Unmodeled", "what |-> \"a new range\"");
	return pages;
}

void __wrap_fxfs_dax_pages_free(struct fxfs_dax_pages *pages)
{
	int i;

	for (i = 0; i < MAX_STATES; i++) {
		if (states[i] == pages)
			states[i] = NULL;
	}
	if (trace.on && pages == trace.state) {
		record("Unmodeled", "what |-> \"the range freed\"");
		trace.state = NULL;
	}
	__real_fxfs_dax_pages_free(pages);
}

/* A page's state, queried. */
static bool private_page(unsigned long page, const void *pages)
{
	/* Dirtying it would make it private: it isn't, or isn't mapped. */
	return !fxfs_dax_pages_would_dirty(pages, page, 1).private &&
	       fxfs_dax_pages_mapped(pages, page);
}

static bool in_set(unsigned long page, const void *set)
{
	return test_bit(page, (const unsigned long *)set);
}

void __wrap_fxfs_dax_pages_fill(struct fxfs_dax_pages *pages, unsigned long first, unsigned long n,
				bool zeros)
{
	if (traced_state(pages, "a fill"))
		record("Unmodeled", "what |-> \"a fill\", from |-> %lu, n |-> %lu", first + 1, n);
	__real_fxfs_dax_pages_fill(pages, first, n, zeros);
}

long __wrap_fxfs_dax_pages_rebind(struct fxfs_dax_pages *pages, unsigned long first,
				  unsigned long n, bool zeros)
{
	unsigned long page;
	struct actor *a;

	if (traced_state(pages, "a rebind")) {
		a = actor();
		if (a && a->rebind)
			looked(a);
		for (page = first; page < first + n; page++) {
			if (private_page(page, pages))
				record("RbMap", "p |-> %lu", page + 1);
		}
	}
	return __real_fxfs_dax_pages_rebind(pages, first, n, zeros);
}

struct fxfs_dax_change __wrap_fxfs_dax_pages_dirty(struct fxfs_dax_pages *pages,
						   unsigned long first, unsigned long n)
{
	struct actor *a = actor();
	unsigned long page;

	if (traced_state(pages, "a dirtying")) {
		for (page = first; page < first + n; page++) {
			record("Throttle", NULL);
			record("Lock", NULL);
			record("Begin", "p |-> %lu", page + 1);
		}
		if (storer(a)) {
			a->begun = true;
			a->faulted = false;
		}
	}
	return __real_fxfs_dax_pages_dirty(pages, first, n);
}

long __wrap_fxfs_dax_pages_private_ahead(struct fxfs_dax_pages *pages, unsigned long first,
					 unsigned long n)
{
	unsigned long page;

	if (traced_state(pages, "pages made private ahead")) {
		for (page = first; page < first + n; page++) {
			if (!private_page(page, pages))
				record("Ahead", "p |-> %lu", page + 1);
		}
	}
	return __real_fxfs_dax_pages_private_ahead(pages, first, n);
}

unsigned long __wrap_fxfs_dax_pages_swap(struct fxfs_dax_pages *pages, unsigned long *set)
{
	unsigned long swapped = __real_fxfs_dax_pages_swap(pages, set);
	char ps[256];
	int i;
	bool write = false, held = false;

	if (traced_state(pages, "a swap")) {
		for (i = 0; i < current->nr_held; i++) {
			if (current->held[i].lock == trace.sem) {
				held = true;
				write = current->held[i].write;
			}
		}
		pages_of(ps, sizeof(ps), 0, trace.pages, in_set, set);
		record("SwapLock", "m |-> \"%s\"", !held ? "none" : write ? "write" : "read");
		record("Swap", "ps |-> %s", ps);
	}
	return swapped;
}

unsigned long __wrap_fxfs_dax_pages_redirty(struct fxfs_dax_pages *pages, const unsigned long *set)
{
	if (traced_state(pages, "a writeback's pages dirty again"))
		record("Unmodeled", "what |-> \"a writeback's pages dirty again\"");
	return __real_fxfs_dax_pages_redirty(pages, set);
}

struct fxfs_dax_change __wrap_fxfs_dax_pages_cut(struct fxfs_dax_pages *pages, unsigned long first,
						 unsigned long n)
{
	if (traced_state(pages, "a cut"))
		record("Unmodeled", "what |-> \"a cut\", from |-> %lu, n |-> %lu", first + 1, n);
	return __real_fxfs_dax_pages_cut(pages, first, n);
}

struct fxfs_dax_change __wrap_fxfs_dax_pages_unmap(struct fxfs_dax_pages *pages,
						   unsigned long first, unsigned long n)
{
	if (traced_state(pages, "an UNMAP"))
		record("Unmodeled", "what |-> \"an UNMAP\", from |-> %lu, n |-> %lu", first + 1, n);
	return __real_fxfs_dax_pages_unmap(pages, first, n);
}

/*
 * Fxfs's writeback of the traced file, a writeback's: its copy of the pages is as of its WRITE,
 * its stores meanwhile to pages the writeback write-protected, faulting, dirtied again; its
 * commit as it returns, with no scheduling point between.
 */
int __wrap_fxfs_write_many(struct fxfs *fs, u64 ino, const struct fxfs_write_part *parts,
			   size_t count, u64 dirty)
{
	bool traced_file = trace.on && ino == trace.ino;
	int err;

	if (traced_file) {
		record("WCopy", NULL);
		trace.writer = current;
	}
	err = __real_fxfs_write_many(fs, ino, parts, count, dirty);
	if (traced_file)
		trace.writer = NULL;
	if (traced_file && trace.on)
		record(err ? "Unmodeled" : "Commit", err ? "what |-> \"a commit failed\"" : NULL);
	return err;
}

/* The device's WRITE of a writeback's pages, the model of Fxfs's, which no seal comes into. */
void sim_spec_device_wrote(void)
{
	if (trace.on && trace.writer == current)
		record("Write", NULL);
}

struct look {
	const struct fxfs_dax_extent *found;
	ssize_t count;
};

/* Whether a rebind looking at what `look` found rebinds `page`: private, clean, not open. */
static bool rebinds(unsigned long page, const void *arg)
{
	const struct look *look = arg;
	unsigned long end;
	u64 at = (u64)page << PAGE_SHIFT;
	ssize_t i;

	if (fxfs_dax_pages_next_rebindable(trace.state, page, &end) != page)
		return false;
	for (i = 0; i < look->count; i++) {
		if (at >= look->found[i].offset && at < look->found[i].offset + look->found[i].len)
			return look->found[i].access == FXFS_DAX_SEALED;
	}
	return true;
}

ssize_t __wrap_fxfs_dax_extents(struct fxfs *fs, u64 ino, u64 offset, u64 len,
				struct fxfs_dax_extent *out, size_t max)
{
	ssize_t count = __real_fxfs_dax_extents(fs, ino, offset, len, out, max);
	struct actor *a = actor();
	struct look look = { out, count };

	if (trace.on && ino == trace.ino && a && a->rebind && count >= 0 && trace.state)
		pages_of(a->look, sizeof(a->look), offset >> PAGE_SHIFT,
			 min_t(unsigned long, DIV_ROUND_UP(len, PAGE_SIZE),
			       trace.pages - (offset >> PAGE_SHIFT)),
			 rebinds, &look);
	return count;
}
