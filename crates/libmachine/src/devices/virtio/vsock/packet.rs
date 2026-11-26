// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The virtio-vsock packet header (virtio 1.2, section 5.10.6), as it
//! crosses the rx and tx queues: 44 little-endian bytes, then `len` bytes of
//! payload.

/// The header's size on the queues.
pub(crate) const HEADER_LEN: usize = 44;
/// The most payload one packet carries: what Linux sends and accepts
/// (VIRTIO_VSOCK_MAX_PKT_BUF_SIZE). A tx packet claiming more is dropped.
pub(crate) const MAX_PAYLOAD: usize = 64 << 10;

/// VIRTIO_VSOCK_TYPE_STREAM, the only socket type the device offers.
pub(crate) const TYPE_STREAM: u16 = 1;

/// SHUTDOWN flags: the sender will receive no more, send no more.
pub(crate) const SHUTDOWN_RCV: u32 = 1;
pub(crate) const SHUTDOWN_SEND: u32 = 2;
pub(crate) const SHUTDOWN_BOTH: u32 = SHUTDOWN_RCV | SHUTDOWN_SEND;

/// The operations (`op`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Request,
    Response,
    Rst,
    Shutdown,
    Rw,
    CreditUpdate,
    CreditRequest,
    /// An op this device does not know; answered with a reset.
    Unknown(u16),
}

impl Op {
    fn from_wire(op: u16) -> Self {
        match op {
            1 => Op::Request,
            2 => Op::Response,
            3 => Op::Rst,
            4 => Op::Shutdown,
            5 => Op::Rw,
            6 => Op::CreditUpdate,
            7 => Op::CreditRequest,
            other => Op::Unknown(other),
        }
    }

    fn to_wire(self) -> u16 {
        match self {
            Op::Request => 1,
            Op::Response => 2,
            Op::Rst => 3,
            Op::Shutdown => 4,
            Op::Rw => 5,
            Op::CreditUpdate => 6,
            Op::CreditRequest => 7,
            Op::Unknown(op) => op,
        }
    }
}

/// A packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) src_cid: u64,
    pub(crate) dst_cid: u64,
    pub(crate) src_port: u32,
    pub(crate) dst_port: u32,
    /// The payload's length.
    pub(crate) len: u32,
    pub(crate) socket_type: u16,
    pub(crate) op: Op,
    pub(crate) flags: u32,
    /// The sender's receive buffer for this connection, in bytes.
    pub(crate) buf_alloc: u32,
    /// How many bytes the sender has taken out of that buffer, ever.
    pub(crate) fwd_cnt: u32,
}

impl Header {
    pub(crate) fn decode(b: &[u8; HEADER_LEN]) -> Self {
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Self {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            socket_type: u16_at(28),
            op: Op::from_wire(u16_at(30)),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }

    pub(crate) fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0; HEADER_LEN];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.socket_type.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_wire().to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_in_the_spec_layout() {
        let h = Header {
            src_cid: 3,
            dst_cid: 2,
            src_port: 0x1234_5678,
            dst_port: 1024,
            len: 77,
            socket_type: TYPE_STREAM,
            op: Op::Rw,
            flags: SHUTDOWN_SEND,
            buf_alloc: 256 << 10,
            fwd_cnt: 0xdead_beef,
        };
        let b = h.encode();
        assert_eq!(&b[0..8], &3u64.to_le_bytes());
        assert_eq!(&b[16..20], &0x1234_5678u32.to_le_bytes());
        assert_eq!(&b[28..32], &[1, 0, 5, 0], "type, then op");
        assert_eq!(&b[40..44], &0xdead_beefu32.to_le_bytes());
        assert_eq!(Header::decode(&b), h);
        for op in 1..=8u16 {
            assert_eq!(Op::from_wire(op).to_wire(), op);
        }
        assert_eq!(Op::from_wire(8), Op::Unknown(8));
    }
}
