// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! virtio-rng (virtio 1.2, section 5.4): an entropy source, filled from the
//! host's `getrandom`. Every board machine has one, so the guest kernel seeds
//! its random number generator early instead of waiting for jitter.

use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use super::queue::DeviceQueue;
use super::{DeviceState, VirtioDevice, other_version};

const VIRTIO_ID_ENTROPY: u16 = 4;
const QUEUE_SIZES: [u16; 1] = [256];
/// The most bytes one request gets: a request for more is answered with
/// this many, as the device may (virtio 1.2, section 5.4.6.1).
const MAX_REQUEST: u32 = 64 << 10;
/// The version of the (empty) state it saves.
const STATE_VERSION: u32 = 1;

/// The entropy device.
pub(crate) struct Rng;

impl VirtioDevice for Rng {
    fn device_type(&self) -> u16 {
        VIRTIO_ID_ENTROPY
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    /// Nothing but its queue, which the transport saves.
    fn save(&self) -> crate::error::Result<DeviceState> {
        Ok(DeviceState { version: STATE_VERSION, data: Vec::new() })
    }

    fn restore(&mut self, state: &DeviceState) -> crate::error::Result<()> {
        if state.version != STATE_VERSION || !state.data.is_empty() {
            return Err(other_version(VIRTIO_ID_ENTROPY, state.version, STATE_VERSION));
        }
        Ok(())
    }

    fn process_queue(&mut self, index: usize, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        let queue = &mut queues[index];
        queue.quiet(mem);
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head_index();
            let mut written = 0;
            for desc in chain.writable() {
                let len = desc.len().min(MAX_REQUEST - written);
                let n = fill_random(mem, desc.addr(), len);
                written += n;
                if n < len || written == MAX_REQUEST {
                    break;
                }
            }
            if !queue.add_used(mem, head, written) {
                return;
            }
        }
    }
}

/// Fills `len` bytes of guest memory at `addr` with random bytes; returns
/// how many it wrote (fewer if the range leaves guest memory).
fn fill_random(mem: &GuestMemoryMmap, addr: GuestAddress, len: u32) -> u32 {
    let mut buf = [0u8; 4096];
    let mut done = 0u32;
    while done < len {
        let n = (len - done).min(buf.len() as u32) as usize;
        crate::sys::getrandom(&mut buf[..n]);
        if mem.write_slice(&buf[..n], addr.unchecked_add(u64::from(done))).is_err() {
            break;
        }
        done += n as u32;
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::test_driver::Driver;

    /// Every buffer is filled and used, and once they are all used the
    /// driver is asked to notify on its next one.
    #[test]
    fn fills_every_buffer_then_asks_for_the_next() {
        for event_idx in [true, false] {
            let mut d = Driver::new(1, event_idx);
            d.post(0, &[(vec![0; 32], true)]);
            d.post(0, &[(vec![0; 8], true), (vec![0; 8], true)]);
            Rng.process_queue(0, &mut d.queues, &d.mem);
            let used = d.used(0);
            assert_eq!(used.len(), 2);
            assert_eq!((used[0].1, used[1].1), (32, 16));
            assert_ne!(d.bytes(0, used[0].0, 32), vec![0; 32]);
            assert!(d.would_notify(0, event_idx), "event_idx {event_idx}");
        }
    }
}
