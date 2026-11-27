// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! virtio-blk (virtio 1.2, section 5.2): a disk, a file of the host's, read and written as the
//! guest asks, on the device thread ([`crate::Disk`]): a root filesystem's image, or a disk a test
//! measures against cladefs's extent store, as Firecracker's drives are.

use std::fs::File;
use std::os::unix::fs::FileExt;

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::queue::DeviceQueue;
use super::{DeviceState, VirtioDevice, other_version};
use crate::error::{Error, Result};

const VIRTIO_ID_BLOCK: u16 = 2;
/// The version of what a disk saves of itself in a snapshot: nothing, its file being the host's
/// (a restore opens it again, where the snapshot's header says), and its requests served by the
/// time the device thread is held off.
const STATE_VERSION: u32 = 1;
const QUEUE_SIZES: [u16; 1] = [256];
const SECTOR: u64 = 512;

/// Its features: read-only (`VIRTIO_BLK_F_RO`), and a flush request (`VIRTIO_BLK_F_FLUSH`).
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

/// The requests (virtio 1.2, section 5.2.6).
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;
/// A request's status, its last byte.
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;
/// The bytes of a GET_ID's answer.
const ID_BYTES: usize = 20;

/// A disk of a machine's: a file of the host's, a whole number of 512-byte sectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// The disk's image.
    pub path: std::path::PathBuf,
    /// Whether the guest may only read it.
    pub read_only: bool,
    /// Whether its flushes are dropped, as Firecracker's `Unsafe` cache type's are: a host's
    /// crash loses what the guest flushed since the host last wrote it back.
    pub unsafe_flushes: bool,
    /// What the guest's GET_ID answers, as Firecracker's `drive_id`: 20 bytes at most.
    pub id: String,
}

impl Disk {
    /// The disk `path`, which the guest may write and whose flushes reach the host's disk.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let id = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Self { path, read_only: false, unsafe_flushes: false, id }
    }
}

/// The disk device.
pub(crate) struct Block {
    file: File,
    /// Its size in bytes, a multiple of [`SECTOR`].
    len: u64,
    read_only: bool,
    unsafe_flushes: bool,
    id: [u8; ID_BYTES],
}

impl Block {
    /// The device of `disk`, its file opened as it may be written.
    pub(crate) fn new(disk: &Disk) -> Result<Self> {
        let invalid =
            |why: String| Error::InvalidConfig(format!("disk {}: {why}", disk.path.display()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!disk.read_only)
            .open(&disk.path)
            .map_err(|e| invalid(e.to_string()))?;
        let len = file.metadata().map_err(|e| invalid(e.to_string()))?.len();
        if len == 0 || !len.is_multiple_of(SECTOR) {
            return Err(invalid(format!("{len} bytes, not a nonzero multiple of {SECTOR}")));
        }
        if disk.id.len() > ID_BYTES {
            return Err(invalid(format!("its ID {:?} is past {ID_BYTES} bytes", disk.id)));
        }
        let mut id = [0; ID_BYTES];
        id[..disk.id.len()].copy_from_slice(disk.id.as_bytes());
        Ok(Self { file, len, read_only: disk.read_only, unsafe_flushes: disk.unsafe_flushes, id })
    }

    /// Serves one request of type `kind` at `sector` over the buffers `data`: its status, and the
    /// bytes it wrote into the guest's buffers.
    fn serve(&self, mem: &GuestMemoryMmap, kind: u32, sector: u64, data: &[Segment]) -> (u8, u32) {
        let total: u64 = data.iter().map(|s| u64::from(s.len)).sum();
        // The request's bytes, within the disk.
        let within = || {
            let start = sector.checked_mul(SECTOR)?;
            (start.checked_add(total)? <= self.len).then_some(start)
        };
        match kind {
            VIRTIO_BLK_T_IN => {
                let Some(mut at) = within() else { return (VIRTIO_BLK_S_IOERR, 0) };
                let mut buf = Vec::new();
                for seg in data.iter().filter(|s| s.writable) {
                    buf.resize(seg.len as usize, 0);
                    if self.file.read_exact_at(&mut buf, at).is_err()
                        || mem.write_slice(&buf, seg.addr).is_err()
                    {
                        return (VIRTIO_BLK_S_IOERR, 0);
                    }
                    at += u64::from(seg.len);
                }
                (VIRTIO_BLK_S_OK, u32::try_from(total).unwrap_or(u32::MAX))
            }
            VIRTIO_BLK_T_OUT if self.read_only => (VIRTIO_BLK_S_IOERR, 0),
            VIRTIO_BLK_T_OUT => {
                let Some(mut at) = within() else { return (VIRTIO_BLK_S_IOERR, 0) };
                let mut buf = Vec::new();
                for seg in data.iter().filter(|s| !s.writable) {
                    buf.resize(seg.len as usize, 0);
                    if mem.read_slice(&mut buf, seg.addr).is_err()
                        || self.file.write_all_at(&buf, at).is_err()
                    {
                        return (VIRTIO_BLK_S_IOERR, 0);
                    }
                    at += u64::from(seg.len);
                }
                (VIRTIO_BLK_S_OK, 0)
            }
            VIRTIO_BLK_T_FLUSH if self.unsafe_flushes => (VIRTIO_BLK_S_OK, 0),
            VIRTIO_BLK_T_FLUSH => match self.file.sync_data() {
                Ok(()) => (VIRTIO_BLK_S_OK, 0),
                Err(_) => (VIRTIO_BLK_S_IOERR, 0),
            },
            VIRTIO_BLK_T_GET_ID => match data.iter().find(|s| s.writable) {
                Some(seg) => {
                    let n = (seg.len as usize).min(ID_BYTES);
                    match mem.write_slice(&self.id[..n], seg.addr) {
                        Ok(()) => (VIRTIO_BLK_S_OK, n as u32),
                        Err(_) => (VIRTIO_BLK_S_IOERR, 0),
                    }
                }
                None => (VIRTIO_BLK_S_IOERR, 0),
            },
            _ => (VIRTIO_BLK_S_UNSUPP, 0),
        }
    }
}

/// A data buffer of a request.
struct Segment {
    addr: GuestAddress,
    len: u32,
    writable: bool,
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u16 {
        VIRTIO_ID_BLOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        VIRTIO_BLK_F_FLUSH | if self.read_only { VIRTIO_BLK_F_RO } else { 0 }
    }

    /// `capacity`, in sectors: the only field of its configuration it has a feature for.
    fn config_size(&self) -> usize {
        8
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let config = (self.len / SECTOR).to_le_bytes();
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
            // The header, the data, then the status byte (virtio 1.2, section 5.2.6): what the
            // device wrote, the status byte with it; a chain not so is used with nothing written.
            let written = match descriptors.as_slice() {
                [header, data @ .., status]
                    if !header.is_write_only() && status.is_write_only() && header.len() >= 16 =>
                {
                    let mut bytes = [0u8; 16];
                    let (code, written) = match mem.read_slice(&mut bytes, header.addr()) {
                        Ok(()) => {
                            let kind = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
                            let sector = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
                            let data: Vec<Segment> = (data.iter())
                                .map(|d| Segment {
                                    addr: d.addr(),
                                    len: d.len(),
                                    writable: d.is_write_only(),
                                })
                                .collect();
                            self.serve(mem, kind, sector, &data)
                        }
                        Err(_) => (VIRTIO_BLK_S_IOERR, 0),
                    };
                    match mem.write_obj(code, status.addr()) {
                        Ok(()) => written + 1,
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
        Ok(DeviceState { version: STATE_VERSION, data: Vec::new() })
    }

    fn restore(&mut self, state: &DeviceState) -> Result<()> {
        if state.version != STATE_VERSION || !state.data.is_empty() {
            return Err(other_version(VIRTIO_ID_BLOCK, state.version, STATE_VERSION));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::test_driver::Driver;

    /// A write then a read of the same sectors gives back what was written, a flush succeeds, a
    /// request past the disk's end fails, and GET_ID answers the disk's ID.
    #[test]
    fn reads_what_it_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        std::fs::File::create(&path).unwrap().set_len(1 << 20).unwrap();
        let mut disk = Disk::new(&path);
        disk.id = "rootfs".into();
        let mut block = Block::new(&disk).unwrap();
        let header = |kind: u32, sector: u64| {
            let mut h = vec![0u8; 16];
            h[0..4].copy_from_slice(&kind.to_le_bytes());
            h[8..16].copy_from_slice(&sector.to_le_bytes());
            h
        };
        let mut d = Driver::new(1, false);
        d.post(0, &[(header(VIRTIO_BLK_T_OUT, 3), false), (vec![7; 1024], false), (vec![9], true)]);
        d.post(0, &[(header(VIRTIO_BLK_T_IN, 3), false), (vec![0; 1024], true), (vec![9], true)]);
        d.post(0, &[(header(VIRTIO_BLK_T_FLUSH, 0), false), (vec![9], true)]);
        d.post(
            0,
            &[(header(VIRTIO_BLK_T_IN, 2047), false), (vec![0; 1024], true), (vec![9], true)],
        );
        d.post(0, &[(header(VIRTIO_BLK_T_GET_ID, 0), false), (vec![0; 20], true), (vec![9], true)]);
        block.process_queue(0, &mut d.queues, &d.mem);
        let used = d.used(0);
        assert_eq!(used.len(), 5);
        // What each wrote: its status byte, and a read's data before it.
        assert_eq!((used[0].1, used[1].1, used[2].1, used[3].1, used[4].1), (1, 1025, 1, 1, 21));
        // Each chain's bytes: its header, its data, its status byte, OK but for the read past
        // the disk's end.
        let status =
            |i: usize, data: usize| d.bytes(0, used[i].0, (16 + data + 1) as u32)[16 + data];
        let statuses =
            [status(0, 1024), status(1, 1024), status(2, 0), status(3, 1024), status(4, 20)];
        assert_eq!(
            statuses,
            [
                VIRTIO_BLK_S_OK,
                VIRTIO_BLK_S_OK,
                VIRTIO_BLK_S_OK,
                VIRTIO_BLK_S_IOERR,
                VIRTIO_BLK_S_OK
            ]
        );
        assert_eq!(d.bytes(0, used[1].0, 16 + 1024)[16..], [7; 1024]);
        let mut id = b"rootfs".to_vec();
        id.resize(20, 0);
        assert_eq!(d.bytes(0, used[4].0, 16 + 20)[16..], id[..]);
    }
}
