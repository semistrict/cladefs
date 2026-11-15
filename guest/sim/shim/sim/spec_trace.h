/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The fxdax Authors. */
/*
 * A run as specs/dax-cow's actions (TASK-444.1, kernel/spec_trace.c): each step one of DaxCow's,
 * as tests/traced.c's tasks take them on the one file it traces, written once the run ends as a
 * TLA+ module that specs/dax-cow/trace/TraceDaxCow.tla follows, step by step, to its end or to
 * the first step the spec doesn't allow. The fake kernel says what it does (the hooks below), and
 * pages.c's transitions, Fxfs's writeback and its lookups of extents are wrapped as they are
 * linked (the Makefile's --wrap), so that dax.c is as it is.
 */
#ifndef _SIM_SPEC_TRACE_H
#define _SIM_SPEC_TRACE_H

#include <sim/base.h>

struct file;
struct task_struct;
struct vm_area_struct;
struct address_space;

/*
 * Traces `file`'s steps from now, those of the tasks named (a storer's as `S...`, a task doing
 * the rest as `F...`), and of dax.c's budget worker (`B`), into the module at `path` once
 * sim_spec_trace_end ends it, with the run's seed and the constants the spec needs: the file's
 * pages, its private pages' most, and how many stores the run starts at most.
 */
void sim_spec_trace_begin(struct file *file, const char *path, u64 seed, unsigned long pages,
			  unsigned long private_max);
void sim_spec_actor(struct task_struct *task, const char *name);
void sim_spec_trace_end(unsigned long stores);
/* The trace so far, written as the run panics, unless one is being written. */
void sim_spec_trace_panicked(void);
/* Whether a run traces. */
bool sim_spec_tracing(void);

/* A storer's store to page `page` beginning: through its mapping (`fault`), or by write(2). */
void sim_spec_store_begins(unsigned long page, bool fault);

/* The fake kernel's hooks. */
void sim_spec_locked(const void *lock, const char *name, bool write);
void sim_spec_unlocked(const void *lock, const char *name);
void sim_spec_pte_writable(struct vm_area_struct *vma, unsigned long addr);
void sim_spec_protected(struct address_space *mapping, unsigned long index);
void sim_spec_cleaned(struct vm_area_struct *vma, unsigned long index);
void sim_spec_user_store(struct vm_area_struct *vma, unsigned long addr, bool landed);
void sim_spec_fsync(struct file *file, bool end);
void sim_spec_seal(void);
void sim_spec_device_wrote(void);

#endif
