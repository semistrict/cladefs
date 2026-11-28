// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The cladefs Authors.
/*
 * The virtio extent-store device's driver (cladefs's backlog doc-4; include/linux/virtio_extent.h).
 *
 * Requests go to the device's request queues as descriptor chains, each CPU's to its own: the
 * header and arguments, the data the device reads, the data it writes, and the response. Many may
 * be in flight; the queue's interrupt, on the CPUs whose queue it is, completes them, calling each
 * one's `done`. A submitter waits for room when
 * the queue is full. The DAX window, if the device has one, is memremapped, so that the guest's
 * Fxfs reads extents mapped into it in place: as pages of a dax_device, with FS_DAX, as
 * virtio_fs_setup_dax makes its cache window, which files map (fs/cladefs/dax.c). cladefs: without
 * FS_DAX, the driver takes no window: nothing would map it.
 */

#include <linux/completion.h>
#include <linux/delay.h>
#include <linux/dax.h>
#include <linux/device/driver.h>
#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/kthread.h>
#include <linux/memremap.h>
#include <linux/list.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/net.h>
#include <linux/sched.h>
#include <linux/sched/clock.h>
#include <linux/sched/mm.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/virtio.h>
#include <linux/virtio_config.h>
#include <linux/virtio_extent.h>
#include <linux/vm_sockets.h>
#include <linux/vmalloc.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <linux/xarray.h>
#include <net/sock.h>

#include "selftest.h"

/* A request queue (cladefs TASK-26.18.5). */
struct vext_queue {
	struct virtqueue *vq;
	/* Serializes the queue's use. */
	spinlock_t lock;
	/*
	 * cladefs: whether it takes no requests, as the device resets (vext_reset): set, and read
	 * before a request is added, under `lock`, so that none is added to a queue the reset goes on
	 * to let go of (TASK-201).
	 */
	bool closed;
	/* Woken as the device completes requests, making room in the queue. */
	wait_queue_head_t room;
	/* The requests it has taken. */
	atomic64_t requests;
	/*
	 * cladefs: the callers polling it for their requests, while it has any of which the device
	 * signals no completions (vext_wait). Guarded by `lock`.
	 */
	unsigned int pollers;
	/*
	 * cladefs: its interrupts, and the requests it completed, which those callers did without one:
	 * each interrupt costs exits to read and acknowledge it (TASK-91.15).
	 */
	atomic64_t interrupts, polled;
	/*
	 * cladefs: the requests completed on a CPU other than the one they were queued on: each wakes
	 * its caller with an interrupt between CPUs, and finds its request in another's cache
	 * (TASK-276).
	 */
	atomic64_t cross;
} ____cacheline_aligned_in_smp;

/*
 * cladefs: the device's DAX window, its shared memory region, as the pages of a dax_device, which
 * mappings of files map (fs/cladefs/dax.c).
 *
 * Its struct pages are made a part at a time, as dax_direct_access first reaches a part
 * (TASK-92.8). Made whole as the device was probed, a 2 GiB window's took a third of the guest's
 * boot, and 32 MiB of its memory (64 bytes a page), most of which a guest never maps: Fxfs hands
 * the window's space out first fit, from its start and from its middle (fs/cladefs/dax.c). A part is
 * a memory section of the window, or what of one it covers: hotplug maps a whole section's struct
 * pages on huge pages, and each making waits for an RCU grace period (mem_hotplug_begin).
 * Nothing reaches the window's memory but through dax_direct_access, so no page is used before it
 * has a struct page.
 */
struct vext_window {
	phys_addr_t phys;
	u64 len;
	/* Its memory, mapped in the kernel, where each part's making maps it (the direct map). */
	void *kaddr;
	struct dax_device *dax;
	/* Its parts' pages, the first's section the window's first; their node's. */
	struct dev_pagemap *parts;
	unsigned int nr_parts;
	int nid;
	/* Which parts have struct pages, which `lock` serializes the making of. */
	unsigned long *made_parts;
	struct mutex lock;
	/* How much of it has struct pages, and the time making them took. */
	atomic64_t made, made_ns;
};

/*
 * cladefs: doc-4's stream transport (F_STREAM, board 14663): the data requests, READ, WRITE, FREE,
 * COMMIT, ROOT and INFO, go over a vsock stream to the host's extent daemon, which serves them
 * itself, rather than on the device's queues, which its VMM serves. Its frames go whole, one after
 * another: a submitter with none queued before it sends its own without waiting, which a signal
 * can't cut short, and a thread of the stream's own, the sender, sends the rest, the ones queued,
 * and what of a submitter's the socket had no room for, waking for no request that went at once.
 * It connects as it first has one to send, the vsock device probed by then. Another reads the replies, in any order,
 * completing each by its tag, as a queue's interrupt does. A stream whose connection is lost, as
 * the vsock device resets every connection when its VM is restored from a snapshot, connects again,
 * and sends its requests in flight again, each whole, as every data request may be: they are its
 * guest's, which sends a barrier only once what it orders completed (doc-4). One that can't, its
 * host's daemon gone, or whose host breaks the protocol, fails every request in flight and every
 * one after (-EIO), as a device whose daemon is gone does (doc-4).
 */
struct vext_stream {
	/*
	 * Its connection's socket, which its sender alone sets, and lets go of, under `sock_lock`, which
	 * a shutdown from any task takes too, so that none shuts down a socket let go of.
	 */
	struct mutex sock_lock;
	struct socket *sock;
	struct task_struct *sender, *receiver;
	/*
	 * Who sends on the connection, one frame at a time, whole: the sender, or a submitter sending
	 * its own request without waiting (vext_stream_submit). `partial` is a request whose frame a
	 * submitter's send began and the socket had no room for the rest of, which the sender sends the
	 * rest of before anything else; under `send_lock`.
	 */
	struct mutex send_lock;
	struct vext_request *partial;
	/* The requests waiting to be sent, which the sender takes, under `lock`. */
	spinlock_t lock;
	struct list_head to_send;
	wait_queue_head_t wake;
	/* The requests sent or being sent, by tag. */
	struct xarray in_flight;
	u32 next_tag;
	/*
	 * The tag of the request the sender is sending, U64_MAX between sends, and what its receiver
	 * waits on for it: a reply read before its request's send returned, which a host's bug could
	 * send, completes it only once the sender is done with its memory.
	 */
	u64 sending;
	wait_queue_head_t sent_wait;
	/* Its -errno once it failed, or closed as the device resets; under `lock`. */
	int error;
	/*
	 * Whether its connection is lost, which its sender connects again (vext_stream_reconnect);
	 * under `lock`. And the connections lost since it last read a reply, VEXT_STREAM_RETRIES at
	 * most: one lost again before any reply fails it.
	 */
	bool broken;
	unsigned int retries;
	/* The requests it sent, and the replies it read. */
	atomic64_t sent, replies;
};

struct virtio_extent {
	struct virtio_device *vdev;
	/*
	 * Its request queues: one per CPU, up to as many as the device has (num_request_queues), each
	 * CPU's requests going to its own, so that CPUs don't contend for one.
	 */
	struct vext_queue *queues;
	unsigned int nr_queues;
	/*
	 * cladefs: the queue each CPU's requests go to, by CPU: the one whose interrupt the transport
	 * made affine to it, so that they complete on it (vext_map_queues, TASK-276).
	 */
	unsigned int *cpu_queue;
	struct vext_config config;
	/* cladefs: its DAX window, if it has one (F_DAX, with FS_DAX). */
	struct vext_window window;
	/* cladefs: its data requests' stream, if it has one (F_STREAM, board 14663). */
	struct vext_stream stream;
	atomic64_t unique;
	/* The requests the device has taken, by type. */
	atomic64_t requests[VIRTIO_EXTENT_T_LAST + 1];
	/* The time the requests it has completed took, queued to completed, by type. */
	atomic64_t request_ns[VIRTIO_EXTENT_T_LAST + 1];
	/*
	 * cladefs: the time it has had a request in flight, all told (busy_ns): the guest waiting for
	 * the host, which its checks' budgets leave out (TASK-263). The requests taken and not yet
	 * completed, since when it has had one, and the time it had one before, under `busy_lock`,
	 * which a queue's lock is taken before.
	 */
	spinlock_t busy_lock;
	unsigned int in_flight;
	u64 busy_since;
	u64 busy_ns;
	/*
	 * cladefs: the time recent requests of each type took, a moving average, to wait for one on
	 * the CPU only if it takes less than that is worth (vext_wait).
	 */
	u64 recent_ns[VIRTIO_EXTENT_T_LAST + 1];
	/* What its configuration's changes go to (virtio_extent_set_config_changed). */
	spinlock_t changed_lock;
	vext_config_changed_t changed;
	void *changed_arg;
	/*
	 * cladefs: what answers the host's asks for the guest's stats (virtio_extent_set_stats), which
	 * `stats_lock` guards; the work answering them, from a configuration change; and the ask it
	 * answered last (TASK-111.4).
	 */
	struct mutex stats_lock;
	vext_stats_t stats;
	void *stats_arg;
	struct work_struct stats_work;
	u64 stats_answered;
	/* cladefs: those who found it and have yet to put it, whom unbinding it waits for. */
	atomic_t users;
	/*
	 * cladefs: its self-test runs, holding it alone: no one finds it meanwhile (board 14352).
	 * Under devices_lock.
	 */
	bool testing;
	struct list_head node;
};

/* The bound devices. */
static LIST_HEAD(devices);
static DEFINE_MUTEX(devices_lock);
/* cladefs: unbinding, for a device's `users` to go; not the device's, which then goes too. */
static DECLARE_WAIT_QUEUE_HEAD(devices_unused);

static struct virtio_extent *vext_find_bound(const char *tag)
{
	struct virtio_extent *vext, *found = NULL;

	mutex_lock(&devices_lock);
	list_for_each_entry(vext, &devices, node) {
		if (!strcmp(vext->config.tag, tag) && !vext->testing) {
			atomic_inc(&vext->users);
			found = vext;
			break;
		}
	}
	mutex_unlock(&devices_lock);
	return found;
}

/*
 * cladefs: takes the device for its self-test alone, which writes extents and commits a root of
 * its own: -EBUSY while anyone holds it, a mounted filesystem whose root it would replace among
 * them (board 14352), or another self-test. No one finds it until vext_selftest_end().
 */
int vext_selftest_begin(struct virtio_extent *vext)
{
	int err = 0;

	mutex_lock(&devices_lock);
	if (vext->testing || atomic_read(&vext->users))
		err = -EBUSY;
	else
		vext->testing = true;
	mutex_unlock(&devices_lock);
	return err;
}

void vext_selftest_end(struct virtio_extent *vext)
{
	mutex_lock(&devices_lock);
	vext->testing = false;
	mutex_unlock(&devices_lock);
}

/*
 * cladefs: the device tagged `tag`, once probes in progress are done if none is bound yet: devices
 * are probed asynchronously (vext_driver), so that the window's memmap, built as a device is
 * probed, a third of a fork's boot, overlaps the rest of the boot (TASK-92.8); a mount by tag
 * waits for its device's probe, as the kernel waits for probes before it mounts the root.
 */
struct virtio_extent *virtio_extent_find(const char *tag)
{
	struct virtio_extent *found = vext_find_bound(tag);

	if (found)
		return found;
	wait_for_device_probe();
	return vext_find_bound(tag);
}
EXPORT_SYMBOL_GPL(virtio_extent_find);

void virtio_extent_put(struct virtio_extent *vext)
{
	if (atomic_dec_and_test(&vext->users))
		wake_up_all(&devices_unused);
}
EXPORT_SYMBOL_GPL(virtio_extent_put);

const struct vext_config *virtio_extent_config(struct virtio_extent *vext)
{
	return &vext->config;
}
EXPORT_SYMBOL_GPL(virtio_extent_config);

struct dax_device *virtio_extent_dax(struct virtio_extent *vext, u64 *window_len)
{
	*window_len = vext->window.len;
	return vext->window.dax;
}
EXPORT_SYMBOL_GPL(virtio_extent_dax);

u64 virtio_extent_private_max(struct virtio_extent *vext)
{
	return vext->config.private_max;
}
EXPORT_SYMBOL_GPL(virtio_extent_private_max);

bool virtio_extent_write_commit(struct virtio_extent *vext)
{
	return virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_WRITE_COMMIT);
}
EXPORT_SYMBOL_GPL(virtio_extent_write_commit);

bool virtio_extent_freeze(struct virtio_extent *vext)
{
	return virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_FREEZE);
}
EXPORT_SYMBOL_GPL(virtio_extent_freeze);

bool virtio_extent_free_many(struct virtio_extent *vext)
{
	return virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_FREE_MANY);
}
EXPORT_SYMBOL_GPL(virtio_extent_free_many);

bool virtio_extent_map_many(struct virtio_extent *vext)
{
	return virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_MAP_MANY);
}
EXPORT_SYMBOL_GPL(virtio_extent_map_many);

bool virtio_extent_map_prefault(struct virtio_extent *vext)
{
	return virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_MAP_PREFAULT);
}
EXPORT_SYMBOL_GPL(virtio_extent_map_prefault);

/* cladefs: whether the host asks the guest for its stats (F_STATS, TASK-111.4). */
static bool vext_stats_asked(struct virtio_extent *vext)
{
	return virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_STATS);
}

/* Describes `len` bytes of `buf`, kmalloc'd or vmalloc'd, in `sgt`, a page at a time if need be. */
static int vext_map_data(struct sg_table *sgt, void *buf, size_t len)
{
	struct scatterlist *sg;
	unsigned int pages, i;
	size_t offset;
	int err;

	if (!is_vmalloc_addr(buf)) {
		err = sg_alloc_table(sgt, 1, GFP_KERNEL);
		if (!err)
			sg_init_one(sgt->sgl, buf, len);
		return err;
	}
	offset = offset_in_page(buf);
	pages = DIV_ROUND_UP(offset + len, PAGE_SIZE);
	err = sg_alloc_table(sgt, pages, GFP_KERNEL);
	if (err)
		return err;
	for_each_sg(sgt->sgl, sg, pages, i) {
		size_t chunk = min_t(size_t, PAGE_SIZE - offset, len);

		sg_set_page(sg, vmalloc_to_page(buf), chunk, offset);
		buf += chunk;
		len -= chunk;
		offset = 0;
	}
	return 0;
}

static struct virtio_extent_id vext_wire_id(struct vext_id id)
{
	return (struct virtio_extent_id) { cpu_to_le64(id.cell), cpu_to_le64(id.sequence) };
}

/*
 * The encoders of requests' arguments, into req->args, given the lengths of the data and the root
 * the request carries: the arguments' length.
 */
static u32 vext_args_io(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.io = (struct virtio_extent_io) {
		.id = vext_wire_id(req->id),
		.offset = cpu_to_le64(req->offset),
		.len = cpu_to_le32(data_len),
		.root_len = cpu_to_le32(root_len),
	};
	return sizeof(req->args.io);
}

static u32 vext_args_id(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.id = vext_wire_id(req->id);
	return sizeof(req->args.id);
}

static u32 vext_args_map(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.map = (struct virtio_extent_map) {
		.id = vext_wire_id(req->id),
		.offset = cpu_to_le64(req->offset),
		.len = cpu_to_le64(req->len),
		.window_offset = cpu_to_le64(req->window_offset),
	};
	return sizeof(req->args.map);
}

static u32 vext_args_unmap(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.unmap = (struct virtio_extent_unmap) {
		.window_offset = cpu_to_le64(req->window_offset),
		.len = cpu_to_le64(req->len),
	};
	return sizeof(req->args.unmap);
}

static u32 vext_args_private(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.private = (struct virtio_extent_private) {
		.window_offset = cpu_to_le64(req->window_offset),
		.len = cpu_to_le64(req->len),
	};
	return sizeof(req->args.private);
}

/* A COMMIT's data is the root it sets. */
static u32 vext_args_commit(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.commit = (struct virtio_extent_commit) { .root_len = cpu_to_le32(data_len) };
	return sizeof(req->args.commit);
}

static u32 vext_args_resized(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.resized = (struct virtio_extent_resized) { .capacity = cpu_to_le64(req->offset) };
	return sizeof(req->args.resized);
}

/* cladefs: a STATS names the ask it answers (TASK-111.4). */
static u32 vext_args_stats(struct vext_request *req, u32 data_len, u32 root_len)
{
	req->args.stats = (struct virtio_extent_stats) { .asked = cpu_to_le64(req->offset) };
	return sizeof(req->args.stats);
}

/* Which way a request's data goes: to the device, which reads it, or from it, which writes it. */
enum vext_way { VEXT_NO_DATA, VEXT_OUT, VEXT_IN };

/*
 * cladefs: what a request of a type carries after its header, a row per type, its whole wire
 * contract (TASK-160.7): its arguments, which `args` encodes, if it has any; its data, the
 * request's `data_len` bytes, which go `data`'s way, if the request has the flag `data_if`, or
 * always if that is 0; and a root after its data, the request's `root_len` bytes, if it has the
 * flag `root_if`. And its name, its counters' in sysfs.
 */
static const struct vext_type {
	const char *name;
	u32 (*args)(struct vext_request *req, u32 data_len, u32 root_len);
	enum vext_way data;
	u32 data_if;
	u32 root_if;
} vext_types[VIRTIO_EXTENT_T_LAST + 1] = {
	[VIRTIO_EXTENT_T_READ] = { "read", vext_args_io, VEXT_IN },
	[VIRTIO_EXTENT_T_WRITE] = { "write", vext_args_io, VEXT_OUT,
				    .root_if = VIRTIO_EXTENT_WRITE_ROOT },
	[VIRTIO_EXTENT_T_FREE] = { "free", vext_args_id, VEXT_OUT, VIRTIO_EXTENT_FREE_MANY },
	[VIRTIO_EXTENT_T_MAP] = { "map", vext_args_map, VEXT_OUT, VIRTIO_EXTENT_MAP_MANY },
	[VIRTIO_EXTENT_T_UNMAP] = { "unmap", vext_args_unmap },
	[VIRTIO_EXTENT_T_COMMIT] = { "commit", vext_args_commit, VEXT_OUT,
				     VIRTIO_EXTENT_COMMIT_ROOT },
	[VIRTIO_EXTENT_T_ROOT] = { "root", NULL, VEXT_IN },
	[VIRTIO_EXTENT_T_INFO] = { "info", NULL, VEXT_IN },
	[VIRTIO_EXTENT_T_RESIZED] = { "resized", vext_args_resized },
	[VIRTIO_EXTENT_T_FREEZE] = { "freeze" },
	/* 11 to 14 were a live follower's, 15 writable DAX's SEALED: no type. */
	[VIRTIO_EXTENT_T_PRIVATE] = { "private", vext_args_private },
	/* cladefs: the guest's stats, its text (TASK-111.4). */
	[VIRTIO_EXTENT_T_STATS] = { "stats", vext_args_stats, VEXT_OUT },
};

/* cladefs: a request taken, in flight until vext_busy_end (`busy_ns`). */
static void vext_busy_begin(struct virtio_extent *vext)
{
	unsigned long flags;

	spin_lock_irqsave(&vext->busy_lock, flags);
	if (!vext->in_flight++)
		vext->busy_since = ktime_get_ns();
	spin_unlock_irqrestore(&vext->busy_lock, flags);
}

/* cladefs: a request completed, or let go of as the device reset. */
static void vext_busy_end(struct virtio_extent *vext)
{
	unsigned long flags;

	spin_lock_irqsave(&vext->busy_lock, flags);
	if (!WARN_ON_ONCE(!vext->in_flight) && !--vext->in_flight)
		vext->busy_ns += ktime_get_ns() - vext->busy_since;
	spin_unlock_irqrestore(&vext->busy_lock, flags);
}

/* The row of `req`'s type, or NULL if it has none. */
static const struct vext_type *vext_type(const struct vext_request *req)
{
	if (req->type > VIRTIO_EXTENT_T_LAST || !vext_types[req->type].name)
		return NULL;
	return &vext_types[req->type];
}

/* Whether `req` has `flag`, which none is had always. */
static bool vext_has(const struct vext_request *req, u32 flag)
{
	return !flag || req->flags & flag;
}

/*
 * cladefs: the bytes a stream buffers each way, its vsock socket's: room for a few requests of
 * max_transfer in flight, the default (256 KiB) less than one (board 14663).
 */
#define VEXT_STREAM_BUFFER (8ULL << 20)

/* cladefs: the stream's connections lost in a row, no reply read between, before it fails. */
#define VEXT_STREAM_RETRIES 3
/* cladefs: the stream's tries to connect again, and the wait between them: a second, all told. */
#define VEXT_STREAM_CONNECT_TRIES 50
#define VEXT_STREAM_CONNECT_WAIT_MS 20

/* cladefs: whether `req` goes on the stream (F_STREAM): a data request, of a device with one. */
static bool vext_on_stream(struct virtio_extent *vext, const struct vext_request *req)
{
	if (!virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_STREAM))
		return false;
	switch (req->type) {
	case VIRTIO_EXTENT_T_READ:
	case VIRTIO_EXTENT_T_WRITE:
	case VIRTIO_EXTENT_T_FREE:
	case VIRTIO_EXTENT_T_COMMIT:
	case VIRTIO_EXTENT_T_ROOT:
	case VIRTIO_EXTENT_T_INFO:
		return true;
	default:
		return false;
	}
}

/* cladefs: completes `req`, sent on the stream, with `status`, as vext_reap a queue's. */
static void vext_stream_complete(struct virtio_extent *vext, struct vext_request *req, int status)
{
	u64 took = ktime_get_ns() - req->queued_ns, *recent;

	atomic64_add(took, &vext->request_ns[req->type]);
	recent = &vext->recent_ns[req->type];
	WRITE_ONCE(*recent, READ_ONCE(*recent) - (READ_ONCE(*recent) >> 3) + (took >> 3));
	req->status = status;
	vext_busy_end(vext);
	req->done(req);
}

/*
 * cladefs: fails the stream with `err`, if it hasn't failed: its socket shut down, which ends its
 * receiver's read and its sender's send, and its sender woken, which fails every request in
 * flight, and waiting to be sent (vext_stream_fail_all). From any task.
 */
/* cladefs: shuts the stream's socket down, if it has one, ending every read and send of it. */
static void vext_stream_shutdown(struct vext_stream *stream)
{
	mutex_lock(&stream->sock_lock);
	if (stream->sock)
		kernel_sock_shutdown(stream->sock, SHUT_RDWR);
	mutex_unlock(&stream->sock_lock);
}

static void vext_stream_fail(struct virtio_extent *vext, int err)
{
	struct vext_stream *stream = &vext->stream;
	unsigned long flags;
	bool first;

	spin_lock_irqsave(&stream->lock, flags);
	first = !stream->error;
	if (first)
		stream->error = err;
	spin_unlock_irqrestore(&stream->lock, flags);
	if (first)
		vext_stream_shutdown(stream);
	wake_up(&stream->wake);
	wake_up(&stream->sent_wait);
}

/*
 * cladefs: the stream's connection is lost, unless it failed: its socket shut down, which ends its
 * receiver's read and its sender's send, and its sender woken, which connects again and sends its
 * requests in flight again (vext_stream_reconnect). From any task.
 */
static void vext_stream_break(struct virtio_extent *vext)
{
	struct vext_stream *stream = &vext->stream;
	unsigned long flags;
	bool first;

	spin_lock_irqsave(&stream->lock, flags);
	first = !stream->error && !stream->broken;
	if (first)
		stream->broken = true;
	spin_unlock_irqrestore(&stream->lock, flags);
	if (first)
		vext_stream_shutdown(stream);
	wake_up(&stream->wake);
	wake_up(&stream->sent_wait);
}

/*
 * cladefs: fails every request of the stream's in flight and waiting to be sent, -EIO, the stream
 * having failed: on its sender, which alone sends, so that none it is sending completes, and its
 * memory goes, meanwhile.
 */
static void vext_stream_fail_all(struct virtio_extent *vext)
{
	struct vext_stream *stream = &vext->stream;
	struct vext_request *req, *next;
	unsigned long index;
	LIST_HEAD(waiting);

	spin_lock_irq(&stream->lock);
	list_splice_init(&stream->to_send, &waiting);
	spin_unlock_irq(&stream->lock);
	list_for_each_entry_safe(req, next, &waiting, stream_node) {
		list_del(&req->stream_node);
		vext_stream_complete(vext, req, -EIO);
	}
	/* Each once: the receiver may take one meanwhile, whose reply it read. */
	xa_for_each(&stream->in_flight, index, req) {
		if (xa_erase(&stream->in_flight, index) == req)
			vext_stream_complete(vext, req, -EIO);
	}
}

/*
 * cladefs: moves `len` bytes between the stream's socket and `vec`, all of them: sent, if `send`,
 * or received. 0, or a -errno, -EPIPE if the stream ended.
 */
static int vext_stream_io(struct socket *sock, struct kvec *vec, unsigned int n, size_t len,
			  bool send)
{
	struct msghdr msg = { .msg_flags = send ? MSG_NOSIGNAL : MSG_WAITALL };
	int moved;

	iov_iter_kvec(&msg.msg_iter, send ? ITER_SOURCE : ITER_DEST, vec, n, len);
	while (msg_data_left(&msg)) {
		moved = send ? sock_sendmsg(sock, &msg) : sock_recvmsg(sock, &msg, msg.msg_flags);
		if (moved < 0)
			return moved;
		if (!moved)
			return -EPIPE;
	}
	return 0;
}

/*
 * cladefs: sends what is left of `req`'s frame, from its `stream_progress`, waiting for room unless
 * `nowait`: 0 once it is all sent; -EAGAIN, with nowait, the socket full, its progress kept; or a
 * -errno, the connection lost.
 */
static int vext_stream_put(struct socket *sock, struct vext_request *req, bool nowait)
{
	struct msghdr msg = { .msg_flags = MSG_NOSIGNAL | (nowait ? MSG_DONTWAIT : 0) };
	size_t len = sizeof(req->stream_hdr) + le32_to_cpu(req->stream_hdr.readable);
	struct kvec vec[5];
	unsigned int n = 0;
	int moved;

	vec[n++] = (struct kvec) { &req->stream_hdr, sizeof(req->stream_hdr) };
	vec[n++] = (struct kvec) { &req->hdr, sizeof(req->hdr) };
	if (req->args_len)
		vec[n++] = (struct kvec) { &req->args, req->args_len };
	if (req->out_len)
		vec[n++] = (struct kvec) { req->data, req->out_len };
	if (req->root_out)
		vec[n++] = (struct kvec) { req->root, req->root_out };
	iov_iter_kvec(&msg.msg_iter, ITER_SOURCE, vec, n, len);
	iov_iter_advance(&msg.msg_iter, req->stream_progress);
	while (msg_data_left(&msg)) {
		moved = sock_sendmsg(sock, &msg);
		if (moved == -EAGAIN && nowait)
			return -EAGAIN;
		if (moved < 0)
			return moved;
		if (!moved)
			return -EPIPE;
		req->stream_progress += moved;
	}
	return 0;
}

/*
 * cladefs: the stream's receiver: reads each reply, and completes the request its tag names with
 * what the device wrote, until the stream fails or closes. A reply of nothing written completes its
 * request -EIO; one naming no request in flight, or of other than its request's writable bytes,
 * fails the stream, which nothing after it could be read in step with.
 */
static int vext_stream_receiver(void *data)
{
	struct virtio_extent *vext = data;
	struct vext_stream *stream = &vext->stream;
	struct virtio_extent_stream_reply head;
	struct vext_request *req;
	struct kvec vec[2];
	unsigned int n;
	u32 written, want;
	u64 tag;
	int err;

	/* Its allocations reclaim nothing that waits on requests of the device's. */
	memalloc_noio_save();
	for (;;) {
		vec[0] = (struct kvec) { &head, sizeof(head) };
		err = vext_stream_io(stream->sock, vec, 1, sizeof(head), false);
		if (err)
			break;
		tag = le64_to_cpu(head.tag);
		written = le32_to_cpu(head.written);
		wait_event(stream->sent_wait, READ_ONCE(stream->sending) != tag ||
						      READ_ONCE(stream->error) || READ_ONCE(stream->broken));
		/* Lost as it is sent: its sender sends it again, or fails it. */
		if (READ_ONCE(stream->sending) == tag) {
			err = -EPIPE;
			break;
		}
		req = tag <= U32_MAX ? xa_erase(&stream->in_flight, tag) : NULL;
		if (!req) {
			dev_err(&vext->vdev->dev, "the stream's reply to request %llu, not in flight\n",
				tag);
			err = -EPROTO;
			break;
		}
		if (!written) {
			vext_stream_complete(vext, req, -EIO);
			continue;
		}
		want = req->in_len + sizeof(req->resp);
		if (written != want) {
			dev_err(&vext->vdev->dev,
				"the stream's reply to request %llu of %u bytes, not its %u\n", tag,
				written, want);
			vext_stream_complete(vext, req, -EIO);
			err = -EPROTO;
			break;
		}
		n = 0;
		if (req->in_len)
			vec[n++] = (struct kvec) { req->data, req->in_len };
		vec[n++] = (struct kvec) { &req->resp, sizeof(req->resp) };
		err = vext_stream_io(stream->sock, vec, n, want, false);
		if (err) {
			/* Lost within its reply: in flight again, sent again, or failed if it can't be. */
			if (xa_err(xa_store(&stream->in_flight, tag, req, GFP_KERNEL)))
				vext_stream_complete(vext, req, -EIO);
			break;
		}
		atomic64_inc(&stream->replies);
		WRITE_ONCE(stream->retries, 0);
		vext_stream_complete(vext, req, -(int)le32_to_cpu(req->resp.status));
	}
	if (READ_ONCE(stream->error))
		return 0;
	if (err == -EPROTO) {
		vext_stream_fail(vext, err);
		return 0;
	}
	dev_info(&vext->vdev->dev, "the stream's connection lost reading (%d): connecting again\n",
		 err);
	vext_stream_break(vext);
	return 0;
}

/*
 * cladefs: connects the stream to the host's port `stream_port`, its socket's buffers
 * VEXT_STREAM_BUFFER, and starts its receiver: 0, or a -errno, when the next request tries again.
 */
static int vext_stream_connect(struct virtio_extent *vext)
{
	struct vext_stream *stream = &vext->stream;
	struct sockaddr_vm addr = {
		.svm_family = AF_VSOCK,
		.svm_cid = VMADDR_CID_HOST,
		.svm_port = vext->config.stream_port,
	};
	u64 size = VEXT_STREAM_BUFFER;
	struct task_struct *receiver;
	struct socket *sock;
	int err;

	err = sock_create_kern(&init_net, AF_VSOCK, SOCK_STREAM, 0, &sock);
	if (err)
		return err;
	/* The most first, which bounds the size. */
	err = sock->ops->setsockopt(sock, AF_VSOCK, SO_VM_SOCKETS_BUFFER_MAX_SIZE,
				    KERNEL_SOCKPTR(&size), sizeof(size));
	if (!err)
		err = sock->ops->setsockopt(sock, AF_VSOCK, SO_VM_SOCKETS_BUFFER_SIZE,
					    KERNEL_SOCKPTR(&size), sizeof(size));
	if (!err)
		err = kernel_connect(sock, (struct sockaddr *)&addr, sizeof(addr), 0);
	if (err)
		goto release;
	mutex_lock(&stream->sock_lock);
	spin_lock_irq(&stream->lock);
	err = stream->error;
	if (!err)
		stream->sock = sock;
	spin_unlock_irq(&stream->lock);
	mutex_unlock(&stream->sock_lock);
	if (err)
		goto release;
	receiver = kthread_create(vext_stream_receiver, vext, "vext-recv/%s", vext->config.tag);
	if (IS_ERR(receiver)) {
		err = PTR_ERR(receiver);
		vext_stream_fail(vext, err);
		return err;
	}
	/* Held before it runs, for its stop, which it may reach itself (vext_stream_close). */
	get_task_struct(receiver);
	stream->receiver = receiver;
	wake_up_process(receiver);
	dev_info(&vext->vdev->dev, "data requests over vsock to the host's port %u\n",
		 vext->config.stream_port);
	return 0;
release:
	sock_release(sock);
	return err;
}

/*
 * cladefs: sends `req` on the stream, whole, connecting it first if it isn't: or completes it with
 * the error that stopped it, the stream failing if its send did.
 */
/*
 * cladefs: sends `req`, in flight as `tag`, on the stream's connection, whole: its connection lost
 * if it can't be, `req` left in flight, which the sender sends again once connected again. Its
 * caller set `sending` to `tag` before it read anything of `req`: a host's bug that replied to it
 * first completes it only once it is sent.
 */
static void vext_stream_transmit(struct virtio_extent *vext, struct vext_request *req, u32 tag)
{
	struct vext_stream *stream = &vext->stream;
	int err;

	req->stream_hdr.tag = cpu_to_le64(tag);
	err = vext_stream_put(stream->sock, req, false);
	if (err) {
		if (!READ_ONCE(stream->error))
			dev_info(&vext->vdev->dev,
				 "the stream's connection lost sending (%d): connecting again\n", err);
		vext_stream_break(vext);
		smp_store_release(&stream->sending, U64_MAX);
		wake_up(&stream->sent_wait);
		return;
	}
	/* Counted before its reply can be: the receiver waits for the send. */
	atomic64_inc(&stream->sent);
	smp_store_release(&stream->sending, U64_MAX);
	if (wq_has_sleeper(&stream->sent_wait))
		wake_up(&stream->sent_wait);
}

/*
 * cladefs: puts `req` in flight and sends it, connecting the stream first if it never was: or
 * completes it with the error that stopped it. Its connection lost, it stays in flight, unsent,
 * and is sent once the stream connects again.
 */
static void vext_stream_send(struct virtio_extent *vext, struct vext_request *req)
{
	struct vext_stream *stream = &vext->stream;
	u32 tag;
	int err;

	err = READ_ONCE(stream->error);
	if (!err && !stream->sock && !READ_ONCE(stream->broken))
		err = vext_stream_connect(vext);
	/* Its tag reserved, then marked sending, before it is in flight for the receiver to take. */
	if (!err)
		err = xa_alloc_cyclic(&stream->in_flight, &tag, NULL, xa_limit_32b, &stream->next_tag,
				      GFP_KERNEL);
	if (err >= 0) {
		WRITE_ONCE(stream->sending, tag);
		err = xa_err(xa_store(&stream->in_flight, tag, req, GFP_KERNEL));
		if (err)
			xa_erase(&stream->in_flight, tag);
	}
	if (err < 0) {
		smp_store_release(&stream->sending, U64_MAX);
		vext_stream_complete(vext, req, err == -ESHUTDOWN ? -ESHUTDOWN : -EIO);
		return;
	}
	if (READ_ONCE(stream->broken)) {
		smp_store_release(&stream->sending, U64_MAX);
		wake_up(&stream->sent_wait);
		return;
	}
	req->stream_progress = 0;
	vext_stream_transmit(vext, req, tag);
}

/*
 * cladefs: sends the rest of the frame a submitter began (`partial`), with `send_lock` held, before
 * anything else goes on the connection: `sending` its tag still.
 */
static void vext_stream_finish_partial(struct virtio_extent *vext)
{
	struct vext_stream *stream = &vext->stream;
	struct vext_request *req = stream->partial;

	if (!req)
		return;
	stream->partial = NULL;
	vext_stream_transmit(vext, req, le64_to_cpu(req->stream_hdr.tag));
}

/*
 * cladefs: a submitter sends its own request on the connection, without waiting, with `send_lock`
 * held, rather than waking the sender for it, which costs a request a wakeup: whether it did, sent
 * or begun, the rest the sender's (`partial`), or put in flight for the connection made again;
 * false if it couldn't, the connection not made, lost or busy, when the sender takes it.
 */
static bool vext_stream_send_now(struct virtio_extent *vext, struct vext_request *req)
{
	struct vext_stream *stream = &vext->stream;
	unsigned int noio;
	u32 tag;
	int err;

	if (!stream->sock || stream->partial || READ_ONCE(stream->broken) ||
	    READ_ONCE(stream->error))
		return false;
	/*
	 * Its allocations, its tag's and the socket's buffers', reclaim nothing that waits on requests
	 * of the device's: its submitter may be writing back, holding what such reclaim would wait for.
	 */
	noio = memalloc_noio_save();
	err = xa_alloc_cyclic(&stream->in_flight, &tag, NULL, xa_limit_32b, &stream->next_tag,
			      GFP_KERNEL);
	if (err >= 0) {
		WRITE_ONCE(stream->sending, tag);
		err = xa_err(xa_store(&stream->in_flight, tag, req, GFP_KERNEL));
		if (err) {
			xa_erase(&stream->in_flight, tag);
			smp_store_release(&stream->sending, U64_MAX);
		}
	}
	if (err < 0) {
		memalloc_noio_restore(noio);
		return false;
	}
	req->stream_hdr.tag = cpu_to_le64(tag);
	req->stream_progress = 0;
	err = vext_stream_put(stream->sock, req, true);
	memalloc_noio_restore(noio);
	if (err == -EAGAIN) {
		/* The rest the sender's, before anything else, `sending` its tag still. */
		stream->partial = req;
		wake_up(&stream->wake);
		return true;
	}
	if (err) {
		if (!READ_ONCE(stream->error))
			dev_info(&vext->vdev->dev,
				 "the stream's connection lost sending (%d): connecting again\n", err);
		vext_stream_break(vext);
	} else {
		atomic64_inc(&stream->sent);
	}
	smp_store_release(&stream->sending, U64_MAX);
	if (wq_has_sleeper(&stream->sent_wait))
		wake_up(&stream->sent_wait);
	return true;
}

/*
 * cladefs: connects the stream again, its connection lost: its old receiver stopped and socket let
 * go of, a new connection made, and every request in flight sent again on it. 0, or the -errno it
 * failed with, past VEXT_STREAM_RETRIES lost in a row, or unable to connect.
 */
static int vext_stream_reconnect(struct virtio_extent *vext)
{
	struct vext_stream *stream = &vext->stream;
	struct task_struct *receiver = stream->receiver;
	struct socket *sock = stream->sock;
	struct vext_request *req;
	unsigned long index;
	int err;

	unsigned int tries;

	if (receiver) {
		kthread_stop(receiver);
		put_task_struct(receiver);
		stream->receiver = NULL;
	}
	mutex_lock(&stream->sock_lock);
	sock = stream->sock;
	stream->sock = NULL;
	spin_lock_irq(&stream->lock);
	stream->broken = false;
	spin_unlock_irq(&stream->lock);
	mutex_unlock(&stream->sock_lock);
	if (sock)
		sock_release(sock);
	/* A frame begun on the lost connection is in flight still, and sent again whole below. */
	stream->partial = NULL;
	if (++stream->retries > VEXT_STREAM_RETRIES) {
		dev_err(&vext->vdev->dev, "the stream's connection lost %u times in a row\n",
			stream->retries - 1);
		vext_stream_fail(vext, -EIO);
		return -EIO;
	}
	/*
	 * Tried a while: a vsock device resetting its connections as its VM is restored refuses one
	 * until its guest has heard of the reset.
	 */
	for (tries = 1;; tries++) {
		err = vext_stream_connect(vext);
		if (!err || tries == VEXT_STREAM_CONNECT_TRIES || READ_ONCE(stream->error) ||
		    kthread_should_stop())
			break;
		msleep(VEXT_STREAM_CONNECT_WAIT_MS);
	}
	if (err) {
		dev_err(&vext->vdev->dev, "connecting the stream again, %u times: %d\n", tries, err);
		vext_stream_fail(vext, -EIO);
		return err;
	}
	/* Nothing in flight to lose again: a connection lost again later is not this one's. */
	if (xa_empty(&stream->in_flight))
		stream->retries = 0;
	/*
	 * Each whole, the new receiver completing them as their replies come: each marked sending
	 * before it is read, then read again, the receiver having completed none of them unsent.
	 */
	xa_for_each(&stream->in_flight, index, req) {
		if (READ_ONCE(stream->broken) || READ_ONCE(stream->error))
			break;
		WRITE_ONCE(stream->sending, index);
		req = xa_load(&stream->in_flight, index);
		if (!req) {
			smp_store_release(&stream->sending, U64_MAX);
			continue;
		}
		req->stream_progress = 0;
		vext_stream_transmit(vext, req, index);
	}
	return 0;
}

/*
 * cladefs: the stream's sender: sends the requests its submitters queue, in order, until it is
 * stopped; and, the stream failed, fails every request it had, then any queued after.
 */
static int vext_stream_sender(void *data)
{
	struct virtio_extent *vext = data;
	struct vext_stream *stream = &vext->stream;
	struct vext_request *req, *next;
	LIST_HEAD(batch);

	/* Its allocations reclaim nothing that waits on requests of the device's. */
	memalloc_noio_save();
	for (;;) {
		wait_event_idle(stream->wake, kthread_should_stop() || READ_ONCE(stream->error) ||
						      READ_ONCE(stream->broken) ||
						      READ_ONCE(stream->partial) ||
						      !list_empty_careful(&stream->to_send));
		if (READ_ONCE(stream->error) || kthread_should_stop())
			break;
		mutex_lock(&stream->send_lock);
		if (READ_ONCE(stream->broken) && vext_stream_reconnect(vext)) {
			mutex_unlock(&stream->send_lock);
			break;
		}
		vext_stream_finish_partial(vext);
		mutex_unlock(&stream->send_lock);
		spin_lock_irq(&stream->lock);
		list_splice_init(&stream->to_send, &batch);
		spin_unlock_irq(&stream->lock);
		list_for_each_entry_safe(req, next, &batch, stream_node) {
			list_del(&req->stream_node);
			/* A submitter's frame begun meanwhile goes first. */
			mutex_lock(&stream->send_lock);
			vext_stream_finish_partial(vext);
			vext_stream_send(vext, req);
			mutex_unlock(&stream->send_lock);
		}
	}
	/* Once no submitter sends: one sending finds the stream failed after. */
	mutex_lock(&stream->send_lock);
	stream->partial = NULL;
	vext_stream_fail_all(vext);
	mutex_unlock(&stream->send_lock);
	/* None is queued past its failure (vext_stream_submit): only its stop is left. */
	wait_event_idle(stream->wake, kthread_should_stop());
	return 0;
}

/*
 * cladefs: queues `req`, a data request whose arguments, data and root are `args_len`,
 * `data_len` and `root_len` bytes, for the stream's sender: 0, or the stream's -errno, once it
 * failed, when `done` isn't called.
 */
static int vext_stream_submit(struct virtio_extent *vext, struct vext_request *req,
			      const struct vext_type *type, u32 args_len, u32 data_len, u32 root_len)
{
	struct vext_stream *stream = &vext->stream;
	unsigned long flags;
	bool now;
	int err;

	req->args_len = args_len;
	req->out_len = type->data == VEXT_OUT ? data_len : 0;
	req->in_len = type->data == VEXT_IN ? data_len : 0;
	req->root_out = root_len;
	req->stream_hdr = (struct virtio_extent_stream_req) {
		.readable = cpu_to_le32(sizeof(req->hdr) + args_len + req->out_len + root_len),
		.writable = cpu_to_le32(req->in_len + sizeof(req->resp)),
	};
	req->queue = NULL;
	req->queued_ns = ktime_get_ns();
	req->cpu = raw_smp_processor_id();
	spin_lock_irqsave(&stream->lock, flags);
	err = stream->error;
	if (!err) {
		atomic64_inc(&vext->requests[req->type]);
		vext_busy_begin(vext);
	}
	now = !err && list_empty(&stream->to_send);
	spin_unlock_irqrestore(&stream->lock, flags);
	if (err)
		return err == -ESHUTDOWN ? -ESHUTDOWN : -EIO;
	/*
	 * Sent here, with none queued before it and no one sending, without waiting: the sender
	 * woken for none of it (board 14663's A/B: a wakeup each fsync waits on).
	 */
	if (now && mutex_trylock(&stream->send_lock)) {
		bool sent = vext_stream_send_now(vext, req);

		mutex_unlock(&stream->send_lock);
		if (sent)
			return 0;
	}
	spin_lock_irqsave(&stream->lock, flags);
	list_add_tail(&req->stream_node, &stream->to_send);
	spin_unlock_irqrestore(&stream->lock, flags);
	wake_up(&stream->wake);
	return 0;
}

/* cladefs: starts the stream's sender, if the device has a stream: 0, or a -errno. */
static int vext_stream_start(struct virtio_extent *vext)
{
	struct vext_stream *stream = &vext->stream;
	struct task_struct *sender;

	spin_lock_init(&stream->lock);
	mutex_init(&stream->sock_lock);
	mutex_init(&stream->send_lock);
	INIT_LIST_HEAD(&stream->to_send);
	init_waitqueue_head(&stream->wake);
	init_waitqueue_head(&stream->sent_wait);
	stream->sending = U64_MAX;
	xa_init_flags(&stream->in_flight, XA_FLAGS_ALLOC);
	if (!virtio_has_feature(vext->vdev, VIRTIO_EXTENT_F_STREAM))
		return 0;
	sender = kthread_create(vext_stream_sender, vext, "vext-send/%s", vext->config.tag);
	if (IS_ERR(sender))
		return PTR_ERR(sender);
	/* Held before it runs, for its stop (vext_stream_close). */
	get_task_struct(sender);
	stream->sender = sender;
	wake_up_process(sender);
	return 0;
}

/*
 * cladefs: closes the stream, as the device resets: every request it had failed, and its threads
 * stopped, its socket let go of.
 */
static void vext_stream_close(struct virtio_extent *vext)
{
	struct vext_stream *stream = &vext->stream;

	if (!stream->sender)
		return;
	vext_stream_fail(vext, -ESHUTDOWN);
	kthread_stop(stream->sender);
	put_task_struct(stream->sender);
	stream->sender = NULL;
	if (stream->receiver) {
		kthread_stop(stream->receiver);
		put_task_struct(stream->receiver);
		stream->receiver = NULL;
	}
	/*
	 * What its receiver put back in flight as it ended, its sender gone: failed here. Once no
	 * submitter sends, which finds the stream failed after.
	 */
	mutex_lock(&stream->send_lock);
	stream->partial = NULL;
	vext_stream_fail_all(vext);
	if (stream->sock) {
		sock_release(stream->sock);
		stream->sock = NULL;
	}
	mutex_unlock(&stream->send_lock);
	xa_destroy(&stream->in_flight);
}

/*
 * cladefs: the most scatterlists a request has besides its data's pages: its header, arguments,
 * root and response, a descriptor each (virtio_extent_submit). A queue's ring holds them and
 * max_segments data pages (vext_check_rings). Before, 3, the root uncounted: a WRITE with its root
 * over max_segments unaligned pages needed a descriptor more than the ring has, which
 * virtqueue_add_sgs refused for good, and its writer waited for room that never came (board 10314).
 */
#define VEXT_FIXED_SGS 4

int virtio_extent_submit(struct virtio_extent *vext, struct vext_request *req)
{
	const struct vext_type *type = vext_type(req);
	struct scatterlist hdr, args, root, resp, *sgs[VEXT_FIXED_SGS + 1];
	unsigned int out_sgs = 0, in_sgs = 0, descs;
	u32 args_len, data_len, root_len;
	struct vext_queue *queue;
	unsigned long flags;
	int err;

	if (!type)
		return -EINVAL;
	data_len = type->data && vext_has(req, type->data_if) ? req->data_len : 0;
	root_len = type->root_if && req->flags & type->root_if ? req->root_len : 0;
	if (data_len && !req->data)
		return -EINVAL;
	/* The root is in one piece: kmalloc'd. */
	if (root_len && (!req->root || is_vmalloc_addr(req->root)))
		return -EINVAL;
	args_len = type->args ? type->args(req, data_len, root_len) : 0;
	req->hdr = (struct virtio_extent_req) {
		.type = cpu_to_le32(req->type),
		.flags = cpu_to_le32(req->flags),
		.unique = cpu_to_le64(atomic64_inc_return(&vext->unique)),
	};
	req->resp.status = 0;
	memset(&req->data_sgt, 0, sizeof(req->data_sgt));
	/* cladefs: a data request of a device with a stream goes on it (board 14663). */
	if (vext_on_stream(vext, req))
		return vext_stream_submit(vext, req, type, args_len, data_len, root_len);
	if (data_len) {
		err = vext_map_data(&req->data_sgt, req->data, data_len);
		if (err)
			return err;
	}

	/* What the device reads: the header, arguments, data and root; then what it writes. */
	sg_init_one(&hdr, &req->hdr, sizeof(req->hdr));
	sgs[out_sgs++] = &hdr;
	if (args_len) {
		sg_init_one(&args, &req->args, args_len);
		sgs[out_sgs++] = &args;
	}
	if (data_len && type->data == VEXT_OUT)
		sgs[out_sgs++] = req->data_sgt.sgl;
	if (root_len) {
		sg_init_one(&root, req->root, root_len);
		sgs[out_sgs++] = &root;
	}
	if (data_len && type->data == VEXT_IN)
		sgs[out_sgs + in_sgs++] = req->data_sgt.sgl;
	sg_init_one(&resp, &req->resp, sizeof(req->resp));
	sgs[out_sgs + in_sgs++] = &resp;

	/* A descriptor per scatterlist entry: the queue has no indirect ones. */
	descs = out_sgs + in_sgs + (req->data_sgt.nents ? req->data_sgt.nents - 1 : 0);
	if (req->data_sgt.nents > vext->config.max_segments) {
		sg_free_table(&req->data_sgt);
		return -EINVAL;
	}

	/*
	 * The queue of the CPU it runs on, whose interrupt completes it there: if it moves meanwhile,
	 * another's, which is as good.
	 */
	queue = &vext->queues[READ_ONCE(vext->cpu_queue[raw_smp_processor_id()])];
	for (;;) {
		req->queued_ns = ktime_get_ns();
		req->cpu = raw_smp_processor_id();
		spin_lock_irqsave(&queue->lock, flags);
		/*
		 * cladefs: closed, as the device resets, it has no queues, or will not once this is
		 * added: looked at once, without the lock, before, a request added after
		 * went to a queue let go of, or that a reset device never completed (TASK-201).
		 */
		err = queue->closed ? -ESHUTDOWN :
			virtqueue_add_sgs(queue->vq, sgs, out_sgs, in_sgs, req, GFP_ATOMIC);
		if (!err) {
			bool notify = virtqueue_kick_prepare(queue->vq);

			atomic64_inc(&vext->requests[req->type]);
			atomic64_inc(&queue->requests);
			/* Under the queue's lock, which reaps it: before it can end. */
			vext_busy_begin(vext);
			req->queue = queue;
			spin_unlock_irqrestore(&queue->lock, flags);
			if (notify)
				virtqueue_notify(queue->vq);
			return 0;
		}
		spin_unlock_irqrestore(&queue->lock, flags);
		if (err != -ENOSPC) {
			sg_free_table(&req->data_sgt);
			return err;
		}
		/* The queue is full: wait for the device to complete a request, or the queue to close. */
		wait_event(queue->room,
			   READ_ONCE(queue->closed) || READ_ONCE(queue->vq->num_free) >= descs);
	}
}
EXPORT_SYMBOL_GPL(virtio_extent_submit);

static void vext_call_done(struct vext_request *req)
{
	complete(req->private);
}

/*
 * Completes the requests the device has done of `queue`, with its lock held: how many there were.
 * The time each took goes to its type's average (vext_wait).
 */
static unsigned int vext_reap(struct virtio_extent *vext, struct vext_queue *queue)
{
	struct vext_request *req;
	unsigned int len, reaped = 0;
	u64 took, *recent;

	while ((req = virtqueue_get_buf(queue->vq, &len))) {
		took = ktime_get_ns() - req->queued_ns;
		atomic64_add(took, &vext->request_ns[req->type]);
		/* An eighth of the latest, as TCP's smoothed round trip time. */
		recent = &vext->recent_ns[req->type];
		WRITE_ONCE(*recent, READ_ONCE(*recent) - (READ_ONCE(*recent) >> 3) + (took >> 3));
		req->status = -(int)le32_to_cpu(req->resp.status);
		if (req->cpu != raw_smp_processor_id())
			atomic64_inc(&queue->cross);
		vext_busy_end(vext);
		sg_free_table(&req->data_sgt);
		req->done(req);
		reaped++;
	}
	return reaped;
}

/*
 * Has the device signal `queue`'s completions again, unless callers poll it, completing those it
 * did meanwhile: with its lock held. How many there were.
 */
static unsigned int vext_signal(struct virtio_extent *vext, struct vext_queue *queue)
{
	unsigned int reaped = 0;

	while (!queue->pollers && !virtqueue_enable_cb(queue->vq)) {
		virtqueue_disable_cb(queue->vq);
		reaped += vext_reap(vext, queue);
	}
	return reaped;
}

/*
 * How long a caller waits for its request on the CPU before sleeping: a request's reply comes
 * back in tens to hundreds of microseconds, and sleeping costs halting the vCPU, which in a nested
 * VM exits to the outer host, and an interrupt to wake it. cladefs: only for requests of a type
 * that recently took less (TASK-91.13): MAPs taking 260-490 us spun their 100 us for nothing,
 * a quarter to half of the guest's time.
 */
#define VEXT_SPIN_NS (100 * NSEC_PER_USEC)
/*
 * cladefs: how long a caller spins, and how recent requests must have taken for it to: nested
 * virtualization takes a device's requests past VEXT_SPIN_NS, so none spins, and each pays a halt
 * and a wakeup (TASK-146.1).
 */
static unsigned long vext_spin_ns = VEXT_SPIN_NS;
module_param_named(spin_ns, vext_spin_ns, ulong, 0644);
MODULE_PARM_DESC(spin_ns, "How long a caller polls for its request before it sleeps, in ns");

/*
 * Waits for `req`, whose completion is `done`: on the CPU first, if requests of its type
 * recently took less than vext_spin_ns, and no other task wants the CPU. cladefs: polling its
 * queue meanwhile, of which the device signals no completions: each interrupt costs exits to
 * read and acknowledge it, which a request done in time saves (TASK-91.15).
 */
static void vext_wait(struct virtio_extent *vext, struct vext_request *req, struct completion *done)
{
	struct vext_queue *queue = req->queue;
	u64 spin_ns = READ_ONCE(vext_spin_ns);
	u64 deadline = local_clock() + spin_ns;
	unsigned int spins = 0, reaped;
	unsigned long flags;
	bool completed;

	/* cladefs: on the stream, its receiver completes it: no queue to poll (board 14663). */
	if (!queue || READ_ONCE(vext->recent_ns[req->type]) >= spin_ns) {
		wait_for_completion(done);
		return;
	}
	/* Not preempted while the device signals no completions of the queue, which it polls. */
	preempt_disable();
	spin_lock_irqsave(&queue->lock, flags);
	if (!queue->pollers++)
		virtqueue_disable_cb(queue->vq);
	spin_unlock_irqrestore(&queue->lock, flags);
	/* Taken here, the completion isn't there for wait_for_completion. */
	while (!(completed = try_wait_for_completion(done))) {
		/* The clock, a paravirtual one, is costly to read: every 64 spins. */
		if (need_resched() || (++spins % 64 == 0 && local_clock() > deadline))
			break;
		/* The used ring, in the guest's memory, every 16. */
		if (spins % 16 == 0 && spin_trylock_irqsave(&queue->lock, flags)) {
			reaped = vext_reap(vext, queue);
			spin_unlock_irqrestore(&queue->lock, flags);
			if (reaped) {
				atomic64_add(reaped, &queue->polled);
				wake_up_all(&queue->room);
			}
		}
		cpu_relax();
	}
	spin_lock_irqsave(&queue->lock, flags);
	queue->pollers--;
	reaped = vext_signal(vext, queue);
	spin_unlock_irqrestore(&queue->lock, flags);
	preempt_enable();
	if (reaped) {
		atomic64_add(reaped, &queue->polled);
		wake_up_all(&queue->room);
	}
	if (!completed)
		wait_for_completion(done);
}

int virtio_extent_call(struct virtio_extent *vext, struct vext_request *req)
{
	DECLARE_COMPLETION_ONSTACK(done);
	int err;

	req->done = vext_call_done;
	req->private = &done;
	err = virtio_extent_submit(vext, req);
	if (err)
		return err;
	vext_wait(vext, req, &done);
	return req->status;
}
EXPORT_SYMBOL_GPL(virtio_extent_call);

/*
 * A queue's interrupt: completes the requests the device has of it. cladefs: and has the device
 * signal it again, unless callers poll it (vext_wait).
 */
static void vext_done(struct virtqueue *vq)
{
	struct virtio_extent *vext = vq->vdev->priv;
	struct vext_queue *queue = &vext->queues[vq->index];
	unsigned long flags;

	atomic64_inc(&queue->interrupts);
	spin_lock_irqsave(&queue->lock, flags);
	virtqueue_disable_cb(vq);
	vext_reap(vext, queue);
	vext_signal(vext, queue);
	spin_unlock_irqrestore(&queue->lock, flags);
	wake_up_all(&queue->room);
}

/*
 * cladefs: whether the queues' interrupts are spread over the CPUs, each queue's affine to the CPUs
 * whose requests go to it (vext_map_queues), and how many queues the driver uses at most, 0 for
 * as many as the device has: what an A/B of the queues' parallelism turns off, interleaved in one
 * build (TASK-276). Read as a device's queues are found.
 */
static bool vext_affine = true;
module_param_named(affine, vext_affine, bool, 0444);
MODULE_PARM_DESC(affine, "Whether each CPU's requests go to a queue whose interrupt is affine to it");
static unsigned int vext_max_queues;
module_param_named(max_queues, vext_max_queues, uint, 0444);
MODULE_PARM_DESC(max_queues, "The most request queues the driver uses, 0 for the device's all");

/*
 * cladefs: for tests (board 8557): `FIELD=N` has the driver take FIELD of the device's
 * configuration, or the size of its queues' rings (`ring`), as N, whatever the device says: what a
 * VMM's bug would offer, which the driver refuses (vext_read_config, vext_check_rings).
 */
enum vext_fault_field {
	VEXT_FAULT_NONE,
	VEXT_FAULT_BLOCK_SIZE,
	VEXT_FAULT_MAP_ALIGNMENT,
	VEXT_FAULT_MAX_TRANSFER,
	VEXT_FAULT_MAX_SEGMENTS,
	VEXT_FAULT_RING,
};

static const char *const vext_fault_fields[] = {
	[VEXT_FAULT_BLOCK_SIZE] = "block_size",
	[VEXT_FAULT_MAP_ALIGNMENT] = "map_alignment",
	[VEXT_FAULT_MAX_TRANSFER] = "max_transfer",
	[VEXT_FAULT_MAX_SEGMENTS] = "max_segments",
	[VEXT_FAULT_RING] = "ring",
};

static struct {
	enum vext_fault_field field;
	u32 value;
} vext_config_fault;

/* `FIELD=N`, FIELD one of vext_fault_fields: anything else is refused, as a typo would be. */
static int vext_config_fault_set(const char *val, const struct kernel_param *kp)
{
	const char *equals = strchr(val, '=');
	enum vext_fault_field field;
	size_t len;
	u32 value;
	int err;

	if (!equals)
		return -EINVAL;
	len = equals - val;
	err = kstrtou32(equals + 1, 0, &value);
	if (err)
		return err;
	for (field = VEXT_FAULT_BLOCK_SIZE; field < ARRAY_SIZE(vext_fault_fields); field++) {
		if (strlen(vext_fault_fields[field]) == len &&
		    !strncmp(val, vext_fault_fields[field], len)) {
			vext_config_fault.field = field;
			vext_config_fault.value = value;
			return 0;
		}
	}
	return -EINVAL;
}

static const struct kernel_param_ops vext_config_fault_ops = { .set = vext_config_fault_set };
/* Set on the kernel's command line alone: no file in sysfs. */
module_param_cb(config_fault, &vext_config_fault_ops, NULL, 0);
MODULE_PARM_DESC(config_fault, "`FIELD=N`: take the device's FIELD (block_size, map_alignment, max_transfer, max_segments, ring) as N: for tests alone");

/* `value`, what the device said of `field`, or what virtio_extent.config_fault says instead. */
static u32 vext_faulted(enum vext_fault_field field, u32 value)
{
	return vext_config_fault.field == field ? vext_config_fault.value : value;
}

/*
 * cladefs: the fewest data segments a request of one block takes: memory not aligned to pages
 * spans one more page than its length needs (cladefs_device_find's max_io).
 */
static u32 vext_min_segments(const struct vext_config *config)
{
	return 1 + DIV_ROUND_UP(config->block_size, PAGE_SIZE);
}

/*
 * cladefs: 0 if each of the `n` queues' rings holds a request of max_segments data pages and
 * VEXT_FIXED_SGS more, max_segments lowered to fit them when the device is probed (`probing`) but
 * never as its queues are found again, the filesystem's requests sized by it. Else -EINVAL,
 * logged: with a ring too small for a request of one block, a request would wait for room that
 * never comes (board 8557), and before, the ring's size less 3 wrapped below 3 entries.
 */
static int vext_check_rings(struct virtio_extent *vext, struct virtqueue **vqs, unsigned int n,
			    bool probing)
{
	struct vext_config *config = &vext->config;
	u32 ring = U32_MAX, fits, needs;
	unsigned int i;

	for (i = 0; i < n; i++)
		ring = min(ring, virtqueue_get_vring_size(vqs[i]));
	ring = vext_faulted(VEXT_FAULT_RING, ring);
	fits = ring > VEXT_FIXED_SGS ? ring - VEXT_FIXED_SGS : 0;
	needs = probing ? vext_min_segments(config) : config->max_segments;
	if (fits < needs) {
		dev_err(&vext->vdev->dev,
			"ring: the request queues' rings hold %u descriptors, fewer than a request of %u data segments takes, %u: refused\n",
			ring, needs, needs + VEXT_FIXED_SGS);
		return -EINVAL;
	}
	config->max_segments = min(config->max_segments, fits);
	return 0;
}

/*
 * cladefs: maps each CPU to the queue whose interrupt the transport made affine to it, as
 * blk_mq_virtio_map_queues does (TASK-276): a request then completes on the CPU that queued it,
 * whose caller is woken there, rather than by an interrupt between CPUs, and finds its request in
 * its cache. Where the transport says no affinity of some queue (vectors shared, or none), the CPUs
 * go round the queues, as they did before.
 */
static void vext_map_queues(struct virtio_extent *vext)
{
	struct virtio_device *vdev = vext->vdev;
	const struct cpumask *mask;
	unsigned int i, cpu;

	for (i = 0; vext_affine && vdev->config->get_vq_affinity && i < vext->nr_queues; i++) {
		if (!vdev->config->get_vq_affinity(vdev, i))
			break;
	}
	if (!vext_affine || !vdev->config->get_vq_affinity || i < vext->nr_queues) {
		for_each_possible_cpu(cpu)
			WRITE_ONCE(vext->cpu_queue[cpu], cpu % vext->nr_queues);
		return;
	}
	for (i = 0; i < vext->nr_queues; i++) {
		mask = vdev->config->get_vq_affinity(vdev, i);
		for_each_cpu(cpu, mask)
			WRITE_ONCE(vext->cpu_queue[cpu], i);
	}
}

/*
 * Finds the device's request queues, one for each CPU, up to as many as it has: one, if it says
 * none (num_request_queues is 1 at least). cladefs: their interrupts spread over the CPUs, each
 * queue's affine to those its requests come from (vext_map_queues), the configuration's apart, as
 * virtio_blk has its queues' (TASK-276).
 */
static int vext_find_queues(struct virtio_extent *vext)
{
	struct virtio_device *vdev = vext->vdev;
	/* The transport counts the configuration's vector among those before the queues'. */
	struct irq_affinity affinity = { 0 };
	struct virtqueue_info *infos;
	struct virtqueue **vqs;
	unsigned int i, offered;
	bool probing = !vext->queues;
	int err;

	virtio_cread_le(vdev, struct virtio_extent_config, num_request_queues, &offered);
	vext->nr_queues = clamp(offered, 1U, nr_cpu_ids);
	if (vext_max_queues)
		vext->nr_queues = min(vext->nr_queues, vext_max_queues);
	vext->queues = devm_kcalloc(&vdev->dev, vext->nr_queues, sizeof(*vext->queues), GFP_KERNEL);
	if (!vext->queues)
		return -ENOMEM;
	for (i = 0; i < vext->nr_queues; i++) {
		spin_lock_init(&vext->queues[i].lock);
		init_waitqueue_head(&vext->queues[i].room);
	}
	/* Every CPU's, the first queue's until they are found: none is found unmapped. */
	vext->cpu_queue = devm_kcalloc(&vdev->dev, nr_cpu_ids, sizeof(*vext->cpu_queue), GFP_KERNEL);
	if (!vext->cpu_queue)
		return -ENOMEM;
	infos = kcalloc(vext->nr_queues, sizeof(*infos), GFP_KERNEL);
	vqs = kcalloc(vext->nr_queues, sizeof(*vqs), GFP_KERNEL);
	err = -ENOMEM;
	if (!infos || !vqs)
		goto out;
	for (i = 0; i < vext->nr_queues; i++)
		infos[i] = (struct virtqueue_info){ "requests", vext_done };
	err = virtio_find_vqs(vdev, vext->nr_queues, vqs, infos, vext_affine ? &affinity : NULL);
	if (err)
		goto out;
	/* Before any is open to requests: one that wouldn't fit them is let go of unused. */
	err = vext_check_rings(vext, vqs, vext->nr_queues, probing);
	if (err) {
		virtio_reset_device(vdev);
		vdev->config->del_vqs(vdev);
		goto out;
	}
	vext_map_queues(vext);
	for (i = 0; i < vext->nr_queues; i++) {
		struct vext_queue *queue = &vext->queues[i];

		spin_lock_irq(&queue->lock);
		queue->vq = vqs[i];
		spin_unlock_irq(&queue->lock);
	}
out:
	kfree(infos);
	kfree(vqs);
	return err;
}

/*
 * Shows `counters`, one per request type, each on a line of its own after the type's name. cladefs:
 * not the numbers no type has (vext_types).
 */
static ssize_t request_counters_show(char *buf, atomic64_t *counters)
{
	ssize_t len = 0;
	int type;

	for (type = 0; type <= VIRTIO_EXTENT_T_LAST; type++) {
		if (vext_types[type].name)
			len += sysfs_emit_at(buf, len, "%s %lld\n", vext_types[type].name,
					     atomic64_read(&counters[type]));
	}
	return len;
}

/* The `requests` attribute: how many requests of each type the device has taken. */
static ssize_t requests_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct virtio_extent *vext = dev_to_virtio(dev)->priv;

	return request_counters_show(buf, vext->requests);
}
static DEVICE_ATTR_RO(requests);

/* The `request_ns` attribute: how long the requests of each type completed took, all told. */
static ssize_t request_ns_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct virtio_extent *vext = dev_to_virtio(dev)->priv;

	return request_counters_show(buf, vext->request_ns);
}
static DEVICE_ATTR_RO(request_ns);

/*
 * cladefs: the `busy_ns` attribute: how long the device has had a request in flight, all told, the
 * one in flight so far among it: the guest's time waiting for the host (TASK-263).
 */
static ssize_t busy_ns_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct virtio_extent *vext = dev_to_virtio(dev)->priv;
	unsigned long flags;
	u64 busy;

	spin_lock_irqsave(&vext->busy_lock, flags);
	busy = vext->busy_ns + (vext->in_flight ? ktime_get_ns() - vext->busy_since : 0);
	spin_unlock_irqrestore(&vext->busy_lock, flags);
	return sysfs_emit(buf, "%llu\n", busy);
}
static DEVICE_ATTR_RO(busy_ns);

/* The `queues` attribute: how many requests each request queue has taken, a line each. */
static ssize_t queues_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct virtio_extent *vext = dev_to_virtio(dev)->priv;
	ssize_t len = 0;
	unsigned int i;

	for (i = 0; i < vext->nr_queues; i++)
		len += sysfs_emit_at(buf, len, "%lld\n", atomic64_read(&vext->queues[i].requests));
	return len;
}
static DEVICE_ATTR_RO(queues);

/*
 * cladefs: the requests its stream sent, and the replies it read, on a line: 0 0 without one
 * (F_STREAM, board 14663).
 */
static ssize_t stream_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct virtio_extent *vext = dev_to_virtio(dev)->priv;

	return sysfs_emit(buf, "%lld %lld\n", atomic64_read(&vext->stream.sent),
			  atomic64_read(&vext->stream.replies));
}
static DEVICE_ATTR_RO(stream);

/*
 * cladefs: the `completions` attribute: the queues' interrupts, and the requests their callers
 * completed polling them, without one (vext_wait), all told; and the requests completed on a CPU
 * other than their own (TASK-276).
 */
static ssize_t completions_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct virtio_extent *vext = dev_to_virtio(dev)->priv;
	s64 interrupts = 0, polled = 0, cross = 0;
	unsigned int i;

	for (i = 0; i < vext->nr_queues; i++) {
		interrupts += atomic64_read(&vext->queues[i].interrupts);
		polled += atomic64_read(&vext->queues[i].polled);
		cross += atomic64_read(&vext->queues[i].cross);
	}
	return sysfs_emit(buf, "interrupts %lld\npolled %lld\ncross %lld\n", interrupts, polled,
			  cross);
}
static DEVICE_ATTR_RO(completions);

/*
 * cladefs: the `window` attribute: the DAX window's length, 0 if the device has none, how much of it
 * has struct pages, and the time making them took (TASK-92.8).
 */
static ssize_t window_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct virtio_extent *vext = dev_to_virtio(dev)->priv;
	struct vext_window *window = &vext->window;

	return sysfs_emit(buf, "len %llu\nmade %lld\nmade_ns %lld\n", window->len,
			  atomic64_read(&window->made), atomic64_read(&window->made_ns));
}
static DEVICE_ATTR_RO(window);

static struct attribute *vext_attrs[] = {
	&dev_attr_requests.attr,
	&dev_attr_request_ns.attr,
	&dev_attr_busy_ns.attr,
	&dev_attr_queues.attr,
	&dev_attr_stream.attr,
	&dev_attr_completions.attr,
	&dev_attr_window.attr,
	NULL,
};

static const struct attribute_group vext_attr_group = { .attrs = vext_attrs };

/*
 * cladefs: the protocol version this driver speaks (TASK-141.3): VIRTIO_EXTENT_VERSION. Another,
 * set for a test, has it refuse the device as it would a device of another version.
 */
static uint vext_version = VIRTIO_EXTENT_VERSION;
module_param_named(version, vext_version, uint, 0444);
MODULE_PARM_DESC(version, "The device protocol version this driver speaks: for tests alone");

/*
 * cladefs: 0 if the device is of the protocol version this driver speaks (TASK-141.3); otherwise
 * -EPROTONOSUPPORT, both versions logged. Read first, as nothing else of the configuration may
 * mean, in another version, what it does in this one.
 */
static int vext_check_version(struct virtio_device *vdev)
{
	u32 version;

	virtio_cread_le(vdev, struct virtio_extent_config, version, &version);
	if (version == vext_version)
		return 0;
	dev_err(&vdev->dev,
		"the device is of protocol version %u, this driver of %u: refused, as a driver of another build is\n",
		version, vext_version);
	return -EPROTONOSUPPORT;
}

/* cladefs: -EINVAL, the device refused, its configuration's `field` named with why. */
static int vext_refuse(struct virtio_device *vdev, const char *field, u32 value, const char *why)
{
	dev_err(&vdev->dev, "%s: the device says %u, %s: refused\n", field, value, why);
	return -EINVAL;
}

/*
 * Reads the device's configuration. cladefs: refusing (-EINVAL, the field named) what the driver
 * or the filesystem would divide by zero by, round to nothing or wrap on: the host is trusted, but
 * a VMM's bug is to be told of, not oopsed on (board 8557). Before, a block size of 0 divided by
 * zero as the filesystem mounted (cladefs_device_find's rounddown), and a max_io of 0, of too few
 * segments or too small a transfer, divided by zero in its Rust (Device::read's div_ceil).
 */
static int vext_read_config(struct virtio_extent *vext)
{
	struct virtio_device *vdev = vext->vdev;
	struct vext_config *config = &vext->config;

	virtio_cread_bytes(vdev, offsetof(struct virtio_extent_config, tag), config->tag,
			   sizeof(config->tag) - 1);
	config->tag[sizeof(config->tag) - 1] = 0;
	virtio_cread_le(vdev, struct virtio_extent_config, block_size, &config->block_size);
	virtio_cread_le(vdev, struct virtio_extent_config, map_alignment, &config->map_alignment);
	virtio_cread_le(vdev, struct virtio_extent_config, max_transfer, &config->max_transfer);
	virtio_cread_le(vdev, struct virtio_extent_config, max_segments, &config->max_segments);
	virtio_cread_le(vdev, struct virtio_extent_config, max_root, &config->max_root);
	virtio_cread_le(vdev, struct virtio_extent_config, cell, &config->cell);
	virtio_cread_le(vdev, struct virtio_extent_config, max_extent, &config->max_extent);
	/* cladefs: set as the device opens, and never changed (doc-13). */
	config->private_max = 0;
	if (virtio_has_feature(vdev, VIRTIO_EXTENT_F_DAX))
		virtio_cread_le(vdev, struct virtio_extent_config, private_max, &config->private_max);
	/* cladefs: board 14663. */
	config->stream_port = 0;
	if (virtio_has_feature(vdev, VIRTIO_EXTENT_F_STREAM)) {
		virtio_cread_le(vdev, struct virtio_extent_config, stream_port, &config->stream_port);
		if (!config->stream_port || config->stream_port == VMADDR_PORT_ANY)
			return vext_refuse(vdev, "stream_port", config->stream_port,
					   "not a port to connect to");
	}

	config->block_size = vext_faulted(VEXT_FAULT_BLOCK_SIZE, config->block_size);
	config->map_alignment = vext_faulted(VEXT_FAULT_MAP_ALIGNMENT, config->map_alignment);
	config->max_transfer = vext_faulted(VEXT_FAULT_MAX_TRANSFER, config->max_transfer);
	config->max_segments = vext_faulted(VEXT_FAULT_MAX_SEGMENTS, config->max_segments);
	if (!is_power_of_2(config->block_size))
		return vext_refuse(vdev, "block_size", config->block_size, "not a power of two");
	/* What a MAP's length is aligned to (the self-test's ALIGN), and DAX's pages counted in. */
	if (!is_power_of_2(config->map_alignment))
		return vext_refuse(vdev, "map_alignment", config->map_alignment, "not a power of two");
	if (config->max_transfer < config->block_size)
		return vext_refuse(vdev, "max_transfer", config->max_transfer,
				   "less than a block");
	if (config->max_segments < vext_min_segments(config))
		return vext_refuse(vdev, "max_segments", config->max_segments,
				   "fewer than a block takes, unaligned");
	return 0;
}

#if IS_ENABLED(CONFIG_FS_DAX)
/* The window's part holding its byte at `offset`. */
static unsigned int vext_window_part(const struct vext_window *window, u64 offset)
{
	return ((window->phys + offset) >> PA_SECTION_SHIFT) - (window->phys >> PA_SECTION_SHIFT);
}

/* Makes the struct pages of the window's part `part`, with its lock held: 0, or a -errno. */
static int vext_window_make_part(struct vext_window *window, unsigned int part)
{
	struct dev_pagemap *pgmap = &window->parts[part];
	u64 start = ktime_get_ns();
	unsigned int nofs;
	void *kaddr;

	/* Its allocations reclaim nothing of a filesystem, whose locks the caller may hold. */
	nofs = memalloc_nofs_save();
	kaddr = memremap_pages(pgmap, window->nid);
	memalloc_nofs_restore(nofs);
	if (IS_ERR(kaddr))
		return PTR_ERR(kaddr);
	WARN_ON_ONCE(kaddr != window->kaddr + (pgmap->range.start - window->phys));
	atomic64_add(range_len(&pgmap->range), &window->made);
	atomic64_add(ktime_get_ns() - start, &window->made_ns);
	/* Pairs with vext_window_make's test_bit_acquire: a part seen made has its pages. */
	smp_mb__before_atomic();
	set_bit(part, window->made_parts);
	return 0;
}

/*
 * Makes the struct pages of the window's parts holding its `len` bytes from `offset`, if they
 * have none yet: 0, or a -errno. Sleeps, if it makes one.
 */
static int vext_window_make(struct vext_window *window, u64 offset, u64 len)
{
	unsigned int part = vext_window_part(window, offset);
	unsigned int last = vext_window_part(window, offset + len - 1);
	int err = 0;

	for (; part <= last && !err; part++) {
		if (test_bit_acquire(part, window->made_parts))
			continue;
		mutex_lock(&window->lock);
		if (!test_bit(part, window->made_parts))
			err = vext_window_make_part(window, part);
		mutex_unlock(&window->lock);
	}
	return err;
}

/*
 * The window's memory at page `pgoff` of it, up to `nr_pages` of it, as virtio_fs's. cladefs: their
 * struct pages made first, if they have none (TASK-92.8): the caller may sleep.
 */
static long vext_dax_direct_access(struct dax_device *dax_dev, pgoff_t pgoff, long nr_pages,
				   enum dax_access_mode mode, void **kaddr, unsigned long *pfn)
{
	struct vext_window *window = dax_get_private(dax_dev);
	phys_addr_t offset = PFN_PHYS(pgoff);
	long pages = min_t(long, nr_pages, window->len / PAGE_SIZE - pgoff);
	int err;

	might_sleep();
	if (pages <= 0)
		return pages;
	err = vext_window_make(window, offset, PFN_PHYS(pages));
	if (err)
		return err;
	if (kaddr)
		*kaddr = window->kaddr + offset;
	if (pfn)
		*pfn = PHYS_PFN(window->phys + offset);
	return pages;
}

static int vext_dax_zero_page_range(struct dax_device *dax_dev, pgoff_t pgoff, size_t nr_pages)
{
	void *kaddr;
	long rc = dax_direct_access(dax_dev, pgoff, nr_pages, DAX_ACCESS, &kaddr, NULL);

	if (rc < 0)
		return dax_mem2blk_err(rc);
	memset(kaddr, 0, nr_pages << PAGE_SHIFT);
	dax_flush(dax_dev, kaddr, nr_pages << PAGE_SHIFT);
	return 0;
}

static const struct dax_operations vext_dax_ops = {
	.direct_access = vext_dax_direct_access,
	.zero_page_range = vext_dax_zero_page_range,
};

static void vext_put_dax(void *data)
{
	struct dax_device *dax = data;

	kill_dax(dax);
	put_dax(dax);
}

/* Lets the window's made parts go, after its dax_device: unbinding, nothing uses them. */
static void vext_window_unmake(void *data)
{
	struct vext_window *window = data;
	unsigned int part;

	for_each_set_bit(part, window->made_parts, window->nr_parts)
		memunmap_pages(&window->parts[part]);
}

/*
 * Sets the window up, `region`, as pages a dax_device hands out, which mappings of files can map,
 * their struct pages made as they are reached: 0, or a -errno.
 */
static int vext_map_window_dax(struct device *dev, struct vext_window *window,
			       const struct virtio_shm_region *region)
{
	u64 end = region->addr + region->len;
	struct dax_device *dax;
	unsigned int part;
	int err;

	window->phys = region->addr;
	window->len = region->len;
	/* Where memremap_pages maps each part of a pgmap of FS_DAX's. */
	window->kaddr = __va(region->addr);
	window->nid = dev_to_node(dev);
	window->nr_parts = vext_window_part(window, region->len - 1) + 1;
	window->parts = devm_kcalloc(dev, window->nr_parts, sizeof(*window->parts), GFP_KERNEL);
	window->made_parts = devm_bitmap_zalloc(dev, window->nr_parts, GFP_KERNEL);
	if (!window->parts || !window->made_parts)
		return -ENOMEM;
	mutex_init(&window->lock);
	for (part = 0; part < window->nr_parts; part++) {
		u64 section = (region->addr >> PA_SECTION_SHIFT) + part;
		struct dev_pagemap *pgmap = &window->parts[part];

		pgmap->type = MEMORY_DEVICE_FS_DAX;
		pgmap->range = (struct range) {
			.start = max_t(u64, region->addr, section << PA_SECTION_SHIFT),
			.end = min_t(u64, end, (section + 1) << PA_SECTION_SHIFT) - 1,
		};
		pgmap->nr_range = 1;
	}
	/* Released after the dax_device's put, below: no direct access is left then. */
	err = devm_add_action_or_reset(dev, vext_window_unmake, window);
	if (err)
		return err;
	dax = alloc_dax(window, &vext_dax_ops);
	if (IS_ERR(dax))
		return PTR_ERR(dax);
	window->dax = dax;
	return devm_add_action_or_reset(dev, vext_put_dax, dax);
}

/* Maps the DAX window, if the device has one. */
static int vext_map_window(struct virtio_extent *vext)
{
	struct virtio_device *vdev = vext->vdev;
	struct virtio_shm_region region;

	if (!virtio_has_feature(vdev, VIRTIO_EXTENT_F_DAX))
		return 0;
	if (!virtio_get_shm_region(vdev, &region, 0)) {
		dev_err(&vdev->dev, "the device offers DAX but has no shared memory region\n");
		return -ENODEV;
	}
	if (!devm_request_mem_region(&vdev->dev, region.addr, region.len,
				     dev_name(&vdev->dev))) {
		dev_err(&vdev->dev, "the DAX window's memory is in use\n");
		return -EBUSY;
	}
	return vext_map_window_dax(&vdev->dev, &vext->window, &region);
}
#endif

static void vext_stats_work(struct work_struct *work);

static int vext_probe(struct virtio_device *vdev)
{
	struct virtio_extent *vext;
	int err;

	err = vext_check_version(vdev);
	if (err)
		return err;
	vext = devm_kzalloc(&vdev->dev, sizeof(*vext), GFP_KERNEL);
	if (!vext)
		return -ENOMEM;
	vext->vdev = vdev;
	vdev->priv = vext;
	spin_lock_init(&vext->changed_lock);
	spin_lock_init(&vext->busy_lock);
	mutex_init(&vext->stats_lock);
	INIT_WORK(&vext->stats_work, vext_stats_work);
	err = vext_read_config(vext);
	if (err)
		return err;
#if IS_ENABLED(CONFIG_FS_DAX)
	err = vext_map_window(vext);
	if (err)
		return err;
#endif
	err = vext_find_queues(vext);
	if (err)
		return err;
	/* cladefs: its data requests' stream, if it has one (board 14663). */
	err = vext_stream_start(vext);
	if (err) {
		vdev->config->del_vqs(vdev);
		return err;
	}
	virtio_device_ready(vdev);
	/*
	 * cladefs: the device activates as it takes DRIVER_OK, and asks for a reset if it can't: as it
	 * refuses a driver that didn't take every feature it offers (TASK-141.3). It serves nothing
	 * then, which the driver would wait for for good.
	 */
	if (vdev->config->get_status(vdev) & VIRTIO_CONFIG_S_NEEDS_RESET) {
		dev_err(&vdev->dev, "the device refused its activation, which the VMM's log says why\n");
		err = -ENODEV;
	} else {
		err = sysfs_create_group(&vdev->dev.kobj, &vext_attr_group);
		if (!err) {
			err = vext_selftest_add(vext, vdev);
			if (err)
				sysfs_remove_group(&vdev->dev.kobj, &vext_attr_group);
		}
	}
	if (err) {
		vext_stream_close(vext);
		virtio_reset_device(vdev);
		vdev->config->del_vqs(vdev);
		return err;
	}
	mutex_lock(&devices_lock);
	list_add(&vext->node, &devices);
	mutex_unlock(&devices_lock);
	dev_info(&vdev->dev, "extent store \"%s\", cell %llx, %u request queue%s%s%s\n",
		 vext->config.tag, vext->config.cell, vext->nr_queues,
		 vext->nr_queues == 1 ? "" : "s",
		 vext->window.dax ? ", with a DAX window copying on write" : "",
		 vext->stream.sender ? ", its data requests over vsock" : "");
	return 0;
}

bool virtio_extent_capacity(struct virtio_extent *vext, u64 *capacity, u64 *asked)
{
	struct virtio_device *vdev = vext->vdev;
	u64 again;

	if (!virtio_has_feature(vdev, VIRTIO_EXTENT_F_RESIZE))
		return false;
	/* Each read whole: the same twice, as the device may change it between halves. */
	virtio_cread_le(vdev, struct virtio_extent_config, capacity, capacity);
	do {
		again = *capacity;
		virtio_cread_le(vdev, struct virtio_extent_config, capacity, capacity);
	} while (*capacity != again);
	virtio_cread_le(vdev, struct virtio_extent_config, asked, asked);
	do {
		again = *asked;
		virtio_cread_le(vdev, struct virtio_extent_config, asked, asked);
	} while (*asked != again);
	return true;
}
EXPORT_SYMBOL_GPL(virtio_extent_capacity);

void virtio_extent_set_config_changed(struct virtio_extent *vext, vext_config_changed_t changed,
				      void *arg)
{
	unsigned long flags;

	spin_lock_irqsave(&vext->changed_lock, flags);
	vext->changed = changed;
	vext->changed_arg = arg;
	spin_unlock_irqrestore(&vext->changed_lock, flags);
}
EXPORT_SYMBOL_GPL(virtio_extent_set_config_changed);

void virtio_extent_set_stats(struct virtio_extent *vext, vext_stats_t stats, void *arg)
{
	mutex_lock(&vext->stats_lock);
	vext->stats = stats;
	vext->stats_arg = arg;
	mutex_unlock(&vext->stats_lock);
}
EXPORT_SYMBOL_GPL(virtio_extent_set_stats);

/*
 * cladefs: answers the host's latest ask for the guest's stats, if it hasn't (F_STATS, TASK-111.4):
 * with what the filesystem says (virtio_extent_set_stats), or nothing. Answered once, failing or
 * not: the host asks again, which answers the asks before it too.
 */
static void vext_stats_work(struct work_struct *work)
{
	struct virtio_extent *vext = container_of(work, struct virtio_extent, stats_work);
	struct vext_request *req = NULL;
	ssize_t len = 0;
	char *buf = NULL;
	u64 asked, again;
	int err;

	/* Read whole: the same twice, as the device may change it between halves. */
	virtio_cread_le(vext->vdev, struct virtio_extent_config, stats_asked, &asked);
	do {
		again = asked;
		virtio_cread_le(vext->vdev, struct virtio_extent_config, stats_asked, &asked);
	} while (asked != again);
	if (!asked || asked == vext->stats_answered)
		return;
	vext->stats_answered = asked;
	req = kzalloc(sizeof(*req), GFP_KERNEL);
	buf = kvmalloc(VIRTIO_EXTENT_MAX_STATS, GFP_KERNEL);
	if (!req || !buf) {
		err = -ENOMEM;
		goto out;
	}
	mutex_lock(&vext->stats_lock);
	if (vext->stats)
		len = vext->stats(vext->stats_arg, buf, VIRTIO_EXTENT_MAX_STATS);
	mutex_unlock(&vext->stats_lock);
	if (len < 0) {
		dev_warn_ratelimited(&vext->vdev->dev, "the filesystem's stats: %zd\n", len);
		len = 0;
	}
	req->type = VIRTIO_EXTENT_T_STATS;
	req->offset = asked;
	req->data = buf;
	req->data_len = min_t(size_t, len, VIRTIO_EXTENT_MAX_STATS);
	err = virtio_extent_call(vext, req);
out:
	/* A device resetting as it unbinds answers itself. */
	if (err && err != -ESHUTDOWN)
		dev_warn_ratelimited(&vext->vdev->dev, "answering an ask for stats: %d\n", err);
	kvfree(buf);
	kfree(req);
}

static void vext_config_changed(struct virtio_device *vdev)
{
	struct virtio_extent *vext = vdev->priv;
	unsigned long flags;

	spin_lock_irqsave(&vext->changed_lock, flags);
	if (vext->changed)
		vext->changed(vext->changed_arg);
	spin_unlock_irqrestore(&vext->changed_lock, flags);
	/* cladefs: an ask for the guest's stats, if it changed, the work finds (TASK-111.4). */
	if (vext_stats_asked(vext))
		schedule_work(&vext->stats_work);
}

/*
 * cladefs: closes its queues to requests, under each one's lock, which a request is added under
 * (virtio_extent_submit): none is added after, and those waiting for room find them closed.
 */
static void vext_close_queues(struct virtio_extent *vext)
{
	unsigned int i;

	for (i = 0; i < vext->nr_queues; i++) {
		struct vext_queue *queue = &vext->queues[i];

		spin_lock_irq(&queue->lock);
		queue->closed = true;
		spin_unlock_irq(&queue->lock);
		wake_up_all(&queue->room);
	}
}

/*
 * cladefs: resets the device, and lets its queues go, as it unbinds (vext_remove). Its queues
 * closed first, then the stats work, which asks of the device whenever the host asks for stats,
 * done: its request in flight answered by the device, live still, and any after refused
 * (-ESHUTDOWN). Then the reset, which nothing adds to a queue after: a request the stats work
 * added once the reset had let go of the queues was a use after free, or, added once it had
 * completed those queued, it waited for ever (TASK-201).
 */
static void vext_reset(struct virtio_device *vdev)
{
	struct virtio_extent *vext = vdev->priv;
	struct vext_request *req;
	unsigned int i;

	vext_close_queues(vext);
	flush_work(&vext->stats_work);
	/* cladefs: its stream's requests failed, none taken after (board 14663). */
	vext_stream_close(vext);
	virtio_reset_device(vdev);
	for (i = 0; i < vext->nr_queues; i++) {
		struct vext_queue *queue = &vext->queues[i];

		for (;;) {
			spin_lock_irq(&queue->lock);
			req = virtqueue_detach_unused_buf(queue->vq);
			spin_unlock_irq(&queue->lock);
			if (!req)
				break;
			req->status = -EIO;
			vext_busy_end(vext);
			sg_free_table(&req->data_sgt);
			req->done(req);
		}
	}
	vdev->config->del_vqs(vdev);
}


static void vext_remove(struct virtio_device *vdev)
{
	struct virtio_extent *vext = vdev->priv;

	mutex_lock(&devices_lock);
	list_del(&vext->node);
	mutex_unlock(&devices_lock);
	/*
	 * cladefs: a filesystem goes on using the device, whose window it maps, and which goes with
	 * this: unbinding waits for it to unmount, which needs requests to go on (TASK-79.48).
	 */
	wait_event_idle(devices_unused, !atomic_read(&vext->users));
	/* cladefs: no ask for stats is answered from now on (TASK-111.4). */
	cancel_work_sync(&vext->stats_work);
	vext_selftest_remove(vdev);
	sysfs_remove_group(&vdev->dev.kobj, &vext_attr_group);
	vext_reset(vdev);
}

static const struct virtio_device_id vext_ids[] = {
	{ VIRTIO_ID_EXTENT, VIRTIO_DEV_ANY_ID },
	{ 0 },
};

static const unsigned int vext_features[] = {
#if IS_ENABLED(CONFIG_FS_DAX)
	VIRTIO_EXTENT_F_DAX,
#endif
	VIRTIO_EXTENT_F_WRITE_COMMIT,
	VIRTIO_EXTENT_F_RESIZE,
	VIRTIO_EXTENT_F_FREEZE,
	VIRTIO_EXTENT_F_FREE_MANY,
	VIRTIO_EXTENT_F_MAP_MANY,
	VIRTIO_EXTENT_F_MAP_PREFAULT,
	VIRTIO_EXTENT_F_STATS,
	VIRTIO_EXTENT_F_STREAM,
};

static struct virtio_driver vext_driver = {
	.driver.name = "virtio_extent",
	/* cladefs: its window's memmap built beside the rest of the boot (virtio_extent_find). */
	.driver.probe_type = PROBE_PREFER_ASYNCHRONOUS,
	.id_table = vext_ids,
	.feature_table = vext_features,
	.feature_table_size = ARRAY_SIZE(vext_features),
	.probe = vext_probe,
	.remove = vext_remove,
	.config_changed = vext_config_changed,
};

module_virtio_driver(vext_driver);
MODULE_DEVICE_TABLE(virtio, vext_ids);
MODULE_DESCRIPTION("The virtio extent-store device's driver");
MODULE_LICENSE("Dual BSD/GPL");
