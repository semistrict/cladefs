/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * The extent-store device the filesystem lives on (drivers/virtio_extent, backlog doc-4), as the
 * filesystem's Rust uses it: its requests, with plain arguments, completed through a callback.
 * Rust declares the same in guest/rust/cladefs-linux's device module.
 */
#ifndef _FS_CLADEFS_DEVICE_H
#define _FS_CLADEFS_DEVICE_H

struct seq_file;

#include <linux/types.h>

/* A bound extent-store device. */
struct cladefs_device;

/* A WRITE may carry VIRTIO_EXTENT_WRITE_COMMIT. */
#define CLADEFS_DEVICE_WRITE_COMMIT (1 << 0)
/* The device takes FREEZE, and INFO says the cell (VIRTIO_EXTENT_F_FREEZE). */
#define CLADEFS_DEVICE_FREEZE (1 << 1)
/* cladefs: a FREE may carry VIRTIO_EXTENT_FREE_MANY, more extents in its data. */
#define CLADEFS_DEVICE_FREE_MANY (1 << 3)

/* What the filesystem needs to know of a device. */
struct cladefs_device_info {
	u32 block_size;
	/* The most bytes one READ or WRITE moves, from anywhere in memory: a multiple of blocks. */
	u32 max_io;
	/* The most bytes the root holds. */
	u32 max_root;
	/* CLADEFS_DEVICE_*. */
	u32 flags;
	/* The cell whose extents the device's filesystem writes. */
	u64 cell;
	/* The most bytes one extent holds. */
	u64 max_extent;
};

/*
 * The device tagged with the `len` bytes of `tag`, filling in `info`, or NULL if there is none.
 * The caller puts it once done with it (cladefs_device_put).
 */
struct cladefs_device *cladefs_device_find(const char *tag, size_t len, struct cladefs_device_info *info);
void cladefs_device_put(struct cladefs_device *dev);

/*
 * A request: its type (VIRTIO_EXTENT_T_*) and flags, the extent it names (READ, WRITE, FREE), and
 * where in it (READ, WRITE), and its data: READ's buffer, WRITE's data, COMMIT's root, ROOT's
 * buffer (max_root bytes, and a struct virtio_extent_root_len after them), or INFO's struct
 * virtio_extent_info. The data must be kmalloc'd or vmalloc'd, and stay until the request is
 * done. A WRITE with VIRTIO_EXTENT_WRITE_ROOT has a root too, which its commit sets, of root_len
 * bytes: kmalloc'd.
 */
struct cladefs_device_op {
	u32 type;
	u32 flags;
	u64 cell;
	u64 sequence;
	u64 offset;
	void *data;
	u32 len;
	u32 root_len;
	void *root;
};

/* Called once the device completes a request, from its interrupt: it must not sleep. */
typedef void (*cladefs_device_done_t)(void *arg, int status);

/*
 * Submits `op`, and calls `done(arg, status)` once the device completes it, with 0 or the device's
 * -errno. Returns 0, or a -errno if it couldn't submit it, when it doesn't call `done`. May sleep.
 */
int cladefs_device_submit(struct cladefs_device *dev, const struct cladefs_device_op *op,
		       cladefs_device_done_t done, void *arg);

/*
 * Requests that seal the extents open (doc-4), a COMMIT, or a WRITE with a barrier, are submitted
 * between cladefs_device_seal_begin and cladefs_device_seal_end, once the device completes it, which
 * count seals (cladefs_device_seals): extents Fxfs takes for sealed from a seal's beginning are on the
 * device only at its end, and the DAX window maps sealed extents alone (doc-13). `sealed(arg)` is
 * called once each seal ends; it may not wait for the filesystem's Rust, which seals.
 */
typedef void (*cladefs_device_sealed_t)(void *arg);
void cladefs_device_set_sealed(struct cladefs_device *dev, cladefs_device_sealed_t sealed, void *arg);

/*
 * The store's capacity, and the shrink the guest is asked to take, or 0, as the device's
 * configuration says now: 0, or -EOPNOTSUPP if it says neither (VIRTIO_EXTENT_F_RESIZE).
 */
int cladefs_device_capacity(struct cladefs_device *dev, u64 *capacity, u64 *asked);
/*
 * Has `changed(arg)` called as the device's configuration changes: the host resizes the store, or
 * a shrink is asked. Or nothing, if `changed` is NULL. Called from the device's interrupt: it must
 * not sleep.
 */
typedef void (*cladefs_device_changed_t)(void *arg);
void cladefs_device_set_changed(struct cladefs_device *dev, cladefs_device_changed_t changed, void *arg);
/* Waits for a seal in progress, if any, to end. */
void cladefs_device_wait_unsealed(struct cladefs_device *dev);
/*
 * The seal count: how many seals have begun and ended, odd while one is in progress, whose request
 * may seal extents opened meanwhile.
 */
u64 cladefs_device_seals(struct cladefs_device *dev);
void cladefs_device_seal_begin(struct cladefs_device *dev);
void cladefs_device_seal_end(struct cladefs_device *dev);

struct dax_device;

/*
 * The DAX window's dax_device, if files' data can be mapped from it, at the page size, which is a
 * block: cladefs: the window copies on write (doc-13), mapping sealed extents and zeros alone,
 * read-only, a store to a page making it the VM's own, private, until a MAP or UNMAP over it. Sets
 * `window_len`.
 */
struct dax_device *cladefs_device_dax(struct cladefs_device *dev, u64 *window_len);
/* cladefs: the most private pages the device holds at once, past which a store stops the VM. */
u64 cladefs_device_private_max(struct cladefs_device *dev);
/*
 * Maps `len` bytes at `offset` in the extent {cell, sequence} at `window_offset` in the window,
 * read-only (MAP), or unmaps `len` bytes there (UNMAP), and waits for the device. cladefs:
 * `prefault` if the guest accesses the pages next, as a read does, which the device then maps for
 * it ahead of its accesses, if it can (MAP_PREFAULT, TASK-112.1).
 */
int cladefs_device_map(struct cladefs_device *dev, u64 cell, u64 sequence, u64 offset, u64 len,
		    u64 window_offset, bool prefault);

/*
 * cladefs: one of cladefs_device_map_many's maps, as cladefs_device_map's arguments say it; or, with
 * `zeros`, cladefs_device_map_zeros's, its extent ignored.
 */
struct cladefs_device_map {
	u64 cell;
	u64 sequence;
	u64 offset;
	u64 len;
	u64 window_offset;
	bool prefault;
	bool zeros;
};

/*
 * cladefs: the `count` maps `maps`, in as few requests as the device takes (MAP_MANY), or one each:
 * a chunk's pieces (TASK-91.4). The first error fails them, whichever others the device mapped.
 */
int cladefs_device_map_many(struct cladefs_device *dev, const struct cladefs_device_map *maps,
			 size_t count);
int cladefs_device_unmap(struct cladefs_device *dev, u64 window_offset, u64 len);
/*
 * cladefs: maps zeros at `len` bytes of the window at `window_offset` (MAP_ZEROS), read-
 * only, as holes and pages past a file's end are; and makes `len` bytes there private now, as
 * stores would, mapped for the guest ahead of its accesses (PRIVATE): -ENOMEM past private_max.
 * Each waits for the device.
 */
int cladefs_device_map_zeros(struct cladefs_device *dev, u64 window_offset, u64 len);
int cladefs_device_private(struct cladefs_device *dev, u64 window_offset, u64 len);
/* cladefs: shows what the window's requests took, as lines of a name and a number (TASK-91.7). */
void cladefs_device_show_stats(struct cladefs_device *dev, struct seq_file *m);

#endif
