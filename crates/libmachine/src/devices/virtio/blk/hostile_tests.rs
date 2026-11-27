// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A hostile guest's virtio-blk chains (board 14346): lengths at a u32's edges, buffers that are no
//! guest memory, a status descriptor holding no byte. The device answers each as the protocol
//! says, holds no more than [`CHUNK`] bytes of a request at once, and never wraps a used length.

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::*;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
/// The device's own queue size ([`QUEUE_SIZES`]), a chain's most descriptors.
const SIZE: u16 = 256;
const MEMORY: usize = 64 << 20;
const DESC: u64 = 0x1_0000;
const AVAIL: u64 = 0x2_0000;
const USED: u64 = 0x3_0000;
const HEADER: u64 = 0x4_0000;
const STATUS: u64 = 0x5_0000;
/// Where every data descriptor points: several may name the same guest memory.
const DATA: u64 = 0x10_0000;
/// No guest memory: past the guest's.
const NOWHERE: u64 = 1 << 40;
/// A data descriptor's length, 254 of which pass 4 GiB.
const EACH: u32 = 16_909_321;

/// A guest's memory and its one queue, of the device's size, ready.
fn guest() -> (GuestMemoryMmap, Vec<DeviceQueue>) {
    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEMORY)]).unwrap();
    let mut queue = DeviceQueue::new(SIZE).unwrap();
    queue.set_desc_table_address(Some(DESC as u32), Some(0));
    queue.set_avail_ring_address(Some(AVAIL as u32), Some(0));
    queue.set_used_ring_address(Some(USED as u32), Some(0));
    queue.set_ready(true);
    assert!(queue.is_valid(&mem));
    (mem, vec![queue])
}

/// Posts one chain of `kind` at sector 0: its header, `data` (address, length), each
/// device-writable for a read and read-only otherwise, and a status descriptor of `status` bytes,
/// which holds 0xff until the device writes it.
fn post(mem: &GuestMemoryMmap, kind: u32, data: &[(u64, u32)], status: u32) {
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(&kind.to_le_bytes());
    mem.write_slice(&header, GuestAddress(HEADER)).unwrap();
    mem.write_obj(0xffu8, GuestAddress(STATUS)).unwrap();
    let flags = if kind == VIRTIO_BLK_T_IN { DESC_F_WRITE } else { 0 };
    let mut chain = vec![(HEADER, 16u32, 0u16)];
    chain.extend(data.iter().map(|&(addr, len)| (addr, len, flags)));
    chain.push((STATUS, status, DESC_F_WRITE));
    assert!(chain.len() <= usize::from(SIZE));
    for (i, &(addr, len, flags)) in chain.iter().enumerate() {
        let last = i + 1 == chain.len();
        let mut d = Vec::new();
        d.extend_from_slice(&addr.to_le_bytes());
        d.extend_from_slice(&len.to_le_bytes());
        d.extend_from_slice(&(flags | if last { 0 } else { DESC_F_NEXT }).to_le_bytes());
        d.extend_from_slice(&(i as u16 + 1).to_le_bytes());
        mem.write_slice(&d, GuestAddress(DESC + 16 * i as u64)).unwrap();
    }
    // The avail ring: its head, descriptor 0, and its index, 1.
    mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();
    mem.write_obj(1u16, GuestAddress(AVAIL + 2)).unwrap();
}

/// The used ring's entries, (head, length), and the status byte.
fn answered(mem: &GuestMemoryMmap) -> (Vec<(u32, u32)>, u8) {
    let idx: u16 = mem.read_obj(GuestAddress(USED + 2)).unwrap();
    let used = (0..u64::from(idx))
        .map(|i| {
            let at = USED + 4 + 8 * i;
            (mem.read_obj(GuestAddress(at)).unwrap(), mem.read_obj(GuestAddress(at + 4)).unwrap())
        })
        .collect();
    (used, mem.read_obj(GuestAddress(STATUS)).unwrap())
}

/// A sparse disk of `len` bytes.
fn disk(dir: &std::path::Path, len: u64) -> Block {
    let path = dir.join("disk.img");
    std::fs::File::create(&path).unwrap().set_len(len).unwrap();
    Block::new(&Disk::new(&path)).unwrap()
}

/// This process's peak resident memory, in bytes (`ru_maxrss`, bytes on macOS).
fn peak_resident() -> u64 {
    // SAFETY: Fills a struct of this frame's.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage
    };
    usage.ru_maxrss as u64
}

/// Serves the chain `post` made on a fresh queue over a disk of `disk_len` bytes: what was
/// answered, and how many MiB the process's peak resident memory grew meanwhile.
fn serve(
    disk_len: u64,
    kind: u32,
    data: &[(u64, u32)],
    status: u32,
) -> ((Vec<(u32, u32)>, u8), u64) {
    let dir = tempfile::tempdir().unwrap();
    let mut block = disk(dir.path(), disk_len);
    let (mem, mut queues) = guest();
    post(&mem, kind, data, status);
    let before = peak_resident();
    block.process_queue(0, &mut queues, &mem);
    (answered(&mem), (peak_resident() - before) >> 20)
}

/// Reads whose data totals the edges of what a used length holds: none, one byte, the most a
/// chain holds with its header and status byte (`u32::MAX - 17`), and past a u32, which
/// virtio-queue ends where its descriptors' lengths pass one (virtio 1.2, 2.7.5.2), the last
/// descriptor it hands taken for the status byte. Each is used with what it wrote, the status
/// byte with it, and no length wraps.
#[test]
fn reads_at_a_u32s_edges_are_used_with_what_they_wrote() {
    let most = u32::MAX - 17;
    let rest = most - 253 * EACH;
    let mut longest = vec![(DATA, EACH); 253];
    longest.push((DATA, rest));
    let disk = 5 << 30;
    assert_eq!(serve(disk, VIRTIO_BLK_T_IN, &[], 1).0, (vec![(0, 1)], VIRTIO_BLK_S_OK));
    assert_eq!(serve(disk, VIRTIO_BLK_T_IN, &[(DATA, 1)], 1).0, (vec![(0, 2)], VIRTIO_BLK_S_OK));
    assert_eq!(serve(disk, VIRTIO_BLK_T_IN, &longest, 1).0, (vec![(0, most + 1)], VIRTIO_BLK_S_OK));
    assert!(254 * u64::from(EACH) >= 1 << 32);
    // The header and 253 of the 254: the last of those taken for the status, 252 read.
    let (past, _) = serve(disk, VIRTIO_BLK_T_IN, &[(DATA, EACH); 254], 1);
    assert_eq!(past.0, [(0, 252 * EACH + 1)]);
}

/// A chain whose status descriptor holds no byte is no request: used with nothing written, its
/// status left as it was, and nothing read.
#[test]
fn a_status_descriptor_of_no_byte_is_no_request() {
    let (answer, grew_mib) = serve(1 << 30, VIRTIO_BLK_T_IN, &[(DATA, 1 << 20)], 0);
    assert_eq!((answer, grew_mib), ((vec![(0, 0)], 0xff), 0));
}

/// A read or a write of 1 GiB whose buffer is no guest memory fails, as it must, without the
/// device holding its length in memory or touching the disk for it: the process grows by no MiB
/// in 64.
#[test]
fn buffers_that_are_no_guest_memory_hold_nothing_of_their_length() {
    for kind in [VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT] {
        let (answer, grew_mib) = serve(1 << 30, kind, &[(NOWHERE, 1 << 30)], 1);
        assert_eq!((answer, grew_mib >> 6), ((vec![(0, 1)], VIRTIO_BLK_S_IOERR), 0), "{kind}");
    }
}

/// A read of guest memory larger than [`CHUNK`], in pieces, and of the whole longest chain, which
/// names the same 16 MiB over and over: the process grows by no more than its chunk, in units of
/// 64 MiB, and the bytes read are the disk's.
#[test]
fn long_reads_go_through_one_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.img");
    let pattern: Vec<u8> = (0..3 * CHUNK + 5).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &pattern).unwrap();
    std::fs::File::options().write(true).open(&path).unwrap().set_len(5 << 30).unwrap();
    let mut block = Block::new(&Disk::new(&path)).unwrap();
    let (mem, mut queues) = guest();
    let len = pattern.len() as u32;
    post(&mem, VIRTIO_BLK_T_IN, &[(DATA, len)], 1);
    block.process_queue(0, &mut queues, &mem);
    assert_eq!(answered(&mem), (vec![(0, len + 1)], VIRTIO_BLK_S_OK));
    let mut read = vec![0; pattern.len()];
    mem.read_slice(&mut read, GuestAddress(DATA)).unwrap();
    assert!(read == pattern, "the bytes read are not the disk's");

    let (_, grew_mib) = serve(5 << 30, VIRTIO_BLK_T_IN, &[(DATA, EACH); 253], 1);
    assert_eq!(grew_mib >> 6, 0);
}
