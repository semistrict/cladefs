// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! virtio-pmem (virtio 1.2, section 5.19): persistent memory, a file of the host's mapped into the
//! guest's physical memory, which the guest reads and writes as memory (with DAX, its filesystem
//! with no page cache of its own), and whose flushes, its one request, sync the file. As
//! Firecracker's pmem devices are, which cladefs's guest tests measure ext4 on (`crate::Pmem`).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

use vm_memory::{Bytes, GuestMemoryMmap};

use super::queue::DeviceQueue;
use super::{DeviceState, VirtioDevice, not_snapshottable};
use crate::devices::pci::PciHost;
use crate::error::{Error, Result};

const VIRTIO_ID_PMEM: u16 = 27;
const QUEUE_SIZES: [u16; 1] = [256];
/// Its memory's alignment, and its size's multiple: virtio-pmem's, as Linux maps it.
pub(crate) const ALIGNMENT: u64 = 2 << 20;
/// Its one request.
const VIRTIO_PMEM_REQ_TYPE_FLUSH: u32 = 0;

/// A persistent memory device of a machine's: a file of the host's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pmem {
    /// The device's memory: the file's bytes, then zeros to a multiple of 2 MiB.
    pub path: std::path::PathBuf,
    /// Whether the guest may only read it.
    pub read_only: bool,
}

impl Pmem {
    /// The device of the file `path`, which the guest may write.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into(), read_only: false }
    }
}

/// The file mapped into this process: the file's bytes, then anonymous zeros to the device's
/// size. Unmapped on drop, after the VM, which drops first (`Live`'s field order).
struct Mapping {
    addr: NonNull<u8>,
    len: usize,
    file: File,
}

// SAFETY: An address range the guest reads and writes through the hypervisor; this process only
// syncs it.
unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: The mapping is this one's, which the VM gone maps no more.
        unsafe { libc::munmap(self.addr.as_ptr().cast(), self.len) };
    }
}

/// The device.
pub(crate) struct PmemDevice {
    mapping: Mapping,
    /// Where the guest finds it, and its size: its configuration.
    start: u64,
    size: u64,
}

impl PmemDevice {
    /// The device of `pmem`, its memory mapped into `vm`'s guest at `start`, a multiple of
    /// [`ALIGNMENT`]: its size, a multiple of [`ALIGNMENT`] too, is [`PmemDevice::size`].
    pub(crate) fn new(pmem: &Pmem, vm: &dyn PciHost, start: u64) -> Result<Self> {
        let invalid =
            |why: String| Error::InvalidConfig(format!("pmem {}: {why}", pmem.path.display()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!pmem.read_only)
            .open(&pmem.path)
            .map_err(|e| invalid(e.to_string()))?;
        let file_len = file.metadata().map_err(|e| invalid(e.to_string()))?.len();
        if file_len == 0 {
            return Err(invalid("it is empty".into()));
        }
        let size = file_len.next_multiple_of(ALIGNMENT);
        let len = usize::try_from(size).map_err(|e| invalid(e.to_string()))?;
        let file_len = usize::try_from(file_len).map_err(|e| invalid(e.to_string()))?;
        let prot = libc::PROT_READ | if pmem.read_only { 0 } else { libc::PROT_WRITE };
        // SAFETY: A new anonymous mapping of the device's size, whose result is checked; then the
        // file over its first bytes (MAP_FIXED, within it), the rest left zeros.
        let addr = unsafe {
            let whole = libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            if whole == libc::MAP_FAILED {
                return Err(invalid(std::io::Error::last_os_error().to_string()));
            }
            let over = libc::mmap(
                whole,
                file_len,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                file.as_raw_fd(),
                0,
            );
            if over == libc::MAP_FAILED {
                let err = std::io::Error::last_os_error();
                libc::munmap(whole, len);
                return Err(invalid(err.to_string()));
            }
            NonNull::new(whole.cast::<u8>()).expect("mmap's address")
        };
        let mapping = Mapping { addr, len, file };
        // SAFETY: The mapping stays until after the VM goes (see `Mapping`).
        unsafe { vm.map_memory(start, mapping.addr.as_ptr(), size) }?;
        Ok(Self { mapping, start, size })
    }

    /// Its size in bytes, a multiple of [`ALIGNMENT`].
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// Writes what the guest wrote back to the file: `msync` of the file's part of the mapping,
    /// as Firecracker's flush is.
    fn flush(&self) -> bool {
        let file_len = self.mapping.file.metadata().map(|m| m.len() as usize).unwrap_or(0);
        // SAFETY: Syncs a range of the device's own mapping.
        let synced = unsafe {
            libc::msync(
                self.mapping.addr.as_ptr().cast(),
                file_len.min(self.mapping.len),
                libc::MS_SYNC,
            )
        };
        synced == 0
    }
}

impl VirtioDevice for PmemDevice {
    fn device_type(&self) -> u16 {
        VIRTIO_ID_PMEM
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    /// `start` and `size`, little-endian, as virtio-pmem's configuration has them.
    fn config_size(&self) -> usize {
        16
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0u8; 16];
        config[..8].copy_from_slice(&self.start.to_le_bytes());
        config[8..].copy_from_slice(&self.size.to_le_bytes());
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset.checked_add(i).and_then(|at| config.get(at)).copied().unwrap_or(0);
        }
    }

    fn process_queue(&mut self, index: usize, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        let queue = &mut queues[index];
        queue.quiet(mem);
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head_index();
            let descriptors: Vec<_> = chain.collect();
            // The request's type, then its answer: 0 if it was done, -1 if not.
            let written = match descriptors.as_slice() {
                [request, response]
                    if !request.is_write_only()
                        && request.len() >= 4
                        && response.is_write_only()
                        && response.len() >= 4 =>
                {
                    let kind: u32 = mem.read_obj(request.addr()).unwrap_or(u32::MAX);
                    let done = kind == VIRTIO_PMEM_REQ_TYPE_FLUSH && self.flush();
                    let answer: i32 = if done { 0 } else { -1 };
                    match mem.write_obj(answer.to_le(), response.addr()) {
                        Ok(()) => 4,
                        Err(_) => 0,
                    }
                }
                _ => 0,
            };
            if !queue.add_used(mem, head, written) {
                return;
            }
        }
    }

    fn save(&self) -> Result<DeviceState> {
        Err(not_snapshottable(
            VIRTIO_ID_PMEM,
            "a pmem device's file is the host's, which no snapshot holds",
        ))
    }
}
