/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * The driver's self-test: writing to a device's `selftest` attribute in sysfs runs every request
 * of the specification on it, and reading the attribute gives the result, "ok" or what failed.
 * The write fails with EBUSY while the device is held, as a mounted filesystem holds it.
 */
#ifndef _VIRTIO_EXTENT_SELFTEST_H
#define _VIRTIO_EXTENT_SELFTEST_H

struct virtio_device;
struct virtio_extent;

int vext_selftest_add(struct virtio_extent *vext, struct virtio_device *vdev);
void vext_selftest_remove(struct virtio_device *vdev);

/*
 * The driver's (driver.c): the self-test holds its device alone, refused (-EBUSY) while anyone
 * holds it, as a mounted filesystem does, whose root the self-test would replace.
 */
int vext_selftest_begin(struct virtio_extent *vext);
void vext_selftest_end(struct virtio_extent *vext);

#endif
