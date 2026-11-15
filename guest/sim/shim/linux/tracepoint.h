/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The fxdax Authors. */
/*
 * Tracepoints: each a function that calls the sim's sim_tp_NAME, if one is defined (a weak
 * symbol: kernel/spec_trace.c defines those its trace of a run takes steps at), and off, so that
 * the kernel's own trace headers compile, and fs/fxfs's (fxfs_trace.h).
 */
#ifndef _SIM_TRACEPOINT_H
#define _SIM_TRACEPOINT_H

#define TP_PROTO(...) __VA_ARGS__
#define TP_ARGS(...) __VA_ARGS__
#define TRACE_DEFINE_ENUM(x)
#define SIM_TP_PARAMS(...) __VA_ARGS__
#define SIM_TRACEPOINT(name, proto, args)					\
	extern void sim_tp_##name(proto) __attribute__((weak));		\
	static inline void trace_##name(proto)					\
	{									\
		if (sim_tp_##name)						\
			sim_tp_##name(args);					\
	}									\
	static inline bool trace_##name##_enabled(void) { return false; }
#define DECLARE_EVENT_CLASS(...)
#define DEFINE_EVENT(template, name, proto, args)				\
	SIM_TRACEPOINT(name, SIM_TP_PARAMS(proto), SIM_TP_PARAMS(args))
#define TRACE_EVENT(name, proto, args, ...)					\
	SIM_TRACEPOINT(name, SIM_TP_PARAMS(proto), SIM_TP_PARAMS(args))

#endif
