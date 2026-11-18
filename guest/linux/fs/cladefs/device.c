// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The cladefs Authors.
/*
 * The extent-store device, as the filesystem uses it (see device.h): requests of the driver's,
 * each allocated as it is submitted and freed as it completes, and the count of seals.
 */

#include <linux/minmax.h>
#include <linux/moduleparam.h>
#include <linux/seq_file.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/string.h>
#include <linux/timekeeping.h>
#include <linux/virtio_extent.h>
#include <linux/wait.h>

#include "device.h"
#include "hist.h"

#define CREATE_TRACE_POINTS
#include "cladefs_trace.h"

struct cladefs_device {
	struct virtio_extent *vext;
	/* Guards the seals. */
	spinlock_t lock;
	/* Whether a seal is in progress. */
	bool sealing;
	/* How many seals have begun and ended: odd while one is in progress. */
	u64 seals;
	/* Woken as seals end. */
	wait_queue_head_t wait;
	cladefs_device_sealed_t sealed;
	void *sealed_arg;
	/* cladefs: what the window's requests took (TASK-91.7). */
	atomic64_t maps, map_ns, unmaps, unmap_ns;
	/* cladefs: what MAP requests took, and UNMAPs, each (TASK-111.6). */
	struct cladefs_hist map_hist, unmap_hist;
	/* cladefs: the requests the maps took, fewer than the maps with MAP_MANY (TASK-91.4). */
	atomic64_t map_requests;
	/* cladefs: the PRIVATE requests, and the bytes they made private (doc-13). */
	atomic64_t privates, private_bytes;
	/* cladefs: what they took, and each (TASK-164.10). */
	atomic64_t private_ns;
	struct cladefs_hist private_hist;
	/* cladefs: the bytes written since a seal last ended, which the next seals (cladefs_seal_end). */
	atomic64_t unsealed_bytes;
};

struct cladefs_device_request {
	struct vext_request req;
	cladefs_device_done_t done;
	void *arg;
	/* cladefs: the id its events share (cladefs_device_submit, cladefs_device_done). */
	u64 trace_id;
};

/*
 * cladefs: the id of a request's events (cladefs_trace.h), if its first event, `on`, is: the guest's
 * own count, as the device's requests carry no id. 0 if it is off.
 */
static atomic64_t cladefs_device_trace_ids = ATOMIC64_INIT(0);

static u64 cladefs_device_trace_id(bool on)
{
	return on ? atomic64_inc_return(&cladefs_device_trace_ids) : 0;
}

struct cladefs_device *cladefs_device_find(const char *tag, size_t len, struct cladefs_device_info *info)
{
	const struct vext_config *config;
	struct virtio_extent *vext;
	struct cladefs_device *dev;
	char name[sizeof(config->tag)];
	u32 max_io;

	if (len >= sizeof(name))
		return NULL;
	memcpy(name, tag, len);
	name[len] = 0;
	vext = virtio_extent_find(name);
	if (!vext)
		return NULL;
	dev = kzalloc(sizeof(*dev), GFP_NOFS);
	if (!dev) {
		virtio_extent_put(vext);
		return NULL;
	}
	dev->vext = vext;
	spin_lock_init(&dev->lock);
	init_waitqueue_head(&dev->wait);
	config = virtio_extent_config(vext);
	/* Memory not aligned to pages spans one more page than its length needs. */
	max_io = min_t(u64, config->max_transfer, (u64)(config->max_segments - 1) * PAGE_SIZE);
	*info = (struct cladefs_device_info) {
		.block_size = config->block_size,
		.max_io = rounddown(max_io, config->block_size),
		.max_root = config->max_root,
		.flags = (virtio_extent_write_commit(vext) ? CLADEFS_DEVICE_WRITE_COMMIT : 0) |
			 (virtio_extent_freeze(vext) ? CLADEFS_DEVICE_FREEZE : 0) |
			 (virtio_extent_read_only(vext) ? CLADEFS_DEVICE_READ_ONLY : 0) |
			 (virtio_extent_free_many(vext) ? CLADEFS_DEVICE_FREE_MANY : 0),
		.cell = config->cell,
		.max_extent = config->max_extent,
	};
	return dev;
}

void cladefs_device_put(struct cladefs_device *dev)
{
	virtio_extent_put(dev->vext);
	kfree(dev);
}

static void cladefs_device_done(struct vext_request *req)
{
	struct cladefs_device_request *r = container_of(req, struct cladefs_device_request, req);

	trace_cladefs_device_done(r->trace_id, req->type, req->status);
	r->done(r->arg, req->status);
	kfree(r);
}

int cladefs_device_submit(struct cladefs_device *dev, const struct cladefs_device_op *op,
		       cladefs_device_done_t done, void *arg)
{
	struct cladefs_device_request *r = kzalloc(sizeof(*r), GFP_NOFS);
	int err;

	if (!r)
		return -ENOMEM;
	r->req.type = op->type;
	r->req.flags = op->flags;
	r->req.id = (struct vext_id) { op->cell, op->sequence };
	r->req.offset = op->offset;
	r->req.data = op->data;
	r->req.data_len = op->len;
	r->req.root = op->root;
	r->req.root_len = op->root_len;
	r->req.done = cladefs_device_done;
	r->done = done;
	r->arg = arg;
	r->trace_id = cladefs_device_trace_id(trace_cladefs_device_submit_enabled());
	if (op->type == VIRTIO_EXTENT_T_WRITE)
		atomic64_add(op->len, &dev->unsealed_bytes);
	trace_cladefs_device_submit(r->trace_id, op->type, op->flags, op->cell, op->sequence,
				 op->offset, op->len);
	err = virtio_extent_submit(dev->vext, &r->req);
	if (err) {
		trace_cladefs_device_done(r->trace_id, op->type, err);
		kfree(r);
	}
	return err;
}

void cladefs_device_set_sealed(struct cladefs_device *dev, cladefs_device_sealed_t sealed, void *arg)
{
	spin_lock(&dev->lock);
	dev->sealed = sealed;
	dev->sealed_arg = arg;
	spin_unlock(&dev->lock);
}

int cladefs_device_capacity(struct cladefs_device *dev, u64 *capacity, u64 *asked)
{
	return virtio_extent_capacity(dev->vext, capacity, asked) ? 0 : -EOPNOTSUPP;
}

void cladefs_device_set_changed(struct cladefs_device *dev, cladefs_device_changed_t changed, void *arg)
{
	virtio_extent_set_config_changed(dev->vext, changed, arg);
}

u64 cladefs_device_committed(struct cladefs_device *dev)
{
	return virtio_extent_committed(dev->vext);
}

void cladefs_device_wait_unsealed(struct cladefs_device *dev)
{
	wait_event(dev->wait, !READ_ONCE(dev->sealing));
}

u64 cladefs_device_seals(struct cladefs_device *dev)
{
	u64 seals;

	spin_lock(&dev->lock);
	seals = dev->seals;
	spin_unlock(&dev->lock);
	return seals;
}

void cladefs_device_seal_begin(struct cladefs_device *dev)
{
	u64 seals;

	spin_lock(&dev->lock);
	while (dev->sealing) {
		spin_unlock(&dev->lock);
		wait_event(dev->wait, !READ_ONCE(dev->sealing));
		spin_lock(&dev->lock);
	}
	dev->sealing = true;
	seals = ++dev->seals;
	spin_unlock(&dev->lock);
	trace_cladefs_seal_begin(seals);
}

void cladefs_device_seal_end(struct cladefs_device *dev)
{
	cladefs_device_sealed_t sealed;
	void *arg;
	u64 seals;

	spin_lock(&dev->lock);
	dev->sealing = false;
	seals = ++dev->seals;
	sealed = dev->sealed;
	arg = dev->sealed_arg;
	spin_unlock(&dev->lock);
	/* cladefs: writes are kept out through a seal (cladefs-linux's Seal): its request's are its. */
	trace_cladefs_seal_end(seals, atomic64_xchg(&dev->unsealed_bytes, 0));
	wake_up_all(&dev->wait);
	if (sealed)
		sealed(arg);
}

struct dax_device *cladefs_device_dax(struct cladefs_device *dev, u64 *window_len)
{
	const struct vext_config *config = virtio_extent_config(dev->vext);

	if (config->block_size != PAGE_SIZE || config->map_alignment > PAGE_SIZE)
		return NULL;
	return virtio_extent_dax(dev->vext, window_len);
}

/*
 * cladefs: diagnostic (TASK-121): whether maps carry MAP_PREFAULT, which has the VM process map the
 * pages for the guest ahead of its accesses, after the MAP, not waited for.
 */
static bool cladefs_device_prefaults = true;
module_param_named(prefault, cladefs_device_prefaults, bool, 0644);
MODULE_PARM_DESC(prefault, "Ask the device to prefault pages the guest reads (diagnostic)");

/* cladefs: the flags of `map`, accessed next if `prefault`, or zeros, that the device takes. */
static u32 cladefs_device_map_flags(struct cladefs_device *dev, const struct cladefs_device_map *map)
{
	u32 flags = 0;

	if (map->zeros)
		flags |= VIRTIO_EXTENT_MAP_ZEROS;
	if (map->prefault && READ_ONCE(cladefs_device_prefaults) &&
	    virtio_extent_map_prefault(dev->vext))
		flags |= VIRTIO_EXTENT_MAP_PREFAULT;
	return flags;
}

/* cladefs: `map`, alone, in a MAP request of its own. */
static int cladefs_device_map_one(struct cladefs_device *dev, const struct cladefs_device_map *map)
{
	struct vext_request *req = kzalloc(sizeof(*req), GFP_NOFS);
	u64 began, id;
	int err;

	if (!req)
		return -ENOMEM;
	req->type = VIRTIO_EXTENT_T_MAP;
	req->flags = cladefs_device_map_flags(dev, map);
	/* Zeros name no extent. */
	if (!map->zeros)
		req->id = (struct vext_id) { map->cell, map->sequence };
	req->offset = map->zeros ? 0 : map->offset;
	req->len = map->len;
	req->window_offset = map->window_offset;
	id = cladefs_device_trace_id(trace_cladefs_window_map_start_enabled());
	trace_cladefs_window_map_start(id, req->id.cell, req->id.sequence, req->offset, req->len,
				    req->window_offset, 1, map->zeros);
	began = ktime_get_ns();
	err = virtio_extent_call(dev->vext, req);
	trace_cladefs_window_map_end(id, err);
	atomic64_inc(&dev->maps);
	atomic64_inc(&dev->map_requests);
	atomic64_add(ktime_get_ns() - began, &dev->map_ns);
	cladefs_hist_add(&dev->map_hist, ktime_get_ns() - began);
	kfree(req);
	return err;
}

int cladefs_device_map(struct cladefs_device *dev, u64 cell, u64 sequence, u64 offset, u64 len,
		    u64 window_offset, bool prefault)
{
	const struct cladefs_device_map map = {
		.cell = cell, .sequence = sequence, .offset = offset, .len = len,
		.window_offset = window_offset, .prefault = prefault,
	};

	return cladefs_device_map_one(dev, &map);
}

int cladefs_device_map_zeros(struct cladefs_device *dev, u64 window_offset, u64 len)
{
	const struct cladefs_device_map map = {
		.len = len, .window_offset = window_offset, .zeros = true,
	};

	return cladefs_device_map_one(dev, &map);
}

int cladefs_device_private(struct cladefs_device *dev, u64 window_offset, u64 len)
{
	struct vext_request *req = kzalloc(sizeof(*req), GFP_NOFS);
	u64 began, took, id;
	int err;

	if (!req)
		return -ENOMEM;
	req->type = VIRTIO_EXTENT_T_PRIVATE;
	req->len = len;
	req->window_offset = window_offset;
	id = cladefs_device_trace_id(trace_cladefs_window_private_start_enabled());
	trace_cladefs_window_private_start(id, window_offset, len);
	began = ktime_get_ns();
	err = virtio_extent_call(dev->vext, req);
	took = ktime_get_ns() - began;
	trace_cladefs_window_private_end(id, err);
	atomic64_inc(&dev->privates);
	atomic64_add(took, &dev->private_ns);
	cladefs_hist_add(&dev->private_hist, took);
	if (!err)
		atomic64_add(len, &dev->private_bytes);
	kfree(req);
	/*
	 * cladefs: the host's ENOMEM, past private_max, is the guest's count and the host's apart,
	 * which the guest keeps from by sending PRIVATE only within it (doc-13): EIO, not a want of
	 * memory to wait out (TASK-170). The guest's own, above, ENOMEM (TASK-44.15).
	 */
	if (err == -ENOMEM) {
		pr_warn_ratelimited("cladefs: the host refused %llu bytes at %llx of the window private: past private_max\n",
				    len, window_offset);
		err = -EIO;
	}
	return err;
}

u64 cladefs_device_private_max(struct cladefs_device *dev)
{
	return virtio_extent_private_max(dev->vext);
}

bool cladefs_device_read_only(struct cladefs_device *dev)
{
	return virtio_extent_read_only(dev->vext);
}

/* cladefs: the most maps one MAP_MANY request holds: its further ones a page's worth. */
#define CLADEFS_DEVICE_MAP_MANY (PAGE_SIZE / sizeof(struct virtio_extent_map_entry) + 1)

/* cladefs: `count` maps, from `maps`, in one MAP_MANY request; `count` at most CLADEFS_DEVICE_MAP_MANY. */
static int cladefs_device_map_one_request(struct cladefs_device *dev, const struct cladefs_device_map *maps,
				       size_t count, struct virtio_extent_map_entry *entries)
{
	struct vext_request *req = kzalloc(sizeof(*req), GFP_NOFS);
	u64 began, id, len;
	size_t i;
	int err;

	if (!req)
		return -ENOMEM;
	/* cladefs: zeros name no extent. */
	for (i = 1; i < count; i++)
		entries[i - 1] = (struct virtio_extent_map_entry) {
			.map = {
				.id = {
					cpu_to_le64(maps[i].zeros ? 0 : maps[i].cell),
					cpu_to_le64(maps[i].zeros ? 0 : maps[i].sequence),
				},
				.offset = cpu_to_le64(maps[i].zeros ? 0 : maps[i].offset),
				.len = cpu_to_le64(maps[i].len),
				.window_offset = cpu_to_le64(maps[i].window_offset),
			},
			.flags = cpu_to_le32(cladefs_device_map_flags(dev, &maps[i])),
		};
	req->type = VIRTIO_EXTENT_T_MAP;
	req->flags = VIRTIO_EXTENT_MAP_MANY | cladefs_device_map_flags(dev, &maps[0]);
	if (!maps[0].zeros)
		req->id = (struct vext_id) { maps[0].cell, maps[0].sequence };
	req->offset = maps[0].zeros ? 0 : maps[0].offset;
	req->len = maps[0].len;
	req->window_offset = maps[0].window_offset;
	req->data = entries;
	req->data_len = (count - 1) * sizeof(*entries);
	id = cladefs_device_trace_id(trace_cladefs_window_map_start_enabled());
	if (id) {
		for (i = 0, len = 0; i < count; i++)
			len += maps[i].len;
		trace_cladefs_window_map_start(id, req->id.cell, req->id.sequence, req->offset, len,
					    req->window_offset, count, maps[0].zeros);
	}
	began = ktime_get_ns();
	err = virtio_extent_call(dev->vext, req);
	trace_cladefs_window_map_end(id, err);
	atomic64_add(count, &dev->maps);
	atomic64_inc(&dev->map_requests);
	atomic64_add(ktime_get_ns() - began, &dev->map_ns);
	cladefs_hist_add(&dev->map_hist, ktime_get_ns() - began);
	kfree(req);
	return err;
}

int cladefs_device_map_many(struct cladefs_device *dev, const struct cladefs_device_map *maps,
			 size_t count)
{
	struct virtio_extent_map_entry *entries;
	size_t done, n;
	int err = 0;

	if (count == 1 || !virtio_extent_map_many(dev->vext)) {
		for (done = 0; done < count && !err; done++)
			err = cladefs_device_map_one(dev, &maps[done]);
		return err;
	}
	entries = kmalloc_array(CLADEFS_DEVICE_MAP_MANY - 1, sizeof(*entries), GFP_NOFS);
	if (!entries)
		return -ENOMEM;
	for (done = 0; done < count && !err; done += n) {
		n = min_t(size_t, count - done, CLADEFS_DEVICE_MAP_MANY);
		err = cladefs_device_map_one_request(dev, maps + done, n, entries);
	}
	kfree(entries);
	return err;
}

/*
 * cladefs: an UNMAP's request is allocated without failing: the window space it unmaps goes to other
 * extents once it is unmapped, and is lost to the window if it can't be (TASK-44.22). A request is
 * a few hundred bytes, which the allocator makes room for. It can't be on the stack, whose memory
 * the device can't be given if it is vmalloc's (VMAP_STACK).
 */
int cladefs_device_unmap(struct cladefs_device *dev, u64 window_offset, u64 len)
{
	struct vext_request *req = kzalloc(sizeof(*req), GFP_NOFS | __GFP_NOFAIL);
	u64 began, id;
	int err;

	req->type = VIRTIO_EXTENT_T_UNMAP;
	req->len = len;
	req->window_offset = window_offset;
	id = cladefs_device_trace_id(trace_cladefs_window_unmap_start_enabled());
	trace_cladefs_window_unmap_start(id, window_offset, len);
	began = ktime_get_ns();
	err = virtio_extent_call(dev->vext, req);
	trace_cladefs_window_unmap_end(id, err);
	atomic64_inc(&dev->unmaps);
	atomic64_add(ktime_get_ns() - began, &dev->unmap_ns);
	cladefs_hist_add(&dev->unmap_hist, ktime_get_ns() - began);
	kfree(req);
	return err;
}

void cladefs_device_show_stats(struct cladefs_device *dev, struct seq_file *m)
{
	seq_printf(m, "window_maps %lld\nwindow_maps_ns %lld\n", atomic64_read(&dev->maps),
		   atomic64_read(&dev->map_ns));
	seq_printf(m, "window_map_requests %lld\n", atomic64_read(&dev->map_requests));
	cladefs_hist_show(m, "window_map_requests", &dev->map_hist);
	seq_printf(m, "window_unmaps %lld\nwindow_unmaps_ns %lld\n", atomic64_read(&dev->unmaps),
		   atomic64_read(&dev->unmap_ns));
	cladefs_hist_show(m, "window_unmaps", &dev->unmap_hist);
	seq_printf(m, "window_privates %lld\nwindow_private_bytes %lld\nwindow_privates_ns %lld\n",
		   atomic64_read(&dev->privates), atomic64_read(&dev->private_bytes),
		   atomic64_read(&dev->private_ns));
	cladefs_hist_show(m, "window_privates", &dev->private_hist);
}
