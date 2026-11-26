// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A virtio device for tests: type 0x4658 (cladefs's extent device's, past
//! 63), a 16-byte device configuration, a shared memory region, two queues
//! it never uses; it counts its activations.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use vm_memory::{GuestMemoryMmap, MmapRegion};

use super::queue::DeviceQueue;
use super::{ShmRegion, VirtioDevice};

pub(crate) const TEST_DEVICE_TYPE: u16 = 0x4658;
pub(crate) const TEST_CONFIG: &[u8; 16] = b"libmachine-test!";

pub(crate) struct TestDevice {
    config: [u8; 16],
    shm: Vec<ShmRegion>,
    /// How many times the driver activated it.
    pub(crate) activations: Arc<AtomicUsize>,
}

impl TestDevice {
    /// The device, with `shm` as its shared memory region 0.
    pub(crate) fn new(shm: Arc<MmapRegion>) -> Self {
        Self {
            config: *TEST_CONFIG,
            shm: vec![ShmRegion { id: 0, memory: shm }],
            activations: Arc::default(),
        }
    }
}

impl VirtioDevice for TestDevice {
    fn device_type(&self) -> u16 {
        TEST_DEVICE_TYPE
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[16, 16]
    }

    fn config_size(&self) -> usize {
        self.config.len()
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            *b = self.config.get(offset + i).copied().unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            if let Some(c) = self.config.get_mut(offset + i) {
                *c = b;
            }
        }
    }

    fn shm_regions(&self) -> &[ShmRegion] {
        &self.shm
    }

    fn activate(&mut self, _features: u64) {
        self.activations.fetch_add(1, Ordering::Relaxed);
    }

    fn process_queue(&mut self, _: usize, _: &mut [DeviceQueue], _: &GuestMemoryMmap) {}
}
