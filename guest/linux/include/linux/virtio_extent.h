/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * The virtio extent-store device (cladefs's backlog doc-4, which specifies it), and the guest
 * kernel's driver of it (drivers/virtio_extent), which the guest's Fxfs stores its filesystem
 * through: extents it names, written once and sealed by the next commit, freed whole, a root
 * naming where its filesystem starts, and a DAX window that sealed extents map into.
 */
#ifndef _LINUX_VIRTIO_EXTENT_H
#define _LINUX_VIRTIO_EXTENT_H

#include <linux/types.h>

#define VIRTIO_ID_EXTENT 0x4658 /* "FX" */

/*
 * cladefs: the device's protocol version, its configuration's first field, which no version moves
 * (cladefs's TASK-141.3): a change to any of its requests, flags, features, configuration or what
 * they mean makes a new one. The driver refuses a device of another at probe, naming both, as
 * cladefs's rule has every format refuse another, rather than serve it with what both happen to
 * know. The features say what the device is configured as, not what it can do: the driver takes
 * every one the device offers, and the device refuses its activation otherwise. Firecracker's
 * device's is VIRTIO_EXTENT_VERSION in vendor/firecracker's virtio/extent/mod.rs.
 */
/* cladefs: 3 added F_STREAM and the configuration's stream_port (board 14663). */
#define VIRTIO_EXTENT_VERSION 3

/*
 * Shared memory region 0 is a DAX window, and MAP and UNMAP work. cladefs: the window copies on write
 * (cladefs's doc-13): it maps sealed extents and zeros alone, read-only, and a store to one of its
 * pages gives the device a private page holding what the page held, at the same address, until a
 * MAP or UNMAP over it; at most the configuration's `private_max` of them. MAP takes MAP_ZEROS, and
 * PRIVATE works. Bits 1, 9 and 10, which were writable DAX's, are unused.
 */
#define VIRTIO_EXTENT_F_DAX 0
/* WRITE takes WRITE_COMMIT: a COMMIT follows the write, in one request. */
#define VIRTIO_EXTENT_F_WRITE_COMMIT 2
/*
 * The configuration holds the store's capacity, which the host changes while the guest runs,
 * telling it with a configuration change; a shrink the guest is asked to take (asked) it answers
 * with RESIZED.
 */
#define VIRTIO_EXTENT_F_RESIZE 3
/*
 * FREEZE tells the device the filesystem is frozen, or thawed: a snapshot of the VM is taken
 * frozen, and a VM restored from it, whose store is a fork of its own, learns its cell, which
 * INFO says, as its filesystem thaws (cladefs's TASK-39.1).
 */
#define VIRTIO_EXTENT_F_FREEZE 4
/* Bit 5 was a live follower's, READ_ONLY, unused. */
/*
 * FREE takes FREE_MANY: its data names more extents to free, each a struct virtio_extent_id,
 * after the one its arguments do: a flush's dead extents in one request (cladefs's TASK-41).
 */
#define VIRTIO_EXTENT_F_FREE_MANY 6
/*
 * MAP takes MAP_MANY: its data holds more maps, each a struct virtio_extent_map_entry, after the
 * one its arguments say: the pieces of a file's chunk mapped in one request (cladefs's TASK-91.4).
 */
#define VIRTIO_EXTENT_F_MAP_MANY 7
/*
 * MAP takes MAP_PREFAULT: the guest accesses the pages it maps next, as it maps them to read, and
 * the device maps them for it ahead of its accesses (cladefs's TASK-112.1).
 */
#define VIRTIO_EXTENT_F_MAP_PREFAULT 8
/*
 * cladefs: the host asks the guest for its stats: the configuration's `stats_asked` changes, telling
 * it with a configuration change, and the guest answers with STATS (cladefs's TASK-111.4).
 */
#define VIRTIO_EXTENT_F_STATS 11
/*
 * cladefs: the guest sends its data requests, READ, WRITE, FREE, COMMIT, ROOT and INFO, over a
 * vsock stream to the host's port `stream_port`, doc-4's stream transport, which the extent daemon
 * serves itself, rather than on the device's queues (struct virtio_extent_stream_req, board 14663).
 */
#define VIRTIO_EXTENT_F_STREAM 12

#define VIRTIO_EXTENT_T_READ 1
#define VIRTIO_EXTENT_T_WRITE 2
#define VIRTIO_EXTENT_T_FREE 3
#define VIRTIO_EXTENT_T_MAP 4
#define VIRTIO_EXTENT_T_UNMAP 5
#define VIRTIO_EXTENT_T_COMMIT 6
#define VIRTIO_EXTENT_T_ROOT 7
#define VIRTIO_EXTENT_T_INFO 8
#define VIRTIO_EXTENT_T_RESIZED 9
#define VIRTIO_EXTENT_T_FREEZE 10
/* 11 to 14 were a live follower's ADVANCE, LET_GO, PIN and UNPIN; 15 writable DAX's SEALED: unused. */
/*
 * F_DAX: makes the window's pages its arguments name private now, as stores would, and maps
 * them for the guest ahead of its accesses: a write(2) of many pages in one round trip, rather than
 * a fault of the host's each. ENOMEM past private_max.
 */
#define VIRTIO_EXTENT_T_PRIVATE 16
/*
 * F_STATS: the guest's stats, its data, text of VIRTIO_EXTENT_MAX_STATS bytes at most, lines of a
 * name and a value: the answer to the ask its arguments name (stats_asked), and those before it.
 */
#define VIRTIO_EXTENT_T_STATS 17
/* The last request type. */
#define VIRTIO_EXTENT_T_LAST VIRTIO_EXTENT_T_STATS

/* F_STATS: the most bytes of the guest's stats a STATS carries. */
#define VIRTIO_EXTENT_MAX_STATS (64 << 10)

/*
 * WRITE: a barrier precedes it; a COMMIT follows it (F_WRITE_COMMIT); that commit is uploaded; it
 * sets the root, of root_len bytes after the data.
 */
#define VIRTIO_EXTENT_WRITE_BARRIER (1 << 0)
#define VIRTIO_EXTENT_WRITE_COMMIT (1 << 1)
#define VIRTIO_EXTENT_WRITE_UPLOADED (1 << 2)
#define VIRTIO_EXTENT_WRITE_ROOT (1 << 3)
/* COMMIT: it completes once uploaded; it sets the root. */
#define VIRTIO_EXTENT_COMMIT_UPLOADED (1 << 0)
#define VIRTIO_EXTENT_COMMIT_ROOT (1 << 1)
/*
 * MAP: its data holds more maps (F_MAP_MANY); accessed next (F_MAP_PREFAULT); cladefs: zeros, its
 * extent ignored, which a store makes a private page of zeros. Bit 0, writable DAX's MAP_WRITE, is
 * unused.
 */
#define VIRTIO_EXTENT_MAP_MANY (1 << 1)
#define VIRTIO_EXTENT_MAP_PREFAULT (1 << 2)
#define VIRTIO_EXTENT_MAP_ZEROS (1 << 3)
/* FREE: its data names more extents (F_FREE_MANY). */
#define VIRTIO_EXTENT_FREE_MANY (1 << 0)
/* RESIZED: the guest took the size it was asked to shrink to. */
#define VIRTIO_EXTENT_RESIZED_TOOK (1 << 0)
/*
 * FREEZE: the filesystem is frozen, nothing open, and it writes nothing until it thaws; without
 * it, thawed.
 */
#define VIRTIO_EXTENT_FREEZE_FROZEN (1 << 0)

struct virtio_extent_config {
	/* cladefs: VIRTIO_EXTENT_VERSION, first in every version. */
	__le32 version;
	__u8 tag[36];
	__le32 num_request_queues;
	__le32 block_size;
	__le32 map_alignment;
	__le32 max_transfer;
	__le32 max_segments;
	__le32 max_root;
	__le64 cell;
	__le64 max_extent;
	/* F_RESIZE: the store's capacity, and the shrink the guest is asked to take, or 0. */
	__le64 capacity;
	__le64 asked;
	/* F_DAX: the most private pages the device holds at once (pages of map_alignment). */
	__le64 private_max;
	/* F_STATS: the host's latest ask for the guest's stats, or 0 before the first. */
	__le64 stats_asked;
	/* F_STREAM: the host's vsock port the guest's data requests go to, or 0. */
	__le32 stream_port;
	__le32 reserved;
} __packed;

/*
 * cladefs: a request on doc-4's stream transport (F_STREAM), as a descriptor chain is on a queue:
 * `readable` bytes follow, the request's header, arguments and data; and it has `writable` bytes
 * for the device to write, its data and its response, which its reply carries. `tag` names the
 * reply: unique among the requests in flight.
 */
struct virtio_extent_stream_req {
	__le64 tag;
	__le32 readable;
	__le32 writable;
};

/*
 * cladefs: the reply to the request tagged `tag`: `written` bytes follow, its writable bytes as the
 * device wrote them, its response last; 0 if it wrote nothing, an EIO. Replies come in any order.
 */
struct virtio_extent_stream_reply {
	__le64 tag;
	__le32 written;
	__le32 reserved;
};

struct virtio_extent_id {
	__le64 cell;
	__le64 sequence;
};

struct virtio_extent_req {
	__le32 type;
	__le32 flags;
	__le64 unique;
};

struct virtio_extent_resp {
	__le32 status;
	__le32 reserved;
};

struct virtio_extent_io {
	struct virtio_extent_id id;
	__le64 offset;
	__le32 len;
	/* WRITE with WRITE_ROOT: the root's bytes, after the data. */
	__le32 root_len;
};

struct virtio_extent_map {
	struct virtio_extent_id id;
	__le64 offset;
	__le64 len;
	__le64 window_offset;
};

/* One of a MAP_MANY's further maps: its flags MAP_PREFAULT and MAP_ZEROS alone. */
struct virtio_extent_map_entry {
	struct virtio_extent_map map;
	__le32 flags;
	__le32 reserved;
};

struct virtio_extent_unmap {
	__le64 window_offset;
	__le64 len;
};

/* PRIVATE: the window's pages to make private. */
struct virtio_extent_private {
	__le64 window_offset;
	__le64 len;
};

struct virtio_extent_commit {
	__le32 root_len;
	__le32 reserved;
};

struct virtio_extent_root_len {
	__le32 len;
	__le32 reserved;
};

/* RESIZED: the capacity the guest was asked to shrink to. */
struct virtio_extent_resized {
	__le64 capacity;
};

/* STATS: the ask it answers. */
struct virtio_extent_stats {
	__le64 asked;
};

struct virtio_extent_info {
	__le64 capacity;
	__le64 used;
	__le64 available;
	__le64 next_sequence;
	/* The lowest sequence number to mint from: its writer's range's start. */
	__le64 sequence_floor;
	/* F_FREEZE: the cell the guest mints extents of, its store's. */
	__le64 cell;
};

#ifdef __KERNEL__

#include <linux/scatterlist.h>

/* A bound device. */
struct virtio_extent;
struct dax_device;

/* An extent's ID, in the CPU's byte order. */
struct vext_id {
	u64 cell;
	u64 sequence;
};

/* The device's configuration, in the CPU's byte order. */
struct vext_config {
	char tag[37];
	u32 block_size;
	u32 map_alignment;
	u32 max_transfer;
	/* The most pages one request's data spans: the device's limit, or the queue's. */
	u32 max_segments;
	u32 max_root;
	u64 cell;
	u64 max_extent;
	/* F_DAX: the most private pages the device holds at once, or 0 without it. */
	u64 private_max;
	/* cladefs: F_STREAM: the host's vsock port the data requests go to, or 0 (board 14663). */
	u32 stream_port;
};

/*
 * A request. The caller fills in the request's part, and the device's part is the driver's.
 * Requests and their data must be memory the device can address: kmalloc'd, or vmalloc'd for
 * data, not on the stack.
 */
struct vext_queue;

struct vext_request {
	/* The request: one of VIRTIO_EXTENT_T_*, and its flags. */
	u32 type;
	u32 flags;
	/* READ, WRITE, FREE, MAP: the extent. */
	struct vext_id id;
	/*
	 * READ, WRITE, MAP: where in the extent. MAP, UNMAP, PRIVATE: how much (len), and where in
	 * the window.
	 * RESIZED: the capacity asked (offset). STATS: the ask answered (offset).
	 */
	u64 offset;
	u64 len;
	u64 window_offset;
	/*
	 * The data, of data_len bytes: READ's buffer; WRITE's data; COMMIT's root, with
	 * VIRTIO_EXTENT_COMMIT_ROOT; FREE's further extents, with VIRTIO_EXTENT_FREE_MANY; MAP's
	 * further maps, with VIRTIO_EXTENT_MAP_MANY; ROOT's buffer, of max_root bytes and a struct
	 * virtio_extent_root_len after them; INFO's struct virtio_extent_info; STATS's text. cladefs:
	 * READ's and WRITE's length too: one length for all data (TASK-160.7).
	 */
	void *data;
	u32 data_len;
	/* WRITE with VIRTIO_EXTENT_WRITE_ROOT: the root its commit sets, kmalloc'd, as it is small. */
	void *root;
	u32 root_len;
	/* Called once the device completes the request, from its interrupt: it must not sleep. */
	void (*done)(struct vext_request *req);
	void *private;
	/* 0, or the device's -errno, once done. */
	int status;

	/* The device's part. */
	struct virtio_extent_req hdr;
	/* When it was queued, for the device's `request_ns`. */
	u64 queued_ns;
	/* cladefs: the queue it went to, which its caller may poll for it (virtio_extent_call). */
	struct vext_queue *queue;
	/* cladefs: the CPU it was queued on, which its completion's is compared with (TASK-276). */
	int cpu;
	union {
		struct virtio_extent_io io;
		struct virtio_extent_id id;
		struct virtio_extent_map map;
		struct virtio_extent_unmap unmap;
		struct virtio_extent_private private;
		struct virtio_extent_commit commit;
		struct virtio_extent_resized resized;
		struct virtio_extent_stats stats;
	} args;
	struct virtio_extent_resp resp;
	struct sg_table data_sgt;
	/*
	 * cladefs: on doc-4's stream transport (F_STREAM, board 14663): its frame's header, the lengths
	 * of its arguments, data and root, and its place in the requests waiting to be sent.
	 */
	struct virtio_extent_stream_req stream_hdr;
	u32 args_len, out_len, in_len, root_out;
	struct list_head stream_node;
	/* cladefs: the bytes of its frame sent so far, as a send without waiting stops short. */
	u32 stream_progress;
};

/*
 * The bound device tagged `tag`, or NULL. It stays bound until its finder puts it: unbinding it
 * waits for that, and finds no more of it meanwhile.
 */
struct virtio_extent *virtio_extent_find(const char *tag);
void virtio_extent_put(struct virtio_extent *vext);
const struct vext_config *virtio_extent_config(struct virtio_extent *vext);

/*
 * Submits `req`, whose `done` the driver calls once the device completes it. Waits for room in
 * the queue if it is full. Returns 0, or a -errno if it couldn't submit it, when it doesn't call
 * `done`: -EINVAL if its data spans more than max_segments pages.
 * Sleeps, so it is called from a task's context.
 */
int virtio_extent_submit(struct virtio_extent *vext, struct vext_request *req);
/* Submits `req` and waits for the device to complete it, returning its status. */
int virtio_extent_call(struct virtio_extent *vext, struct vext_request *req);

/*
 * The DAX window's dax_device, with FS_DAX, through which its memory is reached
 * (dax_direct_access), and the window's length; or NULL if the device has none. cladefs: one
 * accessor of the window, its memory reached no other way (TASK-92.8).
 */
struct dax_device *virtio_extent_dax(struct virtio_extent *vext, u64 *window_len);
/* cladefs: the most private pages the device's DAX window holds at once (F_DAX), or 0. */
u64 virtio_extent_private_max(struct virtio_extent *vext);
/* Whether a WRITE may carry WRITE_COMMIT (F_WRITE_COMMIT). */
bool virtio_extent_write_commit(struct virtio_extent *vext);
/* Whether the device takes FREEZE, and INFO says the cell. */
bool virtio_extent_freeze(struct virtio_extent *vext);
/* Whether a FREE may carry FREE_MANY (F_FREE_MANY). */
bool virtio_extent_free_many(struct virtio_extent *vext);
/* Whether a MAP may carry MAP_MANY (F_MAP_MANY). */
bool virtio_extent_map_many(struct virtio_extent *vext);
/* Whether a MAP, and each of its further maps, may carry MAP_PREFAULT (F_MAP_PREFAULT). */
bool virtio_extent_map_prefault(struct virtio_extent *vext);

/*
 * The store's capacity, and the shrink the guest is asked to take, or 0, as the configuration
 * says now: false if the device doesn't say (F_RESIZE).
 */
bool virtio_extent_capacity(struct virtio_extent *vext, u64 *capacity, u64 *asked);
/*
 * Has the driver call `changed(arg)` as the device's configuration changes, its capacity or a
 * shrink asked; or none, if `changed` is NULL.
 * Called from the device's interrupt: it must not sleep.
 */
typedef void (*vext_config_changed_t)(void *arg);
void virtio_extent_set_config_changed(struct virtio_extent *vext, vext_config_changed_t changed,
				      void *arg);

/*
 * cladefs: has the driver answer the host's asks for the guest's stats (F_STATS) with what
 * `stats(arg, buf, len)` writes in the `len` bytes at `buf`, VIRTIO_EXTENT_MAX_STATS: how many
 * bytes, or a -errno; with nothing while `stats` is NULL, or fails. Called from a work item, it may
 * sleep; once this returns, the one set before is called no more.
 */
typedef ssize_t (*vext_stats_t)(void *arg, char *buf, size_t len);
void virtio_extent_set_stats(struct virtio_extent *vext, vext_stats_t stats, void *arg);

#endif /* __KERNEL__ */

#endif
