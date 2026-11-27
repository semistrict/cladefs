// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The vhost-user protocol's front-end messages, as QEMU's `docs/interop/vhost-user.rst` specifies
//! them: their header, payloads and constants, and the decoder that makes each message a
//! [`Request`] that means something, or refuses it. The front-end is an untrusted, jailed process,
//! and one daemon serves every tenant's (AGENTS.md, "Trust boundaries"): every field is checked
//! here, once, with no unchecked arithmetic or indexing, and code past the decoder relies on it.
//!
//! Written for cladefs rather than taken from rust-vmm's `vhost` crate (hotbed question recorded on
//! board 14537): the few structures and constants a back-end needs, decoded into values, with no
//! endpoint machinery around them.
//!
//! Every multi-byte field is little-endian, as the specification has it (x86-64 and arm64 hosts).

#![deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use std::os::fd::OwnedFd;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The header's version bits, the one version there is.
pub const VERSION: u32 = 0x1;
/// The header's flags: the version's two bits, then a reply's flag, then a request's asking for a
/// reply (`REPLY_ACK`).
pub const VERSION_MASK: u32 = 0x3;
pub const REPLY_FLAG: u32 = 1 << 2;
pub const NEED_REPLY_FLAG: u32 = 1 << 3;

/// The front-end's requests this back-end knows (the specification's numbers).
pub const GET_FEATURES: u32 = 1;
pub const SET_FEATURES: u32 = 2;
pub const SET_OWNER: u32 = 3;
pub const SET_MEM_TABLE: u32 = 5;
pub const SET_VRING_NUM: u32 = 8;
pub const SET_VRING_ADDR: u32 = 9;
pub const SET_VRING_BASE: u32 = 10;
pub const GET_VRING_BASE: u32 = 11;
pub const SET_VRING_KICK: u32 = 12;
pub const SET_VRING_CALL: u32 = 13;
pub const SET_VRING_ERR: u32 = 14;
pub const GET_PROTOCOL_FEATURES: u32 = 15;
pub const SET_PROTOCOL_FEATURES: u32 = 16;
pub const GET_QUEUE_NUM: u32 = 17;
pub const SET_VRING_ENABLE: u32 = 18;
pub const SET_BACKEND_REQ_FD: u32 = 21;
pub const GET_CONFIG: u32 = 24;
pub const RESET_DEVICE: u32 = 34;

/// The back-end's requests on its back-end channel (`SET_BACKEND_REQ_FD`): the device's
/// configuration changed, for the front-end to tell the guest (its configuration change
/// interrupt), which reads it again (`GET_CONFIG`).
pub const BACKEND_CONFIG_CHANGE_MSG: u32 = 2;

/// `VHOST_USER_F_PROTOCOL_FEATURES`: a device feature bit, saying the back-end has protocol
/// features; negotiated, rings start disabled until `SET_VRING_ENABLE`.
pub const F_PROTOCOL_FEATURES: u32 = 30;

/// Protocol features (`VHOST_USER_PROTOCOL_F_*`) this back-end offers. cladefs: board 14537's,
/// and the back-end channel (board 14603); `DEVICE_STATE` (19), `SHMEM` (22) and doc-32's
/// `SHMEM_LAZY` come with the next milestones.
pub const PROTOCOL_F_MQ: u32 = 0;
pub const PROTOCOL_F_REPLY_ACK: u32 = 3;
pub const PROTOCOL_F_BACKEND_REQ: u32 = 5;
pub const PROTOCOL_F_CONFIG: u32 = 9;
pub const PROTOCOL_F_RESET_DEVICE: u32 = 13;

/// The protocol features offered, as a mask.
pub const PROTOCOL_FEATURES: u64 = (1 << PROTOCOL_F_MQ)
    | (1 << PROTOCOL_F_REPLY_ACK)
    | (1 << PROTOCOL_F_BACKEND_REQ)
    | (1 << PROTOCOL_F_CONFIG)
    | (1 << PROTOCOL_F_RESET_DEVICE);

/// A `SET_VRING_KICK`'s, `SET_VRING_CALL`'s or `SET_VRING_ERR`'s payload: the queue's index in its
/// low byte; this bit set if the message carries no descriptor (a ring polled, or not signalled).
pub const VRING_INDEX_MASK: u64 = 0xff;
pub const VRING_NOFD_MASK: u64 = 1 << 8;

/// A `SET_VRING_ADDR`'s flag: the ring's used ring is logged (`VHOST_F_LOG_ALL`), which this
/// back-end doesn't offer.
pub const VRING_F_LOG: u32 = 1 << 0;

/// The most regions a memory table has without `CONFIGURE_MEM_SLOTS`, which isn't offered, and so
/// the most descriptors a message carries.
pub const MAX_REGIONS: usize = 8;
pub const MAX_FDS: usize = MAX_REGIONS;

/// The most bytes of a device's configuration a `GET_CONFIG` reads at once.
pub const MAX_CONFIG_SIZE: u32 = 256;

/// The most bytes a message's payload has: past it, the channel ends at the header, before its
/// payload is read. Room for the largest a request has, a memory table's or a `GET_CONFIG`'s, and
/// for the later milestones' messages.
pub const MAX_PAYLOAD: u32 = 4096;

/// Every message's header.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Header {
    pub request: u32,
    pub flags: u32,
    pub size: u32,
}

/// A payload of one 64-bit word.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct U64 {
    pub value: u64,
}

/// A ring's index and a number for it (`struct vhost_vring_state`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct VringState {
    pub index: u32,
    pub num: u32,
}

/// A ring's addresses, in the front-end's address space (`struct vhost_vring_addr`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct VringAddrWire {
    pub index: u32,
    pub flags: u32,
    pub desc: u64,
    pub used: u64,
    pub avail: u64,
    pub log: u64,
}

/// A memory table's header, before its regions.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct MemoryHeader {
    pub regions: u32,
    pub padding: u32,
}

/// A region of guest memory: where it is in the guest and in the front-end, its size, and its
/// offset in the descriptor that holds it.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RegionWire {
    pub guest: u64,
    pub size: u64,
    pub user: u64,
    pub offset: u64,
}

/// A `GET_CONFIG`'s header, before the configuration's bytes.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct ConfigHeader {
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
}

/// A ring's addresses, decoded: in the front-end's address space, which the memory table
/// translates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VringAddr {
    pub desc: u64,
    pub used: u64,
    pub avail: u64,
}

/// A region of a memory table, decoded, with the descriptor of the memory holding it.
#[derive(Debug)]
pub struct Region {
    pub guest: u64,
    pub size: u64,
    pub user: u64,
    pub offset: u64,
    pub fd: OwnedFd,
}

/// A front-end's request, decoded: every field within what the protocol allows, a queue's index
/// among them within the device's queues, and every descriptor where the request carries one
/// (a back-end channel's, `SetBackendReqFd`, checked a Unix stream socket's as it is served).
#[derive(Debug)]
pub enum Request {
    GetFeatures,
    SetFeatures(u64),
    SetOwner,
    SetMemTable(Vec<Region>),
    SetVringNum { index: u16, num: u16 },
    SetVringAddr { index: u16, addr: VringAddr },
    SetVringBase { index: u16, base: u16 },
    GetVringBase { index: u16 },
    SetVringKick { index: u16, fd: OwnedFd },
    SetVringCall { index: u16, fd: OwnedFd },
    SetVringErr { index: u16, fd: OwnedFd },
    GetProtocolFeatures,
    SetProtocolFeatures(u64),
    GetQueueNum,
    SetVringEnable { index: u16, enable: bool },
    GetConfig { offset: u32, size: u32 },
    ResetDevice,
    SetBackendReqFd(OwnedFd),
}

/// A message received whole: its header, payload and descriptors.
#[derive(Debug)]
pub struct Message {
    pub header: Header,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// Why a message is refused. Each ends the channel, unless the message asked for a reply
/// (`REPLY_ACK`) and was framed as its request's are ([`Refused::answerable`]).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refused {
    #[error("a header of flags {0:#x}: no version 1 request")]
    Header(u32),
    #[error("request {0}, which this back-end serves none of")]
    Unknown(u32),
    #[error("request {request} of {size} payload bytes, where it has {expected}")]
    Size { request: u32, size: usize, expected: usize },
    #[error("request {request} carrying {fds} descriptors, where it carries {expected}")]
    Fds { request: u32, fds: usize, expected: usize },
    #[error("request {request} for queue {index}, of the device's {queues}")]
    Queue { request: u32, index: u64, queues: u16 },
    #[error("request {request}: {why}")]
    Value { request: u32, why: &'static str },
}

impl Refused {
    /// Whether a reply saying so answers it: a well-framed request of a value the back-end
    /// refuses, which leaves the channel in step; a malformed one doesn't.
    pub fn answerable(&self) -> bool {
        matches!(self, Refused::Queue { .. } | Refused::Value { .. })
    }
}

/// `T`, the whole of `payload`.
fn whole<T: FromBytes>(request: u32, payload: &[u8]) -> Result<T, Refused> {
    T::read_from_bytes(payload).map_err(|_| Refused::Size {
        request,
        size: payload.len(),
        expected: size_of::<T>(),
    })
}

/// A queue's index, `index`, within the device's `queues`.
fn queue(request: u32, index: u64, queues: u16) -> Result<u16, Refused> {
    u16::try_from(index).ok().filter(|index| *index < queues).ok_or(Refused::Queue {
        request,
        index,
        queues,
    })
}

/// The one descriptor `fds` holds, or none if `nofd`.
fn one_fd(request: u32, fds: Vec<OwnedFd>, nofd: bool) -> Result<Option<OwnedFd>, Refused> {
    let expected = usize::from(!nofd);
    if fds.len() != expected {
        return Err(Refused::Fds { request, fds: fds.len(), expected });
    }
    Ok(fds.into_iter().next())
}

impl Header {
    /// Whether it asks for a reply-ack.
    pub fn need_reply(&self) -> bool {
        self.flags & NEED_REPLY_FLAG != 0
    }

    /// `Ok` if it heads a request of version 1, of no flags but those, with a payload of
    /// [`MAX_PAYLOAD`] bytes at most: checked before its payload is read.
    pub fn check(&self) -> Result<(), Refused> {
        let flags = self.flags;
        if flags & VERSION_MASK != VERSION
            || flags & !(VERSION_MASK | NEED_REPLY_FLAG) != 0
            || self.size > MAX_PAYLOAD
        {
            return Err(Refused::Header(flags));
        }
        Ok(())
    }

    /// The header of a reply to it, of `size` payload bytes.
    pub fn reply(&self, size: usize) -> Header {
        Header {
            request: self.request,
            flags: VERSION | REPLY_FLAG,
            size: u32::try_from(size).expect("a reply's payload is small"),
        }
    }
}

impl Request {
    /// Decodes `message`, a request of a device of `queues` queues: refused unless it means
    /// something. Its header is checked ([`Header::check`]).
    pub fn decode(message: Message, queues: u16) -> Result<Request, Refused> {
        let Message { header, payload, fds } = message;
        let request = header.request;
        let no_fds = |fds: &Vec<OwnedFd>| match fds.len() {
            0 => Ok(()),
            n => Err(Refused::Fds { request, fds: n, expected: 0 }),
        };
        let empty = |payload: &Vec<u8>| match payload.len() {
            0 => Ok(()),
            n => Err(Refused::Size { request, size: n, expected: 0 }),
        };
        let value = |why| Refused::Value { request, why };
        Ok(match request {
            GET_FEATURES | GET_PROTOCOL_FEATURES | GET_QUEUE_NUM | SET_OWNER | RESET_DEVICE => {
                empty(&payload)?;
                no_fds(&fds)?;
                match request {
                    GET_FEATURES => Request::GetFeatures,
                    GET_PROTOCOL_FEATURES => Request::GetProtocolFeatures,
                    GET_QUEUE_NUM => Request::GetQueueNum,
                    SET_OWNER => Request::SetOwner,
                    _ => Request::ResetDevice,
                }
            }
            SET_FEATURES | SET_PROTOCOL_FEATURES => {
                let word: U64 = whole(request, &payload)?;
                no_fds(&fds)?;
                match request {
                    SET_FEATURES => Request::SetFeatures(u64::from_le(word.value)),
                    _ => Request::SetProtocolFeatures(u64::from_le(word.value)),
                }
            }
            SET_MEM_TABLE => {
                let (memory, rest) =
                    MemoryHeader::read_from_prefix(&payload).map_err(|_| Refused::Size {
                        request,
                        size: payload.len(),
                        expected: size_of::<MemoryHeader>(),
                    })?;
                let count = u32::from_le(memory.regions);
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                if count == 0 || count > MAX_REGIONS {
                    return Err(value("a memory table of no regions, or more than 8"));
                }
                if u32::from_le(memory.padding) != 0 {
                    return Err(value("a memory table's padding not zero"));
                }
                // At most 8 regions of 32 bytes: no product overflows.
                let regions_len = count.saturating_mul(size_of::<RegionWire>());
                if rest.len() != regions_len {
                    return Err(Refused::Size {
                        request,
                        size: payload.len(),
                        expected: size_of::<MemoryHeader>().saturating_add(regions_len),
                    });
                }
                if fds.len() != count {
                    return Err(Refused::Fds { request, fds: fds.len(), expected: count });
                }
                let regions = rest
                    .chunks_exact(size_of::<RegionWire>())
                    .map(|bytes| RegionWire::read_from_bytes(bytes).expect("a region's bytes"))
                    .zip(fds)
                    .map(|(region, fd)| Region {
                        guest: u64::from_le(region.guest),
                        size: u64::from_le(region.size),
                        user: u64::from_le(region.user),
                        offset: u64::from_le(region.offset),
                        fd,
                    })
                    .collect();
                Request::SetMemTable(regions)
            }
            SET_VRING_NUM | SET_VRING_BASE | GET_VRING_BASE | SET_VRING_ENABLE => {
                let state: VringState = whole(request, &payload)?;
                no_fds(&fds)?;
                let index = queue(request, u64::from(u32::from_le(state.index)), queues)?;
                let num = u32::from_le(state.num);
                match request {
                    SET_VRING_NUM => {
                        let num = u16::try_from(num)
                            .map_err(|_| value("a ring of more than 65535 entries"))?;
                        Request::SetVringNum { index, num }
                    }
                    // A split ring's next index: a u16's.
                    SET_VRING_BASE => {
                        let base = u16::try_from(num)
                            .map_err(|_| value("a split ring's base past a u16"))?;
                        Request::SetVringBase { index, base }
                    }
                    GET_VRING_BASE => Request::GetVringBase { index },
                    _ => Request::SetVringEnable {
                        index,
                        enable: match num {
                            0 => false,
                            1 => true,
                            _ => return Err(value("a ring enabled neither 0 nor 1")),
                        },
                    },
                }
            }
            SET_VRING_ADDR => {
                let addr: VringAddrWire = whole(request, &payload)?;
                no_fds(&fds)?;
                let index = queue(request, u64::from(u32::from_le(addr.index)), queues)?;
                let flags = u32::from_le(addr.flags);
                if flags != 0 {
                    return Err(value("a ring's addresses of flags (logging isn't offered)"));
                }
                let addr = VringAddr {
                    desc: u64::from_le(addr.desc),
                    used: u64::from_le(addr.used),
                    avail: u64::from_le(addr.avail),
                };
                Request::SetVringAddr { index, addr }
            }
            SET_VRING_KICK | SET_VRING_CALL | SET_VRING_ERR => {
                let word: U64 = whole(request, &payload)?;
                let word = u64::from_le(word.value);
                if word & !(VRING_INDEX_MASK | VRING_NOFD_MASK) != 0 {
                    return Err(value("a ring's descriptor word of unknown bits"));
                }
                let nofd = word & VRING_NOFD_MASK != 0;
                let fd = one_fd(request, fds, nofd)?;
                let index = queue(request, word & VRING_INDEX_MASK, queues)?;
                // cladefs: no ring polled, nor one never signalled: a back-end polling a ring
                // spins a thread for a guest that may never use it.
                let fd = fd.ok_or(value("a ring without a descriptor (polling isn't served)"))?;
                match request {
                    SET_VRING_KICK => Request::SetVringKick { index, fd },
                    SET_VRING_CALL => Request::SetVringCall { index, fd },
                    _ => Request::SetVringErr { index, fd },
                }
            }
            GET_CONFIG => {
                let (config, rest) =
                    ConfigHeader::read_from_prefix(&payload).map_err(|_| Refused::Size {
                        request,
                        size: payload.len(),
                        expected: size_of::<ConfigHeader>(),
                    })?;
                no_fds(&fds)?;
                let size = u32::from_le(config.size);
                if usize::try_from(size).ok() != Some(rest.len()) {
                    return Err(Refused::Size {
                        request,
                        size: payload.len(),
                        expected: size_of::<ConfigHeader>()
                            .saturating_add(usize::try_from(size).unwrap_or(usize::MAX)),
                    });
                }
                if size > MAX_CONFIG_SIZE {
                    return Err(value("a configuration read past 256 bytes"));
                }
                if u32::from_le(config.flags) != 0 {
                    return Err(value("a configuration read of flags"));
                }
                let offset = u32::from_le(config.offset);
                if offset.checked_add(size).is_none() {
                    return Err(value("a configuration read past a u32"));
                }
                Request::GetConfig { offset, size }
            }
            SET_BACKEND_REQ_FD => {
                empty(&payload)?;
                let fd = one_fd(request, fds, false)?.expect("one descriptor");
                Request::SetBackendReqFd(fd)
            }
            _ => return Err(Refused::Unknown(request)),
        })
    }
}
