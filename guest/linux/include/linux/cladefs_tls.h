/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * cladefs: the thread-local values of fs/cladefs's Rust, which a task's `cladefs_tls` holds
 * (guest/linux/patches/0001-sched-cladefs-tls.patch), and which are dropped as the task exits.
 */
#ifndef _LINUX_CLADEFS_TLS_H
#define _LINUX_CLADEFS_TLS_H

struct task_struct;

#ifdef CONFIG_CLADEFS_FS
void cladefs_rt_tls_exit(struct task_struct *tsk);
#else
static inline void cladefs_rt_tls_exit(struct task_struct *tsk)
{
}
#endif

#endif
